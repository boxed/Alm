'use strict';
// Drives a WasmGC Browser.* module against the DOM stub: provides the `dom_*`
// host imports (backed by an integer node-handle table, reading strings from
// the module's linear memory) and calls `alm_browser_start`. No WASI — the
// WasmGC module is self-contained.

const fs = require('fs');
const { makeClock } = require('./clock.cjs');
const { regexHost } = require('./regex_host.cjs');

function start(wasmPath, doc, clock) {
  clock = clock || makeClock();
  const nodes = [null]; // handle 0 == null
  let mountedRoot = null;
  let memory = null;
  let instance = null;
  let timerIds = []; // active Time.every interval ids (in the virtual clock)
  let domSubs = []; // active document listeners: { name, handler }
  let frameIds = []; // active animation-frame request ids (in the virtual clock)
  let currentUrl = 'http://localhost/'; // matches js_driver's location stub
  const dec = new TextDecoder();
  const str = (p, l) => dec.decode(new Uint8Array(memory.buffer, p, l));
  const reg = (n) => { nodes.push(n); n.__h = nodes.length - 1; return nodes.length - 1; };
  // Reclaim a removed subtree (mirrors the browser shim): release handle slots
  // now, but QUEUE wasm-side handler frees — freeNode runs inside dom_remove_child
  // (called from wasm mid-render), so calling back into wasm there corrupts the
  // render; flushFree drains the queue after the entry returns.
  const pendingFree = [];
  // Reclaim a removed subtree. A node built by dom_build carries the contiguous
  // hid range [__hidLo,__hidHi) of every handler in its subtree, so we free them
  // without walking. We then release handle-table slots: only the root and any
  // descendants actually reached by dom_child (marked __reg) hold slots, so the
  // walk is skipped entirely for untouched subtrees (e.g. a cleared lazy row).
  function releaseHandles(node) {
    if (node.__h != null) { nodes[node.__h] = null; node.__h = null; }
    // Free any listener hids on this reached node (create hids double-free
    // harmlessly with the __hidLo range; patch-added hids are only here).
    if (node.__almhids) for (let i = 0; i < node.__almhids.length; i++) pendingFree.push(node.__almhids[i]);
    if (node.__reg) { const k = node.childNodes; if (k) for (let i = 0; i < k.length; i++) releaseHandles(k[i]); }
  }
  function freeNodeWalk(node) {
    if (!node) return;
    const hids = node.__almhids;
    if (hids) for (let i = 0; i < hids.length; i++) pendingFree.push(hids[i]);
    const k = node.childNodes;
    if (k) for (let i = 0; i < k.length; i++) freeNodeWalk(k[i]);
    if (node.__h != null) { nodes[node.__h] = null; node.__h = null; }
  }
  function freeNode(node) {
    if (!node) return;
    if (node.__hidLo != null) {
      for (let h = node.__hidLo; h < node.__hidHi; h++) pendingFree.push(h);
      node.__hidLo = null;
      releaseHandles(node);
    } else {
      freeNodeWalk(node); // node not built via dom_build — fall back to walking
    }
  }
  function flushFree() {
    if (pendingFree.length === 0) return;
    const CHUNK = 4096;
    for (let off = 0; off < pendingFree.length; off += CHUNK) {
      const n = Math.min(CHUNK, pendingFree.length - off);
      const mem = new Int32Array(memory.buffer, 0, n);
      for (let i = 0; i < n; i++) mem[i] = pendingFree[off + i];
      instance.exports.alm_free_handlers(0, n);
    }
    pendingFree.length = 0;
  }
  // Attach an event listener whose handler id `hid` the wasm side dispatches
  // through (the decoder lives in the module's handler table).
  function attachListener(node, name, hid) {
    (node.__almhids || (node.__almhids = [])).push(hid);
    const handler = (ev) => {
      const t = (ev && ev.target) || {};
      const payload = { target: { value: t.value || '', checked: !!t.checked } };
      const bytes = new TextEncoder().encode(JSON.stringify(payload));
      new Uint8Array(memory.buffer, 0, bytes.length).set(bytes);
      const flags = instance.exports.alm_event(hid, 0, bytes.length) | 0;
      flushFree();
      if ((flags & 1) && ev.preventDefault) ev.preventDefault();
      if ((flags & 2) && ev.stopPropagation) ev.stopPropagation();
    };
    node.addEventListener(name, handler);
    // Record by event name so the patch can find/remove this listener (and its
    // hid) when the handler is dropped or replaced.
    (node.__evt || (node.__evt = {}))[name] = { handler, hid };
  }
  // Build a whole subtree from the module's DOM-build bytecode stream in one
  // call (opcodes: 0 END, 1 OPEN<tag>, 2 CLOSE, 3 TEXT<s>, 4 ATTR<k><v>,
  // 5 STYLE<k><v>, 6 PROP<name>, 7 EVENT<name><hid>). Only the root is registered
  // in the handle table; descendants get handles lazily via dom_child.
  function domBuild(ptr) {
    const buf = new Uint8Array(memory.buffer);
    const dv = new DataView(memory.buffer);
    let p = ptr;
    const u32 = () => { const v = dv.getUint32(p, true); p += 4; return v; };
    const rs = () => { const n = u32(); const s = dec.decode(buf.subarray(p, p + n)); p += n; return s; };
    const stack = [];
    let root = null;
    let hidLo = -1, hidHi = -1; // handler ids in this subtree are contiguous
    const put = (nd) => { if (stack.length) stack[stack.length - 1].appendChild(nd); else root = nd; };
    for (;;) {
      const op = buf[p++];
      if (op === 0) break;
      else if (op === 1) { const el = doc.createElement(rs()); put(el); stack.push(el); }
      else if (op === 2) stack.pop();
      else if (op === 3) put(doc.createTextNode(rs()));
      else if (op === 4) { const k = rs(), v = rs(); stack[stack.length - 1].setAttribute(k, v); }
      else if (op === 5) { const k = rs(), v = rs(); stack[stack.length - 1].style[k] = v; }
      else if (op === 6) { const nm = rs(); stack[stack.length - 1][nm] = true; }
      else if (op === 7) {
        const nm = rs(); const hid = u32();
        attachListener(stack[stack.length - 1], nm, hid);
        if (hidLo < 0) hidLo = hid;
        hidHi = hid + 1;
      }
    }
    // Record the subtree's handler range on its root so removal can reclaim it
    // without walking (see freeNode).
    if (hidLo >= 0) { root.__hidLo = hidLo; root.__hidHi = hidHi; }
    return reg(root);
  }
  const outgoing = {}; // port name -> list of JSON strings (matches js_driver)
  const parkedHttp = []; // in-flight HTTP requests: { reqId, url }

  const env = {
    dom_create_element: (t, tl) => reg(doc.createElement(str(t, tl))),
    dom_create_text: (s, sl) => reg(doc.createTextNode(str(s, sl))),
    dom_set_attribute: (n, kp, kl, vp, vl) => { nodes[n].setAttribute(str(kp, kl), str(vp, vl)); },
    dom_set_style: (n, kp, kl, vp, vl) => { nodes[n].style[str(kp, kl)] = str(vp, vl); },
    dom_set_property: (n, p, l) => { nodes[n][str(p, l)] = true; },
    dom_append_child: (p, c) => { nodes[p].appendChild(nodes[c]); },
    dom_insert_before: (p, n, ref) => { nodes[p].insertBefore(nodes[n], nodes[ref]); },
    // Patch-time: attach a listener for a newly-added event. attachListener
    // records the hid in __almhids, which releaseHandles frees when the node is
    // later removed, so no extra tracking is needed.
    dom_add_event_listener: (n, np, nl, hid) => attachListener(nodes[n], str(np, nl), hid),
    // Patch-time: does this node already have a listener for `name`? Returns its
    // hid so the patch can reuse it (rewrite the decoder) instead of re-attaching.
    dom_get_event_hid: (n, np, nl) => {
      const nd = nodes[n];
      const name = str(np, nl);
      return nd.__evt && nd.__evt[name] ? nd.__evt[name].hid : -1;
    },
    dom_build: (ptr) => domBuild(ptr),
    dom_clear: (p) => { const par = nodes[p]; let c; while ((c = par.firstChild)) { par.removeChild(c); freeNode(c); } },
    dom_mount: (r) => { mountedRoot = nodes[r]; doc.body.appendChild(mountedRoot); },
    dom_replace_root: (r) => {
      const parent = mountedRoot ? mountedRoot.parentNode : doc.body;
      if (mountedRoot && parent) parent.replaceChild(nodes[r], mountedRoot);
      else doc.body.appendChild(nodes[r]);
      mountedRoot = nodes[r];
    },
    dom_child: (p, i) => { const par = nodes[p]; par.__reg = true; const c = par.childNodes[i]; return c.__h != null ? c.__h : reg(c); },
    dom_set_text: (n, s, sl) => { nodes[n].textContent = str(s, sl); },
    dom_remove_attribute: (n, k, kl) => { nodes[n].removeAttribute(str(k, kl)); },
    dom_remove_child: (p, c) => { const ch = nodes[c]; nodes[p].removeChild(ch); freeNode(ch); },
    dom_replace: (o, nw) => {
      const old = nodes[o];
      if (old.parentNode) old.parentNode.replaceChild(nodes[nw], old);
      if (old === mountedRoot) mountedRoot = nodes[nw];
      freeNode(old);
    },
    // Patch-time: drop the listener for `name` and return its freed hid (-1 if
    // none) so the wasm side can release the handler-table slot.
    dom_remove_event_listener: (n, np, nl) => {
      const nd = nodes[n];
      const name = str(np, nl);
      const rec = nd.__evt && nd.__evt[name];
      if (!rec) return -1;
      nd.removeEventListener(name, rec.handler);
      delete nd.__evt[name];
      return rec.hid;
    },
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
    host_set_timeout: (ms, slot) => {
      clock.setTimeout(() => instance.exports.alm_task_resume(slot), ms);
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
    host_clear_frames: () => { frameIds.forEach((id) => clock.cancelAnimationFrame(id)); frameIds = []; },
    host_request_frame: (slot) => {
      // Mirror runtime.js's rAF loop: track `last`, fire (delta, now), re-request.
      let last = clock.now();
      const loop = () => {
        const now = clock.now();
        const delta = now - last;
        last = now;
        instance.exports.alm_frame(slot, delta, now);
        frameIds.push(clock.requestAnimationFrame(loop));
      };
      frameIds.push(clock.requestAnimationFrame(loop));
    },
    host_push_url: (p, l, _replace) => { currentUrl = new URL(str(p, l), currentUrl).href; },
    host_get_url: (out) => {
      const bytes = new TextEncoder().encode(currentUrl);
      new Uint8Array(memory.buffer, out, bytes.length).set(bytes);
      return bytes.length;
    },
    host_load: (p, l) => { currentUrl = new URL(str(p, l), currentUrl).href; },
    // Host Math.* (Basics transcendentals).
    math_sin: Math.sin, math_cos: Math.cos, math_tan: Math.tan,
    math_asin: Math.asin, math_acos: Math.acos, math_atan: Math.atan,
    math_log: Math.log, math_atan2: Math.atan2, math_pow: Math.pow,
    // Time.now (virtual clock) + String.fromFloat/toFloat.
    host_now: () => clock.now(),
    host_ftoa: (x, o) => { const b = Buffer.from(String(x)); new Uint8Array(memory.buffer, o, b.length).set(b); return b.length; },
    host_atof: (p, l, o) => {
      const s = Buffer.from(new Uint8Array(memory.buffer, p, l)).toString();
      if (s.length === 0 || /[\sxbo]/.test(s)) return 0;
      const n = +s; if (n !== n) return 0;
      new DataView(memory.buffer).setFloat64(o, n, true); return 1;
    },
    // elm/regex host — delegate to JS RegExp (see REGEX_HOST below).
    ...regexHost(() => memory),
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
