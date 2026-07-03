//! Port of `Type.Type`, `Type.UnionFind`, and `Type.Unify`.
//!
//! Types under inference live in a union-find pool of `Descriptor`s.
//! Unification merges equivalence classes and enforces Elm's special
//! `number` / `comparable` / `appendable` pseudo-typeclasses.

use std::collections::BTreeMap;

use crate::data::Name;

pub type Variable = usize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Super {
    Number,
    Comparable,
    Appendable,
    CompAppend,
}

impl Super {
    pub fn name(self) -> &'static str {
        match self {
            Super::Number => "number",
            Super::Comparable => "comparable",
            Super::Appendable => "appendable",
            Super::CompAppend => "compappend",
        }
    }
}

#[derive(Debug, Clone)]
pub enum Content {
    FlexVar(Option<Name>),
    FlexSuper(Super, Option<Name>),
    RigidVar(Name),
    RigidSuper(Super, Name),
    Structure(FlatType),
    /// Absorbs everything after an error has been reported, preventing
    /// error cascades.
    Error,
}

#[derive(Debug, Clone)]
pub enum FlatType {
    /// home module, type name, arguments.
    App(Name, Name, Vec<Variable>),
    Fun(Variable, Variable),
    EmptyRecord,
    Record(BTreeMap<Name, Variable>, Variable),
    Unit,
    Tuple(Variable, Variable, Option<Variable>),
}

struct Descriptor {
    parent: Option<Variable>,
    content: Content,
}

pub struct Pool {
    descriptors: Vec<Descriptor>,
}

#[derive(Debug, Clone)]
pub struct UnifyError {
    /// Left and right sides rendered for the error message; the caller adds
    /// region and context.
    pub message: String,
}

type Unify = Result<(), UnifyError>;

impl Pool {
    pub fn new() -> Pool {
        Pool {
            descriptors: Vec::new(),
        }
    }

    pub fn fresh(&mut self, content: Content) -> Variable {
        self.descriptors.push(Descriptor {
            parent: None,
            content,
        });
        self.descriptors.len() - 1
    }

    pub fn fresh_var(&mut self) -> Variable {
        self.fresh(Content::FlexVar(None))
    }

    pub fn find(&mut self, var: Variable) -> Variable {
        let mut root = var;
        while let Some(parent) = self.descriptors[root].parent {
            root = parent;
        }
        // Path compression.
        let mut current = var;
        while let Some(parent) = self.descriptors[current].parent {
            self.descriptors[current].parent = Some(root);
            current = parent;
        }
        root
    }

    pub fn content(&mut self, var: Variable) -> Content {
        let root = self.find(var);
        self.descriptors[root].content.clone()
    }

    pub fn set_content(&mut self, var: Variable, content: Content) {
        let root = self.find(var);
        self.descriptors[root].content = content;
    }

    fn merge(&mut self, a: Variable, b: Variable, content: Content) {
        let a_root = self.find(a);
        let b_root = self.find(b);
        if a_root != b_root {
            self.descriptors[b_root].parent = Some(a_root);
        }
        self.descriptors[a_root].content = content;
    }

    // UNIFICATION

