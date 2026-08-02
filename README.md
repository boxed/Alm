# alm

A port of the [Elm compiler](https://github.com/elm/compiler) from Haskell to Rust.

alm runs Elm 0.19 applications through the same front-end as the original
compiler, then generates code for one of three targets:

- **JavaScript** (the default) — Elm kernel style, byte-identical to
  `elm make` for pure code.
- **Native** — a standalone binary via LLVM, with its own garbage
  collector.
- **WebAssembly** — a from-scratch WasmGC backend.

It compiles real production applications:
[exosphere](https://gitlab.com/exosphere/exosphere) — 59k lines over 212
modules and 58 packages, with ports, Http, Json decoders, Svg, custom
operators and elm/parser — compiles, boots and renders.

## Usage

```sh
alm make src/Main.elm --output=main.js
```

`--target=js|native|wasm-gc` selects the backend (default `js`),
`--source-maps` writes a `.map` beside the JavaScript or WasmGC output,
`--report=json` writes diagnostics as JSON for editors, `--docs=<file>`
writes a package's `docs.json`, and `--optimize` enforces elm's production
rule that no `Debug` call may survive. (The two code-size optimizations elm
couples to that flag — shortening record field names and numbering
constructor tags — are not implemented; alm's runtime reads those names
directly, so renaming them would mean rewriting it.)

`--live` serves the program and rebuilds it whenever a source changes,
swapping the new build into the open page and keeping the running model when
the new build still agrees what a `Model` is. Adding `--output` writes the
program out as well, for the case where the page belongs to a larger app —
a Django template, a Vite entry — and only loads the bundle:

```sh
alm make src/Main.elm --live --output=static/app.js
```

That bundle carries the live-reload client, so the embedding page hot-swaps
without knowing anything about alm; loading it is enough, from whatever
origin it is served on. It is a development bundle — it talks back to the
alm server — so build without `--live` to ship. `--no-hot-reload` keeps the
rebuild-and-write and leaves the page alone, for an app with a reloader of
its own.

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
  self-referential type: both compilers detect the cycle, but alm's unifier
  refuses to build it and blames the unification, where elm builds it and
  checks each binding afterwards, reporting `INFINITE TYPE` against the
  binding.
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

Compile speed for the JavaScript target, measured by
`compile-bench/run.py` on Apple Silicon, median of 5 runs. Every
workload is public and pinned, so the figures can be reproduced:
[exosphere](https://gitlab.com/exosphere/exosphere) at
`be3d7114`, 59k lines over 212 modules and 58 packages.

| | elm 0.19.1 | alm |
|---|---|---|
| project-cold (build cache cleared) | 1509 ms | **1086 ms** |
| incremental (one module edited) | 176 ms | **106 ms** |
| no-op (nothing changed at all) | 161 ms | **56 ms** |

Both compilers cache per module, so all three modes are comparable.
The incremental figure tracks the size of what you edit and how much
depends on it; smaller projects go much further, and a package and its
whole dependency graph rebuilds in a fraction of what elm needs:

| workload | elm (incr.) | alm (incr.) | |
|---|---|---|---|
| terezka/elm-charts | 98 ms | **13 ms** | 7.5x |
| ianmackenzie/elm-geometry | 113 ms | **22 ms** | 5.1x |
| data-viz-lab/elm-chart-builder | 112 ms | **21 ms** | 5.3x |
| exosphere (59k lines) | 176 ms | **106 ms** | 1.7x |

elm's no-op costs about what its incremental build does — 161 ms against
176 ms on exosphere — so with the official compiler you pay nearly the
full price for a save that changed nothing.

alm's cache lives in `.alm-stuff` (self-ignoring, safe to delete). A
module is reused when its source *and* every interface it was checked
against are unchanged, and an untouched file is recognized by its
timestamp and length so it is never read or parsed at all — most of
what an incremental build would otherwise spend its time on is
rediscovering an import graph that has not moved. The cache is
invalidated by the compiler binary itself, so it cannot survive a
change to alm. An incremental build is byte-for-byte what a full build
produces — differential tests hold that — and `ALM_NO_CACHE=1` turns it
off.

Type checking is ~76% of a cold build; `ALM_TIMING=1` breaks a compile
down by phase, and reports how many modules the cache reused.

Bundle sizes for exosphere, pre-minification: alm 3591 KB, elm dev
3312 KB, elm `--optimize` 3093 KB — alm is 8% larger than elm's
development build here. (On a smaller codebase alm comes out well under
elm; tree-shaking the hand-written runtime kernel is worth a fixed
amount, which counts for less the more application code there is.)

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
