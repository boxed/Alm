# Boxed-fallback compilation for un-monomorphizable polymorphic code

Status: proposed. Owner: TBD. Scope: `crates/compiler/src/ir/mono.rs`, `crates/compiler/src/generate/wasmgc.rs`, `crates/compiler/src/generate/typed.rs`.

## 1. Problem

The monomorphizer (`specialize_project`) rebuilds each reachable polymorphic
function as one *concrete* copy per instantiation type, so downstream codegen
sees only ground types. Three registry packages defeat it, and all three are
the same failure family:

| Package | Symptom | Root cause |
|---|---|---|
| **elm-charts** | `wasmgc: unbound local stack$Fn$…vp0…vp1…vp2…` | A **local** polymorphic function reached through the `spec_let` KNOWN GAP (lambda-style member, `mono.rs:1325`): the use site is rewritten to a mangled specialization that is never emitted as a definition. |
| **elm-ui-explorer** | `poly_rec_error` (`mono.rs:269`) | **Polymorphic recursion**: each recursive call at a strictly deeper type ⇒ an unbounded instance set. Cannot be finitely monomorphized *at all*. |
| **athlete** | exit 137 (OOM) | Specialization **explosion**: a finite but combinatorially huge instance set exhausts memory before the `POLY_REC_DEPTH_LIMIT=200` watchdog (`mono.rs:267`) trips (the watchdog bounds *depth*, not *breadth*). |

Today these produce, respectively, a bad reference, a clean error, and a crash.
None compiles.

## 2. The architectural lever

alm has **three backend entry points but only two representation strategies**
(`project.rs`):

1. `compile_project_native` → `native::build`, fed by `lower::lower_project`.
   A **non-monomorphized**, fully-boxed *uniform-word* IR. Every value is one
   `i64` machine word; polymorphism needs no specialization. **This backend
   already compiles all three failing packages' shape of code** — it never runs
   the monomorphizer.
2. `compile_project_typed` → `typed::build`, fed by `MonoProgram`. Emits
   **unboxed** code via the layout engine (`ir::layout`); an `Int` is a raw
   `i64`, a record is a flat struct. This backend **genuinely needs**
   monomorphization: without a ground type it cannot pick a layout.
3. `compile_project_wasmgc` → `wasmgc::build`, fed by the **same** `MonoProgram`.
   But the wasm-gc value model is **already fully boxed**: every value is an
   `eqref` tagged struct (`T_INT`, `T_FLOAT`, …), and *every function has
   signature `(N × eqref) → eqref`* (`wasmgc.rs:9`). wasm-gc boxes everything at
   runtime regardless of the static type.

The consequence: **wasm-gc inherits every monomorphizer failure mode for zero
correctness benefit.** It is forced through `MonoProgram` only because that is
the IR `wasmgc::build` happens to consume — but at runtime it throws the ground
types away. A polymorphic function compiled *once* over `eqref` is exactly as
correct on wasm-gc as N specialized copies, and uses the identical calling
convention wasm-gc already emits.

So "boxed fallback" is not a new machine. It is: **when the monomorphizer cannot
finitely specialize a function, emit a single type-erased copy of it, and let
the backends that are already boxed (wasm-gc; the uniform native backend, which
never sees `MonoProgram` at all) call that copy.** Monomorphization becomes an
*optimization that may decline*, not a *correctness precondition*.

## 3. Design

### 3.1 The generic (type-erased) function

Add a third variety of `TypedFn` alongside per-instance specializations: a
**generic instance**, keyed by `(module, name)` with no type in the mangle.

- **Mangle:** `mangle_generic(module, name)` → e.g. `Chart$stack$G` (the `$G`
  suffix distinguishes it from `$Fn$…`-typed mangles and from unmangled
  top-level symbols). One per source function, not per type.
- **Body type:** every free type variable is erased to a single universal boxed
  type. Introduce `can::Type` sentinel `Erased` (or reuse an existing top type
  if one exists) and a substitution `erase_tyvars(scheme)` mapping each `Var(_)`
  → `Erased`. The body is specialized under that substitution exactly as
  `specialize_project` already does — the only change is the substitution
  content, so all existing machinery (`match_type`, eta-normalization, the
  `Reducer`) applies unchanged.
- **Params/return:** all `Erased`. On wasm-gc this is `eqref`; on the typed
  native backend this is the uniform `i64` word (see §3.4).

A generic instance is emitted **at most once per `(module, name)`**, which is
what tames both the explosion (athlete) and the infinite set (elm-ui-explorer):
the instance count for a fallen-back function collapses from "unbounded/huge" to
exactly one.

### 3.2 When mono falls back

Fallback is chosen per function, at the point the specializer would otherwise
give up or run away. Three triggers, mapped to the three failures:

