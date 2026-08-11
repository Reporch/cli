#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::time::Duration as StdDuration;

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Duration, Utc};
use keyring::v1::{Entry, Error as KeyringError};
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
const MAX_TOKEN_BYTES: usize = 16 * 1024;

#[derive(Clone)]
pub struct NativeAuthConfig {
    issuer: Url,
    client_id: String,
    scopes: Vec<String>,
    redirect_uri: Option<Url>,
    allow_insecure_http: bool,
}

impl NativeAuthConfig {
    pub fn device(
        issuer: &str,
        client_id: &str,
        scopes: Vec<String>,
        allow_insecure_http: bool,
    ) -> Result<Self, NativeAuthError> {
        Self::new(issuer, client_id, scopes, None, allow_insecure_http)
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
        )
    }

    fn new(
        issuer: &str,
        client_id: &str,
        scopes: Vec<String>,
        redirect_uri: Option<&str>,
        allow_insecure_http: bool,
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
        if token.schema != TOKEN_SCHEMA_V1
            || token.issuer != self.issuer()
            || token.client_id != self.client_id
            || token.access_token.is_empty()
            || token.access_token.len() > MAX_TOKEN_BYTES
            || token.token_type != "Bearer"
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
        let response = self
            .http
            .post(endpoint.clone())
            .form(&[
                ("client_id", self.config.client_id.as_str()),
                ("scope", scopes.as_str()),
            ])
            .send()
            .await
            .map_err(NativeAuthError::Network)?;
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
            let response = self
                .http
                .post(self.metadata.token_endpoint.clone())
                .form(&[
                    ("grant_type", DEVICE_GRANT_TYPE),
                    ("device_code", prompt.device_code.as_str()),
                    ("client_id", self.config.client_id.as_str()),
                ])
                .send()
                .await
                .map_err(NativeAuthError::Network)?;
            if response.status().is_success() {
                let token = self.decode_initial_token(response).await?;
                store.save(&self.config.credential_key(), &token).await?;
                return Ok(self.config.status_from_token(Some(&token)));
            }
            let status = response.status();
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
        let token = self.decode_initial_token(response).await?;
        store.save(&self.config.credential_key(), &token).await?;
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
        let mut token = store
            .load(&self.config.credential_key())
            .await?
            .ok_or(NativeAuthError::NotAuthenticated)?;
        self.config.validate_stored_token(&token)?;
        if token.expires_at <= Utc::now() + Duration::seconds(TOKEN_EXPIRY_SKEW_SECONDS) {
            token = self.refresh_token(&token).await?;
            store.save(&self.config.credential_key(), &token).await?;
        }
        Ok(token.access_token)
    }

    pub async fn logout<S: TokenStore + ?Sized>(&self, store: &S) -> Result<bool, NativeAuthError> {
        let token = store.load(&self.config.credential_key()).await?;
        if let Some(token) = token.as_ref()
            && let Err(error) = self.config.validate_stored_token(token)
        {
            store.delete(&self.config.credential_key()).await?;
            return Err(error);
        }
        let remotely_revoked = if let (Some(token), Some(endpoint)) =
            (token.as_ref(), self.metadata.revocation_endpoint.as_ref())
        {
            let revoke_token = token
                .refresh_token
                .as_deref()
                .unwrap_or(&token.access_token);
            self.http
                .post(endpoint.clone())
                .form(&[
                    ("token", revoke_token),
                    ("client_id", self.config.client_id.as_str()),
                ])
                .send()
                .await
                .is_ok_and(|response| response.status().is_success())
        } else {
            false
        };
        store.delete(&self.config.credential_key()).await?;
        Ok(remotely_revoked)
    }

    async fn refresh_token(
        &self,
        previous: &StoredTokenSet,
    ) -> Result<StoredTokenSet, NativeAuthError> {
        let refresh_token = previous
            .refresh_token
            .as_deref()
            .ok_or(NativeAuthError::RefreshUnavailable)?;
        let response = self
            .http
            .post(self.metadata.token_endpoint.clone())
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
                ("client_id", self.config.client_id.as_str()),
            ])
            .send()
            .await
            .map_err(NativeAuthError::Network)?;
        if !response.status().is_success() {
            return Err(provider_error(response).await);
        }
        let wire: TokenResponseWire = decode_provider_json(response).await?;
        self.token_from_wire(wire, previous.refresh_token.clone())
    }

    async fn decode_initial_token(
        &self,
        response: reqwest::Response,
    ) -> Result<StoredTokenSet, NativeAuthError> {
        let wire: TokenResponseWire = decode_provider_json(response).await?;
        let token = self.token_from_wire(wire, None)?;
        if token.refresh_token.is_none() {
            return Err(NativeAuthError::RefreshUnavailable);
        }
        Ok(token)
    }

    fn token_from_wire(
        &self,
        wire: TokenResponseWire,
        fallback_refresh_token: Option<String>,
    ) -> Result<StoredTokenSet, NativeAuthError> {
        if wire.access_token.is_empty()
            || wire.access_token.len() > MAX_TOKEN_BYTES
            || !wire.token_type.eq_ignore_ascii_case("bearer")
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
            schema: TOKEN_SCHEMA_V1.into(),
            issuer: self.config.issuer().into(),
            client_id: self.config.client_id.clone(),
            access_token: wire.access_token,
            refresh_token: wire.refresh_token.or(fallback_refresh_token),
            id_token: wire.id_token,
            token_type: "Bearer".into(),
            expires_at: Utc::now() + Duration::seconds(wire.expires_in as i64),
            scopes,
        })
    }
}

