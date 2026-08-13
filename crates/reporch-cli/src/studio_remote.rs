use std::collections::HashMap;
use std::fs;
use std::future::Future;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use chrono::{DateTime, Utc};
use clap::{ArgAction, Args as ClapArgs, ValueEnum};
use futures_util::StreamExt;
use reqwest::header::{ACCEPT, CONTENT_LENGTH, CONTENT_TYPE, HeaderName, HeaderValue, IF_MATCH};
use reqwest::{RequestBuilder, Response, StatusCode, Url};
use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use studio_contracts::{
    ApiErrorResponse, BeginFileUploadRequest, BeginFileUploadResponse, CommitDetailResponse,
    CommitPage, CommitResponse, CommitWorkingCopyRequestV1, CreateCommitRequest,
    CreateProjectRequest, CreateReleaseRequest, CreateReviewDecisionRequest, CreateWaiverRequest,
    EVENT_SCHEMA_V1, EnqueueValidationRequest, EventEnvelopeV1, FileDownloadResponse,
    FileEntryPage, FileUploadResponse, FileUploadStatus, FileUploadStrategyV1,
    IdentityDirectoryPageV1, ProjectMembershipPage, ProjectMembershipResponse, ProjectPage,
    ProjectResponse, PublicationResponse, PublicationStatus, QuotaStatusV1,
    ReleaseDownloadResponse, ReleasePage, ReleaseResponse, ReleaseStatus, ReviewPage,
    ReviewPoolPageV1, ReviewPoolRequestResponseV1, ReviewResponse, RevokeWaiverRequest,
    StudioCapabilitiesV1, SubmitReviewRequest, UpdateWorkingCopyRequestV1,
    UpsertProjectMembershipRequest, ValidationRunDetailResponse, ValidationRunResponse,
    ValidationRunStatus, WaiverPage, WaiverResponse, WorkingCopyReadinessV1, WorkingCopyV1,
};
use studio_core::{
    ManifestFile, ProblemType, ProjectRole, ReleaseManifestV1, ReviewDecisionKindV1, Sha256Digest,
    SubjectRef, validate_manifest,
};
use studio_native_auth::{KeyringTokenStore, NativeAuthClient};
use tempfile::NamedTempFile;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::io::ReaderStream;
use url::Host;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::{NativeAuthOptions, device_auth_config};

const DEFAULT_API_URL: &str = "https://studio.reporch.com";
const MAX_JSON_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
const MAX_ERROR_RESPONSE_BYTES: usize = 256 * 1024;
const MAX_BLOCK_BYTES: u64 = 64 * 1024 * 1024;
const DEFAULT_WAIT_SECONDS: u64 = 30 * 60;
const MAX_TRANSIENT_POLL_RETRIES: u32 = 8;
const MAX_SSE_BUFFER_BYTES: usize = 1024 * 1024;
const MAX_SSE_DATA_BYTES: usize = 1024 * 1024;

#[derive(Debug, Error)]
#[doc(hidden)]
pub enum StudioApiRequestError {
    #[error("Studio API {error_code}: {message} (trace {trace_id})")]
    Api {
        status: StatusCode,
        error_code: String,
        message: String,
        retryable: bool,
        trace_id: String,
    },
    #[error("Studio API request failed with HTTP {status}")]
    Http { status: StatusCode },
    #[error("Studio API transport failed: {source}")]
    Transport {
        #[source]
        source: reqwest::Error,
    },
}

#[doc(hidden)]
pub struct RemoteErrorMetadata {
    pub error_code: String,
    pub retryable: bool,
    pub trace_id: Option<String>,
    pub status: Option<StatusCode>,
}

#[doc(hidden)]
pub fn remote_error_metadata(error: &anyhow::Error) -> Option<RemoteErrorMetadata> {
    error.chain().find_map(|cause| {
        let remote = cause.downcast_ref::<StudioApiRequestError>()?;
        Some(match remote {
            StudioApiRequestError::Api {
                status,
                error_code,
                retryable,
                trace_id,
                ..
            } => RemoteErrorMetadata {
                error_code: error_code.clone(),
                retryable: *retryable || is_transient_http_status(*status),
                trace_id: Some(trace_id.clone()),
                status: Some(*status),
            },
            StudioApiRequestError::Http { status } => RemoteErrorMetadata {
                error_code: "studio.http_error".into(),
                retryable: is_transient_http_status(*status),
                trace_id: None,
                status: Some(*status),
            },
            StudioApiRequestError::Transport { .. } => RemoteErrorMetadata {
                error_code: "infrastructure.transport".into(),
                retryable: true,
                trace_id: None,
                status: None,
            },
        })
    })
}

impl StudioApiRequestError {
    fn is_transient(&self) -> bool {
        match self {
            Self::Api {
                status, retryable, ..
            } => *retryable || is_transient_http_status(*status),
            Self::Http { status } => is_transient_http_status(*status),
            Self::Transport { source } => {
                source.is_timeout() || source.is_connect() || source.is_request()
            }
        }
    }
}

#[derive(Debug, Clone, ClapArgs)]
pub struct RemoteConnectionOptions {
    #[arg(
        long,
        env = "REPORCH_STUDIO_API_URL",
        default_value = DEFAULT_API_URL
    )]
    pub api_url: String,
    #[command(flatten)]
    pub auth: NativeAuthOptions,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum RemoteProblemType {
    Standard,
    Scored,
    Interactive,
    OutputOnly,
    Library,
    Grader,
}

impl From<RemoteProblemType> for ProblemType {
    fn from(value: RemoteProblemType) -> Self {
        match value {
            RemoteProblemType::Standard => Self::Standard,
            RemoteProblemType::Scored => Self::Scored,
            RemoteProblemType::Interactive => Self::Interactive,
            RemoteProblemType::OutputOnly => Self::OutputOnly,
            RemoteProblemType::Library => Self::Library,
            RemoteProblemType::Grader => Self::Grader,
        }
    }
}

#[derive(Debug, Clone, ClapArgs)]
pub struct CreateOptions {
    #[command(flatten)]
    pub connection: RemoteConnectionOptions,
    #[arg(long)]
    pub title: String,
    #[arg(long, value_enum, default_value_t = RemoteProblemType::Standard)]
    pub problem_type: RemoteProblemType,
    #[arg(long)]
    pub organization_id: Option<String>,
    /// Initialize a bound standard-problem checkout after the server project is created.
    #[arg(long)]
    pub directory: Option<PathBuf>,
    /// Reuse this value after an ambiguous network failure.
    #[arg(long)]
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Clone, ClapArgs)]
pub struct PullOptions {
    #[command(flatten)]
    pub connection: RemoteConnectionOptions,
    #[arg(long)]
    pub project_id: Uuid,
    /// Pull the latest immutable commit when omitted.
    #[arg(long)]
    pub commit_id: Option<Uuid>,
    /// Must be absent or an empty real directory; the checkout is installed atomically.
    #[arg(long)]
    pub directory: PathBuf,
}

#[derive(Debug, Clone, ClapArgs)]
pub struct PushOptions {
    #[command(flatten)]
    pub connection: RemoteConnectionOptions,
    /// Deprecated 0.1.x manifest input. Omit to compile reporch.yaml automatically.
    #[arg(long)]
    pub manifest: Option<PathBuf>,
    #[arg(long)]
    pub source_root: Option<PathBuf>,
    #[arg(long, default_value = "CLI push")]
    pub message: String,
    #[arg(long, default_value_t = 10 * 60)]
    pub timeout_seconds: u64,
}

#[derive(Debug, Clone, ClapArgs)]
pub struct ValidateOptions {
    #[command(flatten)]
    pub connection: RemoteConnectionOptions,
    #[arg(long)]
    pub project_id: Option<Uuid>,
    #[arg(long)]
    pub commit_id: Option<Uuid>,
    /// Reuse this value after an ambiguous network failure.
    #[arg(long)]
    pub idempotency_key: Option<String>,
    #[arg(long, default_value_t = true, action = ArgAction::Set)]
    pub wait: bool,
    #[arg(long, default_value_t = DEFAULT_WAIT_SECONDS)]
    pub timeout_seconds: u64,
}

#[derive(Debug, Clone, ClapArgs)]
pub struct ValidationInspectOptions {
    #[command(flatten)]
    pub connection: RemoteConnectionOptions,
    #[arg(long)]
    pub project_id: Option<Uuid>,
    #[arg(long)]
    pub validation_run_id: Option<Uuid>,
    #[arg(long, default_value_t = DEFAULT_WAIT_SECONDS)]
    pub timeout_seconds: u64,
}

#[derive(Debug, Clone, ClapArgs)]
pub struct PackageOptions {
    #[command(flatten)]
    pub connection: RemoteConnectionOptions,
    #[arg(long)]
    pub project_id: Option<Uuid>,
    #[arg(long)]
    pub commit_id: Option<Uuid>,
    #[arg(long)]
    pub validation_run_id: Option<Uuid>,
    #[arg(long)]
    pub output: PathBuf,
    /// Reuse this value after an ambiguous network failure.
    #[arg(long)]
    pub idempotency_key: Option<String>,
    #[arg(long, default_value_t = DEFAULT_WAIT_SECONDS)]
    pub timeout_seconds: u64,
}

#[derive(Debug, Clone, ClapArgs)]
pub struct ReleaseBuildOptions {
    #[command(flatten)]
    pub connection: RemoteConnectionOptions,
    #[arg(long)]
    pub project_id: Option<Uuid>,
    #[arg(long)]
    pub commit_id: Option<Uuid>,
    #[arg(long)]
    pub validation_run_id: Option<Uuid>,
    #[arg(long)]
    pub idempotency_key: Option<String>,
    #[arg(long, default_value_t = true, action = ArgAction::Set)]
    pub wait: bool,
    #[arg(long, default_value_t = DEFAULT_WAIT_SECONDS)]
    pub timeout_seconds: u64,
}

#[derive(Debug, Clone, ClapArgs)]
pub struct ReleaseScopeOptions {
    #[command(flatten)]
    pub connection: RemoteConnectionOptions,
    #[arg(long)]
    pub project_id: Option<Uuid>,
}

#[derive(Debug, Clone, ClapArgs)]
pub struct ReleaseShowOptions {
    #[command(flatten)]
    pub scope: ReleaseScopeOptions,
    #[arg(long)]
    pub release_id: Option<Uuid>,
}

#[derive(Debug, Clone, ClapArgs)]
pub struct ReleaseDownloadOptions {
    #[command(flatten)]
    pub target: ReleaseShowOptions,
    #[arg(long)]
    pub output: PathBuf,
}

#[derive(Debug, Clone, ClapArgs)]
pub struct EventsWatchOptions {
    #[command(flatten)]
    pub connection: RemoteConnectionOptions,
    /// Resume after this durable Studio event sequence.
    #[arg(long)]
    pub cursor: Option<u64>,
    /// Emit only events for this project. Studio still authorizes every event server-side.
    #[arg(long)]
    pub project_id: Option<Uuid>,
    /// Exit successfully after this many events. Omit to follow until interrupted.
    #[arg(long)]
    pub max_events: Option<usize>,
}

#[derive(Debug, Clone, ClapArgs)]
pub struct SubmitReviewOptions {
    #[command(flatten)]
    pub connection: RemoteConnectionOptions,
    #[arg(long)]
    pub project_id: Option<Uuid>,
    #[arg(long)]
    pub commit_id: Option<Uuid>,
    #[arg(long)]
    pub validation_run_id: Option<Uuid>,
    /// Reuse this value after an ambiguous network failure.
    #[arg(long)]
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Clone, ClapArgs)]
pub struct ListReviewsOptions {
    #[command(flatten)]
    pub connection: RemoteConnectionOptions,
    #[arg(long)]
    pub project_id: Option<Uuid>,
}

#[derive(Debug, Clone, ClapArgs)]
pub struct ApproveReviewOptions {
    #[command(flatten)]
    pub connection: RemoteConnectionOptions,
    #[arg(long)]
    pub project_id: Option<Uuid>,
    #[arg(long)]
    pub review_id: Option<Uuid>,
    /// Decide an assignment claimed from the Reporch review pool.
    #[arg(long, conflicts_with_all = ["project_id", "review_id"])]
    pub pool_request_id: Option<Uuid>,
    #[arg(long)]
    pub comment: Option<String>,
    /// Reuse this value after an ambiguous network failure.
    #[arg(long)]
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Clone, ClapArgs)]
pub struct RequestChangesOptions {
    #[command(flatten)]
    pub connection: RemoteConnectionOptions,
    #[arg(long)]
    pub project_id: Option<Uuid>,
    #[arg(long)]
    pub review_id: Option<Uuid>,
    /// Decide an assignment claimed from the Reporch review pool.
    #[arg(long, conflicts_with_all = ["project_id", "review_id"])]
    pub pool_request_id: Option<Uuid>,
    /// Explain the changes needed. Empty comments are rejected locally.
    #[arg(long)]
    pub comment: String,
    /// Reuse this value after an ambiguous network failure.
    #[arg(long)]
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Clone, ClapArgs)]
pub struct ReviewPoolRequestOptions {
    #[command(flatten)]
    pub connection: RemoteConnectionOptions,
    #[arg(long)]
    pub project_id: Option<Uuid>,
    #[arg(long)]
    pub review_id: Uuid,
    /// Route this review through the independent Reporch reviewer pool.
    #[arg(long, required = true, action = ArgAction::SetTrue)]
    pub pool: bool,
    #[arg(long)]
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Clone, ClapArgs)]
pub struct ReviewPoolTargetOptions {
    #[command(flatten)]
    pub connection: RemoteConnectionOptions,
    #[arg(long)]
    pub pool_request_id: Uuid,
}

#[derive(Debug, Clone, ClapArgs)]
pub struct ReviewPoolInboxOptions {
    #[command(flatten)]
    pub connection: RemoteConnectionOptions,
}

#[derive(Debug, Clone, ClapArgs)]
pub struct MemberScopeOptions {
    #[command(flatten)]
    pub connection: RemoteConnectionOptions,
    #[arg(long)]
    pub project_id: Option<Uuid>,
}

