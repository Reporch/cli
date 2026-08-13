use std::fs;
use std::io::{IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use clap::{Args as ClapArgs, Subcommand, ValueEnum};
use studio_core::{
    CheckerSpec, CheckerTestSpec, ExpectedScoreRange, ExpectedVerdict, OutputSubmissionSpec,
    ProgramSpec, SolutionSpec, TestCaseSpec, TestGroupSpec, ValidatorTestSpec,
};
use uuid::Uuid;

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
    Remove { id: String },
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
    command: RuntimeProgramCommand,
}

#[derive(Debug, ClapArgs)]
pub struct GraderOptions {
    #[command(subcommand)]
    command: RuntimeProgramCommand,
}

#[derive(Debug, Subcommand)]
enum RuntimeProgramCommand {
    Set {
        #[arg(long)]
        source: PathBuf,
        #[arg(long)]
        language: String,
    },
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
    }
}

pub fn tests(options: TestOptions, output: &CliOutput, no_input: bool) -> Result<()> {
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

pub fn generator(options: GeneratorOptions, output: &CliOutput) -> Result<()> {
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

pub fn validator(options: ValidatorOptions, output: &CliOutput) -> Result<()> {
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
    }
}

pub fn checker(options: CheckerOptions, output: &CliOutput) -> Result<()> {
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

pub fn interactor(options: InteractorOptions, output: &CliOutput) -> Result<()> {
    let RuntimeProgramCommand::Set { source, language } = options.command;
    set_runtime_program(source, language, true, output)
}

pub fn grader(options: GraderOptions, output: &CliOutput) -> Result<()> {
    let RuntimeProgramCommand::Set { source, language } = options.command;
    set_runtime_program(source, language, false, output)
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

pub fn output_submission(options: OutputOptions, output: &CliOutput) -> Result<()> {
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
    }
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
