//! `alm init` — port of elm's `Init.hs`.
//!
//! Asks once, then writes an application `elm.json` and creates `src/`. The
//! dependency set is elm's three defaults (core, browser, html) resolved
//! together with everything they need.
//!
//! elm resolves against the package registry; alm resolves against whatever is
//! already in `~/.elm`, so `init` works offline and never downloads anything.
//! On a machine that has built an Elm project before, the answer is the same.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

use alm_compiler::packages::{self, Constraint, Version};
use alm_compiler::reporting::{cyan, green, sentence, words, Doc};

/// The packages `elm init` starts a project with.
const DEFAULTS: &[&str] = &["elm/browser", "elm/core", "elm/html"];

pub fn run(color: bool) -> ExitCode {
    if Path::new("elm.json").is_file() {
        eprint!("{}", already_exists(color));
        return ExitCode::FAILURE;
    }

    print!("{}", question());
    let _ = std::io::stdout().flush();
    if !approved() {
        println!("Okay, I did not make any changes!");
        return ExitCode::SUCCESS;
    }

    let roots: BTreeMap<String, Constraint> =
        DEFAULTS.iter().map(|name| (name.to_string(), Constraint::anything())).collect();
    let solution = match packages::solve(&roots) {
        Ok(solution) => solution,
        Err(packages::SolveError::Unsatisfiable(package)) => {
            eprint!("{}", no_solution(&package, color));
            return ExitCode::FAILURE;
        }
    };

    if let Err(err) = std::fs::create_dir_all("src") {
        eprintln!("I could not create the src directory: {err}");
        return ExitCode::FAILURE;
    }
    if let Err(err) = std::fs::write("elm.json", outline(&solution)) {
        eprintln!("I could not write elm.json: {err}");
        return ExitCode::FAILURE;
    }
    println!("Okay, I created it. Now read that link!");
    ExitCode::SUCCESS
}

/// `[Y/n]` — anything but an explicit "n"/"no" is yes, including just Enter.
pub fn approved() -> bool {
    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer).is_err() {
        return false;
    }
    !matches!(answer.trim().to_ascii_lowercase().as_str(), "n" | "no")
}

fn question() -> String {
    let paragraphs = [
        sentence(
            [
                words("Hello! Elm projects always start with an"),
                vec![green("elm.json")],
                words("file. I can create them!"),
            ]
            .concat(),
        ),
        Doc::reflow(
            "Now you may be wondering, what will be in this file? How do I add Elm files to my \
             project? How do I see it in the browser? How will my code grow? Do I need more \
             directories? What about tests? Etc.",
        ),
        sentence(
            [
                words("Check out"),
                vec![cyan("<https://elm-lang.org/0.19.1/init>")],
                words("for all the answers!"),
            ]
            .concat(),
        ),
        Doc::text("Knowing all that, would you like me to create an elm.json file now? [Y/n]: "),
    ];
    // The prompt is plain: it is a question, not a diagnostic, and elm does not
    // color it on the way out either.
    paragraphs.iter().map(|p| p.render(80)).collect::<Vec<_>>().join("\n\n")
}

fn already_exists(color: bool) -> String {
    report(
        "EXISTING PROJECT",
        Doc::reflow(
            "You already have an elm.json file, so there is nothing for me to initialize!",
        ),
        sentence(
            [
                words("Maybe"),
                vec![cyan("<https://elm-lang.org/0.19.1/init>")],
                words("can help you figure out what to do next?"),
            ]
            .concat(),
        ),
        color,
    )
}

fn no_solution(package: &str, color: bool) -> String {
    report(
        "NO OFFLINE SOLUTION",
        Doc::reflow(&format!(
            "I could not find a version of {package} in your ~/.elm package cache that fits \
             everything else."
        )),
        Doc::reflow(
            "alm never downloads anything: it resolves dependencies from packages already on \
             this machine. Running `elm install` once, or building any project that uses these \
             packages, will populate the cache.",
        ),
        color,
    )
}

