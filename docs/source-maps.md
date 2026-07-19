# Source maps for the JS and WebAssembly-GC targets

Status: in progress (started 2026-07-19). Goal: emit Source Map v3 for both the
JS and wasm-gc backends so browser devtools show Elm source when stepping,
setting breakpoints, and reading stack traces.

Decisions (owner-approved):
- **Granularity: expression-level.** Map individual expressions, not just
  statements/functions. This is precise but requires the emitters to know the
  output position of every sub-expression.
- **Wasm format: Source Map custom section.** Emit a JS-style source map plus a
  `sourceMappingURL` wasm custom section pointing to it (Chrome/Firefox devtools
  consume this). Not DWARF.

## Background from the codebase

- Every canonical AST node is `Located<Expr_>` with a `Region { start, end }` of
  1-based `{ row, col }` — source positions are available at every emission
  point; no new front-end tracking is needed.
- `CheckedProject` retains each module's `path` and `source`, so the map's
  `sources` (file paths) and `sourcesContent` (file text) are available. The JS
  generator already tracks the current module; the wasm generator specializes
  from `MonoProgram`, whose `TypedFn`/`TypedExpr` carry `region` + `module`.
- **JS backend** (`generate/mod.rs`) is a string builder: statements append in
  place to `self.out`; expressions return `String`s that callers concatenate.
- **wasm-gc backend** (`generate/wasmgc.rs`) emits via `wasm_encoder`
  (`Function`/`Instruction`/`CodeSection`); per-instruction byte offsets are not
  currently exposed.

## Source Map v3 (what we emit)

A JSON object: `{ version: 3, file, sources: [...], sourcesContent: [...],
names: [], mappings: "<vlq>" }`.

`mappings` is `;`-separated per generated LINE; within a line, `,`-separated
segments; each segment is 1/4/5 VLQ (base64, continuation bit, sign in LSB)
fields: `[genCol, srcIndex, srcLine, srcCol (, nameIndex)]`. `genCol` is delta
within the line (resets each line); `srcIndex`/`srcLine`/`srcCol`/`nameIndex`
are deltas against the previous segment across the whole file. We can skip
`names` initially (no symbol-name mapping).

For **wasm** the convention is: generated line is always 0 and generated
`column` is the **byte offset of the instruction** in the module. The exact
offset base (whole-module vs code-section-relative) is the known gotcha to pin
down in Phase 2 against real Chrome devtools.

## Phases

### Phase 0 — shared infrastructure (`generate/sourcemap.rs`)
- `SourceMap` builder: interns source files (path → index, with content), holds
  a list of `Mapping { gen_line, gen_col, src, src_line, src_col }`, and
  serializes to v3 JSON (segments sorted by generated position, VLQ-encoded).
- `Region → 0-based (line, col)` conversion (source map is 0-based; `Region` is
  1-based).
- Base64-VLQ encoder.
- Unit tests: VLQ round-trips known vectors; a hand-built map serializes to the
  expected string and decodes (via a JS `source-map` consumer in a node test, or
  a pure-Rust decode check).

Phase 1 is being landed in two verifiable increments (the JS backend is a
working, differentially-tested string builder; the bytes must stay identical):
- **1a — plumbing + per-definition mapping (DONE).** Retain each module's
  path+source through `CheckedProject`; `compile_project_source_maps` /
  `compile_with_source_map` build the map; a lazy output cursor records a
  mapping at each top-level definition's value position; the CLI `--source-maps`
  flag writes `<out>.js.map` + a `//# sourceMappingURL` comment (DCE forced off
  so positions match). Default (no-flag) JS output is byte-identical. Round-trip
  test in `tests/sourcemap_test.rs`.
- **1b — sub-expression granularity (DONE).** The body pipeline
  (`def_value`/`function_named`/`stmts`/`expr`/`binop`/`let_decl_stmts`) now
  returns a `Mapped` (text + byte-offset→region), rebased on concatenation and
  flushed through the cursor in one O(text) scan. Every expression records a
  mapping at its generated start (`Mapped::mark`); a definition's start takes
  priority over its body's first sub-expression (`Mapped::lead`). Verified
  byte-identical across a corpus (elm-charts, elm-visualization, chart-builder,
  one-true-path, yaml, iridescence); mappings jump from ~940 (per-def) to ~18k
  (sub-expression) on elm-charts, resolving mid-line expressions to their exact
  source positions. Test extended to assert sub-expression resolution.

### Phase 1 — JS (`generate/mod.rs`)
For expression-level granularity, the return-`String` emitter is reworked into a
**position-tracking emitter**: a buffer with a running `(line, col)` cursor and a
`map(region)` that records `cursor → region`. `expr()` records a mapping at its
start (and key sub-points) before emitting. Concretely, either (a) convert
`expr` to append into the shared buffer while tracking the cursor, or (b) return
a `MappedCode { text, mappings }` whose mappings rebase on concatenation (a
SourceNode-style builder) and flatten to absolute positions when written to
`out`. (b) localizes the change to the concatenation sites; (a) is simpler but
touches every emission point. Decide during implementation.
Output: `<out>.js.map` + a trailing `//# sourceMappingURL=<name>.map` comment.

### Phase 2 — wasm-gc (`generate/wasmgc.rs`)
- Track the byte offset of each emitted instruction relative to the chosen base.
  `wasm_encoder::Function` does not expose offsets, so either wrap instruction
  emission to measure encoded length incrementally, or compute offsets during a
  post-pass over the encoded function bodies. **Spike this first** — it is the
  main technical risk.
- Build the map (generated line 0, column = offset) via the Phase 0 builder;
  attach a `sourceMappingURL` custom section and write the `.map` file.

### Phase 3 — CLI + tests
- A flag to enable source-map emission (and write the `.map` beside the output).
- Round-trip tests: compile a small program, load the map, assert that chosen
  generated positions map back to the expected Elm `(line, col)`.

## Open risks
- **Wasm offset base** (Phase 2) — must match what Chrome expects; verify against
  live devtools.
- **Expression-level position tracking in JS** — the return-`String` emitter
  needs reworking; keep it mechanical and well-tested to avoid changing emitted
  code (the JS output bytes must stay identical; only a trailing comment + a
  side `.map` file are added).