#[derive(Clone)]
pub struct CredentialKey {
    service: String,
    account: String,
}

#[async_trait]
pub trait TokenStore: Send + Sync {
    async fn load(&self, key: &CredentialKey) -> Result<Option<StoredTokenSet>, NativeAuthError>;
    async fn save(
        &self,
        key: &CredentialKey,
        token: &StoredTokenSet,
    ) -> Result<(), NativeAuthError>;
    async fn delete(&self, key: &CredentialKey) -> Result<(), NativeAuthError>;
}

#[derive(Default)]
pub struct KeyringTokenStore;

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

#[async_trait]
impl TokenStore for KeyringTokenStore {
    async fn load(&self, key: &CredentialKey) -> Result<Option<StoredTokenSet>, NativeAuthError> {
        let key = key.clone();
        let value = tokio::task::spawn_blocking(move || {
            let entry = Entry::new(&key.service, &key.account)?;
            match entry.get_password() {
                Ok(value) => Ok(Some(value)),
                Err(KeyringError::NoEntry) => Ok(None),
                Err(error) => Err(error),
            }
        })
        .await
        .map_err(|error| NativeAuthError::CredentialStore(error.to_string()))?
        .map_err(|error: KeyringError| NativeAuthError::CredentialStore(error.to_string()))?;
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
        tokio::task::spawn_blocking(move || {
            Entry::new(&key.service, &key.account)?.set_password(&value)
        })
        .await
        .map_err(|error| NativeAuthError::CredentialStore(error.to_string()))?
        .map_err(|error| NativeAuthError::CredentialStore(error.to_string()))
    }

    async fn delete(&self, key: &CredentialKey) -> Result<(), NativeAuthError> {
        let key = key.clone();
        tokio::task::spawn_blocking(move || {
            let entry = Entry::new(&key.service, &key.account)?;
            match entry.delete_credential() {
                Ok(()) | Err(KeyringError::NoEntry) => Ok(()),
                Err(error) => Err(error),
            }
        })
        .await
        .map_err(|error| NativeAuthError::CredentialStore(error.to_string()))?
        .map_err(|error: KeyringError| NativeAuthError::CredentialStore(error.to_string()))
    }
}

#[derive(Serialize, Deserialize)]
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

    #[derive(Clone)]
    struct MockProviderState {
        issuer: String,
        token_polls: Arc<AtomicUsize>,
    }

    async fn discovery(State(state): State<MockProviderState>) -> Json<serde_json::Value> {
        Json(json!({
            "issuer": state.issuer,
            "authorization_endpoint": format!("{}/authorize", state.issuer),
            "token_endpoint": format!("{}/token", state.issuer),
            "device_authorization_endpoint": format!("{}/device-authorization", state.issuer),
            "revocation_endpoint": format!("{}/revoke", state.issuer)
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
        if state.token_polls.fetch_add(1, Ordering::SeqCst) == 0 {
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
                "token_type": "Bearer",
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
}
