# Architecture and decisions

Pockets lets people own and run small web applications without managing containers themselves. The founding vision is in [PROJECT_BRIEF.md](../PROJECT_BRIEF.md).

## First milestone

Target macOS on Apple Silicon first. Use Tauri 2, React, TypeScript, and Rust. Keep the native implementation in the desktop crate until a second consumer justifies extracting a core library.

The frontend presents state and invokes a small set of native operations. Rust owns the bundled manifest, runtime executable discovery, process arguments, container identity, port discovery, readiness, and browser opening. Never accept shell commands, image names, URLs, host paths, or container arguments from the webview.

Use the installed Podman CLI through argument arrays, not a shell. This is simpler than exposing its API socket and avoids giving the webview engine access. Runtime operations live behind a narrow module boundary; a Docker implementation can come later. Missing Podman and an unavailable engine are distinct, actionable states. Do not install Podman, create/start its VM, or change runtime configuration automatically.

Use one first-party, single-container example. Build its image explicitly as a development step; starting a Pocket must not build arbitrary code. Use `--pull=never` so a missing local development image cannot trigger an unexpected registry pull. Publish the installed port on `127.0.0.1` and verify the mapping. Require HTTP readiness before enabling browser access. Keep a stable container identity and ownership labels, and refuse to control a container whose ownership does not match. Reconcile state from Podman rather than trusting frontend state.

## Trust and permissions

- A manifest is data, not a deployment script.
- No privileged mode, host networking, engine socket, or arbitrary host mounts.
- Drop container capabilities and disable privilege escalation.
- Persist example data in a dedicated named volume, not a host directory.
- Bind only to loopback; container isolation does not make untrusted code harmless.
- Development images use a local tag; digest pinning and provenance are prerequisites for trusted third-party distribution.
- Local source import is approval-gated; no image-only/catalog installation, secrets UI, marketplace, agent execution, or LAN access yet.

The first milestone uses a bundled, constrained manifest, not a promise that arbitrary community applications are safe to install. Secret management requires a later native storage design; do not store API keys in frontend state or manifests.

Start recreates an owned stopped container from known-safe arguments while keeping its separately owned named volume. It does not restart saved container settings. Stop checks container identity and ownership but does not require a safe HTTP mapping: a broken or unsafe mapping must not prevent shutting down an owned container. Opening and HTTP probes always require the strict loopback mapping.

Podman serves each installed site directly at its assigned loopback port.
Desktop saves the hostname, installation ID, and port together. New installations
take an available port starting at 18788. An existing running Pocket adopts its
verified loopback port on upgrade. Its next Desktop-managed start pins that port
explicitly. Desktop retains address assignments across stops and deletions and
reports a collision if another process takes a saved port. The Mac's loopback
forward is reachable while the machine and container are running, including
after Desktop quits. Development previews use their own live-reload proxy.

All Pocket names resolve to loopback. The port selects the application, and
browser cookies are scoped by hostname across ports. Direct publication assumes
user-approved local applications.

### Private tailnet access via Tailscale Serve

Installed Pockets can be explicitly opted in per Pocket to private tailnet
access via Tailscale Serve (`tailscale serve --bg --yes --https=<port>
http://127.0.0.1:<port>`). While at least one Pocket is opted in and Desktop is
running, a lightweight native loopback portal (`127.0.0.1:18787`, proxied on
Tailscale HTTPS port `443` when free) serves an HTML Pocket launcher at `/` and
`302 Found` redirects from `/<pocket-slug>` to each Pocket's own HTTPS port so
every Pocket keeps its own origin and root `/` path. Containers remain bound
exclusively to `127.0.0.1` — Podman never binds `0.0.0.0`, and `tailscale
funnel` (public internet) and `tailscale serve reset` are never invoked. Native
Rust owns Tailscale CLI discovery, fixed argument execution, DNS name
validation, HTML escaping, and per-Pocket opt-in state (`tailscale-shares.json`).
Before adding or removing any route (including port `443`), native code inspects
`tailscale serve status --json` so Pockets refuses to overwrite or remove
unrelated user Serve routes and gracefully falls back to direct port URLs if
port `443` is occupied. Stopping a Pocket pauses its owned Serve route while
keeping the opt-in; starting restores it; deleting removes both the route and
the saved preference; disabling or deleting the last shared Pocket stops the
portal and removes the port `443` route.

