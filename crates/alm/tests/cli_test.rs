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
        body.contains("var app = Elm.Main.init({ node: node });"),
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

/// Feed a REPL session in on stdin and return what came back on stdout.
fn alm_repl(dir: &Path, session: &str) -> (bool, String, String) {
    use std::io::Write;
    let mut child = Command::new(env!("CARGO_BIN_EXE_alm"))
        .args(["repl", "--no-colors"])
        .current_dir(dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to run alm repl");
    child.stdin.take().unwrap().write_all(session.as_bytes()).unwrap();
    let output = child.wait_with_output().expect("wait");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// A project for the REPL to run in, so the test never touches the real
/// `~/.elm`. No dependencies: everything exercised here is builtin.
fn repl_project(dir: &Path) -> PathBuf {
    let project = dir.join("repl");
    std::fs::create_dir_all(project.join("src")).unwrap();
    std::fs::write(
        project.join("elm.json"),
        r#"{ "type": "application", "source-directories": ["src"],
             "dependencies": { "direct": {}, "indirect": {} } }"#,
    )
    .unwrap();
    project
}

#[test]
fn repl_evaluates_expressions_with_their_types() {
    let dir = temp_dir();
    let project = repl_project(&dir);
    let (ok, stdout, stderr) = alm_repl(
        &project,
        "1 + 1\n\"hello\"\nList.map (\\x -> x * 2) [1,2,3]\n[Just 1, Nothing]\n:exit\n",
    );
    assert!(ok, "stderr: {stderr}");
    // The prompts and the answers share a line, as they do in a terminal.
    assert!(stdout.contains("> 2 : number"), "stdout: {stdout}");
    assert!(stdout.contains("> \"hello\" : String"), "stdout: {stdout}");
    assert!(stdout.contains("> [2,4,6] : List number"), "stdout: {stdout}");
    assert!(stdout.contains("> [Just 1,Nothing] : List (Maybe number)"), "stdout: {stdout}");
}

#[test]
fn repl_remembers_definitions_imports_and_types() {
    let dir = temp_dir();
    let project = repl_project(&dir);
    let (ok, stdout, stderr) = alm_repl(
        &project,
        "x = 40\nx + 2\ndouble n = n * 2\ndouble 21\n\
         type Color = Red | Blue\nRed\n\
         import Dict\nDict.fromList [(1,\"a\")]\n:exit\n",
    );
    assert!(ok, "stderr: {stderr}");
    assert!(stdout.contains("42 : number"), "stdout: {stdout}");
    assert!(stdout.contains("Red : Color"), "stdout: {stdout}");
    // A type from a module imported without `exposing` keeps its prefix, as
    // elm's localizer does.
    assert!(
        stdout.contains("Dict.fromList [(1,\"a\")] : Dict.Dict number String"),
        "stdout: {stdout}"
    );
}

#[test]
fn repl_keeps_going_after_a_bad_entry() {
    let dir = temp_dir();
    let project = repl_project(&dir);
    let (ok, stdout, stderr) = alm_repl(&project, "1 + \"a\"\nnope\n2 + 2\n:exit\n");
    assert!(ok, "stderr: {stderr}");
    assert!(stderr.contains("TYPE MISMATCH"), "stderr: {stderr}");
    assert!(stderr.contains("NAMING"), "stderr: {stderr}");
    // The session survives both.
    assert!(stdout.contains("4 : number"), "stdout: {stdout}");
}

/// A definition that failed must not stay in the session, or every later
/// entry would fail too.
#[test]
fn a_failed_definition_is_not_kept() {
    let dir = temp_dir();
    let project = repl_project(&dir);
    let (ok, stdout, _) = alm_repl(&project, "bad = 1 + \"a\"\ngood = 3\ngood\n:exit\n");
    assert!(ok);
    assert!(stdout.contains("3 : number"), "stdout: {stdout}");
}

/// An entry that goes multi-line ends at a blank line, even once it parses —
/// elm's `ifDone` rule, which is what makes a `let` block work.
#[test]
fn repl_reads_multi_line_entries() {
    let dir = temp_dir();
    let project = repl_project(&dir);
    let (ok, stdout, stderr) = alm_repl(&project, "let\n  y = 5\nin\ny * y\n\n:exit\n");
    assert!(ok, "stderr: {stderr}");
    assert!(stdout.contains("| | | | 25 : number"), "stdout: {stdout}");
}

#[test]
fn repl_commands_work() {
    let dir = temp_dir();
    let project = repl_project(&dir);
    let (ok, stdout, _) = alm_repl(&project, ":help\n:nonsense\nx = 1\n:reset\nx\n:exit\n");
    assert!(ok);
    assert!(stdout.contains(":reset   Clear all previous imports and definitions"));
    assert!(stdout.contains("I do not recognize the :nonsense command."));
    assert!(stdout.contains("<reset>"));
    // After a reset the definition is gone, so referring to it is an error
    // rather than printing 1.
    assert!(!stdout.contains("1 : number\n> \n"), "stdout: {stdout}");
}

#[test]
fn repl_reports_a_port_declaration_it_cannot_run() {
    let dir = temp_dir();
    let project = repl_project(&dir);
    let (ok, stdout, _) = alm_repl(&project, "port send : String -> Cmd msg\n:exit\n");
    assert!(ok);
    assert!(stdout.contains("I cannot handle port declarations."), "stdout: {stdout}");
}

/// A type annotation is not a declaration on its own — the definition has to
/// follow. Ending the entry at the annotation stored it under the name it
/// annotates, and the definition then replaced it, so the types were silently
/// dropped and `f 1.5 2.5` was accepted for `f : Int -> Int -> Int`.
#[test]
fn repl_keeps_an_annotation_with_its_definition() {
    let dir = temp_dir();
    let project = repl_project(&dir);
    let (ok, stdout, stderr) =
        alm_repl(&project, "f : Int -> Int -> Int\nf a b = a + b\n\nf 1 2\nf 1.5 2.5\n:exit\n");
    assert!(ok, "stderr: {stderr}");
    // The annotation and the definition are one entry, so the prompt
    // continues to the definition and then to the blank line that ends it.
    assert!(stdout.contains("> | | "), "stdout: {stdout}");
    // `Int`, not `number` — the annotation is what makes it concrete.
    assert!(stdout.contains("<function> : Int -> Int -> Int"), "stdout: {stdout}");
    assert!(stdout.contains("3 : Int"), "stdout: {stdout}");
    // The annotation survived, so floats are rejected.
    assert!(stderr.contains("`f` needs the 1st argument to be"), "stderr: {stderr}");
}

/// Read from an SSE stream until `count` events have arrived or the deadline
/// passes, returning what was received.
///
/// `subscribed` is signalled once the server's greeting has arrived, which is
/// the point from which a broadcast will actually reach this reader. A missed
/// event is never replayed, so a test must not change anything until then —
/// sleeping instead is a race that only shows up under load.
fn read_events(
    port: u16,
    count: usize,
    deadline: std::time::Duration,
    subscribed: std::sync::mpsc::Sender<()>,
) -> String {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(std::time::Duration::from_millis(50))).unwrap();
    stream
        .write_all(
            b"GET /_alm/live HTTP/1.1\r\nHost: localhost\r\nAccept: text/event-stream\r\n\r\n",
        )
        .unwrap();
    let start = std::time::Instant::now();
    let mut text = String::new();
    let mut buf = [0u8; 4096];
    let mut announced = false;
    while start.elapsed() < deadline {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => text.push_str(&String::from_utf8_lossy(&buf[..n])),
            Err(_) => {}
        }
        if !announced && text.contains(": connected") {
            announced = true;
            let _ = subscribed.send(());
        }
        if text.matches("event: ").count() >= count {
            break;
        }
    }
    text
}

