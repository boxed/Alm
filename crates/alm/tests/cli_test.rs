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

/// Run `alm diff` in a directory built to look like a package cache, so the
/// test does not depend on what happens to be in the developer's ~/.elm.
fn alm_diff(dir: &Path, home: &Path, args: &[&str]) -> (bool, String, String) {
    let mut all = vec!["diff"];
    all.extend(args);
    let output = Command::new(env!("CARGO_BIN_EXE_alm"))
        .args(&all)
        .current_dir(dir)
        .env("ELM_HOME", home)
        .env("NO_COLOR", "1")
        .output()
        .expect("failed to run alm diff");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Put a package version into a fake cache: an outline, a `src` and the
/// `docs.json` a diff reads.
fn cache_package(home: &Path, name: &str, version: &str, docs: &str) -> PathBuf {
    let dir = home.join("0.19.1/packages").join(name).join(version);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("elm.json"),
        format!(
            r#"{{ "type": "package", "name": "{name}", "version": "{version}",
                  "exposed-modules": ["Thing"], "dependencies": {{}} }}"#
        ),
    )
    .unwrap();
    std::fs::write(dir.join("docs.json"), docs).unwrap();
    dir
}

const V1: &str = r#"[{"name":"Thing","comment":" Doc.\n\n@docs one\n","unions":[],
    "aliases":[],"binops":[],
    "values":[{"name":"one","comment":" one ","type":"Basics.Int"}]}]"#;

const V2: &str = r#"[{"name":"Thing","comment":" Doc.\n\n@docs one, two\n","unions":[],
    "aliases":[],"binops":[],
    "values":[{"name":"one","comment":" one ","type":"Basics.Int"},
              {"name":"two","comment":" two ","type":"String.String"}]}]"#;

#[test]
fn diff_compares_two_published_versions() {
    let dir = temp_dir();
    let home = dir.join("elm-home");
    cache_package(&home, "acme/thing", "1.0.0", V1);
    cache_package(&home, "acme/thing", "1.1.0", V2);

    let (ok, stdout, stderr) = alm_diff(&dir, &home, &["acme/thing", "1.0.0", "1.1.0"]);
    assert!(ok, "stderr: {stderr}");
    assert_eq!(
        stdout,
        "This is a MINOR change.\n\n\
         ---- Thing - MINOR ----\n\n\
         \x20   Added:\n        two : String\n\n\n"
    );

    // Comparing a version with itself is a PATCH, and the versions are sorted
    // rather than taken in the order given.
    let (ok, same, _) = alm_diff(&dir, &home, &["acme/thing", "1.0.0", "1.0.0"]);
    assert!(ok);
    assert_eq!(same, "No API changes detected, so this is a PATCH change.\n");
    let (_, backwards, _) = alm_diff(&dir, &home, &["acme/thing", "1.1.0", "1.0.0"]);
    assert_eq!(backwards, stdout);
}

#[test]
fn diff_compares_local_code_against_a_release() {
    let dir = temp_dir();
    let home = dir.join("elm-home");
    cache_package(&home, "acme/thing", "1.0.0", V1);

    // A working copy of the package, one value ahead of the release.
    let project = dir.join("work");
    std::fs::create_dir_all(project.join("src")).unwrap();
    std::fs::write(
        project.join("elm.json"),
        r#"{ "type": "package", "name": "acme/thing", "version": "1.1.0",
             "exposed-modules": ["Thing"], "dependencies": {} }"#,
    )
    .unwrap();
    std::fs::write(
        project.join("src/Thing.elm"),
        "module Thing exposing (one, two)\n\n\
         {-| Doc.\n\n@docs one, two\n-}\n\n\
         {-| one -}\none : Int\none = 1\n\n\
         {-| two -}\ntwo : String\ntwo = \"two\"\n",
    )
    .unwrap();

    // With no version it compares against the newest release in the cache.
    let (ok, stdout, stderr) = alm_diff(&project, &home, &[]);
    assert!(ok, "stderr: {stderr}");
    // Docs generated from source keep their qualifiers, as elm's do.
    assert!(stdout.contains("two : String.String"), "stdout: {stdout}");
    assert!(stdout.starts_with("This is a MINOR change."), "stdout: {stdout}");

    let (ok, explicit, _) = alm_diff(&project, &home, &["1.0.0"]);
    assert!(ok);
    assert_eq!(explicit, stdout);
}

