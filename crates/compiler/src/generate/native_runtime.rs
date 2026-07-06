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
        _ => crash!("function arity too large (max 12 for now)"),
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
        let mut all: [u64; 16] = [0u64; 16];
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
    let mut all: [u64; 16] = [0u64; 16];
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
    let mut all: [u64; 16] = [0u64; 16];
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

fn ascii_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0c' | '\x0b')
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
    let digits = &bytes[i..];
    // Reject empty, and leading zeros (matching the JS round-trip rule).
    if digits.is_empty() || (digits.len() > 1 && digits[0] == b'0') {
        return nothing();
    }
    let mut n: i64 = 0;
    while i < bytes.len() {
        let d = bytes[i];
        if !d.is_ascii_digit() {
            return nothing();
        }
        n = n * 10 + (d - b'0') as i64;
        i += 1;
    }
    if negative && n == 0 {
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
    mkstr(sstr(s).trim_matches(ascii_space).as_bytes().to_vec())
}
unsafe extern "C" fn string_trim_left(s: u64) -> u64 {
    mkstr(sstr(s).trim_start_matches(ascii_space).as_bytes().to_vec())
}
unsafe extern "C" fn string_trim_right(s: u64) -> u64 {
    mkstr(sstr(s).trim_end_matches(ascii_space).as_bytes().to_vec())
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

fn debug_string(out: &mut String, bytes: &[u8]) {
    // Elm strings are UTF-8; render them as characters (not per-byte, which
    // would re-encode multi-byte scalars) so non-ASCII prints faithfully.
    let text = unsafe { std::str::from_utf8_unchecked(bytes) };
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
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
        // The JS runtime represents chars as strings, so they format like
        // one-character strings.
        Value::Char(c) => {
            let s = char::from_u32(*c).unwrap_or('\u{fffd}').to_string();
            debug_string(out, s.as_bytes());
        }
        Value::Str(b) => debug_string(out, b),
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
            for (i, &(name, value)) in fields.iter().enumerate() {
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
