'use strict';
// Drives the JS-backend bundle (runtime.js) against the same DOM stub, as the
// oracle. runtime.js reads `opts.node.ownerDocument` for its document and
// replaces `opts.node` with the rendered root, so we mount a host div under
// body and let it be replaced.
//
// runtime.js reaches for global timers / Date.now for its effects; we point
// those at the backend's virtual clock so time-based effects are deterministic
// and line up with the wasm backend.

const { makeClock } = require('./clock.cjs');

function start(bundlePath, doc, clock, flags) {
  clock = clock || makeClock();
  const saved = {
    setTimeout: globalThis.setTimeout,
    clearTimeout: globalThis.clearTimeout,
    setInterval: globalThis.setInterval,
    clearInterval: globalThis.clearInterval,
    requestAnimationFrame: globalThis.requestAnimationFrame,
    cancelAnimationFrame: globalThis.cancelAnimationFrame,
    dateNow: Date.now,
  };
  globalThis.setTimeout = (fn, ms) => clock.setTimeout(fn, ms);
  globalThis.clearTimeout = (id) => clock.clearTimeout(id);
  globalThis.setInterval = (fn, ms) => clock.setInterval(fn, ms);
  globalThis.clearInterval = (id) => clock.clearInterval(id);
  globalThis.requestAnimationFrame = (fn) => clock.requestAnimationFrame(fn);
  globalThis.cancelAnimationFrame = (id) => clock.cancelAnimationFrame(id);
  Date.now = () => clock.now();

  // A minimal location/history so Browser.application can navigate (node has
  // neither). Backed by one URL string; runtime.js reads location.href and
  // calls history.pushState.
  let currentUrl = 'http://localhost/';
  const locationStub = {
    get href() { return currentUrl; },
    get origin() { return new URL(currentUrl).origin; },
    get pathname() { return new URL(currentUrl).pathname; },
    reload() {},
  };
  const historyStub = {
    pushState(_s, _t, u) { currentUrl = new URL(u, currentUrl).href; },
    replaceState(_s, _t, u) { currentUrl = new URL(u, currentUrl).href; },
    go() {},
  };
  saved.location = globalThis.location;
  saved.history = globalThis.history;
  saved.window = globalThis.window;
  globalThis.location = locationStub;
  globalThis.history = historyStub;
  globalThis.window = { addEventListener() {}, location: locationStub };

  // Stub fetch: park each request; resolveHttp settles the oldest with a
  // Response-like object runtime.js's `_Http_makeTask` consumes.
  const parkedFetch = [];
  saved.fetch = globalThis.fetch;
  globalThis.fetch = (url) => new Promise((resolve, reject) => parkedFetch.push({ resolve, reject, url }));

  const mod = require(bundlePath);
  const program = mod.Test.main;
  const host = doc.createElement('div');
  doc.body.appendChild(host);
  const app = program.init({ node: host, flags: JSON.parse(flags == null ? 'null' : flags) });
  // sandbox/element replace `host` with their root; document/application ignore
  // it and mount into <body> directly — drop the leftover host so the body
  // holds only the app root, matching the wasm mount.
  if (host.parentNode) host.parentNode.removeChild(host);

  function restore() {
    Object.assign(globalThis, {
      setTimeout: saved.setTimeout,
      clearTimeout: saved.clearTimeout,
      setInterval: saved.setInterval,
      clearInterval: saved.clearInterval,
      requestAnimationFrame: saved.requestAnimationFrame,
      cancelAnimationFrame: saved.cancelAnimationFrame,
      location: saved.location,
      history: saved.history,
      window: saved.window,
      fetch: saved.fetch,
    });
    Date.now = saved.dateNow;
  }

  const outgoing = {};
  if (app && app.ports) {
    for (const name of Object.keys(app.ports)) {
      const p = app.ports[name];
      if (p && typeof p.subscribe === 'function') {
        outgoing[name] = [];
        p.subscribe((v) => outgoing[name].push(JSON.stringify(v)));
      }
    }
  }

  return {
    app,
    clock,
    outgoing,
    restore,
    sendPort(name, value) {
      if (app && app.ports && app.ports[name] && app.ports[name].send) {
        app.ports[name].send(value);
      }
    },
    resolveHttp(status, body) {
      const req = parkedFetch.shift();
      if (!req) return;
      // status 0 models a network error — a fetch rejection (runtime.js's
      // .catch turns it into NetworkError_).
      if (status === 0) { req.reject(new Error('network')); return; }
      req.resolve({
        ok: status >= 200 && status < 300,
        status,
        statusText: '',
        url: req.url,
        text: () => Promise.resolve(body || ''),
      });
    },
  };
}

module.exports = { start };