#[test]
fn diff_says_what_it_cannot_find() {
    let dir = temp_dir();
    let home = dir.join("elm-home");
    cache_package(&home, "acme/thing", "1.0.0", V1);

    let (ok, _, stderr) = alm_diff(&dir, &home, &["acme/thing", "1.0.0", "9.9.9"]);
    assert!(!ok);
    assert!(stderr.starts_with("-- UNKNOWN VERSION ---"), "stderr: {stderr}");

    let (ok, _, stderr) = alm_diff(&dir, &home, &["who/what", "1.0.0", "2.0.0"]);
    assert!(!ok);
    assert!(stderr.starts_with("-- UNKNOWN PACKAGE ---"), "stderr: {stderr}");

    let (ok, _, stderr) = alm_diff(&dir, &home, &["acme/thing", "1.0.0", "one"]);
    assert!(!ok);
    assert!(stderr.starts_with("-- BAD ARGUMENT ---"), "stderr: {stderr}");

    // No elm.json here, so there is nothing local to compare.
    let (ok, _, stderr) = alm_diff(&dir, &home, &[]);
    assert!(!ok);
    assert!(stderr.starts_with("-- DIFF WHAT? ---"), "stderr: {stderr}");
}

#[test]
fn diff_refuses_an_application() {
    let dir = temp_dir();
    let home = dir.join("elm-home");
    std::fs::write(
        dir.join("elm.json"),
        r#"{ "type": "application", "source-directories": ["src"],
             "dependencies": { "direct": {}, "indirect": {} } }"#,
    )
    .unwrap();
    let (ok, _, stderr) = alm_diff(&dir, &home, &[]);
    assert!(!ok);
    assert!(stderr.starts_with("-- CANNOT DIFF APPLICATIONS ---"), "stderr: {stderr}");
}

fn alm_bump(dir: &Path, home: &Path, answer: &str) -> (bool, String, String) {
    use std::io::Write;
    let mut child = Command::new(env!("CARGO_BIN_EXE_alm"))
        .arg("bump")
        .current_dir(dir)
        .env("ELM_HOME", home)
        .env("NO_COLOR", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to run alm bump");
    child.stdin.take().unwrap().write_all(answer.as_bytes()).unwrap();
    let output = child.wait_with_output().expect("wait");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// A working copy of `acme/thing` whose API is `one` plus whatever `extra`
/// adds, at version `version`.
fn work_dir(dir: &Path, version: &str, extra: &str) -> PathBuf {
    let project = dir.join("work");
    std::fs::create_dir_all(project.join("src")).unwrap();
    std::fs::write(
        project.join("elm.json"),
        format!(
            r#"{{ "type": "package", "name": "acme/thing", "version": "{version}",
                  "exposed-modules": ["Thing"], "dependencies": {{}} }}"#
        ),
    )
    .unwrap();
    let exposing = if extra.is_empty() { "one" } else { "one, two" };
    let docs = if extra.is_empty() { "@docs one" } else { "@docs one, two" };
    std::fs::write(
        project.join("src/Thing.elm"),
        format!(
            "module Thing exposing ({exposing})\n\n\
             {{-| Doc.\n\n{docs}\n-}}\n\n\
             {{-| one -}}\none : Int\none = 1\n{extra}"
        ),
    )
    .unwrap();
    project
}

#[test]
fn bump_raises_the_version_the_api_change_calls_for() {
    let dir = temp_dir();
    let home = dir.join("elm-home");
    cache_package(&home, "acme/thing", "1.0.0", V1);
    let project = work_dir(&dir, "1.0.0", "\n{-| two -}\ntwo : String\ntwo = \"two\"\n");

    // Declining leaves elm.json alone.
    let (ok, stdout, stderr) = alm_bump(&project, &home, "n\n");
    assert!(ok, "stderr: {stderr}");
    assert!(
        stdout.starts_with(
            "Based on your new API, this should be a MINOR change (1.0.0 => 1.1.0)\n"
        ),
        "stdout: {stdout}"
    );
    assert!(stdout.ends_with("Okay, I did not change anything!\n"), "stdout: {stdout}");
    let outline = std::fs::read_to_string(project.join("elm.json")).unwrap();
    assert!(outline.contains("\"version\": \"1.0.0\""));

    let (ok, stdout, _) = alm_bump(&project, &home, "Y\n");
    assert!(ok);
    assert!(stdout.ends_with("Version changed to 1.1.0!\n"), "stdout: {stdout}");
    let outline = std::fs::read_to_string(project.join("elm.json")).unwrap();
    assert!(outline.contains("\"version\": \"1.1.0\""), "outline: {outline}");
    // Nothing else about the file moved.
    assert!(outline.contains("\"exposed-modules\": [\"Thing\"]"), "outline: {outline}");
}

