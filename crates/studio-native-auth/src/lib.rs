#![deny(unsafe_code)]

#[cfg(windows)]
#[allow(unsafe_code)]
mod windows_credential_acl;

use std::collections::BTreeMap;
use std::fs;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::time::Duration as StdDuration;

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Duration, Utc};
use keyring::v1::{Entry, Error as KeyringError};
use p256::ecdsa::signature::Signer as _;
use p256::ecdsa::{Signature, SigningKey};
use rand::RngCore;
use reqwest::StatusCode;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::{Instant, sleep, timeout_at};
use url::Url;

const KEYRING_SERVICE: &str = "com.reporch.studio.oauth";
const TOKEN_EXPIRY_SKEW_SECONDS: i64 = 60;
const MAX_CALLBACK_BYTES: usize = 16 * 1024;
const MAX_PROVIDER_RESPONSE_BYTES: usize = 64 * 1024;
const CALLBACK_TIMEOUT_SECONDS: u64 = 5 * 60;
const DEVICE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";
const TOKEN_SCHEMA_V1: &str = "reporch.native-token.v1";
const TOKEN_SCHEMA_V2: &str = "reporch.native-credential.v2";
const CREDENTIAL_FILE_SCHEMA_V2: &str = "reporch.native-credential-file.v2";
const MAX_TOKEN_BYTES: usize = 16 * 1024;
const MAX_CREDENTIAL_FILE_BYTES: u64 = 256 * 1024;
const CREDENTIAL_STORE_TIMEOUT: StdDuration = StdDuration::from_secs(15);

#[derive(Clone)]
pub struct NativeAuthConfig {
    issuer: Url,
    client_id: String,
    scopes: Vec<String>,
    redirect_uri: Option<Url>,
    allow_insecure_http: bool,
    dpop_required: bool,
}

impl NativeAuthConfig {
    pub fn device(
        issuer: &str,
        client_id: &str,
        scopes: Vec<String>,
        allow_insecure_http: bool,
    ) -> Result<Self, NativeAuthError> {
        Self::new(issuer, client_id, scopes, None, allow_insecure_http, false)
    }

    pub fn device_dpop(
        issuer: &str,
        client_id: &str,
        scopes: Vec<String>,
        allow_insecure_http: bool,
    ) -> Result<Self, NativeAuthError> {
        Self::new(issuer, client_id, scopes, None, allow_insecure_http, true)
    }

    pub fn loopback_pkce(
        issuer: &str,
        client_id: &str,
        scopes: Vec<String>,
        redirect_uri: &str,
        allow_insecure_http: bool,
    ) -> Result<Self, NativeAuthError> {
        Self::new(
            issuer,
            client_id,
            scopes,
            Some(redirect_uri),
            allow_insecure_http,
            false,
        )
    }

    fn new(
        issuer: &str,
        client_id: &str,
        scopes: Vec<String>,
        redirect_uri: Option<&str>,
        allow_insecure_http: bool,
        dpop_required: bool,
    ) -> Result<Self, NativeAuthError> {
        let issuer = normalized_issuer(issuer, allow_insecure_http)?;
        let client_id = client_id.trim().to_owned();
        if client_id.is_empty() || client_id.len() > 255 {
            return Err(NativeAuthError::InvalidConfiguration(
                "client ID must contain 1 to 255 bytes".into(),
            ));
        }
        let mut normalized_scopes = scopes
            .into_iter()
            .map(|scope| scope.trim().to_owned())
            .filter(|scope| !scope.is_empty())
            .collect::<Vec<_>>();
        normalized_scopes.sort();
        normalized_scopes.dedup();
        if normalized_scopes.is_empty()
            || normalized_scopes.len() > 16
            || normalized_scopes
                .iter()
                .any(|scope| scope.len() > 128 || scope.chars().any(char::is_whitespace))
        {
            return Err(NativeAuthError::InvalidConfiguration(
                "OAuth scopes are invalid".into(),
            ));
        }
        let redirect_uri = redirect_uri.map(validate_loopback_redirect).transpose()?;
        Ok(Self {
            issuer,
            client_id,
            scopes: normalized_scopes,
            redirect_uri,
            allow_insecure_http,
            dpop_required,
        })
    }

    pub fn issuer(&self) -> &str {
        self.issuer.as_str().trim_end_matches('/')
    }

    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    pub fn scopes(&self) -> &[String] {
        &self.scopes
    }

    /// Read the native session from the OS credential store without contacting
    /// the identity provider. This keeps `auth status` useful while offline.
    pub async fn local_session_status<S: TokenStore + ?Sized>(
        &self,
        store: &S,
    ) -> Result<NativeSessionStatus, NativeAuthError> {
        let token = store.load(&self.credential_key()).await?;
        if let Some(token) = token.as_ref() {
            self.validate_stored_token(token)?;
        }
        Ok(self.status_from_token(token.as_ref()))
    }

    /// Remove only the local credential. Remote revocation is attempted by
    /// `NativeAuthClient::logout` when provider discovery is available.
    pub async fn clear_local_session<S: TokenStore + ?Sized>(
        &self,
        store: &S,
    ) -> Result<(), NativeAuthError> {
        store.delete(&self.credential_key()).await
    }

    /// Return a non-secret fingerprint for binding local consent to the
    /// currently stored credential. A fresh login or account change produces
    /// a different value without exposing OAuth material to callers.
    pub async fn local_credential_fingerprint<S: TokenStore + ?Sized>(
        &self,
        store: &S,
    ) -> Result<Option<String>, NativeAuthError> {
        let token = store.load(&self.credential_key()).await?;
        let Some(token) = token else {
            return Ok(None);
        };
        self.validate_stored_token(&token)?;
        let binding = token
            .id_token
            .as_deref()
            .or(token.refresh_token.as_deref())
            .unwrap_or(&token.access_token);
        let mut digest = Sha256::new();
        digest.update(b"reporch.remote-fallback-consent.v1\0");
        digest.update(self.issuer().as_bytes());
        digest.update(b"\0");
        digest.update(self.client_id.as_bytes());
        digest.update(b"\0");
        digest.update(binding.as_bytes());
        Ok(Some(
            digest
                .finalize()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
        ))
    }

    fn credential_key(&self) -> CredentialKey {
        let identity = format!("{}\u{1f}{}", self.issuer(), self.client_id);
        CredentialKey {
            service: KEYRING_SERVICE.into(),
            account: format!("oauth-{}", hex_digest(identity.as_bytes())),
        }
    }

    fn validate_stored_token(&self, token: &StoredTokenSet) -> Result<(), NativeAuthError> {
        let refresh_token_valid = token
            .refresh_token
            .as_ref()
            .is_some_and(|value| !value.is_empty() && value.len() <= MAX_TOKEN_BYTES);
        let id_token_valid = token
            .id_token
            .as_ref()
            .is_none_or(|value| !value.is_empty() && value.len() <= MAX_TOKEN_BYTES);
        let scopes_valid = !token.scopes.is_empty()
            && token.scopes.len() <= 16
            && token.scopes.iter().all(|scope| {
                !scope.is_empty() && scope.len() <= 128 && !scope.chars().any(char::is_whitespace)
            });
        let dpop_valid = match (&token.schema[..], &token.token_type[..], &token.device_key) {
            (TOKEN_SCHEMA_V1, "Bearer", None) => true,
            (TOKEN_SCHEMA_V2, "DPoP", Some(key)) => key.validate().is_ok(),
            _ => false,
        };
        if !dpop_valid
            || token.issuer != self.issuer()
            || token.client_id != self.client_id
            || token.access_token.is_empty()
            || token.access_token.len() > MAX_TOKEN_BYTES
            || !refresh_token_valid
            || !id_token_valid
            || !scopes_valid
        {
            return Err(NativeAuthError::CredentialStoreCorrupt);
        }
        Ok(())
    }

    fn status_from_token(&self, token: Option<&StoredTokenSet>) -> NativeSessionStatus {
        NativeSessionStatus {
            authenticated: token.is_some(),
            issuer: self.issuer().into(),
            client_id: self.client_id.clone(),
            expires_at: token.map(|token| token.expires_at),
            scopes: token
                .map(|token| token.scopes.clone())
                .unwrap_or_else(|| self.scopes.clone()),
            refresh_available: token
                .and_then(|token| token.refresh_token.as_ref())
                .is_some(),
        }
    }
}

