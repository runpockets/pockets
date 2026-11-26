<picture>
  <source media="(prefers-color-scheme: dark)" srcset="brand/pockets-wordmark.svg" />
  <source media="(prefers-color-scheme: light)" srcset="brand/pockets-wordmark-light.svg" />
  <img alt="Pockets" src="brand/pockets-wordmark-light.svg" height="44" />
</picture>

**Run personal self-hosted web apps locally — without managing containers.**

Pockets is an open-source, local-first home for small self-hosted web apps on macOS, Linux, and homelabs. Build a Pocket in your preferred editor or coding tool, watch it live-reload in an isolated preview, and install it locally with persistent storage, native secrets, backups, version history, and optional private Tailscale access from your phone.

Installed Pockets are ordinary containerized web apps with their own data volumes — self-contained, local, and completely yours.

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

Already using Podman Desktop or the official macOS installer? Run `brew install --ignore-dependencies runpockets/tap/pockets` to reuse your existing Podman.

Bookmark `http://pockets.localhost:18786/` (or use Safari **File → Add to Dock…** / Chrome **Install App**) — from then on your saved URL just works.

### Linux (Desktop, Raspberry Pi, Homelab, Cloud VM / VPS)

```sh
curl -fsSL https://raw.githubusercontent.com/runpockets/pockets/main/packaging/install.sh | sh
pockets
```

Installs Podman (via `apt`, `dnf`, or `pacman` if missing), installs the `pockets` binary, enables `systemd --user` with lingering so Pockets stays running after SSH disconnect (pass `POCKETS_NO_SERVICE=1` to skip), and configures non-root Tailscale Serve access when Tailscale is installed. Containers run rootlessly with zero VM overhead on Linux.

**Installing over SSH on a cloud VM?** Use the exact same command above:
* **With Tailscale (recommended):** Pockets automatically publishes the dashboard privately on your tailnet (`https://<vm>.<tailnet>.ts.net:18786`), prints your one-time sign-in link in SSH, and opens installed Pockets and live previews directly over Tailscale HTTPS. (If you install Tailscale afterward, run `sudo tailscale set --operator=$USER` and `pockets tailscale on`.)
* **Without Tailscale:** Pockets prints the exact `ssh -N -L 18786:127.0.0.1:18786 ...` tunnel command, or pass `--public-url https://pockets.example.com` (or `POCKETS_PUBLIC_URL`) if you put your own reverse proxy in front of `127.0.0.1:18786`.

---

## How It Works

### 1. Create or link a Pocket
Choose **New pocket** in the dashboard (or run `pockets new "My App"` / `pockets link .` from your terminal). Pockets scaffolds a standalone starter with `pocket.yaml`, `Containerfile`, tests, and `AGENTS.md` pointing your coding agent (Claude Code, Cursor, Codex, etc.) at the Pocket runtime contract in `POCKETS.md`. You can also pull ready-made Pockets directly from the Pockets Hub or any public GitHub Pocket repository.

### 2. Develop with Live Preview
Open the Pocket folder in your preferred coding tool and click **Start** in **Workspace preview**.
* Saved edits refresh automatically (CSS swaps without restarting; code edits restart and reload after `/health` passes).
* Preview runs in an isolated container with a disposable preview data volume and explicit per-host HTTPS API approvals (`previewApis` in `pocket.yaml`).
* External edits never overwrite your installed app or its data until you click **Build & start** / **Update**.

### 3. Own it locally (and reach it from your phone)
* Each installed Pocket gets a persistent loopback address (`http://<name>.localhost:18788+`) backed by a dedicated Podman named volume.
* Store plain config and API secrets separately per environment (**Preview** vs **Installed app**) in an owner-only (`0600`) local vault inside Pockets' data directory.
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
pockets add <owner/repo>    # Clone and register a Pocket from GitHub
pockets start <id>          # Start an installed Pocket
pockets stop <id>           # Stop a running Pocket (preserves data)
pockets service install     # Install & enable launchd (macOS) or systemd --user (Linux)
pockets service uninstall   # Stop & remove the background service
pockets tailscale on|off    # Enable/disable private Tailscale HTTPS access for the dashboard
pockets uninstall           # Uninstall Pockets (and optionally its data & Podman)
```

### Uninstall

```sh
pockets uninstall
```

Stops the background service, cleans up any Pockets Tailscale Serve routes, uninstalls Pockets, and prompts before removing Pockets containers/volumes/data or uninstalling Podman (checking first for non-Pockets containers or volumes). Your Pocket source code folders are never touched.

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
