use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{ExpectedVerdict, PackageProfile, ProblemType, ReleaseManifestV1};

pub const COMPATIBILITY_REPORT_SCHEMA_V1: &str = "reporch.compatibility-report.v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CompatibilitySeverity {
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CompatibilityIssueV1 {
    pub code: String,
    pub severity: CompatibilitySeverity,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CompatibilityReportV1 {
    pub schema: String,
    pub source_profile: PackageProfile,
    pub target_profile: PackageProfile,
    pub exportable: bool,
    pub lossless: bool,
    pub issues: Vec<CompatibilityIssueV1>,
}

pub fn compatibility_report(
    manifest: &ReleaseManifestV1,
    target: PackageProfile,
) -> CompatibilityReportV1 {
    let mut issues = Vec::new();
    if target == PackageProfile::ReporchNative {
        return report(manifest, target, issues);
    }
    let icpc_based = matches!(
        target,
        PackageProfile::Icpc202509 | PackageProfile::DomjudgeZip
    );

    if !manifest.toolchains.is_empty() {
        warning(
            &mut issues,
            "compatibility.toolchain_digest_extension",
            "versioned Reporch toolchain digests require a sidecar extension",
            None,
        );
    }
    if manifest.publication.is_some() {
        warning(
            &mut issues,
            "compatibility.reporch_catalog_metadata",
            "Reporch category, difficulty, and grading metadata are not portable",
            Some("publication"),
        );
    }
    if !icpc_based
        && manifest
            .solutions
            .iter()
            .any(|solution| solution.expected_score.is_some())
    {
        warning(
            &mut issues,
            "compatibility.solution_score_range",
            "exact expected solution score ranges require a Reporch sidecar",
            Some("solutions"),
        );
    }
    if !icpc_based
        && (!manifest.judging.validator_tests.is_empty()
            || !manifest.judging.checker_tests.is_empty())
    {
        warning(
            &mut issues,
            "compatibility.tool_unit_matrix_sidecar",
            "validator and checker unit expectations require a Reporch sidecar",
            Some("judging"),
        );
    }
    if manifest
        .judging
        .generators
        .iter()
        .any(|generator| !generator.arguments.is_empty())
        || manifest
            .judging
            .tests
            .iter()
            .any(|test| test.generated_by.is_some() || test.seed.is_some())
    {
        warning(
            &mut issues,
            "compatibility.generator_recipe_sidecar",
            "fixed generator arguments and seeds require a Reporch sidecar",
            Some("judging.generators"),
        );
    }
    if !icpc_based && !manifest.judging.groups.is_empty() {
        warning(
            &mut issues,
            "compatibility.group_identity_sidecar",
            "native group identifiers and dependency metadata require a Reporch sidecar",
            Some("judging.groups"),
        );
    }
    if manifest
        .solutions
        .iter()
        .any(|solution| solution.expected_verdict == ExpectedVerdict::Partial)
        && matches!(target, PackageProfile::IcpcLegacy)
    {
        warning(
            &mut issues,
            "compatibility.partial_solution_tag",
            "legacy submission directories cannot preserve an exact partial verdict",
            Some("solutions"),
        );
    }

    match target {
        PackageProfile::Icpc202509 => {
            reject_library_grader(manifest, &mut issues, "ICPC 2025-09");
            require_validator(manifest, &mut issues, "ICPC 2025-09");
            validate_icpc_group_mapping(manifest, &mut issues);
            if manifest.problem_type == ProblemType::OutputOnly {
                warning(
                    &mut issues,
                    "compatibility.icpc_submit_answer_mapping_extension",
                    "ICPC submit-answer does not standardize per-test example-output mapping; a checksummed Reporch sidecar is included for lossless round trips",
                    Some("output_submissions"),
                );
                if manifest.output_submissions.iter().any(|submission| {
                    submission.expected_score.is_some()
                        || submission.expected_verdict == ExpectedVerdict::Partial
                }) {
                    error(
                        &mut issues,
                        "compatibility.icpc_scored_submit_answer_unsupported",
                        "scored submit-answer export is not enabled until native output-only score aggregation is explicit",
                        Some("output_submissions"),
                    );
                }
            }
            if manifest.problem_type == ProblemType::Scored
                && manifest.judging.groups.iter().any(|group| {
                    !group.points.is_finite() || group.points < 0.0 || group.points.fract() != 0.0
                })
            {
                error(
                    &mut issues,
                    "compatibility.icpc_integer_group_score",
                    "ICPC 2025-09 test-group maximum scores must be integers",
                    Some("judging.groups"),
                );
            }
            if manifest
                .solutions
                .iter()
                .any(|solution| solution.expected_verdict == ExpectedVerdict::MemoryLimit)
            {
                warning(
                    &mut issues,
                    "compatibility.icpc_memory_limit_tag",
                    "ICPC 2025-09 has no default MLE submission directory; the solution is exported as generically rejected",
                    Some("solutions"),
                );
            }
        }
        PackageProfile::IcpcLegacy => {
            if manifest.problem_type != ProblemType::Standard {
                error(
                    &mut issues,
                    "compatibility.legacy_icpc_problem_type",
                    "the legacy ICPC subset is restricted to standard pass/fail export",
                    Some("problem_type"),
                );
            }
            require_validator(manifest, &mut issues, "legacy ICPC");
            warning(
                &mut issues,
                "compatibility.legacy_statement_render",
                "Markdown statements must be rendered to a legacy statement artifact",
                Some("statements"),
            );
            warning(
                &mut issues,
                "compatibility.legacy_time_limit_policy",
                "legacy ICPC expresses time scaling rather than an absolute judge time limit; the exact Reporch limit is preserved only in the checksummed extension",
                Some("judging.limits.time_ms"),
            );
            if manifest.title.len() > 1 {
                warning(
                    &mut issues,
                    "compatibility.legacy_localized_title",
                    "legacy problem.yaml preserves only one localized title",
                    Some("title"),
                );
            }
            if !manifest.judging.generators.is_empty() {
                warning(
                    &mut issues,
                    "compatibility.legacy_generator_extension",
                    "the official legacy ICPC subset has no portable generator recipe; Studio preserves generators only in its checksummed extension",
                    Some("judging.generators"),
                );
            }
            if manifest.solutions.iter().any(|solution| {
                matches!(
                    solution.expected_verdict,
                    ExpectedVerdict::MemoryLimit | ExpectedVerdict::Partial
                )
            }) {
                warning(
                    &mut issues,
                    "compatibility.legacy_solution_verdict_extension",
                    "legacy ICPC submission directories cannot represent MLE or partial verdicts exactly; Studio preserves the exact verdict in its checksummed extension",
                    Some("solutions"),
                );
            }
            if manifest.files.iter().any(|file| {
                file.path == "problem.yaml"
                    || file.path == "reporch.problem.json"
                    || file.path == "reporch_compatibility.json"
                    || [
                        "problem_statement/",
                        "data/",
                        "submissions/",
                        "input_validators/",
                        "output_validators/",
                        "reporch_legacy/",
                    ]
                    .iter()
                    .any(|prefix| file.path.starts_with(prefix))
            }) {
                error(
                    &mut issues,
                    "compatibility.legacy_reserved_path",
                    "native files collide with paths reserved by the legacy ICPC projection or Reporch extension",
                    Some("files"),
                );
            }
        }
        PackageProfile::PolygonCompatible => {
            reject_library_grader(manifest, &mut issues, "Polygon");
            if manifest.problem_type == ProblemType::OutputOnly {
                error(
                    &mut issues,
                    "compatibility.polygon_output_only",
                    "the Polygon-compatible profile does not preserve output-only submissions",
                    Some("problem_type"),
                );
            }
            require_validator(manifest, &mut issues, "Polygon");
            if !(250..=15_000).contains(&manifest.judging.limits.time_ms)
                || !manifest.judging.limits.time_ms.is_multiple_of(50)
            {
                error(
                    &mut issues,
                    "compatibility.polygon_time_limit",
                    "Polygon time limits must be between 250 and 15000 milliseconds and divisible by 50",
                    Some("judging.limits.time_ms"),
                );
            }
            if !(4..=1_024).contains(&manifest.judging.limits.memory_mib) {
                error(
                    &mut issues,
                    "compatibility.polygon_memory_limit",
                    "Polygon memory limits must be between 4 and 1024 MiB",
                    Some("judging.limits.memory_mib"),
                );
            }
            if manifest
                .judging
                .tests
                .iter()
                .any(|test| test.groups.len() > 1)
            {
                error(
                    &mut issues,
                    "compatibility.polygon_overlapping_groups",
                    "Polygon assigns at most one group to a test in one testset",
                    Some("judging.tests"),
                );
            }
            if manifest.files.iter().any(|file| {
                file.path == "problem.xml"
                    || file.path == "reporch.problem.json"
                    || file.path.starts_with("-reporch-polygon/")
            }) {
                error(
                    &mut issues,
                    "compatibility.polygon_reserved_path",
                    "native files collide with paths reserved by the Polygon interchange package",
                    Some("files"),
                );
            }
            warning(
                &mut issues,
                "compatibility.polygon_statement_transform",
                "Markdown statements require a deterministic Polygon statement transform",
                Some("statements"),
            );
        }
        PackageProfile::DomjudgeZip => {
            if matches!(
                manifest.problem_type,
                ProblemType::OutputOnly | ProblemType::Library | ProblemType::Grader
            ) {
                error(
                    &mut issues,
                    "compatibility.domjudge_problem_type",
                    "the DOMjudge ZIP profile cannot preserve this problem type",
                    Some("problem_type"),
                );
            }
            require_validator(manifest, &mut issues, "DOMjudge");
            validate_icpc_group_mapping(manifest, &mut issues);
            if manifest.problem_type == ProblemType::Scored
                && manifest.judging.groups.iter().any(|group| {
                    !group.points.is_finite() || group.points < 0.0 || group.points.fract() != 0.0
                })
            {
                error(
                    &mut issues,
                    "compatibility.domjudge_integer_group_score",
                    "DOMjudge ICPC test-group maximum scores must be integers",
                    Some("judging.groups"),
                );
            }
        }
        PackageProfile::ReporchNative => unreachable!(),
    }
    report(manifest, target, issues)
}

fn validate_icpc_group_mapping(
    manifest: &ReleaseManifestV1,
    issues: &mut Vec<CompatibilityIssueV1>,
) {
    let mut mapped = std::collections::BTreeSet::new();
    let mut transformed = false;
    for group in &manifest.judging.groups {
        let component = icpc_component(&group.id);
        transformed |= component != group.id;
        if component.is_empty() || !mapped.insert(component) {
            error(
                issues,
                "compatibility.icpc_group_path_collision",
                "test-group identifiers collide or become empty when mapped to ICPC paths",
                Some("judging.groups"),
            );
            break;
        }
    }
    if transformed {
        warning(
            issues,
            "compatibility.icpc_group_identifier_transform",
            "some native test-group identifiers are normalized for ICPC directory paths",
            Some("judging.groups"),
        );
    }
    if manifest
        .judging
        .tests
        .iter()
        .any(|test| test.groups.len() > 1)
    {
        error(
            issues,
            "compatibility.icpc_overlapping_groups",
            "a native test assigned to multiple groups cannot be exported without duplicating and changing scoring semantics",
            Some("judging.tests"),
        );
    }
}

fn icpc_component(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches(['.', '-'])
        .to_owned()
}

fn reject_library_grader(
    manifest: &ReleaseManifestV1,
    issues: &mut Vec<CompatibilityIssueV1>,
    target: &str,
) {
    if matches!(
        manifest.problem_type,
        ProblemType::Library | ProblemType::Grader
    ) {
        error(
            issues,
            "compatibility.library_grader_harness",
            &format!("{target} has no lossless mapping for the typed grader harness"),
            Some("judging.harness"),
        );
    }
}

fn require_validator(
    manifest: &ReleaseManifestV1,
    issues: &mut Vec<CompatibilityIssueV1>,
    target: &str,
) {
    if manifest.judging.validator_path.is_none()
        && manifest.judging.extra_validator_paths.is_empty()
        && manifest.judging.extra_validators.is_empty()
    {
        error(
            issues,
            "compatibility.input_validator_required",
            &format!("{target} export requires an explicit input validator"),
            Some("judging.validator_path"),
        );
    }
}

fn report(
    manifest: &ReleaseManifestV1,
    target: PackageProfile,
    issues: Vec<CompatibilityIssueV1>,
) -> CompatibilityReportV1 {
    CompatibilityReportV1 {
        schema: COMPATIBILITY_REPORT_SCHEMA_V1.into(),
        source_profile: manifest.package_profile,
        target_profile: target,
        exportable: !issues
            .iter()
            .any(|issue| issue.severity == CompatibilitySeverity::Error),
        lossless: issues.is_empty(),
        issues,
    }
}

fn warning(issues: &mut Vec<CompatibilityIssueV1>, code: &str, message: &str, path: Option<&str>) {
    issue(issues, CompatibilitySeverity::Warning, code, message, path);
}

fn error(issues: &mut Vec<CompatibilityIssueV1>, code: &str, message: &str, path: Option<&str>) {
    issue(issues, CompatibilitySeverity::Error, code, message, path);
}

fn issue(
    issues: &mut Vec<CompatibilityIssueV1>,
    severity: CompatibilitySeverity,
    code: &str,
    message: &str,
    path: Option<&str>,
) {
    issues.push(CompatibilityIssueV1 {
        code: code.into(),
        severity,
        message: message.into(),
        path: path.map(str::to_owned),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> ReleaseManifestV1 {
        serde_json::from_value(serde_json::json!({
            "schema": "reporch.release-manifest.v1",
            "project_id": uuid::Uuid::now_v7(),
            "commit_id": uuid::Uuid::now_v7(),
            "problem_type": "standard",
            "package_profile": "reporch_native",
            "default_locale": "ko",
            "title": {"ko": "fixture"},
            "statements": {},
            "files": [],
            "toolchains": {},
            "judging": {
                "limits": {"time_ms": 1000, "memory_mib": 256, "output_kib": 1024},
                "checker": {"kind": "token"}
            },
            "sources": [],
            "solutions": [],
            "policy_version": "studio-policy-v1"
        }))
        .unwrap()
    }

    #[test]
    fn native_profile_is_lossless() {
        let report = compatibility_report(&manifest(), PackageProfile::ReporchNative);
        assert!(report.exportable);
        assert!(report.lossless);
        assert!(report.issues.is_empty());
    }

    #[test]
    fn icpc_profile_fails_closed_without_a_validator() {
        let report = compatibility_report(&manifest(), PackageProfile::Icpc202509);
        assert!(!report.exportable);
        assert!(report.issues.iter().any(|issue| {
            issue.code == "compatibility.input_validator_required"
                && issue.severity == CompatibilitySeverity::Error
        }));
    }

    #[test]
    fn explicit_validator_makes_basic_icpc_exportable() {
        let mut manifest = manifest();
        manifest.judging.validator_path = Some("validators/main.py".into());
        let report = compatibility_report(&manifest, PackageProfile::Icpc202509);
        assert!(report.exportable);
    }

    #[test]
    fn icpc_submit_answer_is_exportable_with_a_mapping_sidecar() {
        let mut manifest = manifest();
        manifest.problem_type = ProblemType::OutputOnly;
        manifest.judging.validator_path = Some("validators/main.py".into());

        let report = compatibility_report(&manifest, PackageProfile::Icpc202509);

        assert!(report.exportable);
        assert!(!report.lossless);
        assert!(report.issues.iter().any(|issue| {
            issue.code == "compatibility.icpc_submit_answer_mapping_extension"
                && issue.severity == CompatibilitySeverity::Warning
        }));
    }

    #[test]
    fn icpc_scored_submit_answer_fails_closed_until_score_semantics_are_explicit() {
        let mut manifest = manifest();
        manifest.problem_type = ProblemType::OutputOnly;
        manifest.judging.validator_path = Some("validators/main.py".into());
        manifest
            .output_submissions
            .push(crate::OutputSubmissionSpec {
                name: "partial".into(),
                outputs: std::collections::BTreeMap::new(),
                expected_verdict: ExpectedVerdict::Partial,
                expected_score: Some(crate::ExpectedScoreRange {
                    minimum: 40.0,
                    maximum: 60.0,
                }),
            });

        let report = compatibility_report(&manifest, PackageProfile::Icpc202509);

        assert!(!report.exportable);
        assert!(report.issues.iter().any(|issue| {
            issue.code == "compatibility.icpc_scored_submit_answer_unsupported"
                && issue.severity == CompatibilitySeverity::Error
        }));
    }

    #[test]
    fn external_profiles_reject_typed_grader_harness_loss() {
        let mut manifest = manifest();
        manifest.problem_type = ProblemType::Grader;
        manifest.judging.validator_path = Some("validators/main.py".into());
        let report = compatibility_report(&manifest, PackageProfile::PolygonCompatible);
        assert!(!report.exportable);
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.code == "compatibility.library_grader_harness")
        );
    }

    #[test]
    fn polygon_enforces_hosted_limit_and_single_group_constraints() {
        let mut manifest = manifest();
        manifest.judging.validator_path = Some("validators/main.py".into());
        manifest.judging.limits.time_ms = 251;
        manifest.judging.limits.memory_mib = 2_048;
        manifest.judging.tests.push(crate::TestCaseSpec {
            id: uuid::Uuid::now_v7(),
            name: "overlap".into(),
            input_file: "tests/1.in".into(),
            answer_file: Some("tests/1.ans".into()),
            groups: vec!["one".into(), "two".into()],
            generated_by: None,
            generator_arguments: vec![],
            seed: None,
        });

        let report = compatibility_report(&manifest, PackageProfile::PolygonCompatible);

        for code in [
            "compatibility.polygon_time_limit",
            "compatibility.polygon_memory_limit",
            "compatibility.polygon_overlapping_groups",
        ] {
            assert!(report.issues.iter().any(|issue| issue.code == code));
        }
        assert!(!report.exportable);
    }

    #[test]
    fn icpc_scoring_requires_integer_group_scores() {
        let mut manifest = manifest();
        manifest.problem_type = ProblemType::Scored;
        manifest.judging.validator_path = Some("validators/main.py".into());
        manifest.judging.groups.push(crate::TestGroupSpec {
            id: "subtask".into(),
            points: 12.5,
            depends_on: vec![],
            feedback_policy: crate::GroupFeedbackPolicyV1::Complete,
        });

        let report = compatibility_report(&manifest, PackageProfile::Icpc202509);

        assert!(!report.exportable);
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.code == "compatibility.icpc_integer_group_score")
        );
    }

    #[test]
    fn icpc_rejects_overlapping_test_groups() {
        let mut manifest = manifest();
        manifest.problem_type = ProblemType::Scored;
        manifest.judging.validator_path = Some("validators/main.py".into());
        manifest.judging.groups = vec![
            crate::TestGroupSpec {
                id: "one".into(),
                points: 50.0,
                depends_on: vec![],
                feedback_policy: crate::GroupFeedbackPolicyV1::Complete,
            },
            crate::TestGroupSpec {
                id: "two".into(),
                points: 50.0,
                depends_on: vec![],
                feedback_policy: crate::GroupFeedbackPolicyV1::Complete,
            },
        ];
        manifest.judging.tests.push(crate::TestCaseSpec {
            id: uuid::Uuid::now_v7(),
            name: "overlap".into(),
            input_file: "tests/1.in".into(),
            answer_file: Some("tests/1.ans".into()),
            groups: vec!["one".into(), "two".into()],
            generated_by: None,
            generator_arguments: vec![],
            seed: None,
        });

        let report = compatibility_report(&manifest, PackageProfile::Icpc202509);

        assert!(!report.exportable);
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.code == "compatibility.icpc_overlapping_groups")
        );
    }

    #[test]
    fn icpc_rejects_group_identifier_path_collisions() {
        let mut manifest = manifest();
        manifest.problem_type = ProblemType::Scored;
        manifest.judging.validator_path = Some("validators/main.py".into());
        manifest.judging.groups = vec![
            crate::TestGroupSpec {
                id: "a/b".into(),
                points: 50.0,
                depends_on: vec![],
                feedback_policy: crate::GroupFeedbackPolicyV1::Complete,
            },
            crate::TestGroupSpec {
                id: "a?b".into(),
                points: 50.0,
                depends_on: vec![],
                feedback_policy: crate::GroupFeedbackPolicyV1::Complete,
            },
        ];

        let report = compatibility_report(&manifest, PackageProfile::Icpc202509);

        assert!(!report.exportable);
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.code == "compatibility.icpc_group_path_collision")
        );
    }
}