#[derive(Debug, Clone, ClapArgs)]
pub struct MemberSearchOptions {
    #[command(flatten)]
    pub scope: MemberScopeOptions,
    pub query: String,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum MemberRole {
    Maintainer,
    Author,
    Reviewer,
    Translator,
    Viewer,
}

impl From<MemberRole> for ProjectRole {
    fn from(value: MemberRole) -> Self {
        match value {
            MemberRole::Maintainer => Self::Maintainer,
            MemberRole::Author => Self::Author,
            MemberRole::Reviewer => Self::Reviewer,
            MemberRole::Translator => Self::Translator,
            MemberRole::Viewer => Self::Viewer,
        }
    }
}

#[derive(Debug, Clone, ClapArgs)]
pub struct UpsertMemberOptions {
    #[command(flatten)]
    pub scope: MemberScopeOptions,
    #[arg(long)]
    pub issuer: String,
    #[arg(long)]
    pub subject: String,
    #[arg(long, value_enum)]
    pub role: MemberRole,
    #[arg(long, default_value_t = 0)]
    pub entitlement_version: i64,
}

#[derive(Debug, Clone, ClapArgs)]
pub struct RemoveMemberOptions {
    #[command(flatten)]
    pub scope: MemberScopeOptions,
    #[arg(long)]
    pub issuer: String,
    #[arg(long)]
    pub subject: String,
}

#[derive(Debug, Clone, ClapArgs)]
pub struct PublicationOptions {
    #[command(flatten)]
    pub connection: RemoteConnectionOptions,
    #[arg(long)]
    pub project_id: Option<Uuid>,
    #[arg(long)]
    pub release_id: Option<Uuid>,
}

#[derive(Debug, Clone, ClapArgs)]
pub struct PublishOptions {
    #[command(flatten)]
    pub target: PublicationOptions,
    #[arg(long)]
    pub idempotency_key: Option<String>,
    #[arg(long, default_value_t = true, action = ArgAction::Set)]
    pub wait: bool,
    #[arg(long, default_value_t = DEFAULT_WAIT_SECONDS)]
    pub timeout_seconds: u64,
}

#[derive(Debug, Clone, ClapArgs)]
pub struct WaiverScopeOptions {
    #[command(flatten)]
    pub connection: RemoteConnectionOptions,
    #[arg(long)]
    pub project_id: Option<Uuid>,
    #[arg(long)]
    pub validation_run_id: Option<Uuid>,
}

#[derive(Debug, Clone, ClapArgs)]
pub struct RevisionScopeOptions {
    #[command(flatten)]
    pub connection: RemoteConnectionOptions,
    #[arg(long)]
    pub project_id: Option<Uuid>,
}

#[derive(Debug, Clone, ClapArgs)]
pub struct RevisionShowOptions {
    #[command(flatten)]
    pub scope: RevisionScopeOptions,
    #[arg(long)]
    pub commit_id: Option<Uuid>,
}

#[derive(Debug, Clone, ClapArgs)]
pub struct RevisionDiffOptions {
    #[command(flatten)]
    pub scope: RevisionScopeOptions,
    pub from: Uuid,
    pub to: Uuid,
}

#[derive(Debug, Clone, Serialize)]
pub struct RevisionDiffV1 {
    pub schema: &'static str,
    pub project_id: Uuid,
    pub from_commit_id: Uuid,
    pub to_commit_id: Uuid,
    pub metadata_changed: Vec<String>,
    pub files_added: Vec<String>,
    pub files_modified: Vec<String>,
    pub files_removed: Vec<String>,
}

#[derive(Debug, Clone, ClapArgs)]
pub struct CreateWaiverOptions {
    #[command(flatten)]
    pub scope: WaiverScopeOptions,
    #[arg(long)]
    pub issue_code: String,
    #[arg(long)]
    pub reason: String,
    /// RFC 3339 timestamp after which this waiver is invalid.
    #[arg(long)]
    pub expires_at: String,
    #[arg(long)]
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Clone, ClapArgs)]
pub struct RevokeWaiverOptions {
    #[command(flatten)]
    pub scope: WaiverScopeOptions,
    #[arg(long)]
    pub waiver_id: Uuid,
    #[arg(long)]
    pub reason: String,
    #[arg(long)]
    pub idempotency_key: Option<String>,
}

struct StudioApiClient {
    http: reqwest::Client,
    api_base: Url,
    access_token: Zeroizing<String>,
    allow_insecure_loopback: bool,
}

