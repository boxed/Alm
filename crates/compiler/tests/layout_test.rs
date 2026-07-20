//! Phase 3 of monomorphization: mapping concrete types to physical layouts.

use alm_compiler::ast::canonical as can;
use alm_compiler::ir::layout::{Layout, LayoutCtx};
use alm_compiler::{canonicalize, parse};
use std::rc::Rc;

fn ctx(src: &str) -> LayoutCtx {
    let module = parse::parse_module(src).expect("parse");
    let canonical = canonicalize::canonicalize(&module).expect("canonicalize");
    LayoutCtx::new(&canonical)
}

fn int() -> can::Type {
    can::Type::Type("Basics".into(), "Int".into(), Rc::new(vec![]))
}
fn float() -> can::Type {
    can::Type::Type("Basics".into(), "Float".into(), Rc::new(vec![]))
}
fn string() -> can::Type {
    can::Type::Type("String".into(), "String".into(), Rc::new(vec![]))
}
fn custom(name: &str, args: Vec<can::Type>) -> can::Type {
    can::Type::Type("Test".into(), name.into(), Rc::new(args))
}
fn maybe(inner: can::Type) -> can::Type {
    can::Type::Type("Maybe".into(), "Maybe".into(), Rc::new(vec![inner]))
}
fn list(inner: can::Type) -> can::Type {
    can::Type::Type("List".into(), "List".into(), Rc::new(vec![inner]))
}

const EMPTY: &str = "module Test exposing (..)\n\nx = 1\n";

#[test]
fn scalars() {
    let c = ctx(EMPTY);
    assert_eq!(c.layout_of(&int()), Layout::Int);
    assert_eq!(c.layout_of(&float()), Layout::Float);
    assert_eq!(c.layout_of(&string()), Layout::Str);
    assert_eq!(
        c.layout_of(&can::Type::Type("Basics".into(), "Bool".into(), Rc::new(vec![]))),
        Layout::Bool
    );
    assert_eq!(c.layout_of(&can::Type::Unit), Layout::Unit);
    assert_eq!(
        c.layout_of(&can::Type::Lambda(Rc::new(int()), Rc::new(int()))),
        Layout::Closure
    );
}

#[test]
fn lists_carry_element_layout() {
    let c = ctx(EMPTY);
    assert_eq!(c.layout_of(&list(int())), Layout::List(Box::new(Layout::Int)));
    assert_eq!(
        c.layout_of(&list(list(float()))),
        Layout::List(Box::new(Layout::List(Box::new(Layout::Float))))
    );
}

#[test]
fn tuples_and_records_are_flat_structs() {
    let c = ctx(EMPTY);
    assert_eq!(
        c.layout_of(&can::Type::Tuple(Rc::new(int()), Rc::new(string()), None)),
        Layout::Tuple(vec![Layout::Int, Layout::Str])
    );

    // Record fields are sorted by name for a canonical struct order.
    let record = can::Type::Record(
        Rc::new(vec![("y".into(), float()), ("x".into(), int())]),
        None,
    );
    assert_eq!(
        c.layout_of(&record),
        Layout::Record(vec![
            ("x".into(), Layout::Int),
            ("y".into(), Layout::Float),
        ])
    );
}

#[test]
fn nullary_unions_are_enums() {
    // Built-in Order has three nullary constructors.
    let c = ctx(EMPTY);
    assert_eq!(
        c.layout_of(&can::Type::Type("Basics".into(), "Order".into(), Rc::new(vec![]))),
        Layout::Enum(3)
    );

    // A user enum.
    let c = ctx("module Test exposing (..)\n\ntype Color = Red | Green | Blue\n\nx = 1\n");
    assert_eq!(c.layout_of(&custom("Color", vec![])), Layout::Enum(3));
}

#[test]
fn data_carrying_union_is_tagged_with_specialized_fields() {
    // `Maybe Int` -> tagged { Just(Int), Nothing() }. Constructor order
    // follows the builtin definition (Just, then Nothing).
    let c = ctx(EMPTY);
    assert_eq!(
        c.layout_of(&maybe(int())),
        Layout::Tagged(vec![vec![Layout::Int], vec![]])
    );
    // The element type really is specialized: Maybe String differs.
    assert_eq!(
        c.layout_of(&maybe(string())),
        Layout::Tagged(vec![vec![Layout::Str], vec![]])
    );
}

#[test]
fn recursive_union_breaks_with_a_boxed_reference() {
    // type Tree = Leaf Int | Node Tree Tree
    let c = ctx(
        "module Test exposing (..)\n\
         \n\
         type Tree = Leaf Int | Node Tree Tree\n\
         \n\
         x = 1\n",
    );
    // Leaf carries an Int; Node's two Tree fields become boxed refs so the
    // layout stays finite.
    assert_eq!(
        c.layout_of(&custom("Tree", vec![])),
        Layout::Tagged(vec![vec![Layout::Int], vec![Layout::Ref, Layout::Ref]])
    );
}

#[test]
fn polymorphic_container_specializes_per_argument() {
    // type Box a = Box a  -> the field layout follows the type argument.
    let c = ctx(
        "module Test exposing (..)\n\
         \n\
         type Box a = Box a\n\
         \n\
         x = 1\n",
    );
    assert_eq!(
        c.layout_of(&custom("Box", vec![int()])),
        Layout::Tagged(vec![vec![Layout::Int]])
    );
    assert_eq!(
        c.layout_of(&custom("Box", vec![list(float())])),
        Layout::Tagged(vec![vec![Layout::List(Box::new(Layout::Float))]])
    );
}
