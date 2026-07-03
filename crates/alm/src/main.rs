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
         \x20   alm make <file.elm> [--output=<file.js>]\n\n\
         Compiles an Elm module to JavaScript. The output defaults to the\n\
         input file name with a .js extension."
    );
}

fn make(args: &[String]) -> ExitCode {
    let mut input: Option<PathBuf> = None;
    let mut output: Option<PathBuf> = None;
    for arg in args {
        if let Some(path) = arg.strip_prefix("--output=") {
            output = Some(PathBuf::from(path));
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

    let source = match std::fs::read_to_string(&input) {
        Ok(source) => source,
        Err(err) => {
            eprintln!("I could not read {}: {}", input.display(), err);
            return ExitCode::FAILURE;
        }
    };

    match alm_compiler::compile(&source) {
        Ok(javascript) => {
            let output = output.unwrap_or_else(|| input.with_extension("js"));
            if let Err(err) = std::fs::write(&output, javascript) {
                eprintln!("I could not write {}: {}", output.display(), err);
                return ExitCode::FAILURE;
            }
            println!("Success! Compiled to {}", output.display());
            ExitCode::SUCCESS
        }
        Err(reports) => {
            let path = input.display().to_string();
            for report in &reports {
                eprintln!("{}", report.render(&path, &source));
            }
            eprintln!(
                "Detected {} problem{}.",
                reports.len(),
                if reports.len() == 1 { "" } else { "s" }
            );
            ExitCode::FAILURE
        }
    }
}
