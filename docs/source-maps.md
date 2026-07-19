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
  flag writes `<out>.js.map` + a `//# sourceMappingURL` comment. Default (no-flag)
  JS output is byte-identical. Round-trip test in `tests/sourcemap_test.rs`.
  (Initially DCE was forced off; later `tree_shake` was made to return an
  old→new line map so the source map is remapped onto the shaken bundle — mapped
  JS is now tree-shaken, the same size as an ordinary build.)
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

### Phase 2 — wasm-gc (`generate/wasmgc.rs`) — DONE
- **Offsets**: `emit_fn`/`emit_expr` record `(code-section entry index,
  `Function::byte_len()` at emit, region, module)`. After the module is built,
  `build_source_map` re-parses it (wasmparser) for each function body's absolute
  range and computes `column = body_range.start + offset_in_body`. Wasm mappings
  are all on generated line 0; the column is the byte offset.
- **Tree-shaking**: the map resolves against the TREE-SHAKEN binary. The
  un-shaken kernel is not always valid wasm (dead helpers carry latent type
  errors that stubbing removes), and shaking keeps live bodies verbatim, so their
  offsets stay valid at new positions. `tree_shake` now also returns the set of
  kept (non-stubbed) entry indices; mappings for stubbed dead functions are
  dropped.
- **Section + file**: a `sourceMappingURL` custom section (appended after all
  sections, so code offsets don't shift) points at the `<out>.wasm.map` written
  beside the binary. Enabled by `--source-maps` on the `wasm-gc` target.
- Verified: mapped wasm validates (node `WebAssembly.validate`), byte offsets
  are in-range, and expressions resolve to exact source line:col (distinct
  offsets → distinct columns on one source line). Durable test in
  `tests/sourcemap_test.rs::wasm_source_map_has_section_and_resolves`.

### Phase 3 — CLI + tests + polish — DONE
- `--source-maps` flag on the `js` and `wasm-gc` targets writes the `.map`
  beside the output.
- Round-trip tests for both backends (`tests/sourcemap_test.rs`): decode the VLQ
  mappings and assert generated positions map back to the expected Elm
  `(line, col)`, including sub-expression resolution.
- Robustness-swept across real packages (elm-charts, elm-visualization,
  one-true-path, yaml): every mapping resolves to an in-range source line; every
  `--source-maps` wasm validates.
- **Cross-module-inlining guard**: mono's beta-reduction can inline an
  expression from another module into a function body; its region is attributed
  to the function's module, so `build_source_map` drops any mapping whose line
  exceeds that module's file length (rather than emit a bad one). The JS backend
  emits per-module canonical ASTs with no inlining, so it needs no guard. The
  *proper* fix (thread the owning module onto each `TypedExpr`) is deferred; the
  guard removes the harmful (out-of-range) cases.

## Open risks / known limitations
- **Cross-module attribution (wasm) — VERIFIED A NON-ISSUE (2026-07-19).** Every
  region in a `TypedFn` body comes from that function's module, so attributing
  expressions to `TypedFn.module` is correct by construction. Confirmed: mono has
  no cross-module inlining — `spec.expr` resolves foreign references to `Global`
  *calls* (never inlined bodies), and the `Reducer` inlines only local
  zero-param let-bindings within one function. The out-of-range guard dropped 0
  mappings across ~20 multi-module packages, and spot-checked mappings resolve to
  the correct files. A per-`TypedExpr` `module` field (threaded through all ~51
  `TypedExpr` construction sites) would therefore be redundant today; it is worth
  adding only if cross-module *inlining* is ever introduced to mono, at which
  point the guard would catch out-of-range cases but a plausible-but-wrong
  in-range file could slip through. The guard stays as cheap defense.
- **Columns are char counts, not UTF-16 units** — differs only on astral-plane
  characters in string literals; a minor position skew on such lines.
- **`file` field** left empty — optional in v3 and unused when the map is
  referenced via `sourceMappingURL`.
