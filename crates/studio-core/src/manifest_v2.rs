use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{
    CheckerSpec, ExpectedScoreRange, ExpectedVerdict, GroupFeedbackPolicyV1, ManifestError,
    ManifestFile, PackageProfile, ProblemType, PublicationSpecV1, ResourceLimits, Sha256Digest,
    SourceAttribution,
};

pub const RELEASE_MANIFEST_SCHEMA_V2: &str = "reporch.release-manifest.v2";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TestCaseRoleV2 {
    Sample,
    Public,
    Secret,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TestCaseOriginV2 {
    Manual,
    Generated,
    Uploaded,
    Copied,
    Stress,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ScoreAggregationV2 {
    AllOrNothing,
    GroupMinimum,
    Sum,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ProgramSpecV2 {
    pub id: Uuid,
    pub name: String,
    pub source_path: String,
    pub language: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub arguments: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct TestGroupSpecV2 {
    pub id: Uuid,
    pub name: String,
    pub points: f64,
    #[serde(default)]
    pub depends_on: Vec<Uuid>,
    #[serde(default)]
    pub feedback_policy: GroupFeedbackPolicyV1,
    pub aggregation: ScoreAggregationV2,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GeneratorParameterValueV2 {
    Integer { value: i64 },
    Boolean { value: bool },
    String { value: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GeneratorParameterDomainV2 {
    Values {
        values: Vec<GeneratorParameterValueV2>,
    },
    IntegerRange {
        start: i64,
        end: i64,
        step: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum GeneratorMatrixStrategyV2 {
    Cartesian,
    Zip,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct GeneratorRecipeSpecV2 {
    pub id: Uuid,
    pub name: String,
    #[serde(default)]
    pub argument_template: Vec<String>,
    #[serde(default)]
    pub parameters: BTreeMap<String, GeneratorParameterDomainV2>,
    pub matrix: GeneratorMatrixStrategyV2,
    pub seed_start: u64,
    #[serde(default = "default_seed_step")]
    pub seed_step: u64,
    pub count: u32,
    #[serde(default)]
    pub group_ids: Vec<Uuid>,
}

fn default_seed_step() -> u64 {
    1
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct GeneratorSpecV2 {
    pub program: ProgramSpecV2,
    #[serde(default)]
    pub recipes: Vec<GeneratorRecipeSpecV2>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct GeneratedCaseRefV2 {
    pub generator_id: Uuid,
    pub recipe_id: Uuid,
    pub ordinal: u32,
    pub seed: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct TestCaseSpecV2 {
    pub id: Uuid,
    pub name: String,
    pub role: TestCaseRoleV2,
    pub origin: TestCaseOriginV2,
    pub input_file: String,
    pub answer_file: Option<String>,
    #[serde(default)]
    pub group_ids: Vec<Uuid>,
    pub points: Option<f64>,
    pub generated: Option<GeneratedCaseRefV2>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ValidatorUnitSpecV2 {
    pub id: Uuid,
    pub name: String,
    pub input_file: String,
    pub expected_valid: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, Default)]
#[serde(deny_unknown_fields)]
pub struct ValidatorSetSpecV2 {
    pub primary: Option<ProgramSpecV2>,
    #[serde(default)]
    pub extra: Vec<ProgramSpecV2>,
    #[serde(default)]
    pub unit_tests: Vec<ValidatorUnitSpecV2>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckerUnitSpecV2 {
    pub id: Uuid,
    pub name: String,
    pub input_file: String,
    pub answer_file: String,
    pub output_file: String,
    pub expected_accepted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckerMutationSpecV2 {
    pub enabled: bool,
    #[serde(default = "default_mutation_cases")]
    pub cases: u32,
}

fn default_mutation_cases() -> u32 {
    32
}

impl Default for CheckerMutationSpecV2 {
    fn default() -> Self {
        Self {
            enabled: true,
            cases: default_mutation_cases(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckerConfigSpecV2 {
    pub checker: CheckerSpec,
    #[serde(default)]
    pub unit_tests: Vec<CheckerUnitSpecV2>,
    #[serde(default)]
    pub mutation: CheckerMutationSpecV2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SolutionRoleV2 {
    Reference,
    Alternative,
    Oracle,
    Brute,
    KnownWrong,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct GroupSolutionExpectationV2 {
    pub group_id: Uuid,
    pub verdict: ExpectedVerdict,
    pub score: Option<ExpectedScoreRange>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SolutionSpecV2 {
    pub program: ProgramSpecV2,
    pub role: SolutionRoleV2,
    pub expected_verdict: ExpectedVerdict,
    pub expected_score: Option<ExpectedScoreRange>,
    #[serde(default)]
    pub group_expectations: Vec<GroupSolutionExpectationV2>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub notes: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct StressSuiteSpecV2 {
    pub id: Uuid,
    pub name: String,
    pub generator_id: Uuid,
    pub recipe_id: Uuid,
    pub oracle_solution_id: Uuid,
    #[serde(default)]
    pub candidate_solution_ids: Vec<Uuid>,
    pub seed_start: u64,
    pub cases: u32,
    pub timeout_ms: u64,
    #[serde(default)]
    pub minimize_failure: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct TestingSpecV2 {
    pub limits: ResourceLimits,
    #[serde(default)]
    pub groups: Vec<TestGroupSpecV2>,
    #[serde(default)]
    pub tests: Vec<TestCaseSpecV2>,
    #[serde(default)]
    pub generators: Vec<GeneratorSpecV2>,
    #[serde(default)]
    pub validators: ValidatorSetSpecV2,
    pub checker: CheckerConfigSpecV2,
    #[serde(default)]
    pub solutions: Vec<SolutionSpecV2>,
    #[serde(default)]
    pub stress_suites: Vec<StressSuiteSpecV2>,
    #[serde(default = "default_true")]
    pub detect_duplicates: bool,
    #[serde(default = "default_true")]
    pub verify_determinism: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct InteractiveUnitSpecV2 {
    pub id: Uuid,
    pub name: String,
    pub input_file: String,
    pub solution_id: Uuid,
    pub expected_verdict: ExpectedVerdict,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct InteractiveSpecV2 {
    pub interactor: ProgramSpecV2,
    #[serde(default = "default_interactive_idle_timeout_ms")]
    pub idle_timeout_ms: u64,
    #[serde(default = "default_transcript_limit_kib")]
    pub transcript_limit_kib: u64,
    #[serde(default)]
    pub unit_tests: Vec<InteractiveUnitSpecV2>,
}

fn default_interactive_idle_timeout_ms() -> u64 {
    2_000
}

fn default_transcript_limit_kib() -> u64 {
    1_024
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum HarnessKindV2 {
    Library,
    Grader,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct HarnessProfileSpecV2 {
    pub language: String,
    pub source_path: String,
    #[serde(default)]
    pub asset_paths: Vec<String>,
    #[serde(default)]
    pub include_dirs: Vec<String>,
    pub compile_script: Option<String>,
    pub run_script: Option<String>,
    pub compile_command: Option<String>,
    pub run_command: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct HarnessSpecV2 {
    pub kind: HarnessKindV2,
    #[serde(default)]
    pub interface_files: Vec<String>,
    #[serde(default)]
    pub public_files: Vec<String>,
    #[serde(default)]
    pub private_files: Vec<String>,
    #[serde(default)]
    pub stub_templates: BTreeMap<String, String>,
    #[serde(default)]
    pub profiles: BTreeMap<String, HarnessProfileSpecV2>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema, Default)]
#[serde(deny_unknown_fields)]
pub struct ExecutionSpecV2 {
    pub interactive: Option<InteractiveSpecV2>,
    pub harness: Option<HarnessSpecV2>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct OutputSubmissionSpecV2 {
    pub id: Uuid,
    pub name: String,
    pub outputs: BTreeMap<Uuid, String>,
    pub expected_verdict: ExpectedVerdict,
    pub expected_score: Option<ExpectedScoreRange>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ReleaseManifestV2 {
    pub schema: String,
    pub project_id: Uuid,
    pub commit_id: Uuid,
    pub problem_type: ProblemType,
    pub package_profile: PackageProfile,
    pub default_locale: String,
    pub title: BTreeMap<String, String>,
    pub statements: BTreeMap<String, String>,
    #[serde(default)]
    pub tutorials: BTreeMap<String, String>,
    #[serde(default)]
    pub files: Vec<ManifestFile>,
    #[serde(default)]
    pub toolchains: BTreeMap<String, String>,
    pub testing: TestingSpecV2,
    #[serde(default)]
    pub execution: ExecutionSpecV2,
    #[serde(default)]
    pub output_submissions: Vec<OutputSubmissionSpecV2>,
    #[serde(default)]
    pub sources: Vec<SourceAttribution>,
    pub publication: Option<PublicationSpecV1>,
    pub policy_version: String,
}

/// Lossless wire/storage representation for immutable manifests.
///
/// The enum is intentionally untagged because each manifest already carries a
/// stable `schema` discriminator. This keeps the serialized JSON byte-for-byte
/// compatible with existing V1 consumers while allowing new clients to submit
/// V2 without projecting away generators, stress suites, or harness metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(untagged)]
pub enum VersionedReleaseManifest {
    V1(crate::ReleaseManifestV1),
    V2(ReleaseManifestV2),
}

impl VersionedReleaseManifest {
    pub fn schema(&self) -> &str {
        match self {
            Self::V1(manifest) => &manifest.schema,
            Self::V2(manifest) => &manifest.schema,
        }
    }

    pub fn project_id(&self) -> Uuid {
        match self {
            Self::V1(manifest) => manifest.project_id,
            Self::V2(manifest) => manifest.project_id,
        }
    }

    pub fn commit_id(&self) -> Uuid {
        match self {
            Self::V1(manifest) => manifest.commit_id,
            Self::V2(manifest) => manifest.commit_id,
        }
    }

    pub fn set_commit_id(&mut self, commit_id: Uuid) {
        match self {
            Self::V1(manifest) => manifest.commit_id = commit_id,
            Self::V2(manifest) => manifest.commit_id = commit_id,
        }
    }

    pub fn problem_type(&self) -> ProblemType {
        match self {
            Self::V1(manifest) => manifest.problem_type,
            Self::V2(manifest) => manifest.problem_type,
        }
    }

    pub fn package_profile(&self) -> PackageProfile {
        match self {
            Self::V1(manifest) => manifest.package_profile,
            Self::V2(manifest) => manifest.package_profile,
        }
    }

    pub fn default_locale(&self) -> &str {
        match self {
            Self::V1(manifest) => &manifest.default_locale,
            Self::V2(manifest) => &manifest.default_locale,
        }
    }

    pub fn title(&self) -> &BTreeMap<String, String> {
        match self {
            Self::V1(manifest) => &manifest.title,
            Self::V2(manifest) => &manifest.title,
        }
    }

    pub fn statements(&self) -> &BTreeMap<String, String> {
        match self {
            Self::V1(manifest) => &manifest.statements,
            Self::V2(manifest) => &manifest.statements,
        }
    }

    pub fn files(&self) -> &[ManifestFile] {
        match self {
            Self::V1(manifest) => &manifest.files,
            Self::V2(manifest) => &manifest.files,
        }
    }

    pub fn toolchains(&self) -> &BTreeMap<String, String> {
        match self {
            Self::V1(manifest) => &manifest.toolchains,
            Self::V2(manifest) => &manifest.toolchains,
        }
    }

    pub fn policy_version(&self) -> &str {
        match self {
            Self::V1(manifest) => &manifest.policy_version,
            Self::V2(manifest) => &manifest.policy_version,
        }
    }

    pub fn publication(&self) -> Option<&PublicationSpecV1> {
        match self {
            Self::V1(manifest) => manifest.publication.as_ref(),
            Self::V2(manifest) => manifest.publication.as_ref(),
        }
    }

    pub fn validate_references(&self) -> Result<(), ManifestError> {
        match self {
            Self::V1(manifest) => manifest.validate_references(),
            Self::V2(manifest) => manifest.validate_references(),
        }
    }

    pub fn canonical_json(&self) -> Result<Vec<u8>, ManifestError> {
        match self {
            Self::V1(manifest) => manifest.canonical_json(),
            Self::V2(manifest) => manifest.canonical_json(),
        }
    }

    pub fn digest(&self) -> Result<Sha256Digest, ManifestError> {
        Ok(Sha256Digest::from_bytes(&self.canonical_json()?))
    }
}

impl From<crate::ReleaseManifestV1> for VersionedReleaseManifest {
    fn from(value: crate::ReleaseManifestV1) -> Self {
        Self::V1(value)
    }
}

impl From<ReleaseManifestV2> for VersionedReleaseManifest {
    fn from(value: ReleaseManifestV2) -> Self {
        Self::V2(value)
    }
}

impl ReleaseManifestV2 {
    pub fn canonical_json(&self) -> Result<Vec<u8>, ManifestError> {
        if self.schema != RELEASE_MANIFEST_SCHEMA_V2 {
            return Err(ManifestError::UnsupportedSchema(self.schema.clone()));
        }
        Ok(serde_json::to_vec(self)?)
    }

    pub fn digest(&self) -> Result<Sha256Digest, ManifestError> {
        Ok(Sha256Digest::from_bytes(&self.canonical_json()?))
    }

    pub fn validate_references(&self) -> Result<(), ManifestError> {
        if self.schema != RELEASE_MANIFEST_SCHEMA_V2 {
            return Err(ManifestError::UnsupportedSchema(self.schema.clone()));
        }
        let mut normalized_paths = BTreeSet::new();
        for file in &self.files {
            file.validate_path()?;
            let normalized = crate::normalize_relative_path(&file.path)?;
            if !normalized_paths.insert(normalized) {
                return Err(ManifestError::DuplicatePath(file.path.clone()));
            }
        }
        let paths: BTreeSet<&str> = self.files.iter().map(|file| file.path.as_str()).collect();
        for path in self.statements.values().chain(self.tutorials.values()) {
            require_file(&paths, path)?;
        }

        let group_ids = unique_ids(self.testing.groups.iter().map(|group| group.id), "group")?;
        for group in &self.testing.groups {
            for dependency in &group.depends_on {
                require_id(&group_ids, dependency, "group dependency")?;
            }
        }

        let generator_ids = unique_ids(
            self.testing
                .generators
                .iter()
                .map(|generator| generator.program.id),
            "generator",
        )?;
        let mut recipe_ids = BTreeMap::<Uuid, BTreeSet<Uuid>>::new();
        for generator in &self.testing.generators {
            validate_program(&paths, &generator.program)?;
            let ids = unique_ids(generator.recipes.iter().map(|recipe| recipe.id), "recipe")?;
            for recipe in &generator.recipes {
                if recipe.count == 0 || recipe.seed_step == 0 {
                    return Err(ManifestError::InvalidConfiguration(format!(
                        "generator recipe {} has a zero count or seed step",
                        recipe.name
                    )));
                }
                for group_id in &recipe.group_ids {
                    require_id(&group_ids, group_id, "generator recipe group")?;
                }
                for domain in recipe.parameters.values() {
                    validate_parameter_domain(domain)?;
                }
            }
            recipe_ids.insert(generator.program.id, ids);
        }

        let test_ids = unique_ids(self.testing.tests.iter().map(|test| test.id), "test")?;
        for test in &self.testing.tests {
            require_file(&paths, &test.input_file)?;
            if let Some(answer) = &test.answer_file {
                require_file(&paths, answer)?;
            }
            for group_id in &test.group_ids {
                require_id(&group_ids, group_id, "test group")?;
            }
            if let Some(generated) = &test.generated {
                require_id(
                    &generator_ids,
                    &generated.generator_id,
                    "generated test generator",
                )?;
                let recipes = recipe_ids.get(&generated.generator_id).ok_or_else(|| {
                    ManifestError::MissingIdentityReference("generated test recipe".into())
                })?;
                require_id(recipes, &generated.recipe_id, "generated test recipe")?;
            }
        }

        if let Some(primary) = &self.testing.validators.primary {
            validate_program(&paths, primary)?;
        }
        for validator in &self.testing.validators.extra {
            validate_program(&paths, validator)?;
        }
        for test in &self.testing.validators.unit_tests {
            require_file(&paths, &test.input_file)?;
        }
        if let CheckerSpec::Custom { source_path, .. } = &self.testing.checker.checker {
            require_file(&paths, source_path)?;
        }
        for test in &self.testing.checker.unit_tests {
            require_file(&paths, &test.input_file)?;
            require_file(&paths, &test.answer_file)?;
            require_file(&paths, &test.output_file)?;
        }

        let solution_ids = unique_ids(
            self.testing
                .solutions
                .iter()
                .map(|solution| solution.program.id),
            "solution",
        )?;
        for solution in &self.testing.solutions {
            validate_program(&paths, &solution.program)?;
            for expectation in &solution.group_expectations {
                require_id(
                    &group_ids,
                    &expectation.group_id,
                    "solution expectation group",
                )?;
            }
        }
        unique_ids(
            self.testing.stress_suites.iter().map(|suite| suite.id),
            "stress suite",
        )?;
        for suite in &self.testing.stress_suites {
            require_id(&generator_ids, &suite.generator_id, "stress generator")?;
            require_id(&solution_ids, &suite.oracle_solution_id, "stress oracle")?;
            for candidate in &suite.candidate_solution_ids {
                require_id(&solution_ids, candidate, "stress candidate")?;
            }
            let recipes = recipe_ids
                .get(&suite.generator_id)
                .ok_or_else(|| ManifestError::MissingIdentityReference("stress recipe".into()))?;
            require_id(recipes, &suite.recipe_id, "stress recipe")?;
            if suite.cases == 0 || suite.timeout_ms == 0 {
                return Err(ManifestError::InvalidConfiguration(format!(
                    "stress suite {} has a zero case count or timeout",
                    suite.name
                )));
            }
        }

        if let Some(interactive) = &self.execution.interactive {
            validate_program(&paths, &interactive.interactor)?;
            for test in &interactive.unit_tests {
                require_file(&paths, &test.input_file)?;
                require_id(
                    &solution_ids,
                    &test.solution_id,
                    "interactive unit solution",
                )?;
            }
        }
        if let Some(harness) = &self.execution.harness {
            for path in harness
                .interface_files
                .iter()
                .chain(harness.public_files.iter())
                .chain(harness.private_files.iter())
                .chain(harness.stub_templates.values())
            {
                require_file(&paths, path)?;
            }
            for profile in harness.profiles.values() {
                require_file(&paths, &profile.source_path)?;
                for path in profile
                    .asset_paths
                    .iter()
                    .chain(profile.compile_script.iter())
                    .chain(profile.run_script.iter())
                {
                    require_file(&paths, path)?;
                }
            }
        }

        unique_ids(
            self.output_submissions
                .iter()
                .map(|submission| submission.id),
            "output",
        )?;
        for submission in &self.output_submissions {
            for (test_id, path) in &submission.outputs {
                require_id(&test_ids, test_id, "output test")?;
                require_file(&paths, path)?;
            }
        }
        if let Some(publication) = &self.publication {
            for sample in &publication.samples {
                require_file(&paths, &sample.input_file)?;
                require_file(&paths, &sample.output_file)?;
            }
        }
        Ok(())
    }
}

fn validate_program(paths: &BTreeSet<&str>, program: &ProgramSpecV2) -> Result<(), ManifestError> {
    if program.name.trim().is_empty() || program.language.trim().is_empty() {
        return Err(ManifestError::InvalidConfiguration(
            "program name and language are required".into(),
        ));
    }
    require_file(paths, &program.source_path)
}

fn validate_parameter_domain(domain: &GeneratorParameterDomainV2) -> Result<(), ManifestError> {
    match domain {
        GeneratorParameterDomainV2::Values { values } if values.is_empty() => Err(
            ManifestError::InvalidConfiguration("generator parameter values are empty".into()),
        ),
        GeneratorParameterDomainV2::IntegerRange { start, end, step }
            if start > end || *step == 0 =>
        {
            Err(ManifestError::InvalidConfiguration(
                "generator integer range is invalid".into(),
            ))
        }
        _ => Ok(()),
    }
}

fn unique_ids(
    ids: impl IntoIterator<Item = Uuid>,
    kind: &str,
) -> Result<BTreeSet<Uuid>, ManifestError> {
    let mut unique = BTreeSet::new();
    for id in ids {
        if !unique.insert(id) {
            return Err(ManifestError::DuplicateIdentity(format!("{kind}: {id}")));
        }
    }
    Ok(unique)
}

fn require_id(ids: &BTreeSet<Uuid>, id: &Uuid, kind: &str) -> Result<(), ManifestError> {
    if ids.contains(id) {
        Ok(())
    } else {
        Err(ManifestError::MissingIdentityReference(format!(
            "{kind}: {id}"
        )))
    }
}

fn require_file(paths: &BTreeSet<&str>, path: &str) -> Result<(), ManifestError> {
    if paths.contains(path) {
        Ok(())
    } else {
        Err(ManifestError::MissingFileReference(path.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_manifest() -> ReleaseManifestV2 {
        ReleaseManifestV2 {
            schema: RELEASE_MANIFEST_SCHEMA_V2.into(),
            project_id: Uuid::now_v7(),
            commit_id: Uuid::now_v7(),
            problem_type: ProblemType::Standard,
            package_profile: PackageProfile::ReporchNative,
            default_locale: "ko".into(),
            title: BTreeMap::from([("ko".into(), "합".into())]),
            statements: BTreeMap::from([("ko".into(), "statements/ko.md".into())]),
            tutorials: BTreeMap::new(),
            files: vec![ManifestFile {
                path: "statements/ko.md".into(),
                sha256: Sha256Digest::from_bytes(b"statement"),
                size_bytes: 9,
                media_type: "text/markdown".into(),
                executable: false,
            }],
            toolchains: BTreeMap::new(),
            testing: TestingSpecV2 {
                limits: ResourceLimits {
                    time_ms: 1_000,
                    memory_mib: 256,
                    output_kib: 65_536,
                },
                groups: vec![],
                tests: vec![],
                generators: vec![],
                validators: ValidatorSetSpecV2::default(),
                checker: CheckerConfigSpecV2 {
                    checker: CheckerSpec::Token,
                    unit_tests: vec![],
                    mutation: CheckerMutationSpecV2::default(),
                },
                solutions: vec![],
                stress_suites: vec![],
                detect_duplicates: true,
                verify_determinism: true,
            },
            execution: ExecutionSpecV2::default(),
            output_submissions: vec![],
            sources: vec![],
            publication: None,
            policy_version: "studio-policy-v2".into(),
        }
    }

    #[test]
    fn minimal_v2_manifest_is_digestible_and_reference_safe() {
        let manifest = minimal_manifest();
        manifest.validate_references().unwrap();
        assert_eq!(manifest.digest().unwrap(), manifest.digest().unwrap());
    }

    #[test]
    fn generated_cases_require_a_real_generator_and_recipe() {
        let mut manifest = minimal_manifest();
        manifest.testing.tests.push(TestCaseSpecV2 {
            id: Uuid::now_v7(),
            name: "generated".into(),
            role: TestCaseRoleV2::Secret,
            origin: TestCaseOriginV2::Generated,
            input_file: "statements/ko.md".into(),
            answer_file: None,
            group_ids: vec![],
            points: None,
            generated: Some(GeneratedCaseRefV2 {
                generator_id: Uuid::now_v7(),
                recipe_id: Uuid::now_v7(),
                ordinal: 0,
                seed: 1,
            }),
        });
        assert!(matches!(
            manifest.validate_references(),
            Err(ManifestError::MissingIdentityReference(_))
        ));
    }
}
