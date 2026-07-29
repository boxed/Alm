#!/usr/bin/env python3
"""Compile-speed benchmark: alm against the official compiler.

Measures wall-clock time to produce JavaScript for a real Elm codebase, in the
four situations that actually matter during development:

  * project-cold   elm-stuff wiped, so the official compiler redoes everything
  * incremental    the entry file touched, its usual inner-loop case
  * no-op          nothing changed at all, its floor
  * alm            every run, no cache of any kind

The point of the comparison is the last row: alm has no artifact cache, so its
one number has to be read against all three of the others.

Run:  python3 compile-bench/run.py [path/to/elm/project]

The project defaults to $ALM_BENCH_PROJECT, then to ../dryft. Any Elm
application with a `src` directory works; the numbers in the README came from
a ~40k-line one.
"""

import json
import os
import pathlib
import shutil
import statistics
import subprocess
import sys
import tempfile
import time

REPO = pathlib.Path(__file__).resolve().parent.parent
ALM = REPO / "target" / "release" / "alm"
# Medians over this many runs. Suites are slow, so they get fewer.
RUNS_SINGLE = 5
RUNS_SUITE = 3


def find_project(argv):
    if len(argv) > 1:
        candidate = pathlib.Path(argv[1]).expanduser()
    elif os.environ.get("ALM_BENCH_PROJECT"):
        candidate = pathlib.Path(os.environ["ALM_BENCH_PROJECT"]).expanduser()
    else:
        candidate = REPO.parent / "dryft"
    if not (candidate / "elm.json").is_file():
        sys.exit(
            f"No elm.json in {candidate}.\n"
            "Pass an Elm application directory, or set ALM_BENCH_PROJECT."
        )
    return candidate.resolve()


def entry_points(src):
    """Modules with a top-level `main`, which is what can be compiled."""
    out = []
    for path in sorted(src.rglob("*.elm")):
        text = path.read_text(encoding="utf8", errors="replace")
        if any(line.startswith("main =") or line.startswith("main :") for line in text.splitlines()):
            out.append(path)
    return out


def buildable(project, work, entries, out):
    """Drop entry points the official compiler itself rejects.

    A real codebase can contain a module with a `main` that does not build on
    its own — one whose declared name does not match its path, say. Timing a
    failure is not timing a compile, and it has to be excluded from *both*
    sides or the two are not doing the same work.
    """
    keep, dropped = [], []
    for path in entries:
        rel = str(path.relative_to(project))
        done = subprocess.run(
            ["elm", "make", rel, f"--output={out}"], cwd=work, capture_output=True, text=True
        )
        (keep if done.returncode == 0 else dropped).append(path)
    for path in dropped:
        print(f"  skipping {path.name}: the official compiler cannot build it on its own")
    return keep


def time_it(command, cwd):
    """Wall-clock seconds for one command, or None if it failed.

    Output goes to a real file, never /dev/null: the official compiler skips
    code generation entirely when the output is /dev/null, which would flatter
    it by the whole back end.
    """
    start = time.perf_counter()
    done = subprocess.run(command, cwd=cwd, capture_output=True, text=True)
    elapsed = time.perf_counter() - start
    if done.returncode != 0:
        print(f"    ! {' '.join(command[:3])} failed:\n{done.stdout[-400:]}{done.stderr[-400:]}")
        return None
    return elapsed


def median_of(runs, label):
    ok = [r for r in runs if r is not None]
    if not ok:
        return None
    print(f"    {label}: median {statistics.median(ok) * 1000:7.0f} ms   "
          f"best {min(ok) * 1000:7.0f} ms   ({len(ok)} runs)")
    return (statistics.median(ok), min(ok))


def fresh_copy(project, into):
    """A throwaway copy, so the benchmark never touches the real working tree.

    elm-stuff and the mtimes of source files are both inputs to what is being
    measured, and both get mutated here.

    Only `elm.json` and the declared source directories are copied. An Elm
    project often sits inside a larger repository — the one these numbers come
    from has a running server's control socket in it, which is not copyable and
    has nothing to do with the build.
    """
    target = into / "project"
    if target.exists():
        shutil.rmtree(target)
    target.mkdir(parents=True)
    shutil.copy2(project / "elm.json", target / "elm.json")
    outline = json.loads((project / "elm.json").read_text())
    for rel in outline.get("source-directories", ["src"]):
        source = (project / rel).resolve()
        if source.is_dir():
            shutil.copytree(source, target / rel, ignore=shutil.ignore_patterns("elm-stuff"))
    return target


