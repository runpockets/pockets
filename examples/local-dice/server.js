// Local Dice: dependency-free Node HTTP server that rolls one six-sided die.
// The last roll and the total roll count persist to DATA_DIR. Keep
// GET /health returning 200 for readiness; POST /roll is the only mutation.

import { randomInt } from "node:crypto";
import { createServer } from "node:http";
import { mkdir, readFile, rename, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { pathToFileURL } from "node:url";

const STATE_FILE = "dice.json";
// Rolling takes no parameters, so request bodies are only read to bound them.
const MAX_BODY_BYTES = 1024;

const DEFAULT_STATE = { lastRoll: null, rollCount: 0 };

function homePage(state) {
  // Both values are validated integers (or null) before rendering, and the
  // inline script only ever assigns textContent, so nothing is injectable.
  const lastRoll = state.lastRoll === null ? "\u2014" : String(state.lastRoll);
  return `<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <title>Local Dice</title>
    <style>
      body {
        margin: 0;
        min-height: 100vh;
        display: grid;
        place-items: center;
        background: #0f172a;
        color: #e2e8f0;
        font-family: system-ui, sans-serif;
      }
      main {
        text-align: center;
        padding: 2rem;
      }
      .state {
        color: #7dd3fc;
        font-size: 1.2rem;
      }
      button {
        font: inherit;
        padding: 0.4rem 1.4rem;
        cursor: pointer;
      }
    </style>
  </head>
  <body>
    <main>
      <h1>Local Dice</h1>
      <p class="state" data-last-roll="${state.lastRoll ?? ""}">
        Last roll: <strong id="last-roll">${lastRoll}</strong>
      </p>
      <p class="state" data-roll-count="${state.rollCount}">
        Total rolls: <strong id="roll-count">${state.rollCount}</strong>
      </p>
      <p><button id="roll" type="button">Roll</button></p>
    </main>
    <script>
      const button = document.getElementById("roll");
      button.addEventListener("click", async () => {
        button.disabled = true;
        try {
          const res = await fetch("/roll", {
            method: "POST",
            headers: { "content-type": "application/json" },
            body: "{}",
          });
          if (!res.ok) throw new Error("roll failed: HTTP " + res.status);
          const state = await res.json();
          document.getElementById("last-roll").textContent = state.lastRoll;
          document.getElementById("roll-count").textContent = state.rollCount;
        } catch {
          window.alert("Roll failed. Try again.");
        } finally {
          button.disabled = false;
        }
      });
    </script>
  </body>
</html>
`;
}

// Stored state must look like it was written by this app; anything else
// (torn write, foreign file) is treated as absent.
function isValidState(state) {
  return (
    state !== null &&
    typeof state === "object" &&
    !Array.isArray(state) &&
    Number.isInteger(state.rollCount) &&
    state.rollCount >= 0 &&
    (state.lastRoll === null ||
      (Number.isInteger(state.lastRoll) &&
        state.lastRoll >= 1 &&
        state.lastRoll <= 6))
  );
}

async function readState(file) {
  let raw;
  try {
    raw = await readFile(file, "utf8");
  } catch (err) {
    if (err.code === "ENOENT") return { ...DEFAULT_STATE };
    throw err;
  }
  let state;
  try {
    state = JSON.parse(raw);
  } catch {
    return { ...DEFAULT_STATE }; // corrupted file (e.g. torn write); start fresh
  }
  return isValidState(state)
    ? { lastRoll: state.lastRoll, rollCount: state.rollCount }
    : { ...DEFAULT_STATE };
}

async function writeState(file, state) {
  await mkdir(dirname(file), { recursive: true });
  const tmp = join(dirname(file), `${STATE_FILE}.${process.pid}.tmp`);
  await writeFile(tmp, `${JSON.stringify(state)}\n`, "utf8");
  await rename(tmp, file); // atomic replace: readers never see a partial file
}

// Parse the Host header into this app's own origin, accepting only plain
// loopback: http://127.0.0.1 or http://localhost with no credentials, path,
// query, or fragment. A matching pair of hostile Host and Origin headers —
// the DNS-rebinding pattern, where evil.example resolves to 127.0.0.1 —
// still fails here because the Host never names our loopback origin.
// Returns null for anything else.
function loopbackOrigin(hostHeader) {
  if (typeof hostHeader !== "string" || hostHeader === "") return null;
  let url;
  try {
    url = new URL(`http://${hostHeader}`);
  } catch {
    return null; // spaces, control characters, and other junk never parse
  }
  if (url.hostname !== "127.0.0.1" && url.hostname !== "localhost") {
    return null;
  }
  if (url.username || url.password) return null;
  if (url.pathname !== "/" || url.search || url.hash) return null;
  return url.origin;
}

// Browsers always send Origin on POST, so anything but this app's exact
// plain-http origin is cross-origin: https and other schemes are different
// origins even with a matching host, and an opaque "null" origin never
// parses to a scheme-qualified origin. Non-browser clients omit Origin and
// are judged by the valid loopback Host plus the media-type gate instead.
function isExactHttpOrigin(originHeader, hostOrigin) {
  let url;
  try {
    url = new URL(originHeader);
  } catch {
    return false;
  }
  return (
    url.protocol === "http:" &&
    !url.username &&
    !url.password &&
    url.origin === hostOrigin
  );
}

// Strict media type: exactly "application/json" with optional ";"
// parameters, so lookalikes such as application/jsonp are refused.
function isApplicationJson(contentType) {
  const type = (contentType ?? "").split(";", 1)[0].trim().toLowerCase();
  return type === "application/json";
}

// The roll endpoint takes no input, so bodies are only counted (never
// retained) and bounded. This resolves, never rejects: exceeding the limit
// stops reading instead of destroying the request, so the caller's 413 still
// reaches the client; aborts, stream errors, and mid-body socket resets
// resolve as "aborted" so the connection is simply dropped.
function drainBody(req, limit) {
  return new Promise((resolve) => {
    let size = 0;
    let settled = false;
    function finish(result) {
      if (settled) return;
      settled = true;
      req.off("data", onData);
      req.off("end", onEnd);
      req.off("aborted", onGone);
      req.off("error", onGone);
      req.off("close", onGone);
      resolve(result);
    }
    function onData(chunk) {
      size += chunk.length;
      if (size > limit) finish("too-large"); // stop reading; caller sends 413
    }
    function onEnd() {
      finish("complete");
    }
    function onGone() {
      finish("aborted");
    }
    req.on("data", onData);
    req.once("end", onEnd);
    req.once("aborted", onGone);
    req.once("error", onGone);
    req.once("close", onGone); // covers destroys that emit no error
  });
}

// Exported factory so tests can start the server on an ephemeral port with a
// temporary data directory. Options fall back to container-friendly defaults.
export function createPocket(options = {}) {
  const dataDir = options.dataDir ?? process.env.DATA_DIR ?? "/data";
  const host = options.host ?? process.env.HOST ?? "0.0.0.0";
  const port = options.port ?? (Number(process.env.PORT) || 8080);
  const file = join(dataDir, STATE_FILE);

  // Serialize read-modify-write of the state so concurrent requests cannot
  // lose rolls. A failed task surfaces to its own caller but does not poison
  // the queue for later requests.
  let queued = Promise.resolve();
  function enqueue(task) {
    const result = queued.then(task, task);
    queued = result.then(
      () => {},
      () => {},
    );
    return result;
  }

  function sendText(res, status, message, extraHeaders = {}) {
    res.writeHead(status, {
      "content-type": "text/plain; charset=utf-8",
      ...extraHeaders,
    });
    res.end(message);
  }

  async function handleRoll(req, res) {
    // Pin writes to the exact loopback http origin: the Host must name this
    // app itself, any browser-sent Origin must equal that origin exactly,
    // and Sec-Fetch-Site: cross-site is refused even when the browser strips
    // Origin or a client omits it.
    const hostOrigin = loopbackOrigin(req.headers.host);
    const origin = req.headers.origin;
    const secFetchSite = (req.headers["sec-fetch-site"] ?? "").toLowerCase();
    if (
      hostOrigin === null ||
      (origin !== undefined && !isExactHttpOrigin(origin, hostOrigin)) ||
      secFetchSite === "cross-site"
    ) {
      sendText(res, 403, "Cross-origin writes are not allowed\n");
      req.resume(); // drain the rejected request body
      return;
    }
    // Requiring strict application/json forces a CORS preflight for
    // browsers, which this server never grants, and blocks no-preflight
    // form posts.
    if (!isApplicationJson(req.headers["content-type"])) {
      sendText(res, 415, 'Roll requires a "application/json" request body\n');
      req.resume(); // drain the rejected request body
      return;
    }
    const drained = await drainBody(req, MAX_BODY_BYTES);
    if (drained === "aborted") return; // connection gone; nothing to respond to
    if (drained !== "complete") {
      sendText(res, 413, "Request body too large\n", { connection: "close" });
      return;
    }

    let state;
    try {
      state = await enqueue(async () => {
        const current = await readState(file);
        const next = {
          lastRoll: randomInt(1, 7),
          rollCount: current.rollCount + 1,
        };
        await writeState(file, next);
        return next;
      });
    } catch (err) {
      console.error("pocket: failed to record roll", err);
      sendText(res, 500, "Unable to record roll\n");
      return;
    }
    res.writeHead(200, { "content-type": "application/json" });
    res.end(`${JSON.stringify(state)}\n`);
  }

  async function handleRequest(req, res) {
    let pathname;
    try {
      // Route on req.url only, against a fixed base: the Host header is
      // untrusted, must never affect routing, and a malformed one must not
      // throw inside this async handler.
      pathname = new URL(req.url ?? "/", "http://localhost").pathname;
    } catch {
      sendText(res, 400, "Bad request\n");
      return;
    }

    // The only mutation, deliberately POST: never reachable by GET.
    if (pathname === "/roll") {
      if (req.method !== "POST") {
        sendText(res, 405, "Method not allowed\n", { allow: "POST" });
        return;
      }
      await handleRoll(req, res);
      return;
    }

    if (req.method !== "GET") {
      sendText(res, 405, "Method not allowed\n");
      return;
    }

    if (pathname === "/health") {
      res.writeHead(200, { "content-type": "application/json" });
      res.end('{"status":"ok"}\n');
      return;
    }

    if (pathname === "/") {
      let state;
      try {
        // Read-only: viewing the page never changes the count. The atomic
        // rename in writeState makes concurrent reads safe without enqueue.
        state = await readState(file);
      } catch (err) {
        console.error("pocket: failed to read state", err);
        sendText(res, 500, "Unable to read dice state\n");
        return;
      }
      res.writeHead(200, { "content-type": "text/html; charset=utf-8" });
      res.end(homePage(state));
      return;
    }

    sendText(res, 404, "Not found\n");
  }

  const server = createServer((req, res) => {
    // Aborted connections are routine: consume stream errors so a mid-body
    // socket reset can never surface as an uncaught stream error.
    req.on("error", () => {});
    res.on("error", () => {});
    // Final rejection boundary: an async handler that rejects would become an
    // unhandled rejection and crash the process. Log and, only while the
    // response is still usable, send a 500 — never a second response.
    handleRequest(req, res).catch((err) => {
      console.error("pocket: unhandled request error", err);
      if (!res.headersSent && res.writable) {
        sendText(res, 500, "Internal server error\n", { connection: "close" });
      }
    });
  });

  let closing;

  return {
    dataDir,
    server,
    listen() {
      return new Promise((resolve, reject) => {
        const onError = (err) => reject(err);
        server.once("error", onError);
        server.listen(port, host, () => {
          server.removeListener("error", onError);
          resolve();
        });
      });
    },
    address() {
      return server.address();
    },
    close() {
      if (!closing) {
        closing = new Promise((resolve, reject) => {
          server.close((err) => (err ? reject(err) : resolve()));
        });
        // Release idle keep-alive connections so close() settles promptly.
        server.closeIdleConnections();
      }
      return closing;
    },
  };
}

const isMain = Boolean(
  process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href,
);

if (isMain) {
  const pocket = createPocket();
  await pocket.listen();
  const addr = pocket.address();
  console.log(
    `pocket listening on http://${addr.address}:${addr.port} ` +
      `(data: ${pocket.dataDir})`,
  );

  let shuttingDown = false;
  const shutdown = (signal) => {
    if (shuttingDown) return;
    shuttingDown = true;
    console.log(`pocket received ${signal}, shutting down`);
    pocket.close().then(
      () => process.exit(0),
      (err) => {
        console.error("pocket: error during shutdown", err);
        process.exit(1);
      },
    );
  };
  process.on("SIGTERM", () => shutdown("SIGTERM"));
  process.on("SIGINT", () => shutdown("SIGINT"));
}
