"use strict";

// Regression tests for the image pattern in pocket.schema.json.
// Dependency-free: loads the schema JSON and exercises the raw regex.

const test = require("node:test");
const assert = require("node:assert/strict");
const path = require("node:path");

const schema = require(path.join(__dirname, "pocket.schema.json"));
const imagePattern = new RegExp(
  schema.properties.runtime.properties.image.pattern,
);

test("image pattern accepts the documented example image", () => {
  assert.match("localhost/pockets-hello:0.1.0", imagePattern);
});

test("image pattern rejects whitespace anywhere, including first character", () => {
  for (const value of [" image", "image ", "local host/img", "\timage", " "]) {
    assert.doesNotMatch(
      value,
      imagePattern,
      `expected ${JSON.stringify(value)} to be rejected`,
    );
  }
});

test("image pattern rejects leading hyphen and empty string", () => {
  for (const value of ["-flag", "-"]) {
    assert.doesNotMatch(
      value,
      imagePattern,
      `expected ${JSON.stringify(value)} to be rejected`,
    );
  }
  assert.doesNotMatch("", imagePattern);
});