#[derive(Clone, Serialize)]
pub struct DeviceAuthorizationPrompt {
    #[serde(skip_serializing)]
    device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: Option<String>,
    pub expires_at: DateTime<Utc>,
    pub interval_seconds: u64,
    #[serde(skip_serializing)]
    device_key: Option<DeviceKeyV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EcPublicJwkV1 {
    kty: String,
    crv: String,
    x: String,
    y: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeviceKeyV1 {
    private_scalar: String,
    public_jwk: EcPublicJwkV1,
    thumbprint: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct NativeAuthorization {
    pub authorization: String,
    pub dpop_proof: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NativeSessionStatus {
    pub authenticated: bool,
    pub issuer: String,
    pub client_id: String,
    pub expires_at: Option<DateTime<Utc>>,
    pub scopes: Vec<String>,
    pub refresh_available: bool,
}

#[derive(Clone)]
pub struct NativeAuthClient {
    config: NativeAuthConfig,
    metadata: ProviderMetadata,
    http: reqwest::Client,
}

impl NativeAuthClient {
    pub async fn discover(config: NativeAuthConfig) -> Result<Self, NativeAuthError> {
        let discovery_url = config
            .issuer
            .join(".well-known/openid-configuration")
            .map_err(|error| NativeAuthError::InvalidConfiguration(error.to_string()))?;
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(StdDuration::from_secs(15))
            .build()
            .map_err(NativeAuthError::HttpClient)?;
        let response = http
            .get(discovery_url)
            .send()
            .await
            .map_err(NativeAuthError::Network)?;
        if !response.status().is_success() {
            return Err(NativeAuthError::ProviderResponse(response.status()));
        }
        let mut metadata: ProviderMetadata = decode_provider_json(response).await?;
        let discovered_issuer = normalized_issuer(&metadata.issuer, config.allow_insecure_http)?;
        if discovered_issuer != config.issuer {
            return Err(NativeAuthError::IssuerMismatch);
        }
        validate_provider_endpoint(&metadata.authorization_endpoint, config.allow_insecure_http)?;
        validate_provider_endpoint(&metadata.token_endpoint, config.allow_insecure_http)?;
        if let Some(endpoint) = &metadata.device_authorization_endpoint {
            validate_provider_endpoint(endpoint, config.allow_insecure_http)?;
        } else {
            metadata.device_authorization_endpoint = Some(
                config
                    .issuer
                    .join("device-authorization/")
                    .map_err(|error| NativeAuthError::InvalidConfiguration(error.to_string()))?,
            );
        }
        if let Some(endpoint) = &metadata.revocation_endpoint {
            validate_provider_endpoint(endpoint, config.allow_insecure_http)?;
        }
        if config.dpop_required
            && !metadata
                .dpop_signing_alg_values_supported
                .iter()
                .any(|algorithm| algorithm == "ES256")
        {
            return Err(NativeAuthError::ServerUpgradeRequired);
        }
        Ok(Self {
            config,
            metadata,
            http,
        })
    }

    pub async fn request_device_authorization(
        &self,
    ) -> Result<DeviceAuthorizationPrompt, NativeAuthError> {
        let endpoint = self
            .metadata
            .device_authorization_endpoint
            .as_ref()
            .ok_or(NativeAuthError::DeviceAuthorizationUnsupported)?;
        let scopes = self.config.scopes.join(" ");
        let device_key = self
            .config
            .dpop_required
            .then(DeviceKeyV1::generate)
            .transpose()?;
        let mut request = self.http.post(endpoint.clone()).form(&[
            ("client_id", self.config.client_id.as_str()),
            ("scope", scopes.as_str()),
        ]);
        if let Some(key) = &device_key {
            request = request.header("DPoP", key.proof("POST", endpoint, None)?);
        }
        let response = request.send().await.map_err(NativeAuthError::Network)?;
        if !response.status().is_success() {
            return Err(provider_error(response).await);
        }
        let response: DeviceAuthorizationWire = decode_provider_json(response).await?;
        if response.device_code.is_empty()
            || response.user_code.is_empty()
            || response.expires_in == 0
            || response.expires_in > 30 * 60
        {
            return Err(NativeAuthError::InvalidProviderResponse);
        }
        let verification_uri = validate_provider_endpoint(
            &response.verification_uri,
            self.config.allow_insecure_http,
        )?;
        let verification_uri_complete = response
            .verification_uri_complete
            .map(|url| validate_provider_endpoint(&url, self.config.allow_insecure_http))
            .transpose()?;
        Ok(DeviceAuthorizationPrompt {
            device_code: response.device_code,
            user_code: response.user_code,
            verification_uri: verification_uri.to_string(),
            verification_uri_complete: verification_uri_complete.map(|url| url.to_string()),
            expires_at: Utc::now() + Duration::seconds(response.expires_in as i64),
            interval_seconds: response.interval.unwrap_or(5).clamp(1, 30),
            device_key,
        })
    }

    pub async fn finish_device_authorization<S: TokenStore + ?Sized>(
        &self,
        prompt: &DeviceAuthorizationPrompt,
        store: &S,
    ) -> Result<NativeSessionStatus, NativeAuthError> {
        let mut interval = prompt.interval_seconds;
        loop {
            if Utc::now() >= prompt.expires_at {
                return Err(NativeAuthError::DeviceCodeExpired);
            }
            sleep(StdDuration::from_secs(interval)).await;
            let mut request = self.http.post(self.metadata.token_endpoint.clone()).form(&[
                ("grant_type", DEVICE_GRANT_TYPE),
                ("device_code", prompt.device_code.as_str()),
                ("client_id", self.config.client_id.as_str()),
            ]);
            if let Some(key) = &prompt.device_key {
                request = request.header(
                    "DPoP",
                    key.proof("POST", &self.metadata.token_endpoint, None)?,
                );
            }
            let response = match request.send().await {
                Ok(response) => response,
                Err(_error) => {
                    // Device polling is explicitly repeatable until the code expires.
                    // A transient DNS/TLS/connect/read timeout must not discard the
                    // in-memory DPoP key after the user has already approved the code.
                    if Utc::now() >= prompt.expires_at {
                        return Err(NativeAuthError::DeviceCodeExpired);
                    }
                    interval = (interval + 1).min(30);
                    continue;
                }
            };
            if response.status().is_success() {
                let token = self
                    .decode_initial_token(response, prompt.device_key.clone())
                    .await?;
                let key = self.config.credential_key();
                let _guard = store.lock(&key).await?;
                store.save(&key, &token).await?;
                return Ok(self.config.status_from_token(Some(&token)));
            }
            let status = response.status();
            if matches!(status.as_u16(), 429 | 500 | 502 | 503 | 504) {
                interval = (interval + 1).min(30);
                continue;
            }
            let error = decode_provider_json::<OAuthErrorWire>(response)
                .await
                .unwrap_or(OAuthErrorWire {
                    error: "invalid_response".into(),
                    error_description: None,
                });
            match error.error.as_str() {
                "authorization_pending" => {}
                "slow_down" => interval = (interval + 5).min(30),
                "access_denied" => return Err(NativeAuthError::AccessDenied),
                "expired_token" => return Err(NativeAuthError::DeviceCodeExpired),
                _ => {
                    return Err(NativeAuthError::OAuth {
                        code: error.error,
                        description: error.error_description,
                        status,
                    });
                }
            }
        }
    }

    pub async fn login_loopback_pkce<S: TokenStore + ?Sized>(
        &self,
        store: &S,
    ) -> Result<NativeSessionStatus, NativeAuthError> {
        let mut redirect_uri = self.config.redirect_uri.clone().ok_or_else(|| {
            NativeAuthError::InvalidConfiguration("redirect URI is missing".into())
        })?;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, redirect_uri.port().unwrap_or(0)))
            .await
            .map_err(NativeAuthError::Loopback)?;
        let callback_port = listener
            .local_addr()
            .map_err(NativeAuthError::Loopback)?
            .port();
        redirect_uri.set_port(Some(callback_port)).map_err(|()| {
            NativeAuthError::InvalidConfiguration("redirect port is invalid".into())
        })?;
        let verifier = random_urlsafe(48);
        let challenge = pkce_challenge(&verifier);
        let state = random_urlsafe(32);
        let mut authorization_url = self.metadata.authorization_endpoint.clone();
        authorization_url
            .query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &self.config.client_id)
            .append_pair("redirect_uri", redirect_uri.as_str())
            .append_pair("scope", &self.config.scopes.join(" "))
            .append_pair("state", &state)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256");
        let browser_url = authorization_url.to_string();
        tokio::task::spawn_blocking(move || open::that(browser_url))
            .await
            .map_err(|error| NativeAuthError::Browser(error.to_string()))?
            .map_err(|error| NativeAuthError::Browser(error.to_string()))?;
        let code = receive_loopback_code(listener, &redirect_uri, &state).await?;
        let response = self
            .http
            .post(self.metadata.token_endpoint.clone())
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", code.as_str()),
                ("redirect_uri", redirect_uri.as_str()),
                ("client_id", self.config.client_id.as_str()),
                ("code_verifier", verifier.as_str()),
            ])
            .send()
            .await
            .map_err(NativeAuthError::Network)?;
        if !response.status().is_success() {
            return Err(provider_error(response).await);
        }
        let token = self.decode_initial_token(response, None).await?;
        let key = self.config.credential_key();
        let _guard = store.lock(&key).await?;
        store.save(&key, &token).await?;
        Ok(self.config.status_from_token(Some(&token)))
    }

    pub async fn session_status<S: TokenStore + ?Sized>(
        &self,
        store: &S,
    ) -> Result<NativeSessionStatus, NativeAuthError> {
        self.config.local_session_status(store).await
    }

    pub async fn access_token<S: TokenStore + ?Sized>(
        &self,
        store: &S,
    ) -> Result<String, NativeAuthError> {
        let key = self.config.credential_key();
        let _guard = store.lock(&key).await?;
        let mut token = store
            .load(&key)
            .await?
            .ok_or(NativeAuthError::NotAuthenticated)?;
        self.config.validate_stored_token(&token)?;
        if token.expires_at <= Utc::now() + Duration::seconds(TOKEN_EXPIRY_SKEW_SECONDS) {
            token = self.refresh_token(&token).await?;
            store.save(&key, &token).await?;
        }
        Ok(token.access_token)
    }

    pub async fn authorization<S: TokenStore + ?Sized>(
        &self,
        store: &S,
        method: &str,
        target_uri: &Url,
    ) -> Result<NativeAuthorization, NativeAuthError> {
        let key = self.config.credential_key();
        let _guard = store.lock(&key).await?;
        let mut token = store
            .load(&key)
            .await?
            .ok_or(NativeAuthError::NotAuthenticated)?;
        self.config.validate_stored_token(&token)?;
        if token.expires_at <= Utc::now() + Duration::seconds(TOKEN_EXPIRY_SKEW_SECONDS) {
            token = self.refresh_token(&token).await?;
            store.save(&key, &token).await?;
        }
        let dpop_proof = token
            .device_key
            .as_ref()
            .map(|key| key.proof(method, target_uri, Some(&token.access_token)))
            .transpose()?;
        let scheme = if dpop_proof.is_some() {
            "DPoP"
        } else {
            "Bearer"
        };
        Ok(NativeAuthorization {
            authorization: format!("{scheme} {}", token.access_token),
            dpop_proof,
        })
    }

    pub async fn logout<S: TokenStore + ?Sized>(&self, store: &S) -> Result<bool, NativeAuthError> {
        let key = self.config.credential_key();
        let _guard = store.lock(&key).await?;
        let token = store.load(&key).await?;
        if let Some(token) = token.as_ref()
            && let Err(error) = self.config.validate_stored_token(token)
        {
            store.delete(&key).await?;
            return Err(error);
        }
        let Some(token) = token.as_ref() else {
            store.delete(&key).await?;
            return Ok(false);
        };
        let endpoint = self
            .metadata
            .revocation_endpoint
            .as_ref()
            .ok_or(NativeAuthError::RevocationUnavailable)?;
        let revoke_token = token
            .refresh_token
            .as_deref()
            .unwrap_or(&token.access_token);
        let mut request = self.http.post(endpoint.clone()).form(&[
            ("token", revoke_token),
            ("client_id", self.config.client_id.as_str()),
        ]);
        if let Some(key) = token.device_key.as_ref() {
            request = request.header("DPoP", key.proof("POST", endpoint, None)?);
        }
        let response = request.send().await.map_err(NativeAuthError::Network)?;
        if !response.status().is_success() {
            return Err(provider_error(response).await);
        }
        store.delete(&key).await?;
        Ok(true)
    }

    async fn refresh_token(
        &self,
        previous: &StoredTokenSet,
    ) -> Result<StoredTokenSet, NativeAuthError> {
        let refresh_token = previous
            .refresh_token
            .as_deref()
            .ok_or(NativeAuthError::RefreshUnavailable)?;
        let mut request = self.http.post(self.metadata.token_endpoint.clone()).form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", self.config.client_id.as_str()),
        ]);
        if let Some(key) = previous.device_key.as_ref() {
            request = request.header(
                "DPoP",
                key.proof("POST", &self.metadata.token_endpoint, None)?,
            );
        }
        let response = request.send().await.map_err(NativeAuthError::Network)?;
        if !response.status().is_success() {
            return Err(provider_error(response).await);
        }
        let wire: TokenResponseWire = decode_provider_json(response).await?;
        let refreshed = self.token_from_wire(wire, previous.device_key.clone())?;
        validate_refresh_rotation(&refreshed, refresh_token)?;
        Ok(refreshed)
    }

    async fn decode_initial_token(
        &self,
        response: reqwest::Response,
        device_key: Option<DeviceKeyV1>,
    ) -> Result<StoredTokenSet, NativeAuthError> {
        let wire: TokenResponseWire = decode_provider_json(response).await?;
        let token = self.token_from_wire(wire, device_key)?;
        if token.refresh_token.is_none() {
            return Err(NativeAuthError::RefreshUnavailable);
        }
        Ok(token)
    }

    fn token_from_wire(
        &self,
        wire: TokenResponseWire,
        device_key: Option<DeviceKeyV1>,
    ) -> Result<StoredTokenSet, NativeAuthError> {
        let expected_token_type = if device_key.is_some() {
            "DPoP"
        } else {
            "Bearer"
        };
        if wire.access_token.is_empty()
            || wire.access_token.len() > MAX_TOKEN_BYTES
            || !wire.token_type.eq_ignore_ascii_case(expected_token_type)
            || wire.expires_in == 0
            || wire.expires_in > 24 * 60 * 60
        {
            return Err(NativeAuthError::InvalidProviderResponse);
        }
        let scopes = wire
            .scope
            .map(|scope| scope.split_whitespace().map(str::to_owned).collect())
            .unwrap_or_else(|| self.config.scopes.clone());
        if scopes.is_empty()
            || scopes.len() > 16
            || scopes.iter().any(|scope| {
                scope.is_empty() || scope.len() > 128 || scope.chars().any(char::is_whitespace)
            })
            || wire
                .refresh_token
                .as_ref()
                .is_some_and(|value| value.is_empty() || value.len() > MAX_TOKEN_BYTES)
            || wire
                .id_token
                .as_ref()
                .is_some_and(|value| value.is_empty() || value.len() > MAX_TOKEN_BYTES)
        {
            return Err(NativeAuthError::InvalidProviderResponse);
        }
        Ok(StoredTokenSet {
            schema: if device_key.is_some() {
                TOKEN_SCHEMA_V2.into()
            } else {
                TOKEN_SCHEMA_V1.into()
            },
            issuer: self.config.issuer().into(),
            client_id: self.config.client_id.clone(),
            access_token: wire.access_token,
            refresh_token: wire.refresh_token,
            id_token: wire.id_token,
            token_type: expected_token_type.into(),
            expires_at: Utc::now() + Duration::seconds(wire.expires_in as i64),
            scopes,
            device_key,
        })
    }
}

