import assert from "node:assert/strict";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import { TARGETS, assertReleaseBinary, validateOutputArgument } from "./release-lib.mjs";

test("release target set is exact and unique", () => {
  assert.equal(TARGETS.length, 5);
  assert.equal(new Set(TARGETS.map((item) => item.target)).size, 5);
  assert.equal(new Set(TARGETS.map((item) => item.packageName)).size, 5);
});

test("unsafe release outputs and tiny binaries fail closed", () => {
  assert.throws(() => validateOutputArgument("."), /unsafe release output/);
  assert.throws(() => validateOutputArgument("/"), /unsafe release output/);
  const directory = mkdtempSync(join(tmpdir(), "reporch-release-lib-"));
  const binary = join(directory, "reporch");
  writeFileSync(binary, "not a release binary");
  assert.throws(() => assertReleaseBinary(binary), /unsafe size/);
});
