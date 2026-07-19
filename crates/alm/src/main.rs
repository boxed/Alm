use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("make") => make(&args[1..]),
        Some("--help" | "-h") | None => {
            print_help();
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("Unknown command `{}`.\n", other);
            print_help();
            ExitCode::FAILURE
        }
    }
}

fn print_help() {
    println!(
        "alm — an Elm compiler written in Rust\n\n\
         Usage:\n\
         \x20   alm make <file.elm> [--output=<file>] [--target=js|native|wasm|wasm-uniform|native-typed] [--source-maps]\n\n\
         Compiles an Elm module. The default target is JavaScript, with\n\
         the output defaulting to the input file name with a .js\n\
         extension. `--target=native` compiles to a binary instead (the\n\
         output defaults to the input file name without an extension).\n\
         `--source-maps` (js and wasm-gc targets) writes a .map beside the\n\
         output so browser devtools show Elm source; tree-shaking still runs,\n\
         so the output is the same size as an ordinary build."
    );
}

fn make(args: &[String]) -> ExitCode {
    use alm_compiler::generate::native::Target;
    #[derive(PartialEq)]
    enum Backend {
        Js,
        Native(Target),
        /// The typed, monomorphized backend (unboxed native code).
        Typed(Target),
        /// The from-scratch WebAssembly GC backend (engine-managed GC).
        WasmGc,
    }
    let mut input: Option<PathBuf> = None;
    let mut output: Option<PathBuf> = None;
    let mut backend = Backend::Js;
    let mut source_maps = false;
    for arg in args {
        if let Some(path) = arg.strip_prefix("--output=") {
            output = Some(PathBuf::from(path));
        } else if arg == "--source-maps" {
            source_maps = true;
        } else if let Some(target) = arg.strip_prefix("--target=") {
            match target {
                "js" => backend = Backend::Js,
                "native" => backend = Backend::Native(Target::Native),
                // `wasm` uses the monomorphized (typed) backend — unboxed, so
                // allocation-heavy code is fast. `wasm-uniform` is the boxed
                // fallback (broader coverage, the correctness substrate).
                "wasm" | "wasm-typed" => backend = Backend::Typed(Target::Wasm),
                "wasm-uniform" => backend = Backend::Native(Target::Wasm),
                "wasm-gc" | "wasmgc" => backend = Backend::WasmGc,
                "native-typed" => backend = Backend::Typed(Target::Native),
                other => {
                    eprintln!(
                        "Unknown target `{}`. I know js, native, wasm, wasm-uniform, wasm-gc, and native-typed.",
                        other
                    );
                    return ExitCode::FAILURE;
                }
            }
        } else if arg.starts_with("--") {
            eprintln!("Unknown flag `{}`.", arg);
            return ExitCode::FAILURE;
        } else if input.is_some() {
            eprintln!("Please give me exactly one .elm file.");
            return ExitCode::FAILURE;
        } else {
            input = Some(PathBuf::from(arg));
        }
    }

    let Some(input) = input else {
        eprintln!("Which .elm file should I compile? For example:\n\n    alm make src/Main.elm");
        return ExitCode::FAILURE;
    };

    let result = match backend {
        Backend::Native(target) => {
            let ext = if target == Target::Wasm { "wasm" } else { "" };
            let output = output.unwrap_or_else(|| input.with_extension(ext));
            alm_compiler::project::compile_project_native(&input, &output, target).map(|()| output)
        }
        Backend::Typed(target) => {
            let ext = if target == Target::Wasm { "wasm" } else { "" };
            let output = output.unwrap_or_else(|| input.with_extension(ext));
            alm_compiler::project::compile_project_typed(&input, &output, target).map(|()| output)
        }
        Backend::WasmGc => {
            let output = output.unwrap_or_else(|| input.with_extension("wasm"));
            alm_compiler::project::compile_project_wasmgc(&input, &output, source_maps)
                .map(|()| output)
        }
        Backend::Js if source_maps => {
            alm_compiler::project::compile_project_source_maps(&input).and_then(
                |(mut javascript, map)| {
                    let output = output.unwrap_or_else(|| input.with_extension("js"));
                    let map_path = output.with_extension("js.map");
                    let map_name = map_path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    javascript.push_str(&format!("\n//# sourceMappingURL={}\n", map_name));
                    let write = |p: &PathBuf, data: &str| {
                        std::fs::write(p, data).map_err(|err| {
                            eprintln!("I could not write {}: {}", p.display(), err);
                            Vec::new()
                        })
                    };
                    write(&output, &javascript)?;
                    write(&map_path, &map)?;
                    Ok(output)
                },
            )
        }
        Backend::Js => alm_compiler::project::compile_project(&input).and_then(|javascript| {
            let output = output.unwrap_or_else(|| input.with_extension("js"));
            std::fs::write(&output, javascript).map_err(|err| {
                eprintln!("I could not write {}: {}", output.display(), err);
                Vec::new()
            })?;
            Ok(output)
        }),
    };

    match result {
        Ok(output) => {
            println!("Success! Compiled to {}", output.display());
            ExitCode::SUCCESS
        }
        Err(errors) => {
            let count: usize = errors.iter().map(|e| e.reports.len()).sum();
            for error in &errors {
                eprintln!("{}", error.render());
            }
            if count > 0 {
                eprintln!(
                    "Detected {} problem{}.",
                    count,
                    if count == 1 { "" } else { "s" }
                );
            }
            ExitCode::FAILURE
        }
    }
}
