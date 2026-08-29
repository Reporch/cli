import assert from "node:assert/strict";
import test from "node:test";

import { readToolchainLock, validateToolchainLock } from "./check-toolchain-sources.mjs";

test("the RC8 toolchain lock is complete and digest pinned", () => {
  const result = readToolchainLock();
  assert.equal(result.lock.entries.length, 12);
  assert.match(result.sha256, /^[a-f0-9]{64}$/u);
});

test("unpinned and duplicate toolchains fail closed", () => {
  const source = readToolchainLock().lock;
  const unpinned = structuredClone(source);
  unpinned.entries[0].image = "python:latest";
  assert.throws(() => validateToolchainLock(unpinned));
  const duplicate = structuredClone(source);
  duplicate.entries[1].id = duplicate.entries[0].id;
  assert.throws(() => validateToolchainLock(duplicate), /duplicate toolchain ID/u);
});
