//! Port of `Nitpick.PatternMatches` — exhaustiveness and redundancy
//! checking for pattern matches.
//!
//! The algorithm comes from "Warnings for Pattern Matching" by Luc
//! Maranget: <http://moscova.inria.fr/~maranget/papers/warn/warn.pdf>

use std::collections::HashMap;
use std::rc::Rc;

use crate::ast::canonical as can;
use crate::builtins;
use crate::data::Name;
use crate::interface::Interfaces;
use crate::reporting::Region;

#[derive(Debug, Clone)]
pub struct Error {
    pub message: String,
    pub region: Region,
}

// SIMPLIFIED PATTERNS

/// The constructors of one union: (name, arity) pairs.
type Alts = Rc<Vec<(Name, usize)>>;

#[derive(Clone)]
enum Pattern {
    Anything,
    Literal(Literal),
    Ctor(Alts, Name, Vec<Pattern>),
}

#[derive(Clone, PartialEq)]
enum Literal {
    Chr(char),
    Str(String),
    Int(i64),
}

/// Where to find the constructors of every union reachable from a module.
pub struct UnionTable {
    unions: HashMap<(Name, Name), Alts>,
}

impl UnionTable {
    pub fn new(module: &can::Module, interfaces: &Interfaces) -> UnionTable {
        let mut unions = HashMap::new();
        for union in &module.unions {
            unions.insert(
                (module.name.clone(), union.name.clone()),
                Rc::new(
                    union
                        .ctors
                        .iter()
                        .map(|c| (c.name.clone(), c.args.len()))
                        .collect::<Vec<_>>(),
                ),
            );
        }
        for (module_name, interface) in interfaces {
            for union in interface.unions.values() {
                unions.insert(
                    (module_name.clone(), union.name.clone()),
                    Rc::new(
                        union
                            .ctors
                            .iter()
                            .map(|c| (c.name.clone(), c.args.len()))
                            .collect::<Vec<_>>(),
                    ),
                );
            }
        }
        for union in builtins::UNIONS {
            unions.insert(
                (Name::from(union.module), Name::from(union.name)),
                Rc::new(
                    union
                        .ctors
                        .iter()
                        .map(|(name, args)| (Name::from(*name), args.len()))
                        .collect::<Vec<_>>(),
                ),
            );
        }
        UnionTable { unions }
    }

    fn alts(&self, home: &Name, union: &Name, fallback_ctor: &can::Ctor) -> Alts {
        self.unions
            .get(&(home.clone(), union.clone()))
            .cloned()
            .unwrap_or_else(|| {
                // An opaque union imported without (..): we know at least
                // how many constructors it has.
                Rc::new(vec![(
                    fallback_ctor.name.clone(),
                    fallback_ctor.arity as usize,
                )])
            })
    }
}

fn synthetic(names: &[(&str, usize)]) -> Alts {
    Rc::new(
        names
            .iter()
            .map(|(n, a)| (Name::from(*n), *a))
            .collect::<Vec<_>>(),
    )
}

fn simplify(table: &UnionTable, pattern: &can::Pattern) -> Pattern {
    use can::Pattern_::*;
    match &pattern.value {
        Anything | Var(_) | Record(_) => Pattern::Anything,
        Alias(inner, _) => simplify(table, inner),
        Unit => Pattern::Ctor(synthetic(&[("#0", 0)]), Name::from("#0"), vec![]),
        Tuple(a, b, rest) => match rest.first() {
            None => Pattern::Ctor(
                synthetic(&[("#2", 2)]),
                Name::from("#2"),
                vec![simplify(table, a), simplify(table, b)],
            ),
            Some(c) => Pattern::Ctor(
                synthetic(&[("#3", 3)]),
                Name::from("#3"),
                vec![simplify(table, a), simplify(table, b), simplify(table, c)],
            ),
        },
        Ctor(home, union, ctor, args) => {
            let alts = table.alts(home, union, ctor);
            Pattern::Ctor(
                alts,
                ctor.name.clone(),
                args.iter().map(|a| simplify(table, a)).collect(),
            )
        }
        List(entries) => {
            let mut result = nil();
            for entry in entries.iter().rev() {
                result = cons(simplify(table, entry), result);
            }
            result
        }
        Cons(head, tail) => cons(simplify(table, head), simplify(table, tail)),
        Chr(c) => Pattern::Literal(Literal::Chr(*c)),
        Str(s) => Pattern::Literal(Literal::Str(s.clone())),
        Int(n) => Pattern::Literal(Literal::Int(*n)),
    }
}

