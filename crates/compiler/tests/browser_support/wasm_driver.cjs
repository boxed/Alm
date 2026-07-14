'use strict';
// Drives a wasm Browser.* module against a DOM stub via the thin JS shim:
// provides the `dom_*` host imports (backed by an integer node-handle table)
// and calls the module's `alm_browser_start` export.

const fs = require('fs');
const { WASI } = require('node:wasi');
const { setProperty } = require('./dom_stub.cjs');
const { makeClock } = require('./clock.cjs');

// Copy the JSON-safe scalar fields of an object (used to serialize a DOM event
// so the wasm-side decoder can read it — target.value, target.checked, keyCode,
// etc.). Only one level deep plus target/currentTarget.
function jsonSafe(v) {
  const t = typeof v;
  return v === null || t === 'string' || t === 'number' || t === 'boolean';
}
function shallow(o) {
  const r = {};
  for (const k in o) {
    try { if (jsonSafe(o[k])) r[k] = o[k]; } catch (_e) { /* skip getters that throw */ }
  }
  return r;
}
function serializeEvent(e) {
  const r = shallow(e);
  if (e.target) r.target = shallow(e.target);
  if (e.currentTarget) r.currentTarget = shallow(e.currentTarget);
  return r;
}

async function start(wasmPath, doc, clock, flags) {
  clock = clock || makeClock();
  const wasi = new WASI({ version: 'preview1', args: ['p'], env: {}, returnOnExit: true });
  const nodes = [null]; // handle 0 == null
  const handlers = {};   // handler id -> { el, name, fn }
  const domSubs = {};    // sub id -> { name, fn } (document-level Browser.Events)
  const outgoing = {};   // outgoing-port name -> [values sent]
  const intervalHandles = {}; // sub id -> clock interval id
  const parkedHttp = [];      // task ids awaiting a response
  let mountedRoot = null;
  let memory = null;
  let instance = null;
  let currentUrl = 'http://localhost/';
  const dec = new TextDecoder();
  const enc = new TextEncoder();
  const str = (ptr, len) => dec.decode(new Uint8Array(memory.buffer, ptr, len));
  const reg = (node) => { nodes.push(node); return nodes.length - 1; };

  // Copy bytes into wasm memory and return the pointer.
  function into(bytes) {
    const ptr = instance.exports.alm_alloc_in(bytes.length);
    new Uint8Array(memory.buffer, ptr, bytes.length).set(bytes);
    return ptr;
  }
  function pushStr(s) { return enc.encode(s); }

  // Deliver a DOM event into wasm: serialize it, hand it to `alm_event`, and
  // honor the returned preventDefault / stopPropagation flags.
  function fire(hid, e) {
    const bytes = enc.encode(JSON.stringify(serializeEvent(e)));
    const flags = instance.exports.alm_event(hid, into(bytes), bytes.length);
    if (flags & 1 && e.preventDefault) e.preventDefault();
    if (flags & 2 && e.stopPropagation) e.stopPropagation();
  }

  const env = {
    dom_create_element: (t, tl) => reg(doc.createElement(str(t, tl))),
    dom_create_element_ns: (n, nl, t, tl) => reg(doc.createElementNS(str(n, nl), str(t, tl))),
    dom_create_text: (s, sl) => reg(doc.createTextNode(str(s, sl))),
    dom_set_text: (node, s, sl) => { nodes[node].textContent = str(s, sl); },
    dom_append_child: (p, c) => { nodes[p].appendChild(nodes[c]); },
    dom_insert_before: (p, c, r) => { nodes[p].insertBefore(nodes[c], r ? nodes[r] : null); },
    dom_remove_child: (p, c) => { nodes[p].removeChild(nodes[c]); },
    dom_replace_child: (p, nw, old) => { nodes[p].replaceChild(nodes[nw], nodes[old]); },
    dom_set_attribute: (node, k, kl, v, vl) => { nodes[node].setAttribute(str(k, kl), str(v, vl)); },
    dom_set_attribute_ns: (node, n, nl, k, kl, v, vl) => {
      nodes[node].setAttributeNS(str(n, nl), str(k, kl), str(v, vl));
    },
    dom_remove_attribute: (node, k, kl) => { nodes[node].removeAttribute(str(k, kl)); },
    dom_set_property: (node, k, kl, j, jl) => {
      setProperty(nodes[node], str(k, kl), JSON.parse(str(j, jl)));
    },
    dom_remove_property: (node, k, kl, wasBool) => {
      setProperty(nodes[node], str(k, kl), wasBool ? false : '');
    },
    dom_set_style: (node, k, kl, v, vl) => { nodes[node].style[str(k, kl)] = str(v, vl); },
    dom_remove_style: (node, k, kl) => { nodes[node].style[str(k, kl)] = ''; },
    dom_add_event_listener: (node, n, nl, hid) => {
      const name = str(n, nl);
      const el = nodes[node];
      const fn = (e) => fire(hid, e);
      handlers[hid] = { el, name, fn };
      el.addEventListener(name, fn);
    },
    dom_remove_event_listener: (_node, _n, _nl, hid) => {
      const h = handlers[hid];
      if (h) { h.el.removeEventListener(h.name, h.fn); delete handlers[hid]; }
    },
    dom_mount: (root) => { mountedRoot = nodes[root]; doc.body.appendChild(mountedRoot); },
    dom_replace_root: (root) => {
      const parent = mountedRoot ? mountedRoot.parentNode : doc.body;
      if (mountedRoot && parent) parent.replaceChild(nodes[root], mountedRoot);
      else doc.body.appendChild(nodes[root]);
      mountedRoot = nodes[root];
    },
    dom_set_title: (s, sl) => { doc.title = str(s, sl); },

    // -- effect hosts --
    host_now: () => clock.now(),
    host_set_interval: (subId, ms) => {
      intervalHandles[subId] = clock.setInterval(() => instance.exports.alm_on_interval(subId), ms);
    },
    host_clear_interval: (subId) => {
      if (intervalHandles[subId] != null) { clock.clearInterval(intervalHandles[subId]); delete intervalHandles[subId]; }
    },
    host_request_frame: () => {
      clock.requestAnimationFrame((t) => instance.exports.alm_on_frame(t));
    },
    host_set_timeout: (taskId, ms) => {
      clock.setTimeout(() => instance.exports.alm_on_timeout(taskId), ms);
    },
    host_http: (taskId) => { parkedHttp.push(taskId); },
    host_port_out: (n, nl, j, jl) => {
      const name = str(n, nl);
      (outgoing[name] = outgoing[name] || []).push(str(j, jl));
    },
    host_add_dom_sub: (subId, n, nl) => {
      const name = str(n, nl);
      const fn = (e) => {
        const bytes = enc.encode(JSON.stringify(serializeEvent(e)));
        instance.exports.alm_on_dom_event(subId, into(bytes), bytes.length);
      };
      domSubs[subId] = { name, fn };
      doc.addEventListener(name, fn);
    },
    host_remove_dom_sub: (subId) => {
      const s = domSubs[subId];
      if (s) { doc.removeEventListener(s.name, s.fn); delete domSubs[subId]; }
    },
    host_nav: (kind, u, ul) => {
      // push (0) / replace (1): resolve against the current URL and report the
      // change back so onUrlChange fires. go/load/reload are no-ops here.
      if (kind === 0 || kind === 1) {
        currentUrl = new URL(str(u, ul), currentUrl).href;
        const b = enc.encode(currentUrl);
        instance.exports.alm_on_url_change(into(b), b.length);
      }
    },
  };

  const importObject = Object.assign({ env }, wasi.getImportObject());
  const module = await WebAssembly.compile(fs.readFileSync(wasmPath));
  instance = await WebAssembly.instantiate(module, importObject);
  memory = instance.exports.memory;
  // Run WASI `_start` to initialize the runtime + program globals (memory,
  // std HashMap seeding). For a Browser.* program the entry point returns
  // immediately; then we drive it through the browser export.
  wasi.start(instance);
  // Seed the current URL + flags before init reads them.
  {
    const b = enc.encode(currentUrl);
    instance.exports.alm_browser_set_url(into(b), b.length);
    const f = enc.encode(flags == null ? 'null' : flags);
    instance.exports.alm_browser_set_flags(into(f), f.length);
  }
  instance.exports.alm_browser_start();

  return {
    instance,
    nodes,
    clock,
    outgoing,
    // Deliver a value to an incoming port.
    sendPort(name, value) {
      const nb = pushStr(name);
      const jb = pushStr(JSON.stringify(value));
      const np = into(nb);
      const jp = into(jb);
      instance.exports.alm_port_send(np, nb.length, jp, jb.length);
    },
    // Resolve the oldest pending HTTP request (status 0 == network error).
    resolveHttp(status, body) {
      const taskId = parkedHttp.shift();
      if (taskId == null) return;
      const b = enc.encode(body || '');
      instance.exports.alm_on_http(taskId, status, into(b), b.length);
    },
  };
}

module.exports = { start };
