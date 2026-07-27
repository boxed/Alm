//! `alm publish` — the local half of elm's `Publish.hs`.
//!
//! elm's publish runs a series of checks and then registers the package with
//! <https://package.elm-lang.org>. alm never talks to the network, so it runs
//! every check that can be answered locally and stops before the upload,
//! saying so. The checks are the point: they are what catch a missing README,
//! an unbuildable API or a version number that does not match the change, and
//! finding that out before `elm publish` is worth having.
//!
//! Three of elm's checks need the network and are therefore skipped: reading
//! the tag's commit hash from GitHub's API (the local tag is used instead),
//! downloading the tagged zipball, and compiling that download. The summary at
//! the end lists them, so nothing looks verified that is not.

use std::path::Path;
use std::process::{Command, ExitCode};

use alm_compiler::api_diff::{self, Magnitude};
use alm_compiler::docs_json::{self, Documentation, Names};
use alm_compiler::packages::{self, Version};
use alm_compiler::reporting::doc::{Color, Doc};
use alm_compiler::reporting::{cyan, sentence, words};

use crate::init;

/// The summary `elm init` leaves behind, which is not a summary.
const DEFAULT_SUMMARY: &str = "helpful summary of your project, less than 80 characters";

/// elm requires a README with something in it.
const MIN_README_BYTES: u64 = 300;

pub fn run(color: bool) -> ExitCode {
    let root = alm_compiler::project::project_root(Path::new("elm.json"));
    let Ok(outline) = std::fs::read_to_string(root.join("elm.json")) else {
        eprint!("{}", report("PUBLISH WHAT?",
            Doc::reflow("I cannot find an elm.json so I am not sure what you want me to publish."),
            Doc::reflow("Try running this command from an Elm package directory."), color));
        return ExitCode::FAILURE;
    };
    if packages::json_string(&outline, "type") != Some("package") {
        eprint!("{}", about_outline("CANNOT PUBLISH APPLICATIONS",
            Doc::reflow(
                "Your elm.json says this is an application. Only packages can be published on \
                 <https://package.elm-lang.org>."),
            Doc::Empty, color));
        return ExitCode::FAILURE;
    }
    let (Some(name), Some(version)) = (
        packages::json_string(&outline, "name"),
        packages::json_string(&outline, "version").and_then(Version::parse),
    ) else {
        eprint!("{}", about_outline("CORRUPT elm.json",
            Doc::reflow("I could not find a \"name\" and a \"version\" in your elm.json file."),
            Doc::reflow("A package outline needs both before it can be published."), color));
        return ExitCode::FAILURE;
    };

    let published = packages::cached_versions(name);
    if published.is_empty() {
        print!("{}", crate::bump::NEW_PACKAGE_OVERVIEW);
        println!("\nI will now verify that everything is in order...\n");
    } else {
        println!("Verifying {name} {version} ...\n");
    }

    match checks(&root, &outline, name, version, &published, color) {
        Ok(()) => {
            println!();
            print!("{}", cannot_register(name, version, color));
            ExitCode::SUCCESS
        }
        Err(report) => {
            eprint!("{report}");
            ExitCode::FAILURE
        }
    }
}

