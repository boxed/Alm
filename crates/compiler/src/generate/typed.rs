//! Typed (monomorphized) native backend — phase 4, walking skeleton.
//!
//! Where the uniform backend ([`super::native`]) represents every value as a
//! tagged i64 word and routes arithmetic through the runtime, this backend
//! consumes the monomorphized, fully-typed IR ([`crate::ir::mono`]) and the
//! layout engine ([`crate::ir::layout`]) to emit *unboxed* code: an `Int` is a
//! native `i64`, a `Float` an `f64`, arithmetic is a plain LLVM instruction
//! with no tag checks.
//!
//! This first cut covers scalar computation (Int/Float/Bool arithmetic,
//! comparisons, `if`, direct calls, recursion). At the program boundary
//! `alm_main` boxes the result back into the uniform word so the existing
//! runtime prints it unchanged. Aggregates, closures, lists, and typed
//! kernels come next; anything unsupported is reported rather than
//! miscompiled.

use std::collections::HashMap;
use std::path::Path;

use inkwell::context::Context;
use inkwell::debug_info::{
    AsDIScope, DIFile, DIFlags, DIFlagsConstants, DILocation, DISubroutineType, DWARFEmissionKind,
    DWARFSourceLanguage, DebugInfoBuilder,
};
use inkwell::module::{FlagBehavior, Linkage, Module};
use inkwell::types::{BasicMetadataTypeEnum, BasicType, BasicTypeEnum};
use inkwell::values::{BasicValue, BasicValueEnum, FunctionValue};
use inkwell::{FloatPredicate, IntPredicate};

use super::native::{self, Target};
use crate::ir::layout::{Layout, LayoutCtx};
use crate::ir::mono::{MonoProgram, TypedExpr, TypedFn, TypedKind, TypedLetDecl};
use crate::reporting::Region;

/// Compile a monomorphized program to a native/wasm binary at `output`.
pub fn build(
    mono: &MonoProgram,
    layouts: &LayoutCtx,
    output: &Path,
    target: Target,
) -> Result<(), String> {
    let context = Context::create();
    let mut cg = TypedCodegen::new(&context, layouts);
    cg.emit(mono)?;
    // Resolve all debug-info metadata before verification or optimization —
    // the verifier and passes both walk (and validate) DWARF metadata.
    cg.di_builder.finalize();
    cg.module
        .verify()
        .map_err(|e| format!("internal error: generated invalid typed IR:\n{}", e))?;
    native::finish(&cg.module, &context, output, target)
}

struct TypedCodegen<'ctx, 'l> {
    ctx: &'ctx Context,
    module: Module<'ctx>,
    builder: inkwell::builder::Builder<'ctx>,
    layouts: &'l LayoutCtx,

    functions: HashMap<String, FunctionValue<'ctx>>,
    /// Closure wrappers for named functions used as first-class values,
    /// keyed by the function's mangled name.
    wrappers: HashMap<String, FunctionValue<'ctx>>,
    /// A canonical module-global closure value for each named function used as
    /// a first-class value, keyed by mangled name. A top-level function's
    /// wrapper closure captures nothing, so it is a constant: sharing one
    /// global instead of heap-allocating per reference makes every reference to
    /// the same function pointer-identical (so structural `==` on values that
    /// embed a function matches Elm's reference equality) and avoids the
    /// per-reference allocation.
    wrapper_closures: HashMap<String, inkwell::values::PointerValue<'ctx>>,
    /// `box_closure` uniform-boxing trampolines, keyed by the function type they
    /// box. The trampoline depends only on the type (which arguments to
    /// unbox/box), so one is shared across every boxing site of that type —
    /// less generated code, and two boxings of the same function share a
    /// function pointer (needed for `==` on boxed functions to match Elm).
    box_tramps: HashMap<String, FunctionValue<'ctx>>,
    /// Memoized box/unbox helper functions for tagged unions, keyed by the
    /// type. Recursive unions need a real recursive function (rather than
    /// inline structural expansion) so codegen terminates; a self-referential
    /// field reuses the same helper.
    box_fns: HashMap<String, FunctionValue<'ctx>>,
    unbox_fns: HashMap<String, FunctionValue<'ctx>>,
    /// Memoized structural-equality helpers for recursive union types, keyed by
    /// type. A recursive union needs a real recursive function (rather than
    /// inline expansion) so codegen terminates; a self-referential field reuses
    /// the same helper. Mirrors [`box_fns`]/[`unbox_fns`].
    eq_fns: HashMap<String, FunctionValue<'ctx>>,
    locals: HashMap<String, BasicValueEnum<'ctx>>,
    cur_fn: Option<FunctionValue<'ctx>>,
    /// When emitting a self-tail-recursive function, its loop state: a mutable
    /// slot per parameter and the loop header block. A tail self-call stores the
    /// new arguments into the slots and branches back to the header instead of
    /// calling — turning unbounded tail recursion into a loop (as Elm's own JS
    /// backend does), so it runs in constant stack.
    tco: Option<TcoState<'ctx>>,
    blk: usize,
    lam_id: usize,
    /// Monotonic counter for fresh variable names introduced when desugaring
    /// destructuring parameters into `\fresh -> case fresh of pat -> body`.
    fresh_id: usize,

    // DWARF debug info.
    di_builder: DebugInfoBuilder<'ctx>,
    di_subroutine: DISubroutineType<'ctx>,
    /// One `DIFile` per source module, keyed by module name.
    di_files: HashMap<String, DIFile<'ctx>>,
    /// The source file of the top-level function currently being emitted, so
    /// lifted lambdas get subprograms in the right `.elm` file.
    cur_file: Option<DIFile<'ctx>>,
    /// The debug location currently applied to the builder. Tracked here rather
    /// than read back from the builder, because LLVM's `GetCurrentDebugLocation`
    /// returns a bogus empty node when the location is unset. Helpers that
    /// switch to another LLVM function save and restore this.
    cur_loc: Option<DILocation<'ctx>>,
}

#[derive(Clone)]
struct TcoState<'ctx> {
    /// The mangled name of the function whose tail self-calls loop.
    mangled: String,
    /// One mutable slot per parameter: `(name, alloca, llvm type)`.
    slots: Vec<(String, inkwell::values::PointerValue<'ctx>, BasicTypeEnum<'ctx>)>,
    /// The loop header block a tail self-call branches back to.
    header: inkwell::basic_block::BasicBlock<'ctx>,
}

impl<'ctx, 'l> TypedCodegen<'ctx, 'l> {
    fn new(ctx: &'ctx Context, layouts: &'l LayoutCtx) -> Self {
        let module = ctx.create_module("alm_typed");

        // Module-level debug-info flags LLVM requires: DWARF v4 line tables and
        // debug-info metadata schema v3. `Warning` behavior lets the runtime
        // bitcode (which carries no debug info after we strip it) merge in
        // without a flag conflict.
        module.add_basic_value_flag(
            "Dwarf Version",
            FlagBehavior::Warning,
            ctx.i32_type().const_int(4, false),
        );
        module.add_basic_value_flag(
            "Debug Info Version",
            FlagBehavior::Warning,
            ctx.i32_type().const_int(3, false),
        );

        // A single compile unit; each function points at its own module file.
        let (di_builder, di_cu) = module.create_debug_info_builder(
            /* allow_unresolved */ true,
            DWARFSourceLanguage::C,
            /* filename */ "alm",
            /* directory */ ".",
            /* producer */ "alm",
            /* is_optimized */ true,
            /* flags */ "",
            /* runtime_ver */ 0,
            /* split_name */ "",
            DWARFEmissionKind::Full,
            /* dwo_id */ 0,
            /* split_debug_inlining */ false,
            /* debug_info_for_profiling */ false,
            /* sysroot */ "",
            /* sdk */ "",
        );
        let di_subroutine =
            di_builder.create_subroutine_type(di_cu.get_file(), None, &[], DIFlags::ZERO);

        TypedCodegen {
            ctx,
            module,
            builder: ctx.create_builder(),
            layouts,
            functions: HashMap::new(),
            wrappers: HashMap::new(),
            wrapper_closures: HashMap::new(),
            box_tramps: HashMap::new(),
            box_fns: HashMap::new(),
            unbox_fns: HashMap::new(),
            eq_fns: HashMap::new(),
            locals: HashMap::new(),
            cur_fn: None,
            tco: None,
            blk: 0,
            lam_id: 0,
            fresh_id: 0,
            di_builder,
            di_subroutine,
            di_files: HashMap::new(),
            cur_file: None,
            cur_loc: None,
        }
    }

