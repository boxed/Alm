//! `Deps.Diff` — comparing two versions of a package's API.
//!
//! Everything here is about deciding *whether* something changed and how much
//! that costs in semantic versioning terms. Printing the result is `alm diff`'s
//! job; picking the next version number is `alm bump`'s.
//!
//! The subtle part is what counts as a change. Renaming a type variable is not
//! one — `List a -> Int` and `List b -> Int` are the same function — so
//! comparison collects the variable renamings a match would need and then
//! checks they are consistent: each old variable maps to exactly one new one,
//! no two old variables collapse onto the same new one, and a constrained
//! variable is not quietly widened.

use std::collections::{BTreeMap, BTreeSet};

use crate::docs_json::{Alias, Binop, Documentation, Module, Type, Union, Value};

/// How much a change costs, ordered so `max` picks the worse of two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Magnitude {
    Patch,
    Minor,
    Major,
}

impl Magnitude {
    pub fn as_str(self) -> &'static str {
        match self {
            Magnitude::Patch => "PATCH",
            Magnitude::Minor => "MINOR",
            Magnitude::Major => "MAJOR",
        }
    }
}

/// What happened to one kind of entry (unions, say) within one module.
#[derive(Debug, Clone)]
pub struct Changes<T> {
    pub added: BTreeMap<String, T>,
    /// Name to (old, new).
    pub changed: BTreeMap<String, (T, T)>,
    pub removed: BTreeMap<String, T>,
}

impl<T> Default for Changes<T> {
    fn default() -> Self {
        Changes {
            added: BTreeMap::new(),
            changed: BTreeMap::new(),
            removed: BTreeMap::new(),
        }
    }
}

