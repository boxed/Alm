mod bump;
mod diff;
mod init;
mod install;
mod reactor;

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("make") => make(&args[1..]),
        Some("init") => {
            if args.len() > 1 {
                eprintln!("`alm init` takes no arguments.");
                return ExitCode::FAILURE;
            }
            init::run(use_color())
        }
        Some("install") => match &args[1..] {
            [package] => install::run(package, use_color()),
            _ => {
                eprintln!("Usage: alm install <author/package>");
                ExitCode::FAILURE
            }
        },
        Some("diff") => diff::run(&args[1..], use_color()),
        Some("reactor") => reactor::run(&args[1..], use_color()),
        Some("bump") => {
            if args.len() > 1 {
                eprintln!("`alm bump` takes no arguments.");
                return ExitCode::FAILURE;
            }
            bump::run(use_color())
        }
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
         \x20   alm init\n\
         \x20   alm install <author/package>\n\
         \x20   alm diff [<package>] [<version> [<version>]]\n\
         \x20   alm bump\n\
         \x20   alm reactor [--port=8000]\n\
         \x20   alm make <file.elm> [--output=<file>] [--target=js|native|wasm-gc]\n\
         \x20                       [--source-maps] [--dev] [--optimize]\n\
         \x20                       [--report=json] [--docs=<file>]\n\n\
         `init` starts a project: it writes an elm.json and creates src/.\n\
         Dependencies are resolved from the packages already in ~/.elm, so it\n\
         works offline and downloads nothing, and `install` adds a package\n\
         to an existing elm.json the same way.\n\n\
         `diff` compares two versions of a package's API and says whether the\n\
         change is PATCH, MINOR or MAJOR. With no versions it compares the\n\
         code here against the newest release in ~/.elm, and `bump` sets\n\
         the version in elm.json to whatever that change calls for.\n\
         `reactor` serves the current directory: browse to an Elm file and it\n\
         compiles and runs.\n\n\
         `make` compiles an Elm module. The default target is JavaScript, with\n\
         the output defaulting to the input file name with a .js\n\
         extension. `--target=native` compiles to a binary instead (the\n\
         output defaults to the input file name without an extension).\n\
         `--source-maps` (js and wasm-gc targets) writes a .map beside the\n\
         output so browser devtools show Elm source; tree-shaking still runs,\n\
         so the output is the same size as an ordinary build.\n\
         `--optimize` is elm's production build; it refuses to compile while\n\
         any `Debug` call survives. `--report=json` writes diagnostics to\n\
         stderr as JSON for editors. `--docs=<file>` writes a package's\n\
         docs.json."
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
    // `--report=json`: machine-readable diagnostics for editor plugins.
    let mut report_json = false;
    // `--optimize`: elm's production build. Refuses to compile while any
    // `Debug` call survives.
    let mut optimize = false;
    // `--docs=<file>`: write a package's docs.json.
    let mut docs: Option<PathBuf> = None;
    for arg in args {
        if let Some(path) = arg.strip_prefix("--output=") {
            output = Some(PathBuf::from(path));
        } else if arg == "--source-maps" {
            source_maps = true;
        } else if arg == "--dev" {
            opt = OptLevel::Debug;
        } else if arg == "--optimize" {
            optimize = true;
        } else if let Some(path) = arg.strip_prefix("--docs=") {
            docs = Some(PathBuf::from(path));
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
        } else if let Some(kind) = arg.strip_prefix("--report=") {
            match kind {
                "json" => report_json = true,
                other => {
                    eprintln!("Unknown report type `{}`. I only know json.", other);
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

    if let Some(docs_path) = &docs {
        match alm_compiler::project::generate_docs(&input) {
            Ok(json) => {
                if let Err(err) = std::fs::write(docs_path, json) {
                    eprintln!("I could not write {}: {}", docs_path.display(), err);
                    return ExitCode::FAILURE;
                }
            }
            Err(errors) => {
                let root = alm_compiler::project::project_root(&input);
                let color = use_color();
                for error in &errors {
                    eprint!("{}", error.render_from(Some(&root), color));
                }
                return ExitCode::FAILURE;
            }
        }
    }

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
        Backend::Js => alm_compiler::project::compile_project_with(&input, optimize).and_then(
            |(javascript, warnings)| {
                let output = output.unwrap_or_else(|| input.with_extension("js"));
                std::fs::write(&output, javascript).map_err(|err| {
                    eprintln!("I could not write {}: {}", output.display(), err);
                    Vec::new()
                })?;
                Ok((output, warnings))
            },
        ),
    };

    match result {
        Ok((output, warnings)) => {
            // In JSON mode elm reports nothing at all on success — no progress,
            // no tally — so an editor never has to filter prose out.
            if !report_json {
                for w in &warnings {
                    eprintln!("{}\n", w.render());
                }
                println!("Success! Compiled to {}", output.display());
            }
            ExitCode::SUCCESS
        }
        Err(mut errors) => {
            // Headers name each file relative to the project root, as elm does,
            // and modules are ordered by when they were last edited — elm shows
            // the file you touched most recently last, nearest the prompt.
            let root = alm_compiler::project::project_root(&input);
            errors.sort_by_key(|e| std::fs::metadata(&e.path).and_then(|m| m.modified()).ok());

            if report_json {
                // One line on stderr, with no trailing newline — as elm writes
                // it. A whole-build failure gets elm's other envelope.
                match errors.split_first() {
                    Some((only, [])) if only.is_whole_build() => {
                        eprint!("{}", only.to_json_error());
                    }
                    _ => {
                        let bodies: Vec<String> = errors.iter().map(|e| e.to_json()).collect();
                        eprint!(
                            "{{\"type\":\"compile-errors\",\"errors\":[{}]}}",
                            bodies.join(",")
                        );
                    }
                }
                return ExitCode::FAILURE;
            }

            // elm counts *modules*, and its build tracker prints the tally on
            // stdout and flushes it before any report reaches stderr — so when
            // both are the same terminal the summary comes first.
            let modules: std::collections::BTreeSet<_> =
                errors.iter().map(|e| e.path.clone()).collect();
            match modules.len() {
                0 => {}
                1 => println!("Detected problems in 1 module."),
                n => println!("Detected problems in {} modules.", n),
            }
            let _ = std::io::Write::flush(&mut std::io::stdout());

            let color = use_color();
            for (i, error) in errors.iter().enumerate() {
                if i > 0 {
                    let separator = module_separator(
                        &errors[i - 1].module_name(),
                        &error.module_name(),
                        color,
                    );
                    eprint!("{separator}");
                }
                eprint!("{}", error.render_from(Some(&root), color));
            }
            ExitCode::FAILURE
        }
    }
}

/// Whether to write ANSI escapes on stderr. elm's rule is simply "is the handle
/// a terminal"; `NO_COLOR` and `CLICOLOR_FORCE` are the conventional overrides
/// (<https://no-color.org>), which elm does not implement but which cost
/// nothing and are what someone setting them expects.
fn use_color() -> bool {
    if std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) {
        return false;
    }
    if std::env::var_os("CLICOLOR_FORCE").is_some_and(|v| !v.is_empty() && v != "0") {
        return true;
    }
    std::io::IsTerminal::is_terminal(&std::io::stderr())
}

/// The band elm draws between two modules' error reports, in dull red.
fn module_separator(before: &str, after: &str, color: bool) -> String {
    use alm_compiler::reporting::{Color, Doc};
    let head = format!("{before}  \u{2191}    ");
    let indent = 80usize.saturating_sub(head.chars().count());
    let band = format!(
        "{}{}\n====o======================================================================o====\n    \u{2193}  {}",
        " ".repeat(indent),
        head,
        after
    );
    let doc = Doc::color(Color::Red, Doc::text(band));
    let rendered = if color { doc.render_ansi(usize::MAX) } else { doc.render(usize::MAX) };
    format!("{rendered}\n\n\n")
}