1. **Depth watchdog trips** (`enqueue`, `mono.rs:282` and the loop guard at
   `mono.rs:572`). Today this sets `set.error`. Instead: mark
   `(module, name)` as *fallback* and enqueue its **generic** instance rather
   than the deep concrete one. Covers **elm-ui-explorer**.
2. **Breadth budget exceeded.** New: a per-`(module, name)` instance counter.
   When a single source function exceeds `SPEC_BREADTH_LIMIT` distinct
   specializations (start at e.g. 64), stop specializing it, discard its
   already-queued concrete instances for the not-yet-compiled ones, and emit its
   generic instance instead. Covers **athlete** (and is a general
   OOM/compile-time guard). `log`/trace the count so silent truncation is
   visible.
3. **`spec_let` KNOWN GAP** (`mono.rs:1325`). A local group with a lambda-style
   member is currently skipped whole, leaving rewritten uses dangling. Instead
   of skipping: emit the group's members as **generic local decls** (erased
   `TypedLetDecl::Def`/`Recursive`) and route *all* their uses to the un-typed
   local name (no `$Fn$…` mangle). Covers **elm-charts**. This sidesteps the
   author's twice-burned attempt to make the gap *specialize by type* — we do
   the opposite, we make it *not specialize*, which is the one thing known to be
   safe on a boxed backend.

Fallback is **sticky and transitive is not required**: a fallback function's
body still specializes its *callees* normally; only the fallen-back function
itself is erased. Callers of a fallback function route to its generic copy
regardless of the type at the call (see §3.3).

### 3.3 Routing reference sites

References resolve to mangled callee names in two places:

- Top-level: the `Specializer` resolves each project ref to a `Global(mangle(…))`
  and pushes an `Instance` onto `sink` (`mono.rs:668`).
- Local: `rewrite_local_uses` (`mono.rs:1834`) rewrites `Local(n)` to its
  per-type `mangle_local`.

Both gain the same rule: **if the callee `(module, name)` is marked fallback,
resolve to the generic mangle** (`mangle_generic` / bare local name) and do
**not** enqueue a typed instance. Because fallback is decided before the
referring bodies are compiled in the worklist order, we need a *pre-pass* or a
*fixup*:

- **Preferred:** run a cheap reachability/breadth estimate first
  (`analyze_project` already seeds instances) to pre-mark obvious fallbacks
  (poly-recursion by the existing depth analysis; breadth by counting seeded
  instances per name). Bodies then compile with the mark already set.
- **Fallback for late discovery:** when the watchdog/breadth trigger fires
  mid-worklist, mark the function and run a final *rewrite pass* over already
  emitted `TypedFn` bodies replacing any `Global(typed-mangle)` /
  `Local(typed-mangle)` for a now-fallback function with its generic mangle,
  then drop the now-orphaned typed copies. This is the same shape as the
  existing local-use rewrite, lifted to the whole program.

### 3.4 Backend obligations

**wasm-gc (`wasmgc.rs`) — near-free.** Functions are already `(N×eqref)→eqref`
and values already boxed. A generic `TypedFn` compiles through the *existing*
path with `Erased` treated as "already boxed" — i.e. skip the unbox-on-entry /
box-on-return fast paths and keep the value as `eqref`. The two things to audit:
  - **Kernel calls inside a generic body** that assume an unboxed scalar arg
    (e.g. arithmetic peeking `T_INT`). These must go through the boxed kernel
    variants that already exist for the dynamic cases (`emit_binop` already
    dispatches on runtime tag for `+`/`-`/`*` — see the first-class-arithmetic
    fix). Verify every kernel reachable from a generic body has a boxed path;
    where one is missing, it already needs one for first-class use anyway.
  - **`Erased` in layout-driven decisions** (record field order, ctor arg
    types): a generic body must not make a layout choice from `Erased`. On
    wasm-gc records/tuples/ctors are already `T_ARR` of `eqref`, so field
    *identity* (name→index) still comes from the record's own field set, which is
    present in the value, not from the tyvar. Confirm no codegen path keys a
    struct offset off the erased type.

**Typed native (`typed.rs`) — moderate; reuse existing trampolines.** This
backend already carries a complete uniform-word boxing layer for exactly this
situation: `box_closure` trampolines keyed by function type (`typed.rs:74`),
`box_fns`/`unbox_fns` (`:80–85`), boundary box/unbox helpers (`:332–431`), and
`rt_apply`/`rt_closure` over uniform closures (`:431–457`). A generic `TypedFn`
is compiled with the **uniform calling convention**: params and return are the
`i64` word, the body uses the boxed helpers for every operation (the same code
the backend already emits when it hits a value of unknown/boxed layout). Call
sites of a generic function box their args and unbox the result via the existing
per-type trampolines. The work is wiring `Erased` params to the uniform-word
path rather than the layout engine — not new runtime code.

