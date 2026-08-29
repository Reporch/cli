import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readFileSync, renameSync, writeFileSync } from "node:fs";
import { gzipSync } from "node:zlib";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";

const artifactPath = "artifacts/studio-openapi.json.gz.b64";
const checksumPath = "artifacts/studio-openapi.sha256";

export function updateStudioOpenApi(sourcePath) {
  const source = readFileSync(sourcePath);
  assert.ok(source.length > 0 && source.length <= 32 * 1024 * 1024, "invalid OpenAPI size");
  const document = JSON.parse(source.toString("utf8"));
  assert.equal(document.openapi, "3.1.0", "unsupported Studio OpenAPI version");
  assert.ok(document.paths?.["/api/v1/capabilities"]?.get, "missing capabilities contract");
  assert.ok(document.paths?.["/api/v1/runtime-previews"]?.post, "missing runtime preview contract");

  const canonical = Buffer.from(JSON.stringify(document));
  const encoded = `${gzipSync(canonical, { level: 9, mtime: 0 }).toString("base64")}\n`;
  const checksum = createHash("sha256").update(canonical).digest("hex");
  const artifactTemporary = `${artifactPath}.tmp`;
  const checksumTemporary = `${checksumPath}.tmp`;
  writeFileSync(artifactTemporary, encoded, { mode: 0o644 });
  writeFileSync(checksumTemporary, `${checksum}  studio-openapi.json\n`, { mode: 0o644 });
  renameSync(artifactTemporary, artifactPath);
  renameSync(checksumTemporary, checksumPath);
  return checksum;
}

function main() {
  const [sourcePath, ...extra] = process.argv.slice(2);
  if (!sourcePath || extra.length > 0) {
    throw new Error("usage: node scripts/update-studio-openapi.mjs <openapi.json>");
  }
  console.log(`Studio OpenAPI lock updated: ${updateStudioOpenApi(resolve(sourcePath))}`);
}

const invoked = process.argv[1] ? pathToFileURL(resolve(process.argv[1])).href : null;
if (invoked === import.meta.url) main();
