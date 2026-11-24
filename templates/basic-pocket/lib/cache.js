// Dependency-free two-tier (memory + atomic disk JSON) cache under DATA_DIR/cache.
// Designed for API-backed Pockets that want instant cold/cross-device startup
// via stale-while-revalidate without blocking first paint on upstream fetches.

import { createHash, randomBytes } from "node:crypto";
import { mkdir, readFile, rename, unlink, writeFile } from "node:fs/promises";
import { join } from "node:path";

const MAX_PREFIX_LENGTH = 64;

function assertValidKey(key) {
  if (typeof key !== "string" || key.length === 0) {
    throw new TypeError("Cache key must be a non-empty string");
  }
}

function assertValidTtl(ttlMs) {
  if (typeof ttlMs !== "number" || Number.isNaN(ttlMs) || ttlMs < 0) {
    throw new TypeError("Cache ttlMs must be a non-negative number");
  }
}

/**
 * Convert an arbitrary cache key into a collision-resistant, filesystem-safe
 * JSON filename with no path separators or traversal segments.
 */
export function cacheFilename(key) {
  assertValidKey(key);
  const hash = createHash("sha256").update(key).digest("hex").slice(0, 16);
  const safePrefix = key
    .toLowerCase()
    .replace(/[^a-z0-9._-]+/g, "_")
    .replace(/^[._-]+|[._-]+$/g, "")
    .slice(0, MAX_PREFIX_LENGTH);
  return `${safePrefix || "key"}-${hash}.json`;
}

/**
 * Create a two-tier memory + disk JSON cache stored under `<dataDir>/cache`.
 *
 * Options:
 * - `dataDir`: base persistent directory (default `process.env.DATA_DIR ?? "/data"`)
 * - `cacheDir`: explicit cache directory override (default `join(dataDir, "cache")`)
 * - `now`: clock function returning epoch ms (default `Date.now`)
 * - `onError`: optional `(err, context) => void` hook for background refresh or disk write errors
 */
