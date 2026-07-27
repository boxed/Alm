//! `Reporting.Render.Type` — laying a type out as a `Doc`.
//!
//! Shared by everything that has to print a type the way elm prints it: the
//! type-error reports, and `alm diff`. Only the layout lives here; turning a
//! particular representation (a unification `ErrorType`, a `docs.json` type)
//! into these calls is the caller's job.

use super::doc::Doc;

/// `Reporting.Render.Type.Context` — whether a type needs parentheses here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ctx {
    None,
    Func,
    App,
}

pub fn lambda(ctx: Ctx, arg1: Doc, arg2: Doc, rest: Vec<Doc>) -> Doc {
    let mut parts = vec![arg1];
    for a in std::iter::once(arg2).chain(rest) {
        parts.push(Doc::space(Doc::text("->"), a));
    }
    let inner = Doc::align(Doc::sep(parts));
    match ctx {
        Ctx::None => inner,
        Ctx::Func | Ctx::App => Doc::cat(vec![Doc::text("("), inner, Doc::text(")")]),
    }
}

pub fn apply(ctx: Ctx, name: Doc, args: Vec<Doc>) -> Doc {
    if args.is_empty() {
        return name;
    }
    let mut parts = vec![name];
    parts.extend(args);
    let inner = Doc::hang(4, Doc::sep(parts));
    match ctx {
        Ctx::App => Doc::cat(vec![Doc::text("("), inner, Doc::text(")")]),
        Ctx::Func | Ctx::None => inner,
    }
}

pub fn tuple(a: Doc, b: Doc, cs: Vec<Doc>) -> Doc {
    let mut entries = Vec::new();
    for (i, part) in std::iter::once(a).chain(std::iter::once(b)).chain(cs).enumerate() {
        entries.push(Doc::space(Doc::text(if i == 0 { "(" } else { "," }), part));
    }
    Doc::align(Doc::sep(vec![Doc::cat(entries), Doc::text(")")]))
}

pub fn entry(field: Doc, tipe: Doc) -> Doc {
    Doc::hang(4, Doc::sep(vec![Doc::space(field, Doc::text(":")), tipe]))
}

pub fn record(entries: Vec<(Doc, Doc)>, ext: Option<Doc>) -> Doc {
    let fields: Vec<Doc> = entries.into_iter().map(|(f, t)| entry(f, t)).collect();
    match (fields.is_empty(), ext) {
        (true, None) => Doc::text("{}"),
        (false, None) => {
            let mut parts = Vec::new();
            for (i, f) in fields.into_iter().enumerate() {
                parts.push(Doc::space(Doc::text(if i == 0 { "{" } else { "," }), f));
            }
            Doc::align(Doc::sep(vec![Doc::cat(parts), Doc::text("}")]))
        }
        (_, Some(ext)) => {
            let mut parts = Vec::new();
            for (i, f) in fields.into_iter().enumerate() {
                parts.push(Doc::space(Doc::text(if i == 0 { "|" } else { "," }), f));
            }
            let head = Doc::hang(
                4,
                Doc::sep(vec![Doc::space(Doc::text("{"), ext), Doc::cat(parts)]),
            );
            Doc::align(Doc::sep(vec![head, Doc::text("}")]))
        }
    }
}

/// `RT.vrecord` — one field per line, always broken.
pub fn vrecord(entries: Vec<(Doc, Doc)>, ext: Option<Doc>) -> Doc {
    let fields: Vec<Doc> = entries.into_iter().map(|(f, t)| entry(f, t)).collect();
    match (fields.is_empty(), ext) {
        (true, None) => Doc::text("{}"),
        (false, None) => {
            let mut parts = Vec::new();
            for (i, f) in fields.into_iter().enumerate() {
                parts.push(Doc::space(Doc::text(if i == 0 { "{" } else { "," }), f));
            }
            parts.push(Doc::text("}"));
            Doc::vcat(parts)
        }
        (_, Some(ext)) => {
            let mut parts = Vec::new();
            for (i, f) in fields.into_iter().enumerate() {
                parts.push(Doc::space(Doc::text(if i == 0 { "|" } else { "," }), f));
            }
            let head = Doc::hang(
                4,
                Doc::vcat(vec![Doc::space(Doc::text("{"), ext), Doc::cat(parts)]),
            );
            Doc::vcat(vec![head, Doc::text("}")])
        }
    }
}

/// `RT.vrecordSnippet` — the first few fields then `...`.
pub fn vrecord_snippet(first: (Doc, Doc), rest: Vec<(Doc, Doc)>) -> Doc {
    let mut lines = vec![Doc::space(Doc::text("{"), entry(first.0, first.1))];
    for (f, t) in rest {
        lines.push(Doc::space(Doc::text(","), entry(f, t)));
    }
    lines.push(Doc::space(Doc::text(","), Doc::text("...")));
    lines.push(Doc::text("}"));
    Doc::vcat(lines)
}

