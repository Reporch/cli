#![forbid(unsafe_code)]

mod archive_safety;
mod authoring;
mod cli_output;
mod desktop_artifact;
mod icpc_export;
mod icpc_import;
mod icpc_legacy;
mod icpc_submit_answer;
mod local_manifest;
mod native_package;
mod polygon_export;
mod polygon_import;
mod profile_config;
mod statement_tex;
mod versioned_package;

use std::fs;
use std::future::Future;
use std::io::{IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use clap::{ArgAction, Args as ClapArgs, CommandFactory, Parser, Subcommand, ValueEnum};
use cli_output::{CliOutput, ColorMode, OutputFormat};
use reporch_cli::local_sandbox::{LocalSandboxOptions, OciRuntime};
use reporch_cli::studio_remote;
use reporch_cli::{NativeAuthOptions, device_auth_config};
#[cfg(test)]
use studio_core::ReleaseManifestV1;
use studio_core::{
    PackageProfile, ProblemType, VersionedReleaseManifest, compatibility_report, validate_manifest,
};
use studio_native_auth::qualification_keyring_canary;

const CLI_STACK_BYTES: usize = 8 * 1024 * 1024;
use studio_native_auth::{KeyringTokenStore, NativeAuthClient};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
#[error("operation interrupted by SIGINT")]
struct CliInterrupted;

#[derive(Debug, Parser)]
#[command(
    name = "reporch",
    version,
    about = "Create, validate, package, and sync Reporch problems",
    after_help = "Quick start:\n  reporch new --title \"A + B\" --directory a-b\n  cd a-b\n  reporch check\n  reporch auth login\n  reporch project create --title \"A + B\"\n  reporch project link --project-id <UUID>\n  reporch project push\n  reporch verify\n\nRun `reporch <command> --help` for command-specific examples."
)]
struct Args {
    /// Resolve relative paths from this directory.
    #[arg(long, global = true, default_value = ".")]
    cwd: PathBuf,
    /// Named connection profile. Package commands use reporch-native, icpc202509, icpc-legacy, polygon-compatible, or domjudge-zip.
    #[arg(long, global = true, env = "REPORCH_PROFILE")]
    profile: Option<String>,
    #[arg(long, global = true, value_enum, default_value_t = OutputFormat::Human)]
    format: OutputFormat,
    /// Alias for --format json.
    #[arg(long, global = true)]
    json: bool,
    /// Never prompt for missing input.
    #[arg(long, global = true)]
    no_input: bool,
    /// Permit an isolated Studio preview when this host cannot run the Reporch VM.
    #[arg(long, global = true, env = "REPORCH_ALLOW_REMOTE_FALLBACK")]
    allow_remote_fallback: bool,
    /// Confirm safe interactive operations.
    #[arg(long, global = true)]
    yes: bool,
    #[arg(long, global = true)]
    quiet: bool,
    #[arg(long, global = true, action = ArgAction::Count)]
    verbose: u8,
    #[arg(long, global = true, value_enum, default_value_t = ColorMode::Auto)]
    color: ColorMode,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create a safe starter problem. This is the shortest form of `project init`.
    New(ProjectInitOptions),
    /// Migrate a pre-1.0 manifest to the human-editable reporch.yaml format.
    Migrate(MigrateOptions),
    /// Validate reporch.yaml and every declared local file without network access.
    Check,
    /// Show local project linkage and dirty state. Alias for `project status`.
    Status,
    /// Show local file and metadata changes. Alias for `project diff`.
    Diff,
    /// Edit localized problem statements.
    Statement(authoring::StatementOptions),
    /// Add and organize tests. With no subcommand, starts a line-oriented guide.
    Test(authoring::TestOptions),
    /// Manage deterministic test generators.
    Generator(authoring::GeneratorOptions),
    /// Configure input validators and their unit cases.
    Validator(authoring::ValidatorOptions),
    /// Configure standard or custom checkers and unit cases.
    Checker(authoring::CheckerOptions),
    /// Manage expected solution verdicts and score ranges.
    Solution(authoring::SolutionOptions),
    /// Generate deterministic answer files with a reference solution.
    Answer(authoring::AnswerOptions),
    /// Define and run generator/oracle/candidate stress suites.
    Stress(authoring::StressOptions),
    /// Configure an interactive problem's interactor.
    Interactor(authoring::InteractorOptions),
    /// Configure a library or grader problem's grader.
    Grader(authoring::GraderOptions),
    /// Manage output-only expected submission mappings.
    Output(authoring::OutputOptions),
    /// Run official Studio validation for the current local commit.
    Verify(studio_remote::ValidateOptions),
    /// Check, push, validate, and submit the current project for review.
    Submit(SubmitOptions),
    /// Publish a verified release. Shorthand for `publication publish`.
    Publish(studio_remote::PublishOptions),
    /// Authenticate this native client with Reporch without storing tokens in files.
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    Project {
        #[command(subcommand)]
        command: ProjectCommand,
    },
    /// Manage project collaborators.
    Member {
        #[command(subcommand)]
        command: MemberCommand,
    },
    /// Inspect authenticated Studio API compatibility and quota; use `runtime doctor` for the local VM.
    Doctor(studio_remote::RemoteConnectionOptions),
    /// Inspect and maintain the mandatory Reporch virtual-machine runtime.
    Runtime {
        #[command(subcommand)]
        command: RuntimeCommand,
    },
    /// Generate a shell completion script on stdout.
    Completion {
        #[arg(value_enum)]
        shell: CompletionShell,
    },
    /// Inspect validation execution quota.
    Quota {
        #[command(subcommand)]
        command: QuotaCommand,
    },
    /// Build, inspect, list, or download immutable Studio releases.
    Release {
        #[command(subcommand)]
        command: ReleaseCommand,
    },
    /// Publish a verified immutable release or inspect publication status.
    Publication {
        #[command(subcommand)]
        command: PublicationCommand,
    },
    /// Inspect or follow official Studio validation evidence.
    Validation {
        #[command(subcommand)]
        command: ValidationCommand,
    },
    /// Follow reconnectable Studio project and validation progress events.
    Events {
        #[command(subcommand)]
        command: EventsCommand,
    },
    /// Manage evidence-bound validation waivers.
    Waiver {
        #[command(subcommand)]
        command: WaiverCommand,
    },
    /// Inspect immutable project revisions.
    Revision {
        #[command(subcommand)]
        command: RevisionCommand,
    },
    /// Submit and decide digest-bound Studio reviews.
    Review {
        #[command(subcommand)]
        command: ReviewCommand,
    },
    Manifest {
        #[command(subcommand)]
        command: ManifestCommand,
    },
    Package {
        #[command(subcommand)]
        command: PackageCommand,
    },
    /// Plan or run a networkless Reporch VM command; explicit OCI backends are deprecated.
    Sandbox {
        #[command(subcommand)]
        command: SandboxCommand,
    },
    /// List, inspect, or explicitly install signed digest-pinned toolchains.
    Toolchain {
        #[command(subcommand)]
        command: ToolchainCommand,
    },
    /// Verify signed desktop release artifacts without installing them.
    Desktop {
        #[command(subcommand)]
        command: DesktopCommand,
    },
    /// Verify a signed release artifact without extracting or executing it.
    Artifact {
        #[command(subcommand)]
        command: ArtifactCommand,
    },
    #[command(hide = true)]
    QualificationSelfTest,
}

#[derive(Debug, Clone, ClapArgs)]
struct SubmitOptions {
    #[command(flatten)]
    connection: studio_remote::RemoteConnectionOptions,
    #[arg(long, default_value = "CLI submit")]
    message: String,
    #[arg(long, default_value_t = 30 * 60)]
    timeout_seconds: u64,
}

#[derive(Debug, Clone, ClapArgs)]
#[command(
    after_help = "Migration applies only to a pre-1.0 directory that has reporch.problem.json and no reporch.yaml. It creates reporch.problem.pre-1.0.json without overwriting and verifies semantic/file-hash equality. A modern project reports migrated:false because no migration is needed.\n\nExample:\n  reporch migrate --directory ./legacy-problem --yes"
)]
struct MigrateOptions {
    #[arg(long, default_value = ".")]
    directory: PathBuf,
}

#[derive(Debug, Subcommand)]
enum DesktopCommand {
    /// Compatibility alias for caller-key Minisign verification; it does not establish official Reporch trust.
    VerifyUpdaterArtifact(desktop_artifact::VerifyDesktopArtifactOptions),
}

#[derive(Debug, Subcommand)]
enum ArtifactCommand {
    /// Stream and verify one Minisign artifact with an explicit trust root.
    VerifyMinisign(desktop_artifact::VerifyDesktopArtifactOptions),
}

#[derive(Debug, Subcommand)]
enum SandboxCommand {
    /// Verify the runtime and print the exact immutable execution plan.
    Plan(SandboxOptions),
    /// Execute a previously reviewable strict plan.
    Run(SandboxOptions),
}

#[derive(Debug, Subcommand)]
enum ToolchainCommand {
    /// List the toolchains in the embedded signed index.
    List,
    /// Inspect whether an exact signed toolchain digest is installed locally.
    Inspect {
        id: String,
        #[arg(long, value_enum, default_value_t = SandboxRuntime::Auto)]
        runtime: SandboxRuntime,
    },
    /// Explicitly pull one toolchain from the signed index.
    Install {
        id: String,
        #[arg(long, value_enum, default_value_t = SandboxRuntime::Auto)]
        runtime: SandboxRuntime,
    },
    /// Install signed toolchains ahead of time for offline use. First installation can take about a minute; cached runs are faster.
    Prefetch {
        /// Toolchain IDs. When omitted, prefetches the complete signed catalog.
        ids: Vec<String>,
        #[arg(long, value_enum, default_value_t = SandboxRuntime::Auto)]
        runtime: SandboxRuntime,
    },
}

#[derive(Debug, Subcommand)]
enum RuntimeCommand {
    /// Report installation, backend, protocol, and virtualization capability.
    Status,
    /// Diagnose runtime installation and host virtualization support.
    Doctor {
        /// Apply only repairs that do not require implicit privilege elevation.
        #[arg(long)]
        fix: bool,
    },
    /// Atomically install the newest compatible signed runtime bundle.
    Update,
    /// Reinstall and verify the currently selected runtime bundle.
    Repair,
    /// Replace runtime assets while preserving projects and authentication.
    Reset {
        #[arg(long)]
        yes: bool,
    },
    /// Run release qualification against the installed native VM backend.
    #[command(hide = true)]
    Qualification {
        #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u32).range(1..=1000))]
        iterations: u32,
        #[arg(long, default_value = "bash-5.3")]
        toolchain: String,
    },
}

#[derive(Debug, Clone, ClapArgs)]
struct SandboxOptions {
    #[arg(long, value_enum, default_value_t = SandboxRuntime::Auto)]
    runtime: SandboxRuntime,
    /// Digest-pinned image, for example registry/image@sha256:<64 hex>.
    #[arg(long)]
    image: String,
    #[arg(long, default_value = ".")]
    project_directory: PathBuf,
    #[arg(long, default_value_t = 30)]
    timeout_seconds: u64,
    #[arg(long, default_value_t = 512)]
    memory_mib: u64,
    #[arg(long, default_value_t = 1.0)]
    cpus: f64,
    #[arg(long, default_value_t = 1_024)]
    output_kib: u64,
    #[arg(last = true, required = true)]
    command: Vec<String>,
}

