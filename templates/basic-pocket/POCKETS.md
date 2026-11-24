# Developing this Pocket

Use your preferred coding tool to edit this folder. Pockets manages live preview
and self-hosted installation.

Read `POCKETS_BRIEF.md` for the user's goal. Inspect the source before changing
it. Work in this folder and keep this project's identity; do not generate a
separate project or edit Pockets' managed snapshots.

## Current runtime contract

- Keep a root `server.js` and a dependency-free Node runtime for this initial
  development profile. Standard Node modules and browser APIs are available.
  Package installation and framework build servers are not supported yet.
- Bind the server to `HOST` and `PORT` provided by Pockets, with container
  defaults `0.0.0.0` and `8080`. Keep the manifest health endpoint working.
- Persist app data under `DATA_DIR` (default `/data`), never in source files.
  Use temporary data directories and ephemeral ports in tests. For API-backed
  apps, treat `DATA_DIR` (not browser `localStorage`) as the shared cache
  across browsers, devices, and container restarts.
- Keep `pocket.yaml`'s `metadata.id` stable. Update `Containerfile` when adding
  source/assets so the installed image includes everything the preview serves.
- For read-only APIs, declare each exact lowercase hostname under `previewApis`
  in `pocket.yaml` as an object with `host`, `name`, and `purpose`. Desktop asks
  the owner to approve new hosts. When `POCKETS_HTTPS_GATEWAY` is set, send GET
  or HEAD to `/fetch` on that URL with the full provider HTTPS URL in the `url`
  query parameter. The installed app uses the normal provider URL. Handle
  loading, stale data, and failures, and label samples.
- Read app settings from environment variables. Ask the user to configure
  their values in Desktop and start Preview there. Keep credentials out of
  source, browser responses, and logs.
- Source snapshots exclude secrets, Git metadata, local `data/`, caches and
  `node_modules`. Do not depend on excluded files in preview or installation.

## Instant startup for API-backed Pockets (optional)

- Use `DATA_DIR` — not browser `localStorage` — as the shared instant-load
  cache across browsers, devices, and container restarts. Browser storage starts
  empty on a second device, while `DATA_DIR` already holds persisted data.
- Avoid blocking first paint on upstream API fetches or sequential client boot
  waterfalls (for example, preferences → market data → status/quota checks).
  Serve persisted `DATA_DIR` cache immediately on initial page load
  (stale-while-revalidate) via a fast local bootstrap/peek path — such as an
  optional `GET /api/bootstrap` returning cached payloads plus `needsRefresh`
  flags — and refresh stale data and secondary status/quota checks in the
  background.
- The starter includes a dependency-free `lib/cache.js` helper
  (`createCache({ dataDir })`) with two-tier memory + atomic JSON persistence
  under `DATA_DIR/cache`:
  - `peek(key, ttlMs)` performs a local-only read with zero upstream calls and
    returns `{ data, fetchedAt, isFresh }` even when past TTL (or `null` if
    absent).
  - `getOrFetch(key, ttlMs, fetcher, options)` provides TTL-aware fetching,
    in-flight request coalescing, stale fallback on upstream error, and
    optional background revalidation (`staleWhileRevalidate: true`).
- Caching and bootstrap endpoints are opt-in at the app/helper level — never a
  mandatory platform contract (unlike `/health`), and `POCKETS_HTTPS_GATEWAY`
  does not cache responses blindly. Local-only or strict-freshness Pockets can
  ignore them.

## Iterate and verify

1. Review the files and brief, then implement the requested app in this folder.
2. Run `node --check server.js` and `npm test` when authorized by the user's
   session. The starter tests and server require no dependency installation.
3. In Pockets, choose **Develop → Start**. Saved edits refresh automatically;
   CSS swaps live and other changes reload after the health check. Broken code
   reports an error and retries after a fix. Use the preview to review design.
4. Pockets validates the manifest/source on Start and **Sync source**. After
   reviewing the result, the user chooses **Sync source → Build/Update** to
   install the captured version. Further external edits do not deploy it.

Preview uses separate, disposable test data. Stop discards preview data;
installed data is separate and is retained by installed-app Stop/Update.
Keep build, deploy, container execution, destructive data changes and dependency
installation within the user's explicit authorization. Do not start the Podman
VM, pull images or change host runtime configuration as a side effect of edits.

The README documents optional local/container commands. Report the changes,
checks performed and any runtime limitations when handing back to the user.
