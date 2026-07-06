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

    // Host runtime (target-cpu=native so it inlines into host codegen).
    build_runtime(
        source,
        &out_dir,
        "alm_runtime",
        &["-C", "target-cpu=native"],
    );

    // WebAssembly runtime, built for wasm32-wasi. No target-cpu (wasm has
    // none). Only built when the wasm target is installed; the wasm backend
    // needs it, the native backend does not.
    build_runtime(
        source,
        &out_dir,
        "alm_runtime_wasm",
        &["--target", "wasm32-wasi"],
    );

    // Stage the wasm link inputs (WASI crt + libc from the Rust wasm32-wasi
    // sysroot) into OUT_DIR so the wasm backend can embed them and link a
    // self-contained module without needing the toolchain at `alm make`
    // time. Also record the wasm-ld path from the pinned LLVM.
    let libdir = run(
        "rustc",
        &[RUNTIME_TOOLCHAIN, "--target", "wasm32-wasi", "--print", "target-libdir"],
    );
    let self_contained = PathBuf::from(libdir.trim()).join("self-contained");
    for f in ["crt1-command.o", "libc.a"] {
        std::fs::copy(self_contained.join(f), out_dir.join(f))
            .unwrap_or_else(|e| panic!("staging wasm link input {f}: {e}"));
    }
    let llvm_prefix = env::var("LLVM_SYS_160_PREFIX")
        .unwrap_or_else(|_| "/opt/homebrew/opt/llvm@16".to_string());
    println!("cargo:rustc-env=ALM_WASM_LD={}/bin/wasm-ld", llvm_prefix);
    println!("cargo:rustc-env=ALM_DWARFDUMP={}/bin/llvm-dwarfdump", llvm_prefix);
}

fn run(cmd: &str, args: &[&str]) -> String {
    let out = Command::new(cmd)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("running {cmd}: {e}"));
    assert!(out.status.success(), "{cmd} {args:?} failed");
    String::from_utf8(out.stdout).unwrap()
}

fn build_runtime(source: &str, out_dir: &PathBuf, crate_name: &str, extra: &[&str]) {
    let status = Command::new("rustc")
        .arg(RUNTIME_TOOLCHAIN)
        .args([
            "--edition",
            "2021",
            "--crate-name",
            crate_name,
            "--crate-type",
            "staticlib",
            "--emit=llvm-bc,link",
            "-C",
            "panic=abort",
            "-C",
            "opt-level=2",
            "-C",
            "overflow-checks=off",
            "-C",
            "debug-assertions=off",
            "--cap-lints",
            "allow",
        ])
        .args(extra)
        .arg(source)
        .arg("--out-dir")
        .arg(out_dir)
        .status()
        .unwrap_or_else(|e| {
            panic!(
                "failed to invoke `rustc {RUNTIME_TOOLCHAIN}` for {crate_name}: {e}\n\
                 install it with: rustup toolchain install 1.72.1 --profile minimal \
                 && rustup target add wasm32-wasi --toolchain 1.72.1"
            )
        });
    assert!(
        status.success(),
        "building the {crate_name} runtime failed (needs the {RUNTIME_TOOLCHAIN} toolchain \
         and the wasm32-wasi target)"
    );
}
