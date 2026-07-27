//! Port of `Type.Error` and the expression half of `Reporting.Error.Type`.
//!
//! A type error is reported as a *comparison*: elm renders the type it found
//! and the type it wanted side by side, marks the parts that differ, and picks
//! a hint from the specific way they differ (Int vs Float, a String where an
//! Int was wanted, a field typo, …). Both the wording and the layout are
//! reproduced here so alm's reports match `elm make` byte for byte.

use std::collections::BTreeMap;

use super::doc::{Color, Doc};
use super::render_type::{self, Ctx};
use super::{green, grey, hint, labeled, link, note, sentence, words, yellow};
use super::{ElmBody, Report, Section};
use crate::reporting::annotation::Region;

// ---------------------------------------------------------------- error types

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Super {
    Number,
    Comparable,
    Appendable,
    CompAppend,
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

fn to_doc(ctx: Ctx, tipe: &ErrorType) -> Doc {
    match tipe {
        ErrorType::Lambda(a, b, rest) => render_type::lambda(
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
        ErrorType::Type(_, name, args) => render_type::apply(
            ctx,
            Doc::text(name.clone()),
            args.iter().map(|t| to_doc(Ctx::App, t)).collect(),
        ),
        ErrorType::Record(fields, ext) => render_type::record(
            fields.iter().map(|(f, t)| (Doc::text(f.clone()), to_doc(Ctx::None, t))).collect(),
            ext_to_doc(ext),
        ),
        ErrorType::Unit => Doc::text("()"),
        ErrorType::Tuple(a, b, c) => render_type::tuple(
            to_doc(Ctx::None, a),
            to_doc(Ctx::None, b),
            c.iter().map(|t| to_doc(Ctx::None, t)).collect(),
        ),
    }
}

/// `toNearbyRecord` — the record's fields, closest match first, at most four.
fn nearby_record(fields: &BTreeMap<String, ErrorType>, field: &str, ext: &Extension) -> Doc {
    let order = nearby_names(field, &fields.keys().cloned().collect::<Vec<_>>());
    let entries: Vec<(Doc, Doc)> = order
        .iter()
        .map(|name| (Doc::text(name.clone()), to_doc(Ctx::None, &fields[name])))
        .collect();
    let doc = if entries.len() <= 4 {
        render_type::vrecord(entries, ext_to_doc(ext))
    } else {
        let mut it = entries.into_iter();
        let first = it.next().unwrap();
        render_type::vrecord_snippet(first, it.take(3).collect())
    };
    indented(doc)
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
    /// The two sides differ. Callers style the docs themselves: elm marks
    /// only the part that is actually wrong, so a nested mismatch shows
    /// `List ` plain with just the element type yellow, and a missing record
    /// field yellows the field *name* and not its type.
    fn different(left: Doc, right: Doc, problems: Vec<Problem>) -> Diff {
        Diff { left, right, problems: Some(problems) }
    }

    /// Both sides wrong outright — the common case, both dull yellow.
    fn mismatch(left: Doc, right: Doc, problems: Vec<Problem>) -> Diff {
        Diff::different(Doc::color(Color::Yellow, left), Doc::color(Color::Yellow, right), problems)
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
pub fn to_comparison(actual: &ErrorType, expected: &ErrorType) -> (Doc, Doc, Vec<Problem>) {
    let diff = to_diff(Ctx::None, actual, expected);
    let problems = diff.problems.unwrap_or_default();
    (indented(diff.left), indented(diff.right), problems)
}

/// A type as it appears in a report: indented four columns, wrapped at 80.
fn indented(doc: Doc) -> Doc {
    Doc::indent(4, doc)
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
                    left: render_type::lambda(
                        ctx,
                        da.left,
                        db.left,
                        rest.iter().map(|d| d.left.clone()).collect(),
                    ),
                    right: render_type::lambda(
                        ctx,
                        da.right,
                        db.right,
                        rest.iter().map(|d| d.right.clone()).collect(),
                    ),
                    problems: status,
                }
            } else {
                Diff::mismatch(
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
                left: render_type::tuple(da.left, db.left, dc.iter().map(|d| d.left.clone()).collect()),
                right: render_type::tuple(da.right, db.right, dc.iter().map(|d| d.right.clone()).collect()),
                problems: status,
            }
        }

        (Record(f1, e1), Record(f2, e2)) => diff_record(f1, e1, f2, e2),

        (Type(h1, n1, a1), Type(h2, n2, a2)) if h1 == h2 && n1 == n2 && a1.len() == a2.len() => {
            let args: Vec<Diff> =
                a1.iter().zip(a2).map(|(x, y)| to_diff(Ctx::App, x, y)).collect();
            let status = args.iter().fold(None, |acc, d| merge(acc, d.problems.clone()));
            Diff {
                left: render_type::apply(
                    ctx,
                    Doc::text(n1.clone()),
                    args.iter().map(|d| d.left.clone()).collect(),
                ),
                right: render_type::apply(
                    ctx,
                    Doc::text(n2.clone()),
                    args.iter().map(|d| d.right.clone()).collect(),
                ),
                problems: status,
            }
        }

        (Type(h, n, args), t2v) if h == "Maybe" && n == "Maybe" && args.len() == 1 && to_diff(ctx, &args[0], t2v).is_similar() => {
            Diff::different(
                render_type::apply(
                    ctx,
                    Doc::color(Color::Yellow, Doc::text("Maybe")),
                    vec![to_doc(Ctx::App, &args[0])],
                ),
                to_doc(ctx, t2),
                vec![Problem::AnythingFromMaybe],
            )
        }
        (t1v, Type(h, n, args)) if h == "List" && n == "List" && args.len() == 1 && to_diff(ctx, t1v, &args[0]).is_similar() => {
            Diff::different(
                to_doc(ctx, t1),
                render_type::apply(
                    ctx,
                    Doc::color(Color::Yellow, Doc::text("List")),
                    vec![to_doc(Ctx::App, &args[0])],
                ),
                vec![],
            )
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
            Diff::mismatch(left, right, problems)
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
        // Fields present on only one side: elm dull-yellows the *name*.
        let mut unknown_left: std::collections::BTreeSet<&String> = Default::default();
        let mut unknown_right: std::collections::BTreeSet<&String> = Default::default();
        for (name, t1) in fields1 {
            if let Some(t2) = fields2.get(name) {
                let d = to_diff(Ctx::None, t1, t2);
                status = merge(status, d.problems.clone());
                left.insert(name, d.left);
                right.insert(name, d.right);
            } else {
                unknown_left.insert(name);
                left.insert(name, to_doc(Ctx::None, t1));
            }
        }
        for (name, t2) in fields2 {
            if !fields1.contains_key(name) {
                unknown_right.insert(name);
                right.insert(name, to_doc(Ctx::None, t2));
            }
        }
        status = merge(status, Some(vec![]));
        let name_doc = |n: &String, unknown: bool| {
            let text = Doc::text(n.clone());
            if unknown {
                Doc::color(Color::Yellow, text)
            } else {
                text
            }
        };
        left_entries = left
            .into_iter()
            .map(|(n, d)| (name_doc(n, unknown_left.contains(n)), d))
            .collect();
        right_entries = right
            .into_iter()
            .map(|(n, d)| (name_doc(n, unknown_right.contains(n)), d))
            .collect();
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
        left: render_type::record(left_entries, ext_to_doc(ext1)),
        right: render_type::record(right_entries, ext_to_doc(ext2)),
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
    match problem {
        Problem::IntFloat => vec![note(
            [
                words("Read"),
                vec![link("implicit-casts")],
                words("to learn why Elm does not implicitly convert Ints to Floats. Use"),
                vec![green("toFloat"), Doc::text("and"), green("round")],
                words("to do explicit conversions."),
            ]
            .concat(),
        )],
        Problem::StringFromInt => vec![hint(
            [
                words("Want to convert an Int into a String? Use the"),
                vec![green("String.fromInt")],
                words("function!"),
            ]
            .concat(),
        )],
        Problem::StringFromFloat => vec![hint(
            [
                words("Want to convert a Float into a String? Use the"),
                vec![green("String.fromFloat")],
                words("function!"),
            ]
            .concat(),
        )],
        Problem::StringToInt => vec![hint(
            [
                words("Want to convert a String into an Int? Use the"),
                vec![green("String.toInt")],
                words("function!"),
            ]
            .concat(),
        )],
        Problem::StringToFloat => vec![hint(
            [
                words("Want to convert a String into a Float? Use the"),
                vec![green("String.toFloat")],
                words("function!"),
            ]
            .concat(),
        )],
        Problem::AnythingToBool => vec![hint(words(
            "Elm does not have “truthiness” such that ints and strings and lists are \
             automatically converted to booleans. Do that conversion explicitly!",
        ))],
        Problem::AnythingFromMaybe => vec![hint(words(
            "Use Maybe.withDefault to handle possible errors. Longer term, it is usually \
             better to write out the full `case` though!",
        ))],
        Problem::ArityMismatch(x, y) => vec![hint(words(&if x < y {
            format!("It looks like it takes too few arguments. I was expecting {} more.", y - x)
        } else {
            format!("It looks like it takes too many arguments. I see {} extra.", x - y)
        }))],
        Problem::FieldTypo(typo, possibilities) => {
            match nearby_names(typo, possibilities).first() {
                Some(nearest) => vec![
                    hint(
                        [
                            words("Seems like a record field typo. Maybe"),
                            vec![yellow(typo.clone()), Doc::text("should"), Doc::text("be")],
                            vec![Doc::cat2(green(nearest.clone()), Doc::text("?"))],
                        ]
                        .concat(),
                    ),
                    hint(words(
                        "Can more type annotations be added? Type annotations always help me give \
                         more specific messages, and I think they could help a lot in this case!",
                    )),
                ],
                None => vec![],
            }
        }
        Problem::FieldsMissing(fields) => match fields.split_first() {
            None => vec![],
            Some((f, [])) => vec![hint(
                [
                    words("Looks like the"),
                    vec![green(f.clone())],
                    words("field is missing."),
                ]
                .concat(),
            )],
            Some(_) => vec![hint(
                [
                    words("Looks like fields"),
                    comma_sep("and", &fields.iter().map(|f| green(f.clone())).collect::<Vec<_>>()),
                    words("are missing."),
                ]
                .concat(),
            )],
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

/// `D.commaSep` — "a, b, and c" (and "a and b" for two). The commas are
/// appended outside each item's styling, as elm does.
fn comma_sep(conjunction: &str, items: &[Doc]) -> Vec<Doc> {
    match items {
        [] => vec![],
        [a] => vec![a.clone()],
        [a, b] => vec![a.clone(), Doc::text(conjunction), b.clone()],
        _ => {
            let (last, rest) = items.split_last().unwrap();
            let mut out: Vec<Doc> =
                rest.iter().map(|d| Doc::cat2(d.clone(), Doc::text(","))).collect();
            out.push(Doc::text(conjunction));
            out.push(last.clone());
            out
        }
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
        hint(words(&format!(
            "Your type annotation uses type variable `{name}` which means ANY type of value can \
             flow through, but your code is saying it specifically wants {a_thing}. Maybe change \
             your type annotation to be more specific? Maybe change the code to be more general?"
        ))),
        Section::Para(sentence(
            [words("Read"), vec![link("type-annotations")], words("for more advice!")].concat(),
        )),
    ]
}

fn bad_double_rigid(x: &str, y: &str) -> Vec<Section> {
    vec![
        hint(words(&format!(
            "Your type annotation uses `{x}` and `{y}` as separate type variables. Your code seems \
             to be saying they are the same though. Maybe they should be the same in your type \
             annotation? Maybe your code uses them in a weird way?"
        ))),
        Section::Para(sentence(
            [words("Read"), vec![link("type-annotations")], words("for more advice!")].concat(),
        )),
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
        hint(words(&format!(
            "The `{super_type}` in your type annotation is saying that {many_things} can flow \
             through, but your code is saying it specifically wants {a_thing}. Maybe change your \
             type annotation to be more specific? Maybe change the code to be more general?"
        ))),
        Section::Para(sentence(
            [words("Read"), vec![link("type-annotations")], words("for more advice!")].concat(),
        )),
    ]
}

fn bad_flex_flex_super(s1: Super, s2: Super) -> Vec<Section> {
    let like_this = |s: Super| match s {
        Super::Number => "a number",
        Super::Comparable => "comparable",
        Super::CompAppend => "a compappend",
        Super::Appendable => "appendable",
    };
    vec![hint(words(&format!(
        "There are no values in Elm that are both {} and {}.",
        like_this(s1),
        like_this(s2)
    )))]
}

fn bad_flex_super(direction: Direction, super_: Super, tipe: &ErrorType) -> Vec<Section> {
    match super_ {
        Super::Comparable => match tipe {
            ErrorType::Record(..) => vec![Section::Para(sentence(
                [
                    labeled(
                        "Hint",
                        words(
                            "I do not know how to compare records. I can only compare ints, \
                             floats, chars, strings, lists of comparable values, and tuples of \
                             comparable values. Check out",
                        ),
                    ),
                    vec![link("comparing-records")],
                    words("for ideas on how to proceed."),
                ]
                .concat(),
            ))],
            ErrorType::Type(_, name, _) => vec![
                hint(words(&format!(
                    "I do not know how to compare `{name}` values. I can only compare ints, \
                     floats, chars, strings, lists of comparable values, and tuples of comparable \
                     values."
                ))),
                Section::Para(sentence(
                    [
                        words("Check out"),
                        vec![link("comparing-custom-types")],
                        words("for ideas on how to proceed."),
                    ]
                    .concat(),
                )),
            ],
            _ => vec![hint(words(
                "I only know how to compare ints, floats, chars, strings, lists of comparable \
                 values, and tuples of comparable values.",
            ))],
        },
        Super::Appendable => vec![hint(words("I only know how to append strings and lists."))],
        Super::CompAppend => {
            vec![hint(words("Only strings and lists are both comparable and appendable."))]
        }
        Super::Number => match tipe {
            ErrorType::Type(..) if tipe.is_string() => match direction {
                Direction::Have => vec![hint(
                    [
                        words("Try using"),
                        vec![green("String.fromInt")],
                        words("to convert it to a string?"),
                    ]
                    .concat(),
                )],
                Direction::Need => vec![hint(
                    [
                        words("Try using"),
                        vec![green("String.toInt")],
                        words("to convert it to an integer?"),
                    ]
                    .concat(),
                )],
            },
            _ => vec![hint(
                [
                    words("Only"),
                    vec![green("Int"), Doc::text("and"), green("Float")],
                    words("values work as numbers."),
                ]
                .concat(),
            )],
        },
    }
}

// -------------------------------------------------------------- the report

/// `typeComparison`: what I found, what was wanted, then any hints.
fn type_comparison(
    actual: &ErrorType,
    expected: &ErrorType,
    i_am_seeing: impl Into<Doc>,
    instead_of: impl Into<Doc>,
    context_hints: Vec<Section>,
) -> (Doc, Vec<Section>) {
    let (actual_doc, expected_doc, problems) = to_comparison(actual, expected);
    let mut notes = vec![
        Section::Para(actual_doc),
        Section::Para(instead_of.into()),
        Section::Para(expected_doc),
    ];
    notes.extend(context_hints);
    notes.extend(problems_to_hint(&problems));
    (i_am_seeing.into(), notes)
}

/// `loneType`: only what I found, then further details.
fn lone_type(
    actual: &ErrorType,
    expected: &ErrorType,
    i_am_seeing: impl Into<Doc>,
    further: Vec<Section>,
) -> (Doc, Vec<Section>) {
    let (actual_doc, _, problems) = to_comparison(actual, expected);
    let mut notes = vec![Section::Para(actual_doc)];
    notes.extend(further);
    notes.extend(problems_to_hint(&problems));
    (i_am_seeing.into(), notes)
}

/// A report whose caret sits on the expression being blamed. elm's
/// `Report.Report title exprRegion` pairs with `Code.toSnippet source region
/// (Just exprRegion)`: the *report's* region — what `--report=json` publishes —
/// is the expression, while `region` is the wider span of source shown.
fn report(
    title: &str,
    expr_region: Region,
    region: Region,
    before: impl Into<Doc>,
    after: impl Into<Doc>,
    notes: Vec<Section>,
) -> Report {
    report_highlighting(title, expr_region, region, expr_region, before, after, notes)
}

/// As [`report`] but underlining something other than the blamed expression —
/// a record's field, say, while the report still points at the expression.
fn report_highlighting(
    title: &str,
    expr_region: Region,
    region: Region,
    highlight: Region,
    before: impl Into<Doc>,
    after: impl Into<Doc>,
    notes: Vec<Section>,
) -> Report {
    let (before, after) = (before.into(), after.into());
    Report {
        title: title.to_string(),
        region: expr_region,
        message: String::new(),
        elm: Some(ElmBody { before, after, notes, region: Some(region), highlight }),
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
                        vec![hint(
                            [
                                words(
                                    "Everything in a list must be the same type of value. This way, we never run \
                                     into unexpected values partway through a List.map, List.foldl, etc. \
                                     Read",
                                ),
                                vec![link("custom-types")],
                                words("to learn how to “mix” types."),
                            ]
                            .concat(),
                        )],
                    )
                }

                Context::Negate => bad_type(
                    "I do not know how to negate this type of value:".to_string(),
                    "It is".to_string(),
                    vec![Section::Para(sentence(
                        [
                            words("But I only now how to negate"),
                            vec![yellow("Int"), Doc::text("and"), yellow("Float")],
                            words("values."),
                        ]
                        .concat(),
                    ))],
                ),

                Context::IfCondition => bad_type(
                    "This `if` condition does not evaluate to a boolean value, True or False."
                        .to_string(),
                    "It is".to_string(),
                    vec![Section::Para(sentence(
                        [
                            words("But I need this `if` condition to be a"),
                            vec![yellow("Bool")],
                            words("value."),
                        ]
                        .concat(),
                    ))],
                ),

                Context::IfBranch(index) => {
                    let ith = ordinal(*index);
                    mismatch(
                        format!("The {ith} branch of this `if` does not match all the previous branches:"),
                        format!("The {ith} branch is"),
                        "But all the previous branches result in:".to_string(),
                        vec![hint(
                            [
                                words(
                                    "All branches in an `if` must produce the same type of values. This way, no \
                                     matter which branch we take, the result is always a consistent shape. \
                                     Read",
                                ),
                                vec![link("custom-types")],
                                words("to learn how to “mix” types."),
                            ]
                            .concat(),
                        )],
                    )
                }

                Context::CaseBranch(index) => {
                    let ith = ordinal(*index);
                    mismatch(
                        format!("The {ith} branch of this `case` does not match all the previous branches:"),
                        format!("The {ith} branch is"),
                        "But all the previous branches result in:".to_string(),
                        vec![hint(
                            [
                                words(
                                    "All branches in a `case` must produce the same type of values. This way, no \
                                     matter which branch we take, the result is always a consistent shape. \
                                     Read",
                                ),
                                vec![link("custom-types")],
                                words("to learn how to “mix” types."),
                            ]
                            .concat(),
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
                        vec![hint(words(
                            "I always figure out the argument types from left to right. If an \
                             argument is acceptable, I assume it is “correct” and move on. So the \
                             problem may actually be in one of the previous arguments!",
                        ))]
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
                                        Section::Para(nearby_record(fields, field, ext)),
                                        Section::Para(sentence(
                                            [
                                                words("So maybe"),
                                                vec![
                                                    yellow(field.clone()),
                                                    Doc::text("should"),
                                                    Doc::text("be"),
                                                    Doc::cat2(
                                                        green(nearest.clone()),
                                                        Doc::text("?"),
                                                    ),
                                                ],
                                            ]
                                            .concat(),
                                        )),
                                    ],
                                )
                            };
                            report_highlighting(
                                "TYPE MISMATCH",
                                expr_region,
                                *region,
                                *field_region,
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
                                vec![Section::Para(sentence(
                                    [
                                        words("But I need a record with a"),
                                        vec![yellow(field.clone())],
                                        words("field!"),
                                    ]
                                    .concat(),
                                ))],
                            );
                            report_highlighting(
                                "TYPE MISMATCH",
                                expr_region,
                                *region,
                                *record_region,
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
                                vec![Section::para(
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
                                            Section::Para(nearby_record(actual_fields, field, ext)),
                                            Section::Para(sentence(
                                                [
                                                    words("So maybe"),
                                                    vec![
                                                        yellow(field.clone()),
                                                        Doc::text("should"),
                                                        Doc::text("be"),
                                                        Doc::cat2(
                                                            green(nearest.clone()),
                                                            Doc::text("?"),
                                                        ),
                                                    ],
                                                ]
                                                .concat(),
                                            )),
                                        ],
                                    )
                                };
                                report_highlighting(
                                    "TYPE MISMATCH",
                                    expr_region,
                                    *region,
                                    *field_region,
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
                        vec![Section::para("But I need a record!".to_string())],
                    ),
                },

                Context::RecordUpdateValue(field) => mismatch(
                    format!("I cannot update the `{field}` field like this:"),
                    format!("You are trying to update `{field}` to be"),
                    "But it should be:".to_string(),
                    vec![note(words(
                        "The record update syntax does not allow you to change the type of fields. \
                         You can achieve that with record constructors or the record literal \
                         syntax.",
                    ))],
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
type Docs = (Doc, Doc, Vec<Section>);

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
                vec![Section::para("This needs to be some kind of function though!".to_string())],
            );
            (
                Doc::reflow(
                    "The left side of (<|) needs to be a function so I can pipe arguments to it!",
                ),
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
            (Doc::reflow(&format!("The left argument of ({op}) is causing problems:")), after, notes)
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
        // `badCastHelp`: name the two sides in yellow and the two conversion
        // functions in green.
        let (left_article, left_type, right_article, right_type, fix_left, fix_right) =
            if float_then_int {
                ("a", "Float", "an", "Int", "round", "toFloat")
            } else {
                ("an", "Int", "a", "Float", "toFloat", "round")
            };
        (
            false,
            (
                Doc::reflow(&format!(
                    "I need both sides of ({op}) to be the exact same type. Both Int or both Float."
                )),
                sentence(
                    [
                        words("But I see"),
                        vec![Doc::text(left_article), yellow(left_type)],
                        words("on the left and"),
                        vec![Doc::text(right_article), yellow(right_type)],
                        words("on the right."),
                    ]
                    .concat(),
                ),
                vec![
                    Section::Para(sentence(
                        [
                            words("Use"),
                            vec![green(fix_left)],
                            words("on the left (or"),
                            vec![green(fix_right)],
                            words("on the right) to make both sides match!"),
                        ]
                        .concat(),
                    )),
                    implicit_casts_note(),
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
            (true, (Doc::reflow("I cannot send this through the (<|) pipe:"), after, notes))
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
                        Doc::reflow(
                            "This function cannot handle the argument sent through the (|>) pipe:",
                        ),
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
                        Doc::reflow(
                            "The right side of (|>) needs to be a function so I can pipe arguments \
                             to it!",
                        ),
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
        elm: Some(ElmBody {
            before: before.into(),
            after: after.into(),
            notes,
            region: Some(region),
            highlight,
        }),
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
        vec![hint(words(&format!(
            "With operators like ({op}) I always check the left side first. If it seems fine, I \
             assume it is correct and check the right side. So the problem may be in how the left \
             and right arguments interact!"
        )))],
    );
    (Doc::reflow(&format!("The right argument of ({op}) is causing problems.")), after, notes)
}

fn bad_string_add() -> Docs {
    (
        sentence(
            [
                words("I cannot do addition with"),
                vec![yellow("String")],
                words("values like this one:"),
            ]
            .concat(),
        ),
        sentence(
            [
                words("The (+) operator only works with"),
                vec![yellow("Int"), Doc::text("and"), yellow("Float")],
                words("values."),
            ]
            .concat(),
        ),
        vec![hint(
            [words("Switch to the"), vec![green("(++)")], words("operator to append strings!")]
                .concat(),
        )],
    )
}

fn bad_list_add(category: &Category, direction: &str, tipe: &ErrorType, expected: &ErrorType) -> Docs {
    let (after, notes) = lone_type(
        tipe,
        expected,
        Doc::reflow(&add_category(&format!("The {direction} side of (+) is"), category)),
        vec![
            Section::Para(sentence(
                [
                    words("But (+) only works with"),
                    vec![yellow("Int"), Doc::text("and"), yellow("Float")],
                    words("values."),
                ]
                .concat(),
            )),
            hint(
                [words("Switch to the"), vec![green("(++)")], words("operator to append lists!")]
                    .concat(),
            ),
        ],
    );
    (Doc::reflow("I cannot do addition with lists:"), after, notes)
}

fn bad_list_mul(category: &Category, direction: &str, tipe: &ErrorType, expected: &ErrorType) -> Docs {
    bad_math(
        category,
        "Multiplication",
        direction,
        "*",
        tipe,
        expected,
        vec![hint(
            [
                words("Maybe you want"),
                vec![green("List.repeat")],
                words("to build a list of repeated values?"),
            ]
            .concat(),
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
    let mut further = vec![Section::Para(sentence(
        [
            words(&format!("But ({op}) only works with")),
            vec![yellow("Int"), Doc::text("and"), yellow("Float")],
            words("values."),
        ]
        .concat(),
    ))];
    further.extend(other_hints);
    let (after, notes) = lone_type(
        tipe,
        expected,
        Doc::reflow(&add_category(&format!("The {direction} side of ({op}) is"), category)),
        further,
    );
    (Doc::reflow(&format!("{operation} does not work with this value:")), after, notes)
}

fn bad_fdiv(direction: &str, tipe: &ErrorType, expected: &ErrorType) -> Docs {
    let before = Doc::reflow("The (/) operator is specifically for floating-point division:");
    if tipe.is_int() {
        (
            before,
            sentence(
                [
                    words(&format!("The {direction} side of (/) must be a")),
                    vec![
                        Doc::cat2(yellow("Float"), Doc::text(",")),
                        Doc::text("but"),
                        Doc::text("I"),
                        Doc::text("am"),
                        Doc::text("seeing"),
                        Doc::text("an"),
                        Doc::cat2(yellow("Int"), Doc::text(".")),
                    ],
                    words("I recommend:"),
                ]
                .concat(),
            ),
            vec![
                Section::Para(Doc::vcat(vec![
                    Doc::cat2(
                        green("toFloat"),
                        Doc::cat2(
                            Doc::text(" for explicit conversions     "),
                            grey("(toFloat 5 / 2) == 2.5"),
                        ),
                    ),
                    Doc::cat2(
                        green("(//)   "),
                        Doc::cat2(
                            Doc::text(" for integer division         "),
                            grey("(5 // 2)        == 2"),
                        ),
                    ),
                ])),
                implicit_casts_note(),
            ],
        )
    } else {
        let (after, notes) = lone_type(
            tipe,
            expected,
            sentence(
                [
                    words(&format!("The {direction} side of (/) must be a")),
                    vec![Doc::cat2(yellow("Float"), Doc::text(","))],
                    words("but instead I am seeing:"),
                ]
                .concat(),
            ),
            vec![],
        );
        (before, after, notes)
    }
}

fn bad_idiv(direction: &str, tipe: &ErrorType, expected: &ErrorType) -> Docs {
    let before = Doc::reflow("The (//) operator is specifically for integer division:");
    if tipe.is_float() {
        (
            before,
            sentence(
                [
                    words(&format!("The {direction} side of (//) must be an")),
                    vec![
                        Doc::cat2(yellow("Int"), Doc::text(",")),
                        Doc::text("but"),
                        Doc::text("I"),
                        Doc::text("am"),
                        Doc::text("seeing"),
                        Doc::text("a"),
                        Doc::cat2(yellow("Float"), Doc::text(".")),
                    ],
                    words("I recommend doing the conversion explicitly with one of these functions:"),
                ]
                .concat(),
            ),
            vec![
                Section::Para(Doc::vcat(vec![
                    Doc::cat2(green("round"), Doc::text(" 3.5     == 4")),
                    Doc::cat2(green("floor"), Doc::text(" 3.5     == 3")),
                    Doc::cat2(green("ceiling"), Doc::text(" 3.5   == 4")),
                    Doc::cat2(green("truncate"), Doc::text(" 3.5  == 3")),
                ])),
                implicit_casts_note(),
            ],
        )
    } else {
        let (after, notes) = lone_type(
            tipe,
            expected,
            sentence(
                [
                    words(&format!("The {direction} side of (//) must be an")),
                    vec![Doc::cat2(yellow("Int"), Doc::text(","))],
                    words("but instead I am seeing:"),
                ]
                .concat(),
            ),
            vec![],
        );
        (before, after, notes)
    }
}

/// `D.link "Note" "Read" "implicit-casts" ...`, shared by the division and cast
/// reports.
fn implicit_casts_note() -> Section {
    note(
        [
            words("Read"),
            vec![link("implicit-casts")],
            words("to learn why Elm does not implicitly convert Ints to Floats."),
        ]
        .concat(),
    )
}

fn bad_bool(op: &str, direction: &str, tipe: &ErrorType, expected: &ErrorType) -> Docs {
    let (after, notes) = lone_type(
        tipe,
        expected,
        sentence(
            [
                words(&format!("Both sides of ({op}) must be")),
                vec![yellow("Bool")],
                words(&format!("values, but the {direction} side is:")),
            ]
            .concat(),
        ),
        vec![],
    );
    (Doc::reflow("I am struggling with this boolean operation:"), after, notes)
}

fn bad_comp_left(category: &Category, op: &str, direction: &str, tipe: &ErrorType, expected: &ErrorType) -> Docs {
    let (after, notes) = lone_type(
        tipe,
        expected,
        Doc::reflow(&add_category(&format!("The {direction} side of ({op}) is"), category)),
        vec![Section::Para(sentence(
            [
                words(&format!("But ({op}) only works on")),
                vec![
                    Doc::cat2(yellow("Int"), Doc::text(",")),
                    Doc::cat2(yellow("Float"), Doc::text(",")),
                    Doc::cat2(yellow("Char"), Doc::text(",")),
                    Doc::text("and"),
                    yellow("String"),
                ],
                words(
                    "values. It can work on lists and tuples of comparable values as well, but it \
                     is usually better to find a different path.",
                ),
            ]
            .concat(),
        ))],
    );
    (Doc::reflow("I cannot do a comparison with this value:"), after, notes)
}

fn bad_comp_right(op: &str, tipe: &ErrorType, expected: &ErrorType) -> Docs {
    // elm compares expected-then-actual here, so the left operand (already
    // inferred) is shown first and the right one second.
    let (after, notes) = type_comparison(
        expected,
        tipe,
        format!("The left side of ({op}) is:"),
        "But the right side is:".to_string(),
        vec![Section::para(format!(
            "I cannot compare different types though! Which side of ({op}) is the problem?"
        ))],
    );
    (Doc::reflow(&format!("I need both sides of ({op}) to be the same type:")), after, notes)
}

fn bad_equality(op: &str, tipe: &ErrorType, expected: &ErrorType) -> Docs {
    let advice = if tipe.is_float() || expected.is_float() {
        note(words(
            "Equality on floats is not 100% reliable due to the design of IEEE 754. I recommend a \
             check like (abs (x - y) < 0.0001) instead.",
        ))
    } else {
        Section::para("Different types can never be equal though! Which side is messed up?")
    };
    let (after, notes) = type_comparison(
        expected,
        tipe,
        format!("The left side of ({op}) is:"),
        "But the right side is:".to_string(),
        vec![advice],
    );
    (Doc::reflow(&format!("I need both sides of ({op}) to be the same type:")), after, notes)
}

fn bad_cons_right(category: &Category, tipe: &ErrorType, expected: &ErrorType) -> (bool, Docs) {
    if let ErrorType::Type(h1, n1, actual_args) = tipe {
        if h1 == "List" && n1 == "List" && actual_args.len() == 1 {
            if let ErrorType::Type(h2, n2, expected_args) = expected {
                if h2 == "List" && n2 == "List" && expected_args.len() == 1 {
                    let further = if is_list1(&expected_args[0]) {
                        vec![hint(words(
                            "Are you trying to append two lists? The (++) operator appends lists, \
                             whereas the (::) operator is only for adding ONE element to a list.",
                        ))]
                    } else {
                        vec![Section::para("Lists need ALL elements to be the same type though.")]
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
                        (
                            Doc::reflow("I am having trouble with this (::) operator:"),
                            after,
                            notes,
                        ),
                    );
                }
            }
            return (true, bad_op_right_fallback(category, "::", tipe, expected));
        }
    }
    let (after, notes) = lone_type(
        tipe,
        expected,
        Doc::reflow(&add_category("The right side is", category)),
        vec![Section::Para(sentence(
            [words("But (::) needs a"), vec![yellow("List")], words("on the right.")].concat(),
        ))],
    );
    (
        true,
        (Doc::reflow("The (::) operator can only add elements onto lists."), after, notes),
    )
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
            sentence(
                [
                    words("The (++) operator can append List and String values, but not"),
                    vec![yellow(thing)],
                    words("values like this:"),
                ]
                .concat(),
            ),
            sentence(
                [
                    words("Try using"),
                    vec![green(string_from_thing)],
                    words(
                        "to turn it into a string? Or put it in [] to make it a list? Or switch to \
                         the (::) operator?",
                    ),
                ]
                .concat(),
            ),
            vec![],
        ),
        _ => {
            let (after, notes) = lone_type(
                tipe,
                expected,
                Doc::reflow(&add_category("I am seeing", category)),
                vec![Section::Para(sentence(
                    [
                        words("But the (++) operator is only for appending"),
                        vec![yellow("List"), Doc::text("and"), yellow("String")],
                        words("values. Maybe put this value in [] to make it a list?"),
                    ]
                    .concat(),
                ))],
            );
            (Doc::reflow("The (++) operator cannot append this type of value:"), after, notes)
        }
    }
}

fn bad_append_right(category: &Category, tipe: &ErrorType, expected: &ErrorType) -> (bool, Docs) {
    match (to_append_type(expected), to_append_type(tipe)) {
        (AppendType::AString, AppendType::ANumber(thing, string_from_thing)) => (
            true,
            (
                sentence(
                    [
                        words("I thought I was appending"),
                        vec![yellow("String")],
                        words("values here, not"),
                        vec![yellow(thing)],
                        words("values like this:"),
                    ]
                    .concat(),
                ),
                sentence(
                    [
                        words("Try using"),
                        vec![green(string_from_thing)],
                        words("to turn it into a string?"),
                    ]
                    .concat(),
                ),
                vec![],
            ),
        ),
        (AppendType::AList, AppendType::ANumber(thing, _)) => (
            true,
            (
                sentence(
                    [
                        words("I thought I was appending"),
                        vec![yellow("List")],
                        words("values here, not"),
                        vec![yellow(thing)],
                        words("values like this:"),
                    ]
                    .concat(),
                ),
                Doc::reflow("Try putting it in [] to make it a list?"),
                vec![],
            ),
        ),
        (AppendType::AString, AppendType::AList) => (
            false,
            (
                Doc::reflow("The (++) operator needs the same type of value on both sides:"),
                sentence(
                    [
                        words("I see a"),
                        vec![yellow("String")],
                        words("on the left and a"),
                        vec![yellow("List")],
                        words(
                            "on the right. Which should it be? Does the string need [] around it \
                             to become a list?",
                        ),
                    ]
                    .concat(),
                ),
                vec![],
            ),
        ),
        (AppendType::AList, AppendType::AString) => (
            false,
            (
                Doc::reflow("The (++) operator needs the same type of value on both sides:"),
                sentence(
                    [
                        words("I see a"),
                        vec![yellow("List")],
                        words("on the left and a"),
                        vec![yellow("String")],
                        words(
                            "on the right. Which should it be? Does the string need [] around it \
                             to become a list?",
                        ),
                    ]
                    .concat(),
                ),
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
                (Doc::reflow("The (++) operator cannot append these two values:"), after, notes),
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
    i_am_seeing: impl Into<Doc>,
    instead_of: impl Into<Doc>,
    context_hints: Vec<Section>,
) -> (Doc, Vec<Section>) {
    let (actual_doc, expected_doc, problems) = to_comparison(actual, expected);
    let mut notes = vec![
        Section::Para(actual_doc),
        Section::Para(instead_of.into()),
        Section::Para(expected_doc),
    ];
    notes.extend(problems_to_hint(&problems));
    notes.extend(context_hints);
    (i_am_seeing.into(), notes)
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
                    vec![Section::para(
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
                        vec![Section::para(
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
                        vec![Section::para(
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