impl StudioApiClient {
    async fn connect(options: &RemoteConnectionOptions) -> Result<Self> {
        let api_base = validate_api_url(&options.api_url, options.auth.allow_insecure_http)?;
        let auth = NativeAuthClient::discover(device_auth_config(&options.auth)?)
            .await
            .context("discover Reporch native OAuth endpoints")?;
        let access_token = auth
            .access_token(&KeyringTokenStore)
            .await
            .context("load or refresh the CLI credential; run `reporch auth login` first")?;
        let http = reqwest::ClientBuilder::new()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30 * 60))
            .build()
            .context("build Studio API client")?;
        Ok(Self {
            http,
            api_base,
            access_token: Zeroizing::new(access_token),
            allow_insecure_loopback: options.auth.allow_insecure_http,
        })
    }

    fn endpoint(&self, path: &str) -> Result<Url> {
        self.api_base
            .join(path)
            .with_context(|| format!("build Studio API path {path}"))
    }

    fn authenticated(&self, request: RequestBuilder) -> RequestBuilder {
        request
            .bearer_auth(self.access_token.as_str())
            .header("x-request-id", Uuid::now_v7().to_string())
            .timeout(Duration::from_secs(30))
    }

    async fn json<T: DeserializeOwned>(&self, request: RequestBuilder) -> Result<T> {
        let response = self
            .authenticated(request)
            .send()
            .await
            .map_err(studio_api_transport_error)?;
        decode_api_response(response).await
    }

    async fn empty(&self, request: RequestBuilder) -> Result<()> {
        let response = self
            .authenticated(request)
            .send()
            .await
            .map_err(studio_api_transport_error)?;
        if response.status().is_success() {
            return Ok(());
        }
        let _: serde_json::Value = decode_api_response(response).await?;
        Ok(())
    }

    async fn create_project(
        &self,
        request: &CreateProjectRequest,
        idempotency_key: &str,
    ) -> Result<ProjectResponse> {
        self.json(
            self.http
                .post(self.endpoint("projects")?)
                .header("idempotency-key", idempotency_key)
                .json(request),
        )
        .await
    }

    async fn list_projects(&self, cursor: Option<&str>) -> Result<ProjectPage> {
        let mut url = self.endpoint("projects")?;
        url.query_pairs_mut().append_pair("limit", "100");
        if let Some(cursor) = cursor {
            url.query_pairs_mut().append_pair("cursor", cursor);
        }
        self.json(self.http.get(url)).await
    }

    async fn list_commits(&self, project_id: Uuid) -> Result<CommitPage> {
        self.json(
            self.http
                .get(self.endpoint(&format!("projects/{project_id}/commits?limit=1"))?),
        )
        .await
    }

    async fn list_commits_page(
        &self,
        project_id: Uuid,
        cursor: Option<Uuid>,
    ) -> Result<CommitPage> {
        let mut url = self.endpoint(&format!("projects/{project_id}/commits"))?;
        url.query_pairs_mut().append_pair("limit", "100");
        if let Some(cursor) = cursor {
            url.query_pairs_mut()
                .append_pair("cursor", &cursor.to_string());
        }
        self.json(self.http.get(url)).await
    }

    async fn get_commit(&self, project_id: Uuid, commit_id: Uuid) -> Result<CommitDetailResponse> {
        self.json(
            self.http
                .get(self.endpoint(&format!("projects/{project_id}/commits/{commit_id}"))?),
        )
        .await
    }

    async fn list_files(&self, project_id: Uuid) -> Result<FileEntryPage> {
        self.json(
            self.http
                .get(self.endpoint(&format!("projects/{project_id}/files"))?),
        )
        .await
    }

    async fn file_download(&self, project_id: Uuid, path: &str) -> Result<FileDownloadResponse> {
        let mut url = self.endpoint(&format!("projects/{project_id}/file-download"))?;
        url.query_pairs_mut().append_pair("path", path);
        self.json(self.http.get(url)).await
    }

    async fn begin_upload(
        &self,
        project_id: Uuid,
        request: &BeginFileUploadRequest,
    ) -> Result<BeginFileUploadResponse> {
        self.json(
            self.http
                .post(self.endpoint(&format!("projects/{project_id}/uploads"))?)
                .json(request),
        )
        .await
    }

    async fn complete_upload(
        &self,
        project_id: Uuid,
        upload_id: Uuid,
    ) -> Result<FileUploadResponse> {
        self.json(self.http.post(self.endpoint(&format!(
            "projects/{project_id}/uploads/{upload_id}/complete"
        ))?))
        .await
    }

    async fn get_upload(&self, project_id: Uuid, upload_id: Uuid) -> Result<FileUploadResponse> {
        self.json(
            self.http
                .get(self.endpoint(&format!("projects/{project_id}/uploads/{upload_id}"))?),
        )
        .await
    }

    async fn create_commit(
        &self,
        project_id: Uuid,
        request: &CreateCommitRequest,
    ) -> Result<CommitResponse> {
        self.json(
            self.http
                .post(self.endpoint(&format!("projects/{project_id}/commits"))?)
                .json(request),
        )
        .await
    }

    async fn capabilities(&self) -> Result<StudioCapabilitiesV1> {
        self.json(self.http.get(self.endpoint("capabilities")?))
            .await
    }

    async fn quota(&self) -> Result<QuotaStatusV1> {
        self.json(self.http.get(self.endpoint("quota")?)).await
    }

    async fn events_response(&self, cursor: Option<u64>) -> Result<Response> {
        let mut url = self.endpoint("events")?;
        if let Some(cursor) = cursor {
            url.query_pairs_mut()
                .append_pair("cursor", &cursor.to_string());
        }
        let response = self
            .authenticated(self.http.get(url).header(ACCEPT, "text/event-stream"))
            .timeout(Duration::from_secs(30 * 60))
            .send()
            .await
            .map_err(studio_api_transport_error)?;
        if !response.status().is_success() {
            let _: serde_json::Value = decode_api_response(response).await?;
            unreachable!("a successful error response cannot be decoded")
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        ensure!(
            content_type
                .split(';')
                .next()
                .is_some_and(|value| value.trim() == "text/event-stream"),
            "Studio events endpoint returned an unexpected Content-Type"
        );
        Ok(response)
    }

    async fn get_working_copy(&self, project_id: Uuid) -> Result<WorkingCopyV1> {
        self.json(
            self.http
                .get(self.endpoint(&format!("projects/{project_id}/working-copy"))?),
        )
        .await
    }

    async fn update_working_copy(
        &self,
        project_id: Uuid,
        expected_revision: i64,
        request: &UpdateWorkingCopyRequestV1,
    ) -> Result<WorkingCopyV1> {
        self.json(
            self.http
                .put(self.endpoint(&format!("projects/{project_id}/working-copy"))?)
                .header(IF_MATCH, format!("\"{expected_revision}\""))
                .json(request),
        )
        .await
    }

    async fn commit_working_copy(
        &self,
        project_id: Uuid,
        request: &CommitWorkingCopyRequestV1,
        idempotency_key: &str,
    ) -> Result<CommitResponse> {
        self.json(
            self.http
                .post(self.endpoint(&format!("projects/{project_id}/working-copy/commits"))?)
                .header("idempotency-key", idempotency_key)
                .json(request),
        )
        .await
    }

    async fn working_copy_readiness(&self, project_id: Uuid) -> Result<WorkingCopyReadinessV1> {
        self.json(
            self.http
                .get(self.endpoint(&format!("projects/{project_id}/readiness"))?),
        )
        .await
    }

    async fn list_members(&self, project_id: Uuid) -> Result<ProjectMembershipPage> {
        self.json(
            self.http
                .get(self.endpoint(&format!("projects/{project_id}/memberships"))?),
        )
        .await
    }

    async fn search_members(
        &self,
        project_id: Uuid,
        query: &str,
    ) -> Result<IdentityDirectoryPageV1> {
        let mut url = self.endpoint(&format!("projects/{project_id}/identity-directory"))?;
        url.query_pairs_mut().append_pair("q", query);
        self.json(self.http.get(url)).await
    }

    async fn upsert_member(
        &self,
        project_id: Uuid,
        request: &UpsertProjectMembershipRequest,
    ) -> Result<ProjectMembershipResponse> {
        self.json(
            self.http
                .put(self.endpoint(&format!("projects/{project_id}/memberships"))?)
                .json(request),
        )
        .await
    }

    async fn remove_member(&self, project_id: Uuid, issuer: &str, subject: &str) -> Result<()> {
        let mut url = self.endpoint(&format!("projects/{project_id}/memberships"))?;
        url.query_pairs_mut()
            .append_pair("issuer", issuer)
            .append_pair("subject", subject);
        self.empty(self.http.delete(url)).await
    }

    async fn publish_release(
        &self,
        project_id: Uuid,
        release_id: Uuid,
        idempotency_key: &str,
    ) -> Result<PublicationResponse> {
        self.json(
            self.http
                .post(self.endpoint(&format!(
                    "projects/{project_id}/releases/{release_id}/publication"
                ))?)
                .header("idempotency-key", idempotency_key),
        )
        .await
    }

    async fn get_publication(
        &self,
        project_id: Uuid,
        release_id: Uuid,
    ) -> Result<PublicationResponse> {
        self.json(self.http.get(self.endpoint(&format!(
            "projects/{project_id}/releases/{release_id}/publication"
        ))?))
        .await
    }

    async fn enqueue_validation(
        &self,
        project_id: Uuid,
        request: &EnqueueValidationRequest,
        idempotency_key: &str,
    ) -> Result<ValidationRunResponse> {
        self.json(
            self.http
                .post(self.endpoint(&format!("projects/{project_id}/validations"))?)
                .header("idempotency-key", idempotency_key)
                .json(request),
        )
        .await
    }

    async fn get_validation(
        &self,
        project_id: Uuid,
        validation_id: Uuid,
    ) -> Result<ValidationRunDetailResponse> {
        self.json(self.http.get(self.endpoint(&format!(
            "projects/{project_id}/validations/{validation_id}"
        ))?))
        .await
    }

    async fn list_waivers(&self, project_id: Uuid, validation_id: Uuid) -> Result<WaiverPage> {
        self.json(self.http.get(self.endpoint(&format!(
            "projects/{project_id}/validations/{validation_id}/waivers"
        ))?))
        .await
    }

    async fn create_waiver(
        &self,
        project_id: Uuid,
        validation_id: Uuid,
        request: &CreateWaiverRequest,
        idempotency_key: &str,
    ) -> Result<WaiverResponse> {
        self.json(
            self.http
                .post(self.endpoint(&format!(
                    "projects/{project_id}/validations/{validation_id}/waivers"
                ))?)
                .header("idempotency-key", idempotency_key)
                .json(request),
        )
        .await
    }

    async fn revoke_waiver(
        &self,
        project_id: Uuid,
        validation_id: Uuid,
        waiver_id: Uuid,
        request: &RevokeWaiverRequest,
        idempotency_key: &str,
    ) -> Result<WaiverResponse> {
        self.json(
            self.http
                .post(self.endpoint(&format!(
                    "projects/{project_id}/validations/{validation_id}/waivers/{waiver_id}/revocation"
                ))?)
                .header("idempotency-key", idempotency_key)
                .json(request),
        )
        .await
    }

    async fn create_release(
        &self,
        project_id: Uuid,
        request: &CreateReleaseRequest,
        idempotency_key: &str,
    ) -> Result<ReleaseResponse> {
        self.json(
            self.http
                .post(self.endpoint(&format!("projects/{project_id}/releases"))?)
                .header("idempotency-key", idempotency_key)
                .json(request),
        )
        .await
    }

    async fn list_releases(&self, project_id: Uuid, cursor: Option<Uuid>) -> Result<ReleasePage> {
        let mut url = self.endpoint(&format!("projects/{project_id}/releases"))?;
        url.query_pairs_mut().append_pair("limit", "100");
        if let Some(cursor) = cursor {
            url.query_pairs_mut()
                .append_pair("cursor", &cursor.to_string());
        }
        self.json(self.http.get(url)).await
    }

    async fn submit_review(
        &self,
        project_id: Uuid,
        request: &SubmitReviewRequest,
        idempotency_key: &str,
    ) -> Result<ReviewResponse> {
        self.json(
            self.http
                .post(self.endpoint(&format!("projects/{project_id}/reviews"))?)
                .header("idempotency-key", idempotency_key)
                .json(request),
        )
        .await
    }

    async fn list_reviews(&self, project_id: Uuid, cursor: Option<&str>) -> Result<ReviewPage> {
        let mut url = self.endpoint(&format!("projects/{project_id}/reviews"))?;
        url.query_pairs_mut().append_pair("limit", "100");
        if let Some(cursor) = cursor {
            url.query_pairs_mut().append_pair("cursor", cursor);
        }
        self.json(self.http.get(url)).await
    }

    async fn decide_review(
        &self,
        project_id: Uuid,
        review_id: Uuid,
        request: &CreateReviewDecisionRequest,
        idempotency_key: &str,
    ) -> Result<ReviewResponse> {
        self.json(
            self.http
                .post(self.endpoint(&format!(
                    "projects/{project_id}/reviews/{review_id}/decisions"
                ))?)
                .header("idempotency-key", idempotency_key)
                .json(request),
        )
        .await
    }

    async fn request_review_pool(
        &self,
        project_id: Uuid,
        review_id: Uuid,
        idempotency_key: &str,
    ) -> Result<ReviewPoolRequestResponseV1> {
        self.json(
            self.http
                .post(self.endpoint(&format!(
                    "projects/{project_id}/reviews/{review_id}/pool-request"
                ))?)
                .header("idempotency-key", idempotency_key),
        )
        .await
    }

    async fn get_review_pool_request(
        &self,
        request_id: Uuid,
    ) -> Result<ReviewPoolRequestResponseV1> {
        self.json(
            self.http
                .get(self.endpoint(&format!("review-pool/{request_id}"))?),
        )
        .await
    }

    async fn list_review_pool_inbox(&self, cursor: Option<Uuid>) -> Result<ReviewPoolPageV1> {
        let mut url = self.endpoint("review-pool/inbox")?;
        url.query_pairs_mut().append_pair("limit", "100");
        if let Some(cursor) = cursor {
            url.query_pairs_mut()
                .append_pair("cursor", &cursor.to_string());
        }
        self.json(self.http.get(url)).await
    }

    async fn claim_review_pool_request(
        &self,
        request_id: Uuid,
    ) -> Result<ReviewPoolRequestResponseV1> {
        self.json(
            self.http
                .post(self.endpoint(&format!("review-pool/{request_id}/claim"))?),
        )
        .await
    }

    async fn cancel_review_pool_request(
        &self,
        request_id: Uuid,
    ) -> Result<ReviewPoolRequestResponseV1> {
        self.json(
            self.http
                .post(self.endpoint(&format!("review-pool/{request_id}/cancel"))?),
        )
        .await
    }

    async fn decide_pool_review(
        &self,
        request_id: Uuid,
        request: &CreateReviewDecisionRequest,
        idempotency_key: &str,
    ) -> Result<ReviewResponse> {
        self.json(
            self.http
                .post(self.endpoint(&format!("review-pool/{request_id}/decision"))?)
                .header("idempotency-key", idempotency_key)
                .json(request),
        )
        .await
    }

    async fn get_release(&self, project_id: Uuid, release_id: Uuid) -> Result<ReleaseResponse> {
        self.json(
            self.http
                .get(self.endpoint(&format!("projects/{project_id}/releases/{release_id}"))?),
        )
        .await
    }

    async fn release_download(
        &self,
        project_id: Uuid,
        release_id: Uuid,
    ) -> Result<ReleaseDownloadResponse> {
        self.json(self.http.get(self.endpoint(&format!(
            "projects/{project_id}/releases/{release_id}/download"
        ))?))
        .await
    }

    async fn upload_file(&self, source: &Path, upload: &BeginFileUploadResponse) -> Result<()> {
        let signed_url =
            validate_signed_object_url(&upload.upload_url, self.allow_insecure_loopback)?;
        match upload.strategy {
            FileUploadStrategyV1::SinglePut => {
                let file = tokio::fs::File::open(source)
                    .await
                    .with_context(|| format!("open {}", source.display()))?;
                let body = reqwest::Body::wrap_stream(ReaderStream::new(file));
                let mut request = self
                    .http
                    .put(signed_url)
                    .header(CONTENT_LENGTH, upload.upload.size_bytes)
                    .body(body);
                for (name, value) in &upload.required_headers {
                    ensure!(
                        matches!(
                            name.to_ascii_lowercase().as_str(),
                            "content-type" | "x-ms-blob-type" | "x-ms-version"
                        ),
                        "Studio returned an unsupported signed-upload header"
                    );
                    request = request.header(
                        HeaderName::from_bytes(name.as_bytes())
                            .context("invalid signed-upload header name")?,
                        HeaderValue::from_str(value)
                            .context("invalid signed-upload header value")?,
                    );
                }
                expect_object_success(request.send().await.map_err(redact_reqwest_error)?).await
            }
            FileUploadStrategyV1::AzureBlockV1 => {
                self.upload_azure_blocks(source, signed_url, upload).await
            }
        }
    }

    async fn upload_azure_blocks(
        &self,
        source: &Path,
        signed_url: Url,
        upload: &BeginFileUploadResponse,
    ) -> Result<()> {
        ensure!(
            upload.block_id_encoding.as_deref() == Some("base64_zero_padded_decimal_6"),
            "Studio returned an unsupported block ID contract"
        );
        let part_size = upload
            .part_size_bytes
            .context("block part size is missing")?;
        let part_count = upload.part_count.context("block part count is missing")?;
        ensure!(
            part_size > 0 && part_size <= MAX_BLOCK_BYTES,
            "invalid block part size"
        );
        ensure!(part_count > 0, "invalid block part count");
        let mut file = tokio::fs::File::open(source)
            .await
            .with_context(|| format!("open {}", source.display()))?;
        let mut block_ids = Vec::with_capacity(part_count as usize);
        for index in 0..part_count {
            let mut block = vec![0_u8; part_size as usize];
            let mut used = 0_usize;
            while used < block.len() {
                let read = file.read(&mut block[used..]).await?;
                if read == 0 {
                    break;
                }
                used += read;
            }
            ensure!(used > 0, "source ended before the advertised block count");
            block.truncate(used);
            let block_id = STANDARD.encode(format!("{index:06}"));
            let mut url = signed_url.clone();
            url.query_pairs_mut()
                .append_pair("comp", "block")
                .append_pair("blockid", &block_id);
            expect_object_success(
                self.http
                    .put(url)
                    .body(block)
                    .send()
                    .await
                    .map_err(redact_reqwest_error)?,
            )
            .await?;
            block_ids.push(block_id);
        }
        let mut extra = [0_u8; 1];
        ensure!(
            file.read(&mut extra).await? == 0,
            "source exceeds the advertised block count"
        );
        let mut block_list = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?><BlockList>");
        for block_id in block_ids {
            block_list.push_str("<Latest>");
            block_list.push_str(&block_id);
            block_list.push_str("</Latest>");
        }
        block_list.push_str("</BlockList>");
        let mut url = signed_url;
        url.query_pairs_mut().append_pair("comp", "blocklist");
        expect_object_success(
            self.http
                .put(url)
                .header("content-type", "application/xml")
                .body(block_list)
                .send()
                .await?,
        )
        .await
    }

    async fn download_verified(
        &self,
        url: &str,
        expected_size: u64,
        expected_digest: &Sha256Digest,
        output: &Path,
    ) -> Result<()> {
        ensure!(
            !output.exists(),
            "refusing to overwrite {}",
            output.display()
        );
        let parent = output.parent().unwrap_or_else(|| Path::new("."));
        ensure!(
            parent.is_dir(),
            "output parent does not exist: {}",
            parent.display()
        );
        let url = validate_signed_object_url(url, self.allow_insecure_loopback)?;
        let response = self
            .http
            .get(url)
            .send()
            .await
            .map_err(redact_reqwest_error)?;
        ensure!(
            response.status().is_success(),
            "object download failed with HTTP {}",
            response.status()
        );
        if let Some(content_length) = response.content_length() {
            ensure!(
                content_length == expected_size,
                "object Content-Length mismatch"
            );
        }
        let temporary = NamedTempFile::new_in(parent)
            .with_context(|| format!("create temporary output in {}", parent.display()))?;
        let writer = temporary.reopen()?;
        let mut writer = tokio::fs::File::from_std(writer);
        let mut stream = response.bytes_stream();
        let mut size = 0_u64;
        let mut digest = Sha256::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(redact_reqwest_error)?;
            size = size
                .checked_add(chunk.len() as u64)
                .context("download size overflow")?;
            ensure!(size <= expected_size, "object exceeded its declared size");
            digest.update(&chunk);
            writer.write_all(&chunk).await?;
        }
        writer.flush().await?;
        writer.sync_all().await?;
        drop(writer);
        let digest: Sha256Digest = hex::encode(digest.finalize()).parse()?;
        ensure!(size == expected_size, "object size mismatch");
        ensure!(&digest == expected_digest, "object SHA-256 mismatch");
        temporary
            .persist_noclobber(output)
            .map_err(|error| error.error)
            .with_context(|| format!("install {} without overwrite", output.display()))?;
        Ok(())
    }
}

#[derive(Debug, Serialize)]
pub struct PullOperationResult {
    pub project_id: Uuid,
    pub commit_id: Uuid,
    pub directory: PathBuf,
    pub file_count: usize,
}

#[derive(Debug, Serialize)]
pub struct PushOperationResult {
    pub uploaded_files: usize,
    pub commit: CommitResponse,
}

#[derive(Debug, Serialize)]
pub struct ValidationOperationResult {
    pub queued: ValidationRunResponse,
    pub detail: Option<ValidationRunDetailResponse>,
}

#[derive(Debug, Serialize)]
pub struct PackageOperationResult {
    pub release: ReleaseResponse,
    pub output: PathBuf,
    pub package_digest: Sha256Digest,
    pub package_size_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct EventWatchItem {
    pub cursor: u64,
    pub event: EventEnvelopeV1,
}

#[derive(Debug, Serialize)]
pub struct EventWatchResult {
    pub events: Vec<EventWatchItem>,
    pub last_cursor: Option<u64>,
    pub interrupted: bool,
}

#[derive(Debug, Default)]
struct SseDecoder {
    buffer: Vec<u8>,
    id: Option<String>,
    event: Option<String>,
    data_lines: Vec<String>,
    data_bytes: usize,
}

#[derive(Debug, PartialEq, Eq)]
struct SseFrame {
    id: Option<String>,
    event: Option<String>,
    data: Option<String>,
}

impl SseDecoder {
    fn push(&mut self, chunk: &[u8]) -> Result<Vec<SseFrame>> {
        self.buffer.extend_from_slice(chunk);
        ensure!(
            self.buffer.len() <= MAX_SSE_BUFFER_BYTES,
            "Studio SSE line exceeded the 1 MiB client bound"
        );
        let mut frames = Vec::new();
        while let Some(newline) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let mut raw = self.buffer.drain(..=newline).collect::<Vec<_>>();
            raw.pop();
            if raw.last() == Some(&b'\r') {
                raw.pop();
            }
            let line = std::str::from_utf8(&raw).context("Studio SSE was not UTF-8")?;
            if let Some(frame) = self.line(line)? {
                frames.push(frame);
            }
        }
        Ok(frames)
    }

