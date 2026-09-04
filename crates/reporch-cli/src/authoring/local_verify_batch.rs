use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use base64::Engine as _;

use super::*;

pub(super) struct BatchVerifyResult {
    pub(super) generator_checks: Vec<LocalGeneratorCheck>,
    pub(super) validator_units: usize,
    pub(super) checker_units: usize,
    pub(super) solutions: Vec<LocalSolutionCheck>,
}

struct GeneratorTarget<'a> {
    generator: &'a studio_core::GeneratorSpecV2,
    recipe: &'a studio_core::GeneratorRecipeSpecV2,
    seed: u64,
    expected_path: Option<&'a str>,
}

pub(super) async fn try_verify(
    root: &Path,
    spec: &reporch_format::AuthoringSpecV2,
    options: &reporch_cli::authoring_runtime::AuthoringRunOptions,
    output: &CliOutput,
) -> Result<Option<BatchVerifyResult>> {
    if options.runtime != reporch_cli::local_sandbox::OciRuntime::Auto
        && std::env::var_os("REPORCH_DEBUG_ENABLE_LOCAL_VERIFY_BATCH").is_none()
    {
        return Ok(None);
    }
    if !matches!(
        spec.problem_type,
        studio_core::ProblemType::Standard | studio_core::ProblemType::Scored
    ) || matches!(
        spec.testing.checker.checker,
        CheckerSpec::Custom { .. } | CheckerSpec::Floating { .. }
    ) {
        return Ok(None);
    }
    let mut generator_targets = Vec::new();
    let mut covered_recipes = std::collections::BTreeSet::new();
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
        generator_targets.push(GeneratorTarget {
            generator,
            recipe,
            seed: generated.seed,
            expected_path: Some(&test.input_file),
        });
        covered_recipes.insert((generator.program.id, recipe.id));
    }
    for generator in &spec.testing.generators {
        for recipe in &generator.recipes {
            if !covered_recipes.contains(&(generator.program.id, recipe.id)) {
                generator_targets.push(GeneratorTarget {
                    generator,
                    recipe,
                    seed: recipe.seed_start,
                    expected_path: None,
                });
            }
        }
    }
    let validators = spec
        .testing
        .validators
        .primary
        .iter()
        .chain(spec.testing.validators.extra.iter())
        .collect::<Vec<_>>();
    let programs = generator_targets
        .iter()
        .map(|target| &target.generator.program)
        .chain(validators.iter().copied())
        .chain(
            spec.testing
                .solutions
                .iter()
                .map(|solution| &solution.program),
        )
        .collect::<Vec<_>>();
    let Some(first) = programs.first() else {
        return Ok(None);
    };
    let language = reporch_cli::toolchain::resolve_for_language(
        options.toolchain_id.as_deref(),
        &first.language,
    )?
    .language;
    if matches!(language.as_str(), "java" | "csharp") {
        return Ok(None);
    }
    for program in &programs {
        let program_language = reporch_cli::toolchain::resolve_for_language(
            options.toolchain_id.as_deref(),
            &program.language,
        )?
        .language;
        if program_language != language {
            return Ok(None);
        }
    }
    let jobs = generator_targets.len().saturating_mul(2).saturating_add(
        validators
            .len()
            .saturating_mul(spec.testing.validators.unit_tests.len())
            .saturating_add(
                spec.testing
                    .solutions
                    .len()
                    .saturating_mul(spec.testing.tests.len()),
            ),
    );
    let total_timeout_ms = options
        .timeout
        .as_millis()
        .saturating_mul(jobs as u128)
        .saturating_add(30_000);
    if jobs == 0 || total_timeout_ms > 600_000 || programs.len() + spec.files.len() + 2 > 256 {
        return Ok(None);
    }

    verify_standard_checker_units(root, spec)?;

    let scratch_parent = root.join(".reporch").join("local-verify-tmp");
    std::fs::create_dir_all(&scratch_parent)?;
    let mut script_file = tempfile::Builder::new()
        .prefix("batch-")
        .suffix(".sh")
        .tempfile_in(&scratch_parent)?;
    let mut script = String::from("#!/bin/sh\nset -eu\nb64() { base64 < \"$1\" | tr -d '\\n'; }\n");
    let mut generator_commands = std::collections::BTreeMap::new();
    for target in &generator_targets {
        if generator_commands.contains_key(&target.generator.program.id) {
            continue;
        }
        let label = format!("generator-{}", generator_commands.len());
        let (setup, command) = program_shell(&language, &label, &target.generator.program)?;
        script.push_str(&setup);
        generator_commands.insert(target.generator.program.id, command);
    }
    let mut validator_commands = Vec::new();
    for (index, validator) in validators.iter().enumerate() {
        let (setup, command) = program_shell(&language, &format!("validator-{index}"), validator)?;
        script.push_str(&setup);
        validator_commands.push(command);
    }
    let mut solution_commands = Vec::new();
    for (index, solution) in spec.testing.solutions.iter().enumerate() {
        let (setup, command) =
            program_shell(&language, &format!("solution-{index}"), &solution.program)?;
        script.push_str(&setup);
        solution_commands.push(command);
    }
    for (target_index, target) in generator_targets.iter().enumerate() {
        let base_command = generator_commands
            .get(&target.generator.program.id)
            .context("generator batch command is missing")?;
        let recipe_arguments = target
            .recipe
            .argument_template
            .iter()
            .map(|argument| shell_quote(argument))
            .collect::<Vec<_>>()
            .join(" ");
        let command = format!(
            "{base_command}{} {}",
            if recipe_arguments.is_empty() {
                String::new()
            } else {
                format!(" {recipe_arguments}")
            },
            target.seed
        );
        writeln!(
            &mut script,
            "out=/run/reporch/generator-{target_index}.out; repeat=/run/reporch/generator-{target_index}.repeat; err=/run/reporch/generator-{target_index}.err"
        )?;
        append_execution(&mut script, &command, None, "$out", options.timeout)?;
        script.push_str("first_status=$status\n");
        append_execution(&mut script, &command, None, "$repeat", options.timeout)?;
        script.push_str("second_status=$status\nactual=matched; termination=exited\n");
        script.push_str("if [ \"$first_status\" -eq 124 ] || [ \"$second_status\" -eq 124 ]; then actual=timed_out; termination=timed_out\n");
        script.push_str("elif [ \"$first_status\" -ne 0 ] || [ \"$second_status\" -ne 0 ]; then actual=runtime_error\n");
        script.push_str("elif ! cmp -s \"$out\" \"$repeat\"; then actual=nondeterministic\n");
        if let Some(expected) = target.expected_path {
            let expected = guest_path(root, expected)?;
            writeln!(
                &mut script,
                "elif ! cmp -s {} \"$out\"; then actual=mismatch",
                shell_quote(&expected)
            )?;
        }
        script.push_str("fi\n");
        writeln!(
            &mut script,
            "printf 'G\\t{target_index}\\t0\\t%s\\t%s\\t%s\\t' \"$actual\" \"$first_status\" \"$termination\""
        )?;
        script.push_str("b64 \"$out\"; printf '\\t'; b64 \"$err\"; printf '\\n'\n");
    }
    for (validator_index, command) in validator_commands.iter().enumerate() {
        for (unit_index, unit) in spec.testing.validators.unit_tests.iter().enumerate() {
            let input = guest_path(root, &unit.input_file)?;
            writeln!(
                &mut script,
                "out=/run/reporch/validator-{validator_index}-{unit_index}.out; err=/run/reporch/validator-{validator_index}-{unit_index}.err"
            )?;
            append_execution(&mut script, command, Some(&input), "$out", options.timeout)?;
            script.push_str("actual=invalid; termination=exited\n");
            script.push_str("if [ \"$status\" -eq 0 ]; then actual=valid; fi\n");
            script.push_str(
                "if [ \"$status\" -eq 124 ]; then actual=timed_out; termination=timed_out; fi\n",
            );
            script.push_str("if [ \"$status\" -ge 128 ]; then actual=runtime_error; termination=signalled; fi\n");
            writeln!(
                &mut script,
                "printf 'V\\t{validator_index}\\t{unit_index}\\t%s\\t%s\\t%s\\t' \"$actual\" \"$status\" \"$termination\""
            )?;
            script.push_str("b64 \"$out\"; printf '\\t'; b64 \"$err\"; printf '\\n'\n");
        }
    }
    for (solution_index, command) in solution_commands.iter().enumerate() {
        for (test_index, test) in spec.testing.tests.iter().enumerate() {
            let input = guest_path(root, &test.input_file)?;
            let answer = test.answer_file.as_deref().with_context(|| {
                format!("test {} has no answer for solution verification", test.name)
            })?;
            let answer = guest_path(root, answer)?;
            writeln!(
                &mut script,
                "out=/run/reporch/solution-{solution_index}-{test_index}.out; err=/run/reporch/solution-{solution_index}-{test_index}.err"
            )?;
            append_execution(&mut script, command, Some(&input), "$out", options.timeout)?;
            script.push_str("actual=runtime_error; termination=exited\n");
            script.push_str(
                "if [ \"$status\" -eq 124 ]; then actual=time_limit; termination=timed_out\n",
            );
            script.push_str(
                "elif [ \"$status\" -ge 128 ]; then actual=runtime_error; termination=signalled\n",
            );
            writeln!(
                &mut script,
                "elif [ \"$status\" -eq 0 ]; then if {}; then actual=accepted; else actual=wrong_answer; fi\nfi",
                checker_shell(&spec.testing.checker.checker, &answer, "$out")
            )?;
            writeln!(
                &mut script,
                "printf 'S\\t{solution_index}\\t{test_index}\\t%s\\t%s\\t%s\\t' \"$actual\" \"$status\" \"$termination\""
            )?;
            script.push_str("b64 \"$out\"; printf '\\t'; b64 \"$err\"; printf '\\n'\n");
        }
    }
    script.push_str("printf 'D\\n'\n");
    script_file.write_all(script.as_bytes())?;
    script_file.as_file().sync_all()?;
    let script_relative = project_relative(root, script_file.path())?;
    let mut inputs = std::collections::BTreeSet::new();
    inputs.insert(format!("/workspace/{script_relative}"));
    for program in programs {
        inputs.insert(guest_path(root, &program.source_path)?);
    }
    for unit in &spec.testing.validators.unit_tests {
        inputs.insert(guest_path(root, &unit.input_file)?);
    }
    for test in &spec.testing.tests {
        inputs.insert(guest_path(root, &test.input_file)?);
        if let Some(answer) = &test.answer_file {
            inputs.insert(guest_path(root, answer)?);
        }
    }
    let mut command = vec!["bash".to_owned(), format!("/workspace/{script_relative}")];
    command.extend(inputs);
    let mut batch_options = options.clone();
    batch_options.timeout = std::time::Duration::from_millis(total_timeout_ms as u64);
    output.progress(
        "verify",
        &format!("Running {jobs} generator/validator/solution check(s) in one VM"),
    );
    let execution = reporch_cli::authoring_runtime::run_toolchain_command(
        root,
        &language,
        command,
        &batch_options,
    )
    .await?;
    if execution.termination != reporch_runtime_core::GuestTerminationV2::Exited
        || execution.exit_code != 0
    {
        return Err(crate::cli_output::domain_error(
            if execution.termination == reporch_runtime_core::GuestTerminationV2::TimedOut {
                "runtime.execution_timed_out"
            } else {
                "runtime.execution_failed"
            },
            "batched local verification did not exit normally",
            &execution,
        ));
    }

    let mut generator_checks = Vec::new();
    let mut validator_results = Vec::new();
    let mut solution_cases = (0..spec.testing.solutions.len())
        .map(|_| Vec::new())
        .collect::<Vec<_>>();
    let mut completed = false;
    for line in execution.stdout.lines() {
        if line == "D" {
            completed = true;
            continue;
        }
        let fields = line.split('\t').collect::<Vec<_>>();
        ensure!(fields.len() == 8, "invalid local verification result frame");
        let owner = fields[1].parse::<usize>().context("parse result owner")?;
        let case_index = fields[2].parse::<usize>().context("parse result case")?;
        let exit_code = fields[4].parse::<i32>().context("parse result exit code")?;
        let termination = parse_termination(fields[5])?;
        let stdout_bytes = decode_bytes(fields[6])?;
        let stdout = String::from_utf8_lossy(&stdout_bytes).into_owned();
        let stderr = decode_text(fields[7])?;
        match fields[0] {
            "G" => {
                let target = generator_targets
                    .get(owner)
                    .context("generator result index is out of range")?;
                let expected_sha256 = target
                    .expected_path
                    .map(|path| read_project_bytes(root, path))
                    .transpose()?
                    .map(|bytes| hex::encode(Sha256::digest(bytes)));
                generator_checks.push(LocalGeneratorCheck {
                    generator: target.generator.program.name.clone(),
                    recipe: target.recipe.name.clone(),
                    seed: target.seed,
                    expected_sha256,
                    actual_sha256: hex::encode(Sha256::digest(&stdout_bytes)),
                    passed: fields[3] == "matched"
                        && termination == reporch_runtime_core::GuestTerminationV2::Exited,
                });
            }
            "V" => {
                let validator = validators
                    .get(owner)
                    .context("validator result index is out of range")?;
                let unit = spec
                    .testing
                    .validators
                    .unit_tests
                    .get(case_index)
                    .context("validator unit result index is out of range")?;
                let expected = if unit.expected_valid {
                    "valid"
                } else {
                    "invalid"
                };
                validator_results.push(ProgramUnitResult {
                    program: validator.name.clone(),
                    name: unit.name.clone(),
                    expected,
                    actual: parse_unit_actual(fields[3])?,
                    passed: termination == reporch_runtime_core::GuestTerminationV2::Exited
                        && fields[3] == expected,
                    exit_code,
                    termination,
                    duration_ms: 0,
                    stdout,
                    stderr,
                });
            }
            "S" => {
                let test = spec
                    .testing
                    .tests
                    .get(case_index)
                    .context("solution test result index is out of range")?;
                let actual = parse_verdict(fields[3])?;
                solution_cases
                    .get_mut(owner)
                    .context("solution result index is out of range")?
                    .push(LocalSolutionCase {
                        test_id: test.id,
                        test: test.name.clone(),
                        actual: observed_verdict_name(actual),
                        accepted: actual == Some(ExpectedVerdict::Accepted),
                        exit_code,
                        termination,
                        duration_ms: 0,
                        stdout,
                        stderr,
                    });
            }
            _ => anyhow::bail!("unknown local verification result frame"),
        }
    }
    ensure!(completed, "local verification result is incomplete");
    ensure!(
        generator_checks.len() == generator_targets.len(),
        "local verification omitted a generator check"
    );
    if !validator_results.is_empty() {
        let silent = CliOutput::new(
            crate::cli_output::OutputFormat::Human,
            true,
            crate::cli_output::ColorMode::Never,
        );
        emit_unit_report("validator run", validator_results, &silent)?;
    }
    let mut solutions = Vec::new();
    for (solution, cases) in spec.testing.solutions.iter().zip(solution_cases) {
        ensure!(
            cases.len() == spec.testing.tests.len(),
            "local verification omitted a solution case"
        );
        let score = score_v2(&spec.testing.groups, &spec.testing.tests, &cases)?;
        let actual =
            aggregate_solution_verdict(&cases, score, total_score_v2(&spec.testing.groups));
        let score_matches = solution
            .expected_score
            .as_ref()
            .is_none_or(|range| score >= range.minimum && score <= range.maximum);
        solutions.push(LocalSolutionCheck {
            solution: solution.program.name.clone(),
            expected: verdict_name(solution.expected_verdict),
            actual: observed_verdict_name(actual),
            score,
            passed: actual == Some(solution.expected_verdict) && score_matches,
            cases,
        });
    }
    Ok(Some(BatchVerifyResult {
        generator_checks,
        validator_units: spec.testing.validators.unit_tests.len(),
        checker_units: spec.testing.checker.unit_tests.len(),
        solutions,
    }))
}

