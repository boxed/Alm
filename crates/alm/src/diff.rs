//! `alm diff` — port of elm's `Diff.hs`.
//!
//! Compares two versions of a package's public API and says what it costs in
//! semantic versioning terms. Four forms, the same as elm's:
//!
//! ```text
//! alm diff                              # this package's code vs. its latest release
//! alm diff <version>                    # this package's code vs. that release
//! alm diff <version> <version>          # two releases of this package
//! alm diff <package> <version> <ver>    # two releases of some other package
//! ```
//!
//! elm consults the package registry to learn which versions exist; alm looks
//! at `~/.elm`, so "latest" means the newest version on this machine. The
//! comparison itself reads each version's published `docs.json`, which is
//! already in the cache, so nothing is downloaded.

use std::process::ExitCode;

use alm_compiler::api_diff::{self, Changes, Magnitude, ModuleChanges, PackageChanges};
use alm_compiler::docs_json::{self, Alias, Binop, Documentation, Names, Type, Union, Value};
use alm_compiler::packages::{self, Version};
use alm_compiler::reporting::doc::{Color, Doc};
use alm_compiler::reporting::render_type::Ctx;
use alm_compiler::reporting::{cyan, sentence, words};

use crate::init;

pub fn run(args: &[String], color: bool) -> ExitCode {
    let parsed = match args {
        [] => Args::CodeVsLatest,
        [one] => match Version::parse(one) {
            Some(version) => Args::CodeVsExactly(version),
            None => return bad_argument(one, color),
        },
        [a, b] => match (Version::parse(a), Version::parse(b)) {
            (Some(v1), Some(v2)) => Args::LocalInquiry(v1, v2),
            _ => return bad_argument(if Version::parse(a).is_none() { a } else { b }, color),
        },
        [name, a, b] => match (Version::parse(a), Version::parse(b)) {
            (Some(v1), Some(v2)) => Args::GlobalInquiry(name.clone(), v1, v2),
            _ => return bad_argument(if Version::parse(a).is_none() { a } else { b }, color),
        },
        _ => return too_many_arguments(color),
    };
    match load(parsed, color) {
        Ok((old, new)) => {
            let changes = api_diff::diff(&old, &new);
            let rendered = to_doc(&changes);
            print!("{}\n", if color { rendered.render_ansi(80) } else { rendered.render(80) });
            ExitCode::SUCCESS
        }
        Err(report) => {
            eprint!("{report}");
            ExitCode::FAILURE
        }
    }
}

enum Args {
    CodeVsLatest,
    CodeVsExactly(Version),
    LocalInquiry(Version, Version),
    GlobalInquiry(String, Version, Version),
}

/// The two documentations to compare, oldest first. Explicit version pairs are
/// sorted rather than taken in the order given, matching elm: `diff 2.0.0
/// 1.0.0` still reports what 2.0.0 added.
fn load(args: Args, color: bool) -> Result<(Documentation, Documentation), String> {
    match args {
        Args::GlobalInquiry(name, v1, v2) => {
            Ok((published(&name, v1.min(v2), color)?, published(&name, v1.max(v2), color)?))
        }
        Args::LocalInquiry(v1, v2) => {
            let name = local_package(color)?;
            Ok((published(&name, v1.min(v2), color)?, published(&name, v1.max(v2), color)?))
        }
        Args::CodeVsExactly(version) => {
            let name = local_package(color)?;
            Ok((published(&name, version, color)?, local_docs(color)?))
        }
        Args::CodeVsLatest => {
            let name = local_package(color)?;
            let Some(latest) = packages::cached_versions(&name).pop() else {
                return Err(unpublished(&name, color));
            };
            Ok((published(&name, latest, color)?, local_docs(color)?))
        }
    }
}

/// A published version's `docs.json`, straight out of the cache.
fn published(name: &str, version: Version, color: bool) -> Result<Documentation, String> {
    let known = packages::cached_versions(name);
    if known.is_empty() {
        return Err(unknown_package(name, color));
    }
    if !known.contains(&version) {
        return Err(unknown_version(name, version, &known, color));
    }
    let path = packages::packages_root()
        .join(name.replace('/', std::path::MAIN_SEPARATOR_STR))
        .join(version.to_string())
        .join("docs.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Err(no_docs(name, version, color));
    };
    docs_json::parse(&text, Names::Short).ok_or_else(|| corrupt_docs(name, version, color))
}

