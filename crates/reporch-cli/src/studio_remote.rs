use std::collections::HashMap;
use std::fs;
use std::future::Future;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use clap::{ArgAction, Args as ClapArgs, ValueEnum};
use futures_util::StreamExt;
use reqwest::header::{CONTENT_LENGTH, HeaderName, HeaderValue};
use reqwest::{RequestBuilder, Response, StatusCode, Url};
use serde::Serialize;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use studio_contracts::{
    ApiErrorResponse, BeginFileUploadRequest, BeginFileUploadResponse, CommitDetailResponse,
    CommitPage, CommitResponse, CreateCommitRequest, CreateProjectRequest, CreateReleaseRequest,
    CreateReviewDecisionRequest, EnqueueValidationRequest, FileDownloadResponse, FileEntryPage,
    FileUploadResponse, FileUploadStatus, FileUploadStrategyV1, ProjectPage, ProjectResponse,
    ReleaseDownloadResponse, ReleaseResponse, ReleaseStatus, ReviewPage, ReviewResponse,
    SubmitReviewRequest, ValidationRunDetailResponse, ValidationRunResponse, ValidationRunStatus,
};
use studio_core::{
    ManifestFile, ProblemType, ReleaseManifestV1, ReviewDecisionKindV1, Sha256Digest,
    validate_manifest,
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

#[derive(Debug, Error)]
enum StudioApiRequestError {
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
    #[arg(long, default_value = "reporch.problem.json")]
    pub manifest: PathBuf,
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
    pub project_id: Uuid,
    #[arg(long)]
    pub commit_id: Uuid,
    /// Reuse this value after an ambiguous network failure.
    #[arg(long)]
    pub idempotency_key: Option<String>,
    #[arg(long, default_value_t = true, action = ArgAction::Set)]
    pub wait: bool,
    #[arg(long, default_value_t = DEFAULT_WAIT_SECONDS)]
    pub timeout_seconds: u64,
}

#[derive(Debug, Clone, ClapArgs)]
pub struct PackageOptions {
    #[command(flatten)]
    pub connection: RemoteConnectionOptions,
    #[arg(long)]
    pub project_id: Uuid,
    #[arg(long)]
    pub commit_id: Uuid,
    #[arg(long)]
    pub validation_run_id: Uuid,
    #[arg(long)]
    pub output: PathBuf,
    /// Reuse this value after an ambiguous network failure.
    #[arg(long)]
    pub idempotency_key: Option<String>,
    #[arg(long, default_value_t = DEFAULT_WAIT_SECONDS)]
    pub timeout_seconds: u64,
}

#[derive(Debug, Clone, ClapArgs)]
pub struct SubmitReviewOptions {
    #[command(flatten)]
    pub connection: RemoteConnectionOptions,
    #[arg(long)]
    pub project_id: Uuid,
    #[arg(long)]
    pub commit_id: Uuid,
    #[arg(long)]
    pub validation_run_id: Uuid,
    /// Reuse this value after an ambiguous network failure.
    #[arg(long)]
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Clone, ClapArgs)]
pub struct ListReviewsOptions {
    #[command(flatten)]
    pub connection: RemoteConnectionOptions,
    #[arg(long)]
    pub project_id: Uuid,
}

#[derive(Debug, Clone, ClapArgs)]
pub struct ApproveReviewOptions {
    #[command(flatten)]
    pub connection: RemoteConnectionOptions,
    #[arg(long)]
    pub project_id: Uuid,
    #[arg(long)]
    pub review_id: Uuid,
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
    pub project_id: Uuid,
    #[arg(long)]
    pub review_id: Uuid,
    /// Explain the changes needed. Empty comments are rejected locally.
    #[arg(long)]
    pub comment: String,
    /// Reuse this value after an ambiguous network failure.
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
    let manifest = read_manifest(&options.manifest)?;
    let issues = validate_manifest(&manifest);
    if !issues.is_empty() {
        println!("{}", serde_json::to_string_pretty(&issues)?);
        bail!("manifest validation failed with {} issue(s)", issues.len());
    }
    let source_root = options
        .source_root
        .clone()
        .or_else(|| options.manifest.parent().map(Path::to_path_buf))
        .context("manifest path has no source root")?;
    let client = StudioApiClient::connect(&options.connection).await?;
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
        wait_for_upload(
            &client,
            manifest.project_id,
            status,
            options.timeout_seconds,
        )
        .await?;
        uploaded += 1;
    }
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
    let key = operation_key("cli-validation", options.idempotency_key.as_deref())?;
    eprintln!("Idempotency-Key: {key}");
    let client = StudioApiClient::connect(&options.connection).await?;
    let queued = client
        .enqueue_validation(
            options.project_id,
            &EnqueueValidationRequest {
                commit_id: options.commit_id,
            },
            &key,
        )
        .await?;
    ensure!(
        queued.project_id == options.project_id && queued.commit_id == options.commit_id,
        "Studio returned a mismatched validation run"
    );
    if !options.wait {
        return Ok(ValidationOperationResult {
            queued,
            detail: None,
        });
    }
    let detail = wait_for_validation(
        &client,
        options.project_id,
        queued.id,
        options.timeout_seconds,
    )
    .await?;
    Ok(ValidationOperationResult {
        queued,
        detail: Some(detail),
    })
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