    fn line(&mut self, line: &str) -> Result<Option<SseFrame>> {
        if line.is_empty() {
            if self.id.is_none() && self.event.is_none() && self.data_lines.is_empty() {
                return Ok(None);
            }
            let frame = SseFrame {
                id: self.id.take(),
                event: self.event.take(),
                data: (!self.data_lines.is_empty()).then(|| self.data_lines.join("\n")),
            };
            self.data_lines.clear();
            self.data_bytes = 0;
            return Ok(Some(frame));
        }
        if line.starts_with(':') {
            return Ok(None);
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "id" if !value.contains('\0') => self.id = Some(value.to_owned()),
            "event" => self.event = Some(value.to_owned()),
            "data" => {
                self.data_bytes = self
                    .data_bytes
                    .checked_add(value.len() + usize::from(!self.data_lines.is_empty()))
                    .context("Studio SSE data size overflow")?;
                ensure!(
                    self.data_bytes <= MAX_SSE_DATA_BYTES,
                    "Studio SSE event exceeded the 1 MiB client bound"
                );
                self.data_lines.push(value.to_owned());
            }
            _ => {}
        }
        Ok(None)
    }
}

pub async fn list_projects_operation(connection: &RemoteConnectionOptions) -> Result<ProjectPage> {
    let client = StudioApiClient::connect(connection).await?;
    let mut items = Vec::new();
    let mut cursor = None;
    let mut seen_cursors = std::collections::HashSet::new();
    for _ in 0..100 {
        let page = client.list_projects(cursor.as_deref()).await?;
        items.extend(page.items);
        let Some(next_cursor) = page.next_cursor else {
            return Ok(ProjectPage {
                items,
                next_cursor: None,
            });
        };
        ensure!(
            seen_cursors.insert(next_cursor.clone()),
            "Studio returned a repeated project cursor"
        );
        cursor = Some(next_cursor);
    }
    bail!("Studio project listing exceeded the 10,000-item native client bound")
}

pub async fn capabilities_operation(
    connection: &RemoteConnectionOptions,
) -> Result<StudioCapabilitiesV1> {
    let capabilities = StudioApiClient::connect(connection)
        .await?
        .capabilities()
        .await?;
    ensure_cli_compatible(&capabilities)?;
    Ok(capabilities)
}

pub async fn quota_operation(connection: &RemoteConnectionOptions) -> Result<QuotaStatusV1> {
    StudioApiClient::connect(connection).await?.quota().await
}

pub async fn watch_events_operation<F>(
    options: &EventsWatchOptions,
    mut emit: F,
) -> Result<EventWatchResult>
where
    F: FnMut(&EventWatchItem) -> Result<()>,
{
    ensure!(
        options
            .max_events
            .is_none_or(|max_events| (1..=10_000).contains(&max_events)),
        "--max-events must be between 1 and 10000"
    );
    let client = StudioApiClient::connect(&options.connection).await?;
    let mut cursor = options.cursor;
    let mut events = Vec::new();
    let mut reconnect_attempt = 0_u32;
    loop {
        let response = match client.events_response(cursor).await {
            Ok(response) => response,
            Err(error)
                if is_transient_api_error(&error)
                    && reconnect_attempt < MAX_TRANSIENT_POLL_RETRIES =>
            {
                reconnect_attempt += 1;
                tokio::time::sleep(transient_poll_delay(reconnect_attempt)).await;
                continue;
            }
            Err(error) => return Err(error).context("connect Studio event stream"),
        };
        let mut stream = response.bytes_stream();
        let mut decoder = SseDecoder::default();
        let mut reconnect_requested = false;
        loop {
            let next = tokio::select! {
                interrupted = tokio::signal::ctrl_c() => {
                    interrupted.context("listen for interrupt")?;
                    return Ok(EventWatchResult {
                        events,
                        last_cursor: cursor,
                        interrupted: true,
                    });
                }
                next = stream.next() => next,
            };
            let Some(chunk) = next else {
                break;
            };
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(error) => {
                    let error = studio_api_transport_error(error);
                    if is_transient_api_error(&error) {
                        break;
                    }
                    return Err(error).context("read Studio event stream");
                }
            };
            for frame in decoder.push(&chunk)? {
                if let Some(id) = frame.id {
                    cursor = Some(parse_event_cursor(&id)?);
                }
                if frame.event.as_deref() == Some("studio.stream.error.v1") {
                    reconnect_requested = true;
                    break;
                }
                let Some(data) = frame.data else {
                    continue;
                };
                let event: EventEnvelopeV1 =
                    serde_json::from_str(&data).context("decode Studio event envelope")?;
                ensure!(
                    event.schema == EVENT_SCHEMA_V1 && event.event_version == 1,
                    "Studio returned an unsupported event envelope"
                );
                if let Some(sse_type) = frame.event.as_deref() {
                    ensure!(
                        sse_type == event.event_type,
                        "Studio SSE type did not match its event envelope"
                    );
                }
                let event_cursor = cursor.context("Studio event was missing a durable cursor")?;
                reconnect_attempt = 0;
                if options
                    .project_id
                    .is_some_and(|project_id| event.project_id != Some(project_id))
                {
                    continue;
                }
                let item = EventWatchItem {
                    cursor: event_cursor,
                    event,
                };
                if let Some(max_events) = options.max_events {
                    events.push(item);
                    if events.len() == max_events {
                        return Ok(EventWatchResult {
                            events,
                            last_cursor: cursor,
                            interrupted: false,
                        });
                    }
                } else {
                    emit(&item)?;
                }
            }
            if reconnect_requested {
                break;
            }
        }
        reconnect_attempt += 1;
        ensure!(
            reconnect_attempt <= MAX_TRANSIENT_POLL_RETRIES,
            "Studio event stream disconnected repeatedly"
        );
        tokio::time::sleep(transient_poll_delay(reconnect_attempt)).await;
    }
}

fn parse_event_cursor(value: &str) -> Result<u64> {
    ensure!(
        !value.is_empty() && value.len() <= 19 && value.bytes().all(|byte| byte.is_ascii_digit()),
        "Studio returned an invalid event cursor"
    );
    let cursor = value.parse::<u64>()?;
    ensure!(
        cursor < i64::MAX as u64,
        "Studio event cursor exceeded the supported range"
    );
    Ok(cursor)
}

pub async fn list_revisions_operation(options: &RevisionScopeOptions) -> Result<CommitPage> {
    let project_id = resolve_local_project_id(options.project_id)?;
    let client = StudioApiClient::connect(&options.connection).await?;
    let mut items = Vec::new();
    let mut cursor = None;
    let mut seen = std::collections::HashSet::new();
    for _ in 0..100 {
        let page = client.list_commits_page(project_id, cursor).await?;
        ensure!(
            page.items
                .iter()
                .all(|commit| commit.project_id == project_id),
            "Studio returned a revision from another project"
        );
        items.extend(page.items);
        let Some(next_cursor) = page.next_cursor else {
            return Ok(CommitPage {
                items,
                next_cursor: None,
            });
        };
        ensure!(
            seen.insert(next_cursor),
            "Studio returned a repeated revision cursor"
        );
        cursor = Some(next_cursor);
    }
    bail!("Studio revision listing exceeded the 10,000-item client bound")
}

pub async fn show_revision_operation(
    options: &RevisionShowOptions,
) -> Result<CommitDetailResponse> {
    let (project_id, commit_id, _) =
        resolve_local_candidate(options.scope.project_id, options.commit_id)?;
    let commit = StudioApiClient::connect(&options.scope.connection)
        .await?
        .get_commit(project_id, commit_id)
        .await?;
    ensure!(
        commit.project_id == project_id,
        "Studio returned a revision from another project"
    );
    Ok(commit)
}

pub async fn diff_revisions_operation(options: &RevisionDiffOptions) -> Result<RevisionDiffV1> {
    let project_id = resolve_local_project_id(options.scope.project_id)?;
    ensure!(options.from != options.to, "revision IDs must be different");
    let client = StudioApiClient::connect(&options.scope.connection).await?;
    let from = client.get_commit(project_id, options.from).await?;
    let to = client.get_commit(project_id, options.to).await?;
    ensure!(
        from.project_id == project_id && to.project_id == project_id,
        "Studio returned a revision from another project"
    );
    Ok(diff_revisions(&from, &to))
}

fn diff_revisions(from: &CommitDetailResponse, to: &CommitDetailResponse) -> RevisionDiffV1 {
    let from_spec = reporch_format::AuthoringSpecV1::from_manifest(&from.manifest);
    let to_spec = reporch_format::AuthoringSpecV1::from_manifest(&to.manifest);
    let from_json = serde_json::to_value(&from_spec).expect("AuthoringSpec serializes");
    let to_json = serde_json::to_value(&to_spec).expect("AuthoringSpec serializes");
    let mut metadata_changed = Vec::new();
    let empty = serde_json::Map::new();
    let from_object = from_json.as_object().unwrap_or(&empty);
    let to_object = to_json.as_object().unwrap_or(&empty);
    for key in from_object.keys().chain(to_object.keys()) {
        if key != "files"
            && from_object.get(key) != to_object.get(key)
            && !metadata_changed.iter().any(|existing| existing == key)
        {
            metadata_changed.push(key.clone());
        }
    }
    metadata_changed.sort();

    let from_files = from
        .manifest
        .files
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect::<std::collections::BTreeMap<_, _>>();
    let to_files = to
        .manifest
        .files
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect::<std::collections::BTreeMap<_, _>>();
    let files_added = to_files
        .keys()
        .filter(|path| !from_files.contains_key(**path))
        .map(|path| (*path).to_owned())
        .collect();
    let files_removed = from_files
        .keys()
        .filter(|path| !to_files.contains_key(**path))
        .map(|path| (*path).to_owned())
        .collect();
    let files_modified = to_files
        .iter()
        .filter(|(path, file)| from_files.get(**path).is_some_and(|old| *old != **file))
        .map(|(path, _)| (*path).to_owned())
        .collect();
    RevisionDiffV1 {
        schema: "reporch.revision-diff.v1",
        project_id: from.project_id,
        from_commit_id: from.id,
        to_commit_id: to.id,
        metadata_changed,
        files_added,
        files_modified,
        files_removed,
    }
}

pub async fn list_members_operation(options: &MemberScopeOptions) -> Result<ProjectMembershipPage> {
    let project_id = resolve_local_project_id(options.project_id)?;
    StudioApiClient::connect(&options.connection)
        .await?
        .list_members(project_id)
        .await
}

pub async fn search_members_operation(
    options: &MemberSearchOptions,
) -> Result<IdentityDirectoryPageV1> {
    let query = options.query.trim();
    ensure!(
        (2..=100).contains(&query.len()),
        "search query must contain 2 to 100 characters"
    );
    let project_id = resolve_local_project_id(options.scope.project_id)?;
    StudioApiClient::connect(&options.scope.connection)
        .await?
        .search_members(project_id, query)
        .await
}

pub async fn upsert_member_operation(
    options: &UpsertMemberOptions,
) -> Result<ProjectMembershipResponse> {
    ensure!(!options.issuer.trim().is_empty(), "issuer cannot be empty");
    ensure!(
        !options.subject.trim().is_empty(),
        "subject cannot be empty"
    );
    ensure!(
        options.entitlement_version >= 0,
        "entitlement version cannot be negative"
    );
    let project_id = resolve_local_project_id(options.scope.project_id)?;
    StudioApiClient::connect(&options.scope.connection)
        .await?
        .upsert_member(
            project_id,
            &UpsertProjectMembershipRequest {
                member: SubjectRef {
                    issuer: options.issuer.trim().into(),
                    subject: options.subject.trim().into(),
                },
                role: options.role.into(),
                entitlement_version: options.entitlement_version,
            },
        )
        .await
}

pub async fn remove_member_operation(options: &RemoveMemberOptions) -> Result<serde_json::Value> {
    ensure!(!options.issuer.trim().is_empty(), "issuer cannot be empty");
    ensure!(
        !options.subject.trim().is_empty(),
        "subject cannot be empty"
    );
    let project_id = resolve_local_project_id(options.scope.project_id)?;
    StudioApiClient::connect(&options.scope.connection)
        .await?
        .remove_member(project_id, options.issuer.trim(), options.subject.trim())
        .await?;
    Ok(serde_json::json!({
        "project_id": project_id,
        "issuer": options.issuer.trim(),
        "subject": options.subject.trim(),
        "removed": true,
    }))
}

pub async fn publication_status_operation(
    options: &PublicationOptions,
) -> Result<PublicationResponse> {
    let (project_id, release_id) = resolve_local_release(options.project_id, options.release_id)?;
    StudioApiClient::connect(&options.connection)
        .await?
        .get_publication(project_id, release_id)
        .await
}

