import test from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { lstat, mkdtemp, rm, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const testDir = path.dirname(fileURLToPath(import.meta.url));
const packageDir = path.resolve(testDir, "..");
const repoRoot = path.resolve(packageDir, "..", "..");
const cliPath = path.join(packageDir, "src", "cli.js");
const helloManifest = path.join(
  repoRoot,
  "examples",
  "hello-pocket",
  "pocket.yaml",
);

const HELLO_YAML = `apiVersion: pockets.dev/v1alpha1
kind: Pocket
metadata:
  id: hello-pocket
  name: Hello Pocket
  version: "0.1.0"
  description: A small persistent web app you control.
runtime:
  image: localhost/pockets-hello:0.1.0
  architectures: [arm64, amd64]
web:
  port: 8080
  healthcheck: /health
storage:
  name: data
  path: /data
`;

function runCli(args, options = {}) {
  return spawnSync(process.execPath, [cliPath, ...args], {
    encoding: "utf8",
    ...options,
  });
}

async function makeTempDir(t) {
  const dir = await mkdtemp(path.join(os.tmpdir(), "pocket-tools-test-"));
  t.after(() => rm(dir, { recursive: true, force: true }));
  return dir;
}

test("validate accepts the bundled hello manifest from any cwd", async (t) => {
  const cwd = await makeTempDir(t);
  const result = runCli(["validate", helloManifest], { cwd });
  assert.equal(result.status, 0, `stderr: ${result.stderr}`);
  assert.match(result.stdout, /valid/i);
});

test("validate rejects a manifest with an unknown field", async (t) => {
  const dir = await makeTempDir(t);
  const manifestPath = path.join(dir, "pocket.yaml");
  await writeFile(manifestPath, `${HELLO_YAML}sponsor: tbd\n`, "utf8");

  const result = runCli(["validate", manifestPath], { cwd: dir });
  assert.equal(result.status, 1, `stdout: ${result.stdout}`);
  assert.match(result.stderr, /sponsor/);

  // The exported API is usable directly by later tooling.
  const { validateManifestFile } = await import("../src/validate.js");
  const api = await validateManifestFile(manifestPath);
  assert.equal(api.ok, false);
  assert.equal(
    api.errors.some((message) => message.includes("sponsor")),
    true,
    `errors: ${JSON.stringify(api.errors)}`,
  );
});

test("invalid CLI args exit 2 and a nonexistent file exits 1", async (t) => {
  const dir = await makeTempDir(t);

  const noArgs = runCli([], { cwd: dir });
  assert.equal(noArgs.status, 2);
  assert.match(noArgs.stderr, /Usage:.*validate/);
  // `create` is now a real command; later commands must not appear yet.
  assert.doesNotMatch(noArgs.stderr, /template|skills/);

  const unknownCommand = runCli(["deploy", "somewhere"], { cwd: dir });
  assert.equal(unknownCommand.status, 2);
  assert.match(unknownCommand.stderr, /Usage:.*validate/);

  const createNoArgs = runCli(["create"], { cwd: dir });
  assert.equal(createNoArgs.status, 2);

  const createTooMany = runCli(["create", "./one", "./two"], { cwd: dir });
  assert.equal(createTooMany.status, 2);

  const createFlag = runCli(["create", "--dry-run", "./fine-pocket"], {
    cwd: dir,
  });
  assert.equal(createFlag.status, 2);
  await assert.rejects(() => lstat(path.join(dir, "fine-pocket")));

  const missing = runCli(
    ["validate", path.join(dir, "does-not-exist", "pocket.yaml")],
    { cwd: dir },
  );
  assert.equal(missing.status, 1);
  assert.match(missing.stderr, /file not found/);
});
