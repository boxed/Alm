# compile-bench

How fast the compiler is, as opposed to how fast its output is
(`compute-bench`) or how fast the resulting page is (`dom-bench`).

```
cargo build --release -p alm
python3 compile-bench/run.py [path/to/elm/project]
```

Everything it measures is public: package workloads resolve from `~/.elm`, and
application workloads are cloned at a pinned commit. Nothing is read from
whatever project happens to sit beside the repository — the figures used to
come from a private codebase, which meant the published report quoted numbers
nobody else could reproduce or check.

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

Both compilers cache, and for both the cold and warm numbers differ by roughly
an order of magnitude, so each gets a column of each:

| column | what it is |
|---|---|
| `elm (full)` | `elm-stuff` cleared each run — the like-for-like comparison |
| `elm (incr.)` | cache warm, one module edited — what you wait for when editing |
| `elm (no-op)` | cache warm, nothing changed — what a save that touched nothing costs |
| `alm-js` / `alm-js (incr.)` / `alm-js (no-op)` | the same three, with `.alm-stuff` |
| `alm-wasm` / `alm-native` | full build every run; neither back end caches yet |

**An incremental run edits a module rather than touching it.** The two
compilers decide what is stale differently — elm compares mtimes, alm hashes
contents — so a `touch` would rebuild under elm and be a no-op under alm, and
the two columns would not be measuring the same work. Appending a comment line
changes both. (Without *any* edit, elm's incremental column measures its no-op
path, which is what it used to be doing.)

There is one table, not two. A second one used to repeat every mode against a
single application — which, once that application became one of the workloads
above, was the same row measured twice. The only figures it added were the
no-op times, so those are columns now.

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
