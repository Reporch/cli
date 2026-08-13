#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};

use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use studio_core::{
    JudgingSpec, ManifestError, ManifestFile, OutputSubmissionSpec, PackageProfile, ProblemType,
    PublicationSpecV1, RELEASE_MANIFEST_SCHEMA_V1, ReleaseManifestV1, SolutionSpec,
    SourceAttribution, validate_relative_path,
};
use thiserror::Error;
use unicode_normalization::UnicodeNormalization;
use utoipa::ToSchema;
use uuid::Uuid;

pub const AUTHORING_SPEC_SCHEMA_V1: &str = "reporch.authoring-spec.v1";
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

pub fn parse_authoring_spec(bytes: &[u8]) -> Result<AuthoringSpecV1, AuthoringSpecError> {
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

    let spec: AuthoringSpecV1 = serde_yaml_ng::from_str(text)?;
    spec.validate_references()?;
    Ok(spec)
}

pub fn to_authoring_yaml(spec: &AuthoringSpecV1) -> Result<Vec<u8>, AuthoringSpecError> {
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
        let mut keys = Vec::<serde_yaml_ng::Value>::new();
        while let Some(key) = mapping.next_key::<serde_yaml_ng::Value>()? {
            if keys.contains(&key) {
                return Err(de::Error::custom(format!(
                    "duplicate YAML mapping key: {key:?}"
                )));
            }
            keys.push(key);
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
    #[error("invalid YAML: {0}")]
    Yaml(#[from] serde_yaml_ng::Error),
    #[error("invalid authoring reference: {0}")]
    Manifest(#[from] ManifestError),
}

#[cfg(test)]
mod tests {
    use super::*;
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

    #[test]
    fn yaml_round_trip_preserves_the_authoring_contract() {
        let expected = minimal_spec();
        let bytes = to_authoring_yaml(&expected).unwrap();
        assert_eq!(parse_authoring_spec(&bytes).unwrap(), expected);
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
}