#[derive(Debug, Clone, Copy, ValueEnum, Default)]
enum SandboxRuntime {
    #[default]
    Auto,
    /// Deprecated explicit compatibility backend.
    Podman,
    /// Deprecated explicit compatibility backend.
    Docker,
}

impl SandboxRuntime {
    fn into_oci(self) -> OciRuntime {
        match self {
            Self::Auto => OciRuntime::Auto,
            Self::Podman => OciRuntime::Podman,
            Self::Docker => OciRuntime::Docker,
        }
    }
}

impl SandboxOptions {
    fn into_local(self) -> LocalSandboxOptions {
        LocalSandboxOptions {
            runtime: self.runtime.into_oci(),
            image: self.image,
            project_directory: self.project_directory,
            command: self.command,
            timeout: Duration::from_secs(self.timeout_seconds),
            memory_mib: self.memory_mib,
            cpus: self.cpus,
            output_kib: self.output_kib,
        }
    }
}

#[derive(Debug, Subcommand)]
enum AuthCommand {
    /// Sign in using OAuth Device Authorization and store credentials in the OS keychain.
    Login(NativeAuthOptions),
    /// Inspect the local keychain session without making a network request.
    Status(NativeAuthOptions),
    /// Revoke the refresh token when possible and always remove the local credential.
    Logout(NativeAuthOptions),
}

#[derive(Debug, Subcommand)]
#[command(
    after_help = "Package profiles:\n  reporch-native (alias: reporch)\n  icpc202509 (alias: icpc-2025-09)\n  icpc-legacy\n  polygon-compatible (alias: polygon)\n  domjudge-zip (alias: domjudge)"
)]
enum PackageCommand {
    /// Export a package. The project package profile and manifest directory are the defaults.
    Export {
        #[arg(value_name = "MANIFEST")]
        manifest: PathBuf,
        #[arg(value_name = "OUTPUT")]
        output: PathBuf,
        #[arg(long, value_name = "SOURCE_ROOT")]
        source_root: Option<PathBuf>,
    },
    /// Import a supported package into a new or empty project directory.
    Import { input: PathBuf, directory: PathBuf },
}

#[derive(Debug, Clone, ClapArgs)]
struct ProjectInitOptions {
    #[arg(long)]
    title: String,
    #[arg(long, default_value = ".")]
    directory: PathBuf,
    /// Initialize alongside unrelated existing files after checking every generated path for collisions.
    #[arg(long)]
    allow_non_empty: bool,
    /// Include a validator and unit matrix for immediate ICPC, Polygon, and DOMjudge export. `reporch new` enables this automatically.
    #[arg(long)]
    portable: bool,
    #[arg(long, value_enum, default_value_t = studio_remote::RemoteProblemType::Standard)]
    problem_type: studio_remote::RemoteProblemType,
    /// Bind the local manifest to an existing private Studio project.
    #[arg(long)]
    project_id: Option<Uuid>,
}

#[derive(Debug, Subcommand)]
enum ProjectCommand {
    /// Create a safe starter project without overwriting existing files.
    Init(ProjectInitOptions),
    /// Link the current directory to an existing private Studio project.
    Link {
        #[command(flatten)]
        connection: studio_remote::RemoteConnectionOptions,
        #[arg(long)]
        project_id: Uuid,
    },
    /// List Studio projects available to the current account.
    List(studio_remote::RemoteConnectionOptions),
    /// Show the linked Studio project.
    Show(studio_remote::RemoteConnectionOptions),
    /// Open the linked project in Reporch Studio.
    Open {
        #[arg(long)]
        project_id: Option<Uuid>,
        #[arg(
            long,
            env = "REPORCH_STUDIO_WEB_URL",
            default_value = "https://studio.reporch.com"
        )]
        web_url: String,
    },
    /// Show local linkage and dirty state.
    Status,
    /// Compare reporch.yaml and local files with the generated manifest.
    Diff,
    /// Create a private Studio project through native OAuth.
    Create(studio_remote::CreateOptions),
    /// Pull an immutable Studio commit and every digest-bound file.
    Pull(studio_remote::PullOptions),
    /// Upload changed files and create an immutable Studio commit.
    Push(studio_remote::PushOptions),
    /// Enqueue deterministic validation and optionally wait for evidence.
    Validate(studio_remote::ValidateOptions),
    /// Build and download a verified immutable Reporch package.
    Package(studio_remote::PackageOptions),
}

#[derive(Debug, Subcommand)]
enum ReviewCommand {
    /// Submit a passed validation for review.
    Submit(studio_remote::SubmitReviewOptions),
    /// List reviews for a project, newest first.
    List(studio_remote::ListReviewsOptions),
    /// Request an independent reviewer from the Reporch review pool.
    Request(studio_remote::ReviewPoolRequestOptions),
    /// List claimable assignments. Requires the review-pool entitlement.
    Inbox(studio_remote::ReviewPoolInboxOptions),
    /// Show one digest-bound review.
    Show(studio_remote::ReviewShowOptions),
    /// Show a review-pool request or assignment.
    Status(studio_remote::ReviewPoolTargetOptions),
    /// Atomically claim a review-pool request.
    Claim(studio_remote::ReviewPoolTargetOptions),
    /// Cancel a review-pool request created by the current account.
    Cancel(studio_remote::ReviewPoolTargetOptions),
    /// Approve a review as an independent reviewer.
    Approve(studio_remote::ApproveReviewOptions),
    /// Request changes with an explanatory comment.
    RequestChanges(studio_remote::RequestChangesOptions),
}

#[derive(Debug, Subcommand)]
enum MemberCommand {
    /// Search Reporch identities eligible for this project.
    Search(studio_remote::MemberSearchOptions),
    /// List current project memberships.
    List(studio_remote::MemberScopeOptions),
    /// Add a project member.
    Add(studio_remote::UpsertMemberOptions),
    /// Change a project member's role.
    Update(studio_remote::UpsertMemberOptions),
    /// Remove a project member. Owners cannot be removed.
    Remove(studio_remote::RemoveMemberOptions),
}

#[derive(Debug, Subcommand)]
enum QuotaCommand {
    /// Show CPU, concurrency, and storage quota for the authenticated account.
    Show(studio_remote::RemoteConnectionOptions),
}

#[derive(Debug, Subcommand)]
enum ReleaseCommand {
    /// Build an immutable release from passed official validation evidence.
    Build(studio_remote::ReleaseBuildOptions),
    /// List immutable releases for the linked project.
    List(studio_remote::ReleaseScopeOptions),
    /// Show one immutable release.
    Show(studio_remote::ReleaseShowOptions),
    /// Download a ready release with size and SHA-256 verification.
    Download(studio_remote::ReleaseDownloadOptions),
}

#[derive(Debug, Subcommand)]
enum PublicationCommand {
    /// Publish a verified immutable release after explicit confirmation.
    Publish(studio_remote::PublishOptions),
    /// Show the idempotent publication state for a release or linked project.
    Status(studio_remote::PublicationOptions),
}

#[derive(Debug, Subcommand)]
enum ValidationCommand {
    /// List official validation runs for the linked project.
    List(studio_remote::ValidationScopeOptions),
    /// Show one validation run and its deterministic evidence summary.
    Show(studio_remote::ValidationInspectOptions),
    /// Follow one validation run until it passes or reaches a terminal failure.
    Watch(studio_remote::ValidationInspectOptions),
}

#[derive(Debug, Subcommand)]
enum EventsCommand {
    /// Follow authorized events, resuming from the last durable cursor.
    Watch(studio_remote::EventsWatchOptions),
}

#[derive(Debug, Subcommand)]
enum WaiverCommand {
    /// List active evidence-bound waivers for the linked project.
    List(studio_remote::WaiverScopeOptions),
    /// Create a reasoned waiver for a waivable validation finding.
    Create(studio_remote::CreateWaiverOptions),
    /// Revoke a waiver without changing immutable validation evidence.
    Revoke(studio_remote::RevokeWaiverOptions),
}

#[derive(Debug, Subcommand)]
enum RevisionCommand {
    /// List immutable project revisions, newest first.
    List(studio_remote::RevisionScopeOptions),
    /// Show one immutable project revision.
    Show(studio_remote::RevisionShowOptions),
    /// Compare two immutable project revisions.
    Diff(studio_remote::RevisionDiffOptions),
    /// Restore an immutable revision into a new or empty checkout directory.
    Restore(studio_remote::RevisionRestoreOptions),
}

#[derive(Debug, Subcommand)]
enum ManifestCommand {
    Validate {
        /// Immutable manifest or authoring YAML. When omitted, discovers the current project's reporch.yaml.
        #[arg(value_name = "PATH")]
        path: Option<PathBuf>,
    },
    Digest {
        /// Immutable manifest or authoring YAML. When omitted, discovers the current project's reporch.yaml.
        #[arg(value_name = "PATH")]
        path: Option<PathBuf>,
    },
    Compatibility {
        /// Immutable manifest or authoring YAML. When omitted, discovers the current project's reporch.yaml.
        #[arg(value_name = "PATH")]
        path: Option<PathBuf>,
        /// Exit 1 when the target format cannot represent this problem.
        #[arg(long, visible_alias = "strict")]
        require_exportable: bool,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CompatibilityProfile {
    #[value(name = "reporch-native", alias = "reporch_native", alias = "reporch")]
    ReporchNative,
    #[value(
        name = "icpc202509",
        alias = "icpc_202509",
        alias = "icpc_2025_09",
        alias = "icpc-2025-09"
    )]
    Icpc202509,
    #[value(name = "icpc-legacy", alias = "icpc_legacy")]
    IcpcLegacy,
    #[value(
        name = "polygon-compatible",
        alias = "polygon_compatible",
        alias = "polygon"
    )]
    PolygonCompatible,
    #[value(name = "domjudge-zip", alias = "domjudge_zip", alias = "domjudge")]
    DomjudgeZip,
}

