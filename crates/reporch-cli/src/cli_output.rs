use std::io::IsTerminal as _;

use anyhow::Error;
use clap::ValueEnum;
use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    Human,
    Json,
    Jsonl,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ColorMode {
    Auto,
    Always,
    Never,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum ExitCode {
    DomainFailure = 1,
    InvalidInput = 2,
    Conflict = 3,
    AuthenticationRequired = 4,
    PermissionDenied = 5,
    InfrastructureFailure = 6,
    Cancelled = 7,
}

#[derive(Debug, Serialize)]
struct SuccessEnvelope<'a, T> {
    schema: &'static str,
    command: &'a str,
    data: &'a T,
}

#[derive(Debug, Serialize)]
struct ErrorEnvelope<'a> {
    schema: &'static str,
    command: &'a str,
    error_code: String,
    message: String,
    retryable: bool,
    trace_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<&'a Value>,
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct DetailedCliError {
    message: String,
    details: Value,
}

pub fn detailed_error(message: impl Into<String>, details: impl Serialize) -> Error {
    let details = serde_json::to_value(details).unwrap_or_else(|serialization_error| {
        serde_json::json!({
            "schema": "reporch.error-details-serialization.v1",
            "message": serialization_error.to_string(),
        })
    });
    Error::new(DetailedCliError {
        message: message.into(),
        details,
    })
}

#[derive(Debug, Clone)]
pub struct CliOutput {
    format: OutputFormat,
    quiet: bool,
    color: ColorMode,
}

impl CliOutput {
    pub fn new(format: OutputFormat, quiet: bool, color: ColorMode) -> Self {
        Self {
            format,
            quiet,
            color,
        }
    }

    pub fn emit<T: Serialize>(&self, command: &str, data: &T, human: &str) -> anyhow::Result<()> {
        match self.format {
            OutputFormat::Human => {
                if !self.quiet {
                    println!("{human}");
                }
            }
            OutputFormat::Json => println!(
                "{}",
                serde_json::to_string_pretty(&SuccessEnvelope {
                    schema: "reporch.cli-result.v1",
                    command,
                    data,
                })?
            ),
            OutputFormat::Jsonl => println!(
                "{}",
                serde_json::to_string(&SuccessEnvelope {
                    schema: "reporch.cli-result.v1",
                    command,
                    data,
                })?
            ),
        }
        Ok(())
    }

    pub fn emit_error(&self, command: &str, error: &Error) -> ExitCode {
        let classified = classify_error(error);
        let envelope = ErrorEnvelope {
            schema: "reporch.cli-error.v1",
            command,
            error_code: classified.error_code,
            message: format!("{error:#}"),
            retryable: classified.retryable,
            trace_id: classified
                .trace_id
                .unwrap_or_else(|| Uuid::now_v7().to_string()),
            details: error.chain().find_map(|cause| {
                cause
                    .downcast_ref::<DetailedCliError>()
                    .map(|error| &error.details)
            }),
        };
        match self.format {
            OutputFormat::Human => eprintln!("{}: {}", envelope.error_code, envelope.message),
            OutputFormat::Json => eprintln!(
                "{}",
                serde_json::to_string_pretty(&envelope)
                    .unwrap_or_else(|_| "{\"schema\":\"reporch.cli-error.v1\"}".into())
            ),
            OutputFormat::Jsonl => eprintln!(
                "{}",
                serde_json::to_string(&envelope)
                    .unwrap_or_else(|_| "{\"schema\":\"reporch.cli-error.v1\"}".into())
            ),
        }
        classified.exit_code
    }

    pub fn colors_enabled(&self) -> bool {
        match self.color {
            ColorMode::Always => true,
            ColorMode::Never => false,
            ColorMode::Auto => std::io::stdout().is_terminal(),
        }
    }

    pub fn ensure_streaming_format(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.format != OutputFormat::Json,
            "unbounded streaming requires --format jsonl or human output; use --max-events with --format json"
        );
        Ok(())
    }

    pub fn ensure_human_format(&self, command: &str) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.format == OutputFormat::Human,
            "{command} writes a raw shell script and requires --format human"
        );
        Ok(())
    }
}

struct ClassifiedError {
    exit_code: ExitCode,
    error_code: String,
    retryable: bool,
    trace_id: Option<String>,
}

