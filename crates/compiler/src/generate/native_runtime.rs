//! alm native runtime — the Rust twin of the JS backend's `runtime.js`.
//!
//! This is a standalone file compiled by `build.rs` into a static library
//! (`libalm_runtime.a`) and linked into every native binary the compiler
//! produces. It is NOT a module of the compiler crate.
//!
//! Every Elm value is a boxed, immutable [`Value`] behind a raw pointer,
//! matching the uniform representation the LLVM codegen assumes. All the
//! entry points the generated code calls are `extern "C"` with the exact
//! signatures declared in `generate::native`. Memory is allocated and
//! never freed for now; reference counting is a planned pass.
//!
//! Compiled with `panic = abort`, so a Rust panic never unwinds across the
//! C ABI boundary into generated code.

#![allow(non_upper_case_globals, non_snake_case, static_mut_refs)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::UnsafeCell;
use std::ffi::CStr;
use std::io::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

// BUMP ALLOCATOR
//
// The runtime never frees (reference counting is a future pass), so a
// pointer-bump allocator over large chunks is both correct and far faster
// than hitting the system malloc for every boxed value. Single-threaded,
// matching the runtime.

struct Bump;

static mut BUMP_CUR: usize = 0;
static mut BUMP_END: usize = 0;
const BUMP_CHUNK: usize = 64 << 20; // 64 MiB

unsafe impl GlobalAlloc for Bump {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let align = layout.align().max(1);
        let size = layout.size();
        let mut p = (BUMP_CUR + align - 1) & !(align - 1);
        if p + size > BUMP_END {
            let chunk = BUMP_CHUNK.max(size + align);
            let base = System.alloc(Layout::from_size_align_unchecked(chunk, 4096));
            if base.is_null() {
                return std::ptr::null_mut();
            }
            BUMP_CUR = base as usize;
            BUMP_END = BUMP_CUR + chunk;
            p = (BUMP_CUR + align - 1) & !(align - 1);
        }
        BUMP_CUR = p + size;
        p as *mut u8
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
        // Intentionally leak — see module docs.
    }
}

#[global_allocator]
static ALLOCATOR: Bump = Bump;

// VALUES

pub enum Value {
    Float(f64),
    Char(u32),
    Bool(bool),
    Unit,
    Str(Vec<u8>),
    /// A list: a slice of length `len` into a contiguous, refcounted
    /// backing array. Elements are stored REVERSED — the Elm head is at
    /// `backing.data[len - 1]` — so `::` (prepend) is a push at the back
    /// and `tail` is just `len - 1` sharing the same backing.
    List {
        backing: *mut Backing,
        len: usize,
    },
    Ctor {
        name: *const u8,
        index: u32,
        argc: u32,
        // The first argument is stored inline so 0- and 1-argument
        // constructors (Maybe/Result/most custom types) allocate once
        // rather than also heap-allocating an argument array. `rest` holds
        // arguments 1.. and stays unallocated (empty Vec) for argc <= 1.
        arg0: u64,
        rest: Vec<u64>,
    },
    Closure {
        func: *const (),
        arity: u32,
        applied: u32,
        args: Vec<u64>,
    },
    Record {
        fields: Vec<(*const u8, u64)>,
    },
    Tuple(Vec<u64>),
    /// A dictionary: `(key, value)` pairs sorted ascending by key, no
    /// duplicate keys. Immutable — operations copy (bump-allocated).
    Dict(Vec<(u64, u64)>),
    /// A set: elements sorted ascending, unique.
    Set(Vec<u64>),
    /// A persistent array: elements in order (index 0 first).
    Array(Vec<u64>),
    /// An `elm/bytes` `Bytes` value: an immutable byte buffer. Mirrors the JS
    /// runtime's `DataView`; `encode` fills one, `read_*` read from it.
    Bytes(Vec<u8>),
    /// A `Json.Encode.Value` / `Json.Decode.Value` — an opaque JSON tree. In
    /// the JS runtime these are raw JS values; natively they are this tree.
    Json(JsonValue),
    /// A `Json.Decode.Decoder a` — reified as data and run by `run_decoder`,
    /// avoiding a closure per combinator. Sub-decoders and functions are held
    /// as uniform value words.
    Decoder(Decoder),
}

/// A JSON value tree. Object fields keep insertion order (as `JSON.stringify`
/// does), matching the JS runtime's object-key iteration.
#[derive(Clone)]
pub enum JsonValue {
    Null,
    Bool(bool),
    Number(f64),
    JStr(Vec<u8>),
    JArray(Vec<JsonValue>),
    JObject(Vec<(Vec<u8>, JsonValue)>),
}

/// A reified decoder. Word fields point to other `Value::Decoder`s or to
/// uniform closures/values, interpreted by [`run_decoder`].
pub enum Decoder {
    Str,
    Int,
    Float,
    Bool,
    /// `Json.Decode.value` — yield the raw JSON unchanged.
    JsonVal,
    /// `null fallback` — succeed with `fallback` on JSON null.
    Null(u64),
    Succeed(u64),
    Fail(Vec<u8>),
    Field(Vec<u8>, u64),
    Index(usize, u64),
    List(u64),
    Array(u64),
    KeyValuePairs(u64),
    Dict(u64),
    Maybe(u64),
    OneOf(Vec<u64>),
    OneOrMore(u64, u64),
    /// `map f d`.
    Map(u64, u64),
    /// `map2..map8 f d1 d2..` — run each decoder on the same value, then apply
    /// the curried `f` to the results in order.
    MapMany(u64, Vec<u64>),
    AndThen(u64, u64),
    /// `lazy thunk` — force `thunk ()` to a decoder, then run it.
    Lazy(u64),
}

/// A list's backing store, shared by a list and its tails. `rc` counts how
/// many list values reference it (used to decide in-place mutation);
/// `data` holds elements in reversed order (head last).
pub struct Backing {
    rc: usize,
    data: Vec<u64>,
}

fn alloc_backing(data: Vec<u64>) -> *mut Backing {
    Box::into_raw(Box::new(Backing { rc: 1, data }))
}

/// Allocate a value on the heap and return it as a value word. Never freed
/// (see module docs).
fn alloc(value: Value) -> u64 {
    Box::into_raw(Box::new(value)) as u64
}

/// Raw 8-byte-aligned allocation for the typed backend's unboxed heap
/// objects (tagged constructors, boxed recursive fields). Bump-allocated,
/// never freed — like everything else here.
#[no_mangle]
pub unsafe extern "C" fn alm_alloc(size: u64) -> *mut u8 {
    // Fixed-width u64 (not usize) so the ABI matches the typed backend's i64
    // declaration on every target — usize is 32-bit on wasm32, which would
    // otherwise make the linked call invalid.
    let size = size as usize;
    let layout = Layout::from_size_align(size.max(1), 8).unwrap();
    std::alloc::alloc(layout)
}

// Typed list backing for the monomorphized backend: a header
// `[cap: i64][used: i64]` followed by `cap` unboxed elements of `esize`
// bytes each. Elements are stored REVERSED (the list head is last), so a
// list value `{backing, len}` has its head at element `len - 1`; `tail` is
// the same backing with `len - 1` (O(1)), and `cons` appends at the back
// (O(1) amortized). Appends never disturb other views' visible range, so
// sharing is sound without refcounting — the uniform backend's scheme with
// unboxed elements.

/// Allocate a list backing with `count` elements (cap = used = count); the
/// caller fills the data. `esize` is the element size in bytes.
#[no_mangle]
pub unsafe extern "C" fn alm_list_alloc(count: i64, esize: i64) -> *mut u8 {
    let count = count.max(0) as usize;
    let esize = esize.max(1) as usize;
    let p = alm_alloc((16 + count * esize).max(16) as u64);
    let hdr = p as *mut i64;
    *hdr = count as i64;
    *hdr.add(1) = count as i64;
    p
}

/// Prepend `elem` (esize bytes) to the list `{backing, len}`, returning the
/// backing to use (the same one grown in place, or a fresh copy). The new
/// length is always `len + 1`.
#[no_mangle]
pub unsafe extern "C" fn alm_list_cons(
    backing: *mut u8,
    len: i64,
    elem: *const u8,
    esize: i64,
) -> *mut u8 {
    let esize = esize.max(1) as usize;
    let len = len.max(0) as usize;
    if !backing.is_null() {
        let hdr = backing as *mut i64;
        let cap = *hdr as usize;
        let used = *hdr.add(1) as usize;
        if len == used && used < cap {
            let data = backing.add(16);
            std::ptr::copy_nonoverlapping(elem, data.add(used * esize), esize);
            *hdr.add(1) = (used + 1) as i64;
            return backing;
        }
    }
    let old_cap = if backing.is_null() {
        0
    } else {
        *(backing as *const i64) as usize
    };
    let new_cap = ((len + 1) * 2).max(4).max(old_cap * 2);
    let np = alm_alloc((16 + new_cap * esize) as u64);
    let nhdr = np as *mut i64;
    *nhdr = new_cap as i64;
    *nhdr.add(1) = (len + 1) as i64;
    let ndata = np.add(16);
    if !backing.is_null() && len > 0 {
        std::ptr::copy_nonoverlapping(backing.add(16), ndata, len * esize);
    }
    std::ptr::copy_nonoverlapping(elem, ndata.add(len * esize), esize);
    np
}

/// Unbox a uniform int/float value word into a raw machine value — the
/// typed backend's boundary conversion when a uniform runtime kernel
/// returns a value it wants unboxed. The inverses of `rt_int`/`rt_float`.
#[no_mangle]
pub unsafe extern "C" fn rt_unint(w: u64) -> i64 {
    int_val(w)
}

/// Unbox a uniform Char value word to its raw codepoint.
#[no_mangle]
pub unsafe extern "C" fn rt_unchr(w: u64) -> i32 {
    match deref(w) {
        Value::Char(c) => *c as i32,
        _ => 0,
    }
}

/// The constructor index (tag) of a uniform Ctor value.
#[no_mangle]
pub unsafe extern "C" fn rt_ctor_tag(w: u64) -> i32 {
    match deref(w) {
        Value::Ctor { index, .. } => *index as i32,
        _ => 0,
    }
}

#[no_mangle]
pub unsafe extern "C" fn rt_unfloat(w: u64) -> f64 {
    match deref(w) {
        Value::Float(f) => *f,
        _ => 0.0,
    }
}

/// View a pointer value word as a `&Value`. Only valid for non-integer
/// words (`is_int` false); the cast truncates to the target pointer width.
#[inline]
unsafe fn deref<'a>(w: u64) -> &'a Value {
    &*(w as *mut Value)
}
#[inline]
unsafe fn deref_mut<'a>(w: u64) -> &'a mut Value {
    &mut *(w as *mut Value)
}

// VALUE WORD
//
// A value is a 64-bit word (`u64`), independent of the target's pointer
// width. Integers are unboxed tagged immediates (OCaml/V8-SMI style): heap
// allocations are 8-aligned so a pointer value's low bit is 0, and a low bit
// of 1 marks a 63-bit immediate integer. Pointer values hold the address in
// the low bits (32 on wasm32, 64 on the host); `deref` truncates back to a
// real pointer. This gives full-width integers on every target. The LLVM
// codegen moves values opaquely as i64 and never inspects them.

#[inline]
fn is_int(w: u64) -> bool {
    w & 1 == 1
}
#[inline]
fn mk_int(n: i64) -> u64 {
    ((n << 1) | 1) as u64
}
#[inline]
fn int_val(w: u64) -> i64 {
    (w as i64) >> 1
}

/// An exported global holding a `u64`, set once during startup and
/// read by generated code as a plain `ptr`. `repr(transparent)` so its
/// symbol is exactly the pointer.
#[repr(transparent)]
struct Global(UnsafeCell<u64>);
unsafe impl Sync for Global {}
impl Global {
    const NULL: Global = Global(UnsafeCell::new(0u64));
    #[inline]
    unsafe fn set(&self, value: u64) {
        *self.0.get() = value;
    }
    #[inline]
    unsafe fn get(&self) -> u64 {
        *self.0.get()
    }
}

// The three singletons generated code loads directly.
#[export_name = "rt_true_v"]
static RT_TRUE: Global = Global::NULL;
#[export_name = "rt_false_v"]
static RT_FALSE: Global = Global::NULL;
#[export_name = "rt_unit_v"]
static RT_UNIT: Global = Global::NULL;

// Internal singletons.
static NIL: Global = Global::NULL;
static NOTHING: Global = Global::NULL;
static LT: Global = Global::NULL;
static EQ: Global = Global::NULL;
static GT: Global = Global::NULL;

unsafe fn tru() -> u64 {
    RT_TRUE.get()
}
unsafe fn fls() -> u64 {
    RT_FALSE.get()
}
unsafe fn unit() -> u64 {
    RT_UNIT.get()
}
unsafe fn nil() -> u64 {
    NIL.get()
}
unsafe fn nothing() -> u64 {
    NOTHING.get()
}
unsafe fn rt_bool(b: bool) -> u64 {
    if b {
        tru()
    } else {
        fls()
    }
}

// CRASH

#[no_mangle]
pub unsafe extern "C" fn rt_crash(message: *const u8) -> ! {
    let text = CStr::from_ptr(message as *const i8).to_bytes();
    let stderr = std::io::stderr();
    let mut handle = stderr.lock();
    let _ = handle.write_all(b"alm: ");
    let _ = handle.write_all(text);
    let _ = handle.write_all(b"\n");
    let _ = handle.flush();
    std::process::exit(1);
}

macro_rules! crash {
    ($msg:literal) => {
        rt_crash(concat!($msg, "\0").as_ptr())
    };
}

// ACCESSORS / HELPERS

#[inline]
unsafe fn is_num(p: u64) -> bool {
    // `is_int` short-circuits so an immediate is never dereferenced.
    is_int(p) || matches!(deref(p), Value::Float(_))
}

#[inline]
unsafe fn is_float(p: u64) -> bool {
    !is_int(p) && matches!(deref(p), Value::Float(_))
}

#[inline]
unsafe fn num(p: u64) -> f64 {
    if is_int(p) {
        return int_val(p) as f64;
    }
    match deref(p) {
        Value::Float(f) => *f,
        Value::Char(c) => *c as f64,
        Value::Bool(b) => *b as u8 as f64,
        _ => crash!("expected a number"),
    }
}

#[inline]
unsafe fn as_int(p: u64) -> i64 {
    if is_int(p) {
        return int_val(p);
    }
    match deref(p) {
        Value::Char(c) => *c as i64,
        Value::Bool(b) => *b as i64,
        Value::Float(f) => *f as i64,
        _ => crash!("expected an int"),
    }
}

unsafe fn sbytes<'a>(p: u64) -> &'a [u8] {
    match deref(p) {
        Value::Str(b) => b.as_slice(),
        _ => crash!("expected a string"),
    }
}

unsafe fn sstr<'a>(p: u64) -> &'a str {
    // Elm strings are UTF-8 and every runtime operation preserves that.
    std::str::from_utf8_unchecked(sbytes(p))
}

fn mkstr(bytes: Vec<u8>) -> u64 {
    alloc(Value::Str(bytes))
}

/// The list's `(backing, len)`. Panics/crashes if `v` is not a list.
#[inline]
unsafe fn list_view(v: u64) -> (*mut Backing, usize) {
    match deref(v) {
        Value::List { backing, len } => (*backing, *len),
        _ => crash!("expected a list"),
    }
}

#[inline]
unsafe fn list_len(v: u64) -> usize {
    match deref(v) {
        Value::List { len, .. } => *len,
        _ => 0,
    }
}

/// The active elements in reversed storage order (head is the last).
#[inline]
unsafe fn list_store<'a>(v: u64) -> &'a [u64] {
    let (backing, len) = list_view(v);
    &(*backing).data[..len]
}

/// Build a list value from elements already in reversed storage order.
#[inline]
unsafe fn list_from_store(data: Vec<u64>) -> u64 {
    let len = data.len();
    alloc(Value::List {
        backing: alloc_backing(data),
        len,
    })
}

#[inline]
unsafe fn cons(head: u64, tail: u64) -> u64 {
    let (backing, len) = list_view(tail);
    // Grow in place when this list sits at the tip of its backing (its view
    // covers all stored elements). Appending only ever EXTENDS the buffer
    // (writes at data.len(), never overwrites), so no other view — which
    // reads a prefix data[..its_len] — is disturbed; and if two lists share
    // a backing, only the first extends (the second then sees len !=
    // data.len() and copies). This is sound with no refcount, and Vec's
    // exponential growth makes repeated `::` O(1) amortized. `len > 0`
    // avoids mutating the shared empty-list singleton.
    if len > 0 && (*backing).data.len() == len {
        (*backing).data.push(head);
        return alloc(Value::List {
            backing,
            len: len + 1,
        });
    }
    let mut data = Vec::with_capacity(len + 1);
    data.extend_from_slice(&(*backing).data[..len]);
    data.push(head); // head lives at the back of the reversed store
    list_from_store(data)
}

/// Elm-order elements (head first).
unsafe fn to_vec(xs: u64) -> Vec<u64> {
    list_store(xs).iter().rev().copied().collect()
}

/// Build a list from elements in Elm order (head first).
unsafe fn list_from_slice(items: &[u64]) -> u64 {
    list_from_store(items.iter().rev().copied().collect())
}

unsafe fn collect(args: *const u64, n: i32) -> Vec<u64> {
    if n <= 0 || args.is_null() {
        return Vec::new();
    }
    (0..n as usize).map(|i| *args.add(i)).collect()
}

unsafe fn cname<'a>(name: *const u8) -> &'a str {
    CStr::from_ptr(name as *const i8).to_str().unwrap_or("?")
}

unsafe fn ceq(a: *const u8, b: *const u8) -> bool {
    a == b || CStr::from_ptr(a as *const i8) == CStr::from_ptr(b as *const i8)
}

// CONSTRUCTION (called from generated code)

#[no_mangle]
#[inline]
pub unsafe extern "C" fn rt_int(n: i64) -> u64 {
    mk_int(n)
}

#[no_mangle]
#[inline]
pub unsafe extern "C" fn rt_float(f: f64) -> u64 {
    alloc(Value::Float(f))
}

#[no_mangle]
#[inline]
pub unsafe extern "C" fn rt_chr(c: i32) -> u64 {
    alloc(Value::Char(c as u32))
}

#[no_mangle]
pub unsafe extern "C" fn rt_str(ptr: *const u8, len: i64) -> u64 {
    let bytes = if len <= 0 {
        Vec::new()
    } else {
        std::slice::from_raw_parts(ptr, len as usize).to_vec()
    };
    mkstr(bytes)
}

unsafe fn ctor(name: *const u8, index: u32, args: Vec<u64>) -> u64 {
    let argc = args.len() as u32;
    let mut it = args.into_iter();
    let arg0 = it.next().unwrap_or(0u64);
    let rest: Vec<u64> = it.collect();
    alloc(Value::Ctor {
        name,
        index,
        argc,
        arg0,
        rest,
    })
}

/// Allocate a 0- or 1-argument constructor without building an argument
/// Vec (the common case: Nothing, Just, Ok, Err, …).
#[inline]
unsafe fn ctor0(name: *const u8, index: u32) -> u64 {
    alloc(Value::Ctor {
        name,
        index,
        argc: 0,
        arg0: 0u64,
        rest: Vec::new(),
    })
}
#[inline]
unsafe fn ctor1(name: *const u8, index: u32, arg0: u64) -> u64 {
    alloc(Value::Ctor {
        name,
        index,
        argc: 1,
        arg0,
        rest: Vec::new(),
    })
}

/// The i-th argument of a constructor value (arg0 inline, rest spilled).
#[inline]
unsafe fn ctor_get(v: u64, i: usize) -> u64 {
    match deref(v) {
        Value::Ctor { arg0, rest, .. } => {
            if i == 0 {
                *arg0
            } else {
                rest[i - 1]
            }
        }
        _ => crash!("not a constructor"),
    }
}

#[no_mangle]
#[inline]
pub unsafe extern "C" fn rt_ctor(
    name: *const u8,
    index: i32,
    argc: i32,
    args: *const u64,
) -> u64 {
    // Read arguments directly into the inline slot + spill; no intermediate
    // Vec, so 0/1-argument constructors allocate only the Value itself.
    let argc = argc.max(0) as usize;
    let arg0 = if argc >= 1 {
        *args
    } else {
        0u64
    };
    let rest: Vec<u64> = if argc >= 2 {
        (1..argc).map(|i| *args.add(i)).collect()
    } else {
        Vec::new()
    };
    alloc(Value::Ctor {
        name,
        index: index as u32,
        argc: argc as u32,
        arg0,
        rest,
    })
}

#[no_mangle]
pub unsafe extern "C" fn rt_list(n: i32, items: *const u64) -> u64 {
    list_from_slice(&collect(items, n))
}

#[no_mangle]
#[inline]
pub unsafe extern "C" fn rt_cons(head: u64, tail: u64) -> u64 {
    cons(head, tail)
}

#[no_mangle]
pub unsafe extern "C" fn rt_tuple(n: i32, items: *const u64) -> u64 {
    alloc(Value::Tuple(collect(items, n)))
}

#[no_mangle]
pub unsafe extern "C" fn rt_closure(
    func: *const (),
    arity: i32,
    ncaps: i32,
    caps: *const u64,
) -> u64 {
    let arity = arity as usize;
    let ncaps = ncaps as usize;
    let mut args = vec![0u64; arity];
    if ncaps > 0 && !caps.is_null() {
        for (i, slot) in args.iter_mut().enumerate().take(ncaps) {
            *slot = *caps.add(i);
        }
    }
    alloc(Value::Closure {
        func,
        arity: arity as u32,
        applied: ncaps as u32,
        args,
    })
}

// --- DEBUG: closure arity checker (only wired when the typed backend is
// built with ALM_ARITY_CHECK set). Registers each closure wrapper's true LLVM
// parameter count at creation and verifies it at every typed application, to
// catch a closure of one arity being invoked through a slot of another. ---
static mut ARITY_MAP: Option<std::collections::HashMap<usize, u32>> = None;

#[no_mangle]
pub unsafe extern "C" fn alm_dbg_reg(fnptr: u64, arity: u32) {
    let m = ARITY_MAP.get_or_insert_with(std::collections::HashMap::new);
    m.insert(fnptr as usize, arity);
}

