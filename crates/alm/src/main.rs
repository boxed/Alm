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
         \x20   alm make <file.elm> [--output=<file>] [--target=js|native|wasm-gc] [--source-maps] [--dev]\n\n\
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
    use alm_compiler::generate::native::OptLevel;
    #[derive(PartialEq)]
    enum Backend {
        Js,
        /// LLVM native binary with the uniform (boxed) runtime.
        Native,
        /// The from-scratch WebAssembly GC backend (engine-managed GC).
        WasmGc,
    }
    let mut input: Option<PathBuf> = None;
    let mut output: Option<PathBuf> = None;
    let mut backend = Backend::Js;
    let mut source_maps = false;
    // Native/wasm only: skip LLVM's `O2` pipeline (the ~98% of native build time)
    // for fast dev iteration, at the cost of runtime speed. No effect on js/wasm-gc.
    let mut opt = OptLevel::Release;
    for arg in args {
        if let Some(path) = arg.strip_prefix("--output=") {
            output = Some(PathBuf::from(path));
        } else if arg == "--source-maps" {
            source_maps = true;
        } else if arg == "--dev" {
            opt = OptLevel::Debug;
        } else if let Some(target) = arg.strip_prefix("--target=") {
            match target {
                "js" => backend = Backend::Js,
                "native" => backend = Backend::Native,
                "wasm-gc" | "wasmgc" => backend = Backend::WasmGc,
                other => {
                    eprintln!("Unknown target `{}`. I know js, native, and wasm-gc.", other);
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
        Backend::Native => {
            let output = output.unwrap_or_else(|| input.with_extension(""));
            alm_compiler::project::compile_project_native(&input, &output, opt)
                .map(|w| (output, w))
        }
        Backend::WasmGc => {
            let output = output.unwrap_or_else(|| input.with_extension("wasm"));
            alm_compiler::project::compile_project_wasmgc(&input, &output, source_maps)
                .map(|w| (output, w))
        }
        Backend::Js if source_maps => {
            alm_compiler::project::compile_project_source_maps(&input).and_then(
                |(mut javascript, map, warnings)| {
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
                    Ok((output, warnings))
                },
            )
        }
        Backend::Js => alm_compiler::project::compile_project(&input).and_then(|(javascript, warnings)| {
            let output = output.unwrap_or_else(|| input.with_extension("js"));
            std::fs::write(&output, javascript).map_err(|err| {
                eprintln!("I could not write {}: {}", output.display(), err);
                Vec::new()
            })?;
            Ok((output, warnings))
        }),
    };

    match result {
        Ok((output, warnings)) => {
            for w in &warnings {
                eprintln!("{}\n", w.render());
            }
            println!("Success! Compiled to {}", output.display());
            ExitCode::SUCCESS
        }
        Err(mut errors) => {
            // Headers name each file relative to the project root, as elm does,
            // and modules are ordered by when they were last edited — elm shows
            // the file you touched most recently last, nearest the prompt.
            let root = alm_compiler::project::project_root(&input);
            errors.sort_by_key(|e| {
                std::fs::metadata(&e.path).and_then(|m| m.modified()).ok()
            });
            for (i, error) in errors.iter().enumerate() {
                if i > 0 {
                    eprint!("{}", module_separator(&errors[i - 1].module_name(), &error.module_name()));
                }
                eprint!("{}", error.render_from(Some(&root)));
            }
            // elm counts *modules*, and prints the tally on stdout so a piped
            // stderr holds nothing but the reports themselves.
            let modules: std::collections::BTreeSet<_> =
                errors.iter().map(|e| e.path.clone()).collect();
            match modules.len() {
                0 => {}
                1 => println!("Detected problems in 1 module."),
                n => println!("Detected problems in {} modules.", n),
            }
            ExitCode::FAILURE
        }
    }
}

/// The band elm draws between two modules' error reports.
fn module_separator(before: &str, after: &str) -> String {
    let before = format!("{before}  \u{2191}    ");
    let indent = 80usize.saturating_sub(before.chars().count());
    format!(
        "{}{}\n====o======================================================================o====\n    \u{2193}  {}\n\n\n",
        " ".repeat(indent),
        before,
        after
    )
}
