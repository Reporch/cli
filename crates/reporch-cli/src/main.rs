#![forbid(unsafe_code)]

mod desktop_artifact;
mod icpc_export;
mod icpc_import;
mod icpc_legacy;
mod icpc_submit_answer;
mod local_manifest;
mod native_package;
mod polygon_export;
mod polygon_import;
mod statement_tex;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args as ClapArgs, Parser, Subcommand, ValueEnum};
use reporch_cli::local_sandbox::{LocalSandboxOptions, OciRuntime};
use reporch_cli::studio_remote;
use reporch_cli::{NativeAuthOptions, device_auth_config};
use studio_core::{
    CheckerSpec, ExpectedVerdict, JudgingSpec, ManifestFile, PackageProfile, ProblemType,
    PublicationSampleV1, PublicationSpecV1, RELEASE_MANIFEST_SCHEMA_V1, ReleaseManifestV1,
    ResourceLimits, SolutionSpec, StatementSectionsV1, TestCaseSpec, compatibility_report,
    validate_manifest,
};
use studio_native_auth::qualification_keyring_canary;
use studio_native_auth::{KeyringTokenStore, NativeAuthClient};
use uuid::Uuid;

#[derive(Debug, Parser)]
#[command(
    name = "reporch",
    version,
    about = "Create, validate, package, and sync Reporch problems"
)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Authenticate this native client with Reporch without storing tokens in files.
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    Project {
        #[command(subcommand)]
        command: ProjectCommand,
    },
    Manifest {
        #[command(subcommand)]
        command: ManifestCommand,
    },
    Package {
        #[command(subcommand)]
        command: PackageCommand,
    },
    /// Plan or run an opt-in, rootless, networkless OCI command.
    Sandbox {
        #[command(subcommand)]
        command: SandboxCommand,
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

#[derive(Debug, Subcommand)]
enum DesktopCommand {
    /// Stream and verify one Tauri updater artifact with the production trust root.
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
    Podman,
    Docker,
}

impl SandboxOptions {
    fn into_local(self) -> LocalSandboxOptions {
        LocalSandboxOptions {
            runtime: match self.runtime {
                SandboxRuntime::Auto => OciRuntime::Auto,
                SandboxRuntime::Podman => OciRuntime::Podman,
                SandboxRuntime::Docker => OciRuntime::Docker,
            },
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
enum PackageCommand {
    Export {
        manifest: PathBuf,
        output: PathBuf,
        #[arg(long, value_enum)]
        profile: CompatibilityProfile,
        #[arg(long)]
        source_root: Option<PathBuf>,
    },
    Import {
        input: PathBuf,
        directory: PathBuf,
        #[arg(long, value_enum)]
        profile: CompatibilityProfile,
    },
}

#[derive(Debug, Subcommand)]
enum ProjectCommand {
    Init {
        #[arg(long)]
        title: String,
        #[arg(long, default_value = ".")]
        directory: PathBuf,
        #[arg(long, value_enum, default_value_t = studio_remote::RemoteProblemType::Standard)]
        problem_type: studio_remote::RemoteProblemType,
        /// Bind the local manifest to an existing private Studio project.
        #[arg(long)]
        project_id: Option<Uuid>,
    },
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
enum ManifestCommand {
    Validate {
        path: PathBuf,
    },
    Digest {
        path: PathBuf,
    },
    Compatibility {
        path: PathBuf,
        #[arg(long, value_enum)]
        profile: CompatibilityProfile,
        #[arg(long)]
        strict: bool,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CompatibilityProfile {
    ReporchNative,
    Icpc202509,
    IcpcLegacy,
    PolygonCompatible,
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

#[tokio::main]
async fn main() -> Result<()> {
    match Args::parse().command {
        Command::Auth { command } => match command {
            AuthCommand::Login(options) => auth_login(&options).await,
            AuthCommand::Status(options) => auth_status(&options).await,
            AuthCommand::Logout(options) => auth_logout(&options).await,
        },
        Command::Project { command } => match command {
            ProjectCommand::Init {
                title,
                directory,
                problem_type,
                project_id,
            } => {
                let project_id = project_id.unwrap_or_else(Uuid::now_v7);
                if matches!(problem_type, studio_remote::RemoteProblemType::Standard) {
                    init_project_with_id(&directory, &title, project_id)
                } else {
                    reporch_cli::init_project_template(
                        &directory,
                        &title,
                        project_id,
                        problem_type.into(),
                    )?;
                    println!("initialized {}", directory.canonicalize()?.display());
                    Ok(())
                }
            }
            ProjectCommand::Create(options) => studio_remote::create(&options).await,
            ProjectCommand::Pull(options) => studio_remote::pull(&options).await,
            ProjectCommand::Push(options) => studio_remote::push(&options).await,
            ProjectCommand::Validate(options) => studio_remote::validate(&options).await,
            ProjectCommand::Package(options) => studio_remote::package(&options).await,
        },
        Command::Manifest { command } => match command {
            ManifestCommand::Validate { path } => validate(&path, false),
            ManifestCommand::Digest { path } => validate(&path, true),
            ManifestCommand::Compatibility {
                path,
                profile,
                strict,
            } => compatibility(&path, profile.into(), strict),
        },
        Command::Package { command } => match command {
            PackageCommand::Export {
                manifest,
                output,
                profile,
                source_root,
            } => export_package(&manifest, &output, profile.into(), source_root.as_deref()),
            PackageCommand::Import {
                input,
                directory,
                profile,
            } => import_package(&input, &directory, profile.into()),
        },
        Command::Sandbox { command } => match command {
            SandboxCommand::Plan(options) => {
                let plan = reporch_cli::local_sandbox::plan(&options.into_local()).await?;
                println!("{}", serde_json::to_string_pretty(&plan)?);
                Ok(())
            }
            SandboxCommand::Run(options) => {
                let plan = reporch_cli::local_sandbox::plan(&options.into_local()).await?;
                let result = reporch_cli::local_sandbox::execute(&plan).await?;
                println!("{}", serde_json::to_string_pretty(&result)?);
                if result.exit_code == 0 {
                    Ok(())
                } else {
                    bail!("sandbox command exited with {}", result.exit_code)
                }
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

async fn qualification_self_test() -> Result<()> {
    qualification_keyring_canary()
        .await
        .context("OS credential-store canary")?;

    let temporary = tempfile::tempdir().context("create qualification fixture directory")?;
    reporch_cli::init_project_with_id(
        temporary.path(),
        "Reporch Studio CLI qualification",
        Uuid::now_v7(),
    )?;
    let manifest_bytes = fs::read(temporary.path().join("reporch.problem.json"))?;
    let manifest: ReleaseManifestV1 = serde_json::from_slice(&manifest_bytes)?;
    let issues = validate_manifest(&manifest);
    if !issues.is_empty() {
        bail!("generated qualification manifest did not pass static validation");
    }

    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "schema": "reporch.cli-installed-self-test.v1",
            "version": env!("CARGO_PKG_VERSION"),
            "target_os": std::env::consts::OS,
            "target_arch": std::env::consts::ARCH,
            "credential_store_round_trip": true,
            "generated_manifest_valid": true,
            "problem_type_count": 6,
            "passed": true,
        }))?
    );
    Ok(())
}

async fn auth_login(options: &NativeAuthOptions) -> Result<()> {
    let config = device_auth_config(options)?;
    let client = NativeAuthClient::discover(config)
        .await
        .context("discover the Reporch identity provider")?;
    let prompt = client
        .request_device_authorization()
        .await
        .context("start device authorization")?;

    println!("Open this URL to sign in: {}", prompt.verification_uri);
    println!("Enter code: {}", prompt.user_code);
    println!("The code expires at {}.", prompt.expires_at.to_rfc3339());
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
    println!("{}", serde_json::to_string_pretty(&status)?);
    Ok(())
}

async fn auth_status(options: &NativeAuthOptions) -> Result<()> {
    let config = device_auth_config(options)?;
    let status = config
        .local_session_status(&KeyringTokenStore)
        .await
        .context("read the OS credential store")?;
    println!("{}", serde_json::to_string_pretty(&status)?);
    Ok(())
}

async fn auth_logout(options: &NativeAuthOptions) -> Result<()> {
    let config = device_auth_config(options)?;
    let remote_result = match NativeAuthClient::discover(config.clone()).await {
        Ok(client) => client.logout(&KeyringTokenStore).await,
        Err(error) => {
            eprintln!("Remote revocation is unavailable: {error}");
            config.clear_local_session(&KeyringTokenStore).await?;
            println!("Local Studio credential removed.");
            return Ok(());
        }
    };
    match remote_result {
        Ok(true) => println!("Studio credential revoked and removed."),
        Ok(false) => {
            println!("Local Studio credential removed; no revocation endpoint was available.")
        }
        Err(error) => {
            config.clear_local_session(&KeyringTokenStore).await?;
            eprintln!("Remote revocation failed: {error}");
            println!("Local Studio credential removed.");
        }
    }
    Ok(())
}

fn import_package(input: &Path, directory: &Path, profile: PackageProfile) -> Result<()> {
    match profile {
        PackageProfile::Icpc202509 => {
            let manifest = icpc_import::import_icpc_2025_09(input, directory)?;
            println!(
                "imported ICPC 2025-09 package into {}: {}",
                directory.display(),
                manifest.digest()?
            );
            Ok(())
        }
        PackageProfile::IcpcLegacy => {
            let manifest = icpc_legacy::import_icpc_legacy(input, directory)?;
            println!(
                "imported legacy ICPC package into {}: {}",
                directory.display(),
                manifest.digest()?
            );
            Ok(())
        }
        PackageProfile::DomjudgeZip => {
            let manifest = icpc_import::import_domjudge_zip(input, directory)?;
            println!(
                "imported DOMjudge package into {}: {}",
                directory.display(),
                manifest.digest()?
            );
            Ok(())
        }
        PackageProfile::PolygonCompatible => {
            let manifest = polygon_import::import_polygon_package(input, directory)?;
            println!(
                "imported Polygon-compatible package into {}: {}",
                directory.display(),
                manifest.digest()?
            );
            Ok(())
        }
        PackageProfile::ReporchNative => {
            let manifest = native_package::import_native(input, directory)?;
            println!(
                "imported Reporch Native package into {}: {}",
                directory.display(),
                manifest.digest()?
            );
            Ok(())
        }
    }
}

fn export_package(
    manifest_path: &Path,
    output: &Path,
    profile: PackageProfile,
    source_root: Option<&Path>,
) -> Result<()> {
    let manifest = read_manifest(manifest_path)?;
    let source_root = source_root
        .map(Path::to_path_buf)
        .or_else(|| manifest_path.parent().map(Path::to_path_buf))
        .context("manifest path has no parent directory")?;
    match profile {
        PackageProfile::Icpc202509 => {
            icpc_export::export_icpc_2025_09(&manifest, &source_root, output)?;
            println!("exported ICPC 2025-09 package: {}", output.display());
            Ok(())
        }
        PackageProfile::IcpcLegacy => {
            icpc_legacy::export_icpc_legacy(&manifest, &source_root, output)?;
            println!("exported legacy ICPC package: {}", output.display());
            Ok(())
        }
        PackageProfile::DomjudgeZip => {
            icpc_export::export_domjudge_zip(&manifest, &source_root, output)?;
            println!("exported DOMjudge package: {}", output.display());
            Ok(())
        }
        PackageProfile::PolygonCompatible => {
            polygon_export::export_polygon_package(&manifest, &source_root, output)?;
            println!("exported Polygon-compatible package: {}", output.display());
            Ok(())
        }
        PackageProfile::ReporchNative => {
            native_package::export_native(&manifest, &source_root, output)?;
            println!("exported Reporch Native package: {}", output.display());
            Ok(())
        }
    }
}

fn compatibility(path: &Path, profile: PackageProfile, strict: bool) -> Result<()> {
    let manifest = read_manifest(path)?;
    let report = compatibility_report(&manifest, profile);
    println!("{}", serde_json::to_string_pretty(&report)?);
    if strict && !report.exportable {
        bail!("manifest cannot be exported to the requested profile");
    }
    Ok(())
}

fn read_manifest(path: &Path) -> Result<ReleaseManifestV1> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
}

fn validate(path: &Path, print_digest: bool) -> Result<()> {
    let manifest = read_manifest(path)?;
    let issues = validate_manifest(&manifest);
    if !issues.is_empty() {
        println!("{}", serde_json::to_string_pretty(&issues)?);
        bail!("manifest validation failed with {} issue(s)", issues.len());
    }
    local_manifest::verify_files(path, &manifest)?;
    if print_digest {
        println!("{}", manifest.digest()?);
    } else {
        println!("manifest is valid: {}", manifest.digest()?);
    }
    Ok(())
}

pub(crate) fn init_project_with_id(directory: &Path, title: &str, project_id: Uuid) -> Result<()> {
    let title = title.trim();
    if title.is_empty() {
        bail!("title is required");
    }
    preflight_init_directory(directory)?;
    fs::create_dir_all(directory.join("statements"))?;
    fs::create_dir_all(directory.join("tests"))?;
    fs::create_dir_all(directory.join("solutions"))?;
    let statement = format!(
        "# {title}\n\n입력으로 주어진 내용을 그대로 출력하는 시작 템플릿입니다. 문제 설명을 교체하세요.\n"
    );
    let sample_input = "sample\n";
    let sample_answer = "sample\n";
    let accepted = "import sys\nsys.stdout.write(sys.stdin.read())\n";
    let accepted_alt = "value = input()\nprint(value)\n";
    let wrong = "print('wrong')\n";
    write_new_file(directory.join("statements/ko.md"), statement.as_bytes())?;
    write_new_file(directory.join("tests/1.in"), sample_input.as_bytes())?;
    write_new_file(directory.join("tests/1.ans"), sample_answer.as_bytes())?;
    write_new_file(directory.join("solutions/accepted.py"), accepted.as_bytes())?;
    write_new_file(
        directory.join("solutions/accepted-alt.py"),
        accepted_alt.as_bytes(),
    )?;
    write_new_file(directory.join("solutions/wrong.py"), wrong.as_bytes())?;

    let files = vec![
        manifest_file("statements/ko.md", statement.as_bytes(), "text/markdown"),
        manifest_file("tests/1.in", sample_input.as_bytes(), "text/plain"),
        manifest_file("tests/1.ans", sample_answer.as_bytes(), "text/plain"),
        manifest_file(
            "solutions/accepted.py",
            accepted.as_bytes(),
            "text/x-python",
        ),
        manifest_file(
            "solutions/accepted-alt.py",
            accepted_alt.as_bytes(),
            "text/x-python",
        ),
        manifest_file("solutions/wrong.py", wrong.as_bytes(), "text/x-python"),
    ];

    let manifest = ReleaseManifestV1 {
        schema: RELEASE_MANIFEST_SCHEMA_V1.into(),
        project_id,
        commit_id: Uuid::now_v7(),
        problem_type: ProblemType::Standard,
        package_profile: PackageProfile::ReporchNative,
        default_locale: "ko".into(),
        title: BTreeMap::from([("ko".into(), title.into())]),
        statements: BTreeMap::from([("ko".into(), "statements/ko.md".into())]),
        files,
        toolchains: BTreeMap::new(),
        judging: JudgingSpec {
            limits: ResourceLimits {
                time_ms: 1000,
                memory_mib: 256,
                output_kib: 1024,
            },
            checker: CheckerSpec::Token,
            tests: vec![TestCaseSpec {
                id: Uuid::now_v7(),
                name: "sample-1".into(),
                input_file: "tests/1.in".into(),
                answer_file: Some("tests/1.ans".into()),
                groups: vec![],
                generated_by: None,
                generator_arguments: vec![],
                seed: None,
            }],
            groups: vec![],
            generators: vec![],
            validator_path: None,
            validator_language: None,
            extra_validator_paths: vec![],
            extra_validators: vec![],
            validator_tests: vec![],
            checker_tests: vec![],
            interactor_path: None,
            interactor_language: None,
            grader_path: None,
            grader_language: None,
            harness: None,
        },
        sources: vec![],
        solutions: vec![
            SolutionSpec {
                name: "accepted".into(),
                source_path: "solutions/accepted.py".into(),
                language: "python3".into(),
                expected_verdict: ExpectedVerdict::Accepted,
                expected_score: None,
            },
            SolutionSpec {
                name: "accepted-alt".into(),
                source_path: "solutions/accepted-alt.py".into(),
                language: "python3".into(),
                expected_verdict: ExpectedVerdict::Accepted,
                expected_score: None,
            },
            SolutionSpec {
                name: "known-wrong".into(),
                source_path: "solutions/wrong.py".into(),
                language: "python3".into(),
                expected_verdict: ExpectedVerdict::WrongAnswer,
                expected_score: None,
            },
        ],
        output_submissions: vec![],
        publication: Some(PublicationSpecV1 {
            category: "Algorithm".into(),
            difficulty: "Bronze 5".into(),
            grading_category: "algorithmic".into(),
            tags: vec![],
            allowed_languages: vec!["python3".into()],
            statement_sections: BTreeMap::from([(
                "ko".into(),
                StatementSectionsV1 {
                    input_format: "출력할 문자열이 주어집니다.".into(),
                    output_format: "입력을 그대로 출력합니다.".into(),
                    note: String::new(),
                },
            )]),
            samples: vec![PublicationSampleV1 {
                name: "sample-1".into(),
                input_file: "tests/1.in".into(),
                output_file: "tests/1.ans".into(),
            }],
        }),
        policy_version: "studio-policy-v1".into(),
    };
    write_new_file(
        directory.join("reporch.problem.json"),
        &serde_json::to_vec_pretty(&manifest)?,
    )?;
    println!("initialized {}", directory.canonicalize()?.display());
    println!("the generated project is immediately valid; replace the template files as needed");
    Ok(())
}

pub(crate) fn preflight_init_directory(directory: &Path) -> Result<()> {
    if !directory.exists() {
        return Ok(());
    }
    let metadata = fs::symlink_metadata(directory)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        bail!("project directory must be a real directory");
    }
    if fs::read_dir(directory)?.next().transpose()?.is_some() {
        bail!("refusing to initialize a non-empty project directory");
    }
    Ok(())
}

fn write_new_file(path: impl AsRef<Path>, bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;
    let path = path.as_ref();
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("create {} without overwrite", path.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
pub(crate) fn init_project(directory: &Path, title: &str) -> Result<()> {
    init_project_with_id(directory, title, Uuid::now_v7())
}

fn manifest_file(path: &str, bytes: &[u8], media_type: &str) -> ManifestFile {
    ManifestFile {
        path: path.into(),
        sha256: studio_core::Sha256Digest::from_bytes(bytes),
        size_bytes: bytes.len() as u64,
        media_type: media_type.into(),
        executable: false,
    }
}
