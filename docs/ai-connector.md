# Built-in AI connector (retired)

This page records the former built-in workflow. For current behavior and
recovery of older workspace files, see [project creation and migration](creation.md).

The AI connector adds two distinct mechanisms and they stay distinct:

- **Connector chat**: converses through a configured provider model. Chat alone
  grants no file or command access.
- **Agent tools**: create or modify files only inside the controlled creation
  workspace described below. The tool set is fixed in code. Configuration
  controls only which model/provider is used, never which capabilities exist.

Authentication is API-key based, or a ChatGPT subscription sign-in for
providers of kind `chatgpt` (see below). Pockets runs its own minimal agent
harness; it does not embed or integrate OpenCode.

## Provider configuration

Versioned JSON at `<app_data>/ai/settings.json`:

```json
{
  "version": 1,
  "defaultProviderId": "p0e1c7…",
  "providers": [
    {
      "id": "p0e1c7…",
      "label": "LM Studio",
      "baseUrl": "http://localhost:1234/v1",
      "models": ["tool-capable-model"],
      "defaultModel": "tool-capable-model",
      "apiKeyRef": "ai/p0e1c7…"
    }
  ]
}
```

- The file never contains a plaintext key. `apiKeyRef` is the stable secret
  reference into native credential storage: the macOS Keychain (via the
  `keyring` crate) under service `dev.pockets.desktop.ai`, account = the ref.
  Other platforms use their native credential store.
- `baseUrl` must be a bare `https://` origin, or `http://` only for loopback
  hosts (local models). No embedded credentials, query, or fragment. Chat
  requests target `{baseUrl}/chat/completions`; provider checks use
  `{baseUrl}/models`.
- The API key crosses to native only on save (`ai_save_provider`) and is never
  serialized back to the frontend — the view model exposes `hasApiKey` only.
- A settings UI edits this same file; there is no second source of truth.

## ChatGPT subscription providers

A provider may have `"kind": "chatgpt"` (the field is optional; absence means
`"openaiCompatible"`, so earlier settings files remain valid). These providers
use a ChatGPT Plus/Pro sign-in instead of a base URL and API key. They store
neither: their endpoints are code constants, and authentication is an OAuth
token set kept beside — never inside — the settings file.

```json
{
  "id": "p0e1c7…",
  "kind": "chatgpt",
  "label": "ChatGPT",
  "baseUrl": "",
  "models": ["gpt-6.2-sol"],
  "defaultModel": "gpt-6.2-sol",
  "apiKeyRef": null
}
```

Sign-in is the OpenAI device-code flow (the same unofficial flow the Codex
CLI uses): Pockets requests a one-time code, opens `auth.openai.com/codex/device`
in the default browser, and polls until the user authorizes. The resulting
token set (`accessToken`, `refreshToken`, `expiresAt`, `accountId`, `email`,
`plan`) is stored as one JSON value in native credential storage under
`ai/<provider-id>/oauth` — a sibling of the API-key reference, so the two
never collide and removing a provider removes both. Tokens never enter the
settings file, logs, or the frontend; the view model exposes `signedIn` only.
Tokens are refreshed proactively (under five minutes to expiry) during use,
persisting the rotated refresh token. The `id_token` is decoded for display
and routing metadata (email, plan, workspace account id) but never trusted
as authentication.

Model calls ride the Codex Responses wire:
`POST chatgpt.com/backend-api/codex/responses` with `Authorization: Bearer`,
`ChatGPT-Account-ID`, and a pinned client `version` header (the backend gates
its model catalog on it). The domain messages and tool contract are identical
to the compatible path — only the transport differs, so the agent loop is
unchanged. This is an unofficial backend: it can change without notice, and
the pinned client version is a maintenance point. It is used here
deliberately, for a local personal tool, with the account owner's consent.

## Model connection

OpenAI-compatible `POST {baseUrl}/chat/completions` with `stream: true`. The
request carries only `model`, `messages`, `tools` (omitted when empty), and
`stream`. Responses are parsed as SSE: `delta.content` streams as text,
`delta.tool_calls` fragments accumulate by `index` into complete tool calls,
`data: [DONE]` ends the turn. Non-2xx responses surface status plus a bounded
snippet. No-HTTPS-to-cloud, no-redirect, bounded connect/idle/total timeouts,
and cancellation between streamed chunks. No specific provider is promised;
the OpenAI-compatible shape (including tool calling) is the only compatibility
implied.

## Creation workspace and tools

Each generation gets a private native workspace under
`<app_data>/ai/sessions/<session-id>/workspace`, mode 0700, seeded from the
bundled basic-pocket starter (template files embedded at compile time). All
tool paths are workspace-relative and resolved with symlink/escape rejection;
file-count and size caps apply. A restricted working directory is not a
sandbox, so project checks run separately:

