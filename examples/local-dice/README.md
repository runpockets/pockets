# Local Dice

A small Pocket created from the basic template: a dependency-free Node HTTP
server with one Roll button. Each roll produces an integer from 1 to 6, and the
last roll plus the total roll count persist in the Pocket storage volume, so
restarting the app remembers them.

## Endpoints

- `GET /` — plain page with the current state and a Roll button (read-only).
- `POST /roll` — rolls the die and returns `{"lastRoll": 1-6, "rollCount": n}`
  as JSON. This is the only mutation; `GET /roll` returns 405.
- `GET /health` — `200` with `{"status":"ok"}` for readiness checks.

`POST /roll` is protected against cross-origin writes: a browser `Origin`
header that does not match the request host gets 403, and the request must use
an `application/json` body (which forces a CORS preflight this server never
grants). Bodies are capped at 1 KiB and ignored — rolling takes no input.

The server binds `0.0.0.0:8080` in the container (override with `HOST`/`PORT`)
and stores `dice.json` in `/data` (override with `DATA_DIR`). It runs as the
non-root `node` user; `/data` is prepared in the image so a fresh named volume
is writable. Stored state is validated on read; a missing or corrupt file
starts a fresh count. Other storage errors return an error rather than silently
discarding data. No secrets are required at this milestone.

## Build and run (from this project directory)

```sh
podman build -t localhost/pockets-local-dice:0.1.0 .
podman run -d --name pockets-local-dice \
  -p 127.0.0.1:8080:8080 \
  -v pockets-local-dice-data:/data \
  --pull=never --cap-drop=ALL --security-opt=no-new-privileges \
  localhost/pockets-local-dice:0.1.0
```

Open <http://127.0.0.1:8080>, roll a few times, stop the container, start it
again with the same volume, and the last roll and roll count persist.
`pocket.yaml` declares the current build target (`arm64`); no multi-architecture
image has been verified. Desktop import is not implemented yet: this project
can run independently, but cannot currently be installed through Pockets Desktop.

## Local development

```sh
# Bind only to loopback when running directly on the host.
HOST=127.0.0.1 DATA_DIR=./data npm start

# Tests (ephemeral ports + temp data dirs, no container needed)
npm test
```

`npm test` and the server itself have zero dependencies. `npm install` is only
needed for the `format`/`format:check` scripts (Prettier dev dependency);
otherwise use the repository root's `npm run format:check`.