#[test]
fn bump_refuses_a_version_nothing_could_depend_on() {
    let dir = temp_dir();
    let home = dir.join("elm-home");
    cache_package(&home, "acme/thing", "1.0.0", V1);
    let project = work_dir(&dir, "3.4.5", "");

    let (ok, _, stderr) = alm_bump(&project, &home, "Y\n");
    assert!(!ok);
    assert!(stderr.starts_with("-- CANNOT BUMP ---"), "stderr: {stderr}");
    assert!(stderr.contains("relative to version 3.4.5"), "stderr: {stderr}");
    assert!(stderr.trim_end().ends_with("1.0.0"), "stderr: {stderr}");
}

#[test]
fn bump_explains_versioning_for_an_unpublished_package() {
    let dir = temp_dir();
    let home = dir.join("elm-home");
    std::fs::create_dir_all(home.join("0.19.1/packages")).unwrap();
    let project = work_dir(&dir, "1.0.0", "");

    let (ok, stdout, stderr) = alm_bump(&project, &home, "");
    assert!(ok, "stderr: {stderr}");
    assert!(stdout.starts_with("This package has never been published before."));
    assert!(stdout.ends_with(
        "The version number in elm.json is correct so you are all set!\n"
    ));

    // A version other than 1.0.0 is offered a correction.
    let project = work_dir(&dir, "2.0.0", "");
    let (ok, stdout, _) = alm_bump(&project, &home, "Y\n");
    assert!(ok);
    assert!(stdout.contains("change it back to 1.0.0? [Y/n] "), "stdout: {stdout}");
    let outline = std::fs::read_to_string(project.join("elm.json")).unwrap();
    assert!(outline.contains("\"version\": \"1.0.0\""), "outline: {outline}");
}

#[test]
fn bump_needs_a_package() {
    let dir = temp_dir();
    let home = dir.join("elm-home");
    let (ok, _, stderr) = alm_bump(&dir, &home, "");
    assert!(!ok);
    assert!(stderr.starts_with("-- BUMP WHAT? ---"), "stderr: {stderr}");

    std::fs::write(
        dir.join("elm.json"),
        r#"{ "type": "application", "source-directories": ["src"],
             "dependencies": { "direct": {}, "indirect": {} } }"#,
    )
    .unwrap();
    let (ok, _, stderr) = alm_bump(&dir, &home, "");
    assert!(!ok);
    assert!(stderr.starts_with("-- CANNOT BUMP APPLICATIONS ---"), "stderr: {stderr}");
}

