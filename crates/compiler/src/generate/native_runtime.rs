//! alm native runtime — the Rust twin of the JS backend's `runtime.js`.
//!
//! This is a standalone file compiled by `build.rs` into a static library
//! (`libalm_runtime.a`) and linked into every native binary the compiler
//! produces. It is NOT a module of the compiler crate.
//!
//! Every Elm value is a boxed [`Value`] behind a raw pointer, matching the
//! uniform representation the LLVM codegen assumes. Values are immutable
//! with ONE exception: `sbytes` flattens a `StrCat` rope node into a plain
//! `Str` in place, exactly once — the write-once discipline its borrow
//! soundness depends on. All the entry points the generated code calls are
//! `extern "C"` with the exact signatures declared in `generate::native`.
//! Memory is managed by the Boehm conservative GC (see below).
//!
//! Compiled with `panic = abort`, so a Rust panic never unwinds across the
//! C ABI boundary into generated code.

#![allow(non_upper_case_globals, non_snake_case, static_mut_refs)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::UnsafeCell;
use std::ffi::CStr;
use std::io::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

// ALLOCATOR
//
// Native: the Boehm–Demers–Weiser conservative garbage collector (`libgc`).
// The runtime holds Elm values as raw `u64` pointers with no ownership tracking
// and reads them through fabricated lifetimes, so a precise collector would need
// codegen-registered roots. A *conservative* GC needs none of that: it scans the
// stack, registers, and statics treating any word that looks like a heap pointer
// as a root, so it reclaims exactly the unreachable values and — because a value
// stays live as long as any pointer to it is reachable — it also keeps the
// runtime's raw-pointer reads valid. Elm ints are tagged (`w & 1`, i.e. odd), so
// they are never mistaken for pointers and cause no false retention.
//
// `GC_malloc` returns 16-aligned, zeroed memory; every Elm allocation has align
// ≤ 16, and `GC_memalign` covers the rare larger request. `dealloc` is a no-op —
// the collector reclaims. The collector is initialised lazily on first
// allocation (on whatever thread starts the program, which becomes GC-primary),
// and the large-stack worker thread registers itself in `main`.
//
// Wasm: `libgc` is not linked there, so wasm keeps the leak-only bump allocator.

struct Bump;

#[cfg(not(target_arch = "wasm32"))]
#[repr(C)]
struct GcStackBase {
    mem_base: *mut u8,
    // `struct GC_stack_base` is a single pointer on arm64/x86-64; an extra word
    // over-allocates so `GC_get_stack_base` can never write past the struct.
    _reg_base: *mut u8,
}

#[cfg(not(target_arch = "wasm32"))]
extern "C" {
    fn GC_init();
    fn GC_malloc(size: usize) -> *mut u8;
    /// One allocation-lock acquisition returns a whole chain of cleared,
    /// pointer-scanned blocks of the given size, linked through each block's
    /// first word. The backbone of the `alloc` cell pool below.
    fn GC_malloc_many(size: usize) -> *mut u8;
    fn GC_expand_hp(bytes: usize) -> i32;
    fn GC_set_free_space_divisor(d: usize);
    fn GC_memalign(align: usize, size: usize) -> *mut u8;
    fn GC_realloc(p: *mut u8, size: usize) -> *mut u8;
    fn GC_allow_register_threads();
    fn GC_get_stack_base(sb: *mut GcStackBase) -> i32;
    fn GC_register_my_thread(sb: *const GcStackBase) -> i32;
    fn GC_unregister_my_thread() -> i32;
}

#[cfg(not(target_arch = "wasm32"))]
static mut GC_READY: bool = false;

#[cfg(not(target_arch = "wasm32"))]
#[inline]
unsafe fn gc_ensure_init() {
    if !GC_READY {
        GC_READY = true;
        // Divisor 6 (default 3): collect harder before growing the heap —
        // set BEFORE GC_init so the very first growth decisions use it.
        GC_set_free_space_divisor(6);
        FLAG_ARG_CHECK = std::env::var("ALM_ARG_CHECK").is_ok();
        FLAG_NO_POOL = std::env::var("ALM_NO_POOL").is_ok();
        GC_init();
        GC_allow_register_threads();
        // Start with a roomy heap. Elm code allocates in torrents (every
        // cons cell and boxed value); growing from Boehm's tiny default
        // means near-continuous full collections — marking dominated whole
        // workloads. 64MB keeps growth proportional to the live set (a
        // bigger floor overshot the doubling policy to 2.3GB on
        // elm-monocle); divisor 6 halves churn-heavy peaks
        // (base64-bytes 1.6GB → <1GB) at no measurable time cost.
        GC_expand_hp(64 << 20);
    }
}

// Wasm-only leak-everything bump allocator.
#[cfg(target_arch = "wasm32")]
static mut BUMP_CUR: usize = 0;
#[cfg(target_arch = "wasm32")]
static mut BUMP_END: usize = 0;
#[cfg(target_arch = "wasm32")]
const BUMP_CHUNK: usize = 64 << 20; // 64 MiB

unsafe impl GlobalAlloc for Bump {
    #[cfg(not(target_arch = "wasm32"))]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        gc_ensure_init();
        let size = layout.size().max(1);
        if layout.align() > 16 {
            GC_memalign(layout.align(), size)
        } else {
            GC_malloc(size)
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    unsafe fn realloc(&self, ptr: *mut u8, _layout: Layout, new_size: usize) -> *mut u8 {
        GC_realloc(ptr, new_size)
    }

    #[cfg(target_arch = "wasm32")]
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
        // Native: the GC reclaims. Wasm: leak. Either way, nothing to do.
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
    /// A lazy string concatenation (rope node): `a ++ b` in O(1). `len` is
    /// the total byte length. The first read (via `sbytes`) flattens the
    /// tree into a plain `Str` IN PLACE, so repeated `acc ++ x` string
    /// building is amortized O(total) instead of O(total²) — matching V8's
    /// rope strings, which is what the JS backend leans on.
    StrCat { left: u64, right: u64, len: usize },
    /// A substring view: `off..off + len` bytes into `base` (a `Str` or a
    /// rope that flattens on first read). O(1) `String.slice`/`dropLeft`/
    /// `uncons` — matching V8's sliced strings, which is what makes
    /// char-by-char string recursion linear on the JS backend.
    StrSlice { base: u64, off: usize, len: usize },
    /// A non-empty list: a cons cell, exactly like the JS backend. `tail`
    /// is another `List` cell or `Nil`. Cells make `::`, `head` and `tail`
    /// all O(1) with no copying — the previous contiguous-backing scheme
    /// copied O(n) on every `cons` onto a non-tip view (e.g. after `tail`),
    /// which made lazy-list traversal (elm-test fuzzers, byte decoding)
    /// quadratic, and allocated a fresh header per `tail`.
    List {
        head: u64,
        tail: u64,
    },
    /// The empty list. A single shared instance lives in `NIL`.
    Nil,
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
    /// A dictionary: the root of a persistent weight-balanced tree keyed by
    /// `value_cmp` (0 = empty). Immutable; updates path-copy O(log n) nodes and
    /// share the rest, so building N entries in a loop is O(N log N), not O(N²).
    Dict(u64),
    /// A set: the root of a persistent weight-balanced tree (0 = empty); the
    /// node's value slot is unused.
    Set(u64),
    /// A persistent array: the root of a positional weight-balanced tree
    /// (0 = empty), in-order = index order. get/set/push are O(log n) with
    /// structural sharing, so `Array.set`/`push` in a loop no longer copies an
    /// O(n) backing each time.
    Array(u64),
    /// An `elm/bytes` `Bytes` value: an immutable byte buffer. Mirrors the JS
    /// runtime's `DataView`; `encode` fills one, `read_*` read from it.
    Bytes(Vec<u8>),
    /// A linear-algebra vector or matrix (elm-explorations/linear-algebra):
    /// the native twin of the JS kernel's Float64Array, len 2/3/4/16.
    Floats(Vec<f64>),
    /// A `Json.Encode.Value` / `Json.Decode.Value` — an opaque JSON tree. In
    /// the JS runtime these are raw JS values; natively they are this tree.
    Json(JsonValue),
    /// A `Json.Decode.Decoder a` — reified as data and run by `run_decoder`,
    /// avoiding a closure per combinator. Sub-decoders and functions are held
    /// as uniform value words.
    Decoder(Decoder),
    /// A compiled `Regex.Regex` — an opaque pointer into the `alm-regex` glue
    /// (`fancy-regex`). Never freed (bump-allocator model).
    Regex(*const core::ffi::c_void),
}

// Native: the elm/regex engine lives in the `alm-regex` glue, linked into each
// program. On wasm that glue is not linked, so provide aborting/empty stubs to
// keep the wasm runtime self-contained — `fromStringWith` there yields `null`,
// so regex degrades to "never compiles" (unsupported on wasm for now).
#[cfg(not(target_arch = "wasm32"))]
extern "C" {
    fn alm_rx_compile(pat: *const u8, plen: usize, ci: bool, ml: bool) -> *const core::ffi::c_void;
    fn alm_rx_contains(re: *const core::ffi::c_void, txt: *const u8, tlen: usize) -> i32;
    fn alm_rx_find(
        re: *const core::ffi::c_void,
        txt: *const u8,
        tlen: usize,
        limit: i64,
        out_len: *mut usize,
    ) -> *mut i64;
    fn alm_rx_split(
        re: *const core::ffi::c_void,
        txt: *const u8,
        tlen: usize,
        limit: i64,
        out_len: *mut usize,
    ) -> *mut i64;
    fn alm_rx_free(ptr: *mut i64, len: usize);
}

#[cfg(target_arch = "wasm32")]
unsafe fn alm_rx_compile(_: *const u8, _: usize, _: bool, _: bool) -> *const core::ffi::c_void {
    core::ptr::null()
}
#[cfg(target_arch = "wasm32")]
unsafe fn alm_rx_contains(_: *const core::ffi::c_void, _: *const u8, _: usize) -> i32 {
    0
}
#[cfg(target_arch = "wasm32")]
unsafe fn alm_rx_find(
    _: *const core::ffi::c_void,
    _: *const u8,
    _: usize,
    _: i64,
    out_len: *mut usize,
) -> *mut i64 {
    *out_len = 0;
    core::ptr::null_mut()
}
#[cfg(target_arch = "wasm32")]
unsafe fn alm_rx_split(
    re: *const core::ffi::c_void,
    txt: *const u8,
    tlen: usize,
    limit: i64,
    out_len: *mut usize,
) -> *mut i64 {
    let _ = (re, txt, tlen, limit);
    *out_len = 0;
    core::ptr::null_mut()
}
#[cfg(target_arch = "wasm32")]
unsafe fn alm_rx_free(_: *mut i64, _: usize) {}

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
    /// An opaque embedded Elm value word (a closure, decoder, or other value)
    /// carried inside a JSON tree. Not producible by JSON parsing or encoding —
    /// used only by `HtmlAsJson`, which reflects the virtual dom (with its live
    /// event decoders and `map` taggers) into the JSON shape Test.Html decodes.
    Elm(u64),
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

/// Head of a free chain of `Value`-sized blocks from `GC_malloc_many`,
/// linked through each free block's first word. The mutator is single-
/// threaded (all Elm code runs on the one big-stack runner thread), so a
/// plain static suffices. The static is a scanned root and the link words
/// live inside scanned blocks, so the pooled blocks are never reclaimed
/// out from under us; handing one out overwrites the link word.
#[cfg(not(target_arch = "wasm32"))]
static mut CELL_POOL: *mut u8 = std::ptr::null_mut();

/// Diagnostic env flags, read ONCE at collector init (single-threaded —
/// getenv per call took a libc lock inside the hottest paths, and a lazy
/// OnceLock here once deadlocked the parallel test suite).
static mut FLAG_ARG_CHECK: bool = false;
static mut FLAG_NO_POOL: bool = false;

/// Allocate a value on the heap and return it as a value word.
///
/// Values are the hottest allocation in the uniform backend (every cons
/// cell, tuple and constructor), and `GC_malloc` takes the collector's
/// allocation lock on every call. `GC_malloc_many` amortizes that: one
/// locked call refills a local chain of `Value`-sized blocks that we carve
/// off lock-free.
///
/// (An `inline(never)` here was once thought load-bearing for the
/// collector's root scan; the real culprit was the runtime-bitcode merge,
/// now disabled — see `native.rs`. With every rt_* call behind a C-ABI
/// boundary the attribute is moot, but the merge experiment flag can
/// reintroduce cross-inlining, so it stays as cheap insurance.)
#[cfg(not(target_arch = "wasm32"))]
#[inline(never)]
fn alloc(value: Value) -> u64 {
    unsafe {
        // DIAG: ALM_NO_POOL=1 falls back to plain Box (GC_malloc).
        if *std::ptr::addr_of!(FLAG_NO_POOL) {
            return Box::into_raw(Box::new(value)) as u64;
        }
        let mut p = *std::ptr::addr_of!(CELL_POOL);
        if p.is_null() {
            gc_ensure_init();
            p = GC_malloc_many(std::mem::size_of::<Value>());
            if p.is_null() {
                // Refill failed (heap pressure): fall back to a plain boxed
                // allocation rather than crashing here.
                return Box::into_raw(Box::new(value)) as u64;
            }
        }
        *std::ptr::addr_of_mut!(CELL_POOL) = *(p as *mut *mut u8);
        std::ptr::write(p as *mut Value, value);
        p as u64
    }
}

#[cfg(target_arch = "wasm32")]
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
// sharing is sound without refcounting. (The uniform backend used the same
// scheme before it switched to cons cells; this typed variant remains.)

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
    // Copy path: build a fresh backing holding the caller's `len` elements plus
    // the new head. Size it by `len` (amortized doubling), NOT the source
    // backing's capacity: consing onto a short view of a large shared backing
    // (e.g. `[] = tail [x]`, whose backing has capacity from the tip list) must
    // not inherit — and repeatedly double — that unrelated capacity, which grew
    // it exponentially (1→4→8→…→2^40) until the allocation failed.
    let new_cap = ((len + 1) * 2).max(4);
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
    // A polymorphic number literal in a float position can be boxed as an
    // immediate `Int` (its `number` type defaulted to Int) yet consumed as a
    // `Float` — e.g. `(x, y, 0)` typed `(Float, Float, Float)` at the use site.
    // Coerce it (elm treats all numbers as f64) rather than dereferencing the
    // tagged immediate as a pointer, which segfaults.
    if is_int(w) {
        return int_val(w) as f64;
    }
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

/// The variant of a value word, for crash messages: a kernel that finds the
/// wrong variant should say what it FOUND, not just what it wanted.
unsafe fn variant_name(w: u64) -> &'static str {
    if w == 0 {
        return "null";
    }
    if is_int(w) {
        return "Int (immediate)";
    }
    match deref(w) {
        Value::Float(_) => "Float",
        Value::Char(_) => "Char",
        Value::Bool(_) => "Bool",
        Value::Unit => "Unit",
        Value::Str(_) | Value::StrCat { .. } | Value::StrSlice { .. } => "Str",
        Value::List { .. } | Value::Nil => "List",
        Value::Ctor { name, .. } => {
            // A static constructor name is 'static by construction.
            cname(*name)
        }
        Value::Closure { .. } => "Closure",
        Value::Record { .. } => "Record",
        Value::Tuple(_) => "Tuple",
        Value::Dict(_) => "Dict",
        Value::Set(_) => "Set",
        Value::Array(_) => "Array",
        Value::Bytes(_) => "Bytes",
        Value::Floats(_) => "Floats",
        Value::Json(_) => "Json",
        Value::Decoder(_) => "Decoder",
        Value::Regex(_) => "Regex",
    }
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

// Runtime singletons filled in by `runtime_init`. These MUST be exported (not
// plain `static`s): the backend merges the runtime bitcode into each program
// for cross-module inlining, and an *internal* global is DUPLICATED by that
// merge — the inlined copy of e.g. `array_get` would then read the program's
// private, never-initialized copy of `NOTHING` (0/NULL) while `runtime_init`
// (linked from the static lib) fills in the library's separate copy. Exporting
// them makes each a single symbol the merge marks `available_externally`,
// resolving every reference to the one instance the runtime initializes. (Same
// reason the `rt_*_v` singletons above are exported.)
#[export_name = "alm_NIL"]
static NIL: Global = Global::NULL;
#[export_name = "alm_NOTHING"]
static NOTHING: Global = Global::NULL;
#[export_name = "alm_LT"]
static LT: Global = Global::NULL;
#[export_name = "alm_EQ"]
static EQ: Global = Global::NULL;
#[export_name = "alm_GT"]
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
        Value::StrCat { len, .. } => {
            // Flatten the rope into a plain Str, overwriting this node so
            // the next read is O(1). Iterative right-spine stack: append
            // trees are left-deep, so recurse-on-left would overflow.
            let mut flat = Vec::with_capacity(*len);
            let mut stack = vec![p];
            while let Some(node) = stack.pop() {
                match deref(node) {
                    Value::Str(b) => flat.extend_from_slice(b),
                    Value::StrCat { left, right, .. } => {
                        stack.push(*right);
                        stack.push(*left);
                    }
                    // A slice leaf: read its subrange (flattening ITS base
                    // if needed) without touching the node itself.
                    Value::StrSlice { .. } => flat.extend_from_slice(sbytes(node)),
                    _ => crash!("expected a string"),
                }
            }
            *deref_mut(p) = Value::Str(flat);
            match deref(p) {
                Value::Str(b) => b.as_slice(),
                _ => unreachable!(),
            }
        }
        Value::StrSlice { base, off, len } => {
            let (base, off, len) = (*base, *off, *len);
            let b = sbytes(base); // flattens a rope base in place
            &b[off..off + len]
        }
        _ => crash!("expected a string"),
    }
}

#[inline]
unsafe fn is_str_value(v: u64) -> bool {
    !is_int(v)
        && matches!(
            deref(v),
            Value::Str(_) | Value::StrCat { .. } | Value::StrSlice { .. }
        )
}

/// A substring view of `s` covering byte range `from..to` (caller-validated,
/// on char boundaries). Tiny results are copied — a view that pins a large
/// base alive isn't worth 16 bytes.
unsafe fn mkslice(s: u64, from: usize, to: usize) -> u64 {
    if to <= from {
        return mkstr(Vec::new());
    }
    if to - from <= 16 {
        return mkstr(sbytes(s)[from..to].to_vec());
    }
    // Compose with an existing view so bases never nest.
    if let Value::StrSlice { base, off, .. } = deref(s) {
        return alloc(Value::StrSlice {
            base: *base,
            off: *off + from,
            len: to - from,
        });
    }
    alloc(Value::StrSlice {
        base: s,
        off: from,
        len: to - from,
    })
}

