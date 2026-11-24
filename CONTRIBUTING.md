# Contributing to Pockets

Thanks for helping build Pockets! Whether you want to share a community Pocket, improve the cross-platform daemon/UI, or help shape the upcoming HACS-style marketplace, contributions are very welcome.

## Ways to Contribute

### 1. Build & Share a Pocket
We're looking for practical, clean, local-first Pockets to showcase and seed the upcoming v0.2 community catalog:
- Personal dashboards, trackers, notes, bookmarks, finance/crypto watchlists, weather, or developer utilities.
- Follow the Simplicity Principle: clean, maintainable single-container HTTP web apps with a `/health` endpoint and persistent storage under `/data` (see [templates/basic-pocket](templates/basic-pocket) and [docs/pocket-spec.md](docs/pocket-spec.md)).
- Validate your manifest locally:
  ```sh
  npm run pocket -- validate ./my-pocket/pocket.yaml
  ```
- Open an issue or discussion linking your public Pocket GitHub repository so we can feature it and include it in the upcoming catalog!

### 2. Core Daemon, CLI & Web UI Pull Requests
1. Fork the repo and create a branch from `main`.
2. Make focused changes with tests covering new behavior or bug fixes.
3. Run the verification suite before opening a PR:
   ```sh
   npm run typecheck
   npm run test:desktop
   npm run test:gateway
   npm run test:schema
   npm test --prefix packages/pocket-tools
   cargo test --workspace
   ```
4. Preserve Pockets' core security invariants (see [SECURITY.md](SECURITY.md) and [docs/architecture.md](docs/architecture.md)):
   - Never expose the Podman socket or arbitrary host paths to Pocket containers.
   - Never interpolate shell strings for Podman, Git, or Tailscale commands (fixed argument arrays only).
   - Keep the daemon loopback-only with strict `Host`, `Origin`, and session-token validation.

## License

By contributing to Pockets, you agree that your contributions will be licensed under the [Apache License, Version 2.0](LICENSE).
