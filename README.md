# Pockets

**Describe it. Own it. Share it.**

Pockets is an open-source, local-first platform for creating, running, and owning small self-hosted web apps on macOS, Linux, and homelabs. Use your favorite AI coding tool to build a Pocket, watch it live-reload in an isolated preview, then install it as a persistent local app — optionally reachable from your phone over private Tailscale Serve.

Installed Pockets are ordinary containerized web apps with persistent volumes. AI helps create them, but **installed Pockets never depend on AI to run.**

---

## Install

Pockets installs **Podman** alongside the `pockets` binary so you can get started in one command.

### macOS (Homebrew)

```sh
brew install runpockets/tap/pockets
brew services start pockets
pockets
```

1. Installs `pockets` and `podman` automatically.
2. Runs Pockets in the background (`brew services`) and initializes/starts your Podman VM automatically on first run.
3. Opens `http://pockets.localhost:18786/` in your default browser already signed in.

Bookmark `http://pockets.localhost:18786/` (or use Safari **File → Add to Dock…** / Chrome **Install App**) — from then on your saved URL just works.

### Linux (Desktop, Raspberry Pi, Homelab, VPS)

```sh
curl -fsSL https://raw.githubusercontent.com/runpockets/pockets/main/packaging/install.sh | sh
pockets
```

Installs Podman (via `apt`, `dnf`, or `pacman` if missing), installs the `pockets` binary, and enables the `systemd --user` background service automatically (pass `POCKETS_NO_SERVICE=1` to skip auto-enabling). Containers run rootlessly with zero VM overhead on Linux.

---

## How It Works

### 1. Create or link a Pocket
Choose **New pocket** in the dashboard (or run `pockets new "My App"` / `pockets link .` from your terminal). Pockets scaffolds a standalone starter with `pocket.yaml`, `Containerfile`, tests, and `AGENTS.md` pointing your coding agent (Claude Code, Cursor, Codex, etc.) at the Pocket runtime contract in `POCKETS.md` and your app brief in `POCKETS_BRIEF.md`.

### 2. Develop with Live Preview
Open the Pocket folder in your preferred coding tool and click **Start** in **Workspace preview**.
* Saved edits refresh automatically (CSS swaps without restarting; code edits restart and reload after `/health` passes).
* Preview runs in an isolated container with a disposable preview data volume and explicit per-host HTTPS API approvals (`previewApis` in `pocket.yaml`).
* External edits never overwrite your installed app or its data until you click **Build & start** / **Update**.

### 3. Own it locally (and reach it from your phone)
* Each installed Pocket gets a persistent loopback address (`http://<name>.localhost:18788+`) backed by a dedicated Podman named volume.
* Store plain config and API secrets separately per environment (**Preview** vs **Installed app**) in macOS Keychain or an owner-only (`0600`) local vault on Linux/Windows.
* Export and restore `.tar` data volume backups right from the browser, inspect version diffs, roll back installed versions, and monitor per-Pocket CPU & memory.
* Toggle **Share on your tailnet** to publish a Pocket privately over HTTPS via `tailscale serve` (plus a port `443` Pocket Launcher and scannable QR code) — containers stay strictly on `127.0.0.1` and `tailscale funnel` is never used.

---

## CLI Quick Reference

```sh
pockets                     # Start daemon (if needed) and open dashboard in browser
pockets doctor              # Check Podman, Tailscale, data dir, and daemon status
pockets url                 # Print authenticated sign-in URL for another browser
pockets list                # List registered Pockets
pockets new <name>          # Create and register a new Pocket starter folder
pockets link [path]         # Link an existing Pocket folder (defaults to .)
pockets start <id>          # Start an installed Pocket
pockets stop <id>           # Stop a running Pocket (preserves data)
pockets service install     # Install & enable launchd (macOS) or systemd --user (Linux)
pockets service uninstall   # Stop & remove the background service
```

---

## Call for Community Pockets

We're starting v0.1 lean and inviting early builders to create and share Pockets (personal trackers, local dashboards, utilities, bookmarks, homelab tools, crypto/finance watchlists) ahead of our **HACS-style community Pockets catalog in v0.2**.

See [CONTRIBUTING.md](CONTRIBUTING.md), the starter template in [templates/basic-pocket](templates/basic-pocket), and [docs/pocket-spec.md](docs/pocket-spec.md) to build your first Pocket in minutes!

---

## Security Model

* **Authenticated loopback control plane:** The daemon binds strictly to `127.0.0.1:18786`, enforces `Host` and `Origin` checks against DNS rebinding and cross-port attacks, and protects `/api/*` with a per-machine 256-bit `HttpOnly; SameSite=Strict` session token (`0600`).
* **Least-privilege containers:** No privileged mode, no host networking, no arbitrary host directory mounts, dropped Linux capabilities, `no-new-privileges`, and no engine socket exposure to Pockets.
* **Explicit approval boundaries:** Creating or linking source runs no code; previewing and building/installing are explicit user actions; secrets never enter snapshots, build contexts, or container CLI args.

See [SECURITY.md](SECURITY.md) for vulnerability reporting and [docs/architecture.md](docs/architecture.md) for architectural details.

---

## Building from Source

Prerequisites: **Node.js 22+**, **Rust (stable)**, and **Podman**.

```sh
npm ci
npm run build
./target/release/pockets
```

### Development (hot-reload UI + local daemon)

```sh
# Terminal 1: run Rust daemon allowing Vite dev origin
npm run dev:daemon

# Terminal 2: run Vite dev server (proxies /api and /auth to daemon)
npm run dev:web
```

### Running Tests

```sh
npm run typecheck
npm run test:desktop
npm run test:gateway
npm run test:schema
npm test --prefix packages/pocket-tools
cargo test --workspace
```

---

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
