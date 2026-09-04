use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use studio_core::{CheckerProtocolV1, CheckerSpec};

use crate::local_sandbox::{LocalSandboxOptions, LocalSandboxResult, OciRuntime};

#[derive(Clone)]
pub struct AuthoringProgress(Arc<dyn Fn(&str) + Send + Sync>);

impl AuthoringProgress {
    pub fn new(callback: impl Fn(&str) + Send + Sync + 'static) -> Self {
        Self(Arc::new(callback))
    }

    fn report(&self, message: impl AsRef<str>) {
        (self.0)(message.as_ref());
    }
}

impl fmt::Debug for AuthoringProgress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AuthoringProgress(..)")
    }
}

#[derive(Debug, Clone)]
pub struct AuthoringRunOptions {
    pub runtime: OciRuntime,
    pub toolchain_id: Option<String>,
    pub timeout: Duration,
    pub memory_mib: u64,
    pub cpus: f64,
    pub output_kib: u64,
    pub progress: AuthoringProgress,
}

#[derive(Debug, Clone)]
pub struct ProgramRequest<'a> {
    pub project_directory: &'a Path,
    pub source_path: &'a str,
    pub language: &'a str,
    pub arguments: &'a [String],
    pub stdin_path: Option<&'a str>,
    pub options: &'a AuthoringRunOptions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CustomCheckerVerdict {
    Accepted,
    WrongAnswer,
    JudgeError,
}

#[derive(Debug, Clone)]
pub struct CustomCheckerResult {
    pub verdict: CustomCheckerVerdict,
    pub execution: LocalSandboxResult,
}

/// Run a custom checker according to its versioned process contract.
///
/// ICPC 2025-09 invokes `checker input answer feedback_dir` and supplies the
/// team output on stdin. Exit 42 means accepted, 43 means wrong answer, and
/// every other status is a judge error. Historical Reporch manifests preserve
/// their former `input output answer`/exit-0 contract.
#[allow(clippy::too_many_arguments)]
pub async fn run_custom_checker(
    project_directory: &Path,
    source_path: &str,
    language: &str,
    protocol: CheckerProtocolV1,
    input_path: &str,
    answer_path: &str,
    output_path: &str,
    options: &AuthoringRunOptions,
) -> Result<CustomCheckerResult> {
    // Resolve every project-backed argument to its guest path. Native jobs
    // inventory only explicit `/workspace/...` command arguments, so passing
    // the host-relative spelling would validate the file without actually
    // staging it in the VM.
    let root = project_directory
        .canonicalize()
        .context("resolve checker project directory")?;
    let input_path = checked_workspace_file(&root, input_path)?;
    let answer_path = checked_workspace_file(&root, answer_path)?;
    let output_guest_path = checked_workspace_file(&root, output_path)?;
    let (arguments, stdin_path) = match protocol {
        CheckerProtocolV1::Icpc202509 => (
            vec![
                input_path.clone(),
                answer_path.clone(),
                "/run/reporch".to_owned(),
            ],
            Some(output_path),
        ),
        CheckerProtocolV1::ReporchLegacyV0 => {
            (vec![input_path, output_guest_path, answer_path], None)
        }
    };
    let execution = run_program(&ProgramRequest {
        project_directory: &root,
        source_path,
        language,
        arguments: &arguments,
        stdin_path,
        options,
    })
    .await?;
    let verdict = if execution.termination == reporch_runtime_core::GuestTerminationV2::Exited {
        custom_checker_verdict(protocol, execution.exit_code)
    } else {
        CustomCheckerVerdict::JudgeError
    };
    Ok(CustomCheckerResult { verdict, execution })
}

fn custom_checker_verdict(protocol: CheckerProtocolV1, exit_code: i32) -> CustomCheckerVerdict {
    match protocol {
        CheckerProtocolV1::Icpc202509 => match exit_code {
            42 => CustomCheckerVerdict::Accepted,
            43 => CustomCheckerVerdict::WrongAnswer,
            _ => CustomCheckerVerdict::JudgeError,
        },
        CheckerProtocolV1::ReporchLegacyV0 if exit_code == 0 => CustomCheckerVerdict::Accepted,
        CheckerProtocolV1::ReporchLegacyV0 => CustomCheckerVerdict::WrongAnswer,
    }
}

#[derive(Debug, Clone)]
pub struct LinkedPairRequest<'a> {
    pub project_directory: &'a Path,
    pub first_source_path: &'a str,
    pub second_source_path: &'a str,
    pub language: &'a str,
    pub stdin_path: &'a str,
    pub options: &'a AuthoringRunOptions,
}