fn verify_standard_checker_units(
    root: &Path,
    spec: &reporch_format::AuthoringSpecV2,
) -> Result<()> {
    let mut cases = Vec::new();
    for unit in &spec.testing.checker.unit_tests {
        let answer = read_project_bytes(root, &unit.answer_file)?;
        let actual = read_project_bytes(root, &unit.output_file)?;
        let accepted = reporch_cli::authoring_runtime::standard_checker_matches(
            &spec.testing.checker.checker,
            &answer,
            &actual,
        )?;
        cases.push(ProgramUnitResult {
            program: "checker".into(),
            name: unit.name.clone(),
            expected: if unit.expected_accepted {
                "accepted"
            } else {
                "rejected"
            },
            actual: if accepted { "accepted" } else { "rejected" },
            passed: accepted == unit.expected_accepted,
            exit_code: 0,
            termination: reporch_runtime_core::GuestTerminationV2::Exited,
            duration_ms: 0,
            stdout: String::new(),
            stderr: String::new(),
        });
    }
    if cases.is_empty() {
        return Ok(());
    }
    let silent = CliOutput::new(
        crate::cli_output::OutputFormat::Human,
        true,
        crate::cli_output::ColorMode::Never,
    );
    emit_unit_report("checker run", cases, &silent)
}