fn checks(
    root: &Path,
    outline: &str,
    name: &str,
    version: Version,
    published: &[Version],
    color: bool,
) -> Result<(), String> {
    if exposed_is_empty(outline) {
        return Err(no_exposed(color));
    }
    if summary_is_bad(outline) {
        return Err(no_summary(color));
    }

    check("Looking for README.md", "Found README.md", color, || {
        let readme = root.join("README.md");
        let Ok(meta) = std::fs::metadata(&readme) else {
            return Err(("Problem with your README.md".to_string(), no_readme(color)));
        };
        if meta.len() < MIN_README_BYTES {
            return Err(("Problem with your README.md".to_string(), short_readme(color)));
        }
        Ok(())
    })?;

    check("Looking for LICENSE", "Found LICENSE", color, || {
        root.join("LICENSE")
            .is_file()
            .then_some(())
            .ok_or_else(|| ("Problem with your LICENSE".to_string(), no_license(color)))
    })?;

    let docs = check_with(
        "Verifying documentation...",
        |_| "Verified documentation".to_string(),
        color,
        || {
            let generated = alm_compiler::project::generate_docs(&root.join("elm.json"))
                .map_err(|errors| {
                    let text: String =
                        errors.iter().map(|e| e.render_from(Some(root), color)).collect();
                    ("Problem with documentation".to_string(), text)
                })?;
            docs_json::parse(&generated, Names::Qualified).ok_or_else(|| {
                ("Problem with documentation".to_string(), corrupt_docs(color))
            })
        },
    )?;

    check_with(
        &format!("Checking semantic versioning rules. Is {version} correct?"),
        |good: &GoodVersion| match good {
            GoodVersion::Start => "All packages start at version 1.0.0".to_string(),
            GoodVersion::Bump(old, magnitude) => format!(
                "Version number {version} verified ({} change, {old} => {version})",
                magnitude.as_str()
            ),
        },
        color,
        || {
            verify_version(name, version, &docs, published, color)
                .map_err(|report| (format!("Version {version} is not correct!"), report))
        },
    )?;

    let tagged = check_with(
        &format!("Is version {version} tagged?"),
        |_| format!("Version {version} is tagged"),
        color,
        || {
            git(root, &["rev-list", "-n", "1", &version.to_string()])
                .map(|hash| hash.trim().to_string())
                .filter(|hash| !hash.is_empty())
                .ok_or_else(|| {
                    (
                        format!("Version {version} is not tagged!"),
                        missing_tag(version, color),
                    )
                })
        },
    )?;

    check(
        "Checking for uncommitted changes...",
        "No uncommitted changes in local code",
        color,
        || {
            let clean = Command::new("git")
                .args(["diff-index", "--quiet", &tagged, "--"])
                .current_dir(root)
                .status()
                .map(|status| status.success())
                .unwrap_or(false);
            clean.then_some(()).ok_or_else(|| {
                (
                    "Your local code is different than the code you tagged".to_string(),
                    local_changes(version, color),
                )
            })
        },
    )?;

    Ok(())
}

enum GoodVersion {
    Start,
    Bump(Version, Magnitude),
}

/// The version has to be a legal next step from a published one, and the step
/// has to match what actually changed.
fn verify_version(
    name: &str,
    version: Version,
    docs: &Documentation,
    published: &[Version],
    color: bool,
) -> Result<GoodVersion, String> {
    let one = Version { major: 1, minor: 0, patch: 0 };
    if published.is_empty() {
        return if version == one {
            Ok(GoodVersion::Start)
        } else {
            Err(not_initial_version(version, color))
        };
    }
    if published.contains(&version) {
        return Err(already_published(version, color));
    }
    let latest = *published.last().expect("published is not empty");
    // Which release this version would be a bump of, and of what size.
    let Some((old, magnitude)) = crate::bump::bumpable_versions(published)
        .into_iter()
        .flat_map(|old| {
            [Magnitude::Major, Magnitude::Minor, Magnitude::Patch]
                .into_iter()
                .map(move |m| (old, m))
        })
        .find(|(old, magnitude)| crate::bump::bump_to(*old, *magnitude) == version)
    else {
        return Err(invalid_bump(version, latest, color));
    };

    let old_docs = published_docs(name, old).ok_or_else(|| cannot_get_docs(name, old, color))?;
    let real = api_diff::diff(&old_docs, docs).magnitude();
    if crate::bump::bump_to(old, real) == version {
        Ok(GoodVersion::Bump(old, magnitude))
    } else {
        Err(bad_bump(old, version, magnitude, crate::bump::bump_to(old, real), real, color))
    }
}

fn published_docs(name: &str, version: Version) -> Option<Documentation> {
    let path = packages::packages_root()
        .join(name.replace('/', std::path::MAIN_SEPARATOR_STR))
        .join(version.to_string())
        .join("docs.json");
    docs_json::parse(&std::fs::read_to_string(path).ok()?, Names::Short)
}

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).current_dir(root).output().ok()?;
    output.status.success().then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

// ------------------------------------------------------------ check reporting

/// elm's progress line: an arrow while the check runs, replaced in place by a
/// tick or a cross. The success text is padded to the width of the waiting
/// text so the carriage return leaves nothing behind.
fn check<T>(
    waiting: &str,
    success: &str,
    color: bool,
    work: impl FnOnce() -> Result<T, (String, String)>,
) -> Result<T, String> {
    check_with(waiting, |_| success.to_string(), color, work)
}

