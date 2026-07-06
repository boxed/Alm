use alm_compiler::ast::source::{Def, Expr_, Pattern_, Type_};
use alm_compiler::parse::parse_module;

fn parse_ok(src: &str) -> alm_compiler::ast::source::Module {
    match parse_module(src) {
        Ok(module) => module,
        Err(err) => panic!("parse failed: {} at {:?}", err.message, err.region),
    }
}

fn parse_err(src: &str) {
    if parse_module(src).is_ok() {
        panic!("expected parse failure for:\n{}", src);
    }
}

#[test]
fn empty_module() {
    let m = parse_ok("module Main exposing (..)\n");
    assert_eq!(m.get_name().as_str(), "Main");
    assert!(m.values.is_empty());
}

#[test]
fn simple_value() {
    let m = parse_ok("module Main exposing (..)\n\nx = 42\n");
    assert_eq!(m.values.len(), 1);
    let v = &m.values[0].value;
    assert_eq!(v.name.value.as_str(), "x");
    assert_eq!(v.body.value, Expr_::Int(42));
}

#[test]
fn annotated_function() {
    let m = parse_ok(
        "module Main exposing (..)\n\nadd : Int -> Int -> Int\nadd x y =\n    x + y\n",
    );
    let v = &m.values[0].value;
    assert_eq!(v.name.value.as_str(), "add");
    assert_eq!(v.args.len(), 2);
    assert!(v.type_annotation.is_some());
    match &v.body.value {
        Expr_::Binops(pairs, _) => assert_eq!(pairs[0].1.value.as_str(), "+"),
        other => panic!("expected binops, got {:?}", other),
    }
}

#[test]
fn annotation_name_mismatch() {
    parse_err("module Main exposing (..)\n\nadd : Int\nsub = 1\n");
}

#[test]
fn dotted_module_and_imports() {
    let m = parse_ok(
        "module Json.Decode.Extra exposing (..)\n\nimport List\nimport Json.Decode as JD exposing (field, Value)\n\nx = 1\n",
    );
    assert_eq!(m.get_name().as_str(), "Json.Decode.Extra");
    assert_eq!(m.imports.len(), 2);
    assert_eq!(m.imports[1].alias.as_ref().unwrap().as_str(), "JD");
}

#[test]
fn explicit_exposing() {
    let m = parse_ok("module Main exposing (x, Maybe(..), Result, (|>))\n\nx = 1\n");
    match &m.exports.value {
        alm_compiler::ast::source::Exposing::Explicit(items) => assert_eq!(items.len(), 4),
        _ => panic!("expected explicit exposing"),
    }
}

#[test]
fn literals() {
    let m = parse_ok(
        "module M exposing (..)\n\na = 0x1F\nb = 3.14\nc = 'x'\nd = \"hello\\n\"\ne = \"\"\"multi\nline\"\"\"\nf = ()\n",
    );
    assert_eq!(m.values[0].value.body.value, Expr_::Int(31));
    assert_eq!(m.values[1].value.body.value, Expr_::Float(3.14));
    assert_eq!(m.values[2].value.body.value, Expr_::Chr('x'));
    assert_eq!(m.values[3].value.body.value, Expr_::Str("hello\n".into()));
    assert_eq!(
        m.values[4].value.body.value,
        Expr_::Str("multi\nline".into())
    );
    assert_eq!(m.values[5].value.body.value, Expr_::Unit);
}

#[test]
fn if_then_else() {
    let m = parse_ok(
        "module M exposing (..)\n\nx =\n    if a then\n        1\n    else if b then\n        2\n    else\n        3\n",
    );
    match &m.values[0].value.body.value {
        Expr_::If(branches, _) => assert_eq!(branches.len(), 2),
        other => panic!("expected if, got {:?}", other),
    }
}

#[test]
fn let_in() {
    let m = parse_ok(
        "module M exposing (..)\n\nx =\n    let\n        y =\n            1\n\n        z : Int\n        z =\n            2\n    in\n    y + z\n",
    );
    match &m.values[0].value.body.value {
        Expr_::Let(defs, _) => {
            assert_eq!(defs.len(), 2);
            match &defs[1].value {
                Def::Define(name, _, _, ann) => {
                    assert_eq!(name.value.as_str(), "z");
                    assert!(ann.is_some());
                }
                other => panic!("expected define, got {:?}", other),
            }
        }
        other => panic!("expected let, got {:?}", other),
    }
}

