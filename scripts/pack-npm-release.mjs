import assert from "node:assert/strict";
import { mkdirSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { spawnSync } from "node:child_process";

const [releaseDirectory = "dist"] = process.argv.slice(2);
const releaseRoot = resolve(releaseDirectory);
const tarballs = join(releaseRoot, "tarballs");
mkdirSync(tarballs);
const packageDirectories = [
  "darwin-arm64",
  "darwin-x64",
  "linux-arm64-gnu",
  "linux-x64-gnu",
  "win32-x64-msvc",
  "cli"
];
const packed = [];
for (const directory of packageDirectories) {
  const result = spawnSync(
    "npm",
    ["pack", join(releaseRoot, directory), "--json", "--pack-destination", tarballs],
    { encoding: "utf8" }
  );
  assert.equal(result.status, 0, result.stderr || result.stdout);
  const [entry] = JSON.parse(result.stdout);
  assert.ok(entry?.filename && entry?.integrity && entry?.shasum, `invalid npm pack output for ${directory}`);
  const manifest = JSON.parse(
    readFileSync(join(releaseRoot, directory, "package.json"), "utf8")
  );
  assert.equal(entry.id, `${manifest.name}@${manifest.version}`);
  assert.ok(entry.files.every((file) => !file.path.startsWith("test/")));
  assert.ok(entry.files.every((file) => !file.path.includes(".env")));
  packed.push({
    name: manifest.name,
    version: manifest.version,
    filename: entry.filename,
    integrity: entry.integrity,
    shasum: entry.shasum,
    size: entry.size,
    unpackedSize: entry.unpackedSize
  });
}
assert.equal(readdirSync(tarballs).filter((name) => name.endsWith(".tgz")).length, 6);
writeFileSync(
  join(releaseRoot, "npm-pack-manifest.json"),
  `${JSON.stringify({ schema: "reporch.cli-npm-pack.v1", packages: packed }, null, 2)}\n`
);
console.log(`packed ${packed.length} npm packages`);
