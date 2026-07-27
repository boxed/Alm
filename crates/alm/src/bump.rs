//! `alm bump` — port of elm's `Bump.hs`.
//!
//! Sets the version in `elm.json` to what the API changes justify: PATCH if
//! nothing about the API moved, MINOR if things were only added, MAJOR if
//! anything was changed or removed. The comparison is `alm diff`'s, so the
//! same offline rule applies — the release bumped *from* is the one in
//! `~/.elm`, not whatever the registry says is newest.

use std::process::ExitCode;

use alm_compiler::api_diff::{self, Magnitude};
use alm_compiler::docs_json::{self, Documentation, Names};
use alm_compiler::packages::{self, Version};
use alm_compiler::reporting::doc::{Color, Doc};
use alm_compiler::reporting::{sentence, words, yellow};

use crate::init;

/// What elm prints for a package that has never been published.
const NEW_PACKAGE_OVERVIEW: &str = "\
This package has never been published before. Here's how things work:

  - Versions all have exactly three parts: MAJOR.MINOR.PATCH

  - All packages start with initial version 1.0.0

  - Versions are incremented based on how the API changes:

        PATCH = the API is the same, no risk of breaking code
        MINOR = values have been added, existing values are unchanged
        MAJOR = existing values have been changed or removed

  - I will bump versions for you, automatically enforcing these rules
";

pub fn run(color: bool) -> ExitCode {
    let root = alm_compiler::project::project_root(std::path::Path::new("elm.json"));
    let Ok(outline) = std::fs::read_to_string(root.join("elm.json")) else {
        eprint!("{}", no_outline(color));
        return ExitCode::FAILURE;
    };
    if packages::json_string(&outline, "type") != Some("package") {
        eprint!("{}", application(color));
        return ExitCode::FAILURE;
    }
    let (Some(name), Some(version)) = (
        packages::json_string(&outline, "name"),
        packages::json_string(&outline, "version").and_then(Version::parse),
    ) else {
        eprint!("{}", corrupt_outline(color));
        return ExitCode::FAILURE;
    };

    let published = packages::cached_versions(name);
    if published.is_empty() {
        return new_package(&root, &outline, version);
    }

    // elm only bumps from a version someone could actually be depending on:
    // the newest release, or the last release in some major or minor line.
    let bumpable = bumpable_versions(&published);
    if !bumpable.contains(&version) {
        eprint!("{}", cannot_bump(version, &bumpable, color));
        return ExitCode::FAILURE;
    }

    let old_docs = match published_docs(name, version, color) {
        Ok(docs) => docs,
        Err(report) => {
            eprint!("{report}");
            return ExitCode::FAILURE;
        }
    };
    let new_docs = match local_docs(&root, color) {
        Ok(docs) => docs,
        Err(report) => {
            eprint!("{report}");
            return ExitCode::FAILURE;
        }
    };

    let changes = api_diff::diff(&old_docs, &new_docs);
    let magnitude = changes.magnitude();
    let bumped = bump_to(version, magnitude);
    change_version(&root, &outline, bumped, suggestion(version, bumped, magnitude, color))
}

/// Nothing published: say how versions work, and make sure the outline starts
/// at 1.0.0.
fn new_package(root: &std::path::Path, outline: &str, version: Version) -> ExitCode {
    print!("{NEW_PACKAGE_OVERVIEW}");
    let one = Version { major: 1, minor: 0, patch: 0 };
    if version == one {
        println!("The version number in elm.json is correct so you are all set!");
        return ExitCode::SUCCESS;
    }
    change_version(
        root,
        outline,
        one,
        format!(
            "It looks like the version in elm.json has been changed though!\n\
             Would you like me to change it back to {one}? [Y/n] "
        ),
    )
}

/// The versions elm will bump *from*: the newest release, plus the last
/// release of each major line (a minor bump) and of each minor line (a patch
/// bump). Anything else is a version nobody can be depending on.
fn bumpable_versions(published: &[Version]) -> Vec<Version> {
    let last_of = |same: fn(&Version, &Version) -> bool| -> Vec<Version> {
        let mut out: Vec<Version> = Vec::new();
        for version in published {
            match out.last() {
                Some(previous) if same(previous, version) => {
                    *out.last_mut().unwrap() = *version;
                }
                _ => out.push(*version),
            }
        }
        out
    };
    let mut all = vec![*published.last().expect("published is not empty")];
    all.extend(last_of(|a, b| a.major == b.major));
    all.extend(last_of(|a, b| a.major == b.major && a.minor == b.minor));
    all.sort();
    all.dedup();
    all
}

