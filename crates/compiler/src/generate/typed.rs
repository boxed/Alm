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
use inkwell::module::{Linkage, Module};
use inkwell::types::{BasicMetadataTypeEnum, BasicType, BasicTypeEnum};
use inkwell::values::{BasicValue, BasicValueEnum, FunctionValue};
use inkwell::{FloatPredicate, IntPredicate};

use super::native::{self, Target};
use crate::ir::layout::{Layout, LayoutCtx};
use crate::ir::mono::{MonoProgram, TypedExpr, TypedFn, TypedKind};

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
    locals: HashMap<String, BasicValueEnum<'ctx>>,
    cur_fn: Option<FunctionValue<'ctx>>,
    blk: usize,
}

impl<'ctx, 'l> TypedCodegen<'ctx, 'l> {
    fn new(ctx: &'ctx Context, layouts: &'l LayoutCtx) -> Self {
        TypedCodegen {
            ctx,
            module: ctx.create_module("alm_typed"),
            builder: ctx.create_builder(),
            layouts,
            functions: HashMap::new(),
            locals: HashMap::new(),
            cur_fn: None,
            blk: 0,
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
        for name in ["rtb$String$fromInt", "rtb$String$fromFloat"] {
            self.module.add_function(
                name,
                i64_t.fn_type(&[i64_t.into()], false),
                Some(Linkage::External),
            );
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
            _ => self.ctx.i64_type().into(),
        }
    }

    /// A cons cell: `{ element, next-pointer }`.
    fn cons_cell(&self, elem: &Layout) -> inkwell::types::StructType<'ctx> {
        let ptr_t = self.ctx.ptr_type(inkwell::AddressSpace::default());
        self.ctx
            .struct_type(&[self.llvm_type(elem), ptr_t.into()], false)
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
        for (i, (pattern, _)) in f.params.iter().enumerate() {
            if let Some(name) = simple_param_name(pattern) {
                self.locals
                    .insert(name, fv.get_nth_param(i as u32).unwrap());
            } else {
                return Err(format!(
                    "typed backend: unsupported parameter pattern in `{}`",
                    f.original
                ));
            }
        }
        let entry = self.ctx.append_basic_block(fv, "entry");
        self.builder.position_at_end(entry);
        let value = self.gen(&f.body)?;
        self.builder.build_return(Some(&value)).unwrap();
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

        let main = mono.functions.iter().find(|f| f.original.as_str() == "main");
        let Some(main) = main else {
            self.builder.build_return(Some(&i64_t.const_zero())).unwrap();
            return Ok(());
        };
        let main_fn = self.functions[&main.mangled.to_string()];
        let raw = self
            .builder
            .build_call(main_fn, &[], "main")
            .unwrap()
            .try_as_basic_value()
            .left()
            .unwrap();

        let boxed = match self.layouts.layout_of(&main.body.tipe) {
            Layout::Int => self.call_box("rt_int", raw),
            Layout::Float => self.call_box("rt_float", raw),
            // A string is already a uniform word; hand it straight to print.
            Layout::Str => raw,
            other => {
                return Err(format!(
                    "typed backend: main has layout {:?}, which the skeleton cannot box yet",
                    other
                ))
            }
        };
        self.builder.build_return(Some(&boxed)).unwrap();
        Ok(())
    }

