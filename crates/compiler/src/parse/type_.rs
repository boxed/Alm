//! Port of `Parse.Type`.

use super::{IndentCheck, PResult, ParseError, Parser};
use crate::ast::source::{Type, Type_};
use crate::reporting::{Located, Region};

/// Port of `Type.term`: a type that needs no parentheses.
pub fn term(p: &mut Parser) -> PResult<Type> {
    let start = p.position();
    match p.peek() {
        Some(b'(') => parens_or_tuple(p),
        Some(b'{') => record(p),
        _ if p.starts_lower() => {
            let name = p.lower_name("a type variable")?;
            Ok(Located::at(start, p.position(), Type_::Var(name)))
        }
        _ if p.starts_upper() => {
            // No arguments at term level.
            let (qual, name, _) = p.qualified_name("a type")?;
            let region = Region::new(start, p.position());
            let type_ = match qual {
                Some(q) => Type_::TypeQual(region, q, name, vec![]),
                None => Type_::Type(region, name, vec![]),
            };
            Ok(Located::at(start, p.position(), type_))
        }
        _ => Err(p.error("Expecting a type")),
    }
}

/// A type constructor possibly applied to arguments, or a plain term.
fn app(p: &mut Parser) -> PResult<Type> {
    let start = p.position();
    if !p.starts_upper() {
        return term(p);
    }
    let (qual, name, _) = p.qualified_name("a type")?;
    let name_region = Region::new(start, p.position());
    let args = p.chomp_indented_terms(term);
    let end = args.last().map(|a| a.region.end).unwrap_or(name_region.end);
    let type_ = match qual {
        Some(q) => Type_::TypeQual(name_region, q, name, args),
        None => Type_::Type(name_region, name, args),
    };
    Ok(Located::at(start, end, type_))
}

/// Port of `Type.expression`: handles `a -> b -> c`.
pub fn expression(p: &mut Parser) -> PResult<Type> {
    let start = p.position();
    let first = app(p)?;
    let snapshot = p.save();
    if p.chomp_space().is_ok() && p.col > p.indent && p.src_from_here().starts_with(b"->") {
        p.bump(2);
        p.chomp_and_check_indent("I was expecting a type after this `->`")?;
        let rest = expression(p)?;
        let end = rest.region.end;
        return Ok(Located::at(
            start,
            end,
            Type_::Lambda(Box::new(first), Box::new(rest)),
        ));
    }
    p.restore(snapshot);
    Ok(first)
}

fn parens_or_tuple(p: &mut Parser) -> PResult<Type> {
    let start = p.position();
    p.eat_byte(b'(', "a type")?;
    p.chomp_and_check_indent("I was expecting a type after this `(`")?;
    if p.peek() == Some(b')') {
        p.bump(1);
        return Ok(Located::at(start, p.position(), Type_::Unit));
    }
    let first = expression(p)?;
    p.chomp_and_check_indent("I was in the middle of a parenthesized type")?;
    let (end, first, rest) = p.chomp_tuple_items(
        first,
        expression,
        IndentCheck::Chomp,
        "I was expecting another type",
        |x| x.region.end,
        |r| ParseError::new("I was expecting a `,` or `)` in this type", r),
    )?;
    if rest.is_empty() {
        Ok(first)
    } else {
        let mut it = rest.into_iter();
        let second = it.next().unwrap();
        Ok(Located::at(
            start,
            end,
            Type_::Tuple(Box::new(first), Box::new(second), it.collect()),
        ))
    }
}

fn record(p: &mut Parser) -> PResult<Type> {
    let start = p.position();
    p.eat_byte(b'{', "a record type")?;
    p.chomp_and_check_indent("I was expecting a record type field")?;
    if p.peek() == Some(b'}') {
        p.bump(1);
        return Ok(Located::at(start, p.position(), Type_::Record(vec![], None)));
    }
    let first_name = p.located(|p| p.lower_name("a field name or type variable"))?;
    p.chomp_and_check_indent("I was in the middle of a record type")?;

    // `{ ext | field : Type }` — extensible record
    if p.peek() == Some(b'|') {
        p.bump(1);
        p.chomp_and_check_indent("I was expecting a field after this `|`")?;
        let mut fields = vec![field(p)?];
        p.sep_until(
            b'}',
            IndentCheck::Chomp,
            field,
            &mut fields,
            "I was expecting another field",
            |r| ParseError::new("I was expecting a `,` or `}` in this record type", r),
        )?;
        return Ok(Located::at(
            start,
            p.position(),
            Type_::Record(fields, Some(first_name)),
        ));
    }

    // Plain record: first_name must be followed by `:`
    p.eat_byte(b':', "a `:` after this record field name")?;
    p.chomp_and_check_indent("I was expecting a type after this `:`")?;
    let first_type = expression(p)?;
    let mut fields = vec![(first_name, first_type)];
    p.sep_until(
        b'}',
        IndentCheck::Chomp,
        field,
        &mut fields,
        "I was expecting another field",
        |r| ParseError::new("I was expecting a `,` or `}` in this record type", r),
    )?;
    Ok(Located::at(start, p.position(), Type_::Record(fields, None)))
}

fn field(p: &mut Parser) -> PResult<(Located<crate::data::Name>, Type)> {
    let name = p.located(|p| p.lower_name("a record field name"))?;
    p.chomp_and_check_indent("I was expecting a `:` after this field name")?;
    p.eat_byte(b':', "a `:` after this record field name")?;
    p.chomp_and_check_indent("I was expecting a type after this `:`")?;
    let tipe = expression(p)?;
    Ok((name, tipe))
}