impl From<CompatibilityProfile> for PackageProfile {
    fn from(value: CompatibilityProfile) -> Self {
        match value {
            CompatibilityProfile::ReporchNative => Self::ReporchNative,
            CompatibilityProfile::Icpc202509 => Self::Icpc202509,
            CompatibilityProfile::IcpcLegacy => Self::IcpcLegacy,
            CompatibilityProfile::PolygonCompatible => Self::PolygonCompatible,
            CompatibilityProfile::DomjudgeZip => Self::DomjudgeZip,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CompletionShell {
    Bash,
    Zsh,
    Fish,
    PowerShell,
    Elvish,
}

impl From<CompletionShell> for clap_complete::Shell {
    fn from(value: CompletionShell) -> Self {
        match value {
            CompletionShell::Bash => Self::Bash,
            CompletionShell::Zsh => Self::Zsh,
            CompletionShell::Fish => Self::Fish,
            CompletionShell::PowerShell => Self::PowerShell,
            CompletionShell::Elvish => Self::Elvish,
        }
    }
}

fn main() {
    let result = std::thread::Builder::new()
        .name("reporch-main".into())
        .stack_size(CLI_STACK_BYTES)
        .spawn(cli_main)
        .expect("spawn Reporch CLI main thread")
        .join();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::main]
async fn cli_main() {
    match profile_config::bootstrap() {
        Ok(Some(exit_code)) => std::process::exit(exit_code),
        Ok(None) => {}
        Err(error) => {
            if raw_arguments_request_json() {
                eprintln!(
                    "{}",
                    serde_json::json!({
                        "schema": "reporch.cli-error.v1",
                        "command": "configuration",
                        "error_code": "configuration.invalid",
                        "message": format!("{error:#}"),
                        "retryable": false,
                        "trace_id": Uuid::now_v7(),
                    })
                );
            } else {
                eprintln!("configuration.invalid: {error:#}");
            }
            std::process::exit(2);
        }
    }
    let arguments = match Args::try_parse() {
        Ok(arguments) => arguments,
        Err(error) => {
            let exit_code = error.exit_code();
            if raw_arguments_request_json() {
                if exit_code == 0 {
                    print!("{error}");
                } else {
                    eprintln!(
                        "{}",
                        serde_json::json!({
                            "schema": "reporch.cli-error.v1",
                            "command": "parse",
                            "error_code": "input.invalid",
                            "message": error.to_string(),
                            "retryable": false,
                            "trace_id": Uuid::now_v7(),
                        })
                    );
                }
            } else {
                let _ = error.print();
            }
            std::process::exit(exit_code);
        }
    };
    let format = if arguments.json {
        OutputFormat::Json
    } else {
        arguments.format
    };
    let output = CliOutput::new(format, arguments.quiet, arguments.color);
    let command = command_name(&arguments.command);
    if let Err(error) = run_until_interrupt(run(arguments, &output), tokio::signal::ctrl_c()).await
    {
        if error.downcast_ref::<CliInterrupted>().is_some() {
            std::process::exit(130);
        }
        let exit_code = output.emit_error(command, &error);
        std::process::exit(exit_code as i32);
    }
}

async fn run_until_interrupt<C, S>(command: C, interrupt: S) -> Result<()>
where
    C: Future<Output = Result<()>>,
    S: Future<Output = std::io::Result<()>>,
{
    tokio::pin!(command);
    tokio::pin!(interrupt);
    tokio::select! {
        biased;
        signal = &mut interrupt => {
            signal.context("wait for SIGINT")?;
            Err(CliInterrupted.into())
        }
        result = &mut command => result,
    }
}

async fn run(arguments: Args, output: &CliOutput) -> Result<()> {
    let Args {
        cwd,
        profile,
        no_input,
        allow_remote_fallback,
        yes,
        verbose,
        command,
        ..
    } = arguments;
    let cwd = fs::canonicalize(&cwd).with_context(|| format!("resolve --cwd {}", cwd.display()))?;
    std::env::set_current_dir(&cwd)
        .with_context(|| format!("change working directory to {}", cwd.display()))?;
    reporch_cli::local_sandbox::configure_remote_fallback(
        allow_remote_fallback,
        no_input,
        profile.as_deref(),
    );
    ensure_mandatory_runtime(&command, output).await?;
    let _configuration = (profile.clone(), no_input, verbose, output.colors_enabled());
    let package_profile = profile_config::package_profile_argument()
        .map(|value| {
            CompatibilityProfile::from_str(&value, true).map_err(|_| {
                anyhow::anyhow!(
                    "unsupported package profile {value:?}; expected one of reporch-native, icpc202509, icpc-legacy, polygon-compatible, or domjudge-zip"
                )
            })
        })
        .transpose()?;

    match command {
        Command::New(options) => execute_project_init(options, "new", output),
        Command::Migrate(options) => migrate(&options, yes, output),
        Command::Check => check_project(output),
        Command::Status => emit_project_status("status", output),
        Command::Diff => emit_project_diff("diff", output),
        Command::Statement(options) => authoring::statement(options, output),
        Command::Test(options) => authoring::tests(options, output, no_input),
        Command::Generator(options) => authoring::generator(options, output).await,
        Command::Validator(options) => authoring::validator(options, output).await,
        Command::Checker(options) => authoring::checker(options, output).await,
        Command::Solution(options) => authoring::solution(options, output),
        Command::Answer(options) => authoring::answer(options, output).await,
        Command::Stress(options) => authoring::stress(options, output).await,
        Command::Interactor(options) => authoring::interactor(options, output).await,
        Command::Grader(options) => authoring::grader(options, output).await,
        Command::Output(options) => authoring::output_submission(options, output).await,
        Command::Verify(options) => {
            let validation = studio_remote::validate_operation(&options).await?;
            if validation.detail.as_ref().is_some_and(|detail| {
                detail.status != studio_contracts::ValidationRunStatus::Passed
            }) {
                bail!("Studio validation did not pass");
            }
            let human = if validation.detail.is_some() {
                "Studio verification passed"
            } else {
                "Studio verification queued"
            };
            output.emit("verify", &validation, human)
        }
        Command::Submit(options) => submit_project(options, output).await,
        Command::Publish(options) => {
            confirm_publication(yes, no_input)?;
            let publication = studio_remote::publish_operation(&options).await?;
            output.emit(
                "publish",
                &publication,
                &format!("Publication status: {:?}", publication.status),
            )
        }
        Command::Auth { command } => match command {
            AuthCommand::Login(options) => auth_login(&options, no_input, output).await,
            AuthCommand::Status(options) => auth_status(&options, output).await,
            AuthCommand::Logout(options) => auth_logout(&options, profile.as_deref(), output).await,
        },
        Command::Project { command } => match command {
            ProjectCommand::Init(options) => execute_project_init(options, "project init", output),
            ProjectCommand::Link {
                connection,
                project_id,
            } => {
                let projects = studio_remote::list_projects_operation(&connection).await?;
                ensure!(
                    projects
                        .items
                        .iter()
                        .any(|project| project.id == project_id),
                    "Studio project {project_id} was not found or is not accessible"
                );
                let status = link_local_project(Path::new("."), &connection.api_url, project_id)?;
                output.emit(
                    "project link",
                    &status,
                    &format!("Linked {}", status.project_id),
                )
            }
            ProjectCommand::List(connection) => {
                let projects = studio_remote::list_projects_operation(&connection).await?;
                output.emit(
                    "project list",
                    &projects,
                    &format!("{} project(s)", projects.items.len()),
                )
            }
            ProjectCommand::Show(connection) => {
                let root = reporch_cli::local_project::discover_project(Path::new("."))?;
                let state = reporch_cli::local_project::read_local_state(&root)?;
                let project_id = state
                    .remote
                    .as_ref()
                    .context("project is not linked; run reporch project link")?
                    .project_id;
                let projects = studio_remote::list_projects_operation(&connection).await?;
                let project = projects
                    .items
                    .into_iter()
                    .find(|project| project.id == project_id)
                    .context("linked Studio project is no longer accessible")?;
                output.emit("project show", &project, &project.title)
            }
            ProjectCommand::Open {
                project_id,
                web_url,
            } => {
                let project_id = studio_remote::current_project_id(project_id)?;
                let mut url = url::Url::parse(&web_url).context("parse Studio web URL")?;
                ensure!(url.scheme() == "https", "Studio web URL must use HTTPS");
                ensure!(
                    url.username().is_empty() && url.password().is_none(),
                    "Studio web URL cannot contain credentials"
                );
                url.set_query(None);
                url.set_fragment(None);
                url.set_path(&format!("/projects/{project_id}"));
                open::that(url.as_str()).context("open Studio project")?;
                output.emit(
                    "project open",
                    &serde_json::json!({ "project_id": project_id, "url": url }),
                    &format!("Opened project {project_id}"),
                )
            }
            ProjectCommand::Status => emit_project_status("project status", output),
            ProjectCommand::Diff => emit_project_diff("project diff", output),
            ProjectCommand::Create(options) => {
                let project = studio_remote::create_operation(&options).await?;
                output.emit(
                    "project create",
                    &project,
                    &format!("Created {} ({})", project.title, project.id),
                )
            }
            ProjectCommand::Pull(options) => {
                let pulled = studio_remote::pull_operation(&options).await?;
                output.emit(
                    "project pull",
                    &pulled,
                    &format!(
                        "Pulled commit {} into {}",
                        pulled.commit_id,
                        pulled.directory.display()
                    ),
                )
            }
            ProjectCommand::Push(options) => {
                let pushed = studio_remote::push_operation(&options).await?;
                output.emit(
                    "project push",
                    &pushed,
                    &format!(
                        "Pushed {} file(s) · commit {}",
                        pushed.uploaded_files, pushed.commit.id
                    ),
                )
            }
            ProjectCommand::Validate(options) => {
                let validation = studio_remote::validate_operation(&options).await?;
                if validation.detail.as_ref().is_some_and(|detail| {
                    detail.status != studio_contracts::ValidationRunStatus::Passed
                }) {
                    bail!("Studio validation did not pass");
                }
                let human = match &validation.detail {
                    Some(detail) => format!("Validation {} passed", detail.id),
                    None => format!("Validation {} queued", validation.queued.id),
                };
                output.emit("project validate", &validation, &human)
            }
            ProjectCommand::Package(options) => {
                let package = studio_remote::package_operation(&options).await?;
                output.emit(
                    "project package",
                    &package,
                    &format!(
                        "Downloaded release {} to {}",
                        package.release.id,
                        package.output.display()
                    ),
                )
            }
        },
        Command::Member { command } => match command {
            MemberCommand::Search(options) => {
                let identities = studio_remote::search_members_operation(&options).await?;
                output.emit(
                    "member search",
                    &identities,
                    &format!("{} matching account(s)", identities.items.len()),
                )
            }
            MemberCommand::List(options) => {
                let members = studio_remote::list_members_operation(&options).await?;
                output.emit(
                    "member list",
                    &members,
                    &format!("{} member(s)", members.items.len()),
                )
            }
            MemberCommand::Add(options) => {
                let member = studio_remote::upsert_member_operation(&options).await?;
                output.emit(
                    "member add",
                    &member,
                    &format!("Added {}", member.member.subject),
                )
            }
            MemberCommand::Update(options) => {
                let member = studio_remote::upsert_member_operation(&options).await?;
                output.emit(
                    "member update",
                    &member,
                    &format!("Updated {}", member.member.subject),
                )
            }
            MemberCommand::Remove(options) => {
                let removed = studio_remote::remove_member_operation(&options).await?;
                output.emit("member remove", &removed, "Removed project member")
            }
        },
        Command::Doctor(connection) => {
            let capabilities = studio_remote::capabilities_operation(&connection).await?;
            let quota = studio_remote::quota_operation(&connection).await?;
            let local = match reporch_cli::local_project::discover_project(Path::new(".")) {
                Ok(root) => Some(local_project_status(&root)?),
                Err(_) => None,
            };
            let data = serde_json::json!({
                "schema": "reporch.doctor.v1",
                "status": "healthy",
                "capabilities": capabilities,
                "quota": quota,
                "local_project": local,
            });
            output.emit(
                "doctor",
                &data,
                "Authentication, API compatibility, and quota are healthy",
            )
        }
        Command::Runtime { command } => match command {
            RuntimeCommand::Status => {
                let status = reporch_runtime_host::status().await?;
                let data = runtime_status_output(&status)?;
                output.emit("runtime status", &data, &runtime_status_human(&status))
            }
            RuntimeCommand::Doctor { fix } => {
                let mut report = reporch_runtime_host::doctor().await?;
                if fix
                    && report
                        .checks
                        .iter()
                        .any(|check| !check.passed && check.repairable)
                {
                    if report.status.installed_version.is_some() {
                        reporch_runtime_host::repair().await?;
                    } else {
                        reporch_runtime_host::update().await?;
                    }
                    report = reporch_runtime_host::doctor().await?;
                }
                let passed = report.checks.iter().filter(|check| check.passed).count();
                if passed != report.checks.len() {
                    return Err(cli_output::domain_error(
                        "runtime.doctor_failed",
                        format!("{passed}/{} runtime checks passed", report.checks.len()),
                        &report,
                    ));
                }
                output.emit(
                    "runtime doctor",
                    &report,
                    &format!("{passed}/{} runtime checks passed", report.checks.len()),
                )
            }
            RuntimeCommand::Update => {
                let result = reporch_runtime_host::update().await?;
                output.emit(
                    "runtime update",
                    &result,
                    &format!("Installed Reporch Runtime {}", result.installed_version),
                )
            }
            RuntimeCommand::Repair => {
                let result = reporch_runtime_host::repair().await?;
                output.emit(
                    "runtime repair",
                    &result,
                    &format!("Repaired Reporch Runtime {}", result.installed_version),
                )
            }
            RuntimeCommand::Reset { yes } => {
                ensure!(yes, "runtime reset requires --yes");
                let result = reporch_runtime_host::reset().await?;
                output.emit(
                    "runtime reset",
                    &result,
                    &format!("Reset Reporch Runtime to {}", result.installed_version),
                )
            }
            RuntimeCommand::Qualification {
                iterations,
                toolchain,
            } => {
                let result =
                    reporch_runtime_host::qualify_installed_native_runtime(iterations, &toolchain)
                        .await?;
                output.emit(
                    "runtime qualification",
                    &result,
                    &format!(
                        "Qualified {} native VM iterations; p95 {} ms",
                        result.iterations, result.p95_ms
                    ),
                )
            }
        },
        Command::Completion { shell } => generate_completion(shell, output),
        Command::Quota { command } => match command {
            QuotaCommand::Show(connection) => {
                let quota = studio_remote::quota_operation(&connection).await?;
                output.emit(
                    "quota show",
                    &quota,
                    &format!(
                        "{} CPU-ms remaining · {}/{} validations active",
                        quota.monthly_cpu_remaining_millis,
                        quota.active_validations,
                        quota.concurrent_validation_limit
                    ),
                )
            }
        },
        Command::Release { command } => match command {
            ReleaseCommand::Build(options) => {
                let release = studio_remote::build_release_operation(&options).await?;
                output.emit(
                    "release build",
                    &release,
                    &format!("Release {} · {:?}", release.id, release.status),
                )
            }
            ReleaseCommand::List(options) => {
                let releases = studio_remote::list_releases_operation(&options).await?;
                output.emit(
                    "release list",
                    &releases,
                    &format!("{} release(s)", releases.items.len()),
                )
            }
            ReleaseCommand::Show(options) => {
                let release = studio_remote::show_release_operation(&options).await?;
                output.emit(
                    "release show",
                    &release,
                    &format!("Release {} · {:?}", release.id, release.status),
                )
            }
            ReleaseCommand::Download(options) => {
                let package = studio_remote::download_release_operation(&options).await?;
                output.emit(
                    "release download",
                    &package,
                    &format!(
                        "Downloaded release {} to {}",
                        package.release.id,
                        package.output.display()
                    ),
                )
            }
        },
        Command::Publication { command } => match command {
            PublicationCommand::Publish(options) => {
                confirm_publication(yes, no_input)?;
                let publication = studio_remote::publish_operation(&options).await?;
                output.emit(
                    "publication publish",
                    &publication,
                    &format!("Publication status: {:?}", publication.status),
                )
            }
            PublicationCommand::Status(options) => {
                let publication = studio_remote::publication_status_operation(&options).await?;
                output.emit(
                    "publication status",
                    &publication,
                    &format!("Publication status: {:?}", publication.status),
                )
            }
        },
        Command::Validation { command } => match command {
            ValidationCommand::List(options) => {
                let validations = studio_remote::list_validations_operation(&options).await?;
                output.emit(
                    "validation list",
                    &validations,
                    &format!("{} validation(s)", validations.items.len()),
                )
            }
            ValidationCommand::Show(options) => {
                let validation = studio_remote::validation_show_operation(&options).await?;
                output.emit(
                    "validation show",
                    &validation,
                    &format!("Validation {}: {:?}", validation.id, validation.status),
                )
            }
            ValidationCommand::Watch(options) => {
                let validation = studio_remote::validation_watch_operation(&options).await?;
                if validation.status != studio_contracts::ValidationRunStatus::Passed {
                    bail!("Studio validation did not pass");
                }
                output.emit(
                    "validation watch",
                    &validation,
                    &format!("Validation {} passed", validation.id),
                )
            }
        },
        Command::Events { command } => match command {
            EventsCommand::Watch(options) => {
                if options.max_events.is_none() {
                    output.ensure_streaming_format()?;
                }
                let bounded = options.max_events.is_some();
                let watched = studio_remote::watch_events_operation(&options, |item| {
                    output.emit(
                        "events watch",
                        item,
                        &format!(
                            "{} · {}",
                            item.event.event_type,
                            item.event
                                .project_id
                                .map_or_else(|| "global".into(), |id| id.to_string())
                        ),
                    )
                })
                .await?;
                if watched.interrupted {
                    return Err(CliInterrupted.into());
                }
                if bounded {
                    output.emit(
                        "events watch",
                        &watched,
                        &format!("Received {} event(s)", watched.events.len()),
                    )
                } else {
                    output.emit("events watch", &watched, "Event stream stopped")
                }
            }
        },
        Command::Waiver { command } => match command {
            WaiverCommand::List(options) => {
                let waivers = studio_remote::list_waivers_operation(&options).await?;
                output.emit(
                    "waiver list",
                    &waivers,
                    &format!("{} waiver(s)", waivers.items.len()),
                )
            }
            WaiverCommand::Create(options) => {
                let waiver = studio_remote::create_waiver_operation(&options).await?;
                output.emit(
                    "waiver create",
                    &waiver,
                    &format!("Created waiver {}", waiver.id),
                )
            }
            WaiverCommand::Revoke(options) => {
                let waiver = studio_remote::revoke_waiver_operation(&options).await?;
                output.emit(
                    "waiver revoke",
                    &waiver,
                    &format!("Revoked waiver {}", waiver.id),
                )
            }
        },
        Command::Revision { command } => match command {
            RevisionCommand::List(options) => {
                let revisions = studio_remote::list_revisions_operation(&options).await?;
                output.emit(
                    "revision list",
                    &revisions,
                    &format!("{} revision(s)", revisions.items.len()),
                )
            }
            RevisionCommand::Show(options) => {
                let revision = studio_remote::show_revision_operation(&options).await?;
                output.emit(
                    "revision show",
                    &revision,
                    &format!("Revision {} · sequence {}", revision.id, revision.sequence),
                )
            }
            RevisionCommand::Diff(options) => {
                let diff = studio_remote::diff_revisions_operation(&options).await?;
                let changes = diff.metadata_changed.len()
                    + diff.files_added.len()
                    + diff.files_modified.len()
                    + diff.files_removed.len();
                output.emit("revision diff", &diff, &format!("{changes} change(s)"))
            }
            RevisionCommand::Restore(options) => {
                let restored = studio_remote::restore_revision_operation(&options).await?;
                output.emit(
                    "revision restore",
                    &restored,
                    &format!(
                        "Restored revision {} into {}",
                        restored.commit_id,
                        restored.directory.display()
                    ),
                )
            }
        },
        Command::Review { command } => match command {
            ReviewCommand::Submit(options) => {
                let review = studio_remote::submit_review_operation(&options).await?;
                output.emit(
                    "review submit",
                    &review,
                    &format!("Submitted review {}", review.id),
                )
            }
            ReviewCommand::List(options) => {
                let reviews = studio_remote::list_reviews_operation(&options).await?;
                output.emit(
                    "review list",
                    &reviews,
                    &format!("{} review(s)", reviews.items.len()),
                )
            }
            ReviewCommand::Request(options) => {
                let request = studio_remote::request_review_pool_operation(&options).await?;
                output.emit(
                    "review request",
                    &request,
                    &format!("Requested independent review {}", request.id),
                )
            }
            ReviewCommand::Inbox(options) => {
                let inbox = studio_remote::list_review_pool_inbox_operation(&options).await?;
                output.emit(
                    "review inbox",
                    &inbox,
                    &format!("{} claimable review(s)", inbox.items.len()),
                )
            }
            ReviewCommand::Show(options) => {
                let review = studio_remote::show_review_operation(&options).await?;
                output.emit(
                    "review show",
                    &review,
                    &format!("Review {} · {:?}", review.id, review.status),
                )
            }
            ReviewCommand::Status(options) => {
                let request = studio_remote::review_pool_status_operation(&options).await?;
                output.emit(
                    "review status",
                    &request,
                    &format!("Review request {} · {:?}", request.id, request.status),
                )
            }
            ReviewCommand::Claim(options) => {
                let request = studio_remote::claim_review_pool_operation(&options).await?;
                output.emit(
                    "review claim",
                    &request,
                    &format!("Claimed review request {}", request.id),
                )
            }
            ReviewCommand::Cancel(options) => {
                let request = studio_remote::cancel_review_pool_operation(&options).await?;
                output.emit(
                    "review cancel",
                    &request,
                    &format!("Cancelled review request {}", request.id),
                )
            }
            ReviewCommand::Approve(options) => {
                let review = studio_remote::approve_review_operation(&options).await?;
                output.emit(
                    "review approve",
                    &review,
                    &format!("Approved review {}", review.id),
                )
            }
            ReviewCommand::RequestChanges(options) => {
                let review = studio_remote::request_review_changes_operation(&options).await?;
                output.emit(
                    "review request-changes",
                    &review,
                    &format!("Requested changes on review {}", review.id),
                )
            }
        },
        Command::Manifest { command } => match command {
            ManifestCommand::Validate { path } => {
                validate(&resolve_manifest_path(path)?, false, output)
            }
            ManifestCommand::Digest { path } => {
                validate(&resolve_manifest_path(path)?, true, output)
            }
            ManifestCommand::Compatibility {
                path,
                require_exportable,
            } => compatibility(
                &resolve_manifest_path(path)?,
                package_profile.map(Into::into),
                require_exportable,
                output,
            ),
        },
        Command::Package { command } => match command {
            PackageCommand::Export {
                manifest,
                output: archive,
                source_root,
            } => export_package(
                &manifest,
                &archive,
                package_profile.map(Into::into),
                source_root.as_deref(),
                output,
            ),
            PackageCommand::Import { input, directory } => import_package(
                &input,
                &directory,
                required_package_profile(package_profile)?,
                output,
            ),
        },
        Command::Sandbox { command } => match command {
            SandboxCommand::Plan(options) => {
                let plan = reporch_cli::local_sandbox::plan(&options.into_local()).await?;
                output.emit("sandbox plan", &plan, "Local sandbox plan is valid")
            }
            SandboxCommand::Run(options) => {
                let plan = reporch_cli::local_sandbox::plan(&options.into_local()).await?;
                let result = reporch_cli::local_sandbox::execute(&plan).await?;
                if result.exit_code == 0 {
                    output.emit("sandbox run", &result, "Local sandbox command passed")
                } else {
                    bail!("sandbox command exited with {}", result.exit_code)
                }
            }
        },
        Command::Toolchain { command } => match command {
            ToolchainCommand::List => {
                let toolchains = reporch_cli::toolchain::list()?;
                output.emit(
                    "toolchain list",
                    &toolchains,
                    &format!("{} signed toolchain(s)", toolchains.entries.len()),
                )
            }
            ToolchainCommand::Inspect { id, runtime } => {
                let inspected = reporch_cli::toolchain::inspect(&id, runtime.into_oci()).await?;
                let human = if inspected.installed {
                    format!("{} is installed", inspected.entry.id)
                } else {
                    format!("{} is not installed", inspected.entry.id)
                };
                output.emit("toolchain inspect", &inspected, &human)
            }
            ToolchainCommand::Install { id, runtime } => {
                output.progress(
                    "toolchain install",
                    &format!(
                        "Preparing signed toolchain {id}; the first install may take a few minutes"
                    ),
                );
                let installed = reporch_cli::toolchain::install(&id, runtime.into_oci()).await?;
                output.emit(
                    "toolchain install",
                    &installed,
                    &format!("Installed {}", installed.entry.id),
                )
            }
            ToolchainCommand::Prefetch { ids, runtime } => {
                let ids = if ids.is_empty() {
                    reporch_cli::toolchain::list()?
                        .entries
                        .into_iter()
                        .map(|entry| entry.id)
                        .collect::<Vec<_>>()
                } else {
                    ids
                };
                let mut installed = Vec::with_capacity(ids.len());
                for id in ids {
                    output.progress(
                        "toolchain prefetch",
                        &format!("Preparing signed toolchain {id}"),
                    );
                    installed.push(reporch_cli::toolchain::install(&id, runtime.into_oci()).await?);
                }
                output.emit(
                    "toolchain prefetch",
                    &installed,
                    &format!("Prefetched {} signed toolchain(s)", installed.len()),
                )
            }
        },
        Command::Desktop { command } => match command {
            DesktopCommand::VerifyUpdaterArtifact(options) => desktop_artifact::verify(&options),
        },
        Command::Artifact { command } => match command {
            ArtifactCommand::VerifyMinisign(options) => {
                desktop_artifact::verify_signed_artifact(&options)
            }
        },
        Command::QualificationSelfTest => qualification_self_test().await,
    }
}

async fn ensure_mandatory_runtime(command: &Command, output: &CliOutput) -> Result<()> {
    if command_skips_runtime_bootstrap(command) {
        return Ok(());
    }
    if cfg!(debug_assertions)
        && std::env::var("REPORCH_DEBUG_SKIP_RUNTIME_BOOTSTRAP")
            .ok()
            .is_some_and(|value| value == "1")
    {
        return Ok(());
    }

    reporch_runtime_host::bootstrap_packaged_seed()
        .await
        .context("import the packaged Reporch Runtime")?;
    let status = reporch_runtime_host::status().await?;
    if status.installed_version.is_none() {
        output.progress(
            "runtime bootstrap",
            "Installing the signed Reporch VM Runtime; this is required once per machine",
        );
        reporch_runtime_host::update()
            .await
            .context("complete mandatory Reporch Runtime bootstrap")?;
        return Ok(());
    }
    if matches!(
        status.availability,
        reporch_runtime_core::RuntimeAvailability::Ready
            | reporch_runtime_core::RuntimeAvailability::RemoteOnly
    ) {
        return Ok(());
    }
    if reporch_runtime_host::verify_installed().await.is_err() {
        output.progress(
            "runtime repair",
            "Repairing the signed Reporch VM Runtime while preserving projects and credentials",
        );
        reporch_runtime_host::repair()
            .await
            .context("repair mandatory Reporch Runtime assets")?;
    }
    Ok(())
}

fn runtime_status_output(
    status: &reporch_runtime_core::RuntimeStatusV1,
) -> Result<serde_json::Value> {
    let mut data = serde_json::to_value(status).context("serialize runtime status")?;
    let fields = data
        .as_object_mut()
        .context("runtime status must serialize as an object")?;
    fields.insert(
        "cli_version".into(),
        serde_json::Value::String(env!("CARGO_PKG_VERSION").into()),
    );
    fields.insert(
        "runtime_version_is_independent".into(),
        serde_json::Value::Bool(true),
    );
    fields.insert(
        "compatibility_basis".into(),
        serde_json::Value::String("protocol_version".into()),
    );
    fields.insert(
        "protocol_compatible".into(),
        serde_json::Value::Bool(status.protocol_version == reporch_runtime_core::PROTOCOL_VERSION),
    );
    Ok(data)
}

fn runtime_status_human(status: &reporch_runtime_core::RuntimeStatusV1) -> String {
    let runtime_version = status
        .installed_version
        .as_deref()
        .unwrap_or("not installed");
    let sequence = status
        .installed_sequence
        .map_or_else(|| "none".into(), |value| value.to_string());
    format!(
        "Reporch Runtime {runtime_version} · sequence {sequence} · protocol {} · {:?} · {:?}. Runtime versions are independent from CLI {}; protocol compatibility governs execution.",
        status.protocol_version,
        status.availability,
        status.backend,
        env!("CARGO_PKG_VERSION"),
    )
}

fn command_skips_runtime_bootstrap(command: &Command) -> bool {
    matches!(command, Command::Completion { .. })
        || matches!(command, Command::Auth { .. })
        || matches!(command, Command::Desktop { .. } | Command::Artifact { .. })
        || matches!(
            command,
            Command::Runtime {
                command: RuntimeCommand::Status
                    | RuntimeCommand::Doctor { .. }
                    | RuntimeCommand::Update
                    | RuntimeCommand::Repair
                    | RuntimeCommand::Reset { .. }
                    | RuntimeCommand::Qualification { .. }
            }
        )
}

fn required_package_profile(profile: Option<CompatibilityProfile>) -> Result<PackageProfile> {
    profile
        .map(Into::into)
        .context("--profile is required for package compatibility, import, and export commands")
}

fn execute_project_init(
    options: ProjectInitOptions,
    command: &str,
    output: &CliOutput,
) -> Result<()> {
    let portable = options.portable || command == "new";
    if portable {
        reporch_cli::init_portable_project_template_with_optional_id(
            &options.directory,
            &options.title,
            options.project_id,
            options.problem_type.into(),
            options.allow_non_empty,
        )?;
    } else {
        reporch_cli::init_project_template_with_optional_id(
            &options.directory,
            &options.title,
            options.project_id,
            options.problem_type.into(),
            options.allow_non_empty,
        )?;
    }
    let status = local_project_status(&options.directory)?;
    let existing_directory_note = if options.allow_non_empty {
        "\nExisting unrelated files were preserved; every generated path was collision-checked before writing."
    } else {
        ""
    };
    output.emit(
        command,
        &status,
        &format!(
            "Initialized {}{existing_directory_note}\nStarter includes: a statement, sample tests, and problem-type examples (accepted/reference and known-wrong when applicable).\nEdit the existing starter files before adding replacements.\nNext: run `reporch check`. To verify remotely, run `reporch auth login`, create/link a Studio project, `reporch project push`, then `reporch verify`.",
            status.root.display(),
        ),
    )
}

fn migrate(options: &MigrateOptions, yes: bool, output: &CliOutput) -> Result<()> {
    if !yes {
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            bail!("migration requires --yes when input is not interactive");
        }
        print!(
            "Create reporch.yaml and a create-only pre-1.0 backup in {}? [y/N] ",
            options.directory.display()
        );
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            bail!("migration cancelled");
        }
    }

    let outcome = reporch_cli::local_project_v2::migrate_project(&options.directory)?;
    let human = if outcome.migrated {
        format!("Migrated {}", outcome.directory.display())
    } else {
        format!("Already migrated: {}", outcome.directory.display())
    };
    output.emit("migrate", &outcome, &human)
}

fn emit_project_status(command: &str, output: &CliOutput) -> Result<()> {
    let status = local_project_status(Path::new("."))?;
    let human = format!(
        "{} · {} · {}",
        status.project_id,
        if status.linked {
            "linked"
        } else {
            "not linked"
        },
        if status.dirty { "changes" } else { "clean" }
    );
    output.emit(command, &status, &human)
}

fn emit_project_diff(command: &str, output: &CliOutput) -> Result<()> {
    let diff = local_project_diff(Path::new("."))?;
    let human = format!(
        "{} added, {} modified, {} removed{}",
        diff.added.len(),
        diff.modified.len(),
        diff.removed.len(),
        if diff.metadata_changed {
            ", metadata changed"
        } else {
            ""
        }
    );
    output.emit(command, &diff, &human)
}

fn check_project(output: &CliOutput) -> Result<()> {
    let root = reporch_cli::local_project::discover_project(Path::new("."))?;
    if reporch_cli::local_project_v2::is_v2_project(&root)? {
        let spec = reporch_cli::local_project_v2::read_authoring_spec(&root)?;
        let manifest =
            reporch_cli::local_project_v2::compile_authoring_spec(&root, &spec, Uuid::nil())?;
        validate_v2_static_semantics(&manifest)?;
        let digest = manifest.digest()?;
        let validator_count = usize::from(manifest.testing.validators.primary.is_some())
            + manifest.testing.validators.extra.len();
        let checker_count = usize::from(matches!(
            manifest.testing.checker.checker,
            studio_core::CheckerSpec::Custom { .. }
        ));
        let interactor_count = usize::from(manifest.execution.interactive.is_some());
        let harness_count = usize::from(manifest.execution.harness.is_some());
        let data = serde_json::json!({
            "schema": "reporch.check-result.v1",
            "authoring_schema": spec.schema,
            "release_schema": manifest.schema,
            "project_id": spec.project_id,
            "problem_type": spec.problem_type,
            "file_count": manifest.files.len(),
            "digest": digest,
            "valid": true,
            "validation_scope": "static",
            "execution_performed": false,
            "unexecuted": {
                "solutions": manifest.testing.solutions.len(),
                "validators": validator_count,
                "generators": manifest.testing.generators.len(),
                "custom_checkers": checker_count,
                "interactors": interactor_count,
                "grader_or_library_harnesses": harness_count,
            },
            "next_step": "reporch verify",
        });
        return output.emit(
            "check",
            &data,
            &format!(
                "Static check passed · {} file(s) · {digest}\nNot executed: {} solution(s), {validator_count} validator(s), {} generator(s), {checker_count} custom checker(s), {interactor_count} interactor(s), {harness_count} grader/library harness(es).\nNext: run `reporch verify` for official Studio execution evidence. Local component checks are available through `reporch validator run`, `reporch checker run`, and `reporch generator run`.",
                manifest.files.len(),
                manifest.testing.solutions.len(),
                manifest.testing.generators.len(),
            ),
        );
    }
    let spec = reporch_cli::local_project::read_authoring_spec(&root)?;
    let manifest = reporch_cli::local_project::compile_authoring_spec(&root, &spec, Uuid::nil())?;
    let digest = manifest.digest()?;
    let validator_count = usize::from(manifest.judging.validator_path.is_some())
        + manifest.judging.extra_validator_paths.len()
        + manifest.judging.extra_validators.len();
    let checker_count = usize::from(matches!(
        manifest.judging.checker,
        studio_core::CheckerSpec::Custom { .. }
    ));
    let interactor_count = usize::from(manifest.judging.interactor_path.is_some());
    let grader_count = usize::from(manifest.judging.grader_path.is_some());
    let data = serde_json::json!({
        "schema": "reporch.check-result.v1",
        "project_id": spec.project_id,
        "problem_type": spec.problem_type,
        "file_count": manifest.files.len(),
        "digest": digest,
        "valid": true,
        "validation_scope": "static",
        "execution_performed": false,
        "unexecuted": {
            "solutions": manifest.solutions.len(),
            "validators": validator_count,
            "generators": manifest.judging.generators.len(),
            "custom_checkers": checker_count,
            "interactors": interactor_count,
            "graders": grader_count,
        },
        "next_step": "reporch verify",
    });
    output.emit(
        "check",
        &data,
        &format!(
            "Static check passed · {} file(s) · {digest}\nNot executed: {} solution(s), {validator_count} validator(s), {} generator(s), {checker_count} custom checker(s), {interactor_count} interactor(s), {grader_count} grader(s).\nNext: run `reporch verify` for official Studio execution evidence. Local component checks are available through `reporch validator run`, `reporch checker run`, and `reporch generator run`.",
            manifest.files.len(),
            manifest.solutions.len(),
            manifest.judging.generators.len(),
        ),
    )
}

fn validate_v2_static_semantics(manifest: &studio_core::ReleaseManifestV2) -> Result<()> {
    if manifest.problem_type == ProblemType::Scored {
        for group in &manifest.testing.groups {
            ensure!(
                group.points.is_finite() && (0.0..=100.0).contains(&group.points),
                "scored group `{}` points must be a finite value from 0 to 100, got {}",
                group.name,
                group.points
            );
        }
        let total = manifest
            .testing
            .groups
            .iter()
            .map(|group| group.points)
            .sum::<f64>();
        ensure!(
            (total - 100.0).abs() <= 1e-9,
            "scored problem group points must total 100, got {total}; inspect with `reporch test group list` and remove or update starter groups"
        );

        let assigned_groups = manifest
            .testing
            .tests
            .iter()
            .flat_map(|test| test.group_ids.iter().copied())
            .chain(
                manifest
                    .testing
                    .generators
                    .iter()
                    .flat_map(|generator| generator.recipes.iter())
                    .flat_map(|recipe| recipe.group_ids.iter().copied()),
            )
            .collect::<std::collections::BTreeSet<_>>();
        for group in &manifest.testing.groups {
            ensure!(
                assigned_groups.contains(&group.id),
                "scored group `{}` has no manual tests or generator recipes; assign one before verification",
                group.name
            );
        }
    }

    let mut reference_count = 0_usize;
    for solution in &manifest.testing.solutions {
        if solution.role == studio_core::SolutionRoleV2::Reference {
            reference_count += 1;
        }
        ensure!(
            !matches!(
                solution.role,
                studio_core::SolutionRoleV2::Reference | studio_core::SolutionRoleV2::Oracle
            ) || solution.expected_verdict == studio_core::ExpectedVerdict::Accepted,
            "solution `{}` is a reference/oracle but its expected verdict is not accepted",
            solution.program.name
        );
        ensure!(
            solution.role != studio_core::SolutionRoleV2::KnownWrong
                || solution.expected_verdict != studio_core::ExpectedVerdict::Accepted,
            "solution `{}` is known-wrong but its expected verdict is accepted",
            solution.program.name
        );
    }
    ensure!(
        reference_count <= 1,
        "exactly one solution may have role reference; found {reference_count}"
    );
    if manifest.problem_type != ProblemType::OutputOnly {
        ensure!(
            reference_count == 1,
            "one accepted reference solution is required; set it with `reporch solution update <name> --role reference --expected accepted`"
        );
    }
    Ok(())
}

fn local_project_status(directory: &Path) -> Result<reporch_cli::local_project::ProjectStatusV1> {
    if reporch_cli::local_project_v2::is_v2_project(directory)? {
        reporch_cli::local_project_v2::project_status(directory)
    } else {
        reporch_cli::local_project::project_status(directory)
    }
}

fn local_project_diff(directory: &Path) -> Result<reporch_cli::local_project::ProjectDiffV1> {
    if reporch_cli::local_project_v2::is_v2_project(directory)? {
        reporch_cli::local_project_v2::project_diff(directory)
    } else {
        reporch_cli::local_project::project_diff(directory)
    }
}

fn link_local_project(
    directory: &Path,
    api_url: &str,
    project_id: Uuid,
) -> Result<reporch_cli::local_project::ProjectStatusV1> {
    if reporch_cli::local_project_v2::is_v2_project(directory)? {
        reporch_cli::local_project_v2::link_project(directory, api_url, project_id)
    } else {
        reporch_cli::local_project::link_project(directory, api_url, project_id)
    }
}

fn generate_completion(shell: CompletionShell, output: &CliOutput) -> Result<()> {
    output.ensure_human_format("completion")?;
    std::thread::Builder::new()
        .name("reporch-completion".into())
        .stack_size(CLI_STACK_BYTES)
        .spawn(move || {
            let mut command = Args::command();
            clap_complete::generate(
                clap_complete::Shell::from(shell),
                &mut command,
                "reporch",
                &mut std::io::stdout(),
            );
        })
        .context("spawn completion generator")?
        .join()
        .map_err(|_| anyhow::anyhow!("completion generator panicked"))?;
    Ok(())
}

fn raw_arguments_request_json() -> bool {
    let arguments: Vec<_> = std::env::args_os().collect();
    arguments.iter().enumerate().any(|(index, argument)| {
        argument == "--json"
            || argument == "--format=json"
            || (argument == "--format"
                && arguments
                    .get(index + 1)
                    .is_some_and(|value| value == "json" || value == "jsonl"))
    })
}

fn confirm_publication(yes: bool, no_input: bool) -> Result<()> {
    if yes {
        return Ok(());
    }
    ensure!(
        !no_input && std::io::stdin().is_terminal() && std::io::stderr().is_terminal(),
        "publication requires --yes when input is disabled or no terminal is attached"
    );
    eprint!("Publish this immutable release to Reporch? [y/N] ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    ensure!(
        matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes"),
        "publication cancelled"
    );
    Ok(())
}

fn command_name(command: &Command) -> &'static str {
    match command {
        Command::New(_) => "new",
        Command::Migrate(_) => "migrate",
        Command::Check => "check",
        Command::Status => "status",
        Command::Diff => "diff",
        Command::Statement(_) => "statement",
        Command::Test(_) => "test",
        Command::Generator(_) => "generator",
        Command::Validator(_) => "validator",
        Command::Checker(_) => "checker",
        Command::Solution(_) => "solution",
        Command::Interactor(_) => "interactor",
        Command::Grader(_) => "grader",
        Command::Output(_) => "output",
        Command::Verify(_) => "verify",
        Command::Submit(_) => "submit",
        Command::Publish(_) => "publish",
        Command::Auth { command } => match command {
            AuthCommand::Login(_) => "auth login",
            AuthCommand::Status(_) => "auth status",
            AuthCommand::Logout(_) => "auth logout",
        },
        Command::Project { command } => match command {
            ProjectCommand::Init(_) => "project init",
            ProjectCommand::Link { .. } => "project link",
            ProjectCommand::List(_) => "project list",
            ProjectCommand::Show(_) => "project show",
            ProjectCommand::Open { .. } => "project open",
            ProjectCommand::Status => "project status",
            ProjectCommand::Diff => "project diff",
            ProjectCommand::Create(_) => "project create",
            ProjectCommand::Pull(_) => "project pull",
            ProjectCommand::Push(_) => "project push",
            ProjectCommand::Validate(_) => "project validate",
            ProjectCommand::Package(_) => "project package",
        },
        Command::Member { command } => match command {
            MemberCommand::Search(_) => "member search",
            MemberCommand::List(_) => "member list",
            MemberCommand::Add(_) => "member add",
            MemberCommand::Update(_) => "member update",
            MemberCommand::Remove(_) => "member remove",
        },
        Command::Doctor(_) => "doctor",
        Command::Runtime { command } => match command {
            RuntimeCommand::Status => "runtime status",
            RuntimeCommand::Doctor { .. } => "runtime doctor",
            RuntimeCommand::Update => "runtime update",
            RuntimeCommand::Repair => "runtime repair",
            RuntimeCommand::Reset { .. } => "runtime reset",
            RuntimeCommand::Qualification { .. } => "runtime qualification",
        },
        Command::Completion { .. } => "completion",
        Command::Quota { .. } => "quota show",
        Command::Release { command } => match command {
            ReleaseCommand::Build(_) => "release build",
            ReleaseCommand::List(_) => "release list",
            ReleaseCommand::Show(_) => "release show",
            ReleaseCommand::Download(_) => "release download",
        },
        Command::Publication { command } => match command {
            PublicationCommand::Publish(_) => "publication publish",
            PublicationCommand::Status(_) => "publication status",
        },
        Command::Validation { command } => match command {
            ValidationCommand::List(_) => "validation list",
            ValidationCommand::Show(_) => "validation show",
            ValidationCommand::Watch(_) => "validation watch",
        },
        Command::Events { .. } => "events watch",
        Command::Waiver { command } => match command {
            WaiverCommand::List(_) => "waiver list",
            WaiverCommand::Create(_) => "waiver create",
            WaiverCommand::Revoke(_) => "waiver revoke",
        },
        Command::Revision { command } => match command {
            RevisionCommand::List(_) => "revision list",
            RevisionCommand::Show(_) => "revision show",
            RevisionCommand::Diff(_) => "revision diff",
            RevisionCommand::Restore(_) => "revision restore",
        },
        Command::Review { command } => match command {
            ReviewCommand::Submit(_) => "review submit",
            ReviewCommand::List(_) => "review list",
            ReviewCommand::Request(_) => "review request",
            ReviewCommand::Inbox(_) => "review inbox",
            ReviewCommand::Show(_) => "review show",
            ReviewCommand::Status(_) => "review status",
            ReviewCommand::Claim(_) => "review claim",
            ReviewCommand::Cancel(_) => "review cancel",
            ReviewCommand::Approve(_) => "review approve",
            ReviewCommand::RequestChanges(_) => "review request-changes",
        },
        Command::Manifest { .. } => "manifest",
        Command::Answer(_) => "answer",
        Command::Stress(_) => "stress",
        Command::Package { .. } => "package",
        Command::Sandbox { .. } => "sandbox",
        Command::Toolchain { command } => match command {
            ToolchainCommand::List => "toolchain list",
            ToolchainCommand::Inspect { .. } => "toolchain inspect",
            ToolchainCommand::Install { .. } => "toolchain install",
            ToolchainCommand::Prefetch { .. } => "toolchain prefetch",
        },
        Command::Desktop { .. } => "desktop",
        Command::Artifact { .. } => "artifact",
        Command::QualificationSelfTest => "qualification-self-test",
    }
}

