# Hello Pocket

A tiny first-party example Pocket for Pockets M1: a dependency-free Node HTTP
server that demonstrates the Pocket lifecycle — HTTP readiness via `/health`
and persistent data in a container volume.

## Endpoints

- `GET /` — hello-world page with a persistent visit count.
- `GET /health` — `200` with `{"status":"ok"}` for readiness checks.

The server binds `0.0.0.0:8080` in the container (override with `HOST`/`PORT`)
and stores `visits.json` in `/data` (override with `DATA_DIR`). It runs as the
non-root `node` user; `/data` is prepared in the image so a fresh named volume
is writable. No secrets are required at this milestone.

## Build and run (from the repository root)

```sh
podman build -t localhost/pockets-hello:0.1.0 examples/hello-pocket
podman run -d --name pockets-hello \
  -p 127.0.0.1:8080:8080 \
  -v pockets-hello-data:/data \
  localhost/pockets-hello:0.1.0
```

Open <http://127.0.0.1:8080>, stop the container, start it again with the same
volume, and the visit count persists.

## Local development

```sh
cd examples/hello-pocket
DATA_DIR=/tmp/pockets-hello-data node server.js

# Tests (ephemeral ports + temp data dirs, no container needed)
npm test
```