def main():
    project = find_project(sys.argv)
    if not ALM.is_file():
        sys.exit(f"No alm binary at {ALM}. Build it first:\n    cargo build --release -p alm")

    src = project / "src"
    entries = entry_points(src)
    lines = sum(
        len(p.read_text(encoding="utf8", errors="replace").splitlines())
        for p in src.rglob("*.elm")
    )
    biggest = max(entries, key=lambda p: len(p.read_text(encoding="utf8", errors="replace").splitlines()))
    biggest_lines = len(biggest.read_text(encoding="utf8", errors="replace").splitlines())

    print(f"project      {project}")
    print(f"             {lines:,} lines of Elm, {len(entries)} entry points")
    print(f"single entry {biggest.relative_to(project)} ({biggest_lines:,} lines)")
    print(f"alm          {subprocess.run([str(ALM), '--help'], capture_output=True, text=True).stdout.splitlines()[0]}")
    print(f"elm          {subprocess.run(['elm', '--version'], capture_output=True, text=True).stdout.strip()}")
    print()

    results = {}
    with tempfile.TemporaryDirectory(dir=REPO / ".almtmp") as scratch:
        scratch = pathlib.Path(scratch)
        work = fresh_copy(project, scratch)
        out = scratch / "out.js"
        entry = work / biggest.relative_to(project)
        rel = str(biggest.relative_to(project))

        print(f"== one entry point: {rel} ==")

        print("  elm, project-cold (elm-stuff wiped each run)")
        runs = []
        for _ in range(RUNS_SINGLE):
            shutil.rmtree(work / "elm-stuff", ignore_errors=True)
            runs.append(time_it(["elm", "make", rel, f"--output={out}"], work))
        results["elm-cold"] = median_of(runs, "elm cold")

        print("  elm, incremental (entry file touched)")
        runs = []
        for _ in range(RUNS_SINGLE):
            entry.touch()
            runs.append(time_it(["elm", "make", rel, f"--output={out}"], work))
        results["elm-incremental"] = median_of(runs, "elm incr")

        print("  elm, no-op (nothing changed)")
        runs = [time_it(["elm", "make", rel, f"--output={out}"], work) for _ in range(RUNS_SINGLE)]
        results["elm-noop"] = median_of(runs, "elm no-op")

        print("  alm, full rebuild, no cache")
        runs = [time_it([str(ALM), "make", rel, f"--output={out}"], work) for _ in range(RUNS_SINGLE)]
        results["alm"] = median_of(runs, "alm")

        print()
        entries = buildable(project, work, entries, out)
        print(f"== all {len(entries)} entry points ==")
        rels = [str(p.relative_to(project)) for p in entries]

        def compile_all(binary):
            total = 0.0
            for r in rels:
                took = time_it([binary, "make", r, f"--output={out}"], work)
                if took is None:
                    return None
                total += took
            return total

        print("  elm, project-cold")
        runs = []
        for _ in range(RUNS_SUITE):
            shutil.rmtree(work / "elm-stuff", ignore_errors=True)
            runs.append(compile_all("elm"))
        results["suite-elm-cold"] = median_of(runs, "elm cold")

        print("  elm, all sources touched (warm elm-stuff)")
        runs = []
        for _ in range(RUNS_SUITE):
            now = time.time()
            for p in work.joinpath("src").rglob("*.elm"):
                os.utime(p, (now, now))
            runs.append(compile_all("elm"))
        results["suite-elm-touched"] = median_of(runs, "elm touched")

        print("  alm, full rebuild every time")
        runs = [compile_all(str(ALM)) for _ in range(RUNS_SUITE)]
        results["suite-alm"] = median_of(runs, "alm")
        suite_count = len(entries)

    print()
    report(results, biggest_lines, suite_count, lines)
    write_results(results, project, biggest, biggest_lines, suite_count, lines)


