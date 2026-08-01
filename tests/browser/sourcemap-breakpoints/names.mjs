// No variable may be shown under a name that is not its own.
//
// A debugger resolves an identifier's original name through the map entry
// *covering* its position — the nearest at or before it, not an exact match. So a
// `names` field that covers only some identifiers does not merely under-deliver:
// every identifier without an entry of its own inherits whatever name the
// preceding entry carried. Naming references but not declarations showed a
// function's two parameters both under the function's name, and a `let` binding
// under whatever value happened to sit before it.
//
// That is invisible to a test that checks the map is valid, and invisible to the
// debugger protocol too, because the renaming happens in the DevTools frontend.
// So this reproduces the frontend's lookup and checks the answer.
import fs from 'node:fs';
import path from 'node:path';

const B = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';
const IDX = Object.fromEntries([...B].map((c, i) => [c, i]));
const dec = (seg) => {
  const out = []; let v = 0, sh = 0;
  for (const ch of seg) {
    const d = IDX[ch]; v |= (d & 31) << sh;
    if (d & 32) sh += 5;
    else { out.push(v & 1 ? -(v >> 1) : v >> 1); v = 0; sh = 0; }
  }
  return out;
};

/** Mappings grouped by generated line, each with the name it carries (or null). */
function index(map) {
  const byLine = new Map();
  let si = 0, sl = 0, sc = 0, ni = 0;
  map.mappings.split(';').forEach((line, gl) => {
    if (!line) return;
    let gc = 0;
    const segs = [];
    for (const seg of line.split(',')) {
      const f = dec(seg);
      gc += f[0];
      if (f.length >= 4) { si += f[1]; sl += f[2]; sc += f[3]; }
      let name = null;
      if (f.length === 5) { ni += f[4]; name = map.names[ni]; }
      segs.push({ col: gc, name });
    }
    byLine.set(gl, segs);
  });
  return byLine;
}

/**
 * Every declaration in `js` whose resolved name would differ from the identifier
 * actually declared. Empty is the only acceptable answer: a declaration either
 * resolves to its own name, or to no name at all and keeps the generated one.
 *
 * Declarations are found by pattern rather than by parsing — parameters of named
 * function expressions, and `let`/`var` bindings — which is enough, because those
 * are exactly what a debugger lists in a scope.
 */
export function misnamed(dir, js) {
  const map = JSON.parse(fs.readFileSync(path.join(dir, `${js}.map`), 'utf8'));
  if (!map.names || map.names.length === 0) return [];
  const byLine = index(map);
  const covering = (line, col) => {
    let best = null;
    for (const s of byLine.get(line) || []) {
      if (s.col <= col && (!best || s.col > best.col)) best = s;
    }
    return best;
  };

  const bad = [];
  fs.readFileSync(path.join(dir, js), 'utf8').split('\n').forEach((text, line) => {
    const declarations = [];
    for (const m of text.matchAll(/function [\w$]* ?\(([^)]*)\)/g)) {
      const open = m.index + m[0].indexOf('(') + 1;
      let at = open;
      for (const raw of m[1].split(',')) {
        const id = raw.trim();
        if (id) declarations.push({ id, col: text.indexOf(id, at) });
        at += raw.length + 1;
      }
    }
    for (const m of text.matchAll(/\b(?:let|var) ([\w$]+) =/g)) {
      declarations.push({ id: m[1], col: m.index + m[0].indexOf(m[1]) });
    }
    for (const d of declarations) {
      if (d.col < 0) continue;
      const entry = covering(line, d.col);
      if (entry && entry.name && entry.name !== d.id) {
        bad.push({ line: line + 1, declared: d.id, shownAs: entry.name });
      }
    }
  });
  return bad;
}
