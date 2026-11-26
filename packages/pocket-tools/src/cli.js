#!/usr/bin/env node
// pockets CLI. Commands: create, validate.

import { createPocket } from "./create.js";
import { validateManifestFile } from "./validate.js";

const USAGE = `Usage: npm run pocket -- create <new-directory> | validate <path-to-pocket.yaml>

Commands:
  create <new-directory>   Create a new Pocket project in a new directory
  validate <path>          Validate a Pocket manifest against pocket.schema.json
`;

function usageError(message) {
  if (message) console.error(`Error: ${message}\n`);
  console.error(USAGE.trimEnd());
  process.exit(2);
}

const [command, ...rest] = process.argv.slice(2);

if (command === "create") {
  if (rest.length !== 1) {
    usageError("create requires exactly one new-directory path");
  }
  if (rest[0].startsWith("-")) {
    usageError("create does not accept flags");
  }
  const result = await createPocket(rest[0]);
  if (result.ok) {
    console.log(`created: ${result.destPath}`);
    console.log(
      `  id: ${result.metadata.id}  name: ${result.metadata.name}  architecture: ${result.arch}`,
    );
    console.log(`  ${result.note}`);
    // Next steps are static text: the path may contain shell metacharacters,
    // so never print an interpolated `cd` command.
    console.log("next steps:");
    console.log(
      "  1. Open the new folder in your coding tool. AGENTS.md points to the runtime requirements in POCKETS.md.",
    );
    console.log(
      "  2. Inside the new folder, run npm test after reviewing the code; no install is needed for the starter.",
    );
    console.log(
      "  3. In Pockets, choose Link project, then Develop → Start for live preview. Read POCKETS.md and README.md for the runtime contract and explicit installation steps.",
    );
    process.exitCode = 0;
  } else {
    for (const message of result.errors) console.error(`error: ${message}`);
    process.exitCode = 1;
  }
} else if (command === "validate") {
  if (rest.length !== 1) {
    usageError("validate requires exactly one manifest path");
  }
  const result = await validateManifestFile(rest[0]);
  if (result.ok) {
    console.log(`valid: ${result.filePath}`);
    process.exitCode = 0;
  } else {
    for (const message of result.errors) console.error(`error: ${message}`);
    process.exitCode = 1;
  }
} else {
  usageError(command ? `unknown command: ${command}` : "missing command");
}
