use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::Path;

use anyhow::{Context, Result, bail};
use studio_core::{
    CheckerSpec, CustomImplExpectedOutputMode, CustomImplInputMode, CustomImplProfileV1,
    ExecutionHarnessV1, ExpectedScoreRange, ExpectedVerdict, InteractiveStdioProfileV1,
    JudgingSpec, ManifestFile, OutputSubmissionSpec, PackageProfile, ProblemType,
    PublicationSampleV1, PublicationSpecV1, RELEASE_MANIFEST_SCHEMA_V1, ReleaseManifestV1,
    ResourceLimits, ScoreAggregation, SolutionSpec, StatementSectionsV1, TestCaseSpec,
    TestGroupSpec,
};
use uuid::Uuid;

struct TemplateFile {
    path: &'static str,
    content: Vec<u8>,
    media_type: &'static str,
    executable: bool,
}

impl TemplateFile {
    fn text(path: &'static str, content: impl Into<Vec<u8>>, media_type: &'static str) -> Self {
        Self {
            path,
            content: content.into(),
            media_type,
            executable: false,
        }
    }

    fn executable(path: &'static str, content: impl Into<Vec<u8>>) -> Self {
        Self {
            path,
            content: content.into(),
            media_type: "text/x-shellscript",
            executable: true,
        }
    }

    fn manifest_file(&self) -> ManifestFile {
        ManifestFile {
            path: self.path.into(),
            sha256: studio_core::Sha256Digest::from_bytes(&self.content),
            size_bytes: self.content.len() as u64,
            media_type: self.media_type.into(),
            executable: self.executable,
        }
    }
}

pub fn init_project_with_id(directory: &Path, title: &str, project_id: Uuid) -> Result<()> {
    init_project_template(directory, title, project_id, ProblemType::Standard)
}