    /// Get (or create) the `DIFile` for a source module. Real `.elm` paths are
    /// not threaded this far, so we derive `Foo/Bar.elm` from the module name
    /// `Foo.Bar` (filename = last segment, directory = the earlier segments).
    fn di_file(&mut self, module: &str) -> DIFile<'ctx> {
        if let Some(f) = self.di_files.get(module) {
            return *f;
        }
        let (dir, base) = match module.rsplit_once('.') {
            Some((prefix, last)) => (prefix.replace('.', "/"), last.to_string()),
            None => (".".to_string(), module.to_string()),
        };
        let dir = if dir.is_empty() { ".".to_string() } else { dir };
        let filename = format!("{}.elm", base);
        let file = self.di_builder.create_file(&filename, &dir);
        self.di_files.insert(module.to_string(), file);
        file
    }

    /// Attach a source location to the instructions emitted next. The scope is
    /// taken from the *current* LLVM function's subprogram, so it always
    /// matches (satisfying the verifier). Functions without a subprogram
    /// (runtime glue: box/unbox helpers, trampolines) get no location.
    fn set_loc(&mut self, region: Region) {
        let scope = self
            .builder
            .get_insert_block()
            .and_then(|b| b.get_parent())
            .and_then(|f| f.get_subprogram());
        match scope {
            Some(sp) if region.start.row != 0 => {
                let loc = self.di_builder.create_debug_location(
                    self.ctx,
                    region.start.row,
                    region.start.col,
                    sp.as_debug_info_scope(),
                    None,
                );
                self.cur_loc = Some(loc);
                self.builder.set_current_debug_location(loc);
            }
            // No subprogram (or synthetic node): leave instructions unlocated
            // rather than pointing at the wrong scope.
            _ => self.clear_loc(),
        }
    }

    /// Drop the current debug location. Used at the entry of a generated
    /// function so its first instructions carry no (foreign) location.
    fn clear_loc(&mut self) {
        self.cur_loc = None;
        self.builder.unset_current_debug_location();
    }

    /// Restore a previously-saved debug location after returning from a helper
    /// that switched to another LLVM function.
    fn restore_loc(&mut self, loc: Option<DILocation<'ctx>>) {
        self.cur_loc = loc;
        match loc {
            Some(loc) => self.builder.set_current_debug_location(loc),
            None => self.builder.unset_current_debug_location(),
        }
    }

    fn emit(&mut self, mono: &MonoProgram) -> Result<(), String> {
        // Runtime boxing helpers used only at the print boundary.
        let i64_t = self.ctx.i64_type();
        let f64_t = self.ctx.f64_type();
        self.module.add_function(
            "rt_int",
            i64_t.fn_type(&[i64_t.into()], false),
            Some(Linkage::External),
        );
        self.module.add_function(
            "rt_float",
            i64_t.fn_type(&[f64_t.into()], false),
            Some(Linkage::External),
        );
        // Raw heap allocation for tagged constructors: alm_alloc(size) -> ptr.
        let ptr_t = self.ctx.ptr_type(inkwell::AddressSpace::default());
        self.module.add_function(
            "alm_alloc",
            ptr_t.fn_type(&[i64_t.into()], false),
            Some(Linkage::External),
        );
        self.module.add_function(
            "alm_list_alloc",
            ptr_t.fn_type(&[i64_t.into(), i64_t.into()], false),
            Some(Linkage::External),
        );
        self.module.add_function(
            "alm_list_cons",
            ptr_t.fn_type(&[ptr_t.into(), i64_t.into(), ptr_t.into(), i64_t.into()], false),
            Some(Linkage::External),
        );
        // String helpers — strings stay boxed as the uniform word (i64) and
        // interoperate with the runtime.
        self.module.add_function(
            "rt_str",
            i64_t.fn_type(&[ptr_t.into(), i64_t.into()], false),
            Some(Linkage::External),
        );
        self.module.add_function(
            "rt_append",
            i64_t.fn_type(&[i64_t.into(), i64_t.into()], false),
            Some(Linkage::External),
        );
        for name in ["rtb$String$fromInt", "rtb$String$fromFloat", "rtb$String$length"] {
            self.module.add_function(
                name,
                i64_t.fn_type(&[i64_t.into()], false),
                Some(Linkage::External),
            );
        }
        // Value comparison (for strings and other boxed values) + truthiness.
        for name in ["rt_eq", "rt_neq", "rt_lt", "rt_le", "rt_gt", "rt_ge"] {
            self.module.add_function(
                name,
                i64_t.fn_type(&[i64_t.into(), i64_t.into()], false),
                Some(Linkage::External),
            );
        }
        self.module.add_function(
            "rt_is_true",
            self.ctx.bool_type().fn_type(&[i64_t.into()], false),
            Some(Linkage::External),
        );
        // Boundary unboxing: uniform word -> raw scalar.
        self.module.add_function(
            "rt_unint",
            i64_t.fn_type(&[i64_t.into()], false),
            Some(Linkage::External),
        );
        self.module.add_function(
            "rt_unfloat",
            f64_t.fn_type(&[i64_t.into()], false),
            Some(Linkage::External),
        );
        // Unboxing accessors: read parts of a uniform value (for unbox_value).
        for name in ["rt_unchr", "rt_ctor_tag"] {
            self.module.add_function(
                name,
                self.ctx.i32_type().fn_type(&[i64_t.into()], false),
                Some(Linkage::External),
            );
        }
        for name in ["rt_list_head", "rt_list_tail", "string_to_int", "string_to_float"] {
            self.module.add_function(
                name,
                i64_t.fn_type(&[i64_t.into()], false),
                Some(Linkage::External),
            );
        }
        for name in ["rt_tuple_item", "rt_ctor_arg"] {
            self.module.add_function(
                name,
                i64_t.fn_type(&[i64_t.into(), self.ctx.i32_type().into()], false),
                Some(Linkage::External),
            );
        }
        self.module.add_function(
            "rt_access",
            i64_t.fn_type(&[i64_t.into(), ptr_t.into()], false),
            Some(Linkage::External),
        );
        self.module.add_function(
            "rt_is_nil",
            self.ctx.bool_type().fn_type(&[i64_t.into()], false),
            Some(Linkage::External),
        );

        // Boxing helpers: build uniform values from unboxed ones (for
        // Debug.toString and non-scalar main printing).
        self.module.add_function(
            "rt_chr",
            i64_t.fn_type(&[self.ctx.i32_type().into()], false),
            Some(Linkage::External),
        );
        self.module.add_function(
            "rt_cons",
            i64_t.fn_type(&[i64_t.into(), i64_t.into()], false),
            Some(Linkage::External),
        );
        for name in ["rt_tuple", "rt_list"] {
            self.module.add_function(
                name,
                i64_t.fn_type(&[self.ctx.i32_type().into(), ptr_t.into()], false),
                Some(Linkage::External),
            );
        }
        self.module.add_function(
            "rt_record_new",
            i64_t.fn_type(&[self.ctx.i32_type().into()], false),
            Some(Linkage::External),
        );
        self.module.add_function(
            "rt_record_set",
            self.ctx.void_type().fn_type(
                &[i64_t.into(), self.ctx.i32_type().into(), ptr_t.into(), i64_t.into()],
                false,
            ),
            Some(Linkage::External),
        );
        self.module.add_function(
            "rt_ctor",
            i64_t.fn_type(
                &[ptr_t.into(), self.ctx.i32_type().into(), self.ctx.i32_type().into(), ptr_t.into()],
                false,
            ),
            Some(Linkage::External),
        );
        self.module.add_function(
            "debug_to_string",
            i64_t.fn_type(&[i64_t.into()], false),
            Some(Linkage::External),
        );
        // rt_apply(closure, argc, args) applies a uniform closure.
        self.module.add_function(
            "rt_apply",
            i64_t.fn_type(&[i64_t.into(), self.ctx.i32_type().into(), ptr_t.into()], false),
            Some(Linkage::External),
        );
        // rt_closure(fn_ptr, arity, applied, args) builds a uniform closure.
        self.module.add_function(
            "rt_closure",
            i64_t.fn_type(
                &[ptr_t.into(), self.ctx.i32_type().into(), self.ctx.i32_type().into(), ptr_t.into()],
                false,
            ),
            Some(Linkage::External),
        );
        // DEBUG arity checker (only emitted under ALM_ARITY_CHECK).
        self.module.add_function(
            "alm_dbg_reg",
            self.ctx.void_type().fn_type(&[i64_t.into(), self.ctx.i32_type().into()], false),
            Some(Linkage::External),
        );
        self.module.add_function(
            "alm_dbg_check",
            self.ctx.void_type().fn_type(&[i64_t.into(), self.ctx.i32_type().into()], false),
            Some(Linkage::External),
        );
        // Closure box/unbox identity: register a box trampoline; recover the
        // original typed closure when unboxing one.
        self.module.add_function(
            "alm_reg_box_tramp",
            self.ctx.void_type().fn_type(&[i64_t.into()], false),
            Some(Linkage::External),
        );
        self.module.add_function(
            "alm_recover_boxed",
            i64_t.fn_type(&[i64_t.into()], false),
            Some(Linkage::External),
        );
        // Effect kernels (TEA): all take/return uniform words.
        for (name, arity) in [
            ("platform_worker", 1),
            ("terminal_write_line", 1),
            ("time_every", 2),
            ("cmd_batch", 1),
            ("sub_batch", 1),
            ("cmd_map", 2),
            ("sub_map", 2),
            ("task_succeed", 1),
            ("task_fail", 1),
            ("task_and_then", 2),
            ("task_on_error", 2),
            ("task_map", 2),
            ("task_map2", 3),
            ("task_map_error", 2),
            ("task_sequence", 1),
            ("task_perform", 2),
            ("task_attempt", 2),
            ("process_sleep", 1),
        ] {
            let params = vec![i64_t.into(); arity];
            self.module
                .add_function(name, i64_t.fn_type(&params, false), Some(Linkage::External));
        }
        for name in ["rt_true_v", "rt_false_v", "rt_unit_v"] {
            let g = self.module.add_global(i64_t, None, name);
            g.set_linkage(Linkage::External);
        }

        // Forward-declare every specialization so calls resolve.
        for f in &mono.functions {
            let ret = self.llvm_type(&self.layouts.layout_of(&f.body.tipe));
            let params: Vec<BasicMetadataTypeEnum> = f
                .params
                .iter()
                .map(|(_, t)| self.llvm_type(&self.layouts.layout_of(t)).into())
                .collect();
            let fn_type = ret.fn_type(&params, false);
            let fv = self.module.add_function(
                &f.mangled,
                fn_type,
                Some(Linkage::Internal),
            );
            self.functions.insert(f.mangled.to_string(), fv);
        }

        for f in &mono.functions {
            self.emit_function(f)?;
        }

        self.emit_init();
        self.emit_main(mono)?;
        Ok(())
    }

    /// Map a layout to its LLVM representation. Non-scalar layouts fall back
    /// to the uniform i64 word until they are handled natively.
    fn llvm_type(&self, layout: &Layout) -> BasicTypeEnum<'ctx> {
        match layout {
            Layout::Int => self.ctx.i64_type().into(),
            Layout::Float => self.ctx.f64_type().into(),
            Layout::Bool => self.ctx.bool_type().into(),
            Layout::Char => self.ctx.i32_type().into(),
            Layout::Unit => self.ctx.i64_type().into(),
            // An enumeration is a bare constructor tag.
            Layout::Enum(_) => self.ctx.i32_type().into(),
            Layout::Tuple(elems) => {
                let fields: Vec<BasicTypeEnum> =
                    elems.iter().map(|l| self.llvm_type(l)).collect();
                self.ctx.struct_type(&fields, false).into()
            }
            Layout::Record(fields) => {
                let fields: Vec<BasicTypeEnum> =
                    fields.iter().map(|(_, l)| self.llvm_type(l)).collect();
                self.ctx.struct_type(&fields, false).into()
            }
            // A data-carrying union is a pointer to a heap {tag, fields}
            // block; a boxed recursive reference is likewise a pointer.
            Layout::Tagged(_) | Layout::Ref => {
                self.ctx.ptr_type(inkwell::AddressSpace::default()).into()
            }
            // A list is a pointer to a cons cell (or null for empty).
            Layout::List(_) => self.ctx.ptr_type(inkwell::AddressSpace::default()).into(),
            // A function value is a pointer to a closure {fn_ptr, captures}.
            Layout::Closure => self.ctx.ptr_type(inkwell::AddressSpace::default()).into(),
            // Opaque values are the uniform runtime word.
            Layout::Opaque => self.ctx.i64_type().into(),
            _ => self.ctx.i64_type().into(),
        }
    }

    // ARRAY-BACKED LISTS
    //
    // A `List a` value is a pointer to a `{ i64 len, ptr backing }` header
    // (never null; the empty list is a shared constant header). The backing,
    // managed by the runtime, is `[cap][used][elements...]` with elements
    // stored REVERSED — the head is the last element — so `tail` is O(1) and
    // `cons` appends at the back (O(1) amortized). Element `k` counting from
    // the head is at data index `len - 1 - k`.

    /// The list header type `{ i64 len, ptr backing }`.
    fn list_hdr(&self) -> inkwell::types::StructType<'ctx> {
        let ptr_t = self.ctx.ptr_type(inkwell::AddressSpace::default());
        self.ctx.struct_type(&[self.ctx.i64_type().into(), ptr_t.into()], false)
    }

    /// The size in bytes of a list element's layout.
    fn elem_size(&self, elem: &Layout) -> inkwell::values::IntValue<'ctx> {
        self.llvm_type(elem).size_of().unwrap()
    }

    /// Allocate a list header holding `len` and `backing`, returning it.
    fn make_list(
        &self,
        len: inkwell::values::IntValue<'ctx>,
        backing: inkwell::values::PointerValue<'ctx>,
    ) -> inkwell::values::PointerValue<'ctx> {
        let hdr = self.list_hdr();
        let alloc = self.module.get_function("alm_alloc").unwrap();
        let raw = self
            .builder
            .build_call(alloc, &[hdr.size_of().unwrap().into()], "lh")
            .unwrap()
            .try_as_basic_value()
            .left()
            .unwrap()
            .into_pointer_value();
        let lp = self.builder.build_struct_gep(hdr, raw, 0, "lenp").unwrap();
        self.builder.build_store(lp, len).unwrap();
        let bp = self.builder.build_struct_gep(hdr, raw, 1, "bkp").unwrap();
        self.builder.build_store(bp, backing).unwrap();
        raw
    }

    /// The shared empty-list header `{0, null}`.
    fn empty_list(&mut self) -> inkwell::values::PointerValue<'ctx> {
        let name = "alm.empty_list";
        if let Some(g) = self.module.get_global(name) {
            return g.as_pointer_value();
        }
        let hdr = self.list_hdr();
        let ptr_t = self.ctx.ptr_type(inkwell::AddressSpace::default());
        let init = hdr.const_named_struct(&[
            self.ctx.i64_type().const_zero().into(),
            ptr_t.const_null().into(),
        ]);
        let g = self.module.add_global(hdr, None, name);
        g.set_initializer(&init);
        g.set_constant(true);
        g.set_linkage(Linkage::Private);
        g.as_pointer_value()
    }

    /// Load the length field of a list header.
    fn list_len(&self, list: inkwell::values::PointerValue<'ctx>) -> inkwell::values::IntValue<'ctx> {
        let hdr = self.list_hdr();
        let lp = self.builder.build_struct_gep(hdr, list, 0, "lenp").unwrap();
        self.builder.build_load(self.ctx.i64_type(), lp, "len").unwrap().into_int_value()
    }

    /// Load the backing pointer of a list header.
    fn list_backing(&self, list: inkwell::values::PointerValue<'ctx>) -> inkwell::values::PointerValue<'ctx> {
        let hdr = self.list_hdr();
        let ptr_t = self.ctx.ptr_type(inkwell::AddressSpace::default());
        let bp = self.builder.build_struct_gep(hdr, list, 1, "bkp").unwrap();
        self.builder.build_load(ptr_t, bp, "bk").unwrap().into_pointer_value()
    }

    /// Pointer to element `i` of a backing (data starts 16 bytes in).
    fn list_elem_ptr(
        &self,
        backing: inkwell::values::PointerValue<'ctx>,
        elem: &Layout,
        i: inkwell::values::IntValue<'ctx>,
    ) -> inkwell::values::PointerValue<'ctx> {
        let i8_t = self.ctx.i8_type();
        let data = unsafe {
            self.builder
                .build_in_bounds_gep(i8_t, backing, &[self.ctx.i64_type().const_int(16, false)], "data")
                .unwrap()
        };
        unsafe {
            self.builder
                .build_in_bounds_gep(self.llvm_type(elem), data, &[i], "elp")
                .unwrap()
        }
    }

    /// Load element `i` from a backing.
    fn list_load(
        &self,
        backing: inkwell::values::PointerValue<'ctx>,
        elem: &Layout,
        i: inkwell::values::IntValue<'ctx>,
    ) -> BasicValueEnum<'ctx> {
        let ep = self.list_elem_ptr(backing, elem, i);
        self.builder.build_load(self.llvm_type(elem), ep, "el").unwrap()
    }

    /// Store `v` into element `i` of a backing.
    fn list_store(
        &self,
        backing: inkwell::values::PointerValue<'ctx>,
        elem: &Layout,
        i: inkwell::values::IntValue<'ctx>,
        v: BasicValueEnum<'ctx>,
    ) {
        let ep = self.list_elem_ptr(backing, elem, i);
        self.builder.build_store(ep, v).unwrap();
    }

    /// Emit `for i in 0..count { body(self, i) }`. State is kept in allocas
    /// (mem2reg promotes them), and `body` receives `&mut self` explicitly so
    /// it can emit code and even create blocks.
    fn for_count(
        &mut self,
        count: inkwell::values::IntValue<'ctx>,
        mut body: impl FnMut(&mut Self, inkwell::values::IntValue<'ctx>) -> Result<(), String>,
    ) -> Result<(), String> {
        let i64_t = self.ctx.i64_type();
        let i_slot = self.entry_alloca(i64_t.into(), "i");
        self.builder.build_store(i_slot, i64_t.const_zero()).unwrap();
        let loop_bb = self.new_block("for.loop");
        let body_bb = self.new_block("for.body");
        let done_bb = self.new_block("for.done");
        self.builder.build_unconditional_branch(loop_bb).unwrap();
        self.builder.position_at_end(loop_bb);
        let i = self.builder.build_load(i64_t, i_slot, "i").unwrap().into_int_value();
        let cond = self.builder.build_int_compare(IntPredicate::SLT, i, count, "c").unwrap();
        self.builder.build_conditional_branch(cond, body_bb, done_bb).unwrap();
        self.builder.position_at_end(body_bb);
        body(self, i)?;
        let i2 = self.builder.build_int_add(i, i64_t.const_int(1, false), "i2").unwrap();
        self.builder.build_store(i_slot, i2).unwrap();
        self.builder.build_unconditional_branch(loop_bb).unwrap();
        self.builder.position_at_end(done_bb);
        Ok(())
    }

    /// Element layout of a `List` type.
    fn elem_layout(&self, list_tipe: &crate::ast::canonical::Type) -> Result<Layout, String> {
        match self.layouts.layout_of(list_tipe) {
            Layout::List(elem) => Ok(*elem),
            other => Err(format!("typed backend: expected a list, got {:?}", other)),
        }
    }

    /// The heap struct type for one constructor: an i32 tag followed by the
    /// constructor's field types.
    fn ctor_struct(&self, fields: &[Layout]) -> inkwell::types::StructType<'ctx> {
        let mut members: Vec<BasicTypeEnum> = vec![self.ctx.i32_type().into()];
        members.extend(fields.iter().map(|l| self.llvm_type(l)));
        self.ctx.struct_type(&members, false)
    }

    /// The sorted field names of a record type — the canonical struct order
    /// (matches [`Layout::Record`]).
    fn record_fields(&self, tipe: &crate::ast::canonical::Type) -> Result<Vec<String>, String> {
        match self.layouts.layout_of(tipe) {
            Layout::Record(fields) => Ok(fields.iter().map(|(n, _)| n.to_string()).collect()),
            other => Err(format!("typed backend: expected a record layout, got {:?}", other)),
        }
    }

    fn emit_function(&mut self, f: &TypedFn) -> Result<(), String> {
        let fv = self.functions[&f.mangled.to_string()];
        self.cur_fn = Some(fv);
        self.locals.clear();

        // A DISubprogram per specialization, named for the source function and
        // located in its module's `.elm` file. Attaching it to `fv` lets
        // `set_loc` scope each expression's location to this function.
        let file = self.di_file(f.module.as_str());
        self.cur_file = Some(file);
        let line = f.region.start.row;
        let subprogram = self.di_builder.create_function(
            file.as_debug_info_scope(),
            f.original.as_str(),
            Some(f.mangled.as_str()),
            file,
            line,
            self.di_subroutine,
            /* is_local_to_unit */ true,
            /* is_definition */ true,
            /* scope_line */ line,
            DIFlags::ZERO,
            /* is_optimized */ true,
        );
        fv.set_subprogram(subprogram);

        // Desugar destructuring parameters (`unwrap (Id n) = n`, `f (a, b) = ..`)
        // into fresh variables plus a case-matched body, exactly as `gen_closure`
        // does for lambdas, so the case/pattern compiler binds them.
        let (owned_params, owned_body);
        let (params, body) = if f.params.iter().any(|(p, _)| simple_param_name(p).is_none()) {
            let (np, nb) = desugar_destructuring_params(&mut self.fresh_id, &f.params, &f.body);
            owned_params = np;
            owned_body = nb;
            (owned_params.as_slice(), &owned_body)
        } else {
            (f.params.as_slice(), &f.body)
        };

        for (_, (pattern, _)) in params.iter().enumerate() {
            if simple_param_name(pattern).is_none() {
                return Err(format!(
                    "typed backend: unsupported parameter pattern in `{}`",
                    f.original
                ));
            }
        }
        let entry = self.ctx.append_basic_block(fv, "entry");
        self.builder.position_at_end(entry);
        // Clear any location left over from the previously-emitted function, so
        // the loop-slot allocas (and any entry-block scratch positioned before
        // them) carry no foreign scope.
        self.clear_loc();

        // A top-level nullary value is an Elm constant: referentially
        // transparent and, per Elm's evaluation model, computed once. But a
        // top-level value compiles to a 0-argument function that every
        // reference calls, so its body would re-run on every use. For a heap
        // value — e.g. a lookup `Array`/`Dict` built via `Array.fromList` and
        // consulted in a hot loop — that rebuilds the whole structure on each
        // access, which under the non-freeing bump allocator grows without
        // bound (16 GB on elm-secret-sharing's GF256 tables). Memoize it: a
        // module global caches the computed value behind a "done" flag, so the
        // body runs at most once. Sound because Elm forbids value-level
        // self-reference, so nullary values form a DAG (no memo cycle).
        //
        // Only worth it when the body allocates or calls (same guard as the
        // in-function hoist): a scalar/leaf constant is cheap to recompute and
        // LLVM folds it, whereas the memo would emit two globals + a branch and
        // block constant propagation for anything the optimizer can't prove
        // constant through the cache.
        if params.is_empty() && self.allocating_const(body) {
            let ret_ty = self.llvm_type(&self.layouts.layout_of(&body.tipe));
            let memo = self.module.add_global(ret_ty, None, &format!("{}$memo", f.mangled));
            memo.set_initializer(&ret_ty.const_zero());
            memo.set_linkage(Linkage::Internal);
            let bool_t = self.ctx.bool_type();
            let done = self.module.add_global(bool_t, None, &format!("{}$done", f.mangled));
            done.set_initializer(&bool_t.const_zero());
            done.set_linkage(Linkage::Internal);

            let cached_bb = self.new_block("memo.cached");
            let compute_bb = self.new_block("memo.compute");
            let done_v = self
                .builder
                .build_load(bool_t, done.as_pointer_value(), "done")
                .unwrap()
                .into_int_value();
            self.builder
                .build_conditional_branch(done_v, cached_bb, compute_bb)
                .unwrap();

            self.builder.position_at_end(cached_bb);
            let cv = self.builder.build_load(ret_ty, memo.as_pointer_value(), "memov").unwrap();
            self.builder.build_return(Some(&cv)).unwrap();

            self.builder.position_at_end(compute_bb);
            let value = self.gen(body)?;
            self.builder.build_store(memo.as_pointer_value(), value).unwrap();
            self.builder
                .build_store(done.as_pointer_value(), bool_t.const_int(1, false))
                .unwrap();
            self.builder.build_return(Some(&value)).unwrap();
            return Ok(());
        }

        // A self-tail-recursive function loops instead of recursing: give each
        // parameter a mutable slot, and let `gen_tail` turn a tail self-call
        // into "store new args, branch to header" (constant stack).
        let mangled = f.mangled.to_string();
        if !params.is_empty() && tail_has_self_call(body, &mangled, params.len()) {
            let mut slots = Vec::with_capacity(params.len());
            for (i, (pattern, ty)) in params.iter().enumerate() {
                let name = simple_param_name(pattern).unwrap();
                let lty = self.llvm_type(&self.layouts.layout_of(ty));
                let slot = self.builder.build_alloca(lty, &name).unwrap();
                self.builder
                    .build_store(slot, fv.get_nth_param(i as u32).unwrap())
                    .unwrap();
                slots.push((name, slot, lty));
            }
            let header = self.new_block("tail.header");
            self.builder.build_unconditional_branch(header).unwrap();
            self.builder.position_at_end(header);
            for (name, slot, lty) in &slots {
                let v = self.builder.build_load(*lty, *slot, name).unwrap();
                self.locals.insert(name.clone(), v);
            }
            self.tco = Some(TcoState {
                mangled,
                slots,
                header,
            });
            let r = self.gen_tail(body);
            self.tco = None;
            return r;
        }

        for (i, (pattern, _)) in params.iter().enumerate() {
            let name = simple_param_name(pattern).unwrap();
            self.locals.insert(name, fv.get_nth_param(i as u32).unwrap());
        }
        let value = self.gen(body)?;
        self.builder.build_return(Some(&value)).unwrap();
        Ok(())
    }

    /// Generate an expression in tail position within a self-tail-recursive
    /// function. Every path terminates its block: a tail self-call stores the
    /// new arguments and branches to the loop header; anything else evaluates
    /// and returns. `if`/`case`/`let` recurse into their tail sub-expressions.
    fn gen_tail(&mut self, expr: &TypedExpr) -> Result<(), String> {
        self.set_loc(expr.region);
        let tco = self.tco.clone();
        match &expr.kind {
            TypedKind::Call(func, args) => {
                if let (Some(tco), TypedKind::Global(n)) = (&tco, &func.kind) {
                    if n.to_string() == tco.mangled && args.len() == tco.slots.len() {
                        let mut vals = Vec::with_capacity(args.len());
                        for a in args {
                            vals.push(self.gen(a)?);
                        }
                        for ((_, slot, _), v) in tco.slots.iter().zip(vals) {
                            self.builder.build_store(*slot, v).unwrap();
                        }
                        self.builder.build_unconditional_branch(tco.header).unwrap();
                        return Ok(());
                    }
                }
                let v = self.gen(expr)?;
                self.builder.build_return(Some(&v)).unwrap();
                Ok(())
            }
            TypedKind::If(branches, otherwise) => {
                for (cond, then) in branches {
                    let cv = self.gen(cond)?.into_int_value();
                    let then_bb = self.new_block("if.then");
                    let else_bb = self.new_block("if.else");
                    self.builder
                        .build_conditional_branch(cv, then_bb, else_bb)
                        .unwrap();
                    self.builder.position_at_end(then_bb);
                    self.gen_tail(then)?;
                    self.builder.position_at_end(else_bb);
                }
                self.gen_tail(otherwise)
            }
            TypedKind::Case(scrutinee, branches) => self.gen_case_tail(scrutinee, branches),
            TypedKind::Let(decls, body) => {
                self.bind_let_decls(decls)?;
                self.gen_tail(body)
            }
            _ => {
                let v = self.gen(expr)?;
                self.builder.build_return(Some(&v)).unwrap();
                Ok(())
            }
        }
    }

    /// The tail-position variant of [`gen_case`]: each matched branch is
    /// generated in tail position (returning or looping) rather than feeding a
    /// join phi.
    fn gen_case_tail(
        &mut self,
        scrutinee: &TypedExpr,
        branches: &[(crate::ast::canonical::Pattern, TypedExpr)],
    ) -> Result<(), String> {
        let subject = self.gen(scrutinee)?;
        let mut matched_all = false;
        for (pattern, body) in branches {
            let fail = self.new_block("case.next");
            let refutable = self.match_pattern(pattern, subject, &scrutinee.tipe, fail)?;
            self.gen_tail(body)?;
            self.builder.position_at_end(fail);
            if !refutable {
                self.builder.build_unreachable().unwrap();
                matched_all = true;
                break;
            }
        }
        if !matched_all {
            self.builder.build_unreachable().unwrap();
        }
        Ok(())
    }

    /// `alm_init` — nothing to initialize; top-level values are functions.
    fn emit_init(&mut self) {
        let fv = self.module.add_function(
            "alm_init",
            self.ctx.void_type().fn_type(&[], false),
            Some(Linkage::External),
        );
        let block = self.ctx.append_basic_block(fv, "entry");
        self.builder.position_at_end(block);
        self.clear_loc();
        self.builder.build_return(None).unwrap();
    }

    /// `alm_main` calls the `main` specialization and boxes its result into
    /// the uniform word so the runtime's print path handles it.
    fn emit_main(&mut self, mono: &MonoProgram) -> Result<(), String> {
        let i64_t = self.ctx.i64_type();
        let fv = self
            .module
            .add_function("alm_main", i64_t.fn_type(&[], false), Some(Linkage::External));
        let block = self.ctx.append_basic_block(fv, "entry");
        self.builder.position_at_end(block);
        // `alm_main` (and the box helpers it calls) carry no subprogram; keep
        // their instructions unlocated so they never reference a stale scope.
        self.clear_loc();

        let main = mono.functions.iter().find(|f| f.original.as_str() == "main");
        let Some(main) = main else {
            self.builder.build_return(Some(&i64_t.const_zero())).unwrap();
            return Ok(());
        };
        let main_fn = self.functions[&main.mangled.to_string()];
        let main_ty = main.body.tipe.clone();
        let raw = self
            .builder
            .build_call(main_fn, &[], "main")
            .unwrap()
            .try_as_basic_value()
            .left()
            .unwrap();

        // Box the result into the uniform word. For a Program this is the
        // Program value the runtime's TEA loop consumes; for Int/Float/String
        // it is what the print path handles.
        let boxed = self.box_value(raw, &main_ty)?;
        self.builder.build_return(Some(&boxed)).unwrap();
        Ok(())
    }

    fn call_box(&self, name: &str, arg: BasicValueEnum<'ctx>) -> BasicValueEnum<'ctx> {
        self.call_named(name, &[arg])
    }

    /// Get or declare a runtime function of `argc` uniform (i64) parameters
    /// returning a uniform word.
    fn runtime_fn(&self, name: &str, argc: usize) -> FunctionValue<'ctx> {
        self.module.get_function(name).unwrap_or_else(|| {
            let i64_t = self.ctx.i64_type();
            let params = vec![i64_t.into(); argc];
            self.module
                .add_function(name, i64_t.fn_type(&params, false), Some(Linkage::External))
        })
    }

    /// Call a runtime collection function: box each argument to a uniform
    /// value, invoke the function, and unbox the result to `result_tipe`. Used
    /// for Dict/Set/Array whose values are opaque uniform words.
    fn marshal_call(
        &mut self,
        symbol: &str,
        args: &[TypedExpr],
        result_tipe: &crate::ast::canonical::Type,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let mut boxed = Vec::with_capacity(args.len());
        for arg in args {
            let v = self.gen(arg)?;
            boxed.push(self.box_value(v, &arg.tipe)?);
        }
        let f = self.runtime_fn(symbol, boxed.len());
        let argv: Vec<_> = boxed.iter().map(|b| (*b).into()).collect();
        let uniform = self
            .builder
            .build_call(f, &argv, "coll")
            .unwrap()
            .try_as_basic_value()
            .left()
            .unwrap();
        self.unbox_value(uniform, result_tipe)
    }

    /// Call a runtime effect function: box each argument into a uniform value
    /// and invoke it, yielding the uniform result word.
    fn effect_call(
        &mut self,
        symbol: &str,
        args: &[TypedExpr],
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let mut boxed = Vec::with_capacity(args.len());
        for arg in args {
            let v = self.gen(arg)?;
            boxed.push(self.box_value(v, &arg.tipe)?);
        }
        Ok(self.call_named(symbol, &boxed))
    }

    /// Load a runtime value global (e.g. `$Platform$Cmd$none`) as a uniform
    /// word, declaring it on first use.
    fn load_uniform_global(&mut self, sym: &str) -> BasicValueEnum<'ctx> {
        let g = self.module.get_global(sym).unwrap_or_else(|| {
            let g = self.module.add_global(self.ctx.i64_type(), None, sym);
            g.set_linkage(Linkage::External);
            g
        });
        self.builder
            .build_load(self.ctx.i64_type(), g.as_pointer_value(), "gv")
            .unwrap()
    }

    fn load_global(&self, name: &str) -> BasicValueEnum<'ctx> {
        let g = self.module.get_global(name).unwrap();
        self.builder
            .build_load(self.ctx.i64_type(), g.as_pointer_value(), name)
            .unwrap()
    }

    /// A private, null-terminated C string constant, returning a pointer to it.
    fn cstr(&mut self, text: &str) -> inkwell::values::PointerValue<'ctx> {
        let data = self.ctx.const_string(text.as_bytes(), true);
        let g = self.module.add_global(data.get_type(), None, "cstr");
        g.set_initializer(&data);
        g.set_constant(true);
        g.set_linkage(Linkage::Private);
        g.as_pointer_value()
    }

    /// Convert an unboxed value to the uniform runtime word, recursively, so
    /// the runtime's Debug renderer can format it. Custom unions are not
    /// handled yet.
    fn box_value(
        &mut self,
        val: BasicValueEnum<'ctx>,
        tipe: &crate::ast::canonical::Type,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        match self.layouts.layout_of(tipe) {
            Layout::Int => Ok(self.call_named("rt_int", &[val])),
            Layout::Float => Ok(self.call_named("rt_float", &[val])),
            Layout::Char => Ok(self.call_named("rt_chr", &[val])),
            Layout::Str => Ok(val),
            Layout::Unit => Ok(self.load_global("rt_unit_v")),
            Layout::Bool => {
                let t = self.load_global("rt_true_v");
                let f = self.load_global("rt_false_v");
                Ok(self.builder.build_select(val.into_int_value(), t, f, "boolbox").unwrap())
            }
            Layout::Tuple(_) => self.box_tuple(val, tipe),
            Layout::Record(_) => self.box_record(val, tipe),
            Layout::List(_) => self.box_list(val, tipe),
            Layout::Enum(_) => self.box_enum(val, tipe),
            // Route through a memoized recursive helper so recursive unions
            // (whose fields refer back to themselves) terminate at codegen.
            Layout::Tagged(_) => {
                let f = self.get_box_fn(tipe)?;
                Ok(self
                    .builder
                    .build_call(f, &[val.into()], "boxu")
                    .unwrap()
                    .try_as_basic_value()
                    .left()
                    .unwrap())
            }
            Layout::Closure => self.box_closure(val, tipe),
            // Opaque values are already the uniform word.
            Layout::Opaque => Ok(val),
            // An unresolved type variable is carried as an opaque uniform word
            // held in a pointer slot (see `unbox_value`'s `Ref` arm, which is
            // `int_to_ptr`). Boxing is the inverse — `ptr_to_int` — NOT a unit
            // placeholder: this arm really does run when a value of unresolved
            // type flows through a polymorphic boundary (e.g. a msg passed to
            // `Test.Html`'s `taggerFunction : Tagger -> (a -> msg)`, whose `a`
            // never gets pinned). Discarding it as unit corrupted the value.
            Layout::Ref => {
                // A Ref value is carried in a pointer slot; if it is already an
                // integer word (some phantom sites), pass it through.
                if val.is_pointer_value() {
                    Ok(self
                        .builder
                        .build_ptr_to_int(val.into_pointer_value(), self.ctx.i64_type(), "refbox")
                        .unwrap()
                        .into())
                } else {
                    Ok(val)
                }
            }
        }
    }

    fn box_tuple(
        &mut self,
        val: BasicValueEnum<'ctx>,
        tipe: &crate::ast::canonical::Type,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        use crate::ast::canonical::Type;
        let subs: Vec<&Type> = match tipe {
            Type::Tuple(a, b, c) => {
                let mut v = vec![a.as_ref(), b.as_ref()];
                if let Some(c) = c {
                    v.push(c);
                }
                v
            }
            _ => return Err("typed backend: box_tuple on non-tuple".to_string()),
        };
        let i64_t = self.ctx.i64_type();
        let sv = val.into_struct_value();
        let arr = self.entry_alloca(i64_t.array_type(subs.len() as u32).into(), "tup");
        for (i, sub) in subs.iter().enumerate() {
            let fv = self.builder.build_extract_value(sv, i as u32, "e").unwrap();
            let boxed = self.box_value(fv, sub)?;
            let ep = unsafe {
                self.builder
                    .build_in_bounds_gep(i64_t, arr, &[i64_t.const_int(i as u64, false)], "ep")
                    .unwrap()
            };
            self.builder.build_store(ep, boxed).unwrap();
        }
        let n = self.ctx.i32_type().const_int(subs.len() as u64, false).into();
        Ok(self.call_named("rt_tuple", &[n, arr.into()]))
    }

    fn box_record(
        &mut self,
        val: BasicValueEnum<'ctx>,
        tipe: &crate::ast::canonical::Type,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        use crate::ast::canonical::Type;
        let field_types = match tipe {
            Type::Record(fields, _) => fields.clone(),
            _ => return Err("typed backend: box_record on non-record".to_string()),
        };
        let sorted = match self.layouts.layout_of(tipe) {
            Layout::Record(fs) => fs,
            _ => unreachable!(),
        };
        let sv = val.into_struct_value();
        let n = self.ctx.i32_type().const_int(sorted.len() as u64, false).into();
        let rec = self.call_named("rt_record_new", &[n]);
        let set = self.module.get_function("rt_record_set").unwrap();
        for (i, (fname, _)) in sorted.iter().enumerate() {
            let field_ty = field_types
                .iter()
                .find(|(n, _)| n == fname)
                .map(|(_, t)| t.clone())
                .ok_or_else(|| format!("typed backend: missing field `{}`", fname))?;
            let fv = self.builder.build_extract_value(sv, i as u32, "f").unwrap();
            let boxed = self.box_value(fv, &field_ty)?;
            let nameptr = self.cstr(fname.as_str());
            let idx = self.ctx.i32_type().const_int(i as u64, false);
            self.builder
                .build_call(
                    set,
                    &[rec.into(), idx.into(), nameptr.into(), boxed.into()],
                    "",
                )
                .unwrap();
        }
        Ok(rec)
    }

    /// Box an enum value (i32 tag) into a uniform nullary constructor,
    /// switching on the tag to pick the constructor name.
    fn box_enum(
        &mut self,
        val: BasicValueEnum<'ctx>,
        tipe: &crate::ast::canonical::Type,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let ctors = self
            .layouts
            .union_ctors(tipe)
            .ok_or_else(|| "typed backend: unknown enum for Debug.toString".to_string())?;
        let i32_t = self.ctx.i32_type();
        let ptr_t = self.ctx.ptr_type(inkwell::AddressSpace::default());
        let tag = val.into_int_value();
        let default_bb = self.new_block("boxe.def");
        let merge = self.new_block("boxe.end");
        let mut incoming: Vec<(BasicValueEnum<'ctx>, inkwell::basic_block::BasicBlock<'ctx>)> =
            Vec::new();
        let blocks: Vec<_> = (0..ctors.len()).map(|k| self.new_block(&format!("boxe.{}", k))).collect();
        let cases: Vec<_> = blocks
            .iter()
            .enumerate()
            .map(|(k, bb)| (i32_t.const_int(k as u64, false), *bb))
            .collect();
        self.builder.build_switch(tag, default_bb, &cases).unwrap();

        for (k, bb) in blocks.iter().enumerate() {
            self.builder.position_at_end(*bb);
            let nameptr = self.cstr(ctors[k].0.as_str());
            let ctor = self.call_named(
                "rt_ctor",
                &[
                    nameptr.into(),
                    i32_t.const_int(k as u64, false).into(),
                    i32_t.const_zero().into(),
                    ptr_t.const_null().into(),
                ],
            );
            self.builder.build_unconditional_branch(merge).unwrap();
            incoming.push((ctor, *bb));
        }
        self.builder.position_at_end(default_bb);
        self.builder.build_unreachable().unwrap();

        self.builder.position_at_end(merge);
        let phi = self.builder.build_phi(self.ctx.i64_type(), "enum").unwrap();
        for (v, bb) in &incoming {
            phi.add_incoming(&[(v as &dyn BasicValue, *bb)]);
        }
        Ok(phi.as_basic_value())
    }

    /// Box a tagged-union value into a uniform constructor: switch on the tag,
    /// box each field of the matched constructor, call rt_ctor.
    /// Get (or generate) a recursive helper `box_T(ptr) -> uniform` for a
    /// tagged union. Registered in the cache *before* its body is emitted so a
    /// self-referential field reuses the same function instead of expanding
    /// the type forever.
    fn get_box_fn(
        &mut self,
        tipe: &crate::ast::canonical::Type,
    ) -> Result<FunctionValue<'ctx>, String> {
        let key = format!("{:?}", tipe);
        if let Some(f) = self.box_fns.get(&key) {
            return Ok(*f);
        }
        let i64_t = self.ctx.i64_type();
        let ptr_t = self.ctx.ptr_type(inkwell::AddressSpace::default());
        let fname = format!("box.{}", self.lam_id);
        self.lam_id += 1;
        let fty = i64_t.fn_type(&[ptr_t.into()], false);
        let f = self.module.add_function(&fname, fty, Some(Linkage::Internal));
        self.box_fns.insert(key, f);

        let saved_locals = std::mem::take(&mut self.locals);
        let saved_fn = self.cur_fn;
        let saved_block = self.builder.get_insert_block();
        let saved_loc = self.cur_loc;
        self.cur_fn = Some(f);
        let entry = self.ctx.append_basic_block(f, "entry");
        self.builder.position_at_end(entry);
        self.clear_loc();
        let param = f.get_nth_param(0).unwrap();
        let result = self.box_tagged(param, tipe);
        self.locals = saved_locals;
        self.cur_fn = saved_fn;
        let boxed = result?;
        self.builder.build_return(Some(&boxed)).unwrap();
        if let Some(b) = saved_block {
            self.builder.position_at_end(b);
        }
        // Restore the caller's debug location: a helper must neither leak its
        // own (subprogram-less) scope nor its inner body's scope into the
        // function it returns to.
        self.restore_loc(saved_loc);
        Ok(f)
    }

    /// Symmetric to [`get_box_fn`]: a recursive helper `unbox_T(uniform) ->
    /// ptr` for a tagged union.
    fn get_unbox_fn(
        &mut self,
        tipe: &crate::ast::canonical::Type,
    ) -> Result<FunctionValue<'ctx>, String> {
        let key = format!("{:?}", tipe);
        if let Some(f) = self.unbox_fns.get(&key) {
            return Ok(*f);
        }
        let i64_t = self.ctx.i64_type();
        let ptr_t = self.ctx.ptr_type(inkwell::AddressSpace::default());
        let fname = format!("unbox.{}", self.lam_id);
        self.lam_id += 1;
        let fty = ptr_t.fn_type(&[i64_t.into()], false);
        let f = self.module.add_function(&fname, fty, Some(Linkage::Internal));
        self.unbox_fns.insert(key, f);

        let saved_locals = std::mem::take(&mut self.locals);
        let saved_fn = self.cur_fn;
        let saved_block = self.builder.get_insert_block();
        let saved_loc = self.cur_loc;
        self.cur_fn = Some(f);
        let entry = self.ctx.append_basic_block(f, "entry");
        self.builder.position_at_end(entry);
        self.clear_loc();
        let param = f.get_nth_param(0).unwrap();
        let result = self.unbox_tagged(param, tipe);
        self.locals = saved_locals;
        self.cur_fn = saved_fn;
        let unboxed = result?;
        self.builder.build_return(Some(&unboxed)).unwrap();
        if let Some(b) = saved_block {
            self.builder.position_at_end(b);
        }
        // Restore the caller's debug location: a helper must neither leak its
        // own (subprogram-less) scope nor its inner body's scope into the
        // function it returns to.
        self.restore_loc(saved_loc);
        Ok(f)
    }

    fn box_tagged(
        &mut self,
        val: BasicValueEnum<'ctx>,
        tipe: &crate::ast::canonical::Type,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let ctors = self
            .layouts
            .union_ctors(tipe)
            .ok_or_else(|| "typed backend: unknown union for Debug.toString".to_string())?;
        let i32_t = self.ctx.i32_type();
        let i64_t = self.ctx.i64_type();
        let ptr = val.into_pointer_value();
        let tag = self.builder.build_load(i32_t, ptr, "tag").unwrap().into_int_value();
        let default_bb = self.new_block("boxt.def");
        let merge = self.new_block("boxt.end");
        let mut incoming: Vec<(BasicValueEnum<'ctx>, inkwell::basic_block::BasicBlock<'ctx>)> =
            Vec::new();
        let blocks: Vec<_> = (0..ctors.len()).map(|k| self.new_block(&format!("boxt.{}", k))).collect();
        let cases: Vec<_> = blocks
            .iter()
            .enumerate()
            .map(|(k, bb)| (i32_t.const_int(k as u64, false), *bb))
            .collect();
        self.builder.build_switch(tag, default_bb, &cases).unwrap();

        for (k, bb) in blocks.iter().enumerate() {
            self.builder.position_at_end(*bb);
            let (name, field_types) = &ctors[k];
            let field_layouts: Vec<Layout> =
                field_types.iter().map(|t| self.layouts.layout_of(t)).collect();
            let sty = self.ctor_struct(&field_layouts);
            let argc = field_types.len();
            let args_arr =
                self.entry_alloca(i64_t.array_type(argc.max(1) as u32).into(), "args");
            for (i, fty) in field_types.iter().enumerate() {
                let fp = self.builder.build_struct_gep(sty, ptr, (i + 1) as u32, "fp").unwrap();
                let fv = self.builder.build_load(self.llvm_type(&field_layouts[i]), fp, "fv").unwrap();
                let boxed = self.box_value(fv, fty)?;
                let ep = unsafe {
                    self.builder
                        .build_in_bounds_gep(i64_t, args_arr, &[i64_t.const_int(i as u64, false)], "ep")
                        .unwrap()
                };
                self.builder.build_store(ep, boxed).unwrap();
            }
            let nameptr = self.cstr(name.as_str());
            let ctor = self.call_named(
                "rt_ctor",
                &[
                    nameptr.into(),
                    i32_t.const_int(k as u64, false).into(),
                    i32_t.const_int(argc as u64, false).into(),
                    args_arr.into(),
                ],
            );
            let end = self.builder.get_insert_block().unwrap();
            self.builder.build_unconditional_branch(merge).unwrap();
            incoming.push((ctor, end));
        }
        self.builder.position_at_end(default_bb);
        self.builder.build_unreachable().unwrap();

        self.builder.position_at_end(merge);
        let phi = self.builder.build_phi(i64_t, "union").unwrap();
        for (v, bb) in &incoming {
            phi.add_incoming(&[(v as &dyn BasicValue, *bb)]);
        }
        Ok(phi.as_basic_value())
    }

    /// Box a typed closure into a uniform runtime closure: generate a
    /// trampoline that the runtime can call with uniform arguments, which
    /// unboxes them, applies the typed closure, and boxes the result.
    fn box_closure(
        &mut self,
        val: BasicValueEnum<'ctx>,
        tipe: &crate::ast::canonical::Type,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        use crate::ast::canonical::Type;
        let mut arg_types = Vec::new();
        let mut t = tipe.clone();
        while let Type::Lambda(a, b) = t {
            arg_types.push(*a);
            t = *b;
        }
        let ret_type = t;
        let n = arg_types.len();
        let i64_t = self.ctx.i64_type();
        let ptr_t = self.ctx.ptr_type(inkwell::AddressSpace::default());

        // Trampoline: (captured typed closure, arg0.., arg_{n-1}) -> result,
        // all uniform words. It depends only on the function type, so it is
        // shared across every boxing site of that type (keyed below) — less
        // generated code, and two boxings of the same function get the same
        // function pointer so a boxed-function `==` can match.
        let key = format!("{:?}", tipe);
        let tramp = if let Some(t) = self.box_tramps.get(&key) {
            *t
        } else {
            let tramp_params: Vec<BasicMetadataTypeEnum> = vec![i64_t.into(); n + 1];
            let tramp_ty = i64_t.fn_type(&tramp_params, false);
            let name = format!("clos.{}", self.lam_id);
            self.lam_id += 1;
            let tramp = self.module.add_function(&name, tramp_ty, Some(Linkage::Internal));

            let saved_locals = std::mem::take(&mut self.locals);
            let saved_fn = self.cur_fn;
            let saved_block = self.builder.get_insert_block();
            let saved_loc = self.cur_loc;
            self.cur_fn = Some(tramp);
            let entry = self.ctx.append_basic_block(tramp, "entry");
            self.builder.position_at_end(entry);
            self.clear_loc();
            let cap = tramp.get_nth_param(0).unwrap().into_int_value();
            let clos = self.builder.build_int_to_ptr(cap, ptr_t, "clos").unwrap();
            let mut typed_args = Vec::new();
            for (i, at) in arg_types.iter().enumerate() {
                let uni = tramp.get_nth_param((i + 1) as u32).unwrap();
                typed_args.push(self.unbox_value(uni, at)?);
            }
            let ret_layout = self.layouts.layout_of(&ret_type);
            let result = self.apply_closure(clos, &typed_args, &ret_layout);
            let body_result = self.box_value(result, &ret_type);
            self.locals = saved_locals;
            self.cur_fn = saved_fn;
            let boxed = body_result?;
            self.builder.build_return(Some(&boxed)).unwrap();
            if let Some(b) = saved_block {
                self.builder.position_at_end(b);
            }
            // Restore the caller's debug location: a helper must neither leak
            // its own (subprogram-less) scope nor its inner body's scope into
            // the function it returns to.
            self.restore_loc(saved_loc);
            self.box_tramps.insert(key, tramp);
            tramp
        };

        // Build the uniform closure capturing the typed closure pointer, and
        // register the trampoline so `unbox_closure` can recover this exact
        // typed closure — box∘unbox is then identity (a function that round-
        // trips through the uniform representation stays the same value).
        let tramp_word = self
            .builder
            .build_ptr_to_int(tramp.as_global_value().as_pointer_value(), i64_t, "tw")
            .unwrap();
        let reg_fn = self.module.get_function("alm_reg_box_tramp").unwrap();
        self.builder.build_call(reg_fn, &[tramp_word.into()], "").unwrap();
        let closure_word = self
            .builder
            .build_ptr_to_int(val.into_pointer_value(), i64_t, "cw")
            .unwrap();
        let args_arr = self.entry_alloca(i64_t.array_type(1).into(), "capargs");
        self.builder.build_store(args_arr, closure_word).unwrap();
        Ok(self.call_named(
            "rt_closure",
            &[
                tramp.as_global_value().as_pointer_value().into(),
                self.ctx.i32_type().const_int((n + 1) as u64, false).into(),
                self.ctx.i32_type().const_int(1, false).into(),
                args_arr.into(),
            ],
        ))
    }

    /// The inverse of `box_closure`: wrap a uniform runtime closure word in a
    /// typed closure `{fn_ptr, uniform_word}` whose lifted function boxes its
    /// typed arguments, applies the uniform closure through `rt_apply`, and
    /// unboxes the uniform result back to the typed result. This lets a function
    /// value that flowed through a boxed/polymorphic slot (e.g. a `Dict` value,
    /// or any generic container) be called with the typed calling convention.
    fn unbox_closure(
        &mut self,
        w: BasicValueEnum<'ctx>,
        tipe: &crate::ast::canonical::Type,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        use crate::ast::canonical::Type;
        let mut arg_types = Vec::new();
        let mut t = tipe.clone();
        while let Type::Lambda(a, b) = t {
            arg_types.push(*a);
            t = *b;
        }
        let ret_type = t;
        let n = arg_types.len();
        let i64_t = self.ctx.i64_type();
        let ptr_t = self.ctx.ptr_type(inkwell::AddressSpace::default());

        // Lifted typed function: (env, typed args...) -> typed result.
        let ret_layout = self.layouts.layout_of(&ret_type);
        let ret_ty = self.llvm_type(&ret_layout);
        let mut fn_params: Vec<BasicMetadataTypeEnum> = vec![ptr_t.into()];
        for at in &arg_types {
            fn_params.push(self.llvm_type(&self.layouts.layout_of(at)).into());
        }
        let fn_ty = ret_ty.fn_type(&fn_params, false);
        let name = format!("unclos.{}", self.lam_id);
        self.lam_id += 1;
        let lifted = self.module.add_function(&name, fn_ty, Some(Linkage::Internal));

        let saved_locals = std::mem::take(&mut self.locals);
        let saved_fn = self.cur_fn;
        let saved_block = self.builder.get_insert_block();
        let saved_loc = self.cur_loc;
        self.cur_fn = Some(lifted);
        let entry = self.ctx.append_basic_block(lifted, "entry");
        self.builder.position_at_end(entry);
        self.clear_loc();

        let body = (|s: &mut Self| -> Result<BasicValueEnum<'ctx>, String> {
            // The typed closure layout is {fn_ptr, uniform_word}; the captured
            // uniform closure word lives in field 1.
            let clos_ty = s.ctx.struct_type(&[ptr_t.into(), i64_t.into()], false);
            let env = lifted.get_nth_param(0).unwrap().into_pointer_value();
            let fp = s.builder.build_struct_gep(clos_ty, env, 1, "capp").unwrap();
            let uni_clos = s.builder.build_load(i64_t, fp, "unic").unwrap();
            // Box each typed argument into a uniform word.
            let mut boxed = Vec::with_capacity(n);
            for (i, at) in arg_types.iter().enumerate() {
                let a = lifted.get_nth_param((i + 1) as u32).unwrap();
                boxed.push(s.box_value(a, at)?);
            }
            let args_arr = s.entry_alloca(i64_t.array_type(n as u32).into(), "uargs");
            for (i, b) in boxed.iter().enumerate() {
                let ep = unsafe {
                    s.builder
                        .build_in_bounds_gep(i64_t, args_arr, &[i64_t.const_int(i as u64, false)], "ep")
                        .unwrap()
                };
                s.builder.build_store(ep, *b).unwrap();
            }
            let rt_apply = s.module.get_function("rt_apply").unwrap();
            let uniform = s
                .builder
                .build_call(
                    rt_apply,
                    &[
                        uni_clos.into(),
                        s.ctx.i32_type().const_int(n as u64, false).into(),
                        args_arr.into(),
                    ],
                    "uapp",
                )
                .unwrap()
                .try_as_basic_value()
                .left()
                .unwrap();
            s.unbox_value(uniform, &ret_type)
        })(self);

        self.locals = saved_locals;
        self.cur_fn = saved_fn;
        let result = body?;
        self.builder.build_return(Some(&result)).unwrap();
        if let Some(b) = saved_block {
            self.builder.position_at_end(b);
        }
        self.restore_loc(saved_loc);

        self.emit_arity_reg(lifted);
        // If `w` is a closure that `box_closure` produced, recover the original
        // typed closure instead of wrapping it again — so box∘unbox is identity
        // and a round-tripped function keeps its identity (Elm's `==` on
        // functions is reference equality). Otherwise wrap the uniform closure.
        //
        // BUT only when the requested type matches the type the closure was
        // boxed at. When an argument type is an unresolved variable (`Ref`
        // layout), this unbox is happening at a polymorphic boundary that
        // ERASED a concrete type — e.g. `Test.Html`'s `taggerFunction : Tagger
        // -> (a -> msg)` hands back a closure boxed at `Inner -> Outer` but now
        // requested as `va -> Outer`. Recovering the original `Inner`-typed
        // closure and then applying it through the `va` (opaque, un-unboxed)
        // path feeds it a raw uniform word where it expects an unboxed `Inner`,
        // corrupting the value. Keep it as the uniform closure and wrap it, so
        // application routes through the closure's own correct trampoline.
        let has_unresolved_arg = arg_types
            .iter()
            .any(|t| matches!(self.layouts.layout_of(t), Layout::Ref));
        let recovered = self
            .call_named("alm_recover_boxed", &[w])
            .into_int_value();
        let is_rec = if has_unresolved_arg {
            self.ctx.bool_type().const_zero()
        } else {
            self.builder
                .build_int_compare(IntPredicate::NE, recovered, i64_t.const_zero(), "isrec")
                .unwrap()
        };
        let rec_bb = self.new_block("unbox.recover");
        let wrap_bb = self.new_block("unbox.wrap");
        let cont_bb = self.new_block("unbox.cont");
        self.builder.build_conditional_branch(is_rec, rec_bb, wrap_bb).unwrap();

        self.builder.position_at_end(rec_bb);
        let rec_ptr = self.builder.build_int_to_ptr(recovered, ptr_t, "recptr").unwrap();
        self.builder.build_unconditional_branch(cont_bb).unwrap();

        self.builder.position_at_end(wrap_bb);
        let wrapped = self.build_closure_value(lifted.as_global_value().as_pointer_value(), &[w]);
        let wrap_end = self.builder.get_insert_block().unwrap();
        self.builder.build_unconditional_branch(cont_bb).unwrap();

        self.builder.position_at_end(cont_bb);
        let phi = self.builder.build_phi(ptr_t, "unboxed").unwrap();
        phi.add_incoming(&[(&rec_ptr, rec_bb), (&wrapped, wrap_end)]);
        Ok(phi.as_basic_value())
    }

    fn box_list(
        &mut self,
        val: BasicValueEnum<'ctx>,
        tipe: &crate::ast::canonical::Type,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        use crate::ast::canonical::Type;
        let elem_ty = match tipe {
            Type::Type(_, _, args) if args.len() == 1 => args[0].clone(),
            _ => return Err("typed backend: box_list on non-list".to_string()),
        };
        let elem_layout = self.elem_layout(tipe)?;
        let list = val.into_pointer_value();
        let len = self.list_len(list);
        let backing = self.list_backing(list);
        let i64_t = self.ctx.i64_type();
        let ptr_t = self.ctx.ptr_type(inkwell::AddressSpace::default());
        let nil = self.call_named(
            "rt_list",
            &[self.ctx.i32_type().const_zero().into(), ptr_t.const_null().into()],
        );
        let acc_slot = self.entry_alloca(i64_t.into(), "acc");
        self.builder.build_store(acc_slot, nil).unwrap();
        // data[0] is the last element; consing data[0]..data[len-1] onto the
        // accumulator yields the original head-first order.
        self.for_count(len, |s, i| {
            let v = s.list_load(backing, &elem_layout, i);
            let boxed = s.box_value(v, &elem_ty)?;
            let acc = s.builder.build_load(i64_t, acc_slot, "acc").unwrap();
            let acc2 = s.call_named("rt_cons", &[boxed, acc]);
            s.builder.build_store(acc_slot, acc2).unwrap();
            Ok(())
        })?;
        Ok(self.builder.build_load(i64_t, acc_slot, "boxed").unwrap())
    }

    /// Convert a uniform runtime word to an unboxed value of the given type —
    /// the inverse of `box_value`. Used at boundaries where a runtime kernel
    /// returns a uniform value the typed code consumes.
    fn unbox_value(
        &mut self,
        w: BasicValueEnum<'ctx>,
        tipe: &crate::ast::canonical::Type,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        match self.layouts.layout_of(tipe) {
            Layout::Int => Ok(self.call_named("rt_unint", &[w])),
            Layout::Float => Ok(self.call_named("rt_unfloat", &[w])),
            Layout::Bool => Ok(self.call_named("rt_is_true", &[w])),
            Layout::Char => Ok(self.call_named("rt_unchr", &[w])),
            Layout::Str => Ok(w),
            Layout::Unit => Ok(self.ctx.i64_type().const_zero().into()),
            Layout::Enum(_) => Ok(self.call_named("rt_ctor_tag", &[w])),
            Layout::Tuple(_) => self.unbox_tuple(w, tipe),
            Layout::Record(_) => self.unbox_record(w, tipe),
            Layout::List(_) => self.unbox_list(w, tipe),
            // Route through a memoized recursive helper so recursive unions
            // terminate at codegen (mirrors `box_value`).
            Layout::Tagged(_) => {
                let f = self.get_unbox_fn(tipe)?;
                Ok(self
                    .builder
                    .build_call(f, &[w.into()], "unbu")
                    .unwrap()
                    .try_as_basic_value()
                    .left()
                    .unwrap())
            }
            // Opaque values are the uniform word already.
            Layout::Opaque => Ok(w),
            Layout::Ref => Ok(self
                .builder
                .build_int_to_ptr(w.into_int_value(), self.ctx.ptr_type(inkwell::AddressSpace::default()), "ref")
                .unwrap()
                .into()),
            Layout::Closure => self.unbox_closure(w, tipe),
            other => Err(format!("typed backend: cannot unbox layout {:?}", other)),
        }
    }

    fn unbox_tuple(
        &mut self,
        w: BasicValueEnum<'ctx>,
        tipe: &crate::ast::canonical::Type,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        use crate::ast::canonical::Type;
        let subs: Vec<Type> = match tipe {
            Type::Tuple(a, b, c) => {
                let mut v = vec![(**a).clone(), (**b).clone()];
                if let Some(c) = c {
                    v.push((**c).clone());
                }
                v
            }
            _ => return Err("typed backend: unbox_tuple on non-tuple".to_string()),
        };
        let struct_ty = self.llvm_type(&self.layouts.layout_of(tipe)).into_struct_type();
        let mut agg = struct_ty.get_undef();
        for (i, sub) in subs.iter().enumerate() {
            let idx = self.ctx.i32_type().const_int(i as u64, false).into();
            let item = self.call_named("rt_tuple_item", &[w, idx]);
            let tv = self.unbox_value(item, sub)?;
            agg = self.builder.build_insert_value(agg, tv, i as u32, "t").unwrap().into_struct_value();
        }
        Ok(agg.into())
    }

    fn unbox_record(
        &mut self,
        w: BasicValueEnum<'ctx>,
        tipe: &crate::ast::canonical::Type,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        use crate::ast::canonical::Type;
        let field_types = match tipe {
            Type::Record(fields, _) => fields.clone(),
            _ => return Err("typed backend: unbox_record on non-record".to_string()),
        };
        let sorted = match self.layouts.layout_of(tipe) {
            Layout::Record(fs) => fs,
            _ => unreachable!(),
        };
        let struct_ty = self.llvm_type(&self.layouts.layout_of(tipe)).into_struct_type();
        let mut agg = struct_ty.get_undef();
        for (i, (fname, _)) in sorted.iter().enumerate() {
            let field_ty = field_types
                .iter()
                .find(|(n, _)| n == fname)
                .map(|(_, t)| t.clone())
                .ok_or_else(|| format!("typed backend: missing field `{}`", fname))?;
            let nameptr = self.cstr(fname.as_str());
            let fv_u = self.call_named("rt_access", &[w, nameptr.into()]);
            let tv = self.unbox_value(fv_u, &field_ty)?;
            agg = self.builder.build_insert_value(agg, tv, i as u32, "f").unwrap().into_struct_value();
        }
        Ok(agg.into())
    }

    fn unbox_list(
        &mut self,
        w: BasicValueEnum<'ctx>,
        tipe: &crate::ast::canonical::Type,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        use crate::ast::canonical::Type;
        let elem_ty = match tipe {
            Type::Type(_, _, args) if args.len() == 1 => args[0].clone(),
            _ => return Err("typed backend: unbox_list on non-list".to_string()),
        };
        let elem_layout = self.elem_layout(tipe)?;
        let i64_t = self.ctx.i64_type();

        // Walk the uniform list head-first, consing each unboxed element onto
        // a typed accumulator (which reverses), then reverse to restore order.
        let acc_slot = self.entry_alloca(self.ctx.ptr_type(inkwell::AddressSpace::default()).into(), "acc");
        let empty = self.empty_list();
        self.builder.build_store(acc_slot, empty).unwrap();

        let entry = self.builder.get_insert_block().unwrap();
        let loop_bb = self.new_block("unbl.loop");
        let body_bb = self.new_block("unbl.body");
        let done_bb = self.new_block("unbl.done");
        self.builder.build_unconditional_branch(loop_bb).unwrap();
        self.builder.position_at_end(loop_bb);
        let cur = self.builder.build_phi(i64_t, "cur").unwrap();
        cur.add_incoming(&[(&w as &dyn BasicValue, entry)]);
        let cur_v = cur.as_basic_value();
        let is_nil = self.call_named("rt_is_nil", &[cur_v]).into_int_value();
        self.builder.build_conditional_branch(is_nil, done_bb, body_bb).unwrap();

        self.builder.position_at_end(body_bb);
        let head_u = self.call_named("rt_list_head", &[cur_v]);
        let tv = self.unbox_value(head_u, &elem_ty)?;
        let acc = self.builder.build_load(self.ctx.ptr_type(inkwell::AddressSpace::default()), acc_slot, "acc").unwrap().into_pointer_value();
        let consed = self.cons(&elem_layout, tv, acc);
        self.builder.build_store(acc_slot, consed).unwrap();
        let tail = self.call_named("rt_list_tail", &[cur_v]);
        let body_end = self.builder.get_insert_block().unwrap();
        self.builder.build_unconditional_branch(loop_bb).unwrap();
        cur.add_incoming(&[(&tail as &dyn BasicValue, body_end)]);

        self.builder.position_at_end(done_bb);
        let reversed = self.builder.build_load(self.ctx.ptr_type(inkwell::AddressSpace::default()), acc_slot, "rev").unwrap().into_pointer_value();
        Ok(self.emit_reverse(reversed, &elem_layout).into())
    }

    fn unbox_tagged(
        &mut self,
        w: BasicValueEnum<'ctx>,
        tipe: &crate::ast::canonical::Type,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let ctors = self
            .layouts
            .union_ctors(tipe)
            .ok_or_else(|| "typed backend: unknown union to unbox".to_string())?;
        let i32_t = self.ctx.i32_type();
        let ptr_t = self.ctx.ptr_type(inkwell::AddressSpace::default());
        let tag = self.call_named("rt_ctor_tag", &[w]).into_int_value();
        let default_bb = self.new_block("unbt.def");
        let merge = self.new_block("unbt.end");
        let mut incoming: Vec<(BasicValueEnum<'ctx>, inkwell::basic_block::BasicBlock<'ctx>)> =
            Vec::new();
        let blocks: Vec<_> = (0..ctors.len()).map(|k| self.new_block(&format!("unbt.{}", k))).collect();
        let cases: Vec<_> = blocks
            .iter()
            .enumerate()
            .map(|(k, bb)| (i32_t.const_int(k as u64, false), *bb))
            .collect();
        self.builder.build_switch(tag, default_bb, &cases).unwrap();

        for (k, bb) in blocks.iter().enumerate() {
            self.builder.position_at_end(*bb);
            let (_, field_types) = &ctors[k];
            let field_layouts: Vec<Layout> =
                field_types.iter().map(|t| self.layouts.layout_of(t)).collect();
            let mut fields = Vec::new();
            for (i, fty) in field_types.iter().enumerate() {
                let idx = i32_t.const_int(i as u64, false).into();
                let arg_u = self.call_named("rt_ctor_arg", &[w, idx]);
                fields.push(self.unbox_value(arg_u, fty)?);
            }
            let raw: BasicValueEnum = self.alloc_tagged(&field_layouts, k as u32, &fields).into();
            let end = self.builder.get_insert_block().unwrap();
            self.builder.build_unconditional_branch(merge).unwrap();
            incoming.push((raw, end));
        }
        self.builder.position_at_end(default_bb);
        self.builder.build_unreachable().unwrap();

        self.builder.position_at_end(merge);
        let phi = self.builder.build_phi(ptr_t, "union").unwrap();
        for (v, bb) in &incoming {
            phi.add_incoming(&[(v as &dyn BasicValue, *bb)]);
        }
        Ok(phi.as_basic_value())
    }

    fn call_named(&self, name: &str, args: &[BasicValueEnum<'ctx>]) -> BasicValueEnum<'ctx> {
        let f = self.module.get_function(name).unwrap();
        let argv: Vec<_> = args.iter().map(|a| (*a).into()).collect();
        self.builder
            .build_call(f, &argv, "rt")
            .unwrap()
            .try_as_basic_value()
            .left()
            .unwrap()
    }

    fn gen(&mut self, expr: &TypedExpr) -> Result<BasicValueEnum<'ctx>, String> {
        self.set_loc(expr.region);
        match &expr.kind {
            TypedKind::Int(n) => {
                // A number literal used at Float type must be an f64 constant
                // (Elm number literals are polymorphic).
                if matches!(self.layouts.layout_of(&expr.tipe), Layout::Float) {
                    Ok(self.ctx.f64_type().const_float(*n as f64).into())
                } else {
                    Ok(self.ctx.i64_type().const_int(*n as u64, true).into())
                }
            }
            TypedKind::Float(f) => Ok(self.ctx.f64_type().const_float(*f).into()),
            TypedKind::Str(s) => self.gen_string(s),
            // A character literal is its Unicode scalar in an i32 (the `Char`
            // layout).
            TypedKind::Chr(c) => Ok(self.ctx.i32_type().const_int(*c as u64, false).into()),
            TypedKind::Unit => Ok(self.ctx.i64_type().const_zero().into()),
            TypedKind::Local(name) => self
                .locals
                .get(name.as_str())
                .copied()
                .ok_or_else(|| format!("typed backend: unbound local `{}`", name)),
            TypedKind::Global(name) => {
                let f = *self
                    .functions
                    .get(&name.to_string())
                    .ok_or_else(|| format!("typed backend: unknown global `{}`", name))?;
                // A function used as a first-class value becomes a closure;
                // a zero-argument value is called to produce its value.
                if matches!(expr.tipe, crate::ast::canonical::Type::Lambda(..)) {
                    Ok(self.wrap_global(&name.to_string(), &expr.tipe).into())
                } else {
                    Ok(self
                        .builder
                        .build_call(f, &[], "v")
                        .unwrap()
                        .try_as_basic_value()
                        .left()
                        .unwrap())
                }
            }
            TypedKind::Call(func, args) => self.gen_call(expr, func, args),
            TypedKind::List(items) => self.gen_list(expr, items),
            TypedKind::Binop(op, _, _, l, r)
                if op.as_str() == "|>" || op.as_str() == "<|" =>
            {
                self.gen_pipe(expr, op.as_str(), l, r)
            }
            TypedKind::Binop(op, _, _, l, r) if op.as_str() == "<<" || op.as_str() == ">>" => {
                self.gen_compose(l, r, expr, op.as_str() == "<<")
            }
            TypedKind::Binop(op, _, _, l, r) if op.as_str() == "::" => self.gen_cons(expr, l, r),
            TypedKind::Binop(op, _, _, l, r) if op.as_str() == "++" => self.gen_append(l, r),
            TypedKind::Binop(op, _, _, l, r) if op.as_str() == "&&" || op.as_str() == "||" => {
                self.gen_and_or(op.as_str(), l, r)
            }
            TypedKind::Binop(op, _, _, l, r) if op.as_str() == "==" => {
                self.gen_equals(l, r, false)
            }
            TypedKind::Binop(op, _, _, l, r) if op.as_str() == "/=" => {
                self.gen_equals(l, r, true)
            }
            TypedKind::Binop(op, _, _, l, r) => self.gen_binop(op.as_str(), l, r),
            TypedKind::If(branches, otherwise) => self.gen_if(branches, otherwise, expr),
            TypedKind::Let(decls, body) => self.gen_let(decls, body),
            TypedKind::Case(scrutinee, branches) => self.gen_case(scrutinee, branches, expr),
            TypedKind::Negate(inner) => self.gen_negate(inner),
            TypedKind::Tuple(a, b, c) => self.gen_tuple(expr, a, b, c.as_deref()),
            TypedKind::Record(fields) => self.gen_record(expr, fields),
            TypedKind::Access(record, field) => self.gen_access(record, field.as_str()),
            TypedKind::Update(record, fields) => self.gen_update(record, fields),
            TypedKind::Ctor(home, union, ctor) => self.gen_ctor(expr, home, union, ctor),
            // A built-in used as a bare value. If its type is a function
            // (e.g. `String.fromInt` passed to `List.map`), eta-expand it into
            // a closure `\x.. -> f x..` so the closure machinery can pass it
            // around; otherwise it is a nullary kernel (e.g. `Cmd.none`).
            TypedKind::Foreign(module, name) => {
                if matches!(expr.tipe, crate::ast::canonical::Type::Lambda(..)) {
                    self.eta_expand_foreign(module, name, &expr.tipe, &[], expr.region)
                } else {
                    self.gen_kernel(expr, module.as_str(), name.as_str(), &[])
                }
            }
            TypedKind::Lambda(params, body) => {
                let result_layout = self.layouts.layout_of(&body.tipe);
                self.gen_closure(params, body, &result_layout)
            }
            // A record accessor `.field` is the function `\r -> r.field`.
            TypedKind::Accessor(field) => {
                use crate::ast::canonical::Type;
                let (rec_ty, field_ty) = match &expr.tipe {
                    Type::Lambda(a, b) => ((**a).clone(), (**b).clone()),
                    other => {
                        return Err(format!(
                            "typed backend: accessor has non-function type {:?}",
                            other
                        ))
                    }
                };
                let rname = format!("$acc{}", self.lam_id);
                let r_local = TypedExpr {
                    tipe: rec_ty.clone(),
                    kind: TypedKind::Local(crate::data::Name::from(rname.clone())),
                    region: expr.region,
                };
                let mut body = TypedExpr {
                    tipe: field_ty.clone(),
                    kind: TypedKind::Access(Box::new(r_local), field.clone()),
                    region: expr.region,
                };
                let mk_pat = |name: String| crate::reporting::Located {
                    region: crate::reporting::Region::ZERO,
                    value: crate::ast::canonical::Pattern_::Var(crate::data::Name::from(name)),
                };
                let mut params = vec![(mk_pat(rname), rec_ty)];
                // Eta-expand over the FIELD type's arrows. Closure application
                // assumes a closure of type `T1 -> .. -> Tn -> R` is flat n-ary
                // (one parameter per arrow); when the accessed field is itself a
                // function (`.addPerson : rec -> s -> s`), a bare 1-parameter
                // accessor closure would be called with all n arguments at once
                // and return the field's closure as if it were the final result.
                // `\r p1 .. -> (r.field) p1 ..` restores the invariant.
                if let Type::Lambda(..) = &field_ty {
                    let mut eta_args = Vec::new();
                    let mut remaining = field_ty.clone();
                    let mut idx = 0u32;
                    while let Type::Lambda(a, b) = remaining {
                        let pname = format!("$accp{}_{}", self.lam_id, idx);
                        eta_args.push(TypedExpr {
                            tipe: (*a).clone(),
                            kind: TypedKind::Local(crate::data::Name::from(pname.clone())),
                            region: expr.region,
                        });
                        params.push((mk_pat(pname), (*a).clone()));
                        remaining = *b;
                        idx += 1;
                    }
                    body = TypedExpr {
                        tipe: remaining,
                        kind: TypedKind::Call(Box::new(body), eta_args),
                        region: expr.region,
                    };
                }
                let result_layout = self.layouts.layout_of(&body.tipe);
                self.gen_closure(&params, &body, &result_layout)
            }
        }
    }

    fn gen_call(
        &mut self,
        whole: &TypedExpr,
        func: &TypedExpr,
        args: &[TypedExpr],
    ) -> Result<BasicValueEnum<'ctx>, String> {
        // A saturated constructor application builds a tagged heap value; a
        // partial one (result type still a function) is eta-expanded into a
        // closure that constructs once the remaining arguments arrive.
        if let TypedKind::Ctor(home, union, ctor) = &func.kind {
            if matches!(whole.tipe, crate::ast::canonical::Type::Lambda(..)) {
                return self.gen_ctor_partial(whole, home, union, ctor, args);
            }
            return self.gen_ctor_apply(whole, ctor, args);
        }
        // A built-in call becomes a generated, type-specialized kernel — but
        // only when saturated. A builtin applied to fewer than its arity of
        // arguments (e.g. `modBy 2` passed to `List.map`) is eta-expanded into
        // a closure that takes the remaining arguments.
        if let TypedKind::Foreign(module, name) = &func.kind {
            let arity = foreign_intrinsic_arity(module.as_str(), name.as_str())
                .unwrap_or_else(|| foreign_arity(&func.tipe));
            if args.len() < arity {
                return self.eta_expand_foreign(module, name, &func.tipe, args, whole.region);
            }
            if args.len() > arity {
                // Over-application: the builtin returns a function that is
                // applied further in the same flattened call —
                // `(List.foldl (>>) identity fs) x`. Passing all the arguments
                // to gen_kernel would make it read `whole.tipe` (the FINAL
                // type) as the kernel's result type — e.g. a fold whose
                // accumulator is a closure laid out as the final Int. Call the
                // kernel at its own arity and result type, then apply the
                // returned closure to the rest.
                let mut mid_ty = func.tipe.clone();
                for _ in 0..arity {
                    match mid_ty {
                        crate::ast::canonical::Type::Lambda(_, r) => mid_ty = *r,
                        _ => break,
                    }
                }
                let mid = TypedExpr {
                    tipe: mid_ty.clone(),
                    kind: whole.kind.clone(),
                    region: whole.region,
                };
                let closure = self
                    .gen_kernel(&mid, module.as_str(), name.as_str(), &args[..arity])?
                    .into_pointer_value();
                let mut rest = Vec::with_capacity(args.len() - arity);
                for arg in &args[arity..] {
                    rest.push(self.gen(arg)?);
                }
                return Ok(self.apply_closure_curried(closure, &rest, &mid_ty));
            }
            return self.gen_kernel(whole, module.as_str(), name.as_str(), args);
        }
        // A direct call to a named function, when the argument count matches
        // the function's arity.
        if let TypedKind::Global(name) = &func.kind {
            let f = *self
                .functions
                .get(&name.to_string())
                .ok_or_else(|| format!("typed backend: unknown call target `{}`", name))?;
            let np = f.count_params() as usize;
            if np == args.len() {
                let mut argv = Vec::with_capacity(args.len());
                for arg in args {
                    argv.push(self.gen(arg)?);
                }
                // call_fn widens i64 args to double params (unresolved
                // `number` literals defaulted to Int at a Float call site).
                return Ok(self.call_fn(f, &argv));
            }
            if args.len() < np {
                // Partial application: capture the given arguments in a closure
                // that takes the rest.
                let mut applied = Vec::with_capacity(args.len());
                for arg in args {
                    applied.push(self.gen(arg)?);
                }
                return Ok(self.gen_partial_app(f, &applied));
            }
            // Over-application: a point-free (or under-arity) function whose
            // result is itself a closure applied to further arguments — e.g.
            // `encode = Elm.Kernel.Bytes.encode` called as `encode x`, or
            // `signedInt32 = I32` called as `signedInt32 BE n`. Call the
            // function with its own parameters, then apply the returned closure
            // to the remaining arguments.
            let mut evaled = Vec::with_capacity(args.len());
            for arg in args {
                evaled.push(self.gen(arg)?);
            }
            let closure = self.call_fn(f, &evaled[..np]).into_pointer_value();
            // The closure returned by the point-free (or under-arity) function
            // has the callee's type with its `np` compiled parameters peeled
            // off. Apply the remaining arguments with currying so an
            // under-saturated application builds a partial closure rather than
            // calling with too few arguments.
            let mut closure_ty = func.tipe.clone();
            for _ in 0..np {
                match closure_ty {
                    crate::ast::canonical::Type::Lambda(_, r) => closure_ty = *r,
                    _ => break,
                }
            }
            return Ok(self.apply_closure_curried(closure, &evaled[np..], &closure_ty));
        }

        // A directly-applied lambda literal — including a *partial* application
        // (fewer arguments than the lambda's parameters, e.g. the `\f (a,b) ->
        // f a b` half of a composed function applied to one argument). Route it
        // through `apply_fn_expr`, which inlines a saturated lambda and builds a
        // partial-application closure when under-applied.
        if let TypedKind::Lambda(..) = &func.kind {
            let mut argv = Vec::with_capacity(args.len());
            for arg in args {
                argv.push(self.gen(arg)?);
            }
            return self.apply_fn_expr(func, &argv);
        }

        // Otherwise the callee is a closure value (a function-typed local,
        // the result of another call, a field, …): apply it indirectly, with
        // currying so an under-saturated application builds a partial closure.
        let closure = self.gen(func)?.into_pointer_value();
        let mut argv = Vec::with_capacity(args.len());
        for arg in args {
            argv.push(self.gen(arg)?);
        }
        Ok(self.apply_closure_curried(closure, &argv, &func.tipe))
    }

    /// Build a data-carrying constructor: allocate `{tag, fields}` on the
    /// heap, store the tag and each argument, and yield the pointer.
    fn gen_ctor_apply(
        &mut self,
        whole: &TypedExpr,
        ctor: &crate::ast::canonical::Ctor,
        args: &[TypedExpr],
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let field_layouts = match self.layouts.layout_of(&whole.tipe) {
            Layout::Tagged(variants) => variants
                .get(ctor.index as usize)
                .cloned()
                .ok_or_else(|| format!("typed backend: bad ctor index for `{}`", ctor.name))?,
            other => {
                return Err(format!(
                    "typed backend: applying `{}` to layout {:?} is not supported",
                    ctor.name, other
                ))
            }
        };
        let mut fields = Vec::with_capacity(args.len());
        for arg in args {
            fields.push(self.gen(arg)?);
        }
        Ok(self.alloc_tagged(&field_layouts, ctor.index, &fields).into())
    }

    /// Allocate a heap `{i32 tag, fields...}` block and store the tag and
    /// field values. Shared by constructor application and Maybe-returning
    /// kernels.
    fn alloc_tagged(
        &self,
        field_layouts: &[Layout],
        tag: u32,
        fields: &[BasicValueEnum<'ctx>],
    ) -> inkwell::values::PointerValue<'ctx> {
        let struct_ty = self.ctor_struct(field_layouts);
        let size = struct_ty.size_of().unwrap();
        let alloc = self.module.get_function("alm_alloc").unwrap();
        let raw = self
            .builder
            .build_call(alloc, &[size.into()], "box")
            .unwrap()
            .try_as_basic_value()
            .left()
            .unwrap()
            .into_pointer_value();
        let tag_ptr = self.builder.build_struct_gep(struct_ty, raw, 0, "tagp").unwrap();
        self.builder
            .build_store(tag_ptr, self.ctx.i32_type().const_int(tag as u64, false))
            .unwrap();
        for (i, v) in fields.iter().enumerate() {
            let fp = self
                .builder
                .build_struct_gep(struct_ty, raw, (i + 1) as u32, "fp")
                .unwrap();
            self.builder.build_store(fp, *v).unwrap();
        }
        raw
    }

    fn gen_let(
        &mut self,
        decls: &[crate::ir::mono::TypedLetDecl],
        body: &TypedExpr,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        self.bind_let_decls(decls)?;
        self.gen(body)
    }

    /// Whether a closed `let` value is worth memoizing: recomputing it does real
    /// allocation or work. True when the result is a heap structure (list,
    /// string, record, union, dict/array, closure) or the body calls a function
    /// or builds an aggregate — as opposed to a scalar leaf that LLVM folds and
    /// that the memo load/branch would only slow down.
    fn allocating_const(&self, expr: &TypedExpr) -> bool {
        use crate::ir::mono::TypedKind::*;
        if matches!(
            &expr.kind,
            Str(_) | Ctor(..) | List(_) | Call(..) | Case(..) | Record(_) | Tuple(..) | Update(..)
        ) {
            return true;
        }
        // A pipe/compose is a `Binop` in the IR; judge it (and any other node)
        // by whether it yields a heap-allocated value worth caching.
        matches!(
            self.layouts.layout_of(&expr.tipe),
            Layout::List(_)
                | Layout::Str
                | Layout::Record(_)
                | Layout::Tagged(_)
                | Layout::Ref
                | Layout::Opaque
                | Layout::Closure
        )
    }

    /// Compute a closed (argument-independent) `let` value at most once for the
    /// whole program, caching it in a module global behind a "done" flag — the
    /// same memoization as a top-level nullary value, applied to a constant
    /// found inside a function. The body runs on the first execution and every
    /// later one loads the cached value. Sound only for closed bodies (checked
    /// by the caller): they reference no locals, so the cached value is valid
    /// for every call.
    fn emit_hoisted_const(&mut self, body: &TypedExpr) -> Result<BasicValueEnum<'ctx>, String> {
        let ret_ty = self.llvm_type(&self.layouts.layout_of(&body.tipe));
        let id = self.lam_id;
        self.lam_id += 1;
        let memo = self.module.add_global(ret_ty, None, &format!("hoist.{}$memo", id));
        memo.set_initializer(&ret_ty.const_zero());
        memo.set_linkage(Linkage::Internal);
        let bool_t = self.ctx.bool_type();
        let done = self.module.add_global(bool_t, None, &format!("hoist.{}$done", id));
        done.set_initializer(&bool_t.const_zero());
        done.set_linkage(Linkage::Internal);

        let cached_bb = self.new_block("hoist.cached");
        let compute_bb = self.new_block("hoist.compute");
        let cont_bb = self.new_block("hoist.cont");
        let done_v = self
            .builder
            .build_load(bool_t, done.as_pointer_value(), "hdone")
            .unwrap()
            .into_int_value();
        self.builder
            .build_conditional_branch(done_v, cached_bb, compute_bb)
            .unwrap();

        self.builder.position_at_end(cached_bb);
        let cv = self.builder.build_load(ret_ty, memo.as_pointer_value(), "hmemo").unwrap();
        self.builder.build_unconditional_branch(cont_bb).unwrap();
        let cached_end = self.builder.get_insert_block().unwrap();

        self.builder.position_at_end(compute_bb);
        // The body is closed, so it needs no enclosing locals.
        let val = self.gen(body)?;
        self.builder.build_store(memo.as_pointer_value(), val).unwrap();
        self.builder
            .build_store(done.as_pointer_value(), bool_t.const_int(1, false))
            .unwrap();
        self.builder.build_unconditional_branch(cont_bb).unwrap();
        let compute_end = self.builder.get_insert_block().unwrap();

        self.builder.position_at_end(cont_bb);
        let phi = self.builder.build_phi(ret_ty, "hoisted").unwrap();
        phi.add_incoming(&[(&cv, cached_end), (&val, compute_end)]);
        Ok(phi.as_basic_value())
    }

    /// Bind a `let` block's declarations into the local scope (shared by
    /// [`gen_let`] and the tail-position path).
    fn bind_let_decls(&mut self, decls: &[crate::ir::mono::TypedLetDecl]) -> Result<(), String> {
        use crate::ir::mono::TypedLetDecl::*;
        for decl in decls {
            match decl {
                Def { name, params, body } if params.is_empty() => {
                    // A `let` value whose right-hand side references none of the
                    // enclosing function's arguments (or any local) is a
                    // constant relative to every call — the same generalization
                    // as a top-level nullary value, but reached inside a
                    // function. If it also allocates (builds a structure or
                    // calls a function), computing it on every call — every loop
                    // iteration, even — is wasteful and, under the non-freeing
                    // allocator, unbounded. Hoist it to a memoized global so the
                    // work happens at most once for the whole program. (A closed
                    // scalar/leaf is left inline: LLVM folds it and the memo
                    // machinery would only add overhead.)
                    if self.allocating_const(body) && is_closed(body) {
                        let v = self.emit_hoisted_const(body)?;
                        self.locals.insert(name.to_string(), v);
                    } else {
                        let v = self.gen(body)?;
                        self.locals.insert(name.to_string(), v);
                    }
                }
                // A non-recursive local function `let f x = e`: closure-convert
                // it like a lambda (capturing outer locals) and bind the closure.
                Def { name, params, body } => {
                    let result_layout = self.layouts.layout_of(&body.tipe);
                    let clos = self.gen_closure(params, body, &result_layout)?;
                    self.locals.insert(name.to_string(), clos);
                }
                Destruct(pattern, value) => {
                    let v = self.gen(value)?;
                    let layout = self.layouts.layout_of(&value.tipe);
                    self.bind_pattern(pattern, v, &layout)?;
                }
                Recursive(group) => self.gen_rec_group(group)?,
            }
        }
        Ok(())
    }

    /// Generate a group of mutually-recursive local definitions. A singleton
    /// group is a self-recursive function, compiled with `gen_closure_named` so
    /// its body reaches itself through the environment pointer. Larger groups
    /// (genuine mutual recursion) are not yet supported by the typed backend.
    fn gen_rec_group(
        &mut self,
        group: &[crate::ir::mono::TypedLetDecl],
    ) -> Result<(), String> {
        use crate::ir::mono::TypedLetDecl::*;
        match group {
            [Def { name, params, body }] if !params.is_empty() => {
                let result_layout = self.layouts.layout_of(&body.tipe);
                let clos =
                    self.gen_closure_named(params, body, &result_layout, Some(name.as_str()))?;
                self.locals.insert(name.to_string(), clos);
                Ok(())
            }
            // A recursive *value* (e.g. `x = 1 :: x`) — not a function. Rare and
            // not modelled by the typed backend.
            [Def { params, .. }] if params.is_empty() => Err(
                "typed backend: recursive local value bindings are not supported".to_string(),
            ),
            _ => self.gen_mutual_rec_group(group),
        }
    }

    /// Generate a group of N mutually-recursive local functions. Each member is
    /// lifted to a function `lam_i(env, params...)`. Every member's closure
    /// environment carries one slot for *every* member of the group (so any
    /// member can reach any other), followed by that member's external
    /// captures. The closures are heap-allocated first and their group slots
    /// backpatched with the other members' pointers afterwards — an
    /// allocate-then-backpatch that ties the mutual-recursion knot.
    fn gen_mutual_rec_group(
        &mut self,
        group: &[crate::ir::mono::TypedLetDecl],
    ) -> Result<(), String> {
        use crate::ir::mono::TypedLetDecl::Def;
        let n = group.len();

        // Group member names, in a fixed order that indexes every env's slots.
        let mut names: Vec<String> = Vec::with_capacity(n);
        for d in group {
            match d {
                Def { name, params, .. } if !params.is_empty() => names.push(name.to_string()),
                Def { name, .. } => {
                    return Err(format!(
                        "typed backend: mutually-recursive local value bindings are \
                         not supported (`{}` in a recursive group)",
                        name
                    ))
                }
                _ => {
                    return Err(
                        "typed backend: unexpected non-function in a recursive group".to_string()
                    )
                }
            }
        }

        struct Member<'ctx> {
            name: String,
            params: Vec<(crate::ast::canonical::Pattern, crate::ast::canonical::Type)>,
            body: TypedExpr,
            param_names: Vec<String>,
            externals: Vec<(String, BasicValueEnum<'ctx>)>,
            result_layout: Layout,
        }

        // Gather each member, desugaring destructuring parameters and computing
        // the external captures (free variables that are neither parameters nor
        // group members) against the *current* locals.
        let mut members: Vec<Member<'ctx>> = Vec::with_capacity(n);
        for d in group {
            let Def { name, params, body } = d else { unreachable!() };
            let (params, body) = if params.iter().any(|(p, _)| simple_param_name(p).is_none()) {
                desugar_destructuring_params(&mut self.fresh_id, params, body)
            } else {
                (params.clone(), body.clone())
            };
            let mut param_names = Vec::with_capacity(params.len());
            for (p, _) in &params {
                match simple_param_name(p) {
                    Some(nm) => param_names.push(nm),
                    None => {
                        return Err("typed backend: destructuring parameters in a \
                                    mutually-recursive group are not supported"
                            .to_string())
                    }
                }
            }
            let mut bound: std::collections::HashSet<String> =
                param_names.iter().cloned().collect();
            for g in &names {
                bound.insert(g.clone());
            }
            let mut refs = Vec::new();
            free_vars(&body, &mut bound, &mut refs);
            let externals: Vec<(String, BasicValueEnum<'ctx>)> = refs
                .iter()
                .filter_map(|nm| self.locals.get(nm).map(|v| (nm.clone(), *v)))
                .collect();
            let result_layout = self.layouts.layout_of(&body.tipe);
            members.push(Member {
                name: name.to_string(),
                params,
                body,
                param_names,
                externals,
                result_layout,
            });
        }

        let ptr_t = self.ctx.ptr_type(inkwell::AddressSpace::default());

        // The closure struct type of each member: {fn_ptr, group slots.., external
        // capture types..}. Group slots are all pointers.
        let clos_tys: Vec<inkwell::types::StructType<'ctx>> = members
            .iter()
            .map(|m| {
                let mut fields: Vec<BasicTypeEnum> = vec![ptr_t.into()];
                for _ in 0..n {
                    fields.push(ptr_t.into());
                }
                for (_, v) in &m.externals {
                    fields.push(v.get_type());
                }
                self.ctx.struct_type(&fields, false)
            })
            .collect();

        // Phase 1: emit each member's lifted function body.
        let mut lifted_fns: Vec<FunctionValue<'ctx>> = Vec::with_capacity(n);
        for (i, m) in members.iter().enumerate() {
            let clos_ty = clos_tys[i];
            let ret_ty = self.llvm_type(&m.result_layout);
            let mut fn_params: Vec<BasicMetadataTypeEnum> = vec![ptr_t.into()];
            for (_, t) in &m.params {
                fn_params.push(self.llvm_type(&self.layouts.layout_of(t)).into());
            }
            let fn_ty = ret_ty.fn_type(&fn_params, false);
            let fname = format!("lam.{}", self.lam_id);
            self.lam_id += 1;
            let lifted = self.module.add_function(&fname, fn_ty, Some(Linkage::Internal));
            lifted_fns.push(lifted);

            let saved_locals = std::mem::take(&mut self.locals);
            let saved_fn = self.cur_fn;
            let saved_block = self.builder.get_insert_block();
            let saved_loc = self.cur_loc;
            self.cur_fn = Some(lifted);
            let entry = self.ctx.append_basic_block(lifted, "entry");
            self.builder.position_at_end(entry);
            if let Some(file) = self.cur_file {
                let line = m.body.region.start.row;
                let sp = self.di_builder.create_function(
                    file.as_debug_info_scope(),
                    &fname,
                    Some(&fname),
                    file,
                    line,
                    self.di_subroutine,
                    true,
                    true,
                    line,
                    DIFlags::ZERO,
                    true,
                );
                lifted.set_subprogram(sp);
            }
            self.clear_loc();

            let env = lifted.get_nth_param(0).unwrap().into_pointer_value();
            // Bind every group member from its env slot (a closure pointer).
            for (j, gname) in names.iter().enumerate() {
                let fp = self
                    .builder
                    .build_struct_gep(clos_ty, env, (j + 1) as u32, "grp")
                    .unwrap();
                let v = self.builder.build_load(ptr_t, fp, gname).unwrap();
                self.locals.insert(gname.clone(), v);
            }
            // Bind the external captures.
            for (k, (extname, extval)) in m.externals.iter().enumerate() {
                let fp = self
                    .builder
                    .build_struct_gep(clos_ty, env, (n + k + 1) as u32, "capp")
                    .unwrap();
                let v = self.builder.build_load(extval.get_type(), fp, extname).unwrap();
                self.locals.insert(extname.clone(), v);
            }
            // Bind the parameters.
            for (idx, pn) in m.param_names.iter().enumerate() {
                let val = lifted.get_nth_param((idx + 1) as u32).unwrap();
                self.locals.insert(pn.clone(), val);
            }

            let body_result = self.gen(&m.body);
            self.locals = saved_locals;
            self.cur_fn = saved_fn;
            let ret = body_result?;
            self.builder.build_return(Some(&ret)).unwrap();
            if let Some(b) = saved_block {
                self.builder.position_at_end(b);
            }
            self.restore_loc(saved_loc);
        }

        // Phase 2: allocate the closures, store fn pointers and external
        // captures, then backpatch the group slots with the sibling pointers.
        let alloc = self.module.get_function("alm_alloc").unwrap();
        let mut raws: Vec<inkwell::values::PointerValue<'ctx>> = Vec::with_capacity(n);
        for (i, m) in members.iter().enumerate() {
            let clos_ty = clos_tys[i];
            let raw = self
                .builder
                .build_call(alloc, &[clos_ty.size_of().unwrap().into()], "rclos")
                .unwrap()
                .try_as_basic_value()
                .left()
                .unwrap()
                .into_pointer_value();
            let f0 = self.builder.build_struct_gep(clos_ty, raw, 0, "fnp").unwrap();
            self.builder
                .build_store(f0, lifted_fns[i].as_global_value().as_pointer_value())
                .unwrap();
            for (k, (_, extval)) in m.externals.iter().enumerate() {
                let fp = self
                    .builder
                    .build_struct_gep(clos_ty, raw, (n + k + 1) as u32, "cap")
                    .unwrap();
                self.builder.build_store(fp, *extval).unwrap();
            }
            raws.push(raw);
        }
        for i in 0..n {
            let clos_ty = clos_tys[i];
            for j in 0..n {
                let fp = self
                    .builder
                    .build_struct_gep(clos_ty, raws[i], (j + 1) as u32, "grpp")
                    .unwrap();
                self.builder.build_store(fp, raws[j]).unwrap();
            }
        }
        for (i, m) in members.iter().enumerate() {
            self.locals.insert(m.name.clone(), raws[i].into());
        }
        Ok(())
    }

    fn gen_tuple(
        &mut self,
        whole: &TypedExpr,
        a: &TypedExpr,
        b: &TypedExpr,
        c: Option<&TypedExpr>,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let struct_ty = self
            .llvm_type(&self.layouts.layout_of(&whole.tipe))
            .into_struct_type();
        let mut agg = struct_ty.get_undef();
        let mut vals = vec![self.gen(a)?, self.gen(b)?];
        if let Some(c) = c {
            vals.push(self.gen(c)?);
        }
        for (i, v) in vals.into_iter().enumerate() {
            let v = self.coerce_to_slot(v, struct_ty.get_field_type_at_index(i as u32));
            agg = self
                .builder
                .build_insert_value(agg, v, i as u32, "tup")
                .unwrap()
                .into_struct_value();
        }
        Ok(agg.into())
    }

    /// Widen an `i64` to `f64` when the aggregate slot is a float — a
    /// polymorphic number literal in a float position (`( x, y, 0 )` where the
    /// tuple is `(Float, Float, Float)`) is generated as an `Int` by `layout_of`
    /// (see `to_float`), and inserting it into an `f64` slot would otherwise
    /// build a struct whose layout disagrees between `case` branches.
    fn coerce_to_slot(
        &self,
        v: BasicValueEnum<'ctx>,
        slot: Option<BasicTypeEnum<'ctx>>,
    ) -> BasicValueEnum<'ctx> {
        if matches!(slot, Some(t) if t.is_float_type()) && v.is_int_value() {
            self.to_float(v).into()
        } else {
            v
        }
    }

    fn gen_record(
        &mut self,
        whole: &TypedExpr,
        fields: &[(crate::data::Name, TypedExpr)],
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let order = self.record_fields(&whole.tipe)?;
        let struct_ty = self
            .llvm_type(&self.layouts.layout_of(&whole.tipe))
            .into_struct_type();
        let mut agg = struct_ty.get_undef();
        for (name, value) in fields {
            let idx = order
                .iter()
                .position(|n| n == name.as_str())
                .ok_or_else(|| format!("typed backend: record has no field `{}`", name))?;
            let v = self.gen(value)?;
            let v = self.coerce_to_slot(v, struct_ty.get_field_type_at_index(idx as u32));
            agg = self
                .builder
                .build_insert_value(agg, v, idx as u32, "rec")
                .unwrap()
                .into_struct_value();
        }
        Ok(agg.into())
    }

    fn gen_access(
        &mut self,
        record: &TypedExpr,
        field: &str,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let order = self.record_fields(&record.tipe)?;
        let idx = order
            .iter()
            .position(|n| n == field)
            .ok_or_else(|| format!("typed backend: record has no field `{}`", field))?;
        let sv = self.gen(record)?.into_struct_value();
        Ok(self
            .builder
            .build_extract_value(sv, idx as u32, "field")
            .unwrap())
    }

    fn gen_update(
        &mut self,
        record: &TypedExpr,
        fields: &[(crate::data::Name, TypedExpr)],
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let order = self.record_fields(&record.tipe)?;
        let mut agg = self.gen(record)?.into_struct_value();
        for (name, value) in fields {
            let idx = order
                .iter()
                .position(|n| n == name.as_str())
                .ok_or_else(|| format!("typed backend: record has no field `{}`", name))?;
            let v = self.gen(value)?;
            agg = self
                .builder
                .build_insert_value(agg, v, idx as u32, "upd")
                .unwrap()
                .into_struct_value();
        }
        Ok(agg.into())
    }

    /// Bind a pattern's variables to (parts of) a value, guided by the
    /// value's layout. Supports variables, wildcards, aliases, and tuple and
    /// record destructuring of unboxed structs.
    fn bind_pattern(
        &mut self,
        pattern: &crate::ast::canonical::Pattern,
        value: BasicValueEnum<'ctx>,
        layout: &Layout,
    ) -> Result<(), String> {
        use crate::ast::canonical::Pattern_::*;
        match &pattern.value {
            Var(name) => {
                self.locals.insert(name.to_string(), value);
                Ok(())
            }
            Anything => Ok(()),
            Alias(inner, name) => {
                self.locals.insert(name.value.to_string(), value);
                self.bind_pattern(inner, value, layout)
            }
            Tuple(a, b, rest) => {
                let Layout::Tuple(elem_layouts) = layout else {
                    return Err("typed backend: tuple pattern on non-tuple value".to_string());
                };
                let sv = value.into_struct_value();
                let parts: Vec<&crate::ast::canonical::Pattern> = std::iter::once(a.as_ref())
                    .chain(std::iter::once(b.as_ref()))
                    .chain(rest.iter())
                    .collect();
                for (i, p) in parts.into_iter().enumerate() {
                    let elem = self.builder.build_extract_value(sv, i as u32, "elt").unwrap();
                    self.bind_pattern(p, elem, &elem_layouts[i])?;
                }
                Ok(())
            }
            Record(field_names) => {
                let Layout::Record(fields) = layout else {
                    return Err("typed backend: record pattern on non-record value".to_string());
                };
                let sv = value.into_struct_value();
                for located in field_names {
                    let idx = fields
                        .iter()
                        .position(|(n, _)| n.as_str() == located.value.as_str())
                        .ok_or_else(|| {
                            format!("typed backend: record has no field `{}`", located.value)
                        })?;
                    let elem = self
                        .builder
                        .build_extract_value(sv, idx as u32, "field")
                        .unwrap();
                    self.locals.insert(located.value.to_string(), elem);
                }
                Ok(())
            }
            Unit => Ok(()),
            // An irrefutable single-constructor destructure `let (Ctor a b) = e`
            // (e.g. unwrapping a newtype-style union such as `Fuzzer f`): read
            // the constructor's fields out of the heap block and bind them.
            Ctor(_, _, ctor, args) => {
                if args.is_empty() {
                    return Ok(());
                }
                let Layout::Tagged(variants) = layout else {
                    return Err(
                        "typed backend: constructor destructure on non-tagged value".to_string(),
                    );
                };
                let field_layouts = variants
                    .get(ctor.index as usize)
                    .cloned()
                    .ok_or_else(|| format!("typed backend: bad ctor index for `{}`", ctor.name))?;
                let struct_ty = self.ctor_struct(&field_layouts);
                let ptr = value.into_pointer_value();
                for (i, argpat) in args.iter().enumerate() {
                    let fp = self
                        .builder
                        .build_struct_gep(struct_ty, ptr, (i + 1) as u32, "fp")
                        .unwrap();
                    let v = self
                        .builder
                        .build_load(self.llvm_type(&field_layouts[i]), fp, "fld")
                        .unwrap();
                    self.bind_pattern(argpat, v, &field_layouts[i])?;
                }
                Ok(())
            }
            _ => Err("typed backend: unsupported destructuring pattern".to_string()),
        }
    }

    /// Construct a nullary constructor value: a `Bool` bit or an enum tag.
    /// (Data-carrying constructors are handled where they are applied.)
    fn gen_ctor(
        &mut self,
        whole: &TypedExpr,
        home: &crate::data::Name,
        union: &crate::data::Name,
        ctor: &crate::ast::canonical::Ctor,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        match self.layouts.layout_of(&whole.tipe) {
            Layout::Bool => Ok(self
                .ctx
                .bool_type()
                .const_int((ctor.name.as_str() == "True") as u64, false)
                .into()),
            Layout::Enum(_) => Ok(self
                .ctx
                .i32_type()
                .const_int(ctor.index as u64, false)
                .into()),
            // A nullary constructor of a data-carrying union (e.g. Nothing in
            // Maybe): a heap {tag} block with no fields.
            Layout::Tagged(_) => self.gen_ctor_apply(whole, ctor, &[]),
            // A constructor used as a function value (e.g. `Tick` passed to
            // Time.every): desugar to `\a.. -> Ctor a..` and closure-convert.
            Layout::Closure => self.gen_ctor_as_function(whole, home, union, ctor),
            other => Err(format!(
                "typed backend: constructing `{}` (layout {:?}) is not supported yet",
                ctor.name, other
            )),
        }
    }

    /// A constructor used as a first-class function: build `\a0..an -> Ctor
    /// a0..an` and closure-convert it.
    fn gen_ctor_as_function(
        &mut self,
        whole: &TypedExpr,
        home: &crate::data::Name,
        union: &crate::data::Name,
        ctor: &crate::ast::canonical::Ctor,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        use crate::ast::canonical::{Pattern_, Type};
        let mut arg_types = Vec::new();
        let mut t = whole.tipe.clone();
        while let Type::Lambda(a, b) = t {
            arg_types.push(*a);
            t = *b;
        }
        let result_ty = t;
        let mut params = Vec::new();
        let mut arg_exprs = Vec::new();
        for (i, at) in arg_types.iter().enumerate() {
            let pname = crate::data::Name::from(format!("$ctor{}_{}", self.lam_id, i));
            params.push((
                crate::reporting::Located {
                    region: crate::reporting::Region::ZERO,
                    value: Pattern_::Var(pname.clone()),
                },
                at.clone(),
            ));
            arg_exprs.push(TypedExpr {
                tipe: at.clone(),
                kind: TypedKind::Local(pname),
                region: whole.region,
            });
        }
        let ctor_fn = TypedExpr {
            tipe: whole.tipe.clone(),
            kind: TypedKind::Ctor(home.clone(), union.clone(), ctor.clone()),
            region: whole.region,
        };
        let body = TypedExpr {
            tipe: result_ty.clone(),
            kind: TypedKind::Call(Box::new(ctor_fn), arg_exprs),
            region: whole.region,
        };
        let result_layout = self.layouts.layout_of(&result_ty);
        self.gen_closure(&params, &body, &result_layout)
    }

    /// A partially-applied constructor `Ctor a0..ak` whose result is still a
    /// function: eta-expand into `\rest.. -> Ctor a0..ak rest..` and
    /// closure-convert it. `whole.tipe` is the remaining function type.
    fn gen_ctor_partial(
        &mut self,
        whole: &TypedExpr,
        home: &crate::data::Name,
        union: &crate::data::Name,
        ctor: &crate::ast::canonical::Ctor,
        applied: &[TypedExpr],
    ) -> Result<BasicValueEnum<'ctx>, String> {
        use crate::ast::canonical::{Pattern_, Type};
        // Peel the remaining argument types and the final (constructed) type.
        let mut rest_types = Vec::new();
        let mut t = whole.tipe.clone();
        while let Type::Lambda(a, b) = t {
            rest_types.push(*a);
            t = *b;
        }
        let result_ty = t;
        let id = self.lam_id;
        self.lam_id += 1;
        let mut params = Vec::new();
        let mut call_args: Vec<TypedExpr> = applied.to_vec();
        for (i, at) in rest_types.iter().enumerate() {
            let pname = crate::data::Name::from(format!("$cpart{}_{}", id, i));
            params.push((
                crate::reporting::Located {
                    region: crate::reporting::Region::ZERO,
                    value: Pattern_::Var(pname.clone()),
                },
                at.clone(),
            ));
            call_args.push(TypedExpr {
                tipe: at.clone(),
                kind: TypedKind::Local(pname),
                region: whole.region,
            });
        }
        let ctor_fn = TypedExpr {
            // The constructor's full function type: applied-arg types -> whole.tipe.
            tipe: whole.tipe.clone(),
            kind: TypedKind::Ctor(home.clone(), union.clone(), ctor.clone()),
            region: whole.region,
        };
        let body = TypedExpr {
            tipe: result_ty.clone(),
            kind: TypedKind::Call(Box::new(ctor_fn), call_args),
            region: whole.region,
        };
        let result_layout = self.layouts.layout_of(&result_ty);
        self.gen_closure(&params, &body, &result_layout)
    }

    /// A built-in used as a first-class value or applied to fewer than its
    /// arity of arguments: eta-expand into `\p.. -> f applied.. p..` and
    /// closure-convert it. `full_type` is the built-in's full function type;
    /// `applied` are the arguments already supplied (possibly none).
    fn eta_expand_foreign(
        &mut self,
        module: &crate::data::Name,
        name: &crate::data::Name,
        full_type: &crate::ast::canonical::Type,
        applied: &[TypedExpr],
        region: crate::reporting::Region,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        use crate::ast::canonical::{Pattern_, Type};
        // Peel the function type into argument types and the final result type.
        let mut arg_types = Vec::new();
        let mut t = full_type.clone();
        while let Type::Lambda(a, b) = t {
            arg_types.push(*a);
            t = *b;
        }
        let result_ty = t;
        let id = self.lam_id;
        self.lam_id += 1;
        // Fresh parameters for the not-yet-supplied arguments.
        let mut params = Vec::new();
        let mut call_args: Vec<TypedExpr> = applied.to_vec();
        for i in applied.len()..arg_types.len() {
            let at = arg_types[i].clone();
            let pname = crate::data::Name::from(format!("$eta{}_{}", id, i));
            params.push((
                crate::reporting::Located {
                    region: crate::reporting::Region::ZERO,
                    value: Pattern_::Var(pname.clone()),
                },
                at.clone(),
            ));
            call_args.push(TypedExpr {
                tipe: at,
                kind: TypedKind::Local(pname),
                region,
            });
        }
        let foreign = TypedExpr {
            tipe: full_type.clone(),
            kind: TypedKind::Foreign(module.clone(), name.clone()),
            region,
        };
        let body = TypedExpr {
            tipe: result_ty.clone(),
            kind: TypedKind::Call(Box::new(foreign), call_args),
            region,
        };
        let result_layout = self.layouts.layout_of(&result_ty);
        self.gen_closure(&params, &body, &result_layout)
    }

    /// Emit a generated, type-specialized kernel for a built-in call. Each is
    /// an inline loop over the unboxed representation — no boxing, no call
    /// into the uniform runtime kernels.
    fn gen_kernel(
        &mut self,
        whole: &TypedExpr,
        module: &str,
        name: &str,
        args: &[TypedExpr],
    ) -> Result<BasicValueEnum<'ctx>, String> {
        match (module, name) {
            // `(::)` used first-class (e.g. `List.foldl (::) [] xs`) arrives
            // here via eta-expansion; without this entry it would fall back to
            // `generic_foreign`, which boxes the WHOLE tail list to uniform
            // and unboxes the WHOLE result per call — O(n) per element, so
            // O(n²) inside a fold.
            ("List", "cons") => self.gen_cons(whole, &args[0], &args[1]),
            ("Maybe", "map")
                if args.len() == 2
                    && matches!(&args[0].tipe, crate::ast::canonical::Type::Lambda(..))
                    && matches!(self.layouts.layout_of(&args[1].tipe), Layout::Tagged(_))
                    && matches!(self.layouts.layout_of(&whole.tipe), Layout::Tagged(_)) =>
            {
                self.kernel_maybe_map(whole, args)
            }
            ("Maybe", "withDefault")
                if args.len() == 2
                    && matches!(self.layouts.layout_of(&args[1].tipe), Layout::Tagged(_)) =>
            {
                self.kernel_maybe_with_default(args)
            }
            // String/Array folds: rewrite `M.foldX f acc c` as
            // `List.foldX f acc (M.toList c)`. `toList` marshals once
            // (O(n)); the fold then runs typed. Without this the whole fold
            // goes through `generic_foreign`, which re-boxes the accumulator
            // per element — O(n²) when the accumulator carries a list
            // (base64 decoders folding chars into a List Encoder, elm-flate
            // folding an Array of ints into encoders).
            ("String", "foldl") | ("String", "foldr") | ("Array", "foldl") | ("Array", "foldr")
                if args.len() == 3
                    && matches!(&args[0].tipe, crate::ast::canonical::Type::Lambda(..)) =>
            {
                use crate::ast::canonical::Type;
                let elem_ty = match &args[0].tipe {
                    Type::Lambda(a, _) => (**a).clone(),
                    _ => unreachable!(),
                };
                let list_ty = Type::Type(
                    crate::data::Name::from("List"),
                    crate::data::Name::from("List"),
                    vec![elem_ty],
                );
                let to_list = TypedExpr {
                    tipe: list_ty.clone(),
                    kind: TypedKind::Call(
                        Box::new(TypedExpr {
                            tipe: Type::Lambda(
                                Box::new(args[2].tipe.clone()),
                                Box::new(list_ty),
                            ),
                            kind: TypedKind::Foreign(
                                crate::data::Name::from(module),
                                crate::data::Name::from("toList"),
                            ),
                            region: whole.region,
                        }),
                        vec![args[2].clone()],
                    ),
                    region: whole.region,
                };
                let new_args = vec![args[0].clone(), args[1].clone(), to_list];
                self.gen_kernel(whole, "List", name, &new_args)
            }
            ("List", "sum") => self.kernel_list_sum(args),
            ("List", "length") => self.kernel_list_length(args),
            ("List", "range") => self.kernel_list_range(whole, args),
            ("List", "foldl") => self.kernel_list_foldl(whole, args),
            ("List", "foldr") => self.kernel_list_foldr(whole, args),
            ("List", "map") => self.kernel_list_map(whole, args),
            ("List", "map2") => self.kernel_list_map2(whole, args),
            ("List", "indexedMap") => self.kernel_list_indexed_map(whole, args),
            ("List", "reverse") => self.kernel_list_reverse(whole, args),
            ("List", "filter") => self.kernel_list_filter(whole, args),
            ("List", "member") => self.kernel_list_member(args),
            ("List", "head") => self.kernel_list_head_tail(whole, args, true),
            ("List", "tail") => self.kernel_list_head_tail(whole, args, false),
            ("List", "drop") => self.kernel_list_drop(args),
            ("List", "take") => self.kernel_list_take(whole, args),
            ("List", "all") => self.kernel_list_all_any(args, true),
            ("List", "any") => self.kernel_list_all_any(args, false),
            ("List", "isEmpty") => {
                let list = self.gen(&args[0])?.into_pointer_value();
                let len = self.list_len(list);
                Ok(self
                    .builder
                    .build_int_compare(IntPredicate::EQ, len, self.ctx.i64_type().const_zero(), "isempty")
                    .unwrap()
                    .into())
            }
            ("String", "fromInt") => {
                let n = self.gen(&args[0])?;
                let boxed = self.call_box("rt_int", n);
                Ok(self.call_named("rtb$String$fromInt", &[boxed]))
            }
            ("String", "fromFloat") => {
                let f = self.gen(&args[0])?;
                let boxed = self.call_box("rt_float", f);
                Ok(self.call_named("rtb$String$fromFloat", &[boxed]))
            }
            ("String", "toInt") => {
                let s = self.gen(&args[0])?;
                let uniform = self.call_named("string_to_int", &[s]);
                self.unbox_value(uniform, &whole.tipe)
            }
            ("String", "toFloat") => {
                let s = self.gen(&args[0])?;
                let uniform = self.call_named("string_to_float", &[s]);
                self.unbox_value(uniform, &whole.tipe)
            }
            ("String", "length") => {
                let s = self.gen(&args[0])?;
                let boxed_len = self.call_named("rtb$String$length", &[s]);
                // rtb$String$length returns a uniform int word; unbox it.
                Ok(self.call_named("rt_unint", &[boxed_len]))
            }
            ("String", "join") => self.kernel_string_join(args),
            ("Debug", "toString") => {
                let v = self.gen(&args[0])?;
                let boxed = self.box_value(v, &args[0].tipe)?;
                Ok(self.call_named("debug_to_string", &[boxed]))
            }
            // TEA effects: box the arguments into uniform values and call the
            // runtime. Results (Cmd/Sub/Program) are opaque uniform words.
            ("Platform", "worker") => self.effect_call("platform_worker", args),
            ("Terminal", "writeLine") => self.effect_call("terminal_write_line", args),
            ("Time", "every") => self.effect_call("time_every", args),
            ("Platform.Cmd", "batch") => self.effect_call("cmd_batch", args),
            ("Platform.Cmd", "map") => self.effect_call("cmd_map", args),
            ("Platform.Sub", "batch") => self.effect_call("sub_batch", args),
            ("Platform.Sub", "map") => self.effect_call("sub_map", args),
            ("Platform.Cmd", "none") => Ok(self.load_uniform_global("$Platform$Cmd$none")),
            ("Platform.Sub", "none") => Ok(self.load_uniform_global("$Platform$Sub$none")),
            ("Task", "succeed") => self.effect_call("task_succeed", args),
            ("Task", "fail") => self.effect_call("task_fail", args),
            ("Task", "andThen") => self.effect_call("task_and_then", args),
            ("Task", "onError") => self.effect_call("task_on_error", args),
            ("Task", "map") => self.effect_call("task_map", args),
            ("Task", "map2") => self.effect_call("task_map2", args),
            ("Task", "mapError") => self.effect_call("task_map_error", args),
            ("Task", "sequence") => self.effect_call("task_sequence", args),
            ("Task", "perform") => self.effect_call("task_perform", args),
            ("Task", "attempt") => self.effect_call("task_attempt", args),
            ("Process", "sleep") => self.effect_call("process_sleep", args),
            ("Time", "now") => Ok(self.load_uniform_global("$Time$now")),
            ("Basics", "modBy") => self.kernel_mod_by(args),
            ("Basics", "remainderBy") => {
                let m = self.gen(&args[0])?.into_int_value();
                let x = self.gen(&args[1])?.into_int_value();
                Ok(self.builder.build_int_signed_rem(x, m, "rem").unwrap().into())
            }
            ("Basics", "abs") => {
                let v = self.gen(&args[0])?;
                Ok(if matches!(self.layouts.layout_of(&args[0].tipe), Layout::Float) {
                    let x = v.into_float_value();
                    let neg = self.builder.build_float_neg(x, "neg").unwrap();
                    let is_neg = self
                        .builder
                        .build_float_compare(FloatPredicate::OLT, x, self.ctx.f64_type().const_zero(), "lt")
                        .unwrap();
                    self.builder.build_select(is_neg, neg, x, "abs").unwrap()
                } else {
                    let x = v.into_int_value();
                    let neg = self.builder.build_int_neg(x, "neg").unwrap();
                    let is_neg = self
                        .builder
                        .build_int_compare(IntPredicate::SLT, x, self.ctx.i64_type().const_zero(), "lt")
                        .unwrap();
                    self.builder.build_select(is_neg, neg, x, "abs").unwrap()
                })
            }
            ("Basics", "toFloat") => {
                let n = self.gen(&args[0])?.into_int_value();
                Ok(self
                    .builder
                    .build_signed_int_to_float(n, self.ctx.f64_type(), "tof")
                    .unwrap()
                    .into())
            }
            ("Basics", "min") => self.kernel_min_max(args, true),
            ("Basics", "max") => self.kernel_min_max(args, false),
            ("Basics", "clamp") => self.kernel_clamp(args),
            ("Basics", "truncate") => {
                let x = self.gen(&args[0])?.into_float_value();
                Ok(self.f_to_int(x))
            }
            ("Basics", "floor") => {
                let x = self.gen(&args[0])?.into_float_value();
                let f = self.call_f64_intrinsic("llvm.floor.f64", x);
                Ok(self.f_to_int(f))
            }
            ("Basics", "ceiling") => {
                let x = self.gen(&args[0])?.into_float_value();
                let f = self.call_f64_intrinsic("llvm.ceil.f64", x);
                Ok(self.f_to_int(f))
            }
            ("Basics", "round") => {
                // Elm/JS round-half-up: floor(x + 0.5).
                let x = self.gen(&args[0])?.into_float_value();
                let half = self.ctx.f64_type().const_float(0.5);
                let shifted = self.builder.build_float_add(x, half, "half").unwrap();
                let f = self.call_f64_intrinsic("llvm.floor.f64", shifted);
                Ok(self.f_to_int(f))
            }
            ("Basics", "sqrt") => {
                let x = self.gen(&args[0])?.into_float_value();
                Ok(self.call_f64_intrinsic("llvm.sqrt.f64", x).into())
            }
            // elm/bytes: `Bytes` is an opaque uniform byte buffer; `Encoder` is
            // a normal tagged union the runtime's `bytes_encode` tree-walks; a
            // `Decoder`'s inner `Bytes -> Int -> (Int, a)` function is applied
            // by `bytes_decode`. Every primitive marshals through the runtime.
            ("Elm.Kernel.Bytes", "encode") => self.marshal_call("bytes_encode", args, &whole.tipe),
            ("Elm.Kernel.Bytes", "width") => self.marshal_call("bytes_width", args, &whole.tipe),
            ("Elm.Kernel.Bytes", "getStringWidth") => {
                self.marshal_call("bytes_get_string_width", args, &whole.tipe)
            }
            ("Elm.Kernel.Bytes", "getHostEndianness") => {
                self.marshal_call("bytes_get_host_endianness", args, &whole.tipe)
            }
            ("Elm.Kernel.Bytes", "decode") => self.marshal_call("bytes_decode", args, &whole.tipe),
            ("Elm.Kernel.Bytes", "decodeFailure") => {
                self.marshal_call("bytes_decode_failure", args, &whole.tipe)
            }
            ("Elm.Kernel.Bytes", "read_i8") => self.marshal_call("bytes_read_i8", args, &whole.tipe),
            ("Elm.Kernel.Bytes", "read_u8") => self.marshal_call("bytes_read_u8", args, &whole.tipe),
            ("Elm.Kernel.Bytes", "read_i16") => self.marshal_call("bytes_read_i16", args, &whole.tipe),
            ("Elm.Kernel.Bytes", "read_u16") => self.marshal_call("bytes_read_u16", args, &whole.tipe),
            ("Elm.Kernel.Bytes", "read_i32") => self.marshal_call("bytes_read_i32", args, &whole.tipe),
            ("Elm.Kernel.Bytes", "read_u32") => self.marshal_call("bytes_read_u32", args, &whole.tipe),
            ("Elm.Kernel.Bytes", "read_f32") => self.marshal_call("bytes_read_f32", args, &whole.tipe),
            ("Elm.Kernel.Bytes", "read_f64") => self.marshal_call("bytes_read_f64", args, &whole.tipe),
            ("Elm.Kernel.Bytes", "read_bytes") => {
                self.marshal_call("bytes_read_bytes", args, &whole.tipe)
            }
            ("Elm.Kernel.Bytes", "read_string") => {
                self.marshal_call("bytes_read_string", args, &whole.tipe)
            }
            // Dict/Set/Array: opaque uniform values managed by the runtime.
            // Empty collections are value globals; the rest marshal.
            ("Dict", "empty") => Ok(self.load_uniform_global("$Dict$empty")),
            ("Set", "empty") => Ok(self.load_uniform_global("$Set$empty")),
            ("Array", "empty") => Ok(self.load_uniform_global("$Array$empty")),
            _ => match collection_symbol(module, name) {
                Some(symbol) => self.marshal_call(symbol, args, &whole.tipe),
                // Any other built-in: go through the uniform runtime's
                // `$Module$name` closure (box args, apply, unbox result).
                None => self.generic_foreign(module, name, args, &whole.tipe),
            },
        }
    }

    /// Fallback for built-ins without a specialized typed kernel: box the
    /// arguments, apply the uniform runtime closure `$Module$name`, and unbox
    /// the result. Hot paths (List/arithmetic/records) are handled before
    /// this; everything else works correctly through the runtime, boxed.
    fn generic_foreign(
        &mut self,
        module: &str,
        name: &str,
        args: &[TypedExpr],
        result_tipe: &crate::ast::canonical::Type,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let sym = format!("${}${}", module.replace('.', "$"), name);
        let clos = self.load_uniform_global(&sym);
        if args.is_empty() {
            // A value global (e.g. Basics.pi); unbox to the result type.
            return self.unbox_value(clos, result_tipe);
        }
        let i64_t = self.ctx.i64_type();
        let mut boxed = Vec::with_capacity(args.len());
        for arg in args {
            let v = self.gen(arg)?;
            boxed.push(self.box_value(v, &arg.tipe)?);
        }
        let args_arr = self.entry_alloca(i64_t.array_type(boxed.len() as u32).into(), "aargs");
        for (i, b) in boxed.iter().enumerate() {
            let ep = unsafe {
                self.builder
                    .build_in_bounds_gep(i64_t, args_arr, &[i64_t.const_int(i as u64, false)], "ep")
                    .unwrap()
            };
            self.builder.build_store(ep, *b).unwrap();
        }
        let rt_apply = self.module.get_function("rt_apply").unwrap();
        let uniform = self
            .builder
            .build_call(
                rt_apply,
                &[
                    clos.into(),
                    self.ctx.i32_type().const_int(boxed.len() as u64, false).into(),
                    args_arr.into(),
                ],
                "gf",
            )
            .unwrap()
            .try_as_basic_value()
            .left()
            .unwrap();
        self.unbox_value(uniform, result_tipe)
    }

    fn call_fn(
        &self,
        f: FunctionValue<'ctx>,
        args: &[BasicValueEnum<'ctx>],
    ) -> BasicValueEnum<'ctx> {
        // Widen an i64 argument to a double parameter: a `number` literal whose
        // variable never resolved defaults to Int at the use site, while the
        // callee may be specialized at Float (`yFromLstar 50`). Elm's `number`
        // makes the widening semantically exact.
        let param_tys = f.get_type().get_param_types();
        let argv: Vec<_> = args
            .iter()
            .enumerate()
            .map(|(i, a)| {
                self.coerce_to_slot(*a, param_tys.get(i).copied().map(Into::into))
                    .into()
            })
            .collect();
        self.builder
            .build_call(f, &argv, "hof")
            .unwrap()
            .try_as_basic_value()
            .left()
            .unwrap()
    }

    /// Apply a function-valued expression to already-evaluated arguments.
    /// A named function is called directly; a lambda is inlined in place —
    /// its parameters bound to the arguments, its free variables already in
    /// scope as the enclosing function's locals. This lets kernels accept
    /// lambdas without general closure conversion.
    fn apply_fn_expr(
        &mut self,
        func: &TypedExpr,
        args: &[BasicValueEnum<'ctx>],
    ) -> Result<BasicValueEnum<'ctx>, String> {
        match &func.kind {
            TypedKind::Global(name) => {
                let f = *self
                    .functions
                    .get(&name.to_string())
                    .ok_or_else(|| format!("typed backend: unknown function `{}`", name))?;
                let np = f.count_params() as usize;
                if np == args.len() {
                    return Ok(self.call_fn(f, args));
                }
                if np > args.len() {
                    // Partial application: a multi-argument function passed
                    // point-free to a higher-order kernel and applied to fewer
                    // arguments than it names (e.g. `List.map predicateFromSelector
                    // selectors`, where the 2-ary `predicateFromSelector` maps to
                    // `List (ElmHtml -> Bool)`). Build a closure capturing the
                    // supplied arguments rather than calling with too few.
                    return Ok(self.gen_partial_app(f, args));
                }
                // Over-application: a point-free function (e.g. a bare
                // constructor `unsignedInt8 = U8`) passed to a higher-order
                // kernel and applied to more arguments than it names. Call it
                // with its own parameters, then apply the returned closure to
                // the rest.
                let closure = self.call_fn(f, &args[..np]).into_pointer_value();
                let mut closure_ty = func.tipe.clone();
                for _ in 0..np {
                    match closure_ty {
                        crate::ast::canonical::Type::Lambda(_, r) => closure_ty = *r,
                        _ => break,
                    }
                }
                Ok(self.apply_closure_curried(closure, &args[np..], &closure_ty))
            }
            TypedKind::Lambda(params, body) => {
                // Desugar destructuring parameters into fresh vars + a case-matched
                // body, mirroring `gen_closure`, so an inlined lambda accepts unit,
                // constructor, tuple and record parameters.
                let (owned_params, owned_body);
                let (params, body) =
                    if params.iter().any(|(p, _)| simple_param_name(p).is_none()) {
                        let (np, nb) =
                            desugar_destructuring_params(&mut self.fresh_id, params, body);
                        owned_params = np;
                        owned_body = nb;
                        (owned_params.as_slice(), &owned_body)
                    } else {
                        (params.as_slice(), body.as_ref())
                    };
                if args.len() < params.len() {
                    // Partial application of a lambda literal in a kernel (e.g.
                    // `List.map (\a b c -> ...) xs` applies the 3-param lambda to
                    // one element, yielding a 2-param closure). Bind the supplied
                    // parameters as locals, then build a closure over the
                    // remaining parameters with the same body — the just-bound
                    // parameters are captured as free variables, exactly like any
                    // other closure environment.
                    let mut saved: Vec<(String, Option<BasicValueEnum<'ctx>>)> = Vec::new();
                    for ((pat, _), val) in params.iter().zip(args) {
                        match simple_param_name(pat) {
                            Some(n) => {
                                saved.push((n.clone(), self.locals.get(&n).copied()));
                                self.locals.insert(n, *val);
                            }
                            None => {
                                return Err(
                                    "typed backend: destructuring lambda parameters are not \
                                     supported in kernels yet"
                                        .to_string(),
                                )
                            }
                        }
                    }
                    // The ultimate result type: peel one arrow per lambda param.
                    let mut t = func.tipe.clone();
                    for _ in 0..params.len() {
                        match t {
                            crate::ast::canonical::Type::Lambda(_, r) => t = *r,
                            _ => break,
                        }
                    }
                    let ret_layout = self.layouts.layout_of(&t);
                    let result = self.gen_closure(&params[args.len()..], body, &ret_layout);
                    for (n, old) in saved {
                        match old {
                            Some(v) => {
                                self.locals.insert(n, v);
                            }
                            None => {
                                self.locals.remove(&n);
                            }
                        }
                    }
                    return result;
                }
                if params.len() != args.len() {
                    return Err(format!(
                        "typed backend: over-applied lambda in a kernel is not supported \
                         (params={}, args={})",
                        params.len(),
                        args.len(),
                    ));
                }
                let mut saved: Vec<(String, Option<BasicValueEnum<'ctx>>)> = Vec::new();
                for ((pat, _), val) in params.iter().zip(args) {
                    match simple_param_name(pat) {
                        Some(n) => {
                            saved.push((n.clone(), self.locals.get(&n).copied()));
                            self.locals.insert(n, *val);
                        }
                        None => {
                            return Err(
                                "typed backend: destructuring lambda parameters are not \
                                 supported in kernels yet"
                                    .to_string(),
                            )
                        }
                    }
                }
                let v = self.gen(body)?;
                for (n, old) in saved {
                    match old {
                        Some(v) => {
                            self.locals.insert(n, v);
                        }
                        None => {
                            self.locals.remove(&n);
                        }
                    }
                }
                Ok(v)
            }
            _ => {
                // A closure value (e.g. a function-typed parameter): apply it
                // indirectly, with currying so an under-saturated application
                // builds a partial closure rather than calling with too few
                // arguments.
                let closure = self.gen(func)?.into_pointer_value();
                Ok(self.apply_closure_curried(closure, args, &func.tipe))
            }
        }
    }

    /// `List.foldl : (a -> b -> b) -> b -> List a -> b` — fold left, calling
    /// the specialized element function with no boxing.
    fn kernel_list_foldl(
        &mut self,
        whole: &TypedExpr,
        args: &[TypedExpr],
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let elem = self.elem_layout(&args[2].tipe)?;
        let init = self.gen(&args[1])?;
        let list = self.gen(&args[2])?.into_pointer_value();
        let acc_ty = self.llvm_type(&self.layouts.layout_of(&whole.tipe));
        self.emit_fold(list, &elem, init, &args[0], acc_ty, true)
    }

    /// `List.foldr` — like foldl but visiting elements tail-to-head.
    fn kernel_list_foldr(
        &mut self,
        whole: &TypedExpr,
        args: &[TypedExpr],
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let elem = self.elem_layout(&args[2].tipe)?;
        let init = self.gen(&args[1])?;
        let list = self.gen(&args[2])?.into_pointer_value();
        let acc_ty = self.llvm_type(&self.layouts.layout_of(&whole.tipe));
        self.emit_fold(list, &elem, init, &args[0], acc_ty, false)
    }

    /// Fold over an array-backed list. `head_first` iterates head-to-tail
    /// (foldl); otherwise tail-to-head (foldr). Elements are stored reversed,
    /// so the head is at data[len-1].
    fn emit_fold(
        &mut self,
        list: inkwell::values::PointerValue<'ctx>,
        elem: &Layout,
        init: BasicValueEnum<'ctx>,
        f: &TypedExpr,
        acc_ty: BasicTypeEnum<'ctx>,
        head_first: bool,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let i64_t = self.ctx.i64_type();
        let len = self.list_len(list);
        let backing = self.list_backing(list);
        let acc_slot = self.entry_alloca(acc_ty, "acc");
        self.builder.build_store(acc_slot, init).unwrap();
        let elem = elem.clone();
        let f = f.clone();
        self.for_count(len, |s, i| {
            // head_first: data index len-1-i; otherwise i.
            let idx = if head_first {
                let m1 = s.builder.build_int_sub(len, i64_t.const_int(1, false), "m1").unwrap();
                s.builder.build_int_sub(m1, i, "idx").unwrap()
            } else {
                i
            };
            let v = s.list_load(backing, &elem, idx);
            let acc = s.builder.build_load(acc_ty, acc_slot, "acc").unwrap();
            let new_acc = s.apply_fn_expr(&f, &[v, acc])?;
            s.builder.build_store(acc_slot, new_acc).unwrap();
            Ok(())
        })?;
        Ok(self.builder.build_load(acc_ty, acc_slot, "acc").unwrap())
    }

    /// `List.map : (a -> b) -> List a -> List b`. With reversed contiguous
    /// storage the mapping is a parallel index loop that preserves order:
    /// out.data[i] = f(in.data[i]).
    fn kernel_list_map(
        &mut self,
        whole: &TypedExpr,
        args: &[TypedExpr],
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let src_elem = self.elem_layout(&args[1].tipe)?;
        let dst_elem = self.elem_layout(&whole.tipe)?;
        let list = self.gen(&args[1])?.into_pointer_value();
        let len = self.list_len(list);
        let backing = self.list_backing(list);
        let out = self.list_alloc(&dst_elem, len);
        let f = args[0].clone();
        self.for_count(len, |s, i| {
            let v = s.list_load(backing, &src_elem, i);
            let mapped = s.apply_fn_expr(&f, &[v])?;
            s.list_store(out, &dst_elem, i, mapped);
            Ok(())
        })?;
        Ok(self.make_list(len, out).into())
    }

    /// Truncate an f64 toward zero to an i64.
    fn f_to_int(&self, x: inkwell::values::FloatValue<'ctx>) -> BasicValueEnum<'ctx> {
        // Ints are 63-bit tagged immediates, so a magnitude at or beyond 2^62
        // overflows the tag bit; a raw fptosi on ±infinity / out-of-range is
        // also poison. `round (1 / 0)` is a real idiom (elm-review's
        // `infinity : Int` sentinel, expected to compare larger than any real
        // value), so clamp to the representable tagged range on the float side
        // — where minnum/maxnum also fold NaN to the bound — before converting.
        // Keep this in step with native_runtime's `f64_to_int_word`.
        let f64t = self.ctx.f64_type();
        // 2^62 - 1024 (largest f64 strictly below 2^62) and -2^62.
        let hi = f64t.const_float(4611686018427386880.0);
        let lo = f64t.const_float(-4611686018427387904.0);
        let t = self.call_f64_intrinsic2("llvm.minnum.f64", x, hi);
        let t = self.call_f64_intrinsic2("llvm.maxnum.f64", t, lo);
        let clamped = self
            .builder
            .build_float_to_signed_int(t, self.ctx.i64_type(), "toint")
            .unwrap();
        // NaN must become 0, not a bound: on JS `Int` is a double, so
        // `ceiling (0/0)` stays NaN and `NaN >= 0` is False (e.g. an empty
        // `Float.Extra.range`). minnum/maxnum fold NaN to the non-NaN operand
        // (here `hi`), which would wrongly yield a huge count, so override it.
        let is_nan = self
            .builder
            .build_float_compare(inkwell::FloatPredicate::UNO, x, x, "isnan")
            .unwrap();
        let zero = self.ctx.i64_type().const_zero();
        self.builder
            .build_select(is_nan, zero, clamped, "nan0")
            .unwrap()
    }

    /// Call a unary f64 LLVM intrinsic (e.g. `llvm.floor.f64`), declaring it
    /// on first use.
    fn call_f64_intrinsic(
        &self,
        name: &str,
        x: inkwell::values::FloatValue<'ctx>,
    ) -> inkwell::values::FloatValue<'ctx> {
        let f = self.module.get_function(name).unwrap_or_else(|| {
            let f64 = self.ctx.f64_type();
            self.module.add_function(name, f64.fn_type(&[f64.into()], false), None)
        });
        self.builder
            .build_call(f, &[x.into()], "intr")
            .unwrap()
            .try_as_basic_value()
            .left()
            .unwrap()
            .into_float_value()
    }

    /// Call a binary f64 LLVM intrinsic (e.g. `llvm.pow.f64`), declaring it on
    /// first use.
    fn call_f64_intrinsic2(
        &self,
        name: &str,
        x: inkwell::values::FloatValue<'ctx>,
        y: inkwell::values::FloatValue<'ctx>,
    ) -> inkwell::values::FloatValue<'ctx> {
        let f = self.module.get_function(name).unwrap_or_else(|| {
            let f64 = self.ctx.f64_type();
            self.module
                .add_function(name, f64.fn_type(&[f64.into(), f64.into()], false), None)
        });
        self.builder
            .build_call(f, &[x.into(), y.into()], "intr2")
            .unwrap()
            .try_as_basic_value()
            .left()
            .unwrap()
            .into_float_value()
    }

    /// `String.join sep xs` — concatenate the string list head-first with the
    /// separator between elements.
    fn kernel_string_join(&mut self, args: &[TypedExpr]) -> Result<BasicValueEnum<'ctx>, String> {
        let sep = self.gen(&args[0])?;
        let list = self.gen(&args[1])?.into_pointer_value();
        let i64_t = self.ctx.i64_type();
        let len = self.list_len(list);
        let backing = self.list_backing(list);
        let rt_append = self.module.get_function("rt_append").unwrap();
        let empty = self.gen_string("")?;
        let result = self.entry_alloca(i64_t.into(), "res");
        self.builder.build_store(result, empty).unwrap();

        let elem = Layout::Str;
        self.for_count(len, |s, i| {
            // Head-first element at data[len-1-i]; separator before all but
            // the first (i > 0).
            let m1 = s.builder.build_int_sub(len, i64_t.const_int(1, false), "m1").unwrap();
            let idx = s.builder.build_int_sub(m1, i, "idx").unwrap();
            let head = s.list_load(backing, &elem, idx);
            let acc = s.builder.build_load(i64_t, result, "acc").unwrap();
            let is_first = s
                .builder
                .build_int_compare(IntPredicate::EQ, i, i64_t.const_zero(), "first")
                .unwrap();
            let with_sep = s
                .builder
                .build_call(rt_append, &[acc.into(), sep.into()], "ws")
                .unwrap()
                .try_as_basic_value()
                .left()
                .unwrap();
            let base = s.builder.build_select(is_first, acc, with_sep, "base").unwrap();
            let r2 = s
                .builder
                .build_call(rt_append, &[base.into(), head.into()], "r2")
                .unwrap()
                .try_as_basic_value()
                .left()
                .unwrap();
            s.builder.build_store(result, r2).unwrap();
            Ok(())
        })?;
        Ok(self.builder.build_load(i64_t, result, "joined").unwrap())
    }

    /// `clamp lo hi x` = `if x < lo then lo else if x > hi then hi else x`.
    fn kernel_clamp(&mut self, args: &[TypedExpr]) -> Result<BasicValueEnum<'ctx>, String> {
        let lo = self.gen(&args[0])?;
        let hi = self.gen(&args[1])?;
        let x = self.gen(&args[2])?;
        let is_float = matches!(self.layouts.layout_of(&args[2].tipe), Layout::Float);
        let (below, above) = if is_float {
            (
                self.builder
                    .build_float_compare(FloatPredicate::OLT, x.into_float_value(), lo.into_float_value(), "below")
                    .unwrap(),
                self.builder
                    .build_float_compare(FloatPredicate::OGT, x.into_float_value(), hi.into_float_value(), "above")
                    .unwrap(),
            )
        } else {
            (
                self.builder
                    .build_int_compare(IntPredicate::SLT, x.into_int_value(), lo.into_int_value(), "below")
                    .unwrap(),
                self.builder
                    .build_int_compare(IntPredicate::SGT, x.into_int_value(), hi.into_int_value(), "above")
                    .unwrap(),
            )
        };
        let low_clamped = self.builder.build_select(below, lo, x, "loclamp").unwrap();
        Ok(self.builder.build_select(above, hi, low_clamped, "clamp").unwrap())
    }

    /// `min`/`max` on any `comparable`. Int/Float compare inline; other
    /// comparables (strings, or a value left with an unresolved `comparable`
    /// layout that is carried as the uniform runtime word) route through the
    /// runtime's generic comparison, then select.
    fn kernel_min_max(
        &mut self,
        args: &[TypedExpr],
        is_min: bool,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let lay = self.layouts.layout_of(&args[0].tipe);
        let a = self.gen(&args[0])?;
        let b = self.gen(&args[1])?;
        let cond = match lay {
            Layout::Float => {
                let pred = if is_min { FloatPredicate::OLT } else { FloatPredicate::OGT };
                self.builder
                    .build_float_compare(pred, a.into_float_value(), b.into_float_value(), "mm")
                    .unwrap()
            }
            ref l if l.is_scalar() => {
                let pred = if is_min { IntPredicate::SLT } else { IntPredicate::SGT };
                self.builder
                    .build_int_compare(pred, a.into_int_value(), b.into_int_value(), "mm")
                    .unwrap()
            }
            // Comparables carried as the uniform runtime word: an unresolved
            // `comparable` (Ref, a word bit-cast to a pointer), a heap string,
            // or an opaque word. Recover the word and use the runtime compare.
            Layout::Ref | Layout::Str | Layout::Opaque => {
                let i64_t = self.ctx.i64_type();
                let to_word = |v: BasicValueEnum<'ctx>| -> BasicValueEnum<'ctx> {
                    if v.is_pointer_value() {
                        self.builder
                            .build_ptr_to_int(v.into_pointer_value(), i64_t, "w")
                            .unwrap()
                            .into()
                    } else {
                        v
                    }
                };
                let aw = to_word(a);
                let bw = to_word(b);
                let sym = if is_min { "rt_lt" } else { "rt_gt" };
                let cmp = self.call_named(sym, &[aw, bw]);
                self.call_named("rt_is_true", &[cmp]).into_int_value()
            }
            other => {
                return Err(format!(
                    "typed backend: min/max on layout {:?} is not supported",
                    other
                ))
            }
        };
        Ok(self.builder.build_select(cond, a, b, "minmax").unwrap())
    }

    /// `modBy m x` — floored modulo (the result takes the sign of the
    /// modulus), matching Elm/JS semantics rather than truncated remainder.
    fn kernel_mod_by(&mut self, args: &[TypedExpr]) -> Result<BasicValueEnum<'ctx>, String> {
        let m = self.gen(&args[0])?.into_int_value();
        let x = self.gen(&args[1])?.into_int_value();
        let b = &self.builder;
        let zero = self.ctx.i64_type().const_zero();
        let r = b.build_int_signed_rem(x, m, "r").unwrap();
        let r_nz = b.build_int_compare(IntPredicate::NE, r, zero, "rnz").unwrap();
        let r_neg = b.build_int_compare(IntPredicate::SLT, r, zero, "rneg").unwrap();
        let m_neg = b.build_int_compare(IntPredicate::SLT, m, zero, "mneg").unwrap();
        let diff = b.build_xor(r_neg, m_neg, "diff").unwrap();
        let need = b.build_and(r_nz, diff, "need").unwrap();
        let radd = b.build_int_add(r, m, "radd").unwrap();
        Ok(b.build_select(need, radd, r, "mod").unwrap())
    }

    /// `Maybe.map f m`, typed: tag check + one closure call. Without this,
    /// the call goes through `generic_foreign`, boxing `m` (and the closure's
    /// captured state) to uniform per call — quadratic when the mapped value
    /// carries a list and the call sits in a fold (base64 decoders).
    fn kernel_maybe_map(
        &mut self,
        whole: &TypedExpr,
        args: &[TypedExpr],
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let in_variants = match self.layouts.layout_of(&args[1].tipe) {
            Layout::Tagged(v) => v,
            other => return Err(format!("typed backend: expected Maybe, got {:?}", other)),
        };
        let out_variants = match self.layouts.layout_of(&whole.tipe) {
            Layout::Tagged(v) => v,
            other => return Err(format!("typed backend: expected Maybe, got {:?}", other)),
        };
        let f = self.gen(&args[0])?.into_pointer_value();
        let m = self.gen(&args[1])?.into_pointer_value();
        let i32_t = self.ctx.i32_type();
        let ptr_t = self.ctx.ptr_type(inkwell::AddressSpace::default());
        // Tag order matches kernel_list_head_tail: Just = 0, Nothing = 1.
        let tag = self.builder.build_load(i32_t, m, "tag").unwrap().into_int_value();
        let is_just = self
            .builder
            .build_int_compare(IntPredicate::EQ, tag, i32_t.const_zero(), "isjust")
            .unwrap();
        let just_bb = self.new_block("mbm.just");
        let nothing_bb = self.new_block("mbm.nothing");
        let merge = self.new_block("mbm.end");
        self.builder.build_conditional_branch(is_just, just_bb, nothing_bb).unwrap();

        self.builder.position_at_end(just_bb);
        let in_struct = self.ctor_struct(&in_variants[0]);
        let fp = self.builder.build_struct_gep(in_struct, m, 1, "fp").unwrap();
        let x = self
            .builder
            .build_load(self.llvm_type(&in_variants[0][0]), fp, "x")
            .unwrap();
        let fx = self.apply_closure_curried(f, &[x], &args[0].tipe);
        let just: BasicValueEnum = self.alloc_tagged(&out_variants[0], 0, &[fx]).into();
        // apply_closure_curried can emit blocks; branch from the CURRENT one.
        let just_end = self.builder.get_insert_block().unwrap();
        self.builder.build_unconditional_branch(merge).unwrap();

        self.builder.position_at_end(nothing_bb);
        let nothing: BasicValueEnum = self.alloc_tagged(&out_variants[1], 1, &[]).into();
        self.builder.build_unconditional_branch(merge).unwrap();

        self.builder.position_at_end(merge);
        let phi = self.builder.build_phi(ptr_t, "maybe").unwrap();
        phi.add_incoming(&[
            (&just as &dyn BasicValue, just_end),
            (&nothing as &dyn BasicValue, nothing_bb),
        ]);
        Ok(phi.as_basic_value())
    }

    /// `Maybe.withDefault d m`, typed — same motivation as [`kernel_maybe_map`].
    fn kernel_maybe_with_default(
        &mut self,
        args: &[TypedExpr],
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let in_variants = match self.layouts.layout_of(&args[1].tipe) {
            Layout::Tagged(v) => v,
            other => return Err(format!("typed backend: expected Maybe, got {:?}", other)),
        };
        let d = self.gen(&args[0])?;
        let m = self.gen(&args[1])?.into_pointer_value();
        let i32_t = self.ctx.i32_type();
        let tag = self.builder.build_load(i32_t, m, "tag").unwrap().into_int_value();
        let is_just = self
            .builder
            .build_int_compare(IntPredicate::EQ, tag, i32_t.const_zero(), "isjust")
            .unwrap();
        let just_bb = self.new_block("mbw.just");
        let merge = self.new_block("mbw.end");
        let from_bb = self.builder.get_insert_block().unwrap();
        self.builder.build_conditional_branch(is_just, just_bb, merge).unwrap();

        self.builder.position_at_end(just_bb);
        let in_struct = self.ctor_struct(&in_variants[0]);
        let fp = self.builder.build_struct_gep(in_struct, m, 1, "fp").unwrap();
        let x = self
            .builder
            .build_load(self.llvm_type(&in_variants[0][0]), fp, "x")
            .unwrap();
        self.builder.build_unconditional_branch(merge).unwrap();

        self.builder.position_at_end(merge);
        let phi = self.builder.build_phi(d.get_type(), "mwd").unwrap();
        phi.add_incoming(&[(&d as &dyn BasicValue, from_bb), (&x as &dyn BasicValue, just_bb)]);
        Ok(phi.as_basic_value())
    }

    /// `List.head`/`List.tail : List a -> Maybe _` — Nothing on empty, else
    /// Just of the head (or the tail list). Maybe's constructors are Just
    /// (variant 0, one field) and Nothing (variant 1, no fields).
    fn kernel_list_head_tail(
        &mut self,
        whole: &TypedExpr,
        args: &[TypedExpr],
        is_head: bool,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let variants = match self.layouts.layout_of(&whole.tipe) {
            Layout::Tagged(v) => v,
            other => return Err(format!("typed backend: expected Maybe, got {:?}", other)),
        };
        let elem = self.elem_layout(&args[0].tipe)?;
        let list = self.gen(&args[0])?.into_pointer_value();
        let i64_t = self.ctx.i64_type();
        let ptr_t = self.ctx.ptr_type(inkwell::AddressSpace::default());
        let len = self.list_len(list);
        let backing = self.list_backing(list);
        let is_empty = self
            .builder
            .build_int_compare(IntPredicate::EQ, len, i64_t.const_zero(), "empty")
            .unwrap();
        let just_bb = self.new_block("mb.just");
        let nothing_bb = self.new_block("mb.nothing");
        let merge = self.new_block("mb.end");
        self.builder.build_conditional_branch(is_empty, nothing_bb, just_bb).unwrap();

        self.builder.position_at_end(just_bb);
        let last = self.builder.build_int_sub(len, i64_t.const_int(1, false), "last").unwrap();
        let field: BasicValueEnum = if is_head {
            self.list_load(backing, &elem, last)
        } else {
            // tail = same backing, length len-1.
            self.make_list(last, backing).into()
        };
        let just: BasicValueEnum = self.alloc_tagged(&variants[0], 0, &[field]).into();
        self.builder.build_unconditional_branch(merge).unwrap();

        self.builder.position_at_end(nothing_bb);
        let nothing: BasicValueEnum = self.alloc_tagged(&variants[1], 1, &[]).into();
        self.builder.build_unconditional_branch(merge).unwrap();

        self.builder.position_at_end(merge);
        let phi = self.builder.build_phi(ptr_t, "maybe").unwrap();
        phi.add_incoming(&[(&just as &dyn BasicValue, just_bb), (&nothing as &dyn BasicValue, nothing_bb)]);
        Ok(phi.as_basic_value())
    }

    /// `List.drop n xs` — an O(1) view: the remaining length shares the same
    /// backing (dropping heads only shrinks the visible length).
    fn kernel_list_drop(&mut self, args: &[TypedExpr]) -> Result<BasicValueEnum<'ctx>, String> {
        let n = self.gen(&args[0])?.into_int_value();
        let list = self.gen(&args[1])?.into_pointer_value();
        let len = self.list_len(list);
        let backing = self.list_backing(list);
        let nclamp = self.clamp_count(n, len);
        let m = self.builder.build_int_sub(len, nclamp, "m").unwrap();
        Ok(self.make_list(m, backing).into())
    }

    /// `List.take n xs` — copy the first `n` head elements.
    fn kernel_list_take(
        &mut self,
        whole: &TypedExpr,
        args: &[TypedExpr],
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let elem = self.elem_layout(&whole.tipe)?;
        let n = self.gen(&args[0])?.into_int_value();
        let list = self.gen(&args[1])?.into_pointer_value();
        let len = self.list_len(list);
        let backing = self.list_backing(list);
        let nclamp = self.clamp_count(n, len);
        let out = self.list_alloc(&elem, nclamp);
        // result.data[k] = input.data[len - nclamp + k] (top n' slots).
        let base = self.builder.build_int_sub(len, nclamp, "base").unwrap();
        self.for_count(nclamp, |s, k| {
            let src = s.builder.build_int_add(base, k, "src").unwrap();
            let v = s.list_load(backing, &elem, src);
            s.list_store(out, &elem, k, v);
            Ok(())
        })?;
        Ok(self.make_list(nclamp, out).into())
    }

    /// Clamp a requested count into `0..=len`.
    fn clamp_count(
        &self,
        n: inkwell::values::IntValue<'ctx>,
        len: inkwell::values::IntValue<'ctx>,
    ) -> inkwell::values::IntValue<'ctx> {
        let zero = self.ctx.i64_type().const_zero();
        let neg = self.builder.build_int_compare(IntPredicate::SLT, n, zero, "neg").unwrap();
        let pos = self.builder.build_select(neg, zero, n, "pos").unwrap().into_int_value();
        let over = self.builder.build_int_compare(IntPredicate::SGT, pos, len, "over").unwrap();
        self.builder.build_select(over, len, pos, "clamp").unwrap().into_int_value()
    }

    /// `List.all`/`List.any pred xs : Bool` — short-circuiting predicate fold.
    fn kernel_list_all_any(
        &mut self,
        args: &[TypedExpr],
        is_all: bool,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let elem = self.elem_layout(&args[1].tipe)?;
        let list = self.gen(&args[1])?.into_pointer_value();
        let len = self.list_len(list);
        let backing = self.list_backing(list);
        let i64_t = self.ctx.i64_type();
        let i1_t = self.ctx.bool_type();
        let entry = self.builder.get_insert_block().unwrap();
        let loop_bb = self.new_block("aa.loop");
        let body_bb = self.new_block("aa.body");
        let cont_bb = self.new_block("aa.cont");
        let short_bb = self.new_block("aa.short");
        let end_bb = self.new_block("aa.end");
        let merge = self.new_block("aa.merge");

        self.builder.build_unconditional_branch(loop_bb).unwrap();
        self.builder.position_at_end(loop_bb);
        let idx = self.builder.build_phi(i64_t, "i").unwrap();
        let zero: BasicValueEnum = i64_t.const_zero().into();
        idx.add_incoming(&[(&zero as &dyn BasicValue, entry)]);
        let i = idx.as_basic_value().into_int_value();
        let more = self.builder.build_int_compare(IntPredicate::SLT, i, len, "more").unwrap();
        self.builder.build_conditional_branch(more, body_bb, end_bb).unwrap();

        self.builder.position_at_end(body_bb);
        let v = self.list_load(backing, &elem, i);
        let p = self.apply_fn_expr(&args[0], &[v])?.into_int_value();
        // all: continue while p true, short-circuit on false. any: opposite.
        if is_all {
            self.builder.build_conditional_branch(p, cont_bb, short_bb).unwrap();
        } else {
            self.builder.build_conditional_branch(p, short_bb, cont_bb).unwrap();
        }

        self.builder.position_at_end(cont_bb);
        let i2: BasicValueEnum = self.builder.build_int_add(i, i64_t.const_int(1, false), "i2").unwrap().into();
        let cont_end = self.builder.get_insert_block().unwrap();
        self.builder.build_unconditional_branch(loop_bb).unwrap();
        idx.add_incoming(&[(&i2 as &dyn BasicValue, cont_end)]);

        // Reached the end without short-circuiting: all=true, any=false.
        self.builder.position_at_end(end_bb);
        self.builder.build_unconditional_branch(merge).unwrap();
        // Short-circuited: all=false, any=true.
        self.builder.position_at_end(short_bb);
        self.builder.build_unconditional_branch(merge).unwrap();

        self.builder.position_at_end(merge);
        let phi = self.builder.build_phi(i1_t, "aa").unwrap();
        let end_val: BasicValueEnum = i1_t.const_int(is_all as u64, false).into();
        let short_val: BasicValueEnum = i1_t.const_int(!is_all as u64, false).into();
        phi.add_incoming(&[(&end_val as &dyn BasicValue, end_bb), (&short_val as &dyn BasicValue, short_bb)]);
        Ok(phi.as_basic_value())
    }

    /// `List.member x xs : Bool` — walk comparing each element to `x`
    /// (scalar elements only), short-circuiting on the first match.
    fn kernel_list_member(&mut self, args: &[TypedExpr]) -> Result<BasicValueEnum<'ctx>, String> {
        let elem = self.elem_layout(&args[1].tipe)?;
        let target = self.gen(&args[0])?;
        let list = self.gen(&args[1])?.into_pointer_value();
        let len = self.list_len(list);
        let backing = self.list_backing(list);
        let i64_t = self.ctx.i64_type();
        let entry = self.builder.get_insert_block().unwrap();
        let loop_bb = self.new_block("mem.loop");
        let body_bb = self.new_block("mem.body");
        let cont_bb = self.new_block("mem.cont");
        let found_bb = self.new_block("mem.found");
        let none_bb = self.new_block("mem.none");
        let merge = self.new_block("mem.end");

        self.builder.build_unconditional_branch(loop_bb).unwrap();
        self.builder.position_at_end(loop_bb);
        let idx = self.builder.build_phi(i64_t, "i").unwrap();
        let zero: BasicValueEnum = i64_t.const_zero().into();
        idx.add_incoming(&[(&zero as &dyn BasicValue, entry)]);
        let i = idx.as_basic_value().into_int_value();
        let more = self.builder.build_int_compare(IntPredicate::SLT, i, len, "more").unwrap();
        self.builder.build_conditional_branch(more, body_bb, none_bb).unwrap();

        self.builder.position_at_end(body_bb);
        let head = self.list_load(backing, &elem, i);
        let eq = self.equals_vals(head, target, &elem)?;
        self.builder.build_conditional_branch(eq, found_bb, cont_bb).unwrap();

        self.builder.position_at_end(cont_bb);
        let i2: BasicValueEnum = self.builder.build_int_add(i, i64_t.const_int(1, false), "i2").unwrap().into();
        let cont_end = self.builder.get_insert_block().unwrap();
        self.builder.build_unconditional_branch(loop_bb).unwrap();
        idx.add_incoming(&[(&i2 as &dyn BasicValue, cont_end)]);

        self.builder.position_at_end(found_bb);
        self.builder.build_unconditional_branch(merge).unwrap();
        self.builder.position_at_end(none_bb);
        self.builder.build_unconditional_branch(merge).unwrap();

        self.builder.position_at_end(merge);
        let phi = self.builder.build_phi(self.ctx.bool_type(), "member").unwrap();
        let t: BasicValueEnum = self.ctx.bool_type().const_int(1, false).into();
        let f: BasicValueEnum = self.ctx.bool_type().const_int(0, false).into();
        phi.add_incoming(&[(&t as &dyn BasicValue, found_bb), (&f as &dyn BasicValue, none_bb)]);
        Ok(phi.as_basic_value())
    }

    /// `List.map2 : (a -> b -> c) -> List a -> List b -> List c` — walk both
    /// lists in lockstep, stopping at the shorter, applying the function.
    fn kernel_list_map2(
        &mut self,
        whole: &TypedExpr,
        args: &[TypedExpr],
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let a_elem = self.elem_layout(&args[1].tipe)?;
        let b_elem = self.elem_layout(&args[2].tipe)?;
        let c_elem = self.elem_layout(&whole.tipe)?;
        let xs = self.gen(&args[1])?.into_pointer_value();
        let ys = self.gen(&args[2])?.into_pointer_value();
        let xlen = self.list_len(xs);
        let ylen = self.list_len(ys);
        let xb = self.list_backing(xs);
        let yb = self.list_backing(ys);
        // Pair by distance from the head; result length is the shorter.
        let x_lt = self.builder.build_int_compare(IntPredicate::SLT, xlen, ylen, "xlt").unwrap();
        let minlen = self.builder.build_select(x_lt, xlen, ylen, "min").unwrap().into_int_value();
        let xoff = self.builder.build_int_sub(xlen, minlen, "xoff").unwrap();
        let yoff = self.builder.build_int_sub(ylen, minlen, "yoff").unwrap();
        let out = self.list_alloc(&c_elem, minlen);
        let f = args[0].clone();
        self.for_count(minlen, |s, i| {
            let xi = s.builder.build_int_add(xoff, i, "xi").unwrap();
            let yi = s.builder.build_int_add(yoff, i, "yi").unwrap();
            let hx = s.list_load(xb, &a_elem, xi);
            let hy = s.list_load(yb, &b_elem, yi);
            let mapped = s.apply_fn_expr(&f, &[hx, hy])?;
            s.list_store(out, &c_elem, i, mapped);
            Ok(())
        })?;
        Ok(self.make_list(minlen, out).into())
    }

    /// `List.indexedMap : (Int -> a -> b) -> List a -> List b` — map with a
    /// running index passed as the first argument.
    fn kernel_list_indexed_map(
        &mut self,
        whole: &TypedExpr,
        args: &[TypedExpr],
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let a_elem = self.elem_layout(&args[1].tipe)?;
        let b_elem = self.elem_layout(&whole.tipe)?;
        let list = self.gen(&args[1])?.into_pointer_value();
        let len = self.list_len(list);
        let backing = self.list_backing(list);
        let out = self.list_alloc(&b_elem, len);
        let i64_t = self.ctx.i64_type();
        let f = args[0].clone();
        // Element at data[i] is the (len-1-i)-th from the head; that is its
        // index for the mapping function.
        self.for_count(len, |s, i| {
            let m1 = s.builder.build_int_sub(len, i64_t.const_int(1, false), "m1").unwrap();
            let index = s.builder.build_int_sub(m1, i, "index").unwrap();
            let v = s.list_load(backing, &a_elem, i);
            let mapped = s.apply_fn_expr(&f, &[index.into(), v])?;
            s.list_store(out, &b_elem, i, mapped);
            Ok(())
        })?;
        Ok(self.make_list(len, out).into())
    }

    /// `List.reverse : List a -> List a`.
    fn kernel_list_reverse(
        &mut self,
        whole: &TypedExpr,
        args: &[TypedExpr],
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let elem = self.elem_layout(&whole.tipe)?;
        let list = self.gen(&args[0])?.into_pointer_value();
        Ok(self.emit_reverse(list, &elem).into())
    }

    /// Reverse an array-backed list: out.data[i] = in.data[len-1-i].
    fn emit_reverse(
        &mut self,
        list: inkwell::values::PointerValue<'ctx>,
        elem: &Layout,
    ) -> inkwell::values::PointerValue<'ctx> {
        let i64_t = self.ctx.i64_type();
        let len = self.list_len(list);
        let backing = self.list_backing(list);
        let out = self.list_alloc(elem, len);
        let elem = elem.clone();
        let _ = self.for_count(len, |s, i| {
            let m1 = s.builder.build_int_sub(len, i64_t.const_int(1, false), "m1").unwrap();
            let src = s.builder.build_int_sub(m1, i, "src").unwrap();
            let v = s.list_load(backing, &elem, src);
            s.list_store(out, &elem, i, v);
            Ok(())
        });
        self.make_list(len, out)
    }

    /// `List.filter : (a -> Bool) -> List a -> List a` — keep elements the
    /// predicate accepts, building the result in order.
    fn kernel_list_filter(
        &mut self,
        whole: &TypedExpr,
        args: &[TypedExpr],
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let elem = self.elem_layout(&whole.tipe)?;
        let list = self.gen(&args[1])?.into_pointer_value();
        let len = self.list_len(list);
        let backing = self.list_backing(list);
        let i64_t = self.ctx.i64_type();
        // Scan tail-to-head appending kept elements at increasing output
        // index; the reversed layout makes the result head-first order come
        // out correct, with output length `j`.
        let out = self.list_alloc(&elem, len);
        let j_slot = self.entry_alloca(i64_t.into(), "j");
        self.builder.build_store(j_slot, i64_t.const_zero()).unwrap();
        let f = args[0].clone();
        self.for_count(len, |s, i| {
            let v = s.list_load(backing, &elem, i);
            let keep = s.apply_fn_expr(&f, &[v])?.into_int_value();
            let keep_bb = s.new_block("filt.keep");
            let cont_bb = s.new_block("filt.cont");
            s.builder.build_conditional_branch(keep, keep_bb, cont_bb).unwrap();
            s.builder.position_at_end(keep_bb);
            let j = s.builder.build_load(i64_t, j_slot, "j").unwrap().into_int_value();
            s.list_store(out, &elem, j, v);
            let j2 = s.builder.build_int_add(j, i64_t.const_int(1, false), "j2").unwrap();
            s.builder.build_store(j_slot, j2).unwrap();
            s.builder.build_unconditional_branch(cont_bb).unwrap();
            s.builder.position_at_end(cont_bb);
            Ok(())
        })?;
        let j = self.builder.build_load(i64_t, j_slot, "j").unwrap().into_int_value();
        Ok(self.make_list(j, out).into())
    }

    /// `List.sum : List number -> number`.
    fn kernel_list_sum(&mut self, args: &[TypedExpr]) -> Result<BasicValueEnum<'ctx>, String> {
        let elem = self.elem_layout(&args[0].tipe)?;
        let is_float = matches!(elem, Layout::Float);
        let list = self.gen(&args[0])?.into_pointer_value();
        let len = self.list_len(list);
        let backing = self.list_backing(list);
        let acc_ty = self.llvm_type(&elem);
        let acc_slot = self.entry_alloca(acc_ty, "acc");
        let zero: BasicValueEnum = if is_float {
            self.ctx.f64_type().const_zero().into()
        } else {
            self.ctx.i64_type().const_zero().into()
        };
        self.builder.build_store(acc_slot, zero).unwrap();
        let elem2 = elem.clone();
        self.for_count(len, |s, i| {
            let v = s.list_load(backing, &elem2, i);
            let acc = s.builder.build_load(acc_ty, acc_slot, "acc").unwrap();
            let sum: BasicValueEnum = if is_float {
                s.builder.build_float_add(acc.into_float_value(), v.into_float_value(), "s").unwrap().into()
            } else {
                s.builder.build_int_add(acc.into_int_value(), v.into_int_value(), "s").unwrap().into()
            };
            s.builder.build_store(acc_slot, sum).unwrap();
            Ok(())
        })?;
        Ok(self.builder.build_load(acc_ty, acc_slot, "sum").unwrap())
    }

    /// `List.length : List a -> Int` — just the header's length field.
    fn kernel_list_length(&mut self, args: &[TypedExpr]) -> Result<BasicValueEnum<'ctx>, String> {
        let list = self.gen(&args[0])?.into_pointer_value();
        Ok(self.list_len(list).into())
    }

    /// `List.range lo hi` — build `[lo..hi]`. With reversed storage the head
    /// (lo) is last, so data[i] = hi - i.
    fn kernel_list_range(
        &mut self,
        whole: &TypedExpr,
        args: &[TypedExpr],
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let elem = self.elem_layout(&whole.tipe)?;
        let lo = self.gen(&args[0])?.into_int_value();
        let hi = self.gen(&args[1])?.into_int_value();
        let i64_t = self.ctx.i64_type();
        // len = max(0, hi - lo + 1).
        let span = self.builder.build_int_sub(hi, lo, "span").unwrap();
        let raw_len = self.builder.build_int_add(span, i64_t.const_int(1, false), "rlen").unwrap();
        let neg = self.builder.build_int_compare(IntPredicate::SLT, raw_len, i64_t.const_zero(), "neg").unwrap();
        let len = self.builder.build_select(neg, i64_t.const_zero(), raw_len, "len").unwrap().into_int_value();
        let out = self.list_alloc(&elem, len);
        self.for_count(len, |s, i| {
            let v = s.builder.build_int_sub(hi, i, "v").unwrap();
            s.list_store(out, &elem, i, v.into());
            Ok(())
        })?;
        Ok(self.make_list(len, out).into())
    }

    /// Prepend `head` to the list `tail`, returning a new list header. The
    /// runtime grows or copies the backing; the new length is tail's + 1.
    fn cons(
        &mut self,
        elem: &Layout,
        head: BasicValueEnum<'ctx>,
        tail: inkwell::values::PointerValue<'ctx>,
    ) -> inkwell::values::PointerValue<'ctx> {
        let esize = self.elem_size(elem);
        let tlen = self.list_len(tail);
        let tbacking = self.list_backing(tail);
        let tmp = self.entry_alloca(self.llvm_type(elem), "consh");
        self.builder.build_store(tmp, head).unwrap();
        let cons = self.module.get_function("alm_list_cons").unwrap();
        let nb = self
            .builder
            .build_call(cons, &[tbacking.into(), tlen.into(), tmp.into(), esize.into()], "nb")
            .unwrap()
            .try_as_basic_value()
            .left()
            .unwrap()
            .into_pointer_value();
        let nlen = self
            .builder
            .build_int_add(tlen, self.ctx.i64_type().const_int(1, false), "nlen")
            .unwrap();
        self.make_list(nlen, nb)
    }

    /// A string literal: an interned byte constant handed to `rt_str`, which
    /// yields a uniform string word.
    fn gen_string(&mut self, s: &str) -> Result<BasicValueEnum<'ctx>, String> {
        let bytes = s.as_bytes();
        let data = self.ctx.const_string(bytes, false);
        let g = self.module.add_global(data.get_type(), None, "str");
        g.set_initializer(&data);
        g.set_constant(true);
        g.set_linkage(Linkage::Private);
        let rt_str = self.module.get_function("rt_str").unwrap();
        let len = self.ctx.i64_type().const_int(bytes.len() as u64, false);
        Ok(self
            .builder
            .build_call(rt_str, &[g.as_pointer_value().into(), len.into()], "s")
            .unwrap()
            .try_as_basic_value()
            .left()
            .unwrap())
    }

    /// `++`: strings go through the runtime; lists are concatenated by copying
    /// the left cons cells and pointing the copy's tail at the right list.
    fn gen_append(
        &mut self,
        l: &TypedExpr,
        r: &TypedExpr,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        match self.layouts.layout_of(&l.tipe) {
            Layout::Str => {
                let lv = self.gen(l)?;
                let rv = self.gen(r)?;
                let rt_append = self.module.get_function("rt_append").unwrap();
                Ok(self
                    .builder
                    .build_call(rt_append, &[lv.into(), rv.into()], "app")
                    .unwrap()
                    .try_as_basic_value()
                    .left()
                    .unwrap())
            }
            Layout::List(elem) => self.gen_list_append(&elem, l, r),
            // A polymorphic `appendable` that monomorphization left unresolved
            // (layout Ref) — e.g. a helper `wrap a b = a ++ b` specialized only
            // at its call sites. Both operands are already uniform words, so
            // dispatch on the runtime value (String vs List) like the uniform
            // backend's `rt_append`; the result is likewise a uniform word.
            Layout::Ref => {
                let lv = self.gen(l)?;
                let rv = self.gen(r)?;
                let lw = self.as_word(lv);
                let rw = self.as_word(rv);
                let word = self.call_named("rt_append", &[lw, rw]);
                // `rt_append` yields a uniform word; the result type is the same
                // unresolved `appendable` (Ref), so present it in that layout.
                self.unbox_value(word, &l.tipe)
            }
            other => Err(format!("typed backend: ++ is not supported on {:?}", other)),
        }
    }

    fn gen_list_append(
        &mut self,
        elem: &Layout,
        l: &TypedExpr,
        r: &TypedExpr,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let left = self.gen(l)?.into_pointer_value();
        let right = self.gen(r)?.into_pointer_value();
        let xlen = self.list_len(left);
        let ylen = self.list_len(right);
        let xb = self.list_backing(left);
        let yb = self.list_backing(right);
        let rlen = self.builder.build_int_add(xlen, ylen, "rlen").unwrap();
        let out = self.list_alloc(elem, rlen);
        // Reversed layout: out.data[0..ylen] = right's data; out.data[ylen..]
        // = left's data.
        let elem_r = elem.clone();
        self.for_count(ylen, |s, i| {
            let v = s.list_load(yb, &elem_r, i);
            s.list_store(out, &elem_r, i, v);
            Ok(())
        })?;
        let elem_l = elem.clone();
        self.for_count(xlen, |s, j| {
            let dst = s.builder.build_int_add(ylen, j, "dst").unwrap();
            let v = s.list_load(xb, &elem_l, j);
            s.list_store(out, &elem_l, dst, v);
            Ok(())
        })?;
        Ok(self.make_list(rlen, out).into())
    }

    /// Function composition `f << g` (= `\x -> f (g x)`) and `f >> g`
    /// (= `\x -> g (f x)`). Desugared to a synthetic lambda and run through
    /// the closure machinery, so the composed functions are captured.
    fn gen_compose(
        &mut self,
        l: &TypedExpr,
        r: &TypedExpr,
        whole: &TypedExpr,
        is_left: bool,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        use crate::ast::canonical::Type;
        let (a_ty, c_ty) = match &whole.tipe {
            Type::Lambda(a, c) => ((**a).clone(), (**c).clone()),
            other => {
                return Err(format!(
                    "typed backend: composition result is not a function: {:?}",
                    other
                ))
            }
        };
        // `<<`: apply g (right) then f (left); `>>`: apply f (left) then g.
        let (first, second) = if is_left { (r, l) } else { (l, r) };
        let b_ty = match &first.tipe {
            Type::Lambda(_, b) => (**b).clone(),
            other => {
                return Err(format!(
                    "typed backend: composition operand is not a function: {:?}",
                    other
                ))
            }
        };

        let xname = format!("$compose{}", self.lam_id);
        let x_local = TypedExpr {
            tipe: a_ty.clone(),
            kind: TypedKind::Local(crate::data::Name::from(xname.clone())),
            region: whole.region,
        };
        let inner = TypedExpr {
            tipe: b_ty,
            kind: TypedKind::Call(Box::new(first.clone()), vec![x_local]),
            region: whole.region,
        };
        let mut outer = TypedExpr {
            tipe: c_ty.clone(),
            kind: TypedKind::Call(Box::new(second.clone()), vec![inner]),
            region: whole.region,
        };
        let pat = crate::reporting::Located {
            region: crate::reporting::Region::ZERO,
            value: crate::ast::canonical::Pattern_::Var(crate::data::Name::from(xname.clone())),
        };
        let mut params = vec![(pat, a_ty)];
        // `\x -> second (first x)` is always an arity-1 closure, but the
        // composition's type can have more arrows (`add << negate` has type
        // `Int -> Int -> Int`). A closure's compiled arity must equal its type
        // arrow count, or a caller passing one argument per arrow reads past
        // its parameters. Eta-expand the extra arrows.
        let mut ret = c_ty;
        let mut i = 0;
        while let Type::Lambda(arg, rest) = ret {
            let pn = format!("{}_e{}", xname, i);
            let plocal = TypedExpr {
                tipe: (*arg).clone(),
                kind: TypedKind::Local(crate::data::Name::from(pn.clone())),
                region: whole.region,
            };
            let ppat = crate::reporting::Located {
                region: crate::reporting::Region::ZERO,
                value: crate::ast::canonical::Pattern_::Var(crate::data::Name::from(pn)),
            };
            params.push((ppat, (*arg).clone()));
            outer = TypedExpr {
                tipe: (*rest).clone(),
                kind: TypedKind::Call(Box::new(outer), vec![plocal]),
                region: whole.region,
            };
            ret = *rest;
            i += 1;
        }
        let result_layout = self.layouts.layout_of(&ret);
        self.gen_closure(&params, &outer, &result_layout)
    }

    /// `x |> f` and `f <| x` are application. Normalize to a flattened call —
    /// appending the piped value as the callee's final argument — and reuse
    /// the ordinary call path (so pipes into kernels/constructors just work).
    fn gen_pipe(
        &mut self,
        whole: &TypedExpr,
        op: &str,
        l: &TypedExpr,
        r: &TypedExpr,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let (callee, extra) = if op == "|>" { (r, l) } else { (l, r) };
        let (func, args): (TypedExpr, Vec<TypedExpr>) = match &callee.kind {
            TypedKind::Call(f, existing) => {
                let mut args = existing.clone();
                args.push(extra.clone());
                ((**f).clone(), args)
            }
            _ => (callee.clone(), vec![extra.clone()]),
        };
        self.gen_call(whole, &func, &args)
    }

    /// `==` / `/=` — structural equality. Scalars compare directly, strings
    /// via the runtime, tuples and records field-by-field (recursively).
    fn gen_equals(
        &mut self,
        l: &TypedExpr,
        r: &TypedExpr,
        negate: bool,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let layout = self.layouts.layout_of(&l.tipe);
        let lv = self.gen(l)?;
        let rv = self.gen(r)?;
        // A recursive type's layout contains `Ref` (the pointer that breaks
        // self-reference); comparing such a value field-by-field on layout alone
        // would reach a recursive occurrence as `Ref` and fall back to pointer
        // identity. Compare by *type* instead — structurally and in place, with
        // recursion broken at the function level by a memoized per-type helper
        // (no allocation). Non-recursive types keep the direct layout path.
        let eq = if layout_has_ref(&layout) {
            self.equals_typed(lv, rv, &l.tipe)?
        } else {
            self.equals_vals(lv, rv, &layout)?
        };
        Ok(if negate {
            self.builder.build_not(eq, "neq").unwrap().into()
        } else {
            eq.into()
        })
    }

    /// Recursively test two values of a given layout for equality, yielding an
    /// i1.
    /// Reinterpret a value as the uniform i64 word: a pointer becomes its
    /// address (the word a heap value carries), an integer word passes through.
    /// Used where a string/opaque value may arrive boxed (as a pointer) or
    /// unboxed (as the word).
    fn as_word(&self, v: BasicValueEnum<'ctx>) -> BasicValueEnum<'ctx> {
        if v.is_pointer_value() {
            self.builder
                .build_ptr_to_int(v.into_pointer_value(), self.ctx.i64_type(), "word")
                .unwrap()
                .into()
        } else {
            v
        }
    }

    fn equals_vals(
        &mut self,
        a: BasicValueEnum<'ctx>,
        b: BasicValueEnum<'ctx>,
        layout: &Layout,
    ) -> Result<inkwell::values::IntValue<'ctx>, String> {
        match layout {
            // Unit has a single inhabitant, so `() == ()` is always true.
            Layout::Unit => Ok(self.ctx.bool_type().const_int(1, false)),
            Layout::Int | Layout::Char | Layout::Bool | Layout::Enum(_) => Ok(self
                .builder
                .build_int_compare(IntPredicate::EQ, a.into_int_value(), b.into_int_value(), "eq")
                .unwrap()),
            Layout::Float => Ok(self
                .builder
                .build_float_compare(FloatPredicate::OEQ, a.into_float_value(), b.into_float_value(), "eq")
                .unwrap()),
            Layout::Str | Layout::Opaque => {
                // Both are uniform words, but a string/opaque value that flowed
                // through a `Ref`-layout slot (a polymorphic field/container)
                // arrives as a pointer whose address *is* that word. Coerce
                // either operand to the integer word before comparing, so a
                // boxed and an unboxed occurrence of the same value compare
                // equal (and the runtime call is well-typed).
                let aw = self.as_word(a);
                let bw = self.as_word(b);
                let cmp = self.call_named("rt_eq", &[aw, bw]);
                Ok(self.call_named("rt_is_true", &[cmp]).into_int_value())
            }
            Layout::Tuple(elems) => {
                let sa = a.into_struct_value();
                let sb = b.into_struct_value();
                let mut acc = self.ctx.bool_type().const_int(1, false);
                for (i, el) in elems.iter().enumerate() {
                    let fa = self.builder.build_extract_value(sa, i as u32, "a").unwrap();
                    let fb = self.builder.build_extract_value(sb, i as u32, "b").unwrap();
                    let e = self.equals_vals(fa, fb, el)?;
                    acc = self.builder.build_and(acc, e, "and").unwrap();
                }
                Ok(acc)
            }
            Layout::Record(fields) => {
                let sa = a.into_struct_value();
                let sb = b.into_struct_value();
                let mut acc = self.ctx.bool_type().const_int(1, false);
                for (i, (_, fl)) in fields.iter().enumerate() {
                    let fa = self.builder.build_extract_value(sa, i as u32, "a").unwrap();
                    let fb = self.builder.build_extract_value(sb, i as u32, "b").unwrap();
                    let e = self.equals_vals(fa, fb, fl)?;
                    acc = self.builder.build_and(acc, e, "and").unwrap();
                }
                Ok(acc)
            }
            Layout::List(elem) => self.equals_lists(a, b, elem),
            Layout::Tagged(variants) => {
                let variants = variants.clone();
                self.equals_tagged(a, b, &variants)
            }
            // A function value, or an opaque boxed reference (e.g. the phantom
            // element type of an empty list): pointer identity. Elm's `==` on
            // functions is reference equality (it errors on non-equal
            // functions); references to the same function are canonical (a
            // shared global, and box/unbox preserves identity), so pointer
            // equality gives the reference-equal answer and, unlike Elm,
            // gracefully returns `False` rather than crashing otherwise — which
            // also lets a value that merely *contains* a function be compared
            // (e.g. a lazy list, or a model carrying a comparator).
            Layout::Ref | Layout::Closure => {
                let i64_t = self.ctx.i64_type();
                let ai = self
                    .builder
                    .build_ptr_to_int(a.into_pointer_value(), i64_t, "ai")
                    .unwrap();
                let bi = self
                    .builder
                    .build_ptr_to_int(b.into_pointer_value(), i64_t, "bi")
                    .unwrap();
                Ok(self.builder.build_int_compare(IntPredicate::EQ, ai, bi, "refeq").unwrap())
            }
            other => Err(format!(
                "typed backend: == on layout {:?} is not supported yet",
                other
            )),
        }
    }

    /// Tagged-union equality: equal iff same constructor tag and, for that
    /// constructor, all fields equal (recursively). A switch dispatches on the
    /// tag to a per-constructor field comparison.
    fn equals_tagged(
        &mut self,
        a: BasicValueEnum<'ctx>,
        b: BasicValueEnum<'ctx>,
        variants: &[Vec<Layout>],
    ) -> Result<inkwell::values::IntValue<'ctx>, String> {
        let ptr_a = a.into_pointer_value();
        let ptr_b = b.into_pointer_value();
        let i32_t = self.ctx.i32_type();
        let i1_t = self.ctx.bool_type();
        let tag_a = self.builder.build_load(i32_t, ptr_a, "taga").unwrap().into_int_value();
        let tag_b = self.builder.build_load(i32_t, ptr_b, "tagb").unwrap().into_int_value();
        let teq = self.builder.build_int_compare(IntPredicate::EQ, tag_a, tag_b, "teq").unwrap();

        let sw_bb = self.new_block("teq.sw");
        let false_bb = self.new_block("teq.false");
        let merge = self.new_block("teq.end");
        self.builder.build_conditional_branch(teq, sw_bb, false_bb).unwrap();

        let mut incoming: Vec<(BasicValueEnum<'ctx>, inkwell::basic_block::BasicBlock<'ctx>)> =
            Vec::new();
        self.builder.position_at_end(false_bb);
        self.builder.build_unconditional_branch(merge).unwrap();
        let false_v: BasicValueEnum = i1_t.const_zero().into();
        incoming.push((false_v, false_bb));

        // Build the switch and a block per constructor.
        self.builder.position_at_end(sw_bb);
        let var_blocks: Vec<_> = (0..variants.len())
            .map(|k| self.new_block(&format!("teq.v{}", k)))
            .collect();
        let cases: Vec<_> = var_blocks
            .iter()
            .enumerate()
            .map(|(k, bb)| (i32_t.const_int(k as u64, false), *bb))
            .collect();
        self.builder.build_switch(tag_a, false_bb, &cases).unwrap();

        for (k, bb) in var_blocks.iter().enumerate() {
            self.builder.position_at_end(*bb);
            let fields = &variants[k];
            let sty = self.ctor_struct(fields);
            let mut acc = i1_t.const_int(1, false);
            for (i, fl) in fields.iter().enumerate() {
                let fap = self.builder.build_struct_gep(sty, ptr_a, (i + 1) as u32, "fa").unwrap();
                let fa = self.builder.build_load(self.llvm_type(fl), fap, "fav").unwrap();
                let fbp = self.builder.build_struct_gep(sty, ptr_b, (i + 1) as u32, "fb").unwrap();
                let fb = self.builder.build_load(self.llvm_type(fl), fbp, "fbv").unwrap();
                let e = self.equals_vals(fa, fb, fl)?;
                acc = self.builder.build_and(acc, e, "and").unwrap();
            }
            let end = self.builder.get_insert_block().unwrap();
            self.builder.build_unconditional_branch(merge).unwrap();
            incoming.push((acc.into(), end));
        }

        self.builder.position_at_end(merge);
        let phi = self.builder.build_phi(i1_t, "tageq").unwrap();
        for (v, bb) in &incoming {
            phi.add_incoming(&[(v as &dyn BasicValue, *bb)]);
        }
        Ok(phi.as_basic_value().into_int_value())
    }

    /// List equality: walk both lists in lockstep — equal iff same length and
    /// all elements equal.
    fn equals_lists(
        &mut self,
        a: BasicValueEnum<'ctx>,
        b: BasicValueEnum<'ctx>,
        elem: &Layout,
    ) -> Result<inkwell::values::IntValue<'ctx>, String> {
        let i1_t = self.ctx.bool_type();
        let lena = self.list_len(a.into_pointer_value());
        let lenb = self.list_len(b.into_pointer_value());
        let ba = self.list_backing(a.into_pointer_value());
        let bb = self.list_backing(b.into_pointer_value());
        let len_eq = self.builder.build_int_compare(IntPredicate::EQ, lena, lenb, "leneq").unwrap();
        // Equal length is necessary; if so, all elements must match.
        let res = self.entry_alloca(i1_t.into(), "eqres");
        self.builder.build_store(res, len_eq).unwrap();
        let scan_bb = self.new_block("eql.scan");
        let after_bb = self.new_block("eql.after");
        self.builder.build_conditional_branch(len_eq, scan_bb, after_bb).unwrap();

        self.builder.position_at_end(scan_bb);
        let elem = elem.clone();
        self.for_count(lena, |s, i| {
            let av = s.list_load(ba, &elem, i);
            let bv = s.list_load(bb, &elem, i);
            let e = s.equals_vals(av, bv, &elem)?;
            let cur = s.builder.build_load(i1_t, res, "cur").unwrap().into_int_value();
            let and = s.builder.build_and(cur, e, "and").unwrap();
            s.builder.build_store(res, and).unwrap();
            Ok(())
        })?;
        self.builder.build_unconditional_branch(after_bb).unwrap();

        self.builder.position_at_end(after_bb);
        Ok(self.builder.build_load(i1_t, res, "listeq").unwrap().into_int_value())
    }

    /// Type-directed structural equality for a value whose layout contains a
    /// `Ref` (a recursive type). It walks the value in place — no boxing — and
    /// breaks recursion at the function level via [`get_eq_fn`], so comparing
    /// two equal trees/records/JSON:API resources costs no allocation. Tuples,
    /// records and lists recurse into their element/field *types* (so a
    /// recursive occurrence dispatches on its real type, never bottoming out at
    /// `Ref`); a union routes to its memoized helper.
    fn equals_typed(
        &mut self,
        a: BasicValueEnum<'ctx>,
        b: BasicValueEnum<'ctx>,
        tipe: &crate::ast::canonical::Type,
    ) -> Result<inkwell::values::IntValue<'ctx>, String> {
        use crate::ast::canonical::Type;
        let layout = self.layouts.layout_of(tipe);
        if !layout_has_ref(&layout) {
            // No recursion below here: the direct layout comparison is exact.
            return self.equals_vals(a, b, &layout);
        }
        let i1_t = self.ctx.bool_type();
        match &layout {
            Layout::Tuple(_) => {
                let subs: Vec<Type> = match tipe {
                    Type::Tuple(x, y, z) => {
                        let mut v = vec![(**x).clone(), (**y).clone()];
                        if let Some(z) = z {
                            v.push((**z).clone());
                        }
                        v
                    }
                    _ => return self.equals_vals(a, b, &layout),
                };
                let sa = a.into_struct_value();
                let sb = b.into_struct_value();
                let mut acc = i1_t.const_int(1, false);
                for (i, st) in subs.iter().enumerate() {
                    let fa = self.builder.build_extract_value(sa, i as u32, "a").unwrap();
                    let fb = self.builder.build_extract_value(sb, i as u32, "b").unwrap();
                    let e = self.equals_typed(fa, fb, st)?;
                    acc = self.builder.build_and(acc, e, "and").unwrap();
                }
                Ok(acc)
            }
            Layout::Record(sorted) => {
                let sorted = sorted.clone();
                let field_types = match tipe {
                    Type::Record(fs, _) => fs.clone(),
                    _ => return self.equals_vals(a, b, &layout),
                };
                let sa = a.into_struct_value();
                let sb = b.into_struct_value();
                let mut acc = i1_t.const_int(1, false);
                for (i, (fname, _)) in sorted.iter().enumerate() {
                    let fty = field_types
                        .iter()
                        .find(|(n, _)| n == fname)
                        .map(|(_, t)| t.clone())
                        .ok_or_else(|| format!("typed backend: == missing field `{}`", fname))?;
                    let fa = self.builder.build_extract_value(sa, i as u32, "a").unwrap();
                    let fb = self.builder.build_extract_value(sb, i as u32, "b").unwrap();
                    let e = self.equals_typed(fa, fb, &fty)?;
                    acc = self.builder.build_and(acc, e, "and").unwrap();
                }
                Ok(acc)
            }
            Layout::List(_) => {
                let et = match tipe {
                    Type::Type(_, _, args) if args.len() == 1 => args[0].clone(),
                    _ => return self.equals_vals(a, b, &layout),
                };
                self.equals_lists_typed(a, b, &et)
            }
            Layout::Tagged(_) => {
                let f = self.get_eq_fn(tipe)?;
                let call = self.builder.build_call(f, &[a.into(), b.into()], "eqrec").unwrap();
                Ok(call.try_as_basic_value().left().unwrap().into_int_value())
            }
            // A bare `Ref` here is a genuine phantom (e.g. the element type of an
            // empty list, never actually compared): pointer identity as before.
            _ => self.equals_vals(a, b, &layout),
        }
    }

    /// List equality with a type-directed element comparison (the recursive
    /// twin of [`equals_lists`]).
    fn equals_lists_typed(
        &mut self,
        a: BasicValueEnum<'ctx>,
        b: BasicValueEnum<'ctx>,
        elem: &crate::ast::canonical::Type,
    ) -> Result<inkwell::values::IntValue<'ctx>, String> {
        let i1_t = self.ctx.bool_type();
        let elem_layout = self.layouts.layout_of(elem);
        let lena = self.list_len(a.into_pointer_value());
        let lenb = self.list_len(b.into_pointer_value());
        let ba = self.list_backing(a.into_pointer_value());
        let bb = self.list_backing(b.into_pointer_value());
        let len_eq = self.builder.build_int_compare(IntPredicate::EQ, lena, lenb, "leneq").unwrap();
        let res = self.entry_alloca(i1_t.into(), "eqres");
        self.builder.build_store(res, len_eq).unwrap();
        let scan_bb = self.new_block("eqlt.scan");
        let after_bb = self.new_block("eqlt.after");
        self.builder.build_conditional_branch(len_eq, scan_bb, after_bb).unwrap();

        self.builder.position_at_end(scan_bb);
        let elem = elem.clone();
        self.for_count(lena, |s, i| {
            let av = s.list_load(ba, &elem_layout, i);
            let bv = s.list_load(bb, &elem_layout, i);
            let e = s.equals_typed(av, bv, &elem)?;
            let cur = s.builder.build_load(i1_t, res, "cur").unwrap().into_int_value();
            let and = s.builder.build_and(cur, e, "and").unwrap();
            s.builder.build_store(res, and).unwrap();
            Ok(())
        })?;
        self.builder.build_unconditional_branch(after_bb).unwrap();
        self.builder.position_at_end(after_bb);
        Ok(self.builder.build_load(i1_t, res, "listeq").unwrap().into_int_value())
    }

    /// A memoized recursive structural-equality helper `eq_T(a, b) -> i1` for a
    /// (possibly recursive) tagged union, mirroring [`get_box_fn`]. Inserted
    /// into the cache before its body is built, so a self-referential field
    /// reuses it and codegen terminates.
    fn get_eq_fn(
        &mut self,
        tipe: &crate::ast::canonical::Type,
    ) -> Result<FunctionValue<'ctx>, String> {
        let key = format!("{:?}", tipe);
        if let Some(f) = self.eq_fns.get(&key) {
            return Ok(*f);
        }
        let ptr_t = self.ctx.ptr_type(inkwell::AddressSpace::default());
        let i1_t = self.ctx.bool_type();
        let fname = format!("eq.{}", self.lam_id);
        self.lam_id += 1;
        let fty = i1_t.fn_type(&[ptr_t.into(), ptr_t.into()], false);
        let f = self.module.add_function(&fname, fty, Some(Linkage::Internal));
        self.eq_fns.insert(key, f);

        let ctors = self
            .layouts
            .union_ctors(tipe)
            .ok_or_else(|| "typed backend: unknown union in ==".to_string())?;

        let saved_locals = std::mem::take(&mut self.locals);
        let saved_fn = self.cur_fn;
        let saved_block = self.builder.get_insert_block();
        let saved_loc = self.cur_loc;
        self.cur_fn = Some(f);
        let entry = self.ctx.append_basic_block(f, "entry");
        self.builder.position_at_end(entry);
        self.clear_loc();

        let body = (|s: &mut Self| -> Result<(), String> {
            let i32_t = s.ctx.i32_type();
            let ptr_a = f.get_nth_param(0).unwrap().into_pointer_value();
            let ptr_b = f.get_nth_param(1).unwrap().into_pointer_value();
            let tag_a = s.builder.build_load(i32_t, ptr_a, "taga").unwrap().into_int_value();
            let tag_b = s.builder.build_load(i32_t, ptr_b, "tagb").unwrap().into_int_value();
            let teq = s.builder.build_int_compare(IntPredicate::EQ, tag_a, tag_b, "teq").unwrap();
            let sw_bb = s.new_block("eqf.sw");
            let false_bb = s.new_block("eqf.false");
            let merge = s.new_block("eqf.end");
            s.builder.build_conditional_branch(teq, sw_bb, false_bb).unwrap();

            let mut incoming: Vec<(BasicValueEnum<'ctx>, inkwell::basic_block::BasicBlock<'ctx>)> =
                Vec::new();
            s.builder.position_at_end(false_bb);
            s.builder.build_unconditional_branch(merge).unwrap();
            incoming.push((i1_t.const_zero().into(), false_bb));

            s.builder.position_at_end(sw_bb);
            let var_blocks: Vec<_> = (0..ctors.len())
                .map(|k| s.new_block(&format!("eqf.v{}", k)))
                .collect();
            let cases: Vec<_> = var_blocks
                .iter()
                .enumerate()
                .map(|(k, bb)| (i32_t.const_int(k as u64, false), *bb))
                .collect();
            s.builder.build_switch(tag_a, false_bb, &cases).unwrap();

            for (k, bb) in var_blocks.iter().enumerate() {
                s.builder.position_at_end(*bb);
                let (_, field_types) = &ctors[k];
                let field_layouts: Vec<Layout> =
                    field_types.iter().map(|t| s.layouts.layout_of(t)).collect();
                let sty = s.ctor_struct(&field_layouts);
                let mut acc = i1_t.const_int(1, false);
                for (i, fty) in field_types.iter().enumerate() {
                    let fap = s.builder.build_struct_gep(sty, ptr_a, (i + 1) as u32, "fa").unwrap();
                    let fa = s.builder.build_load(s.llvm_type(&field_layouts[i]), fap, "fav").unwrap();
                    let fbp = s.builder.build_struct_gep(sty, ptr_b, (i + 1) as u32, "fb").unwrap();
                    let fb = s.builder.build_load(s.llvm_type(&field_layouts[i]), fbp, "fbv").unwrap();
                    let e = s.equals_typed(fa, fb, fty)?;
                    acc = s.builder.build_and(acc, e, "and").unwrap();
                }
                let end = s.builder.get_insert_block().unwrap();
                s.builder.build_unconditional_branch(merge).unwrap();
                incoming.push((acc.into(), end));
            }

            s.builder.position_at_end(merge);
            let phi = s.builder.build_phi(i1_t, "eqf").unwrap();
            for (v, bb) in &incoming {
                phi.add_incoming(&[(v as &dyn BasicValue, *bb)]);
            }
            s.builder.build_return(Some(&phi.as_basic_value())).unwrap();
            Ok(())
        })(self);

        self.locals = saved_locals;
        self.cur_fn = saved_fn;
        body?;
        if let Some(b) = saved_block {
            self.builder.position_at_end(b);
        }
        self.restore_loc(saved_loc);
        Ok(f)
    }

    /// Short-circuiting `&&` / `||`: the right operand is only evaluated when
    /// the left doesn't already decide the result (important since a pure but
    /// partial RHS — e.g. a division guarded by the LHS — must be skipped).
    fn gen_and_or(
        &mut self,
        op: &str,
        l: &TypedExpr,
        r: &TypedExpr,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let lv = self.gen(l)?.into_int_value();
        let cur = self.builder.get_insert_block().unwrap();
        let rhs_bb = self.new_block("sc.rhs");
        let merge = self.new_block("sc.end");
        if op == "&&" {
            self.builder.build_conditional_branch(lv, rhs_bb, merge).unwrap();
        } else {
            self.builder.build_conditional_branch(lv, merge, rhs_bb).unwrap();
        }
        self.builder.position_at_end(rhs_bb);
        let rv = self.gen(r)?;
        let rhs_end = self.builder.get_insert_block().unwrap();
        self.builder.build_unconditional_branch(merge).unwrap();

        self.builder.position_at_end(merge);
        let phi = self.builder.build_phi(self.ctx.bool_type(), "sc").unwrap();
        // From `cur`: the short-circuit value (false for &&, true for ||).
        let shortcut: BasicValueEnum = self
            .ctx
            .bool_type()
            .const_int((op == "||") as u64, false)
            .into();
        phi.add_incoming(&[(&shortcut as &dyn BasicValue, cur), (&rv as &dyn BasicValue, rhs_end)]);
        Ok(phi.as_basic_value())
    }

    fn gen_cons(
        &mut self,
        whole: &TypedExpr,
        head: &TypedExpr,
        tail: &TypedExpr,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let elem = self.elem_layout(&whole.tipe)?;
        let h = self.gen(head)?;
        let t = self.gen(tail)?.into_pointer_value();
        Ok(self.cons(&elem, h, t).into())
    }

    fn gen_list(
        &mut self,
        whole: &TypedExpr,
        items: &[TypedExpr],
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let elem = self.elem_layout(&whole.tipe)?;
        let n = items.len();
        if n == 0 {
            return Ok(self.empty_list().into());
        }
        let mut values = Vec::with_capacity(n);
        for item in items {
            values.push(self.gen(item)?);
        }
        let backing = self.list_alloc(&elem, self.ctx.i64_type().const_int(n as u64, false));
        // Reversed storage: item k (from the head) goes to data[n-1-k].
        for (k, v) in values.into_iter().enumerate() {
            let idx = self.ctx.i64_type().const_int((n - 1 - k) as u64, false);
            self.list_store(backing, &elem, idx, v);
        }
        Ok(self.make_list(self.ctx.i64_type().const_int(n as u64, false), backing).into())
    }

    /// Allocate a list backing for `count` elements of the given layout.
    fn list_alloc(
        &self,
        elem: &Layout,
        count: inkwell::values::IntValue<'ctx>,
    ) -> inkwell::values::PointerValue<'ctx> {
        let esize = self.elem_size(elem);
        let f = self.module.get_function("alm_list_alloc").unwrap();
        self.builder
            .build_call(f, &[count.into(), esize.into()], "lb")
            .unwrap()
            .try_as_basic_value()
            .left()
            .unwrap()
            .into_pointer_value()
    }

    /// Closure-convert a lambda: capture the free variables in scope, lift the
    /// body to a top-level function taking the captured environment as its
    /// first parameter, and allocate a closure `{fn_ptr, captures...}`.
    fn gen_closure(
        &mut self,
        params: &[(crate::ast::canonical::Pattern, crate::ast::canonical::Type)],
        body: &TypedExpr,
        result_layout: &Layout,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        self.gen_closure_named(params, body, result_layout, None)
    }

    /// As [`gen_closure`], but `self_name`, when set, names the closure itself
    /// for a recursive local function: the name is excluded from the captured
    /// free variables and, inside the lifted body, bound to the environment
    /// pointer — which *is* the closure `{fn_ptr, captures...}`, so a
    /// self-call re-enters through the ordinary closure calling convention.
    fn gen_closure_named(
        &mut self,
        params: &[(crate::ast::canonical::Pattern, crate::ast::canonical::Type)],
        body: &TypedExpr,
        result_layout: &Layout,
        self_name: Option<&str>,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        // Desugar destructuring parameters (`\() ->`, `\(Id n) ->`, `\(a,b) ->`,
        // `\{x} ->`) into a fresh variable per such parameter whose body matches
        // it with a single-arm `case`, reusing the case/pattern compiler.
        let (owned_params, owned_body);
        let (params, body) = if params.iter().any(|(p, _)| simple_param_name(p).is_none()) {
            let (np, nb) = desugar_destructuring_params(&mut self.fresh_id, params, body);
            owned_params = np;
            owned_body = nb;
            (owned_params.as_slice(), &owned_body)
        } else {
            (params, body)
        };

        // Parameter names (all simple after desugaring).
        let mut param_names = Vec::with_capacity(params.len());
        for (p, _) in params {
            match simple_param_name(p) {
                Some(n) => param_names.push(n),
                None => {
                    return Err(
                        "typed backend: destructuring closure parameters are not supported"
                            .to_string(),
                    )
                }
            }
        }

        // Free variables = referenced locals not bound within the lambda. A
        // recursive function's own name is not a capture: it is supplied inside
        // the body from the environment pointer.
        let mut bound: std::collections::HashSet<String> =
            param_names.iter().cloned().collect();
        if let Some(sn) = self_name {
            bound.insert(sn.to_string());
        }
        let mut refs = Vec::new();
        free_vars(body, &mut bound, &mut refs);
        let captures: Vec<(String, BasicValueEnum<'ctx>)> = refs
            .iter()
            .filter_map(|n| self.locals.get(n).map(|v| (n.clone(), *v)))
            .collect();

        let ptr_t = self.ctx.ptr_type(inkwell::AddressSpace::default());
        // Closure struct: {fn_ptr, capture types...}.
        let mut struct_fields: Vec<BasicTypeEnum> = vec![ptr_t.into()];
        for (_, v) in &captures {
            struct_fields.push(v.get_type());
        }
        let clos_ty = self.ctx.struct_type(&struct_fields, false);

        // Lifted function type: (env, params...) -> result.
        let ret_ty = self.llvm_type(result_layout);
        let mut fn_params: Vec<BasicMetadataTypeEnum> = vec![ptr_t.into()];
        for (_, t) in params {
            fn_params.push(self.llvm_type(&self.layouts.layout_of(t)).into());
        }
        let fn_ty = ret_ty.fn_type(&fn_params, false);
        let name = format!("lam.{}", self.lam_id);
        self.lam_id += 1;
        let lifted = self.module.add_function(&name, fn_ty, Some(Linkage::Internal));

        // Emit the lifted body with fresh codegen state.
        let saved_locals = std::mem::take(&mut self.locals);
        let saved_fn = self.cur_fn;
        let saved_block = self.builder.get_insert_block();
        let saved_loc = self.cur_loc;
        self.cur_fn = Some(lifted);
        let entry = self.ctx.append_basic_block(lifted, "entry");
        self.builder.position_at_end(entry);
        // A subprogram for the lifted lambda so its body carries line info in
        // the enclosing module's file; without one, clear the location so its
        // instructions don't inherit the outer function's scope.
        if let Some(file) = self.cur_file {
            let line = body.region.start.row;
            let sp = self.di_builder.create_function(
                file.as_debug_info_scope(),
                &name,
                Some(&name),
                file,
                line,
                self.di_subroutine,
                true,
                true,
                line,
                DIFlags::ZERO,
                true,
            );
            lifted.set_subprogram(sp);
        }
        self.clear_loc();
        let env = lifted.get_nth_param(0).unwrap().into_pointer_value();
        for (i, (n, _)) in captures.iter().enumerate() {
            let fp = self
                .builder
                .build_struct_gep(clos_ty, env, (i + 1) as u32, "capp")
                .unwrap();
            let v = self.builder.build_load(struct_fields[i + 1], fp, n).unwrap();
            self.locals.insert(n.clone(), v);
        }
        // A recursive function refers to itself through the environment pointer,
        // which is precisely its own closure value.
        if let Some(sn) = self_name {
            self.locals.insert(sn.to_string(), env.into());
        }
        for (idx, pn) in param_names.iter().enumerate() {
            let val = lifted.get_nth_param((idx + 1) as u32).unwrap();
            self.locals.insert(pn.clone(), val);
        }
        let body_result = self.gen(body);
        // Restore state regardless of outcome.
        self.locals = saved_locals;
        self.cur_fn = saved_fn;
        let ret = body_result?;
        self.builder.build_return(Some(&ret)).unwrap();
        if let Some(b) = saved_block {
            self.builder.position_at_end(b);
        }
        // Restore the caller's debug location: a helper must neither leak its
        // own (subprogram-less) scope nor its inner body's scope into the
        // function it returns to.
        self.restore_loc(saved_loc);

        let capture_vals: Vec<BasicValueEnum> = captures.iter().map(|(_, v)| *v).collect();
        self.emit_arity_reg(lifted);
        Ok(self
            .build_closure_value(lifted.as_global_value().as_pointer_value(), &capture_vals)
            .into())
    }

    /// Partially apply a named function: build a closure capturing the given
    /// arguments, with a wrapper that takes the remaining arguments and calls
    /// the full function.
    fn gen_partial_app(
        &mut self,
        f: FunctionValue<'ctx>,
        applied: &[BasicValueEnum<'ctx>],
    ) -> BasicValueEnum<'ctx> {
        let ptr_t = self.ctx.ptr_type(inkwell::AddressSpace::default());
        let all_params = f.get_type().get_param_types();
        let m = applied.len();
        let remaining = &all_params[m..];

        // Closure struct: {fn_ptr, applied types...}.
        let mut clos_fields: Vec<BasicTypeEnum> = vec![ptr_t.into()];
        for a in applied {
            clos_fields.push(a.get_type());
        }
        let clos_ty = self.ctx.struct_type(&clos_fields, false);

        // Wrapper: (env, remaining...) -> return.
        let mut wparams: Vec<BasicMetadataTypeEnum> = vec![ptr_t.into()];
        wparams.extend(remaining.iter().map(|t| Into::<BasicMetadataTypeEnum>::into(*t)));
        let wty = match f.get_type().get_return_type() {
            Some(ret) => ret.fn_type(&wparams, false),
            None => self.ctx.void_type().fn_type(&wparams, false),
        };
        let wname = format!("pap.{}", self.lam_id);
        self.lam_id += 1;
        let wrapper = self.module.add_function(&wname, wty, Some(Linkage::Internal));

        let saved_block = self.builder.get_insert_block();
        let saved_loc = self.cur_loc;
        let entry = self.ctx.append_basic_block(wrapper, "entry");
        self.builder.position_at_end(entry);
        self.clear_loc();
        let env = wrapper.get_nth_param(0).unwrap().into_pointer_value();
        let mut call_args: Vec<inkwell::values::BasicMetadataValueEnum> = Vec::new();
        for (i, a) in applied.iter().enumerate() {
            let fp = self.builder.build_struct_gep(clos_ty, env, (i + 1) as u32, "ap").unwrap();
            let v = self.builder.build_load(a.get_type(), fp, "apv").unwrap();
            call_args.push(v.into());
        }
        for j in 0..remaining.len() {
            call_args.push(wrapper.get_nth_param((j + 1) as u32).unwrap().into());
        }
        let call = self.builder.build_call(f, &call_args, "full").unwrap();
        match call.try_as_basic_value().left() {
            Some(v) => self.builder.build_return(Some(&v)).unwrap(),
            None => self.builder.build_return(None).unwrap(),
        };
        if let Some(b) = saved_block {
            self.builder.position_at_end(b);
        }
        // Restore the caller's debug location: a helper must neither leak its
        // own (subprogram-less) scope nor its inner body's scope into the
        // function it returns to.
        self.restore_loc(saved_loc);

        self.emit_arity_reg(wrapper);
        self.build_closure_value(wrapper.as_global_value().as_pointer_value(), applied)
            .into()
    }

    /// A canonical, module-global closure value `{fn_ptr}` for a captureless
    /// wrapper — the closure used when a named top-level function is referenced
    /// as a first-class value. Because it captures nothing it is a constant, so
    /// one global is shared across all references: they become pointer-identical
    /// (matching Elm's reference equality for functions) and cost no allocation.
    /// Keyed by the wrapper's name.
    fn global_closure(&mut self, wrapper: FunctionValue<'ctx>) -> inkwell::values::PointerValue<'ctx> {
        let key = wrapper.get_name().to_string_lossy().to_string();
        if let Some(g) = self.wrapper_closures.get(&key) {
            return *g;
        }
        let ptr_t = self.ctx.ptr_type(inkwell::AddressSpace::default());
        let sty = self.ctx.struct_type(&[ptr_t.into()], false);
        let fnptr = wrapper.as_global_value().as_pointer_value();
        let g = self.module.add_global(sty, None, &format!("{}$closv", key));
        g.set_initializer(&sty.const_named_struct(&[fnptr.into()]));
        g.set_linkage(Linkage::Internal);
        g.set_constant(true);
        let p = g.as_pointer_value();
        self.wrapper_closures.insert(key, p);
        p
    }

    /// Allocate a closure `{fn_ptr, captures...}` on the heap.
    fn build_closure_value(
        &self,
        fn_ptr: inkwell::values::PointerValue<'ctx>,
        captures: &[BasicValueEnum<'ctx>],
    ) -> inkwell::values::PointerValue<'ctx> {
        let ptr_t = self.ctx.ptr_type(inkwell::AddressSpace::default());
        let mut fields: Vec<BasicTypeEnum> = vec![ptr_t.into()];
        for c in captures {
            fields.push(c.get_type());
        }
        let sty = self.ctx.struct_type(&fields, false);
        let alloc = self.module.get_function("alm_alloc").unwrap();
        let raw = self
            .builder
            .build_call(alloc, &[sty.size_of().unwrap().into()], "clos")
            .unwrap()
            .try_as_basic_value()
            .left()
            .unwrap()
            .into_pointer_value();
        let f0 = self.builder.build_struct_gep(sty, raw, 0, "fnp").unwrap();
        self.builder.build_store(f0, fn_ptr).unwrap();
        for (i, c) in captures.iter().enumerate() {
            let fp = self.builder.build_struct_gep(sty, raw, (i + 1) as u32, "cap").unwrap();
            self.builder.build_store(fp, *c).unwrap();
        }
        raw
    }

    /// DEBUG: register a closure wrapper's true param count (env + args) at
    /// creation, under ALM_ARITY_CHECK. Emitted in the caller's current block.
    fn emit_arity_reg(&self, wrapper: FunctionValue<'ctx>) {
        if std::env::var("ALM_ARITY_CHECK").is_err() {
            return;
        }
        let f = self.module.get_function("alm_dbg_reg").unwrap();
        let fnptr = self
            .builder
            .build_ptr_to_int(wrapper.as_global_value().as_pointer_value(), self.ctx.i64_type(), "fpi")
            .unwrap();
        let arity = self.ctx.i32_type().const_int(wrapper.count_params() as u64, false);
        self.builder.build_call(f, &[fnptr.into(), arity.into()], "").unwrap();
    }

    /// Apply a closure value to already-evaluated arguments (saturated).
    fn apply_closure(
        &self,
        closure: inkwell::values::PointerValue<'ctx>,
        args: &[BasicValueEnum<'ctx>],
        result_layout: &Layout,
    ) -> BasicValueEnum<'ctx> {
        let ptr_t = self.ctx.ptr_type(inkwell::AddressSpace::default());
        // The function pointer is field 0, at offset 0.
        let fn_ptr = self
            .builder
            .build_load(ptr_t, closure, "fnptr")
            .unwrap()
            .into_pointer_value();
        if std::env::var("ALM_ARITY_CHECK").is_ok() {
            let f = self.module.get_function("alm_dbg_check").unwrap();
            let fpi = self.builder.build_ptr_to_int(fn_ptr, self.ctx.i64_type(), "fpc").unwrap();
            let argc = self.ctx.i32_type().const_int((args.len() + 1) as u64, false);
            self.builder.build_call(f, &[fpi.into(), argc.into()], "").unwrap();
        }
        let ret_ty = self.llvm_type(result_layout);
        let mut ptypes: Vec<BasicMetadataTypeEnum> = vec![ptr_t.into()];
        ptypes.extend(
            args.iter()
                .map(|a| Into::<BasicMetadataTypeEnum>::into(a.get_type())),
        );
        let fn_ty = ret_ty.fn_type(&ptypes, false);
        let mut argv: Vec<inkwell::values::BasicMetadataValueEnum> = vec![closure.into()];
        argv.extend(
            args.iter()
                .map(|a| Into::<inkwell::values::BasicMetadataValueEnum>::into(*a)),
        );
        self.builder
            .build_indirect_call(fn_ty, fn_ptr, &argv, "clcall")
            .unwrap()
            .try_as_basic_value()
            .left()
            .unwrap()
    }

    /// Apply a closure value, respecting currying. A closure of type
    /// `T1 -> .. -> Tn -> R` (R not a function) is a flat n-ary closure — it
    /// must be called with all `n` arguments at once. When fewer are supplied
    /// (e.g. `fuzz pair desc`, where the point-free `fuzz` yields a 3-ary
    /// closure applied to 2 arguments), build a partial-application closure for
    /// the rest instead of under-saturating the call (which would read garbage
    /// arguments and return a corrupt value). More arguments than arrows means
    /// the result is itself a closure applied further.
    fn apply_closure_curried(
        &mut self,
        closure: inkwell::values::PointerValue<'ctx>,
        args: &[BasicValueEnum<'ctx>],
        closure_ty: &crate::ast::canonical::Type,
    ) -> BasicValueEnum<'ctx> {
        use crate::ast::canonical::Type;
        let mut arg_tys: Vec<Type> = Vec::new();
        let mut t = closure_ty.clone();
        while let Type::Lambda(a, b) = t {
            arg_tys.push(*a);
            t = *b;
        }
        let arity = arg_tys.len();
        if arity == 0 || args.len() == arity {
            let ret_layout = self.layouts.layout_of(&t);
            return self.apply_closure(closure, args, &ret_layout);
        }
        if args.len() < arity {
            let remaining_layouts: Vec<Layout> = arg_tys[args.len()..]
                .iter()
                .map(|t| self.layouts.layout_of(t))
                .collect();
            let final_layout = self.layouts.layout_of(&t);
            return self.build_partial_closure(closure, args, &remaining_layouts, &final_layout);
        }
        // Over-application: saturate this closure, then apply the rest to the
        // closure it returns.
        let ret_layout = self.layouts.layout_of(&t);
        let mid = self
            .apply_closure(closure, &args[..arity], &ret_layout)
            .into_pointer_value();
        self.apply_closure_curried(mid, &args[arity..], &t)
    }

    /// Build a partial-application closure over a closure *value*: capture the
    /// original closure plus the already-supplied arguments in a wrapper that
    /// takes the remaining arguments and applies the original to all of them.
    fn build_partial_closure(
        &mut self,
        closure: inkwell::values::PointerValue<'ctx>,
        applied: &[BasicValueEnum<'ctx>],
        remaining_layouts: &[Layout],
        final_layout: &Layout,
    ) -> BasicValueEnum<'ctx> {
        let ptr_t = self.ctx.ptr_type(inkwell::AddressSpace::default());
        // Captures: [original closure, applied args...].
        let mut caps: Vec<BasicValueEnum<'ctx>> = Vec::with_capacity(applied.len() + 1);
        caps.push(closure.into());
        caps.extend_from_slice(applied);
        let mut clos_fields: Vec<BasicTypeEnum> = vec![ptr_t.into()];
        for c in &caps {
            clos_fields.push(c.get_type());
        }
        let clos_ty = self.ctx.struct_type(&clos_fields, false);

        // Wrapper: (env, remaining...) -> final result.
        let mut wparams: Vec<BasicMetadataTypeEnum> = vec![ptr_t.into()];
        for l in remaining_layouts {
            wparams.push(self.llvm_type(l).into());
        }
        let ret_ty = self.llvm_type(final_layout);
        let wty = ret_ty.fn_type(&wparams, false);
        let wname = format!("pap.{}", self.lam_id);
        self.lam_id += 1;
        let wrapper = self.module.add_function(&wname, wty, Some(Linkage::Internal));

        let saved_block = self.builder.get_insert_block();
        let saved_loc = self.cur_loc;
        let entry = self.ctx.append_basic_block(wrapper, "entry");
        self.builder.position_at_end(entry);
        self.clear_loc();
        let env = wrapper.get_nth_param(0).unwrap().into_pointer_value();
        let origp = self.builder.build_struct_gep(clos_ty, env, 1, "origp").unwrap();
        let orig = self
            .builder
            .build_load(ptr_t, origp, "orig")
            .unwrap()
            .into_pointer_value();
        let mut all: Vec<BasicValueEnum<'ctx>> = Vec::new();
        for (i, a) in applied.iter().enumerate() {
            let fp = self
                .builder
                .build_struct_gep(clos_ty, env, (i + 2) as u32, "ap")
                .unwrap();
            all.push(self.builder.build_load(a.get_type(), fp, "apv").unwrap());
        }
        for j in 0..remaining_layouts.len() {
            all.push(wrapper.get_nth_param((j + 1) as u32).unwrap());
        }
        let result = self.apply_closure(orig, &all, final_layout);
        self.builder.build_return(Some(&result)).unwrap();
        if let Some(b) = saved_block {
            self.builder.position_at_end(b);
        }
        self.restore_loc(saved_loc);

        self.emit_arity_reg(wrapper);
        self.build_closure_value(wrapper.as_global_value().as_pointer_value(), &caps)
            .into()
    }

    /// A closure wrapping a named function used as a first-class value: an
    /// env-ignoring trampoline `wrap(env, args...) = f(args...)`, cached.
    ///
    /// `tipe` is the function's value type. A point-free top-level definition
    /// (e.g. `f = g []`) compiles to fewer LLVM parameters than its type
    /// arity, but the closure ABI is arity-less: whoever applies this closure
    /// passes one argument per arrow in the type. So when the compiled arity
    /// is short, the trampoline is eta-expanded — it calls the target with the
    /// arguments it does take, then applies the returned closure to the rest.
    fn wrap_global(
        &mut self,
        mangled: &str,
        tipe: &crate::ast::canonical::Type,
    ) -> inkwell::values::PointerValue<'ctx> {
        if let Some(w) = self.wrappers.get(mangled) {
            let w = *w;
            self.emit_arity_reg(w);
            return self.global_closure(w);
        }
        let compiled = self.functions[mangled].count_params() as usize;
        let type_arity = {
            let mut n = 0;
            let mut t = tipe;
            while let crate::ast::canonical::Type::Lambda(_, b) = t {
                n += 1;
                t = b;
            }
            n
        };
        if type_arity > compiled {
            return self.wrap_global_eta(mangled, tipe, compiled, type_arity);
        }
        let target = self.functions[mangled];
        let ptr_t = self.ctx.ptr_type(inkwell::AddressSpace::default());
        let target_ty = target.get_type();
        let param_types = target_ty.get_param_types();
        let mut wrap_params: Vec<BasicMetadataTypeEnum> = vec![ptr_t.into()];
        wrap_params.extend(
            param_types
                .iter()
                .map(|t| Into::<BasicMetadataTypeEnum>::into(*t)),
        );
        let wrap_ty = match target_ty.get_return_type() {
            Some(ret) => ret.fn_type(&wrap_params, false),
            None => self.ctx.void_type().fn_type(&wrap_params, false),
        };
        let wname = format!("{}$clos", mangled);
        let wrapper = self.module.add_function(&wname, wrap_ty, Some(Linkage::Internal));

        let saved_block = self.builder.get_insert_block();
        let saved_loc = self.cur_loc;
        let entry = self.ctx.append_basic_block(wrapper, "entry");
        self.builder.position_at_end(entry);
        self.clear_loc();
        let fwd: Vec<inkwell::values::BasicMetadataValueEnum> = (0..param_types.len())
            .map(|i| wrapper.get_nth_param((i + 1) as u32).unwrap().into())
            .collect();
        let call = self.builder.build_call(target, &fwd, "fwd").unwrap();
        match call.try_as_basic_value().left() {
            Some(v) => self.builder.build_return(Some(&v)).unwrap(),
            None => self.builder.build_return(None).unwrap(),
        };
        if let Some(b) = saved_block {
            self.builder.position_at_end(b);
        }
        // Restore the caller's debug location: a helper must neither leak its
        // own (subprogram-less) scope nor its inner body's scope into the
        // function it returns to.
        self.restore_loc(saved_loc);
        self.wrappers.insert(mangled.to_string(), wrapper);
        self.emit_arity_reg(wrapper);
        self.global_closure(wrapper)
    }

    /// The eta-expanding variant of `wrap_global` for a point-free (under-arity)
    /// global. The trampoline takes one parameter per arrow in the type; it
    /// calls the target with its `compiled` leading parameters (producing a
    /// closure) and applies that closure to the remaining `type_arity -
    /// compiled` parameters, so the closure the caller applies is saturated the
    /// same way an eta-expanded definition would be.
    fn wrap_global_eta(
        &mut self,
        mangled: &str,
        tipe: &crate::ast::canonical::Type,
        compiled: usize,
        type_arity: usize,
    ) -> inkwell::values::PointerValue<'ctx> {
        use crate::ast::canonical::Type;
        // The layout of each argument (one per arrow) and of the final result.
        let mut arg_layouts = Vec::with_capacity(type_arity);
        let mut t = tipe;
        while let Type::Lambda(a, b) = t {
            arg_layouts.push(self.layouts.layout_of(a));
            t = b;
        }
        let ret_layout = self.layouts.layout_of(t);

        let target = self.functions[mangled];
        let ptr_t = self.ctx.ptr_type(inkwell::AddressSpace::default());
        let mut wrap_params: Vec<BasicMetadataTypeEnum> = vec![ptr_t.into()];
        for l in &arg_layouts {
            wrap_params.push(self.llvm_type(l).into());
        }
        let ret_ty = self.llvm_type(&ret_layout);
        let wrap_ty = ret_ty.fn_type(&wrap_params, false);
        let wname = format!("{}$clos", mangled);
        let wrapper = self.module.add_function(&wname, wrap_ty, Some(Linkage::Internal));

        let saved_block = self.builder.get_insert_block();
        let saved_loc = self.cur_loc;
        let entry = self.ctx.append_basic_block(wrapper, "entry");
        self.builder.position_at_end(entry);
        self.clear_loc();

        // Call the target with the leading arguments it actually takes.
        let lead: Vec<inkwell::values::BasicMetadataValueEnum> = (0..compiled)
            .map(|i| wrapper.get_nth_param((i + 1) as u32).unwrap().into())
            .collect();
        let inner = self
            .builder
            .build_call(target, &lead, "eta")
            .unwrap()
            .try_as_basic_value()
            .left()
            .unwrap()
            .into_pointer_value();
        // Apply the returned closure to the remaining arguments.
        let rest: Vec<BasicValueEnum<'ctx>> = (compiled..type_arity)
            .map(|i| wrapper.get_nth_param((i + 1) as u32).unwrap())
            .collect();
        let result = self.apply_closure(inner, &rest, &ret_layout);
        self.builder.build_return(Some(&result)).unwrap();

        if let Some(b) = saved_block {
            self.builder.position_at_end(b);
        }
        self.restore_loc(saved_loc);
        self.wrappers.insert(mangled.to_string(), wrapper);
        self.emit_arity_reg(wrapper);
        self.global_closure(wrapper)
    }

    fn gen_negate(&mut self, inner: &TypedExpr) -> Result<BasicValueEnum<'ctx>, String> {
        let v = self.gen(inner)?;
        Ok(match self.layouts.layout_of(&inner.tipe) {
            Layout::Float => self
                .builder
                .build_float_neg(v.into_float_value(), "neg")
                .unwrap()
                .into(),
            _ => self
                .builder
                .build_int_neg(v.into_int_value(), "neg")
                .unwrap()
                .into(),
        })
    }

    /// Coerce a scalar operand to `f64`. A polymorphic number literal in a
    /// float context can arrive as an `i64` (its `number` type defaulted to
    /// `Int` in `layout_of`), so widen it — in well-typed Elm an integer can
    /// only meet a float through such a literal.
    fn to_float(&self, v: BasicValueEnum<'ctx>) -> inkwell::values::FloatValue<'ctx> {
        if v.is_float_value() {
            v.into_float_value()
        } else {
            self.builder
                .build_signed_int_to_float(v.into_int_value(), self.ctx.f64_type(), "itof")
                .unwrap()
        }
    }

    fn gen_binop(
        &mut self,
        op: &str,
        l: &TypedExpr,
        r: &TypedExpr,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let layout = self.layouts.layout_of(&l.tipe);
        // Comparisons on comparables that are not a single scalar register must
        // compare contents, not the raw machine value: strings and unresolved
        // `comparable`/opaque words (already uniform), and the compound
        // comparables tuples and lists (elm allows ordering these), which we box
        // to the uniform representation so the runtime does the structural
        // lexicographic comparison. Route through the runtime's value ops.
        if matches!(
            layout,
            Layout::Str | Layout::Ref | Layout::Opaque | Layout::Tuple(_) | Layout::List(_)
        ) {
            let sym = match op {
                "==" => "rt_eq",
                "/=" => "rt_neq",
                "<" => "rt_lt",
                "<=" => "rt_le",
                ">" => "rt_gt",
                ">=" => "rt_ge",
                _ => {
                    return Err(format!(
                        "typed backend: `{}` is not supported on layout {:?}",
                        op, layout
                    ))
                }
            };
            let i64_t = self.ctx.i64_type();
            let compound = matches!(layout, Layout::Tuple(_) | Layout::List(_));
            let lv = self.gen(l)?;
            let rv = self.gen(r)?;
            // Compound comparables (tuple/list) are unboxed structs; box them to
            // the uniform value. Strings/opaques are already the word; a Ref is a
            // pointer whose word is its address.
            let lw = if compound {
                self.box_value(lv, &l.tipe)?
            } else if lv.is_pointer_value() {
                self.builder
                    .build_ptr_to_int(lv.into_pointer_value(), i64_t, "w")
                    .unwrap()
                    .into()
            } else {
                lv
            };
            let rw = if compound {
                self.box_value(rv, &r.tipe)?
            } else if rv.is_pointer_value() {
                self.builder
                    .build_ptr_to_int(rv.into_pointer_value(), i64_t, "w")
                    .unwrap()
                    .into()
            } else {
                rv
            };
            let cmp = self.call_named(sym, &[lw, rw]);
            return Ok(self.call_named("rt_is_true", &[cmp]));
        }
        // A float op if either operand is a float — a number literal on the
        // other side defaults to `Int` in `layout_of` but must widen (matching
        // the runtime's rt_add "either side float" rule).
        let is_float = matches!(layout, Layout::Float)
            || matches!(self.layouts.layout_of(&r.tipe), Layout::Float);
        let lv = self.gen(l)?;
        let rv = self.gen(r)?;
        if is_float {
            let (x, y) = (self.to_float(lv), self.to_float(rv));
            let b = &self.builder;
            let v: BasicValueEnum = match op {
                "+" => b.build_float_add(x, y, "f").unwrap().into(),
                "-" => b.build_float_sub(x, y, "f").unwrap().into(),
                "*" => b.build_float_mul(x, y, "f").unwrap().into(),
                "/" => b.build_float_div(x, y, "f").unwrap().into(),
                "^" => self.call_f64_intrinsic2("llvm.pow.f64", x, y).into(),
                "==" => cmp_f(b, FloatPredicate::OEQ, x, y),
                "/=" => cmp_f(b, FloatPredicate::ONE, x, y),
                "<" => cmp_f(b, FloatPredicate::OLT, x, y),
                "<=" => cmp_f(b, FloatPredicate::OLE, x, y),
                ">" => cmp_f(b, FloatPredicate::OGT, x, y),
                ">=" => cmp_f(b, FloatPredicate::OGE, x, y),
                _ => return Err(format!("typed backend: unsupported float op `{}`", op)),
            };
            Ok(v)
        } else {
            let b = &self.builder;
            let (x, y) = (lv.into_int_value(), rv.into_int_value());
            let v: BasicValueEnum = match op {
                "+" => b.build_int_add(x, y, "i").unwrap().into(),
                "-" => b.build_int_sub(x, y, "i").unwrap().into(),
                "*" => b.build_int_mul(x, y, "i").unwrap().into(),
                "//" => b.build_int_signed_div(x, y, "i").unwrap().into(),
                // Integer exponentiation matches the JS backend's `Math.pow`:
                // compute in f64 and truncate back to i64.
                "^" => {
                    let f64t = self.ctx.f64_type();
                    let xf = b.build_signed_int_to_float(x, f64t, "xf").unwrap();
                    let yf = b.build_signed_int_to_float(y, f64t, "yf").unwrap();
                    let rf = self.call_f64_intrinsic2("llvm.pow.f64", xf, yf);
                    b.build_float_to_signed_int(rf, self.ctx.i64_type(), "powi")
                        .unwrap()
                        .into()
                }
                "==" => cmp_i(b, IntPredicate::EQ, x, y),
                "/=" => cmp_i(b, IntPredicate::NE, x, y),
                "<" => cmp_i(b, IntPredicate::SLT, x, y),
                "<=" => cmp_i(b, IntPredicate::SLE, x, y),
                ">" => cmp_i(b, IntPredicate::SGT, x, y),
                ">=" => cmp_i(b, IntPredicate::SGE, x, y),
                _ => return Err(format!("typed backend: unsupported int op `{}`", op)),
            };
            Ok(v)
        }
    }

    fn gen_if(
        &mut self,
        branches: &[(TypedExpr, TypedExpr)],
        otherwise: &TypedExpr,
        whole: &TypedExpr,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let result_ty = self.llvm_type(&self.layouts.layout_of(&whole.tipe));
        let merge = self.new_block("if.end");
        let mut incoming: Vec<(BasicValueEnum<'ctx>, inkwell::basic_block::BasicBlock<'ctx>)> =
            Vec::new();

        for (cond, then) in branches {
            let cv = self.gen(cond)?.into_int_value();
            let then_bb = self.new_block("if.then");
            let else_bb = self.new_block("if.else");
            self.builder
                .build_conditional_branch(cv, then_bb, else_bb)
                .unwrap();
            self.builder.position_at_end(then_bb);
            // Widen an Int-literal branch to a Float result (an unresolved
            // `number` defaulted to Int on one arm of a Float-typed `if`).
            let tv = self.gen(then)?;
            let tv = self.coerce_to_slot(tv, Some(result_ty));
            incoming.push((tv, self.builder.get_insert_block().unwrap()));
            self.builder.build_unconditional_branch(merge).unwrap();
            self.builder.position_at_end(else_bb);
        }
        let ev = self.gen(otherwise)?;
        let ev = self.coerce_to_slot(ev, Some(result_ty));
        incoming.push((ev, self.builder.get_insert_block().unwrap()));
        self.builder.build_unconditional_branch(merge).unwrap();

        self.builder.position_at_end(merge);
        let phi = self.builder.build_phi(result_ty, "if").unwrap();
        for (val, bb) in &incoming {
            phi.add_incoming(&[(val as &dyn BasicValue, *bb)]);
        }
        Ok(phi.as_basic_value())
    }

    /// Compile a `case`. Each branch is matched as a short-circuiting decision
    /// tree: [`match_pattern`] emits the shape/literal tests, branching to the
    /// next branch on any mismatch — including *nested* refutable sub-patterns
    /// (e.g. `(Just x, _)`, `[ a, 0 ]`, `Ok (n :: _)`) — and binds the pattern's
    /// variables along the matched path. Branch results feed a phi at the join.
    fn gen_case(
        &mut self,
        scrutinee: &TypedExpr,
        branches: &[(crate::ast::canonical::Pattern, TypedExpr)],
        whole: &TypedExpr,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let subject = self.gen(scrutinee)?;
        let result_ty = self.llvm_type(&self.layouts.layout_of(&whole.tipe));
        let merge = self.new_block("case.end");
        let mut incoming: Vec<(BasicValueEnum<'ctx>, inkwell::basic_block::BasicBlock<'ctx>)> =
            Vec::new();
        let mut matched_all = false;

        for (pattern, body) in branches {
            // On mismatch the tests jump to `fail`, where the next branch is
            // compiled; on a full match the builder falls through with the
            // pattern's variables bound.
            let fail = self.new_block("case.next");
            let refutable = self.match_pattern(pattern, subject, &scrutinee.tipe, fail)?;
            // Widen an Int-literal branch to a Float result (see gen_if).
            let v = self.gen(body)?;
            let v = self.coerce_to_slot(v, Some(result_ty));
            incoming.push((v, self.builder.get_insert_block().unwrap()));
            self.builder.build_unconditional_branch(merge).unwrap();
            self.builder.position_at_end(fail);
            if !refutable {
                // The branch always matches; its `fail` block is unreachable.
                self.builder.build_unreachable().unwrap();
                matched_all = true;
                break;
            }
        }

        // Elm case-expressions are exhaustive; falling past the last refutable
        // branch is unreachable.
        if !matched_all {
            self.builder.build_unreachable().unwrap();
        }

        self.builder.position_at_end(merge);
        let phi = self.builder.build_phi(result_ty, "case").unwrap();
        for (val, bb) in &incoming {
            phi.add_incoming(&[(val as &dyn BasicValue, *bb)]);
        }
        Ok(phi.as_basic_value())
    }

    /// Branch to a fresh continuation block when `cond` holds, else to `fail`,
    /// then position the builder in the continuation. Used to thread pattern
    /// tests: a mismatch drops through to the next `case` branch.
    fn branch_or_fail(
        &mut self,
        cond: inkwell::values::IntValue<'ctx>,
        fail: inkwell::basic_block::BasicBlock<'ctx>,
    ) {
        let cont = self.new_block("case.cont");
        self.builder
            .build_conditional_branch(cond, cont, fail)
            .unwrap();
        self.builder.position_at_end(cont);
    }

    /// Compile a pattern match: emit the tests deciding whether `value`
    /// matches `pattern`, branching to `fail` on any mismatch, and bind the
    /// pattern's variables along the successful path. Descends into aggregate
    /// patterns (tuples, records, lists, cons cells, constructors) so nested
    /// refutable sub-patterns are tested too; each shape test is emitted before
    /// the sub-values it guards are read, so extraction is always well-defined.
    /// Returns whether the pattern was refutable (emitted a branch to `fail`).
    fn match_pattern(
        &mut self,
        pattern: &crate::ast::canonical::Pattern,
        value: BasicValueEnum<'ctx>,
        tipe: &crate::ast::canonical::Type,
        fail: inkwell::basic_block::BasicBlock<'ctx>,
    ) -> Result<bool, String> {
        use crate::ast::canonical::Pattern_::*;
        // Type-directed: the layout is derived here, but sub-patterns recurse on
        // their *types*, so a constructor field of a recursive type (whose
        // layout is `Ref`) is still matched against the concrete union rather
        // than bottoming out at `Ref`. Mirrors `equals_typed`.
        let layout = self.layouts.layout_of(tipe);
        match &pattern.value {
            Var(name) => {
                self.locals.insert(name.to_string(), value);
                Ok(false)
            }
            Anything | Unit => Ok(false),
            Alias(inner, name) => {
                self.locals.insert(name.value.to_string(), value);
                self.match_pattern(inner, value, tipe, fail)
            }
            Int(n) => {
                let cond = self
                    .builder
                    .build_int_compare(
                        IntPredicate::EQ,
                        value.into_int_value(),
                        self.ctx.i64_type().const_int(*n as u64, true),
                        "casei",
                    )
                    .unwrap();
                self.branch_or_fail(cond, fail);
                Ok(true)
            }
            Chr(c) => {
                let cond = self
                    .builder
                    .build_int_compare(
                        IntPredicate::EQ,
                        value.into_int_value(),
                        self.ctx.i32_type().const_int(*c as u64, false),
                        "casec",
                    )
                    .unwrap();
                self.branch_or_fail(cond, fail);
                Ok(true)
            }
            Str(s) => {
                let lit = self.gen_string(s)?;
                let eq = self.call_named("rt_eq", &[value, lit]);
                let cond = self.call_named("rt_is_true", &[eq]).into_int_value();
                self.branch_or_fail(cond, fail);
                Ok(true)
            }
            Tuple(a, b, rest) => {
                use crate::ast::canonical::Type;
                let sub_types: Vec<Type> = match tipe {
                    Type::Tuple(x, y, z) => {
                        let mut v = vec![(**x).clone(), (**y).clone()];
                        if let Some(z) = z {
                            v.push((**z).clone());
                        }
                        v
                    }
                    _ => return Err("typed backend: tuple pattern on non-tuple value".to_string()),
                };
                let sv = value.into_struct_value();
                let parts: Vec<&crate::ast::canonical::Pattern> = std::iter::once(a.as_ref())
                    .chain(std::iter::once(b.as_ref()))
                    .chain(rest.iter())
                    .collect();
                let mut refutable = false;
                for (i, p) in parts.into_iter().enumerate() {
                    let elem = self.builder.build_extract_value(sv, i as u32, "elt").unwrap();
                    refutable |= self.match_pattern(p, elem, &sub_types[i], fail)?;
                }
                Ok(refutable)
            }
            Record(field_names) => {
                let Layout::Record(fields) = &layout else {
                    return Err("typed backend: record pattern on non-record value".to_string());
                };
                let sv = value.into_struct_value();
                // Record patterns bind field names; they carry no sub-patterns.
                for located in field_names {
                    let idx = fields
                        .iter()
                        .position(|(n, _)| n.as_str() == located.value.as_str())
                        .ok_or_else(|| {
                            format!("typed backend: record has no field `{}`", located.value)
                        })?;
                    let elem = self
                        .builder
                        .build_extract_value(sv, idx as u32, "field")
                        .unwrap();
                    self.locals.insert(located.value.to_string(), elem);
                }
                Ok(false)
            }
            List(elems) => {
                use crate::ast::canonical::Type;
                let (Layout::List(elem_layout), Type::Type(_, _, targs)) = (&layout, tipe) else {
                    return Err("typed backend: list pattern on non-list value".to_string());
                };
                let elem_layout = (**elem_layout).clone();
                let elem_type = targs.first().cloned().ok_or_else(|| {
                    "typed backend: list pattern on a list type without an element".to_string()
                })?;
                let list = value.into_pointer_value();
                let len = self.list_len(list);
                let want = self.ctx.i64_type().const_int(elems.len() as u64, false);
                let cond = self
                    .builder
                    .build_int_compare(IntPredicate::EQ, len, want, "listlen")
                    .unwrap();
                self.branch_or_fail(cond, fail);
                // Elements are stored reversed: element k is data[len - 1 - k].
                let backing = self.list_backing(list);
                let n = elems.len();
                for (k, p) in elems.iter().enumerate() {
                    let idx = self.ctx.i64_type().const_int((n - 1 - k) as u64, false);
                    let elem = self.list_load(backing, &elem_layout, idx);
                    self.match_pattern(p, elem, &elem_type, fail)?;
                }
                Ok(true)
            }
            Cons(head, tail) => {
                use crate::ast::canonical::Type;
                let (Layout::List(elem_layout), Type::Type(_, _, targs)) = (&layout, tipe) else {
                    return Err("typed backend: cons pattern on non-list value".to_string());
                };
                let elem_layout = (**elem_layout).clone();
                let elem_type = targs.first().cloned().ok_or_else(|| {
                    "typed backend: cons pattern on a list type without an element".to_string()
                })?;
                let list = value.into_pointer_value();
                let len = self.list_len(list);
                let cond = self
                    .builder
                    .build_int_compare(
                        IntPredicate::NE,
                        len,
                        self.ctx.i64_type().const_zero(),
                        "iscons",
                    )
                    .unwrap();
                self.branch_or_fail(cond, fail);
                // Head is data[len - 1]; tail shares the backing with length len - 1.
                let backing = self.list_backing(list);
                let one = self.ctx.i64_type().const_int(1, false);
                let last = self.builder.build_int_sub(len, one, "last").unwrap();
                let head_val = self.list_load(backing, &elem_layout, last);
                self.match_pattern(head, head_val, &elem_type, fail)?;
                let tail_val = self.make_list(last, backing);
                self.match_pattern(tail, tail_val.into(), tipe, fail)?;
                Ok(true)
            }
            Ctor(_, _, ctor, args) => match &layout {
                Layout::Bool => {
                    let cond = self
                        .builder
                        .build_int_compare(
                            IntPredicate::EQ,
                            value.into_int_value(),
                            self.ctx
                                .bool_type()
                                .const_int((ctor.name.as_str() == "True") as u64, false),
                            "casebool",
                        )
                        .unwrap();
                    self.branch_or_fail(cond, fail);
                    Ok(true)
                }
                Layout::Enum(_) => {
                    let cond = self
                        .builder
                        .build_int_compare(
                            IntPredicate::EQ,
                            value.into_int_value(),
                            self.ctx.i32_type().const_int(ctor.index as u64, false),
                            "caseenum",
                        )
                        .unwrap();
                    self.branch_or_fail(cond, fail);
                    Ok(true)
                }
                // A data-carrying union is a heap `{tag, fields}`. `Ref` is the
                // same representation reached through a recursive occurrence
                // (its layout was broken to `Ref`); both dispatch identically,
                // recovering the constructors' field *types* from the concrete
                // union type so field sub-patterns keep their type as they
                // recurse. This is what lets `case` match a recursive field.
                Layout::Tagged(_) | Layout::Ref => {
                    let ctors = self.layouts.union_ctors(tipe).ok_or_else(|| {
                        format!("typed backend: case on non-union type for `{}`", ctor.name)
                    })?;
                    let ptr = value.into_pointer_value();
                    let tag = self
                        .builder
                        .build_load(self.ctx.i32_type(), ptr, "tag")
                        .unwrap()
                        .into_int_value();
                    let cond = self
                        .builder
                        .build_int_compare(
                            IntPredicate::EQ,
                            tag,
                            self.ctx.i32_type().const_int(ctor.index as u64, false),
                            "casetag",
                        )
                        .unwrap();
                    self.branch_or_fail(cond, fail);
                    // The tag matched: bind this constructor's fields.
                    if !args.is_empty() {
                        let field_types = ctors
                            .get(ctor.index as usize)
                            .map(|(_, fts)| fts.clone())
                            .ok_or_else(|| {
                                format!("typed backend: bad ctor index for `{}`", ctor.name)
                            })?;
                        let field_layouts: Vec<Layout> =
                            field_types.iter().map(|t| self.layouts.layout_of(t)).collect();
                        let struct_ty = self.ctor_struct(&field_layouts);
                        for (i, argpat) in args.iter().enumerate() {
                            let fp = self
                                .builder
                                .build_struct_gep(struct_ty, ptr, (i + 1) as u32, "fp")
                                .unwrap();
                            let v = self
                                .builder
                                .build_load(self.llvm_type(&field_layouts[i]), fp, "fld")
                                .unwrap();
                            self.match_pattern(argpat, v, &field_types[i], fail)?;
                        }
                    }
                    Ok(true)
                }
                other => Err(format!(
                    "typed backend: case on layout {:?} is not supported yet",
                    other
                )),
            },
        }
    }

    /// An alloca placed at the top of the current function's entry block, so
    /// it is executed once even when the using code runs inside a loop.
    fn entry_alloca(
        &self,
        ty: BasicTypeEnum<'ctx>,
        name: &str,
    ) -> inkwell::values::PointerValue<'ctx> {
        let entry = self.cur_fn.unwrap().get_first_basic_block().unwrap();
        let b = self.ctx.create_builder();
        match entry.get_first_instruction() {
            Some(inst) => b.position_before(&inst),
            None => b.position_at_end(entry),
        }
        b.build_alloca(ty, name).unwrap()
    }

    fn new_block(&mut self, base: &str) -> inkwell::basic_block::BasicBlock<'ctx> {
        self.blk += 1;
        self.ctx
            .append_basic_block(self.cur_fn.unwrap(), &format!("{}{}", base, self.blk))
    }
}

