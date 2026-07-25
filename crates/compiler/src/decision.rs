//! Compiling `case` expressions to decision trees (Maranget, "Compiling Pattern
//! Matching to Good Decision Trees"). This is the shared, backend-agnostic core:
//! it turns a list of branch *patterns* into a [`Tree`] that tests each sub-path
//! of the scrutinee at most once and shares common prefixes between branches.
//! Each backend walks the tree with its own value-extraction / test / binding
//! primitives (the branch *bodies* never appear here — leaves carry a branch
//! index and the variable bindings that branch needs).
//!
//! Maranget's algorithm can copy a wildcard row into several sub-matrices, so a
//! branch may end up at more than one leaf. A branch's variables always bind to
//! the same absolute paths (their positions are fixed by the pattern), so such a
//! branch's body — bindings included — is identical at every leaf; a backend can
//! emit it once as a shared *join point* and jump to it from the other leaves
//! ([`leaf_counts`] reports which branches repeat). The tree itself can still
//! grow large, so [`compile`] bounds the node count and returns `None` (fall
//! back to the sequential `if`-chain) only on genuine blow-up.

use crate::ast::canonical as can;
use crate::data::Name;

/// One step from a value to an immediately-contained sub-value.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Step {
    /// Constructor argument `i` (also newtype unwrap).
    Arg(u32),
    /// Tuple element `i` (0, 1, or 2).
    Elem(u32),
    /// Record field.
    Field(Name),
    /// Head of a non-empty list.
    Head,
    /// Tail of a non-empty list.
    Tail,
}

/// A location within the scrutinee: the sequence of steps to reach it.
pub type Path = Vec<Step>;

/// A refutable test performed at a path.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Test {
    Ctor {
        home: Name,
        union: Name,
        name: Name,
        index: u32,
        num_ctors: u32,
        arity: u32,
    },
    Int(i64),
    Chr(char),
    Str(String),
    /// A non-empty list (`::`).
    Cons,
    /// The empty list (`[]`).
    Nil,
}

/// A compiled decision tree.
#[derive(Debug)]
pub enum Tree {
    /// Branch `branch` matches; `binds` maps each bound variable to the path
    /// whose value it names.
    Leaf {
        branch: usize,
        binds: Vec<(Name, Path)>,
    },
    /// Test the value at `path`; take the first matching edge, else `default`.
    Switch {
        path: Path,
        edges: Vec<(Test, Tree)>,
        /// `None` when the edges are exhaustive (all constructors covered).
        default: Option<Box<Tree>>,
    },
    /// No branch matches (unreachable — exhaustiveness guarantees a match).
    Fail,
}

/// An outstanding match obligation and the accumulated variable bindings for one
/// branch as the matrix is refined.
#[derive(Clone)]
struct Row {
    /// `(path, pattern)` obligations still to be discharged. After normalization
    /// every pattern here is refutable.
    obligations: Vec<(Path, can::Pattern)>,
    binds: Vec<(Name, Path)>,
    branch: usize,
}

/// The largest decision tree we build before giving up (and falling back to the
/// sequential `if`-chain). Duplication from wildcard rows can make a tree grow
/// super-linearly; realistic matches stay far below this.
const NODE_BUDGET: usize = 4000;

/// Build a decision tree for a `case`'s branch patterns (in source order).
/// Returns `None` when the match is trivial (one branch / no test) or the tree
/// would exceed [`NODE_BUDGET`] nodes — in both cases the caller falls back to
/// sequential compilation. A returned tree may reference a branch from several
/// leaves; see [`leaf_counts`].
pub fn compile(patterns: &[can::Pattern]) -> Option<Tree> {
    if patterns.len() < 2 {
        return None; // a single branch needs no dispatch
    }
    let rows: Vec<Row> = patterns
        .iter()
        .enumerate()
        .map(|(branch, pat)| Row {
            obligations: vec![(Vec::new(), pat.clone())],
            binds: Vec::new(),
            branch,
        })
        .collect();
    let mut budget = NODE_BUDGET;
    let tree = build(rows, &mut budget)?;
    // A tree with no refutable test (single leaf) adds nothing over the
    // straight-line binding the caller already does.
    if matches!(tree, Tree::Leaf { .. }) {
        return None;
    }
    Some(tree)
}

/// How many times each branch (by index) appears as a leaf. A count above 1
/// marks a branch a backend should emit once as a shared join point.
pub fn leaf_counts(tree: &Tree, n_branches: usize) -> Vec<usize> {
    let mut counts = vec![0usize; n_branches];
    count_leaves(tree, &mut counts);
    counts
}