fn validate_refresh_rotation(
    refreshed: &StoredTokenSet,
    previous_refresh_token: &str,
) -> Result<(), NativeAuthError> {
    refreshed
        .refresh_token
        .as_deref()
        .filter(|candidate| !candidate.is_empty() && *candidate != previous_refresh_token)
        .ok_or(NativeAuthError::RefreshRotationRequired)?;
    Ok(())
}

#[derive(Clone)]
pub struct CredentialKey {
    service: String,
    account: String,
}

#[async_trait]
pub trait TokenStore: Send + Sync {
    async fn lock(
        &self,
        _key: &CredentialKey,
    ) -> Result<CredentialOperationGuard, NativeAuthError> {
        Ok(CredentialOperationGuard::default())
    }
    async fn load(&self, key: &CredentialKey) -> Result<Option<StoredTokenSet>, NativeAuthError>;
    async fn save(
        &self,
        key: &CredentialKey,
        token: &StoredTokenSet,
    ) -> Result<(), NativeAuthError>;
    async fn delete(&self, key: &CredentialKey) -> Result<(), NativeAuthError>;
}

#[derive(Default)]
pub struct CredentialOperationGuard(Option<fs::File>);

impl Drop for CredentialOperationGuard {
    fn drop(&mut self) {
        if let Some(file) = self.0.as_ref() {
            let _ = fs2::FileExt::unlock(file);
        }
    }
}

#[derive(Debug, Clone)]
pub struct NativeFileTokenStore {
    path: PathBuf,
}

impl NativeFileTokenStore {
    pub fn discover() -> Result<Self, NativeAuthError> {
        Ok(Self {
            path: native_credential_path()?,
        })
    }

    fn lock_path(&self) -> PathBuf {
        self.path.with_extension("lock")
    }
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeCredentialFileV2 {
    schema: String,
    entries: BTreeMap<String, StoredTokenSet>,
}

fn credential_map_key(key: &CredentialKey) -> String {
    format!("{}\u{1f}{}", key.service, key.account)
}

#[async_trait]
impl TokenStore for NativeFileTokenStore {
    async fn lock(
        &self,
        _key: &CredentialKey,
    ) -> Result<CredentialOperationGuard, NativeAuthError> {
        let path = self.lock_path();
        tokio::task::spawn_blocking(move || acquire_credential_file_lock(&path))
            .await
            .map_err(|error| NativeAuthError::CredentialStore(error.to_string()))?
    }

    async fn load(&self, key: &CredentialKey) -> Result<Option<StoredTokenSet>, NativeAuthError> {
        let map_key = credential_map_key(key);
        // The credential file is bounded to 256 KiB and read exactly once. Keeping this
        // operation inline avoids creating a fresh blocking-pool thread for every short-lived
        // `reporch auth status` process while retaining all ownership, mode, link, and size
        // checks in `read_credential_file`.
        let file = read_credential_file(&self.path)?;
        Ok(file.entries.get(&map_key).cloned())
    }

    async fn save(
        &self,
        key: &CredentialKey,
        token: &StoredTokenSet,
    ) -> Result<(), NativeAuthError> {
        let path = self.path.clone();
        let map_key = credential_map_key(key);
        let token = token.clone();
        tokio::task::spawn_blocking(move || {
            let mut file = read_credential_file(&path)?;
            file.entries.insert(map_key, token);
            write_credential_file(&path, &file)
        })
        .await
        .map_err(|error| NativeAuthError::CredentialStore(error.to_string()))?
    }

