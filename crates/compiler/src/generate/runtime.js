// alm runtime kernel — the subset of Elm's Kernel/*.js that alm's
// built-in modules need.

// CURRIED FUNCTION HELPERS

function F(arity, fun, wrapper) { wrapper.a = arity; wrapper.f = fun; return wrapper; }
function F2(fun) { return F(2, fun, function (a) { return function (b) { return fun(a, b); }; }); }
function F3(fun) { return F(3, fun, function (a) { return function (b) { return function (c) { return fun(a, b, c); }; }; }); }
function F4(fun) { return F(4, fun, function (a) { return function (b) { return function (c) { return function (d) { return fun(a, b, c, d); }; }; }; }); }
function F5(fun) { return F(5, fun, function (a) { return function (b) { return function (c) { return function (d) { return function (e) { return fun(a, b, c, d, e); }; }; }; }; }); }
function F6(fun) { return F(6, fun, function (a) { return function (b) { return function (c) { return function (d) { return function (e) { return function (f) { return fun(a, b, c, d, e, f); }; }; }; }; }; }); }
function F7(fun) { return F(7, fun, function (a) { return function (b) { return function (c) { return function (d) { return function (e) { return function (f) { return function (g) { return fun(a, b, c, d, e, f, g); }; }; }; }; }; }; }); }
// Generic builders for arities above 7 (record aliases can have dozens
// of fields). F8..F24 and A8..A24 are emitted by the code generator.
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
function _An(f, args) {
    if (f.a === args.length) { return f.f.apply(null, args); }
    var result = f;
    for (var i = 0; i < args.length; i++) { result = result(args[i]); }
    return result;
}
function A2(f, a, b) { return f.a === 2 ? f.f(a, b) : f(a)(b); }
function A3(f, a, b, c) { return f.a === 3 ? f.f(a, b, c) : f(a)(b)(c); }
function A4(f, a, b, c, d) { return f.a === 4 ? f.f(a, b, c, d) : f(a)(b)(c)(d); }
function A5(f, a, b, c, d, e) { return f.a === 5 ? f.f(a, b, c, d, e) : f(a)(b)(c)(d)(e); }
function A6(f, a, b, c, d, e, g) { return f.a === 6 ? f.f(a, b, c, d, e, g) : f(a)(b)(c)(d)(e)(g); }
function A7(f, a, b, c, d, e, g, h) { return f.a === 7 ? f.f(a, b, c, d, e, g, h) : f(a)(b)(c)(d)(e)(g)(h); }

// UNIT AND TUPLES

var _Utils_Tuple0 = { $: '#0' };

// LISTS

var _List_Nil = { $: '[]' };
function _List_Cons(hd, tl) { return { $: '::', a: hd, b: tl }; }
function _List_fromArray(arr) {
    var out = _List_Nil;
    for (var i = arr.length; i--;) { out = _List_Cons(arr[i], out); }
    return out;
}
function _List_toArray(xs) {
    var out = [];
    for (; xs.$ === '::'; xs = xs.b) { out.push(xs.a); }
    return out;
}

// EQUALITY — structural, like _Utils_eq

function _Utils_eq(x, y) {
    if (x === y) { return true; }
    if (typeof x !== 'object' || x === null || y === null) { return false; }
    for (var key in x) {
        if (!_Utils_eq(x[key], y[key])) { return false; }
    }
    for (var key2 in y) {
        if (!(key2 in x)) { return false; }
    }
    return true;
}

// COMPARISON — only ever called on comparable values

function _Utils_cmp(x, y) {
    if (typeof x !== 'object') {
        return x === y ? 0 : x < y ? -1 : 1;
    }
    if (x.$ === '#2' || x.$ === '#3') {
        var n = _Utils_cmp(x.a, y.a);
        if (n !== 0) { return n; }
        n = _Utils_cmp(x.b, y.b);
        if (n !== 0) { return n; }
        return x.$ === '#3' ? _Utils_cmp(x.c, y.c) : 0;
    }
    // lists
    for (; x.$ === '::' && y.$ === '::'; x = x.b, y = y.b) {
        var m = _Utils_cmp(x.a, y.a);
        if (m !== 0) { return m; }
    }
    return x.$ === '[]' ? (y.$ === '[]' ? 0 : -1) : 1;
}

// APPEND

function _Utils_ap(x, y) {
    if (typeof x === 'string') { return x + y; }
    if (x.$ === '[]') { return y; }
    var arr = _List_toArray(x);
    var out = y;
    for (var i = arr.length; i--;) { out = _List_Cons(arr[i], out); }
    return out;
}

// RECORD UPDATE

function _Utils_update(oldRecord, updatedFields) {
    var newRecord = {};
    for (var key in oldRecord) { newRecord[key] = oldRecord[key]; }
    for (var key2 in updatedFields) { newRecord[key2] = updatedFields[key2]; }
    return newRecord;
}

// BASICS

var $Basics$add = F2(function (a, b) { return a + b; });
var $Basics$sub = F2(function (a, b) { return a - b; });
var $Basics$mul = F2(function (a, b) { return a * b; });
var $Basics$fdiv = F2(function (a, b) { return a / b; });
var $Basics$idiv = F2(function (a, b) { return (a / b) | 0; });
var $Basics$pow = F2(function (a, b) { return Math.pow(a, b); });
var $Basics$negate = function (n) { return -n; };
var $Basics$abs = function (n) { return n < 0 ? -n : n; };
var $Basics$clamp = F3(function (lo, hi, n) { return n < lo ? lo : n > hi ? hi : n; });
var $Basics$sqrt = Math.sqrt;
var $Basics$logBase = F2(function (base, n) { return Math.log(n) / Math.log(base); });
var $Basics$e = Math.E;
var $Basics$pi = Math.PI;
var $Basics$cos = Math.cos;
var $Basics$sin = Math.sin;
var $Basics$tan = Math.tan;
var $Basics$acos = Math.acos;
var $Basics$asin = Math.asin;
var $Basics$atan = Math.atan;
var $Basics$atan2 = F2(function (y, x) { return Math.atan2(y, x); });
var $Basics$modBy = F2(function (m, n) {
    if (m === 0) { throw new Error('modBy 0 is undefined'); }
    var r = n % m;
    return (r > 0 && m < 0) || (r < 0 && m > 0) ? r + m : r;
});
var $Basics$remainderBy = F2(function (m, n) { return n % m; });
var $Basics$toFloat = function (n) { return n; };
var $Basics$round = Math.round;
var $Basics$floor = Math.floor;
var $Basics$ceiling = Math.ceil;
var $Basics$truncate = function (n) { return n | 0; };
var $Basics$eq = F2(_Utils_eq);
var $Basics$neq = F2(function (a, b) { return !_Utils_eq(a, b); });
var $Basics$lt = F2(function (a, b) { return _Utils_cmp(a, b) < 0; });
var $Basics$gt = F2(function (a, b) { return _Utils_cmp(a, b) > 0; });
var $Basics$le = F2(function (a, b) { return _Utils_cmp(a, b) < 1; });
var $Basics$ge = F2(function (a, b) { return _Utils_cmp(a, b) > -1; });
var $Basics$min = F2(function (a, b) { return _Utils_cmp(a, b) < 0 ? a : b; });
var $Basics$max = F2(function (a, b) { return _Utils_cmp(a, b) > 0 ? a : b; });
var $Basics$LT = { $: 'LT' };
var $Basics$EQ = { $: 'EQ' };
var $Basics$GT = { $: 'GT' };
var $Basics$compare = F2(function (a, b) {
    var n = _Utils_cmp(a, b);
    return n < 0 ? $Basics$LT : n ? $Basics$GT : $Basics$EQ;
});
var $Basics$not = function (b) { return !b; };
var $Basics$and = F2(function (a, b) { return a && b; });
var $Basics$or = F2(function (a, b) { return a || b; });
var $Basics$xor = F2(function (a, b) { return a !== b; });
var $Basics$append = F2(_Utils_ap);
var $Basics$identity = function (x) { return x; };
var $Basics$never = function (_n) { throw new Error('Basics.never was called (this is impossible in well-typed code)'); };
var $Basics$always = F2(function (x, _y) { return x; });
var $Basics$apL = F2(function (f, x) { return f(x); });
var $Basics$apR = F2(function (x, f) { return f(x); });
var $Basics$composeL = F3(function (g, f, x) { return g(f(x)); });
var $Basics$composeR = F3(function (f, g, x) { return g(f(x)); });

// MAYBE

var $Maybe$Nothing = { $: 'Nothing' };
var $Maybe$Just = function (a) { return { $: 'Just', a: a }; };
var $Maybe$withDefault = F2(function (fallback, maybe) {
    return maybe.$ === 'Just' ? maybe.a : fallback;
});
var $Maybe$map = F2(function (f, maybe) {
    return maybe.$ === 'Just' ? $Maybe$Just(f(maybe.a)) : maybe;
});
var $Maybe$map2 = F3(function (f, ma, mb) {
    return ma.$ === 'Just' && mb.$ === 'Just' ? $Maybe$Just(A2(f, ma.a, mb.a)) : $Maybe$Nothing;
});
var $Maybe$andThen = F2(function (f, maybe) {
    return maybe.$ === 'Just' ? f(maybe.a) : maybe;
});

// RESULT

var $Result$Ok = function (a) { return { $: 'Ok', a: a }; };
var $Result$Err = function (a) { return { $: 'Err', a: a }; };
var $Result$withDefault = F2(function (fallback, result) {
    return result.$ === 'Ok' ? result.a : fallback;
});
var $Result$map = F2(function (f, result) {
    return result.$ === 'Ok' ? $Result$Ok(f(result.a)) : result;
});
var $Result$mapError = F2(function (f, result) {
    return result.$ === 'Err' ? $Result$Err(f(result.a)) : result;
});
var $Result$andThen = F2(function (f, result) {
    return result.$ === 'Ok' ? f(result.a) : result;
});
var $Result$toMaybe = function (result) {
    return result.$ === 'Ok' ? $Maybe$Just(result.a) : $Maybe$Nothing;
};
var $Result$fromMaybe = F2(function (err, maybe) {
    return maybe.$ === 'Just' ? $Result$Ok(maybe.a) : $Result$Err(err);
});

// LIST

