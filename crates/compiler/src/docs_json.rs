//! Reading a `docs.json` back into a package's public API.
//!
//! [`crate::docs`] writes this file; this reads one. The two are not inverses:
//! what is written comes from canonical types, while what is read back is the
//! `Elm.Compiler.Type` the strings in the file spell out. That is exactly what
//! elm does too — `Elm.Compiler.Type.decoder` runs the ordinary type parser
//! over the string and, like it, keeps only the last segment of a qualified
//! name. So `"Basics.Int"` reads back as `Int`, which is why `alm diff` prints
//! short names where `docs.json` holds long ones.
//!
//! [`Names`] exists because elm only takes that path for *published* docs.
//! Docs generated from source on the spot never go through the string form at
//! all, so they keep their qualifiers, and `elm diff` prints a package's own
//! new code qualified while printing the release it is compared against short.
//! Reproducing that means being able to read a `docs.json` both ways.

use std::collections::BTreeMap;

use crate::json::Json;
use crate::reporting::doc::Doc;
use crate::reporting::render_type::{self as rt, Ctx};

/// `Elm.Compiler.Type` — a type as `docs.json` records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Type {
    Lambda(Box<Type>, Box<Type>),
    Var(String),
    /// A type constructor and its arguments. The name is unqualified.
    Type(String, Vec<Type>),
    /// Fields and, for an extensible record, the variable being extended.
    Record(Vec<(String, Type)>, Option<String>),
    Unit,
    /// Always at least two elements; Elm has no one-tuples.
    Tuple(Vec<Type>),
}

#[derive(Debug, Clone)]
pub struct Union {
    pub comment: String,
    pub args: Vec<String>,
    pub cases: Vec<(String, Vec<Type>)>,
}

#[derive(Debug, Clone)]
pub struct Alias {
    pub comment: String,
    pub args: Vec<String>,
    pub tipe: Type,
}

#[derive(Debug, Clone)]
pub struct Value {
    pub comment: String,
    pub tipe: Type,
}

#[derive(Debug, Clone)]
pub struct Binop {
    pub comment: String,
    pub tipe: Type,
    pub associativity: String,
    pub precedence: i64,
}

/// One module's exposed API.
#[derive(Debug, Clone, Default)]
pub struct Module {
    pub comment: String,
    pub unions: BTreeMap<String, Union>,
    pub aliases: BTreeMap<String, Alias>,
    pub values: BTreeMap<String, Value>,
    pub binops: BTreeMap<String, Binop>,
}

/// A whole package's API: every exposed module, by name.
pub type Documentation = BTreeMap<String, Module>;

/// Whether to keep the module qualifier on a type name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Names {
    /// `Basics.Int` reads back as `Int`, as elm's decoder does it.
    Short,
    /// `Basics.Int` stays `Basics.Int`, as freshly generated docs are.
    Qualified,
}

pub fn parse(text: &str, names: Names) -> Option<Documentation> {
    let modules = crate::json::parse(text)?;
    let mut docs = Documentation::new();
    for module in modules.as_array()? {
        let entry = Module {
            comment: module.string("comment"),
            unions: module
                .array("unions")
                .iter()
                .map(|u| {
                    let cases = u
                        .array("cases")
                        .iter()
                        .filter_map(|case| {
                            let pair = case.as_array()?;
                            let name = pair.first()?.as_str()?.to_string();
                            let args = pair
                                .get(1)?
                                .as_array()?
                                .iter()
                                .map(|t| read_type(t, names))
                                .collect::<Vec<_>>();
                            Some((name, args))
                        })
                        .collect();
                    let union =
                        Union { comment: u.string("comment"), args: string_list(u, "args"), cases };
                    (u.string("name"), union)
                })
                .collect(),
            aliases: module
                .array("aliases")
                .iter()
                .map(|a| {
                    let alias = Alias {
                        comment: a.string("comment"),
                        args: string_list(a, "args"),
                        tipe: read_type(a.get("type").unwrap_or(&Json::Null), names),
                    };
                    (a.string("name"), alias)
                })
                .collect(),
            values: module
                .array("values")
                .iter()
                .map(|v| {
                    let value = Value {
                        comment: v.string("comment"),
                        tipe: read_type(v.get("type").unwrap_or(&Json::Null), names),
                    };
                    (v.string("name"), value)
                })
                .collect(),
            binops: module
                .array("binops")
                .iter()
                .map(|b| {
                    let binop = Binop {
                        comment: b.string("comment"),
                        tipe: read_type(b.get("type").unwrap_or(&Json::Null), names),
                        associativity: b.string("associativity"),
                        precedence: b.get("precedence").and_then(Json::as_f64).unwrap_or(0.0) as i64,
                    };
                    (b.string("name"), binop)
                })
                .collect(),
        };
        docs.insert(module.string("name"), entry);
    }
    Some(docs)
}

