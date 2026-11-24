import assert from "node:assert/strict";
import { test } from "node:test";
import {
  mkdir,
  mkdtemp,
  readdir,
  readFile,
  rm,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { cacheFilename, createCache } from "../lib/cache.js";

async function makeDataDir(t) {
  const dataDir = await mkdtemp(join(tmpdir(), "pockets-cache-"));
  t.after(() => rm(dataDir, { recursive: true, force: true }));
  return dataDir;
}

function deferred() {
  let resolvePromise;
  let rejectPromise;
  const promise = new Promise((res, rej) => {
    resolvePromise = res;
    rejectPromise = rej;
  });
  return { promise, resolve: resolvePromise, reject: rejectPromise };
}

test("memory hits avoid disk reads and upstream fetches while fresh", async (t) => {
  const dataDir = await makeDataDir(t);
  let clock = 1_000;
  const cache = createCache({ dataDir, now: () => clock });

  let fetches = 0;
  const first = await cache.getOrFetch("prices:btc", 60_000, async () => {
    fetches += 1;
    return { nzd: 150_000 };
  });
  assert.equal(fetches, 1);
  assert.deepEqual(first, {
    data: { nzd: 150_000 },
    fetchedAt: 1_000,
    isFresh: true,
    stale: false,
    source: "fetch",
    revalidating: false,
  });

  // Remove the disk file to prove the next hit is served from memory.
  await rm(join(cache.cacheDir, cacheFilename("prices:btc")));

  clock = 30_000;
  const second = await cache.getOrFetch("prices:btc", 60_000, async () => {
    fetches += 1;
    return { nzd: 999_999 };
  });
  assert.equal(fetches, 1);
  assert.equal(second.source, "memory");
  assert.equal(second.isFresh, true);
  assert.equal(second.stale, false);
  assert.deepEqual(second.data, { nzd: 150_000 });

  const peeked = await cache.peek("prices:btc", 60_000);
  assert.deepEqual(peeked, {
    data: { nzd: 150_000 },
    fetchedAt: 1_000,
    isFresh: true,
  });

  // forceRefresh bypasses the fresh cache hit and updates memory + disk.
  clock = 35_000;
  const forced = await cache.getOrFetch(
    "prices:btc",
    60_000,
    async () => {
      fetches += 1;
      return { nzd: 152_000 };
    },
    { forceRefresh: true },
  );
  assert.equal(fetches, 2);
  assert.equal(forced.source, "fetch");
  assert.equal(forced.fetchedAt, 35_000);
  assert.deepEqual(forced.data, { nzd: 152_000 });
});

test("disk persistence survives restarts and hydrates memory", async (t) => {
  const dataDir = await makeDataDir(t);
  let clock = 10_000;

  const firstInstance = createCache({ dataDir, now: () => clock });
  await firstInstance.getOrFetch("portfolio", 60_000, async () => ({
    totalNzd: 42_500,
  }));

  // Simulate a container restart with a fresh cache instance against the same DATA_DIR.
  clock = 25_000;
  const restarted = createCache({ dataDir, now: () => clock });

  let fetches = 0;
  const fromDisk = await restarted.getOrFetch("portfolio", 60_000, async () => {
    fetches += 1;
    return { totalNzd: 0 };
  });
  assert.equal(fetches, 0);
  assert.equal(fromDisk.source, "disk");
  assert.equal(fromDisk.isFresh, true);
  assert.equal(fromDisk.fetchedAt, 10_000);
  assert.deepEqual(fromDisk.data, { totalNzd: 42_500 });

  // Subsequent reads in the restarted process hit memory.
  const fromMemory = await restarted.getOrFetch(
    "portfolio",
    60_000,
    async () => {
      fetches += 1;
      return { totalNzd: 0 };
    },
  );
  assert.equal(fetches, 0);
  assert.equal(fromMemory.source, "memory");
});

test("peek returns stale data past TTL with zero upstream calls and null when absent", async (t) => {
  const dataDir = await makeDataDir(t);
  let clock = 5_000;
  const writer = createCache({ dataDir, now: () => clock });

  assert.equal(await writer.peek("missing", 60_000), null);

  await writer.set("rates", { usdNzd: 1.67 });
  await writer.set("falsy-zero", 0);
  await writer.set("falsy-null", null);

  // Advance well past TTL and open a cold instance (simulating another browser/restart).
  clock = 500_000;
  const reader = createCache({ dataDir, now: () => clock });

  const stale = await reader.peek("rates", 60_000);
  assert.deepEqual(stale, {
    data: { usdNzd: 1.67 },
    fetchedAt: 5_000,
    isFresh: false,
  });

  // Zero TTL is always stale, even at the exact write timestamp.
  const zeroTtl = await writer.peek("rates", 0);
  assert.equal(zeroTtl.isFresh, false);

  // Falsy JSON payloads round-trip intact.
  assert.deepEqual(await reader.peek("falsy-zero", 1_000_000), {
    data: 0,
    fetchedAt: 5_000,
    isFresh: true,
  });
  assert.deepEqual(await reader.peek("falsy-null", 1_000_000), {
    data: null,
    fetchedAt: 5_000,
    isFresh: true,
  });
});

test("corrupted and malformed disk entries are ignored safely and heal on fetch", async (t) => {
  const dataDir = await makeDataDir(t);
  const cache = createCache({ dataDir, now: () => 2_000 });
  await mkdir(cache.cacheDir, { recursive: true });

  await writeFile(
    join(cache.cacheDir, cacheFilename("corrupt")),
    "{not valid json",
    "utf8",
  );
  await writeFile(
    join(cache.cacheDir, cacheFilename("wrong-shape")),
    JSON.stringify({ key: "wrong-shape", fetchedAt: "bad" }),
    "utf8",
  );
  await writeFile(
    join(cache.cacheDir, cacheFilename("wrong-key")),
    JSON.stringify({ key: "other", fetchedAt: 1_000, data: "nope" }),
    "utf8",
  );

  assert.equal(await cache.peek("corrupt", 60_000), null);
  assert.equal(await cache.peek("wrong-shape", 60_000), null);
  assert.equal(await cache.peek("wrong-key", 60_000), null);

  const healed = await cache.getOrFetch("corrupt", 60_000, async () => ({
    ok: true,
  }));
  assert.deepEqual(healed.data, { ok: true });
  assert.equal(healed.source, "fetch");

  const reloaded = createCache({ dataDir, now: () => 2_500 });
  assert.deepEqual(await reloaded.peek("corrupt", 60_000), {
    data: { ok: true },
    fetchedAt: 2_000,
    isFresh: true,
  });
});

test("cache keys are sanitized, stay inside DATA_DIR/cache, and do not collide", async (t) => {
  const dataDir = await makeDataDir(t);
  const cache = createCache({ dataDir, now: () => 1_000 });

  const trickyKeys = [
    "../escape/secret",
    "../../etc/passwd",
    "prices:BTC/NZD?interval=1h&limit=24",
    "prices:btc/nzd?interval=1h&limit=24",
    "a/b",
    "a:b",
    "   ...   ",
    "k".repeat(300),
  ];

  const filenames = new Set();
  for (const [index, key] of trickyKeys.entries()) {
    const file = cacheFilename(key);
    assert.ok(!file.includes("/") && !file.includes("\\"));
    assert.ok(!file.includes(".."));
    assert.ok(file.endsWith(".json"));
    assert.ok(
      resolve(cache.cacheDir, file).startsWith(`${resolve(cache.cacheDir)}`),
    );
    assert.ok(!filenames.has(file), `expected unique filename for ${key}`);
    filenames.add(file);

    await cache.set(key, { index, key });
  }

  const cold = createCache({ dataDir, now: () => 1_000 });
  for (const [index, key] of trickyKeys.entries()) {
    const entry = await cold.peek(key, 60_000);
    assert.deepEqual(entry?.data, { index, key });
  }

  await assert.rejects(() => cache.peek("", 1_000), TypeError);
  await assert.rejects(() => cache.peek(null, 1_000), TypeError);
  await assert.rejects(() => cache.peek("ok", -1), TypeError);
});

test("atomic writes leave no temp files and serialize concurrent updates cleanly", async (t) => {
  const dataDir = await makeDataDir(t);
  let clock = 1_000;
  const cache = createCache({ dataDir, now: () => ++clock });

  // Launch many concurrent writes to the same key.
  await Promise.all(
    Array.from({ length: 12 }, (_, i) => cache.set("ticker", { seq: i })),
  );

  const entries = await readdir(cache.cacheDir);
  assert.deepEqual(entries, [cacheFilename("ticker")]);

  const raw = await readFile(
    join(cache.cacheDir, cacheFilename("ticker")),
    "utf8",
  );
  const parsed = JSON.parse(raw);
  assert.equal(parsed.key, "ticker");
  assert.equal(parsed.data.seq, 11);

  // Attempting to write an unserializable value leaves the existing valid file intact.
  const circular = {};
  circular.self = circular;
  await assert.rejects(() => cache.set("ticker", circular), TypeError);

  const afterFailure = await readdir(cache.cacheDir);
  assert.deepEqual(afterFailure, [cacheFilename("ticker")]);
  const intact = JSON.parse(
    await readFile(join(cache.cacheDir, cacheFilename("ticker")), "utf8"),
  );
  assert.equal(intact.data.seq, 11);
});

test("in-flight coalescing shares one upstream fetch across concurrent callers", async (t) => {
  const dataDir = await makeDataDir(t);
  let clock = 1_000;
  const cache = createCache({ dataDir, now: () => clock });

  const started = deferred();
  const gate = deferred();
  let calls = 0;
  const fetcher = async () => {
    calls += 1;
    started.resolve();
    return gate.promise;
  };

  const pending = Promise.all([
    cache.getOrFetch("market", 60_000, fetcher),
    cache.getOrFetch("market", 60_000, fetcher),
    cache.getOrFetch("market", 60_000, fetcher),
  ]);

  await started.promise;
  assert.equal(calls, 1);
  clock = 1_250;
  gate.resolve({ btc: 155_000 });

  const results = await pending;
  assert.equal(calls, 1);
  for (const res of results) {
    assert.deepEqual(res, {
      data: { btc: 155_000 },
      fetchedAt: 1_250,
      isFresh: true,
      stale: false,
      source: "fetch",
      revalidating: false,
    });
  }
});

test("getOrFetch falls back to stale cache on upstream error and throws on cold miss", async (t) => {
  const dataDir = await makeDataDir(t);
  let clock = 1_000;
  const cache = createCache({ dataDir, now: () => clock });

  const upstreamError = new Error("503 upstream unavailable");

  // Cold miss with failing upstream throws.
  await assert.rejects(
    () =>
      cache.getOrFetch("cold", 60_000, async () => {
        throw upstreamError;
      }),
    /503 upstream unavailable/,
  );

  // Seed disk cache, advance past TTL, and restart.
  await cache.set("prices", { eth: 5_200 });
  clock = 120_000;
  const restarted = createCache({ dataDir, now: () => clock });

  const fallback = await restarted.getOrFetch("prices", 60_000, async () => {
    throw upstreamError;
  });
  assert.equal(fallback.isFresh, false);
  assert.equal(fallback.stale, true);
  assert.equal(fallback.source, "disk");
  assert.equal(fallback.fetchedAt, 1_000);
  assert.deepEqual(fallback.data, { eth: 5_200 });
  assert.equal(fallback.error, upstreamError);

  // Opting out of stale fallback propagates the upstream error.
  await assert.rejects(
    () =>
      restarted.getOrFetch(
        "prices",
        60_000,
        async () => {
          throw upstreamError;
        },
        { fallbackToStale: false },
      ),
    /503 upstream unavailable/,
  );
});

test("staleWhileRevalidate returns stale immediately and refreshes in background with coalescing", async (t) => {
  const dataDir = await makeDataDir(t);
  let clock = 1_000;
  const errors = [];
  const cache = createCache({
    dataDir,
    now: () => clock,
    onError: (err, ctx) => errors.push({ err, ctx }),
  });

  await cache.set("dashboard", { v: 1 });
  clock = 120_000; // Past 60s TTL.

  const gate = deferred();
  let calls = 0;
  const fetcher = async () => {
    calls += 1;
    return gate.promise;
  };

  // Two concurrent SWR callers both get stale data immediately and share one refresh.
  const [swrA, swrB] = await Promise.all([
    cache.getOrFetch("dashboard", 60_000, fetcher, {
      staleWhileRevalidate: true,
    }),
    cache.getOrFetch("dashboard", 60_000, fetcher, {
      staleWhileRevalidate: true,
    }),
  ]);
  assert.equal(calls, 1);
  assert.deepEqual(swrA.data, { v: 1 });
  assert.equal(swrA.stale, true);
  assert.equal(swrA.revalidating, true);
  assert.deepEqual(swrB.data, { v: 1 });
  assert.equal(swrB.revalidating, true);

  // A foreground caller joins the same in-flight refresh instead of starting a second one.
  const foregroundPromise = cache.getOrFetch("dashboard", 60_000, fetcher);
  assert.equal(calls, 1);

  clock = 121_000;
  gate.resolve({ v: 2 });

  const foreground = await foregroundPromise;
  await cache.whenIdle();

  assert.equal(calls, 1);
  assert.deepEqual(foreground.data, { v: 2 });
  assert.equal(foreground.isFresh, true);
  assert.equal(foreground.source, "fetch");

  // Verify the background refresh persisted to disk.
  const restarted = createCache({ dataDir, now: () => 122_000 });
  assert.deepEqual(await restarted.peek("dashboard", 60_000), {
    data: { v: 2 },
    fetchedAt: 121_000,
    isFresh: true,
  });

  // A failing background refresh reports to onError and keeps the existing stale value.
  clock = 300_000;
  const staleRes = await cache.getOrFetch(
    "dashboard",
    60_000,
    async () => {
      throw new Error("rate limited");
    },
    { staleWhileRevalidate: true },
  );
  assert.deepEqual(staleRes.data, { v: 2 });
  assert.equal(staleRes.revalidating, true);
  await cache.whenIdle();

  assert.equal(errors.length, 1);
  assert.equal(errors[0].ctx.key, "dashboard");
  assert.equal(errors[0].ctx.phase, "revalidate");
  assert.match(errors[0].err.message, /rate limited/);

  const stillCached = await cache.peek("dashboard", 60_000);
  assert.deepEqual(stillCached.data, { v: 2 });
});

test("delete removes both memory and disk entries", async (t) => {
  const dataDir = await makeDataDir(t);
  const cache = createCache({ dataDir, now: () => 1_000 });

  await cache.set("temp", { ok: true });
  assert.notEqual(await cache.peek("temp", 60_000), null);

  await cache.delete("temp");
  assert.equal(await cache.peek("temp", 60_000), null);

  // Deleting a missing key is a no-op.
  await cache.delete("never-existed");
});
