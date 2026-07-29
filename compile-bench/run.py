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

def wipe_cache(directory):
    """Clear `elm-stuff` so the official compiler rebuilds the dependency set."""
    shutil.rmtree(directory / "elm-stuff", ignore_errors=True)


# elm gets two columns because it has two speeds and they differ by ~7x. With
# `elm-stuff` cleared it recompiles the dependency set, which is what alm does
# on every run; with the cache warm it recompiles only what changed. Showing
# just one of them would either flatter alm or flatter elm, so both are here.
COMPILERS = [
    ("elm (full)", lambda entry, out: ["elm", "make", entry, f"--output={out}.js"], wipe_cache),
    ("elm (incr.)", lambda entry, out: ["elm", "make", entry, f"--output={out}.js"], None),
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


# ------------------------------------------------------------- per compiler

def matrix(projects, scratch):
    """Every compiler against every project all of them can build."""
    out = []
    for name, directory, entry in projects:
        target = scratch / "out"
        # A workload has to build everywhere, or the columns are not measuring
        # the same work. One that does not is dropped, and said so.
        broken = [
            label for label, build, _ in COMPILERS
            if time_it(build(entry, str(target)), directory) is None
        ]
        if broken:
            print(f"  {name:32s} skipped — {', '.join(broken)} cannot build it")
            continue

        row = {"op": name, "lines": elm_count(directory)}
        for label, build, setup in COMPILERS:
            runs = []
            for _ in range(RUNS_MATRIX):
                if setup:
                    setup(directory)
                runs.append(time_it(build(entry, str(target)), directory))
            row[label] = median_ms(runs)
        cells = "  ".join(f"{label} {row[label]:.0f}" for label, _, _ in COMPILERS)
        print(f"  {name:32s} {cells}")
        out.append(row)
    return out


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

        print("== per compiler ==")
        rows = matrix(projects, scratch)

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