/// The API of the package in the current directory, compiled right now.
fn local_docs(color: bool) -> Result<Documentation, String> {
    let root = outline_root(color)?;
    let generated = alm_compiler::project::generate_docs(&root.join("elm.json"))
        .map_err(|errors| build_failed(&errors, &root, color))?;
    // Generated docs keep their qualifiers: elm never runs them through the
    // string form, so `alm diff` with no version prints the new code's types
    // as `String.String` and the release it is compared against as `String`.
    docs_json::parse(&generated, Names::Qualified).ok_or_else(|| corrupt_local_docs(color))
}

/// The directory holding the project's `elm.json`. `project_root` falls back
/// to the current directory when there is none, so the file is checked for
/// rather than trusted.
fn outline_root(color: bool) -> Result<std::path::PathBuf, String> {
    let root = alm_compiler::project::project_root(std::path::Path::new("elm.json"));
    if root.join("elm.json").is_file() {
        Ok(root)
    } else {
        Err(no_outline(color))
    }
}

/// The `"name"` an `elm.json` declares, which only a package outline has.
fn local_package(color: bool) -> Result<String, String> {
    let root = outline_root(color)?;
    let outline =
        std::fs::read_to_string(root.join("elm.json")).map_err(|_| no_outline(color))?;
    if packages::json_string(&outline, "type") != Some("package") {
        return Err(application(color));
    }
    packages::json_string(&outline, "name")
        .map(str::to_string)
        .ok_or_else(|| corrupt_outline(color))
}

// -------------------------------------------------------------------- to a doc

/// `Diff.toDoc`.
fn to_doc(changes: &PackageChanges) -> Doc {
    if changes.is_empty() {
        return sentence(
            [
                words("No API changes detected, so this is a"),
                vec![Doc::color(Color::Green, Doc::text("PATCH"))],
                words("change."),
            ]
            .concat(),
        );
    }
    let header = Doc::concat(vec![
        Doc::text("This is a "),
        Doc::color(Color::Green, Doc::text(changes.magnitude().as_str())),
        Doc::text(" change."),
    ]);
    let mut chunks = Vec::new();
    if !changes.added.is_empty() {
        chunks.push(chunk(
            "ADDED MODULES",
            Magnitude::Minor,
            Doc::vcat(changes.added.iter().map(Doc::text).collect()),
        ));
    }
    if !changes.removed.is_empty() {
        chunks.push(chunk(
            "REMOVED MODULES",
            Magnitude::Major,
            Doc::vcat(changes.removed.iter().map(Doc::text).collect()),
        ));
    }
    for (name, module) in &changes.changed {
        chunks.push(chunk(name, module.magnitude(), module_details(module)));
    }
    let mut parts = vec![header, Doc::Empty];
    parts.extend(chunks);
    Doc::vcat(parts)
}

/// One `---- Title - MAGNITUDE ----` section. The two blank lines at the end
/// are part of the section, so sections are separated whether or not another
/// follows.
fn chunk(title: &str, magnitude: Magnitude, details: Doc) -> Doc {
    let bar = Doc::text(format!("---- {title} - {} ----", magnitude.as_str()));
    Doc::vcat(vec![
        Doc::color(Color::Cyan, bar),
        Doc::Empty,
        Doc::indent(4, details),
        Doc::Empty,
        Doc::Empty,
    ])
}

fn module_details(module: &ModuleChanges) -> Doc {
    let sections: Vec<Doc> = [
        section(module, Side::Added),
        section(module, Side::Removed),
        changed_section(module),
    ]
    .into_iter()
    .flatten()
    .collect();
    let mut parts = Vec::new();
    for (i, s) in sections.into_iter().enumerate() {
        if i > 0 {
            parts.push(Doc::Empty);
        }
        parts.push(s);
    }
    Doc::vcat(parts)
}

