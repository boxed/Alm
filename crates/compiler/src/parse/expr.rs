//! Port of `Parse.Expression`.

use super::{one_of, pattern, IndentCheck, NumberLit, PResult, Parser};
use crate::ast::source::{Def, Expr, Expr_, VarType};
use crate::data::Name;
use crate::reporting::{Located, Position};

// TERMS

pub fn term(p: &mut Parser) -> PResult<Expr> {
    let start = p.position();
    match p.peek() {
        Some(b'"') => {
            let s = p.string()?;
            Ok(Located::at(start, p.position(), Expr_::Str(s)))
        }
        Some(b'\'') => {
            let c = p.character()?;
            Ok(Located::at(start, p.position(), Expr_::Chr(c)))
        }
        Some(b) if b.is_ascii_digit() => match p.number()? {
            NumberLit::Int(n) => Ok(Located::at(start, p.position(), Expr_::Int(n))),
            NumberLit::Float(f) => Ok(Located::at(start, p.position(), Expr_::Float(f))),
        },
        Some(b'[') if p.src_from_here().starts_with(b"[glsl|") => shader(p),
        Some(b'[') => list(p),
        Some(b'{') => {
            let expr = record(p)?;
            accessible(p, start, expr)
        }
        Some(b'(') => {
            let expr = tuple(p)?;
            accessible(p, start, expr)
        }
        Some(b'.') => {
            p.bump(1);
            let field = p.lower_name("a field name after this `.`")?;
            Ok(Located::at(start, p.position(), Expr_::Accessor(field)))
        }
        _ if p.starts_lower() || p.starts_upper() => {
            let (qual, name, is_upper) = p.qualified_name("an expression")?;
            let var_type = if is_upper {
                VarType::CapVar
            } else {
                VarType::LowVar
            };
            let expr_ = match qual {
                Some(q) => Expr_::VarQual(var_type, q, name),
                None => Expr_::Var(var_type, name),
            };
            let expr = Located::at(start, p.position(), expr_);
            accessible(p, start, expr)
        }
        _ => Err(p.error("Expecting an expression")),
    }
}

/// Port of `accessible`: chomp `.field` chains directly after a term.
fn accessible(p: &mut Parser, start: Position, expr: Expr) -> PResult<Expr> {
    let mut expr = expr;
    while p.peek() == Some(b'.') && p.char_at(1).is_some_and(super::is_lower_start) {
        p.bump(1);
        let field_start = p.position();
        let field = p.lower_name("a field name after this `.`")?;
        let end = p.position();
        let field_located = Located::at(field_start, end, field);
        expr = Located::at(start, end, Expr_::Access(Box::new(expr), field_located));
    }
    Ok(expr)
}

// GLSL SHADERS

fn shader(p: &mut Parser) -> PResult<Expr> {
    let start = p.position();
    let src = p.shader()?;
    let info = parse_glsl(&src);
    Ok(Located::at(start, p.position(), Expr_::Shader(info.with_src(src))))
}

/// The `attribute`/`uniform`/`varying` names collected while scanning a GLSL
/// block. Deliberately lenient: we only need the declared names.
struct GlslInfo {
    attributes: Vec<Name>,
    uniforms: Vec<Name>,
    varyings: Vec<Name>,
}

impl GlslInfo {
    fn with_src(self, src: String) -> crate::ast::source::Shader {
        crate::ast::source::Shader {
            src,
            attributes: self.attributes,
            uniforms: self.uniforms,
            varyings: self.varyings,
        }
    }
}

/// Scan GLSL source for `attribute TYPE name;`, `uniform TYPE name;`, and
/// `varying TYPE name;` declarations, matching how the Elm compiler harvests
/// the shader's input/output variables. The last whitespace-separated word
/// before the terminating `;` is the variable name.
fn parse_glsl(src: &str) -> GlslInfo {
    let mut attributes = Vec::new();
    let mut uniforms = Vec::new();
    let mut varyings = Vec::new();
    for raw in src.split(';') {
        let stmt = raw.trim();
        let mut words = stmt.split_whitespace();
        let bucket = match words.next() {
            Some("attribute") => &mut attributes,
            Some("uniform") => &mut uniforms,
            Some("varying") => &mut varyings,
            _ => continue,
        };
        // `<qualifier> <type> <name>` — the name is the final token, minus
        // any array suffix like `name[3]`.
        if let Some(last) = stmt.split_whitespace().last() {
            let name = last.split('[').next().unwrap_or(last);
            if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
                let n = Name::from(name);
                if !bucket.contains(&n) {
                    bucket.push(n);
                }
            }
        }
    }
    GlslInfo {
        attributes,
        uniforms,
        varyings,
    }
}