#[no_mangle]
pub unsafe extern "C" fn alm_dbg_check(fnptr: u64, argc: u32) {
    if let Some(m) = &*std::ptr::addr_of!(ARITY_MAP) {
        if let Some(&ar) = m.get(&(fnptr as usize)) {
            if ar != argc {
                eprintln!(
                    "\n=== ARITY MISMATCH: fnptr={:#x} registered={} called_with={} ===",
                    fnptr, ar, argc
                );
                eprintln!("{}", std::backtrace::Backtrace::force_capture());
                std::process::abort();
            }
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn rt_closure_set(closure: u64, i: i32, value: u64) {
    if let Value::Closure { args, .. } = deref_mut(closure) {
        args[i as usize] = value;
    }
}

#[no_mangle]
pub unsafe extern "C" fn rt_record_new(n: i32) -> u64 {
    alloc(Value::Record {
        fields: vec![(std::ptr::null(), 0u64); n as usize],
    })
}

#[no_mangle]
pub unsafe extern "C" fn rt_record_set(
    record: u64,
    i: i32,
    name: *const u8,
    value: u64,
) {
    if let Value::Record { fields } = deref_mut(record) {
        fields[i as usize] = (name, value);
    }
}

#[no_mangle]
pub unsafe extern "C" fn rt_record_clone(record: u64) -> u64 {
    match deref(record) {
        Value::Record { fields } => alloc(Value::Record {
            fields: fields.clone(),
        }),
        _ => crash!("record clone: not a record"),
    }
}

#[no_mangle]
pub unsafe extern "C" fn rt_record_replace(record: u64, name: *const u8, value: u64) {
    if let Value::Record { fields } = deref_mut(record) {
        for field in fields.iter_mut() {
            if ceq(field.0, name) {
                field.1 = value;
                return;
            }
        }
    }
    crash!("record update: unknown field");
}

// ACCESS

#[no_mangle]
#[inline]
pub unsafe extern "C" fn rt_access(record: u64, name: *const u8) -> u64 {
    if let Value::Record { fields } = deref(record) {
        for &(field_name, value) in fields {
            if ceq(field_name, name) {
                return value;
            }
        }
    }
    crash!("record access: unknown field");
}

#[no_mangle]
#[inline]
pub unsafe extern "C" fn rt_ctor_arg(v: u64, i: i32) -> u64 {
    ctor_get(v, i as usize)
}

#[no_mangle]
#[inline]
pub unsafe extern "C" fn rt_tuple_item(v: u64, i: i32) -> u64 {
    match deref(v) {
        Value::Tuple(items) => items[i as usize],
        _ => crash!("not a tuple"),
    }
}

#[no_mangle]
#[inline]
pub unsafe extern "C" fn rt_list_head(v: u64) -> u64 {
    let store = list_store(v);
    match store.last() {
        Some(&h) => h, // head is the last element of the reversed store
        None => crash!("head of an empty list"),
    }
}

#[no_mangle]
pub unsafe extern "C" fn rt_list_tail(v: u64) -> u64 {
    let (backing, len) = list_view(v);
    if len == 0 {
        crash!("tail of an empty list");
    }
    // Drop the head (the last stored element) by shrinking the view; the
    // backing is shared.
    (*backing).rc += 1;
    alloc(Value::List {
        backing,
        len: len - 1,
    })
}

// TESTS

#[no_mangle]
#[inline]
pub unsafe extern "C" fn rt_is_true(v: u64) -> bool {
    !is_int(v) && matches!(deref(v), Value::Bool(true))
}

#[no_mangle]
#[inline]
pub unsafe extern "C" fn rt_is_ctor(v: u64, index: i32) -> bool {
    !is_int(v) && matches!(deref(v), Value::Ctor { index: i, .. } if *i == index as u32)
}

#[no_mangle]
#[inline]
pub unsafe extern "C" fn rt_is_bool(v: u64, b: bool) -> bool {
    !is_int(v) && matches!(deref(v), Value::Bool(x) if *x == b)
}

#[no_mangle]
#[inline]
pub unsafe extern "C" fn rt_is_int(v: u64, n: i64) -> bool {
    is_int(v) && int_val(v) == n
}

#[no_mangle]
#[inline]
pub unsafe extern "C" fn rt_is_chr(v: u64, c: i32) -> bool {
    !is_int(v) && matches!(deref(v), Value::Char(x) if *x == c as u32)
}

#[no_mangle]
pub unsafe extern "C" fn rt_is_str(v: u64, ptr: *const u8, len: i64) -> bool {
    if is_int(v) {
        return false;
    }
    match deref(v) {
        Value::Str(b) => b.len() == len as usize && b.as_slice() == std::slice::from_raw_parts(ptr, len as usize),
        _ => false,
    }
}

#[no_mangle]
#[inline]
pub unsafe extern "C" fn rt_is_cons(v: u64) -> bool {
    !is_int(v) && matches!(deref(v), Value::List { len, .. } if *len > 0)
}

#[no_mangle]
#[inline]
pub unsafe extern "C" fn rt_is_nil(v: u64) -> bool {
    !is_int(v) && matches!(deref(v), Value::List { len: 0, .. })
}

// CURRYING — the Rust twin of the JS F/A helpers.

type Fn1 = unsafe extern "C" fn(u64) -> u64;
type Fn2 = unsafe extern "C" fn(u64, u64) -> u64;
type Fn3 = unsafe extern "C" fn(u64, u64, u64) -> u64;
type Fn4 = unsafe extern "C" fn(u64, u64, u64, u64) -> u64;
type Fn5 = unsafe extern "C" fn(u64, u64, u64, u64, u64) -> u64;
type Fn6 = unsafe extern "C" fn(
    u64, u64, u64, u64, u64, u64,
) -> u64;
type Fn7 = unsafe extern "C" fn(
    u64, u64, u64, u64, u64, u64, u64,
) -> u64;
type Fn8 = unsafe extern "C" fn(
    u64, u64, u64, u64, u64, u64, u64, u64,
) -> u64;
type Fn9 = unsafe extern "C" fn(
    u64, u64, u64, u64, u64, u64, u64, u64,
    u64,
) -> u64;
type Fn10 = unsafe extern "C" fn(
    u64, u64, u64, u64, u64, u64, u64, u64,
    u64, u64,
) -> u64;
type Fn11 = unsafe extern "C" fn(
    u64, u64, u64, u64, u64, u64, u64, u64,
    u64, u64, u64,
) -> u64;
type Fn12 = unsafe extern "C" fn(
    u64, u64, u64, u64, u64, u64, u64, u64,
    u64, u64, u64, u64,
) -> u64;
type Fn13 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
type Fn14 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
type Fn15 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
type Fn16 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
type Fn17 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
type Fn18 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
type Fn19 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
type Fn20 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
type Fn21 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
type Fn22 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
type Fn23 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
type Fn24 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
type Fn25 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
type Fn26 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
type Fn27 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
type Fn28 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
type Fn29 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
type Fn30 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
type Fn31 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
type Fn32 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;

#[inline]
unsafe fn call_fn(func: *const (), arity: usize, a: &[u64]) -> u64 {
    use std::mem::transmute;
    match arity {
        1 => (transmute::<_, Fn1>(func))(a[0]),
        2 => (transmute::<_, Fn2>(func))(a[0], a[1]),
        3 => (transmute::<_, Fn3>(func))(a[0], a[1], a[2]),
        4 => (transmute::<_, Fn4>(func))(a[0], a[1], a[2], a[3]),
        5 => (transmute::<_, Fn5>(func))(a[0], a[1], a[2], a[3], a[4]),
        6 => (transmute::<_, Fn6>(func))(a[0], a[1], a[2], a[3], a[4], a[5]),
        7 => (transmute::<_, Fn7>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6]),
        8 => (transmute::<_, Fn8>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7]),
        9 => (transmute::<_, Fn9>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8]),
        10 => (transmute::<_, Fn10>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9]),
        11 => (transmute::<_, Fn11>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10]),
        12 => (transmute::<_, Fn12>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11]),
        13 => (transmute::<_, Fn13>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11], a[12]),
        14 => (transmute::<_, Fn14>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11], a[12], a[13]),
        15 => (transmute::<_, Fn15>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11], a[12], a[13], a[14]),
        16 => (transmute::<_, Fn16>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11], a[12], a[13], a[14], a[15]),
        17 => (transmute::<_, Fn17>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11], a[12], a[13], a[14], a[15], a[16]),
        18 => (transmute::<_, Fn18>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11], a[12], a[13], a[14], a[15], a[16], a[17]),
        19 => (transmute::<_, Fn19>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11], a[12], a[13], a[14], a[15], a[16], a[17], a[18]),
        20 => (transmute::<_, Fn20>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11], a[12], a[13], a[14], a[15], a[16], a[17], a[18], a[19]),
        21 => (transmute::<_, Fn21>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11], a[12], a[13], a[14], a[15], a[16], a[17], a[18], a[19], a[20]),
        22 => (transmute::<_, Fn22>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11], a[12], a[13], a[14], a[15], a[16], a[17], a[18], a[19], a[20], a[21]),
        23 => (transmute::<_, Fn23>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11], a[12], a[13], a[14], a[15], a[16], a[17], a[18], a[19], a[20], a[21], a[22]),
        24 => (transmute::<_, Fn24>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11], a[12], a[13], a[14], a[15], a[16], a[17], a[18], a[19], a[20], a[21], a[22], a[23]),
        25 => (transmute::<_, Fn25>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11], a[12], a[13], a[14], a[15], a[16], a[17], a[18], a[19], a[20], a[21], a[22], a[23], a[24]),
        26 => (transmute::<_, Fn26>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11], a[12], a[13], a[14], a[15], a[16], a[17], a[18], a[19], a[20], a[21], a[22], a[23], a[24], a[25]),
        27 => (transmute::<_, Fn27>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11], a[12], a[13], a[14], a[15], a[16], a[17], a[18], a[19], a[20], a[21], a[22], a[23], a[24], a[25], a[26]),
        28 => (transmute::<_, Fn28>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11], a[12], a[13], a[14], a[15], a[16], a[17], a[18], a[19], a[20], a[21], a[22], a[23], a[24], a[25], a[26], a[27]),
        29 => (transmute::<_, Fn29>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11], a[12], a[13], a[14], a[15], a[16], a[17], a[18], a[19], a[20], a[21], a[22], a[23], a[24], a[25], a[26], a[27], a[28]),
        30 => (transmute::<_, Fn30>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11], a[12], a[13], a[14], a[15], a[16], a[17], a[18], a[19], a[20], a[21], a[22], a[23], a[24], a[25], a[26], a[27], a[28], a[29]),
        31 => (transmute::<_, Fn31>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11], a[12], a[13], a[14], a[15], a[16], a[17], a[18], a[19], a[20], a[21], a[22], a[23], a[24], a[25], a[26], a[27], a[28], a[29], a[30]),
        32 => (transmute::<_, Fn32>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11], a[12], a[13], a[14], a[15], a[16], a[17], a[18], a[19], a[20], a[21], a[22], a[23], a[24], a[25], a[26], a[27], a[28], a[29], a[30], a[31]),
        _ => crash!("function arity too large (max 32)"),
    }
}

#[no_mangle]
pub unsafe extern "C" fn rt_apply(mut f: u64, n: i32, mut args: *const u64) -> u64 {
    let mut n = n as usize;
    while n > 0 {
        let (func, arity, applied) = match deref(f) {
            Value::Closure { func, arity, applied, .. } => {
                (*func, *arity as usize, *applied as usize)
            }
            _ => crash!("applied a non-function"),
        };
        let missing = arity - applied;
        let take = n.min(missing);
        // Build the argument list on the stack — closure application is
        // extremely hot, so this must not allocate. Arity is bounded by
        // call_fn's max (12).
        let mut all: [u64; 32] = [0u64; 32];
        if let Value::Closure { args: caps, .. } = deref(f) {
            all[..applied].copy_from_slice(&caps[..applied]);
        }
        for i in 0..take {
            all[applied + i] = *args.add(i);
        }
        if take < missing {
            return rt_closure(func, arity as i32, (applied + take) as i32, all.as_ptr());
        }
        f = call_fn(func, arity, &all[..arity]);
        args = args.add(take);
        n -= take;
    }
    f
}

/// Apply a one-argument call. The saturating case (this call completes the
/// closure) is handled inline so it folds into the hot kernel loops
/// (`List.map`/`foldl`/…) instead of calling the general `rt_apply`.
#[inline(always)]
unsafe fn ap1(f: u64, a: u64) -> u64 {
    let mut all: [u64; 32] = [0u64; 32];
    let (func, arity) = match deref(f) {
        Value::Closure { func, arity, applied, args } => {
            let (func, arity, applied) = (*func, *arity as usize, *applied as usize);
            if applied + 1 != arity {
                return rt_apply(f, 1, [a].as_ptr());
            }
            all[..applied].copy_from_slice(&args[..applied]);
            all[applied] = a;
            (func, arity)
        }
        _ => return rt_apply(f, 1, [a].as_ptr()),
    };
    call_fn(func, arity, &all[..arity])
}

#[inline(always)]
unsafe fn ap2(f: u64, a: u64, b: u64) -> u64 {
    let mut all: [u64; 32] = [0u64; 32];
    let (func, arity) = match deref(f) {
        Value::Closure { func, arity, applied, args } => {
            let (func, arity, applied) = (*func, *arity as usize, *applied as usize);
            if applied + 2 != arity {
                return rt_apply(f, 2, [a, b].as_ptr());
            }
            all[..applied].copy_from_slice(&args[..applied]);
            all[applied] = a;
            all[applied + 1] = b;
            (func, arity)
        }
        _ => return rt_apply(f, 2, [a, b].as_ptr()),
    };
    call_fn(func, arity, &all[..arity])
}

// NUMBERS
//
// Elm number literals are polymorphic and the IR is untyped, so an
// Int-boxed literal can flow into a Float operation. Float ops coerce with
// `num`, and int/float dispatch treats "either side is a float" as float
// (matching JS, where every number is a double).

// Arithmetic and comparisons: a tiny inlinable fast path for the common
// case of two integers, with the float handling pushed out-of-line so LLVM
// inlines the hot path into generated code. The fast path goes through
// `int_val`/`mk_int` so it is correct for both the unboxed (host) and boxed
// (wasm) integer representations.

#[no_mangle]
#[inline]
pub unsafe extern "C" fn rt_add(a: u64, b: u64) -> u64 {
    if is_int(a) && is_int(b) {
        return mk_int(int_val(a).wrapping_add(int_val(b)));
    }
    rt_add_slow(a, b)
}
#[inline(never)]
unsafe fn rt_add_slow(a: u64, b: u64) -> u64 {
    rt_float(num(a) + num(b))
}

#[no_mangle]
#[inline]
pub unsafe extern "C" fn rt_sub(a: u64, b: u64) -> u64 {
    if is_int(a) && is_int(b) {
        return mk_int(int_val(a).wrapping_sub(int_val(b)));
    }
    rt_sub_slow(a, b)
}
#[inline(never)]
unsafe fn rt_sub_slow(a: u64, b: u64) -> u64 {
    rt_float(num(a) - num(b))
}

#[no_mangle]
#[inline]
pub unsafe extern "C" fn rt_mul(a: u64, b: u64) -> u64 {
    if is_int(a) && is_int(b) {
        return mk_int(int_val(a).wrapping_mul(int_val(b)));
    }
    rt_mul_slow(a, b)
}
#[inline(never)]
unsafe fn rt_mul_slow(a: u64, b: u64) -> u64 {
    rt_float(num(a) * num(b))
}

#[no_mangle]
#[inline]
pub unsafe extern "C" fn rt_fdiv(a: u64, b: u64) -> u64 {
    rt_float(num(a) / num(b))
}

#[no_mangle]
#[inline]
pub unsafe extern "C" fn rt_idiv(a: u64, b: u64) -> u64 {
    // Match Elm's JS semantics: x // 0 == 0.
    let d = as_int(b);
    if d == 0 {
        rt_int(0)
    } else {
        rt_int(as_int(a) / d)
    }
}

#[no_mangle]
#[inline]
pub unsafe extern "C" fn rt_pow(a: u64, b: u64) -> u64 {
    if is_float(a) || is_float(b) {
        rt_float(num(a).powf(num(b)))
    } else {
        rt_int((as_int(a) as f64).powf(as_int(b) as f64) as i64)
    }
}

#[no_mangle]
#[inline]
pub unsafe extern "C" fn rt_neg(a: u64) -> u64 {
    if is_float(a) {
        rt_float(-num(a))
    } else {
        rt_int(-as_int(a))
    }
}

// EQUALITY AND ORDERING

#[inline]
/// Structural equality of two JSON trees, matching how JS `==`
/// (`_Utils_eq`) compares the raw values a `Json.*.Value` wraps: arrays
/// element-wise, objects by key/value with keys matched by name (order
/// independent).
fn json_eq(a: &JsonValue, b: &JsonValue) -> bool {
    use JsonValue::*;
    match (a, b) {
        (Null, Null) => true,
        (Bool(x), Bool(y)) => x == y,
        (Number(x), Number(y)) => x == y,
        (JStr(x), JStr(y)) => x == y,
        (JArray(x), JArray(y)) => x.len() == y.len() && x.iter().zip(y).all(|(p, q)| json_eq(p, q)),
        (JObject(x), JObject(y)) => {
            x.len() == y.len()
                && x.iter().all(|(k, v)| {
                    y.iter()
                        .find(|(k2, _)| k2 == k)
                        .map_or(false, |(_, v2)| json_eq(v, v2))
                })
        }
        _ => false,
    }
}

unsafe fn value_eq(a: u64, b: u64) -> bool {
    // Numbers first: this also handles a polymorphic-literal Int flowing
    // into a Float comparison (immediate int vs boxed float).
    if is_num(a) && is_num(b) {
        return num(a) == num(b);
    }
    if a == b {
        return true;
    }
    if is_int(a) || is_int(b) {
        // One side is an immediate int, the other a non-number pointer.
        return false;
    }
    match (deref(a), deref(b)) {
        (Value::Char(x), Value::Char(y)) => x == y,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Unit, Value::Unit) => true,
        (Value::Str(x), Value::Str(y)) => x == y,
        // `Json.Encode.Value`/`Json.Decode.Value` wrap raw JS values in Elm, so
        // `==` compares them structurally (deep-equal, with object keys matched
        // by name regardless of order — matching JS `_Utils_eq`).
        (Value::Json(x), Value::Json(y)) => json_eq(x, y),
        (Value::List { .. }, Value::List { .. }) => {
            let (x, y) = (list_store(a), list_store(b));
            x.len() == y.len() && x.iter().zip(y).all(|(&p, &q)| value_eq(p, q))
        }
        (Value::Ctor { index: i1, argc: n1, .. }, Value::Ctor { index: i2, argc: n2, .. }) => {
            i1 == i2 && n1 == n2 && (0..*n1 as usize).all(|i| value_eq(ctor_get(a, i), ctor_get(b, i)))
        }
        (Value::Tuple(x), Value::Tuple(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(&p, &q)| value_eq(p, q))
        }
        (Value::Record { fields }, Value::Record { .. }) => {
            fields.iter().all(|&(name, value)| value_eq(value, rt_access(b, name)))
        }
        (Value::Dict(x), Value::Dict(y)) => {
            x.len() == y.len()
                && x.iter()
                    .zip(y)
                    .all(|((k1, v1), (k2, v2))| value_eq(*k1, *k2) && value_eq(*v1, *v2))
        }
        (Value::Set(x), Value::Set(y)) | (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(&p, &q)| value_eq(p, q))
        }
        (Value::Bytes(x), Value::Bytes(y)) => x == y,
        // Functions: Elm's `==` short-circuits to `true` for the same function
        // reference (and errors otherwise). Native builds a fresh closure value
        // per reference, so pointer identity of the closure value fails even for
        // the same source function; compare by the underlying function pointer
        // plus the already-applied capture *words* by raw identity. The captures
        // of a boxed closure are raw pointers (e.g. a wrapped typed closure),
        // not uniform values, so they must NOT be recursed into with `value_eq`
        // — raw-word equality is both safe and the right notion of "same
        // reference". Canonical top-level-function closures (a shared global)
        // make this hold for e.g. a shared `Dict`/model comparator.
        (
            Value::Closure { func: f1, applied: n1, args: a1, .. },
            Value::Closure { func: f2, applied: n2, args: a2, .. },
        ) => f1 == f2 && n1 == n2 && a1[..*n1 as usize] == a2[..*n2 as usize],
        _ => false,
    }
}

#[inline]
unsafe fn value_cmp(a: u64, b: u64) -> i32 {
    if is_num(a) && is_num(b) {
        let (x, y) = (num(a), num(b));
        return if x < y {
            -1
        } else if x > y {
            1
        } else {
            0
        };
    }
    match (deref(a), deref(b)) {
        (Value::Char(x), Value::Char(y)) => (*x as i64 - *y as i64).signum() as i32,
        (Value::Str(x), Value::Str(y)) => match x.cmp(y) {
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Greater => 1,
        },
        (Value::List { .. }, Value::List { .. }) => {
            // Lexicographic in Elm order (head first = reversed store).
            let (x, y) = (list_store(a), list_store(b));
            let mut i = x.len();
            let mut j = y.len();
            while i > 0 && j > 0 {
                i -= 1;
                j -= 1;
                let c = value_cmp(x[i], y[j]);
                if c != 0 {
                    return c;
                }
            }
            (x.len() as i64 - y.len() as i64).signum() as i32
        }
        (Value::Tuple(x), Value::Tuple(y)) => {
            for (&p, &q) in x.iter().zip(y) {
                let c = value_cmp(p, q);
                if c != 0 {
                    return c;
                }
            }
            0
        }
        _ => crash!("cannot order these values"),
    }
}

// The two-integer case is a fast path through `int_val` (correct for both
// the unboxed and boxed representations), with structural eq/cmp out-of-line.

#[no_mangle]
#[inline]
pub unsafe extern "C" fn rt_eq(a: u64, b: u64) -> u64 {
    if is_int(a) && is_int(b) {
        return rt_bool(int_val(a) == int_val(b));
    }
    rt_bool(value_eq(a, b))
}
#[no_mangle]
#[inline]
pub unsafe extern "C" fn rt_neq(a: u64, b: u64) -> u64 {
    if is_int(a) && is_int(b) {
        return rt_bool(int_val(a) != int_val(b));
    }
    rt_bool(!value_eq(a, b))
}
#[no_mangle]
#[inline]
pub unsafe extern "C" fn rt_lt(a: u64, b: u64) -> u64 {
    if is_int(a) && is_int(b) {
        return rt_bool(int_val(a) < int_val(b));
    }
    rt_bool(value_cmp(a, b) < 0)
}
#[no_mangle]
#[inline]
pub unsafe extern "C" fn rt_le(a: u64, b: u64) -> u64 {
    if is_int(a) && is_int(b) {
        return rt_bool(int_val(a) <= int_val(b));
    }
    rt_bool(value_cmp(a, b) <= 0)
}
#[no_mangle]
#[inline]
pub unsafe extern "C" fn rt_gt(a: u64, b: u64) -> u64 {
    if is_int(a) && is_int(b) {
        return rt_bool(int_val(a) > int_val(b));
    }
    rt_bool(value_cmp(a, b) > 0)
}
#[no_mangle]
#[inline]
pub unsafe extern "C" fn rt_ge(a: u64, b: u64) -> u64 {
    if is_int(a) && is_int(b) {
        return rt_bool(int_val(a) >= int_val(b));
    }
    rt_bool(value_cmp(a, b) >= 0)
}

// APPEND — strings and lists.

#[no_mangle]
pub unsafe extern "C" fn rt_append(a: u64, b: u64) -> u64 {
    match deref(a) {
        Value::Str(x) => {
            let mut bytes = x.clone();
            bytes.extend_from_slice(sbytes(b));
            mkstr(bytes)
        }
        Value::List { .. } => {
            // Result in Elm order is a ++ b; in reversed storage that is
            // b's store followed by a's store.
            let (sa, sb) = (list_store(a), list_store(b));
            let mut data = Vec::with_capacity(sa.len() + sb.len());
            data.extend_from_slice(sb);
            data.extend_from_slice(sa);
            list_from_store(data)
        }
        _ => crash!("++ on a non-appendable"),
    }
}

// BUILTIN CTOR HELPERS — indices match `builtins.rs`:
// Just=0/Nothing=1, Ok=0/Err=1, LT=0/EQ=1/GT=2.

unsafe fn just(v: u64) -> u64 {
    ctor1(b"Just\0".as_ptr(), 0, v)
}
unsafe fn res_ok(v: u64) -> u64 {
    ctor1(b"Ok\0".as_ptr(), 0, v)
}
unsafe fn res_err(v: u64) -> u64 {
    ctor1(b"Err\0".as_ptr(), 1, v)
}
unsafe fn is_ctor0(v: u64) -> bool {
    matches!(deref(v), Value::Ctor { index: 0, .. })
}
unsafe fn pair(a: u64, b: u64) -> u64 {
    alloc(Value::Tuple(vec![a, b]))
}

// FLOAT FORMATTING — Rust's shortest round-tripping decimal matches JS
// `String(n)` for all non-exponential magnitudes (the range Elm programs
// use); NaN/Infinity/-0 are special-cased to Elm's spellings.

fn fmt_float(x: f64) -> String {
    if x.is_nan() {
        return "NaN".to_string();
    }
    if x.is_infinite() {
        return if x < 0.0 { "-Infinity" } else { "Infinity" }.to_string();
    }
    if x == 0.0 {
        return "0".to_string();
    }
    format!("{}", x)
}

// BASICS

unsafe extern "C" fn basics_identity(a: u64) -> u64 {
    a
}
unsafe extern "C" fn basics_always(a: u64, _b: u64) -> u64 {
    a
}
unsafe extern "C" fn basics_not(b: u64) -> u64 {
    rt_bool(!rt_is_true(b))
}
unsafe extern "C" fn basics_xor(a: u64, b: u64) -> u64 {
    rt_bool(rt_is_true(a) != rt_is_true(b))
}
#[export_name = "rtb$Basics$modBy"]
unsafe extern "C" fn basics_mod_by(m: u64, n: u64) -> u64 {
    let m = as_int(m);
    if m == 0 {
        crash!("modBy 0 is undefined");
    }
    let mut r = as_int(n) % m;
    if (r > 0 && m < 0) || (r < 0 && m > 0) {
        r += m;
    }
    rt_int(r)
}
#[export_name = "rtb$Basics$remainderBy"]
unsafe extern "C" fn basics_remainder_by(m: u64, n: u64) -> u64 {
    let m = as_int(m);
    if m == 0 {
        crash!("remainderBy 0 is undefined");
    }
    rt_int(as_int(n) % m)
}
unsafe extern "C" fn basics_abs(a: u64) -> u64 {
    if is_float(a) {
        rt_float(num(a).abs())
    } else {
        rt_int(as_int(a).abs())
    }
}
unsafe extern "C" fn basics_min(a: u64, b: u64) -> u64 {
    if value_cmp(a, b) < 0 {
        a
    } else {
        b
    }
}
unsafe extern "C" fn basics_max(a: u64, b: u64) -> u64 {
    if value_cmp(a, b) > 0 {
        a
    } else {
        b
    }
}
#[export_name = "rtb$Basics$clamp"]
unsafe extern "C" fn basics_clamp(lo: u64, hi: u64, x: u64) -> u64 {
    if value_cmp(x, lo) < 0 {
        lo
    } else if value_cmp(x, hi) > 0 {
        hi
    } else {
        x
    }
}
unsafe extern "C" fn basics_compare(a: u64, b: u64) -> u64 {
    match value_cmp(a, b) {
        c if c < 0 => LT.get(),
        c if c > 0 => GT.get(),
        _ => EQ.get(),
    }
}
unsafe extern "C" fn basics_to_float(n: u64) -> u64 {
    rt_float(num(n))
}
unsafe extern "C" fn basics_round(x: u64) -> u64 {
    // Math.round: half rounds toward +infinity.
    rt_int((num(x) + 0.5).floor() as i64)
}
unsafe extern "C" fn basics_floor(x: u64) -> u64 {
    rt_int(num(x).floor() as i64)
}
unsafe extern "C" fn basics_ceiling(x: u64) -> u64 {
    rt_int(num(x).ceil() as i64)
}
unsafe extern "C" fn basics_truncate(x: u64) -> u64 {
    rt_int(num(x) as i64)
}
unsafe extern "C" fn basics_sqrt(x: u64) -> u64 {
    rt_float(num(x).sqrt())
}
unsafe extern "C" fn basics_log_base(base: u64, x: u64) -> u64 {
    rt_float(num(x).ln() / num(base).ln())
}
unsafe extern "C" fn basics_compose_l(g: u64, f: u64, x: u64) -> u64 {
    ap1(g, ap1(f, x))
}
unsafe extern "C" fn basics_compose_r(f: u64, g: u64, x: u64) -> u64 {
    ap1(g, ap1(f, x))
}
unsafe extern "C" fn basics_ap_l(f: u64, x: u64) -> u64 {
    ap1(f, x)
}
unsafe extern "C" fn basics_ap_r(x: u64, f: u64) -> u64 {
    ap1(f, x)
}
unsafe extern "C" fn basics_never(_n: u64) -> u64 {
    crash!("Basics.never was called (this is impossible in well-typed code)");
}

// LIST

