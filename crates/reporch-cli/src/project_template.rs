use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use cap_std::ambient_authority;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use studio_core::{
    CheckerSpec, CustomImplExpectedOutputMode, CustomImplInputMode, CustomImplProfileV1,
    ExecutionHarnessV1, ExpectedScoreRange, ExpectedVerdict, InteractiveStdioProfileV1,
    JudgingSpec, ManifestFile, OutputSubmissionSpec, PackageProfile, ProblemType,
    PublicationSampleV1, PublicationSpecV1, RELEASE_MANIFEST_SCHEMA_V1, ReleaseManifestV1,
    ResourceLimits, ScoreAggregation, SolutionSpec, StatementSectionsV1, TestCaseSpec,
    TestGroupSpec,
};
use uuid::Uuid;

const INIT_TRANSACTION_SCHEMA_V1: &str = "reporch.project-init-transaction.v1";
const INIT_TRANSACTION_PATH: &str = ".reporch-init-transaction.json";
const INIT_TRANSACTION_TEMP_PATH: &str = ".reporch-init-transaction.tmp";
const INIT_LOCK_ROOT_NAME: &str = "reporch-cli-project-init-locks";
const MAX_INIT_JOURNAL_BYTES: u64 = 256 * 1024;
const MAX_INIT_TRANSACTION_FILES: usize = 64;
const MAX_INIT_TRANSACTION_DIRECTORIES: usize = 64;
const MAX_INIT_TEMPLATE_FILE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_INIT_TRANSACTION_PATH_BYTES: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InitTransactionJournal {
    schema: String,
    transaction_id: Uuid,
    files: Vec<InitTransactionFile>,
    directories: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InitTransactionFile {
    path: String,
    temporary_path: String,
    sha256: studio_core::Sha256Digest,
    size_bytes: u64,
}

#[derive(Debug)]
struct ProjectInitLock {
    _file: fs::File,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InitRecoveryOutcome {
    None,
    RolledBack,
    Committed,
}

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
    init_project_template_with_options(directory, title, project_id, problem_type, false)
}

pub fn init_project_template_with_options(
    directory: &Path,
    title: &str,
    project_id: Uuid,
    problem_type: ProblemType,
    allow_non_empty: bool,
) -> Result<()> {
    init_project_template_versioned(
        directory,
        title,
        project_id,
        problem_type,
        true,
        allow_non_empty,
        Some(project_id),
    )
}

pub fn init_project_template_with_optional_id(
    directory: &Path,
    title: &str,
    project_id: Option<Uuid>,
    problem_type: ProblemType,
    allow_non_empty: bool,
) -> Result<()> {
    let generated_project_id = project_id.unwrap_or_else(Uuid::now_v7);
    init_project_template_versioned(
        directory,
        title,
        generated_project_id,
        problem_type,
        true,
        allow_non_empty,
        project_id,
    )
}

#[doc(hidden)]
pub fn init_legacy_v1_project_template(
    directory: &Path,
    title: &str,
    project_id: Uuid,
    problem_type: ProblemType,
) -> Result<()> {
    init_project_template_versioned(
        directory,
        title,
        project_id,
        problem_type,
        false,
        false,
        Some(project_id),
    )
}

fn init_project_template_versioned(
    directory: &Path,
    title: &str,
    project_id: Uuid,
    problem_type: ProblemType,
    emit_v2: bool,
    allow_non_empty: bool,
    required_project_id: Option<Uuid>,
) -> Result<()> {
    let title = title.trim();
    if title.is_empty() {
        bail!("title is required");
    }
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
    let authoring_v1 = reporch_format::AuthoringSpecV1::from_manifest(&manifest);
    if emit_v2 {
        let authoring_v2 = reporch_format::AuthoringSpecV2::migrate_v1(&authoring_v1)?;
        let manifest_v2 = authoring_v2.materialize(manifest.commit_id, manifest.files.clone())?;
        files.extend([
            TemplateFile::text(
                "reporch.problem.json",
                serde_json::to_vec_pretty(&manifest_v2)?,
                "application/json",
            ),
            TemplateFile::text(
                "reporch.yaml",
                reporch_format::to_authoring_yaml_v2(&authoring_v2)?,
                "application/yaml",
            ),
        ]);
    } else {
        files.extend([
            TemplateFile::text(
                "reporch.problem.json",
                serde_json::to_vec_pretty(&manifest)?,
                "application/json",
            ),
            TemplateFile::text(
                "reporch.yaml",
                reporch_format::to_authoring_yaml(&authoring_v1)?,
                "application/yaml",
            ),
        ]);
    }
    preflight_init_before_lock(directory, &files, allow_non_empty)?;
    let _lock = ProjectInitLock::acquire(directory)?;
    let directory_existed = directory.exists();
    if !directory_existed {
        fs::create_dir_all(directory)
            .with_context(|| format!("create project directory {}", directory.display()))?;
    }
    let root = open_project_directory_capability(directory)?;
    let recovery = recover_interrupted_template_transaction(&root, &files, required_project_id)?;
    if recovery == InitRecoveryOutcome::Committed {
        return Ok(());
    }
    if recovery == InitRecoveryOutcome::RolledBack && !allow_non_empty {
        preflight_recovered_template_destinations(directory, &files)?;
    } else {
        preflight_template_destinations(directory, &files, allow_non_empty)?;
    }
    let result = write_template_transaction(&root, &files);
    drop(root);
    if result.is_err()
        && !directory_existed
        && fs::read_dir(directory)?.next().transpose()?.is_none()
    {
        fs::remove_dir(directory)
            .with_context(|| format!("remove empty project directory {}", directory.display()))?;
    }
    result
}

fn open_project_directory_capability(directory: &Path) -> Result<cap_std::fs::Dir> {
    let absolute = std::path::absolute(directory)?;
    let parent_path = absolute
        .parent()
        .context("project directory must have a parent directory")?;
    let name = absolute
        .file_name()
        .context("project directory cannot be a filesystem root")?;
    let parent = cap_std::fs::Dir::open_ambient_dir(parent_path, ambient_authority())
        .with_context(|| format!("open project parent capability {}", parent_path.display()))?;
    let parent_file = parent.try_clone()?.into_std_file();
    let directory = cap_primitives::fs::open_dir_nofollow(&parent_file, Path::new(name))
        .context("open project directory without following a symlink")?;
    Ok(cap_std::fs::Dir::from_std_file(directory))
}

fn preflight_recovered_template_destinations(
    directory: &Path,
    files: &[TemplateFile],
) -> Result<()> {
    let allowed_directories = files
        .iter()
        .flat_map(|file| {
            Path::new(file.path)
                .ancestors()
                .skip(1)
                .filter(|path| !path.as_os_str().is_empty())
                .map(PathBuf::from)
                .collect::<Vec<_>>()
        })
        .collect::<BTreeSet<_>>();
    validate_recovered_empty_directories(directory, directory, &allowed_directories)?;
    if directory.join(".reporch/state.json").exists() {
        bail!(
            "existing .reporch/state.json belongs to another local project state; move it aside or use `reporch project link` instead; no files were written"
        );
    }
    for relative in files.iter().map(|file| file.path) {
        preflight_generated_path(directory, Path::new(relative))?;
    }
    Ok(())
}

fn validate_recovered_empty_directories(
    root: &Path,
    directory: &Path,
    allowed_directories: &BTreeSet<PathBuf>,
) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        let relative = entry
            .path()
            .strip_prefix(root)
            .context("inspect recovered project initialization directory")?
            .to_path_buf();
        if !metadata.is_dir()
            || metadata.file_type().is_symlink()
            || !allowed_directories.contains(&relative)
        {
            bail!(
                "refusing to initialize alongside unrelated path {}; use `--allow-non-empty` only after reviewing the directory",
                entry.path().display()
            );
        }
        validate_recovered_empty_directories(root, &entry.path(), allowed_directories)?;
    }
    Ok(())
}

