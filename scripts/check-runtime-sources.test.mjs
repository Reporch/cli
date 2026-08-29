import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import test from "node:test";

test("runtime source identities and hashes remain pinned", () => {
  const result = spawnSync(process.execPath, ["scripts/check-runtime-sources.mjs"], {
    encoding: "utf8"
  });
  assert.equal(result.status, 0, result.stderr || result.stdout);
  assert.match(result.stdout, /complete and immutable/);
});