async fn submit_project(options: SubmitOptions, output: &CliOutput) -> Result<()> {
    let root = reporch_cli::local_project::discover_project(Path::new("."))?;
    let checked_file_count = if reporch_cli::local_project_v2::is_v2_project(&root)? {
        let spec = reporch_cli::local_project_v2::read_authoring_spec(&root)?;
        reporch_cli::local_project_v2::compile_authoring_spec(&root, &spec, Uuid::nil())?
            .files
            .len()
    } else {
        let spec = reporch_cli::local_project::read_authoring_spec(&root)?;
        reporch_cli::local_project::compile_authoring_spec(&root, &spec, Uuid::nil())?
            .files
            .len()
    };
    let push = studio_remote::push_operation(&studio_remote::PushOptions {
        connection: options.connection.clone(),
        manifest: None,
        source_root: None,
        message: options.message,
        timeout_seconds: options.timeout_seconds.min(10 * 60),
    })
    .await?;
    let validation = studio_remote::validate_operation(&studio_remote::ValidateOptions {
        connection: options.connection.clone(),
        project_id: Some(push.commit.project_id),
        commit_id: Some(push.commit.id),
        idempotency_key: Some(format!("validation-{}", push.commit.id)),
        wait: true,
        timeout_seconds: options.timeout_seconds,
    })
    .await?;
    let detail = validation
        .detail
        .as_ref()
        .context("Studio validation did not return evidence")?;
    ensure!(
        detail.status == studio_contracts::ValidationRunStatus::Passed,
        "Studio validation did not pass"
    );
    // submit passes explicit IDs to the shared validation operation, so that
    // operation cannot infer which local checkout should receive the durable
    // resume pointer. Persist it here before the review request; a transient
    // review failure must not make the successful official evidence disappear
    // from local state.
    let mut state = reporch_cli::local_project::read_local_state(&root)?;
    state.last_validation_run_id = Some(detail.id);
    reporch_cli::local_project::write_local_state(&root, &state)?;
    let review = studio_remote::submit_review_operation(&studio_remote::SubmitReviewOptions {
        connection: options.connection,
        project_id: Some(push.commit.project_id),
        commit_id: Some(push.commit.id),
        validation_run_id: Some(detail.id),
        idempotency_key: Some(format!("review-{}", push.commit.id)),
    })
    .await?;
    let human = format!("Submitted review {}", review.id);
    let result = serde_json::json!({
        "schema": "reporch.submit-result.v1",
        "check": {
            "file_count": checked_file_count,
            "valid": true,
        },
        "push": push,
        "validation": validation,
        "review": review,
    });
    output.emit("submit", &result, &human)
}

