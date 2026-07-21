// Real-browser DOM benchmark driver. Serves each framework's page to system
// Chrome (via puppeteer-core, REAL time — never --virtual-time-budget, which
// virtualizes performance.now() and corrupts timing) and reads the paint-
// inclusive medians the in-page runner (runner.js) computed. Each page is run
// REPEATS times; the reported value per op is the median across those runs.
//
// Chrome path: set CHROME env, else the macOS default below.
import fs from 'node:fs';
import path from 'node:path';
import puppeteer from 'puppeteer-core';

const J = path.dirname(new URL(import.meta.url).pathname);
const CHROME = process.env.CHROME || '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
const REPEATS = Number(process.env.REPEATS || 3);
const rd = (f) => fs.readFileSync(path.join(J, f), 'utf8');
const runner = rd('runner.js');

function page(headScripts, bodyPre, bootstrap) {
  return `<!DOCTYPE html><html><head><meta charset="utf-8"><title>run</title>
${headScripts}</head><body>${bodyPre}
<script>${runner}</script>
<script>try{${bootstrap}\nsetTimeout(runBench,150);}catch(e){const p=document.createElement('pre');p.id='results';p.textContent='ERROR '+(e&&e.stack||e);document.body.appendChild(p);document.title='DONE';}</script>
</body></html>`;
}

const wasmB64 = fs.readFileSync(path.join(J, 'build', 'almwasm.wasm')).toString('base64');
const rdb = (f) => fs.readFileSync(path.join(J, 'build', f), 'utf8');
const configs = {
  elm: page(`<script>${rdb('elm.js')}</script>`, `<div id="app"></div>`,
    `Elm.Main.init({node:document.getElementById('app')});`),
  'alm-js': page(`<script>${rdb('almjs.js')}</script>`, `<div id="app"></div>`,
    `var M=(Elm.Main.main&&Elm.Main.main.init)?Elm.Main.main:Elm.Main;M.init({node:document.getElementById('app')});`),
  'alm-wasm': page(`<script>${rd('shim.js')}</script>`, ``,
    `window.almStart(Uint8Array.from(atob(${JSON.stringify(wasmB64)}),c=>c.charCodeAt(0)));`),
  react: page(``, `<div id="app"></div><script>${rdb('react.bundle.js')}</script>`, ``),
  svelte: page(``, `<div id="app"></div><script>${rdb('svelte.bundle.js')}</script>`, ``),
  // Optimized (Html.Lazy) alm variants — Main_lazy.elm, so the JS entry is
  // Elm.Main_lazy; the wasm module is module-agnostic (almStart).
  'alm-js-opt': page(`<script>${rdb('almjs_lazy.js')}</script>`, `<div id="app"></div>`,
    `var M=(Elm.Main_lazy.main&&Elm.Main_lazy.main.init)?Elm.Main_lazy.main:Elm.Main_lazy;M.init({node:document.getElementById('app')});`),
  'alm-wasm-opt': page(`<script>${rd('shim.js')}</script>`, ``,
    `window.almStart(Uint8Array.from(atob(${JSON.stringify(fs.readFileSync(path.join(J, 'build', 'almwasm_lazy.wasm')).toString('base64'))}),c=>c.charCodeAt(0)));`),
};

const browser = await puppeteer.launch({ executablePath: CHROME, headless: 'new', args: ['--no-sandbox', '--disable-gpu'] });
const results = {};
const outDir = path.join(J, 'build');
for (const [name, html] of Object.entries(configs)) {
  const f = path.join(outDir, `page_${name}.html`);
  fs.writeFileSync(f, html);
  const runs = [];
  for (let rep = 0; rep < REPEATS; rep++) {
    const pg = await browser.newPage();
    const errs = [];
    pg.on('pageerror', (e) => errs.push(String(e)));
    await pg.goto('file://' + f, { waitUntil: 'load' });
    try {
      await pg.waitForFunction(() => document.title === 'DONE', { timeout: 180000 });
      const txt = await pg.$eval('#results', (el) => el.textContent);
      if (txt.startsWith('ERROR')) { results[name] = { error: txt }; await pg.close(); break; }
      runs.push(JSON.parse(txt));
    } catch (e) {
      results[name] = { error: String(e) + ' :: ' + errs.join(' | ') };
      await pg.close();
      break;
    }
    await pg.close();
    console.error(`done ${name} rep${rep}`);
  }
  if (runs.length) {
    const agg = {};
    for (const k of Object.keys(runs[0])) {
      const v = runs.map((r) => r[k]).sort((a, b) => a - b);
      agg[k] = Math.round(v[v.length >> 1] * 10) / 10;
    }
    results[name] = agg;
  }
}
await browser.close();
fs.writeFileSync(path.join(outDir, 'results.json'), JSON.stringify(results, null, 2));
console.log(JSON.stringify(results, null, 2));
