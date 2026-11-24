import test from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import {
  lstat,
  mkdir,
  mkdtemp,
  readFile,
  readdir,
  rm,
  writeFile,
} from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { validateManifestFile } from "../src/validate.js";

const testDir = path.dirname(fileURLToPath(import.meta.url));
const packageDir = path.resolve(testDir, "..");
const repoRoot = path.resolve(packageDir, "..", "..");
const cliPath = path.join(packageDir, "src", "cli.js");
const templateDir = path.join(repoRoot, "templates", "basic-pocket");

const HOST_ARCH = process.arch === "x64" ? "amd64" : "arm64";

function runCli(args, options = {}) {
  return spawnSync(process.execPath, [cliPath, ...args], {
    encoding: "utf8",
    ...options,
  });
}

async function makeTempDir(t, prefix = "pocket-tools-create-") {
  const dir = await mkdtemp(path.join(os.tmpdir(), prefix));
  t.after(() => rm(dir, { recursive: true, force: true }));
  return dir;
}

async function listFilesRecursive(root, prefix = "") {
  const found = [];
  for (const entry of await readdir(root, { withFileTypes: true })) {
    const relative = prefix ? `${prefix}/${entry.name}` : entry.name;
    if (entry.isDirectory())
      found.push(
        ...(await listFilesRecursive(path.join(root, entry.name), relative)),
      );
    else found.push(relative);
  }
  return found.sort();
}

test("create renders a valid project into a parent directory whose name contains spaces", async (t) => {
  const base = await makeTempDir(t, "pocket create ");
  const parent = path.join(base, "my projects");
  await mkdir(parent);

  const result = runCli(["create", "./my projects/my-new-pocket"], {
    cwd: base,
  });
  assert.equal(result.status, 0, `stderr: ${result.stderr}`);
  assert.match(result.stdout, /created:/);
  assert.match(result.stdout, /next steps/);
  assert.doesNotMatch(result.stdout, /^cd\s/m);

  const dest = path.join(parent, "my-new-pocket");
  const manifestPath = path.join(dest, "pocket.yaml");

  // The rendered manifest passes the canonical schema validation.
  const manifest = await validateManifestFile(manifestPath);
  assert.equal(manifest.ok, true, `errors: ${JSON.stringify(manifest.errors)}`);
  assert.equal(manifest.data.metadata.id, "my-new-pocket");
  assert.equal(manifest.data.metadata.name, "My New Pocket");
  assert.deepEqual(manifest.data.runtime.architectures, [HOST_ARCH]);
  assert.equal(
    manifest.data.runtime.image,
    "localhost/pockets-my-new-pocket:0.1.0",
  );

  // Dotfiles are copied with the reviewed template's content.
  for (const dotfile of [".gitignore", ".containerignore"]) {
    assert.equal(
      await readFile(path.join(dest, dotfile), "utf8"),
      await readFile(path.join(templateDir, dotfile), "utf8"),
      `${dotfile} content`,
    );
  }

  // Title Case display name reached the rendered text files.
  assert.match(
    await readFile(path.join(dest, "README.md"), "utf8"),
    /My New Pocket/,
  );
  const agentInstructions = await readFile(
    path.join(dest, "AGENTS.md"),
    "utf8",
  );
  assert.match(agentInstructions, /POCKETS\.md/);
  assert.match(agentInstructions, /POCKETS_BRIEF\.md/);

  const runtimeInstructions = await readFile(
    path.join(dest, "POCKETS.md"),
    "utf8",
  );
  assert.match(runtimeInstructions, /lib\/cache\.js/);
  assert.match(runtimeInstructions, /stale-while-revalidate/);

  const containerfile = await readFile(
    path.join(dest, "Containerfile"),
    "utf8",
  );
  assert.match(containerfile, /COPY lib \.\/lib/);

  // No unresolved __POCKET_*__ tokens anywhere in the output.
  const files = await listFilesRecursive(dest);
  assert.deepEqual(files, await listFilesRecursive(templateDir));
  assert.ok(files.includes("lib/cache.js"));
  assert.ok(files.includes("test/cache.test.js"));
  for (const relative of files) {
    const contents = await readFile(
      path.join(dest, ...relative.split("/")),
      "utf8",
    );
    assert.doesNotMatch(
      contents,
      /__POCKET_[A-Z0-9_]*__/,
      `unresolved token in ${relative}`,
    );
  }
});

test("create refuses an existing destination and leaves it unchanged", async (t) => {
  const base = await makeTempDir(t);
  const dest = path.join(base, "taken-pocket");
  await mkdir(dest);
  await writeFile(path.join(dest, "keep.txt"), "keep\n", "utf8");

  const result = runCli(["create", "./taken-pocket"], { cwd: base });
  assert.equal(result.status, 1);
  assert.match(result.stderr, /already exists/);

  assert.equal(await readFile(path.join(dest, "keep.txt"), "utf8"), "keep\n");
  assert.deepEqual(await readdir(dest), ["keep.txt"]);

  // An empty directory is still an existing destination.
  await mkdir(path.join(base, "empty-pocket"));
  const empty = runCli(["create", "./empty-pocket"], { cwd: base });
  assert.equal(empty.status, 1);
  assert.match(empty.stderr, /already exists/);
  assert.deepEqual(await readdir(path.join(base, "empty-pocket")), []);
});

test("create refuses an invalid Pocket ID slug and creates no directory", async (t) => {
  const base = await makeTempDir(t);

  for (const bad of ["Bad_ID", "good--bad"]) {
    const result = runCli(["create", `./${bad}`], { cwd: base });
    assert.equal(result.status, 1, `${bad}: stderr: ${result.stderr}`);
    assert.match(result.stderr, /invalid Pocket ID/);
    await assert.rejects(() => lstat(path.join(base, bad)), { code: "ENOENT" });
  }
});
