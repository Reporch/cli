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
