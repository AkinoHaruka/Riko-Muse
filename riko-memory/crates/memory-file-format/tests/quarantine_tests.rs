use memory_file_format::quarantine::{
    validate_identity, CorpusValidator, FormatError, QuarantineReport, ReasonCode,
};

#[test]
fn duplicate_ids_have_stable_reason_code() {
    let mut corpus = CorpusValidator::default();
    corpus.add_object("id-1", 1).unwrap();
    assert_eq!(
        corpus.add_object("id-1", 1).unwrap_err().code,
        ReasonCode::DuplicateId
    );
}

#[test]
fn version_regression_has_stable_reason_code() {
    let mut corpus = CorpusValidator::default();
    corpus.add_object("id-1", 3).unwrap();
    assert_eq!(
        corpus.require_newer_version("id-1", 3, 3).unwrap_err().code,
        ReasonCode::VersionRegression
    );
}

#[test]
fn id_domain_and_actor_tampering_have_distinct_codes() {
    assert_eq!(
        validate_identity("id", "a", "b").unwrap_err().code,
        ReasonCode::IdMismatch
    );
    assert_eq!(
        validate_identity("domain_id", "a", "b").unwrap_err().code,
        ReasonCode::DomainMismatch
    );
    assert_eq!(
        validate_identity("actor_id", "a", "b").unwrap_err().code,
        ReasonCode::ActorMismatch
    );
}

#[test]
fn quarantine_report_does_not_include_private_body() {
    let error = FormatError::new(ReasonCode::HashMismatch, Some("claim_sha256"));
    let report = QuarantineReport::from_error("claim.md", Some("id-1"), &error);
    let json = serde_json::to_string(&report).unwrap();
    assert_eq!(report.reason_code, "hash_mismatch");
    assert!(!json.contains("private body"));
}