pub fn init_project_template(
    directory: &Path,
    title: &str,
    project_id: Uuid,
    problem_type: ProblemType,
) -> Result<()> {
    let title = title.trim();
    if title.is_empty() {
        bail!("title is required");
    }
    preflight_init_directory(directory)?;

    let statement = format!(
        "# {title}\n\n이 템플릿은 생성 직후 검증 가능한 최소 예제입니다. 문제 설명과 구현을 교체하세요.\n"
    );
    let (sample_input, sample_answer) = match problem_type {
        ProblemType::Interactive => ("2\n", "4\n"),
        _ => ("1 2\n", "3\n"),
    };
    let mut files = vec![
        TemplateFile::text("statements/ko.md", statement, "text/markdown"),
        TemplateFile::text("tests/1.in", sample_input, "text/plain"),
        TemplateFile::text("tests/1.ans", sample_answer, "text/plain"),
    ];

    match problem_type {
        ProblemType::Standard => add_python_solutions(&mut files),
        ProblemType::Scored => {
            add_python_solutions(&mut files);
            files.extend([
                TemplateFile::text("tests/2.in", "100 200\n", "text/plain"),
                TemplateFile::text("tests/2.ans", "300\n", "text/plain"),
                TemplateFile::text(
                    "solutions/partial.py",
                    "a, b = map(int, input().split())\nprint(a + b if a <= 10 and b <= 10 else 0)\n",
                    "text/x-python",
                ),
            ]);
        }
        ProblemType::OutputOnly => files.extend([
            TemplateFile::text("outputs/official.txt", sample_answer, "text/plain"),
            TemplateFile::text("outputs/known-wrong.txt", "-1\n", "text/plain"),
        ]),
        ProblemType::Interactive => {
            add_cpp_solutions(&mut files, true);
            files.extend([
                TemplateFile::text(
                    "templates/cpp/solution.cpp",
                    "#include <iostream>\nint main(){ return 0; }\n",
                    "text/x-c++src",
                ),
                TemplateFile::text(
                    "interactive/interactor.cpp",
                    "#include <fstream>\n#include <iostream>\nint main(int argc, char** argv){ if(argc < 2) return 2; std::ifstream input(argv[1]); long long n, answer; input >> n; std::cout << n << std::endl; if(!(std::cin >> answer)) return 1; return answer == 2 * n ? 0 : 1; }\n",
                    "text/x-c++src",
                ),
            ]);
        }
        ProblemType::Library | ProblemType::Grader => {
            add_cpp_solutions(&mut files, false);
            files.extend([
                TemplateFile::text(
                    "cpp/solution.cpp",
                    "long long solve(long long a, long long b){ return 0; }\n",
                    "text/x-c++src",
                ),
                TemplateFile::text(
                    "cpp/grader.cpp",
                    "#include <iostream>\nextern long long solve(long long, long long);\nint main(){ long long a, b; if(std::cin >> a >> b) std::cout << solve(a, b) << '\\n'; }\n",
                    "text/x-c++src",
                ),
                TemplateFile::executable(
                    "cpp/compile.sh",
                    "#!/bin/sh\nset -eu\ng++ -std=gnu++17 -O2 solution.cpp grader.cpp -o main\n",
                ),
                TemplateFile::executable("cpp/run.sh", "#!/bin/sh\nset -eu\nexec ./main\n"),
            ]);
        }
    }

    for file in &files {
        let output = directory.join(file.path);
        if let Some(parent) = output.parent() {
            fs::create_dir_all(parent)?;
        }
        write_new_file(&output, &file.content, file.executable)?;
    }

    let test_id = Uuid::now_v7();
    let mut tests = vec![TestCaseSpec {
        id: test_id,
        name: "sample-1".into(),
        input_file: "tests/1.in".into(),
        answer_file: Some("tests/1.ans".into()),
        groups: if problem_type == ProblemType::Scored {
            vec!["easy".into()]
        } else {
            vec![]
        },
        generated_by: None,
        generator_arguments: vec![],
        seed: None,
    }];
    if problem_type == ProblemType::Scored {
        tests.push(TestCaseSpec {
            id: Uuid::now_v7(),
            name: "hidden-2".into(),
            input_file: "tests/2.in".into(),
            answer_file: Some("tests/2.ans".into()),
            groups: vec!["hard".into()],
            generated_by: None,
            generator_arguments: vec![],
            seed: None,
        });
    }

    let (interactor_path, interactor_language, grader_path, grader_language, harness) =
        specialized_harness(problem_type);
    let manifest = ReleaseManifestV1 {
        schema: RELEASE_MANIFEST_SCHEMA_V1.into(),
        project_id,
        commit_id: Uuid::now_v7(),
        problem_type,
        package_profile: PackageProfile::ReporchNative,
        default_locale: "ko".into(),
        title: BTreeMap::from([("ko".into(), title.into())]),
        statements: BTreeMap::from([("ko".into(), "statements/ko.md".into())]),
        files: files.iter().map(TemplateFile::manifest_file).collect(),
        toolchains: BTreeMap::new(),
        judging: JudgingSpec {
            limits: ResourceLimits {
                time_ms: 1_000,
                memory_mib: 256,
                output_kib: 64 * 1_024,
            },
            checker: CheckerSpec::Token,
            tests,
            groups: scoring_groups(problem_type),
            generators: vec![],
            validator_path: None,
            validator_language: None,
            extra_validator_paths: vec![],
            extra_validators: vec![],
            validator_tests: vec![],
            checker_tests: vec![],
            interactor_path,
            interactor_language,
            grader_path,
            grader_language,
            harness,
        },
        sources: vec![],
        solutions: template_solutions(problem_type),
        output_submissions: template_output_submissions(problem_type, test_id),
        publication: Some(template_publication(problem_type)),
        policy_version: "studio-policy-v1".into(),
    };
    let issues = studio_core::validate_manifest(&manifest);
    if !issues.is_empty() {
        bail!(
            "generated template failed validation: {}",
            serde_json::to_string(&issues)?
        );
    }
    write_new_file(
        &directory.join("reporch.problem.json"),
        &serde_json::to_vec_pretty(&manifest)?,
        false,
    )?;
    Ok(())
}

fn add_python_solutions(files: &mut Vec<TemplateFile>) {
    files.extend([
        TemplateFile::text(
            "solutions/accepted.py",
            "a, b = map(int, input().split())\nprint(a + b)\n",
            "text/x-python",
        ),
        TemplateFile::text(
            "solutions/accepted-alt.py",
            "values = list(map(int, input().split()))\nprint(sum(values))\n",
            "text/x-python",
        ),
        TemplateFile::text(
            "solutions/wrong.py",
            "a, b = map(int, input().split())\nprint(a - b)\n",
            "text/x-python",
        ),
    ]);
}

