import assert from "node:assert/strict";
import { test } from "node:test";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import net from "node:net";
import { createPocket } from "../server.js";

const JSON_HEADERS = { "content-type": "application/json" };

async function startPocket(dataDir) {
  const pocket = createPocket({ dataDir, host: "127.0.0.1", port: 0 });
  await pocket.listen();
  return { pocket, base: `http://127.0.0.1:${pocket.address().port}` };
}

async function makeDataDir(t) {
  const dataDir = await mkdtemp(join(tmpdir(), "pockets-dice-"));
  t.after(() => rm(dataDir, { recursive: true, force: true }));
  return dataDir;
}

async function roll(base, headers = JSON_HEADERS) {
  return fetch(`${base}/roll`, { method: "POST", headers, body: "{}" });
}

// Parse the state the home page renders from stored data.
async function shownState(base) {
  const body = await (await fetch(`${base}/`)).text();
  const last = body.match(/data-last-roll="(\d*)"/);
  const count = body.match(/data-roll-count="(\d+)"/);
  assert.ok(last && count, `expected dice state in body:\n${body}`);
  return {
    lastRoll: last[1] === "" ? null : Number(last[1]),
    rollCount: Number(count[1]),
  };
}

// Raw HTTP over TCP so oversized or truncated requests are exact. Resolves
// with whatever bytes arrived before the socket closed, so a server-side
// reset after the response cannot hide the status line.
function rawRequest(port, request) {
  return new Promise((resolve, reject) => {
    let data = "";
    const socket = net.connect(port, "127.0.0.1");
    const timer = setTimeout(() => {
      socket.destroy();
      reject(new Error(`raw request timed out; got: ${data}`));
    }, 2000);
    socket.on("connect", () => socket.end(request));
    socket.on("data", (chunk) => {
      data += chunk;
    });
    socket.on("error", () => {}); // the server may reset after responding
    socket.on("close", () => {
      clearTimeout(timer);
      resolve(data);
    });
  });
}

// Write a truncated request, then reset mid-body so the server is left
// waiting on a stream that will never finish.
function rawAbort(port, request) {
  return new Promise((resolve) => {
    const socket = net.connect(port, "127.0.0.1");
    const timer = setTimeout(() => {
      socket.destroy();
      resolve();
    }, 2000);
    socket.on("connect", () => {
      socket.write(request);
      socket.destroy();
    });
    socket.on("error", () => {});
    socket.on("close", () => {
      clearTimeout(timer);
      resolve();
    });
  });
}

test("GET /health returns 200 JSON", async (t) => {
  const dataDir = await makeDataDir(t);
  const { pocket, base } = await startPocket(dataDir);
  t.after(() => pocket.close());

  const res = await fetch(`${base}/health`);
  assert.equal(res.status, 200);
  assert.match(res.headers.get("content-type"), /application\/json/);
  assert.deepEqual(await res.json(), { status: "ok" });
});

test("POST /roll yields 1..6 and counts up; writes are same-origin and POST-only", async (t) => {
  const dataDir = await makeDataDir(t);
  const { pocket, base } = await startPocket(dataDir);
  t.after(() => pocket.close());

  assert.deepEqual(await shownState(base), { lastRoll: null, rollCount: 0 });

  const first = await roll(base);
  assert.equal(first.status, 200);
  const firstState = await first.json();
  assert.ok(
    Number.isInteger(firstState.lastRoll) &&
      firstState.lastRoll >= 1 &&
      firstState.lastRoll <= 6,
    `roll out of range: ${firstState.lastRoll}`,
  );
  assert.equal(firstState.rollCount, 1);

  const second = await (await roll(base)).json();
  assert.equal(second.rollCount, 2);
  assert.deepEqual(await shownState(base), {
    lastRoll: second.lastRoll,
    rollCount: 2,
  });

  // A cross-origin POST is refused and does not count as a roll.
  const crossOrigin = await roll(base, {
    ...JSON_HEADERS,
    origin: "http://evil.example",
  });
  assert.equal(crossOrigin.status, 403);

  // The mutation is never reachable by GET.
  assert.equal((await fetch(`${base}/roll`)).status, 405);

  assert.deepEqual(await shownState(base), {
    lastRoll: second.lastRoll,
    rollCount: 2,
  });
});