export function createCache(options = {}) {
  const dataDir = options.dataDir ?? process.env.DATA_DIR ?? "/data";
  const cacheDir = options.cacheDir ?? join(dataDir, "cache");
  const now = options.now ?? Date.now;
  const defaultOnError = options.onError;

  const memory = new Map();
  const inFlight = new Map();
  const diskReads = new Map();
  const writeQueues = new Map();
  let tmpSeq = 0;

  function filePathFor(key) {
    return join(cacheDir, cacheFilename(key));
  }

  function isEntryFresh(entry, ttlMs) {
    return ttlMs > 0 && now() - entry.fetchedAt < ttlMs;
  }

  function reportError(handler, err, context) {
    const fn = handler ?? defaultOnError;
    if (typeof fn === "function") {
      try {
        fn(err, context);
      } catch {
        // Never allow an error hook to crash background work.
      }
    }
  }

  async function readDisk(key) {
    const filePath = filePathFor(key);
    let raw;
    try {
      raw = await readFile(filePath, "utf8");
    } catch (err) {
      if (err.code === "ENOENT" || err.code === "ENOTDIR") {
        return null;
      }
      return null;
    }

    let parsed;
    try {
      parsed = JSON.parse(raw);
    } catch {
      return null; // Corrupted or truncated file; treat as a cache miss.
    }

    if (
      !parsed ||
      typeof parsed !== "object" ||
      parsed.key !== key ||
      typeof parsed.fetchedAt !== "number" ||
      !Number.isFinite(parsed.fetchedAt) ||
      !("data" in parsed)
    ) {
      return null;
    }

    return { data: parsed.data, fetchedAt: parsed.fetchedAt };
  }

  function enqueueDiskWrite(key, task) {
    const previous = writeQueues.get(key) ?? Promise.resolve();
    const run = previous.then(task, task);
    const tail = run.then(
      () => {
        if (writeQueues.get(key) === tail) {
          writeQueues.delete(key);
        }
      },
      () => {
        if (writeQueues.get(key) === tail) {
          writeQueues.delete(key);
        }
      },
    );
    writeQueues.set(key, tail);
    return run;
  }

  async function writeDiskAtomic(key, entry) {
    if (entry.data === undefined) {
      throw new TypeError("Cache value must not be undefined");
    }
    const payload = `${JSON.stringify({
      key,
      fetchedAt: entry.fetchedAt,
      data: entry.data,
    })}\n`;

    return enqueueDiskWrite(key, async () => {
      await mkdir(cacheDir, { recursive: true });
      const fileName = cacheFilename(key);
      const filePath = join(cacheDir, fileName);
      const nonce = randomBytes(4).toString("hex");
      const tmpPath = join(
        cacheDir,
        `.${fileName}.${process.pid}.${++tmpSeq}.${nonce}.tmp`,
      );
      try {
        await writeFile(tmpPath, payload, "utf8");
        await rename(tmpPath, filePath);
      } catch (err) {
        await unlink(tmpPath).catch(() => {});
        throw err;
      }
    });
  }

  function readDiskCoalesced(key) {
    const existing = diskReads.get(key);
    if (existing) {
      return existing;
    }
    const promise = readDisk(key).finally(() => {
      if (diskReads.get(key) === promise) {
        diskReads.delete(key);
      }
    });
    diskReads.set(key, promise);
    return promise;
  }

  async function readLocal(key) {
    if (memory.has(key)) {
      return { entry: memory.get(key), source: "memory" };
    }
    const diskEntry = await readDiskCoalesced(key);
    if (!diskEntry) {
      if (memory.has(key)) {
        return { entry: memory.get(key), source: "memory" };
      }
      return null;
    }
    // Only hydrate if nothing wrote a newer value while disk read was pending.
    if (!memory.has(key)) {
      memory.set(key, diskEntry);
      return { entry: diskEntry, source: "disk" };
    }
    return { entry: memory.get(key), source: "memory" };
  }

  function startFetch(key, fetcher, onError) {
    const existing = inFlight.get(key);
    if (existing) {
      return existing;
    }

    const promise = (async () => {
      const data = await fetcher();
      if (data === undefined) {
        throw new TypeError("Cache fetcher must not return undefined");
      }
      const entry = { data, fetchedAt: now() };
      memory.set(key, entry);
      try {
        await writeDiskAtomic(key, entry);
      } catch (err) {
        reportError(onError, err, { key, phase: "write" });
      }
      return entry;
    })();

    const tracked = promise.finally(() => {
      if (inFlight.get(key) === tracked) {
        inFlight.delete(key);
      }
    });

    inFlight.set(key, tracked);
    return tracked;
  }

  /**
   * Local-only read with zero upstream calls.
   * Checks memory first, then DATA_DIR/cache (hydrating memory on hit).
   * Returns `{ data, fetchedAt, isFresh }` even when past TTL, or `null` if absent.
   */
  async function peek(key, ttlMs = Infinity) {
    assertValidKey(key);
    assertValidTtl(ttlMs);

    const local = await readLocal(key);
    if (!local) {
      return null;
    }
    return {
      data: local.entry.data,
      fetchedAt: local.entry.fetchedAt,
      isFresh: isEntryFresh(local.entry, ttlMs),
    };
  }

  /**
   * TTL-aware read with in-flight request coalescing, graceful stale fallback
   * on upstream error, and optional stale-while-revalidate background refresh.
   *
   * Options:
   * - `forceRefresh`: bypass fresh cache and fetch upstream (default `false`)
   * - `staleWhileRevalidate`: return stale local entry immediately and refresh in background (default `false`)
   * - `fallbackToStale`: return stale entry if `fetcher` fails (default `true`)
   * - `onError`: optional `(err, context) => void` hook for background refresh / disk write errors
   */
  async function getOrFetch(key, ttlMs, fetcher, callOptions = {}) {
    assertValidKey(key);
    assertValidTtl(ttlMs);
    if (typeof fetcher !== "function") {
      throw new TypeError("Cache fetcher must be a function");
    }

    const {
      forceRefresh = false,
      staleWhileRevalidate = false,
      fallbackToStale = true,
      onError,
    } = callOptions;

    const local = await readLocal(key);
    if (local && !forceRefresh) {
      const fresh = isEntryFresh(local.entry, ttlMs);
      if (fresh) {
        return {
          data: local.entry.data,
          fetchedAt: local.entry.fetchedAt,
          isFresh: true,
          stale: false,
          source: local.source,
          revalidating: false,
        };
      }
      if (staleWhileRevalidate) {
        startFetch(key, fetcher, onError).catch((err) => {
          reportError(onError, err, { key, phase: "revalidate" });
        });
        return {
          data: local.entry.data,
          fetchedAt: local.entry.fetchedAt,
          isFresh: false,
          stale: true,
          source: local.source,
          revalidating: true,
        };
      }
    }

    try {
      const entry = await startFetch(key, fetcher, onError);
      return {
        data: entry.data,
        fetchedAt: entry.fetchedAt,
        isFresh: true,
        stale: false,
        source: "fetch",
        revalidating: false,
      };
    } catch (err) {
      const staleLocal = local ?? (await readLocal(key));
      if (fallbackToStale && staleLocal) {
        return {
          data: staleLocal.entry.data,
          fetchedAt: staleLocal.entry.fetchedAt,
          isFresh: false,
          stale: true,
          source: staleLocal.source,
          revalidating: false,
          error: err,
        };
      }
      throw err;
    }
  }

  /**
   * Store a value directly in memory and atomically on disk.
   */
  async function set(key, data, fetchedAt = now()) {
    assertValidKey(key);
    if (data === undefined) {
      throw new TypeError("Cache value must not be undefined");
    }
    if (typeof fetchedAt !== "number" || !Number.isFinite(fetchedAt)) {
      throw new TypeError("Cache fetchedAt must be a finite number");
    }
    const entry = { data, fetchedAt };
    memory.set(key, entry);
    await writeDiskAtomic(key, entry);
    return { data: entry.data, fetchedAt: entry.fetchedAt, isFresh: true };
  }

  /**
   * Remove an entry from memory and disk.
   */
  async function del(key) {
    assertValidKey(key);
    memory.delete(key);
    await enqueueDiskWrite(key, async () => {
      try {
        await unlink(filePathFor(key));
      } catch (err) {
        if (err.code !== "ENOENT") {
          throw err;
        }
      }
    });
  }

  /**
   * Wait for all currently in-flight fetches and pending disk writes to settle.
   */
  async function whenIdle() {
    while (inFlight.size > 0 || writeQueues.size > 0) {
      await Promise.allSettled([...inFlight.values(), ...writeQueues.values()]);
    }
  }

  return {
    cacheDir,
    peek,
    getOrFetch,
    set,
    delete: del,
    whenIdle,
  };
}
