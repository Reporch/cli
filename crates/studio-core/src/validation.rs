use std::collections::{BTreeMap, BTreeSet};

use pulldown_cmark::{Event, Options, Parser, Tag, html};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{
    CheckerSpec, ExecutionHarnessV1, ExpectedScoreRange, ExpectedVerdict,
    NATIVE_PACKAGE_RESERVED_PATHS, ProblemType, ReleaseManifestV1, ReleaseManifestV2,
    SolutionRoleV2, VersionedReleaseManifest, validate_relative_path,
};

const MIN_TIME_LIMIT_MS: u64 = 10;
const MAX_TIME_LIMIT_MS: u64 = 10 * 60 * 1_000;
const MIN_MEMORY_LIMIT_MIB: u64 = 16;
const MAX_MEMORY_LIMIT_MIB: u64 = 8 * 1_024;
const MIN_OUTPUT_LIMIT_KIB: u64 = 1;
const MAX_OUTPUT_LIMIT_KIB: u64 = 1_024 * 1_024;
const MAX_TEST_GROUPS: usize = 10_000;
const MAX_TEST_GROUP_EDGES: usize = 100_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum IssueSeverity {
    Error,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ValidationIssue {
    pub code: String,
    pub severity: IssueSeverity,
    pub message: String,
    #[serde(default)]
    pub path: Option<String>,
}

/// Canonical semantic validator shared by every manifest consumer.
///
/// Callers must not special-case a manifest version. Keeping the dispatch here
/// guarantees that `check`, package, push, import/export and Studio compilation
/// apply the same release-blocking rules.
pub fn validate_versioned_manifest(manifest: &VersionedReleaseManifest) -> Vec<ValidationIssue> {
    let mut issues = match manifest {
        VersionedReleaseManifest::V1(manifest) => validate_manifest(manifest),
        VersionedReleaseManifest::V2(manifest) => validate_manifest_v2(manifest),
    };
    issues.sort_by(|left, right| {
        (
            left.code.as_str(),
            left.path.as_deref().unwrap_or_default(),
            left.message.as_str(),
        )
            .cmp(&(
                right.code.as_str(),
                right.path.as_deref().unwrap_or_default(),
                right.message.as_str(),
            ))
    });
    issues
}

pub fn validate_manifest_v2(manifest: &ReleaseManifestV2) -> Vec<ValidationIssue> {
    let mut issues = Vec::new();
    if let Err(error) = manifest.validate_references() {
        issues.push(error_issue("manifest.references", error.to_string()));
    }
    if !manifest.title.contains_key(&manifest.default_locale) {
        issues.push(error_issue(
            "statement.default_title_missing",
            format!("default locale {} has no title", manifest.default_locale),
        ));
    }
    if !manifest.statements.contains_key(&manifest.default_locale) {
        issues.push(error_issue(
            "statement.default_statement_missing",
            format!(
                "default locale {} has no statement",
                manifest.default_locale
            ),
        ));
    }
    let limits = &manifest.testing.limits;
    if !(MIN_TIME_LIMIT_MS..=MAX_TIME_LIMIT_MS).contains(&limits.time_ms)
        || !(MIN_MEMORY_LIMIT_MIB..=MAX_MEMORY_LIMIT_MIB).contains(&limits.memory_mib)
        || !(MIN_OUTPUT_LIMIT_KIB..=MAX_OUTPUT_LIMIT_KIB).contains(&limits.output_kib)
    {
        issues.push(error_issue(
            "judging.invalid_limits",
            format!(
                "limits must be within time {MIN_TIME_LIMIT_MS}..={MAX_TIME_LIMIT_MS} ms, memory {MIN_MEMORY_LIMIT_MIB}..={MAX_MEMORY_LIMIT_MIB} MiB, and output {MIN_OUTPUT_LIMIT_KIB}..={MAX_OUTPUT_LIMIT_KIB} KiB"
            ),
        ));
    }
    if manifest.testing.tests.is_empty() {
        issues.push(error_issue(
            "tests.empty",
            "at least one test is required".into(),
        ));
    }

    validate_native_package_paths_v2(manifest, &mut issues);
    validate_tests_v2(manifest, &mut issues);
    validate_groups_v2(manifest, &mut issues);
    validate_program_matrix_v2(manifest, &mut issues);
    validate_checker_matrix_v2(manifest, &mut issues);
    validate_problem_type_v2(manifest, &mut issues);
    validate_output_submissions_v2(manifest, &mut issues);
    validate_publication_v2(manifest, &mut issues);
    validate_provenance_v2(manifest, &mut issues);
    issues
}

fn validate_native_package_paths_v2(
    manifest: &ReleaseManifestV2,
    issues: &mut Vec<ValidationIssue>,
) {
    let reserved = NATIVE_PACKAGE_RESERVED_PATHS
        .iter()
        .map(|path| path.to_lowercase())
        .collect::<BTreeSet<_>>();
    let mut portable_paths = BTreeSet::new();
    for file in &manifest.files {
        let portable_path = file.path.to_lowercase();
        if reserved.contains(&portable_path) {
            issues.push(ValidationIssue {
                code: "files.native_package_reserved_path".into(),
                severity: IssueSeverity::Error,
                message: "manifest file collides with Reporch Native package metadata".into(),
                path: Some(file.path.clone()),
            });
        }
        if !portable_paths.insert(portable_path) {
            issues.push(ValidationIssue {
                code: "files.portable_path_collision".into(),
                severity: IssueSeverity::Error,
                message: "manifest paths collide on case-insensitive supported platforms".into(),
                path: Some(file.path.clone()),
            });
        }
    }
}

fn validate_tests_v2(manifest: &ReleaseManifestV2, issues: &mut Vec<ValidationIssue>) {
    let mut ids = BTreeSet::new();
    let mut names = BTreeSet::new();
    let file_digests = manifest
        .files
        .iter()
        .map(|file| (file.path.as_str(), file.sha256.as_str()))
        .collect::<BTreeMap<_, _>>();
    let mut input_digests = BTreeMap::new();
    for test in &manifest.testing.tests {
        if test.name.trim().is_empty()
            || test.name.len() > 120
            || !ids.insert(test.id)
            || !names.insert(test.name.as_str())
        {
            issues.push(error_issue(
                "tests.duplicate_or_invalid_identity",
                "test identifiers and non-empty names must be unique".into(),
            ));
        }
        if manifest.testing.detect_duplicates
            && let Some(digest) = file_digests.get(test.input_file.as_str())
            && let Some(first) = input_digests.insert(*digest, test.name.as_str())
        {
            issues.push(error_issue(
                "tests.duplicate_input",
                format!(
                    "tests {first} and {} contain byte-identical input",
                    test.name
                ),
            ));
        }
        if matches!(
            manifest.problem_type,
            ProblemType::Standard
                | ProblemType::Scored
                | ProblemType::Library
                | ProblemType::Grader
        ) && test.answer_file.is_none()
        {
            issues.push(error_issue(
                "tests.answer_missing",
                format!("test {} requires an answer file", test.name),
            ));
        }
        let generated_origin = matches!(test.origin, crate::TestCaseOriginV2::Generated);
        if generated_origin != test.generated.is_some() {
            issues.push(error_issue(
                "tests.generator_binding_invalid",
                format!(
                    "generated test {} must bind exactly one deterministic generator recipe",
                    test.name
                ),
            ));
        }
        if manifest.problem_type == ProblemType::Scored && test.group_ids.is_empty() {
            issues.push(error_issue(
                "tests.scored_group_missing",
                format!("scored test {} must belong to a group", test.name),
            ));
        }
        if let Some(points) = test.points
            && (!points.is_finite() || !(0.0..=100.0).contains(&points))
        {
            issues.push(error_issue(
                "tests.invalid_points",
                format!(
                    "test {} points must be a finite value from 0 to 100",
                    test.name
                ),
            ));
        }
    }
}

fn validate_groups_v2(manifest: &ReleaseManifestV2, issues: &mut Vec<ValidationIssue>) {
    let groups = manifest
        .testing
        .groups
        .iter()
        .map(|group| (group.id, group))
        .collect::<BTreeMap<_, _>>();
    if groups.len() != manifest.testing.groups.len() {
        issues.push(error_issue(
            "groups.duplicate_id",
            "test group identifiers must be unique".into(),
        ));
    }
    let mut names = BTreeSet::new();
    for group in groups.values() {
        if group.name.trim().is_empty()
            || group.name.len() > 120
            || !names.insert(group.name.as_str())
        {
            issues.push(error_issue(
                "groups.duplicate_or_invalid_name",
                "test group names must be non-empty and unique".into(),
            ));
        }
        if !group.points.is_finite() || !(0.0..=100.0).contains(&group.points) {
            issues.push(error_issue(
                "groups.invalid_points",
                format!(
                    "group {} points must be a finite value from 0 to 100",
                    group.name
                ),
            ));
        }
        if group.depends_on.contains(&group.id)
            || group
                .depends_on
                .iter()
                .any(|dependency| !groups.contains_key(dependency))
        {
            issues.push(error_issue(
                "groups.invalid_dependency",
                format!("group {} has an invalid dependency", group.name),
            ));
        }
    }
    if groups.len() <= MAX_TEST_GROUPS
        && groups
            .values()
            .map(|group| group.depends_on.len())
            .sum::<usize>()
            <= MAX_TEST_GROUP_EDGES
    {
        let mut remaining = groups
            .iter()
            .map(|(id, group)| {
                (
                    *id,
                    group.depends_on.iter().copied().collect::<BTreeSet<_>>(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        loop {
            let ready = remaining
                .iter()
                .filter(|(_, dependencies)| dependencies.is_empty())
                .map(|(id, _)| *id)
                .collect::<Vec<_>>();
            if ready.is_empty() {
                break;
            }
            for id in ready {
                remaining.remove(&id);
                for dependencies in remaining.values_mut() {
                    dependencies.remove(&id);
                }
            }
        }
        if !remaining.is_empty() {
            issues.push(error_issue(
                "groups.dependency_cycle",
                "test group dependency graph contains a cycle".into(),
            ));
        }
    } else {
        issues.push(error_issue(
            "groups.resource_limit",
            format!(
                "test groups are limited to {MAX_TEST_GROUPS} nodes and {MAX_TEST_GROUP_EDGES} dependency edges"
            ),
        ));
    }
    if manifest.problem_type == ProblemType::Scored {
        let total = groups.values().map(|group| group.points).sum::<f64>();
        if !total.is_finite() || (total - 100.0).abs() > 1e-9 {
            issues.push(error_issue(
                "groups.points_total",
                format!("scored problem group points must total 100, got {total}"),
            ));
        }
        let assigned = manifest
            .testing
            .tests
            .iter()
            .flat_map(|test| test.group_ids.iter())
            .chain(
                manifest
                    .testing
                    .generators
                    .iter()
                    .flat_map(|generator| generator.recipes.iter())
                    .flat_map(|recipe| recipe.group_ids.iter()),
            )
            .copied()
            .collect::<BTreeSet<_>>();
        for group in groups.values() {
            if !assigned.contains(&group.id) {
                issues.push(error_issue(
                    "groups.without_tests",
                    format!("scoring group {} has no tests", group.name),
                ));
            }
        }
    }
}

fn validate_program_matrix_v2(manifest: &ReleaseManifestV2, issues: &mut Vec<ValidationIssue>) {
    let mut accepted_references = Vec::new();
    let mut has_negative = false;
    let mut solution_names = BTreeSet::new();
    for solution in &manifest.testing.solutions {
        if solution.program.name.trim().is_empty()
            || !solution_names.insert(solution.program.name.as_str())
        {
            issues.push(error_issue(
                "solutions.invalid_metadata",
                "solution names must be non-empty and unique".into(),
            ));
        }
        validate_expected_score(
            solution.expected_verdict,
            solution.expected_score.as_ref(),
            "solutions",
            issues,
        );
        for expectation in &solution.group_expectations {
            validate_expected_score(
                expectation.verdict,
                expectation.score.as_ref(),
                "solutions.group_expectations",
                issues,
            );
        }
        if solution.role == SolutionRoleV2::Reference {
            if solution.expected_verdict != ExpectedVerdict::Accepted {
                issues.push(error_issue(
                    "solutions.reference_not_accepted",
                    format!(
                        "reference solution {} must expect accepted",
                        solution.program.name
                    ),
                ));
            }
            accepted_references.push(solution);
        }
        if solution.role == SolutionRoleV2::KnownWrong
            && solution.expected_verdict == ExpectedVerdict::Accepted
        {
            issues.push(error_issue(
                "solutions.known_wrong_accepted",
                format!(
                    "known-wrong solution {} cannot expect accepted",
                    solution.program.name
                ),
            ));
        }
        has_negative |= solution.expected_verdict != ExpectedVerdict::Accepted;
    }
    if manifest.problem_type != ProblemType::OutputOnly {
        if accepted_references.len() != 1 {
            issues.push(error_issue(
                "solutions.reference_count",
                "exactly one accepted reference solution is required".into(),
            ));
        }
        if !has_negative {
            issues.push(error_issue(
                "solutions.negative_missing",
                "at least one non-accepted solution is required for the verdict matrix".into(),
            ));
        }
    }
}

fn validate_checker_matrix_v2(manifest: &ReleaseManifestV2, issues: &mut Vec<ValidationIssue>) {
    match &manifest.testing.checker.checker {
        CheckerSpec::Floating {
            absolute_error,
            relative_error,
        } => {
            let absolute = absolute_error.parse::<f64>().ok();
            let relative = relative_error.parse::<f64>().ok();
            if absolute.is_none_or(|value| !value.is_finite() || value < 0.0)
                || relative.is_none_or(|value| !value.is_finite() || value < 0.0)
                || absolute == Some(0.0) && relative == Some(0.0)
            {
                issues.push(error_issue(
                    "checker.invalid_float_tolerance",
                    "floating checker tolerances must be finite and non-negative, with at least one greater than zero"
                        .into(),
                ));
            }
        }
        CheckerSpec::Custom { language, .. } if language.trim().is_empty() => {
            issues.push(error_issue(
                "checker.language_missing",
                "custom checkers require an explicit toolchain language".into(),
            ));
        }
        _ => {}
    }
    let units = &manifest.testing.checker.unit_tests;
    if matches!(manifest.testing.checker.checker, CheckerSpec::Custom { .. })
        && (!units.iter().any(|unit| unit.expected_accepted)
            || !units.iter().any(|unit| !unit.expected_accepted))
    {
        issues.push(error_issue(
            "checker.unit_matrix_incomplete",
            "custom checker unit tests require both an accepted and rejected output".into(),
        ));
    }
    let validators_present = manifest.testing.validators.primary.is_some()
        || !manifest.testing.validators.extra.is_empty();
    if validators_present
        && (!manifest
            .testing
            .validators
            .unit_tests
            .iter()
            .any(|unit| unit.expected_valid)
            || !manifest
                .testing
                .validators
                .unit_tests
                .iter()
                .any(|unit| !unit.expected_valid))
    {
        issues.push(error_issue(
            "validator.unit_matrix_incomplete",
            "validators require both valid and invalid unit inputs".into(),
        ));
    }
}

fn validate_problem_type_v2(manifest: &ReleaseManifestV2, issues: &mut Vec<ValidationIssue>) {
    match manifest.problem_type {
        ProblemType::Interactive if manifest.execution.interactive.is_none() => {
            issues.push(error_issue(
                "interactive.interactor_missing",
                "interactive problems require an interactor".into(),
            ))
        }
        ProblemType::Library | ProblemType::Grader if manifest.execution.harness.is_none() => {
            issues.push(error_issue(
                "harness.missing",
                "library/grader problems require a language harness".into(),
            ));
        }
        ProblemType::Standard | ProblemType::Scored | ProblemType::OutputOnly
            if manifest.execution.interactive.is_some() || manifest.execution.harness.is_some() =>
        {
            issues.push(error_issue(
                "execution.unexpected",
                "this problem type cannot declare an interactor or grader/library harness".into(),
            ));
        }
        _ => {}
    }
    if let Some(harness) = &manifest.execution.harness {
        let expected = match manifest.problem_type {
            ProblemType::Library => Some(crate::HarnessKindV2::Library),
            ProblemType::Grader => Some(crate::HarnessKindV2::Grader),
            _ => None,
        };
        if expected.is_some_and(|kind| kind != harness.kind) {
            issues.push(error_issue(
                "harness.kind_mismatch",
                "harness kind does not match the problem type".into(),
            ));
        }
    }
}

fn validate_output_submissions_v2(manifest: &ReleaseManifestV2, issues: &mut Vec<ValidationIssue>) {
    if manifest.problem_type != ProblemType::OutputOnly {
        if !manifest.output_submissions.is_empty() {
            issues.push(error_issue(
                "output_submissions.unexpected",
                "output submissions are only valid for output-only problems".into(),
            ));
        }
        return;
    }
    if !manifest.testing.solutions.is_empty() {
        issues.push(error_issue(
            "output_submissions.code_solutions_unexpected",
            "output-only problems use output submissions instead of code solutions".into(),
        ));
    }
    let expected_tests = manifest
        .testing
        .tests
        .iter()
        .map(|test| test.id)
        .collect::<BTreeSet<_>>();
    let mut names = BTreeSet::new();
    for submission in &manifest.output_submissions {
        if submission.name.trim().is_empty()
            || submission.name.len() > 120
            || !names.insert(submission.name.as_str())
        {
            issues.push(error_issue(
                "output_submissions.invalid_name",
                "output submission names must be non-empty and unique".into(),
            ));
        }
        if submission.outputs.keys().copied().collect::<BTreeSet<_>>() != expected_tests {
            issues.push(error_issue(
                "output_submissions.incomplete_test_coverage",
                format!(
                    "output submission {} must provide exactly one output for every test",
                    submission.name
                ),
            ));
        }
        validate_expected_score_v2(
            submission.expected_verdict,
            submission.expected_score.as_ref(),
            "output_submissions",
            issues,
        );
    }
    if !manifest
        .output_submissions
        .iter()
        .any(|submission| submission.expected_verdict == ExpectedVerdict::Accepted)
    {
        issues.push(error_issue(
            "output_submissions.accepted_missing",
            "at least one accepted reference output submission is required".into(),
        ));
    }
    if !manifest
        .output_submissions
        .iter()
        .any(|submission| submission.expected_verdict != ExpectedVerdict::Accepted)
    {
        issues.push(error_issue(
            "output_submissions.negative_missing",
            "at least one non-accepted output submission is required".into(),
        ));
    }
}

fn validate_expected_score_v2(
    verdict: ExpectedVerdict,
    score: Option<&ExpectedScoreRange>,
    prefix: &str,
    issues: &mut Vec<ValidationIssue>,
) {
    validate_expected_score(verdict, score, prefix, issues);
}

fn validate_publication_v2(manifest: &ReleaseManifestV2, issues: &mut Vec<ValidationIssue>) {
    if let Some(publication) = &manifest.publication {
        if !publication
            .statement_sections
            .contains_key(&manifest.default_locale)
        {
            issues.push(error_issue(
                "publication.default_statement_sections_missing",
                format!(
                    "default locale {} has no publication statement sections",
                    manifest.default_locale
                ),
            ));
        }
        for locale in publication.statement_sections.keys() {
            if !manifest.title.contains_key(locale) || !manifest.statements.contains_key(locale) {
                issues.push(error_issue(
                    "publication.locale_statement_missing",
                    format!("publication locale {locale} has no title or statement"),
                ));
            }
        }
    }
}

fn validate_provenance_v2(manifest: &ReleaseManifestV2, issues: &mut Vec<ValidationIssue>) {
    if manifest.policy_version.trim().is_empty() || manifest.policy_version.len() > 128 {
        issues.push(error_issue(
            "manifest.invalid_policy_version",
            "policy version must be non-empty and at most 128 bytes".into(),
        ));
    }
    let mut source_ids = BTreeSet::new();
    for source in &manifest.sources {
        let valid_url = url::Url::parse(&source.canonical_url)
            .ok()
            .is_some_and(|url| {
                url.scheme() == "https"
                    && url.host_str().is_some()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.fragment().is_none()
            });
        if source.provider.trim().is_empty()
            || source.external_id.trim().is_empty()
            || !source_ids.insert((source.provider.as_str(), source.external_id.as_str()))
            || !valid_url
            || source.license_name.trim().is_empty()
            || source.attribution.trim().is_empty()
        {
            issues.push(error_issue(
                "sources.invalid_attribution",
                "external sources require a unique provider/id, credential-free HTTPS URL, license, and attribution"
                    .into(),
            ));
        }
    }
}

pub fn validate_manifest(manifest: &ReleaseManifestV1) -> Vec<ValidationIssue> {
    let mut issues = Vec::new();
    if let Err(error) = manifest.validate_references() {
        issues.push(error_issue("manifest.references", error.to_string()));
    }
    if !manifest.title.contains_key(&manifest.default_locale) {
        issues.push(error_issue(
            "statement.default_title_missing",
            format!("default locale {} has no title", manifest.default_locale),
        ));
    }
    if !manifest.statements.contains_key(&manifest.default_locale) {
        issues.push(error_issue(
            "statement.default_statement_missing",
            format!(
                "default locale {} has no statement",
                manifest.default_locale
            ),
        ));
    }
    if !(MIN_TIME_LIMIT_MS..=MAX_TIME_LIMIT_MS).contains(&manifest.judging.limits.time_ms)
        || !(MIN_MEMORY_LIMIT_MIB..=MAX_MEMORY_LIMIT_MIB)
            .contains(&manifest.judging.limits.memory_mib)
        || !(MIN_OUTPUT_LIMIT_KIB..=MAX_OUTPUT_LIMIT_KIB)
            .contains(&manifest.judging.limits.output_kib)
    {
        issues.push(error_issue(
            "judging.invalid_limits",
            format!(
                "limits must be within time {MIN_TIME_LIMIT_MS}..={MAX_TIME_LIMIT_MS} ms, memory {MIN_MEMORY_LIMIT_MIB}..={MAX_MEMORY_LIMIT_MIB} MiB, and output {MIN_OUTPUT_LIMIT_KIB}..={MAX_OUTPUT_LIMIT_KIB} KiB"
            ),
        ));
    }
    if manifest.judging.tests.is_empty() && manifest.problem_type != ProblemType::OutputOnly {
        issues.push(error_issue(
            "tests.empty",
            "at least one test is required for this problem type".into(),
        ));
    }

    validate_groups(manifest, &mut issues);
    validate_native_package_paths(manifest, &mut issues);
    validate_tests(manifest, &mut issues);
    validate_problem_type(manifest, &mut issues);
    validate_solutions(manifest, &mut issues);
    validate_publication(manifest, &mut issues);
    validate_provenance(manifest, &mut issues);
    issues
}

fn validate_native_package_paths(manifest: &ReleaseManifestV1, issues: &mut Vec<ValidationIssue>) {
    let reserved = NATIVE_PACKAGE_RESERVED_PATHS
        .iter()
        .map(|path| path.to_lowercase())
        .collect::<BTreeSet<_>>();
    let mut portable_paths = BTreeSet::new();
    for file in &manifest.files {
        let portable_path = file.path.to_lowercase();
        if reserved.contains(&portable_path) {
            issues.push(ValidationIssue {
                code: "files.native_package_reserved_path".into(),
                severity: IssueSeverity::Error,
                message: "manifest file collides with Reporch Native package metadata".into(),
                path: Some(file.path.clone()),
            });
        }
        if !portable_paths.insert(portable_path) {
            issues.push(ValidationIssue {
                code: "files.portable_path_collision".into(),
                severity: IssueSeverity::Error,
                message: "manifest paths collide on case-insensitive supported platforms".into(),
                path: Some(file.path.clone()),
            });
        }
    }
}

pub fn validate_statement_markdown(markdown: &str) -> Vec<ValidationIssue> {
    let mut issues = Vec::new();
    if markdown.is_empty() {
        issues.push(error_issue(
            "statement.empty",
            "problem statement must not be empty".into(),
        ));
        return issues;
    }
    if markdown.contains('\0')
        || markdown
            .chars()
            .any(|value| value.is_control() && !matches!(value, '\n' | '\r' | '\t'))
    {
        issues.push(error_issue(
            "statement.control_character",
            "problem statement contains a forbidden control character".into(),
        ));
    }

    let options = Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_MATH;
    for event in Parser::new_ext(markdown, options) {
        match event {
            Event::Html(_) | Event::InlineHtml(_) => issues.push(error_issue(
                "statement.raw_html_forbidden",
                "raw HTML is not allowed in Studio Markdown".into(),
            )),
            Event::Start(Tag::Link { dest_url, .. })
            | Event::Start(Tag::Image { dest_url, .. })
                if has_unsafe_url_scheme(dest_url.as_ref()) =>
            {
                issues.push(error_issue(
                    "statement.unsafe_url",
                    "statement links and images cannot use executable or embedded URL schemes"
                        .into(),
                ));
            }
            Event::Start(Tag::Image { dest_url, .. })
                if has_external_image_url(dest_url.as_ref()) =>
            {
                issues.push(error_issue(
                    "statement.external_image_forbidden",
                    "statement images must use a project-relative or same-origin path".into(),
                ));
            }
            Event::Start(Tag::Image { dest_url, .. }) => {
                if let Err(message) = validate_statement_image_path(dest_url.as_ref()) {
                    issues.push(error_issue("statement.invalid_image_path", message));
                }
            }
            _ => {}
        }
    }
    issues
}

/// Returns normalized project-relative image paths after applying the exact same
/// policy used by statement validation. Renderers use this list to load only the
/// immutable CAS objects explicitly referenced by the Markdown document.
pub fn statement_image_paths(markdown: &str) -> Result<BTreeSet<String>, Vec<ValidationIssue>> {
    let issues = validate_statement_markdown(markdown);
    if !issues.is_empty() {
        return Err(issues);
    }
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_MATH;
    Ok(Parser::new_ext(markdown, options)
        .filter_map(|event| match event {
            Event::Start(Tag::Image { dest_url, .. }) => {
                Some(normalize_statement_image_path(dest_url.as_ref()))
            }
            _ => None,
        })
        .collect())
}

/// Renders only the Markdown subset accepted by `validate_statement_markdown`.
/// The complete document and policy are intentionally fixed so identical input
/// bytes produce identical output bytes across API and worker processes.
pub fn render_statement_html(markdown: &str) -> Result<String, Vec<ValidationIssue>> {
    let issues = validate_statement_markdown(markdown);
    if !issues.is_empty() {
        return Err(issues);
    }
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_MATH;
    let mut body = String::new();
    html::push_html(&mut body, Parser::new_ext(markdown, options));
    Ok(format!(
        "<!doctype html>\n<html><head><meta charset=\"utf-8\"><meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; img-src 'self'; style-src 'none'; font-src 'none'; base-uri 'none'; form-action 'none'\"><meta name=\"referrer\" content=\"no-referrer\"></head><body><main>{body}</main></body></html>\n"
    ))
}

fn has_unsafe_url_scheme(value: &str) -> bool {
    let normalized = value.trim().to_ascii_lowercase();
    normalized.starts_with("javascript:")
        || normalized.starts_with("vbscript:")
        || normalized.starts_with("data:")
        || normalized.starts_with("file:")
}

fn has_external_image_url(value: &str) -> bool {
    let normalized = value.trim();
    normalized.starts_with("//") || url::Url::parse(normalized).is_ok()
}

fn validate_statement_image_path(value: &str) -> Result<(), String> {
    let normalized = normalize_statement_image_path(value);
    if value.contains(['?', '#']) {
        return Err("statement image paths cannot contain a query or fragment".into());
    }
    validate_relative_path(&normalized)
        .map_err(|error| format!("invalid project-relative statement image path: {error}"))?;
    let extension = normalized
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase());
    if !matches!(
        extension.as_deref(),
        Some("png" | "jpg" | "jpeg" | "gif" | "svg")
    ) {
        return Err("statement images must be PNG, JPEG, GIF, or SVG files".into());
    }
    Ok(())
}

fn normalize_statement_image_path(value: &str) -> String {
    value.trim().trim_start_matches("./").to_string()
}

fn validate_tests(manifest: &ReleaseManifestV1, issues: &mut Vec<ValidationIssue>) {
    let generator_ids: BTreeSet<&str> = manifest
        .judging
        .generators
        .iter()
        .map(|generator| generator.id.as_str())
        .collect();
    let mut ids = BTreeSet::new();
    let mut names = BTreeSet::new();
    let file_digests: BTreeMap<&str, &str> = manifest
        .files
        .iter()
        .map(|file| (file.path.as_str(), file.sha256.as_str()))
        .collect();
    let mut input_digests = BTreeMap::new();
    for test in &manifest.judging.tests {
        if test.name.trim().is_empty()
            || test.name.len() > 120
            || !ids.insert(test.id)
            || !names.insert(test.name.as_str())
        {
            issues.push(error_issue(
                "tests.duplicate_or_invalid_identity",
                "test identifiers and non-empty names must be unique".into(),
            ));
        }
        if let Some(digest) = file_digests.get(test.input_file.as_str())
            && let Some(first_test) = input_digests.insert(*digest, test.name.as_str())
        {
            issues.push(error_issue(
                "tests.duplicate_input",
                format!(
                    "tests {first_test} and {} contain byte-identical input",
                    test.name
                ),
            ));
        }
        if matches!(
            manifest.problem_type,
            ProblemType::Standard
                | ProblemType::Scored
                | ProblemType::Library
                | ProblemType::Grader
        ) && test.answer_file.is_none()
        {
            issues.push(error_issue(
                "tests.answer_missing",
                format!("test {} requires an answer file", test.name),
            ));
        }
        if test.generated_by.is_some() != test.seed.is_some() {
            issues.push(error_issue(
                "tests.generator_seed_binding_invalid",
                format!(
                    "generated test {} must bind both a generator and a fixed seed",
                    test.name
                ),
            ));
        }
        if test.generated_by.is_none() && !test.generator_arguments.is_empty() {
            issues.push(error_issue(
                "tests.generator_arguments_without_generator",
                format!(
                    "test {} declares generator arguments without a generator",
                    test.name
                ),
            ));
        }
        if test
            .generated_by
            .as_deref()
            .is_some_and(|generator| !generator_ids.contains(generator))
        {
            issues.push(error_issue(
                "tests.generator_unknown",
                format!(
                    "generated test {} references an unknown generator",
                    test.name
                ),
            ));
        }
        if manifest.problem_type == ProblemType::Scored && test.groups.is_empty() {
            issues.push(error_issue(
                "tests.scored_group_missing",
                format!(
                    "scored test {} must belong to at least one group",
                    test.name
                ),
            ));
        }
    }
}

fn validate_solutions(manifest: &ReleaseManifestV1, issues: &mut Vec<ValidationIssue>) {
    if manifest.problem_type == ProblemType::OutputOnly {
        validate_output_submissions(manifest, issues);
        return;
    }
    if !manifest.output_submissions.is_empty() {
        issues.push(error_issue(
            "output_submissions.unexpected",
            "output submissions are only valid for output-only problems".into(),
        ));
    }
    let mut names = BTreeSet::new();
    for solution in &manifest.solutions {
        if solution.name.trim().is_empty()
            || solution.name.len() > 120
            || !names.insert(solution.name.as_str())
            || solution.language.trim().is_empty()
            || solution.language.len() > 50
        {
            issues.push(error_issue(
                "solutions.invalid_metadata",
                "solution names must be unique and solution languages must be present".into(),
            ));
            break;
        }
        validate_expected_score(
            solution.expected_verdict,
            solution.expected_score.as_ref(),
            "solutions",
            issues,
        );
    }
    let accepted_solutions: Vec<_> = manifest
        .solutions
        .iter()
        .filter(|solution| solution.expected_verdict == ExpectedVerdict::Accepted)
        .collect();
    if accepted_solutions.is_empty() {
        issues.push(error_issue(
            "solutions.accepted_missing",
            "at least one accepted reference solution is required".into(),
        ));
    } else {
        let source_digests: BTreeSet<_> = accepted_solutions
            .iter()
            .filter_map(|solution| {
                manifest
                    .files
                    .iter()
                    .find(|file| file.path == solution.source_path)
                    .map(|file| file.sha256.as_str())
            })
            .collect();
        if accepted_solutions.len() < 2 || source_digests.len() < 2 {
            issues.push(error_issue(
                "solutions.accepted_cross_check_missing",
                "at least two accepted solutions with distinct source digests are required".into(),
            ));
        }
    }
    if !manifest
        .solutions
        .iter()
        .any(|solution| solution.expected_verdict != ExpectedVerdict::Accepted)
    {
        issues.push(error_issue(
            "solutions.negative_missing",
            "at least one non-accepted solution is required for the verdict matrix".into(),
        ));
    }
}

fn validate_output_submissions(manifest: &ReleaseManifestV1, issues: &mut Vec<ValidationIssue>) {
    if !manifest.solutions.is_empty() {
        issues.push(error_issue(
            "output_submissions.code_solutions_unexpected",
            "output-only problems use output submissions instead of code solutions".into(),
        ));
    }
    if manifest.judging.tests.is_empty() {
        issues.push(error_issue(
            "output_submissions.tests_missing",
            "output-only problems require at least one input/answer test".into(),
        ));
    }
    let expected_tests: BTreeSet<_> = manifest.judging.tests.iter().map(|test| test.id).collect();
    let mut names = BTreeSet::new();
    for submission in &manifest.output_submissions {
        if submission.name.trim().is_empty()
            || submission.name.len() > 120
            || !names.insert(submission.name.as_str())
        {
            issues.push(error_issue(
                "output_submissions.invalid_name",
                "output submission names must be non-empty and unique".into(),
            ));
        }
        let submitted_tests: BTreeSet<_> = submission.outputs.keys().copied().collect();
        if submitted_tests != expected_tests {
            issues.push(error_issue(
                "output_submissions.incomplete_test_coverage",
                format!(
                    "output submission {} must provide exactly one output for every test",
                    submission.name
                ),
            ));
        }
        validate_expected_score(
            submission.expected_verdict,
            submission.expected_score.as_ref(),
            "output_submissions",
            issues,
        );
    }
    if !manifest
        .output_submissions
        .iter()
        .any(|submission| submission.expected_verdict == ExpectedVerdict::Accepted)
    {
        issues.push(error_issue(
            "output_submissions.accepted_missing",
            "at least one accepted reference output submission is required".into(),
        ));
    }
    if !manifest
        .output_submissions
        .iter()
        .any(|submission| submission.expected_verdict != ExpectedVerdict::Accepted)
    {
        issues.push(error_issue(
            "output_submissions.negative_missing",
            "at least one non-accepted output submission is required".into(),
        ));
    }
}

fn validate_expected_score(
    verdict: ExpectedVerdict,
    score: Option<&crate::ExpectedScoreRange>,
    prefix: &str,
    issues: &mut Vec<ValidationIssue>,
) {
    match (verdict, score) {
        (ExpectedVerdict::Partial, Some(range))
            if range.minimum.is_finite()
                && range.maximum.is_finite()
                && range.minimum >= 0.0
                && range.minimum <= range.maximum
                && range.maximum <= 100.0 => {}
        (ExpectedVerdict::Partial, _) => issues.push(error_issue(
            &format!("{prefix}.partial_score_range_invalid"),
            "partial entries require a finite score range within 0..100".into(),
        )),
        (_, Some(_)) => issues.push(error_issue(
            &format!("{prefix}.unexpected_score_range"),
            "only partial entries may declare an expected score range".into(),
        )),
        _ => {}
    }
}

fn validate_provenance(manifest: &ReleaseManifestV1, issues: &mut Vec<ValidationIssue>) {
    if manifest.policy_version.trim().is_empty() || manifest.policy_version.len() > 128 {
        issues.push(error_issue(
            "manifest.invalid_policy_version",
            "policy version must be non-empty and at most 128 bytes".into(),
        ));
    }
    let mut source_ids = BTreeSet::new();
    for source in &manifest.sources {
        let parsed_url = url::Url::parse(&source.canonical_url).ok();
        let valid_url = parsed_url.as_ref().is_some_and(|url| {
            url.scheme() == "https"
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.fragment().is_none()
        });
        if source.provider.trim().is_empty()
            || source.provider.len() > 120
            || source.external_id.trim().is_empty()
            || source.external_id.len() > 255
            || !source_ids.insert((source.provider.as_str(), source.external_id.as_str()))
            || !valid_url
            || source.license_name.trim().is_empty()
            || source.license_name.len() > 120
            || source.attribution.trim().is_empty()
            || source.attribution.len() > 2_000
        {
            issues.push(error_issue(
                "sources.invalid_attribution",
                "external sources require a unique provider/id, credential-free HTTPS URL, license, and attribution"
                    .into(),
            ));
        }
    }
}

fn validate_publication(manifest: &ReleaseManifestV1, issues: &mut Vec<ValidationIssue>) {
    let Some(publication) = &manifest.publication else {
        return;
    };
    if publication.category.trim().is_empty()
        || publication.category.len() > 50
        || publication.difficulty.trim().is_empty()
        || publication.difficulty.len() > 15
        || publication.grading_category.trim().is_empty()
        || publication.grading_category.len() > 50
    {
        issues.push(error_issue(
            "publication.invalid_catalog_metadata",
            "category, difficulty, and grading category must fit the public catalog contract"
                .into(),
        ));
    }
    if !publication
        .statement_sections
        .contains_key(&manifest.default_locale)
    {
        issues.push(error_issue(
            "publication.default_statement_sections_missing",
            format!(
                "default locale {} has no publication statement sections",
                manifest.default_locale
            ),
        ));
    }
    for locale in publication.statement_sections.keys() {
        if !manifest.title.contains_key(locale) || !manifest.statements.contains_key(locale) {
            issues.push(error_issue(
                "publication.locale_statement_missing",
                format!("publication locale {locale} has no title or statement"),
            ));
        }
    }
    if publication.samples.len() > 100 {
        issues.push(error_issue(
            "publication.too_many_samples",
            "public projection supports at most 100 samples".into(),
        ));
    }
    let mut sample_names = BTreeSet::new();
    for sample in &publication.samples {
        if sample.name.trim().is_empty() || !sample_names.insert(sample.name.as_str()) {
            issues.push(error_issue(
                "publication.invalid_sample_name",
                "publication sample names must be non-empty and unique".into(),
            ));
        }
    }
    let mut tags = BTreeSet::new();
    if publication
        .tags
        .iter()
        .any(|tag| tag.trim().is_empty() || tag.len() > 50 || !tags.insert(tag.as_str()))
    {
        issues.push(error_issue(
            "publication.invalid_tags",
            "publication tags must be non-empty, unique, and at most 50 bytes".into(),
        ));
    }
    for (locale, title) in &manifest.title {
        if title.trim().is_empty() || title.chars().count() > 255 {
            issues.push(error_issue(
                "publication.invalid_title",
                format!("title for locale {locale} is empty or too long"),
            ));
        }
    }
}

fn validate_groups(manifest: &ReleaseManifestV1, issues: &mut Vec<ValidationIssue>) {
    let groups: BTreeMap<&str, &crate::TestGroupSpec> = manifest
        .judging
        .groups
        .iter()
        .map(|group| (group.id.as_str(), group))
        .collect();

    if groups.len() != manifest.judging.groups.len() {
        issues.push(error_issue(
            "groups.duplicate_id",
            "test group identifiers must be unique".into(),
        ));
    }
    let edge_count = manifest
        .judging
        .groups
        .iter()
        .try_fold(0_usize, |total, group| {
            total.checked_add(group.depends_on.len())
        });
    let graph_within_limits = groups.len() <= MAX_TEST_GROUPS
        && edge_count.is_some_and(|count| count <= MAX_TEST_GROUP_EDGES);
    if !graph_within_limits {
        issues.push(error_issue(
            "groups.resource_limit",
            format!(
                "test groups are limited to {MAX_TEST_GROUPS} nodes and {MAX_TEST_GROUP_EDGES} dependency edges"
            ),
        ));
    }
    for test in &manifest.judging.tests {
        for group in &test.groups {
            if !groups.contains_key(group.as_str()) {
                issues.push(error_issue(
                    "groups.unknown_reference",
                    format!("test {} references unknown group {group}", test.name),
                ));
            }
        }
    }
    for group in groups.values() {
        if !group.points.is_finite() || !(0.0..=100.0).contains(&group.points) {
            issues.push(error_issue(
                "groups.invalid_points",
                format!(
                    "group {} points must be a finite value from 0 to 100",
                    group.id
                ),
            ));
        }
        for dependency in &group.depends_on {
            if dependency == &group.id || !groups.contains_key(dependency.as_str()) {
                issues.push(error_issue(
                    "groups.invalid_dependency",
                    format!("group {} has invalid dependency {dependency}", group.id),
                ));
            }
        }
    }
    if graph_within_limits {
        for group in cyclic_or_blocked_groups(&groups) {
            issues.push(error_issue(
                "groups.dependency_cycle",
                format!("group {group} participates in or depends on a dependency cycle"),
            ));
        }
    }

    if manifest.problem_type == ProblemType::Scored {
        let total: f64 = groups.values().map(|group| group.points).sum();
        if (total - 100.0).abs() > 1e-6 {
            issues.push(error_issue(
                "groups.points_total",
                format!("scored problem group points must total 100, got {total}"),
            ));
        }
        let referenced: BTreeSet<&str> = manifest
            .judging
            .tests
            .iter()
            .flat_map(|test| test.groups.iter().map(String::as_str))
            .collect();
        for group in groups.keys() {
            if !referenced.contains(group) {
                issues.push(error_issue(
                    "groups.without_tests",
                    format!("scoring group {group} has no tests"),
                ));
            }
        }
    }
}

fn cyclic_or_blocked_groups<'a>(
    groups: &BTreeMap<&'a str, &'a crate::TestGroupSpec>,
) -> BTreeSet<&'a str> {
    let mut pending_dependencies = groups
        .iter()
        .map(|(id, group)| {
            (
                *id,
                group
                    .depends_on
                    .iter()
                    .filter(|dependency| groups.contains_key(dependency.as_str()))
                    .count(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut dependents = BTreeMap::<&str, Vec<&str>>::new();
    for (id, group) in groups {
        for dependency in &group.depends_on {
            if let Some((dependency_id, _)) = groups.get_key_value(dependency.as_str()) {
                dependents.entry(*dependency_id).or_default().push(*id);
            }
        }
    }
    let mut ready = pending_dependencies
        .iter()
        .filter_map(|(id, count)| (*count == 0).then_some(*id))
        .collect::<Vec<_>>();
    while let Some(id) = ready.pop() {
        pending_dependencies.remove(id);
        for dependent in dependents.get(id).into_iter().flatten() {
            if let Some(count) = pending_dependencies.get_mut(dependent) {
                *count -= 1;
                if *count == 0 {
                    ready.push(*dependent);
                }
            }
        }
    }
    pending_dependencies.into_keys().collect()
}

fn validate_problem_type(manifest: &ReleaseManifestV1, issues: &mut Vec<ValidationIssue>) {
    let mut program_ids = BTreeSet::new();
    for generator in &manifest.judging.generators {
        if generator.id.trim().is_empty()
            || !program_ids.insert(generator.id.as_str())
            || generator.language.trim().is_empty()
        {
            issues.push(error_issue(
                "generator.invalid_metadata",
                "generator identifiers must be unique and languages must be present".into(),
            ));
        }
    }
    if manifest.judging.validator_path.is_some() {
        if manifest
            .judging
            .validator_language
            .as_deref()
            .is_none_or(|language| language.trim().is_empty())
        {
            issues.push(error_issue(
                "validator.language_missing",
                "validators require an explicit toolchain language".into(),
            ));
        }
        let positive = manifest
            .judging
            .validator_tests
            .iter()
            .any(|test| test.expected_valid);
        let negative = manifest
            .judging
            .validator_tests
            .iter()
            .any(|test| !test.expected_valid);
        if !positive || !negative {
            issues.push(error_issue(
                "validator.unit_matrix_incomplete",
                "validators require at least one expected-valid and one expected-invalid unit test"
                    .into(),
            ));
        }
    } else if manifest.judging.validator_language.is_some()
        || !manifest.judging.extra_validator_paths.is_empty()
        || !manifest.judging.extra_validators.is_empty()
        || !manifest.judging.validator_tests.is_empty()
    {
        issues.push(error_issue(
            "validator.primary_missing",
            "extra validators and validator tests require a primary validator".into(),
        ));
    }
    let mut extra_validator_ids = BTreeSet::new();
    for validator in &manifest.judging.extra_validators {
        if validator.id.trim().is_empty()
            || !extra_validator_ids.insert(validator.id.as_str())
            || validator.language.trim().is_empty()
        {
            issues.push(error_issue(
                "validator.extra_metadata_invalid",
                "extra validator identifiers must be unique and languages must be present".into(),
            ));
        }
    }
    if !manifest.judging.extra_validator_paths.is_empty() {
        issues.push(error_issue(
            "validator.legacy_extra_language_missing",
            "legacy extra_validator_paths cannot execute safely; use extra_validators with language"
                .into(),
        ));
    }
    match manifest.problem_type {
        ProblemType::Interactive if manifest.judging.interactor_path.is_none() => {
            issues.push(error_issue(
                "interactive.interactor_missing",
                "interactive problems require an interactor".into(),
            ))
        }
        ProblemType::Library | ProblemType::Grader if manifest.judging.grader_path.is_none() => {
            issues.push(error_issue(
                "grader.missing",
                "library/grader problems require a grader".into(),
            ))
        }
        _ => {}
    }
    if manifest.judging.interactor_path.is_some() != manifest.judging.interactor_language.is_some()
    {
        issues.push(error_issue(
            "interactive.language_binding_invalid",
            "interactor path and language must be declared together".into(),
        ));
    }
    if manifest.judging.grader_path.is_some() != manifest.judging.grader_language.is_some() {
        issues.push(error_issue(
            "grader.language_binding_invalid",
            "grader path and language must be declared together".into(),
        ));
    }
    validate_execution_harness(manifest, issues);
    if let CheckerSpec::Floating {
        absolute_error,
        relative_error,
    } = &manifest.judging.checker
    {
        let absolute = absolute_error.parse::<f64>().ok();
        let relative = relative_error.parse::<f64>().ok();
        if absolute.is_none_or(|value| !value.is_finite() || value < 0.0)
            || relative.is_none_or(|value| !value.is_finite() || value < 0.0)
            || absolute == Some(0.0) && relative == Some(0.0)
        {
            issues.push(error_issue(
                "checker.invalid_float_tolerance",
                "floating checker tolerances must be finite and non-negative, with at least one greater than zero"
                    .into(),
            ));
        }
    }
    if let CheckerSpec::Custom { language, .. } = &manifest.judging.checker {
        if language.trim().is_empty() {
            issues.push(error_issue(
                "checker.language_missing",
                "custom checkers require an explicit toolchain language".into(),
            ));
        }
        let positive = manifest
            .judging
            .checker_tests
            .iter()
            .any(|test| test.expected_accepted);
        let negative = manifest
            .judging
            .checker_tests
            .iter()
            .any(|test| !test.expected_accepted);
        if !positive || !negative {
            issues.push(error_issue(
                "checker.unit_matrix_incomplete",
                "custom checkers require at least one accepted and one rejected unit test".into(),
            ));
        }
    } else if !manifest.judging.checker_tests.is_empty() {
        issues.push(error_issue(
            "checker.tests_without_custom_checker",
            "checker unit tests are only valid for custom checkers".into(),
        ));
    }
}

fn validate_execution_harness(manifest: &ReleaseManifestV1, issues: &mut Vec<ValidationIssue>) {
    let harness = manifest.judging.harness.as_ref();
    match (manifest.problem_type, harness) {
        (ProblemType::Interactive, Some(ExecutionHarnessV1::InteractiveStdio { .. })) => {}
        (ProblemType::Interactive, _) => {
            issues.push(error_issue(
                "interactive.harness_missing",
                "interactive problems require an interactive_stdio harness".into(),
            ));
            return;
        }
        (
            ProblemType::Library | ProblemType::Grader,
            Some(ExecutionHarnessV1::CustomImpl { .. }),
        ) => {}
        (ProblemType::Library | ProblemType::Grader, _) => {
            issues.push(error_issue(
                "grader.harness_missing",
                "library/grader problems require a custom_impl harness".into(),
            ));
            return;
        }
        (_, Some(_)) => {
            issues.push(error_issue(
                "harness.problem_type_mismatch",
                "execution harnesses are only valid for interactive, library, and grader problems"
                    .into(),
            ));
            return;
        }
        (_, None) => return,
    }

    match harness.expect("specialized problem harness was matched above") {
        ExecutionHarnessV1::InteractiveStdio {
            profiles,
            score_scale,
            ..
        } => {
            if profiles.is_empty() || *score_scale == 0 {
                issues.push(error_issue(
                    "interactive.harness_invalid",
                    "interactive harness profiles and a positive score scale are required".into(),
                ));
            }
            for (language, profile) in profiles {
                if language.trim().is_empty()
                    || !matches!(
                        language.trim().to_ascii_lowercase().as_str(),
                        "c++" | "c++17" | "c++20" | "cpp" | "cpp17" | "cpp20"
                    )
                {
                    issues.push(error_issue(
                        "interactive.language_unsupported",
                        format!("interactive stdio profile {language} is not supported"),
                    ));
                }
                if manifest.judging.interactor_path.as_deref()
                    != Some(profile.interactor_source_path.as_str())
                    || !profile.asset_paths.contains(&profile.source_path)
                    || !profile
                        .asset_paths
                        .contains(&profile.interactor_source_path)
                {
                    issues.push(error_issue(
                        "interactive.profile_binding_invalid",
                        format!(
                            "interactive profile {language} must bind its source and declared interactor as assets"
                        ),
                    ));
                }
                if !(100..=60_000).contains(&profile.idle_timeout_ms)
                    || !(1..=65_536).contains(&profile.transcript_limit_kib)
                {
                    issues.push(error_issue(
                        "interactive.runtime_limits_invalid",
                        format!(
                            "interactive profile {language} has an invalid idle timeout or transcript limit"
                        ),
                    ));
                }
                validate_harness_paths(
                    language,
                    profile
                        .asset_paths
                        .iter()
                        .chain(profile.include_dirs.iter()),
                    issues,
                );
                validate_nonempty_commands(
                    language,
                    [
                        profile.solver_compile_command.as_ref(),
                        profile.solver_run_command.as_ref(),
                        profile.interactor_compile_command.as_ref(),
                        profile.interactor_run_command.as_ref(),
                    ],
                    issues,
                );
            }
        }
        ExecutionHarnessV1::CustomImpl { profiles, .. } => {
            if profiles.is_empty() {
                issues.push(error_issue(
                    "grader.harness_invalid",
                    "custom implementation harness profiles are required".into(),
                ));
            }
            let grader_path = manifest.judging.grader_path.as_deref();
            for (language, profile) in profiles {
                if language.trim().is_empty()
                    || !profile.asset_paths.contains(&profile.source_path)
                    || grader_path
                        .is_none_or(|path| !profile.asset_paths.iter().any(|asset| asset == path))
                    || (profile.compile_script.is_none() && profile.compile_command.is_none())
                    || (profile.run_script.is_none() && profile.run_command.is_none())
                {
                    issues.push(error_issue(
                        "grader.profile_binding_invalid",
                        format!(
                            "custom implementation profile {language} must bind source, grader, compile, and run behavior"
                        ),
                    ));
                }
                validate_harness_paths(language, profile.asset_paths.iter(), issues);
                validate_nonempty_commands(
                    language,
                    [
                        profile.compile_command.as_ref(),
                        profile.run_command.as_ref(),
                    ],
                    issues,
                );
            }
        }
    }
}

fn validate_harness_paths<'a>(
    language: &str,
    paths: impl Iterator<Item = &'a String>,
    issues: &mut Vec<ValidationIssue>,
) {
    if paths
        .map(String::as_str)
        .any(|path| validate_relative_path(path).is_err())
    {
        issues.push(error_issue(
            "harness.path_invalid",
            format!("harness profile {language} contains an unsafe path"),
        ));
    }
}

fn validate_nonempty_commands<'a>(
    language: &str,
    commands: impl IntoIterator<Item = Option<&'a String>>,
    issues: &mut Vec<ValidationIssue>,
) {
    if commands
        .into_iter()
        .flatten()
        .any(|command| command.trim().is_empty())
    {
        issues.push(error_issue(
            "harness.command_empty",
            format!("harness profile {language} contains an empty command"),
        ));
    }
}

fn error_issue(code: &str, message: String) -> ValidationIssue {
    ValidationIssue {
        code: code.into(),
        severity: IssueSeverity::Error,
        message,
        path: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::*;
    use std::collections::BTreeMap;
    use std::str::FromStr;
    use uuid::Uuid;

    fn manifest() -> ReleaseManifestV1 {
        let statement = ManifestFile {
            path: "statements/ko.md".into(),
            sha256: Sha256Digest::from_str(&"0".repeat(64)).unwrap(),
            size_bytes: 10,
            media_type: "text/markdown".into(),
            executable: false,
        };
        let input = ManifestFile {
            path: "tests/1.in".into(),
            sha256: Sha256Digest::from_str(&"1".repeat(64)).unwrap(),
            size_bytes: 2,
            media_type: "text/plain".into(),
            executable: false,
        };
        let answer = ManifestFile {
            path: "tests/1.ans".into(),
            sha256: Sha256Digest::from_str(&"2".repeat(64)).unwrap(),
            size_bytes: 2,
            media_type: "text/plain".into(),
            executable: false,
        };
        ReleaseManifestV1 {
            schema: RELEASE_MANIFEST_SCHEMA_V1.into(),
            project_id: Uuid::now_v7(),
            commit_id: Uuid::now_v7(),
            problem_type: ProblemType::Standard,
            package_profile: PackageProfile::ReporchNative,
            default_locale: "ko".into(),
            title: BTreeMap::from([("ko".into(), "A + B".into())]),
            statements: BTreeMap::from([("ko".into(), statement.path.clone())]),
            files: vec![
                statement,
                input,
                answer,
                ManifestFile {
                    path: "solutions/accepted.py".into(),
                    sha256: Sha256Digest::from_str(&"3".repeat(64)).unwrap(),
                    size_bytes: 12,
                    media_type: "text/x-python".into(),
                    executable: false,
                },
                ManifestFile {
                    path: "solutions/accepted-alt.py".into(),
                    sha256: Sha256Digest::from_str(&"7".repeat(64)).unwrap(),
                    size_bytes: 16,
                    media_type: "text/x-python".into(),
                    executable: false,
                },
                ManifestFile {
                    path: "solutions/wrong.py".into(),
                    sha256: Sha256Digest::from_str(&"4".repeat(64)).unwrap(),
                    size_bytes: 12,
                    media_type: "text/x-python".into(),
                    executable: false,
                },
            ],
            toolchains: BTreeMap::new(),
            judging: JudgingSpec {
                limits: ResourceLimits {
                    time_ms: 1000,
                    memory_mib: 256,
                    output_kib: 1024,
                },
                checker: CheckerSpec::Token,
                tests: vec![TestCaseSpec {
                    id: Uuid::now_v7(),
                    name: "sample-1".into(),
                    input_file: "tests/1.in".into(),
                    answer_file: Some("tests/1.ans".into()),
                    groups: vec![],
                    generated_by: None,
                    generator_arguments: vec![],
                    seed: None,
                }],
                groups: vec![],
                generators: vec![],
                validator_path: None,
                validator_language: None,
                extra_validator_paths: vec![],
                extra_validators: vec![],
                validator_tests: vec![],
                checker_tests: vec![],
                interactor_path: None,
                interactor_language: None,
                grader_path: None,
                grader_language: None,
                harness: None,
            },
            sources: vec![],
            solutions: vec![
                SolutionSpec {
                    name: "accepted".into(),
                    source_path: "solutions/accepted.py".into(),
                    language: "python3".into(),
                    expected_verdict: ExpectedVerdict::Accepted,
                    expected_score: None,
                },
                SolutionSpec {
                    name: "accepted-alt".into(),
                    source_path: "solutions/accepted-alt.py".into(),
                    language: "python3".into(),
                    expected_verdict: ExpectedVerdict::Accepted,
                    expected_score: None,
                },
                SolutionSpec {
                    name: "known-wrong".into(),
                    source_path: "solutions/wrong.py".into(),
                    language: "python3".into(),
                    expected_verdict: ExpectedVerdict::WrongAnswer,
                    expected_score: None,
                },
            ],
            output_submissions: vec![],
            publication: Some(PublicationSpecV1 {
                category: "Algorithm".into(),
                difficulty: "Bronze 5".into(),
                grading_category: "algorithmic".into(),
                tags: vec![],
                allowed_languages: vec![],
                statement_sections: BTreeMap::from([(
                    "ko".into(),
                    StatementSectionsV1 {
                        input_format: "two integers".into(),
                        output_format: "their sum".into(),
                        note: String::new(),
                    },
                )]),
                samples: vec![PublicationSampleV1 {
                    name: "sample-1".into(),
                    input_file: "tests/1.in".into(),
                    output_file: "tests/1.ans".into(),
                }],
            }),
            policy_version: "studio-policy-v1".into(),
        }
    }

    #[test]
    fn valid_standard_manifest_passes_static_validation() {
        assert!(validate_manifest(&manifest()).is_empty());
    }

    #[test]
    fn accepted_cross_check_requires_two_distinct_implementations() {
        let mut value = manifest();
        value
            .solutions
            .retain(|solution| solution.name != "accepted-alt");
        assert!(
            validate_manifest(&value)
                .iter()
                .any(|issue| { issue.code == "solutions.accepted_cross_check_missing" })
        );

        let mut value = manifest();
        let accepted_digest = value
            .files
            .iter()
            .find(|file| file.path == "solutions/accepted.py")
            .unwrap()
            .sha256
            .clone();
        value
            .files
            .iter_mut()
            .find(|file| file.path == "solutions/accepted-alt.py")
            .unwrap()
            .sha256 = accepted_digest;
        assert!(
            validate_manifest(&value)
                .iter()
                .any(|issue| { issue.code == "solutions.accepted_cross_check_missing" })
        );
    }

    #[test]
    fn execution_limits_are_bounded_before_sandbox_admission() {
        let mut value = manifest();
        value.judging.limits.time_ms = MAX_TIME_LIMIT_MS + 1;
        assert!(
            validate_manifest(&value)
                .iter()
                .any(|issue| { issue.code == "judging.invalid_limits" })
        );

        let mut value = manifest();
        value.judging.limits.output_kib = 0;
        assert!(
            validate_manifest(&value)
                .iter()
                .any(|issue| { issue.code == "judging.invalid_limits" })
        );
    }

    #[test]
    fn external_sources_require_license_and_safe_canonical_url() {
        let mut value = manifest();
        value.sources.push(crate::SourceAttribution {
            provider: "polygon".into(),
            external_id: "123".into(),
            canonical_url: "http://example.test/problem/123".into(),
            license_name: String::new(),
            attribution: String::new(),
        });
        assert!(
            validate_manifest(&value)
                .iter()
                .any(|issue| issue.code == "sources.invalid_attribution")
        );

        value.sources[0].canonical_url = "https://example.test/problem/123".into();
        value.sources[0].license_name = "CC BY 4.0".into();
        value.sources[0].attribution = "Original author".into();
        assert!(validate_manifest(&value).is_empty());
    }

    #[test]
    fn duplicate_test_inputs_are_rejected_by_digest() {
        let mut value = manifest();
        let mut duplicate_file = value
            .files
            .iter()
            .find(|file| file.path == "tests/1.in")
            .unwrap()
            .clone();
        duplicate_file.path = "tests/duplicate.in".into();
        value.files.push(duplicate_file);
        let mut duplicate_test = value.judging.tests[0].clone();
        duplicate_test.id = Uuid::now_v7();
        duplicate_test.name = "duplicate".into();
        duplicate_test.input_file = "tests/duplicate.in".into();
        value.judging.tests.push(duplicate_test);

        assert!(
            validate_manifest(&value)
                .iter()
                .any(|issue| issue.code == "tests.duplicate_input")
        );
    }

    #[test]
    fn statement_markdown_rejects_raw_html_and_unsafe_urls() {
        let issues = validate_statement_markdown(
            "# Title\n<script>alert(1)</script>\n[click](javascript:alert(1))",
        );
        assert!(
            issues
                .iter()
                .any(|issue| issue.code == "statement.raw_html_forbidden")
        );
        assert!(
            issues
                .iter()
                .any(|issue| issue.code == "statement.unsafe_url")
        );
    }

    #[test]
    fn statement_markdown_accepts_tables_math_and_relative_images() {
        let issues = validate_statement_markdown(
            "# Sum\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n$x+y$\n\n![plot](assets/plot.png)",
        );
        assert!(issues.is_empty(), "{issues:?}");
    }

    #[test]
    fn statement_markdown_rejects_external_images() {
        let issues = validate_statement_markdown("![tracking](https://example.test/pixel.png)");
        assert!(
            issues
                .iter()
                .any(|issue| issue.code == "statement.external_image_forbidden")
        );
    }

    #[test]
    fn statement_image_paths_are_normalized_and_strictly_bounded() {
        let paths = statement_image_paths(
            "![first](./assets/plot.png)\n\n![same](assets/plot.png)\n\n![diagram](images/a.svg)",
        )
        .unwrap();
        assert_eq!(
            paths,
            BTreeSet::from(["assets/plot.png".into(), "images/a.svg".into()])
        );

        for markdown in [
            "![escape](../secret.png)",
            "![query](assets/plot.png?token=secret)",
            "![unsupported](assets/archive.zip)",
        ] {
            assert!(
                validate_statement_markdown(markdown)
                    .iter()
                    .any(|issue| issue.code == "statement.invalid_image_path"),
                "{markdown}"
            );
        }
    }

    #[test]
    fn statement_html_is_deterministic_and_csp_locked() {
        let markdown = "# Sum\n\n$x+y$\n\n![plot](assets/plot.png)\n";
        let first = render_statement_html(markdown).unwrap();
        let second = render_statement_html(markdown).unwrap();
        assert_eq!(first, second);
        assert!(first.starts_with("<!doctype html>\n"));
        assert!(first.contains("default-src 'none'"));
        assert!(first.contains("<h1>Sum</h1>"));
        assert!(!first.contains("<script"));
    }

    #[test]
    fn interactive_manifest_requires_interactor() {
        let mut value = manifest();
        value.problem_type = ProblemType::Interactive;
        assert!(
            validate_manifest(&value)
                .iter()
                .any(|issue| issue.code == "interactive.interactor_missing")
        );
    }

    #[test]
    fn interactive_manifest_accepts_typed_harness() {
        let mut value = manifest();
        value.problem_type = ProblemType::Interactive;
        for (path, digest) in [
            ("templates/cpp/solution.cpp", "5".repeat(64)),
            ("interactive/interactor.cpp", "6".repeat(64)),
        ] {
            value.files.push(ManifestFile {
                path: path.into(),
                sha256: Sha256Digest::from_str(&digest).unwrap(),
                size_bytes: 10,
                media_type: "text/x-c++src".into(),
                executable: false,
            });
        }
        value.judging.interactor_path = Some("interactive/interactor.cpp".into());
        value.judging.interactor_language = Some("cpp17".into());
        value.judging.harness = Some(ExecutionHarnessV1::InteractiveStdio {
            profiles: BTreeMap::from([(
                "cpp17".into(),
                InteractiveStdioProfileV1 {
                    source_path: "templates/cpp/solution.cpp".into(),
                    interactor_source_path: "interactive/interactor.cpp".into(),
                    asset_paths: vec![
                        "templates/cpp/solution.cpp".into(),
                        "interactive/interactor.cpp".into(),
                    ],
                    include_dirs: vec![],
                    idle_timeout_ms: 2_000,
                    transcript_limit_kib: 1_024,
                    solver_compile_command: None,
                    solver_run_command: None,
                    interactor_compile_command: None,
                    interactor_run_command: None,
                },
            )]),
            score_type: ScoreAggregation::GroupMin,
            score_scale: 100,
        });
        assert!(validate_manifest(&value).is_empty());
    }

    #[test]
    fn library_manifest_requires_typed_grader_harness() {
        let mut value = manifest();
        value.problem_type = ProblemType::Library;
        value.judging.grader_path = Some("grader.cpp".into());
        value.judging.grader_language = Some("cpp17".into());
        assert!(
            validate_manifest(&value)
                .iter()
                .any(|issue| issue.code == "grader.harness_missing")
        );
    }

    #[test]
    fn validator_requires_positive_and_negative_unit_cases() {
        let mut value = manifest();
        value.judging.validator_path = Some("validator.py".into());
        assert!(
            validate_manifest(&value)
                .iter()
                .any(|issue| issue.code == "validator.unit_matrix_incomplete")
        );
    }

    #[test]
    fn custom_checker_requires_accept_and_reject_unit_cases() {
        let mut value = manifest();
        value.judging.checker = CheckerSpec::Custom {
            source_path: "checker.py".into(),
            language: "python3".into(),
            protocol: crate::CheckerProtocolV1::Icpc202509,
        };
        assert!(
            validate_manifest(&value)
                .iter()
                .any(|issue| issue.code == "checker.unit_matrix_incomplete")
        );
    }

    #[test]
    fn generated_tests_must_reference_declared_generator() {
        let mut value = manifest();
        value.judging.tests[0].generated_by = Some("missing".into());
        value.judging.tests[0].seed = Some(7);
        assert!(
            validate_manifest(&value)
                .iter()
                .any(|issue| issue.code == "tests.generator_unknown")
        );
    }

    #[test]
    fn output_only_requires_complete_accepted_and_negative_outputs() {
        let mut value = manifest();
        value.problem_type = ProblemType::OutputOnly;
        value.solutions.clear();
        let test_id = value.judging.tests[0].id;
        for (path, digest) in [
            ("outputs/accepted.txt", "5".repeat(64)),
            ("outputs/wrong.txt", "6".repeat(64)),
        ] {
            value.files.push(ManifestFile {
                path: path.into(),
                sha256: Sha256Digest::from_str(&digest).unwrap(),
                size_bytes: 2,
                media_type: "text/plain".into(),
                executable: false,
            });
        }
        value.output_submissions = vec![
            OutputSubmissionSpec {
                name: "official".into(),
                outputs: BTreeMap::from([(test_id, "outputs/accepted.txt".into())]),
                expected_verdict: ExpectedVerdict::Accepted,
                expected_score: None,
            },
            OutputSubmissionSpec {
                name: "known-wrong".into(),
                outputs: BTreeMap::from([(test_id, "outputs/wrong.txt".into())]),
                expected_verdict: ExpectedVerdict::WrongAnswer,
                expected_score: None,
            },
        ];
        assert!(validate_manifest(&value).is_empty());
    }
}