fn check_with<T>(
    waiting: &str,
    success: impl FnOnce(&T) -> String,
    color: bool,
    work: impl FnOnce() -> Result<T, (String, String)>,
) -> Result<T, String> {
    use std::io::Write;
    let mark = |symbol: &str, hue: Color| {
        let doc = Doc::color(hue, Doc::text(symbol));
        if color {
            doc.render_ansi(usize::MAX)
        } else {
            doc.render(usize::MAX)
        }
    };
    let padded = |message: &str| {
        let pad = waiting.chars().count().saturating_sub(message.chars().count());
        format!("{message}{}", " ".repeat(pad))
    };

    print!("  {} {waiting}", mark("→", Color::Yellow));
    let _ = std::io::stdout().flush();
    match work() {
        Ok(value) => {
            println!("\r  {} {}", mark("●", Color::Green), padded(&success(&value)));
            let _ = std::io::stdout().flush();
            Ok(value)
        }
        Err((failure, report)) => {
            println!("\r  {} {}\n", mark("✗", Color::Red), padded(&failure));
            let _ = std::io::stdout().flush();
            Err(report)
        }
    }
}

// ------------------------------------------------------------ outline checks

fn exposed_is_empty(outline: &str) -> bool {
    alm_compiler::project::exposed_modules(outline).is_empty()
}

fn summary_is_bad(outline: &str) -> bool {
    match packages::json_string(outline, "summary") {
        None => true,
        Some(summary) => summary.is_empty() || summary == DEFAULT_SUMMARY,
    }
}

// -------------------------------------------------------------------- reports

fn report(title: &str, before: Doc, after: Doc, color: bool) -> String {
    init::report(title, before, after, color)
}

fn about_outline(title: &str, before: Doc, after: Doc, color: bool) -> String {
    init::report_about("elm.json", title, before, after, color)
}

fn no_exposed(color: bool) -> String {
    about_outline(
        "NO EXPOSED MODULES",
        Doc::reflow(
            "To publish a package, the \"exposed-modules\" field of your elm.json must list at \
             least one module.",
        ),
        Doc::reflow("Which modules should people be able to import?"),
        color,
    )
}

fn no_summary(color: bool) -> String {
    about_outline(
        "NO SUMMARY",
        Doc::reflow(
            "To publish a package, the \"summary\" field of your elm.json must say what the \
             package is for, in fewer than 80 characters.",
        ),
        Doc::reflow("It is the one line people see before they decide to read any further."),
        color,
    )
}

fn no_readme(color: bool) -> String {
    report(
        "NO README",
        Doc::reflow("Every published package needs a README.md, and I could not find one."),
        Doc::reflow(
            "It is the first thing anyone reads about your package: say what it is for and show \
             a small example of using it.",
        ),
        color,
    )
}

fn short_readme(color: bool) -> String {
    report(
        "SHORT README",
        Doc::reflow(&format!(
            "Your README.md is shorter than {MIN_README_BYTES} bytes, which is too short to \
             explain anything."
        )),
        Doc::reflow(
            "Say what the package is for and show a small example of using it. That is usually \
             enough.",
        ),
        color,
    )
}

fn no_license(color: bool) -> String {
    report(
        "NO LICENSE FILE",
        Doc::reflow(
            "Every published package needs a LICENSE file, and I could not find one. Elm packages \
             must be open source.",
        ),
        Doc::reflow(
            "The \"license\" field of your elm.json says which license it should be; \
             <https://choosealicense.com> can give you the text.",
        ),
        color,
    )
}

fn corrupt_docs(color: bool) -> String {
    report(
        "CORRUPT DOCS",
        Doc::reflow("I could not read back the documentation I just generated."),
        Doc::reflow("This is a bug in alm. Please report it."),
        color,
    )
}

fn not_initial_version(version: Version, color: bool) -> String {
    about_outline(
        "INVALID VERSION",
        Doc::reflow(&format!(
            "I cannot publish {version} as the first release. Every package starts at 1.0.0."
        )),
        Doc::reflow("Change the \"version\" in your elm.json to 1.0.0 and try again."),
        color,
    )
}

fn already_published(version: Version, color: bool) -> String {
    about_outline(
        "ALREADY PUBLISHED",
        Doc::reflow(&format!("Version {version} has already been published.")),
        sentence(
            [
                words("Run"),
                vec![cyan("alm bump")],
                words("to work out what the next version should be."),
            ]
            .concat(),
        ),
        color,
    )
}

