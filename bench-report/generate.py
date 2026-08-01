#!/usr/bin/env python3
"""Build the benchmark report from the harnesses' results.

    make report                     (or: python3 bench-report/generate.py)

Every figure, column header and date on the page comes from a `results.json`
written by the harness that measured it:

    dom-bench/build/results.json    runtime
    compute-bench/results.json      computation
    compile-bench/results.json      compile speed

`template.html` is presentation only — CSS and a renderer. **Nothing on the
page is authored.** Earlier versions carried hand-written methodology prose,
which drifted out of step with the numbers every time a benchmark was re-run,
to the point of describing measurements the page was no longer showing. If
something needs explaining, the harness should record it in its results so it
can be rendered from there.
"""

import datetime
import json
import pathlib
import re
import sys

REPO = pathlib.Path(__file__).resolve().parent.parent
OUT = REPO / "bench-report" / "report.html"
TEMPLATE = REPO / "bench-report" / "template.html"

SOURCES = {
    "runtime": REPO / "dom-bench" / "build" / "results.json",
    "compute": REPO / "compute-bench" / "results.json",
    "compile": REPO / "compile-bench" / "results.json",
}

# A section measured more than this far behind the newest one is flagged.
STALE_AFTER = datetime.timedelta(days=2)


def check_template_has_no_copy(text):
    """Fail if anyone (me included) writes page copy into the template.

    The rule this enforces: the template holds CSS and a renderer, and the body
    is one empty container. Every word the page shows comes from the results.
    It is written as a check rather than a comment because the comment was
    already there and did not stop it happening.
    """
    body = text.split("</style>", 1)[-1]
    for chunk in re.split(r"<script\b.*?</script>", body, flags=re.S):
        stripped = re.sub(r"<[^>]+>", "", chunk).strip()
        if stripped:
            sys.exit(
                f"template.html has authored copy in its body: {stripped[:80]!r}\n"
                "The page is generated; put anything that needs saying into the "
                "harness's results.json so it can be rendered from data."
            )


def load(path):
    """Results plus when they were taken.

    A harness that records `measured` is believed; otherwise the file's
    modification time stands in. Either way the page shows a date, so a
    section that has fallen behind cannot pass for current.
    """
    if not path.is_file():
        return None, None
    data = json.loads(path.read_text())
    stamp = None
    if isinstance(data, dict) and "measured" in data:
        stamp = datetime.datetime.fromisoformat(data["measured"])
    if stamp is None:
        stamp = datetime.datetime.fromtimestamp(path.stat().st_mtime).astimezone()
    return data, stamp


def freshness(stamp, newest):
    if stamp is None:
        return {"state": "missing", "note": "never measured"}
    when = stamp.strftime("%Y-%m-%d")
    if newest and newest - stamp > STALE_AFTER:
        return {"state": "stale",
                "note": f"{when} — {(newest - stamp).days} days behind the newest run"}
    return {"state": "fresh", "note": when}


# -------------------------------------------------------------------- sections

def runtime_section(data):
    columns = [
        ("elm", "elm"), ("elm-opt", "elm (lazy)"),
        ("alm-js", "alm-js"), ("alm-js-opt", "alm-js (lazy)"),
        ("alm-wasm", "alm-wasm"), ("alm-wasm-opt", "alm-wasm (lazy)"),
        ("react", "react"), ("react-opt", "react (memo)"), ("svelte", "svelte"),
    ]
    present = [(k, label) for k, label in columns if k in data]
    groups = [
        ("bulk render", ["create 1k", "replace 1k", "create 10k", "clear 1k"]),
        ("incremental updates", ["select", "update 10th", "swap", "remove", "append 1k"]),
    ]
    tables = []
    for caption, ops in groups:
        rows = []
        for op in ops:
            row = {"op": op}
            for key, label in present:
                if op in data.get(key, {}):
                    row[label] = data[key][op]
            if len(row) > 1:
                rows.append(row)
        tables.append({
            "caption": f"{caption} · milliseconds, paint-inclusive",
            "columns": [label for _, label in present],
            "rows": rows, "decimals": 1, "unit": "",
        })
    return {
        "title": "Runtime",
        "link": {"text": "js-framework-benchmark",
                 "href": "https://github.com/krausest/js-framework-benchmark"},
        "tables": tables,
    }