    fn call_box(&self, name: &str, arg: BasicValueEnum<'ctx>) -> BasicValueEnum<'ctx> {
        self.call_named(name, &[arg])
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
        match &expr.kind {
            TypedKind::Int(n) => Ok(self
                .ctx
                .i64_type()
                .const_int(*n as u64, true)
                .into()),
            TypedKind::Float(f) => Ok(self.ctx.f64_type().const_float(*f).into()),
            TypedKind::Str(s) => self.gen_string(s),
            TypedKind::Unit => Ok(self.ctx.i64_type().const_zero().into()),
            TypedKind::Local(name) => self
                .locals
                .get(name.as_str())
                .copied()
                .ok_or_else(|| format!("typed backend: unbound local `{}`", name)),
            TypedKind::Global(name) => {
                // A zero-argument value: call it.
                let f = *self
                    .functions
                    .get(&name.to_string())
                    .ok_or_else(|| format!("typed backend: unknown global `{}`", name))?;
                Ok(self
                    .builder
                    .build_call(f, &[], "v")
                    .unwrap()
                    .try_as_basic_value()
                    .left()
                    .unwrap())
            }
            TypedKind::Call(func, args) => self.gen_call(expr, func, args),
            TypedKind::List(items) => self.gen_list(expr, items),
            TypedKind::Binop(op, _, _, l, r)
                if op.as_str() == "|>" || op.as_str() == "<|" =>
            {
                self.gen_pipe(expr, op.as_str(), l, r)
            }
            TypedKind::Binop(op, _, _, l, r) if op.as_str() == "::" => self.gen_cons(expr, l, r),
            TypedKind::Binop(op, _, _, l, r) if op.as_str() == "++" => self.gen_append(l, r),
            TypedKind::Binop(op, _, _, l, r) if op.as_str() == "&&" || op.as_str() == "||" => {
                self.gen_and_or(op.as_str(), l, r)
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
            TypedKind::Ctor(_, _, ctor) => self.gen_ctor(expr, ctor),
            other => Err(format!(
                "typed backend: unsupported expression {:?}",
                std::mem::discriminant(other)
            )),
        }
    }

    fn gen_call(
        &mut self,
        whole: &TypedExpr,
        func: &TypedExpr,
        args: &[TypedExpr],
    ) -> Result<BasicValueEnum<'ctx>, String> {
        // A saturated constructor application builds a tagged heap value.
        if let TypedKind::Ctor(_, _, ctor) = &func.kind {
            return self.gen_ctor_apply(whole, ctor, args);
        }
        // A built-in call becomes a generated, type-specialized kernel.
        if let TypedKind::Foreign(module, name) = &func.kind {
            return self.gen_kernel(whole, module.as_str(), name.as_str(), args);
        }
        let TypedKind::Global(name) = &func.kind else {
            return Err("typed backend: only direct calls are supported yet".to_string());
        };
        let f = *self
            .functions
            .get(&name.to_string())
            .ok_or_else(|| format!("typed backend: unknown call target `{}`", name))?;
        let mut argv = Vec::with_capacity(args.len());
        for arg in args {
            argv.push(self.gen(arg)?.into());
        }
        Ok(self
            .builder
            .build_call(f, &argv, "call")
            .unwrap()
            .try_as_basic_value()
            .left()
            .unwrap())
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
        let struct_ty = self.ctor_struct(&field_layouts);
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

        // Store the tag, then each field.
        let tag_ptr = self.builder.build_struct_gep(struct_ty, raw, 0, "tagp").unwrap();
        self.builder
            .build_store(tag_ptr, self.ctx.i32_type().const_int(ctor.index as u64, false))
            .unwrap();
        for (i, arg) in args.iter().enumerate() {
            let v = self.gen(arg)?;
            let fp = self
                .builder
                .build_struct_gep(struct_ty, raw, (i + 1) as u32, "fp")
                .unwrap();
            self.builder.build_store(fp, v).unwrap();
        }
        Ok(raw.into())
    }

    fn gen_let(
        &mut self,
        decls: &[crate::ir::mono::TypedLetDecl],
        body: &TypedExpr,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        use crate::ir::mono::TypedLetDecl::*;
        for decl in decls {
            match decl {
                Def { name, params, body } if params.is_empty() => {
                    let v = self.gen(body)?;
                    self.locals.insert(name.to_string(), v);
                }
                Destruct(pattern, value) => {
                    let v = self.gen(value)?;
                    let layout = self.layouts.layout_of(&value.tipe);
                    self.bind_pattern(pattern, v, &layout)?;
                }
                _ => {
                    return Err(
                        "typed backend: local function definitions are not supported yet"
                            .to_string(),
                    )
                }
            }
        }
        self.gen(body)
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
            agg = self
                .builder
                .build_insert_value(agg, v, i as u32, "tup")
                .unwrap()
                .into_struct_value();
        }
        Ok(agg.into())
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
            _ => Err("typed backend: unsupported destructuring pattern".to_string()),
        }
    }

    /// Construct a nullary constructor value: a `Bool` bit or an enum tag.
    /// (Data-carrying constructors are handled where they are applied.)
    fn gen_ctor(
        &mut self,
        whole: &TypedExpr,
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
            other => Err(format!(
                "typed backend: constructing `{}` (layout {:?}) is not supported yet",
                ctor.name, other
            )),
        }
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
            ("List", "sum") => self.kernel_list_sum(args),
            ("List", "length") => self.kernel_list_length(args),
            ("List", "range") => self.kernel_list_range(whole, args),
            ("List", "foldl") => self.kernel_list_foldl(whole, args),
            ("List", "foldr") => self.kernel_list_foldr(whole, args),
            ("List", "map") => self.kernel_list_map(whole, args),
            ("List", "reverse") => self.kernel_list_reverse(whole, args),
            ("List", "filter") => self.kernel_list_filter(whole, args),
            ("List", "isEmpty") => {
                let list = self.gen(&args[0])?.into_pointer_value();
                Ok(self.builder.build_is_null(list, "isempty").unwrap().into())
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
            _ => Err(format!(
                "typed backend: built-in `{}.{}` has no generated kernel yet",
                module, name
            )),
        }
    }

    fn call_fn(
        &self,
        f: FunctionValue<'ctx>,
        args: &[BasicValueEnum<'ctx>],
    ) -> BasicValueEnum<'ctx> {
        let argv: Vec<_> = args.iter().map(|a| (*a).into()).collect();
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
                Ok(self.call_fn(f, args))
            }
            TypedKind::Lambda(params, body) => {
                if params.len() != args.len() {
                    return Err(
                        "typed backend: partially-applied lambda in a kernel is not supported"
                            .to_string(),
                    );
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
            _ => Err(
                "typed backend: higher-order kernels need a named function or lambda argument"
                    .to_string(),
            ),
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
        self.emit_foldl(list, &elem, init, &args[0], acc_ty)
    }

    /// `List.foldr f init xs` = `foldl f init (reverse xs)` — same element
    /// function, list walked right-to-left.
    fn kernel_list_foldr(
        &mut self,
        whole: &TypedExpr,
        args: &[TypedExpr],
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let elem = self.elem_layout(&args[2].tipe)?;
        let list = self.gen(&args[2])?.into_pointer_value();
        let reversed = self.emit_reverse(list, &elem);
        let init = self.gen(&args[1])?;
        let acc_ty = self.llvm_type(&self.layouts.layout_of(&whole.tipe));
        self.emit_foldl(reversed, &elem, init, &args[0], acc_ty)
    }

    /// Emit a left-fold loop over a cons list, calling `f` per element.
    fn emit_foldl(
        &mut self,
        list: inkwell::values::PointerValue<'ctx>,
        elem: &Layout,
        init: BasicValueEnum<'ctx>,
        f: &TypedExpr,
        acc_ty: BasicTypeEnum<'ctx>,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let elem_ty = self.llvm_type(elem);
        let cell = self.cons_cell(elem);
        let ptr_t = self.ctx.ptr_type(inkwell::AddressSpace::default());
        let entry = self.builder.get_insert_block().unwrap();
        let loop_bb = self.new_block("foldl.loop");
        let body_bb = self.new_block("foldl.body");
        let done_bb = self.new_block("foldl.done");

        self.builder.build_unconditional_branch(loop_bb).unwrap();
        self.builder.position_at_end(loop_bb);
        let cur = self.builder.build_phi(ptr_t, "cur").unwrap();
        let acc = self.builder.build_phi(acc_ty, "acc").unwrap();
        cur.add_incoming(&[(&list as &dyn BasicValue, entry)]);
        acc.add_incoming(&[(&init as &dyn BasicValue, entry)]);
        let cur_ptr = cur.as_basic_value().into_pointer_value();
        let is_null = self.builder.build_is_null(cur_ptr, "end").unwrap();
        self.builder
            .build_conditional_branch(is_null, done_bb, body_bb)
            .unwrap();

        self.builder.position_at_end(body_bb);
        let hp = self.builder.build_struct_gep(cell, cur_ptr, 0, "hp").unwrap();
        let head = self.builder.build_load(elem_ty, hp, "head").unwrap();
        let new_acc = self.apply_fn_expr(f, &[head, acc.as_basic_value()])?;
        let tp = self.builder.build_struct_gep(cell, cur_ptr, 1, "tp").unwrap();
        let next = self.builder.build_load(ptr_t, tp, "next").unwrap();
        let body_end = self.builder.get_insert_block().unwrap();
        self.builder.build_unconditional_branch(loop_bb).unwrap();
        cur.add_incoming(&[(&next as &dyn BasicValue, body_end)]);
        acc.add_incoming(&[(&new_acc as &dyn BasicValue, body_end)]);

        self.builder.position_at_end(done_bb);
        Ok(acc.as_basic_value())
    }

    /// `List.map : (a -> b) -> List a -> List b` — map, building the result
    /// in order via a moving tail slot, calling the specialized function.
    fn kernel_list_map(
        &mut self,
        whole: &TypedExpr,
        args: &[TypedExpr],
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let src_elem = self.elem_layout(&args[1].tipe)?;
        let dst_elem = self.elem_layout(&whole.tipe)?;
        let list = self.gen(&args[1])?.into_pointer_value();
        let src_cell = self.cons_cell(&src_elem);
        let dst_cell = self.cons_cell(&dst_elem);
        let src_elem_ty = self.llvm_type(&src_elem);
        let ptr_t = self.ctx.ptr_type(inkwell::AddressSpace::default());

        // `result` holds the head; `slot` holds the address of the next-field
        // to write into (initially the address of `result`).
        let result = self.entry_alloca(ptr_t.into(), "result");
        self.builder.build_store(result, ptr_t.const_null()).unwrap();
        let slot = self.entry_alloca(ptr_t.into(), "slot");
        self.builder.build_store(slot, result).unwrap();

        let entry = self.builder.get_insert_block().unwrap();
        let loop_bb = self.new_block("map.loop");
        let body_bb = self.new_block("map.body");
        let done_bb = self.new_block("map.done");

        self.builder.build_unconditional_branch(loop_bb).unwrap();
        self.builder.position_at_end(loop_bb);
        let cur = self.builder.build_phi(ptr_t, "cur").unwrap();
        cur.add_incoming(&[(&list as &dyn BasicValue, entry)]);
        let cur_ptr = cur.as_basic_value().into_pointer_value();
        let is_null = self.builder.build_is_null(cur_ptr, "end").unwrap();
        self.builder
            .build_conditional_branch(is_null, done_bb, body_bb)
            .unwrap();

        self.builder.position_at_end(body_bb);
        let hp = self.builder.build_struct_gep(src_cell, cur_ptr, 0, "hp").unwrap();
        let head = self.builder.build_load(src_elem_ty, hp, "head").unwrap();
        let mapped = self.apply_fn_expr(&args[0], &[head])?;
        let newcell = self.cons(&dst_elem, mapped, ptr_t.const_null());
        // *slot = newcell
        let dest = self.builder.build_load(ptr_t, slot, "dest").unwrap().into_pointer_value();
        self.builder.build_store(dest, newcell).unwrap();
        // slot = &newcell.next
        let nextfield = self.builder.build_struct_gep(dst_cell, newcell, 1, "nf").unwrap();
        self.builder.build_store(slot, nextfield).unwrap();
        let tp = self.builder.build_struct_gep(src_cell, cur_ptr, 1, "tp").unwrap();
        let next = self.builder.build_load(ptr_t, tp, "next").unwrap();
        let body_end = self.builder.get_insert_block().unwrap();
        self.builder.build_unconditional_branch(loop_bb).unwrap();
        cur.add_incoming(&[(&next as &dyn BasicValue, body_end)]);

        self.builder.position_at_end(done_bb);
        Ok(self.builder.build_load(ptr_t, result, "mapped").unwrap())
    }

    /// Truncate an f64 toward zero to an i64.
    fn f_to_int(&self, x: inkwell::values::FloatValue<'ctx>) -> BasicValueEnum<'ctx> {
        self.builder
            .build_float_to_signed_int(x, self.ctx.i64_type(), "toint")
            .unwrap()
            .into()
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

    /// `min`/`max` on Int or Float — a comparison and select.
    fn kernel_min_max(
        &mut self,
        args: &[TypedExpr],
        is_min: bool,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let a = self.gen(&args[0])?;
        let b = self.gen(&args[1])?;
        let cond = if matches!(self.layouts.layout_of(&args[0].tipe), Layout::Float) {
            let pred = if is_min { FloatPredicate::OLT } else { FloatPredicate::OGT };
            self.builder
                .build_float_compare(pred, a.into_float_value(), b.into_float_value(), "mm")
                .unwrap()
        } else {
            let pred = if is_min { IntPredicate::SLT } else { IntPredicate::SGT };
            self.builder
                .build_int_compare(pred, a.into_int_value(), b.into_int_value(), "mm")
                .unwrap()
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

    /// `List.reverse : List a -> List a` — cons each element onto an
    /// accumulator, which reverses.
    fn kernel_list_reverse(
        &mut self,
        whole: &TypedExpr,
        args: &[TypedExpr],
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let elem = self.elem_layout(&whole.tipe)?;
        let list = self.gen(&args[0])?.into_pointer_value();
        Ok(self.emit_reverse(list, &elem).into())
    }

    /// Emit a loop that reverses a cons list, returning the new head pointer.
    fn emit_reverse(
        &mut self,
        list: inkwell::values::PointerValue<'ctx>,
        elem: &Layout,
    ) -> inkwell::values::PointerValue<'ctx> {
        let cell = self.cons_cell(elem);
        let elem_ty = self.llvm_type(elem);
        let ptr_t = self.ctx.ptr_type(inkwell::AddressSpace::default());
        let entry = self.builder.get_insert_block().unwrap();
        let loop_bb = self.new_block("rev.loop");
        let body_bb = self.new_block("rev.body");
        let done_bb = self.new_block("rev.done");

        self.builder.build_unconditional_branch(loop_bb).unwrap();
        self.builder.position_at_end(loop_bb);
        let cur = self.builder.build_phi(ptr_t, "cur").unwrap();
        let acc = self.builder.build_phi(ptr_t, "acc").unwrap();
        let null = ptr_t.const_null();
        cur.add_incoming(&[(&list as &dyn BasicValue, entry)]);
        acc.add_incoming(&[(&null as &dyn BasicValue, entry)]);
        let cur_ptr = cur.as_basic_value().into_pointer_value();
        let is_null = self.builder.build_is_null(cur_ptr, "end").unwrap();
        self.builder
            .build_conditional_branch(is_null, done_bb, body_bb)
            .unwrap();

        self.builder.position_at_end(body_bb);
        let hp = self.builder.build_struct_gep(cell, cur_ptr, 0, "hp").unwrap();
        let head = self.builder.build_load(elem_ty, hp, "head").unwrap();
        let new_acc: BasicValueEnum = self
            .cons(elem, head, acc.as_basic_value().into_pointer_value())
            .into();
        let tp = self.builder.build_struct_gep(cell, cur_ptr, 1, "tp").unwrap();
        let next = self.builder.build_load(ptr_t, tp, "next").unwrap();
        let body_end = self.builder.get_insert_block().unwrap();
        self.builder.build_unconditional_branch(loop_bb).unwrap();
        cur.add_incoming(&[(&next as &dyn BasicValue, body_end)]);
        acc.add_incoming(&[(&new_acc as &dyn BasicValue, body_end)]);

        self.builder.position_at_end(done_bb);
        acc.as_basic_value().into_pointer_value()
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
        let cell = self.cons_cell(&elem);
        let elem_ty = self.llvm_type(&elem);
        let ptr_t = self.ctx.ptr_type(inkwell::AddressSpace::default());

        let result = self.entry_alloca(ptr_t.into(), "result");
        self.builder.build_store(result, ptr_t.const_null()).unwrap();
        let slot = self.entry_alloca(ptr_t.into(), "slot");
        self.builder.build_store(slot, result).unwrap();

        let entry = self.builder.get_insert_block().unwrap();
        let loop_bb = self.new_block("filt.loop");
        let body_bb = self.new_block("filt.body");
        let keep_bb = self.new_block("filt.keep");
        let cont_bb = self.new_block("filt.cont");
        let done_bb = self.new_block("filt.done");

        self.builder.build_unconditional_branch(loop_bb).unwrap();
        self.builder.position_at_end(loop_bb);
        let cur = self.builder.build_phi(ptr_t, "cur").unwrap();
        cur.add_incoming(&[(&list as &dyn BasicValue, entry)]);
        let cur_ptr = cur.as_basic_value().into_pointer_value();
        let is_null = self.builder.build_is_null(cur_ptr, "end").unwrap();
        self.builder
            .build_conditional_branch(is_null, done_bb, body_bb)
            .unwrap();

        self.builder.position_at_end(body_bb);
        let hp = self.builder.build_struct_gep(cell, cur_ptr, 0, "hp").unwrap();
        let head = self.builder.build_load(elem_ty, hp, "head").unwrap();
        let keep = self.apply_fn_expr(&args[0], &[head])?.into_int_value();
        self.builder
            .build_conditional_branch(keep, keep_bb, cont_bb)
            .unwrap();

        self.builder.position_at_end(keep_bb);
        let newcell = self.cons(&elem, head, ptr_t.const_null());
        let dest = self.builder.build_load(ptr_t, slot, "dest").unwrap().into_pointer_value();
        self.builder.build_store(dest, newcell).unwrap();
        let nextfield = self.builder.build_struct_gep(cell, newcell, 1, "nf").unwrap();
        self.builder.build_store(slot, nextfield).unwrap();
        self.builder.build_unconditional_branch(cont_bb).unwrap();

        self.builder.position_at_end(cont_bb);
        let tp = self.builder.build_struct_gep(cell, cur_ptr, 1, "tp").unwrap();
        let next = self.builder.build_load(ptr_t, tp, "next").unwrap();
        let cont_end = self.builder.get_insert_block().unwrap();
        self.builder.build_unconditional_branch(loop_bb).unwrap();
        cur.add_incoming(&[(&next as &dyn BasicValue, cont_end)]);

        self.builder.position_at_end(done_bb);
        Ok(self.builder.build_load(ptr_t, result, "filtered").unwrap())
    }

    /// `List.sum : List number -> number` — walk the cons list accumulating.
    fn kernel_list_sum(&mut self, args: &[TypedExpr]) -> Result<BasicValueEnum<'ctx>, String> {
        let elem = self.elem_layout(&args[0].tipe)?;
        let is_float = matches!(elem, Layout::Float);
        let list = self.gen(&args[0])?.into_pointer_value();
        let cell = self.cons_cell(&elem);
        let acc_ty = self.llvm_type(&elem);
        let entry = self.builder.get_insert_block().unwrap();
        let loop_bb = self.new_block("sum.loop");
        let body_bb = self.new_block("sum.body");
        let done_bb = self.new_block("sum.done");

        self.builder.build_unconditional_branch(loop_bb).unwrap();
        self.builder.position_at_end(loop_bb);
        let ptr_t = self.ctx.ptr_type(inkwell::AddressSpace::default());
        let cur = self.builder.build_phi(ptr_t, "cur").unwrap();
        let acc = self.builder.build_phi(acc_ty, "acc").unwrap();
        let zero: BasicValueEnum = if is_float {
            self.ctx.f64_type().const_zero().into()
        } else {
            self.ctx.i64_type().const_zero().into()
        };
        cur.add_incoming(&[(&list as &dyn BasicValue, entry)]);
        acc.add_incoming(&[(&zero as &dyn BasicValue, entry)]);
        let cur_ptr = cur.as_basic_value().into_pointer_value();
        let is_null = self.builder.build_is_null(cur_ptr, "end").unwrap();
        self.builder
            .build_conditional_branch(is_null, done_bb, body_bb)
            .unwrap();

        self.builder.position_at_end(body_bb);
        let hp = self.builder.build_struct_gep(cell, cur_ptr, 0, "hp").unwrap();
        let head = self.builder.build_load(acc_ty, hp, "head").unwrap();
        let new_acc: BasicValueEnum = if is_float {
            self.builder
                .build_float_add(
                    acc.as_basic_value().into_float_value(),
                    head.into_float_value(),
                    "acc2",
                )
                .unwrap()
                .into()
        } else {
            self.builder
                .build_int_add(
                    acc.as_basic_value().into_int_value(),
                    head.into_int_value(),
                    "acc2",
                )
                .unwrap()
                .into()
        };
        let tp = self.builder.build_struct_gep(cell, cur_ptr, 1, "tp").unwrap();
        let next = self.builder.build_load(ptr_t, tp, "next").unwrap();
        let body_end = self.builder.get_insert_block().unwrap();
        self.builder.build_unconditional_branch(loop_bb).unwrap();
        cur.add_incoming(&[(&next as &dyn BasicValue, body_end)]);
        acc.add_incoming(&[(&new_acc as &dyn BasicValue, body_end)]);

        self.builder.position_at_end(done_bb);
        Ok(acc.as_basic_value())
    }

    /// `List.length : List a -> Int` — walk counting elements.
    fn kernel_list_length(&mut self, args: &[TypedExpr]) -> Result<BasicValueEnum<'ctx>, String> {
        let elem = self.elem_layout(&args[0].tipe)?;
        let list = self.gen(&args[0])?.into_pointer_value();
        let cell = self.cons_cell(&elem);
        let i64_t = self.ctx.i64_type();
        let ptr_t = self.ctx.ptr_type(inkwell::AddressSpace::default());
        let entry = self.builder.get_insert_block().unwrap();
        let loop_bb = self.new_block("len.loop");
        let body_bb = self.new_block("len.body");
        let done_bb = self.new_block("len.done");

        self.builder.build_unconditional_branch(loop_bb).unwrap();
        self.builder.position_at_end(loop_bb);
        let cur = self.builder.build_phi(ptr_t, "cur").unwrap();
        let acc = self.builder.build_phi(i64_t, "n").unwrap();
        let zero: BasicValueEnum = i64_t.const_zero().into();
        cur.add_incoming(&[(&list as &dyn BasicValue, entry)]);
        acc.add_incoming(&[(&zero as &dyn BasicValue, entry)]);
        let cur_ptr = cur.as_basic_value().into_pointer_value();
        let is_null = self.builder.build_is_null(cur_ptr, "end").unwrap();
        self.builder
            .build_conditional_branch(is_null, done_bb, body_bb)
            .unwrap();

        self.builder.position_at_end(body_bb);
        let new_acc: BasicValueEnum = self
            .builder
            .build_int_add(acc.as_basic_value().into_int_value(), i64_t.const_int(1, false), "n2")
            .unwrap()
            .into();
        let tp = self.builder.build_struct_gep(cell, cur_ptr, 1, "tp").unwrap();
        let next = self.builder.build_load(ptr_t, tp, "next").unwrap();
        let body_end = self.builder.get_insert_block().unwrap();
        self.builder.build_unconditional_branch(loop_bb).unwrap();
        cur.add_incoming(&[(&next as &dyn BasicValue, body_end)]);
        acc.add_incoming(&[(&new_acc as &dyn BasicValue, body_end)]);

        self.builder.position_at_end(done_bb);
        Ok(acc.as_basic_value())
    }

    /// `List.range : Int -> Int -> List Int` — build [lo..hi] ascending by
    /// consing from hi down to lo.
    fn kernel_list_range(
        &mut self,
        whole: &TypedExpr,
        args: &[TypedExpr],
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let elem = self.elem_layout(&whole.tipe)?;
        let lo = self.gen(&args[0])?.into_int_value();
        let hi = self.gen(&args[1])?.into_int_value();
        let i64_t = self.ctx.i64_type();
        let ptr_t = self.ctx.ptr_type(inkwell::AddressSpace::default());
        let entry = self.builder.get_insert_block().unwrap();
        let loop_bb = self.new_block("range.loop");
        let body_bb = self.new_block("range.body");
        let done_bb = self.new_block("range.done");

        self.builder.build_unconditional_branch(loop_bb).unwrap();
        self.builder.position_at_end(loop_bb);
        let i = self.builder.build_phi(i64_t, "i").unwrap();
        let acc = self.builder.build_phi(ptr_t, "acc").unwrap();
        let null = ptr_t.const_null();
        i.add_incoming(&[(&hi as &dyn BasicValue, entry)]);
        acc.add_incoming(&[(&null as &dyn BasicValue, entry)]);
        let iv = i.as_basic_value().into_int_value();
        let cond = self
            .builder
            .build_int_compare(IntPredicate::SGE, iv, lo, "cmp")
            .unwrap();
        self.builder
            .build_conditional_branch(cond, body_bb, done_bb)
            .unwrap();

        self.builder.position_at_end(body_bb);
        let acc_ptr = acc.as_basic_value().into_pointer_value();
        let cell = self.cons(&elem, iv.into(), acc_ptr);
        let i2: BasicValueEnum = self
            .builder
            .build_int_sub(iv, i64_t.const_int(1, false), "i2")
            .unwrap()
            .into();
        let cell_val: BasicValueEnum = cell.into();
        let body_end = self.builder.get_insert_block().unwrap();
        self.builder.build_unconditional_branch(loop_bb).unwrap();
        i.add_incoming(&[(&i2 as &dyn BasicValue, body_end)]);
        acc.add_incoming(&[(&cell_val as &dyn BasicValue, body_end)]);

        self.builder.position_at_end(done_bb);
        Ok(acc.as_basic_value())
    }

    /// Allocate a cons cell `{elem, tail}` and return the pointer.
    fn cons(
        &mut self,
        elem: &Layout,
        head: BasicValueEnum<'ctx>,
        tail: inkwell::values::PointerValue<'ctx>,
    ) -> inkwell::values::PointerValue<'ctx> {
        let cell = self.cons_cell(elem);
        let alloc = self.module.get_function("alm_alloc").unwrap();
        let raw = self
            .builder
            .build_call(alloc, &[cell.size_of().unwrap().into()], "cons")
            .unwrap()
            .try_as_basic_value()
            .left()
            .unwrap()
            .into_pointer_value();
        let hp = self.builder.build_struct_gep(cell, raw, 0, "hp").unwrap();
        self.builder.build_store(hp, head).unwrap();
        let tp = self.builder.build_struct_gep(cell, raw, 1, "tp").unwrap();
        self.builder.build_store(tp, tail).unwrap();
        raw
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

    /// `++` on strings: the operands are uniform string words, appended by the
    /// runtime. (List append is not handled by this path.)
    fn gen_append(
        &mut self,
        l: &TypedExpr,
        r: &TypedExpr,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        if !matches!(self.layouts.layout_of(&l.tipe), Layout::Str) {
            return Err("typed backend: ++ is only supported on strings yet".to_string());
        }
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
        let ptr_t = self.ctx.ptr_type(inkwell::AddressSpace::default());
        // Build right-to-left so the head ends up outermost.
        let mut values = Vec::with_capacity(items.len());
        for item in items {
            values.push(self.gen(item)?);
        }
        let mut acc = ptr_t.const_null();
        for v in values.into_iter().rev() {
            acc = self.cons(&elem, v, acc);
        }
        Ok(acc.into())
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

    fn gen_binop(
        &mut self,
        op: &str,
        l: &TypedExpr,
        r: &TypedExpr,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let is_float = matches!(self.layouts.layout_of(&l.tipe), Layout::Float);
        let lv = self.gen(l)?;
        let rv = self.gen(r)?;
        let b = &self.builder;
        if is_float {
            let (x, y) = (lv.into_float_value(), rv.into_float_value());
            let v: BasicValueEnum = match op {
                "+" => b.build_float_add(x, y, "f").unwrap().into(),
                "-" => b.build_float_sub(x, y, "f").unwrap().into(),
                "*" => b.build_float_mul(x, y, "f").unwrap().into(),
                "/" => b.build_float_div(x, y, "f").unwrap().into(),
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
            let (x, y) = (lv.into_int_value(), rv.into_int_value());
            let v: BasicValueEnum = match op {
                "+" => b.build_int_add(x, y, "i").unwrap().into(),
                "-" => b.build_int_sub(x, y, "i").unwrap().into(),
                "*" => b.build_int_mul(x, y, "i").unwrap().into(),
                "//" => b.build_int_signed_div(x, y, "i").unwrap().into(),
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
            let tv = self.gen(then)?;
            incoming.push((tv, self.builder.get_insert_block().unwrap()));
            self.builder.build_unconditional_branch(merge).unwrap();
            self.builder.position_at_end(else_bb);
        }
        let ev = self.gen(otherwise)?;
        incoming.push((ev, self.builder.get_insert_block().unwrap()));
        self.builder.build_unconditional_branch(merge).unwrap();

        self.builder.position_at_end(merge);
        let phi = self.builder.build_phi(result_ty, "if").unwrap();
        for (val, bb) in &incoming {
            phi.add_incoming(&[(val as &dyn BasicValue, *bb)]);
        }
        Ok(phi.as_basic_value())
    }

    /// `case` on a scalar scrutinee (Int/Char), with literal patterns and a
    /// variable/wildcard catch-all. Compiles to a test chain feeding a phi.
    fn gen_case(
        &mut self,
        scrutinee: &TypedExpr,
        branches: &[(crate::ast::canonical::Pattern, TypedExpr)],
        whole: &TypedExpr,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        let subject = self.gen(scrutinee)?;
        let subject_layout = self.layouts.layout_of(&scrutinee.tipe);
        let result_ty = self.llvm_type(&self.layouts.layout_of(&whole.tipe));
        let merge = self.new_block("case.end");
        let mut incoming: Vec<(BasicValueEnum<'ctx>, inkwell::basic_block::BasicBlock<'ctx>)> =
            Vec::new();
        let mut matched_all = false;

        for (pattern, body) in branches {
            match self.pattern_test(pattern, subject, &subject_layout)? {
                None => {
                    // Irrefutable: always matches; bind and stop.
                    self.bind_case_pattern(pattern, subject, &subject_layout)?;
                    let v = self.gen(body)?;
                    incoming.push((v, self.builder.get_insert_block().unwrap()));
                    self.builder.build_unconditional_branch(merge).unwrap();
                    matched_all = true;
                    break;
                }
                Some(cond) => {
                    let then_bb = self.new_block("case.then");
                    let else_bb = self.new_block("case.else");
                    self.builder
                        .build_conditional_branch(cond, then_bb, else_bb)
                        .unwrap();
                    self.builder.position_at_end(then_bb);
                    self.bind_case_pattern(pattern, subject, &subject_layout)?;
                    let v = self.gen(body)?;
                    incoming.push((v, self.builder.get_insert_block().unwrap()));
                    self.builder.build_unconditional_branch(merge).unwrap();
                    self.builder.position_at_end(else_bb);
                }
            }
        }

        // Elm case-expressions are exhaustive; if the source had no explicit
        // catch-all the final else is unreachable.
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

    /// The condition under which a pattern matches `subject`, or `None` if the
    /// pattern is irrefutable (always matches).
    fn pattern_test(
        &mut self,
        pattern: &crate::ast::canonical::Pattern,
        subject: BasicValueEnum<'ctx>,
        layout: &Layout,
    ) -> Result<Option<inkwell::values::IntValue<'ctx>>, String> {
        use crate::ast::canonical::Pattern_::*;
        let eq_i = |b: &inkwell::builder::Builder<'ctx>, x, y| {
            b.build_int_compare(IntPredicate::EQ, x, y, "casetest").unwrap()
        };
        Ok(match &pattern.value {
            Var(_) | Anything => None,
            Alias(inner, _) => self.pattern_test(inner, subject, layout)?,
            Int(n) => Some(eq_i(
                &self.builder,
                subject.into_int_value(),
                self.ctx.i64_type().const_int(*n as u64, true),
            )),
            Chr(c) => Some(eq_i(
                &self.builder,
                subject.into_int_value(),
                self.ctx.i32_type().const_int(*c as u64, false),
            )),
            // Empty-list pattern: the pointer is null.
            List(elems) if elems.is_empty() => Some(
                self.builder
                    .build_is_null(subject.into_pointer_value(), "isnil")
                    .unwrap(),
            ),
            // Cons pattern: the pointer is non-null.
            Cons(_, _) => Some(
                self.builder
                    .build_is_not_null(subject.into_pointer_value(), "iscons")
                    .unwrap(),
            ),
            Ctor(_, _, ctor, _) => match layout {
                Layout::Bool => Some(eq_i(
                    &self.builder,
                    subject.into_int_value(),
                    self.ctx
                        .bool_type()
                        .const_int((ctor.name.as_str() == "True") as u64, false),
                )),
                Layout::Enum(_) => Some(eq_i(
                    &self.builder,
                    subject.into_int_value(),
                    self.ctx.i32_type().const_int(ctor.index as u64, false),
                )),
                Layout::Tagged(_) => {
                    // Load the tag from the front of the heap block.
                    let tag = self
                        .builder
                        .build_load(self.ctx.i32_type(), subject.into_pointer_value(), "tag")
                        .unwrap()
                        .into_int_value();
                    Some(eq_i(
                        &self.builder,
                        tag,
                        self.ctx.i32_type().const_int(ctor.index as u64, false),
                    ))
                }
                other => {
                    return Err(format!(
                        "typed backend: case on layout {:?} is not supported yet",
                        other
                    ))
                }
            },
            _ => {
                return Err(
                    "typed backend: unsupported case pattern (nested refutable patterns \
                     are not compiled yet)"
                        .to_string(),
                )
            }
        })
    }

    /// Bind the variables a pattern introduces, given a successful match.
    /// Called while positioned in the matched branch.
    fn bind_case_pattern(
        &mut self,
        pattern: &crate::ast::canonical::Pattern,
        subject: BasicValueEnum<'ctx>,
        layout: &Layout,
    ) -> Result<(), String> {
        use crate::ast::canonical::Pattern_::*;
        match &pattern.value {
            Var(_) | Anything | Tuple(..) | Record(_) => {
                self.bind_pattern(pattern, subject, layout)
            }
            Alias(inner, name) => {
                self.locals.insert(name.value.to_string(), subject);
                self.bind_case_pattern(inner, subject, layout)
            }
            Int(_) | Chr(_) => Ok(()),
            List(elems) if elems.is_empty() => Ok(()),
            Cons(head, tail) => {
                let Layout::List(elem) = layout else {
                    return Err("typed backend: cons pattern on non-list value".to_string());
                };
                let cell = self.cons_cell(elem);
                let ptr = subject.into_pointer_value();
                let hp = self.builder.build_struct_gep(cell, ptr, 0, "hp").unwrap();
                let head_val = self
                    .builder
                    .build_load(self.llvm_type(elem), hp, "head")
                    .unwrap();
                self.bind_pattern(head, head_val, elem)?;
                let tp = self.builder.build_struct_gep(cell, ptr, 1, "tp").unwrap();
                let tail_val = self
                    .builder
                    .build_load(
                        self.ctx.ptr_type(inkwell::AddressSpace::default()),
                        tp,
                        "tail",
                    )
                    .unwrap();
                self.bind_pattern(tail, tail_val, layout)?;
                Ok(())
            }
            Ctor(_, _, ctor, args) => {
                if args.is_empty() {
                    return Ok(());
                }
                let Layout::Tagged(variants) = layout else {
                    return Err("typed backend: constructor pattern on non-tagged value".to_string());
                };
                let field_layouts = variants
                    .get(ctor.index as usize)
                    .cloned()
                    .ok_or_else(|| format!("typed backend: bad ctor index for `{}`", ctor.name))?;
                let struct_ty = self.ctor_struct(&field_layouts);
                let ptr = subject.into_pointer_value();
                for (i, argpat) in args.iter().enumerate() {
                    let fp = self
                        .builder
                        .build_struct_gep(struct_ty, ptr, (i + 1) as u32, "fp")
                        .unwrap();
                    let val = self
                        .builder
                        .build_load(self.llvm_type(&field_layouts[i]), fp, "fld")
                        .unwrap();
                    self.bind_pattern(argpat, val, &field_layouts[i])?;
                }
                Ok(())
            }
            _ => Err("typed backend: unsupported case pattern".to_string()),
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

/// The name a simple `Var` parameter binds, if that is all this pattern is.
fn simple_param_name(pattern: &crate::ast::canonical::Pattern) -> Option<String> {
    use crate::ast::canonical::Pattern_::*;
    match &pattern.value {
        Var(name) => Some(name.to_string()),
        Anything => Some("_".to_string()),
        _ => None,
    }
}
