//! Port of `Nitpick.Debug`: which modules still reference the `Debug` module.
//!
//! `--optimize` strips out the information `Debug` needs — record field names
//! are shortened and single-constructor types are unboxed — so elm refuses to
//! build an optimized bundle while any `Debug` call survives, and names the
//! modules to fix.

use crate::ast::canonical as can;
use crate::data::Name;

/// The modules that still call something from `Debug`, in the order elm lists
/// them (module order, which is dependency order).
pub fn modules_using_debug(modules: &[can::Module]) -> Vec<Name> {
    modules
        .iter()
        .filter(|module| module.decls.iter().any(group_uses_debug))
        .map(|module| module.name.clone())
        .collect()
}

fn group_uses_debug(group: &can::DeclGroup) -> bool {
    match group {
        can::DeclGroup::Value(def) => def_uses_debug(def),
        can::DeclGroup::Recursive(defs) => defs.iter().any(def_uses_debug),
    }
}

fn def_uses_debug(def: &can::Def) -> bool {
    uses_debug(&def.body)
}

fn uses_debug(expr: &can::Expr) -> bool {
    use can::Expr_::*;
    match &expr.value {
        VarForeign(home, _) => home.as_str() == "Debug",
        // `Debug.log`/`Debug.todo` reach codegen as kernel references too.
        VarLocal(_) | VarTopLevel(_) | VarCtor(..) | Chr(_) | Str(_) | Int(_) | Float(_)
        | Unit | Accessor(_) | Shader(_) => false,
        List(items) => items.iter().any(uses_debug),
        Negate(inner) | Access(inner, _) => uses_debug(inner),
        Binop(_, _, _, left, right) => uses_debug(left) || uses_debug(right),
        Lambda(_, body) => uses_debug(body),
        Call(func, args) => uses_debug(func) || args.iter().any(uses_debug),
        If(branches, otherwise) => {
            branches.iter().any(|(c, b)| uses_debug(c) || uses_debug(b)) || uses_debug(otherwise)
        }
        Let(decls, body) => decls.iter().any(let_decl_uses_debug) || uses_debug(body),
        Case(scrutinee, branches) => {
            uses_debug(scrutinee) || branches.iter().any(|(_, branch)| uses_debug(branch))
        }
        Update(record, fields) => {
            uses_debug(record) || fields.iter().any(|(_, value)| uses_debug(value))
        }
        Record(fields) => fields.iter().any(|(_, value)| uses_debug(value)),
        Tuple(a, b, rest) => uses_debug(a) || uses_debug(b) || rest.iter().any(uses_debug),
    }
}

fn let_decl_uses_debug(decl: &can::LetDecl) -> bool {
    match decl {
        can::LetDecl::Def(def) => def_uses_debug(def),
        can::LetDecl::Recursive(defs) => defs.iter().any(def_uses_debug),
        can::LetDecl::Destruct(_, value) => uses_debug(value),
    }
}

/// `Reporting.Exit.GenerateCannotOptimizeDebugValues` — the report elm shows
/// when `--optimize` meets a surviving `Debug` call. This one has no source
/// snippet: it names the modules and explains why the restriction exists.
pub fn debug_remnants_report(modules: &[Name]) -> crate::reporting::Report {
    use crate::reporting::{sentence, words, Doc, ElmBody, Region, Report, Section};
    let listed: Vec<Doc> = modules
        .iter()
        .map(|name| Doc::color(crate::reporting::Color::RedVivid, Doc::text(name.to_string())))
        .collect();
    Report {
        title: "DEBUG REMNANTS".to_string(),
        region: Region::ZERO,
        message: String::new(),
        elm: Some(ElmBody {
            before: Doc::reflow(
                "There are uses of the `Debug` module in the following modules:",
            ),
            // With no snippet, `after` is simply the next block; the rest
            // follow as notes, which reproduces elm's `D.stack`.
            after: Doc::indent(4, sentence(listed)),
            notes: vec![
                Section::para(
                    "But the --optimize flag only works if all `Debug` functions are removed!",
                ),
                crate::reporting::note(words(
                    "The issue is that --optimize strips out info needed by `Debug` functions. \
                     Here are two examples:",
                )),
                Section::Para(Doc::indent(
                    4,
                    Doc::reflow(
                        "(1) It shortens record field names. This makes the generated JavaScript \
                         is smaller, but `Debug.toString` cannot know the real field names \
                         anymore.",
                    ),
                )),
                Section::Para(Doc::indent(
                    4,
                    Doc::reflow(
                        "(2) Values like `type Height = Height Float` are unboxed. This reduces \
                         allocation, but it also means that `Debug.toString` cannot tell if it \
                         is looking at a `Height` or `Float` value.",
                    ),
                )),
                Section::para(
                    "There are a few other cases like that, and it will be much worse once we \
                     start inlining code. That optimization could move `Debug.log` and \
                     `Debug.todo` calls, resulting in unpredictable behavior. I hope that \
                     clarifies why this restriction exists!",
                ),
            ],
            // No source snippet: the report names modules, not positions.
            region: None,
            highlight: Region::ZERO,
        }),
    }
}
