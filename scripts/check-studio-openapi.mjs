import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { gunzipSync } from "node:zlib";

const artifactPath = "artifacts/studio-openapi.json.gz.b64";
const checksumPath = "artifacts/studio-openapi.sha256";

const encoded = readFileSync(artifactPath, "utf8").trim();
const openApiBytes = gunzipSync(Buffer.from(encoded, "base64"));
const expectedChecksum = readFileSync(checksumPath, "utf8").trim().split(/\s+/u)[0];
const actualChecksum = createHash("sha256").update(openApiBytes).digest("hex");

assert.match(expectedChecksum, /^[a-f0-9]{64}$/u, "invalid Studio OpenAPI checksum");
assert.equal(actualChecksum, expectedChecksum, "Studio OpenAPI artifact checksum drifted");

const document = JSON.parse(openApiBytes.toString("utf8"));
const requiredPaths = [
  "/api/v1/projects/{project_id}/reviews/{review_id}/pool-request",
  "/api/v1/review-pool/inbox",
  "/api/v1/review-pool/{request_id}",
  "/api/v1/review-pool/{request_id}/claim",
  "/api/v1/review-pool/{request_id}/cancel",
  "/api/v1/review-pool/{request_id}/decision"
];

for (const path of requiredPaths) {
  assert.ok(document.paths?.[path], `Studio OpenAPI is missing ${path}`);
}

const schemas = document.components?.schemas;
for (const schema of [
  "ReviewPoolRequestResponseV1",
  "ReviewPoolPageV1",
  "ReviewPoolStatusV1",
  "ReviewApprovalSourceV1"
]) {
  assert.ok(schemas?.[schema], `Studio OpenAPI is missing schema ${schema}`);
}

const decision = schemas?.ReviewDecisionResponse;
assert.ok(decision?.properties?.approval_source, "review decision lacks approval_source");
assert.ok(decision?.properties?.pool_assignment_id, "review decision lacks pool_assignment_id");

console.log(`Studio OpenAPI lock passed: ${actualChecksum}`);