var $List$cons = F2(_List_Cons);
var $List$singleton = function (x) { return _List_Cons(x, _List_Nil); };
var $List$repeat = F2(function (n, x) {
    var out = _List_Nil;
    for (; n > 0; n--) { out = _List_Cons(x, out); }
    return out;
});
var $List$range = F2(function (lo, hi) {
    var out = _List_Nil;
    for (; lo <= hi; hi--) { out = _List_Cons(hi, out); }
    return out;
});
var $List$map = F2(function (f, xs) {
    return _List_fromArray(_List_toArray(xs).map(function (x) { return f(x); }));
});
var $List$indexedMap = F2(function (f, xs) {
    return _List_fromArray(_List_toArray(xs).map(function (x, i) { return A2(f, i, x); }));
});
var $List$foldl = F3(function (f, acc, xs) {
    for (; xs.$ === '::'; xs = xs.b) { acc = A2(f, xs.a, acc); }
    return acc;
});
var $List$foldr = F3(function (f, acc, xs) {
    var arr = _List_toArray(xs);
    for (var i = arr.length; i--;) { acc = A2(f, arr[i], acc); }
    return acc;
});
var $List$filter = F2(function (isGood, xs) {
    return _List_fromArray(_List_toArray(xs).filter(function (x) { return isGood(x); }));
});
var $List$filterMap = F2(function (f, xs) {
    var out = [];
    for (; xs.$ === '::'; xs = xs.b) {
        var m = f(xs.a);
        if (m.$ === 'Just') { out.push(m.a); }
    }
    return _List_fromArray(out);
});
var $List$length = function (xs) {
    var n = 0;
    for (; xs.$ === '::'; xs = xs.b) { n++; }
    return n;
};
var $List$reverse = function (xs) {
    var out = _List_Nil;
    for (; xs.$ === '::'; xs = xs.b) { out = _List_Cons(xs.a, out); }
    return out;
};
var $List$member = F2(function (x, xs) {
    for (; xs.$ === '::'; xs = xs.b) { if (_Utils_eq(x, xs.a)) { return true; } }
    return false;
});
var $List$all = F2(function (isGood, xs) {
    for (; xs.$ === '::'; xs = xs.b) { if (!isGood(xs.a)) { return false; } }
    return true;
});
var $List$any = F2(function (isGood, xs) {
    for (; xs.$ === '::'; xs = xs.b) { if (isGood(xs.a)) { return true; } }
    return false;
});
var $List$maximum = function (xs) {
    if (xs.$ !== '::') { return $Maybe$Nothing; }
    var best = xs.a;
    for (xs = xs.b; xs.$ === '::'; xs = xs.b) { if (_Utils_cmp(xs.a, best) > 0) { best = xs.a; } }
    return $Maybe$Just(best);
};
var $List$minimum = function (xs) {
    if (xs.$ !== '::') { return $Maybe$Nothing; }
    var best = xs.a;
    for (xs = xs.b; xs.$ === '::'; xs = xs.b) { if (_Utils_cmp(xs.a, best) < 0) { best = xs.a; } }
    return $Maybe$Just(best);
};
var $List$sum = function (xs) {
    var n = 0;
    for (; xs.$ === '::'; xs = xs.b) { n += xs.a; }
    return n;
};
var $List$product = function (xs) {
    var n = 1;
    for (; xs.$ === '::'; xs = xs.b) { n *= xs.a; }
    return n;
};
var $List$append = F2(_Utils_ap);
var $List$concat = function (xss) {
    var out = [];
    for (; xss.$ === '::'; xss = xss.b) {
        out.push.apply(out, _List_toArray(xss.a));
    }
    return _List_fromArray(out);
};
var $List$concatMap = F2(function (f, xs) {
    return $List$concat(A2($List$map, f, xs));
});
var $List$intersperse = F2(function (sep, xs) {
    if (xs.$ !== '::') { return xs; }
    var out = [xs.a];
    for (xs = xs.b; xs.$ === '::'; xs = xs.b) { out.push(sep, xs.a); }
    return _List_fromArray(out);
});
var $List$map2 = F3(function (f, xs, ys) {
    var out = [];
    for (; xs.$ === '::' && ys.$ === '::'; xs = xs.b, ys = ys.b) {
        out.push(A2(f, xs.a, ys.a));
    }
    return _List_fromArray(out);
});
var $List$sort = function (xs) {
    return _List_fromArray(_List_toArray(xs).sort(_Utils_cmp));
};
var $List$sortBy = F2(function (toComparable, xs) {
    return _List_fromArray(_List_toArray(xs).sort(function (a, b) {
        return _Utils_cmp(toComparable(a), toComparable(b));
    }));
});
var $List$isEmpty = function (xs) { return xs.$ === '[]'; };
var $List$head = function (xs) {
    return xs.$ === '::' ? $Maybe$Just(xs.a) : $Maybe$Nothing;
};
var $List$tail = function (xs) {
    return xs.$ === '::' ? $Maybe$Just(xs.b) : $Maybe$Nothing;
};
var $List$take = F2(function (n, xs) {
    var out = [];
    for (; n > 0 && xs.$ === '::'; n--, xs = xs.b) { out.push(xs.a); }
    return _List_fromArray(out);
});
var $List$drop = F2(function (n, xs) {
    for (; n > 0 && xs.$ === '::'; n--) { xs = xs.b; }
    return xs;
});
var $List$partition = F2(function (isGood, xs) {
    var yes = [], no = [];
    for (; xs.$ === '::'; xs = xs.b) { (isGood(xs.a) ? yes : no).push(xs.a); }
    return { $: '#2', a: _List_fromArray(yes), b: _List_fromArray(no) };
});
var $List$unzip = function (pairs) {
    var xs = [], ys = [];
    for (; pairs.$ === '::'; pairs = pairs.b) { xs.push(pairs.a.a); ys.push(pairs.a.b); }
    return { $: '#2', a: _List_fromArray(xs), b: _List_fromArray(ys) };
};

// STRING

var $String$isEmpty = function (s) { return s === ''; };
var $String$length = function (s) { return s.length; };
var $String$reverse = function (s) { return s.split('').reverse().join(''); };
var $String$repeat = F2(function (n, s) { return n < 1 ? '' : s.repeat(n); });
var $String$replace = F3(function (before, after, s) { return s.split(before).join(after); });
var $String$append = F2(function (a, b) { return a + b; });
var $String$concat = function (xs) { return _List_toArray(xs).join(''); };
var $String$split = F2(function (sep, s) { return _List_fromArray(s.split(sep)); });
var $String$join = F2(function (sep, xs) { return _List_toArray(xs).join(sep); });
var $String$words = function (s) {
    var trimmed = s.trim();
    return _List_fromArray(trimmed === '' ? [] : trimmed.split(/\s+/));
};
var $String$lines = function (s) { return _List_fromArray(s.split('\n')); };
var $String$slice = F3(function (a, b, s) {
    return s.slice(a < 0 ? Math.max(0, s.length + a) : a, b < 0 ? s.length + b : b);
});
var $String$left = F2(function (n, s) { return n < 1 ? '' : s.slice(0, n); });
var $String$right = F2(function (n, s) { return n < 1 ? '' : s.slice(-n); });
var $String$dropLeft = F2(function (n, s) { return n < 1 ? s : s.slice(n); });
var $String$dropRight = F2(function (n, s) { return n < 1 ? s : s.slice(0, -n); });
var $String$contains = F2(function (sub, s) { return s.indexOf(sub) > -1; });
var $String$startsWith = F2(function (sub, s) { return s.indexOf(sub) === 0; });
var $String$endsWith = F2(function (sub, s) {
    return s.length >= sub.length && s.lastIndexOf(sub) === s.length - sub.length;
});
var $String$toInt = function (s) {
    var n = parseInt(s, 10);
    return isNaN(n) || String(n) !== s.replace(/^\+/, '') ? $Maybe$Nothing : $Maybe$Just(n);
};
var $String$fromInt = function (n) { return String(n); };
var $String$toFloat = function (s) {
    if (s === '' || /[^0-9+\-.eE]/.test(s)) { return $Maybe$Nothing; }
    var n = Number(s);
    return isNaN(n) ? $Maybe$Nothing : $Maybe$Just(n);
};
var $String$fromFloat = function (n) { return String(n); };
var $String$fromChar = function (c) { return c; };
var $String$toList = function (s) { return _List_fromArray(Array.from(s)); };
var $String$fromList = function (cs) { return _List_toArray(cs).join(''); };
var $String$toUpper = function (s) { return s.toUpperCase(); };
var $String$toLower = function (s) { return s.toLowerCase(); };
var $String$trim = function (s) { return s.trim(); };
var $String$trimLeft = function (s) { return s.replace(/^\s+/, ''); };
var $String$trimRight = function (s) { return s.replace(/\s+$/, ''); };
var $String$pad = F3(function (n, c, s) {
    var half = Math.max(0, n - s.length) / 2;
    return c.repeat(Math.ceil(half)) + s + c.repeat(Math.floor(half));
});
var $String$padLeft = F3(function (n, c, s) {
    return c.repeat(Math.max(0, n - s.length)) + s;
});
var $String$padRight = F3(function (n, c, s) {
    return s + c.repeat(Math.max(0, n - s.length));
});
var $String$filter = F2(function (isGood, s) {
    return Array.from(s).filter(function (c) { return isGood(c); }).join('');
});
var $String$map = F2(function (f, s) {
    return Array.from(s).map(function (c) { return f(c); }).join('');
});

// CHAR

var $Char$toCode = function (c) { return c.codePointAt(0); };
var $Char$fromCode = function (n) { return String.fromCodePoint(n); };
var $Char$isDigit = function (c) { return c >= '0' && c <= '9'; };
var $Char$isAlpha = function (c) { return /^[a-zA-Z]$/.test(c); };
var $Char$isUpper = function (c) { return c >= 'A' && c <= 'Z'; };
var $Char$isLower = function (c) { return c >= 'a' && c <= 'z'; };
var $Char$toUpper = function (c) { return c.toUpperCase(); };
var $Char$toLower = function (c) { return c.toLowerCase(); };
var $Char$toLocaleUpper = function (c) { return c.toLocaleUpperCase(); };
var $Char$toLocaleLower = function (c) { return c.toLocaleLowerCase(); };

// TUPLE

var $Tuple$pair = F2(function (a, b) { return { $: '#2', a: a, b: b }; });
var $Tuple$first = function (t) { return t.a; };
var $Tuple$second = function (t) { return t.b; };
var $Tuple$mapFirst = F2(function (f, t) { return { $: '#2', a: f(t.a), b: t.b }; });
var $Tuple$mapSecond = F2(function (f, t) { return { $: '#2', a: t.a, b: f(t.b) }; });
var $Tuple$mapBoth = F3(function (f, g, t) { return { $: '#2', a: f(t.a), b: g(t.b) }; });

// DEBUG

