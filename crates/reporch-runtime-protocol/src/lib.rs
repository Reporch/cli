#![forbid(unsafe_code)]

use std::collections::{BTreeMap, HashMap, HashSet};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_bytes::ByteBuf;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use unicode_normalization::UnicodeNormalization as _;
use uuid::Uuid;

pub const PROTOCOL_VERSION: u16 = 2;
pub const HANDSHAKE_SCHEMA: &str = "reporch.guest-handshake.v1";
pub const HOST_CHALLENGE_SCHEMA: &str = "reporch.host-challenge.v1";
pub const JOB_SCHEMA: &str = "reporch.guest-job.v1";
pub const RESULT_SCHEMA: &str = "reporch.guest-result.v2";
pub const SERVICE_REQUEST_SCHEMA: &str = "reporch.runtime-service-request.v1";
pub const SERVICE_RESPONSE_SCHEMA: &str = "reporch.runtime-service-response.v1";
pub const MAX_WIRE_FRAME_BYTES: usize = 12 * 1024 * 1024;
pub const MAX_CONTENT_CHUNK_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_INPUT_OBJECTS: usize = 10_000;
pub const MAX_INPUT_BYTES: u64 = 1_073_741_824;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum WireMessageV1 {
    HostChallenge(HostChallengeV1),
    Handshake(GuestHandshakeV1),
    Job(GuestJobV1),
    InputChunk(InputChunkV1),
    Result(GuestResultV1),
    ProtocolError(ProtocolFailureV1),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostChallengeV1 {
    pub schema: String,
    pub protocol_version: u16,
    pub nonce: String,
    pub runtime_bundle_digest: String,
}

impl HostChallengeV1 {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.schema != HOST_CHALLENGE_SCHEMA {
            return Err(ProtocolError::UnsupportedSchema);
        }
        if self.protocol_version != PROTOCOL_VERSION {
            return Err(ProtocolError::IncompatibleVersion);
        }
        validate_nonce(&self.nonce)?;
        validate_digest(&self.runtime_bundle_digest)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeServiceRequestV1 {
    pub schema: String,
    pub protocol_version: u16,
    pub id: Uuid,
    pub command: RuntimeServiceCommandV1,
}

impl RuntimeServiceRequestV1 {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.schema != SERVICE_REQUEST_SCHEMA {
            return Err(ProtocolError::UnsupportedSchema);
        }
        if self.protocol_version != PROTOCOL_VERSION {
            return Err(ProtocolError::IncompatibleVersion);
        }
        match &self.command {
            RuntimeServiceCommandV1::Ping => {}
            RuntimeServiceCommandV1::UpdateRuntime { .. } => {}
            RuntimeServiceCommandV1::InstallToolchain { id } => validate_toolchain_id(id)?,
            RuntimeServiceCommandV1::ValidateSpool { objects } => validate_objects(objects)?,
            RuntimeServiceCommandV1::RunJob {
                job,
                runtime_sequence,
                runtime_bundle_digest,
            } => {
                job.validate()?;
                if *runtime_sequence == 0 {
                    return Err(ProtocolError::InvalidField("runtime_sequence"));
                }
                validate_digest(runtime_bundle_digest)?;
            }
        }
        Ok(())
    }
}

fn validate_objects(objects: &[ContentObjectV1]) -> Result<(), ProtocolError> {
    if objects.len() > MAX_INPUT_OBJECTS {
        return Err(ProtocolError::InvalidField("objects"));
    }
    let mut total = 0_u64;
    let mut paths = HashSet::new();
    let mut digest_sizes = HashMap::new();
    for object in objects {
        validate_digest(&object.sha256)?;
        validate_relative_path(&object.path)?;
        if !paths.insert(portable_path_key(&object.path)?) {
            return Err(ProtocolError::InvalidField("objects"));
        }
        if digest_sizes
            .insert(object.sha256.as_str(), object.size)
            .is_some_and(|size| size != object.size)
        {
            return Err(ProtocolError::InvalidField("objects"));
        }
        total = total
            .checked_add(object.size)
            .ok_or(ProtocolError::InvalidField("objects"))?;
    }
    if total > MAX_INPUT_BYTES {
        return Err(ProtocolError::InvalidField("objects"));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "command", content = "payload", rename_all = "snake_case")]
pub enum RuntimeServiceCommandV1 {
    Ping,
    UpdateRuntime {
        force: bool,
    },
    InstallToolchain {
        id: String,
    },
    ValidateSpool {
        objects: Vec<ContentObjectV1>,
    },
    RunJob {
        job: Box<GuestJobV1>,
        runtime_sequence: u64,
        runtime_bundle_digest: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeServiceResponseV1 {
    pub schema: String,
    pub protocol_version: u16,
    pub request_id: Uuid,
    pub result: RuntimeServiceResultV1,
}

impl RuntimeServiceResponseV1 {
    pub fn validate_for(&self, request: &RuntimeServiceRequestV1) -> Result<(), ProtocolError> {
        if self.schema != SERVICE_RESPONSE_SCHEMA {
            return Err(ProtocolError::UnsupportedSchema);
        }
        if self.protocol_version != PROTOCOL_VERSION {
            return Err(ProtocolError::IncompatibleVersion);
        }
        if self.request_id != request.id {
            return Err(ProtocolError::JobMismatch);
        }
        match (&self.result, &request.command) {
            (RuntimeServiceResultV1::Pong { .. }, RuntimeServiceCommandV1::Ping)
            | (
                RuntimeServiceResultV1::SpoolValid { .. },
                RuntimeServiceCommandV1::ValidateSpool { .. },
            )
            | (RuntimeServiceResultV1::Error(_), _) => Ok(()),
            (
                RuntimeServiceResultV1::RuntimeUpdated {
                    installed_version,
                    sequence,
                    target,
                    ..
                },
                RuntimeServiceCommandV1::UpdateRuntime { .. },
            ) => {
                if *sequence == 0
                    || installed_version.is_empty()
                    || installed_version.len() > 128
                    || !installed_version.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_')
                    })
                    || !matches!(
                        target.as_str(),
                        "darwin-arm64"
                            | "darwin-x64"
                            | "linux-arm64-gnu"
                            | "linux-x64-gnu"
                            | "windows-x64-msvc"
                    )
                {
                    return Err(ProtocolError::InvalidField("runtime_update"));
                }
                Ok(())
            }
            (
                RuntimeServiceResultV1::ToolchainInstalled {
                    id,
                    index_sequence,
                    bundle_sha256,
                },
                RuntimeServiceCommandV1::InstallToolchain { id: requested },
            ) => {
                validate_toolchain_id(id)?;
                validate_digest(bundle_sha256)?;
                if *index_sequence == 0 || id != requested {
                    return Err(ProtocolError::JobMismatch);
                }
                Ok(())
            }
            (
                RuntimeServiceResultV1::JobCompleted { result },
                RuntimeServiceCommandV1::RunJob { job, .. },
            ) => result.validate_for(job),
            _ => Err(ProtocolError::JobMismatch),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", content = "payload", rename_all = "snake_case")]
pub enum RuntimeServiceResultV1 {
    Pong {
        service_version: String,
    },
    RuntimeUpdated {
        previous_version: Option<String>,
        installed_version: String,
        sequence: u64,
        target: String,
        repaired: bool,
    },
    ToolchainInstalled {
        id: String,
        index_sequence: u64,
        bundle_sha256: String,
    },
    SpoolValid {
        object_count: u32,
        total_bytes: u64,
    },
    JobCompleted {
        result: Box<GuestResultV1>,
    },
    Error(ProtocolFailureV1),
}

fn validate_toolchain_id(value: &str) -> Result<(), ProtocolError> {
    if value.is_empty()
        || value.len() > 64
        || !value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b'_')
        })
    {
        return Err(ProtocolError::InvalidField("toolchain_id"));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputChunkV1 {
    pub object_index: u32,
    pub offset: u64,
    #[serde(with = "serde_bytes")]
    pub bytes: ByteBuf,
    pub eof: bool,
}

impl InputChunkV1 {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.bytes.len() > MAX_CONTENT_CHUNK_BYTES || (self.bytes.is_empty() && !self.eof) {
            return Err(ProtocolError::InvalidField("input_chunk"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtocolFailureV1 {
    pub error_code: String,
    pub message: String,
}

impl ProtocolFailureV1 {
    pub fn bounded(error_code: impl Into<String>, message: impl Into<String>) -> Self {
        let mut error_code = error_code.into();
        let mut message = message.into();
        error_code.truncate(128);
        message.truncate(1_024);
        Self {
            error_code,
            message,
        }
    }
}

pub async fn write_wire_message(
    writer: &mut (impl AsyncWrite + Unpin),
    message: &WireMessageV1,
) -> Result<(), WireError> {
    write_frame(writer, message).await
}

pub fn write_wire_message_sync(
    writer: &mut impl std::io::Write,
    message: &WireMessageV1,
) -> Result<(), WireError> {
    write_frame_sync(writer, message)
}

pub async fn write_service_request(
    writer: &mut (impl AsyncWrite + Unpin),
    request: &RuntimeServiceRequestV1,
) -> Result<(), WireError> {
    write_frame(writer, request).await
}

pub async fn read_service_request(
    reader: &mut (impl AsyncRead + Unpin),
) -> Result<RuntimeServiceRequestV1, WireError> {
    read_frame(reader).await
}

pub async fn write_service_response(
    writer: &mut (impl AsyncWrite + Unpin),
    response: &RuntimeServiceResponseV1,
) -> Result<(), WireError> {
    write_frame(writer, response).await
}

pub async fn read_service_response(
    reader: &mut (impl AsyncRead + Unpin),
) -> Result<RuntimeServiceResponseV1, WireError> {
    read_frame(reader).await
}

async fn write_frame<T: Serialize>(
    writer: &mut (impl AsyncWrite + Unpin),
    value: &T,
) -> Result<(), WireError> {
    let bytes = rmp_serde::to_vec_named(value)?;
    if !(1..=MAX_WIRE_FRAME_BYTES).contains(&bytes.len()) {
        return Err(WireError::InvalidFrameSize);
    }
    writer
        .write_all(&(bytes.len() as u32).to_be_bytes())
        .await?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

pub async fn read_wire_message(
    reader: &mut (impl AsyncRead + Unpin),
) -> Result<WireMessageV1, WireError> {
    read_frame(reader).await
}

pub fn read_wire_message_sync(reader: &mut impl std::io::Read) -> Result<WireMessageV1, WireError> {
    read_frame_sync(reader)
}

async fn read_frame<T: serde::de::DeserializeOwned>(
    reader: &mut (impl AsyncRead + Unpin),
) -> Result<T, WireError> {
    let mut length = [0_u8; 4];
    reader.read_exact(&mut length).await?;
    let length = u32::from_be_bytes(length) as usize;
    if !(1..=MAX_WIRE_FRAME_BYTES).contains(&length) {
        return Err(WireError::InvalidFrameSize);
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).await?;
    Ok(rmp_serde::from_slice(&bytes)?)
}

fn write_frame_sync<T: Serialize>(
    writer: &mut impl std::io::Write,
    value: &T,
) -> Result<(), WireError> {
    let bytes = rmp_serde::to_vec_named(value)?;
    if !(1..=MAX_WIRE_FRAME_BYTES).contains(&bytes.len()) {
        return Err(WireError::InvalidFrameSize);
    }
    writer.write_all(&(bytes.len() as u32).to_be_bytes())?;
    writer.write_all(&bytes)?;
    writer.flush()?;
    Ok(())
}

fn read_frame_sync<T: serde::de::DeserializeOwned>(
    reader: &mut impl std::io::Read,
) -> Result<T, WireError> {
    let mut length = [0_u8; 4];
    reader.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if !(1..=MAX_WIRE_FRAME_BYTES).contains(&length) {
        return Err(WireError::InvalidFrameSize);
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes)?;
    Ok(rmp_serde::from_slice(&bytes)?)
}

#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error("runtime transport I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("runtime transport encoding failed: {0}")]
    Encode(#[from] rmp_serde::encode::Error),
    #[error("runtime transport decoding failed: {0}")]
    Decode(#[from] rmp_serde::decode::Error),
    #[error("runtime transport frame has an invalid size")]
    InvalidFrameSize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestHandshakeV1 {
    pub schema: String,
    pub protocol_version: u16,
    pub guest_version: String,
    pub runtime_bundle_digest: String,
    pub nonce: String,
}

impl GuestHandshakeV1 {
    pub fn validate(
        &self,
        expected_nonce: &str,
        expected_bundle_digest: &str,
    ) -> Result<(), ProtocolError> {
        if self.schema != HANDSHAKE_SCHEMA {
            return Err(ProtocolError::UnsupportedSchema);
        }
        if self.protocol_version != PROTOCOL_VERSION {
            return Err(ProtocolError::IncompatibleVersion);
        }
        validate_nonce(&self.nonce)?;
        validate_digest(&self.runtime_bundle_digest)?;
        if self.nonce != expected_nonce || self.runtime_bundle_digest != expected_bundle_digest {
            return Err(ProtocolError::HandshakeMismatch);
        }
        if self.guest_version.is_empty() || self.guest_version.len() > 128 {
            return Err(ProtocolError::InvalidField("guest_version"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuestOperationV1 {
    Program,
    Generator,
    ValidatorUnit,
    CheckerUnit,
    SolutionMatrix,
    Interactive,
    Grader,
    FullVerify,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceLimitsV1 {
    pub timeout_ms: u64,
    pub memory_mib: u64,
    pub cpu_millis: u32,
    pub pids: u32,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
    pub artifact_bytes: u64,
}

impl ResourceLimitsV1 {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if !(1_000..=600_000).contains(&self.timeout_ms) {
            return Err(ProtocolError::InvalidField("timeout_ms"));
        }
        if !(16..=8_192).contains(&self.memory_mib) {
            return Err(ProtocolError::InvalidField("memory_mib"));
        }
        if !(100..=16_000).contains(&self.cpu_millis) {
            return Err(ProtocolError::InvalidField("cpu_millis"));
        }
        if !(1..=4_096).contains(&self.pids) {
            return Err(ProtocolError::InvalidField("pids"));
        }
        for (field, value) in [
            ("stdout_bytes", self.stdout_bytes),
            ("stderr_bytes", self.stderr_bytes),
        ] {
            if !(1..=4 * 1_048_576).contains(&value) {
                return Err(ProtocolError::InvalidField(field));
            }
        }
        if !(1..=1_073_741_824).contains(&self.artifact_bytes) {
            return Err(ProtocolError::InvalidField("artifact_bytes"));
        }
        let retained_output = self
            .stdout_bytes
            .checked_add(self.stderr_bytes)
            .ok_or(ProtocolError::InvalidField("output_bytes"))?;
        if retained_output > self.memory_mib * 1_048_576 / 2 {
            return Err(ProtocolError::InvalidField("output_bytes"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContentObjectV1 {
    pub path: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestJobV1 {
    pub schema: String,
    pub protocol_version: u16,
    pub id: Uuid,
    pub nonce: String,
    pub operation: GuestOperationV1,
    pub toolchain_id: String,
    pub toolchain_index_sequence: Option<u64>,
    pub toolchain_bundle_sha256: Option<String>,
    pub toolchain_lock_sha256: Option<String>,
    pub command: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub inputs: Vec<ContentObjectV1>,
    pub limits: ResourceLimitsV1,
}

impl GuestJobV1 {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.schema != JOB_SCHEMA {
            return Err(ProtocolError::UnsupportedSchema);
        }
        if self.protocol_version != PROTOCOL_VERSION {
            return Err(ProtocolError::IncompatibleVersion);
        }
        validate_nonce(&self.nonce)?;
        if self.toolchain_id.is_empty() || self.toolchain_id.len() > 128 {
            return Err(ProtocolError::InvalidField("toolchain_id"));
        }
        let runtime_self_test = self.toolchain_id == "runtime-self-test";
        match (
            self.toolchain_index_sequence,
            self.toolchain_bundle_sha256.as_deref(),
            self.toolchain_lock_sha256.as_deref(),
        ) {
            (None, None, None) if runtime_self_test => {}
            (Some(sequence), Some(bundle), Some(toolchain_lock))
                if !runtime_self_test && sequence > 0 =>
            {
                validate_digest(bundle)?;
                validate_digest(toolchain_lock)?;
            }
            _ => return Err(ProtocolError::InvalidField("toolchain_identity")),
        }
        if self.command.is_empty() || self.command.len() > 256 {
            return Err(ProtocolError::InvalidField("command"));
        }
        if self
            .command
            .iter()
            .any(|value| value.is_empty() || value.len() > 4_096 || value.contains('\0'))
        {
            return Err(ProtocolError::InvalidField("command"));
        }
        if self.environment.len() > 64
            || self.environment.iter().any(|(key, value)| {
                !valid_environment_key(key)
                    || value.len() > 4_096
                    || value.contains('\0')
                    || looks_secret(key)
            })
        {
            return Err(ProtocolError::InvalidField("environment"));
        }
        if self.inputs.len() > MAX_INPUT_OBJECTS {
            return Err(ProtocolError::InvalidField("inputs"));
        }
        let mut total = 0_u64;
        let mut paths = HashSet::new();
        let mut digest_sizes = HashMap::new();
        for input in &self.inputs {
            validate_relative_path(&input.path)?;
            validate_digest(&input.sha256)?;
            if !paths.insert(portable_path_key(&input.path)?) {
                return Err(ProtocolError::InvalidField("inputs"));
            }
            if digest_sizes
                .insert(input.sha256.as_str(), input.size)
                .is_some_and(|size| size != input.size)
            {
                return Err(ProtocolError::InvalidField("inputs"));
            }
            total = total
                .checked_add(input.size)
                .ok_or(ProtocolError::InvalidField("inputs"))?;
        }
        let memory_bound = self
            .limits
            .memory_mib
            .checked_mul(1_048_576)
            .and_then(|bytes| bytes.checked_div(4))
            .ok_or(ProtocolError::InvalidField("inputs"))?;
        if total > MAX_INPUT_BYTES.min(memory_bound) {
            return Err(ProtocolError::InvalidField("inputs"));
        }
        self.limits.validate()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestResultV1 {
    pub schema: String,
    pub protocol_version: u16,
    pub job_id: Uuid,
    pub nonce: String,
    pub exit_code: i32,
    pub termination: GuestTerminationV2,
    pub duration_ms: u64,
    pub stdout: GuestOutputV1,
    pub stderr: GuestOutputV1,
    pub artifacts: Vec<ContentObjectV1>,
}

/// Stable workload termination classification. A workload timeout is a normal
/// judge result, never a transport/protocol failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuestTerminationV2 {
    Exited,
    TimedOut,
    Signalled,
    OutputLimit,
    InternalError,
}

pub type GuestResultV2 = GuestResultV1;

impl GuestResultV1 {
    pub fn validate_for(&self, job: &GuestJobV1) -> Result<(), ProtocolError> {
        if self.schema != RESULT_SCHEMA {
            return Err(ProtocolError::UnsupportedSchema);
        }
        if self.protocol_version != PROTOCOL_VERSION {
            return Err(ProtocolError::IncompatibleVersion);
        }
        if self.job_id != job.id || self.nonce != job.nonce {
            return Err(ProtocolError::JobMismatch);
        }
        self.stdout.validate(job.limits.stdout_bytes)?;
        self.stderr.validate(job.limits.stderr_bytes)?;
        let mut total = 0_u64;
        let mut paths = HashSet::new();
        for artifact in &self.artifacts {
            validate_relative_path(&artifact.path)?;
            validate_digest(&artifact.sha256)?;
            if !paths.insert(portable_path_key(&artifact.path)?) {
                return Err(ProtocolError::InvalidField("artifacts"));
            }
            total = total
                .checked_add(artifact.size)
                .ok_or(ProtocolError::InvalidField("artifacts"))?;
        }
        if total > job.limits.artifact_bytes {
            return Err(ProtocolError::OutputLimitExceeded);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuestOutputEncodingV1 {
    Utf8,
    Base64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestOutputV1 {
    pub encoding: GuestOutputEncodingV1,
    pub data: String,
    pub original_bytes: u64,
    pub truncated: bool,
}

impl GuestOutputV1 {
    pub fn from_bytes(bytes: &[u8], truncated: bool) -> Self {
        match std::str::from_utf8(bytes) {
            Ok(value) => Self {
                encoding: GuestOutputEncodingV1::Utf8,
                data: value.into(),
                original_bytes: bytes.len() as u64,
                truncated,
            },
            Err(_) => Self {
                encoding: GuestOutputEncodingV1::Base64,
                data: base64::engine::general_purpose::STANDARD.encode(bytes),
                original_bytes: bytes.len() as u64,
                truncated,
            },
        }
    }

    pub fn validate(&self, limit: u64) -> Result<(), ProtocolError> {
        if self.original_bytes > limit {
            return Err(ProtocolError::OutputLimitExceeded);
        }
        let actual = match self.encoding {
            GuestOutputEncodingV1::Utf8 => self.data.len(),
            GuestOutputEncodingV1::Base64 => base64::engine::general_purpose::STANDARD
                .decode(&self.data)
                .map_err(|_| ProtocolError::InvalidField("output"))?
                .len(),
        };
        if actual as u64 != self.original_bytes {
            return Err(ProtocolError::InvalidField("output"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ProtocolError {
    #[error("unsupported runtime protocol schema")]
    UnsupportedSchema,
    #[error("runtime protocol version is incompatible")]
    IncompatibleVersion,
    #[error("runtime handshake does not match this session")]
    HandshakeMismatch,
    #[error("runtime result does not match the submitted job")]
    JobMismatch,
    #[error("runtime result exceeds its output limit")]
    OutputLimitExceeded,
    #[error("invalid runtime protocol field: {0}")]
    InvalidField(&'static str),
}

fn validate_nonce(value: &str) -> Result<(), ProtocolError> {
    if !(16..=128).contains(&value.len())
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(ProtocolError::InvalidField("nonce"));
    }
    Ok(())
}

fn validate_digest(value: &str) -> Result<(), ProtocolError> {
    let Some(hex) = value.strip_prefix("sha256:") else {
        return Err(ProtocolError::InvalidField("sha256"));
    };
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(ProtocolError::InvalidField("sha256"));
    }
    Ok(())
}

fn validate_relative_path(value: &str) -> Result<(), ProtocolError> {
    if value.is_empty()
        || value.len() > 4_096
        || value.starts_with('/')
        || value.contains('\0')
        || value.contains('\\')
        || value.contains(':')
        || value.chars().any(char::is_control)
        || value.nfc().ne(value.chars())
    {
        return Err(ProtocolError::InvalidField("path"));
    }
    if value.split('/').any(|part| {
        part.is_empty()
            || matches!(part, "." | "..")
            || part.ends_with([' ', '.'])
            || is_windows_reserved_name(part)
    }) {
        return Err(ProtocolError::InvalidField("path"));
    }
    Ok(())
}

fn portable_path_key(value: &str) -> Result<String, ProtocolError> {
    validate_relative_path(value)?;
    Ok(value.to_lowercase())
}

fn is_windows_reserved_name(part: &str) -> bool {
    let base = part
        .split_once('.')
        .map_or(part, |(base, _)| base)
        .to_ascii_uppercase();
    matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || base
            .strip_prefix("COM")
            .or_else(|| base.strip_prefix("LPT"))
            .is_some_and(|suffix| suffix.len() == 1 && matches!(suffix.as_bytes()[0], b'1'..=b'9'))
}

fn valid_environment_key(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
}

fn looks_secret(key: &str) -> bool {
    [
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PASS",
        "KEY",
        "CREDENTIAL",
        "COOKIE",
        "AUTHORIZATION",
    ]
    .iter()
    .any(|marker| key.contains(marker))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job() -> GuestJobV1 {
        GuestJobV1 {
            schema: JOB_SCHEMA.into(),
            protocol_version: PROTOCOL_VERSION,
            id: Uuid::now_v7(),
            nonce: "nonce-0123456789abcdef".into(),
            operation: GuestOperationV1::Program,
            toolchain_id: "python-3.14".into(),
            toolchain_index_sequence: Some(1),
            toolchain_bundle_sha256: Some(format!("sha256:{}", "b".repeat(64))),
            toolchain_lock_sha256: Some(format!("sha256:{}", "c".repeat(64))),
            command: vec!["python3".into(), "solution.py".into()],
            environment: BTreeMap::from([("LANG".into(), "C.UTF-8".into())]),
            inputs: vec![ContentObjectV1 {
                path: "solution.py".into(),
                sha256: format!("sha256:{}", "a".repeat(64)),
                size: 10,
            }],
            limits: ResourceLimitsV1 {
                timeout_ms: 10_000,
                memory_mib: 256,
                cpu_millis: 1_000,
                pids: 64,
                stdout_bytes: 1_024,
                stderr_bytes: 1_024,
                artifact_bytes: 1_024,
            },
        }
    }

    #[test]
    fn valid_job_is_accepted() {
        job().validate().unwrap();
    }

    #[test]
    fn traversal_and_secret_environment_are_rejected() {
        let mut traversal = job();
        traversal.inputs[0].path = "../secret".into();
        assert!(traversal.validate().is_err());

        let mut secret = job();
        secret
            .environment
            .insert("ACCESS_TOKEN".into(), "value".into());
        assert!(secret.validate().is_err());

        let mut duplicate = job();
        duplicate.inputs.push(duplicate.inputs[0].clone());
        assert!(duplicate.validate().is_err());

        let mut alternate_data_stream = job();
        alternate_data_stream.inputs[0].path = "src/main.rs:secret".into();
        assert!(alternate_data_stream.validate().is_err());

        let mut reserved = job();
        reserved.inputs[0].path = "tests/CON.txt".into();
        assert!(reserved.validate().is_err());

        let mut case_collision = job();
        case_collision.inputs[0].path = "src/Main.rs".into();
        let mut second = case_collision.inputs[0].clone();
        second.path = "src/main.rs".into();
        case_collision.inputs.push(second);
        assert!(case_collision.validate().is_err());

        let mut non_nfc = job();
        non_nfc.inputs[0].path = "statements/e\u{301}.md".into();
        assert!(non_nfc.validate().is_err());
    }

    #[test]
    fn input_staging_is_memory_bounded_and_digest_sizes_are_consistent() {
        let mut oversized = job();
        oversized.inputs[0].size = oversized.limits.memory_mib * 1_048_576 / 4 + 1;
        assert_eq!(
            oversized.validate(),
            Err(ProtocolError::InvalidField("inputs"))
        );

        let mut inconsistent = job();
        let mut alias = inconsistent.inputs[0].clone();
        alias.path = "alias.py".into();
        alias.size += 1;
        inconsistent.inputs.push(alias);
        assert_eq!(
            inconsistent.validate(),
            Err(ProtocolError::InvalidField("inputs"))
        );
    }

    #[test]
    fn result_is_bound_to_job_and_limits() {
        let job = job();
        let mut result = GuestResultV1 {
            schema: RESULT_SCHEMA.into(),
            protocol_version: PROTOCOL_VERSION,
            job_id: job.id,
            nonce: job.nonce.clone(),
            exit_code: 0,
            termination: GuestTerminationV2::Exited,
            duration_ms: 1,
            stdout: GuestOutputV1::from_bytes(b"ok", false),
            stderr: GuestOutputV1::from_bytes(b"", false),
            artifacts: vec![],
        };
        result.validate_for(&job).unwrap();
        result.job_id = Uuid::now_v7();
        assert_eq!(result.validate_for(&job), Err(ProtocolError::JobMismatch));
    }

    #[test]
    fn binary_output_round_trips_without_lossy_expansion() {
        let output = GuestOutputV1::from_bytes(&[0xff, 0x00, 0xfe], false);
        assert_eq!(output.encoding, GuestOutputEncodingV1::Base64);
        output.validate(3).unwrap();
        assert_eq!(output.validate(2), Err(ProtocolError::OutputLimitExceeded));
    }

    #[test]
    fn synchronous_hyperv_transport_uses_the_same_bounded_frames() {
        let message = WireMessageV1::Job(job());
        let mut bytes = Vec::new();
        write_wire_message_sync(&mut bytes, &message).unwrap();
        assert!(bytes.len() <= MAX_WIRE_FRAME_BYTES + 4);
        let decoded = read_wire_message_sync(&mut bytes.as_slice()).unwrap();
        assert_eq!(decoded, message);

        let mut invalid = [0xff, 0xff, 0xff, 0xff].as_slice();
        assert!(matches!(
            read_wire_message_sync(&mut invalid),
            Err(WireError::InvalidFrameSize)
        ));
    }

    #[tokio::test]
    async fn wire_transport_preserves_binary_chunks_and_rejects_oversize() {
        let message = WireMessageV1::InputChunk(InputChunkV1 {
            object_index: 2,
            offset: 7,
            bytes: ByteBuf::from(vec![0, 255, 1, 254]),
            eof: false,
        });
        let mut wire = Vec::new();
        write_wire_message(&mut wire, &message).await.unwrap();
        let decoded = read_wire_message(&mut wire.as_slice()).await.unwrap();
        assert_eq!(decoded, message);

        let oversize = WireMessageV1::InputChunk(InputChunkV1 {
            object_index: 0,
            offset: 0,
            bytes: ByteBuf::from(vec![0; MAX_WIRE_FRAME_BYTES]),
            eof: false,
        });
        assert!(
            write_wire_message(&mut Vec::new(), &oversize)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn runtime_service_frames_are_request_bound() {
        let request = RuntimeServiceRequestV1 {
            schema: SERVICE_REQUEST_SCHEMA.into(),
            protocol_version: PROTOCOL_VERSION,
            id: Uuid::now_v7(),
            command: RuntimeServiceCommandV1::Ping,
        };
        request.validate().unwrap();
        let mut wire = Vec::new();
        write_service_request(&mut wire, &request).await.unwrap();
        let decoded = read_service_request(&mut wire.as_slice()).await.unwrap();
        assert_eq!(decoded, request);
        let response = RuntimeServiceResponseV1 {
            schema: SERVICE_RESPONSE_SCHEMA.into(),
            protocol_version: PROTOCOL_VERSION,
            request_id: request.id,
            result: RuntimeServiceResultV1::Pong {
                service_version: "1.0.0-rc.8".into(),
            },
        };
        response.validate_for(&request).unwrap();

        let install = RuntimeServiceRequestV1 {
            schema: SERVICE_REQUEST_SCHEMA.into(),
            protocol_version: PROTOCOL_VERSION,
            id: Uuid::now_v7(),
            command: RuntimeServiceCommandV1::InstallToolchain {
                id: "python-3.14".into(),
            },
        };
        install.validate().unwrap();
        let installed = RuntimeServiceResponseV1 {
            schema: SERVICE_RESPONSE_SCHEMA.into(),
            protocol_version: PROTOCOL_VERSION,
            request_id: install.id,
            result: RuntimeServiceResultV1::ToolchainInstalled {
                id: "python-3.14".into(),
                index_sequence: 1,
                bundle_sha256: format!("sha256:{}", "a".repeat(64)),
            },
        };
        installed.validate_for(&install).unwrap();
        assert_eq!(
            installed.validate_for(&request),
            Err(ProtocolError::JobMismatch)
        );

        let mut invalid = install;
        invalid.command = RuntimeServiceCommandV1::InstallToolchain {
            id: "../host".into(),
        };
        assert!(invalid.validate().is_err());
    }
}
