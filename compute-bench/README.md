# Compute benchmarks

Pure-computation micro-benchmarks for the alm backends — no DOM, no I/O — to
measure raw compute where the js-framework-benchmark (DOM-bound) can't. Each
`src/*.elm` is a value program (`main : Int`) exposing `bench : Int -> Int`, so
the *same* source runs on every backend and results are checked to agree.

Backends compared:

- **alm (JS)** — `--target=js`, `bench(size)` called in-process (JIT-warmed).
- **alm (wasm-gc)** — `--target=wasm-gc`, `main_int()` on the instantiated module.
- **alm (native)** — `--target=native-typed`, the unboxed AOT binary (plain
  `--target=native` is the uniform boxed+Boehm backend, ~10× slower — not used).

## Run

```sh
./build.sh        # compile every workload to js / wasm-gc / native
node run.mjs      # correctness-check, then time; writes results.json
```

Timing is the median of 25 timed calls after 10 warmup calls (native: min of 12
process runs). Lower is better.

## Workloads

| file | stresses |
|------|----------|
| `Fib` | naive recursion — call overhead + integer arithmetic, ~no allocation |
| `BinaryTrees` | allocation/GC churn — build & walk many short-lived trees |
| `ListPipeline` | immutable list allocation — range → map → filter → fold |
| `DictOps` | balanced-tree build + lookup + fold |
| `Mandelbrot` | float math, no allocation |
| `Sort` | `List.sort` of a pseudo-random list + checksum |

Note: Elm's `Int` is JS `f64` (exact to 2^53) on the JS backend but `i64` on the
wasm/native backends — workloads keep intermediate values under 2^53 so all three
agree. The correctness check fails loudly if a workload overflows that.
