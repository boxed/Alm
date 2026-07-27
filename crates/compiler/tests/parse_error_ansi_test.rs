//! Differential test for the *colored* rendering of syntax errors.
//!
//! `parse_error_test` pins the plain text; this pins the ANSI a terminal gets.
//! Each `parse_errors_ansi/<name>.txt` is `elm make` 0.19.1's exact terminal
//! output for the matching `parse_errors/<name>.elm`, captured under a pty.
//!
//! The shared chrome — dull-cyan header bar, vivid-red carets and `>` markers,
//! underlined `Hint`/`Note` labels — is done. What remains is per-report: elm
//! also colors Elm keywords quoted in prose (vivid cyan), code fragments and
//! type names (dull yellow), suggested syntax (vivid green) and a few
//! constructor names (vivid blue), and which words those are is a decision made
//! separately in each of the ~90 reports in `Reporting.Error.Syntax`. Fixtures
//! still needing that are listed in `NEEDS_INLINE_COLOR`; the test fails both
//! if a listed one starts matching (delete it) and if an unlisted one breaks.
//!
//! `pkg_*` fixtures are excluded: they need a package `elm.json`, which the
//! plain-text suite handles by compiling them differently.

use std::fs;
use std::path::Path;

/// Fixtures whose bodies still need elm's inline word coloring.
const NEEDS_INLINE_COLOR: &[&str] = &[
    "alias_body", "alias_eq", "alias_name", "alias_problem", "case_arrow", "case_of",
    "char_empty", "custom_bar", "custom_problem", "custom_type", "def_body", "def_equals",
    "escape_unknown", "exposing_comma", "exposing_end", "exposing_value",
    "exposing_variants", "if_indent", "if_then", "import_alias", "import_as",
    "import_expecting_name", "import_name", "lambda_arrow", "let_eq", "let_in", "list_end",
    "list_pattern_end", "list_pattern_indent", "list_pattern_open", "module_exposing",
    "module_mismatch", "module_name", "name_mismatch", "need_more_indent", "no_ports",
    "number_dot", "op_function", "operator_bad", "paren_end", "pattern_alias",
    "pattern_float", "pattern_start", "port_annotation", "port_in_normal",
    "port_module_decl", "problem_in_def", "record_accessor", "record_end", "record_eq",
    "record_pattern_end", "record_pattern_field", "record_pattern_indent",
    "record_type_problem", "record_type_unfinished", "string_end", "tuple_pattern_expr",
    "tuple_type_unfinished", "type_ann", "type_name", "unexpected_backtick",
    "unexpected_capital", "unexpected_equals", "unexpected_symbol_decl", "unfinished_case",
    "unfinished_expr", "unfinished_let", "unfinished_tuple", "unicode_bad", "unicode_long",
    "unicode_short", "weird_decl",
];

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
