import assert from "node:assert/strict";
import { createHash, generateKeyPairSync, randomUUID, sign } from "node:crypto";
import test from "node:test";

import { startFixture } from "./qualify-installed-auth.mjs";

const { privateKey, publicKey } = generateKeyPairSync("ec", { namedCurve: "P-256" });
const jwk = publicKey.export({ format: "jwk" });

function proof(method, target, accessToken) {
  const targetUrl = new URL(target);
  targetUrl.search = "";
  targetUrl.hash = "";
  const header = Buffer.from(JSON.stringify({ alg: "ES256", typ: "dpop+jwt", jwk })).toString("base64url");
  const claims = {
    htm: method,
    htu: targetUrl.toString(),
    iat: Math.floor(Date.now() / 1000),
    jti: randomUUID(),
  };
  if (accessToken) {
    claims.ath = createHash("sha256").update(accessToken).digest("base64url");
  }
  const payload = Buffer.from(JSON.stringify(claims)).toString("base64url");
  const signature = sign("sha256", Buffer.from(`${header}.${payload}`), {
    key: privateKey,
    dsaEncoding: "ieee-p1363",
  }).toString("base64url");
  return `${header}.${payload}.${signature}`;
}

test("installed auth fixture enforces the complete OAuth and DPoP sequence", async () => {
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
      headers: {
        "content-type": "application/x-www-form-urlencoded",
        dpop: proof("POST", metadata.device_authorization_endpoint),
      },
      body: new URLSearchParams({
        client_id: "reporch-studio-cli-v1",
        scope: "openid offline_access profile studio:entitlements",
      }),
    });
    assert.equal(device.status, 200);
    const prompt = await device.json();

    const token = await fetch(metadata.token_endpoint, {
      method: "POST",
      headers: {
        "content-type": "application/x-www-form-urlencoded",
        dpop: proof("POST", metadata.token_endpoint),
      },
      body: new URLSearchParams({
        grant_type: "urn:ietf:params:oauth:grant-type:device_code",
        device_code: prompt.device_code,
        client_id: "reporch-studio-cli-v1",
      }),
    });
    assert.equal(token.status, 200);
    const credentials = await token.json();

    const refresh = await fetch(metadata.token_endpoint, {
      method: "POST",
      headers: {
        "content-type": "application/x-www-form-urlencoded",
        dpop: proof("POST", metadata.token_endpoint),
      },
      body: new URLSearchParams({
        grant_type: "refresh_token",
        refresh_token: credentials.refresh_token,
        client_id: "reporch-studio-cli-v1",
      }),
    });
    assert.equal(refresh.status, 200);
    const refreshed = await refresh.json();
    assert.notEqual(refreshed.refresh_token, credentials.refresh_token);

    const projects = await fetch(`${fixture.apiUrl}projects?limit=100`, {
      headers: {
        authorization: `DPoP ${refreshed.access_token}`,
        dpop: proof("GET", `${fixture.apiUrl}projects?limit=100`, refreshed.access_token),
      },
    });
    assert.equal(projects.status, 200);
    assert.deepEqual(await projects.json(), { items: [], next_cursor: null });

    const sessionsUrl = `${fixture.apiUrl}auth/device-sessions`;
    const sessions = await fetch(sessionsUrl, {
      headers: {
        authorization: `DPoP ${refreshed.access_token}`,
        dpop: proof("GET", sessionsUrl, refreshed.access_token),
      },
    });
    assert.equal(sessions.status, 200);
    assert.equal((await sessions.json()).items.length, 1);
    const revokeUrl = `${sessionsUrl}/019f8fc9-cff3-7421-8cf8-0661a7a484dd`;
    const revokeDevice = await fetch(revokeUrl, {
      method: "DELETE",
      headers: {
        authorization: `DPoP ${refreshed.access_token}`,
        dpop: proof("DELETE", revokeUrl, refreshed.access_token),
      },
    });
    assert.equal(revokeDevice.status, 200);

    const revocation = await fetch(metadata.revocation_endpoint, {
      method: "POST",
      headers: {
        "content-type": "application/x-www-form-urlencoded",
        dpop: proof("POST", metadata.revocation_endpoint),
      },
      body: new URLSearchParams({ token: refreshed.refresh_token }),
    });
    assert.equal(revocation.status, 200);
    assert.deepEqual(fixture.state, {
      deviceAuthorizations: 1,
      tokenGrants: 2,
      projectLists: 1,
      deviceSessionLists: 1,
      deviceSessionRevocations: 1,
      revocations: 1,
      browserVisits: 0,
    });
  } finally {
    await fixture.close();
  }
});
