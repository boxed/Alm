//! Shared plumbing for integration tests that execute compiled JavaScript
//! under node.

#![allow(dead_code)]

use std::path::PathBuf;
use std::process::Command;

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
pub fn write_js(tag: &str, javascript: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "alm-{}-{}-{:?}",
        tag,
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("bundle.js");
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