fn program_shell(
    language: &str,
    label: &str,
    program: &studio_core::ProgramSpecV2,
) -> Result<(String, String)> {
    let source = shell_quote(&format!("/workspace/{}", program.source_path));
    let arguments = program
        .arguments
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
        "python" => Ok((String::new(), format!("python3 {source}{suffix}"))),
        "pypy" => Ok((String::new(), format!("pypy3 {source}{suffix}"))),
        "javascript" => Ok((String::new(), format!("node {source}{suffix}"))),
        "php" => Ok((String::new(), format!("php {source}{suffix}"))),
        "r" => Ok((String::new(), format!("Rscript {source}{suffix}"))),
        "bash" => Ok((String::new(), format!("bash {source}{suffix}"))),
        "c" => Ok((
            format!("cc -std=c17 -O2 -pipe {source} -o /run/reporch/{label}\n"),
            format!("/run/reporch/{label}{suffix}"),
        )),
        "cpp" => Ok((
            format!("c++ -std=c++20 -O2 -pipe {source} -o /run/reporch/{label}\n"),
            format!("/run/reporch/{label}{suffix}"),
        )),
        "rust" => Ok((
            format!("rustc --edition=2024 -O {source} -o /run/reporch/{label}\n"),
            format!("/run/reporch/{label}{suffix}"),
        )),
        "swift" => Ok((
            format!("swiftc -O {source} -o /run/reporch/{label}\n"),
            format!("/run/reporch/{label}{suffix}"),
        )),
        _ => anyhow::bail!("unsupported batched verification language: {language}"),
    }
}

