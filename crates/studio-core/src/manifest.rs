use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{Display, Formatter};
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use unicode_normalization::UnicodeNormalization;
use utoipa::ToSchema;
use uuid::Uuid;

pub const RELEASE_MANIFEST_SCHEMA_V1: &str = "reporch.release-manifest.v1";
pub const NATIVE_PACKAGE_RESERVED_PATHS: [&str; 7] = [
    "META-INF/reporch-release.json",
    "META-INF/reporch-source.json",
    "manifest.json",
    "reporch.problem.json",
    "validation-report.json",
    "reporch.import-report.json",
    "reporch.yaml",
];

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(transparent)]
pub struct Sha256Digest(String);

impl Sha256Digest {
    pub fn from_bytes(bytes: &[u8]) -> Self {
        Self(hex::encode(Sha256::digest(bytes)))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for Sha256Digest {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for Sha256Digest {
    type Err = ManifestError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let normalized = value.to_ascii_lowercase();
        if normalized.len() != 64 || !normalized.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(ManifestError::InvalidSha256);
        }
        Ok(Self(normalized))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProblemType {
    Standard,
    Scored,
    Interactive,
    OutputOnly,
    Library,
    Grader,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PackageProfile {
    ReporchNative,
    Icpc202509,
    IcpcLegacy,
    PolygonCompatible,
    DomjudgeZip,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ManifestFile {
    pub path: String,
    pub sha256: Sha256Digest,
    pub size_bytes: u64,
    pub media_type: String,
    #[serde(default)]
    pub executable: bool,
}

impl ManifestFile {
    pub fn validate_path(&self) -> Result<(), ManifestError> {
        validate_relative_path(&self.path)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ResourceLimits {
    pub time_ms: u64,
    pub memory_mib: u64,
    #[serde(default = "default_output_limit")]
    pub output_kib: u64,
}

fn default_output_limit() -> u64 {
    64 * 1024
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct TestCaseSpec {
    pub id: Uuid,
    pub name: String,
    pub input_file: String,
    pub answer_file: Option<String>,
    #[serde(default)]
    pub groups: Vec<String>,
    #[serde(default)]
    pub generated_by: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub generator_arguments: Vec<String>,
    #[serde(default)]
    pub seed: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct TestGroupSpec {
    pub id: String,
    pub points: f64,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub feedback_policy: crate::GroupFeedbackPolicyV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ProgramSpec {
    pub id: String,
    pub source_path: String,
    pub language: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub arguments: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ValidatorTestSpec {
    pub name: String,
    pub input_file: String,
    pub expected_valid: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CheckerTestSpec {
    pub name: String,
    pub input_file: String,
    pub answer_file: String,
    pub output_file: String,
    pub expected_accepted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExpectedVerdict {
    Accepted,
    WrongAnswer,
    TimeLimit,
    MemoryLimit,
    RuntimeError,
    Partial,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ExpectedScoreRange {
    pub minimum: f64,
    pub maximum: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct SolutionSpec {
    pub name: String,
    pub source_path: String,
    pub language: String,
    pub expected_verdict: ExpectedVerdict,
    #[serde(default)]
    pub expected_score: Option<ExpectedScoreRange>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct OutputSubmissionSpec {
    pub name: String,
    pub outputs: BTreeMap<Uuid, String>,
    pub expected_verdict: ExpectedVerdict,
    #[serde(default)]
    pub expected_score: Option<ExpectedScoreRange>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ScoreAggregation {
    AllOrNothing,
    GroupMin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum CustomImplInputMode {
    #[default]
    Raw,
    SkipNonNumericFirstLine,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum CustomImplExpectedOutputMode {
    #[default]
    Raw,
    StripBojOkPrelude,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct InteractiveStdioProfileV1 {
    pub source_path: String,
    pub interactor_source_path: String,
    pub asset_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include_dirs: Vec<String>,
    #[serde(default = "default_interactive_idle_timeout_ms")]
    pub idle_timeout_ms: u64,
    #[serde(default = "default_interactive_transcript_limit_kib")]
    pub transcript_limit_kib: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub solver_compile_command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub solver_run_command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interactor_compile_command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interactor_run_command: Option<String>,
}

fn default_interactive_idle_timeout_ms() -> u64 {
    2_000
}

fn default_interactive_transcript_limit_kib() -> u64 {
    1_024
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CustomImplProfileV1 {
    pub source_path: String,
    pub asset_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compile_script: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_script: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compile_command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_command: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExecutionHarnessV1 {
    InteractiveStdio {
        profiles: BTreeMap<String, InteractiveStdioProfileV1>,
        score_type: ScoreAggregation,
        score_scale: u32,
    },
    CustomImpl {
        profiles: BTreeMap<String, CustomImplProfileV1>,
        #[serde(default)]
        input_mode: CustomImplInputMode,
        #[serde(default)]
        expected_output_mode: CustomImplExpectedOutputMode,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CheckerSpec {
    Exact,
    Token,
    CaseInsensitive,
    Floating {
        absolute_error: String,
        relative_error: String,
    },
    Custom {
        source_path: String,
        language: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct JudgingSpec {
    pub limits: ResourceLimits,
    pub checker: CheckerSpec,
    #[serde(default)]
    pub tests: Vec<TestCaseSpec>,
    #[serde(default)]
    pub groups: Vec<TestGroupSpec>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub generators: Vec<ProgramSpec>,
    #[serde(default)]
    pub validator_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validator_language: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_validator_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_validators: Vec<ProgramSpec>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub validator_tests: Vec<ValidatorTestSpec>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checker_tests: Vec<CheckerTestSpec>,
    #[serde(default)]
    pub interactor_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interactor_language: Option<String>,
    #[serde(default)]
    pub grader_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grader_language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<ExecutionHarnessV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SourceAttribution {
    pub provider: String,
    pub external_id: String,
    #[serde(default)]
    pub canonical_url: String,
    #[serde(default)]
    pub license_name: String,
    #[serde(default)]
    pub attribution: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct StatementSectionsV1 {
    #[serde(default)]
    pub input_format: String,
    #[serde(default)]
    pub output_format: String,
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PublicationSampleV1 {
    pub name: String,
    pub input_file: String,
    pub output_file: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PublicationSpecV1 {
    pub category: String,
    pub difficulty: String,
    pub grading_category: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub allowed_languages: Vec<String>,
    pub statement_sections: BTreeMap<String, StatementSectionsV1>,
    #[serde(default)]
    pub samples: Vec<PublicationSampleV1>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ReleaseManifestV1 {
    pub schema: String,
    pub project_id: Uuid,
    pub commit_id: Uuid,
    pub problem_type: ProblemType,
    pub package_profile: PackageProfile,
    pub default_locale: String,
    pub title: BTreeMap<String, String>,
    pub statements: BTreeMap<String, String>,
    #[serde(default)]
    pub files: Vec<ManifestFile>,
    #[serde(default)]
    pub toolchains: BTreeMap<String, String>,
    pub judging: JudgingSpec,
    #[serde(default)]
    pub sources: Vec<SourceAttribution>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub solutions: Vec<SolutionSpec>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output_submissions: Vec<OutputSubmissionSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publication: Option<PublicationSpecV1>,
    pub policy_version: String,
}

impl ReleaseManifestV1 {
    pub fn canonical_json(&self) -> Result<Vec<u8>, ManifestError> {
        if self.schema != RELEASE_MANIFEST_SCHEMA_V1 {
            return Err(ManifestError::UnsupportedSchema(self.schema.clone()));
        }
        Ok(serde_json::to_vec(self)?)
    }

    pub fn digest(&self) -> Result<Sha256Digest, ManifestError> {
        Ok(Sha256Digest::from_bytes(&self.canonical_json()?))
    }

    pub fn validate_references(&self) -> Result<(), ManifestError> {
        let mut normalized_paths = BTreeSet::new();
        for file in &self.files {
            file.validate_path()?;
            let normalized: String = file.path.nfc().collect();
            if !normalized_paths.insert(normalized) {
                return Err(ManifestError::DuplicatePath(file.path.clone()));
            }
        }

        let paths: BTreeSet<&str> = self.files.iter().map(|file| file.path.as_str()).collect();
        for (locale, path) in &self.statements {
            if !paths.contains(path.as_str()) {
                return Err(ManifestError::MissingFileReference(format!(
                    "statement {locale}: {path}"
                )));
            }
        }
        for test in &self.judging.tests {
            if !paths.contains(test.input_file.as_str()) {
                return Err(ManifestError::MissingFileReference(test.input_file.clone()));
            }
            if let Some(answer) = &test.answer_file
                && !paths.contains(answer.as_str())
            {
                return Err(ManifestError::MissingFileReference(answer.clone()));
            }
        }
        for generator in &self.judging.generators {
            if !paths.contains(generator.source_path.as_str()) {
                return Err(ManifestError::MissingFileReference(
                    generator.source_path.clone(),
                ));
            }
        }
        for validator in &self.judging.extra_validators {
            if !paths.contains(validator.source_path.as_str()) {
                return Err(ManifestError::MissingFileReference(
                    validator.source_path.clone(),
                ));
            }
        }
        for validator_test in &self.judging.validator_tests {
            if !paths.contains(validator_test.input_file.as_str()) {
                return Err(ManifestError::MissingFileReference(
                    validator_test.input_file.clone(),
                ));
            }
        }
        for checker_test in &self.judging.checker_tests {
            for path in [
                &checker_test.input_file,
                &checker_test.answer_file,
                &checker_test.output_file,
            ] {
                if !paths.contains(path.as_str()) {
                    return Err(ManifestError::MissingFileReference(path.clone()));
                }
            }
        }
        for path in [
            self.judging.validator_path.as_ref(),
            self.judging.interactor_path.as_ref(),
            self.judging.grader_path.as_ref(),
        ]
        .into_iter()
        .flatten()
        .chain(self.judging.extra_validator_paths.iter())
        {
            if !paths.contains(path.as_str()) {
                return Err(ManifestError::MissingFileReference(path.clone()));
            }
        }
        if let CheckerSpec::Custom { source_path, .. } = &self.judging.checker
            && !paths.contains(source_path.as_str())
        {
            return Err(ManifestError::MissingFileReference(source_path.clone()));
        }
        for solution in &self.solutions {
            if !paths.contains(solution.source_path.as_str()) {
                return Err(ManifestError::MissingFileReference(
                    solution.source_path.clone(),
                ));
            }
        }
        for submission in &self.output_submissions {
            for path in submission.outputs.values() {
                if !paths.contains(path.as_str()) {
                    return Err(ManifestError::MissingFileReference(path.clone()));
                }
            }
        }
        if let Some(harness) = &self.judging.harness {
            match harness {
                ExecutionHarnessV1::InteractiveStdio { profiles, .. } => {
                    for profile in profiles.values() {
                        for path in std::iter::once(&profile.source_path)
                            .chain(std::iter::once(&profile.interactor_source_path))
                            .chain(profile.asset_paths.iter())
                        {
                            if !paths.contains(path.as_str()) {
                                return Err(ManifestError::MissingFileReference(path.clone()));
                            }
                        }
                    }
                }
                ExecutionHarnessV1::CustomImpl { profiles, .. } => {
                    for profile in profiles.values() {
                        for path in std::iter::once(&profile.source_path)
                            .chain(profile.asset_paths.iter())
                            .chain(profile.compile_script.iter())
                            .chain(profile.run_script.iter())
                        {
                            if !paths.contains(path.as_str()) {
                                return Err(ManifestError::MissingFileReference(path.clone()));
                            }
                        }
                    }
                }
            }
        }
        if let Some(publication) = &self.publication {
            for sample in &publication.samples {
                for path in [&sample.input_file, &sample.output_file] {
                    if !paths.contains(path.as_str()) {
                        return Err(ManifestError::MissingFileReference(path.clone()));
                    }
                }
            }
        }
        Ok(())
    }
}

pub fn validate_relative_path(path: &str) -> Result<(), ManifestError> {
    if path.is_empty()
        || path.starts_with('/')
        || path.starts_with('\\')
        || path.contains('\\')
        || path.contains('\0')
    {
        return Err(ManifestError::UnsafePath(path.to_owned()));
    }

    let mut depth = 0_usize;
    for component in path.split('/') {
        match component {
            "" | "." => return Err(ManifestError::UnsafePath(path.to_owned())),
            ".." => {
                if depth == 0 {
                    return Err(ManifestError::UnsafePath(path.to_owned()));
                }
                depth -= 1;
            }
            value if value.chars().any(char::is_control) => {
                return Err(ManifestError::UnsafePath(path.to_owned()));
            }
            value if !is_portable_path_component(value) => {
                return Err(ManifestError::UnsafePath(path.to_owned()));
            }
            _ => depth += 1,
        }
    }
    if path.split('/').any(|component| component == "..") {
        return Err(ManifestError::UnsafePath(path.to_owned()));
    }
    Ok(())
}

fn is_portable_path_component(component: &str) -> bool {
    // These names have special meaning on Windows even when an extension is
    // present. Colons also cover drive-qualified paths and NTFS alternate data
    // streams. Enforcing this policy on every OS keeps a package digest bound
    // to one filesystem interpretation.
    if component.contains(':') || component.ends_with(['.', ' ']) {
        return false;
    }
    let stem = component
        .split_once('.')
        .map_or(component, |(stem, _)| stem)
        .to_ascii_uppercase();
    !matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        && !is_numbered_windows_device(&stem, "COM")
        && !is_numbered_windows_device(&stem, "LPT")
}

fn is_numbered_windows_device(value: &str, prefix: &str) -> bool {
    value
        .strip_prefix(prefix)
        .is_some_and(|suffix| matches!(suffix.as_bytes(), [b'1'..=b'9']))
}

pub fn normalize_relative_path(path: &str) -> Result<String, ManifestError> {
    validate_relative_path(path)?;
    Ok(path.nfc().collect())
}

#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("invalid SHA-256 digest")]
    InvalidSha256,
    #[error("unsupported manifest schema: {0}")]
    UnsupportedSchema(String),
    #[error("unsafe relative path: {0}")]
    UnsafePath(String),
    #[error("duplicate or Unicode-colliding path: {0}")]
    DuplicatePath(String),
    #[error("manifest references a missing file: {0}")]
    MissingFileReference(String),
    #[error("duplicate manifest identity: {0}")]
    DuplicateIdentity(String),
    #[error("manifest references a missing identity: {0}")]
    MissingIdentityReference(String),
    #[error("invalid manifest configuration: {0}")]
    InvalidConfiguration(String),
    #[error("manifest serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn rejects_unsafe_paths() {
        for path in [
            "",
            "/etc/passwd",
            "../secret",
            "a/../secret",
            "a\\b",
            "a//b",
            "C:/Windows/System32",
            "C:relative.txt",
            "statement.md:payload",
            "CON",
            "nul.txt",
            "tools/COM1.exe",
            "tools/LPT9",
            "statement.md.",
            "statement.md ",
        ] {
            assert!(validate_relative_path(path).is_err(), "{path}");
        }
    }

    #[test]
    fn normalizes_unicode_paths_to_nfc() {
        assert_eq!(
            normalize_relative_path("statements/cafe\u{301}.md").unwrap(),
            "statements/café.md"
        );
    }

    #[test]
    fn digest_is_stable() {
        assert_eq!(
            Sha256Digest::from_bytes(b"studio").as_str(),
            "da0daaba4b156961c049ab4b85b6d0bcba2872200b7dedb4aa77f9208c601444"
        );
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(1_024))]

        #[test]
        fn normalization_is_idempotent_for_safe_ascii_paths(
            components in prop::collection::vec("[a-zA-Z0-9._-]{1,24}", 1..=12)
        ) {
            prop_assume!(components.iter().all(|component| {
                component != "." && component != ".." && is_portable_path_component(component)
            }));
            let path = components.join("/");
            let normalized = normalize_relative_path(&path).unwrap();
            prop_assert_eq!(normalize_relative_path(&normalized).unwrap(), normalized);
        }

        #[test]
        fn arbitrary_parent_traversal_is_always_rejected(
            suffix in "[a-zA-Z0-9._/-]{0,128}"
        ) {
            let path = format!("../{suffix}");
            prop_assert!(validate_relative_path(&path).is_err());
        }

        #[test]
        fn arbitrary_unicode_paths_never_panic(path in ".{0,512}") {
            let _ = normalize_relative_path(&path);
        }
    }
}