    async fn delete(&self, key: &CredentialKey) -> Result<(), NativeAuthError> {
        let path = self.path.clone();
        let map_key = credential_map_key(key);
        tokio::task::spawn_blocking(move || {
            let mut file = read_credential_file(&path)?;
            file.entries.remove(&map_key);
            if file.entries.is_empty() {
                match fs::remove_file(&path) {
                    Ok(()) => Ok(()),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    Err(error) => Err(NativeAuthError::CredentialStore(error.to_string())),
                }
            } else {
                write_credential_file(&path, &file)
            }
        })
        .await
        .map_err(|error| NativeAuthError::CredentialStore(error.to_string()))?
    }
}

#[derive(Default)]
pub struct KeyringTokenStore;

async fn run_keyring_operation<T, F>(operation: F) -> Result<T, NativeAuthError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, KeyringError> + Send + 'static,
{
    run_keyring_operation_with_timeout(CREDENTIAL_STORE_TIMEOUT, operation).await
}

async fn run_keyring_operation_with_timeout<T, F>(
    timeout: StdDuration,
    operation: F,
) -> Result<T, NativeAuthError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, KeyringError> + Send + 'static,
{
    let (sender, receiver) = tokio::sync::oneshot::channel();
    // Keep OS credential calls outside Tokio's blocking pool. Some platform keyrings can wait
    // indefinitely for user approval, and a timed-out `spawn_blocking` task would still delay
    // runtime shutdown. Dropping this handle deliberately detaches the operation so a CLI process
    // can fail closed at the deadline; the operating system terminates it when the process exits.
    let _worker = std::thread::Builder::new()
        .name("reporch-credential-store".into())
        .spawn(move || {
            let _ = sender.send(operation());
        })
        .map_err(|error| NativeAuthError::CredentialStore(error.to_string()))?;

    match tokio::time::timeout(timeout, receiver).await {
        Ok(Ok(result)) => {
            result.map_err(|error| NativeAuthError::CredentialStore(error.to_string()))
        }
        Ok(Err(_)) => Err(NativeAuthError::CredentialStore(
            "credential-store worker stopped unexpectedly".into(),
        )),
        Err(_) => Err(NativeAuthError::CredentialStoreTimeout),
    }
}

/// Exercise the same OS credential-store adapter used for OAuth refresh tokens.
/// The random canary is deleted before this function returns and is never logged.
pub async fn qualification_keyring_canary() -> Result<(), NativeAuthError> {
    let mut random = [0_u8; 32];
    rand::thread_rng().fill_bytes(&mut random);
    let canary = URL_SAFE_NO_PAD.encode(random);
    let key = CredentialKey {
        service: KEYRING_SERVICE.into(),
        account: format!("qualification-{}", hex_digest(canary.as_bytes())),
    };
    let token = StoredTokenSet {
        schema: TOKEN_SCHEMA_V1.into(),
        issuer: "https://qualification.invalid/oauth".into(),
        client_id: "reporch-studio-desktop-qualification".into(),
        access_token: canary.clone(),
        refresh_token: Some(canary.clone()),
        id_token: None,
        token_type: "Bearer".into(),
        expires_at: Utc::now() + Duration::minutes(5),
        scopes: vec!["openid".into()],
        device_key: None,
    };
    let store = KeyringTokenStore;
    let result = async {
        store.save(&key, &token).await?;
        let restored = store.load(&key).await?.ok_or_else(|| {
            NativeAuthError::CredentialStore("qualification canary was not restored".into())
        })?;
        if restored.schema != TOKEN_SCHEMA_V1
            || restored.issuer != token.issuer
            || restored.client_id != token.client_id
            || restored.access_token != canary
            || restored.refresh_token.as_deref() != Some(canary.as_str())
        {
            return Err(NativeAuthError::CredentialStoreCorrupt);
        }
        Ok(())
    }
    .await;
    let cleanup = store.delete(&key).await;
    result?;
    cleanup
}

/// Exercise the 1.0 permission-restricted credential file without touching a
/// user's real login or invoking an OS keychain prompt.
pub async fn qualification_native_file_canary() -> Result<(), NativeAuthError> {
    let directory = std::env::temp_dir().join(format!(
        "reporch-credential-qualification-{}",
        uuid::Uuid::now_v7()
    ));
    fs::create_dir(&directory)
        .map_err(|error| NativeAuthError::CredentialStore(error.to_string()))?;
    let store = NativeFileTokenStore {
        path: directory.join("credentials-v2.json"),
    };
    let key = CredentialKey {
        service: KEYRING_SERVICE.into(),
        account: "qualification-native-file".into(),
    };
    let token = StoredTokenSet {
        schema: TOKEN_SCHEMA_V2.into(),
        issuer: "https://qualification.invalid/oauth".into(),
        client_id: "reporch-studio-cli-v1".into(),
        access_token: "qualification-access-token".into(),
        refresh_token: Some("qualification-refresh-token".into()),
        id_token: None,
        token_type: "DPoP".into(),
        expires_at: Utc::now() + Duration::minutes(5),
        scopes: vec!["openid".into()],
        device_key: Some(DeviceKeyV1::generate()?),
    };
    let result = async {
        let _guard = store.lock(&key).await?;
        store.save(&key, &token).await?;
        let restored = store
            .load(&key)
            .await?
            .ok_or(NativeAuthError::CredentialStoreCorrupt)?;
        if restored.access_token != token.access_token
            || restored.refresh_token != token.refresh_token
            || restored
                .device_key
                .as_ref()
                .is_none_or(|key| key.validate().is_err())
        {
            return Err(NativeAuthError::CredentialStoreCorrupt);
        }
        store.delete(&key).await
    }
    .await;
    let _ = fs::remove_dir_all(&directory);
    result
}

/// Remove a pre-1.0 bearer credential from the OS keychain before a fresh
/// sender-constrained login. Bearer refresh tokens cannot be safely converted
/// into DPoP credentials, so migration deliberately consists of revoking local
/// reuse and completing a new authorization flow.
pub async fn remove_legacy_keyring_credential(
    issuer: &str,
    client_id: &str,
    allow_insecure_http: bool,
) -> Result<bool, NativeAuthError> {
    let config = NativeAuthConfig::device(
        issuer,
        client_id,
        vec!["openid".into()],
        allow_insecure_http,
    )?;
    let key = config.credential_key();
    let store = KeyringTokenStore;
    let existed = store.load(&key).await?.is_some();
    if existed {
        match NativeAuthClient::discover(config.clone()).await {
            Ok(client) => {
                let _ = client.logout(&store).await?;
            }
            Err(_) => store.delete(&key).await?,
        }
    }
    Ok(existed)
}

#[async_trait]
impl TokenStore for KeyringTokenStore {
    async fn load(&self, key: &CredentialKey) -> Result<Option<StoredTokenSet>, NativeAuthError> {
        let key = key.clone();
        let value = run_keyring_operation(move || {
            let entry = Entry::new(&key.service, &key.account)?;
            match entry.get_password() {
                Ok(value) => Ok(Some(value)),
                Err(KeyringError::NoEntry) => Ok(None),
                Err(error) => Err(error),
            }
        })
        .await?;
        value
            .map(|value| {
                serde_json::from_str(&value).map_err(|_| NativeAuthError::CredentialStoreCorrupt)
            })
            .transpose()
    }

    async fn save(
        &self,
        key: &CredentialKey,
        token: &StoredTokenSet,
    ) -> Result<(), NativeAuthError> {
        let key = key.clone();
        let value = serde_json::to_string(token)
            .map_err(|error| NativeAuthError::CredentialStore(error.to_string()))?;
        run_keyring_operation(move || Entry::new(&key.service, &key.account)?.set_password(&value))
            .await
    }