unsafe extern "C" fn list_singleton(x: u64) -> u64 {
    list_from_store(vec![x])
}
#[export_name = "rtb$List$repeat"]
unsafe extern "C" fn list_repeat(n: u64, x: u64) -> u64 {
    // Build the backing directly — repeated `cons` would copy-on-write each
    // step (O(n^2)).
    let n = as_int(n).max(0) as usize;
    list_from_store(vec![x; n])
}
#[export_name = "rtb$List$range"]
unsafe extern "C" fn list_range(lo: u64, hi: u64) -> u64 {
    let (lo, hi) = (as_int(lo), as_int(hi));
    if hi < lo {
        return nil();
    }
    // Reversed storage (head = lo at the back): [hi, hi-1, ..., lo].
    let mut data = Vec::with_capacity((hi - lo + 1) as usize);
    let mut h = hi;
    while h >= lo {
        data.push(mk_int(h));
        h -= 1;
    }
    list_from_store(data)
}
#[export_name = "rtb$List$map"]
unsafe extern "C" fn list_map(f: u64, xs: u64) -> u64 {
    // Store stays reversed under the map, so build the new store directly.
    let store = list_store(xs);
    let data: Vec<u64> = store.iter().map(|&x| ap1(f, x)).collect();
    list_from_store(data)
}
#[export_name = "rtb$List$indexedMap"]
unsafe extern "C" fn list_indexed_map(f: u64, xs: u64) -> u64 {
    let store = list_store(xs);
    let n = store.len();
    // store[i] is Elm index (n-1-i); build results in Elm order then store.
    let mut out = Vec::with_capacity(n);
    for (elm_i, &x) in store.iter().rev().enumerate() {
        out.push(ap2(f, mk_int(elm_i as i64), x));
    }
    list_from_slice(&out)
}
#[export_name = "rtb$List$foldl"]
unsafe extern "C" fn list_foldl(f: u64, mut acc: u64, xs: u64) -> u64 {
    // Elm order = reversed store.
    for &x in list_store(xs).iter().rev() {
        acc = ap2(f, x, acc);
    }
    acc
}
#[export_name = "rtb$List$foldr"]
unsafe extern "C" fn list_foldr(f: u64, mut acc: u64, xs: u64) -> u64 {
    // Right fold visits last-to-first = store order.
    for &x in list_store(xs) {
        acc = ap2(f, x, acc);
    }
    acc
}
#[export_name = "rtb$List$filter"]
unsafe extern "C" fn list_filter(is_good: u64, xs: u64) -> u64 {
    let store = list_store(xs);
    let data: Vec<u64> = store
        .iter()
        .copied()
        .filter(|&x| rt_is_true(ap1(is_good, x)))
        .collect();
    list_from_store(data)
}
#[export_name = "rtb$List$filterMap"]
unsafe extern "C" fn list_filter_map(f: u64, xs: u64) -> u64 {
    let store = list_store(xs);
    let mut data = Vec::new();
    for &x in store {
        let m = ap1(f, x);
        if is_ctor0(m) {
            data.push(rt_ctor_arg(m, 0));
        }
    }
    list_from_store(data)
}
#[export_name = "rtb$List$length"]
unsafe extern "C" fn list_length(xs: u64) -> u64 {
    rt_int(list_len(xs) as i64)
}
#[export_name = "rtb$List$reverse"]
unsafe extern "C" fn list_reverse(xs: u64) -> u64 {
    // Reversing the list reverses the store.
    list_from_store(list_store(xs).iter().rev().copied().collect())
}
#[export_name = "rtb$List$member"]
unsafe extern "C" fn list_member(y: u64, xs: u64) -> u64 {
    rt_bool(to_vec(xs).into_iter().any(|x| value_eq(y, x)))
}
unsafe extern "C" fn list_all(is_good: u64, xs: u64) -> u64 {
    rt_bool(to_vec(xs).into_iter().all(|x| rt_is_true(ap1(is_good, x))))
}
unsafe extern "C" fn list_any(is_good: u64, xs: u64) -> u64 {
    rt_bool(to_vec(xs).into_iter().any(|x| rt_is_true(ap1(is_good, x))))
}
unsafe extern "C" fn list_maximum(xs: u64) -> u64 {
    let items = to_vec(xs);
    match items.split_first() {
        None => nothing(),
        Some((&first, rest)) => {
            let mut best = first;
            for &x in rest {
                if value_cmp(x, best) > 0 {
                    best = x;
                }
            }
            just(best)
        }
    }
}
unsafe extern "C" fn list_minimum(xs: u64) -> u64 {
    let items = to_vec(xs);
    match items.split_first() {
        None => nothing(),
        Some((&first, rest)) => {
            let mut best = first;
            for &x in rest {
                if value_cmp(x, best) < 0 {
                    best = x;
                }
            }
            just(best)
        }
    }
}
#[export_name = "rtb$List$sum"]
unsafe extern "C" fn list_sum(xs: u64) -> u64 {
    let items = to_vec(xs);
    if items.iter().any(|&x| is_float(x)) {
        rt_float(items.iter().map(|&x| num(x)).sum())
    } else {
        rt_int(items.iter().map(|&x| as_int(x)).sum())
    }
}
#[export_name = "rtb$List$product"]
unsafe extern "C" fn list_product(xs: u64) -> u64 {
    let items = to_vec(xs);
    if items.iter().any(|&x| is_float(x)) {
        rt_float(items.iter().map(|&x| num(x)).product())
    } else {
        rt_int(items.iter().map(|&x| as_int(x)).product())
    }
}
#[export_name = "rtb$List$concat"]
unsafe extern "C" fn list_concat(xss: u64) -> u64 {
    let mut out = Vec::new();
    for xs in to_vec(xss) {
        out.extend(to_vec(xs));
    }
    list_from_slice(&out)
}
unsafe extern "C" fn list_concat_map(f: u64, xs: u64) -> u64 {
    list_concat(list_map(f, xs))
}
unsafe extern "C" fn list_intersperse(sep: u64, xs: u64) -> u64 {
    let items = to_vec(xs);
    let mut out = Vec::new();
    for (i, &x) in items.iter().enumerate() {
        if i > 0 {
            out.push(sep);
        }
        out.push(x);
    }
    list_from_slice(&out)
}
#[export_name = "rtb$List$map2"]
unsafe extern "C" fn list_map2(f: u64, xs: u64, ys: u64) -> u64 {
    let (xs, ys) = (to_vec(xs), to_vec(ys));
    let out: Vec<u64> = xs
        .iter()
        .zip(ys.iter())
        .map(|(&a, &b)| ap2(f, a, b))
        .collect();
    list_from_slice(&out)
}
#[export_name = "rtb$List$isEmpty"]
unsafe extern "C" fn list_is_empty(xs: u64) -> u64 {
    rt_bool(list_len(xs) == 0)
}
#[export_name = "rtb$List$head"]
unsafe extern "C" fn list_head(xs: u64) -> u64 {
    match list_store(xs).last() {
        Some(&h) => just(h),
        None => nothing(),
    }
}
#[export_name = "rtb$List$tail"]
unsafe extern "C" fn list_tail(xs: u64) -> u64 {
    if list_len(xs) == 0 {
        nothing()
    } else {
        just(rt_list_tail(xs))
    }
}
#[export_name = "rtb$List$take"]
unsafe extern "C" fn list_take(n: u64, xs: u64) -> u64 {
    let store = list_store(xs);
    let count = (as_int(n).max(0) as usize).min(store.len());
    // Take the first `count` in Elm order = the last `count` of the store.
    list_from_store(store[store.len() - count..].to_vec())
}
#[export_name = "rtb$List$drop"]
unsafe extern "C" fn list_drop(n: u64, xs: u64) -> u64 {
    let (backing, len) = list_view(xs);
    let drop = (as_int(n).max(0) as usize).min(len);
    // Dropping `drop` from the head (the back of the store) shrinks the
    // view and shares the backing.
    (*backing).rc += 1;
    alloc(Value::List {
        backing,
        len: len - drop,
    })
}
unsafe extern "C" fn list_partition(is_good: u64, xs: u64) -> u64 {
    let (mut yes, mut no) = (Vec::new(), Vec::new());
    for x in to_vec(xs) {
        if rt_is_true(ap1(is_good, x)) {
            yes.push(x);
        } else {
            no.push(x);
        }
    }
    pair(list_from_slice(&yes), list_from_slice(&no))
}
unsafe extern "C" fn list_unzip(pairs: u64) -> u64 {
    let (mut xs, mut ys) = (Vec::new(), Vec::new());
    for p in to_vec(pairs) {
        if let Value::Tuple(items) = deref(p) {
            xs.push(items[0]);
            ys.push(items[1]);
        }
    }
    pair(list_from_slice(&xs), list_from_slice(&ys))
}

/// Sort modes: 0 = plain, 1 = by key, 2 = by an Order-returning function.
unsafe fn sort_list(xs: u64, mode: u8, f: u64) -> u64 {
    let mut items = to_vec(xs);
    items.sort_by(|&a, &b| {
        let c = match mode {
            0 => value_cmp(a, b),
            1 => value_cmp(ap1(f, a), ap1(f, b)),
            _ => match deref(ap2(f, a, b)) {
                Value::Ctor { index: 0, .. } => -1,
                Value::Ctor { index: 1, .. } => 0,
                _ => 1,
            },
        };
        c.cmp(&0)
    });
    list_from_slice(&items)
}
unsafe extern "C" fn list_sort(xs: u64) -> u64 {
    sort_list(xs, 0, 0u64)
}
unsafe extern "C" fn list_sort_by(f: u64, xs: u64) -> u64 {
    sort_list(xs, 1, f)
}
unsafe extern "C" fn list_sort_with(f: u64, xs: u64) -> u64 {
    sort_list(xs, 2, f)
}

// STRING

/// The whitespace set JavaScript's `String.prototype.trim` removes — which
/// Elm's `String.trim`/`trimLeft`/`trimRight` delegate to. It is the
/// ECMAScript `WhiteSpace` set (tab, vertical tab, form feed, space, NBSP, the
/// Unicode `Zs` "space separator" category, and the BOM) plus `LineTerminator`
/// (LF, CR, line/paragraph separators). Notably wider than ASCII whitespace:
/// e.g. `String.trim "\u{00A0}"` is empty in Elm.
fn trim_space(c: char) -> bool {
    matches!(
        c,
        '\u{0009}' | '\u{000A}' | '\u{000B}' | '\u{000C}' | '\u{000D}'
            | '\u{0020}'
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200A}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202F}'
            | '\u{205F}'
            | '\u{3000}'
            | '\u{FEFF}'
    )
}

/// Byte offset of the `idx`-th codepoint, clamped to the string length.
fn char_byte(s: &str, idx: usize) -> usize {
    s.char_indices().nth(idx).map(|(b, _)| b).unwrap_or(s.len())
}

unsafe fn slice_cp(p: u64, mut start: i64, mut end: i64) -> u64 {
    let s = sstr(p);
    let len = s.chars().count() as i64;
    if start < 0 {
        start += len;
    }
    if end < 0 {
        end += len;
    }
    start = start.max(0);
    end = end.min(len);
    if end <= start {
        return mkstr(Vec::new());
    }
    let from = char_byte(s, start as usize);
    let to = char_byte(s, end as usize);
    mkstr(s.as_bytes()[from..to].to_vec())
}

#[export_name = "rtb$String$fromInt"]
unsafe extern "C" fn string_from_int(n: u64) -> u64 {
    mkstr(as_int(n).to_string().into_bytes())
}
#[export_name = "rtb$String$fromFloat"]
unsafe extern "C" fn string_from_float(n: u64) -> u64 {
    mkstr(fmt_float(num(n)).into_bytes())
}
#[export_name = "rtb$String$length"]
unsafe extern "C" fn string_length(s: u64) -> u64 {
    rt_int(sstr(s).chars().count() as i64)
}
unsafe extern "C" fn string_is_empty(s: u64) -> u64 {
    rt_bool(sbytes(s).is_empty())
}
unsafe extern "C" fn string_reverse(s: u64) -> u64 {
    mkstr(sstr(s).chars().rev().collect::<String>().into_bytes())
}
unsafe extern "C" fn string_repeat(n: u64, s: u64) -> u64 {
    let count = as_int(n);
    if count < 1 {
        return mkstr(Vec::new());
    }
    mkstr(sbytes(s).repeat(count as usize))
}
unsafe extern "C" fn string_concat(xs: u64) -> u64 {
    let mut out = Vec::new();
    for x in to_vec(xs) {
        out.extend_from_slice(sbytes(x));
    }
    mkstr(out)
}
#[export_name = "rtb$String$join"]
unsafe extern "C" fn string_join(sep: u64, xs: u64) -> u64 {
    let sep = sbytes(sep);
    let mut out = Vec::new();
    for (i, x) in to_vec(xs).into_iter().enumerate() {
        if i > 0 {
            out.extend_from_slice(sep);
        }
        out.extend_from_slice(sbytes(x));
    }
    mkstr(out)
}
unsafe extern "C" fn string_split(sep: u64, s: u64) -> u64 {
    let (sep, s) = (sstr(sep), sstr(s));
    let parts: Vec<u64> = if sep.is_empty() {
        s.chars().map(|c| mkstr(c.to_string().into_bytes())).collect()
    } else {
        s.split(sep).map(|p| mkstr(p.as_bytes().to_vec())).collect()
    };
    list_from_slice(&parts)
}
unsafe extern "C" fn string_replace(before: u64, after: u64, s: u64) -> u64 {
    string_join(after, string_split(before, s))
}
unsafe extern "C" fn string_words(s: u64) -> u64 {
    let parts: Vec<u64> = sstr(s)
        .split_whitespace()
        .map(|w| mkstr(w.as_bytes().to_vec()))
        .collect();
    list_from_slice(&parts)
}
unsafe extern "C" fn string_lines(s: u64) -> u64 {
    let parts: Vec<u64> = sstr(s)
        .split('\n')
        .map(|l| mkstr(l.as_bytes().to_vec()))
        .collect();
    list_from_slice(&parts)
}
unsafe extern "C" fn string_slice(a: u64, b: u64, s: u64) -> u64 {
    slice_cp(s, as_int(a), as_int(b))
}
unsafe extern "C" fn string_left(n: u64, s: u64) -> u64 {
    if as_int(n) < 1 {
        mkstr(Vec::new())
    } else {
        slice_cp(s, 0, as_int(n))
    }
}
unsafe extern "C" fn string_right(n: u64, s: u64) -> u64 {
    let n = as_int(n);
    if n < 1 {
        mkstr(Vec::new())
    } else {
        slice_cp(s, -n, sstr(s).chars().count() as i64)
    }
}
unsafe extern "C" fn string_drop_left(n: u64, s: u64) -> u64 {
    if as_int(n) < 1 {
        s
    } else {
        slice_cp(s, as_int(n), sstr(s).chars().count() as i64)
    }
}
unsafe extern "C" fn string_drop_right(n: u64, s: u64) -> u64 {
    if as_int(n) < 1 {
        s
    } else {
        slice_cp(s, 0, -as_int(n))
    }
}
unsafe extern "C" fn string_contains(sub: u64, s: u64) -> u64 {
    rt_bool(sstr(s).contains(sstr(sub)))
}
unsafe extern "C" fn string_starts_with(sub: u64, s: u64) -> u64 {
    rt_bool(sstr(s).starts_with(sstr(sub)))
}
unsafe extern "C" fn string_ends_with(sub: u64, s: u64) -> u64 {
    rt_bool(sstr(s).ends_with(sstr(sub)))
}
unsafe extern "C" fn string_indexes(sub: u64, s: u64) -> u64 {
    let (sub, s) = (sstr(sub), sstr(s));
    if sub.is_empty() {
        return nil();
    }
    // Codepoint indices of each byte match, mapped through char positions.
    let mut byte_to_cp = std::collections::HashMap::new();
    for (cp, (byte, _)) in s.char_indices().enumerate() {
        byte_to_cp.insert(byte, cp);
    }
    let out: Vec<u64> = s
        .match_indices(sub)
        .filter_map(|(byte, _)| byte_to_cp.get(&byte).map(|&cp| rt_int(cp as i64)))
        .collect();
    list_from_slice(&out)
}
#[no_mangle]
pub unsafe extern "C" fn string_to_int(s: u64) -> u64 {
    let bytes = sbytes(s);
    let (mut i, negative) = match bytes.first() {
        Some(b'+') => (1, false),
        Some(b'-') => (1, true),
        _ => (0, false),
    };
    // elm accepts an optional sign then one or more digits, leading zeros
    // included ("007" -> Just 7); only empty / bare-sign / non-digit is Nothing.
    let start = i;
    let mut n: i64 = 0;
    while i < bytes.len() {
        let d = bytes[i];
        if !d.is_ascii_digit() {
            return nothing();
        }
        n = n * 10 + (d - b'0') as i64;
        i += 1;
    }
    if i == start {
        return nothing();
    }
    just(rt_int(if negative { -n } else { n }))
}
#[no_mangle]
pub unsafe extern "C" fn string_to_float(s: u64) -> u64 {
    let text = sstr(s);
    if text.is_empty()
        || !text
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'+' | b'-' | b'.' | b'e' | b'E'))
    {
        return nothing();
    }
    match text.parse::<f64>() {
        Ok(f) => just(rt_float(f)),
        Err(_) => nothing(),
    }
}
unsafe extern "C" fn string_from_char(c: u64) -> u64 {
    let ch = char::from_u32(as_int(c) as u32).unwrap_or('\u{fffd}');
    mkstr(ch.to_string().into_bytes())
}
unsafe extern "C" fn string_cons(c: u64, s: u64) -> u64 {
    rt_append(string_from_char(c), s)
}
unsafe extern "C" fn string_uncons(s: u64) -> u64 {
    match sstr(s).chars().next() {
        None => nothing(),
        Some(ch) => {
            let rest = &sbytes(s)[ch.len_utf8()..];
            just(pair(rt_chr(ch as i32), mkstr(rest.to_vec())))
        }
    }
}
unsafe extern "C" fn string_to_list(s: u64) -> u64 {
    let chars: Vec<u64> = sstr(s).chars().map(|c| rt_chr(c as i32)).collect();
    list_from_slice(&chars)
}
unsafe extern "C" fn string_from_list(chars: u64) -> u64 {
    let mut out = String::new();
    for c in to_vec(chars) {
        out.push(char::from_u32(as_int(c) as u32).unwrap_or('\u{fffd}'));
    }
    mkstr(out.into_bytes())
}
unsafe extern "C" fn string_to_upper(s: u64) -> u64 {
    mkstr(sstr(s).chars().map(|c| c.to_ascii_uppercase()).collect::<String>().into_bytes())
}
unsafe extern "C" fn string_to_lower(s: u64) -> u64 {
    mkstr(sstr(s).chars().map(|c| c.to_ascii_lowercase()).collect::<String>().into_bytes())
}
unsafe extern "C" fn string_trim(s: u64) -> u64 {
    mkstr(sstr(s).trim_matches(trim_space).as_bytes().to_vec())
}
unsafe extern "C" fn string_trim_left(s: u64) -> u64 {
    mkstr(sstr(s).trim_start_matches(trim_space).as_bytes().to_vec())
}
unsafe extern "C" fn string_trim_right(s: u64) -> u64 {
    mkstr(sstr(s).trim_end_matches(trim_space).as_bytes().to_vec())
}
unsafe extern "C" fn string_pad_left(n: u64, c: u64, s: u64) -> u64 {
    let ch = char::from_u32(as_int(c) as u32).unwrap_or(' ');
    let deficit = as_int(n) - sstr(s).chars().count() as i64;
    let mut out = String::new();
    for _ in 0..deficit.max(0) {
        out.push(ch);
    }
    out.push_str(sstr(s));
    mkstr(out.into_bytes())
}
unsafe extern "C" fn string_pad_right(n: u64, c: u64, s: u64) -> u64 {
    let ch = char::from_u32(as_int(c) as u32).unwrap_or(' ');
    let deficit = as_int(n) - sstr(s).chars().count() as i64;
    let mut out = String::from(sstr(s));
    for _ in 0..deficit.max(0) {
        out.push(ch);
    }
    mkstr(out.into_bytes())
}
unsafe extern "C" fn string_map(f: u64, s: u64) -> u64 {
    let mut out = String::new();
    for ch in sstr(s).chars() {
        let mapped = ap1(f, rt_chr(ch as i32));
        out.push(char::from_u32(as_int(mapped) as u32).unwrap_or('\u{fffd}'));
    }
    mkstr(out.into_bytes())
}
unsafe extern "C" fn string_filter(is_good: u64, s: u64) -> u64 {
    let mut out = String::new();
    for ch in sstr(s).chars() {
        if rt_is_true(ap1(is_good, rt_chr(ch as i32))) {
            out.push(ch);
        }
    }
    mkstr(out.into_bytes())
}
unsafe extern "C" fn string_any(is_good: u64, s: u64) -> u64 {
    rt_bool(sstr(s).chars().any(|ch| rt_is_true(ap1(is_good, rt_chr(ch as i32)))))
}
unsafe extern "C" fn string_all(is_good: u64, s: u64) -> u64 {
    rt_bool(sstr(s).chars().all(|ch| rt_is_true(ap1(is_good, rt_chr(ch as i32)))))
}

// CHAR (ASCII case only, matching the JS backend for the tested range)

unsafe extern "C" fn char_to_code(c: u64) -> u64 {
    rt_int(as_int(c))
}
unsafe extern "C" fn char_from_code(n: u64) -> u64 {
    rt_chr(as_int(n) as i32)
}
unsafe extern "C" fn char_is_digit(c: u64) -> u64 {
    let n = as_int(c);
    rt_bool((b'0' as i64..=b'9' as i64).contains(&n))
}
unsafe extern "C" fn char_is_upper(c: u64) -> u64 {
    let n = as_int(c);
    rt_bool((b'A' as i64..=b'Z' as i64).contains(&n))
}
unsafe extern "C" fn char_is_lower(c: u64) -> u64 {
    let n = as_int(c);
    rt_bool((b'a' as i64..=b'z' as i64).contains(&n))
}
unsafe extern "C" fn char_is_alpha(c: u64) -> u64 {
    let n = as_int(c);
    rt_bool((b'a' as i64..=b'z' as i64).contains(&n) || (b'A' as i64..=b'Z' as i64).contains(&n))
}
unsafe extern "C" fn char_to_upper(c: u64) -> u64 {
    let n = as_int(c);
    if (b'a' as i64..=b'z' as i64).contains(&n) {
        rt_chr((n - 32) as i32)
    } else {
        c
    }
}
unsafe extern "C" fn char_to_lower(c: u64) -> u64 {
    let n = as_int(c);
    if (b'A' as i64..=b'Z' as i64).contains(&n) {
        rt_chr((n + 32) as i32)
    } else {
        c
    }
}

// MAYBE

unsafe extern "C" fn maybe_with_default(fallback: u64, m: u64) -> u64 {
    if is_ctor0(m) {
        rt_ctor_arg(m, 0)
    } else {
        fallback
    }
}
unsafe extern "C" fn maybe_map(f: u64, m: u64) -> u64 {
    if is_ctor0(m) {
        just(ap1(f, rt_ctor_arg(m, 0)))
    } else {
        m
    }
}
unsafe extern "C" fn maybe_map2(f: u64, ma: u64, mb: u64) -> u64 {
    if is_ctor0(ma) && is_ctor0(mb) {
        just(ap2(f, rt_ctor_arg(ma, 0), rt_ctor_arg(mb, 0)))
    } else {
        nothing()
    }
}
unsafe extern "C" fn maybe_and_then(f: u64, m: u64) -> u64 {
    if is_ctor0(m) {
        ap1(f, rt_ctor_arg(m, 0))
    } else {
        m
    }
}

// RESULT

unsafe extern "C" fn result_with_default(fallback: u64, r: u64) -> u64 {
    if is_ctor0(r) {
        rt_ctor_arg(r, 0)
    } else {
        fallback
    }
}
unsafe extern "C" fn result_map(f: u64, r: u64) -> u64 {
    if is_ctor0(r) {
        res_ok(ap1(f, rt_ctor_arg(r, 0)))
    } else {
        r
    }
}
unsafe extern "C" fn result_map_error(f: u64, r: u64) -> u64 {
    if is_ctor0(r) {
        r
    } else {
        res_err(ap1(f, rt_ctor_arg(r, 0)))
    }
}
unsafe extern "C" fn result_and_then(f: u64, r: u64) -> u64 {
    if is_ctor0(r) {
        ap1(f, rt_ctor_arg(r, 0))
    } else {
        r
    }
}
unsafe extern "C" fn result_to_maybe(r: u64) -> u64 {
    if is_ctor0(r) {
        just(rt_ctor_arg(r, 0))
    } else {
        nothing()
    }
}
unsafe extern "C" fn result_from_maybe(e: u64, m: u64) -> u64 {
    if is_ctor0(m) {
        res_ok(rt_ctor_arg(m, 0))
    } else {
        res_err(e)
    }
}

// TUPLE

unsafe extern "C" fn tuple_pair(a: u64, b: u64) -> u64 {
    pair(a, b)
}
unsafe extern "C" fn tuple_first(t: u64) -> u64 {
    rt_tuple_item(t, 0)
}
unsafe extern "C" fn tuple_second(t: u64) -> u64 {
    rt_tuple_item(t, 1)
}
unsafe extern "C" fn tuple_map_first(f: u64, t: u64) -> u64 {
    pair(ap1(f, rt_tuple_item(t, 0)), rt_tuple_item(t, 1))
}
unsafe extern "C" fn tuple_map_second(f: u64, t: u64) -> u64 {
    pair(rt_tuple_item(t, 0), ap1(f, rt_tuple_item(t, 1)))
}
unsafe extern "C" fn tuple_map_both(f: u64, g: u64, t: u64) -> u64 {
    pair(ap1(f, rt_tuple_item(t, 0)), ap1(g, rt_tuple_item(t, 1)))
}

// DEBUG — mirrors runtime.js's _Debug_toString formatting.

fn debug_string(out: &mut String, bytes: &[u8], is_char: bool) {
    // Elm strings are UTF-8; render them as characters (not per-byte, which
    // would re-encode multi-byte scalars) so non-ASCII prints faithfully.
    // Escaping matches elm's addSlashes: only \ \n \t \r \v \0 and the quote —
    // other control chars pass through raw. Chars use single quotes.
    let text = unsafe { std::str::from_utf8_unchecked(bytes) };
    let quote = if is_char { '\'' } else { '"' };
    out.push(quote);
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\u{0B}' => out.push_str("\\v"),
            '\0' => out.push_str("\\0"),
            '\'' if is_char => out.push_str("\\'"),
            '"' if !is_char => out.push_str("\\\""),
            c => out.push(c),
        }
    }
    out.push(quote);
}

unsafe fn debug_fmt(out: &mut String, v: u64) {
    if is_int(v) {
        out.push_str(&int_val(v).to_string());
        return;
    }
    match deref(v) {
        Value::Float(f) => out.push_str(&fmt_float(*f)),
        Value::Bool(b) => out.push_str(if *b { "True" } else { "False" }),
        Value::Unit => out.push_str("()"),
        // Chars render with single quotes, like elm (and the JS backend, which
        // boxes chars to distinguish them from one-char strings).
        Value::Char(c) => {
            let s = char::from_u32(*c).unwrap_or('\u{fffd}').to_string();
            debug_string(out, s.as_bytes(), true);
        }
        Value::Str(b) => debug_string(out, b, false),
        Value::List { .. } => {
            out.push('[');
            for (i, x) in to_vec(v).into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                debug_fmt(out, x);
            }
            out.push(']');
        }
        Value::Tuple(items) => {
            out.push('(');
            for (i, &x) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                debug_fmt(out, x);
            }
            out.push(')');
        }
        Value::Record { fields } => {
            out.push_str("{ ");
            // elm renders record fields in alphabetical order, not definition order.
            let mut sorted: Vec<&(*const u8, u64)> = fields.iter().collect();
            sorted.sort_by(|a, b| cname(a.0).cmp(cname(b.0)));
            for (i, &&(name, value)) in sorted.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(cname(name));
                out.push_str(" = ");
                debug_fmt(out, value);
            }
            out.push_str(" }");
        }
        Value::Ctor { name, argc, .. } => {
            out.push_str(cname(*name));
            for i in 0..*argc as usize {
                out.push(' ');
                let mut inner = String::new();
                debug_fmt(&mut inner, ctor_get(v, i));
                let head = inner.chars().next().unwrap_or(' ');
                let wrap = inner.contains(' ') && !matches!(head, '"' | '{' | '(' | '[');
                if wrap {
                    out.push('(');
                }
                out.push_str(&inner);
                if wrap {
                    out.push(')');
                }
            }
        }
        Value::Closure { .. } => out.push_str("<function>"),
        Value::Dict(pairs) => {
            out.push_str("Dict.fromList [");
            for (i, (k, val)) in pairs.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push('(');
                debug_fmt(out, *k);
                out.push(',');
                debug_fmt(out, *val);
                out.push(')');
            }
            out.push(']');
        }
        Value::Set(els) => {
            out.push_str("Set.fromList [");
            for (i, &x) in els.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                debug_fmt(out, x);
            }
            out.push(']');
        }
        Value::Array(els) => {
            out.push_str("Array.fromList [");
            for (i, &x) in els.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                debug_fmt(out, x);
            }
            out.push(']');
        }
        Value::Bytes(bytes) => {
            out.push('<');
            out.push_str(&bytes.len().to_string());
            out.push_str(" bytes>");
        }
        // A decoder is opaque; a JSON value renders as the JS backend does when
        // Debug.toString hits a raw JS value (arrays as numeric-key objects,
        // null as `<internal>`, object keys sorted) — see debug_json.
        Value::Decoder(_) => out.push_str("<internals>"),
        Value::Json(j) => debug_json(out, j),
    }
}

