// Browser wasm-gc host shim (ported from tests/browser_support/wasmgc_driver.cjs
// to a real DOM + real timers). Exposes window.almStart(wasmBytes) -> app.
window.almStart = function (bytes) {
  const doc = document;
  const nodes = [null];
  let mountedRoot = null, memory = null, instance = null;
  const dec = new TextDecoder();
  const str = (p, l) => dec.decode(new Uint8Array(memory.buffer, p, l));
  const reg = (n) => { nodes.push(n); n.__h = nodes.length - 1; return nodes.length - 1; };
  const pendingFree = [];
  function releaseHandles(node) {
    if (node.__h != null) { nodes[node.__h] = null; node.__h = null; }
    if (node.__almhids) for (let i = 0; i < node.__almhids.length; i++) pendingFree.push(node.__almhids[i]);
    if (node.__reg) { const k = node.childNodes; if (k) for (let i = 0; i < k.length; i++) releaseHandles(k[i]); }
  }
  function freeNodeWalk(node) {
    if (!node) return;
    if (node.__almhids) for (let i = 0; i < node.__almhids.length; i++) pendingFree.push(node.__almhids[i]);
    const k = node.childNodes; if (k) for (let i = 0; i < k.length; i++) freeNodeWalk(k[i]);
    if (node.__h != null) { nodes[node.__h] = null; node.__h = null; }
  }
  function freeNode(node) {
    if (!node) return;
    if (node.__hidLo != null) { for (let h = node.__hidLo; h < node.__hidHi; h++) pendingFree.push(h); node.__hidLo = null; releaseHandles(node); }
    else freeNodeWalk(node);
  }
  function flushFree() {
    if (!pendingFree.length) return;
    const CHUNK = 4096;
    for (let off = 0; off < pendingFree.length; off += CHUNK) {
      const n = Math.min(CHUNK, pendingFree.length - off);
      const mem = new Int32Array(memory.buffer, 0, n);
      for (let i = 0; i < n; i++) mem[i] = pendingFree[off + i];
      instance.exports.alm_free_handlers(0, n);
    }
    pendingFree.length = 0;
  }
  function attachListener(node, name, hid) {
    (node.__almhids || (node.__almhids = [])).push(hid);
    const handler = (ev) => {
      const t = (ev && ev.target) || {};
      const payload = { target: { value: t.value || '', checked: !!t.checked } };
      const b = new TextEncoder().encode(JSON.stringify(payload));
      new Uint8Array(memory.buffer, 0, b.length).set(b);
      const flags = instance.exports.alm_event(hid, 0, b.length) | 0;
      flushFree();
      if ((flags & 1) && ev.preventDefault) ev.preventDefault();
      if ((flags & 2) && ev.stopPropagation) ev.stopPropagation();
    };
    node.addEventListener(name, handler);
    (node.__evt || (node.__evt = {}))[name] = { handler, hid };
  }
  function domBuild(ptr) {
    const buf = new Uint8Array(memory.buffer), dv = new DataView(memory.buffer);
    let p = ptr;
    const u32 = () => { const v = dv.getUint32(p, true); p += 4; return v; };
    const rs = () => { const n = u32(); const s = dec.decode(buf.subarray(p, p + n)); p += n; return s; };
    const stack = []; let root = null, hidLo = -1, hidHi = -1;
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
      else if (op === 7) { const nm = rs(); const hid = u32(); attachListener(stack[stack.length - 1], nm, hid); if (hidLo < 0) hidLo = hid; hidHi = hid + 1; }
    }
    if (hidLo >= 0) { root.__hidLo = hidLo; root.__hidHi = hidHi; }
    return reg(root);
  }
  const noop = () => 0;
  const env = {
    dom_create_element: (t, tl) => reg(doc.createElement(str(t, tl))),
    dom_create_text: (s, sl) => reg(doc.createTextNode(str(s, sl))),
    dom_set_attribute: (n, kp, kl, vp, vl) => { nodes[n].setAttribute(str(kp, kl), str(vp, vl)); },
    dom_set_style: (n, kp, kl, vp, vl) => { nodes[n].style[str(kp, kl)] = str(vp, vl); },
    dom_set_property: (n, p, l) => { nodes[n][str(p, l)] = true; },
    dom_append_child: (p, c) => { nodes[p].appendChild(nodes[c]); },
    dom_insert_before: (p, n, ref) => { nodes[p].insertBefore(nodes[n], nodes[ref]); },
    dom_add_event_listener: (n, np, nl, hid) => attachListener(nodes[n], str(np, nl), hid),
    dom_get_event_hid: (n, np, nl) => { const nd = nodes[n], name = str(np, nl); return nd.__evt && nd.__evt[name] ? nd.__evt[name].hid : -1; },
    dom_build: (p) => domBuild(p),
    dom_clear: (p) => { const par = nodes[p]; let c; while ((c = par.firstChild)) { par.removeChild(c); freeNode(c); } },
    dom_mount: (r) => { mountedRoot = nodes[r]; doc.body.appendChild(mountedRoot); },
    dom_replace_root: (r) => { const par = mountedRoot ? mountedRoot.parentNode : doc.body; if (mountedRoot && par) par.replaceChild(nodes[r], mountedRoot); else doc.body.appendChild(nodes[r]); mountedRoot = nodes[r]; },
    dom_child: (p, i) => { const par = nodes[p]; par.__reg = true; const c = par.childNodes[i]; return c.__h != null ? c.__h : reg(c); },
    dom_set_text: (n, s, sl) => { nodes[n].textContent = str(s, sl); },
    dom_remove_attribute: (n, k, kl) => { nodes[n].removeAttribute(str(k, kl)); },
    dom_remove_child: (p, c) => { const ch = nodes[c]; nodes[p].removeChild(ch); freeNode(ch); },
    dom_replace: (o, nw) => { const old = nodes[o]; if (old.parentNode) old.parentNode.replaceChild(nodes[nw], old); if (old === mountedRoot) mountedRoot = nodes[nw]; freeNode(old); },
    dom_remove_event_listener: (n, np, nl) => { const nd = nodes[n], name = str(np, nl); const rec = nd.__evt && nd.__evt[name]; if (!rec) return -1; nd.removeEventListener(name, rec.handler); delete nd.__evt[name]; return rec.hid; },
    host_port_out: noop, host_set_title: (p, l) => { doc.title = str(p, l); },
    host_http: noop, host_clear_timers: noop, host_set_interval: noop, host_clear_dom: noop,
    host_add_dom: noop, host_push_url: noop, host_get_url: () => 0, host_load: noop,
    host_clear_frames: noop, host_request_frame: noop, host_set_timeout: noop,
    math_sin: Math.sin, math_cos: Math.cos, math_tan: Math.tan, math_asin: Math.asin,
    math_acos: Math.acos, math_atan: Math.atan, math_log: Math.log, math_atan2: Math.atan2, math_pow: Math.pow,
    host_now: () => performance.now(),
    host_ftoa: (x, o) => { const b = new TextEncoder().encode(String(x)); new Uint8Array(memory.buffer, o, b.length).set(b); return b.length; },
    host_atof: (p, l, o) => { const s = str(p, l); if (!s.length || /[\sxbo]/.test(s)) return 0; const n = +s; if (n !== n) return 0; new DataView(memory.buffer).setFloat64(o, n, true); return 1; },
    host_regex_compile: noop, host_regex_find: noop, host_regex_split: noop,
  };
  instance = new WebAssembly.Instance(new WebAssembly.Module(bytes), { env: new Proxy(env, { get: (t, k) => k in t ? t[k] : noop }) });
  memory = instance.exports.memory;
  instance.exports.alm_browser_start();
  return { instance, flushFree };
};