#[derive(Debug, Clone)]
pub struct InteractivePairRequest<'a> {
    pub project_directory: &'a Path,
    pub solver_source_path: &'a str,
    pub interactor_source_path: &'a str,
    pub language: &'a str,
    pub input_path: &'a str,
    pub options: &'a AuthoringRunOptions,
}

pub async fn run_program(request: &ProgramRequest<'_>) -> Result<LocalSandboxResult> {
    ensure!(
        request.arguments.len() <= 256
            && request
                .arguments
                .iter()
                .all(|argument| { !argument.contains('\0') && argument.len() <= 4_096 }),
        "program arguments exceed the local execution limits"
    );
    let root = request
        .project_directory
        .canonicalize()
        .context("resolve authoring project directory")?;
    ensure!(root.is_dir(), "authoring project root is not a directory");
    let source = checked_workspace_file(&root, request.source_path)?;
    let stdin = request
        .stdin_path
        .map(|path| checked_workspace_file(&root, path))
        .transpose()?;

    request.options.progress.report(format!(
        "Resolving the signed {} toolchain",
        request.language
    ));
    let entry = crate::toolchain::resolve_for_language(
        request.options.toolchain_id.as_deref(),
        request.language,
    )?;
    request.options.progress.report(format!(
        "Installing or verifying signed toolchain {}; first use may download assets",
        entry.id
    ));
    let inspection = if request.options.runtime == OciRuntime::Auto {
        crate::toolchain::install(&entry.id, request.options.runtime).await?
    } else {
        crate::toolchain::inspect(&entry.id, request.options.runtime).await?
    };
    ensure!(
        inspection.installed,
        "signed toolchain {} is not installed; run `reporch toolchain install {}` explicitly",
        entry.id,
        entry.id
    );

    let command = program_command(
        &entry.language,
        &source,
        stdin.as_deref(),
        request.arguments,
    )?;
    let sandbox = LocalSandboxOptions {
        runtime: request.options.runtime,
        image: entry.image,
        project_directory: root,
        command,
        timeout: request.options.timeout,
        memory_mib: request.options.memory_mib,
        cpus: request.options.cpus,
        output_kib: request.options.output_kib,
    };
    request
        .options
        .progress
        .report("Preparing the isolated Reporch VM");
    let plan = crate::local_sandbox::plan(&sandbox).await?;
    request
        .options
        .progress
        .report("Running the isolated Reporch VM job");
    crate::local_sandbox::execute(&plan).await
}

/// Run one trusted orchestration command inside a language toolchain VM.
///
/// Every project file needed by the command must be passed as an explicit
/// `/workspace/...` argument so the native backend can inventory and stage it.
pub async fn run_toolchain_command(
    project_directory: &Path,
    language: &str,
    command: Vec<String>,
    options: &AuthoringRunOptions,
) -> Result<LocalSandboxResult> {
    ensure!(
        !command.is_empty()
            && command.len() <= 256
            && command
                .iter()
                .all(|argument| !argument.contains('\0') && argument.len() <= 4_096),
        "toolchain command exceeds the local execution limits"
    );
    let root = project_directory
        .canonicalize()
        .context("resolve authoring project directory")?;
    ensure!(root.is_dir(), "authoring project root is not a directory");
    let entry = checked_installed_toolchain(language, options).await?;
    execute_in_toolchain(root, command, entry.image, options).await
}

pub async fn run_linked_pair(request: &LinkedPairRequest<'_>) -> Result<LocalSandboxResult> {
    let root = request
        .project_directory
        .canonicalize()
        .context("resolve authoring project directory")?;
    let first = checked_workspace_file(&root, request.first_source_path)?;
    let second = checked_workspace_file(&root, request.second_source_path)?;
    let input = checked_workspace_file(&root, request.stdin_path)?;
    let entry = checked_installed_toolchain(request.language, request.options).await?;
    let compiler = match entry.language.as_str() {
        "c" => "cc -std=c17 -O2 -pipe \"$first\" \"$second\" -o /run/reporch/program",
        "cpp" => "c++ -std=c++20 -O2 -pipe \"$first\" \"$second\" -o /run/reporch/program",
        _ => bail!("linked grader execution currently requires the signed C or C++ toolchain"),
    };
    let command = vec![
        "sh".into(),
        "-c".into(),
        format!(
            "set -eu; first=$1; second=$2; input=$3; {compiler}; exec /run/reporch/program < \"$input\""
        ),
        "reporch".into(),
        first,
        second,
        input,
    ];
    execute_in_toolchain(root, command, entry.image, request.options).await
}