#[derive(Clone, Copy)]
enum Side {
    Added,
    Removed,
}

impl Side {
    fn label(self) -> &'static str {
        match self {
            Side::Added => "Added",
            Side::Removed => "Removed",
        }
    }

    fn of<T>(self, changes: &Changes<T>) -> &std::collections::BTreeMap<String, T> {
        match self {
            Side::Added => &changes.added,
            Side::Removed => &changes.removed,
        }
    }
}

/// `Added:` and `Removed:` list whole entries, in elm's order: unions,
/// aliases, binops, then values.
fn section(module: &ModuleChanges, side: Side) -> Option<Doc> {
    let mut entries = Vec::new();
    entries.extend(side.of(&module.unions).iter().map(|(n, u)| union_doc(n, u)));
    entries.extend(side.of(&module.aliases).iter().map(|(n, a)| alias_doc(n, a)));
    entries.extend(side.of(&module.binops).iter().map(|(n, b)| binop_doc(n, b)));
    entries.extend(side.of(&module.values).iter().map(|(n, v)| value_doc(n, v)));
    if entries.is_empty() {
        return None;
    }
    let mut parts = vec![Doc::text(format!("{}:", side.label()))];
    parts.extend(entries.into_iter().map(|e| Doc::indent(4, e)));
    Some(Doc::vcat(parts))
}

/// `Changed:` shows each entry twice, old then new, with a blank line after.
fn changed_section(module: &ModuleChanges) -> Option<Doc> {
    let mut entries = Vec::new();
    let diffed = |old: Doc, new: Doc| {
        Doc::vcat(vec![
            Doc::cat2(Doc::text("  - "), old),
            Doc::cat2(Doc::text("  + "), new),
            Doc::Empty,
        ])
    };
    for (n, (old, new)) in &module.unions.changed {
        entries.push(diffed(union_doc(n, old), union_doc(n, new)));
    }
    for (n, (old, new)) in &module.aliases.changed {
        entries.push(diffed(alias_doc(n, old), alias_doc(n, new)));
    }
    for (n, (old, new)) in &module.binops.changed {
        entries.push(diffed(binop_doc(n, old), binop_doc(n, new)));
    }
    for (n, (old, new)) in &module.values.changed {
        entries.push(diffed(value_doc(n, old), value_doc(n, new)));
    }
    if entries.is_empty() {
        return None;
    }
    let mut parts = vec![Doc::text("Changed:")];
    parts.extend(entries);
    Some(Doc::vcat(parts))
}

/// `type Name a b = A | B x`. With no arguments the trailing `<+>` still emits
/// a space, so elm prints `type Body ` — kept, since these are compared
/// byte-for-byte against elm's output.
fn union_doc(name: &str, union: &Union) -> Doc {
    let setup = Doc::space(
        Doc::space(Doc::text("type"), Doc::text(name)),
        hsep(union.args.iter().map(Doc::text).collect()),
    );
    let mut parts = vec![setup];
    for (i, (ctor, args)) in union.cases.iter().enumerate() {
        let applied = Type::Type(ctor.clone(), args.clone());
        parts.push(Doc::space(
            Doc::text(if i == 0 { "=" } else { "|" }),
            docs_json::to_doc(Ctx::None, &applied),
        ));
    }
    Doc::hang(4, Doc::sep(parts))
}

fn alias_doc(name: &str, alias: &Alias) -> Doc {
    let head = std::iter::once(Doc::text(name))
        .chain(alias.args.iter().map(Doc::text))
        .collect::<Vec<_>>();
    let declaration = Doc::space(
        Doc::space(Doc::space(Doc::text("type"), Doc::text("alias")), hsep(head)),
        Doc::text("="),
    );
    Doc::hang(4, Doc::sep(vec![declaration, docs_json::to_doc(Ctx::None, &alias.tipe)]))
}

fn value_doc(name: &str, value: &Value) -> Doc {
    Doc::hang(
        4,
        Doc::sep(vec![
            Doc::space(Doc::text(name), Doc::text(":")),
            docs_json::to_doc(Ctx::None, &value.tipe),
        ]),
    )
}

