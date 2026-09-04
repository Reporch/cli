use std::fs;
use std::io::{IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use clap::{Args as ClapArgs, Subcommand, ValueEnum};
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use studio_core::{
    CheckerSpec, CheckerTestSpec, ExpectedScoreRange, ExpectedVerdict, OutputSubmissionSpec,
    ProgramSpec, SolutionSpec, TestCaseSpec, TestGroupSpec, ValidatorTestSpec,
};
use uuid::Uuid;

mod local_verify_batch;
mod v2;

use crate::cli_output::CliOutput;

#[derive(Debug, ClapArgs)]
pub struct StatementOptions {
    #[command(subcommand)]
    command: StatementCommand,
}

#[derive(Debug, Subcommand)]
enum StatementCommand {
    Add {
        #[arg(long)]
        locale: String,
        #[arg(long)]
        path: PathBuf,
        #[arg(long)]
        title: Option<String>,
        /// Create a safe Markdown starter when the path does not exist.
        #[arg(long)]
        create: bool,
    },
    Open {
        #[arg(long)]
        locale: Option<String>,
    },
    Check,
    Render {
        #[arg(long)]
        locale: Option<String>,
        #[arg(long, value_enum, default_value_t = StatementRenderFormat::Html)]
        render_format: StatementRenderFormat,
        #[arg(long)]
        output: Option<PathBuf>,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum StatementRenderFormat {
    Markdown,
    Html,
    Latex,
}

#[derive(Debug, ClapArgs)]
#[command(
    after_help = "Examples:\n  reporch test group add samples --points 0\n  reporch test case add --name sample-1 --input-text '1 2' --answer-text '3' --group samples\n  reporch test case add --name sample-2 --input-text '3 4' --answer-text '7'\n  reporch test group add full-score --points 100 --depends-on samples\n\n`--group` is optional. Sample tests can remain ungrouped. A 0-point sample group organizes tests and dependencies without changing the scored total."
)]
pub struct TestOptions {
    #[command(subcommand)]
    command: Option<TestCommand>,
}

#[derive(Debug, Subcommand)]
enum TestCommand {
    Case {
        #[command(subcommand)]
        command: TestCaseCommand,
    },
    Group {
        #[command(subcommand)]
        command: TestGroupCommand,
    },
}

#[derive(Debug, Subcommand)]
enum TestCaseCommand {
    List,
    Add(TestCaseAddOptions),
    Import(TestCaseImportOptions),
    Update(TestCaseUpdateOptions),
    Remove {
        /// Test name, UUID, or declared input path.
        #[arg(value_name = "NAME|UUID|PATH")]
        selector: String,
    },
}

#[derive(Debug, ClapArgs)]
struct TestCaseAddOptions {
    #[arg(long)]
    name: String,
    /// Read the test input from a project file.
    #[arg(
        long,
        value_name = "INPUT_FILE",
        required_unless_present = "input_text",
        conflicts_with = "input_text"
    )]
    input: Option<PathBuf>,
    /// Create a safe test input file from literal text.
    #[arg(long, value_name = "TEXT", conflicts_with = "input")]
    input_text: Option<String>,
    /// Read the expected answer from a project file.
    #[arg(long, value_name = "ANSWER_FILE", conflicts_with = "answer_text")]
    answer: Option<PathBuf>,
    /// Create a safe expected-answer file from literal text.
    #[arg(long, value_name = "TEXT", conflicts_with = "answer")]
    answer_text: Option<String>,
    #[arg(long = "group")]
    groups: Vec<String>,
    #[arg(long)]
    generated_by: Option<String>,
    #[arg(long)]
    seed: Option<u64>,
}

#[derive(Debug, ClapArgs)]
struct TestCaseImportOptions {
    directory: PathBuf,
    #[arg(long = "group")]
    groups: Vec<String>,
}

#[derive(Debug, ClapArgs)]
struct TestCaseUpdateOptions {
    /// Test name, UUID, or declared input path.
    #[arg(value_name = "NAME|UUID|PATH")]
    selector: String,
    #[arg(long)]
    name: Option<String>,
    #[arg(long = "group")]
    groups: Vec<String>,
}

#[derive(Debug, Subcommand)]
enum TestGroupCommand {
    List,
    Add(TestGroupAddOptions),
    Update(TestGroupUpdateOptions),
    Remove { id: String },
}

#[derive(Debug, ClapArgs)]
struct TestGroupAddOptions {
    /// Stable human-readable group name (for example `samples` or `full-score`).
    #[arg(value_name = "NAME")]
    id: String,
    #[arg(long)]
    points: f64,
    #[arg(long = "depends-on")]
    depends_on: Vec<String>,
}

#[derive(Debug, ClapArgs)]
struct TestGroupUpdateOptions {
    id: String,
    #[arg(long)]
    points: Option<f64>,
    #[arg(long = "depends-on")]
    depends_on: Vec<String>,
}

#[derive(Debug, ClapArgs)]
#[command(
    after_help = "Examples:\n  reporch generator add --id random --source generators/random.py --language python3\n  reporch generator run random --seed 1 --output tests/generated/01.in --name random-1 --group full-score\n  reporch generator recipe random --name-prefix random --count 20 --seed-start 1 --group full-score"
)]
pub struct GeneratorOptions {
    #[command(subcommand)]
    command: GeneratorCommand,
}

#[derive(Debug, Subcommand)]
enum GeneratorCommand {
    List,
    Add(ProgramAddOptions),
    Recipe(GeneratorRecipeOptions),
    Run(GeneratorRunOptions),
    Remove { id: String },
}

#[derive(Debug, ClapArgs)]
struct GeneratorRunOptions {
    /// Generator ID declared by `generator add`.
    id: String,
    #[arg(long)]
    output: PathBuf,
    #[arg(long)]
    name: Option<String>,
    #[arg(long = "group")]
    groups: Vec<String>,
    #[arg(long = "argument")]
    arguments: Vec<String>,
    #[arg(long)]
    seed: Option<u64>,
    #[command(flatten)]
    runtime: RuntimeOptions,
}

#[derive(Debug, ClapArgs)]
struct GeneratorRecipeOptions {
    /// Generator ID declared by `generator add`.
    id: String,
    /// Prefix for generated test names.
    #[arg(long)]
    name_prefix: String,
    #[arg(long, default_value = "tests/generated")]
    output_directory: PathBuf,
    #[arg(long, default_value_t = 1)]
    count: u32,
    #[arg(long, default_value_t = 1)]
    seed_start: u64,
    #[arg(long = "group")]
    groups: Vec<String>,
    #[arg(long = "argument")]
    arguments: Vec<String>,
    #[command(flatten)]
    runtime: RuntimeOptions,
}

#[derive(Debug, ClapArgs)]
struct ProgramAddOptions {
    /// Stable generator ID used by run and recipe commands.
    #[arg(long)]
    id: String,
    /// Generator source path inside this project.
    #[arg(long)]
    source: PathBuf,
    /// Toolchain language, for example python3, cpp20, or rust.
    #[arg(long)]
    language: String,
    /// Fixed argument passed before seed and recipe arguments; repeat as needed.
    #[arg(long = "argument")]
    arguments: Vec<String>,
}

#[derive(Debug, ClapArgs)]
#[command(
    after_help = "Examples:\n  reporch validator set --source validators/input.py --language python3\n  reporch validator unit-add --name accepts-sample --input-text '1 2' --expected valid\n  reporch validator unit-add --name rejects-text --input-text 'x' --expected invalid\n  reporch validator run"
)]
pub struct ValidatorOptions {
    #[command(subcommand)]
    command: ValidatorCommand,
}

#[derive(Debug, Subcommand)]
enum ValidatorCommand {
    Set {
        #[arg(long)]
        source: PathBuf,
        #[arg(long)]
        language: String,
        #[arg(long)]
        extra: bool,
    },
    UnitAdd(ValidatorUnitAddOptions),
    Run {
        #[arg(long)]
        name: Option<String>,
        #[command(flatten)]
        runtime: RuntimeOptions,
    },
}

#[derive(Debug, ClapArgs)]
struct ValidatorUnitAddOptions {
    #[arg(long)]
    name: String,
    /// Read the validator unit input from a project file.
    #[arg(
        long,
        value_name = "INPUT_FILE",
        required_unless_present = "input_text",
        conflicts_with = "input_text"
    )]
    input: Option<PathBuf>,
    /// Create a safe validator unit input file from literal text.
    #[arg(long, value_name = "TEXT", conflicts_with = "input")]
    input_text: Option<String>,
    #[arg(long, value_enum)]
    expected: ValidityExpectation,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ValidityExpectation {
    Valid,
    Invalid,
}

#[derive(Debug, ClapArgs)]
#[command(
    after_help = "Examples:\n  reporch checker set --kind token\n  reporch checker set --kind floating --absolute-error 1e-6 --relative-error 1e-6\n  reporch checker unit-add --name rejects-wrong --input tests/01.in --answer tests/01.ans --output checker-tests/wrong.out --expected reject\n  reporch checker test"
)]
pub struct CheckerOptions {
    #[command(subcommand)]
    command: CheckerCommand,
}

#[derive(Debug, Subcommand)]
enum CheckerCommand {
    ListStandard,
    /// Inspect or change the process contract for a custom checker.
    Protocol {
        #[command(subcommand)]
        command: CheckerProtocolCommand,
    },
    Set {
        #[arg(long, value_enum)]
        kind: CheckerKind,
        #[arg(long)]
        source: Option<PathBuf>,
        #[arg(long)]
        language: Option<String>,
        #[arg(long)]
        absolute_error: Option<String>,
        #[arg(long)]
        relative_error: Option<String>,
    },
    UnitAdd {
        #[arg(long)]
        name: String,
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        answer: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long, value_enum)]
        expected: CheckerExpectation,
    },
    #[command(visible_alias = "test")]
    Run {
        #[arg(long)]
        name: Option<String>,
        #[command(flatten)]
        runtime: RuntimeOptions,
    },
}

