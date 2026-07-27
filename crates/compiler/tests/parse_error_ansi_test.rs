//! Differential test for the *colored* rendering of syntax errors.
//!
//! `parse_error_test` pins the plain text; this pins the ANSI a terminal gets.
//! Each `parse_errors_ansi/<name>.txt` is `elm make` 0.19.1's exact terminal
//! output for the matching `parse_errors/<name>.elm`, captured under a pty.
//!
//! Besides the shared chrome — dull-cyan header bar, vivid-red carets and `>`
//! markers, underlined `Hint`/`Note` labels — elm colors words inside the
//! reports: Elm keywords (vivid cyan), code fragments and type names (dull
//! yellow), suggested syntax (vivid green) and constructors in example code
//! (vivid blue). Which words those are is decided separately in each report of
//! `Reporting.Error.Syntax`, so they are written out rather than inferred.
//!
//! `NEEDS_INLINE_COLOR` is empty: every fixture matches. The test still fails
//! if a listed one starts matching, so the list cannot silently rot.
//!
//! `pkg_*` fixtures are excluded: they need a package `elm.json`, which the
//! plain-text suite handles by compiling them differently.

use std::fs;
use std::path::Path;

/// Fixtures whose bodies do not yet match. Empty — kept so a regression can
/// be quarantined deliberately rather than by deleting the assertion.
const NEEDS_INLINE_COLOR: &[&str] = &[];

fn render_ansi(source: &str, is_package: bool) -> String {
    match alm_compiler::compile_named_typed(source, "Main", is_package) {
        Ok(_) => panic!("expected a parse error, but compilation succeeded"),
        Err(reports) => reports
            .iter()
            .map(|r| r.render_ansi("src/Main.elm", source))
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

/// Escapes shown as `\e[..m` so a failure diff is readable.
fn visible(text: &str) -> String {
    text.replace('\u{1b}', "\\e")
}

#[test]
fn colored_parse_errors_match_elm() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/parse_errors");
    let ansi_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/parse_errors_ansi");
    let mut fixtures: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|e| e == "elm").unwrap_or(false))
        .filter(|p| !p.file_stem().unwrap().to_string_lossy().starts_with("pkg_"))
        .collect();
    fixtures.sort();
    assert!(!fixtures.is_empty(), "no fixtures found in {}", dir.display());

    let mut regressed = Vec::new();
    let mut newly_matching = Vec::new();
    for elm in &fixtures {
        let name = elm.file_stem().unwrap().to_string_lossy().to_string();
        let expected_path = ansi_dir.join(format!("{name}.txt"));
        let Ok(expected) = fs::read_to_string(&expected_path) else {
            panic!("missing ANSI fixture {}", expected_path.display());
        };
        let source = fs::read_to_string(elm).unwrap();
        let got = render_ansi(&source, false);
        let known = NEEDS_INLINE_COLOR.contains(&name.as_str());
        match (got == expected, known) {
            (true, true) => newly_matching.push(name),
            (false, false) => regressed.push(format!(
                "\n=== {} ===\n--- expected (elm) ---\n{}\n--- got (alm) ---\n{}",
                name,
                visible(&expected),
                visible(&got)
            )),
            _ => {}
        }
    }
    assert!(
        newly_matching.is_empty(),
        "these fixtures now match elm — remove them from NEEDS_INLINE_COLOR: {newly_matching:?}"
    );
    assert!(regressed.is_empty(), "{} fixture(s) differ:{}", regressed.len(), regressed.join(""));
}
