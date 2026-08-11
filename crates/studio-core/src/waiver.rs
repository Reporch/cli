use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{Sha256Digest, SubjectRef, ValidationIssue};

pub const MAX_WAIVER_LIFETIME_DAYS: i64 = 90;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum WaiverStatusV1 {
    Active,
    Expired,
    Revoked,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaiverRecord {
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
    pub expires_at: DateTime<Utc>,
    pub idempotency_key: String,
    pub created_at: DateTime<Utc>,
}

impl WaiverRecord {
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        project_id: Uuid,
        commit_id: Uuid,
        validation_run_id: Uuid,
        manifest_digest: Sha256Digest,
        policy_digest: Sha256Digest,
        issue_code: String,
        issue_digest: Sha256Digest,
        issue_snapshot: Vec<ValidationIssue>,
        reason: String,
        approved_by: SubjectRef,
        entitlement_version: i64,
        expires_at: DateTime<Utc>,
        idempotency_key: String,
        now: DateTime<Utc>,
    ) -> Result<Self, WaiverError> {
        let issue_code = issue_code.trim().to_owned();
        let reason = reason.trim().to_owned();
        if issue_code.is_empty() || issue_code.len() > 128 || issue_snapshot.is_empty() {
            return Err(WaiverError::InvalidIssue);
        }
        if !(20..=2_000).contains(&reason.len()) {
            return Err(WaiverError::InvalidReason);
        }
        if entitlement_version < 0 {
            return Err(WaiverError::InvalidEntitlementVersion);
        }
        if expires_at <= now || expires_at > now + Duration::days(MAX_WAIVER_LIFETIME_DAYS) {
            return Err(WaiverError::InvalidExpiry);
        }
        Ok(Self {
            id: Uuid::now_v7(),
            project_id,
            commit_id,
            validation_run_id,
            manifest_digest,
            policy_digest,
            issue_code,
            issue_digest,
            issue_snapshot,
            reason,
            approved_by,
            entitlement_version,
            expires_at,
            idempotency_key,
            created_at: now,
        })
    }

    pub fn status(
        &self,
        revocation: Option<&WaiverRevocationRecord>,
        now: DateTime<Utc>,
    ) -> WaiverStatusV1 {
        if revocation.is_some() {
            WaiverStatusV1::Revoked
        } else if self.expires_at <= now {
            WaiverStatusV1::Expired
        } else {
            WaiverStatusV1::Active
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaiverRevocationRecord {
    pub id: Uuid,
    pub waiver_id: Uuid,
    pub reason: String,
    pub revoked_by: SubjectRef,
    pub idempotency_key: String,
    pub created_at: DateTime<Utc>,
}

impl WaiverRevocationRecord {
    pub fn create(
        waiver_id: Uuid,
        reason: String,
        revoked_by: SubjectRef,
        idempotency_key: String,
        now: DateTime<Utc>,
    ) -> Result<Self, WaiverError> {
        let reason = reason.trim().to_owned();
        if !(10..=2_000).contains(&reason.len()) {
            return Err(WaiverError::InvalidRevocationReason);
        }
        Ok(Self {
            id: Uuid::now_v7(),
            waiver_id,
            reason,
            revoked_by,
            idempotency_key,
            created_at: now,
        })
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum WaiverError {
    #[error("waiver issue must identify at least one warning and be at most 128 bytes")]
    InvalidIssue,
    #[error("waiver reason must contain 20 to 2000 bytes")]
    InvalidReason,
    #[error("waiver expiry must be in the future and at most 90 days away")]
    InvalidExpiry,
    #[error("waiver entitlement version must be non-negative")]
    InvalidEntitlementVersion,
    #[error("waiver revocation reason must contain 10 to 2000 bytes")]
    InvalidRevocationReason,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{IssueSeverity, ValidationIssue};

    fn warning() -> ValidationIssue {
        ValidationIssue {
            code: "tests.high_similarity".into(),
            severity: IssueSeverity::Warning,
            message: "two tests are highly similar".into(),
            path: Some("tests/002.in".into()),
        }
    }

    #[test]
    fn waiver_requires_a_bounded_reason_and_expiry() {
        let now = Utc::now();
        let result = WaiverRecord::create(
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Sha256Digest::from_bytes(b"manifest"),
            Sha256Digest::from_bytes(b"policy"),
            "tests.high_similarity".into(),
            Sha256Digest::from_bytes(b"issue"),
            vec![warning()],
            "Reviewed test intent and accepted the overlap.".into(),
            SubjectRef {
                issuer: "https://id.example".into(),
                subject: "reviewer".into(),
            },
            4,
            now + Duration::days(30),
            "waiver-idempotency".into(),
            now,
        )
        .unwrap();
        assert_eq!(result.status(None, now), WaiverStatusV1::Active);
        assert_eq!(
            result.status(None, result.expires_at),
            WaiverStatusV1::Expired
        );
    }
}
