//! Port of `Parse.Module` and `Parse.Declaration`.

use super::{expr, type_, IndentCheck, PResult, ParseError, Parser};
use crate::ast::source::{
    Alias, Associativity, Def, Exposed, Exposing, Import, Infix, Module, Port, Privacy, Union,
    Value,
};
use crate::data::Name;
use crate::reporting::{Located, Region};

pub fn parse_module(source: &str) -> Result<Module, ParseError> {
    let mut p = Parser::new(source);
    let module = chomp_module(&mut p)?;
    if !p.is_at_end() {
        return Err(p.error("I got stuck here. I was expecting a top-level definition, `type`, or `import`."));
    }
    Ok(module)
}

fn chomp_module(p: &mut Parser) -> PResult<Module> {
    p.chomp_space()?;

    // HEADER — `module Name exposing (..)` or `port module ...`
    let mut is_port_module = false;
    // Region of the `module` keyword, for the UNEXPECTED PORTS diagnostic.
    let mut module_kw = Region::ZERO;
    let (name, exports) = if p.src_from_here().starts_with(b"port module") {
        is_port_module = true;
        p.keyword("port")?;
        p.chomp_and_check_indent("I was expecting `module` after `port`")?;
        chomp_header(p)?
    } else if p.src_from_here().starts_with(b"module") {
        let a = p.position();
        module_kw = Region::new(a, crate::reporting::Position::new(a.row, a.col + 6));
        chomp_header(p)?
    } else {
        (
            None,
            Located::new(Region::ZERO, Exposing::Open),
        )
    };

    p.chomp_space()?;

    // IMPORTS
    let mut imports = Vec::new();
    while p.col == 1 && p.peek_keyword("import") {
        imports.push(chomp_import(p)?);
        p.chomp_space()?;
    }

    // DECLARATIONS
    let mut values: Vec<Located<Value>> = Vec::new();
    let mut unions: Vec<Located<Union>> = Vec::new();
    let mut aliases: Vec<Located<Alias>> = Vec::new();
    let mut binops: Vec<Located<Infix>> = Vec::new();
    let mut ports: Vec<Port> = Vec::new();

    while !p.is_at_end() {
        p.check_fresh_line(
            "I was expecting a new top-level definition here, starting at the beginning of the line.",
        )?;
        if p.peek_keyword("type") {
            let start = p.position();
            p.keyword("type")?;
            p.chomp_and_check_indent("I was expecting a type name after `type`")?;
            if p.src_from_here().starts_with(b"alias") {
                p.keyword("alias")?;
                p.chomp_and_check_indent("I was expecting a name after `type alias`")?;
                let alias = chomp_alias(p)?;
                let end = p.position();
                aliases.push(Located::at(start, end, alias));
            } else {
                let union = chomp_union(p)?;
                let end = p.position();
                unions.push(Located::at(start, end, union));
            }
        } else if p.peek_keyword("infix") {
            let start = p.position();
            let infix = chomp_infix(p)?;
            let end = p.position();
            binops.push(Located::at(start, end, infix));
        } else if p.src_from_here().starts_with(b"port")
            && !p.peek_at(4).is_some_and(super::is_inner_char)
        {
            if !is_port_module {
                return Err(ParseError::from_syntax(
                    crate::reporting::syntax::SyntaxError::UnexpectedPorts { region: module_kw },
                ));
            }
            p.keyword("port")?;
            p.chomp_and_check_indent("I was expecting a port name after `port`")?;
            let name = p.located(|p| p.lower_name("a port name"))?;
            let port_stuck = || {
                ParseError::from_syntax(crate::reporting::syntax::SyntaxError::UnfinishedPort {
                    region: Region::new(name.region.end, name.region.end),
                })
            };
            p.chomp_and_check_indent("").map_err(|_| port_stuck())?;
            p.eat_byte(b':', "").map_err(|_| port_stuck())?;
            p.chomp_and_check_indent("I was expecting the port's type after `:`")?;
            let tipe = type_::expression(p)?;
            ports.push(Port { name, tipe });
        } else {
            let decl_start = p.position();
            let def = expr::definition(p).map_err(|e| {
                // Only a failure to even parse the definition name (error anchored
                // at the start of the line) is a WEIRD DECLARATION; a failure
                // further in — a missing body, a bad expression — keeps its own
                // report.
                if e.region.start == decl_start {
                    ParseError::from_syntax(
                        crate::reporting::syntax::SyntaxError::WeirdDeclaration {
                            region: Region::new(decl_start, decl_start),
                        },
                    )
                } else {
                    e
                }
            })?;
            let region = def.region;
            match def.value {
                Def::Define(name, args, body, annotation) => {
                    values.push(Located::new(
                        region,
                        Value {
                            name,
                            args,
                            body,
                            type_annotation: annotation,
                        },
                    ));
                }
                Def::Destruct(..) => {
                    return Err(ParseError::new(
                        "Destructuring definitions are not allowed at the top level.",
                        region,
                    ))
                }
            }
        }
        p.chomp_space()?;
    }

    Ok(Module {
        name,
        exports,
        imports,
        values,
        unions,
        aliases,
        binops,
        ports,
    })
}