fn cmp_i<'ctx>(
    b: &inkwell::builder::Builder<'ctx>,
    pred: IntPredicate,
    x: inkwell::values::IntValue<'ctx>,
    y: inkwell::values::IntValue<'ctx>,
) -> BasicValueEnum<'ctx> {
    b.build_int_compare(pred, x, y, "cmp").unwrap().into()
}

fn cmp_f<'ctx>(
    b: &inkwell::builder::Builder<'ctx>,
    pred: FloatPredicate,
    x: inkwell::values::FloatValue<'ctx>,
    y: inkwell::values::FloatValue<'ctx>,
) -> BasicValueEnum<'ctx> {
    b.build_float_compare(pred, x, y, "cmp").unwrap().into()
}

/// The runtime symbol for a Dict/Set/Array operation, or None if unknown.
fn collection_symbol(module: &str, name: &str) -> Option<&'static str> {
    Some(match (module, name) {
        ("Dict", "singleton") => "dict_singleton",
        ("Dict", "insert") => "dict_insert",
        ("Dict", "get") => "dict_get",
        ("Dict", "remove") => "dict_remove",
        ("Dict", "member") => "dict_member",
        ("Dict", "isEmpty") => "dict_is_empty",
        ("Dict", "size") => "dict_size",
        ("Dict", "keys") => "dict_keys",
        ("Dict", "values") => "dict_values",
        ("Dict", "toList") => "dict_to_list",
        ("Dict", "fromList") => "dict_from_list",
        ("Dict", "foldl") => "dict_foldl",
        ("Dict", "foldr") => "dict_foldr",
        ("Dict", "map") => "dict_map",
        ("Dict", "filter") => "dict_filter",
        ("Dict", "update") => "dict_update",
        ("Dict", "union") => "dict_union",
        ("Dict", "intersect") => "dict_intersect",
        ("Dict", "diff") => "dict_diff",
        ("Dict", "partition") => "dict_partition",
        ("Dict", "merge") => "dict_merge",
        ("Set", "singleton") => "set_singleton",
        ("Set", "insert") => "set_insert",
        ("Set", "remove") => "set_remove",
        ("Set", "member") => "set_member",
        ("Set", "isEmpty") => "set_is_empty",
        ("Set", "size") => "set_size",
        ("Set", "toList") => "set_to_list",
        ("Set", "fromList") => "set_from_list",
        ("Set", "union") => "set_union",
        ("Set", "intersect") => "set_intersect",
        ("Set", "diff") => "set_diff",
        ("Set", "foldl") => "set_foldl",
        ("Set", "foldr") => "set_foldr",
        ("Set", "map") => "set_map",
        ("Set", "filter") => "set_filter",
        ("Array", "isEmpty") => "array_is_empty",
        ("Array", "length") => "array_length",
        ("Array", "initialize") => "array_initialize",
        ("Array", "repeat") => "array_repeat",
        ("Array", "fromList") => "array_from_list",
        ("Array", "toList") => "array_to_list",
        ("Array", "get") => "array_get",
        ("Array", "set") => "array_set",
        ("Array", "push") => "array_push",
        ("Array", "foldl") => "array_foldl",
        ("Array", "foldr") => "array_foldr",
        ("Array", "map") => "array_map",
        ("Array", "indexedMap") => "array_indexed_map",
        ("Array", "filter") => "array_filter",
        ("Array", "slice") => "array_slice",
        _ => return None,
    })
}

