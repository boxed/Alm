// alm runtime kernel — the subset of Elm's Kernel/*.js that alm's
// built-in modules need.

// CURRIED FUNCTION HELPERS

function F(arity, fun, wrapper) { wrapper.a = arity; wrapper.f = fun; return wrapper; }
function F2(fun) { return F(2, fun, function (a) { return function (b) { return fun(a, b); }; }); }
function F3(fun) { return F(3, fun, function (a) { return function (b) { return function (c) { return fun(a, b, c); }; }; }); }
function F4(fun) { return F(4, fun, function (a) { return function (b) { return function (c) { return function (d) { return fun(a, b, c, d); }; }; }; }); }
function A2(f, a, b) { return f.a === 2 ? f.f(a, b) : f(a)(b); }
function A3(f, a, b, c) { return f.a === 3 ? f.f(a, b, c) : f(a)(b)(c); }
function A4(f, a, b, c, d) { return f.a === 4 ? f.f(a, b, c, d) : f(a)(b)(c)(d); }

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
