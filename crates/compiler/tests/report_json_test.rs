//! Differential test for `--report=json`.
//!
//! Editor plugins consume this, so the shape is fixed by elm: a message is an
//! array mixing bare strings (unstyled runs) with
//! `{"bold":…,"underline":…,"color":…,"string":…}` objects, and the title and
//! region are separate fields — the `-- TITLE ---` bar must *not* appear in the
//! message. Each `report_json/<name>.txt` is `elm make --report=json`'s exact
//! stderr for the matching `type_errors/<name>.elm`.
//!
//! The absolute `path` and the module `name` are the CLI's business (see
//! `BuildError::to_json`); this checks the per-problem objects, which is where
//! all the structure lives.

use std::fs;
use std::path::Path;

/// Fixtures whose plain rendering already differs (see `type_error_test`).
const KNOWN_DIFFERENT: &[&str] = &["infinite_type"];

/// The `"problems"` array for a source, as the CLI would emit it.
fn problems_json(source: &str) -> String {
    match alm_compiler::compile_named_typed(source, "Main", false) {
        Ok(_) => panic!("expected a type error, but compilation succeeded"),
        Err(reports) => {
            let bodies: Vec<String> = reports.iter().map(|r| r.to_json(source)).collect();
            format!("[{}]", bodies.join(","))
        }
    }
}

/// Pull the `problems` array out of elm's full envelope, so the comparison does
/// not depend on this machine's absolute paths.
fn elm_problems(envelope: &str) -> String {
    let key = "\"problems\":";
    let start = envelope.find(key).expect("no problems key") + key.len();
    // Scan to the matching bracket, skipping brackets inside strings.
    let bytes: Vec<char> = envelope[start..].chars().collect();
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for (i, c) in bytes.iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if *c == '\\' {
                escaped = true;
            } else if *c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return bytes[..=i].iter().collect();
                }
            }
            _ => {}
        }
    }
    panic!("unterminated problems array");
}

#[test]
fn json_reports_match_elm() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/type_errors");
    let json_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/report_json");
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
        let envelope = fs::read_to_string(json_dir.join(format!("{name}.txt")))
            .unwrap_or_else(|_| panic!("missing JSON fixture for {name}"));
        let expected = elm_problems(&envelope);
        let got = problems_json(&source);
        let known = KNOWN_DIFFERENT.contains(&name.as_str());
        match (got == expected, known) {
            (true, true) => newly_matching.push(name),
            (false, false) => regressed.push(format!(
                "\n=== {name} ===\n--- expected (elm) ---\n{expected}\n--- got (alm) ---\n{got}"
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