/// The arity of a built-in from its function type: the number of leading
/// `->` arrows.
fn foreign_arity(tipe: &crate::ast::canonical::Type) -> usize {
    use crate::ast::canonical::Type;
    let mut n = 0;
    let mut t = tipe;
    while let Type::Lambda(_, b) = t {
        n += 1;
        t = b;
    }
    n
}

/// The intrinsic arity of core builtins whose SCHEME result is a type
/// variable. Counting the instantiated type's arrows over-counts when the
/// result is itself a function — `(List.foldl (>>) identity fs) x` flattens to
/// a 4-argument call whose type has four arrows, which looked saturated: the
/// kernel then consumed the call's FINAL type as its result type and silently
/// dropped the extra argument. All other builtins' scheme arity equals their
/// arrow count, so they need no entry.
fn foreign_intrinsic_arity(module: &str, name: &str) -> Option<usize> {
    Some(match (module, name) {
        ("Basics", "identity") => 1,
        ("Basics", "always") => 2,
        ("Basics", "apL") | ("Basics", "apR") => 2,
        ("Basics", "composeL") | ("Basics", "composeR") => 3,
        ("Debug", "log") => 2,
        ("Debug", "todo") => 1,
        ("List", "foldl") | ("List", "foldr") => 3,
        ("String", "foldl") | ("String", "foldr") => 3,
        ("Array", "foldl") | ("Array", "foldr") => 3,
        ("Dict", "foldl") | ("Dict", "foldr") => 3,
        ("Set", "foldl") | ("Set", "foldr") => 3,
        ("Dict", "merge") => 6,
        ("Maybe", "withDefault") => 2,
        ("Result", "withDefault") => 2,
        ("Tuple", "first") | ("Tuple", "second") => 1,
        // `Test.Html`'s reflection kernel `taggerFunction : Tagger -> (a -> msg)`
        // is a true 1-argument extractor (it hands back the tagger's mapping
        // function), but its type has two arrows. Arrow-counting made
        // `taggerFunction tagger` look under-applied, so the backend filled the
        // phantom second argument with a unit placeholder and applied the
        // extracted function to `()` — unboxing unit as the msg ctor ('not a
        // constructor, found Unit'). It is arity 1.
        ("Elm.Kernel.HtmlAsJson", "taggerFunction") => 1,
        _ => return None,
    })
}