fn classify_error(error: &Error) -> ClassifiedError {
    if let Some(remote) = crate::studio_remote::remote_error_metadata(error) {
        let exit_code = classify_remote_error(
            &remote.error_code,
            remote.status.map(|status| status.as_u16()),
            remote.retryable,
        );
        return ClassifiedError {
            exit_code,
            error_code: remote.error_code,
            retryable: remote.retryable,
            trace_id: remote.trace_id,
        };
    }
    if error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<studio_native_auth::NativeAuthError>(),
            Some(studio_native_auth::NativeAuthError::CredentialStoreTimeout)
        )
    }) {
        return ClassifiedError {
            exit_code: ExitCode::InfrastructureFailure,
            error_code: "infrastructure.unavailable".into(),
            retryable: true,
            trace_id: None,
        };
    }
    if let Some(runtime) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<reporch_runtime_core::RuntimeError>())
    {
        return ClassifiedError {
            exit_code: if matches!(
                runtime,
                reporch_runtime_core::RuntimeError::RemoteQuotaExceeded
            ) {
                ExitCode::PermissionDenied
            } else {
                ExitCode::InfrastructureFailure
            },
            error_code: runtime.code().into(),
            retryable: runtime.retryable(),
            trace_id: None,
        };
    }
    let message = format!("{error:#}").to_ascii_lowercase();
    if message.contains("cancelled") || message.contains("canceled") {
        return ClassifiedError {
            exit_code: ExitCode::Cancelled,
            error_code: "operation.cancelled".into(),
            retryable: false,
            trace_id: None,
        };
    }
    if message.contains("credential")
        || message.contains("authentication")
        || message.contains("unauthorized")
    {
        return ClassifiedError {
            exit_code: ExitCode::AuthenticationRequired,
            error_code: "auth.required".into(),
            retryable: false,
            trace_id: None,
        };
    }
    if message.contains("forbidden")
        || message.contains("permission")
        || message.contains("quota")
        || message.contains("policy denied")
    {
        return ClassifiedError {
            exit_code: ExitCode::PermissionDenied,
            error_code: "policy.denied".into(),
            retryable: false,
            trace_id: None,
        };
    }
    if message.contains("etag")
        || message.contains("revision conflict")
        || message.contains("stale revision")
        || message.contains("http 409")
    {
        return ClassifiedError {
            exit_code: ExitCode::Conflict,
            error_code: "revision.conflict".into(),
            retryable: false,
            trace_id: None,
        };
    }
    if message.contains("timeout")
        || message.contains("timed out")
        || message.contains("transport")
        || message.contains("connection")
        || message.contains("http 502")
        || message.contains("http 503")
        || message.contains("http 504")
    {
        return ClassifiedError {
            exit_code: ExitCode::InfrastructureFailure,
            error_code: "infrastructure.unavailable".into(),
            retryable: true,
            trace_id: None,
        };
    }
    if message.contains("validation did not pass")
        || message.contains("output validation did not pass")
        || message.contains("cannot be exported")
        || message.contains("release build failed")
        || message.contains("expected verdict")
    {
        return ClassifiedError {
            exit_code: ExitCode::DomainFailure,
            error_code: "operation.failed".into(),
            retryable: false,
            trace_id: None,
        };
    }
    ClassifiedError {
        exit_code: ExitCode::InvalidInput,
        error_code: "input.invalid".into(),
        retryable: false,
        trace_id: None,
    }
}

