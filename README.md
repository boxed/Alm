# alm

A port of the [Elm compiler](https://github.com/elm/compiler) from Haskell to Rust.

alm runs Elm 0.19 applications through the same front-end as the original
compiler, then generates code for one of three targets:

- **JavaScript** (the default) — Elm kernel style, byte-identical to
  `elm make` for pure code.
- **Native** — a standalone binary via LLVM, with its own garbage
  collector.
- **WebAssembly** — a from-scratch WasmGC backend.

It compiles real production applications: all 19 entry points of a
~40k-line production codebase (ports, Http, Json decoders, Svg, custom
operators, elm/parser, two dozen package dependencies) compile, boot,
and render.

## Usage

```sh
alm make src/Main.elm --output=main.js
```

`--target=js|native|wasm-gc` selects the backend (default `js`), and
`--source-maps` writes a `.map` beside the JavaScript or WasmGC output.

Projects are discovered through `elm.json` (`source-directories`), and
package dependencies compile directly from the `~/.elm` cache — pure Elm
packages need no porting. In the browser or node:

```js
var app = Elm.Main.init({ node: mountPoint, flags: {...} });
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
- **Byte-exact parse errors** (`Reporting.Error.Syntax`): unfinished
  if/case/let/lambda/record/list/tuple/parens, missing arrows and
  expressions, endless strings and comments, weird numbers and bad
  escapes, endless shaders, pattern and type-annotation problems, module /
  import / exposing / port / type-alias / custom-type declaration errors,
  stray-token classification, indentation problems, module-name mismatches,
  `effect module` misuse, and application/package port validation all render
  identically to the official compiler — pinned by a differential test suite
  of 89 fixtures that diffs alm against `elm make` 0.19.1 output
  byte-for-byte, in plain text, in color, and as `--report=json`.
  Malformed GLSL in a `[glsl|…|]` block is also caught
  (`SHADER PROBLEM`), via a vendored Rust GLSL parser; its embedded message
  differs from elm's, which delegates to a different 3rd-party parser, so
  that one report is not byte-exact.
- **Byte-exact type errors** (`Reporting.Error.Type`, `Type.Error`): a
  mismatch is reported as elm reports it — the type found and the type
  wanted, laid out by a port of elm's pretty-printer, with the parts that
  differ marked and the hint chosen from *how* they differ (Int vs Float,
  a String where an Int was wanted, a record field typo, a rigid type
  variable pinned to something concrete). The wording follows the context:
  which argument of which call, which branch of an `if` or `case`, which
  element of a list, which side of which operator, which field of a record
  update, which pattern in a `case`. All of a module's errors are reported,
  not just the first, and several failing modules are separated the way elm
  separates them. Pinned by a differential suite of 42 fixtures; 41 match
  `elm make` byte-for-byte on both stdout and stderr. The exception is the
  occurs check: alm has none, so a self-referential type is reported as an
  ordinary mismatch rather than elm's `INFINITE TYPE`.
- **Color and `--report=json`**: in a terminal the reports are colored
  exactly as elm colors them — a dull-cyan header bar, vivid-red carets,
  dull-yellow for the thing at fault, vivid green for what to use instead,
  vivid cyan for Elm keywords quoted in prose, an underlined `Hint`/`Note`
  label — and piped output stays plain, matching elm's "is stderr a
  terminal" rule (`NO_COLOR`/`CLICOLOR_FORCE` are honored too, which elm
  does not do). `--report=json` emits the machine-readable form editor
  plugins consume, with each message an array of styled runs. Both are held
  to the same fixtures as the plain text: every syntax-error fixture matches
  byte-for-byte in all three renderings.
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
- **Decision-tree pattern matching** (`Optimize/DecisionTree`): a `case`
  compiles to a tree that tests each sub-path of the scrutinee once and
  shares common prefixes — a jump table (`switch` on JS, `br_table` on
  wasm-gc; LLVM switch-conversion on native) at each dense constructor
  node, nested for deeper patterns. A branch reached from several leaves
  is emitted once as a shared join point — a labeled-block `break` on JS,
  a `br` to a join block on wasm-gc — rather than duplicated, with tail
  calls still compiling to constant-stack loops through it.
- Standard library: Basics, List, String, Char, Maybe, Result, Tuple,
  Dict, Set, Array, Bitwise, Debug, Json.Decode/Encode, Task, Process,
  Time, Http, File, Url, Random, UUID, Html(+Attributes/Events/Keyed/
  Lazy), Svg(+Attributes), Browser(+Dom/Events/Navigation), Platform.
  Every value of every module `elm/core`, `elm/json`, `elm/html`,
  `elm/virtual-dom`, `elm/browser`, `elm/url`, `elm/time`, `elm/random`,
  `elm/file`, `elm/bytes`, `elm/parser`, `elm/regex`, `elm/http` and
  `elm/svg` publish compiles under alm at the type it is published with —
  as does `elm-explorations/test`, `markdown`, `benchmark`,
  `linear-algebra`, `webgl` and `elm/project-metadata-utils`.
  `elm-explorations/markdown` renders through the same `marked` build elm
  vendors, so its HTML is identical.
- **WebGL** (`elm-explorations/webgl`): GLSL `[glsl|…|]` shaders (parsed,
  type-checked, `SHADER PROBLEM` errors), meshes, the `linear-algebra`
  `Math.Vector*`/`Matrix4` kernel, and the full rendering kernel —
  `WebGL.toHtml` mounts a canvas that compiles/links shaders, uploads
  attribute/index buffers, sets uniforms, applies blend/depth/stencil/…
  settings, and draws. `WebGL.Texture.load` fetches an image and uploads
  it to the GPU (`gl.texImage2D`, mipmaps, wrap/filter, `SizeError`/
  `LoadError`), sampled through `sampler2D` uniforms. Verified pixel-exact
  in a headless browser (`tests/browser/webgl`, `tests/browser/webgl-texture`).

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

## Effect managers

Unlike stock elm — which restricts `effect module` to the `@elm`
organization — alm compiles and runs **user-defined effect modules**.
`command`/`subscription` build effect leaves, each manager becomes a
stateful process with a mailbox, and `Platform.sendToApp`/`sendToSelf`
plus `Process.spawn`/`kill` are wired up, mirroring elm's `_Platform`
protocol. This runs on **all three backends** — **JavaScript** (on alm's
CPS task model), **native** (on the reified-task interpreter), and
**WasmGC** (on a monomorphized raw-wasm port of the protocol). A
differential test runs the same command / self-message / subscription
programs on every backend and checks their output agrees.

`Time`, `Random`, and `Http` are themselves **real effect modules** — not
special-cased runtime effects. They compile from bundled `effect module`
sources, so `Time.every` and `Http.track` are genuine subscriptions,
`Random.generate`/`Http.request`/`Http.cancel` genuine commands, all routed
through the same manager protocol on every backend. Their pure helpers
(calendar math, PCG generators, request/body/expect builders) stay as
backend intrinsics behind `Elm.Kernel.*`, and dropping a `Time.every`
subscription or cancelling a tracked request now `Process.kill`s the
underlying timer/request — alm's scheduler gained real task cancellation.

`elm/bytes` works on all three backends (encode/decode of ints, floats,
strings, and bytes, `Decode.loop`/`map`/`andThen`, and decode failures),
verified byte-for-byte across js, native, and wasm-gc by a differential
test.

Outgoing **ports** carry any port-legal payload on all three backends. On
wasm-gc — where the host boundary is a JSON string rather than a live JS
value — a type-directed encoder converts the payload (scalars, `List`,
`Array`, tuples, records, `Maybe`, `Json.Value`) to JSON before it crosses,
matching the JS backend value-for-value (checked by a differential test).

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
                                   native_runtime.rs (LLVM native),
                                   wasmgc.rs (WasmGC), sourcemap.rs
  interface.rs   Elm/Interface.hs  module interfaces
  project.rs     builder/          elm.json, module discovery, packages
  builtins.rs                      core library signatures (parsed by alm)
crates/alm/                        the `alm make` CLI
```

A reference checkout of the Haskell sources is expected at
`../alm-reference` for module-by-module comparison.
