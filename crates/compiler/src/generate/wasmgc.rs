//! WasmGC backend (experimental, in progress). Emits a WebAssembly-GC module
//! binary directly (via `wasm-encoder`), using `struct`/`array`/`ref` heap
//! types so the host engine garbage-collects Elm values. Shares the front end
//! and monomorphizer with the LLVM backend; only codegen differs.
//!
//! Uniform value model: every Elm value is `(ref null eq)`. Int/Float are boxed
//! structs; Bool/Char/Unit are `i31`; String is `array i8`; List is a cons
//! struct (null = `[]`); Tuple/Record/ctor-args are arrays; custom types are
//! `struct { i32 tag, array args }`. All functions are `(N × eqref) -> eqref`.

use std::collections::HashMap;
use std::path::Path;

use wasm_encoder::{
    AbstractHeapType, BlockType, CodeSection, ConstExpr, DataCountSection, DataSection,
    ElementSection, Elements, EntityType, ExportKind, ExportSection, FieldType, Function,
    FunctionSection, GlobalSection, GlobalType, HeapType, ImportSection, Instruction, MemArg,
    MemorySection, MemoryType, Module, RawSection, RefType, StorageType, TypeSection, ValType,
};

use crate::ast::canonical as can;
use crate::ir::mono::{MonoProgram, TypedExpr, TypedKind, TypedLetDecl};
use crate::reporting::annotation::Region;

/// Dead-code-eliminate a finished WasmGC module. The code generator emits every
/// runtime helper (Dict, Json, Html, DOM, …) unconditionally, so most programs
/// carry kernel functions they never call. This computes the set of functions
/// reachable from the module's exports (following `call`/`return_call`/`ref.func`
/// edges, plus function references taken by globals and active/passive element
/// segments) and replaces every unreachable function body with a 3-byte
/// `unreachable` stub. Function *indices* are left untouched — only the Code
/// section changes — so no call site, export, or element entry needs rewriting.
fn tree_shake(bytes: &[u8]) -> Vec<u8> {
    use wasmparser::{ElementKind, Operator, Parser, Payload};

    // The `ref.func` targets reachable from a const-expr (global init / element
    // init). Returns the referenced function index, if any.
    fn const_expr_func(reader: wasmparser::OperatorsReader) -> Vec<u32> {
        let mut out = Vec::new();
        for op in reader {
            if let Ok(Operator::RefFunc { function_index }) = op {
                out.push(function_index);
            }
        }
        out
    }

    // Every import this generator emits is a host/DOM function (memory is
    // defined, not imported), so the imported-function count is exactly the
    // fixed import count and defined functions begin at that index.
    let num_imported_funcs: u32 = N_IMPORTS;
    let mut roots: Vec<u32> = Vec::new();
    // Per defined function (code order): (content_range, callee indices).
    let mut bodies: Vec<(std::ops::Range<usize>, Vec<u32>)> = Vec::new();

    for payload in Parser::new(0).parse_all(bytes) {
        let payload = match payload {
            Ok(p) => p,
            Err(_) => return bytes.to_vec(), // parse failure: leave the module as-is
        };
        match payload {
            Payload::ExportSection(reader) => {
                for export in reader {
                    if let Ok(ex) = export {
                        if ex.kind == wasmparser::ExternalKind::Func {
                            roots.push(ex.index);
                        }
                    }
                }
            }
            Payload::StartSection { func, .. } => roots.push(func),
            Payload::GlobalSection(reader) => {
                for g in reader {
                    if let Ok(g) = g {
                        roots.extend(const_expr_func(g.init_expr.get_operators_reader()));
                    }
                }
            }
            Payload::ElementSection(reader) => {
                for el in reader {
                    if let Ok(el) = el {
                        // A `declared` segment only permits `ref.func`; it does
                        // not itself keep functions live. Active/passive segments
                        // populate a table, so their functions are roots.
                        if matches!(el.kind, ElementKind::Declared) {
                            continue;
                        }
                        match el.items {
                            wasmparser::ElementItems::Functions(fs) => {
                                for fi in fs {
                                    if let Ok(fi) = fi {
                                        roots.push(fi);
                                    }
                                }
                            }
                            wasmparser::ElementItems::Expressions(_, exprs) => {
                                for e in exprs {
                                    if let Ok(e) = e {
                                        roots.extend(const_expr_func(e.get_operators_reader()));
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Payload::CodeSectionEntry(body) => {
                let mut callees = Vec::new();
                if let Ok(reader) = body.get_operators_reader() {
                    for op in reader {
                        match op {
                            Ok(Operator::Call { function_index })
                            | Ok(Operator::ReturnCall { function_index })
                            | Ok(Operator::RefFunc { function_index }) => callees.push(function_index),
                            _ => {}
                        }
                    }
                }
                bodies.push((body.range(), callees));
            }
            _ => {}
        }
    }

    // BFS over the call graph. Defined function `d` owns body `d - num_imported`.
    let mut reachable = std::collections::HashSet::new();
    let mut stack = roots;
    while let Some(f) = stack.pop() {
        if !reachable.insert(f) {
            continue;
        }
        if f < num_imported_funcs {
            continue; // imports have no body
        }
        if let Some((_, callees)) = bodies.get((f - num_imported_funcs) as usize) {
            for &c in callees {
                if !reachable.contains(&c) {
                    stack.push(c);
                }
            }
        }
    }

    // Rebuild the module: copy every section verbatim except Code, where dead
    // bodies become `unreachable` stubs (local-count 0, `unreachable`, `end`).
    const STUB: &[u8] = &[0x00, 0x00, 0x0b];
    let mut out = Module::new();
    let mut emitted_code = false;
    for payload in Parser::new(0).parse_all(bytes) {
        let payload = payload.expect("re-parse of a just-built module cannot fail");
        // The code section is emitted from `bodies`; skip its per-entry payloads.
        if matches!(payload, Payload::CodeSectionEntry(_) | Payload::CodeSectionStart { .. }) {
            if !emitted_code {
                let mut code = CodeSection::new();
                for (i, (range, _)) in bodies.iter().enumerate() {
                    if reachable.contains(&(num_imported_funcs + i as u32)) {
                        code.raw(&bytes[range.clone()]);
                    } else {
                        code.raw(STUB);
                    }
                }
                out.section(&code);
                emitted_code = true;
            }
            continue;
        }
        if let Some((id, range)) = payload.as_section() {
            out.section(&RawSection { id, data: &bytes[range] });
        }
    }
    out.finish()
}

const T_INT: u32 = 0; // struct { i64 }
const T_FLOAT: u32 = 1; // struct { f64 }
const T_STR: u32 = 2; // array (mut i8)
const T_ARR: u32 = 3; // array (mut eqref) — records, tuples, ctor args, list backing
// A `List a` is a vector, not a cons list (matching alm's native backend):
// elements live at data[cap-len .. cap) in head-first order (so iteration runs
// forward through memory), with free space at the FRONT of the backing. `cons`
// prepends into that slack in amortized O(1); `tail` just shortens the view.
const T_BACK: u32 = 4; // struct { mut i32 head, (ref T_ARR) data } — head = frontmost used index
const T_LIST: u32 = 5; // struct { i32 len, (ref null T_BACK) bk }
const T_CTOR: u32 = 6; // struct { i32 tag, (ref null T_ARR) args }
const T_CLOS: u32 = 7; // struct { funcref, i32 arity, i32 applied, (ref null T_ARR) args }
// Dict/Set = a persistent treap (BST by key, heap by (priority,key) where
// priority = val_hash(key) — deterministic, so equal key-sets give identical
// trees). O(log n) persistent insert/get/remove; in-order traversal is sorted.
const T_TNODE: u32 = 8; // struct { key, value:eqref, pri:i32, left,right:(ref null T_TNODE) }
// A growable byte buffer for O(n) JSON serialization: `json_write` appends
// primitives straight into `buf` (amortized doubling) instead of allocating an
// intermediate String per node and joining them.
const T_SB: u32 = 9; // struct { mut buf: (ref T_STR), mut len: i32 }
const N_FIXED: u32 = 10;

// Imported DOM host functions occupy the first function indices; defined
// functions are therefore offset by N_IMPORTS (see build()).
const DOM_CREATE_ELEMENT: u32 = 0; // (ptr,len) -> handle
const DOM_CREATE_TEXT: u32 = 1; // (ptr,len) -> handle
const DOM_SET_ATTRIBUTE: u32 = 2; // (node,kp,kl,vp,vl)
const DOM_SET_STYLE: u32 = 3; // (node,kp,kl,vp,vl)
const DOM_APPEND_CHILD: u32 = 4; // (parent,child)
const DOM_ADD_EVENT_LISTENER: u32 = 5; // (node,np,nl,hid)
const DOM_MOUNT: u32 = 6; // (root)
const DOM_REPLACE_ROOT: u32 = 7; // (root)
const DOM_CHILD: u32 = 8; // (parent,i) -> handle
const DOM_SET_TEXT: u32 = 9; // (node,ptr,len)
const DOM_REMOVE_ATTRIBUTE: u32 = 10; // (node,ptr,len)
const DOM_REMOVE_CHILD: u32 = 11; // (parent,child)
const DOM_REPLACE: u32 = 12; // (old,new) — replace old with new in its parent
const DOM_REMOVE_EVENT_LISTENER: u32 = 13; // (node,ptr,len,hid)
const HOST_PORT_OUT: u32 = 14; // (nameptr,namelen,jsonptr,jsonlen) — outgoing port
const HOST_SET_TITLE: u32 = 15; // (ptr,len) — Browser.document title
const HOST_HTTP: u32 = 16; // (urlptr,urllen,reqId) — start an HTTP GET
const HOST_CLEAR_TIMERS: u32 = 17; // () — cancel all Time.every intervals
const HOST_SET_INTERVAL: u32 = 18; // (intervalMs:f64, slot) — register a timer
const HOST_CLEAR_DOM: u32 = 19; // () — remove all document-event listeners
const HOST_ADD_DOM: u32 = 20; // (nameptr,namelen,slot) — add a document listener
const HOST_PUSH_URL: u32 = 21; // (ptr,len,replace) — history push/replace
const HOST_GET_URL: u32 = 22; // (outptr) -> len — write current href to memory
const HOST_LOAD: u32 = 23; // (ptr,len) — full page navigation
const HOST_CLEAR_FRAMES: u32 = 24; // () — cancel the animation-frame loop
const HOST_REQUEST_FRAME: u32 = 25; // (slot) — start an animation-frame loop
// Host Math.* for the transcendentals (no wasm intrinsics; matches the JS
// backend's libm bit-for-bit). Unary (f64)->f64 except atan2/pow ((f64,f64)->f64).
const MATH_SIN: u32 = 26;
const MATH_COS: u32 = 27;
const MATH_TAN: u32 = 28;
const MATH_ASIN: u32 = 29;
const MATH_ACOS: u32 = 30;
const MATH_ATAN: u32 = 31;
const MATH_LOG: u32 = 32;
const MATH_ATAN2: u32 = 33;
const MATH_POW: u32 = 34;
const N_IMPORTS: u32 = 35;

// Globals: 0=jstr,1=jpos,2=jerr (JSON parser); 3=model,4=update,5=view (the
// running program); 6=handlers array,7=next handler id; 8=mem bump pointer.
const G_MODEL: u32 = 3;
const G_UPDATE: u32 = 4;
const G_VIEW: u32 = 5;
const G_HANDLERS: u32 = 6;
const G_NEXT_HID: u32 = 7;
const G_BUMP: u32 = 8;
const G_PREV: u32 = 9; // previously-rendered vdom (for diff/patch)
const G_ROOT: u32 = 10; // mounted root DOM handle
const G_KIND: u32 = 11; // program kind: 0 = sandbox, 1 = element, 2 = document
const G_SUBS: u32 = 12; // subscriptions function (null for sandbox)
const G_HTTP: u32 = 13; // in-flight HTTP expects, indexed by request id
const G_NEXT_REQ: u32 = 14; // next HTTP request id
const G_TICKS: u32 = 15; // active Time.every toMsg callbacks, indexed by slot
const G_NEXT_TICK: u32 = 16; // next timer slot (reset each reconcile)
const G_DOMSUBS: u32 = 17; // active Browser.Events decoders, indexed by slot
const G_NEXT_DOM: u32 = 18; // next document-sub slot (reset each reconcile)
const G_URLCHG: u32 = 19; // Browser.application onUrlChange handler (null otherwise)
const G_FRAMES: u32 = 20; // active onAnimationFrame subs, indexed by slot
const G_NEXT_FRAME: u32 = 21; // next frame-sub slot (reset each reconcile)
const G_SORT_CMP: u32 = 22; // List.sortWith comparator (set for the sort's duration)

/// How the merge sort orders elements. `Value`: `val_compare` on the element.
/// `ByKey`: `val_compare` on `element[0]` (for Dict/Set pair lists). `Cmp`: the
/// user comparator in `G_SORT_CMP`, reading its `Order` result (LT=0/EQ=1/GT=2).
#[derive(Clone, Copy, PartialEq)]
enum SortMode {
    Value,
    ByKey,
    Cmp,
}
// Int representation: values in [-2^30, 2^30) live UNBOXED as i31ref (no heap
// allocation); larger values box as T_INT. `box_int`/`unbox_int` bridge the two.
const I31_MIN: i64 = -(1 << 30);
const I31_MAX: i64 = 1 << 30;
const BUMP_BASE: i32 = 1 << 16; // DOM string scratch starts at 64 KiB
const MAX_HANDLERS: u32 = 4096;
/// Highest arity the closure `apply` dispatcher handles.
const MAX_ARITY: u32 = 6;

fn eqref() -> ValType {
    ValType::Ref(RefType {
        nullable: true,
        heap_type: HeapType::Abstract { shared: false, ty: AbstractHeapType::Eq },
    })
}

fn ref_to(idx: u32) -> ValType {
    ValType::Ref(RefType { nullable: true, heap_type: HeapType::Concrete(idx) })
}

fn eq_heap() -> HeapType {
    HeapType::Abstract { shared: false, ty: AbstractHeapType::Eq }
}

fn is_named_type(tipe: &can::Type, name: &str) -> bool {
    matches!(tipe, can::Type::Type(_, n, _) if n.as_str() == name)
}
fn is_float(tipe: &can::Type) -> bool {
    is_named_type(tipe, "Float")
}
fn is_string(tipe: &can::Type) -> bool {
    is_named_type(tipe, "String")
}

fn funcref() -> ValType {
    ValType::Ref(RefType {
        nullable: true,
        heap_type: HeapType::Abstract { shared: false, ty: AbstractHeapType::Func },
    })
}

/// Position of `field` in a record type's fields, sorted by name (the layout
/// order used by `T_ARR`-backed records).
fn record_field_index(tipe: &can::Type, field: &str) -> Result<usize, String> {
    if let can::Type::Record(fields, _) = tipe {
        let mut names: Vec<&str> = fields.iter().map(|(n, _)| n.as_str()).collect();
        names.sort();
        return names
            .iter()
            .position(|n| *n == field)
            .ok_or_else(|| format!("wasmgc: record has no field `{field}`"));
    }
    Err(format!("wasmgc: field access on non-record type for `{field}`"))
}

fn cast_to(idx: u32) -> Instruction<'static> {
    Instruction::RefCastNonNull(HeapType::Concrete(idx))
}

/// Nullable downcast (e.g. an eqref that may be a T_TNODE or null).
fn cast_null(idx: u32) -> Instruction<'static> {
    Instruction::RefCastNullable(HeapType::Concrete(idx))
}

fn struct_type(types: &mut TypeSection, fields: &[FieldType]) {
    types.ty().struct_(fields.iter().copied());
}

fn mem0() -> MemArg {
    MemArg { offset: 0, align: 0, memory_index: 0 }
}

/// Emit `local -= 1` for an i32 local.
fn dec(f: &mut Function, local: u32) {
    f.instruction(&Instruction::LocalGet(local));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Sub);
    f.instruction(&Instruction::LocalSet(local));
}

/// Read one byte `s[i + k]` (unsigned) onto the stack.
fn byte_at(f: &mut Function, s: u32, i: u32, k: i32) {
    f.instruction(&Instruction::LocalGet(s));
    f.instruction(&Instruction::LocalGet(i));
    if k != 0 {
        f.instruction(&Instruction::I32Const(k));
        f.instruction(&Instruction::I32Add);
    }
    f.instruction(&Instruction::ArrayGetU(T_STR));
}

/// Decode one UTF-8 code point from `s` at index `i` (locals), writing the
/// code point to `cp` and its byte width to `adv`.
fn utf8_decode(f: &mut Function, s: u32, i: u32, cp: u32, adv: u32) {
    // cp = b0
    byte_at(f, s, i, 0);
    f.instruction(&Instruction::LocalSet(cp));
    f.instruction(&Instruction::LocalGet(cp));
    f.instruction(&Instruction::I32Const(0x80));
    f.instruction(&Instruction::I32LtU);
    f.instruction(&Instruction::If(BlockType::Empty));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::LocalSet(adv));
    f.instruction(&Instruction::Else);
    f.instruction(&Instruction::LocalGet(cp));
    f.instruction(&Instruction::I32Const(0xE0));
    f.instruction(&Instruction::I32LtU);
    f.instruction(&Instruction::If(BlockType::Empty));
    // 2-byte
    f.instruction(&Instruction::I32Const(2));
    f.instruction(&Instruction::LocalSet(adv));
    f.instruction(&Instruction::LocalGet(cp));
    f.instruction(&Instruction::I32Const(0x1F));
    f.instruction(&Instruction::I32And);
    f.instruction(&Instruction::I32Const(6));
    f.instruction(&Instruction::I32Shl);
    byte_at(f, s, i, 1);
    f.instruction(&Instruction::I32Const(0x3F));
    f.instruction(&Instruction::I32And);
    f.instruction(&Instruction::I32Or);
    f.instruction(&Instruction::LocalSet(cp));
    f.instruction(&Instruction::Else);
    f.instruction(&Instruction::LocalGet(cp));
    f.instruction(&Instruction::I32Const(0xF0));
    f.instruction(&Instruction::I32LtU);
    f.instruction(&Instruction::If(BlockType::Empty));
    // 3-byte
    f.instruction(&Instruction::I32Const(3));
    f.instruction(&Instruction::LocalSet(adv));
    f.instruction(&Instruction::LocalGet(cp));
    f.instruction(&Instruction::I32Const(0x0F));
    f.instruction(&Instruction::I32And);
    f.instruction(&Instruction::I32Const(12));
    f.instruction(&Instruction::I32Shl);
    byte_at(f, s, i, 1);
    f.instruction(&Instruction::I32Const(0x3F));
    f.instruction(&Instruction::I32And);
    f.instruction(&Instruction::I32Const(6));
    f.instruction(&Instruction::I32Shl);
    f.instruction(&Instruction::I32Or);
    byte_at(f, s, i, 2);
    f.instruction(&Instruction::I32Const(0x3F));
    f.instruction(&Instruction::I32And);
    f.instruction(&Instruction::I32Or);
    f.instruction(&Instruction::LocalSet(cp));
    f.instruction(&Instruction::Else);
    // 4-byte
    f.instruction(&Instruction::I32Const(4));
    f.instruction(&Instruction::LocalSet(adv));
    f.instruction(&Instruction::LocalGet(cp));
    f.instruction(&Instruction::I32Const(0x07));
    f.instruction(&Instruction::I32And);
    f.instruction(&Instruction::I32Const(18));
    f.instruction(&Instruction::I32Shl);
    byte_at(f, s, i, 1);
    f.instruction(&Instruction::I32Const(0x3F));
    f.instruction(&Instruction::I32And);
    f.instruction(&Instruction::I32Const(12));
    f.instruction(&Instruction::I32Shl);
    f.instruction(&Instruction::I32Or);
    byte_at(f, s, i, 2);
    f.instruction(&Instruction::I32Const(0x3F));
    f.instruction(&Instruction::I32And);
    f.instruction(&Instruction::I32Const(6));
    f.instruction(&Instruction::I32Shl);
    f.instruction(&Instruction::I32Or);
    byte_at(f, s, i, 3);
    f.instruction(&Instruction::I32Const(0x3F));
    f.instruction(&Instruction::I32And);
    f.instruction(&Instruction::I32Or);
    f.instruction(&Instruction::LocalSet(cp));
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);
}

/// Compute the UTF-8 byte width of the code point in local `cp`, into `bl`.
fn utf8_byte_len(f: &mut Function, cp: u32, bl: u32) {
    f.instruction(&Instruction::I32Const(4));
    f.instruction(&Instruction::LocalSet(bl));
    for (limit, len) in [(0x10000, 3), (0x800, 2), (0x80, 1)] {
        f.instruction(&Instruction::LocalGet(cp));
        f.instruction(&Instruction::I32Const(limit));
        f.instruction(&Instruction::I32LtU);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(len));
        f.instruction(&Instruction::LocalSet(bl));
        f.instruction(&Instruction::End);
    }
}

/// Write `out[off] = 0x80 | ((cp >> shift) & 0x3F)` (a UTF-8 continuation or,
/// with the leading bits pre-masked by the caller, any body byte).
fn write_byte(f: &mut Function, out: u32, off: u32, off_delta: i32, lead: i32, cp: u32, shift: i32, mask: i32) {
    f.instruction(&Instruction::LocalGet(out));
    f.instruction(&Instruction::LocalGet(off));
    if off_delta != 0 {
        f.instruction(&Instruction::I32Const(off_delta));
        f.instruction(&Instruction::I32Add);
    }
    f.instruction(&Instruction::I32Const(lead));
    f.instruction(&Instruction::LocalGet(cp));
    if shift != 0 {
        f.instruction(&Instruction::I32Const(shift));
        f.instruction(&Instruction::I32ShrU);
    }
    f.instruction(&Instruction::I32Const(mask));
    f.instruction(&Instruction::I32And);
    f.instruction(&Instruction::I32Or);
    f.instruction(&Instruction::ArraySet(T_STR));
}

/// Encode the code point in local `cp` into `out` at byte offset `off`,
/// advancing `off` by the encoded width.
fn utf8_encode(f: &mut Function, out: u32, off: u32, cp: u32) {
    f.instruction(&Instruction::LocalGet(cp));
    f.instruction(&Instruction::I32Const(0x80));
    f.instruction(&Instruction::I32LtU);
    f.instruction(&Instruction::If(BlockType::Empty));
    write_byte(f, out, off, 0, 0, cp, 0, 0x7F);
    bump(f, off, 1);
    f.instruction(&Instruction::Else);
    f.instruction(&Instruction::LocalGet(cp));
    f.instruction(&Instruction::I32Const(0x800));
    f.instruction(&Instruction::I32LtU);
    f.instruction(&Instruction::If(BlockType::Empty));
    write_byte(f, out, off, 0, 0xC0, cp, 6, 0x1F);
    write_byte(f, out, off, 1, 0x80, cp, 0, 0x3F);
    bump(f, off, 2);
    f.instruction(&Instruction::Else);
    f.instruction(&Instruction::LocalGet(cp));
    f.instruction(&Instruction::I32Const(0x10000));
    f.instruction(&Instruction::I32LtU);
    f.instruction(&Instruction::If(BlockType::Empty));
    write_byte(f, out, off, 0, 0xE0, cp, 12, 0x0F);
    write_byte(f, out, off, 1, 0x80, cp, 6, 0x3F);
    write_byte(f, out, off, 2, 0x80, cp, 0, 0x3F);
    bump(f, off, 3);
    f.instruction(&Instruction::Else);
    write_byte(f, out, off, 0, 0xF0, cp, 18, 0x07);
    write_byte(f, out, off, 1, 0x80, cp, 12, 0x3F);
    write_byte(f, out, off, 2, 0x80, cp, 6, 0x3F);
    write_byte(f, out, off, 3, 0x80, cp, 0, 0x3F);
    bump(f, off, 4);
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);
}

/// Build a fresh `T_STR` holding bytes `s[a..b)` into local `out`, using `i`
/// as a scratch counter. All arguments are local indices.
fn slice_into(f: &mut Function, s: u32, a: u32, b: u32, out: u32, i: u32) {
    f.instruction(&Instruction::LocalGet(b));
    f.instruction(&Instruction::LocalGet(a));
    f.instruction(&Instruction::I32Sub);
    f.instruction(&Instruction::ArrayNewDefault(T_STR));
    f.instruction(&Instruction::LocalSet(out));
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalSet(i));
    f.instruction(&Instruction::Block(BlockType::Empty));
    f.instruction(&Instruction::Loop(BlockType::Empty));
    f.instruction(&Instruction::LocalGet(i));
    f.instruction(&Instruction::LocalGet(b));
    f.instruction(&Instruction::LocalGet(a));
    f.instruction(&Instruction::I32Sub);
    f.instruction(&Instruction::I32GeS);
    f.instruction(&Instruction::BrIf(1));
    f.instruction(&Instruction::LocalGet(out));
    f.instruction(&Instruction::LocalGet(i));
    f.instruction(&Instruction::LocalGet(s));
    f.instruction(&Instruction::LocalGet(a));
    f.instruction(&Instruction::LocalGet(i));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::ArrayGetU(T_STR));
    f.instruction(&Instruction::ArraySet(T_STR));
    f.instruction(&Instruction::LocalGet(i));
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalSet(i));
    f.instruction(&Instruction::Br(0));
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);
}

/// Push the `i32` tag of the custom-type value in `local`.
fn ctor_tag(f: &mut Function, local: u32) {
    f.instruction(&Instruction::LocalGet(local));
    f.instruction(&cast_to(T_CTOR));
    f.instruction(&Instruction::StructGet { struct_type_index: T_CTOR, field_index: 0 });
}

/// Push argument 0 of the custom-type value in `local`.
fn ctor_arg0(f: &mut Function, local: u32) {
    ctor_argn(f, local, 0);
}

/// Push argument `n` of the custom-type value in `local`.
fn ctor_argn(f: &mut Function, local: u32, n: i32) {
    f.instruction(&Instruction::LocalGet(local));
    f.instruction(&cast_to(T_CTOR));
    f.instruction(&Instruction::StructGet { struct_type_index: T_CTOR, field_index: 1 });
    f.instruction(&Instruction::I32Const(n));
    f.instruction(&Instruction::ArrayGet(T_ARR));
}

/// Push a JSON `Value` = `T_CTOR { tag, [arg-on-stack] }` — arg must already be
/// on the stack; emits `[tag] arg -> ...` is impossible, so callers push tag
/// then the arg then call this to wrap. Here `tag` is pushed, so use as:
/// `push tag; <compute arg>; wrap1()`.
fn wrap1(f: &mut Function) {
    f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 1 });
    f.instruction(&Instruction::StructNew(T_CTOR));
}

/// Push a decode failure: `Err (Failure "" null)` (Result tag 1 → Error tag 3).
fn push_decode_err(f: &mut Function) {
    f.instruction(&Instruction::I32Const(1)); // Err
    f.instruction(&Instruction::I32Const(3)); // Failure
    push_str_const(f, "");
    f.instruction(&Instruction::I32Const(0)); // a JNULL Value
    f.instruction(&Instruction::RefNull(HeapType::Concrete(T_ARR)));
    f.instruction(&Instruction::StructNew(T_CTOR));
    f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 2 });
    f.instruction(&Instruction::StructNew(T_CTOR)); // Failure
    f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 1 });
    f.instruction(&Instruction::StructNew(T_CTOR)); // Err
}

/// Push the current parse byte `jstr[jpos]`, or -1 at end of input.
/// (globals: 0=jstr, 1=jpos.)
fn json_cur(f: &mut Function) {
    f.instruction(&Instruction::GlobalGet(1));
    f.instruction(&Instruction::GlobalGet(0));
    f.instruction(&cast_to(T_STR));
    f.instruction(&Instruction::ArrayLen);
    f.instruction(&Instruction::I32GeS);
    f.instruction(&Instruction::If(BlockType::Result(ValType::I32)));
    f.instruction(&Instruction::I32Const(-1));
    f.instruction(&Instruction::Else);
    f.instruction(&Instruction::GlobalGet(0));
    f.instruction(&cast_to(T_STR));
    f.instruction(&Instruction::GlobalGet(1));
    f.instruction(&Instruction::ArrayGetU(T_STR));
    f.instruction(&Instruction::End);
}

/// Emit `local += n` for an i32 local.
fn bump(f: &mut Function, local: u32, n: i32) {
    f.instruction(&Instruction::LocalGet(local));
    f.instruction(&Instruction::I32Const(n));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalSet(local));
}

/// The DOM event name for a plain-message `Html.Events.on*` helper (those whose
/// handler is just `Decode.succeed msg`), or None for the rest.
fn html_event_name(ev: &str) -> Option<&'static str> {
    Some(match ev {
        "onClick" => "click",
        "onDoubleClick" => "dblclick",
        "onMouseDown" => "mousedown",
        "onMouseUp" => "mouseup",
        "onMouseEnter" => "mouseenter",
        "onMouseLeave" => "mouseleave",
        "onMouseOver" => "mouseover",
        "onMouseOut" => "mouseout",
        "onSubmit" => "submit",
        "onBlur" => "blur",
        "onFocus" => "focus",
        _ => return None,
    })
}

/// Common string-valued Html.Attributes helpers → their DOM attribute name.
/// (attribute/style are handled separately; boolean/property attrs are not
/// here.) Elm's trailing-underscore names (type_) map to the bare attribute.
fn html_attr_name(a: &str) -> Option<&'static str> {
    Some(match a {
        "id" => "id",
        "class" => "class",
        "href" => "href",
        "src" => "src",
        "title" => "title",
        "placeholder" => "placeholder",
        "value" => "value",
        "name" => "name",
        "alt" => "alt",
        "type_" => "type",
        "for_" => "for",
        "rel" => "rel",
        "target" => "target",
        _ => return None,
    })
}

/// Browser.Events decoder subscriptions → the document event name they listen
/// for. (onResize/onAnimationFrame* take a different shape and are separate.)
fn browser_event_name(ev: &str) -> Option<&'static str> {
    Some(match ev {
        "onKeyDown" => "keydown",
        "onKeyUp" => "keyup",
        "onKeyPress" => "keypress",
        "onClick" => "click",
        "onMouseMove" => "mousemove",
        "onMouseDown" => "mousedown",
        "onMouseUp" => "mouseup",
        _ => return None,
    })
}

/// A nullable reference to concrete type `idx`.
fn ref_null_to(idx: u32) -> ValType {
    ValType::Ref(RefType { nullable: true, heap_type: HeapType::Concrete(idx) })
}

// ---- List (vector) primitives ----------------------------------------------
// A list value lives in local `l` (a `T_LIST`). Elements occupy
// `data[cap-len .. cap)` head-first; these helpers read a list without needing
// scratch locals (they re-read `l` as required).

/// Push the list's element count (`i32`).
fn list_len(f: &mut Function, l: u32) {
    f.instruction(&Instruction::LocalGet(l));
    f.instruction(&cast_to(T_LIST));
    f.instruction(&Instruction::StructGet { struct_type_index: T_LIST, field_index: 0 });
}

/// Push the list's backing (`ref null T_BACK`).
fn list_bk(f: &mut Function, l: u32) {
    f.instruction(&Instruction::LocalGet(l));
    f.instruction(&cast_to(T_LIST));
    f.instruction(&Instruction::StructGet { struct_type_index: T_LIST, field_index: 1 });
}

/// Push the backing data array (`ref T_ARR`). Traps on the empty list.
fn list_data(f: &mut Function, l: u32) {
    list_bk(f, l);
    f.instruction(&cast_to(T_BACK));
    f.instruction(&Instruction::StructGet { struct_type_index: T_BACK, field_index: 1 });
}

/// Push the head's index into the backing (`start = cap - len`).
fn list_start(f: &mut Function, l: u32) {
    list_data(f, l);
    f.instruction(&Instruction::ArrayLen);
    list_len(f, l);
    f.instruction(&Instruction::I32Sub);
}

/// Push the head element (`data[start]`). Caller must ensure the list is
/// non-empty.
fn list_head(f: &mut Function, l: u32) {
    list_data(f, l);
    list_start(f, l);
    f.instruction(&Instruction::ArrayGet(T_ARR));
}

/// Push the tail as a fresh `T_LIST` view sharing the backing (`{len-1, bk}`).
fn list_tail(f: &mut Function, l: u32) {
    list_len(f, l);
    f.instruction(&Instruction::I32Const(1));
    f.instruction(&Instruction::I32Sub);
    list_bk(f, l);
    f.instruction(&Instruction::StructNew(T_LIST));
}

/// Push element at head-offset held in local `iloc` (`data[start+i]`).
fn list_elem(f: &mut Function, l: u32, iloc: u32) {
    list_data(f, l);
    list_start(f, l);
    f.instruction(&Instruction::LocalGet(iloc));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::ArrayGet(T_ARR));
}

/// Push i32 1 iff the list is empty.
fn list_is_empty(f: &mut Function, l: u32) {
    list_len(f, l);
    f.instruction(&Instruction::I32Eqz);
}

/// Build a constant `T_STR` on the stack from ASCII bytes (via `array.new_fixed`,
/// so synth helpers get string literals without a data segment).
fn push_str_const(f: &mut Function, s: &str) {
    for b in s.bytes() {
        f.instruction(&Instruction::I32Const(b as i32));
    }
    f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_STR, array_size: s.len() as u32 });
}

/// Push the empty list value (`{0, null}`).
fn push_empty_list(f: &mut Function) {
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::RefNull(HeapType::Concrete(T_BACK)));
    f.instruction(&Instruction::StructNew(T_LIST));
}

/// Copy every element of list `src` into `dst[dstoff..]`, head-first. All of
/// `i,sd,ss,len` are scratch i32/ref locals the caller reserves. Safe on the
/// empty list (reads the backing only when non-empty).
#[allow(clippy::too_many_arguments)]
fn copy_into(
    f: &mut Function,
    src: u32,
    dst: u32,
    dstoff: u32,
    i: u32,
    sd: u32,
    ss: u32,
    len: u32,
) {
    list_len(f, src);
    f.instruction(&Instruction::LocalSet(len));
    f.instruction(&Instruction::LocalGet(len));
    f.instruction(&Instruction::If(BlockType::Empty));
    list_data(f, src);
    f.instruction(&Instruction::LocalSet(sd));
    list_start(f, src);
    f.instruction(&Instruction::LocalSet(ss));
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::LocalSet(i));
    f.instruction(&Instruction::Block(BlockType::Empty));
    f.instruction(&Instruction::Loop(BlockType::Empty));
    f.instruction(&Instruction::LocalGet(i));
    f.instruction(&Instruction::LocalGet(len));
    f.instruction(&Instruction::I32GeS);
    f.instruction(&Instruction::BrIf(1));
    f.instruction(&Instruction::LocalGet(dst));
    f.instruction(&Instruction::LocalGet(dstoff));
    f.instruction(&Instruction::LocalGet(i));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalGet(sd));
    f.instruction(&Instruction::LocalGet(ss));
    f.instruction(&Instruction::LocalGet(i));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::ArrayGet(T_ARR));
    f.instruction(&Instruction::ArraySet(T_ARR));
    bump(f, i, 1);
    f.instruction(&Instruction::Br(0));
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);
    f.instruction(&Instruction::End);
}

pub fn build(
    mono: &MonoProgram,
    output: &Path,
    ports: &HashMap<String, bool>,
) -> Result<(), String> {
    let mut cg = Codegen::new(mono);
    cg.ports = ports.clone();
    let bytes = cg.build()?;
    std::fs::write(output, bytes).map_err(|e| e.to_string())
}

struct Codegen<'a> {
    mono: &'a MonoProgram,
    /// Port name -> outgoing? (for `Call(port, [arg])` → Cmd resolution).
    ports: HashMap<String, bool>,
    func_index: HashMap<String, u32>,
    /// arity -> function-type index.
    fn_types: HashMap<u32, u32>,
    fn_type_order: Vec<u32>, // arities in type-index order
    next_type: u32,
    /// Concatenated bytes of all string literals (one passive data segment).
    string_data: Vec<u8>,
    /// literal text -> (offset, len) in `string_data`.
    str_offsets: HashMap<String, (u32, u32)>,
    /// Synthesized-helper function indices (set once user funcs are counted).
    str_append_idx: u32,
    str_from_int_idx: u32,
    apply1_idx: u32,
    list_cons_idx: u32,
    list_map_idx: u32,
    list_foldl_idx: u32,
    list_length_idx: u32,
    list_append_idx: u32,
    val_eq_idx: u32,
    list_reverse_idx: u32,
    list_filter_idx: u32,
    list_foldr_idx: u32,
    modby_idx: u32,
    list_range_idx: u32,
    list_member_idx: u32,
    list_take_idx: u32,
    list_drop_idx: u32,
    list_concat_idx: u32,
    list_head_idx: u32,
    list_tail_idx: u32,
    maybe_with_default_idx: u32,
    maybe_map_idx: u32,
    maybe_and_then_idx: u32,
    maybe_map2_idx: u32,
    maybe_map3_idx: u32,
    result_map2_idx: u32,
    result_map3_idx: u32,
    str_join_idx: u32,
    str_repeat_idx: u32,
    str_starts_with_idx: u32,
    str_ends_with_idx: u32,
    val_compare_idx: u32,
    list_insert_idx: u32,
    list_sort_idx: u32,
    list_all_idx: u32,
    list_any_idx: u32,
    list_min_idx: u32,
    list_max_idx: u32,
    list_indexed_map_idx: u32,
    list_sum_idx: u32,
    list_product_idx: u32,
    list_map2_idx: u32,
    str_upper_idx: u32,
    str_lower_idx: u32,
    str_trim_idx: u32,
    str_left_idx: u32,
    str_right_idx: u32,
    str_dropleft_idx: u32,
    str_dropright_idx: u32,
    str_to_int_idx: u32,
    str_contains_idx: u32,
    str_to_list_idx: u32,
    str_from_list_idx: u32,
    str_from_char_idx: u32,
    str_uncons_idx: u32,
    str_length_idx: u32,
    clamp_idx: u32,
    result_with_default_idx: u32,
    result_map_idx: u32,
    result_map_error_idx: u32,
    result_and_then_idx: u32,
    result_to_maybe_idx: u32,
    result_from_maybe_idx: u32,
    str_split_idx: u32,
    str_words_idx: u32,
    str_lines_idx: u32,
    list_repeat_idx: u32,
    list_filter_map_idx: u32,
    list_sortby_idx: u32,
    list_sortby_insert_idx: u32,
    str_pad_left_idx: u32,
    str_pad_right_idx: u32,
    from_polar_idx: u32,
    to_polar_idx: u32,
    str_slice_idx: u32,
    str_pad_both_idx: u32,
    list_intersperse_idx: u32,
    list_map3_idx: u32,
    list_map4_idx: u32,
    list_map5_idx: u32,
    list_partition_idx: u32,
    list_unzip_idx: u32,
    dict_get_idx: u32,
    dict_insert_idx: u32,
    dict_remove_idx: u32,
    dict_from_list_idx: u32,
    dict_foldl_idx: u32,
    dict_foldr_idx: u32,
    dict_map_idx: u32,
    dict_filter_idx: u32,
    dict_keys_idx: u32,
    dict_values_idx: u32,
    dict_intersect_idx: u32,
    dict_diff_idx: u32,
    dict_update_idx: u32,
    set_insert_idx: u32,
    set_member_idx: u32,
    set_remove_idx: u32,
    set_from_list_idx: u32,
    set_intersect_idx: u32,
    set_diff_idx: u32,
    val_hash_idx: u32,
    treap_get_idx: u32,
    treap_insert_idx: u32,
    treap_merge_idx: u32,
    treap_remove_idx: u32,
    treap_pairs_idx: u32,
    treap_foldl_idx: u32,
    treap_foldr_idx: u32,
    treap_insert_pairs_idx: u32,
    treap_insert_elems_idx: u32,
    array_get_idx: u32,
    array_set_idx: u32,
    array_push_idx: u32,
    array_tighten_idx: u32,
    list_to_array_idx: u32,
    array_slice_idx: u32,
    array_initialize_idx: u32,
    array_to_indexed_idx: u32,
    json_enc_idx: u32,
    json_escape_idx: u32,
    json_dict_pairs_idx: u32,
    json_write_idx: u32,
    sb_ensure_idx: u32,
    sb_push_byte_idx: u32,
    sb_push_str_idx: u32,
    sb_escape_idx: u32,
    sb_push_int_idx: u32,
    sb_finish_idx: u32,
    json_skipws_idx: u32,
    json_pstr_idx: u32,
    json_pnum_idx: u32,
    json_pval_idx: u32,
    json_parr_idx: u32,
    json_pobj_idx: u32,
    json_parse_idx: u32,
    json_run_idx: u32,
    json_decstr_idx: u32,
    html_esc_text_idx: u32,
    html_esc_attr_idx: u32,
    serialize_html_idx: u32,
    view_html_idx: u32,
    marshal_idx: u32,
    render_dom_idx: u32,
    rerender_idx: u32,
    patch_idx: u32,
    run_cmd_idx: u32,
    str_from_mem_idx: u32,
    render_document_idx: u32,
    dispatch_msg_idx: u32,
    port_in_idx: u32,
    sub_find_port_idx: u32,
    html_map_idx: u32,
    http_response_idx: u32,
    reconcile_subs_idx: u32,
    walk_timers_idx: u32,
    tick_idx: u32,
    dom_event_idx: u32,
    index_byte_idx: u32,
    url_from_string_idx: u32,
    strip_keys_idx: u32,
    keyed_reconcile_idx: u32,
    frame_idx: u32,
    box_int_idx: u32,
    unbox_int_idx: u32,
    msort_idx: u32,
    msort_key_idx: u32,
    msort_cmp_idx: u32,
    list_sort_with_idx: u32,
    random_peel_idx: u32,
    random_next_seed_idx: u32,
    random_initial_seed_idx: u32,
    random_step_idx: u32,
    task_run_idx: u32,
    dict_sorted_build_idx: u32,
    set_sorted_build_idx: u32,
    alm_event_idx: u32,
    /// mangled function name -> arity (parameter count).
    func_arity: HashMap<String, u32>,
    /// Lifted lambdas / local functions: (total arity incl. captures, body).
    /// Their wasm function indices are `lifted_base + i`.
    lifted: Vec<(u32, Function)>,
    lifted_base: u32,
}

impl<'a> Codegen<'a> {
    fn new(mono: &'a MonoProgram) -> Self {
        Codegen {
            mono,
            func_index: HashMap::new(),
            fn_types: HashMap::new(),
            fn_type_order: Vec::new(),
            next_type: N_FIXED,
            string_data: Vec::new(),
            str_offsets: HashMap::new(),
            str_append_idx: 0,
            str_from_int_idx: 0,
            apply1_idx: 0,
            list_cons_idx: 0,
            list_map_idx: 0,
            list_foldl_idx: 0,
            list_length_idx: 0,
            list_append_idx: 0,
            val_eq_idx: 0,
            list_reverse_idx: 0,
            list_filter_idx: 0,
            list_foldr_idx: 0,
            modby_idx: 0,
            list_range_idx: 0,
            list_member_idx: 0,
            list_take_idx: 0,
            list_drop_idx: 0,
            list_concat_idx: 0,
            list_head_idx: 0,
            list_tail_idx: 0,
            maybe_with_default_idx: 0,
            maybe_map_idx: 0,
            maybe_and_then_idx: 0,
            maybe_map2_idx: 0,
            maybe_map3_idx: 0,
            result_map2_idx: 0,
            result_map3_idx: 0,
            str_join_idx: 0,
            str_repeat_idx: 0,
            str_starts_with_idx: 0,
            str_ends_with_idx: 0,
            val_compare_idx: 0,
            list_insert_idx: 0,
            list_sort_idx: 0,
            list_all_idx: 0,
            list_any_idx: 0,
            list_min_idx: 0,
            list_max_idx: 0,
            list_indexed_map_idx: 0,
            list_sum_idx: 0,
            list_product_idx: 0,
            list_map2_idx: 0,
            str_upper_idx: 0,
            str_lower_idx: 0,
            str_trim_idx: 0,
            str_left_idx: 0,
            str_right_idx: 0,
            str_dropleft_idx: 0,
            str_dropright_idx: 0,
            str_to_int_idx: 0,
            str_contains_idx: 0,
            str_to_list_idx: 0,
            str_from_list_idx: 0,
            str_from_char_idx: 0,
            str_uncons_idx: 0,
            str_length_idx: 0,
            clamp_idx: 0,
            result_with_default_idx: 0,
            result_map_idx: 0,
            result_map_error_idx: 0,
            result_and_then_idx: 0,
            result_to_maybe_idx: 0,
            result_from_maybe_idx: 0,
            str_split_idx: 0,
            str_words_idx: 0,
            str_lines_idx: 0,
            list_repeat_idx: 0,
            list_filter_map_idx: 0,
            list_sortby_idx: 0,
            list_sortby_insert_idx: 0,
            str_pad_left_idx: 0,
            str_pad_right_idx: 0,
            from_polar_idx: 0,
            to_polar_idx: 0,
            str_slice_idx: 0,
            str_pad_both_idx: 0,
            list_intersperse_idx: 0,
            list_map3_idx: 0,
            list_map4_idx: 0,
            list_map5_idx: 0,
            list_partition_idx: 0,
            list_unzip_idx: 0,
            dict_get_idx: 0,
            dict_insert_idx: 0,
            dict_remove_idx: 0,
            dict_from_list_idx: 0,
            dict_foldl_idx: 0,
            dict_foldr_idx: 0,
            dict_map_idx: 0,
            dict_filter_idx: 0,
            dict_keys_idx: 0,
            dict_values_idx: 0,
            dict_intersect_idx: 0,
            dict_diff_idx: 0,
            dict_update_idx: 0,
            set_insert_idx: 0,
            set_member_idx: 0,
            set_remove_idx: 0,
            set_from_list_idx: 0,
            set_intersect_idx: 0,
            set_diff_idx: 0,
            val_hash_idx: 0,
            treap_get_idx: 0,
            treap_insert_idx: 0,
            treap_merge_idx: 0,
            treap_remove_idx: 0,
            treap_pairs_idx: 0,
            treap_foldl_idx: 0,
            treap_foldr_idx: 0,
            treap_insert_pairs_idx: 0,
            treap_insert_elems_idx: 0,
            array_get_idx: 0,
            array_set_idx: 0,
            array_push_idx: 0,
            array_tighten_idx: 0,
            list_to_array_idx: 0,
            array_slice_idx: 0,
            array_initialize_idx: 0,
            array_to_indexed_idx: 0,
            json_enc_idx: 0,
            json_escape_idx: 0,
            json_dict_pairs_idx: 0,
            json_write_idx: 0,
            sb_ensure_idx: 0,
            sb_push_byte_idx: 0,
            sb_push_str_idx: 0,
            sb_escape_idx: 0,
            sb_push_int_idx: 0,
            sb_finish_idx: 0,
            json_skipws_idx: 0,
            json_pstr_idx: 0,
            json_pnum_idx: 0,
            json_pval_idx: 0,
            json_parr_idx: 0,
            json_pobj_idx: 0,
            json_parse_idx: 0,
            json_run_idx: 0,
            json_decstr_idx: 0,
            html_esc_text_idx: 0,
            html_esc_attr_idx: 0,
            serialize_html_idx: 0,
            view_html_idx: 0,
            marshal_idx: 0,
            render_dom_idx: 0,
            rerender_idx: 0,
            patch_idx: 0,
            run_cmd_idx: 0,
            str_from_mem_idx: 0,
            render_document_idx: 0,
            dispatch_msg_idx: 0,
            port_in_idx: 0,
            sub_find_port_idx: 0,
            html_map_idx: 0,
            http_response_idx: 0,
            reconcile_subs_idx: 0,
            walk_timers_idx: 0,
            tick_idx: 0,
            dom_event_idx: 0,
            index_byte_idx: 0,
            url_from_string_idx: 0,
            strip_keys_idx: 0,
            keyed_reconcile_idx: 0,
            frame_idx: 0,
            box_int_idx: 0,
            unbox_int_idx: 0,
            msort_idx: 0,
            msort_key_idx: 0,
            msort_cmp_idx: 0,
            list_sort_with_idx: 0,
            random_peel_idx: 0,
            random_next_seed_idx: 0,
            random_initial_seed_idx: 0,
            random_step_idx: 0,
            task_run_idx: 0,
            dict_sorted_build_idx: 0,
            set_sorted_build_idx: 0,
            alm_event_idx: 0,
            ports: HashMap::new(),
            func_arity: HashMap::new(),
            lifted: Vec::new(),
            lifted_base: 0,
        }
    }

    /// Intern a string literal into the data segment, returning (offset, len).
    fn intern_str(&mut self, s: &str) -> (u32, u32) {
        if let Some(&off) = self.str_offsets.get(s) {
            return off;
        }
        let off = (self.string_data.len() as u32, s.len() as u32);
        self.string_data.extend_from_slice(s.as_bytes());
        self.str_offsets.insert(s.to_string(), off);
        off
    }

    fn fn_type(&mut self, arity: u32) -> u32 {
        if let Some(&t) = self.fn_types.get(&arity) {
            return t;
        }
        let t = self.next_type;
        self.next_type += 1;
        self.fn_types.insert(arity, t);
        self.fn_type_order.push(arity);
        t
    }

    fn build(&mut self) -> Result<Vec<u8>, String> {
        for (i, f) in self.mono.functions.iter().enumerate() {
            self.func_index.insert(f.mangled.to_string(), N_IMPORTS + i as u32);
            self.func_arity.insert(f.mangled.to_string(), f.params.len() as u32);
        }
        let n = self.mono.functions.len() as u32;
        // Synthesized helper function indices, after the imports and user funcs
        // (a running counter, so adding a helper needs no manual re-indexing).
        let mut s = N_IMPORTS + n;
        let mut next = || {
            let i = s;
            s += 1;
            i
        };
        self.str_append_idx = next();
        self.str_from_int_idx = next();
        self.apply1_idx = next();
        self.list_cons_idx = next();
        self.list_map_idx = next();
        self.list_foldl_idx = next();
        self.list_length_idx = next();
        self.list_append_idx = next();
        self.val_eq_idx = next();
        self.list_reverse_idx = next();
        self.list_filter_idx = next();
        self.list_foldr_idx = next();
        self.modby_idx = next();
        self.list_range_idx = next();
        self.list_member_idx = next();
        self.list_take_idx = next();
        self.list_drop_idx = next();
        self.list_concat_idx = next();
        self.list_head_idx = next();
        self.list_tail_idx = next();
        self.maybe_with_default_idx = next();
        self.maybe_map_idx = next();
        self.maybe_and_then_idx = next();
        self.maybe_map2_idx = next();
        self.maybe_map3_idx = next();
        self.str_join_idx = next();
        self.str_repeat_idx = next();
        self.str_starts_with_idx = next();
        self.str_ends_with_idx = next();
        self.val_compare_idx = next();
        self.list_insert_idx = next();
        self.list_sort_idx = next();
        self.list_all_idx = next();
        self.list_any_idx = next();
        self.list_min_idx = next();
        self.list_max_idx = next();
        self.list_indexed_map_idx = next();
        self.list_sum_idx = next();
        self.list_product_idx = next();
        self.list_map2_idx = next();
        self.str_upper_idx = next();
        self.str_lower_idx = next();
        self.str_trim_idx = next();
        self.str_left_idx = next();
        self.str_right_idx = next();
        self.str_dropleft_idx = next();
        self.str_dropright_idx = next();
        self.str_to_int_idx = next();
        self.str_contains_idx = next();
        self.str_to_list_idx = next();
        self.str_from_list_idx = next();
        self.str_from_char_idx = next();
        self.str_uncons_idx = next();
        self.str_length_idx = next();
        self.clamp_idx = next();
        self.result_with_default_idx = next();
        self.result_map_idx = next();
        self.result_map_error_idx = next();
        self.result_and_then_idx = next();
        self.result_to_maybe_idx = next();
        self.result_from_maybe_idx = next();
        self.result_map2_idx = next();
        self.result_map3_idx = next();
        self.str_split_idx = next();
        self.str_words_idx = next();
        self.str_lines_idx = next();
        self.list_repeat_idx = next();
        self.list_filter_map_idx = next();
        self.list_sortby_idx = next();
        self.list_sortby_insert_idx = next();
        self.str_pad_left_idx = next();
        self.str_pad_right_idx = next();
        self.from_polar_idx = next();
        self.to_polar_idx = next();
        self.str_slice_idx = next();
        self.str_pad_both_idx = next();
        self.list_intersperse_idx = next();
        self.list_map3_idx = next();
        self.list_map4_idx = next();
        self.list_map5_idx = next();
        self.list_partition_idx = next();
        self.list_unzip_idx = next();
        self.dict_get_idx = next();
        self.dict_insert_idx = next();
        self.dict_remove_idx = next();
        self.dict_from_list_idx = next();
        self.dict_foldl_idx = next();
        self.dict_foldr_idx = next();
        self.dict_map_idx = next();
        self.dict_filter_idx = next();
        self.dict_keys_idx = next();
        self.dict_values_idx = next();
        self.dict_intersect_idx = next();
        self.dict_diff_idx = next();
        self.dict_update_idx = next();
        self.set_insert_idx = next();
        self.set_member_idx = next();
        self.set_remove_idx = next();
        self.set_from_list_idx = next();
        self.set_intersect_idx = next();
        self.set_diff_idx = next();
        self.val_hash_idx = next();
        self.treap_get_idx = next();
        self.treap_insert_idx = next();
        self.treap_merge_idx = next();
        self.treap_remove_idx = next();
        self.treap_pairs_idx = next();
        self.treap_foldl_idx = next();
        self.treap_foldr_idx = next();
        self.treap_insert_pairs_idx = next();
        self.treap_insert_elems_idx = next();
        self.array_get_idx = next();
        self.array_set_idx = next();
        self.array_push_idx = next();
        self.array_tighten_idx = next();
        self.list_to_array_idx = next();
        self.array_slice_idx = next();
        self.array_initialize_idx = next();
        self.array_to_indexed_idx = next();
        self.json_enc_idx = next();
        self.json_escape_idx = next();
        self.json_dict_pairs_idx = next();
        self.json_write_idx = next();
        self.sb_ensure_idx = next();
        self.sb_push_byte_idx = next();
        self.sb_push_str_idx = next();
        self.sb_escape_idx = next();
        self.sb_push_int_idx = next();
        self.sb_finish_idx = next();
        self.json_skipws_idx = next();
        self.json_pstr_idx = next();
        self.json_pnum_idx = next();
        self.json_pval_idx = next();
        self.json_parr_idx = next();
        self.json_pobj_idx = next();
        self.json_parse_idx = next();
        self.json_run_idx = next();
        self.json_decstr_idx = next();
        self.html_esc_text_idx = next();
        self.html_esc_attr_idx = next();
        self.serialize_html_idx = next();
        self.view_html_idx = next();
        self.marshal_idx = next();
        self.render_dom_idx = next();
        self.rerender_idx = next();
        self.patch_idx = next();
        self.run_cmd_idx = next();
        self.str_from_mem_idx = next();
        self.render_document_idx = next();
        self.dispatch_msg_idx = next();
        self.port_in_idx = next();
        self.sub_find_port_idx = next();
        self.html_map_idx = next();
        self.http_response_idx = next();
        self.reconcile_subs_idx = next();
        self.walk_timers_idx = next();
        self.tick_idx = next();
        self.dom_event_idx = next();
        self.index_byte_idx = next();
        self.url_from_string_idx = next();
        self.strip_keys_idx = next();
        self.keyed_reconcile_idx = next();
        self.frame_idx = next();
        self.box_int_idx = next();
        self.unbox_int_idx = next();
        self.msort_idx = next();
        self.msort_key_idx = next();
        self.msort_cmp_idx = next();
        self.list_sort_with_idx = next();
        self.random_peel_idx = next();
        self.random_next_seed_idx = next();
        self.random_initial_seed_idx = next();
        self.random_step_idx = next();
        self.task_run_idx = next();
        self.dict_sorted_build_idx = next();
        self.set_sorted_build_idx = next();
        self.alm_event_idx = next();
        let main_int_idx = next();
        let render_idx = next();
        let render_html_idx = next();
        let alm_browser_start_idx = next();
        // Lifted lambdas / local functions occupy indices after the helpers.
        self.lifted_base = s;

        let main = self
            .mono
            .functions
            .iter()
            .find(|f| f.original.as_str() == "main")
            .ok_or("wasmgc: no `main`")?;
        if !main.params.is_empty() {
            return Err("wasmgc: `main` must be nullary".into());
        }
        // A browser program is `main : Program flags model msg`; anything else
        // (String / Html / Int) is a plain value. This decides which entry
        // points we export — and hence what the DCE pass keeps live.
        let is_browser = matches!(&main.tipe, can::Type::Type(_, n, _) if n.as_str() == "Program");
        let main_idx = self.func_index[main.mangled.as_str()];

        // Emit user bodies (discovers fn-types via calls; interns string
        // literals). The string helpers need fn-types for arity 1 and 2.
        let mut bodies: Vec<Function> = Vec::new();
        for f in &self.mono.functions {
            bodies.push(self.emit_fn(f)?);
        }
        let func_type_idx: Vec<u32> =
            self.mono.functions.iter().map(|f| self.fn_type(f.params.len() as u32)).collect();
        let ft1 = self.fn_type(1); // str_from_int, list_length : eqref -> eqref
        let ft2 = self.fn_type(2); // str_append, apply1, list_map
        let ft3 = self.fn_type(3); // list_foldl
        let ft4 = self.fn_type(4); // list_map3
        let ft5 = self.fn_type(5); // list_map4
        let ft6 = self.fn_type(6); // list_map5
        // Ensure a fn-type exists for every arity the apply-dispatcher handles.
        for a in 1..=MAX_ARITY {
            self.fn_type(a);
        }
        let main_int_ty = self.next_type;
        let render_ty = self.next_type + 1;
        let val_compare_ty = self.next_type + 2; // (eqref, eqref) -> i32
        let json_void_ty = self.next_type + 3; // () -> ()
        let json_ret_ty = self.next_type + 4; // () -> eqref
        // Import + browser-runtime fn types.
        let imp_ii_i = self.next_type + 5; // (i32,i32) -> i32
        let imp_i5_v = self.next_type + 6; // (i32,i32,i32,i32,i32) -> ()
        let imp_ii_v = self.next_type + 7; // (i32,i32) -> ()
        let imp_i4_v = self.next_type + 8; // (i32,i32,i32,i32) -> ()
        let imp_i_v = self.next_type + 9; // (i32) -> ()
        let eqref_to_i32_ty = self.next_type + 10; // eqref -> i32 (marshal, render_dom)
        let alm_event_ty = self.next_type + 11; // (i32,i32,i32) -> i32
        let imp_i3_v = self.next_type + 12; // (i32,i32,i32) -> ()
        let patch_ty = self.next_type + 13; // (i32,eqref,eqref) -> i32
        let eqref_to_void_ty = self.next_type + 14; // eqref -> () (run_cmd)
        let ii_eqref_ty = self.next_type + 15; // (i32,i32) -> eqref (str_from_mem)
        let ee_eqref_ty = self.next_type + 16; // (eqref,eqref) -> eqref (sub_find_port)
        let fi_v_ty = self.next_type + 17; // (f64,i32) -> () (host_set_interval)
        let if_v_ty = self.next_type + 18; // (i32,f64) -> () (alm_tick)
        let e_e_ty = self.next_type + 19; // (eqref) -> eqref (url_from_string)
        let ei_i_ty = self.next_type + 20; // (eqref,i32) -> i32 (index_byte)
        let i_i_ty = self.next_type + 21; // (i32) -> i32 (host_get_url)
        let iff_v_ty = self.next_type + 22; // (i32,f64,f64) -> () (alm_frame)
        let i64_e_ty = self.next_type + 23; // (i64) -> eqref (box_int)
        let e_i64_ty = self.next_type + 24; // (eqref) -> i64 (unbox_int)
        let msort_ty = self.next_type + 25; // (arr, buf, lo, hi) -> () (merge sort)
        let jw_ty = self.next_type + 26; // (ref T_SB, eqref, eqref, eqref) -> () (json_write)
        let sb_i_v_ty = self.next_type + 27; // (ref T_SB, i32) -> () (sb_ensure, sb_push_byte)
        let sb_e_v_ty = self.next_type + 28; // (ref T_SB, eqref) -> () (sb_push_str, sb_escape)
        let sb_ret_ty = self.next_type + 29; // (ref T_SB) -> eqref (sb_finish)
        let f64_f64_ty = self.next_type + 30; // (f64) -> f64 (Math unary)
        let f64f64_f64_ty = self.next_type + 31; // (f64,f64) -> f64 (Math atan2/pow)
        let kr_ty = self.next_type + 32; // (i32,eqref,eqref) -> () (keyed_reconcile)
        self.next_type += 33;

        // Synthesized helper bodies.
        let str_append = self.emit_str_append();
        let str_from_int = self.emit_str_from_int();
        let apply1 = self.emit_apply1();
        let list_cons = self.emit_list_cons();
        let list_map = self.emit_list_map();
        let list_foldl = self.emit_list_foldl();
        let list_length = self.emit_list_length();
        let list_append = self.emit_list_append();
        let val_eq = self.emit_val_eq();
        let list_reverse = self.emit_list_reverse();
        let list_filter = self.emit_list_filter();
        let list_foldr = self.emit_list_foldr();
        let modby = self.emit_modby();
        let list_range = self.emit_list_range();
        let list_member = self.emit_list_member();
        let list_take = self.emit_list_take();
        let list_drop = self.emit_list_drop();
        let list_concat = self.emit_list_concat();
        let list_head = self.emit_list_head(false);
        let list_tail = self.emit_list_head(true);
        let maybe_with_default = self.emit_maybe_with_default();
        let maybe_map = self.emit_maybe_map();
        let maybe_and_then = self.emit_maybe_and_then();
        let maybe_map2 = self.emit_maybe_mapn(2);
        let maybe_map3 = self.emit_maybe_mapn(3);
        let str_join = self.emit_str_join();
        let str_repeat = self.emit_str_repeat();
        let str_starts_with = self.emit_str_affix(false);
        let str_ends_with = self.emit_str_affix(true);
        let val_compare = self.emit_val_compare();
        let list_insert = self.emit_list_insert();
        let list_sort = self.emit_list_sort();
        let list_all = self.emit_list_all_any(true);
        let list_any = self.emit_list_all_any(false);
        let list_min = self.emit_list_min_max(false);
        let list_max = self.emit_list_min_max(true);
        let list_indexed_map = self.emit_list_indexed_map();
        let list_sum = self.emit_list_sum_prod(false);
        let list_product = self.emit_list_sum_prod(true);
        let list_map2 = self.emit_list_map2();
        let str_upper = self.emit_str_case(true);
        let str_lower = self.emit_str_case(false);
        let str_trim = self.emit_str_trim();
        let str_left = self.emit_str_range(false, false);
        let str_right = self.emit_str_range(true, false);
        let str_dropleft = self.emit_str_range(true, true);
        let str_dropright = self.emit_str_range(false, true);
        let str_to_int = self.emit_str_to_int();
        let str_contains = self.emit_str_contains();
        let str_to_list = self.emit_str_to_list();
        let str_from_list = self.emit_str_from_list();
        let str_from_char = self.emit_str_from_char();
        let str_uncons = self.emit_str_uncons();
        let str_length = self.emit_str_length();
        let clamp = self.emit_clamp();
        let result_with_default = self.emit_result_with_default();
        let result_map = self.emit_result_map(false);
        let result_map_error = self.emit_result_map(true);
        let result_and_then = self.emit_result_and_then();
        let result_to_maybe = self.emit_result_to_maybe();
        let result_from_maybe = self.emit_result_from_maybe();
        let result_map2 = self.emit_result_mapn(2);
        let result_map3 = self.emit_result_mapn(3);
        let str_split = self.emit_str_split();
        let str_words = self.emit_str_words();
        let str_lines = self.emit_str_lines();
        let list_repeat = self.emit_list_repeat();
        let list_filter_map = self.emit_list_filter_map();
        let list_sortby = self.emit_list_sortby();
        let list_sortby_insert = self.emit_list_sortby_insert();
        let str_pad_left = self.emit_str_pad(true);
        let str_pad_right = self.emit_str_pad(false);
        let from_polar = self.emit_from_polar();
        let to_polar = self.emit_to_polar();
        let str_slice = self.emit_str_slice();
        let str_pad_both = self.emit_str_pad_both();
        let list_intersperse = self.emit_list_intersperse();
        let list_map3 = self.emit_list_map3();
        let list_map4 = self.emit_list_mapn(4, self.list_map4_idx);
        let list_map5 = self.emit_list_mapn(5, self.list_map5_idx);
        let list_partition = self.emit_list_partition();
        let list_unzip = self.emit_list_unzip();
        let dict_get = self.emit_dict_get();
        let dict_insert = self.emit_dict_insert();
        let dict_remove = self.emit_dict_remove();
        let dict_from_list = self.emit_dict_from_list();
        let dict_foldl = self.emit_dict_fold(false);
        let dict_foldr = self.emit_dict_fold(true);
        let dict_map = self.emit_dict_map();
        let dict_filter = self.emit_dict_filter();
        let dict_keys = self.emit_dict_project(0);
        let dict_values = self.emit_dict_project(1);
        let dict_intersect = self.emit_dict_intersect();
        let dict_diff = self.emit_dict_diff();
        let dict_update = self.emit_dict_update();
        let set_insert = self.emit_set_insert();
        let set_member = self.emit_set_member();
        let set_remove = self.emit_set_remove();
        let set_from_list = self.emit_set_from_list();
        let set_intersect = self.emit_set_intersect();
        let set_diff = self.emit_set_diff();
        let val_hash = self.emit_val_hash();
        let treap_get = self.emit_treap_get();
        let treap_insert = self.emit_treap_insert();
        let treap_merge = self.emit_treap_merge();
        let treap_remove = self.emit_treap_remove();
        let treap_pairs = self.emit_treap_pairs();
        let treap_foldl = self.emit_treap_fold(false);
        let treap_foldr = self.emit_treap_fold(true);
        let treap_insert_pairs = self.emit_treap_insert_seq(false);
        let treap_insert_elems = self.emit_treap_insert_seq(true);
        let array_get = self.emit_array_get();
        let array_set = self.emit_array_set();
        let array_push = self.emit_array_push();
        let array_tighten = self.emit_array_tighten();
        let list_to_array = self.emit_list_to_array();
        let array_slice = self.emit_array_slice();
        let array_initialize = self.emit_array_initialize();
        let array_to_indexed = self.emit_array_to_indexed();
        let json_enc = self.emit_json_enc();
        let json_escape = self.emit_json_escape();
        let json_dict_pairs = self.emit_json_dict_pairs();
        let json_write = self.emit_json_write();
        let sb_ensure = self.emit_sb_ensure();
        let sb_push_byte = self.emit_sb_push_byte();
        let sb_push_str = self.emit_sb_push_str();
        let sb_escape = self.emit_sb_escape();
        let sb_push_int = self.emit_sb_push_int();
        let sb_finish = self.emit_sb_finish();
        let json_skipws = self.emit_json_skipws();
        let json_pstr = self.emit_json_pstr();
        let json_pnum = self.emit_json_pnum();
        let json_pval = self.emit_json_pval();
        let json_parr = self.emit_json_parr();
        let json_pobj = self.emit_json_pobj();
        let json_parse = self.emit_json_parse();
        let json_run = self.emit_json_run();
        let json_decstr = self.emit_json_decstr();
        let html_esc_text = self.emit_html_escape(false);
        let html_esc_attr = self.emit_html_escape(true);
        let serialize_html = self.emit_serialize_html();
        let view_html = self.emit_view_html(main_idx);
        let render_html = self.emit_render(self.view_html_idx);
        let marshal = self.emit_marshal();
        let render_dom = self.emit_render_dom();
        let rerender = self.emit_rerender();
        let patch = self.emit_patch();
        let run_cmd = self.emit_run_cmd();
        let str_from_mem = self.emit_str_from_mem();
        let render_document = self.emit_render_document();
        let dispatch_msg = self.emit_dispatch_msg();
        let port_in = self.emit_port_in();
        let sub_find_port = self.emit_sub_find_port();
        let html_map = self.emit_html_map();
        let http_response = self.emit_http_response();
        let reconcile_subs = self.emit_reconcile_subs();
        let walk_timers = self.emit_walk_timers();
        let tick = self.emit_tick();
        let dom_event = self.emit_dom_event();
        let index_byte = self.emit_index_byte();
        let url_from_string = self.emit_url_from_string();
        let strip_keys = self.emit_strip_keys();
        let keyed_reconcile = self.emit_keyed_reconcile();
        let frame = self.emit_frame();
        let box_int = self.emit_box_int();
        let unbox_int = self.emit_unbox_int();
        let msort = self.emit_msort_rec(SortMode::Value);
        let msort_key = self.emit_msort_rec(SortMode::ByKey);
        let msort_cmp = self.emit_msort_rec(SortMode::Cmp);
        let list_sort_with = self.emit_list_sort_with();
        let random_peel = self.emit_random_peel();
        let random_next_seed = self.emit_random_next_seed();
        let random_initial_seed = self.emit_random_initial_seed();
        let random_step = self.emit_random_step();
        let task_run = self.emit_task_run();
        let dict_sorted_build = self.emit_sorted_build(true);
        let set_sorted_build = self.emit_sorted_build(false);
        let alm_event = self.emit_alm_event();
        let alm_browser_start = self.emit_alm_browser_start(main_idx);
        let mut mi = Function::new([]);
        mi.instruction(&Instruction::Call(main_idx));
        mi.instruction(&Instruction::Call(self.unbox_int_idx));
        mi.instruction(&Instruction::End);
        let render = self.emit_render(main_idx);

        // Type section: fixed types, function types, then helper types.
        let mut types = TypeSection::new();
        struct_type(&mut types, &[FieldType { element_type: StorageType::Val(ValType::I64), mutable: false }]); // T_INT
        struct_type(&mut types, &[FieldType { element_type: StorageType::Val(ValType::F64), mutable: false }]); // T_FLOAT
        types.ty().array(&StorageType::I8, true); // T_STR
        types.ty().array(&StorageType::Val(eqref()), true); // T_ARR
        struct_type(&mut types, &[
            FieldType { element_type: StorageType::Val(ValType::I32), mutable: true },
            FieldType { element_type: StorageType::Val(ref_to(T_ARR)), mutable: false },
        ]); // T_BACK { mut i32 head, (ref T_ARR) data }
        struct_type(&mut types, &[
            FieldType { element_type: StorageType::Val(ValType::I32), mutable: false },
            FieldType {
                element_type: StorageType::Val(ValType::Ref(RefType {
                    nullable: true,
                    heap_type: HeapType::Concrete(T_BACK),
                })),
                mutable: false,
            },
        ]); // T_LIST { i32 len, (ref null T_BACK) bk }
        struct_type(&mut types, &[
            FieldType { element_type: StorageType::Val(ValType::I32), mutable: false },
            FieldType { element_type: StorageType::Val(ref_to(T_ARR)), mutable: false },
        ]); // T_CTOR
        struct_type(&mut types, &[
            FieldType { element_type: StorageType::Val(funcref()), mutable: false },
            FieldType { element_type: StorageType::Val(ValType::I32), mutable: false },
            FieldType { element_type: StorageType::Val(ValType::I32), mutable: false },
            FieldType { element_type: StorageType::Val(ref_to(T_ARR)), mutable: false },
        ]); // T_CLOS
        struct_type(&mut types, &[
            FieldType { element_type: StorageType::Val(eqref()), mutable: false },
            FieldType { element_type: StorageType::Val(eqref()), mutable: false },
            FieldType { element_type: StorageType::Val(ValType::I32), mutable: false },
            FieldType { element_type: StorageType::Val(ref_null_to(T_TNODE)), mutable: false },
            FieldType { element_type: StorageType::Val(ref_null_to(T_TNODE)), mutable: false },
        ]); // T_TNODE { key, value, pri, left, right }
        struct_type(&mut types, &[
            FieldType { element_type: StorageType::Val(ref_to(T_STR)), mutable: true },
            FieldType { element_type: StorageType::Val(ValType::I32), mutable: true },
        ]); // T_SB { mut buf: (ref T_STR), mut len }
        for &arity in &self.fn_type_order {
            types.ty().function(vec![eqref(); arity as usize], vec![eqref()]);
        }
        types.ty().function(vec![], vec![ValType::I64]); // main_int
        types.ty().function(vec![], vec![ValType::I32]); // render
        types.ty().function(vec![eqref(), eqref()], vec![ValType::I32]); // val_compare
        types.ty().function(vec![], vec![]); // json_void: () -> ()
        types.ty().function(vec![], vec![eqref()]); // json_ret: () -> eqref
        types.ty().function(vec![ValType::I32, ValType::I32], vec![ValType::I32]); // imp_ii_i
        types.ty().function(
            vec![ValType::I32, ValType::I32, ValType::I32, ValType::I32, ValType::I32],
            vec![],
        ); // imp_i5_v
        types.ty().function(vec![ValType::I32, ValType::I32], vec![]); // imp_ii_v
        types.ty().function(
            vec![ValType::I32, ValType::I32, ValType::I32, ValType::I32],
            vec![],
        ); // imp_i4_v
        types.ty().function(vec![ValType::I32], vec![]); // imp_i_v
        types.ty().function(vec![eqref()], vec![ValType::I32]); // eqref_to_i32
        types.ty().function(
            vec![ValType::I32, ValType::I32, ValType::I32],
            vec![ValType::I32],
        ); // alm_event
        types.ty().function(vec![ValType::I32, ValType::I32, ValType::I32], vec![]); // imp_i3_v
        types.ty().function(vec![ValType::I32, eqref(), eqref()], vec![ValType::I32]); // patch
        types.ty().function(vec![eqref()], vec![]); // run_cmd: eqref -> ()
        types.ty().function(vec![ValType::I32, ValType::I32], vec![eqref()]); // str_from_mem
        types.ty().function(vec![eqref(), eqref()], vec![eqref()]); // sub_find_port
        types.ty().function(vec![ValType::F64, ValType::I32], vec![]); // host_set_interval
        types.ty().function(vec![ValType::I32, ValType::F64], vec![]); // alm_tick
        types.ty().function(vec![eqref()], vec![eqref()]); // url_from_string
        types.ty().function(vec![eqref(), ValType::I32], vec![ValType::I32]); // index_byte
        types.ty().function(vec![ValType::I32], vec![ValType::I32]); // host_get_url
        types.ty().function(vec![ValType::I32, ValType::F64, ValType::F64], vec![]); // alm_frame
        types.ty().function(vec![ValType::I64], vec![eqref()]); // box_int
        types.ty().function(vec![eqref()], vec![ValType::I64]); // unbox_int
        types.ty().function(
            vec![ref_null_to(T_ARR), ref_null_to(T_ARR), ValType::I32, ValType::I32],
            vec![],
        ); // msort_rec
        types.ty().function(vec![ref_to(T_SB), eqref(), eqref(), eqref()], vec![]); // json_write
        types.ty().function(vec![ref_to(T_SB), ValType::I32], vec![]); // sb_i_v
        types.ty().function(vec![ref_to(T_SB), eqref()], vec![]); // sb_e_v
        types.ty().function(vec![ref_to(T_SB)], vec![eqref()]); // sb_ret
        types.ty().function(vec![ValType::F64], vec![ValType::F64]); // f64_f64 (Math unary)
        types.ty().function(vec![ValType::F64, ValType::F64], vec![ValType::F64]); // f64f64_f64
        types.ty().function(vec![ValType::I32, eqref(), eqref()], vec![]); // keyed_reconcile

        // Function section: user funcs, str_append, str_from_int, main_int, render.
        let mut funcs = FunctionSection::new();
        for &t in &func_type_idx {
            funcs.function(t);
        }
        funcs.function(ft2); // str_append
        funcs.function(ft1); // str_from_int
        funcs.function(ft2); // apply1 : (clos, arg) -> eqref
        funcs.function(ft2); // list_cons : (x, xs) -> list
        funcs.function(ft2); // list_map
        funcs.function(ft3); // list_foldl
        funcs.function(ft1); // list_length
        funcs.function(ft2); // list_append
        funcs.function(ft2); // val_eq
        funcs.function(ft1); // list_reverse
        funcs.function(ft2); // list_filter
        funcs.function(ft3); // list_foldr
        funcs.function(ft2); // modby
        funcs.function(ft2); // list_range
        funcs.function(ft2); // list_member
        funcs.function(ft2); // list_take
        funcs.function(ft2); // list_drop
        funcs.function(ft1); // list_concat
        funcs.function(ft1); // list_head
        funcs.function(ft1); // list_tail
        funcs.function(ft2); // maybe_with_default
        funcs.function(ft2); // maybe_map
        funcs.function(ft2); // maybe_and_then
        funcs.function(ft3); // maybe_map2
        funcs.function(ft4); // maybe_map3
        funcs.function(ft2); // str_join
        funcs.function(ft2); // str_repeat
        funcs.function(ft2); // str_starts_with
        funcs.function(ft2); // str_ends_with
        funcs.function(val_compare_ty); // val_compare
        funcs.function(ft2); // list_insert
        funcs.function(ft1); // list_sort
        funcs.function(ft2); // list_all
        funcs.function(ft2); // list_any
        funcs.function(ft1); // list_min
        funcs.function(ft1); // list_max
        funcs.function(ft3); // list_indexed_map
        funcs.function(ft1); // list_sum
        funcs.function(ft1); // list_product
        funcs.function(ft3); // list_map2
        funcs.function(ft1); // str_upper
        funcs.function(ft1); // str_lower
        funcs.function(ft1); // str_trim
        funcs.function(ft2); // str_left
        funcs.function(ft2); // str_right
        funcs.function(ft2); // str_dropleft
        funcs.function(ft2); // str_dropright
        funcs.function(ft1); // str_to_int
        funcs.function(ft2); // str_contains
        funcs.function(ft1); // str_to_list
        funcs.function(ft1); // str_from_list
        funcs.function(ft1); // str_from_char
        funcs.function(ft1); // str_uncons
        funcs.function(ft1); // str_length
        funcs.function(ft3); // clamp
        funcs.function(ft2); // result_with_default
        funcs.function(ft2); // result_map
        funcs.function(ft2); // result_map_error
        funcs.function(ft2); // result_and_then
        funcs.function(ft1); // result_to_maybe
        funcs.function(ft2); // result_from_maybe
        funcs.function(ft3); // result_map2
        funcs.function(ft4); // result_map3
        funcs.function(ft2); // str_split
        funcs.function(ft1); // str_words
        funcs.function(ft1); // str_lines
        funcs.function(ft2); // list_repeat
        funcs.function(ft2); // list_filter_map
        funcs.function(ft2); // list_sortby
        funcs.function(ft3); // list_sortby_insert
        funcs.function(ft3); // str_pad_left
        funcs.function(ft3); // str_pad_right
        funcs.function(ft1); // from_polar
        funcs.function(ft1); // to_polar
        funcs.function(ft3); // str_slice
        funcs.function(ft3); // str_pad_both
        funcs.function(ft2); // list_intersperse
        funcs.function(ft4); // list_map3
        funcs.function(ft5); // list_map4
        funcs.function(ft6); // list_map5
        funcs.function(ft2); // list_partition
        funcs.function(ft1); // list_unzip
        funcs.function(ft2); // dict_get
        funcs.function(ft3); // dict_insert
        funcs.function(ft2); // dict_remove
        funcs.function(ft2); // dict_from_list (pairs, acc)
        funcs.function(ft3); // dict_foldl
        funcs.function(ft3); // dict_foldr
        funcs.function(ft2); // dict_map
        funcs.function(ft2); // dict_filter
        funcs.function(ft1); // dict_keys
        funcs.function(ft1); // dict_values
        funcs.function(ft2); // dict_intersect
        funcs.function(ft2); // dict_diff (toRemove, acc)
        funcs.function(ft3); // dict_update
        funcs.function(ft2); // set_insert
        funcs.function(ft2); // set_member
        funcs.function(ft2); // set_remove
        funcs.function(ft2); // set_from_list (xs, acc)
        funcs.function(ft2); // set_intersect
        funcs.function(ft2); // set_diff (toRemove, base)
        funcs.function(eqref_to_i32_ty); // val_hash
        funcs.function(ft2); // treap_get (k, t)
        funcs.function(ft3); // treap_insert (k, v, t)
        funcs.function(ft2); // treap_merge (l, r)
        funcs.function(ft2); // treap_remove (k, t)
        funcs.function(ft2); // treap_pairs (t, acc)
        funcs.function(ft3); // treap_foldl (f, acc, t)
        funcs.function(ft3); // treap_foldr (f, acc, t)
        funcs.function(ft2); // treap_insert_pairs (pairs, t)
        funcs.function(ft2); // treap_insert_elems (elems, t)
        funcs.function(ft2); // array_get
        funcs.function(ft3); // array_set
        funcs.function(ft2); // array_push
        funcs.function(ft1); // array_tighten
        funcs.function(ft1); // list_to_array
        funcs.function(ft3); // array_slice
        funcs.function(ft2); // array_initialize
        funcs.function(ft1); // array_to_indexed
        funcs.function(ft3); // json_enc (value, gap, prefix)
        funcs.function(ft1); // json_escape
        funcs.function(ft3); // json_dict_pairs
        funcs.function(jw_ty); // json_write
        funcs.function(sb_i_v_ty); // sb_ensure
        funcs.function(sb_i_v_ty); // sb_push_byte
        funcs.function(sb_e_v_ty); // sb_push_str
        funcs.function(sb_e_v_ty); // sb_escape
        funcs.function(sb_e_v_ty); // sb_push_int
        funcs.function(sb_ret_ty); // sb_finish
        funcs.function(json_void_ty); // json_skipws : () -> ()
        funcs.function(json_ret_ty); // json_pstr : () -> eqref
        funcs.function(json_ret_ty); // json_pnum : () -> eqref
        funcs.function(json_ret_ty); // json_pval : () -> eqref
        funcs.function(json_ret_ty); // json_parr : () -> eqref
        funcs.function(json_ret_ty); // json_pobj : () -> eqref
        funcs.function(ft1); // json_parse : String -> Result
        funcs.function(ft2); // json_run : (decoder, value) -> Result
        funcs.function(ft2); // json_decstr : (decoder, string) -> Result
        funcs.function(ft1); // html_esc_text
        funcs.function(ft1); // html_esc_attr
        funcs.function(ft1); // serialize_html
        funcs.function(json_ret_ty); // view_html : () -> eqref
        funcs.function(eqref_to_i32_ty); // marshal
        funcs.function(eqref_to_i32_ty); // render_dom
        funcs.function(json_void_ty); // rerender
        funcs.function(patch_ty); // patch
        funcs.function(eqref_to_void_ty); // run_cmd
        funcs.function(ii_eqref_ty); // str_from_mem
        funcs.function(json_ret_ty); // doc_vnode : () -> eqref
        funcs.function(eqref_to_void_ty); // dispatch_msg : eqref -> ()
        funcs.function(imp_i4_v); // alm_port_in : (i32,i32,i32,i32) -> ()
        funcs.function(ee_eqref_ty); // sub_find_port : (eqref,eqref) -> eqref
        funcs.function(ee_eqref_ty); // html_map : (eqref,eqref) -> eqref
        funcs.function(imp_i4_v); // alm_http_response : (reqId,status,ptr,len) -> ()
        funcs.function(json_void_ty); // reconcile_subs : () -> ()
        funcs.function(eqref_to_void_ty); // walk_timers : eqref -> ()
        funcs.function(if_v_ty); // alm_tick : (slot, millis:f64) -> ()
        funcs.function(imp_i3_v); // alm_dom_event : (slot, ptr, len) -> ()
        funcs.function(ei_i_ty); // index_byte : (str, ch) -> i32
        funcs.function(e_e_ty); // url_from_string : String -> Maybe Url
        funcs.function(e_e_ty); // strip_keys : List (k, Html) -> List Html
        funcs.function(kr_ty); // keyed_reconcile (dom, oldKids, newKids)
        funcs.function(iff_v_ty); // alm_frame : (slot, delta, now) -> ()
        funcs.function(i64_e_ty); // box_int : (i64) -> eqref
        funcs.function(e_i64_ty); // unbox_int : (eqref) -> i64
        funcs.function(msort_ty); // msort (by element)
        funcs.function(msort_ty); // msort_key (by pair[0])
        funcs.function(msort_ty); // msort_cmp (by user comparator)
        funcs.function(ft2); // list_sort_with (cmp, list)
        funcs.function(e_i64_ty); // random_peel (seed) -> i64
        funcs.function(ft1); // random_next_seed (seed) -> seed
        funcs.function(ft1); // random_initial_seed (x) -> seed
        funcs.function(ft2); // random_step (gen, seed) -> (value, seed)
        funcs.function(ft1); // task_run (task) -> Result
        funcs.function(e_e_ty); // dict_sorted_build : pairs -> Dict
        funcs.function(e_e_ty); // set_sorted_build : elems -> Set
        funcs.function(alm_event_ty); // alm_event
        funcs.function(main_int_ty);
        funcs.function(render_ty); // render
        funcs.function(render_ty); // render_html
        funcs.function(json_void_ty); // alm_browser_start
        let lifted_types: Vec<u32> =
            self.lifted.iter().map(|(a, _)| self.fn_types[a]).collect();
        for &t in &lifted_types {
            funcs.function(t);
        }

        let mut code = CodeSection::new();
        for b in &bodies {
            code.function(b);
        }
        code.function(&str_append);
        code.function(&str_from_int);
        code.function(&apply1);
        code.function(&list_cons);
        code.function(&list_map);
        code.function(&list_foldl);
        code.function(&list_length);
        code.function(&list_append);
        code.function(&val_eq);
        code.function(&list_reverse);
        code.function(&list_filter);
        code.function(&list_foldr);
        code.function(&modby);
        code.function(&list_range);
        code.function(&list_member);
        code.function(&list_take);
        code.function(&list_drop);
        code.function(&list_concat);
        code.function(&list_head);
        code.function(&list_tail);
        code.function(&maybe_with_default);
        code.function(&maybe_map);
        code.function(&maybe_and_then);
        code.function(&maybe_map2);
        code.function(&maybe_map3);
        code.function(&str_join);
        code.function(&str_repeat);
        code.function(&str_starts_with);
        code.function(&str_ends_with);
        code.function(&val_compare);
        code.function(&list_insert);
        code.function(&list_sort);
        code.function(&list_all);
        code.function(&list_any);
        code.function(&list_min);
        code.function(&list_max);
        code.function(&list_indexed_map);
        code.function(&list_sum);
        code.function(&list_product);
        code.function(&list_map2);
        code.function(&str_upper);
        code.function(&str_lower);
        code.function(&str_trim);
        code.function(&str_left);
        code.function(&str_right);
        code.function(&str_dropleft);
        code.function(&str_dropright);
        code.function(&str_to_int);
        code.function(&str_contains);
        code.function(&str_to_list);
        code.function(&str_from_list);
        code.function(&str_from_char);
        code.function(&str_uncons);
        code.function(&str_length);
        code.function(&clamp);
        code.function(&result_with_default);
        code.function(&result_map);
        code.function(&result_map_error);
        code.function(&result_and_then);
        code.function(&result_to_maybe);
        code.function(&result_from_maybe);
        code.function(&result_map2);
        code.function(&result_map3);
        code.function(&str_split);
        code.function(&str_words);
        code.function(&str_lines);
        code.function(&list_repeat);
        code.function(&list_filter_map);
        code.function(&list_sortby);
        code.function(&list_sortby_insert);
        code.function(&str_pad_left);
        code.function(&str_pad_right);
        code.function(&from_polar);
        code.function(&to_polar);
        code.function(&str_slice);
        code.function(&str_pad_both);
        code.function(&list_intersperse);
        code.function(&list_map3);
        code.function(&list_map4);
        code.function(&list_map5);
        code.function(&list_partition);
        code.function(&list_unzip);
        code.function(&dict_get);
        code.function(&dict_insert);
        code.function(&dict_remove);
        code.function(&dict_from_list);
        code.function(&dict_foldl);
        code.function(&dict_foldr);
        code.function(&dict_map);
        code.function(&dict_filter);
        code.function(&dict_keys);
        code.function(&dict_values);
        code.function(&dict_intersect);
        code.function(&dict_diff);
        code.function(&dict_update);
        code.function(&set_insert);
        code.function(&set_member);
        code.function(&set_remove);
        code.function(&set_from_list);
        code.function(&set_intersect);
        code.function(&set_diff);
        code.function(&val_hash);
        code.function(&treap_get);
        code.function(&treap_insert);
        code.function(&treap_merge);
        code.function(&treap_remove);
        code.function(&treap_pairs);
        code.function(&treap_foldl);
        code.function(&treap_foldr);
        code.function(&treap_insert_pairs);
        code.function(&treap_insert_elems);
        code.function(&array_get);
        code.function(&array_set);
        code.function(&array_push);
        code.function(&array_tighten);
        code.function(&list_to_array);
        code.function(&array_slice);
        code.function(&array_initialize);
        code.function(&array_to_indexed);
        code.function(&json_enc);
        code.function(&json_escape);
        code.function(&json_dict_pairs);
        code.function(&json_write);
        code.function(&sb_ensure);
        code.function(&sb_push_byte);
        code.function(&sb_push_str);
        code.function(&sb_escape);
        code.function(&sb_push_int);
        code.function(&sb_finish);
        code.function(&json_skipws);
        code.function(&json_pstr);
        code.function(&json_pnum);
        code.function(&json_pval);
        code.function(&json_parr);
        code.function(&json_pobj);
        code.function(&json_parse);
        code.function(&json_run);
        code.function(&json_decstr);
        code.function(&html_esc_text);
        code.function(&html_esc_attr);
        code.function(&serialize_html);
        code.function(&view_html);
        code.function(&marshal);
        code.function(&render_dom);
        code.function(&rerender);
        code.function(&patch);
        code.function(&run_cmd);
        code.function(&str_from_mem);
        code.function(&render_document);
        code.function(&dispatch_msg);
        code.function(&port_in);
        code.function(&sub_find_port);
        code.function(&html_map);
        code.function(&http_response);
        code.function(&reconcile_subs);
        code.function(&walk_timers);
        code.function(&tick);
        code.function(&dom_event);
        code.function(&index_byte);
        code.function(&url_from_string);
        code.function(&strip_keys);
        code.function(&keyed_reconcile);
        code.function(&frame);
        code.function(&box_int);
        code.function(&unbox_int);
        code.function(&msort);
        code.function(&msort_key);
        code.function(&msort_cmp);
        code.function(&list_sort_with);
        code.function(&random_peel);
        code.function(&random_next_seed);
        code.function(&random_initial_seed);
        code.function(&random_step);
        code.function(&task_run);
        code.function(&dict_sorted_build);
        code.function(&set_sorted_build);
        code.function(&alm_event);
        code.function(&mi);
        code.function(&render);
        code.function(&render_html);
        code.function(&alm_browser_start);
        for (_, body) in &self.lifted {
            code.function(body);
        }

        // Declare every function so `ref.func` on it is valid.
        let mut elems = ElementSection::new();
        let total_funcs = self.lifted_base + self.lifted.len() as u32;
        let idxs: Vec<u32> = (0..total_funcs).collect();
        elems.declared(Elements::Functions(std::borrow::Cow::Borrowed(&idxs)));

        // 1 page of linear memory for string marshalling at the JS boundary.
        let mut mems = MemorySection::new();
        mems.memory(MemoryType {
            minimum: 16,
            maximum: None,
            memory64: false,
            shared: false,
            page_size_log2: None,
        });

        let mut data = DataSection::new();
        data.passive(self.string_data.iter().copied());

        // Globals for the JSON parser cursor (see emit_json_parse): the input
        // string, the byte position, and an error flag.
        let mut globals = GlobalSection::new();
        globals.global(
            GlobalType { val_type: ref_null_to(T_STR), mutable: true, shared: false },
            &ConstExpr::ref_null(HeapType::Concrete(T_STR)),
        );
        globals.global(
            GlobalType { val_type: ValType::I32, mutable: true, shared: false },
            &ConstExpr::i32_const(0),
        );
        globals.global(
            GlobalType { val_type: ValType::I32, mutable: true, shared: false },
            &ConstExpr::i32_const(0),
        );
        // 3=model, 4=update, 5=view (eqref, null until a program starts).
        for _ in 0..3 {
            globals.global(
                GlobalType { val_type: eqref(), mutable: true, shared: false },
                &ConstExpr::ref_null(eq_heap()),
            );
        }
        // 6=handlers (ref null T_ARR).
        globals.global(
            GlobalType { val_type: ref_null_to(T_ARR), mutable: true, shared: false },
            &ConstExpr::ref_null(HeapType::Concrete(T_ARR)),
        );
        // 7=next handler id, 8=mem bump pointer.
        globals.global(
            GlobalType { val_type: ValType::I32, mutable: true, shared: false },
            &ConstExpr::i32_const(0),
        );
        globals.global(
            GlobalType { val_type: ValType::I32, mutable: true, shared: false },
            &ConstExpr::i32_const(BUMP_BASE),
        );
        // 9=prev vdom (eqref), 10=root handle (i32), 11=program kind (i32).
        globals.global(
            GlobalType { val_type: eqref(), mutable: true, shared: false },
            &ConstExpr::ref_null(eq_heap()),
        );
        globals.global(
            GlobalType { val_type: ValType::I32, mutable: true, shared: false },
            &ConstExpr::i32_const(0),
        );
        globals.global(
            GlobalType { val_type: ValType::I32, mutable: true, shared: false },
            &ConstExpr::i32_const(0),
        );
        // 12=subscriptions fn (eqref, null for sandbox).
        globals.global(
            GlobalType { val_type: eqref(), mutable: true, shared: false },
            &ConstExpr::ref_null(eq_heap()),
        );
        // 13=in-flight HTTP expects (ref null T_ARR), 14=next request id (i32).
        globals.global(
            GlobalType { val_type: ref_null_to(T_ARR), mutable: true, shared: false },
            &ConstExpr::ref_null(HeapType::Concrete(T_ARR)),
        );
        globals.global(
            GlobalType { val_type: ValType::I32, mutable: true, shared: false },
            &ConstExpr::i32_const(0),
        );
        // 15=active timer toMsg callbacks (ref null T_ARR), 16=next slot (i32).
        globals.global(
            GlobalType { val_type: ref_null_to(T_ARR), mutable: true, shared: false },
            &ConstExpr::ref_null(HeapType::Concrete(T_ARR)),
        );
        globals.global(
            GlobalType { val_type: ValType::I32, mutable: true, shared: false },
            &ConstExpr::i32_const(0),
        );
        // 17=active document-event decoders (ref null T_ARR), 18=next slot (i32).
        globals.global(
            GlobalType { val_type: ref_null_to(T_ARR), mutable: true, shared: false },
            &ConstExpr::ref_null(HeapType::Concrete(T_ARR)),
        );
        globals.global(
            GlobalType { val_type: ValType::I32, mutable: true, shared: false },
            &ConstExpr::i32_const(0),
        );
        // 19=onUrlChange handler (eqref, null unless Browser.application).
        globals.global(
            GlobalType { val_type: eqref(), mutable: true, shared: false },
            &ConstExpr::ref_null(eq_heap()),
        );
        // 20=active animation-frame subs (ref null T_ARR), 21=next slot (i32).
        globals.global(
            GlobalType { val_type: ref_null_to(T_ARR), mutable: true, shared: false },
            &ConstExpr::ref_null(HeapType::Concrete(T_ARR)),
        );
        globals.global(
            GlobalType { val_type: ValType::I32, mutable: true, shared: false },
            &ConstExpr::i32_const(0),
        );
        // 22=List.sortWith comparator (eqref, null except during a sortWith).
        globals.global(
            GlobalType { val_type: eqref(), mutable: true, shared: false },
            &ConstExpr::ref_null(eq_heap()),
        );

        // DOM host imports (function indices 0..N_IMPORTS).
        let mut imports = ImportSection::new();
        for (name, ty) in [
            ("dom_create_element", imp_ii_i),
            ("dom_create_text", imp_ii_i),
            ("dom_set_attribute", imp_i5_v),
            ("dom_set_style", imp_i5_v),
            ("dom_append_child", imp_ii_v),
            ("dom_add_event_listener", imp_i4_v),
            ("dom_mount", imp_i_v),
            ("dom_replace_root", imp_i_v),
            ("dom_child", imp_ii_i),
            ("dom_set_text", imp_i3_v),
            ("dom_remove_attribute", imp_i3_v),
            ("dom_remove_child", imp_ii_v),
            ("dom_replace", imp_ii_v),
            ("dom_remove_event_listener", imp_i4_v),
            ("host_port_out", imp_i4_v),
            ("host_set_title", imp_ii_v),
            ("host_http", imp_i3_v),
            ("host_clear_timers", json_void_ty),
            ("host_set_interval", fi_v_ty),
            ("host_clear_dom", json_void_ty),
            ("host_add_dom", imp_i3_v),
            ("host_push_url", imp_i3_v),
            ("host_get_url", i_i_ty),
            ("host_load", imp_ii_v),
            ("host_clear_frames", json_void_ty),
            ("host_request_frame", imp_i_v),
            // Math.* (indices MATH_SIN..MATH_POW) — the transcendentals.
            ("math_sin", f64_f64_ty),
            ("math_cos", f64_f64_ty),
            ("math_tan", f64_f64_ty),
            ("math_asin", f64_f64_ty),
            ("math_acos", f64_f64_ty),
            ("math_atan", f64_f64_ty),
            ("math_log", f64_f64_ty),
            ("math_atan2", f64f64_f64_ty),
            ("math_pow", f64f64_f64_ty),
        ] {
            imports.import("env", name, EntityType::Function(ty));
        }

        // Export only the entry points the program kind actually uses, so the
        // DCE pass can prune the rest. A value program (`main : String/Int/…`)
        // never touches the browser runtime; a browser program never uses the
        // value entries. `memory` is always exported (the JS boundary reads it).
        let mut exports = ExportSection::new();
        if is_browser {
            exports.export("render_html", ExportKind::Func, render_html_idx);
            exports.export("alm_browser_start", ExportKind::Func, alm_browser_start_idx);
            exports.export("alm_event", ExportKind::Func, self.alm_event_idx);
            exports.export("alm_port_in", ExportKind::Func, self.port_in_idx);
            exports.export("alm_http_response", ExportKind::Func, self.http_response_idx);
            exports.export("alm_tick", ExportKind::Func, self.tick_idx);
            exports.export("alm_frame", ExportKind::Func, self.frame_idx);
            exports.export("alm_dom_event", ExportKind::Func, self.dom_event_idx);
        } else {
            exports.export("main_int", ExportKind::Func, main_int_idx);
            exports.export("render", ExportKind::Func, render_idx);
        }
        exports.export("memory", ExportKind::Memory, 0);

        let mut module = Module::new();
        module.section(&types);
        module.section(&imports);
        module.section(&funcs);
        module.section(&mems);
        module.section(&globals);
        module.section(&exports);
        module.section(&elems);
        // DataCount must precede Code when code uses `array.new_data`.
        module.section(&DataCountSection { count: 1 });
        module.section(&code);
        module.section(&data);
        let _ = ConstExpr::i32_const(0);
        let bytes = module.finish();
        // Tree-shake: the kernel emits every runtime helper unconditionally, so a
        // program that never touches Dict/Json/Html still carries them. Stub the
        // bodies unreachable from the exports (`ALM_NO_DCE=1` keeps them all).
        if std::env::var_os("ALM_NO_DCE").is_some() {
            Ok(bytes)
        } else {
            Ok(tree_shake(&bytes))
        }
    }

    /// str_append(a, b) : (eqref, eqref) -> eqref — concatenate two strings.
    fn emit_str_append(&self) -> Function {
        // locals: 2 params (a,b); ca(2),cb(3): ref T_STR; alen(4),blen(5): i32; dest(6): ref T_STR
        let mut f = Function::new([(2, ref_to(T_STR)), (2, ValType::I32), (1, ref_to(T_STR))]);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_STR));
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_STR));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::ArrayLen);
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::ArrayLen);
        f.instruction(&Instruction::LocalSet(5));
        // dest = new T_STR of len (alen+blen)
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::ArrayNewDefault(T_STR));
        f.instruction(&Instruction::LocalSet(6));
        // array.copy dest[0..] <- a[0..alen]
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::ArrayCopy { array_type_index_dst: T_STR, array_type_index_src: T_STR });
        // array.copy dest[alen..] <- b[0..blen]
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::ArrayCopy { array_type_index_dst: T_STR, array_type_index_src: T_STR });
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::End);
        f
    }

    /// str_from_int(boxed) : eqref -> eqref — decimal rendering of an Int.
    /// Writes digits into linear memory backwards then builds a T_STR array.
    fn emit_str_from_int(&self) -> Function {
        // locals: param(0); n(1):i64; neg(2):i32; pos(3):i32; i(4):i32; len(5):i32; dest(6):ref T_STR
        let mut f = Function::new([
            (1, ValType::I64),
            (3, ValType::I32),
            (1, ValType::I32),
            (1, ref_to(T_STR)),
        ]);
        // n = unbox
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::Call(self.unbox_int_idx));
        f.instruction(&Instruction::LocalSet(1));
        // neg = n < 0 ; if neg n = -n
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I64Const(0));
        f.instruction(&Instruction::I64LtS);
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I64Const(0));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I64Sub);
        f.instruction(&Instruction::LocalSet(1));
        f.instruction(&Instruction::End);
        // pos = 64
        f.instruction(&Instruction::I32Const(64));
        f.instruction(&Instruction::LocalSet(3));
        // if n == 0 { pos--; mem[pos]='0' } else { loop digits }
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I64Eqz);
        f.instruction(&Instruction::If(BlockType::Empty));
        dec(&mut f, 3);
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Const(b'0' as i32));
        f.instruction(&Instruction::I32Store8(mem0()));
        f.instruction(&Instruction::Else);
        // loop while n>0
        f.instruction(&Instruction::Loop(BlockType::Empty));
        dec(&mut f, 3); // pos--
        // mem[pos] = '0' + (n % 10)
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Const(b'0' as i32));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I64Const(10));
        f.instruction(&Instruction::I64RemS);
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I32Store8(mem0()));
        // n /= 10
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I64Const(10));
        f.instruction(&Instruction::I64DivS);
        f.instruction(&Instruction::LocalSet(1));
        // if n>0 continue
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I64Const(0));
        f.instruction(&Instruction::I64GtS);
        f.instruction(&Instruction::BrIf(0));
        f.instruction(&Instruction::End); // loop
        f.instruction(&Instruction::End); // if
        // if neg { pos--; mem[pos]='-' }
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::If(BlockType::Empty));
        dec(&mut f, 3);
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Const(b'-' as i32));
        f.instruction(&Instruction::I32Store8(mem0()));
        f.instruction(&Instruction::End);
        // len = 64 - pos ; dest = new T_STR len
        f.instruction(&Instruction::I32Const(64));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::ArrayNewDefault(T_STR));
        f.instruction(&Instruction::LocalSet(6));
        // i=0; loop while i<len { dest[i] = mem[pos+i]; i++ }
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1)); // exit block
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I32Load8U(mem0()));
        f.instruction(&Instruction::ArraySet(T_STR));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End); // loop
        f.instruction(&Instruction::End); // block
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::End);
        f
    }

    /// render() : () -> i32 — compute `main : String`, copy its bytes into
    /// linear memory at offset 0, and return the length (JS reads memory).
    fn emit_render(&self, main_idx: u32) -> Function {
        // locals: s(0):ref T_STR; len(1):i32; i(2):i32
        let mut f = Function::new([(1, ref_to(T_STR)), (2, ValType::I32)]);
        f.instruction(&Instruction::Call(main_idx));
        f.instruction(&cast_to(T_STR));
        f.instruction(&Instruction::LocalSet(0));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::ArrayLen);
        f.instruction(&Instruction::LocalSet(1));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        // mem[i] = s[i]
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::ArrayGetU(T_STR));
        f.instruction(&Instruction::I32Store8(mem0()));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::End);
        f
    }

    /// apply1(clos, arg) : (eqref, eqref) -> eqref — apply one argument to a
    /// closure; either build a bigger closure or, when saturated, call the
    /// underlying function via `call_ref`.
    fn emit_apply1(&self) -> Function {
        // locals: clos(0), arg(1) params; c(2):ref T_CLOS; arity(3),applied(4):i32;
        // na(5):ref T_ARR; napplied(6):i32
        let mut f = Function::new([
            (1, ref_to(T_CLOS)),
            (2, ValType::I32),
            (1, ref_to(T_ARR)),
            (1, ValType::I32),
        ]);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_CLOS));
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::StructGet { struct_type_index: T_CLOS, field_index: 1 });
        f.instruction(&Instruction::LocalSet(3)); // arity
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::StructGet { struct_type_index: T_CLOS, field_index: 2 });
        f.instruction(&Instruction::LocalSet(4)); // applied
        // na = new T_ARR arity; copy old args[0..applied]; na[applied] = arg
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::ArrayNewDefault(T_ARR));
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::StructGet { struct_type_index: T_CLOS, field_index: 3 });
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::ArrayCopy { array_type_index_dst: T_ARR, array_type_index_src: T_ARR });
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::ArraySet(T_ARR));
        // napplied = applied + 1
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(6));
        // if napplied == arity { dispatch } else { new closure }
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        self.emit_dispatch(&mut f, 3, 5, 2, 1);
        f.instruction(&Instruction::Else);
        // new T_CLOS { func, arity, napplied, na }
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::StructGet { struct_type_index: T_CLOS, field_index: 0 });
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::StructNew(T_CLOS));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f
    }

    /// Emit the arity-dispatch if-chain for `apply1`: when `arity_local == a`,
    /// load `a` args and `call_ref` the closure's function; else recurse.
    fn emit_dispatch(&self, f: &mut Function, arity_local: u32, args_local: u32, clos_local: u32, a: u32) {
        if a > MAX_ARITY {
            f.instruction(&Instruction::Unreachable);
            return;
        }
        let ft = self.fn_types[&a];
        f.instruction(&Instruction::LocalGet(arity_local));
        f.instruction(&Instruction::I32Const(a as i32));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        for k in 0..a {
            f.instruction(&Instruction::LocalGet(args_local));
            f.instruction(&Instruction::I32Const(k as i32));
            f.instruction(&Instruction::ArrayGet(T_ARR));
        }
        f.instruction(&Instruction::LocalGet(clos_local));
        f.instruction(&Instruction::StructGet { struct_type_index: T_CLOS, field_index: 0 });
        f.instruction(&Instruction::RefCastNonNull(HeapType::Concrete(ft)));
        f.instruction(&Instruction::CallRef(ft));
        f.instruction(&Instruction::Else);
        self.emit_dispatch(f, arity_local, args_local, clos_local, a + 1);
        f.instruction(&Instruction::End);
    }

    /// Lift a lambda / local function to a synthesized top-level wasm function
    /// (capturing free enclosing locals) and emit a closure value for it onto
    /// `f`, with the captured values pre-applied.
    fn lift(
        &mut self,
        params: &[(can::Pattern, can::Type)],
        body: &TypedExpr,
        ctx: &mut FnCtx,
        f: &mut Function,
    ) -> Result<(), String> {
        // Names bound by the parameters.
        let mut bound: std::collections::HashSet<String> = std::collections::HashSet::new();
        for (p, _) in params {
            for n in pat_names(p) {
                bound.insert(n);
            }
        }
        // Free locals of the body that are bound in the enclosing scope = captures.
        let mut free = Vec::new();
        free_locals(body, &bound, &mut free);
        let captures: Vec<(String, u32)> =
            free.iter().filter_map(|n| ctx.lookup(n).map(|s| (n.clone(), s))).collect();
        let ncap = captures.len() as u32;
        let nparams = params.len() as u32;
        let total = ncap + nparams;
        self.fn_type(total);

        // Reserve this lifted function's index before generating its body (which
        // may lift nested lambdas that take later indices).
        let lidx = self.lifted_base + self.lifted.len() as u32;
        self.lifted.push((total, Function::new([])));

        let param_dtor: u32 = params
            .iter()
            .filter(|(p, _)| !matches!(p.value, can::Pattern_::Var(_) | can::Pattern_::Anything))
            .map(|(p, _)| pat_size(p))
            .sum();
        let extra = count_bindings(body) + param_dtor;
        let mut lf = Function::new([(extra + 1, eqref()), (1, ValType::I64)]);
        let mut lctx = FnCtx::new();
        lctx.next_local = total;
        lctx.scratch_eqref = total + extra;
        lctx.scratch_i64 = total + extra + 1;
        for (i, (name, _)) in captures.iter().enumerate() {
            lctx.scope.push((name.clone(), i as u32));
        }
        for (i, (p, _)) in params.iter().enumerate() {
            self.bind_pat(p, ncap + i as u32, &mut lctx, &mut lf)?;
        }
        self.emit_expr(body, &mut lctx, &mut lf)?;
        lf.instruction(&Instruction::End);
        let slot = (lidx - self.lifted_base) as usize;
        self.lifted[slot] = (total, lf);

        // Emit the closure: ref.func, arity=total, applied=ncap, args = fixed
        // array [captures..., nulls for the remaining params].
        f.instruction(&Instruction::RefFunc(lidx));
        f.instruction(&Instruction::I32Const(total as i32));
        f.instruction(&Instruction::I32Const(ncap as i32));
        for (_, s) in &captures {
            f.instruction(&Instruction::LocalGet(*s));
        }
        for _ in ncap..total {
            f.instruction(&Instruction::RefNull(eq_heap()));
        }
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: total });
        f.instruction(&Instruction::StructNew(T_CLOS));
        Ok(())
    }

    /// list_cons(x, xs) : prepend `x`. Amortized O(1) — writes into the
    /// backing's front slack when this view owns it, else grows with fresh
    /// front space (new capacity `2*(len+1)`).
    fn emit_list_cons(&self) -> Function {
        // params: x(0), xs(1). locals: len(2), cap(3), start(4), newcap(5),
        //   nstart(6), k(7):i32, bk(8):ref null T_BACK, ndata(9), odata(10):ref T_ARR
        let mut f = Function::new([
            (6, ValType::I32),
            (1, ref_null_to(T_BACK)),
            (2, ref_to(T_ARR)),
        ]);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_LIST));
        f.instruction(&Instruction::StructGet { struct_type_index: T_LIST, field_index: 0 });
        f.instruction(&Instruction::LocalSet(2)); // len
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_LIST));
        f.instruction(&Instruction::StructGet { struct_type_index: T_LIST, field_index: 1 });
        f.instruction(&Instruction::LocalSet(8)); // bk
        // in-place fast path when bk != null
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::RefIsNull);
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Empty));
        // odata = bk.data
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&cast_to(T_BACK));
        f.instruction(&Instruction::StructGet { struct_type_index: T_BACK, field_index: 1 });
        f.instruction(&Instruction::LocalSet(10));
        // cap = odata.len; start = cap - len
        f.instruction(&Instruction::LocalGet(10));
        f.instruction(&Instruction::ArrayLen);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::LocalSet(4));
        // if start == bk.head && start > 0
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&cast_to(T_BACK));
        f.instruction(&Instruction::StructGet { struct_type_index: T_BACK, field_index: 0 });
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32GtS);
        f.instruction(&Instruction::I32And);
        f.instruction(&Instruction::If(BlockType::Empty));
        // odata[start-1] = x
        f.instruction(&Instruction::LocalGet(10));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::ArraySet(T_ARR));
        // bk.head = start-1
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&cast_to(T_BACK));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::StructSet { struct_type_index: T_BACK, field_index: 0 });
        // return {len+1, bk}
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::StructNew(T_LIST));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        // grow path: newcap = 2*(len+1)
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I32Const(2));
        f.instruction(&Instruction::I32Mul);
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::ArrayNewDefault(T_ARR));
        f.instruction(&Instruction::LocalSet(9)); // ndata
        // nstart = newcap - (len+1)
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::LocalSet(6));
        // ndata[nstart] = x
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::ArraySet(T_ARR));
        // copy old elements when bk != null
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::RefIsNull);
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(7)); // k
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        // ndata[nstart+1+k] = odata[start+k]
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(10));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::ArraySet(T_ARR));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(7));
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        // return {len+1, {head:nstart, data:ndata}}
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::StructNew(T_BACK));
        f.instruction(&Instruction::StructNew(T_LIST));
        f.instruction(&Instruction::End);
        f
    }

    /// list_map(f, xs) : map `f` over the vector, producing a fresh list.
    fn emit_list_map(&self) -> Function {
        // params f(0), xs(1). locals: len(2), start(3), i(4):i32,
        //   xdata(5), ndata(6):ref T_ARR
        let mut f = Function::new([(3, ValType::I32), (2, ref_to(T_ARR))]);
        list_len(&mut f, 1);
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::ArrayNewDefault(T_ARR));
        f.instruction(&Instruction::LocalSet(6));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::If(BlockType::Empty));
        list_data(&mut f, 1);
        f.instruction(&Instruction::LocalSet(5));
        list_start(&mut f, 1);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&Instruction::ArraySet(T_ARR));
        bump(&mut f, 4, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        // wrap ndata (start=0, len = ndata.len)
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::StructNew(T_BACK));
        f.instruction(&Instruction::StructNew(T_LIST));
        f.instruction(&Instruction::End);
        f
    }

    /// list_foldl(f, acc, xs) : left fold `f element acc`, head first.
    fn emit_list_foldl(&self) -> Function {
        // params f(0), acc(1), xs(2). locals: len(3), start(4), i(5):i32,
        //   xdata(6):ref T_ARR, a(7):eqref
        let mut f =
            Function::new([(3, ValType::I32), (1, ref_to(T_ARR)), (1, eqref())]);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalSet(7)); // a = acc
        list_len(&mut f, 2);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::If(BlockType::Empty));
        list_data(&mut f, 2);
        f.instruction(&Instruction::LocalSet(6));
        list_start(&mut f, 2);
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        // a = f (xdata[start+i]) a
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&Instruction::LocalSet(7));
        bump(&mut f, 5, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::End);
        f
    }

    /// list_length(xs) : number of elements, boxed as Int.
    fn emit_list_length(&self) -> Function {
        let mut f = Function::new([]);
        list_len(&mut f, 0);
        f.instruction(&Instruction::I64ExtendI32S);
        f.instruction(&Instruction::Call(self.box_int_idx));
        f.instruction(&Instruction::End);
        f
    }

    /// A kernel used as a first-class value: synthesize a lifted wrapper that
    /// performs the kernel on its parameters, and emit a capture-free closure
    /// over it (so it flows through apply1 / higher-order functions).
    fn emit_foreign_value(
        &mut self,
        module: &str,
        name: &str,
        tipe: &can::Type,
        f: &mut Function,
    ) -> Result<(), String> {
        // Nullary numeric constants.
        if module == "Basics" && (name == "pi" || name == "e") {
            let v = if name == "pi" { std::f64::consts::PI } else { std::f64::consts::E };
            f.instruction(&Instruction::F64Const(v.into()));
            f.instruction(&Instruction::StructNew(T_FLOAT));
            return Ok(());
        }
        // Nullary empty collections.
        if module == "Array" && name == "empty" {
            push_empty_list(f);
            return Ok(());
        }
        // Dict/Set are treaps; the empty value is a null node.
        if (module == "Dict" || module == "Set") && name == "empty" {
            f.instruction(&Instruction::RefNull(HeapType::Concrete(T_TNODE)));
            return Ok(());
        }
        // Platform.Cmd.none / Platform.Sub.none : CMD_NONE (tag0, no args).
        if (module == "Platform.Cmd" || module == "Platform.Sub") && name == "none" {
            f.instruction(&Instruction::I32Const(0));
            f.instruction(&Instruction::RefNull(HeapType::Concrete(T_ARR)));
            f.instruction(&Instruction::StructNew(T_CTOR));
            return Ok(());
        }
        // Json.Encode.null : a JSON Value with tag 0, no args.
        if module == "Json.Encode" && name == "null" {
            f.instruction(&Instruction::I32Const(0));
            f.instruction(&Instruction::RefNull(HeapType::Concrete(T_ARR)));
            f.instruction(&Instruction::StructNew(T_CTOR));
            return Ok(());
        }
        // Leaf Json.Decode decoders (used as first-class values): a decoder AST
        // node with no args. string=0 int=1 float=2 bool=3 value=4.
        if module == "Json.Decode" {
            let leaf = match name {
                "string" => Some(0),
                "int" => Some(1),
                "float" => Some(2),
                "bool" => Some(3),
                "value" => Some(4),
                _ => None,
            };
            if let Some(tag) = leaf {
                f.instruction(&Instruction::I32Const(tag));
                f.instruction(&Instruction::RefNull(HeapType::Concrete(T_ARR)));
                f.instruction(&Instruction::StructNew(T_CTOR));
                return Ok(());
            }
        }
        // Basics.pi / e : Float constants.
        if module == "Basics" && (name == "pi" || name == "e") {
            let v = if name == "pi" { std::f64::consts::PI } else { std::f64::consts::E };
            f.instruction(&Instruction::F64Const(v.into()));
            f.instruction(&Instruction::StructNew(T_FLOAT));
            return Ok(());
        }
        // Random.minInt / maxInt : the 32-bit signed bounds, as boxed Ints.
        if module == "Random" && (name == "minInt" || name == "maxInt") {
            let v: i64 = if name == "minInt" { -2147483648 } else { 2147483647 };
            f.instruction(&Instruction::I64Const(v));
            f.instruction(&Instruction::Call(self.box_int_idx));
            return Ok(());
        }
        let arity: u32 = match (module, name) {
            ("Basics", "add") | ("Basics", "sub") | ("Basics", "mul") => 2,
            ("String", "append") => 2,
            ("String", "fromInt") | ("String", "length") | ("Char", "toCode") | ("Basics", "not") => 1,
            // Generic fallback: wrap any other kernel by synthesizing local
            // arguments and reusing the ordinary kernel dispatch (emit_kernel).
            _ => return self.emit_foreign_value_generic(module, name, tipe, f),
        };
        self.fn_type(arity);
        let lidx = self.lifted_base + self.lifted.len() as u32;
        let mut lf = Function::new([]);
        match (module, name) {
            ("Basics", "add") | ("Basics", "sub") | ("Basics", "mul") => {
                lf.instruction(&Instruction::LocalGet(0));
                lf.instruction(&Instruction::Call(self.unbox_int_idx));
                lf.instruction(&Instruction::LocalGet(1));
                lf.instruction(&Instruction::Call(self.unbox_int_idx));
                lf.instruction(&match name {
                    "add" => Instruction::I64Add,
                    "sub" => Instruction::I64Sub,
                    _ => Instruction::I64Mul,
                });
                lf.instruction(&Instruction::Call(self.box_int_idx));
            }
            ("String", "append") => {
                lf.instruction(&Instruction::LocalGet(0));
                lf.instruction(&Instruction::LocalGet(1));
                lf.instruction(&Instruction::Call(self.str_append_idx));
            }
            ("String", "fromInt") => {
                lf.instruction(&Instruction::LocalGet(0));
                lf.instruction(&Instruction::Call(self.str_from_int_idx));
            }
            ("String", "length") => {
                lf.instruction(&Instruction::LocalGet(0));
                lf.instruction(&Instruction::Call(self.str_length_idx));
            }
            ("Char", "toCode") => {
                lf.instruction(&Instruction::LocalGet(0));
                lf.instruction(&Instruction::RefCastNonNull(HeapType::Abstract {
                    shared: false,
                    ty: AbstractHeapType::I31,
                }));
                lf.instruction(&Instruction::I31GetS);
                lf.instruction(&Instruction::I64ExtendI32S);
                lf.instruction(&Instruction::Call(self.box_int_idx));
            }
            _ => {
                // Basics.not
                lf.instruction(&Instruction::LocalGet(0));
                lf.instruction(&Instruction::RefCastNonNull(HeapType::Abstract {
                    shared: false,
                    ty: AbstractHeapType::I31,
                }));
                lf.instruction(&Instruction::I31GetS);
                lf.instruction(&Instruction::I32Eqz);
                lf.instruction(&Instruction::RefI31);
            }
        }
        lf.instruction(&Instruction::End);
        self.lifted.push((arity, lf));
        self.emit_make_closure(lidx, arity, f);
        Ok(())
    }

    /// Wrap an arbitrary kernel as a first-class value by synthesizing a lifted
    /// function whose parameters become `Local` arguments to the normal kernel
    /// dispatch. Lets e.g. `Char.toUpper` be passed to `List.map` directly.
    fn emit_foreign_value_generic(
        &mut self,
        module: &str,
        name: &str,
        tipe: &can::Type,
        f: &mut Function,
    ) -> Result<(), String> {
        // Peel the function type into argument types.
        let mut arg_types = Vec::new();
        let mut cur = tipe;
        while let can::Type::Lambda(a, b) = cur {
            arg_types.push((**a).clone());
            cur = b;
        }
        let arity = arg_types.len() as u32;
        if arity == 0 {
            return Err(format!("wasmgc: `{module}.{name}` is not yet usable as a value"));
        }
        self.fn_type(arity);
        // Synthesize `Local` args bound to parameter slots 0..arity.
        let args: Vec<TypedExpr> = arg_types
            .into_iter()
            .enumerate()
            .map(|(i, t)| TypedExpr {
                tipe: t,
                kind: TypedKind::Local(format!("$farg{i}").into()),
                region: Region::ZERO,
            })
            .collect();
        let mut lctx = FnCtx::new();
        lctx.next_local = arity;
        for (i, _) in args.iter().enumerate() {
            lctx.scope.push((format!("$farg{i}"), i as u32));
        }
        let lidx = self.lifted_base + self.lifted.len() as u32;
        self.lifted.push((arity, Function::new([])));
        let mut lf = Function::new([]);
        self.emit_kernel(module, name, &args, &mut lctx, &mut lf)?;
        lf.instruction(&Instruction::End);
        let slot = (lidx - self.lifted_base) as usize;
        self.lifted[slot] = (arity, lf);
        self.emit_make_closure(lidx, arity, f);
        Ok(())
    }

    /// list_append(xs, ys) : `xs ++ ys` — allocate `lenx+leny`, copy both.
    fn emit_list_append(&self) -> Function {
        // params xs(0), ys(1). locals: tot(2),lenx(3),i(4),len(5),ss(6),off(7):i32,
        //   data(8), sd(9):ref T_ARR
        let mut f = Function::new([(6, ValType::I32), (2, ref_to(T_ARR))]);
        list_len(&mut f, 0);
        f.instruction(&Instruction::LocalSet(3)); // lenx
        f.instruction(&Instruction::LocalGet(3));
        list_len(&mut f, 1);
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(2)); // tot
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::ArrayNewDefault(T_ARR));
        f.instruction(&Instruction::LocalSet(8)); // data
        // copy xs at offset 0
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(7)); // off
        copy_into(&mut f, 0, 8, 7, 4, 9, 6, 5);
        // copy ys at offset lenx
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalSet(7));
        copy_into(&mut f, 1, 8, 7, 4, 9, 6, 5);
        // wrap
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::StructNew(T_BACK));
        f.instruction(&Instruction::StructNew(T_LIST));
        f.instruction(&Instruction::End);
        f
    }

    /// list_reverse(xs) : allocate `len`, copy element k to slot len-1-k.
    fn emit_list_reverse(&self) -> Function {
        // params xs(0). locals: len(1), start(2), i(3):i32, xdata(4), ndata(5):ref T_ARR
        let mut f = Function::new([(3, ValType::I32), (2, ref_to(T_ARR))]);
        list_len(&mut f, 0);
        f.instruction(&Instruction::LocalSet(1));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::ArrayNewDefault(T_ARR));
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::If(BlockType::Empty));
        list_data(&mut f, 0);
        f.instruction(&Instruction::LocalSet(4));
        list_start(&mut f, 0);
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        // ndata[len-1-i] = xdata[start+i]
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::ArraySet(T_ARR));
        bump(&mut f, 3, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::StructNew(T_BACK));
        f.instruction(&Instruction::StructNew(T_LIST));
        f.instruction(&Instruction::End);
        f
    }

    /// list_filter(pred, xs) : keep elements where `pred` holds. Scans from the
    /// tail so consing yields head-first order.
    fn emit_list_filter(&self) -> Function {
        // params pred(0), xs(1). locals: len(2), start(3), i(4):i32,
        //   xdata(5):ref T_ARR, acc(6):eqref, elem(7):eqref
        let mut f =
            Function::new([(3, ValType::I32), (1, ref_to(T_ARR)), (2, eqref())]);
        push_empty_list(&mut f);
        f.instruction(&Instruction::LocalSet(6)); // acc = []
        list_len(&mut f, 1);
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::If(BlockType::Empty));
        list_data(&mut f, 1);
        f.instruction(&Instruction::LocalSet(5));
        list_start(&mut f, 1);
        f.instruction(&Instruction::LocalSet(3));
        // i = len-1 downto 0
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::BrIf(1));
        // elem = xdata[start+i]
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::LocalSet(7));
        // if pred elem: acc = cons(elem, acc)
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&Instruction::RefCastNonNull(HeapType::Abstract { shared: false, ty: AbstractHeapType::I31 }));
        f.instruction(&Instruction::I31GetS);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::Call(self.list_cons_idx));
        f.instruction(&Instruction::LocalSet(6));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::End);
        f
    }

    /// list_foldr(f, acc, xs) : right fold `f element acc`, tail first.
    fn emit_list_foldr(&self) -> Function {
        // params f(0), acc(1), xs(2). locals: len(3), start(4), i(5):i32,
        //   xdata(6):ref T_ARR, a(7):eqref
        let mut f =
            Function::new([(3, ValType::I32), (1, ref_to(T_ARR)), (1, eqref())]);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalSet(7));
        list_len(&mut f, 2);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::If(BlockType::Empty));
        list_data(&mut f, 2);
        f.instruction(&Instruction::LocalSet(6));
        list_start(&mut f, 2);
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::LocalSet(5)); // i = len-1
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::BrIf(1));
        // a = f (xdata[start+i]) a
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&Instruction::LocalSet(7));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::End);
        f
    }

    /// modby(m, x) : Elm `modBy m x` — floored modulo (result takes the sign of
    /// the modulus), boxed as Int.
    fn emit_modby(&self) -> Function {
        // m(0), x(1); mm(2):i64, r(3):i64
        let mut f = Function::new([(2, ValType::I64)]);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::Call(self.unbox_int_idx));
        f.instruction(&Instruction::LocalSet(2)); // mm
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(self.unbox_int_idx));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I64RemS);
        f.instruction(&Instruction::LocalSet(3)); // r = x rem m
        // if r != 0 && (r ^ m) < 0 { r += m }
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I64Eqz);
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I64Xor);
        f.instruction(&Instruction::I64Const(0));
        f.instruction(&Instruction::I64LtS);
        f.instruction(&Instruction::I32And);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I64Add);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::Call(self.box_int_idx));
        f.instruction(&Instruction::End);
        f
    }

    /// list_range(lo, hi) : `[lo, lo+1, .., hi]`.
    fn emit_list_range(&self) -> Function {
        // params lo(0), hi(1). locals: loi(2):i64, cnt(3):i64, n(4), i(5):i32,
        //   data(6):ref T_ARR
        let mut f = Function::new([(2, ValType::I64), (2, ValType::I32), (1, ref_to(T_ARR))]);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::Call(self.unbox_int_idx));
        f.instruction(&Instruction::LocalSet(2)); // loi
        // cnt = hi - lo + 1
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(self.unbox_int_idx));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I64Sub);
        f.instruction(&Instruction::I64Const(1));
        f.instruction(&Instruction::I64Add);
        f.instruction(&Instruction::LocalSet(3));
        // n = cnt < 0 ? 0 : cnt (as i32)
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I64Const(0));
        f.instruction(&Instruction::I64LtS);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::ArrayNewDefault(T_ARR));
        f.instruction(&Instruction::LocalSet(6));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        // data[i] = box(loi + i)
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I64ExtendI32S);
        f.instruction(&Instruction::I64Add);
        f.instruction(&Instruction::Call(self.box_int_idx));
        f.instruction(&Instruction::ArraySet(T_ARR));
        bump(&mut f, 5, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::StructNew(T_BACK));
        f.instruction(&Instruction::StructNew(T_LIST));
        f.instruction(&Instruction::End);
        f
    }

    /// list_member(x, xs) : whether `x` structurally equals some element.
    fn emit_list_member(&self) -> Function {
        // params x(0), xs(1). locals: len(2), start(3), i(4):i32, data(5):ref T_ARR
        let mut f = Function::new([(3, ValType::I32), (1, ref_to(T_ARR))]);
        list_len(&mut f, 1);
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::If(BlockType::Empty));
        list_data(&mut f, 1);
        f.instruction(&Instruction::LocalSet(5));
        list_start(&mut f, 1);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::Call(self.val_eq_idx));
        f.instruction(&Instruction::RefCastNonNull(HeapType::Abstract { shared: false, ty: AbstractHeapType::I31 }));
        f.instruction(&Instruction::I31GetS);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::RefI31);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        bump(&mut f, 4, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::RefI31);
        f.instruction(&Instruction::End);
        f
    }

    /// list_take(n, xs) : the first `n` elements (copied).
    fn emit_list_take(&self) -> Function {
        // params n(0), xs(1). locals: n2(2), len(3), start(4), i(5):i32,
        //   xdata(6), ndata(7):ref T_ARR
        let mut f = Function::new([(4, ValType::I32), (2, ref_to(T_ARR))]);
        list_len(&mut f, 1);
        f.instruction(&Instruction::LocalSet(3));
        // n2 = clamp(unbox n, 0, len)
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::Call(self.unbox_int_idx));
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::LocalSet(2));
        // if n2 < 0 { 0 }
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::End);
        // if n2 > len { len }
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32GtS);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::ArrayNewDefault(T_ARR));
        f.instruction(&Instruction::LocalSet(7));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::If(BlockType::Empty));
        list_data(&mut f, 1);
        f.instruction(&Instruction::LocalSet(6));
        list_start(&mut f, 1);
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::ArraySet(T_ARR));
        bump(&mut f, 5, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::StructNew(T_BACK));
        f.instruction(&Instruction::StructNew(T_LIST));
        f.instruction(&Instruction::End);
        f
    }

    /// list_drop(n, xs) : all but the first `n` — a shared view `{len-n2, bk}`.
    fn emit_list_drop(&self) -> Function {
        // params n(0), xs(1). locals: n2(2), len(3):i32
        let mut f = Function::new([(2, ValType::I32)]);
        list_len(&mut f, 1);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::Call(self.unbox_int_idx));
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32GtS);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::End);
        // {len - n2, xs.bk}
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Sub);
        list_bk(&mut f, 1);
        f.instruction(&Instruction::StructNew(T_LIST));
        f.instruction(&Instruction::End);
        f
    }

    /// list_concat(xss) : concatenate a list of lists (two passes: sum, copy).
    fn emit_list_concat(&self) -> Function {
        // params xss(0). locals (index 1 unused): outerlen(2),total(3),oi(4),
        //   off(5),ci(6),css(7),clen(8):i32, data(9), csd(10):ref T_ARR, inner(11):eqref
        let mut f =
            Function::new([(8, ValType::I32), (2, ref_to(T_ARR)), (1, eqref())]);
        list_len(&mut f, 0);
        f.instruction(&Instruction::LocalSet(2));
        // pass 1: total
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        // inner = xss[oi]
        list_data(&mut f, 0);
        list_start(&mut f, 0);
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::LocalSet(11));
        f.instruction(&Instruction::LocalGet(3));
        list_len(&mut f, 11);
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(3));
        bump(&mut f, 4, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::ArrayNewDefault(T_ARR));
        f.instruction(&Instruction::LocalSet(9));
        // pass 2: copy each inner at running offset
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        list_data(&mut f, 0);
        list_start(&mut f, 0);
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::LocalSet(11));
        copy_into(&mut f, 11, 9, 5, 6, 10, 7, 8);
        f.instruction(&Instruction::LocalGet(5));
        list_len(&mut f, 11);
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(5));
        bump(&mut f, 4, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::StructNew(T_BACK));
        f.instruction(&Instruction::StructNew(T_LIST));
        f.instruction(&Instruction::End);
        f
    }

    /// list_head/list_tail(xs) : `Nothing` on `[]`, else `Just head`/`Just tail`.
    fn emit_list_head(&self, tail: bool) -> Function {
        let mut f = Function::new([]);
        list_is_empty(&mut f, 0);
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        // Nothing
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::RefNull(HeapType::Concrete(T_ARR)));
        f.instruction(&Instruction::StructNew(T_CTOR));
        f.instruction(&Instruction::Else);
        // Just head / Just tail
        f.instruction(&Instruction::I32Const(0));
        if tail {
            list_tail(&mut f, 0);
        } else {
            list_head(&mut f, 0);
        }
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 1 });
        f.instruction(&Instruction::StructNew(T_CTOR));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f
    }

    /// maybe_with_default(d, m) : `m ? unwrap : d`.
    fn emit_maybe_with_default(&self) -> Function {
        let mut f = Function::new([]);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_CTOR));
        f.instruction(&Instruction::StructGet { struct_type_index: T_CTOR, field_index: 0 });
        f.instruction(&Instruction::I32Eqz); // tag == 0 (Just)
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_CTOR));
        f.instruction(&Instruction::StructGet { struct_type_index: T_CTOR, field_index: 1 });
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::End); // if
        f.instruction(&Instruction::End); // function
        f
    }

    /// maybe_map(f, m) : `Just (f x)` on `Just x`, else `Nothing`.
    fn emit_maybe_map(&self) -> Function {
        let mut f = Function::new([]);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_CTOR));
        f.instruction(&Instruction::StructGet { struct_type_index: T_CTOR, field_index: 0 });
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        // Just (apply1(f, x))
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_CTOR));
        f.instruction(&Instruction::StructGet { struct_type_index: T_CTOR, field_index: 1 });
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 1 });
        f.instruction(&Instruction::StructNew(T_CTOR));
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::End); // if
        f.instruction(&Instruction::End); // function
        f
    }

    /// maybe_and_then(f, m) : `f x` on `Just x` (f returns a Maybe), else `m`.
    fn emit_maybe_and_then(&self) -> Function {
        let mut f = Function::new([]);
        ctor_tag(&mut f, 1);
        f.instruction(&Instruction::I32Eqz); // Just?
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        f.instruction(&Instruction::LocalGet(0));
        ctor_arg0(&mut f, 1);
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f
    }

    /// maybe_mapN(f, m1..mn) : `Just (f x1 .. xn)` if every argument is `Just`,
    /// else `Nothing`. Params are f(0) then the n maybes (locals 1..=n).
    fn emit_maybe_mapn(&self, n: u32) -> Function {
        let mut f = Function::new([]);
        for i in 1..=n {
            ctor_tag(&mut f, i);
            f.instruction(&Instruction::I32Eqz); // Just?
            f.instruction(&Instruction::If(BlockType::Result(eqref())));
        }
        // Just (apply1(..apply1(f, x1).., xn))
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(0));
        for i in 1..=n {
            ctor_arg0(&mut f, i);
            f.instruction(&Instruction::Call(self.apply1_idx));
        }
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 1 });
        f.instruction(&Instruction::StructNew(T_CTOR));
        for _ in 0..n {
            f.instruction(&Instruction::Else);
            f.instruction(&Instruction::I32Const(1)); // Nothing
            f.instruction(&Instruction::RefNull(HeapType::Concrete(T_ARR)));
            f.instruction(&Instruction::StructNew(T_CTOR));
            f.instruction(&Instruction::End);
        }
        f.instruction(&Instruction::End); // function
        f
    }

    /// result_mapN(f, r1..rn) : `Ok (f x1 .. xn)` if every argument is `Ok`,
    /// else the leftmost `Err`. Params f(0) then the n results (locals 1..=n).
    fn emit_result_mapn(&self, n: u32) -> Function {
        let mut f = Function::new([]);
        for i in 1..=n {
            ctor_tag(&mut f, i);
            f.instruction(&Instruction::I32Eqz); // Ok?
            f.instruction(&Instruction::If(BlockType::Result(eqref())));
        }
        f.instruction(&Instruction::I32Const(0)); // Ok
        f.instruction(&Instruction::LocalGet(0));
        for i in 1..=n {
            ctor_arg0(&mut f, i);
            f.instruction(&Instruction::Call(self.apply1_idx));
        }
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 1 });
        f.instruction(&Instruction::StructNew(T_CTOR));
        // Each Else yields the corresponding Err argument (innermost = rn).
        for i in (1..=n).rev() {
            f.instruction(&Instruction::Else);
            f.instruction(&Instruction::LocalGet(i));
            f.instruction(&Instruction::End);
        }
        f.instruction(&Instruction::End); // function
        f
    }

    // ---- Random (elm/random PCG) ----
    // A Seed is a 2-tuple T_ARR [a, b] of boxed Ints holding u32 state/increment.
    // A Generator is a reified T_CTOR: tag 0 GInt[lo,hi], 1 GFloat[a,b],
    // 2 GConst[x], 3 GMap[f,g], 4 GMap2[f,ga,gb], 5 GMap3[f,ga,gb,gc],
    // 6 GAndThen[f,g], 7 GPair[ga,gb]. `random_step` interprets it, returning
    // the (value, nextSeed) 2-tuple.

    /// random_peel(seed) -> i64 : the PCG output word (a u32, in an i64). The
    /// `* 277803737` MUST be done in f64 on the *signed* int32 xor result, to
    /// match elm/random's JS (the product exceeds 2^53, so its low bits are lost
    /// to float rounding — an exact i64 multiply diverges from JS and native).
    fn emit_random_peel(&self) -> Function {
        // param seed(0). locals a(1), word(2): i64
        let mut f = Function::new([(2, ValType::I64)]);
        const M: i64 = 0xFFFF_FFFF;
        // a = seed[0] & 0xFFFFFFFF
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_ARR));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::Call(self.unbox_int_idx));
        f.instruction(&Instruction::I64Const(M));
        f.instruction(&Instruction::I64And);
        f.instruction(&Instruction::LocalSet(1));
        // tmp = a ^ (a >>u ((a >>u 28) + 4)), as a signed int32
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I64Const(28));
        f.instruction(&Instruction::I64ShrU);
        f.instruction(&Instruction::I64Const(4));
        f.instruction(&Instruction::I64Add);
        f.instruction(&Instruction::I64ShrU);
        f.instruction(&Instruction::I64Xor);
        f.instruction(&Instruction::I64Extend32S);
        // word = ToUint32((f64)tmp * 277803737.0)  (float rounding is significant)
        f.instruction(&Instruction::F64ConvertI64S);
        f.instruction(&Instruction::F64Const(277803737.0.into()));
        f.instruction(&Instruction::F64Mul);
        f.instruction(&Instruction::I64TruncF64S);
        f.instruction(&Instruction::I64Const(M));
        f.instruction(&Instruction::I64And);
        f.instruction(&Instruction::LocalSet(2));
        // ((word >>u 22) ^ word) & 0xFFFFFFFF
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I64Const(22));
        f.instruction(&Instruction::I64ShrU);
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I64Xor);
        f.instruction(&Instruction::I64Const(M));
        f.instruction(&Instruction::I64And);
        f.instruction(&Instruction::End);
        f
    }

    /// random_next_seed(seed) -> seed : advance the LCG state, keep the increment.
    fn emit_random_next_seed(&self) -> Function {
        let mut f = Function::new([]);
        const M: i64 = 0xFFFF_FFFF;
        // box((seed[0]*1664525 + seed[1]) & 0xFFFFFFFF)
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_ARR));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::Call(self.unbox_int_idx));
        f.instruction(&Instruction::I64Const(1664525));
        f.instruction(&Instruction::I64Mul);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_ARR));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::Call(self.unbox_int_idx));
        f.instruction(&Instruction::I64Add);
        f.instruction(&Instruction::I64Const(M));
        f.instruction(&Instruction::I64And);
        f.instruction(&Instruction::Call(self.box_int_idx));
        // seed[1] (unchanged)
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_ARR));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 2 });
        f.instruction(&Instruction::End);
        f
    }

    /// random_initial_seed(x) -> seed : Random.initialSeed.
    fn emit_random_initial_seed(&self) -> Function {
        // param x(0). local seed1(1): eqref
        let mut f = Function::new([(1, eqref())]);
        const M: i64 = 0xFFFF_FFFF;
        // seed1 = next_seed([0, 1013904223])
        f.instruction(&Instruction::I64Const(0));
        f.instruction(&Instruction::Call(self.box_int_idx));
        f.instruction(&Instruction::I64Const(1013904223));
        f.instruction(&Instruction::Call(self.box_int_idx));
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 2 });
        f.instruction(&Instruction::Call(self.random_next_seed_idx));
        f.instruction(&Instruction::LocalSet(1));
        // state2 = (seed1[0] + x) & 0xFFFFFFFF
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_ARR));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::Call(self.unbox_int_idx));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::Call(self.unbox_int_idx));
        f.instruction(&Instruction::I64Add);
        f.instruction(&Instruction::I64Const(M));
        f.instruction(&Instruction::I64And);
        f.instruction(&Instruction::Call(self.box_int_idx));
        // next_seed([state2, seed1[1]])
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_ARR));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 2 });
        f.instruction(&Instruction::Call(self.random_next_seed_idx));
        f.instruction(&Instruction::End);
        f
    }

    /// random_step(gen, seed) -> (value, seed) : interpret a reified Generator.
    fn emit_random_step(&self) -> Function {
        // params gen(0), seed(1). locals: ta(2),tb(3),tc(4),cur(5):eqref;
        //   lo(6),hi(7),range(8),x(9),thr(10):i64; tag(11):i32
        let mut f = Function::new([(4, eqref()), (5, ValType::I64), (1, ValType::I32)]);
        const M32: i64 = 0x1_0000_0000;
        // element i of the tuple/ctor-args held in `local`
        let telem = |f: &mut Function, local: u32, i: i32| {
            f.instruction(&Instruction::LocalGet(local));
            f.instruction(&cast_to(T_ARR));
            f.instruction(&Instruction::I32Const(i));
            f.instruction(&Instruction::ArrayGet(T_ARR));
        };
        ctor_tag(&mut f, 0);
        f.instruction(&Instruction::LocalSet(11));

        // GConst (2): (x, seed)
        f.instruction(&Instruction::LocalGet(11));
        f.instruction(&Instruction::I32Const(2));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Empty));
        ctor_argn(&mut f, 0, 0);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 2 });
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);

        // GMap (3): t = step(g, seed); (f t.0, t.1)
        f.instruction(&Instruction::LocalGet(11));
        f.instruction(&Instruction::I32Const(3));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Empty));
        ctor_argn(&mut f, 0, 1);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(self.random_step_idx));
        f.instruction(&Instruction::LocalSet(2));
        ctor_argn(&mut f, 0, 0); // f
        telem(&mut f, 2, 0);
        f.instruction(&Instruction::Call(self.apply1_idx));
        telem(&mut f, 2, 1);
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 2 });
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);

        // GMap2 (4): ta=step(ga,seed); tb=step(gb,ta.1); (f ta.0 tb.0, tb.1)
        f.instruction(&Instruction::LocalGet(11));
        f.instruction(&Instruction::I32Const(4));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Empty));
        ctor_argn(&mut f, 0, 1);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(self.random_step_idx));
        f.instruction(&Instruction::LocalSet(2));
        ctor_argn(&mut f, 0, 2);
        telem(&mut f, 2, 1);
        f.instruction(&Instruction::Call(self.random_step_idx));
        f.instruction(&Instruction::LocalSet(3));
        ctor_argn(&mut f, 0, 0); // f
        telem(&mut f, 2, 0);
        f.instruction(&Instruction::Call(self.apply1_idx));
        telem(&mut f, 3, 0);
        f.instruction(&Instruction::Call(self.apply1_idx));
        telem(&mut f, 3, 1);
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 2 });
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);

        // GMap3 (5)
        f.instruction(&Instruction::LocalGet(11));
        f.instruction(&Instruction::I32Const(5));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Empty));
        ctor_argn(&mut f, 0, 1);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(self.random_step_idx));
        f.instruction(&Instruction::LocalSet(2));
        ctor_argn(&mut f, 0, 2);
        telem(&mut f, 2, 1);
        f.instruction(&Instruction::Call(self.random_step_idx));
        f.instruction(&Instruction::LocalSet(3));
        ctor_argn(&mut f, 0, 3);
        telem(&mut f, 3, 1);
        f.instruction(&Instruction::Call(self.random_step_idx));
        f.instruction(&Instruction::LocalSet(4));
        ctor_argn(&mut f, 0, 0); // f
        telem(&mut f, 2, 0);
        f.instruction(&Instruction::Call(self.apply1_idx));
        telem(&mut f, 3, 0);
        f.instruction(&Instruction::Call(self.apply1_idx));
        telem(&mut f, 4, 0);
        f.instruction(&Instruction::Call(self.apply1_idx));
        telem(&mut f, 4, 1);
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 2 });
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);

        // GAndThen (6): t = step(g, seed); step(f t.0, t.1)
        f.instruction(&Instruction::LocalGet(11));
        f.instruction(&Instruction::I32Const(6));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Empty));
        ctor_argn(&mut f, 0, 1);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(self.random_step_idx));
        f.instruction(&Instruction::LocalSet(2));
        ctor_argn(&mut f, 0, 0); // f
        telem(&mut f, 2, 0);
        f.instruction(&Instruction::Call(self.apply1_idx)); // new generator
        telem(&mut f, 2, 1);
        f.instruction(&Instruction::Call(self.random_step_idx));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);

        // GPair (7): ta=step(ga,seed); tb=step(gb,ta.1); ((ta.0,tb.0), tb.1)
        f.instruction(&Instruction::LocalGet(11));
        f.instruction(&Instruction::I32Const(7));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Empty));
        ctor_argn(&mut f, 0, 0);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(self.random_step_idx));
        f.instruction(&Instruction::LocalSet(2));
        ctor_argn(&mut f, 0, 1);
        telem(&mut f, 2, 1);
        f.instruction(&Instruction::Call(self.random_step_idx));
        f.instruction(&Instruction::LocalSet(3));
        telem(&mut f, 2, 0);
        telem(&mut f, 3, 0);
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 2 });
        telem(&mut f, 3, 1);
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 2 });
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);

        // GFloat (1)
        f.instruction(&Instruction::LocalGet(11));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Empty));
        // cur = next_seed(seed)  (= seed1)
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(self.random_next_seed_idx));
        f.instruction(&Instruction::LocalSet(5));
        // val = (hi*2^27 + lo)/2^53 where hi=peel(seed)&0x03FFFFFF, lo=peel(seed1)&0x07FFFFFF
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(self.random_peel_idx));
        f.instruction(&Instruction::I64Const(0x03FF_FFFF));
        f.instruction(&Instruction::I64And);
        f.instruction(&Instruction::F64ConvertI64U);
        f.instruction(&Instruction::F64Const(134217728.0.into()));
        f.instruction(&Instruction::F64Mul);
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::Call(self.random_peel_idx));
        f.instruction(&Instruction::I64Const(0x07FF_FFFF));
        f.instruction(&Instruction::I64And);
        f.instruction(&Instruction::F64ConvertI64U);
        f.instruction(&Instruction::F64Add);
        f.instruction(&Instruction::F64Const(9007199254740992.0.into()));
        f.instruction(&Instruction::F64Div);
        // range = abs(b - a)
        ctor_argn(&mut f, 0, 1);
        f.instruction(&cast_to(T_FLOAT));
        f.instruction(&Instruction::StructGet { struct_type_index: T_FLOAT, field_index: 0 });
        ctor_argn(&mut f, 0, 0);
        f.instruction(&cast_to(T_FLOAT));
        f.instruction(&Instruction::StructGet { struct_type_index: T_FLOAT, field_index: 0 });
        f.instruction(&Instruction::F64Sub);
        f.instruction(&Instruction::F64Abs);
        f.instruction(&Instruction::F64Mul);
        // + a
        ctor_argn(&mut f, 0, 0);
        f.instruction(&cast_to(T_FLOAT));
        f.instruction(&Instruction::StructGet { struct_type_index: T_FLOAT, field_index: 0 });
        f.instruction(&Instruction::F64Add);
        f.instruction(&Instruction::StructNew(T_FLOAT));
        // seed' = next_seed(seed1)
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::Call(self.random_next_seed_idx));
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 2 });
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);

        // GInt (0): lo/hi from args; rejection-sample to match elm/random.
        f.instruction(&Instruction::LocalGet(11));
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Empty));
        ctor_argn(&mut f, 0, 0);
        f.instruction(&Instruction::Call(self.unbox_int_idx));
        f.instruction(&Instruction::LocalSet(6));
        ctor_argn(&mut f, 0, 1);
        f.instruction(&Instruction::Call(self.unbox_int_idx));
        f.instruction(&Instruction::LocalSet(7));
        // if lo > hi, swap → 6=min, 7=max
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I64GtS);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::LocalSet(9));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::LocalSet(6));
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::LocalSet(7));
        f.instruction(&Instruction::End);
        // range = hi - lo + 1
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I64Sub);
        f.instruction(&Instruction::I64Const(1));
        f.instruction(&Instruction::I64Add);
        f.instruction(&Instruction::LocalSet(8));
        // power-of-two fast path: ((range-1) & range) == 0
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::I64Const(1));
        f.instruction(&Instruction::I64Sub);
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::I64And);
        f.instruction(&Instruction::I64Eqz);
        f.instruction(&Instruction::If(BlockType::Empty));
        // val = ((range-1) & peel(seed)) + lo ; seed' = next_seed(seed)
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::I64Const(1));
        f.instruction(&Instruction::I64Sub);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(self.random_peel_idx));
        f.instruction(&Instruction::I64And);
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I64Add);
        f.instruction(&Instruction::Call(self.box_int_idx));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(self.random_next_seed_idx));
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 2 });
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // rejection loop: threshold = (2^32 - range) % range ; cur = seed
        f.instruction(&Instruction::I64Const(M32));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::I64Sub);
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::I64RemU);
        f.instruction(&Instruction::LocalSet(10));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        // x = peel(cur)
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::Call(self.random_peel_idx));
        f.instruction(&Instruction::LocalSet(9));
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::LocalGet(10));
        f.instruction(&Instruction::I64LtU);
        f.instruction(&Instruction::If(BlockType::Empty));
        // reject: cur = next_seed(cur); retry
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::Call(self.random_next_seed_idx));
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::Br(1));
        f.instruction(&Instruction::End);
        // accept: ((x % range) + lo, next_seed(cur))
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::I64RemU);
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I64Add);
        f.instruction(&Instruction::Call(self.box_int_idx));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::Call(self.random_next_seed_idx));
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 2 });
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End); // loop
        f.instruction(&Instruction::End); // block
        f.instruction(&Instruction::End); // if tag==0
        f.instruction(&Instruction::Unreachable);
        f.instruction(&Instruction::End);
        f
    }

    /// task_run(task) -> Result x a : interpret a reified Task synchronously.
    /// Tags: 0 Succeed[x], 1 Fail[e], 2 Map[f,t], 3 AndThen[f,t], 4 MapError[f,t],
    /// 5 OnError[f,t], 6 Map2[f,ta,tb], 7 Map3[f,ta,tb,tc], 8 Sequence[list].
    /// (Async leaf tasks — sleep/now/http — are not modeled; those are compile
    /// errors, so every Task reaching here is synchronous.)
    fn emit_task_run(&self) -> Function {
        // param t(0). locals: ra(1),rb(2),rc(3):eqref; arr(4):ref T_ARR;
        //   i(5),len(6):i32; lst(7):eqref
        let mut f = Function::new([(3, eqref()), (1, ref_to(T_ARR)), (2, ValType::I32), (1, eqref())]);
        ctor_tag(&mut f, 0);
        // Succeed (0) → Ok(arg0) ; Fail (1) → Err(arg0)
        f.instruction(&Instruction::LocalTee(5)); // reuse i as tag scratch
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(0));
        ctor_arg0(&mut f, 0);
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 1 });
        f.instruction(&Instruction::StructNew(T_CTOR));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(1));
        ctor_arg0(&mut f, 0);
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 1 });
        f.instruction(&Instruction::StructNew(T_CTOR));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // Map (2): ra = run(arg1); Ok → Ok(f ra.v) else ra
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Const(2));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Empty));
        ctor_argn(&mut f, 0, 1);
        f.instruction(&Instruction::Call(self.task_run_idx));
        f.instruction(&Instruction::LocalSet(1));
        ctor_tag(&mut f, 1);
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        f.instruction(&Instruction::I32Const(0));
        ctor_arg0(&mut f, 0);
        ctor_arg0(&mut f, 1);
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 1 });
        f.instruction(&Instruction::StructNew(T_CTOR));
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // AndThen (3): ra = run(arg1); Ok → run(f ra.v) else ra
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Const(3));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Empty));
        ctor_argn(&mut f, 0, 1);
        f.instruction(&Instruction::Call(self.task_run_idx));
        f.instruction(&Instruction::LocalSet(1));
        ctor_tag(&mut f, 1);
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        ctor_arg0(&mut f, 0);
        ctor_arg0(&mut f, 1);
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&Instruction::Call(self.task_run_idx));
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // MapError (4): ra = run(arg1); Err → Err(f e) else ra
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Const(4));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Empty));
        ctor_argn(&mut f, 0, 1);
        f.instruction(&Instruction::Call(self.task_run_idx));
        f.instruction(&Instruction::LocalSet(1));
        ctor_tag(&mut f, 1);
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::I32Const(1));
        ctor_arg0(&mut f, 0);
        ctor_arg0(&mut f, 1);
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 1 });
        f.instruction(&Instruction::StructNew(T_CTOR));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // OnError (5): ra = run(arg1); Err → run(f e) else ra
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Const(5));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Empty));
        ctor_argn(&mut f, 0, 1);
        f.instruction(&Instruction::Call(self.task_run_idx));
        f.instruction(&Instruction::LocalSet(1));
        ctor_tag(&mut f, 1);
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Else);
        ctor_arg0(&mut f, 0);
        ctor_arg0(&mut f, 1);
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&Instruction::Call(self.task_run_idx));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // Map2 (6): ra=run(arg1); Err→ra; rb=run(arg2); Err→rb; Ok(f a b)
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Const(6));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Empty));
        ctor_argn(&mut f, 0, 1);
        f.instruction(&Instruction::Call(self.task_run_idx));
        f.instruction(&Instruction::LocalSet(1));
        ctor_tag(&mut f, 1);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        ctor_argn(&mut f, 0, 2);
        f.instruction(&Instruction::Call(self.task_run_idx));
        f.instruction(&Instruction::LocalSet(2));
        ctor_tag(&mut f, 2);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::I32Const(0));
        ctor_arg0(&mut f, 0);
        ctor_arg0(&mut f, 1);
        f.instruction(&Instruction::Call(self.apply1_idx));
        ctor_arg0(&mut f, 2);
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 1 });
        f.instruction(&Instruction::StructNew(T_CTOR));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // Map3 (7)
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Const(7));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Empty));
        ctor_argn(&mut f, 0, 1);
        f.instruction(&Instruction::Call(self.task_run_idx));
        f.instruction(&Instruction::LocalSet(1));
        ctor_tag(&mut f, 1);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        ctor_argn(&mut f, 0, 2);
        f.instruction(&Instruction::Call(self.task_run_idx));
        f.instruction(&Instruction::LocalSet(2));
        ctor_tag(&mut f, 2);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        ctor_argn(&mut f, 0, 3);
        f.instruction(&Instruction::Call(self.task_run_idx));
        f.instruction(&Instruction::LocalSet(3));
        ctor_tag(&mut f, 3);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::I32Const(0));
        ctor_arg0(&mut f, 0);
        ctor_arg0(&mut f, 1);
        f.instruction(&Instruction::Call(self.apply1_idx));
        ctor_arg0(&mut f, 2);
        f.instruction(&Instruction::Call(self.apply1_idx));
        ctor_arg0(&mut f, 3);
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 1 });
        f.instruction(&Instruction::StructNew(T_CTOR));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // Sequence (8): run each; first Err short-circuits; else Ok(list of values)
        ctor_arg0(&mut f, 0);
        f.instruction(&Instruction::LocalSet(7)); // lst
        list_len(&mut f, 7);
        f.instruction(&Instruction::LocalSet(6));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::ArrayNewDefault(T_ARR));
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        list_elem(&mut f, 7, 5);
        f.instruction(&Instruction::Call(self.task_run_idx));
        f.instruction(&Instruction::LocalSet(1));
        ctor_tag(&mut f, 1);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(5));
        ctor_arg0(&mut f, 1);
        f.instruction(&Instruction::ArraySet(T_ARR));
        bump(&mut f, 5, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        // Ok(T_LIST{len, T_BACK{0, arr}})
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::StructNew(T_BACK));
        f.instruction(&Instruction::StructNew(T_LIST));
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 1 });
        f.instruction(&Instruction::StructNew(T_CTOR));
        f.instruction(&Instruction::End);
        f
    }

    /// str_join(sep, list) : concatenate strings with `sep` between them.
    fn emit_str_join(&self) -> Function {
        // Two-pass O(n) join: sum lengths, allocate once, array.copy each piece.
        // (A naive acc = acc ++ item loop is O(n²) — it re-copied the whole
        // accumulator every step, which made String.join catastrophically slow.)
        // params sep(0), list(1). locals: len(2),total(3),i(4),off(5),seplen(6),
        //   start(7),itemlen(8):i32; data(9):ref T_ARR; out(10),item(11):ref T_STR
        let mut f = Function::new([(7, ValType::I32), (1, ref_to(T_ARR)), (2, ref_to(T_STR))]);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_STR));
        f.instruction(&Instruction::ArrayLen);
        f.instruction(&Instruction::LocalSet(6)); // seplen
        list_len(&mut f, 1);
        f.instruction(&Instruction::LocalSet(2)); // len
        list_data(&mut f, 1);
        f.instruction(&Instruction::LocalSet(9)); // data
        list_start(&mut f, 1);
        f.instruction(&Instruction::LocalSet(7)); // start
        // item accessor: data[start + i] as T_STR (leaves it on the stack)
        let item = |f: &mut Function| {
            f.instruction(&Instruction::LocalGet(9));
            f.instruction(&Instruction::LocalGet(7));
            f.instruction(&Instruction::LocalGet(4));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::ArrayGet(T_ARR));
            f.instruction(&cast_to(T_STR));
        };
        // pass 1: total = Σ len(item) + seplen*(len-1)
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(3)); // total
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(4)); // i
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(3));
        item(&mut f);
        f.instruction(&Instruction::ArrayLen);
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(3));
        bump(&mut f, 4, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        // + separators (len > 0 ? seplen*(len-1) : 0)
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32GtS);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::I32Mul);
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::End);
        // pass 2: allocate and copy
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::ArrayNewDefault(T_STR));
        f.instruction(&Instruction::LocalSet(10)); // out
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(5)); // off
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(4)); // i
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        // if i > 0: copy sep at off, off += seplen
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32GtS);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(10));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_STR));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::ArrayCopy { array_type_index_dst: T_STR, array_type_index_src: T_STR });
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::End);
        // copy item at off, off += itemlen
        item(&mut f);
        f.instruction(&Instruction::LocalSet(11));
        f.instruction(&Instruction::LocalGet(11));
        f.instruction(&Instruction::ArrayLen);
        f.instruction(&Instruction::LocalSet(8)); // itemlen
        f.instruction(&Instruction::LocalGet(10));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(11));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::ArrayCopy { array_type_index_dst: T_STR, array_type_index_src: T_STR });
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(5));
        bump(&mut f, 4, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(10));
        f.instruction(&Instruction::End);
        f
    }

    /// A float element of a 2-tuple expr in `local` (index i), unboxed to f64.
    fn tuple_f64(&self, f: &mut Function, local: u32, i: i32) {
        f.instruction(&Instruction::LocalGet(local));
        f.instruction(&cast_to(T_ARR));
        f.instruction(&Instruction::I32Const(i));
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&cast_to(T_FLOAT));
        f.instruction(&Instruction::StructGet { struct_type_index: T_FLOAT, field_index: 0 });
    }

    /// fromPolar (r, theta) = (r*cos theta, r*sin theta).
    fn emit_from_polar(&self) -> Function {
        let mut f = Function::new([]); // param t(0): the (r, theta) tuple
        self.tuple_f64(&mut f, 0, 0); // r
        self.tuple_f64(&mut f, 0, 1); // theta
        f.instruction(&Instruction::Call(MATH_COS));
        f.instruction(&Instruction::F64Mul);
        f.instruction(&Instruction::StructNew(T_FLOAT));
        self.tuple_f64(&mut f, 0, 0); // r
        self.tuple_f64(&mut f, 0, 1); // theta
        f.instruction(&Instruction::Call(MATH_SIN));
        f.instruction(&Instruction::F64Mul);
        f.instruction(&Instruction::StructNew(T_FLOAT));
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 2 });
        f.instruction(&Instruction::End);
        f
    }

    /// toPolar (x, y) = (sqrt(x*x + y*y), atan2 y x).
    fn emit_to_polar(&self) -> Function {
        let mut f = Function::new([]); // param t(0): the (x, y) tuple
        // r
        self.tuple_f64(&mut f, 0, 0);
        self.tuple_f64(&mut f, 0, 0);
        f.instruction(&Instruction::F64Mul);
        self.tuple_f64(&mut f, 0, 1);
        self.tuple_f64(&mut f, 0, 1);
        f.instruction(&Instruction::F64Mul);
        f.instruction(&Instruction::F64Add);
        f.instruction(&Instruction::F64Sqrt);
        f.instruction(&Instruction::StructNew(T_FLOAT));
        // theta = atan2(y, x)
        self.tuple_f64(&mut f, 0, 1);
        self.tuple_f64(&mut f, 0, 0);
        f.instruction(&Instruction::Call(MATH_ATAN2));
        f.instruction(&Instruction::StructNew(T_FLOAT));
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 2 });
        f.instruction(&Instruction::End);
        f
    }

    /// str_slice(start, end, s) : substring over code-point indices `[a, b)`,
    /// with Elm's negative-from-end + clamping, built from dropLeft + left.
    fn emit_str_slice(&self) -> Function {
        // params start(0), end(1), s(2). locals: len(3),a(4),b(5):i32
        let mut f = Function::new([(3, ValType::I32)]);
        // len = str_length(s)
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::Call(self.str_length_idx));
        f.instruction(&Instruction::Call(self.unbox_int_idx));
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::LocalSet(3));
        // normalize each index into a/b
        let norm = |f: &mut Function, src: u32, dst: u32| {
            f.instruction(&Instruction::LocalGet(src));
            f.instruction(&Instruction::Call(self.unbox_int_idx));
            f.instruction(&Instruction::I32WrapI64);
            f.instruction(&Instruction::LocalSet(dst));
            // if < 0: += len
            f.instruction(&Instruction::LocalGet(dst));
            f.instruction(&Instruction::I32Const(0));
            f.instruction(&Instruction::I32LtS);
            f.instruction(&Instruction::If(BlockType::Empty));
            f.instruction(&Instruction::LocalGet(3));
            f.instruction(&Instruction::LocalGet(dst));
            f.instruction(&Instruction::I32Add);
            f.instruction(&Instruction::LocalSet(dst));
            f.instruction(&Instruction::End);
            // clamp [0, len]
            f.instruction(&Instruction::LocalGet(dst));
            f.instruction(&Instruction::I32Const(0));
            f.instruction(&Instruction::I32LtS);
            f.instruction(&Instruction::If(BlockType::Empty));
            f.instruction(&Instruction::I32Const(0));
            f.instruction(&Instruction::LocalSet(dst));
            f.instruction(&Instruction::End);
            f.instruction(&Instruction::LocalGet(dst));
            f.instruction(&Instruction::LocalGet(3));
            f.instruction(&Instruction::I32GtS);
            f.instruction(&Instruction::If(BlockType::Empty));
            f.instruction(&Instruction::LocalGet(3));
            f.instruction(&Instruction::LocalSet(dst));
            f.instruction(&Instruction::End);
        };
        norm(&mut f, 0, 4);
        norm(&mut f, 1, 5);
        // if a >= b: ""
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::If(BlockType::Empty));
        push_str_const(&mut f, "");
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // left(b-a, dropLeft(a, s))
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::I64ExtendI32S);
        f.instruction(&Instruction::Call(self.box_int_idx));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I64ExtendI32S);
        f.instruction(&Instruction::Call(self.box_int_idx));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::Call(self.str_dropleft_idx));
        f.instruction(&Instruction::Call(self.str_left_idx));
        f.instruction(&Instruction::End);
        f
    }

    /// str_pad_both(n, ch, s) : center `s` to width `n` — left gets floor of the
    /// deficit, right the rest (Elm's `String.pad`), via padLeft then padRight.
    fn emit_str_pad_both(&self) -> Function {
        // params n(0), ch(1), s(2). locals: len(3),left(4):i32
        let mut f = Function::new([(2, ValType::I32)]);
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::Call(self.str_length_idx));
        f.instruction(&Instruction::Call(self.unbox_int_idx));
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::LocalSet(3)); // len
        // leftTarget = len + (n - len)/2
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::Call(self.unbox_int_idx));
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::I32Const(2));
        f.instruction(&Instruction::I32DivS);
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(4));
        // padRight(n, ch, padLeft(leftTarget, ch, s))
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I64ExtendI32S);
        f.instruction(&Instruction::Call(self.box_int_idx));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::Call(self.str_pad_left_idx));
        f.instruction(&Instruction::Call(self.str_pad_right_idx));
        f.instruction(&Instruction::End);
        f
    }

    /// str_repeat(n, s) : `s` concatenated `n` times. O(n·|s|): allocate the
    /// full result once and array.copy `s` into each slot (a naive acc ++ s loop
    /// would be O(n²)).
    fn emit_str_repeat(&self) -> Function {
        // n(0), s(1); n32(2),i(3),slen(4),off(5):i32; sc(6),out(7):ref T_STR
        let mut f = Function::new([(4, ValType::I32), (2, ref_to(T_STR))]);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::Call(self.unbox_int_idx));
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::LocalSet(2));
        // clamp n to >= 0
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_STR));
        f.instruction(&Instruction::LocalSet(6)); // sc
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::ArrayLen);
        f.instruction(&Instruction::LocalSet(4)); // slen
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Mul);
        f.instruction(&Instruction::ArrayNewDefault(T_STR));
        f.instruction(&Instruction::LocalSet(7)); // out
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(5)); // off
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(3)); // i
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::ArrayCopy { array_type_index_dst: T_STR, array_type_index_src: T_STR });
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(5));
        bump(&mut f, 3, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::End);
        f
    }

    /// str_affix(a, s) : byte-correct startsWith (suffix=false) / endsWith
    /// (suffix=true). Valid over UTF-8 since it compares whole affixes.
    fn emit_str_affix(&self, suffix: bool) -> Function {
        // a(0), s(1); al(2), sl(3), i(4), off(5): i32
        let mut f = Function::new([(4, ValType::I32)]);
        let false_ret = |f: &mut Function| {
            f.instruction(&Instruction::I32Const(0));
            f.instruction(&Instruction::RefI31);
            f.instruction(&Instruction::Return);
        };
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_STR));
        f.instruction(&Instruction::ArrayLen);
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_STR));
        f.instruction(&Instruction::ArrayLen);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32GtS);
        f.instruction(&Instruction::If(BlockType::Empty));
        false_ret(&mut f);
        f.instruction(&Instruction::End);
        if suffix {
            f.instruction(&Instruction::LocalGet(3));
            f.instruction(&Instruction::LocalGet(2));
            f.instruction(&Instruction::I32Sub);
        } else {
            f.instruction(&Instruction::I32Const(0));
        }
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_STR));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::ArrayGetU(T_STR));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_STR));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::ArrayGetU(T_STR));
        f.instruction(&Instruction::I32Ne);
        f.instruction(&Instruction::If(BlockType::Empty));
        false_ret(&mut f);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::RefI31);
        f.instruction(&Instruction::End);
        f
    }

    /// val_compare(a, b) -> i32 (-1/0/1): ordering over comparables (Int, Float,
    /// Char, String, List, Tuple), lexicographic for sequences.
    fn emit_val_compare(&self) -> Function {
        // a(0), b(1); i(2),la(3),lb(4),c(5): i32; ai(6),bi(7): i64; af(8),bf(9): f64
        let mut f = Function::new([(4, ValType::I32), (2, ValType::I64), (2, ValType::F64)]);
        let sign_i = |f: &mut Function, x: u32, y: u32, i64ty: bool| {
            f.instruction(&Instruction::LocalGet(x));
            f.instruction(&Instruction::LocalGet(y));
            f.instruction(if i64ty { &Instruction::I64GtS } else { &Instruction::I32GtS });
            f.instruction(&Instruction::LocalGet(x));
            f.instruction(&Instruction::LocalGet(y));
            f.instruction(if i64ty { &Instruction::I64LtS } else { &Instruction::I32LtS });
            f.instruction(&Instruction::I32Sub);
            f.instruction(&Instruction::Return);
        };
        // nil ordering
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::RefIsNull);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::RefIsNull);
        f.instruction(&Instruction::If(BlockType::Result(ValType::I32)));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::I32Const(-1));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::RefIsNull);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // i31 (Char/Bool/Unit, or a small Int) — unbox BOTH as i64 so a small
        // i31 Int orders correctly against a large boxed-T_INT Int.
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::RefTestNonNull(HeapType::Abstract { shared: false, ty: AbstractHeapType::I31 }));
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::Call(self.unbox_int_idx));
        f.instruction(&Instruction::LocalSet(6));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(self.unbox_int_idx));
        f.instruction(&Instruction::LocalSet(7));
        sign_i(&mut f, 6, 7, true);
        f.instruction(&Instruction::End);
        // T_INT (large Int)
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::RefTestNonNull(HeapType::Concrete(T_INT)));
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::Call(self.unbox_int_idx));
        f.instruction(&Instruction::LocalSet(6));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(self.unbox_int_idx));
        f.instruction(&Instruction::LocalSet(7));
        sign_i(&mut f, 6, 7, true);
        f.instruction(&Instruction::End);
        // T_FLOAT
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::RefTestNonNull(HeapType::Concrete(T_FLOAT)));
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_FLOAT));
        f.instruction(&Instruction::StructGet { struct_type_index: T_FLOAT, field_index: 0 });
        f.instruction(&Instruction::LocalSet(8));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_FLOAT));
        f.instruction(&Instruction::StructGet { struct_type_index: T_FLOAT, field_index: 0 });
        f.instruction(&Instruction::LocalSet(9));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::F64Gt);
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::F64Lt);
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // T_STR: lexicographic bytes
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::RefTestNonNull(HeapType::Concrete(T_STR)));
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_STR));
        f.instruction(&Instruction::ArrayLen);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_STR));
        f.instruction(&Instruction::ArrayLen);
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        // i < la && i < lb
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        // x=a[i], y=b[i]
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_STR));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::ArrayGetU(T_STR));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_STR));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::ArrayGetU(T_STR));
        f.instruction(&Instruction::I32Ne);
        f.instruction(&Instruction::If(BlockType::Empty));
        // return sign(a[i], b[i])
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_STR));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::ArrayGetU(T_STR));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_STR));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::ArrayGetU(T_STR));
        f.instruction(&Instruction::I32GtU);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_STR));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::ArrayGetU(T_STR));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_STR));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::ArrayGetU(T_STR));
        f.instruction(&Instruction::I32LtU);
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        // lengths
        sign_i(&mut f, 3, 4, false);
        f.instruction(&Instruction::End);
        // T_LIST: lexicographic over elements, then by length
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::RefTestNonNull(HeapType::Concrete(T_LIST)));
        f.instruction(&Instruction::If(BlockType::Empty));
        list_len(&mut f, 0);
        f.instruction(&Instruction::LocalSet(3)); // la
        list_len(&mut f, 1);
        f.instruction(&Instruction::LocalSet(4)); // lb
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(2)); // i
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::I32Or);
        f.instruction(&Instruction::BrIf(1));
        list_elem(&mut f, 0, 2);
        list_elem(&mut f, 1, 2);
        f.instruction(&Instruction::Call(self.val_compare_idx));
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        bump(&mut f, 2, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        // shorter list sorts first: sign(la - lb)
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32GtS);
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // T_ARR (tuples): elementwise
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::RefTestNonNull(HeapType::Concrete(T_ARR)));
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_ARR));
        f.instruction(&Instruction::ArrayLen);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_ARR));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_ARR));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::Call(self.val_compare_idx));
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // fallback
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::End);
        f
    }

    /// list_insert(x, sorted) : insert `x` into an ascending-sorted list.
    fn emit_list_insert(&self) -> Function {
        let mut f = Function::new([]);
        list_is_empty(&mut f, 1);
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        // [x]
        f.instruction(&Instruction::LocalGet(0));
        push_empty_list(&mut f);
        f.instruction(&Instruction::Call(self.list_cons_idx));
        f.instruction(&Instruction::Else);
        // compare(x, head) <= 0 ? cons(x, sorted) : cons(head, insert(x, tail))
        f.instruction(&Instruction::LocalGet(0));
        list_head(&mut f, 1);
        f.instruction(&Instruction::Call(self.val_compare_idx));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32LeS);
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(self.list_cons_idx));
        f.instruction(&Instruction::Else);
        list_head(&mut f, 1);
        f.instruction(&Instruction::LocalGet(0));
        list_tail(&mut f, 1);
        f.instruction(&Instruction::Call(self.list_insert_idx));
        f.instruction(&Instruction::Call(self.list_cons_idx));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f
    }

    /// list_sort(xs) : O(n log n) merge sort using val_compare. Copies the
    /// elements into a fresh contiguous backing, sorts, and re-wraps.
    fn emit_list_sort(&self) -> Function {
        // param list(0). locals: n(1),start(2):i32; arr(3),buf(4),data(5):ref T_ARR
        let mut f = Function::new([(2, ValType::I32), (3, ref_to(T_ARR))]);
        list_len(&mut f, 0);
        f.instruction(&Instruction::LocalSet(1));
        list_data(&mut f, 0);
        f.instruction(&Instruction::LocalSet(5));
        list_start(&mut f, 0);
        f.instruction(&Instruction::LocalSet(2));
        // arr = fresh[n]; arr[0..n] <- data[start..start+n]
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::ArrayNewDefault(T_ARR));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::ArrayCopy { array_type_index_dst: T_ARR, array_type_index_src: T_ARR });
        // buf = fresh[n]; msort(arr, buf, 0, n)
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::ArrayNewDefault(T_ARR));
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(self.msort_idx));
        // T_LIST{n, T_BACK{0, arr}}
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::StructNew(T_BACK));
        f.instruction(&Instruction::StructNew(T_LIST));
        f.instruction(&Instruction::End);
        f
    }

    /// list_sort_with(cmp, list) : stable merge sort using the user comparator
    /// `cmp : a -> a -> Order`. The comparator is stashed in G_SORT_CMP for the
    /// duration (saved/restored so a comparator that itself sorts still works).
    fn emit_list_sort_with(&self) -> Function {
        // params cmp(0), list(1). locals: n(2),start(3):i32;
        //   arr(4),buf(5),data(6):ref T_ARR; oldcmp(7):eqref
        let mut f = Function::new([(2, ValType::I32), (3, ref_to(T_ARR)), (1, eqref())]);
        // save + install the comparator
        f.instruction(&Instruction::GlobalGet(G_SORT_CMP));
        f.instruction(&Instruction::LocalSet(7));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::GlobalSet(G_SORT_CMP));
        list_len(&mut f, 1);
        f.instruction(&Instruction::LocalSet(2));
        list_data(&mut f, 1);
        f.instruction(&Instruction::LocalSet(6));
        list_start(&mut f, 1);
        f.instruction(&Instruction::LocalSet(3));
        // arr = fresh[n]; arr[0..n] <- data[start..start+n]
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::ArrayNewDefault(T_ARR));
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::ArrayCopy { array_type_index_dst: T_ARR, array_type_index_src: T_ARR });
        // buf = fresh[n]; msort_cmp(arr, buf, 0, n)
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::ArrayNewDefault(T_ARR));
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::Call(self.msort_cmp_idx));
        // restore the previous comparator
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::GlobalSet(G_SORT_CMP));
        // T_LIST{n, T_BACK{0, arr}}
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::StructNew(T_BACK));
        f.instruction(&Instruction::StructNew(T_LIST));
        f.instruction(&Instruction::End);
        f
    }

    /// list_all/any(pred, xs) : whether `pred` holds for all / any elements.
    fn emit_list_all_any(&self, all: bool) -> Function {
        // params pred(0), xs(1). locals: len(2), start(3), i(4):i32, data(5):ref T_ARR
        let mut f = Function::new([(3, ValType::I32), (1, ref_to(T_ARR))]);
        list_len(&mut f, 1);
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::If(BlockType::Empty));
        list_data(&mut f, 1);
        f.instruction(&Instruction::LocalSet(5));
        list_start(&mut f, 1);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&Instruction::RefCastNonNull(HeapType::Abstract { shared: false, ty: AbstractHeapType::I31 }));
        f.instruction(&Instruction::I31GetS);
        if all {
            f.instruction(&Instruction::I32Eqz);
        }
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(if all { 0 } else { 1 }));
        f.instruction(&Instruction::RefI31);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        bump(&mut f, 4, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::I32Const(if all { 1 } else { 0 }));
        f.instruction(&Instruction::RefI31);
        f.instruction(&Instruction::End);
        f
    }

    /// list_min/max(xs) : Maybe (least / greatest element) by val_compare.
    fn emit_list_min_max(&self, max: bool) -> Function {
        // params xs(0). locals: len(1), start(2), i(3):i32, data(4):ref T_ARR,
        //   best(5):eqref
        let mut f = Function::new([(3, ValType::I32), (1, ref_to(T_ARR)), (1, eqref())]);
        list_len(&mut f, 0);
        f.instruction(&Instruction::LocalSet(1));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::RefNull(HeapType::Concrete(T_ARR)));
        f.instruction(&Instruction::StructNew(T_CTOR));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        list_data(&mut f, 0);
        f.instruction(&Instruction::LocalSet(4));
        list_start(&mut f, 0);
        f.instruction(&Instruction::LocalSet(2));
        // best = data[start]
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        // e = data[start+i]; if compare(e, best) (max:>0/min:<0) best = e
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::Call(self.val_compare_idx));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(if max { &Instruction::I32GtS } else { &Instruction::I32LtS });
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::End);
        bump(&mut f, 3, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 1 });
        f.instruction(&Instruction::StructNew(T_CTOR));
        f.instruction(&Instruction::End);
        f
    }

    /// list_indexedMap(f, base, xs) : map with element index (from `base`).
    fn emit_list_indexed_map(&self) -> Function {
        // params f(0), base(1), xs(2). locals: len(3), start(4), i(5), base_i(6):i32,
        //   xdata(7), ndata(8):ref T_ARR
        let mut f = Function::new([(4, ValType::I32), (2, ref_to(T_ARR))]);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(self.unbox_int_idx));
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::LocalSet(6));
        list_len(&mut f, 2);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::ArrayNewDefault(T_ARR));
        f.instruction(&Instruction::LocalSet(8));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::If(BlockType::Empty));
        list_data(&mut f, 2);
        f.instruction(&Instruction::LocalSet(7));
        list_start(&mut f, 2);
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        // ndata[i] = f (box(base+i)) (xdata[start+i])
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I64ExtendI32S);
        f.instruction(&Instruction::Call(self.box_int_idx));
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&Instruction::ArraySet(T_ARR));
        bump(&mut f, 5, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::StructNew(T_BACK));
        f.instruction(&Instruction::StructNew(T_LIST));
        f.instruction(&Instruction::End);
        f
    }

    /// list_sum/product(xs) : numeric fold over Int or Float (dispatch on the
    /// first element); empty yields the Int identity (0 / 1), matching Elm.
    fn emit_list_sum_prod(&self, product: bool) -> Function {
        let ident = if product { 1 } else { 0 };
        // params xs(0). locals: len(1),start(2),i(3):i32, data(4):ref T_ARR,
        //   iacc(5):i64, facc(6):f64
        let mut f = Function::new([
            (3, ValType::I32),
            (1, ref_to(T_ARR)),
            (1, ValType::I64),
            (1, ValType::F64),
        ]);
        list_len(&mut f, 0);
        f.instruction(&Instruction::LocalSet(1));
        // empty -> box Int identity
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I64Const(ident));
        f.instruction(&Instruction::Call(self.box_int_idx));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        list_data(&mut f, 0);
        f.instruction(&Instruction::LocalSet(4));
        list_start(&mut f, 0);
        f.instruction(&Instruction::LocalSet(2));
        // float path if data[start] is a Float
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::RefTestNonNull(HeapType::Concrete(T_FLOAT)));
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::F64Const((ident as f64).into()));
        f.instruction(&Instruction::LocalSet(6));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&cast_to(T_FLOAT));
        f.instruction(&Instruction::StructGet { struct_type_index: T_FLOAT, field_index: 0 });
        f.instruction(if product { &Instruction::F64Mul } else { &Instruction::F64Add });
        f.instruction(&Instruction::LocalSet(6));
        bump(&mut f, 3, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::StructNew(T_FLOAT));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // int path
        f.instruction(&Instruction::I64Const(ident));
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::Call(self.unbox_int_idx));
        f.instruction(if product { &Instruction::I64Mul } else { &Instruction::I64Add });
        f.instruction(&Instruction::LocalSet(5));
        bump(&mut f, 3, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::Call(self.box_int_idx));
        f.instruction(&Instruction::End);
        f
    }

    /// list_map2(f, xs, ys) : zip-map, stopping at the shorter list.
    fn emit_list_map2(&self) -> Function {
        // params f(0), xs(1), ys(2). locals: n(3),xs0(4),ys0(5),i(6):i32,
        //   xd(7),yd(8),nd(9):ref T_ARR
        let mut f = Function::new([(4, ValType::I32), (3, ref_to(T_ARR))]);
        // n = min(len xs, len ys)
        list_len(&mut f, 1);
        f.instruction(&Instruction::LocalSet(3));
        list_len(&mut f, 2);
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::If(BlockType::Empty));
        list_len(&mut f, 2);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::ArrayNewDefault(T_ARR));
        f.instruction(&Instruction::LocalSet(9));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::If(BlockType::Empty));
        list_data(&mut f, 1);
        f.instruction(&Instruction::LocalSet(7));
        list_data(&mut f, 2);
        f.instruction(&Instruction::LocalSet(8));
        list_start(&mut f, 1);
        f.instruction(&Instruction::LocalSet(4));
        list_start(&mut f, 2);
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(6));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&Instruction::ArraySet(T_ARR));
        bump(&mut f, 6, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::StructNew(T_BACK));
        f.instruction(&Instruction::StructNew(T_LIST));
        f.instruction(&Instruction::End);
        f
    }

    /// str_upper/lower(s) : ASCII-only case fold (matches JS for ASCII; other
    /// code points pass through unchanged, so non-ASCII parity is partial).
    fn emit_str_case(&self, upper: bool) -> Function {
        // locals: sstr(1):str, n(2), i(3), c(4):i32, out(5):str
        let mut f = Function::new([
            (1, ref_to(T_STR)),
            (3, ValType::I32),
            (1, ref_to(T_STR)),
        ]);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_STR));
        f.instruction(&Instruction::LocalSet(1));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::ArrayLen);
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::ArrayNewDefault(T_STR));
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::ArrayGetU(T_STR));
        f.instruction(&Instruction::LocalSet(4));
        // ASCII range test
        let (lo, hi, delta): (i32, i32, i32) =
            if upper { (97, 122, -32) } else { (65, 90, 32) };
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(lo));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(hi));
        f.instruction(&Instruction::I32LeS);
        f.instruction(&Instruction::I32And);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(delta));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::ArraySet(T_STR));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::End);
        f
    }

    /// str_trim(s) : drop leading/trailing ASCII whitespace (space/tab/CR/LF).
    fn emit_str_trim(&self) -> Function {
        // locals: sstr(1):str, len(2), start(3), end(4), i(5), c(6):i32, out(7):str
        let mut f = Function::new([
            (1, ref_to(T_STR)),
            (5, ValType::I32),
            (1, ref_to(T_STR)),
        ]);
        // is-whitespace test for the byte in local 6, leaving an i32 bool
        let push_is_ws = |f: &mut Function| {
            f.instruction(&Instruction::LocalGet(6));
            f.instruction(&Instruction::I32Const(32));
            f.instruction(&Instruction::I32Eq);
            for ws in [9, 10, 13] {
                f.instruction(&Instruction::LocalGet(6));
                f.instruction(&Instruction::I32Const(ws));
                f.instruction(&Instruction::I32Eq);
                f.instruction(&Instruction::I32Or);
            }
        };
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_STR));
        f.instruction(&Instruction::LocalSet(1));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::ArrayLen);
        f.instruction(&Instruction::LocalSet(2));
        // advance start past whitespace
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::ArrayGetU(T_STR));
        f.instruction(&Instruction::LocalSet(6));
        push_is_ws(&mut f);
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        // pull end back past whitespace
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32LeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::ArrayGetU(T_STR));
        f.instruction(&Instruction::LocalSet(6));
        push_is_ws(&mut f);
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        // out = new(end - start); copy
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::ArrayNewDefault(T_STR));
        f.instruction(&Instruction::LocalSet(7));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::ArrayGetU(T_STR));
        f.instruction(&Instruction::ArraySet(T_STR));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::End);
        f
    }

    /// str_range(n, s) : byte-range slice implementing left/right/dropLeft/
    /// dropRight (ASCII-correct; byte-indexed, so non-ASCII parity is partial).
    /// The half-open range [a, b) is `from_end`/`drop`-selected then clamped.
    fn emit_str_range(&self, from_end: bool, drop: bool) -> Function {
        // params: n(0), s(1). locals: sstr(2):str, nval(3), len(4), a(5), b(6), i(7):i32, out(8):str
        let mut f = Function::new([
            (1, ref_to(T_STR)),
            (5, ValType::I32),
            (1, ref_to(T_STR)),
        ]);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_STR));
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::ArrayLen);
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::Call(self.unbox_int_idx));
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::LocalSet(3));
        // a
        match (from_end, drop) {
            (false, _) => {
                f.instruction(&Instruction::I32Const(0)); // left / dropRight
            }
            (true, false) => {
                f.instruction(&Instruction::LocalGet(4)); // right: len - n
                f.instruction(&Instruction::LocalGet(3));
                f.instruction(&Instruction::I32Sub);
            }
            (true, true) => {
                f.instruction(&Instruction::LocalGet(3)); // dropLeft: n
            }
        }
        f.instruction(&Instruction::LocalSet(5));
        // b
        match (from_end, drop) {
            (false, false) => {
                f.instruction(&Instruction::LocalGet(3)); // left: n
            }
            (false, true) => {
                f.instruction(&Instruction::LocalGet(4)); // dropRight: len - n
                f.instruction(&Instruction::LocalGet(3));
                f.instruction(&Instruction::I32Sub);
            }
            (true, _) => {
                f.instruction(&Instruction::LocalGet(4)); // right / dropLeft: len
            }
        }
        f.instruction(&Instruction::LocalSet(6));
        // clamp a and b into [0, len]; then a = min(a, b)
        for slot in [5u32, 6] {
            f.instruction(&Instruction::LocalGet(slot));
            f.instruction(&Instruction::LocalGet(4));
            f.instruction(&Instruction::I32GtS);
            f.instruction(&Instruction::If(BlockType::Empty));
            f.instruction(&Instruction::LocalGet(4));
            f.instruction(&Instruction::LocalSet(slot));
            f.instruction(&Instruction::End);
            f.instruction(&Instruction::LocalGet(slot));
            f.instruction(&Instruction::I32Const(0));
            f.instruction(&Instruction::I32LtS);
            f.instruction(&Instruction::If(BlockType::Empty));
            f.instruction(&Instruction::I32Const(0));
            f.instruction(&Instruction::LocalSet(slot));
            f.instruction(&Instruction::End);
        }
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I32GtS);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::End);
        // out = new(b - a); copy sstr[a + i]
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::ArrayNewDefault(T_STR));
        f.instruction(&Instruction::LocalSet(8));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(7));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::ArrayGetU(T_STR));
        f.instruction(&Instruction::ArraySet(T_STR));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(7));
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::End);
        f
    }

    /// str_to_int(s) : parse to `Maybe Int` (optional leading +/-, then ASCII
    /// digits; anything else yields Nothing). Matches Elm's String.toInt.
    fn emit_str_to_int(&self) -> Function {
        // locals: sstr(1):str, len(2), i(3), neg(4), c(5):i32, acc(6):i64
        let mut f = Function::new([
            (1, ref_to(T_STR)),
            (4, ValType::I32),
            (1, ValType::I64),
        ]);
        // Nothing = ctor tag 1, null args
        let nothing = |f: &mut Function| {
            f.instruction(&Instruction::I32Const(1));
            f.instruction(&Instruction::RefNull(HeapType::Concrete(T_ARR)));
            f.instruction(&Instruction::StructNew(T_CTOR));
        };
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_STR));
        f.instruction(&Instruction::LocalSet(1));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::ArrayLen);
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Empty));
        nothing(&mut f);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(4));
        // sign
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::ArrayGetU(T_STR));
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Const(45)); // '-'
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Const(43)); // '+'
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        // a lone sign is not a number
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::If(BlockType::Empty));
        nothing(&mut f);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::I64Const(0));
        f.instruction(&Instruction::LocalSet(6));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::ArrayGetU(T_STR));
        f.instruction(&Instruction::LocalSet(5));
        // non-digit → Nothing
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Const(48));
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Const(57));
        f.instruction(&Instruction::I32GtS);
        f.instruction(&Instruction::I32Or);
        f.instruction(&Instruction::If(BlockType::Empty));
        nothing(&mut f);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // acc = acc*10 + (c - 48)
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I64Const(10));
        f.instruction(&Instruction::I64Mul);
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Const(48));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::I64ExtendI32S);
        f.instruction(&Instruction::I64Add);
        f.instruction(&Instruction::LocalSet(6));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        // apply sign
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I64Const(0));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I64Sub);
        f.instruction(&Instruction::LocalSet(6));
        f.instruction(&Instruction::End);
        // Just acc
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::Call(self.box_int_idx));
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 1 });
        f.instruction(&Instruction::StructNew(T_CTOR));
        f.instruction(&Instruction::End);
        f
    }

    /// str_contains(sub, s) : whether `s` contains `sub` (naive byte search).
    fn emit_str_contains(&self) -> Function {
        // params: sub(0), s(1). locals: substr(2):str, sstr(3):str,
        // sublen(4), len(5), start(6), j(7):i32
        let mut f = Function::new([(2, ref_to(T_STR)), (4, ValType::I32)]);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_STR));
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_STR));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::ArrayLen);
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::ArrayLen);
        f.instruction(&Instruction::LocalSet(5));
        // empty needle → true
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::RefI31);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(6));
        f.instruction(&Instruction::Block(BlockType::Empty)); // outer_block (depth target 1 from outer_loop)
        f.instruction(&Instruction::Loop(BlockType::Empty)); // outer_loop
        // if start + sublen > len → break outer
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32GtS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::Block(BlockType::Empty)); // inner_block
        f.instruction(&Instruction::Loop(BlockType::Empty)); // inner_loop
        // all matched → true
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::RefI31);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // substr[j] != sstr[start+j] → break inner (mismatch)
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::ArrayGetU(T_STR));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::ArrayGetU(T_STR));
        f.instruction(&Instruction::I32Ne);
        f.instruction(&Instruction::BrIf(1)); // break inner_block
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(7));
        f.instruction(&Instruction::Br(0)); // inner_loop
        f.instruction(&Instruction::End); // inner_loop
        f.instruction(&Instruction::End); // inner_block
        // mismatch: reset j, advance start
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(7));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(6));
        f.instruction(&Instruction::Br(0)); // outer_loop
        f.instruction(&Instruction::End); // outer_loop
        f.instruction(&Instruction::End); // outer_block
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::RefI31);
        f.instruction(&Instruction::End);
        f
    }

    /// str_to_list(s) : String -> List Char. Two passes: count code points,
    /// allocate, then fill head-first.
    fn emit_str_to_list(&self) -> Function {
        // params s(0). locals: sstr(1):str, blen(2),i(3),cp(4),adv(5),count(6),
        //   k(7):i32, data(8):ref T_ARR
        let mut f = Function::new([(1, ref_to(T_STR)), (6, ValType::I32), (1, ref_to(T_ARR))]);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_STR));
        f.instruction(&Instruction::LocalSet(1));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::ArrayLen);
        f.instruction(&Instruction::LocalSet(2));
        // pass 1: count
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(6));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        utf8_decode(&mut f, 1, 3, 4, 5);
        bump(&mut f, 6, 1);
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        // pass 2: fill
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::ArrayNewDefault(T_ARR));
        f.instruction(&Instruction::LocalSet(8));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(7));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        utf8_decode(&mut f, 1, 3, 4, 5);
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::RefI31);
        f.instruction(&Instruction::ArraySet(T_ARR));
        bump(&mut f, 7, 1);
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::StructNew(T_BACK));
        f.instruction(&Instruction::StructNew(T_LIST));
        f.instruction(&Instruction::End);
        f
    }

    /// str_from_list(chars) : List Char -> String. Two passes: sum byte widths,
    /// then encode.
    fn emit_str_from_list(&self) -> Function {
        // params chars(0). locals: len(1),total(2),off(3),cp(4),bl(5),i(6),
        //   start(7):i32, xdata(8):ref T_ARR, out(9):str
        let mut f = Function::new([(7, ValType::I32), (1, ref_to(T_ARR)), (1, ref_to(T_STR))]);
        list_len(&mut f, 0);
        f.instruction(&Instruction::LocalSet(1));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::If(BlockType::Empty));
        list_data(&mut f, 0);
        f.instruction(&Instruction::LocalSet(8));
        list_start(&mut f, 0);
        f.instruction(&Instruction::LocalSet(7));
        // pass 1: total bytes
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(6));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::RefCastNonNull(HeapType::Abstract { shared: false, ty: AbstractHeapType::I31 }));
        f.instruction(&Instruction::I31GetS);
        f.instruction(&Instruction::LocalSet(4));
        utf8_byte_len(&mut f, 4, 5);
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(2));
        bump(&mut f, 6, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        // allocate output
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::ArrayNewDefault(T_STR));
        f.instruction(&Instruction::LocalSet(9));
        // pass 2: encode
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(6));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::RefCastNonNull(HeapType::Abstract { shared: false, ty: AbstractHeapType::I31 }));
        f.instruction(&Instruction::I31GetS);
        f.instruction(&Instruction::LocalSet(4));
        utf8_encode(&mut f, 9, 3, 4);
        bump(&mut f, 6, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::End);
        f
    }

    /// str_from_char(c) : Char -> single-code-point String.
    fn emit_str_from_char(&self) -> Function {
        // locals: cp(1), bl(2), off(3):i32, out(4):str
        let mut f = Function::new([(3, ValType::I32), (1, ref_to(T_STR))]);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::RefCastNonNull(HeapType::Abstract { shared: false, ty: AbstractHeapType::I31 }));
        f.instruction(&Instruction::I31GetS);
        f.instruction(&Instruction::LocalSet(1));
        utf8_byte_len(&mut f, 1, 2);
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::ArrayNewDefault(T_STR));
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(3));
        utf8_encode(&mut f, 4, 3, 1);
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::End);
        f
    }

    /// str_uncons(s) : String -> Maybe (Char, String) — first code point paired
    /// with the remaining bytes, or Nothing when empty.
    fn emit_str_uncons(&self) -> Function {
        // locals: sstr(1):str, len(2), idx(3), cp(4), adv(5):i32, rest(6):str
        let mut f = Function::new([
            (1, ref_to(T_STR)),
            (4, ValType::I32),
            (1, ref_to(T_STR)),
        ]);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_STR));
        f.instruction(&Instruction::LocalSet(1));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::ArrayLen);
        f.instruction(&Instruction::LocalSet(2));
        // empty -> Nothing
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::RefNull(HeapType::Concrete(T_ARR)));
        f.instruction(&Instruction::StructNew(T_CTOR));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(3));
        utf8_decode(&mut f, 1, 3, 4, 5);
        // rest = slice [adv, len)
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::ArrayNewDefault(T_STR));
        f.instruction(&Instruction::LocalSet(6));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::ArrayGetU(T_STR));
        f.instruction(&Instruction::ArraySet(T_STR));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        // Just (Char cp, rest)
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::RefI31);
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 2 });
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 1 });
        f.instruction(&Instruction::StructNew(T_CTOR));
        f.instruction(&Instruction::End);
        f
    }

    /// str_length(s) : number of UTF-16 code units (matching JS `.length`, so
    /// astral code points count as 2), boxed as Int.
    fn emit_str_length(&self) -> Function {
        // locals: sstr(1):str, len(2), i(3), cp(4), adv(5), count(6):i32
        let mut f = Function::new([(1, ref_to(T_STR)), (5, ValType::I32)]);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_STR));
        f.instruction(&Instruction::LocalSet(1));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::ArrayLen);
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(6));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        utf8_decode(&mut f, 1, 3, 4, 5);
        // +1 per code point, +1 more for astral (surrogate pair)
        bump(&mut f, 6, 1);
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(0x10000));
        f.instruction(&Instruction::I32GeU);
        f.instruction(&Instruction::If(BlockType::Empty));
        bump(&mut f, 6, 1);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I64ExtendI32U);
        f.instruction(&Instruction::Call(self.box_int_idx));
        f.instruction(&Instruction::End);
        f
    }

    /// clamp(low, high, n) : constrain `n` to [low, high] via val_compare
    /// (works for any comparable).
    fn emit_clamp(&self) -> Function {
        // params: low(0), high(1), n(2). local: r(3):eqref
        let mut f = Function::new([(1, eqref())]);
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalSet(3));
        // if n < low → r = low
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::Call(self.val_compare_idx));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::End);
        // if r > high → r = high
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(self.val_compare_idx));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32GtS);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::End);
        f
    }

    /// result_with_default(default, r) : the Ok value, or `default` on Err.
    fn emit_result_with_default(&self) -> Function {
        let mut f = Function::new([]);
        ctor_tag(&mut f, 1);
        f.instruction(&Instruction::I32Eqz); // tag 0 = Ok
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        ctor_arg0(&mut f, 1);
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f
    }

    /// result_map / result_map_error(f, r) : apply `f` under Ok (resp. Err),
    /// passing the other constructor through unchanged.
    fn emit_result_map(&self, on_err: bool) -> Function {
        // matched tag: Ok=0 for map, Err=1 for mapError; rebuild with same tag
        let tag: i32 = if on_err { 1 } else { 0 };
        let mut f = Function::new([]);
        ctor_tag(&mut f, 1);
        f.instruction(&Instruction::I32Const(tag));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        f.instruction(&Instruction::I32Const(tag));
        f.instruction(&Instruction::LocalGet(0));
        ctor_arg0(&mut f, 1);
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 1 });
        f.instruction(&Instruction::StructNew(T_CTOR));
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f
    }

    /// result_and_then(f, r) : Ok v -> f v ; Err e -> r.
    fn emit_result_and_then(&self) -> Function {
        let mut f = Function::new([]);
        ctor_tag(&mut f, 1);
        f.instruction(&Instruction::I32Eqz); // Ok
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        f.instruction(&Instruction::LocalGet(0));
        ctor_arg0(&mut f, 1);
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f
    }

    /// result_to_maybe(r) : Ok v -> Just v ; Err _ -> Nothing.
    fn emit_result_to_maybe(&self) -> Function {
        let mut f = Function::new([]);
        ctor_tag(&mut f, 0);
        f.instruction(&Instruction::I32Eqz); // Ok
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        f.instruction(&Instruction::I32Const(0)); // Just
        ctor_arg0(&mut f, 0);
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 1 });
        f.instruction(&Instruction::StructNew(T_CTOR));
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::I32Const(1)); // Nothing
        f.instruction(&Instruction::RefNull(HeapType::Concrete(T_ARR)));
        f.instruction(&Instruction::StructNew(T_CTOR));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f
    }

    /// result_from_maybe(err, m) : Just v -> Ok v ; Nothing -> Err err.
    fn emit_result_from_maybe(&self) -> Function {
        let mut f = Function::new([]);
        ctor_tag(&mut f, 1);
        f.instruction(&Instruction::I32Eqz); // Just
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        f.instruction(&Instruction::I32Const(0)); // Ok
        ctor_arg0(&mut f, 1);
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 1 });
        f.instruction(&Instruction::StructNew(T_CTOR));
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::I32Const(1)); // Err
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 1 });
        f.instruction(&Instruction::StructNew(T_CTOR));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f
    }

    /// str_split(sep, s) : List String, matching JS `s.split(sep)`. An empty
    /// separator splits into single code points.
    fn emit_str_split(&self) -> Function {
        // params: sep(0), s(1). locals:
        //   substr(2):str, sstr(3):str,
        //   sublen(4), len(5), start(6), i(7), a(8), b(9), cp(10), adv(11), j(12):i32,
        //   acc(13):eqref, piece(14):str, si(15):i32
        let mut f = Function::new([
            (2, ref_to(T_STR)),
            (9, ValType::I32),
            (1, eqref()),
            (1, ref_to(T_STR)),
            (1, ValType::I32),
        ]);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_STR));
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_STR));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::ArrayLen);
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::ArrayLen);
        f.instruction(&Instruction::LocalSet(5));
        push_empty_list(&mut f);
        f.instruction(&Instruction::LocalSet(13));
        // empty separator → one piece per code point
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(7));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        utf8_decode(&mut f, 3, 7, 10, 11);
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::LocalSet(8)); // a = i
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::LocalGet(11));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(9)); // b = i + adv
        slice_into(&mut f, 3, 8, 9, 14, 15);
        f.instruction(&Instruction::LocalGet(14));
        f.instruction(&Instruction::LocalGet(13));
        f.instruction(&Instruction::Call(self.list_cons_idx));
        f.instruction(&Instruction::LocalSet(13));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::LocalGet(11));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(7));
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(13));
        f.instruction(&Instruction::Call(self.list_reverse_idx));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // non-empty separator: scan for occurrences
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(6)); // start
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(7)); // i
        f.instruction(&Instruction::Block(BlockType::Empty)); // outer_block
        f.instruction(&Instruction::Loop(BlockType::Empty)); // outer_loop
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32GtS);
        f.instruction(&Instruction::BrIf(1)); // i+sublen > len → break outer
        f.instruction(&Instruction::Block(BlockType::Empty)); // matchfail
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(12)); // j
        f.instruction(&Instruction::Loop(BlockType::Empty)); // jloop
        f.instruction(&Instruction::LocalGet(12));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::If(BlockType::Empty)); // full match at i
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::LocalSet(8)); // a = start
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::LocalSet(9)); // b = i
        slice_into(&mut f, 3, 8, 9, 14, 15);
        f.instruction(&Instruction::LocalGet(14));
        f.instruction(&Instruction::LocalGet(13));
        f.instruction(&Instruction::Call(self.list_cons_idx));
        f.instruction(&Instruction::LocalSet(13));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(7)); // i += sublen
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::LocalSet(6)); // start = i
        f.instruction(&Instruction::Br(3)); // continue outer_loop
        f.instruction(&Instruction::End); // if
        // compare substr[j] vs sstr[i+j]
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(12));
        f.instruction(&Instruction::ArrayGetU(T_STR));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::LocalGet(12));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::ArrayGetU(T_STR));
        f.instruction(&Instruction::I32Ne);
        f.instruction(&Instruction::BrIf(1)); // mismatch → break matchfail
        f.instruction(&Instruction::LocalGet(12));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(12));
        f.instruction(&Instruction::Br(0)); // jloop
        f.instruction(&Instruction::End); // jloop
        f.instruction(&Instruction::End); // matchfail
        // mismatch at i → advance i
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(7));
        f.instruction(&Instruction::Br(0)); // outer_loop
        f.instruction(&Instruction::End); // outer_loop
        f.instruction(&Instruction::End); // outer_block
        // final piece = s[start..len]
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::LocalSet(8));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalSet(9));
        slice_into(&mut f, 3, 8, 9, 14, 15);
        f.instruction(&Instruction::LocalGet(14));
        f.instruction(&Instruction::LocalGet(13));
        f.instruction(&Instruction::Call(self.list_cons_idx));
        f.instruction(&Instruction::LocalSet(13));
        f.instruction(&Instruction::LocalGet(13));
        f.instruction(&Instruction::Call(self.list_reverse_idx));
        f.instruction(&Instruction::End);
        f
    }

    /// str_words(s) : split on runs of ASCII whitespace, dropping empties.
    fn emit_str_words(&self) -> Function {
        // locals: sstr(1):str, len(2), i(3), start(4), a(5), b(6), c(7):i32,
        //   acc(8):eqref, piece(9):str, si(10):i32
        let mut f = Function::new([
            (1, ref_to(T_STR)),
            (6, ValType::I32),
            (1, eqref()),
            (1, ref_to(T_STR)),
            (1, ValType::I32),
        ]);
        // whitespace test on local 7: space (32) or 9..=13 (tab/LF/VT/FF/CR)
        let is_ws = |f: &mut Function| {
            f.instruction(&Instruction::LocalGet(7));
            f.instruction(&Instruction::I32Const(32));
            f.instruction(&Instruction::I32Eq);
            f.instruction(&Instruction::LocalGet(7));
            f.instruction(&Instruction::I32Const(9));
            f.instruction(&Instruction::I32Sub);
            f.instruction(&Instruction::I32Const(5));
            f.instruction(&Instruction::I32LtU);
            f.instruction(&Instruction::I32Or);
        };
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_STR));
        f.instruction(&Instruction::LocalSet(1));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::ArrayLen);
        f.instruction(&Instruction::LocalSet(2));
        push_empty_list(&mut f);
        f.instruction(&Instruction::LocalSet(8));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Block(BlockType::Empty)); // outer_block
        f.instruction(&Instruction::Loop(BlockType::Empty)); // outer_loop
        // skip whitespace
        f.instruction(&Instruction::Block(BlockType::Empty)); // skip
        f.instruction(&Instruction::Loop(BlockType::Empty)); // skipl
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::ArrayGetU(T_STR));
        f.instruction(&Instruction::LocalSet(7));
        is_ws(&mut f);
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::BrIf(1)); // non-ws → stop skipping
        bump(&mut f, 3, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End); // skipl
        f.instruction(&Instruction::End); // skip
        // if exhausted, done
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1)); // break outer_block
        // consume a word
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalSet(4)); // start
        f.instruction(&Instruction::Block(BlockType::Empty)); // word
        f.instruction(&Instruction::Loop(BlockType::Empty)); // wordl
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::ArrayGetU(T_STR));
        f.instruction(&Instruction::LocalSet(7));
        is_ws(&mut f);
        f.instruction(&Instruction::BrIf(1)); // ws → end of word
        bump(&mut f, 3, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End); // wordl
        f.instruction(&Instruction::End); // word
        // piece = s[start..i]
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalSet(6));
        slice_into(&mut f, 1, 5, 6, 9, 10);
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::Call(self.list_cons_idx));
        f.instruction(&Instruction::LocalSet(8));
        f.instruction(&Instruction::Br(0)); // outer_loop
        f.instruction(&Instruction::End); // outer_loop
        f.instruction(&Instruction::End); // outer_block
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::Call(self.list_reverse_idx));
        f.instruction(&Instruction::End);
        f
    }

    /// str_lines(s) : split on `\n`, `\r`, or `\r\n` (matching Elm/JS).
    fn emit_str_lines(&self) -> Function {
        // locals: sstr(1):str, len(2), i(3), start(4), a(5), b(6), c(7):i32,
        //   acc(8):eqref, piece(9):str, si(10):i32
        let mut f = Function::new([
            (1, ref_to(T_STR)),
            (6, ValType::I32),
            (1, eqref()),
            (1, ref_to(T_STR)),
            (1, ValType::I32),
        ]);
        // emit: piece = s[start..i]; acc = cons(piece, acc)
        let emit_cut = |f: &mut Function| {
            f.instruction(&Instruction::LocalGet(4));
            f.instruction(&Instruction::LocalSet(5));
            f.instruction(&Instruction::LocalGet(3));
            f.instruction(&Instruction::LocalSet(6));
            slice_into(f, 1, 5, 6, 9, 10);
            f.instruction(&Instruction::LocalGet(9));
            f.instruction(&Instruction::LocalGet(8));
            f.instruction(&Instruction::Call(self.list_cons_idx));
            f.instruction(&Instruction::LocalSet(8));
        };
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_STR));
        f.instruction(&Instruction::LocalSet(1));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::ArrayLen);
        f.instruction(&Instruction::LocalSet(2));
        push_empty_list(&mut f);
        f.instruction(&Instruction::LocalSet(8));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::ArrayGetU(T_STR));
        f.instruction(&Instruction::LocalSet(7));
        // \n
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Const(10));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Empty));
        emit_cut(&mut f);
        bump(&mut f, 3, 1);
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::Else);
        // \r (optionally \r\n)
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Const(13));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Empty));
        emit_cut(&mut f);
        bump(&mut f, 3, 1);
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::ArrayGetU(T_STR));
        f.instruction(&Instruction::I32Const(10));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Empty));
        bump(&mut f, 3, 1);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::Else);
        bump(&mut f, 3, 1);
        f.instruction(&Instruction::End); // \r if
        f.instruction(&Instruction::End); // \n if/else
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End); // loop
        f.instruction(&Instruction::End); // block
        // final piece
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalSet(6));
        slice_into(&mut f, 1, 5, 6, 9, 10);
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::Call(self.list_cons_idx));
        f.instruction(&Instruction::LocalSet(8));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::Call(self.list_reverse_idx));
        f.instruction(&Instruction::End);
        f
    }

    /// list_repeat(n, x) : a list of `n` copies of `x` (empty if n <= 0).
    fn emit_list_repeat(&self) -> Function {
        // params n(0), x(1). locals: n2(2), i(3):i32, data(4):ref T_ARR
        let mut f = Function::new([(2, ValType::I32), (1, ref_to(T_ARR))]);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::Call(self.unbox_int_idx));
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::ArrayNewDefault(T_ARR));
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::ArraySet(T_ARR));
        bump(&mut f, 3, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::StructNew(T_BACK));
        f.instruction(&Instruction::StructNew(T_LIST));
        f.instruction(&Instruction::End);
        f
    }

    /// list_filterMap(f, xs) : map with `f : a -> Maybe b`, keeping Justs.
    fn emit_list_filter_map(&self) -> Function {
        // params f(0), xs(1). local r(2):eqref
        let mut f = Function::new([(1, eqref())]);
        list_is_empty(&mut f, 1);
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        push_empty_list(&mut f);
        f.instruction(&Instruction::Else);
        // r = f head
        f.instruction(&Instruction::LocalGet(0));
        list_head(&mut f, 1);
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&Instruction::LocalSet(2));
        ctor_tag(&mut f, 2);
        f.instruction(&Instruction::I32Eqz); // Just
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        ctor_arg0(&mut f, 2);
        f.instruction(&Instruction::LocalGet(0));
        list_tail(&mut f, 1);
        f.instruction(&Instruction::Call(self.list_filter_map_idx));
        f.instruction(&Instruction::Call(self.list_cons_idx));
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::LocalGet(0));
        list_tail(&mut f, 1);
        f.instruction(&Instruction::Call(self.list_filter_map_idx));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f
    }

    /// list_sortBy(f, xs) : insertion sort keyed by `f`, stable, ascending.
    fn emit_list_sortby(&self) -> Function {
        let mut f = Function::new([]);
        list_is_empty(&mut f, 1);
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        push_empty_list(&mut f);
        f.instruction(&Instruction::Else);
        // insert(f, head, sortBy(f, tail))
        f.instruction(&Instruction::LocalGet(0));
        list_head(&mut f, 1);
        f.instruction(&Instruction::LocalGet(0));
        list_tail(&mut f, 1);
        f.instruction(&Instruction::Call(self.list_sortby_idx));
        f.instruction(&Instruction::Call(self.list_sortby_insert_idx));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f
    }

    /// list_sortByInsert(f, x, ys) : insert `x` into key-sorted `ys`.
    fn emit_list_sortby_insert(&self) -> Function {
        let mut f = Function::new([]);
        list_is_empty(&mut f, 2);
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        // [x]
        f.instruction(&Instruction::LocalGet(1));
        push_empty_list(&mut f);
        f.instruction(&Instruction::Call(self.list_cons_idx));
        f.instruction(&Instruction::Else);
        // compare(f x, f head) <= 0 → cons(x, ys)
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&Instruction::LocalGet(0));
        list_head(&mut f, 2);
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&Instruction::Call(self.val_compare_idx));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32LeS);
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::Call(self.list_cons_idx));
        f.instruction(&Instruction::Else);
        // cons(head, insert(f, x, tail))
        list_head(&mut f, 2);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(1));
        list_tail(&mut f, 2);
        f.instruction(&Instruction::Call(self.list_sortby_insert_idx));
        f.instruction(&Instruction::Call(self.list_cons_idx));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f
    }

    /// str_padLeft / padRight(n, ch, s) : pad `s` with `ch` to width `n`
    /// (UTF-16 length), on the left or right. Shorter targets return `s`.
    fn emit_str_pad(&self, left: bool) -> Function {
        // params: n(0), ch(1), s(2). locals: len(3), need(4):i32, pad(5):eqref
        let mut f = Function::new([(2, ValType::I32), (1, eqref())]);
        // len = str_length(s)
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::Call(self.str_length_idx));
        f.instruction(&Instruction::Call(self.unbox_int_idx));
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::LocalSet(3));
        // need = n - len
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::Call(self.unbox_int_idx));
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::LocalSet(4));
        // already wide enough → s
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32LeS);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // pad = repeat(need, fromChar ch)
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I64ExtendI32S);
        f.instruction(&Instruction::Call(self.box_int_idx));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(self.str_from_char_idx));
        f.instruction(&Instruction::Call(self.str_repeat_idx));
        f.instruction(&Instruction::LocalSet(5));
        if left {
            f.instruction(&Instruction::LocalGet(5));
            f.instruction(&Instruction::LocalGet(2));
        } else {
            f.instruction(&Instruction::LocalGet(2));
            f.instruction(&Instruction::LocalGet(5));
        }
        f.instruction(&Instruction::Call(self.str_append_idx));
        f.instruction(&Instruction::End);
        f
    }

    /// list_intersperse(sep, xs) : `sep` between consecutive elements.
    fn emit_list_intersperse(&self) -> Function {
        // params sep(0), xs(1). locals head(2), tail(3):eqref
        let mut f = Function::new([(2, eqref())]);
        list_is_empty(&mut f, 1);
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        push_empty_list(&mut f);
        f.instruction(&Instruction::Else);
        list_head(&mut f, 1);
        f.instruction(&Instruction::LocalSet(2));
        list_tail(&mut f, 1);
        f.instruction(&Instruction::LocalSet(3));
        list_is_empty(&mut f, 3);
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        // [head]
        f.instruction(&Instruction::LocalGet(2));
        push_empty_list(&mut f);
        f.instruction(&Instruction::Call(self.list_cons_idx));
        f.instruction(&Instruction::Else);
        // cons(head, cons(sep, intersperse(sep, tail)))
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::Call(self.list_intersperse_idx));
        f.instruction(&Instruction::Call(self.list_cons_idx));
        f.instruction(&Instruction::Call(self.list_cons_idx));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f
    }

    /// list_map3(f, xs, ys, zs) : zip-map of three lists, stopping at shortest.
    fn emit_list_map3(&self) -> Function {
        let mut f = Function::new([]);
        list_is_empty(&mut f, 1);
        list_is_empty(&mut f, 2);
        f.instruction(&Instruction::I32Or);
        list_is_empty(&mut f, 3);
        f.instruction(&Instruction::I32Or);
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        push_empty_list(&mut f);
        f.instruction(&Instruction::Else);
        // cons(f h1 h2 h3, map3(f, t1, t2, t3))
        f.instruction(&Instruction::LocalGet(0));
        list_head(&mut f, 1);
        f.instruction(&Instruction::Call(self.apply1_idx));
        list_head(&mut f, 2);
        f.instruction(&Instruction::Call(self.apply1_idx));
        list_head(&mut f, 3);
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&Instruction::LocalGet(0));
        list_tail(&mut f, 1);
        list_tail(&mut f, 2);
        list_tail(&mut f, 3);
        f.instruction(&Instruction::Call(self.list_map3_idx));
        f.instruction(&Instruction::Call(self.list_cons_idx));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f
    }

    /// list_mapN(f, l1..ln) : zip-map over n lists, stopping at the shortest
    /// (recursive cons; `self_idx` is this function's own index). Params are
    /// f(0) then the n lists (locals 1..=n).
    fn emit_list_mapn(&self, n: u32, self_idx: u32) -> Function {
        let mut f = Function::new([]);
        // empty if ANY input is empty
        list_is_empty(&mut f, 1);
        for i in 2..=n {
            list_is_empty(&mut f, i);
            f.instruction(&Instruction::I32Or);
        }
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        push_empty_list(&mut f);
        f.instruction(&Instruction::Else);
        // cons(f h1 .. hn, mapN(f, t1 .. tn))
        f.instruction(&Instruction::LocalGet(0));
        for i in 1..=n {
            list_head(&mut f, i);
            f.instruction(&Instruction::Call(self.apply1_idx));
        }
        f.instruction(&Instruction::LocalGet(0));
        for i in 1..=n {
            list_tail(&mut f, i);
        }
        f.instruction(&Instruction::Call(self_idx));
        f.instruction(&Instruction::Call(self.list_cons_idx));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f
    }

    /// list_partition(pred, xs) : `(matching, non-matching)` preserving order.
    fn emit_list_partition(&self) -> Function {
        // params pred(0), xs(1). locals rest(2), head(3):eqref
        let mut f = Function::new([(2, eqref())]);
        list_is_empty(&mut f, 1);
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        push_empty_list(&mut f);
        push_empty_list(&mut f);
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 2 });
        f.instruction(&Instruction::Else);
        // rest = partition(pred, tail); head = xs.head
        f.instruction(&Instruction::LocalGet(0));
        list_tail(&mut f, 1);
        f.instruction(&Instruction::Call(self.list_partition_idx));
        f.instruction(&Instruction::LocalSet(2));
        list_head(&mut f, 1);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&Instruction::RefCastNonNull(HeapType::Abstract { shared: false, ty: AbstractHeapType::I31 }));
        f.instruction(&Instruction::I31GetS);
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        // [cons(head, rest[0]), rest[1]]
        f.instruction(&Instruction::LocalGet(3));
        self.load_arr(2, 0, &mut f);
        f.instruction(&Instruction::Call(self.list_cons_idx));
        self.load_arr(2, 1, &mut f);
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 2 });
        f.instruction(&Instruction::Else);
        // [rest[0], cons(head, rest[1])]
        self.load_arr(2, 0, &mut f);
        f.instruction(&Instruction::LocalGet(3));
        self.load_arr(2, 1, &mut f);
        f.instruction(&Instruction::Call(self.list_cons_idx));
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 2 });
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f
    }

    /// list_unzip(xs) : `(firsts, seconds)` from a list of pairs.
    fn emit_list_unzip(&self) -> Function {
        // param xs(0). locals rest(1), pair(2):eqref
        let mut f = Function::new([(2, eqref())]);
        list_is_empty(&mut f, 0);
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        push_empty_list(&mut f);
        push_empty_list(&mut f);
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 2 });
        f.instruction(&Instruction::Else);
        list_tail(&mut f, 0);
        f.instruction(&Instruction::Call(self.list_unzip_idx));
        f.instruction(&Instruction::LocalSet(1));
        list_head(&mut f, 0);
        f.instruction(&Instruction::LocalSet(2));
        // [cons(pair[0], rest[0]), cons(pair[1], rest[1])]
        self.load_arr(2, 0, &mut f);
        self.load_arr(1, 0, &mut f);
        f.instruction(&Instruction::Call(self.list_cons_idx));
        self.load_arr(2, 1, &mut f);
        self.load_arr(1, 1, &mut f);
        f.instruction(&Instruction::Call(self.list_cons_idx));
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 2 });
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f
    }

    // ---- Dict: a key-sorted vector of [k,v] pairs (pairs are T_ARR tuples) ----

    /// dict_get(k, d) : Maybe v — linear scan, early-exits once keys pass `k`.
    fn emit_dict_get(&self) -> Function {
        // params k(0), d(1). locals: len(2),i(3),c(4):i32, pair(5):eqref
        let mut f = Function::new([(3, ValType::I32), (1, eqref())]);
        list_len(&mut f, 1);
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        list_elem(&mut f, 1, 3);
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::LocalGet(0));
        self.load_arr(5, 0, &mut f);
        f.instruction(&Instruction::Call(self.val_compare_idx));
        f.instruction(&Instruction::LocalSet(4));
        // c == 0 → Just pair[1]
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(0));
        self.load_arr(5, 1, &mut f);
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 1 });
        f.instruction(&Instruction::StructNew(T_CTOR));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // c < 0 → passed it, Nothing
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::RefNull(HeapType::Concrete(T_ARR)));
        f.instruction(&Instruction::StructNew(T_CTOR));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        bump(&mut f, 3, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::RefNull(HeapType::Concrete(T_ARR)));
        f.instruction(&Instruction::StructNew(T_CTOR));
        f.instruction(&Instruction::End);
        f
    }

    /// dict_insert(k, v, d) : splice `[k,v]` into the sorted vector, replacing
    /// an existing key.
    fn emit_dict_insert(&self) -> Function {
        // params k(0),v(1),d(2). locals: len(3),pos(4),skip(5),i(6),di(7),rlen(8):i32,
        //   ndata(9):ref T_ARR, pair(10):eqref
        let mut f = Function::new([(6, ValType::I32), (1, ref_to(T_ARR)), (1, eqref())]);
        list_len(&mut f, 2);
        f.instruction(&Instruction::LocalSet(3));
        // pos = first index where key >= k
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        list_elem(&mut f, 2, 4);
        f.instruction(&Instruction::LocalSet(10));
        f.instruction(&Instruction::LocalGet(0));
        self.load_arr(10, 0, &mut f);
        f.instruction(&Instruction::Call(self.val_compare_idx));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32LeS);
        f.instruction(&Instruction::BrIf(1));
        bump(&mut f, 4, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        // skip = (pos < len && d[pos].key == k) ? 1 : 0
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::If(BlockType::Empty));
        list_elem(&mut f, 2, 4);
        f.instruction(&Instruction::LocalSet(10));
        f.instruction(&Instruction::LocalGet(0));
        self.load_arr(10, 0, &mut f);
        f.instruction(&Instruction::Call(self.val_compare_idx));
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        // rlen = len + 1 - skip
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::LocalSet(8));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::ArrayNewDefault(T_ARR));
        f.instruction(&Instruction::LocalSet(9));
        // copy prefix [0, pos)
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(6));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::LocalGet(6));
        list_elem(&mut f, 2, 6);
        f.instruction(&Instruction::ArraySet(T_ARR));
        bump(&mut f, 6, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        // ndata[pos] = [k, v]
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 2 });
        f.instruction(&Instruction::ArraySet(T_ARR));
        // copy suffix: src i = pos+skip, dst di = pos+1
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(6));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(7));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::LocalGet(7));
        list_elem(&mut f, 2, 6);
        f.instruction(&Instruction::ArraySet(T_ARR));
        bump(&mut f, 6, 1);
        bump(&mut f, 7, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        // wrap
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::StructNew(T_BACK));
        f.instruction(&Instruction::StructNew(T_LIST));
        f.instruction(&Instruction::End);
        f
    }

    /// dict_remove(k, d) : the sorted vector without key `k`.
    fn emit_dict_remove(&self) -> Function {
        // params k(0),d(1). locals: len(2),found(3),rlen(4),i(5),di(6):i32,
        //   ndata(7):ref T_ARR, pair(8):eqref
        let mut f = Function::new([(5, ValType::I32), (1, ref_to(T_ARR)), (1, eqref())]);
        list_len(&mut f, 1);
        f.instruction(&Instruction::LocalSet(2));
        // found?
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        list_elem(&mut f, 1, 5);
        f.instruction(&Instruction::LocalSet(8));
        f.instruction(&Instruction::LocalGet(0));
        self.load_arr(8, 0, &mut f);
        f.instruction(&Instruction::Call(self.val_compare_idx));
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::End);
        bump(&mut f, 5, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::ArrayNewDefault(T_ARR));
        f.instruction(&Instruction::LocalSet(7));
        // di=0; copy pairs whose key != k
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(6));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        list_elem(&mut f, 1, 5);
        f.instruction(&Instruction::LocalSet(8));
        f.instruction(&Instruction::LocalGet(0));
        self.load_arr(8, 0, &mut f);
        f.instruction(&Instruction::Call(self.val_compare_idx));
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::ArraySet(T_ARR));
        bump(&mut f, 6, 1);
        f.instruction(&Instruction::End);
        bump(&mut f, 5, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::StructNew(T_BACK));
        f.instruction(&Instruction::StructNew(T_LIST));
        f.instruction(&Instruction::End);
        f
    }

    /// dict_from_list(pairs, acc) : insert each `[k,v]` of `pairs` into `acc`
    /// (later duplicates win, matching Elm's foldl-insert).
    fn emit_dict_from_list(&self) -> Function {
        // params pairs(0), acc(1). locals: a(2),cur(3),pair(4):eqref
        let mut f = Function::new([(3, eqref())]);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        list_is_empty(&mut f, 3);
        f.instruction(&Instruction::BrIf(1));
        list_head(&mut f, 3);
        f.instruction(&Instruction::LocalSet(4));
        self.load_arr(4, 0, &mut f);
        self.load_arr(4, 1, &mut f);
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::Call(self.dict_insert_idx));
        f.instruction(&Instruction::LocalSet(2));
        list_tail(&mut f, 3);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::End);
        f
    }

    /// dict_foldl/foldr(f, acc, d) : fold `f key value acc` in key order.
    fn emit_dict_fold(&self, rev: bool) -> Function {
        // params f(0),acc(1),d(2). locals: len(3),i(4):i32, a(5),pair(6):eqref
        let mut f = Function::new([(2, ValType::I32), (2, eqref())]);
        list_len(&mut f, 2);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalSet(5));
        if rev {
            f.instruction(&Instruction::LocalGet(3));
            f.instruction(&Instruction::I32Const(1));
            f.instruction(&Instruction::I32Sub);
            f.instruction(&Instruction::LocalSet(4));
        } else {
            f.instruction(&Instruction::I32Const(0));
            f.instruction(&Instruction::LocalSet(4));
        }
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        if rev {
            f.instruction(&Instruction::LocalGet(4));
            f.instruction(&Instruction::I32Const(0));
            f.instruction(&Instruction::I32LtS);
        } else {
            f.instruction(&Instruction::LocalGet(4));
            f.instruction(&Instruction::LocalGet(3));
            f.instruction(&Instruction::I32GeS);
        }
        f.instruction(&Instruction::BrIf(1));
        list_elem(&mut f, 2, 4);
        f.instruction(&Instruction::LocalSet(6));
        f.instruction(&Instruction::LocalGet(0));
        self.load_arr(6, 0, &mut f);
        f.instruction(&Instruction::Call(self.apply1_idx));
        self.load_arr(6, 1, &mut f);
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&Instruction::LocalSet(5));
        if rev {
            f.instruction(&Instruction::LocalGet(4));
            f.instruction(&Instruction::I32Const(1));
            f.instruction(&Instruction::I32Sub);
            f.instruction(&Instruction::LocalSet(4));
        } else {
            bump(&mut f, 4, 1);
        }
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::End);
        f
    }

    /// dict_map(f, d) : rebuild with `[k, f k v]` (keys/order unchanged).
    fn emit_dict_map(&self) -> Function {
        // params f(0),d(1). locals: len(2),i(3):i32, ndata(4):ref T_ARR, pair(5):eqref
        let mut f = Function::new([(2, ValType::I32), (1, ref_to(T_ARR)), (1, eqref())]);
        list_len(&mut f, 1);
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::ArrayNewDefault(T_ARR));
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        list_elem(&mut f, 1, 3);
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(3));
        self.load_arr(5, 0, &mut f);
        f.instruction(&Instruction::LocalGet(0));
        self.load_arr(5, 0, &mut f);
        f.instruction(&Instruction::Call(self.apply1_idx));
        self.load_arr(5, 1, &mut f);
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 2 });
        f.instruction(&Instruction::ArraySet(T_ARR));
        bump(&mut f, 3, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::StructNew(T_BACK));
        f.instruction(&Instruction::StructNew(T_LIST));
        f.instruction(&Instruction::End);
        f
    }

    /// dict_filter(pred, d) : keep pairs where `pred k v` (scans from the tail
    /// so consing preserves ascending order).
    fn emit_dict_filter(&self) -> Function {
        // params pred(0),d(1). locals: len(2),i(3):i32, acc(4),pair(5):eqref
        let mut f = Function::new([(2, ValType::I32), (2, eqref())]);
        list_len(&mut f, 1);
        f.instruction(&Instruction::LocalSet(2));
        push_empty_list(&mut f);
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::BrIf(1));
        list_elem(&mut f, 1, 3);
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::LocalGet(0));
        self.load_arr(5, 0, &mut f);
        f.instruction(&Instruction::Call(self.apply1_idx));
        self.load_arr(5, 1, &mut f);
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&Instruction::RefCastNonNull(HeapType::Abstract { shared: false, ty: AbstractHeapType::I31 }));
        f.instruction(&Instruction::I31GetS);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::Call(self.list_cons_idx));
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::End);
        f
    }

    /// dict_keys / dict_values(d) : project field 0 / 1 of each pair.
    fn emit_dict_project(&self, field: u32) -> Function {
        // params d(0). locals: len(1),i(2):i32, ndata(3):ref T_ARR, pair(4):eqref
        let mut f = Function::new([(2, ValType::I32), (1, ref_to(T_ARR)), (1, eqref())]);
        list_len(&mut f, 0);
        f.instruction(&Instruction::LocalSet(1));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::ArrayNewDefault(T_ARR));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        list_elem(&mut f, 0, 2);
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(2));
        self.load_arr(4, field, &mut f);
        f.instruction(&Instruction::ArraySet(T_ARR));
        bump(&mut f, 2, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::StructNew(T_BACK));
        f.instruction(&Instruction::StructNew(T_LIST));
        f.instruction(&Instruction::End);
        f
    }

    /// dict_intersect(t1, t2) : pairs of `t1` whose key is in `t2` (t1's values).
    fn emit_dict_intersect(&self) -> Function {
        // params t1(0),t2(1). locals: len(2),i(3):i32, acc(4),pair(5):eqref
        let mut f = Function::new([(2, ValType::I32), (2, eqref())]);
        list_len(&mut f, 0);
        f.instruction(&Instruction::LocalSet(2));
        push_empty_list(&mut f);
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::BrIf(1));
        list_elem(&mut f, 0, 3);
        f.instruction(&Instruction::LocalSet(5));
        // if member(pair.key, t2): keep
        self.load_arr(5, 0, &mut f);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(self.dict_get_idx));
        f.instruction(&cast_to(T_CTOR));
        f.instruction(&Instruction::StructGet { struct_type_index: T_CTOR, field_index: 0 });
        f.instruction(&Instruction::I32Eqz); // Just
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::Call(self.list_cons_idx));
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::End);
        f
    }

    /// dict_diff(toRemove, base) : `base` without any key present in `toRemove`.
    fn emit_dict_diff(&self) -> Function {
        // params toRemove(0), base(1). locals: a(2),cur(3),pair(4):eqref
        let mut f = Function::new([(3, eqref())]);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        list_is_empty(&mut f, 3);
        f.instruction(&Instruction::BrIf(1));
        list_head(&mut f, 3);
        f.instruction(&Instruction::LocalSet(4));
        self.load_arr(4, 0, &mut f);
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::Call(self.dict_remove_idx));
        f.instruction(&Instruction::LocalSet(2));
        list_tail(&mut f, 3);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::End);
        f
    }

    /// dict_update(k, alter, d) : `alter (get k d)` then insert/remove.
    fn emit_dict_update(&self) -> Function {
        // params k(0), alter(1), d(2). local r(3):eqref
        let mut f = Function::new([(1, eqref())]);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::Call(self.dict_get_idx));
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&Instruction::LocalSet(3));
        ctor_tag(&mut f, 3);
        f.instruction(&Instruction::I32Eqz); // Just
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        f.instruction(&Instruction::LocalGet(0));
        ctor_arg0(&mut f, 3);
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::Call(self.dict_insert_idx));
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::Call(self.dict_remove_idx));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f
    }

    // ---- Array: the same T_LIST vector; most ops are the List kernels ----

    /// array_get(i, a) : `Just a[i]` in bounds, else `Nothing`.
    fn emit_array_get(&self) -> Function {
        // params i(0), a(1). locals: ii(2), len(3):i32
        let mut f = Function::new([(2, ValType::I32)]);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::Call(self.unbox_int_idx));
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::LocalSet(2));
        list_len(&mut f, 1);
        f.instruction(&Instruction::LocalSet(3));
        // 0 <= ii < len ?
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::I32And);
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        f.instruction(&Instruction::I32Const(0)); // Just
        // front-anchored: element i is data[i] (start = 0)
        list_data(&mut f, 1);
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 1 });
        f.instruction(&Instruction::StructNew(T_CTOR));
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::I32Const(1)); // Nothing
        f.instruction(&Instruction::RefNull(HeapType::Concrete(T_ARR)));
        f.instruction(&Instruction::StructNew(T_CTOR));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f
    }

    /// array_set(i, x, a) : copy with index `i` replaced (unchanged if out of
    /// bounds).
    fn emit_array_set(&self) -> Function {
        // params i(0), x(1), a(2). locals: ii(3),len(4),j(5):i32, ndata(6):ref T_ARR
        let mut f = Function::new([(3, ValType::I32), (1, ref_to(T_ARR))]);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::Call(self.unbox_int_idx));
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::LocalSet(3));
        list_len(&mut f, 2);
        f.instruction(&Instruction::LocalSet(4));
        // out of bounds → return a unchanged
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::I32Or);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::ArrayNewDefault(T_ARR));
        f.instruction(&Instruction::LocalSet(6));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        // ndata[j] = (j == ii) ? x : a[j]
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Else);
        list_elem(&mut f, 2, 5);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::ArraySet(T_ARR));
        bump(&mut f, 5, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::StructNew(T_BACK));
        f.instruction(&Instruction::StructNew(T_LIST));
        f.instruction(&Instruction::End);
        f
    }

    /// array_push(x, a) : amortized-O(1) append. Arrays are FRONT-anchored
    /// (elements at data[0..len), back slack [len,cap)); `bk.head` is the
    /// back water-mark (highest used index, the push-ownership marker, mirroring
    /// how cons uses it for the front). In-place when this view owns the tail
    /// slot and slack exists; otherwise grow (double) and copy.
    fn emit_array_push(&self) -> Function {
        // params x(0), a(1). locals: len(2),cap(3),k(4):i32; bk(5):ref null T_BACK;
        //   data(6),ndata(7):ref T_ARR
        let mut f = Function::new([(3, ValType::I32), (1, ref_null_to(T_BACK)), (2, ref_to(T_ARR))]);
        list_len(&mut f, 1);
        f.instruction(&Instruction::LocalSet(2));
        list_bk(&mut f, 1);
        f.instruction(&Instruction::LocalSet(5));
        // in-place fast path when bk != null, len == bk.head (owns tail), len < cap
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::RefIsNull);
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&cast_to(T_BACK));
        f.instruction(&Instruction::StructGet { struct_type_index: T_BACK, field_index: 1 });
        f.instruction(&Instruction::LocalSet(6)); // data
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::ArrayLen);
        f.instruction(&Instruction::LocalSet(3)); // cap
        // len == bk.head && len < cap
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&cast_to(T_BACK));
        f.instruction(&Instruction::StructGet { struct_type_index: T_BACK, field_index: 0 });
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::I32And);
        f.instruction(&Instruction::If(BlockType::Empty));
        // data[len] = x ; bk.head = len+1 ; return {len+1, bk}
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::ArraySet(T_ARR));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&cast_to(T_BACK));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::StructSet { struct_type_index: T_BACK, field_index: 0 });
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::StructNew(T_LIST));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        // grow: ndata = fresh[2*(len+1)]; copy [0..len); ndata[len] = x
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I32Const(2));
        f.instruction(&Instruction::I32Mul);
        f.instruction(&Instruction::ArrayNewDefault(T_ARR));
        f.instruction(&Instruction::LocalSet(7));
        // copy existing elements data[0..len) (front-anchored) when bk != null
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::RefIsNull);
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&cast_to(T_BACK));
        f.instruction(&Instruction::StructGet { struct_type_index: T_BACK, field_index: 1 });
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::ArrayCopy { array_type_index_dst: T_ARR, array_type_index_src: T_ARR });
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::ArraySet(T_ARR));
        // return {len+1, {head: len+1, ndata}}
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::StructNew(T_BACK));
        f.instruction(&Instruction::StructNew(T_LIST));
        f.instruction(&Instruction::End);
        f
    }

    /// array_tighten(a) : return a front-anchored array whose backing is exactly
    /// `len` long (no back slack), so ops that assume tight vectors (start=0=
    /// cap-len) can reuse the List helpers. Identity when already tight/empty.
    fn emit_array_tighten(&self) -> Function {
        // param a(0). locals: len(1):i32, ndata(2):ref T_ARR
        let mut f = Function::new([(1, ValType::I32), (1, ref_to(T_ARR))]);
        list_bk(&mut f, 0);
        f.instruction(&Instruction::RefIsNull);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        list_len(&mut f, 0);
        f.instruction(&Instruction::LocalSet(1));
        // already tight (cap == len)? return a
        list_data(&mut f, 0);
        f.instruction(&Instruction::ArrayLen);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // ndata[0..len) <- data[0..len)
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::ArrayNewDefault(T_ARR));
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(0));
        list_data(&mut f, 0);
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::ArrayCopy { array_type_index_dst: T_ARR, array_type_index_src: T_ARR });
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::StructNew(T_BACK));
        f.instruction(&Instruction::StructNew(T_LIST));
        f.instruction(&Instruction::End);
        f
    }

    /// list_to_array(list) : `Array.fromList` — copy a (back-anchored) List's
    /// elements into a fresh front-anchored, tight Array backing.
    fn emit_list_to_array(&self) -> Function {
        // param list(0). locals: len(1),start(2):i32, ndata(3):ref T_ARR
        let mut f = Function::new([(2, ValType::I32), (1, ref_to(T_ARR))]);
        list_len(&mut f, 0);
        f.instruction(&Instruction::LocalSet(1));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Empty));
        push_empty_list(&mut f);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        list_start(&mut f, 0);
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::ArrayNewDefault(T_ARR));
        f.instruction(&Instruction::LocalSet(3));
        // ndata[0..len) <- listdata[start..start+len)
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Const(0));
        list_data(&mut f, 0);
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::ArrayCopy { array_type_index_dst: T_ARR, array_type_index_src: T_ARR });
        // {len, {head:len, ndata}}
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::StructNew(T_BACK));
        f.instruction(&Instruction::StructNew(T_LIST));
        f.instruction(&Instruction::End);
        f
    }

    /// array_slice(from, to, a) : JS/Elm `slice` — negative indices count from
    /// the end; clamps to bounds.
    fn emit_array_slice(&self) -> Function {
        // params from(0), to(1), a(2). locals: len(3),s(4),e(5),j(6):i32,
        //   ndata(7):ref T_ARR
        let mut f = Function::new([(4, ValType::I32), (1, ref_to(T_ARR))]);
        list_len(&mut f, 2);
        f.instruction(&Instruction::LocalSet(3));
        // s = normalize(from)
        self.emit_slice_index(&mut f, 0, 3, 4);
        // e = normalize(to)
        self.emit_slice_index(&mut f, 1, 3, 5);
        // if e < s { e = s }
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::End);
        // ndata = new(e - s); copy a[s + j]
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::ArrayNewDefault(T_ARR));
        f.instruction(&Instruction::LocalSet(7));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(6));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::LocalGet(6));
        // a[s + j] : reuse list_elem with a temp index in local 6? need s+j.
        // compute index into a fresh push: use local pattern via list_data/start
        list_data(&mut f, 2);
        list_start(&mut f, 2);
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::ArraySet(T_ARR));
        bump(&mut f, 6, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::StructNew(T_BACK));
        f.instruction(&Instruction::StructNew(T_LIST));
        f.instruction(&Instruction::End);
        f
    }

    /// In array_slice: read boxed Int arg `arg`, normalize it against `len`
    /// (negative = from end, clamp to [0,len]), store into local `out`.
    fn emit_slice_index(&self, f: &mut Function, arg: u32, len: u32, out: u32) {
        f.instruction(&Instruction::LocalGet(arg));
        f.instruction(&Instruction::Call(self.unbox_int_idx));
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::LocalSet(out));
        // if out < 0 { out += len }
        f.instruction(&Instruction::LocalGet(out));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(out));
        f.instruction(&Instruction::LocalGet(len));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(out));
        f.instruction(&Instruction::End);
        // clamp to [0, len]
        f.instruction(&Instruction::LocalGet(out));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(out));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(out));
        f.instruction(&Instruction::LocalGet(len));
        f.instruction(&Instruction::I32GtS);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(len));
        f.instruction(&Instruction::LocalSet(out));
        f.instruction(&Instruction::End);
    }

    /// array_initialize(n, gen) : `[gen 0, gen 1, .., gen (n-1)]`.
    fn emit_array_initialize(&self) -> Function {
        // params n(0), gen(1). locals: n2(2),i(3):i32, ndata(4):ref T_ARR
        let mut f = Function::new([(2, ValType::I32), (1, ref_to(T_ARR))]);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::Call(self.unbox_int_idx));
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::ArrayNewDefault(T_ARR));
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(3));
        // gen(box i)
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I64ExtendI32S);
        f.instruction(&Instruction::Call(self.box_int_idx));
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&Instruction::ArraySet(T_ARR));
        bump(&mut f, 3, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::StructNew(T_BACK));
        f.instruction(&Instruction::StructNew(T_LIST));
        f.instruction(&Instruction::End);
        f
    }

    /// array_toIndexedList(a) : `[(0,a0),(1,a1),..]`.
    fn emit_array_to_indexed(&self) -> Function {
        // params a(0). locals: len(1),i(2):i32, ndata(3):ref T_ARR
        let mut f = Function::new([(2, ValType::I32), (1, ref_to(T_ARR))]);
        list_len(&mut f, 0);
        f.instruction(&Instruction::LocalSet(1));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::ArrayNewDefault(T_ARR));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        // ndata[i] = (box i, a[i])
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I64ExtendI32S);
        f.instruction(&Instruction::Call(self.box_int_idx));
        list_elem(&mut f, 0, 2);
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 2 });
        f.instruction(&Instruction::ArraySet(T_ARR));
        bump(&mut f, 2, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::StructNew(T_BACK));
        f.instruction(&Instruction::StructNew(T_LIST));
        f.instruction(&Instruction::End);
        f
    }

    // ---- Set: a sorted vector of unique elements ----

    /// set_insert(x, s) : splice `x` into the sorted vector (dedup).
    fn emit_set_insert(&self) -> Function {
        // params x(0), s(1). locals: len(2),pos(3),skip(4),i(5),di(6),rlen(7):i32,
        //   ndata(8):ref T_ARR
        let mut f = Function::new([(6, ValType::I32), (1, ref_to(T_ARR))]);
        list_len(&mut f, 1);
        f.instruction(&Instruction::LocalSet(2));
        // pos = first index where s[i] >= x
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(0));
        list_elem(&mut f, 1, 3);
        f.instruction(&Instruction::Call(self.val_compare_idx));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32LeS);
        f.instruction(&Instruction::BrIf(1));
        bump(&mut f, 3, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        // skip = (pos < len && s[pos] == x) ? 1 : 0
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(0));
        list_elem(&mut f, 1, 3);
        f.instruction(&Instruction::Call(self.val_compare_idx));
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::LocalSet(7));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::ArrayNewDefault(T_ARR));
        f.instruction(&Instruction::LocalSet(8));
        // prefix
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::LocalGet(5));
        list_elem(&mut f, 1, 5);
        f.instruction(&Instruction::ArraySet(T_ARR));
        bump(&mut f, 5, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        // ndata[pos] = x
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::ArraySet(T_ARR));
        // suffix
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(6));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::LocalGet(6));
        list_elem(&mut f, 1, 5);
        f.instruction(&Instruction::ArraySet(T_ARR));
        bump(&mut f, 5, 1);
        bump(&mut f, 6, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::StructNew(T_BACK));
        f.instruction(&Instruction::StructNew(T_LIST));
        f.instruction(&Instruction::End);
        f
    }

    /// set_member(x, s) : Bool.
    fn emit_set_member(&self) -> Function {
        // params x(0), s(1). locals: len(2),i(3),c(4):i32
        let mut f = Function::new([(3, ValType::I32)]);
        list_len(&mut f, 1);
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(0));
        list_elem(&mut f, 1, 3);
        f.instruction(&Instruction::Call(self.val_compare_idx));
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::RefI31);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::RefI31);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        bump(&mut f, 3, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::RefI31);
        f.instruction(&Instruction::End);
        f
    }

    /// set_remove(x, s) : the sorted vector without `x`.
    fn emit_set_remove(&self) -> Function {
        // params x(0), s(1). locals: len(2),found(3),rlen(4),i(5),di(6):i32,
        //   ndata(7):ref T_ARR
        let mut f = Function::new([(5, ValType::I32), (1, ref_to(T_ARR))]);
        list_len(&mut f, 1);
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(0));
        list_elem(&mut f, 1, 5);
        f.instruction(&Instruction::Call(self.val_compare_idx));
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::End);
        bump(&mut f, 5, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::ArrayNewDefault(T_ARR));
        f.instruction(&Instruction::LocalSet(7));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(6));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(0));
        list_elem(&mut f, 1, 5);
        f.instruction(&Instruction::Call(self.val_compare_idx));
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::LocalGet(6));
        list_elem(&mut f, 1, 5);
        f.instruction(&Instruction::ArraySet(T_ARR));
        bump(&mut f, 6, 1);
        f.instruction(&Instruction::End);
        bump(&mut f, 5, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::StructNew(T_BACK));
        f.instruction(&Instruction::StructNew(T_LIST));
        f.instruction(&Instruction::End);
        f
    }

    /// set_from_list(xs, acc) : insert each element of `xs` into `acc`.
    fn emit_set_from_list(&self) -> Function {
        // params xs(0), acc(1). locals: a(2),cur(3):eqref
        let mut f = Function::new([(2, eqref())]);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        list_is_empty(&mut f, 3);
        f.instruction(&Instruction::BrIf(1));
        list_head(&mut f, 3);
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::Call(self.set_insert_idx));
        f.instruction(&Instruction::LocalSet(2));
        list_tail(&mut f, 3);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::End);
        f
    }

    /// set_intersect(s1, s2) : elements of `s1` also in `s2`.
    fn emit_set_intersect(&self) -> Function {
        // params s1(0), s2(1). locals: len(2),i(3):i32, acc(4),elem(5):eqref
        let mut f = Function::new([(2, ValType::I32), (2, eqref())]);
        list_len(&mut f, 0);
        f.instruction(&Instruction::LocalSet(2));
        push_empty_list(&mut f);
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::BrIf(1));
        list_elem(&mut f, 0, 3);
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(self.set_member_idx));
        f.instruction(&Instruction::RefCastNonNull(HeapType::Abstract { shared: false, ty: AbstractHeapType::I31 }));
        f.instruction(&Instruction::I31GetS);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::Call(self.list_cons_idx));
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::End);
        f
    }

    /// set_diff(toRemove, base) : `base` without any element of `toRemove`.
    fn emit_set_diff(&self) -> Function {
        // params toRemove(0), base(1). locals: a(2),cur(3):eqref
        let mut f = Function::new([(2, eqref())]);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        list_is_empty(&mut f, 3);
        f.instruction(&Instruction::BrIf(1));
        list_head(&mut f, 3);
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::Call(self.set_remove_idx));
        f.instruction(&Instruction::LocalSet(2));
        list_tail(&mut f, 3);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::End);
        f
    }

    // ---- Dict/Set persistent treap (BST by key, heap by val_hash priority) ----

    /// val_hash(v) : deterministic hash of a comparable key (Int/Float/Char/
    /// String/Bool/tuple/list). Used as the treap priority so equal key-sets
    /// balance the same way regardless of insertion order.
    fn emit_val_hash(&self) -> Function {
        // param v(0). locals: h(1),i(2),n(3):i32; s(4):ref T_STR
        let mut f = Function::new([(3, ValType::I32), (1, ref_to(T_STR))]);
        // i31 (Char/Bool/Unit/small Int) → its scalar value
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::RefTestNonNull(HeapType::Abstract { shared: false, ty: AbstractHeapType::I31 }));
        f.instruction(&Instruction::If(BlockType::Result(ValType::I32)));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::RefCastNonNull(HeapType::Abstract { shared: false, ty: AbstractHeapType::I31 }));
        f.instruction(&Instruction::I31GetS);
        f.instruction(&Instruction::I32Const(-1640531535)); // Knuth mix: scramble
        f.instruction(&Instruction::I32Mul);                // so sequential keys
        f.instruction(&Instruction::Else);                  // don't degenerate the treap
        // T_INT → lo ^ hi
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::RefTestNonNull(HeapType::Concrete(T_INT)));
        f.instruction(&Instruction::If(BlockType::Result(ValType::I32)));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_INT));
        f.instruction(&Instruction::StructGet { struct_type_index: T_INT, field_index: 0 });
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_INT));
        f.instruction(&Instruction::StructGet { struct_type_index: T_INT, field_index: 0 });
        f.instruction(&Instruction::I64Const(32));
        f.instruction(&Instruction::I64ShrU);
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::I32Xor);
        f.instruction(&Instruction::I32Const(-1640531535));
        f.instruction(&Instruction::I32Mul);
        f.instruction(&Instruction::Else);
        // T_FLOAT → bits lo ^ hi
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::RefTestNonNull(HeapType::Concrete(T_FLOAT)));
        f.instruction(&Instruction::If(BlockType::Result(ValType::I32)));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_FLOAT));
        f.instruction(&Instruction::StructGet { struct_type_index: T_FLOAT, field_index: 0 });
        f.instruction(&Instruction::I64ReinterpretF64);
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_FLOAT));
        f.instruction(&Instruction::StructGet { struct_type_index: T_FLOAT, field_index: 0 });
        f.instruction(&Instruction::I64ReinterpretF64);
        f.instruction(&Instruction::I64Const(32));
        f.instruction(&Instruction::I64ShrU);
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::I32Xor);
        f.instruction(&Instruction::Else);
        // T_STR → FNV-1a over bytes
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::RefTestNonNull(HeapType::Concrete(T_STR)));
        f.instruction(&Instruction::If(BlockType::Result(ValType::I32)));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_STR));
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::I32Const(-2128831035)); // FNV offset basis
        f.instruction(&Instruction::LocalSet(1));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::ArrayLen);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::ArrayGetU(T_STR));
        f.instruction(&Instruction::I32Xor);
        f.instruction(&Instruction::I32Const(16777619)); // FNV prime
        f.instruction(&Instruction::I32Mul);
        f.instruction(&Instruction::LocalSet(1));
        bump(&mut f, 2, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Else);
        // T_ARR (tuple) → combine element hashes
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::RefTestNonNull(HeapType::Concrete(T_ARR)));
        f.instruction(&Instruction::If(BlockType::Result(ValType::I32)));
        f.instruction(&Instruction::I32Const(7));
        f.instruction(&Instruction::LocalSet(1));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_ARR));
        f.instruction(&Instruction::ArrayLen);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(31));
        f.instruction(&Instruction::I32Mul);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_ARR));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::Call(self.val_hash_idx));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(1));
        bump(&mut f, 2, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Else);
        // T_LIST (list key) → combine element hashes
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::RefTestNonNull(HeapType::Concrete(T_LIST)));
        f.instruction(&Instruction::If(BlockType::Result(ValType::I32)));
        f.instruction(&Instruction::I32Const(11));
        f.instruction(&Instruction::LocalSet(1));
        list_len(&mut f, 0);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(31));
        f.instruction(&Instruction::I32Mul);
        list_elem(&mut f, 0, 2);
        f.instruction(&Instruction::Call(self.val_hash_idx));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(1));
        bump(&mut f, 2, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End); // function terminal
        f
    }

    /// treap_get(k, t) : `Maybe v` — iterative BST search by val_compare.
    fn emit_treap_get(&self) -> Function {
        // params k(0), t(1):eqref. locals: c(2):i32; cur(3):ref null T_TNODE
        let mut f = Function::new([(1, ValType::I32), (1, ref_null_to(T_TNODE))]);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_null(T_TNODE));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::RefIsNull);
        f.instruction(&Instruction::BrIf(1));
        // c = val_compare(k, cur.key)
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 0 });
        f.instruction(&Instruction::Call(self.val_compare_idx));
        f.instruction(&Instruction::LocalTee(2));
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Empty));
        // Just(cur.value)
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 1 });
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 1 });
        f.instruction(&Instruction::StructNew(T_CTOR));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // cur = c < 0 ? cur.left : cur.right
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::If(BlockType::Result(ref_null_to(T_TNODE))));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 3 });
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 4 });
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        // Nothing
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::RefNull(HeapType::Concrete(T_ARR)));
        f.instruction(&Instruction::StructNew(T_CTOR));
        f.instruction(&Instruction::End);
        f
    }

    /// mknode: expects [key, value, pri, left, right] on the stack.
    fn treap_node(f: &mut Function) {
        f.instruction(&Instruction::StructNew(T_TNODE));
    }

    /// treap_insert(k, v, t) : persistent insert with priority rotations.
    fn emit_treap_insert(&self) -> Function {
        // params k(0), v(1), t(2). locals: c(3):i32; child(4):ref null T_TNODE
        let mut f = Function::new([(1, ValType::I32), (1, ref_null_to(T_TNODE))]);
        // t null → new leaf
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::RefIsNull);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(0)); // key
        f.instruction(&Instruction::LocalGet(1)); // value
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::Call(self.val_hash_idx)); // pri
        f.instruction(&Instruction::RefNull(HeapType::Concrete(T_TNODE)));
        f.instruction(&Instruction::RefNull(HeapType::Concrete(T_TNODE)));
        Self::treap_node(&mut f);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // c = val_compare(k, t.key)
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 0 });
        f.instruction(&Instruction::Call(self.val_compare_idx));
        f.instruction(&Instruction::LocalTee(3));
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Empty));
        // equal: replace value, keep structure
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 0 });
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 2 });
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 3 });
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 4 });
        Self::treap_node(&mut f);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // recurse left (c<0) or right
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::If(BlockType::Result(ref_null_to(T_TNODE))));
        // child = insert(k,v,t.left)
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 3 });
        f.instruction(&Instruction::Call(self.treap_insert_idx));
        f.instruction(&cast_null(T_TNODE));
        f.instruction(&Instruction::LocalSet(4));
        // if child.pri > t.pri → rotate right, else node(t; left=child)
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 2 });
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 2 });
        f.instruction(&Instruction::I32GtS);
        f.instruction(&Instruction::If(BlockType::Result(ref_null_to(T_TNODE))));
        // rotate right: newRoot = child; child.right = node(t.key,t.val,t.pri, child.right, t.right)
        // child fields
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 0 });
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 1 });
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 2 });
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 3 }); // child.left
        // new right subtree = node(t.key,t.val,t.pri, child.right, t.right)
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 0 });
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 1 });
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 2 });
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 4 }); // child.right
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 4 }); // t.right
        Self::treap_node(&mut f);
        Self::treap_node(&mut f);
        f.instruction(&Instruction::Else);
        // node(t.key,t.val,t.pri, child, t.right)
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 0 });
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 1 });
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 2 });
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 4 });
        Self::treap_node(&mut f);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::Else);
        // c > 0: child = insert(k,v,t.right)
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 4 });
        f.instruction(&Instruction::Call(self.treap_insert_idx));
        f.instruction(&cast_null(T_TNODE));
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 2 });
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 2 });
        f.instruction(&Instruction::I32GtS);
        f.instruction(&Instruction::If(BlockType::Result(ref_null_to(T_TNODE))));
        // rotate left: newRoot = child; child.left = node(t.key,t.val,t.pri, t.left, child.left)
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 0 });
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 1 });
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 2 });
        // new left subtree = node(t.key,t.val,t.pri, t.left, child.left)
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 0 });
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 1 });
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 2 });
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 3 }); // t.left
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 3 }); // child.left
        Self::treap_node(&mut f);
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 4 }); // child.right
        Self::treap_node(&mut f);
        f.instruction(&Instruction::Else);
        // node(t.key,t.val,t.pri, t.left, child)
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 0 });
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 1 });
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 2 });
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 3 });
        f.instruction(&Instruction::LocalGet(4));
        Self::treap_node(&mut f);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f
    }

    /// treap_merge(l, r) : merge two treaps (all keys(l) < all keys(r)).
    fn emit_treap_merge(&self) -> Function {
        // params l(0), r(1).
        let mut f = Function::new([]);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::RefIsNull);
        f.instruction(&Instruction::If(BlockType::Result(ref_null_to(T_TNODE))));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_null(T_TNODE));
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::RefIsNull);
        f.instruction(&Instruction::If(BlockType::Result(ref_null_to(T_TNODE))));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_null(T_TNODE));
        f.instruction(&Instruction::Else);
        // both non-null: higher priority becomes root
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 2 });
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 2 });
        f.instruction(&Instruction::I32GtS);
        f.instruction(&Instruction::If(BlockType::Result(ref_null_to(T_TNODE))));
        // l root: node(l.key,l.val,l.pri, l.left, merge(l.right, r))
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 0 });
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 1 });
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 2 });
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 3 });
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 4 });
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(self.treap_merge_idx));
        f.instruction(&cast_null(T_TNODE));
        Self::treap_node(&mut f);
        f.instruction(&Instruction::Else);
        // r root: node(r.key,r.val,r.pri, merge(l, r.left), r.right)
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 0 });
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 1 });
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 2 });
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 3 });
        f.instruction(&Instruction::Call(self.treap_merge_idx));
        f.instruction(&cast_null(T_TNODE));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 4 });
        Self::treap_node(&mut f);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End); // function terminal
        f
    }

    /// treap_remove(k, t) : persistent delete (merge the two subtrees at the hit).
    fn emit_treap_remove(&self) -> Function {
        // params k(0), t(1). locals: c(2):i32
        let mut f = Function::new([(1, ValType::I32)]);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::RefIsNull);
        f.instruction(&Instruction::If(BlockType::Result(ref_null_to(T_TNODE))));
        f.instruction(&Instruction::RefNull(HeapType::Concrete(T_TNODE)));
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 0 });
        f.instruction(&Instruction::Call(self.val_compare_idx));
        f.instruction(&Instruction::LocalTee(2));
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Result(ref_null_to(T_TNODE))));
        // hit: merge(t.left, t.right)
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 3 });
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 4 });
        f.instruction(&Instruction::Call(self.treap_merge_idx));
        f.instruction(&cast_null(T_TNODE));
        f.instruction(&Instruction::Else);
        // rebuild: node(t.key, t.val, t.pri, newLeft, newRight)
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 0 });
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 1 });
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 2 });
        // newLeft = c<0 ? remove(k, t.left) : t.left
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::If(BlockType::Result(ref_null_to(T_TNODE))));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 3 });
        f.instruction(&Instruction::Call(self.treap_remove_idx));
        f.instruction(&cast_null(T_TNODE));
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 3 });
        f.instruction(&Instruction::End);
        // newRight = c<0 ? t.right : remove(k, t.right)
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::If(BlockType::Result(ref_null_to(T_TNODE))));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 4 });
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 4 });
        f.instruction(&Instruction::Call(self.treap_remove_idx));
        f.instruction(&cast_null(T_TNODE));
        f.instruction(&Instruction::End);
        Self::treap_node(&mut f);
        f.instruction(&Instruction::End); // c==0 If
        f.instruction(&Instruction::End); // tnull If
        f.instruction(&Instruction::End); // function terminal
        f
    }

    /// treap_pairs(t, acc) : prepend this tree's [k,v] pairs (ascending) onto
    /// `acc`, producing a key-sorted list. In-order, right-to-left cons.
    fn emit_treap_pairs(&self) -> Function {
        // params t(0), acc(1).
        let mut f = Function::new([]);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::RefIsNull);
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Else);
        // pairs(t.left, cons([k,v], pairs(t.right, acc)))
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 3 });
        // [k,v] pair
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 0 });
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 1 });
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 2 });
        // pairs(t.right, acc)
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 4 });
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(self.treap_pairs_idx));
        f.instruction(&Instruction::Call(self.list_cons_idx));
        f.instruction(&Instruction::Call(self.treap_pairs_idx));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End); // function terminal
        f
    }

    /// treap_fold(f, acc, t) : in-order fold applying `f key value acc`.
    /// foldl = ascending (left,node,right); foldr = descending.
    fn emit_treap_fold(&self, rev: bool) -> Function {
        // params f(0), acc(1), t(2).
        let self_idx = if rev { self.treap_foldr_idx } else { self.treap_foldl_idx };
        let (first, second) = if rev { (4u32, 3u32) } else { (3u32, 4u32) };
        let mut f = Function::new([]);
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::RefIsNull);
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Else);
        // acc = fold(f, acc, first); acc = f k v acc; acc = fold(f, acc, second)
        f.instruction(&Instruction::LocalGet(0));
        // fold(f, acc, first-subtree)
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: first });
        f.instruction(&Instruction::Call(self_idx));
        // now stack: [f, acc']; apply f key value acc'
        f.instruction(&Instruction::LocalSet(1));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 0 });
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: 1 });
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&Instruction::LocalSet(1));
        // fold(f, acc, second-subtree)
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&cast_to(T_TNODE));
        f.instruction(&Instruction::StructGet { struct_type_index: T_TNODE, field_index: second });
        f.instruction(&Instruction::Call(self_idx));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End); // function terminal
        f
    }

    /// Set (treap on the stack) → its sorted element list (the treap's keys).
    fn emit_set_to_list(&self, f: &mut Function) {
        push_empty_list(f);
        f.instruction(&Instruction::Call(self.treap_pairs_idx));
        f.instruction(&Instruction::Call(self.dict_keys_idx));
    }

    /// treap_insert_seq(xs, t) : fold treap_insert over a list into treap `t`.
    /// elems=false: `xs` is a list of [k,v] pairs (Dict). elems=true: `xs` is a
    /// list of bare elements inserted with a Unit value (Set). Later entries win.
    fn emit_treap_insert_seq(&self, elems: bool) -> Function {
        // params xs(0), t(1). locals: acc(2),cur(3),x(4):eqref
        let mut f = Function::new([(3, eqref())]);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalSet(2)); // acc = t
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalSet(3)); // cur = xs
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        list_is_empty(&mut f, 3);
        f.instruction(&Instruction::BrIf(1));
        list_head(&mut f, 3);
        f.instruction(&Instruction::LocalSet(4)); // x = head
        // treap_insert(key, value, acc)
        if elems {
            f.instruction(&Instruction::LocalGet(4)); // key = element
            f.instruction(&Instruction::I32Const(0));
            f.instruction(&Instruction::RefI31); // value = ()
        } else {
            f.instruction(&Instruction::LocalGet(4));
            f.instruction(&cast_to(T_ARR));
            f.instruction(&Instruction::I32Const(0));
            f.instruction(&Instruction::ArrayGet(T_ARR)); // key = pair[0]
            f.instruction(&Instruction::LocalGet(4));
            f.instruction(&cast_to(T_ARR));
            f.instruction(&Instruction::I32Const(1));
            f.instruction(&Instruction::ArrayGet(T_ARR)); // value = pair[1]
        }
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::Call(self.treap_insert_idx));
        f.instruction(&Instruction::LocalSet(2));
        list_tail(&mut f, 3);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::End);
        f
    }

    // ---- Json.Encode: Value is a T_CTOR tagged 0=null 1=bool 2=int 3=float
    //      4=string 5=array(List Value) 6=object(List (String,Value)). ----

    /// json_escape(s) : the JSON-quoted, escaped form of `s` (matches
    /// JSON.stringify string escaping).
    fn emit_json_escape(&self) -> Function {
        // params s(0). locals: sstr(1):str, len(2),i(3),c(4):i32, out(5):eqref, d(6):i32
        let mut f = Function::new([(1, ref_to(T_STR)), (3, ValType::I32), (1, eqref()), (1, ValType::I32)]);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_STR));
        f.instruction(&Instruction::LocalSet(1));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::ArrayLen);
        f.instruction(&Instruction::LocalSet(2));
        push_str_const(&mut f, "\"");
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::ArrayGetU(T_STR));
        f.instruction(&Instruction::LocalSet(4));
        // out = str_append(out, <escaped c>)
        f.instruction(&Instruction::LocalGet(5));
        // escaped-piece dispatch, leaving one String on the stack
        let simple: &[(i32, &str)] = &[
            (34, "\\\""),
            (92, "\\\\"),
            (10, "\\n"),
            (9, "\\t"),
            (13, "\\r"),
            (8, "\\b"),
            (12, "\\f"),
        ];
        let mut depth = 0u32;
        for (code, esc) in simple {
            f.instruction(&Instruction::LocalGet(4));
            f.instruction(&Instruction::I32Const(*code));
            f.instruction(&Instruction::I32Eq);
            f.instruction(&Instruction::If(BlockType::Result(eqref())));
            push_str_const(&mut f, esc);
            f.instruction(&Instruction::Else);
            depth += 1;
        }
        // c < 32 → \u00XX
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(32));
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        f.instruction(&Instruction::I32Const(92)); // '\'
        f.instruction(&Instruction::I32Const(117)); // 'u'
        f.instruction(&Instruction::I32Const(48)); // '0'
        f.instruction(&Instruction::I32Const(48)); // '0'
        // hi nibble
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(4));
        f.instruction(&Instruction::I32ShrU);
        f.instruction(&Instruction::LocalSet(6));
        self.hex_digit(&mut f, 6);
        // lo nibble
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(15));
        f.instruction(&Instruction::I32And);
        f.instruction(&Instruction::LocalSet(6));
        self.hex_digit(&mut f, 6);
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_STR, array_size: 6 });
        f.instruction(&Instruction::Else);
        // raw byte
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_STR, array_size: 1 });
        f.instruction(&Instruction::End);
        for _ in 0..depth {
            f.instruction(&Instruction::End);
        }
        f.instruction(&Instruction::Call(self.str_append_idx));
        f.instruction(&Instruction::LocalSet(5));
        bump(&mut f, 3, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(5));
        push_str_const(&mut f, "\"");
        f.instruction(&Instruction::Call(self.str_append_idx));
        f.instruction(&Instruction::End);
        f
    }

    /// Push the ASCII code of the lowercase hex digit for the nibble in `d`.
    fn hex_digit(&self, f: &mut Function, d: u32) {
        f.instruction(&Instruction::LocalGet(d));
        f.instruction(&Instruction::I32Const(48)); // '0'
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(d));
        f.instruction(&Instruction::I32Const(9));
        f.instruction(&Instruction::I32GtS);
        f.instruction(&Instruction::I32Const(39)); // 'a'-'0'-10
        f.instruction(&Instruction::I32Mul);
        f.instruction(&Instruction::I32Add);
    }

    /// json_enc(value, gap, prefix) -> String : the public entry. Allocates one
    /// growable buffer and serializes into it via `json_write` — O(n), with no
    /// per-node intermediate String and no final join. `gap` is the per-level
    /// indent (empty ⇒ compact); `prefix` is the current line's indentation.
    fn emit_json_enc(&self) -> Function {
        // params v(0), gap(1), prefix(2); local sb(3): ref T_SB
        let mut f = Function::new([(1, ref_to(T_SB))]);
        // sb = T_SB { buf: new T_STR(64), len: 0 }
        f.instruction(&Instruction::I32Const(64));
        f.instruction(&Instruction::ArrayNewDefault(T_STR));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::StructNew(T_SB));
        f.instruction(&Instruction::LocalSet(3));
        // json_write(sb, v, gap, prefix)
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::Call(self.json_write_idx));
        // return the tightened buffer
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::Call(self.sb_finish_idx));
        f.instruction(&Instruction::End);
        f
    }

    /// json_write(sb, value, gap, prefix) : append the JSON rendering of `value`
    /// straight into buffer `sb`. Byte-identical to `JSON.stringify(v, null,
    /// indent)`; recurses for arrays/objects. Compact when `gap` is empty.
    fn emit_json_write(&self) -> Function {
        // params sb(0), v(1), gap(2), prefix(3). locals:
        //   tag(4),compact(5):i32, childPrefix(6):eqref, len(7),i(8):i32, sub(9):eqref
        let mut f = Function::new([(2, ValType::I32), (1, eqref()), (2, ValType::I32), (1, eqref())]);
        ctor_tag(&mut f, 1);
        f.instruction(&Instruction::LocalSet(4));
        // compact = len(gap) == 0
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&cast_to(T_STR));
        f.instruction(&Instruction::ArrayLen);
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::LocalSet(5));
        // --- scalars: write then return ---
        // null (tag 0)
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(0));
        push_str_const(&mut f, "null");
        f.instruction(&Instruction::Call(self.sb_push_str_idx));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // bool (tag 1)
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(0));
        ctor_arg0(&mut f, 1);
        f.instruction(&Instruction::RefCastNonNull(HeapType::Abstract { shared: false, ty: AbstractHeapType::I31 }));
        f.instruction(&Instruction::I31GetS);
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        push_str_const(&mut f, "true");
        f.instruction(&Instruction::Else);
        push_str_const(&mut f, "false");
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::Call(self.sb_push_str_idx));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // int (tag 2)
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(2));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(0));
        ctor_arg0(&mut f, 1);
        f.instruction(&Instruction::Call(self.sb_push_int_idx));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // float (tag 3) — integral formatted exactly; non-integral is a known gap
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(3));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(0));
        ctor_arg0(&mut f, 1);
        f.instruction(&cast_to(T_FLOAT));
        f.instruction(&Instruction::StructGet { struct_type_index: T_FLOAT, field_index: 0 });
        f.instruction(&Instruction::I64TruncF64S);
        f.instruction(&Instruction::Call(self.box_int_idx));
        f.instruction(&Instruction::Call(self.sb_push_int_idx));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // string (tag 4)
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(4));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(0));
        ctor_arg0(&mut f, 1);
        f.instruction(&Instruction::Call(self.sb_escape_idx));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // --- array (tag 5) / object (tag 6): shared sequence machinery ---
        // childPrefix = compact ? (unused null) : prefix ++ gap
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::Call(self.str_append_idx));
        f.instruction(&Instruction::LocalSet(6));
        f.instruction(&Instruction::End);
        // sub = items/pairs; len = list_len(sub)
        ctor_arg0(&mut f, 1);
        f.instruction(&Instruction::LocalSet(9));
        list_len(&mut f, 9);
        f.instruction(&Instruction::LocalSet(7));
        // array?
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(5));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Empty));
        self.emit_json_seq_body(&mut f, false);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // else object
        self.emit_json_seq_body(&mut f, true);
        f.instruction(&Instruction::End); // function
        f
    }

    /// The array/object loop of `json_write`, comptime-specialized by `object`.
    /// Appends `"[" (openSep item (sep item)* closeSep) "]"` (braces + `key:`
    /// prefixes for objects) into `sb` (local 0). Uses locals gap(2),prefix(3),
    /// compact(5),childPrefix(6),len(7),i(8),sub(9) set up by the caller.
    fn emit_json_seq_body(&self, f: &mut Function, object: bool) {
        // opening bracket
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Const(if object { 123 } else { 91 }));
        f.instruction(&Instruction::Call(self.sb_push_byte_idx));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(8));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        // "," between elements
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32GtS);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Const(44));
        f.instruction(&Instruction::Call(self.sb_push_byte_idx));
        f.instruction(&Instruction::End);
        // "\n" ++ childPrefix when pretty
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Const(10));
        f.instruction(&Instruction::Call(self.sb_push_byte_idx));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::Call(self.sb_push_str_idx));
        f.instruction(&Instruction::End);
        if object {
            // escape(pair[0]) ++ colon ++ json_write(pair[1])
            f.instruction(&Instruction::LocalGet(0));
            list_elem(f, 9, 8);
            f.instruction(&cast_to(T_ARR));
            f.instruction(&Instruction::I32Const(0));
            f.instruction(&Instruction::ArrayGet(T_ARR));
            f.instruction(&Instruction::Call(self.sb_escape_idx));
            f.instruction(&Instruction::LocalGet(0));
            f.instruction(&Instruction::I32Const(58)); // ':'
            f.instruction(&Instruction::Call(self.sb_push_byte_idx));
            f.instruction(&Instruction::LocalGet(5));
            f.instruction(&Instruction::I32Eqz);
            f.instruction(&Instruction::If(BlockType::Empty));
            f.instruction(&Instruction::LocalGet(0));
            f.instruction(&Instruction::I32Const(32)); // ' '
            f.instruction(&Instruction::Call(self.sb_push_byte_idx));
            f.instruction(&Instruction::End);
            f.instruction(&Instruction::LocalGet(0));
            list_elem(f, 9, 8);
            f.instruction(&cast_to(T_ARR));
            f.instruction(&Instruction::I32Const(1));
            f.instruction(&Instruction::ArrayGet(T_ARR));
            f.instruction(&Instruction::LocalGet(2));
            f.instruction(&Instruction::LocalGet(6));
            f.instruction(&Instruction::Call(self.json_write_idx));
        } else {
            // json_write(sb, item, gap, childPrefix)
            f.instruction(&Instruction::LocalGet(0));
            list_elem(f, 9, 8);
            f.instruction(&Instruction::LocalGet(2));
            f.instruction(&Instruction::LocalGet(6));
            f.instruction(&Instruction::Call(self.json_write_idx));
        }
        bump(f, 8, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End); // loop
        f.instruction(&Instruction::End); // block
        // closeSep: "\n" ++ prefix when pretty and non-empty
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32GtS);
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::I32And);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Const(10));
        f.instruction(&Instruction::Call(self.sb_push_byte_idx));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::Call(self.sb_push_str_idx));
        f.instruction(&Instruction::End);
        // closing bracket
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Const(if object { 125 } else { 93 }));
        f.instruction(&Instruction::Call(self.sb_push_byte_idx));
    }

    /// sb_ensure(sb, extra) : grow `sb.buf` (doubling) so `sb.len + extra` bytes
    /// fit, copying the live prefix. No-op when there's already room.
    fn emit_sb_ensure(&self) -> Function {
        // params sb(0), extra(1). locals: need(2),newcap(3):i32, newbuf(4):ref T_STR
        let mut f = Function::new([(2, ValType::I32), (1, ref_to(T_STR))]);
        // need = sb.len + extra
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::StructGet { struct_type_index: T_SB, field_index: 1 });
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(2));
        // if need > buf.len
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::StructGet { struct_type_index: T_SB, field_index: 0 });
        f.instruction(&Instruction::ArrayLen);
        f.instruction(&Instruction::I32GtS);
        f.instruction(&Instruction::If(BlockType::Empty));
        // newcap = buf.len * 2
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::StructGet { struct_type_index: T_SB, field_index: 0 });
        f.instruction(&Instruction::ArrayLen);
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Shl);
        f.instruction(&Instruction::LocalSet(3));
        // if newcap < need: newcap = need
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::End);
        // newbuf = new T_STR(newcap); copy [0,len)
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::ArrayNewDefault(T_STR));
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::StructGet { struct_type_index: T_SB, field_index: 0 });
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::StructGet { struct_type_index: T_SB, field_index: 1 });
        f.instruction(&Instruction::ArrayCopy { array_type_index_dst: T_STR, array_type_index_src: T_STR });
        // sb.buf = newbuf
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::StructSet { struct_type_index: T_SB, field_index: 0 });
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f
    }

    /// sb_push_byte(sb, c) : append one byte.
    fn emit_sb_push_byte(&self) -> Function {
        // params sb(0), c(1)
        let mut f = Function::new([]);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::Call(self.sb_ensure_idx));
        // buf[len] = c
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::StructGet { struct_type_index: T_SB, field_index: 0 });
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::StructGet { struct_type_index: T_SB, field_index: 1 });
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::ArraySet(T_STR));
        // len += 1
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::StructGet { struct_type_index: T_SB, field_index: 1 });
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::StructSet { struct_type_index: T_SB, field_index: 1 });
        f.instruction(&Instruction::End);
        f
    }

    /// sb_push_str(sb, s) : append all bytes of String `s` (bulk array.copy).
    fn emit_sb_push_str(&self) -> Function {
        // params sb(0), s(1). locals: ss(2):ref T_STR, slen(3):i32
        let mut f = Function::new([(1, ref_to(T_STR)), (1, ValType::I32)]);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_STR));
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::ArrayLen);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::Call(self.sb_ensure_idx));
        // array.copy buf[len..] <- ss[0..slen]
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::StructGet { struct_type_index: T_SB, field_index: 0 });
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::StructGet { struct_type_index: T_SB, field_index: 1 });
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::ArrayCopy { array_type_index_dst: T_STR, array_type_index_src: T_STR });
        // len += slen
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::StructGet { struct_type_index: T_SB, field_index: 1 });
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::StructSet { struct_type_index: T_SB, field_index: 1 });
        f.instruction(&Instruction::End);
        f
    }

    /// Append `ss[a..b)` (all local indices) straight into `sb`'s buffer. A zero
    /// or empty run is a harmless no-op (array.copy of length 0).
    fn sb_flush_run(&self, f: &mut Function, sb: u32, ss: u32, a: u32, b: u32) {
        f.instruction(&Instruction::LocalGet(sb));
        f.instruction(&Instruction::LocalGet(b));
        f.instruction(&Instruction::LocalGet(a));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::Call(self.sb_ensure_idx));
        f.instruction(&Instruction::LocalGet(sb));
        f.instruction(&Instruction::StructGet { struct_type_index: T_SB, field_index: 0 });
        f.instruction(&Instruction::LocalGet(sb));
        f.instruction(&Instruction::StructGet { struct_type_index: T_SB, field_index: 1 });
        f.instruction(&Instruction::LocalGet(ss));
        f.instruction(&Instruction::LocalGet(a));
        f.instruction(&Instruction::LocalGet(b));
        f.instruction(&Instruction::LocalGet(a));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::ArrayCopy { array_type_index_dst: T_STR, array_type_index_src: T_STR });
        f.instruction(&Instruction::LocalGet(sb));
        f.instruction(&Instruction::LocalGet(sb));
        f.instruction(&Instruction::StructGet { struct_type_index: T_SB, field_index: 1 });
        f.instruction(&Instruction::LocalGet(b));
        f.instruction(&Instruction::LocalGet(a));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::StructSet { struct_type_index: T_SB, field_index: 1 });
    }

    /// Append the escape sequence for byte `c` (a local index; known to need
    /// escaping) into `sb`; `d` is a scratch i32 local for hex nibbles.
    fn emit_escape_char(&self, f: &mut Function, sb: u32, c: u32, d: u32) {
        let simple: &[(i32, i32)] = &[
            (34, 34), (92, 92), (10, 110), (9, 116), (13, 114), (8, 98), (12, 102),
        ];
        let mut depth = 0u32;
        for (code, second) in simple {
            f.instruction(&Instruction::LocalGet(c));
            f.instruction(&Instruction::I32Const(*code));
            f.instruction(&Instruction::I32Eq);
            f.instruction(&Instruction::If(BlockType::Empty));
            f.instruction(&Instruction::LocalGet(sb));
            f.instruction(&Instruction::I32Const(92)); // '\'
            f.instruction(&Instruction::Call(self.sb_push_byte_idx));
            f.instruction(&Instruction::LocalGet(sb));
            f.instruction(&Instruction::I32Const(*second));
            f.instruction(&Instruction::Call(self.sb_push_byte_idx));
            f.instruction(&Instruction::Else);
            depth += 1;
        }
        // \u00XX
        for b in [92, 117, 48, 48] {
            f.instruction(&Instruction::LocalGet(sb));
            f.instruction(&Instruction::I32Const(b));
            f.instruction(&Instruction::Call(self.sb_push_byte_idx));
        }
        // hi nibble
        f.instruction(&Instruction::LocalGet(c));
        f.instruction(&Instruction::I32Const(4));
        f.instruction(&Instruction::I32ShrU);
        f.instruction(&Instruction::LocalSet(d));
        f.instruction(&Instruction::LocalGet(sb));
        self.hex_digit(f, d);
        f.instruction(&Instruction::Call(self.sb_push_byte_idx));
        // lo nibble
        f.instruction(&Instruction::LocalGet(c));
        f.instruction(&Instruction::I32Const(15));
        f.instruction(&Instruction::I32And);
        f.instruction(&Instruction::LocalSet(d));
        f.instruction(&Instruction::LocalGet(sb));
        self.hex_digit(f, d);
        f.instruction(&Instruction::Call(self.sb_push_byte_idx));
        for _ in 0..depth {
            f.instruction(&Instruction::End);
        }
    }

    /// sb_escape(sb, s) : append the JSON-quoted, escaped form of `s`, copying
    /// unescaped runs in bulk and emitting escapes only where needed.
    fn emit_sb_escape(&self) -> Function {
        // params sb(0), s(1). locals: ss(2):ref T_STR, len(3),i(4),run(5),c(6),d(7):i32
        let mut f = Function::new([(1, ref_to(T_STR)), (5, ValType::I32)]);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_STR));
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::ArrayLen);
        f.instruction(&Instruction::LocalSet(3));
        // opening quote
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Const(34));
        f.instruction(&Instruction::Call(self.sb_push_byte_idx));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        // c = ss[i]
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::ArrayGetU(T_STR));
        f.instruction(&Instruction::LocalSet(6));
        // needsEscape = c<32 || c==34 || c==92
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I32Const(32));
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I32Const(34));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::I32Or);
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I32Const(92));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::I32Or);
        f.instruction(&Instruction::If(BlockType::Empty));
        // flush clean run [run, i)
        self.sb_flush_run(&mut f, 0, 2, 5, 4);
        // escape c
        self.emit_escape_char(&mut f, 0, 6, 7);
        // run = i + 1
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::End);
        bump(&mut f, 4, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        // flush trailing run [run, len)
        self.sb_flush_run(&mut f, 0, 2, 5, 3);
        // closing quote
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Const(34));
        f.instruction(&Instruction::Call(self.sb_push_byte_idx));
        f.instruction(&Instruction::End);
        f
    }

    /// sb_push_int(sb, boxedInt) : format an Int in decimal straight into the
    /// buffer (itoa — no intermediate String). Digits are written MSD-first into
    /// reserved space, then `len` is advanced.
    fn emit_sb_push_int(&self) -> Function {
        // params sb(0), v(1). locals: n(2),tmp(4):i64; d(3),start(5),idx(6):i32
        let mut f = Function::new([(1, ValType::I64), (1, ValType::I32), (1, ValType::I64), (2, ValType::I32)]);
        // n = unbox_int(v)
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(self.unbox_int_idx));
        f.instruction(&Instruction::LocalSet(2));
        // if n < 0: emit '-', n = -n
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I64Const(0));
        f.instruction(&Instruction::I64LtS);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Const(45)); // '-'
        f.instruction(&Instruction::Call(self.sb_push_byte_idx));
        f.instruction(&Instruction::I64Const(0));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I64Sub);
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::End);
        // count digits: d = 1; tmp = n / 10; while tmp > 0 { d++; tmp /= 10 }
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I64Const(10));
        f.instruction(&Instruction::I64DivS);
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I64Const(0));
        f.instruction(&Instruction::I64LeS);
        f.instruction(&Instruction::BrIf(1));
        bump(&mut f, 3, 1);
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I64Const(10));
        f.instruction(&Instruction::I64DivS);
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        // reserve d bytes; start = len
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::Call(self.sb_ensure_idx));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::StructGet { struct_type_index: T_SB, field_index: 1 });
        f.instruction(&Instruction::LocalSet(5));
        // idx = start + d - 1; write digits LSD-first backward to idx == start
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::LocalSet(6));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::BrIf(1));
        // buf[idx] = '0' + (n % 10)
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::StructGet { struct_type_index: T_SB, field_index: 0 });
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I32Const(48));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I64Const(10));
        f.instruction(&Instruction::I64RemS);
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::ArraySet(T_STR));
        // n /= 10; idx--
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I64Const(10));
        f.instruction(&Instruction::I64DivS);
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::LocalSet(6));
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        // len += d
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::StructGet { struct_type_index: T_SB, field_index: 1 });
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::StructSet { struct_type_index: T_SB, field_index: 1 });
        f.instruction(&Instruction::End);
        f
    }

    /// sb_finish(sb) -> String : the buffer's live bytes as a `T_STR`. Returns
    /// the backing array directly when exactly full, else an exact-size copy.
    fn emit_sb_finish(&self) -> Function {
        // params sb(0). locals: out(1):ref T_STR
        let mut f = Function::new([(1, ref_to(T_STR))]);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::StructGet { struct_type_index: T_SB, field_index: 1 });
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::StructGet { struct_type_index: T_SB, field_index: 0 });
        f.instruction(&Instruction::ArrayLen);
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::StructGet { struct_type_index: T_SB, field_index: 0 });
        f.instruction(&Instruction::Else);
        // out = new T_STR(len); copy [0,len)
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::StructGet { struct_type_index: T_SB, field_index: 1 });
        f.instruction(&Instruction::ArrayNewDefault(T_STR));
        f.instruction(&Instruction::LocalSet(1));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::StructGet { struct_type_index: T_SB, field_index: 0 });
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::StructGet { struct_type_index: T_SB, field_index: 1 });
        f.instruction(&Instruction::ArrayCopy { array_type_index_dst: T_STR, array_type_index_src: T_STR });
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f
    }

    /// json_dict_pairs(toKey, toVal, d) : `List (String, Value)` from a Dict.
    fn emit_json_dict_pairs(&self) -> Function {
        // params toKey(0), toVal(1), d(2). locals: len(3),i(4):i32,
        //   ndata(5):ref T_ARR, pair(6):eqref
        let mut f = Function::new([(2, ValType::I32), (1, ref_to(T_ARR)), (1, eqref())]);
        list_len(&mut f, 2);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::ArrayNewDefault(T_ARR));
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        list_elem(&mut f, 2, 4);
        f.instruction(&Instruction::LocalSet(6));
        // ndata[i] = (toKey pair[0], toVal pair[1])
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(0));
        self.load_arr(6, 0, &mut f);
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&Instruction::LocalGet(1));
        self.load_arr(6, 1, &mut f);
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 2 });
        f.instruction(&Instruction::ArraySet(T_ARR));
        bump(&mut f, 4, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::StructNew(T_BACK));
        f.instruction(&Instruction::StructNew(T_LIST));
        f.instruction(&Instruction::End);
        f
    }

    // ---- Json.Decode: a recursive-descent parser (globals 0=jstr,1=jpos,
    //      2=jerr) producing the tagged Value, and a tagged-decoder interpreter.

    /// json_skipws() : advance jpos past JSON whitespace.
    fn emit_json_skipws(&self) -> Function {
        let mut f = Function::new([(1, ValType::I32)]); // c
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        json_cur(&mut f);
        f.instruction(&Instruction::LocalSet(0));
        // stop at end (-1)
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::BrIf(1));
        // is ws? 32/9/10/13
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Const(32));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Const(9));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::I32Or);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Const(10));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::I32Or);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Const(13));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::I32Or);
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::BrIf(1)); // not ws → stop
        f.instruction(&Instruction::GlobalGet(1));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::GlobalSet(1));
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f
    }

    /// json_pstr() : parse a `"…"` string at jpos, returning its content String.
    fn emit_json_pstr(&self) -> Function {
        // locals: c(0),e(1),cp(2),b(3),k(4):i32, out(5):eqref
        let mut f = Function::new([(5, ValType::I32), (1, eqref())]);
        // skip opening quote
        f.instruction(&Instruction::GlobalGet(1));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::GlobalSet(1));
        push_str_const(&mut f, "");
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        json_cur(&mut f);
        f.instruction(&Instruction::LocalSet(0));
        // end of input → error
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::GlobalSet(2));
        f.instruction(&Instruction::Br(2));
        f.instruction(&Instruction::End);
        // consume c
        f.instruction(&Instruction::GlobalGet(1));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::GlobalSet(1));
        // closing quote?
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Const(34));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::BrIf(1));
        // escape?
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Const(92));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Empty));
        // e = next byte
        json_cur(&mut f);
        f.instruction(&Instruction::LocalSet(1));
        f.instruction(&Instruction::GlobalGet(1));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::GlobalSet(1));
        // out ++= piece
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(117)); // 'u'
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        // \uXXXX → codepoint → str_from_char
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(2)); // cp
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(4)); // k
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(4));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        // hd = hexval(cur); jpos++
        json_cur(&mut f);
        f.instruction(&Instruction::LocalSet(3));
        self.json_hexval(&mut f, 3);
        // cp = cp*16 + hd
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(16));
        f.instruction(&Instruction::I32Mul);
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::GlobalGet(1));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::GlobalSet(1));
        bump(&mut f, 4, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::RefI31);
        f.instruction(&Instruction::Call(self.str_from_char_idx));
        f.instruction(&Instruction::Else);
        // simple escape → one byte
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalSet(3)); // b = e (", \, / map to self)
        for (esc, byte) in [(110, 10), (116, 9), (114, 13), (98, 8), (102, 12)] {
            f.instruction(&Instruction::LocalGet(1));
            f.instruction(&Instruction::I32Const(esc));
            f.instruction(&Instruction::I32Eq);
            f.instruction(&Instruction::If(BlockType::Empty));
            f.instruction(&Instruction::I32Const(byte));
            f.instruction(&Instruction::LocalSet(3));
            f.instruction(&Instruction::End);
        }
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_STR, array_size: 1 });
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::Call(self.str_append_idx));
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::Else);
        // ordinary byte
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_STR, array_size: 1 });
        f.instruction(&Instruction::Call(self.str_append_idx));
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::End);
        f
    }

    /// Convert the hex-digit byte in local `d` to its value (in place).
    fn json_hexval(&self, f: &mut Function, d: u32) {
        // d = (d>=97 ? d-87 : (d>=65 ? d-55 : d-48))
        f.instruction(&Instruction::LocalGet(d));
        f.instruction(&Instruction::I32Const(97));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::If(BlockType::Result(ValType::I32)));
        f.instruction(&Instruction::LocalGet(d));
        f.instruction(&Instruction::I32Const(87));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::LocalGet(d));
        f.instruction(&Instruction::I32Const(65));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::If(BlockType::Result(ValType::I32)));
        f.instruction(&Instruction::LocalGet(d));
        f.instruction(&Instruction::I32Const(55));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::LocalGet(d));
        f.instruction(&Instruction::I32Const(48));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
    }

    /// json_pnum() : parse a number → JINT (tag 2) if it has no '.'/'e', else
    /// JFLOAT (tag 3). (Hand-rolled decimal→f64: exact for short inputs.)
    fn emit_json_pnum(&self) -> Function {
        // locals: c(0),sign(1),isf(2),exp(3),esign(4):i32, ival(5):i64,
        //   fval(6),scale(7):f64
        let mut f = Function::new([(5, ValType::I32), (1, ValType::I64), (2, ValType::F64)]);
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::LocalSet(1)); // sign
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(2)); // isf
        f.instruction(&Instruction::I64Const(0));
        f.instruction(&Instruction::LocalSet(5)); // ival
        f.instruction(&Instruction::F64Const(0.0.into()));
        f.instruction(&Instruction::LocalSet(6)); // fval
        // sign
        json_cur(&mut f);
        f.instruction(&Instruction::I32Const(45));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(-1));
        f.instruction(&Instruction::LocalSet(1));
        f.instruction(&Instruction::GlobalGet(1));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::GlobalSet(1));
        f.instruction(&Instruction::End);
        // integer digits
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        json_cur(&mut f);
        f.instruction(&Instruction::LocalSet(0));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Const(48));
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Const(57));
        f.instruction(&Instruction::I32GtS);
        f.instruction(&Instruction::I32Or);
        f.instruction(&Instruction::BrIf(1));
        // ival = ival*10 + (c-48); fval = fval*10 + (c-48)
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I64Const(10));
        f.instruction(&Instruction::I64Mul);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Const(48));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::I64ExtendI32S);
        f.instruction(&Instruction::I64Add);
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::F64Const(10.0.into()));
        f.instruction(&Instruction::F64Mul);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Const(48));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::F64ConvertI32S);
        f.instruction(&Instruction::F64Add);
        f.instruction(&Instruction::LocalSet(6));
        f.instruction(&Instruction::GlobalGet(1));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::GlobalSet(1));
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        // fraction
        json_cur(&mut f);
        f.instruction(&Instruction::I32Const(46)); // '.'
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::LocalSet(2)); // isf
        f.instruction(&Instruction::GlobalGet(1));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::GlobalSet(1));
        f.instruction(&Instruction::F64Const(0.1.into()));
        f.instruction(&Instruction::LocalSet(7)); // scale
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        json_cur(&mut f);
        f.instruction(&Instruction::LocalSet(0));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Const(48));
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Const(57));
        f.instruction(&Instruction::I32GtS);
        f.instruction(&Instruction::I32Or);
        f.instruction(&Instruction::BrIf(1));
        // fval += (c-48)*scale ; scale *= 0.1
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Const(48));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::F64ConvertI32S);
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::F64Mul);
        f.instruction(&Instruction::F64Add);
        f.instruction(&Instruction::LocalSet(6));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::F64Const(0.1.into()));
        f.instruction(&Instruction::F64Mul);
        f.instruction(&Instruction::LocalSet(7));
        f.instruction(&Instruction::GlobalGet(1));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::GlobalSet(1));
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        // exponent
        json_cur(&mut f);
        f.instruction(&Instruction::I32Const(32));
        f.instruction(&Instruction::I32Or); // lower-case: 'E'|32 == 'e'
        f.instruction(&Instruction::I32Const(101)); // 'e'
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::GlobalGet(1));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::GlobalSet(1));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::LocalSet(4)); // esign
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(3)); // exp
        // optional sign
        json_cur(&mut f);
        f.instruction(&Instruction::I32Const(45));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(-1));
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::GlobalGet(1));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::GlobalSet(1));
        f.instruction(&Instruction::Else);
        json_cur(&mut f);
        f.instruction(&Instruction::I32Const(43));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::GlobalGet(1));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::GlobalSet(1));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        json_cur(&mut f);
        f.instruction(&Instruction::LocalSet(0));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Const(48));
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Const(57));
        f.instruction(&Instruction::I32GtS);
        f.instruction(&Instruction::I32Or);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Const(10));
        f.instruction(&Instruction::I32Mul);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Const(48));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::GlobalGet(1));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::GlobalSet(1));
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        // fval *= 10^(esign*exp) via repeated mul/div
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32GtS);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::F64Const(10.0.into()));
        f.instruction(&Instruction::F64Mul);
        f.instruction(&Instruction::LocalSet(6));
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::F64Const(10.0.into()));
        f.instruction(&Instruction::F64Div);
        f.instruction(&Instruction::LocalSet(6));
        f.instruction(&Instruction::End);
        dec(&mut f, 3);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End); // exponent if
        // build result
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        // JFLOAT: tag 3 [ sign * fval ]
        f.instruction(&Instruction::I32Const(3));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::F64ConvertI32S);
        f.instruction(&Instruction::F64Mul);
        f.instruction(&Instruction::StructNew(T_FLOAT));
        wrap1(&mut f);
        f.instruction(&Instruction::Else);
        // JINT: tag 2 [ sign * ival ]
        f.instruction(&Instruction::I32Const(2));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I64ExtendI32S);
        f.instruction(&Instruction::I64Mul);
        f.instruction(&Instruction::Call(self.box_int_idx));
        wrap1(&mut f);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f
    }

    /// json_pval() : parse any JSON value at jpos (skips leading ws).
    fn emit_json_pval(&self) -> Function {
        let mut f = Function::new([(1, ValType::I32)]); // c
        f.instruction(&Instruction::Call(self.json_skipws_idx));
        json_cur(&mut f);
        f.instruction(&Instruction::LocalSet(0));
        // '{'
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Const(123));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        f.instruction(&Instruction::Call(self.json_pobj_idx));
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Const(91)); // '['
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        f.instruction(&Instruction::Call(self.json_parr_idx));
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Const(34)); // '"'
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        f.instruction(&Instruction::I32Const(4)); // JSTRING
        f.instruction(&Instruction::Call(self.json_pstr_idx));
        wrap1(&mut f);
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Const(116)); // 't'
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        self.json_advance(&mut f, 4);
        f.instruction(&Instruction::I32Const(1)); // JBOOL
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::RefI31);
        wrap1(&mut f);
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Const(102)); // 'f'
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        self.json_advance(&mut f, 5);
        f.instruction(&Instruction::I32Const(1)); // JBOOL
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::RefI31);
        wrap1(&mut f);
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Const(110)); // 'n'
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        self.json_advance(&mut f, 4);
        f.instruction(&Instruction::I32Const(0)); // JNULL
        f.instruction(&Instruction::RefNull(HeapType::Concrete(T_ARR)));
        f.instruction(&Instruction::StructNew(T_CTOR));
        f.instruction(&Instruction::Else);
        // number if '-' or digit, else error
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Const(45));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Const(48));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Const(57));
        f.instruction(&Instruction::I32LeS);
        f.instruction(&Instruction::I32And);
        f.instruction(&Instruction::I32Or);
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        f.instruction(&Instruction::Call(self.json_pnum_idx));
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::GlobalSet(2)); // jerr
        f.instruction(&Instruction::I32Const(0)); // JNULL placeholder
        f.instruction(&Instruction::RefNull(HeapType::Concrete(T_ARR)));
        f.instruction(&Instruction::StructNew(T_CTOR));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f
    }

    /// Advance jpos by `n` (consume a literal like true/false/null).
    fn json_advance(&self, f: &mut Function, n: i32) {
        f.instruction(&Instruction::GlobalGet(1));
        f.instruction(&Instruction::I32Const(n));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::GlobalSet(1));
    }

    /// json_parr() : parse a `[…]` array at jpos → JARRAY.
    fn emit_json_parr(&self) -> Function {
        // locals: acc(0):eqref, c(1):i32
        let mut f = Function::new([(1, eqref()), (1, ValType::I32)]);
        self.json_advance(&mut f, 1); // '['
        push_empty_list(&mut f);
        f.instruction(&Instruction::LocalSet(0));
        f.instruction(&Instruction::Call(self.json_skipws_idx));
        json_cur(&mut f);
        f.instruction(&Instruction::I32Const(93)); // ']'
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Empty));
        self.json_advance(&mut f, 1);
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        // acc = cons(pval(), acc)
        f.instruction(&Instruction::Call(self.json_pval_idx));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::Call(self.list_cons_idx));
        f.instruction(&Instruction::LocalSet(0));
        f.instruction(&Instruction::Call(self.json_skipws_idx));
        json_cur(&mut f);
        f.instruction(&Instruction::LocalSet(1));
        self.json_advance(&mut f, 1); // consume separator/close
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(44)); // ','
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::BrIf(0));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(93)); // ']'
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::GlobalSet(2)); // jerr
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        // JARRAY tag 5 [ reverse acc ]
        f.instruction(&Instruction::I32Const(5));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::Call(self.list_reverse_idx));
        wrap1(&mut f);
        f.instruction(&Instruction::End);
        f
    }

    /// json_pobj() : parse a `{…}` object at jpos → JOBJECT.
    fn emit_json_pobj(&self) -> Function {
        // locals: acc(0),key(1),v(2):eqref, c(3):i32
        let mut f = Function::new([(3, eqref()), (1, ValType::I32)]);
        self.json_advance(&mut f, 1); // '{'
        push_empty_list(&mut f);
        f.instruction(&Instruction::LocalSet(0));
        f.instruction(&Instruction::Call(self.json_skipws_idx));
        json_cur(&mut f);
        f.instruction(&Instruction::I32Const(125)); // '}'
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Empty));
        self.json_advance(&mut f, 1);
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::Call(self.json_skipws_idx));
        // key
        f.instruction(&Instruction::Call(self.json_pstr_idx));
        f.instruction(&Instruction::LocalSet(1));
        f.instruction(&Instruction::Call(self.json_skipws_idx));
        // expect ':'
        json_cur(&mut f);
        f.instruction(&Instruction::I32Const(58));
        f.instruction(&Instruction::I32Ne);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::GlobalSet(2));
        f.instruction(&Instruction::Br(2));
        f.instruction(&Instruction::End);
        self.json_advance(&mut f, 1); // ':'
        f.instruction(&Instruction::Call(self.json_pval_idx));
        f.instruction(&Instruction::LocalSet(2));
        // acc = cons([key, v], acc)
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 2 });
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::Call(self.list_cons_idx));
        f.instruction(&Instruction::LocalSet(0));
        f.instruction(&Instruction::Call(self.json_skipws_idx));
        json_cur(&mut f);
        f.instruction(&Instruction::LocalSet(3));
        self.json_advance(&mut f, 1);
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Const(44)); // ','
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::BrIf(0));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Const(125)); // '}'
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::GlobalSet(2));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::I32Const(6));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::Call(self.list_reverse_idx));
        wrap1(&mut f);
        f.instruction(&Instruction::End);
        f
    }

    /// json_parse(s) : String -> Result Error Value.
    fn emit_json_parse(&self) -> Function {
        // param s(0). local v(1):eqref
        let mut f = Function::new([(1, eqref())]);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_STR));
        f.instruction(&Instruction::GlobalSet(0)); // jstr
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::GlobalSet(1)); // jpos
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::GlobalSet(2)); // jerr
        f.instruction(&Instruction::Call(self.json_pval_idx));
        f.instruction(&Instruction::LocalSet(1));
        f.instruction(&Instruction::Call(self.json_skipws_idx));
        // ok if jerr==0 && jpos>=len
        f.instruction(&Instruction::GlobalGet(2));
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::GlobalGet(1));
        f.instruction(&Instruction::GlobalGet(0));
        f.instruction(&cast_to(T_STR));
        f.instruction(&Instruction::ArrayLen);
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::I32And);
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        f.instruction(&Instruction::I32Const(0)); // Ok
        f.instruction(&Instruction::LocalGet(1));
        wrap1(&mut f);
        f.instruction(&Instruction::Else);
        push_decode_err(&mut f);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f
    }

    /// json_run(dec, val) : interpret a tagged decoder against a JSON Value,
    /// returning `Result Error a`. Decoder tags: 0 string,1 int,2 float,3 bool,
    /// 4 value,5 null,6 list,7 array,8 field,9 index,10 at,11 keyValuePairs,
    /// 12 dict,13 maybe,14 nullable,15 map,16 map2,17 map3,18 andThen,19 oneOf,
    /// 20 succeed,21 fail,22 lazy. Value tags: 0 null,1 bool,2 int,3 float,
    /// 4 string,5 array,6 object.
    fn emit_json_run(&self) -> Function {
        // params dec(0), val(1). locals: t(2),len(4),i(5),ii(6):i32,
        //   acc(7),r(8),items(9),d(10),pair(11),sub(12):eqref, fv(13):f64
        let mut f = Function::new([(5, ValType::I32), (6, eqref()), (1, ValType::F64)]);
        ctor_tag(&mut f, 0);
        f.instruction(&Instruction::LocalSet(2));
        // helper closures capturing nothing — emit inline via small fns:
        let arm = |f: &mut Function, tag: i32| {
            f.instruction(&Instruction::LocalGet(2));
            f.instruction(&Instruction::I32Const(tag));
            f.instruction(&Instruction::I32Eq);
            f.instruction(&Instruction::If(BlockType::Empty));
        };
        let vtag_is = |f: &mut Function, tag: i32| {
            ctor_tag(f, 1);
            f.instruction(&Instruction::I32Const(tag));
            f.instruction(&Instruction::I32Eq);
        };
        // 0 string
        arm(&mut f, 0);
        vtag_is(&mut f, 4);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(0));
        ctor_arg0(&mut f, 1);
        wrap1(&mut f);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        push_decode_err(&mut f);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // 1 int
        arm(&mut f, 1);
        vtag_is(&mut f, 2);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(0));
        ctor_arg0(&mut f, 1);
        wrap1(&mut f);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        vtag_is(&mut f, 3);
        f.instruction(&Instruction::If(BlockType::Empty));
        ctor_arg0(&mut f, 1);
        f.instruction(&cast_to(T_FLOAT));
        f.instruction(&Instruction::StructGet { struct_type_index: T_FLOAT, field_index: 0 });
        f.instruction(&Instruction::LocalSet(13));
        f.instruction(&Instruction::LocalGet(13));
        f.instruction(&Instruction::LocalGet(13));
        f.instruction(&Instruction::I64TruncF64S);
        f.instruction(&Instruction::F64ConvertI64S);
        f.instruction(&Instruction::F64Eq);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(13));
        f.instruction(&Instruction::I64TruncF64S);
        f.instruction(&Instruction::Call(self.box_int_idx));
        wrap1(&mut f);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        push_decode_err(&mut f);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // 2 float
        arm(&mut f, 2);
        vtag_is(&mut f, 3);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(0));
        ctor_arg0(&mut f, 1);
        wrap1(&mut f);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        vtag_is(&mut f, 2);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(0));
        ctor_arg0(&mut f, 1);
        f.instruction(&Instruction::Call(self.unbox_int_idx));
        f.instruction(&Instruction::F64ConvertI64S);
        f.instruction(&Instruction::StructNew(T_FLOAT));
        wrap1(&mut f);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        push_decode_err(&mut f);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // 3 bool
        arm(&mut f, 3);
        vtag_is(&mut f, 1);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(0));
        ctor_arg0(&mut f, 1);
        wrap1(&mut f);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        push_decode_err(&mut f);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // 4 value
        arm(&mut f, 4);
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(1));
        wrap1(&mut f);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // 5 null
        arm(&mut f, 5);
        vtag_is(&mut f, 0);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(0));
        ctor_arg0(&mut f, 0);
        wrap1(&mut f);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        push_decode_err(&mut f);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // 6 list / 7 array
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(6));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(7));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::I32Or);
        f.instruction(&Instruction::If(BlockType::Empty));
        vtag_is(&mut f, 5);
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Empty));
        push_decode_err(&mut f);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        ctor_arg0(&mut f, 1);
        f.instruction(&Instruction::LocalSet(9)); // items
        ctor_arg0(&mut f, 0);
        f.instruction(&Instruction::LocalSet(12)); // sub
        push_empty_list(&mut f);
        f.instruction(&Instruction::LocalSet(7)); // acc
        list_len(&mut f, 9);
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(12));
        list_elem(&mut f, 9, 5);
        f.instruction(&Instruction::Call(self.json_run_idx));
        f.instruction(&Instruction::LocalSet(8));
        ctor_tag(&mut f, 8);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        ctor_arg0(&mut f, 8);
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::Call(self.list_cons_idx));
        f.instruction(&Instruction::LocalSet(7));
        bump(&mut f, 5, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::Call(self.list_reverse_idx));
        wrap1(&mut f);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // 8 field
        arm(&mut f, 8);
        vtag_is(&mut f, 6);
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Empty));
        push_decode_err(&mut f);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        ctor_arg0(&mut f, 1);
        f.instruction(&Instruction::LocalSet(9)); // pairs
        list_len(&mut f, 9);
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        list_elem(&mut f, 9, 5);
        f.instruction(&Instruction::LocalSet(11)); // pair
        ctor_arg0(&mut f, 0); // name
        self.load_arr(11, 0, &mut f);
        f.instruction(&Instruction::Call(self.val_eq_idx));
        f.instruction(&Instruction::RefCastNonNull(HeapType::Abstract { shared: false, ty: AbstractHeapType::I31 }));
        f.instruction(&Instruction::I31GetS);
        f.instruction(&Instruction::If(BlockType::Empty));
        ctor_argn(&mut f, 0, 1); // sub
        self.load_arr(11, 1, &mut f);
        f.instruction(&Instruction::Call(self.json_run_idx));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        bump(&mut f, 5, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        push_decode_err(&mut f);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // 9 index
        arm(&mut f, 9);
        vtag_is(&mut f, 5);
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Empty));
        push_decode_err(&mut f);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        ctor_arg0(&mut f, 1);
        f.instruction(&Instruction::LocalSet(9));
        ctor_arg0(&mut f, 0);
        f.instruction(&Instruction::Call(self.unbox_int_idx));
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::LocalSet(6));
        list_len(&mut f, 9);
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::I32And);
        f.instruction(&Instruction::If(BlockType::Empty));
        ctor_argn(&mut f, 0, 1);
        list_elem(&mut f, 9, 6);
        f.instruction(&Instruction::Call(self.json_run_idx));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        push_decode_err(&mut f);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // 10 at: build nested DField(name, ...) then run
        arm(&mut f, 10);
        ctor_arg0(&mut f, 0); // names
        f.instruction(&Instruction::LocalSet(9));
        ctor_argn(&mut f, 0, 1); // sub
        f.instruction(&Instruction::LocalSet(10)); // d
        list_len(&mut f, 9);
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::BrIf(1));
        // d = DField(names[i], d)  (tag 8)
        f.instruction(&Instruction::I32Const(8));
        list_elem(&mut f, 9, 5);
        f.instruction(&Instruction::LocalGet(10));
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 2 });
        f.instruction(&Instruction::StructNew(T_CTOR));
        f.instruction(&Instruction::LocalSet(10));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(10));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(self.json_run_idx));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // 11 keyValuePairs / 12 dict
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(11));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(12));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::I32Or);
        f.instruction(&Instruction::If(BlockType::Empty));
        vtag_is(&mut f, 6);
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Empty));
        push_decode_err(&mut f);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        ctor_arg0(&mut f, 1);
        f.instruction(&Instruction::LocalSet(9)); // pairs
        ctor_arg0(&mut f, 0);
        f.instruction(&Instruction::LocalSet(12)); // sub
        push_empty_list(&mut f);
        f.instruction(&Instruction::LocalSet(7));
        list_len(&mut f, 9);
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        list_elem(&mut f, 9, 5);
        f.instruction(&Instruction::LocalSet(11)); // pair
        f.instruction(&Instruction::LocalGet(12));
        self.load_arr(11, 1, &mut f);
        f.instruction(&Instruction::Call(self.json_run_idx));
        f.instruction(&Instruction::LocalSet(8)); // r
        ctor_tag(&mut f, 8);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // acc = cons([key, r.arg0], acc)
        self.load_arr(11, 0, &mut f);
        ctor_arg0(&mut f, 8);
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 2 });
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::Call(self.list_cons_idx));
        f.instruction(&Instruction::LocalSet(7));
        bump(&mut f, 5, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        // reversed kv list
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::Call(self.list_reverse_idx));
        f.instruction(&Instruction::LocalSet(7));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(12));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        // Ok (Dict.fromList kv)
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(7));
        push_empty_list(&mut f);
        f.instruction(&Instruction::Call(self.dict_from_list_idx));
        wrap1(&mut f);
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(7));
        wrap1(&mut f);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // 13 maybe
        arm(&mut f, 13);
        ctor_arg0(&mut f, 0);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(self.json_run_idx));
        f.instruction(&Instruction::LocalSet(8));
        ctor_tag(&mut f, 8);
        f.instruction(&Instruction::I32Eqz); // Ok
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        f.instruction(&Instruction::I32Const(0)); // Ok
        f.instruction(&Instruction::I32Const(0)); // Just
        ctor_arg0(&mut f, 8);
        wrap1(&mut f);
        wrap1(&mut f);
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::I32Const(0)); // Ok
        f.instruction(&Instruction::I32Const(1)); // Nothing
        f.instruction(&Instruction::RefNull(HeapType::Concrete(T_ARR)));
        f.instruction(&Instruction::StructNew(T_CTOR));
        wrap1(&mut f);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // 14 nullable
        arm(&mut f, 14);
        vtag_is(&mut f, 0);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(0)); // Ok
        f.instruction(&Instruction::I32Const(1)); // Nothing
        f.instruction(&Instruction::RefNull(HeapType::Concrete(T_ARR)));
        f.instruction(&Instruction::StructNew(T_CTOR));
        wrap1(&mut f);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        ctor_arg0(&mut f, 0);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(self.json_run_idx));
        f.instruction(&Instruction::LocalSet(8));
        ctor_tag(&mut f, 8);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::I32Const(0)); // Ok
        f.instruction(&Instruction::I32Const(0)); // Just
        ctor_arg0(&mut f, 8);
        wrap1(&mut f);
        wrap1(&mut f);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // 15 map
        arm(&mut f, 15);
        ctor_argn(&mut f, 0, 1); // sub
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(self.json_run_idx));
        f.instruction(&Instruction::LocalSet(8));
        ctor_tag(&mut f, 8);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::I32Const(0)); // Ok
        ctor_arg0(&mut f, 0); // f
        ctor_arg0(&mut f, 8); // x
        f.instruction(&Instruction::Call(self.apply1_idx));
        wrap1(&mut f);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // 16 map2
        arm(&mut f, 16);
        ctor_argn(&mut f, 0, 1);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(self.json_run_idx));
        f.instruction(&Instruction::LocalSet(8));
        ctor_tag(&mut f, 8);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        ctor_arg0(&mut f, 8);
        f.instruction(&Instruction::LocalSet(7)); // x1
        ctor_argn(&mut f, 0, 2);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(self.json_run_idx));
        f.instruction(&Instruction::LocalSet(8));
        ctor_tag(&mut f, 8);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::I32Const(0)); // Ok
        ctor_arg0(&mut f, 0); // f
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::Call(self.apply1_idx));
        ctor_arg0(&mut f, 8);
        f.instruction(&Instruction::Call(self.apply1_idx));
        wrap1(&mut f);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // 17 map3
        arm(&mut f, 17);
        ctor_argn(&mut f, 0, 1);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(self.json_run_idx));
        f.instruction(&Instruction::LocalSet(8));
        ctor_tag(&mut f, 8);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        ctor_arg0(&mut f, 8);
        f.instruction(&Instruction::LocalSet(7)); // x1
        ctor_argn(&mut f, 0, 2);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(self.json_run_idx));
        f.instruction(&Instruction::LocalSet(8));
        ctor_tag(&mut f, 8);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        ctor_arg0(&mut f, 8);
        f.instruction(&Instruction::LocalSet(9)); // x2
        ctor_argn(&mut f, 0, 3);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(self.json_run_idx));
        f.instruction(&Instruction::LocalSet(8));
        ctor_tag(&mut f, 8);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::I32Const(0)); // Ok
        ctor_arg0(&mut f, 0);
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::Call(self.apply1_idx));
        ctor_arg0(&mut f, 8);
        f.instruction(&Instruction::Call(self.apply1_idx));
        wrap1(&mut f);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // 18 andThen
        arm(&mut f, 18);
        ctor_argn(&mut f, 0, 1); // sub
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(self.json_run_idx));
        f.instruction(&Instruction::LocalSet(8));
        ctor_tag(&mut f, 8);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        ctor_arg0(&mut f, 0); // f
        ctor_arg0(&mut f, 8); // x
        f.instruction(&Instruction::Call(self.apply1_idx)); // f x = Decoder
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(self.json_run_idx));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // 19 oneOf
        arm(&mut f, 19);
        ctor_arg0(&mut f, 0);
        f.instruction(&Instruction::LocalSet(9)); // decoders
        list_len(&mut f, 9);
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        list_elem(&mut f, 9, 5);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(self.json_run_idx));
        f.instruction(&Instruction::LocalSet(8));
        ctor_tag(&mut f, 8);
        f.instruction(&Instruction::I32Eqz); // Ok
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        bump(&mut f, 5, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        push_decode_err(&mut f);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // 20 succeed
        arm(&mut f, 20);
        f.instruction(&Instruction::I32Const(0));
        ctor_arg0(&mut f, 0);
        wrap1(&mut f);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // 21 fail
        arm(&mut f, 21);
        push_decode_err(&mut f);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // 22 lazy
        arm(&mut f, 22);
        ctor_arg0(&mut f, 0); // thunk
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::RefI31); // Unit
        f.instruction(&Instruction::Call(self.apply1_idx)); // thunk () = Decoder
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(self.json_run_idx));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // fallback
        push_decode_err(&mut f);
        f.instruction(&Instruction::End);
        f
    }

    /// json_decstr(dec, s) : decodeString — parse then run.
    fn emit_json_decstr(&self) -> Function {
        // params dec(0), s(1). local p(2):eqref
        let mut f = Function::new([(1, eqref())]);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(self.json_parse_idx));
        f.instruction(&Instruction::LocalSet(2));
        ctor_tag(&mut f, 2);
        f.instruction(&Instruction::I32Eqz); // Ok
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        f.instruction(&Instruction::LocalGet(0));
        ctor_arg0(&mut f, 2);
        f.instruction(&Instruction::Call(self.json_run_idx));
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::LocalGet(2)); // propagate Err
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f
    }

    // ---- Html static render: vdom is a T_CTOR (VTEXT tag0 [str]; VNODE tag1
    //      [tagName, attrs, kids]); attrs are AATTR tag0 [k,v] / ASTYLE tag1
    //      [k,v]. render_html serializes to the exact HTML the DOM stub emits.

    /// html_escape(s) : escape text (`&<>`) or an attribute value (`&"<`).
    fn emit_html_escape(&self, attr: bool) -> Function {
        // param s(0). locals: sstr(1):str, len(2),i(3),c(4):i32, out(5):eqref
        let mut f = Function::new([(1, ref_to(T_STR)), (3, ValType::I32), (1, eqref())]);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_STR));
        f.instruction(&Instruction::LocalSet(1));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::ArrayLen);
        f.instruction(&Instruction::LocalSet(2));
        push_str_const(&mut f, "");
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::ArrayGetU(T_STR));
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::LocalGet(5));
        // escapes: '&' always; text also '<'/'>'; attr also '"'/'<'
        let escs: &[(i32, &str)] = if attr {
            &[(38, "&amp;"), (34, "&quot;"), (60, "&lt;")]
        } else {
            &[(38, "&amp;"), (60, "&lt;"), (62, "&gt;")]
        };
        for (code, rep) in escs {
            f.instruction(&Instruction::LocalGet(4));
            f.instruction(&Instruction::I32Const(*code));
            f.instruction(&Instruction::I32Eq);
            f.instruction(&Instruction::If(BlockType::Result(eqref())));
            push_str_const(&mut f, rep);
            f.instruction(&Instruction::Else);
        }
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_STR, array_size: 1 });
        for _ in escs {
            f.instruction(&Instruction::End);
        }
        f.instruction(&Instruction::Call(self.str_append_idx));
        f.instruction(&Instruction::LocalSet(5));
        bump(&mut f, 3, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::End);
        f
    }

    /// serialize_html(node) : the HTML string for a vdom node.
    fn emit_serialize_html(&self) -> Function {
        // param node(0). locals: tag(1),len(2),i(3):i32,
        //   out(4),styleAcc(5),attrsAcc(6),attr(7),sub(8),tagName(9):eqref
        let mut f = Function::new([(3, ValType::I32), (6, eqref())]);
        ctor_tag(&mut f, 0);
        f.instruction(&Instruction::LocalSet(1));
        // VTEXT (tag 0)
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        ctor_arg0(&mut f, 0);
        f.instruction(&Instruction::Call(self.html_esc_text_idx));
        f.instruction(&Instruction::Else);
        // VNODE (tag 1): tagName=arg0, attrs=arg1, kids=arg2
        ctor_arg0(&mut f, 0);
        f.instruction(&Instruction::LocalSet(9)); // tagName
        push_str_const(&mut f, "");
        f.instruction(&Instruction::LocalSet(6)); // attrsAcc
        push_str_const(&mut f, "");
        f.instruction(&Instruction::LocalSet(5)); // styleAcc
        // iterate attrs
        ctor_argn(&mut f, 0, 1);
        f.instruction(&Instruction::LocalSet(8)); // sub = attrs
        list_len(&mut f, 8);
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        list_elem(&mut f, 8, 3);
        f.instruction(&Instruction::LocalSet(7)); // attr
        ctor_tag(&mut f, 7);
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Empty));
        // AATTR: attrsAcc ++= " " ++ key ++ "=\"" ++ escAttr(val) ++ "\""
        f.instruction(&Instruction::LocalGet(6));
        push_str_const(&mut f, " ");
        f.instruction(&Instruction::Call(self.str_append_idx));
        ctor_arg0(&mut f, 7);
        f.instruction(&Instruction::Call(self.str_append_idx));
        push_str_const(&mut f, "=\"");
        f.instruction(&Instruction::Call(self.str_append_idx));
        ctor_argn(&mut f, 7, 1);
        f.instruction(&Instruction::Call(self.html_esc_attr_idx));
        f.instruction(&Instruction::Call(self.str_append_idx));
        push_str_const(&mut f, "\"");
        f.instruction(&Instruction::Call(self.str_append_idx));
        f.instruction(&Instruction::LocalSet(6));
        f.instruction(&Instruction::Else);
        // ASTYLE (tag 1): styleAcc ++= key ++ ":" ++ val ++ ";" (AEVENT skipped)
        ctor_tag(&mut f, 7);
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(5));
        ctor_arg0(&mut f, 7);
        f.instruction(&Instruction::Call(self.str_append_idx));
        push_str_const(&mut f, ":");
        f.instruction(&Instruction::Call(self.str_append_idx));
        ctor_argn(&mut f, 7, 1);
        f.instruction(&Instruction::Call(self.str_append_idx));
        push_str_const(&mut f, ";");
        f.instruction(&Instruction::Call(self.str_append_idx));
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        bump(&mut f, 3, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        // if styleAcc non-empty: attrsAcc ++= " style=\"" ++ escAttr(styleAcc) ++ "\""
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&cast_to(T_STR));
        f.instruction(&Instruction::ArrayLen);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(6));
        push_str_const(&mut f, " style=\"");
        f.instruction(&Instruction::Call(self.str_append_idx));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::Call(self.html_esc_attr_idx));
        f.instruction(&Instruction::Call(self.str_append_idx));
        push_str_const(&mut f, "\"");
        f.instruction(&Instruction::Call(self.str_append_idx));
        f.instruction(&Instruction::LocalSet(6));
        f.instruction(&Instruction::End);
        // out = "<" ++ tagName ++ attrsAcc ++ ">"
        push_str_const(&mut f, "<");
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::Call(self.str_append_idx));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::Call(self.str_append_idx));
        push_str_const(&mut f, ">");
        f.instruction(&Instruction::Call(self.str_append_idx));
        f.instruction(&Instruction::LocalSet(4)); // out
        // kids
        ctor_argn(&mut f, 0, 2);
        f.instruction(&Instruction::LocalSet(8)); // sub = kids
        list_len(&mut f, 8);
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(4));
        // node = (VKEYED) ? kid[1] : kid   (tag is in local 1)
        list_elem(&mut f, 8, 3);
        f.instruction(&Instruction::LocalSet(7));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(2));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&cast_to(T_ARR));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::Call(self.serialize_html_idx));
        f.instruction(&Instruction::Call(self.str_append_idx));
        f.instruction(&Instruction::LocalSet(4));
        bump(&mut f, 3, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        // out ++= "</" ++ tagName ++ ">"
        f.instruction(&Instruction::LocalGet(4));
        push_str_const(&mut f, "</");
        f.instruction(&Instruction::Call(self.str_append_idx));
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::Call(self.str_append_idx));
        push_str_const(&mut f, ">");
        f.instruction(&Instruction::Call(self.str_append_idx));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f
    }

    /// view_html() : run the Browser.sandbox program's `view` on its initial
    /// model and serialize the resulting Html. Program = T_CTOR tag0 [record];
    /// record fields (sorted) are init(0), update(1), view(2).
    fn emit_view_html(&self, main_idx: u32) -> Function {
        let mut f = Function::new([(1, eqref())]); // rec(0)
        // rec = main().args[0]  (program = T_CTOR tag0 [record])
        f.instruction(&Instruction::Call(main_idx));
        f.instruction(&cast_to(T_CTOR));
        f.instruction(&Instruction::StructGet { struct_type_index: T_CTOR, field_index: 1 });
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::LocalSet(0));
        // view (rec[2]) applied to model (rec[0]) → Html → serialize
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_ARR));
        f.instruction(&Instruction::I32Const(2));
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_ARR));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&Instruction::Call(self.serialize_html_idx));
        f.instruction(&Instruction::End);
        f
    }

    // ---- Browser runtime (real DOM via host imports) ----

    /// marshal(s) : copy a T_STR into linear memory at the bump pointer and
    /// return its offset (length is `s.len`, read separately by the caller).
    fn emit_marshal(&self) -> Function {
        // param s(0). locals: sstr(1):str, len(2),i(3),ptr(4):i32
        let mut f = Function::new([(1, ref_to(T_STR)), (3, ValType::I32)]);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_STR));
        f.instruction(&Instruction::LocalSet(1));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::ArrayLen);
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::GlobalGet(G_BUMP));
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::ArrayGetU(T_STR));
        f.instruction(&Instruction::I32Store8(mem0()));
        bump(&mut f, 3, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        // bump += len
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::GlobalSet(G_BUMP));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::End);
        f
    }

    /// Push `[ptr, len]` for the string in local `s_local` (marshal + arraylen).
    fn dom_str(&self, f: &mut Function, s_local: u32) {
        f.instruction(&Instruction::LocalGet(s_local));
        f.instruction(&Instruction::Call(self.marshal_idx));
        f.instruction(&Instruction::LocalGet(s_local));
        f.instruction(&cast_to(T_STR));
        f.instruction(&Instruction::ArrayLen);
    }

    /// render_dom(node) : build a real DOM subtree via host imports, returning
    /// the node handle. Attrs → set_attribute/set_style; events register a
    /// handler id; children recurse.
    fn emit_render_dom(&self) -> Function {
        // param node(0). locals: tag(1),i(2),h(3),hid(4):i32,
        //   attr(5),sub(6),key(7),val(8):eqref
        let mut f = Function::new([(4, ValType::I32), (4, eqref())]);
        ctor_tag(&mut f, 0);
        f.instruction(&Instruction::LocalSet(1));
        // VTEXT
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Result(ValType::I32)));
        ctor_arg0(&mut f, 0);
        f.instruction(&Instruction::LocalSet(7));
        self.dom_str(&mut f, 7);
        f.instruction(&Instruction::Call(DOM_CREATE_TEXT));
        f.instruction(&Instruction::Else);
        // VNODE: create element
        ctor_arg0(&mut f, 0);
        f.instruction(&Instruction::LocalSet(7)); // tagName
        self.dom_str(&mut f, 7);
        f.instruction(&Instruction::Call(DOM_CREATE_ELEMENT));
        f.instruction(&Instruction::LocalSet(3)); // h
        // attrs
        ctor_argn(&mut f, 0, 1);
        f.instruction(&Instruction::LocalSet(6));
        list_len(&mut f, 6);
        f.instruction(&Instruction::LocalSet(2)); // len
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(4)); // i (hid slot reused as attr counter)
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        list_elem(&mut f, 6, 4);
        f.instruction(&Instruction::LocalSet(5)); // attr
        ctor_arg0(&mut f, 5);
        f.instruction(&Instruction::LocalSet(7)); // attr key/name (arg0)
        ctor_argn(&mut f, 5, 1);
        f.instruction(&Instruction::LocalSet(8)); // attr val/decoder (arg1)
        // AATTR (0)
        ctor_tag(&mut f, 5);
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(3));
        self.dom_str(&mut f, 7);
        self.dom_str(&mut f, 8);
        f.instruction(&Instruction::Call(DOM_SET_ATTRIBUTE));
        f.instruction(&Instruction::Else);
        // ASTYLE (1)
        ctor_tag(&mut f, 5);
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(3));
        self.dom_str(&mut f, 7);
        self.dom_str(&mut f, 8);
        f.instruction(&Instruction::Call(DOM_SET_STYLE));
        f.instruction(&Instruction::Else);
        // AEVENT (2): register decoder at a fresh hid, add listener
        f.instruction(&Instruction::GlobalGet(G_HANDLERS));
        f.instruction(&cast_to(T_ARR));
        f.instruction(&Instruction::GlobalGet(G_NEXT_HID));
        f.instruction(&Instruction::LocalGet(8)); // decoder
        f.instruction(&Instruction::ArraySet(T_ARR));
        f.instruction(&Instruction::LocalGet(3)); // node
        self.dom_str(&mut f, 7); // event name
        f.instruction(&Instruction::GlobalGet(G_NEXT_HID)); // hid
        f.instruction(&Instruction::Call(DOM_ADD_EVENT_LISTENER));
        f.instruction(&Instruction::GlobalGet(G_NEXT_HID));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::GlobalSet(G_NEXT_HID));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        bump(&mut f, 4, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        // kids
        ctor_argn(&mut f, 0, 2);
        f.instruction(&Instruction::LocalSet(6));
        list_len(&mut f, 6);
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(3));
        // node = (VKEYED) ? kid[1] : kid
        list_elem(&mut f, 6, 4);
        f.instruction(&Instruction::LocalSet(7));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(2));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&cast_to(T_ARR));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::Call(self.render_dom_idx));
        f.instruction(&Instruction::Call(DOM_APPEND_CHILD));
        bump(&mut f, 4, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::End); // top-level if
        f.instruction(&Instruction::End); // function
        f
    }

    /// Reset the per-render scratch (bump + handler ids) and set the model /
    /// handler-table globals; shared by browser_start and rerender.
    fn emit_reset_render(f: &mut Function) {
        f.instruction(&Instruction::I32Const(BUMP_BASE));
        f.instruction(&Instruction::GlobalSet(G_BUMP));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::GlobalSet(G_NEXT_HID));
    }

    /// rerender() : re-render the current model and replace the mounted root.
    fn emit_rerender(&self) -> Function {
        let mut f = Function::new([]);
        Self::emit_reset_render(&mut f);
        f.instruction(&Instruction::GlobalGet(G_VIEW));
        f.instruction(&Instruction::GlobalGet(G_MODEL));
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&Instruction::Call(self.render_dom_idx));
        f.instruction(&Instruction::Call(DOM_REPLACE_ROOT));
        f.instruction(&Instruction::End);
        f
    }

    /// patch(dom, old, new) : diff `old`→`new` and mutate the DOM in place,
    /// returning the (possibly replaced) handle. `val_eq`-equal subtrees are
    /// skipped, preserving DOM node identity. (First draft: attrs are re-applied
    /// rather than diffed, and event listeners persist across a patch — fine for
    /// nullary-message handlers.)
    fn emit_patch(&self) -> Function {
        // params dom(0):i32, old(1),new(2):eqref. locals: t(3),olen(4),nlen(5),
        //   i(6),common(7),cdom(8):i32, attr(9),osub(10),nsub(11),key(12),val(13):eqref
        let mut f = Function::new([(6, ValType::I32), (5, eqref())]);
        // identical subtree → nothing to do
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::Call(self.val_eq_idx));
        f.instruction(&Instruction::RefCastNonNull(HeapType::Abstract { shared: false, ty: AbstractHeapType::I31 }));
        f.instruction(&Instruction::I31GetS);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // replace = tags differ, or same VNODE/VKEYED with different tagName
        ctor_tag(&mut f, 1);
        ctor_tag(&mut f, 2);
        f.instruction(&Instruction::I32Ne);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Empty));
        ctor_tag(&mut f, 1);
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32GeS); // VNODE (1) or VKEYED (2) carry a tagName
        f.instruction(&Instruction::If(BlockType::Empty));
        ctor_arg0(&mut f, 1);
        ctor_arg0(&mut f, 2);
        f.instruction(&Instruction::Call(self.val_eq_idx));
        f.instruction(&Instruction::RefCastNonNull(HeapType::Abstract { shared: false, ty: AbstractHeapType::I31 }));
        f.instruction(&Instruction::I31GetS);
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::Call(self.render_dom_idx));
        f.instruction(&Instruction::LocalSet(8));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::Call(DOM_REPLACE));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // VTEXT (both, text changed): set text
        ctor_tag(&mut f, 1);
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Empty));
        ctor_arg0(&mut f, 2);
        f.instruction(&Instruction::LocalSet(12));
        f.instruction(&Instruction::LocalGet(0));
        self.dom_str(&mut f, 12);
        f.instruction(&Instruction::Call(DOM_SET_TEXT));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // VNODE same tag: reapply new attrs (AATTR/ASTYLE; events persist)
        ctor_argn(&mut f, 2, 1);
        f.instruction(&Instruction::LocalSet(11));
        list_len(&mut f, 11);
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(6));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        list_elem(&mut f, 11, 6);
        f.instruction(&Instruction::LocalSet(9));
        ctor_arg0(&mut f, 9);
        f.instruction(&Instruction::LocalSet(12));
        ctor_argn(&mut f, 9, 1);
        f.instruction(&Instruction::LocalSet(13));
        ctor_tag(&mut f, 9);
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(0));
        self.dom_str(&mut f, 12);
        self.dom_str(&mut f, 13);
        f.instruction(&Instruction::Call(DOM_SET_ATTRIBUTE));
        f.instruction(&Instruction::Else);
        ctor_tag(&mut f, 9);
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(0));
        self.dom_str(&mut f, 12);
        self.dom_str(&mut f, 13);
        f.instruction(&Instruction::Call(DOM_SET_STYLE));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        bump(&mut f, 6, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        // VKEYED (tag 2): reconcile children by key, preserving DOM identity.
        ctor_tag(&mut f, 2);
        f.instruction(&Instruction::I32Const(2));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(0)); // dom
        ctor_argn(&mut f, 1, 2); // old keyed kids
        ctor_argn(&mut f, 2, 2); // new keyed kids
        f.instruction(&Instruction::Call(self.keyed_reconcile_idx));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // patch kids by position (non-keyed VNODE)
        ctor_argn(&mut f, 1, 2);
        f.instruction(&Instruction::LocalSet(10)); // old kids
        ctor_argn(&mut f, 2, 2);
        f.instruction(&Instruction::LocalSet(11)); // new kids
        list_len(&mut f, 10);
        f.instruction(&Instruction::LocalSet(4)); // olen
        list_len(&mut f, 11);
        f.instruction(&Instruction::LocalSet(5)); // nlen
        // common = min(olen, nlen)
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalSet(7));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalSet(7));
        f.instruction(&Instruction::End);
        // patch [0, common)
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(6));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::Call(DOM_CHILD));
        list_elem(&mut f, 10, 6);
        list_elem(&mut f, 11, 6);
        f.instruction(&Instruction::Call(self.patch_idx));
        f.instruction(&Instruction::Drop);
        bump(&mut f, 6, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        // append extra new kids [common, nlen)
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(0));
        list_elem(&mut f, 11, 6);
        f.instruction(&Instruction::Call(self.render_dom_idx));
        f.instruction(&Instruction::Call(DOM_APPEND_CHILD));
        bump(&mut f, 6, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        // remove extra old kids: (olen-nlen) removals at index nlen
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32LeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::Call(DOM_CHILD));
        f.instruction(&Instruction::Call(DOM_REMOVE_CHILD));
        dec(&mut f, 4);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::End);
        f
    }

    /// run_cmd(cmd) : execute a Cmd. CMD_NONE=tag0 (nothing), CMD_BATCH=tag1
    /// [List Cmd], CMD_PORT=tag2 [name, Json.Value] → host_port_out.
    fn emit_run_cmd(&self) -> Function {
        // param cmd(0). locals: tag(1),len(2),i(3):i32, list(4),s(5):eqref
        let mut f = Function::new([(3, ValType::I32), (2, eqref())]);
        ctor_tag(&mut f, 0);
        f.instruction(&Instruction::LocalSet(1));
        // CMD_PORT (2)
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(2));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Empty));
        ctor_arg0(&mut f, 0); // name
        f.instruction(&Instruction::LocalSet(5));
        self.dom_str(&mut f, 5); // [nameptr, namelen]
        // json = json_enc(value, "", "")
        ctor_argn(&mut f, 0, 1);
        push_str_const(&mut f, "");
        push_str_const(&mut f, "");
        f.instruction(&Instruction::Call(self.json_enc_idx));
        f.instruction(&Instruction::LocalSet(5));
        self.dom_str(&mut f, 5); // [jsonptr, jsonlen]
        f.instruction(&Instruction::Call(HOST_PORT_OUT));
        f.instruction(&Instruction::End);
        // CMD_BATCH (1)
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Empty));
        ctor_arg0(&mut f, 0);
        f.instruction(&Instruction::LocalSet(4));
        list_len(&mut f, 4);
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        list_elem(&mut f, 4, 3);
        f.instruction(&Instruction::Call(self.run_cmd_idx));
        bump(&mut f, 3, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        // CMD_HTTP (3): [url, expect]. Register expect by request id, start GET.
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(3));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::GlobalGet(G_NEXT_REQ));
        f.instruction(&Instruction::LocalSet(2)); // reqId
        f.instruction(&Instruction::GlobalGet(G_NEXT_REQ));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::GlobalSet(G_NEXT_REQ));
        // G_HTTP[reqId] = expect (cmd.arg1)
        f.instruction(&Instruction::GlobalGet(G_HTTP));
        f.instruction(&cast_to(T_ARR));
        f.instruction(&Instruction::LocalGet(2));
        ctor_argn(&mut f, 0, 1);
        f.instruction(&Instruction::ArraySet(T_ARR));
        // host_http(marshal(url=cmd.arg0), len, reqId)
        ctor_arg0(&mut f, 0);
        f.instruction(&Instruction::LocalSet(5));
        self.dom_str(&mut f, 5);
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::Call(HOST_HTTP));
        f.instruction(&Instruction::End);
        // CMD_NAV push (4) / replace (5): change history, then fire onUrlChange
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(4));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(5));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::I32Or);
        f.instruction(&Instruction::If(BlockType::Empty));
        ctor_arg0(&mut f, 0);
        f.instruction(&Instruction::LocalSet(5));
        self.dom_str(&mut f, 5); // [ptr, len]
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(5));
        f.instruction(&Instruction::I32Eq); // replace flag
        f.instruction(&Instruction::Call(HOST_PUSH_URL));
        // onUrlChange(parse(current href)) if a handler is registered
        f.instruction(&Instruction::GlobalGet(G_URLCHG));
        f.instruction(&Instruction::RefIsNull);
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::Call(HOST_GET_URL));
        f.instruction(&Instruction::LocalSet(2)); // len
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::Call(self.str_from_mem_idx));
        f.instruction(&Instruction::Call(self.url_from_string_idx));
        f.instruction(&Instruction::LocalSet(4)); // Maybe Url
        f.instruction(&Instruction::GlobalGet(G_URLCHG));
        ctor_arg0(&mut f, 4); // Just → Url
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&Instruction::Call(self.dispatch_msg_idx));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        // CMD_NAV load (6): full navigation
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(6));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Empty));
        ctor_arg0(&mut f, 0);
        f.instruction(&Instruction::LocalSet(5));
        self.dom_str(&mut f, 5);
        f.instruction(&Instruction::Call(HOST_LOAD));
        f.instruction(&Instruction::End);
        // CMD_TASK_PERFORM (7): [toMsg, task]. Run the (synchronous) task; it
        // cannot fail (Task Never a), so dispatch toMsg applied to the Ok value.
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(7));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Empty));
        ctor_argn(&mut f, 0, 1);
        f.instruction(&Instruction::Call(self.task_run_idx));
        f.instruction(&Instruction::LocalSet(5)); // r = Ok value
        ctor_arg0(&mut f, 0); // toMsg
        ctor_arg0(&mut f, 5); // the Ok value
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&Instruction::Call(self.dispatch_msg_idx));
        f.instruction(&Instruction::End);
        // CMD_TASK_ATTEMPT (8): [toMsg, task]. dispatch toMsg applied to the
        // whole Result.
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Empty));
        ctor_arg0(&mut f, 0); // toMsg
        ctor_argn(&mut f, 0, 1);
        f.instruction(&Instruction::Call(self.task_run_idx));
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&Instruction::Call(self.dispatch_msg_idx));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f
    }

    /// str_from_mem(ptr, len) : copy `len` bytes of linear memory into a fresh
    /// T_STR GC array (the inverse of `marshal`). Used to lift a host-written
    /// event-JSON payload into a String the JSON parser can consume.
    fn emit_str_from_mem(&self) -> Function {
        // params ptr(0), len(1). locals: i(2):i32, arr(3):T_STR
        let mut f = Function::new([(1, ValType::I32), (1, ref_to(T_STR))]);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::ArrayNewDefault(T_STR));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I32Load8U(mem0()));
        f.instruction(&Instruction::ArraySet(T_STR));
        bump(&mut f, 2, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::End);
        f
    }

    /// doc_vnode() : evaluate a `Browser.document` view into a vdom node. The
    /// view yields `{ body : List (Html msg), title : String }` (fields sorted:
    /// body=0, title=1). We set the page title via the host and wrap the body
    /// list in a `<div>` VNODE — matching the JS runtime — so the ordinary
    /// single-root render/diff path handles it unchanged.
    fn emit_render_document(&self) -> Function {
        // locals: doc(0), title(1):eqref
        let mut f = Function::new([(2, eqref())]);
        // doc = view model
        f.instruction(&Instruction::GlobalGet(G_VIEW));
        f.instruction(&Instruction::GlobalGet(G_MODEL));
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&cast_to(T_ARR));
        f.instruction(&Instruction::LocalSet(0));
        // host_set_title(marshal(doc.title), len)  (title = doc[1])
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_ARR));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::LocalSet(1));
        self.dom_str(&mut f, 1); // marshals the title string
        f.instruction(&Instruction::Call(HOST_SET_TITLE));
        // VNODE("div", [], doc.body)  (body = doc[0])
        f.instruction(&Instruction::I32Const(1)); // VNODE
        push_str_const(&mut f, "div");
        push_empty_list(&mut f);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_ARR));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 3 });
        f.instruction(&Instruction::StructNew(T_CTOR));
        f.instruction(&Instruction::End);
        f
    }

    /// alm_event(hid, ptr, len) : run the handler decoder against the event
    /// payload (JSON text at [ptr, len) in linear memory, or `null` when
    /// len==0), update the model, and diff/patch the DOM.
    fn emit_alm_event(&self) -> Function {
        // params hid(0),ptr(1),len(2). locals: dec(3),r(4),new(5),upd(6),val(7):eqref
        let mut f = Function::new([(5, eqref())]);
        f.instruction(&Instruction::GlobalGet(G_HANDLERS));
        f.instruction(&cast_to(T_ARR));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::LocalSet(3));
        // val = len>0 ? Ok-value(json_parse(str_from_mem(ptr,len))) : JSON null
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::Call(self.str_from_mem_idx));
        f.instruction(&Instruction::Call(self.json_parse_idx));
        f.instruction(&Instruction::LocalSet(7));
        ctor_arg0(&mut f, 7); // Result Ok [value] → value
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::RefNull(HeapType::Concrete(T_ARR)));
        f.instruction(&Instruction::StructNew(T_CTOR));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalSet(7));
        f.instruction(&Instruction::LocalGet(3)); // dec
        f.instruction(&Instruction::LocalGet(7)); // val
        f.instruction(&Instruction::Call(self.json_run_idx));
        f.instruction(&Instruction::LocalSet(4));
        ctor_tag(&mut f, 4);
        f.instruction(&Instruction::I32Eqz); // Ok
        f.instruction(&Instruction::If(BlockType::Empty));
        // dispatch the decoded msg (Result Ok [msg] → msg)
        ctor_arg0(&mut f, 4);
        f.instruction(&Instruction::Call(self.dispatch_msg_idx));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::End);
        f
    }

    /// dispatch_msg(msg) : run `update msg model`, apply any resulting Cmd, then
    /// re-render (diff/patch). Shared by DOM events and incoming ports.
    fn emit_dispatch_msg(&self) -> Function {
        // param msg(0). locals: upd(1), new(2):eqref
        let mut f = Function::new([(2, eqref())]);
        // upd = update msg model
        f.instruction(&Instruction::GlobalGet(G_UPDATE));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&Instruction::GlobalGet(G_MODEL));
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&Instruction::LocalSet(1));
        // element/document (G_KIND != 0): upd is a (model, cmd) tuple
        f.instruction(&Instruction::GlobalGet(G_KIND));
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_ARR));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::GlobalSet(G_MODEL));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_ARR));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::Call(self.run_cmd_idx));
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::GlobalSet(G_MODEL));
        f.instruction(&Instruction::End);
        // new = (document/application ? doc_vnode() : view model) ; patch
        f.instruction(&Instruction::I32Const(BUMP_BASE));
        f.instruction(&Instruction::GlobalSet(G_BUMP));
        f.instruction(&Instruction::GlobalGet(G_KIND));
        f.instruction(&Instruction::I32Const(2));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        f.instruction(&Instruction::Call(self.render_document_idx));
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::GlobalGet(G_VIEW));
        f.instruction(&Instruction::GlobalGet(G_MODEL));
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::GlobalGet(G_ROOT));
        f.instruction(&Instruction::GlobalGet(G_PREV));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::Call(self.patch_idx));
        f.instruction(&Instruction::GlobalSet(G_ROOT));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::GlobalSet(G_PREV));
        // subscriptions may have changed → re-register timers
        f.instruction(&Instruction::Call(self.reconcile_subs_idx));
        f.instruction(&Instruction::End);
        f
    }

    /// html_map(f, node) : `Html.map f node` — rebuild the vdom so every event
    /// handler's decoded message is mapped through `f`. VTEXT is unchanged;
    /// VNODE rewrites each AEVENT attr's decoder to `Json.Decode.map f decoder`
    /// (DMap tag15) and recurses into children. Attrs/kids lists are rebuilt
    /// head-first (T_BACK head=0, elements at data[0..len)).
    fn emit_html_map(&self) -> Function {
        // params f(0), node(1). locals: tag(2),len(3),i(4):i32,
        //   arr(5):ref T_ARR, elem(6),kid(7):eqref
        let mut f = Function::new([(3, ValType::I32), (1, ref_to(T_ARR)), (2, eqref())]);
        ctor_tag(&mut f, 1);
        f.instruction(&Instruction::LocalSet(2));
        // VTEXT (tag 0): unchanged
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // VNODE (1) / VKEYED (2): build <tag>[ node.arg0, mappedAttrs, mappedKids ]
        f.instruction(&Instruction::LocalGet(2)); // preserve VNODE vs VKEYED
        ctor_arg0(&mut f, 1); // tagName
        // --- mapped attrs (node.arg1) ---
        ctor_argn(&mut f, 1, 1);
        f.instruction(&Instruction::LocalSet(6)); // reuse elem slot to hold attrs list
        list_len(&mut f, 6);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::ArrayNewDefault(T_ARR));
        f.instruction(&Instruction::LocalSet(5)); // arr
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(5)); // arr (for ArraySet)
        f.instruction(&Instruction::LocalGet(4)); // i
        // attr = attrs[i]; but attrs list is in local 6 — re-fetch each iter
        ctor_argn(&mut f, 1, 1);
        f.instruction(&Instruction::LocalSet(6));
        list_elem(&mut f, 6, 4);
        f.instruction(&Instruction::LocalSet(6)); // elem = attr
        // if AEVENT (tag 2): AEVENT[name, DMap[f, decoder]] else elem
        ctor_tag(&mut f, 6);
        f.instruction(&Instruction::I32Const(2));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        f.instruction(&Instruction::I32Const(2)); // AEVENT
        ctor_arg0(&mut f, 6); // event name
        f.instruction(&Instruction::I32Const(15)); // DMap
        f.instruction(&Instruction::LocalGet(0)); // f
        ctor_argn(&mut f, 6, 1); // decoder
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 2 });
        f.instruction(&Instruction::StructNew(T_CTOR)); // DMap
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 2 });
        f.instruction(&Instruction::StructNew(T_CTOR)); // AEVENT
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::ArraySet(T_ARR));
        bump(&mut f, 4, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        // wrap arr as T_LIST{len, T_BACK{0, arr}}
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::StructNew(T_BACK));
        f.instruction(&Instruction::StructNew(T_LIST));
        // --- mapped kids (node.arg2), recursively ---
        ctor_argn(&mut f, 1, 2);
        f.instruction(&Instruction::LocalSet(6));
        list_len(&mut f, 6);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::ArrayNewDefault(T_ARR));
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(4));
        ctor_argn(&mut f, 1, 2);
        f.instruction(&Instruction::LocalSet(6));
        list_elem(&mut f, 6, 4);
        f.instruction(&Instruction::LocalSet(7)); // kid (node, or (key,node) pair)
        // VKEYED: arr[i] = [kid[0], html_map(f, kid[1])] ; else html_map(f, kid)
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(2));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&cast_to(T_ARR));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::ArrayGet(T_ARR)); // key
        f.instruction(&Instruction::LocalGet(0)); // f
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&cast_to(T_ARR));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::ArrayGet(T_ARR)); // node
        f.instruction(&Instruction::Call(self.html_map_idx));
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 2 });
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::LocalGet(0)); // f
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::Call(self.html_map_idx));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::ArraySet(T_ARR));
        bump(&mut f, 4, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::StructNew(T_BACK));
        f.instruction(&Instruction::StructNew(T_LIST));
        // <tag>[tagName, attrs, kids]
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 3 });
        f.instruction(&Instruction::StructNew(T_CTOR));
        f.instruction(&Instruction::End);
        f
    }

    /// index_byte(str, ch) : first index of byte `ch` in the T_STR, or -1.
    /// (Byte-indexed — matches the backend's byte-based String slicing, which is
    /// exact for the ASCII delimiters URLs are split on.)
    fn emit_index_byte(&self) -> Function {
        // params s(0):eqref, ch(1):i32. locals: i(2),n(3):i32, str(4):ref T_STR
        let mut f = Function::new([(2, ValType::I32), (1, ref_to(T_STR))]);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_STR));
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::ArrayLen);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::ArrayGetU(T_STR));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        bump(&mut f, 2, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::I32Const(-1));
        f.instruction(&Instruction::End);
        f
    }

    /// url_from_string(s) : `Url.fromString` — parse `http(s)://host[:port]/path
    /// [?query][#fragment]` into `Maybe Url`. Follows elm/url's algorithm
    /// (chompAfterProtocol → fragment → query → path → host/port). Url is a
    /// record {fragment,host,path,port_,protocol,query} (sorted); Protocol
    /// Http=tag0/Https=tag1; Maybe Just=tag0[x]/Nothing=tag1. Byte-indexed
    /// (ASCII-correct, which covers the delimiter set).
    fn emit_url_from_string(&self) -> Function {
        // param s(0). locals: proto(1),idx(2):i32; rest(3),frag(4),beforeFrag(5),
        //   query(6),beforeQuery(7),path(8),beforePath(9),host(10),portM(11),tmp(12):eqref;
        //   idx1(13):i32 (idx+1 scratch, appended so eqref indices stay put)
        let mut f = Function::new([(2, ValType::I32), (10, eqref()), (1, ValType::I32)]);
        // Maybe: Just = tag 0 [x], Nothing = tag 1 (Elm declares Just first).
        let nothing = |f: &mut Function| {
            f.instruction(&Instruction::I32Const(1));
            f.instruction(&Instruction::RefNull(HeapType::Concrete(T_ARR)));
            f.instruction(&Instruction::StructNew(T_CTOR));
        };
        // isEmpty(local) → push i32 (1 if empty)
        let is_empty = |f: &mut Function, l: u32| {
            f.instruction(&Instruction::LocalGet(l));
            f.instruction(&cast_to(T_STR));
            f.instruction(&Instruction::ArrayLen);
            f.instruction(&Instruction::I32Eqz);
        };
        // str_left(n_i32_local, src_local) → pushes String
        let str_left_idx = self.str_left_idx;
        let str_dropleft_idx = self.str_dropleft_idx;
        let left = move |f: &mut Function, n: u32, src: u32| {
            f.instruction(&Instruction::LocalGet(n));
            f.instruction(&Instruction::I64ExtendI32S);
            f.instruction(&Instruction::Call(self.box_int_idx));
            f.instruction(&Instruction::LocalGet(src));
            f.instruction(&Instruction::Call(str_left_idx));
        };
        // str_dropleft(n_i32_local, src_local) → pushes String
        let dropleft = move |f: &mut Function, n: u32, src: u32| {
            f.instruction(&Instruction::LocalGet(n));
            f.instruction(&Instruction::I64ExtendI32S);
            f.instruction(&Instruction::Call(self.box_int_idx));
            f.instruction(&Instruction::LocalGet(src));
            f.instruction(&Instruction::Call(str_dropleft_idx));
        };
        // --- protocol ---
        push_str_const(&mut f, "http://");
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::Call(self.str_starts_with_idx));
        f.instruction(&Instruction::RefCastNonNull(HeapType::Abstract { shared: false, ty: AbstractHeapType::I31 }));
        f.instruction(&Instruction::I31GetS);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(0)); // Http
        f.instruction(&Instruction::LocalSet(1));
        f.instruction(&Instruction::I32Const(7));
        f.instruction(&Instruction::LocalSet(2));
        dropleft(&mut f, 2, 0);
        f.instruction(&Instruction::LocalSet(3)); // rest
        f.instruction(&Instruction::Else);
        push_str_const(&mut f, "https://");
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::Call(self.str_starts_with_idx));
        f.instruction(&Instruction::RefCastNonNull(HeapType::Abstract { shared: false, ty: AbstractHeapType::I31 }));
        f.instruction(&Instruction::I31GetS);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(1)); // Https
        f.instruction(&Instruction::LocalSet(1));
        f.instruction(&Instruction::I32Const(8));
        f.instruction(&Instruction::LocalSet(2));
        dropleft(&mut f, 2, 0);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Else);
        nothing(&mut f);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        // if isEmpty(rest) → Nothing
        is_empty(&mut f, 3);
        f.instruction(&Instruction::If(BlockType::Empty));
        nothing(&mut f);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // --- fragment: idx = index_byte(rest, '#') ---
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Const(35)); // '#'
        f.instruction(&Instruction::Call(self.index_byte_idx));
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::If(BlockType::Empty));
        // frag = Just(dropLeft(idx+1, rest)); beforeFrag = left(idx, rest)
        f.instruction(&Instruction::I32Const(0)); // Just tag
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(13));
        dropleft(&mut f, 13, 3);
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 1 });
        f.instruction(&Instruction::StructNew(T_CTOR));
        f.instruction(&Instruction::LocalSet(4)); // frag
        left(&mut f, 2, 3);
        f.instruction(&Instruction::LocalSet(5)); // beforeFrag
        f.instruction(&Instruction::Else);
        nothing(&mut f);
        f.instruction(&Instruction::LocalSet(4)); // frag = Nothing
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalSet(5)); // beforeFrag = rest
        f.instruction(&Instruction::End);
        // if isEmpty(beforeFrag) → Nothing
        is_empty(&mut f, 5);
        f.instruction(&Instruction::If(BlockType::Empty));
        nothing(&mut f);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // --- query: idx = index_byte(beforeFrag, '?') ---
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Const(63)); // '?'
        f.instruction(&Instruction::Call(self.index_byte_idx));
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(0)); // Just
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(13));
        dropleft(&mut f, 13, 5);
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 1 });
        f.instruction(&Instruction::StructNew(T_CTOR));
        f.instruction(&Instruction::LocalSet(6)); // query
        left(&mut f, 2, 5);
        f.instruction(&Instruction::LocalSet(7)); // beforeQuery
        f.instruction(&Instruction::Else);
        nothing(&mut f);
        f.instruction(&Instruction::LocalSet(6));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalSet(7));
        f.instruction(&Instruction::End);
        // if isEmpty(beforeQuery) → Nothing
        is_empty(&mut f, 7);
        f.instruction(&Instruction::If(BlockType::Empty));
        nothing(&mut f);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // --- path: idx = index_byte(beforeQuery, '/') ---
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Const(47)); // '/'
        f.instruction(&Instruction::Call(self.index_byte_idx));
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::If(BlockType::Empty));
        dropleft(&mut f, 2, 7);
        f.instruction(&Instruction::LocalSet(8)); // path
        left(&mut f, 2, 7);
        f.instruction(&Instruction::LocalSet(9)); // beforePath
        f.instruction(&Instruction::Else);
        push_str_const(&mut f, "/");
        f.instruction(&Instruction::LocalSet(8)); // path = "/"
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::LocalSet(9)); // beforePath = beforeQuery
        f.instruction(&Instruction::End);
        // if isEmpty(beforePath) or contains '@' → Nothing
        is_empty(&mut f, 9);
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::I32Const(64)); // '@'
        f.instruction(&Instruction::Call(self.index_byte_idx));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::I32Or);
        f.instruction(&Instruction::If(BlockType::Empty));
        nothing(&mut f);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // --- host/port: idx = index_byte(beforePath, ':') ---
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::I32Const(58)); // ':'
        f.instruction(&Instruction::Call(self.index_byte_idx));
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::If(BlockType::Empty));
        // no colon: host = beforePath, port_ = Nothing
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::LocalSet(10));
        nothing(&mut f);
        f.instruction(&Instruction::LocalSet(11));
        f.instruction(&Instruction::Else);
        // one colon expected: portStr = dropLeft(idx+1, beforePath)
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(13));
        dropleft(&mut f, 13, 9);
        f.instruction(&Instruction::LocalSet(12)); // portStr
        // reject a second colon in portStr
        f.instruction(&Instruction::LocalGet(12));
        f.instruction(&Instruction::I32Const(58));
        f.instruction(&Instruction::Call(self.index_byte_idx));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::If(BlockType::Empty));
        nothing(&mut f);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // portM = str_to_int(portStr) ; if Nothing → Nothing
        f.instruction(&Instruction::LocalGet(12));
        f.instruction(&Instruction::Call(self.str_to_int_idx));
        f.instruction(&Instruction::LocalSet(11)); // portM (Maybe Int)
        ctor_tag(&mut f, 11);
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Eq); // Nothing (tag 1)?
        f.instruction(&Instruction::If(BlockType::Empty));
        nothing(&mut f);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // host = left(idx, beforePath)
        left(&mut f, 2, 9);
        f.instruction(&Instruction::LocalSet(10));
        f.instruction(&Instruction::End);
        // --- build Just (Url record) ---
        // record = [frag, host, path, port_, protocol, query]
        f.instruction(&Instruction::I32Const(0)); // Just tag
        f.instruction(&Instruction::LocalGet(4)); // fragment
        f.instruction(&Instruction::LocalGet(10)); // host
        f.instruction(&Instruction::LocalGet(8)); // path
        f.instruction(&Instruction::LocalGet(11)); // port_
        f.instruction(&Instruction::LocalGet(1)); // protocol (i32 tag)
        f.instruction(&Instruction::RefNull(HeapType::Concrete(T_ARR)));
        f.instruction(&Instruction::StructNew(T_CTOR)); // Protocol T_CTOR{proto, null}
        f.instruction(&Instruction::LocalGet(6)); // query
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 6 });
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 1 });
        f.instruction(&Instruction::StructNew(T_CTOR)); // Just
        f.instruction(&Instruction::End);
        f
    }

    /// strip_keys(list) : `List (key, Html) -> List Html` — drop each keyed
    /// pair's key (tuple index 1 is the node), rebuilt head-first. Output is
    /// correct for position-based diffing; keys are an optimization we skip.
    fn emit_strip_keys(&self) -> Function {
        // param list(0). locals: len(1),i(2):i32, arr(3):ref T_ARR, elem(4):eqref
        let mut f = Function::new([(2, ValType::I32), (1, ref_to(T_ARR)), (1, eqref())]);
        list_len(&mut f, 0);
        f.instruction(&Instruction::LocalSet(1));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::ArrayNewDefault(T_ARR));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(3)); // arr (for ArraySet)
        f.instruction(&Instruction::LocalGet(2)); // i
        list_elem(&mut f, 0, 2); // the (key, html) tuple
        f.instruction(&cast_to(T_ARR));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::ArrayGet(T_ARR)); // tuple[1] = html
        f.instruction(&Instruction::ArraySet(T_ARR));
        bump(&mut f, 2, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        // T_LIST{len, T_BACK{0, arr}}
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::StructNew(T_BACK));
        f.instruction(&Instruction::StructNew(T_LIST));
        f.instruction(&Instruction::End);
        f
    }

    /// keyed_reconcile(dom, oldKids, newKids) : diff two `(key, node)` child
    /// lists by KEY, preserving DOM node identity across reorders. Matched nodes
    /// are patched and moved into new order via appendChild (which relocates an
    /// existing child to the end) — so after appending every target in order the
    /// unmatched old nodes are left at the front and removed. Not LIS-minimal in
    /// move count, but identity-preserving and O(n log n) (a key→index treap).
    fn emit_keyed_reconcile(&self) -> Function {
        // params dom(0):i32, oldk(1), newk(2):eqref. locals:
        //   olen(3),nlen(4),j(5),matched(6),unused(7),handle(8),p(9):i32;
        //   oldHandles(10):ref T_ARR; idxMap(11),pair(12),key(13),found(14):eqref
        let mut f = Function::new([(7, ValType::I32), (1, ref_to(T_ARR)), (4, eqref())]);
        let i31 = || HeapType::Abstract { shared: false, ty: AbstractHeapType::I31 };
        list_len(&mut f, 1);
        f.instruction(&Instruction::LocalSet(3));
        list_len(&mut f, 2);
        f.instruction(&Instruction::LocalSet(4));
        // oldHandles = new T_ARR(olen) ; idxMap = empty treap
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::ArrayNewDefault(T_ARR));
        f.instruction(&Instruction::LocalSet(10));
        f.instruction(&Instruction::RefNull(HeapType::Concrete(T_TNODE)));
        f.instruction(&Instruction::LocalSet(11));
        // for i in 0..olen: capture child handle, index key→i
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        // oldHandles[i] = i31(dom_child(dom, i))
        f.instruction(&Instruction::LocalGet(10));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::Call(DOM_CHILD));
        f.instruction(&Instruction::RefI31);
        f.instruction(&Instruction::ArraySet(T_ARR));
        // idxMap = treap_insert(oldk[i][0], box(i), idxMap)
        list_elem(&mut f, 1, 5);
        f.instruction(&cast_to(T_ARR));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I64ExtendI32S);
        f.instruction(&Instruction::Call(self.box_int_idx));
        f.instruction(&Instruction::LocalGet(11));
        f.instruction(&Instruction::Call(self.treap_insert_idx));
        f.instruction(&Instruction::LocalSet(11));
        bump(&mut f, 5, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        // for j in 0..nlen: reuse (patch) or create, then append in order
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(6)); // matched
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(5)); // j
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        list_elem(&mut f, 2, 5);
        f.instruction(&Instruction::LocalSet(12)); // pair
        f.instruction(&Instruction::LocalGet(12));
        f.instruction(&cast_to(T_ARR));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::LocalSet(13)); // key
        f.instruction(&Instruction::LocalGet(13));
        f.instruction(&Instruction::LocalGet(11));
        f.instruction(&Instruction::Call(self.treap_get_idx));
        f.instruction(&Instruction::LocalSet(14)); // found : Maybe Int
        ctor_tag(&mut f, 14);
        f.instruction(&Instruction::I32Eqz); // Just?
        f.instruction(&Instruction::If(BlockType::Result(ValType::I32)));
        // p = unbox(found.arg0)
        ctor_arg0(&mut f, 14);
        f.instruction(&Instruction::Call(self.unbox_int_idx));
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::LocalSet(9));
        // patch(oldHandles[p], oldk[p][1], pair[1])
        f.instruction(&Instruction::LocalGet(10));
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::RefCastNonNull(i31()));
        f.instruction(&Instruction::I31GetS);
        list_elem(&mut f, 1, 9);
        f.instruction(&cast_to(T_ARR));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::LocalGet(12));
        f.instruction(&cast_to(T_ARR));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::Call(self.patch_idx));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(6));
        f.instruction(&Instruction::Else);
        // render_dom(pair[1])
        f.instruction(&Instruction::LocalGet(12));
        f.instruction(&cast_to(T_ARR));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::Call(self.render_dom_idx));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalSet(8)); // handle
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::Call(DOM_APPEND_CHILD));
        bump(&mut f, 5, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        // remove the (olen - matched) unmatched old nodes, now at the front
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::LocalSet(7));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::Call(DOM_CHILD));
        f.instruction(&Instruction::Call(DOM_REMOVE_CHILD));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::LocalSet(7));
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f
    }

    /// reconcile_subs() : recompute subscriptions and re-register Time.every
    /// timers with the host (clear-all + recreate, matching the JS runtime).
    fn emit_reconcile_subs(&self) -> Function {
        let mut f = Function::new([(1, eqref())]); // sub(0)
        f.instruction(&Instruction::GlobalGet(G_SUBS));
        f.instruction(&Instruction::RefIsNull);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::Call(HOST_CLEAR_TIMERS));
        f.instruction(&Instruction::Call(HOST_CLEAR_DOM));
        f.instruction(&Instruction::Call(HOST_CLEAR_FRAMES));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::GlobalSet(G_NEXT_TICK));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::GlobalSet(G_NEXT_DOM));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::GlobalSet(G_NEXT_FRAME));
        f.instruction(&Instruction::GlobalGet(G_SUBS));
        f.instruction(&Instruction::GlobalGet(G_MODEL));
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&Instruction::Call(self.walk_timers_idx));
        f.instruction(&Instruction::End);
        f
    }

    /// walk_timers(sub) : register each SubTime (tag4 [interval, toMsg]) with the
    /// host, recursing into Sub.batch (tag1). Stores toMsg in G_TICKS by slot.
    fn emit_walk_timers(&self) -> Function {
        // param sub(0). locals: tag(1),len(2),i(3),slot(4):i32, list(5):eqref
        let mut f = Function::new([(4, ValType::I32), (1, eqref())]);
        ctor_tag(&mut f, 0);
        f.instruction(&Instruction::LocalSet(1));
        // SubTime (tag 4)
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(4));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::GlobalGet(G_NEXT_TICK));
        f.instruction(&Instruction::LocalSet(4)); // slot
        f.instruction(&Instruction::GlobalGet(G_NEXT_TICK));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::GlobalSet(G_NEXT_TICK));
        // G_TICKS[slot] = toMsg (sub.arg1)
        f.instruction(&Instruction::GlobalGet(G_TICKS));
        f.instruction(&cast_to(T_ARR));
        f.instruction(&Instruction::LocalGet(4));
        ctor_argn(&mut f, 0, 1);
        f.instruction(&Instruction::ArraySet(T_ARR));
        // host_set_interval(interval(sub.arg0) as f64, slot)
        ctor_arg0(&mut f, 0);
        f.instruction(&cast_to(T_FLOAT));
        f.instruction(&Instruction::StructGet { struct_type_index: T_FLOAT, field_index: 0 });
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::Call(HOST_SET_INTERVAL));
        f.instruction(&Instruction::End);
        // SubDom (tag 5): document-event decoder [name, decoder]
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(5));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::GlobalGet(G_NEXT_DOM));
        f.instruction(&Instruction::LocalSet(4)); // slot
        f.instruction(&Instruction::GlobalGet(G_NEXT_DOM));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::GlobalSet(G_NEXT_DOM));
        // G_DOMSUBS[slot] = decoder (sub.arg1)
        f.instruction(&Instruction::GlobalGet(G_DOMSUBS));
        f.instruction(&cast_to(T_ARR));
        f.instruction(&Instruction::LocalGet(4));
        ctor_argn(&mut f, 0, 1);
        f.instruction(&Instruction::ArraySet(T_ARR));
        // host_add_dom(marshal(name=sub.arg0), len, slot)
        ctor_arg0(&mut f, 0);
        f.instruction(&Instruction::LocalSet(5));
        self.dom_str(&mut f, 5);
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::Call(HOST_ADD_DOM));
        f.instruction(&Instruction::End);
        // SubAnimation (tag 6): [toMsg, deltaFlag] → request an animation frame
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(6));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::GlobalGet(G_NEXT_FRAME));
        f.instruction(&Instruction::LocalSet(4)); // slot
        f.instruction(&Instruction::GlobalGet(G_NEXT_FRAME));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::GlobalSet(G_NEXT_FRAME));
        // G_FRAMES[slot] = sub (keeps toMsg + deltaFlag)
        f.instruction(&Instruction::GlobalGet(G_FRAMES));
        f.instruction(&cast_to(T_ARR));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::ArraySet(T_ARR));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::Call(HOST_REQUEST_FRAME));
        f.instruction(&Instruction::End);
        // Sub.batch (tag 1): recurse
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Empty));
        ctor_arg0(&mut f, 0);
        f.instruction(&Instruction::LocalSet(5));
        list_len(&mut f, 5);
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        list_elem(&mut f, 5, 3);
        f.instruction(&Instruction::Call(self.walk_timers_idx));
        bump(&mut f, 3, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f
    }

    /// alm_tick(slot, millis) : fire a Time.every timer — apply its toMsg to
    /// `millisToPosix millis` (Posix is opaque: T_CTOR tag0 [Int millis]) and
    /// dispatch.
    fn emit_tick(&self) -> Function {
        // params slot(0):i32, millis(1):f64. local toMsg(2):eqref
        let mut f = Function::new([(1, eqref())]);
        f.instruction(&Instruction::GlobalGet(G_TICKS));
        f.instruction(&cast_to(T_ARR));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::LocalSet(2)); // toMsg
        f.instruction(&Instruction::LocalGet(2));
        // posix = T_CTOR tag0 [ Int (trunc millis) ]
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I64TruncF64S);
        f.instruction(&Instruction::Call(self.box_int_idx));
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 1 });
        f.instruction(&Instruction::StructNew(T_CTOR));
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&Instruction::Call(self.dispatch_msg_idx));
        f.instruction(&Instruction::End);
        f
    }

    /// alm_frame(slot, delta, now) : fire an onAnimationFrame(Delta) sub. The
    /// stored SubAnimation is tag6 [toMsg, deltaFlag]; deltaFlag true → apply
    /// toMsg to the frame delta (Float), else to `millisToPosix now`.
    fn emit_frame(&self) -> Function {
        // params slot(0):i32, delta(1),now(2):f64. locals: sub(3),toMsg(4):eqref
        let mut f = Function::new([(2, eqref())]);
        f.instruction(&Instruction::GlobalGet(G_FRAMES));
        f.instruction(&cast_to(T_ARR));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::LocalSet(3)); // sub
        ctor_arg0(&mut f, 3);
        f.instruction(&Instruction::LocalSet(4)); // toMsg
        f.instruction(&Instruction::LocalGet(4)); // toMsg (for apply1)
        // deltaFlag = sub.arg1 (Bool i31)
        ctor_argn(&mut f, 3, 1);
        f.instruction(&Instruction::RefCastNonNull(HeapType::Abstract { shared: false, ty: AbstractHeapType::I31 }));
        f.instruction(&Instruction::I31GetS);
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        // delta as Float
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::StructNew(T_FLOAT));
        f.instruction(&Instruction::Else);
        // Posix now (opaque: T_CTOR tag0 [Int])
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I64TruncF64S);
        f.instruction(&Instruction::Call(self.box_int_idx));
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 1 });
        f.instruction(&Instruction::StructNew(T_CTOR));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&Instruction::Call(self.dispatch_msg_idx));
        f.instruction(&Instruction::End);
        f
    }

    /// box_int(v) : represent an i64 as an Int value. Small values [-2^30, 2^30)
    /// become an UNBOXED i31ref (no heap allocation — the whole point); larger
    /// values fall back to a boxed T_INT. `unbox_int` reads either form.
    fn emit_box_int(&self) -> Function {
        // param v(0):i64
        let mut f = Function::new([]);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I64Const(I31_MIN));
        f.instruction(&Instruction::I64GeS);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I64Const(I31_MAX));
        f.instruction(&Instruction::I64LtS);
        f.instruction(&Instruction::I32And);
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::RefI31);
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::StructNew(T_INT));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f
    }

    /// unbox_int(v) : read an Int (either i31ref or boxed T_INT) as an i64.
    fn emit_unbox_int(&self) -> Function {
        // param v(0):eqref
        let mut f = Function::new([]);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::RefTestNonNull(HeapType::Abstract { shared: false, ty: AbstractHeapType::I31 }));
        f.instruction(&Instruction::If(BlockType::Result(ValType::I64)));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::RefCastNonNull(HeapType::Abstract { shared: false, ty: AbstractHeapType::I31 }));
        f.instruction(&Instruction::I31GetS);
        f.instruction(&Instruction::I64ExtendI32S);
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_INT));
        f.instruction(&Instruction::StructGet { struct_type_index: T_INT, field_index: 0 });
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f
    }

    /// msort_rec(arr, buf, lo, hi) : stable recursive merge sort of arr[lo,hi)
    /// using buf[lo,hi) as scratch, ordering by val_compare. `by_key` compares
    /// element[0] (for Dict/Set pairs) instead of the element itself. O(n log n)
    /// — replaces the old O(n²) insertion sort / fold-insert.
    fn emit_msort_rec(&self, mode: SortMode) -> Function {
        // params arr(0),buf(1):ref null T_ARR, lo(2),hi(3):i32.
        // locals mid(4),li(5),ri(6),di(7):i32
        let mut f = Function::new([(4, ValType::I32)]);
        let self_idx = match mode {
            SortMode::Value => self.msort_idx,
            SortMode::ByKey => self.msort_key_idx,
            SortMode::Cmp => self.msort_cmp_idx,
        };
        // push arr[idx] (or arr[idx][0] when by_key) as eqref, for comparison
        let operand = |f: &mut Function, idx_local: u32| {
            f.instruction(&Instruction::LocalGet(0));
            f.instruction(&cast_to(T_ARR));
            f.instruction(&Instruction::LocalGet(idx_local));
            f.instruction(&Instruction::ArrayGet(T_ARR));
            if mode == SortMode::ByKey {
                f.instruction(&cast_to(T_ARR));
                f.instruction(&Instruction::I32Const(0));
                f.instruction(&Instruction::ArrayGet(T_ARR));
            }
        };
        // if hi - lo <= 1: return
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32LeS);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // mid = (lo + hi) / 2
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32ShrS);
        f.instruction(&Instruction::LocalSet(4));
        // sort halves
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::Call(self_idx));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::Call(self_idx));
        // merge into buf: li=lo, ri=mid, di=lo
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalSet(6));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalSet(7));
        // set buf[di] = arr[src], then di++/src++
        let take = |f: &mut Function, src_local: u32| {
            f.instruction(&Instruction::LocalGet(1));
            f.instruction(&cast_to(T_ARR));
            f.instruction(&Instruction::LocalGet(7));
            f.instruction(&Instruction::LocalGet(0));
            f.instruction(&cast_to(T_ARR));
            f.instruction(&Instruction::LocalGet(src_local));
            f.instruction(&Instruction::ArrayGet(T_ARR));
            f.instruction(&Instruction::ArraySet(T_ARR));
            bump(f, 7, 1);
            bump(f, src_local, 1);
        };
        // main merge loop
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        // take_left iff arr[li] should not come after arr[ri] (stable on ties).
        if mode == SortMode::Cmp {
            // cmp li ri == LT|EQ  (Order tag < GT=2)
            f.instruction(&Instruction::GlobalGet(G_SORT_CMP));
            operand(&mut f, 5);
            f.instruction(&Instruction::Call(self.apply1_idx));
            operand(&mut f, 6);
            f.instruction(&Instruction::Call(self.apply1_idx));
            f.instruction(&cast_to(T_CTOR));
            f.instruction(&Instruction::StructGet { struct_type_index: T_CTOR, field_index: 0 });
            f.instruction(&Instruction::I32Const(2));
            f.instruction(&Instruction::I32LtS);
        } else {
            operand(&mut f, 5);
            operand(&mut f, 6);
            f.instruction(&Instruction::Call(self.val_compare_idx));
            f.instruction(&Instruction::I32Const(0));
            f.instruction(&Instruction::I32LeS);
        }
        f.instruction(&Instruction::If(BlockType::Empty));
        take(&mut f, 5);
        f.instruction(&Instruction::Else);
        take(&mut f, 6);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        // drain left
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        take(&mut f, 5);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        // drain right
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        take(&mut f, 6);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        // copy buf[lo,hi) back into arr[lo,hi)
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_ARR));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_ARR));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::ArrayCopy { array_type_index_dst: T_ARR, array_type_index_src: T_ARR });
        f.instruction(&Instruction::End);
        f
    }

    /// sorted_build(list) : build a Dict (by_key=true; `list` = (k,v) pairs) or
    /// Set (elements) as a key-sorted vector in O(n log n) — merge sort then
    /// dedup, keeping the LAST of each equal-key run (matches Elm fromList's
    /// last-wins, since a stable sort preserves input order within a run).
    /// Replaces the old O(n²) fold-insert.
    fn emit_sorted_build(&self, by_key: bool) -> Function {
        // param list(0). locals: n(1),i(2),oi(3),start(4):i32;
        //   arr(5),buf(6),data(7),out(8):ref T_ARR
        let mut f = Function::new([(4, ValType::I32), (4, ref_to(T_ARR))]);
        let sort_idx = if by_key { self.msort_key_idx } else { self.msort_idx };
        // push the compare-key of arr[i] (+1 for the next element)
        let key = |f: &mut Function, plus1: bool| {
            f.instruction(&Instruction::LocalGet(5));
            f.instruction(&cast_to(T_ARR));
            f.instruction(&Instruction::LocalGet(2));
            if plus1 {
                f.instruction(&Instruction::I32Const(1));
                f.instruction(&Instruction::I32Add);
            }
            f.instruction(&Instruction::ArrayGet(T_ARR));
            if by_key {
                f.instruction(&cast_to(T_ARR));
                f.instruction(&Instruction::I32Const(0));
                f.instruction(&Instruction::ArrayGet(T_ARR));
            }
        };
        list_len(&mut f, 0);
        f.instruction(&Instruction::LocalSet(1));
        // empty input → empty result (an empty list's backing is null)
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Empty));
        push_empty_list(&mut f);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        list_data(&mut f, 0);
        f.instruction(&Instruction::LocalSet(7));
        list_start(&mut f, 0);
        f.instruction(&Instruction::LocalSet(4));
        // arr = fresh[n]; arr[0..n] <- data[start..start+n]
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::ArrayNewDefault(T_ARR));
        f.instruction(&Instruction::LocalSet(5));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::ArrayCopy { array_type_index_dst: T_ARR, array_type_index_src: T_ARR });
        // buf = fresh[n]; sort arr[0,n)
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::ArrayNewDefault(T_ARR));
        f.instruction(&Instruction::LocalSet(6));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(sort_idx));
        // dedup keep-last into out[0..oi)
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::ArrayNewDefault(T_ARR));
        f.instruction(&Instruction::LocalSet(8));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        // keep = (i+1 >= n) || key(i) != key(i+1)
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::If(BlockType::Result(ValType::I32)));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::Else);
        key(&mut f, false);
        key(&mut f, true);
        f.instruction(&Instruction::Call(self.val_compare_idx));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32Ne);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::If(BlockType::Empty));
        // out[oi] = arr[i]; oi++
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&cast_to(T_ARR));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&cast_to(T_ARR));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::ArraySet(T_ARR));
        bump(&mut f, 3, 1);
        f.instruction(&Instruction::End);
        bump(&mut f, 2, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        // Copy the oi kept elements into a tight backing (the vector invariant
        // is head+len==cap; reuse buf local 6 for the final array).
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::ArrayNewDefault(T_ARR));
        f.instruction(&Instruction::LocalSet(6));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(8));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::ArrayCopy { array_type_index_dst: T_ARR, array_type_index_src: T_ARR });
        // T_LIST{oi, T_BACK{0, final}}
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::StructNew(T_BACK));
        f.instruction(&Instruction::StructNew(T_LIST));
        f.instruction(&Instruction::End);
        f
    }

    /// alm_dom_event(slot, ptr, len) : fire a Browser.Events document listener —
    /// parse the event JSON at [ptr,len), run the stored decoder, and dispatch
    /// the message on success (a failed decode is silently ignored, as in Elm).
    fn emit_dom_event(&self) -> Function {
        // params slot(0),ptr(1),len(2). locals: decoder(3),val(4),r(5):eqref
        let mut f = Function::new([(3, eqref())]);
        f.instruction(&Instruction::GlobalGet(G_DOMSUBS));
        f.instruction(&cast_to(T_ARR));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::LocalSet(3)); // decoder
        // val = Ok-value(json_parse(str_from_mem(ptr,len)))
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::Call(self.str_from_mem_idx));
        f.instruction(&Instruction::Call(self.json_parse_idx));
        f.instruction(&Instruction::LocalSet(4));
        ctor_arg0(&mut f, 4);
        f.instruction(&Instruction::LocalSet(4));
        // r = json_run(decoder, val)
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::Call(self.json_run_idx));
        f.instruction(&Instruction::LocalSet(5));
        ctor_tag(&mut f, 5);
        f.instruction(&Instruction::I32Eqz); // Ok?
        f.instruction(&Instruction::If(BlockType::Empty));
        ctor_arg0(&mut f, 5);
        f.instruction(&Instruction::Call(self.dispatch_msg_idx));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f
    }

    /// alm_http_response(reqId, status, ptr, len) : settle an in-flight request.
    /// Look up the stored Expect, build a `Result Http.Error a` (Http.Error tags:
    /// BadUrl=0, Timeout=1, NetworkError=2, BadStatus=3, BadBody=4), apply the
    /// Expect's `toMsg`, and dispatch. Supports expectString (tag0) and
    /// expectJson (tag1 [toMsg, decoder]). status 0 = network error.
    fn emit_http_response(&self) -> Function {
        // params reqId(0),status(1),ptr(2),len(3). locals: tag(4),is2xx(5):i32,
        //   expect(6),toMsg(7),body(8),result(9):eqref
        let mut f = Function::new([(2, ValType::I32), (4, eqref())]);
        // expect = G_HTTP[reqId]
        f.instruction(&Instruction::GlobalGet(G_HTTP));
        f.instruction(&cast_to(T_ARR));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::LocalSet(6));
        ctor_tag(&mut f, 6);
        f.instruction(&Instruction::LocalSet(4)); // expect kind
        ctor_arg0(&mut f, 6);
        f.instruction(&Instruction::LocalSet(7)); // toMsg
        // body = str_from_mem(ptr, len)
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::Call(self.str_from_mem_idx));
        f.instruction(&Instruction::LocalSet(8));
        // is2xx = 200 <= status < 300
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(200));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Const(300));
        f.instruction(&Instruction::I32LtS);
        f.instruction(&Instruction::I32And);
        f.instruction(&Instruction::LocalSet(5));
        // result:
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        // 2xx: json vs string
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        // expectJson: r = decodeString(decoder, body)
        ctor_argn(&mut f, 6, 1); // decoder
        f.instruction(&Instruction::LocalGet(8)); // body
        f.instruction(&Instruction::Call(self.json_decstr_idx));
        f.instruction(&Instruction::LocalSet(9));
        ctor_tag(&mut f, 9);
        f.instruction(&Instruction::I32Eqz); // Ok?
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        f.instruction(&Instruction::LocalGet(9)); // Ok val — already a Result
        f.instruction(&Instruction::Else);
        // Err (BadBody "")
        f.instruction(&Instruction::I32Const(1)); // Err
        f.instruction(&Instruction::I32Const(4)); // BadBody
        push_str_const(&mut f, "");
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 1 });
        f.instruction(&Instruction::StructNew(T_CTOR));
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 1 });
        f.instruction(&Instruction::StructNew(T_CTOR));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::Else);
        // expectWhatever (kind 2) → Ok () ; expectString → Ok body
        f.instruction(&Instruction::I32Const(0)); // Ok
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Const(2));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::RefI31); // ()
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::LocalGet(8)); // body
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 1 });
        f.instruction(&Instruction::StructNew(T_CTOR));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::Else);
        // non-2xx: status 0 → NetworkError, else BadStatus status
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        f.instruction(&Instruction::I32Const(1)); // Err
        f.instruction(&Instruction::I32Const(2)); // NetworkError
        f.instruction(&Instruction::RefNull(HeapType::Concrete(T_ARR)));
        f.instruction(&Instruction::StructNew(T_CTOR));
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 1 });
        f.instruction(&Instruction::StructNew(T_CTOR));
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::I32Const(1)); // Err
        f.instruction(&Instruction::I32Const(3)); // BadStatus
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::I64ExtendI32S);
        f.instruction(&Instruction::Call(self.box_int_idx));
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 1 });
        f.instruction(&Instruction::StructNew(T_CTOR));
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 1 });
        f.instruction(&Instruction::StructNew(T_CTOR));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalSet(9)); // result
        // msg = toMsg result ; dispatch
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::LocalGet(9));
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&Instruction::Call(self.dispatch_msg_idx));
        f.instruction(&Instruction::End);
        f
    }

    /// sub_find_port(sub, name) : return the `toMsg` of the SubPort in `sub`
    /// whose name equals `name`, or null if none. Sub value model: none=tag0,
    /// batch=tag1 [List Sub], SubPort=tag2 [name, toMsg]. Recurses into batches.
    fn emit_sub_find_port(&self) -> Function {
        // params sub(0), name(1). locals: tag(2),len(3),i(4):i32, list(5),r(6):eqref
        let mut f = Function::new([(3, ValType::I32), (2, eqref())]);
        ctor_tag(&mut f, 0);
        f.instruction(&Instruction::LocalSet(2));
        // SubPort (tag 2): compare names
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(2));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Empty));
        ctor_arg0(&mut f, 0); // sub name
        f.instruction(&Instruction::LocalGet(1)); // target name
        f.instruction(&Instruction::Call(self.val_eq_idx));
        f.instruction(&Instruction::RefCastNonNull(HeapType::Abstract {
            shared: false,
            ty: AbstractHeapType::I31,
        }));
        f.instruction(&Instruction::I31GetS);
        f.instruction(&Instruction::If(BlockType::Empty));
        ctor_argn(&mut f, 0, 1); // toMsg
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        // batch (tag 1): search each child
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Empty));
        ctor_arg0(&mut f, 0);
        f.instruction(&Instruction::LocalSet(5)); // list
        list_len(&mut f, 5);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        list_elem(&mut f, 5, 4);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(self.sub_find_port_idx));
        f.instruction(&Instruction::LocalSet(6));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::RefIsNull);
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        bump(&mut f, 4, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        // none / not found
        f.instruction(&Instruction::RefNull(eq_heap()));
        f.instruction(&Instruction::End);
        f
    }

    /// alm_port_in(np, nl, jp, jl) : deliver an incoming-port message. Parse the
    /// JSON payload, walk the current subscriptions for a SubPort whose name
    /// matches, apply its `toMsg` to the value, and dispatch. `sub_find_port`
    /// recurses into Sub.batch (tag1); SubPort is tag2 [name, toMsg].
    fn emit_port_in(&self) -> Function {
        // params np(0),nl(1),jp(2),jl(3). locals: name(4), val(5), sub(6), toMsg(7):eqref
        let mut f = Function::new([(4, eqref())]);
        // subscriptions must exist (element/document with a Sub)
        f.instruction(&Instruction::GlobalGet(G_SUBS));
        f.instruction(&Instruction::RefIsNull);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // name = str_from_mem(np, nl)
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(self.str_from_mem_idx));
        f.instruction(&Instruction::LocalSet(4));
        // sub = subscriptions(model)
        f.instruction(&Instruction::GlobalGet(G_SUBS));
        f.instruction(&Instruction::GlobalGet(G_MODEL));
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&Instruction::LocalSet(6));
        // toMsg = sub_find_port(sub, name)  (null if none)
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::Call(self.sub_find_port_idx));
        f.instruction(&Instruction::LocalSet(7));
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::RefIsNull);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // val = Ok-value(json_parse(str_from_mem(jp, jl)))
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::Call(self.str_from_mem_idx));
        f.instruction(&Instruction::Call(self.json_parse_idx));
        f.instruction(&Instruction::LocalSet(5));
        ctor_arg0(&mut f, 5); // Result Ok [value] → value
        f.instruction(&Instruction::LocalSet(5));
        // dispatch_msg(toMsg value)
        f.instruction(&Instruction::LocalGet(7));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&Instruction::Call(self.dispatch_msg_idx));
        f.instruction(&Instruction::End);
        f
    }

    /// alm_browser_start() : unpack the program (sandbox / element / document /
    /// application), wire model/update/view (+ initial cmd), render and mount.
    fn emit_alm_browser_start(&self, main_idx: u32) -> Function {
        // locals: rec(0), prog(1), initcmd(2), urlM(3):eqref
        let mut f = Function::new([(4, eqref())]);
        f.instruction(&Instruction::Call(main_idx));
        f.instruction(&cast_to(T_CTOR));
        f.instruction(&Instruction::LocalSet(1)); // prog
        // record = prog.arg[0]
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_CTOR));
        f.instruction(&Instruction::StructGet { struct_type_index: T_CTOR, field_index: 1 });
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::LocalSet(0)); // record
        let field = |f: &mut Function, i: i32| {
            f.instruction(&Instruction::LocalGet(0));
            f.instruction(&cast_to(T_ARR));
            f.instruction(&Instruction::I32Const(i));
            f.instruction(&Instruction::ArrayGet(T_ARR));
        };
        // re-read the program record into local 0 (used after init overwrites it)
        let reload_rec = |f: &mut Function| {
            f.instruction(&Instruction::LocalGet(1));
            f.instruction(&cast_to(T_CTOR));
            f.instruction(&Instruction::StructGet { struct_type_index: T_CTOR, field_index: 1 });
            f.instruction(&Instruction::I32Const(0));
            f.instruction(&Instruction::ArrayGet(T_ARR));
            f.instruction(&Instruction::LocalSet(0));
        };
        // unpack a (model, cmd) init tuple: model→G_MODEL, cmd→initcmd(2)
        let unpack_tuple = |f: &mut Function| {
            f.instruction(&cast_to(T_ARR));
            f.instruction(&Instruction::LocalSet(0));
            f.instruction(&Instruction::LocalGet(0));
            f.instruction(&cast_to(T_ARR));
            f.instruction(&Instruction::I32Const(0));
            f.instruction(&Instruction::ArrayGet(T_ARR));
            f.instruction(&Instruction::GlobalSet(G_MODEL));
            f.instruction(&Instruction::LocalGet(0));
            f.instruction(&cast_to(T_ARR));
            f.instruction(&Instruction::I32Const(1));
            f.instruction(&Instruction::ArrayGet(T_ARR));
            f.instruction(&Instruction::LocalSet(2));
        };
        // G_KIND = program tag (0 sandbox, 1 element, 2 document, 3 application)
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_CTOR));
        f.instruction(&Instruction::StructGet { struct_type_index: T_CTOR, field_index: 0 });
        f.instruction(&Instruction::GlobalSet(G_KIND));
        f.instruction(&Instruction::GlobalGet(G_KIND));
        f.instruction(&Instruction::I32Const(3));
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::If(BlockType::Empty));
        // application: {init:0, onUrlChange:1, onUrlRequest:2, subscriptions:3,
        // update:4, view:5}. tuple = init flags url key.
        field(&mut f, 0); // init
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::RefI31); // flags = ()
        f.instruction(&Instruction::Call(self.apply1_idx));
        // url = Just-value(url_from_string(str_from_mem(0, host_get_url(0))))
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::Call(HOST_GET_URL));
        f.instruction(&Instruction::Call(self.str_from_mem_idx));
        f.instruction(&Instruction::Call(self.url_from_string_idx));
        f.instruction(&Instruction::LocalSet(3));
        ctor_arg0(&mut f, 3); // Just → Url
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::RefI31); // key = ()
        f.instruction(&Instruction::Call(self.apply1_idx));
        unpack_tuple(&mut f);
        reload_rec(&mut f);
        field(&mut f, 4);
        f.instruction(&Instruction::GlobalSet(G_UPDATE));
        field(&mut f, 5);
        f.instruction(&Instruction::GlobalSet(G_VIEW));
        field(&mut f, 3);
        f.instruction(&Instruction::GlobalSet(G_SUBS));
        field(&mut f, 1);
        f.instruction(&Instruction::GlobalSet(G_URLCHG));
        f.instruction(&Instruction::Else);
        // element/document (tag 1/2) vs sandbox (tag 0)
        f.instruction(&Instruction::GlobalGet(G_KIND));
        f.instruction(&Instruction::If(BlockType::Empty));
        // record = {init:0, subscriptions:1, update:2, view:3}; init(()) → tuple
        field(&mut f, 0);
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::RefI31);
        f.instruction(&Instruction::Call(self.apply1_idx));
        unpack_tuple(&mut f);
        reload_rec(&mut f);
        field(&mut f, 2);
        f.instruction(&Instruction::GlobalSet(G_UPDATE));
        field(&mut f, 3);
        f.instruction(&Instruction::GlobalSet(G_VIEW));
        field(&mut f, 1);
        f.instruction(&Instruction::GlobalSet(G_SUBS)); // subscriptions fn
        f.instruction(&Instruction::Else);
        // sandbox: record = {init:0, update:1, view:2}
        field(&mut f, 0);
        f.instruction(&Instruction::GlobalSet(G_MODEL));
        field(&mut f, 1);
        f.instruction(&Instruction::GlobalSet(G_UPDATE));
        field(&mut f, 2);
        f.instruction(&Instruction::GlobalSet(G_VIEW));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        // handlers = new T_ARR(MAX_HANDLERS)
        f.instruction(&Instruction::I32Const(MAX_HANDLERS as i32));
        f.instruction(&Instruction::ArrayNewDefault(T_ARR));
        f.instruction(&Instruction::GlobalSet(G_HANDLERS));
        // http = new T_ARR(MAX_HANDLERS)
        f.instruction(&Instruction::I32Const(MAX_HANDLERS as i32));
        f.instruction(&Instruction::ArrayNewDefault(T_ARR));
        f.instruction(&Instruction::GlobalSet(G_HTTP));
        // ticks = new T_ARR(MAX_HANDLERS)
        f.instruction(&Instruction::I32Const(MAX_HANDLERS as i32));
        f.instruction(&Instruction::ArrayNewDefault(T_ARR));
        f.instruction(&Instruction::GlobalSet(G_TICKS));
        // domsubs = new T_ARR(MAX_HANDLERS)
        f.instruction(&Instruction::I32Const(MAX_HANDLERS as i32));
        f.instruction(&Instruction::ArrayNewDefault(T_ARR));
        f.instruction(&Instruction::GlobalSet(G_DOMSUBS));
        // frames = new T_ARR(MAX_HANDLERS)
        f.instruction(&Instruction::I32Const(MAX_HANDLERS as i32));
        f.instruction(&Instruction::ArrayNewDefault(T_ARR));
        f.instruction(&Instruction::GlobalSet(G_FRAMES));
        Self::emit_reset_render(&mut f);
        // run initial cmd (element / document, i.e. G_KIND != 0)
        f.instruction(&Instruction::GlobalGet(G_KIND));
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::Call(self.run_cmd_idx));
        f.instruction(&Instruction::End);
        Self::emit_reset_render(&mut f);
        // prev = document/application ? doc_vnode() : view model
        f.instruction(&Instruction::GlobalGet(G_KIND));
        f.instruction(&Instruction::I32Const(2));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        f.instruction(&Instruction::Call(self.render_document_idx));
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::GlobalGet(G_VIEW));
        f.instruction(&Instruction::GlobalGet(G_MODEL));
        f.instruction(&Instruction::Call(self.apply1_idx));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::GlobalSet(G_PREV));
        // root = render_dom(prev) ; mount ; record both
        f.instruction(&Instruction::GlobalGet(G_PREV));
        f.instruction(&Instruction::Call(self.render_dom_idx));
        f.instruction(&Instruction::GlobalSet(G_ROOT));
        f.instruction(&Instruction::GlobalGet(G_ROOT));
        f.instruction(&Instruction::Call(DOM_MOUNT));
        // register initial timer subscriptions
        f.instruction(&Instruction::Call(self.reconcile_subs_idx));
        f.instruction(&Instruction::End);
        f
    }

    /// val_eq(a, b) : structural equality, returning a Bool (`i31`). Dispatches
    /// on the runtime heap type; recurses into cons cells, ctor args, and
    /// tuple/record arrays.
    fn emit_val_eq(&self) -> Function {
        // locals: a(0), b(1), i(2):i32, la(3):i32, lb(4):i32
        let mut f = Function::new([(3, ValType::I32)]);
        let false_ret = |f: &mut Function| {
            f.instruction(&Instruction::I32Const(0));
            f.instruction(&Instruction::RefI31);
            f.instruction(&Instruction::Return);
        };
        let test = |f: &mut Function, local: u32, ty: u32| {
            f.instruction(&Instruction::LocalGet(local));
            f.instruction(&Instruction::RefTestNonNull(HeapType::Concrete(ty)));
        };
        // Nil handling: a null?
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::RefIsNull);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::RefIsNull);
        f.instruction(&Instruction::RefI31);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // a non-null; b null → false
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::RefIsNull);
        f.instruction(&Instruction::If(BlockType::Empty));
        false_ret(&mut f);
        f.instruction(&Instruction::End);
        // i31 (Bool/Char/Unit, or a small Int) — unbox BOTH as i64 so a small
        // i31 Int compares correctly against a large boxed-T_INT Int.
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::RefTestNonNull(HeapType::Abstract { shared: false, ty: AbstractHeapType::I31 }));
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::Call(self.unbox_int_idx));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(self.unbox_int_idx));
        f.instruction(&Instruction::I64Eq);
        f.instruction(&Instruction::RefI31);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // T_TNODE (Dict/Set): equal iff their sorted pair-lists are equal.
        test(&mut f, 0, T_TNODE);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(0));
        push_empty_list(&mut f);
        f.instruction(&Instruction::Call(self.treap_pairs_idx));
        f.instruction(&Instruction::LocalGet(1));
        push_empty_list(&mut f);
        f.instruction(&Instruction::Call(self.treap_pairs_idx));
        f.instruction(&Instruction::Call(self.val_eq_idx));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // T_INT (large Int)
        test(&mut f, 0, T_INT);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::Call(self.unbox_int_idx));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(self.unbox_int_idx));
        f.instruction(&Instruction::I64Eq);
        f.instruction(&Instruction::RefI31);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // T_FLOAT
        test(&mut f, 0, T_FLOAT);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_FLOAT));
        f.instruction(&Instruction::StructGet { struct_type_index: T_FLOAT, field_index: 0 });
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_FLOAT));
        f.instruction(&Instruction::StructGet { struct_type_index: T_FLOAT, field_index: 0 });
        f.instruction(&Instruction::F64Eq);
        f.instruction(&Instruction::RefI31);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // T_STR: byte compare
        test(&mut f, 0, T_STR);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_STR));
        f.instruction(&Instruction::ArrayLen);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_STR));
        f.instruction(&Instruction::ArrayLen);
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Ne);
        f.instruction(&Instruction::If(BlockType::Empty));
        false_ret(&mut f);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_STR));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::ArrayGetU(T_STR));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_STR));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::ArrayGetU(T_STR));
        f.instruction(&Instruction::I32Ne);
        f.instruction(&Instruction::If(BlockType::Empty));
        false_ret(&mut f);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::RefI31);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // T_LIST: equal length, then elementwise
        test(&mut f, 0, T_LIST);
        f.instruction(&Instruction::If(BlockType::Empty));
        list_len(&mut f, 0);
        f.instruction(&Instruction::LocalSet(3));
        list_len(&mut f, 1);
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Ne);
        f.instruction(&Instruction::If(BlockType::Empty));
        false_ret(&mut f);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        list_elem(&mut f, 0, 2);
        list_elem(&mut f, 1, 2);
        f.instruction(&Instruction::Call(self.val_eq_idx));
        f.instruction(&Instruction::RefCastNonNull(HeapType::Abstract { shared: false, ty: AbstractHeapType::I31 }));
        f.instruction(&Instruction::I31GetS);
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Empty));
        false_ret(&mut f);
        f.instruction(&Instruction::End);
        bump(&mut f, 2, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::RefI31);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // T_CTOR: tags equal, then args arrays elementwise
        test(&mut f, 0, T_CTOR);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_CTOR));
        f.instruction(&Instruction::StructGet { struct_type_index: T_CTOR, field_index: 0 });
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_CTOR));
        f.instruction(&Instruction::StructGet { struct_type_index: T_CTOR, field_index: 0 });
        f.instruction(&Instruction::I32Ne);
        f.instruction(&Instruction::If(BlockType::Empty));
        false_ret(&mut f);
        f.instruction(&Instruction::End);
        // nullary ctor (args null) → equal
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_CTOR));
        f.instruction(&Instruction::StructGet { struct_type_index: T_CTOR, field_index: 1 });
        f.instruction(&Instruction::RefIsNull);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::RefI31);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // compare arg arrays: reuse local 0/1 as the arrays
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_CTOR));
        f.instruction(&Instruction::StructGet { struct_type_index: T_CTOR, field_index: 1 });
        f.instruction(&Instruction::LocalSet(0));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_CTOR));
        f.instruction(&Instruction::StructGet { struct_type_index: T_CTOR, field_index: 1 });
        f.instruction(&Instruction::LocalSet(1));
        self.emit_arr_eq(&mut f);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // T_ARR: tuples / records, elementwise
        test(&mut f, 0, T_ARR);
        f.instruction(&Instruction::If(BlockType::Empty));
        self.emit_arr_eq(&mut f);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // Fallback: reference identity.
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::RefEq);
        f.instruction(&Instruction::RefI31);
        f.instruction(&Instruction::End);
        f
    }

    /// In `val_eq`: compare the two `T_ARR`s in locals 0/1 elementwise, ending
    /// with a `return` of the Bool result. Uses locals 2(i),3(la),4(lb).
    fn emit_arr_eq(&self, f: &mut Function) {
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_ARR));
        f.instruction(&Instruction::ArrayLen);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_ARR));
        f.instruction(&Instruction::ArrayLen);
        f.instruction(&Instruction::LocalSet(4));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::I32Ne);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::RefI31);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_ARR));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_ARR));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::Call(self.val_eq_idx));
        f.instruction(&Instruction::RefCastNonNull(HeapType::Abstract { shared: false, ty: AbstractHeapType::I31 }));
        f.instruction(&Instruction::I31GetS);
        f.instruction(&Instruction::I32Eqz);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::RefI31);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::RefI31);
    }

    /// Build a closure value for a top-level function used first-class.
    fn emit_make_closure(&self, func_idx: u32, arity: u32, f: &mut Function) {
        f.instruction(&Instruction::RefFunc(func_idx));
        f.instruction(&Instruction::I32Const(arity as i32));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32Const(arity as i32));
        f.instruction(&Instruction::ArrayNewDefault(T_ARR));
        f.instruction(&Instruction::StructNew(T_CLOS));
    }

    /// Emit one function: N eqref params -> eqref body.
    fn emit_fn(&mut self, f: &crate::ir::mono::TypedFn) -> Result<Function, String> {
        let nparams = f.params.len() as u32;
        // Extra eqref locals for `let`/`case`/destructure bindings, plus any
        // temporaries needed to destructure non-trivial parameter patterns.
        let param_dtor: u32 = f
            .params
            .iter()
            .filter(|(p, _)| !matches!(p.value, can::Pattern_::Var(_) | can::Pattern_::Anything))
            .map(|(p, _)| pat_size(p))
            .sum();
        let extra = count_bindings(&f.body) + param_dtor;
        // Reserve two scratch locals after the binding block: one eqref + one
        // i64 for inlining unbox_int/box_int on the arithmetic hot path.
        let mut wf = Function::new([(extra + 1, eqref()), (1, ValType::I64)]);
        let mut ctx = FnCtx::new();
        ctx.next_local = nparams;
        ctx.scratch_eqref = nparams + extra;
        ctx.scratch_i64 = nparams + extra + 1;
        for (i, (pat, _)) in f.params.iter().enumerate() {
            self.bind_pat(pat, i as u32, &mut ctx, &mut wf)?;
        }
        self.emit_expr(&f.body, &mut ctx, &mut wf)?;
        wf.instruction(&Instruction::End);
        Ok(wf)
    }

    /// Emit an expression, leaving one `eqref` on the stack.
    fn emit_expr(&mut self, e: &TypedExpr, ctx: &mut FnCtx, f: &mut Function) -> Result<(), String> {
        match &e.kind {
            TypedKind::Int(n) => {
                f.instruction(&Instruction::I64Const(*n));
                f.instruction(&Instruction::Call(self.box_int_idx));
            }
            TypedKind::Float(x) => {
                f.instruction(&Instruction::F64Const((*x).into()));
                f.instruction(&Instruction::StructNew(T_FLOAT));
            }
            TypedKind::Str(s) => {
                let (off, len) = self.intern_str(s);
                f.instruction(&Instruction::I32Const(off as i32));
                f.instruction(&Instruction::I32Const(len as i32));
                f.instruction(&Instruction::ArrayNewData {
                    array_type_index: T_STR,
                    array_data_index: 0,
                });
            }
            TypedKind::Chr(c) => {
                f.instruction(&Instruction::I32Const(*c as i32));
                f.instruction(&Instruction::RefI31);
            }
            TypedKind::Unit => {
                f.instruction(&Instruction::I32Const(0));
                f.instruction(&Instruction::RefI31);
            }
            TypedKind::Ctor(_, _, ctor) if ctor.name.as_str() == "True" => {
                f.instruction(&Instruction::I32Const(1));
                f.instruction(&Instruction::RefI31);
            }
            TypedKind::Ctor(_, _, ctor) if ctor.name.as_str() == "False" => {
                f.instruction(&Instruction::I32Const(0));
                f.instruction(&Instruction::RefI31);
            }
            // A constructor used as a value: nullary (`Nothing`, `Red`) builds
            // the tagged value directly; one with arguments (e.g. `Just`,
            // `Typed` passed to `map`) becomes a closure that constructs it.
            TypedKind::Ctor(_, _, ctor) => {
                let arity = ctor.arity as u32;
                if arity == 0 {
                    self.emit_ctor(ctor.index, &[], ctx, f)?;
                } else {
                    self.fn_type(arity);
                    let args: Vec<TypedExpr> = (0..arity)
                        .map(|i| TypedExpr {
                            tipe: e.tipe.clone(),
                            kind: TypedKind::Local(format!("$carg{i}").into()),
                            region: Region::ZERO,
                        })
                        .collect();
                    let mut lctx = FnCtx::new();
                    lctx.next_local = arity;
                    for i in 0..arity {
                        lctx.scope.push((format!("$carg{i}"), i));
                    }
                    let lidx = self.lifted_base + self.lifted.len() as u32;
                    self.lifted.push((arity, Function::new([])));
                    let mut lf = Function::new([]);
                    self.emit_ctor(ctor.index, &args, &mut lctx, &mut lf)?;
                    lf.instruction(&Instruction::End);
                    let slot = (lidx - self.lifted_base) as usize;
                    self.lifted[slot] = (arity, lf);
                    self.emit_make_closure(lidx, arity, f);
                }
            }
            TypedKind::List(items) => {
                // Build a tight vector: push len and head-index (constants),
                // then the elements head-first, then fold into T_ARR/T_BACK/T_LIST.
                if items.is_empty() {
                    push_empty_list(f);
                } else {
                    let n = items.len() as u32;
                    f.instruction(&Instruction::I32Const(n as i32)); // len
                    f.instruction(&Instruction::I32Const(0)); // head index
                    for item in items {
                        self.emit_expr(item, ctx, f)?;
                    }
                    f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: n });
                    f.instruction(&Instruction::StructNew(T_BACK));
                    f.instruction(&Instruction::StructNew(T_LIST));
                }
            }
            TypedKind::Tuple(a, b, c) => {
                self.emit_expr(a, ctx, f)?;
                self.emit_expr(b, ctx, f)?;
                let mut n = 2;
                if let Some(c) = c {
                    self.emit_expr(c, ctx, f)?;
                    n = 3;
                }
                f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: n });
            }
            TypedKind::Record(fields) => {
                let mut sorted: Vec<_> = fields.iter().collect();
                sorted.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
                for (_, v) in &sorted {
                    self.emit_expr(v, ctx, f)?;
                }
                f.instruction(&Instruction::ArrayNewFixed {
                    array_type_index: T_ARR,
                    array_size: sorted.len() as u32,
                });
            }
            TypedKind::Access(rec, field) => {
                let idx = record_field_index(&rec.tipe, field.as_str())?;
                self.emit_expr(rec, ctx, f)?;
                f.instruction(&cast_to(T_ARR));
                f.instruction(&Instruction::I32Const(idx as i32));
                f.instruction(&Instruction::ArrayGet(T_ARR));
            }
            TypedKind::Update(rec, updates) => {
                // Build a fresh record: overridden fields take the new value,
                // the rest are copied from the old record (fields sorted by name).
                let names: Vec<String> = match &rec.tipe {
                    can::Type::Record(fs, _) => {
                        let mut n: Vec<String> = fs.iter().map(|(n, _)| n.to_string()).collect();
                        n.sort();
                        n
                    }
                    _ => return Err("wasmgc: record update on a non-record type".into()),
                };
                let r = ctx.bind("$upd");
                self.emit_expr(rec, ctx, f)?;
                f.instruction(&Instruction::LocalSet(r));
                for (i, fname) in names.iter().enumerate() {
                    if let Some((_, ve)) = updates.iter().find(|(n, _)| n.as_str() == fname) {
                        self.emit_expr(ve, ctx, f)?;
                    } else {
                        f.instruction(&Instruction::LocalGet(r));
                        f.instruction(&cast_to(T_ARR));
                        f.instruction(&Instruction::I32Const(i as i32));
                        f.instruction(&Instruction::ArrayGet(T_ARR));
                    }
                }
                f.instruction(&Instruction::ArrayNewFixed {
                    array_type_index: T_ARR,
                    array_size: names.len() as u32,
                });
            }
            TypedKind::Case(scrut, branches) => self.emit_case(scrut, branches, ctx, f)?,
            TypedKind::Lambda(params, body) => self.lift(params, body, ctx, f)?,
            // `.field` as a first-class function → a lambda `\$acc -> $acc.field`.
            TypedKind::Accessor(field) => {
                let (rec_ty, field_ty) = match &e.tipe {
                    can::Type::Lambda(a, b) => ((**a).clone(), (**b).clone()),
                    _ => return Err("wasmgc: accessor has non-function type".into()),
                };
                let acc = TypedExpr {
                    tipe: rec_ty.clone(),
                    kind: TypedKind::Local("$acc".into()),
                    region: e.region,
                };
                let body = TypedExpr {
                    tipe: field_ty,
                    kind: TypedKind::Access(Box::new(acc), field.clone()),
                    region: e.region,
                };
                let pat = crate::reporting::annotation::Located {
                    region: Region::ZERO,
                    value: can::Pattern_::Var("$acc".into()),
                };
                self.lift(&[(pat, rec_ty)], &body, ctx, f)?;
            }
            // A kernel used as a first-class value (e.g. `(+)` passed to foldl).
            TypedKind::Foreign(module, name) => {
                self.emit_foreign_value(module.as_str(), name.as_str(), &e.tipe, f)?
            }
            TypedKind::Negate(x) if is_float(&x.tipe) => {
                self.emit_f64(x, ctx, f)?;
                f.instruction(&Instruction::F64Neg);
                f.instruction(&Instruction::StructNew(T_FLOAT));
            }
            TypedKind::Negate(x) => {
                f.instruction(&Instruction::I64Const(0));
                self.emit_i64(x, ctx, f)?;
                f.instruction(&Instruction::I64Sub);
                f.instruction(&Instruction::Call(self.box_int_idx));
            }
            TypedKind::Binop(op, _, _, l, r) => self.emit_binop(op.as_str(), l, r, ctx, f)?,
            TypedKind::If(branches, otherwise) => self.emit_if(branches, otherwise, ctx, f)?,
            TypedKind::Local(name) => {
                let idx = ctx
                    .lookup(name.as_str())
                    .ok_or_else(|| format!("wasmgc: unbound local `{name}`"))?;
                f.instruction(&Instruction::LocalGet(idx));
            }
            TypedKind::Let(decls, body) => self.emit_let(decls, body, ctx, f)?,
            TypedKind::Global(name) => {
                let key = name.to_string();
                let idx = *self
                    .func_index
                    .get(&key)
                    .ok_or_else(|| format!("wasmgc: unknown global `{name}`"))?;
                let arity = self.func_arity[&key];
                if arity == 0 {
                    f.instruction(&Instruction::Call(idx));
                } else {
                    // A function used as a first-class value → a closure.
                    self.emit_make_closure(idx, arity, f);
                }
            }
            TypedKind::Call(func, args) => self.emit_call(func, args, ctx, f)?,
            other => return Err(format!("wasmgc: unsupported expression {other:?}")),
        }
        Ok(())
    }

    /// Emit an `Int`-typed expression, leaving an unboxed `i64`.
    fn emit_i64(&mut self, e: &TypedExpr, ctx: &mut FnCtx, f: &mut Function) -> Result<(), String> {
        // Literal operand: emit the raw i64, skipping a pointless box→unbox.
        if let TypedKind::Int(n) = &e.kind {
            f.instruction(&Instruction::I64Const(*n));
            return Ok(());
        }
        self.emit_expr(e, ctx, f)?;
        // Int is i31ref (small) or boxed T_INT (large). Inline the read when a
        // scratch local is reserved (hot path), else call the out-of-line helper.
        let s = ctx.scratch_eqref;
        if s == u32::MAX {
            f.instruction(&Instruction::Call(self.unbox_int_idx));
            return Ok(());
        }
        f.instruction(&Instruction::LocalTee(s));
        f.instruction(&Instruction::RefTestNonNull(HeapType::Abstract { shared: false, ty: AbstractHeapType::I31 }));
        f.instruction(&Instruction::If(BlockType::Result(ValType::I64)));
        f.instruction(&Instruction::LocalGet(s));
        f.instruction(&Instruction::RefCastNonNull(HeapType::Abstract { shared: false, ty: AbstractHeapType::I31 }));
        f.instruction(&Instruction::I31GetS);
        f.instruction(&Instruction::I64ExtendI32S);
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::LocalGet(s));
        f.instruction(&cast_to(T_INT));
        f.instruction(&Instruction::StructGet { struct_type_index: T_INT, field_index: 0 });
        f.instruction(&Instruction::End);
        Ok(())
    }

    /// Box an i64 (on the stack) as an Int, inlining the i31 fast path when a
    /// scratch local is reserved, else calling box_int.
    fn emit_box_int_inline(&self, ctx: &FnCtx, f: &mut Function) {
        let s = ctx.scratch_i64;
        if s == u32::MAX {
            f.instruction(&Instruction::Call(self.box_int_idx));
            return;
        }
        f.instruction(&Instruction::LocalSet(s));
        f.instruction(&Instruction::LocalGet(s));
        f.instruction(&Instruction::I64Const(I31_MIN));
        f.instruction(&Instruction::I64GeS);
        f.instruction(&Instruction::LocalGet(s));
        f.instruction(&Instruction::I64Const(I31_MAX));
        f.instruction(&Instruction::I64LtS);
        f.instruction(&Instruction::I32And);
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        f.instruction(&Instruction::LocalGet(s));
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::RefI31);
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::LocalGet(s));
        f.instruction(&Instruction::StructNew(T_INT));
        f.instruction(&Instruction::End);
    }

    /// Emit a `Char`-typed expression, leaving its unboxed code point (`i32`).
    fn emit_char_code(&mut self, e: &TypedExpr, ctx: &mut FnCtx, f: &mut Function) -> Result<(), String> {
        self.emit_expr(e, ctx, f)?;
        f.instruction(&Instruction::RefCastNonNull(HeapType::Abstract {
            shared: false,
            ty: AbstractHeapType::I31,
        }));
        f.instruction(&Instruction::I31GetS);
        Ok(())
    }

    /// Emit a `Float`-typed expression, leaving an unboxed `f64`.
    fn emit_f64(&mut self, e: &TypedExpr, ctx: &mut FnCtx, f: &mut Function) -> Result<(), String> {
        self.emit_expr(e, ctx, f)?;
        f.instruction(&cast_to(T_FLOAT));
        f.instruction(&Instruction::StructGet { struct_type_index: T_FLOAT, field_index: 0 });
        Ok(())
    }

    /// Emit a `Bool`-typed expression, leaving an unboxed `i32` (0/1).
    fn emit_bool(&mut self, e: &TypedExpr, ctx: &mut FnCtx, f: &mut Function) -> Result<(), String> {
        // Bool is i31; extract the i32.
        self.emit_expr(e, ctx, f)?;
        f.instruction(&Instruction::RefCastNonNull(HeapType::Abstract {
            shared: false,
            ty: AbstractHeapType::I31,
        }));
        f.instruction(&Instruction::I31GetS);
        Ok(())
    }

    fn emit_binop(
        &mut self,
        op: &str,
        l: &TypedExpr,
        r: &TypedExpr,
        ctx: &mut FnCtx,
        f: &mut Function,
    ) -> Result<(), String> {
        match op {
            "+" | "-" | "*" if is_float(&l.tipe) => {
                self.emit_f64(l, ctx, f)?;
                self.emit_f64(r, ctx, f)?;
                f.instruction(&match op {
                    "+" => Instruction::F64Add,
                    "-" => Instruction::F64Sub,
                    _ => Instruction::F64Mul,
                });
                f.instruction(&Instruction::StructNew(T_FLOAT));
            }
            "+" | "-" | "*" | "//" => {
                self.emit_i64(l, ctx, f)?;
                self.emit_i64(r, ctx, f)?;
                f.instruction(&match op {
                    "+" => Instruction::I64Add,
                    "-" => Instruction::I64Sub,
                    "*" => Instruction::I64Mul,
                    _ => Instruction::I64DivS,
                });
                self.emit_box_int_inline(ctx, f);
            }
            "/" => {
                self.emit_f64(l, ctx, f)?;
                self.emit_f64(r, ctx, f)?;
                f.instruction(&Instruction::F64Div);
                f.instruction(&Instruction::StructNew(T_FLOAT));
            }
            // `^` is Math.pow for both Int and Float (as in JS); the Int result
            // is the truncated power.
            "^" if is_float(&l.tipe) => {
                self.emit_f64(l, ctx, f)?;
                self.emit_f64(r, ctx, f)?;
                f.instruction(&Instruction::Call(MATH_POW));
                f.instruction(&Instruction::StructNew(T_FLOAT));
            }
            "^" => {
                self.emit_i64(l, ctx, f)?;
                f.instruction(&Instruction::F64ConvertI64S);
                self.emit_i64(r, ctx, f)?;
                f.instruction(&Instruction::F64ConvertI64S);
                f.instruction(&Instruction::Call(MATH_POW));
                f.instruction(&Instruction::I64TruncF64S);
                self.emit_box_int_inline(ctx, f);
            }
            "<" | ">" | "<=" | ">=" => {
                if is_float(&l.tipe) {
                    self.emit_f64(l, ctx, f)?;
                    self.emit_f64(r, ctx, f)?;
                    f.instruction(&match op {
                        "<" => Instruction::F64Lt,
                        ">" => Instruction::F64Gt,
                        "<=" => Instruction::F64Le,
                        _ => Instruction::F64Ge,
                    });
                } else {
                    self.emit_i64(l, ctx, f)?;
                    self.emit_i64(r, ctx, f)?;
                    f.instruction(&match op {
                        "<" => Instruction::I64LtS,
                        ">" => Instruction::I64GtS,
                        "<=" => Instruction::I64LeS,
                        _ => Instruction::I64GeS,
                    });
                }
                f.instruction(&Instruction::RefI31);
            }
            "==" => {
                self.emit_expr(l, ctx, f)?;
                self.emit_expr(r, ctx, f)?;
                f.instruction(&Instruction::Call(self.val_eq_idx));
            }
            "/=" => {
                self.emit_expr(l, ctx, f)?;
                self.emit_expr(r, ctx, f)?;
                f.instruction(&Instruction::Call(self.val_eq_idx));
                f.instruction(&Instruction::RefCastNonNull(HeapType::Abstract {
                    shared: false,
                    ty: AbstractHeapType::I31,
                }));
                f.instruction(&Instruction::I31GetS);
                f.instruction(&Instruction::I32Eqz);
                f.instruction(&Instruction::RefI31);
            }
            "&&" => {
                self.emit_bool(l, ctx, f)?;
                f.instruction(&Instruction::If(BlockType::Result(eqref())));
                self.emit_expr(r, ctx, f)?;
                f.instruction(&Instruction::Else);
                f.instruction(&Instruction::I32Const(0));
                f.instruction(&Instruction::RefI31);
                f.instruction(&Instruction::End);
            }
            "||" => {
                self.emit_bool(l, ctx, f)?;
                f.instruction(&Instruction::If(BlockType::Result(eqref())));
                f.instruction(&Instruction::I32Const(1));
                f.instruction(&Instruction::RefI31);
                f.instruction(&Instruction::Else);
                self.emit_expr(r, ctx, f)?;
                f.instruction(&Instruction::End);
            }
            "++" if is_string(&l.tipe) => {
                self.emit_expr(l, ctx, f)?;
                self.emit_expr(r, ctx, f)?;
                f.instruction(&Instruction::Call(self.str_append_idx));
            }
            "++" => {
                self.emit_expr(l, ctx, f)?;
                self.emit_expr(r, ctx, f)?;
                f.instruction(&Instruction::Call(self.list_append_idx));
            }
            "::" => {
                self.emit_expr(l, ctx, f)?;
                self.emit_expr(r, ctx, f)?;
                f.instruction(&Instruction::Call(self.list_cons_idx));
            }
            "|>" => {
                // x |> f  ==  f x
                self.emit_expr(r, ctx, f)?; // the function
                self.emit_expr(l, ctx, f)?; // the argument
                f.instruction(&Instruction::Call(self.apply1_idx));
            }
            "<|" => {
                // f <| x  ==  f x
                self.emit_expr(l, ctx, f)?;
                self.emit_expr(r, ctx, f)?;
                f.instruction(&Instruction::Call(self.apply1_idx));
            }
            other => return Err(format!("wasmgc: unsupported binop `{other}`")),
        }
        Ok(())
    }

    fn emit_if(
        &mut self,
        branches: &[(TypedExpr, TypedExpr)],
        otherwise: &TypedExpr,
        ctx: &mut FnCtx,
        f: &mut Function,
    ) -> Result<(), String> {
        match branches.split_first() {
            None => self.emit_expr(otherwise, ctx, f),
            Some(((cond, then), rest)) => {
                self.emit_bool(cond, ctx, f)?;
                f.instruction(&Instruction::If(BlockType::Result(eqref())));
                self.emit_expr(then, ctx, f)?;
                f.instruction(&Instruction::Else);
                self.emit_if(rest, otherwise, ctx, f)?;
                f.instruction(&Instruction::End);
                Ok(())
            }
        }
    }

    fn emit_let(
        &mut self,
        decls: &[TypedLetDecl],
        body: &TypedExpr,
        ctx: &mut FnCtx,
        f: &mut Function,
    ) -> Result<(), String> {
        let mark = ctx.scope.len();
        for d in decls {
            self.emit_let_decl(d, ctx, f)?;
        }
        self.emit_expr(body, ctx, f)?;
        ctx.scope.truncate(mark);
        Ok(())
    }

    fn emit_let_decl(
        &mut self,
        d: &TypedLetDecl,
        ctx: &mut FnCtx,
        f: &mut Function,
    ) -> Result<(), String> {
        match d {
            TypedLetDecl::Def { name, params, body } if params.is_empty() => {
                self.emit_expr(body, ctx, f)?;
                let slot = ctx.bind(name.as_str());
                f.instruction(&Instruction::LocalSet(slot));
            }
            TypedLetDecl::Destruct(pat, expr) => {
                // Evaluate once into a scratch local, then bind the pattern.
                self.emit_expr(expr, ctx, f)?;
                let slot = ctx.bind("$destr");
                f.instruction(&Instruction::LocalSet(slot));
                self.bind_pat(pat, slot, ctx, f)?;
            }
            // A local function: lift it to a closure and bind the name.
            TypedLetDecl::Def { name, params, body } => {
                self.lift(params, body, ctx, f)?;
                let slot = ctx.bind(name.as_str());
                f.instruction(&Instruction::LocalSet(slot));
            }
            // A single-binding non-recursive group is common; flatten it.
            TypedLetDecl::Recursive(ds) => {
                for d2 in ds {
                    self.emit_let_decl(d2, ctx, f)?;
                }
            }
        }
        Ok(())
    }

    fn emit_call(
        &mut self,
        func: &TypedExpr,
        args: &[TypedExpr],
        ctx: &mut FnCtx,
        f: &mut Function,
    ) -> Result<(), String> {
        // A port applied to its argument: `outPort payload` builds CMD_PORT
        // (T_CTOR tag2 [name, jsonValue]). Ports have no definition.
        if let TypedKind::Local(name) | TypedKind::Global(name) = &func.kind {
            if let Some(&outgoing) = self.ports.get(name.as_str()) {
                if args.len() == 1 {
                    // outgoing → CMD_PORT [name, value]; incoming → SubPort
                    // [name, toMsg]. Both are T_CTOR tag2 (in the Cmd/Sub space).
                    f.instruction(&Instruction::I32Const(2));
                    push_str_const(f, name.as_str());
                    self.emit_expr(&args[0], ctx, f)?;
                    f.instruction(&Instruction::ArrayNewFixed {
                        array_type_index: T_ARR,
                        array_size: 2,
                    });
                    f.instruction(&Instruction::StructNew(T_CTOR));
                    let _ = outgoing;
                    return Ok(());
                }
            }
        }
        if let TypedKind::Global(name) = &func.kind {
            if let Some(&idx) = self.func_index.get(&name.to_string()) {
                let arity = self.func_arity[&name.to_string()] as usize;
                if args.len() == arity {
                    for a in args {
                        self.emit_expr(a, ctx, f)?;
                    }
                    f.instruction(&Instruction::Call(idx));
                } else if args.len() < arity {
                    // Partial application → build up a closure.
                    self.emit_make_closure(idx, arity as u32, f);
                    for a in args {
                        self.emit_expr(a, ctx, f)?;
                        f.instruction(&Instruction::Call(self.apply1_idx));
                    }
                } else {
                    // Over-application: call at full arity, then apply the rest.
                    for a in &args[..arity] {
                        self.emit_expr(a, ctx, f)?;
                    }
                    f.instruction(&Instruction::Call(idx));
                    for a in &args[arity..] {
                        self.emit_expr(a, ctx, f)?;
                        f.instruction(&Instruction::Call(self.apply1_idx));
                    }
                }
                return Ok(());
            }
        }
        if let TypedKind::Ctor(_, _, ctor) = &func.kind {
            if ctor.name.as_str() == "True" || ctor.name.as_str() == "False" {
                // (shouldn't be applied, but be safe)
                return self.emit_expr(func, ctx, f);
            }
            return self.emit_ctor(ctor.index, args, ctx, f);
        }
        if let TypedKind::Foreign(module, name) = &func.kind {
            return self.emit_kernel(module.as_str(), name.as_str(), args, ctx, f);
        }
        // The callee is an expression that evaluates to a closure (e.g. a
        // let-bound or parameter function): apply each argument via apply1.
        self.emit_expr(func, ctx, f)?;
        for a in args {
            self.emit_expr(a, ctx, f)?;
            f.instruction(&Instruction::Call(self.apply1_idx));
        }
        Ok(())
    }

    /// Build a JSON `Value` (`Json.Encode`): a `T_CTOR { tag, [arg] }`.
    fn emit_json_value(
        &mut self,
        tag: i32,
        arg: &TypedExpr,
        ctx: &mut FnCtx,
        f: &mut Function,
    ) -> Result<(), String> {
        f.instruction(&Instruction::I32Const(tag));
        self.emit_expr(arg, ctx, f)?;
        f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 1 });
        f.instruction(&Instruction::StructNew(T_CTOR));
        Ok(())
    }

    /// Build a decoder AST node: `T_CTOR { tag, [args…] }` (null args if none).
    fn emit_dnode(
        &mut self,
        tag: i32,
        args: &[&TypedExpr],
        ctx: &mut FnCtx,
        f: &mut Function,
    ) -> Result<(), String> {
        f.instruction(&Instruction::I32Const(tag));
        if args.is_empty() {
            f.instruction(&Instruction::RefNull(HeapType::Concrete(T_ARR)));
        } else {
            for a in args {
                self.emit_expr(a, ctx, f)?;
            }
            f.instruction(&Instruction::ArrayNewFixed {
                array_type_index: T_ARR,
                array_size: args.len() as u32,
            });
        }
        f.instruction(&Instruction::StructNew(T_CTOR));
        Ok(())
    }

    /// A saturated call to a known kernel (`Foreign`).
    fn emit_kernel(
        &mut self,
        module: &str,
        name: &str,
        args: &[TypedExpr],
        ctx: &mut FnCtx,
        f: &mut Function,
    ) -> Result<(), String> {
        match (module, name) {
            ("String", "fromInt") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::Call(self.str_from_int_idx));
            }
            ("String", "append") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.str_append_idx));
            }
            ("String", "length") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::Call(self.str_length_idx));
            }
            ("List", "map") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.list_map_idx));
            }
            ("List", "foldl") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                self.emit_expr(&args[2], ctx, f)?;
                f.instruction(&Instruction::Call(self.list_foldl_idx));
            }
            ("List", "length") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::Call(self.list_length_idx));
            }
            ("List", "reverse") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::Call(self.list_reverse_idx));
            }
            ("List", "filter") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.list_filter_idx));
            }
            ("List", "foldr") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                self.emit_expr(&args[2], ctx, f)?;
                f.instruction(&Instruction::Call(self.list_foldr_idx));
            }
            ("List", "append") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.list_append_idx));
            }
            ("List", "range") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.list_range_idx));
            }
            ("List", "member") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.list_member_idx));
            }
            ("List", "take") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.list_take_idx));
            }
            ("List", "drop") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.list_drop_idx));
            }
            ("List", "concat") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::Call(self.list_concat_idx));
            }
            ("List", "head") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::Call(self.list_head_idx));
            }
            ("List", "tail") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::Call(self.list_tail_idx));
            }
            ("List", "singleton") => {
                self.emit_expr(&args[0], ctx, f)?;
                push_empty_list(f);
                f.instruction(&Instruction::Call(self.list_cons_idx));
            }
            ("List", "cons") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.list_cons_idx));
            }
            // String.concat xs = String.join "" xs (str_join takes sep, then list).
            ("String", "concat") => {
                push_str_const(f, "");
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::Call(self.str_join_idx));
            }
            ("Maybe", "withDefault") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.maybe_with_default_idx));
            }
            ("Maybe", "map") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.maybe_map_idx));
            }
            ("Maybe", "andThen") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.maybe_and_then_idx));
            }
            ("Maybe", "map2") | ("Maybe", "map3") => {
                for a in args {
                    self.emit_expr(a, ctx, f)?;
                }
                let idx = if name == "map2" { self.maybe_map2_idx } else { self.maybe_map3_idx };
                f.instruction(&Instruction::Call(idx));
            }
            ("Result", "withDefault") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.result_with_default_idx));
            }
            ("Result", "map") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.result_map_idx));
            }
            ("Result", "mapError") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.result_map_error_idx));
            }
            ("Result", "map2") | ("Result", "map3") => {
                for a in args {
                    self.emit_expr(a, ctx, f)?;
                }
                let idx = if name == "map2" { self.result_map2_idx } else { self.result_map3_idx };
                f.instruction(&Instruction::Call(idx));
            }
            // Random: reify each generator as a tagged ctor; random_step runs it.
            ("Random", "int") | ("Random", "float") | ("Random", "constant")
            | ("Random", "map") | ("Random", "map2") | ("Random", "map3")
            | ("Random", "andThen") | ("Random", "pair") => {
                let tag: i32 = match name {
                    "int" => 0,
                    "float" => 1,
                    "constant" => 2,
                    "map" => 3,
                    "map2" => 4,
                    "map3" => 5,
                    "andThen" => 6,
                    _ => 7, // pair
                };
                f.instruction(&Instruction::I32Const(tag));
                for a in args {
                    self.emit_expr(a, ctx, f)?;
                }
                f.instruction(&Instruction::ArrayNewFixed {
                    array_type_index: T_ARR,
                    array_size: args.len() as u32,
                });
                f.instruction(&Instruction::StructNew(T_CTOR));
            }
            ("Random", "step") => {
                self.emit_expr(&args[0], ctx, f)?; // generator
                self.emit_expr(&args[1], ctx, f)?; // seed
                f.instruction(&Instruction::Call(self.random_step_idx));
            }
            ("Random", "initialSeed") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::Call(self.random_initial_seed_idx));
            }
            // Task: reify combinators as tagged ctors; task_run interprets them.
            ("Task", "succeed") | ("Task", "fail") | ("Task", "map")
            | ("Task", "andThen") | ("Task", "mapError") | ("Task", "onError")
            | ("Task", "map2") | ("Task", "map3") | ("Task", "sequence") => {
                let tag: i32 = match name {
                    "succeed" => 0,
                    "fail" => 1,
                    "map" => 2,
                    "andThen" => 3,
                    "mapError" => 4,
                    "onError" => 5,
                    "map2" => 6,
                    "map3" => 7,
                    _ => 8, // sequence
                };
                f.instruction(&Instruction::I32Const(tag));
                for a in args {
                    self.emit_expr(a, ctx, f)?;
                }
                f.instruction(&Instruction::ArrayNewFixed {
                    array_type_index: T_ARR,
                    array_size: args.len() as u32,
                });
                f.instruction(&Instruction::StructNew(T_CTOR));
            }
            // Task.perform toMsg task → CMD_TASK_PERFORM (7); attempt → (8).
            ("Task", "perform") | ("Task", "attempt") => {
                let tag: i32 = if name == "perform" { 7 } else { 8 };
                f.instruction(&Instruction::I32Const(tag));
                self.emit_expr(&args[0], ctx, f)?; // toMsg
                self.emit_expr(&args[1], ctx, f)?; // task
                f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 2 });
                f.instruction(&Instruction::StructNew(T_CTOR));
            }
            ("Result", "andThen") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.result_and_then_idx));
            }
            ("Result", "toMaybe") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::Call(self.result_to_maybe_idx));
            }
            ("Result", "fromMaybe") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.result_from_maybe_idx));
            }
            ("Basics", "clamp") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                self.emit_expr(&args[2], ctx, f)?;
                f.instruction(&Instruction::Call(self.clamp_idx));
            }
            // Dict: a key-sorted vector of [k,v] pairs.
            // Dict/Set are persistent treaps (see emit_val_hash/emit_treap_*).
            // Fast ops (insert/get/remove/member) use the treap directly;
            // bulk ops convert treap→sorted-pair-list, reuse the pair-list
            // helpers, and rebuild the treap via treap_insert_pairs.
            ("Dict", "empty") => { f.instruction(&Instruction::RefNull(HeapType::Concrete(T_TNODE))); }
            ("Dict", "singleton") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::RefNull(HeapType::Concrete(T_TNODE)));
                f.instruction(&Instruction::Call(self.treap_insert_idx));
            }
            ("Dict", "insert") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                self.emit_expr(&args[2], ctx, f)?;
                f.instruction(&Instruction::Call(self.treap_insert_idx));
            }
            ("Dict", "get") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.treap_get_idx));
            }
            ("Dict", "member") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.treap_get_idx));
                f.instruction(&cast_to(T_CTOR));
                f.instruction(&Instruction::StructGet { struct_type_index: T_CTOR, field_index: 0 });
                f.instruction(&Instruction::I32Eqz); // Just → True
                f.instruction(&Instruction::RefI31);
            }
            ("Dict", "remove") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.treap_remove_idx));
            }
            ("Dict", "update") => {
                // from_pairs(dict_update(k, f, pairs(d)))
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                self.emit_expr(&args[2], ctx, f)?;
                push_empty_list(f);
                f.instruction(&Instruction::Call(self.treap_pairs_idx));
                f.instruction(&Instruction::Call(self.dict_update_idx));
                f.instruction(&Instruction::RefNull(HeapType::Concrete(T_TNODE)));
                f.instruction(&Instruction::Call(self.treap_insert_pairs_idx));
            }
            ("Dict", "size") => {
                self.emit_expr(&args[0], ctx, f)?;
                push_empty_list(f);
                f.instruction(&Instruction::Call(self.treap_pairs_idx));
                f.instruction(&Instruction::Call(self.list_length_idx));
            }
            ("Dict", "isEmpty") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::RefIsNull);
                f.instruction(&Instruction::RefI31);
            }
            ("Dict", "toList") => {
                self.emit_expr(&args[0], ctx, f)?;
                push_empty_list(f);
                f.instruction(&Instruction::Call(self.treap_pairs_idx));
            }
            ("Dict", "keys") => {
                self.emit_expr(&args[0], ctx, f)?;
                push_empty_list(f);
                f.instruction(&Instruction::Call(self.treap_pairs_idx));
                f.instruction(&Instruction::Call(self.dict_keys_idx));
            }
            ("Dict", "values") => {
                self.emit_expr(&args[0], ctx, f)?;
                push_empty_list(f);
                f.instruction(&Instruction::Call(self.treap_pairs_idx));
                f.instruction(&Instruction::Call(self.dict_values_idx));
            }
            ("Dict", "fromList") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::RefNull(HeapType::Concrete(T_TNODE)));
                f.instruction(&Instruction::Call(self.treap_insert_pairs_idx));
            }
            ("Dict", "foldl") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                self.emit_expr(&args[2], ctx, f)?;
                push_empty_list(f);
                f.instruction(&Instruction::Call(self.treap_pairs_idx));
                f.instruction(&Instruction::Call(self.dict_foldl_idx));
            }
            ("Dict", "foldr") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                self.emit_expr(&args[2], ctx, f)?;
                push_empty_list(f);
                f.instruction(&Instruction::Call(self.treap_pairs_idx));
                f.instruction(&Instruction::Call(self.dict_foldr_idx));
            }
            ("Dict", "map") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                push_empty_list(f);
                f.instruction(&Instruction::Call(self.treap_pairs_idx));
                f.instruction(&Instruction::Call(self.dict_map_idx));
                f.instruction(&Instruction::RefNull(HeapType::Concrete(T_TNODE)));
                f.instruction(&Instruction::Call(self.treap_insert_pairs_idx));
            }
            ("Dict", "filter") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                push_empty_list(f);
                f.instruction(&Instruction::Call(self.treap_pairs_idx));
                f.instruction(&Instruction::Call(self.dict_filter_idx));
                f.instruction(&Instruction::RefNull(HeapType::Concrete(T_TNODE)));
                f.instruction(&Instruction::Call(self.treap_insert_pairs_idx));
            }
            ("Dict", "union") => {
                // insert all of a's pairs into b (a wins)
                self.emit_expr(&args[0], ctx, f)?;
                push_empty_list(f);
                f.instruction(&Instruction::Call(self.treap_pairs_idx));
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.treap_insert_pairs_idx));
            }
            ("Dict", "intersect") => {
                // from_pairs(dict_intersect(pairs(a), pairs(b)))
                self.emit_expr(&args[0], ctx, f)?;
                push_empty_list(f);
                f.instruction(&Instruction::Call(self.treap_pairs_idx));
                self.emit_expr(&args[1], ctx, f)?;
                push_empty_list(f);
                f.instruction(&Instruction::Call(self.treap_pairs_idx));
                f.instruction(&Instruction::Call(self.dict_intersect_idx));
                f.instruction(&Instruction::RefNull(HeapType::Concrete(T_TNODE)));
                f.instruction(&Instruction::Call(self.treap_insert_pairs_idx));
            }
            ("Dict", "diff") => {
                // from_pairs(dict_diff(pairs(b) toRemove, pairs(a) base))
                self.emit_expr(&args[1], ctx, f)?;
                push_empty_list(f);
                f.instruction(&Instruction::Call(self.treap_pairs_idx));
                self.emit_expr(&args[0], ctx, f)?;
                push_empty_list(f);
                f.instruction(&Instruction::Call(self.treap_pairs_idx));
                f.instruction(&Instruction::Call(self.dict_diff_idx));
                f.instruction(&Instruction::RefNull(HeapType::Concrete(T_TNODE)));
                f.instruction(&Instruction::Call(self.treap_insert_pairs_idx));
            }
            // Set = a treap of (element → Unit). toList = the treap's keys.
            ("Set", "empty") => { f.instruction(&Instruction::RefNull(HeapType::Concrete(T_TNODE))); }
            ("Set", "singleton") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::I32Const(0));
                f.instruction(&Instruction::RefI31);
                f.instruction(&Instruction::RefNull(HeapType::Concrete(T_TNODE)));
                f.instruction(&Instruction::Call(self.treap_insert_idx));
            }
            ("Set", "insert") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::I32Const(0));
                f.instruction(&Instruction::RefI31);
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.treap_insert_idx));
            }
            ("Set", "remove") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.treap_remove_idx));
            }
            ("Set", "member") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.treap_get_idx));
                f.instruction(&cast_to(T_CTOR));
                f.instruction(&Instruction::StructGet { struct_type_index: T_CTOR, field_index: 0 });
                f.instruction(&Instruction::I32Eqz);
                f.instruction(&Instruction::RefI31);
            }
            ("Set", "size") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_set_to_list(f);
                f.instruction(&Instruction::Call(self.list_length_idx));
            }
            ("Set", "isEmpty") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::RefIsNull);
                f.instruction(&Instruction::RefI31);
            }
            ("Set", "toList") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_set_to_list(f);
            }
            ("Set", "fromList") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::RefNull(HeapType::Concrete(T_TNODE)));
                f.instruction(&Instruction::Call(self.treap_insert_elems_idx));
            }
            ("Set", "foldl") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                self.emit_expr(&args[2], ctx, f)?;
                self.emit_set_to_list(f);
                f.instruction(&Instruction::Call(self.list_foldl_idx));
            }
            ("Set", "foldr") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                self.emit_expr(&args[2], ctx, f)?;
                self.emit_set_to_list(f);
                f.instruction(&Instruction::Call(self.list_foldr_idx));
            }
            ("Set", "filter") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                self.emit_set_to_list(f);
                f.instruction(&Instruction::Call(self.list_filter_idx));
                f.instruction(&Instruction::RefNull(HeapType::Concrete(T_TNODE)));
                f.instruction(&Instruction::Call(self.treap_insert_elems_idx));
            }
            ("Set", "map") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                self.emit_set_to_list(f);
                f.instruction(&Instruction::Call(self.list_map_idx));
                f.instruction(&Instruction::RefNull(HeapType::Concrete(T_TNODE)));
                f.instruction(&Instruction::Call(self.treap_insert_elems_idx));
            }
            ("Set", "union") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_set_to_list(f);
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.treap_insert_elems_idx));
            }
            ("Set", "intersect") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_set_to_list(f);
                self.emit_expr(&args[1], ctx, f)?;
                self.emit_set_to_list(f);
                f.instruction(&Instruction::Call(self.set_intersect_idx));
                f.instruction(&Instruction::RefNull(HeapType::Concrete(T_TNODE)));
                f.instruction(&Instruction::Call(self.treap_insert_elems_idx));
            }
            ("Set", "diff") => {
                self.emit_expr(&args[1], ctx, f)?;
                self.emit_set_to_list(f);
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_set_to_list(f);
                f.instruction(&Instruction::Call(self.set_diff_idx));
                f.instruction(&Instruction::RefNull(HeapType::Concrete(T_TNODE)));
                f.instruction(&Instruction::Call(self.treap_insert_elems_idx));
            }
            ("Set", "partition") => {
                // list_partition over the elements → (inList, outList); wrap each
                // element-list back into a Set.
                let t = ctx.bind("$setpart");
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                self.emit_set_to_list(f);
                f.instruction(&Instruction::Call(self.list_partition_idx));
                f.instruction(&Instruction::LocalSet(t));
                // ( setOf part[0], setOf part[1] )
                f.instruction(&Instruction::LocalGet(t));
                f.instruction(&cast_to(T_ARR));
                f.instruction(&Instruction::I32Const(0));
                f.instruction(&Instruction::ArrayGet(T_ARR));
                f.instruction(&Instruction::RefNull(HeapType::Concrete(T_TNODE)));
                f.instruction(&Instruction::Call(self.treap_insert_elems_idx));
                f.instruction(&Instruction::LocalGet(t));
                f.instruction(&cast_to(T_ARR));
                f.instruction(&Instruction::I32Const(1));
                f.instruction(&Instruction::ArrayGet(T_ARR));
                f.instruction(&Instruction::RefNull(HeapType::Concrete(T_TNODE)));
                f.instruction(&Instruction::Call(self.treap_insert_elems_idx));
                f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 2 });
            }
            // Array: the same T_LIST vector — toList/fromList are identity and
            // most ops reuse the List kernels.
            ("Array", "empty") => push_empty_list(f),
            ("Array", "fromList") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::Call(self.list_to_array_idx));
            }
            ("Array", "toList") => {
                // a tight front-anchored array is a valid (tight) List
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::Call(self.array_tighten_idx));
            }
            ("Array", "length") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&cast_to(T_LIST));
                f.instruction(&Instruction::StructGet { struct_type_index: T_LIST, field_index: 0 });
                f.instruction(&Instruction::I64ExtendI32S);
                f.instruction(&Instruction::Call(self.box_int_idx));
            }
            ("Array", "isEmpty") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&cast_to(T_LIST));
                f.instruction(&Instruction::StructGet { struct_type_index: T_LIST, field_index: 0 });
                f.instruction(&Instruction::I32Eqz);
                f.instruction(&Instruction::RefI31);
            }
            ("Array", "repeat") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.list_repeat_idx));
            }
            ("Array", "get") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.array_get_idx));
            }
            ("Array", "set") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                self.emit_expr(&args[2], ctx, f)?;
                f.instruction(&Instruction::Call(self.array_tighten_idx));
                f.instruction(&Instruction::Call(self.array_set_idx));
            }
            ("Array", "push") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.array_push_idx));
            }
            ("Array", "append") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::Call(self.array_tighten_idx));
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.array_tighten_idx));
                f.instruction(&Instruction::Call(self.list_append_idx));
            }
            ("Array", "slice") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                self.emit_expr(&args[2], ctx, f)?;
                f.instruction(&Instruction::Call(self.array_tighten_idx));
                f.instruction(&Instruction::Call(self.array_slice_idx));
            }
            ("Array", "initialize") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.array_initialize_idx));
            }
            ("Array", "toIndexedList") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::Call(self.array_tighten_idx));
                f.instruction(&Instruction::Call(self.array_to_indexed_idx));
            }
            ("Array", "foldl") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                self.emit_expr(&args[2], ctx, f)?;
                f.instruction(&Instruction::Call(self.array_tighten_idx));
                f.instruction(&Instruction::Call(self.list_foldl_idx));
            }
            ("Array", "foldr") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                self.emit_expr(&args[2], ctx, f)?;
                f.instruction(&Instruction::Call(self.array_tighten_idx));
                f.instruction(&Instruction::Call(self.list_foldr_idx));
            }
            ("Array", "map") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.array_tighten_idx));
                f.instruction(&Instruction::Call(self.list_map_idx));
            }
            ("Array", "indexedMap") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::I64Const(0));
                f.instruction(&Instruction::Call(self.box_int_idx));
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.array_tighten_idx));
                f.instruction(&Instruction::Call(self.list_indexed_map_idx));
            }
            ("Array", "filter") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.array_tighten_idx));
                f.instruction(&Instruction::Call(self.list_filter_idx));
                // filter's result is a (non-tight, back-anchored) List — copy it
                // into a proper front-anchored Array backing.
                f.instruction(&Instruction::Call(self.list_to_array_idx));
            }
            // Json.Encode: build a tagged Value (0 null 1 bool 2 int 3 float
            // 4 string 5 array 6 object), then encode via json_enc.
            ("Json.Encode", "string") => self.emit_json_value(4, &args[0], ctx, f)?,
            ("Json.Encode", "int") => self.emit_json_value(2, &args[0], ctx, f)?,
            ("Json.Encode", "float") => self.emit_json_value(3, &args[0], ctx, f)?,
            ("Json.Encode", "bool") => self.emit_json_value(1, &args[0], ctx, f)?,
            ("Json.Encode", "object") => self.emit_json_value(6, &args[0], ctx, f)?,
            ("Json.Encode", "list") | ("Json.Encode", "array") | ("Json.Encode", "set") => {
                // tag 5 with (List.map f coll)
                f.instruction(&Instruction::I32Const(5));
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.list_map_idx));
                f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 1 });
                f.instruction(&Instruction::StructNew(T_CTOR));
            }
            ("Json.Encode", "dict") => {
                // tag 6 with json_dict_pairs(toKey, toVal, d)
                f.instruction(&Instruction::I32Const(6));
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                self.emit_expr(&args[2], ctx, f)?;
                f.instruction(&Instruction::Call(self.json_dict_pairs_idx));
                f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 1 });
                f.instruction(&Instruction::StructNew(T_CTOR));
            }
            ("Json.Encode", "encode") => {
                // json_enc(value, str_repeat(indent, " "), "")
                self.emit_expr(&args[1], ctx, f)?;
                self.emit_expr(&args[0], ctx, f)?;
                push_str_const(f, " ");
                f.instruction(&Instruction::Call(self.str_repeat_idx));
                push_str_const(f, "");
                f.instruction(&Instruction::Call(self.json_enc_idx));
            }
            // Json.Decode: build a decoder AST node (leaf decoders are nullary
            // values handled in emit_foreign_value).
            ("Json.Decode", "null") => self.emit_dnode(5, &[&args[0]], ctx, f)?,
            ("Json.Decode", "list") => self.emit_dnode(6, &[&args[0]], ctx, f)?,
            ("Json.Decode", "array") => self.emit_dnode(7, &[&args[0]], ctx, f)?,
            ("Json.Decode", "field") => self.emit_dnode(8, &[&args[0], &args[1]], ctx, f)?,
            ("Json.Decode", "index") => self.emit_dnode(9, &[&args[0], &args[1]], ctx, f)?,
            ("Json.Decode", "at") => self.emit_dnode(10, &[&args[0], &args[1]], ctx, f)?,
            ("Json.Decode", "keyValuePairs") => self.emit_dnode(11, &[&args[0]], ctx, f)?,
            ("Json.Decode", "dict") => self.emit_dnode(12, &[&args[0]], ctx, f)?,
            ("Json.Decode", "maybe") => self.emit_dnode(13, &[&args[0]], ctx, f)?,
            ("Json.Decode", "nullable") => self.emit_dnode(14, &[&args[0]], ctx, f)?,
            ("Json.Decode", "map") => self.emit_dnode(15, &[&args[0], &args[1]], ctx, f)?,
            ("Json.Decode", "map2") => {
                self.emit_dnode(16, &[&args[0], &args[1], &args[2]], ctx, f)?
            }
            ("Json.Decode", "map3") => {
                self.emit_dnode(17, &[&args[0], &args[1], &args[2], &args[3]], ctx, f)?
            }
            ("Json.Decode", "andThen") => self.emit_dnode(18, &[&args[0], &args[1]], ctx, f)?,
            ("Json.Decode", "oneOf") => self.emit_dnode(19, &[&args[0]], ctx, f)?,
            ("Json.Decode", "succeed") => self.emit_dnode(20, &[&args[0]], ctx, f)?,
            ("Json.Decode", "fail") => self.emit_dnode(21, &[&args[0]], ctx, f)?,
            ("Json.Decode", "lazy") => self.emit_dnode(22, &[&args[0]], ctx, f)?,
            ("Json.Decode", "decodeString") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.json_decstr_idx));
            }
            ("Json.Decode", "decodeValue") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.json_run_idx));
            }
            ("Tuple", "first") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&cast_to(T_ARR));
                f.instruction(&Instruction::I32Const(0));
                f.instruction(&Instruction::ArrayGet(T_ARR));
            }
            ("Tuple", "second") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&cast_to(T_ARR));
                f.instruction(&Instruction::I32Const(1));
                f.instruction(&Instruction::ArrayGet(T_ARR));
            }
            ("Tuple", "pair") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 2 });
            }
            ("Tuple", "mapFirst") => {
                // (f a, b)
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&cast_to(T_ARR));
                f.instruction(&Instruction::I32Const(0));
                f.instruction(&Instruction::ArrayGet(T_ARR));
                f.instruction(&Instruction::Call(self.apply1_idx));
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&cast_to(T_ARR));
                f.instruction(&Instruction::I32Const(1));
                f.instruction(&Instruction::ArrayGet(T_ARR));
                f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 2 });
            }
            ("Tuple", "mapSecond") => {
                // (a, f b)
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&cast_to(T_ARR));
                f.instruction(&Instruction::I32Const(0));
                f.instruction(&Instruction::ArrayGet(T_ARR));
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&cast_to(T_ARR));
                f.instruction(&Instruction::I32Const(1));
                f.instruction(&Instruction::ArrayGet(T_ARR));
                f.instruction(&Instruction::Call(self.apply1_idx));
                f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 2 });
            }
            ("Tuple", "mapBoth") => {
                // mapBoth f g (a, b) = (f a, g b)
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[2], ctx, f)?;
                f.instruction(&cast_to(T_ARR));
                f.instruction(&Instruction::I32Const(0));
                f.instruction(&Instruction::ArrayGet(T_ARR));
                f.instruction(&Instruction::Call(self.apply1_idx));
                self.emit_expr(&args[1], ctx, f)?;
                self.emit_expr(&args[2], ctx, f)?;
                f.instruction(&cast_to(T_ARR));
                f.instruction(&Instruction::I32Const(1));
                f.instruction(&Instruction::ArrayGet(T_ARR));
                f.instruction(&Instruction::Call(self.apply1_idx));
                f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 2 });
            }
            ("Basics", "xor") => {
                self.emit_bool(&args[0], ctx, f)?;
                self.emit_bool(&args[1], ctx, f)?;
                f.instruction(&Instruction::I32Ne);
                f.instruction(&Instruction::RefI31);
            }
            // Char classifiers: `(code - base) < span` (unsigned).
            ("Char", "isDigit") | ("Char", "isLower") | ("Char", "isUpper") => {
                let (base, span) = match name {
                    "isDigit" => (48, 10),
                    "isLower" => (97, 26),
                    _ => (65, 26),
                };
                self.emit_char_code(&args[0], ctx, f)?;
                f.instruction(&Instruction::I32Const(base));
                f.instruction(&Instruction::I32Sub);
                f.instruction(&Instruction::I32Const(span));
                f.instruction(&Instruction::I32LtU);
                f.instruction(&Instruction::RefI31);
            }
            // ASCII-only case fold; other code points pass through (partial
            // parity on non-ASCII, matching JS on ASCII).
            ("Char", "toUpper") | ("Char", "toLower")
            | ("Char", "toLocaleUpper") | ("Char", "toLocaleLower") => {
                let (base, span, delta) = if name.ends_with("Upper") {
                    (97, 26, -32)
                } else {
                    (65, 26, 32)
                };
                self.emit_char_code(&args[0], ctx, f)?; // folded
                f.instruction(&Instruction::I32Const(delta));
                f.instruction(&Instruction::I32Add);
                self.emit_char_code(&args[0], ctx, f)?; // unchanged
                self.emit_char_code(&args[0], ctx, f)?; // in-range test
                f.instruction(&Instruction::I32Const(base));
                f.instruction(&Instruction::I32Sub);
                f.instruction(&Instruction::I32Const(span));
                f.instruction(&Instruction::I32LtU);
                f.instruction(&Instruction::Select);
                f.instruction(&Instruction::RefI31);
            }
            // predicates built from unsigned range tests OR'd together
            ("Char", "isAlpha")
            | ("Char", "isAlphaNum")
            | ("Char", "isHexDigit")
            | ("Char", "isOctDigit") => {
                // (base, span) ranges whose union defines the class
                let ranges: &[(i32, i32)] = match name {
                    "isAlpha" => &[(65, 26), (97, 26)],
                    "isAlphaNum" => &[(48, 10), (65, 26), (97, 26)],
                    "isHexDigit" => &[(48, 10), (65, 6), (97, 6)],
                    _ => &[(48, 8)], // isOctDigit
                };
                for (i, &(base, span)) in ranges.iter().enumerate() {
                    self.emit_char_code(&args[0], ctx, f)?;
                    f.instruction(&Instruction::I32Const(base));
                    f.instruction(&Instruction::I32Sub);
                    f.instruction(&Instruction::I32Const(span));
                    f.instruction(&Instruction::I32LtU);
                    if i > 0 {
                        f.instruction(&Instruction::I32Or);
                    }
                }
                f.instruction(&Instruction::RefI31);
            }
            ("Basics", "abs") if is_float(&args[0].tipe) => {
                self.emit_f64(&args[0], ctx, f)?;
                f.instruction(&Instruction::F64Abs);
                f.instruction(&Instruction::StructNew(T_FLOAT));
            }
            ("Basics", "abs") => {
                // -a
                f.instruction(&Instruction::I64Const(0));
                self.emit_i64(&args[0], ctx, f)?;
                f.instruction(&Instruction::I64Sub);
                f.instruction(&Instruction::Call(self.box_int_idx));
                // a
                self.emit_expr(&args[0], ctx, f)?;
                // cond a < 0
                self.emit_i64(&args[0], ctx, f)?;
                f.instruction(&Instruction::I64Const(0));
                f.instruction(&Instruction::I64LtS);
                f.instruction(&Instruction::TypedSelect(eqref()));
            }
            ("Basics", "negate") if is_float(&args[0].tipe) => {
                self.emit_f64(&args[0], ctx, f)?;
                f.instruction(&Instruction::F64Neg);
                f.instruction(&Instruction::StructNew(T_FLOAT));
            }
            ("Basics", "negate") => {
                f.instruction(&Instruction::I64Const(0));
                self.emit_i64(&args[0], ctx, f)?;
                f.instruction(&Instruction::I64Sub);
                f.instruction(&Instruction::Call(self.box_int_idx));
            }
            ("Basics", "min") | ("Basics", "max") => {
                // a and b for the select, then compare(a,b) for the condition.
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.val_compare_idx));
                f.instruction(&Instruction::I32Const(0));
                // min: pick a when compare<=0; max: pick a when compare>0.
                f.instruction(if name == "min" {
                    &Instruction::I32LeS
                } else {
                    &Instruction::I32GtS
                });
                f.instruction(&Instruction::TypedSelect(eqref()));
            }
            ("Basics", "compare") => {
                // val_compare -> -1/0/1; Order ctor tag = that + 1 (LT/EQ/GT).
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.val_compare_idx));
                f.instruction(&Instruction::I32Const(1));
                f.instruction(&Instruction::I32Add);
                f.instruction(&Instruction::RefNull(HeapType::Concrete(T_ARR)));
                f.instruction(&Instruction::StructNew(T_CTOR));
            }
            ("List", "sort") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::Call(self.list_sort_idx));
            }
            ("List", "repeat") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.list_repeat_idx));
            }
            ("List", "filterMap") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.list_filter_map_idx));
            }
            ("List", "sortBy") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.list_sortby_idx));
            }
            ("List", "sortWith") => {
                self.emit_expr(&args[0], ctx, f)?; // comparator
                self.emit_expr(&args[1], ctx, f)?; // list
                f.instruction(&Instruction::Call(self.list_sort_with_idx));
            }
            ("List", "concatMap") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.list_map_idx));
                f.instruction(&Instruction::Call(self.list_concat_idx));
            }
            ("List", "partition") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.list_partition_idx));
            }
            ("List", "unzip") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::Call(self.list_unzip_idx));
            }
            ("List", "intersperse") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.list_intersperse_idx));
            }
            ("List", "all") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.list_all_idx));
            }
            ("List", "any") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.list_any_idx));
            }
            ("List", "minimum") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::Call(self.list_min_idx));
            }
            ("List", "maximum") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::Call(self.list_max_idx));
            }
            ("List", "indexedMap") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::I64Const(0));
                f.instruction(&Instruction::Call(self.box_int_idx));
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.list_indexed_map_idx));
            }
            ("List", "sum") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::Call(self.list_sum_idx));
            }
            ("List", "product") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::Call(self.list_product_idx));
            }
            ("List", "map2") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                self.emit_expr(&args[2], ctx, f)?;
                f.instruction(&Instruction::Call(self.list_map2_idx));
            }
            ("List", "map3") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                self.emit_expr(&args[2], ctx, f)?;
                self.emit_expr(&args[3], ctx, f)?;
                f.instruction(&Instruction::Call(self.list_map3_idx));
            }
            ("List", "map4") | ("List", "map5") => {
                for a in args {
                    self.emit_expr(a, ctx, f)?;
                }
                let idx = if name == "map4" { self.list_map4_idx } else { self.list_map5_idx };
                f.instruction(&Instruction::Call(idx));
            }
            ("Basics", "add") | ("Basics", "sub") | ("Basics", "mul") => {
                let op = match name {
                    "add" => "+",
                    "sub" => "-",
                    _ => "*",
                };
                self.emit_binop(op, &args[0], &args[1], ctx, f)?;
            }
            ("Basics", "modBy") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.modby_idx));
            }
            ("Basics", "remainderBy") => {
                // remainderBy divisor value = value rem divisor (truncated).
                self.emit_i64(&args[1], ctx, f)?;
                self.emit_i64(&args[0], ctx, f)?;
                f.instruction(&Instruction::I64RemS);
                f.instruction(&Instruction::Call(self.box_int_idx));
            }
            ("Basics", "toFloat") => {
                self.emit_i64(&args[0], ctx, f)?;
                f.instruction(&Instruction::F64ConvertI64S);
                f.instruction(&Instruction::StructNew(T_FLOAT));
            }
            // `round`, following JS `Math.round`: floor(x + 0.5).
            ("Basics", "round") => {
                self.emit_f64(&args[0], ctx, f)?;
                f.instruction(&Instruction::F64Const(0.5.into()));
                f.instruction(&Instruction::F64Add);
                f.instruction(&Instruction::F64Floor);
                f.instruction(&Instruction::I64TruncF64S);
                f.instruction(&Instruction::Call(self.box_int_idx));
            }
            ("Basics", "floor") | ("Basics", "ceiling") | ("Basics", "truncate") => {
                self.emit_f64(&args[0], ctx, f)?;
                match name {
                    "floor" => f.instruction(&Instruction::F64Floor),
                    "ceiling" => f.instruction(&Instruction::F64Ceil),
                    _ => f.instruction(&Instruction::F64Trunc),
                };
                f.instruction(&Instruction::I64TruncF64S);
                f.instruction(&Instruction::Call(self.box_int_idx));
            }
            ("Basics", "sqrt") => {
                self.emit_f64(&args[0], ctx, f)?;
                f.instruction(&Instruction::F64Sqrt);
                f.instruction(&Instruction::StructNew(T_FLOAT));
            }
            // Transcendentals: delegate to the host Math.* (libm), matching JS.
            ("Basics", "sin") | ("Basics", "cos") | ("Basics", "tan")
            | ("Basics", "asin") | ("Basics", "acos") | ("Basics", "atan") => {
                let idx = match name {
                    "sin" => MATH_SIN,
                    "cos" => MATH_COS,
                    "tan" => MATH_TAN,
                    "asin" => MATH_ASIN,
                    "acos" => MATH_ACOS,
                    _ => MATH_ATAN,
                };
                self.emit_f64(&args[0], ctx, f)?;
                f.instruction(&Instruction::Call(idx));
                f.instruction(&Instruction::StructNew(T_FLOAT));
            }
            ("Basics", "atan2") => {
                self.emit_f64(&args[0], ctx, f)?; // y
                self.emit_f64(&args[1], ctx, f)?; // x
                f.instruction(&Instruction::Call(MATH_ATAN2));
                f.instruction(&Instruction::StructNew(T_FLOAT));
            }
            // logBase b x = log x / log b
            ("Basics", "logBase") => {
                self.emit_f64(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(MATH_LOG));
                self.emit_f64(&args[0], ctx, f)?;
                f.instruction(&Instruction::Call(MATH_LOG));
                f.instruction(&Instruction::F64Div);
                f.instruction(&Instruction::StructNew(T_FLOAT));
            }
            // degrees d = d * pi/180 ; turns t = 2*pi*t ; radians r = r
            ("Basics", "degrees") => {
                self.emit_f64(&args[0], ctx, f)?;
                f.instruction(&Instruction::F64Const((std::f64::consts::PI / 180.0).into()));
                f.instruction(&Instruction::F64Mul);
                f.instruction(&Instruction::StructNew(T_FLOAT));
            }
            ("Basics", "turns") => {
                self.emit_f64(&args[0], ctx, f)?;
                f.instruction(&Instruction::F64Const((2.0 * std::f64::consts::PI).into()));
                f.instruction(&Instruction::F64Mul);
                f.instruction(&Instruction::StructNew(T_FLOAT));
            }
            ("Basics", "radians") => {
                self.emit_expr(&args[0], ctx, f)?; // identity (already a Float)
            }
            ("Basics", "fromPolar") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::Call(self.from_polar_idx));
            }
            ("Basics", "toPolar") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::Call(self.to_polar_idx));
            }
            ("Basics", "isNaN") => {
                // NaN is the only value not equal to itself.
                self.emit_f64(&args[0], ctx, f)?;
                self.emit_f64(&args[0], ctx, f)?;
                f.instruction(&Instruction::F64Ne);
                f.instruction(&Instruction::RefI31);
            }
            ("Basics", "isInfinite") => {
                self.emit_f64(&args[0], ctx, f)?;
                f.instruction(&Instruction::F64Abs);
                f.instruction(&Instruction::F64Const(f64::INFINITY.into()));
                f.instruction(&Instruction::F64Eq);
                f.instruction(&Instruction::RefI31);
            }
            // Bitwise: Elm operates on 32-bit ints (JS `| 0` semantics).
            ("Bitwise", "and") | ("Bitwise", "or") | ("Bitwise", "xor") => {
                self.emit_i64(&args[0], ctx, f)?;
                f.instruction(&Instruction::I32WrapI64);
                self.emit_i64(&args[1], ctx, f)?;
                f.instruction(&Instruction::I32WrapI64);
                f.instruction(match name {
                    "and" => &Instruction::I32And,
                    "or" => &Instruction::I32Or,
                    _ => &Instruction::I32Xor,
                });
                f.instruction(&Instruction::I64ExtendI32S);
                f.instruction(&Instruction::Call(self.box_int_idx));
            }
            ("Bitwise", "complement") => {
                self.emit_i64(&args[0], ctx, f)?;
                f.instruction(&Instruction::I32WrapI64);
                f.instruction(&Instruction::I32Const(-1));
                f.instruction(&Instruction::I32Xor);
                f.instruction(&Instruction::I64ExtendI32S);
                f.instruction(&Instruction::Call(self.box_int_idx));
            }
            // shift ops take `shiftLeftBy offset value` → value shifted by offset
            ("Bitwise", "shiftLeftBy") | ("Bitwise", "shiftRightBy") => {
                self.emit_i64(&args[1], ctx, f)?;
                f.instruction(&Instruction::I32WrapI64);
                self.emit_i64(&args[0], ctx, f)?;
                f.instruction(&Instruction::I32WrapI64);
                f.instruction(if name == "shiftLeftBy" {
                    &Instruction::I32Shl
                } else {
                    &Instruction::I32ShrS
                });
                f.instruction(&Instruction::I64ExtendI32S);
                f.instruction(&Instruction::Call(self.box_int_idx));
            }
            ("Bitwise", "shiftRightZfBy") => {
                self.emit_i64(&args[1], ctx, f)?;
                f.instruction(&Instruction::I32WrapI64);
                self.emit_i64(&args[0], ctx, f)?;
                f.instruction(&Instruction::I32WrapI64);
                f.instruction(&Instruction::I32ShrU);
                f.instruction(&Instruction::I64ExtendI32U); // zero-fill → unsigned
                f.instruction(&Instruction::Call(self.box_int_idx));
            }
            ("Basics", "not") => {
                self.emit_bool(&args[0], ctx, f)?;
                f.instruction(&Instruction::I32Eqz);
                f.instruction(&Instruction::RefI31);
            }
            ("Basics", "identity") => self.emit_expr(&args[0], ctx, f)?,
            ("Basics", "always") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Drop);
            }
            ("Char", "toCode") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::RefCastNonNull(HeapType::Abstract {
                    shared: false,
                    ty: AbstractHeapType::I31,
                }));
                f.instruction(&Instruction::I31GetS);
                f.instruction(&Instruction::I64ExtendI32S);
                f.instruction(&Instruction::Call(self.box_int_idx));
            }
            ("Char", "fromCode") => {
                self.emit_i64(&args[0], ctx, f)?;
                f.instruction(&Instruction::I32WrapI64);
                f.instruction(&Instruction::RefI31);
            }
            ("String", "join") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.str_join_idx));
            }
            ("String", "repeat") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.str_repeat_idx));
            }
            ("String", "startsWith") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.str_starts_with_idx));
            }
            ("String", "endsWith") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.str_ends_with_idx));
            }
            ("String", "isEmpty") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&cast_to(T_STR));
                f.instruction(&Instruction::ArrayLen);
                f.instruction(&Instruction::I32Eqz);
                f.instruction(&Instruction::RefI31);
            }
            ("String", "toUpper") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::Call(self.str_upper_idx));
            }
            ("String", "toLower") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::Call(self.str_lower_idx));
            }
            ("String", "trim") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::Call(self.str_trim_idx));
            }
            ("String", "left") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.str_left_idx));
            }
            ("String", "right") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.str_right_idx));
            }
            ("String", "dropLeft") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.str_dropleft_idx));
            }
            ("String", "dropRight") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.str_dropright_idx));
            }
            ("String", "toInt") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::Call(self.str_to_int_idx));
            }
            ("String", "contains") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.str_contains_idx));
            }
            ("String", "split") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.str_split_idx));
            }
            // replace before after s = join after (split before s)
            ("String", "replace") => {
                self.emit_expr(&args[1], ctx, f)?; // after (join sep)
                self.emit_expr(&args[0], ctx, f)?; // before (split sep)
                self.emit_expr(&args[2], ctx, f)?; // s
                f.instruction(&Instruction::Call(self.str_split_idx));
                f.instruction(&Instruction::Call(self.str_join_idx));
            }
            ("String", "slice") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                self.emit_expr(&args[2], ctx, f)?;
                f.instruction(&Instruction::Call(self.str_slice_idx));
            }
            ("String", "pad") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                self.emit_expr(&args[2], ctx, f)?;
                f.instruction(&Instruction::Call(self.str_pad_both_idx));
            }
            ("String", "padLeft") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                self.emit_expr(&args[2], ctx, f)?;
                f.instruction(&Instruction::Call(self.str_pad_left_idx));
            }
            ("String", "padRight") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                self.emit_expr(&args[2], ctx, f)?;
                f.instruction(&Instruction::Call(self.str_pad_right_idx));
            }
            ("String", "words") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::Call(self.str_words_idx));
            }
            ("String", "lines") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::Call(self.str_lines_idx));
            }
            ("String", "toList") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::Call(self.str_to_list_idx));
            }
            ("String", "fromList") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::Call(self.str_from_list_idx));
            }
            ("String", "fromChar") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::Call(self.str_from_char_idx));
            }
            ("String", "cons") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::Call(self.str_from_char_idx));
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.str_append_idx));
            }
            ("String", "uncons") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::Call(self.str_uncons_idx));
            }
            // code-point-correct string transforms via toList/list-op/fromList
            ("String", "reverse") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::Call(self.str_to_list_idx));
                f.instruction(&Instruction::Call(self.list_reverse_idx));
                f.instruction(&Instruction::Call(self.str_from_list_idx));
            }
            ("String", "map") => {
                self.emit_expr(&args[0], ctx, f)?; // f
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.str_to_list_idx));
                f.instruction(&Instruction::Call(self.list_map_idx));
                f.instruction(&Instruction::Call(self.str_from_list_idx));
            }
            ("String", "filter") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.str_to_list_idx));
                f.instruction(&Instruction::Call(self.list_filter_idx));
                f.instruction(&Instruction::Call(self.str_from_list_idx));
            }
            ("String", "foldl") => {
                self.emit_expr(&args[0], ctx, f)?; // f
                self.emit_expr(&args[1], ctx, f)?; // acc
                self.emit_expr(&args[2], ctx, f)?;
                f.instruction(&Instruction::Call(self.str_to_list_idx));
                f.instruction(&Instruction::Call(self.list_foldl_idx));
            }
            ("String", "foldr") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                self.emit_expr(&args[2], ctx, f)?;
                f.instruction(&Instruction::Call(self.str_to_list_idx));
                f.instruction(&Instruction::Call(self.list_foldr_idx));
            }
            ("String", "any") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.str_to_list_idx));
                f.instruction(&Instruction::Call(self.list_any_idx));
            }
            ("String", "all") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.str_to_list_idx));
                f.instruction(&Instruction::Call(self.list_all_idx));
            }
            ("List", "isEmpty") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&cast_to(T_LIST));
                f.instruction(&Instruction::StructGet { struct_type_index: T_LIST, field_index: 0 });
                f.instruction(&Instruction::I32Eqz);
                f.instruction(&Instruction::RefI31);
            }
            // Html static vdom (VTEXT tag0 [str]; VNODE tag1 [tag, attrs, kids]).
            ("Html", "text") => {
                f.instruction(&Instruction::I32Const(0));
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 1 });
                f.instruction(&Instruction::StructNew(T_CTOR));
            }
            ("Html", "node") => {
                f.instruction(&Instruction::I32Const(1));
                self.emit_expr(&args[0], ctx, f)?; // tag name
                self.emit_expr(&args[1], ctx, f)?; // attrs
                self.emit_expr(&args[2], ctx, f)?; // kids
                f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 3 });
                f.instruction(&Instruction::StructNew(T_CTOR));
            }
            ("Html", "map") => {
                self.emit_expr(&args[0], ctx, f)?; // f
                self.emit_expr(&args[1], ctx, f)?; // node
                f.instruction(&Instruction::Call(self.html_map_idx));
            }
            // Html.Lazy.lazyN f a…: no memoization — just apply f to the args.
            ("Html.Lazy", n) if n.starts_with("lazy") => {
                self.emit_expr(&args[0], ctx, f)?; // f
                for a in &args[1..] {
                    self.emit_expr(a, ctx, f)?;
                    f.instruction(&Instruction::Call(self.apply1_idx));
                }
            }
            // Html.Keyed.node tag attrs keyed → VKEYED[tag, attrs, keyed] (tag 2).
            // Keys are preserved (not stripped) so `patch` can reconcile by key.
            ("Html.Keyed", "node") => {
                f.instruction(&Instruction::I32Const(2)); // VKEYED
                self.emit_expr(&args[0], ctx, f)?; // tag
                self.emit_expr(&args[1], ctx, f)?; // attrs
                self.emit_expr(&args[2], ctx, f)?; // keyed children (key, node) pairs
                f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 3 });
                f.instruction(&Instruction::StructNew(T_CTOR));
            }
            ("Html.Keyed", tag) => {
                // Keyed.ul/ol attrs keyed → VKEYED[<tag>, attrs, keyed].
                f.instruction(&Instruction::I32Const(2));
                push_str_const(f, tag);
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 3 });
                f.instruction(&Instruction::StructNew(T_CTOR));
            }
            ("Html", tag) => {
                // element helper: Html.<tag> attrs kids
                f.instruction(&Instruction::I32Const(1));
                push_str_const(f, tag);
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 3 });
                f.instruction(&Instruction::StructNew(T_CTOR));
            }
            ("Html.Attributes", "attribute") => {
                f.instruction(&Instruction::I32Const(0)); // AATTR
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 2 });
                f.instruction(&Instruction::StructNew(T_CTOR));
            }
            ("Html.Attributes", "style") => {
                f.instruction(&Instruction::I32Const(1)); // ASTYLE
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 2 });
                f.instruction(&Instruction::StructNew(T_CTOR));
            }
            // Common string attributes: Html.Attributes.<name> value → AATTR.
            ("Html.Attributes", a) if html_attr_name(a).is_some() => {
                f.instruction(&Instruction::I32Const(0)); // AATTR
                push_str_const(f, html_attr_name(a).unwrap());
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 2 });
                f.instruction(&Instruction::StructNew(T_CTOR));
            }
            ("Browser", "sandbox") => {
                // Program = T_CTOR tag0 [record].
                f.instruction(&Instruction::I32Const(0));
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 1 });
                f.instruction(&Instruction::StructNew(T_CTOR));
            }
            ("Browser", "element") => {
                // Program = T_CTOR tag1 [record].
                f.instruction(&Instruction::I32Const(1));
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 1 });
                f.instruction(&Instruction::StructNew(T_CTOR));
            }
            ("Browser", "document") => {
                // Program = T_CTOR tag2 [record].
                f.instruction(&Instruction::I32Const(2));
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 1 });
                f.instruction(&Instruction::StructNew(T_CTOR));
            }
            ("Browser", "application") => {
                // Program = T_CTOR tag3 [record].
                f.instruction(&Instruction::I32Const(3));
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 1 });
                f.instruction(&Instruction::StructNew(T_CTOR));
            }
            // Http.get { url, expect } → CMD_HTTP tag3 [url, expect].
            // Record fields sorted: expect=0, url=1.
            ("Http", "get") => {
                let r = ctx.bind("$httpcfg");
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::LocalSet(r));
                f.instruction(&Instruction::I32Const(3));
                f.instruction(&Instruction::LocalGet(r));
                f.instruction(&cast_to(T_ARR));
                f.instruction(&Instruction::I32Const(1));
                f.instruction(&Instruction::ArrayGet(T_ARR)); // url
                f.instruction(&Instruction::LocalGet(r));
                f.instruction(&cast_to(T_ARR));
                f.instruction(&Instruction::I32Const(0));
                f.instruction(&Instruction::ArrayGet(T_ARR)); // expect
                f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 2 });
                f.instruction(&Instruction::StructNew(T_CTOR));
            }
            // Http.Expect: expectString tag0 [toMsg], expectJson tag1 [toMsg, decoder].
            ("Http", "expectString") => {
                f.instruction(&Instruction::I32Const(0));
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 1 });
                f.instruction(&Instruction::StructNew(T_CTOR));
            }
            ("Http", "expectJson") => {
                f.instruction(&Instruction::I32Const(1));
                self.emit_expr(&args[0], ctx, f)?; // toMsg
                self.emit_expr(&args[1], ctx, f)?; // decoder
                f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 2 });
                f.instruction(&Instruction::StructNew(T_CTOR));
            }
            ("Http", "expectWhatever") => {
                f.instruction(&Instruction::I32Const(2)); // EXPECT_WHATEVER
                self.emit_expr(&args[0], ctx, f)?; // toMsg
                f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 1 });
                f.instruction(&Instruction::StructNew(T_CTOR));
            }
            // Browser.Events document listeners → SubDom tag5 [eventName, decoder].
            ("Browser.Events", ev) if browser_event_name(ev).is_some() => {
                let name = browser_event_name(ev).unwrap();
                f.instruction(&Instruction::I32Const(5));
                push_str_const(f, name);
                self.emit_expr(&args[0], ctx, f)?; // decoder
                f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 2 });
                f.instruction(&Instruction::StructNew(T_CTOR));
            }
            ("Url", "fromString") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::Call(self.url_from_string_idx));
            }
            // Browser.Navigation → CMD tag 4 push / 5 replace / 6 load [url].
            ("Browser.Navigation", "pushUrl") | ("Browser.Navigation", "replaceUrl") => {
                let tag = if name == "pushUrl" { 4 } else { 5 };
                f.instruction(&Instruction::I32Const(tag));
                self.emit_expr(&args[1], ctx, f)?; // url (args[0] is the Key)
                f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 1 });
                f.instruction(&Instruction::StructNew(T_CTOR));
            }
            ("Browser.Navigation", "load") => {
                f.instruction(&Instruction::I32Const(6));
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 1 });
                f.instruction(&Instruction::StructNew(T_CTOR));
            }
            // onAnimationFrame(Delta) → SubAnimation tag6 [toMsg, deltaFlag].
            ("Browser.Events", "onAnimationFrameDelta")
            | ("Browser.Events", "onAnimationFrame") => {
                let is_delta = name == "onAnimationFrameDelta";
                f.instruction(&Instruction::I32Const(6));
                self.emit_expr(&args[0], ctx, f)?; // toMsg
                f.instruction(&Instruction::I32Const(is_delta as i32));
                f.instruction(&Instruction::RefI31); // deltaFlag Bool
                f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 2 });
                f.instruction(&Instruction::StructNew(T_CTOR));
            }
            // Time.every interval toMsg → SubTime tag4 [interval, toMsg].
            ("Time", "every") => {
                f.instruction(&Instruction::I32Const(4));
                self.emit_expr(&args[0], ctx, f)?; // interval (Float)
                self.emit_expr(&args[1], ctx, f)?; // toMsg
                f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 2 });
                f.instruction(&Instruction::StructNew(T_CTOR));
            }
            // Posix is opaque: T_CTOR tag0 [Int millis].
            ("Time", "millisToPosix") => {
                f.instruction(&Instruction::I32Const(0));
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 1 });
                f.instruction(&Instruction::StructNew(T_CTOR));
            }
            ("Time", "posixToMillis") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&cast_to(T_CTOR));
                f.instruction(&Instruction::StructGet { struct_type_index: T_CTOR, field_index: 1 });
                f.instruction(&Instruction::I32Const(0));
                f.instruction(&Instruction::ArrayGet(T_ARR));
            }
            // Platform.Cmd.batch / Platform.Sub.batch → tag1 [List].
            ("Platform.Cmd", "batch") | ("Platform.Sub", "batch") => {
                f.instruction(&Instruction::I32Const(1));
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 1 });
                f.instruction(&Instruction::StructNew(T_CTOR));
            }
            // Html.Events: plain-message handlers → AEVENT [name, succeed msg].
            ("Html.Events", ev) if html_event_name(ev).is_some() => {
                let name = html_event_name(ev).unwrap();
                f.instruction(&Instruction::I32Const(2)); // AEVENT
                push_str_const(f, name);
                f.instruction(&Instruction::I32Const(20)); // DSucceed
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 1 });
                f.instruction(&Instruction::StructNew(T_CTOR));
                f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 2 });
                f.instruction(&Instruction::StructNew(T_CTOR));
            }
            ("Html.Events", "onInput") => {
                // onInput toMsg = on "input" (map toMsg (at ["target","value"] string)).
                // AEVENT ["input", DMap[toMsg, DField "target" (DField "value" DString)]].
                f.instruction(&Instruction::I32Const(2)); // AEVENT
                push_str_const(f, "input");
                f.instruction(&Instruction::I32Const(15)); // DMap
                self.emit_expr(&args[0], ctx, f)?; // toMsg
                f.instruction(&Instruction::I32Const(8)); // DField
                push_str_const(f, "target");
                f.instruction(&Instruction::I32Const(8)); // DField
                push_str_const(f, "value");
                f.instruction(&Instruction::I32Const(0)); // DString
                f.instruction(&Instruction::RefNull(HeapType::Concrete(T_ARR)));
                f.instruction(&Instruction::StructNew(T_CTOR));
                f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 2 });
                f.instruction(&Instruction::StructNew(T_CTOR)); // inner DField "value"
                f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 2 });
                f.instruction(&Instruction::StructNew(T_CTOR)); // outer DField "target"
                f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 2 });
                f.instruction(&Instruction::StructNew(T_CTOR)); // DMap
                f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 2 });
                f.instruction(&Instruction::StructNew(T_CTOR)); // AEVENT
            }
            ("Html.Events", "on") => {
                // on name decoder → AEVENT [name, decoder]
                f.instruction(&Instruction::I32Const(2));
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::ArrayNewFixed { array_type_index: T_ARR, array_size: 2 });
                f.instruction(&Instruction::StructNew(T_CTOR));
            }
            _ => return Err(format!("wasmgc: unsupported kernel `{module}.{name}`")),
        }
        Ok(())
    }

    /// Build a custom-type value: `struct T_CTOR { tag, args-array }`.
    fn emit_ctor(
        &mut self,
        tag: u32,
        args: &[TypedExpr],
        ctx: &mut FnCtx,
        f: &mut Function,
    ) -> Result<(), String> {
        f.instruction(&Instruction::I32Const(tag as i32));
        if args.is_empty() {
            f.instruction(&Instruction::RefNull(HeapType::Concrete(T_ARR)));
        } else {
            for a in args {
                self.emit_expr(a, ctx, f)?;
            }
            f.instruction(&Instruction::ArrayNewFixed {
                array_type_index: T_ARR,
                array_size: args.len() as u32,
            });
        }
        f.instruction(&Instruction::StructNew(T_CTOR));
        Ok(())
    }

    fn emit_case(
        &mut self,
        scrut: &TypedExpr,
        branches: &[(can::Pattern, TypedExpr)],
        ctx: &mut FnCtx,
        f: &mut Function,
    ) -> Result<(), String> {
        let s = ctx.bind("$scrut");
        self.emit_expr(scrut, ctx, f)?;
        f.instruction(&Instruction::LocalSet(s));
        self.emit_branches(branches, s, ctx, f)
    }

    fn emit_branches(
        &mut self,
        branches: &[(can::Pattern, TypedExpr)],
        s: u32,
        ctx: &mut FnCtx,
        f: &mut Function,
    ) -> Result<(), String> {
        match branches.split_first() {
            None => {
                // The exhaustiveness checker guarantees a match; guard anyway.
                f.instruction(&Instruction::Unreachable);
                Ok(())
            }
            Some(((pat, body), rest)) => {
                self.emit_test(pat, s, ctx, f)?;
                f.instruction(&Instruction::If(BlockType::Result(eqref())));
                let mark = ctx.scope.len();
                self.bind_pat(pat, s, ctx, f)?;
                self.emit_expr(body, ctx, f)?;
                ctx.scope.truncate(mark);
                f.instruction(&Instruction::Else);
                self.emit_branches(rest, s, ctx, f)?;
                f.instruction(&Instruction::End);
                Ok(())
            }
        }
    }

    /// Push an i32 (1 = matches) testing `pat` against the value in local `s`.
    fn emit_test(
        &mut self,
        pat: &can::Pattern,
        s: u32,
        ctx: &mut FnCtx,
        f: &mut Function,
    ) -> Result<(), String> {
        use can::Pattern_::*;
        match &pat.value {
            Anything | Var(_) | Record(_) => {
                f.instruction(&Instruction::I32Const(1));
            }
            Alias(inner, _) => self.emit_test(inner, s, ctx, f)?,
            Int(n) => {
                f.instruction(&Instruction::LocalGet(s));
                f.instruction(&Instruction::Call(self.unbox_int_idx));
                f.instruction(&Instruction::I64Const(*n));
                f.instruction(&Instruction::I64Eq);
            }
            Chr(c) => {
                f.instruction(&Instruction::LocalGet(s));
                f.instruction(&Instruction::RefCastNonNull(HeapType::Abstract {
                    shared: false,
                    ty: AbstractHeapType::I31,
                }));
                f.instruction(&Instruction::I31GetS);
                f.instruction(&Instruction::I32Const(*c as i32));
                f.instruction(&Instruction::I32Eq);
            }
            Ctor(_, _, ctor, args) if ctor.name.as_str() == "True" => {
                self.load_i31(s, f);
            }
            Ctor(_, _, ctor, args) if ctor.name.as_str() == "False" => {
                self.load_i31(s, f);
                f.instruction(&Instruction::I32Eqz);
                let _ = args;
            }
            Ctor(_, _, ctor, args) => {
                // Guard on the tag, THEN test args — so the arg-extraction casts
                // only run when the constructor matches (avoids illegal casts).
                f.instruction(&Instruction::LocalGet(s));
                f.instruction(&cast_to(T_CTOR));
                f.instruction(&Instruction::StructGet { struct_type_index: T_CTOR, field_index: 0 });
                f.instruction(&Instruction::I32Const(ctor.index as i32));
                f.instruction(&Instruction::I32Eq);
                f.instruction(&Instruction::If(BlockType::Result(ValType::I32)));
                f.instruction(&Instruction::I32Const(1));
                for (i, ap) in args.iter().enumerate() {
                    if non_trivial(ap) {
                        let sub = ctx.bind("$a");
                        self.load_ctor_arg(s, i as u32, f);
                        f.instruction(&Instruction::LocalSet(sub));
                        self.emit_test(ap, sub, ctx, f)?;
                        f.instruction(&Instruction::I32And);
                    }
                }
                f.instruction(&Instruction::Else);
                f.instruction(&Instruction::I32Const(0));
                f.instruction(&Instruction::End);
            }
            List(items) if items.is_empty() => {
                list_is_empty(f, s);
            }
            List(items) => {
                // non-empty AND head matches items[0] AND tail matches List(rest)
                self.emit_cons_test(&items[0], &make_list_tail(pat, items), s, ctx, f)?;
            }
            Cons(h, t) => self.emit_cons_test(h, t, s, ctx, f)?,
            Tuple(a, b, rest) => {
                f.instruction(&Instruction::I32Const(1));
                let mut elems: Vec<&can::Pattern> = vec![a, b];
                elems.extend(rest.iter());
                for (i, ep) in elems.iter().enumerate() {
                    if non_trivial(ep) {
                        let sub = ctx.bind("$t");
                        self.load_arr(s, i as u32, f);
                        f.instruction(&Instruction::LocalSet(sub));
                        self.emit_test(ep, sub, ctx, f)?;
                        f.instruction(&Instruction::I32And);
                    }
                }
            }
            other => return Err(format!("wasmgc: unsupported pattern in test: {other:?}")),
        }
        Ok(())
    }

    /// Push i32: non-null AND head matches `h` AND tail matches `t`.
    fn emit_cons_test(
        &mut self,
        h: &can::Pattern,
        t: &can::Pattern,
        s: u32,
        ctx: &mut FnCtx,
        f: &mut Function,
    ) -> Result<(), String> {
        // Guard on non-empty, THEN test head/tail (head/tail are only valid on
        // a non-empty vector).
        list_is_empty(f, s);
        f.instruction(&Instruction::I32Eqz); // non-empty
        f.instruction(&Instruction::If(BlockType::Result(ValType::I32)));
        f.instruction(&Instruction::I32Const(1));
        if non_trivial(h) {
            let sub = ctx.bind("$h");
            self.load_cons(s, 0, f);
            f.instruction(&Instruction::LocalSet(sub));
            self.emit_test(h, sub, ctx, f)?;
            f.instruction(&Instruction::I32And);
        }
        if non_trivial(t) {
            let sub = ctx.bind("$tl");
            self.load_cons(s, 1, f);
            f.instruction(&Instruction::LocalSet(sub));
            self.emit_test(t, sub, ctx, f)?;
            f.instruction(&Instruction::I32And);
        }
        f.instruction(&Instruction::Else);
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::End);
        Ok(())
    }

    /// Bind a pattern's variables, given the matched value in local `s`.
    fn bind_pat(
        &mut self,
        pat: &can::Pattern,
        s: u32,
        ctx: &mut FnCtx,
        f: &mut Function,
    ) -> Result<(), String> {
        use can::Pattern_::*;
        match &pat.value {
            Anything | Int(_) | Chr(_) | Str(_) => {}
            Var(name) => ctx.scope.push((name.to_string(), s)),
            Alias(inner, name) => {
                ctx.scope.push((name.value.to_string(), s));
                self.bind_pat(inner, s, ctx, f)?;
            }
            Ctor(_, _, ctor, args) if ctor.name.as_str() == "True" || ctor.name.as_str() == "False" => {
                let _ = (ctor, args);
            }
            Ctor(_, _, _, args) => {
                for (i, ap) in args.iter().enumerate() {
                    let sub = ctx.bind("$ba");
                    self.load_ctor_arg(s, i as u32, f);
                    f.instruction(&Instruction::LocalSet(sub));
                    self.bind_pat(ap, sub, ctx, f)?;
                }
            }
            Cons(h, t) => {
                let hs = ctx.bind("$bh");
                self.load_cons(s, 0, f);
                f.instruction(&Instruction::LocalSet(hs));
                self.bind_pat(h, hs, ctx, f)?;
                let ts = ctx.bind("$bt");
                self.load_cons(s, 1, f);
                f.instruction(&Instruction::LocalSet(ts));
                self.bind_pat(t, ts, ctx, f)?;
            }
            List(items) if items.is_empty() => {}
            List(items) => {
                let hs = ctx.bind("$bh");
                self.load_cons(s, 0, f);
                f.instruction(&Instruction::LocalSet(hs));
                self.bind_pat(&items[0], hs, ctx, f)?;
                let ts = ctx.bind("$bt");
                self.load_cons(s, 1, f);
                f.instruction(&Instruction::LocalSet(ts));
                self.bind_pat(&make_list_tail(pat, items), ts, ctx, f)?;
            }
            Tuple(a, b, rest) => {
                let mut elems: Vec<&can::Pattern> = vec![a, b];
                elems.extend(rest.iter());
                for (i, ep) in elems.iter().enumerate() {
                    let sub = ctx.bind("$bt");
                    self.load_arr(s, i as u32, f);
                    f.instruction(&Instruction::LocalSet(sub));
                    self.bind_pat(ep, sub, ctx, f)?;
                }
            }
            other => return Err(format!("wasmgc: unsupported pattern in bind: {other:?}")),
        }
        Ok(())
    }

    fn load_i31(&self, s: u32, f: &mut Function) {
        f.instruction(&Instruction::LocalGet(s));
        f.instruction(&Instruction::RefCastNonNull(HeapType::Abstract {
            shared: false,
            ty: AbstractHeapType::I31,
        }));
        f.instruction(&Instruction::I31GetS);
    }
    fn load_ctor_arg(&self, s: u32, i: u32, f: &mut Function) {
        f.instruction(&Instruction::LocalGet(s));
        f.instruction(&cast_to(T_CTOR));
        f.instruction(&Instruction::StructGet { struct_type_index: T_CTOR, field_index: 1 });
        f.instruction(&Instruction::I32Const(i as i32));
        f.instruction(&Instruction::ArrayGet(T_ARR));
    }
    /// Load a list's head (field 0) or tail (field 1) for pattern matching.
    fn load_cons(&self, s: u32, field: u32, f: &mut Function) {
        if field == 0 {
            list_head(f, s);
        } else {
            list_tail(f, s);
        }
    }
    fn load_arr(&self, s: u32, i: u32, f: &mut Function) {
        f.instruction(&Instruction::LocalGet(s));
        f.instruction(&cast_to(T_ARR));
        f.instruction(&Instruction::I32Const(i as i32));
        f.instruction(&Instruction::ArrayGet(T_ARR));
    }
}

struct FnCtx {
    /// Scope stack of (name, local index); shadowing = last match wins.
    scope: Vec<(String, u32)>,
    next_local: u32,
    /// Reserved scratch locals for inlining box_int/unbox_int on the arithmetic
    /// hot path (avoids a function call per operation). u32::MAX = not reserved
    /// (fall back to the out-of-line helper).
    scratch_eqref: u32,
    scratch_i64: u32,
}

impl FnCtx {
    fn new() -> Self {
        FnCtx { scope: Vec::new(), next_local: 0, scratch_eqref: u32::MAX, scratch_i64: u32::MAX }
    }
    fn lookup(&self, name: &str) -> Option<u32> {
        self.scope.iter().rev().find(|(n, _)| n == name).map(|(_, i)| *i)
    }
    fn bind(&mut self, name: &str) -> u32 {
        let slot = self.next_local;
        self.next_local += 1;
        self.scope.push((name.to_string(), slot));
        slot
    }
}

/// Over-approximate count of `let`/`case`/destructure-bound locals in a body,
/// to size the function's local declarations (slots are never reused, so the
/// count is the total of all bindings introduced anywhere in the tree).
fn count_bindings(e: &TypedExpr) -> u32 {
    use TypedKind::*;
    match &e.kind {
        Int(_) | Float(_) | Str(_) | Chr(_) | Unit | Local(_) | Global(_) | Foreign(_, _)
        | Accessor(_) => 0,
        Negate(x) | Access(x, _) => count_bindings(x),
        Binop(_, _, _, l, r) => count_bindings(l) + count_bindings(r),
        Ctor(_, _, _) => 0,
        List(xs) => 1 + xs.iter().map(count_bindings).sum::<u32>(),
        Call(g, args) => count_bindings(g) + args.iter().map(count_bindings).sum::<u32>(),
        If(bs, otherwise) => {
            bs.iter().map(|(c, t)| count_bindings(c) + count_bindings(t)).sum::<u32>()
                + count_bindings(otherwise)
        }
        Lambda(ps, b) => ps.len() as u32 + count_bindings(b),
        Let(decls, body) => {
            decls
                .iter()
                .map(|d| match d {
                    TypedLetDecl::Def { params, body, .. } => {
                        1 + params.len() as u32 + count_bindings(body)
                    }
                    TypedLetDecl::Destruct(p, ex) => 1 + pat_size(p) + count_bindings(ex),
                    TypedLetDecl::Recursive(ds) => ds
                        .iter()
                        .map(|d| match d {
                            TypedLetDecl::Def { params, body, .. } => {
                                1 + params.len() as u32 + count_bindings(body)
                            }
                            TypedLetDecl::Destruct(p, ex) => 1 + pat_size(p) + count_bindings(ex),
                            TypedLetDecl::Recursive(_) => 0,
                        })
                        .sum(),
                })
                .sum::<u32>()
                + count_bindings(body)
        }
        Case(scrut, branches) => {
            // 1 for the scrutinee local, then per branch reserve enough for the
            // bound vars AND the extraction temporaries used by test/bind
            // (`pat_size` over-approximates both).
            1 + count_bindings(scrut)
                + branches
                    .iter()
                    .map(|(p, b)| 2 * pat_size(p) + count_bindings(b))
                    .sum::<u32>()
        }
        Update(x, fields) => {
            count_bindings(x) + fields.iter().map(|(_, e)| count_bindings(e)).sum::<u32>()
        }
        Record(fields) => fields.iter().map(|(_, e)| count_bindings(e)).sum(),
        Tuple(a, b, c) => {
            count_bindings(a)
                + count_bindings(b)
                + c.as_ref().map(|x| count_bindings(x)).unwrap_or(0)
        }
    }
}

/// Names a pattern binds.
fn pat_names(p: &can::Pattern) -> Vec<String> {
    use can::Pattern_::*;
    match &p.value {
        Var(n) => vec![n.to_string()],
        Alias(inner, n) => {
            let mut v = pat_names(inner);
            v.push(n.value.to_string());
            v
        }
        Tuple(a, b, rest) => {
            let mut v = pat_names(a);
            v.extend(pat_names(b));
            for r in rest {
                v.extend(pat_names(r));
            }
            v
        }
        Ctor(_, _, _, args) => args.iter().flat_map(pat_names).collect(),
        List(items) => items.iter().flat_map(pat_names).collect(),
        Cons(h, t) => {
            let mut v = pat_names(h);
            v.extend(pat_names(t));
            v
        }
        Record(fields) => fields.iter().map(|n| n.value.to_string()).collect(),
        _ => Vec::new(),
    }
}

/// Collect the free local variable names of `e` (those referenced but not bound
/// within `e`, given the already-`bound` names), preserving first-seen order.
fn free_locals(e: &TypedExpr, bound: &std::collections::HashSet<String>, out: &mut Vec<String>) {
    use TypedKind::*;
    match &e.kind {
        Local(n) => {
            if !bound.contains(n.as_str()) && !out.iter().any(|x| x == n.as_str()) {
                out.push(n.to_string());
            }
        }
        Int(_) | Float(_) | Str(_) | Chr(_) | Unit | Global(_) | Foreign(_, _) | Accessor(_)
        | Ctor(_, _, _) => {}
        Negate(x) | Access(x, _) => free_locals(x, bound, out),
        Binop(_, _, _, l, r) => {
            free_locals(l, bound, out);
            free_locals(r, bound, out);
        }
        List(xs) => xs.iter().for_each(|x| free_locals(x, bound, out)),
        Call(g, args) => {
            free_locals(g, bound, out);
            args.iter().for_each(|a| free_locals(a, bound, out));
        }
        If(bs, otherwise) => {
            for (c, t) in bs {
                free_locals(c, bound, out);
                free_locals(t, bound, out);
            }
            free_locals(otherwise, bound, out);
        }
        Lambda(ps, b) => {
            let mut b2 = bound.clone();
            for (p, _) in ps {
                for n in pat_names(p) {
                    b2.insert(n);
                }
            }
            free_locals(b, &b2, out);
        }
        Let(decls, body) => {
            let mut b2 = bound.clone();
            for d in decls {
                collect_let_names(d, &mut b2);
            }
            for d in decls {
                free_locals_let(d, &b2, out);
            }
            free_locals(body, &b2, out);
        }
        Case(scrut, branches) => {
            free_locals(scrut, bound, out);
            for (p, b) in branches {
                let mut b2 = bound.clone();
                for n in pat_names(p) {
                    b2.insert(n);
                }
                free_locals(b, &b2, out);
            }
        }
        Update(x, fields) => {
            free_locals(x, bound, out);
            fields.iter().for_each(|(_, e)| free_locals(e, bound, out));
        }
        Record(fields) => fields.iter().for_each(|(_, e)| free_locals(e, bound, out)),
        Tuple(a, b, c) => {
            free_locals(a, bound, out);
            free_locals(b, bound, out);
            if let Some(c) = c {
                free_locals(c, bound, out);
            }
        }
    }
}

fn collect_let_names(d: &TypedLetDecl, bound: &mut std::collections::HashSet<String>) {
    match d {
        TypedLetDecl::Def { name, .. } => {
            bound.insert(name.to_string());
        }
        TypedLetDecl::Destruct(p, _) => {
            for n in pat_names(p) {
                bound.insert(n);
            }
        }
        TypedLetDecl::Recursive(ds) => ds.iter().for_each(|d| collect_let_names(d, bound)),
    }
}

fn free_locals_let(d: &TypedLetDecl, bound: &std::collections::HashSet<String>, out: &mut Vec<String>) {
    match d {
        TypedLetDecl::Def { params, body, .. } => {
            let mut b2 = bound.clone();
            for (p, _) in params {
                for n in pat_names(p) {
                    b2.insert(n);
                }
            }
            free_locals(body, &b2, out);
        }
        TypedLetDecl::Destruct(_, e) => free_locals(e, bound, out),
        TypedLetDecl::Recursive(ds) => ds.iter().for_each(|d| free_locals_let(d, bound, out)),
    }
}

/// A pattern that needs a runtime test (i.e. not a catch-all binding).
fn non_trivial(p: &can::Pattern) -> bool {
    !matches!(p.value, can::Pattern_::Anything | can::Pattern_::Var(_))
}

/// The tail-of-list pattern `[items[1..]]`, for desugaring a fixed-length list
/// pattern into nested cons matches.
fn make_list_tail(orig: &can::Pattern, items: &[can::Pattern]) -> can::Pattern {
    can::Pattern {
        region: orig.region,
        value: can::Pattern_::List(items[1..].to_vec()),
    }
}

/// Total nodes in a pattern — an over-approximation of the locals its match
/// needs (bound vars plus extraction temporaries).
fn pat_size(p: &can::Pattern) -> u32 {
    use can::Pattern_::*;
    1 + match &p.value {
        Alias(inner, _) => pat_size(inner),
        Tuple(a, b, rest) => {
            pat_size(a) + pat_size(b) + rest.iter().map(pat_size).sum::<u32>()
        }
        Ctor(_, _, _, args) => args.iter().map(pat_size).sum(),
        List(items) => items.iter().map(pat_size).sum(),
        Cons(h, t) => pat_size(h) + pat_size(t),
        Record(fields) => fields.len() as u32,
        _ => 0,
    }
}
