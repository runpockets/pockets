# Verification

## Pocket-only monitor revision — November 19, 2026

Removed whole-computer and VM metrics from the UI and native snapshot contract.
The backend no longer samples host counters or runs `podman info`; the sysinfo
dependency was removed. Only the installed-pocket inventory, container states,
and owned running container stats are collected.

Native verification: **38 tests passed, 3 opt-in live tests ignored**; Rust
formatting and native check passed. The explicitly invoked read-only live
monitor test also passed against Podman 6.2.0 with no warnings, returning only
`sampledAtMs`, `pockets`, `totals`, and `warnings`. The empty test store reported
Hello Pocket using 17,977,344 bytes, with the same value in totals. The test
performed no container or VM mutations.

Desktop tests passed (**16/16**), including pocket-only scope assertions and
binary-unit labels. TypeScript, repository formatting, and the macOS debug
application bundle build passed (39.44 MiB). The native-window visibility
manual acceptance limitation from the initial checkpoint still applies.

## Resource Monitor checkpoint — November 19, 2026

The initial checkpoint below included host and guest measurements. The later
pocket-only revision removes both sources and their UI panels; those historical
measurements are not part of the current monitor.

The native read-only live sampler passed against Podman 6.2.0:

```sh
cargo test --manifest-path apps/desktop/src-tauri/Cargo.toml --lib \
  live_resource_snapshot_is_readable -- --ignored --nocapture
```

The test used an empty temporary installation store and therefore reported only
Hello Pocket, even though an unrelated preview and a managed project also
existed in the engine. It returned host CPU/memory, guest CPU/memory, Hello's
running status and raw stats, matching combined totals, and no warnings.
Example readings were 12.23% host CPU, 0.02% guest CPU since boot, and
17,653,760 bytes Hello memory; these are transient readings, not benchmarks.
No container or VM lifecycle operation was performed.

The full native suite passed with **40 tests passed, 3 opt-in live tests
ignored**, and Rust formatting passed. Resource Monitor coverage includes
ownership conflicts, installed-versus-draft selection, unsupported states,
missing stats, partial installation inventories, numeric validation and
overflow-safe totals, guest parsing, and the serialized IPC contract.

Desktop component tests passed: **16 total**, including 12 Resource Monitor
tests for deferred opening, metric scopes, null/error/warning handling,
stale-data recovery, close/reopen, hidden/visible transitions, and
non-overlapping requests (including close/reopen during a pending sample).
Workspace TypeScript and repository Prettier checks passed. The macOS debug
application bundle built successfully at
`apps/desktop/src-tauri/target/debug/bundle/macos/Pockets.app` (39.92 MiB),
including the frontend production build. This does not verify signing,
notarization, or Windows.

The browser component was also opened with mocked IPC and a representative
snapshot to check accessible rendering. This was not a native IPC test.
Screenshot capture was unavailable because the browser tab's desktop window
was not visible. Native-window visibility events and end-to-end rendering
remain manual acceptance items; see [Resource Monitor](resource-monitor.md).

## Environment baseline

Initial inspection on macOS / `arm64`:

- Node `v26.0.0`, npm `11.12.1`.
- Rust and Cargo `1.98.0`.
- Xcode developer directory available.
- Podman not found on PATH or at `/opt/homebrew/bin/podman`, `/usr/local/bin/podman`, `/opt/podman/bin/podman`.

No system packages installed or container machines configured by this project.

## Checks completed during the initial foundation

