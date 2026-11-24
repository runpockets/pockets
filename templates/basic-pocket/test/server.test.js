import assert from "node:assert/strict";
import { test } from "node:test";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createPocket } from "../server.js";

async function startPocket(dataDir) {
  const pocket = createPocket({ dataDir, host: "127.0.0.1", port: 0 });
  await pocket.listen();
  return { pocket, base: `http://127.0.0.1:${pocket.address().port}` };
}

async function makeDataDir(t) {
  const dataDir = await mkdtemp(join(tmpdir(), "pockets-basic-"));
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
  await first.pocket.close();

  // Restart against the same data directory: the count must survive.
  const second = await startPocket(dataDir);
  t.after(() => second.pocket.close());
  assert.equal(await visitsIn(await fetch(`${second.base}/`)), 2);

  const stored = JSON.parse(
    await readFile(join(dataDir, "visits.json"), "utf8"),
  );
  assert.equal(stored.visits, 2);
});
