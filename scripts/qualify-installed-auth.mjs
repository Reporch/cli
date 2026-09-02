import { createHash, createPublicKey, randomBytes, verify } from "node:crypto";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { createServer } from "node:http";
import { spawn } from "node:child_process";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const MAX_BODY_BYTES = 64 * 1024;
const MAX_COMMAND_OUTPUT_BYTES = 1024 * 1024;
const COMMAND_TIMEOUT_MS = 90_000;

function requiredArgument(name) {
  const index = process.argv.indexOf(name);
  if (index === -1 || !process.argv[index + 1]) {
    throw new Error(`missing ${name}`);
  }
  return process.argv[index + 1];
}

function sendJson(response, status, value) {
  const body = Buffer.from(JSON.stringify(value));
  response.writeHead(status, {
    "content-type": "application/json",
    "content-length": String(body.length),
    "cache-control": "no-store",
    "x-content-type-options": "nosniff",
  });
  response.end(body);
}

async function readForm(request) {
  const chunks = [];
  let size = 0;
  for await (const chunk of request) {
    size += chunk.length;
    if (size > MAX_BODY_BYTES) {
      throw new Error("request body exceeded the qualification bound");
    }
    chunks.push(chunk);
  }
  return new URLSearchParams(Buffer.concat(chunks).toString("utf8"));
}

function randomOpaque(prefix) {
  return `${prefix}-${randomBytes(24).toString("base64url")}`;
}

function encode(value) {
  return Buffer.from(value).toString("base64url");
}

function normalizeHtu(value) {
  const url = new URL(value);
  url.search = "";
  url.hash = "";
  return url.toString();
}

function jwkThumbprint(jwk) {
  const canonical = JSON.stringify({ crv: "P-256", kty: "EC", x: jwk.x, y: jwk.y });
  return encode(createHash("sha256").update(canonical).digest());
}