fn append_execution(
    script: &mut String,
    command: &str,
    input: Option<&str>,
    output: &str,
    timeout: std::time::Duration,
) -> Result<()> {
    script.push_str("set +e\n");
    let input = input
        .map(|path| format!(" < {}", shell_quote(path)))
        .unwrap_or_default();
    writeln!(
        script,
        "timeout --signal=KILL --kill-after=1s {:.3}s {command}{input} > \"{output}\" 2> \"$err\"",
        timeout.as_secs_f64(),
    )?;
    script.push_str("status=$?\nset -e\n");
    Ok(())
}

fn checker_shell(checker: &CheckerSpec, answer: &str, actual: &str) -> String {
    let answer = shell_quote(answer);
    match checker {
        CheckerSpec::Exact => format!("cmp -s {answer} \"{actual}\""),
        CheckerSpec::Token => format!(
            "awk '{{for(i=1;i<=NF;i++) print $i}}' {answer} > /run/reporch/answer.tokens && awk '{{for(i=1;i<=NF;i++) print $i}}' \"{actual}\" > /run/reporch/actual.tokens && cmp -s /run/reporch/answer.tokens /run/reporch/actual.tokens"
        ),
        CheckerSpec::CaseInsensitive => format!(
            "awk '{{for(i=1;i<=NF;i++) print tolower($i)}}' {answer} > /run/reporch/answer.tokens && awk '{{for(i=1;i<=NF;i++) print tolower($i)}}' \"{actual}\" > /run/reporch/actual.tokens && cmp -s /run/reporch/answer.tokens /run/reporch/actual.tokens"
        ),
        CheckerSpec::Floating { .. } | CheckerSpec::Custom { .. } => "false".into(),
    }
}