/// A string's byte length without flattening.
unsafe fn str_len_bytes(p: u64) -> usize {
    match deref(p) {
        Value::Str(b) => b.len(),
        Value::StrCat { len, .. } | Value::StrSlice { len, .. } => *len,
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

#[inline]
unsafe fn list_len(v: u64) -> usize {
    let mut n = 0;
    let mut cur = v;
    while let Value::List { tail, .. } = deref(cur) {
        n += 1;
        cur = *tail;
    }
    n
}

#[inline]
unsafe fn cons(head: u64, tail: u64) -> u64 {
    alloc(Value::List { head, tail })
}

/// Elm-order elements (head first).
unsafe fn to_vec(xs: u64) -> Vec<u64> {
    let mut out = Vec::new();
    let mut cur = xs;
    while let Value::List { head, tail } = deref(cur) {
        out.push(*head);
        cur = *tail;
    }
    out
}

/// Build a list from elements in Elm order (head first).
unsafe fn list_from_slice(items: &[u64]) -> u64 {
    let mut out = nil();
    for &x in items.iter().rev() {
        out = cons(x, out);
    }
    out
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
        _ => {
            eprintln!("alm: not a constructor, found {}", variant_name(v));
            if std::env::var("ALM_CRASH_BT").is_ok() {
                eprintln!("{}", std::backtrace::Backtrace::force_capture());
            }
            crash!("not a constructor")
        }
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
#[export_name = "alm_ARITY_MAP"]
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

// Exported like NIL/NOTHING (see the singleton comment above): the bitcode
// merge would otherwise DUPLICATE this static — generated code registering
// into one copy while inlined runtime code consults the other, silently
// disabling (or, for diagnostics, falsifying) trampoline identification.
#[export_name = "alm_BOX_TRAMPS"]
static mut BOX_TRAMPS: Option<std::collections::HashSet<usize>> = None;

/// Register a `box_closure` trampoline's function pointer so `unbox_closure`
/// can recognize a boxed typed closure and recover the original — making
/// box∘unbox identity-preserving (a function that round-trips through the
/// uniform representation stays the same value, so `==` on it still matches
/// Elm's reference equality).
#[no_mangle]
pub unsafe extern "C" fn alm_reg_box_tramp(fnptr: u64) {
    BOX_TRAMPS
        .get_or_insert_with(std::collections::HashSet::new)
        .insert(fnptr as usize);
}

/// If `w` is a uniform closure produced by `box_closure` (its function is a
/// registered trampoline), return the original captured typed-closure word;
/// otherwise 0.
#[no_mangle]
pub unsafe extern "C" fn alm_recover_boxed(w: u64) -> u64 {
    if is_int(w) {
        return 0;
    }
    if let Value::Closure { func, applied, args, .. } = deref(w) {
        // Only a *freshly* boxed closure — exactly its captured typed-closure
        // word, no user arguments applied yet (`applied == 1`). One that has
        // since been partially applied carries extra args that recovering the
        // original would drop, so leave it to be wrapped normally.
        if *applied == 1 {
            if let Some(set) = &*std::ptr::addr_of!(BOX_TRAMPS) {
                if set.contains(&(*func as usize)) {
                    return args[0];
                }
            }
        }
    }
    0
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
        _ => {
            eprintln!("alm: not a tuple, found {}", variant_name(v));
            if std::env::var("ALM_CRASH_BT").is_ok() {
                eprintln!("{}", std::backtrace::Backtrace::force_capture());
            }
            crash!("not a tuple")
        }
    }
}

#[no_mangle]
#[inline]
pub unsafe extern "C" fn rt_list_head(v: u64) -> u64 {
    match deref(v) {
        Value::List { head, .. } => *head,
        _ => crash!("head of an empty list"),
    }
}

/// The typed backend's `modBy 0` crash — elm/core's kernel crashes here.
#[no_mangle]
pub unsafe extern "C" fn rt_mod_by_zero() -> u64 {
    crash!("modBy 0 is undefined");
}

#[no_mangle]
pub unsafe extern "C" fn rt_list_tail(v: u64) -> u64 {
    match deref(v) {
        Value::List { tail, .. } => *tail,
        _ => crash!("tail of an empty list"),
    }
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
        Value::Str(_) | Value::StrCat { .. } | Value::StrSlice { .. } => {
            let b = sbytes(v);
            b.len() == len as usize && b == std::slice::from_raw_parts(ptr, len as usize)
        }
        _ => false,
    }
}

#[no_mangle]
#[inline]
pub unsafe extern "C" fn rt_is_cons(v: u64) -> bool {
    !is_int(v) && matches!(deref(v), Value::List { .. })
}

#[no_mangle]
#[inline]
pub unsafe extern "C" fn rt_is_nil(v: u64) -> bool {
    !is_int(v) && matches!(deref(v), Value::Nil)
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
type Fn33 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
type Fn34 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
type Fn35 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
type Fn36 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
type Fn37 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
type Fn38 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
type Fn39 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
type Fn40 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
type Fn41 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
type Fn42 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
type Fn43 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
type Fn44 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
type Fn45 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
type Fn46 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
type Fn47 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
type Fn48 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
type Fn49 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
type Fn50 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
type Fn51 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
type Fn52 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
type Fn53 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
type Fn54 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
type Fn55 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
type Fn56 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
type Fn57 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
type Fn58 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
type Fn59 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
type Fn60 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
type Fn61 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
type Fn62 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
type Fn63 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
type Fn64 = unsafe extern "C" fn(u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64, u64) -> u64;

#[inline]
unsafe fn call_fn(func: *const (), arity: usize, a: &[u64]) -> u64 {
    // DIAG (ALM_ARG_CHECK=1): every application funnels through here —
    // validate each argument word (int-tagged, or a plausible heap pointer
    // with a sane discriminant) and abort with context on the first bad
    // one. Trampoline closures carry ONE raw typed-closure word as their
    // first (captured) argument; it is exempt.
    if *std::ptr::addr_of!(FLAG_ARG_CHECK) {
        let tramp = if let Some(set) = &*std::ptr::addr_of!(BOX_TRAMPS) {
            set.contains(&(func as usize))
        } else {
            false
        };
        for (i, &w) in a.iter().enumerate() {
            if tramp && i == 0 {
                continue;
            }
            let bad =
                !is_int(w) && (w < 0x1_0000 || w % 8 != 0 || (*(w as *const u8)) > 40);
            if bad {
                eprintln!(
                    "=== ALM ARG CHECK (call_fn): bad a[{}]={:#x} arity={} func={:#x} ===",
                    i, w, arity, func as usize
                );
                for (k, &v) in a.iter().enumerate() {
                    eprintln!("  a[{}] = {:#x}", k, v);
                }
                eprintln!("a[{}]'s first 6 words:", i);
                for k in 0..6u64 {
                    if w >= 0x7000_0000_0000 {
                        eprintln!("  (unmapped-looking; skipping reads)");
                        break;
                    }
                    let v = *((w + k * 8) as *const u64);
                    let is_tramp = if let Some(set) = &*std::ptr::addr_of!(BOX_TRAMPS) {
                        set.contains(&(v as usize))
                    } else {
                        false
                    };
                    eprintln!("  +{:#04x}: {:#018x}{}", k * 8, v, if is_tramp { "  <-- REGISTERED TRAMPOLINE" } else { "" });
                }
                eprintln!("call_fn's own addr for slide calc: {:#x}", call_fn as usize);
                // Fault on purpose so ALM_SEGV_DUMP prints registers (lr/fp
                // give the caller); abort() would bypass the handler's info.
                std::ptr::read_volatile(8 as *const u64);
                std::process::abort();
            }
        }
    }

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
        33 => (transmute::<_, Fn33>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11], a[12], a[13], a[14], a[15], a[16], a[17], a[18], a[19], a[20], a[21], a[22], a[23], a[24], a[25], a[26], a[27], a[28], a[29], a[30], a[31], a[32]),
        34 => (transmute::<_, Fn34>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11], a[12], a[13], a[14], a[15], a[16], a[17], a[18], a[19], a[20], a[21], a[22], a[23], a[24], a[25], a[26], a[27], a[28], a[29], a[30], a[31], a[32], a[33]),
        35 => (transmute::<_, Fn35>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11], a[12], a[13], a[14], a[15], a[16], a[17], a[18], a[19], a[20], a[21], a[22], a[23], a[24], a[25], a[26], a[27], a[28], a[29], a[30], a[31], a[32], a[33], a[34]),
        36 => (transmute::<_, Fn36>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11], a[12], a[13], a[14], a[15], a[16], a[17], a[18], a[19], a[20], a[21], a[22], a[23], a[24], a[25], a[26], a[27], a[28], a[29], a[30], a[31], a[32], a[33], a[34], a[35]),
        37 => (transmute::<_, Fn37>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11], a[12], a[13], a[14], a[15], a[16], a[17], a[18], a[19], a[20], a[21], a[22], a[23], a[24], a[25], a[26], a[27], a[28], a[29], a[30], a[31], a[32], a[33], a[34], a[35], a[36]),
        38 => (transmute::<_, Fn38>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11], a[12], a[13], a[14], a[15], a[16], a[17], a[18], a[19], a[20], a[21], a[22], a[23], a[24], a[25], a[26], a[27], a[28], a[29], a[30], a[31], a[32], a[33], a[34], a[35], a[36], a[37]),
        39 => (transmute::<_, Fn39>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11], a[12], a[13], a[14], a[15], a[16], a[17], a[18], a[19], a[20], a[21], a[22], a[23], a[24], a[25], a[26], a[27], a[28], a[29], a[30], a[31], a[32], a[33], a[34], a[35], a[36], a[37], a[38]),
        40 => (transmute::<_, Fn40>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11], a[12], a[13], a[14], a[15], a[16], a[17], a[18], a[19], a[20], a[21], a[22], a[23], a[24], a[25], a[26], a[27], a[28], a[29], a[30], a[31], a[32], a[33], a[34], a[35], a[36], a[37], a[38], a[39]),
        41 => (transmute::<_, Fn41>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11], a[12], a[13], a[14], a[15], a[16], a[17], a[18], a[19], a[20], a[21], a[22], a[23], a[24], a[25], a[26], a[27], a[28], a[29], a[30], a[31], a[32], a[33], a[34], a[35], a[36], a[37], a[38], a[39], a[40]),
        42 => (transmute::<_, Fn42>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11], a[12], a[13], a[14], a[15], a[16], a[17], a[18], a[19], a[20], a[21], a[22], a[23], a[24], a[25], a[26], a[27], a[28], a[29], a[30], a[31], a[32], a[33], a[34], a[35], a[36], a[37], a[38], a[39], a[40], a[41]),
        43 => (transmute::<_, Fn43>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11], a[12], a[13], a[14], a[15], a[16], a[17], a[18], a[19], a[20], a[21], a[22], a[23], a[24], a[25], a[26], a[27], a[28], a[29], a[30], a[31], a[32], a[33], a[34], a[35], a[36], a[37], a[38], a[39], a[40], a[41], a[42]),
        44 => (transmute::<_, Fn44>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11], a[12], a[13], a[14], a[15], a[16], a[17], a[18], a[19], a[20], a[21], a[22], a[23], a[24], a[25], a[26], a[27], a[28], a[29], a[30], a[31], a[32], a[33], a[34], a[35], a[36], a[37], a[38], a[39], a[40], a[41], a[42], a[43]),
        45 => (transmute::<_, Fn45>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11], a[12], a[13], a[14], a[15], a[16], a[17], a[18], a[19], a[20], a[21], a[22], a[23], a[24], a[25], a[26], a[27], a[28], a[29], a[30], a[31], a[32], a[33], a[34], a[35], a[36], a[37], a[38], a[39], a[40], a[41], a[42], a[43], a[44]),
        46 => (transmute::<_, Fn46>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11], a[12], a[13], a[14], a[15], a[16], a[17], a[18], a[19], a[20], a[21], a[22], a[23], a[24], a[25], a[26], a[27], a[28], a[29], a[30], a[31], a[32], a[33], a[34], a[35], a[36], a[37], a[38], a[39], a[40], a[41], a[42], a[43], a[44], a[45]),
        47 => (transmute::<_, Fn47>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11], a[12], a[13], a[14], a[15], a[16], a[17], a[18], a[19], a[20], a[21], a[22], a[23], a[24], a[25], a[26], a[27], a[28], a[29], a[30], a[31], a[32], a[33], a[34], a[35], a[36], a[37], a[38], a[39], a[40], a[41], a[42], a[43], a[44], a[45], a[46]),
        48 => (transmute::<_, Fn48>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11], a[12], a[13], a[14], a[15], a[16], a[17], a[18], a[19], a[20], a[21], a[22], a[23], a[24], a[25], a[26], a[27], a[28], a[29], a[30], a[31], a[32], a[33], a[34], a[35], a[36], a[37], a[38], a[39], a[40], a[41], a[42], a[43], a[44], a[45], a[46], a[47]),
        49 => (transmute::<_, Fn49>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11], a[12], a[13], a[14], a[15], a[16], a[17], a[18], a[19], a[20], a[21], a[22], a[23], a[24], a[25], a[26], a[27], a[28], a[29], a[30], a[31], a[32], a[33], a[34], a[35], a[36], a[37], a[38], a[39], a[40], a[41], a[42], a[43], a[44], a[45], a[46], a[47], a[48]),
        50 => (transmute::<_, Fn50>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11], a[12], a[13], a[14], a[15], a[16], a[17], a[18], a[19], a[20], a[21], a[22], a[23], a[24], a[25], a[26], a[27], a[28], a[29], a[30], a[31], a[32], a[33], a[34], a[35], a[36], a[37], a[38], a[39], a[40], a[41], a[42], a[43], a[44], a[45], a[46], a[47], a[48], a[49]),
        51 => (transmute::<_, Fn51>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11], a[12], a[13], a[14], a[15], a[16], a[17], a[18], a[19], a[20], a[21], a[22], a[23], a[24], a[25], a[26], a[27], a[28], a[29], a[30], a[31], a[32], a[33], a[34], a[35], a[36], a[37], a[38], a[39], a[40], a[41], a[42], a[43], a[44], a[45], a[46], a[47], a[48], a[49], a[50]),
        52 => (transmute::<_, Fn52>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11], a[12], a[13], a[14], a[15], a[16], a[17], a[18], a[19], a[20], a[21], a[22], a[23], a[24], a[25], a[26], a[27], a[28], a[29], a[30], a[31], a[32], a[33], a[34], a[35], a[36], a[37], a[38], a[39], a[40], a[41], a[42], a[43], a[44], a[45], a[46], a[47], a[48], a[49], a[50], a[51]),
        53 => (transmute::<_, Fn53>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11], a[12], a[13], a[14], a[15], a[16], a[17], a[18], a[19], a[20], a[21], a[22], a[23], a[24], a[25], a[26], a[27], a[28], a[29], a[30], a[31], a[32], a[33], a[34], a[35], a[36], a[37], a[38], a[39], a[40], a[41], a[42], a[43], a[44], a[45], a[46], a[47], a[48], a[49], a[50], a[51], a[52]),
        54 => (transmute::<_, Fn54>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11], a[12], a[13], a[14], a[15], a[16], a[17], a[18], a[19], a[20], a[21], a[22], a[23], a[24], a[25], a[26], a[27], a[28], a[29], a[30], a[31], a[32], a[33], a[34], a[35], a[36], a[37], a[38], a[39], a[40], a[41], a[42], a[43], a[44], a[45], a[46], a[47], a[48], a[49], a[50], a[51], a[52], a[53]),
        55 => (transmute::<_, Fn55>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11], a[12], a[13], a[14], a[15], a[16], a[17], a[18], a[19], a[20], a[21], a[22], a[23], a[24], a[25], a[26], a[27], a[28], a[29], a[30], a[31], a[32], a[33], a[34], a[35], a[36], a[37], a[38], a[39], a[40], a[41], a[42], a[43], a[44], a[45], a[46], a[47], a[48], a[49], a[50], a[51], a[52], a[53], a[54]),
        56 => (transmute::<_, Fn56>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11], a[12], a[13], a[14], a[15], a[16], a[17], a[18], a[19], a[20], a[21], a[22], a[23], a[24], a[25], a[26], a[27], a[28], a[29], a[30], a[31], a[32], a[33], a[34], a[35], a[36], a[37], a[38], a[39], a[40], a[41], a[42], a[43], a[44], a[45], a[46], a[47], a[48], a[49], a[50], a[51], a[52], a[53], a[54], a[55]),
        57 => (transmute::<_, Fn57>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11], a[12], a[13], a[14], a[15], a[16], a[17], a[18], a[19], a[20], a[21], a[22], a[23], a[24], a[25], a[26], a[27], a[28], a[29], a[30], a[31], a[32], a[33], a[34], a[35], a[36], a[37], a[38], a[39], a[40], a[41], a[42], a[43], a[44], a[45], a[46], a[47], a[48], a[49], a[50], a[51], a[52], a[53], a[54], a[55], a[56]),
        58 => (transmute::<_, Fn58>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11], a[12], a[13], a[14], a[15], a[16], a[17], a[18], a[19], a[20], a[21], a[22], a[23], a[24], a[25], a[26], a[27], a[28], a[29], a[30], a[31], a[32], a[33], a[34], a[35], a[36], a[37], a[38], a[39], a[40], a[41], a[42], a[43], a[44], a[45], a[46], a[47], a[48], a[49], a[50], a[51], a[52], a[53], a[54], a[55], a[56], a[57]),
        59 => (transmute::<_, Fn59>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11], a[12], a[13], a[14], a[15], a[16], a[17], a[18], a[19], a[20], a[21], a[22], a[23], a[24], a[25], a[26], a[27], a[28], a[29], a[30], a[31], a[32], a[33], a[34], a[35], a[36], a[37], a[38], a[39], a[40], a[41], a[42], a[43], a[44], a[45], a[46], a[47], a[48], a[49], a[50], a[51], a[52], a[53], a[54], a[55], a[56], a[57], a[58]),
        60 => (transmute::<_, Fn60>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11], a[12], a[13], a[14], a[15], a[16], a[17], a[18], a[19], a[20], a[21], a[22], a[23], a[24], a[25], a[26], a[27], a[28], a[29], a[30], a[31], a[32], a[33], a[34], a[35], a[36], a[37], a[38], a[39], a[40], a[41], a[42], a[43], a[44], a[45], a[46], a[47], a[48], a[49], a[50], a[51], a[52], a[53], a[54], a[55], a[56], a[57], a[58], a[59]),
        61 => (transmute::<_, Fn61>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11], a[12], a[13], a[14], a[15], a[16], a[17], a[18], a[19], a[20], a[21], a[22], a[23], a[24], a[25], a[26], a[27], a[28], a[29], a[30], a[31], a[32], a[33], a[34], a[35], a[36], a[37], a[38], a[39], a[40], a[41], a[42], a[43], a[44], a[45], a[46], a[47], a[48], a[49], a[50], a[51], a[52], a[53], a[54], a[55], a[56], a[57], a[58], a[59], a[60]),
        62 => (transmute::<_, Fn62>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11], a[12], a[13], a[14], a[15], a[16], a[17], a[18], a[19], a[20], a[21], a[22], a[23], a[24], a[25], a[26], a[27], a[28], a[29], a[30], a[31], a[32], a[33], a[34], a[35], a[36], a[37], a[38], a[39], a[40], a[41], a[42], a[43], a[44], a[45], a[46], a[47], a[48], a[49], a[50], a[51], a[52], a[53], a[54], a[55], a[56], a[57], a[58], a[59], a[60], a[61]),
        63 => (transmute::<_, Fn63>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11], a[12], a[13], a[14], a[15], a[16], a[17], a[18], a[19], a[20], a[21], a[22], a[23], a[24], a[25], a[26], a[27], a[28], a[29], a[30], a[31], a[32], a[33], a[34], a[35], a[36], a[37], a[38], a[39], a[40], a[41], a[42], a[43], a[44], a[45], a[46], a[47], a[48], a[49], a[50], a[51], a[52], a[53], a[54], a[55], a[56], a[57], a[58], a[59], a[60], a[61], a[62]),
        64 => (transmute::<_, Fn64>(func))(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7], a[8], a[9], a[10], a[11], a[12], a[13], a[14], a[15], a[16], a[17], a[18], a[19], a[20], a[21], a[22], a[23], a[24], a[25], a[26], a[27], a[28], a[29], a[30], a[31], a[32], a[33], a[34], a[35], a[36], a[37], a[38], a[39], a[40], a[41], a[42], a[43], a[44], a[45], a[46], a[47], a[48], a[49], a[50], a[51], a[52], a[53], a[54], a[55], a[56], a[57], a[58], a[59], a[60], a[61], a[62], a[63]),
        _ => crash!("function arity too large (max 64)"),
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
        // call_fn's max (64; a wide record alias constructor curried through a
        // decode pipeline can exceed 32 — elm-mastodon's entities).
        let mut all: [u64; 64] = [0u64; 64];
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
    let mut all: [u64; 64] = [0u64; 64];
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
    let mut all: [u64; 64] = [0u64; 64];
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
        // Embedded Elm values (HtmlAsJson taggers/decoders): same notion of
        // equality as any function/opaque value — by the underlying word.
        (Elm(x), Elm(y)) => unsafe { value_eq(*x, *y) },
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
        (
            Value::Str(_) | Value::StrCat { .. } | Value::StrSlice { .. },
            Value::Str(_) | Value::StrCat { .. } | Value::StrSlice { .. },
        ) => sbytes(a) == sbytes(b),
        // `Json.Encode.Value`/`Json.Decode.Value` wrap raw JS values in Elm, so
        // `==` compares them structurally (deep-equal, with object keys matched
        // by name regardless of order — matching JS `_Utils_eq`).
        (Value::Json(x), Value::Json(y)) => json_eq(x, y),
        (Value::Nil, Value::Nil) => true,
        (Value::List { .. }, Value::List { .. }) => {
            let (mut x, mut y) = (a, b);
            loop {
                match (deref(x), deref(y)) {
                    (Value::List { head: h1, tail: t1 }, Value::List { head: h2, tail: t2 }) => {
                        if !value_eq(*h1, *h2) {
                            return false;
                        }
                        x = *t1;
                        y = *t2;
                    }
                    (Value::Nil, Value::Nil) => return true,
                    _ => return false,
                }
            }
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
        // Dict/Set are balanced trees whose shape isn't canonical (same
        // contents, different rotations), so compare by in-order contents.
        (Value::Dict(x), Value::Dict(y)) => {
            if tsize(*x) != tsize(*y) {
                return false;
            }
            let (mut px, mut py) = (Vec::new(), Vec::new());
            tcollect(*x, &mut px);
            tcollect(*y, &mut py);
            px.iter()
                .zip(&py)
                .all(|((k1, v1), (k2, v2))| value_eq(*k1, *k2) && value_eq(*v1, *v2))
        }
        (Value::Set(x), Value::Set(y)) => {
            if tsize(*x) != tsize(*y) {
                return false;
            }
            let (mut px, mut py) = (Vec::new(), Vec::new());
            tcollect(*x, &mut px);
            tcollect(*y, &mut py);
            px.iter().zip(&py).all(|((k1, _), (k2, _))| value_eq(*k1, *k2))
        }
        (Value::Array(x), Value::Array(y)) => {
            if tsize(*x) != tsize(*y) {
                return false;
            }
            let (mut px, mut py) = (Vec::new(), Vec::new());
            tcollect_vals(*x, &mut px);
            tcollect_vals(*y, &mut py);
            px.iter().zip(&py).all(|(&p, &q)| value_eq(p, q))
        }
        (Value::Bytes(x), Value::Bytes(y)) => x == y,
        // linear-algebra vectors/matrices: element-wise float equality,
        // matching JS `_Utils_eq` over Float64Array contents.
        (Value::Floats(x), Value::Floats(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| p == q)
        }
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
        (
            Value::Str(_) | Value::StrCat { .. } | Value::StrSlice { .. },
            Value::Str(_) | Value::StrCat { .. } | Value::StrSlice { .. },
        ) => match sbytes(a).cmp(sbytes(b)) {
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Greater => 1,
        },
        (Value::Nil, Value::Nil) => 0,
        (Value::Nil, Value::List { .. }) => -1,
        (Value::List { .. }, Value::Nil) => 1,
        (Value::List { .. }, Value::List { .. }) => {
            // Lexicographic in Elm order (head first).
            let (mut x, mut y) = (a, b);
            loop {
                match (deref(x), deref(y)) {
                    (Value::List { head: h1, tail: t1 }, Value::List { head: h2, tail: t2 }) => {
                        let c = value_cmp(*h1, *h2);
                        if c != 0 {
                            return c;
                        }
                        x = *t1;
                        y = *t2;
                    }
                    (Value::Nil, Value::Nil) => return 0,
                    (Value::Nil, _) => return -1,
                    _ => return 1,
                }
            }
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
        Value::Str(_) | Value::StrCat { .. } | Value::StrSlice { .. } => {
            let (la, lb) = (str_len_bytes(a), str_len_bytes(b));
            if la == 0 {
                return b;
            }
            if lb == 0 {
                return a;
            }
            // Short result: concatenate eagerly (a rope node costs more than
            // the copy). Long result: O(1) rope node, flattened on first read.
            if la + lb <= 64 {
                let mut bytes = Vec::with_capacity(la + lb);
                bytes.extend_from_slice(sbytes(a));
                bytes.extend_from_slice(sbytes(b));
                mkstr(bytes)
            } else {
                alloc(Value::StrCat { left: a, right: b, len: la + lb })
            }
        }
        Value::Nil => b,
        Value::List { .. } => {
            // a ++ b: re-cons a's elements onto b (which is shared, not
            // copied — the JS backend does the same).
            let mut out = b;
            for &x in to_vec(a).iter().rev() {
                out = cons(x, out);
            }
            out
        }
        _ => {
            eprintln!("alm: ++ on a non-appendable, found {}", variant_name(a));
            if std::env::var("ALM_CRASH_BT").is_ok() {
                eprintln!("{}", std::backtrace::Backtrace::force_capture());
            }
            crash!("++ on a non-appendable")
        }
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
// `&&` / `||` as point-free function values (`Basics.and` / `Basics.or`, e.g.
// `List.map2 and xs ys`). Total — no short-circuit is observable in Elm.
unsafe extern "C" fn basics_and(a: u64, b: u64) -> u64 {
    rt_bool(rt_is_true(a) && rt_is_true(b))
}
unsafe extern "C" fn basics_or(a: u64, b: u64) -> u64 {
    rt_bool(rt_is_true(a) || rt_is_true(b))
}
#[export_name = "rtb$Basics$modBy"]
unsafe extern "C" fn basics_mod_by(m: u64, n: u64) -> u64 {
    let m = as_int(m);
    if m == 0 {
        // elm/core's kernel CRASHES on modBy 0 (unlike remainderBy, whose
        // NaN result flows on) — match it.
        crash!("modBy 0 is undefined");
    }
    if m == -1 {
        // i64::MIN % -1 overflows in Rust; x mod -1 == 0 for every x.
        return rt_int(0);
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
        // JS: n % 0 is NaN, which every later comparison treats as false —
        // 0 reproduces that comparison behavior in i64 (select-list's fuzz
        // tests do `remainderBy (List.length xs) n` on possibly-empty xs).
        return rt_int(0);
    }
    if m == -1 {
        // i64::MIN % -1 overflows in Rust; the result is 0 for every x.
        return rt_int(0);
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
/// Convert a float to the integer word, clamped to the representable tagged
/// range. Ints are 63-bit tagged immediates (`(n << 1) | 1`), so a value at or
/// beyond ±2^62 overflows the tag bit and wraps to garbage. `Basics.round (1 /
/// 0)` is a real idiom (elm-review's `infinity : Int` sentinel): on JS
/// `round Infinity` stays `Infinity` and compares larger than every real value,
/// so clamp to the tagged max/min to preserve that ordering rather than wrap.
#[inline]
fn f64_to_int_word(f: f64) -> i64 {
    const HI: f64 = (1i64 << 62) as f64 - 1.0;
    const LO: f64 = -((1i64 << 62) as f64);
    if f.is_nan() {
        0
    } else if f >= HI {
        (1i64 << 62) - 1
    } else if f <= LO {
        -(1i64 << 62)
    } else {
        f as i64
    }
}
unsafe extern "C" fn basics_round(x: u64) -> u64 {
    // Math.round: half rounds toward +infinity.
    rt_int(f64_to_int_word((num(x) + 0.5).floor()))
}
unsafe extern "C" fn basics_floor(x: u64) -> u64 {
    rt_int(f64_to_int_word(num(x).floor()))
}
unsafe extern "C" fn basics_ceiling(x: u64) -> u64 {
    rt_int(f64_to_int_word(num(x).ceil()))
}
unsafe extern "C" fn basics_truncate(x: u64) -> u64 {
    rt_int(f64_to_int_word(num(x)))
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
    cons(x, nil())
}
#[export_name = "rtb$List$repeat"]
unsafe extern "C" fn list_repeat(n: u64, x: u64) -> u64 {
    let n = as_int(n).max(0) as usize;
    let mut out = nil();
    for _ in 0..n {
        out = cons(x, out);
    }
    out
}
#[export_name = "rtb$List$range"]
unsafe extern "C" fn list_range(lo: u64, hi: u64) -> u64 {
    let (lo, hi) = (as_int(lo), as_int(hi));
    let mut out = nil();
    let mut h = hi;
    while h >= lo {
        out = cons(mk_int(h), out);
        h -= 1;
    }
    out
}
#[export_name = "rtb$List$map"]
unsafe extern "C" fn list_map(f: u64, xs: u64) -> u64 {
    let mut out: Vec<u64> = to_vec(xs);
    for x in out.iter_mut() {
        *x = ap1(f, *x);
    }
    list_from_slice(&out)
}
#[export_name = "rtb$List$indexedMap"]
unsafe extern "C" fn list_indexed_map(f: u64, xs: u64) -> u64 {
    let mut out: Vec<u64> = to_vec(xs);
    for (elm_i, x) in out.iter_mut().enumerate() {
        *x = ap2(f, mk_int(elm_i as i64), *x);
    }
    list_from_slice(&out)
}
#[export_name = "rtb$List$foldl"]
unsafe extern "C" fn list_foldl(f: u64, mut acc: u64, xs: u64) -> u64 {
    let mut cur = xs;
    while let Value::List { head, tail } = deref(cur) {
        let (h, t) = (*head, *tail);
        acc = ap2(f, h, acc);
        cur = t;
    }
    acc
}
#[export_name = "rtb$List$foldr"]
unsafe extern "C" fn list_foldr(f: u64, mut acc: u64, xs: u64) -> u64 {
    // Right fold visits last-to-first.
    for &x in to_vec(xs).iter().rev() {
        acc = ap2(f, x, acc);
    }
    acc
}
#[export_name = "rtb$List$filter"]
unsafe extern "C" fn list_filter(is_good: u64, xs: u64) -> u64 {
    let data: Vec<u64> = to_vec(xs)
        .into_iter()
        .filter(|&x| rt_is_true(ap1(is_good, x)))
        .collect();
    list_from_slice(&data)
}
#[export_name = "rtb$List$filterMap"]
unsafe extern "C" fn list_filter_map(f: u64, xs: u64) -> u64 {
    let mut data = Vec::new();
    for x in to_vec(xs) {
        let m = ap1(f, x);
        if is_ctor0(m) {
            data.push(rt_ctor_arg(m, 0));
        }
    }
    list_from_slice(&data)
}
#[export_name = "rtb$List$length"]
unsafe extern "C" fn list_length(xs: u64) -> u64 {
    rt_int(list_len(xs) as i64)
}
#[export_name = "rtb$List$reverse"]
unsafe extern "C" fn list_reverse(xs: u64) -> u64 {
    let mut out = nil();
    let mut cur = xs;
    while let Value::List { head, tail } = deref(cur) {
        out = cons(*head, out);
        cur = *tail;
    }
    out
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
    rt_bool(rt_is_nil(xs))
}
#[export_name = "rtb$List$head"]
unsafe extern "C" fn list_head(xs: u64) -> u64 {
    match deref(xs) {
        Value::List { head, .. } => just(*head),
        _ => nothing(),
    }
}
#[export_name = "rtb$List$tail"]
unsafe extern "C" fn list_tail(xs: u64) -> u64 {
    match deref(xs) {
        Value::List { tail, .. } => just(*tail),
        _ => nothing(),
    }
}
#[export_name = "rtb$List$take"]
unsafe extern "C" fn list_take(n: u64, xs: u64) -> u64 {
    let mut count = as_int(n).max(0) as usize;
    let mut taken = Vec::with_capacity(count.min(64));
    let mut cur = xs;
    while count > 0 {
        match deref(cur) {
            Value::List { head, tail } => {
                taken.push(*head);
                cur = *tail;
                count -= 1;
            }
            _ => break,
        }
    }
    list_from_slice(&taken)
}
#[export_name = "rtb$List$drop"]
unsafe extern "C" fn list_drop(n: u64, xs: u64) -> u64 {
    let mut count = as_int(n).max(0);
    let mut cur = xs;
    while count > 0 {
        match deref(cur) {
            Value::List { tail, .. } => {
                cur = *tail;
                count -= 1;
            }
            _ => break,
        }
    }
    cur
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

/// Byte offset of the `cp`-th codepoint in a UTF-8 byte slice, clamped to the
/// slice length. The byte-slice twin of [`char_byte`], used by the elm/parser
/// primitives whose offsets are codepoint indices (so they stay consistent
/// with `String.slice`, which is codepoint-based). O(cp); parser inputs are
/// short or ASCII in practice.
fn cp_byte(s: &[u8], cp: usize) -> usize {
    let mut o = 0;
    let mut n = 0;
    while n < cp && o < s.len() {
        let (_, len) = decode_char(s, o);
        o += len;
        n += 1;
    }
    o
}

/// [`cp_byte`] with a per-string cache for the parser primitives, which call
/// it once per parse step: a fresh O(offset) rescan each step made parsing an
/// N-char source O(N²) (elm-syntax's module-sized inputs timed out). ASCII
/// sources (checked once per string) convert in O(1); others advance a
/// monotonic (codepoint, byte) cursor and only rescan on backtrack. The
/// mutator is single-threaded and string bytes are immutable (a rope flatten
/// preserves content), so plain statics are sound. Keying by the value word
/// is safe against address reuse: the word stored in the static is itself a
/// scanned GC root, so the cached string cannot be collected — and its
/// address cannot be recycled — while it is the cache key.
unsafe fn parser_cp_byte(sword: u64, s: &[u8], cp: usize) -> usize {
    static mut ASCII: [(u64, bool); 4] = [(0, false); 4];
    static mut ASCII_NEXT: usize = 0;
    static mut CURSOR: (u64, usize, usize) = (0, 0, 0);
    // Key the ASCII flag by the slice's BASE string: code that recurses on
    // `String.slice`d tails creates a fresh view per step, and a per-view
    // scan would stay O(n²). ASCII-ness of the base covers every subrange,
    // the base word is stable across the whole walk, and a few entries
    // cover code walking a couple of strings in lockstep. Cached words are
    // GC roots (see below), so keys are never stale.
    // Tiny strings: scan inline — caching them would evict the entries
    // that matter (a loop slicing 4-char groups off a megabyte source must
    // not push the source's entry out and force a full rescan per group).
    let is_ascii = if s.len() <= 64 {
        s.iter().all(|b| b & 0x80 == 0)
    } else {
        let key = match deref(sword) {
            Value::StrSlice { base, .. } => *base,
            _ => sword,
        };
        let ascii_cache = &mut *std::ptr::addr_of_mut!(ASCII);
        match ascii_cache.iter().find(|e| e.0 == key) {
            Some(e) => e.1,
            None => {
                let a = sbytes(key).iter().all(|b| b & 0x80 == 0);
                let slot = &mut *std::ptr::addr_of_mut!(ASCII_NEXT);
                ascii_cache[*slot] = (key, a);
                *slot = (*slot + 1) % 4;
                a
            }
        }
    };
    if is_ascii {
        return cp.min(s.len());
    }
    let cursor = &mut *std::ptr::addr_of_mut!(CURSOR);
    let (mut n, mut o) = if cursor.0 == sword && cursor.1 <= cp {
        (cursor.1, cursor.2)
    } else {
        (0, 0)
    };
    while n < cp && o < s.len() {
        let (_, len) = decode_char(s, o);
        o += len;
        n += 1;
    }
    *cursor = (sword, cp.min(n), o);
    o
}

/// Number of codepoints in a UTF-8 byte slice.
fn cp_count(s: &[u8]) -> usize {
    let mut o = 0;
    let mut n = 0;
    while o < s.len() {
        let (_, len) = decode_char(s, o);
        o += len;
        n += 1;
    }
    n
}

unsafe fn slice_cp(p: u64, mut start: i64, mut end: i64) -> u64 {
    let s = sstr(p);
    // Count chars (O(n)) only when an index is negative (from-the-end);
    // `char_byte` clamps past-the-end indices, so a huge `end` needs no
    // count. Keeps `dropLeft`/`left` walks linear instead of quadratic.
    if start < 0 || end < 0 {
        let len = s.chars().count() as i64;
        if start < 0 {
            start += len;
        }
        if end < 0 {
            end += len;
        }
    }
    start = start.max(0);
    if end <= start {
        return mkstr(Vec::new());
    }
    // Cached conversion: ParserFast-style code slices per character
    // (`String.slice o (o+1) src`), and a fresh O(offset) walk per call made
    // that O(n²) over the source.
    let from = parser_cp_byte(p, s.as_bytes(), start as usize);
    // Whole-tail slice (the `dropLeft` shape): skip the second walk.
    let to = if end >= i64::MAX / 2 || end as usize >= s.len() {
        s.len()
    } else {
        parser_cp_byte(p, s.as_bytes(), end as usize)
    };
    mkslice(p, from, to)
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
    rt_bool(str_len_bytes(s) == 0)
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
        slice_cp(s, -n, i64::MAX)
    }
}
unsafe extern "C" fn string_drop_left(n: u64, s: u64) -> u64 {
    if as_int(n) < 1 {
        s
    } else {
        slice_cp(s, as_int(n), i64::MAX)
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
            let blen = sbytes(s).len();
            let rest = mkslice(s, ch.len_utf8(), blen);
            just(pair(rt_chr(ch as i32), rest))
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
    // Full Unicode, like JS String.prototype.toUpperCase.
    mkstr(sstr(s).to_uppercase().into_bytes())
}
unsafe extern "C" fn string_to_lower(s: u64) -> u64 {
    mkstr(sstr(s).to_lowercase().into_bytes())
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
unsafe extern "C" fn string_pad(n: u64, c: u64, s: u64) -> u64 {
    // `pad n char string`: center, extra padding on the right for an odd
    // deficit — `repeat (ceil half) c ++ string ++ repeat (floor half) c`.
    let ch = char::from_u32(as_int(c) as u32).unwrap_or(' ');
    let deficit = (as_int(n) - sstr(s).chars().count() as i64).max(0);
    let left = (deficit + 1) / 2;
    let right = deficit / 2;
    let mut out = String::new();
    for _ in 0..left {
        out.push(ch);
    }
    out.push_str(sstr(s));
    for _ in 0..right {
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
unsafe extern "C" fn char_is_oct_digit(c: u64) -> u64 {
    let n = as_int(c);
    rt_bool((b'0' as i64..=b'7' as i64).contains(&n))
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
    // JS kernel: String.prototype.toUpperCase — full Unicode, not ASCII
    // (elm-syntax's Char.Extra relies on Greek letters case-mapping).
    // KNOWN LIMIT: expansion mappings ('ß' → "SS") truncate to their first
    // char — a native Char is one codepoint, JS's is a string.
    match char::from_u32(as_int(c) as u32) {
        Some(ch) => rt_chr(ch.to_uppercase().next().unwrap_or(ch) as i32),
        None => c,
    }
}
unsafe extern "C" fn char_to_lower(c: u64) -> u64 {
    match char::from_u32(as_int(c) as u32) {
        Some(ch) => rt_chr(ch.to_lowercase().next().unwrap_or(ch) as i32),
        None => c,
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
        Value::Str(_) | Value::StrCat { .. } | Value::StrSlice { .. } => debug_string(out, sbytes(v), false),
        Value::List { .. } | Value::Nil => {
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
        Value::Regex(_) => out.push_str("<internals>"),
        Value::Floats(_) => out.push_str("<internals>"),
        Value::Dict(root) => {
            let mut pairs = Vec::new();
            tcollect(*root, &mut pairs);
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
        Value::Set(root) => {
            let mut pairs = Vec::new();
            tcollect(*root, &mut pairs);
            out.push_str("Set.fromList [");
            for (i, (x, _)) in pairs.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                debug_fmt(out, *x);
            }
            out.push(']');
        }
        Value::Array(root) => {
            let mut els = Vec::new();
            tcollect_vals(*root, &mut els);
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
        JsonValue::Elm(_) => out.push_str("<internals>"),
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
    if rt_is_nil(tasks) {
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

// `tan` ported verbatim from V8's src/base/ieee754.cc (fdlibm: __kernel_tan,
// __ieee754_rem_pio2, tan) — the implementation behind `Math.tan` — so native
// results are bit-identical to the JS backend. The host libm (Apple's)
// differs from fdlibm by 1 ulp on some inputs (e.g. tan(pi/8)). Two caveats:
//
// * node/V8 is compiled by clang with its default `-ffp-contract=on`, which
//   fuses every single-use `a*b` feeding an add/sub into an FMA. Rust never
//   contracts, so those exact contractions are spelled out here with
//   `f64::mul_add` — verified bit-identical to this machine's node on a large
//   sweep. A plain (uncontracted) transcription differs from node by 1 ulp on
//   ~1.4% of arguments.
// * Arguments at or above 2^20*(pi/2) fall back to the host libm: full
//   reduction would need fdlibm's multi-precision `__kernel_rem_pio2`, and
//   such inputs keep today's (host-libm) behavior.

/// fdlibm `__kernel_tan` on ~[-pi/4, pi/4]; `y` is the tail of `x`;
/// `iy == 1` returns tan, `iy == -1` returns -1/tan.
fn k_tan(mut x: f64, mut y: f64, iy: i32) -> f64 {
    const T: [f64; 13] = [
        3.33333333333334091986e-01,  /* 3FD55555, 55555563 */
        1.33333333333201242699e-01,  /* 3FC11111, 1110FE7A */
        5.39682539762260521377e-02,  /* 3FABA1BA, 1BB341FE */
        2.18694882948595424599e-02,  /* 3F9664F4, 8406D637 */
        8.86323982359930005737e-03,  /* 3F8226E3, E96E8493 */
        3.59207910759131235356e-03,  /* 3F6D6D22, C9560328 */
        1.45620945432529025516e-03,  /* 3F57DBC8, FEE08315 */
        5.88041240820264096874e-04,  /* 3F4344D8, F2F26501 */
        2.46463134818469906812e-04,  /* 3F3026F7, 1A8D1068 */
        7.81794442939557092300e-05,  /* 3F147E88, A03792A6 */
        7.14072491382608190305e-05,  /* 3F12B80F, 32F0A7E9 */
        -1.85586374855275456654e-05, /* BEF375CB, DB605373 */
        2.59073051863633712884e-05,  /* 3EFB2A70, 74BF7AD4 */
    ];
    const ONE: f64 = 1.0;
    const PIO4: f64 = 7.85398163397448278999e-01; /* 3FE921FB, 54442D18 */
    const PIO4_LO: f64 = 3.06161699786838301793e-17; /* 3C81A626, 33145C07 */
    fn zero_low_word(x: f64) -> f64 {
        f64::from_bits(f64::to_bits(x) & 0xFFFF_FFFF_0000_0000)
    }

    let hx = (f64::to_bits(x) >> 32) as i32;
    let ix = hx & 0x7fffffff;
    if ix < 0x3E300000 {
        /* x < 2**-28 */
        if x as i64 == 0 {
            let low = f64::to_bits(x) as u32;
            if ((ix as u32 | low) | (iy + 1) as u32) == 0 {
                return ONE / x.abs();
            } else if iy == 1 {
                return x;
            } else {
                /* compute -1 / (x+y) carefully */
                let w = x + y;
                let z = zero_low_word(w);
                let v = y - (z - x);
                let a = -ONE / w;
                let t = zero_low_word(a);
                let s = t.mul_add(z, ONE);
                return a.mul_add(t.mul_add(v, s), t);
            }
        }
    }
    let big = ix >= 0x3FE59428; /* |x| >= 0.6744 */
    if big {
        if hx < 0 {
            x = -x;
            y = -y;
        }
        let z = PIO4 - x;
        let w = PIO4_LO - y;
        x = z + w;
        y = 0.0;
    }
    let z = x * x;
    let w = z * z;
    let r = w.mul_add(
        w.mul_add(w.mul_add(w.mul_add(w.mul_add(T[11], T[9]), T[7]), T[5]), T[3]),
        T[1],
    );
    let vi = w.mul_add(
        w.mul_add(w.mul_add(w.mul_add(w.mul_add(T[12], T[10]), T[8]), T[6]), T[4]),
        T[2],
    );
    let s = z * x;
    /* r = y + z*(s*(r+v)+y); r += T[0]*s — with v = z*vi and clang's fusions */
    let r = z.mul_add(s.mul_add(z.mul_add(vi, r), y), y);
    let r = T[0].mul_add(s, r);
    let w = x + r;
    if big {
        let v = iy as f64;
        let sgn = (1 - ((hx >> 30) & 2)) as f64;
        return sgn * (-2.0f64).mul_add(x - (w * w / (w + v) - r), v);
    }
    if iy == 1 {
        return w;
    }
    /* compute -1.0/(x+r) accurately */
    let z = zero_low_word(w);
    let v = r - (z - x); /* z+v = r+x */
    let a = -1.0 / w;
    let t = zero_low_word(a);
    let s = t.mul_add(z, 1.0);
    a.mul_add(t.mul_add(v, s), t)
}

/// fdlibm/V8 `__ieee754_rem_pio2`: x rem pi/2 as (n, y0, y1). The caller
/// handles |x| ~<= pi/4 and |x| >= 2^20*(pi/2) (the `__kernel_rem_pio2` case).
fn rem_pio2(x: f64) -> (i32, f64, f64) {
    const HALF: f64 = 0.5;
    /// 53 bits of 2/pi
    const INV_PIO2: f64 = 6.36619772367581382433e-01; /* 0x3FE45F30, 0x6DC9C883 */
    /// first 33 bits of pi/2
    const PIO2_1: f64 = 1.57079632673412561417e+00; /* 0x3FF921FB, 0x54400000 */
    /// pi/2 - PIO2_1
    const PIO2_1T: f64 = 6.07710050650619224932e-11; /* 0x3DD0B461, 0x1A626331 */
    /// second 33 bits of pi/2
    const PIO2_2: f64 = 6.07710050630396597660e-11; /* 0x3DD0B461, 0x1A600000 */
    /// pi/2 - (PIO2_1+PIO2_2)
    const PIO2_2T: f64 = 2.02226624879595063154e-21; /* 0x3BA3198A, 0x2E037073 */
    /// third 33 bits of pi/2
    const PIO2_3: f64 = 2.02226624871116645580e-21; /* 0x3BA3198A, 0x2E000000 */
    /// pi/2 - (PIO2_1+PIO2_2+PIO2_3)
    const PIO2_3T: f64 = 8.47842766036889956997e-32; /* 0x397B839A, 0x252049C1 */
    const NPIO2_HW: [u32; 32] = [
        0x3FF921FB, 0x400921FB, 0x4012D97C, 0x401921FB, 0x401F6A7A, 0x4022D97C, 0x4025FDBB,
        0x402921FB, 0x402C463A, 0x402F6A7A, 0x4031475C, 0x4032D97C, 0x40346B9C, 0x4035FDBB,
        0x40378FDB, 0x403921FB, 0x403AB41B, 0x403C463A, 0x403DD85A, 0x403F6A7A, 0x40407E4C,
        0x4041475C, 0x4042106C, 0x4042D97C, 0x4043A28C, 0x40446B9C, 0x404534AC, 0x4045FDBB,
        0x4046C6CB, 0x40478FDB, 0x404858EB, 0x404921FB,
    ];

    let hx = (f64::to_bits(x) >> 32) as i32;
    let ix = (hx & 0x7fffffff) as u32;

    if ix < 0x4002D97C {
        /* |x| < 3pi/4, special case with n=+-1 */
        if hx > 0 {
            let mut z = x - PIO2_1;
            if ix != 0x3FF921FB {
                /* 33+53 bit pi is good enough */
                let y0 = z - PIO2_1T;
                let y1 = (z - y0) - PIO2_1T;
                return (1, y0, y1);
            } else {
                /* near pi/2, use 33+33+53 bit pi */
                z -= PIO2_2;
                let y0 = z - PIO2_2T;
                let y1 = (z - y0) - PIO2_2T;
                return (1, y0, y1);
            }
        } else {
            /* negative x */
            let mut z = x + PIO2_1;
            if ix != 0x3FF921FB {
                let y0 = z + PIO2_1T;
                let y1 = (z - y0) + PIO2_1T;
                return (-1, y0, y1);
            } else {
                z += PIO2_2;
                let y0 = z + PIO2_2T;
                let y1 = (z - y0) + PIO2_2T;
                return (-1, y0, y1);
            }
        }
    }
    /* |x| ~<= 2^19*(pi/2), medium size (the caller excludes larger x) */
    let t = x.abs();
    let n = t.mul_add(INV_PIO2, HALF) as i32;
    let f_n = n as f64;
    let mut r = (-f_n).mul_add(PIO2_1, t); /* t - fn*pio2_1 (exact product) */
    let mut w = f_n * PIO2_1T; /* 1st round good to 85 bits */
    let mut y0;
    if n < 32 && ix != NPIO2_HW[(n - 1) as usize] {
        y0 = r - w; /* quick check no cancellation */
    } else {
        let j = (ix >> 20) as i32;
        y0 = r - w;
        let high = (f64::to_bits(y0) >> 52) as i32 & 0x7ff;
        if j - high > 16 {
            /* 2nd iteration needed, good to 118 */
            let t2 = r;
            w = f_n * PIO2_2;
            r = t2 - w;
            w = f_n.mul_add(PIO2_2T, -((t2 - r) - w));
            y0 = r - w;
            let high = (f64::to_bits(y0) >> 52) as i32 & 0x7ff;
            if j - high > 49 {
                /* 3rd iteration needed, 151 bits acc */
                let t3 = r;
                w = f_n * PIO2_3;
                r = t3 - w;
                w = f_n.mul_add(PIO2_3T, -((t3 - r) - w));
                y0 = r - w;
            }
        }
    }
    let y1 = (r - y0) - w;
    if hx < 0 {
        (-n, -y0, -y1)
    } else {
        (n, y0, y1)
    }
}

/// fdlibm/V8 `Math.tan`.
fn js_tan(x: f64) -> f64 {
    let ix = (f64::to_bits(x) >> 32) as u32 & 0x7fffffff;
    /* |x| ~< pi/4 */
    if ix <= 0x3FE921FB {
        return k_tan(x, 0.0, 1);
    }
    /* tan(Inf or NaN) is NaN */
    if ix >= 0x7FF00000 {
        return x - x;
    }
    if ix > 0x413921FB {
        /* beyond 2^20*(pi/2): host-libm fallback (see module comment) */
        return x.tan();
    }
    /* argument reduction */
    let (n, y0, y1) = rem_pio2(x);
    /* 1 -> n even, -1 -> n odd */
    k_tan(y0, y1, 1 - ((n & 1) << 1))
}

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
    rt_float(js_tan(num(x)))
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
    for x in set_elems(s) {
        if truthy(ap1(f, x)) {
            yes.push((x, 0));
        } else {
            no.push((x, 0));
        }
    }
    pair(mk_set(tbuild(&yes)), mk_set(tbuild(&no)))
}
#[no_mangle]
pub unsafe extern "C" fn array_append(a: u64, b: u64) -> u64 {
    let mut els = arr_elems(a);
    els.extend(arr_elems(b));
    mk_array(tbuild_vals(&els))
}
#[no_mangle]
pub unsafe extern "C" fn array_to_indexed_list(a: u64) -> u64 {
    let items: Vec<u64> = arr_elems(a)
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

// PERSISTENT WEIGHT-BALANCED TREE (Adams' variant, delta=3 gamma=2), keyed by
// `value_cmp`. Backs both Dict and Set. Nodes are immutable and bump-allocated;
// an update path-copies the O(log n) nodes on the search path and shares the
// rest, so a fold that builds N entries allocates O(N log N) nodes rather than
// copying an O(n) array N times (O(N²)). Not freed yet — reference counting is
// a later stage; this stage removes the per-update copy that caused the OOMs.

struct TNode {
    key: u64,
    val: u64,
    size: u32,
    left: u64,
    right: u64,
}
const WBT_DELTA: u32 = 3;
const WBT_GAMMA: u32 = 2;

#[inline]
unsafe fn tref<'a>(n: u64) -> &'a TNode {
    &*(n as *const TNode)
}
#[inline]
unsafe fn tsize(n: u64) -> u32 {
    if n == 0 {
        0
    } else {
        tref(n).size
    }
}
unsafe fn tnode(key: u64, val: u64, left: u64, right: u64) -> u64 {
    let size = 1 + tsize(left) + tsize(right);
    Box::into_raw(Box::new(TNode { key, val, size, left, right })) as u64
}
unsafe fn t_single_l(k: u64, v: u64, l: u64, r: u64) -> u64 {
    let rr = tref(r);
    tnode(rr.key, rr.val, tnode(k, v, l, rr.left), rr.right)
}
unsafe fn t_single_r(k: u64, v: u64, l: u64, r: u64) -> u64 {
    let ll = tref(l);
    tnode(ll.key, ll.val, ll.left, tnode(k, v, ll.right, r))
}
unsafe fn t_double_l(k: u64, v: u64, l: u64, r: u64) -> u64 {
    let (rr, rl) = (tref(r), tref(tref(r).left));
    tnode(
        rl.key,
        rl.val,
        tnode(k, v, l, rl.left),
        tnode(rr.key, rr.val, rl.right, rr.right),
    )
}
unsafe fn t_double_r(k: u64, v: u64, l: u64, r: u64) -> u64 {
    let (ll, lr) = (tref(l), tref(tref(l).right));
    tnode(
        lr.key,
        lr.val,
        tnode(ll.key, ll.val, ll.left, lr.left),
        tnode(k, v, lr.right, r),
    )
}
unsafe fn tbalance(k: u64, v: u64, l: u64, r: u64) -> u64 {
    let (ln, rn) = (tsize(l), tsize(r));
    if ln + rn <= 1 {
        tnode(k, v, l, r)
    } else if rn > WBT_DELTA * ln {
        let rr = tref(r);
        if tsize(rr.left) < WBT_GAMMA * tsize(rr.right) {
            t_single_l(k, v, l, r)
        } else {
            t_double_l(k, v, l, r)
        }
    } else if ln > WBT_DELTA * rn {
        let ll = tref(l);
        if tsize(ll.right) < WBT_GAMMA * tsize(ll.left) {
            t_single_r(k, v, l, r)
        } else {
            t_double_r(k, v, l, r)
        }
    } else {
        tnode(k, v, l, r)
    }
}
unsafe fn tinsert(n: u64, k: u64, v: u64) -> u64 {
    if n == 0 {
        return tnode(k, v, 0, 0);
    }
    let nd = tref(n);
    match value_cmp(k, nd.key) {
        c if c < 0 => tbalance(nd.key, nd.val, tinsert(nd.left, k, v), nd.right),
        c if c > 0 => tbalance(nd.key, nd.val, nd.left, tinsert(nd.right, k, v)),
        _ => tnode(k, v, nd.left, nd.right),
    }
}
unsafe fn tfind(n: u64, k: u64) -> Option<u64> {
    let mut cur = n;
    while cur != 0 {
        let nd = tref(cur);
        match value_cmp(k, nd.key) {
            c if c < 0 => cur = nd.left,
            c if c > 0 => cur = nd.right,
            _ => return Some(nd.val),
        }
    }
    None
}
unsafe fn t_min(n: u64) -> (u64, u64) {
    let mut cur = n;
    loop {
        let nd = tref(cur);
        if nd.left == 0 {
            return (nd.key, nd.val);
        }
        cur = nd.left;
    }
}
unsafe fn t_delmin(n: u64) -> u64 {
    let nd = tref(n);
    if nd.left == 0 {
        nd.right
    } else {
        tbalance(nd.key, nd.val, t_delmin(nd.left), nd.right)
    }
}
unsafe fn t_glue(l: u64, r: u64) -> u64 {
    if l == 0 {
        r
    } else if r == 0 {
        l
    } else {
        let (mk, mv) = t_min(r);
        tbalance(mk, mv, l, t_delmin(r))
    }
}
unsafe fn tremove(n: u64, k: u64) -> u64 {
    if n == 0 {
        return 0;
    }
    let nd = tref(n);
    match value_cmp(k, nd.key) {
        c if c < 0 => tbalance(nd.key, nd.val, tremove(nd.left, k), nd.right),
        c if c > 0 => tbalance(nd.key, nd.val, nd.left, tremove(nd.right, k)),
        _ => t_glue(nd.left, nd.right),
    }
}
unsafe fn tcollect(n: u64, out: &mut Vec<(u64, u64)>) {
    if n == 0 {
        return;
    }
    let nd = tref(n);
    tcollect(nd.left, out);
    out.push((nd.key, nd.val));
    tcollect(nd.right, out);
}
/// A balanced tree from already-sorted, deduplicated pairs, in O(n).
unsafe fn tbuild(items: &[(u64, u64)]) -> u64 {
    if items.is_empty() {
        return 0;
    }
    let mid = items.len() / 2;
    let (k, v) = items[mid];
    tnode(k, v, tbuild(&items[..mid]), tbuild(&items[mid + 1..]))
}
unsafe fn tmap(n: u64, f: u64) -> u64 {
    if n == 0 {
        return 0;
    }
    let nd = tref(n);
    // Post-order-ish; keys and shape unchanged, only values remapped.
    let left = tmap(nd.left, f);
    let val = ap2(f, nd.key, nd.val);
    let right = tmap(nd.right, f);
    tnode(nd.key, val, left, right)
}

// Distinct accessors per variant: keeping them separate (rather than one that
// matches both Dict and Set) is also what stops LLVM's function-merging pass
// from folding the identical-looking set_*/dict_* wrappers into thunks with
// external linkage — which duplicated symbols against the runtime archive.
#[inline]
unsafe fn dict_root(v: u64) -> u64 {
    match deref(v) {
        Value::Dict(r) => *r,
        _ => 0,
    }
}
#[inline]
unsafe fn set_root(v: u64) -> u64 {
    match deref(v) {
        Value::Set(r) => *r,
        _ => 0,
    }
}
unsafe fn troot_pairs(root: u64) -> Vec<(u64, u64)> {
    let mut out = Vec::with_capacity(tsize(root) as usize);
    tcollect(root, &mut out);
    out
}
unsafe fn dict_pairs(v: u64) -> Vec<(u64, u64)> {
    troot_pairs(dict_root(v))
}
unsafe fn set_elems(v: u64) -> Vec<u64> {
    troot_pairs(set_root(v)).into_iter().map(|(k, _)| k).collect()
}
#[inline]
unsafe fn mk_dict(root: u64) -> u64 {
    alloc(Value::Dict(root))
}
#[inline]
unsafe fn mk_set(root: u64) -> u64 {
    alloc(Value::Set(root))
}

// Array = a positional weight-balanced tree (same nodes as Dict/Set, but keyed
// by in-order rank rather than `value_cmp`; the `key` slot is unused). Reuses
// `tbalance`/`tnode`/`tsize` — WBT rotations preserve in-order order, so they
// rebalance a positional tree correctly.
unsafe fn tget_rank(n: u64, mut i: u32) -> u64 {
    let mut cur = n;
    loop {
        let nd = tref(cur);
        let ls = tsize(nd.left);
        if i < ls {
            cur = nd.left;
        } else if i == ls {
            return nd.val;
        } else {
            i -= ls + 1;
            cur = nd.right;
        }
    }
}
unsafe fn tset_rank(n: u64, i: u32, v: u64) -> u64 {
    let nd = tref(n);
    let ls = tsize(nd.left);
    if i < ls {
        tnode(0, nd.val, tset_rank(nd.left, i, v), nd.right)
    } else if i == ls {
        tnode(0, v, nd.left, nd.right)
    } else {
        tnode(0, nd.val, nd.left, tset_rank(nd.right, i - ls - 1, v))
    }
}
unsafe fn tpush_right(n: u64, v: u64) -> u64 {
    if n == 0 {
        return tnode(0, v, 0, 0);
    }
    let nd = tref(n);
    tbalance(0, nd.val, nd.left, tpush_right(nd.right, v))
}
unsafe fn tcollect_vals(n: u64, out: &mut Vec<u64>) {
    if n == 0 {
        return;
    }
    let nd = tref(n);
    tcollect_vals(nd.left, out);
    out.push(nd.val);
    tcollect_vals(nd.right, out);
}
unsafe fn tbuild_vals(items: &[u64]) -> u64 {
    if items.is_empty() {
        return 0;
    }
    let mid = items.len() / 2;
    tnode(0, items[mid], tbuild_vals(&items[..mid]), tbuild_vals(&items[mid + 1..]))
}
#[inline]
unsafe fn arr_root(v: u64) -> u64 {
    match deref(v) {
        Value::Array(r) => *r,
        _ => 0,
    }
}
unsafe fn arr_elems(v: u64) -> Vec<u64> {
    let root = arr_root(v);
    let mut out = Vec::with_capacity(tsize(root) as usize);
    tcollect_vals(root, &mut out);
    out
}
#[inline]
unsafe fn mk_array(root: u64) -> u64 {
    alloc(Value::Array(root))
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
    mk_dict(tnode(k, v, 0, 0))
}
#[no_mangle]
pub unsafe extern "C" fn dict_insert(k: u64, v: u64, d: u64) -> u64 {
    mk_dict(tinsert(dict_root(d), k, v))
}
#[no_mangle]
pub unsafe extern "C" fn dict_get(k: u64, d: u64) -> u64 {
    match tfind(dict_root(d), k) {
        Some(v) => just(v),
        None => nothing(),
    }
}
#[no_mangle]
pub unsafe extern "C" fn dict_remove(k: u64, d: u64) -> u64 {
    mk_dict(tremove(dict_root(d), k))
}
#[no_mangle]
pub unsafe extern "C" fn dict_member(k: u64, d: u64) -> u64 {
    mkbool(tfind(dict_root(d), k).is_some())
}
#[no_mangle]
pub unsafe extern "C" fn dict_is_empty(d: u64) -> u64 {
    mkbool(dict_root(d) == 0)
}
#[no_mangle]
pub unsafe extern "C" fn dict_size(d: u64) -> u64 {
    mk_int(tsize(dict_root(d)) as i64)
}
#[no_mangle]
pub unsafe extern "C" fn dict_keys(d: u64) -> u64 {
    let ks: Vec<u64> = dict_pairs(d).iter().map(|(k, _)| *k).collect();
    list_from_slice(&ks)
}
#[no_mangle]
pub unsafe extern "C" fn dict_values(d: u64) -> u64 {
    let vs: Vec<u64> = dict_pairs(d).iter().map(|(_, v)| *v).collect();
    list_from_slice(&vs)
}
#[no_mangle]
pub unsafe extern "C" fn dict_to_list(d: u64) -> u64 {
    let items: Vec<u64> = dict_pairs(d).iter().map(|(k, v)| pair(*k, *v)).collect();
    list_from_slice(&items)
}
#[no_mangle]
pub unsafe extern "C" fn dict_from_list(list: u64) -> u64 {
    let mut root = 0u64;
    // Head-first: later entries override earlier (Elm semantics).
    for p in to_vec(list) {
        if let Value::Tuple(kv) = deref(p) {
            root = tinsert(root, kv[0], kv[1]);
        }
    }
    mk_dict(root)
}
#[no_mangle]
pub unsafe extern "C" fn dict_foldl(f: u64, init: u64, d: u64) -> u64 {
    let mut acc = init;
    for &(k, v) in &dict_pairs(d) {
        acc = ap3(f, k, v, acc);
    }
    acc
}
#[no_mangle]
pub unsafe extern "C" fn dict_foldr(f: u64, init: u64, d: u64) -> u64 {
    let mut acc = init;
    for &(k, v) in dict_pairs(d).iter().rev() {
        acc = ap3(f, k, v, acc);
    }
    acc
}
#[no_mangle]
pub unsafe extern "C" fn dict_map(f: u64, d: u64) -> u64 {
    mk_dict(tmap(dict_root(d), f))
}
#[no_mangle]
pub unsafe extern "C" fn dict_filter(f: u64, d: u64) -> u64 {
    let kept: Vec<(u64, u64)> = dict_pairs(d)
        .into_iter()
        .filter(|&(k, v)| truthy(ap2(f, k, v)))
        .collect();
    mk_dict(tbuild(&kept))
}
#[no_mangle]
pub unsafe extern "C" fn dict_update(k: u64, f: u64, d: u64) -> u64 {
    let current = dict_get(k, d);
    let result = ap1(f, current);
    match deref(result) {
        Value::Ctor { index: 0, .. } => dict_insert(k, ctor_get(result, 0), d),
        _ => dict_remove(k, d),
    }
}
#[no_mangle]
pub unsafe extern "C" fn dict_union(a: u64, b: u64) -> u64 {
    // Keep all keys; on conflict `a` wins (insert a's entries over b).
    let mut root = dict_root(b);
    for &(k, v) in &dict_pairs(a) {
        root = tinsert(root, k, v);
    }
    mk_dict(root)
}
#[no_mangle]
pub unsafe extern "C" fn dict_intersect(a: u64, b: u64) -> u64 {
    let broot = dict_root(b);
    let kept: Vec<(u64, u64)> = dict_pairs(a)
        .into_iter()
        .filter(|&(k, _)| tfind(broot, k).is_some())
        .collect();
    mk_dict(tbuild(&kept))
}
#[no_mangle]
pub unsafe extern "C" fn dict_diff(a: u64, b: u64) -> u64 {
    let broot = dict_root(b);
    let kept: Vec<(u64, u64)> = dict_pairs(a)
        .into_iter()
        .filter(|&(k, _)| tfind(broot, k).is_none())
        .collect();
    mk_dict(tbuild(&kept))
}
#[no_mangle]
pub unsafe extern "C" fn dict_partition(f: u64, d: u64) -> u64 {
    let mut yes = Vec::new();
    let mut no = Vec::new();
    for (k, v) in dict_pairs(d) {
        if truthy(ap2(f, k, v)) {
            yes.push((k, v));
        } else {
            no.push((k, v));
        }
    }
    pair(mk_dict(tbuild(&yes)), mk_dict(tbuild(&no)))
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
    // Collect first: the step closures allocate.
    let la = dict_pairs(left);
    let ra = dict_pairs(right);
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
    mk_set(tnode(x, 0, 0, 0))
}
#[no_mangle]
pub unsafe extern "C" fn set_insert(x: u64, s: u64) -> u64 {
    mk_set(tinsert(set_root(s), x, 0))
}
#[no_mangle]
pub unsafe extern "C" fn set_remove(x: u64, s: u64) -> u64 {
    mk_set(tremove(set_root(s), x))
}
#[no_mangle]
pub unsafe extern "C" fn set_member(x: u64, s: u64) -> u64 {
    mkbool(tfind(set_root(s), x).is_some())
}
#[no_mangle]
pub unsafe extern "C" fn set_is_empty(s: u64) -> u64 {
    mkbool(set_root(s) == 0)
}
#[no_mangle]
pub unsafe extern "C" fn set_size(s: u64) -> u64 {
    mk_int(tsize(set_root(s)) as i64)
}
#[no_mangle]
pub unsafe extern "C" fn set_to_list(s: u64) -> u64 {
    list_from_slice(&set_elems(s))
}
#[no_mangle]
pub unsafe extern "C" fn set_from_list(list: u64) -> u64 {
    let mut root = 0u64;
    for x in to_vec(list) {
        root = tinsert(root, x, 0);
    }
    mk_set(root)
}
#[no_mangle]
pub unsafe extern "C" fn set_union(a: u64, b: u64) -> u64 {
    let mut root = set_root(b);
    for &x in &set_elems(a) {
        root = tinsert(root, x, 0);
    }
    mk_set(root)
}
#[no_mangle]
pub unsafe extern "C" fn set_intersect(a: u64, b: u64) -> u64 {
    let broot = set_root(b);
    let kept: Vec<(u64, u64)> = set_elems(a)
        .into_iter()
        .filter(|&x| tfind(broot, x).is_some())
        .map(|x| (x, 0))
        .collect();
    mk_set(tbuild(&kept))
}
#[no_mangle]
pub unsafe extern "C" fn set_diff(a: u64, b: u64) -> u64 {
    let broot = set_root(b);
    let kept: Vec<(u64, u64)> = set_elems(a)
        .into_iter()
        .filter(|&x| tfind(broot, x).is_none())
        .map(|x| (x, 0))
        .collect();
    mk_set(tbuild(&kept))
}
#[no_mangle]
pub unsafe extern "C" fn set_foldl(f: u64, init: u64, s: u64) -> u64 {
    let mut acc = init;
    for &x in &set_elems(s) {
        acc = ap2(f, x, acc);
    }
    acc
}
#[no_mangle]
pub unsafe extern "C" fn set_foldr(f: u64, init: u64, s: u64) -> u64 {
    let mut acc = init;
    for &x in set_elems(s).iter().rev() {
        acc = ap2(f, x, acc);
    }
    acc
}
#[no_mangle]
pub unsafe extern "C" fn set_map(f: u64, s: u64) -> u64 {
    let mut root = 0u64;
    for &x in &set_elems(s) {
        root = tinsert(root, ap1(f, x), 0);
    }
    mk_set(root)
}
#[no_mangle]
pub unsafe extern "C" fn set_filter(f: u64, s: u64) -> u64 {
    let kept: Vec<(u64, u64)> = set_elems(s)
        .into_iter()
        .filter(|&x| truthy(ap1(f, x)))
        .map(|x| (x, 0))
        .collect();
    mk_set(tbuild(&kept))
}

// -- Array --

#[no_mangle]
pub unsafe extern "C" fn array_is_empty(a: u64) -> u64 {
    mkbool(arr_root(a) == 0)
}
#[no_mangle]
pub unsafe extern "C" fn array_length(a: u64) -> u64 {
    mk_int(tsize(arr_root(a)) as i64)
}
#[no_mangle]
pub unsafe extern "C" fn array_initialize(n: u64, f: u64) -> u64 {
    let n = int_val(n).max(0);
    let els: Vec<u64> = (0..n).map(|i| ap1(f, mk_int(i))).collect();
    mk_array(tbuild_vals(&els))
}
#[no_mangle]
pub unsafe extern "C" fn array_repeat(n: u64, x: u64) -> u64 {
    let n = int_val(n).max(0) as usize;
    mk_array(tbuild_vals(&vec![x; n]))
}
#[no_mangle]
pub unsafe extern "C" fn array_from_list(list: u64) -> u64 {
    let els: Vec<u64> = to_vec(list);
    mk_array(tbuild_vals(&els))
}
#[no_mangle]
pub unsafe extern "C" fn array_to_list(a: u64) -> u64 {
    list_from_slice(&arr_elems(a))
}
#[no_mangle]
pub unsafe extern "C" fn array_get(i: u64, a: u64) -> u64 {
    let root = arr_root(a);
    let i = int_val(i);
    if i >= 0 && (i as u32 as i64) == i && (i as u32) < tsize(root) {
        just(tget_rank(root, i as u32))
    } else {
        nothing()
    }
}
#[no_mangle]
pub unsafe extern "C" fn array_set(i: u64, v: u64, a: u64) -> u64 {
    let root = arr_root(a);
    let i = int_val(i);
    if i >= 0 && (i as u32 as i64) == i && (i as u32) < tsize(root) {
        mk_array(tset_rank(root, i as u32, v))
    } else {
        mk_array(root)
    }
}
#[no_mangle]
pub unsafe extern "C" fn array_push(v: u64, a: u64) -> u64 {
    mk_array(tpush_right(arr_root(a), v))
}
#[no_mangle]
pub unsafe extern "C" fn array_foldl(f: u64, init: u64, a: u64) -> u64 {
    let mut acc = init;
    for &x in &arr_elems(a) {
        acc = ap2(f, x, acc);
    }
    acc
}
#[no_mangle]
pub unsafe extern "C" fn array_foldr(f: u64, init: u64, a: u64) -> u64 {
    let mut acc = init;
    for &x in arr_elems(a).iter().rev() {
        acc = ap2(f, x, acc);
    }
    acc
}
#[no_mangle]
pub unsafe extern "C" fn array_map(f: u64, a: u64) -> u64 {
    let els: Vec<u64> = arr_elems(a).iter().map(|&x| ap1(f, x)).collect();
    mk_array(tbuild_vals(&els))
}
#[no_mangle]
pub unsafe extern "C" fn array_indexed_map(f: u64, a: u64) -> u64 {
    let els: Vec<u64> = arr_elems(a)
        .iter()
        .enumerate()
        .map(|(i, &x)| ap2(f, mk_int(i as i64), x))
        .collect();
    mk_array(tbuild_vals(&els))
}
#[no_mangle]
pub unsafe extern "C" fn array_filter(f: u64, a: u64) -> u64 {
    let els: Vec<u64> = arr_elems(a).into_iter().filter(|&x| truthy(ap1(f, x))).collect();
    mk_array(tbuild_vals(&els))
}
#[no_mangle]
pub unsafe extern "C" fn array_slice(from: u64, to: u64, a: u64) -> u64 {
    let arr = arr_elems(a);
    let len = arr.len() as i64;
    let norm = |i: i64| -> i64 {
        let i = if i < 0 { len + i } else { i };
        i.clamp(0, len)
    };
    let (lo, hi) = (norm(int_val(from)), norm(int_val(to)));
    let els = if lo < hi {
        &arr[lo as usize..hi as usize]
    } else {
        &[][..]
    };
    mk_array(tbuild_vals(els))
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
#[allow(dead_code)] // used on the wasm sentinel path only
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

// Bytes.Decode failure — non-local exit, the twin of the JS runtime throwing
// from `_Bytes_read_*` and `_Bytes_decode` catching. elm/bytes' combinators
// (`map`/`andThen`/`loop`, plain Elm) apply user callbacks unconditionally,
// relying on a failed read aborting the whole decode; returning a sentinel
// instead would hand a dummy value to code that inspects it at an arbitrary
// (unboxed) layout — dereferencing a tagged int as a pointer. The jump lives in
// a C shim (`bytes_jmp.c`) because `setjmp` needs `returns_twice` codegen.
//
// Wasm has no shim linked (and no longjmp), so it keeps the sentinel encoding —
// `(BYTES_FAIL_OFFSET, dummy)` — which is correct on the uniform (boxed) wasm
// path where the dummy is never read at a concrete layout.
#[cfg(not(target_arch = "wasm32"))]
extern "C" {
    fn alm_bytes_try(
        run: unsafe extern "C" fn(u64, u64) -> u64,
        decoder: u64,
        bytes: u64,
        failed: *mut u64,
    ) -> u64;
    fn alm_bytes_fail() -> !;
}

#[cfg(not(target_arch = "wasm32"))]
unsafe fn bytes_fail(_dummy: impl FnOnce() -> u64) -> u64 {
    alm_bytes_fail()
}
#[cfg(target_arch = "wasm32")]
unsafe fn bytes_fail(dummy: impl FnOnce() -> u64) -> u64 {
    pair(mk_int(BYTES_FAIL_OFFSET), dummy())
}

#[no_mangle]
pub unsafe extern "C" fn bytes_read_i8(b: u64, off: u64) -> u64 {
    let off = int_val(off);
    match read_at(b, off, 1) {
        Some(s) => read_result(off, 1, mk_int(s[0] as i8 as i64)),
        None => bytes_fail(|| mk_int(0)),
    }
}
#[no_mangle]
pub unsafe extern "C" fn bytes_read_u8(b: u64, off: u64) -> u64 {
    let off = int_val(off);
    match read_at(b, off, 1) {
        Some(s) => read_result(off, 1, mk_int(s[0] as i64)),
        None => bytes_fail(|| mk_int(0)),
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
                None => bytes_fail(|| mk_int(0)),
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
                None => bytes_fail(|| rt_float(0.0)),
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
        None => bytes_fail(|| alloc(Value::Bytes(Vec::new()))),
    }
}

#[no_mangle]
pub unsafe extern "C" fn bytes_read_string(len: u64, b: u64, off: u64) -> u64 {
    // Exact port of elm/bytes' JS `_Bytes_read_string`: decode UTF-8 by
    // hand, where continuation bytes are read PAST the requested slice (a
    // multi-byte sequence near the end advances the offset beyond
    // `off + len`), and a read past the BUFFER end fails the whole decode
    // (JS: DataView RangeError caught by `_Bytes_decode`). A plain
    // "copy the slice" version silently accepted truncated/invalid
    // sequences that JS rejects (bbase64's InvalidByteSequence tests).
    let off0 = int_val(off);
    let len = int_val(len).max(0) as usize;
    let buf = as_bytes(b);
    if off0 < 0 {
        return bytes_fail(|| mkstr(Vec::new()));
    }
    let mut o = off0 as usize;
    let end = o + len;
    // Fast path: a byte range that is entire, in-bounds, valid UTF-8 — the
    // overwhelmingly common case — is a straight copy (matches JS, which
    // yields the same string when every sequence decodes cleanly).
    if end <= buf.len() {
        if let Ok(v) = std::str::from_utf8(&buf[o..end]) {
            return read_result(off0, len, mkstr(v.as_bytes().to_vec()));
        }
    }
    let mut out = String::new();
    macro_rules! next {
        () => {{
            if o >= buf.len() {
                return bytes_fail(|| mkstr(Vec::new()));
            }
            let v = buf[o];
            o += 1;
            v as u32
        }};
    }
    while o < end {
        let byte = next!();
        let cp = if byte < 128 {
            byte
        } else if byte & 0xE0 == 0xC0 {
            (byte & 0x1F) << 6 | (next!() & 0x3F)
        } else if byte & 0xF0 == 0xE0 {
            (byte & 0xF) << 12 | (next!() & 0x3F) << 6 | (next!() & 0x3F)
        } else {
            (byte & 0x7) << 18 | (next!() & 0x3F) << 12 | (next!() & 0x3F) << 6 | (next!() & 0x3F)
        };
        // JS builds UTF-16 code units (garbage sequences can yield lone
        // surrogates); UTF-8 storage renders those as replacement chars,
        // like a JS string printed to the outside world.
        out.push(char::from_u32(cp).unwrap_or('\u{fffd}'));
    }
    read_result(off0, o - off0 as usize, mkstr(out.into_bytes()))
}

/// `Bytes.Decode.fail` — always fails.
#[no_mangle]
pub unsafe extern "C" fn bytes_decode_failure(_b: u64, _off: u64) -> u64 {
    bytes_fail(|| mk_int(0))
}

unsafe extern "C" fn bytes_run_decoder(decoder: u64, bytes: u64) -> u64 {
    ap2(decoder, bytes, mk_int(0))
}

#[no_mangle]
pub unsafe extern "C" fn bytes_decode(decoder: u64, bytes: u64) -> u64 {
    #[cfg(not(target_arch = "wasm32"))]
    let result = {
        let mut failed: u64 = 0;
        let r = alm_bytes_try(bytes_run_decoder, decoder, bytes, &mut failed);
        if failed != 0 {
            return nothing();
        }
        r
    };
    #[cfg(target_arch = "wasm32")]
    let result = bytes_run_decoder(decoder, bytes);
    // `result` is `(offset, value)`; a negative offset means a read ran past
    // the end (wasm's sentinel path — a native failure longjmp'd above).
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
        Value::StrCat { .. } | Value::StrSlice { .. } => sbytes(w).to_vec(),
        _ => {
            eprintln!("alm: expected a String, found {}", variant_name(w));
            crash!("expected a String")
        }
    }
}

/// A String's UTF-8 bytes, borrowed (values are never freed, so 'a is sound).
unsafe fn str_slice<'a>(w: u64) -> &'a [u8] {
    match deref(w) {
        Value::Str(b) => b,
        Value::StrCat { .. } | Value::StrSlice { .. } => sbytes(w),
        _ => {
            eprintln!("alm: expected a String, found {}", variant_name(w));
            if std::env::var("ALM_CRASH_BT").is_ok() {
                eprintln!("{}", std::backtrace::Backtrace::force_capture());
            }
            crash!("expected a String")
        }
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
    // Offsets are CODEPOINT indices (consistent with `String.slice`, which
    // elm/parser's `getChompedString` uses): a byte-offset scheme would slice
    // the wrong substring for multi-byte input. `char_byte` maps the codepoint
    // offset to a byte position; the returned offset is again a codepoint index.
    let s = str_slice(small);
    let b = str_slice(big);
    let co = int_val(offset) as usize;
    let mut bo = parser_cp_byte(big, b, co);
    let mut new_co = co;
    let mut r = int_val(row);
    let mut c = int_val(col);
    let mut si = 0usize;
    let mut good = true;
    while si < s.len() {
        if bo >= b.len() {
            good = false;
            break;
        }
        let (sc, sl) = decode_char(s, si);
        let (bc, bl) = decode_char(b, bo);
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
        bo += bl;
        si += sl;
        new_co += 1;
    }
    triple(rt_int(if good { new_co as i64 } else { -1 }), rt_int(r), rt_int(c))
}

// All parser offsets below are CODEPOINT indices (see `parser_is_sub_string`):
// `char_byte` maps a codepoint offset to a byte position; digits/ASCII are one
// byte each so the codepoint offset advances by the number matched.
unsafe extern "C" fn parser_is_sub_char(predicate: u64, offset: u64, string: u64) -> u64 {
    let s = str_slice(string);
    let co = int_val(offset) as usize;
    let bo = parser_cp_byte(string, s, co);
    if s.len() <= bo {
        return rt_int(-1);
    }
    let (cp, _len) = decode_char(s, bo);
    if !rt_is_true(ap1(predicate, alloc(Value::Char(cp)))) {
        return rt_int(-1);
    }
    if cp == 0x0A {
        rt_int(-2)
    } else {
        rt_int((co + 1) as i64)
    }
}

unsafe extern "C" fn parser_is_ascii_code(code: u64, offset: u64, string: u64) -> u64 {
    let s = str_slice(string);
    let bo = parser_cp_byte(string, s, int_val(offset) as usize);
    let byte = if bo < s.len() { s[bo] as i64 } else { -1 };
    rt_bool(byte == int_val(code))
}

unsafe extern "C" fn parser_chomp_base10(offset: u64, string: u64) -> u64 {
    let s = str_slice(string);
    let co = int_val(offset) as usize;
    let mut bo = parser_cp_byte(string, s, co);
    let mut n = co;
    while bo < s.len() && (0x30..=0x39).contains(&s[bo]) {
        bo += 1;
        n += 1;
    }
    rt_int(n as i64)
}

unsafe extern "C" fn parser_consume_base(base: u64, offset: u64, string: u64) -> u64 {
    let s = str_slice(string);
    let base = int_val(base);
    let co = int_val(offset) as usize;
    let mut bo = parser_cp_byte(string, s, co);
    let mut n = co;
    let mut total: i64 = 0;
    while bo < s.len() {
        let digit = s[bo] as i64 - 0x30;
        if digit < 0 || base <= digit {
            break;
        }
        total = base * total + digit;
        bo += 1;
        n += 1;
    }
    pair(rt_int(n as i64), rt_int(total))
}

unsafe extern "C" fn parser_consume_base16(offset: u64, string: u64) -> u64 {
    let s = str_slice(string);
    let co = int_val(offset) as usize;
    let mut bo = parser_cp_byte(string, s, co);
    let mut n = co;
    let mut total: i64 = 0;
    while bo < s.len() {
        let code = s[bo];
        let d = match code {
            0x30..=0x39 => (code - 0x30) as i64,
            0x41..=0x46 => (code - 55) as i64,
            0x61..=0x66 => (code - 87) as i64,
            _ => break,
        };
        total = 16 * total + d;
        bo += 1;
        n += 1;
    }
    pair(rt_int(n as i64), rt_int(total))
}

unsafe extern "C" fn parser_find_sub_string(
    small: u64,
    offset: u64,
    row: u64,
    col: u64,
    big: u64,
) -> u64 {
    // Codepoint offsets (see `parser_is_sub_string`). Find `small` by bytes,
    // then report the match position as a codepoint offset.
    let s = str_slice(small);
    let b = str_slice(big);
    let o0 = int_val(offset) as usize;
    let bo0 = parser_cp_byte(big, b, o0);
    let bmatch: i64 = if s.is_empty() {
        bo0.min(b.len()) as i64
    } else if bo0 <= b.len() {
        b[bo0..]
            .windows(s.len())
            .position(|w| w == s)
            .map(|p| (bo0 + p) as i64)
            .unwrap_or(-1)
    } else {
        -1
    };
    let new_offset: i64 = if bmatch < 0 {
        -1
    } else {
        (o0 + cp_count(&b[bo0..bmatch as usize])) as i64
    };
    let target = if bmatch < 0 {
        b.len()
    } else {
        bmatch as usize + s.len()
    };
    let mut o = bo0;
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
    let out: Vec<JsonValue> = arr_elems(arr)
        .into_iter()
        .map(|x| as_json(ap1(f, x)).clone())
        .collect();
    mk_json(JsonValue::JArray(out))
}
unsafe extern "C" fn encode_set(f: u64, set: u64) -> u64 {
    let out: Vec<JsonValue> = set_elems(set)
        .into_iter()
        .map(|x| as_json(ap1(f, x)).clone())
        .collect();
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
    let entries = dict_pairs(dict);
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
        // An embedded Elm value has no JSON serialization (it is never encoded;
        // HtmlAsJson trees are decoded, not stringified). Emit null defensively.
        JsonValue::Elm(_) => out.push_str("null"),
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

// --- elm/regex kernels (engine via the alm-regex glue) ---

unsafe fn regex_ptr(re: u64) -> *const core::ffi::c_void {
    match deref(re) {
        Value::Regex(p) => *p,
        _ => std::ptr::null(),
    }
}

/// Slice `s` to the codepoint range `[cstart, cend)` as a new string value.
unsafe fn str_char_slice(s: &str, cstart: i64, cend: i64) -> u64 {
    let bs = char_byte(s, cstart.max(0) as usize);
    let be = char_byte(s, cend.max(0) as usize);
    mkstr(s.as_bytes()[bs..be].to_vec())
}

unsafe extern "C" fn regex_from_string_with(options: u64, string: u64) -> u64 {
    let ci = rt_is_true(rt_access(options, b"caseInsensitive ".as_ptr()));
    let ml = rt_is_true(rt_access(options, b"multiline ".as_ptr()));
    let s = str_slice(string);
    let p = alm_rx_compile(s.as_ptr(), s.len(), ci, ml);
    if p.is_null() {
        nothing()
    } else {
        just(alloc(Value::Regex(p)))
    }
}

unsafe extern "C" fn regex_contains(re: u64, string: u64) -> u64 {
    let p = regex_ptr(re);
    if p.is_null() {
        return rt_bool(false);
    }
    let s = str_slice(string);
    rt_bool(alm_rx_contains(p, s.as_ptr(), s.len()) != 0)
}

/// Build an elm `Match` record from the flat find buffer at `buf[i..]`,
/// returning `(record, next_i)`. `num` is the 1-based match ordinal.
unsafe fn build_match(s: &str, buf: &[i64], mut i: usize, num: i64) -> (u64, usize) {
    let mstart = buf[i];
    let mend = buf[i + 1];
    let ngroups = buf[i + 2] as usize;
    i += 3;
    let match_str = str_char_slice(s, mstart, mend);
    let mut gvals: Vec<u64> = Vec::with_capacity(ngroups);
    for _ in 0..ngroups {
        let gs = buf[i];
        let ge = buf[i + 1];
        i += 2;
        gvals.push(if gs < 0 { nothing() } else { just(str_char_slice(s, gs, ge)) });
    }
    let mut subs = nil();
    for mv in gvals.into_iter().rev() {
        subs = cons(mv, subs);
    }
    // Record fields in sorted-by-name order: index, match, number, submatches.
    let rec = rt_record_new(4);
    rt_record_set(rec, 0, b"index ".as_ptr(), mk_int(mstart));
    rt_record_set(rec, 1, b"match ".as_ptr(), match_str);
    rt_record_set(rec, 2, b"number ".as_ptr(), mk_int(num));
    rt_record_set(rec, 3, b"submatches ".as_ptr(), subs);
    (rec, i)
}

unsafe extern "C" fn regex_find_at_most(limit: u64, re: u64, string: u64) -> u64 {
    let p = regex_ptr(re);
    if p.is_null() {
        return nil();
    }
    let s = str_slice(string);
    let ss = std::str::from_utf8_unchecked(s);
    let mut out_len = 0usize;
    let buf_ptr = alm_rx_find(p, s.as_ptr(), s.len(), int_val(limit), &mut out_len);
    let buf = std::slice::from_raw_parts(buf_ptr, out_len);
    let nmatches = buf[0];
    let mut recs: Vec<u64> = Vec::with_capacity(nmatches as usize);
    let mut i = 1usize;
    for k in 0..nmatches {
        let (rec, ni) = build_match(ss, buf, i, k + 1);
        recs.push(rec);
        i = ni;
    }
    alm_rx_free(buf_ptr, out_len);
    let mut lst = nil();
    for rec in recs.into_iter().rev() {
        lst = cons(rec, lst);
    }
    lst
}

unsafe extern "C" fn regex_split_at_most(limit: u64, re: u64, string: u64) -> u64 {
    let p = regex_ptr(re);
    if p.is_null() {
        return cons(string, nil());
    }
    let s = str_slice(string);
    let ss = std::str::from_utf8_unchecked(s);
    let mut out_len = 0usize;
    let buf_ptr = alm_rx_split(p, s.as_ptr(), s.len(), int_val(limit), &mut out_len);
    let buf = std::slice::from_raw_parts(buf_ptr, out_len);
    let npieces = buf[0];
    let mut pieces: Vec<u64> = Vec::with_capacity(npieces as usize);
    let mut i = 1usize;
    for _ in 0..npieces {
        pieces.push(str_char_slice(ss, buf[i], buf[i + 1]));
        i += 2;
    }
    alm_rx_free(buf_ptr, out_len);
    let mut lst = nil();
    for pc in pieces.into_iter().rev() {
        lst = cons(pc, lst);
    }
    lst
}

unsafe extern "C" fn regex_replace_at_most(limit: u64, re: u64, replacer: u64, string: u64) -> u64 {
    let p = regex_ptr(re);
    let s = str_slice(string);
    if p.is_null() {
        return string;
    }
    let ss = std::str::from_utf8_unchecked(s);
    let mut out_len = 0usize;
    let buf_ptr = alm_rx_find(p, s.as_ptr(), s.len(), int_val(limit), &mut out_len);
    let buf = std::slice::from_raw_parts(buf_ptr, out_len);
    let nmatches = buf[0];
    let mut result: Vec<u8> = Vec::new();
    let mut last_char = 0i64;
    let mut i = 1usize;
    for k in 0..nmatches {
        let mstart = buf[i];
        let mend = buf[i + 1];
        let (rec, ni) = build_match(ss, buf, i, k + 1);
        i = ni;
        let bs = char_byte(ss, last_char.max(0) as usize);
        let be = char_byte(ss, mstart.max(0) as usize);
        result.extend_from_slice(&s[bs..be]);
        let repl = ap1(replacer, rec);
        result.extend_from_slice(str_slice(repl));
        last_char = mend;
    }
    let bs = char_byte(ss, last_char.max(0) as usize);
    result.extend_from_slice(&s[bs..]);
    alm_rx_free(buf_ptr, out_len);
    mkstr(result)
}

// --- elm/html + elm/virtual-dom (the Rust twin of runtime.js's _VDom_*) ---
//
// Virtual-dom nodes and attributes are ordinary tagged `Value::Ctor`s, so the
// generic structural `value_eq` compares them exactly as elm's `==` compares
// the plain objects `runtime.js` builds (Html without event handlers is
// comparable in elm, and elm-explorations/test relies on it). Each node kind
// and attribute kind gets a distinct constructor index — `value_eq` keys on
// index, so the indices must not collide across kinds. There is no renderer:
// native programs build and compare virtual dom, they do not mount it.

const NS_SVG: &[u8] = b"http://www.w3.org/2000/svg";

const VI_TEXT: u32 = 0;
const VI_NODE: u32 = 1;
const VI_KEYED: u32 = 2;
const VI_MAP: u32 = 4;
const AI_ATTR: u32 = 10;
const AI_PROP: u32 = 11;
const AI_STYLE: u32 = 12;
const AI_EVENT: u32 = 13;

unsafe fn str_is(w: u64, lit: &[u8]) -> bool {
    is_str_value(w) && sbytes(w) == lit
}

/// `a ++ " " ++ b` for two class strings (no leading space when `a` is empty),
/// mirroring elm/virtual-dom's className merge.
unsafe fn join_class(a: u64, b: u64) -> u64 {
    // Property values are `Json.Encode.Value`s: plain Str from the class
    // kernels, Json(JStr) from `VirtualDom.property "className" (Encode.string
    // s)` (elm-css, bootstrap) — accept both (see `prop_str`).
    let mut s = prop_str(a).to_vec();
    if !s.is_empty() {
        s.push(b' ');
    }
    s.extend_from_slice(prop_str(b));
    mkstr(s)
}

/// Merge repeated `className` properties / `class` attributes on one node into
/// a single space-joined value, like elm/virtual-dom's `_VirtualDom_organizeFacts`
/// (runtime.js `_VDom_organize`) — so `div [ class "a", class "b" ]` compares
/// equal to `div [ class "a b" ]`.
unsafe fn vdom_organize(attrs: u64) -> u64 {
    let items = to_vec(attrs);
    let mut props = 0usize;
    let mut raws = 0usize;
    for &a in &items {
        if let Value::Ctor { index, arg0, rest, .. } = deref(a) {
            if *index == AI_PROP && str_is(*arg0, b"className") {
                props += 1;
            } else if *index == AI_ATTR && str_is(*arg0, b"class") && rest[1] == nothing() {
                raws += 1;
            }
        }
    }
    if props < 2 && raws < 2 {
        return attrs;
    }
    let mut out: Vec<u64> = Vec::with_capacity(items.len());
    let mut prop_at: isize = -1;
    let mut raw_at: isize = -1;
    for &a in &items {
        match deref(a) {
            Value::Ctor { index, arg0, rest, .. }
                if *index == AI_PROP && str_is(*arg0, b"className") =>
            {
                if prop_at < 0 {
                    prop_at = out.len() as isize;
                    out.push(a);
                } else {
                    let merged = join_class(ctor_get(out[prop_at as usize], 1), rest[0]);
                    out[prop_at as usize] =
                        ctor(b"AProp\0".as_ptr(), AI_PROP, vec![*arg0, merged]);
                }
            }
            Value::Ctor { index, arg0, rest, .. }
                if *index == AI_ATTR && str_is(*arg0, b"class") && rest[1] == nothing() =>
            {
                if raw_at < 0 {
                    raw_at = out.len() as isize;
                    out.push(a);
                } else {
                    let merged = join_class(ctor_get(out[raw_at as usize], 1), rest[0]);
                    out[raw_at as usize] =
                        ctor(b"AAttr\0".as_ptr(), AI_ATTR, vec![*arg0, merged, nothing()]);
                }
            }
            _ => out.push(a),
        }
    }
    list_from_slice(&out)
}

unsafe fn vnode(tag: u64, attrs: u64, kids: u64, ns: u64) -> u64 {
    ctor(b"VNode\0".as_ptr(), VI_NODE, vec![tag, vdom_organize(attrs), kids, ns])
}

// text : String -> Html msg
unsafe extern "C" fn vdom_text1(text: u64) -> u64 {
    ctor(b"VText\0".as_ptr(), VI_TEXT, vec![text])
}
// node : String -> List (Attribute msg) -> List (Html msg) -> Html msg
unsafe extern "C" fn vdom_node3(tag: u64, attrs: u64, kids: u64) -> u64 {
    vnode(tag, attrs, kids, nothing())
}
// Svg.node — same, in the SVG namespace.
unsafe extern "C" fn vdom_node_ns3(tag: u64, attrs: u64, kids: u64) -> u64 {
    vnode(tag, attrs, kids, just(mkstr(NS_SVG.to_vec())))
}
// VirtualDom.nodeNS : String -> String -> ... (namespace supplied by caller)
unsafe extern "C" fn vdom_node_ns4(ns: u64, tag: u64, attrs: u64, kids: u64) -> u64 {
    vnode(tag, attrs, kids, just(ns))
}
// map : (a -> msg) -> Html a -> Html msg
unsafe extern "C" fn vdom_map2(f: u64, node: u64) -> u64 {
    ctor(b"VMap\0".as_ptr(), VI_MAP, vec![f, node])
}
// Keyed.node : String -> List (Attribute msg) -> List ( String, Html msg ) -> Html msg
unsafe extern "C" fn vdom_keyed3(tag: u64, attrs: u64, kids: u64) -> u64 {
    ctor(b"VKeyed\0".as_ptr(), VI_KEYED, vec![tag, vdom_organize(attrs), kids, nothing()])
}
unsafe extern "C" fn vdom_keyed_ns4(ns: u64, tag: u64, attrs: u64, kids: u64) -> u64 {
    ctor(b"VKeyed\0".as_ptr(), VI_KEYED, vec![tag, vdom_organize(attrs), kids, just(ns)])
}

// Attribute builders. `attr_property2`/`attr_attribute2` also back the
// per-attribute helpers (`class`, `href`, …) with the DOM key pre-applied.
unsafe extern "C" fn attr_style2(key: u64, val: u64) -> u64 {
    ctor(b"AStyle\0".as_ptr(), AI_STYLE, vec![key, val])
}
unsafe extern "C" fn attr_attribute2(key: u64, val: u64) -> u64 {
    ctor(b"AAttr\0".as_ptr(), AI_ATTR, vec![key, val, nothing()])
}
unsafe extern "C" fn attr_property2(key: u64, val: u64) -> u64 {
    ctor(b"AProp\0".as_ptr(), AI_PROP, vec![key, val])
}
// Int-valued attributes render to a string attribute (String.fromInt n).
unsafe extern "C" fn attr_int_str2(key: u64, n: u64) -> u64 {
    ctor(b"AAttr\0".as_ptr(), AI_ATTR, vec![key, string_from_int(n), nothing()])
}
// `autocomplete : Bool -> Attribute` is a string property "on"/"off" in elm.
unsafe extern "C" fn attr_autocomplete1(b: u64) -> u64 {
    let v = if rt_is_true(b) { b"on".to_vec() } else { b"off".to_vec() };
    ctor(b"AProp\0".as_ptr(), AI_PROP, vec![mkstr(b"autocomplete".to_vec()), mkstr(v)])
}
// classList : List ( String, Bool ) -> Attribute — the `True` names, space-joined.
unsafe extern "C" fn attr_class_list1(pairs: u64) -> u64 {
    let mut names: Vec<u8> = Vec::new();
    for &pair in &to_vec(pairs) {
        if rt_is_true(tuple_second(pair)) {
            if !names.is_empty() {
                names.push(b' ');
            }
            names.extend_from_slice(str_slice(tuple_first(pair)));
        }
    }
    ctor(b"AProp\0".as_ptr(), AI_PROP, vec![mkstr(b"className".to_vec()), mkstr(names)])
}
// Attributes.map : (a -> msg) -> Attribute a -> Attribute msg. Only events
// carry the msg; everything else is msg-agnostic and passes through.
unsafe extern "C" fn attr_map2(f: u64, attr: u64) -> u64 {
    match deref(attr) {
        Value::Ctor { index, arg0, rest, .. } if *index == AI_EVENT => {
            let dec = mk_decoder(Decoder::Map(f, rest[0]));
            ctor(b"AEvent\0".as_ptr(), AI_EVENT, vec![*arg0, dec, rest[1]])
        }
        _ => attr,
    }
}

// Events. `opts` is a tag: 0 normal, 1 stopPropagation, 2 preventDefault,
// 3 custom (mirrors runtime.js's option object; native never dispatches, so
// only the built value's shape matters).
unsafe fn event(name: &[u8], decoder: u64, opts: i64) -> u64 {
    ctor(b"AEvent\0".as_ptr(), AI_EVENT, vec![mkstr(name.to_vec()), decoder, mk_int(opts)])
}
unsafe fn on_msg(name: &[u8], msg: u64) -> u64 {
    event(name, mk_decoder(Decoder::Succeed(msg)), 0)
}
// `e.target.value` / `e.target.checked` decoders (as elm's targetValue/Checked).
unsafe fn target_field(field: &[u8], leaf: Decoder) -> u64 {
    mk_decoder(Decoder::Field(
        b"target".to_vec(),
        mk_decoder(Decoder::Field(field.to_vec(), mk_decoder(leaf))),
    ))
}
unsafe extern "C" fn events_on2(name: u64, decoder: u64) -> u64 {
    ctor(b"AEvent\0".as_ptr(), AI_EVENT, vec![name, decoder, mk_int(0)])
}
unsafe extern "C" fn events_stop_on2(name: u64, decoder: u64) -> u64 {
    ctor(b"AEvent\0".as_ptr(), AI_EVENT, vec![name, decoder, mk_int(1)])
}
unsafe extern "C" fn events_prevent_on2(name: u64, decoder: u64) -> u64 {
    ctor(b"AEvent\0".as_ptr(), AI_EVENT, vec![name, decoder, mk_int(2)])
}
unsafe extern "C" fn events_custom2(name: u64, decoder: u64) -> u64 {
    ctor(b"AEvent\0".as_ptr(), AI_EVENT, vec![name, decoder, mk_int(3)])
}
unsafe extern "C" fn events_on_click1(msg: u64) -> u64 { on_msg(b"click", msg) }
unsafe extern "C" fn events_on_dblclick1(msg: u64) -> u64 { on_msg(b"dblclick", msg) }
unsafe extern "C" fn events_on_mousedown1(msg: u64) -> u64 { on_msg(b"mousedown", msg) }
unsafe extern "C" fn events_on_mouseup1(msg: u64) -> u64 { on_msg(b"mouseup", msg) }
unsafe extern "C" fn events_on_mouseenter1(msg: u64) -> u64 { on_msg(b"mouseenter", msg) }
unsafe extern "C" fn events_on_mouseleave1(msg: u64) -> u64 { on_msg(b"mouseleave", msg) }
unsafe extern "C" fn events_on_mouseover1(msg: u64) -> u64 { on_msg(b"mouseover", msg) }
unsafe extern "C" fn events_on_mouseout1(msg: u64) -> u64 { on_msg(b"mouseout", msg) }
unsafe extern "C" fn events_on_blur1(msg: u64) -> u64 { on_msg(b"blur", msg) }
unsafe extern "C" fn events_on_focus1(msg: u64) -> u64 { on_msg(b"focus", msg) }
unsafe extern "C" fn events_on_submit1(msg: u64) -> u64 {
    // elm/html: `onSubmit msg = preventDefaultOn "submit" (Decode.map
    // alwaysPreventDefault (Decode.succeed msg))` where `alwaysPreventDefault m
    // = ( m, True )`. The handler is MayPreventDefault (opts 2), whose decoder
    // must yield the `( msg, Bool )` tuple — Test.Html destructures it as one.
    event(b"submit", mk_decoder(Decoder::Succeed(pair(msg, mkbool(true)))), 2)
}
unsafe extern "C" fn events_on_input1(to_msg: u64) -> u64 {
    event(b"input", mk_decoder(Decoder::Map(to_msg, target_field(b"value", Decoder::Str))), 0)
}
unsafe extern "C" fn events_on_check1(to_msg: u64) -> u64 {
    event(b"change", mk_decoder(Decoder::Map(to_msg, target_field(b"checked", Decoder::Bool))), 0)
}

// `VirtualDom.on name handler` — handler is the `Handler` union (Normal /
// MayStopPropagation / MayPreventDefault / Custom, each wrapping a decoder).
// Match by constructor NAME rather than index so this is independent of the
// tag-assignment scheme. The opts tag mirrors `event`'s encoding.
unsafe extern "C" fn vdom_on2(name: u64, handler: u64) -> u64 {
    let (tag, dec) = match deref(handler) {
        Value::Ctor { name: cn, arg0, .. } => {
            let opts = match cname(*cn) {
                "MayStopPropagation" => 1,
                "MayPreventDefault" => 2,
                "Custom" => 3,
                _ => 0, // Normal
            };
            (opts, *arg0)
        }
        _ => (0, handler),
    };
    ctor(b"AEvent\0".as_ptr(), AI_EVENT, vec![name, dec, mk_int(tag)])
}

// `Html.Lazy.lazy*` / `VirtualDom.lazy*` — force eagerly. Laziness is purely a
// render-cache optimization; the value is the forced node, and Test.Html (the
// only consumer of native vdom) forces lazies before reflecting anyway.
macro_rules! vdom_lazy {
    ($fname:ident, $($arg:ident),+) => {
        unsafe extern "C" fn $fname(f: u64, $($arg: u64),+) -> u64 {
            let mut r = f;
            $( r = ap1(r, $arg); )+
            r
        }
    };
}
vdom_lazy!(vdom_lazy2, a);
vdom_lazy!(vdom_lazy3, a, b);
vdom_lazy!(vdom_lazy4, a, b, c);
vdom_lazy!(vdom_lazy5, a, b, c, d);
vdom_lazy!(vdom_lazy6, a, b, c, d, e);
vdom_lazy!(vdom_lazy7, a, b, c, d, e, g);
vdom_lazy!(vdom_lazy8, a, b, c, d, e, g, h);
vdom_lazy!(vdom_lazy9, a, b, c, d, e, g, h, i);

// --- elm/http: structural twins of runtime.js's `$Http$*`. Native programs
// never perform requests (tests only construct and inspect these values), so
// the kernels build the same shapes the JS runtime does: headers and bodies are
// records, an Expect is a record carrying `toMsg` (the handler closure is never
// invoked natively — a unit placeholder), and `request` wraps its config in a
// `CmdHttp` constructor.
unsafe extern "C" fn http_header2(name: u64, value: u64) -> u64 {
    let rec = rt_record_new(2);
    rt_record_set(rec, 0, b"name\0".as_ptr(), name);
    rt_record_set(rec, 1, b"value\0".as_ptr(), value);
    rec
}
unsafe fn http_body(content_type: u64, content: u64) -> u64 {
    let rec = rt_record_new(2);
    rt_record_set(rec, 0, b"contentType\0".as_ptr(), content_type);
    rt_record_set(rec, 1, b"content\0".as_ptr(), content);
    rec
}
unsafe extern "C" fn http_empty_body0() -> u64 {
    // JS uses nulls; the JSON null is the equality-comparable native twin.
    let null = alloc(Value::Json(JsonValue::Null));
    http_body(null, null)
}
unsafe extern "C" fn http_string_body2(content_type: u64, content: u64) -> u64 {
    http_body(content_type, content)
}
unsafe extern "C" fn http_json_body1(value: u64) -> u64 {
    http_body(
        mkstr(b"application/json".to_vec()),
        encode_encode(mk_int(0), value),
    )
}
unsafe fn http_expect(to_msg: u64) -> u64 {
    let rec = rt_record_new(2);
    rt_record_set(rec, 0, b"toMsg\0".as_ptr(), to_msg);
    rt_record_set(rec, 1, b"handle\0".as_ptr(), unit());
    rec
}
unsafe extern "C" fn http_expect_string1(to_msg: u64) -> u64 {
    http_expect(to_msg)
}
unsafe extern "C" fn http_expect_whatever1(to_msg: u64) -> u64 {
    http_expect(to_msg)
}
unsafe extern "C" fn http_expect_json2(to_msg: u64, _decoder: u64) -> u64 {
    http_expect(to_msg)
}
unsafe extern "C" fn http_expect_string_response2(to_msg: u64, _to_result: u64) -> u64 {
    http_expect(to_msg)
}
unsafe extern "C" fn http_expect_bytes_response2(to_msg: u64, _to_result: u64) -> u64 {
    http_expect(to_msg)
}
unsafe extern "C" fn http_request1(config: u64) -> u64 {
    ctor1(b"CmdHttp\0".as_ptr(), 0, config)
}
unsafe fn http_simple_request(method: &[u8], url: u64, body: u64, expect: u64) -> u64 {
    let rec = rt_record_new(7);
    rt_record_set(rec, 0, b"method\0".as_ptr(), mkstr(method.to_vec()));
    rt_record_set(rec, 1, b"headers\0".as_ptr(), nil());
    rt_record_set(rec, 2, b"url\0".as_ptr(), url);
    rt_record_set(rec, 3, b"body\0".as_ptr(), body);
    rt_record_set(rec, 4, b"expect\0".as_ptr(), expect);
    rt_record_set(rec, 5, b"timeout\0".as_ptr(), nothing());
    rt_record_set(rec, 6, b"tracker\0".as_ptr(), nothing());
    http_request1(rec)
}
unsafe extern "C" fn http_get1(config: u64) -> u64 {
    let url = rt_access(config, b"url\0".as_ptr());
    let expect = rt_access(config, b"expect\0".as_ptr());
    http_simple_request(b"GET", url, http_empty_body0(), expect)
}
unsafe extern "C" fn http_post1(config: u64) -> u64 {
    let url = rt_access(config, b"url\0".as_ptr());
    let body = rt_access(config, b"body\0".as_ptr());
    let expect = rt_access(config, b"expect\0".as_ptr());
    http_simple_request(b"POST", url, body, expect)
}
unsafe extern "C" fn http_string_resolver1(to_result: u64) -> u64 {
    let rec = rt_record_new(1);
    rt_record_set(rec, 0, b"toResult\0".as_ptr(), to_result);
    rec
}
// `Http.task`/`riskyTask` — a Task value that is never executed natively
// (tests only construct and compose it); an opaque constructor suffices.
unsafe extern "C" fn http_task1(config: u64) -> u64 {
    ctor1(b"TaskHttp\0".as_ptr(), 0, config)
}

// --- Browser.Dom: headless stand-ins (no DOM natively). The JS runtime's
// equivalents also run without a document and yield failing/zeroed tasks;
// these are opaque Task values that tests construct but never perform.
unsafe extern "C" fn dom_get_element1(id: u64) -> u64 {
    ctor1(b"TaskDom\0".as_ptr(), 0, id)
}
unsafe extern "C" fn dom_set_viewport_of3(id: u64, _x: u64, _y: u64) -> u64 {
    ctor1(b"TaskDom\0".as_ptr(), 0, id)
}

// --- elm-explorations/webgl (Elm.Kernel.WebGL): headless stand-ins mirroring
// runtime.js — real rendering needs a GPU/DOM `gl` context. The nodes carry
// their inputs as plain data so structural equality behaves predictably;
// `enable*` settings/options all yield unit.
unsafe extern "C" fn webgl_entity5(settings: u64, vert: u64, frag: u64, mesh: u64, uniforms: u64) -> u64 {
    ctor(b"Entity\0".as_ptr(), 0, vec![settings, vert, frag, mesh, uniforms])
}
unsafe extern "C" fn webgl_to_html3(options: u64, attributes: u64, entities: u64) -> u64 {
    ctor(b"WebGLScene\0".as_ptr(), 0, vec![options, attributes, entities])
}
unsafe extern "C" fn webgl_enable2(_ctx: u64, _setting: u64) -> u64 {
    unit()
}
// Texture.load can never succeed headlessly (mirrors the JS stand-in, which
// fails the task); size reads the (never-constructible) texture's dimensions.
unsafe extern "C" fn texture_load6(_mag: u64, _min: u64, _hw: u64, _vw: u64, _fy: u64, _url: u64) -> u64 {
    task_fail(unit())
}
unsafe extern "C" fn texture_size1(texture: u64) -> u64 {
    pair(
        rt_access(texture, b"width\0".as_ptr()),
        rt_access(texture, b"height\0".as_ptr()),
    )
}

// --- elm-explorations/linear-algebra (Elm.Kernel.MJS): pure vector/matrix
// math, ported verbatim from the JS kernel (same formulas, same argument
// order, same edge cases — e.g. `1/0 = inf` in normalize/direction and the
// exact `det == 0` check in `inverse`). Vectors and matrices are
// `Value::Floats` of length 2/3/4/16, the native twin of the JS
// Float64Array; matrices are column-major like the JS kernel.

/// Deref a linear-algebra value's float storage (borrowed; values are never
/// freed, so 'a is sound — same model as `str_slice`).
unsafe fn floats<'a>(w: u64) -> &'a [f64] {
    match deref(w) {
        Value::Floats(v) => v,
        _ => crash!("expected a linear-algebra vector or matrix"),
    }
}

unsafe fn mk_floats(v: Vec<f64>) -> u64 {
    alloc(Value::Floats(v))
}

// Vector2

unsafe extern "C" fn mjs_v2_2(x: u64, y: u64) -> u64 {
    mk_floats(vec![num(x), num(y)])
}
unsafe extern "C" fn mjs_v2_get_x1(a: u64) -> u64 {
    rt_float(floats(a)[0])
}
unsafe extern "C" fn mjs_v2_get_y1(a: u64) -> u64 {
    rt_float(floats(a)[1])
}
unsafe extern "C" fn mjs_v2_set_x2(x: u64, a: u64) -> u64 {
    let a = floats(a);
    mk_floats(vec![num(x), a[1]])
}
unsafe extern "C" fn mjs_v2_set_y2(y: u64, a: u64) -> u64 {
    let a = floats(a);
    mk_floats(vec![a[0], num(y)])
}
unsafe extern "C" fn mjs_v2_to_record1(a: u64) -> u64 {
    let a = floats(a);
    let rec = rt_record_new(2);
    rt_record_set(rec, 0, b"x\0".as_ptr(), rt_float(a[0]));
    rt_record_set(rec, 1, b"y\0".as_ptr(), rt_float(a[1]));
    rec
}
unsafe extern "C" fn mjs_v2_from_record1(r: u64) -> u64 {
    mk_floats(vec![
        num(rt_access(r, b"x\0".as_ptr())),
        num(rt_access(r, b"y\0".as_ptr())),
    ])
}
unsafe extern "C" fn mjs_v2_add2(a: u64, b: u64) -> u64 {
    let (a, b) = (floats(a), floats(b));
    mk_floats(vec![a[0] + b[0], a[1] + b[1]])
}
unsafe extern "C" fn mjs_v2_sub2(a: u64, b: u64) -> u64 {
    let (a, b) = (floats(a), floats(b));
    mk_floats(vec![a[0] - b[0], a[1] - b[1]])
}
unsafe extern "C" fn mjs_v2_negate1(a: u64) -> u64 {
    let a = floats(a);
    mk_floats(vec![-a[0], -a[1]])
}
fn v2_length(a: &[f64]) -> f64 {
    (a[0] * a[0] + a[1] * a[1]).sqrt()
}
unsafe extern "C" fn mjs_v2_direction2(a: u64, b: u64) -> u64 {
    let (a, b) = (floats(a), floats(b));
    let r = [a[0] - b[0], a[1] - b[1]];
    let im = 1.0 / v2_length(&r);
    mk_floats(vec![r[0] * im, r[1] * im])
}
unsafe extern "C" fn mjs_v2_length1(a: u64) -> u64 {
    rt_float(v2_length(floats(a)))
}
unsafe extern "C" fn mjs_v2_length_squared1(a: u64) -> u64 {
    let a = floats(a);
    rt_float(a[0] * a[0] + a[1] * a[1])
}
unsafe extern "C" fn mjs_v2_distance2(a: u64, b: u64) -> u64 {
    let (a, b) = (floats(a), floats(b));
    let dx = a[0] - b[0];
    let dy = a[1] - b[1];
    rt_float((dx * dx + dy * dy).sqrt())
}
unsafe extern "C" fn mjs_v2_distance_squared2(a: u64, b: u64) -> u64 {
    let (a, b) = (floats(a), floats(b));
    let dx = a[0] - b[0];
    let dy = a[1] - b[1];
    rt_float(dx * dx + dy * dy)
}
unsafe extern "C" fn mjs_v2_normalize1(a: u64) -> u64 {
    let a = floats(a);
    let im = 1.0 / v2_length(a);
    mk_floats(vec![a[0] * im, a[1] * im])
}
unsafe extern "C" fn mjs_v2_scale2(k: u64, a: u64) -> u64 {
    let k = num(k);
    let a = floats(a);
    mk_floats(vec![a[0] * k, a[1] * k])
}
unsafe extern "C" fn mjs_v2_dot2(a: u64, b: u64) -> u64 {
    let (a, b) = (floats(a), floats(b));
    rt_float(a[0] * b[0] + a[1] * b[1])
}

// Vector3

unsafe extern "C" fn mjs_v3_3(x: u64, y: u64, z: u64) -> u64 {
    mk_floats(vec![num(x), num(y), num(z)])
}
unsafe extern "C" fn mjs_v3_get_x1(a: u64) -> u64 {
    rt_float(floats(a)[0])
}
unsafe extern "C" fn mjs_v3_get_y1(a: u64) -> u64 {
    rt_float(floats(a)[1])
}
unsafe extern "C" fn mjs_v3_get_z1(a: u64) -> u64 {
    rt_float(floats(a)[2])
}
unsafe extern "C" fn mjs_v3_set_x2(x: u64, a: u64) -> u64 {
    let a = floats(a);
    mk_floats(vec![num(x), a[1], a[2]])
}
unsafe extern "C" fn mjs_v3_set_y2(y: u64, a: u64) -> u64 {
    let a = floats(a);
    mk_floats(vec![a[0], num(y), a[2]])
}
unsafe extern "C" fn mjs_v3_set_z2(z: u64, a: u64) -> u64 {
    let a = floats(a);
    mk_floats(vec![a[0], a[1], num(z)])
}
unsafe extern "C" fn mjs_v3_to_record1(a: u64) -> u64 {
    let a = floats(a);
    let rec = rt_record_new(3);
    rt_record_set(rec, 0, b"x\0".as_ptr(), rt_float(a[0]));
    rt_record_set(rec, 1, b"y\0".as_ptr(), rt_float(a[1]));
    rt_record_set(rec, 2, b"z\0".as_ptr(), rt_float(a[2]));
    rec
}
unsafe extern "C" fn mjs_v3_from_record1(r: u64) -> u64 {
    mk_floats(vec![
        num(rt_access(r, b"x\0".as_ptr())),
        num(rt_access(r, b"y\0".as_ptr())),
        num(rt_access(r, b"z\0".as_ptr())),
    ])
}
unsafe extern "C" fn mjs_v3_add2(a: u64, b: u64) -> u64 {
    let (a, b) = (floats(a), floats(b));
    mk_floats(vec![a[0] + b[0], a[1] + b[1], a[2] + b[2]])
}
fn v3_sub(a: &[f64], b: &[f64]) -> [f64; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}
unsafe extern "C" fn mjs_v3_sub2(a: u64, b: u64) -> u64 {
    mk_floats(v3_sub(floats(a), floats(b)).to_vec())
}
unsafe extern "C" fn mjs_v3_negate1(a: u64) -> u64 {
    let a = floats(a);
    mk_floats(vec![-a[0], -a[1], -a[2]])
}
fn v3_length(a: &[f64]) -> f64 {
    (a[0] * a[0] + a[1] * a[1] + a[2] * a[2]).sqrt()
}
fn v3_normalize(a: &[f64]) -> [f64; 3] {
    let im = 1.0 / v3_length(a);
    [a[0] * im, a[1] * im, a[2] * im]
}
fn v3_direction(a: &[f64], b: &[f64]) -> [f64; 3] {
    v3_normalize(&v3_sub(a, b))
}
unsafe extern "C" fn mjs_v3_direction2(a: u64, b: u64) -> u64 {
    mk_floats(v3_direction(floats(a), floats(b)).to_vec())
}
unsafe extern "C" fn mjs_v3_length1(a: u64) -> u64 {
    rt_float(v3_length(floats(a)))
}
unsafe extern "C" fn mjs_v3_length_squared1(a: u64) -> u64 {
    let a = floats(a);
    rt_float(a[0] * a[0] + a[1] * a[1] + a[2] * a[2])
}
unsafe extern "C" fn mjs_v3_distance2(a: u64, b: u64) -> u64 {
    let (a, b) = (floats(a), floats(b));
    let dx = a[0] - b[0];
    let dy = a[1] - b[1];
    let dz = a[2] - b[2];
    rt_float((dx * dx + dy * dy + dz * dz).sqrt())
}
unsafe extern "C" fn mjs_v3_distance_squared2(a: u64, b: u64) -> u64 {
    let (a, b) = (floats(a), floats(b));
    let dx = a[0] - b[0];
    let dy = a[1] - b[1];
    let dz = a[2] - b[2];
    rt_float(dx * dx + dy * dy + dz * dz)
}
unsafe extern "C" fn mjs_v3_normalize1(a: u64) -> u64 {
    mk_floats(v3_normalize(floats(a)).to_vec())
}
unsafe extern "C" fn mjs_v3_scale2(k: u64, a: u64) -> u64 {
    let k = num(k);
    let a = floats(a);
    mk_floats(vec![a[0] * k, a[1] * k, a[2] * k])
}
fn v3_dot(a: &[f64], b: &[f64]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
unsafe extern "C" fn mjs_v3_dot2(a: u64, b: u64) -> u64 {
    rt_float(v3_dot(floats(a), floats(b)))
}
fn v3_cross(a: &[f64], b: &[f64]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}
unsafe extern "C" fn mjs_v3_cross2(a: u64, b: u64) -> u64 {
    mk_floats(v3_cross(floats(a), floats(b)).to_vec())
}
unsafe extern "C" fn mjs_v3_mul4x4_2(m: u64, v: u64) -> u64 {
    let (m, v) = (floats(m), floats(v));
    let w = v3_dot(v, &[m[3], m[7], m[11]]) + m[15];
    mk_floats(vec![
        (v3_dot(v, &[m[0], m[4], m[8]]) + m[12]) / w,
        (v3_dot(v, &[m[1], m[5], m[9]]) + m[13]) / w,
        (v3_dot(v, &[m[2], m[6], m[10]]) + m[14]) / w,
    ])
}

// Vector4

unsafe extern "C" fn mjs_v4_4(x: u64, y: u64, z: u64, w: u64) -> u64 {
    mk_floats(vec![num(x), num(y), num(z), num(w)])
}
unsafe extern "C" fn mjs_v4_get_x1(a: u64) -> u64 {
    rt_float(floats(a)[0])
}
unsafe extern "C" fn mjs_v4_get_y1(a: u64) -> u64 {
    rt_float(floats(a)[1])
}
unsafe extern "C" fn mjs_v4_get_z1(a: u64) -> u64 {
    rt_float(floats(a)[2])
}
unsafe extern "C" fn mjs_v4_get_w1(a: u64) -> u64 {
    rt_float(floats(a)[3])
}
unsafe extern "C" fn mjs_v4_set_x2(x: u64, a: u64) -> u64 {
    let a = floats(a);
    mk_floats(vec![num(x), a[1], a[2], a[3]])
}
unsafe extern "C" fn mjs_v4_set_y2(y: u64, a: u64) -> u64 {
    let a = floats(a);
    mk_floats(vec![a[0], num(y), a[2], a[3]])
}
unsafe extern "C" fn mjs_v4_set_z2(z: u64, a: u64) -> u64 {
    let a = floats(a);
    mk_floats(vec![a[0], a[1], num(z), a[3]])
}
unsafe extern "C" fn mjs_v4_set_w2(w: u64, a: u64) -> u64 {
    let a = floats(a);
    mk_floats(vec![a[0], a[1], a[2], num(w)])
}
unsafe extern "C" fn mjs_v4_to_record1(a: u64) -> u64 {
    // Fields in alphabetical order: w, x, y, z.
    let a = floats(a);
    let rec = rt_record_new(4);
    rt_record_set(rec, 0, b"w\0".as_ptr(), rt_float(a[3]));
    rt_record_set(rec, 1, b"x\0".as_ptr(), rt_float(a[0]));
    rt_record_set(rec, 2, b"y\0".as_ptr(), rt_float(a[1]));
    rt_record_set(rec, 3, b"z\0".as_ptr(), rt_float(a[2]));
    rec
}
unsafe extern "C" fn mjs_v4_from_record1(r: u64) -> u64 {
    mk_floats(vec![
        num(rt_access(r, b"x\0".as_ptr())),
        num(rt_access(r, b"y\0".as_ptr())),
        num(rt_access(r, b"z\0".as_ptr())),
        num(rt_access(r, b"w\0".as_ptr())),
    ])
}
unsafe extern "C" fn mjs_v4_add2(a: u64, b: u64) -> u64 {
    let (a, b) = (floats(a), floats(b));
    mk_floats(vec![a[0] + b[0], a[1] + b[1], a[2] + b[2], a[3] + b[3]])
}
unsafe extern "C" fn mjs_v4_sub2(a: u64, b: u64) -> u64 {
    let (a, b) = (floats(a), floats(b));
    mk_floats(vec![a[0] - b[0], a[1] - b[1], a[2] - b[2], a[3] - b[3]])
}
unsafe extern "C" fn mjs_v4_negate1(a: u64) -> u64 {
    let a = floats(a);
    mk_floats(vec![-a[0], -a[1], -a[2], -a[3]])
}
fn v4_length(a: &[f64]) -> f64 {
    (a[0] * a[0] + a[1] * a[1] + a[2] * a[2] + a[3] * a[3]).sqrt()
}
unsafe extern "C" fn mjs_v4_direction2(a: u64, b: u64) -> u64 {
    let (a, b) = (floats(a), floats(b));
    let r = [a[0] - b[0], a[1] - b[1], a[2] - b[2], a[3] - b[3]];
    let im = 1.0 / v4_length(&r);
    mk_floats(vec![r[0] * im, r[1] * im, r[2] * im, r[3] * im])
}
unsafe extern "C" fn mjs_v4_length1(a: u64) -> u64 {
    rt_float(v4_length(floats(a)))
}
unsafe extern "C" fn mjs_v4_length_squared1(a: u64) -> u64 {
    let a = floats(a);
    rt_float(a[0] * a[0] + a[1] * a[1] + a[2] * a[2] + a[3] * a[3])
}
unsafe extern "C" fn mjs_v4_distance2(a: u64, b: u64) -> u64 {
    let (a, b) = (floats(a), floats(b));
    let dx = a[0] - b[0];
    let dy = a[1] - b[1];
    let dz = a[2] - b[2];
    let dw = a[3] - b[3];
    rt_float((dx * dx + dy * dy + dz * dz + dw * dw).sqrt())
}
unsafe extern "C" fn mjs_v4_distance_squared2(a: u64, b: u64) -> u64 {
    let (a, b) = (floats(a), floats(b));
    let dx = a[0] - b[0];
    let dy = a[1] - b[1];
    let dz = a[2] - b[2];
    let dw = a[3] - b[3];
    rt_float(dx * dx + dy * dy + dz * dz + dw * dw)
}
unsafe extern "C" fn mjs_v4_normalize1(a: u64) -> u64 {
    let a = floats(a);
    let im = 1.0 / v4_length(a);
    mk_floats(vec![a[0] * im, a[1] * im, a[2] * im, a[3] * im])
}
unsafe extern "C" fn mjs_v4_scale2(k: u64, a: u64) -> u64 {
    let k = num(k);
    let a = floats(a);
    mk_floats(vec![a[0] * k, a[1] * k, a[2] * k, a[3] * k])
}
unsafe extern "C" fn mjs_v4_dot2(a: u64, b: u64) -> u64 {
    let (a, b) = (floats(a), floats(b));
    rt_float(a[0] * b[0] + a[1] * b[1] + a[2] * b[2] + a[3] * b[3])
}

// Matrix4 (column-major, like the JS kernel: m[1] is row 2 / column 1).

unsafe extern "C" fn mjs_m4x4_from_record1(r: u64) -> u64 {
    mk_floats(vec![
        num(rt_access(r, b"m11\0".as_ptr())),
        num(rt_access(r, b"m21\0".as_ptr())),
        num(rt_access(r, b"m31\0".as_ptr())),
        num(rt_access(r, b"m41\0".as_ptr())),
        num(rt_access(r, b"m12\0".as_ptr())),
        num(rt_access(r, b"m22\0".as_ptr())),
        num(rt_access(r, b"m32\0".as_ptr())),
        num(rt_access(r, b"m42\0".as_ptr())),
        num(rt_access(r, b"m13\0".as_ptr())),
        num(rt_access(r, b"m23\0".as_ptr())),
        num(rt_access(r, b"m33\0".as_ptr())),
        num(rt_access(r, b"m43\0".as_ptr())),
        num(rt_access(r, b"m14\0".as_ptr())),
        num(rt_access(r, b"m24\0".as_ptr())),
        num(rt_access(r, b"m34\0".as_ptr())),
        num(rt_access(r, b"m44\0".as_ptr())),
    ])
}
unsafe extern "C" fn mjs_m4x4_to_record1(m: u64) -> u64 {
    // Fields in alphabetical order (m11, m12, ..., m44); field mIJ (row I,
    // column J) reads the column-major slot m[(J-1)*4 + (I-1)].
    let m = floats(m);
    let rec = rt_record_new(16);
    rt_record_set(rec, 0, b"m11\0".as_ptr(), rt_float(m[0]));
    rt_record_set(rec, 1, b"m12\0".as_ptr(), rt_float(m[4]));
    rt_record_set(rec, 2, b"m13\0".as_ptr(), rt_float(m[8]));
    rt_record_set(rec, 3, b"m14\0".as_ptr(), rt_float(m[12]));
    rt_record_set(rec, 4, b"m21\0".as_ptr(), rt_float(m[1]));
    rt_record_set(rec, 5, b"m22\0".as_ptr(), rt_float(m[5]));
    rt_record_set(rec, 6, b"m23\0".as_ptr(), rt_float(m[9]));
    rt_record_set(rec, 7, b"m24\0".as_ptr(), rt_float(m[13]));
    rt_record_set(rec, 8, b"m31\0".as_ptr(), rt_float(m[2]));
    rt_record_set(rec, 9, b"m32\0".as_ptr(), rt_float(m[6]));
    rt_record_set(rec, 10, b"m33\0".as_ptr(), rt_float(m[10]));
    rt_record_set(rec, 11, b"m34\0".as_ptr(), rt_float(m[14]));
    rt_record_set(rec, 12, b"m41\0".as_ptr(), rt_float(m[3]));
    rt_record_set(rec, 13, b"m42\0".as_ptr(), rt_float(m[7]));
    rt_record_set(rec, 14, b"m43\0".as_ptr(), rt_float(m[11]));
    rt_record_set(rec, 15, b"m44\0".as_ptr(), rt_float(m[15]));
    rec
}
unsafe extern "C" fn mjs_m4x4_inverse1(mw: u64) -> u64 {
    let m = floats(mw);
    let mut r = [0.0f64; 16];

    r[0] = m[5] * m[10] * m[15] - m[5] * m[11] * m[14] - m[9] * m[6] * m[15]
        + m[9] * m[7] * m[14] + m[13] * m[6] * m[11] - m[13] * m[7] * m[10];
    r[4] = -m[4] * m[10] * m[15] + m[4] * m[11] * m[14] + m[8] * m[6] * m[15]
        - m[8] * m[7] * m[14] - m[12] * m[6] * m[11] + m[12] * m[7] * m[10];
    r[8] = m[4] * m[9] * m[15] - m[4] * m[11] * m[13] - m[8] * m[5] * m[15]
        + m[8] * m[7] * m[13] + m[12] * m[5] * m[11] - m[12] * m[7] * m[9];
    r[12] = -m[4] * m[9] * m[14] + m[4] * m[10] * m[13] + m[8] * m[5] * m[14]
        - m[8] * m[6] * m[13] - m[12] * m[5] * m[10] + m[12] * m[6] * m[9];
    r[1] = -m[1] * m[10] * m[15] + m[1] * m[11] * m[14] + m[9] * m[2] * m[15]
        - m[9] * m[3] * m[14] - m[13] * m[2] * m[11] + m[13] * m[3] * m[10];
    r[5] = m[0] * m[10] * m[15] - m[0] * m[11] * m[14] - m[8] * m[2] * m[15]
        + m[8] * m[3] * m[14] + m[12] * m[2] * m[11] - m[12] * m[3] * m[10];
    r[9] = -m[0] * m[9] * m[15] + m[0] * m[11] * m[13] + m[8] * m[1] * m[15]
        - m[8] * m[3] * m[13] - m[12] * m[1] * m[11] + m[12] * m[3] * m[9];
    r[13] = m[0] * m[9] * m[14] - m[0] * m[10] * m[13] - m[8] * m[1] * m[14]
        + m[8] * m[2] * m[13] + m[12] * m[1] * m[10] - m[12] * m[2] * m[9];
    r[2] = m[1] * m[6] * m[15] - m[1] * m[7] * m[14] - m[5] * m[2] * m[15]
        + m[5] * m[3] * m[14] + m[13] * m[2] * m[7] - m[13] * m[3] * m[6];
    r[6] = -m[0] * m[6] * m[15] + m[0] * m[7] * m[14] + m[4] * m[2] * m[15]
        - m[4] * m[3] * m[14] - m[12] * m[2] * m[7] + m[12] * m[3] * m[6];
    r[10] = m[0] * m[5] * m[15] - m[0] * m[7] * m[13] - m[4] * m[1] * m[15]
        + m[4] * m[3] * m[13] + m[12] * m[1] * m[7] - m[12] * m[3] * m[5];
    r[14] = -m[0] * m[5] * m[14] + m[0] * m[6] * m[13] + m[4] * m[1] * m[14]
        - m[4] * m[2] * m[13] - m[12] * m[1] * m[6] + m[12] * m[2] * m[5];
    r[3] = -m[1] * m[6] * m[11] + m[1] * m[7] * m[10] + m[5] * m[2] * m[11]
        - m[5] * m[3] * m[10] - m[9] * m[2] * m[7] + m[9] * m[3] * m[6];
    r[7] = m[0] * m[6] * m[11] - m[0] * m[7] * m[10] - m[4] * m[2] * m[11]
        + m[4] * m[3] * m[10] + m[8] * m[2] * m[7] - m[8] * m[3] * m[6];
    r[11] = -m[0] * m[5] * m[11] + m[0] * m[7] * m[9] + m[4] * m[1] * m[11]
        - m[4] * m[3] * m[9] - m[8] * m[1] * m[7] + m[8] * m[3] * m[5];
    r[15] = m[0] * m[5] * m[10] - m[0] * m[6] * m[9] - m[4] * m[1] * m[10]
        + m[4] * m[2] * m[9] + m[8] * m[1] * m[6] - m[8] * m[2] * m[5];

    let mut det = m[0] * r[0] + m[1] * r[4] + m[2] * r[8] + m[3] * r[12];

    if det == 0.0 {
        return nothing();
    }

    det = 1.0 / det;

    for x in r.iter_mut() {
        *x *= det;
    }

    just(mk_floats(r.to_vec()))
}
fn m4_transpose(m: &[f64]) -> [f64; 16] {
    [
        m[0], m[4], m[8], m[12],
        m[1], m[5], m[9], m[13],
        m[2], m[6], m[10], m[14],
        m[3], m[7], m[11], m[15],
    ]
}
unsafe extern "C" fn mjs_m4x4_inverse_orthonormal1(mw: u64) -> u64 {
    let m = floats(mw);
    let mut r = m4_transpose(m);
    let t = [m[12], m[13], m[14]];
    r[3] = 0.0;
    r[7] = 0.0;
    r[11] = 0.0;
    r[12] = -v3_dot(&[r[0], r[4], r[8]], &t);
    r[13] = -v3_dot(&[r[1], r[5], r[9]], &t);
    r[14] = -v3_dot(&[r[2], r[6], r[10]], &t);
    mk_floats(r.to_vec())
}
fn m4_make_frustum(left: f64, right: f64, bottom: f64, top: f64, znear: f64, zfar: f64) -> Vec<f64> {
    vec![
        2.0 * znear / (right - left),
        0.0,
        0.0,
        0.0,
        0.0,
        2.0 * znear / (top - bottom),
        0.0,
        0.0,
        (right + left) / (right - left),
        (top + bottom) / (top - bottom),
        -(zfar + znear) / (zfar - znear),
        -1.0,
        0.0,
        0.0,
        -2.0 * zfar * znear / (zfar - znear),
        0.0,
    ]
}
unsafe extern "C" fn mjs_m4x4_make_frustum6(
    left: u64,
    right: u64,
    bottom: u64,
    top: u64,
    znear: u64,
    zfar: u64,
) -> u64 {
    mk_floats(m4_make_frustum(
        num(left),
        num(right),
        num(bottom),
        num(top),
        num(znear),
        num(zfar),
    ))
}
unsafe extern "C" fn mjs_m4x4_make_perspective4(fovy: u64, aspect: u64, znear: u64, zfar: u64) -> u64 {
    let (fovy, aspect, znear, zfar) = (num(fovy), num(aspect), num(znear), num(zfar));
    let ymax = znear * js_tan(fovy * std::f64::consts::PI / 360.0);
    let ymin = -ymax;
    let xmin = ymin * aspect;
    let xmax = ymax * aspect;
    mk_floats(m4_make_frustum(xmin, xmax, ymin, ymax, znear, zfar))
}
fn m4_make_ortho(left: f64, right: f64, bottom: f64, top: f64, znear: f64, zfar: f64) -> Vec<f64> {
    vec![
        2.0 / (right - left),
        0.0,
        0.0,
        0.0,
        0.0,
        2.0 / (top - bottom),
        0.0,
        0.0,
        0.0,
        0.0,
        -2.0 / (zfar - znear),
        0.0,
        -(right + left) / (right - left),
        -(top + bottom) / (top - bottom),
        -(zfar + znear) / (zfar - znear),
        1.0,
    ]
}
unsafe extern "C" fn mjs_m4x4_make_ortho6(
    left: u64,
    right: u64,
    bottom: u64,
    top: u64,
    znear: u64,
    zfar: u64,
) -> u64 {
    mk_floats(m4_make_ortho(
        num(left),
        num(right),
        num(bottom),
        num(top),
        num(znear),
        num(zfar),
    ))
}
unsafe extern "C" fn mjs_m4x4_make_ortho2d4(left: u64, right: u64, bottom: u64, top: u64) -> u64 {
    mk_floats(m4_make_ortho(num(left), num(right), num(bottom), num(top), -1.0, 1.0))
}
fn m4_mul(a: &[f64], b: &[f64]) -> Vec<f64> {
    let a11 = a[0];
    let a21 = a[1];
    let a31 = a[2];
    let a41 = a[3];
    let a12 = a[4];
    let a22 = a[5];
    let a32 = a[6];
    let a42 = a[7];
    let a13 = a[8];
    let a23 = a[9];
    let a33 = a[10];
    let a43 = a[11];
    let a14 = a[12];
    let a24 = a[13];
    let a34 = a[14];
    let a44 = a[15];
    let b11 = b[0];
    let b21 = b[1];
    let b31 = b[2];
    let b41 = b[3];
    let b12 = b[4];
    let b22 = b[5];
    let b32 = b[6];
    let b42 = b[7];
    let b13 = b[8];
    let b23 = b[9];
    let b33 = b[10];
    let b43 = b[11];
    let b14 = b[12];
    let b24 = b[13];
    let b34 = b[14];
    let b44 = b[15];
    vec![
        a11 * b11 + a12 * b21 + a13 * b31 + a14 * b41,
        a21 * b11 + a22 * b21 + a23 * b31 + a24 * b41,
        a31 * b11 + a32 * b21 + a33 * b31 + a34 * b41,
        a41 * b11 + a42 * b21 + a43 * b31 + a44 * b41,
        a11 * b12 + a12 * b22 + a13 * b32 + a14 * b42,
        a21 * b12 + a22 * b22 + a23 * b32 + a24 * b42,
        a31 * b12 + a32 * b22 + a33 * b32 + a34 * b42,
        a41 * b12 + a42 * b22 + a43 * b32 + a44 * b42,
        a11 * b13 + a12 * b23 + a13 * b33 + a14 * b43,
        a21 * b13 + a22 * b23 + a23 * b33 + a24 * b43,
        a31 * b13 + a32 * b23 + a33 * b33 + a34 * b43,
        a41 * b13 + a42 * b23 + a43 * b33 + a44 * b43,
        a11 * b14 + a12 * b24 + a13 * b34 + a14 * b44,
        a21 * b14 + a22 * b24 + a23 * b34 + a24 * b44,
        a31 * b14 + a32 * b24 + a33 * b34 + a34 * b44,
        a41 * b14 + a42 * b24 + a43 * b34 + a44 * b44,
    ]
}
unsafe extern "C" fn mjs_m4x4_mul2(a: u64, b: u64) -> u64 {
    mk_floats(m4_mul(floats(a), floats(b)))
}
unsafe extern "C" fn mjs_m4x4_mul_affine2(a: u64, b: u64) -> u64 {
    let (a, b) = (floats(a), floats(b));
    let a11 = a[0];
    let a21 = a[1];
    let a31 = a[2];
    let a12 = a[4];
    let a22 = a[5];
    let a32 = a[6];
    let a13 = a[8];
    let a23 = a[9];
    let a33 = a[10];
    let a14 = a[12];
    let a24 = a[13];
    let a34 = a[14];
    let b11 = b[0];
    let b21 = b[1];
    let b31 = b[2];
    let b12 = b[4];
    let b22 = b[5];
    let b32 = b[6];
    let b13 = b[8];
    let b23 = b[9];
    let b33 = b[10];
    let b14 = b[12];
    let b24 = b[13];
    let b34 = b[14];
    mk_floats(vec![
        a11 * b11 + a12 * b21 + a13 * b31,
        a21 * b11 + a22 * b21 + a23 * b31,
        a31 * b11 + a32 * b21 + a33 * b31,
        0.0,
        a11 * b12 + a12 * b22 + a13 * b32,
        a21 * b12 + a22 * b22 + a23 * b32,
        a31 * b12 + a32 * b22 + a33 * b32,
        0.0,
        a11 * b13 + a12 * b23 + a13 * b33,
        a21 * b13 + a22 * b23 + a23 * b33,
        a31 * b13 + a32 * b23 + a33 * b33,
        0.0,
        a11 * b14 + a12 * b24 + a13 * b34 + a14,
        a21 * b14 + a22 * b24 + a23 * b34 + a24,
        a31 * b14 + a32 * b24 + a33 * b34 + a34,
        1.0,
    ])
}
unsafe extern "C" fn mjs_m4x4_make_rotate2(angle: u64, axis: u64) -> u64 {
    let angle = num(angle);
    let axis = v3_normalize(floats(axis));
    let x = axis[0];
    let y = axis[1];
    let z = axis[2];
    let c = angle.cos();
    let c1 = 1.0 - c;
    let s = angle.sin();
    mk_floats(vec![
        x * x * c1 + c,
        y * x * c1 + z * s,
        z * x * c1 - y * s,
        0.0,
        x * y * c1 - z * s,
        y * y * c1 + c,
        y * z * c1 + x * s,
        0.0,
        x * z * c1 + y * s,
        y * z * c1 - x * s,
        z * z * c1 + c,
        0.0,
        0.0,
        0.0,
        0.0,
        1.0,
    ])
}
unsafe extern "C" fn mjs_m4x4_rotate3(angle: u64, axis: u64, m: u64) -> u64 {
    let angle = num(angle);
    let axis = floats(axis);
    let m = floats(m);
    let im = 1.0 / v3_length(axis);
    let x = axis[0] * im;
    let y = axis[1] * im;
    let z = axis[2] * im;
    let c = angle.cos();
    let c1 = 1.0 - c;
    let s = angle.sin();
    let xs = x * s;
    let ys = y * s;
    let zs = z * s;
    let xyc1 = x * y * c1;
    let xzc1 = x * z * c1;
    let yzc1 = y * z * c1;
    let t11 = x * x * c1 + c;
    let t21 = xyc1 + zs;
    let t31 = xzc1 - ys;
    let t12 = xyc1 - zs;
    let t22 = y * y * c1 + c;
    let t32 = yzc1 + xs;
    let t13 = xzc1 + ys;
    let t23 = yzc1 - xs;
    let t33 = z * z * c1 + c;
    let (m11, m21, m31, m41) = (m[0], m[1], m[2], m[3]);
    let (m12, m22, m32, m42) = (m[4], m[5], m[6], m[7]);
    let (m13, m23, m33, m43) = (m[8], m[9], m[10], m[11]);
    let (m14, m24, m34, m44) = (m[12], m[13], m[14], m[15]);
    mk_floats(vec![
        m11 * t11 + m12 * t21 + m13 * t31,
        m21 * t11 + m22 * t21 + m23 * t31,
        m31 * t11 + m32 * t21 + m33 * t31,
        m41 * t11 + m42 * t21 + m43 * t31,
        m11 * t12 + m12 * t22 + m13 * t32,
        m21 * t12 + m22 * t22 + m23 * t32,
        m31 * t12 + m32 * t22 + m33 * t32,
        m41 * t12 + m42 * t22 + m43 * t32,
        m11 * t13 + m12 * t23 + m13 * t33,
        m21 * t13 + m22 * t23 + m23 * t33,
        m31 * t13 + m32 * t23 + m33 * t33,
        m41 * t13 + m42 * t23 + m43 * t33,
        m14,
        m24,
        m34,
        m44,
    ])
}
fn m4_make_scale3(x: f64, y: f64, z: f64) -> Vec<f64> {
    vec![
        x, 0.0, 0.0, 0.0,
        0.0, y, 0.0, 0.0,
        0.0, 0.0, z, 0.0,
        0.0, 0.0, 0.0, 1.0,
    ]
}
unsafe extern "C" fn mjs_m4x4_make_scale3_3(x: u64, y: u64, z: u64) -> u64 {
    mk_floats(m4_make_scale3(num(x), num(y), num(z)))
}
unsafe extern "C" fn mjs_m4x4_make_scale1(v: u64) -> u64 {
    let v = floats(v);
    mk_floats(m4_make_scale3(v[0], v[1], v[2]))
}
unsafe fn m4_scale3(x: f64, y: f64, z: f64, m: &[f64]) -> u64 {
    mk_floats(vec![
        m[0] * x,
        m[1] * x,
        m[2] * x,
        m[3] * x,
        m[4] * y,
        m[5] * y,
        m[6] * y,
        m[7] * y,
        m[8] * z,
        m[9] * z,
        m[10] * z,
        m[11] * z,
        m[12],
        m[13],
        m[14],
        m[15],
    ])
}
unsafe extern "C" fn mjs_m4x4_scale3_4(x: u64, y: u64, z: u64, m: u64) -> u64 {
    m4_scale3(num(x), num(y), num(z), floats(m))
}
unsafe extern "C" fn mjs_m4x4_scale2(v: u64, m: u64) -> u64 {
    let v = floats(v);
    m4_scale3(v[0], v[1], v[2], floats(m))
}
fn m4_make_translate3(x: f64, y: f64, z: f64) -> Vec<f64> {
    vec![
        1.0, 0.0, 0.0, 0.0,
        0.0, 1.0, 0.0, 0.0,
        0.0, 0.0, 1.0, 0.0,
        x, y, z, 1.0,
    ]
}
unsafe extern "C" fn mjs_m4x4_make_translate3_3(x: u64, y: u64, z: u64) -> u64 {
    mk_floats(m4_make_translate3(num(x), num(y), num(z)))
}
unsafe extern "C" fn mjs_m4x4_make_translate1(v: u64) -> u64 {
    let v = floats(v);
    mk_floats(m4_make_translate3(v[0], v[1], v[2]))
}
unsafe fn m4_translate3(x: f64, y: f64, z: f64, m: &[f64]) -> u64 {
    let (m11, m21, m31, m41) = (m[0], m[1], m[2], m[3]);
    let (m12, m22, m32, m42) = (m[4], m[5], m[6], m[7]);
    let (m13, m23, m33, m43) = (m[8], m[9], m[10], m[11]);
    mk_floats(vec![
        m11,
        m21,
        m31,
        m41,
        m12,
        m22,
        m32,
        m42,
        m13,
        m23,
        m33,
        m43,
        m11 * x + m12 * y + m13 * z + m[12],
        m21 * x + m22 * y + m23 * z + m[13],
        m31 * x + m32 * y + m33 * z + m[14],
        m41 * x + m42 * y + m43 * z + m[15],
    ])
}
unsafe extern "C" fn mjs_m4x4_translate3_4(x: u64, y: u64, z: u64, m: u64) -> u64 {
    m4_translate3(num(x), num(y), num(z), floats(m))
}
unsafe extern "C" fn mjs_m4x4_translate2(v: u64, m: u64) -> u64 {
    let v = floats(v);
    m4_translate3(v[0], v[1], v[2], floats(m))
}
unsafe extern "C" fn mjs_m4x4_make_look_at3(eye: u64, center: u64, up: u64) -> u64 {
    let (eye, center, up) = (floats(eye), floats(center), floats(up));
    let z = v3_direction(eye, center);
    let x = v3_normalize(&v3_cross(up, &z));
    let y = v3_normalize(&v3_cross(&z, &x));
    let tm1 = [
        x[0], y[0], z[0], 0.0,
        x[1], y[1], z[1], 0.0,
        x[2], y[2], z[2], 0.0,
        0.0, 0.0, 0.0, 1.0,
    ];
    let tm2 = [
        1.0, 0.0, 0.0, 0.0,
        0.0, 1.0, 0.0, 0.0,
        0.0, 0.0, 1.0, 0.0,
        -eye[0], -eye[1], -eye[2], 1.0,
    ];
    mk_floats(m4_mul(&tm1, &tm2))
}
unsafe extern "C" fn mjs_m4x4_transpose1(m: u64) -> u64 {
    mk_floats(m4_transpose(floats(m)).to_vec())
}
unsafe extern "C" fn mjs_m4x4_make_basis3(vx: u64, vy: u64, vz: u64) -> u64 {
    let (vx, vy, vz) = (floats(vx), floats(vy), floats(vz));
    mk_floats(vec![
        vx[0], vx[1], vx[2], 0.0,
        vy[0], vy[1], vy[2], 0.0,
        vz[0], vz[1], vz[2], 0.0,
        0.0, 0.0, 0.0, 1.0,
    ])
}

// --- HtmlAsJson: reflect the native vdom into the elm/virtual-dom JSON shape
// that elm-explorations/test's Test.Html decoders read (the Rust twin of
// runtime.js's `_HtmlAsJson_*`). Node kinds map to `$`: 0 text, 1 node, 2 keyed,
// 4 tagger; facts bucket into a0 (events), a1 (styles), a3 (attributes), a4
// (namespaced attributes), plain properties at top level, merged classes under
// `className`. Live leaves (event decoders, `map` taggers) ride along as
// `JsonValue::Elm`, recovered by eventHandler/taggerFunction.

fn jfield(key: &[u8], v: JsonValue) -> (Vec<u8>, JsonValue) {
    (key.to_vec(), v)
}

unsafe fn jstr_of(w: u64) -> JsonValue {
    JsonValue::JStr(str_slice(w).to_vec())
}

// A DOM property / attribute value as JSON, keeping non-string values (bools,
// numbers, wrapped Json.Value) faithful and carrying anything else opaquely.
unsafe fn val_to_json(w: u64) -> JsonValue {
    if is_int(w) {
        return JsonValue::Number(int_val(w) as f64);
    }
    match deref(w) {
        Value::Str(_) | Value::StrCat { .. } | Value::StrSlice { .. } => {
            JsonValue::JStr(sbytes(w).to_vec())
        }
        Value::Bool(b) => JsonValue::Bool(*b),
        Value::Float(f) => JsonValue::Number(*f),
        Value::Json(j) => j.clone(),
        _ => JsonValue::Elm(w),
    }
}

fn add_class(classes: &mut Option<Vec<u8>>, v: &[u8]) {
    match classes {
        None => *classes = Some(v.to_vec()),
        Some(c) => {
            c.push(b' ');
            c.extend_from_slice(v);
        }
    }
}

/// The descendant count `.b` of an already-translated node (0 when absent, e.g.
/// text nodes), so parents can sum `1 + countOf(child)` like the JS translator.
fn json_count_of(v: &JsonValue) -> f64 {
    if let JsonValue::JObject(fs) = v {
        for (k, val) in fs {
            if k == b"b" {
                if let JsonValue::Number(n) = val {
                    return *n;
                }
            }
        }
    }
    0.0
}

// _HtmlAsJson_handler: an AEvent → the `VirtualDom.Handler` union value the
// event carries. In JS a union and a `{$:tag,a:x}` object share a shape, so the
// JS translator emits a plain object; natively a union is a real `Value::Ctor`,
// so we build one (embedded opaquely) — `eventHandler` hands it back and
// Test.Html pattern-matches it. Ctor indices match VirtualDom.Handler's order:
// Normal 0, MayStopPropagation 1, MayPreventDefault 2, Custom 3.
unsafe fn html_handler(attr: u64) -> JsonValue {
    let decoder = ctor_get(attr, 1);
    let (name, index): (&[u8], u32) = match int_val(ctor_get(attr, 2)) {
        3 => (b"Custom\0", 3),
        1 => (b"MayStopPropagation\0", 1),
        2 => (b"MayPreventDefault\0", 2),
        _ => (b"Normal\0", 0),
    };
    JsonValue::Elm(ctor1(name.as_ptr(), index, decoder))
}

// _HtmlAsJson_facts.
/// A DOM property's string content: the class/property kernels store a plain
/// `Str`, while `VirtualDom.property` stores the `Json.Encode.Value` itself
/// (a `Json(JStr)` for encoded strings). Both spell the same JS string.
unsafe fn prop_str<'a>(w: u64) -> &'a [u8] {
    if let Value::Json(JsonValue::JStr(s)) = deref(w) {
        return s;
    }
    str_slice(w)
}

unsafe fn html_facts(attrs: u64) -> JsonValue {
    let mut styles: Vec<(Vec<u8>, JsonValue)> = Vec::new();
    let mut string_attrs: Vec<(Vec<u8>, JsonValue)> = Vec::new();
    let mut ns_attrs: Vec<(Vec<u8>, JsonValue)> = Vec::new();
    let mut events: Vec<(Vec<u8>, JsonValue)> = Vec::new();
    let mut props: Vec<(Vec<u8>, JsonValue)> = Vec::new();
    let mut classes: Option<Vec<u8>> = None;
    for &a in &to_vec(attrs) {
        if let Value::Ctor { index, arg0, rest, .. } = deref(a) {
            let key = *arg0;
            match *index {
                AI_STYLE => styles.push((str_slice(key).to_vec(), jstr_of(rest[0]))),
                AI_ATTR => {
                    if rest[1] != nothing() {
                        ns_attrs.push((
                            str_slice(key).to_vec(),
                            JsonValue::JObject(vec![
                                jfield(b"o", jstr_of(rest[0])),
                                jfield(b"f", jstr_of(ctor_get(rest[1], 0))),
                            ]),
                        ));
                    } else if str_is(key, b"class") {
                        add_class(&mut classes, str_slice(rest[0]));
                    } else {
                        string_attrs.push((str_slice(key).to_vec(), jstr_of(rest[0])));
                    }
                }
                AI_PROP => {
                    if str_is(key, b"className") {
                        // A property VALUE is a `Json.Encode.Value`: the class
                        // kernels build a plain Str, but code going through
                        // `VirtualDom.property "className" (Encode.string s)`
                        // (elm-css, origami) carries Json — read either. On the
                        // JS side both are raw strings, so JS never noticed.
                        add_class(&mut classes, prop_str(rest[0]));
                    } else {
                        props.push((str_slice(key).to_vec(), val_to_json(rest[0])));
                    }
                }
                AI_EVENT => events.push((
                    str_slice(key).to_vec(),
                    JsonValue::JObject(vec![jfield(b"a", html_handler(a))]),
                )),
                _ => {}
            }
        }
    }
    let mut facts: Vec<(Vec<u8>, JsonValue)> = Vec::new();
    if !events.is_empty() {
        facts.push(jfield(b"a0", JsonValue::JObject(events)));
    }
    if !styles.is_empty() {
        facts.push(jfield(b"a1", JsonValue::JObject(styles)));
    }
    if !string_attrs.is_empty() {
        facts.push(jfield(b"a3", JsonValue::JObject(string_attrs)));
    }
    if !ns_attrs.is_empty() {
        facts.push(jfield(b"a4", JsonValue::JObject(ns_attrs)));
    }
    facts.extend(props);
    if let Some(c) = classes {
        facts.push(jfield(b"className", JsonValue::JStr(c)));
    }
    JsonValue::JObject(facts)
}

unsafe fn html_element_obj(
    tag_num: f64,
    tag: u64,
    attrs: u64,
    kids: Vec<JsonValue>,
    count: f64,
    ns: u64,
) -> JsonValue {
    let mut fields = vec![
        jfield(b"$", JsonValue::Number(tag_num)),
        jfield(b"c", jstr_of(tag)),
        jfield(b"d", html_facts(attrs)),
        jfield(b"e", JsonValue::JArray(kids)),
        jfield(b"b", JsonValue::Number(count)),
    ];
    // `f` (namespace) only for namespaced nodes; a normal node omits it, which
    // the decoder reads the same as JS's `f: undefined` (a missing field).
    if ns != nothing() {
        fields.push(jfield(b"f", jstr_of(ctor_get(ns, 0))));
    }
    JsonValue::JObject(fields)
}

// _HtmlAsJson_translate.
unsafe fn html_translate(node: u64) -> JsonValue {
    let index = match deref(node) {
        Value::Ctor { index, .. } => *index,
        _ => return JsonValue::Null,
    };
    match index {
        VI_TEXT => JsonValue::JObject(vec![
            jfield(b"$", JsonValue::Number(0.0)),
            jfield(b"a", jstr_of(ctor_get(node, 0))),
        ]),
        VI_MAP => JsonValue::JObject(vec![
            jfield(b"$", JsonValue::Number(4.0)),
            jfield(
                b"j",
                JsonValue::JObject(vec![jfield(b"a", JsonValue::Elm(ctor_get(node, 0)))]),
            ),
            jfield(b"k", html_translate(ctor_get(node, 1))),
        ]),
        VI_KEYED => {
            let mut e: Vec<JsonValue> = Vec::new();
            let mut count = 0.0;
            for &pair in &to_vec(ctor_get(node, 2)) {
                let kt = html_translate(tuple_second(pair));
                count += 1.0 + json_count_of(&kt);
                e.push(JsonValue::JObject(vec![
                    jfield(b"a", jstr_of(tuple_first(pair))),
                    jfield(b"b", kt),
                ]));
            }
            html_element_obj(2.0, ctor_get(node, 0), ctor_get(node, 1), e, count, ctor_get(node, 3))
        }
        VI_NODE => {
            let mut e: Vec<JsonValue> = Vec::new();
            let mut count = 0.0;
            for &kid in &to_vec(ctor_get(node, 2)) {
                let kt = html_translate(kid);
                count += 1.0 + json_count_of(&kt);
                e.push(kt);
            }
            html_element_obj(1.0, ctor_get(node, 0), ctor_get(node, 1), e, count, ctor_get(node, 3))
        }
        _ => JsonValue::Null,
    }
}

// _HtmlAsJson_attribute.
unsafe fn html_attribute(attr: u64) -> JsonValue {
    let (index, key, rest) = match deref(attr) {
        Value::Ctor { index, arg0, rest, .. } => (*index, *arg0, rest),
        _ => return JsonValue::Null,
    };
    match index {
        AI_STYLE => JsonValue::JObject(vec![
            jfield(b"$", JsonValue::JStr(b"a1".to_vec())),
            jfield(b"n", jstr_of(key)),
            jfield(b"o", jstr_of(rest[0])),
        ]),
        AI_PROP => JsonValue::JObject(vec![
            jfield(b"$", JsonValue::JStr(b"a2".to_vec())),
            jfield(b"n", jstr_of(key)),
            jfield(b"o", JsonValue::JObject(vec![jfield(b"a", val_to_json(rest[0]))])),
        ]),
        AI_EVENT => JsonValue::JObject(vec![
            jfield(b"$", JsonValue::JStr(b"a0".to_vec())),
            jfield(b"n", jstr_of(key)),
            jfield(
                b"o",
                JsonValue::JObject(vec![
                    jfield(b"a", JsonValue::Elm(rest[0])),
                    jfield(b"opts", JsonValue::Number(int_val(rest[1]) as f64)),
                ]),
            ),
        ]),
        AI_ATTR if rest[1] != nothing() => JsonValue::JObject(vec![
            jfield(b"$", JsonValue::JStr(b"a4".to_vec())),
            jfield(b"n", jstr_of(key)),
            jfield(
                b"o",
                JsonValue::JObject(vec![
                    jfield(b"o", jstr_of(rest[0])),
                    jfield(b"f", jstr_of(ctor_get(rest[1], 0))),
                ]),
            ),
        ]),
        AI_ATTR => JsonValue::JObject(vec![
            jfield(b"$", JsonValue::JStr(b"a3".to_vec())),
            jfield(b"n", jstr_of(key)),
            jfield(b"o", jstr_of(rest[0])),
        ]),
        _ => JsonValue::Null,
    }
}

/// Read an object field and hand back the embedded Elm value it carries
/// (eventHandler/taggerFunction recover the live decoder / tagger from the
/// reflected JSON). A non-Elm field is re-wrapped as a `Json.Value`.
unsafe fn json_get_elm_field(w: u64, key: &[u8]) -> u64 {
    if let Value::Json(JsonValue::JObject(fs)) = deref(w) {
        for (k, v) in fs {
            if k.as_slice() == key {
                return match v {
                    JsonValue::Elm(e) => *e,
                    other => mk_json(other.clone()),
                };
            }
        }
    }
    mk_json(JsonValue::Null)
}

unsafe extern "C" fn html_to_json(html: u64) -> u64 {
    mk_json(html_translate(html))
}
unsafe extern "C" fn html_attribute_to_json(attr: u64) -> u64 {
    mk_json(html_attribute(attr))
}
unsafe extern "C" fn html_event_handler(h: u64) -> u64 {
    json_get_elm_field(h, b"a")
}
unsafe extern "C" fn html_tagger_function(t: u64) -> u64 {
    json_get_elm_field(t, b"a")
}

// --- elm/url (the Rust twin of runtime.js's $Url$* / _Url_*) ---

fn hex_digit(n: u8) -> u8 {
    if n < 10 { b'0' + n } else { b'A' + (n - 10) }
}
fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}
unsafe fn is_nothing(w: u64) -> bool {
    !is_int(w) && matches!(deref(w), Value::Ctor { index: 1, argc: 0, .. })
}

// percentEncode = encodeURIComponent: keep the unreserved set (letters, digits,
// and -_.!~*'()), percent-encode every other byte of the UTF-8 form.
unsafe extern "C" fn url_percent_encode(s: u64) -> u64 {
    let mut out: Vec<u8> = Vec::new();
    for &b in str_slice(s) {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')') {
            out.push(b);
        } else {
            out.push(b'%');
            out.push(hex_digit(b >> 4));
            out.push(hex_digit(b & 0xf));
        }
    }
    mkstr(out)
}

// percentDecode = decodeURIComponent: decode %XX to bytes then require valid
// UTF-8; `Nothing` on any malformed escape or invalid UTF-8 (the try/catch).
unsafe extern "C" fn url_percent_decode(s: u64) -> u64 {
    let bytes = str_slice(s);
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return nothing();
            }
            match (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push((h << 4) | l);
                    i += 3;
                }
                _ => return nothing(),
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    match std::str::from_utf8(&out) {
        Ok(_) => just(mkstr(out)),
        Err(_) => nothing(),
    }
}

// Build the `Url` record (fields sorted by name: fragment, host, path, port_,
// protocol, query). `proto` is the Protocol tag (0 Http, 1 Https).
unsafe fn mk_url(proto: u32, host: &[u8], port_: u64, path: &[u8], query: u64, frag: u64) -> u64 {
    let protocol = if proto == 0 {
        ctor0(b"Http\0".as_ptr(), 0)
    } else {
        ctor0(b"Https\0".as_ptr(), 1)
    };
    let rec = rt_record_new(6);
    rt_record_set(rec, 0, b"fragment\0".as_ptr(), frag);
    rt_record_set(rec, 1, b"host\0".as_ptr(), mkstr(host.to_vec()));
    rt_record_set(rec, 2, b"path\0".as_ptr(), mkstr(path.to_vec()));
    rt_record_set(rec, 3, b"port_\0".as_ptr(), port_);
    rt_record_set(rec, 4, b"protocol\0".as_ptr(), protocol);
    rt_record_set(rec, 5, b"query\0".as_ptr(), query);
    rec
}

fn find(s: &[u8], c: u8) -> Option<usize> {
    s.iter().position(|&b| b == c)
}

// The parse pipeline mirrors runtime.js's _Url_chomp*. All split points are
// ASCII delimiters, so byte slicing preserves the UTF-8 substrings.
unsafe fn url_chomp_port(proto: u32, query: u64, frag: u64, authority: &[u8], path: &[u8]) -> u64 {
    match find(authority, b':') {
        None => just(mk_url(proto, authority, nothing(), path, query, frag)),
        Some(i) => {
            let port = string_to_int(mkstr(authority[i + 1..].to_vec()));
            if is_nothing(port) {
                nothing()
            } else {
                just(mk_url(proto, &authority[..i], port, path, query, frag))
            }
        }
    }
}
unsafe fn url_chomp_authority(proto: u32, query: u64, frag: u64, authority: &[u8], path: &[u8]) -> u64 {
    if authority.is_empty() {
        return nothing();
    }
    let auth = match find(authority, b'@') {
        Some(i) => &authority[i + 1..],
        None => authority,
    };
    url_chomp_port(proto, query, frag, auth, path)
}
unsafe fn url_chomp_before_query(proto: u32, query: u64, frag: u64, s: &[u8]) -> u64 {
    if s.is_empty() {
        return nothing();
    }
    match find(s, b'/') {
        // A pathless URL defaults to "/" (matching elm).
        None => url_chomp_authority(proto, query, frag, s, b"/"),
        Some(i) => url_chomp_authority(proto, query, frag, &s[..i], &s[i..]),
    }
}
unsafe fn url_chomp_before_fragment(proto: u32, frag: u64, s: &[u8]) -> u64 {
    if s.is_empty() {
        return nothing();
    }
    match find(s, b'?') {
        None => url_chomp_before_query(proto, nothing(), frag, s),
        Some(i) => url_chomp_before_query(proto, just(mkstr(s[i + 1..].to_vec())), frag, &s[..i]),
    }
}
unsafe fn url_chomp_after_protocol(proto: u32, s: &[u8]) -> u64 {
    if s.is_empty() {
        return nothing();
    }
    match find(s, b'#') {
        None => url_chomp_before_fragment(proto, nothing(), s),
        Some(i) => url_chomp_before_fragment(proto, just(mkstr(s[i + 1..].to_vec())), &s[..i]),
    }
}
unsafe extern "C" fn url_from_string(s: u64) -> u64 {
    let b = str_slice(s);
    if b.starts_with(b"http://") {
        url_chomp_after_protocol(0, &b[7..])
    } else if b.starts_with(b"https://") {
        url_chomp_after_protocol(1, &b[8..])
    } else {
        nothing()
    }
}
unsafe extern "C" fn url_to_string(url: u64) -> u64 {
    let is_https = rt_ctor_tag(rt_access(url, b"protocol\0".as_ptr())) == 1;
    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(if is_https { b"https://" } else { b"http://" });
    out.extend_from_slice(str_slice(rt_access(url, b"host\0".as_ptr())));
    let port = rt_access(url, b"port_\0".as_ptr());
    if !is_nothing(port) {
        out.push(b':');
        out.extend_from_slice(str_slice(string_from_int(ctor_get(port, 0))));
    }
    out.extend_from_slice(str_slice(rt_access(url, b"path\0".as_ptr())));
    let query = rt_access(url, b"query\0".as_ptr());
    if !is_nothing(query) {
        out.push(b'?');
        out.extend_from_slice(str_slice(ctor_get(query, 0)));
    }
    let frag = rt_access(url, b"fragment\0".as_ptr());
    if !is_nothing(frag) {
        out.push(b'#');
        out.extend_from_slice(str_slice(ctor_get(frag, 0)));
    }
    mkstr(out)
}

// Per-tag / per-attribute globals whose DOM key is baked into a closure
// capture — the native twin of runtime.js's `var $Html$div = _VDom_node('div')`
// etc. Each entry: an ident, its exported mangled symbol, and the closure
// value to initialize it with. Mirrors the tables walked in `generate::mod`
// (HTML_TAGS / HTML_*_ATTRS); keep them in sync.
macro_rules! baked_globals {
    ($( $id:ident $sym:literal = $init:expr ; )*) => {
        $( #[export_name = $sym] static $id: Global = Global::NULL; )*
        unsafe fn init_baked_globals() {
            $( $id.set($init); )*
        }
    };
}

baked_globals! {
    G_HTTP_EMPTYBODY "$Http$emptyBody" = http_empty_body0();
    // elm-explorations/linear-algebra: the identity matrix is a kernel VALUE.
    G_MJS_M4X4IDENTITY "$Elm$Kernel$MJS$m4x4identity" = mk_floats(vec![
        1.0, 0.0, 0.0, 0.0,
        0.0, 1.0, 0.0, 0.0,
        0.0, 0.0, 1.0, 0.0,
        0.0, 0.0, 0.0, 1.0,
    ]);
    G_HTMLKEYED_UL "$Html$Keyed$ul" = closure(vdom_keyed3 as *const (), 3, &[mkstr(b"ul".to_vec())]);
    G_HTMLKEYED_OL "$Html$Keyed$ol" = closure(vdom_keyed3 as *const (), 3, &[mkstr(b"ol".to_vec())]);
    // elm/svg element + attribute helpers (SVG namespace); mirrors
    // generate::mod's SVG_TAGS / SVG_ATTRS.
    G_SVGTAG_SVG "$Svg$svg" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"svg".to_vec())]);
    G_SVGTAG_FOREIGNOBJECT "$Svg$foreignObject" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"foreignObject".to_vec())]);
    G_SVGTAG_ANIMATE "$Svg$animate" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"animate".to_vec())]);
    G_SVGTAG_ANIMATECOLOR "$Svg$animateColor" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"animateColor".to_vec())]);
    G_SVGTAG_ANIMATEMOTION "$Svg$animateMotion" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"animateMotion".to_vec())]);
    G_SVGTAG_ANIMATETRANSFORM "$Svg$animateTransform" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"animateTransform".to_vec())]);
    G_SVGTAG_MPATH "$Svg$mpath" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"mpath".to_vec())]);
    G_SVGTAG_SET "$Svg$set" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"set".to_vec())]);
    G_SVGTAG_A "$Svg$a" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"a".to_vec())]);
    G_SVGTAG_DEFS "$Svg$defs" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"defs".to_vec())]);
    G_SVGTAG_G "$Svg$g" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"g".to_vec())]);
    G_SVGTAG_MARKER "$Svg$marker" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"marker".to_vec())]);
    G_SVGTAG_MASK "$Svg$mask" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"mask".to_vec())]);
    G_SVGTAG_PATTERN "$Svg$pattern" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"pattern".to_vec())]);
    G_SVGTAG_SWITCH "$Svg$switch" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"switch".to_vec())]);
    G_SVGTAG_SYMBOL "$Svg$symbol" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"symbol".to_vec())]);
    G_SVGTAG_DESC "$Svg$desc" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"desc".to_vec())]);
    G_SVGTAG_METADATA "$Svg$metadata" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"metadata".to_vec())]);
    G_SVGTAG_TITLE "$Svg$title" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"title".to_vec())]);
    G_SVGTAG_FEBLEND "$Svg$feBlend" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"feBlend".to_vec())]);
    G_SVGTAG_FECOLORMATRIX "$Svg$feColorMatrix" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"feColorMatrix".to_vec())]);
    G_SVGTAG_FECOMPONENTTRANSFER "$Svg$feComponentTransfer" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"feComponentTransfer".to_vec())]);
    G_SVGTAG_FECOMPOSITE "$Svg$feComposite" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"feComposite".to_vec())]);
    G_SVGTAG_FECONVOLVEMATRIX "$Svg$feConvolveMatrix" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"feConvolveMatrix".to_vec())]);
    G_SVGTAG_FEDIFFUSELIGHTING "$Svg$feDiffuseLighting" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"feDiffuseLighting".to_vec())]);
    G_SVGTAG_FEDISPLACEMENTMAP "$Svg$feDisplacementMap" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"feDisplacementMap".to_vec())]);
    G_SVGTAG_FEFLOOD "$Svg$feFlood" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"feFlood".to_vec())]);
    G_SVGTAG_FEFUNCA "$Svg$feFuncA" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"feFuncA".to_vec())]);
    G_SVGTAG_FEFUNCB "$Svg$feFuncB" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"feFuncB".to_vec())]);
    G_SVGTAG_FEFUNCG "$Svg$feFuncG" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"feFuncG".to_vec())]);
    G_SVGTAG_FEFUNCR "$Svg$feFuncR" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"feFuncR".to_vec())]);
    G_SVGTAG_FEGAUSSIANBLUR "$Svg$feGaussianBlur" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"feGaussianBlur".to_vec())]);
    G_SVGTAG_FEIMAGE "$Svg$feImage" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"feImage".to_vec())]);
    G_SVGTAG_FEMERGE "$Svg$feMerge" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"feMerge".to_vec())]);
    G_SVGTAG_FEMERGENODE "$Svg$feMergeNode" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"feMergeNode".to_vec())]);
    G_SVGTAG_FEMORPHOLOGY "$Svg$feMorphology" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"feMorphology".to_vec())]);
    G_SVGTAG_FEOFFSET "$Svg$feOffset" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"feOffset".to_vec())]);
    G_SVGTAG_FESPECULARLIGHTING "$Svg$feSpecularLighting" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"feSpecularLighting".to_vec())]);
    G_SVGTAG_FETILE "$Svg$feTile" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"feTile".to_vec())]);
    G_SVGTAG_FETURBULENCE "$Svg$feTurbulence" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"feTurbulence".to_vec())]);
    G_SVGTAG_FONT "$Svg$font" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"font".to_vec())]);
    G_SVGTAG_LINEARGRADIENT "$Svg$linearGradient" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"linearGradient".to_vec())]);
    G_SVGTAG_RADIALGRADIENT "$Svg$radialGradient" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"radialGradient".to_vec())]);
    G_SVGTAG_STOP "$Svg$stop" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"stop".to_vec())]);
    G_SVGTAG_CIRCLE "$Svg$circle" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"circle".to_vec())]);
    G_SVGTAG_ELLIPSE "$Svg$ellipse" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"ellipse".to_vec())]);
    G_SVGTAG_IMAGE "$Svg$image" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"image".to_vec())]);
    G_SVGTAG_LINE "$Svg$line" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"line".to_vec())]);
    G_SVGTAG_PATH "$Svg$path" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"path".to_vec())]);
    G_SVGTAG_POLYGON "$Svg$polygon" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"polygon".to_vec())]);
    G_SVGTAG_POLYLINE "$Svg$polyline" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"polyline".to_vec())]);
    G_SVGTAG_RECT "$Svg$rect" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"rect".to_vec())]);
    G_SVGTAG_USE "$Svg$use" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"use".to_vec())]);
    G_SVGTAG_FEDISTANTLIGHT "$Svg$feDistantLight" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"feDistantLight".to_vec())]);
    G_SVGTAG_FEPOINTLIGHT "$Svg$fePointLight" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"fePointLight".to_vec())]);
    G_SVGTAG_FESPOTLIGHT "$Svg$feSpotLight" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"feSpotLight".to_vec())]);
    G_SVGTAG_ALTGLYPH "$Svg$altGlyph" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"altGlyph".to_vec())]);
    G_SVGTAG_ALTGLYPHDEF "$Svg$altGlyphDef" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"altGlyphDef".to_vec())]);
    G_SVGTAG_ALTGLYPHITEM "$Svg$altGlyphItem" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"altGlyphItem".to_vec())]);
    G_SVGTAG_GLYPH "$Svg$glyph" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"glyph".to_vec())]);
    G_SVGTAG_GLYPHREF "$Svg$glyphRef" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"glyphRef".to_vec())]);
    G_SVGTAG_TEXTPATH "$Svg$textPath" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"textPath".to_vec())]);
    G_SVGTAG_TEXT_ "$Svg$text_" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"text".to_vec())]);
    G_SVGTAG_TREF "$Svg$tref" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"tref".to_vec())]);
    G_SVGTAG_TSPAN "$Svg$tspan" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"tspan".to_vec())]);
    G_SVGTAG_CLIPPATH "$Svg$clipPath" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"clipPath".to_vec())]);
    G_SVGTAG_COLORPROFILE "$Svg$colorProfile" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"colorProfile".to_vec())]);
    G_SVGTAG_CURSOR "$Svg$cursor" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"cursor".to_vec())]);
    G_SVGTAG_FILTER "$Svg$filter" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"filter".to_vec())]);
    G_SVGTAG_STYLE "$Svg$style" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"style".to_vec())]);
    G_SVGTAG_VIEW "$Svg$view" = closure(vdom_node_ns3 as *const (), 3, &[mkstr(b"view".to_vec())]);
    G_SVGATTR_ACCELERATE "$Svg$Attributes$accelerate" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"accelerate".to_vec())]);
    G_SVGATTR_ACCENTHEIGHT "$Svg$Attributes$accentHeight" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"accent-height".to_vec())]);
    G_SVGATTR_ACCUMULATE "$Svg$Attributes$accumulate" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"accumulate".to_vec())]);
    G_SVGATTR_ADDITIVE "$Svg$Attributes$additive" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"additive".to_vec())]);
    G_SVGATTR_ALIGNMENTBASELINE "$Svg$Attributes$alignmentBaseline" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"alignment-baseline".to_vec())]);
    G_SVGATTR_ALLOWREORDER "$Svg$Attributes$allowReorder" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"allowReorder".to_vec())]);
    G_SVGATTR_ALPHABETIC "$Svg$Attributes$alphabetic" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"alphabetic".to_vec())]);
    G_SVGATTR_AMPLITUDE "$Svg$Attributes$amplitude" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"amplitude".to_vec())]);
    G_SVGATTR_ARABICFORM "$Svg$Attributes$arabicForm" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"arabic-form".to_vec())]);
    G_SVGATTR_ASCENT "$Svg$Attributes$ascent" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"ascent".to_vec())]);
    G_SVGATTR_ATTRIBUTENAME "$Svg$Attributes$attributeName" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"attributeName".to_vec())]);
    G_SVGATTR_ATTRIBUTETYPE "$Svg$Attributes$attributeType" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"attributeType".to_vec())]);
    G_SVGATTR_AUTOREVERSE "$Svg$Attributes$autoReverse" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"autoReverse".to_vec())]);
    G_SVGATTR_AZIMUTH "$Svg$Attributes$azimuth" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"azimuth".to_vec())]);
    G_SVGATTR_BASEFREQUENCY "$Svg$Attributes$baseFrequency" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"baseFrequency".to_vec())]);
    G_SVGATTR_BASEPROFILE "$Svg$Attributes$baseProfile" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"baseProfile".to_vec())]);
    G_SVGATTR_BASELINESHIFT "$Svg$Attributes$baselineShift" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"baseline-shift".to_vec())]);
    G_SVGATTR_BBOX "$Svg$Attributes$bbox" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"bbox".to_vec())]);
    G_SVGATTR_BEGIN "$Svg$Attributes$begin" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"begin".to_vec())]);
    G_SVGATTR_BIAS "$Svg$Attributes$bias" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"bias".to_vec())]);
    G_SVGATTR_BY "$Svg$Attributes$by" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"by".to_vec())]);
    G_SVGATTR_CALCMODE "$Svg$Attributes$calcMode" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"calcMode".to_vec())]);
    G_SVGATTR_CAPHEIGHT "$Svg$Attributes$capHeight" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"cap-height".to_vec())]);
    G_SVGATTR_CLASS "$Svg$Attributes$class" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"class".to_vec())]);
    G_SVGATTR_CLIP "$Svg$Attributes$clip" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"clip".to_vec())]);
    G_SVGATTR_CLIPPATH "$Svg$Attributes$clipPath" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"clip-path".to_vec())]);
    G_SVGATTR_CLIPPATHUNITS "$Svg$Attributes$clipPathUnits" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"clipPathUnits".to_vec())]);
    G_SVGATTR_CLIPRULE "$Svg$Attributes$clipRule" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"clip-rule".to_vec())]);
    G_SVGATTR_COLOR "$Svg$Attributes$color" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"color".to_vec())]);
    G_SVGATTR_COLORINTERPOLATION "$Svg$Attributes$colorInterpolation" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"color-interpolation".to_vec())]);
    G_SVGATTR_COLORINTERPOLATIONFILTERS "$Svg$Attributes$colorInterpolationFilters" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"color-interpolation-filters".to_vec())]);
    G_SVGATTR_COLORPROFILE "$Svg$Attributes$colorProfile" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"color-profile".to_vec())]);
    G_SVGATTR_COLORRENDERING "$Svg$Attributes$colorRendering" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"color-rendering".to_vec())]);
    G_SVGATTR_CONTENTSCRIPTTYPE "$Svg$Attributes$contentScriptType" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"contentScriptType".to_vec())]);
    G_SVGATTR_CONTENTSTYLETYPE "$Svg$Attributes$contentStyleType" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"contentStyleType".to_vec())]);
    G_SVGATTR_CURSOR "$Svg$Attributes$cursor" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"cursor".to_vec())]);
    G_SVGATTR_CX "$Svg$Attributes$cx" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"cx".to_vec())]);
    G_SVGATTR_CY "$Svg$Attributes$cy" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"cy".to_vec())]);
    G_SVGATTR_D "$Svg$Attributes$d" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"d".to_vec())]);
    G_SVGATTR_DECELERATE "$Svg$Attributes$decelerate" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"decelerate".to_vec())]);
    G_SVGATTR_DESCENT "$Svg$Attributes$descent" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"descent".to_vec())]);
    G_SVGATTR_DIFFUSECONSTANT "$Svg$Attributes$diffuseConstant" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"diffuseConstant".to_vec())]);
    G_SVGATTR_DIRECTION "$Svg$Attributes$direction" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"direction".to_vec())]);
    G_SVGATTR_DISPLAY "$Svg$Attributes$display" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"display".to_vec())]);
    G_SVGATTR_DIVISOR "$Svg$Attributes$divisor" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"divisor".to_vec())]);
    G_SVGATTR_DOMINANTBASELINE "$Svg$Attributes$dominantBaseline" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"dominant-baseline".to_vec())]);
    G_SVGATTR_DUR "$Svg$Attributes$dur" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"dur".to_vec())]);
    G_SVGATTR_DX "$Svg$Attributes$dx" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"dx".to_vec())]);
    G_SVGATTR_DY "$Svg$Attributes$dy" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"dy".to_vec())]);
    G_SVGATTR_EDGEMODE "$Svg$Attributes$edgeMode" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"edgeMode".to_vec())]);
    G_SVGATTR_ELEVATION "$Svg$Attributes$elevation" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"elevation".to_vec())]);
    G_SVGATTR_ENABLEBACKGROUND "$Svg$Attributes$enableBackground" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"enable-background".to_vec())]);
    G_SVGATTR_END "$Svg$Attributes$end" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"end".to_vec())]);
    G_SVGATTR_EXPONENT "$Svg$Attributes$exponent" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"exponent".to_vec())]);
    G_SVGATTR_EXTERNALRESOURCESREQUIRED "$Svg$Attributes$externalResourcesRequired" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"externalResourcesRequired".to_vec())]);
    G_SVGATTR_FILL "$Svg$Attributes$fill" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"fill".to_vec())]);
    G_SVGATTR_FILLOPACITY "$Svg$Attributes$fillOpacity" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"fill-opacity".to_vec())]);
    G_SVGATTR_FILLRULE "$Svg$Attributes$fillRule" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"fill-rule".to_vec())]);
    G_SVGATTR_FILTER "$Svg$Attributes$filter" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"filter".to_vec())]);
    G_SVGATTR_FILTERRES "$Svg$Attributes$filterRes" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"filterRes".to_vec())]);
    G_SVGATTR_FILTERUNITS "$Svg$Attributes$filterUnits" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"filterUnits".to_vec())]);
    G_SVGATTR_FLOODCOLOR "$Svg$Attributes$floodColor" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"flood-color".to_vec())]);
    G_SVGATTR_FLOODOPACITY "$Svg$Attributes$floodOpacity" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"flood-opacity".to_vec())]);
    G_SVGATTR_FONTFAMILY "$Svg$Attributes$fontFamily" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"font-family".to_vec())]);
    G_SVGATTR_FONTSIZE "$Svg$Attributes$fontSize" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"font-size".to_vec())]);
    G_SVGATTR_FONTSIZEADJUST "$Svg$Attributes$fontSizeAdjust" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"font-size-adjust".to_vec())]);
    G_SVGATTR_FONTSTRETCH "$Svg$Attributes$fontStretch" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"font-stretch".to_vec())]);
    G_SVGATTR_FONTSTYLE "$Svg$Attributes$fontStyle" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"font-style".to_vec())]);
    G_SVGATTR_FONTVARIANT "$Svg$Attributes$fontVariant" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"font-variant".to_vec())]);
    G_SVGATTR_FONTWEIGHT "$Svg$Attributes$fontWeight" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"font-weight".to_vec())]);
    G_SVGATTR_FORMAT "$Svg$Attributes$format" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"format".to_vec())]);
    G_SVGATTR_FROM "$Svg$Attributes$from" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"from".to_vec())]);
    G_SVGATTR_FX "$Svg$Attributes$fx" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"fx".to_vec())]);
    G_SVGATTR_FY "$Svg$Attributes$fy" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"fy".to_vec())]);
    G_SVGATTR_G1 "$Svg$Attributes$g1" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"g1".to_vec())]);
    G_SVGATTR_G2 "$Svg$Attributes$g2" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"g2".to_vec())]);
    G_SVGATTR_GLYPHNAME "$Svg$Attributes$glyphName" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"glyph-name".to_vec())]);
    G_SVGATTR_GLYPHORIENTATIONHORIZONTAL "$Svg$Attributes$glyphOrientationHorizontal" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"glyph-orientation-horizontal".to_vec())]);
    G_SVGATTR_GLYPHORIENTATIONVERTICAL "$Svg$Attributes$glyphOrientationVertical" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"glyph-orientation-vertical".to_vec())]);
    G_SVGATTR_GLYPHREF "$Svg$Attributes$glyphRef" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"glyphRef".to_vec())]);
    G_SVGATTR_GRADIENTTRANSFORM "$Svg$Attributes$gradientTransform" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"gradientTransform".to_vec())]);
    G_SVGATTR_GRADIENTUNITS "$Svg$Attributes$gradientUnits" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"gradientUnits".to_vec())]);
    G_SVGATTR_HANGING "$Svg$Attributes$hanging" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"hanging".to_vec())]);
    G_SVGATTR_HEIGHT "$Svg$Attributes$height" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"height".to_vec())]);
    G_SVGATTR_HORIZADVX "$Svg$Attributes$horizAdvX" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"horiz-adv-x".to_vec())]);
    G_SVGATTR_HORIZORIGINX "$Svg$Attributes$horizOriginX" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"horiz-origin-x".to_vec())]);
    G_SVGATTR_HORIZORIGINY "$Svg$Attributes$horizOriginY" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"horiz-origin-y".to_vec())]);
    G_SVGATTR_ID "$Svg$Attributes$id" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"id".to_vec())]);
    G_SVGATTR_IDEOGRAPHIC "$Svg$Attributes$ideographic" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"ideographic".to_vec())]);
    G_SVGATTR_IMAGERENDERING "$Svg$Attributes$imageRendering" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"image-rendering".to_vec())]);
    G_SVGATTR_IN2 "$Svg$Attributes$in2" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"in2".to_vec())]);
    G_SVGATTR_IN_ "$Svg$Attributes$in_" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"in".to_vec())]);
    G_SVGATTR_INTERCEPT "$Svg$Attributes$intercept" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"intercept".to_vec())]);
    G_SVGATTR_K "$Svg$Attributes$k" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"k".to_vec())]);
    G_SVGATTR_K1 "$Svg$Attributes$k1" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"k1".to_vec())]);
    G_SVGATTR_K2 "$Svg$Attributes$k2" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"k2".to_vec())]);
    G_SVGATTR_K3 "$Svg$Attributes$k3" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"k3".to_vec())]);
    G_SVGATTR_K4 "$Svg$Attributes$k4" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"k4".to_vec())]);
    G_SVGATTR_KERNELMATRIX "$Svg$Attributes$kernelMatrix" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"kernelMatrix".to_vec())]);
    G_SVGATTR_KERNELUNITLENGTH "$Svg$Attributes$kernelUnitLength" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"kernelUnitLength".to_vec())]);
    G_SVGATTR_KERNING "$Svg$Attributes$kerning" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"kerning".to_vec())]);
    G_SVGATTR_KEYPOINTS "$Svg$Attributes$keyPoints" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"keyPoints".to_vec())]);
    G_SVGATTR_KEYSPLINES "$Svg$Attributes$keySplines" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"keySplines".to_vec())]);
    G_SVGATTR_KEYTIMES "$Svg$Attributes$keyTimes" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"keyTimes".to_vec())]);
    G_SVGATTR_LANG "$Svg$Attributes$lang" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"lang".to_vec())]);
    G_SVGATTR_LENGTHADJUST "$Svg$Attributes$lengthAdjust" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"lengthAdjust".to_vec())]);
    G_SVGATTR_LETTERSPACING "$Svg$Attributes$letterSpacing" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"letter-spacing".to_vec())]);
    G_SVGATTR_LIGHTINGCOLOR "$Svg$Attributes$lightingColor" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"lighting-color".to_vec())]);
    G_SVGATTR_LIMITINGCONEANGLE "$Svg$Attributes$limitingConeAngle" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"limitingConeAngle".to_vec())]);
    G_SVGATTR_LOCAL "$Svg$Attributes$local" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"local".to_vec())]);
    G_SVGATTR_MARKEREND "$Svg$Attributes$markerEnd" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"marker-end".to_vec())]);
    G_SVGATTR_MARKERHEIGHT "$Svg$Attributes$markerHeight" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"markerHeight".to_vec())]);
    G_SVGATTR_MARKERMID "$Svg$Attributes$markerMid" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"marker-mid".to_vec())]);
    G_SVGATTR_MARKERSTART "$Svg$Attributes$markerStart" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"marker-start".to_vec())]);
    G_SVGATTR_MARKERUNITS "$Svg$Attributes$markerUnits" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"markerUnits".to_vec())]);
    G_SVGATTR_MARKERWIDTH "$Svg$Attributes$markerWidth" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"markerWidth".to_vec())]);
    G_SVGATTR_MASK "$Svg$Attributes$mask" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"mask".to_vec())]);
    G_SVGATTR_MASKCONTENTUNITS "$Svg$Attributes$maskContentUnits" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"maskContentUnits".to_vec())]);
    G_SVGATTR_MASKUNITS "$Svg$Attributes$maskUnits" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"maskUnits".to_vec())]);
    G_SVGATTR_MATHEMATICAL "$Svg$Attributes$mathematical" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"mathematical".to_vec())]);
    G_SVGATTR_MAX "$Svg$Attributes$max" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"max".to_vec())]);
    G_SVGATTR_MEDIA "$Svg$Attributes$media" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"media".to_vec())]);
    G_SVGATTR_METHOD "$Svg$Attributes$method" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"method".to_vec())]);
    G_SVGATTR_MIN "$Svg$Attributes$min" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"min".to_vec())]);
    G_SVGATTR_MODE "$Svg$Attributes$mode" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"mode".to_vec())]);
    G_SVGATTR_NAME "$Svg$Attributes$name" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"name".to_vec())]);
    G_SVGATTR_NUMOCTAVES "$Svg$Attributes$numOctaves" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"numOctaves".to_vec())]);
    G_SVGATTR_OFFSET "$Svg$Attributes$offset" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"offset".to_vec())]);
    G_SVGATTR_OPACITY "$Svg$Attributes$opacity" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"opacity".to_vec())]);
    G_SVGATTR_OPERATOR "$Svg$Attributes$operator" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"operator".to_vec())]);
    G_SVGATTR_ORDER "$Svg$Attributes$order" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"order".to_vec())]);
    G_SVGATTR_ORIENT "$Svg$Attributes$orient" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"orient".to_vec())]);
    G_SVGATTR_ORIENTATION "$Svg$Attributes$orientation" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"orientation".to_vec())]);
    G_SVGATTR_ORIGIN "$Svg$Attributes$origin" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"origin".to_vec())]);
    G_SVGATTR_OVERFLOW "$Svg$Attributes$overflow" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"overflow".to_vec())]);
    G_SVGATTR_OVERLINEPOSITION "$Svg$Attributes$overlinePosition" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"overline-position".to_vec())]);
    G_SVGATTR_OVERLINETHICKNESS "$Svg$Attributes$overlineThickness" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"overline-thickness".to_vec())]);
    G_SVGATTR_PANOSE1 "$Svg$Attributes$panose1" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"panose-1".to_vec())]);
    G_SVGATTR_PATH "$Svg$Attributes$path" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"path".to_vec())]);
    G_SVGATTR_PATHLENGTH "$Svg$Attributes$pathLength" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"pathLength".to_vec())]);
    G_SVGATTR_PATTERNCONTENTUNITS "$Svg$Attributes$patternContentUnits" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"patternContentUnits".to_vec())]);
    G_SVGATTR_PATTERNTRANSFORM "$Svg$Attributes$patternTransform" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"patternTransform".to_vec())]);
    G_SVGATTR_PATTERNUNITS "$Svg$Attributes$patternUnits" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"patternUnits".to_vec())]);
    G_SVGATTR_POINTORDER "$Svg$Attributes$pointOrder" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"point-order".to_vec())]);
    G_SVGATTR_POINTEREVENTS "$Svg$Attributes$pointerEvents" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"pointer-events".to_vec())]);
    G_SVGATTR_POINTS "$Svg$Attributes$points" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"points".to_vec())]);
    G_SVGATTR_POINTSATX "$Svg$Attributes$pointsAtX" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"pointsAtX".to_vec())]);
    G_SVGATTR_POINTSATY "$Svg$Attributes$pointsAtY" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"pointsAtY".to_vec())]);
    G_SVGATTR_POINTSATZ "$Svg$Attributes$pointsAtZ" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"pointsAtZ".to_vec())]);
    G_SVGATTR_PRESERVEALPHA "$Svg$Attributes$preserveAlpha" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"preserveAlpha".to_vec())]);
    G_SVGATTR_PRESERVEASPECTRATIO "$Svg$Attributes$preserveAspectRatio" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"preserveAspectRatio".to_vec())]);
    G_SVGATTR_PRIMITIVEUNITS "$Svg$Attributes$primitiveUnits" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"primitiveUnits".to_vec())]);
    G_SVGATTR_R "$Svg$Attributes$r" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"r".to_vec())]);
    G_SVGATTR_RADIUS "$Svg$Attributes$radius" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"radius".to_vec())]);
    G_SVGATTR_REFX "$Svg$Attributes$refX" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"refX".to_vec())]);
    G_SVGATTR_REFY "$Svg$Attributes$refY" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"refY".to_vec())]);
    G_SVGATTR_RENDERINGINTENT "$Svg$Attributes$renderingIntent" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"rendering-intent".to_vec())]);
    G_SVGATTR_REPEATCOUNT "$Svg$Attributes$repeatCount" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"repeatCount".to_vec())]);
    G_SVGATTR_REPEATDUR "$Svg$Attributes$repeatDur" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"repeatDur".to_vec())]);
    G_SVGATTR_REQUIREDEXTENSIONS "$Svg$Attributes$requiredExtensions" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"requiredExtensions".to_vec())]);
    G_SVGATTR_REQUIREDFEATURES "$Svg$Attributes$requiredFeatures" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"requiredFeatures".to_vec())]);
    G_SVGATTR_RESTART "$Svg$Attributes$restart" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"restart".to_vec())]);
    G_SVGATTR_RESULT "$Svg$Attributes$result" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"result".to_vec())]);
    G_SVGATTR_ROTATE "$Svg$Attributes$rotate" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"rotate".to_vec())]);
    G_SVGATTR_RX "$Svg$Attributes$rx" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"rx".to_vec())]);
    G_SVGATTR_RY "$Svg$Attributes$ry" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"ry".to_vec())]);
    G_SVGATTR_SCALE "$Svg$Attributes$scale" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"scale".to_vec())]);
    G_SVGATTR_SEED "$Svg$Attributes$seed" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"seed".to_vec())]);
    G_SVGATTR_SHAPERENDERING "$Svg$Attributes$shapeRendering" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"shape-rendering".to_vec())]);
    G_SVGATTR_SLOPE "$Svg$Attributes$slope" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"slope".to_vec())]);
    G_SVGATTR_SPACING "$Svg$Attributes$spacing" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"spacing".to_vec())]);
    G_SVGATTR_SPECULARCONSTANT "$Svg$Attributes$specularConstant" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"specularConstant".to_vec())]);
    G_SVGATTR_SPECULAREXPONENT "$Svg$Attributes$specularExponent" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"specularExponent".to_vec())]);
    G_SVGATTR_SPEED "$Svg$Attributes$speed" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"speed".to_vec())]);
    G_SVGATTR_SPREADMETHOD "$Svg$Attributes$spreadMethod" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"spreadMethod".to_vec())]);
    G_SVGATTR_STARTOFFSET "$Svg$Attributes$startOffset" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"startOffset".to_vec())]);
    G_SVGATTR_STDDEVIATION "$Svg$Attributes$stdDeviation" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"stdDeviation".to_vec())]);
    G_SVGATTR_STEMH "$Svg$Attributes$stemh" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"stemh".to_vec())]);
    G_SVGATTR_STEMV "$Svg$Attributes$stemv" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"stemv".to_vec())]);
    G_SVGATTR_STITCHTILES "$Svg$Attributes$stitchTiles" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"stitchTiles".to_vec())]);
    G_SVGATTR_STOPCOLOR "$Svg$Attributes$stopColor" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"stop-color".to_vec())]);
    G_SVGATTR_STOPOPACITY "$Svg$Attributes$stopOpacity" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"stop-opacity".to_vec())]);
    G_SVGATTR_STRIKETHROUGHPOSITION "$Svg$Attributes$strikethroughPosition" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"strikethrough-position".to_vec())]);
    G_SVGATTR_STRIKETHROUGHTHICKNESS "$Svg$Attributes$strikethroughThickness" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"strikethrough-thickness".to_vec())]);
    G_SVGATTR_STRING "$Svg$Attributes$string" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"string".to_vec())]);
    G_SVGATTR_STROKE "$Svg$Attributes$stroke" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"stroke".to_vec())]);
    G_SVGATTR_STROKEDASHARRAY "$Svg$Attributes$strokeDasharray" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"stroke-dasharray".to_vec())]);
    G_SVGATTR_STROKEDASHOFFSET "$Svg$Attributes$strokeDashoffset" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"stroke-dashoffset".to_vec())]);
    G_SVGATTR_STROKELINECAP "$Svg$Attributes$strokeLinecap" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"stroke-linecap".to_vec())]);
    G_SVGATTR_STROKELINEJOIN "$Svg$Attributes$strokeLinejoin" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"stroke-linejoin".to_vec())]);
    G_SVGATTR_STROKEMITERLIMIT "$Svg$Attributes$strokeMiterlimit" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"stroke-miterlimit".to_vec())]);
    G_SVGATTR_STROKEOPACITY "$Svg$Attributes$strokeOpacity" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"stroke-opacity".to_vec())]);
    G_SVGATTR_STROKEWIDTH "$Svg$Attributes$strokeWidth" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"stroke-width".to_vec())]);
    G_SVGATTR_STYLE "$Svg$Attributes$style" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"style".to_vec())]);
    G_SVGATTR_SURFACESCALE "$Svg$Attributes$surfaceScale" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"surfaceScale".to_vec())]);
    G_SVGATTR_SYSTEMLANGUAGE "$Svg$Attributes$systemLanguage" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"systemLanguage".to_vec())]);
    G_SVGATTR_TABLEVALUES "$Svg$Attributes$tableValues" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"tableValues".to_vec())]);
    G_SVGATTR_TARGET "$Svg$Attributes$target" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"target".to_vec())]);
    G_SVGATTR_TARGETX "$Svg$Attributes$targetX" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"targetX".to_vec())]);
    G_SVGATTR_TARGETY "$Svg$Attributes$targetY" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"targetY".to_vec())]);
    G_SVGATTR_TEXTANCHOR "$Svg$Attributes$textAnchor" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"text-anchor".to_vec())]);
    G_SVGATTR_TEXTDECORATION "$Svg$Attributes$textDecoration" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"text-decoration".to_vec())]);
    G_SVGATTR_TEXTLENGTH "$Svg$Attributes$textLength" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"textLength".to_vec())]);
    G_SVGATTR_TEXTRENDERING "$Svg$Attributes$textRendering" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"text-rendering".to_vec())]);
    G_SVGATTR_TITLE "$Svg$Attributes$title" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"title".to_vec())]);
    G_SVGATTR_TO "$Svg$Attributes$to" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"to".to_vec())]);
    G_SVGATTR_TRANSFORM "$Svg$Attributes$transform" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"transform".to_vec())]);
    G_SVGATTR_TYPE_ "$Svg$Attributes$type_" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"type".to_vec())]);
    G_SVGATTR_U1 "$Svg$Attributes$u1" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"u1".to_vec())]);
    G_SVGATTR_U2 "$Svg$Attributes$u2" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"u2".to_vec())]);
    G_SVGATTR_UNDERLINEPOSITION "$Svg$Attributes$underlinePosition" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"underline-position".to_vec())]);
    G_SVGATTR_UNDERLINETHICKNESS "$Svg$Attributes$underlineThickness" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"underline-thickness".to_vec())]);
    G_SVGATTR_UNICODE "$Svg$Attributes$unicode" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"unicode".to_vec())]);
    G_SVGATTR_UNICODEBIDI "$Svg$Attributes$unicodeBidi" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"unicode-bidi".to_vec())]);
    G_SVGATTR_UNICODERANGE "$Svg$Attributes$unicodeRange" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"unicode-range".to_vec())]);
    G_SVGATTR_UNITSPEREM "$Svg$Attributes$unitsPerEm" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"units-per-em".to_vec())]);
    G_SVGATTR_VALPHABETIC "$Svg$Attributes$vAlphabetic" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"v-alphabetic".to_vec())]);
    G_SVGATTR_VHANGING "$Svg$Attributes$vHanging" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"v-hanging".to_vec())]);
    G_SVGATTR_VIDEOGRAPHIC "$Svg$Attributes$vIdeographic" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"v-ideographic".to_vec())]);
    G_SVGATTR_VMATHEMATICAL "$Svg$Attributes$vMathematical" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"v-mathematical".to_vec())]);
    G_SVGATTR_VALUES "$Svg$Attributes$values" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"values".to_vec())]);
    G_SVGATTR_VERSION "$Svg$Attributes$version" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"version".to_vec())]);
    G_SVGATTR_VERTADVY "$Svg$Attributes$vertAdvY" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"vert-adv-y".to_vec())]);
    G_SVGATTR_VERTORIGINX "$Svg$Attributes$vertOriginX" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"vert-origin-x".to_vec())]);
    G_SVGATTR_VERTORIGINY "$Svg$Attributes$vertOriginY" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"vert-origin-y".to_vec())]);
    G_SVGATTR_VIEWBOX "$Svg$Attributes$viewBox" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"viewBox".to_vec())]);
    G_SVGATTR_VIEWTARGET "$Svg$Attributes$viewTarget" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"viewTarget".to_vec())]);
    G_SVGATTR_VISIBILITY "$Svg$Attributes$visibility" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"visibility".to_vec())]);
    G_SVGATTR_WIDTH "$Svg$Attributes$width" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"width".to_vec())]);
    G_SVGATTR_WIDTHS "$Svg$Attributes$widths" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"widths".to_vec())]);
    G_SVGATTR_WORDSPACING "$Svg$Attributes$wordSpacing" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"word-spacing".to_vec())]);
    G_SVGATTR_WRITINGMODE "$Svg$Attributes$writingMode" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"writing-mode".to_vec())]);
    G_SVGATTR_X "$Svg$Attributes$x" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"x".to_vec())]);
    G_SVGATTR_X1 "$Svg$Attributes$x1" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"x1".to_vec())]);
    G_SVGATTR_X2 "$Svg$Attributes$x2" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"x2".to_vec())]);
    G_SVGATTR_XCHANNELSELECTOR "$Svg$Attributes$xChannelSelector" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"xChannelSelector".to_vec())]);
    G_SVGATTR_XHEIGHT "$Svg$Attributes$xHeight" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"x-height".to_vec())]);
    G_SVGATTR_XLINKACTUATE "$Svg$Attributes$xlinkActuate" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"xlink:actuate".to_vec())]);
    G_SVGATTR_XLINKARCROLE "$Svg$Attributes$xlinkArcrole" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"xlink:arcrole".to_vec())]);
    G_SVGATTR_XLINKHREF "$Svg$Attributes$xlinkHref" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"xlink:href".to_vec())]);
    G_SVGATTR_XLINKROLE "$Svg$Attributes$xlinkRole" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"xlink:role".to_vec())]);
    G_SVGATTR_XLINKSHOW "$Svg$Attributes$xlinkShow" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"xlink:show".to_vec())]);
    G_SVGATTR_XLINKTITLE "$Svg$Attributes$xlinkTitle" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"xlink:title".to_vec())]);
    G_SVGATTR_XLINKTYPE "$Svg$Attributes$xlinkType" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"xlink:type".to_vec())]);
    G_SVGATTR_XMLBASE "$Svg$Attributes$xmlBase" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"xml:base".to_vec())]);
    G_SVGATTR_XMLLANG "$Svg$Attributes$xmlLang" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"xml:lang".to_vec())]);
    G_SVGATTR_XMLSPACE "$Svg$Attributes$xmlSpace" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"xml:space".to_vec())]);
    G_SVGATTR_Y "$Svg$Attributes$y" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"y".to_vec())]);
    G_SVGATTR_Y1 "$Svg$Attributes$y1" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"y1".to_vec())]);
    G_SVGATTR_Y2 "$Svg$Attributes$y2" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"y2".to_vec())]);
    G_SVGATTR_YCHANNELSELECTOR "$Svg$Attributes$yChannelSelector" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"yChannelSelector".to_vec())]);
    G_SVGATTR_Z "$Svg$Attributes$z" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"z".to_vec())]);
    G_SVGATTR_ZOOMANDPAN "$Svg$Attributes$zoomAndPan" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"zoomAndPan".to_vec())]);
    G_HTMLTAG_DIV "$Html$div" = closure(vdom_node3 as *const (), 3, &[mkstr(b"div".to_vec())]);
    G_HTMLTAG_SPAN "$Html$span" = closure(vdom_node3 as *const (), 3, &[mkstr(b"span".to_vec())]);
    G_HTMLTAG_P "$Html$p" = closure(vdom_node3 as *const (), 3, &[mkstr(b"p".to_vec())]);
    G_HTMLTAG_A "$Html$a" = closure(vdom_node3 as *const (), 3, &[mkstr(b"a".to_vec())]);
    G_HTMLTAG_IMG "$Html$img" = closure(vdom_node3 as *const (), 3, &[mkstr(b"img".to_vec())]);
    G_HTMLTAG_BR "$Html$br" = closure(vdom_node3 as *const (), 3, &[mkstr(b"br".to_vec())]);
    G_HTMLTAG_HR "$Html$hr" = closure(vdom_node3 as *const (), 3, &[mkstr(b"hr".to_vec())]);
    G_HTMLTAG_PRE "$Html$pre" = closure(vdom_node3 as *const (), 3, &[mkstr(b"pre".to_vec())]);
    G_HTMLTAG_CODE "$Html$code" = closure(vdom_node3 as *const (), 3, &[mkstr(b"code".to_vec())]);
    G_HTMLTAG_EM "$Html$em" = closure(vdom_node3 as *const (), 3, &[mkstr(b"em".to_vec())]);
    G_HTMLTAG_STRONG "$Html$strong" = closure(vdom_node3 as *const (), 3, &[mkstr(b"strong".to_vec())]);
    G_HTMLTAG_I "$Html$i" = closure(vdom_node3 as *const (), 3, &[mkstr(b"i".to_vec())]);
    G_HTMLTAG_B "$Html$b" = closure(vdom_node3 as *const (), 3, &[mkstr(b"b".to_vec())]);
    G_HTMLTAG_U "$Html$u" = closure(vdom_node3 as *const (), 3, &[mkstr(b"u".to_vec())]);
    G_HTMLTAG_SUB "$Html$sub" = closure(vdom_node3 as *const (), 3, &[mkstr(b"sub".to_vec())]);
    G_HTMLTAG_SUP "$Html$sup" = closure(vdom_node3 as *const (), 3, &[mkstr(b"sup".to_vec())]);
    G_HTMLTAG_H1 "$Html$h1" = closure(vdom_node3 as *const (), 3, &[mkstr(b"h1".to_vec())]);
    G_HTMLTAG_H2 "$Html$h2" = closure(vdom_node3 as *const (), 3, &[mkstr(b"h2".to_vec())]);
    G_HTMLTAG_H3 "$Html$h3" = closure(vdom_node3 as *const (), 3, &[mkstr(b"h3".to_vec())]);
    G_HTMLTAG_H4 "$Html$h4" = closure(vdom_node3 as *const (), 3, &[mkstr(b"h4".to_vec())]);
    G_HTMLTAG_H5 "$Html$h5" = closure(vdom_node3 as *const (), 3, &[mkstr(b"h5".to_vec())]);
    G_HTMLTAG_H6 "$Html$h6" = closure(vdom_node3 as *const (), 3, &[mkstr(b"h6".to_vec())]);
    G_HTMLTAG_UL "$Html$ul" = closure(vdom_node3 as *const (), 3, &[mkstr(b"ul".to_vec())]);
    G_HTMLTAG_OL "$Html$ol" = closure(vdom_node3 as *const (), 3, &[mkstr(b"ol".to_vec())]);
    G_HTMLTAG_LI "$Html$li" = closure(vdom_node3 as *const (), 3, &[mkstr(b"li".to_vec())]);
    G_HTMLTAG_DL "$Html$dl" = closure(vdom_node3 as *const (), 3, &[mkstr(b"dl".to_vec())]);
    G_HTMLTAG_DT "$Html$dt" = closure(vdom_node3 as *const (), 3, &[mkstr(b"dt".to_vec())]);
    G_HTMLTAG_DD "$Html$dd" = closure(vdom_node3 as *const (), 3, &[mkstr(b"dd".to_vec())]);
    G_HTMLTAG_TABLE "$Html$table" = closure(vdom_node3 as *const (), 3, &[mkstr(b"table".to_vec())]);
    G_HTMLTAG_CAPTION "$Html$caption" = closure(vdom_node3 as *const (), 3, &[mkstr(b"caption".to_vec())]);
    G_HTMLTAG_THEAD "$Html$thead" = closure(vdom_node3 as *const (), 3, &[mkstr(b"thead".to_vec())]);
    G_HTMLTAG_TBODY "$Html$tbody" = closure(vdom_node3 as *const (), 3, &[mkstr(b"tbody".to_vec())]);
    G_HTMLTAG_TFOOT "$Html$tfoot" = closure(vdom_node3 as *const (), 3, &[mkstr(b"tfoot".to_vec())]);
    G_HTMLTAG_TR "$Html$tr" = closure(vdom_node3 as *const (), 3, &[mkstr(b"tr".to_vec())]);
    G_HTMLTAG_TD "$Html$td" = closure(vdom_node3 as *const (), 3, &[mkstr(b"td".to_vec())]);
    G_HTMLTAG_TH "$Html$th" = closure(vdom_node3 as *const (), 3, &[mkstr(b"th".to_vec())]);
    G_HTMLTAG_FORM "$Html$form" = closure(vdom_node3 as *const (), 3, &[mkstr(b"form".to_vec())]);
    G_HTMLTAG_FIELDSET "$Html$fieldset" = closure(vdom_node3 as *const (), 3, &[mkstr(b"fieldset".to_vec())]);
    G_HTMLTAG_LEGEND "$Html$legend" = closure(vdom_node3 as *const (), 3, &[mkstr(b"legend".to_vec())]);
    G_HTMLTAG_LABEL "$Html$label" = closure(vdom_node3 as *const (), 3, &[mkstr(b"label".to_vec())]);
    G_HTMLTAG_INPUT "$Html$input" = closure(vdom_node3 as *const (), 3, &[mkstr(b"input".to_vec())]);
    G_HTMLTAG_TEXTAREA "$Html$textarea" = closure(vdom_node3 as *const (), 3, &[mkstr(b"textarea".to_vec())]);
    G_HTMLTAG_BUTTON "$Html$button" = closure(vdom_node3 as *const (), 3, &[mkstr(b"button".to_vec())]);
    G_HTMLTAG_SELECT "$Html$select" = closure(vdom_node3 as *const (), 3, &[mkstr(b"select".to_vec())]);
    G_HTMLTAG_OPTION "$Html$option" = closure(vdom_node3 as *const (), 3, &[mkstr(b"option".to_vec())]);
    G_HTMLTAG_SECTION "$Html$section" = closure(vdom_node3 as *const (), 3, &[mkstr(b"section".to_vec())]);
    G_HTMLTAG_HEADER "$Html$header" = closure(vdom_node3 as *const (), 3, &[mkstr(b"header".to_vec())]);
    G_HTMLTAG_FOOTER "$Html$footer" = closure(vdom_node3 as *const (), 3, &[mkstr(b"footer".to_vec())]);
    G_HTMLTAG_NAV "$Html$nav" = closure(vdom_node3 as *const (), 3, &[mkstr(b"nav".to_vec())]);
    G_HTMLTAG_ARTICLE "$Html$article" = closure(vdom_node3 as *const (), 3, &[mkstr(b"article".to_vec())]);
    G_HTMLTAG_ASIDE "$Html$aside" = closure(vdom_node3 as *const (), 3, &[mkstr(b"aside".to_vec())]);
    G_HTMLTAG_MAIN_ "$Html$main_" = closure(vdom_node3 as *const (), 3, &[mkstr(b"main".to_vec())]);
    G_HTMLTAG_FIGURE "$Html$figure" = closure(vdom_node3 as *const (), 3, &[mkstr(b"figure".to_vec())]);
    G_HTMLTAG_FIGCAPTION "$Html$figcaption" = closure(vdom_node3 as *const (), 3, &[mkstr(b"figcaption".to_vec())]);
    G_HTMLTAG_BLOCKQUOTE "$Html$blockquote" = closure(vdom_node3 as *const (), 3, &[mkstr(b"blockquote".to_vec())]);
    G_HTMLTAG_IFRAME "$Html$iframe" = closure(vdom_node3 as *const (), 3, &[mkstr(b"iframe".to_vec())]);
    G_HTMLTAG_CANVAS "$Html$canvas" = closure(vdom_node3 as *const (), 3, &[mkstr(b"canvas".to_vec())]);
    G_HTMLTAG_AUDIO "$Html$audio" = closure(vdom_node3 as *const (), 3, &[mkstr(b"audio".to_vec())]);
    G_HTMLTAG_VIDEO "$Html$video" = closure(vdom_node3 as *const (), 3, &[mkstr(b"video".to_vec())]);
    G_HTMLTAG_SOURCE "$Html$source" = closure(vdom_node3 as *const (), 3, &[mkstr(b"source".to_vec())]);
    G_HTMLTAG_SMALL "$Html$small" = closure(vdom_node3 as *const (), 3, &[mkstr(b"small".to_vec())]);
    G_HTMLTAG_CITE "$Html$cite" = closure(vdom_node3 as *const (), 3, &[mkstr(b"cite".to_vec())]);
    G_HTMLTAG_DETAILS "$Html$details" = closure(vdom_node3 as *const (), 3, &[mkstr(b"details".to_vec())]);
    G_HTMLTAG_SUMMARY "$Html$summary" = closure(vdom_node3 as *const (), 3, &[mkstr(b"summary".to_vec())]);
    G_HTMLTAG_ABBR "$Html$abbr" = closure(vdom_node3 as *const (), 3, &[mkstr(b"abbr".to_vec())]);
    G_HTMLTAG_ADDRESS "$Html$address" = closure(vdom_node3 as *const (), 3, &[mkstr(b"address".to_vec())]);
    G_HTMLTAG_MARK "$Html$mark" = closure(vdom_node3 as *const (), 3, &[mkstr(b"mark".to_vec())]);
    G_HTMLTAG_METER "$Html$meter" = closure(vdom_node3 as *const (), 3, &[mkstr(b"meter".to_vec())]);
    G_HTMLTAG_PROGRESS "$Html$progress" = closure(vdom_node3 as *const (), 3, &[mkstr(b"progress".to_vec())]);
    G_HTMLTAG_OUTPUT "$Html$output" = closure(vdom_node3 as *const (), 3, &[mkstr(b"output".to_vec())]);
    G_HTMLTAG_DATALIST "$Html$datalist" = closure(vdom_node3 as *const (), 3, &[mkstr(b"datalist".to_vec())]);
    G_HTMLTAG_OPTGROUP "$Html$optgroup" = closure(vdom_node3 as *const (), 3, &[mkstr(b"optgroup".to_vec())]);
    G_HTMLTAG_S "$Html$s" = closure(vdom_node3 as *const (), 3, &[mkstr(b"s".to_vec())]);
    G_HTMLTAG_Q "$Html$q" = closure(vdom_node3 as *const (), 3, &[mkstr(b"q".to_vec())]);
    G_HTMLTAG_DEL "$Html$del" = closure(vdom_node3 as *const (), 3, &[mkstr(b"del".to_vec())]);
    G_HTMLTAG_INS "$Html$ins" = closure(vdom_node3 as *const (), 3, &[mkstr(b"ins".to_vec())]);
    G_HTMLTAG_COL "$Html$col" = closure(vdom_node3 as *const (), 3, &[mkstr(b"col".to_vec())]);
    G_HTMLTAG_COLGROUP "$Html$colgroup" = closure(vdom_node3 as *const (), 3, &[mkstr(b"colgroup".to_vec())]);
    G_HTMLTAG_TRACK "$Html$track" = closure(vdom_node3 as *const (), 3, &[mkstr(b"track".to_vec())]);
    G_HTMLTAG_EMBED "$Html$embed" = closure(vdom_node3 as *const (), 3, &[mkstr(b"embed".to_vec())]);
    G_HTMLTAG_OBJECT "$Html$object" = closure(vdom_node3 as *const (), 3, &[mkstr(b"object".to_vec())]);
    G_HTMLTAG_PARAM "$Html$param" = closure(vdom_node3 as *const (), 3, &[mkstr(b"param".to_vec())]);
    G_HTMLTAG_MATH "$Html$math" = closure(vdom_node3 as *const (), 3, &[mkstr(b"math".to_vec())]);
    G_HTMLTAG_DFN "$Html$dfn" = closure(vdom_node3 as *const (), 3, &[mkstr(b"dfn".to_vec())]);
    G_HTMLTAG_TIME "$Html$time" = closure(vdom_node3 as *const (), 3, &[mkstr(b"time".to_vec())]);
    G_HTMLTAG_VAR "$Html$var" = closure(vdom_node3 as *const (), 3, &[mkstr(b"var".to_vec())]);
    G_HTMLTAG_SAMP "$Html$samp" = closure(vdom_node3 as *const (), 3, &[mkstr(b"samp".to_vec())]);
    G_HTMLTAG_KBD "$Html$kbd" = closure(vdom_node3 as *const (), 3, &[mkstr(b"kbd".to_vec())]);
    G_HTMLTAG_RUBY "$Html$ruby" = closure(vdom_node3 as *const (), 3, &[mkstr(b"ruby".to_vec())]);
    G_HTMLTAG_RT "$Html$rt" = closure(vdom_node3 as *const (), 3, &[mkstr(b"rt".to_vec())]);
    G_HTMLTAG_RP "$Html$rp" = closure(vdom_node3 as *const (), 3, &[mkstr(b"rp".to_vec())]);
    G_HTMLTAG_BDI "$Html$bdi" = closure(vdom_node3 as *const (), 3, &[mkstr(b"bdi".to_vec())]);
    G_HTMLTAG_BDO "$Html$bdo" = closure(vdom_node3 as *const (), 3, &[mkstr(b"bdo".to_vec())]);
    G_HTMLTAG_WBR "$Html$wbr" = closure(vdom_node3 as *const (), 3, &[mkstr(b"wbr".to_vec())]);
    G_HTMLTAG_MENU "$Html$menu" = closure(vdom_node3 as *const (), 3, &[mkstr(b"menu".to_vec())]);
    G_HTMLTAG_MENUITEM "$Html$menuitem" = closure(vdom_node3 as *const (), 3, &[mkstr(b"menuitem".to_vec())]);
    G_HTMLATTR_CLASS "$Html$Attributes$class" = closure(attr_property2 as *const (), 2, &[mkstr(b"className".to_vec())]);
    G_HTMLATTR_ID "$Html$Attributes$id" = closure(attr_property2 as *const (), 2, &[mkstr(b"id".to_vec())]);
    G_HTMLATTR_TITLE "$Html$Attributes$title" = closure(attr_property2 as *const (), 2, &[mkstr(b"title".to_vec())]);
    G_HTMLATTR_HREF "$Html$Attributes$href" = closure(attr_property2 as *const (), 2, &[mkstr(b"href".to_vec())]);
    G_HTMLATTR_SRC "$Html$Attributes$src" = closure(attr_property2 as *const (), 2, &[mkstr(b"src".to_vec())]);
    G_HTMLATTR_ALT "$Html$Attributes$alt" = closure(attr_property2 as *const (), 2, &[mkstr(b"alt".to_vec())]);
    G_HTMLATTR_NAME "$Html$Attributes$name" = closure(attr_property2 as *const (), 2, &[mkstr(b"name".to_vec())]);
    G_HTMLATTR_PLACEHOLDER "$Html$Attributes$placeholder" = closure(attr_property2 as *const (), 2, &[mkstr(b"placeholder".to_vec())]);
    G_HTMLATTR_VALUE "$Html$Attributes$value" = closure(attr_property2 as *const (), 2, &[mkstr(b"value".to_vec())]);
    G_HTMLATTR_TYPE_ "$Html$Attributes$type_" = closure(attr_property2 as *const (), 2, &[mkstr(b"type".to_vec())]);
    G_HTMLATTR_DRAGGABLE "$Html$Attributes$draggable" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"draggable".to_vec())]);
    G_HTMLATTR_FOR "$Html$Attributes$for" = closure(attr_property2 as *const (), 2, &[mkstr(b"htmlFor".to_vec())]);
    G_HTMLATTR_ACTION "$Html$Attributes$action" = closure(attr_property2 as *const (), 2, &[mkstr(b"action".to_vec())]);
    G_HTMLATTR_METHOD "$Html$Attributes$method" = closure(attr_property2 as *const (), 2, &[mkstr(b"method".to_vec())]);
    G_HTMLATTR_TARGET "$Html$Attributes$target" = closure(attr_property2 as *const (), 2, &[mkstr(b"target".to_vec())]);
    G_HTMLATTR_REL "$Html$Attributes$rel" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"rel".to_vec())]);
    G_HTMLATTR_WRAP "$Html$Attributes$wrap" = closure(attr_property2 as *const (), 2, &[mkstr(b"wrap".to_vec())]);
    G_HTMLATTR_ACCEPT "$Html$Attributes$accept" = closure(attr_property2 as *const (), 2, &[mkstr(b"accept".to_vec())]);
    G_HTMLATTR_LIST "$Html$Attributes$list" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"list".to_vec())]);
    G_HTMLATTR_MAX "$Html$Attributes$max" = closure(attr_property2 as *const (), 2, &[mkstr(b"max".to_vec())]);
    G_HTMLATTR_MIN "$Html$Attributes$min" = closure(attr_property2 as *const (), 2, &[mkstr(b"min".to_vec())]);
    G_HTMLATTR_STEP "$Html$Attributes$step" = closure(attr_property2 as *const (), 2, &[mkstr(b"step".to_vec())]);
    G_HTMLATTR_PATTERN "$Html$Attributes$pattern" = closure(attr_property2 as *const (), 2, &[mkstr(b"pattern".to_vec())]);
    G_HTMLATTR_LANG "$Html$Attributes$lang" = closure(attr_property2 as *const (), 2, &[mkstr(b"lang".to_vec())]);
    G_HTMLATTR_DIR "$Html$Attributes$dir" = closure(attr_property2 as *const (), 2, &[mkstr(b"dir".to_vec())]);
    G_HTMLATTR_DOWNLOAD "$Html$Attributes$download" = closure(attr_property2 as *const (), 2, &[mkstr(b"download".to_vec())]);
    G_HTMLATTR_HREFLANG "$Html$Attributes$hreflang" = closure(attr_property2 as *const (), 2, &[mkstr(b"hreflang".to_vec())]);
    G_HTMLATTR_MEDIA "$Html$Attributes$media" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"media".to_vec())]);
    G_HTMLATTR_PING "$Html$Attributes$ping" = closure(attr_property2 as *const (), 2, &[mkstr(b"ping".to_vec())]);
    G_HTMLATTR_USEMAP "$Html$Attributes$usemap" = closure(attr_property2 as *const (), 2, &[mkstr(b"useMap".to_vec())]);
    G_HTMLATTR_SHAPE "$Html$Attributes$shape" = closure(attr_property2 as *const (), 2, &[mkstr(b"shape".to_vec())]);
    G_HTMLATTR_COORDS "$Html$Attributes$coords" = closure(attr_property2 as *const (), 2, &[mkstr(b"coords".to_vec())]);
    G_HTMLATTR_ENCTYPE "$Html$Attributes$enctype" = closure(attr_property2 as *const (), 2, &[mkstr(b"enctype".to_vec())]);
    G_HTMLATTR_DATETIME "$Html$Attributes$datetime" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"datetime".to_vec())]);
    G_HTMLATTR_CHARSET "$Html$Attributes$charset" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"charset".to_vec())]);
    G_HTMLATTR_CONTENT "$Html$Attributes$content" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"content".to_vec())]);
    G_HTMLATTR_HTTPEQUIV "$Html$Attributes$httpEquiv" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"http-equiv".to_vec())]);
    G_HTMLATTR_POSTER "$Html$Attributes$poster" = closure(attr_property2 as *const (), 2, &[mkstr(b"poster".to_vec())]);
    G_HTMLATTR_KIND "$Html$Attributes$kind" = closure(attr_property2 as *const (), 2, &[mkstr(b"kind".to_vec())]);
    G_HTMLATTR_SRCLANG "$Html$Attributes$srclang" = closure(attr_property2 as *const (), 2, &[mkstr(b"srclang".to_vec())]);
    G_HTMLATTR_SANDBOX "$Html$Attributes$sandbox" = closure(attr_property2 as *const (), 2, &[mkstr(b"sandbox".to_vec())]);
    G_HTMLATTR_SRCDOC "$Html$Attributes$srcdoc" = closure(attr_property2 as *const (), 2, &[mkstr(b"srcdoc".to_vec())]);
    G_HTMLATTR_MANIFEST "$Html$Attributes$manifest" = closure(attr_attribute2 as *const (), 2, &[mkstr(b"manifest".to_vec())]);
    G_HTMLATTR_HEADERS "$Html$Attributes$headers" = closure(attr_property2 as *const (), 2, &[mkstr(b"headers".to_vec())]);
    G_HTMLATTR_SCOPE "$Html$Attributes$scope" = closure(attr_property2 as *const (), 2, &[mkstr(b"scope".to_vec())]);
    G_HTMLATTR_ACCESSKEY "$Html$Attributes$accesskey" = closure(attr_property2 as *const (), 2, &[mkstr(b"accessKey".to_vec())]);
    G_HTMLATTR_CITE "$Html$Attributes$cite" = closure(attr_property2 as *const (), 2, &[mkstr(b"cite".to_vec())]);
    G_HTMLATTR_ALIGN "$Html$Attributes$align" = closure(attr_property2 as *const (), 2, &[mkstr(b"align".to_vec())]);
    G_HTMLATTR_ACCEPTCHARSET "$Html$Attributes$acceptCharset" = closure(attr_property2 as *const (), 2, &[mkstr(b"acceptCharset".to_vec())]);
    G_HTMLATTR_CHECKED "$Html$Attributes$checked" = closure(attr_property2 as *const (), 2, &[mkstr(b"checked".to_vec())]);
    G_HTMLATTR_SELECTED "$Html$Attributes$selected" = closure(attr_property2 as *const (), 2, &[mkstr(b"selected".to_vec())]);
    G_HTMLATTR_DISABLED "$Html$Attributes$disabled" = closure(attr_property2 as *const (), 2, &[mkstr(b"disabled".to_vec())]);
    G_HTMLATTR_HIDDEN "$Html$Attributes$hidden" = closure(attr_property2 as *const (), 2, &[mkstr(b"hidden".to_vec())]);
    G_HTMLATTR_READONLY "$Html$Attributes$readonly" = closure(attr_property2 as *const (), 2, &[mkstr(b"readOnly".to_vec())]);
    G_HTMLATTR_REQUIRED "$Html$Attributes$required" = closure(attr_property2 as *const (), 2, &[mkstr(b"required".to_vec())]);
    G_HTMLATTR_AUTOFOCUS "$Html$Attributes$autofocus" = closure(attr_property2 as *const (), 2, &[mkstr(b"autofocus".to_vec())]);
    G_HTMLATTR_CONTENTEDITABLE "$Html$Attributes$contenteditable" = closure(attr_property2 as *const (), 2, &[mkstr(b"contentEditable".to_vec())]);
    G_HTMLATTR_AUTOPLAY "$Html$Attributes$autoplay" = closure(attr_property2 as *const (), 2, &[mkstr(b"autoplay".to_vec())]);
    G_HTMLATTR_CONTROLS "$Html$Attributes$controls" = closure(attr_property2 as *const (), 2, &[mkstr(b"controls".to_vec())]);
    G_HTMLATTR_LOOP "$Html$Attributes$loop" = closure(attr_property2 as *const (), 2, &[mkstr(b"loop".to_vec())]);
    G_HTMLATTR_MULTIPLE "$Html$Attributes$multiple" = closure(attr_property2 as *const (), 2, &[mkstr(b"multiple".to_vec())]);
    G_HTMLATTR_NOVALIDATE "$Html$Attributes$novalidate" = closure(attr_property2 as *const (), 2, &[mkstr(b"noValidate".to_vec())]);
    G_HTMLATTR_SPELLCHECK "$Html$Attributes$spellcheck" = closure(attr_property2 as *const (), 2, &[mkstr(b"spellcheck".to_vec())]);
    G_HTMLATTR_AUTOCOMPLETE "$Html$Attributes$autocomplete" = closure(attr_autocomplete1 as *const (), 1, &[]);
    G_HTMLATTR_ISMAP "$Html$Attributes$ismap" = closure(attr_property2 as *const (), 2, &[mkstr(b"isMap".to_vec())]);
    G_HTMLATTR_DEFAULT "$Html$Attributes$default" = closure(attr_property2 as *const (), 2, &[mkstr(b"default".to_vec())]);
    G_HTMLATTR_ROWS "$Html$Attributes$rows" = closure(attr_int_str2 as *const (), 2, &[mkstr(b"rows".to_vec())]);
    G_HTMLATTR_COLS "$Html$Attributes$cols" = closure(attr_int_str2 as *const (), 2, &[mkstr(b"cols".to_vec())]);
    G_HTMLATTR_COLSPAN "$Html$Attributes$colspan" = closure(attr_int_str2 as *const (), 2, &[mkstr(b"colspan".to_vec())]);
    G_HTMLATTR_ROWSPAN "$Html$Attributes$rowspan" = closure(attr_int_str2 as *const (), 2, &[mkstr(b"rowspan".to_vec())]);
    G_HTMLATTR_TABINDEX "$Html$Attributes$tabindex" = closure(attr_int_str2 as *const (), 2, &[mkstr(b"tabIndex".to_vec())]);
    G_HTMLATTR_SIZE "$Html$Attributes$size" = closure(attr_int_str2 as *const (), 2, &[mkstr(b"size".to_vec())]);
    G_HTMLATTR_MAXLENGTH "$Html$Attributes$maxlength" = closure(attr_int_str2 as *const (), 2, &[mkstr(b"maxlength".to_vec())]);
    G_HTMLATTR_MINLENGTH "$Html$Attributes$minlength" = closure(attr_int_str2 as *const (), 2, &[mkstr(b"minLength".to_vec())]);
    G_HTMLATTR_HEIGHT "$Html$Attributes$height" = closure(attr_int_str2 as *const (), 2, &[mkstr(b"height".to_vec())]);
    G_HTMLATTR_WIDTH "$Html$Attributes$width" = closure(attr_int_str2 as *const (), 2, &[mkstr(b"width".to_vec())]);
    G_HTMLATTR_START "$Html$Attributes$start" = closure(attr_property2 as *const (), 2, &[mkstr(b"start".to_vec())]);

}

