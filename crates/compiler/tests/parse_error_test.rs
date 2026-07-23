//! Differential parse-error tests: each `parse_errors/<name>.elm` fixture must
//! produce a diagnostic byte-identical to `parse_errors/<name>.txt`, which is
//! the exact stderr of `elm make` 0.19.1 on the same source. This pins alm's
//! syntax errors to the official compiler's (`Reporting.Error.Syntax`).

use std::fs;
use std::path::Path;

/// Compile a fixture source and render its diagnostics the way `elm make` does
/// (path `src/Main.elm`), returning the exact report text.
fn render(source: &str) -> String {
    match alm_compiler::compile(source) {
        Ok(_) => panic!("expected a parse error, but compilation succeeded"),
        Err(reports) => reports
            .iter()
            .map(|r| r.render("src/Main.elm", source))
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

#[test]
fn parse_errors_match_elm() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/parse_errors");
    let mut fixtures: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|e| e == "elm").unwrap_or(false))
        .collect();
    fixtures.sort();
    assert!(!fixtures.is_empty(), "no fixtures found in {}", dir.display());

    let mut failures = Vec::new();
    for elm in &fixtures {
        let name = elm.file_stem().unwrap().to_string_lossy().to_string();
        let source = fs::read_to_string(elm).unwrap();
        let expected = fs::read_to_string(elm.with_extension("txt")).unwrap();
        let got = render(&source);
        if got != expected {
            failures.push(format!(
                "\n=== {} ===\n--- expected (elm) ---\n{}\n--- got (alm) ---\n{}",
                name, expected, got
            ));
        }
    }
    assert!(failures.is_empty(), "{} fixture(s) differ:{}", failures.len(), failures.join(""));
}
