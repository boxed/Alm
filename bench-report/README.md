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

Use each harness's `build.sh`, not `npm run build`. `dom-bench/build.mjs` builds
only the React and Svelte bundles — `build.sh` is what compiles the elm and alm
ones. Running the driver against stale bundles produces pages that never mount,
and every operation then measures an empty frame: a suspiciously constant
~8.3 ms per row on a 120 Hz display.

```
cd dom-bench && ./build.sh && node drive.mjs        # runtime / memory / size
cd compute-bench && ./build.sh && node run.mjs      # computation
python3 compile-bench/run.py                        # compile speed
python3 bench-report/generate.py                    # rebuild the page
```

Then publish `bench-report/report.html`. Re-publishing to the same URL keeps
the artifact's history.