/// Rewrite a lambda's parameter list so every destructuring pattern becomes a
/// fresh variable, and wrap the body in nested single-arm `case` expressions
/// that re-match each fresh variable against the original pattern. This is the
/// standard `\pat -> body` ==> `\fresh -> case fresh of pat -> body`
/// desugaring, letting the typed backend reuse its case/pattern compiler for
/// unit, constructor, tuple and record parameters. Simple `Var`/`_` parameters
/// are left untouched. Earlier parameters wrap outermost.
fn desugar_destructuring_params(
    fresh: &mut usize,
    params: &[(crate::ast::canonical::Pattern, crate::ast::canonical::Type)],
    body: &TypedExpr,
) -> (
    Vec<(crate::ast::canonical::Pattern, crate::ast::canonical::Type)>,
    TypedExpr,
) {
    use crate::ast::canonical::Pattern_;
    let mut new_params = Vec::with_capacity(params.len());
    // (fresh name, original pattern, parameter type) for each destructuring param.
    let mut to_wrap: Vec<(String, crate::ast::canonical::Pattern, crate::ast::canonical::Type)> =
        Vec::new();
    for (pat, ty) in params {
        if simple_param_name(pat).is_some() {
            new_params.push((pat.clone(), ty.clone()));
        } else {
            let name = format!("$dp{}", *fresh);
            *fresh += 1;
            let var_pat = crate::reporting::Located {
                region: pat.region,
                value: Pattern_::Var(crate::data::Name::from(name.clone())),
            };
            new_params.push((var_pat, ty.clone()));
            to_wrap.push((name, pat.clone(), ty.clone()));
        }
    }
    // Fold from the last destructuring param inward so the earliest one ends up
    // as the outermost `case`.
    let mut wrapped = body.clone();
    for (name, pat, ty) in to_wrap.into_iter().rev() {
        let scrut = TypedExpr {
            tipe: ty,
            kind: TypedKind::Local(crate::data::Name::from(name)),
            region: body.region,
        };
        wrapped = TypedExpr {
            tipe: wrapped.tipe.clone(),
            kind: TypedKind::Case(Box::new(scrut), vec![(pat, wrapped)]),
            region: body.region,
        };
    }
    (new_params, wrapped)
}