function _Debug_toString(value) {
    if (value === true) { return 'True'; }
    if (value === false) { return 'False'; }
    if (typeof value === 'number') { return String(value); }
    if (typeof value === 'string') { return JSON.stringify(value); }
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
    if (tag === 'Dict') {
        return 'Dict.fromList ' + _Debug_toString($Dict$toList(value));
    }
    if (tag === 'Set') {
        return 'Set.fromList ' + _Debug_toString($Dict$keys(value.d));
    }
    if (tag === 'Array') {
        return 'Array.fromList ' + _Debug_toString(_List_fromArray(value.a));
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
    // record
    var fields = [];
    for (var name in value) {
        fields.push(name + ' = ' + _Debug_toString(value[name]));
    }
    return '{ ' + fields.join(', ') + ' }';
}
var $Debug$toString = _Debug_toString;
var $Debug$log = F2(function (label, value) {
    console.log(label + ': ' + _Debug_toString(value));
    return value;
});
var $Debug$todo = function (message) {
    throw new Error('TODO: ' + message);
};

// KERNEL SHIMS — compiler-internal `Elm.Kernel.*` values referenced by
// source-compiled packages (elm/core, elm-explorations/test). Mapped to alm's
// own implementations where possible; HtmlAsJson (test introspection) stubbed.
var $Elm$Kernel$Debug$toString = _Debug_toString;
var $Elm$Kernel$Debug$log = $Debug$log;
var $Elm$Kernel$Test$runThunk = function (thunk) {
    try {
        return $Result$Ok(thunk(_Utils_Tuple0));
    } catch (err) {
        return $Result$Err(err.toString());
    }
};
var $Elm$Kernel$HtmlAsJson$toJson = function (_html) { return null; };
var $Elm$Kernel$HtmlAsJson$attributeToJson = function (_attr) { return null; };
var $Elm$Kernel$HtmlAsJson$eventHandler = function (_h) { return null; };
var $Elm$Kernel$HtmlAsJson$taggerFunction = function (_t) { return null; };

// Elm.Kernel.Parser — string-scanning primitives for elm/parser (ported from
// its reference kernel; Char is a plain JS string here, tuples are #2/#3).
var $Elm$Kernel$Parser$isSubString = F5(function (small, offset, row, col, big) {
    var smallLength = small.length;
    var isGood = offset + smallLength <= big.length;
    for (var i = 0; isGood && i < smallLength;) {
        var code = big.charCodeAt(offset);
        isGood = small[i++] === big[offset++]
            && (code === 0x0A ? (row++, col = 1)
                : (col++, (code & 0xF800) === 0xD800 ? small[i++] === big[offset++] : 1));
    }
    return { $: '#3', a: isGood ? offset : -1, b: row, c: col };
});
var $Elm$Kernel$Parser$isSubChar = F3(function (predicate, offset, string) {
    return string.length <= offset ? -1
        : (string.charCodeAt(offset) & 0xF800) === 0xD800
            ? (predicate(string.substr(offset, 2)) ? offset + 2 : -1)
        : predicate(string[offset]) ? (string[offset] === '\n' ? -2 : offset + 1) : -1;
});
var $Elm$Kernel$Parser$isAsciiCode = F3(function (code, offset, string) {
    return string.charCodeAt(offset) === code;
});
var $Elm$Kernel$Parser$chompBase10 = F2(function (offset, string) {
    for (; offset < string.length; offset++) {
        var code = string.charCodeAt(offset);
        if (code < 0x30 || 0x39 < code) { return offset; }
    }
    return offset;
});
var $Elm$Kernel$Parser$consumeBase = F3(function (base, offset, string) {
    for (var total = 0; offset < string.length; offset++) {
        var digit = string.charCodeAt(offset) - 0x30;
        if (digit < 0 || base <= digit) { break; }
        total = base * total + digit;
    }
    return { $: '#2', a: offset, b: total };
});
var $Elm$Kernel$Parser$consumeBase16 = F2(function (offset, string) {
    for (var total = 0; offset < string.length; offset++) {
        var code = string.charCodeAt(offset);
        if (0x30 <= code && code <= 0x39) { total = 16 * total + code - 0x30; }
        else if (0x41 <= code && code <= 0x46) { total = 16 * total + code - 55; }
        else if (0x61 <= code && code <= 0x66) { total = 16 * total + code - 87; }
        else { break; }
    }
    return { $: '#2', a: offset, b: total };
});
var $Elm$Kernel$Parser$findSubString = F5(function (small, offset, row, col, big) {
    var newOffset = big.indexOf(small, offset);
    var target = newOffset < 0 ? big.length : newOffset + small.length;
    while (offset < target) {
        var code = big.charCodeAt(offset++);
        code === 0x0A ? (col = 1, row++) : (col++, (code & 0xF800) === 0xD800 && offset++);
    }
    return { $: '#3', a: newOffset, b: row, c: col };
});

// Elm.Kernel.Bytes — a real elm/bytes 1.0.8 runtime ported from the reference
// kernel (Elm/Kernel/Bytes.js), adapted to alm's value representations.
//
// A `Bytes` value is represented as a JS `DataView` (matching the reference).
// `width` reads `.byteLength`; `encode` allocates an `ArrayBuffer`, writes into
// it via a `DataView`, and returns that view; `read_bytes` returns a sub-`DataView`.
//
// The generated Elm `Bytes.Encode.write`/`getWidth` are subject to dead-code
// elimination (they are only reachable from this kernel, which alm does not scan
// for dependencies), so `encode` cannot rely on them. Instead it walks the
// `Encoder` tree here, pattern-matching on the constructor tags alm emits for
// `Bytes.Encode.Encoder` (`I8`/`I16`/`I32`/`U8`/`U16`/`U32`/`F32`/`F64`/`Seq`/
// `Utf8`/`Bytes`). Endianness values are `{ $: 'LE' }` / `{ $: 'BE' }`.

function _Bytes_isLE(endianness) { return endianness.$ === 'LE'; }

// The UTF-8 byte length of a string (not `.length`, which counts UTF-16 units).
function _Bytes_getStringWidth(string) {
	for (var width = 0, i = 0; i < string.length; i++) {
		var code = string.charCodeAt(i);
		width +=
			(code < 0x80) ? 1 :
			(code < 0x800) ? 2 :
			(code < 0xD800 || 0xDBFF < code) ? 3 : (i++, 4);
	}
	return width;
}

// Write `string` as UTF-8 into `mb` at `offset`; return the new offset.
function _Bytes_writeString(mb, offset, string) {
	for (var i = 0; i < string.length; i++) {
		var code = string.charCodeAt(i);
		offset +=
			(code < 0x80)
				? (mb.setUint8(offset, code), 1)
				:
			(code < 0x800)
				? (mb.setUint16(offset,
					0xC080 | (code >>> 6 & 0x1F) << 8 | code & 0x3F), 2)
				:
			(code < 0xD800 || 0xDBFF < code)
				? (mb.setUint16(offset,
					0xE080 | (code >>> 12 & 0xF) << 8 | code >>> 6 & 0x3F)
				, mb.setUint8(offset + 2, 0x80 | code & 0x3F), 3)
				:
				(code = (code - 0xD800) * 0x400 + string.charCodeAt(++i) - 0xDC00 + 0x10000
				, mb.setUint32(offset,
					0xF0808080
					| (code >>> 18 & 0x7) << 24
					| (code >>> 12 & 0x3F) << 16
					| (code >>> 6 & 0x3F) << 8
					| code & 0x3F), 4);
	}
	return offset;
}

// Copy the bytes of DataView `bytes` into `mb` at `offset`; return new offset.
function _Bytes_writeBytes(mb, offset, bytes) {
	for (var i = 0, len = bytes.byteLength, limit = len - 4; i <= limit; i += 4) {
		mb.setUint32(offset + i, bytes.getUint32(i));
	}
	for (; i < len; i++) {
		mb.setUint8(offset + i, bytes.getUint8(i));
	}
	return offset + len;
}

// Total width, in bytes, of an `Encoder` tree. `Seq`/`Utf8` carry a precomputed
// width in `.a` (via `Bytes.Encode.sequence`/`string`), mirroring the reference.
function _Bytes_encoderWidth(encoder) {
	switch (encoder.$) {
		case 'I8': case 'U8': return 1;
		case 'I16': case 'U16': return 2;
		case 'I32': case 'U32': case 'F32': return 4;
		case 'F64': return 8;
		case 'Seq': return encoder.a;
		case 'Utf8': return encoder.a;
		case 'Bytes': return encoder.a.byteLength;
	}
	return 0;
}

// Write an `Encoder` tree into `mb` at `offset`; return the new offset.
function _Bytes_writeEncoder(mb, offset, encoder) {
	switch (encoder.$) {
		case 'I8': mb.setInt8(offset, encoder.a); return offset + 1;
		case 'U8': mb.setUint8(offset, encoder.a); return offset + 1;
		case 'I16': mb.setInt16(offset, encoder.b, _Bytes_isLE(encoder.a)); return offset + 2;
		case 'U16': mb.setUint16(offset, encoder.b, _Bytes_isLE(encoder.a)); return offset + 2;
		case 'I32': mb.setInt32(offset, encoder.b, _Bytes_isLE(encoder.a)); return offset + 4;
		case 'U32': mb.setUint32(offset, encoder.b, _Bytes_isLE(encoder.a)); return offset + 4;
		case 'F32': mb.setFloat32(offset, encoder.b, _Bytes_isLE(encoder.a)); return offset + 4;
		case 'F64': mb.setFloat64(offset, encoder.b, _Bytes_isLE(encoder.a)); return offset + 8;
		case 'Seq': {
			var arr = _List_toArray(encoder.b);
			for (var i = 0; i < arr.length; i++) {
				offset = _Bytes_writeEncoder(mb, offset, arr[i]);
			}
			return offset;
		}
		case 'Utf8': return _Bytes_writeString(mb, offset, encoder.b);
		case 'Bytes': return _Bytes_writeBytes(mb, offset, encoder.a);
	}
	return offset;
}

// `getHostEndianness : Task x Endianness` — resolves to LE on little-endian
// machines (virtually all of them), otherwise BE.
var $Elm$Kernel$Bytes$getHostEndianness = F2(function (le, be) {
	return $Task$succeed(new Uint8Array(new Uint32Array([1]).buffer)[0] === 1 ? le : be);
});
var $Elm$Kernel$Bytes$width = function (bytes) { return bytes.byteLength; };
var $Elm$Kernel$Bytes$getStringWidth = function (s) { return _Bytes_getStringWidth(s); };
var $Elm$Kernel$Bytes$encode = function (encoder) {
	var mb = new DataView(new ArrayBuffer(_Bytes_encoderWidth(encoder)));
	_Bytes_writeEncoder(mb, 0, encoder);
	return mb;
};

// A decoder is `Bytes -> Int -> (Int, a)`; run it at offset 0 and take the
// value. Out-of-range reads (DataView throws) or `fail` become `Nothing`.
var $Elm$Kernel$Bytes$decode = F2(function (decoder, bytes) {
	try {
		return $Maybe$Just(A2(decoder, bytes, 0).b);
	} catch (e) {
		return $Maybe$Nothing;
	}
});
var $Elm$Kernel$Bytes$decodeFailure = F2(function () { throw 0; });

var $Elm$Kernel$Bytes$read_i8 = F2(function (bytes, offset) { return { $: '#2', a: offset + 1, b: bytes.getInt8(offset) }; });
var $Elm$Kernel$Bytes$read_i16 = F3(function (isLE, bytes, offset) { return { $: '#2', a: offset + 2, b: bytes.getInt16(offset, isLE) }; });
var $Elm$Kernel$Bytes$read_i32 = F3(function (isLE, bytes, offset) { return { $: '#2', a: offset + 4, b: bytes.getInt32(offset, isLE) }; });
var $Elm$Kernel$Bytes$read_u8 = F2(function (bytes, offset) { return { $: '#2', a: offset + 1, b: bytes.getUint8(offset) }; });
var $Elm$Kernel$Bytes$read_u16 = F3(function (isLE, bytes, offset) { return { $: '#2', a: offset + 2, b: bytes.getUint16(offset, isLE) }; });
var $Elm$Kernel$Bytes$read_u32 = F3(function (isLE, bytes, offset) { return { $: '#2', a: offset + 4, b: bytes.getUint32(offset, isLE) }; });
var $Elm$Kernel$Bytes$read_f32 = F3(function (isLE, bytes, offset) { return { $: '#2', a: offset + 4, b: bytes.getFloat32(offset, isLE) }; });
var $Elm$Kernel$Bytes$read_f64 = F3(function (isLE, bytes, offset) { return { $: '#2', a: offset + 8, b: bytes.getFloat64(offset, isLE) }; });
var $Elm$Kernel$Bytes$read_bytes = F3(function (len, bytes, offset) {
	return { $: '#2', a: offset + len, b: new DataView(bytes.buffer, bytes.byteOffset + offset, len) };
});
var $Elm$Kernel$Bytes$read_string = F3(function (len, bytes, offset) {
	var string = '';
	var end = offset + len;
	for (; offset < end;) {
		var byte = bytes.getUint8(offset++);
		string +=
			(byte < 128)
				? String.fromCharCode(byte)
				:
			((byte & 0xE0) === 0xC0)
				? String.fromCharCode((byte & 0x1F) << 6 | bytes.getUint8(offset++) & 0x3F)
				:
			((byte & 0xF0) === 0xE0)
				? String.fromCharCode(
					(byte & 0xF) << 12
					| (bytes.getUint8(offset++) & 0x3F) << 6
					| bytes.getUint8(offset++) & 0x3F)
				:
				(byte =
					((byte & 0x7) << 18
						| (bytes.getUint8(offset++) & 0x3F) << 12
						| (bytes.getUint8(offset++) & 0x3F) << 6
						| bytes.getUint8(offset++) & 0x3F) - 0x10000
				, String.fromCharCode(Math.floor(byte / 0x400) + 0xD800, byte % 0x400 + 0xDC00));
	}
	return { $: '#2', a: offset, b: string };
});

// The generated `Bytes.Encode.write` dispatches to these when it is present;
// `encode` above does not depend on them.
var $Elm$Kernel$Bytes$write_i8 = F3(function (mb, offset, n) { mb.setInt8(offset, n); return offset + 1; });
var $Elm$Kernel$Bytes$write_i16 = F4(function (mb, offset, n, isLE) { mb.setInt16(offset, n, isLE); return offset + 2; });
var $Elm$Kernel$Bytes$write_i32 = F4(function (mb, offset, n, isLE) { mb.setInt32(offset, n, isLE); return offset + 4; });
var $Elm$Kernel$Bytes$write_u8 = F3(function (mb, offset, n) { mb.setUint8(offset, n); return offset + 1; });
var $Elm$Kernel$Bytes$write_u16 = F4(function (mb, offset, n, isLE) { mb.setUint16(offset, n, isLE); return offset + 2; });
var $Elm$Kernel$Bytes$write_u32 = F4(function (mb, offset, n, isLE) { mb.setUint32(offset, n, isLE); return offset + 4; });
var $Elm$Kernel$Bytes$write_f32 = F4(function (mb, offset, n, isLE) { mb.setFloat32(offset, n, isLE); return offset + 4; });
var $Elm$Kernel$Bytes$write_f64 = F4(function (mb, offset, n, isLE) { mb.setFloat64(offset, n, isLE); return offset + 8; });
var $Elm$Kernel$Bytes$write_bytes = F3(function (mb, offset, bytes) { return _Bytes_writeBytes(mb, offset, bytes); });
var $Elm$Kernel$Bytes$write_string = F3(function (mb, offset, string) { return _Bytes_writeString(mb, offset, string); });

// BASICS — extras

var $Basics$isNaN = function (n) { return isNaN(n); };
var $Basics$isInfinite = function (n) { return n === Infinity || n === -Infinity; };
var $Basics$degrees = function (d) { return d * Math.PI / 180; };
var $Basics$radians = function (r) { return r; };
var $Basics$turns = function (t) { return t * 2 * Math.PI; };
var $Basics$toPolar = function (p) {
    return { $: '#2', a: Math.sqrt(p.a * p.a + p.b * p.b), b: Math.atan2(p.b, p.a) };
};
var $Basics$fromPolar = function (p) {
    return { $: '#2', a: p.a * Math.cos(p.b), b: p.a * Math.sin(p.b) };
};

// LIST — extras

var $List$sortWith = F2(function (compare, xs) {
    return _List_fromArray(_List_toArray(xs).sort(function (a, b) {
        var order = A2(compare, a, b);
        return order.$ === 'LT' ? -1 : order.$ === 'EQ' ? 0 : 1;
    }));
});
var $List$map3 = F4(function (f, xs, ys, zs) {
    var out = [];
    for (; xs.$ === '::' && ys.$ === '::' && zs.$ === '::'; xs = xs.b, ys = ys.b, zs = zs.b) {
        out.push(A3(f, xs.a, ys.a, zs.a));
    }
    return _List_fromArray(out);
});

// STRING — extras

var $String$uncons = function (s) {
    if (s === '') { return $Maybe$Nothing; }
    var c = Array.from(s)[0];
    return $Maybe$Just({ $: '#2', a: c, b: s.slice(c.length) });
};
var $String$cons = F2(function (c, s) { return c + s; });
var $String$indexes = F2(function (sub, s) {
    if (sub === '') { return _List_Nil; }
    var out = [];
    var i = s.indexOf(sub);
    while (i > -1) { out.push(i); i = s.indexOf(sub, i + sub.length); }
    return _List_fromArray(out);
});
var $String$indices = $String$indexes;
var $String$any = F2(function (isGood, s) {
    return Array.from(s).some(function (c) { return isGood(c); });
});
var $String$all = F2(function (isGood, s) {
    return Array.from(s).every(function (c) { return isGood(c); });
});
var $String$foldl = F3(function (f, acc, s) {
    var chars = Array.from(s);
    for (var i = 0; i < chars.length; i++) { acc = A2(f, chars[i], acc); }
    return acc;
});
var $String$foldr = F3(function (f, acc, s) {
    var chars = Array.from(s);
    for (var i = chars.length; i--;) { acc = A2(f, chars[i], acc); }
    return acc;
});

// CHAR — extras

var $Char$isAlphaNum = function (c) { return /^[a-zA-Z0-9]$/.test(c); };
var $Char$isHexDigit = function (c) { return /^[0-9a-fA-F]$/.test(c); };
var $Char$isOctDigit = function (c) { return c >= '0' && c <= '7'; };

// MAYBE — extras

var $Maybe$map3 = F4(function (f, ma, mb, mc) {
    return ma.$ === 'Just' && mb.$ === 'Just' && mc.$ === 'Just'
        ? $Maybe$Just(A3(f, ma.a, mb.a, mc.a))
        : $Maybe$Nothing;
});
var $Maybe$map4 = F5(function (f, ma, mb, mc, md) {
    return ma.$ === 'Just' && mb.$ === 'Just' && mc.$ === 'Just' && md.$ === 'Just'
        ? $Maybe$Just(A4(f, ma.a, mb.a, mc.a, md.a))
        : $Maybe$Nothing;
});

// RESULT — extras

var $Result$map2 = F3(function (f, ra, rb) {
    if (ra.$ === 'Err') { return ra; }
    if (rb.$ === 'Err') { return rb; }
    return $Result$Ok(A2(f, ra.a, rb.a));
});

// DICT
//
// Elm's Dict is a red-black tree; alm uses an immutable sorted array of
// keys with a parallel array of values. Same observable behavior;
// insert/remove are O(n) copies rather than O(log n).

var $Dict$empty = { $: 'Dict', keys: [], vals: [] };

function _Dict_search(dict, key) {
    // Binary search: returns index if found, otherwise ~insertionPoint.
    var lo = 0, hi = dict.keys.length - 1;
    while (lo <= hi) {
        var mid = (lo + hi) >> 1;
        var cmp = _Utils_cmp(dict.keys[mid], key);
        if (cmp === 0) { return mid; }
        if (cmp < 0) { lo = mid + 1; } else { hi = mid - 1; }
    }
    return ~lo;
}

var $Dict$singleton = F2(function (key, value) {
    return { $: 'Dict', keys: [key], vals: [value] };
});
var $Dict$insert = F3(function (key, value, dict) {
    var i = _Dict_search(dict, key);
    var keys = dict.keys.slice();
    var vals = dict.vals.slice();
    if (i >= 0) {
        vals[i] = value;
    } else {
        keys.splice(~i, 0, key);
        vals.splice(~i, 0, value);
    }
    return { $: 'Dict', keys: keys, vals: vals };
});
var $Dict$remove = F2(function (key, dict) {
    var i = _Dict_search(dict, key);
    if (i < 0) { return dict; }
    var keys = dict.keys.slice();
    var vals = dict.vals.slice();
    keys.splice(i, 1);
    vals.splice(i, 1);
    return { $: 'Dict', keys: keys, vals: vals };
});
var $Dict$update = F3(function (key, alter, dict) {
    var i = _Dict_search(dict, key);
    var current = i >= 0 ? $Maybe$Just(dict.vals[i]) : $Maybe$Nothing;
    var next = alter(current);
    return next.$ === 'Just'
        ? A3($Dict$insert, key, next.a, dict)
        : A2($Dict$remove, key, dict);
});
var $Dict$isEmpty = function (dict) { return dict.keys.length === 0; };
var $Dict$member = F2(function (key, dict) { return _Dict_search(dict, key) >= 0; });
var $Dict$get = F2(function (key, dict) {
    var i = _Dict_search(dict, key);
    return i >= 0 ? $Maybe$Just(dict.vals[i]) : $Maybe$Nothing;
});
var $Dict$size = function (dict) { return dict.keys.length; };
var $Dict$keys = function (dict) { return _List_fromArray(dict.keys); };
var $Dict$values = function (dict) { return _List_fromArray(dict.vals); };
var $Dict$toList = function (dict) {
    return _List_fromArray(dict.keys.map(function (k, i) {
        return { $: '#2', a: k, b: dict.vals[i] };
    }));
};
var $Dict$fromList = function (pairs) {
    var dict = $Dict$empty;
    for (; pairs.$ === '::'; pairs = pairs.b) {
        dict = A3($Dict$insert, pairs.a.a, pairs.a.b, dict);
    }
    return dict;
};
var $Dict$map = F2(function (f, dict) {
    return {
        $: 'Dict',
        keys: dict.keys.slice(),
        vals: dict.vals.map(function (v, i) { return A2(f, dict.keys[i], v); })
    };
});
var $Dict$foldl = F3(function (f, acc, dict) {
    for (var i = 0; i < dict.keys.length; i++) { acc = A3(f, dict.keys[i], dict.vals[i], acc); }
    return acc;
});
var $Dict$foldr = F3(function (f, acc, dict) {
    for (var i = dict.keys.length; i--;) { acc = A3(f, dict.keys[i], dict.vals[i], acc); }
    return acc;
});
var $Dict$filter = F2(function (isGood, dict) {
    var keys = [], vals = [];
    for (var i = 0; i < dict.keys.length; i++) {
        if (A2(isGood, dict.keys[i], dict.vals[i])) {
            keys.push(dict.keys[i]);
            vals.push(dict.vals[i]);
        }
    }
    return { $: 'Dict', keys: keys, vals: vals };
});
var $Dict$partition = F2(function (isGood, dict) {
    var yes = { $: 'Dict', keys: [], vals: [] };
    var no = { $: 'Dict', keys: [], vals: [] };
    for (var i = 0; i < dict.keys.length; i++) {
        var target = A2(isGood, dict.keys[i], dict.vals[i]) ? yes : no;
        target.keys.push(dict.keys[i]);
        target.vals.push(dict.vals[i]);
    }
    return { $: '#2', a: yes, b: no };
});
var $Dict$union = F2(function (left, right) {
    var result = right;
    for (var i = 0; i < left.keys.length; i++) {
        result = A3($Dict$insert, left.keys[i], left.vals[i], result);
    }
    return result;
});
var $Dict$intersect = F2(function (left, right) {
    return A2($Dict$filter, F2(function (k, _v) { return A2($Dict$member, k, right); }), left);
});
var $Dict$diff = F2(function (left, right) {
    return A2($Dict$filter, F2(function (k, _v) { return !A2($Dict$member, k, right); }), left);
});
var $Dict$merge = F6(function (leftStep, bothStep, rightStep, left, right, initial) {
    var acc = initial;
    var i = 0, j = 0;
    while (i < left.keys.length && j < right.keys.length) {
        var lk = left.keys[i], rk = right.keys[j];
        var c = _Utils_cmp(lk, rk);
        if (c < 0) { acc = A3(leftStep, lk, left.vals[i], acc); i++; }
        else if (c > 0) { acc = A3(rightStep, rk, right.vals[j], acc); j++; }
        else { acc = A4(bothStep, lk, left.vals[i], right.vals[j], acc); i++; j++; }
    }
    for (; i < left.keys.length; i++) { acc = A3(leftStep, left.keys[i], left.vals[i], acc); }
    for (; j < right.keys.length; j++) { acc = A3(rightStep, right.keys[j], right.vals[j], acc); }
    return acc;
});

// SET — a Dict with unit values.

var $Set$empty = { $: 'Set', d: $Dict$empty };
var $Set$singleton = function (key) { return { $: 'Set', d: A2($Dict$singleton, key, 0) }; };
var $Set$insert = F2(function (key, set) { return { $: 'Set', d: A3($Dict$insert, key, 0, set.d) }; });
var $Set$remove = F2(function (key, set) { return { $: 'Set', d: A2($Dict$remove, key, set.d) }; });
var $Set$isEmpty = function (set) { return $Dict$isEmpty(set.d); };
var $Set$member = F2(function (key, set) { return A2($Dict$member, key, set.d); });
var $Set$size = function (set) { return $Dict$size(set.d); };
var $Set$toList = function (set) { return $Dict$keys(set.d); };
var $Set$fromList = function (xs) {
    var set = $Set$empty;
    for (; xs.$ === '::'; xs = xs.b) { set = A2($Set$insert, xs.a, set); }
    return set;
};
var $Set$map = F2(function (f, set) {
    return $Set$fromList(A2($List$map, f, $Set$toList(set)));
});
var $Set$foldl = F3(function (f, acc, set) {
    return A3($Dict$foldl, F3(function (k, _v, a) { return A2(f, k, a); }), acc, set.d);
});
var $Set$foldr = F3(function (f, acc, set) {
    return A3($Dict$foldr, F3(function (k, _v, a) { return A2(f, k, a); }), acc, set.d);
});
var $Set$filter = F2(function (isGood, set) {
    return { $: 'Set', d: A2($Dict$filter, F2(function (k, _v) { return isGood(k); }), set.d) };
});
var $Set$partition = F2(function (isGood, set) {
    var pair = A2($Dict$partition, F2(function (k, _v) { return isGood(k); }), set.d);
    return { $: '#2', a: { $: 'Set', d: pair.a }, b: { $: 'Set', d: pair.b } };
});
var $Set$union = F2(function (a, b) { return { $: 'Set', d: A2($Dict$union, a.d, b.d) }; });
var $Set$intersect = F2(function (a, b) { return { $: 'Set', d: A2($Dict$intersect, a.d, b.d) }; });
var $Set$diff = F2(function (a, b) { return { $: 'Set', d: A2($Dict$diff, a.d, b.d) }; });

// ARRAY — immutable JS array copies (Elm uses a Hickey trie).

var $Array$empty = { $: 'Array', a: [] };
var $Array$initialize = F2(function (n, f) {
    var out = [];
    for (var i = 0; i < n; i++) { out.push(f(i)); }
    return { $: 'Array', a: out };
});
var $Array$repeat = F2(function (n, x) {
    var out = [];
    for (var i = 0; i < n; i++) { out.push(x); }
    return { $: 'Array', a: out };
});
var $Array$fromList = function (xs) { return { $: 'Array', a: _List_toArray(xs) }; };
var $Array$isEmpty = function (arr) { return arr.a.length === 0; };
var $Array$length = function (arr) { return arr.a.length; };
var $Array$get = F2(function (i, arr) {
    return i >= 0 && i < arr.a.length ? $Maybe$Just(arr.a[i]) : $Maybe$Nothing;
});
var $Array$set = F3(function (i, x, arr) {
    if (i < 0 || i >= arr.a.length) { return arr; }
    var out = arr.a.slice();
    out[i] = x;
    return { $: 'Array', a: out };
});
var $Array$push = F2(function (x, arr) {
    var out = arr.a.slice();
    out.push(x);
    return { $: 'Array', a: out };
});
var $Array$toList = function (arr) { return _List_fromArray(arr.a); };
var $Array$toIndexedList = function (arr) {
    return _List_fromArray(arr.a.map(function (x, i) { return { $: '#2', a: i, b: x }; }));
};
var $Array$map = F2(function (f, arr) {
    return { $: 'Array', a: arr.a.map(function (x) { return f(x); }) };
});
var $Array$indexedMap = F2(function (f, arr) {
    return { $: 'Array', a: arr.a.map(function (x, i) { return A2(f, i, x); }) };
});
var $Array$foldl = F3(function (f, acc, arr) {
    for (var i = 0; i < arr.a.length; i++) { acc = A2(f, arr.a[i], acc); }
    return acc;
});
var $Array$foldr = F3(function (f, acc, arr) {
    for (var i = arr.a.length; i--;) { acc = A2(f, arr.a[i], acc); }
    return acc;
});
var $Array$filter = F2(function (isGood, arr) {
    return { $: 'Array', a: arr.a.filter(function (x) { return isGood(x); }) };
});
var $Array$append = F2(function (a, b) { return { $: 'Array', a: a.a.concat(b.a) }; });
var $Array$slice = F3(function (from, to, arr) {
    var len = arr.a.length;
    if (from < 0) { from = Math.max(0, len + from); }
    if (to < 0) { to = len + to; }
    return { $: 'Array', a: arr.a.slice(from, to) };
});

// BITWISE

var $Bitwise$and = F2(function (a, b) { return a & b; });
var $Bitwise$or = F2(function (a, b) { return a | b; });
var $Bitwise$xor = F2(function (a, b) { return a ^ b; });
var $Bitwise$complement = function (a) { return ~a; };
var $Bitwise$shiftLeftBy = F2(function (offset, a) { return a << offset; });
var $Bitwise$shiftRightBy = F2(function (offset, a) { return a >> offset; });
var $Bitwise$shiftRightZfBy = F2(function (offset, a) { return a >>> offset; });

// VIRTUAL DOM

function _VDom_text(text) { return { $: 'VText', text: text }; }
function _VDom_node(tag) {
    return F2(function (attrs, kids) {
        return { $: 'VNode', tag: tag, attrs: _List_toArray(attrs), kids: _List_toArray(kids) };
    });
}
function _VDom_nodeNS(tag) {
    return F2(function (attrs, kids) {
        return {
            $: 'VNode', tag: tag, ns: 'http://www.w3.org/2000/svg',
            attrs: _List_toArray(attrs), kids: _List_toArray(kids)
        };
    });
}

var $Html$text = _VDom_text;
var $VirtualDom$text = _VDom_text;
var $VirtualDom$node = function (tag) { return _VDom_node(tag); };
var $VirtualDom$nodeNS = F2(function (ns, tag) {
    return F2(function (attrs, kids) {
        return { $: 'VNode', tag: tag, ns: ns, attrs: _List_toArray(attrs), kids: _List_toArray(kids) };
    });
});
var $VirtualDom$attribute = F2(function (key, val) { return { $: 'AAttr', key: key, val: val }; });
var $VirtualDom$property = F2(function (key, val) { return { $: 'AProp', key: key, val: val }; });
var $VirtualDom$style = F2(function (key, val) { return { $: 'AStyle', key: key, val: val }; });
var $VirtualDom$map = F2(function (f, vnode) { return { $: 'VMap', f: f, node: vnode }; });
var $Html$node = function (tag) { return _VDom_node(tag); };
var $Html$map = F2(function (f, vnode) { return { $: 'VMap', f: f, node: vnode }; });
var $Svg$map = $Html$map;
var $Svg$text = _VDom_text;
var $Svg$node = function (tag) { return _VDom_nodeNS(tag); };

var $Html$Keyed$node = function (tag) {
    return F2(function (attrs, keyedKids) {
        return {
            $: 'VKeyed', tag: tag, attrs: _List_toArray(attrs),
            kids: _List_toArray(keyedKids) // (key, node) tuples
        };
    });
};
var $Html$Keyed$ul = $Html$Keyed$node('ul');
var $Html$Keyed$ol = $Html$Keyed$node('ol');
var $VirtualDom$keyedNode = $Html$Keyed$node;
var $VirtualDom$keyedNodeNS = F2(function (ns, tag) {
    return F2(function (attrs, keyedKids) {
        return {
            $: 'VKeyed', tag: tag, ns: ns, attrs: _List_toArray(attrs),
            kids: _List_toArray(keyedKids)
        };
    });
});
var $VirtualDom$attributeNS = F3(function (ns, key, val) {
    return { $: 'AAttr', key: key, val: val, ns: ns };
});
var $VirtualDom$on = F2(function (name, handler) {
    switch (handler.$) {
        case 'MayStopPropagation': return _VDom_on(name, handler.a, { pair: true, stopField: true });
        case 'MayPreventDefault': return _VDom_on(name, handler.a, { pair: true, preventField: true });
        case 'Custom': return _VDom_on(name, handler.a, { custom: true });
        default: return _VDom_on(name, handler.a);
    }
});

var $Html$Lazy$lazy = F2(function (f, a) { return { $: 'VLazy', f: f, args: [a] }; });
var $Html$Lazy$lazy2 = F3(function (f, a, b) { return { $: 'VLazy', f: f, args: [a, b] }; });
var $Html$Lazy$lazy3 = F4(function (f, a, b, c) { return { $: 'VLazy', f: f, args: [a, b, c] }; });
var $Html$Lazy$lazy4 = F5(function (f, a, b, c, d) { return { $: 'VLazy', f: f, args: [a, b, c, d] }; });
var $Html$Lazy$lazy5 = _Fn(6, function (f, a, b, c, d, e) { return { $: 'VLazy', f: f, args: [a, b, c, d, e] }; });
var $Html$Lazy$lazy6 = _Fn(7, function (f, a, b, c, d, e, g) { return { $: 'VLazy', f: f, args: [a, b, c, d, e, g] }; });
var $Html$Lazy$lazy7 = _Fn(8, function (f, a, b, c, d, e, g, h) { return { $: 'VLazy', f: f, args: [a, b, c, d, e, g, h] }; });
var $Html$Lazy$lazy8 = _Fn(9, function (f, a, b, c, d, e, g, h, i) { return { $: 'VLazy', f: f, args: [a, b, c, d, e, g, h, i] }; });
var $VirtualDom$lazy = $Html$Lazy$lazy;
var $VirtualDom$lazy2 = $Html$Lazy$lazy2;
var $VirtualDom$lazy3 = $Html$Lazy$lazy3;
var $VirtualDom$lazy4 = $Html$Lazy$lazy4;
var $VirtualDom$lazy5 = $Html$Lazy$lazy5;
var $VirtualDom$lazy6 = $Html$Lazy$lazy6;
var $VirtualDom$lazy7 = $Html$Lazy$lazy7;
var $VirtualDom$lazy8 = $Html$Lazy$lazy8;

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
var $Html$Attributes$attribute = F2(function (key, val) { return { $: 'AAttr', key: key, val: val }; });
var $Html$Attributes$map = F2(function (f, attr) {
    return attr.$ === 'AEvent' ? { $: 'AEvent', name: attr.name, toMsg: function (e) { return f(attr.toMsg(e)); }, opts: attr.opts } : attr;
});
var $VirtualDom$mapAttribute = $Html$Attributes$map;
function _VDom_prop(key) {
    return function (val) { return { $: 'AProp', key: key, val: val }; };
}
// Events carry a Json decoder run against the DOM event, like real Elm.
// The decoder yields the message; `stop`/`prevent` control propagation.
function _VDom_on(name, decoder, opts) {
    return { $: 'AEvent', name: name, decoder: decoder, opts: opts };
}

function _Json_succeedDecoder(msg) {
    return { $: 'Decoder', run: function (_v) { return { ok: true, value: msg }; } };
}

var $Html$Events$on = F2(function (name, decoder) { return _VDom_on(name, decoder); });
var $Html$Events$stopPropagationOn = F2(function (name, decoder) {
    return _VDom_on(name, decoder, { pair: true, stopField: true });
});
var $Html$Events$preventDefaultOn = F2(function (name, decoder) {
    return _VDom_on(name, decoder, { pair: true, preventField: true });
});
var $Html$Events$onClick = function (msg) { return _VDom_on('click', _Json_succeedDecoder(msg)); };
var $Html$Events$onDoubleClick = function (msg) { return _VDom_on('dblclick', _Json_succeedDecoder(msg)); };
var $Html$Events$onMouseDown = function (msg) { return _VDom_on('mousedown', _Json_succeedDecoder(msg)); };
var $Html$Events$onMouseUp = function (msg) { return _VDom_on('mouseup', _Json_succeedDecoder(msg)); };
var $Html$Events$onMouseEnter = function (msg) { return _VDom_on('mouseenter', _Json_succeedDecoder(msg)); };
var $Html$Events$onMouseLeave = function (msg) { return _VDom_on('mouseleave', _Json_succeedDecoder(msg)); };
var $Html$Events$onMouseOver = function (msg) { return _VDom_on('mouseover', _Json_succeedDecoder(msg)); };
var $Html$Events$onMouseOut = function (msg) { return _VDom_on('mouseout', _Json_succeedDecoder(msg)); };
var $Html$Events$custom = F2(function (name, decoder) {
    return _VDom_on(name, decoder, { custom: true });
});
var $Html$Events$targetValue = _Json_decoder(function (e) {
    return e && e.target && typeof e.target.value === 'string'
        ? _Json_ok(e.target.value)
        : _Json_expecting('an event with target.value', e);
});
var $Html$Events$targetChecked = _Json_decoder(function (e) {
    return e && e.target && typeof e.target.checked === 'boolean'
        ? _Json_ok(e.target.checked)
        : _Json_expecting('an event with target.checked', e);
});
var $Html$Events$keyCode = _Json_decoder(function (e) {
    return e && typeof e.keyCode === 'number'
        ? _Json_ok(e.keyCode)
        : _Json_expecting('an event with keyCode', e);
});
var $Html$Events$onBlur = function (msg) { return _VDom_on('blur', _Json_succeedDecoder(msg)); };
var $Html$Events$onFocus = function (msg) { return _VDom_on('focus', _Json_succeedDecoder(msg)); };
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

    if (oldV.$ !== newV.$ || oldV.tag !== newV.tag || oldV.ns !== newV.ns) {
        var replacement = _VDom_render(newV, dispatch, doc);
        dom.parentNode.replaceChild(replacement, dom);
        return replacement;
    }

    // Same tag: patch attributes...
    var oldAttrs = {};
    for (var i = 0; i < oldV.attrs.length; i++) {
        oldAttrs[_VDom_attrKey(oldV.attrs[i])] = oldV.attrs[i];
    }
    var newKeys = {};
    for (var j = 0; j < newV.attrs.length; j++) {
        var attr = newV.attrs[j];
        newKeys[_VDom_attrKey(attr)] = true;
        _VDom_applyAttr(dom, attr, dispatch);
    }
    for (var key in oldAttrs) {
        if (!newKeys[key]) { _VDom_unapplyAttr(dom, oldAttrs[key]); }
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

function _VDom_patchKeyed(dom, oldV, newV, dispatch, doc) {
    // Reuse DOM nodes for matching keys; rebuild the child list in order.
    var oldByKey = {};
    for (var i = 0; i < oldV.kids.length; i++) {
        oldByKey[oldV.kids[i].a] = { vnode: oldV.kids[i].b, dom: dom.childNodes[i] };
    }
    var newDoms = [];
    var used = {};
    for (var j = 0; j < newV.kids.length; j++) {
        var key = newV.kids[j].a;
        var newKid = newV.kids[j].b;
        var old = !used[key] && oldByKey[key];
        if (old) {
            used[key] = true;
            newDoms.push(_VDom_patch(old.dom, old.vnode, newKid, dispatch, doc));
        } else {
            newDoms.push(_VDom_render(newKid, dispatch, doc));
        }
    }
    while (dom.childNodes.length > 0) {
        dom.removeChild(dom.childNodes[dom.childNodes.length - 1]);
    }
    for (var n = 0; n < newDoms.length; n++) {
        dom.appendChild(newDoms[n]);
    }
    return dom;
}

// JSON — Elm.Kernel.Json. Decoders are objects with a `run` function from
// a JS value to { ok: true, value } or { ok: false, error }.

function _Json_ok(value) { return { ok: true, value: value }; }
function _Json_err(error) { return { ok: false, error: error }; }
function _Json_failure(msg, value) {
    return _Json_err({ $: 'Failure', a: msg, b: value });
}
function _Json_decoder(run) { return { $: 'Decoder', run: run }; }
function _Json_expecting(what, value) {
    return _Json_failure('Expecting ' + what, value);
}
function _Json_runDecoder(decoder, value) {
    var r = decoder.run(value);
    return r.ok ? $Result$Ok(r.value) : $Result$Err(r.error);
}

var $Json$Decode$string = _Json_decoder(function (v) {
    return typeof v === 'string' ? _Json_ok(v) : _Json_expecting('a STRING', v);
});
var $Json$Decode$int = _Json_decoder(function (v) {
    return typeof v === 'number' && (v | 0) === v ? _Json_ok(v) : _Json_expecting('an INT', v);
});
var $Json$Decode$float = _Json_decoder(function (v) {
    return typeof v === 'number' ? _Json_ok(v) : _Json_expecting('a FLOAT', v);
});
var $Json$Decode$bool = _Json_decoder(function (v) {
    return typeof v === 'boolean' ? _Json_ok(v) : _Json_expecting('a BOOL', v);
});
var $Json$Decode$value = _Json_decoder(_Json_ok);
var $Json$Decode$_null = function (fallback) {
    return _Json_decoder(function (v) {
        return v === null ? _Json_ok(fallback) : _Json_expecting('null', v);
    });
};
var $Json$Decode$succeed = function (x) {
    return _Json_decoder(function (_v) { return _Json_ok(x); });
};
var $Json$Decode$fail = function (msg) {
    return _Json_decoder(function (v) { return _Json_failure(msg, v); });
};
var $Json$Decode$nullable = function (decoder) {
    return _Json_decoder(function (v) {
        if (v === null || v === undefined) { return _Json_ok($Maybe$Nothing); }
        var r = decoder.run(v);
        return r.ok ? _Json_ok($Maybe$Just(r.value)) : r;
    });
};
var $Json$Decode$maybe = function (decoder) {
    return _Json_decoder(function (v) {
        var r = decoder.run(v);
        return _Json_ok(r.ok ? $Maybe$Just(r.value) : $Maybe$Nothing);
    });
};
var $Json$Decode$list = function (decoder) {
    return _Json_decoder(function (v) {
        if (!Array.isArray(v)) { return _Json_expecting('a LIST', v); }
        var out = [];
        for (var i = 0; i < v.length; i++) {
            var r = decoder.run(v[i]);
            if (!r.ok) { return _Json_err({ $: 'Index', a: i, b: r.error }); }
            out.push(r.value);
        }
        return _Json_ok(_List_fromArray(out));
    });
};
var $Json$Decode$oneOrMore = F2(function (toValue, decoder) {
    return _Json_decoder(function (v) {
        var r = $Json$Decode$list(decoder).run(v);
        if (!r.ok) { return r; }
        var arr = _List_toArray(r.value);
        if (arr.length === 0) { return _Json_expecting('a JSON ARRAY with at least ONE element', v); }
        return _Json_ok(A2(toValue, arr[0], _List_fromArray(arr.slice(1))));
    });
});
var $Json$Decode$array = function (decoder) {
    return _Json_decoder(function (v) {
        var r = $Json$Decode$list(decoder).run(v);
        return r.ok ? _Json_ok({ $: 'Array', a: _List_toArray(r.value) }) : r;
    });
};
var $Json$Decode$keyValuePairs = function (decoder) {
    return _Json_decoder(function (v) {
        if (v === null || typeof v !== 'object' || Array.isArray(v)) {
            return _Json_expecting('an OBJECT', v);
        }
        var out = [];
        for (var key in v) {
            var r = decoder.run(v[key]);
            if (!r.ok) { return _Json_err({ $: 'Field', a: key, b: r.error }); }
            out.push({ $: '#2', a: key, b: r.value });
        }
        return _Json_ok(_List_fromArray(out));
    });
};
var $Json$Decode$dict = function (decoder) {
    return _Json_decoder(function (v) {
        var r = $Json$Decode$keyValuePairs(decoder).run(v);
        return r.ok ? _Json_ok($Dict$fromList(r.value)) : r;
    });
};
var $Json$Decode$field = F2(function (name, decoder) {
    return _Json_decoder(function (v) {
        if (v === null || typeof v !== 'object' || Array.isArray(v) || !(name in v)) {
            return _Json_expecting('an OBJECT with a field named `' + name + '`', v);
        }
        var r = decoder.run(v[name]);
        return r.ok ? r : _Json_err({ $: 'Field', a: name, b: r.error });
    });
});
var $Json$Decode$at = F2(function (path, decoder) {
    var names = _List_toArray(path);
    var result = decoder;
    for (var i = names.length; i--;) { result = A2($Json$Decode$field, names[i], result); }
    return result;
});
var $Json$Decode$index = F2(function (i, decoder) {
    return _Json_decoder(function (v) {
        if (!Array.isArray(v)) { return _Json_expecting('an ARRAY', v); }
        if (i >= v.length) {
            return _Json_expecting('a LONGER array — need index ' + i, v);
        }
        var r = decoder.run(v[i]);
        return r.ok ? r : _Json_err({ $: 'Index', a: i, b: r.error });
    });
});
var $Json$Decode$oneOf = function (decoders) {
    var arr = _List_toArray(decoders);
    return _Json_decoder(function (v) {
        var errors = [];
        for (var i = 0; i < arr.length; i++) {
            var r = arr[i].run(v);
            if (r.ok) { return r; }
            errors.push(r.error);
        }
        return _Json_err({ $: 'OneOf', a: _List_fromArray(errors) });
    });
};
var $Json$Decode$lazy = function (thunk) {
    return _Json_decoder(function (v) { return thunk(_Utils_Tuple0).run(v); });
};
var $Json$Decode$map = F2(function (f, d) {
    return _Json_decoder(function (v) {
        var r = d.run(v);
        return r.ok ? _Json_ok(f(r.value)) : r;
    });
});
function _Json_mapMany(f, decoders) {
    return _Json_decoder(function (v) {
        var result = f;
        for (var i = 0; i < decoders.length; i++) {
            var r = decoders[i].run(v);
            if (!r.ok) { return r; }
            result = result(r.value);
        }
        return _Json_ok(result);
    });
}
var $Json$Decode$map2 = F3(function (f, a, b) { return _Json_mapMany(f, [a, b]); });
var $Json$Decode$map3 = F4(function (f, a, b, c) { return _Json_mapMany(f, [a, b, c]); });
var $Json$Decode$map4 = F5(function (f, a, b, c, d) { return _Json_mapMany(f, [a, b, c, d]); });
var $Json$Decode$map5 = F6(function (f, a, b, c, d, e) { return _Json_mapMany(f, [a, b, c, d, e]); });
var $Json$Decode$map6 = F7(function (f, a, b, c, d, e, g) { return _Json_mapMany(f, [a, b, c, d, e, g]); });
var $Json$Decode$map7 = function (f) { return function (a) { return function (b) { return function (c) { return function (d) { return function (e) { return function (g) { return function (h) { return _Json_mapMany(f, [a, b, c, d, e, g, h]); }; }; }; }; }; }; }; };
var $Json$Decode$map8 = function (f) { return function (a) { return function (b) { return function (c) { return function (d) { return function (e) { return function (g) { return function (h) { return function (i) { return _Json_mapMany(f, [a, b, c, d, e, g, h, i]); }; }; }; }; }; }; }; }; };
var $Json$Decode$andThen = F2(function (f, d) {
    return _Json_decoder(function (v) {
        var r = d.run(v);
        return r.ok ? f(r.value).run(v) : r;
    });
});
var $Json$Decode$decodeValue = F2(_Json_runDecoder);
var $Json$Decode$decodeString = F2(function (decoder, str) {
    try {
        var v = JSON.parse(str);
    } catch (e) {
        return $Result$Err({ $: 'Failure', a: 'This is not valid JSON! ' + e.message, b: str });
    }
    return _Json_runDecoder(decoder, v);
});
var $Json$Decode$errorToString = function (error) {
    switch (error.$) {
        case 'Field':
            return 'Problem with the value at .' + error.a + ':\n' + $Json$Decode$errorToString(error.b);
        case 'Index':
            return 'Problem with the value at [' + error.a + ']:\n' + $Json$Decode$errorToString(error.b);
        case 'OneOf': {
            var errors = _List_toArray(error.a);
            return 'All possibilities failed:\n' + errors.map($Json$Decode$errorToString).join('\n');
        }
        default:
            return error.a + '\n\n' + JSON.stringify(error.b, null, 4);
    }
};

var $Json$Encode$string = function (s) { return s; };
var $Json$Encode$int = function (n) { return n; };
var $Json$Encode$float = function (n) { return n; };
var $Json$Encode$bool = function (b) { return b; };
var $Json$Encode$_null = null;
var $Json$Encode$list = F2(function (encodeItem, items) {
    return _List_toArray(items).map(function (x) { return encodeItem(x); });
});
var $Json$Encode$array = F2(function (encodeItem, arr) {
    return arr.a.map(function (x) { return encodeItem(x); });
});
var $Json$Encode$set = F2(function (encodeItem, set) {
    return $Dict$keys(set.d) === undefined ? [] : _List_toArray($Dict$keys(set.d)).map(function (x) { return encodeItem(x); });
});
var $Json$Encode$object = function (pairs) {
    var out = {};
    for (var xs = pairs; xs.$ === '::'; xs = xs.b) { out[xs.a.a] = xs.a.b; }
    return out;
};
var $Json$Encode$dict = F3(function (toKey, toValue, dict) {
    var out = {};
    for (var i = 0; i < dict.keys.length; i++) {
        out[toKey(dict.keys[i])] = toValue(dict.vals[i]);
    }
    return out;
});
var $Json$Encode$encode = F2(function (indent, value) {
    return JSON.stringify(value === undefined ? null : value, null, indent) || 'null';
});

// TASKS — CPS-style: { fork: function (onSuccess, onFailure) }.

function _Task(fork) { return { $: 'Task', fork: fork }; }
var $Task$succeed = function (value) {
    return _Task(function (ok, _err) { ok(value); });
};
var $Task$fail = function (error) {
    return _Task(function (_ok, err) { err(error); });
};
var $Task$andThen = F2(function (f, task) {
    return _Task(function (ok, err) {
        task.fork(function (a) { f(a).fork(ok, err); }, err);
    });
});
var $Task$onError = F2(function (f, task) {
    return _Task(function (ok, err) {
        task.fork(ok, function (x) { f(x).fork(ok, err); });
    });
});
var $Task$map = F2(function (f, task) {
    return _Task(function (ok, err) {
        task.fork(function (a) { ok(f(a)); }, err);
    });
});
var $Task$map2 = F3(function (f, ta, tb) {
    return A2($Task$andThen, function (a) {
        return A2($Task$map, function (b) { return A2(f, a, b); }, tb);
    }, ta);
});
var $Task$map3 = F4(function (f, ta, tb, tc) {
    return A2($Task$andThen, function (a) {
        return A3($Task$map2, function (b, c) { return A3(f, a, b, c); }, tb, tc);
    }, ta);
});
var $Task$map4 = F5(function (f, ta, tb, tc, td) {
    return A2($Task$andThen, function (a) {
        return A4($Task$map3, function (b, c, d) { return A4(f, a, b, c, d); }, tb, tc, td);
    }, ta);
});
var $Task$map5 = F6(function (f, ta, tb, tc, td, te) {
    return A2($Task$andThen, function (a) {
        return A5($Task$map4, function (b, c, d, e) { return A5(f, a, b, c, d, e); }, tb, tc, td, te);
    }, ta);
});
var $Task$mapError = F2(function (f, task) {
    return _Task(function (ok, err) {
        task.fork(ok, function (x) { err(f(x)); });
    });
});
var $Task$sequence = function (tasks) {
    var arr = _List_toArray(tasks);
    return _Task(function (ok, err) {
        var results = [];
        function step(i) {
            if (i >= arr.length) { return ok(_List_fromArray(results)); }
            arr[i].fork(function (v) { results.push(v); step(i + 1); }, err);
        }
        step(0);
    });
};
var $Task$perform = F2(function (toMsg, task) {
    return { $: 'CmdTask', task: A2($Task$map, toMsg, task) };
});
var $Task$attempt = F2(function (toMsg, task) {
    return {
        $: 'CmdTask',
        task: _Task(function (ok, _err) {
            task.fork(
                function (v) { ok(toMsg($Result$Ok(v))); },
                function (x) { ok(toMsg($Result$Err(x))); }
            );
        })
    };
});

var $Process$sleep = function (ms) {
    return _Task(function (ok, _err) {
        setTimeout(function () { ok(_Utils_Tuple0); }, ms);
    });
};

var $Terminal$writeLine = function (s) { return { $: 'CmdWrite', s: s }; };

// TIME

function _Time_posix(ms) { return { $: 'Posix', ms: ms }; }
var $Time$millisToPosix = function (ms) { return _Time_posix(ms); };
var $Time$posixToMillis = function (posix) { return posix.ms; };
var $Time$utc = { $: 'Zone', offset: 0, eras: [] };
var $Time$customZone = F2(function (offset, eras) {
    return { $: 'Zone', offset: offset, eras: _List_toArray(eras) };
});
var $Time$here = _Task(function (ok, _err) {
    ok({ $: 'Zone', offset: -new Date().getTimezoneOffset(), eras: [] });
});
var $Time$now = _Task(function (ok, _err) { ok(_Time_posix(Date.now())); });
var $Time$getZoneName = _Task(function (ok, _err) {
    try {
        ok({ $: 'Name', a: Intl.DateTimeFormat().resolvedOptions().timeZone });
    } catch (e) {
        ok({ $: 'Offset', a: -new Date().getTimezoneOffset() });
    }
});
var $Time$every = F2(function (interval, toMsg) {
    return { $: 'SubTime', interval: interval, toMsg: toMsg };
});
function _Time_toAdjusted(zone, posix) {
    var minutes = posix.ms / 60000;
    var offset = zone.offset;
    for (var i = 0; i < zone.eras.length; i++) {
        if (zone.eras[i].start < minutes) { offset = zone.eras[i].offset; break; }
    }
    return new Date(posix.ms + offset * 60000);
}
var $Time$toYear = F2(function (zone, posix) { return _Time_toAdjusted(zone, posix).getUTCFullYear(); });
var _Time_months = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];
var $Time$toMonth = F2(function (zone, posix) {
    return { $: _Time_months[_Time_toAdjusted(zone, posix).getUTCMonth()] };
});
var $Time$toDay = F2(function (zone, posix) { return _Time_toAdjusted(zone, posix).getUTCDate(); });
var $Time$toHour = F2(function (zone, posix) { return _Time_toAdjusted(zone, posix).getUTCHours(); });
var $Time$toMinute = F2(function (zone, posix) { return _Time_toAdjusted(zone, posix).getUTCMinutes(); });
var $Time$toSecond = F2(function (zone, posix) { return _Time_toAdjusted(zone, posix).getUTCSeconds(); });
var $Time$toMillis = F2(function (zone, posix) { return _Time_toAdjusted(zone, posix).getUTCMilliseconds(); });
var _Time_weekdays = ['Sun', 'Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat'];
var $Time$toWeekday = F2(function (zone, posix) {
    return { $: _Time_weekdays[_Time_toAdjusted(zone, posix).getUTCDay()] };
});

// HTTP — fetch-based.

var $Http$header = F2(function (name, value) { return { name: name, value: value }; });
var $Http$emptyBody = { contentType: null, content: null };
var $Http$stringBody = F2(function (contentType, content) {
    return { contentType: contentType, content: content };
});
var $Http$fileBody = function (file) {
    return { contentType: file.type || 'application/octet-stream', content: file };
};
var $Http$jsonBody = function (value) {
    return { contentType: 'application/json', content: JSON.stringify(value) };
};
var $Http$stringPart = F2(function (name, value) { return { $: 'StringPart', name: name, value: value }; });
var $Http$filePart = F2(function (name, file) { return { $: 'FilePart', name: name, file: file }; });
var $Http$multipartBody = function (parts) {
    return { contentType: 'multipart', parts: _List_toArray(parts) };
};
var $Http$expectString = function (toMsg) {
    return { toMsg: toMsg, handle: function (response) {
        return _Http_defaultHandle(response, function (body) { return $Result$Ok(body); });
    } };
};
var $Http$expectWhatever = function (toMsg) {
    return { toMsg: toMsg, handle: function (response) {
        return _Http_defaultHandle(response, function (_body) { return $Result$Ok(_Utils_Tuple0); });
    } };
};
var $Http$expectStringResponse = F2(function (toMsg, toResult) {
    return { toMsg: toMsg, handle: function (response) { return toResult(response); } };
});
var $Http$expectBytes = F2(function (toMsg, _decoder) {
    return { toMsg: toMsg, handle: function (response) {
        return _Http_defaultHandle(response, function (body) { return $Result$Ok(body); });
    } };
});
var $Http$expectBytesResponse = F2(function (toMsg, toResult) {
    return { toMsg: toMsg, handle: function (response) { return toResult(response); } };
});
var $Http$bytesBody = F2(function (mime, bytes) { return { $: 'StringBody', mime: mime, body: bytes }; });
var $Http$bytesPart = F3(function (name, mime, bytes) { return { $: 'Part', name: name, mime: mime, body: bytes }; });
var $Http$expectJson = F2(function (toMsg, decoder) {
    return { toMsg: toMsg, handle: function (response) {
        return _Http_defaultHandle(response, function (body) {
            var r = A2($Json$Decode$decodeString, decoder, body);
            return r.$ === 'Ok'
                ? r
                : $Result$Err({ $: 'BadBody', a: $Json$Decode$errorToString(r.a) });
        });
    } };
});
function _Http_defaultHandle(response, onGood) {
    switch (response.$) {
        case 'BadUrl_': return $Result$Err({ $: 'BadUrl', a: response.a });
        case 'Timeout_': return $Result$Err({ $: 'Timeout' });
        case 'NetworkError_': return $Result$Err({ $: 'NetworkError' });
        case 'BadStatus_': return $Result$Err({ $: 'BadStatus', a: response.a.statusCode });
        default: return onGood(response.b);
    }
}
function _Http_makeTask(config, handle) {
    return _Task(function (ok, _err) {
        var headers = {};
        for (var i = 0; i < config.headers.length; i++) {
            headers[config.headers[i].name] = config.headers[i].value;
        }
        var body = null;
        if (config.body.contentType === 'multipart') {
            var form = new FormData();
            config.body.parts.forEach(function (part) {
                if (part.$ === 'StringPart') { form.append(part.name, part.value); }
                else { form.append(part.name, part.file); }
            });
            body = form;
        } else if (config.body.content !== null) {
            body = config.body.content;
            if (config.body.contentType) { headers['Content-Type'] = config.body.contentType; }
        }
        var controller = typeof AbortController !== 'undefined' ? new AbortController() : null;
        var timer = null;
        if (config.timeout && config.timeout.$ === 'Just' && controller) {
            timer = setTimeout(function () { controller.abort(); }, config.timeout.a);
        }
        fetch(config.url, {
            method: config.method,
            headers: headers,
            body: config.method === 'GET' || config.method === 'HEAD' ? undefined : body,
            signal: controller ? controller.signal : undefined
        }).then(function (response) {
            return response.text().then(function (text) {
                if (timer) { clearTimeout(timer); }
                var metadata = {
                    url: response.url,
                    statusCode: response.status,
                    statusText: response.statusText,
                    headers: $Dict$empty
                };
                ok(response.ok
                    ? { $: 'GoodStatus_', a: metadata, b: text }
                    : { $: 'BadStatus_', a: metadata, b: text });
            });
        }).catch(function (e) {
            if (timer) { clearTimeout(timer); }
            ok(e.name === 'AbortError' ? { $: 'Timeout_' } : { $: 'NetworkError_' });
        });
    });
}
var $Http$request = function (config) {
    return { $: 'CmdHttp', config: config };
};
var $Http$riskyRequest = $Http$request;
var $Http$get = function (config) {
    return $Http$request({
        method: 'GET', headers: [], url: config.url, body: $Http$emptyBody,
        expect: config.expect, timeout: $Maybe$Nothing, tracker: $Maybe$Nothing
    });
};
var $Http$post = function (config) {
    return $Http$request({
        method: 'POST', headers: [], url: config.url, body: config.body,
        expect: config.expect, timeout: $Maybe$Nothing, tracker: $Maybe$Nothing
    });
};
var $Http$stringResolver = function (toResult) { return { toResult: toResult }; };
var $Http$bytesResolver = function (toResult) { return { toResult: toResult }; };
var $Http$track = F2(function (_tracker, _toMsg) { return $Platform$Sub$none; });
var $Http$cancel = function (_tracker) { return $Platform$Cmd$none; };
var $Http$fractionSent = function (p) {
    return p.size > 0 ? p.sent / p.size : 1;
};
var $Http$fractionReceived = function (p) {
    return p.size.$ === 'Just' && p.size.a > 0 ? p.received / p.size.a : 0;
};
var $Http$task = function (config) {
    var cfg = {
        method: config.method,
        headers: _List_toArray(config.headers),
        url: config.url,
        body: config.body,
        timeout: config.timeout
    };
    return A2($Task$andThen, function (response) {
        var r = config.resolver.toResult(response);
        return r.$ === 'Ok' ? $Task$succeed(r.a) : $Task$fail(r.a);
    }, _Http_makeTask(cfg, null));
};
var $Http$riskyTask = $Http$task;

// FILE

var $File$decoder = _Json_decoder(function (v) {
    return v && typeof v === 'object' ? _Json_ok(v) : _Json_expecting('a FILE', v);
});
var $File$name = function (file) { return file.name; };
var $File$size = function (file) { return file.size; };
var $File$mime = function (file) { return file.type; };
var $File$lastModified = function (file) { return _Time_posix(file.lastModified || 0); };
var $File$toString = function (file) {
    return _Task(function (ok, _err) {
        if (file && typeof file.text === 'function') { file.text().then(ok); } else { ok(''); }
    });
};
var $File$toUrl = function (file) {
    return _Task(function (ok, _err) {
        if (typeof FileReader !== 'undefined') {
            var r = new FileReader();
            r.onload = function () { ok(r.result); };
            r.readAsDataURL(file);
        } else { ok(''); }
    });
};
var $File$toBytes = function (_file) {
    return _Task(function (ok, _err) { ok(null); });
};

// URL

var $Url$percentEncode = function (s) { return encodeURIComponent(s); };
var $Url$percentDecode = function (s) {
    try { return $Maybe$Just(decodeURIComponent(s)); }
    catch (e) { return $Maybe$Nothing; }
};
var $Url$fromString = function (str) {
    var match = /^(https?):\/\/([^/:?#]*)(?::(\d+))?([^?#]*)(?:\?([^#]*))?(?:#(.*))?$/.exec(str);
    if (!match) { return $Maybe$Nothing; }
    return $Maybe$Just({
        protocol: match[1] === 'https' ? { $: 'Https' } : { $: 'Http' },
        host: match[2],
        port_: match[3] ? $Maybe$Just(parseInt(match[3], 10)) : $Maybe$Nothing,
        path: match[4] || '/',
        query: match[5] !== undefined ? $Maybe$Just(match[5]) : $Maybe$Nothing,
        fragment: match[6] !== undefined ? $Maybe$Just(match[6]) : $Maybe$Nothing
    });
};
var $Url$toString = function (url) {
    var s = (url.protocol.$ === 'Https' ? 'https' : 'http') + '://' + url.host;
    if (url.port_.$ === 'Just') { s += ':' + url.port_.a; }
    s += url.path;
    if (url.query.$ === 'Just') { s += '?' + url.query.a; }
    if (url.fragment.$ === 'Just') { s += '#' + url.fragment.a; }
    return s;
};

// BROWSER.DOM

function _Dom_byId(id, andThen) {
    return _Task(function (ok, err) {
        var node = typeof document !== 'undefined' && document.getElementById
            ? document.getElementById(id)
            : null;
        if (!node) { return err({ $: 'NotFound', a: id }); }
        ok(andThen(node));
    });
}
var $Browser$Dom$focus = function (id) {
    return _Dom_byId(id, function (node) {
        if (node.focus) { node.focus(); }
        return _Utils_Tuple0;
    });
};
var $Browser$Dom$blur = function (id) {
    return _Dom_byId(id, function (node) {
        if (node.blur) { node.blur(); }
        return _Utils_Tuple0;
    });
};
var $Browser$Dom$getViewport = _Task(function (ok, _err) {
    var w = typeof window !== 'undefined' ? window : { pageXOffset: 0, pageYOffset: 0, innerWidth: 0, innerHeight: 0 };
    ok({
        scene: { width: w.innerWidth || 0, height: w.innerHeight || 0 },
        viewport: { x: w.pageXOffset || 0, y: w.pageYOffset || 0, width: w.innerWidth || 0, height: w.innerHeight || 0 }
    });
});
var $Browser$Dom$setViewport = F2(function (x, y) {
    return _Task(function (ok, _err) {
        if (typeof window !== 'undefined' && window.scroll) { window.scroll(x, y); }
        ok(_Utils_Tuple0);
    });
});
var $Browser$Dom$getViewportOf = function (id) {
    return _Dom_byId(id, function (node) {
        return {
            scene: { width: node.scrollWidth || 0, height: node.scrollHeight || 0 },
            viewport: {
                x: node.scrollLeft || 0, y: node.scrollTop || 0,
                width: node.clientWidth || 0, height: node.clientHeight || 0
            }
        };
    });
};
var $Browser$Dom$setViewportOf = F3(function (id, x, y) {
    return _Dom_byId(id, function (node) {
        node.scrollLeft = x;
        node.scrollTop = y;
        return _Utils_Tuple0;
    });
});
var $Browser$Dom$getElement = function (id) {
    return _Dom_byId(id, function (node) {
        var rect = node.getBoundingClientRect ? node.getBoundingClientRect() : { left: 0, top: 0, width: 0, height: 0 };
        var x = typeof window !== 'undefined' ? window.pageXOffset : 0;
        var y = typeof window !== 'undefined' ? window.pageYOffset : 0;
        return {
            scene: { width: 0, height: 0 },
            viewport: { x: x, y: y, width: 0, height: 0 },
            element: { x: x + rect.left, y: y + rect.top, width: rect.width, height: rect.height }
        };
    });
};

// BROWSER.EVENTS

function _Browser_on(name, decoder) {
    return { $: 'SubDom', name: name, decoder: decoder };
}
var $Browser$Events$onKeyDown = function (d) { return _Browser_on('keydown', d); };
var $Browser$Events$onKeyUp = function (d) { return _Browser_on('keyup', d); };
var $Browser$Events$onKeyPress = function (d) { return _Browser_on('keypress', d); };
var $Browser$Events$onClick = function (d) { return _Browser_on('click', d); };
var $Browser$Events$onMouseMove = function (d) { return _Browser_on('mousemove', d); };
var $Browser$Events$onMouseDown = function (d) { return _Browser_on('mousedown', d); };
var $Browser$Events$onMouseUp = function (d) { return _Browser_on('mouseup', d); };
var $Browser$Events$onResize = function (toMsg) {
    return _Browser_on('resize', _Json_decoder(function (_e) {
        var w = typeof window !== 'undefined' ? window : { innerWidth: 0, innerHeight: 0 };
        return _Json_ok(A2(toMsg, w.innerWidth, w.innerHeight));
    }));
};
var $Browser$Events$onAnimationFrameDelta = function (toMsg) {
    return { $: 'SubAnimation', toMsg: toMsg, delta: true };
};
var $Browser$Events$onAnimationFrame = function (toMsg) {
    return { $: 'SubAnimation', toMsg: toMsg, delta: false };
};
var $Browser$Events$onVisibilityChange = function (toMsg) {
    return _Browser_on('visibilitychange', _Json_decoder(function (_e) {
        var hidden = typeof document !== 'undefined' && document.hidden;
        return _Json_ok(toMsg(hidden ? $Browser$Events$Hidden : $Browser$Events$Visible));
    }));
};

// BROWSER.NAVIGATION

var $Browser$Navigation$load = function (url) { return { $: 'CmdLoad', url: url }; };
var $Browser$Navigation$reload = { $: 'CmdReload' };
var $Browser$Navigation$reloadAndSkipCache = { $: 'CmdReload' };

// RANDOM — generators as seed -> [value, seed] functions.

function _Random_next(seed) {
    // mulberry32
    var t = (seed + 0x6D2B79F5) | 0;
    var r = Math.imul(t ^ (t >>> 15), 1 | t);
    r = (r + Math.imul(r ^ (r >>> 7), 61 | r)) ^ r;
    return { state: t, value: ((r ^ (r >>> 14)) >>> 0) / 4294967296 };
}
function _Random_gen(fn) { return { $: 'Generator', gen: fn }; }
var $Random$minInt = -2147483648;
var $Random$maxInt = 2147483647;
var $Random$initialSeed = function (n) { return { $: 'Seed', state: n | 0 }; };
var $Random$int = F2(function (lo, hi) {
    return _Random_gen(function (seed) {
        var next = _Random_next(seed.state);
        var value = lo + Math.floor(next.value * (hi - lo + 1));
        return [value, { $: 'Seed', state: next.state }];
    });
});
var $Random$float = F2(function (lo, hi) {
    return _Random_gen(function (seed) {
        var next = _Random_next(seed.state);
        return [lo + next.value * (hi - lo), { $: 'Seed', state: next.state }];
    });
});
var $Random$constant = function (x) {
    return _Random_gen(function (seed) { return [x, seed]; });
};
var $Random$map = F2(function (f, g) {
    return _Random_gen(function (seed) {
        var r = g.gen(seed);
        return [f(r[0]), r[1]];
    });
});
var $Random$map2 = F3(function (f, ga, gb) {
    return _Random_gen(function (seed) {
        var ra = ga.gen(seed);
        var rb = gb.gen(ra[1]);
        return [A2(f, ra[0], rb[0]), rb[1]];
    });
});
var $Random$map3 = F4(function (f, ga, gb, gc) {
    return _Random_gen(function (seed) {
        var ra = ga.gen(seed), rb = gb.gen(ra[1]), rc = gc.gen(rb[1]);
        return [A3(f, ra[0], rb[0], rc[0]), rc[1]];
    });
});
var $Random$map4 = F5(function (f, ga, gb, gc, gd) {
    return _Random_gen(function (seed) {
        var ra = ga.gen(seed), rb = gb.gen(ra[1]), rc = gc.gen(rb[1]), rd = gd.gen(rc[1]);
        return [A4(f, ra[0], rb[0], rc[0], rd[0]), rd[1]];
    });
});
var $Random$map5 = F6(function (f, ga, gb, gc, gd, ge) {
    return _Random_gen(function (seed) {
        var ra = ga.gen(seed), rb = gb.gen(ra[1]), rc = gc.gen(rb[1]), rd = gd.gen(rc[1]), re = ge.gen(rd[1]);
        return [A5(f, ra[0], rb[0], rc[0], rd[0], re[0]), re[1]];
    });
});
var $Random$andThen = F2(function (f, g) {
    return _Random_gen(function (seed) {
        var r = g.gen(seed);
        return f(r[0]).gen(r[1]);
    });
});
var $Random$lazy = function (thunk) {
    return _Random_gen(function (seed) { return thunk(_Utils_Tuple0).gen(seed); });
};
var $Random$pair = F2(function (ga, gb) {
    return _Random_gen(function (seed) {
        var ra = ga.gen(seed), rb = gb.gen(ra[1]);
        return [{ $: '#2', a: ra[0], b: rb[0] }, rb[1]];
    });
});
var $Random$uniform = F2(function (head, tail) {
    var arr = [head].concat(_List_toArray(tail));
    return _Random_gen(function (seed) {
        var next = _Random_next(seed.state);
        var i = Math.floor(next.value * arr.length);
        if (i >= arr.length) { i = arr.length - 1; }
        return [arr[i], { $: 'Seed', state: next.state }];
    });
});
var $Random$weighted = F2(function (headPair, tailPairs) {
    var pairs = [headPair].concat(_List_toArray(tailPairs));
    var total = 0;
    for (var i = 0; i < pairs.length; i++) { total += Math.abs(pairs[i].a); }
    return _Random_gen(function (seed) {
        var next = _Random_next(seed.state);
        var target = next.value * total;
        var acc = 0;
        for (var j = 0; j < pairs.length; j++) {
            acc += Math.abs(pairs[j].a);
            if (target <= acc) { return [pairs[j].b, { $: 'Seed', state: next.state }]; }
        }
        return [pairs[pairs.length - 1].b, { $: 'Seed', state: next.state }];
    });
});
var $Random$independentSeed = _Random_gen(function (seed) {
    var a = _Random_next(seed.state);
    var b = _Random_next(a.state);
    return [{ $: 'Seed', state: a.state }, { $: 'Seed', state: b.state }];
});
var $Random$list = F2(function (n, g) {
    return _Random_gen(function (seed) {
        var out = [];
        for (var i = 0; i < n; i++) {
            var r = g.gen(seed);
            out.push(r[0]);
            seed = r[1];
        }
        return [_List_fromArray(out), seed];
    });
});
var $Random$step = F2(function (g, seed) {
    var r = g.gen(seed);
    return { $: '#2', a: r[0], b: r[1] };
});
var $Random$generate = F2(function (toMsg, g) {
    return {
        $: 'CmdTask',
        task: _Task(function (ok, _err) {
            var seed = { $: 'Seed', state: (Math.random() * 0xFFFFFFFF) | 0 };
            ok(toMsg(g.gen(seed)[0]));
        })
    };
});

// UUID

var $UUID$generator = _Random_gen(function (seed) {
    var hex = '';
    var s = seed;
    for (var i = 0; i < 32; i++) {
        var next = _Random_next(s.state !== undefined ? s.state : s);
        s = { state: next.state };
        hex += ((next.value * 16) | 0).toString(16);
    }
    var uuid = hex.slice(0, 8) + '-' + hex.slice(8, 12) + '-4' + hex.slice(13, 16) +
        '-' + ((parseInt(hex[16], 16) & 3 | 8)).toString(16) + hex.slice(17, 20) + '-' + hex.slice(20, 32);
    return [{ $: 'UUID', s: uuid }, { $: 'Seed', state: s.state }];
});
var $UUID$toString = function (uuid) { return uuid.s; };
var $UUID$compare = F2(function (a, b) { return A2($Basics$compare, a.s, b.s); });
var $UUID$toRepresentation = F2(function (representation, uuid) {
    switch (representation.$) {
        case 'Compact': return uuid.s.replace(/-/g, '');
        case 'Guid': return '{' + uuid.s + '}';
        case 'Urn': return 'urn:uuid:' + uuid.s;
        default: return uuid.s;
    }
});
var $UUID$fromString = function (s) {
    var normalized = s.trim().toLowerCase();
    return /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/.test(normalized)
        ? $Result$Ok({ $: 'UUID', s: normalized })
        : $Result$Err({ $: 'WrongFormat' });
};
var $UUID$jsonDecoder = _Json_decoder(function (v) {
    if (typeof v !== 'string') { return _Json_expecting('a UUID string', v); }
    var r = $UUID$fromString(v);
    return r.$ === 'Ok' ? _Json_ok(r.a) : _Json_failure('Invalid UUID', v);
});
var $UUID$toValue = function (uuid) { return uuid.s; };

// ELM.KERNEL.PARSER — the primitives behind elm/parser, ported from
// its kernel JavaScript so the package compiles from source.

var $Elm$Kernel$Parser$isSubString = F5(function (smallString, offset, row, col, bigString) {
    var smallLength = smallString.length;
    var isGood = offset + smallLength <= bigString.length;
    for (var i = 0; isGood && i < smallLength;) {
        var code = bigString.charCodeAt(offset);
        isGood = smallString[i++] === bigString[offset++]
            && (code === 0x000A
                ? (row++, col = 1, true)
                : (col++, (code & 0xF800) === 0xD800 ? smallString[i++] === bigString[offset++] : true));
    }
    return { $: '#3', a: isGood ? offset : -1, b: row, c: col };
});
var $Elm$Kernel$Parser$isSubChar = F3(function (predicate, offset, string) {
    return string.length <= offset
        ? -1
        : (string.charCodeAt(offset) & 0xF800) === 0xD800
            ? (predicate(string.substr(offset, 2)) ? offset + 2 : -1)
            : (predicate(string[offset])
                ? (string[offset] === '\n' ? -2 : offset + 1)
                : -1);
});
var $Elm$Kernel$Parser$isAsciiCode = F3(function (code, offset, string) {
    return string.charCodeAt(offset) === code;
});
var $Elm$Kernel$Parser$chompBase10 = F2(function (offset, string) {
    for (; offset < string.length; offset++) {
        var code = string.charCodeAt(offset);
        if (code < 0x30 || 0x39 < code) { return offset; }
    }
    return offset;
});
var $Elm$Kernel$Parser$consumeBase = F3(function (base, offset, string) {
    for (var total = 0; offset < string.length; offset++) {
        var digit = string.charCodeAt(offset) - 0x30;
        if (digit < 0 || base <= digit) { break; }
        total = base * total + digit;
    }
    return { $: '#2', a: offset, b: total };
});
var $Elm$Kernel$Parser$consumeBase16 = F2(function (offset, string) {
    for (var total = 0; offset < string.length; offset++) {
        var code = string.charCodeAt(offset);
        if (0x30 <= code && code <= 0x39) {
            total = 16 * total + code - 0x30;
        } else if (0x41 <= code && code <= 0x46) {
            total = 16 * total + code - 55;
        } else if (0x61 <= code && code <= 0x66) {
            total = 16 * total + code - 87;
        } else {
            break;
        }
    }
    return { $: '#2', a: offset, b: total };
});
var $Elm$Kernel$Parser$findSubString = F5(function (smallString, offset, row, col, bigString) {
    var newOffset = bigString.indexOf(smallString, offset);
    var target = newOffset < 0 ? bigString.length : newOffset + smallString.length;
    while (offset < target) {
        var code = bigString.charCodeAt(offset++);
        code === 0x000A
            ? (col = 1, row++)
            : (col++, (code & 0xF800) === 0xD800 && offset++);
    }
    return { $: '#3', a: newOffset, b: row, c: col };
});

// COMMANDS AND SUBSCRIPTIONS

var $Platform$Cmd$none = { $: 'CmdNone' };
var $Platform$Cmd$batch = function (cmds) { return { $: 'CmdBatch', cmds: _List_toArray(cmds) }; };
var $Platform$Cmd$map = F2(function (f, cmd) { return { $: 'CmdMap', f: f, cmd: cmd }; });
var $Platform$Sub$none = { $: 'SubNone' };
var $Platform$Sub$batch = function (subs) { return { $: 'SubBatch', subs: _List_toArray(subs) }; };
var $Platform$Sub$map = F2(function (f, sub) { return { $: 'SubMap', f: f, sub: sub }; });

// PORTS

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

// PROGRAMS

var $Browser$sandbox = function (impl) {
    return { $: 'Program', kind: 'sandbox', impl: impl };
};
var $Browser$element = function (impl) {
    return { $: 'Program', kind: 'element', impl: impl };
};
var $Browser$document = function (impl) {
    return { $: 'Program', kind: 'document', impl: impl };
};
var $Browser$application = function (impl) {
    return { $: 'Program', kind: 'application', impl: impl };
};
var $Platform$worker = function (impl) {
    return { $: 'Program', kind: 'worker', impl: impl };
};

var $Browser$Navigation$pushUrl = F2(function (_key, url) {
    return { $: 'CmdNav', kind: 'push', url: url };
});
var $Browser$Navigation$replaceUrl = F2(function (_key, url) {
    return { $: 'CmdNav', kind: 'replace', url: url };
});
var $Browser$Navigation$back = F2(function (_key, n) {
    return { $: 'CmdNav', kind: 'go', n: -n };
});
var $Browser$Navigation$forward = F2(function (_key, n) {
    return { $: 'CmdNav', kind: 'go', n: n };
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
    var activeTimers = [];     // { interval, id }
    var animationFrame = null;

    function dispatch(msg) {
        if (isSandbox) {
            model = A2(impl.update, msg, model);
        } else {
            var next = A2(impl.update, msg, model);
            model = next.a;
            runCmd(next.b, function (m) { return m; });
        }
        if (view) {
            var newVnode = view(model);
            root = _VDom_patch(root, vnode, newVnode, dispatch, doc);
            vnode = newVnode;
        }
        updateSubs();
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
                cmd.task.fork(
                    function (msg) { dispatch(tagger(msg)); },
                    function (x) {
                        throw new Error('Task failed without an error handler: ' + _Debug_toString(x));
                    }
                );
                return;
            case 'CmdHttp': {
                var cfg = {
                    method: cmd.config.method,
                    headers: _List_toArray(cmd.config.headers),
                    url: cmd.config.url,
                    body: cmd.config.body,
                    timeout: cmd.config.timeout
                };
                _Http_makeTask(cfg, null).fork(function (response) {
                    var result = cmd.config.expect.handle(response);
                    dispatch(tagger(cmd.config.expect.toMsg(result)));
                }, function () {});
                return;
            }
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
            case 'SubTime':
                acc.timers.push({ interval: sub.interval, toMsg: sub.toMsg, tagger: tagger });
                return;
            case 'SubAnimation':
                acc.animation.push({ toMsg: sub.toMsg, delta: sub.delta, tagger: tagger });
                return;
        }
    }

    function updateSubs() {
        var acc = { ports: {}, dom: [], timers: [], animation: [] };
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

        // Timers.
        activeTimers.forEach(function (t) { clearInterval(t.id); });
        activeTimers = acc.timers.map(function (spec) {
            return {
                id: setInterval(function () {
                    dispatch(spec.tagger(spec.toMsg(_Time_posix(Date.now()))));
                }, spec.interval)
            };
        });

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
