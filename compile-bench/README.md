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
the runtime and computation benchmarks are laid out. Two kinds of workload:

*Package wrappers.* A small application importing a package's whole public
surface, so the package and everything under it has to be compiled. A bare
library cannot be a workload on its own — it has no `main`, and neither compiler
will emit a program without one.

*Applications.* A real one, checked out at a pinned commit: hundreds of the
project's own modules on top of the dependency graph, which is the shape of the
compile people actually wait for. `exosphere/exosphere` is ~59k lines over 212
modules and 58 packages. The checkout is cached under `.almtmp/checkouts` and
its dependency set is completed from `~/.elm` before building — a project that
patches a package (exosphere sideloads forks of `elm/html` and friends) records
the *forked* graph in `elm.json`, which the official compiler rejects as invalid
when resolved against the published packages. Filling the gap from the cache
builds what the maintainers build, without touching `~/.elm`.

elm gets **two** columns because it has two speeds and they differ by ~7x:

| column | what it is |
|---|---|
| `elm (full)` | `elm-stuff` cleared each run — the like-for-like comparison |
| `elm (incr.)` | cache warm, entry module touched — what you wait for when editing |
| `alm-js` / `alm-wasm` / `alm-native` | full build of the whole graph, every run |

The incremental column touches the entry module before each run on purpose:
without an edit, elm checks mtimes and exits, and the column would be measuring
its no-op path rather than a rebuild.

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

**A compiler that cannot build a workload gets no figure for it**, and the
reason is recorded so the report can say which rather than show a blank. Timing
a failure is not timing a compile. This used to drop the whole row, which cost
the most informative workloads: alm's native backend emits a binary, so it
cannot build a browser application like exosphere, and `elm-chart-builder`
defeats it too — but that is a fact about one back end, not a reason to stop
measuring the four compilers that do build them. A row nothing can build is
still dropped; there is nothing left to compare.

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