test("last roll and roll count persist across restarts", async (t) => {
  const dataDir = await makeDataDir(t);

  const first = await startPocket(dataDir);
  t.after(() => first.pocket.close());
  const rolled = await (await roll(first.base)).json();
  await first.pocket.close();

  assert.deepEqual(
    JSON.parse(await readFile(join(dataDir, "dice.json"), "utf8")),
    { lastRoll: rolled.lastRoll, rollCount: 1 },
  );

  // Restart against the same data directory: both values must survive and
  // the next roll must continue the count.
  const second = await startPocket(dataDir);
  t.after(() => second.pocket.close());
  assert.deepEqual(await shownState(second.base), {
    lastRoll: rolled.lastRoll,
    rollCount: 1,
  });

  const next = await (await roll(second.base)).json();
  assert.equal(next.rollCount, 2);
});

test("oversized bodies get 413; aborted or malformed requests cannot kill the server", async (t) => {
  const dataDir = await makeDataDir(t);
  const { pocket, base } = await startPocket(dataDir);
  t.after(() => pocket.close());
  const port = pocket.address().port;
  const hostHeader = `Host: 127.0.0.1:${port}`;

  await roll(base); // one counted roll as the baseline

  // A body beyond the 1 KiB cap is refused, and the 413 is still delivered
  // because the drain stops reading instead of destroying the request.
  const oversized = await rawRequest(
    port,
    [
      "POST /roll HTTP/1.1",
      hostHeader,
      "Content-Type: application/json",
      "Content-Length: 2048",
      "Connection: close",
      "",
      "x".repeat(2048),
    ].join("\r\n"),
  );
  assert.match(oversized, /^HTTP\/1\.1 413\b/);

  // Mid-body client resets and malformed request lines must not crash the
  // process or count as rolls.
  await rawAbort(
    port,
    [
      "POST /roll HTTP/1.1",
      hostHeader,
      "Content-Type: application/json",
      "Content-Length: 4096",
      "",
      '{"tru',
    ].join("\r\n"),
  );
  await rawRequest(port, "BROKEN REQUEST LINE\r\n\r\n");

  const health = await fetch(`${base}/health`);
  assert.equal(health.status, 200);
  assert.equal((await shownState(base)).rollCount, 1);
});

test("roll accepts only the exact loopback http origin and application/json", async (t) => {
  const dataDir = await makeDataDir(t);
  const { pocket, base } = await startPocket(dataDir);
  t.after(() => pocket.close());
  const port = pocket.address().port;

  await roll(base); // one counted roll as the baseline

  // https is not this plain-http app's origin, even with the same host.
  const httpsOrigin = await roll(base, {
    ...JSON_HEADERS,
    origin: `https://127.0.0.1:${port}`,
  });
  assert.equal(httpsOrigin.status, 403);

  // DNS rebinding: a matching hostile Host + Origin pair, sent over raw TCP
  // because fetch derives Host from the connection target.
  const rebound = await rawRequest(
    port,
    [
      "POST /roll HTTP/1.1",
      "Host: evil.example",
      "Origin: http://evil.example",
      "Content-Type: application/json",
      "Content-Length: 2",
      "Connection: close",
      "",
      "{}",
    ].join("\r\n"),
  );
  assert.match(rebound, /^HTTP\/1\.1 403\b/);

  // application/jsonp merely starts with application/json but is not it.
  const jsonp = await roll(base, {
    ...JSON_HEADERS,
    "content-type": "application/jsonp",
  });
  assert.equal(jsonp.status, 415);

  // None of the refused attempts counted as a roll.
  assert.equal((await shownState(base)).rollCount, 1);
});