/// The name a simple `Var` parameter binds, if that is all this pattern is.
fn simple_param_name(pattern: &crate::ast::canonical::Pattern) -> Option<String> {
    use crate::ast::canonical::Pattern_::*;
    match &pattern.value {
        Var(name) => Some(name.to_string()),
        Anything => Some("_".to_string()),
        _ => None,
    }
}

/// Whether `expr` contains, in tail position, a saturated call to the
/// function `mangled` (arity `nparams`). Tail positions are the branches of
/// `if`/`case` and the body of `let`; anything else is not a tail call.
fn tail_has_self_call(expr: &TypedExpr, mangled: &str, nparams: usize) -> bool {
    match &expr.kind {
        TypedKind::Call(func, args) => {
            matches!(&func.kind, TypedKind::Global(n) if n.as_str() == mangled)
                && args.len() == nparams
        }
        TypedKind::If(branches, otherwise) => {
            branches
                .iter()
                .any(|(_, b)| tail_has_self_call(b, mangled, nparams))
                || tail_has_self_call(otherwise, mangled, nparams)
        }
        TypedKind::Case(_, branches) => branches
            .iter()
            .any(|(_, b)| tail_has_self_call(b, mangled, nparams)),
        TypedKind::Let(_, body) => tail_has_self_call(body, mangled, nparams),
        _ => false,
    }
}