- `npm test --prefix examples/hello-pocket`: 4 passing tests (health, visit persistence/concurrency, malformed request handling).
- `node --test packages/pocket-spec/schema.test.cjs`: 3 passing image-pattern regression tests. These exercise the schema's image regex, not full JSON Schema validation.
- `npm run typecheck`: passed.
- `npm run build:web --workspace @pockets/desktop`: passed.
- `npm run format:check`: passed.
- Integrated native tests (`cargo test --manifest-path apps/desktop/src-tauri/Cargo.toml --lib`): 11 tests pass (manifest 3, runtime discovery 2, container inspection/arguments 4, HTTP readiness 2). These use fixtures and local test servers, not a real Podman engine.
- Native formatting (`cargo fmt --manifest-path apps/desktop/src-tauri/Cargo.toml --check`): passed.
- `npm run tauri --workspace @pockets/desktop -- build --debug --bundles app`: passed without compiler warnings; produced `apps/desktop/src-tauri/target/debug/bundle/macos/Pockets.app` (35.55 MiB). Opened with macOS LaunchServices and confirmed its bundled executable remained running. The development runner was explicitly stopped afterward; its exit code 130 reflects that intentional interrupt.
- `npm run dev`: compiled and launched the native executable. Browser preview shows its explicit desktop-required notice with no console errors; this is not a native IPC acceptance test.
- `npm audit --json`: five high-severity findings in the Vite development toolchain, all traced to `picomatch` advisory `GHSA-fc5q-39xc-vjpf`. No fixed current-version dependency was available at initial inspection; audit's proposed workaround downgrades Vite by two majors. No automatic downgrade applied.

### Live Podman verification — November 19, 2026

The user installed Podman 6.2.0 at `/opt/podman/bin/podman` and started its rootless ARM64 Linux machine. The first-party image built successfully.

An opt-in native integration test passed against the real engine:

```sh
cargo test --manifest-path apps/desktop/src-tauri/Cargo.toml --lib live_podman -- --ignored --nocapture
```

- First start: healthy at `http://127.0.0.1:33723`, visit count 1.
- Stop: confirmed stopped.
- Start again: different container ID and loopback port (`40795`), visit count 2.
- Final cleanup: stopped; named data volume retained.

This verifies native container creation, VM loopback forwarding, HTTP readiness, stopping, and persistent-volume initialization/data retention across recreation. The test refuses to mutate a running example and is ignored in the normal suite (11 pass, 1 ignored). It does not delete data, images, or the VM.

The desktop button flow and default-browser handoff still need manual acceptance. A release-profile build, signing/notarization, and Windows execution were not tested.

## External-agent creation checkpoint

The creation commands are implemented in `packages/pocket-tools`, the standalone starter is in `templates/basic-pocket`, and portable agent instructions are in `agent/skills/create-pocket/SKILL.md`.

- Tooling: 9 tests pass, covering manifest validation/parser rejection cases and project generation/refusal to overwrite.
- Starter: 2 tests pass, covering health and persistent visit counts.
- Independent generated starter smoke: created in a temporary directory outside the repository; manifest validated and its 2 tests passed without installing dependencies.
- Fresh-agent Local Dice trial: 5 tests pass, covering health, rolls, persistence, request-abort/body-limit handling, and exact write-origin/media-type restrictions.
- Existing Hello Pocket: 4 tests pass; schema image-pattern checks: 3 tests pass.
- Desktop TypeScript check passed; production npm audit found zero vulnerabilities. The previously recorded development-toolchain advisory remains a separate limitation.

### Live Local Dice preview

Image `localhost/pockets-local-dice:0.1.0` built successfully; inspected platform was `linux/arm64`, configured user `node`. Its Containerfile and scripts were reviewed before the explicit build; neither create nor validate performs builds automatically.

An isolated container published only `127.0.0.1:46681` to its internal port 8080. `/health` returned 200. Browser interaction rolled a 4, updated the count to 1, and retained both values on reload. After stopping and removing only that preview container (not its volume), recreation on `127.0.0.1:36529` retained the same last roll and count. No browser console errors were observed.

The preview was stopped after verification. Retained resources:

- Image: `localhost/pockets-local-dice:0.1.0`.
- Stopped container: `pockets-creation-trial-local-dice`.
- Data volume: `pockets-creation-trial-local-dice-data`.
- Preview ownership label: `dev.pockets.preview=local-dice-creation-trial`.

Hello Pocket was not modified, stopped, or restarted. Local Dice was **not installed in Desktop** at that earlier checkpoint; the import path had not been implemented yet. The trial verified an agent-assisted source/build/preview workflow, not built-in AI creation or autonomous safe code generation.

## Initial managed Desktop build/preview checkpoint

Integration gates at that checkpoint (the Preview/Keep UX was subsequently simplified below):

