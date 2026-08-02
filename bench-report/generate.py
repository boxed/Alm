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
    """One table per build mode.

    A number is only meaningful against another number of the same kind. With
    every mode in one table the bar and the star were scaled to the row's best
    across all of them, so a no-op time set the scale a full build was drawn
    against — and a column that simply had no incremental mode read as a gap
    rather than as a fact about that back end. Each mode gets its own table, and
    each says which compilers can do it and why the others cannot.
    """
    modes = [
        (
            "full",
            "every module recompiled from scratch, build cache cleared",
        ),
        (
            "incremental",
            "one module edited, cache warm — what you wait for while working",
        ),
        (
            "no-op",
            "nothing changed at all — what a save that touched nothing costs",
        ),
    ]
    # (compiler, mode) -> the key its measurements are recorded under.
    columns = data.get("compilers") or []
    projects = data.get("projects") or []
    cacheless = data.get("cacheless") or {}

    unbuilt = {}
    for entry in data.get("unbuilt") or []:
        unbuilt.setdefault(entry["compiler"], []).append(entry["project"])

    tables = []
    for mode, caption in modes:
        present = [c for c in columns if c["mode"] == mode]
        names = [
            c["name"] for c in present
            if any(row.get(c["column"]) is not None for row in projects)
        ]
        if not names:
            continue
        by_name = {c["name"]: c["column"] for c in present}
        rows = [
            {
                "op": row["op"],
                **{
                    name: row[by_name[name]]
                    for name in names
                    if row.get(by_name[name]) is not None
                },
            }
            for row in projects
        ]
        notes = [
            f"n/a — {compiler} cannot build {', '.join(sorted(where))}"
            for compiler, where in sorted(unbuilt.items())
            if compiler in names
        ]
        # Say who is missing from this mode, so an absent column is read as a
        # fact about the back end rather than as a measurement nobody took.
        missing = sorted(
            compiler for compiler, why in cacheless.items()
            if compiler not in names and any(c["name"] == compiler for c in columns)
        )
        for compiler in missing:
            notes.append(f"no {mode} build for {compiler} — {cacheless[compiler]}")
        tables.append({
            "label": "project",
            "caption": f"{mode} · milliseconds · {caption}",
            "columns": names,
            "rows": rows,
            "decimals": 0,
            "unit": "",
            **({"notes": notes} if notes else {}),
        })

    parts = [data.get("machine")]
    if data.get("elm_version"):
        parts.append(f"elm {data['elm_version']}")
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
