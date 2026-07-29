#!/usr/bin/env python3
"""Build the benchmark report from the harnesses' results, not by hand.

    python3 bench-report/generate.py            # writes bench-report/report.html

Every number on the page comes from a `results.json` written by the harness
that measured it:

    dom-bench/build/results.json    runtime, memory, bundle size
    compute-bench/results.json      pure-computation workloads
    compile-bench/results.json      compile speed

The prose is hand-written and lives in this file; the tables are not. That
split is the point. The report used to carry its numbers as JavaScript arrays
edited in place, and a section went a year stale without anyone noticing —
the page even carried a footnote admitting some figures were older than the
rest. Now a section states when it was measured, and says so loudly when it
is behind the others.

Re-running one harness and regenerating is enough; the untouched sections keep
their own dates.
"""

import datetime
import html
import json
import pathlib
import sys

REPO = pathlib.Path(__file__).resolve().parent.parent
OUT = REPO / "bench-report" / "report.html"

SOURCES = {
    "runtime": REPO / "dom-bench" / "build" / "results.json",
    "compute": REPO / "compute-bench" / "results.json",
    "compile": REPO / "compile-bench" / "results.json",
}

# A section measured more than this long before the newest one is called out
# on the page rather than blending in.
STALE_AFTER = datetime.timedelta(days=2)


def load(path):
    """Results plus when they were taken.

    Only compile-bench records its own timestamp; for the other two the file's
    modification time is what there is. Both are reported the same way, so a
    stale section is visible either way.
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


# --------------------------------------------------------------------- tables

def runtime_tables(data):
    """The keyed-table app, split into bulk and incremental operations."""
    columns = [
        ("elm", "elm (naive)"),
        ("elm-opt", "elm (opt)"),
        ("alm-js", "alm-js (naive)"),
        ("alm-js-opt", "alm-js (opt)"),
        ("alm-wasm", "alm-wasm (naive)"),
        ("alm-wasm-opt", "alm-wasm (opt)"),
        ("react", "react (naive)"),
        ("react-opt", "react (opt)"),
        ("svelte", "svelte"),
    ]
    present = [(key, label) for key, label in columns if key in data]
    bulk_ops = ["create 1k", "replace 1k", "create 10k", "clear 1k"]
    incr_ops = ["select", "update 10th", "swap", "remove", "append 1k"]

    def rows(ops):
        out = []
        for op in ops:
            row = {"op": op}
            for key, label in present:
                if op in data.get(key, {}):
                    row[label] = data[key][op]
            out.append(row)
        return out

    labels = [label for _, label in present]
    return rows(bulk_ops), rows(incr_ops), labels


def compute_table(data):
    """Same Elm source through official elm and alm's three backends."""
    columns = [("elm", "elm"), ("js", "alm-js"), ("wasm", "alm-wasm"),
               ("native", "alm-native")]
    rows = []
    for entry in data:
        row = {"op": entry["name"]}
        for key, label in columns:
            if entry.get(key) is not None:
                row[label] = round(entry[key], 1)
        rows.append(row)
    return rows, [label for _, label in columns]


def compile_tables(data):
    """Compile speed, as the harness measured it."""
    def rows(section, unit):
        out = []
        for label, value in data[section].items():
            if value is None:
                continue
            ms = value["median_ms"]
            out.append({
                "op": label,
                "median": round(ms / 1000, 2) if unit == "s" else round(ms),
                "best": round(value["best_ms"] / 1000, 2) if unit == "s"
                        else round(value["best_ms"]),
            })
        return out
    return rows("single", "ms"), rows("suite", "s")


# ----------------------------------------------------------------------- page

def freshness(stamps):
    """A dated badge per section, flagged when it lags the newest run."""
    newest = max((s for s in stamps.values() if s), default=None)
    out = {}
    for name, stamp in stamps.items():
        if stamp is None:
            out[name] = ("missing", "never measured")
            continue
        when = stamp.strftime("%Y-%m-%d")
        if newest and newest - stamp > STALE_AFTER:
            behind = (newest - stamp).days
            out[name] = ("stale", f"measured {when} — {behind} days older than the newest run")
        else:
            out[name] = ("fresh", f"measured {when}")
    return out


def main():
    data, stamps = {}, {}
    for name, path in SOURCES.items():
        data[name], stamps[name] = load(path)

    missing = [n for n, d in data.items() if d is None]
    if missing:
        print(f"no results for: {', '.join(missing)} — those sections will say so",
              file=sys.stderr)

    tags = freshness(stamps)
    payload = {"freshness": {k: {"state": v[0], "note": v[1]} for k, v in tags.items()}}

    if data["runtime"]:
        bulk, incr, cols = runtime_tables(data["runtime"])
        payload["bulk"], payload["incr"], payload["runtimeCols"] = bulk, incr, cols
    if data["compute"]:
        rows, cols = compute_table(data["compute"])
        payload["compute"], payload["computeCols"] = rows, cols
    if data["compile"]:
        single, suite = compile_tables(data["compile"])
        payload["compileSingle"], payload["compileSuite"] = single, suite
        payload["compileProject"] = data["compile"]["project"]

    template = (REPO / "bench-report" / "template.html").read_text()
    page = template.replace("/*{{DATA}}*/", json.dumps(payload, indent=2))
    page = page.replace(
        "{{GENERATED}}",
        html.escape(datetime.datetime.now().astimezone().strftime("%Y-%m-%d %H:%M %Z")),
    )
    OUT.write_text(page)
    print(f"wrote {OUT.relative_to(REPO)}")
    for name, (state, note) in tags.items():
        print(f"  {name:8s} {note}")


if __name__ == "__main__":
    main()
