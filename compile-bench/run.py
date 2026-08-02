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


# Measurements are grouped by MODE, and the report gives each mode its own
# table, because a figure is only meaningful against another figure of the same
# kind. Mixed together, a no-op time sets the scale a full build is drawn
# against and every full build looks terrible — the numbers were fine, the
# comparison was not.
#
# `alm-wasm` and `alm-native` appear under `full` only, not because they were
# left out but because they have one speed: monomorphization is whole-program,
# so there is no per-module unit to reuse and those builds do not read the cache
# at all. `CACHELESS` records why, so the report can say it rather than leave a
# gap to be read as a missing run.
CACHELESS = {
    "alm-wasm": "monomorphization is whole-program; no per-module cache",
    "alm-native": "monomorphization is whole-program; no per-module cache",
}

FULL = "full"
INCREMENTAL = "incremental"
NOOP = "no-op"

# `no-op` needs no setup at all: the untimed warm-up run in `matrix` leaves a
# complete cache, and then nothing changes. It is what an editor or a watch loop
# pays on a save that touched nothing relevant.
COMPILERS = [
    ("elm", FULL, lambda entry, out: ["elm", "make", entry, f"--output={out}.js"], wipe_elm_cache),
    ("elm", INCREMENTAL, lambda entry, out: ["elm", "make", entry, f"--output={out}.js"],
     edit_entry),
    ("elm", NOOP, lambda entry, out: ["elm", "make", entry, f"--output={out}.js"], None),
    ("alm-js", FULL, lambda entry, out: [str(ALM), "make", entry, f"--output={out}.js"],
     wipe_alm_cache),
    ("alm-js", INCREMENTAL, lambda entry, out: [str(ALM), "make", entry, f"--output={out}.js"],
     edit_entry),
    ("alm-js", NOOP, lambda entry, out: [str(ALM), "make", entry, f"--output={out}.js"], None),
    ("alm-wasm", FULL, lambda entry, out: [str(ALM), "make", entry, "--target=wasm-gc",
                                           f"--output={out}.wasm"], wipe_alm_cache),
    ("alm-native", FULL, lambda entry, out: [str(ALM), "make", entry, "--target=native",
                                             f"--output={out}.bin"], wipe_alm_cache),
]

def column(name, mode):
    """The key a measurement is recorded under."""
    return f"{name}|{mode}"


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
        # Whether a compiler can build this at all is a property of the
        # compiler, not of the mode, so it is probed once per compiler.
        compilers = sorted({c for c, _, _, _ in COMPILERS}, key=lambda c: c)
        probe = {c: b for c, m, b, _ in COMPILERS if m == FULL for _ in [0]}
        broken = [
            c for c in compilers
            if time_it(probe[c](entry, str(target)), directory) is None
        ]
        if len(broken) == len(compilers):
            print(f"  {name:32s} skipped — no compiler can build it")
            continue
        for compiler in broken:
            unbuilt.append({"project": name, "compiler": compiler})
        if broken:
            print(f"  {name:32s} not built by {', '.join(broken)}")

        row = {"op": name, "lines": elm_count(directory)}
        for compiler, mode, build, setup in COMPILERS:
            label = column(compiler, mode)
            if compiler in broken:
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
            f"{c}/{m} {'—' if row[column(c, m)] is None else format(row[column(c, m)], '.0f')}"
            for c, m, _, _ in COMPILERS
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
        # (compiler, mode) pairs, so the report can group by mode instead of
        # parsing labels back apart.
        "compilers": [{"name": c, "mode": m, "column": column(c, m)}
                      for c, m, _, _ in COMPILERS],
        "projects": rows,
        "unbuilt": unbuilt,
        "cacheless": CACHELESS,
    }
    RESULTS.write_text(json.dumps(payload, indent=2) + "\n")
    print(f"\nwrote {RESULTS.relative_to(REPO)}")
    report(rows)


def report(rows):
    """The markdown summary, one table per mode.

    Never one table across modes: a full build and a no-op are not comparable,
    and putting them side by side invites exactly that comparison.
    """
    for mode in (FULL, INCREMENTAL, NOOP):
        compilers = [c for c, m, _, _ in COMPILERS if m == mode]
        present = [
            c for c in compilers
            if any(row.get(column(c, mode)) is not None for row in rows)
        ]
        if not present:
            continue
        print(f"\n**{mode}**\n")
        print("| project | " + " | ".join(present) + " |")
        print("|---" * (len(present) + 1) + "|")
        for row in rows:
            cells = " | ".join(
                "—" if row.get(column(c, mode)) is None
                else f"{row[column(c, mode)]:.0f} ms"
                for c in present
            )
            print(f"| {row['op']} | {cells} |")


if __name__ == "__main__":
    main()
