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

  const env = {
    dom_create_element: (t, tl) => reg(doc.createElement(str(t, tl))),
    dom_create_text: (s, sl) => reg(doc.createTextNode(str(s, sl))),
    dom_set_attribute: (n, kp, kl, vp, vl) => { nodes[n].setAttribute(str(kp, kl), str(vp, vl)); },
    dom_set_style: (n, kp, kl, vp, vl) => { nodes[n].style[str(kp, kl)] = str(vp, vl); },
    dom_append_child: (p, c) => { nodes[p].appendChild(nodes[c]); },
    dom_add_event_listener: (n, np, nl, hid) => {
      const name = str(np, nl);
      nodes[n].addEventListener(name, () => instance.exports.alm_event(hid, 0, 0));
    },
    dom_mount: (r) => { mountedRoot = nodes[r]; doc.body.appendChild(mountedRoot); },
    dom_replace_root: (r) => {
      const parent = mountedRoot ? mountedRoot.parentNode : doc.body;
      if (mountedRoot && parent) parent.replaceChild(nodes[r], mountedRoot);
      else doc.body.appendChild(nodes[r]);
      mountedRoot = nodes[r];
    },
  };

  instance = new WebAssembly.Instance(new WebAssembly.Module(fs.readFileSync(wasmPath)), { env });
  memory = instance.exports.memory;
  instance.exports.alm_browser_start();
  return { instance, nodes };
}

module.exports = { start };
