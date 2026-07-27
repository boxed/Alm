//! Differential test for `--report=json` on syntax errors.
//!
//! The counterpart to `report_json_test`, which covers type errors. Syntax
//! errors are the more demanding case for the JSON shape: they carry the
//! widest variety of inline coloring, and their reports point at a position
//! inside a wider span of printed source, so the published region and the
//! snippet disagree.
//!
//! Each `parse_errors_json/<name>.txt` is `elm make --report=json`'s exact
//! stderr for the matching fixture. Only the `problems` array is compared, so
//! the check does not depend on this machine's absolute paths.

use std::fs;
use std::path::Path;

/// `no_tabs` is the one fixture that cannot match, and deliberately so: elm
/// escapes only `\r`, `\n`, `"` and `\\` in JSON strings, so a tab in the
/// source reaches its output raw and makes elm's own report invalid JSON.
/// alm escapes it (see `reporting::json_str`).
const KNOWN_DIFFERENT: &[&str] = &["no_tabs"];

fn problems_json(source: &str) -> String {
    match alm_compiler::compile_named_typed(source, "Main", false) {
        Ok(_) => panic!("expected a parse error, but compilation succeeded"),
        Err(reports) => {
            let bodies: Vec<String> = reports.iter().map(|r| r.to_json(source)).collect();
            format!("[{}]", bodies.join(","))
        }
    }
}

/// Pull the `problems` array out of elm's envelope.
fn elm_problems(envelope: &str) -> String {
    let key = "\"problems\":";
    let start = envelope.find(key).expect("no problems key") + key.len();
    let chars: Vec<char> = envelope[start..].chars().collect();
    let (mut depth, mut in_string, mut escaped) = (0i32, false, false);
    for (i, c) in chars.iter().enumerate() {
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
                    return chars[..=i].iter().collect();
                }
            }
            _ => {}
        }
    }
    panic!("unterminated problems array");
}

#[test]
fn json_syntax_reports_match_elm() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/parse_errors");
    let json_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/parse_errors_json");
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