fn preflight_init_before_lock(
    directory: &Path,
    files: &[TemplateFile],
    allow_non_empty: bool,
) -> Result<()> {
    if !directory.exists() {
        return Ok(());
    }
    let metadata = fs::symlink_metadata(directory)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        bail!("project directory must be a real directory");
    }
    let root = cap_std::fs::Dir::open_ambient_dir(directory, ambient_authority())?;
    let marker = read_init_journal_if_present(&root, Path::new(INIT_TRANSACTION_PATH))?;
    let temporary = read_init_journal_if_present(&root, Path::new(INIT_TRANSACTION_TEMP_PATH))?;
    if marker.is_some() || temporary.is_some() {
        for journal in marker.iter().chain(temporary.iter()) {
            validate_init_journal(journal)?;
            validate_init_journal_against_templates(journal, files)?;
        }
        if let (Some(marker), Some(temporary)) = (&marker, &temporary)
            && marker != temporary
        {
            bail!(
                "project initialization recovery found mismatched transaction journals; no files were changed"
            );
        }
        return Ok(());
    }
    preflight_template_destinations(directory, files, allow_non_empty)
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

impl ProjectInitLock {
    fn acquire(directory: &Path) -> Result<Self> {
        let lock_root = project_init_lock_root()?;
        let lock_key = project_init_lock_key(directory)?;
        let lock_path = lock_root.join(format!("{lock_key}.lock"));
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("open project initialization lock {}", lock_path.display()))?;
        file.try_lock().map_err(|_| {
            anyhow::anyhow!(
                "another `reporch project init` is already running for {}; wait for it to finish and retry",
                directory.display()
            )
        })?;
        Ok(Self { _file: file })
    }
}

#[cfg(unix)]
fn project_init_lock_root() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context(
        "HOME is not set; set it to a private user directory before running `project init`",
    )?;
    secure_unix_lock_root(&fs::canonicalize(PathBuf::from(home)).context("resolve HOME")?)
}

#[cfg(unix)]
fn secure_unix_lock_root(home: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _};

    let home_metadata = fs::symlink_metadata(home).context("inspect HOME")?;
    let effective_uid = rustix::process::geteuid().as_raw();
    if !home_metadata.is_dir()
        || home_metadata.file_type().is_symlink()
        || home_metadata.uid() != effective_uid
        || home_metadata.mode() & 0o022 != 0
    {
        bail!(
            "HOME must resolve to a real directory owned by the current user and not writable by group or other users"
        );
    }

    let lock_root = home.join(format!(".{INIT_LOCK_ROOT_NAME}"));
    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700);
    match builder.create(&lock_root) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(error).context("create private project initialization lock directory");
        }
    }
    let metadata = fs::symlink_metadata(&lock_root)
        .context("inspect private project initialization lock directory")?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != effective_uid
        || metadata.mode() & 0o077 != 0
    {
        bail!(
            "project initialization lock directory must be a real 0700 user-owned directory: {}",
            lock_root.display()
        );
    }
    Ok(lock_root)
}

#[cfg(windows)]
fn project_init_lock_root() -> Result<PathBuf> {
    let user_root = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("APPDATA"))
        .context("LOCALAPPDATA and APPDATA are not set")?;
    let lock_root = PathBuf::from(user_root)
        .join("Reporch")
        .join(INIT_LOCK_ROOT_NAME);
    fs::create_dir_all(&lock_root).context("create project initialization lock directory")?;
    let metadata = fs::symlink_metadata(&lock_root)
        .context("inspect project initialization lock directory")?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        bail!(
            "project initialization lock directory must be a real directory: {}",
            lock_root.display()
        );
    }
    Ok(lock_root)
}

fn project_init_lock_key(directory: &Path) -> Result<studio_core::Sha256Digest> {
    let absolute = std::path::absolute(directory)?;
    let existing_ancestor = absolute
        .ancestors()
        .find(|ancestor| ancestor.exists())
        .context("project directory has no existing ancestor")?;
    let suffix = absolute.strip_prefix(existing_ancestor)?;
    let resolved = fs::canonicalize(existing_ancestor)?.join(suffix);
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        Ok(studio_core::Sha256Digest::from_bytes(
            resolved.as_os_str().as_bytes(),
        ))
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt as _;
        let bytes = resolved
            .as_os_str()
            .encode_wide()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        Ok(studio_core::Sha256Digest::from_bytes(&bytes))
    }
    #[cfg(not(any(unix, windows)))]
    Ok(studio_core::Sha256Digest::from_bytes(
        resolved.to_string_lossy().as_bytes(),
    ))
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
        bail!(
            "refusing to initialize a non-empty project directory; choose `--directory <new-empty-directory>` or explicitly use `--allow-non-empty` after reviewing the directory; Reporch collision-checks every generated path and never overwrites existing files"
        );
    }
    Ok(())
}

fn preflight_template_destinations(
    directory: &Path,
    files: &[TemplateFile],
    allow_non_empty: bool,
) -> Result<()> {
    if allow_non_empty {
        if directory.exists() {
            let metadata = fs::symlink_metadata(directory)?;
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                bail!("project directory must be a real directory");
            }
        }
    } else {
        preflight_init_directory(directory)?;
    }

    if directory.join(".reporch/state.json").exists() {
        bail!(
            "existing .reporch/state.json belongs to another local project state; move it aside or use `reporch project link` instead; no files were written"
        );
    }
    for relative in files.iter().map(|file| file.path) {
        preflight_generated_path(directory, Path::new(relative))?;
    }
    Ok(())
}