async fn qualification_self_test() -> Result<()> {
    qualification_keyring_canary()
        .await
        .context("OS credential-store canary")?;

    let temporary = tempfile::tempdir().context("create qualification fixture directory")?;
    let (validated_problem_types, package_profiles, compatibility_report_count) =
        qualification_authoring_matrix(temporary.path())?;

    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "schema": "reporch.cli-installed-self-test.v1",
            "version": env!("CARGO_PKG_VERSION"),
            "target_os": std::env::consts::OS,
            "target_arch": std::env::consts::ARCH,
            "credential_store_round_trip": true,
            "generated_manifest_valid": true,
            "generated_authoring_spec_valid": true,
            "problem_types": validated_problem_types,
            "problem_type_count": validated_problem_types.len(),
            "package_profiles": package_profiles,
            "package_profile_count": package_profiles.len(),
            "compatibility_report_count": compatibility_report_count,
            "passed": true,
        }))?
    );
    Ok(())
}

fn qualification_authoring_matrix(
    root: &Path,
) -> Result<(Vec<ProblemType>, Vec<PackageProfile>, usize)> {
    let problem_types = [
        ("standard", ProblemType::Standard),
        ("scored", ProblemType::Scored),
        ("interactive", ProblemType::Interactive),
        ("output-only", ProblemType::OutputOnly),
        ("library", ProblemType::Library),
        ("grader", ProblemType::Grader),
    ];
    let package_profiles = [
        PackageProfile::ReporchNative,
        PackageProfile::Icpc202509,
        PackageProfile::IcpcLegacy,
        PackageProfile::PolygonCompatible,
        PackageProfile::DomjudgeZip,
    ];
    let mut validated_problem_types = Vec::with_capacity(problem_types.len());
    let mut compatibility_report_count = 0_usize;

    for (directory_name, problem_type) in problem_types {
        let project_directory = root.join(directory_name);
        let project_id = Uuid::now_v7();
        reporch_cli::init_project_template(
            &project_directory,
            &format!("Reporch CLI {directory_name} qualification"),
            project_id,
            problem_type,
        )?;
        let manifest_bytes = fs::read(project_directory.join("reporch.problem.json"))?;
        let manifest: VersionedReleaseManifest = serde_json::from_slice(&manifest_bytes)?;
        manifest.validate_references()?;
        let authoring = reporch_cli::local_project_v2::read_authoring_spec(&project_directory)?;
        ensure!(
            authoring.project_id == project_id && authoring.problem_type == problem_type,
            "generated AuthoringSpecV2 changed the requested problem identity or type"
        );
        let VersionedReleaseManifest::V2(manifest) = manifest else {
            bail!("new project template did not generate ReleaseManifestV2")
        };
        ensure!(
            manifest.project_id == project_id && manifest.problem_type == problem_type,
            "generated ReleaseManifestV2 changed the requested problem identity or type"
        );
        for profile in package_profiles {
            let report = versioned_package::compatibility_v2(&manifest, profile)?;
            ensure!(
                report.schema == studio_core::COMPATIBILITY_REPORT_SCHEMA_V1
                    && report.target_profile == profile,
                "package compatibility report was not bound to the requested profile"
            );
            compatibility_report_count += 1;
        }
        validated_problem_types.push(problem_type);
    }
    Ok((
        validated_problem_types,
        package_profiles.to_vec(),
        compatibility_report_count,
    ))
}

