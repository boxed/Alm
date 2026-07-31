// Hot reload into a page alm does not serve. See README.md.
//
// Two origins on purpose: alm on one port, a static server standing in for the
// surrounding app on another. Same-origin would pass while the cross-origin
// case — the real one — was broken.
import { spawn } from 'node:child_process';
import fs from 'node:fs';
import http from 'node:http';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import puppeteer from '../../../dom-bench/node_modules/puppeteer-core/lib/puppeteer/puppeteer-core.js';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const ALM = process.env.ALM || path.join(HERE, '../../../target/debug/alm');
const CHROME = process.env.CHROME
  || '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
const ALM_PORT = 8791;
const APP_PORT = 8792;

const SERVED_PORT = 8793;

const WORK = path.join(HERE, 'work');
const SRC = path.join(WORK, 'src/Main.elm');
// alm's own served page starts the program with no options at all, so it cannot
// run one that takes flags. Same code, second module.
const SERVED_SRC = path.join(WORK, 'src/Served.elm');

// A counter, so a swap has state worth preserving. `view` and `Model` are edited
// separately below, which is the whole point: one must keep the count, the other
// must not.
const program = ({ label, model, module: mod = 'Main', flags = true }) =>
  `module ${mod} exposing (main)

import Browser
import Html exposing (Html, button, div, text)
import Html.Events exposing (onClick)


type alias Flags =
    ${flags ? '{ greeting : String }' : '()'}


type alias Model =
    ${model}


init : Flags -> ( Model, Cmd Msg )
init flags =
    ( ${model === '{ count : Int, greeting : String }'
        ? `{ count = 0, greeting = ${flags ? 'flags.greeting' : '"served"'} }`
        : `{ count = 0, greeting = ${flags ? 'flags.greeting' : '"served"'}, extra = "" }`}
    , Cmd.none
    )


type Msg
    = Bump


update : Msg -> Model -> ( Model, Cmd Msg )
update _ model =
    ( { model | count = model.count + 1 }, Cmd.none )


view : Model -> Html Msg
view model =
    div []
        [ div [] [ text (model.greeting ++ " ${label} " ++ String.fromInt model.count) ]
        , button [ onClick Bump ] [ text "bump" ]
        ]


main : Program Flags Model Msg
main =
    Browser.element
        { init = init, update = update, view = view, subscriptions = always Sub.none }
`;

const COUNTER = { label: 'first', model: '{ count : Int, greeting : String }' };
// Written into the Model, so it is visible in the view: if a swap loses the
// flags, the program cannot even start.
const GREETING = 'hello';

fs.rmSync(WORK, { recursive: true, force: true });
fs.mkdirSync(path.join(WORK, 'src'), { recursive: true });
fs.mkdirSync(path.join(WORK, 'static'), { recursive: true });
fs.writeFileSync(
  path.join(WORK, 'elm.json'),
  JSON.stringify({
    type: 'application',
    'source-directories': ['src'],
    'elm-version': '0.19.1',
    dependencies: {
      direct: {
        'elm/browser': '1.0.2',
        'elm/core': '1.0.5',
        'elm/html': '1.0.0',
      },
      indirect: { 'elm/json': '1.1.3', 'elm/time': '1.0.0', 'elm/url': '1.0.0', 'elm/virtual-dom': '1.0.3' },
    },
    'test-dependencies': { direct: {}, indirect: {} },
  }, null, 4),
);
fs.writeFileSync(SRC, program(COUNTER));
fs.writeFileSync(SERVED_SRC, program({ ...COUNTER, module: 'Served', flags: false }));

// The page the surrounding app would serve: it loads the bundle and starts the
// program. Nothing about alm in it.
// `#host` is a stable handle for the harness: mounting *replaces* the node it is
// given with the program's own root, so `#widget` does not outlive init.
const PAGE = `<!DOCTYPE html>
<html><head><meta charset="utf-8"><title>embedded</title></head>
<body>
<h1>A larger app</h1>
<div id="host"><div id="widget"></div></div>
<script>
  var n = Number(sessionStorage.getItem('loads') || 0) + 1;
  sessionStorage.setItem('loads', n);
  window.__navigations = n;
</script>
<script src="/static/app.js"></script>
<script>Elm.Main.init({ node: document.getElementById('widget'), flags: { greeting: 'hello' } });</script>
</body></html>
`;