#[derive(Debug, Subcommand)]
enum CheckerProtocolCommand {
    Show,
    Set {
        #[arg(value_enum)]
        protocol: CheckerProtocolChoice,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CheckerProtocolChoice {
    #[value(name = "icpc-2025-09")]
    Icpc202509,
    #[value(name = "reporch-legacy-v0")]
    ReporchLegacyV0,
}

impl From<CheckerProtocolChoice> for studio_core::CheckerProtocolV1 {
    fn from(value: CheckerProtocolChoice) -> Self {
        match value {
            CheckerProtocolChoice::Icpc202509 => Self::Icpc202509,
            CheckerProtocolChoice::ReporchLegacyV0 => Self::ReporchLegacyV0,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CheckerKind {
    Exact,
    Token,
    CaseInsensitive,
    Floating,
    Custom,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CheckerExpectation {
    Accept,
    Reject,
}

#[derive(Debug, ClapArgs)]
pub struct SolutionOptions {
    #[command(subcommand)]
    command: SolutionCommand,
}

#[derive(Debug, ClapArgs)]
pub struct InteractorOptions {
    #[command(subcommand)]
    command: InteractorCommand,
}

#[derive(Debug, ClapArgs)]
pub struct GraderOptions {
    #[command(subcommand)]
    command: GraderCommand,
}

#[derive(Debug, Subcommand)]
enum InteractorCommand {
    Set {
        #[arg(long)]
        source: PathBuf,
        #[arg(long)]
        language: String,
    },
    Run(RuntimeProgramRunOptions),
    Transcript(RuntimeProgramRunOptions),
}

#[derive(Debug, Subcommand)]
enum GraderCommand {
    Set {
        /// Judge-side grader or manager source.
        #[arg(long)]
        source: PathBuf,
        #[arg(long)]
        language: String,
        /// Harmless contestant source template replaced by submissions.
        #[arg(long)]
        submission_template: Option<PathBuf>,
        #[arg(long, conflicts_with = "compile_command")]
        compile_script: Option<PathBuf>,
        #[arg(long, conflicts_with = "compile_script")]
        compile_command: Option<String>,
        #[arg(long, conflicts_with = "run_command")]
        run_script: Option<PathBuf>,
        #[arg(long, conflicts_with = "run_script")]
        run_command: Option<String>,
        /// Additional judge-side build/runtime asset.
        #[arg(long)]
        asset: Vec<PathBuf>,
        /// Interface file visible to every supported contestant language.
        #[arg(long)]
        interface_file: Vec<PathBuf>,
        /// Additional contestant-visible resource.
        #[arg(long)]
        public_file: Vec<PathBuf>,
    },
    Run(RuntimeProgramRunOptions),
}

#[derive(Debug, Clone, ClapArgs)]
struct RuntimeProgramRunOptions {
    /// Solution name, UUID, or declared source path (for example `accepted` or `solutions/accepted.cpp`).
    #[arg(long, value_name = "NAME|UUID|PATH")]
    solution: String,
    /// Test name, UUID, or declared input path (for example `sample-1` or `tests/1.in`).
    #[arg(long, value_name = "NAME|UUID|PATH")]
    test: String,
    /// Save captured program output inside the project.
    #[arg(
        long,
        value_name = "PROJECT_RELATIVE_PATH",
        value_parser = parse_runtime_output_path
    )]
    output: Option<PathBuf>,
    #[command(flatten)]
    runtime: RuntimeOptions,
}

#[derive(Debug, ClapArgs)]
pub struct OutputOptions {
    #[command(subcommand)]
    command: OutputCommand,
}

#[derive(Debug, ClapArgs)]
pub struct AnswerOptions {
    #[command(subcommand)]
    command: AnswerCommand,
}

#[derive(Debug, Subcommand)]
enum AnswerCommand {
    /// Generate expected output files with an accepted reference solution.
    Generate {
        #[arg(long)]
        solution: String,
        /// Generate only one test; omitted means every selected test.
        #[arg(long)]
        test: Option<Uuid>,
        /// Skip tests that already have an answer file instead of failing.
        #[arg(long)]
        missing_only: bool,
        #[command(flatten)]
        runtime: RuntimeOptions,
    },
}

#[derive(Debug, ClapArgs)]
pub struct StressOptions {
    #[command(subcommand)]
    command: StressCommand,
}

#[derive(Debug, Subcommand)]
enum StressCommand {
    List,
    Add {
        #[arg(long)]
        name: String,
        #[arg(long)]
        generator: String,
        #[arg(long)]
        recipe: String,
        #[arg(long)]
        oracle: String,
        #[arg(long = "candidate", required = true)]
        candidates: Vec<String>,
        #[arg(long, default_value_t = 1)]
        seed_start: u64,
        #[arg(long, default_value_t = 100)]
        cases: u32,
        #[arg(long, default_value_t = 2_000)]
        timeout_ms: u64,
        #[arg(long)]
        minimize_failure: bool,
    },
    Run {
        name: String,
        #[command(flatten)]
        runtime: RuntimeOptions,
    },
    Remove {
        name: String,
    },
}

#[derive(Debug, ClapArgs)]
struct OutputRemoveOptions {
    /// Submission name. The positional form is retained for compatibility.
    #[arg(value_name = "NAME", required_unless_present = "name_option")]
    name: Option<String>,
    /// Submission name.
    #[arg(long = "name", value_name = "NAME", conflicts_with = "name")]
    name_option: Option<String>,
}

impl OutputRemoveOptions {
    fn into_name(self) -> String {
        self.name_option
            .or(self.name)
            .expect("clap requires a name")
    }
}

#[derive(Debug, Subcommand)]
enum OutputCommand {
    List,
    Add {
        #[arg(long)]
        name: String,
        #[arg(long, value_enum)]
        expected: Verdict,
        /// Test UUID and output path, for example UUID=outputs/01.txt.
        #[arg(long = "map", value_parser = parse_output_mapping)]
        mappings: Vec<(Uuid, String)>,
        #[arg(long)]
        minimum_score: Option<f64>,
        #[arg(long)]
        maximum_score: Option<f64>,
    },
    Remove(OutputRemoveOptions),
    Test {
        #[arg(long)]
        name: Option<String>,
        #[command(flatten)]
        runtime: RuntimeOptions,
    },
}

#[derive(Debug, Clone, ClapArgs)]
struct RuntimeOptions {
    /// Signed toolchain catalog ID. Inferred from the configured language when omitted.
    #[arg(long)]
    toolchain: Option<String>,
    /// Execution backend. `auto` uses the mandatory Reporch VM; `podman` and `docker` are deprecated explicit compatibility modes.
    #[arg(long, value_enum, default_value_t = RuntimeKind::Auto)]
    runtime: RuntimeKind,
    /// Sandbox wall timeout in seconds.
    #[arg(
        long,
        default_value_t = 30,
        value_parser = clap::value_parser!(u64).range(1..=600)
    )]
    timeout_seconds: u64,
    #[arg(long, default_value_t = 512)]
    memory_mib: u64,
    #[arg(long, default_value_t = 1.0)]
    cpus: f64,
    #[arg(long, default_value_t = 1_024)]
    output_kib: u64,
}

#[derive(Debug, Clone, Copy, Default, ValueEnum)]
pub(crate) enum RuntimeKind {
    #[default]
    Auto,
    /// Deprecated explicit compatibility backend.
    Podman,
    /// Deprecated explicit compatibility backend.
    Docker,
}

impl RuntimeOptions {
    fn into_run_options(
        self,
        output: &CliOutput,
    ) -> reporch_cli::authoring_runtime::AuthoringRunOptions {
        output.progress("local execution", "Initializing local verification");
        let progress_output = output.clone();
        let progress = reporch_cli::authoring_runtime::AuthoringProgress::new(move |message| {
            progress_output.progress("local execution", message);
        });
        let runtime = match self.runtime {
            RuntimeKind::Auto => reporch_cli::local_sandbox::OciRuntime::Auto,
            RuntimeKind::Podman => reporch_cli::local_sandbox::OciRuntime::Podman,
            RuntimeKind::Docker => reporch_cli::local_sandbox::OciRuntime::Docker,
        };
        reporch_cli::authoring_runtime::AuthoringRunOptions {
            runtime,
            toolchain_id: self.toolchain,
            timeout: Duration::from_secs(self.timeout_seconds),
            memory_mib: self.memory_mib,
            cpus: self.cpus,
            output_kib: self.output_kib,
            progress,
        }
    }
}

#[derive(Debug, Serialize)]
struct LocalVerifyReport {
    schema: &'static str,
    evidence: &'static str,
    passed: bool,
    generator_checks: Vec<LocalGeneratorCheck>,
    validator_units: usize,
    checker_units: usize,
    solutions: Vec<LocalSolutionCheck>,
    output_submissions: usize,
}

#[derive(Debug, Serialize)]
struct LocalGeneratorCheck {
    generator: String,
    recipe: String,
    seed: u64,
    expected_sha256: Option<String>,
    actual_sha256: String,
    passed: bool,
}

#[derive(Debug, Serialize)]
struct LocalSolutionCheck {
    solution: String,
    expected: &'static str,
    actual: &'static str,
    score: f64,
    passed: bool,
    cases: Vec<LocalSolutionCase>,
}

#[derive(Debug, Serialize)]
struct LocalSolutionCase {
    test_id: Uuid,
    test: String,
    actual: &'static str,
    accepted: bool,
    exit_code: i32,
    termination: reporch_runtime_core::GuestTerminationV2,
    duration_ms: u128,
    stdout: String,
    stderr: String,
}

pub async fn verify_local(
    timeout_seconds: u64,
    runtime_kind: RuntimeKind,
    output: &CliOutput,
) -> Result<()> {
    ensure!(
        (1..=600).contains(&timeout_seconds),
        "local verification timeout must be between 1 and 600 seconds"
    );
    let root = reporch_cli::local_project::discover_project(Path::new("."))?;
    let spec = if reporch_cli::local_project_v2::is_v2_project(&root)? {
        reporch_cli::local_project_v2::read_authoring_spec(&root)?
    } else {
        let legacy = reporch_cli::local_project::read_authoring_spec(&root)?;
        reporch_format::AuthoringSpecV2::migrate_v1(&legacy)
            .context("migrate the legacy authoring model for local verification")?
    };
    let silent = CliOutput::new(
        crate::cli_output::OutputFormat::Human,
        true,
        crate::cli_output::ColorMode::Never,
    );
    let runtime = RuntimeOptions {
        toolchain: None,
        runtime: runtime_kind,
        timeout_seconds,
        memory_mib: spec.testing.limits.memory_mib,
        cpus: 1.0,
        output_kib: spec.testing.limits.output_kib,
    };
    let mut run_options = runtime.clone().into_run_options(output);
    run_options.timeout = Duration::from_millis(
        spec.testing
            .limits
            .time_ms
            .max(1)
            .min(timeout_seconds.saturating_mul(1_000)),
    );

    let batch = local_verify_batch::try_verify(&root, &spec, &run_options, output).await?;
    let (generator_checks, validator_units, checker_units, solutions, output_submissions) =
        if let Some(batch) = batch {
            (
                batch.generator_checks,
                batch.validator_units,
                batch.checker_units,
                batch.solutions,
                0,
            )
        } else {
            let generator_checks = verify_generators(&root, &spec, &run_options).await?;
            if !spec.testing.validators.unit_tests.is_empty() {
                v2::validator(
                    ValidatorOptions {
                        command: ValidatorCommand::Run {
                            name: None,
                            runtime: runtime.clone(),
                        },
                    },
                    &silent,
                )
                .await?;
            }
            if !spec.testing.checker.unit_tests.is_empty() {
                v2::checker(
                    CheckerOptions {
                        command: CheckerCommand::Run {
                            name: None,
                            runtime: runtime.clone(),
                        },
                    },
                    &silent,
                )
                .await?;
            }
            let (solutions, output_submissions) =
                if spec.problem_type == studio_core::ProblemType::OutputOnly {
                    v2::output_submission(
                        OutputOptions {
                            command: OutputCommand::Test {
                                name: None,
                                runtime: runtime.clone(),
                            },
                        },
                        &silent,
                    )
                    .await?;
                    (Vec::new(), spec.output_submissions.len())
                } else {
                    (verify_solution_matrix(&root, &spec, &run_options).await?, 0)
                };
            (
                generator_checks,
                spec.testing.validators.unit_tests.len(),
                spec.testing.checker.unit_tests.len(),
                solutions,
                output_submissions,
            )
        };
    let report = LocalVerifyReport {
        schema: "reporch.local-verification.v1",
        evidence: "local_preflight_only",
        passed: generator_checks.iter().all(|check| check.passed)
            && solutions.iter().all(|solution| solution.passed),
        generator_checks,
        validator_units,
        checker_units,
        solutions,
        output_submissions,
    };
    if !report.passed {
        return Err(crate::cli_output::domain_error(
            "operation.failed",
            "local verification did not match the declared expectations",
            &report,
        ));
    }
    output.emit(
        "verify",
        &report,
        "Local verification passed. This is a preflight only; run `reporch verify` for official Studio evidence.",
    )
}

async fn verify_generators(
    root: &Path,
    spec: &reporch_format::AuthoringSpecV2,
    options: &reporch_cli::authoring_runtime::AuthoringRunOptions,
) -> Result<Vec<LocalGeneratorCheck>> {
    let mut checks = Vec::new();
    let mut covered = std::collections::BTreeSet::new();
    for test in &spec.testing.tests {
        let Some(generated) = &test.generated else {
            continue;
        };
        let generator = spec
            .testing
            .generators
            .iter()
            .find(|generator| generator.program.id == generated.generator_id)
            .context("generated test references a missing generator")?;
        let recipe = generator
            .recipes
            .iter()
            .find(|recipe| recipe.id == generated.recipe_id)
            .context("generated test references a missing recipe")?;
        let bytes = materialize_generator(
            root,
            &legacy_program_v2(&generator.program),
            &recipe.argument_template,
            Some(generated.seed),
            options,
        )
        .await?;
        let expected = read_project_bytes(root, &test.input_file)?;
        let expected_sha256 = hex::encode(Sha256::digest(&expected));
        let actual_sha256 = hex::encode(Sha256::digest(&bytes));
        checks.push(LocalGeneratorCheck {
            generator: generator.program.name.clone(),
            recipe: recipe.name.clone(),
            seed: generated.seed,
            expected_sha256: Some(expected_sha256.clone()),
            actual_sha256: actual_sha256.clone(),
            passed: expected_sha256 == actual_sha256,
        });
        covered.insert((generator.program.id, recipe.id));
    }
    for generator in &spec.testing.generators {
        for recipe in &generator.recipes {
            if covered.contains(&(generator.program.id, recipe.id)) {
                continue;
            }
            let bytes = materialize_generator(
                root,
                &legacy_program_v2(&generator.program),
                &recipe.argument_template,
                Some(recipe.seed_start),
                options,
            )
            .await?;
            checks.push(LocalGeneratorCheck {
                generator: generator.program.name.clone(),
                recipe: recipe.name.clone(),
                seed: recipe.seed_start,
                expected_sha256: None,
                actual_sha256: hex::encode(Sha256::digest(&bytes)),
                passed: true,
            });
        }
    }
    Ok(checks)
}

async fn verify_solution_matrix(
    root: &Path,
    spec: &reporch_format::AuthoringSpecV2,
    options: &reporch_cli::authoring_runtime::AuthoringRunOptions,
) -> Result<Vec<LocalSolutionCheck>> {
    let mut reports = Vec::new();
    for solution in &spec.testing.solutions {
        let mut cases = Vec::new();
        for test in &spec.testing.tests {
            let result = execute_solution_case(root, spec, solution, test, options).await?;
            let actual_verdict = if spec.problem_type == studio_core::ProblemType::Interactive {
                interactive_execution_verdict(&result)
            } else {
                let checker_accepted = if result.termination
                    == reporch_runtime_core::GuestTerminationV2::Exited
                    && result.exit_code == 0
                {
                    let answer = test.answer_file.as_deref().with_context(|| {
                        format!("test {} has no answer for solution verification", test.name)
                    })?;
                    checker_accepts_bytes(
                        root,
                        &spec.testing.checker.checker,
                        &test.input_file,
                        answer,
                        &result.stdout_bytes,
                        options,
                    )
                    .await?
                } else {
                    false
                };
                program_execution_verdict(&result, checker_accepted)
            };
            cases.push(LocalSolutionCase {
                test_id: test.id,
                test: test.name.clone(),
                actual: observed_verdict_name(actual_verdict),
                accepted: actual_verdict == Some(ExpectedVerdict::Accepted),
                exit_code: result.exit_code,
                termination: result.termination,
                duration_ms: result.duration_ms,
                stdout: result.stdout,
                stderr: result.stderr,
            });
        }
        let score = score_v2(&spec.testing.groups, &spec.testing.tests, &cases)?;
        let actual =
            aggregate_solution_verdict(&cases, score, total_score_v2(&spec.testing.groups));
        let score_matches = solution
            .expected_score
            .as_ref()
            .is_none_or(|range| score >= range.minimum && score <= range.maximum);
        reports.push(LocalSolutionCheck {
            solution: solution.program.name.clone(),
            expected: verdict_name(solution.expected_verdict),
            actual: observed_verdict_name(actual),
            score,
            passed: actual == Some(solution.expected_verdict) && score_matches,
            cases,
        });
    }
    Ok(reports)
}

fn legacy_program_v2(program: &studio_core::ProgramSpecV2) -> ProgramSpec {
    ProgramSpec {
        id: program.name.clone(),
        source_path: program.source_path.clone(),
        language: program.language.clone(),
        arguments: program.arguments.clone(),
    }
}

async fn execute_solution_case(
    root: &Path,
    spec: &reporch_format::AuthoringSpecV2,
    solution: &studio_core::SolutionSpecV2,
    test: &studio_core::TestCaseSpecV2,
    options: &reporch_cli::authoring_runtime::AuthoringRunOptions,
) -> Result<reporch_cli::local_sandbox::LocalSandboxResult> {
    match spec.problem_type {
        studio_core::ProblemType::Interactive => {
            let interactive = spec
                .execution
                .interactive
                .as_ref()
                .context("interactive problem has no interactor")?;
            let interactor_toolchain = reporch_cli::toolchain::resolve_for_language(
                options.toolchain_id.as_deref(),
                &interactive.interactor.language,
            )?;
            let solution_toolchain = reporch_cli::toolchain::resolve_for_language(
                options.toolchain_id.as_deref(),
                &solution.program.language,
            )?;
            ensure!(
                interactor_toolchain.language == solution_toolchain.language,
                "local interactive pairing requires matching toolchain languages"
            );
            reporch_cli::authoring_runtime::run_interactive_pair(
                &reporch_cli::authoring_runtime::InteractivePairRequest {
                    project_directory: root,
                    solver_source_path: &solution.program.source_path,
                    interactor_source_path: &interactive.interactor.source_path,
                    language: &interactive.interactor.language,
                    input_path: &test.input_file,
                    idle_timeout: Duration::from_millis(interactive.idle_timeout_ms),
                    options,
                },
            )
            .await
        }
        studio_core::ProblemType::Library | studio_core::ProblemType::Grader => {
            let harness = spec
                .execution
                .harness
                .as_ref()
                .context("library/grader problem has no harness")?;
            let profile = harness
                .profiles
                .get(&solution.program.language)
                .or_else(|| harness.profiles.values().next())
                .context("library/grader harness has no language profile")?;
            ensure!(
                reporch_cli::toolchain::resolve_for_language(None, &profile.language)?.language
                    == reporch_cli::toolchain::resolve_for_language(
                        None,
                        &solution.program.language,
                    )?
                    .language,
                "local grader linking requires matching C or C++ toolchains"
            );
            reporch_cli::authoring_runtime::run_linked_pair(
                &reporch_cli::authoring_runtime::LinkedPairRequest {
                    project_directory: root,
                    first_source_path: &solution.program.source_path,
                    second_source_path: &profile.source_path,
                    language: &profile.language,
                    stdin_path: &test.input_file,
                    options,
                },
            )
            .await
        }
        _ => {
            reporch_cli::authoring_runtime::run_program(
                &reporch_cli::authoring_runtime::ProgramRequest {
                    project_directory: root,
                    source_path: &solution.program.source_path,
                    language: &solution.program.language,
                    arguments: &solution.program.arguments,
                    stdin_path: Some(&test.input_file),
                    options,
                },
            )
            .await
        }
    }
}

fn aggregate_solution_verdict(
    cases: &[LocalSolutionCase],
    score: f64,
    total_score: f64,
) -> Option<ExpectedVerdict> {
    if cases.iter().any(|case| case.actual == "judge_error") {
        return None;
    }
    if cases.iter().all(|case| case.accepted) {
        return Some(ExpectedVerdict::Accepted);
    }
    for verdict in [
        ExpectedVerdict::RuntimeError,
        ExpectedVerdict::MemoryLimit,
        ExpectedVerdict::TimeLimit,
    ] {
        if cases
            .iter()
            .any(|case| case.actual == verdict_name(verdict))
        {
            return Some(verdict);
        }
    }
    if score > 0.0 && score < total_score {
        Some(ExpectedVerdict::Partial)
    } else {
        Some(ExpectedVerdict::WrongAnswer)
    }
}

fn total_score_v2(groups: &[studio_core::TestGroupSpecV2]) -> f64 {
    if groups.is_empty() {
        100.0
    } else {
        groups.iter().map(|group| group.points).sum()
    }
}

fn score_v2(
    groups: &[studio_core::TestGroupSpecV2],
    tests: &[studio_core::TestCaseSpecV2],
    cases: &[LocalSolutionCase],
) -> Result<f64> {
    if groups.is_empty() {
        return Ok(if cases.iter().all(|case| case.accepted) {
            100.0
        } else {
            0.0
        });
    }
    let accepted = cases
        .iter()
        .map(|case| (case.test_id, case.accepted))
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut resolved = std::collections::BTreeMap::<Uuid, bool>::new();
    while resolved.len() < groups.len() {
        let before = resolved.len();
        for group in groups {
            if resolved.contains_key(&group.id)
                || group
                    .depends_on
                    .iter()
                    .any(|dependency| !resolved.contains_key(dependency))
            {
                continue;
            }
            let dependencies_passed = group
                .depends_on
                .iter()
                .all(|dependency| resolved.get(dependency) == Some(&true));
            let group_tests = tests
                .iter()
                .filter(|test| test.group_ids.contains(&group.id))
                .collect::<Vec<_>>();
            ensure!(
                !group_tests.is_empty(),
                "score group has no test cases: {}",
                group.name
            );
            let tests_passed = group_tests
                .iter()
                .all(|test| accepted.get(&test.id) == Some(&true));
            resolved.insert(group.id, dependencies_passed && tests_passed);
        }
        ensure!(
            resolved.len() > before,
            "score group dependencies contain a cycle or unknown group"
        );
    }
    Ok(groups
        .iter()
        .filter(|group| resolved.get(&group.id) == Some(&true))
        .map(|group| group.points)
        .sum())
}

#[derive(Debug, Subcommand)]
enum SolutionCommand {
    List,
    Add(SolutionAddOptions),
    Update(SolutionUpdateOptions),
    Remove { name: String },
    Matrix,
}

#[derive(Debug, ClapArgs)]
struct SolutionAddOptions {
    #[arg(long)]
    name: String,
    #[arg(long)]
    source: PathBuf,
    #[arg(long)]
    language: String,
    #[arg(long, value_enum)]
    expected: Verdict,
    /// How this solution is used by answer generation and verification.
    #[arg(long, value_enum)]
    role: Option<SolutionRoleOption>,
    #[arg(long)]
    minimum_score: Option<f64>,
    #[arg(long)]
    maximum_score: Option<f64>,
}

#[derive(Debug, ClapArgs)]
struct SolutionUpdateOptions {
    name: String,
    #[arg(long, value_enum)]
    expected: Option<Verdict>,
    /// Change how this solution is used by answer generation and verification.
    #[arg(long, value_enum)]
    role: Option<SolutionRoleOption>,
    #[arg(long)]
    minimum_score: Option<f64>,
    #[arg(long)]
    maximum_score: Option<f64>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Verdict {
    Accepted,
    WrongAnswer,
    TimeLimit,
    MemoryLimit,
    RuntimeError,
    Partial,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SolutionRoleOption {
    Reference,
    Alternative,
    Oracle,
    Brute,
    KnownWrong,
}

impl From<SolutionRoleOption> for studio_core::SolutionRoleV2 {
    fn from(value: SolutionRoleOption) -> Self {
        match value {
            SolutionRoleOption::Reference => Self::Reference,
            SolutionRoleOption::Alternative => Self::Alternative,
            SolutionRoleOption::Oracle => Self::Oracle,
            SolutionRoleOption::Brute => Self::Brute,
            SolutionRoleOption::KnownWrong => Self::KnownWrong,
        }
    }
}

impl From<Verdict> for ExpectedVerdict {
    fn from(value: Verdict) -> Self {
        match value {
            Verdict::Accepted => Self::Accepted,
            Verdict::WrongAnswer => Self::WrongAnswer,
            Verdict::TimeLimit => Self::TimeLimit,
            Verdict::MemoryLimit => Self::MemoryLimit,
            Verdict::RuntimeError => Self::RuntimeError,
            Verdict::Partial => Self::Partial,
        }
    }
}

pub fn statement(options: StatementOptions, output: &CliOutput) -> Result<()> {
    if v2::is_active_project()? {
        return v2::statement(options, output);
    }
    match options.command {
        StatementCommand::Add {
            locale,
            path,
            title,
            create,
        } => {
            let relative = relative_string(&path)?;
            let root = reporch_cli::local_project::discover_project(Path::new("."))?;
            let created = if create {
                materialize_statement_file(&root, &relative, title.as_deref(), &locale)?
            } else {
                None
            };
            let updated = reporch_cli::local_project::update_authoring_spec(&root, |root, spec| {
                reporch_cli::local_project::declare_project_file(
                    root,
                    spec,
                    &relative,
                    "text/markdown",
                    false,
                )
                .with_context(|| {
                    format!(
                        "create {relative} first or add --create, then rerun `reporch statement add --locale {locale} --path {relative}`"
                    )
                })?;
                spec.statements.insert(locale.clone(), relative.clone());
                if let Some(title) = &title {
                    ensure!(!title.trim().is_empty(), "title cannot be empty");
                    spec.title.insert(locale.clone(), title.trim().to_owned());
                }
                Ok(())
            });
            let spec = match updated {
                Ok(spec) => spec,
                Err(error) => {
                    if let Some(path) = created {
                        let _ = fs::remove_file(path);
                    }
                    return Err(error);
                }
            };
            output.emit(
                "statement add",
                &spec.statements,
                &format!("Added {locale} statement"),
            )
        }
        StatementCommand::Open { locale } => {
            let root = reporch_cli::local_project::discover_project(Path::new("."))?;
            let spec = reporch_cli::local_project::read_authoring_spec(&root)?;
            let locale = locale.unwrap_or(spec.default_locale);
            let path = spec
                .statements
                .get(&locale)
                .with_context(|| format!("no statement for locale {locale}"))?;
            let file = spec
                .files
                .iter()
                .find(|file| file.path == *path)
                .with_context(|| format!("statement file is not declared: {path}"))?;
            let checked = checked_statement_path(&root, path, &file.media_type, file.executable)?;
            open::that(checked).context("open statement in the default application")?;
            output.emit(
                "statement open",
                &serde_json::json!({ "locale": locale, "path": path }),
                &format!("Opened {path}"),
            )
        }
        StatementCommand::Check => {
            let root = reporch_cli::local_project::discover_project(Path::new("."))?;
            let spec = reporch_cli::local_project::read_authoring_spec(&root)?;
            reporch_cli::local_project::validate_statement_documents(&root, &spec)?;
            for (locale, path) in &spec.statements {
                let file = spec
                    .files
                    .iter()
                    .find(|file| file.path == *path)
                    .with_context(|| format!("statement file is not declared: {path}"))?;
                let contents =
                    read_statement_markdown(&root, path, &file.media_type, file.executable)
                        .with_context(|| format!("read {locale} statement {path}"))?;
                ensure!(!contents.trim().is_empty(), "{locale} statement is empty");
            }
            output.emit(
                "statement check",
                &spec.statements,
                &format!("{} statement(s) are readable", spec.statements.len()),
            )
        }
        StatementCommand::Render {
            locale,
            render_format,
            output: destination,
        } => {
            let root = reporch_cli::local_project::discover_project(Path::new("."))?;
            let spec = reporch_cli::local_project::read_authoring_spec(&root)?;
            reporch_cli::local_project::validate_statement_documents(&root, &spec)?;
            let locale = locale.unwrap_or(spec.default_locale);
            let source = spec
                .statements
                .get(&locale)
                .with_context(|| format!("no statement for locale {locale}"))?;
            let file = spec
                .files
                .iter()
                .find(|file| file.path == *source)
                .with_context(|| format!("statement file is not declared: {source}"))?;
            let markdown =
                read_statement_markdown(&root, source, &file.media_type, file.executable)
                    .with_context(|| format!("read {locale} statement {source}"))?;
            let rendered = match render_format {
                StatementRenderFormat::Markdown => markdown,
                StatementRenderFormat::Latex => crate::statement_tex::markdown_to_tex(&markdown),
                StatementRenderFormat::Html => safe_statement_html(&markdown)?,
            };
            let destination = destination.as_deref().map(relative_string).transpose()?;
            if let Some(path) = &destination {
                write_project_bytes_atomic(&root, path, rendered.as_bytes())?;
            }
            let data = StatementRenderResult {
                locale,
                format: match render_format {
                    StatementRenderFormat::Markdown => "markdown",
                    StatementRenderFormat::Html => "html",
                    StatementRenderFormat::Latex => "latex",
                },
                source,
                output: destination,
                contents: rendered.clone(),
            };
            output.emit("statement render", &data, &rendered)
        }
    }
}

#[derive(Debug, Serialize)]
struct StatementRenderResult<'a> {
    locale: String,
    format: &'static str,
    source: &'a str,
    output: Option<String>,
    contents: String,
}

fn safe_statement_html(markdown: &str) -> Result<String> {
    studio_core::render_statement_html(markdown).map_err(|issues| {
        anyhow::anyhow!(
            "statement Markdown is unsafe: {}",
            issues
                .iter()
                .map(|issue| issue.message.as_str())
                .collect::<Vec<_>>()
                .join("; ")
        )
    })
}

pub fn tests(options: TestOptions, output: &CliOutput, no_input: bool) -> Result<()> {
    if v2::is_active_project()? {
        return v2::tests(options, output, no_input);
    }
    match options.command {
        None => guided_test_case(output, no_input),
        Some(TestCommand::Case { command }) => test_case(command, output),
        Some(TestCommand::Group { command }) => test_group(command, output),
    }
}

fn guided_test_case(output: &CliOutput, no_input: bool) -> Result<()> {
    ensure!(
        !no_input && std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        "test guide requires an interactive terminal; use test case add in CI"
    );
    let root = reporch_cli::local_project::discover_project(Path::new("."))?;
    let spec = reporch_cli::local_project::read_authoring_spec(&root)?;
    let name = prompt(
        "Test name",
        &next_test_case_name(spec.judging.tests.iter().map(|test| test.name.as_str())),
    )?;
    let input = prompt("Input data (single line)", "")?;
    let answer = prompt("Expected output (single line; blank for none)", "")?;
    test_case(
        TestCaseCommand::Add(TestCaseAddOptions {
            name,
            input: None,
            input_text: Some(guided_test_text(&input)),
            answer: None,
            answer_text: (!answer.is_empty()).then(|| guided_test_text(&answer)),
            groups: vec![],
            generated_by: None,
            seed: None,
        }),
        output,
    )
}

fn test_case(command: TestCaseCommand, output: &CliOutput) -> Result<()> {
    match command {
        TestCaseCommand::List => {
            let root = reporch_cli::local_project::discover_project(Path::new("."))?;
            let spec = reporch_cli::local_project::read_authoring_spec(&root)?;
            output.emit(
                "test case list",
                &spec.judging.tests,
                &format!("{} test case(s)", spec.judging.tests.len()),
            )
        }
        TestCaseCommand::Add(options) => {
            let test_id = Uuid::now_v7();
            let root = reporch_cli::local_project::discover_project(Path::new("."))?;
            let materialized = materialize_manual_case_files(&root, test_id, &options)?;
            let input = materialized.input.clone();
            let answer = materialized.answer.clone();
            let updated = reporch_cli::local_project::update_authoring_spec(&root, |root, spec| {
                ensure_unique_test_name(spec, &options.name, None)?;
                ensure_groups_exist(spec, &options.groups)?;
                if answer.is_some() {
                    ensure_unique_test_input(
                        root,
                        &input,
                        &options.name,
                        spec.judging
                            .tests
                            .iter()
                            .map(|test| (test.name.as_str(), test.input_file.as_str())),
                    )?;
                }
                if let Some(generator) = &options.generated_by {
                    ensure!(
                        spec.judging
                            .generators
                            .iter()
                            .any(|candidate| candidate.id == *generator),
                        "unknown generator: {generator}"
                    );
                }
                reporch_cli::local_project::declare_project_file(
                    root,
                    spec,
                    &input,
                    "text/plain",
                    false,
                )?;
                if let Some(answer) = &answer {
                    reporch_cli::local_project::declare_project_file(
                        root,
                        spec,
                        answer,
                        "text/plain",
                        false,
                    )?;
                }
                spec.judging.tests.push(TestCaseSpec {
                    id: test_id,
                    name: normalize_name(&options.name)?,
                    input_file: input.clone(),
                    answer_file: answer.clone(),
                    groups: options.groups.clone(),
                    generated_by: options.generated_by.clone(),
                    generator_arguments: vec![],
                    seed: options.seed,
                });
                Ok(())
            });
            let spec = match updated {
                Ok(spec) => spec,
                Err(error) => {
                    materialized.rollback();
                    return Err(error);
                }
            };
            output.emit(
                "test case add",
                &spec.judging.tests,
                &format!("Added test case {test_id}"),
            )
        }
        TestCaseCommand::Import(options) => import_test_cases(options, output),
        TestCaseCommand::Update(options) => {
            let spec = reporch_cli::local_project::update_authoring_spec(
                Path::new("."),
                |_root, spec| {
                    let test_id = find_legacy_test(spec, &options.selector)?.id;
                    ensure_groups_exist(spec, &options.groups)?;
                    if let Some(name) = &options.name {
                        ensure_unique_test_name(spec, name, Some(test_id))?;
                    }
                    let test = spec
                        .judging
                        .tests
                        .iter_mut()
                        .find(|test| test.id == test_id)
                        .context("test case was not found")?;
                    if let Some(name) = &options.name {
                        test.name = normalize_name(name)?;
                    }
                    if !options.groups.is_empty() {
                        test.groups.clone_from(&options.groups);
                    }
                    Ok(())
                },
            )?;
            output.emit(
                "test case update",
                &spec.judging.tests,
                &format!("Updated test case {}", options.selector),
            )
        }
        TestCaseCommand::Remove { selector } => {
            let spec = reporch_cli::local_project::update_authoring_spec(
                Path::new("."),
                |_root, spec| {
                    let id = find_legacy_test(spec, &selector)?.id;
                    let before = spec.judging.tests.len();
                    spec.judging.tests.retain(|test| test.id != id);
                    ensure!(
                        before != spec.judging.tests.len(),
                        "test case was not found"
                    );
                    for submission in &mut spec.output_submissions {
                        submission.outputs.remove(&id);
                    }
                    Ok(())
                },
            )?;
            output.emit(
                "test case remove",
                &spec.judging.tests,
                &format!("Removed test case {selector}"),
            )
        }
    }
}

fn import_test_cases(options: TestCaseImportOptions, output: &CliOutput) -> Result<()> {
    let root = reporch_cli::local_project::discover_project(Path::new("."))?;
    let directory = fs::canonicalize(&options.directory)
        .with_context(|| format!("resolve {}", options.directory.display()))?;
    ensure!(
        directory.starts_with(&root),
        "import directory must be inside the project"
    );
    let mut inputs = fs::read_dir(&directory)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|extension| extension == "in"))
        .collect::<Vec<_>>();
    inputs.sort();
    ensure!(!inputs.is_empty(), "no .in files were found");
    let mut imported = Vec::new();
    let spec = reporch_cli::local_project::update_authoring_spec(&root, |root, spec| {
        ensure_groups_exist(spec, &options.groups)?;
        for input_path in &inputs {
            let input = project_relative(root, input_path)?;
            let answer_path = input_path.with_extension("ans");
            let answer = answer_path
                .is_file()
                .then(|| project_relative(root, &answer_path))
                .transpose()?;
            let stem = input_path
                .file_stem()
                .and_then(|value| value.to_str())
                .context("test input has a non-Unicode file name")?;
            let name = normalize_name(stem)?;
            ensure_unique_test_name(spec, &name, None)?;
            if answer.is_some() {
                ensure_unique_test_input(
                    root,
                    &input,
                    &name,
                    spec.judging
                        .tests
                        .iter()
                        .map(|test| (test.name.as_str(), test.input_file.as_str())),
                )?;
            }
            reporch_cli::local_project::declare_project_file(
                root,
                spec,
                &input,
                "text/plain",
                false,
            )?;
            if let Some(answer) = &answer {
                reporch_cli::local_project::declare_project_file(
                    root,
                    spec,
                    answer,
                    "text/plain",
                    false,
                )?;
            }
            let id = Uuid::now_v7();
            imported.push(id);
            spec.judging.tests.push(TestCaseSpec {
                id,
                name,
                input_file: input,
                answer_file: answer,
                groups: options.groups.clone(),
                generated_by: None,
                generator_arguments: vec![],
                seed: None,
            });
        }
        Ok(())
    })?;
    output.emit(
        "test case import",
        &serde_json::json!({ "imported_ids": imported, "tests": spec.judging.tests }),
        &format!("Imported {} test case(s)", imported.len()),
    )
}

fn test_group(command: TestGroupCommand, output: &CliOutput) -> Result<()> {
    match command {
        TestGroupCommand::List => {
            let root = reporch_cli::local_project::discover_project(Path::new("."))?;
            let spec = reporch_cli::local_project::read_authoring_spec(&root)?;
            output.emit(
                "test group list",
                &spec.judging.groups,
                &format!("{} group(s)", spec.judging.groups.len()),
            )
        }
        TestGroupCommand::Add(options) => {
            validate_group_points(options.points)?;
            let spec = reporch_cli::local_project::update_authoring_spec(
                Path::new("."),
                |_root, spec| {
                    validate_group_id(&options.id)?;
                    ensure!(
                        !spec
                            .judging
                            .groups
                            .iter()
                            .any(|group| group.id == options.id),
                        "group already exists: {}",
                        options.id
                    );
                    ensure_groups_exist(spec, &options.depends_on)?;
                    spec.judging.groups.push(TestGroupSpec {
                        id: options.id.clone(),
                        points: options.points,
                        depends_on: options.depends_on.clone(),
                        feedback_policy: studio_core::GroupFeedbackPolicyV1::Complete,
                    });
                    ensure_v1_group_dependencies_acyclic(&spec.judging.groups)?;
                    Ok(())
                },
            )?;
            output.emit(
                "test group add",
                &spec.judging.groups,
                &group_points_feedback_v1(
                    spec.problem_type,
                    &spec.judging.groups,
                    &format!("Added group {}", options.id),
                    &options.id,
                ),
            )
        }
        TestGroupCommand::Update(options) => {
            if let Some(points) = options.points {
                validate_group_points(points)?;
            }
            let spec = reporch_cli::local_project::update_authoring_spec(
                Path::new("."),
                |_root, spec| {
                    ensure!(
                        options.depends_on.iter().all(|id| id != &options.id),
                        "a group cannot depend on itself"
                    );
                    ensure_groups_exist(spec, &options.depends_on)?;
                    let group = spec
                        .judging
                        .groups
                        .iter_mut()
                        .find(|group| group.id == options.id)
                        .context("group was not found")?;
                    if let Some(points) = options.points {
                        group.points = points;
                    }
                    if !options.depends_on.is_empty() {
                        group.depends_on.clone_from(&options.depends_on);
                    }
                    ensure_v1_group_dependencies_acyclic(&spec.judging.groups)?;
                    Ok(())
                },
            )?;
            output.emit(
                "test group update",
                &spec.judging.groups,
                &group_points_feedback_v1(
                    spec.problem_type,
                    &spec.judging.groups,
                    &format!("Updated group {}", options.id),
                    &options.id,
                ),
            )
        }
        TestGroupCommand::Remove { id } => {
            let spec = reporch_cli::local_project::update_authoring_spec(
                Path::new("."),
                |_root, spec| {
                    ensure!(
                        !spec
                            .judging
                            .tests
                            .iter()
                            .any(|test| test.groups.iter().any(|group| group == &id)),
                        "group is still used by a test case"
                    );
                    ensure!(
                        !spec.judging.groups.iter().any(|group| group
                            .depends_on
                            .iter()
                            .any(|dependency| dependency == &id)),
                        "another group still depends on this group"
                    );
                    let before = spec.judging.groups.len();
                    spec.judging.groups.retain(|group| group.id != id);
                    ensure!(before != spec.judging.groups.len(), "group was not found");
                    Ok(())
                },
            )?;
            output.emit(
                "test group remove",
                &spec.judging.groups,
                &format!("Removed group {id}"),
            )
        }
    }
}

pub async fn generator(options: GeneratorOptions, output: &CliOutput) -> Result<()> {
    if v2::is_active_project()? {
        return v2::generator(options, output).await;
    }
    match options.command {
        GeneratorCommand::List => {
            let root = reporch_cli::local_project::discover_project(Path::new("."))?;
            let spec = reporch_cli::local_project::read_authoring_spec(&root)?;
            output.emit(
                "generator list",
                &spec.judging.generators,
                &format!("{} generator(s)", spec.judging.generators.len()),
            )
        }
        GeneratorCommand::Add(options) => {
            let source = relative_string(&options.source)?;
            let spec =
                reporch_cli::local_project::update_authoring_spec(Path::new("."), |root, spec| {
                    ensure!(
                        !spec
                            .judging
                            .generators
                            .iter()
                            .any(|generator| generator.id == options.id),
                        "generator already exists: {}",
                        options.id
                    );
                    reporch_cli::local_project::declare_project_file(
                        root,
                        spec,
                        &source,
                        source_media_type(&options.language),
                        false,
                    )?;
                    spec.judging.generators.push(ProgramSpec {
                        id: options.id.clone(),
                        source_path: source.clone(),
                        language: options.language.clone(),
                        arguments: options.arguments.clone(),
                    });
                    Ok(())
                })?;
            output.emit(
                "generator add",
                &spec.judging.generators,
                &format!("Added generator {}", options.id),
            )
        }
        GeneratorCommand::Run(options) => {
            let root = reporch_cli::local_project::discover_project(Path::new("."))?;
            let spec = reporch_cli::local_project::read_authoring_spec(&root)?;
            ensure_groups_exist(&spec, &options.groups)?;
            let generator = spec
                .judging
                .generators
                .iter()
                .find(|generator| generator.id == options.id)
                .with_context(|| format!("generator was not found: {}", options.id))?
                .clone();
            let path = relative_string(&options.output)?;
            let name = normalize_name(options.name.as_deref().unwrap_or(&options.id))?;
            ensure_unique_test_name(&spec, &name, None)?;
            let run_options = options.runtime.into_run_options(output);
            let bytes = materialize_generator(
                &root,
                &generator,
                &options.arguments,
                options.seed,
                &run_options,
            )
            .await?;
            write_project_bytes_atomic(&root, &path, &bytes)?;
            let test_id = Uuid::now_v7();
            let updated =
                reporch_cli::local_project::update_authoring_spec(&root, |root, spec| {
                    ensure_unique_test_name(spec, &name, None)?;
                    ensure_groups_exist(spec, &options.groups)?;
                    reporch_cli::local_project::declare_project_file(
                        root,
                        spec,
                        &path,
                        "text/plain",
                        false,
                    )?;
                    spec.judging.tests.push(TestCaseSpec {
                        id: test_id,
                        name: name.clone(),
                        input_file: path.clone(),
                        answer_file: None,
                        groups: options.groups.clone(),
                        generated_by: Some(generator.id.clone()),
                        generator_arguments: options.arguments.clone(),
                        seed: options.seed,
                    });
                    Ok(())
                })?;
            let result = GeneratorMaterialization {
                generator_id: generator.id,
                test_ids: vec![test_id],
                paths: vec![path],
                sha256: vec![hex::encode(Sha256::digest(&bytes))],
            };
            output.emit(
                "generator run",
                &result,
                &format!(
                    "Generated {} ({})",
                    updated.judging.tests.last().unwrap().name,
                    test_id
                ),
            )
        }
        GeneratorCommand::Recipe(options) => {
            ensure!(
                (1..=10_000).contains(&options.count),
                "recipe count must be between 1 and 10000"
            );
            let root = reporch_cli::local_project::discover_project(Path::new("."))?;
            let spec = reporch_cli::local_project::read_authoring_spec(&root)?;
            ensure_groups_exist(&spec, &options.groups)?;
            let generator = spec
                .judging
                .generators
                .iter()
                .find(|generator| generator.id == options.id)
                .with_context(|| format!("generator was not found: {}", options.id))?
                .clone();
            let prefix = normalize_name(&options.name_prefix)?;
            let directory = relative_string(&options.output_directory)?;
            let run_options = options.runtime.into_run_options(output);
            let mut materialized = Vec::with_capacity(options.count as usize);
            for index in 0..options.count {
                let seed = options
                    .seed_start
                    .checked_add(u64::from(index))
                    .context("recipe seed range overflows u64")?;
                let name = format!("{prefix}-{}", index + 1);
                ensure_unique_test_name(&spec, &name, None)?;
                let path = format!("{directory}/{}.in", index + 1);
                let bytes = materialize_generator(
                    &root,
                    &generator,
                    &options.arguments,
                    Some(seed),
                    &run_options,
                )
                .await?;
                materialized.push((Uuid::now_v7(), name, path, seed, bytes));
            }
            for (_, _, path, _, bytes) in &materialized {
                write_project_bytes_atomic(&root, path, bytes)?;
            }
            reporch_cli::local_project::update_authoring_spec(&root, |root, spec| {
                ensure_groups_exist(spec, &options.groups)?;
                for (id, name, path, seed, _) in &materialized {
                    ensure_unique_test_name(spec, name, None)?;
                    reporch_cli::local_project::declare_project_file(
                        root,
                        spec,
                        path,
                        "text/plain",
                        false,
                    )?;
                    spec.judging.tests.push(TestCaseSpec {
                        id: *id,
                        name: name.clone(),
                        input_file: path.clone(),
                        answer_file: None,
                        groups: options.groups.clone(),
                        generated_by: Some(generator.id.clone()),
                        generator_arguments: options.arguments.clone(),
                        seed: Some(*seed),
                    });
                }
                Ok(())
            })?;
            let result = GeneratorMaterialization {
                generator_id: generator.id,
                test_ids: materialized.iter().map(|entry| entry.0).collect(),
                paths: materialized.iter().map(|entry| entry.2.clone()).collect(),
                sha256: materialized
                    .iter()
                    .map(|entry| hex::encode(Sha256::digest(&entry.4)))
                    .collect(),
            };
            output.emit(
                "generator recipe",
                &result,
                &format!("Generated {} deterministic test case(s)", options.count),
            )
        }
        GeneratorCommand::Remove { id } => {
            let spec = reporch_cli::local_project::update_authoring_spec(
                Path::new("."),
                |_root, spec| {
                    ensure!(
                        !spec
                            .judging
                            .tests
                            .iter()
                            .any(|test| test.generated_by.as_deref() == Some(&id)),
                        "generator is still used by a test case"
                    );
                    let before = spec.judging.generators.len();
                    spec.judging
                        .generators
                        .retain(|generator| generator.id != id);
                    ensure!(
                        before != spec.judging.generators.len(),
                        "generator was not found"
                    );
                    Ok(())
                },
            )?;
            output.emit(
                "generator remove",
                &spec.judging.generators,
                &format!("Removed generator {id}"),
            )
        }
    }
}

#[derive(Debug, Serialize)]
struct GeneratorMaterialization {
    generator_id: String,
    test_ids: Vec<Uuid>,
    paths: Vec<String>,
    sha256: Vec<String>,
}

async fn materialize_generator(
    root: &Path,
    generator: &ProgramSpec,
    arguments: &[String],
    seed: Option<u64>,
    options: &reporch_cli::authoring_runtime::AuthoringRunOptions,
) -> Result<Vec<u8>> {
    let mut run_arguments = generator.arguments.clone();
    run_arguments.extend(arguments.iter().cloned());
    if let Some(seed) = seed {
        run_arguments.push(seed.to_string());
    }
    let request = reporch_cli::authoring_runtime::ProgramRequest {
        project_directory: root,
        source_path: &generator.source_path,
        language: &generator.language,
        arguments: &run_arguments,
        stdin_path: None,
        options,
    };
    let first = reporch_cli::authoring_runtime::run_program(&request).await?;
    ensure!(
        first.exit_code == 0,
        "generator validation did not pass: exited with {}: {}",
        first.exit_code,
        first.stderr.trim()
    );
    let second = reporch_cli::authoring_runtime::run_program(&request).await?;
    ensure!(
        second.exit_code == 0,
        "generator validation did not pass: repeat exited with {}: {}",
        second.exit_code,
        second.stderr.trim()
    );
    ensure!(
        first.stdout_bytes == second.stdout_bytes,
        "generator validation did not pass: fixed-seed output was not deterministic"
    );
    Ok(first.stdout_bytes)
}

pub async fn validator(options: ValidatorOptions, output: &CliOutput) -> Result<()> {
    if v2::is_active_project()? {
        return v2::validator(options, output).await;
    }
    match options.command {
        ValidatorCommand::Set {
            source,
            language,
            extra,
        } => {
            let source = relative_string(&source)?;
            let spec =
                reporch_cli::local_project::update_authoring_spec(Path::new("."), |root, spec| {
                    reporch_cli::local_project::declare_project_file(
                        root,
                        spec,
                        &source,
                        source_media_type(&language),
                        false,
                    )?;
                    if extra {
                        let id = format!("extra-{}", spec.judging.extra_validators.len() + 1);
                        spec.judging.extra_validators.push(ProgramSpec {
                            id,
                            source_path: source.clone(),
                            language: language.clone(),
                            arguments: vec![],
                        });
                    } else {
                        if source != "validators/input.py"
                            && spec.judging.validator_path.as_deref() == Some("validators/input.py")
                        {
                            spec.judging.validator_tests.retain(|unit| {
                                !is_starter_validator_unit(
                                    &unit.name,
                                    &unit.input_file,
                                    unit.expected_valid,
                                )
                            });
                        }
                        spec.judging.validator_path = Some(source.clone());
                        spec.judging.validator_language = Some(language.clone());
                    }
                    Ok(())
                })?;
            output.emit(
                "validator set",
                &spec.judging.validator_path,
                &format!("Configured validator {source}"),
            )
        }
        ValidatorCommand::UnitAdd(options) => {
            let root = reporch_cli::local_project::discover_project(Path::new("."))?;
            let materialized = materialize_validator_unit_input(&root, &options)?;
            let input = materialized.path.clone();
            let updated = reporch_cli::local_project::update_authoring_spec(&root, |root, spec| {
                reporch_cli::local_project::declare_project_file(
                    root,
                    spec,
                    &input,
                    "text/plain",
                    false,
                )?;
                spec.judging.validator_tests.push(ValidatorTestSpec {
                    name: normalize_name(&options.name)?,
                    input_file: input.clone(),
                    expected_valid: matches!(options.expected, ValidityExpectation::Valid),
                });
                Ok(())
            });
            let spec = match updated {
                Ok(spec) => spec,
                Err(error) => {
                    materialized.rollback();
                    return Err(error);
                }
            };
            output.emit(
                "validator unit-add",
                &spec.judging.validator_tests,
                &format!("Added validator unit {}", options.name),
            )
        }
        ValidatorCommand::Run { name, runtime } => {
            let root = reporch_cli::local_project::discover_project(Path::new("."))?;
            let spec = reporch_cli::local_project::read_authoring_spec(&root)?;
            let mut validators = Vec::new();
            if let (Some(source_path), Some(language)) = (
                spec.judging.validator_path.clone(),
                spec.judging.validator_language.clone(),
            ) {
                validators.push(ProgramSpec {
                    id: "primary".into(),
                    source_path,
                    language,
                    arguments: vec![],
                });
            }
            validators.extend(spec.judging.extra_validators.clone());
            ensure!(!validators.is_empty(), "no validator is configured");
            let units = selected_by_name(&spec.judging.validator_tests, name.as_deref(), |unit| {
                unit.name.as_str()
            })?;
            ensure!(!units.is_empty(), "no validator unit tests are configured");
            let run_options = runtime.into_run_options(output);
            let mut cases = Vec::new();
            for validator in &validators {
                for unit in &units {
                    output.progress(
                        "validator run",
                        &format!("Running validator {} · unit {}", validator.id, unit.name),
                    );
                    let result = reporch_cli::authoring_runtime::run_program(
                        &reporch_cli::authoring_runtime::ProgramRequest {
                            project_directory: &root,
                            source_path: &validator.source_path,
                            language: &validator.language,
                            arguments: &validator.arguments,
                            stdin_path: Some(&unit.input_file),
                            options: &run_options,
                        },
                    )
                    .await?;
                    let exited =
                        result.termination == reporch_runtime_core::GuestTerminationV2::Exited;
                    let actual_valid = exited && result.exit_code == 0;
                    cases.push(ProgramUnitResult {
                        program: validator.id.clone(),
                        name: unit.name.clone(),
                        expected: if unit.expected_valid {
                            "valid"
                        } else {
                            "invalid"
                        },
                        actual: if exited {
                            if actual_valid { "valid" } else { "invalid" }
                        } else {
                            termination_name(result.termination)
                        },
                        passed: exited && actual_valid == unit.expected_valid,
                        exit_code: result.exit_code,
                        termination: result.termination,
                        duration_ms: result.duration_ms,
                        stdout: result.stdout,
                        stderr: result.stderr,
                    });
                }
            }
            emit_unit_report("validator run", cases, output)
        }
    }
}

pub async fn checker(options: CheckerOptions, output: &CliOutput) -> Result<()> {
    if v2::is_active_project()? {
        return v2::checker(options, output).await;
    }
    match options.command {
        CheckerCommand::ListStandard => output.emit(
            "checker list-standard",
            &["exact", "token", "case-insensitive", "floating", "custom"],
            "exact, token, case-insensitive, floating, custom",
        ),
        CheckerCommand::Protocol { command } => match command {
            CheckerProtocolCommand::Show => {
                let root = reporch_cli::local_project::discover_project(Path::new("."))?;
                let spec = reporch_cli::local_project::read_authoring_spec(&root)?;
                let CheckerSpec::Custom { protocol, .. } = spec.judging.checker else {
                    bail!("checker protocol is available only for a custom checker")
                };
                output.emit(
                    "checker protocol show",
                    &protocol,
                    &format!("Custom checker protocol: {protocol:?}"),
                )
            }
            CheckerProtocolCommand::Set { protocol } => {
                let protocol = studio_core::CheckerProtocolV1::from(protocol);
                let spec = reporch_cli::local_project::update_authoring_spec(
                    Path::new("."),
                    |_, spec| {
                        let CheckerSpec::Custom {
                            protocol: current, ..
                        } = &mut spec.judging.checker
                        else {
                            bail!("checker protocol is available only for a custom checker")
                        };
                        *current = protocol;
                        Ok(())
                    },
                )?;
                output.emit(
                    "checker protocol set",
                    &spec.judging.checker,
                    &format!("Custom checker protocol set to {protocol:?}"),
                )
            }
        },
        CheckerCommand::Set {
            kind,
            source,
            language,
            absolute_error,
            relative_error,
        } => {
            let source = source.as_deref().map(relative_string).transpose()?;
            let checker = match kind {
                CheckerKind::Exact => CheckerSpec::Exact,
                CheckerKind::Token => CheckerSpec::Token,
                CheckerKind::CaseInsensitive => CheckerSpec::CaseInsensitive,
                CheckerKind::Floating => {
                    let absolute_error = absolute_error.context("--absolute-error is required")?;
                    let relative_error = relative_error.context("--relative-error is required")?;
                    validate_floating_tolerances(&absolute_error, &relative_error)?;
                    CheckerSpec::Floating {
                        absolute_error,
                        relative_error,
                    }
                }
                CheckerKind::Custom => CheckerSpec::Custom {
                    source_path: source.clone().context("--source is required")?,
                    language: language.clone().context("--language is required")?,
                    protocol: studio_core::CheckerProtocolV1::Icpc202509,
                },
            };
            let spec =
                reporch_cli::local_project::update_authoring_spec(Path::new("."), |root, spec| {
                    if let (Some(source), Some(language)) = (&source, &language) {
                        reporch_cli::local_project::declare_project_file(
                            root,
                            spec,
                            source,
                            source_media_type(language),
                            false,
                        )?;
                    }
                    spec.judging.checker = checker.clone();
                    Ok(())
                })?;
            output.emit("checker set", &spec.judging.checker, "Configured checker")
        }
        CheckerCommand::UnitAdd {
            name,
            input,
            answer,
            output: actual_output,
            expected,
        } => {
            let input = relative_string(&input)?;
            let answer = relative_string(&answer)?;
            let actual_output = relative_string(&actual_output)?;
            let spec =
                reporch_cli::local_project::update_authoring_spec(Path::new("."), |root, spec| {
                    for path in [&input, &answer, &actual_output] {
                        reporch_cli::local_project::declare_project_file(
                            root,
                            spec,
                            path,
                            "text/plain",
                            false,
                        )?;
                    }
                    spec.judging.checker_tests.push(CheckerTestSpec {
                        name: normalize_name(&name)?,
                        input_file: input.clone(),
                        answer_file: answer.clone(),
                        output_file: actual_output.clone(),
                        expected_accepted: matches!(expected, CheckerExpectation::Accept),
                    });
                    Ok(())
                })?;
            output.emit(
                "checker unit-add",
                &spec.judging.checker_tests,
                &format!("Added checker unit {name}"),
            )
        }
        CheckerCommand::Run { name, runtime } => {
            let root = reporch_cli::local_project::discover_project(Path::new("."))?;
            let spec = reporch_cli::local_project::read_authoring_spec(&root)?;
            let units = selected_by_name(&spec.judging.checker_tests, name.as_deref(), |unit| {
                unit.name.as_str()
            })?;
            ensure!(
                !units.is_empty(),
                "no checker unit tests are configured. Add one with `reporch checker unit-add --name accepts-sample --input tests/1.in --answer tests/1.ans --output tests/1.ans --expected accept`, then run `reporch checker test`"
            );
            let run_options = runtime.into_run_options(output);
            let mut cases = Vec::new();
            for unit in units {
                output.progress("checker run", &format!("Checking unit {}", unit.name));
                let (actual, passed, exit_code, termination, duration_ms, stdout, stderr) =
                    if let CheckerSpec::Custom {
                        source_path,
                        language,
                        protocol,
                    } = &spec.judging.checker
                    {
                        let result = reporch_cli::authoring_runtime::run_custom_checker(
                            &root,
                            source_path,
                            language,
                            *protocol,
                            &unit.input_file,
                            &unit.answer_file,
                            &unit.output_file,
                            &run_options,
                        )
                        .await?;
                        let exited = result.execution.termination
                            == reporch_runtime_core::GuestTerminationV2::Exited;
                        let actual = if exited {
                            match result.verdict {
                                reporch_cli::authoring_runtime::CustomCheckerVerdict::Accepted => {
                                    "accepted"
                                }
                                reporch_cli::authoring_runtime::CustomCheckerVerdict::WrongAnswer => {
                                    "rejected"
                                }
                                reporch_cli::authoring_runtime::CustomCheckerVerdict::JudgeError => {
                                    "judge_error"
                                }
                            }
                        } else {
                            termination_name(result.execution.termination)
                        };
                        let passed = exited && match result.verdict {
                            reporch_cli::authoring_runtime::CustomCheckerVerdict::Accepted => {
                                unit.expected_accepted
                            }
                            reporch_cli::authoring_runtime::CustomCheckerVerdict::WrongAnswer => {
                                !unit.expected_accepted
                            }
                            reporch_cli::authoring_runtime::CustomCheckerVerdict::JudgeError => {
                                false
                            }
                        };
                        (
                            actual,
                            passed,
                            result.execution.exit_code,
                            result.execution.termination,
                            result.execution.duration_ms,
                            result.execution.stdout,
                            result.execution.stderr,
                        )
                    } else {
                        let answer = read_project_bytes(&root, &unit.answer_file)?;
                        let actual = read_project_bytes(&root, &unit.output_file)?;
                        let accepted = reporch_cli::authoring_runtime::standard_checker_matches(
                            &spec.judging.checker,
                            &answer,
                            &actual,
                        )?;
                        (
                            if accepted { "accepted" } else { "rejected" },
                            accepted == unit.expected_accepted,
                            0,
                            reporch_runtime_core::GuestTerminationV2::Exited,
                            0,
                            String::new(),
                            String::new(),
                        )
                    };
                cases.push(ProgramUnitResult {
                    program: "checker".into(),
                    name: unit.name.clone(),
                    expected: if unit.expected_accepted {
                        "accepted"
                    } else {
                        "rejected"
                    },
                    actual,
                    passed,
                    exit_code,
                    termination,
                    duration_ms,
                    stdout,
                    stderr,
                });
            }
            emit_unit_report("checker run", cases, output)
        }
    }
}

pub fn solution(options: SolutionOptions, output: &CliOutput) -> Result<()> {
    if v2::is_active_project()? {
        return v2::solution(options, output);
    }
    match options.command {
        SolutionCommand::List => {
            let root = reporch_cli::local_project::discover_project(Path::new("."))?;
            let spec = reporch_cli::local_project::read_authoring_spec(&root)?;
            output.emit(
                "solution list",
                &spec.solutions,
                &format!("{} solution expectation(s)", spec.solutions.len()),
            )
        }
        SolutionCommand::Matrix => {
            let root = reporch_cli::local_project::discover_project(Path::new("."))?;
            let spec = reporch_cli::local_project::read_authoring_spec(&root)?;
            output.emit(
                "solution matrix",
                &spec.solutions,
                &legacy_solution_matrix_human(&spec.solutions),
            )
        }
        SolutionCommand::Add(options) => {
            ensure!(
                options.role.is_none(),
                "solution roles require AuthoringSpecV2; run `reporch migrate --yes` first"
            );
            let source = relative_string(&options.source)?;
            let expected_score = score_range(
                options.minimum_score,
                options.maximum_score,
                options.expected,
            )?;
            let spec =
                reporch_cli::local_project::update_authoring_spec(Path::new("."), |root, spec| {
                    ensure!(
                        !spec
                            .solutions
                            .iter()
                            .any(|solution| solution.name == options.name),
                        "solution already exists: {}",
                        options.name
                    );
                    reporch_cli::local_project::declare_project_file(
                        root,
                        spec,
                        &source,
                        source_media_type(&options.language),
                        false,
                    )?;
                    spec.solutions.push(SolutionSpec {
                        name: normalize_name(&options.name)?,
                        source_path: source.clone(),
                        language: options.language.clone(),
                        expected_verdict: options.expected.into(),
                        expected_score: expected_score.clone(),
                    });
                    Ok(())
                })?;
            output.emit(
                "solution add",
                &spec.solutions,
                &format!("Added solution {}", options.name),
            )
        }
        SolutionCommand::Update(options) => {
            ensure!(
                options.role.is_none(),
                "solution roles require AuthoringSpecV2; run `reporch migrate --yes` first"
            );
            let score_was_supplied =
                options.minimum_score.is_some() || options.maximum_score.is_some();
            let spec = reporch_cli::local_project::update_authoring_spec(
                Path::new("."),
                |_root, spec| {
                    let solution = spec
                        .solutions
                        .iter_mut()
                        .find(|solution| solution.name == options.name)
                        .context("solution was not found")?;
                    let expected_verdict = options
                        .expected
                        .map(Into::into)
                        .unwrap_or(solution.expected_verdict);
                    if options.expected.is_some() || score_was_supplied {
                        solution.expected_score = score_range_for_verdict(
                            options.minimum_score,
                            options.maximum_score,
                            expected_verdict,
                        )?;
                    }
                    solution.expected_verdict = expected_verdict;
                    Ok(())
                },
            )?;
            output.emit(
                "solution update",
                &spec.solutions,
                &format!("Updated solution {}", options.name),
            )
        }
        SolutionCommand::Remove { name } => {
            let spec = reporch_cli::local_project::update_authoring_spec(
                Path::new("."),
                |_root, spec| {
                    let before = spec.solutions.len();
                    spec.solutions.retain(|solution| solution.name != name);
                    ensure!(before != spec.solutions.len(), "solution was not found");
                    Ok(())
                },
            )?;
            output.emit(
                "solution remove",
                &spec.solutions,
                &format!("Removed solution {name}"),
            )
        }
    }
}

pub async fn answer(options: AnswerOptions, output: &CliOutput) -> Result<()> {
    ensure!(
        v2::is_active_project()?,
        "answer generation requires AuthoringSpecV2; run `reporch migrate --yes`"
    );
    v2::answer(options, output).await
}

pub async fn stress(options: StressOptions, output: &CliOutput) -> Result<()> {
    ensure!(
        v2::is_active_project()?,
        "stress testing requires AuthoringSpecV2; run `reporch migrate --yes`"
    );
    v2::stress(options, output).await
}

pub async fn interactor(options: InteractorOptions, output: &CliOutput) -> Result<()> {
    if v2::is_active_project()? {
        return v2::interactor(options, output).await;
    }
    match options.command {
        InteractorCommand::Set { source, language } => {
            set_runtime_program(source, language, true, output)
        }
        InteractorCommand::Run(options) => run_interactor(options, false, output).await,
        InteractorCommand::Transcript(options) => run_interactor(options, true, output).await,
    }
}

pub async fn grader(options: GraderOptions, output: &CliOutput) -> Result<()> {
    if v2::is_active_project()? {
        return v2::grader(options, output).await;
    }
    match options.command {
        GraderCommand::Set {
            source, language, ..
        } => set_runtime_program(source, language, false, output),
        GraderCommand::Run(options) => run_grader(options, output).await,
    }
}

async fn run_interactor(
    options: RuntimeProgramRunOptions,
    transcript: bool,
    output: &CliOutput,
) -> Result<()> {
    let root = reporch_cli::local_project::discover_project(Path::new("."))?;
    let spec = reporch_cli::local_project::read_authoring_spec(&root)?;
    let interactor_path = spec
        .judging
        .interactor_path
        .as_deref()
        .context("no interactor is configured")?;
    let interactor_language = spec
        .judging
        .interactor_language
        .as_deref()
        .context("configured interactor has no language")?;
    let solution = find_legacy_solution(&spec, &options.solution)?;
    let test = find_legacy_test(&spec, &options.test)?;
    let run_options = options.runtime.into_run_options(output);
    let interactor_toolchain = reporch_cli::toolchain::resolve_for_language(
        run_options.toolchain_id.as_deref(),
        interactor_language,
    )?;
    let solution_toolchain = reporch_cli::toolchain::resolve_for_language(
        run_options.toolchain_id.as_deref(),
        &solution.language,
    )?;
    ensure!(
        interactor_toolchain.language == solution_toolchain.language,
        "local interactive pairing requires matching toolchain languages; use Studio verification for cross-language pairing"
    );
    let result = reporch_cli::authoring_runtime::run_interactive_pair(
        &reporch_cli::authoring_runtime::InteractivePairRequest {
            project_directory: &root,
            solver_source_path: &solution.source_path,
            interactor_source_path: interactor_path,
            language: interactor_language,
            input_path: &test.input_file,
            idle_timeout: run_options.timeout,
            options: &run_options,
        },
    )
    .await?;
    if let Some(path) = options.output.as_deref() {
        let path = relative_string(path)?;
        write_project_bytes_atomic(&root, &path, &result.stdout_bytes)?;
    }
    let actual_verdict = interactive_execution_verdict(&result);
    let transcript_value = transcript.then(|| result.stdout.clone());
    let report = RuntimeProgramReport {
        solution: solution.name.clone(),
        test_id: test.id,
        expected: verdict_name(solution.expected_verdict),
        actual: observed_verdict_name(actual_verdict),
        passed: actual_verdict == Some(solution.expected_verdict),
        exit_code: result.exit_code,
        termination: result.termination,
        duration_ms: result.duration_ms,
        stdout: result.stdout,
        transcript: transcript_value,
        stderr: result.stderr,
    };
    if !report.passed {
        return Err(crate::cli_output::domain_error(
            "operation.failed",
            format!(
                "interactive validation did not pass: expected {}, got {}",
                report.expected, report.actual
            ),
            &report,
        ));
    }
    output.emit(
        if transcript {
            "interactor transcript"
        } else {
            "interactor run"
        },
        &report,
        if transcript {
            report.transcript.as_deref().unwrap_or("")
        } else {
            "Interactive run matched the expected verdict"
        },
    )
}

async fn run_grader(options: RuntimeProgramRunOptions, output: &CliOutput) -> Result<()> {
    let root = reporch_cli::local_project::discover_project(Path::new("."))?;
    let spec = reporch_cli::local_project::read_authoring_spec(&root)?;
    let grader_path = spec
        .judging
        .grader_path
        .as_deref()
        .context("no grader is configured")?;
    let grader_language = spec
        .judging
        .grader_language
        .as_deref()
        .context("configured grader has no language")?;
    let solution = find_legacy_solution(&spec, &options.solution)?;
    ensure!(
        reporch_cli::toolchain::resolve_for_language(None, grader_language)?.language
            == reporch_cli::toolchain::resolve_for_language(None, &solution.language)?.language,
        "local grader linking requires the solution and grader to use the same C or C++ toolchain"
    );
    let test = find_legacy_test(&spec, &options.test)?;
    let answer_path = test
        .answer_file
        .as_deref()
        .context("grader test has no answer file")?;
    let run_options = options.runtime.into_run_options(output);
    let result = reporch_cli::authoring_runtime::run_linked_pair(
        &reporch_cli::authoring_runtime::LinkedPairRequest {
            project_directory: &root,
            first_source_path: &solution.source_path,
            second_source_path: grader_path,
            language: grader_language,
            stdin_path: &test.input_file,
            options: &run_options,
        },
    )
    .await?;
    let checker_accepted = if result.termination == reporch_runtime_core::GuestTerminationV2::Exited
        && result.exit_code == 0
    {
        checker_accepts_bytes(
            &root,
            &spec.judging.checker,
            &test.input_file,
            answer_path,
            &result.stdout_bytes,
            &run_options,
        )
        .await?
    } else {
        false
    };
    if let Some(path) = options.output.as_deref() {
        let path = relative_string(path)?;
        write_project_bytes_atomic(&root, &path, &result.stdout_bytes)?;
    }
    let actual_verdict = program_execution_verdict(&result, checker_accepted);
    let report = RuntimeProgramReport {
        solution: solution.name.clone(),
        test_id: test.id,
        expected: verdict_name(solution.expected_verdict),
        actual: observed_verdict_name(actual_verdict),
        passed: actual_verdict == Some(solution.expected_verdict),
        exit_code: result.exit_code,
        termination: result.termination,
        duration_ms: result.duration_ms,
        stdout: result.stdout,
        transcript: None,
        stderr: result.stderr,
    };
    if !report.passed {
        return Err(crate::cli_output::domain_error(
            "operation.failed",
            format!(
                "grader validation did not pass: expected {}, got {}",
                report.expected, report.actual
            ),
            &report,
        ));
    }
    output.emit(
        "grader run",
        &report,
        "Grader run matched the expected verdict",
    )
}

#[derive(Debug, Serialize)]
struct RuntimeProgramReport {
    solution: String,
    test_id: Uuid,
    expected: &'static str,
    actual: &'static str,
    passed: bool,
    exit_code: i32,
    termination: reporch_runtime_core::GuestTerminationV2,
    duration_ms: u128,
    stdout: String,
    transcript: Option<String>,
    stderr: String,
}

fn program_execution_verdict(
    result: &reporch_cli::local_sandbox::LocalSandboxResult,
    checker_accepted: bool,
) -> Option<ExpectedVerdict> {
    if reporch_cli::authoring_runtime::compilation_failed(result) {
        return None;
    }
    match result.termination {
        reporch_runtime_core::GuestTerminationV2::Exited if result.exit_code == 0 => {
            Some(if checker_accepted {
                ExpectedVerdict::Accepted
            } else {
                ExpectedVerdict::WrongAnswer
            })
        }
        reporch_runtime_core::GuestTerminationV2::Exited => Some(ExpectedVerdict::RuntimeError),
        reporch_runtime_core::GuestTerminationV2::TimedOut => Some(ExpectedVerdict::TimeLimit),
        reporch_runtime_core::GuestTerminationV2::Signalled
        | reporch_runtime_core::GuestTerminationV2::OutputLimit => {
            Some(ExpectedVerdict::RuntimeError)
        }
        reporch_runtime_core::GuestTerminationV2::InternalError => None,
    }
}

fn interactive_execution_verdict(
    result: &reporch_cli::local_sandbox::LocalSandboxResult,
) -> Option<ExpectedVerdict> {
    if reporch_cli::authoring_runtime::compilation_failed(result) {
        return None;
    }
    match result.termination {
        reporch_runtime_core::GuestTerminationV2::Exited if result.exit_code == 0 => {
            Some(ExpectedVerdict::Accepted)
        }
        reporch_runtime_core::GuestTerminationV2::Exited if result.exit_code == 1 => {
            Some(ExpectedVerdict::WrongAnswer)
        }
        reporch_runtime_core::GuestTerminationV2::Exited
        | reporch_runtime_core::GuestTerminationV2::InternalError => None,
        reporch_runtime_core::GuestTerminationV2::TimedOut => Some(ExpectedVerdict::TimeLimit),
        reporch_runtime_core::GuestTerminationV2::Signalled
        | reporch_runtime_core::GuestTerminationV2::OutputLimit => {
            Some(ExpectedVerdict::RuntimeError)
        }
    }
}

fn observed_verdict_name(verdict: Option<ExpectedVerdict>) -> &'static str {
    verdict.map(verdict_name).unwrap_or("judge_error")
}