- Native suite: **24 passed, 2 opt-in live tests ignored** in the normal run; warning-free compilation and clean Rust formatting.
- Desktop component tests: **4 passed**, covering native import cancellation, escaped source review, fresh progress on build retry, and Preview → Keep with ID-only commands (including Keep when the runtime is unavailable).
- Tooling: **9 passed**; Hello Pocket: **4 passed**; schema pattern tests: **3 passed**.
- Desktop typecheck, frontend production build, and repository formatting: passed.
- Production npm audit: zero vulnerabilities. The development-toolchain advisory recorded above remains separate.
- macOS debug `.app` build: passed; current bundle is **39.21 MiB**. The dashboard was restarted from this bundle and its process remained running. Hello Pocket continued running unchanged.

The managed Notes native live test was explicitly run:

```sh
cargo test --manifest-path apps/desktop/src-tauri/Cargo.toml --lib \
  managed_live_import_build_start_save_restart_preserves_note -- --ignored --nocapture
```

**Result: 1 passed**, completing in about 3 seconds against Podman 6.2.0:

1. Imported `examples/personal-notes` through the filtered source snapshot service into an isolated temporary store whose path contained spaces.
2. Built through the production managed-build service and verified that the saved record remained a draft with an immutable image ID, not an installed app.
3. Started through the managed lifecycle service, confirmed HTTP readiness and an inspected loopback URL, and saved a unique test note through the real form/CSRF path.
4. Stopped, reopened the store, recreated the container, and verified the note survived in its dedicated volume.
5. Removed only the fixture's ownership-checked container/volume and unique image tag. Follow-up engine queries confirmed no managed-test resources remained.

The test did not read or modify the original local development note, operate on
Hello Pocket, or change the Podman machine. It did not launch a browser or bypass
an approval through a frontend API: its scoped, explicitly invoked test helper
represents an already approved build.

**Manual acceptance still required:** the actual macOS folder picker and build
confirmation dialogs, the complete desktop button-to-default-browser flow, and
reopening the dashboard with a real kept installation. Mocked component tests
and native service tests do not prove those OS interactions. Follow
[`managed-creation.md`](managed-creation.md#acceptance-path) to check them.

## Single-action Build/Open revision

Build is now the sole in-app approval action: no native build confirmation,
mandatory review, Preview gate, or Keep step. A successful build atomically
persists the image ID and Installed phase. Existing built drafts are projected
as installed without rewriting their records or changing identity/data.

- Native suite: **24 passed, 2 opt-in tests ignored** in the normal run.
- Desktop component suite: **4 passed**, including Build without expanding
  Technical details, automatic saving, ID-only Open, and build retry progress.
- Tooling suite: **9 passed**; typecheck and both format checks passed.
- The explicitly invoked managed Notes live test passed again, now asserting
  automatic installation immediately after build and preserved notes across
  container recreation.
- The updated macOS debug bundle built successfully (**39.14 MiB**) and was
  launched. Dashboard restart did not stop the running Hello or managed Notes
  containers; their uptime was unchanged.

The native folder picker remains, but building invokes no message dialog.
Technical details is optional and contains the source inventory, Containerfile,
internal IDs, raw status messages, and detailed output. Manual button-to-browser
acceptance of the revised UI remains separate from the component/service tests.

## Live acceptance procedure

Run this only after deliberately installing Podman and making its engine operational:

1. Confirm `podman info` succeeds.
2. Build the first-party image using the example README.
3. Launch the desktop app.
4. Confirm the runtime reports ready and the example initially reports stopped.
5. Start the Pocket; confirm readiness before Open becomes available.
6. Open the website and note the visit count.
7. Confirm `podman port pockets-hello-pocket` shows only `127.0.0.1` as the host address.
8. Stop; confirm the container is no longer running and Open is unavailable.
9. Start and open again; confirm the visit count survived.
10. Close and reopen the desktop while the container runs; confirm status reconciles with the actual runtime. Stop explicitly afterward.

Also check missing-image and unavailable-engine messages. Do not delete volumes or change unrelated containers as part of acceptance.

Fixture/local-server tests alone cannot demonstrate Podman VM port forwarding. The opt-in live test covers forwarding and native lifecycle helpers; it does not cover Tauri IPC, desktop interaction, or default-browser integration.
