#![no_main]
#![forbid(unsafe_code)]

use blink::{
    search::{Options, prepare},
    source::{Control, Source},
};
use libfuzzer_sys::fuzz_target;
use std::sync::OnceLock;

fuzz_target!(|data: &[u8]| {
    static ROOT: OnceLock<tempfile::TempDir> = OnceLock::new();
    let root = ROOT.get_or_init(|| tempfile::tempdir().unwrap());
    std::fs::write(root.path().join("source.rs"), data).unwrap();
    let source = Source::open(root.path()).unwrap();
    let prepared = prepare(
        &source,
        "where is a value changed",
        &Options::default(),
        &mut || Control::Continue,
    );
    let eligible = std::str::from_utf8(data).is_ok()
        && !data.iter().any(|byte| *byte < 0x20 && !matches!(*byte, b'\n' | b'\r' | b'\t' | 0x0c))
        && data.len() <= 1024 * 1024;
    assert!(prepared.coverage().complete);
    assert_eq!(prepared.coverage().files_included, usize::from(eligible));
    assert!(prepared.candidate_count() <= Options::default().candidate_cap());
    if eligible && !data.is_empty() {
        assert!(prepared.window_count() > 0);
    } else {
        assert_eq!(prepared.window_count(), 0);
    }
});
