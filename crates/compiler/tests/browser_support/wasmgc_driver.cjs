'use strict';
// Drives a WasmGC Browser.* module against the DOM stub: provides the `dom_*`
// host imports (backed by an integer node-handle table, reading strings from
// the module's linear memory) and calls `alm_browser_start`. No WASI — the
// WasmGC module is self-contained.

const fs = require('fs');
const { makeClock } = require('./clock.cjs');

function start(wasmPath, doc, clock) {
  clock = clock || makeClock();
  const nodes = [null]; // handle 0 == null
  let mountedRoot = null;
  let memory = null;
  let instance = null;
  let timerIds = []; // active Time.every interval ids (in the virtual clock)
  let domSubs = []; // active document listeners: { name, handler }
  const dec = new TextDecoder();
  const str = (p, l) => dec.decode(new Uint8Array(memory.buffer, p, l));
  const reg = (n) => { nodes.push(n); return nodes.length - 1; };
  const outgoing = {}; // port name -> list of JSON strings (matches js_driver)
  const parkedHttp = []; // in-flight HTTP requests: { reqId, url }

  const env = {
    dom_create_element: (t, tl) => reg(doc.createElement(str(t, tl))),
    dom_create_text: (s, sl) => reg(doc.createTextNode(str(s, sl))),
    dom_set_attribute: (n, kp, kl, vp, vl) => { nodes[n].setAttribute(str(kp, kl), str(vp, vl)); },
    dom_set_style: (n, kp, kl, vp, vl) => { nodes[n].style[str(kp, kl)] = str(vp, vl); },
    dom_append_child: (p, c) => { nodes[p].appendChild(nodes[c]); },
    dom_add_event_listener: (n, np, nl, hid) => {
      const name = str(np, nl);
      nodes[n].addEventListener(name, (ev) => {
        // Serialize a minimal event object to JSON and hand it to the module in
        // the reserved [0, 64KiB) scratch region (bump strings live above it).
        const payload = { target: { value: (ev && ev.target && ev.target.value) || '' } };
        const bytes = new TextEncoder().encode(JSON.stringify(payload));
        new Uint8Array(memory.buffer, 0, bytes.length).set(bytes);
        instance.exports.alm_event(hid, 0, bytes.length);
      });
    },
    dom_mount: (r) => { mountedRoot = nodes[r]; doc.body.appendChild(mountedRoot); },
    dom_replace_root: (r) => {
      const parent = mountedRoot ? mountedRoot.parentNode : doc.body;
      if (mountedRoot && parent) parent.replaceChild(nodes[r], mountedRoot);
      else doc.body.appendChild(nodes[r]);
      mountedRoot = nodes[r];
    },
    dom_child: (p, i) => reg(nodes[p].childNodes[i]),
    dom_set_text: (n, s, sl) => { nodes[n].textContent = str(s, sl); },
    dom_remove_attribute: (n, k, kl) => { nodes[n].removeAttribute(str(k, kl)); },
    dom_remove_child: (p, c) => { nodes[p].removeChild(nodes[c]); },
    dom_replace: (o, nw) => {
      const old = nodes[o];
      if (old.parentNode) old.parentNode.replaceChild(nodes[nw], old);
      if (old === mountedRoot) mountedRoot = nodes[nw];
    },
    dom_remove_event_listener: () => {},
    host_port_out: (np, nl, jp, jl) => {
      const name = str(np, nl);
      (outgoing[name] = outgoing[name] || []).push(str(jp, jl));
    },
    host_set_title: (p, l) => { doc.title = str(p, l); },
    host_http: (up, ul, reqId) => { parkedHttp.push({ reqId, url: str(up, ul) }); },
    host_clear_timers: () => { timerIds.forEach((id) => clock.clearInterval(id)); timerIds = []; },
    host_set_interval: (ms, slot) => {
      timerIds.push(clock.setInterval(() => instance.exports.alm_tick(slot, clock.now()), ms));
    },
    host_clear_dom: () => {
      domSubs.forEach((s) => doc.removeEventListener && doc.removeEventListener(s.name, s.handler));
      domSubs = [];
    },
    host_add_dom: (np, nl, slot) => {
      const name = str(np, nl);
      const handler = (ev) => {
        // Serialize the event's own JSON-able props (functions are skipped) into
        // the reserved inbound region and run the decoder on the wasm side.
        const bytes = new TextEncoder().encode(JSON.stringify(ev || {}));
        new Uint8Array(memory.buffer, 0, bytes.length).set(bytes);
        instance.exports.alm_dom_event(slot, 0, bytes.length);
      };
      doc.addEventListener(name, handler);
      domSubs.push({ name, handler });
    },
  };

  instance = new WebAssembly.Instance(new WebAssembly.Module(fs.readFileSync(wasmPath)), { env });
  memory = instance.exports.memory;
  instance.exports.alm_browser_start();
  // Deliver an incoming-port message. Name goes at offset 0, JSON at 32 KiB —
  // both inside the reserved [0, 64 KiB) inbound region (below the bump base).
  function sendPort(name, value) {
    const enc = new TextEncoder();
    const nb = enc.encode(name);
    const jb = enc.encode(JSON.stringify(value));
    new Uint8Array(memory.buffer, 0, nb.length).set(nb);
    new Uint8Array(memory.buffer, 32768, jb.length).set(jb);
    instance.exports.alm_port_in(0, nb.length, 32768, jb.length);
  }
  // Settle the oldest in-flight request. Body goes at offset 0 (the reserved
  // inbound region); alm_http_response reads it synchronously.
  function resolveHttp(status, body) {
    const req = parkedHttp.shift();
    if (!req) return;
    const jb = new TextEncoder().encode(body || '');
    new Uint8Array(memory.buffer, 0, jb.length).set(jb);
    instance.exports.alm_http_response(req.reqId, status, 0, jb.length);
  }
  return { instance, nodes, outgoing, sendPort, resolveHttp, clock };
}

module.exports = { start };