fn set_runtime_program(
    source: PathBuf,
    language: String,
    interactor: bool,
    output: &CliOutput,
) -> Result<()> {
    let source = relative_string(&source)?;
    let spec = reporch_cli::local_project::update_authoring_spec(Path::new("."), |root, spec| {
        reporch_cli::local_project::declare_project_file(
            root,
            spec,
            &source,
            source_media_type(&language),
            false,
        )?;
        if interactor {
            spec.judging.interactor_path = Some(source.clone());
            spec.judging.interactor_language = Some(language.clone());
        } else {
            spec.judging.grader_path = Some(source.clone());
            spec.judging.grader_language = Some(language.clone());
        }
        Ok(())
    })?;
    output.emit(
        if interactor {
            "interactor set"
        } else {
            "grader set"
        },
        if interactor {
            &spec.judging.interactor_path
        } else {
            &spec.judging.grader_path
        },
        &format!(
            "Configured {}",
            if interactor { "interactor" } else { "grader" }
        ),
    )
}

pub async fn output_submission(options: OutputOptions, output: &CliOutput) -> Result<()> {
    if v2::is_active_project()? {
        return v2::output_submission(options, output).await;
    }
    match options.command {
        OutputCommand::List => {
            let root = reporch_cli::local_project::discover_project(Path::new("."))?;
            let spec = reporch_cli::local_project::read_authoring_spec(&root)?;
            output.emit(
                "output list",
                &spec.output_submissions,
                &format!("{} output submission(s)", spec.output_submissions.len()),
            )
        }
        OutputCommand::Add {
            name,
            expected,
            mappings,
            minimum_score,
            maximum_score,
        } => {
            ensure!(!mappings.is_empty(), "at least one --map is required");
            let expected_score = score_range(minimum_score, maximum_score, expected)?;
            let spec = reporch_cli::local_project::update_authoring_spec(
                Path::new("."),
                |root, spec| {
                    ensure!(
                        !spec
                            .output_submissions
                            .iter()
                            .any(|submission| submission.name == name),
                        "output submission already exists: {name}"
                    );
                    let mut outputs = std::collections::BTreeMap::new();
                    for (test_id, path) in &mappings {
                        ensure!(
                            spec.judging.tests.iter().any(|test| test.id == *test_id),
                            "unknown test case: {test_id}. List test UUIDs with `reporch test case list --format json`"
                        );
                        reporch_cli::local_project::declare_project_file(
                            root,
                            spec,
                            path,
                            "text/plain",
                            false,
                        )?;
                        ensure!(
                            outputs.insert(*test_id, path.clone()).is_none(),
                            "duplicate test mapping: {test_id}"
                        );
                    }
                    spec.output_submissions.push(OutputSubmissionSpec {
                        name: normalize_name(&name)?,
                        outputs,
                        expected_verdict: expected.into(),
                        expected_score: expected_score.clone(),
                    });
                    Ok(())
                },
            )?;
            output.emit(
                "output add",
                &spec.output_submissions,
                &format!("Added output submission {name}"),
            )
        }
        OutputCommand::Remove(options) => {
            let name = options.into_name();
            let mut pruned = 0_usize;
            let spec = reporch_cli::local_project::update_authoring_spec(
                Path::new("."),
                |_root, spec| {
                    let removed_paths = spec
                        .output_submissions
                        .iter()
                        .filter(|submission| submission.name == name)
                        .flat_map(|submission| submission.outputs.values().cloned())
                        .collect::<Vec<_>>();
                    let before = spec.output_submissions.len();
                    spec.output_submissions
                        .retain(|submission| submission.name != name);
                    ensure!(
                        before != spec.output_submissions.len(),
                        "output submission was not found"
                    );
                    pruned = prune_legacy_output_file_declarations(spec, &removed_paths);
                    Ok(())
                },
            )?;
            output.emit(
                "output remove",
                &spec.output_submissions,
                &format!(
                    "Removed output submission {name}. Pruned {pruned} unused file declaration(s); files remain on disk."
                ),
            )
        }
        OutputCommand::Test { name, runtime } => {
            let root = reporch_cli::local_project::discover_project(Path::new("."))?;
            let spec = reporch_cli::local_project::read_authoring_spec(&root)?;
            let submissions =
                selected_by_name(&spec.output_submissions, name.as_deref(), |submission| {
                    submission.name.as_str()
                })?;
            ensure!(
                !submissions.is_empty(),
                "no output submissions are configured"
            );
            let run_options = runtime.into_run_options(output);
            let mut reports = Vec::new();
            for submission in submissions {
                let mut cases = Vec::new();
                for test in &spec.judging.tests {
                    let actual_path = submission.outputs.get(&test.id).with_context(|| {
                        format!(
                            "output submission {} has no mapping for test {}",
                            submission.name, test.id
                        )
                    })?;
                    let answer_path = test
                        .answer_file
                        .as_deref()
                        .context("output-only test has no answer file")?;
                    let accepted = checker_accepts_path(
                        &root,
                        &spec.judging.checker,
                        &test.input_file,
                        answer_path,
                        actual_path,
                        &run_options,
                    )
                    .await?;
                    cases.push(OutputCaseResult {
                        test_id: test.id,
                        name: test.name.clone(),
                        accepted,
                    });
                }
                let score = output_score(&spec.judging.groups, &spec.judging.tests, &cases)?;
                let actual_verdict = if cases.iter().all(|case| case.accepted) {
                    ExpectedVerdict::Accepted
                } else if score > 0.0 {
                    ExpectedVerdict::Partial
                } else {
                    ExpectedVerdict::WrongAnswer
                };
                let score_matches = submission
                    .expected_score
                    .as_ref()
                    .is_none_or(|range| score >= range.minimum && score <= range.maximum);
                reports.push(OutputSubmissionResult {
                    name: submission.name.clone(),
                    expected: verdict_name(submission.expected_verdict),
                    actual: verdict_name(actual_verdict),
                    score,
                    passed: actual_verdict == submission.expected_verdict && score_matches,
                    cases,
                });
            }
            let report = OutputTestReport {
                schema: "reporch.output-test-report.v1",
                passed: reports.iter().all(|report| report.passed),
                submissions: reports,
            };
            if !report.passed {
                let mismatches = report
                    .submissions
                    .iter()
                    .filter(|submission| !submission.passed)
                    .map(|submission| {
                        format!(
                            "{}: expected {}, actual {}, score {}",
                            submission.name,
                            submission.expected,
                            submission.actual,
                            submission.score
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("; ");
                return Err(crate::cli_output::detailed_error(
                    format!("output validation did not pass: {mismatches}"),
                    &report,
                ));
            }
            output.emit(
                "output test",
                &report,
                "All output submissions matched their expected verdicts",
            )
        }
    }
}

#[derive(Debug, Serialize)]
struct OutputTestReport {
    schema: &'static str,
    passed: bool,
    submissions: Vec<OutputSubmissionResult>,
}

struct MaterializedManualCase {
    input: String,
    answer: Option<String>,
    created: Vec<PathBuf>,
}

struct MaterializedValidatorInput {
    path: String,
    created: Option<PathBuf>,
}

impl MaterializedValidatorInput {
    fn rollback(&self) {
        if let Some(path) = &self.created {
            let _ = fs::remove_file(path);
        }
    }
}

fn materialize_validator_unit_input(
    root: &Path,
    options: &ValidatorUnitAddOptions,
) -> Result<MaterializedValidatorInput> {
    if let Some(path) = &options.input {
        return Ok(MaterializedValidatorInput {
            path: relative_string(path)?,
            created: None,
        });
    }
    let path = format!("validator-tests/{}.in", Uuid::now_v7());
    write_project_bytes_atomic(
        root,
        &path,
        options
            .input_text
            .as_deref()
            .context("provide --input INPUT_FILE or --input-text TEXT")?
            .as_bytes(),
    )?;
    Ok(MaterializedValidatorInput {
        created: Some(root.join(&path)),
        path,
    })
}

fn is_starter_validator_unit(name: &str, input_file: &str, expected_valid: bool) -> bool {
    matches!(
        (name, input_file, expected_valid),
        ("accepts-sample", "tests/1.in", true)
            | ("rejects-malformed", "validator-tests/invalid.in", false)
    )
}

impl MaterializedManualCase {
    fn rollback(&self) {
        for path in &self.created {
            let _ = fs::remove_file(path);
        }
    }
}

fn materialize_manual_case_files(
    root: &Path,
    test_id: Uuid,
    options: &TestCaseAddOptions,
) -> Result<MaterializedManualCase> {
    let mut created = Vec::new();
    let input = if let Some(path) = &options.input {
        relative_string(path)?
    } else {
        let path = format!("tests/manual/{test_id}.in");
        write_project_bytes_atomic(
            root,
            &path,
            options
                .input_text
                .as_deref()
                .context("provide --input INPUT_FILE or --input-text TEXT")?
                .as_bytes(),
        )?;
        created.push(root.join(&path));
        path
    };
    let answer_result = if let Some(path) = &options.answer {
        relative_string(path).map(Some)
    } else if let Some(text) = &options.answer_text {
        let path = format!("tests/manual/{test_id}.ans");
        if let Err(error) = write_project_bytes_atomic(root, &path, text.as_bytes()) {
            for created_path in &created {
                let _ = fs::remove_file(created_path);
            }
            return Err(error);
        }
        created.push(root.join(&path));
        Ok(Some(path))
    } else {
        Ok(None)
    };
    let answer = match answer_result {
        Ok(answer) => answer,
        Err(error) => {
            for created_path in &created {
                let _ = fs::remove_file(created_path);
            }
            return Err(error);
        }
    };
    Ok(MaterializedManualCase {
        input,
        answer,
        created,
    })
}

#[derive(Debug, Serialize)]
struct OutputSubmissionResult {
    name: String,
    expected: &'static str,
    actual: &'static str,
    score: f64,
    passed: bool,
    cases: Vec<OutputCaseResult>,
}

fn verdict_name(verdict: ExpectedVerdict) -> &'static str {
    match verdict {
        ExpectedVerdict::Accepted => "accepted",
        ExpectedVerdict::WrongAnswer => "wrong_answer",
        ExpectedVerdict::TimeLimit => "time_limit",
        ExpectedVerdict::MemoryLimit => "memory_limit",
        ExpectedVerdict::RuntimeError => "runtime_error",
        ExpectedVerdict::Partial => "partial",
    }
}

fn output_score(
    groups: &[TestGroupSpec],
    tests: &[TestCaseSpec],
    cases: &[OutputCaseResult],
) -> Result<f64> {
    if groups.is_empty() {
        return Ok(if cases.iter().all(|case| case.accepted) {
            100.0
        } else {
            0.0
        });
    }
    let accepted = cases
        .iter()
        .map(|case| (case.test_id, case.accepted))
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut resolved = std::collections::BTreeMap::<String, bool>::new();
    while resolved.len() < groups.len() {
        let before = resolved.len();
        for group in groups {
            if resolved.contains_key(&group.id)
                || group
                    .depends_on
                    .iter()
                    .any(|dependency| !resolved.contains_key(dependency))
            {
                continue;
            }
            let dependencies_passed = group
                .depends_on
                .iter()
                .all(|dependency| resolved.get(dependency) == Some(&true));
            let group_tests = tests
                .iter()
                .filter(|test| test.groups.iter().any(|id| id == &group.id))
                .collect::<Vec<_>>();
            ensure!(
                !group_tests.is_empty(),
                "score group has no test cases: {}",
                group.id
            );
            let tests_passed = group_tests
                .iter()
                .all(|test| accepted.get(&test.id) == Some(&true));
            resolved.insert(group.id.clone(), dependencies_passed && tests_passed);
        }
        ensure!(
            resolved.len() > before,
            "score group dependencies contain a cycle or unknown group"
        );
    }
    Ok(groups
        .iter()
        .filter(|group| resolved.get(&group.id) == Some(&true))
        .map(|group| group.points)
        .sum())
}

#[derive(Debug, Serialize)]
struct OutputCaseResult {
    test_id: Uuid,
    name: String,
    accepted: bool,
}

#[derive(Debug, Serialize)]
struct ProgramUnitReport {
    passed: bool,
    cases: Vec<ProgramUnitResult>,
}

#[derive(Debug, Serialize)]
struct ProgramUnitResult {
    program: String,
    name: String,
    expected: &'static str,
    actual: &'static str,
    passed: bool,
    exit_code: i32,
    termination: reporch_runtime_core::GuestTerminationV2,
    duration_ms: u128,
    stdout: String,
    stderr: String,
}

fn emit_unit_report(
    command: &str,
    cases: Vec<ProgramUnitResult>,
    output: &CliOutput,
) -> Result<()> {
    let report = ProgramUnitReport {
        passed: cases.iter().all(|case| case.passed),
        cases,
    };
    if report
        .cases
        .iter()
        .any(|case| case.termination == reporch_runtime_core::GuestTerminationV2::TimedOut)
    {
        return Err(crate::cli_output::domain_error(
            "runtime.execution_timed_out",
            "a local validation workload exceeded its configured timeout",
            &report,
        ));
    }
    if report
        .cases
        .iter()
        .any(|case| case.termination != reporch_runtime_core::GuestTerminationV2::Exited)
    {
        return Err(crate::cli_output::domain_error(
            "runtime.execution_failed",
            "a local validation workload did not exit normally",
            &report,
        ));
    }
    if !report.passed {
        let failed = report
            .cases
            .iter()
            .filter(|case| !case.passed)
            .map(|case| format!("{}:{}", case.program, case.name))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(crate::cli_output::domain_error(
            "operation.failed",
            format!("validation did not pass for {failed}"),
            &report,
        ));
    }
    output.emit(command, &report, "All configured unit cases passed")
}

fn termination_name(termination: reporch_runtime_core::GuestTerminationV2) -> &'static str {
    match termination {
        reporch_runtime_core::GuestTerminationV2::Exited => "exited",
        reporch_runtime_core::GuestTerminationV2::TimedOut => "timed_out",
        reporch_runtime_core::GuestTerminationV2::Signalled => "signalled",
        reporch_runtime_core::GuestTerminationV2::OutputLimit => "output_limit",
        reporch_runtime_core::GuestTerminationV2::InternalError => "internal_error",
    }
}

fn find_legacy_solution<'a>(
    spec: &'a reporch_format::AuthoringSpecV1,
    selector: &str,
) -> Result<&'a SolutionSpec> {
    let matches = spec
        .solutions
        .iter()
        .filter(|solution| solution.name == selector || solution.source_path == selector)
        .collect::<Vec<_>>();
    ensure!(
        matches.len() <= 1,
        "ambiguous solution selector {selector:?}; use a unique solution name from `reporch solution list`"
    );
    matches.into_iter().next().with_context(|| {
            format!(
                "solution was not found: {selector}; use a solution name or source path from `reporch solution list`"
            )
        })
}