def write_results(results, project, entry, entry_lines, entry_count, total_lines):
    """Record the run as JSON for the report generator.

    Includes when it was taken: a report assembled from several benchmarks has
    no other way to notice that one of its sections is older than the rest,
    and that is exactly how the compile figures went stale once.
    """
    import datetime, json, platform, subprocess

    def pair(key):
        value = results.get(key)
        return None if value is None else {
            "median_ms": round(value[0] * 1000, 1),
            "best_ms": round(value[1] * 1000, 1),
        }

    payload = {
        "measured": datetime.datetime.now().astimezone().isoformat(timespec="seconds"),
        "machine": f"{platform.system()} {platform.machine()}",
        "elm_version": subprocess.run(
            ["elm", "--version"], capture_output=True, text=True
        ).stdout.strip(),
        "project": {
            "entry": str(entry.relative_to(project)),
            "entry_lines": entry_lines,
            "total_lines": total_lines,
            "entry_points": entry_count,
        },
        "runs": {"single": RUNS_SINGLE, "suite": RUNS_SUITE},
        "single": {
            "elm project-cold": pair("elm-cold"),
            "elm incremental": pair("elm-incremental"),
            "elm no-op": pair("elm-noop"),
            "alm full rebuild": pair("alm"),
        },
        "suite": {
            "elm project-cold": pair("suite-elm-cold"),
            "elm all touched": pair("suite-elm-touched"),
            "alm full rebuild": pair("suite-alm"),
        },
    }
    out = pathlib.Path(__file__).resolve().parent / "results.json"
    out.write_text(json.dumps(payload, indent=2) + "\n")
    print(f"\nwrote {out.relative_to(REPO)}")


def ms(value):
    return "—" if value is None else f"{value[0] * 1000:.0f} ms"


def best(value):
    return "—" if value is None else f"{value[1] * 1000:.0f} ms"


def secs(value):
    return "—" if value is None else f"{value[0]:.2f} s"


def report(r, entry_lines, entry_count, total_lines):
    print("Markdown for the README:\n")
    print(f"One {entry_lines:,}-line entry point and its module graph:\n")
    print("| | median | best |")
    print("|---|---|---|")
    print(f"| elm 0.19.1, project-cold (elm-stuff wiped) | {ms(r['elm-cold'])} | {best(r['elm-cold'])} |")
    print(f"| elm 0.19.1, incremental (entry file touched) | {ms(r['elm-incremental'])} | {best(r['elm-incremental'])} |")
    print(f"| elm 0.19.1, no-op (nothing changed at all) | {ms(r['elm-noop'])} | {best(r['elm-noop'])} |")
    print(f"| **alm, full rebuild, no cache** | **{ms(r['alm'])}** | **{best(r['alm'])}** |")
    print()
    print(f"All {entry_count} entry points of the same codebase (~{total_lines // 1000}k lines):\n")
    print("| | median |")
    print("|---|---|")
    print(f"| elm 0.19.1, project-cold | {secs(r['suite-elm-cold'])} |")
    print(f"| elm 0.19.1, all sources touched (warm elm-stuff) | {secs(r['suite-elm-touched'])} |")
    print(f"| **alm, full rebuild every time, no cache** | **{secs(r['suite-alm'])}** |")
    print()
    if r["alm"] and r["elm-incremental"]:
        print(f"alm full rebuild vs elm incremental: {r['elm-incremental'][0] / r['alm'][0]:.1f}x")
    if r["alm"] and r["elm-noop"]:
        print(f"alm full rebuild vs elm doing nothing: {r['elm-noop'][0] / r['alm'][0]:.1f}x")
    if r["suite-alm"] and r["suite-elm-cold"]:
        print(f"suite, vs elm project-cold:           {r['suite-elm-cold'][0] / r['suite-alm'][0]:.1f}x")
    if r["suite-alm"] and r["suite-elm-touched"]:
        print(f"suite, vs elm all-touched:            {r['suite-elm-touched'][0] / r['suite-alm'][0]:.1f}x")


if __name__ == "__main__":
    main()
