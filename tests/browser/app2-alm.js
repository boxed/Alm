(function () {
'use strict';

// alm runtime kernel — the subset of Elm's Kernel/*.js that alm's
// built-in modules need.

// CURRIED FUNCTION HELPERS

function F(arity, fun, wrapper) { wrapper.a = arity; wrapper.f = fun; return wrapper; }
function F2(fun) { return F(2, fun, function (a) { return function (b) { return fun(a, b); }; }); }
function F3(fun) { return F(3, fun, function (a) { return function (b) { return function (c) { return fun(a, b, c); }; }; }); }
function A2(f, a, b) { return f.a === 2 ? f.f(a, b) : f(a)(b); }
function A3(f, a, b, c) { return f.a === 3 ? f.f(a, b, c) : f(a)(b)(c); }
function A4(f, a, b, c, d) { return f.a === 4 ? f.f(a, b, c, d) : f(a)(b)(c)(d); }
var _List_LIMIT = 8192;
var _List_Nil = { $: '[]' };
// cons prepends into the head chunk's front slack IN PLACE when this list owns
// the frontmost slot (`d.hd` tracks it), so a run of conses shares one backing
// — amortized O(1) allocation, one chunk per up-to-_List_LIMIT elements, rather
// than an allocation per element. A cons onto a tail view (or a full/foreign
// chunk) can't reuse the slack (it would clobber another list's element), so it
// starts a fresh chunk — no O(n) copy either. Chunks grow geometrically.
function _List_fromArrayOnto(arr, tail) {
    if (arr.length === 0) { return tail; }
    return { $: '::', d: arr, o: 0, b: tail };
}
function _List_fromArray(arr) { return _List_fromArrayOnto(arr, _List_Nil); }
function _List_toArray(xs) {
    if (xs.$ !== '::') { return []; }
    // Single dense chunk: copy it out directly. Callers such as `List.sort`
    // mutate the result in place, so never return the shared backing.
    if (xs.b.$ === '[]') { return xs.o === 0 ? xs.d.slice() : xs.d.slice(xs.o); }
    var out = [];
    for (; xs.$ === '::'; xs = xs.b) {
        var d = xs.d;
        for (var i = xs.o, n = d.length; i < n; i++) { out.push(d[i]); }
    }
    return out;
}

// CHAR — dev builds box every Char as a `new String(c)` object so `Debug.toString`
// (and `instanceof String` checks) can tell a Char apart from a 1-char String.
function _Utils_eqHelp(x, y, depth, stack) {
    if (x === y) { return true; }
    // A `y === undefined` here means x has a field/index that y lacks (a shape
    // mismatch, e.g. comparing a non-empty array-backed Dict with Dict.empty):
    // they are unequal — guard so we don't recurse into `undefined` and throw.
    if (typeof x !== 'object' || x === null || y === null || y === undefined) { return false; }
    // Boxed chars compare by value: two `new String('a')` are equal.
    if (x instanceof String) { return x.valueOf() === y.valueOf(); }
    // Json decoders are opaque closures in alm. elm represents them as data and
    // compares structurally; `Json.succeed a == Json.succeed b` iff `a == b`.
    // Compare `succeed` decoders by their value; others by identity (the `run`
    // closures are functions, which are otherwise incomparable).
    if (x.$ === 'Decoder' || y.$ === 'Decoder') {
        if (!y || x.$ !== 'Decoder' || y.$ !== 'Decoder') { return false; }
        if (x.succeed && y.succeed) { return _Utils_eqHelp(x.value, y.value, depth + 1, stack); }
        return x.run === y.run;
    }
    // Lists compare by logical element sequence — chunk boundaries are not part
    // of a list's identity, so two equal lists may be chunked differently.
    if (x.$ === '::' || x.$ === '[]') {
        if (y.$ !== '::' && y.$ !== '[]') { return false; }
        var exi = x.$ === '::' ? x.o : 0;
        var eyi = y.$ === '::' ? y.o : 0;
        while (x.$ === '::' && y.$ === '::') {
            if (!_Utils_eqHelp(x.d[exi], y.d[eyi], depth + 1, stack)) { return false; }
            if (++exi >= x.d.length) { x = x.b; exi = x.$ === '::' ? x.o : 0; }
            if (++eyi >= y.d.length) { y = y.b; eyi = y.$ === '::' ? y.o : 0; }
        }
        return x.$ === '[]' && y.$ === '[]';
    }
    if (depth > 100) {
        stack.push({ $: '#2', a: x, b: y });
        return true;
    }
    // Dict/Set are red-black trees: two trees with identical contents can have
    // different shapes, so structural field comparison would wrongly disagree.
    // Compare by their canonical sorted list instead (as elm's kernel does).
    if (x.$ === 'Set_elm_builtin') {
        x = $Set$toList(x); y = $Set$toList(y);
    } else if (x.$ === 'RBNode_elm_builtin' || x.$ === 'RBEmpty_elm_builtin') {
        x = $Dict$toList(x); y = $Dict$toList(y);
    }
    for (var key in x) {
        if (!_Utils_eqHelp(x[key], y[key], depth + 1, stack)) { return false; }
    }
    for (var key2 in y) {
        if (!(key2 in x)) { return false; }
    }
    return true;
}

// COMPARISON — only ever called on comparable values

function _Utils_ap(x, y) {
    if (typeof x === 'string') { return x + y; }
    if (x.$ === '[]') { return y; }
    // Prepend x's elements onto y as dense chunks; the copy is bounded by |x|,
    // and y's spine is shared (not copied).
    return _List_fromArrayOnto(_List_toArray(x), y);
}

// RECORD UPDATE

function _Utils_update(oldRecord, updatedFields) {
    var newRecord = {};
    for (var key in oldRecord) { newRecord[key] = oldRecord[key]; }
    for (var key2 in updatedFields) { newRecord[key2] = updatedFields[key2]; }
    return newRecord;
}

// BASICS

var $Maybe$Nothing = { $: 'Nothing' };
var $Maybe$Just = function (a) { return { $: 'Just', a: a }; };
var $String$toInt = function (s) {
    // elm accepts an optional leading +/- and any run of digits (including
    // leading zeros: "01" -> Just 1); rejects empty / non-digit / bare sign.
    var total = 0;
    var code0 = s.charCodeAt(0);
    var start = code0 === 0x2B || code0 === 0x2D ? 1 : 0;
    var i = start;
    for (; i < s.length; ++i) {
        var code = s.charCodeAt(i);
        if (code < 0x30 || 0x39 < code) { return $Maybe$Nothing; }
        total = 10 * total + code - 0x30;
    }
    return i === start ? $Maybe$Nothing : $Maybe$Just(code0 === 0x2D ? -total : total);
};
var $String$fromInt = function (n) { return String(n); };
function _Debug_addSlashes(str, isChar) {
    var s = String(str)
        .replace(/\\/g, '\\\\')
        .replace(/\n/g, '\\n')
        .replace(/\t/g, '\\t')
        .replace(/\r/g, '\\r')
        .replace(/\v/g, '\\v')
        .replace(/\0/g, '\\0');
    return isChar ? s.replace(/'/g, "\\'") : s.replace(/"/g, '\\"');
}
function _Debug_toString(value) {
    if (value === true) { return 'True'; }
    if (value === false) { return 'False'; }
    if (typeof value === 'number') { return String(value); }
    // A boxed String object is elm's dev-build Char representation → single quotes.
    if (value instanceof String) { return "'" + _Debug_addSlashes(value.valueOf(), true) + "'"; }
    if (typeof value === 'string') { return '"' + _Debug_addSlashes(value, false) + '"'; }
    if (typeof value === 'function') { return '<function>'; }
    if (value === null || value === undefined) { return '<internal>'; }
    var tag = value.$;
    if (tag === '#0') { return '()'; }
    if (tag === '#2') { return '(' + _Debug_toString(value.a) + ',' + _Debug_toString(value.b) + ')'; }
    if (tag === '#3') {
        return '(' + _Debug_toString(value.a) + ',' + _Debug_toString(value.b) + ',' + _Debug_toString(value.c) + ')';
    }
    if (tag === '[]' || tag === '::') {
        return '[' + _List_toArray(value).map(_Debug_toString).join(',') + ']';
    }
    // Builtin Dict/Set/Array carry collision-proof `_elm_builtin` tags (alm's
    // parser forbids that suffix in user constructor names), so a user type
    // named `Dict`/`Set`/`Array` has tag `'Dict'`/`'Set'`/`'Array'` and falls
    // through to the generic custom-type rendering below.
    if (tag === 'RBNode_elm_builtin' || tag === 'RBEmpty_elm_builtin') {
        return 'Dict.fromList ' + _Debug_toString($Dict$toList(value));
    }
    if (tag === 'Set_elm_builtin') {
        return 'Set.fromList ' + _Debug_toString($Dict$keys(value.d));
    }
    if (tag === 'Array_elm_builtin') {
        return 'Array.fromList ' + _Debug_toString($Array$toList(value));
    }
    // Internal scheduler values (Tasks) render as `<internals>`, like elm — its
    // scheduler tags these with a number, ours with `'Task'` plus a `fork`
    // closure or numeric `tag`, which a user `Task` constructor never carries.
    if (tag === 'Task' && (typeof value.fork === 'function' || typeof value.tag === 'number')) {
        return '<internals>';
    }
    if (tag !== undefined) {
        var out = tag;
        for (var key in value) {
            if (key === '$') { continue; }
            var s = _Debug_toString(value[key]);
            out += ' ' + (/[ ]/.test(s) && s[0] !== '"' && s[0] !== '{' && s[0] !== '(' && s[0] !== '[' ? '(' + s + ')' : s);
        }
        return out;
    }
    // record — elm renders fields in alphabetical order, not definition order
    var names = [];
    for (var name in value) { names.push(name); }
    names.sort();
    var fields = names.map(function (n) { return n + ' = ' + _Debug_toString(value[n]); });
    return '{ ' + fields.join(', ') + ' }';
}
function _Dict_toArray(dict) {
    var out = [];
    var stack = [];
    var node = dict;
    while (node.$ === 'RBNode_elm_builtin' || stack.length) {
        while (node.$ === 'RBNode_elm_builtin') { stack.push(node); node = node.left; }
        node = stack.pop();
        out.push(node);
        node = node.right;
    }
    return out;
}
var $Dict$keys = function (dict) {
    return _List_fromArray(_Dict_toArray(dict).map(function (n) { return n.key; }));
};
var $Dict$toList = function (dict) {
    return _List_fromArray(_Dict_toArray(dict).map(function (n) { return { $: '#2', a: n.key, b: n.val }; }));
};
var $Set$toList = function (set) { return $Dict$keys(set.d); };
function _Array_toJsArray(array) {
    var out = [];
    var tree = array.c;
    function go(node) {
        if (node.$ === 'SubTree') { var t = node.a; for (var i = 0; i < t.length; i++) { go(t[i]); } }
        else { var v = node.a; for (var j = 0; j < v.length; j++) { out.push(v[j]); } }
    }
    for (var i = 0; i < tree.length; i++) { go(tree[i]); }
    var tail = array.d;
    for (var k = 0; k < tail.length; k++) { out.push(tail[k]); }
    return out;
}
// Build a (canonical) array from a plain JS array via repeated push.
var $Array$toList = function (array) {
    return _List_fromArray(_Array_toJsArray(array));
};
function _VDom_text(text) { return { $: 'VText', text: text }; }
// elm/virtual-dom's `_VirtualDom_organizeFacts` merges repeated `className`
// (and `class`) declarations into a single space-joined value (see
// `_VirtualDom_addClass`). alm keeps facts as an attribute list, so mirror that
// merge here — otherwise `Html.div [ class "a", class "b" ]` would not be
// structurally equal to a node built with a single `class "a b"`.
function _VDom_organize(attrs) {
    var classNames = 0;
    var classes = 0;
    for (var i = 0; i < attrs.length; i++) {
        var a = attrs[i];
        if (a.$ === 'AProp' && a.key === 'className') { classNames++; }
        else if (a.$ === 'AAttr' && a.key === 'class' && !a.ns) { classes++; }
    }
    if (classNames < 2 && classes < 2) { return attrs; }
    var out = [];
    var cnIdx = -1;
    var cIdx = -1;
    for (var j = 0; j < attrs.length; j++) {
        var b = attrs[j];
        if (b.$ === 'AProp' && b.key === 'className') {
            if (cnIdx === -1) { cnIdx = out.length; out.push(b); }
            else { var p = out[cnIdx]; out[cnIdx] = { $: 'AProp', key: 'className', val: p.val ? p.val + ' ' + b.val : b.val }; }
        } else if (b.$ === 'AAttr' && b.key === 'class' && !b.ns) {
            if (cIdx === -1) { cIdx = out.length; out.push(b); }
            else { var q = out[cIdx]; out[cIdx] = { $: 'AAttr', key: 'class', val: q.val ? q.val + ' ' + b.val : b.val }; }
        } else { out.push(b); }
    }
    return out;
}
// XSS ATTACK VECTOR CHECKS — elm/virtual-dom screens every tag, attribute key
// and URI that a program can build dynamically. The regexes look freaky
// because tabs may appear inside a href protocol and it still works, so
// '\tjava\tSCRIPT:alert(1)' and 'javascript:alert(1)' are the same in practice.
var _VDom_RE_js = /^\s*j\s*a\s*v\s*a\s*s\s*c\s*r\s*i\s*p\s*t\s*:/i;
var _VDom_XSS = 'javascript:alert("This is an XSS vector. Please use ports or web components instead.")';
function _VDom_noJavaScriptUri(value) { return _VDom_RE_js.test(value) ? _VDom_XSS : value; }
function _VDom_node(tag) {
    return F2(function (attrs, kids) {
        return { $: 'VNode', tag: tag, attrs: _VDom_organize(_List_toArray(attrs)), kids: _List_toArray(kids) };
    });
}
var $Html$text = _VDom_text;
function _VDom_forceLazy(vnode) {
    if (!vnode.forced) {
        var result = vnode.f;
        for (var i = 0; i < vnode.args.length; i++) { result = result(vnode.args[i]); }
        vnode.forced = result;
    }
    return vnode.forced;
}

function _VDom_sameLazy(a, b) {
    if (a.f !== b.f || a.args.length !== b.args.length) { return false; }
    for (var i = 0; i < a.args.length; i++) {
        if (a.args[i] !== b.args[i]) { return false; }
    }
    return true;
}

// Attributes are tagged with how they apply to a DOM node.
function _VDom_prop(key) {
    return function (val) { return { $: 'AProp', key: key, val: val }; };
}
// Events carry a Json decoder run against the DOM event, like real Elm.
// The decoder yields the message; `stop`/`prevent` control propagation.
function _VDom_on(name, decoder, opts) {
    return { $: 'AEvent', name: name, decoder: decoder, opts: opts };
}

function _Json_succeedDecoder(msg) {
    // `succeed`/`value` let structural equality compare two `succeed` decoders
    // by their produced value (elm represents decoders as data, so `==` works;
    // alm uses closures, so record the value for comparison — see _Utils_eqHelp).
    return { $: 'Decoder', run: function (_v) { return { ok: true, value: msg }; }, succeed: true, value: msg };
}

var $Html$Events$onClick = function (msg) { return _VDom_on('click', _Json_succeedDecoder(msg)); };
function _VDom_render(vnode, dispatch, doc) {
    switch (vnode.$) {
        case 'VText':
            return doc.createTextNode(vnode.text);
        case 'VMap': {
            var f = vnode.f;
            return _VDom_render(vnode.node, function (msg) { dispatch(f(msg)); }, doc);
        }
        case 'VLazy':
            return _VDom_render(_VDom_forceLazy(vnode), dispatch, doc);
        case 'VCustom': {
            // A managed widget (e.g. WebGL): `render(model)` builds the DOM
            // element; `attrs` are ordinary facts applied on top.
            var cdom = vnode.render(vnode.model);
            cdom._almListeners = {};
            for (var ci = 0; ci < vnode.attrs.length; ci++) {
                _VDom_applyAttr(cdom, vnode.attrs[ci], dispatch);
            }
            return cdom;
        }
        default: {
            var dom = vnode.ns && doc.createElementNS
                ? doc.createElementNS(vnode.ns, vnode.tag)
                : doc.createElement(vnode.tag);
            dom._almListeners = {};
            for (var i = 0; i < vnode.attrs.length; i++) {
                _VDom_applyAttr(dom, vnode.attrs[i], dispatch);
            }
            for (var k = 0; k < vnode.kids.length; k++) {
                var kid = vnode.$ === 'VKeyed' ? vnode.kids[k].b : vnode.kids[k];
                dom.appendChild(_VDom_render(kid, dispatch, doc));
            }
            return dom;
        }
    }
}

function _VDom_applyAttr(dom, attr, dispatch) {
    switch (attr.$) {
        case 'AStyle':
            dom.style[attr.key] = attr.val;
            return;
        case 'AAttr':
            if (attr.ns && dom.setAttributeNS) { dom.setAttributeNS(attr.ns, attr.key, attr.val); }
            else { dom.setAttribute(attr.key, attr.val); }
            return;
        case 'AProp':
            dom[attr.key] = attr.val;
            return;
        case 'AEvent': {
            var record = dom._almListeners[attr.name];
            if (!record) {
                record = dom._almListeners[attr.name] = {
                    handler: function (e) {
                        var opts = record.opts || {};
                        if (opts.preventDefault && e.preventDefault) { e.preventDefault(); }
                        var result = record.decoder.run(e);
                        if (!result.ok) { return; }
                        var msg = result.value;
                        if (opts.custom) {
                            if (msg.stopPropagation && e.stopPropagation) { e.stopPropagation(); }
                            if (msg.preventDefault && e.preventDefault) { e.preventDefault(); }
                            msg = msg.message;
                        } else if (opts.pair) {
                            // Decoder produced ( msg, Bool ).
                            var doIt = msg.b;
                            msg = msg.a;
                            if (doIt && opts.stopField && e.stopPropagation) { e.stopPropagation(); }
                            if (doIt && opts.preventField && e.preventDefault) { e.preventDefault(); }
                        }
                        record.dispatch(msg);
                    }
                };
                dom.addEventListener(attr.name, record.handler);
            }
            record.decoder = attr.decoder;
            record.opts = attr.opts;
            record.dispatch = dispatch;
            return;
        }
    }
}

function _VDom_attrKey(attr) {
    return attr.$ + ':' + (attr.key || attr.name);
}

// Whether the new attr differs from the old one with the same key, so patch can
// skip re-applying an unchanged attribute (avoids thousands of redundant
// className/setAttribute writes when a keyed list re-renders and only a couple
// of rows actually changed). Events always "change": re-applying refreshes the
// handler's decoder + dispatch (cheap — the listener is only attached once), and
// their decoder closures aren't value-comparable.
function _VDom_attrChanged(prev, attr) {
    switch (attr.$) {
        case 'AStyle': return prev.val !== attr.val;
        case 'AAttr':  return prev.val !== attr.val || prev.ns !== attr.ns;
        case 'AProp':  return prev.val !== attr.val;
        default:       return true; // AEvent
    }
}

function _VDom_unapplyAttr(dom, attr) {
    switch (attr.$) {
        case 'AStyle':
            dom.style[attr.key] = '';
            return;
        case 'AAttr':
            dom.removeAttribute(attr.key);
            return;
        case 'AProp':
            dom[attr.key] = typeof attr.val === 'boolean' ? false : '';
            return;
        case 'AEvent': {
            var record = dom._almListeners[attr.name];
            if (record) {
                dom.removeEventListener(attr.name, record.handler);
                delete dom._almListeners[attr.name];
            }
            return;
        }
    }
}

// PATCH — diff by position, mutating the existing DOM where possible.

function _VDom_patch(dom, oldV, newV, dispatch, doc) {
    if (oldV === newV) { return dom; }

    if (oldV.$ === 'VLazy' && newV.$ === 'VLazy' && _VDom_sameLazy(oldV, newV)) {
        newV.forced = oldV.forced;
        return dom;
    }
    if (oldV.$ === 'VLazy' || newV.$ === 'VLazy') {
        var oldForced = oldV.$ === 'VLazy' ? _VDom_forceLazy(oldV) : oldV;
        var newForced = newV.$ === 'VLazy' ? _VDom_forceLazy(newV) : newV;
        return _VDom_patch(dom, oldForced, newForced, dispatch, doc);
    }

    if (oldV.$ === 'VMap' && newV.$ === 'VMap') {
        var f = newV.f;
        return _VDom_patch(dom, oldV.node, newV.node, function (msg) { dispatch(f(msg)); }, doc);
    }

    if (oldV.$ === 'VText' && newV.$ === 'VText') {
        if (oldV.text !== newV.text) { dom.textContent = newV.text; }
        return dom;
    }

    // A managed widget with the same `render`: let its `diff` redraw the DOM in
    // place, then reconcile the plain facts. A different render (or kind) falls
    // through to a full replace below.
    if (oldV.$ === 'VCustom' && newV.$ === 'VCustom' && oldV.render === newV.render) {
        var cdom = newV.diff(oldV.model, newV.model)(dom);
        var oldCA = {};
        for (var ca = 0; ca < oldV.attrs.length; ca++) { oldCA[_VDom_attrKey(oldV.attrs[ca])] = oldV.attrs[ca]; }
        var newCK = {};
        for (var cb = 0; cb < newV.attrs.length; cb++) {
            var cattr = newV.attrs[cb], cak = _VDom_attrKey(cattr);
            newCK[cak] = true;
            var cprev = oldCA[cak];
            if (cprev === undefined || _VDom_attrChanged(cprev, cattr)) { _VDom_applyAttr(cdom, cattr, dispatch); }
        }
        for (var cak2 in oldCA) { if (!newCK[cak2]) { _VDom_unapplyAttr(cdom, oldCA[cak2]); } }
        return cdom;
    }

    if (oldV.$ !== newV.$ || oldV.tag !== newV.tag || oldV.ns !== newV.ns) {
        var replacement = _VDom_render(newV, dispatch, doc);
        dom.parentNode.replaceChild(replacement, dom);
        return replacement;
    }

    // Same tag: diff attributes. Fast path — when the attr lists line up by key
    // (the common case: same view code, only values differ), diff positionally
    // with NO per-node map allocation. This is the hot path of a keyed re-render
    // (thousands of nodes), so avoiding the `{}` + inserts + `for..in` per node
    // matters. Fall back to a keyed diff only when attrs were added/removed/
    // reordered (lengths or per-position keys differ).
    var oldA = oldV.attrs, newA = newV.attrs, i, aligned = oldA.length === newA.length;
    if (aligned) {
        for (i = 0; i < newA.length; i++) {
            var oa = oldA[i], na = newA[i];
            if (oa.$ !== na.$ || (oa.key || oa.name) !== (na.key || na.name)) { aligned = false; break; }
        }
    }
    if (aligned) {
        for (i = 0; i < newA.length; i++) {
            if (_VDom_attrChanged(oldA[i], newA[i])) { _VDom_applyAttr(dom, newA[i], dispatch); }
        }
    } else {
        var oldAttrs = {};
        for (i = 0; i < oldA.length; i++) { oldAttrs[_VDom_attrKey(oldA[i])] = oldA[i]; }
        var newKeys = {};
        for (i = 0; i < newA.length; i++) {
            var attr = newA[i], ak = _VDom_attrKey(attr);
            newKeys[ak] = true;
            var prev = oldAttrs[ak];
            if (prev === undefined || _VDom_attrChanged(prev, attr)) { _VDom_applyAttr(dom, attr, dispatch); }
        }
        for (var key in oldAttrs) {
            if (!newKeys[key]) { _VDom_unapplyAttr(dom, oldAttrs[key]); }
        }
    }

    if (oldV.$ === 'VKeyed') {
        return _VDom_patchKeyed(dom, oldV, newV, dispatch, doc);
    }

    // ...then children by index.
    var oldKids = oldV.kids, newKids = newV.kids;
    var shared = Math.min(oldKids.length, newKids.length);
    for (var k = 0; k < shared; k++) {
        _VDom_patch(dom.childNodes[k], oldKids[k], newKids[k], dispatch, doc);
    }
    for (var d = oldKids.length; d > newKids.length; d--) {
        dom.removeChild(dom.childNodes[d - 1]);
    }
    for (var n = oldKids.length; n < newKids.length; n++) {
        dom.appendChild(_VDom_render(newKids[n], dispatch, doc));
    }
    return dom;
}

// Longest increasing subsequence over `source` (old positions of the new
// children; -1 marks a freshly rendered node). Returns a boolean mask marking
// the reused children whose relative order is already correct, so they can stay
// put while everything else is moved into place. Standard patience-sorting LIS.
function _VDom_keyedStable(source) {
    var n = source.length;
    var stay = new Array(n);
    for (var x = 0; x < n; x++) { stay[x] = false; }
    var parent = new Array(n);
    var tails = []; // indices into source; source[tails[k]] increasing
    for (var i = 0; i < n; i++) {
        if (source[i] === -1) { continue; } // new node: never stays
        if (tails.length === 0) { tails.push(i); parent[i] = -1; continue; }
        var last = tails[tails.length - 1];
        if (source[last] < source[i]) { parent[i] = last; tails.push(i); continue; }
        var lo = 0, hi = tails.length - 1;
        while (lo < hi) {
            var mid = (lo + hi) >> 1;
            if (source[tails[mid]] < source[i]) { lo = mid + 1; } else { hi = mid; }
        }
        parent[i] = lo > 0 ? tails[lo - 1] : -1;
        tails[lo] = i;
    }
    var u = tails.length;
    var v = u > 0 ? tails[u - 1] : -1;
    while (u-- > 0) { stay[v] = true; v = parent[v]; }
    return stay;
}

function _VDom_patchKeyed(dom, oldV, newV, dispatch, doc) {
    // Reuse DOM nodes for matching keys and move only the ones that actually
    // moved. `source[j]` is the old index of new child j (-1 if freshly
    // rendered); the LIS of `source` is the set of nodes already in the right
    // relative order, which we leave untouched.
    var oldKids = oldV.kids, newKids = newV.kids;
    var n = newKids.length;

    // Fast path: identical keys in identical order — nothing moved (select,
    // update-in-place, an edit that doesn't reorder). Diff positionally with
    // ZERO allocation, skipping the oldByKey map, the source/used scratch and
    // the LIS pass. Mirrors the wasm reconciler: for an unchanged lazy child
    // (999/1000 on select) the reference compare short-circuits BEFORE touching
    // the DOM, so only the rows that actually changed cost a childNodes fetch +
    // patch. A same-key positional patch replaces the node in place if its tag
    // changed, so `kids` stays index-aligned throughout.
    if (n === oldKids.length) {
        var aligned = true;
        for (var s = 0; s < n; s++) {
            if (oldKids[s].a !== newKids[s].a) { aligned = false; break; }
        }
        if (aligned) {
            var kids = dom.childNodes;
            for (var p = 0; p < n; p++) {
                var ov = oldKids[p].b, nv = newKids[p].b;
                if (ov === nv) { continue; }
                if (ov.$ === 'VLazy' && nv.$ === 'VLazy' && _VDom_sameLazy(ov, nv)) { nv.forced = ov.forced; continue; }
                _VDom_patch(kids[p], ov, nv, dispatch, doc);
            }
            return dom;
        }
    }

    var oldByKey = {};
    for (var i = 0; i < oldKids.length; i++) {
        oldByKey[oldKids[i].a] = { vnode: oldKids[i].b, dom: dom.childNodes[i], index: i };
    }

    var newDoms = new Array(n);
    var source = new Array(n);
    var used = {};
    for (var j = 0; j < n; j++) {
        var key = newKids[j].a;
        var newKid = newKids[j].b;
        var old = !used[key] && oldByKey[key];
        if (old) {
            used[key] = true;
            source[j] = old.index;
            newDoms[j] = _VDom_patch(old.dom, old.vnode, newKid, dispatch, doc);
        } else {
            source[j] = -1;
            newDoms[j] = _VDom_render(newKid, dispatch, doc);
        }
    }

    // Drop DOM nodes whose key disappeared.
    for (var k = 0; k < oldKids.length; k++) {
        if (!used[oldKids[k].a]) {
            var gone = oldByKey[oldKids[k].a].dom;
            if (gone && gone.parentNode === dom) { dom.removeChild(gone); }
        }
    }

    // Insert/move from the end so `next` is always an already-placed node.
    var stay = _VDom_keyedStable(source);
    var next = null;
    for (var m = n - 1; m >= 0; m--) {
        var node = newDoms[m];
        if (source[m] === -1 || !stay[m]) {
            dom.insertBefore(node, next);
        }
        next = node;
    }
    return dom;
}

// JSON — Elm.Kernel.Json. Decoders are objects with a `run` function from
// a JS value to { ok: true, value } or { ok: false, error }.

function _Task_fork(task, ok, err) {
    if (task.tag === 0) { return ok(task.a); }
    if (task.tag === 1) { return err(task.a); }
    return task.fork(ok, err);
}
function _Time_posix(ms) { return { $: 'Posix', a: ms }; }

// Time kernel primitives. Posix/Zone, the calendar math, and `Time.every` now
// come from the bundled Time effect module (builtin_src/Time.elm); only these
// four primitives are kernel. `now`/`here`/`getZoneName` build the source ctor
// reps via the module's own `millisToPosix`/`customZone`/`Name`/`Offset`.
// `setInterval` is a cancellable timer task: its fork returns a canceller that
// `Process.kill` invokes when the Time manager drops a subscription.
function _Url_chompPort(protocol, params, frag, authority, path) {
    var i = authority.indexOf(':');
    if (i < 0) {
        return $Maybe$Just({ protocol: protocol, host: authority, port_: $Maybe$Nothing, path: path, query: params, fragment: frag });
    }
    var portNum = $String$toInt(authority.slice(i + 1));
    if (portNum.$ !== 'Just') { return $Maybe$Nothing; }
    return $Maybe$Just({ protocol: protocol, host: authority.slice(0, i), port_: portNum, path: path, query: params, fragment: frag });
}
function _Url_chompAfterAuthority(protocol, params, frag, authority, path) {
    if (authority === '') { return $Maybe$Nothing; }
    var i = authority.indexOf('@');
    if (i < 0) { return _Url_chompPort(protocol, params, frag, authority, path); }
    return _Url_chompPort(protocol, params, frag, authority.slice(i + 1), path);
}
function _Url_chompBeforeQuery(protocol, params, frag, str) {
    if (str === '') { return $Maybe$Nothing; }
    var i = str.indexOf('/');
    // elm defaults a pathless URL to "/" (so "https://x.com" -> path "/").
    if (i < 0) { return _Url_chompAfterAuthority(protocol, params, frag, str, '/'); }
    return _Url_chompAfterAuthority(protocol, params, frag, str.slice(0, i), str.slice(i));
}
function _Url_chompBeforeFragment(protocol, frag, str) {
    if (str === '') { return $Maybe$Nothing; }
    var i = str.indexOf('?');
    if (i < 0) { return _Url_chompBeforeQuery(protocol, $Maybe$Nothing, frag, str); }
    return _Url_chompBeforeQuery(protocol, $Maybe$Just(str.slice(i + 1)), frag, str.slice(0, i));
}
function _Url_chompAfterProtocol(protocol, str) {
    if (str === '') { return $Maybe$Nothing; }
    var i = str.indexOf('#');
    if (i < 0) { return _Url_chompBeforeFragment(protocol, $Maybe$Nothing, str); }
    return _Url_chompBeforeFragment(protocol, $Maybe$Just(str.slice(i + 1)), str.slice(0, i));
}
var $Url$fromString = function (str) {
    if (str.indexOf('http://') === 0) { return _Url_chompAfterProtocol({ $: 'Http' }, str.slice(7)); }
    if (str.indexOf('https://') === 0) { return _Url_chompAfterProtocol({ $: 'Https' }, str.slice(8)); }
    return $Maybe$Nothing;
};
var $Browser$Navigation$load = function (url) { return { $: 'CmdLoad', url: url }; };
var $Platform$Cmd$none = { $: 'CmdNone' };
var $Platform$Sub$none = { $: 'SubNone' };
var _Platform_portDefs = {};
var _Platform_effectManagers = {};

function _Platform_toEffect(map, taggers, value) {
    return A2(map, function (x) {
        for (var t = taggers; t; t = t.rest) { x = t.tagger(x); }
        return x;
    }, value);
}

function _Platform_gatherEffects(isCmd, bag, taggers, dict) {
    if (!bag) { return; }
    switch (bag.$) {
        case 'CmdNone': case 'SubNone': return;
        case 'CmdBatch':
            bag.cmds.forEach(function (b) { _Platform_gatherEffects(isCmd, b, taggers, dict); });
            return;
        case 'SubBatch':
            bag.subs.forEach(function (b) { _Platform_gatherEffects(isCmd, b, taggers, dict); });
            return;
        case 'CmdMap':
            _Platform_gatherEffects(isCmd, bag.cmd, { tagger: bag.f, rest: taggers }, dict);
            return;
        case 'SubMap':
            _Platform_gatherEffects(isCmd, bag.sub, { tagger: bag.f, rest: taggers }, dict);
            return;
        case 'Leaf': {
            var mgr = _Platform_effectManagers[bag.home];
            var effect = _Platform_toEffect(isCmd ? mgr.cmdMap : mgr.subMap, taggers, bag.value);
            var slot = dict[bag.home] || (dict[bag.home] = { cmds: [], subs: [] });
            (isCmd ? slot.cmds : slot.subs).push(effect);
            return;
        }
        default: return;
    }
}

// A manager process: init runs once to produce initial state; messages
// (`fx` batches from the app, or `self` messages from sendToSelf) are handled
// one at a time, each producing the next state via a CPS task.
function _Platform_instantiateManager(home, sendToApp) {
    var info = _Platform_effectManagers[home];
    var proc = { mailbox: [], state: undefined, ready: false, running: false, info: info };
    proc.router = { sendToApp: sendToApp, proc: proc };
    _Task_fork(info.init, function (initialState) {
        proc.state = initialState;
        proc.ready = true;
        _Platform_stepManager(proc);
    }, function () {});
    return proc;
}

function _Platform_stepManager(proc) {
    if (proc.running || !proc.ready || proc.mailbox.length === 0) { return; }
    proc.running = true;
    var msg = proc.mailbox.shift();
    var info = proc.info;
    var task;
    if (msg.type === 'self') {
        task = A3(info.onSelfMsg, proc.router, msg.value, proc.state);
    } else {
        task = (info.cmdMap && info.subMap)
            ? A4(info.onEffects, proc.router, msg.cmds, msg.subs, proc.state)
            : A3(info.onEffects, proc.router, info.cmdMap ? msg.cmds : msg.subs, proc.state);
    }
    _Task_fork(task, function (newState) {
        proc.state = newState;
        proc.running = false;
        _Platform_stepManager(proc);
    }, function () {
        proc.running = false;
        _Platform_stepManager(proc);
    });
}

function _Platform_sendToManager(proc, msg) {
    proc.mailbox.push(msg);
    _Platform_stepManager(proc);
}

var $Browser$application = function (impl) {
    return { $: 'Program', kind: 'application', impl: impl };
};
var $Browser$Navigation$pushUrl = F2(function (_key, url) {
    return { $: 'CmdNav', kind: 'push', url: url };
});
function _Browser_currentUrl() {
    var parsed = $Url$fromString(typeof location !== 'undefined' ? location.href : 'http://localhost/');
    return parsed.$ === 'Just' ? parsed.a : {
        protocol: { $: 'Http' }, host: 'localhost', port_: $Maybe$Nothing,
        path: '/', query: $Maybe$Nothing, fragment: $Maybe$Nothing
    };
}

// Run `fn` after the current synchronous frame. Elm defers a program's
// initial Cmd this way so that ports subscribed right after `init()` returns
// (the `app.ports.x.subscribe(...)` line) are registered before the Cmd fires.
function _Platform_defer(fn) {
    if (typeof queueMicrotask === 'function') { queueMicrotask(fn); }
    else { Promise.resolve().then(fn); }
}

function _Platform_wrap(value) {
    if (!value || value.$ !== 'Program') { return value; }
    return {
        init: function (opts) {
            return _Platform_initialize(value, opts || {});
        }
    };
}

// One compiled module's public object. elm puts the program's initializer
// directly on it (`Elm.Main.init(...)`); alm additionally exposes every
// top-level binding, so `init` is layered on last and a binding of that name
// never shadows the program.
function _Platform_module(exports, main) {
    if (main && main.$ === 'Program') { exports.init = _Platform_wrap(main).init; }
    return exports;
}

// Publish the bundle as `Elm` in whatever scope loaded it — `module.exports`
// under CommonJS, the global object in a browser. Two bundles loaded into one
// scope merge, so several `alm make` outputs can share a page.
function _Platform_export(scope, exports) {
    if (scope.Elm) {
        for (var name in exports) { scope.Elm[name] = exports[name]; }
    } else {
        scope.Elm = exports;
    }
}

function _Platform_initialize(program, opts) {
    var impl = program.impl;
    var doc = (opts.node && opts.node.ownerDocument) ||
        (typeof document !== 'undefined' ? document : null);
    var isSandbox = program.kind === 'sandbox';
    var isDocument = program.kind === 'document' || program.kind === 'application';

    var model;
    var initialCmd = null;
    if (isSandbox) {
        model = impl.init;
    } else if (program.kind === 'application') {
        var key = { $: 'Key' };
        var triple = A3(impl.init, opts.flags, _Browser_currentUrl(), key);
        model = triple.a;
        initialCmd = triple.b;
    } else {
        var pair = impl.init(opts.flags);
        model = pair.a;
        initialCmd = pair.b;
    }

    var lastTitle = null;
    function view(m) {
        if (!isDocument) { return impl.view(m); }
        var docView = impl.view(m);
        if (doc && docView.title !== lastTitle) {
            lastTitle = docView.title;
            doc.title = docView.title;
        }
        return {
            $: 'VNode', tag: 'div', attrs: [],
            kids: _List_toArray(docView.body)
        };
    }
    if (!impl.view) { view = null; }

    var vnode = null;
    var root = null;

    // Live subscription state.
    var activePortSubs = {};   // port name -> [handler]
    var activeDomSubs = [];    // { name, handler } attached to document
    var animationFrame = null;

    function dispatch(msg) {
        if (isSandbox) {
            model = A2(impl.update, msg, model);
        } else {
            var next = A2(impl.update, msg, model);
            model = next.a;
            runCmd(next.b, function (m) { return m; });
            enqueueManagerEffects(next.b);
        }
        if (view) {
            var newVnode = view(model);
            root = _VDom_patch(root, vnode, newVnode, dispatch, doc);
            vnode = newVnode;
        }
        updateSubs();
    }

    // Effect-manager processes for this program instance (empty for the common
    // case of no `effect module`s). Manager definitions are global; each program
    // gets its own processes so two mounted programs do not share state.
    var managerHomes = Object.keys(_Platform_effectManagers);
    var managerProcs = {};
    for (var _mi = 0; _mi < managerHomes.length; _mi++) {
        managerProcs[managerHomes[_mi]] = _Platform_instantiateManager(managerHomes[_mi], dispatch);
    }
    function enqueueManagerEffects(cmdBag) {
        if (managerHomes.length === 0) { return; }
        var subBag = (!isSandbox && impl.subscriptions) ? impl.subscriptions(model) : null;
        var dict = {};
        _Platform_gatherEffects(true, cmdBag, null, dict);
        _Platform_gatherEffects(false, subBag, null, dict);
        for (var i = 0; i < managerHomes.length; i++) {
            var home = managerHomes[i];
            var fx = dict[home] || { cmds: [], subs: [] };
            _Platform_sendToManager(managerProcs[home], {
                type: 'fx',
                cmds: _List_fromArray(fx.cmds),
                subs: _List_fromArray(fx.subs)
            });
        }
    }

    function runCmd(cmd, tagger) {
        if (!cmd) { return; }
        switch (cmd.$) {
            case 'CmdNone': return;
            case 'CmdBatch': cmd.cmds.forEach(function (c) { runCmd(c, tagger); }); return;
            case 'CmdMap': {
                var f = cmd.f;
                runCmd(cmd.cmd, function (m) { return tagger(f(m)); });
                return;
            }
            case 'CmdPort': {
                var def = _Platform_portDefs[cmd.name];
                if (def) {
                    def.subscribers.slice().forEach(function (fn) { fn(cmd.value); });
                }
                return;
            }
            case 'CmdWrite':
                console.log(cmd.s);
                return;
            case 'CmdTask':
                _Task_fork(cmd.task,
                    function (msg) { dispatch(tagger(msg)); },
                    function (x) {
                        throw new Error('Task failed without an error handler: ' + _Debug_toString(x));
                    }
                );
                return;
            case 'CmdLoad':
                if (typeof window !== 'undefined') { window.location.href = cmd.url; }
                return;
            case 'CmdReload':
                if (typeof window !== 'undefined') { window.location.reload(); }
                return;
            case 'CmdNav': {
                if (typeof history === 'undefined') { return; }
                if (cmd.kind === 'push') {
                    history.pushState({}, '', cmd.url);
                    dispatch(impl.onUrlChange(_Browser_currentUrl()));
                } else if (cmd.kind === 'replace') {
                    history.replaceState({}, '', cmd.url);
                    dispatch(impl.onUrlChange(_Browser_currentUrl()));
                } else {
                    history.go(cmd.n); // popstate will fire onUrlChange
                }
                return;
            }
        }
    }

    function collectSubs(sub, tagger, acc) {
        if (!sub) { return; }
        switch (sub.$) {
            case 'SubNone': return;
            case 'SubBatch': sub.subs.forEach(function (s) { collectSubs(s, tagger, acc); }); return;
            case 'SubMap': {
                var f = sub.f;
                collectSubs(sub.sub, function (m) { return tagger(f(m)); }, acc);
                return;
            }
            case 'SubPort': {
                (acc.ports[sub.name] = acc.ports[sub.name] || []).push(function (jsValue) {
                    dispatch(tagger(sub.toMsg(sub.converter(jsValue))));
                });
                return;
            }
            case 'SubDom':
                acc.dom.push({ name: sub.name, decoder: sub.decoder, tagger: tagger });
                return;
            case 'SubAnimation':
                acc.animation.push({ toMsg: sub.toMsg, delta: sub.delta, tagger: tagger });
                return;
        }
    }

    function updateSubs() {
        var acc = { ports: {}, dom: [], animation: [] };
        if (!isSandbox && impl.subscriptions) {
            collectSubs(impl.subscriptions(model), function (m) { return m; }, acc);
        }
        activePortSubs = acc.ports;

        // Document-level DOM listeners: drop and re-add (simple and correct).
        if (doc && doc.addEventListener) {
            activeDomSubs.forEach(function (record) {
                doc.removeEventListener(record.name, record.handler);
            });
            activeDomSubs = acc.dom.map(function (spec) {
                var handler = function (e) {
                    var r = spec.decoder.run(e);
                    if (r.ok) { dispatch(spec.tagger(r.value)); }
                };
                doc.addEventListener(spec.name, handler);
                return { name: spec.name, handler: handler };
            });
        }

        // Time subscriptions are handled by the Time effect manager (see the
        // bundled Time effect module), not here.

        // Animation frames.
        if (animationFrame) {
            (typeof cancelAnimationFrame !== 'undefined' ? cancelAnimationFrame : clearTimeout)(animationFrame.id);
            animationFrame = null;
        }
        if (acc.animation.length > 0) {
            var raf = typeof requestAnimationFrame !== 'undefined'
                ? requestAnimationFrame
                : function (fn) { return setTimeout(function () { fn(Date.now()); }, 16); };
            var last = Date.now();
            var loop = function () {
                var now = Date.now();
                var delta = now - last;
                last = now;
                acc.animation.forEach(function (spec) {
                    dispatch(spec.tagger(spec.toMsg(spec.delta ? delta : _Time_posix(now))));
                });
                if (animationFrame) { animationFrame.id = raf(loop); }
            };
            animationFrame = { id: raf(loop) };
        }
    }

    if (view) {
        vnode = view(model);
        root = _VDom_render(vnode, dispatch, doc);
        if (isDocument) {
            // Browser.document/application own the page: mount a root
            // container into <body>.
            doc.body.appendChild(root);
        } else if (opts.node) {
            if (opts.node.parentNode) {
                opts.node.parentNode.replaceChild(root, opts.node);
            } else {
                opts.node.appendChild(root);
            }
        } else {
            throw new Error('This program needs a DOM node: Elm.Main.init({ node: ... })');
        }
    }

    if (program.kind === 'application' && doc && doc.addEventListener) {
        // Intercept plain left-clicks on same-origin links.
        doc.addEventListener('click', function (e) {
            if (e.defaultPrevented || e.button !== 0 || e.ctrlKey || e.metaKey || e.shiftKey) {
                return;
            }
            var t = e.target;
            while (t && t.tagName !== 'A') { t = t.parentNode; }
            if (!t || !t.href || t.hasAttribute('download') || t.getAttribute('target')) {
                return;
            }
            e.preventDefault();
            var parsed = $Url$fromString(t.href);
            var sameOrigin = typeof location !== 'undefined' &&
                t.href.indexOf(location.origin + '/') === 0;
            dispatch(impl.onUrlRequest(
                sameOrigin && parsed.$ === 'Just'
                    ? { $: 'Internal', a: parsed.a }
                    : { $: 'External', a: t.href }
            ));
        });
        if (typeof window !== 'undefined') {
            window.addEventListener('popstate', function () {
                dispatch(impl.onUrlChange(_Browser_currentUrl()));
            });
        }
    }

    updateSubs();
    // Defer the initial Cmd so a port subscriber attached synchronously after
    // `init()` returns receives values the Cmd sends (matching Elm).
    if (initialCmd) {
        _Platform_defer(function () { runCmd(initialCmd, function (m) { return m; }); });
    }
    // Deliver the initial batch of manager effects (initial Cmd + subscriptions),
    // deferred like the initial Cmd so port subscribers attached right after
    // `init()` returns are in place before a manager can call back into the app.
    if (managerHomes.length > 0) {
        _Platform_defer(function () { enqueueManagerEffects(initialCmd); });
    }

    // The app.ports API.
    var ports = {};
    Object.keys(_Platform_portDefs).forEach(function (name) {
        var def = _Platform_portDefs[name];
        if (def.direction === 'outgoing') {
            ports[name] = {
                subscribe: function (fn) { def.subscribers.push(fn); },
                unsubscribe: function (fn) {
                    var i = def.subscribers.indexOf(fn);
                    if (i > -1) { def.subscribers.splice(i, 1); }
                }
            };
        } else {
            ports[name] = {
                send: function (value) {
                    (activePortSubs[name] || []).slice().forEach(function (fn) { fn(value); });
                }
            };
        }
    });

    return { ports: ports };
}

// HIGHER-ARITY CURRY HELPERS
var $Html$div = _VDom_node('div');
var $Html$a = _VDom_node('a');
var $Html$button = _VDom_node('button');
var $Html$Attributes$href = function (v) { return { $: 'AProp', key: 'href', val: _VDom_noJavaScriptUri(v) }; };
var $Html$Attributes$id = _VDom_prop('id');
var $App$UrlRequested = function (a) { return { $: 'UrlRequested', a: a }; };
var $App$UrlChanged = function (a) { return { $: 'UrlChanged', a: a }; };
var $App$GoThree = { $: 'GoThree' };

var $App$init = F3(function (_v1, url, key) { return { $: '#2', a: { key: key, path: url.path, changes: 0 }, b: $Platform$Cmd$none }; });
var $App$update = F2(function (msg, model) { var _v2 = msg; switch (_v2.$) { case 'UrlRequested': { var _v3 = _v2.a; switch (_v3.$) { case 'Internal': { var url = _v2.a.a; return { $: '#2', a: model, b: A2($Browser$Navigation$pushUrl, model.key, url.path) }; } case 'External': { var href = _v2.a.a; return { $: '#2', a: model, b: $Browser$Navigation$load(href) }; } default: { throw new Error('Missing case branch (compiler bug: exhaustiveness checking should have caught this)'); } } } case 'UrlChanged': { var url = _v2.a; return { $: '#2', a: _Utils_update(model, { path: url.path, changes: (model.changes + 1) }), b: $Platform$Cmd$none }; } case 'GoThree': { return { $: '#2', a: model, b: A2($Browser$Navigation$pushUrl, model.key, '/three') }; } default: { throw new Error('Missing case branch (compiler bug: exhaustiveness checking should have caught this)'); } } });
var $App$view = function (model) { return { title: _Utils_ap('page:', model.path), body: _List_fromArray([A2($Html$div, _List_fromArray([$Html$Attributes$id('path')]), _List_fromArray([$Html$text(model.path)])), A2($Html$div, _List_fromArray([$Html$Attributes$id('changes')]), _List_fromArray([$Html$text($String$fromInt(model.changes))])), A2($Html$a, _List_fromArray([$Html$Attributes$id('link-two'), $Html$Attributes$href('/two')]), _List_fromArray([$Html$text('to two')])), A2($Html$button, _List_fromArray([$Html$Attributes$id('go-three'), $Html$Events$onClick($App$GoThree)]), _List_fromArray([$Html$text('to three')]))]) }; };
var $App$main = $Browser$application({ init: $App$init, update: $App$update, subscriptions: function (_v4) { return $Platform$Sub$none; }, view: $App$view, onUrlRequest: $App$UrlRequested, onUrlChange: $App$UrlChanged });

var Elm = { 'App': _Platform_module({ 'init': _Platform_wrap($App$init), 'update': _Platform_wrap($App$update), 'view': _Platform_wrap($App$view), 'main': _Platform_wrap($App$main) }, $App$main) };
_Platform_export(this, Elm);
}).call(this);