const failures = [];
const check = (what, ok, detail) => {
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${what}${detail ? ` — ${detail}` : ''}`);
  if (!ok) failures.push(what);
};

const appServer = http.createServer((req, res) => {
  const url = req.url.split('?')[0];
  if (url === '/' || url === '/index.html') {
    res.writeHead(200, { 'Content-Type': 'text/html' });
    res.end(PAGE);
    return;
  }
  // Chrome asks unprompted, and a 404 for it would be the one thing in the
  // console — leaving nothing for a real error to stand out against.
  if (url === '/favicon.ico') {
    res.writeHead(204);
    res.end();
    return;
  }
  // Served from disk, so a rebuild is picked up by a reload — and never cached,
  // so a reload really does get the new one.
  const file = path.join(WORK, url);
  if (!file.startsWith(WORK)) {
    res.writeHead(403);
    res.end();
    return;
  }
  fs.readFile(file, (err, data) => {
    if (err) {
      res.writeHead(404);
      res.end();
      return;
    }
    res.writeHead(200, { 'Content-Type': 'text/javascript', 'Cache-Control': 'no-store' });
    res.end(data);
  });
});

const alm = spawn(
  ALM,
  ['make', 'src/Main.elm', '--live', `--port=${ALM_PORT}`, '--output=static/app.js'],
  { cwd: WORK, stdio: ['ignore', 'pipe', 'pipe'] },
);
const served = spawn(
  ALM,
  ['make', 'src/Served.elm', '--live', `--port=${SERVED_PORT}`],
  { cwd: WORK, stdio: ['ignore', 'pipe', 'pipe'] },
);
let almLog = '';
for (const [who, proc] of [['embedded', alm], ['served', served]]) {
  for (const stream of [proc.stdout, proc.stderr]) {
    stream.on('data', (d) => { almLog += `${who}: ${d}`; });
  }
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/// Poll until `read()` gives something `ok` likes, or give up.
async function until(read, ok, ms = 20000) {
  const deadline = Date.now() + ms;
  for (;;) {
    const value = await read();
    if (ok(value)) return value;
    if (Date.now() > deadline) return value;
    await sleep(100);
  }
}

let browser;
try {
  await new Promise((r) => appServer.listen(APP_PORT, r));

  // The bundle has to exist before the page can load it.
  const written = await until(
    () => fs.existsSync(path.join(WORK, 'static/app.js'))
      && fs.readFileSync(path.join(WORK, 'static/app.js'), 'utf8'),
    (text) => text && text.includes('_Platform_export'),
  );
  if (!written) {
    console.log(`FAIL  alm never wrote the bundle\n${almLog}`);
    process.exit(1);
  }
  check(
    'the written bundle carries the live-reload client',
    written.includes(`http://127.0.0.1:${ALM_PORT}/_alm/live`),
  );

  browser = await puppeteer.launch({
    executablePath: CHROME,
    headless: 'new',
    args: ['--no-sandbox', '--disable-gpu'],
  });
  const page = await browser.newPage();
  const problems = [];
  // Chrome asks both servers for a favicon unprompted and neither has one; that
  // 404 is the only resource failure either page is expected to see, and the
  // console line for it does not name the URL, so it is filtered by request.
  const watch = (target, who) => {
    target.on('console', (m) => {
      const text = m.text();
      if (m.type() !== 'error' && m.type() !== 'warning') return;
      if (/Failed to load resource/.test(text)) return;
      problems.push(`${who}: ${text}`);
    });
    target.on('pageerror', (e) => problems.push(`${who}: ${e}`));
    target.on('requestfailed', (r) => problems.push(`${who}: request failed ${r.url()}`));
    target.on('response', (r) => {
      if (r.status() >= 400 && !r.url().endsWith('/favicon.ico')) {
        problems.push(`${who}: ${r.status()} for ${r.url()}`);
      }
    });
  };
  watch(page, 'embedding page');

  await page.goto(`http://localhost:${APP_PORT}/`, { waitUntil: 'load' });
  const widget = () => page.$eval('#host', (el) => el.textContent);

  await page.waitForFunction(() => document.querySelector('#host button'), { timeout: 15000 });
  check('the program starts in the embedding page', (await widget()).includes('first 0'));
  // The greeting comes from the flags the page passed, so seeing it means the
  // flags arrived — and every later check that it is still there means a swap
  // did not drop them.
  check('the page\'s flags reach the program', (await widget()).includes(GREETING));

  // Count to three, so there is a model worth carrying.
  for (let i = 0; i < 3; i++) await page.click('#host button');
  check('the counter runs', (await widget()).includes('first 3'), await widget());

  // 1 + 2: change the view only. The new view must arrive, and the count with it.
  fs.writeFileSync(SRC, program({ ...COUNTER, label: 'second' }));
  const swapped = await until(widget, (t) => t.includes('second'));
  check('the swap reaches a page alm did not serve', swapped.includes('second'), swapped);
  check(
    'the model survives a cross-origin swap',
    swapped.includes('second 3'),
    `${swapped} (a reset to 0 means the Model fingerprint did not survive CORS)`,
  );
  check(
    'the flags survive a swap',
    swapped.includes(GREETING),
    `${swapped} (a swap re-initializes the program, so it must pass the flags again)`,
  );
  // A reload masks almost any swap failure: the client falls back to one, and the
  // page's own script re-runs with the real flags, so the result looks right.
  // Counting navigations is what tells a swap from a reload.
  const navigations = () => page.evaluate(() => window.__navigations);
  check('the page was swapped, not reloaded', (await navigations()) === 1, `${await navigations()} loads`);

  // 3: change the Model. Carrying the old one across would be unsound, so this
  // one has to start fresh.
  fs.writeFileSync(SRC, program({ label: 'third', model: '{ count : Int, greeting : String, extra : String }' }));
  const reset = await until(widget, (t) => t.includes('third'));
  check('a changed Model still swaps', reset.includes('third'), reset);
  check('a changed Model starts fresh', reset.includes('third 0'), reset);
  check('and did so by a swap, not a reload', (await navigations()) === 1, `${await navigations()} loads`);
  // Even here the flags have to be passed again: it is a fresh init, and this is
  // the swap that does *not* reuse the previous options wholesale.
  check('and still gets the flags', reset.includes(GREETING), reset);

  // The page alm serves itself goes through the same registry, so it has to keep
  // working — same swap, same model preservation, relative URLs.
  const own = await browser.newPage();
  watch(own, 'served page');
  await own.goto(`http://localhost:${SERVED_PORT}/`, { waitUntil: 'load' });
  await own.waitForFunction(() => document.querySelector('button'), { timeout: 15000 });
  // The program's own root, not `body`: that page carries the bundle inline, and
  // `body.textContent` would hand back the whole script source with it.
  const text = () => own.$eval('body > div', (el) => el.textContent);
  for (let i = 0; i < 2; i++) await own.click('button');
  check('alm\'s own page still runs the program', (await text()).includes('first 2'), await text());

  fs.writeFileSync(
    SERVED_SRC,
    program({ ...COUNTER, label: 'later', module: 'Served', flags: false }),
  );
  const ownSwapped = await until(text, (t) => t.includes('later'));
  check('alm\'s own page still swaps', ownSwapped.includes('later'), ownSwapped);
  check('and still keeps the model', ownSwapped.includes('later 2'), ownSwapped);
  await own.close();

  const unexpected = problems.filter((p) => !/Model changed, started fresh/.test(p));
  check('no errors in the console', unexpected.length === 0, unexpected.join(' | '));
} finally {
  if (browser) await browser.close();
  alm.kill();
  served.kill();
  appServer.close();
}

if (failures.length) {
  console.log(`\n${failures.length} failed: ${failures.join(', ')}\n\nalm said:\n${almLog}`);
  process.exit(1);
}
console.log('\nAll checks passed.');