fn find_legacy_test<'a>(
    spec: &'a reporch_format::AuthoringSpecV1,
    selector: &str,
) -> Result<&'a TestCaseSpec> {
    let parsed = Uuid::parse_str(selector).ok();
    let matches = spec
        .judging
        .tests
        .iter()
        .filter(|test| {
            parsed == Some(test.id) || test.name == selector || test.input_file == selector
        })
        .collect::<Vec<_>>();
    ensure!(
        matches.len() <= 1,
        "ambiguous test selector {selector:?}; use the exact UUID from `reporch test case list`"
    );
    matches.into_iter().next().with_context(|| {
            format!(
                "test case was not found: {selector}; use a test name, UUID, or input path from `reporch test case list`"
            )
        })
}

fn legacy_solution_matrix_human(solutions: &[SolutionSpec]) -> String {
    let mut lines = vec![format!("{} solution expectation(s):", solutions.len())];
    lines.extend(solutions.iter().map(|solution| {
        let score = solution
            .expected_score
            .as_ref()
            .map(|range| format!(" · score {}..{}", range.minimum, range.maximum))
            .unwrap_or_default();
        format!(
            "- {} · {}{} · {}",
            human_safe(&solution.name),
            verdict_name(solution.expected_verdict),
            score,
            human_safe(&solution.source_path)
        )
    }));
    lines.push("This lists expectations only; run `reporch verify` for execution evidence.".into());
    lines.join("\n")
}

