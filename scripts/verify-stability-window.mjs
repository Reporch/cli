import { readFileSync } from "node:fs";
import { pathToFileURL } from "node:url";

const WINDOW_MILLIS = 30 * 24 * 60 * 60 * 1000;
const TITLE_PATTERN = /^Reporch CLI 1\.0\.0-rc\.[0-9A-Za-z.-]+ — 30-day stability window$/;
const MARKER_PATTERN = /<!-- reporch-cli-stability:(\d{4}-\d{2}-\d{2}):passed:(\d+):(\d+) -->/;
const TRUSTED_AUTHORS = new Set(["github-actions", "github-actions[bot]"]);

function utcDay(value) {
  const millis = Date.parse(value);
  if (!Number.isFinite(millis)) throw new Error("stability timestamp is invalid");
  return Math.floor(millis / (24 * 60 * 60 * 1000));
}

export function verifyStabilityWindow(issues, commentsEnvelope) {
  const candidates = issues
    .filter((issue) => TITLE_PATTERN.test(String(issue.title)))
    .sort((left, right) => Date.parse(right.createdAt) - Date.parse(left.createdAt));
  const latest = candidates[0];
  if (!latest?.closedAt) throw new Error("latest RC stability issue is not closed");
  const openedAt = Date.parse(latest.createdAt);
  const closedAt = Date.parse(latest.closedAt);
  if (!Number.isFinite(openedAt) || !Number.isFinite(closedAt) || closedAt - openedAt < WINDOW_MILLIS) {
    throw new Error("RC stability issue was closed before 30 full days elapsed");
  }

  const openedDay = utcDay(latest.createdAt);
  const closedDay = utcDay(latest.closedAt);
  const comments = Array.isArray(commentsEnvelope?.comments) ? commentsEnvelope.comments : [];
  const passedDates = new Set();
  for (const { author, body } of comments) {
    if (!TRUSTED_AUTHORS.has(author?.login)) continue;
    const match = String(body).match(MARKER_PATTERN);
    if (!match) continue;
    const markerDay = utcDay(`${match[1]}T00:00:00Z`);
    if (markerDay < openedDay || markerDay > closedDay) {
      throw new Error("daily stability evidence is outside the active window");
    }
    if (Number(match[2]) <= 0 || Number(match[3]) <= 0) {
      throw new Error("daily stability evidence has an invalid run identity");
    }
    passedDates.add(match[1]);
  }
  if (passedDates.size < 30) {
    throw new Error("fewer than 30 distinct passing daily monitor records exist");
  }
  return {
    schema: "reporch.cli-stability-gate.v1",
    issue_number: latest.number,
    opened_at: latest.createdAt,
    closed_at: latest.closedAt,
    passed_day_count: passedDates.size,
    accepted: true,
  };
}

if (import.meta.url === pathToFileURL(process.argv[1] ?? "").href) {
  if (process.argv.length !== 4) {
    console.error("usage: verify-stability-window.mjs <issues.json> <comments.json>");
    process.exit(2);
  }
  try {
    const issues = JSON.parse(readFileSync(process.argv[2], "utf8"));
    const comments = JSON.parse(readFileSync(process.argv[3], "utf8"));
    console.log(JSON.stringify(verifyStabilityWindow(issues, comments)));
  } catch (error) {
    console.error(error instanceof Error ? error.message : "stability verification failed");
    process.exit(1);
  }
}