type Header = (Option<Located<Name>>, Located<Exposing>);

fn chomp_header(p: &mut Parser) -> PResult<Header> {
    use crate::reporting::syntax::SyntaxError;
    p.keyword("module")?;
    p.chomp_and_check_indent("I was expecting the module name after `module`")?;
    let name_pos = p.position();
    let name = p.located(|p| chomp_module_name(p)).map_err(|_| {
        ParseError::from_syntax(SyntaxError::ExpectingModuleName {
            region: Region::new(name_pos, name_pos),
        })
    })?;
    p.chomp_and_check_indent("I was expecting `exposing` after the module name")?;
    p.keyword("exposing")?;
    let after_exposing = p.position();
    let unfinished = || {
        ParseError::from_syntax(SyntaxError::UnfinishedModuleDecl {
            region: Region::new(after_exposing, after_exposing),
        })
    };
    p.chomp_and_check_indent("").map_err(|_| unfinished())?;
    let exports = p.located(chomp_exposing)?;
    Ok((Some(name), exports))
}

fn chomp_module_name(p: &mut Parser) -> PResult<Name> {
    let mut full = String::new();
    loop {
        let part = p.upper_name("a module name like `Main` or `Json.Decode`")?;
        full.push_str(part.as_str());
        if p.peek() == Some(b'.') {
            p.bump(1);
            full.push('.');
        } else {
            return Ok(Name::from(full));
        }
    }
}

/// An `import` that got stuck, underlined at `pos`.
fn import_stuck(pos: crate::reporting::Position) -> ParseError {
    ParseError::from_syntax(crate::reporting::syntax::SyntaxError::UnfinishedImport {
        region: Region::new(pos, pos),
    })
}

fn chomp_import(p: &mut Parser) -> PResult<Import> {
    p.keyword("import")?;
    let name_stuck = p.position();
    p.chomp_and_check_indent("")
        .map_err(|_| import_stuck(name_stuck))?;
    let name = p
        .located(|p| chomp_module_name(p))
        .map_err(|_| import_stuck(name_stuck))?;

    let mut alias = None;
    let mut exposing = Exposing::Explicit(vec![]);

    let snapshot = p.save();
    p.chomp_space()?;
    if p.col > 1 && p.src_from_here().starts_with(b"as") && p.keyword("as").is_ok() {
        let as_stuck = p.position();
        p.chomp_and_check_indent("")
            .map_err(|_| import_stuck(as_stuck))?;
        alias = Some(
            p.upper_name("an alias like `JD`")
                .map_err(|_| import_stuck(as_stuck))?,
        );
    } else {
        p.restore(snapshot);
    }

    let snapshot = p.save();
    p.chomp_space()?;
    if p.col > 1 && p.src_from_here().starts_with(b"exposing") && p.keyword("exposing").is_ok() {
        p.chomp_and_check_indent("I was expecting `(..)` or an explicit list after `exposing`")?;
        exposing = chomp_exposing(p)?;
    } else {
        p.restore(snapshot);
    }

    Ok(Import {
        name,
        alias,
        exposing,
    })
}

fn chomp_exposing(p: &mut Parser) -> PResult<Exposing> {
    p.eat_byte(b'(', "an exposing list like `(..)` or `(name, Type)`")?;
    p.chomp_and_check_indent("I was expecting something to expose")?;
    if p.src_from_here().starts_with(b"..") {
        p.bump(2);
        p.chomp_and_check_indent("I was expecting `)` after `..`")?;
        p.eat_byte(b')', "a closing `)`")?;
        return Ok(Exposing::Open);
    }
    let mut exposed = vec![chomp_exposed(p)?];
    p.sep_until(
        b')',
        IndentCheck::Chomp,
        chomp_exposed,
        &mut exposed,
        "I was expecting another name to expose",
        |r| {
            ParseError::from_syntax(crate::reporting::syntax::SyntaxError::UnfinishedExposing {
                region: r,
            })
        },
    )
    .map_err(|e| match &e.syntax {
        // Point the caret at the end of the last exposed value: the loop chomps
        // trailing newlines, so the raw stuck position lands on a later line.
        Some(crate::reporting::syntax::SyntaxError::UnfinishedExposing { .. }) => {
            if let Some(end) = exposed.last().map(exposed_end) {
                return ParseError::from_syntax(
                    crate::reporting::syntax::SyntaxError::UnfinishedExposing {
                        region: Region::new(end, end),
                    },
                );
            }
            e
        }
        _ => e,
    })?;
    Ok(Exposing::Explicit(exposed))
}

/// The end position of an exposed value, for caret placement.
fn exposed_end(e: &Exposed) -> crate::reporting::Position {
    match e {
        Exposed::Lower(n) | Exposed::Upper(n, _) => n.region.end,
        Exposed::Operator(r, _) => r.end,
    }
}