    pub fn unify(&mut self, a: Variable, b: Variable) -> Unify {
        let a_root = self.find(a);
        let b_root = self.find(b);
        if a_root == b_root {
            return Ok(());
        }
        let a_content = self.descriptors[a_root].content.clone();
        let b_content = self.descriptors[b_root].content.clone();
        use Content::*;
        match (&a_content, &b_content) {
            (Error, _) | (_, Error) => {
                self.merge(a_root, b_root, Error);
                Ok(())
            }
            (FlexVar(_), _) => {
                self.occurs_guard(a_root, b_root)?;
                self.merge(b_root, a_root, b_content);
                Ok(())
            }
            (_, FlexVar(_)) => {
                self.occurs_guard(b_root, a_root)?;
                self.merge(a_root, b_root, a_content);
                Ok(())
            }
            (FlexSuper(a_super, _), FlexSuper(b_super, name)) => {
                match combine_supers(*a_super, *b_super) {
                    Some(combined) => {
                        self.merge(a_root, b_root, FlexSuper(combined, name.clone()));
                        Ok(())
                    }
                    None => Err(self.mismatch(a_root, b_root)),
                }
            }
            (FlexSuper(super_, _), Structure(_)) => {
                self.unify_super_structure(a_root, *super_, b_root)
            }
            (Structure(_), FlexSuper(super_, _)) => {
                self.unify_super_structure(b_root, *super_, a_root)
            }
            (FlexSuper(flex, _), RigidSuper(rigid, _)) if rigid_satisfies(*rigid, *flex) => {
                self.merge(b_root, a_root, b_content);
                Ok(())
            }
            (RigidSuper(rigid, _), FlexSuper(flex, _)) if rigid_satisfies(*rigid, *flex) => {
                self.merge(a_root, b_root, a_content);
                Ok(())
            }
            (RigidVar(_), _)
            | (_, RigidVar(_))
            | (RigidSuper(..), _)
            | (_, RigidSuper(..)) => Err(self.mismatch(a_root, b_root)),
            (Structure(a_flat), Structure(b_flat)) => {
                self.unify_structures(a_root, a_flat.clone(), b_root, b_flat.clone())
            }
        }
    }

    fn unify_structures(
        &mut self,
        a_root: Variable,
        a_flat: FlatType,
        b_root: Variable,
        b_flat: FlatType,
    ) -> Unify {
        use FlatType::*;
        match (&a_flat, &b_flat) {
            (App(home1, name1, args1), App(home2, name2, args2))
                if home1 == home2 && name1 == name2 && args1.len() == args2.len() =>
            {
                let pairs: Vec<_> = args1.iter().copied().zip(args2.iter().copied()).collect();
                for (x, y) in pairs {
                    self.unify(x, y)?;
                }
                self.merge(a_root, b_root, Content::Structure(a_flat));
                Ok(())
            }
            (Fun(arg1, result1), Fun(arg2, result2)) => {
                let (arg1, result1, arg2, result2) = (*arg1, *result1, *arg2, *result2);
                self.unify(arg1, arg2)?;
                self.unify(result1, result2)?;
                self.merge(a_root, b_root, Content::Structure(a_flat));
                Ok(())
            }
            (EmptyRecord, EmptyRecord) => {
                self.merge(a_root, b_root, Content::Structure(a_flat));
                Ok(())
            }
            (Record(..), Record(..)) => {
                let (fields1, ext1) = self.gather_fields(a_root);
                let (fields2, ext2) = self.gather_fields(b_root);
                self.unify_records(a_root, b_root, fields1, ext1, fields2, ext2)
            }
            (Record(..), EmptyRecord) | (EmptyRecord, Record(..)) => {
                // {} only unifies with a record whose fields can all be
                // pushed into an empty extension — i.e. no fields at all.
                let (rec_root, empty_root) = if matches!(a_flat, Record(..)) {
                    (a_root, b_root)
                } else {
                    (b_root, a_root)
                };
                let (fields, ext) = self.gather_fields(rec_root);
                if fields.is_empty() {
                    self.unify(ext, empty_root)
                } else {
                    Err(self.mismatch(a_root, b_root))
                }
            }
            (Unit, Unit) => {
                self.merge(a_root, b_root, Content::Structure(a_flat));
                Ok(())
            }
            (Tuple(a1, b1, c1), Tuple(a2, b2, c2)) if c1.is_some() == c2.is_some() => {
                let (a1, b1, c1) = (*a1, *b1, *c1);
                let (a2, b2, c2) = (*a2, *b2, *c2);
                self.unify(a1, a2)?;
                self.unify(b1, b2)?;
                if let (Some(c1), Some(c2)) = (c1, c2) {
                    self.unify(c1, c2)?;
                }
                self.merge(a_root, b_root, Content::Structure(a_flat));
                Ok(())
            }
            _ => Err(self.mismatch(a_root, b_root)),
        }
    }

