# alm-mmtk — native GC binding (MMTk)

An alternative to the Boehm conservative collector for alm's native backend,
built on [MMTk](https://www.mmtk.io/). Motivation: profiling showed
allocation-churn workloads (e.g. `binary-trees`) are allocation-bound on
native, and a pure bump allocator nearly halves them (16 → 9.9 ms, beating
wasm) — MMTk's Immix gives that bump allocation *with* collection.

## Architecture

MMTk needs a modern rustc, but the runtime (`native_runtime.rs`) is pinned to
1.72.1 for LLVM-16 bitcode. So this is a **separate crate**, excluded from the
workspace, built with a modern toolchain into a staticlib with a **C ABI**
(`almmtk_init`, `almmtk_alloc`, …). The runtime calls that ABI; the crates never
share the Rust ABI.

```sh
cargo +1.95 build --release        # builds libalm_mmtk.a exporting the C ABI
```

## Status / roadmap

- [x] **M0 feasibility** — mmtk 0.32 fetches + compiles with 1.95; VMBinding API mapped.
- [x] **M1a binding (NoGC)** — `AlmVM: VMBinding` with the NoGC plan (bump, never
      collects). Compiles; staticlib exports the C ABI. *(this commit)*
- [ ] **M1b build integration** — `build.rs` builds this with 1.95 into `OUT_DIR`,
      localizes all symbols but the `almmtk_*` C entry points (its bundled `std`
      would otherwise clash at link, like the regex glue), embeds + links it.
- [ ] **M1c wiring + measure** — route `alm_alloc`/`tnode`/Value-cell alloc to
      `almmtk_alloc` under `ALM_GC=mmtk`; measure binary-trees/Dict vs Boehm.
      NoGC leaks, so this only validates the allocation ceiling.
- [ ] **M2 Immix + conservative** — swap plan to `StickyImmix`/`Immix`; implement
      the conservative object model (VO-bit via the `is_mmtk_object` feature),
      object scanning, and conservative stack roots (pin, since we lack precise
      layout). This is what makes it collect and thus shippable.

The `mmtk` conservative templates live in the crate source under
`src/util/test_util/mock_vm.rs` and `src/vm/tests/mock_tests/mock_test_conservatism.rs`.