| Tool                 | Effect                                                                                                                                                                                                |
| -------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `list_files`         | Paths + sizes inside the workspace                                                                                                                                                                    |
| `read_file`          | UTF-8 file contents (bounded)                                                                                                                                                                         |
| `write_file`         | Create/replace one bounded file                                                                                                                                                                       |
| `edit_file`          | Exact single-match replacement                                                                                                                                                                        |
| `validate_pocket`    | Native `pocket.yaml` parse + value-constraint validation                                                                                                                                              |
| `run_project_checks` | The project's tests inside a `podman run --rm --network=none` container on the pinned official Node image, workspace bind-mounted, engine socket and host paths otherwise absent, bounded time/output |

No user-installable skills or plugins, no arbitrary MCP tools, no unrestricted
host shell or filesystem access. Built-in creation instructions and the
starter template are part of Pockets itself.

## Agent loop

One generation at a time (busy is an immediate error, mirroring builds).
Bounded model turns and tool calls. The system prompt embeds the Pocket
runtime contract (single container, `/health`, persistent `DATA_DIR` storage,
no external services, dependency-free Node baseline) and the Simplicity
Principle. The loop appends native tool results and ends when the model
finishes without tool calls. Cancellation is cooperative between chunks.

On completion Pockets natively re-validates `pocket.yaml`; a valid workspace
is imported through the existing snapshot importer as an ordinary Draft
project and announced to the panel, which hands the finished session to the
live preview and the created-Pocket list — promotion stays one explicit
action: the list panel's **Build**, which reports _Installing_, _Installed_,
or a failure with the full log exactly like a manual build. Nothing installs
on its own; completion is a review moment, not an approval ([live
preview](live-preview.md)). The model holds no authority over installed-app
lifecycle or deletion. An invalid final workspace yields a failed session with
the validation reason; nothing is imported and nothing is built. The external
agent path (M2) remains available.

## Command surface (camelCase JSON)

- `ai_get_settings() -> Settings { version, defaultProviderId|null, providers[] }`
- `ai_save_provider(provider, apiKey?) -> Settings` — `apiKey: null` keeps the
  stored key, `""` deletes it, a value stores it in credential storage.
- `ai_delete_provider(id) -> Settings`
- `ai_set_default_provider(id) -> Settings`
- `ai_test_provider(id) -> TestResult { ok, detail }` — GET `{baseUrl}/models`,
  or the Codex catalog for ChatGPT providers (which also proves the sign-in
  works).
- `ai_chatgpt_start_login(providerId) -> { verificationUrl, userCode }` —
  begins the device-code sign-in and opens the verification page; only valid
  for a `chatgpt` provider.
- `ai_chatgpt_poll_login(providerId) -> { status, email?, plan? }` — polled;
  `pending | signedIn | expired | failed | none`. Terminal states are sticky
  until a new start or sign-out.
- `ai_chatgpt_sign_out(providerId) -> bool` — deletes the stored token set
  and any pending login.
- `ai_start_generation(prompt, providerId?) -> Session { id, … }`
- `ai_generation_status(id, cursor) -> GenerationStatus { state, events[since cursor], nextCursor, error?, projectId? }` — polled.
- `ai_cancel_generation(id) -> bool`

`GenerationEvent` kinds: `text { text }`, `tool { name, status: started |
finished, ok? }`, `note { text }`. Session states: `running | completed |
failed | cancelled`.

## Known limits

- The model can read/write anything inside the current workspace, including
  files it did not author; the workspace is disposable and isolated from
  installed apps and user data.
- `run_project_checks` containers keep private container networking disabled
  (`--network=none`), but the Podman engine itself may be remote; the Node
  image pull is a network operation like Pocket builds.
- Model turns are billed by the provider; a stuck loop is bounded by the turn
  cap and cancellation.
- Keychain access from unsigned dev builds may prompt for permission on macOS.
- The transcript lives in memory: after a restart, a session interrupted
  mid-run reads back as failed with an explanation and its events are not
  recoverable. The durable record keeps the outcome. A workspace left by an
  interrupted session is disposable, not resumable.
- ChatGPT providers ride an unofficial backend: OpenAI can change or close
  it without notice, and automation of a subscription sits in a ToS grey
  zone — accepted here for a local personal tool with the account owner's
  consent. A stale pinned `version` header makes every model appear
  unsupported; updating the constant is the fix. Sign-in requires "device
  code authentication" to be enabled in the account's ChatGPT Codex security
  settings. A failed sign-in reports `failed` without the upstream reason
  (the wire response carries nothing safe to surface).

## Acceptance (first slice)

1. Add a provider (local OpenAI-compatible endpoint or cloud key), see it in
   settings with `Key stored`, and pass `Check`.
2. Describe a small app; watch streamed text and tool activity; Stop works.
3. A completed session shows its Pocket as a draft in the created-Pocket list
   and points the preview panel at the finished session; no build runs without
   a click.
4. Build installs it; Open starts it — the app's data survives Stop/Open.
5. Failed or cancelled sessions leave no imported project.

Automated coverage: settings/secret-ref validation and base-URL policy,
mock-SSE streaming with tool-call accumulation and cancellation, workspace
path scoping and caps, tool-dispatch behavior including fake-Podman
argument-surface assertions, and agent slot/record/transcript bounds. The
real-model run inside Desktop and the browser steps remain manual acceptance.
