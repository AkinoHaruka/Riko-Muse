use memory_file_format::json_files::{ExpectedIdentity, FenceFile, OperationFile, ReceiptFile};
use memory_file_format::quarantine::ReasonCode;

const OPERATION: &[u8] = include_bytes!("golden/operation.json");
const BAD_OPERATION: &[u8] = include_bytes!("golden/operation.bad.json");
const RECEIPT: &[u8] = include_bytes!("golden/receipt.json");
const BAD_RECEIPT: &[u8] = include_bytes!("golden/receipt.bad.json");
const FENCE: &[u8] = include_bytes!("golden/fence.json");
const BAD_FENCE: &[u8] = include_bytes!("golden/fence.bad.json");

#[test]
fn operation_golden_round_trip_is_byte_stable() {
    let parsed = OperationFile::parse(OPERATION).unwrap();
    assert_eq!(parsed.to_bytes().unwrap(), OPERATION);
}

#[test]
fn operation_bad_format_is_quarantined() {
    assert_eq!(
        OperationFile::parse(BAD_OPERATION).unwrap_err().code,
        ReasonCode::UnsupportedFormat
    );
}

#[test]
fn receipt_golden_round_trip_is_byte_stable() {
    let parsed = ReceiptFile::parse(RECEIPT).unwrap();
    assert_eq!(parsed.response_json, "{\"version\":4}");
    assert_eq!(parsed.to_bytes().unwrap(), RECEIPT);
}

#[test]
fn receipt_bad_idempotency_key_is_quarantined() {
    let error = ReceiptFile::parse(BAD_RECEIPT).unwrap_err();
    assert_eq!(error.code, ReasonCode::InvalidField);
    assert_eq!(error.field, Some("idempotency_key"));
}

#[test]
fn fence_golden_round_trip_is_byte_stable() {
    let parsed = FenceFile::parse(FENCE).unwrap();
    assert_eq!(parsed.to_bytes().unwrap(), FENCE);
}

#[test]
fn forget_fence_without_claim_hash_is_quarantined() {
    let error = FenceFile::parse(BAD_FENCE).unwrap_err();
    assert_eq!(error.code, ReasonCode::MissingField);
    assert_eq!(error.field, Some("claim_sha256"));
}

#[test]
fn operation_scope_tampering_has_machine_reason() {
    let parsed = OperationFile::parse(OPERATION).unwrap();
    let error = parsed
        .validate_identity(ExpectedIdentity {
            id: Some("op-1"),
            scope_id: "other-scope",
            domain_id: Some("user_main"),
            actor_kind: Some("user"),
            actor_id: Some("user-1"),
        })
        .unwrap_err();
    assert_eq!(error.code, ReasonCode::ScopeMismatch);
}

#[test]
fn fence_actor_tampering_has_machine_reason() {
    let parsed = FenceFile::parse(FENCE).unwrap();
    let error = parsed
        .validate_identity(ExpectedIdentity {
            id: Some("fence-1"),
            scope_id: "scope-1",
            domain_id: Some("user_main"),
            actor_kind: Some("admin"),
            actor_id: Some("user-1"),
        })
        .unwrap_err();
    assert_eq!(error.code, ReasonCode::ActorMismatch);
}
