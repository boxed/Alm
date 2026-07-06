//! Layouts — phase 3.
//!
//! A [`Layout`] is the physical representation chosen for a value of a given
//! *concrete* type. This is where monomorphization pays off: an `Int` becomes
//! a raw 64-bit integer instead of a tagged word, a `Float` a native `f64`, a
//! record a flat struct — no tag checks, no boxing.
//!
//! Scalars and aggregates of scalars unbox fully. Data-carrying custom types
//! remain heap-allocated tagged values for now (the uniform runtime already
//! handles them); recursion is broken with [`Layout::Ref`]. Enumerations —
//! unions whose constructors take no arguments — collapse to a bare integer
//! tag.

use std::collections::HashMap;

use crate::ast::canonical as can;
use crate::builtins;
use crate::data::Name;

#[derive(Debug, Clone, PartialEq)]
pub enum Layout {
    /// Unboxed 64-bit integer.
    Int,
    /// Native `f64`.
    Float,
    /// A single bit (`i1`).
    Bool,
    /// A Unicode scalar (`i32`).
    Char,
    /// The unit value — zero-sized.
    Unit,
    /// An opaque pointer to a heap string.
    Str,
    /// A heap closure (function value with captured environment).
    Closure,
    /// A pointer to a typed array of the element layout.
    List(Box<Layout>),
    /// A flat struct of element layouts, in order.
    Tuple(Vec<Layout>),
    /// A flat struct of named fields, sorted by name for a canonical order.
    Record(Vec<(Name, Layout)>),
    /// An enumeration: every constructor is nullary, so the value is just an
    /// integer tag. Carries the constructor count.
    Enum(u32),
    /// A data-carrying union: a heap-allocated tagged value. Each inner
    /// vector is one constructor's field layouts, in constructor order.
    Tagged(Vec<Vec<Layout>>),
    /// A boxed reference used to break type recursion (a constructor field
    /// whose type is the union being laid out) — a pointer to the same tagged
    /// value, and the fallback for unresolved type variables.
    Ref,
    /// An opaque value carried as the uniform runtime word (i64): a type the
    /// layout engine does not model, such as a platform type (`Cmd`, `Sub`,
    /// `Task`, `Time.Posix`). These pass through the typed backend unchanged
    /// and interoperate directly with the runtime.
    Opaque,
}

impl Layout {
    /// Whether this layout is a scalar that lives in a machine register.
    pub fn is_scalar(&self) -> bool {
        matches!(
            self,
            Layout::Int | Layout::Float | Layout::Bool | Layout::Char | Layout::Unit
        )
    }
}

struct UnionDef {
    vars: Vec<Name>,
    /// Each constructor's name and field types, in constructor order.
    ctors: Vec<(Name, Vec<can::Type>)>,
}

/// Resolves concrete types to layouts. Knows the module's own unions plus the
/// built-in ones (Maybe, Result, Order, …).
pub struct LayoutCtx {
    unions: HashMap<(Name, Name), UnionDef>,
}

impl LayoutCtx {
    pub fn new(module: &can::Module) -> LayoutCtx {
        Self::for_modules(std::slice::from_ref(&module))
    }

    /// Build a layout context knowing the unions of every given module (plus
    /// the built-ins), so cross-module constructors lay out correctly.
    pub fn for_modules(modules: &[&can::Module]) -> LayoutCtx {
        let mut unions = HashMap::new();
        for module in modules {
            for union in &module.unions {
                unions.insert(
                    (module.name.clone(), union.name.clone()),
                    UnionDef {
                        vars: union.vars.clone(),
                        ctors: union
                            .ctors
                            .iter()
                            .map(|c| (c.name.clone(), c.args.clone()))
                            .collect(),
                    },
                );
            }
        }
        for union in builtins::UNIONS {
            unions.insert(
                (Name::from(union.module), Name::from(union.name)),
                UnionDef {
                    vars: union.vars.iter().map(|v| Name::from(*v)).collect(),
                    ctors: union
                        .ctors
                        .iter()
                        .map(|(name, args)| {
                            (
                                Name::from(*name),
                                args.iter().map(|a| builtins::parse_signature(a)).collect(),
                            )
                        })
                        .collect(),
                },
            );
        }
        LayoutCtx { unions }
    }

    pub fn layout_of(&self, tipe: &can::Type) -> Layout {
        self.go(tipe, &mut Vec::new())
    }