    async fn delete(&self, key: &CredentialKey) -> Result<(), NativeAuthError> {
        let key = key.clone();
        run_keyring_operation(move || {
            let entry = Entry::new(&key.service, &key.account)?;
            match entry.delete_credential() {
                Ok(()) | Err(KeyringError::NoEntry) => Ok(()),
                Err(error) => Err(error),
            }
        })
        .await
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct StoredTokenSet {
    schema: String,
    issuer: String,
    client_id: String,
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    token_type: String,
    expires_at: DateTime<Utc>,
    scopes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    device_key: Option<DeviceKeyV1>,
}

#[derive(Clone, Deserialize)]
struct ProviderMetadata {
    issuer: String,
    authorization_endpoint: Url,
    token_endpoint: Url,
    #[serde(default)]
    device_authorization_endpoint: Option<Url>,
    #[serde(default)]
    revocation_endpoint: Option<Url>,
    #[serde(default)]
    dpop_signing_alg_values_supported: Vec<String>,
}

#[derive(Deserialize)]
struct DeviceAuthorizationWire {
    device_code: String,
    user_code: String,
    verification_uri: Url,
    #[serde(default)]
    verification_uri_complete: Option<Url>,
    expires_in: u64,
    #[serde(default)]
    interval: Option<u64>,
}

#[derive(Deserialize)]
struct TokenResponseWire {
    access_token: String,
    token_type: String,
    expires_in: u64,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    scope: Option<String>,
}

#[derive(Deserialize)]
struct OAuthErrorWire {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

#[derive(Debug, Error)]
pub enum NativeAuthError {
    #[error("native OAuth configuration is invalid: {0}")]
    InvalidConfiguration(String),
    #[error("OIDC discovery returned a different issuer")]
    IssuerMismatch,
    #[error("native OAuth provider returned HTTP {0}")]
    ProviderResponse(StatusCode),
    #[error("native OAuth provider returned an invalid response")]
    InvalidProviderResponse,
    #[error("device authorization is not supported by this provider")]
    DeviceAuthorizationUnsupported,
    #[error("Reporch authentication server does not support the required DPoP CLI capability")]
    ServerUpgradeRequired,
    #[error("device authorization expired")]
    DeviceCodeExpired,
    #[error("the user denied authorization")]
    AccessDenied,
    #[error("OAuth error {code}: {description:?} (HTTP {status})")]
    OAuth {
        code: String,
        description: Option<String>,
        status: StatusCode,
    },
    #[error("a refresh token was not issued")]
    RefreshUnavailable,
    #[error("the OAuth provider did not rotate the refresh token")]
    RefreshRotationRequired,
    #[error("the OAuth provider does not expose a revocation endpoint")]
    RevocationUnavailable,
    #[error("native client is not authenticated")]
    NotAuthenticated,
    #[error("native OAuth network request failed")]
    Network(#[source] reqwest::Error),
    #[error("native OAuth HTTP client could not be built")]
    HttpClient(#[source] reqwest::Error),
    #[error("loopback callback failed")]
    Loopback(#[source] std::io::Error),
    #[error("loopback callback timed out")]
    LoopbackTimeout,
    #[error("loopback callback state did not match")]
    StateMismatch,
    #[error("system browser could not be opened: {0}")]
    Browser(String),
    #[error("OS credential store failed: {0}")]
    CredentialStore(String),
    #[error("OS credential store timed out; approve any system credential prompt and retry")]
    CredentialStoreTimeout,
    #[error("OS credential store contains an invalid Studio token")]
    CredentialStoreCorrupt,
}

async fn provider_error(response: reqwest::Response) -> NativeAuthError {
    let status = response.status();
    match decode_provider_json::<OAuthErrorWire>(response).await {
        Ok(error) => NativeAuthError::OAuth {
            code: error.error,
            description: error.error_description,
            status,
        },
        Err(_) => NativeAuthError::ProviderResponse(status),
    }
}

async fn decode_provider_json<T: DeserializeOwned>(
    mut response: reqwest::Response,
) -> Result<T, NativeAuthError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_PROVIDER_RESPONSE_BYTES as u64)
    {
        return Err(NativeAuthError::InvalidProviderResponse);
    }
    let mut body = Vec::with_capacity(
        response
            .content_length()
            .unwrap_or(4096)
            .min(MAX_PROVIDER_RESPONSE_BYTES as u64) as usize,
    );
    while let Some(chunk) = response.chunk().await.map_err(NativeAuthError::Network)? {
        if body.len().saturating_add(chunk.len()) > MAX_PROVIDER_RESPONSE_BYTES {
            return Err(NativeAuthError::InvalidProviderResponse);
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).map_err(|_| NativeAuthError::InvalidProviderResponse)
}

impl DeviceKeyV1 {
    fn generate() -> Result<Self, NativeAuthError> {
        let signing_key = SigningKey::random(&mut rand::rngs::OsRng);
        let point = signing_key.verifying_key().to_encoded_point(false);
        let x = point.x().ok_or(NativeAuthError::CredentialStoreCorrupt)?;
        let y = point.y().ok_or(NativeAuthError::CredentialStoreCorrupt)?;
        let public_jwk = EcPublicJwkV1 {
            kty: "EC".into(),
            crv: "P-256".into(),
            x: URL_SAFE_NO_PAD.encode(x),
            y: URL_SAFE_NO_PAD.encode(y),
        };
        let thumbprint = jwk_thumbprint(&public_jwk)?;
        Ok(Self {
            private_scalar: URL_SAFE_NO_PAD.encode(signing_key.to_bytes()),
            public_jwk,
            thumbprint,
        })
    }

    fn signing_key(&self) -> Result<SigningKey, NativeAuthError> {
        let private = URL_SAFE_NO_PAD
            .decode(&self.private_scalar)
            .map_err(|_| NativeAuthError::CredentialStoreCorrupt)?;
        SigningKey::from_slice(&private).map_err(|_| NativeAuthError::CredentialStoreCorrupt)
    }

    fn validate(&self) -> Result<(), NativeAuthError> {
        if self.public_jwk.kty != "EC"
            || self.public_jwk.crv != "P-256"
            || self.thumbprint != jwk_thumbprint(&self.public_jwk)?
        {
            return Err(NativeAuthError::CredentialStoreCorrupt);
        }
        let signing_key = self.signing_key()?;
        let point = signing_key.verifying_key().to_encoded_point(false);
        if point.x().map(|value| URL_SAFE_NO_PAD.encode(value)) != Some(self.public_jwk.x.clone())
            || point.y().map(|value| URL_SAFE_NO_PAD.encode(value))
                != Some(self.public_jwk.y.clone())
        {
            return Err(NativeAuthError::CredentialStoreCorrupt);
        }
        Ok(())
    }

    fn proof(
        &self,
        method: &str,
        target_uri: &Url,
        access_token: Option<&str>,
    ) -> Result<String, NativeAuthError> {
        self.validate()?;
        let method = method.trim().to_ascii_uppercase();
        if !matches!(method.as_str(), "GET" | "POST" | "PUT" | "PATCH" | "DELETE") {
            return Err(NativeAuthError::InvalidConfiguration(
                "DPoP method is unsupported".into(),
            ));
        }
        let htu = normalized_dpop_htu(target_uri)?;
        let header = serde_json::json!({
            "alg": "ES256",
            "typ": "dpop+jwt",
            "jwk": self.public_jwk,
        });
        let mut claims = serde_json::json!({
            "htm": method,
            "htu": htu,
            "iat": Utc::now().timestamp(),
            "jti": uuid::Uuid::now_v7().to_string(),
        });
        if let Some(access_token) = access_token {
            claims["ath"] = serde_json::Value::String(
                URL_SAFE_NO_PAD.encode(Sha256::digest(access_token.as_bytes())),
            );
        }
        let encoded_header = URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&header)
                .map_err(|error| NativeAuthError::CredentialStore(error.to_string()))?,
        );
        let encoded_claims = URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&claims)
                .map_err(|error| NativeAuthError::CredentialStore(error.to_string()))?,
        );
        let signing_input = format!("{encoded_header}.{encoded_claims}");
        let signature: Signature = self.signing_key()?.sign(signing_input.as_bytes());
        Ok(format!(
            "{signing_input}.{}",
            URL_SAFE_NO_PAD.encode(signature.to_bytes())
        ))
    }
}

fn jwk_thumbprint(jwk: &EcPublicJwkV1) -> Result<String, NativeAuthError> {
    let canonical = serde_json::to_vec(&serde_json::json!({
        "crv": "P-256",
        "kty": "EC",
        "x": jwk.x,
        "y": jwk.y,
    }))
    .map_err(|error| NativeAuthError::CredentialStore(error.to_string()))?;
    Ok(URL_SAFE_NO_PAD.encode(Sha256::digest(canonical)))
}

fn normalized_dpop_htu(value: &Url) -> Result<String, NativeAuthError> {
    validate_provider_endpoint(value, value.scheme() == "http")?;
    let mut value = value.clone();
    value.set_query(None);
    value.set_fragment(None);
    if (value.scheme() == "https" && value.port() == Some(443))
        || (value.scheme() == "http" && value.port() == Some(80))
    {
        value.set_port(None).map_err(|()| {
            NativeAuthError::InvalidConfiguration("invalid DPoP target port".into())
        })?;
    }
    Ok(value.to_string())
}

fn native_credential_path() -> Result<PathBuf, NativeAuthError> {
    if let Some(override_path) = std::env::var_os("REPORCH_CONFIG_HOME") {
        let root = PathBuf::from(override_path);
        if !root.is_absolute() {
            return Err(NativeAuthError::CredentialStore(
                "REPORCH_CONFIG_HOME must be absolute".into(),
            ));
        }
        return Ok(root.join("credentials-v2.json"));
    }
    #[cfg(windows)]
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .ok_or_else(|| NativeAuthError::CredentialStore("APPDATA is unavailable".into()))?;
    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .ok_or_else(|| {
            NativeAuthError::CredentialStore("config directory is unavailable".into())
        })?;
    Ok(base.join("reporch").join("credentials-v2.json"))
}

fn ensure_secure_credential_directory(path: &Path) -> Result<(), NativeAuthError> {
    let parent = path
        .parent()
        .ok_or_else(|| NativeAuthError::CredentialStore("credential path has no parent".into()))?;
    match fs::symlink_metadata(parent) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => return Err(NativeAuthError::CredentialStoreCorrupt),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(parent)
                .map_err(|error| NativeAuthError::CredentialStore(error.to_string()))?;
        }
        Err(error) => return Err(NativeAuthError::CredentialStore(error.to_string())),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
            .map_err(|error| NativeAuthError::CredentialStore(error.to_string()))?;
        let metadata = fs::symlink_metadata(parent)
            .map_err(|error| NativeAuthError::CredentialStore(error.to_string()))?;
        if metadata.uid() != rustix::process::geteuid().as_raw() || metadata.mode() & 0o077 != 0 {
            return Err(NativeAuthError::CredentialStoreCorrupt);
        }
    }
    #[cfg(windows)]
    secure_windows_path(parent)?;
    Ok(())
}

fn read_credential_file(path: &Path) -> Result<NativeCredentialFileV2, NativeAuthError> {
    ensure_secure_credential_directory(path)?;
    let bytes = match read_secure_file(path, MAX_CREDENTIAL_FILE_BYTES) {
        Ok(bytes) => bytes,
        Err(NativeAuthError::CredentialStore(message)) if message == "not found" => {
            return Ok(NativeCredentialFileV2 {
                schema: CREDENTIAL_FILE_SCHEMA_V2.into(),
                entries: BTreeMap::new(),
            });
        }
        Err(error) => return Err(error),
    };
    let file: NativeCredentialFileV2 =
        serde_json::from_slice(&bytes).map_err(|_| NativeAuthError::CredentialStoreCorrupt)?;
    if file.schema != CREDENTIAL_FILE_SCHEMA_V2 || file.entries.len() > 32 {
        return Err(NativeAuthError::CredentialStoreCorrupt);
    }
    Ok(file)
}

#[cfg(unix)]
fn read_secure_file(path: &Path, maximum: u64) -> Result<Vec<u8>, NativeAuthError> {
    use std::io::Read as _;
    use std::os::unix::fs::MetadataExt as _;
    let fd = match rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) => {
            return Err(NativeAuthError::CredentialStore("not found".into()));
        }
        Err(error) => return Err(NativeAuthError::CredentialStore(error.to_string())),
    };
    let mut file = fs::File::from(fd);
    let metadata = file
        .metadata()
        .map_err(|error| NativeAuthError::CredentialStore(error.to_string()))?;
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o077 != 0
        || metadata.len() > maximum
    {
        return Err(NativeAuthError::CredentialStoreCorrupt);
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.read_to_end(&mut bytes)
        .map_err(|error| NativeAuthError::CredentialStore(error.to_string()))?;
    Ok(bytes)
}

#[cfg(windows)]
fn read_secure_file(path: &Path, maximum: u64) -> Result<Vec<u8>, NativeAuthError> {
    use std::io::Read as _;
    use std::os::windows::fs::OpenOptionsExt as _;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    let mut file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(NativeAuthError::CredentialStore("not found".into()));
        }
        Err(error) => return Err(NativeAuthError::CredentialStore(error.to_string())),
    };
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| NativeAuthError::CredentialStore(error.to_string()))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > maximum {
        return Err(NativeAuthError::CredentialStoreCorrupt);
    }
    verify_windows_path(path)?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.read_to_end(&mut bytes)
        .map_err(|error| NativeAuthError::CredentialStore(error.to_string()))?;
    Ok(bytes)
}

