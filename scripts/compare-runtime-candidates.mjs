import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { lstatSync, readFileSync, readdirSync } from "node:fs";
import { basename, join, relative, resolve, sep } from "node:path";
import { pathToFileURL } from "node:url";

const MAX_FILES = 512;
const MAX_TOTAL_BYTES = 16 * 1024 * 1024 * 1024;

function sha256(path) {
  return createHash("sha256").update(readFileSync(path)).digest("hex");
}

function inventory(rootArgument) {
  const root = resolve(rootArgument);
  const rootStat = lstatSync(root);
  assert.ok(rootStat.isDirectory() && !rootStat.isSymbolicLink(), "candidate root must be a directory");
  const records = [];
  let totalBytes = 0;

  function visit(directory) {
    for (const name of readdirSync(directory).sort()) {
      assert.ok(name !== "." && name !== ".." && !name.includes("\0"), "unsafe candidate entry");
      const path = join(directory, name);
      const stat = lstatSync(path);
      assert.ok(!stat.isSymbolicLink(), `candidate contains a symlink: ${path}`);
      if (stat.isDirectory()) {
        visit(path);
        continue;
      }
      assert.ok(stat.isFile(), `candidate contains a non-regular entry: ${path}`);
      totalBytes += stat.size;
      assert.ok(totalBytes <= MAX_TOTAL_BYTES, "candidate tree exceeds the byte bound");
      const candidatePath = relative(root, path).split(sep).join("/");
      assert.ok(candidatePath && !candidatePath.startsWith("../"), "candidate path escaped its root");
      records.push({
        path: candidatePath,
        mode: stat.mode & 0o777,
        size: stat.size,
        sha256: sha256(path)
      });
      assert.ok(records.length <= MAX_FILES, "candidate tree exceeds the file-count bound");
    }
  }

  visit(root);
  assert.ok(records.length > 0, "candidate tree is empty");
  return { root, records, totalBytes };
}

function sourceRevision(inventoryValue) {
  const revisions = new Set();
  for (const record of inventoryValue.records.filter(({ path }) => path.endsWith(".intoto.jsonl"))) {
    const statement = JSON.parse(readFileSync(join(inventoryValue.root, record.path), "utf8"));
    const revision = statement?.predicate?.buildDefinition?.internalParameters?.sourceRevision;
    assert.match(revision ?? "", /^[a-f0-9]{40}$/u, `invalid source revision in ${record.path}`);
    revisions.add(revision);
  }
  assert.equal(revisions.size, 1, "runtime provenance must bind one source revision");
  return [...revisions][0];
}

export function compareRuntimeCandidates(firstArgument, secondArgument) {
  const first = inventory(firstArgument);
  const second = inventory(secondArgument);
  assert.notEqual(first.root, second.root, "reproducibility requires independent output directories");
  assert.deepEqual(second.records, first.records, "runtime candidate builds are not byte-for-byte reproducible");
  assert.equal(second.totalBytes, first.totalBytes);
  const revision = sourceRevision(first);
  assert.equal(sourceRevision(second), revision, "candidate builds used different source revisions");
  const treeSha256 = createHash("sha256").update(JSON.stringify(first.records)).digest("hex");
  const targets = first.records
    .filter(({ path }) => /^manifests\/runtime-.+-manifest\.json$/u.test(path))
    .map(({ path }) => basename(path).slice("runtime-".length, -"-manifest.json".length))
    .sort();
  assert.deepEqual(targets, [
    "darwin-arm64",
    "darwin-x64",
    "linux-arm64-gnu",
    "linux-x64-gnu",
    "windows-x64-msvc"
  ]);
  return {
    schema: "reporch.runtime-reproducibility.v1",
    source_revision: revision,
    targets,
    file_count: first.records.length,
    total_bytes: first.totalBytes,
    tree_sha256: treeSha256
  };
}

function main() {
  const [first, second, ...extra] = process.argv.slice(2);
  if (!first || !second || extra.length > 0) {
    throw new Error("usage: node scripts/compare-runtime-candidates.mjs <first> <second>");
  }
  process.stdout.write(`${JSON.stringify(compareRuntimeCandidates(first, second), null, 2)}\n`);
}

const invoked = process.argv[1] ? pathToFileURL(resolve(process.argv[1])).href : null;
if (invoked === import.meta.url) main();
