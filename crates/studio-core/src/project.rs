use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{ProblemType, ReleaseManifestV1, Sha256Digest};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
pub struct SubjectRef {
    pub issuer: String,
    pub subject: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ProjectRole {
    Owner,
    Maintainer,
    Author,
    Reviewer,
    Translator,
    Viewer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectCapability {
    Read,
    Edit,
    Comment,
    Validate,
    Review,
    ManageMembers,
    Publish,
}

impl ProjectRole {
    pub fn allows(self, capability: ProjectCapability) -> bool {
        match self {
            Self::Owner => true,
            Self::Maintainer => !matches!(capability, ProjectCapability::Review),
            Self::Author => matches!(
                capability,
                ProjectCapability::Read
                    | ProjectCapability::Edit
                    | ProjectCapability::Comment
                    | ProjectCapability::Validate
            ),
            Self::Reviewer => matches!(
                capability,
                ProjectCapability::Read | ProjectCapability::Comment | ProjectCapability::Review
            ),
            Self::Translator => matches!(
                capability,
                ProjectCapability::Read | ProjectCapability::Edit | ProjectCapability::Comment
            ),
            Self::Viewer => capability == ProjectCapability::Read,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ProjectMembership {
    pub project_id: Uuid,
    pub member: SubjectRef,
    pub role: ProjectRole,
    pub entitlement_version: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReviewState {
    Draft,
    InReview,
    ChangesRequested,
    Approved,
    Released,
    Archived,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum AuthoringCheckerKindV1 {
    #[default]
    Token,
    Custom,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AuthoringSettingsV1 {
    pub default_locale: String,
    pub input_format: String,
    pub output_format: String,
    #[serde(default)]
    pub validator_enabled: bool,
    #[serde(default)]
    pub generator_enabled: bool,
    #[serde(default)]
    pub checker_kind: AuthoringCheckerKindV1,
}

impl Default for AuthoringSettingsV1 {
    fn default() -> Self {
        Self {
            default_locale: "ko".into(),
            input_format: "두 정수를 공백으로 구분해 입력합니다.".into(),
            output_format: "두 정수의 합을 출력합니다.".into(),
            validator_enabled: false,
            generator_enabled: false,
            checker_kind: AuthoringCheckerKindV1::Token,
        }
    }
}

impl AuthoringSettingsV1 {
    fn validate(&self) -> Result<(), ProjectError> {
        let locale_valid = !self.default_locale.trim().is_empty()
            && self.default_locale.len() <= 35
            && self
                .default_locale
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
        if !locale_valid || self.input_format.len() > 20_000 || self.output_format.len() > 20_000 {
            return Err(ProjectError::InvalidAuthoringSettings);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Project {
    pub id: Uuid,
    pub owner: SubjectRef,
    pub organization_id: Option<String>,
    pub title: String,
    pub problem_type: ProblemType,
    pub state: ReviewState,
    pub revision: i64,
    #[serde(default)]
    pub authoring: AuthoringSettingsV1,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Project {
    pub fn create(
        owner: SubjectRef,
        organization_id: Option<String>,
        title: String,
        problem_type: ProblemType,
        now: DateTime<Utc>,
    ) -> Result<Self, ProjectError> {
        let title = title.trim().to_owned();
        if title.is_empty() || title.chars().count() > 255 {
            return Err(ProjectError::InvalidTitle);
        }
        if owner.issuer.trim().is_empty() || owner.subject.trim().is_empty() {
            return Err(ProjectError::InvalidOwner);
        }
        Ok(Self {
            id: Uuid::now_v7(),
            owner,
            organization_id,
            title,
            problem_type,
            state: ReviewState::Draft,
            revision: 1,
            authoring: AuthoringSettingsV1::default(),
            created_at: now,
            updated_at: now,
        })
    }

    pub fn rename(
        &mut self,
        title: String,
        expected_revision: i64,
        now: DateTime<Utc>,
    ) -> Result<(), ProjectError> {
        self.update(Some(title), None, expected_revision, now)
    }

    pub fn update(
        &mut self,
        title: Option<String>,
        authoring: Option<AuthoringSettingsV1>,
        expected_revision: i64,
        now: DateTime<Utc>,
    ) -> Result<(), ProjectError> {
        if self.revision != expected_revision {
            return Err(ProjectError::RevisionConflict {
                expected: expected_revision,
                actual: self.revision,
            });
        }
        if title.is_none() && authoring.is_none() {
            return Err(ProjectError::EmptyUpdate);
        }
        let mut changed = false;
        if let Some(title) = title {
            let title = title.trim().to_owned();
            if title.is_empty() || title.chars().count() > 255 {
                return Err(ProjectError::InvalidTitle);
            }
            if self.title != title {
                self.title = title;
                changed = true;
            }
        }
        if let Some(authoring) = authoring {
            authoring.validate()?;
            if self.authoring != authoring {
                self.authoring = authoring;
                changed = true;
            }
        }
        if changed {
            self.revision += 1;
            self.updated_at = now;
            self.invalidate_review();
        }
        Ok(())
    }

    pub fn transition(
        &mut self,
        next: ReviewState,
        now: DateTime<Utc>,
    ) -> Result<(), ProjectError> {
        let allowed = matches!(
            (self.state, next),
            (ReviewState::Draft, ReviewState::InReview)
                | (ReviewState::InReview, ReviewState::ChangesRequested)
                | (ReviewState::InReview, ReviewState::Approved)
                | (ReviewState::ChangesRequested, ReviewState::InReview)
                | (ReviewState::Approved, ReviewState::Released)
                | (_, ReviewState::Archived)
        );
        if !allowed {
            return Err(ProjectError::InvalidTransition {
                from: self.state,
                to: next,
            });
        }
        self.state = next;
        self.revision += 1;
        self.updated_at = now;
        Ok(())
    }

    fn invalidate_review(&mut self) {
        if matches!(
            self.state,
            ReviewState::InReview | ReviewState::ChangesRequested | ReviewState::Approved
        ) {
            self.state = ReviewState::Draft;
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ImmutableCommit {
    pub id: Uuid,
    pub project_id: Uuid,
    pub sequence: i64,
    pub manifest: ReleaseManifestV1,
    pub manifest_digest: Sha256Digest,
    pub authored_by: SubjectRef,
    pub message: String,
    pub created_at: DateTime<Utc>,
}

impl ImmutableCommit {
    pub fn create(
        project_id: Uuid,
        sequence: i64,
        manifest: ReleaseManifestV1,
        authored_by: SubjectRef,
        message: String,
        now: DateTime<Utc>,
    ) -> Result<Self, ProjectError> {
        if manifest.project_id != project_id {
            return Err(ProjectError::ManifestProjectMismatch);
        }
        manifest.validate_references()?;
        let manifest_digest = manifest.digest()?;
        Ok(Self {
            id: manifest.commit_id,
            project_id,
            sequence,
            manifest,
            manifest_digest,
            authored_by,
            message: message.trim().chars().take(500).collect(),
            created_at: now,
        })
    }
}

#[derive(Debug, Error)]
pub enum ProjectError {
    #[error("project title must contain 1 to 255 characters")]
    InvalidTitle,
    #[error("project authoring settings are invalid")]
    InvalidAuthoringSettings,
    #[error("project update must contain at least one field")]
    EmptyUpdate,
    #[error("project owner issuer and subject are required")]
    InvalidOwner,
    #[error("revision conflict: expected {expected}, actual {actual}")]
    RevisionConflict { expected: i64, actual: i64 },
    #[error("invalid project state transition from {from:?} to {to:?}")]
    InvalidTransition { from: ReviewState, to: ReviewState },
    #[error("manifest project does not match commit project")]
    ManifestProjectMismatch,
    #[error(transparent)]
    Manifest(#[from] crate::ManifestError),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner() -> SubjectRef {
        SubjectRef {
            issuer: "https://reporch.example".into(),
            subject: "user-1".into(),
        }
    }

    #[test]
    fn content_change_invalidates_approval() {
        let now = Utc::now();
        let mut project =
            Project::create(owner(), None, "A + B".into(), ProblemType::Standard, now).unwrap();
        project.state = ReviewState::Approved;
        project.rename("A plus B".into(), 1, now).unwrap();
        assert_eq!(project.state, ReviewState::Draft);
        assert_eq!(project.revision, 2);
    }

    #[test]
    fn rejects_stale_revision() {
        let now = Utc::now();
        let mut project =
            Project::create(owner(), None, "A + B".into(), ProblemType::Standard, now).unwrap();
        let error = project.rename("B + C".into(), 0, now).unwrap_err();
        assert!(matches!(error, ProjectError::RevisionConflict { .. }));
    }

    #[test]
    fn authoring_settings_are_revision_guarded_and_invalidate_approval() {
        let now = Utc::now();
        let mut project =
            Project::create(owner(), None, "A + B".into(), ProblemType::Standard, now).unwrap();
        project.state = ReviewState::Approved;
        let settings = AuthoringSettingsV1 {
            validator_enabled: true,
            generator_enabled: true,
            checker_kind: AuthoringCheckerKindV1::Custom,
            ..AuthoringSettingsV1::default()
        };
        project
            .update(None, Some(settings.clone()), 1, now)
            .unwrap();
        assert_eq!(project.authoring, settings);
        assert_eq!(project.revision, 2);
        assert_eq!(project.state, ReviewState::Draft);
        assert!(matches!(
            project.update(None, Some(AuthoringSettingsV1::default()), 1, now),
            Err(ProjectError::RevisionConflict { .. })
        ));
    }

    #[test]
    fn project_roles_follow_least_privilege_capabilities() {
        assert!(ProjectRole::Owner.allows(ProjectCapability::Publish));
        assert!(ProjectRole::Maintainer.allows(ProjectCapability::ManageMembers));
        assert!(!ProjectRole::Maintainer.allows(ProjectCapability::Review));
        assert!(ProjectRole::Author.allows(ProjectCapability::Validate));
        assert!(!ProjectRole::Author.allows(ProjectCapability::Publish));
        assert!(ProjectRole::Reviewer.allows(ProjectCapability::Review));
        assert!(ProjectRole::Reviewer.allows(ProjectCapability::Comment));
        assert!(!ProjectRole::Reviewer.allows(ProjectCapability::Edit));
        assert!(ProjectRole::Translator.allows(ProjectCapability::Edit));
        assert!(ProjectRole::Viewer.allows(ProjectCapability::Read));
        assert!(!ProjectRole::Viewer.allows(ProjectCapability::Comment));
        assert!(!ProjectRole::Viewer.allows(ProjectCapability::Edit));
    }
}
