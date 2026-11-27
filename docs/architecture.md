# Architecture and Security Boundaries

Pockets lets people own and run small self-hosted web applications locally without managing containers directly.

## Runtime & Daemon Architecture

Pockets ships as a single cross-platform Rust binary (`pockets`, built from `crates/pockets-core` and `crates/pockets-cli`) embedding its React + TypeScript web dashboard (`apps/desktop`):

- **Loopback-only control plane:** `pockets serve` binds exclusively to `127.0.0.1:18786` (serving `http://pockets.localhost:18786/`). On remote/headless hosts, the daemon still binds strictly to `127.0.0.1` and can optionally be reached privately via Tailscale Serve (`--tailscale`) or an explicit trusted reverse-proxy origin (`--public-url` / `POCKETS_PUBLIC_URL`).
- **Authenticated local API:** Every request validates the `Host` header against loopback dashboard hosts (`pockets.localhost`, `localhost`, `127.0.0.1`) plus any explicitly configured Tailscale or `--public-url` origin, enforces strict `Origin` checks on state-changing `POST` requests, and protects `/api/*` with a per-machine 256-bit random session key (`session.key`, `0600` permissions) verified via an `HttpOnly; SameSite=Strict` cookie (`Secure` when served over HTTPS) or `Authorization: Bearer` header.
- **No shell interpolation:** Rust owns runtime discovery, fixed argument arrays, container identity, port allocation, readiness probes, Tailscale Serve routing, and secret handling. Never accept arbitrary shell commands, image names, host mounts, or container flags from the frontend.
- **Podman engine:** Pockets uses the installed Podman CLI via fixed argument arrays. On Linux, rootless Podman runs natively without a VM. On macOS and Windows, `pockets` can automatically initialize and start a Podman machine VM (`podman machine init` / `podman machine start`) when needed.

## Trust and Container Isolation

- A `pocket.yaml` manifest is data, not a deployment script.
- No privileged mode, host networking, engine socket exposure, or arbitrary host directory mounts.
- Containers run with dropped Linux capabilities (`--cap-drop=ALL`) and `no-new-privileges`.
- Persistent Pocket data lives in a dedicated Podman named volume, separate from source snapshots.
- Installed Pockets bind strictly to loopback (`127.0.0.1`) on persistent assigned ports starting at `18788`, reachable via `http://<name>.localhost:<port>`.
- Runtime launch pins the immutable built image ID (`sha256:...`) rather than trusting mutable tags.

## Cross-Platform Runtime Secrets

- Per-Pocket runtime variables and secrets are stored separately for **Preview** and **Installed** environments.
- On macOS, Linux, and Windows, secrets are stored in an owner-only (`0700` dir / `0600` file, atomic write) local vault inside Pockets' data directory so secrets survive unsigned CLI/Homebrew upgrades and background service restarts cleanly (on macOS, readable legacy Keychain items are transparently migrated into the vault on first read).
- Secrets are injected into Podman via stdin/scoped engine secrets, never written into snapshots, build contexts, manifests, or container CLI arguments.

## Private Tailnet Access via Tailscale Serve

Installed Pockets can be explicitly opted in per Pocket to private tailnet access via Tailscale Serve (`tailscale serve --bg --yes --https=<port> http://127.0.0.1:<port>`):

- While at least one Pocket is opted in and the Pockets daemon is running, a lightweight loopback portal (`127.0.0.1:18787`, proxied on Tailscale HTTPS port `443` when free) serves an HTML Pocket launcher at `/` and `302 Found` redirects from `/<pocket-slug>` to each Pocket's own HTTPS port.
- Containers remain bound exclusively to `127.0.0.1`. Podman never binds `0.0.0.0`, and `tailscale funnel` (public internet) and `tailscale serve reset` are never invoked.
- Before adding or removing any route, Pockets inspects `tailscale serve status --json` and refuses to overwrite or remove unrelated user Serve routes, falling back to direct port URLs if port `443` is occupied.
- Stopping a Pocket pauses its owned Serve route while remembering opt-in; starting restores it; deleting removes both the route and saved preference.

## External Development & Live Preview

- Creation can start in the Pockets Web UI or CLI (`pockets new`, `pockets link .`). Creation writes a standalone starter (`templates/basic-pocket`) and never executes generated code or builds containers automatically.
- Each project keeps its native identity, private linked-folder metadata, filtered working snapshot, and separately pinned installed revision. External edits never deploy automatically.
- Live preview mirrors filtered source into a private read-only mount on an isolated container bridge with a disposable preview data volume and an opt-in per-project HTTPS egress gateway (`previewApis`).
- The preview renders in an isolated `<iframe>` on the preview proxy's separate loopback origin (cross-origin isolated from the `pockets.localhost:18786` control cookie). See [live preview](live-preview.md).

## First-Party Resource Monitoring

Resource Monitor is an on-demand read-only view backed by `podman ps` + `podman stats --no-stream` restricted strictly to ownership-checked managed Pocket containers. Totals exclude the Pockets daemon, host OS, and Podman VM overhead. See [Resource Monitor](resource-monitor.md).