fn debug_json(out: &mut String, j: &JsonValue) {
    match j {
        JsonValue::Null => out.push_str("<internal>"),
        JsonValue::Bool(b) => out.push_str(if *b { "True" } else { "False" }),
        JsonValue::Number(n) => {
            if n.is_finite() && n.fract() == 0.0 {
                out.push_str(&(*n as i64).to_string());
            } else {
                out.push_str(&fmt_float(*n));
            }
        }
        JsonValue::JStr(b) => {
            // SAFETY-free: debug_string only reads the bytes.
            unsafe { debug_string(out, b, false) }
        }
        JsonValue::JArray(items) => {
            if items.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push_str("{ ");
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&i.to_string());
                out.push_str(" = ");
                debug_json(out, item);
            }
            out.push_str(" }");
        }
        JsonValue::JObject(fields) => {
            if fields.is_empty() {
                out.push_str("{}");
                return;
            }
            let mut sorted: Vec<&(Vec<u8>, JsonValue)> = fields.iter().collect();
            sorted.sort_by(|a, b| a.0.cmp(&b.0));
            out.push_str("{ ");
            for (i, (k, v)) in sorted.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&String::from_utf8_lossy(k));
                out.push_str(" = ");
                debug_json(out, v);
            }
            out.push_str(" }");
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn debug_to_string(v: u64) -> u64 {
    let mut out = String::new();
    debug_fmt(&mut out, v);
    mkstr(out.into_bytes())
}
unsafe extern "C" fn debug_log(label: u64, v: u64) -> u64 {
    let rendered = debug_to_string(v);
    let mut line = sbytes(label).to_vec();
    line.extend_from_slice(b": ");
    line.extend_from_slice(sbytes(rendered));
    out_line(&line);
    v
}
unsafe extern "C" fn debug_todo(message: u64) -> u64 {
    let mut text = b"TODO: ".to_vec();
    text.extend_from_slice(sbytes(message));
    text.push(0);
    rt_crash(text.as_ptr());
}

// TASK / CMD / SUB / TIME / PLATFORM — see the TEA section below.

const TT_SUCCEED: u32 = 0;
const TT_FAIL: u32 = 1;
const TT_AND_THEN: u32 = 2;
const TT_ON_ERROR: u32 = 3;
const TT_SLEEP: u32 = 4;
const TT_NOW: u32 = 5;

const CT_NONE: u32 = 0;
const CT_BATCH: u32 = 1;
const CT_MAP: u32 = 2;
const CT_TASK: u32 = 3;
const CT_WRITE: u32 = 4;

const ST_NONE: u32 = 0;
const ST_BATCH: u32 = 1;
const ST_MAP: u32 = 2;
const ST_TIME: u32 = 3;

unsafe fn ctor_index(v: u64) -> u32 {
    match deref(v) {
        Value::Ctor { index, .. } => *index,
        _ => crash!("expected a constructor"),
    }
}

unsafe fn closure(func: *const (), arity: i32, caps: &[u64]) -> u64 {
    let ptr = if caps.is_empty() {
        std::ptr::null()
    } else {
        caps.as_ptr()
    };
    rt_closure(func, arity, caps.len() as i32, ptr)
}

unsafe fn time_posix(ms: f64) -> u64 {
    ctor(b"Posix\0".as_ptr(), 0, vec![rt_int(ms as i64)])
}

// Task constructors.
#[no_mangle]
pub unsafe extern "C" fn task_succeed(v: u64) -> u64 {
    ctor(b"TaskSucceed\0".as_ptr(), TT_SUCCEED, vec![v])
}
#[no_mangle]
pub unsafe extern "C" fn task_fail(e: u64) -> u64 {
    ctor(b"TaskFail\0".as_ptr(), TT_FAIL, vec![e])
}
#[no_mangle]
pub unsafe extern "C" fn task_and_then(f: u64, t: u64) -> u64 {
    ctor(b"TaskAndThen\0".as_ptr(), TT_AND_THEN, vec![f, t])
}
#[no_mangle]
pub unsafe extern "C" fn task_on_error(f: u64, t: u64) -> u64 {
    ctor(b"TaskOnError\0".as_ptr(), TT_ON_ERROR, vec![f, t])
}

unsafe extern "C" fn task_map_step(f: u64, a: u64) -> u64 {
    task_succeed(ap1(f, a))
}
#[no_mangle]
pub unsafe extern "C" fn task_map(f: u64, t: u64) -> u64 {
    task_and_then(closure(task_map_step as *const (), 2, &[f]), t)
}
unsafe extern "C" fn task_map2_inner(f: u64, a: u64, b: u64) -> u64 {
    ap2(f, a, b)
}
unsafe extern "C" fn task_map2_step(f: u64, tb: u64, a: u64) -> u64 {
    task_map(closure(task_map2_inner as *const (), 3, &[f, a]), tb)
}
#[no_mangle]
pub unsafe extern "C" fn task_map2(f: u64, ta: u64, tb: u64) -> u64 {
    task_and_then(closure(task_map2_step as *const (), 3, &[f, tb]), ta)
}
unsafe extern "C" fn task_map_error_step(f: u64, e: u64) -> u64 {
    task_fail(ap1(f, e))
}
#[no_mangle]
pub unsafe extern "C" fn task_map_error(f: u64, t: u64) -> u64 {
    task_on_error(closure(task_map_error_step as *const (), 2, &[f]), t)
}
unsafe extern "C" fn task_sequence_cons(v: u64, vs: u64) -> u64 {
    cons(v, vs)
}
unsafe extern "C" fn task_sequence_step(rest: u64, v: u64) -> u64 {
    task_map(closure(task_sequence_cons as *const (), 2, &[v]), task_sequence(rest))
}
#[no_mangle]
pub unsafe extern "C" fn task_sequence(tasks: u64) -> u64 {
    if list_len(tasks) == 0 {
        task_succeed(nil())
    } else {
        let head = rt_list_head(tasks);
        let tail = rt_list_tail(tasks);
        task_and_then(closure(task_sequence_step as *const (), 2, &[tail]), head)
    }
}
#[no_mangle]
pub unsafe extern "C" fn task_perform(to_msg: u64, t: u64) -> u64 {
    ctor(b"CmdTask\0".as_ptr(), CT_TASK, vec![task_map(to_msg, t)])
}
unsafe extern "C" fn task_attempt_ok(v: u64) -> u64 {
    task_succeed(res_ok(v))
}
unsafe extern "C" fn task_attempt_err(e: u64) -> u64 {
    task_succeed(res_err(e))
}
#[no_mangle]
pub unsafe extern "C" fn task_attempt(to_msg: u64, t: u64) -> u64 {
    let wrapped = task_on_error(
        closure(task_attempt_err as *const (), 1, &[]),
        task_and_then(closure(task_attempt_ok as *const (), 1, &[]), t),
    );
    ctor(b"CmdTask\0".as_ptr(), CT_TASK, vec![task_map(to_msg, wrapped)])
}

#[no_mangle]
pub unsafe extern "C" fn process_sleep(ms: u64) -> u64 {
    ctor(b"TaskSleep\0".as_ptr(), TT_SLEEP, vec![ms])
}

#[no_mangle]
pub unsafe extern "C" fn time_every(interval: u64, to_msg: u64) -> u64 {
    ctor(b"SubTime\0".as_ptr(), ST_TIME, vec![interval, to_msg])
}
unsafe extern "C" fn time_millis_to_posix(n: u64) -> u64 {
    ctor(b"Posix\0".as_ptr(), 0, vec![n])
}
unsafe extern "C" fn time_posix_to_millis(p: u64) -> u64 {
    rt_ctor_arg(p, 0)
}

// Time civil-date math — a port of elm/time's Elm.Kernel.Time / Time.elm.
// `Zone` is `Zone Int (List { start : Int, offset : Int })`; `Posix` is
// `Posix Int` (milliseconds).

/// `Math.floor(n / d)`; every divisor here is positive so div_euclid is floor.
fn time_floored_div(n: i64, d: i64) -> i64 {
    n.div_euclid(d)
}

struct Civil {
    year: i64,
    month: i64,
    day: i64,
}
// Ported verbatim from _Time_toCivil; JS `| 0` is truncation toward zero,
// which Rust integer `/` matches for the non-negative operands used here.
fn time_to_civil(minutes: i64) -> Civil {
    let raw_day = time_floored_div(minutes, 1440) + 719468;
    let era = (if raw_day >= 0 { raw_day } else { raw_day - 146096 }) / 146097;
    let day_of_era = raw_day - era * 146097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36524 - day_of_era / 146096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let month = mp + if mp < 10 { 3 } else { -9 };
    Civil {
        year: year + if month <= 2 { 1 } else { 0 },
        month,
        day: day_of_year - (153 * mp + 2) / 5 + 1,
    }
}

unsafe fn time_adjusted_minutes(zone: u64, posix: u64) -> i64 {
    let ms = int_val(rt_ctor_arg(posix, 0));
    let posix_minutes = time_floored_div(ms, 60000);
    // First era whose start is before now wins; otherwise the base offset.
    for era in to_vec(rt_ctor_arg(zone, 1)) {
        let start = int_val(rt_access(era, b"start\0".as_ptr()));
        if start < posix_minutes {
            return posix_minutes + int_val(rt_access(era, b"offset\0".as_ptr()));
        }
    }
    posix_minutes + int_val(rt_ctor_arg(zone, 0))
}

unsafe extern "C" fn time_custom_zone(offset: u64, eras: u64) -> u64 {
    ctor(b"Zone\0".as_ptr(), 0, vec![offset, eras])
}
unsafe extern "C" fn time_to_year(zone: u64, posix: u64) -> u64 {
    rt_int(time_to_civil(time_adjusted_minutes(zone, posix)).year)
}
unsafe extern "C" fn time_to_month(zone: u64, posix: u64) -> u64 {
    // Month = Jan | Feb | .. | Dec (indices 0..11); civil month is 1..12.
    const NAMES: [&[u8]; 12] = [
        b"Jan\0", b"Feb\0", b"Mar\0", b"Apr\0", b"May\0", b"Jun\0", b"Jul\0", b"Aug\0", b"Sep\0",
        b"Oct\0", b"Nov\0", b"Dec\0",
    ];
    let idx = (time_to_civil(time_adjusted_minutes(zone, posix)).month - 1) as u32;
    ctor(NAMES[idx as usize].as_ptr(), idx, Vec::new())
}
unsafe extern "C" fn time_to_day(zone: u64, posix: u64) -> u64 {
    rt_int(time_to_civil(time_adjusted_minutes(zone, posix)).day)
}
unsafe extern "C" fn time_to_hour(zone: u64, posix: u64) -> u64 {
    rt_int(time_floored_div(time_adjusted_minutes(zone, posix), 60).rem_euclid(24))
}
unsafe extern "C" fn time_to_minute(zone: u64, posix: u64) -> u64 {
    rt_int(time_adjusted_minutes(zone, posix).rem_euclid(60))
}
unsafe extern "C" fn time_to_second(_zone: u64, posix: u64) -> u64 {
    let ms = int_val(rt_ctor_arg(posix, 0));
    rt_int(time_floored_div(ms, 1000).rem_euclid(60))
}
unsafe extern "C" fn time_to_millis(_zone: u64, posix: u64) -> u64 {
    rt_int(int_val(rt_ctor_arg(posix, 0)).rem_euclid(1000))
}
unsafe extern "C" fn time_to_weekday(zone: u64, posix: u64) -> u64 {
    // JS indexes ['Thu','Fri','Sat','Sun','Mon','Tue','Wed'] by
    // modBy 7 (flooredDiv adjMinutes 1440); map each to the elm Weekday index
    // (Mon | Tue | Wed | Thu | Fri | Sat | Sun == 0..6).
    const TABLE: [(&[u8], u32); 7] = [
        (b"Thu\0", 3),
        (b"Fri\0", 4),
        (b"Sat\0", 5),
        (b"Sun\0", 6),
        (b"Mon\0", 0),
        (b"Tue\0", 1),
        (b"Wed\0", 2),
    ];
    let adj = time_adjusted_minutes(zone, posix);
    let wi = time_floored_div(adj, 1440).rem_euclid(7) as usize;
    let (name, idx) = TABLE[wi];
    ctor(name.as_ptr(), idx, Vec::new())
}

#[no_mangle]
pub unsafe extern "C" fn cmd_batch(cmds: u64) -> u64 {
    ctor(b"CmdBatch\0".as_ptr(), CT_BATCH, vec![cmds])
}
#[no_mangle]
pub unsafe extern "C" fn cmd_map(f: u64, cmd: u64) -> u64 {
    ctor(b"CmdMap\0".as_ptr(), CT_MAP, vec![f, cmd])
}
#[no_mangle]
pub unsafe extern "C" fn sub_batch(subs: u64) -> u64 {
    ctor(b"SubBatch\0".as_ptr(), ST_BATCH, vec![subs])
}
#[no_mangle]
pub unsafe extern "C" fn sub_map(f: u64, sub: u64) -> u64 {
    ctor(b"SubMap\0".as_ptr(), ST_MAP, vec![f, sub])
}
#[no_mangle]
pub unsafe extern "C" fn platform_worker(impl_: u64) -> u64 {
    ctor(b"Program\0".as_ptr(), 0, vec![impl_])
}
#[no_mangle]
pub unsafe extern "C" fn terminal_write_line(s: u64) -> u64 {
    ctor(b"CmdWrite\0".as_ptr(), CT_WRITE, vec![s])
}

// ADDITIONAL PURE KERNELS (audit gap-fillers)

unsafe fn ap4(f: u64, a: u64, b: u64, c: u64, d: u64) -> u64 {
    let args = [a, b, c, d];
    rt_apply(f, 4, args.as_ptr())
}
unsafe fn ap5(f: u64, a: u64, b: u64, c: u64, d: u64, e: u64) -> u64 {
    let args = [a, b, c, d, e];
    rt_apply(f, 5, args.as_ptr())
}
#[no_mangle]
pub unsafe extern "C" fn list_map4(f: u64, a: u64, b: u64, c: u64, d: u64) -> u64 {
    let (a, b, c, d) = (to_vec(a), to_vec(b), to_vec(c), to_vec(d));
    let n = a.len().min(b.len()).min(c.len()).min(d.len());
    let out: Vec<u64> = (0..n).map(|i| ap4(f, a[i], b[i], c[i], d[i])).collect();
    list_from_slice(&out)
}
#[no_mangle]
pub unsafe extern "C" fn list_map5(f: u64, a: u64, b: u64, c: u64, d: u64, e: u64) -> u64 {
    let (a, b, c, d, e) = (to_vec(a), to_vec(b), to_vec(c), to_vec(d), to_vec(e));
    let n = a.len().min(b.len()).min(c.len()).min(d.len()).min(e.len());
    let out: Vec<u64> = (0..n).map(|i| ap5(f, a[i], b[i], c[i], d[i], e[i])).collect();
    list_from_slice(&out)
}
#[no_mangle]
pub unsafe extern "C" fn maybe_map5(f: u64, a: u64, b: u64, c: u64, d: u64, e: u64) -> u64 {
    if is_ctor0(a) && is_ctor0(b) && is_ctor0(c) && is_ctor0(d) && is_ctor0(e) {
        just(ap5(
            f,
            rt_ctor_arg(a, 0),
            rt_ctor_arg(b, 0),
            rt_ctor_arg(c, 0),
            rt_ctor_arg(d, 0),
            rt_ctor_arg(e, 0),
        ))
    } else {
        nothing()
    }
}
#[no_mangle]
pub unsafe extern "C" fn result_map3(f: u64, a: u64, b: u64, c: u64) -> u64 {
    if !is_ctor0(a) {
        a
    } else if !is_ctor0(b) {
        b
    } else if !is_ctor0(c) {
        c
    } else {
        res_ok(ap3(f, rt_ctor_arg(a, 0), rt_ctor_arg(b, 0), rt_ctor_arg(c, 0)))
    }
}
#[no_mangle]
pub unsafe extern "C" fn result_map4(f: u64, a: u64, b: u64, c: u64, d: u64) -> u64 {
    for r in [a, b, c, d] {
        if !is_ctor0(r) {
            return r;
        }
    }
    res_ok(ap4(
        f,
        rt_ctor_arg(a, 0),
        rt_ctor_arg(b, 0),
        rt_ctor_arg(c, 0),
        rt_ctor_arg(d, 0),
    ))
}
#[no_mangle]
pub unsafe extern "C" fn result_map5(f: u64, a: u64, b: u64, c: u64, d: u64, e: u64) -> u64 {
    for r in [a, b, c, d, e] {
        if !is_ctor0(r) {
            return r;
        }
    }
    res_ok(ap5(
        f,
        rt_ctor_arg(a, 0),
        rt_ctor_arg(b, 0),
        rt_ctor_arg(c, 0),
        rt_ctor_arg(d, 0),
        rt_ctor_arg(e, 0),
    ))
}
unsafe fn tuple_xy(v: u64) -> (f64, f64) {
    if let Value::Tuple(t) = deref(v) {
        (num(t[0]), num(t[1]))
    } else {
        (0.0, 0.0)
    }
}

// -- Basics: trigonometry and float predicates --
#[no_mangle]
pub unsafe extern "C" fn basics_cos(x: u64) -> u64 {
    rt_float(num(x).cos())
}
#[no_mangle]
pub unsafe extern "C" fn basics_sin(x: u64) -> u64 {
    rt_float(num(x).sin())
}
#[no_mangle]
pub unsafe extern "C" fn basics_tan(x: u64) -> u64 {
    rt_float(num(x).tan())
}
#[no_mangle]
pub unsafe extern "C" fn basics_acos(x: u64) -> u64 {
    rt_float(num(x).acos())
}
#[no_mangle]
pub unsafe extern "C" fn basics_asin(x: u64) -> u64 {
    rt_float(num(x).asin())
}
#[no_mangle]
pub unsafe extern "C" fn basics_atan(x: u64) -> u64 {
    rt_float(num(x).atan())
}
#[no_mangle]
pub unsafe extern "C" fn basics_atan2(y: u64, x: u64) -> u64 {
    rt_float(num(y).atan2(num(x)))
}
#[no_mangle]
pub unsafe extern "C" fn basics_degrees(d: u64) -> u64 {
    rt_float(num(d) * std::f64::consts::PI / 180.0)
}
#[no_mangle]
pub unsafe extern "C" fn basics_radians(r: u64) -> u64 {
    rt_float(num(r))
}
#[no_mangle]
pub unsafe extern "C" fn basics_turns(t: u64) -> u64 {
    rt_float(num(t) * 2.0 * std::f64::consts::PI)
}
#[no_mangle]
pub unsafe extern "C" fn basics_from_polar(p: u64) -> u64 {
    let (r, theta) = tuple_xy(p);
    pair(rt_float(r * theta.cos()), rt_float(r * theta.sin()))
}
#[no_mangle]
pub unsafe extern "C" fn basics_to_polar(p: u64) -> u64 {
    let (x, y) = tuple_xy(p);
    pair(rt_float((x * x + y * y).sqrt()), rt_float(y.atan2(x)))
}
#[no_mangle]
pub unsafe extern "C" fn basics_is_nan(x: u64) -> u64 {
    rt_bool(num(x).is_nan())
}
#[no_mangle]
pub unsafe extern "C" fn basics_is_infinite(x: u64) -> u64 {
    rt_bool(num(x).is_infinite())
}

// -- Bitwise: 32-bit, matching JS semantics --
#[no_mangle]
pub unsafe extern "C" fn bitwise_and(a: u64, b: u64) -> u64 {
    mk_int(((int_val(a) as i32) & (int_val(b) as i32)) as i64)
}
#[no_mangle]
pub unsafe extern "C" fn bitwise_or(a: u64, b: u64) -> u64 {
    mk_int(((int_val(a) as i32) | (int_val(b) as i32)) as i64)
}
#[no_mangle]
pub unsafe extern "C" fn bitwise_xor(a: u64, b: u64) -> u64 {
    mk_int(((int_val(a) as i32) ^ (int_val(b) as i32)) as i64)
}
#[no_mangle]
pub unsafe extern "C" fn bitwise_complement(a: u64) -> u64 {
    mk_int((!(int_val(a) as i32)) as i64)
}
#[no_mangle]
pub unsafe extern "C" fn bitwise_shift_left_by(offset: u64, a: u64) -> u64 {
    mk_int(((int_val(a) as i32) << (int_val(offset) as u32 & 31)) as i64)
}
#[no_mangle]
pub unsafe extern "C" fn bitwise_shift_right_by(offset: u64, a: u64) -> u64 {
    mk_int(((int_val(a) as i32) >> (int_val(offset) as u32 & 31)) as i64)
}
#[no_mangle]
pub unsafe extern "C" fn bitwise_shift_right_zf_by(offset: u64, a: u64) -> u64 {
    mk_int(((int_val(a) as u32) >> (int_val(offset) as u32 & 31)) as i64)
}

// -- Char --
#[no_mangle]
pub unsafe extern "C" fn char_is_alpha_num(c: u64) -> u64 {
    let n = as_int(c);
    rt_bool(
        (b'a' as i64..=b'z' as i64).contains(&n)
            || (b'A' as i64..=b'Z' as i64).contains(&n)
            || (b'0' as i64..=b'9' as i64).contains(&n),
    )
}
#[no_mangle]
pub unsafe extern "C" fn char_is_hex_digit(c: u64) -> u64 {
    let n = as_int(c);
    rt_bool(
        (b'0' as i64..=b'9' as i64).contains(&n)
            || (b'a' as i64..=b'f' as i64).contains(&n)
            || (b'A' as i64..=b'F' as i64).contains(&n),
    )
}

// -- List/Maybe/Result combinators --
#[no_mangle]
pub unsafe extern "C" fn list_map3(f: u64, xs: u64, ys: u64, zs: u64) -> u64 {
    let (xs, ys, zs) = (to_vec(xs), to_vec(ys), to_vec(zs));
    let n = xs.len().min(ys.len()).min(zs.len());
    let out: Vec<u64> = (0..n).map(|i| ap3(f, xs[i], ys[i], zs[i])).collect();
    list_from_slice(&out)
}
#[no_mangle]
pub unsafe extern "C" fn maybe_map3(f: u64, a: u64, b: u64, c: u64) -> u64 {
    if is_ctor0(a) && is_ctor0(b) && is_ctor0(c) {
        just(ap3(f, rt_ctor_arg(a, 0), rt_ctor_arg(b, 0), rt_ctor_arg(c, 0)))
    } else {
        nothing()
    }
}
#[no_mangle]
pub unsafe extern "C" fn maybe_map4(f: u64, a: u64, b: u64, c: u64, d: u64) -> u64 {
    if is_ctor0(a) && is_ctor0(b) && is_ctor0(c) && is_ctor0(d) {
        just(ap4(
            f,
            rt_ctor_arg(a, 0),
            rt_ctor_arg(b, 0),
            rt_ctor_arg(c, 0),
            rt_ctor_arg(d, 0),
        ))
    } else {
        nothing()
    }
}
#[no_mangle]
pub unsafe extern "C" fn result_map2(f: u64, a: u64, b: u64) -> u64 {
    if !is_ctor0(a) {
        a
    } else if !is_ctor0(b) {
        b
    } else {
        res_ok(ap2(f, rt_ctor_arg(a, 0), rt_ctor_arg(b, 0)))
    }
}

// -- String folds --
#[no_mangle]
pub unsafe extern "C" fn string_foldl(f: u64, init: u64, s: u64) -> u64 {
    let text: String = String::from_utf8_lossy(sbytes(s)).into_owned();
    let mut acc = init;
    for ch in text.chars() {
        acc = ap2(f, rt_chr(ch as i32), acc);
    }
    acc
}
#[no_mangle]
pub unsafe extern "C" fn string_foldr(f: u64, init: u64, s: u64) -> u64 {
    let text: String = String::from_utf8_lossy(sbytes(s)).into_owned();
    let mut acc = init;
    for ch in text.chars().rev() {
        acc = ap2(f, rt_chr(ch as i32), acc);
    }
    acc
}

// -- Set/Array extras --
#[no_mangle]
pub unsafe extern "C" fn set_partition(f: u64, s: u64) -> u64 {
    let mut yes = Vec::new();
    let mut no = Vec::new();
    for &x in as_set(s) {
        if truthy(ap1(f, x)) {
            yes.push(x);
        } else {
            no.push(x);
        }
    }
    pair(alloc(Value::Set(yes)), alloc(Value::Set(no)))
}
#[no_mangle]
pub unsafe extern "C" fn array_append(a: u64, b: u64) -> u64 {
    let mut els = as_array(a).to_vec();
    els.extend_from_slice(as_array(b));
    alloc(Value::Array(els))
}
#[no_mangle]
pub unsafe extern "C" fn array_to_indexed_list(a: u64) -> u64 {
    let items: Vec<u64> = as_array(a)
        .iter()
        .enumerate()
        .map(|(i, &x)| pair(mk_int(i as i64), x))
        .collect();
    list_from_slice(&items)
}

// DICT / SET / ARRAY
//
// Immutable collections over uniform value words. Dict and Set keep their
// contents sorted by `value_cmp` (so `comparable` keys work for any type);
// Array keeps insertion order. Operations copy — bump-allocated, never freed
// like everything here. O(n) inserts (a sorted Vec, not a balanced tree) —
// simple and correct; the tree can come later if it matters.

unsafe fn as_pairs<'a>(v: u64) -> &'a [(u64, u64)] {
    match deref(v) {
        Value::Dict(d) => d,
        _ => &[],
    }
}
unsafe fn as_set<'a>(v: u64) -> &'a [u64] {
    match deref(v) {
        Value::Set(s) => s,
        _ => &[],
    }
}
unsafe fn as_array<'a>(v: u64) -> &'a [u64] {
    match deref(v) {
        Value::Array(a) => a,
        _ => &[],
    }
}
unsafe fn mkbool(b: bool) -> u64 {
    if b {
        tru()
    } else {
        fls()
    }
}
unsafe fn truthy(v: u64) -> bool {
    matches!(deref(v), Value::Bool(true))
}
fn ord(c: i32) -> std::cmp::Ordering {
    c.cmp(&0)
}
unsafe fn ap3(f: u64, a: u64, b: u64, c: u64) -> u64 {
    let args = [a, b, c];
    rt_apply(f, 3, args.as_ptr())
}

// -- Dict --

