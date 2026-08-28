import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { createReadStream, lstatSync, readFileSync } from "node:fs";
import { basename, join, resolve } from "node:path";
import { Transform } from "node:stream";
import { pipeline } from "node:stream/promises";
import { pathToFileURL } from "node:url";

const ID = /^[a-z0-9](?:[a-z0-9._-]{0,62}[a-z0-9])?$/u;

async function digest(path, maximum) {
  const stat = lstatSync(path);
  assert.ok(stat.isFile() && !stat.isSymbolicLink() && stat.size > 0 && stat.size <= maximum);
  const hash = createHash("sha256");
  await pipeline(
    createReadStream(path),
    new Transform({
      transform(chunk, _encoding, callback) {
        hash.update(chunk);
        callback();
      }
    })
  );
  return { mode: stat.mode & 0o777, size: stat.size, sha256: hash.digest("hex") };
}

export async function compareToolchainEntry(primaryArgument, rebuildArgument, id) {
  assert.match(id, ID);
  const primary = resolve(primaryArgument);
  const rebuild = resolve(rebuildArgument);
  assert.notEqual(primary, rebuild);
  const variants = [
    ["linux-arm64.ext4.zst", "linux-arm64.source.spdx.json"],
    ["linux-x64.ext4.zst", "linux-x64.source.spdx.json"],
    ["windows-x64.vhdx.zst", "linux-x64.source.spdx.json"]
  ];
  const records = [];
  for (const [variant, sourceSbom] of variants) {
    const archive = `${id}-${variant}`;
    for (const suffix of ["", ".build.json"]) {
      const name = `${archive}${suffix}`;
      const maximum = suffix ? 16 * 1024 : 2 * 1024 * 1024 * 1024;
      const expected = await digest(join(primary, name), maximum);
      const actual = await digest(join(rebuild, name), maximum);
      assert.deepEqual(actual, expected, `${name} differs from its independent rebuild`);
      records.push({ name, ...expected });
    }
    const primarySbom = join(primary, `${archive}.spdx.json`);
    const rebuiltSbom = join(rebuild, `${id}-${sourceSbom}`);
    const expectedSbom = await digest(primarySbom, 32 * 1024 * 1024);
    const actualSbom = await digest(rebuiltSbom, 32 * 1024 * 1024);
    assert.deepEqual(actualSbom, expectedSbom, `${sourceSbom} differs from its independent rebuild`);
    records.push({ name: basename(primarySbom), ...expectedSbom });
  }
  return {
    schema: "reporch.toolchain-entry-reproducibility.v2",
    id,
    files: records.length,
    bytes: records.reduce((total, record) => total + record.size, 0),
    tree_sha256: createHash("sha256").update(JSON.stringify(records)).digest("hex")
  };
}

export function aggregateToolchainEntryReports(values) {
  assert.ok(Array.isArray(values) && values.length === 12);
  const sorted = [...values].sort((left, right) => left.id.localeCompare(right.id));
  assert.equal(new Set(sorted.map(({ id }) => id)).size, 12);
  for (const value of sorted) {
    assert.equal(value.schema, "reporch.toolchain-entry-reproducibility.v2");
    assert.match(value.tree_sha256, /^[a-f0-9]{64}$/u);
  }
  return {
    schema: "reporch.toolchain-reproducibility.v2",
    toolchains: sorted.length,
    files: sorted.reduce((total, value) => total + value.files, 0),
    bytes: sorted.reduce((total, value) => total + value.bytes, 0),
    entries: sorted,
    evidence_sha256: createHash("sha256").update(JSON.stringify(sorted)).digest("hex")
  };
}

async function main() {
  const values = process.argv.slice(2);
  if (values[0] === "--aggregate") {
    assert.equal(values.length, 2, "usage: node scripts/compare-toolchain-entry.mjs --aggregate <jsonl>");
    const reports = readFileSync(values[1], "utf8")
      .trim()
      .split("\n")
      .filter(Boolean)
      .map((line) => JSON.parse(line));
    process.stdout.write(`${JSON.stringify(aggregateToolchainEntryReports(reports), null, 2)}\n`);
    return;
  }
  assert.equal(values.length, 3, "usage: node scripts/compare-toolchain-entry.mjs <primary> <rebuild> <id>");
  process.stdout.write(`${JSON.stringify(await compareToolchainEntry(...values))}\n`);
}

const invoked = process.argv[1] ? pathToFileURL(resolve(process.argv[1])).href : null;
if (invoked === import.meta.url) await main();
