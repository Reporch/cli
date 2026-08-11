use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use utoipa::ToSchema;
use uuid::Uuid;

use crate::{Sha256Digest, SubjectRef};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReviewStatusV1 {
    InReview,
    ChangesRequested,
    Approved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDecisionKindV1 {
    Approve,
    RequestChanges,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewRecord {
    pub id: Uuid,
    pub project_id: Uuid,
    pub commit_id: Uuid,
    pub validation_run_id: Uuid,
    pub manifest_digest: Sha256Digest,
    pub status: ReviewStatusV1,
    pub submitted_by: SubjectRef,
    pub idempotency_key: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl ReviewRecord {
    pub fn submit(
        project_id: Uuid,
        commit_id: Uuid,
        validation_run_id: Uuid,
        manifest_digest: Sha256Digest,
        submitted_by: SubjectRef,
        idempotency_key: String,
        now: DateTime<Utc>,
    ) -> Self {
        Self {
            id: Uuid::now_v7(),
            project_id,
            commit_id,
            validation_run_id,
            manifest_digest,
            status: ReviewStatusV1::InReview,
            submitted_by,
            idempotency_key,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn apply_decision(
        &mut self,
        decision: ReviewDecisionKindV1,
        now: DateTime<Utc>,
    ) -> Result<(), ReviewError> {
        if self.status != ReviewStatusV1::InReview {
            return Err(ReviewError::AlreadyDecided);
        }
        self.status = match decision {
            ReviewDecisionKindV1::Approve => ReviewStatusV1::Approved,
            ReviewDecisionKindV1::RequestChanges => ReviewStatusV1::ChangesRequested,
        };
        self.updated_at = now;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewDecisionRecord {
    pub id: Uuid,
    pub review_id: Uuid,
    pub decision: ReviewDecisionKindV1,
    pub decided_by: SubjectRef,
    pub entitlement_version: i64,
    pub comment: Option<String>,
    pub idempotency_key: String,
    pub created_at: DateTime<Utc>,
}

impl ReviewDecisionRecord {
    pub fn create(
        review_id: Uuid,
        decision: ReviewDecisionKindV1,
        decided_by: SubjectRef,
        entitlement_version: i64,
        comment: Option<String>,
        idempotency_key: String,
        now: DateTime<Utc>,
    ) -> Result<Self, ReviewError> {
        let comment = comment
            .map(|comment| comment.trim().to_owned())
            .filter(|comment| !comment.is_empty());
        if entitlement_version < 0
            || comment
                .as_ref()
                .is_some_and(|comment| comment.len() > 4_000)
        {
            return Err(ReviewError::InvalidComment);
        }
        if decision == ReviewDecisionKindV1::RequestChanges && comment.is_none() {
            return Err(ReviewError::ChangesCommentRequired);
        }
        Ok(Self {
            id: Uuid::now_v7(),
            review_id,
            decision,
            decided_by,
            entitlement_version,
            comment,
            idempotency_key,
            created_at: now,
        })
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ReviewError {
    #[error("review has already received a final decision")]
    AlreadyDecided,
    #[error("review comment must not exceed 4000 bytes")]
    InvalidComment,
    #[error("requesting changes requires an explanatory comment")]
    ChangesCommentRequired,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn actor() -> SubjectRef {
        SubjectRef {
            issuer: "https://reporch.test/oauth".into(),
            subject: "reviewer".into(),
        }
    }

    #[test]
    fn decision_is_terminal_and_changes_require_a_reason() {
        let now = Utc::now();
        let digest = "a".repeat(64).parse::<Sha256Digest>().unwrap();
        let mut review = ReviewRecord::submit(
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            digest,
            actor(),
            "review-key".into(),
            now,
        );
        assert!(matches!(
            ReviewDecisionRecord::create(
                review.id,
                ReviewDecisionKindV1::RequestChanges,
                actor(),
                1,
                None,
                "decision-key".into(),
                now,
            ),
            Err(ReviewError::ChangesCommentRequired)
        ));
        review
            .apply_decision(ReviewDecisionKindV1::Approve, now)
            .unwrap();
        assert_eq!(review.status, ReviewStatusV1::Approved);
        assert_eq!(
            review.apply_decision(ReviewDecisionKindV1::RequestChanges, now),
            Err(ReviewError::AlreadyDecided)
        );
    }
}