fn string_list(value: &Json, key: &str) -> Vec<String> {
    value.array(key).iter().filter_map(|n| n.as_str().map(str::to_string)).collect()
}

/// A malformed type string becomes `?`, matching how the type checker prints a
/// type it could not work out. A `docs.json` in the cache is machine-written,
/// so this only fires on a corrupt file, and there it is better to show the
/// rest of the diff than to refuse the whole thing.
fn read_type(value: &Json, names: Names) -> Type {
    value
        .as_str()
        .and_then(|text| parse_type_with(text, names))
        .unwrap_or_else(|| Type::Type("?".to_string(), Vec::new()))
}

// ------------------------------------------------------------- parsing a type

/// Parse the Elm type expression a `docs.json` string holds, dropping module
/// qualifiers the way elm's decoder does.
pub fn parse_type(text: &str) -> Option<Type> {
    parse_type_with(text, Names::Short)
}

pub fn parse_type_with(text: &str, names: Names) -> Option<Type> {
    let mut p = TypeParser { chars: text.chars().collect(), at: 0, names };
    p.spaces();
    let tipe = p.expression()?;
    p.spaces();
    (p.at == p.chars.len()).then_some(tipe)
}

struct TypeParser {
    chars: Vec<char>,
    at: usize,
    names: Names,
}