pub async fn publish_operation(options: &PublishOptions) -> Result<PublicationResponse> {
    validate_wait_timeout(options.timeout_seconds)?;
    let (project_id, release_id) =
        resolve_local_release(options.target.project_id, options.target.release_id)?;
    let key = operation_key("cli-publication", options.idempotency_key.as_deref())?;
    let client = StudioApiClient::connect(&options.target.connection).await?;
    let publication = client.publish_release(project_id, release_id, &key).await?;
    if !options.wait || is_publication_terminal(publication.status) {
        return Ok(publication);
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(options.timeout_seconds);
    loop {
        ensure!(
            tokio::time::Instant::now() < deadline,
            "publication timed out"
        );
        tokio::time::sleep(Duration::from_millis(750)).await;
        let publication = client.get_publication(project_id, release_id).await?;
        if is_publication_terminal(publication.status) {
            ensure!(
                publication.status == PublicationStatus::Published,
                "publication failed: {}",
                publication.error_code.as_deref().unwrap_or("unknown")
            );
            return Ok(publication);
        }
    }
}

pub async fn create_operation(options: &CreateOptions) -> Result<ProjectResponse> {
    let title = options.title.trim();
    ensure!(
        !title.is_empty() && title.chars().count() <= 255,
        "title must contain 1 to 255 characters"
    );
    if options.directory.is_some() {
        crate::preflight_init_directory(options.directory.as_deref().expect("checked above"))?;
    }
    let key = operation_key("cli-project-create", options.idempotency_key.as_deref())?;
    eprintln!("Idempotency-Key: {key}");
    let client = StudioApiClient::connect(&options.connection).await?;
    let project = client
        .create_project(
            &CreateProjectRequest {
                title: title.to_owned(),
                problem_type: options.problem_type.into(),
                organization_id: options.organization_id.clone(),
            },
            &key,
        )
        .await?;
    if let Some(directory) = &options.directory {
        crate::init_project_template(directory, title, project.id, options.problem_type.into())?;
        crate::local_project::link_project(directory, &options.connection.api_url, project.id)?;
    }
    Ok(project)
}

pub async fn create(options: &CreateOptions) -> Result<()> {
    let project = create_operation(options).await?;
    println!("{}", serde_json::to_string_pretty(&project)?);
    Ok(())
}

pub async fn pull_operation(options: &PullOptions) -> Result<PullOperationResult> {
    let destination_was_empty = if options.directory.exists() {
        let metadata = fs::symlink_metadata(&options.directory)?;
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "checkout destination must be a real directory"
        );
        ensure!(
            fs::read_dir(&options.directory)?
                .next()
                .transpose()?
                .is_none(),
            "checkout destination already exists and is not empty: {}",
            options.directory.display()
        );
        true
    } else {
        false
    };
    let parent = options
        .directory
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    ensure!(
        parent.is_dir(),
        "checkout parent does not exist: {}",
        parent.display()
    );
    let client = StudioApiClient::connect(&options.connection).await?;
    let commit_id = match options.commit_id {
        Some(commit_id) => commit_id,
        None => client
            .list_commits(options.project_id)
            .await?
            .items
            .first()
            .map(|commit| commit.id)
            .context("the Studio project has no commits")?,
    };
    let commit = client.get_commit(options.project_id, commit_id).await?;
    ensure!(
        commit.project_id == options.project_id && commit.id == commit_id,
        "Studio returned a mismatched commit"
    );
    ensure!(
        commit.manifest.project_id == options.project_id && commit.manifest.commit_id == commit_id,
        "commit manifest identity mismatch"
    );
    ensure!(
        commit.manifest.digest()? == commit.manifest_digest,
        "commit manifest digest mismatch"
    );
    commit.manifest.validate_references()?;

    let staging = tempfile::Builder::new()
        .prefix(".reporch-pull-")
        .tempdir_in(parent)?;
    for file in &commit.manifest.files {
        let descriptor = client.file_download(options.project_id, &file.path).await?;
        verify_file_descriptor(file, &descriptor)?;
        let output = staging.path().join(&file.path);
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)?;
        }
        client
            .download_verified(
                &descriptor.download_url,
                file.size_bytes,
                &file.sha256,
                &output,
            )
            .await?;
    }
    write_json_new(
        &staging.path().join("reporch.problem.json"),
        &commit.manifest,
    )?;
    let authoring_spec = reporch_format::AuthoringSpecV1::from_manifest(&commit.manifest);
    crate::local_project::write_authoring_spec_create_new(staging.path(), &authoring_spec)?;
    let working_copy_revision = match client.get_working_copy(options.project_id).await {
        Ok(working_copy) if working_copy.spec == authoring_spec => {
            Some(working_copy.revision.to_string())
        }
        _ => None,
    };
    let state = crate::local_project::LocalStateV1 {
        remote: Some(crate::local_project::RemoteLinkV1 {
            api_url: options.connection.api_url.trim_end_matches('/').to_owned(),
            project_id: options.project_id,
        }),
        base_revision: working_copy_revision,
        baseline_working_digest: Some(commit.manifest_digest.to_string()),
        last_commit_id: Some(commit.id),
        ..Default::default()
    };
    crate::local_project::write_local_state(staging.path(), &state)?;
    let staging = staging.keep();
    if destination_was_empty {
        fs::remove_dir(&options.directory).with_context(|| {
            format!(
                "remove selected empty checkout directory {}",
                options.directory.display()
            )
        })?;
    }
    if let Err(error) = fs::rename(&staging, &options.directory) {
        if destination_was_empty {
            let _ = fs::create_dir(&options.directory);
        }
        return Err(error).with_context(|| {
            format!(
                "atomically install checkout {}",
                options.directory.display()
            )
        });
    }
    Ok(PullOperationResult {
        project_id: options.project_id,
        commit_id,
        directory: options.directory.clone(),
        file_count: commit.manifest.files.len(),
    })
}

pub async fn pull(options: &PullOptions) -> Result<()> {
    let pulled = pull_operation(options).await?;
    println!(
        "pulled project {} commit {} into {}",
        pulled.project_id,
        pulled.commit_id,
        pulled.directory.display()
    );
    Ok(())
}

pub async fn push_operation(options: &PushOptions) -> Result<PushOperationResult> {
    validate_wait_timeout(options.timeout_seconds)?;
    let Some(manifest_path) = options.manifest.as_ref() else {
        return push_authoring_operation(options).await;
    };
    let manifest = read_manifest(manifest_path)?;
    let issues = validate_manifest(&manifest);
    if !issues.is_empty() {
        bail!(
            "manifest validation failed with {} issue(s): {}",
            issues.len(),
            serde_json::to_string(&issues)?
        );
    }
    let source_root = options
        .source_root
        .clone()
        .or_else(|| manifest_path.parent().map(Path::to_path_buf))
        .context("manifest path has no source root")?;
    push_manifest(options, manifest, source_root).await
}

async fn push_authoring_operation(options: &PushOptions) -> Result<PushOperationResult> {
    let root = crate::local_project::discover_project(Path::new("."))?;
    if let Some(source_root) = &options.source_root {
        ensure!(
            fs::canonicalize(source_root)? == root,
            "--source-root must match the discovered reporch.yaml project root"
        );
    }
    let spec = crate::local_project::read_authoring_spec(&root)?;
    let mut state = crate::local_project::read_local_state(&root)?;
    let remote = state
        .remote
        .as_ref()
        .context("project is not linked; run reporch project link")?;
    ensure!(
        remote.project_id == spec.project_id,
        "local link and reporch.yaml project IDs differ"
    );
    ensure!(
        remote.api_url.trim_end_matches('/') == options.connection.api_url.trim_end_matches('/'),
        "linked Studio API URL differs from the selected connection"
    );

    let client = StudioApiClient::connect(&options.connection).await?;
    let capabilities = client.capabilities().await?;
    ensure_cli_compatible(&capabilities)?;
    ensure!(
        capabilities
            .authoring_spec_versions
            .iter()
            .any(|schema| schema == reporch_format::AUTHORING_SPEC_SCHEMA_V1),
        "Studio does not support {}",
        reporch_format::AUTHORING_SPEC_SCHEMA_V1
    );

    let upload_manifest = crate::local_project::compile_authoring_spec(&root, &spec, Uuid::nil())?;
    let uploaded = upload_manifest_files(&client, options, &upload_manifest, &root).await?;

    if uploaded == 0
        && let Some(last_commit_id) = state.last_commit_id
    {
        let candidate = crate::local_project::compile_authoring_spec(&root, &spec, last_commit_id)?;
        if state.baseline_working_digest.as_deref() == Some(candidate.digest()?.as_str()) {
            let commits = client.list_commits(spec.project_id).await?;
            if let Some(head) = commits.items.first()
                && head.id == last_commit_id
                && head.manifest_digest == candidate.digest()?
            {
                return Ok(PushOperationResult {
                    uploaded_files: 0,
                    commit: CommitResponse {
                        id: head.id,
                        project_id: head.project_id,
                        sequence: head.sequence,
                        manifest_digest: head.manifest_digest.clone(),
                        created_at: head.created_at,
                    },
                });
            }
        }
    }

    let remote_copy = client.get_working_copy(spec.project_id).await?;
    if let Some(base_revision) = state.base_revision.as_deref() {
        let base_revision: i64 = base_revision
            .parse()
            .context("invalid working-copy base revision in local state")?;
        ensure!(
            base_revision == remote_copy.revision,
            "revision conflict: local base {base_revision}, remote working copy {}",
            remote_copy.revision
        );
    } else if remote_copy.revision > 0 && remote_copy.spec != spec {
        bail!(
            "revision conflict: the remote working copy changed; pull or inspect the diff before pushing"
        );
    }
    let saved_copy = if remote_copy.spec == spec {
        remote_copy
    } else {
        client
            .update_working_copy(
                spec.project_id,
                remote_copy.revision,
                &UpdateWorkingCopyRequestV1 { spec: spec.clone() },
            )
            .await?
    };
    state.base_revision = Some(saved_copy.revision.to_string());
    let push_operation = format!(
        "push:{}",
        Sha256Digest::from_bytes(
            format!("{}\n{}", saved_copy.revision, options.message).as_bytes()
        )
    );
    let commit_key = state
        .pending_idempotency_keys
        .entry(push_operation)
        .or_insert(operation_key("cli-push", None)?)
        .clone();
    crate::local_project::write_local_state(&root, &state)?;
    let readiness = client.working_copy_readiness(spec.project_id).await?;
    ensure!(
        readiness.can_commit,
        "working copy is not ready: {}",
        readiness.issues.join("; ")
    );
    let commit = client
        .commit_working_copy(
            spec.project_id,
            &CommitWorkingCopyRequestV1 {
                message: options.message.clone(),
            },
            &commit_key,
        )
        .await?;
    let manifest = crate::local_project::compile_authoring_spec(&root, &spec, commit.id)?;
    let manifest_digest = manifest.digest()?;
    ensure!(
        commit.manifest_digest == manifest_digest,
        "Studio compiled a different immutable manifest"
    );
    let result = PushOperationResult {
        uploaded_files: uploaded,
        commit,
    };

    crate::local_project::write_generated_manifest_atomic(&root, &manifest)?;
    state.baseline_working_digest = Some(manifest_digest.to_string());
    state.last_commit_id = Some(result.commit.id);
    state.last_validation_run_id = None;
    state.last_release_id = None;
    state
        .pending_idempotency_keys
        .retain(|key, _| !key.starts_with("push:"));
    crate::local_project::write_local_state(&root, &state)?;
    Ok(result)
}

async fn push_manifest(
    options: &PushOptions,
    manifest: ReleaseManifestV1,
    source_root: PathBuf,
) -> Result<PushOperationResult> {
    let client = StudioApiClient::connect(&options.connection).await?;
    let uploaded = upload_manifest_files(&client, options, &manifest, &source_root).await?;
    let manifest_digest = manifest.digest()?;
    if uploaded == 0 {
        let commits = client.list_commits(manifest.project_id).await?;
        if let Some(head) = commits.items.first()
            && is_unchanged_remote_head(
                uploaded,
                manifest.commit_id,
                &manifest_digest,
                head.id,
                &head.manifest_digest,
            )
        {
            return Ok(PushOperationResult {
                uploaded_files: 0,
                commit: CommitResponse {
                    id: head.id,
                    project_id: head.project_id,
                    sequence: head.sequence,
                    manifest_digest: head.manifest_digest.clone(),
                    created_at: head.created_at,
                },
            });
        }
    }
    let commit = client
        .create_commit(
            manifest.project_id,
            &CreateCommitRequest {
                message: options.message.clone(),
                manifest: manifest.clone(),
            },
        )
        .await?;
    ensure!(
        commit.id == manifest.commit_id,
        "Studio returned a mismatched commit ID"
    );
    ensure!(
        commit.manifest_digest == manifest_digest,
        "Studio returned a mismatched manifest digest"
    );
    Ok(PushOperationResult {
        uploaded_files: uploaded,
        commit,
    })
}

async fn upload_manifest_files(
    client: &StudioApiClient,
    options: &PushOptions,
    manifest: &ReleaseManifestV1,
    source_root: &Path,
) -> Result<usize> {
    let remote = client
        .list_files(manifest.project_id)
        .await?
        .items
        .into_iter()
        .map(|file| (file.path.clone(), file))
        .collect::<HashMap<_, _>>();
    let mut uploaded = 0_usize;
    for file in &manifest.files {
        let source = source_root.join(&file.path);
        verify_local_file(&source, file).await?;
        if remote.get(&file.path).is_some_and(|remote| {
            remote.sha256 == file.sha256
                && remote.size_bytes == file.size_bytes
                && remote.media_type == file.media_type
                && remote.executable == file.executable
        }) {
            continue;
        }
        let upload = client
            .begin_upload(
                manifest.project_id,
                &BeginFileUploadRequest {
                    path: file.path.clone(),
                    sha256: file.sha256.clone(),
                    size_bytes: file.size_bytes,
                    media_type: file.media_type.clone(),
                    executable: file.executable,
                },
            )
            .await?;
        client.upload_file(&source, &upload).await?;
        let status = client
            .complete_upload(manifest.project_id, upload.upload.id)
            .await?;
        wait_for_upload(client, manifest.project_id, status, options.timeout_seconds).await?;
        uploaded += 1;
    }
    Ok(uploaded)
}

fn is_unchanged_remote_head(
    uploaded_files: usize,
    local_commit_id: Uuid,
    local_digest: &Sha256Digest,
    remote_commit_id: Uuid,
    remote_digest: &Sha256Digest,
) -> bool {
    uploaded_files == 0 && local_commit_id == remote_commit_id && local_digest == remote_digest
}

pub async fn push(options: &PushOptions) -> Result<()> {
    let pushed = push_operation(options).await?;
    println!(
        "pushed {} file(s); commit {} sequence {} digest {}",
        pushed.uploaded_files,
        pushed.commit.id,
        pushed.commit.sequence,
        pushed.commit.manifest_digest
    );
    Ok(())
}

pub async fn validate_operation(options: &ValidateOptions) -> Result<ValidationOperationResult> {
    validate_wait_timeout(options.timeout_seconds)?;
    let (project_id, commit_id, local_root) =
        resolve_local_candidate(options.project_id, options.commit_id)?;
    let key = operation_key("cli-validation", options.idempotency_key.as_deref())?;
    eprintln!("Idempotency-Key: {key}");
    let client = StudioApiClient::connect(&options.connection).await?;
    let queued = client
        .enqueue_validation(project_id, &EnqueueValidationRequest { commit_id }, &key)
        .await?;
    ensure!(
        queued.project_id == project_id && queued.commit_id == commit_id,
        "Studio returned a mismatched validation run"
    );
    if !options.wait {
        let result = ValidationOperationResult {
            queued,
            detail: None,
        };
        if let Some(root) = local_root {
            let mut state = crate::local_project::read_local_state(&root)?;
            state.last_validation_run_id = Some(result.queued.id);
            crate::local_project::write_local_state(&root, &state)?;
        }
        return Ok(result);
    }
    let detail =
        wait_for_validation(&client, project_id, queued.id, options.timeout_seconds).await?;
    let result = ValidationOperationResult {
        queued,
        detail: Some(detail),
    };
    if let Some(root) = local_root {
        let mut state = crate::local_project::read_local_state(&root)?;
        state.last_validation_run_id = Some(result.queued.id);
        crate::local_project::write_local_state(&root, &state)?;
    }
    Ok(result)
}

