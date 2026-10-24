use memory_domain::{claim_sha256, normalize_v1, MemoryKind};
use memory_file_format::claim::ClaimDocument;
use memory_file_format::quarantine::ReasonCode;

const GOOD: &[u8] = include_bytes!("golden/claim.md");
const BAD: &[u8] = include_bytes!("golden/claim.bad.md");

#[test]
fn claim_golden_round_trip_is_byte_stable() {
    let parsed = ClaimDocument::parse(GOOD).unwrap();
    assert_eq!(parsed.to_bytes().unwrap(), GOOD);
}

#[test]
fn claim_bad_hash_is_quarantined() {
    let error = ClaimDocument::parse(BAD).unwrap_err();
    assert_eq!(error.code, ReasonCode::HashMismatch);
    assert_eq!(error.field, Some("claim_sha256"));
}

#[test]
fn claim_front_matter_order_is_fixed() {
    let text = std::str::from_utf8(GOOD).unwrap();
    let changed = text.replace(
        "domain_id: user_main\nkind: preference",
        "kind: preference\ndomain_id: user_main",
    );
    assert_eq!(
        ClaimDocument::parse(changed.as_bytes()).unwrap_err().code,
        ReasonCode::InvalidSyntax
    );
}

#[test]
fn claim_body_bytes_are_not_normalized() {
    let mut document = ClaimDocument::parse(GOOD).unwrap();
    document.body = "Cafe\u{301}\r\n".into();
    document.front.normalized_claim = normalize_v1(&document.body);
    document.front.claim_sha256 = claim_sha256(MemoryKind::Preference, &document.body);
    let bytes = document.to_bytes().unwrap();
    let parsed = ClaimDocument::parse(&bytes).unwrap();
    assert_eq!(parsed.body.as_bytes(), b"Cafe\xCC\x81\r\n");
}

#[test]
fn claim_hash_delegates_to_current_memory_domain_definition() {
    assert_eq!(
        claim_sha256(MemoryKind::Preference, "  A  "),
        claim_sha256(MemoryKind::Preference, "a")
    );
}

#[test]
fn claim_full_evidence_reference_uses_paired_null_spans() {
    let text = std::str::from_utf8(GOOD).unwrap();
    let changed = text
        .replace("start_byte: 0", "start_byte: null")
        .replace("end_byte: 42", "end_byte: null");
    let parsed = ClaimDocument::parse(changed.as_bytes()).unwrap();
    assert_eq!(parsed.front.evidence[0].start_byte, None);
    assert_eq!(parsed.to_bytes().unwrap(), changed.as_bytes());
}

#[test]
fn claim_slot_and_precedence_round_trip() {
    let mut doc = ClaimDocument::parse(GOOD).unwrap();
    doc.front.slot = Some("reading_preference".into());
    doc.front.precedence = Some("session".into());
    let bytes = doc.to_bytes().unwrap();
    let text = std::str::from_utf8(&bytes).unwrap();
    assert!(text.contains("slot: \"reading_preference\"\n"));
    assert!(text.contains("precedence: session\n"));
    let reparsed = ClaimDocument::parse(&bytes).unwrap();
    assert_eq!(reparsed.front.slot.as_deref(), Some("reading_preference"));
    assert_eq!(reparsed.front.precedence.as_deref(), Some("session"));
    assert_eq!(reparsed.to_bytes().unwrap(), bytes);
}
