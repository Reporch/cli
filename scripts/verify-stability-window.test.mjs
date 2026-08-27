import assert from "node:assert/strict";
import test from "node:test";

import { verifyStabilityWindow } from "./verify-stability-window.mjs";

const openedAt = "2026-08-24T19:00:16Z";
const closedAt = "2026-09-23T19:00:16Z";

function issues(overrides = {}) {
  return [{
    number: 29,
    title: "Reporch CLI 1.0.0-rc.6 — 30-day stability window",
    createdAt: openedAt,
    closedAt,
    ...overrides,
  }];
}

function comments({ count = 30, author = "github-actions[bot]", duplicate = false } = {}) {
  const start = Date.parse("2026-08-25T00:00:00Z");
  return {
    comments: Array.from({ length: count }, (_, index) => {
      const offset = duplicate ? 0 : index;
      const date = new Date(start + offset * 24 * 60 * 60 * 1000)
        .toISOString()
        .slice(0, 10);
      return {
        author: { login: author },
        body: `<!-- reporch-cli-stability:${date}:passed:${1000 + index}:1 -->`,
      };
    }),
  };
}

test("accepts 30 distinct bot-authored passing days after 30 elapsed days", () => {
  const result = verifyStabilityWindow(issues(), comments());
  assert.equal(result.accepted, true);
  assert.equal(result.passed_day_count, 30);
  assert.equal(result.issue_number, 29);
});

test("rejects an early close even with enough comments", () => {
  assert.throws(
    () => verifyStabilityWindow(issues({ closedAt: "2026-09-22T19:00:15Z" }), comments()),
    /30 full days/,
  );
});

test("rejects duplicate days and untrusted authors", () => {
  assert.throws(() => verifyStabilityWindow(issues(), comments({ duplicate: true })), /fewer than 30/);
  assert.throws(() => verifyStabilityWindow(issues(), comments({ author: "attacker" })), /fewer than 30/);
});

test("rejects daily evidence outside the active window", () => {
  const evidence = comments();
  evidence.comments[0].body = "<!-- reporch-cli-stability:2026-08-23:passed:999:1 -->";
  assert.throws(() => verifyStabilityWindow(issues(), evidence), /outside the active window/);
});