impl TypeParser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.at).copied()
    }

    fn spaces(&mut self) {
        while matches!(self.peek(), Some(' ' | '\n' | '\r' | '\t')) {
            self.at += 1;
        }
    }

    fn eat(&mut self, c: char) -> bool {
        let hit = self.peek() == Some(c);
        if hit {
            self.at += 1;
        }
        hit
    }

    fn eat_arrow(&mut self) -> bool {
        if self.peek() == Some('-') && self.chars.get(self.at + 1) == Some(&'>') {
            self.at += 2;
            return true;
        }
        false
    }

    /// `->` is right associative, so the tail is a whole expression again.
    fn expression(&mut self) -> Option<Type> {
        let head = self.application()?;
        self.spaces();
        if self.eat_arrow() {
            self.spaces();
            return Some(Type::Lambda(Box::new(head), Box::new(self.expression()?)));
        }
        Some(head)
    }

    /// A type constructor applied to atoms. Only a constructor takes
    /// arguments; anything else stands alone.
    fn application(&mut self) -> Option<Type> {
        let head = self.atom()?;
        let Type::Type(name, _) = head else {
            return Some(head);
        };
        let mut args = Vec::new();
        loop {
            let before = self.at;
            self.spaces();
            match self.atom() {
                Some(arg) => args.push(arg),
                None => {
                    self.at = before;
                    return Some(Type::Type(name, args));
                }
            }
        }
    }

    fn atom(&mut self) -> Option<Type> {
        match self.peek()? {
            '(' => self.parenthesized(),
            '{' => self.record(),
            c if c.is_lowercase() => Some(Type::Var(self.name()?)),
            c if c.is_uppercase() => Some(Type::Type(self.qualified_name()?, Vec::new())),
            _ => None,
        }
    }

    /// `()`, `( a )` and `( a, b, … )` all start the same way.
    fn parenthesized(&mut self) -> Option<Type> {
        self.eat('(').then_some(())?;
        self.spaces();
        if self.eat(')') {
            return Some(Type::Unit);
        }
        let mut parts = vec![self.expression()?];
        loop {
            self.spaces();
            if self.eat(',') {
                self.spaces();
                parts.push(self.expression()?);
                continue;
            }
            self.eat(')').then_some(())?;
            return Some(if parts.len() == 1 {
                parts.pop().unwrap()
            } else {
                Type::Tuple(parts)
            });
        }
    }

    fn record(&mut self) -> Option<Type> {
        self.eat('{').then_some(())?;
        self.spaces();
        if self.eat('}') {
            return Some(Type::Record(Vec::new(), None));
        }
        // A leading lowercase name is either the first field or the variable
        // an extensible record extends; the following character says which.
        let first = self.name()?;
        self.spaces();
        let extended = self.eat('|');
        let mut ext = None;
        let mut field = first;
        if extended {
            self.spaces();
            ext = Some(std::mem::replace(&mut field, self.name()?));
        }
        let mut fields = Vec::new();
        loop {
            self.spaces();
            self.eat(':').then_some(())?;
            self.spaces();
            fields.push((field, self.expression()?));
            self.spaces();
            if self.eat(',') {
                self.spaces();
                field = self.name()?;
                continue;
            }
            self.eat('}').then_some(())?;
            return Some(Type::Record(fields, ext));
        }
    }

    fn name(&mut self) -> Option<String> {
        let start = self.at;
        while self.peek().is_some_and(|c| c.is_alphanumeric() || c == '_') {
            self.at += 1;
        }
        (self.at > start).then(|| self.chars[start..self.at].iter().collect())
    }

    /// `Dict.Dict` and `Dict` both name the same thing here, so a comparison
    /// has to allow for either spelling however this is read.
    fn qualified_name(&mut self) -> Option<String> {
        let start = self.at;
        let mut last = self.name()?;
        while self.peek() == Some('.') {
            // Only a following uppercase letter continues the name; a `.` in
            // any other position is not part of a type.
            let Some(c) = self.chars.get(self.at + 1) else { break };
            if !c.is_alphabetic() {
                break;
            }
            self.at += 1;
            last = self.name()?;
        }
        Some(match self.names {
            Names::Short => last,
            Names::Qualified => self.chars[start..self.at].iter().collect(),
        })
    }
}

// ------------------------------------------------------------------- printing

/// `Elm.Compiler.Type.toDoc`.
pub fn to_doc(ctx: Ctx, tipe: &Type) -> Doc {
    match tipe {
        Type::Lambda(_, _) => {
            let parts: Vec<Doc> =
                collect_lambdas(tipe).iter().map(|t| to_doc(Ctx::Func, t)).collect();
            let mut it = parts.into_iter();
            let a = it.next().expect("a lambda has at least two parts");
            let b = it.next().expect("a lambda has at least two parts");
            rt::lambda(ctx, a, b, it.collect())
        }
        Type::Var(name) => Doc::text(name.clone()),
        Type::Unit => Doc::text("()"),
        Type::Tuple(parts) => {
            let docs: Vec<Doc> = parts.iter().map(|t| to_doc(Ctx::None, t)).collect();
            let mut it = docs.into_iter();
            let a = it.next().expect("a tuple has at least two parts");
            let b = it.next().expect("a tuple has at least two parts");
            rt::tuple(a, b, it.collect())
        }
        Type::Type(name, args) => rt::apply(
            ctx,
            Doc::text(name.clone()),
            args.iter().map(|t| to_doc(Ctx::App, t)).collect(),
        ),
        Type::Record(fields, ext) => rt::record(
            fields.iter().map(|(f, t)| (Doc::text(f.clone()), to_doc(Ctx::None, t))).collect(),
            ext.as_ref().map(|e| Doc::text(e.clone())),
        ),
    }
}