/// Whether a layout contains a `Ref` anywhere — i.e. the type is recursive (or
/// carries an unresolved variable). Such a value cannot be compared field by
/// field in the typed representation, because a recursive occurrence surfaces
/// as `Ref` and would be compared by pointer identity; it is boxed and compared
/// structurally by the runtime instead. Terminates: `layout_of` breaks
/// recursion with `Ref`, so the layout tree is finite.
fn layout_has_ref(l: &Layout) -> bool {
    match l {
        Layout::Ref => true,
        Layout::List(e) => layout_has_ref(e),
        Layout::Tuple(elems) => elems.iter().any(layout_has_ref),
        Layout::Record(fields) => fields.iter().any(|(_, fl)| layout_has_ref(fl)),
        Layout::Tagged(variants) => variants.iter().flatten().any(layout_has_ref),
        _ => false,
    }
}

/// Whether an expression references no local variables at all — i.e. it is a
/// compile-time constant relative to any enclosing function's arguments,
/// depending only on top-level values, kernels, and literals. Used to decide
/// whether a `let` binding can be hoisted to a memoized global.
fn is_closed(expr: &TypedExpr) -> bool {
    let mut out = Vec::new();
    free_vars(expr, &mut std::collections::HashSet::new(), &mut out);
    out.is_empty()
}