// LISTS

fn list(p: &mut Parser) -> PResult<Expr> {
    let start = p.position();
    p.eat_byte(b'[', "a list")?;
    p.chomp_and_check_indent("I was expecting an expression or `]`")?;
    if p.peek() == Some(b']') {
        p.bump(1);
        return Ok(Located::at(start, p.position(), Expr_::List(vec![])));
    }
    let mut entries = vec![expression(p)?];
    p.sep_until(
        b']',
        IndentCheck::NoChomp,
        expression,
        &mut entries,
        "I was in the middle of a list and need `,` or `]` to be indented",
        "I was expecting another list entry",
        "I was expecting a `,` or `]` in this list",
    )?;
    Ok(Located::at(start, p.position(), Expr_::List(entries)))
}

// TUPLES, PARENS, OPERATOR REFERENCES

fn tuple(p: &mut Parser) -> PResult<Expr> {
    let start = p.position();
    p.eat_byte(b'(', "an expression")?;
    let before = p.position();
    p.chomp_and_check_indent("I was expecting an expression after this `(`")?;
    let after = p.position();

    if before == after {
        // No space after `(` — could be an operator reference or unit.
        if p.peek() == Some(b')') {
            p.bump(1);
            return Ok(Located::at(start, p.position(), Expr_::Unit));
        }
        let snapshot = p.save();
        if let Ok(op) = p.operator() {
            if op.as_str() == "-" {
                if p.peek() == Some(b')') {
                    p.bump(1);
                    return Ok(Located::at(start, p.position(), Expr_::Op(op)));
                }
                // `(-x)` — negated expression
                let negated = term(p)?;
                let neg_end = negated.region.end;
                let neg =
                    Located::at(after, neg_end, Expr_::Negate(Box::new(negated)));
                p.chomp_space()?;
                let entry = chomp_expr_end(p, after, Vec::new(), neg, Vec::new())?;
                p.check_indent("I was expecting the closing `)` to be indented")?;
                return chomp_tuple_end(p, start, entry);
            } else {
                p.eat_byte(b')', "a closing `)` after this operator")?;
                return Ok(Located::at(start, p.position(), Expr_::Op(op)));
            }
        }
        p.restore(snapshot);
    }

    let entry = expression(p)?;
    p.check_indent("I was expecting the closing `)` to be indented")?;
    chomp_tuple_end(p, start, entry)
}

fn chomp_tuple_end(p: &mut Parser, start: Position, first: Expr) -> PResult<Expr> {
    let mut rest = Vec::new();
    loop {
        match p.peek() {
            Some(b',') => {
                p.bump(1);
                p.chomp_and_check_indent("I was expecting another tuple entry")?;
                rest.push(expression(p)?);
                p.check_indent("I was expecting the closing `)` to be indented")?;
            }
            Some(b')') => {
                p.bump(1);
                let end = p.position();
                return match rest.len() {
                    0 => Ok(first),
                    _ => {
                        let mut it = rest.into_iter();
                        let second = it.next().unwrap();
                        Ok(Located::at(
                            start,
                            end,
                            Expr_::Tuple(Box::new(first), Box::new(second), it.collect()),
                        ))
                    }
                };
            }
            _ => return Err(p.error("I was expecting a `,` or `)` here")),
        }
    }
}

// RECORDS