fn recover_interrupted_template_transaction(
    root: &cap_std::fs::Dir,
    expected_files: &[TemplateFile],
    required_project_id: Option<Uuid>,
) -> Result<InitRecoveryOutcome> {
    let journal = match read_init_journal_if_present(root, Path::new(INIT_TRANSACTION_PATH))? {
        Some(journal) => journal,
        None => {
            if let Some(orphan) =
                read_init_journal_if_present(root, Path::new(INIT_TRANSACTION_TEMP_PATH))?
            {
                validate_init_journal(&orphan)?;
                validate_init_journal_against_templates(&orphan, expected_files)?;
                root.remove_file(INIT_TRANSACTION_TEMP_PATH).with_context(
                    || "remove an interrupted project-init journal that was never published",
                )?;
                sync_capability_directory(root, None)?;
                return Ok(InitRecoveryOutcome::RolledBack);
            }
            return Ok(InitRecoveryOutcome::None);
        }
    };
    validate_init_journal(&journal)?;
    validate_init_journal_against_templates(&journal, expected_files)?;

    if let Some(temporary_journal) =
        read_init_journal_if_present(root, Path::new(INIT_TRANSACTION_TEMP_PATH))?
    {
        validate_init_journal(&temporary_journal)?;
        validate_init_journal_against_templates(&temporary_journal, expected_files)?;
        if temporary_journal != journal {
            bail!(
                "project initialization recovery found mismatched transaction journals; no files were changed; preserve the directory and contact Reporch support"
            );
        }
    }

    // Every temporary file is staged and synced before the first final path is
    // published. Recovery can finish a partial commit without ever treating an
    // untrusted journal as authority to delete a user-visible final path.
    let mut published_files = BTreeSet::new();
    for entry in &journal.files {
        let path = Path::new(&entry.path);
        match root.symlink_metadata(path) {
            Ok(metadata) => {
                if !metadata.is_file() || metadata.file_type().is_symlink() {
                    bail!(
                        "interrupted project initialization left a changed path at {}; no files were changed; move it aside and retry",
                        entry.path
                    );
                }
                if metadata.len() != entry.size_bytes
                    || hash_capability_file(root, path, entry.size_bytes)? != entry.sha256
                {
                    bail!(
                        "interrupted project initialization left a changed file at {}; no files were changed; move it aside and retry",
                        entry.path
                    );
                }
                published_files.insert(entry.path.as_str());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }

    if published_files.is_empty() {
        for entry in &journal.files {
            remove_capability_file_if_present(root, Path::new(&entry.temporary_path))?;
            sync_capability_file_parent(root, Path::new(&entry.temporary_path))?;
        }
        remove_capability_file_if_present(root, Path::new(INIT_TRANSACTION_TEMP_PATH))?;
        remove_capability_file_if_present(root, Path::new(INIT_TRANSACTION_PATH))?;
        sync_capability_directory(root, None)?;
        return Ok(InitRecoveryOutcome::RolledBack);
    }

    validate_recovered_template_documents(root, &journal, expected_files, required_project_id)?;

    let missing_files = journal
        .files
        .iter()
        .filter(|entry| !published_files.contains(entry.path.as_str()))
        .collect::<Vec<_>>();
    // Validate every staged source before publishing any missing final path.
    for entry in &missing_files {
        let temporary_path = Path::new(&entry.temporary_path);
        let metadata = root.symlink_metadata(temporary_path).with_context(|| {
            format!(
                "interrupted project initialization is missing staged data for {}; no files were changed",
                entry.path
            )
        })?;
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.len() != entry.size_bytes
            || hash_capability_file(root, temporary_path, entry.size_bytes)? != entry.sha256
        {
            bail!(
                "interrupted project initialization has invalid staged data for {}; no files were changed",
                entry.path
            );
        }
    }
    for entry in missing_files {
        let final_path = Path::new(&entry.path);
        if let Some(parent) = final_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            let metadata = root.symlink_metadata(parent).with_context(|| {
                format!(
                    "interrupted project initialization is missing the parent for {}; no additional files were published",
                    entry.path
                )
            })?;
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                bail!(
                    "interrupted project initialization has an unsafe parent for {}; no additional files were published",
                    entry.path
                );
            }
        }
        root.hard_link(&entry.temporary_path, root, final_path)
            .with_context(|| {
                format!(
                    "finish interrupted project initialization for {} without overwrite",
                    entry.path
                )
            })?;
        sync_capability_file_parent(root, final_path)?;
    }

    for entry in journal.files.iter().rev() {
        remove_capability_file_if_present(root, Path::new(&entry.temporary_path))?;
        sync_capability_file_parent(root, Path::new(&entry.path))?;
    }
    remove_capability_file_if_present(root, Path::new(INIT_TRANSACTION_TEMP_PATH))?;
    remove_capability_file_if_present(root, Path::new(INIT_TRANSACTION_PATH))?;
    sync_capability_directory(root, None)?;
    Ok(InitRecoveryOutcome::Committed)
}

fn read_init_journal_if_present(
    root: &cap_std::fs::Dir,
    path: &Path,
) -> Result<Option<InitTransactionJournal>> {
    match root.symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                bail!(
                    "reserved project initialization recovery path is not a regular file: {}; no files were removed",
                    path.display()
                );
            }
            if metadata.len() > MAX_INIT_JOURNAL_BYTES {
                bail!(
                    "project initialization recovery journal is too large: {}; no files were changed",
                    path.display()
                );
            }
            let bytes = read_capability_file(root, path)?;
            let journal = serde_json::from_slice(&bytes).with_context(|| {
                format!(
                    "reserved project initialization recovery file is invalid: {}; no files were removed",
                    path.display()
                )
            })?;
            Ok(Some(journal))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn read_capability_file(root: &cap_std::fs::Dir, path: &Path) -> Result<Vec<u8>> {
    let file = root
        .open(path)
        .with_context(|| format!("open {} within project directory", path.display()))?;
    let mut bytes = Vec::new();
    file.take(MAX_INIT_JOURNAL_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_INIT_JOURNAL_BYTES {
        bail!(
            "project initialization recovery journal is too large: {}; no files were changed",
            path.display()
        );
    }
    Ok(bytes)
}

fn hash_capability_file(
    root: &cap_std::fs::Dir,
    path: &Path,
    expected_size: u64,
) -> Result<studio_core::Sha256Digest> {
    if expected_size > MAX_INIT_TEMPLATE_FILE_BYTES {
        bail!(
            "project initialization recovery file exceeds the size limit: {}; no files were changed",
            path.display()
        );
    }
    let mut file = root
        .open(path)
        .with_context(|| format!("open {} within project directory", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 16 * 1024];
    let mut total = 0_u64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read as u64);
        if total > expected_size {
            bail!(
                "project initialization recovery file changed size: {}; no files were changed",
                path.display()
            );
        }
        hasher.update(&buffer[..read]);
    }
    if total != expected_size {
        bail!(
            "project initialization recovery file changed size: {}; no files were changed",
            path.display()
        );
    }
    Ok(hex::encode(hasher.finalize()).parse()?)
}

