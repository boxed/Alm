//! Port of `Elm.Interface` — what one module exposes to its importers.

use std::collections::{HashMap, HashSet};

use crate::ast::canonical as can;
use crate::ast::source::Associativity;
use crate::data::Name;

/// A custom operator exported by a module (e.g. elm/parser's `|=`).
#[derive(Debug, Clone)]
pub struct BinopDef {
    pub associativity: Associativity,
    pub precedence: u8,
    pub function: Name,
    /// The underlying function's type, filled in after type checking.
    pub tipe: Option<can::Type>,
}

#[derive(Debug, Clone, Default)]
pub struct Interface {
    /// Exported values and their (generalized) types. Types are filled in
    /// after the module is type checked.
    pub values: HashMap<Name, can::Type>,
    /// Exported value names (known before type checking).
    pub value_names: HashSet<Name>,
    /// Exported union types.
    pub unions: HashMap<Name, can::Union>,
    /// Unions exported with `(..)` — their constructors are visible.
    pub open_unions: HashSet<Name>,
    /// Exported type aliases: name -> (vars, canonical body).
    pub aliases: HashMap<Name, (Vec<Name>, can::Type)>,
    /// Exported custom operators.
    pub binops: HashMap<Name, BinopDef>,
}

pub type Interfaces = HashMap<Name, Interface>;
