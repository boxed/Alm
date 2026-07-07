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

    // Regex glue: a sibling crate that wraps `fancy-regex` behind a C ABI, so
    // the elm/regex kernels have a real engine without vendoring one into the
    // single-file runtime. It has crate dependencies, so it's built with cargo
    // (host default toolchain — it needs no LLVM-16 bitcode, only a linkable
    // archive) into an isolated target dir to avoid contending with the outer
    // build's lock. The `.a` is embedded and linked into native programs.
    build_regex_glue(&out_dir);
}

fn build_regex_glue(out_dir: &PathBuf) {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let glue_src = manifest_dir.join("../alm-regex/src/lib.rs");
    println!("cargo:rerun-if-changed={}", glue_src.display());
    let rx_target = out_dir.join("rxbuild");
    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let status = Command::new(cargo)
        .args(["build", "--release", "-p", "alm-regex", "--target-dir"])
        .arg(&rx_target)
        // Match the runtime's `panic=abort`: a panic must never unwind across
        // the C ABI, and it drops std's unwinding symbols (`_rust_eh_personality`)
        // that would otherwise clash with the runtime archive's std at link.
        .env("RUSTFLAGS", "-C panic=abort")
        .status()
        .unwrap_or_else(|e| panic!("building alm-regex glue: {e}"));
    assert!(status.success(), "building the alm-regex glue crate failed");

    // The glue archive statically bundles its own `std` (and `fancy-regex`),
    // which would collide with the runtime archive's `std` at link
    // (`_rust_eh_personality`, allocator shims, …). Merge the glue's objects
    // into ONE relocatable object and localize every symbol except the C entry
    // points (`ld -r -exported_symbols_list`), so its private `std` copy is
    // self-contained and only `alm_rx_*` is visible to the final link.
    let glue_a = rx_target.join("release/libalm_regex.a");
    let merge_dir = out_dir.join("rxmerge");
    let _ = std::fs::remove_dir_all(&merge_dir);
    std::fs::create_dir_all(&merge_dir).unwrap();
    let ar_ok = Command::new("ar")
        .current_dir(&merge_dir)
        .arg("x")
        .arg(&glue_a)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(ar_ok, "extracting the regex glue archive failed");
    let exports = merge_dir.join("exports.txt");
    std::fs::write(
        &exports,
        "_alm_rx_compile\n_alm_rx_contains\n_alm_rx_find\n_alm_rx_split\n_alm_rx_free\n",
    )
    .unwrap();
    let objs: Vec<PathBuf> = std::fs::read_dir(&merge_dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|x| x == "o").unwrap_or(false))
        .collect();
    let merged = out_dir.join("libalm_regex.o");
    let mut cmd = Command::new("ld");
    cmd.arg("-r").arg("-exported_symbols_list").arg(&exports);
    cmd.args(&objs);
    cmd.arg("-o").arg(&merged);
    let ld_ok = cmd.status().map(|s| s.success()).unwrap_or(false);
    assert!(ld_ok, "ld -r merge of the regex glue failed");
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
