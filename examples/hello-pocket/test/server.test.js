import assert from "node:assert/strict";
import { test } from "node:test";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import net from "node:net";
import { createPocket } from "../server.js";

async function startPocket(dataDir) {
  const pocket = createPocket({ dataDir, host: "127.0.0.1", port: 0 });
  await pocket.listen();
  return { pocket, base: `http://127.0.0.1:${pocket.address().port}` };
}

async function makeDataDir(t) {
  const dataDir = await mkdtemp(join(tmpdir(), "pockets-hello-"));
  t.after(() => rm(dataDir, { recursive: true, force: true }));
  return dataDir;
}

async function visitsIn(response) {
  const body = await response.text();
  const match = body.match(/data-visits="(\d+)"/);
  assert.ok(match, `expected a visit count in body:\n${body}`);
  return Number(match[1]);
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

test("visit count persists across restarts", async (t) => {
  const dataDir = await makeDataDir(t);

  const first = await startPocket(dataDir);
  t.after(() => first.pocket.close());
  assert.equal(await visitsIn(await fetch(`${first.base}/`)), 1);
  assert.equal(await visitsIn(await fetch(`${first.base}/`)), 2);
  await first.pocket.close();

  // Restart against the same data directory: the count must survive.
  const second = await startPocket(dataDir);
  t.after(() => second.pocket.close());
  assert.equal(await visitsIn(await fetch(`${second.base}/`)), 3);

  const stored = JSON.parse(
    await readFile(join(dataDir, "visits.json"), "utf8"),
  );
  assert.equal(stored.visits, 3);
});

test("concurrent visits are serialized (no lost updates)", async (t) => {
  const dataDir = await makeDataDir(t);
  const { pocket, base } = await startPocket(dataDir);
  t.after(() => pocket.close());

  const responses = await Promise.all(
    Array.from({ length: 10 }, () => fetch(`${base}/`)),
  );
  const counts = (await Promise.all(responses.map(visitsIn))).sort(
    (a, b) => a - b,
  );
  assert.deepEqual(counts, [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
});

// Send raw bytes over a socket so malformed request lines/headers are not
// normalized by fetch, and return the raw response start.
function rawRequest(port, requestBytes) {
  return new Promise((resolve, reject) => {
    const socket = net.connect(port, "127.0.0.1", () =>
      socket.end(requestBytes),
    );
    let data = "";
    let settled = false;
    const finish = () => {
      if (!settled) {
        settled = true;
        resolve(data);
      }
    };
    socket.on("data", (chunk) => (data += chunk));
    socket.on("end", finish);
    socket.on("close", finish);
    socket.on("error", reject);
  });
}

test("malformed Host or request target returns an error without crashing", async (t) => {
  const dataDir = await makeDataDir(t);
  const { pocket, base } = await startPocket(dataDir);
  t.after(() => pocket.close());
  const { port } = pocket.address();

  // A Host header that would make URL parsing throw must be ignored: routing
  // uses req.url against a fixed base, never the untrusted Host.
  const rawHost = await rawRequest(
    port,
    "GET /health HTTP/1.1\r\nHost: ]invalid[\r\nConnection: close\r\n\r\n",
  );
  assert.match(rawHost, /^HTTP\/1\.1 200 /, `got: ${rawHost}`);

  // A malformed request target must produce a 400 (from the URL guard or the
  // HTTP parser), never a rejected handler that terminates the process.
  const rawTarget = await rawRequest(
    port,
    "GET http://[ HTTP/1.1\r\nHost: example.invalid\r\nConnection: close\r\n\r\n",
  );
  assert.match(rawTarget, /^HTTP\/1\.1 400 /, `got: ${rawTarget}`);

  // The server is still alive and healthy after both malformed requests.
  const res = await fetch(`${base}/health`);
  assert.equal(res.status, 200);
  assert.deepEqual(await res.json(), { status: "ok" });
});
