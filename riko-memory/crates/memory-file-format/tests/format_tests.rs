use memory_file_format::format::FormatDefinition;
use memory_file_format::quarantine::ReasonCode;

const GOOD: &[u8] = include_bytes!("golden/FORMAT.toml");
const BAD: &[u8] = include_bytes!("golden/FORMAT.bad.toml");

#[test]
fn format_golden_round_trip_is_byte_stable() {
    let parsed = FormatDefinition::parse(GOOD).unwrap();
    assert_eq!(parsed.to_bytes().unwrap(), GOOD);
}

#[test]
fn format_rejects_unknown_version() {
    let error = FormatDefinition::parse(BAD).unwrap_err();
    assert_eq!(error.code, ReasonCode::UnsupportedFormat);
}

#[test]
fn format_read_and_write_file() {
    let path = std::env::temp_dir().join(format!(
        "riko-f1-format-{}-{}.toml",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    FormatDefinition::default().write(&path).unwrap();
    assert_eq!(
        FormatDefinition::read(&path).unwrap(),
        FormatDefinition::default()
    );
    assert_eq!(std::fs::read(&path).unwrap(), GOOD);
    std::fs::remove_file(path).unwrap();
}