/// Boot `alm reactor` on a free port and return the child plus its base URL.
/// The port comes from binding one and letting it go: a race is possible in
/// principle, but the alternative is a fixed port that collides with whatever
/// else is on the machine.
fn start_reactor(dir: &Path) -> (std::process::Child, u16) {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let child = Command::new(env!("CARGO_BIN_EXE_alm"))
        .args(["reactor", &format!("--port={port}")])
        .current_dir(dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to start alm reactor");
    // Wait for it to accept connections rather than sleeping a fixed time.
    for _ in 0..200 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return (child, port);
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    panic!("reactor never came up on port {port}");
}

/// A bare GET, returning (status line, headers, body).
fn get(port: u16, path: &str) -> (String, String, String) {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
        .unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let (status, headers) = head.split_once("\r\n").unwrap_or((head, ""));
    (status.to_string(), headers.to_string(), body.to_string())
}

#[test]
fn reactor_serves_compiles_and_refuses_to_escape() {
    let dir = temp_dir();
    std::fs::create_dir_all(dir.join("work/src")).unwrap();
    let project = dir.join("work");
    std::fs::write(
        project.join("elm.json"),
        r#"{ "type": "application", "source-directories": ["src"],
             "dependencies": { "direct": {}, "indirect": {} } }"#,
    )
    .unwrap();
    std::fs::write(
        project.join("src/Main.elm"),
        "module Main exposing (main)\n\nmain = \"hi\"\n",
    )
    .unwrap();
    std::fs::write(project.join("src/Broken.elm"), "module Broken exposing (x)\n\nx = 1 + \"a\"\n")
        .unwrap();
    std::fs::write(project.join("notes.md"), "# Notes\n").unwrap();
    std::fs::write(dir.join("secret.txt"), "not yours").unwrap();

    let (mut child, port) = start_reactor(&project);

    // The dashboard lists what is there.
    let (status, _, body) = get(port, "/");
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");
    assert!(body.contains("<a href=\"/src/\">src/</a>"), "{body}");

    // An Elm module compiles and comes back in elm's page.
    let (status, _, body) = get(port, "/src/Main.elm");
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");
    assert!(body.starts_with("<!DOCTYPE HTML>"), "{body}");
    assert!(
        body.contains("var app = Elm.Main.init({ node: document.getElementById(\"elm\") });"),
        "no init call in the page"
    );

    // A failing module reports instead of serving a broken page.
    let (status, _, body) = get(port, "/src/Broken.elm");
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");
    assert!(body.contains("TYPE MISMATCH"), "{body}");

    // A file with no mime type is shown as source, one with a type is served.
    let (_, headers, body) = get(port, "/notes.md");
    assert!(headers.contains("text/html"), "{headers}");
    assert!(body.contains("# Notes"), "{body}");
    let (_, headers, body) = get(port, "/elm.json");
    assert!(headers.contains("application/json"), "{headers}");
    assert!(body.contains("\"source-directories\""), "{body}");

    let (status, _, _) = get(port, "/nothing-here");
    assert!(status.starts_with("HTTP/1.1 404"), "{status}");

    // Nothing above the served directory is reachable, however it is spelled.
    for escape in ["/../secret.txt", "/%2e%2e/secret.txt", "/src/../../secret.txt"] {
        let (status, _, body) = get(port, escape);
        assert!(status.starts_with("HTTP/1.1 404"), "{escape}: {status}");
        assert!(!body.contains("not yours"), "{escape} escaped the root");
    }

    let _ = child.kill();
    let _ = child.wait();
}

fn alm_publish(dir: &Path, home: &Path) -> (bool, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_alm"))
        .arg("publish")
        .current_dir(dir)
        .env("ELM_HOME", home)
        .env("NO_COLOR", "1")
        .output()
        .expect("failed to run alm publish");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("git")
        .status
        .success();
    assert!(ok, "git {args:?} failed");
}

/// A package that passes every check: summary, README, LICENSE, buildable
/// docs, a correctly bumped version, and a matching tag.
fn publishable(dir: &Path, home: &Path) -> PathBuf {
    cache_package(home, "acme/thing", "1.0.0", V1);
    let project = dir.join("pub");
    std::fs::create_dir_all(project.join("src")).unwrap();
    std::fs::write(
        project.join("elm.json"),
        r#"{ "type": "package", "name": "acme/thing", "summary": "Does a thing well",
             "license": "BSD-3-Clause", "version": "1.1.0",
             "exposed-modules": ["Thing"], "elm-version": "0.19.0 <= v < 0.20.0",
             "dependencies": {}, "test-dependencies": {} }"#,
    )
    .unwrap();
    std::fs::write(
        project.join("src/Thing.elm"),
        "module Thing exposing (one, two)\n\n\
         {-| Doc.\n\n@docs one, two\n-}\n\n\
         {-| one -}\none : Int\none = 1\n\n\
         {-| two -}\ntwo : String\ntwo = \"two\"\n",
    )
    .unwrap();
    std::fs::write(project.join("LICENSE"), "BSD-3-Clause\n").unwrap();
    std::fs::write(project.join("README.md"), "# Thing\n\n".to_string() + &"x".repeat(400))
        .unwrap();
    git(&project, &["init", "-q", "."]);
    git(&project, &["add", "-A"]);
    git(&project, &["commit", "-qm", "release"]);
    git(&project, &["tag", "1.1.0"]);
    project
}

#[test]
fn publish_runs_every_local_check_then_stops() {
    let dir = temp_dir();
    let home = dir.join("elm-home");
    let project = publishable(&dir, &home);

    let (ok, stdout, stderr) = alm_publish(&project, &home);
    assert!(ok, "stderr: {stderr}");
    assert!(stdout.starts_with("Verifying acme/thing 1.1.0 ...\n"), "stdout: {stdout}");
    for step in [
        "Found README.md",
        "Found LICENSE",
        "Verified documentation",
        "Version number 1.1.0 verified (MINOR change, 1.0.0 => 1.1.0)",
        "Version 1.1.0 is tagged",
        "No uncommitted changes in local code",
    ] {
        assert!(stdout.contains(step), "missing step {step:?} in:\n{stdout}");
    }
    // It stops short of registering, and says so rather than claiming success.
    assert!(stdout.contains("-- READY, BUT NOT PUBLISHED --"), "stdout: {stdout}");
    assert!(!stdout.contains("Success!"), "stdout: {stdout}");
}