pub async fn validation_show_operation(
    options: &ValidationInspectOptions,
) -> Result<ValidationRunDetailResponse> {
    let (project_id, validation_id) =
        resolve_local_validation(options.project_id, options.validation_run_id)?;
    StudioApiClient::connect(&options.connection)
        .await?
        .get_validation(project_id, validation_id)
        .await
}

pub async fn validation_watch_operation(
    options: &ValidationInspectOptions,
) -> Result<ValidationRunDetailResponse> {
    validate_wait_timeout(options.timeout_seconds)?;
    let (project_id, validation_id) =
        resolve_local_validation(options.project_id, options.validation_run_id)?;
    let client = StudioApiClient::connect(&options.connection).await?;
    wait_for_validation(&client, project_id, validation_id, options.timeout_seconds).await
}

pub async fn validate(options: &ValidateOptions) -> Result<()> {
    let validation = validate_operation(options).await?;
    if let Some(detail) = validation.detail {
        println!("{}", serde_json::to_string_pretty(&detail)?);
        if detail.status != ValidationRunStatus::Passed {
            bail!("Studio validation did not pass");
        }
    } else {
        println!("{}", serde_json::to_string_pretty(&validation.queued)?);
    }
    Ok(())
}

pub async fn build_release_operation(options: &ReleaseBuildOptions) -> Result<ReleaseResponse> {
    validate_wait_timeout(options.timeout_seconds)?;
    let (project_id, commit_id, local_root) =
        resolve_local_candidate(options.project_id, options.commit_id)?;
    let validation_run_id = match options.validation_run_id {
        Some(id) => id,
        None => {
            let root = local_root
                .as_ref()
                .context("--validation-run-id is required outside a local project")?;
            crate::local_project::read_local_state(root)?
                .last_validation_run_id
                .context("no validation is recorded; run reporch verify first")?
        }
    };
    let key = operation_key("cli-release", options.idempotency_key.as_deref())?;
    eprintln!("Idempotency-Key: {key}");
    let client = StudioApiClient::connect(&options.connection).await?;
    let release = client
        .create_release(
            project_id,
            &CreateReleaseRequest {
                commit_id,
                validation_run_id,
            },
            &key,
        )
        .await?;
    let release = if options.wait {
        wait_for_release(&client, project_id, release, options.timeout_seconds).await?
    } else {
        release
    };
    if options.wait {
        ensure!(
            release.status == ReleaseStatus::Ready,
            "release build failed: {}",
            release.error_code.as_deref().unwrap_or("unknown")
        );
    }
    if let Some(root) = local_root {
        let mut state = crate::local_project::read_local_state(&root)?;
        state.last_release_id = Some(release.id);
        crate::local_project::write_local_state(&root, &state)?;
    }
    Ok(release)
}

pub async fn list_releases_operation(options: &ReleaseScopeOptions) -> Result<ReleasePage> {
    let project_id = resolve_local_project_id(options.project_id)?;
    let client = StudioApiClient::connect(&options.connection).await?;
    let mut items = Vec::new();
    let mut cursor = None;
    let mut seen = std::collections::HashSet::new();
    for _ in 0..100 {
        let page = client.list_releases(project_id, cursor).await?;
        ensure!(
            page.items
                .iter()
                .all(|release| release.project_id == project_id),
            "Studio returned a release from another project"
        );
        items.extend(page.items);
        let Some(next_cursor) = page.next_cursor else {
            return Ok(ReleasePage {
                items,
                next_cursor: None,
            });
        };
        ensure!(
            seen.insert(next_cursor),
            "Studio returned a repeated release cursor"
        );
        cursor = Some(next_cursor);
    }
    bail!("Studio release listing exceeded the 10,000-item native client bound")
}

pub async fn show_release_operation(options: &ReleaseShowOptions) -> Result<ReleaseResponse> {
    let (project_id, release_id) =
        resolve_local_release(options.scope.project_id, options.release_id)?;
    StudioApiClient::connect(&options.scope.connection)
        .await?
        .get_release(project_id, release_id)
        .await
}

pub async fn download_release_operation(
    options: &ReleaseDownloadOptions,
) -> Result<PackageOperationResult> {
    ensure!(
        !options.output.exists(),
        "refusing to overwrite {}",
        options.output.display()
    );
    let (project_id, release_id) =
        resolve_local_release(options.target.scope.project_id, options.target.release_id)?;
    let client = StudioApiClient::connect(&options.target.scope.connection).await?;
    let release = client.get_release(project_id, release_id).await?;
    ensure!(
        release.status == ReleaseStatus::Ready,
        "release is not ready: {:?}",
        release.status
    );
    download_release_to(&client, project_id, release, &options.output).await
}

pub async fn package_operation(options: &PackageOptions) -> Result<PackageOperationResult> {
    validate_wait_timeout(options.timeout_seconds)?;
    let (project_id, commit_id, local_root) =
        resolve_local_candidate(options.project_id, options.commit_id)?;
    let validation_run_id = match options.validation_run_id {
        Some(id) => id,
        None => {
            let root = local_root
                .as_ref()
                .context("--validation-run-id is required outside a local project")?;
            crate::local_project::read_local_state(root)?
                .last_validation_run_id
                .context("no validation is recorded; run reporch project validate first")?
        }
    };
    ensure!(
        !options.output.exists(),
        "refusing to overwrite {}",
        options.output.display()
    );
    let key = operation_key("cli-release", options.idempotency_key.as_deref())?;
    eprintln!("Idempotency-Key: {key}");
    let client = StudioApiClient::connect(&options.connection).await?;
    let release = client
        .create_release(
            project_id,
            &CreateReleaseRequest {
                commit_id,
                validation_run_id,
            },
            &key,
        )
        .await?;
    let release = wait_for_release(&client, project_id, release, options.timeout_seconds).await?;
    ensure!(
        release.status == ReleaseStatus::Ready,
        "release build failed: {:?}",
        release.error_code
    );
    let result = download_release_to(&client, project_id, release, &options.output).await?;
    if let Some(root) = local_root {
        let mut state = crate::local_project::read_local_state(&root)?;
        state.last_release_id = Some(result.release.id);
        crate::local_project::write_local_state(&root, &state)?;
    }
    Ok(result)
}

async fn download_release_to(
    client: &StudioApiClient,
    project_id: Uuid,
    release: ReleaseResponse,
    output: &Path,
) -> Result<PackageOperationResult> {
    let download = client.release_download(project_id, release.id).await?;
    ensure!(
        download.release_id == release.id,
        "Studio returned a mismatched release download"
    );
    ensure!(
        release.package_digest.as_ref() == Some(&download.package_digest),
        "release digest changed before download"
    );
    ensure!(
        release.package_size_bytes == Some(download.package_size_bytes),
        "release size changed before download"
    );
    client
        .download_verified(
            &download.download_url,
            download.package_size_bytes,
            &download.package_digest,
            output,
        )
        .await?;
    Ok(PackageOperationResult {
        release,
        output: output.to_owned(),
        package_digest: download.package_digest,
        package_size_bytes: download.package_size_bytes,
    })
}

pub async fn package(options: &PackageOptions) -> Result<()> {
    let packaged = package_operation(options).await?;
    println!(
        "downloaded release {} to {}: {} bytes, sha256 {}",
        packaged.release.id,
        packaged.output.display(),
        packaged.package_size_bytes,
        packaged.package_digest
    );
    Ok(())
}

pub async fn submit_review_operation(options: &SubmitReviewOptions) -> Result<ReviewResponse> {
    let (project_id, commit_id, local_root) =
        resolve_local_candidate(options.project_id, options.commit_id)?;
    let validation_run_id = match options.validation_run_id {
        Some(validation_run_id) => validation_run_id,
        None => {
            let root = local_root
                .as_ref()
                .context("validation run ID was omitted outside a local project")?;
            crate::local_project::read_local_state(root)?
                .last_validation_run_id
                .context("no validation is recorded; run reporch verify first")?
        }
    };
    let key = operation_key("cli-review-submit", options.idempotency_key.as_deref())?;
    eprintln!("Idempotency-Key: {key}");
    let client = StudioApiClient::connect(&options.connection).await?;
    let review = client
        .submit_review(
            project_id,
            &SubmitReviewRequest {
                commit_id,
                validation_run_id,
            },
            &key,
        )
        .await?;
    ensure_review_scope(&review, project_id, Some(commit_id))?;
    ensure!(
        review.validation_run_id == validation_run_id,
        "Studio returned a review for another validation"
    );
    Ok(review)
}

pub async fn submit_review(options: &SubmitReviewOptions) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(&submit_review_operation(options).await?)?
    );
    Ok(())
}

pub async fn list_reviews_operation(options: &ListReviewsOptions) -> Result<ReviewPage> {
    let project_id = resolve_local_project_id(options.project_id)?;
    let client = StudioApiClient::connect(&options.connection).await?;
    let mut items = Vec::new();
    let mut cursor = None;
    let mut seen_cursors = std::collections::HashSet::new();
    for _ in 0..100 {
        let page = client.list_reviews(project_id, cursor.as_deref()).await?;
        ensure!(
            page.items
                .iter()
                .all(|review| review.project_id == project_id),
            "Studio returned a review from another project"
        );
        items.extend(page.items);
        let Some(next_cursor) = page.next_cursor else {
            return Ok(ReviewPage {
                items,
                next_cursor: None,
            });
        };
        ensure!(
            seen_cursors.insert(next_cursor.clone()),
            "Studio returned a repeated review cursor"
        );
        cursor = Some(next_cursor);
    }
    bail!("Studio review listing exceeded the 10,000-item native client bound")
}

pub async fn list_reviews(options: &ListReviewsOptions) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(&list_reviews_operation(options).await?)?
    );
    Ok(())
}

pub async fn request_review_pool_operation(
    options: &ReviewPoolRequestOptions,
) -> Result<ReviewPoolRequestResponseV1> {
    ensure!(options.pool, "review request requires --pool");
    let project_id = resolve_local_project_id(options.project_id)?;
    let key = operation_key(
        "cli-review-pool-request",
        options.idempotency_key.as_deref(),
    )?;
    eprintln!("Idempotency-Key: {key}");
    let response = StudioApiClient::connect(&options.connection)
        .await?
        .request_review_pool(project_id, options.review_id, &key)
        .await?;
    ensure!(
        response.project_id == project_id && response.review_id == options.review_id,
        "Studio returned a review pool request outside the requested candidate"
    );
    Ok(response)
}

pub async fn review_pool_status_operation(
    options: &ReviewPoolTargetOptions,
) -> Result<ReviewPoolRequestResponseV1> {
    let response = StudioApiClient::connect(&options.connection)
        .await?
        .get_review_pool_request(options.pool_request_id)
        .await?;
    ensure!(
        response.id == options.pool_request_id,
        "Studio returned a different review pool request"
    );
    Ok(response)
}

pub async fn list_review_pool_inbox_operation(
    options: &ReviewPoolInboxOptions,
) -> Result<ReviewPoolPageV1> {
    let client = StudioApiClient::connect(&options.connection).await?;
    let mut items = Vec::new();
    let mut cursor = None;
    let mut seen = std::collections::HashSet::new();
    for _ in 0..100 {
        let page = client.list_review_pool_inbox(cursor).await?;
        items.extend(page.items);
        let Some(next) = page.next_cursor else {
            return Ok(ReviewPoolPageV1 {
                items,
                next_cursor: None,
            });
        };
        ensure!(
            seen.insert(next),
            "Studio returned a repeated review pool cursor"
        );
        cursor = Some(next);
    }
    bail!("Studio review pool inbox exceeded the 10,000-item native client bound")
}

pub async fn claim_review_pool_operation(
    options: &ReviewPoolTargetOptions,
) -> Result<ReviewPoolRequestResponseV1> {
    let response = StudioApiClient::connect(&options.connection)
        .await?
        .claim_review_pool_request(options.pool_request_id)
        .await?;
    ensure!(
        response.id == options.pool_request_id && response.assignment_id.is_some(),
        "Studio returned an invalid review pool assignment"
    );
    Ok(response)
}

pub async fn cancel_review_pool_operation(
    options: &ReviewPoolTargetOptions,
) -> Result<ReviewPoolRequestResponseV1> {
    let response = StudioApiClient::connect(&options.connection)
        .await?
        .cancel_review_pool_request(options.pool_request_id)
        .await?;
    ensure!(
        response.id == options.pool_request_id,
        "Studio returned a different review pool request"
    );
    Ok(response)
}

pub async fn approve_review_operation(options: &ApproveReviewOptions) -> Result<ReviewResponse> {
    let comment = normalize_optional_review_comment(options.comment.as_deref())?;
    decide_review_target(
        &options.connection,
        options.project_id,
        options.review_id,
        options.pool_request_id,
        ReviewDecisionKindV1::Approve,
        comment,
        options.idempotency_key.as_deref(),
    )
    .await
}

pub async fn list_waivers_operation(options: &WaiverScopeOptions) -> Result<WaiverPage> {
    let (project_id, validation_id) =
        resolve_local_validation(options.project_id, options.validation_run_id)?;
    StudioApiClient::connect(&options.connection)
        .await?
        .list_waivers(project_id, validation_id)
        .await
}

