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
    Remove { id: Uuid },
}

#[derive(Debug, ClapArgs)]
struct TestCaseAddOptions {
    #[arg(long)]
    name: String,
    #[arg(long)]
    input: PathBuf,
    #[arg(long)]
    answer: Option<PathBuf>,
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
    id: Uuid,
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
    id: String,
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
    #[arg(long)]
    id: String,
    #[arg(long)]
    source: PathBuf,
    #[arg(long)]
    language: String,
    #[arg(long = "argument")]
    arguments: Vec<String>,
}

#[derive(Debug, ClapArgs)]
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
    UnitAdd {
        #[arg(long)]
        name: String,
        #[arg(long)]
        input: PathBuf,
        #[arg(long, value_enum)]
        expected: ValidityExpectation,
    },
    Run {
        #[arg(long)]
        name: Option<String>,
        #[command(flatten)]
        runtime: RuntimeOptions,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ValidityExpectation {
    Valid,
    Invalid,
}

#[derive(Debug, ClapArgs)]
pub struct CheckerOptions {
    #[command(subcommand)]
    command: CheckerCommand,
}

#[derive(Debug, Subcommand)]
enum CheckerCommand {
    ListStandard,
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
    Run {
        #[arg(long)]
        name: Option<String>,
        #[command(flatten)]
        runtime: RuntimeOptions,
    },
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
        #[arg(long)]
        source: PathBuf,
        #[arg(long)]
        language: String,
    },
    Run(RuntimeProgramRunOptions),
}

#[derive(Debug, Clone, ClapArgs)]
struct RuntimeProgramRunOptions {
    #[arg(long)]
    solution: String,
    #[arg(long)]
    test: Uuid,
    #[arg(long)]
    output: Option<PathBuf>,
    #[command(flatten)]
    runtime: RuntimeOptions,
}

#[derive(Debug, ClapArgs)]
pub struct OutputOptions {
    #[command(subcommand)]
    command: OutputCommand,
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
    Remove {
        name: String,
    },
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
    #[arg(long, value_enum, default_value_t = RuntimeKind::Auto)]
    runtime: RuntimeKind,
    #[arg(long, default_value_t = 30)]
    timeout_seconds: u64,
    #[arg(long, default_value_t = 512)]
    memory_mib: u64,
    #[arg(long, default_value_t = 1.0)]
    cpus: f64,
    #[arg(long, default_value_t = 1_024)]
    output_kib: u64,
}

#[derive(Debug, Clone, Copy, Default, ValueEnum)]
enum RuntimeKind {
    #[default]
    Auto,
    Podman,
    Docker,
}