fn classify_remote_error(error_code: &str, status: Option<u16>, retryable: bool) -> ExitCode {
    match error_code {
        "auth.session_required" => return ExitCode::AuthenticationRequired,
        "auth.forbidden"
        | "auth.organization_forbidden"
        | "authoring.action_restricted"
        | "quota.monthly_cpu_exceeded"
        | "quota.concurrent_validations_exceeded"
        | "review.approval_required"
        | "review.separation_required"
        | "review_pool.assignment_required"
        | "waiver.separation_required" => return ExitCode::PermissionDenied,
        "release.not_ready" | "waiver.required" => return ExitCode::DomainFailure,
        "collaboration.path_conflict"
        | "concurrency.conflict"
        | "concurrency.if_match_required"
        | "idempotency.key_reused"
        | "review_pool.already_claimed"
        | "review_pool.candidate_stale"
        | "trust_appeal.conflict"
        | "webhook.event_conflict"
        | "working_copy.revision_conflict" => return ExitCode::Conflict,
        _ => {}
    }
    match status {
        Some(401) => ExitCode::AuthenticationRequired,
        Some(403 | 429) => ExitCode::PermissionDenied,
        Some(409 | 412 | 428) => ExitCode::Conflict,
        Some(500..=599) | None if retryable => ExitCode::InfrastructureFailure,
        _ => ExitCode::InvalidInput,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    #[test]
    fn stable_exit_codes_match_the_public_contract() {
        assert_eq!(ExitCode::DomainFailure as i32, 1);
        assert_eq!(ExitCode::InvalidInput as i32, 2);
        assert_eq!(ExitCode::Conflict as i32, 3);
        assert_eq!(ExitCode::AuthenticationRequired as i32, 4);
        assert_eq!(ExitCode::PermissionDenied as i32, 5);
        assert_eq!(ExitCode::InfrastructureFailure as i32, 6);
        assert_eq!(ExitCode::Cancelled as i32, 7);
    }

    #[test]
    fn retryable_infrastructure_errors_are_classified_separately() {
        let classified = classify_error(&anyhow!("Studio transport timeout"));
        assert_eq!(classified.exit_code, ExitCode::InfrastructureFailure);
        assert!(classified.retryable);
    }

    #[test]
    fn credential_store_timeout_is_retryable_infrastructure_not_missing_auth() {
        let error = anyhow::Error::new(studio_native_auth::NativeAuthError::CredentialStoreTimeout)
            .context("read the OS credential store");
        let classified = classify_error(&error);
        assert_eq!(classified.exit_code, ExitCode::InfrastructureFailure);
        assert_eq!(classified.error_code, "infrastructure.unavailable");
        assert!(classified.retryable);
    }

    #[test]
    fn runtime_errors_keep_their_stable_error_codes() {
        let error = anyhow::Error::new(
            reporch_runtime_core::RuntimeError::VirtualizationUnavailable("KVM missing".into()),
        );
        let classified = classify_error(&error);
        assert_eq!(classified.exit_code, ExitCode::InfrastructureFailure);
        assert_eq!(classified.error_code, "runtime.virtualization_unavailable");
        assert!(!classified.retryable);

        let unavailable =
            anyhow::Error::new(reporch_runtime_core::RuntimeError::ServiceUnavailable(
                "docker command exceeded 8 seconds".into(),
            ))
            .context("inspect Docker security mode");
        let classified = classify_error(&unavailable);
        assert_eq!(classified.exit_code, ExitCode::InfrastructureFailure);
        assert_eq!(classified.error_code, "runtime.service_unavailable");
        assert!(classified.retryable);
    }

    #[test]
    fn studio_error_codes_and_trace_ids_survive_classification() {
        let error = anyhow::Error::new(crate::studio_remote::StudioApiRequestError::Api {
            status: reqwest::StatusCode::CONFLICT,
            error_code: "working_copy.revision_conflict".into(),
            message: "stale".into(),
            retryable: false,
            trace_id: "server-trace-id".into(),
        });
        let classified = classify_error(&error);
        assert_eq!(classified.exit_code, ExitCode::Conflict);
        assert_eq!(classified.error_code, "working_copy.revision_conflict");
        assert_eq!(classified.trace_id.as_deref(), Some("server-trace-id"));
    }

    #[test]
    fn policy_and_domain_errors_follow_the_stable_exit_contract() {
        assert_eq!(
            classify_remote_error("review.separation_required", Some(422), false),
            ExitCode::PermissionDenied
        );
        assert_eq!(
            classify_remote_error("quota.monthly_cpu_exceeded", Some(429), false),
            ExitCode::PermissionDenied
        );
        assert_eq!(
            classify_remote_error("release.not_ready", Some(422), false),
            ExitCode::DomainFailure
        );
        assert_eq!(
            classify_remote_error("review_pool.candidate_stale", Some(409), false),
            ExitCode::Conflict
        );
    }

    #[test]
    fn unbounded_streams_require_human_or_jsonl_output() {
        assert!(
            CliOutput::new(OutputFormat::Json, false, ColorMode::Never)
                .ensure_streaming_format()
                .is_err()
        );
        assert!(
            CliOutput::new(OutputFormat::Jsonl, false, ColorMode::Never)
                .ensure_streaming_format()
                .is_ok()
        );
    }

    #[test]
    fn raw_artifacts_require_human_output() {
        assert!(
            CliOutput::new(OutputFormat::Human, false, ColorMode::Never)
                .ensure_human_format("completion")
                .is_ok()
        );
        assert!(
            CliOutput::new(OutputFormat::Json, false, ColorMode::Never)
                .ensure_human_format("completion")
                .is_err()
        );
    }
}