#[no_mangle]
pub unsafe extern "C" fn dict_singleton(k: u64, v: u64) -> u64 {
    alloc(Value::Dict(vec![(k, v)]))
}
#[no_mangle]
pub unsafe extern "C" fn dict_insert(k: u64, v: u64, d: u64) -> u64 {
    let mut pairs = as_pairs(d).to_vec();
    match pairs.binary_search_by(|(kk, _)| ord(value_cmp(*kk, k))) {
        Ok(i) => pairs[i].1 = v,
        Err(i) => pairs.insert(i, (k, v)),
    }
    alloc(Value::Dict(pairs))
}
#[no_mangle]
pub unsafe extern "C" fn dict_get(k: u64, d: u64) -> u64 {
    let pairs = as_pairs(d);
    match pairs.binary_search_by(|(kk, _)| ord(value_cmp(*kk, k))) {
        Ok(i) => just(pairs[i].1),
        Err(_) => nothing(),
    }
}
#[no_mangle]
pub unsafe extern "C" fn dict_remove(k: u64, d: u64) -> u64 {
    let mut pairs = as_pairs(d).to_vec();
    if let Ok(i) = pairs.binary_search_by(|(kk, _)| ord(value_cmp(*kk, k))) {
        pairs.remove(i);
    }
    alloc(Value::Dict(pairs))
}
#[no_mangle]
pub unsafe extern "C" fn dict_member(k: u64, d: u64) -> u64 {
    mkbool(as_pairs(d).binary_search_by(|(kk, _)| ord(value_cmp(*kk, k))).is_ok())
}
#[no_mangle]
pub unsafe extern "C" fn dict_is_empty(d: u64) -> u64 {
    mkbool(as_pairs(d).is_empty())
}
#[no_mangle]
pub unsafe extern "C" fn dict_size(d: u64) -> u64 {
    mk_int(as_pairs(d).len() as i64)
}
#[no_mangle]
pub unsafe extern "C" fn dict_keys(d: u64) -> u64 {
    let ks: Vec<u64> = as_pairs(d).iter().map(|(k, _)| *k).collect();
    list_from_slice(&ks)
}
#[no_mangle]
pub unsafe extern "C" fn dict_values(d: u64) -> u64 {
    let vs: Vec<u64> = as_pairs(d).iter().map(|(_, v)| *v).collect();
    list_from_slice(&vs)
}
#[no_mangle]
pub unsafe extern "C" fn dict_to_list(d: u64) -> u64 {
    let items: Vec<u64> = as_pairs(d).iter().map(|(k, v)| pair(*k, *v)).collect();
    list_from_slice(&items)
}
#[no_mangle]
pub unsafe extern "C" fn dict_from_list(list: u64) -> u64 {
    let mut acc = alloc(Value::Dict(Vec::new()));
    // Head-first: later entries override earlier (Elm semantics).
    for &p in list_store(list).iter().rev() {
        if let Value::Tuple(kv) = deref(p) {
            acc = dict_insert(kv[0], kv[1], acc);
        }
    }
    acc
}
#[no_mangle]
pub unsafe extern "C" fn dict_foldl(f: u64, init: u64, d: u64) -> u64 {
    let mut acc = init;
    for &(k, v) in as_pairs(d) {
        acc = ap3(f, k, v, acc);
    }
    acc
}
#[no_mangle]
pub unsafe extern "C" fn dict_foldr(f: u64, init: u64, d: u64) -> u64 {
    let mut acc = init;
    for &(k, v) in as_pairs(d).iter().rev() {
        acc = ap3(f, k, v, acc);
    }
    acc
}
#[no_mangle]
pub unsafe extern "C" fn dict_map(f: u64, d: u64) -> u64 {
    let pairs = as_pairs(d).iter().map(|&(k, v)| (k, ap2(f, k, v))).collect();
    alloc(Value::Dict(pairs))
}
#[no_mangle]
pub unsafe extern "C" fn dict_filter(f: u64, d: u64) -> u64 {
    let pairs = as_pairs(d)
        .iter()
        .filter(|&&(k, v)| truthy(ap2(f, k, v)))
        .copied()
        .collect();
    alloc(Value::Dict(pairs))
}
#[no_mangle]
pub unsafe extern "C" fn dict_update(k: u64, f: u64, d: u64) -> u64 {
    let current = dict_get(k, d);
    match deref(ap1(f, current)) {
        Value::Ctor { index: 0, .. } => {
            let v = ctor_get(ap1(f, current), 0);
            dict_insert(k, v, d)
        }
        _ => dict_remove(k, d),
    }
}
#[no_mangle]
pub unsafe extern "C" fn dict_union(a: u64, b: u64) -> u64 {
    let mut acc = b;
    for &(k, v) in as_pairs(a) {
        acc = dict_insert(k, v, acc);
    }
    acc
}
#[no_mangle]
pub unsafe extern "C" fn dict_intersect(a: u64, b: u64) -> u64 {
    let pairs = as_pairs(a)
        .iter()
        .filter(|&&(k, _)| as_pairs(b).binary_search_by(|(kk, _)| ord(value_cmp(*kk, k))).is_ok())
        .copied()
        .collect();
    alloc(Value::Dict(pairs))
}
#[no_mangle]
pub unsafe extern "C" fn dict_diff(a: u64, b: u64) -> u64 {
    let pairs = as_pairs(a)
        .iter()
        .filter(|&&(k, _)| as_pairs(b).binary_search_by(|(kk, _)| ord(value_cmp(*kk, k))).is_err())
        .copied()
        .collect();
    alloc(Value::Dict(pairs))
}
#[no_mangle]
pub unsafe extern "C" fn dict_partition(f: u64, d: u64) -> u64 {
    let mut yes = Vec::new();
    let mut no = Vec::new();
    for &(k, v) in as_pairs(d) {
        if truthy(ap2(f, k, v)) {
            yes.push((k, v));
        } else {
            no.push((k, v));
        }
    }
    pair(alloc(Value::Dict(yes)), alloc(Value::Dict(no)))
}
#[no_mangle]
pub unsafe extern "C" fn dict_merge(
    left_step: u64,
    both_step: u64,
    right_step: u64,
    left: u64,
    right: u64,
    initial: u64,
) -> u64 {
    // Copy first: the step closures allocate, which may move the arena.
    let la = as_pairs(left).to_vec();
    let ra = as_pairs(right).to_vec();
    let mut acc = initial;
    let (mut i, mut j) = (0usize, 0usize);
    while i < la.len() && j < ra.len() {
        let (lk, lv) = la[i];
        let (rk, rv) = ra[j];
        match value_cmp(lk, rk) {
            c if c < 0 => {
                acc = ap3(left_step, lk, lv, acc);
                i += 1;
            }
            c if c > 0 => {
                acc = ap3(right_step, rk, rv, acc);
                j += 1;
            }
            _ => {
                acc = ap4(both_step, lk, lv, rv, acc);
                i += 1;
                j += 1;
            }
        }
    }
    while i < la.len() {
        acc = ap3(left_step, la[i].0, la[i].1, acc);
        i += 1;
    }
    while j < ra.len() {
        acc = ap3(right_step, ra[j].0, ra[j].1, acc);
        j += 1;
    }
    acc
}

// -- Set --

#[no_mangle]
pub unsafe extern "C" fn set_singleton(x: u64) -> u64 {
    alloc(Value::Set(vec![x]))
}
#[no_mangle]
pub unsafe extern "C" fn set_insert(x: u64, s: u64) -> u64 {
    let mut els = as_set(s).to_vec();
    if let Err(i) = els.binary_search_by(|e| ord(value_cmp(*e, x))) {
        els.insert(i, x);
    }
    alloc(Value::Set(els))
}
#[no_mangle]
pub unsafe extern "C" fn set_remove(x: u64, s: u64) -> u64 {
    let mut els = as_set(s).to_vec();
    if let Ok(i) = els.binary_search_by(|e| ord(value_cmp(*e, x))) {
        els.remove(i);
    }
    alloc(Value::Set(els))
}
#[no_mangle]
pub unsafe extern "C" fn set_member(x: u64, s: u64) -> u64 {
    mkbool(as_set(s).binary_search_by(|e| ord(value_cmp(*e, x))).is_ok())
}
#[no_mangle]
pub unsafe extern "C" fn set_is_empty(s: u64) -> u64 {
    mkbool(as_set(s).is_empty())
}
#[no_mangle]
pub unsafe extern "C" fn set_size(s: u64) -> u64 {
    mk_int(as_set(s).len() as i64)
}
#[no_mangle]
pub unsafe extern "C" fn set_to_list(s: u64) -> u64 {
    let els: Vec<u64> = as_set(s).to_vec();
    list_from_slice(&els)
}
#[no_mangle]
pub unsafe extern "C" fn set_from_list(list: u64) -> u64 {
    let mut acc = alloc(Value::Set(Vec::new()));
    for &x in list_store(list).iter().rev() {
        acc = set_insert(x, acc);
    }
    acc
}
#[no_mangle]
pub unsafe extern "C" fn set_union(a: u64, b: u64) -> u64 {
    let mut acc = b;
    for &x in as_set(a) {
        acc = set_insert(x, acc);
    }
    acc
}
#[no_mangle]
pub unsafe extern "C" fn set_intersect(a: u64, b: u64) -> u64 {
    let els = as_set(a)
        .iter()
        .filter(|&&x| as_set(b).binary_search_by(|e| ord(value_cmp(*e, x))).is_ok())
        .copied()
        .collect();
    alloc(Value::Set(els))
}
#[no_mangle]
pub unsafe extern "C" fn set_diff(a: u64, b: u64) -> u64 {
    let els = as_set(a)
        .iter()
        .filter(|&&x| as_set(b).binary_search_by(|e| ord(value_cmp(*e, x))).is_err())
        .copied()
        .collect();
    alloc(Value::Set(els))
}
#[no_mangle]
pub unsafe extern "C" fn set_foldl(f: u64, init: u64, s: u64) -> u64 {
    let mut acc = init;
    for &x in as_set(s) {
        acc = ap2(f, x, acc);
    }
    acc
}
#[no_mangle]
pub unsafe extern "C" fn set_foldr(f: u64, init: u64, s: u64) -> u64 {
    let mut acc = init;
    for &x in as_set(s).iter().rev() {
        acc = ap2(f, x, acc);
    }
    acc
}
#[no_mangle]
pub unsafe extern "C" fn set_map(f: u64, s: u64) -> u64 {
    let mut acc = alloc(Value::Set(Vec::new()));
    for &x in as_set(s) {
        acc = set_insert(ap1(f, x), acc);
    }
    acc
}
#[no_mangle]
pub unsafe extern "C" fn set_filter(f: u64, s: u64) -> u64 {
    let els = as_set(s).iter().filter(|&&x| truthy(ap1(f, x))).copied().collect();
    alloc(Value::Set(els))
}

// -- Array --

#[no_mangle]
pub unsafe extern "C" fn array_is_empty(a: u64) -> u64 {
    mkbool(as_array(a).is_empty())
}
#[no_mangle]
pub unsafe extern "C" fn array_length(a: u64) -> u64 {
    mk_int(as_array(a).len() as i64)
}
#[no_mangle]
pub unsafe extern "C" fn array_initialize(n: u64, f: u64) -> u64 {
    let n = int_val(n).max(0);
    let els = (0..n).map(|i| ap1(f, mk_int(i))).collect();
    alloc(Value::Array(els))
}
#[no_mangle]
pub unsafe extern "C" fn array_repeat(n: u64, x: u64) -> u64 {
    let n = int_val(n).max(0) as usize;
    alloc(Value::Array(vec![x; n]))
}
#[no_mangle]
pub unsafe extern "C" fn array_from_list(list: u64) -> u64 {
    let els: Vec<u64> = list_store(list).iter().rev().copied().collect();
    alloc(Value::Array(els))
}
#[no_mangle]
pub unsafe extern "C" fn array_to_list(a: u64) -> u64 {
    let els: Vec<u64> = as_array(a).to_vec();
    list_from_slice(&els)
}
#[no_mangle]
pub unsafe extern "C" fn array_get(i: u64, a: u64) -> u64 {
    let arr = as_array(a);
    let i = int_val(i);
    if i >= 0 && (i as usize) < arr.len() {
        just(arr[i as usize])
    } else {
        nothing()
    }
}
#[no_mangle]
pub unsafe extern "C" fn array_set(i: u64, v: u64, a: u64) -> u64 {
    let mut arr = as_array(a).to_vec();
    let i = int_val(i);
    if i >= 0 && (i as usize) < arr.len() {
        arr[i as usize] = v;
    }
    alloc(Value::Array(arr))
}
#[no_mangle]
pub unsafe extern "C" fn array_push(v: u64, a: u64) -> u64 {
    let mut arr = as_array(a).to_vec();
    arr.push(v);
    alloc(Value::Array(arr))
}
#[no_mangle]
pub unsafe extern "C" fn array_foldl(f: u64, init: u64, a: u64) -> u64 {
    let mut acc = init;
    for &x in as_array(a) {
        acc = ap2(f, x, acc);
    }
    acc
}
#[no_mangle]
pub unsafe extern "C" fn array_foldr(f: u64, init: u64, a: u64) -> u64 {
    let mut acc = init;
    for &x in as_array(a).iter().rev() {
        acc = ap2(f, x, acc);
    }
    acc
}
#[no_mangle]
pub unsafe extern "C" fn array_map(f: u64, a: u64) -> u64 {
    let els = as_array(a).iter().map(|&x| ap1(f, x)).collect();
    alloc(Value::Array(els))
}
#[no_mangle]
pub unsafe extern "C" fn array_indexed_map(f: u64, a: u64) -> u64 {
    let els = as_array(a)
        .iter()
        .enumerate()
        .map(|(i, &x)| ap2(f, mk_int(i as i64), x))
        .collect();
    alloc(Value::Array(els))
}
#[no_mangle]
pub unsafe extern "C" fn array_filter(f: u64, a: u64) -> u64 {
    let els = as_array(a).iter().filter(|&&x| truthy(ap1(f, x))).copied().collect();
    alloc(Value::Array(els))
}
#[no_mangle]
pub unsafe extern "C" fn array_slice(from: u64, to: u64, a: u64) -> u64 {
    let arr = as_array(a);
    let len = arr.len() as i64;
    let norm = |i: i64| -> i64 {
        let i = if i < 0 { len + i } else { i };
        i.clamp(0, len)
    };
    let (lo, hi) = (norm(int_val(from)), norm(int_val(to)));
    let els = if lo < hi {
        arr[lo as usize..hi as usize].to_vec()
    } else {
        Vec::new()
    };
    alloc(Value::Array(els))
}

// -- elm/bytes --
//
// `Bytes` is a `Value::Bytes(Vec<u8>)` (the JS runtime uses a DataView).
// `encode` walks the alm-generated `Encoder` tree — a uniform tagged value
// whose constructor names match `Bytes.Encode` (`I8`/`U8`/`I16`/…/`Bytes`) —
// exactly as the JS kernel does, so it does not depend on the (dead-code-
// eliminated) Elm `Bytes.Encode.write`. A `Decoder` is a function
// `Bytes -> Int -> (Int, a)`; `decode` runs it at offset 0. Out-of-range
// reads set a sticky failure flag (the analogue of the JS `DataView` throwing)
// which `decode` turns into `Nothing`.

// Decode failure — the analogue of the JS `DataView` throwing — is threaded
// through the decoder's *offset* rather than a shared flag: a failed read
// returns this sentinel offset, which is negative so every subsequent read's
// bounds check fails too (propagating it), and `decode` reports `Nothing`
// whenever the final offset is negative. Real offsets are always in
// `0..=len`, so a negative offset is unambiguous, and threading it through
// the value the decoder already returns is robust under any optimization.
const BYTES_FAIL_OFFSET: i64 = i64::MIN / 2;

unsafe fn as_bytes<'a>(v: u64) -> &'a [u8] {
    match deref(v) {
        Value::Bytes(b) => b.as_slice(),
        _ => crash!("expected Bytes"),
    }
}

/// The constructor name of an `Encoder`/`Endianness` value.
unsafe fn enc_name<'a>(v: u64) -> &'a str {
    match deref(v) {
        Value::Ctor { name, .. } => cname(*name),
        _ => "?",
    }
}

/// `Endianness` is `LE | BE`; little-endian is constructor index 0.
unsafe fn enc_is_le(endianness: u64) -> bool {
    match deref(endianness) {
        Value::Ctor { index, .. } => *index == 0,
        _ => true,
    }
}

/// Total encoded width, in bytes, of an `Encoder` tree.
unsafe fn encoder_width(enc: u64) -> usize {
    match enc_name(enc) {
        "I8" | "U8" => 1,
        "I16" | "U16" => 2,
        "I32" | "U32" | "F32" => 4,
        "F64" => 8,
        "Seq" => to_vec(ctor_get(enc, 1)).into_iter().map(|e| encoder_width(e)).sum(),
        "Utf8" => sbytes(ctor_get(enc, 1)).len(),
        "Bytes" => as_bytes(ctor_get(enc, 0)).len(),
        _ => 0,
    }
}

/// Append the encoding of `enc` to `buf`.
unsafe fn write_encoder(buf: &mut Vec<u8>, enc: u64) {
    match enc_name(enc) {
        "I8" | "U8" => buf.push(int_val(ctor_get(enc, 0)) as u8),
        "I16" | "U16" => {
            let le = enc_is_le(ctor_get(enc, 0));
            let n = int_val(ctor_get(enc, 1)) as u16;
            let b = n.to_le_bytes();
            if le {
                buf.extend_from_slice(&b);
            } else {
                buf.extend_from_slice(&[b[1], b[0]]);
            }
        }
        "I32" | "U32" => {
            let le = enc_is_le(ctor_get(enc, 0));
            let n = int_val(ctor_get(enc, 1)) as u32;
            if le {
                buf.extend_from_slice(&n.to_le_bytes());
            } else {
                buf.extend_from_slice(&n.to_be_bytes());
            }
        }
        "F32" => {
            let le = enc_is_le(ctor_get(enc, 0));
            let f = rt_unfloat(ctor_get(enc, 1)) as f32;
            if le {
                buf.extend_from_slice(&f.to_le_bytes());
            } else {
                buf.extend_from_slice(&f.to_be_bytes());
            }
        }
        "F64" => {
            let le = enc_is_le(ctor_get(enc, 0));
            let f = rt_unfloat(ctor_get(enc, 1));
            if le {
                buf.extend_from_slice(&f.to_le_bytes());
            } else {
                buf.extend_from_slice(&f.to_be_bytes());
            }
        }
        "Seq" => {
            for e in to_vec(ctor_get(enc, 1)) {
                write_encoder(buf, e);
            }
        }
        "Utf8" => buf.extend_from_slice(sbytes(ctor_get(enc, 1))),
        "Bytes" => buf.extend_from_slice(as_bytes(ctor_get(enc, 0))),
        _ => {}
    }
}

#[no_mangle]
pub unsafe extern "C" fn bytes_encode(enc: u64) -> u64 {
    let mut buf = Vec::with_capacity(encoder_width(enc));
    write_encoder(&mut buf, enc);
    alloc(Value::Bytes(buf))
}

#[no_mangle]
pub unsafe extern "C" fn bytes_width(b: u64) -> u64 {
    mk_int(as_bytes(b).len() as i64)
}

#[no_mangle]
pub unsafe extern "C" fn bytes_get_string_width(s: u64) -> u64 {
    // Elm strings are stored UTF-8, so the byte length is the width.
    mk_int(sbytes(s).len() as i64)
}

/// `getHostEndianness le be` — the host is little-endian; succeed with `le`.
#[no_mangle]
pub unsafe extern "C" fn bytes_get_host_endianness(le: u64, _be: u64) -> u64 {
    task_succeed(le)
}

/// A `read_*` helper: the `width` bytes at `off`, or `None` on an out-of-range
/// (or already-failed, i.e. negative) offset.
unsafe fn read_at(b: u64, off: i64, width: usize) -> Option<&'static [u8]> {
    let buf = as_bytes(b);
    if off < 0 || off as usize + width > buf.len() {
        return None;
    }
    Some(&buf[off as usize..off as usize + width])
}

/// Build the `(newOffset, value)` result of a successful decoder read.
unsafe fn read_result(off: i64, width: usize, value: u64) -> u64 {
    pair(mk_int(off + width as i64), value)
}

#[no_mangle]
pub unsafe extern "C" fn bytes_read_i8(b: u64, off: u64) -> u64 {
    let off = int_val(off);
    match read_at(b, off, 1) {
        Some(s) => read_result(off, 1, mk_int(s[0] as i8 as i64)),
        None => pair(mk_int(BYTES_FAIL_OFFSET), mk_int(0)),
    }
}
#[no_mangle]
pub unsafe extern "C" fn bytes_read_u8(b: u64, off: u64) -> u64 {
    let off = int_val(off);
    match read_at(b, off, 1) {
        Some(s) => read_result(off, 1, mk_int(s[0] as i64)),
        None => pair(mk_int(BYTES_FAIL_OFFSET), mk_int(0)),
    }
}

macro_rules! read_int {
    ($name:ident, $width:literal, $ty:ty) => {
        #[no_mangle]
        pub unsafe extern "C" fn $name(is_le: u64, b: u64, off: u64) -> u64 {
            let off = int_val(off);
            match read_at(b, off, $width) {
                Some(s) => {
                    let mut a = [0u8; $width];
                    a.copy_from_slice(s);
                    let v = if rt_is_true(is_le) {
                        <$ty>::from_le_bytes(a)
                    } else {
                        <$ty>::from_be_bytes(a)
                    };
                    read_result(off, $width, mk_int(v as i64))
                }
                None => pair(mk_int(BYTES_FAIL_OFFSET), mk_int(0)),
            }
        }
    };
}
read_int!(bytes_read_i16, 2, i16);
read_int!(bytes_read_u16, 2, u16);
read_int!(bytes_read_i32, 4, i32);
read_int!(bytes_read_u32, 4, u32);

macro_rules! read_float {
    ($name:ident, $width:literal, $ty:ty) => {
        #[no_mangle]
        pub unsafe extern "C" fn $name(is_le: u64, b: u64, off: u64) -> u64 {
            let off = int_val(off);
            match read_at(b, off, $width) {
                Some(s) => {
                    let mut a = [0u8; $width];
                    a.copy_from_slice(s);
                    let v = if rt_is_true(is_le) {
                        <$ty>::from_le_bytes(a)
                    } else {
                        <$ty>::from_be_bytes(a)
                    };
                    read_result(off, $width, rt_float(v as f64))
                }
                None => pair(mk_int(BYTES_FAIL_OFFSET), rt_float(0.0)),
            }
        }
    };
}
read_float!(bytes_read_f32, 4, f32);
read_float!(bytes_read_f64, 8, f64);

#[no_mangle]
pub unsafe extern "C" fn bytes_read_bytes(len: u64, b: u64, off: u64) -> u64 {
    let off = int_val(off);
    let len = int_val(len).max(0) as usize;
    match read_at(b, off, len) {
        Some(s) => read_result(off, len, alloc(Value::Bytes(s.to_vec()))),
        None => pair(mk_int(BYTES_FAIL_OFFSET), alloc(Value::Bytes(Vec::new()))),
    }
}

#[no_mangle]
pub unsafe extern "C" fn bytes_read_string(len: u64, b: u64, off: u64) -> u64 {
    let off = int_val(off);
    let len = int_val(len).max(0) as usize;
    match read_at(b, off, len) {
        Some(s) => read_result(off, len, mkstr(s.to_vec())),
        None => pair(mk_int(BYTES_FAIL_OFFSET), mkstr(Vec::new())),
    }
}

/// `Bytes.Decode.fail` — always fails.
#[no_mangle]
pub unsafe extern "C" fn bytes_decode_failure(_b: u64, _off: u64) -> u64 {
    pair(mk_int(BYTES_FAIL_OFFSET), mk_int(0))
}

#[no_mangle]
pub unsafe extern "C" fn bytes_decode(decoder: u64, bytes: u64) -> u64 {
    let result = ap2(decoder, bytes, mk_int(0));
    // `result` is `(offset, value)`; a negative offset means a read ran past
    // the end (or `fail` fired), so the decode failed.
    match deref(result) {
        Value::Tuple(items) if items.len() == 2 && int_val(items[0]) >= 0 => just(items[1]),
        _ => nothing(),
    }
}

// RANDOM — elm/random's PCG-XSH-RR, ported byte-identically from runtime.js
// so generated sequences (and thus fuzz-test inputs) match elm/JS bit-for-bit.
// A Seed is the uniform ctor `Seed a b` (index 0, two uint32 ints) and a
// Generator is the uniform ctor `Generator gen` (index 0) wrapping a
// `seed -> (value, seed)` closure — exactly the representation the typed
// backend's box/unbox produces for elm/random's `type Seed = Seed Int Int`
// and `type Generator a = Generator (Seed -> ( a, Seed ))`, so kernel-built
// and Elm-built generators interoperate across the boxed boundary.

/// ECMAScript `ToUint32` of an f64: truncate toward zero, then take mod 2^32.
/// The PCG `word` multiply overflows 2^53, so JS's `>>> 0` recovers the low
/// 32 bits of the f64's exact integer value — an `i64` cast reproduces that
/// exactly (the product magnitude stays well under 2^63).
fn to_uint32(f: f64) -> u32 {
    if !f.is_finite() {
        return 0;
    }
    (f as i64 as u64 & 0xFFFF_FFFF) as u32
}

unsafe fn seed_a(seed: u64) -> u32 {
    int_val(rt_ctor_arg(seed, 0)) as u32
}
unsafe fn seed_b(seed: u64) -> u32 {
    int_val(rt_ctor_arg(seed, 1)) as u32
}
unsafe fn mk_seed(a: u32, b: u32) -> u64 {
    ctor(b"Seed\0".as_ptr(), 0, vec![rt_int(a as i64), rt_int(b as i64)])
}
unsafe fn mk_generator(gen: u64) -> u64 {
    ctor1(b"Generator\0".as_ptr(), 0, gen)
}

fn random_peel(a: u32) -> u32 {
    // RXS-M-XS output permutation, mirroring `_Random_peel` op-for-op.
    let shift = (a >> 28) + 4; // 4..=19
    let inner = (a as i32) ^ ((a >> shift) as i32);
    let word = to_uint32((inner as f64) * 277803737.0);
    ((word >> 22) as i32 ^ word as i32) as u32
}

unsafe fn random_next_seed(seed: u64) -> u64 {
    let a = seed_a(seed);
    let b = seed_b(seed);
    // `a * 1664525 + b` stays under 2^53, so the u64 arithmetic is exact and
    // truncating to u32 matches JS's `>>> 0`.
    let na = ((a as u64) * 1664525 + b as u64) as u32;
    mk_seed(na, b)
}

unsafe extern "C" fn random_initial_seed(x: u64) -> u64 {
    let seed1 = random_next_seed(mk_seed(0, 1013904223));
    let state2 = to_uint32(seed_a(seed1) as f64 + int_val(x) as f64);
    random_next_seed(mk_seed(state2, seed_b(seed1)))
}

unsafe extern "C" fn random_int(a: u64, b: u64) -> u64 {
    mk_generator(closure(random_int_gen as *const (), 3, &[a, b]))
}
unsafe extern "C" fn random_int_gen(a: u64, b: u64, seed0: u64) -> u64 {
    let av = int_val(a);
    let bv = int_val(b);
    let lo = av.min(bv);
    let hi = av.max(bv);
    let range = hi - lo + 1;
    if ((range - 1) as i32) & (range as i32) == 0 {
        // Power-of-two range: one peel masked to the low bits (via ToInt32),
        // then `>>> 0` and `+ lo`.
        let masked = ((range - 1) as i32) & (random_peel(seed_a(seed0)) as i32);
        return pair(rt_int(masked as u32 as i64 + lo), random_next_seed(seed0));
    }
    let neg = to_uint32(-(range as f64));
    let threshold = (neg as u64 % range as u64) as u32;
    let mut seed = seed0;
    loop {
        let x = random_peel(seed_a(seed));
        let seed_n = random_next_seed(seed);
        if x < threshold {
            seed = seed_n;
            continue;
        }
        let val = (x as u64 % range as u64) as i64 + lo;
        return pair(rt_int(val), seed_n);
    }
}

unsafe extern "C" fn random_float(a: u64, b: u64) -> u64 {
    mk_generator(closure(random_float_gen as *const (), 3, &[a, b]))
}
unsafe extern "C" fn random_float_gen(a: u64, b: u64, seed0: u64) -> u64 {
    let af = num(a);
    let seed1 = random_next_seed(seed0);
    let n0 = random_peel(seed_a(seed0));
    let n1 = random_peel(seed_a(seed1));
    let hi = (n0 & 0x03FF_FFFF) as f64;
    let lo = (n1 & 0x07FF_FFFF) as f64;
    let val = (hi * 134217728.0 + lo) / 9007199254740992.0;
    let range = (num(b) - af).abs();
    pair(rt_float(val * range + af), random_next_seed(seed1))
}