export async function startFixture() {
  const state = {
    deviceAuthorizations: 0,
    tokenGrants: 0,
    projectLists: 0,
    deviceSessionLists: 0,
    deviceSessionRevocations: 0,
    revocations: 0,
    browserVisits: 0,
  };
  const accessToken = randomOpaque("access");
  const refreshedAccessToken = randomOpaque("access-rotated");
  const refreshToken = randomOpaque("refresh");
  const rotatedRefreshToken = randomOpaque("refresh-rotated");
  const deviceCode = randomOpaque("device");
  const replayIds = new Set();
  let deviceJkt;
  let origin;

  function validateDpop(request, accessTokenForProof) {
    const parts = String(request.headers.dpop ?? "").split(".");
    if (parts.length !== 3) throw new Error("missing DPoP proof");
    const header = JSON.parse(Buffer.from(parts[0], "base64url"));
    const claims = JSON.parse(Buffer.from(parts[1], "base64url"));
    if (header.alg !== "ES256" || String(header.typ).toLowerCase() !== "dpop+jwt") {
      throw new Error("invalid DPoP header");
    }
    if (header.jwk?.kty !== "EC" || header.jwk?.crv !== "P-256") {
      throw new Error("invalid DPoP key");
    }
    const publicKey = createPublicKey({ key: header.jwk, format: "jwk" });
    if (!verify(
      "sha256",
      Buffer.from(`${parts[0]}.${parts[1]}`),
      { key: publicKey, dsaEncoding: "ieee-p1363" },
      Buffer.from(parts[2], "base64url"),
    )) throw new Error("invalid DPoP signature");
    const target = normalizeHtu(new URL(request.url, origin).toString());
    if (claims.htm !== request.method || normalizeHtu(claims.htu) !== target) {
      throw new Error("invalid DPoP target");
    }
    if (!Number.isInteger(claims.iat) || Math.abs(Date.now() / 1000 - claims.iat) > 300) {
      throw new Error("invalid DPoP time");
    }
    if (typeof claims.jti !== "string" || replayIds.has(claims.jti)) {
      throw new Error("replayed DPoP proof");
    }
    replayIds.add(claims.jti);
    const jkt = jwkThumbprint(header.jwk);
    if (deviceJkt && deviceJkt !== jkt) throw new Error("wrong DPoP key");
    if (accessTokenForProof) {
      const expectedAth = encode(createHash("sha256").update(accessTokenForProof).digest());
      if (claims.ath !== expectedAth) throw new Error("invalid DPoP token hash");
    } else if ("ath" in claims) {
      throw new Error("unexpected DPoP token hash");
    }
    return jkt;
  }

  const server = createServer(async (request, response) => {
    try {
      const url = new URL(request.url, origin ?? "http://127.0.0.1");
      const issuer = `${origin}/oauth`;
      if (
        request.method === "GET" &&
        url.pathname === "/oauth/.well-known/openid-configuration"
      ) {
        sendJson(response, 200, {
          issuer,
          authorization_endpoint: `${issuer}/authorize`,
          token_endpoint: `${issuer}/token`,
          device_authorization_endpoint: `${issuer}/device-authorization`,
          revocation_endpoint: `${issuer}/revoke`,
          dpop_signing_alg_values_supported: ["ES256"],
        });
        return;
      }
      if (request.method === "POST" && url.pathname === "/oauth/device-authorization") {
        const form = await readForm(request);
        if (
          form.get("client_id") !== "reporch-studio-cli-v1" ||
          !form.get("scope")?.split(/\s+/).includes("studio:entitlements")
        ) {
          sendJson(response, 400, { error: "invalid_request" });
          return;
        }
        deviceJkt = validateDpop(request);
        state.deviceAuthorizations += 1;
        sendJson(response, 200, {
          device_code: deviceCode,
          user_code: "RC-E2E-1",
          verification_uri: `${issuer}/device`,
          expires_in: 300,
          interval: 1,
        });
        return;
      }
      if (request.method === "POST" && url.pathname === "/oauth/token") {
        const form = await readForm(request);
        if (form.get("client_id") !== "reporch-studio-cli-v1") {
          sendJson(response, 400, { error: "invalid_grant" });
          return;
        }
        validateDpop(request);
        state.tokenGrants += 1;
        if (
          form.get("grant_type") === "urn:ietf:params:oauth:grant-type:device_code" &&
          form.get("device_code") === deviceCode
        ) {
          sendJson(response, 200, {
            access_token: accessToken,
            refresh_token: refreshToken,
            token_type: "DPoP",
            expires_in: 1,
            scope: "openid offline_access profile studio:entitlements",
          });
          return;
        }
        if (
          form.get("grant_type") === "refresh_token" &&
          form.get("refresh_token") === refreshToken
        ) {
          sendJson(response, 200, {
            access_token: refreshedAccessToken,
            refresh_token: rotatedRefreshToken,
            token_type: "DPoP",
            expires_in: 3600,
            scope: "openid offline_access profile studio:entitlements",
          });
          return;
        }
        sendJson(response, 400, { error: "invalid_grant" });
        return;
      }
      if (request.method === "POST" && url.pathname === "/oauth/revoke") {
        const form = await readForm(request);
        if (form.get("token") !== rotatedRefreshToken) {
          sendJson(response, 400, { error: "invalid_token" });
          return;
        }
        validateDpop(request);
        state.revocations += 1;
        response.writeHead(200, { "cache-control": "no-store" });
        response.end();
        return;
      }
      if (request.method === "GET" && url.pathname === "/oauth/device") {
        state.browserVisits += 1;
        response.writeHead(200, {
          "content-type": "text/plain; charset=utf-8",
          "cache-control": "no-store",
        });
        response.end("Installed-artifact OAuth qualification");
        return;
      }
      if (request.method === "GET" && url.pathname === "/api/v1/projects") {
        if (request.headers.authorization !== `DPoP ${refreshedAccessToken}`) {
          sendJson(response, 401, {
            error_code: "auth.required",
            message: "DPoP token required",
            retryable: false,
          });
          return;
        }
        validateDpop(request, refreshedAccessToken);
        state.projectLists += 1;
        sendJson(response, 200, { items: [], next_cursor: null });
        return;
      }
      if (request.method === "GET" && url.pathname === "/api/v1/auth/device-sessions") {
        if (request.headers.authorization !== `DPoP ${refreshedAccessToken}`) {
          sendJson(response, 401, { error_code: "auth.required", message: "DPoP token required" });
          return;
        }
        validateDpop(request, refreshedAccessToken);
        state.deviceSessionLists += 1;
        sendJson(response, 200, {
          schema: "reporch.device-session-page.v1",
          items: [{
            id: "019f8fc9-cff3-7421-8cf8-0661a7a484dd",
            client_id: "reporch-studio-cli-v1",
            created_at: "2026-09-01T00:00:00Z",
            last_used_at: "2026-09-02T00:00:00Z",
            expires_at: "2026-12-01T00:00:00Z",
            revoked_at: null,
          }],
        });
        return;
      }
      if (
        request.method === "DELETE" &&
        url.pathname === "/api/v1/auth/device-sessions/019f8fc9-cff3-7421-8cf8-0661a7a484dd"
      ) {
        if (request.headers.authorization !== `DPoP ${refreshedAccessToken}`) {
          sendJson(response, 401, { error_code: "auth.required", message: "DPoP token required" });
          return;
        }
        validateDpop(request, refreshedAccessToken);
        state.deviceSessionRevocations += 1;
        sendJson(response, 200, {
          schema: "reporch.device-session-revocation.v1",
          id: "019f8fc9-cff3-7421-8cf8-0661a7a484dd",
          revoked: true,
        });
        return;
      }
      sendJson(response, 404, { error: "not_found" });
    } catch {
      sendJson(response, 400, { error: "invalid_request" });
    }
  });

  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  const address = server.address();
  if (!address || typeof address === "string") {
    throw new Error("qualification fixture did not bind a TCP port");
  }
  origin = `http://127.0.0.1:${address.port}`;
  return {
    issuer: `${origin}/oauth`,
    apiUrl: `${origin}/api/v1/`,
    state,
    close: () =>
      new Promise((resolve, reject) => {
        server.closeAllConnections();
        server.close((error) => (error ? reject(error) : resolve()));
      }),
  };
}

