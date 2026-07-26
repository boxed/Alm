//! Differential type-error tests: each `type_errors/<name>.elm` fixture must
//! produce diagnostics byte-identical to `type_errors/<name>.txt`, which is the
//! exact stderr of `elm make` 0.19.1 on the same source. This pins alm's type
//! errors to the official compiler's (`Reporting.Error.Type`), the way
//! `parse_error_test` pins its syntax errors.
//!
//! Fixtures still known to differ are listed in `KNOWN_DIFFERENT`; the test
//! fails if one of those starts matching (delete it from the list) as well as
//! if a matching one regresses.

use std::fs;
use std::path::Path;

/// Fixtures alm does not yet render exactly like elm. Each entry is a category
/// of `Reporting.Error.Type` that has not been ported.
const KNOWN_DIFFERENT: &[&str] = &[
    "case_pattern_mismatch",
    "field_typo_hint",
    "if_condition",
    "infinite_type",
    "record_access_non_record",
    "record_missing_field",
    "record_update_field",
];

fn render(source: &str) -> String {
    match alm_compiler::compile_named_typed(source, "Main", false) {
        Ok(_) => panic!("expected a type error, but compilation succeeded"),
        Err(reports) => {
            let mut out = String::new();
            for report in &reports {
                out.push_str(&report.render("src/Main.elm", source));
            }
            out
        }
    }
}

#[test]
fn type_errors_match_elm() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/type_errors");
    let mut fixtures: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|e| e == "elm").unwrap_or(false))
        .collect();
    fixtures.sort();
    assert!(!fixtures.is_empty(), "no fixtures found in {}", dir.display());

    let mut regressed = Vec::new();
    let mut newly_matching = Vec::new();
    for elm in &fixtures {
        let name = elm.file_stem().unwrap().to_string_lossy().to_string();
        let source = fs::read_to_string(elm).unwrap();
        let expected = fs::read_to_string(elm.with_extension("txt")).unwrap();
        let got = render(&source);
        let known = KNOWN_DIFFERENT.contains(&name.as_str());
        match (got == expected, known) {
            (true, true) => newly_matching.push(name),
            (false, false) => regressed.push(format!(
                "\n=== {} ===\n--- expected (elm) ---\n{}\n--- got (alm) ---\n{}",
                name, expected, got
            )),
            _ => {}
        }
    }
    assert!(
        newly_matching.is_empty(),
        "these fixtures now match elm — remove them from KNOWN_DIFFERENT: {newly_matching:?}"
    );
    assert!(regressed.is_empty(), "{} fixture(s) differ:{}", regressed.len(), regressed.join(""));
}
