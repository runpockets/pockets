# Managed Build and Open

New development uses **New pocket** or **Link project**, then **Develop → Start**
for live preview. Use **Sync source** before Build/Update to choose the install
snapshot. See [creation](creation.md) for the current flow. The snapshot-only
Import workflow remains available for older projects.

## User flow

Add source using the native picker, or create a linked project. Pockets keeps
an isolated working snapshot in its application-data directory. Choose **Build
and start** to package a draft, save it as installed, start it and open its
website. If startup/open fails it is still installed and can be opened from
Live pockets. **Update** replaces the same Pocket's installed version and keeps
its data; a running app is replaced and checked for readiness, while a stopped
app stays stopped. Failed deployments attempt to restore the previous version.
Failed builds remain available to retry. There is no second confirmation or Keep step.

Deleting is the one confirmed action in this panel: Delete opens a
confirmation whose strength follows the record's phase. A draft confirms
with the Delete Pocket button alone; an installed Pocket unlocks only when
its exact name is typed. An unbuilt draft's dialog names only what it can
actually own — the record and its source snapshot; no container or data
volume is claimed for a draft that never ran.
Confirming permanently removes the Pocket from Pockets — its record, build
snapshot, container, and data volume with everything the Pocket stored while
running (drafts included). Source folders imported from elsewhere on the
computer are never touched; only the snapshot inside Pockets dies. Saved drafts use the same Build and start action.

The main build explanation is short and calm. Containerfile, source inventory,
IDs, and detailed output live under optional Technical details, not a mandatory
review gate.

The external coding agent still writes the application source. This milestone brings the generated application's build, preview, and saved installation into Desktop; it does not add an embedded coding agent or model connection.

## Native boundaries

- The frontend supplies project IDs, not filesystem paths, image names, build flags, shell commands, or browser URLs.
- A native folder picker selects the source. Source import executes nothing.
- Each project gets a random native installation ID, independent of its manifest ID. Duplicate app names cannot collide with Hello Pocket or each other's containers/data.
- Records and source snapshots live under the native application-data directory. Records use atomic writes and survive Desktop restarts.
- Snapshot import excludes development data, dependency caches, version-control files, hidden files except known build-ignore files, and common credential file types. It rejects symlinks and special files and enforces file/count/size limits.
- Choosing Build and start (or Update) is the explicit approval. The trusted application UI no longer asks for a second native confirmation. Build instructions still execute and can access the network; source details remain available for inspection. Snapshot filtering is not secret detection or a guarantee of safe code.
- Builds use a fixed native Podman argument surface, a private staged context, bounded time and log retention, and no host mounts, engine socket, forwarded secrets, SSH agents, or host networking.
- Runtime launch uses the successfully built immutable image ID, never a mutable manifest image tag. It retains loopback-only port allocation, ownership checks, dropped capabilities, and a dedicated named data volume.
- Build progress reads must remain responsive while a build is running. Lifecycle operations remain native and serialized.

Private build networking is not a firewall: build instructions can contact
services reachable from the configured Podman engine, as well as the internet.
Only reviewed, trusted source should be built in this alpha. A build-client
timeout stops the client but cannot guarantee cleanup of all remote engine-side
intermediate build resources.

## Data and lifecycle

Import copies source, not a local development `data/` directory. Existing notes in the source project are left untouched; the installed app starts with its own data volume. Data migration must become a separate, explicit action rather than baking personal data into an image.

Native build success persists the immutable image ID and installed state;
Desktop's Build and start action then starts/opens it. Open also starts it on
demand. Previously built drafts are
recognized as installed without changing their identity, image, source, or data.
Closing the window still does not stop containers. Use Stop explicitly.

Snapshot-only imports remain separate from linked source. Linked projects
refresh preview automatically, while only Sync changes the installation
snapshot and an explicit Update applies it. Pockets does not edit external
source. Background services and data migrations remain future work; uninstall
uses Delete. Deletion leaves the built image because images are content-addressed
and may back another Pocket built from identical source.

## Acceptance path

1. Add `examples/personal-notes` using the native folder picker.
2. Confirm Technical details is optional and its source inventory excludes `data/`, `.env`, and dependency caches.
3. Choose Build and start without expanding details. No second confirmation should appear.
4. Confirm real progress and automatic saving on success; failed builds should offer retry.
5. Choose Open: Pockets starts the app, waits for readiness, and opens the current inspected localhost address without asking for a port.
6. Save a test note, restart the dashboard, find the saved Pocket, stop it, and open it again. The saved test note must survive.
7. Confirm there is no Preview/Keep gate, and the original project's local development note was neither copied into the image nor changed.
8. Delete a Pocket: a wrong or partial name keeps the confirm button disabled; the exact name unlocks it. After confirming, the card is gone, `podman ps -a` shows no container for it, and `podman volume ls` shows no `pockets-project-<id>-data` volume. An imported source folder on disk is unchanged.

Automated native service tests cover the underlying source/store/build/lifecycle operations. Native folder selection and the complete button-to-browser flow require separate UI acceptance; do not infer them solely from service tests.