impl RuntimeOptions {
    fn into_run_options(self) -> reporch_cli::authoring_runtime::AuthoringRunOptions {
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
        }
    }
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
        } => {
            let relative = relative_string(&path)?;
            let spec =
                reporch_cli::local_project::update_authoring_spec(Path::new("."), |root, spec| {
                    reporch_cli::local_project::declare_project_file(
                        root,
                        spec,
                        &relative,
                        "text/markdown",
                        false,
                    )?;
                    spec.statements.insert(locale.clone(), relative.clone());
                    if let Some(title) = &title {
                        ensure!(!title.trim().is_empty(), "title cannot be empty");
                        spec.title.insert(locale.clone(), title.trim().to_owned());
                    }
                    Ok(())
                })?;
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
            open::that(root.join(path)).context("open statement in the default application")?;
            output.emit(
                "statement open",
                &serde_json::json!({ "locale": locale, "path": path }),
                &format!("Opened {path}"),
            )
        }
        StatementCommand::Check => {
            let root = reporch_cli::local_project::discover_project(Path::new("."))?;
            let spec = reporch_cli::local_project::read_authoring_spec(&root)?;
            for (locale, path) in &spec.statements {
                let contents = fs::read_to_string(root.join(path))
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
            let locale = locale.unwrap_or(spec.default_locale);
            let source = spec
                .statements
                .get(&locale)
                .with_context(|| format!("no statement for locale {locale}"))?;
            let markdown = fs::read_to_string(root.join(source))
                .with_context(|| format!("read {locale} statement {source}"))?;
            let rendered = match render_format {
                StatementRenderFormat::Markdown => markdown,
                StatementRenderFormat::Latex => crate::statement_tex::markdown_to_tex(&markdown),
                StatementRenderFormat::Html => safe_statement_html(&markdown),
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

fn safe_statement_html(markdown: &str) -> String {
    use pulldown_cmark::{Event, Options, Parser, html};

    let events = Parser::new_ext(
        markdown,
        Options::ENABLE_MATH | Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH,
    )
    .filter(|event| !matches!(event, Event::Html(_) | Event::InlineHtml(_)));
    let mut html_output = String::new();
    html::push_html(&mut html_output, events);
    html_output
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
    let name = prompt("Test name", "sample-1")?;
    let input = prompt("Input file", "tests/1.in")?;
    let answer = prompt("Answer file (blank for none)", "tests/1.ans")?;
    test_case(
        TestCaseCommand::Add(TestCaseAddOptions {
            name,
            input: PathBuf::from(input),
            answer: (!answer.is_empty()).then(|| PathBuf::from(answer)),
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
            let input = relative_string(&options.input)?;
            let answer = options.answer.as_deref().map(relative_string).transpose()?;
            let test_id = Uuid::now_v7();
            let spec =
                reporch_cli::local_project::update_authoring_spec(Path::new("."), |root, spec| {
                    ensure_unique_test_name(spec, &options.name, None)?;
                    ensure_groups_exist(spec, &options.groups)?;
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
                })?;
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
                    ensure_groups_exist(spec, &options.groups)?;
                    if let Some(name) = &options.name {
                        ensure_unique_test_name(spec, name, Some(options.id))?;
                    }
                    let test = spec
                        .judging
                        .tests
                        .iter_mut()
                        .find(|test| test.id == options.id)
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
                &format!("Updated test case {}", options.id),
            )
        }
        TestCaseCommand::Remove { id } => {
            let spec = reporch_cli::local_project::update_authoring_spec(
                Path::new("."),
                |_root, spec| {
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
                &format!("Removed test case {id}"),
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
                    Ok(())
                },
            )?;
            output.emit(
                "test group add",
                &spec.judging.groups,
                &format!("Added group {}", options.id),
            )
        }
        TestGroupCommand::Update(options) => {
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
                    Ok(())
                },
            )?;
            output.emit(
                "test group update",
                &spec.judging.groups,
                &format!("Updated group {}", options.id),
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
            let run_options = options.runtime.into_run_options();
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
            let run_options = options.runtime.into_run_options();
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
        ValidatorCommand::UnitAdd {
            name,
            input,
            expected,
        } => {
            let input = relative_string(&input)?;
            let spec =
                reporch_cli::local_project::update_authoring_spec(Path::new("."), |root, spec| {
                    reporch_cli::local_project::declare_project_file(
                        root,
                        spec,
                        &input,
                        "text/plain",
                        false,
                    )?;
                    spec.judging.validator_tests.push(ValidatorTestSpec {
                        name: normalize_name(&name)?,
                        input_file: input.clone(),
                        expected_valid: matches!(expected, ValidityExpectation::Valid),
                    });
                    Ok(())
                })?;
            output.emit(
                "validator unit-add",
                &spec.judging.validator_tests,
                &format!("Added validator unit {name}"),
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
            let run_options = runtime.into_run_options();
            let mut cases = Vec::new();
            for validator in &validators {
                for unit in &units {
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
                    let actual_valid = result.exit_code == 0;
                    cases.push(ProgramUnitResult {
                        program: validator.id.clone(),
                        name: unit.name.clone(),
                        expected: if unit.expected_valid {
                            "valid"
                        } else {
                            "invalid"
                        },
                        actual: if actual_valid { "valid" } else { "invalid" },
                        passed: actual_valid == unit.expected_valid,
                        exit_code: result.exit_code,
                        duration_ms: result.duration_ms,
                        stderr: result.stderr,
                    });
                }
            }
            emit_unit_report("validator run", cases, output)
        }
    }
}

pub async fn checker(options: CheckerOptions, output: &CliOutput) -> Result<()> {
    match options.command {
        CheckerCommand::ListStandard => output.emit(
            "checker list-standard",
            &["exact", "token", "case-insensitive", "floating", "custom"],
            "exact, token, case-insensitive, floating, custom",
        ),
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
                CheckerKind::Floating => CheckerSpec::Floating {
                    absolute_error: absolute_error.context("--absolute-error is required")?,
                    relative_error: relative_error.context("--relative-error is required")?,
                },
                CheckerKind::Custom => CheckerSpec::Custom {
                    source_path: source.clone().context("--source is required")?,
                    language: language.clone().context("--language is required")?,
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
            ensure!(!units.is_empty(), "no checker unit tests are configured");
            let run_options = runtime.into_run_options();
            let mut cases = Vec::new();
            for unit in units {
                let (actual_accepted, exit_code, duration_ms, stderr) =
                    if let CheckerSpec::Custom {
                        source_path,
                        language,
                    } = &spec.judging.checker
                    {
                        let arguments = vec![
                            unit.input_file.clone(),
                            unit.output_file.clone(),
                            unit.answer_file.clone(),
                        ];
                        let result = reporch_cli::authoring_runtime::run_program(
                            &reporch_cli::authoring_runtime::ProgramRequest {
                                project_directory: &root,
                                source_path,
                                language,
                                arguments: &arguments,
                                stdin_path: None,
                                options: &run_options,
                            },
                        )
                        .await?;
                        (
                            result.exit_code == 0,
                            result.exit_code,
                            result.duration_ms,
                            result.stderr,
                        )
                    } else {
                        let answer = read_project_bytes(&root, &unit.answer_file)?;
                        let actual = read_project_bytes(&root, &unit.output_file)?;
                        (
                            reporch_cli::authoring_runtime::standard_checker_matches(
                                &spec.judging.checker,
                                &answer,
                                &actual,
                            )?,
                            0,
                            0,
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
                    actual: if actual_accepted {
                        "accepted"
                    } else {
                        "rejected"
                    },
                    passed: actual_accepted == unit.expected_accepted,
                    exit_code,
                    duration_ms,
                    stderr,
                });
            }
            emit_unit_report("checker run", cases, output)
        }
    }
}

pub fn solution(options: SolutionOptions, output: &CliOutput) -> Result<()> {
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
                &format!("{} solution expectation(s)", spec.solutions.len()),
            )
        }
        SolutionCommand::Add(options) => {
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
            let expected_score = match options.expected {
                Some(expected) => {
                    score_range(options.minimum_score, options.maximum_score, expected)?
                }
                None => None,
            };
            let spec = reporch_cli::local_project::update_authoring_spec(
                Path::new("."),
                |_root, spec| {
                    let solution = spec
                        .solutions
                        .iter_mut()
                        .find(|solution| solution.name == options.name)
                        .context("solution was not found")?;
                    if let Some(expected) = options.expected {
                        solution.expected_verdict = expected.into();
                        solution.expected_score = expected_score.clone();
                    }
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

pub async fn interactor(options: InteractorOptions, output: &CliOutput) -> Result<()> {
    match options.command {
        InteractorCommand::Set { source, language } => {
            set_runtime_program(source, language, true, output)
        }
        InteractorCommand::Run(options) => run_interactor(options, false, output).await,
        InteractorCommand::Transcript(options) => run_interactor(options, true, output).await,
    }
}

pub async fn grader(options: GraderOptions, output: &CliOutput) -> Result<()> {
    match options.command {
        GraderCommand::Set { source, language } => {
            set_runtime_program(source, language, false, output)
        }
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
    let solution = spec
        .solutions
        .iter()
        .find(|solution| solution.name == options.solution)
        .with_context(|| format!("solution was not found: {}", options.solution))?;
    let test = spec
        .judging
        .tests
        .iter()
        .find(|test| test.id == options.test)
        .with_context(|| format!("test case was not found: {}", options.test))?;
    let run_options = options.runtime.into_run_options();
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
            options: &run_options,
        },
    )
    .await?;
    if let Some(path) = options.output.as_deref() {
        let path = relative_string(path)?;
        write_project_bytes_atomic(&root, &path, &result.stdout_bytes)?;
    }
    let expected_accepted = solution.expected_verdict == ExpectedVerdict::Accepted;
    let passed = (result.exit_code == 0) == expected_accepted;
    let report = RuntimeProgramReport {
        solution: solution.name.clone(),
        test_id: test.id,
        expected: if expected_accepted {
            "accepted"
        } else {
            "rejected"
        },
        actual: if result.exit_code == 0 {
            "accepted"
        } else {
            "rejected"
        },
        passed,
        exit_code: result.exit_code,
        duration_ms: result.duration_ms,
        transcript: transcript.then_some(result.stdout),
        stderr: result.stderr,
    };
    ensure!(
        report.passed,
        "interactive validation did not pass: expected {}, got {}",
        report.expected,
        report.actual
    );
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
    let solution = spec
        .solutions
        .iter()
        .find(|solution| solution.name == options.solution)
        .with_context(|| format!("solution was not found: {}", options.solution))?;
    ensure!(
        reporch_cli::toolchain::resolve_for_language(None, grader_language)?.language
            == reporch_cli::toolchain::resolve_for_language(None, &solution.language)?.language,
        "local grader linking requires the solution and grader to use the same C or C++ toolchain"
    );
    let test = spec
        .judging
        .tests
        .iter()
        .find(|test| test.id == options.test)
        .with_context(|| format!("test case was not found: {}", options.test))?;
    let answer_path = test
        .answer_file
        .as_deref()
        .context("grader test has no answer file")?;
    let run_options = options.runtime.into_run_options();
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
    let actual_accepted = if result.exit_code == 0 {
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
    let expected_accepted = solution.expected_verdict == ExpectedVerdict::Accepted;
    let report = RuntimeProgramReport {
        solution: solution.name.clone(),
        test_id: test.id,
        expected: if expected_accepted {
            "accepted"
        } else {
            "rejected"
        },
        actual: if actual_accepted {
            "accepted"
        } else {
            "rejected"
        },
        passed: actual_accepted == expected_accepted,
        exit_code: result.exit_code,
        duration_ms: result.duration_ms,
        transcript: None,
        stderr: result.stderr,
    };
    ensure!(
        report.passed,
        "grader validation did not pass: expected {}, got {}",
        report.expected,
        report.actual
    );
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
    duration_ms: u128,
    transcript: Option<String>,
    stderr: String,
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
            let spec =
                reporch_cli::local_project::update_authoring_spec(Path::new("."), |root, spec| {
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
                            "unknown test case: {test_id}"
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
                })?;
            output.emit(
                "output add",
                &spec.output_submissions,
                &format!("Added output submission {name}"),
            )
        }
        OutputCommand::Remove { name } => {
            let spec = reporch_cli::local_project::update_authoring_spec(
                Path::new("."),
                |_root, spec| {
                    let before = spec.output_submissions.len();
                    spec.output_submissions
                        .retain(|submission| submission.name != name);
                    ensure!(
                        before != spec.output_submissions.len(),
                        "output submission was not found"
                    );
                    Ok(())
                },
            )?;
            output.emit(
                "output remove",
                &spec.output_submissions,
                &format!("Removed output submission {name}"),
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
            let run_options = runtime.into_run_options();
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
                passed: reports.iter().all(|report| report.passed),
                submissions: reports,
            };
            ensure!(
                report.passed,
                "output validation did not pass: one or more submissions disagreed with the expected verdict"
            );
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
    passed: bool,
    submissions: Vec<OutputSubmissionResult>,
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
    duration_ms: u128,
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
    if !report.passed {
        let failed = report
            .cases
            .iter()
            .filter(|case| !case.passed)
            .map(|case| format!("{}:{}", case.program, case.name))
            .collect::<Vec<_>>()
            .join(", ");
        bail!("validation did not pass for {failed}");
    }
    output.emit(command, &report, "All configured unit cases passed")
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
        } => {
            let _ = read_project_bytes(root, input_path)?;
            let arguments = vec![
                input_path.to_owned(),
                actual_path.to_owned(),
                answer_path.to_owned(),
            ];
            let result = reporch_cli::authoring_runtime::run_program(
                &reporch_cli::authoring_runtime::ProgramRequest {
                    project_directory: root,
                    source_path,
                    language,
                    arguments: &arguments,
                    stdin_path: None,
                    options,
                },
            )
            .await?;
            Ok(result.exit_code == 0)
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

fn read_project_bytes(root: &Path, path: &str) -> Result<Vec<u8>> {
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
    fs::read(canonical).with_context(|| format!("read project file {normalized}"))
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
    if !matches!(verdict, Verdict::Partial) {
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
            "unknown group: {id}"
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

fn normalize_name(value: &str) -> Result<String> {
    let value = value.trim();
    ensure!(
        !value.is_empty() && value.chars().count() <= 255,
        "name must contain 1-255 characters"
    );
    Ok(value.to_owned())
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
        .map_err(|_| "mapping contains an invalid test UUID".to_owned())?;
    let path = relative_string(Path::new(path)).map_err(|error| error.to_string())?;
    Ok((test_id, path))
}