async function runCommand(binary, args, environment) {
  const child = spawn(binary, args, {
    env: environment,
    stdio: ["ignore", "pipe", "pipe"],
    windowsHide: true,
  });
  const stdout = [];
  const stderr = [];
  let stdoutBytes = 0;
  let stderrBytes = 0;
  const collect = (target, chunk, totalName) => {
    if (totalName === "stdout") {
      stdoutBytes += chunk.length;
      if (stdoutBytes > MAX_COMMAND_OUTPUT_BYTES) child.kill();
    } else {
      stderrBytes += chunk.length;
      if (stderrBytes > MAX_COMMAND_OUTPUT_BYTES) child.kill();
    }
    target.push(chunk);
  };
  child.stdout.on("data", (chunk) => collect(stdout, chunk, "stdout"));
  child.stderr.on("data", (chunk) => collect(stderr, chunk, "stderr"));
  let forceKillTimer;
  const timer = setTimeout(() => {
    child.kill();
    forceKillTimer = setTimeout(() => child.kill("SIGKILL"), 5_000);
  }, COMMAND_TIMEOUT_MS);
  const result = await new Promise((resolve, reject) => {
    child.once("error", reject);
    child.once("close", (code, signal) => resolve({ code, signal }));
  });
  clearTimeout(timer);
  if (forceKillTimer) clearTimeout(forceKillTimer);
  if (
    result.code !== 0 ||
    stdoutBytes > MAX_COMMAND_OUTPUT_BYTES ||
    stderrBytes > MAX_COMMAND_OUTPUT_BYTES
  ) {
    const diagnostic = Buffer.concat(stderr).toString("utf8").slice(0, 2000);
    throw new Error(
      `installed command failed (${args.join(" ")}): code=${result.code} signal=${result.signal ?? "none"} ${diagnostic}`,
    );
  }
  const output = Buffer.concat(stdout).toString("utf8");
  try {
    return JSON.parse(output);
  } catch {
    throw new Error(`installed command returned invalid JSON (${args.join(" ")})`);
  }
}

function assertEnvelope(value, command) {
  if (value?.schema !== "reporch.cli-result.v1" || value.command !== command) {
    throw new Error(`${command} returned an incompatible CLI envelope`);
  }
  return value.data;
}

