import assert from "node:assert/strict";
import test from "node:test";

import {
  NPM_REGISTRY_PROPAGATION_RETRY_DELAYS_MS,
  waitForExpectedIntegrity
} from "./npm-registry.mjs";

const notFound = {
  status: 1,
  stdout: "",
  stderr: "npm error E404 No match found for version 0.1.0"
};

test("allows five minutes for bounded npm registry propagation", () => {
  assert.equal(
    NPM_REGISTRY_PROPAGATION_RETRY_DELAYS_MS.reduce((total, delay) => total + delay, 0),
    300_000
  );
  assert.equal(NPM_REGISTRY_PROPAGATION_RETRY_DELAYS_MS.at(-1), 120_000);
});

test("retries bounded npm registry propagation and verifies integrity", async () => {
  const responses = [notFound, notFound, { status: 0, stdout: '"sha512-expected"', stderr: "" }];
  const waits = [];
  await waitForExpectedIntegrity({
    lookup: () => responses.shift(),
    spec: "@reporch/cli-test@0.1.0",
    expectedIntegrity: "sha512-expected",
    retryDelaysMs: [10, 20],
    wait: async (milliseconds) => waits.push(milliseconds)
  });
  assert.deepEqual(waits, [10, 20]);
});

test("fails closed when npm exposes different immutable bytes", async () => {
  await assert.rejects(
    waitForExpectedIntegrity({
      lookup: () => ({ status: 0, stdout: '"sha512-different"', stderr: "" }),
      spec: "@reporch/cli-test@0.1.0",
      expectedIntegrity: "sha512-expected",
      retryDelaysMs: [],
      wait: async () => {}
    }),
    /registry integrity differs/
  );
});

test("fails closed after the registry propagation deadline", async () => {
  await assert.rejects(
    waitForExpectedIntegrity({
      lookup: () => notFound,
      spec: "@reporch/cli-test@0.1.0",
      expectedIntegrity: "sha512-expected",
      retryDelaysMs: [10, 20],
      wait: async () => {}
    }),
    /not visible with the expected integrity after bounded retries/
  );
});