fn write_credential_file(
    path: &Path,
    file: &NativeCredentialFileV2,
) -> Result<(), NativeAuthError> {
    ensure_secure_credential_directory(path)?;
    let bytes = serde_json::to_vec(file)
        .map_err(|error| NativeAuthError::CredentialStore(error.to_string()))?;
    if bytes.len() as u64 > MAX_CREDENTIAL_FILE_BYTES {
        return Err(NativeAuthError::CredentialStoreCorrupt);
    }
    if path.exists() {
        let _ = read_secure_file(path, MAX_CREDENTIAL_FILE_BYTES)?;
    }
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::now_v7()));
    #[cfg(unix)]
    let mut output = {
        use std::os::unix::fs::OpenOptionsExt as _;
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|error| NativeAuthError::CredentialStore(error.to_string()))?
    };
    #[cfg(windows)]
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| NativeAuthError::CredentialStore(error.to_string()))?;
    use std::io::Write as _;
    let result = (|| {
        output
            .write_all(&bytes)
            .and_then(|()| output.sync_all())
            .map_err(|error| NativeAuthError::CredentialStore(error.to_string()))?;
        drop(output);
        #[cfg(windows)]
        secure_windows_path(&temporary)?;
        atomic_replace_credential_file(&temporary, path)?;
        #[cfg(windows)]
        verify_windows_path(path)?;
        #[cfg(unix)]
        sync_parent_directory(path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(unix)]
fn atomic_replace_credential_file(
    source: &Path,
    destination: &Path,
) -> Result<(), NativeAuthError> {
    fs::rename(source, destination)
        .map_err(|error| NativeAuthError::CredentialStore(error.to_string()))
}

#[cfg(windows)]
fn atomic_replace_credential_file(
    source: &Path,
    destination: &Path,
) -> Result<(), NativeAuthError> {
    windows_credential_acl::atomic_replace(source, destination)
        .map_err(NativeAuthError::CredentialStore)
}

#[cfg(unix)]
fn sync_parent_directory(path: &Path) -> Result<(), NativeAuthError> {
    let parent = path
        .parent()
        .ok_or_else(|| NativeAuthError::CredentialStore("credential path has no parent".into()))?;
    fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| NativeAuthError::CredentialStore(error.to_string()))
}

fn acquire_credential_file_lock(path: &Path) -> Result<CredentialOperationGuard, NativeAuthError> {
    ensure_secure_credential_directory(path)?;
    #[cfg(unix)]
    let file = {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        let fd = rustix::fs::open(
            path,
            rustix::fs::OFlags::RDWR
                | rustix::fs::OFlags::CREATE
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )
        .map_err(|error| NativeAuthError::CredentialStore(error.to_string()))?;
        let file = fs::File::from(fd);
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|error| NativeAuthError::CredentialStore(error.to_string()))?;
        let metadata = file
            .metadata()
            .map_err(|error| NativeAuthError::CredentialStore(error.to_string()))?;
        if !metadata.is_file()
            || metadata.nlink() != 1
            || metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.mode() & 0o077 != 0
        {
            return Err(NativeAuthError::CredentialStoreCorrupt);
        }
        file
    };
    #[cfg(windows)]
    let file = {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|error| NativeAuthError::CredentialStore(error.to_string()))?;
        secure_windows_path(path)?;
        file
    };
    let deadline = std::time::Instant::now() + StdDuration::from_secs(30);
    loop {
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => return Ok(CredentialOperationGuard(Some(file))),
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(StdDuration::from_millis(25));
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                return Err(NativeAuthError::CredentialStoreTimeout);
            }
            Err(error) => return Err(NativeAuthError::CredentialStore(error.to_string())),
        }
    }
}

#[cfg(windows)]
fn secure_windows_path(path: &Path) -> Result<(), NativeAuthError> {
    windows_credential_acl::secure_path(path).map_err(NativeAuthError::CredentialStore)
}

#[cfg(windows)]
fn verify_windows_path(path: &Path) -> Result<(), NativeAuthError> {
    windows_credential_acl::verify_path(path).map_err(|_| NativeAuthError::CredentialStoreCorrupt)
}

fn normalized_issuer(value: &str, allow_insecure_http: bool) -> Result<Url, NativeAuthError> {
    let mut issuer = Url::parse(value.trim())
        .map_err(|error| NativeAuthError::InvalidConfiguration(error.to_string()))?;
    validate_provider_endpoint(&issuer, allow_insecure_http)?;
    if issuer.query().is_some() || issuer.fragment().is_some() {
        return Err(NativeAuthError::InvalidConfiguration(
            "issuer must not contain a query or fragment".into(),
        ));
    }
    issuer.set_query(None);
    issuer.set_fragment(None);
    if !issuer.path().ends_with('/') {
        issuer.set_path(&format!("{}/", issuer.path()));
    }
    Ok(issuer)
}

fn validate_provider_endpoint(
    value: &Url,
    allow_insecure_http: bool,
) -> Result<Url, NativeAuthError> {
    let secure = value.scheme() == "https";
    let allowed_insecure = allow_insecure_http
        && value.scheme() == "http"
        && matches!(value.host_str(), Some("127.0.0.1" | "localhost"));
    if (!secure && !allowed_insecure)
        || !value.username().is_empty()
        || value.password().is_some()
        || value.host_str().is_none()
        || value.fragment().is_some()
    {
        return Err(NativeAuthError::InvalidConfiguration(
            "provider endpoints must use HTTPS without credentials or fragments".into(),
        ));
    }
    Ok(value.clone())
}

fn validate_loopback_redirect(value: &str) -> Result<Url, NativeAuthError> {
    let url = Url::parse(value)
        .map_err(|error| NativeAuthError::InvalidConfiguration(error.to_string()))?;
    if url.scheme() != "http"
        || url.host_str() != Some("127.0.0.1")
        || url.path().is_empty()
        || url.path() == "/"
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(NativeAuthError::InvalidConfiguration(
            "native redirect URI must be an exact http://127.0.0.1[:port]/<path> URL".into(),
        ));
    }
    Ok(url)
}

