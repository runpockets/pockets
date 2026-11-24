# Prototype-first workflow

## Product decisions

Pockets starts with a high-fidelity prototype: a designed interface with
representative sample data and local UI interactions, not a connected production
backend. The prototype evolves into the app; it is not a disposable screenshot
or a second implementation of the UI.

The progression is:

**Describe → prototype → iterate → add real functionality → build/install**

- Use real HTML, CSS, and minimal JavaScript. Forms, navigation, toggles, and other
  UI interactions can work against temporary sample state.
- Clearly identify generated prototypes as **prototype · sample data**.
- Do not imply that authentication, payments, remote integrations, or durable
  application data are implemented when they are only demonstrated.
- Keep sample state separate from the UI so real data can be connected later.
- Do not introduce a backend, dependencies, or outbound calls just to demonstrate
  a layout. The preview's HTTP server is serving infrastructure, not evidence of a
  functional application backend.

## Workspace and management

The workspace consists of three coordinated regions:

1. A compact pocket selector, with filtering and an **all pockets →** action.
2. The selected pocket's saved conversation and prototype-edit composer.
3. The selected draft's isolated native preview.

Selecting a pocket changes the conversation and preview together. Selection is
encoded in hash routes so it works with the desktop app's custom protocol and
survives reloads:

- `#/`: a new-pocket conversation.
- `#/p/:id`: a selected pocket's workspace.
- `#/pockets`: the separate management page.

The All pockets page owns the detailed cards, source metadata, import, explicit
Build and start, installed-app controls, and delete confirmation (typed name for
installed pockets, plain confirm for drafts). Provider settings,
runtime diagnostics, the bundled example, and resource monitoring are secondary
management controls, not the contents of the workspace sidebar.

## One pocket, working and installed revisions

A pocket has one native identity, conversation, and editable working source.
Successive chat messages update that same pocket, whether or not it has already
been installed. Editing an installed pocket does not create another sidebar
entry.

The installed image and its manifest are pinned separately from working source.
Chat edits and preview refreshes do not change the running installed version.
Lifecycle actions must use the installed manifest, never a pending working
manifest that might describe a different port or healthcheck.

**Build** performs the first installation. **Update** builds the pending working
revision and replaces the installed version of that same pocket. A running app
stays on its old image while the candidate builds; deployment preserves its
native identity, data volume, and running host port. A stopped app stays stopped.
Installed metadata is committed only after a running replacement is healthy. A
failed deployment or commit attempts to restore the previous container rather
than claim the update succeeded.

Validated source snapshots are retained in project-local history. Restoring one
changes the working source and preview; it does **not** deploy the old code
immediately. Update is still the explicit action to apply it. Model context is
reset after a source restore, but the public conversation history is retained.
Restore must refuse while a generation, paused conversation, or build could race
the file replacement.

**Reverting code does not revert saved data.** Updates and restores never remove
or replace the installed data volume. Application-level schema compatibility
still matters: restoring an old image cannot undo data migrations performed by
newer app code. Source replacement and installed-image commit must be serialized
against build and delete operations.

Earlier releases created separate draft copies when editing installed pockets.
Those existing records are not automatically merged or deleted.

## Preview boundaries

Keep the native child-webview and loopback-proxy trust boundary documented in
[live-preview.md](live-preview.md). Do not replace it with an unrestricted iframe
or run arbitrary generated commands directly on the host.

The draft preview serves the editable workspace. File changes use the existing
proxy's reload mechanism rather than rebuilding an installable app image for
every edit. Initial preparation of an isolated preview container is distinct from
the explicit full Build operation.

Stylesheet-only edits publish live style updates after the changed files become
visible inside the container; they do not restart its process. Server-code edits
(including HTML or CSS embedded in a server file) may require an automatic
process restart, but keep the same preview proxy origin.

Normal edits keep the same working workspace and preview session, including
edits to already-installed pockets. An explicit Stop remains stopped; selecting
a pocket or restoring source does not silently start a stopped preview.

An imported project's source may already contain real backend behavior. Merely
opening it in an isolated preview does not convert it into a sample-data
prototype; labels must distinguish the prototype workflow from claims about
unmodified imported code.

## Conversation boundaries

Conversation history belongs in native, project-keyed storage, not only in React
state. Switching pages must not cancel a running native generation or mix
responses between projects.

The frontend receives only bounded, display-safe turns and tool activity. Private
model context, provider credentials, raw tool results, and host filesystem paths
must not be exposed by a history endpoint. Existing paused-run Continue, Stop,
turn budgets, and the single active-generation slot remain meaningful.

Past releases did not persist completed transcripts. Missing old history must be
shown honestly; it cannot be reconstructed from generated source files.
