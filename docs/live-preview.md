# Live development preview

## External project workflow

Linked projects now have a project-owned development workspace and preview.
Choose **Link project**, open **Develop**, and choose **Start** in the preview
pane. Edit the linked folder in any coding tool. The native host watches saved
files even while that tool has focus. **Open in browser** uses the same loopback
proxy as the embedded pane.

Preview is explicit code execution. It does not build or install an image.
**Sync source** captures a separate authoritative snapshot for a later explicit
**Build/Update**. Automatic preview refresh never publishes an installed update
or rewrites external source. Installed data and disposable preview data stay
separate. Preview data survives edits/reconfiguration; Stop discards that test
data and prevents further saved edits from restarting the preview. Stop before
relinking or unlinking the development folder.

The initial profile needs a root `server.js`, a valid Pocket manifest and
dependency-free Node runtime code. `devDependencies` may be declared but are
not installed. The existing Podman VM must be operational and
`docker.io/library/node:24-alpine` cached. Preview never pulls an image or
installs packages. Preview runs on an internal network. Start from Pockets to
load saved Preview variables and any approved API access. Variables stay
available across live reload until Stop, which removes the engine secrets.
See [runtime variables](creation.md#runtime-variables).

## Preview API access

Declare each service the project needs in `pocket.yaml`. A coding tool can add
these declarations while implementing the connection.

```yaml
previewApis:
  - host: api.kraken.com
    name: Kraken pricing
    purpose: Show live BTC prices.
```

Click **Start** in Pockets. Preview starts with its saved approvals. If the
Pocket requests new hostnames at startup or during a live edit, Pockets opens,
highlights, and focuses **Preview API access** in the centre pane. The new hosts
stay blocked until you choose **Allow**. This saves approval for the linked
folder and updates the preview while keeping its URL, variables, and test data.
Choose **Ignore** to dismiss the request for this preview session.

**Allowed hostnames** shows saved access. To edit the list, stop Preview and
use **Remove** beside a hostname or **Add hostname** to enter one manually.
Relinking to a different folder or replacing the linked folder clears its
approvals.

The preview container stays on an isolated bridge. Pockets adds a separate
gateway for that project and sets `POCKETS_HTTPS_GATEWAY` for the app’s server.
The allowlist governs the preview server. Browser requests follow the browser’s
network rules.
The gateway allows HTTPS on port 443, accepts GET and HEAD, and rejects
unapproved hosts, redirects, and private or special-use addresses. Destinations
need public IPv4 DNS records. The gateway uses bounded requests and caps
responses at 4 MiB. An approved hostname can receive the request path, query,
and supported API headers. Use development credentials and read provider keys
from runtime variables in server code.

Use the exact provider URL on the server. The same code can call the provider
directly when it runs as an installed app.

```js
async function fetchApi(endpoint, headers = {}) {
  const gateway = process.env.POCKETS_HTTPS_GATEWAY;
  const url = gateway ? new URL("/fetch", gateway) : new URL(endpoint);
  if (gateway) url.searchParams.set("url", endpoint);
  return fetch(url, { headers, redirect: "error" });
}
```

Supported headers include `Authorization`, `Accept`, `API-Key`, conditional
request headers, and `X-…-Key` and `X-…-Token` headers. The gateway preserves API
status codes such as `429` and supported response headers including
`Retry-After`, and does not cache responses itself. Check `response.ok` and
handle errors and stale data in the app. API-backed Pockets can opt into the
starter’s `lib/cache.js` helper under `DATA_DIR/cache` for shared
stale-while-revalidate startup across browsers, devices, and restarts.

## Host watcher and runtime

- Project services resolve registered folder metadata, manifest and runtime
  configuration natively. No AI session or AI workspace allocation is involved.
- Metadata polling every 400 ms ignores the same excluded paths as snapshot
  capture. Two equal observations settle a batch; every 15 seconds a full
  capture reconciles content even if timestamps were preserved. Changes made
  during a refresh are picked up in a subsequent batch.
- A validated capture enters a private mirror with a stable root inode, mounted
  read-only at `/work`. Secrets, Git metadata, local data, caches and dependencies
  never enter it. The external folder and install snapshot are never mounted.
- Host operations hold the shared lifecycle lock. Before restart or style
  publication, the existing bounded container-visible-file gate proves changed
  bytes match and removed paths are absent through the Podman VM mount.
- CSS-only changes swap styles without restart. Other source changes restart
  and full-reload after readiness. A small trusted supervisor keeps the
  container alive after application crashes, so a later edit can still pass the
  visibility gate and recover. Unsupported/malformed edits report an error;
  unchanged failed source is not executed repeatedly.
- Port/storage changes recreate only the owned preview container. They preserve
  its active variables, data volume, and proxy origin. Failed configuration
  changes can be reverted. The startup sweep skips active previews.
- Native commands are `project_preview_start`, `project_preview_status`,
  `project_preview_stop`, `project_preview_open`, `project_preview_set_bounds`
  `project_preview_hide` and `project_preview_logs`.

The opt-in `project_preview_live` test exercises the production watcher against
a disposable project and existing Podman engine, including CSS refresh, code
failure/recovery, stable URL/data, configuration recovery and Stop. Frontend
tests cover project command routing and the development workspace.

## Embedded preview

The preview renders inside a sandboxed `<iframe>` on the preview proxy’s
separate loopback origin, keeping it cross-origin isolated from the Pockets
control API and session cookie. The proxy injects a live client for stylesheet
swaps and reloads. **Open in browser** uses the same preview URL. Collapsing
the pane keeps the watcher running. **Stop** tears down the preview container,
proxy, test volume, and engine copies of its variables.

**Preview logs → Read logs** fetches recent output from the verified preview
container on demand. Build output is available under **Technical details** in
the installation card. Preview state and installed-app status appear in
separate controls.
