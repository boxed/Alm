'use strict';
// A minimal DOM, just enough for elm/virtual-dom rendering, event dispatch,
// and deterministic serialization. Both backends drive the SAME stub, so the
// differential test asserts "the two backends agree", not "matches a real
// browser" — the serializer only has to be consistent.
//
// runtime.js (the JS oracle) mutates the DOM imperatively — `dom.className =
// 'x'`, `dom.style.color = 'red'`, `dom[prop] = val`. The wasm shim does the
// same via `setProperty`. Elements expose accessor properties for the common
// reflected DOM props (className, value, checked, ...), each routing to the
// serialized attribute map; both backends therefore go through the identical
// `el[key] = val` path. style is a plain object (direct `[k]=v` works).
//
// Elements are stored raw in the tree (no Proxy) so that childNode identity is
// stable — runtime.js's keyed reconciliation relies on it.

function escapeText(s) {
  return String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
}
function escapeAttr(s) {
  return String(s).replace(/&/g, '&amp;').replace(/"/g, '&quot;').replace(/</g, '&lt;');
}
function propToAttr(key) {
  if (key === 'className') return 'class';
  if (key === 'htmlFor') return 'for';
  return key;
}

class Node {
  constructor(doc) {
    this.ownerDocument = doc;
    this.childNodes = [];
    this.parentNode = null;
  }
  // Nodes are stored as-is (elements stay wrapped in their Proxy), so that a
  // later `child.prop = v` assignment reached via `parent.childNodes[k]` still
  // hits the Proxy and reflects — matching how the initial render set it.
  appendChild(c) {
    if (c.parentNode) c.parentNode.removeChild(c);
    c.parentNode = this;
    this.childNodes.push(c);
    return c;
  }
  insertBefore(c, ref) {
    if (c.parentNode) c.parentNode.removeChild(c);
    c.parentNode = this;
    if (!ref) { this.childNodes.push(c); return c; }
    const i = this.childNodes.indexOf(ref);
    this.childNodes.splice(i < 0 ? this.childNodes.length : i, 0, c);
    return c;
  }
  removeChild(c) {
    const i = this.childNodes.indexOf(c);
    if (i >= 0) this.childNodes.splice(i, 1);
    c.parentNode = null;
    return c;
  }
  replaceChild(nw, old) {
    const i = this.childNodes.indexOf(old);
    if (i >= 0) {
      if (nw.parentNode) nw.parentNode.removeChild(nw);
      this.childNodes[i] = nw;
      nw.parentNode = this;
      old.parentNode = null;
    }
    return old;
  }
}

class TextNode extends Node {
  constructor(doc, text) { super(doc); this.nodeType = 3; this._text = text; }
  get textContent() { return this._text; }
  set textContent(v) { this._text = v; }
  get data() { return this._text; }
  set data(v) { this._text = v; }
  serialize() { return escapeText(this._text); }
}

class Element extends Node {
  constructor(doc, tag, ns) {
    super(doc);
    this.nodeType = 1;
    this.tagName = tag;
    this.ns = ns || null;
    this.style = {};            // `dom.style[k] = v`; '' means "no value"
    this.listeners = {};        // name -> [fn]
    this._almListeners = {};    // runtime.js stashes its listener records here
    // Insertion-ordered attribute map keyed by serialized name, so attributes
    // and reflected properties serialize in application order — identical for
    // both backends (both apply the same Elm attr list in order).
    this._attrs = new Map();    // name -> string | true (boolean attr)
  }
  _set(name, val) { this._attrs.set(name, val); }
  _del(name) { this._attrs.delete(name); }

  setAttribute(k, v) { this._set(k, String(v)); }
  removeAttribute(k) { this._del(k); }
  setAttributeNS(_ns, k, v) { this._set(k, String(v)); }
  removeAttributeNS(_ns, k) { this._del(k); }
  hasAttribute(k) { return this._attrs.has(k); }
  getAttribute(k) { const v = this._attrs.get(k); return v == null ? null : (v === true ? '' : v); }

  addEventListener(name, fn) { (this.listeners[name] = this.listeners[name] || []).push(fn); }
  removeEventListener(name, fn) {
    const l = this.listeners[name];
    if (l) { const i = l.indexOf(fn); if (i >= 0) l.splice(i, 1); }
  }

  get textContent() {
    return this.childNodes.map((c) => c.nodeType === 3 ? c._text : c.textContent).join('');
  }
  set textContent(v) {
    for (const c of this.childNodes) c.parentNode = null;
    this.childNodes = [];
    if (v !== '') this.appendChild(new TextNode(this.ownerDocument, v));
  }

  serialize() {
    let attrs = '';
    for (const [name, val] of this._attrs) {
      if (val === true) attrs += ` ${name}`;
      else if (val === false) { /* absent */ }
      else attrs += ` ${name}="${escapeAttr(val)}"`;
    }
    let style = '';
    for (const k of Object.keys(this.style)) {
      const v = this.style[k];
      if (v !== '' && v != null) style += `${k}:${v};`;
    }
    if (style !== '') attrs += ` style="${escapeAttr(style)}"`;
    const kids = this.childNodes.map((c) => c.serialize()).join('');
    return `<${this.tagName}${attrs}>${kids}</${this.tagName}>`;
  }
}

// The DOM properties elm/virtual-dom assigns via `dom[key] = val`, reflected to
// serialized attributes. Anything outside this set is set as a plain field and
// ignored by serialization — but identically on both backends, so they agree.
const REFLECTED = [
  'className', 'htmlFor', 'id', 'value', 'checked', 'disabled', 'selected',
  'hidden', 'title', 'href', 'src', 'type', 'placeholder', 'name', 'tabIndex',
  'readOnly', 'multiple', 'required', 'autofocus', 'min', 'max', 'step',
  'cols', 'rows', 'wrap', 'dir', 'lang', 'alt', 'width', 'height', 'target',
  'rel', 'action', 'method', 'accept', 'pattern', 'for', 'class',
];
for (const key of REFLECTED) {
  const name = propToAttr(key);
  Object.defineProperty(Element.prototype, key, {
    configurable: true,
    get() { const v = this._attrs.get(name); return v == null ? '' : (v === true ? true : v); },
    set(val) {
      if (typeof val === 'boolean') { if (val) this._set(name, true); else this._del(name); }
      else if (val === '' || val == null) this._del(name);
      else this._set(name, String(val));
    },
  });
}

// The wasm shim delivers property assignments through here; same `el[key] = val`
// path the JS runtime uses, so reflection is identical.
function setProperty(el, key, val) {
  el[key] = val;
}
function unwrap(n) { return n; }

class Document {
  constructor() {
    this.title = '';
    this.body = new Element(this, 'body');
    this._domListeners = {};
  }
  createElement(tag) { return new Element(this, tag); }
  createElementNS(ns, tag) { return new Element(this, tag, ns); }
  createTextNode(text) { return new TextNode(this, text); }
  addEventListener(name, fn) { (this._domListeners[name] = this._domListeners[name] || []).push(fn); }
  removeEventListener(name, fn) {
    const l = this._domListeners[name];
    if (l) { const i = l.indexOf(fn); if (i >= 0) l.splice(i, 1); }
  }
}

// Fire an event at an element: run its listeners for `name` with `evt`.
function dispatchEvent(el, name, evt) {
  el = unwrap(el);
  evt = evt || {};
  evt.target = evt.target || el;
  evt.currentTarget = el;
  evt.preventDefault = evt.preventDefault || function () { evt.defaultPrevented = true; };
  evt.stopPropagation = evt.stopPropagation || function () {};
  for (const fn of (el.listeners[name] || []).slice()) fn(evt);
}

// Fire a document-level event (for Browser.Events subscriptions).
function dispatchDocEvent(doc, name, evt) {
  evt = evt || {};
  evt.preventDefault = evt.preventDefault || function () { evt.defaultPrevented = true; };
  evt.stopPropagation = evt.stopPropagation || function () {};
  for (const fn of (doc._domListeners[name] || []).slice()) fn(evt);
}

// Serialize the mounted tree under body.
function serializeBody(doc) {
  return doc.body.childNodes.map((c) => c.serialize()).join('');
}

module.exports = {
  Document, Element, TextNode, setProperty, dispatchEvent, dispatchDocEvent, serializeBody, unwrap,
};