async fn auth_login(options: &NativeAuthOptions, no_input: bool, output: &CliOutput) -> Result<()> {
    ensure!(
        !no_input,
        "interactive input is disabled for this command; rerun `reporch auth login` without --no-input"
    );
    let config = device_auth_config(options)?;
    let client = NativeAuthClient::discover(config)
        .await
        .context("discover the Reporch identity provider")?;
    let prompt = client
        .request_device_authorization()
        .await
        .context("start device authorization")?;

    eprintln!("Open this URL to sign in: {}", prompt.verification_uri);
    eprintln!("Enter code: {}", prompt.user_code);
    eprintln!("The code expires at {}.", prompt.expires_at.to_rfc3339());
    let browser_url = prompt
        .verification_uri_complete
        .clone()
        .unwrap_or_else(|| prompt.verification_uri.clone());
    let browser_opened = tokio::task::spawn_blocking(move || open::that(browser_url))
        .await
        .is_ok_and(|result| result.is_ok());
    if !browser_opened {
        eprintln!("The system browser could not be opened; use the URL above.");
    }

    let status = client
        .finish_device_authorization(&prompt, &KeyringTokenStore)
        .await
        .context("finish device authorization")?;
    output.emit("auth login", &status, "Signed in to Reporch")
}

async fn auth_status(options: &NativeAuthOptions, output: &CliOutput) -> Result<()> {
    let config = device_auth_config(options)?;
    let status = config
        .local_session_status(&KeyringTokenStore)
        .await
        .context("read the OS credential store")?;
    let human = if status.authenticated {
        "Signed in"
    } else {
        "Not signed in"
    };
    output.emit("auth status", &status, human)
}