fn count_leaves(tree: &Tree, counts: &mut [usize]) {
    match tree {
        Tree::Leaf { branch, .. } => counts[*branch] += 1,
        Tree::Switch { edges, default, .. } => {
            for (_, sub) in edges {
                count_leaves(sub, counts);
            }
            if let Some(d) = default {
                count_leaves(d, counts);
            }
        }
        Tree::Fail => {}
    }
}

fn build(mut rows: Vec<Row>, budget: &mut usize) -> Option<Tree> {
    if *budget == 0 {
        return None; // tree too large — fall back to sequential
    }
    *budget -= 1;
    let first = rows.first()?;
    // Normalize the first row: discharge every irrefutable obligation (binders,
    // tuples, records, newtypes), leaving only refutable ones.
    let mut row0 = first.clone();
    normalize(&mut row0);
    if row0.obligations.is_empty() {
        // The first row matches unconditionally.
        return Some(Tree::Leaf {
            branch: row0.branch,
            binds: row0.binds,
        });
    }
    rows[0] = row0;

    // Choose the path of the first refutable obligation in row 0.
    let path = rows[0].obligations[0].0.clone();

    // Normalize every row and gather the tests appearing at `path`, in order of
    // first appearance.
    for r in rows.iter_mut() {
        normalize(r);
    }
    let mut tests: Vec<Test> = Vec::new();
    let mut num_ctors_seen: Option<u32> = None;
    for r in &rows {
        if let Some(pat) = obligation_at(r, &path) {
            let t = test_of(pat)?;
            if let Test::Ctor { num_ctors, .. } = &t {
                num_ctors_seen = Some(*num_ctors);
            }
            if !tests.contains(&t) {
                tests.push(t);
            }
        }
    }
    if tests.is_empty() {
        return None; // no discriminant here — shouldn't happen
    }

    // One edge per test: the specialized sub-matrix, recursively compiled.
    let mut edges: Vec<(Test, Tree)> = Vec::new();
    for t in &tests {
        let sub = specialize(&rows, &path, t);
        edges.push((t.clone(), build(sub, budget)?));
    }

    // Is the set of edges exhaustive? Then no default is needed.
    let exhaustive = match (&tests[0], num_ctors_seen) {
        (Test::Ctor { .. }, Some(n)) => {
            tests.iter().all(|t| matches!(t, Test::Ctor { .. })) && tests.len() as u32 == n
        }
        (Test::Cons, _) | (Test::Nil, _) => {
            tests.iter().any(|t| *t == Test::Cons) && tests.iter().any(|t| *t == Test::Nil)
        }
        _ => false, // Int/Chr/Str: infinite domain, always need a default
    };

    let default = if exhaustive {
        None
    } else {
        let sub = default_matrix(&rows, &path);
        if sub.is_empty() {
            Some(Box::new(Tree::Fail))
        } else {
            Some(Box::new(build(sub, budget)?))
        }
    };

    Some(Tree::Switch {
        path,
        edges,
        default,
    })
}

/// Discharge every irrefutable leading obligation of `row`, recording variable
/// bindings and expanding product patterns, until all obligations are refutable.
fn normalize(row: &mut Row) {
    use can::Pattern_::*;
    let mut i = 0;
    while i < row.obligations.len() {
        let (path, pat) = row.obligations[i].clone();
        match pat.value {
            Anything | Unit => {
                row.obligations.remove(i);
            }
            Var(name) => {
                row.binds.push((name, path));
                row.obligations.remove(i);
            }
            Alias(inner, name) => {
                row.binds.push((name.value, path.clone()));
                row.obligations[i] = (path, (*inner).clone());
            }
            Record(fields) => {
                for field in &fields {
                    let mut p = path.clone();
                    p.push(Step::Field(field.value.clone()));
                    row.binds.push((field.value.clone(), p));
                }
                row.obligations.remove(i);
            }
            Tuple(a, b, rest) => {
                let mut expanded = vec![
                    (step(&path, Step::Elem(0)), (*a).clone()),
                    (step(&path, Step::Elem(1)), (*b).clone()),
                ];
                if let Some(c) = rest.first() {
                    expanded.push((step(&path, Step::Elem(2)), c.clone()));
                }
                row.obligations.splice(i..=i, expanded);
            }
            // A single-constructor union is irrefutable: unwrap its arguments.
            Ctor(_, _, ref ctor, ref args) if ctor.num_ctors <= 1 => {
                let expanded: Vec<(Path, can::Pattern)> = args
                    .iter()
                    .enumerate()
                    .map(|(k, a)| (step(&path, Step::Arg(k as u32)), a.clone()))
                    .collect();
                row.obligations.splice(i..=i, expanded);
            }
            // Refutable: leave it and advance.
            Ctor(..) | Int(_) | Chr(_) | Str(_) | List(_) | Cons(..) => {
                i += 1;
            }
        }
    }
}

