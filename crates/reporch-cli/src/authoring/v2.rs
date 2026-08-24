use std::fs;
use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use reporch_format::{VersionedAuthoringSpec, parse_versioned_authoring_spec};
use studio_core::{
    GeneratedCaseRefV2, GeneratorMatrixStrategyV2, GeneratorRecipeSpecV2, ScoreAggregationV2,
    TestCaseOriginV2, TestCaseRoleV2, TestCaseSpecV2, TestGroupSpecV2,
};
use uuid::Uuid;

use super::*;

pub(super) fn is_active_project() -> Result<bool> {
    let root = reporch_cli::local_project::discover_project(Path::new("."))?;
    let path = root.join(reporch_cli::local_project::AUTHORING_FILE_NAME);
    let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    Ok(matches!(
        parse_versioned_authoring_spec(&bytes)?,
        VersionedAuthoringSpec::V2(_)
    ))
}

pub(super) fn statement(options: StatementOptions, output: &CliOutput) -> Result<()> {
    match options.command {
        StatementCommand::Add {
            locale,
            path,
            title,
        } => {
            let relative = relative_string(&path)?;
            let spec = reporch_cli::local_project_v2::update_authoring_spec(
                Path::new("."),
                |root, spec| {
                    reporch_cli::local_project_v2::declare_project_file(
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
                },
            )?;
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
            open::that(root.join(path)).context("open statement in the default application")?;
            output.emit(
                "statement open",
                &serde_json::json!({ "locale": locale, "path": path }),
                &format!("Opened {path}"),
            )
        }
        StatementCommand::Check => {
            let root = reporch_cli::local_project::discover_project(Path::new("."))?;
            let spec = reporch_cli::local_project_v2::read_authoring_spec(&root)?;
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
            let spec = reporch_cli::local_project_v2::read_authoring_spec(&root)?;
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
            let spec = reporch_cli::local_project_v2::read_authoring_spec(&root)?;
            output.emit(
                "test case list",
                &spec.testing.tests,
                &format!("{} test case(s)", spec.testing.tests.len()),
            )
        }
        TestCaseCommand::Add(options) => {
            let input = relative_string(&options.input)?;
            let answer = options.answer.as_deref().map(relative_string).transpose()?;
            let test_id = Uuid::now_v7();
            let spec = reporch_cli::local_project_v2::update_authoring_spec(
                Path::new("."),
                |root, spec| {
                    ensure_unique_test_name(spec, &options.name, None)?;
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
                },
            )?;
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
                    let group_ids = if options.groups.is_empty() {
                        None
                    } else {
                        Some(resolve_group_ids(spec, &options.groups)?)
                    };
                    if let Some(name) = &options.name {
                        ensure_unique_test_name(spec, name, Some(options.id))?;
                    }
                    let test = spec
                        .testing
                        .tests
                        .iter_mut()
                        .find(|test| test.id == options.id)
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
                &format!("Updated test case {}", options.id),
            )
        }
        TestCaseCommand::Remove { id } => {
            let spec = reporch_cli::local_project_v2::update_authoring_spec(
                Path::new("."),
                |_root, spec| {
                    let before = spec.testing.tests.len();
                    spec.testing.tests.retain(|test| test.id != id);
                    ensure!(
                        before != spec.testing.tests.len(),
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
                &spec.testing.tests,
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
                    Ok(())
                },
            )?;
            output.emit(
                "test group add",
                &spec.testing.groups,
                &format!("Added group {}", options.id),
            )
        }
        TestGroupCommand::Update(options) => {
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
                    Ok(())
                },
            )?;
            output.emit(
                "test group update",
                &spec.testing.groups,
                &format!("Updated group {}", options.id),
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

fn find_group<'a>(
    spec: &'a reporch_format::AuthoringSpecV2,
    value: &str,
) -> Result<&'a TestGroupSpecV2> {
    let parsed = Uuid::parse_str(value).ok();
    spec.testing
        .groups
        .iter()
        .find(|group| group.name == value || parsed == Some(group.id))
        .with_context(|| format!("unknown group: {value}"))
}