async fn auth_logout(
    options: &NativeAuthOptions,
    profile: Option<&str>,
    output: &CliOutput,
) -> Result<()> {
    if let Err(error) =
        reporch_cli::remote_consent::clear_for_auth(&options.issuer, &options.client_id, profile)
    {
        eprintln!("Could not clear stored remote fallback consent: {error}");
    }
    let config = device_auth_config(options)?;
    let remote_result = match NativeAuthClient::discover(config.clone()).await {
        Ok(client) => client.logout(&KeyringTokenStore).await,
        Err(error) => {
            eprintln!("Remote revocation is unavailable: {error}");
            config.clear_local_session(&KeyringTokenStore).await?;
            let result = serde_json::json!({
                "schema": "reporch.auth-logout-result.v1",
                "local_removed": true,
                "remote_revoked": false,
            });
            return output.emit("auth logout", &result, "Local Studio credential removed");
        }
    };
    let remote_revoked = match remote_result {
        Ok(value) => value,
        Err(error) => {
            config.clear_local_session(&KeyringTokenStore).await?;
            eprintln!("Remote revocation failed: {error}");
            false
        }
    };
    let result = serde_json::json!({
        "schema": "reporch.auth-logout-result.v1",
        "local_removed": true,
        "remote_revoked": remote_revoked,
    });
    output.emit("auth logout", &result, "Signed out")
}

fn import_package(
    input: &Path,
    directory: &Path,
    profile: PackageProfile,
    output: &CliOutput,
) -> Result<()> {
    if directory.exists() {
        return Err(cli_output::detailed_error(
            "import destination already exists. Choose a new empty directory; Reporch never merges or overwrites an import",
            serde_json::json!({
                "schema": "reporch.package-import-destination-conflict.v1",
                "directory": directory,
                "recovery": "choose_new_empty_directory",
            }),
        ));
    }
    let manifest: VersionedReleaseManifest = if profile != PackageProfile::ReporchNative
        && versioned_package::contains_v2_sidecar(input)?
    {
        versioned_package::import_v2_sidecar(input, directory, profile)?
    } else {
        match profile {
            PackageProfile::Icpc202509 => {
                icpc_import::import_icpc_2025_09(input, directory)?.into()
            }
            PackageProfile::IcpcLegacy => icpc_legacy::import_icpc_legacy(input, directory)?.into(),
            PackageProfile::DomjudgeZip => {
                icpc_import::import_domjudge_zip(input, directory)?.into()
            }
            PackageProfile::PolygonCompatible => {
                polygon_import::import_polygon_package(input, directory)?.into()
            }
            PackageProfile::ReporchNative => {
                native_package::import_native_versioned(input, directory)?
            }
        }
    };
    let mut cleanup = ImportCleanup::new(directory);
    match &manifest {
        VersionedReleaseManifest::V1(manifest) => {
            reporch_cli::local_project::write_authoring_spec_create_new(
                directory,
                &reporch_format::AuthoringSpecV1::from_manifest(manifest),
            )?;
        }
        VersionedReleaseManifest::V2(manifest) => {
            reporch_cli::local_project_v2::write_authoring_spec_create_new(
                directory,
                &reporch_format::AuthoringSpecV2::from_manifest(manifest),
            )?;
        }
    }
    cleanup.finish();
    let data = serde_json::json!({
        "schema": "reporch.package-import-result.v1",
        "profile": profile,
        "input": input,
        "directory": directory,
        "project_id": manifest.project_id(),
        "manifest_digest": manifest.digest()?,
    });
    output.emit(
        "package import",
        &data,
        &format!("Imported package into {}", directory.display()),
    )
}