    /// Collect all fields reachable through nested record extensions,
    /// returning them with the final (non-record) extension variable.
    pub fn gather_fields_public(&mut self, var: Variable) -> (BTreeMap<Name, Variable>, Variable) {
        self.gather_fields(var)
    }

    fn gather_fields(&mut self, var: Variable) -> (BTreeMap<Name, Variable>, Variable) {
        let mut fields = BTreeMap::new();
        let mut current = var;
        loop {
            match self.content(current) {
                Content::Structure(FlatType::Record(more, ext)) => {
                    for (name, field_var) in more {
                        fields.entry(name).or_insert(field_var);
                    }
                    current = ext;
                }
                _ => return (fields, current),
            }
        }
    }

    /// Port of `Type.Unify.unifyRecord` — row-polymorphic record unification.
    fn unify_records(
        &mut self,
        a_root: Variable,
        b_root: Variable,
        fields1: BTreeMap<Name, Variable>,
        ext1: Variable,
        fields2: BTreeMap<Name, Variable>,
        ext2: Variable,
    ) -> Unify {
        let mut shared = Vec::new();
        let mut unique1 = BTreeMap::new();
        for (name, var) in &fields1 {
            match fields2.get(name) {
                Some(other) => shared.push((*var, *other)),
                None => {
                    unique1.insert(name.clone(), *var);
                }
            }
        }
        let unique2: BTreeMap<Name, Variable> = fields2
            .iter()
            .filter(|(name, _)| !fields1.contains_key(*name))
            .map(|(n, v)| (n.clone(), *v))
            .collect();

        if unique1.is_empty() && unique2.is_empty() {
            self.unify(ext1, ext2)?;
        } else if unique1.is_empty() {
            // Side 1 is missing fields that side 2 has: its extension must
            // provide them.
            let sub_record = self.fresh(Content::Structure(FlatType::Record(unique2, ext2)));
            self.unify(ext1, sub_record)?;
        } else if unique2.is_empty() {
            let sub_record = self.fresh(Content::Structure(FlatType::Record(unique1, ext1)));
            self.unify(ext2, sub_record)?;
        } else {
            let shared_ext = self.fresh_var();
            let sub1 = self.fresh(Content::Structure(FlatType::Record(unique1, shared_ext)));
            let sub2 = self.fresh(Content::Structure(FlatType::Record(unique2, shared_ext)));
            self.unify(ext1, sub2)?;
            self.unify(ext2, sub1)?;
        }

        for (x, y) in shared {
            self.unify(x, y)?;
        }

        let (all_fields, final_ext) = self.gather_fields(a_root);
        self.merge(
            a_root,
            b_root,
            Content::Structure(FlatType::Record(all_fields, final_ext)),
        );
        Ok(())
    }

    fn unify_super_structure(
        &mut self,
        super_var: Variable,
        super_: Super,
        structure_var: Variable,
    ) -> Unify {
        let content = self.content(structure_var);
        let Content::Structure(flat) = &content else {
            return Err(self.mismatch(super_var, structure_var));
        };
        let ok = match super_ {
            Super::Number => is_number_type(flat),
            Super::Comparable => self.check_comparable(flat)?,
            Super::Appendable => self.check_appendable(flat),
            Super::CompAppend => self.check_compappend(flat)?,
        };
        if ok {
            self.occurs_guard(super_var, structure_var)?;
            self.merge(structure_var, super_var, content);
            Ok(())
        } else {
            Err(self.mismatch(super_var, structure_var))
        }
    }

