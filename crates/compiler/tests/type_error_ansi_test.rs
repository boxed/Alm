//! Differential test for the *colored* rendering of type errors.
//!
//! `type_error_test` pins the plain text a redirected `alm make` writes.
//! In a terminal elm also emits ANSI escapes — a dull-cyan header bar, vivid-red
//! carets, dull-yellow for the types at fault, vivid green for the thing it
//! suggests you use instead, and an underlined `Hint`/`Note` label. Each
//! `type_errors_ansi/<name>.txt` is `elm make` 0.19.1's exact terminal output
//! for the matching `type_errors/<name>.elm`, captured under a pty.
//!
//! Fixtures listed in `KNOWN_DIFFERENT` here are the ones whose *plain* text
//! already differs (see `type_error_test`), so their colors cannot match
//! either.

use std::fs;
use std::path::Path;

const KNOWN_DIFFERENT: &[&str] = &[
    // Reports a mismatch where elm reports INFINITE TYPE; see
    // `type_error_test` for why.
    "infinite_type",
];

fn render_ansi(source: &str) -> String {
    match alm_compiler::compile_named_typed(source, "Main", false) {
        Ok(_) => panic!("expected a type error, but compilation succeeded"),
        Err(reports) => {
            let mut out = String::new();
            for report in &reports {
                out.push_str(&report.render_ansi("src/Main.elm", source));
            }
            out
        }
    }
}

/// Escapes shown as `\e[..m` so a failure diff is readable.
fn visible(text: &str) -> String {
    text.replace('\u{1b}', "\\e")
}

#[test]
fn colored_type_errors_match_elm() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/type_errors");
    let ansi_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/type_errors_ansi");
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
        let expected = fs::read_to_string(ansi_dir.join(format!("{name}.txt"))).unwrap();
        let got = render_ansi(&source);
        let known = KNOWN_DIFFERENT.contains(&name.as_str());
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
        "these fixtures now match elm — remove them from KNOWN_DIFFERENT: {newly_matching:?}"
    );
    assert!(regressed.is_empty(), "{} fixture(s) differ:{}", regressed.len(), regressed.join(""));
}
