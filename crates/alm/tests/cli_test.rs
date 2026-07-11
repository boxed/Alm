//! CLI integration tests: drive the `alm` binary end to end.

use std::path::{Path, PathBuf};
use std::process::Command;

/// A per-test scratch directory that removes itself when the test finishes
/// (kept on panic, or with ALM_KEEP_TEST_DIRS=1, for inspection). TMPDIR is
/// redirected into the in-project `.almtmp/` by `.cargo/config.toml`, and the
/// per-PID names meant every run leaked a new set of directories.
struct TestDir {
    path: PathBuf,
}

impl std::ops::Deref for TestDir {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        if std::thread::panicking() || std::env::var_os("ALM_KEEP_TEST_DIRS").is_some() {
            return;
        }
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn alm(args: &[&str]) -> (bool, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_alm"))
        .args(args)
        .output()
        .expect("failed to run alm");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn temp_dir() -> TestDir {
    let path = std::env::temp_dir().join(format!(
        "alm-cli-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&path).unwrap();
    TestDir { path }
}

#[test]
fn help_flag_and_no_args() {
    let (ok, stdout, _) = alm(&["--help"]);
    assert!(ok);
    assert!(stdout.contains("alm make"));
    let (ok, stdout, _) = alm(&[]);
    assert!(ok);
    assert!(stdout.contains("Usage"));
}

#[test]
fn unknown_command() {
    let (ok, _, stderr) = alm(&["build"]);
    assert!(!ok);
    assert!(stderr.contains("Unknown command `build`"));
}

#[test]
fn make_requires_a_file() {
    let (ok, _, stderr) = alm(&["make"]);
    assert!(!ok);
    assert!(stderr.contains("Which .elm file"));
}

#[test]
fn make_rejects_unknown_flags_and_extra_files() {
    let (ok, _, stderr) = alm(&["make", "--wat"]);
    assert!(!ok);
    assert!(stderr.contains("Unknown flag `--wat`"));
    let (ok, _, stderr) = alm(&["make", "a.elm", "b.elm"]);
    assert!(!ok);
    assert!(stderr.contains("exactly one"));
}

#[test]
fn make_reports_missing_files() {
    let (ok, _, stderr) = alm(&["make", "/nonexistent/Nope.elm"]);
    assert!(!ok);
    assert!(stderr.contains("could not read") || stderr.contains("FILE PROBLEM"));
}

#[test]
fn make_compiles_to_default_and_explicit_output() {
    let dir = temp_dir();
    let elm = dir.join("Main.elm");
    std::fs::write(&elm, "module Main exposing (main)\n\nmain = \"hi\"\n").unwrap();

    let (ok, stdout, _) = alm(&["make", elm.to_str().unwrap()]);
    assert!(ok, "compile failed: {}", stdout);
    assert!(stdout.contains("Success"));
    assert!(dir.join("Main.js").is_file());

    let out = dir.join("custom.js");
    let (ok, _, _) = alm(&[
        "make",
        elm.to_str().unwrap(),
        &format!("--output={}", out.display()),
    ]);
    assert!(ok);
    assert!(out.is_file());
}

#[test]
fn make_prints_compile_errors_and_counts() {
    let dir = temp_dir();
    let elm = dir.join("Bad.elm");
    std::fs::write(
        &elm,
        "module Bad exposing (..)\n\nx : String\nx = 1\n\ny = alsoMissing\n",
    )
    .unwrap();
    let (ok, _, stderr) = alm(&["make", elm.to_str().unwrap()]);
    assert!(!ok);
    assert!(stderr.contains("problem"), "got: {}", stderr);
}

#[test]
fn output_write_failure_is_reported() {
    let dir = temp_dir();
    let elm = dir.join("Main2.elm");
    std::fs::write(&elm, "module Main2 exposing (main)\n\nmain = \"x\"\n").unwrap();
    let (ok, _, stderr) = alm(&[
        "make",
        elm.to_str().unwrap(),
        "--output=/nonexistent-dir/out.js",
    ]);
    assert!(!ok);
    assert!(stderr.contains("could not write"));
}