struct ImportCleanup {
    path: PathBuf,
    armed: bool,
}

impl ImportCleanup {
    fn new(path: &Path) -> Self {
        Self {
            path: path.to_owned(),
            armed: true,
        }
    }

    fn finish(&mut self) {
        self.armed = false;
    }
}

impl Drop for ImportCleanup {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

fn export_package(
    manifest_path: &Path,
    output: &Path,
    profile: Option<PackageProfile>,
    source_root: Option<&Path>,
    cli_output: &CliOutput,
) -> Result<()> {
    let manifest = read_versioned_manifest(manifest_path)?;
    let profile = profile.unwrap_or_else(|| manifest.package_profile());
    let manifest_digest = manifest.digest()?;
    if output.exists() {
        return Err(cli_output::detailed_error(
            format!(
                "export destination already exists and may be stale; current manifest digest is {manifest_digest}. Choose a new output path. Reporch never overwrites packages"
            ),
            serde_json::json!({
                "schema": "reporch.package-destination-conflict.v1",
                "output": output,
                "current_manifest_digest": manifest_digest,
                "recovery": "choose_new_output_path",
            }),
        ));
    }
    let source_root = source_root
        .map(Path::to_path_buf)
        .or_else(|| {
            manifest_path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .map(Path::to_path_buf)
        })
        .unwrap_or_else(|| PathBuf::from("."));
    let preflight_report = match (&manifest, profile) {
        (_, PackageProfile::ReporchNative) => None,
        (VersionedReleaseManifest::V1(manifest), profile) => {
            Some(compatibility_report(manifest, profile))
        }
        (VersionedReleaseManifest::V2(manifest), profile) => {
            Some(versioned_package::compatibility_v2(manifest, profile)?)
        }
    };
    if let Some(report) = &preflight_report
        && !report.exportable
    {
        return Err(cli_output::detailed_error(
            format!(
                "package cannot be exported to {}; inspect `details` for every blocking capability",
                package_profile_name(profile)
            ),
            report,
        ));
    }
    let report = match (&manifest, profile) {
        (_, PackageProfile::ReporchNative) => {
            native_package::export_native_versioned(&manifest, &source_root, output)?;
            None
        }
        (VersionedReleaseManifest::V1(manifest), PackageProfile::Icpc202509) => {
            icpc_export::export_icpc_2025_09(manifest, &source_root, output)?;
            Some(compatibility_report(manifest, profile))
        }
        (VersionedReleaseManifest::V1(manifest), PackageProfile::IcpcLegacy) => {
            icpc_legacy::export_icpc_legacy(manifest, &source_root, output)?;
            Some(compatibility_report(manifest, profile))
        }
        (VersionedReleaseManifest::V1(manifest), PackageProfile::DomjudgeZip) => {
            icpc_export::export_domjudge_zip(manifest, &source_root, output)?;
            Some(compatibility_report(manifest, profile))
        }
        (VersionedReleaseManifest::V1(manifest), PackageProfile::PolygonCompatible) => {
            polygon_export::export_polygon_package(manifest, &source_root, output)?;
            Some(compatibility_report(manifest, profile))
        }
        (VersionedReleaseManifest::V2(manifest), target) => {
            versioned_package::export_v2_with_sidecar(
                manifest,
                &source_root,
                output,
                target,
                |projection, root, destination| match target {
                    PackageProfile::Icpc202509 => {
                        icpc_export::export_icpc_2025_09(projection, root, destination)
                    }
                    PackageProfile::IcpcLegacy => {
                        icpc_legacy::export_icpc_legacy(projection, root, destination)
                    }
                    PackageProfile::DomjudgeZip => {
                        icpc_export::export_domjudge_zip(projection, root, destination)
                    }
                    PackageProfile::PolygonCompatible => {
                        polygon_export::export_polygon_package(projection, root, destination)
                    }
                    PackageProfile::ReporchNative => unreachable!(),
                },
            )?;
            preflight_report
        }
    };
    let data = serde_json::json!({
        "schema": "reporch.package-export-result.v1",
        "profile": profile,
        "output": output,
        "manifest_digest": manifest_digest,
        "compatibility": report,
    });
    cli_output.emit(
        "package export",
        &data,
        &format!("Exported package to {}", output.display()),
    )
}

fn package_profile_name(profile: PackageProfile) -> &'static str {
    match profile {
        PackageProfile::ReporchNative => "reporch-native",
        PackageProfile::Icpc202509 => "icpc202509",
        PackageProfile::IcpcLegacy => "icpc-legacy",
        PackageProfile::PolygonCompatible => "polygon-compatible",
        PackageProfile::DomjudgeZip => "domjudge-zip",
    }
}

fn compatibility(
    path: &Path,
    profile: Option<PackageProfile>,
    require_exportable: bool,
    output: &CliOutput,
) -> Result<()> {
    let manifest = read_versioned_manifest(path)?;
    let profile = profile.unwrap_or_else(|| manifest.package_profile());
    let report = match &manifest {
        VersionedReleaseManifest::V1(manifest) => compatibility_report(manifest, profile),
        VersionedReleaseManifest::V2(manifest) => {
            versioned_package::compatibility_v2(manifest, profile)?
        }
    };
    if require_exportable && !report.exportable {
        return Err(cli_output::detailed_error(
            "manifest cannot be exported to the requested profile; inspect `details` for every blocking capability",
            &report,
        ));
    }
    output.emit(
        "manifest compatibility",
        &report,
        if report.exportable {
            "Manifest is exportable"
        } else {
            "Manifest has compatibility losses"
        },
    )
}

fn read_versioned_manifest(path: &Path) -> Result<VersionedReleaseManifest> {
    let bytes = reporch_cli::local_project::read_bounded_regular_file(path, 16 * 1024 * 1024)?;
    if let Ok(manifest) = serde_json::from_slice(&bytes) {
        return Ok(manifest);
    }

    let source_root = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    match reporch_format::parse_versioned_authoring_spec(&bytes).with_context(|| {
        format!(
            "parse {} as an immutable manifest or authoring spec",
            path.display()
        )
    })? {
        reporch_format::VersionedAuthoringSpec::V1(spec) => Ok(
            reporch_cli::local_project::compile_authoring_spec(source_root, &spec, Uuid::nil())?
                .into(),
        ),
        reporch_format::VersionedAuthoringSpec::V2(spec) => Ok(
            reporch_cli::local_project_v2::compile_authoring_spec(source_root, &spec, Uuid::nil())?
                .into(),
        ),
    }
}

#[cfg(test)]
fn read_manifest(path: &Path) -> Result<ReleaseManifestV1> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
}

fn resolve_manifest_path(path: Option<PathBuf>) -> Result<PathBuf> {
    match path {
        Some(path) => Ok(path),
        None => Ok(
            reporch_cli::local_project::discover_project(Path::new("."))?
                .join(reporch_cli::local_project::AUTHORING_FILE_NAME),
        ),
    }
}

fn validate(path: &Path, print_digest: bool, output: &CliOutput) -> Result<()> {
    let manifest = read_versioned_manifest(path)?;
    manifest.validate_references()?;
    let issues = match &manifest {
        VersionedReleaseManifest::V1(manifest) => validate_manifest(manifest),
        VersionedReleaseManifest::V2(_) => Vec::new(),
    };
    if !issues.is_empty() {
        bail!(
            "manifest validation failed with {} issue(s): {}",
            issues.len(),
            serde_json::to_string(&issues)?
        );
    }
    local_manifest::verify_versioned_files(path, &manifest)?;
    let digest = manifest.digest()?;
    let data = serde_json::json!({
        "schema": "reporch.manifest-validation-result.v1",
        "path": path,
        "digest": digest,
        "valid": true,
    });
    let human = if print_digest {
        digest.to_string()
    } else {
        format!("Manifest is valid: {digest}")
    };
    output.emit(
        if print_digest {
            "manifest digest"
        } else {
            "manifest validate"
        },
        &data,
        &human,
    )
}

#[cfg(test)]
pub(crate) fn init_project(directory: &Path, title: &str) -> Result<()> {
    reporch_cli::init_legacy_v1_project_template(
        directory,
        title,
        Uuid::now_v7(),
        studio_core::ProblemType::Standard,
    )
}

#[cfg(test)]
mod interrupt_regression_tests {
    // QA regression: CLI-090-004.
    // Generated by Codex on 2026-08-14 after SIGINT incorrectly returned success.

    use super::*;
    use std::future::{pending, ready};

    #[tokio::test]
    async fn sigint_cancels_the_active_command_with_the_internal_marker() {
        let result =
            run_until_interrupt(pending::<Result<()>>(), ready(std::io::Result::Ok(()))).await;
        assert!(
            result
                .unwrap_err()
                .downcast_ref::<CliInterrupted>()
                .is_some()
        );
    }

    #[tokio::test]
    async fn a_completed_command_is_not_reclassified_as_interrupted() {
        let result =
            run_until_interrupt(ready(Result::Ok(())), pending::<std::io::Result<()>>()).await;
        assert!(result.is_ok());
    }

    #[test]
    fn the_internal_interrupt_marker_takes_the_public_exit_130_path() {
        let error = anyhow::Error::new(CliInterrupted);
        let exit_code = if error.downcast_ref::<CliInterrupted>().is_some() {
            130
        } else {
            0
        };
        assert_eq!(exit_code, 130);
    }
}

#[cfg(test)]
mod qualification_regression_tests {
    use super::*;

    #[test]
    fn installed_self_test_matrix_really_compiles_every_type_and_profile() {
        let temporary = tempfile::tempdir().unwrap();
        let (problem_types, profiles, report_count) =
            qualification_authoring_matrix(temporary.path()).unwrap();
        assert_eq!(
            problem_types,
            vec![
                ProblemType::Standard,
                ProblemType::Scored,
                ProblemType::Interactive,
                ProblemType::OutputOnly,
                ProblemType::Library,
                ProblemType::Grader,
            ]
        );
        assert_eq!(
            profiles,
            vec![
                PackageProfile::ReporchNative,
                PackageProfile::Icpc202509,
                PackageProfile::IcpcLegacy,
                PackageProfile::PolygonCompatible,
                PackageProfile::DomjudgeZip,
            ]
        );
        assert_eq!(report_count, problem_types.len() * profiles.len());
    }
}

#[cfg(test)]
mod runtime_bootstrap_regression_tests {
    use super::*;

    #[test]
    fn signature_verification_can_bootstrap_the_first_runtime_channel() {
        let arguments = Args::try_parse_from([
            "reporch",
            "artifact",
            "verify-minisign",
            "--artifact",
            "runtime-manifest.json",
            "--signature",
            "c2lnbmF0dXJl",
            "--public-key",
            "cHVibGljLWtleQ==",
        ])
        .unwrap();
        assert!(command_skips_runtime_bootstrap(&arguments.command));
        assert!(!command_skips_runtime_bootstrap(&Command::Check));
    }
}