fn bump_to(version: Version, magnitude: Magnitude) -> Version {
    match magnitude {
        Magnitude::Major => Version { major: version.major + 1, minor: 0, patch: 0 },
        Magnitude::Minor => Version { minor: version.minor + 1, patch: 0, ..version },
        Magnitude::Patch => Version { patch: version.patch + 1, ..version },
    }
}

fn suggestion(old: Version, new: Version, magnitude: Magnitude, color: bool) -> String {
    let line = Doc::concat(vec![
        Doc::text("Based on your new API, this should be a "),
        Doc::color(Color::Green, Doc::text(magnitude.as_str())),
        Doc::text(format!(" change ({old} => {new})")),
    ]);
    let head = if color { line.render_ansi(80) } else { line.render(80) };
    format!(
        "{head}\n\
         Bail out of this command and run 'alm diff' for a full explanation.\n\
         \n\
         Should I perform the update ({old} => {new}) in elm.json? [Y/n] "
    )
}

/// Ask, then rewrite just the `"version"` field — an elm.json may carry fields
/// alm does not model, so the rest of the file is left byte for byte.
fn change_version(
    root: &std::path::Path,
    outline: &str,
    target: Version,
    question: String,
) -> ExitCode {
    use std::io::Write;
    print!("{question}");
    let _ = std::io::stdout().flush();
    if !init::approved() {
        println!("Okay, I did not change anything!");
        return ExitCode::SUCCESS;
    }
    let Some(updated) = replace_version(outline, target) else {
        eprintln!("I could not find the \"version\" field in your elm.json.");
        return ExitCode::FAILURE;
    };
    if let Err(err) = std::fs::write(root.join("elm.json"), updated) {
        eprintln!("I could not write elm.json: {err}");
        return ExitCode::FAILURE;
    }
    let done = Doc::concat(vec![
        Doc::text("Version changed to "),
        Doc::color(Color::Green, Doc::text(target.to_string())),
        Doc::text("!"),
    ]);
    println!("{}", if crate::use_color() { done.render_ansi(80) } else { done.render(80) });
    ExitCode::SUCCESS
}

fn replace_version(outline: &str, target: Version) -> Option<String> {
    let at = outline.find("\"version\"")?;
    let rest = &outline[at..];
    let colon = rest.find(':')?;
    let open = rest[colon..].find('"')? + colon;
    let close = rest[open + 1..].find('"')? + open + 1;
    Some(format!("{}{target}{}", &outline[..at + open + 1], &outline[at + close..]))
}

fn published_docs(name: &str, version: Version, color: bool) -> Result<Documentation, String> {
    let path = packages::packages_root()
        .join(name.replace('/', std::path::MAIN_SEPARATOR_STR))
        .join(version.to_string())
        .join("docs.json");
    let text = std::fs::read_to_string(&path).map_err(|_| missing_docs(name, version, color))?;
    docs_json::parse(&text, Names::Short).ok_or_else(|| missing_docs(name, version, color))
}

fn local_docs(root: &std::path::Path, color: bool) -> Result<Documentation, String> {
    let generated = alm_compiler::project::generate_docs(&root.join("elm.json"))
        .map_err(|errors| -> String {
            errors.iter().map(|e| e.render_from(Some(root), color)).collect()
        })?;
    // Generated docs keep their qualifiers; see `docs_json::Names`.
    docs_json::parse(&generated, Names::Qualified).ok_or_else(|| corrupt_local_docs(color))
}

// -------------------------------------------------------------------- reports

fn no_outline(color: bool) -> String {
    init::report(
        "BUMP WHAT?",
        Doc::reflow("I cannot find an elm.json so I am not sure what you want me to bump."),
        Doc::reflow(
            "Elm packages always have an elm.json that says current the version number. If you \
             run this command from a directory with an elm.json file, I will try to bump the \
             version in there based on the API changes.",
        ),
        color,
    )
}

fn application(color: bool) -> String {
    init::report_about(
        "elm.json",
        "CANNOT BUMP APPLICATIONS",
        Doc::reflow(
            "Your elm.json says this is an application. That means it cannot be published on \
             <https://package.elm-lang.org> and therefore has no version to bump!",
        ),
        Doc::Empty,
        color,
    )
}