fn record(p: &mut Parser) -> PResult<Expr> {
    let start = p.position();
    p.eat_byte(b'{', "a record")?;
    p.chomp_and_check_indent("I was expecting a record field or `}`")?;
    if p.peek() == Some(b'}') {
        p.bump(1);
        return Ok(Located::at(start, p.position(), Expr_::Record(vec![])));
    }
    let starter = p.located(|p| p.lower_name("a record field name"))?;
    p.chomp_and_check_indent("I was expecting `=` or `|` after this field name")?;
    match p.peek() {
        Some(b'|') => {
            p.bump(1);
            p.chomp_and_check_indent("I was expecting a field to update")?;
            let mut fields = vec![field(p)?];
            p.sep_until(
                b'}',
                IndentCheck::NoChomp,
                field,
                &mut fields,
                "I was in the middle of a record update",
                "I was expecting another field",
                "I was expecting a `,` or `}` in this record",
            )?;
            Ok(Located::at(start, p.position(), Expr_::Update(starter, fields)))
        }
        Some(b'=') => {
            p.bump(1);
            p.chomp_and_check_indent("I was expecting an expression after this `=`")?;
            let value = expression(p)?;
            p.check_indent("I was in the middle of a record")?;
            let mut fields = vec![(starter, value)];
            loop {
                match p.peek() {
                    Some(b',') => {
                        p.bump(1);
                        p.chomp_and_check_indent("I was expecting another field")?;
                        fields.push(field(p)?);
                        p.check_indent("I was in the middle of a record")?;
                    }
                    Some(b'}') => {
                        p.bump(1);
                        return Ok(Located::at(start, p.position(), Expr_::Record(fields)));
                    }
                    _ => return Err(p.error("I was expecting a `,` or `}` in this record")),
                }
            }
        }
        _ => Err(p.error("I was expecting `=` or `|` after this record field name")),
    }
}

fn field(p: &mut Parser) -> PResult<(Located<Name>, Expr)> {
    let key = p.located(|p| p.lower_name("a record field name"))?;
    p.chomp_and_check_indent("I was expecting `=` after this field name")?;
    p.eat_byte(b'=', "an `=` after this record field name")?;
    p.chomp_and_check_indent("I was expecting an expression after this `=`")?;
    let value = expression(p)?;
    p.check_indent("I was in the middle of a record")?;
    Ok((key, value))
}

// EXPRESSIONS
//
// Like the Haskell `Space.Parser`, `expression` consumes trailing whitespace;
// the pre-space end position is `expr.region.end`.

pub fn expression(p: &mut Parser) -> PResult<Expr> {
    let start = p.position();
    match p.peek() {
        Some(b'l') if starts_keyword(p, "let") => let_(p),
        Some(b'i') if starts_keyword(p, "if") => if_(p),
        Some(b'c') if starts_keyword(p, "case") => case_(p),
        Some(b'\\') => function(p),
        _ => {
            let expr = possibly_negative_term(p)?;
            p.chomp_space()?;
            chomp_expr_end(p, start, Vec::new(), expr, Vec::new())
        }
    }
}

fn starts_keyword(p: &Parser, kw: &str) -> bool {
    p.src_from_here().starts_with(kw.as_bytes())
        && !p
            .peek_at(kw.len())
            .is_some_and(super::is_inner_char)
}

fn possibly_negative_term(p: &mut Parser) -> PResult<Expr> {
    let start = p.position();
    if p.peek() == Some(b'-') && !p.peek_at(1).is_some_and(|b| b == b' ' || b == b'-') {
        p.bump(1);
        let expr = term(p)?;
        let end = expr.region.end;
        return Ok(Located::at(start, end, Expr_::Negate(Box::new(expr))));
    }
    term(p)
}

fn to_call(func: Expr, args: Vec<Expr>) -> Expr {
    if args.is_empty() {
        func
    } else {
        let region = func.region.merge(args.last().unwrap().region);
        Located::new(region, Expr_::Call(Box::new(func), args))
    }
}

