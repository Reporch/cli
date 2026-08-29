import assert from "node:assert/strict";
import { existsSync, lstatSync, readFileSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { basename, dirname, join, resolve } from "node:path";
import { pathToFileURL } from "node:url";

const DIGEST = /^sha256:[a-f0-9]{64}$/u;
const CREATED = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$/u;
const DEPENDENCY_RELATIONSHIPS = new Set([
  "DEPENDS_ON",
  "DEPENDENCY_OF",
  "BUILD_DEPENDENCY_OF",
  "DEV_DEPENDENCY_OF",
  "OPTIONAL_DEPENDENCY_OF",
  "PROVIDED_DEPENDENCY_OF",
  "RUNTIME_DEPENDENCY_OF",
  "TEST_DEPENDENCY_OF"
]);

function replaceIdentity(value, from, to) {
  if (Array.isArray(value)) return value.map((item) => replaceIdentity(item, from, to));
  if (value && typeof value === "object") {
    return Object.fromEntries(
      Object.entries(value).map(([key, item]) => [key, replaceIdentity(item, from, to)])
    );
  }
  return value === from ? to : value;
}

function sorted(values, field) {
  return [...(values ?? [])].sort((left, right) => String(left[field] ?? "").localeCompare(String(right[field] ?? "")));
}

function packageCoordinate(pkg) {
  const purls = (pkg.externalRefs ?? [])
    .filter(({ referenceCategory, referenceType, referenceLocator }) =>
      referenceCategory === "PACKAGE-MANAGER" &&
      referenceType === "purl" &&
      typeof referenceLocator === "string"
    )
    .map(({ referenceLocator }) => referenceLocator)
    .sort();
  if (purls.length === 0) return null;
  return `${pkg.name ?? ""}\0${pkg.versionInfo ?? ""}\0${purls.join("\0")}`;
}

function canonicalDependencyPackageIds(packages) {
  const groups = new Map();
  for (const pkg of packages) {
    const coordinate = packageCoordinate(pkg);
    if (coordinate === null) continue;
    const ids = groups.get(coordinate) ?? [];
    ids.push(pkg.SPDXID);
    groups.set(coordinate, ids);
  }
  const canonical = new Map();
  for (const ids of groups.values()) {
    if (ids.length < 2) continue;
    ids.sort();
    for (const id of ids) canonical.set(id, ids[0]);
  }
  return canonical;
}

function canonicalizeDependencyRelationships(relationships, packageIds) {
  const values = relationships.map((relationship) => {
    const type = relationship.relationshipType ?? "";
    if (!DEPENDENCY_RELATIONSHIPS.has(type)) return relationship;
    return {
      ...relationship,
      spdxElementId: packageIds.get(relationship.spdxElementId) ?? relationship.spdxElementId,
      relatedSpdxElement:
        packageIds.get(relationship.relatedSpdxElement) ?? relationship.relatedSpdxElement
    };
  });
  const unique = new Map();
  for (const relationship of values) unique.set(JSON.stringify(relationship), relationship);
  return [...unique.values()];
}

export function normalizeToolchainSbom(value, sourceIdentity, architecture, created) {
  assert.match(sourceIdentity, DIGEST);
  assert.ok(["amd64", "arm64"].includes(architecture));
  assert.match(created, CREATED);
  assert.equal(value.spdxVersion, "SPDX-2.3");
  assert.equal(value.dataLicense, "CC0-1.0");
  assert.equal(value.SPDXID, "SPDXRef-DOCUMENT");
  assert.ok(value.creationInfo?.creators?.includes("Tool: syft-1.51.0"), "SBOM must come from pinned Syft");
  assert.ok(Array.isArray(value.packages) && value.packages.length > 0, "SBOM package inventory is empty");
  const packageIds = new Set();
  let declaredLicenses = 0;
  for (const pkg of value.packages) {
    assert.match(pkg.SPDXID ?? "", /^SPDXRef-[A-Za-z0-9.-]+$/u);
    assert.ok(!packageIds.has(pkg.SPDXID), `duplicate SPDX package ID: ${pkg.SPDXID}`);
    packageIds.add(pkg.SPDXID);
    assert.equal(typeof pkg.licenseDeclared, "string");
    assert.equal(typeof pkg.licenseConcluded, "string");
    if (pkg.licenseDeclared !== "NOASSERTION") declaredLicenses += 1;
  }
  assert.ok(declaredLicenses > 0, "SBOM contains no declared package licenses");
  const describes = (value.relationships ?? []).filter(
    ({ spdxElementId, relationshipType }) =>
      spdxElementId === "SPDXRef-DOCUMENT" && relationshipType === "DESCRIBES"
  );
  assert.equal(describes.length, 1, "SBOM must describe exactly one OCI root");
  const originalRoot = describes[0].relatedSpdxElement;
  const root = `SPDXRef-ToolchainRoot-${architecture}`;
  const normalized = replaceIdentity(structuredClone(value), originalRoot, root);
  normalized.name = `Reporch Toolchain Source ${sourceIdentity} ${architecture}`;
  normalized.documentNamespace = `https://reporch.com/spdx/toolchain-source/${sourceIdentity.slice("sha256:".length)}/${architecture}`;
  normalized.creationInfo.created = created;
  normalized.creationInfo.creators = ["Organization: Anchore, Inc", "Tool: syft-1.51.0", "Tool: reporch-sbom-normalizer-1.0.1-rc.8"];
  normalized.documentComment = `Exact package and license inventory for ${sourceIdentity} (${architecture}); coordinate-equivalent dependency endpoints are canonicalized before deterministic VM filesystem conversion.`;
  const rootPackage = normalized.packages.find(({ SPDXID }) => SPDXID === root);
  assert.ok(rootPackage, "SBOM described root is not present in the package inventory");
  rootPackage.name = "reporch-toolchain-source";
  rootPackage.downloadLocation = "NOASSERTION";
  rootPackage.externalRefs = [
    {
      referenceCategory: "PACKAGE-MANAGER",
      referenceType: "purl",
      referenceLocator: `pkg:oci/reporch-toolchain-source@${sourceIdentity}?arch=${architecture}`
    }
  ];
  normalized.packages = sorted(normalized.packages, "SPDXID");
  normalized.files = sorted(normalized.files, "SPDXID");
  const canonicalPackageIds = canonicalDependencyPackageIds(normalized.packages);
  normalized.relationships = canonicalizeDependencyRelationships(
    normalized.relationships ?? [],
    canonicalPackageIds
  ).sort((left, right) =>
    `${left.spdxElementId}\0${left.relationshipType}\0${left.relatedSpdxElement}`.localeCompare(
      `${right.spdxElementId}\0${right.relationshipType}\0${right.relatedSpdxElement}`
    )
  );
  const serialized = `${JSON.stringify(normalized, null, 2)}\n`;
  assert.ok(!serialized.includes(value.name), "normalized SBOM retained a host-specific source name");
  return { serialized, packages: normalized.packages.length, declaredLicenses };
}

export function normalizeToolchainSbomFile(rawArgument, sourceIdentity, architecture, created, outputArgument) {
  const raw = resolve(rawArgument);
  const output = resolve(outputArgument);
  const stat = lstatSync(raw);
  assert.ok(stat.isFile() && !stat.isSymbolicLink() && stat.size > 0 && stat.size <= 32 * 1024 * 1024);
  assert.ok(!existsSync(output), `normalized SBOM output already exists: ${output}`);
  assert.notEqual(raw, output);
  const result = normalizeToolchainSbom(JSON.parse(readFileSync(raw)), sourceIdentity, architecture, created);
  const temporary = join(dirname(output), `.toolchain-sbom-${process.pid}-${basename(output)}.tmp`);
  try {
    writeFileSync(temporary, result.serialized, { flag: "wx", mode: 0o444 });
    renameSync(temporary, output);
  } catch (error) {
    rmSync(temporary, { force: true });
    throw error;
  }
  return result;
}

function main() {
  const [raw, identity, architecture, created, output, ...extra] = process.argv.slice(2);
  assert.ok(raw && identity && architecture && created && output && extra.length === 0, "usage: node scripts/normalize-toolchain-sbom.mjs <raw-spdx> <source-identity> <architecture> <created> <new-output>");
  const result = normalizeToolchainSbomFile(raw, identity, architecture, created, output);
  process.stdout.write(`${JSON.stringify({ schema: "reporch.toolchain-sbom-normalization.v1", ...result, serialized: undefined })}\n`);
}

const invoked = process.argv[1] ? pathToFileURL(resolve(process.argv[1])).href : null;
if (invoked === import.meta.url) main();
