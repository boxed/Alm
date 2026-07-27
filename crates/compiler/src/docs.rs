//! Port of `Elm.Docs`: a package's `docs.json`.
//!
//! `alm make --docs=docs.json` on a package project writes the documentation
//! the package website renders and `elm diff`/`elm bump` compare. The format is
//! an array of modules, each listing its unions, aliases, values and binops
//! with their doc comments and their types — and the types are written with
//! every name qualified by the module that *defines* it (`Basics.Int`,
//! `String.String`, `Probe.Point`), not as the source wrote them.
//!
//! Only entries named in the module's `@docs` lines appear, in the order the
//! format sorts them (by name within each kind).

use std::collections::BTreeMap;

use crate::ast::canonical as can;
use crate::data::Name;
use crate::interface::Interface;
use crate::reporting::{json_str, Region};

/// A `{-| … -}` comment and where it sits in the file.
#[derive(Debug, Clone)]
pub struct DocComment {
    /// The text between `{-|` and `-}`, verbatim.
    pub text: String,
    /// 1-based line the `{-|` opens on.
    pub start_row: u32,
    /// 1-based line the `-}` closes on.
    pub end_row: u32,
}

/// Find every `{-| … -}` in a source file.
///
/// The parser discards comments, so they are recovered here. Elm attaches a
/// doc comment positionally — the module's is the first one after the header,
/// and a declaration's is the one directly above it — so position is all that
/// needs recording. Nested `{- -}` and comments inside string literals are
/// handled, since either would otherwise end the scan in the wrong place.
pub fn scan_doc_comments(source: &str) -> Vec<DocComment> {
    scan_comments(source).0
}

/// Doc comments, plus the row span of *every* comment — a line inside a
/// comment must not be mistaken for a declaration, and elm's own sources are
/// full of prose that starts with a value's name.
pub fn scan_comments(source: &str) -> (Vec<DocComment>, Vec<(u32, u32)>) {
    let bytes = source.as_bytes();
    let mut found = Vec::new();
    let mut spans = Vec::new();
    let mut i = 0usize;
    let mut row = 1u32;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => {
                row += 1;
                i += 1;
            }
            // Skip string and character literals whole: a `{-` inside one is
            // not a comment.
            b'"' if bytes[i..].starts_with(b"\"\"\"") => {
                i += 3;
                while i < bytes.len() && !bytes[i..].starts_with(b"\"\"\"") {
                    if bytes[i] == b'\n' {
                        row += 1;
                    }
                    i += if bytes[i] == b'\\' { 2 } else { 1 };
                }
                i = (i + 3).min(bytes.len());
            }
            b'"' | b'\'' => {
                let quote = bytes[i];
                i += 1;
                while i < bytes.len() && bytes[i] != quote {
                    if bytes[i] == b'\n' {
                        row += 1;
                        break;
                    }
                    i += if bytes[i] == b'\\' { 2 } else { 1 };
                }
                i += 1;
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                spans.push((row, row));
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'{' if bytes.get(i + 1) == Some(&b'-') => {
                let is_doc = bytes.get(i + 2) == Some(&b'|');
                let start_row = row;
                let body_start = i + if is_doc { 3 } else { 2 };
                i = body_start;
                let mut depth = 1;
                while i < bytes.len() && depth > 0 {
                    if bytes[i] == b'\n' {
                        row += 1;
                        i += 1;
                    } else if bytes[i..].starts_with(b"{-") {
                        depth += 1;
                        i += 2;
                    } else if bytes[i..].starts_with(b"-}") {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                spans.push((start_row, row));
                if is_doc {
                    let end = i.saturating_sub(2).max(body_start);
                    found.push(DocComment {
                        text: source[body_start..end].to_string(),
                        start_row,
                        end_row: row,
                    });
                }
            }
            _ => i += 1,
        }
    }
    (found, spans)
}

/// Whether `row` falls inside any comment.
fn in_comment(spans: &[(u32, u32)], row: u32) -> bool {
    spans.iter().any(|(start, end)| *start <= row && row <= *end)
}

/// The names listed on this module's `@docs` lines, in source order.
///
/// A module comment may carry several `@docs` lines, and each lists
/// comma-separated names; `(..)` after a union name is stripped, since the
/// format records the constructors separately.
pub fn docs_order(module_comment: &str) -> Vec<String> {
    let lines: Vec<&str> = module_comment.lines().collect();
    let mut names = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let Some(first) = lines[i].trim_start().strip_prefix("@docs") else {
            i += 1;
            continue;
        };
        // A list may wrap: it continues while the text so far ends in a comma.
        let mut listed = first.trim().to_string();
        while listed.ends_with(',') && i + 1 < lines.len() {
            i += 1;
            listed.push(' ');
            listed.push_str(lines[i].trim());
        }
        for entry in listed.split(',') {
            let name = entry.trim().trim_end_matches("(..)").trim();
            if !name.is_empty() {
                names.push(name.to_string());
            }
        }
        i += 1;
    }
    names
}