fn add_cpp_solutions(files: &mut Vec<TemplateFile>, interactive: bool) {
    let contents = if interactive {
        [
            "#include <iostream>\nint main(){ long long n; if(std::cin >> n) std::cout << 2 * n << '\\n'; }\n",
            "#include <iostream>\nint main(){ long long value; if(std::cin >> value) std::cout << value + value << '\\n'; }\n",
            "#include <iostream>\nint main(){ long long n; if(std::cin >> n) std::cout << 3 * n << '\\n'; }\n",
        ]
    } else {
        [
            "long long solve(long long a, long long b){ return a + b; }\n",
            "long long solve(long long a, long long b){ return b + a; }\n",
            "long long solve(long long a, long long b){ return a - b; }\n",
        ]
    };
    for (path, content) in [
        "solutions/accepted.cpp",
        "solutions/accepted-alt.cpp",
        "solutions/wrong.cpp",
    ]
    .into_iter()
    .zip(contents)
    {
        files.push(TemplateFile::text(path, content, "text/x-c++src"));
    }
}

fn scoring_groups(problem_type: ProblemType) -> Vec<TestGroupSpec> {
    if problem_type != ProblemType::Scored {
        return vec![];
    }
    vec![
        TestGroupSpec {
            id: "easy".into(),
            points: 50.0,
            depends_on: vec![],
            feedback_policy: studio_core::GroupFeedbackPolicyV1::Complete,
        },
        TestGroupSpec {
            id: "hard".into(),
            points: 50.0,
            depends_on: vec!["easy".into()],
            feedback_policy: studio_core::GroupFeedbackPolicyV1::Complete,
        },
    ]
}

fn template_solutions(problem_type: ProblemType) -> Vec<SolutionSpec> {
    if problem_type == ProblemType::OutputOnly {
        return vec![];
    }
    let (extension, language) = match problem_type {
        ProblemType::Interactive | ProblemType::Library | ProblemType::Grader => ("cpp", "cpp"),
        _ => ("py", "python3"),
    };
    let mut solutions = vec![
        SolutionSpec {
            name: "accepted".into(),
            source_path: format!("solutions/accepted.{extension}"),
            language: language.into(),
            expected_verdict: ExpectedVerdict::Accepted,
            expected_score: None,
        },
        SolutionSpec {
            name: "accepted-alt".into(),
            source_path: format!("solutions/accepted-alt.{extension}"),
            language: language.into(),
            expected_verdict: ExpectedVerdict::Accepted,
            expected_score: None,
        },
    ];
    if problem_type == ProblemType::Scored {
        solutions.push(SolutionSpec {
            name: "partial-50".into(),
            source_path: "solutions/partial.py".into(),
            language: "python3".into(),
            expected_verdict: ExpectedVerdict::Partial,
            expected_score: Some(ExpectedScoreRange {
                minimum: 50.0,
                maximum: 50.0,
            }),
        });
    }
    solutions.push(SolutionSpec {
        name: "known-wrong".into(),
        source_path: format!("solutions/wrong.{extension}"),
        language: language.into(),
        expected_verdict: ExpectedVerdict::WrongAnswer,
        expected_score: None,
    });
    solutions
}

fn template_output_submissions(
    problem_type: ProblemType,
    test_id: Uuid,
) -> Vec<OutputSubmissionSpec> {
    if problem_type != ProblemType::OutputOnly {
        return vec![];
    }
    vec![
        OutputSubmissionSpec {
            name: "official".into(),
            outputs: BTreeMap::from([(test_id, "outputs/official.txt".into())]),
            expected_verdict: ExpectedVerdict::Accepted,
            expected_score: None,
        },
        OutputSubmissionSpec {
            name: "known-wrong".into(),
            outputs: BTreeMap::from([(test_id, "outputs/known-wrong.txt".into())]),
            expected_verdict: ExpectedVerdict::WrongAnswer,
            expected_score: None,
        },
    ]
}

type HarnessParts = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<ExecutionHarnessV1>,
);