fn step(path: &Path, s: Step) -> Path {
    let mut p = path.clone();
    p.push(s);
    p
}

fn obligation_at<'a>(row: &'a Row, path: &Path) -> Option<&'a can::Pattern> {
    row.obligations
        .iter()
        .find(|(p, _)| p == path)
        .map(|(_, pat)| pat)
}

/// The test a refutable (normalized) pattern discriminates on.
fn test_of(pat: &can::Pattern) -> Option<Test> {
    use can::Pattern_::*;
    match &pat.value {
        Ctor(home, union, ctor, _) => Some(Test::Ctor {
            home: home.clone(),
            union: union.clone(),
            name: ctor.name.clone(),
            index: ctor.index,
            num_ctors: ctor.num_ctors,
            arity: ctor.arity,
        }),
        Int(n) => Some(Test::Int(*n)),
        Chr(c) => Some(Test::Chr(*c)),
        Str(s) => Some(Test::Str(s.clone())),
        Cons(..) => Some(Test::Cons),
        List(items) => Some(if items.is_empty() { Test::Nil } else { Test::Cons }),
        _ => None,
    }
}

/// The sub-obligations produced when `pat` matches `test` at `path`.
fn sub_obligations(pat: &can::Pattern, path: &Path) -> Vec<(Path, can::Pattern)> {
    use can::Pattern_::*;
    match &pat.value {
        Ctor(_, _, _, args) => args
            .iter()
            .enumerate()
            .map(|(k, a)| (step(path, Step::Arg(k as u32)), a.clone()))
            .collect(),
        Cons(head, tail) => vec![
            (step(path, Step::Head), (**head).clone()),
            (step(path, Step::Tail), (**tail).clone()),
        ],
        List(items) if !items.is_empty() => {
            // `[a, rest..]` == `a :: [rest..]`.
            let head = items[0].clone();
            let rest = can::Pattern {
                value: List(items[1..].to_vec()),
                region: pat.region,
            };
            vec![
                (step(path, Step::Head), head),
                (step(path, Step::Tail), rest),
            ]
        }
        // Nil / literals bind nothing.
        _ => Vec::new(),
    }
}

/// The sub-matrix of rows that can match `test` at `path`.
fn specialize(rows: &[Row], path: &Path, test: &Test) -> Vec<Row> {
    let mut out = Vec::new();
    for r in rows {
        match obligation_at(r, path) {
            Some(pat) => {
                // Row constrains this path. Keep it only if it matches `test`.
                if test_of(pat).as_ref() == Some(test) {
                    let mut nr = r.clone();
                    // Replace the obligation at `path` with its sub-obligations.
                    nr.obligations.retain(|(p, _)| p != path);
                    nr.obligations.extend(sub_obligations(pat, path));
                    out.push(nr);
                }
            }
            None => {
                // Wildcard at `path` (already bound during normalization): it
                // matches every test, so it flows into this edge unchanged.
                out.push(r.clone());
            }
        }
    }
    out
}

