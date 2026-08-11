use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{CheckerSpec, ProblemType, ReleaseManifestV1};

/// Stable, ordered validation stages for `reporch.validation-report.v1`.
///
/// The discriminants are part of the public package contract. New behavior is
/// introduced through a new report schema rather than by reordering this list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ValidationStageV1 {
    ManifestSchemaPolicy,
    StatementRendering,
    ProgramCompilation,
    ValidatorUnitTests,
    GeneratorDeterminism,
    TestDataIntegrity,
    AcceptedSolutionCrossCheck,
    SolutionMatrix,
    CheckerRobustness,
    InteractiveProtocol,
    ScoringConsistency,
    LimitCalibration,
    PackageReproducibility,
    ProvenanceReviewPolicy,
}

impl ValidationStageV1 {
    pub const ORDERED: [Self; 14] = [
        Self::ManifestSchemaPolicy,
        Self::StatementRendering,
        Self::ProgramCompilation,
        Self::ValidatorUnitTests,
        Self::GeneratorDeterminism,
        Self::TestDataIntegrity,
        Self::AcceptedSolutionCrossCheck,
        Self::SolutionMatrix,
        Self::CheckerRobustness,
        Self::InteractiveProtocol,
        Self::ScoringConsistency,
        Self::LimitCalibration,
        Self::PackageReproducibility,
        Self::ProvenanceReviewPolicy,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ManifestSchemaPolicy => "manifest_schema_policy",
            Self::StatementRendering => "statement_rendering",
            Self::ProgramCompilation => "program_compilation",
            Self::ValidatorUnitTests => "validator_unit_tests",
            Self::GeneratorDeterminism => "generator_determinism",
            Self::TestDataIntegrity => "test_data_integrity",
            Self::AcceptedSolutionCrossCheck => "accepted_solution_cross_check",
            Self::SolutionMatrix => "solution_matrix",
            Self::CheckerRobustness => "checker_robustness",
            Self::InteractiveProtocol => "interactive_protocol",
            Self::ScoringConsistency => "scoring_consistency",
            Self::LimitCalibration => "limit_calibration",
            Self::PackageReproducibility => "package_reproducibility",
            Self::ProvenanceReviewPolicy => "provenance_review_policy",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ValidationStagePlanV1 {
    pub stage: ValidationStageV1,
    pub required: bool,
    /// Stable machine-readable explanation for an omitted conditional stage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_required_reason: Option<String>,
}

/// Produces all 14 stages in contract order. Conditional stages remain in the
/// plan so reports cannot silently omit evidence merely because a problem type
/// does not use a tool.
pub fn validation_plan(manifest: &ReleaseManifestV1) -> Vec<ValidationStagePlanV1> {
    let has_program = !manifest.solutions.is_empty()
        || !manifest.judging.generators.is_empty()
        || manifest.judging.validator_path.is_some()
        || !manifest.judging.extra_validator_paths.is_empty()
        || !manifest.judging.extra_validators.is_empty()
        || matches!(manifest.judging.checker, CheckerSpec::Custom { .. })
        || manifest.judging.interactor_path.is_some()
        || manifest.judging.grader_path.is_some()
        || manifest.judging.harness.is_some();
    let has_validator = manifest.judging.validator_path.is_some()
        || !manifest.judging.extra_validator_paths.is_empty()
        || !manifest.judging.extra_validators.is_empty();
    let has_solution_matrix =
        !manifest.solutions.is_empty() || !manifest.output_submissions.is_empty();

    ValidationStageV1::ORDERED
        .into_iter()
        .map(|stage| {
            let (required, reason) = match stage {
                ValidationStageV1::ProgramCompilation => {
                    (has_program, "manifest_declares_no_executable_program")
                }
                ValidationStageV1::ValidatorUnitTests => {
                    (has_validator, "manifest_declares_no_validator")
                }
                ValidationStageV1::GeneratorDeterminism => (
                    !manifest.judging.generators.is_empty(),
                    "manifest_declares_no_generator",
                ),
                ValidationStageV1::AcceptedSolutionCrossCheck => (
                    !manifest.solutions.is_empty(),
                    "problem_has_no_executable_reference_solution",
                ),
                ValidationStageV1::SolutionMatrix => {
                    (has_solution_matrix, "manifest_declares_no_solution_matrix")
                }
                ValidationStageV1::CheckerRobustness => (
                    matches!(manifest.judging.checker, CheckerSpec::Custom { .. }),
                    "manifest_uses_a_builtin_checker",
                ),
                ValidationStageV1::InteractiveProtocol => (
                    manifest.problem_type == ProblemType::Interactive,
                    "problem_is_not_interactive",
                ),
                ValidationStageV1::LimitCalibration => (
                    manifest.problem_type != ProblemType::OutputOnly,
                    "output_only_problem_has_no_submission_runtime_limit",
                ),
                _ => (true, ""),
            };
            ValidationStagePlanV1 {
                stage,
                required,
                not_required_reason: (!required).then(|| reason.to_owned()),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ExpectedVerdict, JudgingSpec, PackageProfile, RELEASE_MANIFEST_SCHEMA_V1, ResourceLimits,
        SolutionSpec,
    };
    use std::collections::BTreeMap;
    use uuid::Uuid;

    fn manifest(problem_type: ProblemType) -> ReleaseManifestV1 {
        ReleaseManifestV1 {
            schema: RELEASE_MANIFEST_SCHEMA_V1.into(),
            project_id: Uuid::now_v7(),
            commit_id: Uuid::now_v7(),
            problem_type,
            package_profile: PackageProfile::ReporchNative,
            default_locale: "ko".into(),
            title: BTreeMap::from([("ko".into(), "title".into())]),
            statements: BTreeMap::new(),
            files: vec![],
            toolchains: BTreeMap::new(),
            judging: JudgingSpec {
                limits: ResourceLimits {
                    time_ms: 1_000,
                    memory_mib: 256,
                    output_kib: 64,
                },
                checker: CheckerSpec::Exact,
                tests: vec![],
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
            solutions: vec![],
            output_submissions: vec![],
            publication: None,
            policy_version: "policy-v1".into(),
        }
    }

    #[test]
    fn always_emits_the_versioned_fourteen_stage_order() {
        let plan = validation_plan(&manifest(ProblemType::Standard));
        assert_eq!(plan.len(), 14);
        assert_eq!(
            plan.iter().map(|item| item.stage).collect::<Vec<_>>(),
            ValidationStageV1::ORDERED
        );
    }

    #[test]
    fn marks_conditional_program_stages_without_omitting_them() {
        let mut manifest = manifest(ProblemType::Interactive);
        manifest.solutions.push(SolutionSpec {
            name: "accepted".into(),
            source_path: "solutions/accepted.cpp".into(),
            language: "cpp".into(),
            expected_verdict: ExpectedVerdict::Accepted,
            expected_score: None,
        });
        manifest.judging.checker = CheckerSpec::Custom {
            source_path: "checker.cpp".into(),
            language: "cpp".into(),
        };

        let plan = validation_plan(&manifest);
        let required = |stage| {
            plan.iter()
                .find(|item| item.stage == stage)
                .expect("stage exists")
                .required
        };
        assert!(required(ValidationStageV1::ProgramCompilation));
        assert!(required(ValidationStageV1::AcceptedSolutionCrossCheck));
        assert!(required(ValidationStageV1::CheckerRobustness));
        assert!(required(ValidationStageV1::InteractiveProtocol));
        assert!(!required(ValidationStageV1::GeneratorDeterminism));
    }
}
