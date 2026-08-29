import assert from "node:assert/strict";
import test from "node:test";

import { normalizeToolchainSbom } from "./normalize-toolchain-sbom.mjs";

function fixture() {
  return {
    spdxVersion: "SPDX-2.3",
    dataLicense: "CC0-1.0",
    SPDXID: "SPDXRef-DOCUMENT",
    name: "/private/random/source",
    documentNamespace: "https://anchore.invalid/random-uuid",
    creationInfo: { creators: ["Tool: syft-1.51.0"], created: "2020-01-01T00:00:00Z" },
    packages: [
      { SPDXID: "SPDXRef-RandomRoot", name: "/private/random/source", licenseDeclared: "NOASSERTION", licenseConcluded: "NOASSERTION" },
      { SPDXID: "SPDXRef-Package-b", licenseDeclared: "MIT", licenseConcluded: "NOASSERTION" },
      { SPDXID: "SPDXRef-Package-a", licenseDeclared: "Apache-2.0", licenseConcluded: "NOASSERTION" }
    ],
    relationships: [
      { spdxElementId: "SPDXRef-DOCUMENT", relationshipType: "DESCRIBES", relatedSpdxElement: "SPDXRef-RandomRoot" },
      { spdxElementId: "SPDXRef-RandomRoot", relationshipType: "CONTAINS", relatedSpdxElement: "SPDXRef-Package-a" }
    ]
  };
}

test("Syft SPDX is normalized without losing package licenses", () => {
  const identity = `sha256:${"a".repeat(64)}`;
  const first = normalizeToolchainSbom(fixture(), identity, "amd64", "2026-08-29T00:00:00Z");
  const second = normalizeToolchainSbom(fixture(), identity, "amd64", "2026-08-29T00:00:00Z");
  assert.equal(first.serialized, second.serialized);
  assert.equal(first.packages, 3);
  assert.equal(first.declaredLicenses, 2);
  assert.ok(!first.serialized.includes("/private/random/source"));
  assert.match(first.serialized, /SPDXRef-ToolchainRoot-amd64/u);
  assert.ok(first.serialized.indexOf("SPDXRef-Package-a") < first.serialized.indexOf("SPDXRef-Package-b"));
});

test("empty or license-free inventories fail closed", () => {
  const identity = `sha256:${"b".repeat(64)}`;
  const empty = fixture();
  empty.packages = [];
  assert.throws(() => normalizeToolchainSbom(empty, identity, "arm64", "2026-08-29T00:00:00Z"));
  const unknown = fixture();
  for (const pkg of unknown.packages) pkg.licenseDeclared = "NOASSERTION";
  assert.throws(() => normalizeToolchainSbom(unknown, identity, "arm64", "2026-08-29T00:00:00Z"));
});

test("dependency edges use a stable package ID for duplicate purls", () => {
  const identity = `sha256:${"c".repeat(64)}`;
  const first = fixture();
  const duplicatePackages = [
    {
      SPDXID: "SPDXRef-Package-wheel-a",
      name: "wheel",
      versionInfo: "0.45.1",
      licenseDeclared: "MIT",
      licenseConcluded: "NOASSERTION",
      sourceInfo: "installed package /site-packages/wheel",
      externalRefs: [{ referenceCategory: "PACKAGE-MANAGER", referenceType: "purl", referenceLocator: "pkg:pypi/wheel@0.45.1" }]
    },
    {
      SPDXID: "SPDXRef-Package-wheel-b",
      name: "wheel",
      versionInfo: "0.45.1",
      licenseDeclared: "MIT",
      licenseConcluded: "NOASSERTION",
      sourceInfo: "vendored package /site-packages/setuptools/_vendor/wheel",
      externalRefs: [{ referenceCategory: "PACKAGE-MANAGER", referenceType: "purl", referenceLocator: "pkg:pypi/wheel@0.45.1" }]
    },
    {
      SPDXID: "SPDXRef-Package-setuptools",
      name: "setuptools",
      versionInfo: "80.0.0",
      licenseDeclared: "MIT",
      licenseConcluded: "NOASSERTION",
      externalRefs: [{ referenceCategory: "PACKAGE-MANAGER", referenceType: "purl", referenceLocator: "pkg:pypi/setuptools@80.0.0" }]
    }
  ];
  first.packages.push(...duplicatePackages);
  first.relationships.push({
    spdxElementId: "SPDXRef-Package-wheel-a",
    relationshipType: "DEPENDENCY_OF",
    relatedSpdxElement: "SPDXRef-Package-setuptools"
  });
  const second = structuredClone(first);
  second.relationships.at(-1).spdxElementId = "SPDXRef-Package-wheel-b";

  const normalizedFirst = normalizeToolchainSbom(first, identity, "amd64", "2026-08-29T00:00:00Z");
  const normalizedSecond = normalizeToolchainSbom(second, identity, "amd64", "2026-08-29T00:00:00Z");
  assert.equal(normalizedFirst.serialized, normalizedSecond.serialized);
  assert.match(
    normalizedFirst.serialized,
    /"spdxElementId": "SPDXRef-Package-wheel-a"[\s\S]*"relationshipType": "DEPENDENCY_OF"/u
  );
  assert.ok(!normalizedFirst.serialized.includes('"spdxElementId": "SPDXRef-Package-wheel-b",\n      "relationshipType": "DEPENDENCY_OF"'));
});
