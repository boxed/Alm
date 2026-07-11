//! Shared plumbing for integration tests that execute compiled JavaScript
//! under node.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

/// A per-test scratch directory that removes itself when the test finishes.
///
/// The test harnesses build real binaries (~5 MB each) into `temp_dir()`
/// (redirected to the in-project `.almtmp/` by `.cargo/config.toml`), and the
/// per-PID directory names meant nothing was ever reused or removed — tens of
/// gigabytes accumulated across runs. Dropping the guard deletes the
/// directory; a panicking test keeps its artifacts for inspection, as does
/// setting `ALM_KEEP_TEST_DIRS=1`.
///
/// Derefs to `Path`, so call sites keep using `dir.join(..)` / `&dir`.
pub struct TestDir {
    path: PathBuf,
}

/// Create `temp_dir()/<tag>/<name>-<pid>-<thread>` and return its guard.
pub fn test_dir(tag: &str, name: &str) -> TestDir {
    let path = std::env::temp_dir().join(tag).join(format!(
        "{}-{}-{:?}",
        name,
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&path).expect("create test dir");
    TestDir { path }
}

impl std::ops::Deref for TestDir {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for TestDir {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        if std::thread::panicking() || std::env::var_os("ALM_KEEP_TEST_DIRS").is_some() {
            return;
        }
        let _ = std::fs::remove_dir_all(&self.path);
        // Tidy the shared tag directory too once it empties; `remove_dir`
        // refuses non-empty directories, so concurrent tests are unaffected.
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::remove_dir(parent);
        }
    }
}

/// Compile a single Elm module, panicking with rendered reports on failure.
pub fn compile_single(file_name: &str, source: &str) -> String {
    match alm_compiler::compile(source) {
        Ok(js) => js,
        Err(reports) => panic!(
            "compilation failed:\n{}",
            reports
                .iter()
                .map(|r| r.render(file_name, source))
                .collect::<Vec<_>>()
                .join("\n")
        ),
    }
}

/// Write JavaScript to a per-test temp file and return its path.
///
/// The path is stable per (tag, thread) — no PID — so repeated runs overwrite
/// a handful of small files instead of accumulating a directory per process.
/// Cross-process races are not a concern: each test binary uses its own tag,
/// and within a binary the thread id disambiguates parallel tests.
pub fn write_js(tag: &str, javascript: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("alm-js");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{}-{:?}.js", tag, std::thread::current().id()));
    std::fs::write(&path, javascript).unwrap();
    path
}

/// Run a node script, panicking (with the generated JS attached) if node
/// exits nonzero. Returns trimmed stdout.
pub fn run_node(script: &str, javascript_for_error: &str) -> String {
    let output = Command::new("node")
        .arg("-e")
        .arg(script)
        .output()
        .expect("failed to run node");
    if !output.status.success() {
        panic!(
            "node failed:\n{}\n\ngenerated JS:\n{}",
            String::from_utf8_lossy(&output.stderr),
            javascript_for_error
        );
    }
    String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_string()
}
