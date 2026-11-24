# `__POCKET_NAME__`

A starter Pocket built from the basic template: a dependency-free Node HTTP
server with an HTTP health check and a visit count persisted to `/data`, the
Pocket storage volume. It shows the contract Pockets expects — replace
`server.js` with your own app and keep the lifecycle bits.

## Run in Pockets

If this project was created in Pockets it is already registered. Otherwise,
choose **Link project** (or run `pockets link .` in this folder). Choose **Develop**, then
**Start** in the preview pane with Podman operational and the Node preview
image cached. Saved edits in your coding tool refresh automatically.

Open this folder as your coding tool’s workspace. `AGENTS.md` points to the
runtime requirements in `POCKETS.md` and the app brief in `POCKETS_BRIEF.md`.
Edit the brief to describe the app you want, then ask your coding tool to build it.

Preview runs dependency-free Node code. Declare HTTPS services in `previewApis`
in `pocket.yaml` so Pockets can request approval while Preview runs. Configure
variables in **Runtime variables**. Stop discards preview data. After reviewing
the result, choose **Build & start** / **Update** to capture and install that version,
then **Open** for the installed app. Installed-app Stop preserves its data.

Pockets manages images, localhost ports and readiness. Source and build output
are available under **Technical details**. Local development `data/` is not
copied or changed, and external source edits never deploy an update by themselves.

## Endpoints

- `GET /` — hello page with a persistent visit count.
- `GET /health` — `200` with `{"status":"ok"}` for readiness checks.

The server binds `0.0.0.0:8080` (override with `HOST`/`PORT`) and stores
`visits.json` in `/data` (override with `DATA_DIR`). In the container it runs
as the non-root `node` user; `/data` is prepared in the image so a fresh named
volume is writable. For API-backed apps, `lib/cache.js` provides an optional
dependency-free memory + disk JSON cache under `DATA_DIR/cache` (see
`POCKETS.md`).

## Optional local development (no container needed)

```sh
# Bind loopback only when running directly on the host; the server's
# container default stays 0.0.0.0 so the engine can reach it.
HOST=127.0.0.1 DATA_DIR=./data npm start

# Tests: ephemeral ports + temp data dirs
npm test
```

`npm test` and the server itself have zero dependencies. `npm install` is only
needed for the `format`/`format:check` scripts (Prettier dev dependency).

## Optional developer container build (from this folder)

```sh
podman build -t localhost/pockets-__POCKET_ID__:0.1.0 .
podman run -d --name pockets-__POCKET_ID__ \
  -p 127.0.0.1:8080:8080 \
  -v pockets-__POCKET_ID__-data:/data \
  --pull=never --cap-drop=ALL --security-opt=no-new-privileges \
  localhost/pockets-__POCKET_ID__:0.1.0
```

Open <http://127.0.0.1:8080>, stop the container, start it again with the same
volume, and the visit count persists. `pocket.yaml` describes this Pocket for
the Pockets runtime; `runtime.architectures` lists the architecture this image
was built for.
