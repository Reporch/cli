#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet, HashSet};

use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use studio_core::{
    CheckerConfigSpecV2, CheckerMutationSpecV2, CheckerUnitSpecV2, ExecutionHarnessV1,
    ExecutionSpecV2, ExpectedVerdict, GeneratedCaseRefV2, GeneratorMatrixStrategyV2,
    GeneratorRecipeSpecV2, GeneratorSpecV2, HarnessKindV2, HarnessProfileSpecV2, HarnessSpecV2,
    InteractiveSpecV2, JudgingSpec, ManifestError, ManifestFile, OutputSubmissionSpec,
    OutputSubmissionSpecV2, PackageProfile, ProblemType, ProgramSpecV2, PublicationSpecV1,
    RELEASE_MANIFEST_SCHEMA_V1, RELEASE_MANIFEST_SCHEMA_V2, ReleaseManifestV1, ReleaseManifestV2,
    ScoreAggregationV2, SolutionRoleV2, SolutionSpec, SolutionSpecV2, SourceAttribution,
    TestCaseOriginV2, TestCaseRoleV2, TestCaseSpecV2, TestGroupSpecV2, TestingSpecV2,
    ValidatorSetSpecV2, ValidatorUnitSpecV2, VersionedReleaseManifest, validate_relative_path,
};
use thiserror::Error;
use unicode_normalization::UnicodeNormalization;
use utoipa::ToSchema;
use uuid::Uuid;