#[test]
fn publish_catches_the_things_it_is_there_to_catch() {
    let dir = temp_dir();
    let home = dir.join("elm-home");
    let project = publishable(&dir, &home);
    let outline = std::fs::read_to_string(project.join("elm.json")).unwrap();
    let restore = || std::fs::write(project.join("elm.json"), &outline).unwrap();

    // A version that has already gone out.
    std::fs::write(project.join("elm.json"), outline.replace("1.1.0", "1.0.0")).unwrap();
    let (ok, _, stderr) = alm_publish(&project, &home);
    assert!(!ok);
    assert!(stderr.contains("-- ALREADY PUBLISHED --"), "stderr: {stderr}");

    // A version number that does not match the change.
    std::fs::write(project.join("elm.json"), outline.replace("\"1.1.0\"", "\"2.0.0\"")).unwrap();
    let (ok, _, stderr) = alm_publish(&project, &home);
    assert!(!ok);
    assert!(stderr.contains("-- INVALID VERSION --"), "stderr: {stderr}");
    assert!(stderr.contains("it should be 1.1.0"), "stderr: {stderr}");
    restore();

    // The placeholder summary elm init leaves behind.
    std::fs::write(
        project.join("elm.json"),
        outline.replace(
            "Does a thing well",
            "helpful summary of your project, less than 80 characters",
        ),
    )
    .unwrap();
    let (ok, _, stderr) = alm_publish(&project, &home);
    assert!(!ok);
    assert!(stderr.contains("-- NO SUMMARY --"), "stderr: {stderr}");
    restore();

    // A README too short to say anything.
    std::fs::write(project.join("README.md"), "# Thing\n").unwrap();
    let (ok, _, stderr) = alm_publish(&project, &home);
    assert!(!ok);
    assert!(stderr.contains("-- SHORT README --"), "stderr: {stderr}");
    std::fs::write(project.join("README.md"), "# Thing\n\n".to_string() + &"x".repeat(400))
        .unwrap();

    // No LICENSE.
    std::fs::remove_file(project.join("LICENSE")).unwrap();
    let (ok, _, stderr) = alm_publish(&project, &home);
    assert!(!ok);
    assert!(stderr.contains("-- NO LICENSE FILE --"), "stderr: {stderr}");
    std::fs::write(project.join("LICENSE"), "BSD-3-Clause\n").unwrap();

    // Code that differs from what was tagged.
    std::fs::write(
        project.join("src/Thing.elm"),
        "module Thing exposing (one, two)\n\n\
         {-| Doc.\n\n@docs one, two\n-}\n\n\
         {-| one -}\none : Int\none = 2\n\n\
         {-| two -}\ntwo : String\ntwo = \"two\"\n",
    )
    .unwrap();
    let (ok, _, stderr) = alm_publish(&project, &home);
    assert!(!ok);
    assert!(stderr.contains("-- LOCAL CHANGES --"), "stderr: {stderr}");
}

#[test]
fn publish_needs_a_tag() {
    let dir = temp_dir();
    let home = dir.join("elm-home");
    let project = publishable(&dir, &home);
    git(&project, &["tag", "-d", "1.1.0"]);

    let (ok, _, stderr) = alm_publish(&project, &home);
    assert!(!ok);
    assert!(stderr.contains("-- NO TAG --"), "stderr: {stderr}");
    assert!(stderr.contains("git tag -a 1.1.0"), "stderr: {stderr}");
}

#[test]
fn publish_refuses_an_application() {
    let dir = temp_dir();
    let home = dir.join("elm-home");
    std::fs::write(
        dir.join("elm.json"),
        r#"{ "type": "application", "source-directories": ["src"],
             "dependencies": { "direct": {}, "indirect": {} } }"#,
    )
    .unwrap();
    let (ok, _, stderr) = alm_publish(&dir, &home);
    assert!(!ok);
    assert!(stderr.starts_with("-- CANNOT PUBLISH APPLICATIONS ---"), "stderr: {stderr}");
}
