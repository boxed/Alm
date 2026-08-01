// What does the debugger's Scope pane actually list when you stop in a branch?
//
// Every binding in a function used to be a `var`, which JavaScript scopes to the
// whole function however deeply nested it is written. So pausing in one branch of
// a `case` listed the locals of *every* branch — in dryft's `Insight.update`, 301
// of them, nearly all `undefined` because their branch never ran, several sharing
// a name. Block-scoping them shows only the ones that are actually live.
//
// This pauses in one branch of a three-branch `case` and reads the scope chain
// over the debugger protocol, which is the same thing the Scope pane renders.
import fs from 'node:fs';
import http from 'node:http';
import path from 'node:path';
import puppeteer from '../../../dom-bench/node_modules/puppeteer-core/lib/puppeteer/puppeteer-core.js';

const CHROME = process.env.CHROME
  || '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';

/**
 * Pause where `marker` appears in `js` and return every variable name the
 * debugger reports in scope, innermost frame only, excluding the globals.
 */
export async function scopeAt(dir, js, marker) {
  const text = fs.readFileSync(path.join(dir, js), 'utf8');
  const at = text.indexOf(marker);
  if (at < 0) throw new Error(`marker ${JSON.stringify(marker)} not found in ${js}`);
  const before = text.slice(0, at);
  const lineNumber = before.split('\n').length - 1;
  const columnNumber = at - (before.lastIndexOf('\n') + 1);
  const port = 8600 + (process.pid % 90);

  const server = http.createServer((req, res) => {
    const url = req.url.split('?')[0];
    if (url === '/') {
      res.writeHead(200, { 'Content-Type': 'text/html' });
      res.end(`<!DOCTYPE html><html><body><script src="/${js}"></script></body></html>`);
      return;
    }
    const file = path.join(dir, url);
    if (!file.startsWith(dir) || !fs.existsSync(file)) { res.writeHead(404); res.end(); return; }
    res.writeHead(200, { 'Content-Type': 'text/javascript' });
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
    const paused = new Promise((resolve) => cdp.once('Debugger.paused', resolve));
    // Set before navigating: the program runs as the script evaluates, so there
    // is no later moment at which to catch it.
    await cdp.send('Debugger.setBreakpointByUrl', {
      url: `http://localhost:${port}/${js}`, lineNumber, columnNumber,
    });
    page.goto(`http://localhost:${port}/`).catch(() => {});
    const event = await Promise.race([
      paused,
      new Promise((_, rej) => setTimeout(() => rej(new Error('never paused')), 20000)),
    ]);

    const frame = event.callFrames[0];
    const names = [];
    for (const scope of frame.scopeChain) {
      // The function's own bindings and the block we stopped in. Not `script`
      // or `global`, which hold every top-level definition in the bundle, and
      // not `closure`, which belongs to the enclosing function rather than to
      // the branch under test.
      if (scope.type !== 'local' && scope.type !== 'block') continue;
      const { result } = await cdp.send('Runtime.getProperties', {
        objectId: scope.object.objectId, ownProperties: true,
      });
      for (const p of result) names.push(p.name);
    }
    if (process.env.SCOPE_DEBUG) {
      console.log('paused at', JSON.stringify(frame.location),
        'requested', lineNumber, columnNumber,
        'scopes', frame.scopeChain.map((s) => s.type).join('+'));
    }
    return { names, functionName: frame.functionName };
  } finally {
    await browser.close();
    server.close();
  }
}