pub const AUTHORING_SPEC_SCHEMA_V1: &str = "reporch.authoring-spec.v1";
pub const AUTHORING_SPEC_SCHEMA_V2: &str = "reporch.authoring-spec.v2";
pub const MAX_AUTHORING_SPEC_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AuthoringFileV1 {
    pub path: String,
    pub media_type: String,
    #[serde(default)]
    pub executable: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AuthoringSpecV1 {
    pub schema: String,
    pub project_id: Uuid,
    pub problem_type: ProblemType,
    pub package_profile: PackageProfile,
    pub default_locale: String,
    pub title: BTreeMap<String, String>,
    pub statements: BTreeMap<String, String>,
    #[serde(default)]
    pub files: Vec<AuthoringFileV1>,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AuthoringFileV2 {
    pub path: String,
    pub media_type: String,
    #[serde(default)]
    pub executable: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AuthoringSpecV2 {
    pub schema: String,
    pub project_id: Uuid,
    pub problem_type: ProblemType,
    pub package_profile: PackageProfile,
    pub default_locale: String,
    pub title: BTreeMap<String, String>,
    pub statements: BTreeMap<String, String>,
    #[serde(default)]
    pub tutorials: BTreeMap<String, String>,
    #[serde(default)]
    pub files: Vec<AuthoringFileV2>,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(untagged)]
pub enum VersionedAuthoringSpec {
    V1(Box<AuthoringSpecV1>),
    V2(Box<AuthoringSpecV2>),
}

impl VersionedAuthoringSpec {
    pub fn from_manifest(manifest: &VersionedReleaseManifest) -> Self {
        match manifest {
            VersionedReleaseManifest::V1(manifest) => {
                Self::V1(Box::new(AuthoringSpecV1::from_manifest(manifest)))
            }
            VersionedReleaseManifest::V2(manifest) => {
                Self::V2(Box::new(AuthoringSpecV2::from_manifest(manifest)))
            }
        }
    }

    pub fn schema(&self) -> &str {
        match self {
            Self::V1(spec) => &spec.schema,
            Self::V2(spec) => &spec.schema,
        }
    }

    pub fn project_id(&self) -> Uuid {
        match self {
            Self::V1(spec) => spec.project_id,
            Self::V2(spec) => spec.project_id,
        }
    }

    pub fn problem_type(&self) -> ProblemType {
        match self {
            Self::V1(spec) => spec.problem_type,
            Self::V2(spec) => spec.problem_type,
        }
    }

    pub fn validate_references(&self) -> Result<(), AuthoringSpecError> {
        match self {
            Self::V1(spec) => spec.validate_references(),
            Self::V2(spec) => spec.validate_references(),
        }
    }

    pub fn materialize(
        &self,
        commit_id: Uuid,
        files: Vec<ManifestFile>,
    ) -> Result<VersionedReleaseManifest, AuthoringSpecError> {
        match self {
            Self::V1(spec) => spec.materialize(commit_id, files).map(Into::into),
            Self::V2(spec) => spec.materialize(commit_id, files).map(Into::into),
        }
    }
}

impl AuthoringSpecV1 {
    pub fn from_manifest(manifest: &ReleaseManifestV1) -> Self {
        Self {
            schema: AUTHORING_SPEC_SCHEMA_V1.into(),
            project_id: manifest.project_id,
            problem_type: manifest.problem_type,
            package_profile: manifest.package_profile,
            default_locale: manifest.default_locale.clone(),
            title: manifest.title.clone(),
            statements: manifest.statements.clone(),
            files: manifest
                .files
                .iter()
                .map(|file| AuthoringFileV1 {
                    path: file.path.clone(),
                    media_type: file.media_type.clone(),
                    executable: file.executable,
                })
                .collect(),
            toolchains: manifest.toolchains.clone(),
            judging: manifest.judging.clone(),
            sources: manifest.sources.clone(),
            solutions: manifest.solutions.clone(),
            output_submissions: manifest.output_submissions.clone(),
            publication: manifest.publication.clone(),
            policy_version: manifest.policy_version.clone(),
        }
    }

    pub fn validate_references(&self) -> Result<(), AuthoringSpecError> {
        if self.schema != AUTHORING_SPEC_SCHEMA_V1 {
            return Err(AuthoringSpecError::UnsupportedSchema(self.schema.clone()));
        }

        let mut normalized_paths = BTreeSet::new();
        for file in &self.files {
            validate_relative_path(&file.path)?;
            let normalized: String = file.path.nfc().collect();
            if !normalized_paths.insert(normalized) {
                return Err(AuthoringSpecError::DuplicatePath(file.path.clone()));
            }
        }

        // Reuse the release contract's exhaustive reference checks. Digests and
        // sizes are deliberately synthetic because they are generated later.
        self.materialize_unchecked(
            Uuid::nil(),
            self.files
                .iter()
                .map(|file| ManifestFile {
                    path: file.path.clone(),
                    sha256: studio_core::Sha256Digest::from_bytes(&[]),
                    size_bytes: 0,
                    media_type: file.media_type.clone(),
                    executable: file.executable,
                })
                .collect(),
        )
        .validate_references()?;
        Ok(())
    }

    pub fn materialize(
        &self,
        commit_id: Uuid,
        files: Vec<ManifestFile>,
    ) -> Result<ReleaseManifestV1, AuthoringSpecError> {
        self.validate_references()?;
        validate_materialized_inventory(&self.files, &files)?;
        let manifest = self.materialize_unchecked(commit_id, files);
        manifest.validate_references()?;
        Ok(manifest)
    }

    fn materialize_unchecked(
        &self,
        commit_id: Uuid,
        files: Vec<ManifestFile>,
    ) -> ReleaseManifestV1 {
        ReleaseManifestV1 {
            schema: RELEASE_MANIFEST_SCHEMA_V1.into(),
            project_id: self.project_id,
            commit_id,
            problem_type: self.problem_type,
            package_profile: self.package_profile,
            default_locale: self.default_locale.clone(),
            title: self.title.clone(),
            statements: self.statements.clone(),
            files,
            toolchains: self.toolchains.clone(),
            judging: self.judging.clone(),
            sources: self.sources.clone(),
            solutions: self.solutions.clone(),
            output_submissions: self.output_submissions.clone(),
            publication: self.publication.clone(),
            policy_version: self.policy_version.clone(),
        }
    }
}

impl AuthoringSpecV2 {
    pub fn from_manifest(manifest: &ReleaseManifestV2) -> Self {
        Self {
            schema: AUTHORING_SPEC_SCHEMA_V2.into(),
            project_id: manifest.project_id,
            problem_type: manifest.problem_type,
            package_profile: manifest.package_profile,
            default_locale: manifest.default_locale.clone(),
            title: manifest.title.clone(),
            statements: manifest.statements.clone(),
            tutorials: manifest.tutorials.clone(),
            files: manifest
                .files
                .iter()
                .map(|file| AuthoringFileV2 {
                    path: file.path.clone(),
                    media_type: file.media_type.clone(),
                    executable: file.executable,
                })
                .collect(),
            toolchains: manifest.toolchains.clone(),
            testing: manifest.testing.clone(),
            execution: manifest.execution.clone(),
            output_submissions: manifest.output_submissions.clone(),
            sources: manifest.sources.clone(),
            publication: manifest.publication.clone(),
            policy_version: manifest.policy_version.clone(),
        }
    }

    pub fn migrate_v1(spec: &AuthoringSpecV1) -> Result<Self, AuthoringSpecError> {
        spec.validate_references()?;
        let group_ids: BTreeMap<String, Uuid> = spec
            .judging
            .groups
            .iter()
            .map(|group| (group.id.clone(), Uuid::now_v7()))
            .collect();
        let groups = spec
            .judging
            .groups
            .iter()
            .map(|group| {
                Ok(TestGroupSpecV2 {
                    id: group_ids[&group.id],
                    name: group.id.clone(),
                    points: group.points,
                    depends_on: group
                        .depends_on
                        .iter()
                        .map(|id| {
                            group_ids.get(id).copied().ok_or_else(|| {
                                AuthoringSpecError::Migration(format!(
                                    "group {} depends on missing group {id}",
                                    group.id
                                ))
                            })
                        })
                        .collect::<Result<Vec<_>, _>>()?,
                    feedback_policy: group.feedback_policy,
                    aggregation: if spec.problem_type == ProblemType::Scored {
                        ScoreAggregationV2::GroupMinimum
                    } else {
                        ScoreAggregationV2::AllOrNothing
                    },
                })
            })
            .collect::<Result<Vec<_>, AuthoringSpecError>>()?;

        let generator_ids: BTreeMap<String, Uuid> = spec
            .judging
            .generators
            .iter()
            .map(|generator| (generator.id.clone(), Uuid::now_v7()))
            .collect();
        let mut generators: Vec<GeneratorSpecV2> = spec
            .judging
            .generators
            .iter()
            .map(|generator| GeneratorSpecV2 {
                program: ProgramSpecV2 {
                    id: generator_ids[&generator.id],
                    name: generator.id.clone(),
                    source_path: generator.source_path.clone(),
                    language: generator.language.clone(),
                    arguments: generator.arguments.clone(),
                },
                recipes: Vec::new(),
            })
            .collect();

        let sample_inputs: BTreeSet<&str> = spec
            .publication
            .iter()
            .flat_map(|publication| publication.samples.iter())
            .map(|sample| sample.input_file.as_str())
            .collect();
        let mut tests = Vec::with_capacity(spec.judging.tests.len());
        for test in &spec.judging.tests {
            let mapped_groups = test
                .groups
                .iter()
                .map(|id| {
                    group_ids.get(id).copied().ok_or_else(|| {
                        AuthoringSpecError::Migration(format!(
                            "test {} references missing group {id}",
                            test.name
                        ))
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            let generated = if let Some(generator_name) = &test.generated_by {
                let generator_id = generator_ids.get(generator_name).copied().ok_or_else(|| {
                    AuthoringSpecError::Migration(format!(
                        "test {} references missing generator {generator_name}",
                        test.name
                    ))
                })?;
                let seed = test.seed.ok_or_else(|| {
                    AuthoringSpecError::Migration(format!(
                        "generated test {} has no fixed seed",
                        test.name
                    ))
                })?;
                let recipe_id = Uuid::now_v7();
                let generator = generators
                    .iter_mut()
                    .find(|generator| generator.program.id == generator_id)
                    .ok_or_else(|| {
                        AuthoringSpecError::Migration("generator mapping disappeared".into())
                    })?;
                generator.recipes.push(GeneratorRecipeSpecV2 {
                    id: recipe_id,
                    name: format!("migrated-{}", test.name),
                    argument_template: test.generator_arguments.clone(),
                    parameters: BTreeMap::new(),
                    matrix: GeneratorMatrixStrategyV2::Cartesian,
                    seed_start: seed,
                    seed_step: 1,
                    count: 1,
                    group_ids: mapped_groups.clone(),
                });
                Some(GeneratedCaseRefV2 {
                    generator_id,
                    recipe_id,
                    ordinal: 0,
                    seed,
                })
            } else {
                None
            };
            tests.push(TestCaseSpecV2 {
                id: test.id,
                name: test.name.clone(),
                role: if sample_inputs.contains(test.input_file.as_str()) {
                    TestCaseRoleV2::Sample
                } else {
                    TestCaseRoleV2::Secret
                },
                origin: if generated.is_some() {
                    TestCaseOriginV2::Generated
                } else {
                    TestCaseOriginV2::Manual
                },
                input_file: test.input_file.clone(),
                answer_file: test.answer_file.clone(),
                group_ids: mapped_groups,
                points: None,
                generated,
            });
        }

        let primary_validator = spec
            .judging
            .validator_path
            .as_ref()
            .map(|path| ProgramSpecV2 {
                id: Uuid::now_v7(),
                name: "validator".into(),
                source_path: path.clone(),
                language: spec
                    .judging
                    .validator_language
                    .clone()
                    .unwrap_or_else(|| "unknown".into()),
                arguments: Vec::new(),
            });
        let mut extra_validators: Vec<ProgramSpecV2> = spec
            .judging
            .extra_validators
            .iter()
            .map(|validator| ProgramSpecV2 {
                id: Uuid::now_v7(),
                name: validator.id.clone(),
                source_path: validator.source_path.clone(),
                language: validator.language.clone(),
                arguments: validator.arguments.clone(),
            })
            .collect();
        for (index, path) in spec.judging.extra_validator_paths.iter().enumerate() {
            if !extra_validators
                .iter()
                .any(|validator| validator.source_path == *path)
            {
                extra_validators.push(ProgramSpecV2 {
                    id: Uuid::now_v7(),
                    name: format!("extra-validator-{}", index + 1),
                    source_path: path.clone(),
                    language: spec
                        .judging
                        .validator_language
                        .clone()
                        .unwrap_or_else(|| "unknown".into()),
                    arguments: Vec::new(),
                });
            }
        }

        let solutions = spec
            .solutions
            .iter()
            .enumerate()
            .map(|(index, solution)| SolutionSpecV2 {
                program: ProgramSpecV2 {
                    id: Uuid::now_v7(),
                    name: solution.name.clone(),
                    source_path: solution.source_path.clone(),
                    language: solution.language.clone(),
                    arguments: Vec::new(),
                },
                role: match solution.expected_verdict {
                    ExpectedVerdict::Accepted if index == 0 => SolutionRoleV2::Reference,
                    ExpectedVerdict::Accepted => SolutionRoleV2::Alternative,
                    ExpectedVerdict::WrongAnswer => SolutionRoleV2::KnownWrong,
                    _ => SolutionRoleV2::Alternative,
                },
                expected_verdict: solution.expected_verdict,
                expected_score: solution.expected_score.clone(),
                group_expectations: Vec::new(),
                tags: Vec::new(),
                notes: String::new(),
            })
            .collect();

        let execution = migrate_execution_v1(spec)?;
        Ok(Self {
            schema: AUTHORING_SPEC_SCHEMA_V2.into(),
            project_id: spec.project_id,
            problem_type: spec.problem_type,
            package_profile: spec.package_profile,
            default_locale: spec.default_locale.clone(),
            title: spec.title.clone(),
            statements: spec.statements.clone(),
            tutorials: BTreeMap::new(),
            files: spec
                .files
                .iter()
                .map(|file| AuthoringFileV2 {
                    path: file.path.clone(),
                    media_type: file.media_type.clone(),
                    executable: file.executable,
                })
                .collect(),
            toolchains: spec.toolchains.clone(),
            testing: TestingSpecV2 {
                limits: spec.judging.limits.clone(),
                groups,
                tests,
                generators,
                validators: ValidatorSetSpecV2 {
                    primary: primary_validator,
                    extra: extra_validators,
                    unit_tests: spec
                        .judging
                        .validator_tests
                        .iter()
                        .map(|test| ValidatorUnitSpecV2 {
                            id: Uuid::now_v7(),
                            name: test.name.clone(),
                            input_file: test.input_file.clone(),
                            expected_valid: test.expected_valid,
                        })
                        .collect(),
                },
                checker: CheckerConfigSpecV2 {
                    checker: spec.judging.checker.clone(),
                    unit_tests: spec
                        .judging
                        .checker_tests
                        .iter()
                        .map(|test| CheckerUnitSpecV2 {
                            id: Uuid::now_v7(),
                            name: test.name.clone(),
                            input_file: test.input_file.clone(),
                            answer_file: test.answer_file.clone(),
                            output_file: test.output_file.clone(),
                            expected_accepted: test.expected_accepted,
                        })
                        .collect(),
                    mutation: CheckerMutationSpecV2::default(),
                },
                solutions,
                stress_suites: Vec::new(),
                detect_duplicates: true,
                verify_determinism: true,
            },
            execution,
            output_submissions: spec
                .output_submissions
                .iter()
                .map(|submission| OutputSubmissionSpecV2 {
                    id: Uuid::now_v7(),
                    name: submission.name.clone(),
                    outputs: submission.outputs.clone(),
                    expected_verdict: submission.expected_verdict,
                    expected_score: submission.expected_score.clone(),
                })
                .collect(),
            sources: spec.sources.clone(),
            publication: spec.publication.clone(),
            policy_version: spec.policy_version.clone(),
        })
    }

    pub fn migrate_manifest_v1(manifest: &ReleaseManifestV1) -> Result<Self, AuthoringSpecError> {
        Self::migrate_v1(&AuthoringSpecV1::from_manifest(manifest))
    }

    pub fn validate_references(&self) -> Result<(), AuthoringSpecError> {
        if self.schema != AUTHORING_SPEC_SCHEMA_V2 {
            return Err(AuthoringSpecError::UnsupportedSchema(self.schema.clone()));
        }
        let mut normalized_paths = BTreeSet::new();
        for file in &self.files {
            validate_relative_path(&file.path)?;
            let normalized: String = file.path.nfc().collect();
            if !normalized_paths.insert(normalized) {
                return Err(AuthoringSpecError::DuplicatePath(file.path.clone()));
            }
        }
        self.materialize_unchecked(
            Uuid::nil(),
            self.files
                .iter()
                .map(|file| ManifestFile {
                    path: file.path.clone(),
                    sha256: studio_core::Sha256Digest::from_bytes(&[]),
                    size_bytes: 0,
                    media_type: file.media_type.clone(),
                    executable: file.executable,
                })
                .collect(),
        )
        .validate_references()?;
        Ok(())
    }

    pub fn materialize(
        &self,
        commit_id: Uuid,
        files: Vec<ManifestFile>,
    ) -> Result<ReleaseManifestV2, AuthoringSpecError> {
        self.validate_references()?;
        validate_materialized_inventory_v2(&self.files, &files)?;
        let manifest = self.materialize_unchecked(commit_id, files);
        manifest.validate_references()?;
        Ok(manifest)
    }

    fn materialize_unchecked(
        &self,
        commit_id: Uuid,
        files: Vec<ManifestFile>,
    ) -> ReleaseManifestV2 {
        ReleaseManifestV2 {
            schema: RELEASE_MANIFEST_SCHEMA_V2.into(),
            project_id: self.project_id,
            commit_id,
            problem_type: self.problem_type,
            package_profile: self.package_profile,
            default_locale: self.default_locale.clone(),
            title: self.title.clone(),
            statements: self.statements.clone(),
            tutorials: self.tutorials.clone(),
            files,
            toolchains: self.toolchains.clone(),
            testing: self.testing.clone(),
            execution: self.execution.clone(),
            output_submissions: self.output_submissions.clone(),
            sources: self.sources.clone(),
            publication: self.publication.clone(),
            policy_version: self.policy_version.clone(),
        }
    }
}

fn migrate_execution_v1(spec: &AuthoringSpecV1) -> Result<ExecutionSpecV2, AuthoringSpecError> {
    let interactive_profile = match &spec.judging.harness {
        Some(ExecutionHarnessV1::InteractiveStdio { profiles, .. }) => profiles
            .iter()
            .next()
            .map(|(language, profile)| (language.clone(), profile.interactor_source_path.clone())),
        _ => None,
    };
    let interactive = spec
        .judging
        .interactor_path
        .clone()
        .map(|path| {
            (
                spec.judging
                    .interactor_language
                    .clone()
                    .unwrap_or_else(|| "unknown".into()),
                path,
            )
        })
        .or(interactive_profile)
        .map(|(language, source_path)| InteractiveSpecV2 {
            interactor: ProgramSpecV2 {
                id: Uuid::now_v7(),
                name: "interactor".into(),
                source_path,
                language,
                arguments: Vec::new(),
            },
            idle_timeout_ms: 2_000,
            transcript_limit_kib: 1_024,
            unit_tests: Vec::new(),
        });

    let harness = match &spec.judging.harness {
        Some(ExecutionHarnessV1::CustomImpl { profiles, .. }) => {
            let kind = match spec.problem_type {
                ProblemType::Library => HarnessKindV2::Library,
                ProblemType::Grader => HarnessKindV2::Grader,
                _ => {
                    return Err(AuthoringSpecError::Migration(
                        "custom implementation harness requires a library or grader problem".into(),
                    ));
                }
            };
            Some(HarnessSpecV2 {
                kind,
                interface_files: Vec::new(),
                public_files: Vec::new(),
                private_files: spec.judging.grader_path.iter().cloned().collect(),
                stub_templates: BTreeMap::new(),
                profiles: profiles
                    .iter()
                    .map(|(language, profile)| {
                        (
                            language.clone(),
                            HarnessProfileSpecV2 {
                                language: language.clone(),
                                source_path: spec
                                    .judging
                                    .grader_path
                                    .clone()
                                    .unwrap_or_else(|| profile.source_path.clone()),
                                asset_paths: profile.asset_paths.clone(),
                                include_dirs: Vec::new(),
                                compile_script: profile.compile_script.clone(),
                                run_script: profile.run_script.clone(),
                                compile_command: profile.compile_command.clone(),
                                run_command: profile.run_command.clone(),
                            },
                        )
                    })
                    .collect(),
            })
        }
        _ => None,
    };
    Ok(ExecutionSpecV2 {
        interactive,
        harness,
    })
}

pub fn parse_authoring_spec(bytes: &[u8]) -> Result<AuthoringSpecV1, AuthoringSpecError> {
    let text = checked_yaml_text(bytes)?;
    let spec: AuthoringSpecV1 = serde_yaml_ng::from_str(text)?;
    spec.validate_references()?;
    Ok(spec)
}

pub fn parse_authoring_spec_v2(bytes: &[u8]) -> Result<AuthoringSpecV2, AuthoringSpecError> {
    let text = checked_yaml_text(bytes)?;
    let spec: AuthoringSpecV2 = serde_yaml_ng::from_str(text)?;
    spec.validate_references()?;
    Ok(spec)
}

pub fn parse_versioned_authoring_spec(
    bytes: &[u8],
) -> Result<VersionedAuthoringSpec, AuthoringSpecError> {
    let text = checked_yaml_text(bytes)?;
    let header: AuthoringSchemaHeader = serde_yaml_ng::from_str(text)?;
    match header.schema.as_str() {
        AUTHORING_SPEC_SCHEMA_V1 => {
            let spec: AuthoringSpecV1 = serde_yaml_ng::from_str(text)?;
            spec.validate_references()?;
            Ok(VersionedAuthoringSpec::V1(Box::new(spec)))
        }
        AUTHORING_SPEC_SCHEMA_V2 => {
            let spec: AuthoringSpecV2 = serde_yaml_ng::from_str(text)?;
            spec.validate_references()?;
            Ok(VersionedAuthoringSpec::V2(Box::new(spec)))
        }
        _ => Err(AuthoringSpecError::UnsupportedSchema(header.schema)),
    }
}

#[derive(Deserialize)]
struct AuthoringSchemaHeader {
    schema: String,
}

fn checked_yaml_text(bytes: &[u8]) -> Result<&str, AuthoringSpecError> {
    if bytes.len() > MAX_AUTHORING_SPEC_BYTES {
        return Err(AuthoringSpecError::TooLarge {
            actual: bytes.len(),
            maximum: MAX_AUTHORING_SPEC_BYTES,
        });
    }
    let text = std::str::from_utf8(bytes).map_err(|_| AuthoringSpecError::InvalidUtf8)?;
    reject_yaml_aliases(text)?;

    let mut documents = serde_yaml_ng::Deserializer::from_str(text);
    let duplicate_check = documents.next().ok_or(AuthoringSpecError::EmptyDocument)?;
    UniqueValue
        .deserialize(duplicate_check)
        .map_err(AuthoringSpecError::Yaml)?;
    if documents.next().is_some() {
        return Err(AuthoringSpecError::MultipleDocuments);
    }
    Ok(text)
}

pub fn to_authoring_yaml(spec: &AuthoringSpecV1) -> Result<Vec<u8>, AuthoringSpecError> {
    spec.validate_references()?;
    let mut yaml = serde_yaml_ng::to_string(spec)?;
    if !yaml.ends_with('\n') {
        yaml.push('\n');
    }
    Ok(yaml.into_bytes())
}

pub fn to_authoring_yaml_v2(spec: &AuthoringSpecV2) -> Result<Vec<u8>, AuthoringSpecError> {
    spec.validate_references()?;
    let mut yaml = serde_yaml_ng::to_string(spec)?;
    if !yaml.ends_with('\n') {
        yaml.push('\n');
    }
    Ok(yaml.into_bytes())
}

fn validate_materialized_inventory(
    authoring: &[AuthoringFileV1],
    materialized: &[ManifestFile],
) -> Result<(), AuthoringSpecError> {
    let expected: BTreeMap<&str, (&str, bool)> = authoring
        .iter()
        .map(|file| {
            (
                file.path.as_str(),
                (file.media_type.as_str(), file.executable),
            )
        })
        .collect();
    let actual: BTreeMap<&str, (&str, bool)> = materialized
        .iter()
        .map(|file| {
            (
                file.path.as_str(),
                (file.media_type.as_str(), file.executable),
            )
        })
        .collect();
    if expected != actual || authoring.len() != materialized.len() {
        return Err(AuthoringSpecError::InventoryMismatch);
    }
    Ok(())
}

fn validate_materialized_inventory_v2(
    authoring: &[AuthoringFileV2],
    materialized: &[ManifestFile],
) -> Result<(), AuthoringSpecError> {
    let expected: BTreeMap<&str, (&str, bool)> = authoring
        .iter()
        .map(|file| {
            (
                file.path.as_str(),
                (file.media_type.as_str(), file.executable),
            )
        })
        .collect();
    let actual: BTreeMap<&str, (&str, bool)> = materialized
        .iter()
        .map(|file| {
            (
                file.path.as_str(),
                (file.media_type.as_str(), file.executable),
            )
        })
        .collect();
    if expected != actual || authoring.len() != materialized.len() {
        return Err(AuthoringSpecError::InventoryMismatch);
    }
    Ok(())
}

fn reject_yaml_aliases(text: &str) -> Result<(), AuthoringSpecError> {
    for line in text.lines() {
        let mut single_quoted = false;
        let mut double_quoted = false;
        let mut escaped = false;
        let chars: Vec<char> = line.chars().collect();
        for (index, character) in chars.iter().copied().enumerate() {
            if escaped {
                escaped = false;
                continue;
            }
            if double_quoted && character == '\\' {
                escaped = true;
                continue;
            }
            if character == '\'' && !double_quoted {
                single_quoted = !single_quoted;
                continue;
            }
            if character == '"' && !single_quoted {
                double_quoted = !double_quoted;
                continue;
            }
            if single_quoted || double_quoted {
                continue;
            }
            if character == '#' {
                break;
            }
            if matches!(character, '&' | '*') {
                let previous_is_boundary = index == 0
                    || chars[index - 1].is_whitespace()
                    || matches!(chars[index - 1], '[' | '{' | ',' | ':');
                let next_is_name = chars
                    .get(index + 1)
                    .is_some_and(|next| next.is_ascii_alphanumeric() || matches!(next, '_' | '-'));
                if previous_is_boundary && next_is_name {
                    return Err(AuthoringSpecError::YamlAliasForbidden);
                }
            }
        }
    }
    Ok(())
}

struct UniqueValue;

impl<'de> DeserializeSeed<'de> for UniqueValue {
    type Value = ();

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(UniqueValueVisitor)
    }
}

struct UniqueValueVisitor;

impl<'de> Visitor<'de> for UniqueValueVisitor {
    type Value = ();

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a YAML value without duplicate mapping keys")
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        while sequence.next_element_seed(UniqueValue)?.is_some() {}
        Ok(())
    }

    fn visit_map<A>(self, mut mapping: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut keys = HashSet::<String>::new();
        while let Some(key) = mapping.next_key::<serde_yaml_ng::Value>()? {
            let serde_yaml_ng::Value::String(key) = key else {
                return Err(de::Error::custom("YAML mapping keys must be strings"));
            };
            if !keys.insert(key.clone()) {
                return Err(de::Error::custom(format!(
                    "duplicate YAML mapping key: {key:?}"
                )));
            }
            mapping.next_value_seed(UniqueValue)?;
        }
        Ok(())
    }

    fn visit_bool<E>(self, _value: bool) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_i64<E>(self, _value: i64) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_u64<E>(self, _value: u64) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_f64<E>(self, _value: f64) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_char<E>(self, _value: char) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_str<E>(self, _value: &str) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_string<E>(self, _value: String) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_bytes<E>(self, _value: &[u8]) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_byte_buf<E>(self, _value: Vec<u8>) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(self)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(())
    }

    fn visit_newtype_struct<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(self)
    }
}

#[derive(Debug, Error)]
pub enum AuthoringSpecError {
    #[error("authoring spec is not valid UTF-8")]
    InvalidUtf8,
    #[error("authoring spec is too large: {actual} bytes (maximum {maximum})")]
    TooLarge { actual: usize, maximum: usize },
    #[error("YAML anchors and aliases are not supported")]
    YamlAliasForbidden,
    #[error("multiple YAML documents are not supported")]
    MultipleDocuments,
    #[error("authoring spec is empty")]
    EmptyDocument,
    #[error("unsupported authoring schema: {0}")]
    UnsupportedSchema(String),
    #[error("duplicate or Unicode-colliding path: {0}")]
    DuplicatePath(String),
    #[error("materialized file inventory does not match reporch.yaml")]
    InventoryMismatch,
    #[error("authoring migration failed: {0}")]
    Migration(String),
    #[error("invalid YAML: {0}")]
    Yaml(#[from] serde_yaml_ng::Error),
    #[error("invalid authoring reference: {0}")]
    Manifest(#[from] ManifestError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use studio_core::{CheckerSpec, ResourceLimits};

    fn minimal_spec() -> AuthoringSpecV1 {
        AuthoringSpecV1 {
            schema: AUTHORING_SPEC_SCHEMA_V1.into(),
            project_id: Uuid::now_v7(),
            problem_type: ProblemType::Standard,
            package_profile: PackageProfile::ReporchNative,
            default_locale: "ko".into(),
            title: BTreeMap::from([("ko".into(), "합".into())]),
            statements: BTreeMap::from([("ko".into(), "statements/ko.md".into())]),
            files: vec![AuthoringFileV1 {
                path: "statements/ko.md".into(),
                media_type: "text/markdown".into(),
                executable: false,
            }],
            toolchains: BTreeMap::new(),
            judging: JudgingSpec {
                limits: ResourceLimits {
                    time_ms: 1_000,
                    memory_mib: 256,
                    output_kib: 65_536,
                },
                checker: CheckerSpec::Token,
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
            policy_version: "studio-policy-v1".into(),
        }
    }

    fn minimal_spec_v2() -> AuthoringSpecV2 {
        AuthoringSpecV2 {
            schema: AUTHORING_SPEC_SCHEMA_V2.into(),
            project_id: Uuid::now_v7(),
            problem_type: ProblemType::Standard,
            package_profile: PackageProfile::ReporchNative,
            default_locale: "ko".into(),
            title: BTreeMap::from([("ko".into(), "합".into())]),
            statements: BTreeMap::from([("ko".into(), "statements/ko.md".into())]),
            tutorials: BTreeMap::new(),
            files: vec![AuthoringFileV2 {
                path: "statements/ko.md".into(),
                media_type: "text/markdown".into(),
                executable: false,
            }],
            toolchains: BTreeMap::new(),
            testing: studio_core::TestingSpecV2 {
                limits: ResourceLimits {
                    time_ms: 1_000,
                    memory_mib: 256,
                    output_kib: 65_536,
                },
                groups: vec![],
                tests: vec![],
                generators: vec![],
                validators: studio_core::ValidatorSetSpecV2::default(),
                checker: studio_core::CheckerConfigSpecV2 {
                    checker: CheckerSpec::Token,
                    unit_tests: vec![],
                    mutation: studio_core::CheckerMutationSpecV2::default(),
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
    fn yaml_round_trip_preserves_the_authoring_contract() {
        let expected = minimal_spec();
        let bytes = to_authoring_yaml(&expected).unwrap();
        assert_eq!(parse_authoring_spec(&bytes).unwrap(), expected);
    }

    #[test]
    fn v2_yaml_round_trip_and_materialization_preserve_the_complete_contract() {
        let expected = minimal_spec_v2();
        let bytes = to_authoring_yaml_v2(&expected).unwrap();
        assert_eq!(parse_authoring_spec_v2(&bytes).unwrap(), expected);
        assert!(matches!(
            parse_versioned_authoring_spec(&bytes).unwrap(),
            VersionedAuthoringSpec::V2(_)
        ));
        let files = vec![ManifestFile {
            path: "statements/ko.md".into(),
            sha256: studio_core::Sha256Digest::from_bytes(b"statement"),
            size_bytes: 9,
            media_type: "text/markdown".into(),
            executable: false,
        }];
        let manifest = expected.materialize(Uuid::now_v7(), files).unwrap();
        assert_eq!(manifest.schema, RELEASE_MANIFEST_SCHEMA_V2);
    }

    #[test]
    fn versioned_parser_keeps_v1_compatibility() {
        let bytes = to_authoring_yaml(&minimal_spec()).unwrap();
        assert!(matches!(
            parse_versioned_authoring_spec(&bytes).unwrap(),
            VersionedAuthoringSpec::V1(_)
        ));
    }

    #[test]
    fn v1_migration_creates_a_valid_v2_document_without_changing_file_inventory() {
        let v1 = minimal_spec();
        let v2 = AuthoringSpecV2::migrate_v1(&v1).unwrap();
        assert_eq!(v2.project_id, v1.project_id);
        assert_eq!(v2.problem_type, v1.problem_type);
        assert_eq!(v2.statements, v1.statements);
        assert_eq!(
            v2.files.iter().map(|file| &file.path).collect::<Vec<_>>(),
            v1.files.iter().map(|file| &file.path).collect::<Vec<_>>()
        );
        let bytes = to_authoring_yaml_v2(&v2).unwrap();
        assert!(matches!(
            parse_versioned_authoring_spec(&bytes).unwrap(),
            VersionedAuthoringSpec::V2(_)
        ));
    }

    #[test]
    fn duplicate_keys_are_rejected_before_typed_deserialization() {
        let bytes = to_authoring_yaml(&minimal_spec()).unwrap();
        let yaml = String::from_utf8(bytes).unwrap().replacen(
            "schema: reporch.authoring-spec.v1",
            "schema: reporch.authoring-spec.v1\nschema: reporch.authoring-spec.v1",
            1,
        );
        let error = parse_authoring_spec(yaml.as_bytes()).unwrap_err();
        assert!(error.to_string().contains("duplicate YAML mapping key"));
    }

    #[test]
    fn aliases_are_rejected_without_expansion() {
        let error =
            parse_authoring_spec(b"schema: &schema reporch.authoring-spec.v1\ncopy: *schema\n")
                .unwrap_err();
        assert!(matches!(error, AuthoringSpecError::YamlAliasForbidden));
    }

    #[test]
    fn materialization_requires_the_exact_declared_inventory() {
        let spec = minimal_spec();
        let error = spec.materialize(Uuid::now_v7(), vec![]).unwrap_err();
        assert!(matches!(error, AuthoringSpecError::InventoryMismatch));
    }

    #[test]
    fn oversized_documents_are_rejected_before_yaml_parsing() {
        let document = vec![b'a'; MAX_AUTHORING_SPEC_BYTES + 1];
        assert!(matches!(
            parse_authoring_spec(&document),
            Err(AuthoringSpecError::TooLarge { .. })
        ));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        #[test]
        fn arbitrary_bounded_input_never_panics(
            document in prop::collection::vec(any::<u8>(), 0..=65_536)
        ) {
            let _ = parse_authoring_spec(&document);
        }
    }
}