fn read_transaction_entry_bytes(
    root: &cap_std::fs::Dir,
    entry: &InitTransactionFile,
) -> Result<Vec<u8>> {
    let final_path = Path::new(&entry.path);
    let path = match root.symlink_metadata(final_path) {
        Ok(metadata) => {
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                bail!(
                    "recovered template document is not a regular file: {}",
                    entry.path
                );
            }
            final_path
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Path::new(&entry.temporary_path)
        }
        Err(error) => return Err(error.into()),
    };
    let metadata = root.symlink_metadata(path)?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() != entry.size_bytes
        || hash_capability_file(root, path, entry.size_bytes)? != entry.sha256
    {
        bail!(
            "recovered template document failed integrity validation: {}",
            entry.path
        );
    }
    let file = root.open(path)?;
    let mut bytes = Vec::with_capacity(entry.size_bytes as usize);
    file.take(entry.size_bytes + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 != entry.size_bytes {
        bail!(
            "recovered template document changed while reading: {}",
            entry.path
        );
    }
    Ok(bytes)
}

fn validate_recovered_template_documents(
    root: &cap_std::fs::Dir,
    journal: &InitTransactionJournal,
    expected_files: &[TemplateFile],
    required_project_id: Option<Uuid>,
) -> Result<()> {
    let Some(manifest_entry) = journal
        .files
        .iter()
        .find(|entry| entry.path == "reporch.problem.json")
    else {
        return Ok(());
    };
    let yaml_entry = journal
        .files
        .iter()
        .find(|entry| entry.path == "reporch.yaml")
        .context("project initialization recovery journal is missing reporch.yaml")?;
    let expected_manifest_bytes = expected_files
        .iter()
        .find(|entry| entry.path == "reporch.problem.json")
        .map(|entry| entry.content.as_slice())
        .context("expected project template is missing reporch.problem.json")?;
    let expected_yaml_bytes = expected_files
        .iter()
        .find(|entry| entry.path == "reporch.yaml")
        .map(|entry| entry.content.as_slice())
        .context("expected project template is missing reporch.yaml")?;

    let recovered_manifest: studio_core::VersionedReleaseManifest =
        serde_json::from_slice(&read_transaction_entry_bytes(root, manifest_entry)?)
            .context("parse recovered reporch.problem.json")?;
    recovered_manifest
        .validate_references()
        .context("recovered manifest contains invalid references")?;
    if let Some(required_project_id) = required_project_id
        && recovered_manifest.project_id() != required_project_id
    {
        bail!(
            "interrupted project initialization belongs to project {}, not the explicitly requested project {}; no files were changed",
            recovered_manifest.project_id(),
            required_project_id
        );
    }
    let recovered_authoring = reporch_format::parse_versioned_authoring_spec(
        &read_transaction_entry_bytes(root, yaml_entry)?,
    )
    .context("parse recovered reporch.yaml")?;
    recovered_authoring
        .validate_references()
        .context("recovered authoring spec contains invalid references")?;
    let materialized = recovered_authoring.materialize(
        recovered_manifest.commit_id(),
        recovered_manifest.files().to_vec(),
    )?;
    if serde_json::to_value(&materialized)? != serde_json::to_value(&recovered_manifest)? {
        bail!("recovered reporch.yaml and reporch.problem.json do not describe the same project");
    }

    let expected_manifest: studio_core::VersionedReleaseManifest =
        serde_json::from_slice(expected_manifest_bytes)?;
    let expected_authoring = reporch_format::parse_versioned_authoring_spec(expected_yaml_bytes)?;
    if normalize_template_identities(serde_json::to_value(&recovered_manifest)?)
        != normalize_template_identities(serde_json::to_value(&expected_manifest)?)
        || normalize_template_identities(serde_json::to_value(&recovered_authoring)?)
            != normalize_template_identities(serde_json::to_value(&expected_authoring)?)
    {
        bail!(
            "interrupted project initialization documents do not match the requested template; no additional files were published"
        );
    }
    Ok(())
}

fn normalize_template_identities(mut value: serde_json::Value) -> serde_json::Value {
    fn visit(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::String(string) if Uuid::parse_str(string).is_ok() => {
                *string = "<uuid>".into();
            }
            serde_json::Value::Array(values) => values.iter_mut().for_each(visit),
            serde_json::Value::Object(values) => values.values_mut().for_each(visit),
            _ => {}
        }
    }
    visit(&mut value);
    value
}

