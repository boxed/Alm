//! `alm install <author/package>` — port of elm's `Install.hs`.
//!
//! Adds a package to `elm.json`, re-solving the dependency set so whatever it
//! needs comes along as an indirect dependency. Like the rest of alm, it
//! resolves from the local `~/.elm` cache rather than the registry, so it
//! installs what is already on the machine and never downloads.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

use alm_compiler::packages::{self, Constraint, Version};
use alm_compiler::reporting::{cyan, sentence, words, Doc};

use crate::init;

pub fn run(package: &str, color: bool) -> ExitCode {
    if package.split('/').count() != 2 || package.split('/').any(str::is_empty) {
        eprint!(
            "{}",
            init::report(
                "BAD PACKAGE NAME",
                Doc::reflow(&format!(
                    "I am expecting a package name like elm/http, but I got {package}."
                )),
                Doc::reflow("Package names are always <author>/<project>."),
                color,
            )
        );
        return ExitCode::FAILURE;
    }

    let Ok(outline) = std::fs::read_to_string("elm.json") else {
        eprint!("{}", no_outline(color));
        return ExitCode::FAILURE;
    };

    if packages::cached_versions(package).is_empty() {
        eprint!("{}", not_cached(package, color));
        return ExitCode::FAILURE;
    }

    match Project::parse(&outline) {
        Some(Project::Application(app)) => install_into_app(app, &outline, package),
        Some(Project::Package(deps)) => install_into_package(deps, &outline, package),
        None => {
            eprint!("{}", unreadable(color));
            ExitCode::FAILURE
        }
    }
}

/// The parts of an `elm.json` install cares about.
enum Project {
    /// direct and indirect dependencies.
    Application(AppDeps),
    /// A package's `dependencies` constraints.
    Package(BTreeMap<String, Constraint>),
}

struct AppDeps {
    direct: BTreeMap<String, Version>,
    indirect: BTreeMap<String, Version>,
}

impl Project {
    fn parse(outline: &str) -> Option<Project> {
        let is_package = packages::json_string(outline, "type")? == "package";
        if is_package {
            let deps = packages::object_block(outline, "dependencies")?;
            return Some(Project::Package(
                packages::pairs(deps)
                    .into_iter()
                    .filter_map(|(n, v)| Some((n.to_string(), Constraint::parse(v)?)))
                    .collect(),
            ));
        }
        let deps = packages::object_block(outline, "dependencies")?;
        let versions = |key: &str| -> BTreeMap<String, Version> {
            packages::object_block(deps, key)
                .map(|block| {
                    packages::pairs(block)
                        .into_iter()
                        .filter_map(|(n, v)| Some((n.to_string(), Version::parse(v)?)))
                        .collect()
                })
                .unwrap_or_default()
        };
        Some(Project::Application(AppDeps {
            direct: versions("direct"),
            indirect: versions("indirect"),
        }))
    }
}

