#!/usr/bin/env python3
"""Compile-speed benchmark: how fast each compiler builds the same project.

Two things are measured.

**Per compiler.** One column for elm and one for each alm backend, over a
spread of real projects — the same shape as the runtime and computation
benchmarks, so the report reads consistently.

**Cold against warm.** Both compilers cache, and for both the difference is
most of the story, so each gets a cold column and a warm one. The second table
does the same on a real production application, adding the no-op time and every
entry point rather than one.

The two decide staleness differently — elm compares mtimes, alm hashes contents
— so an incremental run here *edits* a module rather than touching it. A touch
would rebuild under elm and be a no-op under alm, and the columns would not be
measuring the same thing.

    python3 compile-bench/run.py

Package workloads are built from `~/.elm`; application workloads are cloned at
a pinned commit. Everything measured is public and reproducible: the figures
used to come from whichever project happened to sit beside the repository, and
a published benchmark nobody else can run is an assertion rather than a
measurement.
"""

import datetime
import json
import os
import pathlib
import platform
import shutil
import statistics
import subprocess
import sys
import tempfile
import time

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import workloads  # noqa: E402

REPO = pathlib.Path(__file__).resolve().parent.parent
ALM = REPO / "target" / "release" / "alm"
RESULTS = REPO / "compile-bench" / "results.json"
# Cached checkouts of the application workloads. `.almtmp` is the project's own
# scratch directory (git-ignored), so the disk they take is traceable here.
CHECKOUTS = REPO / ".almtmp" / "checkouts"

RUNS_MATRIX = 3
RUNS_SINGLE = 5
RUNS_SUITE = 3

# Libraries with enough of a dependency graph to be worth timing, and small
# enough that a run does not take all afternoon. Anything missing from the
# local cache, or unresolvable from it, is skipped and reported.
LIBRARIES = [
    "elm/http",
    "elm/url",
    "elm/parser",
    "terezka/elm-charts",
    "ianmackenzie/elm-geometry",
    "data-viz-lab/elm-chart-builder",
]

# Real applications, which a wrapper around a package is not: hundreds of the
# project's own modules on top of the dependency graph, which is where a
# compiler's time actually goes. Pinned to a commit, so re-running measures the
# same code, and cloned once into `CHECKOUTS`; packages still resolve from
# `~/.elm`.
APPLICATIONS = [
    {
        "name": "exosphere/exosphere",
        "url": "https://gitlab.com/exosphere/exosphere.git",
        "sha": "be3d71149a683b28ec32c97589e268bfc0a6ea22",
        "entry": "src/Exosphere.elm",
    },
]


def wipe_elm_cache(directory, entry):
    """Clear `elm-stuff` so the official compiler rebuilds the dependency set."""
    shutil.rmtree(directory / "elm-stuff", ignore_errors=True)


def wipe_alm_cache(directory, entry):
    """Clear `.alm-stuff`, so alm rebuilds the whole graph like elm's cold path."""
    shutil.rmtree(directory / ".alm-stuff", ignore_errors=True)


def edit_entry(directory, entry):
    """Make a real one-module edit, which is what an incremental build is *for*.

    Appending a comment rather than `touch`ing, because the two compilers decide
    what is stale differently: elm compares mtimes, alm hashes contents. A touch
    would give elm a rebuild and alm a no-op, and the column would be comparing
    two different things. A trailing comment changes both.
    """
    with (directory / entry).open("a") as f:
        f.write("\n-- compile-bench: one-line edit\n")


