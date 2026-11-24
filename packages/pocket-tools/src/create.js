// Create a new Pocket project from the reviewed templates/basic-pocket
// starter. Copies and renders plain text files only: no scripts are run,
// no dependencies installed, no images built, no runtime contacted.

import {
  lstat,
  mkdir,
  readFile,
  readdir,
  stat,
  writeFile,
} from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { validateManifestFile } from "./validate.js";

const TEMPLATE_DIR = fileURLToPath(
  new URL("../../../templates/basic-pocket/", import.meta.url),
);

const POCKET_ID_PATTERN = /^[a-z0-9]+(-[a-z0-9]+)*$/;
const MAX_POCKET_ID_LENGTH = 63;
const HOST_ARCHITECTURES = { arm64: "arm64", x64: "amd64" };
const UNRESOLVED_TOKEN_PATTERN = /__POCKET_[A-Z0-9_]*__/g;
const MANIFEST_NAME = "pocket.yaml";

/** "my-cool-pocket" -> "My Cool Pocket" */
function titleCase(id) {
  return id
    .split("-")
    .map((word) => word.charAt(0).toUpperCase() + word.slice(1))
    .join(" ");
}

/** Recursively collect template files and directories, rejecting symlinks
 * and anything that is not a regular file or directory. Dotfiles are kept. */
async function collectTemplate(
  dir,
  prefix = "",
  collected = { files: [], dirs: [] },
) {
  let entries;
  try {
    entries = await readdir(dir, { withFileTypes: true });
  } catch (err) {
    const where = path.relative(TEMPLATE_DIR, dir) || ".";
    throw new Error(
      `template directory not readable (${where}): ${err.message}`,
    );
  }
  for (const entry of entries.sort((a, b) => (a.name < b.name ? -1 : 1))) {
    const absolute = path.join(dir, entry.name);
    const relative = prefix ? `${prefix}/${entry.name}` : entry.name;
    if (entry.isSymbolicLink()) {
      throw new Error(`unexpected symlink in template: ${relative}`);
    }
    if (entry.isDirectory()) {
      collected.dirs.push(relative);
      await collectTemplate(absolute, relative, collected);
    } else if (entry.isFile()) {
      collected.files.push(relative);
    } else {
      throw new Error(`unexpected non-regular file in template: ${relative}`);
    }
  }
  return collected;
}

/** Read every template file, substitute the supported tokens, and refuse
 * unresolved __POCKET_*__ tokens or non-text content. */
async function renderTemplate(values) {
  const templateRoot = await lstat(TEMPLATE_DIR).catch(() => null);
  if (!templateRoot || !templateRoot.isDirectory()) {
    throw new Error(`template directory not found: ${TEMPLATE_DIR}`);
  }

  const { files, dirs } = await collectTemplate(TEMPLATE_DIR);
  if (!files.includes(MANIFEST_NAME)) {
    throw new Error(`template is missing ${MANIFEST_NAME}`);
  }

  const rendered = [];
  for (const relative of files) {
    const absolute = path.join(TEMPLATE_DIR, ...relative.split("/"));
    const text = await readFile(absolute, "utf8");
    if (text.includes("\0")) {
      throw new Error(`unexpected non-text file in template: ${relative}`);
    }
    const contents = text
      .replaceAll("__POCKET_ID__", values.id)
      .replaceAll(
        "__POCKET_NAME__",
        relative === MANIFEST_NAME ? JSON.stringify(values.name) : values.name,
      )
      .replaceAll("__POCKET_ARCH__", values.arch);
    const unresolved = [
      ...new Set(contents.match(UNRESOLVED_TOKEN_PATTERN) ?? []),
    ];
    if (unresolved.length > 0) {
      throw new Error(
        `template file ${relative} has unresolved token(s): ${unresolved.join(", ")}`,
      );
    }
    rendered.push({ relative, contents });
  }
  return { dirs, rendered };
}