pub async fn package_operation(options: &PackageOptions) -> Result<PackageOperationResult> {
    validate_wait_timeout(options.timeout_seconds)?;
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
            options.project_id,
            &CreateReleaseRequest {
                commit_id: options.commit_id,
                validation_run_id: options.validation_run_id,
            },
            &key,
        )
        .await?;
    let release = wait_for_release(
        &client,
        options.project_id,
        release,
        options.timeout_seconds,
    )
    .await?;
    ensure!(
        release.status == ReleaseStatus::Ready,
        "release build failed: {:?}",
        release.error_code
    );
    let download = client
        .release_download(options.project_id, release.id)
        .await?;
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
            &options.output,
        )
        .await?;
    Ok(PackageOperationResult {
        release,
        output: options.output.clone(),
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

pub async fn submit_review(options: &SubmitReviewOptions) -> Result<()> {
    let key = operation_key("cli-review-submit", options.idempotency_key.as_deref())?;
    eprintln!("Idempotency-Key: {key}");
    let client = StudioApiClient::connect(&options.connection).await?;
    let review = client
        .submit_review(
            options.project_id,
            &SubmitReviewRequest {
                commit_id: options.commit_id,
                validation_run_id: options.validation_run_id,
            },
            &key,
        )
        .await?;
    ensure_review_scope(&review, options.project_id, Some(options.commit_id))?;
    ensure!(
        review.validation_run_id == options.validation_run_id,
        "Studio returned a review for another validation"
    );
    println!("{}", serde_json::to_string_pretty(&review)?);
    Ok(())
}

pub async fn list_reviews(options: &ListReviewsOptions) -> Result<()> {
    let client = StudioApiClient::connect(&options.connection).await?;
    let mut items = Vec::new();
    let mut cursor = None;
    let mut seen_cursors = std::collections::HashSet::new();
    for _ in 0..100 {
        let page = client
            .list_reviews(options.project_id, cursor.as_deref())
            .await?;
        ensure!(
            page.items
                .iter()
                .all(|review| review.project_id == options.project_id),
            "Studio returned a review from another project"
        );
        items.extend(page.items);
        let Some(next_cursor) = page.next_cursor else {
            println!(
                "{}",
                serde_json::to_string_pretty(&ReviewPage {
                    items,
                    next_cursor: None,
                })?
            );
            return Ok(());
        };
        ensure!(
            seen_cursors.insert(next_cursor.clone()),
            "Studio returned a repeated review cursor"
        );
        cursor = Some(next_cursor);
    }
    bail!("Studio review listing exceeded the 10,000-item native client bound")
}

pub async fn approve_review(options: &ApproveReviewOptions) -> Result<()> {
    let comment = normalize_optional_review_comment(options.comment.as_deref())?;
    decide_review(
        &options.connection,
        options.project_id,
        options.review_id,
        ReviewDecisionKindV1::Approve,
        comment,
        options.idempotency_key.as_deref(),
    )
    .await
}

pub async fn request_review_changes(options: &RequestChangesOptions) -> Result<()> {
    let comment = normalize_required_review_comment(&options.comment)?;
    decide_review(
        &options.connection,
        options.project_id,
        options.review_id,
        ReviewDecisionKindV1::RequestChanges,
        Some(comment),
        options.idempotency_key.as_deref(),
    )
    .await
}

async fn decide_review(
    connection: &RemoteConnectionOptions,
    project_id: Uuid,
    review_id: Uuid,
    decision: ReviewDecisionKindV1,
    comment: Option<String>,
    provided_key: Option<&str>,
) -> Result<()> {
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
    println!("{}", serde_json::to_string_pretty(&review)?);
    Ok(())
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