# Each compiler that has two speeds gets two columns, because the difference is
# most of the story. A cold build recompiles the whole module graph; a warm one
# recompiles the module that changed and whatever depended on it. Showing only
# one of them would flatter somebody.
#
# `alm-wasm` and `alm-native` get one column each, not because they were left
# out but because they have one speed: monomorphization is whole-program, so
# there is no per-module unit to reuse and those builds do not read the cache at
# all. An "incremental" column for them would repeat the number beside it and
# read as though caching did not help, when the truth is that it is not wired
# up. `CACHELESS` records that so the report can say it.
CACHELESS = {
    "alm-wasm": "monomorphization is whole-program; no per-module cache",
    "alm-native": "monomorphization is whole-program; no per-module cache",
}
# A no-op column needs no setup at all: the untimed warm-up run in `matrix`
# leaves a complete cache, and then nothing changes. It is what an editor or a
# watch loop pays on every save that touched nothing relevant.
COMPILERS = [
    ("elm (full)", lambda entry, out: ["elm", "make", entry, f"--output={out}.js"], wipe_elm_cache),
    ("elm (incr.)", lambda entry, out: ["elm", "make", entry, f"--output={out}.js"], edit_entry),
    ("elm (no-op)", lambda entry, out: ["elm", "make", entry, f"--output={out}.js"], None),
    ("alm-js", lambda entry, out: [str(ALM), "make", entry, f"--output={out}.js"], wipe_alm_cache),
    ("alm-js (incr.)", lambda entry, out: [str(ALM), "make", entry, f"--output={out}.js"],
     edit_entry),
    ("alm-js (no-op)", lambda entry, out: [str(ALM), "make", entry, f"--output={out}.js"], None),
    ("alm-wasm", lambda entry, out: [str(ALM), "make", entry, "--target=wasm-gc",
                                     f"--output={out}.wasm"], None),
    ("alm-native", lambda entry, out: [str(ALM), "make", entry, "--target=native",
                                       f"--output={out}.bin"], None),
]


def time_it(command, cwd):
    """Wall-clock seconds, or None if the build failed.

    Output goes to a real file, never `/dev/null`: the official compiler skips
    code generation entirely for `/dev/null`, which would flatter it by the
    whole back end.
    """
    start = time.perf_counter()
    done = subprocess.run(command, cwd=cwd, capture_output=True, text=True)
    elapsed = time.perf_counter() - start
    return None if done.returncode != 0 else elapsed


def median_ms(runs):
    ok = [r for r in runs if r is not None]
    return None if not ok else round(statistics.median(ok) * 1000, 1)


def elm_count(directory):
    source = directory / "src"
    if not source.is_dir():
        return 0
    return sum(
        len(p.read_text(encoding="utf8", errors="replace").splitlines())
        for p in source.rglob("*.elm")
    )


# ------------------------------------------------------------ applications

def checkout(app):
    """The pinned commit of an application, cloned on first use.

    A shallow fetch of the one commit — the history is not being measured. An
    existing checkout is reused as-is once it is at the right commit, so a
    re-run costs nothing.
    """
    directory = CHECKOUTS / app["name"].replace("/", "-")
    head = subprocess.run(["git", "-C", str(directory), "rev-parse", "HEAD"],
                          capture_output=True, text=True)
    if head.returncode == 0 and head.stdout.strip() == app["sha"]:
        return directory

    shutil.rmtree(directory, ignore_errors=True)
    directory.mkdir(parents=True)
    print(f"  fetching {app['name']} @ {app['sha'][:8]}")
    steps = [
        ["git", "init", "-q", "."],
        ["git", "remote", "add", "origin", app["url"]],
        ["git", "fetch", "--depth", "1", "-q", "origin", app["sha"]],
        ["git", "checkout", "-q", "FETCH_HEAD"],
    ]
    for step in steps:
        done = subprocess.run(step, cwd=directory, capture_output=True, text=True)
        if done.returncode != 0:
            shutil.rmtree(directory, ignore_errors=True)
            print(f"  {app['name']:32s} skipped — {' '.join(step)}: "
                  f"{done.stderr.strip().splitlines()[-1] if done.stderr.strip() else 'failed'}")
            return None
    return directory


def stage(app, scratch):
    """An application copied out of its checkout, ready to build.

    Only `elm.json` and the source directories: the timed runs mutate
    `elm-stuff` and source mtimes, and a checkout is not the harness's to
    change. The dependency set is completed from `~/.elm` — see
    `workloads.complete`.
    """
    source = checkout(app)
    if source is None:
        return None

    outline, added = workloads.complete(json.loads((source / "elm.json").read_text()))
    if outline is None:
        print(f"  {app['name']:32s} skipped — {added}")
        return None
    if added:
        print(f"  {app['name']}: completed the dependency set with {', '.join(added)}")

    directory = scratch / "apps" / app["name"].replace("/", "-")
    shutil.rmtree(directory, ignore_errors=True)
    directory.mkdir(parents=True)
    (directory / "elm.json").write_text(json.dumps(outline, indent=4) + "\n")
    for rel in outline.get("source-directories", ["src"]):
        if (source / rel).is_dir():
            shutil.copytree(source / rel, directory / rel,
                            ignore=shutil.ignore_patterns("elm-stuff"))
    return app["name"], directory, app["entry"]


