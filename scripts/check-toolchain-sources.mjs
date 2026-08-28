import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { lstatSync, readFileSync } from "node:fs";
import { basename, resolve } from "node:path";
import { pathToFileURL } from "node:url";

const ID = /^[a-z0-9](?:[a-z0-9._-]{0,62}[a-z0-9])?$/u;
const IMAGE = /^[a-z0-9][a-z0-9./:_-]*@sha256:[a-f0-9]{64}$/u;

export function validateToolchainLock(value) {
  assert.deepEqual(Object.keys(value).sort(), ["entries", "schema", "sequence", "source_date_epoch"]);
  assert.equal(value.schema, "reporch.toolchain-sources-lock.v1");
  assert.equal(value.sequence, 8);
  assert.ok(Number.isSafeInteger(value.source_date_epoch) && value.source_date_epoch > 0);
  assert.ok(Array.isArray(value.entries) && value.entries.length === 12);
  const ids = new Set();
  const languages = new Set();
  for (const entry of value.entries) {
    assert.deepEqual(Object.keys(entry).sort(), ["id", "image", "image_mib", "language"]);
    assert.match(entry.id, ID);
    assert.match(entry.language, ID);
    assert.match(entry.image, IMAGE);
    assert.ok(
      Number.isSafeInteger(entry.image_mib) && entry.image_mib >= 256 && entry.image_mib <= 8192,
      `invalid image size for ${entry.id}`
    );
    assert.ok(!ids.has(entry.id), `duplicate toolchain ID: ${entry.id}`);
    ids.add(entry.id);
    languages.add(entry.language);
  }
  assert.ok(ids.has("bash-5.3") && ids.has("swift-6.3") && ids.has("dotnet-10-csharp"));
  assert.equal(languages.size, 12);
  return value;
}

export function readToolchainLock(pathArgument = "runtime/toolchains.lock.json") {
  const path = resolve(pathArgument);
  const stat = lstatSync(path);
  assert.ok(stat.isFile() && !stat.isSymbolicLink() && stat.size > 0 && stat.size <= 64 * 1024);
  const bytes = readFileSync(path);
  const lock = validateToolchainLock(JSON.parse(bytes));
  return {
    path,
    lock,
    sha256: createHash("sha256").update(bytes).digest("hex")
  };
}

function main() {
  const [path = "runtime/toolchains.lock.json", ...extra] = process.argv.slice(2);
  assert.equal(extra.length, 0, "usage: node scripts/check-toolchain-sources.mjs [lock]");
  const result = readToolchainLock(path);
  process.stdout.write(
    `${JSON.stringify({
      schema: "reporch.toolchain-source-check.v1",
      file: basename(result.path),
      sequence: result.lock.sequence,
      entries: result.lock.entries.length,
      sha256: result.sha256
    })}\n`
  );
}

const invoked = process.argv[1] ? pathToFileURL(resolve(process.argv[1])).href : null;
if (invoked === import.meta.url) main();