fn invalid_bump(version: Version, latest: Version, color: bool) -> String {
    about_outline(
        "INVALID VERSION",
        Doc::reflow(&format!(
            "Your elm.json says the next version is {version}, but that is not a version anyone \
             could reach from {latest}."
        )),
        sentence(
            [
                words("Run"),
                vec![cyan("alm bump")],
                words("and I will work out the right number from what actually changed."),
            ]
            .concat(),
        ),
        color,
    )
}

fn bad_bump(
    old: Version,
    claimed: Version,
    claimed_magnitude: Magnitude,
    real: Version,
    real_magnitude: Magnitude,
    color: bool,
) -> String {
    about_outline(
        "INVALID VERSION",
        Doc::reflow(&format!(
            "Your elm.json says the next version is {claimed}, which would be a {} change from \
             {old}. Comparing the APIs, I see a {} change instead, so it should be {real}.",
            claimed_magnitude.as_str(),
            real_magnitude.as_str()
        )),
        sentence(
            [
                words("Run"),
                vec![cyan("alm diff")],
                words("to see exactly what changed, or"),
                vec![cyan("alm bump")],
                words("to set the number for you."),
            ]
            .concat(),
        ),
        color,
    )
}

fn cannot_get_docs(name: &str, version: Version, color: bool) -> String {
    report(
        "CANNOT FIND DOCS",
        Doc::reflow(&format!(
            "I need the docs.json of {name} {version} to check the version number, and it is not \
             in your ~/.elm package cache."
        )),
        Doc::reflow("Fetching that release once will bring its docs along."),
        color,
    )
}

fn missing_tag(version: Version, color: bool) -> String {
    report(
        "NO TAG",
        Doc::reflow(&format!(
            "Packages must be tagged in git, but I cannot find a {version} tag in this repository."
        )),
        Doc::reflow(&format!(
            "These tags make it possible to find this specific version on GitHub. To tag the \
             most recent commit and push it, run:\n\n    git tag -a {version} -m \"new release\"\n    \
             git push origin {version}"
        )),
        color,
    )
}

fn local_changes(version: Version, color: bool) -> String {
    report(
        "LOCAL CHANGES",
        Doc::reflow(&format!(
            "The code tagged as {version} in git does not match the code in this directory."
        )),
        Doc::reflow(
            "Publishing would upload the tagged code, not what is in front of you. Commit your \
             changes, or move the tag, so the two agree.",
        ),
        color,
    )
}

/// Everything alm can check has passed; the upload itself it cannot do.
fn cannot_register(name: &str, version: Version, color: bool) -> String {
    report(
        "READY, BUT NOT PUBLISHED",
        Doc::reflow(&format!(
            "Everything I can check locally about {name} {version} is in order."
        )),
        Doc::reflow(
            "alm never talks to the network, so it cannot register the package or run the three \
             checks that need GitHub: reading the tag's commit hash, downloading the tagged \
             zipball, and compiling that download. Run `elm publish` to finish.",
        ),
        color,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_placeholder_or_missing_summary_is_rejected() {
        assert!(summary_is_bad("{}"));
        assert!(summary_is_bad(r#"{ "summary": "" }"#));
        assert!(summary_is_bad(&format!(r#"{{ "summary": "{DEFAULT_SUMMARY}" }}"#)));
        assert!(!summary_is_bad(r#"{ "summary": "Parse and print URLs" }"#));
    }

    #[test]
    fn a_package_with_nothing_exposed_is_rejected() {
        assert!(exposed_is_empty(r#"{ "exposed-modules": [] }"#));
        assert!(exposed_is_empty(r#"{ "exposed-modules": { "Core Stuff": [] } }"#));
        assert!(!exposed_is_empty(r#"{ "exposed-modules": ["Url"] }"#));
        assert!(!exposed_is_empty(r#"{ "exposed-modules": { "Core Stuff": ["Url"] } }"#));
    }

    /// The success text is padded to the waiting text's width, so the carriage
    /// return cannot leave the tail of a longer line behind.
    #[test]
    fn a_finished_check_covers_the_line_it_replaces() {
        let waiting = "Checking semantic versioning rules. Is 1.1.0 correct?";
        let padded = {
            let message = "Version number 1.1.0 verified (MINOR change, 1.0.0 => 1.1.0)";
            let pad = waiting.chars().count().saturating_sub(message.chars().count());
            format!("{message}{}", " ".repeat(pad))
        };
        assert!(padded.chars().count() >= waiting.chars().count());
    }
}