/// `(<|) : (a -> b) -> a -> b    (right/0)` — the fixity trails in black,
/// since it is a detail rather than part of the signature.
fn binop_doc(name: &str, binop: &Binop) -> Doc {
    let details = Doc::color(
        Color::Black,
        Doc::text(format!("    ({}/{})", binop.associativity, binop.precedence)),
    );
    Doc::cat2(
        Doc::space(
            Doc::space(Doc::text(format!("({name})")), Doc::text(":")),
            docs_json::to_doc(Ctx::None, &binop.tipe),
        ),
        details,
    )
}

/// `D.hsep` — space separated, never broken. Empty for no parts, which is how
/// a type with no arguments still ends up with a trailing space.
fn hsep(docs: Vec<Doc>) -> Doc {
    let mut it = docs.into_iter();
    let Some(first) = it.next() else { return Doc::Empty };
    it.fold(first, Doc::space)
}

// -------------------------------------------------------------------- reports

fn bad_argument(argument: &str, color: bool) -> ExitCode {
    eprint!(
        "{}",
        init::report(
            "BAD ARGUMENT",
            Doc::reflow(&format!(
                "I was expecting a version number like 1.0.0, but I got {argument} instead."
            )),
            Doc::reflow(
                "Run `alm diff` with no arguments to compare this package against its latest \
                 release, or give it one or two versions to compare.",
            ),
            color,
        )
    );
    ExitCode::FAILURE
}

fn too_many_arguments(color: bool) -> ExitCode {
    eprint!(
        "{}",
        init::report(
            "TOO MANY ARGUMENTS",
            Doc::reflow("I got more arguments than I know what to do with."),
            Doc::reflow(
                "`alm diff` takes nothing, one version, two versions, or a package name and two \
                 versions.",
            ),
            color,
        )
    );
    ExitCode::FAILURE
}

fn no_outline(color: bool) -> String {
    init::report(
        "DIFF WHAT?",
        Doc::reflow("I cannot find an elm.json so I am not sure what you want me to diff."),
        sentence(
            [
                words("Try again from an Elm package directory, or run"),
                vec![cyan("alm diff <package> <version> <version>")],
                words("to compare two releases of some other package."),
            ]
            .concat(),
        ),
        color,
    )
}

fn application(color: bool) -> String {
    init::report(
        "CANNOT DIFF APPLICATIONS",
        Doc::reflow(
            "Only packages have a public API to compare, and this elm.json describes an \
             application.",
        ),
        sentence(
            [
                words("You can still compare two releases of a package with"),
                vec![cyan("alm diff <package> <version> <version>")],
                words("from here."),
            ]
            .concat(),
        ),
        color,
    )
}

fn corrupt_outline(color: bool) -> String {
    init::report(
        "CORRUPT elm.json",
        Doc::reflow("I could not find a \"name\" field in your elm.json file."),
        Doc::reflow("A package outline needs one, in the form \"author/project\"."),
        color,
    )
}

fn unknown_package(name: &str, color: bool) -> String {
    init::report(
        "UNKNOWN PACKAGE",
        Doc::reflow(&format!("I could not find {name} in your ~/.elm package cache.")),
        Doc::reflow(
            "alm never downloads anything, so it can only diff packages already on this machine. \
             Running `elm install` for it once will fetch it.",
        ),
        color,
    )
}

fn unknown_version(name: &str, version: Version, known: &[Version], color: bool) -> String {
    let listed: Vec<String> = known.iter().rev().take(6).map(Version::to_string).collect();
    init::report(
        "UNKNOWN VERSION",
        Doc::reflow(&format!("I cannot find {name} {version} in your ~/.elm package cache.")),
        Doc::reflow(&format!(
            "These versions are cached: {}. alm can only diff versions already on this machine.",
            listed.join(", ")
        )),
        color,
    )
}

fn unpublished(name: &str, color: bool) -> String {
    init::report(
        "UNPUBLISHED",
        Doc::reflow(&format!(
            "There is no cached release of {name}, so I have nothing to compare your code against."
        )),
        Doc::reflow(
            "Give me two versions to compare instead, or fetch a release into ~/.elm first.",
        ),
        color,
    )
}