def compute_section(data):
    columns = [("elm", "elm"), ("js", "alm-js"), ("wasm", "alm-wasm"),
               ("native", "alm-native")]
    rows = []
    for entry in data:
        row = {"op": entry["name"]}
        for key, label in columns:
            if entry.get(key) is not None:
                row[label] = round(entry[key], 1)
        rows.append(row)
    # A compiler with nothing measured gets no column, rather than an empty one
    # implying it ran and came back blank.
    measured = {label for row in rows for label in row if label != "op"}
    return {
        "title": "Computation — no DOM",
        "tables": [{
            "caption": "milliseconds",
            "columns": [label for _, label in columns if label in measured],
            "rows": rows, "decimals": 1, "unit": "",
        }],
    }


def compile_section(data):
    tables = [{
        "label": "project",
        "caption": "milliseconds · cold builds recompile the whole module graph; (incr.) rebuilds one edited module",
        "columns": [c for c in data["compilers"]
                    if any(row.get(c) is not None for row in data.get("projects", []))],
        "rows": [{"op": row["op"],
                  **{c: row[c] for c in data["compilers"] if row.get(c) is not None}}
                 for row in data.get("projects", [])],
        "decimals": 0, "unit": "",
    }]
    # An n/a in this table means that compiler could not build that project —
    # say which, rather than leave a blank cell to be read as a missing run.
    by_compiler = {}
    for entry in data.get("unbuilt") or []:
        by_compiler.setdefault(entry["compiler"], []).append(entry["project"])
    notes = [
        f"n/a — {compiler} cannot build {', '.join(sorted(projects))}"
        for compiler, projects in sorted(by_compiler.items())
    ]
    # A compiler with no incremental column has one because it has one speed,
    # not because the run was skipped. Say which and why.
    for compiler, why in sorted((data.get("cacheless") or {}).items()):
        notes.append(f"no incremental column for {compiler} — {why}")
    if notes:
        tables[0]["notes"] = notes

    detail = data.get("caching")
    facts = None
    if detail:
        project = detail["project"]
        facts = (f"cold against warm: {project['name']} · {project['total_lines']:,} lines · "
                 f"{project.get('entry_points', '?')} entry points")
        # The two rows measure different modes — there is no no-op for a
        # whole-project build — so columns are keyed by name, and a mode a row
        # did not measure is left blank rather than shifted into the next.
        rows = {f"one entry point · {project['entry']}": detail.get("single", {})}
        if detail.get("suite"):
            rows[f"all {project.get('entry_points', '')} entry points"] = detail["suite"]
        order = ["elm, project-cold", "elm, incremental", "elm, all sources touched",
                 "elm, no-op", "alm, project-cold", "alm, incremental",
                 "alm, all sources touched", "alm, no-op"]
        seen = {name for modes in rows.values() for name in modes}
        cols = [c for c in order if c in seen] + [c for c in sorted(seen) if c not in order]
        tables.append({
            "label": "build",
            "caption": "milliseconds · every mode both compilers have, on one real application",
            "columns": cols,
            "rows": [{"op": label, **{k: v for k, v in modes.items() if v is not None}}
                     for label, modes in rows.items()],
            "decimals": 0, "unit": "",
        })

    parts = [data.get("machine")]
    if data.get("elm_version"):
        parts.append(f"elm {data['elm_version']}")
    parts.append(facts)
    return {"title": "Compile speed", "tables": tables,
            "facts": " · ".join(p for p in parts if p)}


BUILDERS = {
    "runtime": runtime_section,
    "compute": compute_section,
    "compile": compile_section,
}


def main():
    data, stamps = {}, {}
    for name, path in SOURCES.items():
        data[name], stamps[name] = load(path)

    newest = max((s for s in stamps.values() if s), default=None)
    sections = []
    for name in SOURCES:
        fresh = freshness(stamps[name], newest)
        if data[name] is None:
            print(f"  {name:8s} {fresh['note']}", file=sys.stderr)
            sections.append({"title": name.capitalize(), "freshness": fresh, "tables": []})
            continue
        section = BUILDERS[name](data[name])
        section["freshness"] = fresh
        sections.append(section)
        print(f"  {name:8s} {fresh['note']}")

    now = datetime.datetime.now().astimezone()
    payload = {
        "title": "alm — runtime, computation and compile speed",
        "generated": f"generated {now.strftime('%Y-%m-%d %H:%M %Z')} by make report",
        "sections": sections,
    }
    template = TEMPLATE.read_text()
    check_template_has_no_copy(template)
    OUT.write_text(template.replace("/*{{DATA}}*/", json.dumps(payload, indent=2)))
    print(f"wrote {OUT.relative_to(REPO)}")


if __name__ == "__main__":
    main()