fn list_alts() -> Alts {
    synthetic(&[("[]", 0), ("::", 2)])
}

fn nil() -> Pattern {
    Pattern::Ctor(list_alts(), Name::from("[]"), vec![])
}

fn cons(head: Pattern, tail: Pattern) -> Pattern {
    Pattern::Ctor(list_alts(), Name::from("::"), vec![head, tail])
}

// CHECK

pub fn check(module: &can::Module, interfaces: &Interfaces) -> Result<(), Vec<Error>> {
    let table = UnionTable::new(module, interfaces);
    let mut errors = Vec::new();
    for group in &module.decls {
        match group {
            can::DeclGroup::Value(def) => check_def(&table, def, &mut errors),
            can::DeclGroup::Recursive(defs) => {
                for def in defs {
                    check_def(&table, def, &mut errors);
                }
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn check_def(table: &UnionTable, def: &can::Def, errors: &mut Vec<Error>) {
    for arg in &def.args {
        check_arg(table, arg, errors);
    }
    check_expr(table, &def.body, errors);
}

fn check_arg(table: &UnionTable, pattern: &can::Pattern, errors: &mut Vec<Error>) {
    check_patterns(
        table,
        pattern.region,
        Context::Arg,
        std::slice::from_ref(pattern),
        errors,
    );
}

fn check_expr(table: &UnionTable, expr: &can::Expr, errors: &mut Vec<Error>) {
    use can::Expr_::*;
    match &expr.value {
        VarLocal(_) | VarTopLevel(_) | VarForeign(..) | VarCtor(..) | Chr(_) | Str(_)
        | Int(_) | Float(_) | Accessor(_) | Unit | Shader(_) => {}
        List(entries) => entries.iter().for_each(|e| check_expr(table, e, errors)),
        Negate(inner) => check_expr(table, inner, errors),
        Binop(_, _, _, left, right) => {
            check_expr(table, left, errors);
            check_expr(table, right, errors);
        }
        Lambda(args, body) => {
            args.iter().for_each(|a| check_arg(table, a, errors));
            check_expr(table, body, errors);
        }
        Call(func, args) => {
            check_expr(table, func, errors);
            args.iter().for_each(|a| check_expr(table, a, errors));
        }
        If(branches, otherwise) => {
            for (condition, branch) in branches {
                check_expr(table, condition, errors);
                check_expr(table, branch, errors);
            }
            check_expr(table, otherwise, errors);
        }
        Let(decls, body) => {
            for decl in decls {
                match decl {
                    can::LetDecl::Def(def) => check_def(table, def, errors),
                    can::LetDecl::Recursive(defs) => {
                        defs.iter().for_each(|d| check_def(table, d, errors))
                    }
                    can::LetDecl::Destruct(pattern, value) => {
                        check_patterns(
                            table,
                            pattern.region,
                            Context::Destruct,
                            std::slice::from_ref(pattern),
                            errors,
                        );
                        check_expr(table, value, errors);
                    }
                }
            }
            check_expr(table, body, errors);
        }
        Case(scrutinee, branches) => {
            check_expr(table, scrutinee, errors);
            let patterns: Vec<can::Pattern> =
                branches.iter().map(|(p, _)| p.clone()).collect();
            check_patterns(table, expr.region, Context::Case, &patterns, errors);
            for (_, branch) in branches {
                check_expr(table, branch, errors);
            }
        }
        Access(record, _) => check_expr(table, record, errors),
        Update(record, fields) => {
            check_expr(table, record, errors);
            fields.iter().for_each(|(_, e)| check_expr(table, e, errors));
        }
        Record(fields) => fields.iter().for_each(|(_, e)| check_expr(table, e, errors)),
        Tuple(a, b, rest) => {
            check_expr(table, a, errors);
            check_expr(table, b, errors);
            rest.iter().for_each(|e| check_expr(table, e, errors));
        }
    }
}

enum Context {
    Arg,
    Destruct,
    Case,
}

fn check_patterns(
    table: &UnionTable,
    region: Region,
    context: Context,
    patterns: &[can::Pattern],
    errors: &mut Vec<Error>,
) {
    // Build the matrix, checking each new row for usefulness (redundancy).
    let mut matrix: Vec<Vec<Pattern>> = Vec::new();
    for (index, pattern) in patterns.iter().enumerate() {
        let row = vec![simplify(table, pattern)];
        if is_useful(&matrix, &row) {
            matrix.push(row);
        } else {
            errors.push(Error {
                message: format!(
                    "This pattern is redundant: branch {} of this `case` already covers everything it matches. Remove it.",
                    index + 1
                ),
                region: pattern.region,
            });
            return;
        }
    }

    let missing = is_exhaustive(&matrix, 1);
    if !missing.is_empty() {
        let rendered: Vec<String> = missing
            .iter()
            .map(|row| render_pattern(&row[0], false))
            .collect();
        let message = match context {
            Context::Case => format!(
                "This `case` does not handle all possible values. It is missing:\n\n    {}\n\nAdd branches for these patterns (or a catch-all like `_`).",
                rendered.join("\n    ")
            ),
            Context::Arg => format!(
                "This argument pattern does not match all possible values. It is missing:\n\n    {}\n\nArgument patterns must match everything; use a `case` to handle alternatives.",
                rendered.join("\n    ")
            ),
            Context::Destruct => format!(
                "This destructuring pattern does not match all possible values. It is missing:\n\n    {}\n\nDestructuring patterns must match everything; use a `case` to handle alternatives.",
                rendered.join("\n    ")
            ),
        };
        errors.push(Error { message, region });
    }
}

// EXHAUSTIVENESS — port of `isExhaustive`

fn is_exhaustive(matrix: &[Vec<Pattern>], n: usize) -> Vec<Vec<Pattern>> {
    if matrix.is_empty() {
        return vec![vec![Pattern::Anything; n]];
    }
    if n == 0 {
        return vec![];
    }

    let ctors = collect_ctors(matrix);
    if ctors.is_empty() {
        let sub_matrix: Vec<Vec<Pattern>> = matrix
            .iter()
            .filter_map(|row| specialize_row_by_anything(row))
            .collect();
        return is_exhaustive(&sub_matrix, n - 1)
            .into_iter()
            .map(|mut rest| {
                rest.insert(0, Pattern::Anything);
                rest
            })
            .collect();
    }

    let alts = ctors.values().next().unwrap().clone();
    let num_seen = ctors.len();
    if num_seen < alts.len() {
        // Some constructors are missing entirely.
        let sub_matrix: Vec<Vec<Pattern>> = matrix
            .iter()
            .filter_map(|row| specialize_row_by_anything(row))
            .collect();
        let rest_rows = is_exhaustive(&sub_matrix, n - 1);
        let mut result = Vec::new();
        for rest in rest_rows {
            for (name, arity) in alts.iter() {
                if !ctors.contains_key(name) {
                    let mut row = Vec::with_capacity(n);
                    row.push(Pattern::Ctor(
                        alts.clone(),
                        name.clone(),
                        vec![Pattern::Anything; *arity],
                    ));
                    row.extend(rest.iter().cloned());
                    result.push(row);
                }
            }
        }
        return result;
    }

    // All constructors seen; recurse into each alternative.
    let mut result = Vec::new();
    for (name, arity) in alts.iter() {
        let sub_matrix: Vec<Vec<Pattern>> = matrix
            .iter()
            .filter_map(|row| specialize_row_by_ctor(name, *arity, row))
            .collect();
        for row in is_exhaustive(&sub_matrix, arity + n - 1) {
            let (args, rest) = row.split_at(*arity);
            let mut recovered = Vec::with_capacity(n);
            recovered.push(Pattern::Ctor(alts.clone(), name.clone(), args.to_vec()));
            recovered.extend(rest.iter().cloned());
            result.push(recovered);
        }
    }
    result
}

// USEFULNESS — port of `isUseful`

fn is_useful(matrix: &[Vec<Pattern>], vector: &[Pattern]) -> bool {
    if matrix.is_empty() {
        return true;
    }
    let Some((first, rest)) = vector.split_first() else {
        return false;
    };
    match first {
        Pattern::Ctor(_, name, args) => {
            let sub_matrix: Vec<Vec<Pattern>> = matrix
                .iter()
                .filter_map(|row| specialize_row_by_ctor(name, args.len(), row))
                .collect();
            let mut new_vector: Vec<Pattern> = args.clone();
            new_vector.extend(rest.iter().cloned());
            is_useful(&sub_matrix, &new_vector)
        }
        Pattern::Anything => match is_complete(matrix) {
            None => {
                let sub_matrix: Vec<Vec<Pattern>> = matrix
                    .iter()
                    .filter_map(|row| specialize_row_by_anything(row))
                    .collect();
                is_useful(&sub_matrix, rest)
            }
            Some(alts) => alts.iter().any(|(name, arity)| {
                let sub_matrix: Vec<Vec<Pattern>> = matrix
                    .iter()
                    .filter_map(|row| specialize_row_by_ctor(name, *arity, row))
                    .collect();
                let mut new_vector = vec![Pattern::Anything; *arity];
                new_vector.extend(rest.iter().cloned());
                is_useful(&sub_matrix, &new_vector)
            }),
        },
        Pattern::Literal(literal) => {
            let sub_matrix: Vec<Vec<Pattern>> = matrix
                .iter()
                .filter_map(|row| specialize_row_by_literal(literal, row))
                .collect();
            is_useful(&sub_matrix, rest)
        }
    }
}

fn specialize_row_by_ctor(name: &Name, arity: usize, row: &[Pattern]) -> Option<Vec<Pattern>> {
    match row.split_first() {
        Some((Pattern::Ctor(_, row_name, args), rest)) => {
            if row_name == name {
                let mut out = args.clone();
                out.extend(rest.iter().cloned());
                Some(out)
            } else {
                None
            }
        }
        Some((Pattern::Anything, rest)) => {
            let mut out = vec![Pattern::Anything; arity];
            out.extend(rest.iter().cloned());
            Some(out)
        }
        Some((Pattern::Literal(_), _)) => {
            unreachable!("ctors and literals cannot align after type checking")
        }
        None => unreachable!("empty rows are never specialized"),
    }
}

fn specialize_row_by_literal(literal: &Literal, row: &[Pattern]) -> Option<Vec<Pattern>> {
    match row.split_first() {
        Some((Pattern::Literal(lit), rest)) => {
            if lit == literal {
                Some(rest.to_vec())
            } else {
                None
            }
        }
        Some((Pattern::Anything, rest)) => Some(rest.to_vec()),
        Some((Pattern::Ctor(..), _)) => {
            unreachable!("ctors and literals cannot align after type checking")
        }
        None => unreachable!("empty rows are never specialized"),
    }
}

fn specialize_row_by_anything(row: &[Pattern]) -> Option<Vec<Pattern>> {
    match row.split_first() {
        Some((Pattern::Anything, rest)) => Some(rest.to_vec()),
        _ => None,
    }
}

/// If the first column covers every constructor of its union, return the
/// union's alternatives.
fn is_complete(matrix: &[Vec<Pattern>]) -> Option<Alts> {
    let ctors = collect_ctors(matrix);
    let (_, alts) = ctors.iter().next()?;
    if ctors.len() == alts.len() {
        Some(alts.clone())
    } else {
        None
    }
}

fn collect_ctors(matrix: &[Vec<Pattern>]) -> HashMap<Name, Alts> {
    let mut ctors = HashMap::new();
    for row in matrix {
        if let Some(Pattern::Ctor(alts, name, _)) = row.first() {
            ctors.insert(name.clone(), alts.clone());
        }
    }
    ctors
}

// RENDER MISSING PATTERNS

fn render_pattern(pattern: &Pattern, needs_parens: bool) -> String {
    match pattern {
        Pattern::Anything => "_".to_string(),
        Pattern::Literal(Literal::Chr(c)) => format!("'{}'", c),
        Pattern::Literal(Literal::Str(s)) => format!("{:?}", s),
        Pattern::Literal(Literal::Int(n)) => n.to_string(),
        Pattern::Ctor(_, name, args) => match name.as_str() {
            "#0" => "()".to_string(),
            "#2" | "#3" => {
                let rendered: Vec<String> =
                    args.iter().map(|a| render_pattern(a, false)).collect();
                format!("( {} )", rendered.join(", "))
            }
            "[]" => "[]".to_string(),
            "::" => {
                let s = format!(
                    "{} :: {}",
                    render_pattern(&args[0], true),
                    render_pattern(&args[1], false)
                );
                if needs_parens {
                    format!("({})", s)
                } else {
                    s
                }
            }
            _ => {
                if args.is_empty() {
                    name.to_string()
                } else {
                    let rendered: Vec<String> =
                        args.iter().map(|a| render_pattern(a, true)).collect();
                    let s = format!("{} {}", name, rendered.join(" "));
                    if needs_parens {
                        format!("({})", s)
                    } else {
                        s
                    }
                }
            }
        },
    }
}
