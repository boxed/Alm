# alm

A port of the [Elm compiler](https://github.com/elm/compiler) from Haskell to Rust.

alm runs Elm 0.19 applications through the same front-end pipeline as the
original compiler, then hands off to one of several code generators:

```
parse → canonicalize → type check → exhaustiveness check → generate
```

The `generate` stage targets JavaScript (the default), native code via
LLVM, or WebAssembly (a from-scratch WasmGC backend).

It compiles real production applications: all 19 entry points of a
~40k-line production codebase (ports, Http, Json decoders, Svg, custom
operators, elm/parser, two dozen package dependencies) compile, boot,
and render. Pure code compiled to JavaScript is byte-identical to the
official compiler's output.

## Usage

```sh
alm make src/Main.elm --output=main.js
```

`--target=js|native|wasm|wasm-gc` selects the backend (default `js`), and
`--source-maps` writes a `.map` beside the JavaScript or WasmGC output.

Projects are discovered through `elm.json` (`source-directories`), and
package dependencies compile directly from the `~/.elm` cache — pure Elm
packages need no porting. In the browser or node:

```js
var app = Elm.Main.main.init({ node: mountPoint, flags: {...} });
app.ports.somePort.subscribe(function (value) { ... });
```

## What works

- **The full Elm language**: modules, imports with aliases and exposing
  lists (including opaque types and one alias covering several modules),
  custom types, extensible records, record-alias constructors, custom
  operators (`infix left 5 (|=) = keeper`), value recursion through
  lambdas (recursive Json decoders), tuples, let/case/lambdas with
  nested patterns, whitespace-sensitive layout, ports, all literal
  forms including surrogate-pair escapes.
- **Hindley-Milner type inference** ported in spirit from `Type/*.hs`:
  union-find unification, let-polymorphism with SCC-based generalization,
  rigid annotation variables scoped over nested annotations,
  row-polymorphic records, and Elm's `number`/`comparable`/`appendable`
  constraints. Friendly error messages with source excerpts.
- **Exhaustiveness checking** (`Nitpick.PatternMatches`, Maranget's
  algorithm): missing case branches are compile errors listing example
  patterns; redundant branches are rejected.
- **Byte-exact parse errors** (`Reporting.Error.Syntax`): syntax errors —
  unfinished if/case/let/record/list, missing arrows, endless strings and
  comments, module/import/port/type-declaration problems — render
  identically to the official compiler, verified by a differential test
  suite that diffs alm against `elm make` 0.19.1 output.
- **Multi-module + package builds**: dependency-ordered compilation
  against module interfaces; pure packages (Json.Decode.Pipeline,
  Round, maybe-extra, elm-sentry, html-extra, ...) compile from their
  published sources; `Elm.Kernel.*` imports resolve to runtime shims
  (elm/parser's kernel is ported).
- **The Elm Architecture**: virtual DOM with keyed/lazy nodes and SVG,
  decoder-based events, `Browser.sandbox`/`element`/`document`/
  `application` (link interception, pushUrl, popstate, titles),
  `Platform.worker`, ports with type-driven JS value conversion, CPS
  task scheduler (Task/Process), Http via fetch, Time, Random,
  Browser.Dom/Events/Navigation subscriptions.
- **Multiple backends**: JavaScript in Elm kernel style (`F2`/`A2`
  currying, tagged objects, cons lists), native code via LLVM (with its
  own garbage collector), and WebAssembly (a from-scratch WasmGC code
  generator). A differential test suite runs the same programs through
  the backends and checks their output agrees. Self tail calls compile
  to loops that run in constant stack space.
- Standard library: Basics, List, String, Char, Maybe, Result, Tuple,
  Dict, Set, Array, Bitwise, Debug, Json.Decode/Encode, Task, Process,
  Time, Http, File, Url, Random, UUID, Html(+Attributes/Events/Keyed/
  Lazy), Svg(+Attributes), Browser(+Dom/Events/Navigation), Platform.

## Benchmark

Compile speed for the JavaScript target. Apple Silicon, production
codebase, median of 5 runs (3 for suites). One 8,357-line entry point
and its 13-module graph:

| | median | best |
|---|---|---|
| elm 0.19.1, project-cold (elm-stuff wiped) | 738 ms | 727 ms |
| elm 0.19.1, incremental (entry file touched) | 293 ms | 208 ms |
| elm 0.19.1, no-op (nothing changed at all) | 108 ms | 105 ms |
| **alm, full rebuild, no cache** | **131 ms** | **129 ms** |

All 19 entry points of the same codebase (~40k lines):

| | median |
|---|---|
| elm 0.19.1, project-cold | 2.81 s |
| elm 0.19.1, all sources touched (warm elm-stuff) | 2.22 s |
| **alm, full rebuild every time, no cache** | **0.84 s** |

A full alm rebuild is 2.2x faster than an incremental official rebuild
and takes barely longer than the official compiler doing *nothing*
(its no-op check alone costs ~108 ms; alm compiles everything in
131 ms). Across the whole suite alm is 2.7-3.4x faster while redoing
all work every run. (The official compiler reuses
per-package artifacts from `~/.elm` even when project-cold; alm
recompiles package sources every run.)

Bundle sizes for the same app: alm 567 KB, elm dev 667 KB, elm
`--optimize` 631 KB (all pre-minification).

Output compared on production code (string/number formatting, Json
decoding pipelines, Round, Debug.toString): byte-identical between the
two compilers (`examples/dryft-compare-test.elm.txt`).

## Real-browser validation

`tests/browser/run.sh` compiles two test apps with alm **and** the
official compiler and drives both through the identical harness in
headless Chrome:

- `Browser.element`: 37 assertions — keyed diffing preserves DOM node
  identity across reorder/insert/remove, controlled inputs, checkbox
  change events, form submit with preventDefault, stopPropagation,
  `Html.Events.custom` flags, conditional subtrees, style/class/property
  patching, SVG namespaces, `Html.map`, `Html.Lazy`, both port
  directions, async tasks.
- `Browser.application` (over http, real History API): 12 assertions —
  link interception, `pushUrl`, `history.back()`/popstate routing,
  document titles, URL bar state.

alm and elm 0.19.1 both pass 49/49.

## Not ported

- Effect managers (`effect module`) — Http/Time/Random are native
  runtime implementations instead; third-party effect modules won't
  compile. WebSockets, elm/bytes, GLSL shaders, and the optimizer pass
  (`Optimize/*`, decision trees).
- The kernel type-checks trusted boundaries loosely: `Elm.Kernel.*`
  values are untyped, like the original.

## Layout

```
crates/compiler/src/
  parse/         Parse/*.hs        recursive descent, layout-aware
  ast/           AST/Source.hs, AST/Canonical.hs
  canonicalize/  Canonicalize/*.hs names, binop precedence, aliases, SCC
  typecheck/     Type/*.hs         union-find HM inference
  nitpick.rs     Nitpick/PatternMatches.hs   exhaustiveness
  generate/      Generate/*.hs     code generation + runtime kernels:
                                   runtime.js (JS), native.rs +
                                   native_runtime.rs (LLVM), wasmgc.rs
                                   (WasmGC), typed.rs (monomorphized),
                                   sourcemap.rs
  interface.rs   Elm/Interface.hs  module interfaces
  project.rs     builder/          elm.json, module discovery, packages
  builtins.rs                      core library signatures (parsed by alm)
crates/alm/                        the `alm make` CLI
```

A reference checkout of the Haskell sources is expected at
`../alm-reference` for module-by-module comparison.