pub async fn run_interactive_pair(
    request: &InteractivePairRequest<'_>,
) -> Result<LocalSandboxResult> {
    let root = request
        .project_directory
        .canonicalize()
        .context("resolve authoring project directory")?;
    let solver = checked_workspace_file(&root, request.solver_source_path)?;
    let interactor = checked_workspace_file(&root, request.interactor_source_path)?;
    let input = checked_workspace_file(&root, request.input_path)?;
    let entry = checked_installed_toolchain(request.language, request.options).await?;
    let (solver_compile, interactor_compile) = match entry.language.as_str() {
        "c" => (
            "cc -std=c17 -O2 -pipe \"$solver\" -o /run/reporch/solver",
            "cc -std=c17 -O2 -pipe \"$interactor\" -o /run/reporch/interactor",
        ),
        "cpp" => (
            "c++ -std=c++20 -O2 -pipe \"$solver\" -o /run/reporch/solver",
            "c++ -std=c++20 -O2 -pipe \"$interactor\" -o /run/reporch/interactor",
        ),
        _ => bail!(
            "local interactive pairing currently requires matching signed C or C++ toolchains; use Studio verification for cross-language pairing"
        ),
    };
    let script = interactive_pair_script(solver_compile, interactor_compile);
    let command = vec![
        "bash".into(),
        "-c".into(),
        script,
        "reporch".into(),
        solver,
        interactor,
        input,
    ];
    execute_in_toolchain(root, command, entry.image, request.options).await
}

fn interactive_pair_script(solver_compile: &str, interactor_compile: &str) -> String {
    format!(
        r#"set -eu
solver=$1
interactor=$2
input=$3
{solver_compile}
{interactor_compile}
mkfifo /run/reporch/solver-to-interactor /run/reporch/interactor-to-solver
exec 3<>/run/reporch/solver-to-interactor
exec 4<>/run/reporch/interactor-to-solver
set +e
set -o pipefail
/run/reporch/solver < /run/reporch/interactor-to-solver 2>/run/reporch/solver.err | tee /run/reporch/solver.out > /run/reporch/solver-to-interactor &
solver_pid=$!
/run/reporch/interactor "$input" < /run/reporch/solver-to-interactor 2>/run/reporch/interactor.err | tee /run/reporch/interactor.out > /run/reporch/interactor-to-solver &
interactor_pid=$!
wait "$solver_pid"
solver_status=$?
wait "$interactor_pid"
interactor_status=$?
printf '%s\n' '--- solver -> interactor ---'
cat /run/reporch/solver.out
printf '%s\n' '--- interactor -> solver ---'
cat /run/reporch/interactor.out
printf '%s\n' '--- solver stderr ---'
cat /run/reporch/solver.err
printf '%s\n' '--- interactor stderr ---'
cat /run/reporch/interactor.err
if [ "$solver_status" -ne 0 ]; then exit 1; fi
if [ "$interactor_status" -eq 42 ]; then exit 0; fi
if [ "$interactor_status" -eq 43 ]; then exit 1; fi
exit 2
"#
    )
}

async fn checked_installed_toolchain(
    language: &str,
    options: &AuthoringRunOptions,
) -> Result<crate::toolchain::ToolchainEntryV1> {
    options
        .progress
        .report(format!("Resolving the signed {language} toolchain"));
    let entry = crate::toolchain::resolve_for_language(options.toolchain_id.as_deref(), language)?;
    options.progress.report(format!(
        "Installing or verifying signed toolchain {}; first use may download assets",
        entry.id
    ));
    let inspection = if options.runtime == OciRuntime::Auto {
        crate::toolchain::install(&entry.id, options.runtime).await?
    } else {
        crate::toolchain::inspect(&entry.id, options.runtime).await?
    };
    ensure!(
        inspection.installed,
        "signed toolchain {} is not installed; run `reporch toolchain install {}` explicitly",
        entry.id,
        entry.id
    );
    Ok(entry)
}

async fn execute_in_toolchain(
    root: PathBuf,
    command: Vec<String>,
    image: String,
    options: &AuthoringRunOptions,
) -> Result<LocalSandboxResult> {
    let sandbox = LocalSandboxOptions {
        runtime: options.runtime,
        image,
        project_directory: root,
        command,
        timeout: options.timeout,
        memory_mib: options.memory_mib,
        cpus: options.cpus,
        output_kib: options.output_kib,
    };
    options.progress.report("Preparing the isolated Reporch VM");
    let plan = crate::local_sandbox::plan(&sandbox).await?;
    options
        .progress
        .report("Running the isolated Reporch VM job");
    crate::local_sandbox::execute(&plan).await
}

