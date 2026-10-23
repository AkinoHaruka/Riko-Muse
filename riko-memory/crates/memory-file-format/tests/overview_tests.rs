use memory_file_format::overview::MemoryOverview;
use memory_file_format::quarantine::{CorpusValidator, ReasonCode};

const GOOD: &[u8] = include_bytes!("golden/MEMORY.md");
const BAD: &[u8] = include_bytes!("golden/MEMORY.bad.md");

#[test]
fn overview_parses_curated_and_compact_annotations() {
    let parsed = MemoryOverview::parse(GOOD).unwrap();
    assert_eq!(parsed.curated.len(), 1);
    assert_eq!(parsed.curated[0].version, 3);
    assert_eq!(parsed.derived.len(), 1);
    assert_eq!(parsed.derived[0].generator, "compact_v1");
}

#[test]
fn overview_bad_ref_version_is_quarantined() {
    let error = MemoryOverview::parse(BAD).unwrap_err();
    assert!(matches!(
        error.code,
        ReasonCode::InvalidSyntax | ReasonCode::InvalidField
    ));
}

#[test]
fn overview_duplicate_curated_id_is_quarantined() {
    let text = std::str::from_utf8(GOOD).unwrap();
    let marker = "<!-- riko:ref memory=019a12ab-4c00-7c01-9000-111111111111 version=3 -->";
    let changed = text.replace(marker, &format!("{marker}\n{marker}"));
    assert_eq!(
        MemoryOverview::parse(changed.as_bytes()).unwrap_err().code,
        ReasonCode::DuplicateId
    );
}

#[test]
fn overview_dangling_ref_is_reported_by_corpus_validator() {
    let parsed = MemoryOverview::parse(GOOD).unwrap();
    let mut corpus = CorpusValidator::default();
    for reference in parsed.curated {
        corpus.add_reference("MEMORY.md", &reference.memory_id);
    }
    assert_eq!(
        corpus.validate_references().unwrap_err().code,
        ReasonCode::DanglingReference
    );
}