unsafe extern "C" fn random_constant(x: u64) -> u64 {
    mk_generator(closure(random_constant_gen as *const (), 2, &[x]))
}
unsafe extern "C" fn random_constant_gen(x: u64, seed: u64) -> u64 {
    pair(x, seed)
}

unsafe extern "C" fn random_map(f: u64, g: u64) -> u64 {
    mk_generator(closure(random_map_gen as *const (), 3, &[f, g]))
}
unsafe extern "C" fn random_map_gen(f: u64, g: u64, seed: u64) -> u64 {
    let r = ap1(rt_ctor_arg(g, 0), seed);
    pair(ap1(f, rt_tuple_item(r, 0)), rt_tuple_item(r, 1))
}

unsafe extern "C" fn random_map2(f: u64, ga: u64, gb: u64) -> u64 {
    mk_generator(closure(random_map2_gen as *const (), 4, &[f, ga, gb]))
}
unsafe extern "C" fn random_map2_gen(f: u64, ga: u64, gb: u64, seed: u64) -> u64 {
    let ra = ap1(rt_ctor_arg(ga, 0), seed);
    let rb = ap1(rt_ctor_arg(gb, 0), rt_tuple_item(ra, 1));
    pair(
        ap2(f, rt_tuple_item(ra, 0), rt_tuple_item(rb, 0)),
        rt_tuple_item(rb, 1),
    )
}

unsafe extern "C" fn random_map3(f: u64, ga: u64, gb: u64, gc: u64) -> u64 {
    mk_generator(closure(random_map3_gen as *const (), 5, &[f, ga, gb, gc]))
}
unsafe extern "C" fn random_map3_gen(f: u64, ga: u64, gb: u64, gc: u64, seed: u64) -> u64 {
    let ra = ap1(rt_ctor_arg(ga, 0), seed);
    let rb = ap1(rt_ctor_arg(gb, 0), rt_tuple_item(ra, 1));
    let rc = ap1(rt_ctor_arg(gc, 0), rt_tuple_item(rb, 1));
    pair(
        ap3(
            f,
            rt_tuple_item(ra, 0),
            rt_tuple_item(rb, 0),
            rt_tuple_item(rc, 0),
        ),
        rt_tuple_item(rc, 1),
    )
}

unsafe extern "C" fn random_map4(f: u64, ga: u64, gb: u64, gc: u64, gd: u64) -> u64 {
    mk_generator(closure(random_map4_gen as *const (), 6, &[f, ga, gb, gc, gd]))
}
unsafe extern "C" fn random_map4_gen(f: u64, ga: u64, gb: u64, gc: u64, gd: u64, seed: u64) -> u64 {
    let ra = ap1(rt_ctor_arg(ga, 0), seed);
    let rb = ap1(rt_ctor_arg(gb, 0), rt_tuple_item(ra, 1));
    let rc = ap1(rt_ctor_arg(gc, 0), rt_tuple_item(rb, 1));
    let rd = ap1(rt_ctor_arg(gd, 0), rt_tuple_item(rc, 1));
    pair(
        ap4(
            f,
            rt_tuple_item(ra, 0),
            rt_tuple_item(rb, 0),
            rt_tuple_item(rc, 0),
            rt_tuple_item(rd, 0),
        ),
        rt_tuple_item(rd, 1),
    )
}

unsafe extern "C" fn random_map5(f: u64, ga: u64, gb: u64, gc: u64, gd: u64, ge: u64) -> u64 {
    mk_generator(closure(
        random_map5_gen as *const (),
        7,
        &[f, ga, gb, gc, gd, ge],
    ))
}
#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn random_map5_gen(
    f: u64,
    ga: u64,
    gb: u64,
    gc: u64,
    gd: u64,
    ge: u64,
    seed: u64,
) -> u64 {
    let ra = ap1(rt_ctor_arg(ga, 0), seed);
    let rb = ap1(rt_ctor_arg(gb, 0), rt_tuple_item(ra, 1));
    let rc = ap1(rt_ctor_arg(gc, 0), rt_tuple_item(rb, 1));
    let rd = ap1(rt_ctor_arg(gd, 0), rt_tuple_item(rc, 1));
    let re = ap1(rt_ctor_arg(ge, 0), rt_tuple_item(rd, 1));
    pair(
        ap5(
            f,
            rt_tuple_item(ra, 0),
            rt_tuple_item(rb, 0),
            rt_tuple_item(rc, 0),
            rt_tuple_item(rd, 0),
            rt_tuple_item(re, 0),
        ),
        rt_tuple_item(re, 1),
    )
}

unsafe extern "C" fn random_and_then(f: u64, g: u64) -> u64 {
    mk_generator(closure(random_and_then_gen as *const (), 3, &[f, g]))
}
unsafe extern "C" fn random_and_then_gen(f: u64, g: u64, seed: u64) -> u64 {
    let r = ap1(rt_ctor_arg(g, 0), seed);
    let g2 = ap1(f, rt_tuple_item(r, 0));
    ap1(rt_ctor_arg(g2, 0), rt_tuple_item(r, 1))
}

unsafe extern "C" fn random_pair(ga: u64, gb: u64) -> u64 {
    mk_generator(closure(random_pair_gen as *const (), 3, &[ga, gb]))
}
unsafe extern "C" fn random_pair_gen(ga: u64, gb: u64, seed: u64) -> u64 {
    let ra = ap1(rt_ctor_arg(ga, 0), seed);
    let rb = ap1(rt_ctor_arg(gb, 0), rt_tuple_item(ra, 1));
    pair(
        pair(rt_tuple_item(ra, 0), rt_tuple_item(rb, 0)),
        rt_tuple_item(rb, 1),
    )
}

// `Random.weighted (w0, v0) [ (w1, v1), .. ]` — pick a value with probability
// proportional to |weight|, mirroring `_Random_weighted` in runtime.js: draw a
// float in `[0, total)` and walk the cumulative weights.
unsafe extern "C" fn random_weighted(first: u64, others: u64) -> u64 {
    let mut total = num(rt_tuple_item(first, 0)).abs();
    for p in to_vec(others) {
        total += num(rt_tuple_item(p, 0)).abs();
    }
    let float_gen = random_float(rt_float(0.0), rt_float(total));
    let picker = closure(random_weighted_pick as *const (), 3, &[first, others]);
    random_map(picker, float_gen)
}
unsafe extern "C" fn random_weighted_pick(first: u64, others: u64, countdown: u64) -> u64 {
    let c = num(countdown);
    let mut acc = num(rt_tuple_item(first, 0)).abs();
    if c <= acc {
        return rt_tuple_item(first, 1);
    }
    let os = to_vec(others);
    for p in &os {
        acc += num(rt_tuple_item(*p, 0)).abs();
        if c <= acc {
            return rt_tuple_item(*p, 1);
        }
    }
    match os.last() {
        Some(p) => rt_tuple_item(*p, 1),
        None => rt_tuple_item(first, 1),
    }
}

// `Random.uniform v0 [ v1, .. ]` — each value equally likely; a weighted pick
// with every weight equal to 1.
unsafe extern "C" fn random_uniform(head: u64, rest: u64) -> u64 {
    let vals = to_vec(rest);
    let n = (1 + vals.len()) as f64;
    let float_gen = random_float(rt_float(0.0), rt_float(n));
    let picker = closure(random_uniform_pick as *const (), 3, &[head, rest]);
    random_map(picker, float_gen)
}
unsafe extern "C" fn random_uniform_pick(head: u64, rest: u64, countdown: u64) -> u64 {
    let c = num(countdown);
    let mut acc = 1.0;
    if c <= acc {
        return head;
    }
    let rs = to_vec(rest);
    for v in &rs {
        acc += 1.0;
        if c <= acc {
            return *v;
        }
    }
    match rs.last() {
        Some(v) => *v,
        None => head,
    }
}

unsafe extern "C" fn random_list(n: u64, g: u64) -> u64 {
    mk_generator(closure(random_list_gen as *const (), 3, &[n, g]))
}
unsafe extern "C" fn random_list_gen(n: u64, g: u64, seed0: u64) -> u64 {
    // elm's listHelp prepends each value (so the result is in reverse
    // generation order) — mirror it exactly for reproducible fuzz inputs.
    let count = int_val(n);
    let gen = rt_ctor_arg(g, 0);
    let mut seed = seed0;
    let mut out: Vec<u64> = Vec::new();
    let mut i = 0i64;
    while i < count {
        let r = ap1(gen, seed);
        out.insert(0, rt_tuple_item(r, 0));
        seed = rt_tuple_item(r, 1);
        i += 1;
    }
    pair(list_from_slice(&out), seed)
}

unsafe extern "C" fn random_step(g: u64, seed: u64) -> u64 {
    // The gen closure already returns the `( value, seed )` 2-tuple elm's
    // `step` produces, so forward it unchanged.
    ap1(rt_ctor_arg(g, 0), seed)
}

unsafe extern "C" fn random_independent_seed_gen(seed0: u64) -> u64 {
    let gen = rt_ctor_arg(random_int(rt_int(0), rt_int(0xFFFF_FFFF)), 0);
    let r1 = ap1(gen, seed0);
    let r2 = ap1(gen, rt_tuple_item(r1, 1));
    let r3 = ap1(gen, rt_tuple_item(r2, 1));
    let r1_0 = int_val(rt_tuple_item(r1, 0)) as u32;
    let r2_0 = int_val(rt_tuple_item(r2, 0)) as u32;
    let r3_0 = int_val(rt_tuple_item(r3, 0)) as u32;
    let b = (1i32 | ((r2_0 as i32) ^ (r3_0 as i32))) as u32;
    let new_seed = random_next_seed(mk_seed(r1_0, b));
    pair(new_seed, rt_tuple_item(r3, 1))
}

unsafe extern "C" fn random_generate(to_msg: u64, g: u64) -> u64 {
    // Non-deterministic like JS's `Math.random()` seed; the result is a Cmd,
    // never a printed value, so it need not be reproducible.
    let seed = random_initial_seed(rt_int(to_uint32(now_ms()) as i64));
    let r = ap1(rt_ctor_arg(g, 0), seed);
    let msg = ap1(to_msg, rt_tuple_item(r, 0));
    ctor(b"CmdTask\0".as_ptr(), CT_TASK, vec![task_succeed(msg)])
}

// TEST — elm-explorations/test's `Elm.Kernel.Test.runThunk`: run a `() -> a`
// thunk and wrap its result in `Ok`. Native crashes abort rather than throw,
// so (unlike JS) a failing thunk cannot be caught into `Err`; the success
// path — all this API needs — matches JS exactly.
unsafe extern "C" fn test_run_thunk(thunk: u64) -> u64 {
    res_ok(ap1(thunk, unit()))
}

// JSON — Json.Decode / Json.Encode, ported from the JS runtime's `_Json_*`.
// `Json.Encode.Value` and `Json.Decode.Value` are the same opaque JSON tree
// (`Value::Json`); a `Decoder` is reified data run by `run_decoder`, so no
// closure is allocated per combinator. The decode `Error` type is
// `Field String Error | Index Int Error | OneOf (List Error) | Failure String Value`
// (constructor indices 0..3, matching elm/json's declaration order).

unsafe fn mk_json(j: JsonValue) -> u64 {
    alloc(Value::Json(j))
}
unsafe fn as_json<'a>(w: u64) -> &'a JsonValue {
    match deref(w) {
        Value::Json(j) => j,
        _ => crash!("expected a Json value"),
    }
}
unsafe fn mk_decoder(d: Decoder) -> u64 {
    alloc(Value::Decoder(d))
}

unsafe fn json_err_failure(msg: &str, jv: &JsonValue) -> u64 {
    ctor(
        b"Failure\0".as_ptr(),
        3,
        vec![mkstr(msg.as_bytes().to_vec()), mk_json(jv.clone())],
    )
}
unsafe fn json_err_expecting(what: &str, jv: &JsonValue) -> u64 {
    json_err_failure(&format!("Expecting {}", what), jv)
}
unsafe fn json_err_field(name: &[u8], inner: u64) -> u64 {
    ctor(b"Field\0".as_ptr(), 0, vec![mkstr(name.to_vec()), inner])
}
unsafe fn json_err_index(i: usize, inner: u64) -> u64 {
    ctor(b"Index\0".as_ptr(), 1, vec![rt_int(i as i64), inner])
}
unsafe fn json_err_oneof(errs: &[u64]) -> u64 {
    ctor(b"OneOf\0".as_ptr(), 2, vec![list_from_slice(errs)])
}

/// Interpret a reified decoder against a JSON value. `Ok` carries the decoded
/// Elm value word; `Err` carries an `Error` value word.
unsafe fn run_decoder(dec_w: u64, jv: &JsonValue) -> Result<u64, u64> {
    let dec = match deref(dec_w) {
        Value::Decoder(d) => d,
        _ => crash!("expected a Decoder"),
    };
    match dec {
        Decoder::Str => match jv {
            JsonValue::JStr(b) => Ok(mkstr(b.clone())),
            _ => Err(json_err_expecting("a STRING", jv)),
        },
        Decoder::Int => match jv {
            JsonValue::Number(n) if n.is_finite() && n.fract() == 0.0 => Ok(rt_int(*n as i64)),
            _ => Err(json_err_expecting("an INT", jv)),
        },
        Decoder::Float => match jv {
            JsonValue::Number(n) => Ok(rt_float(*n)),
            _ => Err(json_err_expecting("a FLOAT", jv)),
        },
        Decoder::Bool => match jv {
            JsonValue::Bool(b) => Ok(rt_bool(*b)),
            _ => Err(json_err_expecting("a BOOL", jv)),
        },
        Decoder::JsonVal => Ok(mk_json(jv.clone())),
        Decoder::Null(fallback) => match jv {
            JsonValue::Null => Ok(*fallback),
            _ => Err(json_err_expecting("null", jv)),
        },
        Decoder::Succeed(v) => Ok(*v),
        Decoder::Fail(msg) => Err(json_err_failure(std::str::from_utf8(msg).unwrap_or(""), jv)),
        Decoder::Field(name, sub) => match jv {
            JsonValue::JObject(fields) => {
                match fields.iter().find(|(k, _)| k == name) {
                    Some((_, val)) => match run_decoder(*sub, val) {
                        Ok(v) => Ok(v),
                        Err(e) => Err(json_err_field(name, e)),
                    },
                    None => Err(json_err_expecting(
                        &format!(
                            "an OBJECT with a field named `{}`",
                            String::from_utf8_lossy(name)
                        ),
                        jv,
                    )),
                }
            }
            // In JS a JSON array is an object, so `field name` delegates to
            // `name in array`: `field "length"` reads the array's length and a
            // numeric field name indexes it. Elm code relies on this (e.g. a
            // `requireLength` combinator decoding `field "length" int`).
            JsonValue::JArray(items) => {
                let picked = if name == b"length" {
                    Some(JsonValue::Number(items.len() as f64))
                } else {
                    std::str::from_utf8(name)
                        .ok()
                        .and_then(|s| s.parse::<usize>().ok())
                        .and_then(|i| items.get(i).cloned())
                };
                match picked {
                    Some(val) => match run_decoder(*sub, &val) {
                        Ok(v) => Ok(v),
                        Err(e) => Err(json_err_field(name, e)),
                    },
                    None => Err(json_err_expecting(
                        &format!(
                            "an OBJECT with a field named `{}`",
                            String::from_utf8_lossy(name)
                        ),
                        jv,
                    )),
                }
            }
            _ => Err(json_err_expecting(
                &format!(
                    "an OBJECT with a field named `{}`",
                    String::from_utf8_lossy(name)
                ),
                jv,
            )),
        },
        Decoder::Index(i, sub) => match jv {
            JsonValue::JArray(items) => {
                if *i >= items.len() {
                    Err(json_err_expecting(
                        &format!(
                            "a LONGER array. Need index {} but only see {} entries",
                            i,
                            items.len()
                        ),
                        jv,
                    ))
                } else {
                    match run_decoder(*sub, &items[*i]) {
                        Ok(v) => Ok(v),
                        Err(e) => Err(json_err_index(*i, e)),
                    }
                }
            }
            _ => Err(json_err_expecting("an ARRAY", jv)),
        },
        Decoder::List(sub) => match jv {
            JsonValue::JArray(items) => {
                let mut out = Vec::with_capacity(items.len());
                for (i, item) in items.iter().enumerate() {
                    match run_decoder(*sub, item) {
                        Ok(v) => out.push(v),
                        Err(e) => return Err(json_err_index(i, e)),
                    }
                }
                Ok(list_from_slice(&out))
            }
            _ => Err(json_err_expecting("a LIST", jv)),
        },
        Decoder::Array(sub) => {
            let list = run_decoder(mk_decoder_ref(Decoder::List(*sub)), jv)?;
            Ok(array_from_list(list))
        }
        Decoder::KeyValuePairs(sub) => match jv {
            JsonValue::JObject(fields) => {
                let mut out = Vec::with_capacity(fields.len());
                for (k, val) in fields {
                    match run_decoder(*sub, val) {
                        Ok(v) => out.push(pair(mkstr(k.clone()), v)),
                        Err(e) => return Err(json_err_field(k, e)),
                    }
                }
                Ok(list_from_slice(&out))
            }
            _ => Err(json_err_expecting("an OBJECT", jv)),
        },
        Decoder::Dict(sub) => {
            let pairs = run_decoder(mk_decoder_ref(Decoder::KeyValuePairs(*sub)), jv)?;
            Ok(dict_from_list(pairs))
        }
        Decoder::Maybe(sub) => match run_decoder(*sub, jv) {
            Ok(v) => Ok(just(v)),
            Err(_) => Ok(nothing()),
        },
        Decoder::OneOf(decs) => {
            let mut errs = Vec::with_capacity(decs.len());
            for d in decs {
                match run_decoder(*d, jv) {
                    Ok(v) => return Ok(v),
                    Err(e) => errs.push(e),
                }
            }
            Err(json_err_oneof(&errs))
        }
        Decoder::OneOrMore(to_value, sub) => {
            let list = run_decoder(mk_decoder_ref(Decoder::List(*sub)), jv)?;
            let arr = to_vec(list);
            if arr.is_empty() {
                Err(json_err_expecting(
                    "a JSON ARRAY with at least ONE element",
                    jv,
                ))
            } else {
                Ok(ap2(*to_value, arr[0], list_from_slice(&arr[1..])))
            }
        }
        Decoder::Map(f, sub) => {
            let v = run_decoder(*sub, jv)?;
            Ok(ap1(*f, v))
        }
        Decoder::MapMany(f, decs) => {
            let mut result = *f;
            for d in decs {
                let v = run_decoder(*d, jv)?;
                result = ap1(result, v);
            }
            Ok(result)
        }
        Decoder::AndThen(f, sub) => {
            let v = run_decoder(*sub, jv)?;
            run_decoder(ap1(*f, v), jv)
        }
        Decoder::Lazy(thunk) => run_decoder(ap1(*thunk, unit()), jv),
    }
}

// `run_decoder` needs to run freshly-built sub-decoders (array/dict reuse
// list/keyValuePairs); allocate them so a `&JsonValue` borrow is not held.
unsafe fn mk_decoder_ref(d: Decoder) -> u64 {
    mk_decoder(d)
}

unsafe fn run_to_result(dec_w: u64, jv: &JsonValue) -> u64 {
    match run_decoder(dec_w, jv) {
        Ok(v) => res_ok(v),
        Err(e) => res_err(e),
    }
}

// --- decoder combinators (values) ---
unsafe extern "C" fn json_null(fallback: u64) -> u64 {
    mk_decoder(Decoder::Null(fallback))
}
unsafe extern "C" fn json_succeed(x: u64) -> u64 {
    mk_decoder(Decoder::Succeed(x))
}
unsafe extern "C" fn json_fail(msg: u64) -> u64 {
    mk_decoder(Decoder::Fail(str_bytes(msg)))
}
unsafe extern "C" fn json_field(name: u64, dec: u64) -> u64 {
    mk_decoder(Decoder::Field(str_bytes(name), dec))
}
unsafe extern "C" fn json_at(path: u64, dec: u64) -> u64 {
    // Fold right: at [a,b] d == field a (field b d).
    let names = to_vec(path);
    let mut result = dec;
    for &n in names.iter().rev() {
        result = mk_decoder(Decoder::Field(str_bytes(n), result));
    }
    result
}
unsafe extern "C" fn json_index(i: u64, dec: u64) -> u64 {
    mk_decoder(Decoder::Index(int_val(i) as usize, dec))
}
unsafe extern "C" fn json_list_dec(dec: u64) -> u64 {
    mk_decoder(Decoder::List(dec))
}
unsafe extern "C" fn json_array_dec(dec: u64) -> u64 {
    mk_decoder(Decoder::Array(dec))
}
unsafe extern "C" fn json_key_value_pairs(dec: u64) -> u64 {
    mk_decoder(Decoder::KeyValuePairs(dec))
}
unsafe extern "C" fn json_dict_dec(dec: u64) -> u64 {
    mk_decoder(Decoder::Dict(dec))
}
unsafe extern "C" fn json_maybe(dec: u64) -> u64 {
    mk_decoder(Decoder::Maybe(dec))
}
unsafe extern "C" fn json_nullable(dec: u64) -> u64 {
    // oneOf [ null Nothing, map Just decoder ].
    let null_branch = mk_decoder(Decoder::Null(nothing()));
    let just_branch = mk_decoder(Decoder::Map(closure(json_just as *const (), 1, &[]), dec));
    mk_decoder(Decoder::OneOf(vec![null_branch, just_branch]))
}
unsafe extern "C" fn json_just(v: u64) -> u64 {
    just(v)
}
unsafe extern "C" fn json_one_of(decoders: u64) -> u64 {
    mk_decoder(Decoder::OneOf(to_vec(decoders)))
}
unsafe extern "C" fn json_one_or_more(to_value: u64, dec: u64) -> u64 {
    mk_decoder(Decoder::OneOrMore(to_value, dec))
}
unsafe extern "C" fn json_lazy(thunk: u64) -> u64 {
    mk_decoder(Decoder::Lazy(thunk))
}
unsafe extern "C" fn json_map(f: u64, dec: u64) -> u64 {
    mk_decoder(Decoder::Map(f, dec))
}
unsafe extern "C" fn json_map2(f: u64, a: u64, b: u64) -> u64 {
    mk_decoder(Decoder::MapMany(f, vec![a, b]))
}
unsafe extern "C" fn json_map3(f: u64, a: u64, b: u64, c: u64) -> u64 {
    mk_decoder(Decoder::MapMany(f, vec![a, b, c]))
}
unsafe extern "C" fn json_map4(f: u64, a: u64, b: u64, c: u64, d: u64) -> u64 {
    mk_decoder(Decoder::MapMany(f, vec![a, b, c, d]))
}
unsafe extern "C" fn json_map5(f: u64, a: u64, b: u64, c: u64, d: u64, e: u64) -> u64 {
    mk_decoder(Decoder::MapMany(f, vec![a, b, c, d, e]))
}
unsafe extern "C" fn json_map6(f: u64, a: u64, b: u64, c: u64, d: u64, e: u64, g: u64) -> u64 {
    mk_decoder(Decoder::MapMany(f, vec![a, b, c, d, e, g]))
}
unsafe extern "C" fn json_map7(f: u64, a: u64, b: u64, c: u64, d: u64, e: u64, g: u64, h: u64) -> u64 {
    mk_decoder(Decoder::MapMany(f, vec![a, b, c, d, e, g, h]))
}
#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn json_map8(
    f: u64,
    a: u64,
    b: u64,
    c: u64,
    d: u64,
    e: u64,
    g: u64,
    h: u64,
    i: u64,
) -> u64 {
    mk_decoder(Decoder::MapMany(f, vec![a, b, c, d, e, g, h, i]))
}
unsafe extern "C" fn json_and_then(f: u64, dec: u64) -> u64 {
    mk_decoder(Decoder::AndThen(f, dec))
}
unsafe extern "C" fn json_decode_value(dec: u64, value: u64) -> u64 {
    run_to_result(dec, as_json(value))
}
unsafe extern "C" fn json_decode_string(dec: u64, s: u64) -> u64 {
    let bytes = str_bytes(s);
    match json_parse(&bytes) {
        Ok(jv) => run_to_result(dec, &jv),
        Err(msg) => res_err(json_err_failure(
            &format!("This is not valid JSON! {}", msg),
            &JsonValue::JStr(bytes),
        )),
    }
}

/// `str_bytes` reads a String value's UTF-8 bytes.
unsafe fn str_bytes(w: u64) -> Vec<u8> {
    match deref(w) {
        Value::Str(b) => b.clone(),
        _ => crash!("expected a String"),
    }
}

/// A String's UTF-8 bytes, borrowed (values are never freed, so 'a is sound).
unsafe fn str_slice<'a>(w: u64) -> &'a [u8] {
    match deref(w) {
        Value::Str(b) => b,
        _ => crash!("expected a String"),
    }
}

// PARSER — Elm.Kernel.Parser string-scanning primitives (elm/parser), ported
// from the JS runtime. Offsets are opaque to Elm code (only these primitives
// interpret them), so they are byte offsets into the UTF-8 string here, while
// row/col count characters (Unicode scalars) — self-consistent, and identical
// to the JS backend for ASCII input.

unsafe fn triple(a: u64, b: u64, c: u64) -> u64 {
    alloc(Value::Tuple(vec![a, b, c]))
}

/// Decode the UTF-8 scalar at byte `o` and its byte length. Assumes valid
/// UTF-8 with `o` on a leading byte and enough bytes following.
fn decode_char(b: &[u8], o: usize) -> (u32, usize) {
    let c0 = b[o];
    if c0 < 0x80 {
        (c0 as u32, 1)
    } else if c0 >> 5 == 0b110 {
        (((c0 as u32 & 0x1F) << 6) | (b[o + 1] as u32 & 0x3F), 2)
    } else if c0 >> 4 == 0b1110 {
        (
            ((c0 as u32 & 0x0F) << 12)
                | ((b[o + 1] as u32 & 0x3F) << 6)
                | (b[o + 2] as u32 & 0x3F),
            3,
        )
    } else {
        (
            ((c0 as u32 & 0x07) << 18)
                | ((b[o + 1] as u32 & 0x3F) << 12)
                | ((b[o + 2] as u32 & 0x3F) << 6)
                | (b[o + 3] as u32 & 0x3F),
            4,
        )
    }
}

unsafe extern "C" fn parser_is_sub_string(
    small: u64,
    offset: u64,
    row: u64,
    col: u64,
    big: u64,
) -> u64 {
    let s = str_slice(small);
    let b = str_slice(big);
    let mut o = int_val(offset) as usize;
    let mut r = int_val(row);
    let mut c = int_val(col);
    let mut good = o + s.len() <= b.len();
    let mut si = 0usize;
    while good && si < s.len() {
        if o >= b.len() {
            good = false;
            break;
        }
        let (sc, sl) = decode_char(s, si);
        let (bc, bl) = decode_char(b, o);
        if sc != bc {
            good = false;
            break;
        }
        if sc == 0x0A {
            r += 1;
            c = 1;
        } else {
            c += 1;
        }
        o += bl;
        si += sl;
    }
    triple(rt_int(if good { o as i64 } else { -1 }), rt_int(r), rt_int(c))
}

