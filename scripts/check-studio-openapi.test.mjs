import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import test from "node:test";

test("the pinned Studio OpenAPI exposes the review-pool contract", () => {
  const result = spawnSync(process.execPath, ["scripts/check-studio-openapi.mjs"], {
    encoding: "utf8"
  });
  assert.equal(result.status, 0, result.stderr || result.stdout);
  assert.match(result.stdout, /Studio OpenAPI lock passed/);
});