/**
 * Create a new Pocket project from the starter at `target` (resolved
 * against the current working directory). The directory basename is the
 * Pocket ID and must satisfy the manifest schema's ID rules.
 *
 * Resolves (never rejects) to:
 *   { ok: true, destPath, metadata, arch, files, note }
 *   { ok: false, destPath, errors: string[] }
 *
 * Refuses an existing destination of any kind (including empty directories
 * and symlinks), never overwrites or deletes files, and requires the parent
 * directory to already exist. On a write failure the partial directory is
 * left in place and reported rather than deleted.
 */
export async function createPocket(target) {
  const destPath = path.resolve(target);
  const result = { ok: false, destPath, errors: [] };

  // Reject unsupported hosts before touching any files.
  const arch = HOST_ARCHITECTURES[process.arch];
  if (!arch) {
    result.errors.push(
      `unsupported host architecture: ${process.arch} (create supports arm64 and amd64 hosts)`,
    );
    return result;
  }

  const id = path.basename(destPath);
  if (!POCKET_ID_PATTERN.test(id) || id.length > MAX_POCKET_ID_LENGTH) {
    result.errors.push(
      `invalid Pocket ID "${id}": use lowercase alphanumeric words separated by single hyphens, at most ${MAX_POCKET_ID_LENGTH} characters`,
    );
    return result;
  }

  // Refuse any existing destination: files, empty directories, and
  // symlinks (lstat does not follow symlinks, so broken ones count too).
  let existing = null;
  try {
    existing = await lstat(destPath);
  } catch (err) {
    if (err.code === "ENOTDIR") {
      result.errors.push(
        `parent is not a directory: ${path.dirname(destPath)}`,
      );
      return result;
    }
    if (err.code !== "ENOENT") {
      result.errors.push(`cannot check destination: ${err.message}`);
      return result;
    }
  }
  if (existing) {
    result.errors.push(
      `destination already exists: ${destPath} (create never overwrites or deletes files)`,
    );
    return result;
  }

  const parent = path.dirname(destPath);
  const parentInfo = await stat(parent).catch(() => null);
  if (!parentInfo) {
    result.errors.push(`parent directory does not exist: ${parent}`);
    return result;
  }
  if (!parentInfo.isDirectory()) {
    result.errors.push(`parent is not a directory: ${parent}`);
    return result;
  }

  // Read and render the whole trusted template before creating anything.
  let template;
  try {
    template = await renderTemplate({ id, name: titleCase(id), arch });
  } catch (err) {
    result.errors.push(err.message);
    return result;
  }

  const partialNote = `partial project left at ${destPath}; review and remove it manually`;

  try {
    await mkdir(destPath);
  } catch (err) {
    result.errors.push(
      err.code === "EEXIST"
        ? `destination already exists: ${destPath} (create never overwrites or deletes files)`
        : `cannot create directory: ${err.message}`,
    );
    return result;
  }

  try {
    for (const dir of template.dirs) {
      await mkdir(path.join(destPath, ...dir.split("/")), { recursive: true });
    }
    for (const { relative, contents } of template.rendered) {
      await writeFile(path.join(destPath, ...relative.split("/")), contents, {
        flag: "wx",
      });
    }
  } catch (err) {
    result.errors.push(`failed writing project files: ${err.message}`);
    result.errors.push(partialNote);
    return result;
  }

  const manifest = await validateManifestFile(
    path.join(destPath, MANIFEST_NAME),
  );
  if (!manifest.ok) {
    for (const message of manifest.errors) {
      result.errors.push(`generated ${MANIFEST_NAME} is invalid: ${message}`);
    }
    result.errors.push(partialNote);
    return result;
  }

  result.ok = true;
  result.errors = [];
  result.metadata = manifest.data.metadata;
  result.arch = arch;
  result.files = template.rendered.map(({ relative }) => relative);
  result.note =
    "Copied and rendered plain text files only: no scripts were executed, no dependencies installed, no images built.";
  return result;
}