/// Spawn a reader and block until it is actually subscribed.
fn events_after(
    port: u16,
    count: usize,
    change: impl FnOnce(),
) -> String {
    let (tx, rx) = std::sync::mpsc::channel();
    let reader =
        std::thread::spawn(move || read_events(port, count, std::time::Duration::from_secs(20), tx));
    rx.recv_timeout(std::time::Duration::from_secs(10))
        .expect("the live endpoint never greeted the reader");
    change();
    reader.join().unwrap()
}

fn start(dir: &Path, args: &[&str]) -> (std::process::Child, u16) {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut all: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    all.push(format!("--port={port}"));
    let child = Command::new(env!("CARGO_BIN_EXE_alm"))
        .args(&all)
        .current_dir(dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to start alm");
    for _ in 0..200 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return (child, port);
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    panic!("server never came up on port {port}");
}

/// A counter program, so a swap has state worth preserving.
fn counter_project(dir: &Path) -> PathBuf {
    let project = dir.join("live");
    std::fs::create_dir_all(project.join("src")).unwrap();
    std::fs::write(
        project.join("elm.json"),
        r#"{ "type": "application", "source-directories": ["src"],
             "dependencies": { "direct": {}, "indirect": {} } }"#,
    )
    .unwrap();
    std::fs::write(
        project.join("src/Main.elm"),
        "module Main exposing (main)\n\nmain = \"one\"\n",
    )
    .unwrap();
    project
}