/// Port of `chompExprEnd`: after a leading term, keep chomping arguments
/// and binary operators for as long as things stay indented.
fn chomp_expr_end(
    p: &mut Parser,
    start: Position,
    mut ops: Vec<(Expr, Located<Name>)>,
    mut expr: Expr,
    mut args: Vec<Expr>,
) -> PResult<Expr> {
    // Invariant: whitespace has been chomped before each iteration.
    let mut end = args.last().map(|a| a.region.end).unwrap_or(expr.region.end);
    loop {
        if p.col <= p.indent || p.is_at_end() {
            break;
        }

        // Try an argument.
        let snapshot = p.save();
        match term(p) {
            Ok(arg) => {
                end = arg.region.end;
                args.push(arg);
                p.chomp_space()?;
                continue;
            }
            Err(_) => p.restore(snapshot),
        }

        // Try a binary operator.
        let op_start = p.position();
        let op_name = match p.operator() {
            Ok(op) => op,
            Err(_) => {
                p.restore(snapshot);
                break;
            }
        };
        let op_end = p.position();
        let op = Located::at(op_start, op_end, op_name.clone());
        p.chomp_and_check_indent(&format!(
            "I was expecting an expression after the `{}` operator",
            op_name
        ))?;
        let new_start = p.position();

        if op_name.as_str() == "-" && end != op_start && op_end == new_start {
            // Space before the `-` but not after: negated argument, `f -x`.
            let negated = term(p)?;
            let neg_end = negated.region.end;
            let arg = Located::at(op_start, neg_end, Expr_::Negate(Box::new(negated)));
            end = neg_end;
            args.push(arg);
            p.chomp_space()?;
            continue;
        }

        let left = to_call(expr, std::mem::take(&mut args));
        ops.push((left, op));

        let snapshot2 = p.save();
        match possibly_negative_term(p) {
            Ok(new_expr) => {
                end = new_expr.region.end;
                expr = new_expr;
                p.chomp_space()?;
                continue;
            }
            Err(_) => p.restore(snapshot2),
        }

        // The right side is a let/if/case/lambda: it is the final expression.
        let last = one_of(
            p,
            &mut [
                &mut let_,
                &mut case_,
                &mut if_,
                &mut function,
                &mut |p: &mut Parser| {
                    Err(p.error(format!(
                        "I was expecting an expression after the `{}` operator",
                        op_name
                    )))
                },
            ],
        )?;
        let last_end = last.region.end;
        let final_expr = Expr_::Binops(std::mem::take(&mut ops), Box::new(last));
        return Ok(Located::at(start, last_end, final_expr));
    }

    // Done.
    let result = to_call(expr, args);
    if ops.is_empty() {
        Ok(result)
    } else {
        let end = result.region.end;
        Ok(Located::at(
            start,
            end,
            Expr_::Binops(ops, Box::new(result)),
        ))
    }
}

// IF EXPRESSIONS

fn if_(p: &mut Parser) -> PResult<Expr> {
    let start = p.position();
    p.keyword("if")?;
    let mut branches = Vec::new();
    loop {
        p.chomp_and_check_indent("I was expecting a condition after `if`")?;
        let condition = expression(p)?;
        p.check_indent("I was expecting `then` to be indented")?;
        p.keyword("then")?;
        p.chomp_and_check_indent("I was expecting an expression after `then`")?;
        let then_branch = expression(p)?;
        p.check_indent("I was expecting `else` to be indented")?;
        p.keyword("else")?;
        p.chomp_and_check_indent("I was expecting an expression after `else`")?;
        branches.push((condition, then_branch));
        if starts_keyword(p, "if") {
            p.keyword("if")?;
            continue;
        }
        let else_branch = expression(p)?;
        let end = else_branch.region.end;
        return Ok(Located::at(
            start,
            end,
            Expr_::If(branches, Box::new(else_branch)),
        ));
    }
}

// LAMBDAS

fn function(p: &mut Parser) -> PResult<Expr> {
    let start = p.position();
    p.eat_byte(b'\\', "a lambda")?;
    p.chomp_and_check_indent("I was expecting an argument pattern after this `\\`")?;
    let mut arg_patterns = vec![pattern::term(p)?];
    p.chomp_and_check_indent("I was expecting `->` or another argument")?;
    loop {
        if p.src_from_here().starts_with(b"->") {
            p.bump(2);
            break;
        }
        arg_patterns.push(pattern::term(p)?);
        p.chomp_and_check_indent("I was expecting `->` or another argument")?;
    }
    p.chomp_and_check_indent("I was expecting the body of this lambda")?;
    let body = expression(p)?;
    let end = body.region.end;
    Ok(Located::at(
        start,
        end,
        Expr_::Lambda(arg_patterns, Box::new(body)),
    ))
}

// CASE EXPRESSIONS

fn case_(p: &mut Parser) -> PResult<Expr> {
    let start = p.position();
    p.keyword("case")?;
    p.chomp_and_check_indent("I was expecting an expression after `case`")?;
    let scrutinee = expression(p)?;
    p.check_indent("I was expecting `of` to be indented")?;
    p.keyword("of")?;
    p.chomp_and_check_indent("I was expecting a pattern after `of`")?;
    p.with_indent(|p| {
        let mut branches = vec![chomp_branch(p)?];
        while p.is_aligned() {
            let snapshot = p.save();
            match chomp_branch(p) {
                Ok(branch) => branches.push(branch),
                Err(err) => {
                    // An aligned token that is not a valid branch is an error
                    // (mirrors CasePatternAlignment commit semantics).
                    p.restore(snapshot);
                    if p.is_at_end() {
                        break;
                    }
                    return Err(err);
                }
            }
        }
        let end = branches
            .last()
            .map(|(_, e)| e.region.end)
            .unwrap_or(start);
        Ok(Located::at(
            start,
            end,
            Expr_::Case(Box::new(scrutinee), branches),
        ))
    })
}

