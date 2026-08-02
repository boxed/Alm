# alm

An [Elm](https://elm-lang.org/) compiler written in Rust with:

- improved compilation speed
- faster runtime execution
- source maps
- live hot module reload
- experimental wasm support
- experimental native compilation

## Usage

```sh
alm make src/Main.elm --output=main.js
```

Flags:

- `--target=js|native|wasm-gc` — backend, default `js`
- `--source-maps` — write a `.map` beside the JS or WasmGC output
- `--report=json` — diagnostics as JSON, for editors
- `--docs=<file>` — write a package's `docs.json`
- `--optimize` — enforce elm's rule that no `Debug` call survives. The two
  code-size optimizations elm couples to that flag (shortening record field
  names, numbering constructor tags) are not implemented: alm's runtime reads
  those names directly.

Projects are discovered through `elm.json` (`source-directories`), and package
dependencies compile straight from the `~/.elm` cache — pure Elm packages need
no porting. In the browser or node:

```js
var app = Elm.Main.init({ node: mountPoint, flags: {...} });
app.ports.somePort.subscribe(function (value) { ... });
```

### Live reload

`--live` serves the program, rebuilds on every source change, and swaps the new
build into the open page — keeping the running model when the new build still
agrees what a `Model` is.

```sh
alm make src/Main.elm --live --output=static/app.js
```

Adding `--output` also writes the bundle out, for when the page belongs to a
larger app (a Django template, a Vite entry) and only loads the bundle. That
bundle carries the live-reload client, so the embedding page hot-swaps without
knowing anything about alm, from whatever origin it is served on. It is a
development bundle — build without `--live` to ship. `--no-hot-reload` keeps the
rebuild-and-write but leaves the page alone, for apps with their own reloader.

## What works

- **The full Elm language**: modules, imports with aliases and exposing lists
  (including opaque types and one alias covering several modules), custom types,
  extensible records, record-alias constructors, custom operators
  (`infix left 5 (|=) = keeper`), value recursion through lambdas, tuples,
  let/case/lambdas with nested patterns, whitespace-sensitive layout, ports, and
  all literal forms including surrogate-pair escapes.
- **Hindley-Milner type inference**, ported from `Type/*.hs`: union-find
  unification, let-polymorphism with SCC-based generalization, rigid annotation
  variables scoped over nested annotations, row-polymorphic records, and elm's
  `number`/`comparable`/`appendable` constraints.
- **Exhaustiveness checking** (`Nitpick.PatternMatches`, Maranget's algorithm):
  missing branches are compile errors listing example patterns; redundant
  branches are rejected.
- **Byte-exact parse errors** (`Reporting.Error.Syntax`): unfinished
  expressions, missing arrows, endless strings and comments, bad numbers and
  escapes, pattern and annotation problems, declaration errors, indentation
  problems, module-name mismatches, `effect module` misuse, port validation.
  Pinned by 89 differential fixtures diffed against `elm make` 0.19.1 in plain
  text, in color, and as `--report=json`. Malformed GLSL is caught too
  (`SHADER PROBLEM`), but its message comes from a different third-party parser
  than elm's, so that one report is not byte-exact.
- **Byte-exact type errors** (`Reporting.Error.Type`): the type found and the
  type wanted, laid out by a port of elm's pretty-printer, with the differing
  parts marked and the hint chosen from *how* they differ. The wording follows
  the context — which argument of which call, which branch, which field of a
  record update. All of a module's errors are reported, not just the first.
  41 of 42 differential fixtures match byte-for-byte on stdout and stderr; the
  exception is the self-referential type, where alm blames the unification and
  elm reports `INFINITE TYPE` against the binding.
- **Color and `--report=json`**: terminal reports are colored exactly as elm
  colors them, and piped output stays plain under the same "is stderr a
  terminal" rule (`NO_COLOR`/`CLICOLOR_FORCE` are honored too, which elm does
  not do). Held to the same fixtures as the plain text.
- **Multi-module and package builds**: dependency-ordered compilation against
  module interfaces; pure packages compile from their published sources;
  `Elm.Kernel.*` imports resolve to runtime shims.
- **The Elm Architecture**: virtual DOM with keyed/lazy nodes and SVG,
  decoder-based events, `Browser.sandbox`/`element`/`document`/`application`
  (link interception, pushUrl, popstate, titles), `Platform.worker`, ports, a
  CPS task scheduler, and Http/Time/Random/Browser.Dom/Events/Navigation
  subscriptions.
- **Three backends**: JavaScript in Elm kernel style (`F2`/`A2` currying, tagged
  objects), native code via LLVM (with its own garbage collector), and WebAssembly
  (a from-scratch WasmGC code generator). Self tail calls compile to loops that
  run in constant stack space.
- **Decision-tree pattern matching** (`Optimize/DecisionTree`): a `case` becomes
  a tree that tests each sub-path of the scrutinee once and shares common
  prefixes, with a jump table at each dense constructor node (`switch` on JS,
  `br_table` on wasm-gc, LLVM switch on native). A branch reached from several
  leaves is emitted once as a shared join point rather than duplicated.
- **Standard library**: every value of every module `elm/core`, `elm/json`,
  `elm/html`, `elm/virtual-dom`, `elm/browser`, `elm/url`, `elm/time`,
  `elm/random`, `elm/file`, `elm/bytes`, `elm/parser`, `elm/regex`, `elm/http`
  and `elm/svg` publish compiles at the type it is published with — as does
  `elm-explorations/test`, `markdown`, `benchmark`, `linear-algebra`, `webgl`
  and `elm/project-metadata-utils`. `elm-explorations/markdown` renders through
  the same `marked` build elm vendors, so its HTML is identical.
- **WebGL** (`elm-explorations/webgl`): GLSL `[glsl|…|]` shaders (parsed and
  type-checked), meshes, the `linear-algebra` `Math.Vector*`/`Matrix4` kernel,
  and the full rendering kernel — `WebGL.toHtml` mounts a canvas that
  compiles/links shaders, uploads buffers, sets uniforms, applies
  blend/depth/stencil settings, and draws. `WebGL.Texture.load` fetches an image
  and uploads it to the GPU. Verified pixel-exact in a headless browser.
- **Cross-backend parity**: a differential suite runs the same programs through
  all three backends and checks their output agrees. `elm/bytes` works on all
  three, byte-for-byte. Outgoing ports carry any port-legal payload on all
  three — on wasm-gc, where the host boundary is a JSON string rather than a
  live JS value, a type-directed encoder converts the payload before it crosses.

## Effect managers

Unlike stock elm — which restricts `effect module` to the `@elm` organization —
alm compiles and runs **user-defined effect modules**. `command`/`subscription`
build effect leaves, each manager becomes a stateful process with a mailbox, and
`Platform.sendToApp`/`sendToSelf` plus `Process.spawn`/`kill` are wired up,
mirroring elm's `_Platform` protocol. This runs on all three backends:
JavaScript (on alm's CPS task model), native (on the reified-task interpreter),
and WasmGC (on a monomorphized raw-wasm port of the protocol).

`Time`, `Random`, and `Http` are themselves real effect modules, not
special-cased runtime effects. They compile from bundled `effect module`
sources, so `Time.every` and `Http.track` are genuine subscriptions and
`Random.generate`/`Http.request`/`Http.cancel` genuine commands, routed through
the same protocol on every backend. Their pure helpers (calendar math, PCG
generators, request builders) stay as backend intrinsics behind `Elm.Kernel.*`.
Dropping a `Time.every` subscription or cancelling a tracked request
`Process.kill`s the underlying timer or request.

## Benchmarks

Compile speed for the JavaScript target, measured by `compile-bench/run.py` on
Apple Silicon, median of 5 runs. Every workload is public and pinned:
[exosphere](https://gitlab.com/exosphere/exosphere) at `be3d7114` is 59k lines
over 212 modules and 58 packages.

| exosphere | elm 0.19.1 | alm-js | alm-wasm |
|---|---|---|---|
| full (build cache cleared) | 1507 ms | **1060 ms** | 2281 ms |
| incremental (one module edited) | 165 ms | **106 ms** | 1157 ms |
| no-op (nothing changed at all) | 168 ms | 55 ms | **15 ms** |

The incremental figure tracks the size of what you edit and how much depends on
it, so smaller projects go further:

| incremental | elm 0.19.1 | alm-js | |
|---|---|---|---|
| terezka/elm-charts | 90 ms | **13 ms** | 6.9x |
| ianmackenzie/elm-geometry | 102 ms | **20 ms** | 5.1x |
| data-viz-lab/elm-chart-builder | 103 ms | **19 ms** | 5.4x |
| exosphere (59k lines) | 170 ms | **103 ms** | 1.7x |

elm's no-op costs about what its incremental build does — 166 ms against 170 ms
on exosphere — so with the official compiler a save that changed nothing pays
nearly the full price.

Bundle sizes for exosphere, pre-minification: alm 3591 KB, elm dev 3312 KB, elm
`--optimize` 3093 KB — 8% larger than elm's development build here. On a smaller
codebase alm comes out well under elm, since tree-shaking the hand-written
runtime kernel is worth a fixed amount that counts for less the more application
code there is.

## Incremental builds

alm's cache lives in `.alm-stuff` (self-ignoring, safe to delete). A module is
reused when its source *and* every interface it was checked against are
unchanged, and an untouched file is recognized by its timestamp and length, so
it is never read or parsed at all. A build whose sources all still match — same
compiler, target and output — is skipped outright rather than redone.

The JavaScript target reuses a module whole: interface, generated code, exports.
wasm-gc and native cannot, because monomorphization is whole-program and reads
every module's AST; they cache the type checker's output instead (~76% of a cold
build) and rebuild the AST from source each time. That is why their incremental
builds improve but stay well above alm-js — the work left is monomorphization
and code generation, and neither is per module. Native gains least of all, since
LLVM dominates its build.

The cache is invalidated by the compiler binary itself, so it cannot survive a
change to alm. An incremental build is byte-for-byte what a full build produces,
held by differential tests for both JavaScript and wasm. `ALM_NO_CACHE=1` turns
it off; `ALM_TIMING=1` breaks a compile down by phase and reports how many
modules the cache reused.

## Validation

`tests/browser/run.sh` compiles two test apps with alm **and** the official
compiler and drives both through the identical harness in headless Chrome:

- `Browser.element`: 37 assertions — keyed diffing preserving DOM node identity
  across reorder/insert/remove, controlled inputs, checkbox change events, form
  submit with preventDefault, stopPropagation, `Html.Events.custom` flags,
  conditional subtrees, style/class/property patching, SVG namespaces,
  `Html.map`, `Html.Lazy`, both port directions, async tasks.
- `Browser.application` (over http, real History API): 12 assertions — link
  interception, `pushUrl`, `history.back()`/popstate routing, document titles,
  URL bar state.

alm and elm 0.19.1 both pass 49/49.

Runtime output on production code (string and number formatting, Json decoding
pipelines, Round, `Debug.toString`) is byte-identical between the two compilers
— see `examples/dryft-compare-test.elm.txt`.

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

A reference checkout of the Haskell sources is expected at `../alm-reference`
for module-by-module comparison.
