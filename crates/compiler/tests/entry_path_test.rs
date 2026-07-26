//! Entry paths must resolve the same however they are spelled.
//!
//! This test changes the process working directory, which is global state, so
//! it lives alone in its own test binary rather than racing sibling tests.

use std::path::Path;

mod common;

const SOURCE: &str = "module Main exposing (main)\n\nmain : String\nmain =\n    \"hi\"\n";

/// `alm make Main.elm` from inside the file's directory used to fail: the
/// parent of a bare file name is `""`, not `None`, so the walk looking for
/// elm.json never ascended and never terminated normally — it settled on an
/// empty source directory, and since every absolute path "starts with" the
/// empty prefix, the expected module name came out as the entry's whole
/// absolute path with the separators turned into dots
/// (`.Users.you.code.Main`). The three spellings must agree.
#[test]
fn bare_relative_and_absolute_entry_paths_agree() {
    let dir = common::test_dir("alm-entry-path", "bare");
    std::fs::write(dir.join("Main.elm"), SOURCE).unwrap();
    let absolute = std::fs::canonicalize(dir.join("Main.elm")).unwrap();
    std::env::set_current_dir(&dir).unwrap();

    for spelling in [Path::new("Main.elm"), Path::new("./Main.elm"), absolute.as_path()] {
        let checked = alm_compiler::project::check_project(spelling).unwrap_or_else(|errors| {
            panic!(
                "`alm make {}` failed:\n{}",
                spelling.display(),
                errors.iter().map(|e| e.render()).collect::<Vec<_>>().join("\n")
            )
        });
        assert_eq!(checked.entry.as_str(), "Main", "entry name for `{}`", spelling.display());
    }
}
