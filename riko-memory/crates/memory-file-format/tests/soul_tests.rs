use memory_domain::normalize_v1;
use memory_file_format::quarantine::ReasonCode;
use memory_file_format::soul::SoulDocument;
use sha2::Digest;

const GOOD: &[u8] = include_bytes!("golden/soul.md");
const BAD: &[u8] = include_bytes!("golden/soul.bad.md");

#[test]
fn soul_golden_round_trip_is_byte_stable() {
    let parsed = SoulDocument::parse(GOOD).unwrap();
    assert_eq!(parsed.to_bytes().unwrap(), GOOD);
}

#[test]
fn soul_bad_actor_or_hash_is_quarantined() {
    let error = SoulDocument::parse(BAD).unwrap_err();
    assert!(matches!(
        error.code,
        ReasonCode::InvalidField | ReasonCode::HashMismatch
    ));
}

#[test]
fn soul_rejects_more_than_2000_unicode_scalars() {
    let mut document = SoulDocument::parse(GOOD).unwrap();
    document.body = "界".repeat(2001);
    assert_eq!(document.to_bytes().unwrap_err().field, Some("body"));
}

#[test]
fn soul_body_bytes_are_not_normalized() {
    let mut document = SoulDocument::parse(GOOD).unwrap();
    document.body = "e\u{301}\r\n".into();
    document.front.body_sha256 = hex::encode(sha2::Sha256::digest(document.body.as_bytes()));
    let bytes = document.to_bytes().unwrap();
    let parsed = SoulDocument::parse(&bytes).unwrap();
    assert_ne!(parsed.body, normalize_v1(&parsed.body));
    assert_eq!(parsed.body.as_bytes(), b"e\xCC\x81\r\n");
}