#[test]
fn let_destructure() {
    let m = parse_ok(
        "module M exposing (..)\n\nx =\n    let\n        ( a, b ) =\n            pair\n    in\n    a\n",
    );
    match &m.values[0].value.body.value {
        Expr_::Let(defs, _) => match &defs[0].value {
            Def::Destruct(pat, _) => match &pat.value {
                Pattern_::Tuple(..) => {}
                other => panic!("expected tuple pattern, got {:?}", other),
            },
            other => panic!("expected destruct, got {:?}", other),
        },
        other => panic!("expected let, got {:?}", other),
    }
}

#[test]
fn case_of() {
    let m = parse_ok(
        "module M exposing (..)\n\nx =\n    case maybe of\n        Just n ->\n            n\n\n        Nothing ->\n            0\n",
    );
    match &m.values[0].value.body.value {
        Expr_::Case(_, branches) => {
            assert_eq!(branches.len(), 2);
            match &branches[0].0.value {
                Pattern_::Ctor(_, name, args) => {
                    assert_eq!(name.as_str(), "Just");
                    assert_eq!(args.len(), 1);
                }
                other => panic!("expected ctor pattern, got {:?}", other),
            }
        }
        other => panic!("expected case, got {:?}", other),
    }
}

#[test]
fn case_with_cons_and_literals() {
    let m = parse_ok(
        "module M exposing (..)\n\nlen list =\n    case list of\n        [] ->\n            0\n\n        x :: rest ->\n            1 + len rest\n",
    );
    match &m.values[0].value.body.value {
        Expr_::Case(_, branches) => {
            match &branches[1].0.value {
                Pattern_::Cons(..) => {}
                other => panic!("expected cons pattern, got {:?}", other),
            }
        }
        other => panic!("expected case, got {:?}", other),
    }
}

#[test]
fn lambda() {
    let m = parse_ok("module M exposing (..)\n\nf = \\x y -> x + y\n");
    match &m.values[0].value.body.value {
        Expr_::Lambda(args, _) => assert_eq!(args.len(), 2),
        other => panic!("expected lambda, got {:?}", other),
    }
}

#[test]
fn application_and_operators() {
    let m = parse_ok("module M exposing (..)\n\ny = f a b + g c |> h\n");
    match &m.values[0].value.body.value {
        Expr_::Binops(pairs, _) => {
            assert_eq!(pairs.len(), 2);
            assert_eq!(pairs[0].1.value.as_str(), "+");
            assert_eq!(pairs[1].1.value.as_str(), "|>");
            match &pairs[0].0.value {
                Expr_::Call(_, args) => assert_eq!(args.len(), 2),
                other => panic!("expected call, got {:?}", other),
            }
        }
        other => panic!("expected binops, got {:?}", other),
    }
}

#[test]
fn negation_versus_subtraction() {
    let m = parse_ok("module M exposing (..)\n\na = x - 1\nb = -x\nc = f -x\nd = x-1\n");
    assert!(matches!(&m.values[0].value.body.value, Expr_::Binops(..)));
    assert!(matches!(&m.values[1].value.body.value, Expr_::Negate(..)));
    match &m.values[2].value.body.value {
        Expr_::Call(_, args) => assert!(matches!(&args[0].value, Expr_::Negate(..))),
        other => panic!("expected call with negated arg, got {:?}", other),
    }
    assert!(matches!(&m.values[3].value.body.value, Expr_::Binops(..)));
}

#[test]
fn records() {
    let m = parse_ok(
        "module M exposing (..)\n\np = { x = 1, y = 2 }\nq = { p | x = 3 }\ngetX r = r.x\nf = .x\n",
    );
    assert!(matches!(&m.values[0].value.body.value, Expr_::Record(f) if f.len() == 2));
    assert!(matches!(&m.values[1].value.body.value, Expr_::Update(..)));
    assert!(matches!(&m.values[2].value.body.value, Expr_::Access(..)));
    assert!(matches!(&m.values[3].value.body.value, Expr_::Accessor(..)));
}

