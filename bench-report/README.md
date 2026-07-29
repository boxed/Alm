# bench-report

Builds the published benchmark page from the harnesses' results.

```
make report          # rebuild the page
make bench           # re-run every harness, then rebuild
```

## The page is generated, all of it

`template.html` is CSS and a renderer; its body is one empty container.
`generate.py` builds every section — titles, column headers, figures, dates —
from the `results.json` each harness writes:

| section | source |
|---|---|
| runtime | `dom-bench/build/results.json` |
| computation | `compute-bench/results.json` |
| compile speed | `compile-bench/results.json` |

**No copy is authored into the page.** This is checked, not just asked for:
`generate.py` refuses to build if the template's body contains any text. The
rule exists because the alternative was tried — hand-written methodology prose
drifted out of step with the numbers on every re-run, until the page was
describing measurements it no longer showed, with the numbers quoted inline
and wrong.

If something needs saying, the harness that knows it should record it in its
results, and the renderer can show it from there.

## Freshness is part of the output

Each section shows the date it was measured — from a `measured` field where
the harness records one, the file's mtime otherwise. A section more than two
days behind the newest run is marked **stale** on the page; a harness that has
never run leaves its section saying so.

So refreshing one benchmark and regenerating is normal: the untouched sections
keep their own dates and admit their age.

## Publishing

Publish `bench-report/report.html`. Re-publishing to the same URL keeps the
artifact's history.
