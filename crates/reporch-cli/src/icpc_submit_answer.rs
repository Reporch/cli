use serde::{Deserialize, Serialize};
use studio_core::{ExpectedScoreRange, ExpectedVerdict};
use uuid::Uuid;

pub(crate) const SIDECAR_PATH: &str = "-reporch-submit-answer.json";
pub(crate) const SIDECAR_SCHEMA_V1: &str = "reporch.icpc-submit-answer.v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SubmitAnswerSidecarV1 {
    pub schema: String,
    pub tests: Vec<SubmitAnswerTestV1>,
    pub submissions: Vec<SubmitAnswerSubmissionV1>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SubmitAnswerTestV1 {
    pub test_id: Uuid,
    pub test_name: String,
    pub test_index: usize,
    pub input_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SubmitAnswerSubmissionV1 {
    pub name: String,
    pub package_path: String,
    pub expected_verdict: ExpectedVerdict,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_score: Option<ExpectedScoreRange>,
    pub outputs: Vec<SubmitAnswerOutputV1>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SubmitAnswerOutputV1 {
    pub test_id: Uuid,
    pub test_index: usize,
    pub path: String,
    pub source_path: String,
    pub sha256: String,
}
