import assert from "node:assert/strict";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import {
  TARGETS,
  assertReleaseBinary,
  npmTagForVersion,
  validateOutputArgument
} from "./release-lib.mjs";

test("release target set is exact and unique", () => {
  assert.equal(TARGETS.length, 5);
  assert.equal(new Set(TARGETS.map((item) => item.target)).size, 5);
  assert.equal(new Set(TARGETS.map((item) => item.packageName)).size, 5);
});

test("prereleases never move the npm latest tag", () => {
  assert.equal(npmTagForVersion("1.0.0-rc.1"), "next");
  assert.equal(npmTagForVersion("1.0.0"), "candidate");
  assert.throws(() => npmTagForVersion("1.0"), /invalid release version/);
});

test("unsafe release outputs and tiny binaries fail closed", () => {
  assert.throws(() => validateOutputArgument("."), /unsafe release output/);
  assert.throws(() => validateOutputArgument("/"), /unsafe release output/);
  const directory = mkdtempSync(join(tmpdir(), "reporch-release-lib-"));
  const binary = join(directory, "reporch");
  writeFileSync(binary, "not a release binary");
  assert.throws(() => assertReleaseBinary(binary), /unsafe size/);
});
