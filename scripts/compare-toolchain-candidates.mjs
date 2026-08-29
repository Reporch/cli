import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { createReadStream, lstatSync, readFileSync, readdirSync } from "node:fs";
import { join, relative, resolve, sep } from "node:path";
import { Transform } from "node:stream";
import { pipeline } from "node:stream/promises";
import { pathToFileURL } from "node:url";

const MAX_FILES = 256;
const MAX_TOTAL_BYTES = 48 * 1024 * 1024 * 1024;

async function sha256(path) {
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
  return hash.digest("hex");
}

async function inventory(rootArgument) {
  const root = resolve(rootArgument);
  const rootStat = lstatSync(root);
  assert.ok(rootStat.isDirectory() && !rootStat.isSymbolicLink(), "candidate root must be a directory");
  const paths = [];
  function visit(directory) {
    for (const name of readdirSync(directory).sort()) {
      const path = join(directory, name);
      const stat = lstatSync(path);
      assert.ok(!stat.isSymbolicLink(), `candidate contains a symlink: ${path}`);
      if (stat.isDirectory()) visit(path);
      else {
        assert.ok(stat.isFile(), `candidate contains a non-regular entry: ${path}`);
        paths.push({ path, stat });
        assert.ok(paths.length <= MAX_FILES, "candidate tree exceeds the file-count bound");
      }
    }
  }
  visit(root);
  assert.ok(paths.length > 0, "candidate tree is empty");
  let totalBytes = 0;
  const records = [];
  for (const value of paths) {
    totalBytes += value.stat.size;
    assert.ok(totalBytes <= MAX_TOTAL_BYTES, "candidate tree exceeds the byte bound");
    const candidatePath = relative(root, value.path).split(sep).join("/");
    assert.ok(candidatePath && !candidatePath.startsWith("../"), "candidate path escaped its root");
    records.push({
      path: candidatePath,
      mode: value.stat.mode & 0o777,
      size: value.stat.size,
      sha256: await sha256(value.path)
    });
  }
  return { root, records, totalBytes };
}

function sourceRevision(value) {
  const revisions = new Set();
  for (const record of value.records.filter(({ path }) => path.endsWith(".intoto.jsonl"))) {
    const statement = JSON.parse(readFileSync(join(value.root, record.path), "utf8"));
    const revision = statement?.predicate?.buildDefinition?.internalParameters?.sourceRevision;
    assert.match(revision ?? "", /^[a-f0-9]{40}$/u, `invalid source revision in ${record.path}`);
    revisions.add(revision);
  }
  assert.equal(revisions.size, 1, "toolchain provenance must bind one source revision");
  return [...revisions][0];
}

function validateIndex(value) {
  const index = JSON.parse(readFileSync(join(value.root, "toolchains-v2-index.json"), "utf8"));
  assert.equal(index.schema, "reporch.toolchain-index.v2");
  assert.equal(index.sequence, 8);
  assert.ok(Array.isArray(index.entries) && index.entries.length === 12);
  for (const entry of index.entries) {
    assert.equal(entry.bundles.length, 5);
    assert.equal(new Set(entry.bundles.map(({ target }) => target)).size, 5);
  }
  return index;
}

export async function compareToolchainCandidates(firstArgument, secondArgument) {
  const first = await inventory(firstArgument);
  const second = await inventory(secondArgument);
  assert.notEqual(first.root, second.root, "reproducibility requires independent output directories");
  assert.deepEqual(second.records, first.records, "toolchain candidates are not byte-for-byte reproducible");
  assert.equal(second.totalBytes, first.totalBytes);
  const source_revision = sourceRevision(first);
  assert.equal(sourceRevision(second), source_revision);
  const index = validateIndex(first);
  assert.deepEqual(validateIndex(second), index);
  return {
    schema: "reporch.toolchain-reproducibility.v2",
    source_revision,
    sequence: index.sequence,
    toolchains: index.entries.length,
    file_count: first.records.length,
    total_bytes: first.totalBytes,
    tree_sha256: createHash("sha256").update(JSON.stringify(first.records)).digest("hex")
  };
}

async function main() {
  const [first, second, ...extra] = process.argv.slice(2);
  assert.ok(first && second && extra.length === 0, "usage: node scripts/compare-toolchain-candidates.mjs <first> <second>");
  process.stdout.write(`${JSON.stringify(await compareToolchainCandidates(first, second), null, 2)}\n`);
}

const invoked = process.argv[1] ? pathToFileURL(resolve(process.argv[1])).href : null;
if (invoked === import.meta.url) await main();
