import assert from "node:assert/strict";
import { test } from "node:test";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createPocket } from "../server.js";

const FORM = { "content-type": "application/x-www-form-urlencoded" };

async function startPocket(dataDir) {
  const pocket = createPocket({ dataDir, host: "127.0.0.1", port: 0 });
  await pocket.listen();
  return { pocket, base: `http://127.0.0.1:${pocket.address().port}` };
}

async function makeDataDir(t) {
  const dataDir = await mkdtemp(join(tmpdir(), "pockets-notes-"));
  t.after(() => rm(dataDir, { recursive: true, force: true }));
  return dataDir;
}

// Parse the CSRF token actually rendered into the form, not the factory's
// csrfToken property, so the test exercises what a browser would submit.
function tokenFrom(html) {
  const match = html.match(
    /<input type="hidden" name="token" value="([0-9a-f]+)"/,
  );
  assert.ok(match, `expected a hidden token input in:\n${html}`);
  return match[1];
}

// Note content is escaped, so the first "</textarea>" ends the real textarea.
function textareaInner(html) {
  const match = html.match(/<textarea[^>]*>([\s\S]*?)<\/textarea>/);
  assert.ok(match, `expected a textarea in:\n${html}`);
  return match[1];
}

function formBody(fields) {
  return new URLSearchParams(fields).toString();
}

async function savedNote(dataDir) {
  return readFile(join(dataDir, "note.txt"), "utf8");
}

test("GET /health returns 200 JSON", async (t) => {
  const dataDir = await makeDataDir(t);
  const { pocket, base } = await startPocket(dataDir);
  t.after(() => pocket.close());

  const res = await fetch(`${base}/health`);
  assert.equal(res.status, 200);
  assert.match(res.headers.get("content-type"), /application\/json/);
  assert.deepEqual(await res.json(), { status: "ok" });
});

test("GET / renders an empty editor and never creates the note file", async (t) => {
  const dataDir = await makeDataDir(t);
  const { pocket, base } = await startPocket(dataDir);
  t.after(() => pocket.close());

  const res = await fetch(`${base}/`);
  assert.equal(res.status, 200);
  assert.match(res.headers.get("content-type"), /text\/html/);

  const html = await res.text();
  assert.equal(textareaInner(html), "");
  assert.equal(tokenFrom(html).length, 64); // 32 random bytes as hex

  // A read-only GET must not materialise note.txt.
  await assert.rejects(savedNote(dataDir), { code: "ENOENT" });
});

test("POST /note saves with the page token, survives a restart, and clears to empty", async (t) => {
  const dataDir = await makeDataDir(t);
  const note = "persisted note\nsecond line";

  const first = await startPocket(dataDir);
  t.after(() => first.pocket.close());
  const token = tokenFrom(await (await fetch(`${first.base}/`)).text());

  const post = await fetch(`${first.base}/note`, {
    method: "POST",
    headers: FORM,
    body: formBody({ token, note }),
    redirect: "manual",
  });
  assert.equal(post.status, 303);
  assert.equal(post.headers.get("location"), "/?saved=1");
  assert.equal(await savedNote(dataDir), note);

  // Following the redirect shows the confirmation message.
  const page = await (await fetch(`${first.base}/?saved=1`)).text();
  assert.match(page, /class="saved"[^>]*>Note saved\.</);

  // Restart against the same data directory: the note must survive and the
  // new process must render its own freshly generated token.
  await first.pocket.close();
  const second = await startPocket(dataDir);
  t.after(() => second.pocket.close());
  assert.equal(await savedNote(dataDir), note);

  const restarted = await (await fetch(`${second.base}/`)).text();
  assert.equal(textareaInner(restarted), note);
  const restartedToken = tokenFrom(restarted);
  assert.notEqual(restartedToken, token);

  // Submitting an empty note clears it: file remains, content is empty.
  const cleared = await fetch(`${second.base}/note`, {
    method: "POST",
    headers: FORM,
    body: formBody({ token: restartedToken, note: "" }),
    redirect: "manual",
  });
  assert.equal(cleared.status, 303);
  assert.equal(await savedNote(dataDir), "");
  assert.equal(
    textareaInner(await (await fetch(`${second.base}/`)).text()),
    "",
  );
});

test("note content is HTML-escaped and a leading newline round-trips", async (t) => {
  const dataDir = await makeDataDir(t);
  const { pocket, base } = await startPocket(dataDir);
  t.after(() => pocket.close());

  const note =
    '\n</textarea><script>alert("xss")</script>\nTom & Jerry\'s <b>bold</b> "quotes"';
  const token = tokenFrom(await (await fetch(`${base}/`)).text());
  const post = await fetch(`${base}/note`, {
    method: "POST",
    headers: FORM,
    body: formBody({ token, note }),
    redirect: "manual",
  });
  assert.equal(post.status, 303);
  assert.equal(await savedNote(dataDir), note); // stored verbatim

  const html = await (await fetch(`${base}/`)).text();
  const escaped = note
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#39;");
  // HTML parsers swallow a newline right after <textarea>, so the page must
  // re-emit it, and nothing may escape the textarea as live markup.
  assert.equal(textareaInner(html), `\n${escaped}`);
  assert.ok(!html.includes("</textarea><script>"), "textarea was broken out");
  assert.ok(!html.includes("<script>"), "raw <script> tag leaked into page");
});

test("invalid tokens and malformed or oversized submissions are rejected without changing the note", async (t) => {
  const dataDir = await makeDataDir(t);
  const { pocket, base } = await startPocket(dataDir);
  t.after(() => pocket.close());

  const token = tokenFrom(await (await fetch(`${base}/`)).text());
  const save = await fetch(`${base}/note`, {
    method: "POST",
    headers: FORM,
    body: formBody({ token, note: "keep me" }),
    redirect: "manual",
  });
  assert.equal(save.status, 303);

  const attempts = [
    {
      status: 403,
      name: "bogus token",
      body: formBody({ token: "0".repeat(64), note: "evil" }),
    },
    { status: 403, name: "missing token", body: formBody({ note: "evil" }) },
    {
      status: 413,
      name: "note over 64 KiB",
      body: formBody({ token, note: "x".repeat(64 * 1024 + 1) }),
    },
    { status: 400, name: "missing note field", body: formBody({ token }) },
    {
      status: 413,
      name: "body over 256 KiB",
      body: formBody({ token, note: "x".repeat(300 * 1024) }),
    },
  ];
  for (const attempt of attempts) {
    const res = await fetch(`${base}/note`, {
      method: "POST",
      headers: FORM,
      body: attempt.body,
    });
    assert.equal(res.status, attempt.status, `${attempt.name} should fail`);
    await res.text();
  }

  // Wrong content type is rejected before anything is read.
  const badType = await fetch(`${base}/note`, {
    method: "POST",
    headers: { "content-type": "text/plain" },
    body: formBody({ token, note: "evil" }),
  });
  assert.equal(badType.status, 415);
  await badType.text();

  assert.equal(await savedNote(dataDir), "keep me");
});