kernel_fns! {
    // elm/html + elm/virtual-dom core (non-baked; all arguments come from the
    // caller). Per-tag/attr helpers live in `baked_globals!` above.
    G_HTML_TEXT "$Html$text" vdom_text1, 1;
    G_VDOM_TEXT "$VirtualDom$text" vdom_text1, 1;
    G_SVG_TEXT "$Svg$text" vdom_text1, 1;
    G_HTML_NODE "$Html$node" vdom_node3, 3;
    G_VDOM_NODE "$VirtualDom$node" vdom_node3, 3;
    G_SVG_NODE "$Svg$node" vdom_node_ns3, 3;
    G_VDOM_NODENS "$VirtualDom$nodeNS" vdom_node_ns4, 4;
    G_HTML_MAP "$Html$map" vdom_map2, 2;
    G_VDOM_MAP "$VirtualDom$map" vdom_map2, 2;
    G_SVG_MAP "$Svg$map" vdom_map2, 2;
    G_HTMLKEYED_NODE "$Html$Keyed$node" vdom_keyed3, 3;
    G_VDOM_KEYEDNODE "$VirtualDom$keyedNode" vdom_keyed3, 3;
    G_VDOM_KEYEDNODENS "$VirtualDom$keyedNodeNS" vdom_keyed_ns4, 4;
    G_HTMLATTR_STYLE "$Html$Attributes$style" attr_style2, 2;
    G_VDOM_STYLE "$VirtualDom$style" attr_style2, 2;
    G_HTMLATTR_ATTRIBUTE "$Html$Attributes$attribute" attr_attribute2, 2;
    G_VDOM_ATTRIBUTE "$VirtualDom$attribute" attr_attribute2, 2;
    G_HTMLATTR_PROPERTY "$Html$Attributes$property" attr_property2, 2;
    G_VDOM_PROPERTY "$VirtualDom$property" attr_property2, 2;
    G_HTMLATTR_CLASSLIST "$Html$Attributes$classList" attr_class_list1, 1;
    G_HTMLATTR_MAP "$Html$Attributes$map" attr_map2, 2;
    G_VDOM_MAPATTRIBUTE "$VirtualDom$mapAttribute" attr_map2, 2;
    G_VDOM_ON "$VirtualDom$on" vdom_on2, 2;
    G_VDOM_LAZY "$VirtualDom$lazy" vdom_lazy2, 2;
    G_VDOM_LAZY2 "$VirtualDom$lazy2" vdom_lazy3, 3;
    G_VDOM_LAZY3 "$VirtualDom$lazy3" vdom_lazy4, 4;
    G_VDOM_LAZY4 "$VirtualDom$lazy4" vdom_lazy5, 5;
    G_VDOM_LAZY5 "$VirtualDom$lazy5" vdom_lazy6, 6;
    G_VDOM_LAZY6 "$VirtualDom$lazy6" vdom_lazy7, 7;
    G_VDOM_LAZY7 "$VirtualDom$lazy7" vdom_lazy8, 8;
    G_VDOM_LAZY8 "$VirtualDom$lazy8" vdom_lazy9, 9;
    G_HTMLLAZY_LAZY "$Html$Lazy$lazy" vdom_lazy2, 2;
    G_HTMLLAZY_LAZY2 "$Html$Lazy$lazy2" vdom_lazy3, 3;
    G_HTMLLAZY_LAZY3 "$Html$Lazy$lazy3" vdom_lazy4, 4;
    G_HTMLLAZY_LAZY4 "$Html$Lazy$lazy4" vdom_lazy5, 5;
    G_HTMLLAZY_LAZY5 "$Html$Lazy$lazy5" vdom_lazy6, 6;
    G_HTMLLAZY_LAZY6 "$Html$Lazy$lazy6" vdom_lazy7, 7;
    G_HTMLLAZY_LAZY7 "$Html$Lazy$lazy7" vdom_lazy8, 8;
    G_HTMLLAZY_LAZY8 "$Html$Lazy$lazy8" vdom_lazy9, 9;
    G_HTTP_HEADER "$Http$header" http_header2, 2;
    G_HTTP_STRINGBODY "$Http$stringBody" http_string_body2, 2;
    G_HTTP_JSONBODY "$Http$jsonBody" http_json_body1, 1;
    G_HTTP_EXPECTSTRING "$Http$expectString" http_expect_string1, 1;
    G_HTTP_EXPECTWHATEVER "$Http$expectWhatever" http_expect_whatever1, 1;
    G_HTTP_EXPECTJSON "$Http$expectJson" http_expect_json2, 2;
    G_HTTP_EXPECTSTRINGRESPONSE "$Http$expectStringResponse" http_expect_string_response2, 2;
    G_HTTP_EXPECTBYTESRESPONSE "$Http$expectBytesResponse" http_expect_bytes_response2, 2;
    G_HTTP_REQUEST "$Http$request" http_request1, 1;
    G_HTTP_RISKYREQUEST "$Http$riskyRequest" http_request1, 1;
    G_HTTP_GET "$Http$get" http_get1, 1;
    G_HTTP_POST "$Http$post" http_post1, 1;
    G_HTTP_STRINGRESOLVER "$Http$stringResolver" http_string_resolver1, 1;
    G_HTTP_BYTESRESOLVER "$Http$bytesResolver" http_string_resolver1, 1;
    G_HTTP_TASK "$Http$task" http_task1, 1;
    G_HTTP_RISKYTASK "$Http$riskyTask" http_task1, 1;
    G_DOM_GETELEMENT "$Browser$Dom$getElement" dom_get_element1, 1;
    G_DOM_SETVIEWPORTOF "$Browser$Dom$setViewportOf" dom_set_viewport_of3, 3;
    G_WEBGL_ENTITY "$Elm$Kernel$WebGL$entity" webgl_entity5, 5;
    G_WEBGL_TOHTML "$Elm$Kernel$WebGL$toHtml" webgl_to_html3, 3;
    G_WEBGL_ENABLEALPHA "$Elm$Kernel$WebGL$enableAlpha" webgl_enable2, 2;
    G_WEBGL_ENABLEANTIALIAS "$Elm$Kernel$WebGL$enableAntialias" webgl_enable2, 2;
    G_WEBGL_ENABLEBLEND "$Elm$Kernel$WebGL$enableBlend" webgl_enable2, 2;
    G_WEBGL_ENABLECLEARCOLOR "$Elm$Kernel$WebGL$enableClearColor" webgl_enable2, 2;
    G_WEBGL_ENABLECOLORMASK "$Elm$Kernel$WebGL$enableColorMask" webgl_enable2, 2;
    G_WEBGL_ENABLECULLFACE "$Elm$Kernel$WebGL$enableCullFace" webgl_enable2, 2;
    G_WEBGL_ENABLEDEPTH "$Elm$Kernel$WebGL$enableDepth" webgl_enable2, 2;
    G_WEBGL_ENABLEDEPTHTEST "$Elm$Kernel$WebGL$enableDepthTest" webgl_enable2, 2;
    G_WEBGL_ENABLEPOLYGONOFFSET "$Elm$Kernel$WebGL$enablePolygonOffset" webgl_enable2, 2;
    G_WEBGL_ENABLEPRESERVEDRAWINGBUFFER "$Elm$Kernel$WebGL$enablePreserveDrawingBuffer" webgl_enable2, 2;
    G_WEBGL_ENABLESAMPLEALPHATOCOVERAGE "$Elm$Kernel$WebGL$enableSampleAlphaToCoverage" webgl_enable2, 2;
    G_WEBGL_ENABLESAMPLECOVERAGE "$Elm$Kernel$WebGL$enableSampleCoverage" webgl_enable2, 2;
    G_WEBGL_ENABLESCISSOR "$Elm$Kernel$WebGL$enableScissor" webgl_enable2, 2;
    G_WEBGL_ENABLESTENCIL "$Elm$Kernel$WebGL$enableStencil" webgl_enable2, 2;
    G_WEBGL_ENABLESTENCILTEST "$Elm$Kernel$WebGL$enableStencilTest" webgl_enable2, 2;
    G_TEXTURE_LOAD "$Elm$Kernel$Texture$load" texture_load6, 6;
    G_TEXTURE_SIZE "$Elm$Kernel$Texture$size" texture_size1, 1;
    G_HTMLEV_ON "$Html$Events$on" events_on2, 2;
    G_HTMLEV_STOPON "$Html$Events$stopPropagationOn" events_stop_on2, 2;
    G_HTMLEV_PREVENTON "$Html$Events$preventDefaultOn" events_prevent_on2, 2;
    G_HTMLEV_CUSTOM "$Html$Events$custom" events_custom2, 2;
    G_HTMLEV_ONCLICK "$Html$Events$onClick" events_on_click1, 1;
    G_HTMLEV_ONDBLCLICK "$Html$Events$onDoubleClick" events_on_dblclick1, 1;
    G_HTMLEV_ONMOUSEDOWN "$Html$Events$onMouseDown" events_on_mousedown1, 1;
    G_HTMLEV_ONMOUSEUP "$Html$Events$onMouseUp" events_on_mouseup1, 1;
    G_HTMLEV_ONMOUSEENTER "$Html$Events$onMouseEnter" events_on_mouseenter1, 1;
    G_HTMLEV_ONMOUSELEAVE "$Html$Events$onMouseLeave" events_on_mouseleave1, 1;
    G_HTMLEV_ONMOUSEOVER "$Html$Events$onMouseOver" events_on_mouseover1, 1;
    G_HTMLEV_ONMOUSEOUT "$Html$Events$onMouseOut" events_on_mouseout1, 1;
    G_HTMLEV_ONBLUR "$Html$Events$onBlur" events_on_blur1, 1;
    G_HTMLEV_ONFOCUS "$Html$Events$onFocus" events_on_focus1, 1;
    G_HTMLEV_ONSUBMIT "$Html$Events$onSubmit" events_on_submit1, 1;
    G_HTMLEV_ONINPUT "$Html$Events$onInput" events_on_input1, 1;
    G_HTMLEV_ONCHECK "$Html$Events$onCheck" events_on_check1, 1;

    // Test.Html introspection: reflect the vdom to elm/virtual-dom JSON.
    G_HTMLASJSON_TOJSON "$Elm$Kernel$HtmlAsJson$toJson" html_to_json, 1;
    G_HTMLASJSON_ATTRTOJSON "$Elm$Kernel$HtmlAsJson$attributeToJson" html_attribute_to_json, 1;
    G_HTMLASJSON_EVENTHANDLER "$Elm$Kernel$HtmlAsJson$eventHandler" html_event_handler, 1;
    G_HTMLASJSON_TAGGERFN "$Elm$Kernel$HtmlAsJson$taggerFunction" html_tagger_function, 1;

    G_REGEX_FROMSTRINGWITH "$Elm$Kernel$Regex$fromStringWith" regex_from_string_with, 2;
    G_REGEX_CONTAINS "$Elm$Kernel$Regex$contains" regex_contains, 2;
    G_REGEX_FINDATMOST "$Elm$Kernel$Regex$findAtMost" regex_find_at_most, 3;
    G_REGEX_SPLITATMOST "$Elm$Kernel$Regex$splitAtMost" regex_split_at_most, 3;
    G_REGEX_REPLACEATMOST "$Elm$Kernel$Regex$replaceAtMost" regex_replace_at_most, 4;

    // elm-explorations/linear-algebra (Elm.Kernel.MJS). `m4x4identity` is a
    // value, not a function — it lives in `baked_globals!`.
    G_MJS_V2 "$Elm$Kernel$MJS$v2" mjs_v2_2, 2;
    G_MJS_V2GETX "$Elm$Kernel$MJS$v2getX" mjs_v2_get_x1, 1;
    G_MJS_V2GETY "$Elm$Kernel$MJS$v2getY" mjs_v2_get_y1, 1;
    G_MJS_V2SETX "$Elm$Kernel$MJS$v2setX" mjs_v2_set_x2, 2;
    G_MJS_V2SETY "$Elm$Kernel$MJS$v2setY" mjs_v2_set_y2, 2;
    G_MJS_V2TORECORD "$Elm$Kernel$MJS$v2toRecord" mjs_v2_to_record1, 1;
    G_MJS_V2FROMRECORD "$Elm$Kernel$MJS$v2fromRecord" mjs_v2_from_record1, 1;
    G_MJS_V2ADD "$Elm$Kernel$MJS$v2add" mjs_v2_add2, 2;
    G_MJS_V2SUB "$Elm$Kernel$MJS$v2sub" mjs_v2_sub2, 2;
    G_MJS_V2NEGATE "$Elm$Kernel$MJS$v2negate" mjs_v2_negate1, 1;
    G_MJS_V2DIRECTION "$Elm$Kernel$MJS$v2direction" mjs_v2_direction2, 2;
    G_MJS_V2LENGTH "$Elm$Kernel$MJS$v2length" mjs_v2_length1, 1;
    G_MJS_V2LENGTHSQUARED "$Elm$Kernel$MJS$v2lengthSquared" mjs_v2_length_squared1, 1;
    G_MJS_V2DISTANCE "$Elm$Kernel$MJS$v2distance" mjs_v2_distance2, 2;
    G_MJS_V2DISTANCESQUARED "$Elm$Kernel$MJS$v2distanceSquared" mjs_v2_distance_squared2, 2;
    G_MJS_V2NORMALIZE "$Elm$Kernel$MJS$v2normalize" mjs_v2_normalize1, 1;
    G_MJS_V2SCALE "$Elm$Kernel$MJS$v2scale" mjs_v2_scale2, 2;
    G_MJS_V2DOT "$Elm$Kernel$MJS$v2dot" mjs_v2_dot2, 2;
    G_MJS_V3 "$Elm$Kernel$MJS$v3" mjs_v3_3, 3;
    G_MJS_V3GETX "$Elm$Kernel$MJS$v3getX" mjs_v3_get_x1, 1;
    G_MJS_V3GETY "$Elm$Kernel$MJS$v3getY" mjs_v3_get_y1, 1;
    G_MJS_V3GETZ "$Elm$Kernel$MJS$v3getZ" mjs_v3_get_z1, 1;
    G_MJS_V3SETX "$Elm$Kernel$MJS$v3setX" mjs_v3_set_x2, 2;
    G_MJS_V3SETY "$Elm$Kernel$MJS$v3setY" mjs_v3_set_y2, 2;
    G_MJS_V3SETZ "$Elm$Kernel$MJS$v3setZ" mjs_v3_set_z2, 2;
    G_MJS_V3TORECORD "$Elm$Kernel$MJS$v3toRecord" mjs_v3_to_record1, 1;
    G_MJS_V3FROMRECORD "$Elm$Kernel$MJS$v3fromRecord" mjs_v3_from_record1, 1;
    G_MJS_V3ADD "$Elm$Kernel$MJS$v3add" mjs_v3_add2, 2;
    G_MJS_V3SUB "$Elm$Kernel$MJS$v3sub" mjs_v3_sub2, 2;
    G_MJS_V3NEGATE "$Elm$Kernel$MJS$v3negate" mjs_v3_negate1, 1;
    G_MJS_V3DIRECTION "$Elm$Kernel$MJS$v3direction" mjs_v3_direction2, 2;
    G_MJS_V3LENGTH "$Elm$Kernel$MJS$v3length" mjs_v3_length1, 1;
    G_MJS_V3LENGTHSQUARED "$Elm$Kernel$MJS$v3lengthSquared" mjs_v3_length_squared1, 1;
    G_MJS_V3DISTANCE "$Elm$Kernel$MJS$v3distance" mjs_v3_distance2, 2;
    G_MJS_V3DISTANCESQUARED "$Elm$Kernel$MJS$v3distanceSquared" mjs_v3_distance_squared2, 2;
    G_MJS_V3NORMALIZE "$Elm$Kernel$MJS$v3normalize" mjs_v3_normalize1, 1;
    G_MJS_V3SCALE "$Elm$Kernel$MJS$v3scale" mjs_v3_scale2, 2;
    G_MJS_V3DOT "$Elm$Kernel$MJS$v3dot" mjs_v3_dot2, 2;
    G_MJS_V3CROSS "$Elm$Kernel$MJS$v3cross" mjs_v3_cross2, 2;
    G_MJS_V3MUL4X4 "$Elm$Kernel$MJS$v3mul4x4" mjs_v3_mul4x4_2, 2;
    G_MJS_V4 "$Elm$Kernel$MJS$v4" mjs_v4_4, 4;
    G_MJS_V4GETX "$Elm$Kernel$MJS$v4getX" mjs_v4_get_x1, 1;
    G_MJS_V4GETY "$Elm$Kernel$MJS$v4getY" mjs_v4_get_y1, 1;
    G_MJS_V4GETZ "$Elm$Kernel$MJS$v4getZ" mjs_v4_get_z1, 1;
    G_MJS_V4GETW "$Elm$Kernel$MJS$v4getW" mjs_v4_get_w1, 1;
    G_MJS_V4SETX "$Elm$Kernel$MJS$v4setX" mjs_v4_set_x2, 2;
    G_MJS_V4SETY "$Elm$Kernel$MJS$v4setY" mjs_v4_set_y2, 2;
    G_MJS_V4SETZ "$Elm$Kernel$MJS$v4setZ" mjs_v4_set_z2, 2;
    G_MJS_V4SETW "$Elm$Kernel$MJS$v4setW" mjs_v4_set_w2, 2;
    G_MJS_V4TORECORD "$Elm$Kernel$MJS$v4toRecord" mjs_v4_to_record1, 1;
    G_MJS_V4FROMRECORD "$Elm$Kernel$MJS$v4fromRecord" mjs_v4_from_record1, 1;
    G_MJS_V4ADD "$Elm$Kernel$MJS$v4add" mjs_v4_add2, 2;
    G_MJS_V4SUB "$Elm$Kernel$MJS$v4sub" mjs_v4_sub2, 2;
    G_MJS_V4NEGATE "$Elm$Kernel$MJS$v4negate" mjs_v4_negate1, 1;
    G_MJS_V4DIRECTION "$Elm$Kernel$MJS$v4direction" mjs_v4_direction2, 2;
    G_MJS_V4LENGTH "$Elm$Kernel$MJS$v4length" mjs_v4_length1, 1;
    G_MJS_V4LENGTHSQUARED "$Elm$Kernel$MJS$v4lengthSquared" mjs_v4_length_squared1, 1;
    G_MJS_V4DISTANCE "$Elm$Kernel$MJS$v4distance" mjs_v4_distance2, 2;
    G_MJS_V4DISTANCESQUARED "$Elm$Kernel$MJS$v4distanceSquared" mjs_v4_distance_squared2, 2;
    G_MJS_V4NORMALIZE "$Elm$Kernel$MJS$v4normalize" mjs_v4_normalize1, 1;
    G_MJS_V4SCALE "$Elm$Kernel$MJS$v4scale" mjs_v4_scale2, 2;
    G_MJS_V4DOT "$Elm$Kernel$MJS$v4dot" mjs_v4_dot2, 2;
    G_MJS_M4X4FROMRECORD "$Elm$Kernel$MJS$m4x4fromRecord" mjs_m4x4_from_record1, 1;
    G_MJS_M4X4TORECORD "$Elm$Kernel$MJS$m4x4toRecord" mjs_m4x4_to_record1, 1;
    G_MJS_M4X4INVERSE "$Elm$Kernel$MJS$m4x4inverse" mjs_m4x4_inverse1, 1;
    G_MJS_M4X4INVERSEORTHONORMAL "$Elm$Kernel$MJS$m4x4inverseOrthonormal" mjs_m4x4_inverse_orthonormal1, 1;
    G_MJS_M4X4MAKEFRUSTUM "$Elm$Kernel$MJS$m4x4makeFrustum" mjs_m4x4_make_frustum6, 6;
    G_MJS_M4X4MAKEPERSPECTIVE "$Elm$Kernel$MJS$m4x4makePerspective" mjs_m4x4_make_perspective4, 4;
    G_MJS_M4X4MAKEORTHO "$Elm$Kernel$MJS$m4x4makeOrtho" mjs_m4x4_make_ortho6, 6;
    G_MJS_M4X4MAKEORTHO2D "$Elm$Kernel$MJS$m4x4makeOrtho2D" mjs_m4x4_make_ortho2d4, 4;
    G_MJS_M4X4MUL "$Elm$Kernel$MJS$m4x4mul" mjs_m4x4_mul2, 2;
    G_MJS_M4X4MULAFFINE "$Elm$Kernel$MJS$m4x4mulAffine" mjs_m4x4_mul_affine2, 2;
    G_MJS_M4X4MAKEROTATE "$Elm$Kernel$MJS$m4x4makeRotate" mjs_m4x4_make_rotate2, 2;
    G_MJS_M4X4ROTATE "$Elm$Kernel$MJS$m4x4rotate" mjs_m4x4_rotate3, 3;
    G_MJS_M4X4MAKESCALE3 "$Elm$Kernel$MJS$m4x4makeScale3" mjs_m4x4_make_scale3_3, 3;
    G_MJS_M4X4MAKESCALE "$Elm$Kernel$MJS$m4x4makeScale" mjs_m4x4_make_scale1, 1;
    G_MJS_M4X4SCALE3 "$Elm$Kernel$MJS$m4x4scale3" mjs_m4x4_scale3_4, 4;
    G_MJS_M4X4SCALE "$Elm$Kernel$MJS$m4x4scale" mjs_m4x4_scale2, 2;
    G_MJS_M4X4MAKETRANSLATE3 "$Elm$Kernel$MJS$m4x4makeTranslate3" mjs_m4x4_make_translate3_3, 3;
    G_MJS_M4X4MAKETRANSLATE "$Elm$Kernel$MJS$m4x4makeTranslate" mjs_m4x4_make_translate1, 1;
    G_MJS_M4X4TRANSLATE3 "$Elm$Kernel$MJS$m4x4translate3" mjs_m4x4_translate3_4, 4;
    G_MJS_M4X4TRANSLATE "$Elm$Kernel$MJS$m4x4translate" mjs_m4x4_translate2, 2;
    G_MJS_M4X4MAKELOOKAT "$Elm$Kernel$MJS$m4x4makeLookAt" mjs_m4x4_make_look_at3, 3;
    G_MJS_M4X4TRANSPOSE "$Elm$Kernel$MJS$m4x4transpose" mjs_m4x4_transpose1, 1;
    G_MJS_M4X4MAKEBASIS "$Elm$Kernel$MJS$m4x4makeBasis" mjs_m4x4_make_basis3, 3;
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
    G_BASICS_FDIV "$Basics$fdiv" rt_fdiv, 2;
    G_BASICS_IDIV "$Basics$idiv" rt_idiv, 2;
    G_BASICS_POW "$Basics$pow" rt_pow, 2;
    G_BASICS_AND "$Basics$and" basics_and, 2;
    G_BASICS_OR "$Basics$or" basics_or, 2;

    G_URL_PERCENTENCODE "$Url$percentEncode" url_percent_encode, 1;
    G_URL_PERCENTDECODE "$Url$percentDecode" url_percent_decode, 1;
    G_URL_FROMSTRING "$Url$fromString" url_from_string, 1;
    G_URL_TOSTRING "$Url$toString" url_to_string, 1;
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
    G_STRING_PAD "$String$pad" string_pad, 3;
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
    G_CHAR_ISOCTDIGIT "$Char$isOctDigit" char_is_oct_digit, 1;
    // Locale variants: elm's own are ASCII-locale-independent for the
    // characters tests exercise; map to the plain case kernels.
    G_CHAR_TOLOCALEUPPER "$Char$toLocaleUpper" char_to_upper, 1;
    G_CHAR_TOLOCALELOWER "$Char$toLocaleLower" char_to_lower, 1;

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
    G_REGEX_NEVER "$Elm$Kernel$Regex$never";
    G_REGEX_INFINITY "$Elm$Kernel$Regex$infinity";
    G_HTMLEV_TARGETVALUE "$Html$Events$targetValue";
    G_HTMLEV_TARGETCHECKED "$Html$Events$targetChecked";
    G_HTMLEV_KEYCODE "$Html$Events$keyCode";
}

