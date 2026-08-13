import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";

const rootPackage = JSON.parse(readFileSync("package.json", "utf8"));
const rootLock = JSON.parse(readFileSync("package-lock.json", "utf8"));
const cliPackage = JSON.parse(readFileSync("npm/cli/package.json", "utf8"));
const cargo = readFileSync("crates/reporch-cli/Cargo.toml", "utf8");
const cargoVersion = cargo.match(/^version = "([^"]+)"$/m)?.[1];

assert.equal(rootPackage.version, cliPackage.version, "source and npm versions differ");
assert.equal(rootLock.version, rootPackage.version, "package lock version differs");
assert.equal(
  rootLock.packages?.[""]?.version,
  rootPackage.version,
  "package lock root version differs"
);
assert.equal(cargoVersion, cliPackage.version, "Cargo and npm versions differ");

const expected = new Set(Object.keys(cliPackage.optionalDependencies));
const seen = new Set();
for (const directory of readdirSync("npm/platforms", { withFileTypes: true })) {
  if (!directory.isDirectory()) continue;
  const manifest = JSON.parse(
    readFileSync(join("npm/platforms", directory.name, "package.json"), "utf8")
  );
  assert.equal(manifest.version, cliPackage.version, `${manifest.name} version differs`);
  assert.equal(
    cliPackage.optionalDependencies[manifest.name],
    cliPackage.version,
    `${manifest.name} is not pinned exactly by @reporch/cli`
  );
  assert.equal(manifest.license, "Apache-2.0", `${manifest.name} license differs`);
  assert.equal(manifest.scripts, undefined, `${manifest.name} must not have scripts`);
  seen.add(manifest.name);
}
assert.deepEqual(seen, expected, "platform package set differs from optionalDependencies");
assert.equal(cliPackage.license, "Apache-2.0");
assert.equal(cliPackage.scripts, undefined, "@reporch/cli must not have lifecycle scripts");
console.log(`version contract passed for ${cliPackage.version} and ${seen.size} targets`);