    fn check_comparable(&mut self, flat: &FlatType) -> Result<bool, UnifyError> {
        match flat {
            FlatType::App(home, name, args) => match (home.as_str(), name.as_str()) {
                ("Basics", "Int") | ("Basics", "Float") => Ok(true),
                ("String", "String") | ("Char", "Char") => Ok(true),
                ("List", "List") => {
                    let item = args[0];
                    let comparable_item =
                        self.fresh(Content::FlexSuper(Super::Comparable, None));
                    self.unify(item, comparable_item)?;
                    Ok(true)
                }
                _ => Ok(false),
            },
            FlatType::Tuple(a, b, c) => {
                for var in [Some(*a), Some(*b), *c].into_iter().flatten() {
                    let comparable_item =
                        self.fresh(Content::FlexSuper(Super::Comparable, None));
                    self.unify(var, comparable_item)?;
                }
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn check_appendable(&mut self, flat: &FlatType) -> bool {
        matches!(
            flat,
            FlatType::App(home, name, _)
                if (home.as_str(), name.as_str()) == ("String", "String")
                    || (home.as_str(), name.as_str()) == ("List", "List")
        )
    }

    fn check_compappend(&mut self, flat: &FlatType) -> Result<bool, UnifyError> {
        match flat {
            FlatType::App(home, name, args) => match (home.as_str(), name.as_str()) {
                ("String", "String") => Ok(true),
                ("List", "List") => {
                    let comparable_item =
                        self.fresh(Content::FlexSuper(Super::Comparable, None));
                    self.unify(args[0], comparable_item)?;
                    Ok(true)
                }
                _ => Ok(false),
            },
            _ => Ok(false),
        }
    }

    /// Occurs check: `var` must not appear inside `structure`.
    fn occurs_guard(&mut self, var: Variable, structure: Variable) -> Unify {
        if self.occurs(var, structure) {
            Err(UnifyError {
                message: "This value has an infinite type — it contains itself.".to_string(),
            })
        } else {
            Ok(())
        }
    }

    fn occurs(&mut self, needle: Variable, haystack: Variable) -> bool {
        let needle_root = self.find(needle);
        let haystack_root = self.find(haystack);
        if needle_root == haystack_root {
            return true;
        }
        match self.content(haystack_root) {
            Content::Structure(flat) => {
                flat_children(&flat).iter().any(|&v| self.occurs(needle, v))
            }
            _ => false,
        }
    }

    fn mismatch(&mut self, a: Variable, b: Variable) -> UnifyError {
        let left = self.render(a);
        let right = self.render(b);
        UnifyError {
            message: format!("`{}` vs `{}`", left, right),
        }
    }

    // RENDERING — port of `Type.Error` / `Reporting.Render.Type` (simplified)

    pub fn render(&mut self, var: Variable) -> String {
        let mut names = NameGenerator::new();
        self.render_help(var, &mut names, false)
    }

    fn render_help(&mut self, var: Variable, names: &mut NameGenerator, parens: bool) -> String {
        let root = self.find(var);
        match self.content(root) {
            Content::FlexVar(Some(name)) | Content::RigidVar(name) => name.to_string(),
            Content::RigidSuper(_, name) => name.to_string(),
            Content::FlexVar(None) => names.name_for(root),
            Content::FlexSuper(super_, _) => super_.name().to_string(),
            Content::Error => "?".to_string(),
            Content::Structure(flat) => match flat {
                FlatType::App(_, name, args) => {
                    if args.is_empty() {
                        name.to_string()
                    } else {
                        let rendered: Vec<String> = args
                            .iter()
                            .map(|&arg| self.render_help(arg, names, true))
                            .collect();
                        let s = format!("{} {}", name, rendered.join(" "));
                        if parens {
                            format!("({})", s)
                        } else {
                            s
                        }
                    }
                }
                FlatType::Fun(arg, result) => {
                    let s = format!(
                        "{} -> {}",
                        self.render_fun_arg(arg, names),
                        self.render_help(result, names, false)
                    );
                    if parens {
                        format!("({})", s)
                    } else {
                        s
                    }
                }
                FlatType::EmptyRecord => "{}".to_string(),
                FlatType::Record(..) => {
                    let (fields, ext) = self.gather_fields(root);
                    let rendered: Vec<String> = fields
                        .iter()
                        .map(|(name, &field_var)| {
                            format!(
                                "{} : {}",
                                name,
                                self.render_help(field_var, names, false)
                            )
                        })
                        .collect();
                    match self.content(ext) {
                        Content::Structure(FlatType::EmptyRecord) => {
                            format!("{{ {} }}", rendered.join(", "))
                        }
                        _ => {
                            let ext_name = self.render_help(ext, names, false);
                            format!("{{ {} | {} }}", ext_name, rendered.join(", "))
                        }
                    }
                }
                FlatType::Unit => "()".to_string(),
                FlatType::Tuple(a, b, c) => {
                    let mut parts = vec![
                        self.render_help(a, names, false),
                        self.render_help(b, names, false),
                    ];
                    if let Some(c) = c {
                        parts.push(self.render_help(c, names, false));
                    }
                    format!("( {} )", parts.join(", "))
                }
            },
        }
    }

    fn render_fun_arg(&mut self, arg: Variable, names: &mut NameGenerator) -> String {
        let root = self.find(arg);
        let needs_parens = matches!(
            self.content(root),
            Content::Structure(FlatType::Fun(..))
        );
        self.render_help(arg, names, needs_parens)
    }
}

fn is_number_type(flat: &FlatType) -> bool {
    matches!(
        flat,
        FlatType::App(home, name, _)
            if home.as_str() == "Basics" && (name.as_str() == "Int" || name.as_str() == "Float")
    )
}

/// Does a rigid variable with constraint `rigid` satisfy a flexible
/// constraint `flex`? (`number` is comparable; `compappend` is both
/// comparable and appendable.)
fn rigid_satisfies(rigid: Super, flex: Super) -> bool {
    use Super::*;
    rigid == flex
        || matches!(
            (rigid, flex),
            (Number, Comparable) | (CompAppend, Comparable) | (CompAppend, Appendable)
        )
}

fn combine_supers(a: Super, b: Super) -> Option<Super> {
    use Super::*;
    match (a, b) {
        (x, y) if x == y => Some(x),
        (Number, Comparable) | (Comparable, Number) => Some(Number),
        (Appendable, Comparable) | (Comparable, Appendable) => Some(CompAppend),
        (CompAppend, Comparable) | (Comparable, CompAppend) => Some(CompAppend),
        (CompAppend, Appendable) | (Appendable, CompAppend) => Some(CompAppend),
        _ => None,
    }
}

pub fn flat_children(flat: &FlatType) -> Vec<Variable> {
    match flat {
        FlatType::App(_, _, args) => args.clone(),
        FlatType::Fun(arg, result) => vec![*arg, *result],
        FlatType::EmptyRecord | FlatType::Unit => vec![],
        FlatType::Record(fields, ext) => {
            let mut children: Vec<Variable> = fields.values().copied().collect();
            children.push(*ext);
            children
        }
        FlatType::Tuple(a, b, c) => {
            let mut children = vec![*a, *b];
            children.extend(*c);
            children
        }
    }
}

/// Generates the `a`, `b`, `c`... names for anonymous type variables when
/// rendering error messages.
struct NameGenerator {
    assigned: std::collections::HashMap<Variable, String>,
    next: usize,
}

impl NameGenerator {
    fn new() -> NameGenerator {
        NameGenerator {
            assigned: std::collections::HashMap::new(),
            next: 0,
        }
    }

    fn name_for(&mut self, var: Variable) -> String {
        if let Some(name) = self.assigned.get(&var) {
            return name.clone();
        }
        let mut n = self.next;
        self.next += 1;
        let mut name = String::new();
        loop {
            name.insert(0, (b'a' + (n % 26) as u8) as char);
            n /= 26;
            if n == 0 {
                break;
            }
            n -= 1;
        }
        self.assigned.insert(var, name.clone());
        name
    }
}
