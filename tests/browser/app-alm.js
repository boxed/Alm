(function () {
'use strict';

// alm runtime kernel — the subset of Elm's Kernel/*.js that alm's
// built-in modules need.

// CURRIED FUNCTION HELPERS

function F(arity, fun, wrapper) { wrapper.a = arity; wrapper.f = fun; return wrapper; }
function F2(fun) { return F(2, fun, function (a) { return function (b) { return fun(a, b); }; }); }
function _Fn(arity, fun) {
    function curried(args) {
        return function (x) {
            var next = args.concat([x]);
            return next.length === arity ? fun.apply(null, next) : curried(next);
        };
    }
    var wrapper = curried([]);
    wrapper.a = arity;
    wrapper.f = fun;
    return wrapper;
}
// Record type-alias constructors used as first-class values (e.g. `map Point xs`)
// must be a single shared function so `(==)` on values built from them matches
// elm (elm emits one top-level constructor; a fresh closure per use would make
// two equal records compare unequal). Memoize by the comma-joined field list.
var _Record_ctorCache = {};
function _Record_ctor(fieldsCsv) {
    var cached = _Record_ctorCache[fieldsCsv];
    if (cached !== undefined) { return cached; }
    var fields = fieldsCsv.length === 0 ? [] : fieldsCsv.split(',');
    var n = fields.length;
    var fn = _Fn(n, function () {
        var rec = {};
        for (var i = 0; i < n; i++) { rec[fields[i]] = arguments[i]; }
        return rec;
    });
    _Record_ctorCache[fieldsCsv] = fn;
    return fn;
}
function A2(f, a, b) { return f.a === 2 ? f.f(a, b) : f(a)(b); }
function A3(f, a, b, c) { return f.a === 3 ? f.f(a, b, c) : f(a)(b)(c); }
function A4(f, a, b, c, d) { return f.a === 4 ? f.f(a, b, c, d) : f(a)(b)(c)(d); }
var _Utils_Tuple0 = { $: '#0' };

// LISTS — an unrolled linked list: a spine of chunks, each a dense array of up
// to `_List_LIMIT` elements. `cons` prepends a singleton chunk (O(1), immutable,
// no cons-after-tail copy), while bulk builders (fromArray/map/filter/range/...)
// pack dense chunks so iteration runs contiguously through memory — most of a
// vector's speed without a vector's O(n) cons. Chunk boundaries are NOT part of a
// list's identity: equality, ordering and pattern matching compare the logical
// element sequence, so `[1,2,3]` built by cons and by fromArray are equal.
// A chunk node is `{ $: '::', d: <Array>, o: <first live index>, b: <tail> }`
// with the invariant `o < d.length` (no empty chunks); `d` is never mutated once
// the node is observable. Nil is `{ $: '[]' }`.

var _List_LIMIT = 8192;
var _List_Nil = { $: '[]' };
// cons prepends into the head chunk's front slack IN PLACE when this list owns
// the frontmost slot (`d.hd` tracks it), so a run of conses shares one backing
// — amortized O(1) allocation, one chunk per up-to-_List_LIMIT elements, rather
// than an allocation per element. A cons onto a tail view (or a full/foreign
// chunk) can't reuse the slack (it would clobber another list's element), so it
// starts a fresh chunk — no O(n) copy either. Chunks grow geometrically.
function _List_Cons(hd, tl) {
    if (tl.$ === '::') {
        var d = tl.d, o = tl.o;
        if (o > 0 && d.hd === o) {
            d[o - 1] = hd;
            d.hd = o - 1;
            return { $: '::', d: d, o: o - 1, b: tl.b };
        }
    }
    var prev = tl.$ === '::' ? tl.d.length - tl.o : 0;
    var cap = prev < 4 ? 8 : prev * 2;
    if (cap > _List_LIMIT) { cap = _List_LIMIT; }
    var nd = new Array(cap);
    nd[cap - 1] = hd;
    nd.hd = cap - 1;
    return { $: '::', d: nd, o: cap - 1, b: tl };
}
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

var $Basics$not = function (b) { return !b; };
var $Maybe$Nothing = { $: 'Nothing' };
var $Maybe$Just = function (a) { return { $: 'Just', a: a }; };
var $List$map = F2(function (f, xs) {
    var out = [];
    for (; xs.$ === '::'; xs = xs.b) { var d = xs.d; for (var i = xs.o, n = d.length; i < n; i++) { out.push(f(d[i])); } }
    return _List_fromArray(out);
});
var $List$reverse = function (xs) {
    var out = _List_toArray(xs);
    out.reverse();
    return _List_fromArray(out);
};
var $List$take = F2(function (n, xs) {
    var out = [];
    for (; n > 0 && xs.$ === '::'; xs = xs.b) {
        var d = xs.d;
        for (var i = xs.o, m = d.length; i < m && n > 0; i++, n--) { out.push(d[i]); }
    }
    return _List_fromArray(out);
});
var $List$drop = F2(function (n, xs) {
    while (n > 0 && xs.$ === '::') {
        var avail = xs.d.length - xs.o;
        if (n < avail) { return { $: '::', d: xs.d, o: xs.o + n, b: xs.b }; }
        n -= avail;
        xs = xs.b;
    }
    return xs;
});
var $String$reverse = function (s) { return Array.from(s).reverse().join(''); };
var $String$join = F2(function (sep, xs) { return _List_toArray(xs).join(sep); });
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
var _VDom_RE_script = /^script$/i;
function _VDom_noScript(tag) { return _VDom_RE_script.test(tag) ? 'p' : tag; }
function _VDom_node(tag) {
    return F2(function (attrs, kids) {
        return { $: 'VNode', tag: tag, attrs: _VDom_organize(_List_toArray(attrs)), kids: _List_toArray(kids) };
    });
}
function _VDom_nodeNS(tag) {
    return F2(function (attrs, kids) {
        return {
            $: 'VNode', tag: tag, ns: 'http://www.w3.org/2000/svg',
            attrs: _VDom_organize(_List_toArray(attrs)), kids: _List_toArray(kids)
        };
    });
}

var $Html$text = _VDom_text;
var $Html$map = F2(function (f, vnode) { return { $: 'VMap', f: f, node: vnode }; });
var $Html$Keyed$node = function (tag) {
    tag = _VDom_noScript(tag);
    return F2(function (attrs, keyedKids) {
        return {
            $: 'VKeyed', tag: tag, attrs: _VDom_organize(_List_toArray(attrs)),
            kids: _List_toArray(keyedKids) // (key, node) tuples
        };
    });
};
var $Html$Keyed$ul = $Html$Keyed$node('ul');
var $Html$Lazy$lazy = F2(function (f, a) { return { $: 'VLazy', f: f, args: [a] }; });
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
var $Html$Attributes$style = F2(function (key, val) { return { $: 'AStyle', key: key, val: val }; });
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

var $Html$Events$stopPropagationOn = F2(function (name, decoder) {
    return _VDom_on(name, decoder, { pair: true, stopField: true });
});
var $Html$Events$onClick = function (msg) { return _VDom_on('click', _Json_succeedDecoder(msg)); };
var $Html$Events$custom = F2(function (name, decoder) {
    return _VDom_on(name, decoder, { custom: true });
});
var $Html$Events$onInput = function (toMsg) {
    return _VDom_on('input', {
        $: 'Decoder',
        run: function (e) { return { ok: true, value: toMsg(e.target.value) }; }
    });
};
var $Html$Events$onCheck = function (toMsg) {
    return _VDom_on('change', {
        $: 'Decoder',
        run: function (e) { return { ok: true, value: toMsg(e.target.checked) }; }
    });
};
var $Html$Events$onSubmit = function (msg) {
    return _VDom_on('submit', _Json_succeedDecoder(msg), { preventDefault: true });
};

// RENDER — build a real DOM node from a virtual node.

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

function _Json_ok(value) { return { ok: true, value: value }; }
var $Json$Decode$succeed = function (x) {
    return { $: 'Decoder', run: function (_v) { return _Json_ok(x); }, succeed: true, value: x };
};
function _Task(fork) { return { $: 'Task', fork: fork }; }
// `Task.succeed`/`Task.fail` are pure DATA (tag 0/1 + value), like elm's
// scheduler, so `Task.succeed x == Task.succeed x` is structurally true. Only
// these carry no closure; every other task is a CPS `{ fork }`. `_Task_fork`
// dispatches so the execution model is unchanged.
function _Task_fork(task, ok, err) {
    if (task.tag === 0) { return ok(task.a); }
    if (task.tag === 1) { return err(task.a); }
    return task.fork(ok, err);
}
var $Task$map = F2(function (f, task) {
    return _Task(function (ok, err) {
        _Task_fork(task, function (a) { ok(f(a)); }, err);
    });
});
var $Task$perform = F2(function (toMsg, task) {
    return { $: 'CmdTask', task: A2($Task$map, toMsg, task) };
});
var $Process$sleep = function (ms) {
    return _Task(function (ok, _err) {
        setTimeout(function () { ok(_Utils_Tuple0); }, ms);
    });
};

// WEBGL TEXTURE (elm-explorations/webgl Elm.Kernel.Texture). `load` fetches an
// image (`new Image()`), and — on first use by the renderer — uploads it to a GL
// texture (`createTexture`, stashed on the returned value; see the SAMPLER_2D
// uniform setter above). It needs a browser (DOM Image + a WebGL context), like
// stock elm. The magnify/minify/wrap args are GL enums; `flipY` a Bool. Non-
// power-of-two sizes are rejected unless clamped + non-mipmapped (a SizeError).
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
var $Platform$Cmd$none = { $: 'CmdNone' };
var _Platform_portDefs = {};
function _Port_id(v) { return v; }
function _Platform_outgoingPort(name, converter) {
    _Platform_portDefs[name] = { direction: 'outgoing', subscribers: [] };
    return function (payload) {
        return { $: 'CmdPort', name: name, value: converter(payload) };
    };
}
function _Platform_incomingPort(name, converter) {
    _Platform_portDefs[name] = { direction: 'incoming', converter: converter };
    return function (toMsg) {
        return { $: 'SubPort', name: name, toMsg: toMsg, converter: converter };
    };
}

// EFFECT MANAGERS
//
// A port of elm's `_Platform` effect-manager protocol onto alm's CPS `_Task`
// model. `command`/`subscription` produce `Leaf` bags tagged with a manager
// `home`; each `effect module` registers a manager here. At program start each
// manager becomes a long-lived "process" with private state and a mailbox.
// Every update we gather the manager's Cmd/Sub leaves and deliver them to its
// `onEffects`; `sendToSelf` posts to its own mailbox, `sendToApp` feeds the app.
// alm's own effects (Http/Time/ports/...) stay concrete and never come here.

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

var $Browser$element = function (impl) {
    return { $: 'Program', kind: 'element', impl: impl };
};
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
var $Html$span = _VDom_node('span');
var $Html$li = _VDom_node('li');
var $Html$form = _VDom_node('form');
var $Html$input = _VDom_node('input');
var $Html$button = _VDom_node('button');
var $Html$Attributes$_class = _VDom_prop('className');
var $Html$Attributes$id = _VDom_prop('id');
var $Html$Attributes$type_ = _VDom_prop('type');
var $Html$Attributes$value = _VDom_prop('value');
var $Html$Attributes$checked = _VDom_prop('checked');
var $Html$Attributes$disabled = _VDom_prop('disabled');
var $Svg$svg = _VDom_nodeNS('svg');
var $Svg$circle = _VDom_nodeNS('circle');
var $Svg$Attributes$cx = function (v) { return { $: 'AAttr', key: 'cx', val: v }; };
var $Svg$Attributes$cy = function (v) { return { $: 'AAttr', key: 'cy', val: v }; };
var $Svg$Attributes$fill = function (v) { return { $: 'AAttr', key: 'fill', val: v }; };
var $Svg$Attributes$r = function (v) { return { $: 'AAttr', key: 'r', val: v }; };
var $Svg$Attributes$viewBox = function (v) { return { $: 'AAttr', key: 'viewBox', val: v }; };
var $Svg$Attributes$width = function (v) { return { $: 'AAttr', key: 'width', val: v }; };
var $Main$Increment = { $: 'Increment' };
var $Main$ChildIncrement = function (a) { return { $: 'ChildIncrement', a: a }; };
var $Main$Reorder = { $: 'Reorder' };
var $Main$InsertFront = { $: 'InsertFront' };
var $Main$RemoveSecond = { $: 'RemoveSecond' };
var $Main$TextChanged = function (a) { return { $: 'TextChanged', a: a }; };
var $Main$CheckboxChanged = function (a) { return { $: 'CheckboxChanged', a: a }; };
var $Main$FormSubmitted = { $: 'FormSubmitted' };
var $Main$OuterClicked = { $: 'OuterClicked' };
var $Main$InnerClicked = { $: 'InnerClicked' };
var $Main$CustomEvent = function (a) { return { $: 'CustomEvent', a: a }; };
var $Main$Slept = function (a) { return { $: 'Slept', a: a }; };
var $Main$GotFromJs = function (a) { return { $: 'GotFromJs', a: a }; };
var $Main$TogglePanel = { $: 'TogglePanel' };
var $Main$ToggleStyle = { $: 'ToggleStyle' };

var $Main$Poke = { $: 'Poke' };

var $Main$fromJs = _Platform_incomingPort('fromJs', _Port_id);
var $Main$toJs = _Platform_outgoingPort('toJs', _Port_id);
var $Main$init = function (_v1) { return { $: '#2', a: { count: 0, items: _List_fromArray([A2(_Record_ctor('key,label'), 'a', 'alpha'), A2(_Record_ctor('key,label'), 'b', 'beta'), A2(_Record_ctor('key,label'), 'c', 'gamma')]), textValue: '', checkboxOn: false, submitted: 0, outerClicks: 0, innerClicks: 0, customLog: _List_Nil, slept: false, portEcho: '', showPanel: false, styleToggle: false }, b: A2($Task$perform, $Main$Slept, $Process$sleep(30)) }; };
var $Main$update = F2(function (msg, model) { var _v2 = msg; switch (_v2.$) { case 'Increment': { return { $: '#2', a: _Utils_update(model, { count: (model.count + 1) }), b: $Platform$Cmd$none }; } case 'ChildIncrement': { return { $: '#2', a: _Utils_update(model, { count: (model.count + 10) }), b: $Platform$Cmd$none }; } case 'Reorder': { return { $: '#2', a: _Utils_update(model, { items: $List$reverse(model.items) }), b: $Platform$Cmd$none }; } case 'InsertFront': { return { $: '#2', a: _Utils_update(model, { items: _List_Cons(A2(_Record_ctor('key,label'), 'new', 'newcomer'), model.items) }), b: $Platform$Cmd$none }; } case 'RemoveSecond': { return { $: '#2', a: _Utils_update(model, { items: _Utils_ap(A2($List$take, 1, model.items), A2($List$drop, 2, model.items)) }), b: $Platform$Cmd$none }; } case 'TextChanged': { var s = _v2.a; return { $: '#2', a: _Utils_update(model, { textValue: s }), b: $Platform$Cmd$none }; } case 'CheckboxChanged': { var b = _v2.a; return { $: '#2', a: _Utils_update(model, { checkboxOn: b }), b: $Platform$Cmd$none }; } case 'FormSubmitted': { return { $: '#2', a: _Utils_update(model, { submitted: (model.submitted + 1) }), b: $Platform$Cmd$none }; } case 'OuterClicked': { return { $: '#2', a: _Utils_update(model, { outerClicks: (model.outerClicks + 1) }), b: $Platform$Cmd$none }; } case 'InnerClicked': { return { $: '#2', a: _Utils_update(model, { innerClicks: (model.innerClicks + 1) }), b: $Platform$Cmd$none }; } case 'CustomEvent': { var tag = _v2.a; return { $: '#2', a: _Utils_update(model, { customLog: _List_Cons(tag, model.customLog) }), b: $Platform$Cmd$none }; } case 'Slept': { return { $: '#2', a: _Utils_update(model, { slept: true }), b: $Platform$Cmd$none }; } case 'GotFromJs': { var s = _v2.a; return { $: '#2', a: _Utils_update(model, { portEcho: s }), b: $Main$toJs(_Utils_ap('echo:', s)) }; } case 'TogglePanel': { return { $: '#2', a: _Utils_update(model, { showPanel: $Basics$not(model.showPanel) }), b: $Platform$Cmd$none }; } case 'ToggleStyle': { return { $: '#2', a: _Utils_update(model, { styleToggle: $Basics$not(model.styleToggle) }), b: $Platform$Cmd$none }; } default: { throw new Error('Missing case branch (compiler bug: exhaustiveness checking should have caught this)'); } } });
var $Main$subscriptions = function (_v3) { return $Main$fromJs($Main$GotFromJs); };
var $Main$viewItem = function (item) { return A2($Html$li, _List_fromArray([$Html$Attributes$_class('item')]), _List_fromArray([$Html$text(item.label)])); };
var $Main$viewBadge = function (n) { return A2($Html$span, _List_fromArray([$Html$Attributes$id('lazy-badge')]), _List_fromArray([$Html$text(_Utils_ap('badge:', $String$fromInt(n)))])); };
var $Main$childView = A2($Html$button, _List_fromArray([$Html$Attributes$id('child-button'), $Html$Events$onClick($Main$Poke)]), _List_fromArray([$Html$text('poke')]));
var $Main$view = function (model) { return A2($Html$div, _List_fromArray([$Html$Attributes$id('app-root')]), _List_fromArray([A2($Html$div, _List_fromArray([$Html$Attributes$id('count')]), _List_fromArray([$Html$text($String$fromInt(model.count))])), A2($Html$button, _List_fromArray([$Html$Attributes$id('inc'), $Html$Events$onClick($Main$Increment)]), _List_fromArray([$Html$text('+')])), A2($Html$map, $Main$ChildIncrement, $Main$childView), A2($Html$button, _List_fromArray([$Html$Attributes$id('reorder'), $Html$Events$onClick($Main$Reorder)]), _List_fromArray([$Html$text('reorder')])), A2($Html$button, _List_fromArray([$Html$Attributes$id('insert'), $Html$Events$onClick($Main$InsertFront)]), _List_fromArray([$Html$text('insert')])), A2($Html$button, _List_fromArray([$Html$Attributes$id('remove'), $Html$Events$onClick($Main$RemoveSecond)]), _List_fromArray([$Html$text('remove')])), A2($Html$Keyed$ul, _List_fromArray([$Html$Attributes$id('keyed-list')]), A2($List$map, function (item) { return { $: '#2', a: item.key, b: $Main$viewItem(item) }; }, model.items)), A2($Html$input, _List_fromArray([$Html$Attributes$id('text-in'), $Html$Attributes$type_('text'), $Html$Attributes$value(model.textValue), $Html$Events$onInput($Main$TextChanged)]), _List_Nil), A2($Html$div, _List_fromArray([$Html$Attributes$id('text-out')]), _List_fromArray([$Html$text($String$reverse(model.textValue))])), A2($Html$input, _List_fromArray([$Html$Attributes$id('check-in'), $Html$Attributes$type_('checkbox'), $Html$Attributes$checked(model.checkboxOn), $Html$Events$onCheck($Main$CheckboxChanged)]), _List_Nil), A2($Html$div, _List_fromArray([$Html$Attributes$id('check-out')]), _List_fromArray([$Html$text((model.checkboxOn ? 'on' : 'off'))])), A2($Html$form, _List_fromArray([$Html$Attributes$id('the-form'), $Html$Events$onSubmit($Main$FormSubmitted)]), _List_fromArray([A2($Html$button, _List_fromArray([$Html$Attributes$id('submit-btn'), $Html$Attributes$type_('submit')]), _List_fromArray([$Html$text('go')]))])), A2($Html$div, _List_fromArray([$Html$Attributes$id('submit-out')]), _List_fromArray([$Html$text($String$fromInt(model.submitted))])), A2($Html$div, _List_fromArray([$Html$Attributes$id('outer'), $Html$Events$onClick($Main$OuterClicked)]), _List_fromArray([A2($Html$button, _List_fromArray([$Html$Attributes$id('stopper'), A2($Html$Events$stopPropagationOn, 'click', $Json$Decode$succeed({ $: '#2', a: $Main$InnerClicked, b: true }))]), _List_fromArray([$Html$text('stop')])), A2($Html$button, _List_fromArray([$Html$Attributes$id('bubbler'), $Html$Events$onClick($Main$InnerClicked)]), _List_fromArray([$Html$text('bubble')]))])), A2($Html$div, _List_fromArray([$Html$Attributes$id('click-out')]), _List_fromArray([$Html$text(_Utils_ap($String$fromInt(model.outerClicks), _Utils_ap('/', $String$fromInt(model.innerClicks))))])), A2($Html$button, _List_fromArray([$Html$Attributes$id('custom-btn'), A2($Html$Events$custom, 'click', $Json$Decode$succeed({ message: $Main$CustomEvent('custom'), stopPropagation: true, preventDefault: true }))]), _List_fromArray([$Html$text('custom')])), A2($Html$div, _List_fromArray([$Html$Attributes$id('custom-out')]), _List_fromArray([$Html$text(A2($String$join, ',', model.customLog))])), A2($Html$div, _List_fromArray([$Html$Attributes$id('sleep-out')]), _List_fromArray([$Html$text((model.slept ? 'awake' : 'sleeping'))])), A2($Html$div, _List_fromArray([$Html$Attributes$id('port-out')]), _List_fromArray([$Html$text(model.portEcho)])), A2($Html$Lazy$lazy, $Main$viewBadge, model.count), (model.showPanel ? A2($Html$div, _List_fromArray([$Html$Attributes$id('panel')]), _List_fromArray([$Html$text('panel-content')])) : $Html$text('')), A2($Html$button, _List_fromArray([$Html$Attributes$id('toggle-panel'), $Html$Events$onClick($Main$TogglePanel)]), _List_fromArray([$Html$text('toggle')])), A2($Html$div, _List_fromArray([$Html$Attributes$id('styled'), A2($Html$Attributes$style, 'color', (model.styleToggle ? 'rgb(255, 0, 0)' : 'rgb(0, 0, 255)')), $Html$Attributes$_class((model.styleToggle ? 'hot' : 'cold')), $Html$Attributes$disabled(model.styleToggle)]), _List_fromArray([$Html$text('styled')])), A2($Html$button, _List_fromArray([$Html$Attributes$id('toggle-style'), $Html$Events$onClick($Main$ToggleStyle)]), _List_fromArray([$Html$text('style')])), A2($Svg$svg, _List_fromArray([$Svg$Attributes$viewBox('0 0 100 100'), $Svg$Attributes$width('50'), $Html$Attributes$id('the-svg')]), _List_fromArray([A2($Svg$circle, _List_fromArray([$Svg$Attributes$cx('50'), $Svg$Attributes$cy('50'), $Svg$Attributes$r('40'), $Svg$Attributes$fill('green')]), _List_Nil)]))])); };
var $Main$main = $Browser$element({ init: $Main$init, update: $Main$update, subscriptions: $Main$subscriptions, view: $Main$view });

var Elm = { 'Main': _Platform_module({ 'init': _Platform_wrap($Main$init), 'update': _Platform_wrap($Main$update), 'subscriptions': _Platform_wrap($Main$subscriptions), 'viewItem': _Platform_wrap($Main$viewItem), 'viewBadge': _Platform_wrap($Main$viewBadge), 'childView': _Platform_wrap($Main$childView), 'view': _Platform_wrap($Main$view), 'main': _Platform_wrap($Main$main) }, $Main$main) };
_Platform_export(this, Elm);
}).call(this);