fn chomp_exposed(p: &mut Parser) -> PResult<Exposed> {
    let start = p.position();
    match p.peek() {
        Some(b'(') => {
            p.bump(1);
            let op = p.operator()?;
            p.eat_byte(b')', "a closing `)` after this operator")?;
            let end = p.position();
            Ok(Exposed::Operator(Region::new(start, end), op))
        }
        _ if p.starts_lower() => {
            let name = p.located(|p| p.lower_name("a value name"))?;
            Ok(Exposed::Lower(name))
        }
        _ if p.starts_upper() => {
            let name = p.located(|p| p.upper_name("a type name"))?;
            // Elm allows whitespace between the type name and `(..)`, e.g.
            // `exposing ( Key (..) )`. Skip it, but only commit if `(..)`
            // actually follows; otherwise this is an opaque exposure.
            let snapshot = p.save();
            let _ = p.chomp_space();
            if p.src_from_here().starts_with(b"(..)") {
                let priv_start = p.position();
                p.bump(4);
                let end = p.position();
                Ok(Exposed::Upper(
                    name,
                    Privacy::Public(Region::new(priv_start, end)),
                ))
            } else {
                p.restore(snapshot);
                Ok(Exposed::Upper(name, Privacy::Private))
            }
        }
        _ => Err(p.error("I was expecting a name or operator to expose")),
    }
}

// TYPE DECLARATIONS

fn chomp_alias(p: &mut Parser) -> PResult<Alias> {
    let name = p.located(|p| p.upper_name("a type alias name"))?;
    p.chomp_and_check_indent("I was expecting `=` or type variables")?;
    let mut vars = Vec::new();
    while p.starts_lower() {
        vars.push(p.located(|p| p.lower_name("a type variable"))?);
        p.chomp_and_check_indent("I was expecting `=` or more type variables")?;
    }
    p.eat_byte(b'=', "an `=` in this type alias")?;
    p.chomp_and_check_indent("I was expecting a type after `=`")?;
    let tipe = type_::expression(p)?;
    Ok(Alias { name, vars, tipe })
}

fn chomp_union(p: &mut Parser) -> PResult<Union> {
    let name = p.located(|p| p.upper_name("a type name"))?;
    p.chomp_and_check_indent("I was expecting `=` or type variables")?;
    let mut vars = Vec::new();
    while p.starts_lower() {
        vars.push(p.located(|p| p.lower_name("a type variable"))?);
        p.chomp_and_check_indent("I was expecting `=` or more type variables")?;
    }
    p.eat_byte(b'=', "an `=` in this type declaration")?;
    p.chomp_and_check_indent("I was expecting a constructor after `=`")?;
    let mut ctors = vec![chomp_ctor(p)?];
    loop {
        let snapshot = p.save();
        if p.chomp_space().is_err() || p.col <= p.indent || p.peek() != Some(b'|') {
            p.restore(snapshot);
            break;
        }
        p.bump(1);
        p.chomp_and_check_indent("I was expecting a constructor after `|`")?;
        ctors.push(chomp_ctor(p)?);
    }
    Ok(Union { name, vars, ctors })
}

fn chomp_ctor(p: &mut Parser) -> PResult<(Located<Name>, Vec<crate::ast::source::Type>)> {
    let name = p.located(|p| p.upper_name("a constructor name"))?;
    let args = p.chomp_indented_terms(type_::term);
    Ok((name, args))
}

// INFIX DECLARATIONS

fn chomp_infix(p: &mut Parser) -> PResult<Infix> {
    p.keyword("infix")?;
    p.chomp_and_check_indent("I was expecting an associativity after `infix`")?;
    let associativity = if p.keyword("left").is_ok() {
        Associativity::Left
    } else if p.keyword("right").is_ok() {
        Associativity::Right
    } else if p.keyword("non").is_ok() {
        Associativity::Non
    } else {
        return Err(p.error("I was expecting `left`, `right`, or `non` after `infix`"));
    };
    p.chomp_and_check_indent("I was expecting a precedence number")?;
    let precedence = match p.number()? {
        super::NumberLit::Int(n) if (0..=9).contains(&n) => n as u8,
        _ => return Err(p.error("Precedence must be an integer from 0 to 9")),
    };
    p.chomp_and_check_indent("I was expecting an operator in parentheses")?;
    p.eat_byte(b'(', "a `(` before the operator")?;
    let op = p.operator()?;
    p.eat_byte(b')', "a `)` after the operator")?;
    p.chomp_and_check_indent("I was expecting `=` next")?;
    p.eat_byte(b'=', "an `=` in this infix declaration")?;
    p.chomp_and_check_indent("I was expecting a function name")?;
    let function = p.lower_name("the function this operator uses")?;
    Ok(Infix {
        op,
        associativity,
        precedence,
        function,
    })
}