fn checked_workspace_file(root: &Path, path: &str) -> Result<String> {
    let normalized = studio_core::normalize_relative_path(path)?;
    let candidate = root.join(&normalized);
    ensure_no_symlink_ancestors(root, Path::new(&normalized))?;
    let metadata = fs::symlink_metadata(&candidate)
        .with_context(|| format!("inspect local execution input {normalized}"))?;
    ensure!(
        metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
        "local execution input must be a regular non-symlink file: {normalized}"
    );
    let canonical = candidate
        .canonicalize()
        .with_context(|| format!("resolve local execution input {normalized}"))?;
    ensure!(
        canonical.starts_with(root),
        "local execution input escapes the project: {normalized}"
    );
    Ok(format!("/workspace/{normalized}"))
}

fn ensure_no_symlink_ancestors(root: &Path, path: &Path) -> Result<()> {
    let mut cursor = root.to_owned();
    if let Some(parent) = path.parent() {
        for component in parent.components() {
            cursor.push(component);
            let metadata = fs::symlink_metadata(&cursor)
                .with_context(|| format!("inspect project path component {}", cursor.display()))?;
            ensure!(
                metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
                "local execution path contains a non-directory or symlink: {}",
                cursor.display()
            );
        }
    }
    Ok(())
}

fn program_command(
    language: &str,
    source: &str,
    stdin: Option<&str>,
    arguments: &[String],
) -> Result<Vec<String>> {
    let input = stdin.unwrap_or("-");
    let mut command = match language {
        "python" => interpreted_command("python3", source, input),
        "pypy" => interpreted_command("pypy3", source, input),
        "javascript" => interpreted_command("node", source, input),
        "php" => interpreted_command("php", source, input),
        "r" => interpreted_command("Rscript", source, input),
        "bash" => interpreted_command("bash", source, input),
        "c" => compiled_command(
            "cc -std=c17 -O2 -pipe \"$src\" -o /run/reporch/program",
            source,
            input,
        ),
        "cpp" => compiled_command(
            "c++ -std=c++20 -O2 -pipe \"$src\" -o /run/reporch/program",
            source,
            input,
        ),
        "rust" => compiled_command(
            "rustc --edition=2024 -O \"$src\" -o /run/reporch/program",
            source,
            input,
        ),
        "swift" => compiled_command("swiftc -O \"$src\" -o /run/reporch/program", source, input),
        "java" => java_command(source, input)?,
        "csharp" => csharp_command(source, input),
        _ => bail!("unsupported signed toolchain language: {language}"),
    };
    command.extend(arguments.iter().cloned());
    Ok(command)
}

fn interpreted_command(interpreter: &str, source: &str, input: &str) -> Vec<String> {
    vec![
        "sh".into(),
        "-c".into(),
        "input=$1; shift; if [ \"$input\" != - ]; then exec \"$@\" < \"$input\"; else exec \"$@\"; fi".into(),
        "reporch".into(),
        input.into(),
        interpreter.into(),
        source.into(),
    ]
}

fn compiled_command(compiler: &str, source: &str, input: &str) -> Vec<String> {
    vec![
        "sh".into(),
        "-c".into(),
        format!(
            "set -eu; src=$1; input=$2; shift 2; {compiler}; if [ \"$input\" != - ]; then exec /run/reporch/program \"$@\" < \"$input\"; else exec /run/reporch/program \"$@\"; fi"
        ),
        "reporch".into(),
        source.into(),
        input.into(),
    ]
}

fn java_command(source: &str, input: &str) -> Result<Vec<String>> {
    let class = PathBuf::from(source)
        .file_stem()
        .and_then(|value| value.to_str())
        .context("Java source name must be valid Unicode")?
        .to_owned();
    ensure!(
        !class.is_empty()
            && class
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$')),
        "Java source file stem is not a valid main class name"
    );
    Ok(vec![
        "sh".into(),
        "-c".into(),
        "set -eu; src=$1; input=$2; class=$3; shift 3; mkdir -p /run/reporch/classes; javac -d /run/reporch/classes \"$src\"; if [ \"$input\" != - ]; then exec java -cp /run/reporch/classes \"$class\" \"$@\" < \"$input\"; else exec java -cp /run/reporch/classes \"$class\" \"$@\"; fi".into(),
        "reporch".into(),
        source.into(),
        input.into(),
        class,
    ])
}

