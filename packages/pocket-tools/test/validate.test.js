import test from "node:test";
import assert from "node:assert/strict";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";

import { validateManifestFile } from "../src/validate.js";

async function makeTempFile(t, name, contents) {
  const dir = await mkdtemp(path.join(os.tmpdir(), "pocket-validate-test-"));
  t.after(() => rm(dir, { recursive: true, force: true }));
  const filePath = path.join(dir, name);
  await writeFile(filePath, contents, "utf8");
  return filePath;
}

test("malformed YAML resolves to errors instead of throwing", async (t) => {
  const filePath = await makeTempFile(
    t,
    "broken.yaml",
    "runtime:\n  image: [unclosed\n",
  );
  const result = await validateManifestFile(filePath);
  assert.equal(result.ok, false);
  assert.equal(result.data, undefined);
  assert.ok(
    result.errors.some((message) => /YAML parse error/i.test(message)),
    `errors: ${JSON.stringify(result.errors)}`,
  );
});

test("duplicate keys and multiple documents are rejected", async (t) => {
  const dupPath = await makeTempFile(
    t,
    "duplicate-keys.yaml",
    "apiVersion: pockets.dev/v1alpha1\napiVersion: pockets.dev/v1alpha2\n",
  );
  const dup = await validateManifestFile(dupPath);
  assert.equal(dup.ok, false);
  assert.ok(
    dup.errors.some((message) => /unique/i.test(message)),
    `errors: ${JSON.stringify(dup.errors)}`,
  );

  const multiPath = await makeTempFile(
    t,
    "multi-doc.yaml",
    "apiVersion: pockets.dev/v1alpha1\n---\nkind: Pocket\n",
  );
  const multi = await validateManifestFile(multiPath);
  assert.equal(multi.ok, false);
  assert.ok(
    multi.errors.some((message) => /exactly one YAML document/i.test(message)),
    `errors: ${JSON.stringify(multi.errors)}`,
  );
});

test("bare anchors and aliases are both rejected", async (t) => {
  const anchoredPath = await makeTempFile(
    t,
    "anchored.yaml",
    "apiVersion: pockets.dev/v1alpha1\nmetadata:\n  id: &the-id hello-pocket\n",
  );
  const anchored = await validateManifestFile(anchoredPath);
  assert.equal(anchored.ok, false);
  assert.ok(
    anchored.errors.some((message) => /anchors and aliases/i.test(message)),
    `errors: ${JSON.stringify(anchored.errors)}`,
  );

  const aliasedPath = await makeTempFile(
    t,
    "aliased.yaml",
    "metadata:\n  id: &the-id hello-pocket\n  name: *the-id\n",
  );
  const aliased = await validateManifestFile(aliasedPath);
  assert.equal(aliased.ok, false);
  assert.ok(
    aliased.errors.some((message) => /anchors and aliases/i.test(message)),
    `errors: ${JSON.stringify(aliased.errors)}`,
  );
});

test("preview API declarations require distinct lowercase public hosts and display text", async (t) => {
  const base = await readFile(
    new URL("../../../examples/hello-pocket/pocket.yaml", import.meta.url),
    "utf8",
  );
  const request =
    "  - host: api.kraken.com\n    name: Kraken pricing\n    purpose: Show live BTC prices.\n";
  const path = await makeTempFile(
    t,
    "api.yaml",
    `${base}\npreviewApis:\n${request}`,
  );
  const parsed = await validateManifestFile(path);
  assert.equal(parsed.ok, true);
  assert.deepEqual(parsed.data.previewApis, [
    {
      host: "api.kraken.com",
      name: "Kraken pricing",
      purpose: "Show live BTC prices.",
    },
  ]);
  for (const invalid of [
    request + request,
    request.replace("api.kraken.com", "*.kraken.com"),
    request.replace("    purpose: Show live BTC prices.\n", ""),
  ]) {
    await writeFile(path, `${base}\npreviewApis:\n${invalid}`);
    assert.equal((await validateManifestFile(path)).ok, false);
  }
});

test("runtimeVariables declarations require distinct valid names, kind, and purpose", async (t) => {
  const base = await readFile(
    new URL("../../../examples/hello-pocket/pocket.yaml", import.meta.url),
    "utf8",
  );
  const decl =
    "  - name: PRICING_API_KEY\n    kind: secret\n    required: true\n    purpose: Authenticates market data lookups.\n  - name: CURRENCY\n    kind: plain\n    purpose: Default display currency.\n";
  const path = await makeTempFile(
    t,
    "runtime-vars.yaml",
    `${base}\nruntimeVariables:\n${decl}`,
  );
  const parsed = await validateManifestFile(path);
  assert.equal(parsed.ok, true, `errors: ${JSON.stringify(parsed.errors)}`);
  assert.deepEqual(parsed.data.runtimeVariables, [
    {
      name: "PRICING_API_KEY",
      kind: "secret",
      required: true,
      purpose: "Authenticates market data lookups.",
    },
    {
      name: "CURRENCY",
      kind: "plain",
      purpose: "Default display currency.",
    },
  ]);

  const single =
    "  - name: PRICING_API_KEY\n    kind: secret\n    purpose: Authenticates market data lookups.\n";
  for (const invalid of [
    single + single,
    single.replace("PRICING_API_KEY", "PORT"),
    single.replace("PRICING_API_KEY", "POCKETS_SECRET"),
    single.replace("PRICING_API_KEY", "BAD-NAME"),
    single.replace("kind: secret", "kind: password"),
    single.replace("    purpose: Authenticates market data lookups.\n", ""),
    `${single}    value: never-allowed\n`,
  ]) {
    await writeFile(path, `${base}\nruntimeVariables:\n${invalid}`);
    assert.equal(
      (await validateManifestFile(path)).ok,
      false,
      `expected rejection for:\n${invalid}`,
    );
  }
});
