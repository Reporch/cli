import test from "node:test";
import { spawnSync } from "node:child_process";
import assert from "node:assert/strict";

test("release workflows preserve the supply-chain contract", () => {
  const result = spawnSync(process.execPath, ["scripts/check-workflows.mjs"], {
    encoding: "utf8"
  });
  assert.equal(result.status, 0, result.stderr || result.stdout);
  assert.match(result.stdout, /workflow contract passed/);
});
