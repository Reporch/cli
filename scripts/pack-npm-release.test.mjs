import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";
import test from "node:test";

const script = join(dirname(fileURLToPath(import.meta.url)), "pack-npm-release.mjs");
const packageDirectories = [
  "darwin-arm64",
  "darwin-x64",
  "linux-arm64-gnu",
  "linux-x64-gnu",
  "win32-x64-msvc",
  "cli"
];

test("packs a relative release directory without treating it as a Git spec", () => {
  const root = mkdtempSync(join(tmpdir(), "reporch-pack-test-"));
  try {
    for (const directory of packageDirectories) {
      const packageDirectory = join(root, "dist", directory);
      mkdirSync(packageDirectory, { recursive: true });
      writeFileSync(
        join(packageDirectory, "package.json"),
        `${JSON.stringify({
          name: directory === "cli" ? "@reporch/cli" : `@reporch/cli-${directory}`,
          version: "0.1.0",
          files: ["payload.txt"]
        })}\n`
      );
      writeFileSync(join(packageDirectory, "payload.txt"), `${directory}\n`);
    }

    const result = spawnSync(process.execPath, [script, "dist"], {
      cwd: root,
      encoding: "utf8"
    });
    assert.equal(result.status, 0, result.stderr || result.stdout);

    const manifest = JSON.parse(
      readFileSync(join(root, "dist", "npm-pack-manifest.json"), "utf8")
    );
    assert.equal(manifest.packages.length, 6);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
