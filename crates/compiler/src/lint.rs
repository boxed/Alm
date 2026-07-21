//! Lightweight performance lints over the canonical AST.
//!
//! Currently one lint: a **keyed list rendered without `Html.Lazy`**. A
//! `Html.Keyed.*` node whose children come from `List.map`/`List.indexedMap`
//! rebuilds and diffs every item on every update — even a change to unrelated
//! state (e.g. which row is selected) forces the whole list through the diff.
//! Wrapping each item's view in `Html.Lazy.lazy*` lets the runtime skip items
//! whose inputs are unchanged (O(changed) instead of O(list)). This is emitted
//! as a HINT, never an error.

use crate::ast::canonical as can;
use crate::data::Name;
use crate::reporting::{Region, Report};
use std::collections::HashMap;
use std::path::PathBuf;

pub struct Warning {
    pub path: PathBuf,
    pub source: String,
    pub report: Report,
}

impl Warning {
    pub fn render(&self) -> String {
        self.report.render(&self.path.display().to_string(), &self.source)
    }
}

const HINT: &str = "PERFORMANCE HINT";

fn message() -> String {
    "This keyed list is rebuilt and diffed on every update — even when only a \
few items change (say, selecting one row), the whole list goes through the \
virtual-DOM diff. Wrap each item's view in `Html.Lazy.lazy*` so items whose \
inputs are unchanged are skipped:\n\n    \
import Html.Lazy exposing (lazy2)\n\n    \
Html.Keyed.node \"tbody\" []\n        \
(List.map (\\row -> ( String.fromInt row.id, lazy2 viewRow (row.id == selected) row )) rows)\n\n\
`Html.Lazy` memoizes on its arguments by reference, so make each argument \
reference-stable across renders (pass the row record and a derived Bool, not \
the whole model). Unchanged rows are then neither rebuilt nor diffed. This is a \
hint, not an error — ignore it for small or rarely-changing lists."
        .to_string()
}

/// Whether `e` is a call to `Html.Lazy.lazyN` (or `VirtualDom.lazyN`).
fn is_lazy_call(e: &can::Expr) -> bool {
    if let can::Expr_::Call(f, _) = &e.value {
        if let can::Expr_::VarForeign(home, name) = &f.value {
            return (home.as_str() == "Html.Lazy" || home.as_str() == "VirtualDom")
                && name.as_str().starts_with("lazy");
        }
    }
    false
}

/// Resolve the item-producing function of a `List.map`/`indexedMap` to its body:
/// an inline `\x -> ...` lambda, or a named top-level `viewRow x = ...`.
fn item_body<'a>(f: &'a can::Expr, defs: &HashMap<&'a str, &'a can::Def>) -> Option<&'a can::Expr> {
    match &f.value {
        can::Expr_::Lambda(_, body) => Some(body),
        can::Expr_::VarTopLevel(n) => defs.get(n.as_str()).map(|d| &d.body),
        // Partial application, e.g. `List.map (viewRow selected) rows` — peel to
        // the underlying function and use its body.
        can::Expr_::Call(inner, _) => item_body(inner, defs),
        _ => None,
    }
}

/// A `Html.Keyed.*` node's children argument. If it is built by a `List` map
/// whose per-item node (the second element of the `(key, node)` tuple) is not a
/// `Html.Lazy` call, return the node's region to flag.
fn keyed_children_region<'a>(
    children: &'a can::Expr,
    defs: &HashMap<&'a str, &'a can::Def>,
) -> Option<Region> {
    let can::Expr_::Call(mf, margs) = &children.value else { return None };
    let can::Expr_::VarForeign(home, name) = &mf.value else { return None };
    if home.as_str() != "List" {
        return None;
    }
    // `List.map f xs` / `List.indexedMap f xs` — the item function is first.
    if !matches!(name.as_str(), "map" | "indexedMap") || margs.is_empty() {
        return None;
    }
    let body = item_body(&margs[0], defs)?;
    // A keyed child is a `(key, node)` tuple; flag when `node` isn't lazy.
    if let can::Expr_::Tuple(_, node, _) = &body.value {
        if !is_lazy_call(node) {
            return Some(node.region);
        }
    }
    None
}

