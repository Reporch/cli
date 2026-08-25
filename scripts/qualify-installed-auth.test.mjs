import assert from "node:assert/strict";
import test from "node:test";

import { startFixture } from "./qualify-installed-auth.mjs";

test("installed auth fixture enforces the complete OAuth and Bearer sequence", async () => {
  const fixture = await startFixture();
  try {
    const discovery = await fetch(`${fixture.issuer}/.well-known/openid-configuration`);
    assert.equal(discovery.status, 200);
    const metadata = await discovery.json();
    assert.equal(metadata.issuer, fixture.issuer);

    const denied = await fetch(`${fixture.apiUrl}projects`);
    assert.equal(denied.status, 401);

    const device = await fetch(metadata.device_authorization_endpoint, {
      method: "POST",
      headers: { "content-type": "application/x-www-form-urlencoded" },
      body: new URLSearchParams({
        client_id: "reporch-studio-cli",
        scope: "openid offline_access profile studio:entitlements",
      }),
    });
    assert.equal(device.status, 200);
    const prompt = await device.json();

    const token = await fetch(metadata.token_endpoint, {
      method: "POST",
      headers: { "content-type": "application/x-www-form-urlencoded" },
      body: new URLSearchParams({
        grant_type: "urn:ietf:params:oauth:grant-type:device_code",
        device_code: prompt.device_code,
        client_id: "reporch-studio-cli",
      }),
    });
    assert.equal(token.status, 200);
    const credentials = await token.json();

    const projects = await fetch(`${fixture.apiUrl}projects?limit=100`, {
      headers: { authorization: `Bearer ${credentials.access_token}` },
    });
    assert.equal(projects.status, 200);
    assert.deepEqual(await projects.json(), { items: [], next_cursor: null });

    const revocation = await fetch(metadata.revocation_endpoint, {
      method: "POST",
      headers: { "content-type": "application/x-www-form-urlencoded" },
      body: new URLSearchParams({ token: credentials.refresh_token }),
    });
    assert.equal(revocation.status, 200);
    assert.deepEqual(fixture.state, {
      deviceAuthorizations: 1,
      tokenGrants: 1,
      projectLists: 1,
      revocations: 1,
      browserVisits: 0,
    });
  } finally {
    await fixture.close();
  }
});
