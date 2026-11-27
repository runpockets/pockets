// __POCKET_NAME__ starter: dependency-free Node HTTP server. Replace this
// file with your own app code and keep GET /health returning 200 for readiness.

import { createServer } from "node:http";
import { pathToFileURL } from "node:url";

function homePage() {
  return `<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <title>Basic Pocket</title>
    <style>
      body {
        margin: 0;
        min-height: 100vh;
        display: grid;
        place-items: center;
        background: #0f172a;
        color: #e2e8f0;
        font-family: system-ui, sans-serif;
      }
      main {
        text-align: center;
        padding: 2rem;
      }
    </style>
  </head>
  <body>
    <main>
      <h1>Hello from your Pocket 👋</h1>
      <p>Get creating..</p>
    </main>
  </body>
</html>
`;
}

// Exported factory so tests can start the server on an ephemeral port with a
// temporary data directory. Options fall back to container-friendly defaults.
export function createPocket(options = {}) {
  const dataDir = options.dataDir ?? process.env.DATA_DIR ?? "/data";
  const host = options.host ?? process.env.HOST ?? "0.0.0.0";
  const port = options.port ?? (Number(process.env.PORT) || 8080);

  const server = createServer((req, res) => {
    let pathname;
    try {
      // Route on req.url only, against a fixed base: the Host header is
      // untrusted, must never affect routing, and a malformed one must not
      // throw inside this request handler.
      pathname = new URL(req.url ?? "/", "http://localhost").pathname;
    } catch {
      res.writeHead(400, { "content-type": "text/plain; charset=utf-8" });
      res.end("Bad request\n");
      return;
    }

    if (req.method !== "GET") {
      res.writeHead(405, { "content-type": "text/plain; charset=utf-8" });
      res.end("Method not allowed\n");
      return;
    }

    if (pathname === "/health") {
      res.writeHead(200, { "content-type": "application/json" });
      res.end('{"status":"ok"}\n');
      return;
    }

    if (pathname === "/") {
      res.writeHead(200, { "content-type": "text/html; charset=utf-8" });
      res.end(homePage());
      return;
    }

    res.writeHead(404, { "content-type": "text/plain; charset=utf-8" });
    res.end("Not found\n");
  });

  let closing;

  return {
    dataDir,
    server,
    listen() {
      return new Promise((resolve, reject) => {
        const onError = (err) => reject(err);
        server.once("error", onError);
        server.listen(port, host, () => {
          server.removeListener("error", onError);
          resolve();
        });
      });
    },
    address() {
      return server.address();
    },
    close() {
      if (!closing) {
        closing = new Promise((resolve, reject) => {
          server.close((err) => (err ? reject(err) : resolve()));
        });
        // Release idle keep-alive connections so close() settles promptly.
        server.closeIdleConnections();
      }
      return closing;
    },
  };
}

const isMain = Boolean(
  process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href,
);

if (isMain) {
  const pocket = createPocket();
  await pocket.listen();
  const addr = pocket.address();
  console.log(
    `pocket listening on http://${addr.address}:${addr.port} ` +
      `(data: ${pocket.dataDir})`,
  );

  let shuttingDown = false;
  const shutdown = (signal) => {
    if (shuttingDown) return;
    shuttingDown = true;
    console.log(`pocket received ${signal}, shutting down`);
    pocket.close().then(
      () => process.exit(0),
      (err) => {
        console.error("pocket: error during shutdown", err);
        process.exit(1);
      },
    );
  };
  process.on("SIGTERM", () => shutdown("SIGTERM"));
  process.on("SIGINT", () => shutdown("SIGINT"));
}