fn no_docs(name: &str, version: Version, color: bool) -> String {
    init::report(
        "MISSING DOCS",
        Doc::reflow(&format!(
            "The cached copy of {name} {version} has no docs.json, so I cannot see its API."
        )),
        Doc::reflow(
            "That happens when a package was downloaded as a dependency rather than published. \
             Removing it from ~/.elm and fetching it again will bring the docs along.",
        ),
        color,
    )
}

fn corrupt_docs(name: &str, version: Version, color: bool) -> String {
    init::report(
        "CORRUPT DOCS",
        Doc::reflow(&format!("I could not read the docs.json of {name} {version}.")),
        Doc::reflow("Deleting it from ~/.elm and fetching the package again should fix it."),
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

/// The build's own reports say what went wrong far better than a summary
/// could, so they are passed straight through.
fn build_failed(
    errors: &[alm_compiler::project::BuildError],
    root: &std::path::Path,
    color: bool,
) -> String {
    errors.iter().map(|e| e.render_from(Some(root), color)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendered(changes: &PackageChanges) -> String {
        format!("{}\n", to_doc(changes).render(80))
    }

    /// Each `tests/diffs/<name>` holds two published `docs.json` and the exact
    /// stdout `elm diff` produced for them (the command is recorded in
    /// `source.txt`). Published docs are read short on both sides, which is
    /// what elm does when neither side was generated from source.
    #[test]
    fn diffs_match_elm() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/diffs");
        let mut fixtures: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_dir())
            .collect();
        fixtures.sort();
        assert!(!fixtures.is_empty(), "no fixtures in {}", dir.display());

        let mut wrong = Vec::new();
        for fixture in &fixtures {
            let name = fixture.file_name().unwrap().to_string_lossy().to_string();
            let read = |file: &str| {
                let text = std::fs::read_to_string(fixture.join(file)).unwrap();
                docs_json::parse(&text, Names::Short)
                    .unwrap_or_else(|| panic!("{name}/{file} did not parse"))
            };
            let expected = std::fs::read_to_string(fixture.join("expected.txt")).unwrap();
            let got = rendered(&api_diff::diff(&read("old.json"), &read("new.json")));
            if got != expected {
                wrong.push(format!(
                    "\n=== {name} ===\n--- elm ---\n{expected}--- alm ---\n{got}"
                ));
            }
        }
        assert!(wrong.is_empty(), "{} fixture(s) differ:{}", wrong.len(), wrong.join(""));
    }

    /// No cached package pair changes a binop, so the one shape the fixtures
    /// cannot reach is pinned here against elm's `binopToDoc`: the signature,
    /// then four spaces and the fixity in parentheses.
    #[test]
    fn a_binop_carries_its_fixity() {
        let doc = binop_doc(
            "|>",
            &Binop {
                comment: String::new(),
                tipe: docs_json::parse_type("a -> (a -> b) -> b").unwrap(),
                associativity: "left".to_string(),
                precedence: 0,
            },
        );
        assert_eq!(doc.render(80), "(|>) : a -> (a -> b) -> b    (left/0)");
    }

    /// A type with no arguments still gets the space its `<+>` puts there, so
    /// elm prints `type Body ` and `type Root  = …`. It looks like a slip and
    /// is not one.
    #[test]
    fn a_union_with_no_arguments_keeps_elms_trailing_space() {
        let union = |cases: Vec<(&str, Vec<&str>)>| Union {
            comment: String::new(),
            args: Vec::new(),
            cases: cases
                .into_iter()
                .map(|(n, args)| {
                    (
                        n.to_string(),
                        args.iter().map(|a| docs_json::parse_type(a).unwrap()).collect(),
                    )
                })
                .collect(),
        };
        assert_eq!(union_doc("Body", &union(vec![])).render(80), "type Body ");
        assert_eq!(
            union_doc("Root", &union(vec![("Absolute", vec![]), ("Deep", vec!["String.String"])]))
                .render(80),
            "type Root  = Absolute | Deep String"
        );
    }
}