fn install_into_app(app: AppDeps, outline: &str, package: &str) -> ExitCode {
    if app.direct.contains_key(package) {
        println!("It is already installed!");
        return ExitCode::SUCCESS;
    }

    // Already present, just not where you can import it: elm offers to move it
    // rather than re-solving.
    if let Some(version) = app.indirect.get(package).copied() {
        print!(
            "I found it in your elm.json file, but in the \"indirect\" dependencies.\n\
             Should I move it into \"direct\" dependencies for more general use? [Y/n]: "
        );
        let _ = std::io::stdout().flush();
        if !init::approved() {
            println!("Okay, I did not change anything!");
            return ExitCode::SUCCESS;
        }
        let mut direct = app.direct;
        let mut indirect = app.indirect;
        direct.insert(package.to_string(), version);
        indirect.remove(package);
        return write_app(outline, &direct, &indirect);
    }

    // A new dependency: re-solve so its own requirements come along, keeping
    // every version already chosen if it still fits.
    let mut roots: BTreeMap<String, Constraint> = app
        .direct
        .iter()
        .chain(app.indirect.iter())
        .map(|(name, version)| (name.clone(), Constraint::covering(*version)))
        .collect();
    roots.insert(package.to_string(), Constraint::anything());
    let solution = match packages::solve(&roots) {
        Ok(solution) => solution,
        Err(packages::SolveError::Unsatisfiable(blocker)) => {
            eprint!("{}", unsolvable(&blocker, false));
            return ExitCode::FAILURE;
        }
    };

    let added: BTreeMap<&String, &Version> =
        solution.iter().filter(|(name, _)| !app.direct.contains_key(*name)
            && !app.indirect.contains_key(*name)).collect();
    print!("{}", plan(&added));
    let _ = std::io::stdout().flush();
    if !init::approved() {
        println!("Okay, I did not change anything!");
        return ExitCode::SUCCESS;
    }

    let mut direct = app.direct;
    let mut indirect = app.indirect;
    direct.insert(package.to_string(), solution[package]);
    for (name, version) in &solution {
        if !direct.contains_key(name) {
            indirect.insert(name.clone(), *version);
        }
    }
    write_app(outline, &direct, &indirect)
}

fn install_into_package(
    deps: BTreeMap<String, Constraint>,
    outline: &str,
    package: &str,
) -> ExitCode {
    if deps.contains_key(package) {
        println!("It is already installed!");
        return ExitCode::SUCCESS;
    }
    let Some(version) = packages::cached_versions(package).pop() else {
        eprint!("{}", not_cached(package, false));
        return ExitCode::FAILURE;
    };
    let constraint = Constraint::covering(version);
    let name = package.to_string();
    let added: BTreeMap<&String, &Version> = [(&name, &version)].into_iter().collect();
    print!("{}", plan(&added));
    let _ = std::io::stdout().flush();
    if !init::approved() {
        println!("Okay, I did not change anything!");
        return ExitCode::SUCCESS;
    }

    let mut updated = deps;
    updated.insert(package.to_string(), constraint);
    let block = updated
        .iter()
        .map(|(name, c)| format!("        \"{name}\": \"{c}\""))
        .collect::<Vec<_>>()
        .join(",\n");
    let replaced = packages::replace_object_block(
        outline,
        "dependencies",
        &format!("\n{block}\n    "),
    );
    match replaced {
        Some(text) => finish(&text),
        None => {
            eprint!("{}", unreadable(false));
            ExitCode::FAILURE
        }
    }
}

/// `Here is my plan:` followed by the packages being added, laid out the way
/// elm lays them out: names padded to the widest plus three, then the version.
fn plan(added: &BTreeMap<&String, &Version>) -> String {
    let width = added.keys().map(|n| n.chars().count()).max().unwrap_or(0) + 3;
    let entries: Vec<String> = added
        .iter()
        .map(|(name, version)| format!("    {name:<width$} {version}"))
        .collect();
    format!(
        "Here is my plan:\n  \n  Add:\n{}\n\nWould you like me to update your elm.json \
         accordingly? [Y/n]: ",
        entries.join("\n")
    )
}

/// Rewrite an application's dependency blocks, leaving the rest of the file
/// alone — an elm.json may carry fields alm does not model.
fn write_app(
    outline: &str,
    direct: &BTreeMap<String, Version>,
    indirect: &BTreeMap<String, Version>,
) -> ExitCode {
    let block = |items: &BTreeMap<String, Version>, indent: &str| {
        if items.is_empty() {
            "{}".to_string()
        } else {
            let body = items
                .iter()
                .map(|(name, version)| format!("{indent}    \"{name}\": \"{version}\""))
                .collect::<Vec<_>>()
                .join(",\n");
            format!("{{\n{body}\n{indent}}}")
        }
    };
    let body = format!(
        "\n        \"direct\": {},\n        \"indirect\": {}\n    ",
        block(direct, "        "),
        block(indirect, "        ")
    );
    match packages::replace_object_block(outline, "dependencies", &body) {
        Some(text) => finish(&text),
        None => {
            eprint!("{}", unreadable(false));
            ExitCode::FAILURE
        }
    }
}

