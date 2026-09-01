use super::{
    INIT_TRANSACTION_SCHEMA_V1, InitTransactionFile, InitTransactionJournal, TemplateFile,
    validate_init_journal_against_templates,
};
use uuid::Uuid;

#[test]
fn interrupted_older_template_can_be_recovered_after_new_starter_files_are_added() {
    let existing = TemplateFile::text("existing.txt", "existing", "text/plain");
    let added_later = TemplateFile::text("added-later.txt", "new", "text/plain");
    let transaction_id = Uuid::now_v7();
    let journal = InitTransactionJournal {
        schema: INIT_TRANSACTION_SCHEMA_V1.into(),
        transaction_id,
        files: vec![InitTransactionFile {
            path: existing.path.into(),
            temporary_path: format!(".reporch-init-{}-0.tmp", transaction_id.simple()),
            sha256: studio_core::Sha256Digest::from_bytes(&existing.content),
            size_bytes: existing.content.len() as u64,
        }],
        directories: vec![],
    };

    validate_init_journal_against_templates(&journal, &[existing, added_later]).unwrap();
}

#[test]
fn interrupted_template_with_an_unknown_path_remains_rejected() {
    let expected = TemplateFile::text("expected.txt", "expected", "text/plain");
    let transaction_id = Uuid::now_v7();
    let journal = InitTransactionJournal {
        schema: INIT_TRANSACTION_SCHEMA_V1.into(),
        transaction_id,
        files: vec![InitTransactionFile {
            path: "unknown.txt".into(),
            temporary_path: format!(".reporch-init-{}-0.tmp", transaction_id.simple()),
            sha256: studio_core::Sha256Digest::from_bytes(b"unknown"),
            size_bytes: 7,
        }],
        directories: vec![],
    };

    let error = validate_init_journal_against_templates(&journal, &[expected]).unwrap_err();
    assert!(error.to_string().contains("does not match this template"));
}