fn prune_legacy_output_file_declarations(
    spec: &mut reporch_format::AuthoringSpecV1,
    removed_paths: &[String],
) -> usize {
    let mut pruned = 0;
    for path in removed_paths {
        if spec
            .output_submissions
            .iter()
            .any(|submission| submission.outputs.values().any(|value| value == path))
        {
            continue;
        }
        let mut candidate = spec.clone();
        let before = candidate.files.len();
        candidate.files.retain(|file| file.path != *path);
        if before != candidate.files.len() && candidate.validate_references().is_ok() {
            *spec = candidate;
            pruned += 1;
        }
    }
    pruned
}

fn selected_by_name<'a, T, F>(items: &'a [T], name: Option<&str>, key: F) -> Result<Vec<&'a T>>
where
    F: Fn(&T) -> &str,
{
    match name {
        Some(name) => {
            let selected = items
                .iter()
                .find(|item| key(item) == name)
                .with_context(|| format!("unit or submission was not found: {name}"))?;
            Ok(vec![selected])
        }
        None => Ok(items.iter().collect()),
    }
}

async fn checker_accepts_path(
    root: &Path,
    checker: &CheckerSpec,
    input_path: &str,
    answer_path: &str,
    actual_path: &str,
    options: &reporch_cli::authoring_runtime::AuthoringRunOptions,
) -> Result<bool> {
    let answer = read_project_bytes(root, answer_path)?;
    let actual = read_project_bytes(root, actual_path)?;
    match checker {
        CheckerSpec::Custom {
            source_path,
            language,
            protocol,
        } => {
            let _ = read_project_bytes(root, input_path)?;
            let result = reporch_cli::authoring_runtime::run_custom_checker(
                root,
                source_path,
                language,
                *protocol,
                input_path,
                answer_path,
                actual_path,
                options,
            )
            .await?;
            match result.verdict {
                reporch_cli::authoring_runtime::CustomCheckerVerdict::Accepted => Ok(true),
                reporch_cli::authoring_runtime::CustomCheckerVerdict::WrongAnswer => Ok(false),
                reporch_cli::authoring_runtime::CustomCheckerVerdict::JudgeError => {
                    Err(crate::cli_output::domain_error(
                        "checker.judge_error",
                        format!(
                            "custom checker failed with {:?} and exit code {}",
                            result.execution.termination, result.execution.exit_code
                        ),
                        &result.execution,
                    ))
                }
            }
        }
        _ => reporch_cli::authoring_runtime::standard_checker_matches(checker, &answer, &actual),
    }
}