unsafe extern "C" fn parser_is_sub_char(predicate: u64, offset: u64, string: u64) -> u64 {
    let s = str_slice(string);
    let o = int_val(offset) as usize;
    if s.len() <= o {
        return rt_int(-1);
    }
    let (cp, len) = decode_char(s, o);
    if !rt_is_true(ap1(predicate, alloc(Value::Char(cp)))) {
        return rt_int(-1);
    }
    if cp == 0x0A {
        rt_int(-2)
    } else {
        rt_int((o + len) as i64)
    }
}

unsafe extern "C" fn parser_is_ascii_code(code: u64, offset: u64, string: u64) -> u64 {
    let s = str_slice(string);
    let o = int_val(offset) as usize;
    let byte = if o < s.len() { s[o] as i64 } else { -1 };
    rt_bool(byte == int_val(code))
}

unsafe extern "C" fn parser_chomp_base10(offset: u64, string: u64) -> u64 {
    let s = str_slice(string);
    let mut o = int_val(offset) as usize;
    while o < s.len() && (0x30..=0x39).contains(&s[o]) {
        o += 1;
    }
    rt_int(o as i64)
}

unsafe extern "C" fn parser_consume_base(base: u64, offset: u64, string: u64) -> u64 {
    let s = str_slice(string);
    let base = int_val(base);
    let mut o = int_val(offset) as usize;
    let mut total: i64 = 0;
    while o < s.len() {
        let digit = s[o] as i64 - 0x30;
        if digit < 0 || base <= digit {
            break;
        }
        total = base * total + digit;
        o += 1;
    }
    pair(rt_int(o as i64), rt_int(total))
}

unsafe extern "C" fn parser_consume_base16(offset: u64, string: u64) -> u64 {
    let s = str_slice(string);
    let mut o = int_val(offset) as usize;
    let mut total: i64 = 0;
    while o < s.len() {
        let code = s[o];
        let d = match code {
            0x30..=0x39 => (code - 0x30) as i64,
            0x41..=0x46 => (code - 55) as i64,
            0x61..=0x66 => (code - 87) as i64,
            _ => break,
        };
        total = 16 * total + d;
        o += 1;
    }
    pair(rt_int(o as i64), rt_int(total))
}

unsafe extern "C" fn parser_find_sub_string(
    small: u64,
    offset: u64,
    row: u64,
    col: u64,
    big: u64,
) -> u64 {
    let s = str_slice(small);
    let b = str_slice(big);
    let o0 = int_val(offset) as usize;
    // Byte `indexOf` of `small` in `big` from `o0`.
    let new_offset: i64 = if s.is_empty() {
        o0.min(b.len()) as i64
    } else if o0 <= b.len() {
        b[o0..]
            .windows(s.len())
            .position(|w| w == s)
            .map(|p| (o0 + p) as i64)
            .unwrap_or(-1)
    } else {
        -1
    };
    let target = if new_offset < 0 {
        b.len()
    } else {
        new_offset as usize + s.len()
    };
    let mut o = o0;
    let mut r = int_val(row);
    let mut c = int_val(col);
    while o < target {
        let (cp, len) = decode_char(b, o);
        if cp == 0x0A {
            c = 1;
            r += 1;
        } else {
            c += 1;
        }
        o += len;
    }
    triple(rt_int(new_offset), rt_int(r), rt_int(c))
}

// --- errorToString (faithful port of elm/json) ---
fn json_indent_err(s: &str) -> String {
    s.replace('\n', "\n    ")
}
unsafe fn error_to_string_help(err: u64, context: &[String]) -> String {
    let (idx, name) = match deref(err) {
        Value::Ctor { index, name, .. } => (*index, cname(*name)),
        _ => return String::new(),
    };
    match name {
        "Field" => {
            let f = String::from_utf8_lossy(&str_bytes(ctor_get(err, 0))).to_string();
            let simple = !f.is_empty()
                && f.as_bytes()[0].is_ascii_alphabetic()
                && f.bytes().skip(1).all(|c| c.is_ascii_alphanumeric());
            let field_name = if simple {
                format!(".{}", f)
            } else {
                format!("['{}']", f)
            };
            let mut ctx = vec![field_name];
            ctx.extend_from_slice(context);
            error_to_string_help(ctor_get(err, 1), &ctx)
        }
        "Index" => {
            let i = int_val(ctor_get(err, 0));
            let mut ctx = vec![format!("[{}]", i)];
            ctx.extend_from_slice(context);
            error_to_string_help(ctor_get(err, 1), &ctx)
        }
        "OneOf" => {
            let errors = to_vec(ctor_get(err, 0));
            let path: String = context.iter().rev().cloned().collect();
            if errors.is_empty() {
                return if context.is_empty() {
                    "Ran into a Json.Decode.oneOf with no possibilities!".to_string()
                } else {
                    format!("Ran into a Json.Decode.oneOf with no possibilities at json{}", path)
                };
            }
            if errors.len() == 1 {
                return error_to_string_help(errors[0], context);
            }
            let starter = if context.is_empty() {
                "Json.Decode.oneOf".to_string()
            } else {
                format!("The Json.Decode.oneOf at json{}", path)
            };
            let mut parts = vec![format!(
                "{} failed in the following {} ways:",
                starter,
                errors.len()
            )];
            for (i, &e) in errors.iter().enumerate() {
                parts.push(format!(
                    "\n\n({}) {}",
                    i + 1,
                    json_indent_err(&error_to_string_help(e, &[]))
                ));
            }
            parts.join("\n\n")
        }
        _ => {
            // Failure.
            let _ = idx;
            let msg = String::from_utf8_lossy(&str_bytes(ctor_get(err, 0))).to_string();
            let value_json = as_json(ctor_get(err, 1));
            let rendered = json_serialize(value_json, 4);
            let intro = if context.is_empty() {
                "Problem with the given value:\n\n".to_string()
            } else {
                let path: String = context.iter().rev().cloned().collect();
                format!("Problem with the value at json{}:\n\n    ", path)
            };
            format!("{}{}\n\n{}", intro, json_indent_err(&rendered), msg)
        }
    }
}
unsafe extern "C" fn json_error_to_string(err: u64) -> u64 {
    mkstr(error_to_string_help(err, &[]).into_bytes())
}

// --- encoders (produce Value::Json) ---
unsafe extern "C" fn encode_string(s: u64) -> u64 {
    mk_json(JsonValue::JStr(str_bytes(s)))
}
unsafe extern "C" fn encode_int(n: u64) -> u64 {
    mk_json(JsonValue::Number(int_val(n) as f64))
}
unsafe extern "C" fn encode_float(n: u64) -> u64 {
    mk_json(JsonValue::Number(num(n)))
}
unsafe extern "C" fn encode_bool(b: u64) -> u64 {
    mk_json(JsonValue::Bool(rt_is_true(b)))
}
unsafe extern "C" fn encode_list(f: u64, items: u64) -> u64 {
    let arr: Vec<JsonValue> = to_vec(items)
        .into_iter()
        .map(|x| as_json(ap1(f, x)).clone())
        .collect();
    mk_json(JsonValue::JArray(arr))
}
unsafe extern "C" fn encode_array(f: u64, arr: u64) -> u64 {
    let words = match deref(arr) {
        Value::Array(els) => els.clone(),
        _ => Vec::new(),
    };
    let out: Vec<JsonValue> = words.into_iter().map(|x| as_json(ap1(f, x)).clone()).collect();
    mk_json(JsonValue::JArray(out))
}
unsafe extern "C" fn encode_set(f: u64, set: u64) -> u64 {
    let words = match deref(set) {
        Value::Set(els) => els.clone(),
        _ => Vec::new(),
    };
    let out: Vec<JsonValue> = words.into_iter().map(|x| as_json(ap1(f, x)).clone()).collect();
    mk_json(JsonValue::JArray(out))
}
unsafe extern "C" fn encode_object(pairs: u64) -> u64 {
    let mut out: Vec<(Vec<u8>, JsonValue)> = Vec::new();
    for p in to_vec(pairs) {
        let key = str_bytes(rt_tuple_item(p, 0));
        let val = as_json(rt_tuple_item(p, 1)).clone();
        // Last write wins for a duplicate key, matching JS object assignment.
        if let Some(slot) = out.iter_mut().find(|(k, _)| *k == key) {
            slot.1 = val;
        } else {
            out.push((key, val));
        }
    }
    mk_json(JsonValue::JObject(out))
}
unsafe extern "C" fn encode_dict(to_key: u64, to_value: u64, dict: u64) -> u64 {
    let entries = match deref(dict) {
        Value::Dict(pairs) => pairs.clone(),
        _ => Vec::new(),
    };
    let mut out: Vec<(Vec<u8>, JsonValue)> = Vec::with_capacity(entries.len());
    for (k, v) in entries {
        let key = str_bytes(ap1(to_key, k));
        let val = as_json(ap1(to_value, v)).clone();
        if let Some(slot) = out.iter_mut().find(|(kk, _)| *kk == key) {
            slot.1 = val;
        } else {
            out.push((key, val));
        }
    }
    mk_json(JsonValue::JObject(out))
}
unsafe extern "C" fn encode_encode(indent: u64, value: u64) -> u64 {
    mkstr(json_serialize(as_json(value), int_val(indent) as usize).into_bytes())
}

// --- JSON serializer, matching JSON.stringify(value, null, indent) ---
fn json_serialize(jv: &JsonValue, indent: usize) -> String {
    let mut out = String::new();
    json_write(jv, indent, 0, &mut out);
    out
}
fn json_number_str(n: f64) -> String {
    if !n.is_finite() {
        return "null".to_string();
    }
    if n == 0.0 {
        return "0".to_string(); // normalizes -0
    }
    fmt_float(n)
}
fn json_write(jv: &JsonValue, indent: usize, depth: usize, out: &mut String) {
    match jv {
        JsonValue::Null => out.push_str("null"),
        JsonValue::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        JsonValue::Number(n) => out.push_str(&json_number_str(*n)),
        JsonValue::JStr(s) => json_str_utf8(s, out),
        JsonValue::JArray(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                json_newline_indent(indent, depth + 1, out);
                json_write(item, indent, depth + 1, out);
            }
            json_newline_indent(indent, depth, out);
            out.push(']');
        }
        JsonValue::JObject(fields) => {
            if fields.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (i, (k, v)) in fields.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                json_newline_indent(indent, depth + 1, out);
                json_str_utf8(k, out);
                out.push(':');
                if indent > 0 {
                    out.push(' ');
                }
                json_write(v, indent, depth + 1, out);
            }
            json_newline_indent(indent, depth, out);
            out.push('}');
        }
    }
}
fn json_newline_indent(indent: usize, depth: usize, out: &mut String) {
    if indent > 0 {
        out.push('\n');
        for _ in 0..indent * depth {
            out.push(' ');
        }
    }
}
// Write a UTF-8 byte string as a JSON string literal (escaping ASCII controls
// and quotes; multi-byte UTF-8 sequences pass through verbatim).
fn json_str_utf8(s: &[u8], out: &mut String) {
    out.push('"');
    let text = String::from_utf8_lossy(s);
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

// --- JSON parser (for decodeString) ---
struct JsonParser<'a> {
    b: &'a [u8],
    i: usize,
}
impl<'a> JsonParser<'a> {
    fn ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }
    fn value(&mut self) -> Result<JsonValue, String> {
        self.ws();
        if self.i >= self.b.len() {
            return Err("Unexpected end of JSON input".to_string());
        }
        match self.b[self.i] {
            b'{' => self.object(),
            b'[' => self.array(),
            b'"' => Ok(JsonValue::JStr(self.string()?)),
            b't' => self.lit(b"true", JsonValue::Bool(true)),
            b'f' => self.lit(b"false", JsonValue::Bool(false)),
            b'n' => self.lit(b"null", JsonValue::Null),
            b'-' | b'0'..=b'9' => self.number(),
            c => Err(format!("Unexpected token {} in JSON", c as char)),
        }
    }
    fn lit(&mut self, word: &[u8], v: JsonValue) -> Result<JsonValue, String> {
        if self.b[self.i..].starts_with(word) {
            self.i += word.len();
            Ok(v)
        } else {
            Err("Unexpected token in JSON".to_string())
        }
    }
    fn number(&mut self) -> Result<JsonValue, String> {
        let start = self.i;
        if self.i < self.b.len() && self.b[self.i] == b'-' {
            self.i += 1;
        }
        while self.i < self.b.len()
            && matches!(self.b[self.i], b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-')
        {
            self.i += 1;
        }
        let text = std::str::from_utf8(&self.b[start..self.i]).unwrap_or("");
        text.parse::<f64>()
            .map(JsonValue::Number)
            .map_err(|_| format!("Invalid number: {}", text))
    }
    fn string(&mut self) -> Result<Vec<u8>, String> {
        self.i += 1; // opening quote
        let mut out = Vec::new();
        while self.i < self.b.len() {
            let c = self.b[self.i];
            match c {
                b'"' => {
                    self.i += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.i += 1;
                    if self.i >= self.b.len() {
                        return Err("Unterminated string".to_string());
                    }
                    match self.b[self.i] {
                        b'"' => out.push(b'"'),
                        b'\\' => out.push(b'\\'),
                        b'/' => out.push(b'/'),
                        b'b' => out.push(0x08),
                        b'f' => out.push(0x0c),
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'u' => {
                            let cp = self.hex4()?;
                            let scalar = if (0xD800..=0xDBFF).contains(&cp) {
                                // High surrogate: expect a following \uXXXX low surrogate.
                                if self.b[self.i + 1..].starts_with(b"\\u") {
                                    self.i += 2; // consume "\\u" for the low half (i is at 'u' of high)
                                    let lo = self.hex4()?;
                                    0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00)
                                } else {
                                    cp
                                }
                            } else {
                                cp
                            };
                            let ch = char::from_u32(scalar).unwrap_or('\u{fffd}');
                            let mut buf = [0u8; 4];
                            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                        }
                        other => return Err(format!("Invalid escape: \\{}", other as char)),
                    }
                    self.i += 1;
                }
                _ => {
                    out.push(c);
                    self.i += 1;
                }
            }
        }
        Err("Unterminated string".to_string())
    }
    fn hex4(&mut self) -> Result<u32, String> {
        // self.i points at 'u'; read the next 4 hex digits.
        if self.i + 4 >= self.b.len() {
            return Err("Invalid \\u escape".to_string());
        }
        let hex = std::str::from_utf8(&self.b[self.i + 1..self.i + 5]).unwrap_or("");
        let v = u32::from_str_radix(hex, 16).map_err(|_| "Invalid \\u escape".to_string())?;
        self.i += 4;
        Ok(v)
    }
    fn array(&mut self) -> Result<JsonValue, String> {
        self.i += 1; // [
        let mut out = Vec::new();
        self.ws();
        if self.i < self.b.len() && self.b[self.i] == b']' {
            self.i += 1;
            return Ok(JsonValue::JArray(out));
        }
        loop {
            out.push(self.value()?);
            self.ws();
            if self.i >= self.b.len() {
                return Err("Unterminated array".to_string());
            }
            match self.b[self.i] {
                b',' => {
                    self.i += 1;
                }
                b']' => {
                    self.i += 1;
                    return Ok(JsonValue::JArray(out));
                }
                c => return Err(format!("Unexpected token {} in array", c as char)),
            }
        }
    }
    fn object(&mut self) -> Result<JsonValue, String> {
        self.i += 1; // {
        let mut out: Vec<(Vec<u8>, JsonValue)> = Vec::new();
        self.ws();
        if self.i < self.b.len() && self.b[self.i] == b'}' {
            self.i += 1;
            return Ok(JsonValue::JObject(out));
        }
        loop {
            self.ws();
            if self.i >= self.b.len() || self.b[self.i] != b'"' {
                return Err("Expected string key in object".to_string());
            }
            let key = self.string()?;
            self.ws();
            if self.i >= self.b.len() || self.b[self.i] != b':' {
                return Err("Expected ':' in object".to_string());
            }
            self.i += 1;
            let val = self.value()?;
            // Last key wins, matching JSON.parse.
            if let Some(slot) = out.iter_mut().find(|(k, _)| *k == key) {
                slot.1 = val;
            } else {
                out.push((key, val));
            }
            self.ws();
            if self.i >= self.b.len() {
                return Err("Unterminated object".to_string());
            }
            match self.b[self.i] {
                b',' => {
                    self.i += 1;
                }
                b'}' => {
                    self.i += 1;
                    return Ok(JsonValue::JObject(out));
                }
                c => return Err(format!("Unexpected token {} in object", c as char)),
            }
        }
    }
}
fn json_parse(bytes: &[u8]) -> Result<JsonValue, String> {
    let mut p = JsonParser { b: bytes, i: 0 };
    let v = p.value()?;
    p.ws();
    if p.i != bytes.len() {
        return Err("Unexpected trailing characters".to_string());
    }
    Ok(v)
}

// KERNEL VALUE TABLE — the globals the generated code imports.