## Lifecycle

The desktop window is a controller, not a container supervisor. Closing the window does not imply that a Pocket stopped. Clearly document this limitation; use explicit Stop. Do not implement automatic VM shutdown or background services yet. Serialize mutating lifecycle operations and bound external command and HTTP waits so the UI cannot hang indefinitely.

## First-party resource monitoring

Resource Monitor is an on-demand trusted Desktop view, not a privileged
ordinary Pocket. Its fixed native read-only command reconciles installed
identities and ownership before sampling running container IDs. No engine
socket or cross-pocket metrics are exposed to applications. Refreshes pause
when closed or hidden. Only managed pocket containers are sampled; whole-host
usage and VM usage are not collected. Totals exclude the Desktop process
and runtime overhead. Unavailable measurements are not reported as zero. See
[Resource Monitor](resource-monitor.md) for interpretation and acceptance.

## External-agent creation boundary

Creation can start in Desktop or in an external coding tool. Desktop accepts a
bounded display name, single-component folder slug and optional brief; the
native picker selects the parent directory. Native code creates only a new
subfolder and registers a linked draft. Existing destinations, symlinks and
managed-store locations are refused. Partial failures preserve new source with
recovery instructions. Creation never invokes providers, Node or the engine.

The Rust build embeds `templates/basic-pocket` as text; the Node creation CLI
renders the same files. Both include `AGENTS.md`, which points to the portable
`POCKETS.md` instructions and `POCKETS_BRIEF.md`. The CLI also validates against
the canonical manifest JSON Schema.
Neither creation path executes scripts, installs dependencies or builds images.
The folder path clipboard action resolves the registered link natively.

Each project keeps its native identity, private versioned development-folder
metadata, filtered working snapshot and separately pinned installed revision.
Absolute source locations are not portable manifest fields. Linking repeatedly
reuses the same identity; Sync captures a validated snapshot and Update applies
a built candidate while preserving installed data. External edits never deploy.

Project-owned preview watches host edits into a separate stable-root mirror,
using the shared container-visible-file gate before restart/style publication.
The isolated container never mounts the external folder, install snapshot or
installed data volume. Start/Stop, status, bounds and thumbnails resolve by
project ID without AI sessions. The initial profile is dependency-free Node.
Desktop can approve exact HTTPS hosts for a per-project gateway on a separate
bridge. See [live preview](live-preview.md).

The generated application is ordinary editable source, not a new framework or an AI-dependent runtime. Its image supports only the architecture actually built unless a multi-architecture build is explicitly verified. Agents must get approval before executing generated code or building/running containers, and must not mount host directories or the engine socket into a generated application's preview.

Passing manifest validation is not a security endorsement. Generated source,
dependency scripts and Containerfiles remain untrusted until reviewed. Start
authorizes preview execution; Build/Update authorizes packaging/installation
from a captured context, with no second warning or Keep step. All saved projects route to the development workspace. It can export a verified
snapshot and link it back to the same project. The app no longer registers built-in
agent or provider commands. See [migration notes](creation.md#existing-pockets-and-older-workspaces)
for recovery of earlier workspace files. See [managed creation](managed-creation.md)
for the Build/Open boundary and retained protections.

## Principal risks

1. **Runtime onboarding:** Podman on macOS requires a Linux VM. Detection is not installation or repair. Native GUI PATH can differ from terminal PATH; search common installation locations as well.
2. **Engine authority:** the native backend has substantial host privileges through Podman. Keep its command surface fixed, validate ownership, and never interpolate shell strings.
3. **Readiness versus running:** an active container may still be starting or unhealthy. Expose useful failure messages and only open an inspected loopback endpoint.
4. **Distribution:** unsigned local Tauri development is not a signed/notarized installer. Windows support requires separate verification.
5. **Specification drift:** schema, Rust deserialization, and examples must agree. Keep this alpha subset small; reject unsupported fields rather than silently granting permissions.
6. **Persistence:** stopping must preserve data. Destructive uninstall, updates, and backups need explicit later policies.
