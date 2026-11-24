// Pocket manifest validation: load YAML as plain data, validate against the
// canonical JSON Schema. No schema execution, no network access, no shell.

import { readFile, stat } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { isAlias, isCollection, isPair, parseAllDocuments } from "yaml";
import Ajv2020 from "ajv/dist/2020.js";

export const MAX_MANIFEST_BYTES = 128 * 1024;

const SCHEMA_PATH = fileURLToPath(
  new URL("../../pocket-spec/pocket.schema.json", import.meta.url),
);

/** Lazily compiled schema validator, shared across calls. */
let validatorPromise;
function getValidator() {
  validatorPromise ??= (async () => {
    const schema = JSON.parse(await readFile(SCHEMA_PATH, "utf8"));
    const ajv = new Ajv2020({ allErrors: true });
    return ajv.compile(schema);
  })();
  return validatorPromise;
}

/** Turn a JSON pointer like "/runtime/architectures/0" into a dotted path. */
function pointerToPath(instancePath) {
  if (!instancePath) return "(root)";
  let out = "";
  for (const raw of instancePath.split("/").slice(1)) {
    const segment = raw.replaceAll("~1", "/").replaceAll("~0", "~");
    if (/^\d+$/.test(segment)) out += `[${segment}]`;
    else out += out ? `.${segment}` : segment;
  }
  return out || "(root)";
}

/** Render one ajv error as "field.path: message (detail)". */
function describeSchemaError(err) {
  let message = err.message ?? "failed validation";
  const params = err.params ?? {};
  switch (err.keyword) {
    case "additionalProperties":
      if (params.additionalProperty !== undefined)
        message += ` (unexpected property "${params.additionalProperty}")`;
      break;
    case "required":
      if (params.missingProperty !== undefined)
        message += ` (missing "${params.missingProperty}")`;
      break;
    case "enum":
      if (Array.isArray(params.allowedValues))
        message += ` (allowed: ${params.allowedValues.map((v) => JSON.stringify(v)).join(", ")})`;
      break;
    case "const":
      if (params.allowedValue !== undefined)
        message += ` (expected ${JSON.stringify(params.allowedValue)})`;
      break;
    case "pattern":
      if (params.pattern !== undefined)
        message += ` (pattern: ${params.pattern})`;
      break;
    case "uniqueItems":
      message += " (duplicate items)";
      break;
    default:
      break;
  }
  return `${pointerToPath(err.instancePath)}: ${message}`;
}

/**
 * Collect every anchored (`&ref`) or aliased (`*ref`) node in a parsed YAML
 * AST. The `anchor` property is only present when `&ref` was written, so this
 * catches bare anchors (including anchored scalars) that toJS would accept.
 */
function findAnchoredNodes(node, found = []) {
  if (!node || typeof node !== "object") return found;
  if (isAlias(node) || node.anchor !== undefined) {
    found.push(node);
  } else if (isPair(node)) {
    findAnchoredNodes(node.key, found);
    findAnchoredNodes(node.value, found);
  } else if (isCollection(node)) {
    for (const item of node.items ?? []) findAnchoredNodes(item, found);
  }
  return found;
}

/**
 * Parse manifest text as plain YAML data.
 * Returns { value, errors } with string errors; never throws.
 */
function parseManifestText(text) {
  let documents;
  try {
    documents = parseAllDocuments(text, { uniqueKeys: true });
  } catch (err) {
    return { value: undefined, errors: [`YAML parse error: ${err.message}`] };
  }

  const errors = [];
  if (documents.length === 0) {
    return { value: undefined, errors: ["file contains no YAML document"] };
  }
  if (documents.length > 1) {
    return {
      value: undefined,
      errors: [`expected exactly one YAML document, found ${documents.length}`],
    };
  }

  const doc = documents[0];
  // yaml error messages already embed line/column context.
  for (const err of doc.errors) errors.push(`YAML parse error: ${err.message}`);
  if (errors.length > 0) return { value: undefined, errors };

  // Conservative: manifests must be plain data; anchors/aliases are rejected.
  let anchored;
  try {
    anchored = findAnchoredNodes(doc.contents);
  } catch (err) {
    return { value: undefined, errors: [`YAML parse error: ${err.message}`] };
  }
  if (anchored.length > 0) {
    errors.push("YAML anchors and aliases are not allowed in a manifest");
    return { value: undefined, errors };
  }

  let value;
  try {
    value = doc.toJS({ maxAliasCount: 0 });
  } catch (err) {
    return { value: undefined, errors: [`YAML parse error: ${err.message}`] };
  }
  if (value === null || value === undefined) {
    return { value: undefined, errors: ["document is empty"] };
  }
  return { value, errors: [] };
}

/**
 * Validate a Pocket manifest file against the canonical pocket.schema.json.
 *
 * Resolves (never rejects) to:
 *   { ok: true,  filePath, data }
 *   { ok: false, filePath, errors: string[] }
 *
 * `filePath` is the resolved absolute path; `data` is the parsed manifest
 * object, suitable for reuse by generators.
 */
export async function validateManifestFile(filePath) {
  const resolved = path.resolve(filePath);
  const result = { ok: false, filePath: resolved, errors: [] };

  let info;
  try {
    info = await stat(resolved);
  } catch (err) {
    result.errors.push(
      err.code === "ENOENT"
        ? `file not found: ${resolved}`
        : `cannot read file: ${err.message}`,
    );
    return result;
  }
  if (!info.isFile()) {
    result.errors.push(`not a regular file: ${resolved}`);
    return result;
  }
  if (info.size > MAX_MANIFEST_BYTES) {
    result.errors.push(
      `file is too large: ${info.size} bytes exceeds the ${MAX_MANIFEST_BYTES}-byte limit`,
    );
    return result;
  }

  let text;
  try {
    text = await readFile(resolved, "utf8");
  } catch (err) {
    result.errors.push(`cannot read file: ${err.message}`);
    return result;
  }

  // The file may have changed between stat and read; recheck the bytes read.
  const bytes = Buffer.byteLength(text, "utf8");
  if (bytes > MAX_MANIFEST_BYTES) {
    result.errors.push(
      `file is too large: ${bytes} bytes exceeds the ${MAX_MANIFEST_BYTES}-byte limit`,
    );
    return result;
  }

  const { value, errors } = parseManifestText(text);
  if (errors.length > 0) {
    result.errors.push(...errors);
    return result;
  }

  let validate;
  try {
    validate = await getValidator();
  } catch (err) {
    result.errors.push(`cannot load schema: ${err.message}`);
    return result;
  }
  if (!validate(value)) {
    for (const err of validate.errors ?? []) {
      result.errors.push(describeSchemaError(err));
    }
    return result;
  }
  const apiHosts = (value.previewApis ?? []).map(({ host }) => host);
  if (new Set(apiHosts).size !== apiHosts.length) {
    result.errors.push("Each previewApis hostname must appear only once.");
    return result;
  }

  result.ok = true;
  result.data = value;
  result.errors = [];
  return result;
}
