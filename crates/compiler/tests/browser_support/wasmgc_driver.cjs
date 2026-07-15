'use strict';
// Drives a WasmGC Browser.* module against the DOM stub: provides the `dom_*`
// host imports (backed by an integer node-handle table, reading strings from
// the module's linear memory) and calls `alm_browser_start`. No WASI — the
// WasmGC module is self-contained.

const fs = require('fs');

function start(wasmPath, doc) {
  const nodes = [null]; // handle 0 == null
  let mountedRoot = null;
  let memory = null;
  let instance = null;
  const dec = new TextDecoder();
  const str = (p, l) => dec.decode(new Uint8Array(memory.buffer, p, l));
  const reg = (n) => { nodes.push(n); return nodes.length - 1; };
  const outgoing = {}; // port name -> list of JSON strings (matches js_driver)

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
  };

  instance = new WebAssembly.Instance(new WebAssembly.Module(fs.readFileSync(wasmPath)), { env });
  memory = instance.exports.memory;
  instance.exports.alm_browser_start();
  return { instance, nodes, outgoing };
}

module.exports = { start };
