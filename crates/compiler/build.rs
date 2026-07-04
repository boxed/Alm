//! Compile the native backend's runtime (a standalone Rust file) into both
//! a static library and LLVM bitcode, stashed in OUT_DIR. `generate::native`
//! embeds both: it links the `.a` (which bundles std and the real symbols)
//! and merges the `.bc` into each program module so LLVM can inline the
//! runtime (rt_add, rt_ctor, rt_apply, the list kernels …) into generated
//! code — cross-module inlining without a separate LTO link step.
//!
//! The bitcode must be readable by the inkwell/llvm-sys LLVM (16), so the
//! runtime is built with a matching LLVM-16 rustc toolchain (1.72.1) rather
//! than whatever the host default is. Built with `panic = abort` so a panic
//! never unwinds across the C ABI into generated code.

use std::env;
use std::path::PathBuf;
use std::process::Command;

/// A rustc toolchain whose bundled LLVM matches the backend's (LLVM 16).
const RUNTIME_TOOLCHAIN: &str = "+1.72.1";

fn main() {
    let source = "src/generate/native_runtime.rs";
    println!("cargo:rerun-if-changed={}", source);

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    let status = Command::new("rustc")
        .arg(RUNTIME_TOOLCHAIN)
        .args([
            "--edition",
            "2021",
            "--crate-name",
            "alm_runtime",
            "--crate-type",
            "staticlib",
            // Emit the static library (link) and the optimized bitcode.
            "--emit=llvm-bc,link",
            "-C",
            "panic=abort",
            "-C",
            "opt-level=2",
            // Match the codegen's target machine so LLVM will inline the
            // runtime into generated code (it refuses across a cpu mismatch).
            "-C",
            "target-cpu=native",
            "-C",
            "overflow-checks=off",
            "-C",
            "debug-assertions=off",
            "--cap-lints",
            "allow",
        ])
        .arg(source)
        .arg("--out-dir")
        .arg(&out_dir)
        .status()
        .unwrap_or_else(|e| {
            panic!(
                "failed to invoke `rustc {RUNTIME_TOOLCHAIN}` for the native runtime: {e}\n\
                 install it with: rustup toolchain install 1.72.1 --profile minimal"
            )
        });

    assert!(
        status.success(),
        "building the native runtime failed (needs the {RUNTIME_TOOLCHAIN} toolchain: \
         `rustup toolchain install 1.72.1 --profile minimal`)"
    );
}
