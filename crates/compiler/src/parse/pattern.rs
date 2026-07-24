//! Port of `Parse.Pattern`.

use super::{IndentCheck, NumberLit, PResult, ParseError, Parser};
use crate::ast::source::{Pattern, Pattern_};
use crate::reporting::syntax::SyntaxError;
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
            NumberLit::Float(_) => Err(ParseError::from_syntax(SyntaxError::PatternFloat {
                region: Region::new(start, p.position()),
            })),
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
        _ => Err(ParseError::from_syntax(SyntaxError::PatternStart {
            region: p.region_here(),
        })),
    }
}

fn record(p: &mut Parser) -> PResult<Pattern> {
    let start = p.position();
    p.eat_byte(b'{', "a record pattern")?;
    let field_err = |region| ParseError::from_syntax(SyntaxError::RecordPatternField { region });
    let end_err = |region| ParseError::from_syntax(SyntaxError::RecordPatternEnd { region });
    let open_end = p.position();
    p.chomp_and_check_indent("")
        .map_err(|_| field_err(Region::new(open_end, open_end)))?;
    if p.peek() == Some(b'}') {
        p.bump(1);
        return Ok(Located::at(start, p.position(), Pattern_::Record(vec![])));
    }
    let at = p.region_here();
    let mut fields = vec![p
        .located(|p| p.lower_name("a record field name"))
        .map_err(|_| field_err(at))?];
    loop {
        let last_end = fields.last().unwrap().region.end;
        p.chomp_and_check_indent("")
            .map_err(|_| end_err(Region::new(last_end, last_end)))?;
        match p.peek() {
            Some(b',') => {
                p.bump(1);
                let comma_end = p.position();
                p.chomp_and_check_indent("")
                    .map_err(|_| field_err(Region::new(comma_end, comma_end)))?;
                let at = p.region_here();
                fields.push(
                    p.located(|p| p.lower_name("a record field name"))
                        .map_err(|_| field_err(at))?,
                );
            }
            Some(b'}') => {
                p.bump(1);
                break;
            }
            _ => return Err(end_err(p.region_here())),
        }
    }
    Ok(Located::at(start, p.position(), Pattern_::Record(fields)))
}

fn list(p: &mut Parser) -> PResult<Pattern> {
    let start = p.position();
    p.eat_byte(b'[', "a list pattern")?;
    let open_end = p.position();
    p.chomp_and_check_indent("").map_err(|_| {
        ParseError::from_syntax(SyntaxError::ListPatternOpen {
            region: Region::new(open_end, open_end),
        })
    })?;
    if p.peek() == Some(b']') {
        p.bump(1);
        return Ok(Located::at(start, p.position(), Pattern_::List(vec![])));
    }
    let mut entries = vec![expression(p)?];
    loop {
        let last_end = entries.last().unwrap().region.end;
        // After an element, elm chomps whitespace then expects `,` or `]`. A
        // dedent/end-of-input here points back at the element's end.
        p.chomp_and_check_indent("").map_err(|_| {
            ParseError::from_syntax(SyntaxError::ListPatternIndentEnd {
                region: Region::new(last_end, last_end),
            })
        })?;
        match p.peek() {
            Some(b',') => {
                p.bump(1);
                let comma_end = p.position();
                p.chomp_and_check_indent("").map_err(|_| {
                    ParseError::from_syntax(SyntaxError::ListPatternExpr {
                        region: Region::new(comma_end, comma_end),
                    })
                })?;
                entries.push(expression(p)?);
            }
            Some(b']') => {
                p.bump(1);
                break;
            }
            // An unexpected token (not `,`/`]`) points at the token itself.
            _ => {
                return Err(ParseError::from_syntax(SyntaxError::ListPatternEnd {
                    region: p.region_here(),
                }))
            }
        }
    }
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
        |x| x.region.end,
        |r| ParseError::new("I was expecting a `,` or `)` in this pattern", r),
        |r| ParseError::new("I was expecting another pattern", r),
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
            let alias_err =
                |region| ParseError::from_syntax(SyntaxError::PatternAlias { region });
            let before = p.region_here();
            p.chomp_and_check_indent("").map_err(|_| alias_err(before))?;
            let at = p.region_here();
            let alias = p
                .located(|p| p.lower_name("an alias name"))
                .map_err(|_| alias_err(at))?;
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
