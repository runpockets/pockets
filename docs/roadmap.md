# Implementation roadmap

## M1 — One working Pocket

1. Runnable Tauri/React desktop shell and reproducible local tooling.
2. Minimal versioned manifest, schema, and first-party example server with persistent data and `/health`.
3. Podman detection and useful missing/unavailable-engine states.
4. Native start/status/stop with owned-container checks and localhost-only port allocation.
5. HTTP readiness and guarded default-browser opening.
6. Connected desktop controls, focused automated tests, and documented development instructions.

Acceptance: launch Pockets Desktop on a Mac with operational Podman, start the bundled example, wait for readiness, open it in a browser, stop it, and retain its data across a restart.

If Podman is missing, verify compilation and automated tests, record the blocked live acceptance test, and do not claim container execution was tested.

## M2a — External-agent creation

Prioritize proving creation before building a general application manager:

1. A local manifest validator backed by the canonical JSON Schema.
2. A standalone, single-container starter with working health checks and persistent data.
3. A local create command that writes the starter into a new directory without executing it.
4. A portable agent skill documenting the development, approval, testing, and packaging workflow.
5. A fresh-agent trial producing a distinct working application from these instructions.

Acceptance for this checkpoint: create a project, validate its manifest, test it, explicitly build its image, and demonstrate localhost execution. Creation must not require model credentials in Pockets itself.

The desktop still supports only its bundled example. Do not claim generated projects can be imported until the next slice is implemented. CLI validation checks manifest structure; it does not establish image safety, provenance, or correctness.

## M2b — Local installation handoff

Support an explicit, approved local-project import, persistent installed-app records, and lifecycle operations keyed by installation ID. Keep Hello Pocket's legacy controls initially, and support installing the agent-created application through the new path. Never accept arbitrary runtime commands from the frontend.

Acceptance: describe an application to an external agent, approve its build and installation, and run it through Desktop with data surviving a restart.

Implemented: native source selection, filtered snapshot import, durable drafts,
single-action Build, progress output, automatic saving, and managed Open/Stop.
Hello Pocket's legacy controls remain available without migrating or
changing its existing data. Native managed Notes save/recreation passed a live
test; the native dialog/button path still needs manual acceptance.

## M3 — Configure and mature lifecycle

Introduce native secret storage, application configuration, and deliberate image/version/update policies. Extend lifecycle regression coverage and define trust policy before opening installation to arbitrary third-party images.

## M4 — Built-in AI connector (planned)

Planned only; no implementation now. This milestone adds two distinct mechanisms and they must stay distinct: a connector chat that converses through configured provider models, and agent tools that create or modify files only inside the controlled harness below. Connector chat alone grants no file or command access. Authentication is API-key based plus an implemented ChatGPT subscription sign-in (device-code OAuth; see [AI connector](ai-connector.md)); subscription OAuth is unofficial and accepted as such for a local personal tool.