/// Render a canonical type the way `docs.json` writes it: every name qualified
/// by the module that defines it, and parentheses only where they are needed.
pub fn render_type(tipe: &can::Type) -> String {
    render(tipe, Ctx::None, &AliasNames::default())
}

/// Render for a person rather than for `docs.json`: a type whose home is in
/// `unqualified` prints as `String`, anything else keeps its `Module.Name`.
///
/// This is elm's localizer. Which spelling you get depends on the imports in
/// scope — `import Dict` alone gives `Dict.Dict`, because nothing brought the
/// bare name in — so the caller decides what is in scope and passes it here.
pub fn render_type_for(
    tipe: &can::Type,
    aliases: &AliasNames,
    unqualified: &std::collections::HashSet<String>,
) -> String {
    let long = render(tipe, Ctx::None, aliases);
    shorten(&long, unqualified)
}

/// Drop the `Module.` in front of each `Module.Name` whose module is in
/// `unqualified`. Only a dot between an identifier character and an upper-case
/// letter qualifies a type name, so nothing else in a rendered type can match.
fn shorten(rendered: &str, unqualified: &std::collections::HashSet<String>) -> String {
    let chars: Vec<char> = rendered.chars().collect();
    let mut out = String::with_capacity(rendered.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '.'
            && i + 1 < chars.len()
            && chars[i + 1].is_uppercase()
            && out.chars().next_back().is_some_and(|p| p.is_alphanumeric() || p == '_')
        {
            // Find where this qualified name started, and drop the prefix only
            // if that module's names are in scope unqualified.
            let mut start = out.len();
            for (at, p) in out.char_indices().rev() {
                if p.is_alphanumeric() || p == '_' || p == '.' {
                    start = at;
                } else {
                    break;
                }
            }
            if unqualified.contains(&out[start..]) {
                out.truncate(start);
                i += 1;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Canonicalization expands type aliases, but `docs.json` publishes the alias
/// name — `Probe.Point`, not the record it stands for. Fold them back by body.
///
/// Only aliases that take no arguments are folded, and only when their body is
/// unique: two aliases sharing a body cannot be told apart from the expansion,
/// so neither is used rather than guessing.
#[derive(Default)]
pub struct AliasNames {
    by_body: BTreeMap<String, String>,
    /// A parameterized alias that is a plain rename of another type
    /// (`type alias Parser a = Internal.QueryParser a`), keyed by the
    /// underlying `home.name`. Packages use these to re-export an internal
    /// type under a public name, and the docs publish the public one.
    renames: BTreeMap<String, String>,
}

impl AliasNames {
    /// Build the fold table used when documenting `home`.
    ///
    /// elm publishes the alias *as the source wrote it*, which alm cannot
    /// recover — canonicalization expands aliases away. Two rules reproduce
    /// what elm prints in practice:
    ///
    /// * a **zero-argument** alias folds wherever its body is unambiguous,
    ///   with the documented module's own alias winning — `Url.Parser` names
    ///   `Url.Url`, and `Json.Encode`'s docs say `Json.Encode.Value` even
    ///   though the underlying type is `Json.Decode.Value`.
    /// * a **parameterized rename** (`Parser a = Internal.QueryParser a`) is
    ///   folded everywhere: it is how a package gives an internal type its
    ///   public face, and every module that mentions it means the public one.
    pub fn preferring(
        interfaces: &BTreeMap<Name, &Interface>,
        home: Option<&Name>,
    ) -> AliasNames {
        // Zero-argument aliases, globally where unambiguous, with the
        // documented module's own winning.
        let mut by_body: BTreeMap<String, String> = BTreeMap::new();
        let mut body_counts: BTreeMap<String, usize> = BTreeMap::new();
        let mut global: BTreeMap<String, String> = BTreeMap::new();
        for (module, interface) in interfaces {
            for (name, (vars, body)) in &interface.aliases {
                if !vars.is_empty() {
                    continue;
                }
                let key = render(body, Ctx::None, &AliasNames::default());
                *body_counts.entry(key.clone()).or_default() += 1;
                global.insert(key, format!("{module}.{name}"));
            }
        }
        by_body.extend(global.into_iter().filter(|(k, _)| body_counts[k] == 1));
        if let Some((home, interface)) = home.and_then(|h| interfaces.get(h).map(|i| (h, i))) {
            for (name, (vars, body)) in &interface.aliases {
                if vars.is_empty() {
                    let key = render(body, Ctx::None, &AliasNames::default());
                    by_body.insert(key, format!("{home}.{name}"));
                }
            }
        }

        let mut renames: BTreeMap<String, String> = BTreeMap::new();
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for (module, interface) in interfaces {
            for (name, (vars, body)) in &interface.aliases {
                if vars.is_empty() {
                    continue;
                }
                if let Some(underlying) = plain_rename(vars, body) {
                    *counts.entry(underlying.clone()).or_default() += 1;
                    renames.insert(underlying, format!("{module}.{name}"));
                }
            }
        }
        AliasNames {
            by_body,
            // Two aliases renaming the same type cannot be told apart from the
            // expansion, so neither is used.
            renames: renames.into_iter().filter(|(k, _)| counts[k] == 1).collect(),
        }
    }

    fn lookup(&self, rendered: &str) -> Option<&String> {
        self.by_body.get(rendered)
    }

    fn rename(&self, home: &Name, name: &Name) -> Option<&String> {
        self.renames.get(&format!("{home}.{name}"))
    }
}

/// `type alias Parser a = Internal.QueryParser a` — an alias whose body is the
/// underlying type applied to exactly its own variables, in order. Returns the
/// underlying `home.name`.
fn plain_rename(vars: &[Name], body: &can::Type) -> Option<String> {
    let can::Type::Type(home, name, args) = body else {
        return None;
    };
    if args.len() != vars.len() {
        return None;
    }
    let same = args.iter().zip(vars).all(|(arg, var)| matches!(arg, can::Type::Var(v) if v == var));
    same.then(|| format!("{home}.{name}"))
}

fn render_with_aliases(tipe: &can::Type, aliases: &AliasNames) -> String {
    render(tipe, Ctx::None, aliases)
}

/// An alias's own body is its definition, so it never folds back into itself —
/// but anything nested inside it still does.
fn render_alias_body(body: &can::Type, aliases: &AliasNames, own: &str) -> String {
    let narrowed = AliasNames {
        by_body: aliases.by_body.clone(),
        renames: aliases.renames.iter().filter(|(_, v)| v.as_str() != own).map(|(k, v)| (k.clone(), v.clone())).collect(),
    };
    render_plain(body, Ctx::None, &narrowed)
}

#[derive(Clone, Copy, PartialEq)]
enum Ctx {
    None,
    Func,
    App,
}

fn render(tipe: &can::Type, ctx: Ctx, aliases: &AliasNames) -> String {
    // An expanded alias prints under its own name.
    if !matches!(tipe, can::Type::Var(_)) {
        let plain = render_plain(tipe, Ctx::None, aliases);
        if let Some(name) = aliases.lookup(&plain) {
            return name.clone();
        }
    }
    render_plain(tipe, ctx, aliases)
}

fn render_plain(tipe: &can::Type, ctx: Ctx, aliases: &AliasNames) -> String {
    match tipe {
        can::Type::Var(name) => name.to_string(),
        can::Type::Unit => "()".to_string(),
        can::Type::Lambda(..) => {
            let mut parts = Vec::new();
            let mut current = tipe;
            while let can::Type::Lambda(arg, result) = current {
                parts.push(render(arg, Ctx::Func, aliases));
                current = result;
            }
            parts.push(render(current, Ctx::None, aliases));
            let joined = parts.join(" -> ");
            if ctx == Ctx::None {
                joined
            } else {
                format!("({joined})")
            }
        }
        can::Type::Type(home, name, args) => {
            let qualified = match aliases.rename(home, name) {
                Some(public) => public.clone(),
                None => format!("{home}.{name}"),
            };
            if args.is_empty() {
                return qualified;
            }
            let rendered: Vec<String> = args.iter().map(|a| render(a, Ctx::App, aliases)).collect();
            let applied = format!("{qualified} {}", rendered.join(" "));
            if ctx == Ctx::App {
                format!("({applied})")
            } else {
                applied
            }
        }
        can::Type::Tuple(a, b, c) => {
            let mut parts = vec![render(a, Ctx::None, aliases), render(b, Ctx::None, aliases)];
            if let Some(c) = c {
                parts.push(render(c, Ctx::None, aliases));
            }
            format!("( {} )", parts.join(", "))
        }
        can::Type::Record(fields, ext) => {
            if fields.is_empty() && ext.is_none() {
                return "{}".to_string();
            }
            let rendered: Vec<String> =
                fields.iter().map(|(n, t)| format!("{n} : {}", render(t, Ctx::None, aliases))).collect();
            match ext {
                Some(var) => format!("{{ {var} | {} }}", rendered.join(", ")),
                None => format!("{{ {} }}", rendered.join(", ")),
            }
        }
    }
}

/// One module's documentation, ready to encode.
struct ModuleDocs<'a> {
    name: &'a Name,
    comment: String,
    unions: Vec<(String, String, Vec<String>, Vec<(String, Vec<String>)>)>,
    aliases: Vec<(String, String, Vec<String>, String)>,
    values: Vec<(String, String, String)>,
    binops: Vec<(String, String, String, &'static str, u8)>,
}

/// Build `docs.json` for the modules a package exposes.
pub fn generate(
    modules: &[can::Module],
    interfaces: &BTreeMap<Name, &Interface>,
    sources: &BTreeMap<Name, String>,
    exposed: &[Name],
) -> String {
    let mut docs = Vec::new();
    for name in exposed {
        let Some(module) = modules.iter().find(|m| &m.name == name) else {
            continue;
        };
        let Some(interface) = interfaces.get(name) else {
            continue;
        };
        let empty = String::new();
        let source = sources.get(name).unwrap_or(&empty);
        let alias_names = AliasNames::preferring(interfaces, Some(name));
        docs.push(module_docs(module, interface, source, &alias_names));
    }
    encode(&docs)
}

fn module_docs<'a>(
    module: &'a can::Module,
    interface: &Interface,
    source: &str,
    alias_names: &AliasNames,
) -> ModuleDocs<'a> {
    let (comments, comment_spans) = scan_comments(source);
    // The module's own comment is the first one in the file; the rest belong to
    // whatever declaration follows them.
    let module_comment = comments.first().map(|c| c.text.clone()).unwrap_or_default();
    let order = docs_order(&module_comment);
    let rank = |name: &str| order.iter().position(|n| n == name).unwrap_or(usize::MAX);

    let mut unions = Vec::new();
    for union in &module.unions {
        if !interface.unions.contains_key(&union.name) {
            continue;
        }
        let cases = if interface.open_unions.contains(&union.name) {
            union
                .ctors
                .iter()
                .map(|ctor| {
                    (
                        ctor.name.to_string(),
                        ctor.args.iter().map(|a| render_with_aliases(a, alias_names)).collect(),
                    )
                })
                .collect()
        } else {
            Vec::new()
        };
        unions.push((
            union.name.to_string(),
            comment_for(&comments, region_of_union(source, &union.name, &comment_spans)),
            union.vars.iter().map(|v| v.to_string()).collect(),
            cases,
        ));
    }

    let mut aliases = Vec::new();
    for (name, (vars, body)) in &interface.aliases {
        aliases.push((
            name.to_string(),
            comment_for(&comments, region_of_union(source, name, &comment_spans)),
            vars.iter().map(|v| v.to_string()).collect(),
            render_alias_body(body, alias_names, &format!("{}.{}", module.name, name)),
        ));
    }

    let mut values = Vec::new();
    for (name, tipe) in &interface.values {
        values.push((
            name.to_string(),
            comment_for(&comments, region_of_decl(source, name.as_str(), &comment_spans)),
            render_with_aliases(tipe, alias_names),
        ));
    }

    let mut binops = Vec::new();
    for (op, def) in &interface.binops {
        if let Some(tipe) = &def.tipe {
            binops.push((
                op.to_string(),
                comment_for(&comments, region_of_decl(source, def.function.as_str(), &comment_spans)),
                render_with_aliases(tipe, alias_names),
                associativity_name(def),
                def.precedence,
            ));
        }
    }

    // `@docs` decides what is published and in what order; anything not listed
    // is omitted, and the rest sort by name within each kind.
    unions.retain(|(n, ..)| rank(n) != usize::MAX);
    aliases.retain(|(n, ..)| rank(n) != usize::MAX);
    values.retain(|(n, ..)| rank(n) != usize::MAX);
    unions.sort_by(|a, b| a.0.cmp(&b.0));
    aliases.sort_by(|a, b| a.0.cmp(&b.0));
    values.sort_by(|a, b| a.0.cmp(&b.0));
    binops.sort_by(|a, b| a.0.cmp(&b.0));

    ModuleDocs { name: &module.name, comment: module_comment, unions, aliases, values, binops }
}

fn associativity_name(def: &crate::interface::BinopDef) -> &'static str {
    use crate::ast::source::Associativity::*;
    match def.associativity {
        Left => "left",
        Right => "right",
        Non => "non",
    }
}

/// The line a top-level declaration named `name` starts on, if it can be found.
/// Declarations start in column 1, so a line beginning with the name (or with
/// `type`/`type alias` introducing it) identifies it without re-parsing.
fn region_of_decl(source: &str, name: &str, spans: &[(u32, u32)]) -> Option<u32> {
    source.lines().enumerate().find_map(|(i, line)| {
        let row = i as u32 + 1;
        let starts = line.starts_with(name)
            && line[name.len()..].starts_with(|c: char| c.is_whitespace() || c == ':')
            && !in_comment(spans, row);
        starts.then_some(row)
    })
}

fn region_of_union(source: &str, name: &Name, spans: &[(u32, u32)]) -> Option<u32> {
    let type_decl = format!("type {name}");
    let alias_decl = format!("type alias {name}");
    source.lines().enumerate().find_map(|(i, line)| {
        let row = i as u32 + 1;
        if in_comment(spans, row) {
            return None;
        }
        let hit = (line.starts_with(&type_decl) || line.starts_with(&alias_decl))
            && line
                .strip_prefix(&type_decl)
                .or_else(|| line.strip_prefix(&alias_decl))
                .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace));
        hit.then_some(row)
    })
}

