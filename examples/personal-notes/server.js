// Personal Notes: dependency-free Node HTTP server for a single plain-text
// note stored in DATA_DIR/note.txt. Server-rendered HTML with one accessible
// textarea form (no JavaScript, no external assets); saves go through
// POST /note. Replace this file with your own app code and keep GET /health
// returning 200 for readiness.

import { createServer } from "node:http";
import { randomBytes, timingSafeEqual } from "node:crypto";
import { mkdir, readFile, rename, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { pathToFileURL } from "node:url";

const NOTE_FILE = "note.txt";

// Hard caps. The body limit is larger than the note limit because the form
// body also carries the CSRF token plus urlencoding overhead.
const MAX_BODY_BYTES = 256 * 1024;
const MAX_NOTE_BYTES = 64 * 1024;

// Security headers on every response. No scripts, no frames, no external
// assets: inline <style> is the only thing needing more than default-src
// 'none'. Deliberately no CORS: no Access-Control-* header is ever sent, so
// cross-origin requests cannot read anything back.
const SECURITY_HEADERS = {
  "cache-control": "no-store",
  "x-content-type-options": "nosniff",
  "x-frame-options": "DENY",
  "content-security-policy":
    "default-src 'none'; script-src 'none'; style-src 'unsafe-inline'; " +
    "form-action 'self'; frame-ancestors 'none'; base-uri 'none'",
};

function send(res, status, headers, body) {
  res.writeHead(status, { ...SECURITY_HEADERS, ...headers });
  res.end(body);
}

function sendText(res, status, text) {
  send(res, status, { "content-type": "text/plain; charset=utf-8" }, text);
}

function methodNotAllowed(res, allow) {
  send(res, 405, { allow }, "Method not allowed\n");
}

function escapeHtml(value) {
  return value
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;") // also neutralises "</textarea>"
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#39;");
}

function homePage(note, saved, token) {
  // HTML parsers ignore a newline immediately after <textarea>, so re-add one
  // when the stored note starts with a newline to round-trip exactly.
  const value = `${note.startsWith("\n") ? "\n" : ""}${escapeHtml(note)}`;
  return `<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <title>Personal Notes</title>
    <style>
      :root { color-scheme: dark; }
      body {
        margin: 0;
        background: #0f172a;
        color: #e2e8f0;
        font-family: system-ui, sans-serif;
        line-height: 1.5;
      }
      main {
        max-width: 46rem;
        margin: 0 auto;
        padding: 2.5rem 1.25rem 3rem;
      }
      h1 { margin: 0 0 0.25rem; font-size: 1.6rem; }
      .about { margin: 0 0 1.5rem; color: #94a3b8; font-size: 0.95rem; }
      .saved {
        margin: 0 0 1.25rem;
        padding: 0.6rem 0.9rem;
        border: 1px solid #16a34a;
        border-radius: 0.5rem;
        background: #052e16;
        color: #bbf7d0;
      }
      label { display: block; margin-bottom: 0.5rem; font-weight: 600; }
      textarea {
        box-sizing: border-box;
        display: block;
        width: 100%;
        min-height: 14rem;
        padding: 0.8rem;
        resize: vertical;
        border: 1px solid #334155;
        border-radius: 0.5rem;
        background: #111c33;
        color: #e2e8f0;
        font: inherit;
      }
      .hint { margin: 0.5rem 0 1.25rem; color: #94a3b8; font-size: 0.85rem; }
      button {
        padding: 0.6rem 1.6rem;
        border: 0;
        border-radius: 0.5rem;
        background: #7dd3fc;
        color: #0f172a;
        font: inherit;
        font-weight: 600;
        cursor: pointer;
      }
      button:hover { background: #a5e3fd; }
      textarea:focus-visible, button:focus-visible {
        outline: 2px solid #7dd3fc;
        outline-offset: 2px;
      }
      @media (max-width: 30rem) {
        main { padding: 1.5rem 1rem 2rem; }
        button { width: 100%; }
      }
    </style>
  </head>
  <body>
    <main>
      <h1>Personal Notes</h1>
      <p class="about">
        One plain-text note shared by everyone who opens this Pocket. It is
        stored in this container&rsquo;s data volume only: local, no accounts,
        no sign-in, and nothing is sent anywhere else.
      </p>
      ${saved ? '<p class="saved" role="status">Note saved.</p>' : ""}
      <form method="post" action="/note">
        <label for="note">Your note</label>
        <textarea id="note" name="note" rows="14" aria-describedby="note-hint">${value}</textarea>
        <p id="note-hint" class="hint">
          Saved as plain text, up to 64 KiB of UTF-8. Every visitor reads and
          writes the same note.
        </p>
        <input type="hidden" name="token" value="${escapeHtml(token)}" />
        <button type="submit">Save</button>
      </form>
    </main>
  </body>
</html>
`;
}

async function readNote(file) {
  try {
    return await readFile(file, "utf8");
  } catch (err) {
    if (err.code === "ENOENT") return ""; // missing file means an empty note
    throw err; // other read errors must surface, never silently overwrite
  }
}

async function writeNote(file, text) {
  await mkdir(dirname(file), { recursive: true });
  const tmp = join(dirname(file), `${NOTE_FILE}.${process.pid}.tmp`);
  await writeFile(tmp, text, "utf8");
  await rename(tmp, file); // atomic replace: readers never see a partial file
}

function bodyTooLarge() {
  return Object.assign(new Error("request body too large"), { status: 413 });
}

// Collect the raw request body, refusing anything past limitBytes whether the
// size is announced via Content-Length or discovered mid-stream.
function readBody(req, limitBytes) {
  return new Promise((resolve, reject) => {
    let done = false;
    const fail = (err) => {
      if (!done) {
        done = true;
        reject(err);
      }
    };
    req.on("error", fail); // an aborted upload must not surface unhandled
    const declared = Number(req.headers["content-length"]);
    if (Number.isInteger(declared) && declared > limitBytes) {
      fail(bodyTooLarge());
      req.resume(); // drain so the connection can close cleanly
      return;
    }
    const chunks = [];
    let size = 0;
    req.on("data", (chunk) => {
      if (done) return;
      size += chunk.length;
      if (size > limitBytes) {
        chunks.length = 0;
        fail(bodyTooLarge());
        return;
      }
      chunks.push(chunk);
    });
    req.on("end", () => {
      if (done) return;
      done = true;
      resolve(Buffer.concat(chunks));
    });
  });
}

function tokenMatches(expected, provided) {
  if (typeof provided !== "string") return false;
  const a = Buffer.from(expected, "utf8");
  const b = Buffer.from(provided, "utf8");
  return a.length === b.length && timingSafeEqual(a, b);
}

// Exported factory so tests can start the server on an ephemeral port with a
// temporary data directory. Options fall back to container-friendly defaults.
export function createPocket(options = {}) {
  const dataDir = options.dataDir ?? process.env.DATA_DIR ?? "/data";
  const host = options.host ?? process.env.HOST ?? "0.0.0.0";
  const port = options.port ?? (Number(process.env.PORT) || 8080);
  const file = join(dataDir, NOTE_FILE);

  // Unpredictable per-process CSRF token rendered into the form and required
  // on save. There is no login and no session: the single shared note is
  // writable by anyone who can load the page, and the token only stops
  // cross-site form submissions. Restarting rotates it.
  const csrfToken = randomBytes(32).toString("hex");

  // Serialize access to the note so a save cannot interleave with a read or
  // another save. A failed task surfaces to its own caller but does not
  // poison the queue for later requests.
  let queued = Promise.resolve();
  function enqueue(task) {
    const result = queued.then(task, task);
    queued = result.then(
      () => {},
      () => {},
    );
    return result;
  }

  async function handleSave(req, res) {
    const type = String(req.headers["content-type"] ?? "")
      .split(";")[0]
      .trim()
      .toLowerCase();
    if (type !== "application/x-www-form-urlencoded") {
      sendText(
        res,
        415,
        "Expected an application/x-www-form-urlencoded form.\n",
      );
      return;
    }

    let body;
    try {
      body = await readBody(req, MAX_BODY_BYTES);
    } catch (err) {
      if (err.status === 413) {
        sendText(
          res,
          413,
          `Form submission exceeds the ${MAX_BODY_BYTES}-byte limit.\n`,
        );
      } else {
        console.error("pocket: failed to read request body", err);
        sendText(res, 400, "Could not read the request body.\n");
      }
      return;
    }

    const form = new URLSearchParams(body.toString("utf8"));
    if (!tokenMatches(csrfToken, form.get("token"))) {
      sendText(
        res,
        403,
        "Missing or invalid CSRF token. Reload the page and try again.\n",
      );
      return;
    }

    const note = form.get("note");
    if (note === null) {
      sendText(res, 400, 'Missing "note" field.\n');
      return;
    }
    if (Buffer.byteLength(note, "utf8") > MAX_NOTE_BYTES) {
      sendText(
        res,
        413,
        `Note exceeds the ${MAX_NOTE_BYTES}-byte UTF-8 limit.\n`,
      );
      return;
    }

    try {
      await enqueue(() => writeNote(file, note));
    } catch (err) {
      console.error("pocket: failed to save note", err);
      sendText(res, 500, "Unable to save the note. Please try again.\n");
      return;
    }

    send(res, 303, { location: "/?saved=1" }, "Note saved. Redirecting…\n");
  }

  const server = createServer(async (req, res) => {
    const method = req.method ?? "GET";
    let url;
    try {
      // Route on req.url only, against a fixed base: the Host header is
      // untrusted, must never affect routing, and a malformed one must not
      // throw inside this async handler.
      url = new URL(req.url ?? "/", "http://localhost");
    } catch {
      sendText(res, 400, "Bad request\n");
      return;
    }
    const pathname = url.pathname;

    if (pathname === "/health") {
      if (method !== "GET") {
        methodNotAllowed(res, "GET");
        return;
      }
      send(
        res,
        200,
        { "content-type": "application/json" },
        '{"status":"ok"}\n',
      );
      return;
    }

    if (pathname === "/") {
      if (method !== "GET") {
        methodNotAllowed(res, "GET");
        return;
      }
      let note;
      try {
        note = await enqueue(() => readNote(file)); // read-only: GET never mutates
      } catch (err) {
        console.error("pocket: failed to read note", err);
        sendText(res, 500, "Unable to read the note.\n");
        return;
      }
      const saved = url.searchParams.get("saved") === "1";
      send(
        res,
        200,
        { "content-type": "text/html; charset=utf-8" },
        homePage(note, saved, csrfToken),
      );
      return;
    }

    if (pathname === "/note") {
      if (method !== "POST") {
        methodNotAllowed(res, "POST");
        return;
      }
      await handleSave(req, res);
      return;
    }

    sendText(res, 404, "Not found\n");
  });

  let closing;

  return {
    dataDir,
    csrfToken,
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
