import test from "node:test";
import { spawnSync } from "node:child_process";
import assert from "node:assert/strict";

test("all release versions and package names are synchronized", () => {
  const result = spawnSync(process.execPath, ["scripts/check-versions.mjs"], {
    encoding: "utf8"
  });
  assert.equal(result.status, 0, result.stderr || result.stdout);
  assert.match(result.stdout, /version contract passed/);
});
