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
    let (ok, stdout, stderr) = alm(&["make", elm.to_str().unwrap()]);
    assert!(!ok);
    // The reports go to stderr and the tally to stdout, as elm splits them.
    assert!(stderr.contains("NAMING PROBLEM"), "stderr: {stderr}");
    assert!(
        stdout.contains("Detected problems in 1 module."),
        "the tally belongs on stdout, got: {stdout}"
    );
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

/// `alm init` in `dir`, answering the prompt with `answer`.
fn alm_init(dir: &Path, answer: &str) -> (bool, String, String) {
    use std::io::Write;
    let mut child = Command::new(env!("CARGO_BIN_EXE_alm"))
        .arg("init")
        .current_dir(dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to run alm init");
    child.stdin.take().unwrap().write_all(answer.as_bytes()).unwrap();
    let output = child.wait_with_output().expect("wait");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn init_creates_a_project() {
    let dir = temp_dir();
    let (ok, stdout, stderr) = alm_init(&dir, "Y\n");
    assert!(ok, "init failed: {stderr}");
    assert!(stdout.contains("Okay, I created it."), "stdout: {stdout}");
    assert!(dir.join("src").is_dir(), "src/ should exist");

    let outline = std::fs::read_to_string(dir.join("elm.json")).expect("elm.json");
    // The three packages elm starts a project with, and the shape it writes.
    for package in ["elm/browser", "elm/core", "elm/html"] {
        assert!(outline.contains(package), "{package} missing from:\n{outline}");
    }
    assert!(outline.contains("\"type\": \"application\""), "outline:\n{outline}");
    assert!(outline.contains("\"source-directories\""), "outline:\n{outline}");
    assert!(outline.ends_with("}\n"), "should end with a newline");
    // The project it produces must actually build.
    std::fs::write(
        dir.join("src/Main.elm"),
        "module Main exposing (main)\n\nimport Html\n\n\nmain : Html.Html msg\nmain =\n    Html.text \"hi\"\n",
    )
    .unwrap();
    let built = Command::new(env!("CARGO_BIN_EXE_alm"))
        .args(["make", "src/Main.elm", "--output=/dev/null"])
        .current_dir(&*dir)
        .output()
        .expect("run alm make");
    assert!(
        built.status.success(),
        "the initialized project should compile:\n{}",
        String::from_utf8_lossy(&built.stderr)
    );
}

/// Answering "n" leaves the directory untouched. Anything else, including a
/// bare Enter, is a yes.
#[test]
fn init_respects_the_answer() {
    let dir = temp_dir();
    let (ok, stdout, _) = alm_init(&dir, "n\n");
    assert!(ok, "declining is not an error");
    assert!(stdout.contains("Okay, I did not make any changes!"), "stdout: {stdout}");
    assert!(!dir.join("elm.json").exists(), "nothing should have been written");

    let dir = temp_dir();
    let (ok, _, _) = alm_init(&dir, "\n");
    assert!(ok);
    assert!(dir.join("elm.json").is_file(), "a bare Enter means yes");
}

/// A second `init` refuses, on stderr, without touching the existing file.
#[test]
fn init_refuses_to_overwrite() {
    let dir = temp_dir();
    assert!(alm_init(&dir, "Y\n").0);
    let before = std::fs::read_to_string(dir.join("elm.json")).unwrap();

    let (ok, _, stderr) = alm_init(&dir, "Y\n");
    assert!(!ok, "should exit nonzero");
    assert!(stderr.starts_with("-- EXISTING PROJECT ---"), "stderr: {stderr}");
    assert_eq!(
        std::fs::read_to_string(dir.join("elm.json")).unwrap(),
        before,
        "the existing elm.json must be left alone"
    );
}

/// `alm install <package>` in `dir`, answering the prompt with `answer`.
fn alm_install(dir: &Path, package: &str, answer: &str) -> (bool, String, String) {
    use std::io::Write;
    let mut child = Command::new(env!("CARGO_BIN_EXE_alm"))
        .args(["install", package])
        .current_dir(dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to run alm install");
    child.stdin.take().unwrap().write_all(answer.as_bytes()).unwrap();
    let output = child.wait_with_output().expect("wait");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn dependencies(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("elm.json")).expect("elm.json")
}

#[test]
fn install_adds_a_package_and_what_it_needs() {
    let dir = temp_dir();
    assert!(alm_init(&dir, "Y\n").0);

    let (ok, stdout, stderr) = alm_install(&dir, "elm/http", "Y\n");
    assert!(ok, "install failed: {stderr}");
    assert!(stdout.starts_with("Here is my plan:"), "stdout: {stdout}");
    assert!(stdout.ends_with("Success!\n"), "stdout: {stdout}");

    let outline = dependencies(&dir);
    assert!(outline.contains("\"elm/http\""), "http should be a dependency:\n{outline}");
    // Its own requirements come along as indirect dependencies.
    for needed in ["elm/bytes", "elm/file"] {
        assert!(outline.contains(needed), "{needed} should have come along:\n{outline}");
    }
    // The project still builds afterwards.
    std::fs::write(
        dir.join("src/Main.elm"),
        "module Main exposing (main)\n\nimport Html\nimport Http\n\n\n\
         main : Html.Html msg\nmain =\n    Html.text (Debug.toString Http.emptyBody)\n",
    )
    .unwrap();
    let built = Command::new(env!("CARGO_BIN_EXE_alm"))
        .args(["make", "src/Main.elm", "--output=/dev/null"])
        .current_dir(&*dir)
        .output()
        .expect("run alm make");
    assert!(
        built.status.success(),
        "the project should still compile:\n{}",
        String::from_utf8_lossy(&built.stderr)
    );
}

/// A package already in `direct` needs no work; one in `indirect` is offered a
/// promotion instead of a re-solve.
#[test]
fn install_recognizes_what_is_already_there() {
    let dir = temp_dir();
    assert!(alm_init(&dir, "Y\n").0);

    let (ok, stdout, _) = alm_install(&dir, "elm/core", "");
    assert!(ok);
    assert_eq!(stdout, "It is already installed!\n");

    // elm/json arrives as an indirect dependency of the defaults.
    let before = dependencies(&dir);
    assert!(before.contains("elm/json"));
    let (ok, stdout, _) = alm_install(&dir, "elm/json", "Y\n");
    assert!(ok);
    assert!(
        stdout.starts_with("I found it in your elm.json file, but in the \"indirect\""),
        "stdout: {stdout}"
    );
    let after = dependencies(&dir);
    let direct_block = after.split("\"indirect\"").next().unwrap();
    assert!(direct_block.contains("elm/json"), "should have moved to direct:\n{after}");
}

#[test]
fn install_leaves_things_alone_when_declined() {
    let dir = temp_dir();
    assert!(alm_init(&dir, "Y\n").0);
    let before = dependencies(&dir);

    let (ok, stdout, _) = alm_install(&dir, "elm/http", "n\n");
    assert!(ok, "declining is not an error");
    assert!(stdout.contains("Okay, I did not change anything!"), "stdout: {stdout}");
    assert_eq!(dependencies(&dir), before, "elm.json must be untouched");
}

#[test]
fn install_needs_a_project_and_a_real_name() {
    let dir = temp_dir();
    let (ok, _, stderr) = alm_install(&dir, "elm/http", "Y\n");
    assert!(!ok);
    assert!(stderr.starts_with("-- NO elm.json FILE ---"), "stderr: {stderr}");

    assert!(alm_init(&dir, "Y\n").0);
    let (ok, _, stderr) = alm_install(&dir, "nonsense", "Y\n");
    assert!(!ok);
    assert!(stderr.starts_with("-- BAD PACKAGE NAME ---"), "stderr: {stderr}");
}