**Uniform native (`native.rs`) — nothing.** It never consumes `MonoProgram`;
`lower_project` already erases everything. It is the existence proof that the
model is sound end-to-end.

### 3.5 Staging by risk

The correctness-free win is wasm-gc; the risk lives in typed native. Ship in
that order:

- **Phase A — wasm-gc only.** Implement §3.1–3.3 and the wasm-gc side of §3.4.
  Typed native keeps returning today's clean `error` for a fallback function
  (no regression: it already errors/OOMs on these). This alone turns
  elm-charts, elm-ui-explorer, and athlete **green on wasm-gc**, closing the
  sweep to 100% on the boxed backend where the win is free.
- **Phase B — typed native.** Extend the generic-instance calling convention to
  `typed.rs`. Higher risk (mixing boxed and unboxed at call boundaries), gated
  by the full native differential suite.

## 4. Risks and gates

- **The `spec_let` fragility warning is explicit** (`mono.rs:1325`): admitting
  the gap *by type* "was tried TWICE and reliably breaks
  elm-monocle/elm-statecharts/intervals." This design does **not** admit it by
  type — it admits it *type-erased*. But those three packages are the canary.
  **Gate:** elm-monocle, elm-statecharts, intervals must stay green (both
  backends) after any `spec_let` change. Add them to a pinned regression list in
  the sweep.
- **Perf regression on hot paths.** Fallback boxes values that were previously
  unboxed. It must trigger *only* when specialization genuinely fails, never as
  a shortcut. The breadth limit (§3.2.2) is the one judgment call — set it high
  enough that no currently-passing package flips to boxed. **Gate:** the sweep
  must report, per run, the set of functions that fell back; a diff in that set
  against a checked-in baseline is a reviewable event, not a silent change. Also
  re-run `compute-bench` — no benchmark should regress (none should even touch
  the fallback path).
- **Erased type leaking into a layout decision.** The one soundness trap
  (§3.4). A generic body that computes a struct offset from `Erased` corrupts
  memory. **Gate:** a targeted test per backend — a generic function that builds
  and reads back a record, a tuple, a custom-type ctor, and a `Dict` — asserting
  round-trip equality against the JS baseline.
- **Determinism.** Fallback selection must be deterministic (sorted iteration,
  no hashmap-order dependence) so the emitted program and the fell-back set are
  reproducible.

## 5. Tests

Durable tests to add (mirroring the existing `wasmgc_test.rs` style):

1. `polymorphic_recursion_falls_back_to_generic` — a minimal poly-recursive
   function (deeper type each call) compiles and runs on wasm-gc; result matches
   JS.
2. `specialization_breadth_limit_falls_back` — a function forced past
   `SPEC_BREADTH_LIMIT` distinct instances compiles as one generic copy;
   result matches JS; assert exactly one generic `TypedFn` emitted for it.
3. `local_lambda_gap_compiles_via_generic` — the elm-charts `stack$…` shape (a
   `let` with a lambda-style polymorphic member used at several types) compiles
   and the reference resolves.
4. `generic_body_roundtrips_aggregates` — the layout-safety test from §4.
5. Registry sweep: elm-charts, elm-ui-explorer, athlete flip OK on wasm-gc; the
   three canaries stay OK.

## 6. Effort estimate

- Phase A (wasm-gc): the generic-instance plumbing in `mono.rs` (§3.1–3.3) is
  the bulk — ~1 focused change to `specialize_project`, `enqueue`, `spec_let`,
  and the two rewrite sites, plus `erase_tyvars`/`mangle_generic`. wasm-gc side
  is mostly auditing existing boxed paths. Medium.
- Phase B (typed native): smaller in code (reuse trampolines) but larger in
  risk/validation. Gate on the full native differential suite before commit.

## 7. Alternatives considered

- **Raise the limits / drain orphans.** Rejected: `POLY_REC_DEPTH_LIMIT` cannot
  fix genuine polymorphic recursion (unbounded by construction), and the
  attempted `spec_let` orphan-drain was a no-op for elm-charts (the missing copy
  is never *created*, so there is nothing to drain).
- **Admit the `spec_let` gap by type.** Rejected by the author's two prior
  attempts (breaks the three canaries). Type *erasure* is the deliberate
  opposite and the safe direction on a boxed backend.
- **Route failing packages to the uniform native backend only.** Rejected: it
  abandons wasm-gc (the actual sweep target) and the typed native backend, and
  gives up perf everywhere for a whole-program switch instead of a
  per-function decision.
