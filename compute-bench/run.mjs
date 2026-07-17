// Compute-benchmark harness. For each workload:
//   * alm (JS)      — call the exported `bench(size)` in-process, JIT-warmed
//   * alm (wasm-gc) — call `main_int()` on the instantiated module, warmed
//   * alm (native)  — spawn the AOT binary (min wall-time), then subtract the
//                     measured startup floor (a noop binary) so the figure is
//                     COMPUTE only — matching how JS/wasm are timed (warm,
//                     in-process, no process startup).
// All three run the SAME Main.elm. Correctness is checked (all agree) before
// timing. Timing = median of TIMED runs after WARMUP runs.
import fs from "node:fs";
import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";
import path from "node:path";

const require = createRequire(import.meta.url);
const dir = path.dirname(fileURLToPath(import.meta.url));
const build = path.join(dir, "build");

const WORKLOADS = [
  { name: "fib 35",              module: "Fib",          size: 35 },
  { name: "binary-trees d14×60", module: "BinaryTrees",  size: 60 },
  { name: "list map/filter/fold 1M", module: "ListPipeline", size: 1000000 },
  { name: "Dict 100k build+get", module: "DictOps",      size: 100000 },
  { name: "mandelbrot 400²",     module: "Mandelbrot",   size: 400 },
  { name: "sort 100k ints",      module: "Sort",         size: 100000 },
];

const WARMUP = 10;
const TIMED = 25;
const NATIVE_RUNS = 12;

// Stub the env imports (pure compute touches none except Math.*).
const env = new Proxy({}, {
  get: (_, n) => {
    const s = String(n);
    if (s.startsWith("math_")) return Math[s.slice(5)] || (() => 0);
    return () => 0;
  },
});

function median(xs) {
  const s = [...xs].sort((a, b) => a - b);
  const m = s.length >> 1;
  return s.length % 2 ? s[m] : (s[m - 1] + s[m]) / 2;
}

async function loadWasm(file) {
  const { instance } = await WebAssembly.instantiate(fs.readFileSync(file), { env });
  return instance.exports.main_int;
}

function timeCalls(fn, warmup, timed) {
  let sink = 0;
  for (let i = 0; i < warmup; i++) sink += Number(fn());
  const ts = [];
  for (let i = 0; i < timed; i++) {
    const t = process.hrtime.bigint();
    sink += Number(fn());
    ts.push(Number(process.hrtime.bigint() - t) / 1e6);
  }
  if (sink === -1) console.log("");
  return median(ts);
}

function timeNative(bin, runs) {
  let best = Infinity, out = null;
  for (let i = 0; i < runs; i++) {
    const t = process.hrtime.bigint();
    out = execFileSync(bin).toString().trim();
    best = Math.min(best, Number(process.hrtime.bigint() - t) / 1e6);
  }
  return { ms: best, out };
}

// Startup floor: wall-time of a no-work native binary (dyld + GC init +
// worker-thread spawn). Subtracted from each workload's native time so the
// reported figure is compute only, comparable to the warm in-process JS/wasm
// numbers. Roughly constant across these similarly-sized binaries.
const startupFloor = timeNative(path.join(build, "Noop.native"), NATIVE_RUNS).ms;

const results = [];
for (const w of WORKLOADS) {
  const js = require(path.join(build, w.module + ".js"))[w.module];
  const jsBench = () => js.bench(w.size);
  const wasmMain = await loadWasm(path.join(build, w.module + ".wasm"));
  const nativeBin = path.join(build, w.module + ".native");

  // correctness
  const jsVal = BigInt(jsBench());
  const wasmVal = BigInt(wasmMain());
  const native = timeNative(nativeBin, NATIVE_RUNS);
  const natVal = BigInt(native.out);
  if (jsVal !== wasmVal || jsVal !== natVal) {
    console.error(`DISAGREE ${w.name}: js=${jsVal} wasm=${wasmVal} native=${natVal}`);
    process.exit(1);
  }

  const jsMs = timeCalls(jsBench, WARMUP, TIMED);
  const wasmMs = timeCalls(wasmMain, WARMUP, TIMED);
  // Subtract the fixed process-startup floor: report native COMPUTE only.
  const nativeMs = Math.max(0, native.ms - startupFloor);
  results.push({ name: w.name, js: jsMs, wasm: wasmMs, native: nativeMs });
}

const pad = (s, n) => String(s).padEnd(n);
const num = (x) => x.toFixed(2).padStart(9);
const col = (s) => String(s).padStart(9);
console.log("\n" + pad("workload", 26) + col("alm-js") + col("wasm-gc") + col("native") + "   wasm vs js   native vs js");
console.log("-".repeat(92));
for (const r of results) {
  console.log(
    pad(r.name, 26) + num(r.js) + num(r.wasm) + num(r.native) +
    `   ${(r.js / r.wasm).toFixed(2)}x`.padStart(13) +
    `   ${(r.js / r.native).toFixed(2)}x`.padStart(14)
  );
}
fs.writeFileSync(path.join(dir, "results.json"), JSON.stringify(results, null, 2));
console.log(
  "\nwrote results.json  (ms, lower is better; median of " + TIMED + " timed after " +
  WARMUP + " warmup; native = min of " + NATIVE_RUNS + " minus a " +
  startupFloor.toFixed(2) + "ms startup floor)"
);