    fn go(&self, tipe: &can::Type, visiting: &mut Vec<(Name, Name)>) -> Layout {
        use can::Type::*;
        match tipe {
            Lambda(..) => Layout::Closure,
            Unit => Layout::Unit,
            Tuple(a, b, c) => {
                let mut parts = vec![self.go(a, visiting), self.go(b, visiting)];
                if let Some(c) = c {
                    parts.push(self.go(c, visiting));
                }
                Layout::Tuple(parts)
            }
            Record(fields, _) => {
                let mut fields: Vec<(Name, Layout)> = fields
                    .iter()
                    .map(|(n, t)| (n.clone(), self.go(t, visiting)))
                    .collect();
                fields.sort_by(|a, b| a.0.cmp(&b.0));
                Layout::Record(fields)
            }
            // An unresolved `number` variable defaults to Int, exactly as
            // Elm does at the end of inference. Other leftover variables
            // (from generalized bindings mono did not specialize) fall back to
            // an opaque boxed reference.
            Var(name) if name.as_str().starts_with("number") => Layout::Int,
            Var(_) => Layout::Ref,
            Type(home, name, args) => self.app_layout(home, name, args, visiting),
        }
    }

    fn app_layout(
        &self,
        home: &Name,
        name: &Name,
        args: &[can::Type],
        visiting: &mut Vec<(Name, Name)>,
    ) -> Layout {
        // Primitive and structural built-ins first.
        match (home.as_str(), name.as_str()) {
            ("Basics", "Int") => return Layout::Int,
            ("Basics", "Float") => return Layout::Float,
            ("Basics", "Bool") => return Layout::Bool,
            ("Char", "Char") => return Layout::Char,
            ("String", "String") => return Layout::Str,
            ("List", "List") if args.len() == 1 => {
                return Layout::List(Box::new(self.go(&args[0], visiting)))
            }
            // `Bytes` is declared `type Bytes = Bytes` (a phantom nullary
            // constructor), which would otherwise lay out as a bare enum tag
            // and discard the actual byte buffer. It is carried as the uniform
            // runtime word (a heap byte buffer) instead, like other opaque
            // platform types.
            ("Bytes", "Bytes") => return Layout::Opaque,
            _ => {}
        }

        let key = (home.clone(), name.clone());
        // Recursion: a constructor field whose type is a union already being
        // laid out becomes a boxed reference.
        if visiting.contains(&key) {
            return Layout::Ref;
        }
        let Some(def) = self.unions.get(&key) else {
            // Unknown type (e.g. a platform/opaque type): carried as the
            // uniform runtime word.
            return Layout::Opaque;
        };

        // Enumeration: all constructors nullary.
        if def.ctors.iter().all(|(_, args)| args.is_empty()) {
            return Layout::Enum(def.ctors.len() as u32);
        }

        // Substitute the type arguments into each constructor's field types,
        // then lay those out.
        let subst: HashMap<Name, can::Type> = def
            .vars
            .iter()
            .cloned()
            .zip(args.iter().cloned())
            .collect();
        visiting.push(key);
        let variants = def
            .ctors
            .iter()
            .map(|(_, fields)| {
                fields
                    .iter()
                    .map(|t| self.go(&substitute(&subst, t), visiting))
                    .collect()
            })
            .collect();
        visiting.pop();
        Layout::Tagged(variants)
    }

    /// The constructors of a union type: each constructor's name and its field
    /// types with the type arguments substituted in. `None` if the type is not
    /// a known union.
    pub fn union_ctors(&self, tipe: &can::Type) -> Option<Vec<(Name, Vec<can::Type>)>> {
        let can::Type::Type(home, name, args) = tipe else {
            return None;
        };
        let def = self.unions.get(&(home.clone(), name.clone()))?;
        let subst: HashMap<Name, can::Type> = def
            .vars
            .iter()
            .cloned()
            .zip(args.iter().cloned())
            .collect();
        Some(
            def.ctors
                .iter()
                .map(|(cname, fields)| {
                    (
                        cname.clone(),
                        fields.iter().map(|t| substitute(&subst, t)).collect(),
                    )
                })
                .collect(),
        )
    }
}

fn substitute(subst: &HashMap<Name, can::Type>, tipe: &can::Type) -> can::Type {
    use can::Type::*;
    match tipe {
        Var(name) => subst.get(name).cloned().unwrap_or_else(|| tipe.clone()),
        Lambda(a, b) => Lambda(
            Box::new(substitute(subst, a)),
            Box::new(substitute(subst, b)),
        ),
        Type(home, name, args) => Type(
            home.clone(),
            name.clone(),
            args.iter().map(|a| substitute(subst, a)).collect(),
        ),
        Record(fields, ext) => Record(
            fields
                .iter()
                .map(|(n, t)| (n.clone(), substitute(subst, t)))
                .collect(),
            ext.clone(),
        ),
        Tuple(a, b, c) => Tuple(
            Box::new(substitute(subst, a)),
            Box::new(substitute(subst, b)),
            c.as_ref().map(|c| Box::new(substitute(subst, c))),
        ),
        Unit => Unit,
    }
}