pub async fn create_waiver_operation(options: &CreateWaiverOptions) -> Result<WaiverResponse> {
    let issue_code = options.issue_code.trim();
    let reason = options.reason.trim();
    ensure!(!issue_code.is_empty(), "issue code cannot be empty");
    ensure!(
        (20..=2_000).contains(&reason.len()),
        "waiver reason must contain 20 to 2000 bytes"
    );
    let expires_at = DateTime::parse_from_rfc3339(&options.expires_at)
        .context("--expires-at must be an RFC 3339 timestamp")?
        .with_timezone(&Utc);
    ensure!(
        expires_at > Utc::now(),
        "waiver expiry must be in the future"
    );
    ensure!(
        expires_at <= Utc::now() + chrono::Duration::days(90),
        "waiver expiry cannot be more than 90 days in the future"
    );
    let (project_id, validation_id) =
        resolve_local_validation(options.scope.project_id, options.scope.validation_run_id)?;
    let key = operation_key("cli-waiver-create", options.idempotency_key.as_deref())?;
    StudioApiClient::connect(&options.scope.connection)
        .await?
        .create_waiver(
            project_id,
            validation_id,
            &CreateWaiverRequest {
                issue_code: issue_code.into(),
                reason: reason.into(),
                expires_at,
            },
            &key,
        )
        .await
}

pub async fn revoke_waiver_operation(options: &RevokeWaiverOptions) -> Result<WaiverResponse> {
    let reason = options.reason.trim();
    ensure!(
        (10..=2_000).contains(&reason.len()),
        "revocation reason must contain 10 to 2000 bytes"
    );
    let (project_id, validation_id) =
        resolve_local_validation(options.scope.project_id, options.scope.validation_run_id)?;
    let key = operation_key("cli-waiver-revoke", options.idempotency_key.as_deref())?;
    StudioApiClient::connect(&options.scope.connection)
        .await?
        .revoke_waiver(
            project_id,
            validation_id,
            options.waiver_id,
            &RevokeWaiverRequest {
                reason: reason.into(),
            },
            &key,
        )
        .await
}

pub async fn request_review_changes_operation(
    options: &RequestChangesOptions,
) -> Result<ReviewResponse> {
    let comment = normalize_required_review_comment(&options.comment)?;
    decide_review_target(
        &options.connection,
        options.project_id,
        options.review_id,
        options.pool_request_id,
        ReviewDecisionKindV1::RequestChanges,
        Some(comment),
        options.idempotency_key.as_deref(),
    )
    .await
}

pub async fn approve_review(options: &ApproveReviewOptions) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(&approve_review_operation(options).await?)?
    );
    Ok(())
}

pub async fn request_review_changes(options: &RequestChangesOptions) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(&request_review_changes_operation(options).await?)?
    );
    Ok(())
}

async fn decide_review_target(
    connection: &RemoteConnectionOptions,
    project_id: Option<Uuid>,
    review_id: Option<Uuid>,
    pool_request_id: Option<Uuid>,
    decision: ReviewDecisionKindV1,
    comment: Option<String>,
    provided_key: Option<&str>,
) -> Result<ReviewResponse> {
    match pool_request_id {
        Some(request_id) => {
            ensure!(
                project_id.is_none() && review_id.is_none(),
                "--pool-request-id cannot be combined with --project-id or --review-id"
            );
            let key = operation_key("cli-review-pool-decision", provided_key)?;
            eprintln!("Idempotency-Key: {key}");
            let review = StudioApiClient::connect(connection)
                .await?
                .decide_pool_review(
                    request_id,
                    &CreateReviewDecisionRequest { decision, comment },
                    &key,
                )
                .await?;
            ensure!(
                review.decision.as_ref().is_some_and(|record| {
                    record.decision == decision
                        && record.pool_assignment_id.is_some()
                        && record.approval_source == studio_core::ReviewApprovalSourceV1::ReviewPool
                }),
                "Studio returned a mismatched review pool decision"
            );
            Ok(review)
        }
        None => {
            decide_review(
                connection,
                project_id.context("--project-id is required without --pool-request-id")?,
                review_id.context("--review-id is required without --pool-request-id")?,
                decision,
                comment,
                provided_key,
            )
            .await
        }
    }
}

async fn decide_review(
    connection: &RemoteConnectionOptions,
    project_id: Uuid,
    review_id: Uuid,
    decision: ReviewDecisionKindV1,
    comment: Option<String>,
    provided_key: Option<&str>,
) -> Result<ReviewResponse> {
    let key = operation_key("cli-review-decision", provided_key)?;
    eprintln!("Idempotency-Key: {key}");
    let client = StudioApiClient::connect(connection).await?;
    let review = client
        .decide_review(
            project_id,
            review_id,
            &CreateReviewDecisionRequest { decision, comment },
            &key,
        )
        .await?;
    ensure_review_scope(&review, project_id, None)?;
    ensure!(review.id == review_id, "Studio returned a different review");
    ensure!(
        review
            .decision
            .as_ref()
            .is_some_and(|record| record.decision == decision),
        "Studio returned a mismatched review decision"
    );
    Ok(review)
}

fn ensure_review_scope(
    review: &ReviewResponse,
    project_id: Uuid,
    commit_id: Option<Uuid>,
) -> Result<()> {
    ensure!(
        review.project_id == project_id,
        "Studio returned a review from another project"
    );
    if let Some(commit_id) = commit_id {
        ensure!(
            review.commit_id == commit_id,
            "Studio returned a review for another commit"
        );
    }
    Ok(())
}

fn normalize_optional_review_comment(comment: Option<&str>) -> Result<Option<String>> {
    comment.map(normalize_required_review_comment).transpose()
}

fn normalize_required_review_comment(comment: &str) -> Result<String> {
    let comment = comment.trim();
    ensure!(!comment.is_empty(), "review comment must not be empty");
    ensure!(
        comment.len() <= 4_000,
        "review comment must not exceed 4000 bytes"
    );
    Ok(comment.to_owned())
}

async fn wait_for_upload(
    client: &StudioApiClient,
    project_id: Uuid,
    mut status: FileUploadResponse,
    timeout_seconds: u64,
) -> Result<FileUploadResponse> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_seconds);
    loop {
        match status.status {
            FileUploadStatus::Ready => return Ok(status),
            FileUploadStatus::Rejected | FileUploadStatus::Expired => {
                bail!("file upload failed: {:?}", status.error_code)
            }
            FileUploadStatus::AwaitingUpload
            | FileUploadStatus::Queued
            | FileUploadStatus::Verifying => {}
        }
        ensure!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for file verification"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
        status = retry_transient_poll(deadline, "file verification status", || {
            client.get_upload(project_id, status.id)
        })
        .await?;
    }
}

async fn wait_for_validation(
    client: &StudioApiClient,
    project_id: Uuid,
    validation_id: Uuid,
    timeout_seconds: u64,
) -> Result<ValidationRunDetailResponse> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_seconds);
    loop {
        let status = retry_transient_poll(deadline, "validation status", || {
            client.get_validation(project_id, validation_id)
        })
        .await?;
        if matches!(
            status.status,
            ValidationRunStatus::Passed
                | ValidationRunStatus::Failed
                | ValidationRunStatus::Cancelled
        ) {
            return Ok(status);
        }
        ensure!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for validation"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn wait_for_release(
    client: &StudioApiClient,
    project_id: Uuid,
    mut release: ReleaseResponse,
    timeout_seconds: u64,
) -> Result<ReleaseResponse> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_seconds);
    loop {
        match release.status {
            ReleaseStatus::Ready | ReleaseStatus::Failed => return Ok(release),
            ReleaseStatus::Queued | ReleaseStatus::Building => {}
        }
        ensure!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for release package"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
        release = retry_transient_poll(deadline, "release status", || {
            client.get_release(project_id, release.id)
        })
        .await?;
    }
}

async fn retry_transient_poll<T, F, Fut>(
    deadline: tokio::time::Instant,
    operation: &str,
    mut request: F,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut attempt = 0_u32;
    loop {
        match request().await {
            Ok(value) => return Ok(value),
            Err(error)
                if is_transient_api_error(&error) && attempt < MAX_TRANSIENT_POLL_RETRIES =>
            {
                attempt += 1;
                let delay = transient_poll_delay(attempt);
                ensure!(
                    tokio::time::Instant::now() + delay < deadline,
                    "timed out while retrying {operation}"
                );
                eprintln!(
                    "temporary {operation} error; retrying in {} ms ({attempt}/{MAX_TRANSIENT_POLL_RETRIES})",
                    delay.as_millis()
                );
                tokio::time::sleep(delay).await;
            }
            Err(error) => return Err(error).with_context(|| format!("poll {operation}")),
        }
    }
}

fn transient_poll_delay(attempt: u32) -> Duration {
    let exponent = attempt.saturating_sub(1).min(5);
    Duration::from_millis((250_u64 << exponent).min(5_000))
}

fn is_transient_api_error(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<StudioApiRequestError>()
        .is_some_and(StudioApiRequestError::is_transient)
}

fn is_transient_http_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::REQUEST_TIMEOUT
            | StatusCode::TOO_EARLY
            | StatusCode::TOO_MANY_REQUESTS
            | StatusCode::INTERNAL_SERVER_ERROR
            | StatusCode::BAD_GATEWAY
            | StatusCode::SERVICE_UNAVAILABLE
            | StatusCode::GATEWAY_TIMEOUT
    )
}

fn operation_key(prefix: &str, provided: Option<&str>) -> Result<String> {
    let key = provided
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{prefix}-{}", Uuid::now_v7()));
    ensure!(
        (8..=255).contains(&key.len()) && !key.chars().any(char::is_whitespace),
        "idempotency key must contain 8 to 255 non-whitespace characters"
    );
    Ok(key)
}

fn ensure_cli_compatible(capabilities: &StudioCapabilitiesV1) -> Result<()> {
    let current = parse_numeric_version(env!("CARGO_PKG_VERSION"))?;
    let minimum = parse_numeric_version(&capabilities.minimum_cli_version)
        .context("Studio returned an invalid minimum CLI version")?;
    ensure!(
        current >= minimum,
        "this Studio requires Reporch CLI {} or newer; current version is {}",
        capabilities.minimum_cli_version,
        env!("CARGO_PKG_VERSION")
    );
    ensure!(
        current.0 <= capabilities.maximum_cli_major,
        "Studio supports Reporch CLI major versions through {}; current version is {}",
        capabilities.maximum_cli_major,
        env!("CARGO_PKG_VERSION")
    );
    Ok(())
}

fn parse_numeric_version(value: &str) -> Result<(u64, u64, u64)> {
    let numeric = value.split_once('-').map_or(value, |(numeric, _)| numeric);
    let mut parts = numeric.split('.');
    let major = parts
        .next()
        .context("version is missing a major component")?;
    let minor = parts
        .next()
        .context("version is missing a minor component")?;
    let patch = parts
        .next()
        .context("version is missing a patch component")?;
    ensure!(parts.next().is_none(), "version has too many components");
    Ok((major.parse()?, minor.parse()?, patch.parse()?))
}

fn resolve_local_candidate(
    project_id: Option<Uuid>,
    commit_id: Option<Uuid>,
) -> Result<(Uuid, Uuid, Option<PathBuf>)> {
    if let (Some(project_id), Some(commit_id)) = (project_id, commit_id) {
        return Ok((project_id, commit_id, None));
    }
    let root = crate::local_project::discover_project(Path::new("."))
        .context("project and commit IDs were omitted and no local project was found")?;
    let spec = crate::local_project::read_authoring_spec(&root)?;
    let state = crate::local_project::read_local_state(&root)?;
    let resolved_project_id = project_id.unwrap_or(spec.project_id);
    ensure!(
        resolved_project_id == spec.project_id,
        "explicit project ID does not match reporch.yaml"
    );
    if let Some(remote) = &state.remote {
        ensure!(
            remote.project_id == resolved_project_id,
            "local remote link does not match reporch.yaml"
        );
    }
    let resolved_commit_id = commit_id
        .or(state.last_commit_id)
        .context("no commit is recorded; run reporch project push first")?;
    Ok((resolved_project_id, resolved_commit_id, Some(root)))
}

fn resolve_local_project_id(project_id: Option<Uuid>) -> Result<Uuid> {
    if let Some(project_id) = project_id {
        return Ok(project_id);
    }
    let root = crate::local_project::discover_project(Path::new("."))
        .context("project ID was omitted and no local project was found")?;
    let spec = crate::local_project::read_authoring_spec(&root)?;
    let state = crate::local_project::read_local_state(&root)?;
    let remote = state
        .remote
        .context("project is not linked; run reporch project link")?;
    ensure!(
        remote.project_id == spec.project_id,
        "local link and reporch.yaml project IDs differ"
    );
    Ok(spec.project_id)
}

pub fn current_project_id(project_id: Option<Uuid>) -> Result<Uuid> {
    resolve_local_project_id(project_id)
}

fn resolve_local_release(
    project_id: Option<Uuid>,
    release_id: Option<Uuid>,
) -> Result<(Uuid, Uuid)> {
    let project_id = resolve_local_project_id(project_id)?;
    if let Some(release_id) = release_id {
        return Ok((project_id, release_id));
    }
    let root = crate::local_project::discover_project(Path::new("."))?;
    let state = crate::local_project::read_local_state(&root)?;
    let release_id = state
        .last_release_id
        .context("no local release is recorded; pass --release-id or build a release")?;
    Ok((project_id, release_id))
}

fn resolve_local_validation(
    project_id: Option<Uuid>,
    validation_id: Option<Uuid>,
) -> Result<(Uuid, Uuid)> {
    let project_id = resolve_local_project_id(project_id)?;
    if let Some(validation_id) = validation_id {
        return Ok((project_id, validation_id));
    }
    let root = crate::local_project::discover_project(Path::new("."))?;
    let state = crate::local_project::read_local_state(&root)?;
    let validation_id = state
        .last_validation_run_id
        .context("no validation is recorded; pass --validation-run-id or run verify")?;
    Ok((project_id, validation_id))
}

fn is_publication_terminal(status: PublicationStatus) -> bool {
    matches!(
        status,
        PublicationStatus::Published | PublicationStatus::Failed
    )
}

fn validate_wait_timeout(value: u64) -> Result<()> {
    ensure!(
        (1..=7_200).contains(&value),
        "timeout must be between 1 and 7200 seconds"
    );
    Ok(())
}