fn finish(outline: &str) -> ExitCode {
    if let Err(err) = std::fs::write("elm.json", outline) {
        eprintln!("I could not write elm.json: {err}");
        return ExitCode::FAILURE;
    }
    println!("Success!");
    ExitCode::SUCCESS
}

fn no_outline(color: bool) -> String {
    init::report(
        "NO elm.json FILE",
        Doc::reflow(
            "You need an elm.json file to install packages, and I could not find one here.",
        ),
        sentence(
            [
                words("Run"),
                vec![cyan("alm init")],
                words("to start a project, and then try installing again."),
            ]
            .concat(),
        ),
        color,
    )
}

fn unreadable(color: bool) -> String {
    init::report(
        "CORRUPT elm.json",
        Doc::reflow("I could not find the dependencies in your elm.json file."),
        Doc::reflow(
            "It should have a \"dependencies\" field: for an application, one holding \
             \"direct\" and \"indirect\" objects; for a package, one mapping names to version \
             constraints.",
        ),
        color,
    )
}

fn not_cached(package: &str, color: bool) -> String {
    init::report(
        "PACKAGE NOT CACHED",
        Doc::reflow(&format!("I could not find {package} in your ~/.elm package cache.")),
        Doc::reflow(
            "alm never downloads anything, so it can only install packages already on this \
             machine. Running `elm install` for it once will fetch it, and then alm can use it.",
        ),
        color,
    )
}

fn unsolvable(blocker: &str, color: bool) -> String {
    init::report(
        "NO OFFLINE SOLUTION",
        Doc::reflow(&format!(
            "Adding this package needs a version of {blocker} that is not in your ~/.elm cache."
        )),
        Doc::reflow(
            "alm resolves dependencies from packages already on this machine. Fetching the \
             missing one with `elm install` will let it continue.",
        ),
        color,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    /// Names are padded to the widest plus three, matching elm's columns.
    #[test]
    fn the_plan_lines_up_like_elms() {
        let (bytes, file, http) = ("elm/bytes".to_string(), "elm/file".to_string(), "elm/http".to_string());
        let (v1, v2, v3) = (version("1.0.8"), version("1.0.5"), version("2.0.0"));
        let added: BTreeMap<&String, &Version> =
            [(&bytes, &v1), (&file, &v2), (&http, &v3)].into_iter().collect();
        assert_eq!(
            plan(&added),
            "Here is my plan:\n  \n  Add:\n    \
             elm/bytes    1.0.8\n    \
             elm/file     1.0.5\n    \
             elm/http     2.0.0\n\n\
             Would you like me to update your elm.json accordingly? [Y/n]: "
        );
    }

    #[test]
    fn an_application_outline_is_parsed_into_its_two_blocks() {
        let outline = r#"{
    "type": "application",
    "dependencies": {
        "direct": { "elm/core": "1.0.5" },
        "indirect": { "elm/json": "1.1.4" }
    }
}"#;
        let Some(Project::Application(app)) = Project::parse(outline) else {
            panic!("should parse as an application");
        };
        assert_eq!(app.direct.get("elm/core"), Some(&version("1.0.5")));
        assert_eq!(app.indirect.get("elm/json"), Some(&version("1.1.4")));
    }

    #[test]
    fn a_package_outline_is_parsed_as_constraints() {
        let outline = r#"{
    "type": "package",
    "dependencies": { "elm/core": "1.0.0 <= v < 2.0.0" }
}"#;
        let Some(Project::Package(deps)) = Project::parse(outline) else {
            panic!("should parse as a package");
        };
        assert_eq!(deps["elm/core"].to_string(), "1.0.0 <= v < 2.0.0");
    }
}