async fn checker_accepts_bytes(
    root: &Path,
    checker: &CheckerSpec,
    input_path: &str,
    answer_path: &str,
    actual: &[u8],
    options: &reporch_cli::authoring_runtime::AuthoringRunOptions,
) -> Result<bool> {
    if !matches!(checker, CheckerSpec::Custom { .. }) {
        let answer = read_project_bytes(root, answer_path)?;
        return reporch_cli::authoring_runtime::standard_checker_matches(checker, &answer, actual);
    }
    let temporary_directory = root.join(".reporch");
    let mut temporary = tempfile::Builder::new()
        .prefix("local-check-")
        .suffix(".out")
        .tempfile_in(&temporary_directory)
        .context("create temporary custom-checker output")?;
    temporary.write_all(actual)?;
    temporary.as_file().sync_all()?;
    let actual_path = project_relative(root, temporary.path())?;
    checker_accepts_path(
        root,
        checker,
        input_path,
        answer_path,
        &actual_path,
        options,
    )
    .await
}

pub(super) fn checked_statement_path(
    root: &Path,
    path: &str,
    media_type: &str,
    executable: bool,
) -> Result<PathBuf> {
    ensure!(
        !executable
            && matches!(media_type, "text/markdown" | "text/x-markdown")
            && matches!(
                Path::new(path)
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .map(str::to_ascii_lowercase)
                    .as_deref(),
                Some("md" | "markdown")
            ),
        "statement must be a declared, non-executable Markdown file: {path}"
    );
    checked_project_file_path(root, path)
}