fn walk<'a>(e: &'a can::Expr, defs: &HashMap<&'a str, &'a can::Def>, out: &mut Vec<Region>) {
    use can::Expr_::*;
    if let Call(f, args) = &e.value {
        if let VarForeign(home, _) = &f.value {
            if home.as_str() == "Html.Keyed" {
                if let Some(children) = args.last() {
                    if let Some(region) = keyed_children_region(children, defs) {
                        out.push(region);
                    }
                }
            }
        }
    }
    // Recurse into every sub-expression.
    match &e.value {
        VarLocal(_) | VarTopLevel(_) | VarForeign(_, _) | VarCtor(_, _, _) | Chr(_) | Str(_)
        | Int(_) | Float(_) | Unit | Accessor(_) | Shader(_) => {}
        Negate(x) | Access(x, _) => walk(x, defs, out),
        Binop(_, _, _, l, r) => {
            walk(l, defs, out);
            walk(r, defs, out);
        }
        List(xs) => xs.iter().for_each(|x| walk(x, defs, out)),
        Call(f, args) => {
            walk(f, defs, out);
            args.iter().for_each(|a| walk(a, defs, out));
        }
        If(bs, otherwise) => {
            for (c, t) in bs {
                walk(c, defs, out);
                walk(t, defs, out);
            }
            walk(otherwise, defs, out);
        }
        Lambda(_, b) => walk(b, defs, out),
        Let(decls, body) => {
            for d in decls {
                match d {
                    can::LetDecl::Def(def) => walk(&def.body, defs, out),
                    can::LetDecl::Recursive(defs2) => defs2.iter().for_each(|d| walk(&d.body, defs, out)),
                    can::LetDecl::Destruct(_, ex) => walk(ex, defs, out),
                }
            }
            walk(body, defs, out);
        }
        Case(scrut, branches) => {
            walk(scrut, defs, out);
            branches.iter().for_each(|(_, b)| walk(b, defs, out));
        }
        Update(x, fields) => {
            walk(x, defs, out);
            fields.iter().for_each(|(_, v)| walk(v, defs, out));
        }
        Record(fields) => fields.iter().for_each(|(_, v)| walk(v, defs, out)),
        Tuple(a, b, rest) => {
            walk(a, defs, out);
            walk(b, defs, out);
            rest.iter().for_each(|x| walk(x, defs, out));
        }
    }
}

fn group_defs(g: &can::DeclGroup) -> &[can::Def] {
    match g {
        can::DeclGroup::Value(d) => std::slice::from_ref(d),
        can::DeclGroup::Recursive(ds) => ds,
    }
}

/// Run the performance lints over every module. `sources` supplies each
/// module's file path + text for rendering.
pub fn lint(
    modules: &[can::Module],
    sources: &HashMap<Name, (PathBuf, String)>,
) -> Vec<Warning> {
    let mut out = Vec::new();
    for m in modules {
        let mut defs: HashMap<&str, &can::Def> = HashMap::new();
        for g in &m.decls {
            for d in group_defs(g) {
                defs.insert(d.name.value.as_str(), d);
            }
        }
        let mut regions = Vec::new();
        for g in &m.decls {
            for d in group_defs(g) {
                walk(&d.body, &defs, &mut regions);
            }
        }
        if regions.is_empty() {
            continue;
        }
        let Some((path, src)) = sources.get(&m.name) else { continue };
        for region in regions {
            out.push(Warning {
                path: path.clone(),
                source: src.clone(),
                report: Report { title: HINT.to_string(), region, message: message() },
            });
        }
    }
    out
}