fn chomp_branch(p: &mut Parser) -> PResult<(crate::ast::source::Pattern, Expr)> {
    let pat = pattern::expression(p)?;
    p.chomp_and_check_indent("I was expecting `->` after this pattern")?;
    p.eat_word("->", "an `->` arrow after this pattern")?;
    p.chomp_and_check_indent("I was expecting an expression after `->`")?;
    let branch = expression(p)?;
    Ok((pat, branch))
}

// LET EXPRESSIONS

fn let_(p: &mut Parser) -> PResult<Expr> {
    let start = p.position();
    p.keyword("let")?;
    let defs = p.with_backset_indent(3, |p| {
        p.chomp_and_check_indent("I was expecting a definition after `let`")?;
        p.with_indent(|p| {
            let mut defs = vec![chomp_let_def(p)?];
            while p.is_aligned() {
                let snapshot = p.save();
                match chomp_let_def(p) {
                    Ok(def) => defs.push(def),
                    Err(err) => {
                        p.restore(snapshot);
                        if starts_keyword(p, "in") {
                            break;
                        }
                        return Err(err);
                    }
                }
            }
            Ok(defs)
        })
    })?;
    p.check_indent("I was expecting `in` to be indented")?;
    p.keyword("in")?;
    p.chomp_and_check_indent("I was expecting an expression after `in`")?;
    let body = expression(p)?;
    let end = body.region.end;
    Ok(Located::at(start, end, Expr_::Let(defs, Box::new(body))))
}

fn chomp_let_def(p: &mut Parser) -> PResult<Located<Def>> {
    one_of(p, &mut [&mut definition, &mut destructure])
}

/// A named definition, possibly preceded by a type annotation:
///
/// ```elm
/// name : Type
/// name arg1 arg2 = body
/// ```
pub(crate) fn definition(p: &mut Parser) -> PResult<Located<Def>> {
    let start = p.position();
    let name = p.located(|p| p.lower_name("a definition name"))?;
    p.chomp_and_check_indent("I was expecting `=` or arguments after this name")?;

    let mut annotation = None;
    let mut def_name = name.clone();
    if p.peek() == Some(b':') && p.peek_at(1) != Some(b':') {
        p.bump(1);
        p.chomp_and_check_indent("I was expecting a type after this `:`")?;
        let tipe = super::type_::expression(p)?;
        annotation = Some(tipe);
        p.chomp_space()?;
        if !p.is_aligned() {
            return Err(p.error(format!(
                "I just saw the type annotation for `{}`, so I was expecting its definition next, starting in the exact same column.",
                name.value
            )));
        }
        def_name = p.located(|p| p.lower_name("a definition name"))?;
        if def_name.value != name.value {
            return Err(super::ParseError::new(
                format!(
                    "I just saw the type annotation for `{}`, but this definition is named `{}`. They must match!",
                    name.value, def_name.value
                ),
                def_name.region,
            ));
        }
        p.chomp_and_check_indent("I was expecting `=` or arguments after this name")?;
    }

    let mut arg_patterns = Vec::new();
    loop {
        if p.peek() == Some(b'=') {
            p.bump(1);
            p.chomp_and_check_indent("I was expecting the body of this definition")?;
            let body = expression(p)?;
            let end = body.region.end;
            return Ok(Located::at(
                start,
                end,
                Def::Define(def_name, arg_patterns, body, annotation),
            ));
        }
        arg_patterns.push(pattern::term(p)?);
        p.chomp_and_check_indent("I was expecting `=` or another argument")?;
    }
}

fn destructure(p: &mut Parser) -> PResult<Located<Def>> {
    let start = p.position();
    let pat = pattern::term(p)?;
    p.chomp_and_check_indent("I was expecting `=` after this pattern")?;
    p.eat_byte(b'=', "an `=` after this pattern")?;
    p.chomp_and_check_indent("I was expecting an expression after `=`")?;
    let body = expression(p)?;
    let end = body.region.end;
    Ok(Located::at(start, end, Def::Destruct(pat, body)))
}
