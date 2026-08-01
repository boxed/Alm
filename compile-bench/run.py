#!/usr/bin/env python3
"""Compile-speed benchmark: how fast each compiler builds the same project.

Two things are measured.

**Per compiler.** One column for elm and one for each alm backend, over a
spread of real projects — the same shape as the runtime and computation
benchmarks, so the report reads consistently.

**Against elm's cache.** alm keeps no incremental cache: every invocation
recompiles the whole module graph, package sources included. So the table
above compares alm's only mode against elm's cold one. The second table also
puts alm next to elm's *incremental* and *no-op* paths, which is the honest
comparison for day-to-day editing.

    python3 compile-bench/run.py [extra/project/dir ...]

Workloads are built from `~/.elm`; nothing is downloaded. A local application
is picked up from $ALM_BENCH_PROJECT, or ../dryft if it is there.
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


def wipe_cache(directory, entry):
    """Clear `elm-stuff` so the official compiler rebuilds the dependency set."""
    shutil.rmtree(directory / "elm-stuff", ignore_errors=True)


def touch_entry(directory, entry):
    """Mark the entry module changed, so the warm cache has something to rebuild.

    Without this the "incremental" column is really elm's *no-op* time: the run
    before it left a complete cache and nothing has changed since, so elm checks
    mtimes and exits. Touching one module is the edit-and-rebuild loop the
    column is meant to stand for.
    """
    (directory / entry).touch()


# elm gets two columns because it has two speeds and they differ by ~7x. With
# `elm-stuff` cleared it recompiles the dependency set, which is what alm does
# on every run; with the cache warm it recompiles only what changed. Showing
# just one of them would either flatter alm or flatter elm, so both are here.
COMPILERS = [
    ("elm (full)", lambda entry, out: ["elm", "make", entry, f"--output={out}.js"], wipe_cache),
    ("elm (incr.)", lambda entry, out: ["elm", "make", entry, f"--output={out}.js"], touch_entry),
    ("alm-js", lambda entry, out: [str(ALM), "make", entry, f"--output={out}.js"], None),
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


# -------------------------------------------------------- against elm's cache

def caching(project, scratch):
    """alm's one number against all three of elm's, on one real application."""
    work = scratch / "caching"
    if work.exists():
        shutil.rmtree(work)
    work.mkdir(parents=True)
    # Only elm.json and the source directories: `elm-stuff` and source mtimes
    # are inputs here and both get mutated, and a real project often sits in a
    # bigger repository holding things that cannot even be copied.
    shutil.copy2(project / "elm.json", work / "elm.json")
    outline = json.loads((project / "elm.json").read_text())
    for rel in outline.get("source-directories", ["src"]):
        if (project / rel).is_dir():
            shutil.copytree(project / rel, work / rel,
                            ignore=shutil.ignore_patterns("elm-stuff"))

    entries = []
    for path in sorted((work / "src").rglob("*.elm")):
        text = path.read_text(encoding="utf8", errors="replace")
        if any(l.startswith("main =") or l.startswith("main :") for l in text.splitlines()):
            entries.append(path)
    if not entries:
        return None

    biggest = max(entries, key=lambda p: len(p.read_text(errors="replace").splitlines()))
    rel = str(biggest.relative_to(work))
    out = scratch / "c.js"
    result = {"project": {
        "name": project.name,
        "entry": rel,
        "entry_lines": len(biggest.read_text(errors="replace").splitlines()),
        "total_lines": elm_count(work),
    }}

    print(f"  one entry point: {rel}")
    runs = []
    for _ in range(RUNS_SINGLE):
        shutil.rmtree(work / "elm-stuff", ignore_errors=True)
        runs.append(time_it(["elm", "make", rel, f"--output={out}"], work))
    cold = median_ms(runs)

    runs = []
    for _ in range(RUNS_SINGLE):
        biggest.touch()
        runs.append(time_it(["elm", "make", rel, f"--output={out}"], work))

    result["single"] = {
        "elm, project-cold": cold,
        "elm, incremental": median_ms(runs),
        "elm, no-op": median_ms([time_it(["elm", "make", rel, f"--output={out}"], work)
                                 for _ in range(RUNS_SINGLE)]),
        "alm, full rebuild": median_ms([time_it([str(ALM), "make", rel, f"--output={out}"], work)
                                        for _ in range(RUNS_SINGLE)]),
    }
    for label, value in result["single"].items():
        print(f"    {label:26s} {value:6.0f} ms")

    # Every entry point, which is what building the project actually costs.
    buildable = [
        p for p in entries
        if time_it(["elm", "make", str(p.relative_to(work)), f"--output={out}"], work) is not None
    ]
    dropped = len(entries) - len(buildable)
    if dropped:
        print(f"  {dropped} entry point(s) the official compiler cannot build alone: skipped")

    def whole(binary):
        total = 0.0
        for p in buildable:
            took = time_it([binary, "make", str(p.relative_to(work)), f"--output={out}"], work)
            if took is None:
                return None
            total += took
        return total

    runs = []
    for _ in range(RUNS_SUITE):
        shutil.rmtree(work / "elm-stuff", ignore_errors=True)
        runs.append(whole("elm"))
    suite_cold = median_ms(runs)

    runs = []
    for _ in range(RUNS_SUITE):
        now = time.time()
        for p in (work / "src").rglob("*.elm"):
            os.utime(p, (now, now))
        runs.append(whole("elm"))

    result["project"]["entry_points"] = len(buildable)
    result["suite"] = {
        "elm, project-cold": suite_cold,
        "elm, all sources touched": median_ms(runs),
        "alm, full rebuild": median_ms([whole(str(ALM)) for _ in range(RUNS_SUITE)]),
    }
    print(f"  all {len(buildable)} entry points:")
    for label, value in result["suite"].items():
        print(f"    {label:26s} {value / 1000:6.2f} s")
    return result


# ------------------------------------------------------------------- driving

def local_project(argv):
    candidates = [pathlib.Path(a).expanduser() for a in argv[1:]]
    if os.environ.get("ALM_BENCH_PROJECT"):
        candidates.append(pathlib.Path(os.environ["ALM_BENCH_PROJECT"]).expanduser())
    candidates.append(REPO.parent / "dryft")
    for candidate in candidates:
        if (candidate / "elm.json").is_file():
            return candidate.resolve()
    return None


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

        local = local_project(sys.argv)
        detail = None
        if local:
            print(f"\n== against elm's cache: {local.name} ==")
            detail = caching(local, scratch)
        else:
            print("\nNo local application found; skipping the cache comparison.")

    payload = {
        "measured": datetime.datetime.now().astimezone().isoformat(timespec="seconds"),
        "machine": f"{platform.system()} {platform.machine()}",
        "elm_version": elm_version,
        "runs": {"matrix": RUNS_MATRIX, "single": RUNS_SINGLE, "suite": RUNS_SUITE},
        "compilers": [label for label, _, _ in COMPILERS],
        "projects": rows,
        "unbuilt": unbuilt,
        "caching": detail,
    }
    RESULTS.write_text(json.dumps(payload, indent=2) + "\n")
    print(f"\nwrote {RESULTS.relative_to(REPO)}")
    report(rows, detail)


def report(rows, detail):
    if rows:
        print("\n| project | " + " | ".join(l for l, _, _ in COMPILERS) + " |")
        print("|---" * (len(COMPILERS) + 1) + "|")
        for row in rows:
            cells = " | ".join(
                "—" if row[l] is None else f"{row[l]:.0f} ms" for l, _, _ in COMPILERS
            )
            print(f"| {row['op']} | {cells} |")
    if detail:
        print()
        for label, value in detail["single"].items():
            print(f"| {label} | {value:.0f} ms |")
        for label, value in detail["suite"].items():
            print(f"| {label} (all entry points) | {value / 1000:.2f} s |")


if __name__ == "__main__":
    main()
