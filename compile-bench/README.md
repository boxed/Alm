# compile-bench

How fast the compiler is, as opposed to how fast its output is
(`compute-bench`) or how fast the resulting page is (`dom-bench`).

```
cargo build --release -p alm
python3 compile-bench/run.py [path/to/elm/project]
```

The project defaults to `$ALM_BENCH_PROJECT`, then to `../dryft`. Any Elm
application works; the figures in the top-level README come from a ~40k-line
one with 19 buildable entry points.

It prints a summary and the markdown for the README's Benchmark section.

## What it measures

**Per compiler** — one column each, over a spread of real projects, matching how
the runtime and computation benchmarks are laid out. Each project is a small
application importing a package's whole public surface, so the package and
everything under it has to be compiled; a bare library cannot be a workload
because it has no `main` and neither compiler will emit a program without one.

elm gets **two** columns because it has two speeds and they differ by ~7x:

| column | what it is |
|---|---|
| `elm (full)` | `elm-stuff` cleared each run — the like-for-like comparison |
| `elm (incr.)` | cache warm, only what changed — what you wait for when editing |
| `alm-js` / `alm-wasm` / `alm-native` | full build of the whole graph, every run |

**Against elm's cache** — the same thing on a real production application, with
elm's no-op time as well, and every entry point rather than one.

## Two things it is careful about

**Never `--output=/dev/null`.** The official compiler detects that and skips
code generation entirely, which would flatter it by the whole back end. Both
compilers write a real file.

**The working tree is never touched.** `elm-stuff` and source mtimes are inputs
to what is being measured and both get mutated, so the run happens on a copy —
`elm.json` plus the declared source directories, nothing else. (A real project
often sits in a larger repository; the one behind these numbers has a live
server socket in it that cannot even be copied.)

A workload that any compiler cannot build is dropped from the table, and
reported when it is. Timing a failure is not timing a compile, and it has to be
excluded from every column or they are not doing equal work. (Right now
`data-viz-lab/elm-chart-builder` goes this way: alm's native backend cannot
build it.)

## Finding out where the time goes

`ALM_TIMING=1` breaks any compile down by phase:

```
$ ALM_TIMING=1 alm make src/Main.elm --output=/dev/null
── alm timing ──
  parse              4.4 ms   2.8%
  canonicalize       7.2 ms   4.6%
  typecheck        131.1 ms  83.1%
  generate          10.0 ms   6.3%
  ...
```

For anything finer, `crates/compiler/examples/profile.rs` compiles one entry
point in a loop so a sampling profiler has a process to attach to:

```
cargo build --release -p alm-compiler --example profile
./target/release/examples/profile src/Main.elm 80 &
sample $! 8 -f /tmp/prof.txt
```

## Feeding the report

Every run also writes `compile-bench/results.json`, which
`bench-report/generate.py` turns into the compile section of the published
benchmark page. The file records when it was measured, so a stale section is
flagged on the page instead of passing for current.
