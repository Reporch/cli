use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{ProblemType, SubjectRef};

pub const TEST_LAB_SCHEMA_V1: &str = "reporch.test-lab.v1";
pub const MAX_TEST_CASES: usize = 10_000;
pub const MAX_TEST_GROUPS: usize = 256;
pub const MAX_GENERATORS: usize = 128;
pub const MAX_INLINE_TEST_BYTES: usize = 512 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct TestLabV1 {
    pub schema: String,
    pub project_id: Uuid,
    pub revision: i64,
    pub draft: TestLabDraftV1,
    pub issues: Vec<TestLabIssueV1>,
    pub updated_by: SubjectRef,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct TestLabDraftV1 {
    pub settings: TestLabSettingsV1,
    pub groups: Vec<TestGroupDraftV1>,
    pub test_cases: Vec<TestCaseDraftV1>,
    pub generators: Vec<GeneratorDraftV1>,
    pub validator: Option<TestProgramDraftV1>,
    #[serde(default)]
    pub extra_validators: Vec<TestProgramDraftV1>,
    pub validator_tests: Vec<ValidatorUnitTestDraftV1>,
    pub checker: TestLabCheckerV1,
    pub checker_tests: Vec<CheckerUnitTestDraftV1>,
    pub solution_expectations: Vec<SolutionExpectationDraftV1>,
}

impl TestLabDraftV1 {
    pub fn initial(problem_type: ProblemType) -> Self {
        let sample_id = Uuid::now_v7();
        let mut groups = Vec::new();
        let mut group_ids = Vec::new();
        if problem_type == ProblemType::Scored {
            let group_id = Uuid::now_v7();
            groups.push(TestGroupDraftV1 {
                id: group_id,
                name: "기본 서브태스크".into(),
                points: 100.0,
                depends_on: Vec::new(),
                feedback_policy: GroupFeedbackPolicyV1::Complete,
            });
            group_ids.push(group_id);
        }
        Self {
            settings: TestLabSettingsV1 {
                time_limit_ms: 1_000,
                memory_limit_mib: 256,
                output_limit_kib: 64 * 1_024,
                answer_source: AnswerSourceV1::AcceptedSolution,
                detect_duplicates: true,
                verify_determinism: true,
            },
            groups,
            test_cases: vec![TestCaseDraftV1 {
                id: sample_id,
                name: "sample-1".into(),
                role: TestCaseRoleV1::Sample,
                origin: TestCaseOriginV1::Manual,
                input_path: "tests/sample/1.in".into(),
                answer_path: Some("tests/sample/1.ans".into()),
                input: "1 2\n".into(),
                answer: Some("3\n".into()),
                group_ids,
                points: None,
                generator_id: None,
                generator_arguments: Vec::new(),
                seed: None,
                materialization: TestMaterializationV1::Materialized,
            }],
            generators: Vec::new(),
            validator: None,
            extra_validators: Vec::new(),
            validator_tests: Vec::new(),
            checker: TestLabCheckerV1::Token,
            checker_tests: Vec::new(),
            solution_expectations: vec![
                SolutionExpectationDraftV1 {
                    id: Uuid::now_v7(),
                    name: "Accepted reference".into(),
                    source_path: "solutions/accepted.py".into(),
                    language: "python3".into(),
                    expected_verdict: SolutionExpectedVerdictV1::Accepted,
                    expected_group_ids: Vec::new(),
                    min_score: None,
                    max_score: None,
                },
                SolutionExpectationDraftV1 {
                    id: Uuid::now_v7(),
                    name: "Accepted cross-check".into(),
                    source_path: "solutions/accepted-alt.py".into(),
                    language: "python3".into(),
                    expected_verdict: SolutionExpectedVerdictV1::Accepted,
                    expected_group_ids: Vec::new(),
                    min_score: None,
                    max_score: None,
                },
                SolutionExpectationDraftV1 {
                    id: Uuid::now_v7(),
                    name: "Known wrong".into(),
                    source_path: "solutions/wrong.py".into(),
                    language: "python3".into(),
                    expected_verdict: SolutionExpectedVerdictV1::WrongAnswer,
                    expected_group_ids: Vec::new(),
                    min_score: None,
                    max_score: None,
                },
            ],
        }
    }

    pub fn analyze(&self, problem_type: ProblemType) -> Vec<TestLabIssueV1> {
        let mut issues = Vec::new();
        if self.test_cases.len() > MAX_TEST_CASES {
            issues.push(error(
                "test_lab.too_many_tests",
                format!("테스트는 최대 {MAX_TEST_CASES}개까지 저장할 수 있습니다."),
                "test_cases",
            ));
        }
        if self.groups.len() > MAX_TEST_GROUPS {
            issues.push(error(
                "test_lab.too_many_groups",
                format!("그룹은 최대 {MAX_TEST_GROUPS}개까지 저장할 수 있습니다."),
                "groups",
            ));
        }
        if self.generators.len() > MAX_GENERATORS {
            issues.push(error(
                "test_lab.too_many_generators",
                format!("Generator는 최대 {MAX_GENERATORS}개까지 저장할 수 있습니다."),
                "generators",
            ));
        }
        if self.settings.time_limit_ms == 0
            || self.settings.memory_limit_mib == 0
            || self.settings.output_limit_kib == 0
        {
            issues.push(error(
                "test_lab.invalid_limits",
                "시간·메모리·출력 제한은 0보다 커야 합니다.",
                "settings",
            ));
        }

        let group_ids = collect_unique_ids(
            self.groups.iter().map(|group| group.id),
            "test_lab.duplicate_group_id",
            "groups",
            &mut issues,
        );
        let generator_ids = collect_unique_ids(
            self.generators.iter().map(|generator| generator.id),
            "test_lab.duplicate_generator_id",
            "generators",
            &mut issues,
        );
        collect_unique_ids(
            self.test_cases.iter().map(|test| test.id),
            "test_lab.duplicate_test_id",
            "test_cases",
            &mut issues,
        );

        let dependencies: BTreeMap<_, _> = self
            .groups
            .iter()
            .map(|group| (group.id, group.depends_on.clone()))
            .collect();
        let mut group_names = BTreeSet::new();
        for (index, group) in self.groups.iter().enumerate() {
            if group.name.trim().is_empty() || !group_names.insert(group.name.trim().to_owned()) {
                issues.push(error(
                    "test_lab.duplicate_or_invalid_group_name",
                    "그룹 이름은 비어 있지 않고 서로 달라야 합니다.",
                    format!("groups[{index}].name"),
                ));
            }
            if !group.points.is_finite() || group.points < 0.0 {
                issues.push(error(
                    "test_lab.invalid_group_points",
                    "그룹 점수는 0 이상의 유한한 값이어야 합니다.",
                    format!("groups[{index}].points"),
                ));
            }
            for dependency in &group.depends_on {
                if !group_ids.contains(dependency) || dependency == &group.id {
                    issues.push(error(
                        "test_lab.invalid_group_dependency",
                        "그룹 의존성이 존재하지 않는 그룹 또는 자기 자신을 가리킵니다.",
                        format!("groups[{index}].depends_on"),
                    ));
                }
            }
        }
        if has_dependency_cycle(&dependencies) {
            issues.push(error(
                "test_lab.group_dependency_cycle",
                "서브태스크 의존성에 순환이 있습니다.",
                "groups",
            ));
        }
        if problem_type == ProblemType::Scored {
            let total: f64 = self.groups.iter().map(|group| group.points).sum();
            if !total.is_finite() || (total - 100.0).abs() > 1e-6 {
                issues.push(error(
                    "test_lab.group_points_total",
                    format!(
                        "부분 점수 문제의 그룹 점수 합은 100이어야 합니다. 현재 {total}점입니다."
                    ),
                    "groups",
                ));
            }
        }

        let mut paths = BTreeSet::new();
        let mut test_names = BTreeSet::new();
        let mut used_groups = BTreeSet::new();
        let mut exact_inputs = BTreeMap::new();
        let mut normalized_inputs = BTreeMap::new();
        let mut pending_generated = 0_usize;
        for (index, test) in self.test_cases.iter().enumerate() {
            if test.name.trim().is_empty() || !test_names.insert(test.name.trim().to_owned()) {
                issues.push(error(
                    "test_lab.duplicate_or_invalid_test_name",
                    "테스트 이름은 비어 있지 않고 서로 달라야 합니다.",
                    format!("test_cases[{index}].name"),
                ));
            }
            validate_path(
                &test.input_path,
                format!("test_cases[{index}].input_path"),
                &mut issues,
            );
            if let Some(path) = &test.answer_path {
                validate_path(
                    path,
                    format!("test_cases[{index}].answer_path"),
                    &mut issues,
                );
            }
            for path in [&test.input_path]
                .into_iter()
                .chain(test.answer_path.iter())
            {
                if !paths.insert(path.clone()) {
                    issues.push(error(
                        "test_lab.duplicate_file_path",
                        "여러 테스트가 같은 파일 경로를 사용합니다.",
                        format!("test_cases[{index}]"),
                    ));
                }
            }
            if test.input.len() > MAX_INLINE_TEST_BYTES
                || test
                    .answer
                    .as_ref()
                    .is_some_and(|answer| answer.len() > MAX_INLINE_TEST_BYTES)
            {
                issues.push(error(
                    "test_lab.inline_content_too_large",
                    "브라우저 편집 본문은 파일당 512 KiB까지입니다. 더 큰 테스트는 파일 업로드를 사용하세요.",
                    format!("test_cases[{index}]"),
                ));
            }
            for group_id in &test.group_ids {
                used_groups.insert(*group_id);
                if !group_ids.contains(group_id) {
                    issues.push(error(
                        "test_lab.unknown_test_group",
                        "테스트가 존재하지 않는 그룹을 참조합니다.",
                        format!("test_cases[{index}].group_ids"),
                    ));
                }
            }
            if let Some(points) = test.points
                && (!points.is_finite() || points < 0.0)
            {
                issues.push(error(
                    "test_lab.invalid_test_points",
                    "테스트 점수는 0 이상의 유한한 값이어야 합니다.",
                    format!("test_cases[{index}].points"),
                ));
            }
            match test.origin {
                TestCaseOriginV1::Generated | TestCaseOriginV1::Stress => {
                    if test.generator_id.is_none() || test.seed.is_none() {
                        issues.push(error(
                            "test_lab.generated_recipe_incomplete",
                            "생성 테스트에는 Generator와 seed가 필요합니다.",
                            format!("test_cases[{index}]"),
                        ));
                    } else if test
                        .generator_id
                        .is_some_and(|id| !generator_ids.contains(&id))
                    {
                        issues.push(error(
                            "test_lab.unknown_generator",
                            "생성 테스트가 존재하지 않는 Generator를 참조합니다.",
                            format!("test_cases[{index}].generator_id"),
                        ));
                    }
                    if test.materialization != TestMaterializationV1::Materialized {
                        pending_generated += 1;
                    }
                }
                _ if test.generator_id.is_some() || test.seed.is_some() => issues.push(warning(
                    "test_lab.unused_generator_recipe",
                    "수동 테스트에 연결된 Generator/seed 정보는 실행에 사용되지 않습니다.",
                    format!("test_cases[{index}]"),
                )),
                _ => {}
            }
            let generation_pending = matches!(
                test.origin,
                TestCaseOriginV1::Generated | TestCaseOriginV1::Stress
            ) && test.materialization
                != TestMaterializationV1::Materialized;
            if test.role == TestCaseRoleV1::Secret
                && test.input.trim().is_empty()
                && !generation_pending
            {
                issues.push(warning(
                    "test_lab.secret_test_empty",
                    "비밀 테스트 입력이 비어 있습니다.",
                    format!("test_cases[{index}].input"),
                ));
            }
            let requires_answer = matches!(
                problem_type,
                ProblemType::Standard | ProblemType::Scored | ProblemType::OutputOnly
            );
            if requires_answer
                && !generation_pending
                && (test.answer_path.is_none() || test.answer.is_none())
            {
                issues.push(error(
                    "test_lab.answer_required",
                    "이 문제 유형의 테스트에는 공식 정답 파일과 내용이 필요합니다.",
                    format!("test_cases[{index}].answer"),
                ));
            }
            if self.settings.detect_duplicates && !test.input.is_empty() {
                let exact = crate::Sha256Digest::from_bytes(test.input.as_bytes()).to_string();
                if let Some(first) = exact_inputs.insert(exact, index) {
                    issues.push(warning(
                        "test_lab.duplicate_input",
                        format!("테스트 입력이 test_cases[{first}]와 완전히 같습니다."),
                        format!("test_cases[{index}].input"),
                    ));
                } else {
                    let normalized = test.input.split_whitespace().collect::<Vec<_>>().join(" ");
                    let digest = crate::Sha256Digest::from_bytes(normalized.as_bytes()).to_string();
                    if let Some(first) = normalized_inputs.insert(digest, index) {
                        issues.push(warning(
                            "test_lab.similar_input",
                            format!("공백을 정규화하면 test_cases[{first}]와 같은 입력입니다."),
                            format!("test_cases[{index}].input"),
                        ));
                    }
                }
            }
        }
        for (index, group) in self.groups.iter().enumerate() {
            if !used_groups.contains(&group.id) {
                issues.push(warning(
                    "test_lab.group_without_tests",
                    "이 그룹에 포함된 테스트가 없습니다.",
                    format!("groups[{index}]"),
                ));
            }
        }
        if pending_generated > 0 {
            issues.push(warning(
                "test_lab.generated_tests_pending",
                format!(
                    "생성 또는 재생성이 필요한 테스트가 {pending_generated}개 있습니다. 검증 시 generator 출력과 고정된 입력을 비교합니다."
                ),
                "test_cases",
            ));
        }

        let mut generator_names = BTreeSet::new();
        for (index, generator) in self.generators.iter().enumerate() {
            validate_path(
                &generator.source_path,
                format!("generators[{index}].source_path"),
                &mut issues,
            );
            if generator.name.trim().is_empty()
                || !generator_names.insert(generator.name.trim().to_owned())
                || generator.language.trim().is_empty()
                || generator.source.trim().is_empty()
            {
                issues.push(error(
                    "test_lab.generator_program_incomplete",
                    "Generator 이름은 고유해야 하며 언어와 소스가 필요합니다.",
                    format!("generators[{index}]"),
                ));
            }
            for (recipe_index, recipe) in generator.recipes.iter().enumerate() {
                if recipe.count == 0 || recipe.count > 10_000 {
                    issues.push(error(
                        "test_lab.invalid_generation_count",
                        "생성 개수는 1~10,000이어야 합니다.",
                        format!("generators[{index}].recipes[{recipe_index}].count"),
                    ));
                }
                for group_id in &recipe.group_ids {
                    if !group_ids.contains(group_id) {
                        issues.push(error(
                            "test_lab.unknown_recipe_group",
                            "생성 recipe가 존재하지 않는 그룹을 참조합니다.",
                            format!("generators[{index}].recipes[{recipe_index}].group_ids"),
                        ));
                    }
                }
            }
        }

        if let Some(validator) = &self.validator {
            validate_path(&validator.source_path, "validator.source_path", &mut issues);
            if validator.language.trim().is_empty() || validator.source.trim().is_empty() {
                issues.push(error(
                    "test_lab.validator_program_incomplete",
                    "Validator 언어와 소스가 필요합니다.",
                    "validator",
                ));
            }
            if self.validator_tests.is_empty() {
                issues.push(warning(
                    "test_lab.validator_tests_missing",
                    "Validator에는 valid/invalid 단위 테스트를 각각 추가하는 것이 안전합니다.",
                    "validator_tests",
                ));
            } else {
                let has_valid = self.validator_tests.iter().any(|test| test.expected_valid);
                let has_invalid = self.validator_tests.iter().any(|test| !test.expected_valid);
                if !has_valid || !has_invalid {
                    issues.push(warning(
                        "test_lab.validator_test_polarity_missing",
                        "Validator 단위 테스트에는 통과 입력과 거부 입력이 모두 있어야 합니다.",
                        "validator_tests",
                    ));
                }
            }
        }
        for (index, validator) in self.extra_validators.iter().enumerate() {
            validate_path(
                &validator.source_path,
                format!("extra_validators[{index}].source_path"),
                &mut issues,
            );
            if validator.language.trim().is_empty() || validator.source.trim().is_empty() {
                issues.push(error(
                    "test_lab.extra_validator_program_incomplete",
                    "Extra validator 언어와 소스가 필요합니다.",
                    format!("extra_validators[{index}]"),
                ));
            }
        }
        if matches!(self.checker, TestLabCheckerV1::Custom { .. }) {
            if let TestLabCheckerV1::Custom {
                source_path,
                language,
                source,
                ..
            } = &self.checker
            {
                validate_path(source_path, "checker.source_path", &mut issues);
                if language.trim().is_empty() || source.trim().is_empty() {
                    issues.push(error(
                        "test_lab.checker_program_incomplete",
                        "Custom checker 언어와 소스가 필요합니다.",
                        "checker",
                    ));
                }
            }
            if self.checker_tests.is_empty() {
                issues.push(warning(
                    "test_lab.checker_tests_missing",
                    "Custom checker에는 accept/reject 단위 테스트가 필요합니다.",
                    "checker_tests",
                ));
            } else {
                let has_accept = self.checker_tests.iter().any(|test| test.expected_accepted);
                let has_reject = self
                    .checker_tests
                    .iter()
                    .any(|test| !test.expected_accepted);
                if !has_accept || !has_reject {
                    issues.push(warning(
                        "test_lab.checker_test_polarity_missing",
                        "Checker 단위 테스트에는 승인 출력과 거부 출력이 모두 있어야 합니다.",
                        "checker_tests",
                    ));
                }
            }
        }
        if let TestLabCheckerV1::Floating {
            absolute_tolerance,
            relative_tolerance,
        } = &self.checker
            && ((!absolute_tolerance.is_finite() || *absolute_tolerance < 0.0)
                || (!relative_tolerance.is_finite() || *relative_tolerance < 0.0)
                || (*absolute_tolerance == 0.0 && *relative_tolerance == 0.0))
        {
            issues.push(error(
                "test_lab.invalid_float_tolerance",
                "Floating checker의 절대/상대 오차 중 하나는 0보다 커야 합니다.",
                "checker",
            ));
        }

        if !self
            .solution_expectations
            .iter()
            .any(|solution| solution.expected_verdict == SolutionExpectedVerdictV1::Accepted)
            && problem_type != ProblemType::OutputOnly
        {
            issues.push(warning(
                "test_lab.accepted_solution_missing",
                "정답 생성과 교차 검증에 사용할 Accepted reference가 필요합니다.",
                "solution_expectations",
            ));
        }
        if !self
            .solution_expectations
            .iter()
            .any(|solution| solution.expected_verdict != SolutionExpectedVerdictV1::Accepted)
        {
            issues.push(warning(
                "test_lab.negative_solution_missing",
                "테스트 강도를 확인하려면 Known Wrong/TLE/Partial solution을 추가하세요.",
                "solution_expectations",
            ));
        }
        issues
    }

    pub fn ensure_storable(
        &self,
        problem_type: ProblemType,
    ) -> Result<Vec<TestLabIssueV1>, TestLabError> {
        let issues = self.analyze(problem_type);
        let exceeds_storage_limits = self.test_cases.len() > MAX_TEST_CASES
            || self.groups.len() > MAX_TEST_GROUPS
            || self.generators.len() > MAX_GENERATORS
            || self.test_cases.iter().any(|test| {
                test.input.len() > MAX_INLINE_TEST_BYTES
                    || test
                        .answer
                        .as_ref()
                        .is_some_and(|answer| answer.len() > MAX_INLINE_TEST_BYTES)
            });
        if exceeds_storage_limits {
            return Err(TestLabError::Invalid(issues));
        }
        Ok(issues)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct TestLabSettingsV1 {
    pub time_limit_ms: u64,
    pub memory_limit_mib: u64,
    pub output_limit_kib: u64,
    pub answer_source: AnswerSourceV1,
    pub detect_duplicates: bool,
    pub verify_determinism: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AnswerSourceV1 {
    AcceptedSolution,
    Manual,
    None,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct TestGroupDraftV1 {
    pub id: Uuid,
    pub name: String,
    pub points: f64,
    pub depends_on: Vec<Uuid>,
    pub feedback_policy: GroupFeedbackPolicyV1,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum GroupFeedbackPolicyV1 {
    #[default]
    Complete,
    FirstError,
    NoFeedback,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct TestCaseDraftV1 {
    pub id: Uuid,
    pub name: String,
    pub role: TestCaseRoleV1,
    pub origin: TestCaseOriginV1,
    pub input_path: String,
    pub answer_path: Option<String>,
    pub input: String,
    pub answer: Option<String>,
    pub group_ids: Vec<Uuid>,
    pub points: Option<f64>,
    pub generator_id: Option<Uuid>,
    pub generator_arguments: Vec<String>,
    pub seed: Option<u64>,
    pub materialization: TestMaterializationV1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TestCaseRoleV1 {
    Sample,
    Secret,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TestCaseOriginV1 {
    Manual,
    Generated,
    Copied,
    Uploaded,
    Stress,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TestMaterializationV1 {
    Pending,
    Materialized,
    Stale,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct GeneratorDraftV1 {
    pub id: Uuid,
    pub name: String,
    pub source_path: String,
    pub language: String,
    pub source: String,
    pub arguments: Vec<String>,
    pub recipes: Vec<GeneratorRecipeDraftV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct GeneratorRecipeDraftV1 {
    pub id: Uuid,
    pub name: String,
    pub arguments: Vec<String>,
    pub seed_start: u64,
    pub count: u32,
    pub group_ids: Vec<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct TestProgramDraftV1 {
    pub source_path: String,
    pub language: String,
    pub source: String,
    pub arguments: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ValidatorUnitTestDraftV1 {
    pub id: Uuid,
    pub name: String,
    pub input: String,
    pub expected_valid: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TestLabCheckerV1 {
    Exact,
    Token,
    CaseInsensitive,
    Floating {
        absolute_tolerance: f64,
        relative_tolerance: f64,
    },
    Custom {
        source_path: String,
        language: String,
        source: String,
        arguments: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckerUnitTestDraftV1 {
    pub id: Uuid,
    pub name: String,
    pub input: String,
    pub answer: String,
    pub output: String,
    pub expected_accepted: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SolutionExpectationDraftV1 {
    pub id: Uuid,
    pub name: String,
    pub source_path: String,
    pub language: String,
    pub expected_verdict: SolutionExpectedVerdictV1,
    pub expected_group_ids: Vec<Uuid>,
    pub min_score: Option<f64>,
    pub max_score: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SolutionExpectedVerdictV1 {
    Accepted,
    WrongAnswer,
    TimeLimit,
    MemoryLimit,
    RuntimeError,
    Partial,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct TestLabIssueV1 {
    pub code: String,
    pub severity: TestLabIssueSeverityV1,
    pub message: String,
    pub path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TestLabIssueSeverityV1 {
    Error,
    Warning,
}

#[derive(Debug, Error)]
pub enum TestLabError {
    #[error("test lab revision conflict: expected {expected}, actual {actual}")]
    RevisionConflict { expected: i64, actual: i64 },
    #[error("test lab contains invalid data")]
    Invalid(Vec<TestLabIssueV1>),
}

fn collect_unique_ids(
    ids: impl Iterator<Item = Uuid>,
    code: &str,
    path: &str,
    issues: &mut Vec<TestLabIssueV1>,
) -> BTreeSet<Uuid> {
    let mut unique = BTreeSet::new();
    for id in ids {
        if !unique.insert(id) {
            issues.push(error(code, "식별자가 중복되었습니다.", path));
        }
    }
    unique
}

fn validate_path(path: &str, issue_path: impl Into<String>, issues: &mut Vec<TestLabIssueV1>) {
    let invalid = path.is_empty()
        || path.starts_with('/')
        || path.contains('\\')
        || path
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..");
    if invalid {
        issues.push(error(
            "test_lab.invalid_path",
            "파일 경로는 상대 경로여야 하며 빈 조각, '.', '..', 역슬래시를 포함할 수 없습니다.",
            issue_path,
        ));
    }
}

fn has_dependency_cycle(dependencies: &BTreeMap<Uuid, Vec<Uuid>>) -> bool {
    fn visit(
        id: Uuid,
        dependencies: &BTreeMap<Uuid, Vec<Uuid>>,
        visiting: &mut BTreeSet<Uuid>,
        visited: &mut BTreeSet<Uuid>,
    ) -> bool {
        if visited.contains(&id) {
            return false;
        }
        if !visiting.insert(id) {
            return true;
        }
        if dependencies
            .get(&id)
            .into_iter()
            .flatten()
            .any(|dependency| visit(*dependency, dependencies, visiting, visited))
        {
            return true;
        }
        visiting.remove(&id);
        visited.insert(id);
        false
    }

    let mut visiting = BTreeSet::new();
    let mut visited = BTreeSet::new();
    dependencies
        .keys()
        .any(|id| visit(*id, dependencies, &mut visiting, &mut visited))
}

fn error(
    code: impl Into<String>,
    message: impl Into<String>,
    path: impl Into<String>,
) -> TestLabIssueV1 {
    TestLabIssueV1 {
        code: code.into(),
        severity: TestLabIssueSeverityV1::Error,
        message: message.into(),
        path: path.into(),
    }
}

fn warning(
    code: impl Into<String>,
    message: impl Into<String>,
    path: impl Into<String>,
) -> TestLabIssueV1 {
    TestLabIssueV1 {
        code: code.into(),
        severity: TestLabIssueSeverityV1::Warning,
        message: message.into(),
        path: path.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_lab_is_storable_for_every_problem_type() {
        for problem_type in [
            ProblemType::Standard,
            ProblemType::Scored,
            ProblemType::Interactive,
            ProblemType::OutputOnly,
            ProblemType::Library,
            ProblemType::Grader,
        ] {
            let draft = TestLabDraftV1::initial(problem_type);
            assert!(
                draft.ensure_storable(problem_type).is_ok(),
                "{problem_type:?}"
            );
        }
    }

    #[test]
    fn rejects_cycles_and_dangling_generator_references() {
        let mut draft = TestLabDraftV1::initial(ProblemType::Scored);
        let first = draft.groups[0].id;
        let second = Uuid::now_v7();
        draft.groups[0].depends_on = vec![second];
        draft.groups.push(TestGroupDraftV1 {
            id: second,
            name: "hard".into(),
            points: 50.0,
            depends_on: vec![first],
            feedback_policy: GroupFeedbackPolicyV1::Complete,
        });
        draft.test_cases[0].origin = TestCaseOriginV1::Generated;
        draft.test_cases[0].generator_id = Some(Uuid::now_v7());
        draft.test_cases[0].seed = Some(1);

        let issues = draft.analyze(ProblemType::Scored);
        assert!(
            issues
                .iter()
                .any(|issue| issue.code == "test_lab.group_dependency_cycle")
        );
        assert!(
            issues
                .iter()
                .any(|issue| issue.code == "test_lab.unknown_generator")
        );
    }

    #[test]
    fn custom_checker_requires_both_test_polarities() {
        let mut draft = TestLabDraftV1::initial(ProblemType::Standard);
        draft.checker = TestLabCheckerV1::Custom {
            source_path: "checkers/checker.cpp".into(),
            language: "cpp20".into(),
            source: "int main() { return 0; }\n".into(),
            arguments: Vec::new(),
        };
        draft.checker_tests.push(CheckerUnitTestDraftV1 {
            id: Uuid::now_v7(),
            name: "accept".into(),
            input: "1 2\n".into(),
            answer: "3\n".into(),
            output: "3\n".into(),
            expected_accepted: true,
        });
        let issues = draft.analyze(ProblemType::Standard);
        assert!(
            issues
                .iter()
                .any(|issue| issue.code == "test_lab.checker_test_polarity_missing")
        );
    }
}
