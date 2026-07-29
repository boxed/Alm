# bench-report

Builds the published benchmark page from the harnesses' results.

```
python3 bench-report/generate.py      # -> bench-report/report.html
```

## Why it is generated

The report used to carry its numbers as JavaScript arrays inside the HTML,
updated by hand after each run. Predictably, they drifted: the compile figures
sat a week behind the runtime ones, and the only sign was a footnote someone
had remembered to write. Anything hand-copied between a measurement and a
published claim will eventually be wrong.

So the split is: **prose is written, numbers are not.** The methodology text
lives in `template.html`; every figure comes from a `results.json` that the
harness which measured it wrote.

| section | source |
|---|---|
| runtime, memory, bundle size | `dom-bench/build/results.json` |
| computation | `compute-bench/results.json` |
| compile speed | `compile-bench/results.json` |

## Freshness is part of the page

Each section states the date it was measured, taken from a `measured` field in
the JSON where the harness records one and the file's mtime otherwise. A
section more than two days behind the newest run is **marked stale** on the
page rather than blending in, and a harness that has never run leaves its
section saying so instead of showing nothing.

That means refreshing one benchmark and regenerating is a normal thing to do —
the untouched sections keep their own dates and admit their age.

## Refreshing

```
make bench          # every harness, then the report
make report         # just rebuild the page from existing results
make runtime        # one harness at a time
make compute
make compile
```

`make bench` re-runs only what is out of date, so touching one benchmark's
sources rebuilds that one and leaves the rest alone — the report then shows
each section's own date and flags the ones behind. A harness that fails does
not stop the others: its `results.json` keeps the last good numbers, the page
marks that section stale, and `make` still exits non-zero so the failure is not
quiet.

Then publish `bench-report/report.html`. Re-publishing to the same URL keeps
the artifact's history.
