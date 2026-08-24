#![forbid(unsafe_code)]

use chrono::{DateTime, Utc};
use reporch_format::VersionedAuthoringSpec;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use studio_core::{
    AuthoringSettingsV1, ManifestFile, PackageProfile, ProblemType, Project, ProjectMembership,
    ProjectRole, ReviewApprovalSourceV1, ReviewDecisionKindV1, ReviewDecisionRecord,
    ReviewPoolStatusV1, ReviewRecord, ReviewState, ReviewStatusV1, Sha256Digest, SubjectRef,
    TestLabCheckerV1, TestLabDraftV1, ValidationIssue, ValidationStagePlanV1,
    VersionedReleaseManifest, WaiverRecord, WaiverRevocationRecord, WaiverStatusV1,
};
use utoipa::ToSchema;
use uuid::Uuid;

pub const WORKING_COPY_SCHEMA_V1: &str = "reporch.working-copy.v1";
pub const STUDIO_CAPABILITIES_SCHEMA_V1: &str = "reporch.studio-capabilities.v1";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkingCopyV1 {
    pub schema: String,
    pub project_id: Uuid,
    pub revision: i64,
    pub spec: VersionedAuthoringSpec,
    pub updated_by: SubjectRef,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateWorkingCopyRequestV1 {
    pub spec: VersionedAuthoringSpec,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CommitWorkingCopyRequestV1 {
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkingCopyReadinessV1 {
    pub schema: String,
    pub project_id: Uuid,
    pub working_copy_revision: i64,
    pub can_commit: bool,
    pub issues: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct StudioCapabilitiesV1 {
    pub schema: String,
    pub api_versions: Vec<String>,
    pub authoring_spec_versions: Vec<String>,
    pub release_manifest_versions: Vec<String>,
    pub minimum_cli_version: String,
    pub maximum_cli_major: u64,
}

pub const EVENT_SCHEMA_V1: &str = "reporch.studio-event.v1";
pub const ENTITLEMENT_EVENT_SCHEMA_V1: &str = "reporch.studio-entitlement-event.v1";
pub const PRESENCE_SCHEMA_V1: &str = "reporch.studio-presence.v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PresenceCursorV1 {
    pub anchor: u32,
    pub head: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PresenceParticipantV1 {
    pub connection_id: Uuid,
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    pub color: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<PresenceCursorV1>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresenceClientMessageV1 {
    pub schema: String,
    #[serde(rename = "type")]
    pub message_type: PresenceClientMessageTypeV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<PresenceCursorV1>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PresenceClientMessageTypeV1 {
    Update,
    Heartbeat,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PresenceServerMessageV1 {
    Snapshot {
        schema: String,
        document_id: Uuid,
        self_connection_id: Uuid,
        participants: Vec<PresenceParticipantV1>,
    },
    Update {
        schema: String,
        document_id: Uuid,
        participant: PresenceParticipantV1,
    },
    Leave {
        schema: String,
        document_id: Uuid,
        connection_id: Uuid,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ApiErrorResponse {
    pub error_code: String,
    pub message: String,
    pub retryable: bool,
    pub trace_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct EntitlementChangedEventV1 {
    pub schema: String,
    pub event_id: Uuid,
    pub occurred_at: DateTime<Utc>,
    pub issuer: String,
    pub sub: String,
    pub membership_version: i64,
    pub organization_ids: Vec<String>,
    pub account_active: bool,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct EntitlementEventReceiptV1 {
    pub event_id: Uuid,
    pub accepted: bool,
    pub applied: bool,
    pub replayed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CatalogCategoryV1 {
    pub name: String,
    pub description: String,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CatalogDifficultyV1 {
    pub name: String,
    pub score: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CatalogGradingCategoryV1 {
    pub code: String,
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CatalogLanguageV1 {
    pub code: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct StudioCatalogV1 {
    pub schema: String,
    pub categories: Vec<CatalogCategoryV1>,
    pub difficulties: Vec<CatalogDifficultyV1>,
    pub grading_categories: Vec<CatalogGradingCategoryV1>,
    pub languages: Vec<CatalogLanguageV1>,
    pub digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CreateProjectRequest {
    pub title: String,
    pub problem_type: ProblemType,
    #[serde(default)]
    pub organization_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UpdateProjectRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authoring: Option<AuthoringSettingsV1>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateTestLabRequestV1 {
    pub draft: TestLabDraftV1,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateTestGenerationRequestV1 {
    pub test_case_id: Uuid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TestGenerationStatusV1 {
    Queued,
    Running,
    Succeeded,
    Failed,
    Stale,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct TestGenerationRunV1 {
    pub id: Uuid,
    pub project_id: Uuid,
    pub test_case_id: Uuid,
    pub base_revision: i64,
    pub status: TestGenerationStatusV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub materialized_revision: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct TestGenerationRequestedV1 {
    pub run_id: Uuid,
    pub project_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestGenerationProgramV1 {
    pub run_id: Uuid,
    pub name: String,
    pub source_path: String,
    pub language: String,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestGenerationRunPayloadV1 {
    pub schema: String,
    pub generator: TestGenerationProgramV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_generator_run_id: Option<Uuid>,
    pub generator_stdin: String,
    pub accepted_solutions: Vec<TestGenerationProgramV1>,
    pub validators: Vec<TestGenerationProgramV1>,
    pub generator_limits: ToolExecutionLimitsV1,
    pub solution_limits: ToolExecutionLimitsV1,
    pub checker_run_ids: Vec<Uuid>,
    pub answer_source: studio_core::AnswerSourceV1,
    pub checker: TestLabCheckerV1,
    pub problem_type: ProblemType,
    pub requested_by: SubjectRef,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ProjectResponse {
    pub id: Uuid,
    pub title: String,
    pub problem_type: ProblemType,
    pub organization_id: Option<String>,
    pub state: ReviewState,
    pub revision: i64,
    pub authoring: AuthoringSettingsV1,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<Project> for ProjectResponse {
    fn from(project: Project) -> Self {
        Self {
            id: project.id,
            title: project.title,
            problem_type: project.problem_type,
            organization_id: project.organization_id,
            state: project.state,
            revision: project.revision,
            authoring: project.authoring,
            created_at: project.created_at,
            updated_at: project.updated_at,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ProjectPage {
    pub items: Vec<ProjectResponse>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UpsertProjectMembershipRequest {
    pub member: SubjectRef,
    pub role: ProjectRole,
    pub entitlement_version: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ProjectMembershipResponse {
    pub project_id: Uuid,
    pub member: SubjectRef,
    pub role: ProjectRole,
    pub entitlement_version: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<IdentityDirectoryEntryV1>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<ProjectMembership> for ProjectMembershipResponse {
    fn from(membership: ProjectMembership) -> Self {
        Self {
            project_id: membership.project_id,
            member: membership.member,
            role: membership.role,
            entitlement_version: membership.entitlement_version,
            profile: None,
            created_at: membership.created_at,
            updated_at: membership.updated_at,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ProjectMembershipPage {
    pub items: Vec<ProjectMembershipResponse>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct IdentityDirectoryEntryV1 {
    pub issuer: String,
    pub sub: String,
    pub username: String,
    pub display_name: String,
    pub membership_version: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct IdentityDirectoryPageV1 {
    pub schema: String,
    pub query: String,
    pub items: Vec<IdentityDirectoryEntryV1>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct QuotaStatusV1 {
    pub schema: String,
    pub month_start: String,
    pub monthly_cpu_limit_millis: i64,
    pub monthly_cpu_used_millis: i64,
    pub monthly_cpu_remaining_millis: i64,
    pub active_reserved_cpu_millis: i64,
    pub active_validations: i64,
    pub concurrent_validation_limit: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CreateWaiverRequest {
    pub issue_code: String,
    pub reason: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RevokeWaiverRequest {
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct WaiverRevocationResponse {
    pub reason: String,
    pub revoked_by: SubjectRef,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct WaiverResponse {
    pub id: Uuid,
    pub project_id: Uuid,
    pub commit_id: Uuid,
    pub validation_run_id: Uuid,
    pub manifest_digest: Sha256Digest,
    pub policy_digest: Sha256Digest,
    pub issue_code: String,
    pub issue_digest: Sha256Digest,
    pub issue_snapshot: Vec<ValidationIssue>,
    pub reason: String,
    pub approved_by: SubjectRef,
    pub entitlement_version: i64,
    pub status: WaiverStatusV1,
    pub expires_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revocation: Option<WaiverRevocationResponse>,
}

impl WaiverResponse {
    pub fn from_records(
        waiver: WaiverRecord,
        revocation: Option<WaiverRevocationRecord>,
        now: DateTime<Utc>,
    ) -> Self {
        let status = waiver.status(revocation.as_ref(), now);
        Self {
            id: waiver.id,
            project_id: waiver.project_id,
            commit_id: waiver.commit_id,
            validation_run_id: waiver.validation_run_id,
            manifest_digest: waiver.manifest_digest,
            policy_digest: waiver.policy_digest,
            issue_code: waiver.issue_code,
            issue_digest: waiver.issue_digest,
            issue_snapshot: waiver.issue_snapshot,
            reason: waiver.reason,
            approved_by: waiver.approved_by,
            entitlement_version: waiver.entitlement_version,
            status,
            expires_at: waiver.expires_at,
            created_at: waiver.created_at,
            revocation: revocation.map(|revocation| WaiverRevocationResponse {
                reason: revocation.reason,
                revoked_by: revocation.revoked_by,
                created_at: revocation.created_at,
            }),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct WaiverPage {
    pub items: Vec<WaiverResponse>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct SubmitReviewRequest {
    pub commit_id: Uuid,
    pub validation_run_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CreateReviewDecisionRequest {
    pub decision: ReviewDecisionKindV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ReviewDecisionResponse {
    pub id: Uuid,
    pub decision: ReviewDecisionKindV1,
    pub decided_by: SubjectRef,
    pub entitlement_version: i64,
    pub approval_source: ReviewApprovalSourceV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool_assignment_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl From<ReviewDecisionRecord> for ReviewDecisionResponse {
    fn from(record: ReviewDecisionRecord) -> Self {
        Self {
            id: record.id,
            decision: record.decision,
            decided_by: record.decided_by,
            entitlement_version: record.entitlement_version,
            approval_source: record.approval_source,
            pool_assignment_id: record.pool_assignment_id,
            comment: record.comment,
            created_at: record.created_at,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ReviewResponse {
    pub id: Uuid,
    pub project_id: Uuid,
    pub commit_id: Uuid,
    pub validation_run_id: Uuid,
    pub manifest_digest: Sha256Digest,
    pub status: ReviewStatusV1,
    pub submitted_by: SubjectRef,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<ReviewDecisionResponse>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ReviewPage {
    pub items: Vec<ReviewResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ReviewPoolRequestResponseV1 {
    pub id: Uuid,
    pub review_id: Uuid,
    pub project_id: Uuid,
    pub commit_id: Uuid,
    pub validation_run_id: Uuid,
    pub manifest_digest: Sha256Digest,
    pub requested_by: SubjectRef,
    pub status: ReviewPoolStatusV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_by: Option<SubjectRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignment_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer_entitlement_version: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ReviewPoolPageV1 {
    pub items: Vec<ReviewPoolRequestResponseV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Uuid>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CommentThreadStatusV1 {
    Open,
    Resolved,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CommentAnchorV1 {
    pub start: String,
    pub end: String,
    pub quote: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateCommentThreadRequest {
    pub document_id: Uuid,
    pub anchor: CommentAnchorV1,
    pub body: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateCommentReplyRequest {
    pub body: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ResolveCommentThreadRequest {
    pub resolved: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CommentResponse {
    pub id: Uuid,
    pub thread_id: Uuid,
    pub body: String,
    pub created_by: SubjectRef,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CommentThreadResponse {
    pub id: Uuid,
    pub project_id: Uuid,
    pub document_id: Uuid,
    pub path: String,
    pub anchor: CommentAnchorV1,
    pub status: CommentThreadStatusV1,
    pub created_by: SubjectRef,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_by: Option<SubjectRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<DateTime<Utc>>,
    pub comments: Vec<CommentResponse>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CommentThreadPage {
    pub items: Vec<CommentThreadResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Uuid>,
}

impl ReviewResponse {
    pub fn from_records(review: ReviewRecord, decision: Option<ReviewDecisionRecord>) -> Self {
        Self {
            id: review.id,
            project_id: review.project_id,
            commit_id: review.commit_id,
            validation_run_id: review.validation_run_id,
            manifest_digest: review.manifest_digest,
            status: review.status,
            submitted_by: review.submitted_by,
            created_at: review.created_at,
            updated_at: review.updated_at,
            decision: decision.map(Into::into),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CreateCommitRequest {
    pub message: String,
    pub manifest: VersionedReleaseManifest,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CommitResponse {
    pub id: Uuid,
    pub project_id: Uuid,
    pub sequence: i64,
    pub manifest_digest: Sha256Digest,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CommitDetailResponse {
    pub id: Uuid,
    pub project_id: Uuid,
    pub sequence: i64,
    pub manifest_digest: Sha256Digest,
    pub manifest: VersionedReleaseManifest,
    pub authored_by: SubjectRef,
    pub message: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CommitFileDownloadResponse {
    pub commit_id: Uuid,
    pub file: ManifestFile,
    pub download_url: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CommitPage {
    pub items: Vec<CommitDetailResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Uuid>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DocumentKind {
    Statement,
    Tutorial,
    Source,
    Notes,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CreateDocumentRequest {
    pub path: String,
    pub kind: DocumentKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_content: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct DocumentResponse {
    pub id: Uuid,
    pub project_id: Uuid,
    pub path: String,
    pub kind: DocumentKind,
    pub latest_sequence: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct DocumentPage {
    pub items: Vec<DocumentResponse>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CollaborationTicketResponse {
    pub ticket: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum FileUploadStatus {
    AwaitingUpload,
    Queued,
    Verifying,
    Ready,
    Rejected,
    Expired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum FileUploadStrategyV1 {
    SinglePut,
    AzureBlockV1,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct BeginFileUploadRequest {
    pub path: String,
    pub sha256: Sha256Digest,
    pub size_bytes: u64,
    pub media_type: String,
    #[serde(default)]
    pub executable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct FileEntryResponse {
    pub project_id: Uuid,
    pub path: String,
    pub sha256: Sha256Digest,
    pub size_bytes: u64,
    pub media_type: String,
    pub executable: bool,
    pub revision: i64,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct FileDownloadResponse {
    pub file: FileEntryResponse,
    pub download_url: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct FileUploadResponse {
    pub id: Uuid,
    pub project_id: Uuid,
    pub path: String,
    pub sha256: Sha256Digest,
    pub size_bytes: u64,
    pub media_type: String,
    pub executable: bool,
    pub status: FileUploadStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<FileEntryResponse>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct BeginFileUploadResponse {
    pub upload: FileUploadResponse,
    pub strategy: FileUploadStrategyV1,
    pub method: String,
    pub upload_url: String,
    pub required_headers: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub part_size_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub part_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_id_encoding: Option<String>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct FileEntryPage {
    pub items: Vec<FileEntryResponse>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ValidationRunStatus {
    Queued,
    Running,
    Passed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct EnqueueValidationRequest {
    pub commit_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ValidationRunResponse {
    pub id: Uuid,
    pub project_id: Uuid,
    pub commit_id: Uuid,
    pub status: ValidationRunStatus,
    pub plan: Vec<ValidationStagePlanV1>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ValidationRunSummaryResponse {
    pub id: Uuid,
    pub project_id: Uuid,
    pub commit_id: Uuid,
    pub status: ValidationRunStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ValidationRunPage {
    pub items: Vec<ValidationRunSummaryResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ValidationRunDetailResponse {
    pub id: Uuid,
    pub project_id: Uuid,
    pub commit_id: Uuid,
    pub status: ValidationRunStatus,
    pub plan: Vec<ValidationStagePlanV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<ValidationReportV1>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ValidationStepV1 {
    pub name: String,
    pub status: String,
    pub duration_ms: u64,
    #[serde(default)]
    pub evidence_uri: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ValidationReportV1 {
    pub schema: String,
    pub run_id: Uuid,
    pub manifest_digest: Sha256Digest,
    pub toolchain_digest: Sha256Digest,
    pub policy_digest: Sha256Digest,
    pub status: ValidationRunStatus,
    pub steps: Vec<ValidationStepV1>,
    pub issues: Vec<ValidationIssue>,
    #[serde(default)]
    pub waiver_ids: Vec<Uuid>,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseStatus {
    Queued,
    Building,
    Ready,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CreateReleaseRequest {
    pub commit_id: Uuid,
    pub validation_run_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ReleaseResponse {
    pub id: Uuid,
    pub project_id: Uuid,
    pub commit_id: Uuid,
    pub validation_run_id: Uuid,
    pub manifest_digest: Sha256Digest,
    pub status: ReleaseStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package_digest: Option<Sha256Digest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package_uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package_size_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ReleasePage {
    pub items: Vec<ReleaseResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ReleaseDownloadResponse {
    pub release_id: Uuid,
    pub package_digest: Sha256Digest,
    pub package_size_bytes: u64,
    pub download_url: String,
    pub expires_at: DateTime<Utc>,
}

pub const NATIVE_RELEASE_PACKAGE_SCHEMA_V1: &str = "reporch.native-package.v1";
pub const NATIVE_SOURCE_PACKAGE_SCHEMA_V1: &str = "reporch.native-source-package.v1";

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NativeReleasePackageMetadataV1 {
    pub schema: String,
    pub release_id: Uuid,
    pub project_id: Uuid,
    pub commit_id: Uuid,
    pub manifest_digest: Sha256Digest,
    pub validation_report_digest: Sha256Digest,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NativeSourcePackageMetadataV1 {
    pub schema: String,
    pub manifest_digest: Sha256Digest,
    pub source_profile: PackageProfile,
    pub file_count: u64,
    pub file_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PublicationStatus {
    Queued,
    Submitting,
    Received,
    QuarantinePending,
    Quarantined,
    Published,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PublicationResponse {
    pub release_id: Uuid,
    pub status: PublicationStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub django_problem_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PublicationOwnerV1 {
    pub issuer: String,
    pub sub: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PublicationRequestV1 {
    pub schema: String,
    pub release_id: Uuid,
    pub release_digest: Sha256Digest,
    pub package_uri: String,
    pub owner: PublicationOwnerV1,
    pub idempotency_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ExecutionRequestV1 {
    pub schema: String,
    pub validation_run_id: Uuid,
    /// Digest-bound candidate package identity. It is never a public release.
    pub release_id: Uuid,
    pub manifest_digest: Sha256Digest,
    pub package_digest: Sha256Digest,
    pub package_uri: String,
    pub owner: PublicationOwnerV1,
    pub idempotency_key: String,
    pub steps: Vec<String>,
    #[serde(default)]
    pub limits: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ExecutionResponseV1 {
    pub id: Uuid,
    pub validation_run_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest_digest: Option<Sha256Digest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package_digest: Option<Sha256Digest>,
    pub status: String,
    #[serde(default)]
    pub result: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolExecutionKindV1 {
    Generator,
    ReferenceSolution,
    Validator,
    Checker,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ToolExecutionLimitsV1 {
    pub timeout_ms: u64,
    pub memory_mib: u64,
    pub output_kib: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ToolExecutionRequestV1 {
    pub schema: String,
    pub run_id: Uuid,
    pub kind: ToolExecutionKindV1,
    pub source: String,
    pub language: String,
    pub stdin: String,
    pub source_digest: Sha256Digest,
    pub stdin_digest: Sha256Digest,
    pub owner: PublicationOwnerV1,
    pub limits: ToolExecutionLimitsV1,
    pub idempotency_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ToolExecutionResponseV1 {
    pub schema: String,
    pub run_id: Uuid,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_sha256: Option<Sha256Digest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_millis: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct EventEnvelopeV1 {
    pub schema: String,
    pub event_id: Uuid,
    pub event_type: String,
    pub event_version: u16,
    pub occurred_at: DateTime<Utc>,
    pub project_id: Option<Uuid>,
    pub trace_id: String,
    pub payload: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateTrustAppealRequestV1 {
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum TrustAppealStatusV1 {
    Open,
    Accepted,
    Rejected,
    Withdrawn,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct TrustAppealResponseV1 {
    pub id: Uuid,
    pub status: TrustAppealStatusV1,
    pub message: String,
    pub resolution_note: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub reviewed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct TrustAppealPageV1 {
    pub schema: String,
    pub items: Vec<TrustAppealResponseV1>,
}

impl EventEnvelopeV1 {
    pub fn new(
        event_type: impl Into<String>,
        project_id: Option<Uuid>,
        trace_id: impl Into<String>,
        payload: impl Serialize,
    ) -> Result<Self, serde_json::Error> {
        Ok(Self {
            schema: EVENT_SCHEMA_V1.into(),
            event_id: Uuid::now_v7(),
            event_type: event_type.into(),
            event_version: 1,
            occurred_at: Utc::now(),
            project_id,
            trace_id: trace_id.into(),
            payload: serde_json::to_value(payload)?,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ValidationRequestedV1 {
    pub validation_run_id: Uuid,
    pub project_id: Uuid,
    pub commit_id: Uuid,
    pub requested_by: SubjectRef,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct FileUploadFinalizeRequestedV1 {
    pub upload_id: Uuid,
    pub project_id: Uuid,
    pub requested_by: SubjectRef,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ReleaseBuildRequestedV1 {
    pub release_id: Uuid,
    pub project_id: Uuid,
    pub commit_id: Uuid,
    pub validation_run_id: Uuid,
    pub requested_by: SubjectRef,
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PublicationRequestedV1 {
    pub release_id: Uuid,
    pub project_id: Uuid,
    pub requested_by: SubjectRef,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_pool_response_matches_the_studio_v1_wire_contract() {
        let value = serde_json::json!({
            "id": "019ffa58-1000-7000-8000-000000000003",
            "review_id": "019ffa58-1000-7000-8000-000000000002",
            "project_id": "019ffa57-39c6-74d1-8b38-61dd782c22ff",
            "commit_id": "019ffa57-f916-7500-8db2-9006ec779a39",
            "validation_run_id": "019ffa58-1000-7000-8000-000000000001",
            "manifest_digest": "92914fe2395158e38da08b6dbe257d4f60a592980d3f9c06dc28a78a019ba07b",
            "requested_by": {
                "issuer": "https://reporch.com/oauth",
                "subject": "author"
            },
            "status": "claimed",
            "claimed_by": {
                "issuer": "https://reporch.com/oauth",
                "subject": "reviewer"
            },
            "assignment_id": "019ffa58-ca85-71b1-8aa7-5dcd38619851",
            "reviewer_entitlement_version": 9,
            "created_at": "2026-08-13T08:58:00Z",
            "updated_at": "2026-08-13T08:59:00Z"
        });
        let response: ReviewPoolRequestResponseV1 = serde_json::from_value(value).unwrap();
        assert_eq!(response.status, ReviewPoolStatusV1::Claimed);
        assert_eq!(response.reviewer_entitlement_version, Some(9));
        assert!(response.assignment_id.is_some());
    }
}
