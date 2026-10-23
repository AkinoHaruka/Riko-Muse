use memory_file_format::evidence::{DailyNote, EvidenceBlock, EvidenceHeader};
use memory_file_format::quarantine::ReasonCode;

const GOOD: &[u8] = include_bytes!("golden/evidence.md");
const BAD: &[u8] = include_bytes!("golden/evidence.bad.md");

fn header(id: &str) -> EvidenceHeader {
    EvidenceHeader {
        id: id.to_owned(),
        host_id: "dsh".into(),
        agent_id: "riko".into(),
        session_id: "s".into(),
        event_seq: 1,
        role: "user".into(),
        source_kind: "user".into(),
        domain_id: "user_main".into(),
        occurred_at: "2026-10-23T00:00:00.000000Z".into(),
        content_bytes: 0,
        content_sha256: String::new(),
        received_at: "2026-10-23T00:00:01.000000Z".into(),
    }
}

#[test]
fn evidence_golden_round_trip_is_byte_stable() {
    let parsed = DailyNote::parse(GOOD).unwrap();
    assert_eq!(parsed.to_bytes().unwrap(), GOOD);
}

#[test]
fn evidence_payload_end_marker_does_not_truncate() {
    let parsed = DailyNote::parse(GOOD).unwrap();
    assert!(parsed.blocks[0]
        .payload
        .contains("<!-- riko:end-evidence -->"));
    assert_eq!(parsed.blocks[0].payload.len(), 64);
}

#[test]
fn evidence_bad_hash_is_quarantined() {
    let error = DailyNote::parse(BAD).unwrap_err();
    assert_eq!(error.code, ReasonCode::HashMismatch);
    assert_eq!(error.field, Some("content_sha256"));
}

#[test]
fn evidence_duplicate_id_is_quarantined() {
    let block = EvidenceBlock::new(header("same"), "one".into());
    let note = DailyNote {
        date: "2026-10-23".into(),
        blocks: vec![block.clone(), block],
    };
    assert_eq!(note.to_bytes().unwrap_err().code, ReasonCode::DuplicateId);
}

#[test]
fn evidence_payload_bytes_are_not_normalized() {
    let payload = "e\u{301}\r\n<!-- riko:end-evidence -->".to_owned();
    let note = DailyNote {
        date: "2026-10-23".into(),
        blocks: vec![EvidenceBlock::new(header("raw"), payload.clone())],
    };
    let bytes = note.to_bytes().unwrap();
    let parsed = DailyNote::parse(&bytes).unwrap();
    assert_eq!(parsed.blocks[0].payload.as_bytes(), payload.as_bytes());
}