/// A whole-command report, in the shape `Reporting.Exit.Help` gives one: a
/// header bar that runs to the margin, then two paragraphs. With no source
/// snippet, `after` is simply the second paragraph.
pub fn report(title: &str, before: Doc, after: Doc, color: bool) -> String {
    report_about("", title, before, after, color)
}

/// The same, for a report elm attributes to a file — the header bar then ends
/// with that name instead of running to the margin.
pub fn report_about(
    path: &str,
    title: &str,
    before: Doc,
    after: Doc,
    color: bool,
) -> String {
    let report = alm_compiler::reporting::Report {
        title: title.to_string(),
        region: alm_compiler::reporting::Region::ZERO,
        message: String::new(),
        elm: Some(alm_compiler::reporting::ElmBody {
            before,
            after,
            notes: Vec::new(),
            region: None,
            highlight: alm_compiler::reporting::Region::ZERO,
        }),
    };
    if color {
        report.render_ansi(path, "")
    } else {
        report.render(path, "")
    }
}

/// The `elm.json` elm writes: four-space indentation, direct dependencies
/// separated from the indirect ones they pulled in.
fn outline(solution: &BTreeMap<String, Version>) -> String {
    let entries = |names: Vec<(&String, &Version)>| -> String {
        names
            .iter()
            .map(|(name, version)| format!("            \"{name}\": \"{version}\""))
            .collect::<Vec<_>>()
            .join(",\n")
    };
    let (direct, indirect): (Vec<_>, Vec<_>) =
        solution.iter().partition(|(name, _)| DEFAULTS.contains(&name.as_str()));
    let block = |items: Vec<(&String, &Version)>| {
        if items.is_empty() {
            "{}".to_string()
        } else {
            format!("{{\n{}\n        }}", entries(items))
        }
    };
    format!(
        r#"{{
    "type": "application",
    "source-directories": [
        "src"
    ],
    "elm-version": "0.19.1",
    "dependencies": {{
        "direct": {},
        "indirect": {}
    }},
    "test-dependencies": {{
        "direct": {{}},
        "indirect": {{}}
    }}
}}
"#,
        block(direct),
        block(indirect)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    #[test]
    fn the_outline_matches_the_one_elm_writes() {
        let solution: BTreeMap<String, Version> = [
            ("elm/browser", "1.0.2"),
            ("elm/core", "1.0.5"),
            ("elm/html", "1.0.1"),
            ("elm/json", "1.1.4"),
            ("elm/time", "1.0.0"),
            ("elm/url", "1.0.0"),
            ("elm/virtual-dom", "1.0.5"),
        ]
        .into_iter()
        .map(|(name, v)| (name.to_string(), version(v)))
        .collect();
        assert_eq!(
            outline(&solution),
            r#"{
    "type": "application",
    "source-directories": [
        "src"
    ],
    "elm-version": "0.19.1",
    "dependencies": {
        "direct": {
            "elm/browser": "1.0.2",
            "elm/core": "1.0.5",
            "elm/html": "1.0.1"
        },
        "indirect": {
            "elm/json": "1.1.4",
            "elm/time": "1.0.0",
            "elm/url": "1.0.0",
            "elm/virtual-dom": "1.0.5"
        }
    },
    "test-dependencies": {
        "direct": {},
        "indirect": {}
    }
}
"#
        );
    }

    #[test]
    fn an_empty_indirect_block_stays_on_one_line() {
        let solution: BTreeMap<String, Version> =
            [("elm/core".to_string(), version("1.0.5"))].into_iter().collect();
        assert!(outline(&solution).contains("\"indirect\": {}"));
    }

    /// The prompt is the text elm prints, wrapped the same way.
    #[test]
    fn the_question_reads_like_elms() {
        let text = question();
        assert!(text.starts_with("Hello! Elm projects always start with an elm.json file."));
        assert!(text.contains("Check out <https://elm-lang.org/0.19.1/init> for all the answers!"));
        assert!(text.ends_with("create an elm.json file now? [Y/n]: "));
        for line in text.lines() {
            assert!(line.chars().count() <= 80, "line wider than 80 columns: {line:?}");
        }
    }
}