/// DIAG (ALM_SEGV_DUMP=1): on SIGSEGV/SIGBUS, dump pc/lr, x0-x28, sp, the
/// faulting address, and nearby memory, then abort. Raw Darwin arm64 ABI
/// (the runtime builds as a single-file rustc crate — no libc crate).
#[cfg(all(not(target_arch = "wasm32"), target_os = "macos"))]
mod segv_dump {
    #[repr(C)]
    pub struct Sigaction {
        pub handler: usize,
        pub mask: u32,
        pub flags: i32,
    }
    extern "C" {
        pub fn sigaction(sig: i32, act: *const Sigaction, old: *mut Sigaction) -> i32;
        pub fn abort() -> !;
        pub fn _dyld_get_image_vmaddr_slide(idx: u32) -> isize;
    }
    pub const SIGSEGV: i32 = 11;
    pub const SIGBUS: i32 = 10;
    pub const SA_SIGINFO: i32 = 0x0040;

    unsafe fn rd(addr: u64) -> u64 {
        core::ptr::read_volatile(addr as *const u64)
    }

    pub unsafe extern "C" fn handler(_sig: i32, info: *const u8, ctx: *const u8) {
        // Darwin: siginfo.si_addr at +24; ucontext.uc_mcontext (ptr) at +48;
        // mcontext64.__ss (thread state) at +16: x[0..29], fp, lr, sp, pc.
        let fault = rd(info.add(24) as u64);
        let mc = rd(ctx.add(48) as u64);
        let ss = mc + 16;
        let x = |i: u64| rd(ss + i * 8);
        let (fp, lr, sp, pc) = (rd(ss + 29 * 8), rd(ss + 30 * 8), rd(ss + 31 * 8), rd(ss + 32 * 8));
        eprintln!("=== ALM SEGV DUMP ===");
        eprintln!("slide={:#x}", _dyld_get_image_vmaddr_slide(0));
        eprintln!("fault={:#x} pc={:#x} lr={:#x} sp={:#x} fp={:#x}", fault, pc, lr, sp, fp);
        for i in 0..29 {
            eprint!("x{}={:#x} ", i, x(i));
            if i % 6 == 5 {
                eprintln!();
            }
        }
        eprintln!();
        for i in 0..29u64 {
            let v = x(i);
            if v > 0x1_0000_0000 && v % 8 == 0 && v.abs_diff(fault) < 256 {
                eprintln!("x{} = {:#x} near fault; 8 words:", i, v);
                for k in 0..8u64 {
                    eprintln!("  +{:#04x}: {:#018x}", k * 8, rd(v + k * 8));
                }
            }
        }
        for i in 0..8u64 {
            let v = x(i);
            if v > 0x1_0000_0000 && v < 0x7_0000_0000 && v % 8 == 0 {
                eprintln!("x{} = {:#x} (heap?); 8 words:", i, v);
                for k in 0..8u64 {
                    eprintln!("  +{:#04x}: {:#018x}", k * 8, rd(v + k * 8));
                }
            }
        }
        eprintln!("fp chain (caller lrs):");
        let mut f = fp;
        for _ in 0..8 {
            if f < 0x1000 || f % 8 != 0 {
                break;
            }
            let next = rd(f);
            let ret = rd(f + 8);
            eprintln!("  fp={:#x} lr={:#x}", f, ret);
            if next <= f {
                break;
            }
            f = next;
        }
        eprintln!("stack sp..sp+256:");
        for k in 0..32u64 {
            eprintln!("  sp+{:#04x}: {:#018x}", k * 8, rd(sp + k * 8));
        }
        abort();
    }
}