fn corrupt_outline(color: bool) -> String {
    init::report(
        "CORRUPT elm.json",
        Doc::reflow("I could not find a \"name\" and a \"version\" in your elm.json file."),
        Doc::reflow("A package outline needs both to know what it would be bumping."),
        color,
    )
}

/// elm points at the registry here; alm points at the cache, since that is
/// where its idea of "published" comes from.
fn cannot_bump(version: Version, bumpable: &[Version], color: bool) -> String {
    let listed = Doc::vcat(
        bumpable.iter().map(|v| Doc::color(Color::Green, Doc::text(v.to_string()))).collect(),
    );
    init::report_about(
        "elm.json",
        "CANNOT BUMP",
        sentence(
            [
                words("Your elm.json says I should bump relative to version"),
                vec![Doc::cat2(Doc::color(Color::RedVivid, Doc::text(version.to_string())),
                     Doc::text(","))],
                words(
                    "but I cannot find that version in your ~/.elm package cache. That means \
                     there is no API for me to diff against and figure out if these are MAJOR, \
                     MINOR, or PATCH changes.",
                ),
            ]
            .concat(),
        ),
        Doc::vcat(vec![
            sentence(
                [
                    words("Try bumping again after changing the"),
                    vec![yellow("\"version\"")],
                    words("in elm.json"),
                    words(if bumpable.len() == 1 { "to:" } else { "to one of these:" }),
                ]
                .concat(),
            ),
            Doc::Empty,
            listed,
        ]),
        color,
    )
}

fn missing_docs(name: &str, version: Version, color: bool) -> String {
    init::report(
        "CANNOT FIND DOCS",
        Doc::reflow(&format!(
            "I could not read the docs.json of {name} {version} out of your ~/.elm package cache, \
             so I have nothing to compare your API against."
        )),
        Doc::reflow(
            "Fetching that release again will bring its docs along, and then bumping can work \
             out what changed.",
        ),
        color,
    )
}

fn corrupt_local_docs(color: bool) -> String {
    init::report(
        "CORRUPT DOCS",
        Doc::reflow("I could not read back the documentation I just generated."),
        Doc::reflow("This is a bug in alm. Please report it."),
        color,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    /// Only the tip of each release line can be bumped from: the newest
    /// version overall, the last of each major, and the last of each minor.
    #[test]
    fn bumpable_versions_are_the_tips_of_each_line() {
        let published: Vec<Version> =
            ["1.0.0", "1.0.1", "1.1.0", "1.1.1", "2.0.0"].iter().map(|s| v(s)).collect();
        assert_eq!(
            bumpable_versions(&published),
            vec![v("1.0.1"), v("1.1.1"), v("2.0.0")]
        );
        assert_eq!(bumpable_versions(&[v("1.0.0")]), vec![v("1.0.0")]);
    }

    #[test]
    fn a_bump_moves_the_part_the_magnitude_names() {
        assert_eq!(bump_to(v("1.2.3"), Magnitude::Patch), v("1.2.4"));
        assert_eq!(bump_to(v("1.2.3"), Magnitude::Minor), v("1.3.0"));
        assert_eq!(bump_to(v("1.2.3"), Magnitude::Major), v("2.0.0"));
    }

    #[test]
    fn the_suggestion_reads_like_elms() {
        assert_eq!(
            suggestion(v("1.0.0"), v("1.1.0"), Magnitude::Minor, false),
            "Based on your new API, this should be a MINOR change (1.0.0 => 1.1.0)\n\
             Bail out of this command and run 'alm diff' for a full explanation.\n\
             \n\
             Should I perform the update (1.0.0 => 1.1.0) in elm.json? [Y/n] "
        );
    }

    /// Only the version changes; every other byte of the outline survives.
    #[test]
    fn the_version_field_is_replaced_in_place() {
        let outline = "{\n    \"type\": \"package\",\n    \"version\": \"1.0.0\",\n    \
                       \"summary\": \"version 1.0.0 of it\"\n}\n";
        assert_eq!(
            replace_version(outline, v("2.0.0")).unwrap(),
            "{\n    \"type\": \"package\",\n    \"version\": \"2.0.0\",\n    \
             \"summary\": \"version 1.0.0 of it\"\n}\n"
        );
    }
}