#[test]
fn live_updating_tells_the_page_when_sources_change() {
    let dir = temp_dir();
    let project = counter_project(&dir);
    let (mut child, port) = start(&project, &["reactor"]);

    // The shim is in the page and points at the live endpoint.
    let (_, _, body) = get(port, "/src/Main.elm");
    assert!(body.contains("/_alm/live"), "no shim in the page");
    assert!(body.contains("EventSource"), "no shim in the page");

    // An edit produces an event on the stream.
    let source = project.join("src/Main.elm");
    let events = events_after(port, 1, || {
        std::fs::write(&source, "module Main exposing (main)\n\nmain = \"two\"\n").unwrap();
    });
    assert!(events.contains(": connected"), "no greeting: {events}");
    assert!(events.contains("event: changed"), "no change event: {events}");

    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn no_hot_reload_turns_the_whole_thing_off() {
    let dir = temp_dir();
    let project = counter_project(&dir);
    let (mut child, port) = start(&project, &["reactor", "--no-hot-reload"]);

    let (_, _, body) = get(port, "/src/Main.elm");
    assert!(!body.contains("_alm/live"), "the page must not be given a shim");
    assert!(!body.contains("__alm_app__"), "no swap handles either");
    let (status, _, _) = get(port, "/_alm/live");
    assert!(status.starts_with("HTTP/1.1 404"), "the endpoint must be gone: {status}");

    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn make_live_serves_the_program_and_its_bundle() {
    let dir = temp_dir();
    let project = counter_project(&dir);
    let (mut child, port) = start(&project, &["make", "src/Main.elm", "--live"]);

    let (status, _, body) = get(port, "/");
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");
    assert!(body.contains("Elm.Main.init"), "the program is not on the page");
    assert!(body.contains("/_alm/bundle.js"), "the shim has nowhere to fetch from");

    // The bundle is the program on its own, so a swap can evaluate it.
    let (status, headers, bundle) = get(port, "/_alm/bundle.js");
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");
    assert!(headers.contains("text/javascript"), "{headers}");
    assert!(bundle.contains("_Platform_export"), "that is not a bundle");
    assert!(!bundle.contains("<!DOCTYPE"), "the bundle must not be a page");

    let _ = child.kill();
    let _ = child.wait();
}

/// A build that fails is reported on the stream rather than taking the server
/// down, and the last good program stays available.
#[test]
fn a_failed_rebuild_is_reported_and_the_last_good_build_survives() {
    let dir = temp_dir();
    let project = counter_project(&dir);
    let (mut child, port) = start(&project, &["make", "src/Main.elm", "--live"]);

    let source = project.join("src/Main.elm");
    let events = events_after(port, 1, || {
        std::fs::write(&source, "module Main exposing (main)\n\nmain = 1 + \"nope\"\n").unwrap();
    });
    assert!(events.contains("event: failed"), "no failure event: {events}");
    assert!(events.contains("TYPE MISMATCH"), "the reports are not in the event: {events}");
    // The reports must survive being put in a `data:` field.
    assert!(
        !events.split("event: failed").nth(1).unwrap().starts_with("\ndata:\n"),
        "the data field is empty"
    );

    // The program from the last build that worked is still being served.
    let (status, _, bundle) = get(port, "/_alm/bundle.js");
    assert!(status.starts_with("HTTP/1.1 200"), "the good build should still serve: {status}");
    assert!(bundle.contains("_Platform_export"));

    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn make_live_rejects_only_what_it_cannot_serve() {
    let dir = temp_dir();
    let project = counter_project(&dir);
    for flag in ["--target=native", "--target=wasm-gc", "--docs=docs.json"] {
        let output = Command::new(env!("CARGO_BIN_EXE_alm"))
            .args(["make", "src/Main.elm", "--live", flag])
            .current_dir(&project)
            .output()
            .expect("run alm");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{flag} should be refused");
        assert!(stderr.contains("does not go with `--live`"), "{flag}: {stderr}");
    }
}

/// `--live --output` is the embedded case: the program is one piece of a larger
/// app, whose own page loads it. So the file has to be written, and it has to
/// carry the live-reload client — that page knows nothing about alm.
#[test]
fn make_live_writes_the_output_with_the_client_in_it() {
    let dir = temp_dir();
    let project = counter_project(&dir);
    std::fs::create_dir_all(project.join("static")).unwrap();
    let (mut child, port) =
        start(&project, &["make", "src/Main.elm", "--live", "--output=static/app.js"]);

    let written = project.join("static/app.js");
    let bundle = wait_for(&written, "_Platform_export");

    // The client is in the bundle, and reaches back to this server by absolute
    // URL — the page loading it comes from somewhere else entirely.
    assert!(bundle.contains("EventSource"), "no live-reload client in the file");
    assert!(
        bundle.contains(&format!("\"http://127.0.0.1:{port}/_alm/live\"")),
        "the client cannot reach the server"
    );
    assert!(
        bundle.contains(&format!("\"http://127.0.0.1:{port}/_alm/bundle.js\"")),
        "the client has nowhere to fetch a new build from"
    );
    // The registry is installed *after* the program, which is what keeps the
    // program on the lines it was compiled at — see
    // `the_written_bundle_starts_exactly_where_a_plain_build_does`. It is read
    // when `init` runs, so coming last is soon enough.
    let registry = bundle.find(REGISTRY).expect("no registry");
    assert!(registry > bundle.find("_Platform_export").unwrap(), "nothing may precede the program");

    // Serving carries on as before, so the same run works either way.
    let (status, _, body) = get(port, "/");
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");
    assert!(body.contains("Elm.Main.init"), "the program is not on the served page");

    // An edit rewrites the file.
    let source = project.join("src/Main.elm");
    std::fs::write(&source, "module Main exposing (main)\n\nmain = \"rewritten\"\n").unwrap();
    wait_for(&written, "rewritten");

    let _ = child.kill();
    let _ = child.wait();
}

/// Installing the registry, as opposed to the runtime merely looking for one.
const REGISTRY: &str = "window.__alm_hot__ = window.__alm_hot__ ||";

/// The output is written after the port is bound, and rebuilt a poll interval
/// after an edit, so reading it is always a wait.
fn wait_for(path: &Path, needle: &str) -> String {
    for _ in 0..400 {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        if text.contains(needle) {
            return text;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    panic!("{} never contained {needle:?}", path.display());
}

/// Nothing may be *prepended* to the written bundle. A source map addresses
/// generated lines, so one line added above the program puts every mapping a line
/// out, and a debugger then silently refuses the breakpoints it cannot place.
/// Appending is free, so the whole live-reload client goes at the end.
#[test]
fn the_written_bundle_starts_exactly_where_a_plain_build_does() {
    let dir = temp_dir();
    let project = counter_project(&dir);

    // What the same program compiles to with no live machinery at all.
    let plain = Command::new(env!("CARGO_BIN_EXE_alm"))
        .args(["make", "src/Main.elm", "--output=plain.js"])
        .current_dir(&project)
        .output()
        .expect("run alm");
    assert!(plain.status.success(), "{}", String::from_utf8_lossy(&plain.stderr));
    let plain = std::fs::read_to_string(project.join("plain.js")).unwrap();

    let (mut child, _port) = start(
        &project,
        &["make", "src/Main.elm", "--live", "--output=live.js", "--source-maps"],
    );
    let written = project.join("live.js");
    let bundle = wait_for(&written, "_Platform_export");

    assert!(
        bundle.starts_with(&plain),
        "the program must occupy the same lines it was compiled at; it starts with {:?}",
        &bundle[..plain.len().min(200)],
    );
    // And the client really is in there — otherwise this passes trivially.
    assert!(bundle.len() > plain.len(), "nothing was appended");
    assert!(bundle.contains("EventSource"), "no live-reload client");
    // The comment naming the map applies to what precedes it, so it goes last.
    assert!(
        bundle.trim_end().lines().last().unwrap().starts_with("//# sourceMappingURL="),
        "the map comment must be the last line"
    );
    assert!(project.join("live.js.map").exists(), "no map was written");

    let _ = child.kill();
    let _ = child.wait();
}

/// The bundle a swap fetches must be the bare program. If it carried the client
/// too, every save would leave another event stream open.
#[test]
fn the_bundle_a_swap_fetches_has_no_client_in_it() {
    let dir = temp_dir();
    let project = counter_project(&dir);
    let (mut child, port) =
        start(&project, &["make", "src/Main.elm", "--live", "--output=app.js"]);

    let (_, headers, bundle) = get(port, "/_alm/bundle.js");
    assert!(bundle.contains("_Platform_export"), "that is not a bundle");
    assert!(!bundle.contains("EventSource"), "the fetched bundle must not carry the client");

    // A page on another origin has to be able to read both the bundle and the
    // fingerprint, or every swap silently loses the model.
    assert!(headers.contains("Access-Control-Allow-Origin: *"), "{headers}");
    assert!(headers.contains("Access-Control-Expose-Headers: X-Alm-Model"), "{headers}");

    let _ = child.kill();
    let _ = child.wait();
}

/// `--no-hot-reload` with `--output` is a plain watch-and-write: the file is kept
/// up to date, and nothing at all is injected into it — for an app whose own
/// reloader will notice the file changing.
#[test]
fn no_hot_reload_writes_a_clean_bundle_and_keeps_writing_it() {
    let dir = temp_dir();
    let project = counter_project(&dir);
    let (mut child, _port) = start(
        &project,
        &["make", "src/Main.elm", "--live", "--output=app.js", "--no-hot-reload"],
    );

    let written = project.join("app.js");
    let bundle = wait_for(&written, "_Platform_export");
    assert!(!bundle.contains("EventSource"), "no client when hot reload is off");
    // The runtime always *looks* for a registry; with nothing to swap, nothing
    // installs one, so it stays the ordinary build it would be without --live.
    assert!(!bundle.contains(REGISTRY), "no registry either");

    // Still rebuilt on a change: what is off is telling a page about it, not
    // keeping the file current.
    let source = project.join("src/Main.elm");
    std::fs::write(&source, "module Main exposing (main)\n\nmain = \"rewritten\"\n").unwrap();
    let bundle = wait_for(&written, "rewritten");
    assert!(!bundle.contains("EventSource"), "still no client after a rebuild");

    let _ = child.kill();
    let _ = child.wait();
}