fn remove_capability_file_if_present(root: &cap_std::fs::Dir, path: &Path) -> Result<()> {
    match root.remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn validate_init_journal(journal: &InitTransactionJournal) -> Result<()> {
    if journal.schema != INIT_TRANSACTION_SCHEMA_V1 {
        bail!(
            "unsupported project initialization recovery schema: {}; no files were removed",
            journal.schema
        );
    }
    if journal.files.is_empty() {
        bail!("empty project initialization recovery journal; no files were removed");
    }
    if journal.files.len() > MAX_INIT_TRANSACTION_FILES
        || journal.directories.len() > MAX_INIT_TRANSACTION_DIRECTORIES
    {
        bail!("project initialization recovery journal exceeds safe limits; no files were changed");
    }
    let mut paths = BTreeSet::new();
    for (index, entry) in journal.files.iter().enumerate() {
        if entry.size_bytes > MAX_INIT_TEMPLATE_FILE_BYTES {
            bail!(
                "project initialization recovery file exceeds the size limit: {}; no files were changed",
                entry.path
            );
        }
        validate_journal_relative_path(&entry.path)?;
        validate_journal_relative_path(&entry.temporary_path)?;
        let final_path = Path::new(&entry.path);
        let expected_temporary =
            final_path
                .parent()
                .unwrap_or_else(|| Path::new(""))
                .join(format!(
                    ".reporch-init-{}-{index}.tmp",
                    journal.transaction_id.simple()
                ));
        if Path::new(&entry.temporary_path) != expected_temporary {
            bail!(
                "invalid project initialization recovery temporary path: {}; no files were removed",
                entry.temporary_path
            );
        }
        if !paths.insert(entry.path.as_str()) || !paths.insert(entry.temporary_path.as_str()) {
            bail!(
                "duplicate project initialization recovery path: {}; no files were removed",
                entry.path
            );
        }
    }
    for directory in &journal.directories {
        validate_journal_relative_path(directory)?;
        let directory_path = Path::new(directory);
        if !journal.files.iter().any(|entry| {
            Path::new(&entry.path)
                .ancestors()
                .skip(1)
                .any(|ancestor| ancestor == directory_path)
        }) {
            bail!(
                "project initialization recovery directory is not a generated-file parent: {directory}; no files were removed"
            );
        }
        if !paths.insert(directory.as_str()) {
            bail!(
                "duplicate project initialization recovery path: {directory}; no files were removed"
            );
        }
    }
    Ok(())
}

fn validate_init_journal_against_templates(
    journal: &InitTransactionJournal,
    expected_files: &[TemplateFile],
) -> Result<()> {
    if journal.files.len() != expected_files.len() {
        bail!(
            "interrupted project initialization does not match this template; rerun with the same title and problem type or move the reserved recovery journal aside; no files were changed"
        );
    }
    for (entry, expected) in journal.files.iter().zip(expected_files) {
        if entry.path != expected.path {
            bail!(
                "interrupted project initialization does not match this template at {}; rerun with the same title and problem type or move the reserved recovery journal aside; no files were changed",
                entry.path
            );
        }
        if !matches!(expected.path, "reporch.problem.json" | "reporch.yaml")
            && (entry.size_bytes != expected.content.len() as u64
                || entry.sha256 != studio_core::Sha256Digest::from_bytes(&expected.content))
        {
            bail!(
                "interrupted project initialization does not match this template at {}; rerun with the same title and problem type or move the reserved recovery journal aside; no files were changed",
                entry.path
            );
        }
    }
    Ok(())
}

fn validate_journal_relative_path(path: &str) -> Result<()> {
    if path.len() > MAX_INIT_TRANSACTION_PATH_BYTES {
        bail!("project initialization recovery path is too long; no files were changed");
    }
    let normalized = studio_core::normalize_relative_path(path).with_context(|| {
        format!("unsafe project initialization recovery path: {path}; no files were removed")
    })?;
    if normalized != path || matches!(path, INIT_TRANSACTION_PATH | INIT_TRANSACTION_TEMP_PATH) {
        bail!("unsafe project initialization recovery path: {path}; no files were removed");
    }
    Ok(())
}

fn preflight_generated_path(directory: &Path, relative: &Path) -> Result<()> {
    let mut current = directory.to_path_buf();
    if let Some(parent) = relative.parent() {
        for component in parent.components() {
            current.push(component.as_os_str());
            match fs::symlink_metadata(&current) {
                Ok(metadata) => {
                    if !metadata.is_dir() || metadata.file_type().is_symlink() {
                        bail!(
                            "generated path has an unsafe parent component: {}",
                            current.display()
                        );
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Err(error) => return Err(error.into()),
            }
        }
    }
    let destination = directory.join(relative);
    match fs::symlink_metadata(&destination) {
        Ok(_) => bail!(
            "generated path already exists: {}; no files were written",
            destination.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn write_template_transaction(root: &cap_std::fs::Dir, files: &[TemplateFile]) -> Result<()> {
    match root.symlink_metadata(".reporch/state.json") {
        Ok(_) => bail!(
            "existing .reporch/state.json belongs to another local project state; move it aside or use `reporch project link` instead; no files were written"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let journal = build_init_journal(root, files)?;
    let mut created_directories = BTreeSet::new();
    let temporary_files = journal
        .files
        .iter()
        .map(|entry| PathBuf::from(&entry.temporary_path))
        .collect::<Vec<_>>();
    let mut created_files = Vec::new();
    let result = (|| -> Result<()> {
        write_init_journal(root, &journal)?;
        for (template, journal_entry) in files.iter().zip(&journal.files) {
            let relative = Path::new(template.path);
            let parent = relative
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty());
            if let Some(parent) = parent {
                create_template_parent_directories(root, parent, &mut created_directories)?;
            }
            let temporary = Path::new(&journal_entry.temporary_path);
            let mut options = cap_std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            let mut file = root
                .open_with(temporary, &options)
                .with_context(|| format!("create temporary file for {}", relative.display()))?;
            file.write_all(&template.content)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                let file = file.into_std();
                if template.executable {
                    file.set_permissions(fs::Permissions::from_mode(0o755))?;
                }
                file.sync_all()?;
            }
            #[cfg(not(unix))]
            {
                let _ = template.executable;
                file.sync_all()?;
            }
            sync_capability_file_parent(root, temporary)?;
        }
        for journal_entry in &journal.files {
            let relative = Path::new(&journal_entry.path);
            let temporary = Path::new(&journal_entry.temporary_path);
            root.hard_link(temporary, root, relative).with_context(|| {
                format!("atomically create {} without overwrite", relative.display())
            })?;
            created_files.push(relative.to_path_buf());
            sync_capability_file_parent(root, relative)?;
        }
        for entry in &journal.files {
            let temporary = Path::new(&entry.temporary_path);
            root.remove_file(temporary)
                .with_context(|| format!("remove temporary file for {}", entry.path))?;
            sync_capability_file_parent(root, Path::new(&entry.path))?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        if !created_files.is_empty() {
            return Err(error.context(
                "project initialization was interrupted after commit began; durable recovery state was preserved and the next identical `project init` will finish safely",
            ));
        }
        let mut rollback_errors = rollback_template_transaction(
            root,
            &created_files,
            &temporary_files,
            &created_directories,
        );
        if rollback_errors.is_empty() {
            if let Err(error) =
                remove_capability_file_if_present(root, Path::new(INIT_TRANSACTION_TEMP_PATH))
            {
                rollback_errors.push(format!(
                    "remove transaction journal temporary file: {error}"
                ));
            }
            if let Err(error) =
                remove_capability_file_if_present(root, Path::new(INIT_TRANSACTION_PATH))
            {
                rollback_errors.push(format!("remove transaction journal: {error}"));
            }
            if let Err(error) = sync_capability_directory(root, None) {
                rollback_errors.push(format!("sync project directory after rollback: {error}"));
            }
        }
        if rollback_errors.is_empty() {
            return Err(error.context(
                "initialize project transaction failed; every path created by this attempt was rolled back",
            ));
        }
        return Err(error.context(format!(
            "initialize project transaction failed and rollback was incomplete: {}",
            rollback_errors.join("; ")
        )));
    }
    remove_capability_file_if_present(root, Path::new(INIT_TRANSACTION_TEMP_PATH))?;
    remove_capability_file_if_present(root, Path::new(INIT_TRANSACTION_PATH))?;
    sync_capability_directory(root, None)?;
    Ok(())
}

fn build_init_journal(
    root: &cap_std::fs::Dir,
    files: &[TemplateFile],
) -> Result<InitTransactionJournal> {
    let transaction_id = Uuid::now_v7();
    let mut directories = BTreeSet::new();
    let transaction_files = files
        .iter()
        .enumerate()
        .map(|(index, template)| {
            let path = Path::new(template.path);
            if let Some(parent) = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
            {
                collect_missing_parent_directories(root, parent, &mut directories)?;
            }
            let temporary_path = path.parent().unwrap_or_else(|| Path::new("")).join(format!(
                ".reporch-init-{}-{index}.tmp",
                transaction_id.simple()
            ));
            Ok(InitTransactionFile {
                path: template.path.into(),
                temporary_path: portable_relative_path(&temporary_path)?,
                sha256: studio_core::Sha256Digest::from_bytes(&template.content),
                size_bytes: template.content.len() as u64,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let journal = InitTransactionJournal {
        schema: INIT_TRANSACTION_SCHEMA_V1.into(),
        transaction_id,
        files: transaction_files,
        directories: directories
            .into_iter()
            .map(|path| portable_relative_path(&path))
            .collect::<Result<Vec<_>>>()?,
    };
    validate_init_journal(&journal)?;
    Ok(journal)
}

fn portable_relative_path(path: &Path) -> Result<String> {
    let mut output = String::new();
    for component in path.components() {
        let std::path::Component::Normal(value) = component else {
            bail!("project initialization path must be relative and normalized");
        };
        let value = value
            .to_str()
            .context("project initialization path must be valid UTF-8")?;
        if !output.is_empty() {
            output.push('/');
        }
        output.push_str(value);
    }
    if output.is_empty() {
        bail!("project initialization path cannot be empty");
    }
    Ok(output)
}

fn write_init_journal(root: &cap_std::fs::Dir, journal: &InitTransactionJournal) -> Result<()> {
    for path in [INIT_TRANSACTION_PATH, INIT_TRANSACTION_TEMP_PATH] {
        match root.symlink_metadata(path) {
            Ok(_) => bail!(
                "project initialization recovery path already exists: {path}; no template files were written"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    let bytes = serde_json::to_vec(journal)?;
    if bytes.len() as u64 > MAX_INIT_JOURNAL_BYTES {
        bail!("project initialization recovery journal exceeds the size limit");
    }
    let mut options = cap_std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    let mut file = root
        .open_with(INIT_TRANSACTION_TEMP_PATH, &options)
        .context("create project initialization recovery journal")?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    root.hard_link(INIT_TRANSACTION_TEMP_PATH, root, INIT_TRANSACTION_PATH)
        .context("atomically publish project initialization recovery journal")?;
    sync_capability_directory(root, None)?;
    root.remove_file(INIT_TRANSACTION_TEMP_PATH)
        .context("remove project initialization recovery journal temporary file")?;
    sync_capability_directory(root, None)?;
    Ok(())
}

fn collect_missing_parent_directories(
    root: &cap_std::fs::Dir,
    path: &Path,
    directories: &mut BTreeSet<std::path::PathBuf>,
) -> Result<()> {
    let mut ancestors = path
        .ancestors()
        .filter(|ancestor| !ancestor.as_os_str().is_empty())
        .collect::<Vec<_>>();
    ancestors.reverse();
    for ancestor in ancestors {
        match root.symlink_metadata(ancestor) {
            Ok(metadata) => {
                if !metadata.is_dir() || metadata.file_type().is_symlink() {
                    bail!(
                        "template parent is not a real directory: {}",
                        ancestor.display()
                    );
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                directories.insert(ancestor.to_path_buf());
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn create_template_parent_directories(
    root: &cap_std::fs::Dir,
    path: &Path,
    created_directories: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    let mut ancestors = path
        .ancestors()
        .filter(|ancestor| !ancestor.as_os_str().is_empty())
        .collect::<Vec<_>>();
    ancestors.reverse();
    for ancestor in ancestors {
        match root.create_dir(ancestor) {
            Ok(()) => {
                created_directories.insert(ancestor.to_path_buf());
                sync_capability_file_parent(root, ancestor)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let metadata = root.symlink_metadata(ancestor)?;
                if !metadata.is_dir() || metadata.file_type().is_symlink() {
                    bail!(
                        "template parent is not a real directory: {}",
                        ancestor.display()
                    );
                }
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("create template directory {}", ancestor.display()));
            }
        }
    }
    Ok(())
}

fn rollback_template_transaction(
    root: &cap_std::fs::Dir,
    files: &[std::path::PathBuf],
    temporary_files: &[std::path::PathBuf],
    directories: &BTreeSet<std::path::PathBuf>,
) -> Vec<String> {
    let mut errors = Vec::new();
    for path in files.iter().rev() {
        if let Err(error) = root.remove_file(path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            errors.push(format!("remove {}: {error}", path.display()));
        }
    }
    for path in temporary_files.iter().rev() {
        if let Err(error) = root.remove_file(path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            errors.push(format!("remove temporary {}: {error}", path.display()));
        }
    }
    let mut directories = directories.iter().collect::<Vec<_>>();
    directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for path in directories {
        if let Err(error) = root.remove_dir(path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            errors.push(format!("remove directory {}: {error}", path.display()));
        }
    }
    errors
}

fn sync_capability_directory(root: &cap_std::fs::Dir, parent: Option<&Path>) -> Result<()> {
    #[cfg(unix)]
    {
        // `cap_std::fs::Dir` may hold an `O_PATH` descriptor on Linux. Such a
        // descriptor is capability-safe but `fsync` returns EBADF, so reopen
        // the already-bounded directory read-only before asking the kernel to
        // persist its entries.
        let path = parent.unwrap_or_else(|| Path::new("."));
        let mut options = cap_std::fs::OpenOptions::new();
        options.read(true);
        root.open_with(path, &options)?.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = (root, parent);
    Ok(())
}

fn sync_capability_file_parent(root: &cap_std::fs::Dir, path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    if let Some(parent) = parent {
        match root.symlink_metadata(parent) {
            Ok(metadata) => {
                if !metadata.is_dir() || metadata.file_type().is_symlink() {
                    bail!(
                        "project transaction parent is not a real directory: {}",
                        parent.display()
                    );
                }
                return sync_capability_directory(root, Some(parent));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    sync_capability_directory(root, None)
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
            let manifest: studio_core::ReleaseManifestV2 = serde_json::from_slice(
                &fs::read(temporary.path().join("reporch.problem.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(manifest.problem_type, problem_type);
            manifest.validate_references().unwrap();
            let authoring = crate::local_project_v2::read_authoring_spec(temporary.path()).unwrap();
            assert_eq!(authoring.problem_type, problem_type);
            assert_eq!(authoring.project_id, manifest.project_id);
            assert!(
                init_project_template(temporary.path(), "Again", Uuid::now_v7(), problem_type)
                    .is_err()
            );
        }
    }

    #[test]
    fn interrupted_init_without_finals_rolls_back_only_staged_files() {
        let temporary = tempfile::tempdir().unwrap();
        let files = vec![
            TemplateFile::text("statements/ko.md", "statement", "text/markdown"),
            TemplateFile::text("tests/1.in", "1 2\n", "text/plain"),
        ];
        let root =
            cap_std::fs::Dir::open_ambient_dir(temporary.path(), ambient_authority()).unwrap();
        let journal = build_init_journal(&root, &files).unwrap();
        write_init_journal(&root, &journal).unwrap();
        stage_test_transaction_file(&root, &files[0], &journal.files[0]);
        drop(root);

        assert_eq!(
            recover_test_transaction(temporary.path(), &files, None).unwrap(),
            InitRecoveryOutcome::RolledBack
        );
        assert!(!temporary.path().join("statements/ko.md").exists());
        assert!(temporary.path().join("statements").exists());
        assert!(!temporary.path().join(INIT_TRANSACTION_PATH).exists());
        assert!(
            !temporary
                .path()
                .join(&journal.files[0].temporary_path)
                .exists()
        );
    }

    #[test]
    fn transaction_journal_paths_are_portable_forward_slash_paths() {
        let temporary = tempfile::tempdir().unwrap();
        let files = vec![TemplateFile::text(
            "grader/private/assets/grader.cpp",
            "int main() {}",
            "text/x-c++src",
        )];
        let root =
            cap_std::fs::Dir::open_ambient_dir(temporary.path(), ambient_authority()).unwrap();
        let journal = build_init_journal(&root, &files).unwrap();

        assert_eq!(journal.files[0].path, "grader/private/assets/grader.cpp");
        assert!(
            journal.files[0]
                .temporary_path
                .starts_with("grader/private/assets/.reporch-init-")
        );
        assert!(
            journal
                .files
                .iter()
                .all(|entry| !entry.temporary_path.contains('\\'))
        );
        assert!(journal.directories.iter().all(|path| !path.contains('\\')));
    }

    #[test]
    fn interrupted_init_finishes_a_partial_commit_without_deleting_finals() {
        let temporary = tempfile::tempdir().unwrap();
        let files = vec![
            TemplateFile::text("statements/ko.md", "statement", "text/markdown"),
            TemplateFile::text("tests/1.in", "1 2\n", "text/plain"),
        ];
        let root =
            cap_std::fs::Dir::open_ambient_dir(temporary.path(), ambient_authority()).unwrap();
        let journal = build_init_journal(&root, &files).unwrap();
        write_init_journal(&root, &journal).unwrap();
        for (template, entry) in files.iter().zip(&journal.files) {
            stage_test_transaction_file(&root, template, entry);
        }
        publish_test_transaction_final(&root, &journal.files[0]);
        drop(root);

        assert_eq!(
            recover_test_transaction(temporary.path(), &files, None).unwrap(),
            InitRecoveryOutcome::Committed
        );
        for template in &files {
            assert_eq!(
                fs::read(temporary.path().join(template.path)).unwrap(),
                template.content
            );
        }
        assert!(!temporary.path().join(INIT_TRANSACTION_PATH).exists());
        assert!(
            journal
                .files
                .iter()
                .all(|entry| !temporary.path().join(&entry.temporary_path).exists())
        );
    }

    #[test]
    fn interrupted_init_never_removes_a_changed_file() {
        let temporary = tempfile::tempdir().unwrap();
        let files = vec![
            TemplateFile::text("statements/ko.md", "statement", "text/markdown"),
            TemplateFile::text("tests/1.in", "1 2\n", "text/plain"),
        ];
        let root =
            cap_std::fs::Dir::open_ambient_dir(temporary.path(), ambient_authority()).unwrap();
        let journal = build_init_journal(&root, &files).unwrap();
        write_init_journal(&root, &journal).unwrap();
        for (template, entry) in files.iter().zip(&journal.files) {
            stage_test_transaction_file(&root, template, entry);
        }
        publish_test_transaction_final(&root, &journal.files[0]);
        drop(root);
        fs::remove_file(temporary.path().join(&journal.files[0].temporary_path)).unwrap();
        fs::write(
            temporary.path().join(&journal.files[0].path),
            "user changed this",
        )
        .unwrap();

        let error = recover_test_transaction(temporary.path(), &files, None).unwrap_err();
        assert!(error.to_string().contains("changed file"));
        assert_eq!(
            fs::read_to_string(temporary.path().join(&journal.files[0].path)).unwrap(),
            "user changed this"
        );
        assert!(temporary.path().join(INIT_TRANSACTION_PATH).exists());
    }

    #[test]
    fn forged_journal_never_authorizes_deleting_an_unrelated_file() {
        let temporary = tempfile::tempdir().unwrap();
        let files = vec![
            TemplateFile::text("statements/ko.md", "statement", "text/markdown"),
            TemplateFile::text("tests/1.in", "1 2\n", "text/plain"),
        ];
        let root =
            cap_std::fs::Dir::open_ambient_dir(temporary.path(), ambient_authority()).unwrap();
        let mut journal = build_init_journal(&root, &files).unwrap();
        fs::write(temporary.path().join("important.txt"), "preserve me").unwrap();
        journal.files[0].path = "important.txt".into();
        journal.files[0].temporary_path =
            format!(".reporch-init-{}-0.tmp", journal.transaction_id.simple());
        journal.files[0].sha256 = studio_core::Sha256Digest::from_bytes(b"preserve me");
        journal.files[0].size_bytes = 11;
        journal.directories.retain(|path| path != "statements");
        write_init_journal(&root, &journal).unwrap();
        root.hard_link("important.txt", &root, &journal.files[0].temporary_path)
            .unwrap();
        drop(root);

        let error = recover_test_transaction(temporary.path(), &files, None).unwrap_err();
        assert!(error.to_string().contains("does not match this template"));
        assert_eq!(
            fs::read_to_string(temporary.path().join("important.txt")).unwrap(),
            "preserve me"
        );
    }

    #[test]
    fn project_init_lock_rejects_concurrent_initialization() {
        let temporary = tempfile::tempdir().unwrap();
        let _first = ProjectInitLock::acquire(temporary.path()).unwrap();
        let error = ProjectInitLock::acquire(temporary.path()).unwrap_err();
        assert!(error.to_string().contains("already running"));
    }

    #[cfg(unix)]
    #[test]
    fn project_init_lock_root_rejects_a_preexisting_symlink_without_chmod() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let home = tempfile::tempdir().unwrap();
        fs::set_permissions(home.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::set_permissions(outside.path(), fs::Permissions::from_mode(0o755)).unwrap();
        symlink(
            outside.path(),
            home.path().join(format!(".{INIT_LOCK_ROOT_NAME}")),
        )
        .unwrap();

        let error = secure_unix_lock_root(home.path()).unwrap_err();
        assert!(error.to_string().contains("real 0700 user-owned directory"));
        assert_eq!(
            fs::metadata(outside.path()).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[cfg(unix)]
    #[test]
    fn opened_project_capability_never_follows_a_later_root_symlink_swap() {
        use std::os::unix::fs::symlink;

        let parent = tempfile::tempdir().unwrap();
        let project = parent.path().join("project");
        let moved = parent.path().join("project-moved");
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir(&project).unwrap();
        let root = open_project_directory_capability(&project).unwrap();
        fs::rename(&project, &moved).unwrap();
        symlink(outside.path(), &project).unwrap();

        let files = vec![TemplateFile::text(
            "statements/ko.md",
            "capability-bound",
            "text/markdown",
        )];
        write_template_transaction(&root, &files).unwrap();
        assert_eq!(
            fs::read_to_string(moved.join("statements/ko.md")).unwrap(),
            "capability-bound"
        );
        assert!(!outside.path().join("statements/ko.md").exists());
    }

    #[test]
    fn interrupted_legacy_v1_init_rolls_forward_and_validates_documents() {
        let temporary = tempfile::tempdir().unwrap();
        init_legacy_v1_project_template(
            temporary.path(),
            "Legacy recovery",
            Uuid::now_v7(),
            ProblemType::Standard,
        )
        .unwrap();
        let (files, journal) = prepare_interrupted_standard_transaction(temporary.path());

        assert_eq!(
            recover_test_transaction(temporary.path(), &files, None).unwrap(),
            InitRecoveryOutcome::Committed
        );
        let manifest: studio_core::VersionedReleaseManifest = serde_json::from_slice(
            &fs::read(temporary.path().join("reporch.problem.json")).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            manifest,
            studio_core::VersionedReleaseManifest::V1(_)
        ));
        assert!(!temporary.path().join(INIT_TRANSACTION_PATH).exists());
        assert!(
            journal
                .files
                .iter()
                .all(|entry| !temporary.path().join(&entry.temporary_path).exists())
        );
    }

    #[test]
    fn interrupted_init_rejects_an_explicit_project_id_mismatch() {
        let temporary = tempfile::tempdir().unwrap();
        let original_project_id = Uuid::now_v7();
        init_project_template(
            temporary.path(),
            "Identity recovery",
            original_project_id,
            ProblemType::Standard,
        )
        .unwrap();
        let (files, _) = prepare_interrupted_standard_transaction(temporary.path());
        let requested_project_id = Uuid::now_v7();

        let error = recover_test_transaction(temporary.path(), &files, Some(requested_project_id))
            .unwrap_err();
        assert!(error.to_string().contains("explicitly requested project"));
        assert!(temporary.path().join("statements/ko.md").exists());
        assert!(temporary.path().join(INIT_TRANSACTION_PATH).exists());
    }

    #[test]
    fn recovered_zero_final_transaction_never_grants_non_empty_opt_in() {
        let temporary = tempfile::tempdir().unwrap();
        init_project_template(
            temporary.path(),
            "No implicit opt in",
            Uuid::now_v7(),
            ProblemType::Standard,
        )
        .unwrap();
        let (files, journal) = prepare_interrupted_standard_transaction(temporary.path());
        // Model a crash before the first final publish while keeping valid staged data.
        fs::remove_file(temporary.path().join(&journal.files[0].path)).unwrap();
        fs::write(temporary.path().join("notes.txt"), "unrelated").unwrap();

        let error = init_project_template_with_optional_id(
            temporary.path(),
            "No implicit opt in",
            None,
            ProblemType::Standard,
            false,
        )
        .unwrap_err();
        assert!(error.to_string().contains("--allow-non-empty"));
        assert_eq!(
            fs::read_to_string(temporary.path().join("notes.txt")).unwrap(),
            "unrelated"
        );
        assert!(!temporary.path().join("reporch.yaml").exists());
        assert_eq!(files.len(), journal.files.len());
    }

    fn prepare_interrupted_standard_transaction(
        directory: &Path,
    ) -> (Vec<TemplateFile>, InitTransactionJournal) {
        let files = [
            ("statements/ko.md", "text/markdown"),
            ("tests/1.in", "text/plain"),
            ("tests/1.ans", "text/plain"),
            ("solutions/accepted.py", "text/x-python"),
            ("solutions/accepted-alt.py", "text/x-python"),
            ("solutions/wrong.py", "text/x-python"),
            ("reporch.problem.json", "application/json"),
            ("reporch.yaml", "application/yaml"),
        ]
        .into_iter()
        .map(|(path, media_type)| {
            TemplateFile::text(path, fs::read(directory.join(path)).unwrap(), media_type)
        })
        .collect::<Vec<_>>();
        for file in &files {
            fs::remove_file(directory.join(file.path)).unwrap();
        }
        let root = cap_std::fs::Dir::open_ambient_dir(directory, ambient_authority()).unwrap();
        let journal = build_init_journal(&root, &files).unwrap();
        write_init_journal(&root, &journal).unwrap();
        for (template, entry) in files.iter().zip(&journal.files) {
            stage_test_transaction_file(&root, template, entry);
        }
        publish_test_transaction_final(&root, &journal.files[0]);
        (files, journal)
    }

    fn recover_test_transaction(
        directory: &Path,
        files: &[TemplateFile],
        required_project_id: Option<Uuid>,
    ) -> Result<InitRecoveryOutcome> {
        let root = open_project_directory_capability(directory)?;
        recover_interrupted_template_transaction(&root, files, required_project_id)
    }

    fn stage_test_transaction_file(
        root: &cap_std::fs::Dir,
        template: &TemplateFile,
        entry: &InitTransactionFile,
    ) {
        let path = Path::new(&entry.path);
        if let Some(parent) = path.parent() {
            root.create_dir_all(parent).unwrap();
        }
        let mut file = root.create(&entry.temporary_path).unwrap();
        file.write_all(&template.content).unwrap();
        file.sync_all().unwrap();
    }

    fn publish_test_transaction_final(root: &cap_std::fs::Dir, entry: &InitTransactionFile) {
        root.hard_link(&entry.temporary_path, root, &entry.path)
            .unwrap();
    }
}
