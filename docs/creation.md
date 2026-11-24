# Create a Pocket with an external agent

Start a project in Pockets or link source created in your preferred coding
tool. Development happens in that tool; Pockets manages live preview,
installation and updates without needing a model connection for this workflow.

## Normal user workflow

1. Choose **New pocket**, enter a name and optional project brief, then choose
   **Choose location & create**. The native picker selects a parent directory;
   Pockets creates a new subfolder and registers a linked draft. It never
   overwrites existing folders or executes code during creation.
2. For existing source, choose **Link existing project** or **Link project**.
   Linking the same folder returns its existing Pocket identity.
3. Use **Open folder** or **Copy folder path** beside the linked path. Open the
   folder as your coding tool’s workspace. The starter’s `AGENTS.md` points to
   the runtime requirements in `POCKETS.md` and the app brief in `POCKETS_BRIEF.md`.
4. Choose **Start** in the preview pane. Saved source edits refresh automatically
   while you iterate in your coding tool. Preview data is separate and
   disposable; preview Stop discards it and keeps the preview stopped.
5. Choose **Sync source** to capture the version you want to install, then
   **Build and start** for the first installation or **Update** for an installed
   app. These are explicit deployment actions; editing source never deploys.
   The first install opens the Pocket's named `.localhost` site on its assigned port.
   Installed-app **Open** starts the app and opens the same address. **Stop**
   retains data.

The current preview profile supports a dependency-free root `server.js` with
the Node image already cached and Podman operational. Use **Preview API access**
for approved, read-only HTTPS requests. Use clearly labeled sample data for
unsupported connections. See [live preview](live-preview.md) for the runtime contract.

Source capture excludes local data, secrets and caches. It never copies local
development `data/` into installed volumes. Source history remains in Pockets;
while linked, restore in your coding tool and Sync rather than overwriting the
external folder from a managed revision. If creation fails after writing files,
the error gives their location so you can review and Link project to recover.

## Runtime variables

Open **Runtime variables** for a saved Pocket and choose **Preview** or
**Installed app**. Add plain configuration such as `CURRENCY` or a secret such
as `PRICING_API_KEY`. A saved secret shows its name and can be replaced or
removed. Its value stays in macOS Keychain after saving.

Start Preview to let the preview code read its values as environment variables.
Opening a stopped installed app applies its Installed app values. Restart the
relevant environment to apply a change or removal. Updates to a running app and
rollbacks keep its active values. Use development credentials for Preview,
since linked code runs again on live reload.

Runtime settings belong to the native Pocket ID. Relinking source, restoring a
revision, and updating an install preserve the saved settings. Pockets sends
values to [Podman secrets](https://docs.podman.io/en/latest/markdown/podman-secret-create.1.html)
through standard input. The engine retains the secrets while the container
definition needs them. Values stay out of source, build contexts, revision
history, and engine command arguments. Deleting the Pocket clears its settings.
App code can read its assigned values and should keep them out of its own logs
and HTTP responses.

## Source creation tools (agents and developers)

From the Pockets repository after `npm ci`:

```sh
npm run pocket -- create ./my-pocket
npm run pocket -- validate ./my-pocket/pocket.yaml
```

Pockets bundles the same `templates/basic-pocket` text starter used by this
CLI. It includes a manifest, container definition, health endpoint, persistent
visit-count demonstration, tests, formatter scripts, coding-tool instructions
and a brief. The CLI destination must be new; its final directory name becomes
the Pocket manifest ID and its title-cased display name. The web UI permits a
separate display name. Neither path installs dependencies or executes code.

Review the generated code, then run its tests:

```sh
cd my-pocket
npm test
```

Only the formatter needs a development dependency installation; the starter server and tests use Node's standard library. The generated project's README contains local execution and explicit container build instructions.

The validator uses the canonical JSON Schema with a single plain YAML document, no anchors/aliases, and a 128 KiB size limit. Errors identify invalid fields. It never pulls images, executes code, or fetches schemas from the network. A valid manifest is not a security endorsement.

## Agent workflow

Ask a fresh coding-agent session:

> Read `agent/skills/create-pocket/SKILL.md` in this repository. Create a Pocket that [describe one small application]. Work in a new directory, keep persistent data under `/data`, and do not build or run containers without approval.

For a project already created in Pockets, open its source folder in the coding
tool and ask it to work from `AGENTS.md` and the project brief. The repository
skill is also available as a portable instruction file.

An execution approval should identify the activity and scope. For example, approve running the generated project's focused tests without also approving a container build or system installation.

## Current boundary

Use the linked workflow to keep one project identity through repeated edits
and updates. The older **Import pocket** action captures a separate snapshot;
re-importing is not an update or data migration. Native installation IDs keep
duplicate manifest names separate. Never alter the bundled Hello manifest to
impersonate an installation.

Every saved Pocket opens in the development workspace. Choose **Export saved
source…** and pick a parent directory to link a new copy of its verified
working snapshot. The new subfolder is named after the Pocket’s manifest ID.
Choose **Link development folder** (or run `pockets link .`) if you already have the source. Both paths
keep the Pocket’s identity, revision history, installed image and data volume.