/// The doc comment sitting directly above line `row`.
fn comment_for(comments: &[DocComment], row: Option<u32>) -> String {
    let Some(row) = row else {
        return String::new();
    };
    comments
        .iter()
        .filter(|c| c.end_row < row)
        .max_by_key(|c| c.end_row)
        .filter(|c| c.end_row + 1 == row)
        .map(|c| c.text.clone())
        .unwrap_or_default()
}

fn encode(docs: &[ModuleDocs]) -> String {
    let mut out = String::from("[");
    for (i, module) in docs.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("{\"name\":");
        json_str(&module.name.to_string(), &mut out);
        out.push_str(",\"comment\":");
        json_str(&module.comment, &mut out);

        out.push_str(",\"unions\":[");
        for (j, (name, comment, args, cases)) in module.unions.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            out.push_str("{\"name\":");
            json_str(name, &mut out);
            out.push_str(",\"comment\":");
            json_str(comment, &mut out);
            out.push_str(",\"args\":");
            encode_strings(args, &mut out);
            out.push_str(",\"cases\":[");
            for (k, (ctor, ctor_args)) in cases.iter().enumerate() {
                if k > 0 {
                    out.push(',');
                }
                out.push('[');
                json_str(ctor, &mut out);
                out.push(',');
                encode_strings(ctor_args, &mut out);
                out.push(']');
            }
            out.push_str("]}");
        }

        out.push_str("],\"aliases\":[");
        for (j, (name, comment, args, tipe)) in module.aliases.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            out.push_str("{\"name\":");
            json_str(name, &mut out);
            out.push_str(",\"comment\":");
            json_str(comment, &mut out);
            out.push_str(",\"args\":");
            encode_strings(args, &mut out);
            out.push_str(",\"type\":");
            json_str(tipe, &mut out);
            out.push('}');
        }

        out.push_str("],\"values\":[");
        for (j, (name, comment, tipe)) in module.values.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            out.push_str("{\"name\":");
            json_str(name, &mut out);
            out.push_str(",\"comment\":");
            json_str(comment, &mut out);
            out.push_str(",\"type\":");
            json_str(tipe, &mut out);
            out.push('}');
        }

        out.push_str("],\"binops\":[");
        for (j, (name, comment, tipe, associativity, precedence)) in
            module.binops.iter().enumerate()
        {
            if j > 0 {
                out.push(',');
            }
            out.push_str("{\"name\":");
            json_str(name, &mut out);
            out.push_str(",\"comment\":");
            json_str(comment, &mut out);
            out.push_str(",\"type\":");
            json_str(tipe, &mut out);
            out.push_str(",\"associativity\":");
            json_str(associativity, &mut out);
            out.push_str(&format!(",\"precedence\":{precedence}}}"));
        }
        out.push_str("]}");
    }
    out.push(']');
    out
}

fn encode_strings(items: &[String], out: &mut String) {
    out.push('[');
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        json_str(item, out);
    }
    out.push(']');
}

/// Unused today, kept so the region helpers keep a single meaning.
#[allow(dead_code)]
fn region_row(region: Region) -> u32 {
    region.start.row
}
