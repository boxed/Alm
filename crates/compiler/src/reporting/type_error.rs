//! Port of `Type.Error` and the expression half of `Reporting.Error.Type`.
//!
//! A type error is reported as a *comparison*: elm renders the type it found
//! and the type it wanted side by side, marks the parts that differ, and picks
//! a hint from the specific way they differ (Int vs Float, a String where an
//! Int was wanted, a field typo, …). Both the wording and the layout are
//! reproduced here so alm's reports match `elm make` byte for byte.

use std::collections::BTreeMap;

use super::doc::Doc;
use super::{ElmBody, Report, Section};
use crate::reporting::annotation::Region;

const WIDTH: usize = 80;

// ---------------------------------------------------------------- error types

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Super {
    Number,
    Comparable,
    Appendable,
    CompAppend,
}

impl Super {
    fn name(self) -> &'static str {
        match self {
            Super::Number => "number",
            Super::Comparable => "comparable",
            Super::Appendable => "appendable",
            Super::CompAppend => "compappend",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Extension {
    Closed,
    FlexOpen(String),
    RigidOpen(String),
}

/// `Type.Error.Type` — a type frozen for reporting, with `?` for the parts
/// unification gave up on and `∞` where it would recurse forever.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorType {
    /// `Lambda a b [c, d]` is `a -> b -> c -> d`; at least two parts.
    Lambda(Box<ErrorType>, Box<ErrorType>, Vec<ErrorType>),
    Infinite,
    Error,
    FlexVar(String),
    FlexSuper(Super, String),
    RigidVar(String),
    RigidSuper(Super, String),
    /// home module, name, arguments.
    Type(String, String, Vec<ErrorType>),
    Record(BTreeMap<String, ErrorType>, Extension),
    Unit,
    Tuple(Box<ErrorType>, Box<ErrorType>, Option<Box<ErrorType>>),
}

impl ErrorType {
    fn is_named(&self, home: &str, name: &str) -> bool {
        matches!(self, ErrorType::Type(h, n, _) if h == home && n == name)
    }

    fn is_int(&self) -> bool {
        self.is_named("Basics", "Int")
    }
    fn is_float(&self) -> bool {
        self.is_named("Basics", "Float")
    }
    fn is_string(&self) -> bool {
        self.is_named("String", "String")
    }
    fn is_char(&self) -> bool {
        self.is_named("Char", "Char")
    }
    fn is_bool(&self) -> bool {
        self.is_named("Basics", "Bool")
    }
    fn is_list(&self) -> bool {
        self.is_named("List", "List")
    }

    /// How many arguments a value of this type takes.
    pub fn arity(&self) -> usize {
        match self {
            ErrorType::Lambda(_, _, rest) => 2 + rest.len() - 1,
            _ => 0,
        }
    }

    fn is_super(&self, super_: Super) -> bool {
        match self {
            ErrorType::Type(_, _, args) => {
                let head = args.first();
                match super_ {
                    Super::Number => self.is_int() || self.is_float(),
                    Super::Comparable => {
                        self.is_int()
                            || self.is_float()
                            || self.is_string()
                            || self.is_char()
                            || (self.is_list() && head.is_some_and(|a| a.is_super(super_)))
                    }
                    Super::Appendable => self.is_string() || self.is_list(),
                    Super::CompAppend => {
                        self.is_string()
                            || (self.is_list() && head.is_some_and(|a| a.is_super(Super::Comparable)))
                    }
                }
            }
            ErrorType::Tuple(a, b, c) => match super_ {
                Super::Comparable => {
                    a.is_super(super_)
                        && b.is_super(super_)
                        && c.as_ref().is_none_or(|c| c.is_super(super_))
                }
                _ => false,
            },
            _ => false,
        }
    }
}

// ------------------------------------------------------------------- to a doc

/// `Reporting.Render.Type.Context` — whether a type needs parentheses here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ctx {
    None,
    Func,
    App,
}