/// The default sub-matrix: rows that are wildcards at `path`.
fn default_matrix(rows: &[Row], path: &Path) -> Vec<Row> {
    rows.iter()
        .filter(|r| obligation_at(r, path).is_none())
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::canonical::{Ctor, Pattern, Pattern_};
    use crate::reporting::{Located, Region};

    fn p(v: Pattern_) -> Pattern {
        Located {
            value: v,
            region: Region::ZERO,
        }
    }
    fn ctor(name: &str, index: u32, num_ctors: u32, args: Vec<Pattern>) -> Pattern {
        p(Pattern_::Ctor(
            Name::from("M"),
            Name::from("U"),
            Ctor {
                name: Name::from(name),
                index,
                arity: args.len() as u32,
                num_ctors,
            },
            args,
        ))
    }
    fn var(n: &str) -> Pattern {
        p(Pattern_::Var(Name::from(n)))
    }
    fn wild() -> Pattern {
        p(Pattern_::Anything)
    }

    fn switch_path(t: &Tree) -> &Path {
        match t {
            Tree::Switch { path, .. } => path,
            _ => panic!("expected a switch, got {t:?}"),
        }
    }

    #[test]
    fn flat_enum_is_an_exhaustive_switch() {
        // A | B | C  ->  switch at root, three edges, no default.
        let tree = compile(&[
            ctor("A", 0, 3, vec![]),
            ctor("B", 1, 3, vec![]),
            ctor("C", 2, 3, vec![]),
        ])
        .unwrap();
        if let Tree::Switch { path, edges, default } = &tree {
            assert!(path.is_empty());
            assert_eq!(edges.len(), 3);
            assert!(default.is_none(), "all ctors covered => no default");
        } else {
            panic!("expected switch");
        }
    }

    #[test]
    fn nested_constructor_tests_inner_once() {
        // Just (A n) -> ; Just B -> ; Nothing ->
        // Outer switch on the Maybe tag; the `Just` edge switches on the inner
        // tag. Crucially the outer `Just` test is shared, not repeated.
        let tree = compile(&[
            ctor("Just", 0, 2, vec![ctor("A", 0, 2, vec![var("n")])]),
            ctor("Just", 0, 2, vec![ctor("B", 1, 2, vec![])]),
            ctor("Nothing", 1, 2, vec![]),
        ])
        .unwrap();
        // Root switch on the outer value.
        assert!(switch_path(&tree).is_empty());
        if let Tree::Switch { edges, .. } = &tree {
            // The Just edge must itself be a switch on Arg(0).
            let (_, just_sub) = edges.iter().find(|(t, _)| matches!(t, Test::Ctor { name, .. } if name.as_str() == "Just")).unwrap();
            assert_eq!(switch_path(just_sub), &vec![Step::Arg(0)]);
        }
    }

    #[test]
    fn no_branch_appears_twice_when_accepted() {
        // Every leaf must reference a distinct branch (no duplication) — checked
        // implicitly by `compile` returning Some.
        let tree = compile(&[
            ctor("Pair", 0, 1, vec![ctor("A", 0, 2, vec![]), var("y")]),
            ctor("Pair", 0, 1, vec![ctor("B", 1, 2, vec![]), var("y")]),
        ])
        .unwrap();
        let mut counts = vec![0; 2];
        count_leaves(&tree, &mut counts);
        assert_eq!(counts, vec![1, 1]);
    }

    #[test]
    fn duplicating_match_repeats_a_branch() {
        // (A, C) -> 1 ; (B, C) -> 2 ; (_, D) -> 3
        // The first column (A|B) is exhaustive, so the wildcard row 3 flows into
        // BOTH the A and B edges and becomes a `D -> 3` leaf in each. The tree is
        // still built (no fall-back); branch 3 appears twice and a backend emits
        // it once as a shared join point.
        let a = || ctor("A", 0, 2, vec![]);
        let b = || ctor("B", 1, 2, vec![]);
        let c = || ctor("C", 0, 2, vec![]);
        let d = || ctor("D", 1, 2, vec![]);
        let tuple = |x: Pattern, y: Pattern| p(Pattern_::Tuple(Box::new(x), Box::new(y), vec![]));
        let tree = compile(&[tuple(a(), c()), tuple(b(), c()), tuple(wild(), d())]).unwrap();
        assert_eq!(leaf_counts(&tree, 3), vec![1, 1, 2], "branch 3 should repeat");
    }

    #[test]
    fn cons_and_nil_are_exhaustive() {
        // x :: xs -> ; [] ->
        let tree = compile(&[
            p(Pattern_::Cons(Box::new(var("x")), Box::new(var("xs")))),
            p(Pattern_::List(vec![])),
        ])
        .unwrap();
        if let Tree::Switch { edges, default, .. } = &tree {
            assert_eq!(edges.len(), 2);
            assert!(default.is_none(), "Cons + Nil is exhaustive");
        } else {
            panic!("expected switch");
        }
    }

    #[test]
    fn int_literals_need_a_default() {
        let tree = compile(&[
            p(Pattern_::Int(0)),
            p(Pattern_::Int(1)),
            var("n"),
        ])
        .unwrap();
        if let Tree::Switch { edges, default, .. } = &tree {
            assert_eq!(edges.len(), 2);
            assert!(default.is_some(), "open Int domain needs a default");
        } else {
            panic!("expected switch");
        }
    }
}
