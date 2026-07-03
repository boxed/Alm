# alm

A port of the [Elm compiler](https://github.com/elm/compiler) from Haskell to Rust.

alm compiles Elm source to JavaScript through the same pipeline as the
original compiler:

```
parse → canonicalize → type check → generate JavaScript
```

## Usage

```sh
alm make examples/FizzBuzz.elm
node -e "console.log(require('./examples/FizzBuzz.js').FizzBuzz.main)"
```

The generated JavaScript exposes every top-level value of the module as
`Elm.<ModuleName>.<name>` (CommonJS `module.exports` under node, `this.Elm`
in a browser).

## What works

- **The full Elm syntax for single modules**: module headers, imports,
  custom types, type aliases, records (including extensible records and
  updates), tuples, let/in with destructuring, case/of with nested
  patterns, lambdas, operator sections, pipelines, whitespace-sensitive
  layout, comments, string/char/number literals.
- **Real Hindley-Milner type inference**, ported in spirit from
  `Type/{Type,Unify,Solve}.hs`: unification with union-find,
  let-polymorphism with dependency-sorted generalization (Tarjan SCC, like
  the original's `Data.Graph` usage), rigid type variables from
  annotations, row-polymorphic records, and Elm's `number` / `comparable`
  / `appendable` / `compappend` constrained variables.
- **Friendly error messages** in the Elm spirit: source excerpt, caret,
  and both types rendered.
- **JavaScript generation** using the same runtime conventions as Elm's
  kernel (`F2`/`A2` currying helpers, `{ $: 'Ctor', a, b }` custom types,
  cons lists, records as plain objects, structural equality and
  comparison), plus a runtime with the core parts of `Basics`, `List`,
  `String`, `Char`, `Maybe`, `Result`, `Tuple`, and `Debug`.

## What is not ported yet

- Multi-module projects and `elm.json` package resolution (the `builder/`
  half of the original compiler).
- The Elm Architecture: `Platform`, `Cmd`/`Sub`, `Html` — programs are
  plain values/functions for now, so `main` is typically a `String`.
- Exhaustiveness checking for `case` (`Nitpick.PatternMatches`) — missing
  branches throw at runtime instead of failing at compile time.
- Tail-call optimization, the optimizer pass (`Optimize/*`), decision
  trees for pattern matches, ports, effect managers, GLSL shaders, and
  the full 5,900-line syntax-error catalogue of `Reporting.Error.Syntax`.

## Layout

```
crates/compiler/src/
  parse/         Parse/*.hs        hand-written recursive descent, layout-aware
  ast/           AST/Source.hs, AST/Canonical.hs
  canonicalize/  Canonicalize/*.hs name resolution, binop precedence, SCC sort
  typecheck/     Type/*.hs         union-find HM inference
  generate/      Generate/*.hs     JS codegen + runtime kernel
  builtins.rs                      core library signatures (parsed by alm itself)
crates/alm/                        the `alm make` CLI
```

The Haskell sources this was ported from are ~36k lines in
`elm/compiler`'s `compiler/src/`; a reference checkout is expected at
`../alm-reference` if you want to compare module by module.