#[test]
fn lists_tuples() {
    let m = parse_ok("module M exposing (..)\n\na = [ 1, 2, 3 ]\nb = ( 1, \"two\" )\nc = []\n");
    assert!(matches!(&m.values[0].value.body.value, Expr_::List(l) if l.len() == 3));
    assert!(matches!(&m.values[1].value.body.value, Expr_::Tuple(..)));
    assert!(matches!(&m.values[2].value.body.value, Expr_::List(l) if l.is_empty()));
}

#[test]
fn custom_types() {
    let m = parse_ok(
        "module M exposing (..)\n\ntype Maybe a\n    = Just a\n    | Nothing\n\ntype alias Point =\n    { x : Float, y : Float }\n",
    );
    assert_eq!(m.unions.len(), 1);
    let union = &m.unions[0].value;
    assert_eq!(union.name.value.as_str(), "Maybe");
    assert_eq!(union.vars.len(), 1);
    assert_eq!(union.ctors.len(), 2);
    assert_eq!(m.aliases.len(), 1);
    match &m.aliases[0].value.tipe.value {
        Type_::Record(fields, None) => assert_eq!(fields.len(), 2),
        other => panic!("expected record type, got {:?}", other),
    }
}

#[test]
fn type_annotations() {
    let m = parse_ok(
        "module M exposing (..)\n\nf : (a -> b) -> List a -> List b\nf g xs = xs\n\ng : { r | name : String } -> String\ng r = r.name\n",
    );
    match &m.values[0].value.type_annotation.as_ref().unwrap().value {
        Type_::Lambda(arg, _) => {
            assert!(matches!(&arg.value, Type_::Lambda(..)));
        }
        other => panic!("expected lambda type, got {:?}", other),
    }
    match &m.values[1].value.type_annotation.as_ref().unwrap().value {
        Type_::Lambda(arg, _) => {
            assert!(matches!(&arg.value, Type_::Record(_, Some(_))));
        }
        other => panic!("expected lambda type, got {:?}", other),
    }
}

#[test]
fn comments() {
    let m = parse_ok(
        "module M exposing (..)\n\n-- a line comment\n{- a block\n   {- nested -}\n   comment -}\nx = 1 -- trailing\n",
    );
    assert_eq!(m.values.len(), 1);
}

#[test]
fn operator_sections() {
    let m = parse_ok("module M exposing (..)\n\nf = foldr (+) 0\nneg = (-)\n");
    match &m.values[0].value.body.value {
        Expr_::Call(_, args) => assert!(matches!(&args[0].value, Expr_::Op(..))),
        other => panic!("expected call, got {:?}", other),
    }
    assert!(matches!(&m.values[1].value.body.value, Expr_::Op(..)));
}

#[test]
fn qualified_vars() {
    let m = parse_ok("module M exposing (..)\n\nx = List.map f xs\ny = Maybe.Just 1\n");
    match &m.values[0].value.body.value {
        Expr_::Call(func, _) => match &func.value {
            Expr_::VarQual(_, qual, name) => {
                assert_eq!(qual.as_str(), "List");
                assert_eq!(name.as_str(), "map");
            }
            other => panic!("expected qualified var, got {:?}", other),
        },
        other => panic!("expected call, got {:?}", other),
    }
}

#[test]
fn tabs_are_errors() {
    parse_err("module M exposing (..)\n\nx =\n\t1\n");
}

#[test]
fn top_level_must_be_fresh_line() {
    parse_err("module M exposing (..)\n\nx = 1\n  y = 2\n");
}

#[test]
fn pipeline_style() {
    let m = parse_ok(
        "module M exposing (..)\n\nresult =\n    [ 1, 2, 3 ]\n        |> List.map double\n        |> List.filter isEven\n",
    );
    match &m.values[0].value.body.value {
        Expr_::Binops(pairs, _) => assert_eq!(pairs.len(), 2),
        other => panic!("expected binops, got {:?}", other),
    }
}

#[test]
fn binop_then_lambda() {
    let m = parse_ok("module M exposing (..)\n\nf = g <| \\x -> x\n");
    match &m.values[0].value.body.value {
        Expr_::Binops(_, last) => assert!(matches!(&last.value, Expr_::Lambda(..))),
        other => panic!("expected binops, got {:?}", other),
    }
}

