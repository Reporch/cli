use std::io::IsTerminal as _;

use anyhow::Error;
use clap::ValueEnum;
use serde::Serialize;
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
    error_code: &'a str,
    message: String,
    retryable: bool,
    trace_id: String,
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
            trace_id: Uuid::now_v7().to_string(),
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
}

struct ClassifiedError {
    exit_code: ExitCode,
    error_code: &'static str,
    retryable: bool,
}

fn classify_error(error: &Error) -> ClassifiedError {
    let message = format!("{error:#}").to_ascii_lowercase();
    if message.contains("cancelled") || message.contains("canceled") {
        return ClassifiedError {
            exit_code: ExitCode::Cancelled,
            error_code: "operation.cancelled",
            retryable: false,
        };
    }
    if message.contains("credential")
        || message.contains("auth login")
        || message.contains("authentication")
        || message.contains("unauthorized")
    {
        return ClassifiedError {
            exit_code: ExitCode::AuthenticationRequired,
            error_code: "auth.required",
            retryable: false,
        };
    }
    if message.contains("forbidden")
        || message.contains("permission")
        || message.contains("quota")
        || message.contains("policy denied")
    {
        return ClassifiedError {
            exit_code: ExitCode::PermissionDenied,
            error_code: "policy.denied",
            retryable: false,
        };
    }
    if message.contains("etag")
        || message.contains("revision conflict")
        || message.contains("stale revision")
        || message.contains("http 409")
    {
        return ClassifiedError {
            exit_code: ExitCode::Conflict,
            error_code: "revision.conflict",
            retryable: false,
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
            error_code: "infrastructure.unavailable",
            retryable: true,
        };
    }
    if message.contains("validation did not pass")
        || message.contains("release build failed")
        || message.contains("expected verdict")
    {
        return ClassifiedError {
            exit_code: ExitCode::DomainFailure,
            error_code: "operation.failed",
            retryable: false,
        };
    }
    ClassifiedError {
        exit_code: ExitCode::InvalidInput,
        error_code: "input.invalid",
        retryable: false,
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
}