fn csharp_command(source: &str, input: &str) -> Vec<String> {
    vec![
        "sh".into(),
        "-c".into(),
        "set -eu; src=$1; input=$2; shift 2; mkdir -p /run/reporch/app; printf '%s\n' '<Project Sdk=\"Microsoft.NET.Sdk\"><PropertyGroup><OutputType>Exe</OutputType><TargetFramework>net10.0</TargetFramework><ImplicitUsings>enable</ImplicitUsings></PropertyGroup></Project>' > /run/reporch/app/app.csproj; cp \"$src\" /run/reporch/app/Program.cs; dotnet build /run/reporch/app/app.csproj --nologo --verbosity quiet -o /run/reporch/out >/dev/null; if [ \"$input\" != - ]; then exec dotnet /run/reporch/out/app.dll \"$@\" < \"$input\"; else exec dotnet /run/reporch/out/app.dll \"$@\"; fi".into(),
        "reporch".into(),
        source.into(),
        input.into(),
    ]
}

pub fn standard_checker_matches(
    checker: &CheckerSpec,
    expected: &[u8],
    actual: &[u8],
) -> Result<bool> {
    match checker {
        CheckerSpec::Exact => Ok(expected == actual),
        CheckerSpec::Token => Ok(tokens(expected)? == tokens(actual)?),
        CheckerSpec::CaseInsensitive => {
            let expected = tokens(expected)?;
            let actual = tokens(actual)?;
            Ok(expected.len() == actual.len()
                && expected
                    .iter()
                    .zip(actual)
                    .all(|(left, right)| left.eq_ignore_ascii_case(&right)))
        }
        CheckerSpec::Floating {
            absolute_error,
            relative_error,
        } => {
            let absolute = parse_tolerance(absolute_error, "absolute")?;
            let relative = parse_tolerance(relative_error, "relative")?;
            let expected = tokens(expected)?;
            let actual = tokens(actual)?;
            if expected.len() != actual.len() {
                return Ok(false);
            }
            for (expected, actual) in expected.iter().zip(actual) {
                let expected = expected
                    .parse::<f64>()
                    .with_context(|| format!("expected token is not a number: {expected}"))?;
                let actual = actual
                    .parse::<f64>()
                    .with_context(|| format!("actual token is not a number: {actual}"))?;
                if !expected.is_finite() || !actual.is_finite() {
                    return Ok(false);
                }
                let difference = (expected - actual).abs();
                if difference > absolute.max(relative * expected.abs()) {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        CheckerSpec::Custom { .. } => bail!("custom checker requires sandbox execution"),
    }
}

fn tokens(bytes: &[u8]) -> Result<Vec<String>> {
    Ok(std::str::from_utf8(bytes)
        .context("standard checker input is not UTF-8")?
        .split_whitespace()
        .map(str::to_owned)
        .collect())
}

fn parse_tolerance(value: &str, name: &str) -> Result<f64> {
    let value = value
        .parse::<f64>()
        .with_context(|| format!("{name} checker tolerance is invalid"))?;
    ensure!(
        value.is_finite() && value >= 0.0,
        "{name} checker tolerance must be finite and non-negative"
    );
    Ok(value)
}

#[cfg(test)]
mod checker_protocol_tests {
    use super::*;

    #[test]
    fn icpc_checker_exit_codes_are_exact_and_fail_closed() {
        assert_eq!(
            custom_checker_verdict(CheckerProtocolV1::Icpc202509, 42),
            CustomCheckerVerdict::Accepted
        );
        assert_eq!(
            custom_checker_verdict(CheckerProtocolV1::Icpc202509, 43),
            CustomCheckerVerdict::WrongAnswer
        );
        for exit_code in [0, 1, 41, 44, 128, 255] {
            assert_eq!(
                custom_checker_verdict(CheckerProtocolV1::Icpc202509, exit_code),
                CustomCheckerVerdict::JudgeError,
                "unexpected ICPC checker exit {exit_code} must be a judge error"
            );
        }
    }

    #[test]
    fn legacy_checker_exit_codes_remain_backward_compatible() {
        assert_eq!(
            custom_checker_verdict(CheckerProtocolV1::ReporchLegacyV0, 0),
            CustomCheckerVerdict::Accepted
        );
        assert_eq!(
            custom_checker_verdict(CheckerProtocolV1::ReporchLegacyV0, 1),
            CustomCheckerVerdict::WrongAnswer
        );
    }

    #[test]
    fn interactive_pair_maps_icpc_interactor_verdicts_before_returning() {
        let script = interactive_pair_script("compile solver", "compile interactor");
        assert!(script.contains("interactor_status\" -eq 42 ]; then exit 0"));
        assert!(script.contains("interactor_status\" -eq 43 ]; then exit 1"));
        assert!(script.ends_with("exit 2\n"));
        assert!(!script.contains("interactor_status\" -ne 0"));
    }
}
