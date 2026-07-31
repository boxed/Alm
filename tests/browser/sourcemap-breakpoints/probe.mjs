// Ask a real Chrome where it can actually put a breakpoint.
//
// This reproduces what DevTools does when you click a line number in a mapped
// source:
//   1. reverse-map (elm file, line) -> a generated (line, column)
//   2. Debugger.setBreakpointByUrl at that generated position
//   3. keep the breakpoint only if the position V8 *chose* maps back to the line
//      that was clicked
//
// Step 3 is the one that fails silently. V8 can only stop at a breakable
// position, so it moves a request forward to the next one; if the map is offset,
// or a whole Elm definition sits on a single enormous generated line, the
// position it lands on belongs to some other Elm line and DevTools throws the
// breakpoint away. Clicking then does nothing at all, which is what this is here
// to catch.
import fs from 'node:fs';
import http from 'node:http';
import path from 'node:path';
import puppeteer from '../../../dom-bench/node_modules/puppeteer-core/lib/puppeteer/puppeteer-core.js';

const CHROME = process.env.CHROME
  || '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';

const B = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';
const IDX = Object.fromEntries([...B].map((c, i) => [c, i]));
const decodeVlq = (seg) => {
  const out = []; let v = 0, sh = 0;
  for (const ch of seg) {
    const d = IDX[ch]; v |= (d & 31) << sh;
    if (d & 32) sh += 5;
    else { out.push(v & 1 ? -(v >> 1) : v >> 1); v = 0; sh = 0; }
  }
  return out;
};

/** Every mapping in a v3 map, as flat records with 0-based lines. */
export function parseMappings(map) {
  const all = [];
  let si = 0, sl = 0, sc = 0;
  map.mappings.split(';').forEach((line, genLine) => {
    if (!line) return;
    let genCol = 0;
    for (const seg of line.split(',')) {
      const f = decodeVlq(seg);
      genCol += f[0];
      if (f.length >= 4) { si += f[1]; sl += f[2]; sc += f[3]; }
      all.push({ genLine, genCol, file: map.sources[si], srcLine: sl, srcCol: sc });
    }
  });
  return all;
}

/**
 * Which lines of `only` a breakpoint can be bound on in `js` (served from `dir`,
 * with `js`.map beside it). `sample` caps how many lines are tried — evenly
 * spread, since the first N are dominated by whichever declaration comes first.
 */
export async function bindable(dir, js, only, sample = 120) {
  const map = JSON.parse(fs.readFileSync(path.join(dir, `${js}.map`), 'utf8'));
  const mappings = parseMappings(map);
  const port = 8700 + (process.pid % 90);

  const server = http.createServer((req, res) => {
    const url = req.url.split('?')[0];
    if (url === '/') {
      res.writeHead(200, { 'Content-Type': 'text/html' });
      res.end(`<!DOCTYPE html><html><body><div id="m"></div>
        <script src="/${js}"></script></body></html>`);
      return;
    }
    const file = path.join(dir, url);
    if (!file.startsWith(dir) || !fs.existsSync(file)) { res.writeHead(404); res.end(); return; }
    res.writeHead(200, {
      'Content-Type': url.endsWith('.map') ? 'application/json' : 'text/javascript',
    });
    res.end(fs.readFileSync(file));
  });
  await new Promise((r) => server.listen(port, r));

  const browser = await puppeteer.launch({
    executablePath: CHROME, headless: 'new', args: ['--no-sandbox', '--disable-gpu'],
  });
  try {
    const page = await browser.newPage();
    const cdp = await page.createCDPSession();
    await cdp.send('Debugger.enable');
    await page.goto(`http://localhost:${port}/`, { waitUntil: 'load' });
    const url = `http://localhost:${port}/${js}`;

    // The first mapping on each Elm line is the one a click on it resolves to.
    const first = new Map();
    for (const m of mappings) {
      const key = `${m.file}:${m.srcLine}`;
      if (!first.has(key)) first.set(key, m);
    }
    let candidates = [...first.values()];
    if (only) candidates = candidates.filter((m) => path.basename(m.file) === only);
    const stride = Math.max(1, Math.floor(candidates.length / sample));
    const wanted = candidates.filter((_, i) => i % stride === 0).slice(0, sample);

    const lines = [];
    const missed = [];
    for (const m of wanted) {
      const { breakpointId, locations } = await cdp.send('Debugger.setBreakpointByUrl', {
        url, lineNumber: m.genLine, columnNumber: m.genCol,
      });
      if (!locations.length) {
        missed.push({ line: m.srcLine + 1, landedOn: null });
        continue;
      }
      // Where did V8 actually put it, and does that map back here?
      const at = locations[0];
      let best = null;
      for (const c of mappings) {
        if (c.genLine !== at.lineNumber) continue;
        if (c.genCol <= at.columnNumber && (!best || c.genCol > best.genCol)) best = c;
      }
      if (best && best.file === m.file && best.srcLine === m.srcLine) lines.push(m.srcLine + 1);
      else missed.push({ line: m.srcLine + 1, landedOn: best && best.srcLine + 1 });
      await cdp.send('Debugger.removeBreakpoint', { breakpointId }).catch(() => {});
    }
    return { bound: lines.length, total: wanted.length, lines, missed };
  } finally {
    await browser.close();
    server.close();
  }
}

// Also runnable directly, to point at any built bundle:
//   node probe.mjs <dir> <bundle.js> [Module.elm]
if (process.argv[1] === fs.realpathSync(new URL(import.meta.url).pathname)) {
  const [dir, js, only] = process.argv.slice(2);
  if (!dir || !js) {
    console.log('usage: node probe.mjs <dir> <bundle.js> [Module.elm]');
    process.exit(2);
  }
  const r = await bindable(path.resolve(dir), js, only);
  for (const m of r.missed) {
    console.log(`  no breakpoint on line ${m.line}`
      + (m.landedOn ? ` (V8 landed on line ${m.landedOn})` : ' (V8 bound nothing)'));
  }
  console.log(`${r.bound}/${r.total} sampled lines are bindable`);
}