1. Versioned, Pockets-specific JSON configuration for providers, models, and defaults, covering cloud and local OpenAI-compatible endpoints. The OpenAI-compatible shape is the only compatibility implied; no specific provider is promised. Configuration controls only the model/provider in use, never agent capabilities, which remain fixed in code. It stores secret references into native credential storage (the M3 mechanism), never plaintext keys. A settings UI, added later, edits this same configuration rather than a second source of truth.
2. The engine decision is settled: Pockets runs a minimal, Pockets-owned agent harness and does not embed or integrate OpenCode. The first slice is a proof with one provider, a dedicated workspace, and a fixed, small, Pockets-owned tool set — list, read, write, and edit assigned project files, validate the Pocket, and run isolated project checks and read their results — plus streaming, cancellation, error handling, and a bounded agent loop. Model-communication SDKs and adapters may be reused where useful without selecting a library here. Initial scope has no user-installable skills or plugins, no arbitrary MCP tools, and no unrestricted host shell or filesystem access; built-in creation instructions and templates are allowed.
3. A controlled web-app creation harness with a dedicated workspace, using file scoping plus separate execution isolation — a restricted working directory alone is not a sandbox. Generated apps follow the single-container Pocket contract, including health checks and persistent storage. Handoff connects to the existing explicit Build → automatic save → Open flow with no extra confirmation step. The model has no authority over installed-app lifecycle or deletion initially. The external-agent creation and import path from M2b remains available. Creation follows the brief's [Simplicity Principle](../PROJECT_BRIEF.md#simplicity-principle): the simplest maintainable stack that meets the request, with a richer frontend framework only where warranted, and the same runtime contract regardless of frontend. The current plain Node.js starter remains the only starter; no additional starters are promised here.
4. Planned conversational live-preview slice, with the user in the loop as the promotion gate: chat and an embedded preview side pane, matching how the category works — the preview is the product surface, not a browser hand-off. The preview comes up during generation as soon as the workspace is runnable — not only after completion — and stays live while the user iterates in chat. Each coherent edit hot-reloads the pane with visible preparing, updating, ready, and error status. Hot reload is tiered: a static-asset-only edit swaps live with no reload; a markup edit DOM-morphs only the changed elements, preserving focus, form input, and scroll; a server-code edit restarts the in-container process and then morphs. Stateful module-level HMR (client-script memory, module graphs, Vite-style Fast Refresh) remains out of scope and would force a frontend framework onto the dependency-free starter. The user then chooses: keep iterating, or promote with the existing explicit Build — the one approval action, still no Keep step or second confirmation. On this path, generation completion no longer auto-installs: preview review replaces the current completion path's automatic install as the decision moment. The pane is a dedicated child webview (Tauri multiwebview behind the `unstable` feature) that loads only the Pockets-owned loopback preview-proxy origin: it is granted no capability, navigation is confined to the preview origin, new windows, downloads, and permissions are denied, and its data store is ephemeral; the system browser stays an escape hatch on the same proxy, and the pane gets no Desktop native APIs. Mechanism: the proxy injects a small live-reload and DOM-morph client into HTML responses and carries the status channel; the preview container runs the workspace on the pinned Node image using the same bind mount the isolated checks already use, with harness edit completions as the change signal rather than host filesystem watching. The preview runs in an isolated, disposable, localhost-only container with preview-dedicated runtime and a preview-dedicated data volume, separate from the installed immutable build and persistent app data: edits never silently affect installed apps or their data, and any update or migration policy for promoting preview state remains future work. The preview container receives no host files beyond the scoped workspace mount, no host credentials, no other Pocket data, and no engine socket; generated code remains untrusted. The network and dependency policy is now decided rather than assumed: the preview attaches to a dedicated internal Podman network — published loopback ports still work, egress does not — and executes nothing that installs dependencies; a project declaring runtime dependencies fails preview with a clear status while Build remains available. Preview control stays a narrow set of Pockets-owned operations, not an unrestricted model shell. The preview executes generated code before the final Build — a distinct trust and execution boundary from Build — and still ends in the explicit Build → automatic save → Open flow. See [live preview](live-preview.md) for the flow, contracts, and resolved spike evidence.

Acceptance: a small end-to-end run inside Desktop — describe an application, watch the preview pane come up during generation, request at least one markup edit in chat and see it morph in the pane without a manual refresh or full-page flash, confirm the pane's JavaScript cannot reach Pockets IPC, use the Open-in-browser escape hatch and see it still hot-reload, choose Build explicitly, and Open it — with the app's data persisting across a restart and no automatic install happening before the user chooses Build.

Implemented (first slice): versioned provider configuration with native-keychain secret references and strict base-URL validation; a bounded, cancellable OpenAI-compatible streaming client; the fixed six-tool set over a template-seeded, path-scoped creation workspace; container-isolated project checks (`podman run --rm --network=none` on the pinned official Node image); a turn- and call-capped agent loop with live transcript polling; and the one-way handoff into the existing explicit Build → automatic save → Open flow. The external-agent path above remains available and unchanged. See [AI connector](ai-connector.md) for mechanisms, boundaries, and acceptance steps. The real-model run inside Desktop (local OpenAI-compatible endpoint) and the browser steps are still pending manual acceptance; transcript resumption, richer settings ergonomics, and connector chat beyond creation remain follow-on work under this milestone, with the live-preview slice above as the agreed next step. Also implemented since: ChatGPT subscription sign-in — a provider kind authenticating through the device-code OAuth flow with tokens in native credential storage and a Codex Responses transport, so everything downstream of connection resolution is unchanged. Implemented since: completion installs on its own — a completed session triggers the same Build automatically, reporting installing, installed, or a failure that points at the list panel’s manual retry, leaving Open as the user’s only action — and created Pockets delete behind a typed-name confirmation that removes the owned container and data volume, then the record directory; images are content-addressed and deliberately kept, and drafts stay deletable even without Podman installed. See [managed creation](managed-creation.md) for the delete flow.

## First-party Resource Monitor

Implemented as an included, on-demand trusted Desktop view:

- Combined and per-pocket CPU/memory with running/stopped status.
- Pocket-only consumption: no whole-computer or VM usage sampling. Totals
  exclude the Desktop process and runtime overhead.
- Native read-only sampling of ownership-checked containers; ordinary pockets
  never receive engine socket or other-pocket access.
- Lightweight refresh only while open and visible.

See [metric definitions and acceptance](resource-monitor.md). Native-window
visibility and rendering still require manual acceptance.

## Later

- Runtime onboarding and better VM lifecycle UX; Windows verification.
- Additional developer templates and external-agent tools, based on evidence from the creation trial.
- Catalog browsing and publishing with trust/provenance.
- Explicitly authorized network/background modes. Built-in AI's first slice is implemented in M4 above; further connector work continues there.

Do not scaffold these future components before they have a working consumer.