# ------------------------------------------------------------- per compiler

def matrix(projects, scratch):
    """Every compiler against every project.

    A compiler that cannot build a workload gets no figure for it, and the
    reason is recorded rather than left to look like a blank. Dropping the whole
    row instead — which is what this did — costs the most informative workloads:
    a real browser application cannot be built by a back end that emits a
    binary, and that is a fact about the back end, not a reason to stop
    measuring the four compilers that do build it. A row nothing can build is
    dropped, since there is then nothing to compare.
    """
    out, unbuilt = [], []
    for name, directory, entry in projects:
        target = scratch / "out"
        broken = [
            label for label, build, _ in COMPILERS
            if time_it(build(entry, str(target)), directory) is None
        ]
        if len(broken) == len(COMPILERS):
            print(f"  {name:32s} skipped — no compiler can build it")
            continue
        for label in broken:
            unbuilt.append({"project": name, "compiler": label})
        if broken:
            print(f"  {name:32s} not built by {', '.join(broken)}")

        row = {"op": name, "lines": elm_count(directory)}
        for label, build, setup in COMPILERS:
            if label in broken:
                row[label] = None
                continue
            # One untimed run first, with the same setup. An incremental column
            # measures a warm cache, and the column before it may have just
            # wiped one; without this the first timed run would be a cold build
            # in an incremental column.
            if setup:
                setup(directory, entry)
            time_it(build(entry, str(target)), directory)

            runs = []
            for _ in range(RUNS_MATRIX):
                if setup:
                    setup(directory, entry)
                runs.append(time_it(build(entry, str(target)), directory))
            row[label] = median_ms(runs)
        cells = "  ".join(
            f"{label} {'—' if row[label] is None else format(row[label], '.0f')}"
            for label, _, _ in COMPILERS
        )
        print(f"  {name:32s} {cells}")
        out.append(row)
    return out, unbuilt


# ------------------------------------------------------------------- driving

def main():
    if not ALM.is_file():
        sys.exit(f"No alm binary at {ALM}.\n    cargo build --release -p alm")

    elm_version = subprocess.run(["elm", "--version"], capture_output=True,
                                 text=True).stdout.strip()
    print(f"alm  {ALM}")
    print(f"elm  {elm_version}\n")

    with tempfile.TemporaryDirectory(dir=REPO / ".almtmp") as scratch:
        scratch = pathlib.Path(scratch)

        projects = []
        for package in LIBRARIES:
            made = workloads.wrap(package, scratch / "workloads")
            if made is None:
                print(f"  {package:32s} skipped — not resolvable from ~/.elm")
                continue
            projects.append((package, made[0], made[1]))
        for app in APPLICATIONS:
            staged = stage(app, scratch)
            if staged is not None:
                projects.append(staged)

        print("== per compiler ==")
        rows, unbuilt = matrix(projects, scratch)



    payload = {
        "measured": datetime.datetime.now().astimezone().isoformat(timespec="seconds"),
        "machine": f"{platform.system()} {platform.machine()}",
        "elm_version": elm_version,
        "runs": {"matrix": RUNS_MATRIX, "single": RUNS_SINGLE, "suite": RUNS_SUITE},
        "compilers": [label for label, _, _ in COMPILERS],
        "projects": rows,
        "unbuilt": unbuilt,
        "cacheless": CACHELESS,
    }
    RESULTS.write_text(json.dumps(payload, indent=2) + "\n")
    print(f"\nwrote {RESULTS.relative_to(REPO)}")
    report(rows)


def report(rows):
    if rows:
        print("\n| project | " + " | ".join(l for l, _, _ in COMPILERS) + " |")
        print("|---" * (len(COMPILERS) + 1) + "|")
        for row in rows:
            cells = " | ".join(
                "—" if row[l] is None else f"{row[l]:.0f} ms" for l, _, _ in COMPILERS
            )
            print(f"| {row['op']} | {cells} |")


if __name__ == "__main__":
    main()