pub(super) fn read_statement_markdown(
    root: &Path,
    path: &str,
    media_type: &str,
    executable: bool,
) -> Result<String> {
    let checked = checked_statement_path(root, path, media_type, executable)?;
    let bytes = fs::read(&checked).with_context(|| format!("read project file {path}"))?;
    String::from_utf8(bytes).with_context(|| format!("statement is not UTF-8: {path}"))
}

fn checked_project_file_path(root: &Path, path: &str) -> Result<PathBuf> {
    let normalized = studio_core::normalize_relative_path(path)?;
    let candidate = root.join(&normalized);
    ensure_no_symlink_parents(root, Path::new(&normalized))?;
    let metadata = fs::symlink_metadata(&candidate)
        .with_context(|| format!("inspect project file {normalized}"))?;
    ensure!(
        metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
        "project file must be a regular non-symlink file: {normalized}"
    );
    let canonical = candidate
        .canonicalize()
        .with_context(|| format!("resolve project file {normalized}"))?;
    ensure!(
        canonical.starts_with(root),
        "project file escapes the project: {normalized}"
    );
    Ok(canonical)
}

fn read_project_bytes(root: &Path, path: &str) -> Result<Vec<u8>> {
    let canonical = checked_project_file_path(root, path)?;
    fs::read(canonical).with_context(|| format!("read project file {path}"))
}