fn specialized_harness(problem_type: ProblemType) -> HarnessParts {
    match problem_type {
        ProblemType::Interactive => (
            Some("interactive/interactor.cpp".into()),
            Some("cpp".into()),
            None,
            None,
            Some(ExecutionHarnessV1::InteractiveStdio {
                profiles: BTreeMap::from([(
                    "cpp".into(),
                    InteractiveStdioProfileV1 {
                        source_path: "templates/cpp/solution.cpp".into(),
                        interactor_source_path: "interactive/interactor.cpp".into(),
                        asset_paths: vec![
                            "templates/cpp/solution.cpp".into(),
                            "interactive/interactor.cpp".into(),
                        ],
                        include_dirs: vec![],
                        idle_timeout_ms: 2_000,
                        transcript_limit_kib: 256,
                        solver_compile_command: None,
                        solver_run_command: None,
                        interactor_compile_command: None,
                        interactor_run_command: None,
                    },
                )]),
                score_type: ScoreAggregation::GroupMin,
                score_scale: 100,
            }),
        ),
        ProblemType::Library | ProblemType::Grader => (
            None,
            None,
            Some("cpp/grader.cpp".into()),
            Some("cpp".into()),
            Some(ExecutionHarnessV1::CustomImpl {
                profiles: BTreeMap::from([(
                    "cpp".into(),
                    CustomImplProfileV1 {
                        source_path: "cpp/solution.cpp".into(),
                        asset_paths: vec![
                            "cpp/solution.cpp".into(),
                            "cpp/grader.cpp".into(),
                            "cpp/compile.sh".into(),
                            "cpp/run.sh".into(),
                        ],
                        compile_script: Some("cpp/compile.sh".into()),
                        run_script: Some("cpp/run.sh".into()),
                        compile_command: None,
                        run_command: None,
                    },
                )]),
                input_mode: CustomImplInputMode::Raw,
                expected_output_mode: CustomImplExpectedOutputMode::Raw,
            }),
        ),
        _ => (None, None, None, None, None),
    }
}

fn template_publication(problem_type: ProblemType) -> PublicationSpecV1 {
    let (difficulty, allowed_languages, note) = match problem_type {
        ProblemType::Scored => (
            "Silver 5",
            vec!["python3".into()],
            "easy와 hard 그룹은 각각 50점이며 hard는 easy에 의존합니다.",
        ),
        ProblemType::OutputOnly => (
            "Silver 5",
            vec![],
            "참가자는 각 테스트에 대한 출력 파일을 제출합니다.",
        ),
        ProblemType::Interactive => (
            "Silver 5",
            vec!["cpp".into()],
            "Interactor와 표준 입출력 프로토콜로 통신합니다.",
        ),
        ProblemType::Library | ProblemType::Grader => (
            "Silver 5",
            vec!["cpp".into()],
            "제출 코드는 공개 인터페이스를 구현하고 private grader와 빌드됩니다.",
        ),
        ProblemType::Standard => ("Bronze 5", vec!["python3".into()], ""),
    };
    PublicationSpecV1 {
        category: "Algorithm".into(),
        difficulty: difficulty.into(),
        grading_category: "algorithmic".into(),
        tags: vec![],
        allowed_languages,
        statement_sections: BTreeMap::from([(
            "ko".into(),
            StatementSectionsV1 {
                input_format: "예제 입력 형식이 주어집니다.".into(),
                output_format: "요구한 값을 출력합니다.".into(),
                note: note.into(),
            },
        )]),
        samples: vec![PublicationSampleV1 {
            name: "sample-1".into(),
            input_file: "tests/1.in".into(),
            output_file: "tests/1.ans".into(),
        }],
    }
}

pub fn preflight_init_directory(directory: &Path) -> Result<()> {
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

fn write_new_file(path: &Path, bytes: &[u8], executable: bool) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("create {} without overwrite", path.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    #[cfg(unix)]
    if executable {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    }
    #[cfg(not(unix))]
    let _ = executable;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_problem_type_template_is_valid_and_never_overwrites() {
        for problem_type in [
            ProblemType::Standard,
            ProblemType::Scored,
            ProblemType::Interactive,
            ProblemType::OutputOnly,
            ProblemType::Library,
            ProblemType::Grader,
        ] {
            let temporary = tempfile::tempdir().unwrap();
            init_project_template(temporary.path(), "Example", Uuid::now_v7(), problem_type)
                .unwrap();
            let manifest: ReleaseManifestV1 = serde_json::from_slice(
                &fs::read(temporary.path().join("reporch.problem.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(manifest.problem_type, problem_type);
            assert!(studio_core::validate_manifest(&manifest).is_empty());
            assert!(
                init_project_template(temporary.path(), "Again", Uuid::now_v7(), problem_type)
                    .is_err()
            );
        }
    }
}
