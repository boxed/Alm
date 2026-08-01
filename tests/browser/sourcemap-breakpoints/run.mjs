// Are the source maps good enough to *debug* with, not merely valid?
//
// A map can be perfectly well-formed and still useless: if the generated line
// numbers are off by even one, Chrome resolves a click in the Elm source to a
// position it cannot place a breakpoint at, and silently drops it. The map
// validates, the sources display, and breakpoints just never appear.
//
// So this measures the thing that actually matters — how many Elm lines a
// breakpoint can be *bound* on — and requires the `--live --output` bundle to be
// no worse than a plain `alm make --source-maps`, which is the baseline.
import { spawn, spawnSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { bindable } from './probe.mjs';
import { scopeAt } from './scopes.mjs';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const ALM = process.env.ALM || path.join(HERE, '../../../target/debug/alm');
const PORT = 8796;
const WORK = path.join(HERE, 'work');

const source = `module Main exposing (main)

import Browser
import Html exposing (Html, button, div, text)
import Html.Events exposing (onClick)


type alias Model =
    { count : Int }


init : Model
init =
    { count = 0 }


type Msg
    = Bump
    | Reset


update : Msg -> Model -> Model
update msg model =
    case msg of
        Bump ->
            { model | count = model.count + 1 }

        Reset ->
            { model | count = 0 }


label : Model -> String
label model =
    if model.count == 0 then
        "nothing yet"

    else
        String.fromInt model.count


view : Model -> Html Msg
view model =
    div []
        [ div [] [ text (label model) ]
        , button [ onClick Bump ] [ text "bump" ]
        , button [ onClick Reset ] [ text "reset" ]
        ]


main : Program () Model Msg
main =
    Browser.sandbox { init = init, update = update, view = view }


type Shape
    = Circle Float
    | Rect Float Float
    | Tri Float Float Float


area : Shape -> Float
area shape =
    case shape of
        Circle radius ->
            pi * radius * radius

        Rect width height ->
            width * height

        Tri sideA sideB sideC ->
            let
                half =
                    (sideA + sideB + sideC) / 2
            in
            sqrt (half * (half - sideA))


measured : Float
measured =
    area (Rect 2 3)
`;

fs.rmSync(WORK, { recursive: true, force: true });
fs.mkdirSync(path.join(WORK, 'src'), { recursive: true });
fs.writeFileSync(
  path.join(WORK, 'elm.json'),
  JSON.stringify({
    type: 'application',
    'source-directories': ['src'],
    'elm-version': '0.19.1',
    dependencies: {
      direct: { 'elm/browser': '1.0.2', 'elm/core': '1.0.5', 'elm/html': '1.0.0' },
      indirect: {
        'elm/json': '1.1.3', 'elm/time': '1.0.0', 'elm/url': '1.0.0',
        'elm/virtual-dom': '1.0.3',
      },
    },
    'test-dependencies': { direct: {}, indirect: {} },
  }, null, 4),
);
fs.writeFileSync(path.join(WORK, 'src/Main.elm'), source);

const failures = [];
const check = (what, ok, detail) => {
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${what}${detail ? ` — ${detail}` : ''}`);
  if (!ok) failures.push(what);
};

// Baseline: an ordinary build with a map.
const plain = spawnSync(ALM, ['make', 'src/Main.elm', '--output=plain.js', '--source-maps'], {
  cwd: WORK, encoding: 'utf8',
});
if (plain.status !== 0) {
  console.log(`FAIL  plain build\n${plain.stdout}${plain.stderr}`);
  process.exit(1);
}

// The bundle written for embedding, which is where a prepended line would break
// every mapping at once.
const live = spawn(
  ALM,
  ['make', 'src/Main.elm', '--live', `--port=${PORT}`, '--output=live.js', '--source-maps'],
  { cwd: WORK, stdio: ['ignore', 'pipe', 'pipe'] },
);
let liveLog = '';
live.stdout.on('data', (d) => { liveLog += d; });
live.stderr.on('data', (d) => { liveLog += d; });

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
try {
  for (let i = 0; i < 400; i++) {
    if (fs.existsSync(path.join(WORK, 'live.js.map'))) break;
    await sleep(25);
  }
  if (!fs.existsSync(path.join(WORK, 'live.js.map'))) {
    console.log(`FAIL  --live never wrote the bundle\n${liveLog}`);
    process.exit(1);
  }

  const baseline = await bindable(WORK, 'plain.js', 'Main.elm');
  const written = await bindable(WORK, 'live.js', 'Main.elm');
  const show = (r) => `${r.bound}/${r.total} lines bindable`;

  // Not "all lines": some Elm lines have no breakable position in any compiler's
  // output — a record field whose value is a constant, say. Chrome refuses those
  // in hand-written JavaScript too.
  check('a plain build is debuggable', baseline.bound / baseline.total > 0.6, show(baseline));
  check(
    'the --live --output bundle is no worse',
    written.bound >= baseline.bound,
    `${show(written)} vs a baseline of ${show(baseline)}`,
  );
  if (written.bound < baseline.bound) {
    console.log('\n  Lines the written bundle loses:');
    for (const line of baseline.lines) {
      if (!written.lines.includes(line)) console.log(`    Main.elm:${line}`);
    }
    console.log('\n  A whole-file loss like this is usually a line added above the'
      + '\n  program: the map addresses generated lines, so everything shifts.');
  }
  // Pausing in the `Rect` branch must not show the other branches' locals.
  // Everything used to be a function-scoped `var`, so all of them were listed.
  const { names, functionName } = await scopeAt(WORK, 'plain.js', 'return (width * height)');
  const others = ['radius', 'sideA', 'sideB', 'sideC', 'half'];
  const leaked = others.filter((n) => names.includes(n));
  check(
    'the branch you stopped in is the only one in scope',
    leaked.length === 0,
    leaked.length ? `also visible: ${leaked.join(', ')}` : `in scope: ${names.join(', ')}`,
  );
  check(
    "and that branch's own bindings are there",
    names.includes('width') && names.includes('height'),
    names.join(', '),
  );
  // The map names the function at its definition, which is what a call stack
  // shows for the frame.
  check('the frame is named', functionName.length > 0, functionName);
} finally {
  live.kill();
}

if (failures.length) {
  console.log(`\n${failures.length} failed: ${failures.join(', ')}`);
  process.exit(1);
}
console.log('\nAll checks passed.');