fn collect_lambdas(tipe: &Type) -> Vec<&Type> {
    match tipe {
        Type::Lambda(arg, body) => {
            let mut out = vec![arg.as_ref()];
            out.extend(collect_lambdas(body));
            out
        }
        _ => vec![tipe],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn show(text: &str) -> String {
        to_doc(Ctx::None, &parse_type(text).unwrap()).render(usize::MAX)
    }

    #[test]
    fn qualified_names_lose_their_qualifier() {
        assert_eq!(show("String.String"), "String");
        assert_eq!(show("Dict.Dict String.String Basics.Int"), "Dict String Int");
        assert_eq!(show("Json.Decode.Decoder a"), "Decoder a");
    }

    #[test]
    fn functions_are_right_associative_and_parenthesize_their_arguments() {
        assert_eq!(show("a -> b -> c"), "a -> b -> c");
        assert_eq!(show("(a -> b) -> c"), "(a -> b) -> c");
        assert_eq!(
            show("(Http.Response body -> Result.Result x a) -> Http.Expect msg"),
            "(Response body -> Result x a) -> Expect msg"
        );
    }

    #[test]
    fn applications_nest_with_parentheses_only_where_needed() {
        assert_eq!(show("Maybe.Maybe (List.List a)"), "Maybe (List a)");
        assert_eq!(show("List.List (Maybe.Maybe a) -> Basics.Int"), "List (Maybe a) -> Int");
    }

    #[test]
    fn units_tuples_and_records_round_trip() {
        assert_eq!(show("()"), "()");
        assert_eq!(show("( a, b )"), "( a, b )");
        assert_eq!(show("( a, b, c )"), "( a, b, c )");
        assert_eq!(show("{}"), "{}");
        assert_eq!(
            show("{ url : String.String, size : Maybe.Maybe Basics.Int }"),
            "{ url : String, size : Maybe Int }"
        );
        assert_eq!(show("{ a | x : Basics.Int }"), "{ a | x : Int }");
    }

    /// A single parenthesized type is not a one-tuple.
    #[test]
    fn redundant_parentheses_disappear() {
        assert_eq!(show("(Basics.Int)"), "Int");
        assert_eq!(parse_type("(Basics.Int)"), parse_type("Basics.Int"));
    }

    #[test]
    fn nonsense_is_rejected_rather_than_half_parsed() {
        assert!(parse_type("a ->").is_none());
        assert!(parse_type("{ a | }").is_none());
        assert!(parse_type("( a, )").is_none());
        assert!(parse_type("a b").is_none());
    }

    /// Every type in every cached package must parse, or a diff would show
    /// spurious changes. Skipped when there is no cache to read.
    #[test]
    fn every_cached_docs_json_parses() {
        let root = crate::packages::packages_root();
        let Ok(authors) = std::fs::read_dir(&root) else { return };
        let mut checked = 0;
        for author in authors.flatten() {
            let Ok(names) = std::fs::read_dir(author.path()) else { continue };
            for name in names.flatten() {
                let Ok(versions) = std::fs::read_dir(name.path()) else { continue };
                for version in versions.flatten() {
                    let path = version.path().join("docs.json");
                    let Ok(text) = std::fs::read_to_string(&path) else { continue };
                    let docs = parse(&text, Names::Short)
                        .unwrap_or_else(|| panic!("could not read {}", path.display()));
                    for (module, api) in &docs {
                        let where_ = || format!("{} {module}", path.display());
                        for (n, v) in &api.values {
                            assert_ne!(v.tipe, unparsed(), "{}: value {n}", where_());
                        }
                        for (n, a) in &api.aliases {
                            assert_ne!(a.tipe, unparsed(), "{}: alias {n}", where_());
                        }
                        for (n, u) in &api.unions {
                            for (case, args) in &u.cases {
                                for arg in args {
                                    assert_ne!(*arg, unparsed(), "{}: {n}.{case}", where_());
                                }
                            }
                        }
                        checked += 1;
                    }
                }
            }
        }
        assert!(checked > 0, "no cached docs.json found under {}", root.display());
    }

    fn unparsed() -> Type {
        Type::Type("?".to_string(), Vec::new())
    }
}
