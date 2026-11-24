"use strict";

const http = require("node:http");
const https = require("node:https");
const { Resolver } = require("node:dns/promises");
const { BlockList, isIPv4 } = require("node:net");
const { networkInterfaces } = require("node:os");

const blocked = new BlockList();
for (const [address, prefix] of [
  ["0.0.0.0", 8],
  ["10.0.0.0", 8],
  ["100.64.0.0", 10],
  ["127.0.0.0", 8],
  ["169.254.0.0", 16],
  ["172.16.0.0", 12],
  ["192.0.0.0", 24],
  ["192.0.2.0", 24],
  ["192.88.99.0", 24],
  ["192.168.0.0", 16],
  ["198.18.0.0", 15],
  ["198.51.100.0", 24],
  ["203.0.113.0", 24],
  ["224.0.0.0", 4],
  ["240.0.0.0", 4],
]) {
  blocked.addSubnet(address, prefix);
}

const REQUEST_HEADERS = new Set([
  "accept",
  "authorization",
  "api-key",
  "if-none-match",
  "if-modified-since",
  "user-agent",
]);
const RESPONSE_HEADERS = [
  "content-type",
  "content-encoding",
  "cache-control",
  "etag",
  "last-modified",
  "retry-after",
];
const MAX_ACTIVE = 8;
const MAX_BODY = 4 * 1024 * 1024;
const UPSTREAM_TIMEOUT = 10_000;

function failure(response, status, code) {
  response.writeHead(status, {
    "content-type": "application/json",
    "cache-control": "no-store",
  });
  response.end(JSON.stringify({ error: code }));
}

function requestedTarget(path) {
  if (!path?.startsWith("/fetch?") || path.length > 8192) return null;
  const parameters = new URLSearchParams(path.slice(7));
  const target = parameters.get("url");
  if (parameters.size !== 1 || !target || /[\s\\#]/.test(target)) return null;
  try {
    const url = new URL(target);
    if (
      url.protocol === "https:" &&
      !url.username &&
      !url.password &&
      !url.port &&
      !url.hash
    )
      return url;
  } catch {}
  return null;
}

function outboundRequest(request, target, address, connect) {
  const headers = {
    "accept-encoding": "identity",
    "user-agent": "PocketsPreview/1.0",
  };
  for (const [name, value] of Object.entries(request.headers)) {
    if (
      typeof value === "string" &&
      (REQUEST_HEADERS.has(name) || /^x-[a-z0-9-]+-(?:key|token)$/.test(name))
    ) {
      headers[name] = value;
    }
  }
  return new Promise((resolve, reject) => {
    const upstream = connect(
      {
        hostname: target.hostname,
        servername: target.hostname,
        path: target.pathname + target.search,
        method: request.method,
        port: 443,
        family: 4,
        autoSelectFamily: false,
        lookup: (_hostname, _options, callback) => callback(null, address, 4),
        agent: false,
        rejectUnauthorized: true,
        signal: AbortSignal.timeout(UPSTREAM_TIMEOUT),
        headers,
      },
      (response) => {
        response.on("error", reject);
        const status = response.statusCode;
        if (
          !status ||
          status < 200 ||
          (status >= 300 && status < 400 && status !== 304)
        ) {
          response.destroy();
          resolve({ error: "redirect_blocked" });
          return;
        }
        const chunks = [];
        let size = 0;
        response.on("data", (chunk) => {
          size += chunk.length;
          if (size > MAX_BODY) {
            response.destroy();
            resolve({ error: "response_too_large" });
            return;
          }
          chunks.push(chunk);
        });
        response.once("end", () => {
          const headers = Object.fromEntries(
            RESPONSE_HEADERS.filter(
              (name) => typeof response.headers[name] === "string",
            ).map((name) => [name, response.headers[name]]),
          );
          resolve({ status, headers, body: Buffer.concat(chunks, size) });
        });
      },
    );
    upstream.on("error", reject);
    upstream.once("upgrade", (_reply, socket) => {
      socket.destroy();
      reject(new Error("upgrade blocked"));
    });
    upstream.end();
  });
}

function createGateway(approvedHosts, resolve4, connect = https.request) {
  const approved = new Set(approvedHosts);
  let active = 0;
  const server = http.createServer(
    {
      headersTimeout: 5_000,
      requestTimeout: 5_000,
      keepAliveTimeout: 1_000,
      maxHeaderSize: 16 * 1024,
    },
    async (request, response) => {
      if (request.method !== "GET" && request.method !== "HEAD")
        return failure(response, 405, "method_not_allowed");
      if (
        request.headers["transfer-encoding"] ||
        Number(request.headers["content-length"] || 0) !== 0
      ) {
        return failure(response, 400, "request_body_blocked");
      }
      if (request.url === "/health") return response.end("ready");
      const target = requestedTarget(request.url);
      if (!target) return failure(response, 400, "invalid_https_url");
      if (!approved.has(target.hostname))
        return failure(response, 403, "host_not_allowed");
      if (active >= MAX_ACTIVE) return failure(response, 503, "gateway_busy");
      active++;
      try {
        const addresses = await resolve4(target.hostname + ".");
        if (
          !addresses.length ||
          addresses.some((ip) => !isIPv4(ip) || blocked.check(ip))
        ) {
          return failure(response, 403, "non_public_address");
        }
        const result = await outboundRequest(
          request,
          target,
          addresses[0],
          connect,
        );
        if (result.error) return failure(response, 502, result.error);
        response.writeHead(result.status, result.headers);
        response.end(result.body);
      } catch {
        if (!response.headersSent)
          failure(response, 502, "upstream_unavailable");
        else response.destroy();
      } finally {
        active--;
      }
    },
  );
  server.on("connect", (_request, socket) => socket.destroy());
  server.on("upgrade", (_request, socket) => socket.destroy());
  server.on("checkContinue", (_request, response) =>
    failure(response, 400, "request_body_blocked"),
  );
  server.maxConnections = 24;
  server.setTimeout(20_000, (socket) => socket.destroy());
  return server;
}

function run(serializedHosts) {
  const resolver = new Resolver({ timeout: 2_000, tries: 2 });
  const ipv4 = networkInterfaces()["pockets-in"]?.filter(
    (entry) => entry.family === "IPv4",
  );
  if (ipv4?.length !== 1)
    throw new Error("The gateway is missing its isolated interface.");
  createGateway(JSON.parse(serializedHosts), (host) =>
    resolver.resolve4(host),
  ).listen(3137, ipv4[0].address);
}

module.exports = { createGateway, run };
