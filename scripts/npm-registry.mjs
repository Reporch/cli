import assert from "node:assert/strict";
import { setTimeout as sleep } from "node:timers/promises";

const NOT_FOUND = /E404|not in this registry|No match found/i;

export const NPM_REGISTRY_PROPAGATION_RETRY_DELAYS_MS = [
  1_000,
  2_000,
  4_000,
  8_000,
  15_000,
  30_000,
  60_000,
  60_000,
  120_000
];

export async function waitForExpectedIntegrity({
  lookup,
  spec,
  expectedIntegrity,
  retryDelaysMs = NPM_REGISTRY_PROPAGATION_RETRY_DELAYS_MS,
  wait = sleep
}) {
  let lastError = "";
  for (let attempt = 0; attempt <= retryDelaysMs.length; attempt += 1) {
    const result = lookup();
    if (result.status === 0) {
      const integrity = JSON.parse(result.stdout);
      assert.equal(integrity, expectedIntegrity, `${spec} registry integrity differs`);
      return;
    }

    lastError = result.stderr || result.stdout || `exit status ${result.status}`;
    assert.match(lastError, NOT_FOUND, lastError);
    if (attempt < retryDelaysMs.length) await wait(retryDelaysMs[attempt]);
  }

  assert.fail(`${spec} was not visible with the expected integrity after bounded retries\n${lastError}`);
}
