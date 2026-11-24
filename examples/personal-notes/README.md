# `Personal Notes`

A small Pocket implementing one shared plain-text note: a dependency-free Node
HTTP server renders an accessible editor with a single Save button, and the
note persists to `note.txt` in the Pocket storage volume. Everything is
server-rendered HTML with inline CSS — no JavaScript, no external assets, no
accounts, and nothing is sent anywhere else.

## Run in Pockets Desktop

With Podman operational:

1. In Pockets, choose **Add created Pocket** and select this project folder.
2. Choose **Build**. A successful build saves the Pocket automatically, with no
   extra confirmation or Keep step.
3. Choose **Open** to start the app and open its automatically managed localhost
   address. **Stop** preserves its saved note.

Source files, Containerfile, and detailed output are available under optional
**Technical details**.

No `npm start` or port selection is needed for normal use. Pockets copies the
source, not this folder's existing `data/note.txt`: that original note stays
untouched, and the managed app starts with its own separate data volume.
Opening and stopping continue to use that same managed volume.

## Endpoints

- `GET /` — the note editor, rendered with the current note. Read-only: an
  empty note does not create `note.txt`.
- `POST /note` — the only mutation. Saves the submitted form as the note.
- `GET /health` — `200` with `{"status":"ok"}` for readiness checks.

## How saving works

- Saving is explicit: the form posts to `POST /note`, and a successful save
  answers `303` back to `/?saved=1`, which shows a "Note saved." status.
- The note is capped at 64 KiB of UTF-8; larger submissions get `413`. The
  form body itself is capped at 256 KiB.
- `POST /note` requires an `application/x-www-form-urlencoded` body carrying
  the CSRF token rendered into the page (otherwise `403`). The token is random
  per process and rotates on restart, so reload the page after a restart.
- One note is shared by everyone who can load the page: no auth, no sessions,
  and no external APIs. The last successful save wins — each save replaces
  the whole note, with no history and no versioning.

## Storage and runtime

The server stores the note in `DATA_DIR/note.txt`, default `/data` (the
Pocket storage volume; override with `DATA_DIR`), and binds `0.0.0.0:8080` in
the container (override with `HOST`/`PORT`). Writes are atomic (temp file
plus rename) so readers never see a partial note. In the container it runs as
the non-root `node` user; `/data` is prepared in the image so a fresh named
volume is writable.

## Optional local development (no container needed)

```sh
# Bind loopback only when running directly on the host; the server's
# container default stays 0.0.0.0 so the engine can reach it.
HOST=127.0.0.1 DATA_DIR=./data npm start

# Focused tests: ephemeral ports + temp data dirs
npm test
```

`npm test` and the server itself have zero dependencies: no `npm install` is
needed to run either. The `format`/`format:check` scripts are optional and
use Prettier pinned exactly to `3.10.1` as a dev dependency; the repository
root already provides that version, so run `npm run format:check` from the
root instead of installing anything here.

## Optional developer container build and preview

The managed pipeline has built and run an isolated copy of this project in its
opt-in integration test, including note retention across container recreation.
That test cleaned up its own temporary image and data; it did not install this
project or read/change your local development note.

For an explicitly requested standalone developer workflow, review `server.js`
and `Containerfile` first and approve the build. From this folder:

```sh
podman build -t localhost/pockets-personal-notes:0.1.0 .
```

`pocket.yaml` declares `arm64` because the creation host is Apple Silicon; no
other architecture has been tested. Desktop uses its own project-specific
image reference; the standalone tag above is independent of a Desktop build.

For an approved preview, use a unique preview container name and a dedicated
named volume, publish only on loopback with an engine-allocated host port,
and discover the port instead of guessing it:

```sh
podman run -d --name pockets-personal-notes-preview \
  -p 127.0.0.1::8080 \
  -v pockets-personal-notes-preview-data:/data \
  --pull=never --cap-drop=ALL --security-opt=no-new-privileges \
  localhost/pockets-personal-notes:0.1.0

# Do not guess the port: ask the engine which host port it allocated.
podman port pockets-personal-notes-preview 8080
```

Open the reported `http://127.0.0.1:<PORT>` URL, wait for `/health`, save a
note, then stop and remove only this preview container. A new preview on the
same volume shows the note again. Stop only the preview you created here and
leave its volume in place unless deleting it was explicitly approved.

For normal use, prefer the Desktop workflow above rather than these manual
developer commands. Imports are snapshots; editing this source folder does not
change an already approved build. Updates and data migration are separate work.
