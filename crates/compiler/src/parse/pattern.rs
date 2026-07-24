//! Port of `Parse.Pattern`.

use super::{IndentCheck, NumberLit, PResult, ParseError, Parser};
use crate::ast::source::{Pattern, Pattern_};
use crate::reporting::{Located, Region};

/// Port of `Pattern.term`: a pattern that needs no parentheses.
pub fn term(p: &mut Parser) -> PResult<Pattern> {
    let start = p.position();
    match p.peek() {
        Some(b'(') => parens_or_tuple(p),
        Some(b'{') => record(p),
        Some(b'[') => list(p),
        Some(b'_') => {
            p.bump(1);
            if p.peek().is_some_and(super::is_inner_char) {
                Err(p.error("Wildcard patterns must be a lone underscore"))
            } else {
                Ok(Located::at(start, p.position(), Pattern_::Anything))
            }
        }
        Some(b'\'') => {
            let c = p.character()?;
            Ok(Located::at(start, p.position(), Pattern_::Chr(c)))
        }
        Some(b'"') => {
            let s = p.string()?;
            Ok(Located::at(start, p.position(), Pattern_::Str(s)))
        }
        Some(b'-') if p.peek_at(1).is_some_and(|b| b.is_ascii_digit()) => {
            p.bump(1);
            match p.number()? {
                NumberLit::Int(n) => Ok(Located::at(start, p.position(), Pattern_::Int(-n))),
                NumberLit::Float(_) => {
                    Err(p.error("I cannot pattern match on floating point numbers"))
                }
            }
        }
        Some(b) if b.is_ascii_digit() => match p.number()? {
            NumberLit::Int(n) => Ok(Located::at(start, p.position(), Pattern_::Int(n))),
            NumberLit::Float(_) => Err(p.error(
                "I cannot pattern match on floating point numbers. Equality on floats is unreliable.",
            )),
        },
        _ if p.starts_lower() => {
            let name = p.lower_name("a pattern")?;
            Ok(Located::at(start, p.position(), Pattern_::Var(name)))
        }
        _ if p.starts_upper() => {
            // A constructor with NO arguments (arguments only in `expression`
            // or inside parens).
            let (qual, name, _) = p.qualified_name("a pattern")?;
            let region = Region::new(start, p.position());
            let pattern_ = match qual {
                Some(q) => Pattern_::CtorQual(region, q, name, vec![]),
                None => Pattern_::Ctor(region, name, vec![]),
            };
            Ok(Located::at(start, p.position(), pattern_))
        }
        _ => Err(p.error("Expecting a pattern")),
    }
}

fn record(p: &mut Parser) -> PResult<Pattern> {
    let start = p.position();
    p.eat_byte(b'{', "a record pattern")?;
    p.chomp_and_check_indent("I was expecting a field name in this record pattern")?;
    if p.peek() == Some(b'}') {
        p.bump(1);
        return Ok(Located::at(start, p.position(), Pattern_::Record(vec![])));
    }
    let mut fields = vec![p.located(|p| p.lower_name("a record field name"))?];
    p.sep_until(
        b'}',
        IndentCheck::Chomp,
        |p| p.located(|p| p.lower_name("a record field name")),
        &mut fields,
        "I was expecting a field name",
        |r| ParseError::new("I was expecting a `,` or `}` in this record pattern", r),
    )?;
    Ok(Located::at(start, p.position(), Pattern_::Record(fields)))
}

fn list(p: &mut Parser) -> PResult<Pattern> {
    let start = p.position();
    p.eat_byte(b'[', "a list pattern")?;
    p.chomp_and_check_indent("I was expecting a pattern or `]`")?;
    if p.peek() == Some(b']') {
        p.bump(1);
        return Ok(Located::at(start, p.position(), Pattern_::List(vec![])));
    }
    let mut entries = vec![expression(p)?];
    p.sep_until(
        b']',
        IndentCheck::Chomp,
        expression,
        &mut entries,
        "I was expecting another pattern",
        |r| ParseError::new("I was expecting a `,` or `]` in this list pattern", r),
    )?;
    Ok(Located::at(start, p.position(), Pattern_::List(entries)))
}

fn parens_or_tuple(p: &mut Parser) -> PResult<Pattern> {
    let start = p.position();
    p.eat_byte(b'(', "a pattern")?;
    p.chomp_and_check_indent("I was expecting a pattern after this `(`")?;
    if p.peek() == Some(b')') {
        p.bump(1);
        return Ok(Located::at(start, p.position(), Pattern_::Unit));
    }
    let first = expression(p)?;
    p.chomp_and_check_indent("I was in the middle of a parenthesized pattern")?;
    let (end, first, rest) = p.chomp_tuple_items(
        first,
        expression,
        IndentCheck::Chomp,
        "I was expecting another pattern",
        "I was in the middle of a tuple pattern",
        "I was expecting a `,` or `)` in this pattern",
    )?;
    if rest.is_empty() {
        Ok(first)
    } else {
        let mut it = rest.into_iter();
        let second = it.next().unwrap();
        Ok(Located::at(
            start,
            end,
            Pattern_::Tuple(Box::new(first), Box::new(second), it.collect()),
        ))
    }
}

/// Port of `Pattern.expression`: constructor applications, cons chains, and
/// `as` aliases are allowed here.
pub fn expression(p: &mut Parser) -> PResult<Pattern> {
    let start = p.position();
    let first = term_with_args(p)?;
    cons_end(p, start, first)
}

fn cons_end(p: &mut Parser, start: crate::reporting::Position, first: Pattern) -> PResult<Pattern> {
    let snapshot = p.save();
    if p.chomp_space().is_ok() && p.col > p.indent && !p.is_at_end() {
        if p.src_from_here().starts_with(b"::") && p.peek_at(2) != Some(b':') {
            p.bump(2);
            p.chomp_and_check_indent("I was expecting a pattern after this `::`")?;
            let rest_start = p.position();
            let rest_first = term_with_args(p)?;
            let rest = cons_end(p, rest_start, rest_first)?;
            let end = rest.region.end;
            return Ok(Located::at(
                start,
                end,
                Pattern_::Cons(Box::new(first), Box::new(rest)),
            ));
        }
        if p.keyword("as").is_ok() {
            p.chomp_and_check_indent("I was expecting a name after `as`")?;
            let alias = p.located(|p| p.lower_name("an alias name"))?;
            let end = p.position();
            return Ok(Located::at(
                start,
                end,
                Pattern_::Alias(Box::new(first), alias),
            ));
        }
    }
    p.restore(snapshot);
    Ok(first)
}

/// A constructor applied to argument patterns, or a plain term.
fn term_with_args(p: &mut Parser) -> PResult<Pattern> {
    let start = p.position();
    let is_ctor = p.starts_upper();
    if !is_ctor {
        return term(p);
    }
    let (qual, name, _) = p.qualified_name("a pattern")?;
    let name_region = Region::new(start, p.position());
    let args = p.chomp_indented_terms(term);
    let end = args.last().map(|a| a.region.end).unwrap_or(name_region.end);
    let pattern_ = match qual {
        Some(q) => Pattern_::CtorQual(name_region, q, name, args),
        None => Pattern_::Ctor(name_region, name, args),
    };
    Ok(Located::at(start, end, pattern_))
}
