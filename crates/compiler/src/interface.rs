//! Port of `Elm.Interface` — what one module exposes to its importers.

use std::collections::{HashMap, HashSet};

use crate::ast::canonical as can;
use crate::data::Name;

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
}

pub type Interfaces = HashMap<Name, Interface>;