fn lambda_doc(ctx: Ctx, arg1: Doc, arg2: Doc, rest: Vec<Doc>) -> Doc {
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

fn apply_doc(ctx: Ctx, name: Doc, args: Vec<Doc>) -> Doc {
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

fn tuple_doc(a: Doc, b: Doc, cs: Vec<Doc>) -> Doc {
    let mut entries = Vec::new();
    for (i, part) in std::iter::once(a).chain(std::iter::once(b)).chain(cs).enumerate() {
        entries.push(Doc::space(Doc::text(if i == 0 { "(" } else { "," }), part));
    }
    Doc::align(Doc::sep(vec![Doc::cat(entries), Doc::text(")")]))
}

fn entry_doc(field: Doc, tipe: Doc) -> Doc {
    Doc::hang(4, Doc::sep(vec![Doc::space(field, Doc::text(":")), tipe]))
}

fn record_doc(entries: Vec<(Doc, Doc)>, ext: Option<Doc>) -> Doc {
    let fields: Vec<Doc> = entries.into_iter().map(|(f, t)| entry_doc(f, t)).collect();
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

fn to_doc(ctx: Ctx, tipe: &ErrorType) -> Doc {
    match tipe {
        ErrorType::Lambda(a, b, rest) => lambda_doc(
            ctx,
            to_doc(Ctx::Func, a),
            to_doc(Ctx::Func, b),
            rest.iter().map(|t| to_doc(Ctx::Func, t)).collect(),
        ),
        ErrorType::Infinite => Doc::text("∞"),
        ErrorType::Error => Doc::text("?"),
        ErrorType::FlexVar(n)
        | ErrorType::FlexSuper(_, n)
        | ErrorType::RigidVar(n)
        | ErrorType::RigidSuper(_, n) => Doc::text(n.clone()),
        ErrorType::Type(_, name, args) => apply_doc(
            ctx,
            Doc::text(name.clone()),
            args.iter().map(|t| to_doc(Ctx::App, t)).collect(),
        ),
        ErrorType::Record(fields, ext) => record_doc(
            fields.iter().map(|(f, t)| (Doc::text(f.clone()), to_doc(Ctx::None, t))).collect(),
            ext_to_doc(ext),
        ),
        ErrorType::Unit => Doc::text("()"),
        ErrorType::Tuple(a, b, c) => tuple_doc(
            to_doc(Ctx::None, a),
            to_doc(Ctx::None, b),
            c.iter().map(|t| to_doc(Ctx::None, t)).collect(),
        ),
    }
}

/// `RT.vrecord` — one field per line, always broken.
fn vrecord_doc(entries: Vec<(Doc, Doc)>, ext: Option<Doc>) -> Doc {
    let fields: Vec<Doc> = entries.into_iter().map(|(f, t)| entry_doc(f, t)).collect();
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
fn vrecord_snippet_doc(first: (Doc, Doc), rest: Vec<(Doc, Doc)>) -> Doc {
    let mut lines = vec![Doc::space(Doc::text("{"), entry_doc(first.0, first.1))];
    for (f, t) in rest {
        lines.push(Doc::space(Doc::text(","), entry_doc(f, t)));
    }
    lines.push(Doc::space(Doc::text(","), Doc::text("...")));
    lines.push(Doc::text("}"));
    Doc::vcat(lines)
}

/// `toNearbyRecord` — the record's fields, closest match first, at most four.
fn nearby_record(fields: &BTreeMap<String, ErrorType>, field: &str, ext: &Extension) -> String {
    let order = nearby_names(field, &fields.keys().cloned().collect::<Vec<_>>());
    let entries: Vec<(Doc, Doc)> = order
        .iter()
        .map(|name| (Doc::text(name.clone()), to_doc(Ctx::None, &fields[name])))
        .collect();
    let doc = if entries.len() <= 4 {
        vrecord_doc(entries, ext_to_doc(ext))
    } else {
        let mut it = entries.into_iter();
        let first = it.next().unwrap();
        vrecord_snippet_doc(first, it.take(3).collect())
    };
    render_type(&doc)
}

fn ext_to_doc(ext: &Extension) -> Option<Doc> {
    match ext {
        Extension::Closed => None,
        Extension::FlexOpen(x) | Extension::RigidOpen(x) => Some(Doc::text(x.clone())),
    }
}

// ---------------------------------------------------------------- comparisons

/// The specific way two types differ, which decides the hint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    IntFloat,
    StringFromInt,
    StringFromFloat,
    StringToInt,
    StringToFloat,
    AnythingToBool,
    AnythingFromMaybe,
    ArityMismatch(usize, usize),
    BadFlexSuper(Direction, Super, String, ErrorType),
    BadRigidVar(String, ErrorType),
    BadRigidSuper(Super, String, ErrorType),
    FieldTypo(String, Vec<String>),
    FieldsMissing(Vec<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Have,
    Need,
}

struct Diff {
    left: Doc,
    right: Doc,
    problems: Option<Vec<Problem>>, // None = Similar
}

impl Diff {
    fn same(doc: Doc) -> Diff {
        Diff { left: doc.clone(), right: doc, problems: None }
    }
    fn similar(left: Doc, right: Doc) -> Diff {
        Diff { left, right, problems: None }
    }
    fn different(left: Doc, right: Doc, problems: Vec<Problem>) -> Diff {
        Diff { left, right, problems: Some(problems) }
    }
    fn is_similar(&self) -> bool {
        self.problems.is_none()
    }
}

fn merge(a: Option<Vec<Problem>>, b: Option<Vec<Problem>>) -> Option<Vec<Problem>> {
    match (a, b) {
        (None, b) => b,
        (a, None) => a,
        (Some(mut a), Some(b)) => {
            a.extend(b);
            Some(a)
        }
    }
}

/// Render both types, marking where they differ, and report how they differ.
pub fn to_comparison(actual: &ErrorType, expected: &ErrorType) -> (String, String, Vec<Problem>) {
    let diff = to_diff(Ctx::None, actual, expected);
    let problems = diff.problems.unwrap_or_default();
    (render_type(&diff.left), render_type(&diff.right), problems)
}

/// A type as it appears in a report: indented four columns, wrapped at 80.
fn render_type(doc: &Doc) -> String {
    Doc::indent(4, doc.clone()).render(WIDTH)
}

fn to_diff(ctx: Ctx, t1: &ErrorType, t2: &ErrorType) -> Diff {
    use ErrorType::*;
    match (t1, t2) {
        (Unit, Unit) | (Error, Error) | (Infinite, Infinite) => Diff::same(to_doc(ctx, t1)),
        (FlexVar(x), FlexVar(y)) if x == y => Diff::same(to_doc(ctx, t1)),
        (FlexSuper(_, x), FlexSuper(_, y)) if x == y => Diff::same(to_doc(ctx, t1)),
        (RigidVar(x), RigidVar(y)) if x == y => Diff::same(to_doc(ctx, t1)),
        (RigidSuper(_, x), RigidSuper(_, y)) if x == y => Diff::same(to_doc(ctx, t1)),

        (FlexVar(_), _) | (_, FlexVar(_)) => {
            Diff::similar(to_doc(ctx, t1), to_doc(ctx, t2))
        }
        (FlexSuper(s, _), t) if t.is_super(*s) => Diff::similar(to_doc(ctx, t1), to_doc(ctx, t2)),
        (t, FlexSuper(s, _)) if t.is_super(*s) => Diff::similar(to_doc(ctx, t1), to_doc(ctx, t2)),

        (Lambda(a, b, cs), Lambda(x, y, zs)) => {
            if cs.len() == zs.len() {
                let da = to_diff(Ctx::Func, a, x);
                let db = to_diff(Ctx::Func, b, y);
                let rest: Vec<Diff> =
                    cs.iter().zip(zs).map(|(c, z)| to_diff(Ctx::Func, c, z)).collect();
                let status = rest.iter().fold(merge(da.problems.clone(), db.problems.clone()), |acc, d| {
                    merge(acc, d.problems.clone())
                });
                Diff {
                    left: lambda_doc(
                        ctx,
                        da.left,
                        db.left,
                        rest.iter().map(|d| d.left.clone()).collect(),
                    ),
                    right: lambda_doc(
                        ctx,
                        da.right,
                        db.right,
                        rest.iter().map(|d| d.right.clone()).collect(),
                    ),
                    problems: status,
                }
            } else {
                Diff::different(
                    to_doc(ctx, t1),
                    to_doc(ctx, t2),
                    vec![Problem::ArityMismatch(2 + cs.len(), 2 + zs.len())],
                )
            }
        }

        (Tuple(a, b, c), Tuple(x, y, z)) if c.is_none() == z.is_none() => {
            let da = to_diff(Ctx::None, a, x);
            let db = to_diff(Ctx::None, b, y);
            let dc = match (c, z) {
                (Some(c), Some(z)) => Some(to_diff(Ctx::None, c, z)),
                _ => None,
            };
            let status = merge(
                merge(da.problems.clone(), db.problems.clone()),
                dc.as_ref().and_then(|d| d.problems.clone()),
            );
            Diff {
                left: tuple_doc(da.left, db.left, dc.iter().map(|d| d.left.clone()).collect()),
                right: tuple_doc(da.right, db.right, dc.iter().map(|d| d.right.clone()).collect()),
                problems: status,
            }
        }

        (Record(f1, e1), Record(f2, e2)) => diff_record(f1, e1, f2, e2),

        (Type(h1, n1, a1), Type(h2, n2, a2)) if h1 == h2 && n1 == n2 && a1.len() == a2.len() => {
            let args: Vec<Diff> =
                a1.iter().zip(a2).map(|(x, y)| to_diff(Ctx::App, x, y)).collect();
            let status = args.iter().fold(None, |acc, d| merge(acc, d.problems.clone()));
            Diff {
                left: apply_doc(
                    ctx,
                    Doc::text(n1.clone()),
                    args.iter().map(|d| d.left.clone()).collect(),
                ),
                right: apply_doc(
                    ctx,
                    Doc::text(n2.clone()),
                    args.iter().map(|d| d.right.clone()).collect(),
                ),
                problems: status,
            }
        }

        (Type(h, n, args), t2) if h == "Maybe" && n == "Maybe" && args.len() == 1 && to_diff(ctx, &args[0], t2).is_similar() => {
            Diff::different(to_doc(ctx, t1), to_doc(ctx, t2), vec![Problem::AnythingFromMaybe])
        }
        (t1v, Type(h, n, args)) if h == "List" && n == "List" && args.len() == 1 && to_diff(ctx, t1v, &args[0]).is_similar() => {
            Diff::different(to_doc(ctx, t1), to_doc(ctx, t2), vec![])
        }

        _ => {
            let left = to_doc(ctx, t1);
            let right = to_doc(ctx, t2);
            let problems = match (t1, t2) {
                (RigidVar(x), other) => vec![Problem::BadRigidVar(x.clone(), other.clone())],
                (FlexSuper(s, x), other) => {
                    vec![Problem::BadFlexSuper(Direction::Have, *s, x.clone(), other.clone())]
                }
                (RigidSuper(s, x), other) => {
                    vec![Problem::BadRigidSuper(*s, x.clone(), other.clone())]
                }
                (other, RigidVar(x)) => vec![Problem::BadRigidVar(x.clone(), other.clone())],
                (other, FlexSuper(s, x)) => {
                    vec![Problem::BadFlexSuper(Direction::Need, *s, x.clone(), other.clone())]
                }
                (other, RigidSuper(s, x)) => {
                    vec![Problem::BadRigidSuper(*s, x.clone(), other.clone())]
                }
                (a @ Type(_, _, l), b @ Type(_, _, r)) if l.is_empty() && r.is_empty() => {
                    if a.is_int() && b.is_float() || a.is_float() && b.is_int() {
                        vec![Problem::IntFloat]
                    } else if a.is_int() && b.is_string() {
                        vec![Problem::StringFromInt]
                    } else if a.is_float() && b.is_string() {
                        vec![Problem::StringFromFloat]
                    } else if a.is_string() && b.is_int() {
                        vec![Problem::StringToInt]
                    } else if a.is_string() && b.is_float() {
                        vec![Problem::StringToFloat]
                    } else if b.is_bool() {
                        vec![Problem::AnythingToBool]
                    } else {
                        vec![]
                    }
                }
                _ => vec![],
            };
            Diff::different(left, right, problems)
        }
    }
}

fn has_fixed_fields(ext: &Extension) -> bool {
    matches!(ext, Extension::Closed | Extension::RigidOpen(_))
}

fn diff_record(
    fields1: &BTreeMap<String, ErrorType>,
    ext1: &Extension,
    fields2: &BTreeMap<String, ErrorType>,
    ext2: &Extension,
) -> Diff {
    let only1: Vec<&String> = fields1.keys().filter(|k| !fields2.contains_key(*k)).collect();
    let only2: Vec<&String> = fields2.keys().filter(|k| !fields1.contains_key(*k)).collect();

    let mut left_entries: Vec<(Doc, Doc)> = Vec::new();
    let mut right_entries: Vec<(Doc, Doc)> = Vec::new();
    let mut status: Option<Vec<Problem>> = None;
    if only1.is_empty() && only2.is_empty() {
        for (name, t1) in fields1 {
            let d = to_diff(Ctx::None, t1, &fields2[name]);
            status = merge(status, d.problems.clone());
            left_entries.push((Doc::text(name.clone()), d.left));
            right_entries.push((Doc::text(name.clone()), d.right));
        }
    } else {
        // elm unions the shared fields with each side's unique ones, so both
        // sides list their own fields in name order.
        let mut left: BTreeMap<&String, Doc> = BTreeMap::new();
        let mut right: BTreeMap<&String, Doc> = BTreeMap::new();
        for (name, t1) in fields1 {
            if let Some(t2) = fields2.get(name) {
                let d = to_diff(Ctx::None, t1, t2);
                status = merge(status, d.problems.clone());
                left.insert(name, d.left);
                right.insert(name, d.right);
            } else {
                left.insert(name, to_doc(Ctx::None, t1));
            }
        }
        for (name, t2) in fields2 {
            if !fields1.contains_key(name) {
                right.insert(name, to_doc(Ctx::None, t2));
            }
        }
        status = merge(status, Some(vec![]));
        left_entries = left.into_iter().map(|(n, d)| (Doc::text(n.clone()), d)).collect();
        right_entries = right.into_iter().map(|(n, d)| (Doc::text(n.clone()), d)).collect();
    }

    let ext_status = ext_to_status(ext1, ext2);
    let problems = match (has_fixed_fields(ext1), has_fixed_fields(ext2)) {
        (true, true) => match only1.first() {
            Some(f) => Some(vec![Problem::FieldTypo(
                (*f).clone(),
                fields2.keys().cloned().collect(),
            )]),
            None if only2.is_empty() => None,
            None => Some(vec![Problem::FieldsMissing(
                only2.iter().map(|s| (*s).clone()).collect(),
            )]),
        },
        (false, true) => only1.first().map(|f| {
            vec![Problem::FieldTypo((*f).clone(), fields2.keys().cloned().collect())]
        }),
        (true, false) => only2.first().map(|f| {
            vec![Problem::FieldTypo((*f).clone(), fields1.keys().cloned().collect())]
        }),
        (false, false) => None,
    };

    Diff {
        left: record_doc(left_entries, ext_to_doc(ext1)),
        right: record_doc(right_entries, ext_to_doc(ext2)),
        problems: merge(merge(status, ext_status), problems),
    }
}

fn ext_to_status(ext1: &Extension, ext2: &Extension) -> Option<Vec<Problem>> {
    match (ext1, ext2) {
        (Extension::Closed, Extension::Closed | Extension::FlexOpen(_)) => None,
        (Extension::Closed, Extension::RigidOpen(_)) => Some(vec![]),
        (Extension::FlexOpen(_), _) => None,
        (Extension::RigidOpen(_), Extension::Closed) => Some(vec![]),
        (Extension::RigidOpen(_), Extension::FlexOpen(_)) => None,
        (Extension::RigidOpen(x), Extension::RigidOpen(y)) => {
            if x == y {
                None
            } else {
                Some(vec![Problem::BadRigidVar(x.clone(), ErrorType::RigidVar(y.clone()))])
            }
        }
    }
}

// ------------------------------------------------------ categories & contexts

/// What kind of expression produced the type we found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Category {
    List,
    Number,
    Float,
    String,
    Char,
    If,
    Case,
    CallResult(MaybeName),
    Lambda,
    Accessor(String),
    Access(String),
    Record,
    Tuple,
    Unit,
    Local(String),
    Foreign(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MaybeName {
    FuncName(String),
    CtorName(String),
    OpName(String),
    NoName,
}

/// Where the expectation came from.
#[derive(Debug, Clone)]
pub enum Expected {
    NoExpectation(ErrorType),
    FromContext(Region, Context, ErrorType),
    /// definition name, arity, sub-context, expected type.
    FromAnnotation(String, usize, SubContext, ErrorType),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubContext {
    TypedIfBranch(usize),
    TypedCaseBranch(usize),
    TypedBody,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Context {
    ListEntry(usize),
    Negate,
    OpLeft(String),
    OpRight(String),
    IfCondition,
    IfBranch(usize),
    CaseBranch(usize),
    CallArity(MaybeName, usize),
    CallArg(MaybeName, usize),
    /// The record's region, the name it was written as, the field's region and
    /// the field being read.
    RecordAccess(Region, Option<String>, Region, String),
    /// The record being updated, and the fields the update mentions with the
    /// region of each.
    RecordUpdateKeys(String, Vec<(String, Region)>),
    RecordUpdateValue(String),
    Destructure,
}

// ------------------------------------------------------------------- wording

/// `D.ordinal` — 1st, 2nd, 3rd, …
fn ordinal(index: usize) -> String {
    match index {
        1 => "1st".to_string(),
        2 => "2nd".to_string(),
        3 => "3rd".to_string(),
        4 => "4th".to_string(),
        5 => "5th".to_string(),
        6 => "6th".to_string(),
        7 => "7th".to_string(),
        8 => "8th".to_string(),
        9 => "9th".to_string(),
        n => format!("{n}th"),
    }
}

/// `D.args` — "1 argument" / "3 arguments".
fn args_word(n: usize) -> String {
    if n == 1 {
        "1 argument".to_string()
    } else {
        format!("{n} arguments")
    }
}

fn add_category(this_is: &str, category: &Category) -> String {
    match category {
        Category::Local(name) | Category::Foreign(name) => format!("This `{name}` value is a:"),
        Category::Access(field) => format!("The value at .{field} is a:"),
        Category::Accessor(field) => format!("This .{field} field access function has type:"),
        Category::If => "This `if` expression produces:".to_string(),
        Category::Case => "This `case` expression produces:".to_string(),
        Category::List => format!("{this_is} a list of type:"),
        Category::Number => format!("{this_is} a number of type:"),
        Category::Float => format!("{this_is} a float of type:"),
        Category::String => format!("{this_is} a string of type:"),
        Category::Char => format!("{this_is} a character of type:"),
        Category::Lambda => format!("{this_is} an anonymous function of type:"),
        Category::Record => format!("{this_is} a record of type:"),
        Category::Tuple => format!("{this_is} a tuple of type:"),
        Category::Unit => format!("{this_is} a unit value:"),
        Category::CallResult(name) => match name {
            MaybeName::NoName | MaybeName::OpName(_) => format!("{this_is}:"),
            MaybeName::FuncName(n) | MaybeName::CtorName(n) => format!("This `{n}` call produces:"),
        },
    }
}

fn problems_to_hint(problems: &[Problem]) -> Vec<Section> {
    match problems.first() {
        None => vec![],
        Some(problem) => problem_to_hint(problem),
    }
}

fn problem_to_hint(problem: &Problem) -> Vec<Section> {
    let hint = |text: &str| vec![Section::Para(format!("Hint: {text}"))];
    match problem {
        Problem::IntFloat => vec![Section::Para(
            "Note: Read <https://elm-lang.org/0.19.1/implicit-casts> to learn why Elm does not \
             implicitly convert Ints to Floats. Use toFloat and round to do explicit conversions."
                .to_string(),
        )],
        Problem::StringFromInt => {
            hint("Want to convert an Int into a String? Use the String.fromInt function!")
        }
        Problem::StringFromFloat => {
            hint("Want to convert a Float into a String? Use the String.fromFloat function!")
        }
        Problem::StringToInt => {
            hint("Want to convert a String into an Int? Use the String.toInt function!")
        }
        Problem::StringToFloat => {
            hint("Want to convert a String into a Float? Use the String.toFloat function!")
        }
        Problem::AnythingToBool => hint(
            "Elm does not have “truthiness” such that ints and strings and lists are\
             \u{a0}automatically converted to booleans. Do that conversion explicitly!",
        ),
        Problem::AnythingFromMaybe => hint(
            "Use Maybe.withDefault to handle possible errors. Longer term, it is usually\
             \u{a0}better to write out the full `case` though!",
        ),
        Problem::ArityMismatch(x, y) => hint(&if x < y {
            format!("It looks like it takes too few arguments. I was expecting {} more.", y - x)
        } else {
            format!("It looks like it takes too many arguments. I see {} extra.", x - y)
        }),
        Problem::FieldTypo(typo, possibilities) => {
            match nearby_names(typo, possibilities).first() {
                Some(nearest) => vec![
                    Section::Para(format!(
                        "Hint: Seems like a record field typo. Maybe {typo} should be {nearest}?"
                    )),
                    Section::Para(
                        "Hint: Can more type annotations be added? Type annotations always help me                          give more specific messages, and I think they could help a lot in this                          case!"
                            .to_string(),
                    ),
                ],
                None => vec![],
            }
        }
        Problem::FieldsMissing(fields) => match fields.split_first() {
            None => vec![],
            Some((f, [])) => hint(&format!("Looks like the {f} field is missing.")),
            Some(_) => hint(&format!(
                "Looks like fields {} are missing.",
                comma_sep("and", fields)
            )),
        },
        Problem::BadFlexSuper(direction, super_, _, tipe) => match tipe {
            ErrorType::Infinite | ErrorType::Error | ErrorType::FlexVar(_) => vec![],
            ErrorType::FlexSuper(s, _) => bad_flex_flex_super(*super_, *s),
            ErrorType::RigidVar(y) => bad_rigid_var(y, &a_super_thing(*super_)),
            ErrorType::RigidSuper(s, _) => bad_rigid_super(*s, &a_super_thing(*super_)),
            _ => bad_flex_super(*direction, *super_, tipe),
        },
        Problem::BadRigidVar(x, tipe) => match tipe {
            ErrorType::Lambda(..) => bad_rigid_var(x, "a function"),
            ErrorType::Infinite | ErrorType::Error | ErrorType::FlexVar(_) => vec![],
            ErrorType::FlexSuper(s, _) => bad_rigid_var(x, &a_super_thing(*s)),
            ErrorType::RigidVar(y) | ErrorType::RigidSuper(_, y) => bad_double_rigid(x, y),
            ErrorType::Type(_, n, _) => bad_rigid_var(x, &format!("a `{n}` value")),
            ErrorType::Record(..) => bad_rigid_var(x, "a record"),
            ErrorType::Unit => bad_rigid_var(x, "a unit value"),
            ErrorType::Tuple(..) => bad_rigid_var(x, "a tuple"),
        },
        Problem::BadRigidSuper(super_, x, tipe) => match tipe {
            ErrorType::Lambda(..) => bad_rigid_super(*super_, "a function"),
            ErrorType::Infinite | ErrorType::Error | ErrorType::FlexVar(_) => vec![],
            ErrorType::FlexSuper(s, _) => bad_rigid_super(*super_, &a_super_thing(*s)),
            ErrorType::RigidVar(y) | ErrorType::RigidSuper(_, y) => bad_double_rigid(x, y),
            ErrorType::Type(_, n, _) => bad_rigid_super(*super_, &format!("a `{n}` value")),
            ErrorType::Record(..) => bad_rigid_super(*super_, "a record"),
            ErrorType::Unit => bad_rigid_super(*super_, "a unit value"),
            ErrorType::Tuple(..) => bad_rigid_super(*super_, "a tuple"),
        },
    }
}

/// `D.commaSep` — "a, b, and c" (and "a and b" for two).
fn comma_sep(conjunction: &str, items: &[String]) -> String {
    match items {
        [] => String::new(),
        [a] => a.clone(),
        [a, b] => format!("{a} {conjunction} {b}"),
        _ => {
            let (last, rest) = items.split_last().unwrap();
            format!("{}, {conjunction} {last}", rest.join(", "))
        }
    }
}

fn a_super_thing(super_: Super) -> String {
    match super_ {
        Super::Number => "a `number` value",
        Super::Comparable => "a `comparable` value",
        Super::CompAppend => "a `compappend` value",
        Super::Appendable => "an `appendable` value",
    }
    .to_string()
}

fn bad_rigid_var(name: &str, a_thing: &str) -> Vec<Section> {
    vec![
        Section::Para(format!(
            "Hint: Your type annotation uses type variable `{name}` which means ANY type of value              can flow through, but your code is saying it specifically wants {a_thing}. Maybe              change your type annotation to be more specific? Maybe change the code to be more              general?"
        )),
        Section::Para(
            "Read <https://elm-lang.org/0.19.1/type-annotations> for more advice!".to_string(),
        ),
    ]
}

fn bad_double_rigid(x: &str, y: &str) -> Vec<Section> {
    vec![
        Section::Para(format!(
            "Hint: Your type annotation uses `{x}` and `{y}` as separate type variables. Your code              seems to be saying they are the same though. Maybe they should be the same in your              type annotation? Maybe your code uses them in a weird way?"
        )),
        Section::Para(
            "Read <https://elm-lang.org/0.19.1/type-annotations> for more advice!".to_string(),
        ),
    ]
}

fn bad_rigid_super(super_: Super, a_thing: &str) -> Vec<Section> {
    let (super_type, many_things) = match super_ {
        Super::Number => ("number", "ints AND floats"),
        Super::Comparable => ("comparable", "ints, floats, chars, strings, lists, and tuples"),
        Super::Appendable => ("appendable", "strings AND lists"),
        Super::CompAppend => ("compappend", "strings AND lists"),
    };
    vec![
        Section::Para(format!(
            "Hint: The `{super_type}` in your type annotation is saying that {many_things} can flow              through, but your code is saying it specifically wants {a_thing}. Maybe change your              type annotation to be more specific? Maybe change the code to be more general?"
        )),
        Section::Para(
            "Read <https://elm-lang.org/0.19.1/type-annotations> for more advice!".to_string(),
        ),
    ]
}

fn bad_flex_flex_super(s1: Super, s2: Super) -> Vec<Section> {
    let like_this = |s: Super| match s {
        Super::Number => "a number",
        Super::Comparable => "comparable",
        Super::CompAppend => "a compappend",
        Super::Appendable => "appendable",
    };
    vec![Section::Para(format!(
        "Hint: There are no values in Elm that are both {} and {}.",
        like_this(s1),
        like_this(s2)
    ))]
}

fn bad_flex_super(direction: Direction, super_: Super, tipe: &ErrorType) -> Vec<Section> {
    match super_ {
        Super::Comparable => match tipe {
            ErrorType::Record(..) => vec![Section::Para(
                "Hint: I do not know how to compare records. I can only compare ints, floats,                  chars, strings, lists of comparable values, and tuples of comparable values. Check                  out <https://elm-lang.org/0.19.1/comparing-records> for ideas on how to proceed."
                    .to_string(),
            )],
            ErrorType::Type(_, name, _) => vec![
                Section::Para(format!(
                    "Hint: I do not know how to compare `{name}` values. I can only compare ints,                      floats, chars, strings, lists of comparable values, and tuples of comparable                      values."
                )),
                Section::Para(
                    "Check out <https://elm-lang.org/0.19.1/comparing-custom-types> for ideas on                      how to proceed."
                        .to_string(),
                ),
            ],
            _ => vec![Section::Para(
                "Hint: I only know how to compare ints, floats, chars, strings, lists of                  comparable values, and tuples of comparable values."
                    .to_string(),
            )],
        },
        Super::Appendable => vec![Section::Para(
            "Hint: I only know how to append strings and lists.".to_string(),
        )],
        Super::CompAppend => vec![Section::Para(
            "Hint: Only strings and lists are both comparable and appendable.".to_string(),
        )],
        Super::Number => match tipe {
            ErrorType::Type(..) if tipe.is_string() => match direction {
                Direction::Have => vec![Section::Para(
                    "Hint: Try using String.fromInt to convert it to a string?".to_string(),
                )],
                Direction::Need => vec![Section::Para(
                    "Hint: Try using String.toInt to convert it to an integer?".to_string(),
                )],
            },
            _ => vec![Section::Para(
                "Hint: Only Int and Float values work as numbers.".to_string(),
            )],
        },
    }
}

/// `Data.Utf8.Suggest.sort` — candidates ordered by edit distance to `target`.
fn nearby_names(target: &str, candidates: &[String]) -> Vec<String> {
    let mut scored: Vec<(usize, &String)> =
        candidates.iter().map(|c| (distance(target, c), c)).collect();
    scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
    scored.into_iter().map(|(_, c)| c.clone()).collect()
}

fn distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut current = vec![0; b.len() + 1];
    for i in 1..=a.len() {
        current[0] = i;
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            current[j] = (prev[j] + 1).min(current[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut current);
    }
    prev[b.len()]
}

// -------------------------------------------------------------- the report

/// `typeComparison`: what I found, what was wanted, then any hints.
fn type_comparison(
    actual: &ErrorType,
    expected: &ErrorType,
    i_am_seeing: String,
    instead_of: String,
    context_hints: Vec<Section>,
) -> (String, Vec<Section>) {
    let (actual_doc, expected_doc, problems) = to_comparison(actual, expected);
    let mut notes = vec![
        Section::Block(actual_doc),
        Section::Para(instead_of),
        Section::Block(expected_doc),
    ];
    notes.extend(context_hints);
    notes.extend(problems_to_hint(&problems));
    (i_am_seeing, notes)
}

/// `loneType`: only what I found, then further details.
fn lone_type(
    actual: &ErrorType,
    expected: &ErrorType,
    i_am_seeing: String,
    further: Vec<Section>,
) -> (String, Vec<Section>) {
    let (actual_doc, _, problems) = to_comparison(actual, expected);
    let mut notes = vec![Section::Block(actual_doc)];
    notes.extend(further);
    notes.extend(problems_to_hint(&problems));
    (i_am_seeing, notes)
}

fn report(title: &str, expr_region: Region, region: Region, before: String, after: String, notes: Vec<Section>) -> Report {
    Report {
        title: title.to_string(),
        region: expr_region,
        message: String::new(),
        elm: Some(ElmBody { before, after, notes, region, highlight: expr_region }),
    }
}

/// `Reporting.Error.Type.toExprReport`.
pub fn to_expr_report(
    expr_region: Region,
    category: &Category,
    tipe: &ErrorType,
    expected: &Expected,
) -> Report {
    match expected {
        Expected::NoExpectation(expected_type) => {
            let (after, notes) = type_comparison(
                tipe,
                expected_type,
                add_category("It is", category),
                "But you are trying to use it as:".to_string(),
                vec![],
            );
            report(
                "TYPE MISMATCH",
                expr_region,
                expr_region,
                "This expression is being used in an unexpected way:".to_string(),
                after,
                notes,
            )
        }

        Expected::FromAnnotation(name, _arity, sub_context, expected_type) => {
            let thing = match sub_context {
                SubContext::TypedIfBranch(i) => {
                    format!("{} branch of this `if` expression:", ordinal(*i))
                }
                SubContext::TypedCaseBranch(i) => {
                    format!("{} branch of this `case` expression:", ordinal(*i))
                }
                SubContext::TypedBody => format!("body of the `{name}` definition:"),
            };
            let it_is = match sub_context {
                SubContext::TypedIfBranch(i) | SubContext::TypedCaseBranch(i) => {
                    format!("The {} branch is", ordinal(*i))
                }
                SubContext::TypedBody => "The body is".to_string(),
            };
            let (after, notes) = type_comparison(
                tipe,
                expected_type,
                add_category(&it_is, category),
                format!("But the type annotation on `{name}` says it should be:"),
                vec![],
            );
            report(
                "TYPE MISMATCH",
                expr_region,
                expr_region,
                format!("Something is off with the {thing}"),
                after,
                notes,
            )
        }

        Expected::FromContext(region, context, expected_type) => {
            let mismatch = |problem: String, this_is: String, instead_of: String, further: Vec<Section>| {
                let (after, notes) =
                    type_comparison(tipe, expected_type, add_category(&this_is, category), instead_of, further);
                report("TYPE MISMATCH", expr_region, *region, problem, after, notes)
            };
            let bad_type = |problem: String, this_is: String, further: Vec<Section>| {
                let (after, notes) =
                    lone_type(tipe, expected_type, add_category(&this_is, category), further);
                report("TYPE MISMATCH", expr_region, *region, problem, after, notes)
            };

            match context {
                Context::ListEntry(index) => {
                    let ith = ordinal(*index);
                    mismatch(
                        format!("The {ith} element of this list does not match all the previous elements:"),
                        format!("The {ith} element is"),
                        "But all the previous elements in the list are:".to_string(),
                        vec![Section::Para(
                            "Hint: Everything in a list must be the same type of value. This way, we \
                             never run into unexpected values partway through a List.map, List.foldl, \
                             etc. Read <https://elm-lang.org/0.19.1/custom-types> to learn how to \
                             “mix” types."
                                .to_string(),
                        )],
                    )
                }

                Context::Negate => bad_type(
                    "I do not know how to negate this type of value:".to_string(),
                    "It is".to_string(),
                    vec![Section::Para(
                        "But I only now how to negate Int and Float values.".to_string(),
                    )],
                ),

                Context::IfCondition => bad_type(
                    "This `if` condition does not evaluate to a boolean value, True or False."
                        .to_string(),
                    "It is".to_string(),
                    vec![Section::Para(
                        "But I need this `if` condition to be a Bool value.".to_string(),
                    )],
                ),

                Context::IfBranch(index) => {
                    let ith = ordinal(*index);
                    mismatch(
                        format!("The {ith} branch of this `if` does not match all the previous branches:"),
                        format!("The {ith} branch is"),
                        "But all the previous branches result in:".to_string(),
                        vec![Section::Para(
                            "Hint: All branches in an `if` must produce the same type of values. This \
                             way, no matter which branch we take, the result is always a consistent \
                             shape. Read <https://elm-lang.org/0.19.1/custom-types> to learn how to \
                             “mix” types."
                                .to_string(),
                        )],
                    )
                }

                Context::CaseBranch(index) => {
                    let ith = ordinal(*index);
                    mismatch(
                        format!("The {ith} branch of this `case` does not match all the previous branches:"),
                        format!("The {ith} branch is"),
                        "But all the previous branches result in:".to_string(),
                        vec![Section::Para(
                            "Hint: All branches in a `case` must produce the same type of values. This \
                             way, no matter which branch we take, the result is always a consistent \
                             shape. Read <https://elm-lang.org/0.19.1/custom-types> to learn how to \
                             “mix” types."
                                .to_string(),
                        )],
                    )
                }

                Context::CallArity(maybe_name, given) => {
                    let arity = tipe.arity();
                    let (before, after) = if arity == 0 {
                        let this_value = match maybe_name {
                            MaybeName::NoName => "This value".to_string(),
                            MaybeName::FuncName(n) | MaybeName::CtorName(n) => {
                                format!("The `{n}` value")
                            }
                            MaybeName::OpName(op) => format!("The ({op}) operator"),
                        };
                        (
                            format!(
                                "{this_value} is not a function, but it was given {}.",
                                args_word(*given)
                            ),
                            "Are there any missing commas? Or missing parentheses?".to_string(),
                        )
                    } else {
                        let this_function = match maybe_name {
                            MaybeName::NoName => "This function".to_string(),
                            MaybeName::FuncName(n) => format!("The `{n}` function"),
                            MaybeName::CtorName(n) => format!("The `{n}` constructor"),
                            MaybeName::OpName(op) => format!("The ({op}) operator"),
                        };
                        (
                            format!(
                                "{this_function} expects {}, but it got {given} instead.",
                                args_word(arity)
                            ),
                            "Are there any missing commas? Or missing parentheses?".to_string(),
                        )
                    };
                    report("TOO MANY ARGS", expr_region, *region, before, after, vec![])
                }

                Context::CallArg(maybe_name, index) => {
                    let ith = ordinal(*index);
                    let this_function = match maybe_name {
                        MaybeName::NoName => "this function".to_string(),
                        MaybeName::FuncName(n) | MaybeName::CtorName(n) => format!("`{n}`"),
                        MaybeName::OpName(op) => format!("({op})"),
                    };
                    let further = if *index == 1 {
                        vec![]
                    } else {
                        vec![Section::Para(
                            "Hint: I always figure out the argument types from left to right. If an \
                             argument is acceptable, I assume it is “correct” and move on. So the \
                             problem may actually be in one of the previous arguments!"
                                .to_string(),
                        )]
                    };
                    mismatch(
                        format!("The {ith} argument to {this_function} is not what I expect:"),
                        "This argument is".to_string(),
                        format!("But {this_function} needs the {ith} argument to be:"),
                        further,
                    )
                }

                Context::OpLeft(op) => op_left_report(expr_region, *region, category, op, tipe, expected_type),
                Context::OpRight(op) => op_right_report(expr_region, *region, category, op, tipe, expected_type),

                Context::RecordAccess(record_region, maybe_name, field_region, field) => {
                    let named = match maybe_name {
                        Some(n) => format!("`{n}` "),
                        None => String::new(),
                    };
                    match tipe {
                        ErrorType::Record(fields, ext) => {
                            let (after, notes) = if fields.is_empty() {
                                ("In fact, it is a record with NO fields!".to_string(), vec![])
                            } else {
                                let nearest =
                                    nearby_names(field, &fields.keys().cloned().collect::<Vec<_>>())
                                        .remove(0);
                                (
                                    format!(
                                        "This is usually a typo. Here are the {named}fields that are most similar:"
                                    ),
                                    vec![
                                        Section::Block(nearby_record(fields, field, ext)),
                                        Section::Para(format!("So maybe {field} should be {nearest}?")),
                                    ],
                                )
                            };
                            report(
                                "TYPE MISMATCH",
                                *field_region,
                                *region,
                                format!("This {named}record does not have a `{field}` field:"),
                                after,
                                notes,
                            )
                        }
                        _ => {
                            let (after, notes) = lone_type(
                                tipe,
                                expected_type,
                                add_category("It is", category),
                                vec![Section::Para(format!(
                                    "But I need a record with a {field} field!"
                                ))],
                            );
                            report(
                                "TYPE MISMATCH",
                                *record_region,
                                *region,
                                "This is not a record, so it has no fields to access!".to_string(),
                                after,
                                notes,
                            )
                        }
                    }
                }

                Context::RecordUpdateKeys(record, expected_fields) => match tipe {
                    ErrorType::Record(actual_fields, ext) => {
                        match expected_fields.iter().find(|(f, _)| !actual_fields.contains_key(f)) {
                            None => mismatch(
                                "Something is off with this record update:".to_string(),
                                format!("The `{record}` record is"),
                                "But this update needs it to be compatable with:".to_string(),
                                vec![Section::Para(
                                    "Do you mind creating an <http://sscce.org/> that produces this                                      error message and sharing it at                                      <https://github.com/elm/error-message-catalog/issues> so we can                                      try to give better advice here?"
                                        .to_string(),
                                )],
                            ),
                            Some((field, field_region)) => {
                                let (after, notes) = if actual_fields.is_empty() {
                                    (
                                        format!("In fact, `{record}` is a record with NO fields!"),
                                        vec![],
                                    )
                                } else {
                                    let nearest = nearby_names(
                                        field,
                                        &actual_fields.keys().cloned().collect::<Vec<_>>(),
                                    )
                                    .remove(0);
                                    (
                                        format!(
                                            "This is usually a typo. Here are the `{record}` fields that are most similar:"
                                        ),
                                        vec![
                                            Section::Block(nearby_record(actual_fields, field, ext)),
                                            Section::Para(format!(
                                                "So maybe {field} should be {nearest}?"
                                            )),
                                        ],
                                    )
                                };
                                report(
                                    "TYPE MISMATCH",
                                    *field_region,
                                    *region,
                                    format!("The `{record}` record does not have a `{field}` field:"),
                                    after,
                                    notes,
                                )
                            }
                        }
                    }
                    _ => bad_type(
                        "This is not a record, so it has no fields to update!".to_string(),
                        "It is".to_string(),
                        vec![Section::Para("But I need a record!".to_string())],
                    ),
                },

                Context::RecordUpdateValue(field) => mismatch(
                    format!("I cannot update the `{field}` field like this:"),
                    format!("You are trying to update `{field}` to be"),
                    "But it should be:".to_string(),
                    vec![Section::Para(
                        "Note: The record update syntax does not allow you to change the type of                          fields. You can achieve that with record constructors or the record                          literal syntax."
                            .to_string(),
                    )],
                ),

                Context::Destructure => {
                    let (after, notes) = type_comparison(
                        tipe,
                        expected_type,
                        add_category("This is", category),
                        "But you are trying to destructure it as:".to_string(),
                        vec![],
                    );
                    report(
                        "TYPE MISMATCH",
                        expr_region,
                        *region,
                        "This definition is trying to destructure an incompatible value:".to_string(),
                        after,
                        notes,
                    )
                }
            }
        }
    }
}

/// The `(before, after, notes)` triple an operator report renders to.
type Docs = (String, String, Vec<Section>);

/// `opLeftToDocs`.
fn op_left_report(
    expr_region: Region,
    region: Region,
    category: &Category,
    op: &str,
    tipe: &ErrorType,
    expected: &ErrorType,
) -> Report {
    let (before, after, notes) = match op {
        "+" if tipe.is_string() => bad_string_add(),
        "+" if is_list1(tipe) => bad_list_add(category, "left", tipe, expected),
        "+" => bad_math(category, "Addition", "left", "+", tipe, expected, vec![]),
        "*" if is_list1(tipe) => bad_list_mul(category, "left", tipe, expected),
        "*" => bad_math(category, "Multiplication", "left", "*", tipe, expected, vec![]),
        "-" => bad_math(category, "Subtraction", "left", "-", tipe, expected, vec![]),
        "^" => bad_math(category, "Exponentiation", "left", "^", tipe, expected, vec![]),
        "/" => bad_fdiv("left", tipe, expected),
        "//" => bad_idiv("left", tipe, expected),
        "&&" | "||" => bad_bool(op, "left", tipe, expected),
        "<" | ">" | "<=" | ">=" => bad_comp_left(category, op, "left", tipe, expected),
        "++" => bad_append_left(category, tipe, expected),
        "<|" => {
            let (after, notes) = lone_type(
                tipe,
                expected,
                add_category("I am seeing", category),
                vec![Section::Para("This needs to be some kind of function though!".to_string())],
            );
            (
                "The left side of (<|) needs to be a function so I can pipe arguments to it!"
                    .to_string(),
                after,
                notes,
            )
        }
        _ => {
            let (after, notes) = type_comparison(
                tipe,
                expected,
                add_category("The left argument is", category),
                format!("But ({op}) needs the left argument to be:"),
                vec![],
            );
            (format!("The left argument of ({op}) is causing problems:"), after, notes)
        }
    };
    report("TYPE MISMATCH", expr_region, region, before, after, notes)
}

/// `opRightToDocs`. `EmphBoth` reports draw no carets (elm passes no highlight),
/// `EmphRight` underlines the right operand.
fn op_right_report(
    expr_region: Region,
    region: Region,
    category: &Category,
    op: &str,
    tipe: &ErrorType,
    expected: &ErrorType,
) -> Report {
    let cast = |op: &str, float_then_int: bool| -> (bool, Docs) {
        let (seen_left, seen_right, fix_left, fix_right) = if float_then_int {
            ("a Float", "an Int", "round", "toFloat")
        } else {
            ("an Int", "a Float", "toFloat", "round")
        };
        (
            false,
            (
                format!(
                    "I need both sides of ({op}) to be the exact same type. Both Int or both Float."
                ),
                format!("But I see {seen_left} on the left and {seen_right} on the right."),
                vec![
                    Section::Para(format!(
                        "Use {fix_left} on the left (or {fix_right} on the right) to make both sides match!"
                    )),
                    Section::Para(
                        "Note: Read <https://elm-lang.org/0.19.1/implicit-casts> to learn why Elm \
                         does not implicitly convert Ints to Floats."
                            .to_string(),
                    ),
                ],
            ),
        )
    };

    let (emph_right, (before, after, notes)) = match op {
        "+" | "-" | "*" | "^" if expected.is_float() && tipe.is_int() => cast(op, true),
        "+" | "-" | "*" | "^" if expected.is_int() && tipe.is_float() => cast(op, false),
        "+" if tipe.is_string() => (true, bad_string_add()),
        "+" if is_list1(tipe) => (true, bad_list_add(category, "right", tipe, expected)),
        "+" => (true, bad_math(category, "Addition", "right", "+", tipe, expected, vec![])),
        "*" if is_list1(tipe) => (true, bad_list_mul(category, "right", tipe, expected)),
        "*" => (true, bad_math(category, "Multiplication", "right", "*", tipe, expected, vec![])),
        "-" => (true, bad_math(category, "Subtraction", "right", "-", tipe, expected, vec![])),
        "^" => (true, bad_math(category, "Exponentiation", "right", "^", tipe, expected, vec![])),
        "/" => (true, bad_fdiv("right", tipe, expected)),
        "//" => (true, bad_idiv("right", tipe, expected)),
        "&&" | "||" => (true, bad_bool(op, "right", tipe, expected)),
        "<" | ">" | "<=" | ">=" => (false, bad_comp_right(op, tipe, expected)),
        "==" | "/=" => (false, bad_equality(op, tipe, expected)),
        "::" => bad_cons_right(category, tipe, expected),
        "++" => bad_append_right(category, tipe, expected),
        "<|" => {
            let (after, notes) = type_comparison(
                tipe,
                expected,
                "The argument is:".to_string(),
                "But (<|) is piping it to a function that expects:".to_string(),
                vec![],
            );
            (true, ("I cannot send this through the (<|) pipe:".to_string(), after, notes))
        }
        "|>" => match (tipe, expected) {
            (ErrorType::Lambda(expected_arg, _, _), ErrorType::Lambda(arg, _, _)) => {
                let (after, notes) = type_comparison(
                    arg,
                    expected_arg,
                    "The argument is:".to_string(),
                    "But (|>) is piping it to a function that expects:".to_string(),
                    vec![],
                );
                (
                    true,
                    (
                        "This function cannot handle the argument sent through the (|>) pipe:"
                            .to_string(),
                        after,
                        notes,
                    ),
                )
            }
            _ => {
                let (after, notes) = lone_type(
                    tipe,
                    expected,
                    add_category("But instead of a function, I am seeing", category),
                    vec![],
                );
                (
                    true,
                    (
                        "The right side of (|>) needs to be a function so I can pipe arguments to it!"
                            .to_string(),
                        after,
                        notes,
                    ),
                )
            }
        },
        _ => (true, bad_op_right_fallback(category, op, tipe, expected)),
    };
    // elm's `Code.toSnippet` underlines the whole shown region when a report
    // gives no sub-region, so an `EmphBoth` report still draws carets — just
    // under the operator expression rather than one operand.
    let highlight = if emph_right { expr_region } else { region };
    Report {
        title: "TYPE MISMATCH".to_string(),
        region: expr_region,
        message: String::new(),
        elm: Some(ElmBody { before, after, notes, region, highlight }),
    }
}

fn is_list1(tipe: &ErrorType) -> bool {
    matches!(tipe, ErrorType::Type(h, n, args) if h == "List" && n == "List" && args.len() == 1)
}

fn bad_op_right_fallback(category: &Category, op: &str, tipe: &ErrorType, expected: &ErrorType) -> Docs {
    let (after, notes) = type_comparison(
        tipe,
        expected,
        add_category("The right argument is", category),
        format!("But ({op}) needs the right argument to be:"),
        vec![Section::Para(format!(
            "Hint: With operators like ({op}) I always check the left side first. If it seems \
             fine, I assume it is correct and check the right side. So the problem may be in how \
             the left and right arguments interact!"
        ))],
    );
    (format!("The right argument of ({op}) is causing problems."), after, notes)
}

fn bad_string_add() -> Docs {
    (
        "I cannot do addition with String values like this one:".to_string(),
        "The (+) operator only works with Int and Float values.".to_string(),
        vec![Section::Para(
            "Hint: Switch to the (++) operator to append strings!".to_string(),
        )],
    )
}

fn bad_list_add(category: &Category, direction: &str, tipe: &ErrorType, expected: &ErrorType) -> Docs {
    let (after, notes) = lone_type(
        tipe,
        expected,
        add_category(&format!("The {direction} side of (+) is"), category),
        vec![
            Section::Para("But (+) only works with Int and Float values.".to_string()),
            Section::Para("Hint: Switch to the (++) operator to append lists!".to_string()),
        ],
    );
    ("I cannot do addition with lists:".to_string(), after, notes)
}

fn bad_list_mul(category: &Category, direction: &str, tipe: &ErrorType, expected: &ErrorType) -> Docs {
    bad_math(
        category,
        "Multiplication",
        direction,
        "*",
        tipe,
        expected,
        vec![Section::Para(
            "Hint: Maybe you want List.repeat to build a list of repeated values?".to_string(),
        )],
    )
}

fn bad_math(
    category: &Category,
    operation: &str,
    direction: &str,
    op: &str,
    tipe: &ErrorType,
    expected: &ErrorType,
    other_hints: Vec<Section>,
) -> Docs {
    let mut further = vec![Section::Para(format!(
        "But ({op}) only works with Int and Float values."
    ))];
    further.extend(other_hints);
    let (after, notes) = lone_type(
        tipe,
        expected,
        add_category(&format!("The {direction} side of ({op}) is"), category),
        further,
    );
    (format!("{operation} does not work with this value:"), after, notes)
}

fn bad_fdiv(direction: &str, tipe: &ErrorType, expected: &ErrorType) -> Docs {
    if tipe.is_int() {
        (
            "The (/) operator is specifically for floating-point division:".to_string(),
            format!(
                "The {direction} side of (/) must be a Float, but I am seeing an Int. I recommend:"
            ),
            vec![
                Section::Block(
                    "toFloat for explicit conversions     (toFloat 5 / 2) == 2.5\n\
                     (//)    for integer division         (5 // 2)        == 2"
                        .to_string(),
                ),
                Section::Para(
                    "Note: Read <https://elm-lang.org/0.19.1/implicit-casts> to learn why Elm does \
                     not implicitly convert Ints to Floats."
                        .to_string(),
                ),
            ],
        )
    } else {
        let (after, notes) = lone_type(
            tipe,
            expected,
            format!("The {direction} side of (/) must be a Float, but instead I am seeing:"),
            vec![],
        );
        ("The (/) operator is specifically for floating-point division:".to_string(), after, notes)
    }
}

fn bad_idiv(direction: &str, tipe: &ErrorType, expected: &ErrorType) -> Docs {
    if tipe.is_float() {
        (
            "The (//) operator is for integer division:".to_string(),
            format!(
                "The {direction} side of (//) must be an Int, but I am seeing a Float. I recommend:"
            ),
            vec![
                Section::Block(
                    "round for explicit conversions     (round 5.0 // 2) == 2\n\
                     (/)   for floating-point division  (5.0 / 2)        == 2.5"
                        .to_string(),
                ),
                Section::Para(
                    "Note: Read <https://elm-lang.org/0.19.1/implicit-casts> to learn why Elm does \
                     not implicitly convert Ints to Floats."
                        .to_string(),
                ),
            ],
        )
    } else {
        let (after, notes) = lone_type(
            tipe,
            expected,
            format!("The {direction} side of (//) must be an Int, but instead I am seeing:"),
            vec![],
        );
        ("The (//) operator is only for Int values:".to_string(), after, notes)
    }
}

fn bad_bool(op: &str, direction: &str, tipe: &ErrorType, expected: &ErrorType) -> Docs {
    let (after, notes) = lone_type(
        tipe,
        expected,
        format!("The {direction} side of ({op}) must be a Bool, but instead I am seeing:"),
        vec![],
    );
    (format!("I am struggling with this boolean operation:"), after, notes)
}

fn bad_comp_left(category: &Category, op: &str, direction: &str, tipe: &ErrorType, expected: &ErrorType) -> Docs {
    let (after, notes) = lone_type(
        tipe,
        expected,
        add_category(&format!("The {direction} side of ({op}) is"), category),
        vec![Section::Para(format!(
            "But ({op}) only works on Int, Float, Char, and String values. It can work on lists \
             and tuples of comparable values as well, but it is usually better to find a different \
             path."
        ))],
    );
    (format!("I cannot do a comparison with this value:"), after, notes)
}

fn bad_comp_right(op: &str, tipe: &ErrorType, expected: &ErrorType) -> Docs {
    let (after, notes) = type_comparison(
        tipe,
        expected,
        format!("The left side of ({op}) is:"),
        "But the right side is:".to_string(),
        vec![Section::Para(format!(
            "Hint: I always check the left side of an operator first. If it seems fine, I assume \
             it is correct and check the right side. So the problem may be in how the left and \
             right arguments interact!"
        ))],
    );
    (
        format!("I need both sides of ({op}) to be the same type:"),
        after,
        notes,
    )
}

fn bad_equality(op: &str, tipe: &ErrorType, expected: &ErrorType) -> Docs {
    let (after, mut notes) = type_comparison(
        tipe,
        expected,
        format!("The left side of ({op}) is:"),
        "But the right side is:".to_string(),
        vec![],
    );
    notes.push(Section::Para(if tipe.is_float() || expected.is_float() {
        "Note: Equality on floats is not 100% reliable due to the design of IEEE 754. I \
         recommend a check like (abs (x - y) < 0.0001) instead."
            .to_string()
    } else {
        format!(
            "Hint: Did you want to compare these two values? I always check the left side of an \
             operator first. If it seems fine, I assume it is correct and check the right side. So \
             the problem may be in how the left and right arguments interact!"
        )
    }));
    (
        format!("I need both sides of ({op}) to be the same type:"),
        after,
        notes,
    )
}

fn bad_cons_right(category: &Category, tipe: &ErrorType, expected: &ErrorType) -> (bool, Docs) {
    if let ErrorType::Type(h1, n1, actual_args) = tipe {
        if h1 == "List" && n1 == "List" && actual_args.len() == 1 {
            if let ErrorType::Type(h2, n2, expected_args) = expected {
                if h2 == "List" && n2 == "List" && expected_args.len() == 1 {
                    let further = if is_list1(&expected_args[0]) {
                        vec![Section::Para(
                            "Hint: Are you trying to append two lists? The (++) operator appends \
                             lists, whereas the (::) operator is only for adding ONE element to a \
                             list."
                                .to_string(),
                        )]
                    } else {
                        vec![Section::Para(
                            "Lists need ALL elements to be the same type though.".to_string(),
                        )]
                    };
                    let (after, notes) = type_comparison(
                        &expected_args[0],
                        &actual_args[0],
                        "The left side of (::) is:".to_string(),
                        "But you are trying to put that into a list filled with:".to_string(),
                        further,
                    );
                    return (
                        false,
                        ("I am having trouble with this (::) operator:".to_string(), after, notes),
                    );
                }
            }
            return (true, bad_op_right_fallback(category, "::", tipe, expected));
        }
    }
    let (after, notes) = lone_type(
        tipe,
        expected,
        add_category("The right side is", category),
        vec![Section::Para("But (::) needs a List on the right.".to_string())],
    );
    (true, ("The (::) operator can only add elements onto lists.".to_string(), after, notes))
}

/// `toAppendType`: what kind of thing is being appended.
enum AppendType {
    ANumber(&'static str, &'static str),
    AString,
    AList,
    AOther,
}

fn to_append_type(tipe: &ErrorType) -> AppendType {
    match tipe {
        ErrorType::Type(..) if tipe.is_int() => AppendType::ANumber("Int", "String.fromInt"),
        ErrorType::Type(..) if tipe.is_float() => AppendType::ANumber("Float", "String.fromFloat"),
        ErrorType::Type(..) if tipe.is_string() => AppendType::AString,
        ErrorType::Type(..) if tipe.is_list() => AppendType::AList,
        ErrorType::FlexSuper(Super::Number, _) => AppendType::ANumber("number", "String.fromInt"),
        _ => AppendType::AOther,
    }
}

fn bad_append_left(category: &Category, tipe: &ErrorType, expected: &ErrorType) -> Docs {
    match to_append_type(tipe) {
        AppendType::ANumber(thing, string_from_thing) => (
            format!(
                "The (++) operator can append List and String values, but not {thing} values like this:"
            ),
            format!(
                "Try using {string_from_thing} to turn it into a string? Or put it in [] to make \
                 it a list? Or switch to the (::) operator?"
            ),
            vec![],
        ),
        _ => {
            let (after, notes) = lone_type(
                tipe,
                expected,
                add_category("I am seeing", category),
                vec![Section::Para(
                    "But the (++) operator is only for appending List and String values. Maybe put \
                     this value in [] to make it a list?"
                        .to_string(),
                )],
            );
            ("The (++) operator cannot append this type of value:".to_string(), after, notes)
        }
    }
}

fn bad_append_right(category: &Category, tipe: &ErrorType, expected: &ErrorType) -> (bool, Docs) {
    match (to_append_type(expected), to_append_type(tipe)) {
        (AppendType::AString, AppendType::ANumber(thing, string_from_thing)) => (
            true,
            (
                format!(
                    "I thought I was appending String values here, not {thing} values like this:"
                ),
                format!("Try using {string_from_thing} to turn it into a string?"),
                vec![],
            ),
        ),
        (AppendType::AList, AppendType::ANumber(thing, _)) => (
            true,
            (
                format!("I thought I was appending List values here, not {thing} values like this:"),
                "Try putting it in [] to make it a list?".to_string(),
                vec![],
            ),
        ),
        (AppendType::AString, AppendType::AList) => (
            false,
            (
                "The (++) operator needs the same type of value on both sides:".to_string(),
                "I see a String on the left and a List on the right. Which should it be? Does the \
                 string need [] around it to become a list?"
                    .to_string(),
                vec![],
            ),
        ),
        (AppendType::AList, AppendType::AString) => (
            false,
            (
                "The (++) operator needs the same type of value on both sides:".to_string(),
                "I see a List on the left and a String on the right. Which should it be? Does the \
                 string need [] around it to become a list?"
                    .to_string(),
                vec![],
            ),
        ),
        _ => {
            let (after, notes) = type_comparison(
                expected,
                tipe,
                "I already figured out that the left side of (++) is:".to_string(),
                add_category("But this clashes with the right side, which is", category),
                vec![],
            );
            (
                false,
                ("The (++) operator cannot append these two values:".to_string(), after, notes),
            )
        }
    }
}

// ------------------------------------------------------------------ patterns

/// What kind of pattern is being matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PCategory {
    PRecord,
    PUnit,
    PTuple,
    PList,
    PCtor(String),
    PInt,
    PStr,
    PChr,
    PBool,
}

/// Where a pattern's expectation came from.
#[derive(Debug, Clone)]
pub enum PExpected {
    PNoExpectation(ErrorType),
    PFromContext(Region, PContext, ErrorType),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PContext {
    PTypedArg(String, usize),
    PCaseMatch(usize),
    PCtorArg(String, usize),
    PListEntry(usize),
    PTail,
}

fn add_pattern_category(trying_to_match: &str, category: &PCategory) -> String {
    let suffix = match category {
        PCategory::PRecord => " record values of type:".to_string(),
        PCategory::PUnit => " unit values:".to_string(),
        PCategory::PTuple => " tuples of type:".to_string(),
        PCategory::PList => " lists of type:".to_string(),
        PCategory::PCtor(name) => format!(" `{name}` values of type:"),
        PCategory::PInt => " integers:".to_string(),
        PCategory::PStr => " strings:".to_string(),
        PCategory::PChr => " characters:".to_string(),
        PCategory::PBool => " booleans:".to_string(),
    };
    format!("{trying_to_match}{suffix}")
}

/// Like `type_comparison`, but a pattern report puts its context note *after*
/// the hint rather than before it.
fn pattern_type_comparison(
    actual: &ErrorType,
    expected: &ErrorType,
    i_am_seeing: String,
    instead_of: String,
    context_hints: Vec<Section>,
) -> (String, Vec<Section>) {
    let (actual_doc, expected_doc, problems) = to_comparison(actual, expected);
    let mut notes = vec![
        Section::Block(actual_doc),
        Section::Para(instead_of),
        Section::Block(expected_doc),
    ];
    notes.extend(problems_to_hint(&problems));
    notes.extend(context_hints);
    (i_am_seeing, notes)
}

/// `Reporting.Error.Type.toPatternReport`.
pub fn to_pattern_report(
    pattern_region: Region,
    category: &PCategory,
    tipe: &ErrorType,
    expected: &PExpected,
) -> Report {
    match expected {
        PExpected::PNoExpectation(expected_type) => {
            let (after, notes) = pattern_type_comparison(
                tipe,
                expected_type,
                add_pattern_category("It is", category),
                "But it needs to match:".to_string(),
                vec![],
            );
            report(
                "TYPE MISMATCH",
                pattern_region,
                pattern_region,
                "This pattern is being used in an unexpected way:".to_string(),
                after,
                notes,
            )
        }
        PExpected::PFromContext(region, context, expected_type) => {
            let (before, i_am_seeing, instead_of, hints) = match context {
                PContext::PTypedArg(name, index) => {
                    let ith = ordinal(*index);
                    (
                        format!("The {ith} argument to `{name}` is weird."),
                        add_pattern_category("The argument is a pattern that matches", category),
                        format!(
                            "But the type annotation on `{name}` says the {ith} argument should be:"
                        ),
                        vec![],
                    )
                }
                PContext::PCaseMatch(index) if *index == 1 => (
                    "The 1st pattern in this `case` causing a mismatch:".to_string(),
                    add_pattern_category("The first pattern is trying to match", category),
                    "But the expression between `case` and `of` is:".to_string(),
                    vec![Section::Para(
                        "These can never match! Is the pattern the problem? Or is it the                          expression?"
                            .to_string(),
                    )],
                ),
                PContext::PCaseMatch(index) => {
                    let ith = ordinal(*index);
                    (
                        format!("The {ith} pattern in this `case` does not match the previous ones."),
                        add_pattern_category(&format!("The {ith} pattern is trying to match"), category),
                        "But all the previous patterns match:".to_string(),
                        vec![Section::Para(
                            "Note: A `case` expression can only handle one type of value, so you                              may want to use <https://elm-lang.org/0.19.1/custom-types> to handle                              “mixing” types."
                                .to_string(),
                        )],
                    )
                }
                PContext::PCtorArg(name, index) => {
                    let ith = ordinal(*index);
                    (
                        format!("The {ith} argument to `{name}` is weird."),
                        add_pattern_category("It is trying to match", category),
                        format!("But `{name}` needs its {ith} argument to be:"),
                        vec![],
                    )
                }
                PContext::PListEntry(index) => {
                    let ith = ordinal(*index);
                    (
                        format!("The {ith} pattern in this list does not match all the previous ones:"),
                        add_pattern_category(&format!("The {ith} pattern is trying to match"), category),
                        "But all the previous patterns in the list are:".to_string(),
                        vec![Section::Para(
                            "Hint: Everything in a list must be the same type of value. This way,                              we never run into unexpected values partway through a List.map,                              List.foldl, etc. Read <https://elm-lang.org/0.19.1/custom-types> to                              learn how to “mix” types."
                                .to_string(),
                        )],
                    )
                }
                PContext::PTail => (
                    "The pattern after (::) is causing issues.".to_string(),
                    add_pattern_category("The pattern after (::) is trying to match", category),
                    "But it needs to match lists like this:".to_string(),
                    vec![],
                ),
            };
            let (after, notes) =
                pattern_type_comparison(tipe, expected_type, i_am_seeing, instead_of, hints);
            report("TYPE MISMATCH", pattern_region, *region, before, after, notes)
        }
    }
}
