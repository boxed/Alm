# dom-bench — real-browser DOM benchmark

A keyed [js-framework-benchmark](https://github.com/krausest/js-framework-benchmark)-style
app (create / append / update / swap / select / remove / clear rows) implemented
five ways and timed in a real headless Chrome:

- **elm** — official 0.19.1 compiler
- **alm-js** — `alm make --target=js`
- **alm-wasm** — `alm make --target=wasm-gc`, run through the DOM-host JS shim (`shim.js`)
- **react** — React 18, keyed, `createRoot`
- **svelte** — Svelte 4, keyed `#each`

`elm`, `alm-js` and `alm-wasm` all compile the **same** `Main.elm`.

## Run

```sh
npm install                     # esbuild, react, svelte, puppeteer-core
./build.sh                      # builds all 5 into build/  (ALM=../target/release/alm)
node drive.mjs                  # runs them in system Chrome, prints build/results.json
```

On non-macOS set the Chrome path: `CHROME=/usr/bin/google-chrome node drive.mjs`.
Runs per page default to 3 (`REPEATS=5 node drive.mjs` for more).

## Metric

Each operation is **paint-inclusive**: timed from the click to a task scheduled
after the next frame's paint (`requestAnimationFrame` → `setTimeout(0)`), so it
counts the framework's work *and* the browser's layout/paint. This is fair to
frameworks that batch renders on `requestAnimationFrame` (elm) and those that
commit synchronously (alm/React) alike — both paint on the same frame. As a
result sub-frame incremental ops (select, swap, remove) converge near one frame
and don't separate the field; the bulk ops (create, append), whose work exceeds
a frame, differentiate. The runner (`runner.js`) reports the median of 15 timed
iterations per op; `drive.mjs` reports the median across `REPEATS` page reloads.

`--virtual-time-budget` is deliberately **not** used — it virtualizes
`performance.now()` and destroys the timing.

## Files

- `Main.elm` — the shared Elm app (elm / alm-js / alm-wasm)
- `App.jsx`, `App.svelte` — React & Svelte apps (same operations, same DOM shape)
- `shim.js` — browser DOM-host shim for the wasm-gc module (mirrors the node test
  driver in `crates/compiler/tests/browser_support/wasmgc_driver.cjs`)
- `runner.js` — shared in-page paint-inclusive timing harness
- `build.mjs` — builds the React & Svelte bundles; `build.sh` builds all five
- `drive.mjs` — puppeteer-core driver over system Chrome