fn ensure_unique_test_input<'a>(
    root: &Path,
    input_path: &str,
    new_name: &str,
    existing: impl Iterator<Item = (&'a str, &'a str)>,
) -> Result<()> {
    let input_digest = Sha256::digest(read_project_bytes(root, input_path)?);
    for (existing_name, existing_path) in existing {
        let duplicate = existing_path == input_path
            || Sha256::digest(read_project_bytes(root, existing_path)?) == input_digest;
        ensure!(
            !duplicate,
            "test input for {new_name} duplicates existing test {existing_name} ({existing_path}); use a different input or update the existing test"
        );
    }
    Ok(())
}

fn next_test_case_name<'a>(tests: impl Iterator<Item = &'a str>) -> String {
    let mut names = std::collections::BTreeSet::new();
    for name in tests {
        names.insert(name);
    }
    for index in 1_u64.. {
        let name = format!("sample-{index}");
        if !names.contains(name.as_str()) {
            return name;
        }
    }
    unreachable!("the test index space is finite only after exhausting u64")
}

fn guided_test_text(value: &str) -> String {
    if value.is_empty() || value.ends_with('\n') {
        value.to_owned()
    } else {
        format!("{value}\n")
    }
}

fn write_project_bytes_atomic(root: &Path, path: &str, bytes: &[u8]) -> Result<()> {
    let normalized = studio_core::normalize_relative_path(path)?;
    let destination = root.join(&normalized);
    let parent = destination.parent().context("output path has no parent")?;
    ensure_no_symlink_parents(root, Path::new(&normalized))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("create output directory {}", parent.display()))?;
    ensure_no_symlink_parents(root, Path::new(&normalized))?;
    let canonical_parent = parent
        .canonicalize()
        .with_context(|| format!("resolve output directory {}", parent.display()))?;
    ensure!(
        canonical_parent.starts_with(root),
        "output directory escapes the project: {normalized}"
    );
    match fs::symlink_metadata(&destination) {
        Ok(_) => bail!("refusing to overwrite existing output: {normalized}"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("inspect output destination {normalized}"));
        }
    }
    let mut temporary = tempfile::NamedTempFile::new_in(&canonical_parent)
        .with_context(|| format!("create temporary output for {normalized}"))?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist_noclobber(&destination)
        .map_err(|error| error.error)
        .with_context(|| format!("atomically create output {normalized}"))?;
    Ok(())
}

fn materialize_statement_file(
    root: &Path,
    relative: &str,
    title: Option<&str>,
    locale: &str,
) -> Result<Option<PathBuf>> {
    let destination = root.join(relative);
    match fs::symlink_metadata(&destination) {
        Ok(metadata) => {
            ensure!(
                metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
                "statement path must be a regular non-symlink file: {relative}"
            );
            Ok(None)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let heading = title
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("Problem statement");
            let starter = format!(
                "# {heading}\n\n*Locale: {locale}*\n\n## Description\n\nWrite the problem description here.\n\n## Input\n\nDescribe the input format.\n\n## Output\n\nDescribe the output format.\n"
            );
            write_project_bytes_atomic(root, relative, starter.as_bytes())?;
            Ok(Some(destination))
        }
        Err(error) => Err(error).with_context(|| format!("inspect statement path {relative}")),
    }
}

fn ensure_no_symlink_parents(root: &Path, path: &Path) -> Result<()> {
    let mut cursor = root.to_owned();
    if let Some(parent) = path.parent() {
        for component in parent.components() {
            cursor.push(component);
            let metadata = match fs::symlink_metadata(&cursor) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("inspect project path component {}", cursor.display())
                    });
                }
            };
            ensure!(
                metadata.file_type().is_dir() && !metadata.file_type().is_symlink(),
                "project path contains a non-directory or symlink: {}",
                cursor.display()
            );
        }
    }
    Ok(())
}

fn score_range(
    minimum: Option<f64>,
    maximum: Option<f64>,
    verdict: Verdict,
) -> Result<Option<ExpectedScoreRange>> {
    score_range_for_verdict(minimum, maximum, verdict.into())
}

fn score_range_for_verdict(
    minimum: Option<f64>,
    maximum: Option<f64>,
    verdict: ExpectedVerdict,
) -> Result<Option<ExpectedScoreRange>> {
    if verdict != ExpectedVerdict::Partial {
        ensure!(
            minimum.is_none() && maximum.is_none(),
            "score range is only valid for partial solutions"
        );
        return Ok(None);
    }
    let minimum = minimum.context("--minimum-score is required for partial")?;
    let maximum = maximum.context("--maximum-score is required for partial")?;
    ensure!(
        minimum.is_finite() && maximum.is_finite() && minimum <= maximum,
        "score range must be finite and ordered"
    );
    Ok(Some(ExpectedScoreRange { minimum, maximum }))
}

fn ensure_unique_test_name(
    spec: &reporch_format::AuthoringSpecV1,
    name: &str,
    except: Option<Uuid>,
) -> Result<()> {
    let name = normalize_name(name)?;
    ensure!(
        !spec
            .judging
            .tests
            .iter()
            .any(|test| test.id != except.unwrap_or_else(Uuid::nil) && test.name == name),
        "test name already exists: {name}"
    );
    Ok(())
}

fn ensure_groups_exist(spec: &reporch_format::AuthoringSpecV1, groups: &[String]) -> Result<()> {
    for id in groups {
        ensure!(
            spec.judging.groups.iter().any(|group| &group.id == id),
            "unknown group: {id}. Create it with `reporch test group add {id} --points 0`, list groups with `reporch test group list`, or omit --group for an ungrouped sample test"
        );
    }
    Ok(())
}

fn validate_group_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty()
            && id.len() <= 64
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
        "group ID must contain 1-64 letters, numbers, '-' or '_'"
    );
    Ok(())
}

fn validate_group_points(points: f64) -> Result<()> {
    ensure!(
        points.is_finite() && (0.0..=100.0).contains(&points),
        "group points must be a finite value from 0 to 100"
    );
    Ok(())
}

fn ensure_v1_group_dependencies_acyclic(groups: &[TestGroupSpec]) -> Result<()> {
    let mut resolved = std::collections::BTreeSet::new();
    while resolved.len() < groups.len() {
        let before = resolved.len();
        for group in groups {
            if !resolved.contains(&group.id)
                && group
                    .depends_on
                    .iter()
                    .all(|dependency| resolved.contains(dependency))
            {
                resolved.insert(group.id.clone());
            }
        }
        ensure!(
            resolved.len() > before,
            "test group dependency graph cannot contain a cycle"
        );
    }
    Ok(())
}

fn validate_floating_tolerances(absolute_error: &str, relative_error: &str) -> Result<()> {
    let absolute = absolute_error.parse::<f64>().ok();
    let relative = relative_error.parse::<f64>().ok();
    ensure!(
        absolute.is_some_and(|value| value.is_finite() && value >= 0.0)
            && relative.is_some_and(|value| value.is_finite() && value >= 0.0)
            && (absolute != Some(0.0) || relative != Some(0.0)),
        "floating checker tolerances must be finite and non-negative, with at least one greater than zero"
    );
    Ok(())
}

fn group_points_feedback_v1(
    problem_type: studio_core::ProblemType,
    groups: &[TestGroupSpec],
    action: &str,
    group: &str,
) -> String {
    if problem_type != studio_core::ProblemType::Scored {
        return action.to_owned();
    }
    scored_points_feedback(action, group, groups.iter().map(|group| group.points).sum())
}

fn scored_points_feedback(action: &str, group: &str, total: f64) -> String {
    let total_display = display_points(total);
    if total > 100.0 {
        let over = display_points(total - 100.0);
        format!(
            "{action} · scored groups total {total_display}/100 ({over} points over; adjust with `reporch test group update {group} --points <POINTS>` before `reporch check`)"
        )
    } else if total < 100.0 {
        let remaining = display_points(100.0 - total);
        format!("{action} · scored groups total {total_display}/100 ({remaining} points remaining)")
    } else {
        format!("{action} · scored groups total 100/100")
    }
}

fn display_points(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        value.to_string()
    }
}

fn normalize_name(value: &str) -> Result<String> {
    let value = value.trim();
    ensure!(
        !value.is_empty() && value.chars().count() <= 255 && !value.chars().any(char::is_control),
        "name must contain 1-255 characters and no control characters"
    );
    Ok(value.to_owned())
}

fn human_safe(value: &str) -> String {
    value
        .chars()
        .flat_map(|character| {
            if character.is_control() {
                character.escape_default().collect::<Vec<_>>()
            } else {
                vec![character]
            }
        })
        .collect()
}

fn relative_string(path: &Path) -> Result<String> {
    let value = path
        .to_str()
        .context("path must contain valid Unicode")?
        .replace('\\', "/");
    studio_core::normalize_relative_path(&value).map_err(Into::into)
}

fn project_relative(root: &Path, path: &Path) -> Result<String> {
    let path = fs::canonicalize(path).with_context(|| format!("resolve {}", path.display()))?;
    ensure!(path.starts_with(root), "path must stay inside the project");
    relative_string(path.strip_prefix(root)?)
}

fn source_media_type(language: &str) -> &'static str {
    match language.to_ascii_lowercase().as_str() {
        "python" | "python3" | "py" => "text/x-python",
        "cpp" | "c++" | "gnu++17" | "gnu++20" => "text/x-c++src",
        "c" | "gnu11" => "text/x-csrc",
        "rust" => "text/x-rustsrc",
        "java" => "text/x-java",
        _ => "text/plain",
    }
}

fn prompt(label: &str, default: &str) -> Result<String> {
    print!("{label} [{default}]: ");
    std::io::stdout().flush()?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let input = input.trim();
    Ok(if input.is_empty() {
        default.to_owned()
    } else {
        input.to_owned()
    })
}

fn parse_output_mapping(value: &str) -> Result<(Uuid, String), String> {
    let (test_id, path) = value
        .split_once('=')
        .ok_or_else(|| "mapping must use UUID=relative/path".to_owned())?;
    let test_id = test_id
        .parse()
        .map_err(|_| {
            "mapping contains an invalid test UUID; list test UUIDs with `reporch test case list --format json`"
                .to_owned()
        })?;
    let path = relative_string(Path::new(path)).map_err(|error| error.to_string())?;
    Ok((test_id, path))
}

fn parse_runtime_output_path(value: &str) -> Result<PathBuf, String> {
    let normalized = relative_string(Path::new(value)).map_err(|_| {
        "output must be a safe project-relative path, for example artifacts/transcript.txt"
            .to_owned()
    })?;
    Ok(PathBuf::from(normalized))
}