async fn receive_loopback_code(
    listener: TcpListener,
    redirect_uri: &Url,
    expected_state: &str,
) -> Result<String, NativeAuthError> {
    let deadline = Instant::now() + StdDuration::from_secs(CALLBACK_TIMEOUT_SECONDS);
    loop {
        let (mut socket, _) = timeout_at(deadline, listener.accept())
            .await
            .map_err(|_| NativeAuthError::LoopbackTimeout)?
            .map_err(NativeAuthError::Loopback)?;
        let mut bytes = Vec::with_capacity(1024);
        loop {
            let mut chunk = [0_u8; 1024];
            let read = timeout_at(deadline, socket.read(&mut chunk))
                .await
                .map_err(|_| NativeAuthError::LoopbackTimeout)?
                .map_err(NativeAuthError::Loopback)?;
            if read == 0 {
                break;
            }
            if bytes.len().saturating_add(read) > MAX_CALLBACK_BYTES {
                send_loopback_response(
                    &mut socket,
                    "413 Content Too Large",
                    "Callback request is too large.",
                )
                .await?;
                return Err(NativeAuthError::InvalidProviderResponse);
            }
            bytes.extend_from_slice(&chunk[..read]);
            if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        let request = String::from_utf8_lossy(&bytes);
        let mut request_line = request
            .lines()
            .next()
            .map(str::split_whitespace)
            .into_iter()
            .flatten();
        let method = request_line.next();
        let target = request_line.next();
        let version = request_line.next();
        let Some(target) = target else {
            send_loopback_response(&mut socket, "400 Bad Request", "Invalid callback request.")
                .await?;
            continue;
        };
        if method != Some("GET")
            || !matches!(version, Some("HTTP/1.1" | "HTTP/1.0"))
            || request_line.next().is_some()
        {
            send_loopback_response(&mut socket, "400 Bad Request", "Invalid callback request.")
                .await?;
            continue;
        }
        let callback = Url::parse(&format!("http://127.0.0.1{target}"))
            .map_err(|_| NativeAuthError::InvalidProviderResponse)?;
        if callback.path() != redirect_uri.path() {
            send_loopback_response(&mut socket, "404 Not Found", "Unknown callback path.").await?;
            continue;
        }
        let mut parameters = BTreeMap::new();
        for (key, value) in callback.query_pairs() {
            if parameters
                .insert(key.into_owned(), value.into_owned())
                .is_some()
            {
                send_loopback_response(
                    &mut socket,
                    "400 Bad Request",
                    "Duplicate callback parameters are not allowed.",
                )
                .await?;
                return Err(NativeAuthError::InvalidProviderResponse);
            }
        }
        if parameters.get("state").map(String::as_str) != Some(expected_state) {
            send_loopback_response(&mut socket, "400 Bad Request", "OAuth state did not match.")
                .await?;
            return Err(NativeAuthError::StateMismatch);
        }
        if let Some(error) = parameters.get("error") {
            send_loopback_response(
                &mut socket,
                "400 Bad Request",
                "Authorization was not completed.",
            )
            .await?;
            return Err(NativeAuthError::OAuth {
                code: error.clone(),
                description: parameters.get("error_description").cloned(),
                status: StatusCode::BAD_REQUEST,
            });
        }
        let code = parameters
            .get("code")
            .filter(|code| !code.is_empty() && code.len() <= 4096)
            .cloned()
            .ok_or(NativeAuthError::InvalidProviderResponse)?;
        send_loopback_response(
            &mut socket,
            "200 OK",
            "Reporch Studio login is complete. You can close this window.",
        )
        .await?;
        return Ok(code);
    }
}

async fn send_loopback_response(
    socket: &mut tokio::net::TcpStream,
    status: &str,
    message: &str,
) -> Result<(), NativeAuthError> {
    let body = format!(
        "<!doctype html><meta charset=utf-8><meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; style-src 'unsafe-inline'\"><title>Reporch Studio</title><style>body{{font:16px system-ui;margin:4rem;max-width:42rem}}</style><h1>Reporch Studio</h1><p>{message}</p>"
    );
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    socket
        .write_all(response.as_bytes())
        .await
        .map_err(NativeAuthError::Loopback)
}

fn random_urlsafe(bytes: usize) -> String {
    let mut value = vec![0_u8; bytes];
    rand::rngs::OsRng.fill_bytes(&mut value);
    URL_SAFE_NO_PAD.encode(value)
}

fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn hex_digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::{Form, State};
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::RwLock;

    #[derive(Default)]
    struct MemoryStore {
        token: RwLock<Option<String>>,
    }

    #[async_trait]
    impl TokenStore for MemoryStore {
        async fn load(
            &self,
            _key: &CredentialKey,
        ) -> Result<Option<StoredTokenSet>, NativeAuthError> {
            let token = self.token.read().await.clone();
            Ok(token.map(|token| serde_json::from_str(&token).unwrap()))
        }

        async fn save(
            &self,
            _key: &CredentialKey,
            token: &StoredTokenSet,
        ) -> Result<(), NativeAuthError> {
            *self.token.write().await = Some(serde_json::to_string(token).unwrap());
            Ok(())
        }

        async fn delete(&self, _key: &CredentialKey) -> Result<(), NativeAuthError> {
            *self.token.write().await = None;
            Ok(())
        }
    }

    fn test_client(dpop_required: bool, revocation_endpoint: Option<Url>) -> NativeAuthClient {
        let config = if dpop_required {
            NativeAuthConfig::device_dpop(
                "https://reporch.test/oauth",
                "native-test",
                vec!["offline_access".into(), "openid".into()],
                false,
            )
        } else {
            NativeAuthConfig::device(
                "https://reporch.test/oauth",
                "native-test",
                vec!["offline_access".into(), "openid".into()],
                false,
            )
        }
        .unwrap();
        NativeAuthClient {
            config,
            metadata: ProviderMetadata {
                issuer: "https://reporch.test/oauth".into(),
                authorization_endpoint: Url::parse("https://reporch.test/oauth/authorize/")
                    .unwrap(),
                token_endpoint: Url::parse("https://reporch.test/oauth/token/").unwrap(),
                device_authorization_endpoint: None,
                revocation_endpoint,
                dpop_signing_alg_values_supported: if dpop_required {
                    vec!["ES256".into()]
                } else {
                    Vec::new()
                },
            },
            http: reqwest::Client::new(),
        }
    }

    fn token_wire(token_type: &str, refresh_token: Option<&str>) -> TokenResponseWire {
        TokenResponseWire {
            access_token: "access-token".into(),
            token_type: token_type.into(),
            expires_in: 300,
            refresh_token: refresh_token.map(str::to_owned),
            id_token: None,
            scope: Some("openid offline_access".into()),
        }
    }

    #[test]
    fn dpop_token_responses_fail_closed_on_bearer_downgrade() {
        let dpop = test_client(true, None);
        assert!(matches!(
            dpop.token_from_wire(
                token_wire("Bearer", Some("refresh-token")),
                Some(DeviceKeyV1::generate().unwrap()),
            ),
            Err(NativeAuthError::InvalidProviderResponse)
        ));
        let accepted = dpop
            .token_from_wire(
                token_wire("DPoP", Some("refresh-token")),
                Some(DeviceKeyV1::generate().unwrap()),
            )
            .unwrap();
        assert_eq!(accepted.token_type, "DPoP");

        let bearer = test_client(false, None);
        assert!(matches!(
            bearer.token_from_wire(token_wire("DPoP", Some("refresh-token")), None),
            Err(NativeAuthError::InvalidProviderResponse)
        ));
        assert_eq!(
            bearer
                .token_from_wire(token_wire("Bearer", Some("refresh-token")), None)
                .unwrap()
                .token_type,
            "Bearer"
        );
    }

    #[test]
    fn refresh_rotation_requires_a_new_distinct_token() {
        let client = test_client(true, None);
        let key = Some(DeviceKeyV1::generate().unwrap());
        let missing = client
            .token_from_wire(token_wire("DPoP", None), key.clone())
            .unwrap();
        assert!(matches!(
            validate_refresh_rotation(&missing, "previous-refresh"),
            Err(NativeAuthError::RefreshRotationRequired)
        ));
        let repeated = client
            .token_from_wire(token_wire("DPoP", Some("previous-refresh")), key.clone())
            .unwrap();
        assert!(matches!(
            validate_refresh_rotation(&repeated, "previous-refresh"),
            Err(NativeAuthError::RefreshRotationRequired)
        ));
        let rotated = client
            .token_from_wire(token_wire("DPoP", Some("next-refresh")), key)
            .unwrap();
        validate_refresh_rotation(&rotated, "previous-refresh").unwrap();
    }

    #[tokio::test]
    async fn failed_remote_revocation_preserves_the_local_device_credential() {
        async fn unavailable() -> (StatusCode, Json<serde_json::Value>) {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "temporarily_unavailable"})),
            )
        }

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let endpoint =
            Url::parse(&format!("http://{}/revoke", listener.local_addr().unwrap())).unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, Router::new().route("/revoke", post(unavailable)))
                .await
                .unwrap()
        });
        let mut client = test_client(true, Some(endpoint));
        client.config.allow_insecure_http = true;
        let store = MemoryStore::default();
        let token = client
            .token_from_wire(
                token_wire("DPoP", Some("refresh-token")),
                Some(DeviceKeyV1::generate().unwrap()),
            )
            .unwrap();
        let key = client.config.credential_key();
        store.save(&key, &token).await.unwrap();

        assert!(client.logout(&store).await.is_err());
        assert!(store.load(&key).await.unwrap().is_some());
        server.abort();
    }

    #[test]
    fn pkce_matches_the_rfc_7636_vector() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn redirect_uri_is_strictly_loopback_and_exact() {
        assert!(validate_loopback_redirect("http://127.0.0.1/callback").is_ok());
        assert!(validate_loopback_redirect("http://127.0.0.1:48173/callback").is_ok());
        assert!(validate_loopback_redirect("http://localhost:48173/callback").is_err());
        assert!(validate_loopback_redirect("https://127.0.0.1:48173/callback").is_err());
        assert!(validate_loopback_redirect("http://127.0.0.1:48173/").is_err());
    }

    #[tokio::test]
    async fn status_never_exposes_stored_tokens() {
        let config = NativeAuthConfig::device(
            "https://reporch.test/oauth",
            "native-test",
            vec!["offline_access".into(), "openid".into()],
            false,
        )
        .unwrap();
        let client = NativeAuthClient {
            config,
            metadata: ProviderMetadata {
                issuer: "https://reporch.test/oauth".into(),
                authorization_endpoint: Url::parse("https://reporch.test/oauth/authorize/")
                    .unwrap(),
                token_endpoint: Url::parse("https://reporch.test/oauth/token/").unwrap(),
                device_authorization_endpoint: None,
                revocation_endpoint: None,
                dpop_signing_alg_values_supported: Vec::new(),
            },
            http: reqwest::Client::new(),
        };
        let store = Arc::new(MemoryStore::default());
        store
            .save(
                &client.config.credential_key(),
                &StoredTokenSet {
                    schema: TOKEN_SCHEMA_V1.into(),
                    issuer: client.config.issuer().into(),
                    client_id: client.config.client_id.clone(),
                    access_token: "must-never-be-returned".into(),
                    refresh_token: Some("also-secret".into()),
                    id_token: None,
                    token_type: "Bearer".into(),
                    expires_at: Utc::now() + Duration::minutes(10),
                    scopes: vec!["openid".into()],
                    device_key: None,
                },
            )
            .await
            .unwrap();
        let serialized =
            serde_json::to_string(&client.session_status(store.as_ref()).await.unwrap()).unwrap();
        assert!(!serialized.contains("must-never-be-returned"));
        assert!(!serialized.contains("also-secret"));
        assert!(serialized.contains("authenticated"));
    }

    #[tokio::test]
    async fn offline_status_rejects_a_token_from_another_client() {
        let config = NativeAuthConfig::device(
            "https://reporch.test/oauth",
            "native-test",
            vec!["offline_access".into(), "openid".into()],
            false,
        )
        .unwrap();
        let store = MemoryStore::default();
        store
            .save(
                &config.credential_key(),
                &StoredTokenSet {
                    schema: TOKEN_SCHEMA_V1.into(),
                    issuer: config.issuer().into(),
                    client_id: "substituted-client".into(),
                    access_token: "access-token".into(),
                    refresh_token: Some("refresh-token".into()),
                    id_token: None,
                    token_type: "Bearer".into(),
                    expires_at: Utc::now() + Duration::minutes(10),
                    scopes: vec!["openid".into()],
                    device_key: None,
                },
            )
            .await
            .unwrap();

        assert!(matches!(
            config.local_session_status(&store).await,
            Err(NativeAuthError::CredentialStoreCorrupt)
        ));
    }

    #[tokio::test]
    async fn local_session_can_be_removed_without_provider_discovery() {
        let config = NativeAuthConfig::device(
            "https://reporch.test/oauth",
            "native-test",
            vec!["offline_access".into(), "openid".into()],
            false,
        )
        .unwrap();
        let store = MemoryStore::default();
        store
            .save(
                &config.credential_key(),
                &StoredTokenSet {
                    schema: TOKEN_SCHEMA_V1.into(),
                    issuer: config.issuer().into(),
                    client_id: config.client_id().into(),
                    access_token: "access-token".into(),
                    refresh_token: Some("refresh-token".into()),
                    id_token: None,
                    token_type: "Bearer".into(),
                    expires_at: Utc::now() + Duration::minutes(10),
                    scopes: vec!["openid".into()],
                    device_key: None,
                },
            )
            .await
            .unwrap();

        config.clear_local_session(&store).await.unwrap();
        assert!(
            !config
                .local_session_status(&store)
                .await
                .unwrap()
                .authenticated
        );
    }

    #[tokio::test]
    async fn credential_fingerprint_is_stable_non_secret_and_account_bound() {
        let config = NativeAuthConfig::device(
            "https://reporch.test/oauth",
            "native-test",
            vec!["offline_access".into(), "openid".into()],
            false,
        )
        .unwrap();
        let store = MemoryStore::default();
        let token = |id_token: &str| StoredTokenSet {
            schema: TOKEN_SCHEMA_V1.into(),
            issuer: config.issuer().into(),
            client_id: config.client_id().into(),
            access_token: "access-token".into(),
            refresh_token: Some("refresh-token".into()),
            id_token: Some(id_token.into()),
            token_type: "Bearer".into(),
            expires_at: Utc::now() + Duration::minutes(10),
            scopes: vec!["openid".into()],
            device_key: None,
        };
        store
            .save(&config.credential_key(), &token("account-a-id-token"))
            .await
            .unwrap();
        let first = config
            .local_credential_fingerprint(&store)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.len(), 64);
        assert!(!first.contains("account-a-id-token"));
        assert_eq!(
            config
                .local_credential_fingerprint(&store)
                .await
                .unwrap()
                .as_deref(),
            Some(first.as_str())
        );
        store
            .save(&config.credential_key(), &token("account-b-id-token"))
            .await
            .unwrap();
        assert_ne!(
            config
                .local_credential_fingerprint(&store)
                .await
                .unwrap()
                .as_deref(),
            Some(first.as_str())
        );
    }

    #[derive(Clone)]
    struct MockProviderState {
        issuer: String,
        token_polls: Arc<AtomicUsize>,
        fail_first_poll: bool,
        dpop: bool,
    }

    async fn discovery(State(state): State<MockProviderState>) -> Json<serde_json::Value> {
        Json(json!({
            "issuer": state.issuer,
            "authorization_endpoint": format!("{}/authorize", state.issuer),
            "token_endpoint": format!("{}/token", state.issuer),
            "device_authorization_endpoint": format!("{}/device-authorization", state.issuer),
            "revocation_endpoint": format!("{}/revoke", state.issuer),
            "dpop_signing_alg_values_supported": ["ES256"]
        }))
    }

    async fn device_authorization(
        State(state): State<MockProviderState>,
        Form(form): Form<HashMap<String, String>>,
    ) -> (StatusCode, Json<serde_json::Value>) {
        assert_eq!(
            form.get("client_id").map(String::as_str),
            Some("native-test")
        );
        (
            StatusCode::OK,
            Json(json!({
                "device_code": "server-only-device-code",
                "user_code": "ABCD-EFGH",
                "verification_uri": format!("{}/device", state.issuer),
                "verification_uri_complete": format!("{}/device?user_code=ABCD-EFGH", state.issuer),
                "expires_in": 30,
                "interval": 1
            })),
        )
    }

    async fn token(
        State(state): State<MockProviderState>,
        Form(form): Form<HashMap<String, String>>,
    ) -> (StatusCode, Json<serde_json::Value>) {
        assert_eq!(
            form.get("grant_type").map(String::as_str),
            Some(DEVICE_GRANT_TYPE)
        );
        assert_eq!(
            form.get("device_code").map(String::as_str),
            Some("server-only-device-code")
        );
        let poll = state.token_polls.fetch_add(1, Ordering::SeqCst);
        if state.fail_first_poll && poll == 0 {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error": "temporarily_unavailable"})),
            );
        }
        if poll == usize::from(state.fail_first_poll) {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "authorization_pending"})),
            );
        }
        (
            StatusCode::OK,
            Json(json!({
                "access_token": "mock-access-token",
                "refresh_token": "mock-refresh-token",
                "token_type": if state.dpop { "DPoP" } else { "Bearer" },
                "expires_in": 600,
                "scope": "openid offline_access"
            })),
        )
    }

    #[tokio::test]
    async fn device_flow_discovers_polls_and_persists_without_exposing_secrets() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let issuer = format!("http://{address}");
        let state = MockProviderState {
            issuer: issuer.clone(),
            token_polls: Arc::new(AtomicUsize::new(0)),
            fail_first_poll: false,
            dpop: false,
        };
        let app = Router::new()
            .route("/.well-known/openid-configuration", get(discovery))
            .route("/device-authorization", post(device_authorization))
            .route("/token", post(token))
            .with_state(state.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let config = NativeAuthConfig::device(
            &issuer,
            "native-test",
            vec!["openid".into(), "offline_access".into()],
            true,
        )
        .unwrap();
        let client = NativeAuthClient::discover(config).await.unwrap();
        let prompt = client.request_device_authorization().await.unwrap();
        let serialized_prompt = serde_json::to_string(&prompt).unwrap();
        assert!(!serialized_prompt.contains("server-only-device-code"));

        let store = MemoryStore::default();
        let status = client
            .finish_device_authorization(&prompt, &store)
            .await
            .unwrap();
        assert!(status.authenticated);
        assert_eq!(state.token_polls.load(Ordering::SeqCst), 2);
        assert_eq!(
            client.access_token(&store).await.unwrap(),
            "mock-access-token"
        );
        let serialized_status = serde_json::to_string(&status).unwrap();
        assert!(!serialized_status.contains("mock-access-token"));
        assert!(!serialized_status.contains("mock-refresh-token"));

        server.abort();
    }

    #[tokio::test]
    async fn device_flow_retries_a_transient_provider_failure_without_losing_the_device_key() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let issuer = format!("http://{address}");
        let state = MockProviderState {
            issuer: issuer.clone(),
            token_polls: Arc::new(AtomicUsize::new(0)),
            fail_first_poll: true,
            dpop: true,
        };
        let app = Router::new()
            .route("/.well-known/openid-configuration", get(discovery))
            .route("/device-authorization", post(device_authorization))
            .route("/token", post(token))
            .with_state(state.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let config = NativeAuthConfig::device_dpop(
            &issuer,
            "native-test",
            vec!["openid".into(), "offline_access".into()],
            true,
        )
        .unwrap();
        let client = NativeAuthClient::discover(config).await.unwrap();
        let prompt = client.request_device_authorization().await.unwrap();
        let store = MemoryStore::default();
        let status = client
            .finish_device_authorization(&prompt, &store)
            .await
            .unwrap();

        assert!(status.authenticated);
        assert_eq!(state.token_polls.load(Ordering::SeqCst), 3);
        assert_eq!(
            client.access_token(&store).await.unwrap(),
            "mock-access-token"
        );
        server.abort();
    }

    #[tokio::test]
    async fn loopback_callback_requires_the_exact_path_and_state() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let redirect_uri =
            Url::parse(&format!("http://127.0.0.1:{}/callback", address.port())).unwrap();
        let receiver = tokio::spawn(async move {
            receive_loopback_code(listener, &redirect_uri, "expected-state").await
        });

        let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
        socket
            .write_all(b"GET /callback?code=authorization-code&state=expected-state HTTP/1.1\r\n")
            .await
            .unwrap();
        tokio::task::yield_now().await;
        socket.write_all(b"Host: 127.0.0.1\r\n\r\n").await.unwrap();
        let mut response = Vec::new();
        socket.read_to_end(&mut response).await.unwrap();

        assert_eq!(receiver.await.unwrap().unwrap(), "authorization-code");
        assert!(String::from_utf8(response).unwrap().contains("200 OK"));
    }

    #[tokio::test]
    async fn loopback_callback_rejects_state_substitution() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let redirect_uri =
            Url::parse(&format!("http://127.0.0.1:{}/callback", address.port())).unwrap();
        let receiver = tokio::spawn(async move {
            receive_loopback_code(listener, &redirect_uri, "expected-state").await
        });

        let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
        socket
            .write_all(
                b"GET /callback?code=authorization-code&state=attacker-state HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
            )
            .await
            .unwrap();
        let mut response = Vec::new();
        socket.read_to_end(&mut response).await.unwrap();

        assert!(matches!(
            receiver.await.unwrap(),
            Err(NativeAuthError::StateMismatch)
        ));
        assert!(
            String::from_utf8(response)
                .unwrap()
                .contains("400 Bad Request")
        );
    }

    #[tokio::test]
    async fn loopback_callback_rejects_duplicate_security_parameters() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let redirect_uri =
            Url::parse(&format!("http://127.0.0.1:{}/callback", address.port())).unwrap();
        let receiver = tokio::spawn(async move {
            receive_loopback_code(listener, &redirect_uri, "expected-state").await
        });

        let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
        socket
            .write_all(
                b"GET /callback?code=authorization-code&state=expected-state&state=substitute HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
            )
            .await
            .unwrap();
        let mut response = Vec::new();
        socket.read_to_end(&mut response).await.unwrap();

        assert!(matches!(
            receiver.await.unwrap(),
            Err(NativeAuthError::InvalidProviderResponse)
        ));
        assert!(
            String::from_utf8(response)
                .unwrap()
                .contains("400 Bad Request")
        );
    }

    #[tokio::test]
    async fn provider_json_is_bounded_before_deserialization() {
        async fn oversized_discovery() -> String {
            "x".repeat(MAX_PROVIDER_RESPONSE_BYTES + 1)
        }

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new().route(
            "/.well-known/openid-configuration",
            get(oversized_discovery),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let config = NativeAuthConfig::device(
            &format!("http://{address}"),
            "native-test",
            vec!["openid".into()],
            true,
        )
        .unwrap();

        assert!(matches!(
            NativeAuthClient::discover(config).await,
            Err(NativeAuthError::InvalidProviderResponse)
        ));
        server.abort();
    }

    #[tokio::test]
    async fn credential_store_operation_times_out_without_blocking_the_cli() {
        let started = std::time::Instant::now();
        let result = run_keyring_operation_with_timeout(
            StdDuration::from_millis(10),
            || -> Result<(), KeyringError> {
                std::thread::sleep(StdDuration::from_millis(250));
                Ok(())
            },
        )
        .await;

        assert!(matches!(
            result,
            Err(NativeAuthError::CredentialStoreTimeout)
        ));
        assert!(started.elapsed() < StdDuration::from_millis(150));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn native_credential_file_rejects_wide_permissions_hardlinks_and_symlinks() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let root = tempfile::tempdir().unwrap();
        let store = NativeFileTokenStore {
            path: root.path().join("config/reporch/credentials-v2.json"),
        };
        let key = CredentialKey {
            service: KEYRING_SERVICE.into(),
            account: "native-security-test".into(),
        };
        let token = StoredTokenSet {
            schema: TOKEN_SCHEMA_V2.into(),
            issuer: "https://reporch.test/oauth".into(),
            client_id: "reporch-studio-cli-v1".into(),
            access_token: "native-security-access-token".into(),
            refresh_token: Some("native-security-refresh-token".into()),
            id_token: None,
            token_type: "DPoP".into(),
            expires_at: Utc::now() + Duration::minutes(5),
            scopes: vec!["openid".into()],
            device_key: Some(DeviceKeyV1::generate().unwrap()),
        };
        store.save(&key, &token).await.unwrap();
        let metadata = fs::metadata(&store.path).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        assert_eq!(
            fs::metadata(store.path.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );

        fs::set_permissions(&store.path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            store.load(&key).await,
            Err(NativeAuthError::CredentialStoreCorrupt)
        ));
        fs::set_permissions(&store.path, fs::Permissions::from_mode(0o600)).unwrap();

        fs::hard_link(&store.path, root.path().join("credential-copy")).unwrap();
        assert!(matches!(
            store.load(&key).await,
            Err(NativeAuthError::CredentialStoreCorrupt)
        ));
        fs::remove_file(root.path().join("credential-copy")).unwrap();
        fs::remove_file(&store.path).unwrap();
        let target = root.path().join("attacker-controlled");
        fs::write(&target, b"{}").unwrap();
        symlink(&target, &store.path).unwrap();
        assert!(store.load(&key).await.is_err());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn native_credential_file_round_trips_with_a_protected_user_only_dacl() {
        qualification_native_file_canary().await.unwrap();
    }
}