#[test]
fn surrogate_pair_escapes() {
    let m = parse_ok("module M exposing (..)\n\nx = \"\\u{D835}\\u{DD04}\"\ny = \"\\u{1F4A9}\"\n");
    assert_eq!(m.values[0].value.body.value, Expr_::Str("𝔄".into()));
    assert_eq!(m.values[1].value.body.value, Expr_::Str("💩".into()));
}

#[test]
fn negation_in_parens() {
    let m = parse_ok("module M exposing (..)\n\nx = (-5) + 1\ny = (-x)\nz = (-)\n");
    assert!(matches!(&m.values[0].value.body.value, Expr_::Binops(..)));
    assert!(matches!(&m.values[2].value.body.value, Expr_::Op(..)));
    parse_err("module M exposing (..)\n\nz = (- )\n");
}

#[test]
fn headerless_module_defaults_to_main() {
    let m = parse_ok("x = 1\n");
    assert_eq!(m.get_name().as_str(), "Main");
}

#[test]
fn as_patterns_and_cons_in_case() {
    let m = parse_ok(
        "module M exposing (..)\n\nf v =\n    case v of\n        ( x :: rest ) as whole ->\n            1\n\n        _ ->\n            0\n",
    );
    assert_eq!(m.values.len(), 1);
}

#[test]
fn name_debug_formatting() {
    let m = parse_ok("module M exposing (..)\n\nx = 1\n");
    let name = &m.values[0].value.name.value;
    assert_eq!(format!("{:?}", name), "\"x\"");
    assert_eq!(format!("{}", name), "x");
}

#[test]
fn char_escapes_parse() {
    let m = parse_ok("module M exposing (..)\n\na = '\\t'\nb = '\\''\nc = '\\u{1F4A9}'\n");
    assert_eq!(m.values[0].value.body.value, Expr_::Chr('\t'));
    assert_eq!(m.values[1].value.body.value, Expr_::Chr('\''));
    assert_eq!(m.values[2].value.body.value, Expr_::Chr('💩'));
}

#[test]
fn multiline_string_escapes() {
    let m = parse_ok(r##"module M exposing (..)

x = """a\""""
y = """tab\there"""
"##);
    assert_eq!(m.values[0].value.body.value, Expr_::Str("a\"".into()));
    assert_eq!(m.values[1].value.body.value, Expr_::Str("tab\there".into()));
}

#[test]
fn identifiers_starting_with_keywords() {
    // `type_`, `typeToString`, `port_` etc. are valid identifiers, not the
    // `type`/`port` keywords. (Regression: registry packages use `type_`.)
    parse_ok(
        "module Test exposing (..)\n\
         \n\
         type_ : Int\n\
         type_ = 1\n\
         \n\
         typeToString : Int -> Int\n\
         typeToString n = n\n\
         \n\
         port_ : Int\n\
         port_ = 2\n",
    );
}

#[test]
fn glsl_shader_literal() {
    // `[glsl| ... |]` parses to a Shader expression, harvesting the declared
    // attribute/uniform/varying names. From emilgoldsmith/elm-speedcubing.
    let m = parse_ok(
        "module Test exposing (..)\n\
         \n\
         vertexShader =\n\
         \x20   [glsl|\n\
         \x20       attribute vec3 position;\n\
         \x20       attribute vec3 color;\n\
         \x20       uniform mat4 rotation;\n\
         \x20       varying vec3 vcolor;\n\
         \x20       void main () {\n\
         \x20           gl_Position = rotation * vec4(position, 1.0);\n\
         \x20           vcolor = color;\n\
         \x20       }\n\
         \x20   |]\n",
    );
    let body = &m.values[0].value.body;
    match &body.value {
        Expr_::Shader(shader) => {
            let names = |ns: &[alm_compiler::data::Name]| {
                ns.iter().map(|n| n.as_str().to_string()).collect::<Vec<_>>()
            };
            assert_eq!(names(&shader.attributes), vec!["position", "color"]);
            assert_eq!(names(&shader.uniforms), vec!["rotation"]);
            assert_eq!(names(&shader.varyings), vec!["vcolor"]);
            assert!(shader.src.contains("gl_Position"));
        }
        other => panic!("expected a Shader expression, got {:?}", other),
    }
}