/// Collect the free variables of an expression: `Local` references not bound
/// by a binder within it. `bound` starts with the enclosing lambda's
/// parameters. Order-preserving and de-duplicated.
fn free_vars(expr: &TypedExpr, bound: &mut std::collections::HashSet<String>, out: &mut Vec<String>) {
    match &expr.kind {
        TypedKind::Local(name) => {
            let n = name.to_string();
            if !bound.contains(&n) && !out.contains(&n) {
                out.push(n);
            }
        }
        TypedKind::List(items) => items.iter().for_each(|e| free_vars(e, bound, out)),
        TypedKind::Negate(inner) => free_vars(inner, bound, out),
        TypedKind::Binop(_, _, _, l, r) => {
            free_vars(l, bound, out);
            free_vars(r, bound, out)
        }
        TypedKind::Lambda(params, body) => {
            let names: Vec<String> = params.iter().flat_map(|(p, _)| pattern_names(p)).collect();
            let added = bind_names(names.into_iter(), bound);
            free_vars(body, bound, out);
            for n in added {
                bound.remove(&n);
            }
        }
        TypedKind::Call(f, args) => {
            free_vars(f, bound, out);
            args.iter().for_each(|e| free_vars(e, bound, out))
        }
        TypedKind::If(branches, otherwise) => {
            for (c, b) in branches {
                free_vars(c, bound, out);
                free_vars(b, bound, out)
            }
            free_vars(otherwise, bound, out)
        }
        TypedKind::Let(decls, body) => {
            // Bind let-def names and destructure names for the whole block.
            let mut added = Vec::new();
            for decl in decls {
                match decl {
                    TypedLetDecl::Def { name, .. } => {
                        if bound.insert(name.to_string()) {
                            added.push(name.to_string());
                        }
                    }
                    TypedLetDecl::Recursive(defs) => {
                        for d in defs {
                            if let TypedLetDecl::Def { name, .. } = d {
                                if bound.insert(name.to_string()) {
                                    added.push(name.to_string());
                                }
                            }
                        }
                    }
                    TypedLetDecl::Destruct(p, _) => {
                        added.extend(bind_names(pattern_names(p).into_iter(), bound))
                    }
                }
            }
            for decl in decls {
                match decl {
                    TypedLetDecl::Def { body, .. } => free_vars(body, bound, out),
                    TypedLetDecl::Recursive(defs) => {
                        for d in defs {
                            if let TypedLetDecl::Def { body, .. } = d {
                                free_vars(body, bound, out)
                            }
                        }
                    }
                    TypedLetDecl::Destruct(_, value) => free_vars(value, bound, out),
                }
            }
            free_vars(body, bound, out);
            for n in added {
                bound.remove(&n);
            }
        }
        TypedKind::Case(scrutinee, branches) => {
            free_vars(scrutinee, bound, out);
            for (pat, body) in branches {
                let added = bind_names(pattern_names(pat).into_iter(), bound);
                free_vars(body, bound, out);
                for n in added {
                    bound.remove(&n);
                }
            }
        }
        TypedKind::Access(record, _) => free_vars(record, bound, out),
        TypedKind::Update(record, fields) => {
            free_vars(record, bound, out);
            fields.iter().for_each(|(_, v)| free_vars(v, bound, out))
        }
        TypedKind::Record(fields) => fields.iter().for_each(|(_, v)| free_vars(v, bound, out)),
        TypedKind::Tuple(a, b, c) => {
            free_vars(a, bound, out);
            free_vars(b, bound, out);
            if let Some(c) = c {
                free_vars(c, bound, out)
            }
        }
        // Literals, globals, foreigns, constructors reference no locals.
        _ => {}
    }
}

/// Insert names into `bound`, returning those that were newly added (so the
/// caller can remove exactly those when the scope closes).
fn bind_names(
    names: impl Iterator<Item = String>,
    bound: &mut std::collections::HashSet<String>,
) -> Vec<String> {
    let mut added = Vec::new();
    for n in names {
        if bound.insert(n.clone()) {
            added.push(n);
        }
    }
    added
}

/// Every variable name a pattern binds.
fn pattern_names(pattern: &crate::ast::canonical::Pattern) -> Vec<String> {
    let mut names = Vec::new();
    fn go(p: &crate::ast::canonical::Pattern, out: &mut Vec<String>) {
        use crate::ast::canonical::Pattern_::*;
        match &p.value {
            Var(n) => out.push(n.to_string()),
            Alias(inner, n) => {
                out.push(n.value.to_string());
                go(inner, out)
            }
            Tuple(a, b, rest) => {
                go(a, out);
                go(b, out);
                rest.iter().for_each(|p| go(p, out))
            }
            Record(fields) => fields.iter().for_each(|f| out.push(f.value.to_string())),
            Ctor(_, _, _, args) => args.iter().for_each(|p| go(p, out)),
            List(items) => items.iter().for_each(|p| go(p, out)),
            Cons(h, t) => {
                go(h, out);
                go(t, out)
            }
            Anything | Unit | Chr(_) | Str(_) | Int(_) => {}
        }
    }
    go(pattern, &mut names);
    names
}