#[cfg(all(not(target_arch = "wasm32"), target_os = "macos"))]
unsafe fn install_segv_dump() {
    if std::env::var("ALM_SEGV_DUMP").is_err() {
        return;
    }
    let sa = segv_dump::Sigaction {
        handler: segv_dump::handler as usize,
        mask: 0,
        flags: segv_dump::SA_SIGINFO,
    };
    segv_dump::sigaction(segv_dump::SIGSEGV, &sa, std::ptr::null_mut());
    segv_dump::sigaction(segv_dump::SIGBUS, &sa, std::ptr::null_mut());
}

#[cfg(not(all(not(target_arch = "wasm32"), target_os = "macos")))]
unsafe fn install_segv_dump() {}

unsafe fn runtime_init() {
    install_segv_dump();
    RT_TRUE.set(alloc(Value::Bool(true)));
    RT_FALSE.set(alloc(Value::Bool(false)));
    RT_UNIT.set(alloc(Value::Unit));
    NIL.set(alloc(Value::Nil));
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
    G_DICT_EMPTY.set(alloc(Value::Dict(0)));
    G_SET_EMPTY.set(alloc(Value::Set(0)));
    G_ARRAY_EMPTY.set(alloc(Value::Array(0)));
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
    // `Regex.never`: a pattern that can never match (`.^` — a char followed by
    // start-of-input). `infinity`: the `Int` limit meaning "all" for
    // find/replace/split (any value larger than a string's match count).
    let never_pat = b".^";
    G_REGEX_NEVER.set(alloc(Value::Regex(alm_rx_compile(
        never_pat.as_ptr(),
        never_pat.len(),
        false,
        false,
    ))));
    G_REGEX_INFINITY.set(mk_int(i32::MAX as i64));

    // elm/html + elm/virtual-dom: the per-tag/attr closures, then the event
    // decoders (`Html.Events.targetValue` reads `e.target.value`, etc.).
    init_baked_globals();
    G_HTMLEV_TARGETVALUE.set(target_field(b"value", Decoder::Str));
    G_HTMLEV_TARGETCHECKED.set(target_field(b"checked", Decoder::Bool));
    G_HTMLEV_KEYCODE.set(mk_decoder(Decoder::Field(b"keyCode".to_vec(), mk_decoder(Decoder::Int))));
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
    // Run the program on a thread with a large stack. Elm code recurses on the
    // native (C) stack for non-tail and mutual recursion (only self-tail-calls
    // become loops), and the 8MB default main-thread stack overflows on the
    // deep recursion typical of recursive-descent parsers and decoders — depth
    // a JS engine's larger, growable stack absorbs. 512MB matches "effectively
    // deep like JS" while staying bounded (a genuine infinite recursion still
    // faults, just later, rather than running unbounded).
    // Ensure the collector is initialised on this (GC-primary) thread before we
    // spawn, so the worker's `GC_register_my_thread` is allowed.
    gc_ensure_init();
    // DIAG: ALM_MAIN_THREAD=1 runs Elm on the primordial thread (8MB stack —
    // deep recursion will overflow) to discriminate worker-thread
    // registration/suspension bugs from everything else.
    if std::env::var("ALM_MAIN_THREAD").is_ok() {
        return alm_entry();
    }
    match std::thread::Builder::new()
        .stack_size(512 * 1024 * 1024)
        .spawn(|| unsafe {
            // The collector scans registered threads' stacks; a bare `std::thread`
            // is invisible to it, so register this thread (base .. current SP)
            // for the duration of the program.
            let mut sb = GcStackBase {
                mem_base: std::ptr::null_mut(),
                _reg_base: std::ptr::null_mut(),
            };
            GC_get_stack_base(&mut sb);
            GC_register_my_thread(&sb);
            let rc = alm_entry();
            // Exit the PROCESS from here, WITHOUT any teardown: returning
            // (and even process::exit, via Darwin's _tlv_exit) runs this
            // thread's TLS destructors, whose registration list lives on
            // the GC heap referenced only from thread-locals — which Boehm
            // does NOT scan. By teardown time those nodes have been
            // collected and recycled, and run_dtors jumps through garbage.
            use std::io::Write;
            let _ = std::io::stdout().flush();
            let _ = std::io::stderr().flush();
            extern "C" {
                fn _exit(code: i32) -> !;
            }
            _exit(rc);
        })
    {
        Ok(h) => h.join().unwrap_or(70),
        // If the thread can't be spawned, fall back to running inline.
        Err(_) => alm_entry(),
    }
}

#[cfg(target_arch = "wasm32")]
#[no_mangle]
pub unsafe extern "C" fn __main_argc_argv(_argc: i32, _argv: *const *const u8) -> i32 {
    alm_entry()
}

/// GC-visible pins for allocations that std keeps reachable only through
/// thread-local storage, which the conservative collector does not scan
/// (`thread::current()`'s Arc<Thread> is the known one). One slot per kind.
#[export_name = "alm_TLS_PINS"]
static mut TLS_PINS: [usize; 2] = [0; 2];

unsafe fn alm_entry() -> i32 {
    // Root this thread's std::thread::Thread handle before any Elm runs:
    // its Arc lives on the GC heap and is otherwise reachable only via TLS.
    let t = std::thread::current();
    *std::ptr::addr_of_mut!(TLS_PINS[0]) = std::mem::transmute::<std::thread::Thread, usize>(t);
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
        Value::Str(_) | Value::StrCat { .. } | Value::StrSlice { .. } => {
            out_line(sbytes(v));
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