macro_rules! kernel_fns {
    ($( $id:ident $sym:literal $f:path , $ar:literal ; )*) => {
        $( #[export_name = $sym] static $id: Global = Global::NULL; )*
        unsafe fn init_kernel_fns() {
            $( $id.set(rt_closure(($f as usize) as *const (), $ar, 0, std::ptr::null())); )*
        }
    };
}

macro_rules! kernel_vals {
    ($( $id:ident $sym:literal ; )*) => {
        $( #[export_name = $sym] static $id: Global = Global::NULL; )*
    };
}

kernel_fns! {
    G_BASICS_IDENTITY "$Basics$identity" basics_identity, 1;
    G_BASICS_ALWAYS "$Basics$always" basics_always, 2;
    G_BASICS_NOT "$Basics$not" basics_not, 1;
    G_BASICS_XOR "$Basics$xor" basics_xor, 2;
    G_BASICS_MODBY "$Basics$modBy" basics_mod_by, 2;
    G_BASICS_REMBY "$Basics$remainderBy" basics_remainder_by, 2;
    G_BASICS_ABS "$Basics$abs" basics_abs, 1;
    G_BASICS_NEGATE "$Basics$negate" rt_neg, 1;
    G_BASICS_MIN "$Basics$min" basics_min, 2;
    G_BASICS_MAX "$Basics$max" basics_max, 2;
    G_BASICS_CLAMP "$Basics$clamp" basics_clamp, 3;
    G_BASICS_COMPARE "$Basics$compare" basics_compare, 2;
    G_BASICS_TOFLOAT "$Basics$toFloat" basics_to_float, 1;
    G_BASICS_ROUND "$Basics$round" basics_round, 1;
    G_BASICS_FLOOR "$Basics$floor" basics_floor, 1;
    G_BASICS_CEILING "$Basics$ceiling" basics_ceiling, 1;
    G_BASICS_TRUNCATE "$Basics$truncate" basics_truncate, 1;
    G_BASICS_SQRT "$Basics$sqrt" basics_sqrt, 1;
    G_BASICS_LOGBASE "$Basics$logBase" basics_log_base, 2;
    G_BASICS_COMPOSEL "$Basics$composeL" basics_compose_l, 3;
    G_BASICS_COMPOSER "$Basics$composeR" basics_compose_r, 3;
    G_BASICS_APL "$Basics$apL" basics_ap_l, 2;
    G_BASICS_APR "$Basics$apR" basics_ap_r, 2;
    G_BASICS_NEVER "$Basics$never" basics_never, 1;
    G_BASICS_APPEND "$Basics$append" rt_append, 2;
    G_BASICS_ADD "$Basics$add" rt_add, 2;
    G_BASICS_SUB "$Basics$sub" rt_sub, 2;
    G_BASICS_MUL "$Basics$mul" rt_mul, 2;
    G_BASICS_EQ "$Basics$eq" rt_eq, 2;
    G_BASICS_NEQ "$Basics$neq" rt_neq, 2;
    G_BASICS_LT "$Basics$lt" rt_lt, 2;
    G_BASICS_GT "$Basics$gt" rt_gt, 2;
    G_BASICS_LE "$Basics$le" rt_le, 2;
    G_BASICS_GE "$Basics$ge" rt_ge, 2;

    G_LIST_SINGLETON "$List$singleton" list_singleton, 1;
    G_LIST_REPEAT "$List$repeat" list_repeat, 2;
    G_LIST_RANGE "$List$range" list_range, 2;
    G_LIST_MAP "$List$map" list_map, 2;
    G_LIST_INDEXEDMAP "$List$indexedMap" list_indexed_map, 2;
    G_LIST_FOLDL "$List$foldl" list_foldl, 3;
    G_LIST_FOLDR "$List$foldr" list_foldr, 3;
    G_LIST_FILTER "$List$filter" list_filter, 2;
    G_LIST_FILTERMAP "$List$filterMap" list_filter_map, 2;
    G_LIST_LENGTH "$List$length" list_length, 1;
    G_LIST_REVERSE "$List$reverse" list_reverse, 1;
    G_LIST_MEMBER "$List$member" list_member, 2;
    G_LIST_ALL "$List$all" list_all, 2;
    G_LIST_ANY "$List$any" list_any, 2;
    G_LIST_MAXIMUM "$List$maximum" list_maximum, 1;
    G_LIST_MINIMUM "$List$minimum" list_minimum, 1;
    G_LIST_SUM "$List$sum" list_sum, 1;
    G_LIST_PRODUCT "$List$product" list_product, 1;
    G_LIST_APPEND "$List$append" rt_append, 2;
    G_LIST_CONCAT "$List$concat" list_concat, 1;
    G_LIST_CONCATMAP "$List$concatMap" list_concat_map, 2;
    G_LIST_INTERSPERSE "$List$intersperse" list_intersperse, 2;
    G_LIST_MAP2 "$List$map2" list_map2, 3;
    G_LIST_ISEMPTY "$List$isEmpty" list_is_empty, 1;
    G_LIST_HEAD "$List$head" list_head, 1;
    G_LIST_TAIL "$List$tail" list_tail, 1;
    G_LIST_TAKE "$List$take" list_take, 2;
    G_LIST_DROP "$List$drop" list_drop, 2;
    G_LIST_PARTITION "$List$partition" list_partition, 2;
    G_LIST_UNZIP "$List$unzip" list_unzip, 1;
    G_LIST_SORT "$List$sort" list_sort, 1;
    G_LIST_SORTBY "$List$sortBy" list_sort_by, 2;
    G_LIST_SORTWITH "$List$sortWith" list_sort_with, 2;
    G_LIST_CONS "$List$cons" rt_cons, 2;

    G_STRING_FROMINT "$String$fromInt" string_from_int, 1;
    G_STRING_FROMFLOAT "$String$fromFloat" string_from_float, 1;
    G_STRING_LENGTH "$String$length" string_length, 1;
    G_STRING_ISEMPTY "$String$isEmpty" string_is_empty, 1;
    G_STRING_REVERSE "$String$reverse" string_reverse, 1;
    G_STRING_REPEAT "$String$repeat" string_repeat, 2;
    G_STRING_REPLACE "$String$replace" string_replace, 3;
    G_STRING_APPEND "$String$append" rt_append, 2;
    G_STRING_CONCAT "$String$concat" string_concat, 1;
    G_STRING_SPLIT "$String$split" string_split, 2;
    G_STRING_JOIN "$String$join" string_join, 2;
    G_STRING_WORDS "$String$words" string_words, 1;
    G_STRING_LINES "$String$lines" string_lines, 1;
    G_STRING_SLICE "$String$slice" string_slice, 3;
    G_STRING_LEFT "$String$left" string_left, 2;
    G_STRING_RIGHT "$String$right" string_right, 2;
    G_STRING_DROPLEFT "$String$dropLeft" string_drop_left, 2;
    G_STRING_DROPRIGHT "$String$dropRight" string_drop_right, 2;
    G_STRING_CONTAINS "$String$contains" string_contains, 2;
    G_STRING_STARTSWITH "$String$startsWith" string_starts_with, 2;
    G_STRING_ENDSWITH "$String$endsWith" string_ends_with, 2;
    G_STRING_INDEXES "$String$indexes" string_indexes, 2;
    G_STRING_INDICES "$String$indices" string_indexes, 2;
    G_STRING_TOINT "$String$toInt" string_to_int, 1;
    G_STRING_TOFLOAT "$String$toFloat" string_to_float, 1;
    G_STRING_FROMCHAR "$String$fromChar" string_from_char, 1;
    G_STRING_CONS "$String$cons" string_cons, 2;
    G_STRING_UNCONS "$String$uncons" string_uncons, 1;
    G_STRING_TOLIST "$String$toList" string_to_list, 1;
    G_STRING_FROMLIST "$String$fromList" string_from_list, 1;
    G_STRING_TOUPPER "$String$toUpper" string_to_upper, 1;
    G_STRING_TOLOWER "$String$toLower" string_to_lower, 1;
    G_STRING_TRIM "$String$trim" string_trim, 1;
    G_STRING_TRIMLEFT "$String$trimLeft" string_trim_left, 1;
    G_STRING_TRIMRIGHT "$String$trimRight" string_trim_right, 1;
    G_STRING_PADLEFT "$String$padLeft" string_pad_left, 3;
    G_STRING_PADRIGHT "$String$padRight" string_pad_right, 3;
    G_STRING_MAP "$String$map" string_map, 2;
    G_STRING_FILTER "$String$filter" string_filter, 2;
    G_STRING_ANY "$String$any" string_any, 2;
    G_STRING_ALL "$String$all" string_all, 2;

    G_CHAR_TOCODE "$Char$toCode" char_to_code, 1;
    G_CHAR_FROMCODE "$Char$fromCode" char_from_code, 1;
    G_CHAR_ISDIGIT "$Char$isDigit" char_is_digit, 1;
    G_CHAR_ISUPPER "$Char$isUpper" char_is_upper, 1;
    G_CHAR_ISLOWER "$Char$isLower" char_is_lower, 1;
    G_CHAR_ISALPHA "$Char$isAlpha" char_is_alpha, 1;
    G_CHAR_TOUPPER "$Char$toUpper" char_to_upper, 1;
    G_CHAR_TOLOWER "$Char$toLower" char_to_lower, 1;

    G_MAYBE_WITHDEFAULT "$Maybe$withDefault" maybe_with_default, 2;
    G_MAYBE_MAP "$Maybe$map" maybe_map, 2;
    G_MAYBE_MAP2 "$Maybe$map2" maybe_map2, 3;
    G_MAYBE_ANDTHEN "$Maybe$andThen" maybe_and_then, 2;

    G_RESULT_WITHDEFAULT "$Result$withDefault" result_with_default, 2;
    G_RESULT_MAP "$Result$map" result_map, 2;
    G_RESULT_MAPERROR "$Result$mapError" result_map_error, 2;
    G_RESULT_ANDTHEN "$Result$andThen" result_and_then, 2;
    G_RESULT_TOMAYBE "$Result$toMaybe" result_to_maybe, 1;
    G_RESULT_FROMMAYBE "$Result$fromMaybe" result_from_maybe, 2;

    G_TUPLE_PAIR "$Tuple$pair" tuple_pair, 2;
    G_TUPLE_FIRST "$Tuple$first" tuple_first, 1;
    G_TUPLE_SECOND "$Tuple$second" tuple_second, 1;
    G_TUPLE_MAPFIRST "$Tuple$mapFirst" tuple_map_first, 2;
    G_TUPLE_MAPSECOND "$Tuple$mapSecond" tuple_map_second, 2;
    G_TUPLE_MAPBOTH "$Tuple$mapBoth" tuple_map_both, 3;

    G_DEBUG_TOSTRING "$Debug$toString" debug_to_string, 1;
    G_DEBUG_LOG "$Debug$log" debug_log, 2;
    G_DEBUG_TODO "$Debug$todo" debug_todo, 1;

    // Compiler-internal `Elm.Kernel.*` values referenced by source-compiled
    // packages (elm/core, elm-explorations/test).
    G_KERNEL_DEBUG_TOSTRING "$Elm$Kernel$Debug$toString" debug_to_string, 1;
    G_KERNEL_DEBUG_LOG "$Elm$Kernel$Debug$log" debug_log, 2;
    G_KERNEL_TEST_RUNTHUNK "$Elm$Kernel$Test$runThunk" test_run_thunk, 1;

    G_RANDOM_INITIALSEED "$Random$initialSeed" random_initial_seed, 1;
    G_RANDOM_INT "$Random$int" random_int, 2;
    G_RANDOM_FLOAT "$Random$float" random_float, 2;
    G_RANDOM_CONSTANT "$Random$constant" random_constant, 1;
    G_RANDOM_WEIGHTED "$Random$weighted" random_weighted, 2;
    G_RANDOM_UNIFORM "$Random$uniform" random_uniform, 2;
    G_RANDOM_MAP "$Random$map" random_map, 2;
    G_RANDOM_MAP2 "$Random$map2" random_map2, 3;
    G_RANDOM_MAP3 "$Random$map3" random_map3, 4;
    G_RANDOM_MAP4 "$Random$map4" random_map4, 5;
    G_RANDOM_MAP5 "$Random$map5" random_map5, 6;
    G_RANDOM_ANDTHEN "$Random$andThen" random_and_then, 2;
    G_RANDOM_PAIR "$Random$pair" random_pair, 2;
    G_RANDOM_LIST "$Random$list" random_list, 2;
    G_RANDOM_STEP "$Random$step" random_step, 2;
    G_RANDOM_GENERATE "$Random$generate" random_generate, 2;

    G_TASK_SUCCEED "$Task$succeed" task_succeed, 1;
    G_TASK_FAIL "$Task$fail" task_fail, 1;
    G_TASK_ANDTHEN "$Task$andThen" task_and_then, 2;
    G_TASK_ONERROR "$Task$onError" task_on_error, 2;
    G_TASK_MAP "$Task$map" task_map, 2;
    G_TASK_MAP2 "$Task$map2" task_map2, 3;
    G_TASK_MAPERROR "$Task$mapError" task_map_error, 2;
    G_TASK_SEQUENCE "$Task$sequence" task_sequence, 1;
    G_TASK_PERFORM "$Task$perform" task_perform, 2;
    G_TASK_ATTEMPT "$Task$attempt" task_attempt, 2;

    G_PROCESS_SLEEP "$Process$sleep" process_sleep, 1;

    G_TIME_EVERY "$Time$every" time_every, 2;
    G_TIME_MILLISTOPOSIX "$Time$millisToPosix" time_millis_to_posix, 1;
    G_TIME_POSIXTOMILLIS "$Time$posixToMillis" time_posix_to_millis, 1;
    G_TIME_CUSTOMZONE "$Time$customZone" time_custom_zone, 2;
    G_TIME_TOYEAR "$Time$toYear" time_to_year, 2;
    G_TIME_TOMONTH "$Time$toMonth" time_to_month, 2;
    G_TIME_TODAY "$Time$toDay" time_to_day, 2;
    G_TIME_TOWEEKDAY "$Time$toWeekday" time_to_weekday, 2;
    G_TIME_TOHOUR "$Time$toHour" time_to_hour, 2;
    G_TIME_TOMINUTE "$Time$toMinute" time_to_minute, 2;
    G_TIME_TOSECOND "$Time$toSecond" time_to_second, 2;
    G_TIME_TOMILLIS "$Time$toMillis" time_to_millis, 2;

    G_PLATFORM_WORKER "$Platform$worker" platform_worker, 1;
    G_PLATFORM_CMD_BATCH "$Platform$Cmd$batch" cmd_batch, 1;
    G_PLATFORM_CMD_MAP "$Platform$Cmd$map" cmd_map, 2;
    G_PLATFORM_SUB_BATCH "$Platform$Sub$batch" sub_batch, 1;
    G_PLATFORM_SUB_MAP "$Platform$Sub$map" sub_map, 2;

    G_TERMINAL_WRITELINE "$Terminal$writeLine" terminal_write_line, 1;
    G_BASICS_COS "$Basics$cos" basics_cos, 1;
    G_BASICS_SIN "$Basics$sin" basics_sin, 1;
    G_BASICS_TAN "$Basics$tan" basics_tan, 1;
    G_BASICS_ACOS "$Basics$acos" basics_acos, 1;
    G_BASICS_ASIN "$Basics$asin" basics_asin, 1;
    G_BASICS_ATAN "$Basics$atan" basics_atan, 1;
    G_BASICS_ATAN2 "$Basics$atan2" basics_atan2, 2;
    G_BASICS_DEGREES "$Basics$degrees" basics_degrees, 1;
    G_BASICS_RADIANS "$Basics$radians" basics_radians, 1;
    G_BASICS_TURNS "$Basics$turns" basics_turns, 1;
    G_BASICS_FROMPOLAR "$Basics$fromPolar" basics_from_polar, 1;
    G_BASICS_TOPOLAR "$Basics$toPolar" basics_to_polar, 1;
    G_BASICS_ISNAN "$Basics$isNaN" basics_is_nan, 1;
    G_BASICS_ISINF "$Basics$isInfinite" basics_is_infinite, 1;
    G_BITWISE_AND "$Bitwise$and" bitwise_and, 2;
    G_BITWISE_OR "$Bitwise$or" bitwise_or, 2;
    G_BITWISE_XOR "$Bitwise$xor" bitwise_xor, 2;
    G_BITWISE_COMPLEMENT "$Bitwise$complement" bitwise_complement, 1;
    G_BITWISE_SHL "$Bitwise$shiftLeftBy" bitwise_shift_left_by, 2;
    G_BITWISE_SHR "$Bitwise$shiftRightBy" bitwise_shift_right_by, 2;
    G_BITWISE_SHRZF "$Bitwise$shiftRightZfBy" bitwise_shift_right_zf_by, 2;
    G_CHAR_ISALPHANUM "$Char$isAlphaNum" char_is_alpha_num, 1;
    G_CHAR_ISHEXDIGIT "$Char$isHexDigit" char_is_hex_digit, 1;
    G_LIST_MAP3 "$List$map3" list_map3, 4;
    G_LIST_MAP4 "$List$map4" list_map4, 5;
    G_LIST_MAP5 "$List$map5" list_map5, 6;
    G_MAYBE_MAP3 "$Maybe$map3" maybe_map3, 4;
    G_MAYBE_MAP4 "$Maybe$map4" maybe_map4, 5;
    G_MAYBE_MAP5 "$Maybe$map5" maybe_map5, 6;
    G_RESULT_MAP2 "$Result$map2" result_map2, 3;
    G_RESULT_MAP3 "$Result$map3" result_map3, 4;
    G_RESULT_MAP4 "$Result$map4" result_map4, 5;
    G_RESULT_MAP5 "$Result$map5" result_map5, 6;
    G_STRING_FOLDL "$String$foldl" string_foldl, 3;
    G_STRING_FOLDR "$String$foldr" string_foldr, 3;
    G_SET_PARTITION "$Set$partition" set_partition, 2;
    G_ARRAY_APPEND "$Array$append" array_append, 2;
    G_ARRAY_TOINDEXEDLIST "$Array$toIndexedList" array_to_indexed_list, 1;
    G_DICT_SINGLETON "$Dict$singleton" dict_singleton, 2;
    G_DICT_INSERT "$Dict$insert" dict_insert, 3;
    G_DICT_GET "$Dict$get" dict_get, 2;
    G_DICT_REMOVE "$Dict$remove" dict_remove, 2;
    G_DICT_MEMBER "$Dict$member" dict_member, 2;
    G_DICT_ISEMPTY "$Dict$isEmpty" dict_is_empty, 1;
    G_DICT_SIZE "$Dict$size" dict_size, 1;
    G_DICT_KEYS "$Dict$keys" dict_keys, 1;
    G_DICT_VALUES "$Dict$values" dict_values, 1;
    G_DICT_TOLIST "$Dict$toList" dict_to_list, 1;
    G_DICT_FROMLIST "$Dict$fromList" dict_from_list, 1;
    G_DICT_FOLDL "$Dict$foldl" dict_foldl, 3;
    G_DICT_FOLDR "$Dict$foldr" dict_foldr, 3;
    G_DICT_MAP "$Dict$map" dict_map, 2;
    G_DICT_FILTER "$Dict$filter" dict_filter, 2;
    G_DICT_UPDATE "$Dict$update" dict_update, 3;
    G_DICT_UNION "$Dict$union" dict_union, 2;
    G_DICT_INTERSECT "$Dict$intersect" dict_intersect, 2;
    G_DICT_DIFF "$Dict$diff" dict_diff, 2;
    G_DICT_PARTITION "$Dict$partition" dict_partition, 2;
    G_DICT_MERGE "$Dict$merge" dict_merge, 6;
    G_SET_SINGLETON "$Set$singleton" set_singleton, 1;
    G_SET_INSERT "$Set$insert" set_insert, 2;
    G_SET_REMOVE "$Set$remove" set_remove, 2;
    G_SET_MEMBER "$Set$member" set_member, 2;
    G_SET_ISEMPTY "$Set$isEmpty" set_is_empty, 1;
    G_SET_SIZE "$Set$size" set_size, 1;
    G_SET_TOLIST "$Set$toList" set_to_list, 1;
    G_SET_FROMLIST "$Set$fromList" set_from_list, 1;
    G_SET_UNION "$Set$union" set_union, 2;
    G_SET_INTERSECT "$Set$intersect" set_intersect, 2;
    G_SET_DIFF "$Set$diff" set_diff, 2;
    G_SET_FOLDL "$Set$foldl" set_foldl, 3;
    G_SET_FOLDR "$Set$foldr" set_foldr, 3;
    G_SET_MAP "$Set$map" set_map, 2;
    G_SET_FILTER "$Set$filter" set_filter, 2;
    G_ARRAY_ISEMPTY "$Array$isEmpty" array_is_empty, 1;
    G_ARRAY_LENGTH "$Array$length" array_length, 1;
    G_ARRAY_INITIALIZE "$Array$initialize" array_initialize, 2;
    G_ARRAY_REPEAT "$Array$repeat" array_repeat, 2;
    G_ARRAY_FROMLIST "$Array$fromList" array_from_list, 1;
    G_ARRAY_TOLIST "$Array$toList" array_to_list, 1;
    G_ARRAY_GET "$Array$get" array_get, 2;
    G_ARRAY_SET "$Array$set" array_set, 3;
    G_ARRAY_PUSH "$Array$push" array_push, 2;
    G_ARRAY_FOLDL "$Array$foldl" array_foldl, 3;
    G_ARRAY_FOLDR "$Array$foldr" array_foldr, 3;
    G_ARRAY_MAP "$Array$map" array_map, 2;
    G_ARRAY_INDEXEDMAP "$Array$indexedMap" array_indexed_map, 2;
    G_ARRAY_FILTER "$Array$filter" array_filter, 2;
    G_ARRAY_SLICE "$Array$slice" array_slice, 3;

    G_JSOND_NULL "$Json$Decode$null" json_null, 1;
    G_JSOND_SUCCEED "$Json$Decode$succeed" json_succeed, 1;
    G_JSOND_FAIL "$Json$Decode$fail" json_fail, 1;
    G_JSOND_FIELD "$Json$Decode$field" json_field, 2;
    G_JSOND_AT "$Json$Decode$at" json_at, 2;
    G_JSOND_INDEX "$Json$Decode$index" json_index, 2;
    G_JSOND_LIST "$Json$Decode$list" json_list_dec, 1;
    G_JSOND_ARRAY "$Json$Decode$array" json_array_dec, 1;
    G_JSOND_KEYVALUEPAIRS "$Json$Decode$keyValuePairs" json_key_value_pairs, 1;
    G_JSOND_DICT "$Json$Decode$dict" json_dict_dec, 1;
    G_JSOND_MAYBE "$Json$Decode$maybe" json_maybe, 1;
    G_JSOND_NULLABLE "$Json$Decode$nullable" json_nullable, 1;
    G_JSOND_ONEOF "$Json$Decode$oneOf" json_one_of, 1;
    G_JSOND_ONEORMORE "$Json$Decode$oneOrMore" json_one_or_more, 2;
    G_JSOND_LAZY "$Json$Decode$lazy" json_lazy, 1;
    G_JSOND_MAP "$Json$Decode$map" json_map, 2;
    G_JSOND_MAP2 "$Json$Decode$map2" json_map2, 3;
    G_JSOND_MAP3 "$Json$Decode$map3" json_map3, 4;
    G_JSOND_MAP4 "$Json$Decode$map4" json_map4, 5;
    G_JSOND_MAP5 "$Json$Decode$map5" json_map5, 6;
    G_JSOND_MAP6 "$Json$Decode$map6" json_map6, 7;
    G_JSOND_MAP7 "$Json$Decode$map7" json_map7, 8;
    G_JSOND_MAP8 "$Json$Decode$map8" json_map8, 9;
    G_JSOND_ANDTHEN "$Json$Decode$andThen" json_and_then, 2;
    G_JSOND_DECODEVALUE "$Json$Decode$decodeValue" json_decode_value, 2;
    G_JSOND_DECODESTRING "$Json$Decode$decodeString" json_decode_string, 2;
    G_JSOND_ERRORTOSTRING "$Json$Decode$errorToString" json_error_to_string, 1;

    G_JSONE_STRING "$Json$Encode$string" encode_string, 1;
    G_JSONE_INT "$Json$Encode$int" encode_int, 1;
    G_JSONE_FLOAT "$Json$Encode$float" encode_float, 1;
    G_JSONE_BOOL "$Json$Encode$bool" encode_bool, 1;
    G_JSONE_LIST "$Json$Encode$list" encode_list, 2;
    G_JSONE_ARRAY "$Json$Encode$array" encode_array, 2;
    G_JSONE_SET "$Json$Encode$set" encode_set, 2;
    G_JSONE_OBJECT "$Json$Encode$object" encode_object, 1;
    G_JSONE_DICT "$Json$Encode$dict" encode_dict, 3;
    G_JSONE_ENCODE "$Json$Encode$encode" encode_encode, 2;

    G_PARSER_ISSUBSTRING "$Elm$Kernel$Parser$isSubString" parser_is_sub_string, 5;
    G_PARSER_ISSUBCHAR "$Elm$Kernel$Parser$isSubChar" parser_is_sub_char, 3;
    G_PARSER_ISASCIICODE "$Elm$Kernel$Parser$isAsciiCode" parser_is_ascii_code, 3;
    G_PARSER_CHOMPBASE10 "$Elm$Kernel$Parser$chompBase10" parser_chomp_base10, 2;
    G_PARSER_CONSUMEBASE "$Elm$Kernel$Parser$consumeBase" parser_consume_base, 3;
    G_PARSER_CONSUMEBASE16 "$Elm$Kernel$Parser$consumeBase16" parser_consume_base16, 2;
    G_PARSER_FINDSUBSTRING "$Elm$Kernel$Parser$findSubString" parser_find_sub_string, 5;
}

kernel_vals! {
    G_BASICS_PI "$Basics$pi";
    G_BASICS_E "$Basics$e";
    G_TIME_NOW "$Time$now";
    G_TIME_UTC "$Time$utc";
    G_TIME_HERE "$Time$here";
    G_PLATFORM_CMD_NONE "$Platform$Cmd$none";
    G_PLATFORM_SUB_NONE "$Platform$Sub$none";
    G_DICT_EMPTY "$Dict$empty";
    G_SET_EMPTY "$Set$empty";
    G_ARRAY_EMPTY "$Array$empty";
    G_RANDOM_MININT "$Random$minInt";
    G_RANDOM_MAXINT "$Random$maxInt";
    G_RANDOM_INDEPENDENTSEED "$Random$independentSeed";
    G_JSOND_STRING "$Json$Decode$string";
    G_JSOND_INT "$Json$Decode$int";
    G_JSOND_FLOAT "$Json$Decode$float";
    G_JSOND_BOOL "$Json$Decode$bool";
    G_JSOND_VALUE "$Json$Decode$value";
    G_JSONE_NULL "$Json$Encode$null";
}

unsafe fn runtime_init() {
    RT_TRUE.set(alloc(Value::Bool(true)));
    RT_FALSE.set(alloc(Value::Bool(false)));
    RT_UNIT.set(alloc(Value::Unit));
    NIL.set(alloc(Value::List {
        backing: alloc_backing(Vec::new()),
        len: 0,
    }));
    NOTHING.set(ctor(b"Nothing\0".as_ptr(), 1, Vec::new()));
    LT.set(ctor(b"LT\0".as_ptr(), 0, Vec::new()));
    EQ.set(ctor(b"EQ\0".as_ptr(), 1, Vec::new()));
    GT.set(ctor(b"GT\0".as_ptr(), 2, Vec::new()));

    init_kernel_fns();

    G_BASICS_PI.set(rt_float(std::f64::consts::PI));
    G_BASICS_E.set(rt_float(std::f64::consts::E));
    G_TIME_NOW.set(ctor(b"TaskNow\0".as_ptr(), TT_NOW, Vec::new()));
    let utc = ctor(b"Zone\0".as_ptr(), 0, vec![rt_int(0), nil()]);
    G_TIME_UTC.set(utc);
    G_TIME_HERE.set(task_succeed(utc));
    G_PLATFORM_CMD_NONE.set(ctor(b"CmdNone\0".as_ptr(), CT_NONE, Vec::new()));
    G_PLATFORM_SUB_NONE.set(ctor(b"SubNone\0".as_ptr(), ST_NONE, Vec::new()));
    G_DICT_EMPTY.set(alloc(Value::Dict(Vec::new())));
    G_SET_EMPTY.set(alloc(Value::Set(Vec::new())));
    G_ARRAY_EMPTY.set(alloc(Value::Array(Vec::new())));
    G_RANDOM_MININT.set(rt_int(-2147483648));
    G_RANDOM_MAXINT.set(rt_int(2147483647));
    G_RANDOM_INDEPENDENTSEED
        .set(mk_generator(closure(random_independent_seed_gen as *const (), 1, &[])));
    G_JSOND_STRING.set(mk_decoder(Decoder::Str));
    G_JSOND_INT.set(mk_decoder(Decoder::Int));
    G_JSOND_FLOAT.set(mk_decoder(Decoder::Float));
    G_JSOND_BOOL.set(mk_decoder(Decoder::Bool));
    G_JSOND_VALUE.set(mk_decoder(Decoder::JsonVal));
    G_JSONE_NULL.set(mk_json(JsonValue::Null));
}

// THE EVENT LOOP — the Rust twin of runtime.js's _Platform_initialize for
// `Platform.worker` programs. Tasks are data interpreted by `run_task`
// with an explicit continuation-frame stack; the loop exits when nothing
// is pending (the same reason a node process exits). Single-threaded.

struct Frame {
    kind: u8, // 0 = andThen, 1 = onError
    f: u64,
}

struct Pending {
    fire_at: f64,
    task: u64,
    frames: Vec<Frame>,
    tagger: u64,
}

struct SubTimer {
    fire_at: f64,
    interval: f64,
    to_msg: u64,
    tagger: u64,
}

static mut TEA_MODEL: u64 = 0u64;
static mut TEA_UPDATE: u64 = 0u64;
static mut TEA_SUBSCRIPTIONS: u64 = 0u64;
static mut PENDING: Vec<Pending> = Vec::new();
static mut TIMERS: Vec<SubTimer> = Vec::new();

fn now_ms() -> f64 {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    d.as_secs_f64() * 1000.0
}

fn out_line(bytes: &[u8]) {
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    let _ = handle.write_all(bytes);
    let _ = handle.write_all(b"\n");
    let _ = handle.flush();
}

unsafe extern "C" fn tagger_compose_step(outer: u64, f: u64, m: u64) -> u64 {
    ap1(outer, ap1(f, m))
}
unsafe fn tagger_compose(outer: u64, f: u64) -> u64 {
    closure(tagger_compose_step as *const (), 3, &[outer, f])
}
unsafe fn identity() -> u64 {
    G_BASICS_IDENTITY.get()
}

unsafe fn run_task(mut task: u64, mut frames: Vec<Frame>, tagger: u64) {
    loop {
        match ctor_index(task) {
            TT_SUCCEED => {
                let v = rt_ctor_arg(task, 0);
                while frames.last().is_some_and(|f| f.kind != 0) {
                    frames.pop();
                }
                match frames.pop() {
                    None => {
                        tea_dispatch(ap1(tagger, v));
                        return;
                    }
                    Some(frame) => task = ap1(frame.f, v),
                }
            }
            TT_FAIL => {
                let e = rt_ctor_arg(task, 0);
                while frames.last().is_some_and(|f| f.kind != 1) {
                    frames.pop();
                }
                match frames.pop() {
                    None => {
                        let rendered = debug_to_string(e);
                        let mut line = b"Task failed without an error handler: ".to_vec();
                        line.extend_from_slice(sbytes(rendered));
                        line.push(0);
                        rt_crash(line.as_ptr());
                    }
                    Some(frame) => task = ap1(frame.f, e),
                }
            }
            TT_AND_THEN => {
                frames.push(Frame {
                    kind: 0,
                    f: rt_ctor_arg(task, 0),
                });
                task = rt_ctor_arg(task, 1);
            }
            TT_ON_ERROR => {
                frames.push(Frame {
                    kind: 1,
                    f: rt_ctor_arg(task, 0),
                });
                task = rt_ctor_arg(task, 1);
            }
            TT_SLEEP => {
                let ms = num(rt_ctor_arg(task, 0));
                PENDING.push(Pending {
                    fire_at: now_ms() + ms,
                    task: task_succeed(unit()),
                    frames,
                    tagger,
                });
                return;
            }
            TT_NOW => task = task_succeed(time_posix(now_ms())),
            _ => crash!("unknown task"),
        }
    }
}

unsafe fn run_cmd(cmd: u64, tagger: u64) {
    match ctor_index(cmd) {
        CT_NONE => {}
        CT_BATCH => {
            for c in to_vec(rt_ctor_arg(cmd, 0)) {
                run_cmd(c, tagger);
            }
        }
        CT_MAP => run_cmd(rt_ctor_arg(cmd, 1), tagger_compose(tagger, rt_ctor_arg(cmd, 0))),
        CT_TASK => run_task(rt_ctor_arg(cmd, 0), Vec::new(), tagger),
        CT_WRITE => out_line(sbytes(rt_ctor_arg(cmd, 0))),
        _ => crash!("unknown command"),
    }
}

unsafe fn collect_subs(sub: u64, tagger: u64) {
    match ctor_index(sub) {
        ST_NONE => {}
        ST_BATCH => {
            for s in to_vec(rt_ctor_arg(sub, 0)) {
                collect_subs(s, tagger);
            }
        }
        ST_MAP => collect_subs(rt_ctor_arg(sub, 1), tagger_compose(tagger, rt_ctor_arg(sub, 0))),
        ST_TIME => {
            let interval = num(rt_ctor_arg(sub, 0));
            TIMERS.push(SubTimer {
                fire_at: now_ms() + interval,
                interval,
                to_msg: rt_ctor_arg(sub, 1),
                tagger,
            });
        }
        _ => crash!("unknown subscription"),
    }
}

/// Re-collect subscriptions after every dispatch (this also restarts the
/// interval timers), matching the JS runtime.
unsafe fn update_subs() {
    TIMERS.clear();
    collect_subs(ap1(TEA_SUBSCRIPTIONS, TEA_MODEL), identity());
}

unsafe fn tea_dispatch(msg: u64) {
    let next = ap2(TEA_UPDATE, msg, TEA_MODEL);
    TEA_MODEL = rt_tuple_item(next, 0);
    run_cmd(rt_tuple_item(next, 1), identity());
    update_subs();
}

unsafe fn tea_run(impl_: u64) {
    TEA_UPDATE = rt_access(impl_, b"update\0".as_ptr());
    TEA_SUBSCRIPTIONS = rt_access(impl_, b"subscriptions\0".as_ptr());
    let init = ap1(rt_access(impl_, b"init\0".as_ptr()), unit());
    TEA_MODEL = rt_tuple_item(init, 0);
    run_cmd(rt_tuple_item(init, 1), identity());
    update_subs();

    loop {
        let now = now_ms();

        // Fire one due pending task, then rescan (a dispatch rebuilds the
        // timers).
        if let Some(i) = PENDING.iter().position(|p| p.fire_at <= now) {
            let p = PENDING.remove(i);
            run_task(p.task, p.frames, p.tagger);
            continue;
        }
        if let Some(i) = TIMERS.iter().position(|t| t.fire_at <= now) {
            let (to_msg, tagger) = (TIMERS[i].to_msg, TIMERS[i].tagger);
            tea_dispatch(ap1(tagger, ap1(to_msg, time_posix(now))));
            continue;
        }

        // Nothing due: sleep until the earliest deadline, or exit when
        // nothing is pending at all.
        let mut next = f64::INFINITY;
        for p in PENDING.iter() {
            next = next.min(p.fire_at);
        }
        for t in TIMERS.iter() {
            next = next.min(t.fire_at);
        }
        if next.is_infinite() {
            return;
        }
        let wait = next - now_ms();
        if wait > 0.0 {
            std::thread::sleep(std::time::Duration::from_secs_f64(wait / 1000.0));
        }
    }
}

// ENTRY — initialize the runtime and the program's globals, then either
// run a `Platform.worker` program or print the entry module's `main`.

extern "C" {
    fn alm_init();
    fn alm_main() -> u64;
}

// Entry point. The host C runtime calls `main`; WASI's crt instead calls
// `__main_argc_argv` (WebAssembly checks signatures strictly, so we provide
// the exact symbol WASI expects rather than a plain `main`). Both just run
// the shared body.
#[cfg(not(target_arch = "wasm32"))]
#[no_mangle]
pub unsafe extern "C" fn main(_argc: i32, _argv: *const *const u8) -> i32 {
    alm_entry()
}

#[cfg(target_arch = "wasm32")]
#[no_mangle]
pub unsafe extern "C" fn __main_argc_argv(_argc: i32, _argv: *const *const u8) -> i32 {
    alm_entry()
}

unsafe fn alm_entry() -> i32 {
    runtime_init();
    alm_init();
    let v = alm_main();
    if v == 0 {
        eprintln!("alm: this program has no main");
        return 1;
    }
    if is_int(v) {
        out_line(int_val(v).to_string().as_bytes());
        return 0;
    }
    match deref(v) {
        Value::Ctor { name, .. } if cname(*name) == "Program" => {
            tea_run(rt_ctor_arg(v, 0));
            0
        }
        Value::Str(bytes) => {
            out_line(bytes);
            0
        }
        Value::Float(f) => {
            out_line(fmt_float(*f).as_bytes());
            0
        }
        _ => {
            eprintln!("alm: main is not a printable value");
            1
        }
    }
}
