const assert = require("node:assert/strict");
const { once, EventEmitter } = require("node:events");
const { Readable } = require("node:stream");
const test = require("node:test");
const { createGateway } = require("./preview-egress.cjs");

async function fixture(t, resolve, connect) {
  const server = createGateway(["api.example.com"], resolve, connect);
  const ready = once(server, "listening");
  server.listen(0, "127.0.0.1");
  await ready;
  t.after(() => {
    server.close();
    server.closeAllConnections();
  });
  return (target, options) => {
    const url = new URL(`http://127.0.0.1:${server.address().port}/fetch`);
    url.searchParams.set("url", target);
    return fetch(url, options);
  };
}

function upstream(status, headers, body, record) {
  return (options, receive) => {
    record?.(options);
    const request = new EventEmitter();
    request.end = () => {
      const reply = Readable.from(body);
      reply.statusCode = status;
      reply.headers = headers;
      queueMicrotask(() => receive(reply));
    };
    return request;
  };
}

test("forwards an authenticated GET to the approved TLS hostname and pins the checked address", async (t) => {
  const reply = await fixture(
    t,
    async (host) => {
      assert.equal(host, "api.example.com.");
      return ["8.8.8.8"];
    },
    upstream(
      429,
      {
        "content-type": "application/json",
        "retry-after": "20",
        "set-cookie": "upstream=synthetic",
      },
      [Buffer.from('{"fixture":true}')],
      (options) => {
        assert.equal(options.hostname, "api.example.com");
        assert.equal(options.servername, "api.example.com");
        assert.equal(options.path, "/prices?currency=nzd");
        assert.equal(options.rejectUnauthorized, true);
        assert.equal(options.agent, false);
        options.lookup("api.example.com", {}, (error, address, family) => {
          assert.equal(error, null);
          assert.equal(address, "8.8.8.8");
          assert.equal(family, 4);
        });
        assert.equal(options.headers.authorization, "Bearer fixture");
        assert.equal(options.headers["x-cg-demo-api-key"], "fixture-key");
        assert.equal(options.headers["x-forwarded-host"], undefined);
        assert.equal(options.headers.cookie, undefined);
      },
    ),
  );
  const result = await reply("https://api.example.com/prices?currency=nzd", {
    headers: {
      authorization: "Bearer fixture",
      "x-cg-demo-api-key": "fixture-key",
      "x-forwarded-host": "private",
      cookie: "fixture=test",
    },
  });
  assert.equal(result.status, 429);
  assert.equal(result.headers.get("retry-after"), "20");
  assert.equal(result.headers.has("set-cookie"), false);
  assert.deepEqual(await result.json(), { fixture: true });
});

test("rejects unapproved hosts, alternate ports, methods, and private or mixed DNS before connection", async (t) => {
  let resolved = 0;
  const addresses = [
    ["8.8.8.8", "169.254.169.254"],
    ["127.1.2.3"],
    ["192.168.1.5"],
  ];
  const reply = await fixture(
    t,
    async () => addresses[resolved++],
    () => assert.fail("must never dial"),
  );
  assert.equal(
    (await reply("https://api.example.com.invalid/prices")).status,
    403,
  );
  assert.equal(
    (await reply("https://api.example.com:8443/prices")).status,
    400,
  );
  assert.equal((await reply("http://api.example.com/prices")).status, 400);
  assert.equal(
    (await reply("https://api.example.com/prices", { method: "POST" })).status,
    405,
  );
  assert.equal(resolved, 0);
  for (const _ of addresses) {
    const response = await reply("https://api.example.com/prices");
    assert.equal(response.status, 403);
    assert.deepEqual(await response.json(), { error: "non_public_address" });
  }
});

test("blocks redirects and oversized responses while supporting HEAD", async (t) => {
  const reply = await fixture(
    t,
    async () => ["8.8.8.8"],
    (options, receive) => {
      if (options.path === "/redirect") {
        return upstream(
          302,
          { location: "https://unapproved.example.com/" },
          [],
        )(options, receive);
      }
      if (options.method === "HEAD") {
        return upstream(200, { etag: '"fixture"' }, [])(options, receive);
      }
      return upstream(200, {}, [Buffer.alloc(4 * 1024 * 1024 + 1)])(
        options,
        receive,
      );
    },
  );
  const redirected = await reply("https://api.example.com/redirect");
  assert.equal(redirected.status, 502);
  assert.equal(redirected.headers.has("location"), false);
  assert.deepEqual(await redirected.json(), { error: "redirect_blocked" });
  const oversized = await reply("https://api.example.com/large");
  assert.equal(oversized.status, 502);
  assert.deepEqual(await oversized.json(), { error: "response_too_large" });
  const head = await reply("https://api.example.com/prices", {
    method: "HEAD",
  });
  assert.equal(head.status, 200);
  assert.equal(head.headers.get("etag"), '"fixture"');
  assert.equal(await head.text(), "");
});
