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
    ElementSection, Elements, ExportKind, ExportSection, FieldType, Function, FunctionSection,
    HeapType, Instruction, MemArg, MemorySection, MemoryType, Module, RefType, StorageType,
    TypeSection, ValType,
};

use crate::ast::canonical as can;
use crate::ir::mono::{MonoProgram, TypedExpr, TypedKind, TypedLetDecl};
use crate::reporting::annotation::Region;

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
const N_FIXED: u32 = 8;
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
    f.instruction(&Instruction::LocalGet(local));
    f.instruction(&cast_to(T_CTOR));
    f.instruction(&Instruction::StructGet { struct_type_index: T_CTOR, field_index: 1 });
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::ArrayGet(T_ARR));
}

/// Emit `local += n` for an i32 local.
fn bump(f: &mut Function, local: u32, n: i32) {
    f.instruction(&Instruction::LocalGet(local));
    f.instruction(&Instruction::I32Const(n));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalSet(local));
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

pub fn build(mono: &MonoProgram, output: &Path) -> Result<(), String> {
    let mut cg = Codegen::new(mono);
    let bytes = cg.build()?;
    std::fs::write(output, bytes).map_err(|e| e.to_string())
}

struct Codegen<'a> {
    mono: &'a MonoProgram,
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
    list_intersperse_idx: u32,
    list_map3_idx: u32,
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
            list_intersperse_idx: 0,
            list_map3_idx: 0,
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
            self.func_index.insert(f.mangled.to_string(), i as u32);
            self.func_arity.insert(f.mangled.to_string(), f.params.len() as u32);
        }
        let n = self.mono.functions.len() as u32;
        // Synthesized helper function indices, appended after the user funcs
        // (a running counter, so adding a helper needs no manual re-indexing).
        let mut s = n;
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
        self.str_split_idx = next();
        self.str_words_idx = next();
        self.str_lines_idx = next();
        self.list_repeat_idx = next();
        self.list_filter_map_idx = next();
        self.list_sortby_idx = next();
        self.list_sortby_insert_idx = next();
        self.str_pad_left_idx = next();
        self.str_pad_right_idx = next();
        self.list_intersperse_idx = next();
        self.list_map3_idx = next();
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
        let main_int_idx = next();
        let render_idx = next();
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
        // Ensure a fn-type exists for every arity the apply-dispatcher handles.
        for a in 1..=MAX_ARITY {
            self.fn_type(a);
        }
        let main_int_ty = self.next_type;
        let render_ty = self.next_type + 1;
        let val_compare_ty = self.next_type + 2; // (eqref, eqref) -> i32
        self.next_type += 3;

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
        let str_split = self.emit_str_split();
        let str_words = self.emit_str_words();
        let str_lines = self.emit_str_lines();
        let list_repeat = self.emit_list_repeat();
        let list_filter_map = self.emit_list_filter_map();
        let list_sortby = self.emit_list_sortby();
        let list_sortby_insert = self.emit_list_sortby_insert();
        let str_pad_left = self.emit_str_pad(true);
        let str_pad_right = self.emit_str_pad(false);
        let list_intersperse = self.emit_list_intersperse();
        let list_map3 = self.emit_list_map3();
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
        let mut mi = Function::new([]);
        mi.instruction(&Instruction::Call(main_idx));
        mi.instruction(&cast_to(T_INT));
        mi.instruction(&Instruction::StructGet { struct_type_index: T_INT, field_index: 0 });
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
        for &arity in &self.fn_type_order {
            types.ty().function(vec![eqref(); arity as usize], vec![eqref()]);
        }
        types.ty().function(vec![], vec![ValType::I64]); // main_int
        types.ty().function(vec![], vec![ValType::I32]); // render
        types.ty().function(vec![eqref(), eqref()], vec![ValType::I32]); // val_compare

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
        funcs.function(ft2); // str_split
        funcs.function(ft1); // str_words
        funcs.function(ft1); // str_lines
        funcs.function(ft2); // list_repeat
        funcs.function(ft2); // list_filter_map
        funcs.function(ft2); // list_sortby
        funcs.function(ft3); // list_sortby_insert
        funcs.function(ft3); // str_pad_left
        funcs.function(ft3); // str_pad_right
        funcs.function(ft2); // list_intersperse
        funcs.function(ft4); // list_map3
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
        funcs.function(main_int_ty);
        funcs.function(render_ty);
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
        code.function(&str_split);
        code.function(&str_words);
        code.function(&str_lines);
        code.function(&list_repeat);
        code.function(&list_filter_map);
        code.function(&list_sortby);
        code.function(&list_sortby_insert);
        code.function(&str_pad_left);
        code.function(&str_pad_right);
        code.function(&list_intersperse);
        code.function(&list_map3);
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
        code.function(&mi);
        code.function(&render);
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

        let mut exports = ExportSection::new();
        exports.export("main_int", ExportKind::Func, main_int_idx);
        exports.export("render", ExportKind::Func, render_idx);
        exports.export("memory", ExportKind::Memory, 0);

        let mut module = Module::new();
        module.section(&types);
        module.section(&funcs);
        module.section(&mems);
        module.section(&exports);
        module.section(&elems);
        // DataCount must precede Code when code uses `array.new_data`.
        module.section(&DataCountSection { count: 1 });
        module.section(&code);
        module.section(&data);
        let _ = ConstExpr::i32_const(0);
        Ok(module.finish())
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
        f.instruction(&cast_to(T_INT));
        f.instruction(&Instruction::StructGet { struct_type_index: T_INT, field_index: 0 });
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
        let mut lf = Function::new([(extra, eqref())]);
        let mut lctx = FnCtx::new();
        lctx.next_local = total;
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
        f.instruction(&Instruction::StructNew(T_INT));
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
        if (module == "Dict" || module == "Set") && name == "empty" {
            push_empty_list(f);
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
                lf.instruction(&cast_to(T_INT));
                lf.instruction(&Instruction::StructGet { struct_type_index: T_INT, field_index: 0 });
                lf.instruction(&Instruction::LocalGet(1));
                lf.instruction(&cast_to(T_INT));
                lf.instruction(&Instruction::StructGet { struct_type_index: T_INT, field_index: 0 });
                lf.instruction(&match name {
                    "add" => Instruction::I64Add,
                    "sub" => Instruction::I64Sub,
                    _ => Instruction::I64Mul,
                });
                lf.instruction(&Instruction::StructNew(T_INT));
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
                lf.instruction(&Instruction::StructNew(T_INT));
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
        f.instruction(&cast_to(T_INT));
        f.instruction(&Instruction::StructGet { struct_type_index: T_INT, field_index: 0 });
        f.instruction(&Instruction::LocalSet(2)); // mm
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_INT));
        f.instruction(&Instruction::StructGet { struct_type_index: T_INT, field_index: 0 });
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
        f.instruction(&Instruction::StructNew(T_INT));
        f.instruction(&Instruction::End);
        f
    }

    /// list_range(lo, hi) : `[lo, lo+1, .., hi]`.
    fn emit_list_range(&self) -> Function {
        // params lo(0), hi(1). locals: loi(2):i64, cnt(3):i64, n(4), i(5):i32,
        //   data(6):ref T_ARR
        let mut f = Function::new([(2, ValType::I64), (2, ValType::I32), (1, ref_to(T_ARR))]);
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_INT));
        f.instruction(&Instruction::StructGet { struct_type_index: T_INT, field_index: 0 });
        f.instruction(&Instruction::LocalSet(2)); // loi
        // cnt = hi - lo + 1
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_INT));
        f.instruction(&Instruction::StructGet { struct_type_index: T_INT, field_index: 0 });
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
        f.instruction(&Instruction::StructNew(T_INT));
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
        f.instruction(&cast_to(T_INT));
        f.instruction(&Instruction::StructGet { struct_type_index: T_INT, field_index: 0 });
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
        f.instruction(&cast_to(T_INT));
        f.instruction(&Instruction::StructGet { struct_type_index: T_INT, field_index: 0 });
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

    /// str_join(sep, list) : concatenate strings with `sep` between them.
    fn emit_str_join(&self) -> Function {
        // params sep(0), list(1). locals: acc(2):eqref, len(3),start(4),i(5):i32,
        //   data(6):ref T_ARR
        let mut f = Function::new([(1, eqref()), (3, ValType::I32), (1, ref_to(T_ARR))]);
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::ArrayNewDefault(T_STR));
        f.instruction(&Instruction::LocalSet(2)); // acc = ""
        list_len(&mut f, 1);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::LocalGet(3));
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
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32GeS);
        f.instruction(&Instruction::BrIf(1));
        // if i > 0: acc = acc ++ sep
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32GtS);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::Call(self.str_append_idx));
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::End);
        // acc = acc ++ data[start+i]
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(6));
        f.instruction(&Instruction::LocalGet(4));
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::I32Add);
        f.instruction(&Instruction::ArrayGet(T_ARR));
        f.instruction(&Instruction::Call(self.str_append_idx));
        f.instruction(&Instruction::LocalSet(2));
        bump(&mut f, 5, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::End);
        f
    }

    /// str_repeat(n, s) : `s` concatenated `n` times.
    fn emit_str_repeat(&self) -> Function {
        // n(0), s(1); acc(2):eqref, i(3):i32
        let mut f = Function::new([(1, eqref()), (1, ValType::I32)]);
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::ArrayNewDefault(T_STR));
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_INT));
        f.instruction(&Instruction::StructGet { struct_type_index: T_INT, field_index: 0 });
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Block(BlockType::Empty));
        f.instruction(&Instruction::Loop(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Const(0));
        f.instruction(&Instruction::I32LeS);
        f.instruction(&Instruction::BrIf(1));
        f.instruction(&Instruction::LocalGet(2));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::Call(self.str_append_idx));
        f.instruction(&Instruction::LocalSet(2));
        f.instruction(&Instruction::LocalGet(3));
        f.instruction(&Instruction::I32Const(1));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(2));
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
        // i31 (Char)
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::RefTestNonNull(HeapType::Abstract { shared: false, ty: AbstractHeapType::I31 }));
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::RefCastNonNull(HeapType::Abstract { shared: false, ty: AbstractHeapType::I31 }));
        f.instruction(&Instruction::I31GetS);
        f.instruction(&Instruction::LocalSet(3));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::RefCastNonNull(HeapType::Abstract { shared: false, ty: AbstractHeapType::I31 }));
        f.instruction(&Instruction::I31GetS);
        f.instruction(&Instruction::LocalSet(4));
        sign_i(&mut f, 3, 4, false);
        f.instruction(&Instruction::End);
        // T_INT
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::RefTestNonNull(HeapType::Concrete(T_INT)));
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_INT));
        f.instruction(&Instruction::StructGet { struct_type_index: T_INT, field_index: 0 });
        f.instruction(&Instruction::LocalSet(6));
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_INT));
        f.instruction(&Instruction::StructGet { struct_type_index: T_INT, field_index: 0 });
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

    /// list_sort(xs) : insertion sort using val_compare.
    fn emit_list_sort(&self) -> Function {
        let mut f = Function::new([]);
        list_is_empty(&mut f, 0);
        f.instruction(&Instruction::If(BlockType::Result(eqref())));
        push_empty_list(&mut f);
        f.instruction(&Instruction::Else);
        // insert(head, sort(tail))
        list_head(&mut f, 0);
        list_tail(&mut f, 0);
        f.instruction(&Instruction::Call(self.list_sort_idx));
        f.instruction(&Instruction::Call(self.list_insert_idx));
        f.instruction(&Instruction::End);
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
        f.instruction(&cast_to(T_INT));
        f.instruction(&Instruction::StructGet { struct_type_index: T_INT, field_index: 0 });
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
        f.instruction(&Instruction::StructNew(T_INT));
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
        f.instruction(&Instruction::StructNew(T_INT));
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
        f.instruction(&cast_to(T_INT));
        f.instruction(&Instruction::StructGet { struct_type_index: T_INT, field_index: 0 });
        f.instruction(if product { &Instruction::I64Mul } else { &Instruction::I64Add });
        f.instruction(&Instruction::LocalSet(5));
        bump(&mut f, 3, 1);
        f.instruction(&Instruction::Br(0));
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::End);
        f.instruction(&Instruction::LocalGet(5));
        f.instruction(&Instruction::StructNew(T_INT));
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
        f.instruction(&cast_to(T_INT));
        f.instruction(&Instruction::StructGet { struct_type_index: T_INT, field_index: 0 });
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
        f.instruction(&Instruction::StructNew(T_INT));
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
        f.instruction(&Instruction::StructNew(T_INT));
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
        f.instruction(&cast_to(T_INT));
        f.instruction(&Instruction::StructGet { struct_type_index: T_INT, field_index: 0 });
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
        f.instruction(&cast_to(T_INT));
        f.instruction(&Instruction::StructGet { struct_type_index: T_INT, field_index: 0 });
        f.instruction(&Instruction::I32WrapI64);
        f.instruction(&Instruction::LocalSet(3));
        // need = n - len
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_INT));
        f.instruction(&Instruction::StructGet { struct_type_index: T_INT, field_index: 0 });
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
        f.instruction(&Instruction::StructNew(T_INT));
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
        // i31 (Bool/Char/Unit)
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::RefTestNonNull(HeapType::Abstract { shared: false, ty: AbstractHeapType::I31 }));
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::RefCastNonNull(HeapType::Abstract { shared: false, ty: AbstractHeapType::I31 }));
        f.instruction(&Instruction::I31GetS);
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&Instruction::RefCastNonNull(HeapType::Abstract { shared: false, ty: AbstractHeapType::I31 }));
        f.instruction(&Instruction::I31GetS);
        f.instruction(&Instruction::I32Eq);
        f.instruction(&Instruction::RefI31);
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
        // T_INT
        test(&mut f, 0, T_INT);
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&cast_to(T_INT));
        f.instruction(&Instruction::StructGet { struct_type_index: T_INT, field_index: 0 });
        f.instruction(&Instruction::LocalGet(1));
        f.instruction(&cast_to(T_INT));
        f.instruction(&Instruction::StructGet { struct_type_index: T_INT, field_index: 0 });
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
        let mut wf = Function::new([(extra, eqref())]);
        let mut ctx = FnCtx::new();
        ctx.next_local = nparams;
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
                f.instruction(&Instruction::StructNew(T_INT));
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
            // A nullary constructor value (e.g. `Nothing`, `Red`).
            TypedKind::Ctor(_, _, ctor) => self.emit_ctor(ctor.index, &[], ctx, f)?,
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
            TypedKind::Case(scrut, branches) => self.emit_case(scrut, branches, ctx, f)?,
            TypedKind::Lambda(params, body) => self.lift(params, body, ctx, f)?,
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
                f.instruction(&Instruction::StructNew(T_INT));
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
        self.emit_expr(e, ctx, f)?;
        f.instruction(&Instruction::RefCastNonNull(HeapType::Concrete(T_INT)));
        f.instruction(&Instruction::StructGet { struct_type_index: T_INT, field_index: 0 });
        Ok(())
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
                f.instruction(&Instruction::StructNew(T_INT));
            }
            "/" => {
                self.emit_f64(l, ctx, f)?;
                self.emit_f64(r, ctx, f)?;
                f.instruction(&Instruction::F64Div);
                f.instruction(&Instruction::StructNew(T_FLOAT));
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
            ("Dict", "empty") => push_empty_list(f),
            ("Dict", "singleton") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                push_empty_list(f);
                f.instruction(&Instruction::Call(self.dict_insert_idx));
            }
            ("Dict", "insert") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                self.emit_expr(&args[2], ctx, f)?;
                f.instruction(&Instruction::Call(self.dict_insert_idx));
            }
            ("Dict", "get") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.dict_get_idx));
            }
            ("Dict", "member") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.dict_get_idx));
                f.instruction(&cast_to(T_CTOR));
                f.instruction(&Instruction::StructGet { struct_type_index: T_CTOR, field_index: 0 });
                f.instruction(&Instruction::I32Eqz); // Just → True
                f.instruction(&Instruction::RefI31);
            }
            ("Dict", "remove") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.dict_remove_idx));
            }
            ("Dict", "update") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                self.emit_expr(&args[2], ctx, f)?;
                f.instruction(&Instruction::Call(self.dict_update_idx));
            }
            ("Dict", "size") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&cast_to(T_LIST));
                f.instruction(&Instruction::StructGet { struct_type_index: T_LIST, field_index: 0 });
                f.instruction(&Instruction::I64ExtendI32S);
                f.instruction(&Instruction::StructNew(T_INT));
            }
            ("Dict", "isEmpty") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&cast_to(T_LIST));
                f.instruction(&Instruction::StructGet { struct_type_index: T_LIST, field_index: 0 });
                f.instruction(&Instruction::I32Eqz);
                f.instruction(&Instruction::RefI31);
            }
            ("Dict", "toList") => self.emit_expr(&args[0], ctx, f)?,
            ("Dict", "keys") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::Call(self.dict_keys_idx));
            }
            ("Dict", "values") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&Instruction::Call(self.dict_values_idx));
            }
            ("Dict", "fromList") => {
                self.emit_expr(&args[0], ctx, f)?;
                push_empty_list(f);
                f.instruction(&Instruction::Call(self.dict_from_list_idx));
            }
            ("Dict", "foldl") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                self.emit_expr(&args[2], ctx, f)?;
                f.instruction(&Instruction::Call(self.dict_foldl_idx));
            }
            ("Dict", "foldr") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                self.emit_expr(&args[2], ctx, f)?;
                f.instruction(&Instruction::Call(self.dict_foldr_idx));
            }
            ("Dict", "map") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.dict_map_idx));
            }
            ("Dict", "filter") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.dict_filter_idx));
            }
            ("Dict", "union") => {
                // union t1 t2 = insert all of t1 into t2 (t1 wins).
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.dict_from_list_idx));
            }
            ("Dict", "intersect") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.dict_intersect_idx));
            }
            ("Dict", "diff") => {
                // diff t1 t2 = remove t2's keys from t1.
                self.emit_expr(&args[1], ctx, f)?; // toRemove = t2
                self.emit_expr(&args[0], ctx, f)?; // base = t1
                f.instruction(&Instruction::Call(self.dict_diff_idx));
            }
            // Set: a sorted vector of unique elements (a Set IS its own toList,
            // so foldl/foldr/filter/partition are the ordinary List kernels).
            ("Set", "empty") => push_empty_list(f),
            ("Set", "singleton") => {
                self.emit_expr(&args[0], ctx, f)?;
                push_empty_list(f);
                f.instruction(&Instruction::Call(self.set_insert_idx));
            }
            ("Set", "insert") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.set_insert_idx));
            }
            ("Set", "remove") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.set_remove_idx));
            }
            ("Set", "member") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.set_member_idx));
            }
            ("Set", "size") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&cast_to(T_LIST));
                f.instruction(&Instruction::StructGet { struct_type_index: T_LIST, field_index: 0 });
                f.instruction(&Instruction::I64ExtendI32S);
                f.instruction(&Instruction::StructNew(T_INT));
            }
            ("Set", "isEmpty") => {
                self.emit_expr(&args[0], ctx, f)?;
                f.instruction(&cast_to(T_LIST));
                f.instruction(&Instruction::StructGet { struct_type_index: T_LIST, field_index: 0 });
                f.instruction(&Instruction::I32Eqz);
                f.instruction(&Instruction::RefI31);
            }
            ("Set", "toList") => self.emit_expr(&args[0], ctx, f)?,
            ("Set", "fromList") => {
                self.emit_expr(&args[0], ctx, f)?;
                push_empty_list(f);
                f.instruction(&Instruction::Call(self.set_from_list_idx));
            }
            ("Set", "foldl") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                self.emit_expr(&args[2], ctx, f)?;
                f.instruction(&Instruction::Call(self.list_foldl_idx));
            }
            ("Set", "foldr") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                self.emit_expr(&args[2], ctx, f)?;
                f.instruction(&Instruction::Call(self.list_foldr_idx));
            }
            ("Set", "filter") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.list_filter_idx));
            }
            ("Set", "partition") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.list_partition_idx));
            }
            ("Set", "map") => {
                // fromList (List.map f (toList s))
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.list_map_idx));
                push_empty_list(f);
                f.instruction(&Instruction::Call(self.set_from_list_idx));
            }
            ("Set", "union") => {
                // insert all of s1 into s2
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.set_from_list_idx));
            }
            ("Set", "intersect") => {
                self.emit_expr(&args[0], ctx, f)?;
                self.emit_expr(&args[1], ctx, f)?;
                f.instruction(&Instruction::Call(self.set_intersect_idx));
            }
            ("Set", "diff") => {
                self.emit_expr(&args[1], ctx, f)?; // toRemove = s2
                self.emit_expr(&args[0], ctx, f)?; // base = s1
                f.instruction(&Instruction::Call(self.set_diff_idx));
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
            ("Char", "toUpper") | ("Char", "toLower") => {
                let (base, span, delta) = if name == "toUpper" {
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
                f.instruction(&Instruction::StructNew(T_INT));
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
                f.instruction(&Instruction::StructNew(T_INT));
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
                f.instruction(&Instruction::StructNew(T_INT));
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
                f.instruction(&Instruction::StructNew(T_INT));
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
                f.instruction(&Instruction::StructNew(T_INT));
            }
            ("Basics", "floor") | ("Basics", "ceiling") | ("Basics", "truncate") => {
                self.emit_f64(&args[0], ctx, f)?;
                match name {
                    "floor" => f.instruction(&Instruction::F64Floor),
                    "ceiling" => f.instruction(&Instruction::F64Ceil),
                    _ => f.instruction(&Instruction::F64Trunc),
                };
                f.instruction(&Instruction::I64TruncF64S);
                f.instruction(&Instruction::StructNew(T_INT));
            }
            ("Basics", "sqrt") => {
                self.emit_f64(&args[0], ctx, f)?;
                f.instruction(&Instruction::F64Sqrt);
                f.instruction(&Instruction::StructNew(T_FLOAT));
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
                f.instruction(&Instruction::StructNew(T_INT));
            }
            ("Bitwise", "complement") => {
                self.emit_i64(&args[0], ctx, f)?;
                f.instruction(&Instruction::I32WrapI64);
                f.instruction(&Instruction::I32Const(-1));
                f.instruction(&Instruction::I32Xor);
                f.instruction(&Instruction::I64ExtendI32S);
                f.instruction(&Instruction::StructNew(T_INT));
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
                f.instruction(&Instruction::StructNew(T_INT));
            }
            ("Bitwise", "shiftRightZfBy") => {
                self.emit_i64(&args[1], ctx, f)?;
                f.instruction(&Instruction::I32WrapI64);
                self.emit_i64(&args[0], ctx, f)?;
                f.instruction(&Instruction::I32WrapI64);
                f.instruction(&Instruction::I32ShrU);
                f.instruction(&Instruction::I64ExtendI32U); // zero-fill → unsigned
                f.instruction(&Instruction::StructNew(T_INT));
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
                f.instruction(&Instruction::StructNew(T_INT));
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
                f.instruction(&Instruction::RefIsNull);
                f.instruction(&Instruction::RefI31);
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
                f.instruction(&cast_to(T_INT));
                f.instruction(&Instruction::StructGet { struct_type_index: T_INT, field_index: 0 });
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
}

impl FnCtx {
    fn new() -> Self {
        FnCtx { scope: Vec::new(), next_local: 0 }
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
