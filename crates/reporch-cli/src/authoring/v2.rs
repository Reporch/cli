use std::fmt::Write as _;
use std::fs;
use std::io::{IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use base64::Engine as _;
use studio_core::{
    CheckerSpec, CheckerUnitSpecV2, GeneratedCaseRefV2, GeneratorMatrixStrategyV2,
    GeneratorRecipeSpecV2, GeneratorSpecV2, HarnessKindV2, HarnessProfileSpecV2, HarnessSpecV2,
    InteractiveSpecV2, OutputSubmissionSpecV2, ProblemType, ProgramSpec, ProgramSpecV2,
    ScoreAggregationV2, SolutionRoleV2, SolutionSpecV2, StressSuiteSpecV2, TestCaseOriginV2,
    TestCaseRoleV2, TestCaseSpecV2, TestGroupSpecV2, ValidatorUnitSpecV2,
};
use uuid::Uuid;

use super::*;

#[derive(Debug, serde::Serialize)]
struct RemovalResult<'a, T: serde::Serialize> {
    remaining: &'a T,
    inventory_removed: Vec<String>,
    files_preserved: Vec<String>,
}

pub(super) fn is_active_project() -> Result<bool> {
    reporch_cli::local_project_v2::is_v2_project(Path::new("."))
}

pub(super) fn statement(options: StatementOptions, output: &CliOutput) -> Result<()> {
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
            let updated = reporch_cli::local_project_v2::update_authoring_spec(
                &root,
                |root, spec| {
                    reporch_cli::local_project_v2::declare_project_file(
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
                },
            );
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
            let spec = reporch_cli::local_project_v2::read_authoring_spec(&root)?;
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
            let spec = reporch_cli::local_project_v2::read_authoring_spec(&root)?;
            reporch_cli::local_project_v2::validate_statement_documents(&root, &spec)?;
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
            let spec = reporch_cli::local_project_v2::read_authoring_spec(&root)?;
            reporch_cli::local_project_v2::validate_statement_documents(&root, &spec)?;
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

pub(super) fn tests(options: TestOptions, output: &CliOutput, no_input: bool) -> Result<()> {
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
    let spec = reporch_cli::local_project_v2::read_authoring_spec(&root)?;
    let defaults = next_test_case_defaults(spec.testing.tests.iter().map(|test| {
        (
            test.name.as_str(),
            test.input_file.as_str(),
            test.answer_file.as_deref(),
        )
    }));
    let name = prompt("Test name", &defaults.0)?;
    let input = prompt("Input file", &defaults.1)?;
    let answer = prompt("Answer file (blank for none)", &defaults.2)?;
    test_case(
        TestCaseCommand::Add(TestCaseAddOptions {
            name,
            input: Some(PathBuf::from(input)),
            input_text: None,
            answer: (!answer.is_empty()).then(|| PathBuf::from(answer)),
            answer_text: None,
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
            let spec = reporch_cli::local_project_v2::read_authoring_spec(&root)?;
            output.emit(
                "test case list",
                &spec.testing.tests,
                &format!("{} test case(s)", spec.testing.tests.len()),
            )
        }
        TestCaseCommand::Add(options) => {
            let test_id = Uuid::now_v7();
            let root = reporch_cli::local_project::discover_project(Path::new("."))?;
            let materialized = materialize_manual_case_files(&root, test_id, &options)?;
            let input = materialized.input.clone();
            let answer = materialized.answer.clone();
            let updated =
                reporch_cli::local_project_v2::update_authoring_spec(&root, |root, spec| {
                    ensure_unique_test_name(spec, &options.name, None)?;
                    ensure_unique_test_input(
                        root,
                        &input,
                        &options.name,
                        spec.testing
                            .tests
                            .iter()
                            .map(|test| (test.name.as_str(), test.input_file.as_str())),
                    )?;
                    let group_ids = resolve_group_ids(spec, &options.groups)?;
                    let generated = if let Some(generator_name) = &options.generated_by {
                        let seed = options
                            .seed
                            .context("a generated test requires --seed so it can be reproduced")?;
                        let generator = spec
                            .testing
                            .generators
                            .iter_mut()
                            .find(|generator| generator.program.name == *generator_name)
                            .with_context(|| format!("unknown generator: {generator_name}"))?;
                        let recipe_id = Uuid::now_v7();
                        generator.recipes.push(GeneratorRecipeSpecV2 {
                            id: recipe_id,
                            name: format!("case-{}", normalize_name(&options.name)?),
                            argument_template: Vec::new(),
                            parameters: Default::default(),
                            matrix: GeneratorMatrixStrategyV2::Cartesian,
                            seed_start: seed,
                            seed_step: 1,
                            count: 1,
                            group_ids: group_ids.clone(),
                        });
                        Some(GeneratedCaseRefV2 {
                            generator_id: generator.program.id,
                            recipe_id,
                            ordinal: 0,
                            seed,
                        })
                    } else {
                        None
                    };
                    reporch_cli::local_project_v2::declare_project_file(
                        root,
                        spec,
                        &input,
                        "text/plain",
                        false,
                    )?;
                    if let Some(answer) = &answer {
                        reporch_cli::local_project_v2::declare_project_file(
                            root,
                            spec,
                            answer,
                            "text/plain",
                            false,
                        )?;
                    }
                    spec.testing.tests.push(TestCaseSpecV2 {
                        id: test_id,
                        name: normalize_name(&options.name)?,
                        role: inferred_role(&options.name),
                        origin: if generated.is_some() {
                            TestCaseOriginV2::Generated
                        } else {
                            TestCaseOriginV2::Manual
                        },
                        input_file: input.clone(),
                        answer_file: answer.clone(),
                        group_ids,
                        points: None,
                        generated,
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
                &spec.testing.tests,
                &format!("Added test case {test_id}"),
            )
        }
        TestCaseCommand::Import(options) => import_test_cases(options, output),
        TestCaseCommand::Update(options) => {
            let spec = reporch_cli::local_project_v2::update_authoring_spec(
                Path::new("."),
                |_root, spec| {
                    let test_id = find_test(spec, &options.selector)?.id;
                    let group_ids = if options.groups.is_empty() {
                        None
                    } else {
                        Some(resolve_group_ids(spec, &options.groups)?)
                    };
                    if let Some(name) = &options.name {
                        ensure_unique_test_name(spec, name, Some(test_id))?;
                    }
                    let test = spec
                        .testing
                        .tests
                        .iter_mut()
                        .find(|test| test.id == test_id)
                        .context("test case was not found")?;
                    if let Some(name) = &options.name {
                        test.name = normalize_name(name)?;
                    }
                    if let Some(group_ids) = group_ids {
                        test.group_ids = group_ids;
                    }
                    Ok(())
                },
            )?;
            output.emit(
                "test case update",
                &spec.testing.tests,
                &format!("Updated test case {}", options.selector),
            )
        }
        TestCaseCommand::Remove { selector } => {
            let mut inventory_removed = Vec::new();
            let mut files_preserved = Vec::new();
            let spec = reporch_cli::local_project_v2::update_authoring_spec(
                Path::new("."),
                |root, spec| {
                    let id = find_test(spec, &selector)?.id;
                    let test = spec
                        .testing
                        .tests
                        .iter()
                        .find(|test| test.id == id)
                        .context("test case was not found")?;
                    files_preserved.push(test.input_file.clone());
                    files_preserved.extend(test.answer_file.iter().cloned());
                    let before = spec.testing.tests.len();
                    spec.testing.tests.retain(|test| test.id != id);
                    ensure!(
                        before != spec.testing.tests.len(),
                        "test case was not found"
                    );
                    for submission in &mut spec.output_submissions {
                        submission.outputs.remove(&id);
                    }
                    inventory_removed =
                        reporch_cli::local_project_v2::prune_unreferenced_file_declarations(
                            root,
                            spec,
                            files_preserved.clone(),
                        )?;
                    Ok(())
                },
            )?;
            files_preserved.sort();
            files_preserved.dedup();
            output.emit(
                "test case remove",
                &RemovalResult {
                    remaining: &spec.testing.tests,
                    inventory_removed,
                    files_preserved,
                },
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
    let spec = reporch_cli::local_project_v2::update_authoring_spec(&root, |root, spec| {
        let group_ids = resolve_group_ids(spec, &options.groups)?;
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
            ensure_unique_test_input(
                root,
                &input,
                &name,
                spec.testing
                    .tests
                    .iter()
                    .map(|test| (test.name.as_str(), test.input_file.as_str())),
            )?;
            reporch_cli::local_project_v2::declare_project_file(
                root,
                spec,
                &input,
                "text/plain",
                false,
            )?;
            if let Some(answer) = &answer {
                reporch_cli::local_project_v2::declare_project_file(
                    root,
                    spec,
                    answer,
                    "text/plain",
                    false,
                )?;
            }
            let id = Uuid::now_v7();
            imported.push(id);
            spec.testing.tests.push(TestCaseSpecV2 {
                id,
                name: name.clone(),
                role: inferred_role(&name),
                origin: TestCaseOriginV2::Uploaded,
                input_file: input,
                answer_file: answer,
                group_ids: group_ids.clone(),
                points: None,
                generated: None,
            });
        }
        Ok(())
    })?;
    output.emit(
        "test case import",
        &serde_json::json!({ "imported_ids": imported, "tests": spec.testing.tests }),
        &format!("Imported {} test case(s)", imported.len()),
    )
}

fn test_group(command: TestGroupCommand, output: &CliOutput) -> Result<()> {
    match command {
        TestGroupCommand::List => {
            let root = reporch_cli::local_project::discover_project(Path::new("."))?;
            let spec = reporch_cli::local_project_v2::read_authoring_spec(&root)?;
            output.emit(
                "test group list",
                &spec.testing.groups,
                &format!("{} group(s)", spec.testing.groups.len()),
            )
        }
        TestGroupCommand::Add(options) => {
            super::validate_group_points(options.points)?;
            let spec = reporch_cli::local_project_v2::update_authoring_spec(
                Path::new("."),
                |_root, spec| {
                    validate_group_id(&options.id)?;
                    ensure!(
                        !spec
                            .testing
                            .groups
                            .iter()
                            .any(|group| group.name == options.id),
                        "group already exists: {}",
                        options.id
                    );
                    let depends_on = resolve_group_ids(spec, &options.depends_on)?;
                    spec.testing.groups.push(TestGroupSpecV2 {
                        id: Uuid::now_v7(),
                        name: options.id.clone(),
                        points: options.points,
                        depends_on,
                        feedback_policy: studio_core::GroupFeedbackPolicyV1::Complete,
                        aggregation: if spec.problem_type == studio_core::ProblemType::Scored {
                            ScoreAggregationV2::GroupMinimum
                        } else {
                            ScoreAggregationV2::AllOrNothing
                        },
                    });
                    ensure_v2_group_dependencies_acyclic(&spec.testing.groups)?;
                    Ok(())
                },
            )?;
            output.emit(
                "test group add",
                &spec.testing.groups,
                &group_points_feedback_v2(
                    spec.problem_type,
                    &spec.testing.groups,
                    &format!("Added group {}", options.id),
                    &options.id,
                ),
            )
        }
        TestGroupCommand::Update(options) => {
            if let Some(points) = options.points {
                super::validate_group_points(points)?;
            }
            let spec = reporch_cli::local_project_v2::update_authoring_spec(
                Path::new("."),
                |_root, spec| {
                    let current_id = find_group(spec, &options.id)?.id;
                    let depends_on = if options.depends_on.is_empty() {
                        None
                    } else {
                        let ids = resolve_group_ids(spec, &options.depends_on)?;
                        ensure!(
                            !ids.contains(&current_id),
                            "a group cannot depend on itself"
                        );
                        Some(ids)
                    };
                    let group = spec
                        .testing
                        .groups
                        .iter_mut()
                        .find(|group| group.id == current_id)
                        .context("group was not found")?;
                    if let Some(points) = options.points {
                        group.points = points;
                    }
                    if let Some(depends_on) = depends_on {
                        group.depends_on = depends_on;
                    }
                    ensure_v2_group_dependencies_acyclic(&spec.testing.groups)?;
                    Ok(())
                },
            )?;
            output.emit(
                "test group update",
                &spec.testing.groups,
                &group_points_feedback_v2(
                    spec.problem_type,
                    &spec.testing.groups,
                    &format!("Updated group {}", options.id),
                    &options.id,
                ),
            )
        }
        TestGroupCommand::Remove { id } => {
            let spec = reporch_cli::local_project_v2::update_authoring_spec(
                Path::new("."),
                |_root, spec| {
                    let group_id = find_group(spec, &id)?.id;
                    ensure!(
                        !spec
                            .testing
                            .tests
                            .iter()
                            .any(|test| test.group_ids.contains(&group_id)),
                        "group is still used by a test case"
                    );
                    ensure!(
                        !spec
                            .testing
                            .groups
                            .iter()
                            .any(|group| group.depends_on.contains(&group_id)),
                        "another group still depends on this group"
                    );
                    let before = spec.testing.groups.len();
                    spec.testing.groups.retain(|group| group.id != group_id);
                    ensure!(before != spec.testing.groups.len(), "group was not found");
                    Ok(())
                },
            )?;
            output.emit(
                "test group remove",
                &spec.testing.groups,
                &format!("Removed group {id}"),
            )
        }
    }
}

pub(super) async fn generator(options: GeneratorOptions, output: &CliOutput) -> Result<()> {
    match options.command {
        GeneratorCommand::List => {
            let root = reporch_cli::local_project::discover_project(Path::new("."))?;
            let spec = reporch_cli::local_project_v2::read_authoring_spec(&root)?;
            output.emit(
                "generator list",
                &spec.testing.generators,
                &format!("{} generator(s)", spec.testing.generators.len()),
            )
        }
        GeneratorCommand::Add(options) => {
            let source = relative_string(&options.source)?;
            let spec = reporch_cli::local_project_v2::update_authoring_spec(
                Path::new("."),
                |root, spec| {
                    ensure!(
                        !spec
                            .testing
                            .generators
                            .iter()
                            .any(|generator| generator.program.name == options.id),
                        "generator already exists: {}",
                        options.id
                    );
                    reporch_cli::local_project_v2::declare_project_file(
                        root,
                        spec,
                        &source,
                        source_media_type(&options.language),
                        false,
                    )?;
                    spec.testing.generators.push(GeneratorSpecV2 {
                        program: ProgramSpecV2 {
                            id: Uuid::now_v7(),
                            name: normalize_name(&options.id)?,
                            source_path: source.clone(),
                            language: options.language.clone(),
                            arguments: options.arguments.clone(),
                        },
                        recipes: Vec::new(),
                    });
                    Ok(())
                },
            )?;
            output.emit(
                "generator add",
                &spec.testing.generators,
                &format!("Added generator {}", options.id),
            )
        }
        GeneratorCommand::Run(options) => {
            let seed = options
                .seed
                .context("generator run requires --seed for deterministic replay")?;
            let root = reporch_cli::local_project::discover_project(Path::new("."))?;
            let spec = reporch_cli::local_project_v2::read_authoring_spec(&root)?;
            let group_ids = resolve_group_ids(&spec, &options.groups)?;
            let generator = find_generator(&spec, &options.id)?.clone();
            let path = relative_string(&options.output)?;
            let name = normalize_name(options.name.as_deref().unwrap_or(&options.id))?;
            ensure_unique_test_name(&spec, &name, None)?;
            let run_options = options.runtime.into_run_options(output);
            let program = legacy_program(&generator.program);
            let bytes = materialize_generator(
                &root,
                &program,
                &options.arguments,
                Some(seed),
                &run_options,
            )
            .await?;
            write_project_bytes_atomic(&root, &path, &bytes)?;
            let test_id = Uuid::now_v7();
            let recipe_id = Uuid::now_v7();
            let updated =
                reporch_cli::local_project_v2::update_authoring_spec(&root, |root, spec| {
                    ensure_unique_test_name(spec, &name, None)?;
                    reporch_cli::local_project_v2::declare_project_file(
                        root,
                        spec,
                        &path,
                        "text/plain",
                        false,
                    )?;
                    let target = spec
                        .testing
                        .generators
                        .iter_mut()
                        .find(|candidate| candidate.program.id == generator.program.id)
                        .context("generator was removed during materialization")?;
                    target.recipes.push(GeneratorRecipeSpecV2 {
                        id: recipe_id,
                        name: format!("case-{name}"),
                        argument_template: options.arguments.clone(),
                        parameters: Default::default(),
                        matrix: GeneratorMatrixStrategyV2::Cartesian,
                        seed_start: seed,
                        seed_step: 1,
                        count: 1,
                        group_ids: group_ids.clone(),
                    });
                    spec.testing.tests.push(TestCaseSpecV2 {
                        id: test_id,
                        name: name.clone(),
                        role: inferred_role(&name),
                        origin: TestCaseOriginV2::Generated,
                        input_file: path.clone(),
                        answer_file: None,
                        group_ids: group_ids.clone(),
                        points: None,
                        generated: Some(GeneratedCaseRefV2 {
                            generator_id: generator.program.id,
                            recipe_id,
                            ordinal: 0,
                            seed,
                        }),
                    });
                    Ok(())
                })?;
            let result = GeneratorMaterialization {
                generator_id: generator.program.name,
                test_ids: vec![test_id],
                paths: vec![path],
                sha256: vec![hex::encode(Sha256::digest(&bytes))],
            };
            output.emit(
                "generator run",
                &result,
                &format!(
                    "Generated {} ({test_id})",
                    updated.testing.tests.last().unwrap().name
                ),
            )
        }
        GeneratorCommand::Recipe(options) => {
            ensure!(
                (1..=10_000).contains(&options.count),
                "recipe count must be between 1 and 10000"
            );
            let root = reporch_cli::local_project::discover_project(Path::new("."))?;
            let spec = reporch_cli::local_project_v2::read_authoring_spec(&root)?;
            let group_ids = resolve_group_ids(&spec, &options.groups)?;
            let generator = find_generator(&spec, &options.id)?.clone();
            let prefix = normalize_name(&options.name_prefix)?;
            let directory = relative_string(&options.output_directory)?;
            let run_options = options.runtime.into_run_options(output);
            let program = legacy_program(&generator.program);
            let recipe_id = Uuid::now_v7();
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
                    &program,
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
            reporch_cli::local_project_v2::update_authoring_spec(&root, |root, spec| {
                for (_, name, _, _, _) in &materialized {
                    ensure_unique_test_name(spec, name, None)?;
                }
                let target = spec
                    .testing
                    .generators
                    .iter_mut()
                    .find(|candidate| candidate.program.id == generator.program.id)
                    .context("generator was removed during materialization")?;
                target.recipes.push(GeneratorRecipeSpecV2 {
                    id: recipe_id,
                    name: prefix.clone(),
                    argument_template: options.arguments.clone(),
                    parameters: Default::default(),
                    matrix: GeneratorMatrixStrategyV2::Cartesian,
                    seed_start: options.seed_start,
                    seed_step: 1,
                    count: options.count,
                    group_ids: group_ids.clone(),
                });
                for (ordinal, (id, name, path, seed, _)) in materialized.iter().enumerate() {
                    reporch_cli::local_project_v2::declare_project_file(
                        root,
                        spec,
                        path,
                        "text/plain",
                        false,
                    )?;
                    spec.testing.tests.push(TestCaseSpecV2 {
                        id: *id,
                        name: name.clone(),
                        role: inferred_role(name),
                        origin: TestCaseOriginV2::Generated,
                        input_file: path.clone(),
                        answer_file: None,
                        group_ids: group_ids.clone(),
                        points: None,
                        generated: Some(GeneratedCaseRefV2 {
                            generator_id: generator.program.id,
                            recipe_id,
                            ordinal: u32::try_from(ordinal)
                                .context("recipe ordinal exceeds u32")?,
                            seed: *seed,
                        }),
                    });
                }
                Ok(())
            })?;
            let result = GeneratorMaterialization {
                generator_id: generator.program.name,
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
            let mut inventory_removed = Vec::new();
            let mut files_preserved = Vec::new();
            let spec = reporch_cli::local_project_v2::update_authoring_spec(
                Path::new("."),
                |root, spec| {
                    let generator = find_generator(spec, &id)?;
                    let generator_id = generator.program.id;
                    files_preserved.push(generator.program.source_path.clone());
                    ensure!(
                        !spec.testing.tests.iter().any(|test| test
                            .generated
                            .as_ref()
                            .is_some_and(|generated| generated.generator_id == generator_id)),
                        "generator is still used by a test case"
                    );
                    ensure!(
                        !spec
                            .testing
                            .stress_suites
                            .iter()
                            .any(|suite| suite.generator_id == generator_id),
                        "generator is still used by a stress suite"
                    );
                    let before = spec.testing.generators.len();
                    spec.testing
                        .generators
                        .retain(|generator| generator.program.id != generator_id);
                    ensure!(
                        before != spec.testing.generators.len(),
                        "generator was not found"
                    );
                    inventory_removed =
                        reporch_cli::local_project_v2::prune_unreferenced_file_declarations(
                            root,
                            spec,
                            files_preserved.clone(),
                        )?;
                    Ok(())
                },
            )?;
            output.emit(
                "generator remove",
                &RemovalResult {
                    remaining: &spec.testing.generators,
                    inventory_removed,
                    files_preserved,
                },
                &format!("Removed generator {id}"),
            )
        }
    }
}

pub(super) async fn validator(options: ValidatorOptions, output: &CliOutput) -> Result<()> {
    match options.command {
        ValidatorCommand::Set {
            source,
            language,
            extra,
        } => {
            let source = relative_string(&source)?;
            let spec = reporch_cli::local_project_v2::update_authoring_spec(
                Path::new("."),
                |root, spec| {
                    reporch_cli::local_project_v2::declare_project_file(
                        root,
                        spec,
                        &source,
                        source_media_type(&language),
                        false,
                    )?;
                    let program = ProgramSpecV2 {
                        id: Uuid::now_v7(),
                        name: if extra {
                            format!("extra-{}", spec.testing.validators.extra.len() + 1)
                        } else {
                            "primary".into()
                        },
                        source_path: source.clone(),
                        language: language.clone(),
                        arguments: Vec::new(),
                    };
                    if extra {
                        spec.testing.validators.extra.push(program);
                    } else {
                        if source != "validators/input.py"
                            && spec
                                .testing
                                .validators
                                .primary
                                .as_ref()
                                .is_some_and(|primary| primary.source_path == "validators/input.py")
                        {
                            spec.testing.validators.unit_tests.retain(|unit| {
                                !is_starter_validator_unit(
                                    &unit.name,
                                    &unit.input_file,
                                    unit.expected_valid,
                                )
                            });
                        }
                        spec.testing.validators.primary = Some(program);
                    }
                    Ok(())
                },
            )?;
            output.emit(
                "validator set",
                &spec.testing.validators,
                &format!("Configured validator {source}"),
            )
        }
        ValidatorCommand::UnitAdd(options) => {
            let root = reporch_cli::local_project::discover_project(Path::new("."))?;
            let materialized = materialize_validator_unit_input(&root, &options)?;
            let input = materialized.path.clone();
            let updated =
                reporch_cli::local_project_v2::update_authoring_spec(&root, |root, spec| {
                    ensure!(
                        !spec
                            .testing
                            .validators
                            .unit_tests
                            .iter()
                            .any(|unit| unit.name == options.name),
                        "validator unit already exists: {}",
                        options.name
                    );
                    reporch_cli::local_project_v2::declare_project_file(
                        root,
                        spec,
                        &input,
                        "text/plain",
                        false,
                    )?;
                    spec.testing
                        .validators
                        .unit_tests
                        .push(ValidatorUnitSpecV2 {
                            id: Uuid::now_v7(),
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
                &spec.testing.validators.unit_tests,
                &format!("Added validator unit {}", options.name),
            )
        }
        ValidatorCommand::Run { name, runtime } => {
            let root = reporch_cli::local_project::discover_project(Path::new("."))?;
            let spec = reporch_cli::local_project_v2::read_authoring_spec(&root)?;
            let validators = spec
                .testing
                .validators
                .primary
                .iter()
                .chain(spec.testing.validators.extra.iter())
                .collect::<Vec<_>>();
            ensure!(!validators.is_empty(), "no validator is configured");
            let units = selected_by_name(
                &spec.testing.validators.unit_tests,
                name.as_deref(),
                |unit| unit.name.as_str(),
            )?;
            ensure!(!units.is_empty(), "no validator unit tests are configured");
            let run_options = runtime.into_run_options(output);
            let mut cases = Vec::new();
            for validator in validators {
                for unit in &units {
                    output.progress(
                        "validator run",
                        &format!("Running validator {} · unit {}", validator.name, unit.name),
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
                        program: validator.name.clone(),
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

pub(super) async fn checker(options: CheckerOptions, output: &CliOutput) -> Result<()> {
    match options.command {
        CheckerCommand::ListStandard => output.emit(
            "checker list-standard",
            &["exact", "token", "case-insensitive", "floating", "custom"],
            "exact, token, case-insensitive, floating, custom",
        ),
        CheckerCommand::Protocol { command } => match command {
            CheckerProtocolCommand::Show => {
                let root = reporch_cli::local_project::discover_project(Path::new("."))?;
                let spec = reporch_cli::local_project_v2::read_authoring_spec(&root)?;
                let CheckerSpec::Custom { protocol, .. } = spec.testing.checker.checker else {
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
                let spec = reporch_cli::local_project_v2::update_authoring_spec(
                    Path::new("."),
                    |_, spec| {
                        let CheckerSpec::Custom {
                            protocol: current, ..
                        } = &mut spec.testing.checker.checker
                        else {
                            bail!("checker protocol is available only for a custom checker")
                        };
                        *current = protocol;
                        Ok(())
                    },
                )?;
                output.emit(
                    "checker protocol set",
                    &spec.testing.checker.checker,
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
                    super::validate_floating_tolerances(&absolute_error, &relative_error)?;
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
            let spec = reporch_cli::local_project_v2::update_authoring_spec(
                Path::new("."),
                |root, spec| {
                    if let (Some(source), Some(language)) = (&source, &language) {
                        reporch_cli::local_project_v2::declare_project_file(
                            root,
                            spec,
                            source,
                            source_media_type(language),
                            false,
                        )?;
                    }
                    spec.testing.checker.checker = checker.clone();
                    Ok(())
                },
            )?;
            output.emit("checker set", &spec.testing.checker, "Configured checker")
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
            let spec = reporch_cli::local_project_v2::update_authoring_spec(
                Path::new("."),
                |root, spec| {
                    ensure!(
                        !spec
                            .testing
                            .checker
                            .unit_tests
                            .iter()
                            .any(|unit| unit.name == name),
                        "checker unit already exists: {name}"
                    );
                    for path in [&input, &answer, &actual_output] {
                        reporch_cli::local_project_v2::declare_project_file(
                            root,
                            spec,
                            path,
                            "text/plain",
                            false,
                        )?;
                    }
                    spec.testing.checker.unit_tests.push(CheckerUnitSpecV2 {
                        id: Uuid::now_v7(),
                        name: normalize_name(&name)?,
                        input_file: input.clone(),
                        answer_file: answer.clone(),
                        output_file: actual_output.clone(),
                        expected_accepted: matches!(expected, CheckerExpectation::Accept),
                    });
                    Ok(())
                },
            )?;
            output.emit(
                "checker unit-add",
                &spec.testing.checker.unit_tests,
                &format!("Added checker unit {name}"),
            )
        }
        CheckerCommand::Run { name, runtime } => {
            let root = reporch_cli::local_project::discover_project(Path::new("."))?;
            let spec = reporch_cli::local_project_v2::read_authoring_spec(&root)?;
            let units =
                selected_by_name(&spec.testing.checker.unit_tests, name.as_deref(), |unit| {
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
                    } = &spec.testing.checker.checker
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
                            &spec.testing.checker.checker,
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

pub(super) fn solution(options: SolutionOptions, output: &CliOutput) -> Result<()> {
    match options.command {
        SolutionCommand::List => emit_solutions("solution list", output),
        SolutionCommand::Matrix => emit_solutions("solution matrix", output),
        SolutionCommand::Add(options) => {
            let source = relative_string(&options.source)?;
            let expected_score = score_range(
                options.minimum_score,
                options.maximum_score,
                options.expected,
            )?;
            let spec = reporch_cli::local_project_v2::update_authoring_spec(
                Path::new("."),
                |root, spec| {
                    ensure!(
                        !spec
                            .testing
                            .solutions
                            .iter()
                            .any(|solution| solution.program.name == options.name),
                        "solution already exists: {}",
                        options.name
                    );
                    reporch_cli::local_project_v2::declare_project_file(
                        root,
                        spec,
                        &source,
                        source_media_type(&options.language),
                        false,
                    )?;
                    let expected_verdict = options.expected.into();
                    let role =
                        options
                            .role
                            .map(Into::into)
                            .unwrap_or_else(|| match expected_verdict {
                                studio_core::ExpectedVerdict::Accepted
                                    if !spec.testing.solutions.iter().any(|solution| {
                                        solution.role == SolutionRoleV2::Reference
                                    }) =>
                                {
                                    SolutionRoleV2::Reference
                                }
                                studio_core::ExpectedVerdict::WrongAnswer => {
                                    SolutionRoleV2::KnownWrong
                                }
                                _ => SolutionRoleV2::Alternative,
                            });
                    validate_solution_role(expected_verdict, role)?;
                    ensure_reference_is_available(spec, role, None)?;
                    spec.testing.solutions.push(SolutionSpecV2 {
                        program: ProgramSpecV2 {
                            id: Uuid::now_v7(),
                            name: normalize_name(&options.name)?,
                            source_path: source.clone(),
                            language: options.language.clone(),
                            arguments: Vec::new(),
                        },
                        role,
                        expected_verdict,
                        expected_score: expected_score.clone(),
                        group_expectations: Vec::new(),
                        tags: Vec::new(),
                        notes: String::new(),
                    });
                    Ok(())
                },
            )?;
            output.emit(
                "solution add",
                &spec.testing.solutions,
                &format!("Added solution {}", options.name),
            )
        }
        SolutionCommand::Update(options) => {
            let score_was_supplied =
                options.minimum_score.is_some() || options.maximum_score.is_some();
            let spec = reporch_cli::local_project_v2::update_authoring_spec(
                Path::new("."),
                |_root, spec| {
                    let solution_index = spec
                        .testing
                        .solutions
                        .iter()
                        .position(|solution| solution.program.name == options.name)
                        .context("solution was not found")?;
                    let current = &spec.testing.solutions[solution_index];
                    let expected_verdict = options
                        .expected
                        .map(Into::into)
                        .unwrap_or(current.expected_verdict);
                    let role = options.role.map(Into::into).unwrap_or(current.role);
                    let expected_score = if options.expected.is_some() || score_was_supplied {
                        score_range_for_verdict(
                            options.minimum_score,
                            options.maximum_score,
                            expected_verdict,
                        )?
                    } else {
                        current.expected_score.clone()
                    };
                    validate_solution_role(expected_verdict, role)?;
                    ensure_reference_is_available(spec, role, Some(solution_index))?;

                    let solution = &mut spec.testing.solutions[solution_index];
                    solution.expected_verdict = expected_verdict;
                    solution.role = role;
                    solution.expected_score = expected_score;
                    Ok(())
                },
            )?;
            output.emit(
                "solution update",
                &spec.testing.solutions,
                &format!("Updated solution {}", options.name),
            )
        }
        SolutionCommand::Remove { name } => {
            let mut inventory_removed = Vec::new();
            let mut files_preserved = Vec::new();
            let spec = reporch_cli::local_project_v2::update_authoring_spec(
                Path::new("."),
                |root, spec| {
                    let solution = find_solution(spec, &name)?;
                    let solution_id = solution.program.id;
                    files_preserved.push(solution.program.source_path.clone());
                    ensure!(
                        !spec.testing.stress_suites.iter().any(|suite| {
                            suite.oracle_solution_id == solution_id
                                || suite.candidate_solution_ids.contains(&solution_id)
                        }),
                        "solution is still used by a stress suite"
                    );
                    ensure!(
                        !spec
                            .execution
                            .interactive
                            .iter()
                            .flat_map(|interactive| interactive.unit_tests.iter())
                            .any(|unit| unit.solution_id == solution_id),
                        "solution is still used by an interactive unit"
                    );
                    let before = spec.testing.solutions.len();
                    spec.testing
                        .solutions
                        .retain(|solution| solution.program.id != solution_id);
                    ensure!(
                        before != spec.testing.solutions.len(),
                        "solution was not found"
                    );
                    inventory_removed =
                        reporch_cli::local_project_v2::prune_unreferenced_file_declarations(
                            root,
                            spec,
                            files_preserved.clone(),
                        )?;
                    Ok(())
                },
            )?;
            output.emit(
                "solution remove",
                &RemovalResult {
                    remaining: &spec.testing.solutions,
                    inventory_removed,
                    files_preserved,
                },
                &format!("Removed solution {name}"),
            )
        }
    }
}

fn emit_solutions(command: &'static str, output: &CliOutput) -> Result<()> {
    let root = reporch_cli::local_project::discover_project(Path::new("."))?;
    let spec = reporch_cli::local_project_v2::read_authoring_spec(&root)?;
    let human = if command == "solution matrix" {
        solution_matrix_human(&spec.testing.solutions)
    } else {
        format!("{} solution expectation(s)", spec.testing.solutions.len())
    };
    output.emit(command, &spec.testing.solutions, &human)
}

fn solution_matrix_human(solutions: &[SolutionSpecV2]) -> String {
    let mut lines = vec![format!("{} solution expectation(s):", solutions.len())];
    lines.extend(solutions.iter().map(|solution| {
        let score = solution
            .expected_score
            .as_ref()
            .map(|range| format!(" · score {}..{}", range.minimum, range.maximum))
            .unwrap_or_default();
        let groups = if solution.group_expectations.is_empty() {
            String::new()
        } else {
            format!(
                " · {} group expectation(s)",
                solution.group_expectations.len()
            )
        };
        format!(
            "- {} · role {} · {}{}{} · {}",
            human_safe(&solution.program.name),
            solution_role_name(solution.role),
            verdict_name(solution.expected_verdict),
            score,
            groups,
            human_safe(&solution.program.source_path)
        )
    }));
    lines.push("This lists expectations only; run `reporch verify` for execution evidence.".into());
    lines.join("\n")
}

fn solution_role_name(role: SolutionRoleV2) -> &'static str {
    match role {
        SolutionRoleV2::Reference => "reference",
        SolutionRoleV2::Alternative => "alternative",
        SolutionRoleV2::Oracle => "oracle",
        SolutionRoleV2::Brute => "brute",
        SolutionRoleV2::KnownWrong => "known-wrong",
    }
}

fn validate_solution_role(
    expected: studio_core::ExpectedVerdict,
    role: SolutionRoleV2,
) -> Result<()> {
    ensure!(
        !matches!(role, SolutionRoleV2::Reference | SolutionRoleV2::Oracle)
            || expected == studio_core::ExpectedVerdict::Accepted,
        "reference and oracle solutions must have expected verdict accepted"
    );
    ensure!(
        role != SolutionRoleV2::KnownWrong || expected != studio_core::ExpectedVerdict::Accepted,
        "known-wrong solutions cannot have expected verdict accepted"
    );
    Ok(())
}

fn ensure_reference_is_available(
    spec: &reporch_format::AuthoringSpecV2,
    requested_role: SolutionRoleV2,
    ignored_index: Option<usize>,
) -> Result<()> {
    if requested_role != SolutionRoleV2::Reference {
        return Ok(());
    }
    if let Some(existing) = spec
        .testing
        .solutions
        .iter()
        .enumerate()
        .find(|(index, solution)| {
            Some(*index) != ignored_index && solution.role == SolutionRoleV2::Reference
        })
        .map(|(_, solution)| &solution.program.name)
    {
        bail!(
            "reference solution already exists: {existing}; demote or remove it first with `reporch solution update {existing} --role alternative`"
        );
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct GeneratedAnswerResult {
    test_id: Uuid,
    test_name: String,
    path: String,
    sha256: String,
}

pub(super) async fn answer(options: AnswerOptions, output: &CliOutput) -> Result<()> {
    let AnswerCommand::Generate {
        solution,
        test,
        missing_only,
        runtime,
    } = options.command;
    let root = reporch_cli::local_project::discover_project(Path::new("."))?;
    let spec = reporch_cli::local_project_v2::read_authoring_spec(&root)?;
    let solution = find_solution(&spec, &solution)?.clone();
    ensure!(
        solution.expected_verdict == studio_core::ExpectedVerdict::Accepted
            && matches!(
                solution.role,
                SolutionRoleV2::Reference | SolutionRoleV2::Oracle
            ),
        "answer generation requires an accepted reference or oracle solution; `{}` is {:?} with {:?}; run `reporch solution update {} --role reference --expected accepted` or select another solution",
        solution.program.name,
        solution.role,
        solution.expected_verdict,
        solution.program.name,
    );
    let selected = spec
        .testing
        .tests
        .iter()
        .filter(|candidate| test.is_none_or(|test| candidate.id == test))
        .cloned()
        .collect::<Vec<_>>();
    ensure!(!selected.is_empty(), "no matching test case was found");
    let run_options = runtime.into_run_options(output);
    let mut generated = Vec::new();
    for test in selected {
        if test.answer_file.is_some() {
            ensure!(
                missing_only,
                "test {} already has an answer file; use --missing-only to skip it",
                test.name
            );
            continue;
        }
        let answer_path = answer_path_for(&test.input_file)?;
        let result = reporch_cli::authoring_runtime::run_program(
            &reporch_cli::authoring_runtime::ProgramRequest {
                project_directory: &root,
                source_path: &solution.program.source_path,
                language: &solution.program.language,
                arguments: &solution.program.arguments,
                stdin_path: Some(&test.input_file),
                options: &run_options,
            },
        )
        .await?;
        ensure!(
            result.exit_code == 0,
            "reference solution failed on {} with exit {}: {}",
            test.name,
            result.exit_code,
            result.stderr.trim()
        );
        generated.push((test, answer_path, result.stdout_bytes));
    }
    for (_, path, bytes) in &generated {
        write_project_bytes_atomic(&root, path, bytes)?;
    }
    reporch_cli::local_project_v2::update_authoring_spec(&root, |root, spec| {
        for (test, path, _) in &generated {
            reporch_cli::local_project_v2::declare_project_file(
                root,
                spec,
                path,
                "text/plain",
                false,
            )?;
            let target = spec
                .testing
                .tests
                .iter_mut()
                .find(|candidate| candidate.id == test.id)
                .context("test disappeared while saving generated answers")?;
            target.answer_file = Some(path.clone());
        }
        Ok(())
    })?;
    let results = generated
        .into_iter()
        .map(|(test, path, bytes)| GeneratedAnswerResult {
            test_id: test.id,
            test_name: test.name,
            path,
            sha256: hex::encode(Sha256::digest(bytes)),
        })
        .collect::<Vec<_>>();
    output.emit(
        "answer generate",
        &results,
        &format!("Generated {} answer file(s)", results.len()),
    )
}

#[derive(Debug, Serialize)]
struct StressCandidateResult {
    candidate: String,
    expected_counterexample: bool,
    counterexample_seed: Option<u64>,
    input_path: Option<String>,
    counterexample_sha256: Option<String>,
    passed: bool,
}

pub(super) async fn stress(options: StressOptions, output: &CliOutput) -> Result<()> {
    match options.command {
        StressCommand::List => {
            let root = reporch_cli::local_project::discover_project(Path::new("."))?;
            let spec = reporch_cli::local_project_v2::read_authoring_spec(&root)?;
            output.emit(
                "stress list",
                &spec.testing.stress_suites,
                &format!("{} stress suite(s)", spec.testing.stress_suites.len()),
            )
        }
        StressCommand::Add {
            name,
            generator,
            recipe,
            oracle,
            candidates,
            seed_start,
            cases,
            timeout_ms,
            minimize_failure,
        } => {
            ensure!(
                (1..=100_000).contains(&cases),
                "cases must be between 1 and 100000"
            );
            ensure!(
                (10..=600_000).contains(&timeout_ms),
                "timeout must be between 10 and 600000 ms"
            );
            let normalized_name = normalize_name(&name)?;
            let spec = reporch_cli::local_project_v2::update_authoring_spec(
                Path::new("."),
                |_root, spec| {
                    ensure!(
                        !spec
                            .testing
                            .stress_suites
                            .iter()
                            .any(|suite| suite.name == normalized_name),
                        "stress suite already exists: {normalized_name}"
                    );
                    let generator = find_generator(spec, &generator)?;
                    let recipe_id = Uuid::parse_str(&recipe).ok();
                    let recipe = generator
                        .recipes
                        .iter()
                        .find(|candidate| {
                            candidate.name == recipe || recipe_id == Some(candidate.id)
                        })
                        .with_context(|| format!("generator recipe was not found: {recipe}"))?;
                    let oracle = find_solution(spec, &oracle)?;
                    ensure!(
                        oracle.expected_verdict == studio_core::ExpectedVerdict::Accepted,
                        "stress oracle must be expected accepted"
                    );
                    let candidate_ids = candidates
                        .iter()
                        .map(|candidate| Ok(find_solution(spec, candidate)?.program.id))
                        .collect::<Result<Vec<_>>>()?;
                    ensure!(
                        !candidate_ids.contains(&oracle.program.id),
                        "the oracle cannot also be a stress candidate"
                    );
                    let mut unique = std::collections::BTreeSet::new();
                    ensure!(
                        candidate_ids.iter().all(|id| unique.insert(*id)),
                        "stress candidates must be unique"
                    );
                    spec.testing.stress_suites.push(StressSuiteSpecV2 {
                        id: Uuid::now_v7(),
                        name: normalized_name.clone(),
                        generator_id: generator.program.id,
                        recipe_id: recipe.id,
                        oracle_solution_id: oracle.program.id,
                        candidate_solution_ids: candidate_ids,
                        seed_start,
                        cases,
                        timeout_ms,
                        minimize_failure,
                    });
                    Ok(())
                },
            )?;
            output.emit(
                "stress add",
                &spec.testing.stress_suites,
                &format!("Added stress suite {normalized_name}"),
            )
        }
        StressCommand::Remove { name } => {
            let spec = reporch_cli::local_project_v2::update_authoring_spec(
                Path::new("."),
                |_root, spec| {
                    let parsed = Uuid::parse_str(&name).ok();
                    let before = spec.testing.stress_suites.len();
                    spec.testing
                        .stress_suites
                        .retain(|suite| suite.name != name && parsed != Some(suite.id));
                    ensure!(
                        before != spec.testing.stress_suites.len(),
                        "stress suite was not found"
                    );
                    Ok(())
                },
            )?;
            output.emit(
                "stress remove",
                &spec.testing.stress_suites,
                &format!("Removed stress suite {name}"),
            )
        }
        StressCommand::Run { name, runtime } => run_stress_suite(&name, runtime, output).await,
    }
}

async fn run_stress_suite(name: &str, runtime: RuntimeOptions, output: &CliOutput) -> Result<()> {
    let root = reporch_cli::local_project::discover_project(Path::new("."))?;
    let spec = reporch_cli::local_project_v2::read_authoring_spec(&root)?;
    let parsed = Uuid::parse_str(name).ok();
    let suite = spec
        .testing
        .stress_suites
        .iter()
        .find(|suite| suite.name == name || parsed == Some(suite.id))
        .with_context(|| format!("stress suite was not found: {name}"))?
        .clone();
    let generator = spec
        .testing
        .generators
        .iter()
        .find(|generator| generator.program.id == suite.generator_id)
        .context("stress generator is missing")?;
    let recipe = generator
        .recipes
        .iter()
        .find(|recipe| recipe.id == suite.recipe_id)
        .context("stress generator recipe is missing")?;
    let oracle = spec
        .testing
        .solutions
        .iter()
        .find(|solution| solution.program.id == suite.oracle_solution_id)
        .context("stress oracle is missing")?;
    let candidates = suite
        .candidate_solution_ids
        .iter()
        .map(|id| {
            spec.testing
                .solutions
                .iter()
                .find(|solution| solution.program.id == *id)
                .context("stress candidate is missing")
        })
        .collect::<Result<Vec<_>>>()?;
    let mut run_options = runtime.into_run_options(output);
    run_options.timeout = std::time::Duration::from_millis(suite.timeout_ms);
    let scratch_parent = root.join(".reporch").join("stress-tmp");
    fs::create_dir_all(&scratch_parent)?;
    let scratch = tempfile::Builder::new()
        .prefix("run-")
        .tempdir_in(&scratch_parent)?;
    if let Some(first_mismatch) = try_run_stress_batch(
        &root,
        scratch.path(),
        &suite,
        generator,
        recipe,
        oracle,
        &candidates,
        &spec.testing.checker.checker,
        &run_options,
        output,
    )
    .await?
    {
        return emit_stress_results(&root, &suite, &candidates, first_mismatch, output);
    }
    let mut first_mismatch =
        std::collections::BTreeMap::<Uuid, (u64, Vec<u8>, Vec<u8>, Vec<u8>)>::new();
    let mut last_progress = std::time::Instant::now();
    for ordinal in 0..suite.cases {
        let seed = suite
            .seed_start
            .checked_add(u64::from(ordinal))
            .context("stress seed range overflows u64")?;
        let input = materialize_generator(
            &root,
            &legacy_program(&generator.program),
            &recipe.argument_template,
            Some(seed),
            &run_options,
        )
        .await?;
        let input_path = scratch.path().join(format!("{seed}.in"));
        fs::write(&input_path, &input)?;
        let input_relative = input_path
            .strip_prefix(&root)?
            .to_str()
            .context("stress scratch path is not valid Unicode")?
            .to_owned();
        let oracle_result = run_solution(&root, oracle, &input_relative, &run_options).await?;
        ensure!(
            oracle_result.exit_code == 0,
            "stress oracle failed for seed {seed}: {}",
            oracle_result.stderr.trim()
        );
        for candidate in &candidates {
            if first_mismatch.contains_key(&candidate.program.id) {
                continue;
            }
            let candidate_result =
                run_solution(&root, candidate, &input_relative, &run_options).await?;
            let matches = candidate_result.exit_code == 0
                && stress_outputs_match(
                    &root,
                    scratch.path(),
                    &input_relative,
                    seed,
                    candidate,
                    &spec.testing.checker.checker,
                    &oracle_result.stdout_bytes,
                    &candidate_result.stdout_bytes,
                    &run_options,
                )
                .await?;
            if !matches {
                let (input, oracle_output, candidate_output) = if suite.minimize_failure {
                    minimize_counterexample(
                        &root,
                        scratch.path(),
                        seed,
                        candidate,
                        oracle,
                        &spec.testing.checker.checker,
                        input.clone(),
                        oracle_result.stdout_bytes.clone(),
                        candidate_result.stdout_bytes,
                        &run_options,
                    )
                    .await?
                } else {
                    (
                        input.clone(),
                        oracle_result.stdout_bytes.clone(),
                        candidate_result.stdout_bytes,
                    )
                };
                first_mismatch.insert(
                    candidate.program.id,
                    (seed, input, oracle_output, candidate_output),
                );
            }
        }
        let completed = ordinal + 1;
        if completed == suite.cases
            || completed % 10 == 0
            || last_progress.elapsed() >= std::time::Duration::from_secs(2)
        {
            output.progress(
                "stress run",
                &format!(
                    "Stress suite {}: {completed}/{} seed(s) checked",
                    suite.name, suite.cases
                ),
            );
            last_progress = std::time::Instant::now();
        }
    }
    emit_stress_results(&root, &suite, &candidates, first_mismatch, output)
}

fn emit_stress_results(
    root: &Path,
    suite: &StressSuiteSpecV2,
    candidates: &[&SolutionSpecV2],
    mut first_mismatch: std::collections::BTreeMap<Uuid, (u64, Vec<u8>, Vec<u8>, Vec<u8>)>,
    output: &CliOutput,
) -> Result<()> {
    let mut results = Vec::new();
    for candidate in candidates {
        let mismatch = first_mismatch.remove(&candidate.program.id);
        let expected_counterexample =
            candidate.expected_verdict != studio_core::ExpectedVerdict::Accepted;
        let passed = mismatch.is_some() == expected_counterexample;
        let (counterexample_seed, input_path, counterexample_sha256) =
            if let Some((seed, input, oracle, actual)) = mismatch {
                let base = format!("stress-failures/{}-{seed}", suite.name);
                write_project_bytes_once_or_same(&root, &format!("{base}.in"), &input)?;
                write_project_bytes_once_or_same(&root, &format!("{base}.oracle"), &oracle)?;
                write_project_bytes_once_or_same(
                    &root,
                    &format!("{base}.{}.out", candidate.program.name),
                    &actual,
                )?;
                (
                    Some(seed),
                    Some(format!("{base}.in")),
                    Some(hex::encode(Sha256::digest(&input))),
                )
            } else {
                (None, None, None)
            };
        results.push(StressCandidateResult {
            candidate: candidate.program.name.clone(),
            expected_counterexample,
            counterexample_seed,
            input_path,
            counterexample_sha256,
            passed,
        });
    }
    ensure!(
        results.iter().all(|result| result.passed),
        "stress expectations failed: {}",
        serde_json::to_string(&results)?
    );
    output.emit(
        "stress run",
        &results,
        &format!(
            "Stress suite {} passed {} candidate(s)",
            suite.name,
            results.len()
        ),
    )
}

#[allow(clippy::too_many_arguments)]
async fn try_run_stress_batch(
    root: &Path,
    scratch: &Path,
    suite: &StressSuiteSpecV2,
    generator: &GeneratorSpecV2,
    recipe: &GeneratorRecipeSpecV2,
    oracle: &SolutionSpecV2,
    candidates: &[&SolutionSpecV2],
    checker: &CheckerSpec,
    options: &reporch_cli::authoring_runtime::AuthoringRunOptions,
    output: &CliOutput,
) -> Result<Option<std::collections::BTreeMap<Uuid, (u64, Vec<u8>, Vec<u8>, Vec<u8>)>>> {
    if options.runtime != reporch_cli::local_sandbox::OciRuntime::Auto
        && std::env::var_os("REPORCH_DEBUG_ENABLE_STRESS_BATCH").is_none()
    {
        return Ok(None);
    }
    if suite.minimize_failure
        || matches!(
            checker,
            CheckerSpec::Custom { .. } | CheckerSpec::Floating { .. }
        )
    {
        return Ok(None);
    }
    let language = reporch_cli::toolchain::resolve_for_language(
        options.toolchain_id.as_deref(),
        &generator.program.language,
    )?
    .language;
    if matches!(language.as_str(), "java" | "csharp") {
        return Ok(None);
    }
    for program in
        std::iter::once(&oracle.program).chain(candidates.iter().map(|value| &value.program))
    {
        let candidate_language = reporch_cli::toolchain::resolve_for_language(
            options.toolchain_id.as_deref(),
            &program.language,
        )?
        .language;
        if candidate_language != language {
            return Ok(None);
        }
    }
    let jobs_per_seed = 3_u64.saturating_add(candidates.len() as u64);
    let total_timeout_ms = suite
        .timeout_ms
        .saturating_mul(u64::from(suite.cases))
        .saturating_mul(jobs_per_seed)
        .saturating_add(30_000);
    if total_timeout_ms > 600_000 || candidates.len() + 3 > 256 {
        return Ok(None);
    }

    let script_path = scratch.join("batch.sh");
    let mut script = String::from("#!/bin/sh\nset -eu\n");
    script.push_str("generator_src=$1\noracle_src=$2\n");
    for index in 0..candidates.len() {
        writeln!(&mut script, "candidate_{index}_src=${}", index + 3)?;
    }
    let mut generator_arguments = generator.program.arguments.clone();
    generator_arguments.extend(recipe.argument_template.iter().cloned());
    let (generator_setup, generator_command) = stress_program_shell(
        &language,
        "generator",
        "$generator_src",
        &generator_arguments,
    )?;
    let (oracle_setup, oracle_command) = stress_program_shell(
        &language,
        "oracle",
        "$oracle_src",
        &oracle.program.arguments,
    )?;
    script.push_str(&generator_setup);
    script.push_str(&oracle_setup);
    let mut candidate_commands = Vec::with_capacity(candidates.len());
    for (index, candidate) in candidates.iter().enumerate() {
        let label = format!("candidate_{index}");
        let source = format!("$candidate_{index}_src");
        let (setup, command) =
            stress_program_shell(&language, &label, &source, &candidate.program.arguments)?;
        script.push_str(&setup);
        candidate_commands.push(command);
    }
    writeln!(&mut script, "seed={}", suite.seed_start)?;
    script.push_str("ordinal=0\n");
    writeln!(
        &mut script,
        "while [ \"$ordinal\" -lt {} ]; do",
        suite.cases
    )?;
    script.push_str("  input=/run/reporch/stress-input\n  repeat=/run/reporch/stress-repeat\n  oracle=/run/reporch/stress-oracle\n");
    let generator_with_seed = format!("{generator_command} \"$seed\"");
    append_checked_stress_command(
        &mut script,
        &generator_with_seed,
        suite.timeout_ms,
        "$input",
        None,
        90,
    )?;
    append_checked_stress_command(
        &mut script,
        &generator_with_seed,
        suite.timeout_ms,
        "$repeat",
        None,
        91,
    )?;
    script.push_str("  cmp -s \"$input\" \"$repeat\" || exit 92\n");
    append_checked_stress_command(
        &mut script,
        &oracle_command,
        suite.timeout_ms,
        "$oracle",
        Some("$input"),
        93,
    )?;
    for (index, command) in candidate_commands.iter().enumerate() {
        writeln!(
            &mut script,
            "  if [ ! -e /run/reporch/found-{index} ]; then"
        )?;
        writeln!(
            &mut script,
            "    candidate=/run/reporch/stress-candidate-{index}"
        )?;
        script.push_str("    set +e\n");
        writeln!(
            &mut script,
            "    {} < \"$input\" > \"$candidate\" 2>/run/reporch/candidate.err",
            timed_stress_command(command, suite.timeout_ms)
        )?;
        script.push_str("    status=$?\n    set -e\n    mismatch=0\n");
        script.push_str("    if [ \"$status\" -ne 0 ]; then mismatch=1; fi\n");
        writeln!(
            &mut script,
            "    if [ \"$mismatch\" -eq 0 ] && ! {}; then mismatch=1; fi",
            stress_compare_shell(checker, index)
        )?;
        script.push_str("    if [ \"$mismatch\" -eq 1 ]; then\n");
        writeln!(&mut script, "      printf 'M\\t{index}\\t%s\\t' \"$seed\"")?;
        script.push_str("      base64 < \"$input\" | tr -d '\\n'\n      printf '\\t'\n      base64 < \"$oracle\" | tr -d '\\n'\n      printf '\\t'\n      base64 < \"$candidate\" | tr -d '\\n'\n      printf '\\n'\n");
        writeln!(&mut script, "      : > /run/reporch/found-{index}")?;
        script.push_str("    fi\n  fi\n");
    }
    script.push_str("  ordinal=$((ordinal + 1))\n  seed=$((seed + 1))\ndone\nprintf 'D\\n'\n");

    let mut file = fs::File::create(&script_path)?;
    file.write_all(script.as_bytes())?;
    file.sync_all()?;
    let relative_script = project_relative(root, &script_path)?;
    let mut command = vec!["bash".to_owned(), format!("/workspace/{relative_script}")];
    command.push(format!("/workspace/{}", generator.program.source_path));
    command.push(format!("/workspace/{}", oracle.program.source_path));
    command.extend(
        candidates
            .iter()
            .map(|candidate| format!("/workspace/{}", candidate.program.source_path)),
    );
    let mut batch_options = options.clone();
    batch_options.timeout = std::time::Duration::from_millis(total_timeout_ms);
    output.progress(
        "stress run",
        &format!(
            "Stress suite {}: running {} seed(s) in one VM",
            suite.name, suite.cases
        ),
    );
    let result = reporch_cli::authoring_runtime::run_toolchain_command(
        root,
        &language,
        command,
        &batch_options,
    )
    .await?;
    if result.termination != reporch_runtime_core::GuestTerminationV2::Exited
        || result.exit_code != 0
    {
        return Err(crate::cli_output::domain_error(
            if result.termination == reporch_runtime_core::GuestTerminationV2::TimedOut {
                "runtime.execution_timed_out"
            } else {
                "runtime.execution_failed"
            },
            format!(
                "batched stress runtime ended as {:?} with exit code {}",
                result.termination, result.exit_code
            ),
            &result,
        ));
    }
    let mut mismatches = std::collections::BTreeMap::new();
    let mut completed = false;
    for line in result.stdout.lines() {
        if line == "D" {
            completed = true;
            continue;
        }
        let fields = line.split('\t').collect::<Vec<_>>();
        ensure!(
            fields.len() == 6 && fields[0] == "M",
            "invalid batched stress result frame"
        );
        let index = fields[1]
            .parse::<usize>()
            .context("parse batched stress candidate index")?;
        let candidate = candidates
            .get(index)
            .context("batched stress candidate index is out of range")?;
        let seed = fields[2]
            .parse::<u64>()
            .context("parse batched stress seed")?;
        let decode = |value: &str| {
            base64::engine::general_purpose::STANDARD
                .decode(value)
                .context("decode batched stress artifact")
        };
        mismatches.entry(candidate.program.id).or_insert((
            seed,
            decode(fields[3])?,
            decode(fields[4])?,
            decode(fields[5])?,
        ));
    }
    ensure!(completed, "batched stress result is incomplete");
    output.progress(
        "stress run",
        &format!(
            "Stress suite {}: {}/{} seed(s) checked",
            suite.name, suite.cases, suite.cases
        ),
    );
    Ok(Some(mismatches))
}

fn stress_program_shell(
    language: &str,
    label: &str,
    source_variable: &str,
    arguments: &[String],
) -> Result<(String, String)> {
    let arguments = arguments
        .iter()
        .map(|argument| shell_quote(argument))
        .collect::<Vec<_>>()
        .join(" ");
    let suffix = if arguments.is_empty() {
        String::new()
    } else {
        format!(" {arguments}")
    };
    match language {
        "python" => Ok((
            String::new(),
            format!("python3 \"{source_variable}\"{suffix}"),
        )),
        "pypy" => Ok((
            String::new(),
            format!("pypy3 \"{source_variable}\"{suffix}"),
        )),
        "javascript" => Ok((String::new(), format!("node \"{source_variable}\"{suffix}"))),
        "php" => Ok((String::new(), format!("php \"{source_variable}\"{suffix}"))),
        "r" => Ok((
            String::new(),
            format!("Rscript \"{source_variable}\"{suffix}"),
        )),
        "bash" => Ok((String::new(), format!("bash \"{source_variable}\"{suffix}"))),
        "c" => Ok((
            format!("cc -std=c17 -O2 -pipe \"{source_variable}\" -o /run/reporch/{label}\n"),
            format!("/run/reporch/{label}{suffix}"),
        )),
        "cpp" => Ok((
            format!("c++ -std=c++20 -O2 -pipe \"{source_variable}\" -o /run/reporch/{label}\n"),
            format!("/run/reporch/{label}{suffix}"),
        )),
        "rust" => Ok((
            format!("rustc --edition=2024 -O \"{source_variable}\" -o /run/reporch/{label}\n"),
            format!("/run/reporch/{label}{suffix}"),
        )),
        "swift" => Ok((
            format!("swiftc -O \"{source_variable}\" -o /run/reporch/{label}\n"),
            format!("/run/reporch/{label}{suffix}"),
        )),
        _ => anyhow::bail!("language is not supported by batched stress execution: {language}"),
    }
}

fn append_checked_stress_command(
    script: &mut String,
    command: &str,
    timeout_ms: u64,
    output_path: &str,
    input_path: Option<&str>,
    failure_exit: i32,
) -> Result<()> {
    script.push_str("  set +e\n");
    let input = input_path
        .map(|path| format!(" < \"{path}\""))
        .unwrap_or_default();
    writeln!(
        script,
        "  {}{} > \"{}\" 2>/run/reporch/stress.err",
        timed_stress_command(command, timeout_ms),
        input,
        output_path
    )?;
    script.push_str("  status=$?\n  set -e\n");
    writeln!(
        script,
        "  if [ \"$status\" -ne 0 ]; then cat /run/reporch/stress.err >&2; exit {failure_exit}; fi"
    )?;
    Ok(())
}

fn timed_stress_command(command: &str, timeout_ms: u64) -> String {
    format!(
        "timeout --signal=KILL --kill-after=1s {:.3}s {command}",
        timeout_ms as f64 / 1_000.0
    )
}

fn stress_compare_shell(checker: &CheckerSpec, index: usize) -> String {
    match checker {
        CheckerSpec::Exact => format!("cmp -s \"$oracle\" /run/reporch/stress-candidate-{index}"),
        CheckerSpec::Token => format!(
            "awk '{{for(i=1;i<=NF;i++) print $i}}' \"$oracle\" > /run/reporch/oracle.tokens && awk '{{for(i=1;i<=NF;i++) print $i}}' /run/reporch/stress-candidate-{index} > /run/reporch/candidate.tokens && cmp -s /run/reporch/oracle.tokens /run/reporch/candidate.tokens"
        ),
        CheckerSpec::CaseInsensitive => format!(
            "awk '{{for(i=1;i<=NF;i++) print tolower($i)}}' \"$oracle\" > /run/reporch/oracle.tokens && awk '{{for(i=1;i<=NF;i++) print tolower($i)}}' /run/reporch/stress-candidate-{index} > /run/reporch/candidate.tokens && cmp -s /run/reporch/oracle.tokens /run/reporch/candidate.tokens"
        ),
        CheckerSpec::Floating { .. } | CheckerSpec::Custom { .. } => "false".into(),
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

async fn run_solution(
    root: &Path,
    solution: &SolutionSpecV2,
    input_path: &str,
    options: &reporch_cli::authoring_runtime::AuthoringRunOptions,
) -> Result<reporch_cli::local_sandbox::LocalSandboxResult> {
    reporch_cli::authoring_runtime::run_program(&reporch_cli::authoring_runtime::ProgramRequest {
        project_directory: root,
        source_path: &solution.program.source_path,
        language: &solution.program.language,
        arguments: &solution.program.arguments,
        stdin_path: Some(input_path),
        options,
    })
    .await
}

#[allow(clippy::too_many_arguments)]
async fn stress_outputs_match(
    root: &Path,
    scratch: &Path,
    input_path: &str,
    seed: u64,
    candidate: &SolutionSpecV2,
    checker: &CheckerSpec,
    oracle_output: &[u8],
    candidate_output: &[u8],
    options: &reporch_cli::authoring_runtime::AuthoringRunOptions,
) -> Result<bool> {
    let CheckerSpec::Custom {
        source_path,
        language,
        protocol,
    } = checker
    else {
        return reporch_cli::authoring_runtime::standard_checker_matches(
            checker,
            oracle_output,
            candidate_output,
        );
    };
    let oracle_path = scratch.join(format!("{seed}.oracle"));
    let candidate_path = scratch.join(format!("{seed}.{}.out", candidate.program.id));
    fs::write(&oracle_path, oracle_output)?;
    fs::write(&candidate_path, candidate_output)?;
    let relative = |path: &Path| -> Result<String> {
        Ok(path
            .strip_prefix(root)?
            .to_str()
            .context("stress checker scratch path is not valid Unicode")?
            .to_owned())
    };
    let candidate_path = relative(&candidate_path)?;
    let oracle_path = relative(&oracle_path)?;
    let result = reporch_cli::authoring_runtime::run_custom_checker(
        root,
        source_path,
        language,
        *protocol,
        input_path,
        &oracle_path,
        &candidate_path,
        options,
    )
    .await?;
    Ok(result.verdict == reporch_cli::authoring_runtime::CustomCheckerVerdict::Accepted)
}

#[allow(clippy::too_many_arguments)]
async fn minimize_counterexample(
    root: &Path,
    scratch: &Path,
    seed: u64,
    candidate: &SolutionSpecV2,
    oracle: &SolutionSpecV2,
    checker: &CheckerSpec,
    input: Vec<u8>,
    oracle_output: Vec<u8>,
    candidate_output: Vec<u8>,
    options: &reporch_cli::authoring_runtime::AuthoringRunOptions,
) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    let mut lines = input
        .split_inclusive(|byte| *byte == b'\n')
        .map(<[u8]>::to_vec)
        .collect::<Vec<_>>();
    if lines.len() <= 1 {
        return Ok((input, oracle_output, candidate_output));
    }
    let mut best_outputs = (oracle_output, candidate_output);
    let mut attempts = 0_u32;
    let mut index = lines.len();
    while index > 0 && attempts < 64 && lines.len() > 1 {
        index -= 1;
        let candidate_input = lines
            .iter()
            .enumerate()
            .filter(|(line_index, _)| *line_index != index)
            .flat_map(|(_, line)| line.iter().copied())
            .collect::<Vec<_>>();
        attempts += 1;
        if let Some(outputs) = evaluate_counterexample(
            root,
            scratch,
            seed,
            candidate,
            oracle,
            checker,
            &candidate_input,
            options,
        )
        .await?
        {
            lines.remove(index);
            best_outputs = outputs;
            index = lines.len();
        }
    }
    Ok((
        lines.into_iter().flatten().collect(),
        best_outputs.0,
        best_outputs.1,
    ))
}

#[allow(clippy::too_many_arguments)]
async fn evaluate_counterexample(
    root: &Path,
    scratch: &Path,
    seed: u64,
    candidate: &SolutionSpecV2,
    oracle: &SolutionSpecV2,
    checker: &CheckerSpec,
    input: &[u8],
    options: &reporch_cli::authoring_runtime::AuthoringRunOptions,
) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
    let input_path = scratch.join(format!("{seed}.min.in"));
    fs::write(&input_path, input)?;
    let input_relative = input_path
        .strip_prefix(root)?
        .to_str()
        .context("minimized stress path is not valid Unicode")?
        .to_owned();
    let oracle_result = run_solution(root, oracle, &input_relative, options).await?;
    if oracle_result.exit_code != 0 {
        return Ok(None);
    }
    let candidate_result = run_solution(root, candidate, &input_relative, options).await?;
    let matches = candidate_result.exit_code == 0
        && stress_outputs_match(
            root,
            scratch,
            &input_relative,
            seed,
            candidate,
            checker,
            &oracle_result.stdout_bytes,
            &candidate_result.stdout_bytes,
            options,
        )
        .await?;
    Ok((!matches).then_some((oracle_result.stdout_bytes, candidate_result.stdout_bytes)))
}

fn answer_path_for(input: &str) -> Result<String> {
    let path = Path::new(input);
    let answer = if path.extension().is_some_and(|extension| extension == "in") {
        path.with_extension("ans")
    } else {
        PathBuf::from(format!("{input}.ans"))
    };
    relative_string(&answer)
}

fn write_project_bytes_once_or_same(root: &Path, path: &str, bytes: &[u8]) -> Result<()> {
    match read_project_bytes(root, path) {
        Ok(existing) => ensure!(
            existing == bytes,
            "existing stress artifact differs: {path}"
        ),
        Err(_) => write_project_bytes_atomic(root, path, bytes)?,
    }
    Ok(())
}

pub(super) async fn interactor(options: InteractorOptions, output: &CliOutput) -> Result<()> {
    match options.command {
        InteractorCommand::Set { source, language } => {
            let source = relative_string(&source)?;
            let spec = reporch_cli::local_project_v2::update_authoring_spec(
                Path::new("."),
                |root, spec| {
                    ensure!(
                        spec.problem_type == ProblemType::Interactive,
                        "interactor can only be configured for an interactive problem"
                    );
                    reporch_cli::local_project_v2::declare_project_file(
                        root,
                        spec,
                        &source,
                        source_media_type(&language),
                        false,
                    )?;
                    spec.execution.interactive = Some(InteractiveSpecV2 {
                        interactor: ProgramSpecV2 {
                            id: Uuid::now_v7(),
                            name: "interactor".into(),
                            source_path: source.clone(),
                            language: language.clone(),
                            arguments: Vec::new(),
                        },
                        idle_timeout_ms: 2_000,
                        transcript_limit_kib: 1_024,
                        unit_tests: Vec::new(),
                    });
                    Ok(())
                },
            )?;
            output.emit(
                "interactor set",
                &spec.execution.interactive,
                "Configured interactor",
            )
        }
        InteractorCommand::Run(options) => run_interactor(options, false, output).await,
        InteractorCommand::Transcript(options) => run_interactor(options, true, output).await,
    }
}

async fn run_interactor(
    options: RuntimeProgramRunOptions,
    transcript: bool,
    output: &CliOutput,
) -> Result<()> {
    let root = reporch_cli::local_project::discover_project(Path::new("."))?;
    let spec = reporch_cli::local_project_v2::read_authoring_spec(&root)?;
    let interactive = spec
        .execution
        .interactive
        .as_ref()
        .context("no interactor is configured")?;
    let solution = find_runtime_solution(&spec, &options.solution)?;
    let test = find_test(&spec, &options.test)?;
    let run_options = options.runtime.into_run_options(output);
    let interactor_toolchain = reporch_cli::toolchain::resolve_for_language(
        run_options.toolchain_id.as_deref(),
        &interactive.interactor.language,
    )?;
    let solution_toolchain = reporch_cli::toolchain::resolve_for_language(
        run_options.toolchain_id.as_deref(),
        &solution.program.language,
    )?;
    ensure!(
        interactor_toolchain.language == solution_toolchain.language,
        "local interactive pairing requires matching toolchain languages; use Studio verification for cross-language pairing"
    );
    let result = reporch_cli::authoring_runtime::run_interactive_pair(
        &reporch_cli::authoring_runtime::InteractivePairRequest {
            project_directory: &root,
            solver_source_path: &solution.program.source_path,
            interactor_source_path: &interactive.interactor.source_path,
            language: &interactive.interactor.language,
            input_path: &test.input_file,
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
        solution: solution.program.name.clone(),
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

pub(super) async fn grader(options: GraderOptions, output: &CliOutput) -> Result<()> {
    match options.command {
        GraderCommand::Set {
            source,
            language,
            submission_template,
            compile_script,
            compile_command,
            run_script,
            run_command,
            asset,
            interface_file,
            public_file,
        } => {
            let source = relative_string(&source)?;
            let submission_template = submission_template
                .as_deref()
                .map(relative_string)
                .transpose()?;
            let compile_script = compile_script.as_deref().map(relative_string).transpose()?;
            let run_script = run_script.as_deref().map(relative_string).transpose()?;
            let assets = asset
                .iter()
                .map(|path| relative_string(path))
                .collect::<Result<Vec<_>>>()?;
            let interface_files = interface_file
                .iter()
                .map(|path| relative_string(path))
                .collect::<Result<Vec<_>>>()?;
            let public_files = public_file
                .iter()
                .map(|path| relative_string(path))
                .collect::<Result<Vec<_>>>()?;
            let normalize_command =
                |value: Option<String>, label: &str| -> Result<Option<String>> {
                    let Some(value) = value else {
                        return Ok(None);
                    };
                    let value = value.trim();
                    ensure!(
                        !value.is_empty() && value.len() <= 4_096,
                        "{label} must be between 1 and 4096 characters"
                    );
                    Ok(Some(value.to_owned()))
                };
            let compile_command = normalize_command(compile_command, "compile command")?;
            let run_command = normalize_command(run_command, "run command")?;
            let spec = reporch_cli::local_project_v2::update_authoring_spec(
                Path::new("."),
                |root, spec| {
                    ensure!(
                        matches!(
                            spec.problem_type,
                            ProblemType::Library | ProblemType::Grader
                        ),
                        "grader can only be configured for a library or grader problem"
                    );
                    ensure!(
                        !language.trim().is_empty() && language.len() <= 50,
                        "grader language must be between 1 and 50 characters"
                    );
                    reporch_cli::local_project_v2::declare_project_file(
                        root,
                        spec,
                        &source,
                        source_media_type(&language),
                        false,
                    )?;
                    if let Some(path) = submission_template.as_deref() {
                        reporch_cli::local_project_v2::declare_project_file(
                            root,
                            spec,
                            path,
                            source_media_type(&language),
                            false,
                        )?;
                    }
                    for path in assets
                        .iter()
                        .chain(interface_files.iter())
                        .chain(public_files.iter())
                    {
                        reporch_cli::local_project_v2::declare_project_file(
                            root,
                            spec,
                            path,
                            source_media_type(&language),
                            false,
                        )?;
                    }
                    for path in compile_script.iter().chain(run_script.iter()) {
                        reporch_cli::local_project_v2::declare_project_file(
                            root,
                            spec,
                            path,
                            "text/x-shellscript",
                            true,
                        )?;
                    }
                    let kind = if spec.problem_type == ProblemType::Library {
                        HarnessKindV2::Library
                    } else {
                        HarnessKindV2::Grader
                    };
                    let harness = spec.execution.harness.get_or_insert_with(|| HarnessSpecV2 {
                        kind,
                        interface_files: Vec::new(),
                        public_files: Vec::new(),
                        private_files: Vec::new(),
                        stub_templates: Default::default(),
                        profiles: Default::default(),
                    });
                    harness.kind = kind;
                    let existing = harness.profiles.remove(&language);
                    if let Some(old_source) = existing
                        .as_ref()
                        .map(|profile| profile.source_path.as_str())
                        .filter(|old_source| *old_source != source)
                        && !harness
                            .profiles
                            .values()
                            .any(|profile| profile.source_path == old_source)
                    {
                        harness.private_files.retain(|path| path != old_source);
                    }
                    if !harness.private_files.contains(&source) {
                        harness.private_files.push(source.clone());
                    }
                    for path in &interface_files {
                        if !harness.interface_files.contains(path) {
                            harness.interface_files.push(path.clone());
                        }
                    }
                    for path in &public_files {
                        if !harness.public_files.contains(path) {
                            harness.public_files.push(path.clone());
                        }
                    }
                    let submission_source_path = submission_template
                        .clone()
                        .or_else(|| {
                            existing
                                .as_ref()
                                .and_then(|profile| profile.submission_source_path.clone())
                        })
                        .context(
                            "grader set requires --submission-template when the profile has no safe contestant template",
                        )?;
                    ensure!(
                        submission_source_path != source
                            && !harness.private_files.contains(&submission_source_path),
                        "contestant submission template must be distinct from every private grader source"
                    );
                    let selected_compile_script = if compile_command.is_some() {
                        None
                    } else {
                        compile_script.clone().or_else(|| {
                            existing
                                .as_ref()
                                .and_then(|profile| profile.compile_script.clone())
                        })
                    };
                    let selected_run_script = if run_command.is_some() {
                        None
                    } else {
                        run_script.clone().or_else(|| {
                            existing
                                .as_ref()
                                .and_then(|profile| profile.run_script.clone())
                        })
                    };
                    let selected_compile_command = if compile_script.is_some() {
                        None
                    } else {
                        compile_command.clone().or_else(|| {
                            existing
                                .as_ref()
                                .and_then(|profile| profile.compile_command.clone())
                        })
                    };
                    let selected_run_command = if run_script.is_some() {
                        None
                    } else {
                        run_command.clone().or_else(|| {
                            existing
                                .as_ref()
                                .and_then(|profile| profile.run_command.clone())
                        })
                    };
                    ensure!(
                        selected_compile_script.is_some() || selected_compile_command.is_some(),
                        "grader profile requires --compile-script or --compile-command"
                    );
                    ensure!(
                        selected_run_script.is_some() || selected_run_command.is_some(),
                        "grader profile requires --run-script or --run-command"
                    );
                    let mut asset_paths = existing
                        .as_ref()
                        .map(|profile| profile.asset_paths.clone())
                        .unwrap_or_default();
                    if let Some(old_source) = existing
                        .as_ref()
                        .map(|profile| profile.source_path.as_str())
                        .filter(|old_source| *old_source != source)
                    {
                        asset_paths.retain(|path| path != old_source);
                    }
                    asset_paths.extend(assets.iter().cloned());
                    asset_paths.extend(interface_files.iter().cloned());
                    asset_paths.extend(public_files.iter().cloned());
                    asset_paths.extend([source.clone(), submission_source_path.clone()]);
                    asset_paths.extend(selected_compile_script.iter().cloned());
                    asset_paths.extend(selected_run_script.iter().cloned());
                    if let Some(path) = harness.stub_templates.get(&language) {
                        asset_paths.push(path.clone());
                    }
                    asset_paths.sort();
                    asset_paths.dedup();
                    ensure!(
                        asset_paths.len() <= 500,
                        "grader profile cannot contain more than 500 assets"
                    );
                    harness.profiles.insert(
                        language.clone(),
                        HarnessProfileSpecV2 {
                            language: language.clone(),
                            source_path: source.clone(),
                            submission_source_path: Some(submission_source_path),
                            asset_paths,
                            include_dirs: existing
                                .as_ref()
                                .map(|profile| profile.include_dirs.clone())
                                .unwrap_or_default(),
                            compile_script: selected_compile_script,
                            run_script: selected_run_script,
                            compile_command: selected_compile_command,
                            run_command: selected_run_command,
                        },
                    );
                    Ok(())
                },
            )?;
            output.emit("grader set", &spec.execution.harness, "Configured grader")
        }
        GraderCommand::Run(options) => run_grader(options, output).await,
    }
}

async fn run_grader(options: RuntimeProgramRunOptions, output: &CliOutput) -> Result<()> {
    let root = reporch_cli::local_project::discover_project(Path::new("."))?;
    let spec = reporch_cli::local_project_v2::read_authoring_spec(&root)?;
    let harness = spec
        .execution
        .harness
        .as_ref()
        .context("no grader is configured")?;
    let solution = find_runtime_solution(&spec, &options.solution)?;
    let profile = harness
        .profiles
        .get(&solution.program.language)
        .or_else(|| harness.profiles.values().next())
        .context("configured grader has no language profile")?;
    ensure!(
        reporch_cli::toolchain::resolve_for_language(None, &profile.language)?.language
            == reporch_cli::toolchain::resolve_for_language(None, &solution.program.language)?
                .language,
        "local grader linking requires the solution and grader to use the same C or C++ toolchain"
    );
    let test = find_test(&spec, &options.test)?;
    let answer_path = test
        .answer_file
        .as_deref()
        .context("grader test has no answer file")?;
    let run_options = options.runtime.into_run_options(output);
    let result = reporch_cli::authoring_runtime::run_linked_pair(
        &reporch_cli::authoring_runtime::LinkedPairRequest {
            project_directory: &root,
            first_source_path: &solution.program.source_path,
            second_source_path: &profile.source_path,
            language: &profile.language,
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
            &spec.testing.checker.checker,
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
        solution: solution.program.name.clone(),
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

pub(super) async fn output_submission(options: OutputOptions, output: &CliOutput) -> Result<()> {
    match options.command {
        OutputCommand::List => {
            let root = reporch_cli::local_project::discover_project(Path::new("."))?;
            let spec = reporch_cli::local_project_v2::read_authoring_spec(&root)?;
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
            let spec = reporch_cli::local_project_v2::update_authoring_spec(
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
                            spec.testing.tests.iter().any(|test| test.id == *test_id),
                            "unknown test case: {test_id}. List test UUIDs with `reporch test case list --format json`"
                        );
                        reporch_cli::local_project_v2::declare_project_file(
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
                    spec.output_submissions.push(OutputSubmissionSpecV2 {
                        id: Uuid::now_v7(),
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
            let spec = reporch_cli::local_project_v2::update_authoring_spec(
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
                    pruned = prune_output_file_declarations(spec, &removed_paths);
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
            let spec = reporch_cli::local_project_v2::read_authoring_spec(&root)?;
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
                for test in &spec.testing.tests {
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
                        &spec.testing.checker.checker,
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
                let score = output_score(&spec.testing.groups, &spec.testing.tests, &cases)?;
                let actual_verdict = if cases.iter().all(|case| case.accepted) {
                    studio_core::ExpectedVerdict::Accepted
                } else if score > 0.0 {
                    studio_core::ExpectedVerdict::Partial
                } else {
                    studio_core::ExpectedVerdict::WrongAnswer
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

fn output_score(
    groups: &[TestGroupSpecV2],
    tests: &[TestCaseSpecV2],
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

fn inferred_role(name: &str) -> TestCaseRoleV2 {
    if name.to_ascii_lowercase().starts_with("sample") {
        TestCaseRoleV2::Sample
    } else {
        TestCaseRoleV2::Secret
    }
}

fn ensure_unique_test_name(
    spec: &reporch_format::AuthoringSpecV2,
    name: &str,
    except: Option<Uuid>,
) -> Result<()> {
    let normalized = normalize_name(name)?;
    ensure!(
        !spec
            .testing
            .tests
            .iter()
            .any(|test| test.id != except.unwrap_or_else(Uuid::nil) && test.name == normalized),
        "test case name already exists: {normalized}"
    );
    Ok(())
}

fn resolve_group_ids(
    spec: &reporch_format::AuthoringSpecV2,
    names: &[String],
) -> Result<Vec<Uuid>> {
    names
        .iter()
        .map(|name| Ok(find_group(spec, name)?.id))
        .collect()
}

fn ensure_v2_group_dependencies_acyclic(groups: &[TestGroupSpecV2]) -> Result<()> {
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
                resolved.insert(group.id);
            }
        }
        ensure!(
            resolved.len() > before,
            "test group dependency graph cannot contain a cycle"
        );
    }
    Ok(())
}

fn find_group<'a>(
    spec: &'a reporch_format::AuthoringSpecV2,
    value: &str,
) -> Result<&'a TestGroupSpecV2> {
    let parsed = Uuid::parse_str(value).ok();
    spec.testing
        .groups
        .iter()
        .find(|group| group.name == value || parsed == Some(group.id))
        .with_context(|| {
            format!(
                "unknown group: {value}. Create it with `reporch test group add {value} --points 0`, list groups with `reporch test group list`, or omit --group for an ungrouped sample test"
            )
        })
}

fn group_points_feedback_v2(
    problem_type: studio_core::ProblemType,
    groups: &[TestGroupSpecV2],
    action: &str,
    group: &str,
) -> String {
    if problem_type != studio_core::ProblemType::Scored {
        return action.to_owned();
    }
    super::scored_points_feedback(action, group, groups.iter().map(|group| group.points).sum())
}

fn find_generator<'a>(
    spec: &'a reporch_format::AuthoringSpecV2,
    value: &str,
) -> Result<&'a GeneratorSpecV2> {
    let parsed = Uuid::parse_str(value).ok();
    spec.testing
        .generators
        .iter()
        .find(|generator| generator.program.name == value || parsed == Some(generator.program.id))
        .with_context(|| format!("generator was not found: {value}"))
}

fn legacy_program(program: &ProgramSpecV2) -> ProgramSpec {
    ProgramSpec {
        id: program.name.clone(),
        source_path: program.source_path.clone(),
        language: program.language.clone(),
        arguments: program.arguments.clone(),
    }
}

fn find_solution<'a>(
    spec: &'a reporch_format::AuthoringSpecV2,
    value: &str,
) -> Result<&'a SolutionSpecV2> {
    find_solution_with_mode(spec, value, false)
}

fn find_runtime_solution<'a>(
    spec: &'a reporch_format::AuthoringSpecV2,
    value: &str,
) -> Result<&'a SolutionSpecV2> {
    find_solution_with_mode(spec, value, true)
}

fn find_solution_with_mode<'a>(
    spec: &'a reporch_format::AuthoringSpecV2,
    value: &str,
    include_source_path: bool,
) -> Result<&'a SolutionSpecV2> {
    let parsed = Uuid::parse_str(value).ok();
    let matches = spec
        .testing
        .solutions
        .iter()
        .filter(|solution| {
            solution.program.name == value
                || (include_source_path && solution.program.source_path == value)
                || parsed == Some(solution.program.id)
        })
        .collect::<Vec<_>>();
    ensure!(
        matches.len() <= 1,
        "ambiguous solution selector {value:?}; use the exact UUID from `reporch solution list`"
    );
    matches.into_iter().next().with_context(|| {
        if include_source_path {
            format!(
                "solution was not found: {value}; use a solution name, UUID, or source path from `reporch solution list`"
            )
        } else {
            format!(
                "solution was not found: {value}; use a solution name or UUID from `reporch solution list`"
            )
        }
    })
}

fn find_test<'a>(
    spec: &'a reporch_format::AuthoringSpecV2,
    value: &str,
) -> Result<&'a TestCaseSpecV2> {
    let parsed = Uuid::parse_str(value).ok();
    let matches = spec
        .testing
        .tests
        .iter()
        .filter(|test| parsed == Some(test.id) || test.name == value || test.input_file == value)
        .collect::<Vec<_>>();
    ensure!(
        matches.len() <= 1,
        "ambiguous test selector {value:?}; use the exact UUID from `reporch test case list`"
    );
    matches.into_iter().next().with_context(|| {
            format!(
                "test case was not found: {value}; use a test name, UUID, or input path from `reporch test case list`"
            )
        })
}

fn prune_output_file_declarations(
    spec: &mut reporch_format::AuthoringSpecV2,
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