fn guest_path(root: &Path, path: &str) -> Result<String> {
    let normalized = studio_core::normalize_relative_path(path)?;
    let _ = read_project_bytes(root, &normalized)?;
    Ok(format!("/workspace/{normalized}"))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn decode_text(value: &str) -> Result<String> {
    Ok(String::from_utf8_lossy(&decode_bytes(value)?).into_owned())
}

fn decode_bytes(value: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(value)
        .context("decode local verification output")
}

fn parse_termination(value: &str) -> Result<reporch_runtime_core::GuestTerminationV2> {
    match value {
        "exited" => Ok(reporch_runtime_core::GuestTerminationV2::Exited),
        "timed_out" => Ok(reporch_runtime_core::GuestTerminationV2::TimedOut),
        "signalled" => Ok(reporch_runtime_core::GuestTerminationV2::Signalled),
        _ => anyhow::bail!("unknown local verification termination: {value}"),
    }
}

fn parse_unit_actual(value: &str) -> Result<&'static str> {
    match value {
        "valid" => Ok("valid"),
        "invalid" => Ok("invalid"),
        "timed_out" => Ok("timed_out"),
        "runtime_error" => Ok("runtime_error"),
        _ => anyhow::bail!("unknown validator result: {value}"),
    }
}

fn parse_verdict(value: &str) -> Result<Option<ExpectedVerdict>> {
    match value {
        "accepted" => Ok(Some(ExpectedVerdict::Accepted)),
        "wrong_answer" => Ok(Some(ExpectedVerdict::WrongAnswer)),
        "time_limit" => Ok(Some(ExpectedVerdict::TimeLimit)),
        "runtime_error" => Ok(Some(ExpectedVerdict::RuntimeError)),
        "judge_error" => Ok(None),
        _ => anyhow::bail!("unknown solution verdict: {value}"),
    }
}