impl<T> Changes<T> {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.changed.is_empty() && self.removed.is_empty()
    }

    /// Removing or altering anything published is breaking; adding is not.
    pub fn magnitude(&self) -> Magnitude {
        if !self.removed.is_empty() || !self.changed.is_empty() {
            Magnitude::Major
        } else if !self.added.is_empty() {
            Magnitude::Minor
        } else {
            Magnitude::Patch
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ModuleChanges {
    pub unions: Changes<Union>,
    pub aliases: Changes<Alias>,
    pub values: Changes<Value>,
    pub binops: Changes<Binop>,
}

impl ModuleChanges {
    pub fn magnitude(&self) -> Magnitude {
        [
            self.unions.magnitude(),
            self.aliases.magnitude(),
            self.values.magnitude(),
            self.binops.magnitude(),
        ]
        .into_iter()
        .max()
        .unwrap()
    }
}

#[derive(Debug, Clone, Default)]
pub struct PackageChanges {
    pub added: Vec<String>,
    /// Only modules that actually changed; a module whose magnitude is PATCH
    /// is dropped, since a comment edit is not worth reporting.
    pub changed: BTreeMap<String, ModuleChanges>,
    pub removed: Vec<String>,
}

impl PackageChanges {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.changed.is_empty() && self.removed.is_empty()
    }

    pub fn magnitude(&self) -> Magnitude {
        let added = if self.added.is_empty() { Magnitude::Patch } else { Magnitude::Minor };
        let removed = if self.removed.is_empty() { Magnitude::Patch } else { Magnitude::Major };
        std::iter::once(added)
            .chain(std::iter::once(removed))
            .chain(self.changed.values().map(ModuleChanges::magnitude))
            .max()
            .unwrap()
    }
}

pub fn diff(old: &Documentation, new: &Documentation) -> PackageChanges {
    let mut changes = PackageChanges::default();
    for name in new.keys() {
        if !old.contains_key(name) {
            changes.added.push(name.clone());
        }
    }
    for (name, old_module) in old {
        let Some(new_module) = new.get(name) else {
            changes.removed.push(name.clone());
            continue;
        };
        let module = diff_module(old_module, new_module);
        if module.magnitude() != Magnitude::Patch {
            changes.changed.insert(name.clone(), module);
        }
    }
    changes
}

pub fn diff_module(old: &Module, new: &Module) -> ModuleChanges {
    ModuleChanges {
        unions: compare(&old.unions, &new.unions, equivalent_union),
        aliases: compare(&old.aliases, &new.aliases, equivalent_alias),
        values: compare(&old.values, &new.values, equivalent_value),
        binops: compare(&old.binops, &new.binops, equivalent_binop),
    }
}

fn compare<T: Clone>(
    old: &BTreeMap<String, T>,
    new: &BTreeMap<String, T>,
    equivalent: fn(&T, &T) -> bool,
) -> Changes<T> {
    let mut changes = Changes::default();
    for (name, new_entry) in new {
        match old.get(name) {
            None => {
                changes.added.insert(name.clone(), new_entry.clone());
            }
            Some(old_entry) if !equivalent(old_entry, new_entry) => {
                changes.changed.insert(name.clone(), (old_entry.clone(), new_entry.clone()));
            }
            Some(_) => {}
        }
    }
    for (name, old_entry) in old {
        if !new.contains_key(name) {
            changes.removed.insert(name.clone(), old_entry.clone());
        }
    }
    changes
}

// ---------------------------------------------------------------- equivalence

/// Constructors must keep their names *and their order*: elm compares them
/// pairwise, so swapping two of them is a change even though the set is equal.
fn equivalent_union(old: &Union, new: &Union) -> bool {
    old.cases.len() == new.cases.len()
        && old.cases.iter().zip(&new.cases).all(|(a, b)| a.0 == b.0)
        && old.cases.iter().zip(&new.cases).all(|((_, old_args), (_, new_args))| {
            old_args.len() == new_args.len()
                && old_args.iter().zip(new_args).all(|(a, b)| {
                    equivalent_with_vars(&old.args, a, &new.args, b)
                })
        })
}

fn equivalent_alias(old: &Alias, new: &Alias) -> bool {
    equivalent_with_vars(&old.args, &old.tipe, &new.args, &new.tipe)
}

fn equivalent_value(old: &Value, new: &Value) -> bool {
    equivalent_with_vars(&[], &old.tipe, &[], &new.tipe)
}

fn equivalent_binop(old: &Binop, new: &Binop) -> bool {
    equivalent_value(
        &Value { comment: String::new(), tipe: old.tipe.clone() },
        &Value { comment: String::new(), tipe: new.tipe.clone() },
    ) && old.associativity == new.associativity
        && old.precedence == new.precedence
}

/// The same type up to renaming, with the declaration's own parameters pinned
/// positionally: `type alias P a b = ( a, b )` and `type alias P b a = ( b, a )`
/// are equivalent, but `= ( b, a )` with the same parameter list is not.
fn equivalent_with_vars(
    old_vars: &[String],
    old: &Type,
    new_vars: &[String],
    new: &Type,
) -> bool {
    let Some(mut renamings) = diff_type(old, new) else {
        return false;
    };
    if old_vars.len() != new_vars.len() {
        return false;
    }
    renamings.extend(old_vars.iter().cloned().zip(new_vars.iter().cloned()));
    consistent_renaming(&renamings)
}

/// The renamings that would make the two types identical, or `None` if no
/// renaming could.
fn diff_type(old: &Type, new: &Type) -> Option<Vec<(String, String)>> {
    match (old, new) {
        (Type::Var(a), Type::Var(b)) => Some(vec![(a.clone(), b.clone())]),
        (Type::Lambda(a, b), Type::Lambda(x, y)) => {
            let mut vars = diff_type(a, x)?;
            vars.extend(diff_type(b, y)?);
            Some(vars)
        }
        (Type::Type(name, args), Type::Type(other, other_args)) => {
            (same_name(name, other) && args.len() == other_args.len()).then_some(())?;
            zip_diff(args, other_args)
        }
        (Type::Record(fields, ext), Type::Record(other_fields, other_ext)) => {
            match (ext, other_ext) {
                (None, Some(_)) | (Some(_), None) => None,
                (None, None) => diff_fields(fields, other_fields),
                (Some(a), Some(b)) => {
                    let mut vars = vec![(a.clone(), b.clone())];
                    vars.extend(diff_fields(fields, other_fields)?);
                    Some(vars)
                }
            }
        }
        (Type::Unit, Type::Unit) => Some(Vec::new()),
        (Type::Tuple(a), Type::Tuple(b)) => {
            (a.len() == b.len()).then_some(())?;
            zip_diff(a, b)
        }
        _ => None,
    }
}

/// `Basics.Int` and `Int` name the same type. One side is unqualified
/// whenever docs read out of the cache are compared against docs generated
/// from source, which is exactly what `alm diff` with no version does; elm
/// allows it for the same reason, plus very old published docs that were
/// written unqualified throughout.
fn same_name(old: &str, new: &str) -> bool {
    let last = |name: &str| name.rsplit('.').next().unwrap_or(name).to_string();
    if !old.contains('.') || !new.contains('.') {
        last(old) == last(new)
    } else {
        old == new
    }
}

fn zip_diff(old: &[Type], new: &[Type]) -> Option<Vec<(String, String)>> {
    let mut vars = Vec::new();
    for (a, b) in old.iter().zip(new) {
        vars.extend(diff_type(a, b)?);
    }
    Some(vars)
}

/// Record fields are unordered, so they are matched by name.
fn diff_fields(
    old: &[(String, Type)],
    new: &[(String, Type)],
) -> Option<Vec<(String, String)>> {
    (old.len() == new.len()).then_some(())?;
    let mut old_sorted: Vec<&(String, Type)> = old.iter().collect();
    let mut new_sorted: Vec<&(String, Type)> = new.iter().collect();
    old_sorted.sort_by(|a, b| a.0.cmp(&b.0));
    new_sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let mut vars = Vec::new();
    for (a, b) in old_sorted.iter().zip(&new_sorted) {
        (a.0 == b.0).then_some(())?;
        vars.extend(diff_type(&a.1, &b.1)?);
    }
    Some(vars)
}

/// A renaming is only a renaming if it is a function *and* an injection: one
/// new name per old name, and no two old names sharing a new one. Without the
/// second half, `a -> b -> c` would look like a rename of `a -> a -> a`.
fn consistent_renaming(pairs: &[(String, String)]) -> bool {
    let mut mapping: BTreeMap<&str, &str> = BTreeMap::new();
    for (old, new) in pairs {
        match mapping.get(old.as_str()) {
            Some(seen) if *seen != new.as_str() => return false,
            Some(_) => {}
            None => {
                mapping.insert(old, new);
            }
        }
    }
    let images: BTreeSet<&str> = mapping.values().copied().collect();
    images.len() == mapping.len()
        && mapping.iter().all(|(old, new)| compatible_vars(old, new))
}

/// Which constraints a type variable carries. Loosening one is fine — an
/// unconstrained variable accepts everything the constrained one did — but
/// tightening it rejects programs that used to compile.
#[derive(PartialEq, Eq, Clone, Copy)]
enum VarCategory {
    CompAppend,
    Comparable,
    Appendable,
    Number,
    Var,
}

fn categorize(name: &str) -> VarCategory {
    if name.starts_with("compappend") {
        VarCategory::CompAppend
    } else if name.starts_with("comparable") {
        VarCategory::Comparable
    } else if name.starts_with("appendable") {
        VarCategory::Appendable
    } else if name.starts_with("number") {
        VarCategory::Number
    } else {
        VarCategory::Var
    }
}

fn compatible_vars(old: &str, new: &str) -> bool {
    use VarCategory::*;
    matches!(
        (categorize(old), categorize(new)),
        (CompAppend, CompAppend)
            | (Comparable, Comparable)
            | (Appendable, Appendable)
            | (Number, Number)
            // `number` is a `comparable`, so widening this way keeps working.
            | (Number, Comparable)
            | (_, Var)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docs_json::parse_type;

    fn value(tipe: &str) -> Value {
        Value { comment: String::new(), tipe: parse_type(tipe).unwrap() }
    }

    fn same(a: &str, b: &str) -> bool {
        equivalent_value(&value(a), &value(b))
    }

    #[test]
    fn renaming_a_type_variable_is_not_a_change() {
        assert!(same("List.List a -> Basics.Int", "List.List b -> Basics.Int"));
        assert!(same("a -> b -> a", "x -> y -> x"));
    }

    /// The renaming has to be one-to-one in both directions.
    #[test]
    fn collapsing_or_splitting_variables_is_a_change() {
        assert!(!same("a -> b", "a -> a"));
        assert!(!same("a -> a", "a -> b"));
        assert!(!same("a -> b -> a", "x -> y -> y"));
    }

    #[test]
    fn constraints_may_loosen_but_not_tighten() {
        assert!(same("number -> number", "a -> a"));
        assert!(same("number -> number", "comparable -> comparable"));
        assert!(!same("a -> a", "number -> number"));
        assert!(!same("comparable -> comparable", "number -> number"));
        assert!(!same("appendable -> appendable", "comparable -> comparable"));
    }

    #[test]
    fn record_fields_are_matched_by_name_not_position() {
        assert!(same("{ x : Basics.Int, y : a }", "{ y : b, x : Basics.Int }"));
        assert!(!same("{ x : Basics.Int }", "{ x : Basics.Int, y : Basics.Int }"));
        assert!(!same("{ x : Basics.Int }", "{ r | x : Basics.Int }"));
    }

    /// Published docs read back short, generated docs stay qualified; the two
    /// still have to compare equal.
    #[test]
    fn a_qualified_name_matches_the_same_name_unqualified() {
        use crate::docs_json::{parse_type_with, Names};
        let short = Value {
            comment: String::new(),
            tipe: parse_type_with("String.String -> Basics.Int", Names::Short).unwrap(),
        };
        let long = Value {
            comment: String::new(),
            tipe: parse_type_with("String.String -> Basics.Int", Names::Qualified).unwrap(),
        };
        assert!(equivalent_value(&short, &long));
        assert!(equivalent_value(&long, &short));
        // Two qualified names must still agree in full.
        assert!(!same_name("Set.Set", "Dict.Set"));
    }

    #[test]
    fn a_different_type_constructor_is_a_change() {
        assert!(!same("Maybe.Maybe a", "Result.Result x a"));
        assert!(!same("List.List a", "List.List a a"));
        assert!(!same("( a, b )", "( a, b, c )"));
    }

    /// elm compares constructors pairwise, so reordering them breaks.
    #[test]
    fn union_constructors_keep_their_order() {
        let union = |cases: Vec<(&str, Vec<&str>)>| Union {
            comment: String::new(),
            args: Vec::new(),
            cases: cases
                .into_iter()
                .map(|(n, args)| {
                    (n.to_string(), args.iter().map(|a| parse_type(a).unwrap()).collect())
                })
                .collect(),
        };
        let ab = union(vec![("A", vec![]), ("B", vec!["Basics.Int"])]);
        let ba = union(vec![("B", vec!["Basics.Int"]), ("A", vec![])]);
        assert!(equivalent_union(&ab, &ab.clone()));
        assert!(!equivalent_union(&ab, &ba));
    }

    #[test]
    fn magnitude_takes_the_worst_of_what_happened() {
        let mut changes = PackageChanges::default();
        assert_eq!(changes.magnitude(), Magnitude::Patch);
        changes.added.push("New".to_string());
        assert_eq!(changes.magnitude(), Magnitude::Minor);
        changes.removed.push("Gone".to_string());
        assert_eq!(changes.magnitude(), Magnitude::Major);
    }

    #[test]
    fn an_added_value_is_minor_and_a_removed_one_is_major() {
        let mut module = ModuleChanges::default();
        module.values.added.insert("new".to_string(), value("Basics.Int"));
        assert_eq!(module.magnitude(), Magnitude::Minor);
        module.values.removed.insert("gone".to_string(), value("Basics.Int"));
        assert_eq!(module.magnitude(), Magnitude::Major);
    }
}