fn read_manifest(path: &Path) -> Result<ReleaseManifestV1> {
    let metadata = fs::metadata(path).with_context(|| format!("inspect {}", path.display()))?;
    ensure!(
        metadata.is_file() && metadata.len() <= 32 * 1024 * 1024,
        "manifest must be a file no larger than 32 MiB"
    );
    serde_json::from_slice(&fs::read(path)?).with_context(|| format!("parse {}", path.display()))
}

async fn verify_local_file(source: &Path, expected: &ManifestFile) -> Result<()> {
    let metadata =
        fs::symlink_metadata(source).with_context(|| format!("inspect {}", source.display()))?;
    ensure!(
        metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
        "manifest source is not a regular file: {}",
        source.display()
    );
    ensure!(
        metadata.len() == expected.size_bytes,
        "source size mismatch: {}",
        expected.path
    );
    let mut file = tokio::fs::File::open(source).await?;
    let mut buffer = vec![0_u8; 1024 * 1024];
    let mut size = 0_u64;
    let mut digest = Sha256::new();
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        size = size
            .checked_add(read as u64)
            .context("source size overflow")?;
        ensure!(
            size <= expected.size_bytes,
            "source grew while hashing: {}",
            expected.path
        );
        digest.update(&buffer[..read]);
    }
    let digest: Sha256Digest = hex::encode(digest.finalize()).parse()?;
    ensure!(
        size == expected.size_bytes && digest == expected.sha256,
        "source digest mismatch: {}",
        expected.path
    );
    Ok(())
}

fn verify_file_descriptor(
    expected: &ManifestFile,
    descriptor: &FileDownloadResponse,
) -> Result<()> {
    let actual = &descriptor.file;
    ensure!(actual.path == expected.path, "file download path mismatch");
    ensure!(
        actual.sha256 == expected.sha256,
        "file download digest mismatch"
    );
    ensure!(
        actual.size_bytes == expected.size_bytes,
        "file download size mismatch"
    );
    ensure!(
        actual.media_type == expected.media_type,
        "file download media type mismatch"
    );
    ensure!(
        actual.executable == expected.executable,
        "file download executable flag mismatch"
    );
    Ok(())
}

fn write_json_new(path: &Path, value: &impl Serialize) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("create {} without overwrite", path.display()))?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(())
}

fn validate_api_url(value: &str, allow_insecure_loopback: bool) -> Result<Url> {
    let mut url = Url::parse(value).context("invalid Studio API URL")?;
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "Studio API URL must not contain credentials"
    );
    ensure!(
        url.query().is_none() && url.fragment().is_none(),
        "Studio API URL must not contain query or fragment components"
    );
    let secure = url.scheme() == "https";
    let insecure_loopback = url.scheme() == "http"
        && allow_insecure_loopback
        && url.host().is_some_and(is_loopback_host);
    ensure!(
        secure || insecure_loopback,
        "Studio API URL must use HTTPS (HTTP is development-only on loopback)"
    );
    url.set_path("/api/v1/");
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

fn validate_signed_object_url(value: &str, allow_insecure_loopback: bool) -> Result<Url> {
    let url = Url::parse(value).context("Studio returned an invalid signed object URL")?;
    ensure!(
        url.username().is_empty() && url.password().is_none() && url.fragment().is_none(),
        "signed object URL contains forbidden components"
    );
    let azure_host = url.host_str().is_some_and(|host| {
        [
            ".blob.core.windows.net",
            ".blob.core.usgovcloudapi.net",
            ".blob.core.chinacloudapi.cn",
        ]
        .iter()
        .any(|suffix| host.ends_with(suffix) && host.len() > suffix.len())
    });
    let secure_azure = url.scheme() == "https" && azure_host;
    let insecure_loopback = url.scheme() == "http"
        && allow_insecure_loopback
        && url.host().is_some_and(is_loopback_host);
    ensure!(
        secure_azure || insecure_loopback,
        "signed object URL host is not approved"
    );
    Ok(url)
}

fn is_loopback_host(host: Host<&str>) -> bool {
    match host {
        Host::Domain(name) => name.eq_ignore_ascii_case("localhost"),
        Host::Ipv4(address) => address.is_loopback(),
        Host::Ipv6(address) => address.is_loopback(),
    }
}

async fn decode_api_response<T: DeserializeOwned>(response: Response) -> Result<T> {
    let status = response.status();
    let limit = if status.is_success() {
        MAX_JSON_RESPONSE_BYTES
    } else {
        MAX_ERROR_RESPONSE_BYTES
    };
    let bytes = read_response_bounded(response, limit).await?;
    if !status.is_success() {
        if let Ok(error) = serde_json::from_slice::<ApiErrorResponse>(&bytes) {
            return Err(StudioApiRequestError::Api {
                status,
                error_code: error.error_code,
                message: error.message,
                retryable: error.retryable,
                trace_id: error.trace_id,
            }
            .into());
        }
        return Err(StudioApiRequestError::Http { status }.into());
    }
    serde_json::from_slice(&bytes).context("decode Studio API response")
}

fn redact_reqwest_error(error: reqwest::Error) -> anyhow::Error {
    anyhow::Error::new(error.without_url())
}

fn studio_api_transport_error(error: reqwest::Error) -> anyhow::Error {
    StudioApiRequestError::Transport {
        source: error.without_url(),
    }
    .into()
}

async fn read_response_bounded(response: Response, limit: usize) -> Result<Vec<u8>> {
    if let Some(content_length) = response.content_length() {
        ensure!(
            content_length <= limit as u64,
            "Studio response exceeds the size limit"
        );
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(redact_reqwest_error)?;
        ensure!(
            body.len().saturating_add(chunk.len()) <= limit,
            "Studio response exceeds the size limit"
        );
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

async fn expect_object_success(response: Response) -> Result<()> {
    if response.status().is_success() {
        return Ok(());
    }
    let status = response.status();
    let _ = read_response_bounded(response, MAX_ERROR_RESPONSE_BYTES).await;
    bail!("signed object upload failed with HTTP {status}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn studio_api_url_is_https_or_explicit_loopback_only() {
        assert_eq!(
            validate_api_url("https://studio.reporch.com/ignored", false)
                .unwrap()
                .as_str(),
            "https://studio.reporch.com/api/v1/"
        );
        assert!(validate_api_url("http://studio.reporch.com", true).is_err());
        assert!(validate_api_url("http://127.0.0.1:8080", false).is_err());
        assert!(validate_api_url("http://127.0.0.1:8080", true).is_ok());
        assert!(validate_api_url("https://user:secret@studio.reporch.com", false).is_err());
    }

    #[test]
    fn signed_object_urls_are_azure_read_write_urls_or_local_test_urls() {
        assert!(
            validate_signed_object_url(
                "https://account.blob.core.windows.net/studio/cas/file?sig=secret",
                false
            )
            .is_ok()
        );
        assert!(validate_signed_object_url("https://evil.test/file?sig=secret", false).is_err());
        assert!(validate_signed_object_url("http://localhost:10000/file?sig=secret", true).is_ok());
    }

    #[test]
    fn operation_keys_are_bounded_and_reusable() {
        assert_eq!(
            operation_key("unused", Some("stable-operation-key")).unwrap(),
            "stable-operation-key"
        );
        assert!(operation_key("unused", Some("short")).is_err());
        assert!(operation_key("unused", Some("contains whitespace")).is_err());
    }

    #[test]
    fn capabilities_reject_incompatible_cli_versions() {
        let compatible = StudioCapabilitiesV1 {
            schema: "reporch.studio-capabilities.v1".into(),
            api_versions: vec!["v1".into()],
            authoring_spec_versions: vec![reporch_format::AUTHORING_SPEC_SCHEMA_V1.into()],
            release_manifest_versions: vec![studio_core::RELEASE_MANIFEST_SCHEMA_V1.into()],
            minimum_cli_version: env!("CARGO_PKG_VERSION").into(),
            maximum_cli_major: 1,
        };
        ensure_cli_compatible(&compatible).unwrap();

        let mut too_new = compatible.clone();
        too_new.minimum_cli_version = "99.0.0".into();
        assert!(ensure_cli_compatible(&too_new).is_err());

        assert_eq!(parse_numeric_version("1.2.3-beta.1").unwrap(), (1, 2, 3));
    }

    #[test]
    fn review_comments_are_trimmed_and_bounded() {
        assert_eq!(
            normalize_optional_review_comment(Some("  approved  ")).unwrap(),
            Some("approved".into())
        );
        assert_eq!(normalize_optional_review_comment(None).unwrap(), None);
        assert!(normalize_required_review_comment("   ").is_err());
        assert!(normalize_required_review_comment(&"x".repeat(4_001)).is_err());
    }

    #[test]
    fn review_scope_rejects_cross_project_and_cross_commit_responses() {
        let project_id = Uuid::now_v7();
        let commit_id = Uuid::now_v7();
        let review = ReviewResponse {
            id: Uuid::now_v7(),
            project_id,
            commit_id,
            validation_run_id: Uuid::now_v7(),
            manifest_digest: "a".repeat(64).parse().unwrap(),
            status: studio_core::ReviewStatusV1::InReview,
            submitted_by: studio_core::SubjectRef {
                issuer: "https://reporch.com/oauth".into(),
                subject: "author".into(),
            },
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            decision: None,
        };
        assert!(ensure_review_scope(&review, project_id, Some(commit_id)).is_ok());
        assert!(ensure_review_scope(&review, Uuid::now_v7(), Some(commit_id)).is_err());
        assert!(ensure_review_scope(&review, project_id, Some(Uuid::now_v7())).is_err());
    }

    #[test]
    fn unchanged_remote_head_makes_push_idempotent() {
        let commit_id = Uuid::now_v7();
        let other_commit_id = Uuid::now_v7();
        let digest: Sha256Digest = "a".repeat(64).parse().unwrap();
        let other_digest: Sha256Digest = "b".repeat(64).parse().unwrap();

        assert!(is_unchanged_remote_head(
            0, commit_id, &digest, commit_id, &digest,
        ));
        assert!(!is_unchanged_remote_head(
            1, commit_id, &digest, commit_id, &digest,
        ));
        assert!(!is_unchanged_remote_head(
            0,
            commit_id,
            &digest,
            other_commit_id,
            &digest,
        ));
        assert!(!is_unchanged_remote_head(
            0,
            commit_id,
            &digest,
            commit_id,
            &other_digest,
        ));
    }

    #[test]
    fn only_safe_transient_api_failures_are_retried() {
        for status in [
            StatusCode::REQUEST_TIMEOUT,
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::BAD_GATEWAY,
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::GATEWAY_TIMEOUT,
        ] {
            let error = anyhow::Error::new(StudioApiRequestError::Http { status });
            assert!(is_transient_api_error(&error), "{status} should retry");
        }

        for status in [
            StatusCode::BAD_REQUEST,
            StatusCode::UNAUTHORIZED,
            StatusCode::FORBIDDEN,
            StatusCode::NOT_FOUND,
            StatusCode::CONFLICT,
        ] {
            let error = anyhow::Error::new(StudioApiRequestError::Http { status });
            assert!(!is_transient_api_error(&error), "{status} must not retry");
        }
    }

    #[test]
    fn structured_retryable_errors_override_the_http_classification() {
        let error = anyhow::Error::new(StudioApiRequestError::Api {
            status: StatusCode::CONFLICT,
            error_code: "temporary".into(),
            message: "try again".into(),
            retryable: true,
            trace_id: "trace".into(),
        });
        assert!(is_transient_api_error(&error));
    }

    #[test]
    fn transient_poll_backoff_is_bounded() {
        assert_eq!(transient_poll_delay(1), Duration::from_millis(250));
        assert_eq!(transient_poll_delay(2), Duration::from_millis(500));
        assert_eq!(transient_poll_delay(8), Duration::from_secs(5));
        assert_eq!(transient_poll_delay(u32::MAX), Duration::from_secs(5));
    }

    #[test]
    fn sse_decoder_handles_chunk_boundaries_checkpoints_and_multiline_data() {
        let mut decoder = SseDecoder::default();
        assert!(
            decoder
                .push(b"id: 41\nevent: studio.progress")
                .unwrap()
                .is_empty()
        );
        let frames = decoder
            .push(b".validation.v1\ndata: {\"first\":1,\ndata: \"second\":2}\n\n")
            .unwrap();
        assert_eq!(
            frames,
            vec![SseFrame {
                id: Some("41".into()),
                event: Some("studio.progress.validation.v1".into()),
                data: Some("{\"first\":1,\n\"second\":2}".into()),
            }]
        );

        let checkpoint = decoder.push(b"id: 42\n: cursor-checkpoint\n\n").unwrap();
        assert_eq!(
            checkpoint,
            vec![SseFrame {
                id: Some("42".into()),
                event: None,
                data: None,
            }]
        );
    }

    #[test]
    fn sse_decoder_and_cursor_fail_closed_on_oversized_or_ambiguous_input() {
        let mut decoder = SseDecoder::default();
        assert!(decoder.push(&vec![b'x'; MAX_SSE_BUFFER_BYTES + 1]).is_err());
        for invalid in ["", "-1", "+1", " 1", "1x", "9223372036854775807"] {
            assert!(parse_event_cursor(invalid).is_err(), "accepted {invalid:?}");
        }
        assert_eq!(parse_event_cursor("42").unwrap(), 42);
    }

    #[tokio::test]
    async fn transient_poll_recovers_without_restarting_the_operation() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&attempts);
        let value = retry_transient_poll(
            tokio::time::Instant::now() + Duration::from_secs(3),
            "test status",
            move || {
                let observed = Arc::clone(&observed);
                async move {
                    let attempt = observed.fetch_add(1, Ordering::SeqCst);
                    if attempt < 2 {
                        Err(anyhow::Error::new(StudioApiRequestError::Http {
                            status: StatusCode::BAD_GATEWAY,
                        }))
                    } else {
                        Ok("ready")
                    }
                }
            },
        )
        .await
        .unwrap();

        assert_eq!(value, "ready");
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }
}