async function main() {
  const binary = requiredArgument("--binary");
  const evidencePath = requiredArgument("--evidence");
  const configHome = await mkdtemp(join(tmpdir(), "reporch-installed-auth-e2e-"));
  const fixture = await startFixture().catch(async (error) => {
    await rm(configHome, { recursive: true, force: false });
    throw error;
  });
  const environment = { ...process.env };
  for (const key of Object.keys(environment)) {
    if (key.startsWith("REPORCH_")) delete environment[key];
  }
  Object.assign(environment, {
    REPORCH_STUDIO_OIDC_ISSUER: fixture.issuer,
    REPORCH_STUDIO_CLI_CLIENT_ID: "reporch-studio-cli-v1",
    REPORCH_STUDIO_API_URL: fixture.apiUrl,
    REPORCH_STUDIO_ALLOW_INSECURE_HTTP: "true",
    REPORCH_CONFIG_HOME: configHome,
  });
  for (const key of [
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "NODE_AUTH_TOKEN",
    "NPM_TOKEN",
    "ACTIONS_ID_TOKEN_REQUEST_TOKEN",
    "ACTIONS_ID_TOKEN_REQUEST_URL",
  ]) {
    delete environment[key];
  }
  let loggedIn = false;
  try {
    const login = assertEnvelope(
      await runCommand(binary, ["--format", "json", "auth", "login"], environment),
      "auth login",
    );
    if (!login.authenticated || !login.refresh_available || login.issuer !== fixture.issuer) {
      throw new Error("installed auth login did not persist the expected session");
    }
    loggedIn = true;

    const authenticatedStatus = assertEnvelope(
      await runCommand(binary, ["--format", "json", "auth", "status"], environment),
      "auth status",
    );
    if (!authenticatedStatus.authenticated || !authenticatedStatus.refresh_available) {
      throw new Error("installed auth status did not restore the protected native credential");
    }
    const measuredStatus = assertEnvelope(
      await runCommand(binary, [
        "--format", "json", "auth", "status", "--qualification-iterations", "100",
      ], environment),
      "auth status",
    );
    if (!measuredStatus.authenticated || measuredStatus.qualification?.iterations !== 100) {
      throw new Error("measured auth status lost the session or qualification evidence");
    }
    const authStatusP95Ms = measuredStatus.qualification.p95_ms;
    if (authStatusP95Ms > 100) {
      throw new Error(`auth status p95 exceeded 100ms: ${authStatusP95Ms.toFixed(2)}ms`);
    }

    // Keep this qualification scoped to commands that intentionally skip the mandatory VM
    // bootstrap. The device-session request below is an authenticated Studio API call with the
    // same DPoP proof validation, without conflating native credential QA with runtime signing.
    const devices = assertEnvelope(
      await runCommand(binary, ["--format", "json", "auth", "devices", "list"], environment),
      "auth devices list",
    );
    if (devices.items?.length !== 1 || devices.items[0].client_id !== "reporch-studio-cli-v1") {
      throw new Error("authenticated device session list returned an incompatible page");
    }
    const revoked = assertEnvelope(
      await runCommand(binary, [
        "--format", "json", "auth", "devices", "revoke",
        "019f8fc9-cff3-7421-8cf8-0661a7a484dd",
      ], environment),
      "auth devices revoke",
    );
    if (!revoked.revoked) throw new Error("device session revocation did not succeed");

    const logout = assertEnvelope(
      await runCommand(binary, ["--format", "json", "auth", "logout"], environment),
      "auth logout",
    );
    if (!logout.local_removed || !logout.remote_revoked) {
      throw new Error("installed auth logout did not revoke and remove the credential");
    }

    const anonymousStatus = assertEnvelope(
      await runCommand(binary, ["--format", "json", "auth", "status"], environment),
      "auth status",
    );
    if (anonymousStatus.authenticated) {
      throw new Error("installed auth logout left a local credential behind");
    }
    loggedIn = false;
    if (
      fixture.state.deviceAuthorizations !== 1 ||
      fixture.state.tokenGrants !== 2 ||
      fixture.state.projectLists !== 0 ||
      fixture.state.deviceSessionLists !== 1 ||
      fixture.state.deviceSessionRevocations !== 1 ||
      fixture.state.revocations !== 1
    ) {
      throw new Error("installed OAuth fixture observed an incomplete request sequence");
    }

    await writeFile(
      evidencePath,
      `${JSON.stringify(
        {
          schema: "reporch.cli-installed-auth-e2e.v1",
          target_os: process.platform,
          target_arch: process.arch,
          device_authorization: true,
          native_credential_restore: true,
          refresh_rotation: true,
          auth_status_p95_ms: authStatusP95Ms,
          authenticated_studio_request: true,
          device_session_management: true,
          remote_revocation: true,
          local_credential_removal: true,
          browser_open_attempted: true,
          browser_fixture_visits: fixture.state.browserVisits,
          passed: true,
        },
        null,
        2,
      )}\n`,
      { mode: 0o600, flag: "wx" },
    );
  } finally {
    if (loggedIn) {
      await runCommand(binary, ["--format", "json", "auth", "logout"], environment).catch(() => {});
    }
    try {
      await fixture.close();
    } finally {
      await rm(configHome, { recursive: true, force: false });
    }
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  await main();
}
