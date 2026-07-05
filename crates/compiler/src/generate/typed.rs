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
use crate::ir::mono::{MonoProgram, TypedExpr, TypedFn, TypedKind, TypedLetDecl};

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
    /// Closure wrappers for named functions used as first-class values,
    /// keyed by the function's mangled name.
    wrappers: HashMap<String, FunctionValue<'ctx>>,
    locals: HashMap<String, BasicValueEnum<'ctx>>,
    cur_fn: Option<FunctionValue<'ctx>>,
    blk: usize,
    lam_id: usize,
}

impl<'ctx, 'l> TypedCodegen<'ctx, 'l> {
    fn new(ctx: &'ctx Context, layouts: &'l LayoutCtx) -> Self {
        TypedCodegen {
            ctx,
            module: ctx.create_module("alm_typed"),
            builder: ctx.create_builder(),
            layouts,
            functions: HashMap::new(),
            wrappers: HashMap::new(),
            locals: HashMap::new(),
            cur_fn: None,
            blk: 0,
            lam_id: 0,
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
                let f = *self
                    .functions
                    .get(&name.to_string())
                    .ok_or_else(|| format!("typed backend: unknown global `{}`", name))?;
                // A function used as a first-class value becomes a closure;
                // a zero-argument value is called to produce its value.
                if matches!(expr.tipe, crate::ast::canonical::Type::Lambda(..)) {
                    Ok(self.wrap_global(&name.to_string()).into())
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
            TypedKind::Ctor(_, _, ctor) => self.gen_ctor(expr, ctor),
            TypedKind::Lambda(params, body) => {
                let result_layout = self.layouts.layout_of(&body.tipe);
                self.gen_closure(params, body, &result_layout)
            }
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
        // A direct call to a named function, when the argument count matches
        // the function's arity.
        if let TypedKind::Global(name) = &func.kind {
            let f = *self
                .functions
                .get(&name.to_string())
                .ok_or_else(|| format!("typed backend: unknown call target `{}`", name))?;
            if f.count_params() as usize == args.len() {
                let mut argv = Vec::with_capacity(args.len());
                for arg in args {
                    argv.push(self.gen(arg)?.into());
                }
                return Ok(self
                    .builder
                    .build_call(f, &argv, "call")
                    .unwrap()
                    .try_as_basic_value()
                    .left()
                    .unwrap());
            }
            return Err(format!(
                "typed backend: partial application of `{}` ({} of {} args) is not \
                 supported yet",
                name,
                args.len(),
                f.count_params()
            ));
        }

        // Otherwise the callee is a closure value (a function-typed local,
        // the result of another call, a field, …): apply it indirectly.
        let closure = self.gen(func)?.into_pointer_value();
        let mut argv = Vec::with_capacity(args.len());
        for arg in args {
            argv.push(self.gen(arg)?);
        }
        let ret_layout = self.layouts.layout_of(&whole.tipe);
        Ok(self.apply_closure(closure, &argv, &ret_layout))
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
            ("List", "map2") => self.kernel_list_map2(whole, args),
            ("List", "indexedMap") => self.kernel_list_indexed_map(whole, args),
            ("List", "reverse") => self.kernel_list_reverse(whole, args),
            ("List", "filter") => self.kernel_list_filter(whole, args),
            ("List", "member") => self.kernel_list_member(args),
            ("List", "head") => self.kernel_list_head_tail(whole, args, true),
            ("List", "tail") => self.kernel_list_head_tail(whole, args, false),
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
            ("String", "length") => {
                let s = self.gen(&args[0])?;
                let boxed_len = self.call_named("rtb$String$length", &[s]);
                // rtb$String$length returns a uniform int word; unbox it.
                Ok(self.call_named("rt_unint", &[boxed_len]))
            }
            ("String", "join") => self.kernel_string_join(args),
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
            _ => {
                // A closure value (e.g. a function-typed parameter): peel the
                // function type by the number of arguments to get the result
                // layout, then apply indirectly.
                let closure = self.gen(func)?.into_pointer_value();
                let mut t = func.tipe.clone();
                for _ in 0..args.len() {
                    match t {
                        crate::ast::canonical::Type::Lambda(_, r) => t = *r,
                        _ => break,
                    }
                }
                let ret_layout = self.layouts.layout_of(&t);
                Ok(self.apply_closure(closure, args, &ret_layout))
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

    /// `String.join sep xs` — walk the (cons-cell) list of string words,
    /// appending each with the separator between elements.
    fn kernel_string_join(&mut self, args: &[TypedExpr]) -> Result<BasicValueEnum<'ctx>, String> {
        let sep = self.gen(&args[0])?;
        let list = self.gen(&args[1])?.into_pointer_value();
        let i64_t = self.ctx.i64_type();
        let i1_t = self.ctx.bool_type();
        let ptr_t = self.ctx.ptr_type(inkwell::AddressSpace::default());
        let cell = self.cons_cell(&Layout::Str);
        let rt_append = self.module.get_function("rt_append").unwrap();
        let empty = self.gen_string("")?;

        let entry = self.builder.get_insert_block().unwrap();
        let loop_bb = self.new_block("join.loop");
        let body_bb = self.new_block("join.body");
        let done_bb = self.new_block("join.done");

        self.builder.build_unconditional_branch(loop_bb).unwrap();
        self.builder.position_at_end(loop_bb);
        let cur = self.builder.build_phi(ptr_t, "cur").unwrap();
        let result = self.builder.build_phi(i64_t, "res").unwrap();
        let first = self.builder.build_phi(i1_t, "first").unwrap();
        cur.add_incoming(&[(&list as &dyn BasicValue, entry)]);
        result.add_incoming(&[(&empty as &dyn BasicValue, entry)]);
        let true_v: BasicValueEnum = i1_t.const_int(1, false).into();
        first.add_incoming(&[(&true_v as &dyn BasicValue, entry)]);
        let cur_ptr = cur.as_basic_value().into_pointer_value();
        let is_null = self.builder.build_is_null(cur_ptr, "end").unwrap();
        self.builder
            .build_conditional_branch(is_null, done_bb, body_bb)
            .unwrap();

        self.builder.position_at_end(body_bb);
        let hp = self.builder.build_struct_gep(cell, cur_ptr, 0, "hp").unwrap();
        let head = self.builder.build_load(i64_t, hp, "head").unwrap();
        let res_val = result.as_basic_value();
        let with_sep = self
            .builder
            .build_call(rt_append, &[res_val.into(), sep.into()], "ws")
            .unwrap()
            .try_as_basic_value()
            .left()
            .unwrap();
        let r1 = self
            .builder
            .build_select(first.as_basic_value().into_int_value(), res_val, with_sep, "r1")
            .unwrap();
        let r2 = self
            .builder
            .build_call(rt_append, &[r1.into(), head.into()], "r2")
            .unwrap()
            .try_as_basic_value()
            .left()
            .unwrap();
        let tp = self.builder.build_struct_gep(cell, cur_ptr, 1, "tp").unwrap();
        let next = self.builder.build_load(ptr_t, tp, "next").unwrap();
        let body_end = self.builder.get_insert_block().unwrap();
        self.builder.build_unconditional_branch(loop_bb).unwrap();
        cur.add_incoming(&[(&next as &dyn BasicValue, body_end)]);
        result.add_incoming(&[(&r2 as &dyn BasicValue, body_end)]);
        let false_v: BasicValueEnum = i1_t.const_int(0, false).into();
        first.add_incoming(&[(&false_v as &dyn BasicValue, body_end)]);

        self.builder.position_at_end(done_bb);
        Ok(result.as_basic_value())
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
        let cell = self.cons_cell(&elem);
        let ptr_t = self.ctx.ptr_type(inkwell::AddressSpace::default());
        let is_null = self.builder.build_is_null(list, "empty").unwrap();
        let just_bb = self.new_block("mb.just");
        let nothing_bb = self.new_block("mb.nothing");
        let merge = self.new_block("mb.end");
        self.builder.build_conditional_branch(is_null, nothing_bb, just_bb).unwrap();

        self.builder.position_at_end(just_bb);
        let field: BasicValueEnum = if is_head {
            let hp = self.builder.build_struct_gep(cell, list, 0, "hp").unwrap();
            self.builder.build_load(self.llvm_type(&elem), hp, "head").unwrap()
        } else {
            let tp = self.builder.build_struct_gep(cell, list, 1, "tp").unwrap();
            self.builder.build_load(ptr_t, tp, "tail").unwrap()
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

    /// `List.member x xs : Bool` — walk comparing each element to `x`
    /// (scalar elements only), short-circuiting on the first match.
    fn kernel_list_member(&mut self, args: &[TypedExpr]) -> Result<BasicValueEnum<'ctx>, String> {
        let elem = self.elem_layout(&args[1].tipe)?;
        if !elem.is_scalar() {
            return Err("typed backend: List.member is only supported on scalar elements".to_string());
        }
        let is_float = matches!(elem, Layout::Float);
        let target = self.gen(&args[0])?;
        let list = self.gen(&args[1])?.into_pointer_value();
        let cell = self.cons_cell(&elem);
        let elem_ty = self.llvm_type(&elem);
        let ptr_t = self.ctx.ptr_type(inkwell::AddressSpace::default());
        let entry = self.builder.get_insert_block().unwrap();
        let loop_bb = self.new_block("mem.loop");
        let body_bb = self.new_block("mem.body");
        let cont_bb = self.new_block("mem.cont");
        let found_bb = self.new_block("mem.found");
        let none_bb = self.new_block("mem.none");
        let merge = self.new_block("mem.end");

        self.builder.build_unconditional_branch(loop_bb).unwrap();
        self.builder.position_at_end(loop_bb);
        let cur = self.builder.build_phi(ptr_t, "cur").unwrap();
        cur.add_incoming(&[(&list as &dyn BasicValue, entry)]);
        let cur_ptr = cur.as_basic_value().into_pointer_value();
        let is_null = self.builder.build_is_null(cur_ptr, "end").unwrap();
        self.builder.build_conditional_branch(is_null, none_bb, body_bb).unwrap();

        self.builder.position_at_end(body_bb);
        let hp = self.builder.build_struct_gep(cell, cur_ptr, 0, "hp").unwrap();
        let head = self.builder.build_load(elem_ty, hp, "head").unwrap();
        let eq = if is_float {
            self.builder
                .build_float_compare(FloatPredicate::OEQ, head.into_float_value(), target.into_float_value(), "eq")
                .unwrap()
        } else {
            self.builder
                .build_int_compare(IntPredicate::EQ, head.into_int_value(), target.into_int_value(), "eq")
                .unwrap()
        };
        self.builder.build_conditional_branch(eq, found_bb, cont_bb).unwrap();

        self.builder.position_at_end(cont_bb);
        let tp = self.builder.build_struct_gep(cell, cur_ptr, 1, "tp").unwrap();
        let next = self.builder.build_load(ptr_t, tp, "next").unwrap();
        let cont_end = self.builder.get_insert_block().unwrap();
        self.builder.build_unconditional_branch(loop_bb).unwrap();
        cur.add_incoming(&[(&next as &dyn BasicValue, cont_end)]);

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
        let a_cell = self.cons_cell(&a_elem);
        let b_cell = self.cons_cell(&b_elem);
        let ptr_t = self.ctx.ptr_type(inkwell::AddressSpace::default());

        let result = self.entry_alloca(ptr_t.into(), "result");
        self.builder.build_store(result, ptr_t.const_null()).unwrap();
        let slot = self.entry_alloca(ptr_t.into(), "slot");
        self.builder.build_store(slot, result).unwrap();

        let entry = self.builder.get_insert_block().unwrap();
        let loop_bb = self.new_block("map2.loop");
        let body_bb = self.new_block("map2.body");
        let done_bb = self.new_block("map2.done");

        self.builder.build_unconditional_branch(loop_bb).unwrap();
        self.builder.position_at_end(loop_bb);
        let cx = self.builder.build_phi(ptr_t, "cx").unwrap();
        let cy = self.builder.build_phi(ptr_t, "cy").unwrap();
        cx.add_incoming(&[(&xs as &dyn BasicValue, entry)]);
        cy.add_incoming(&[(&ys as &dyn BasicValue, entry)]);
        let cxp = cx.as_basic_value().into_pointer_value();
        let cyp = cy.as_basic_value().into_pointer_value();
        let xnull = self.builder.build_is_null(cxp, "xn").unwrap();
        let ynull = self.builder.build_is_null(cyp, "yn").unwrap();
        let either = self.builder.build_or(xnull, ynull, "either").unwrap();
        self.builder.build_conditional_branch(either, done_bb, body_bb).unwrap();

        self.builder.position_at_end(body_bb);
        let hxp = self.builder.build_struct_gep(a_cell, cxp, 0, "hxp").unwrap();
        let hx = self.builder.build_load(self.llvm_type(&a_elem), hxp, "hx").unwrap();
        let hyp = self.builder.build_struct_gep(b_cell, cyp, 0, "hyp").unwrap();
        let hy = self.builder.build_load(self.llvm_type(&b_elem), hyp, "hy").unwrap();
        let mapped = self.apply_fn_expr(&args[0], &[hx, hy])?;
        let newcell = self.cons(&c_elem, mapped, ptr_t.const_null());
        let dest = self.builder.build_load(ptr_t, slot, "dest").unwrap().into_pointer_value();
        self.builder.build_store(dest, newcell).unwrap();
        let nf = self.builder.build_struct_gep(self.cons_cell(&c_elem), newcell, 1, "nf").unwrap();
        self.builder.build_store(slot, nf).unwrap();
        let nxp = self.builder.build_struct_gep(a_cell, cxp, 1, "nxp").unwrap();
        let nx = self.builder.build_load(ptr_t, nxp, "nx").unwrap();
        let nyp = self.builder.build_struct_gep(b_cell, cyp, 1, "nyp").unwrap();
        let ny = self.builder.build_load(ptr_t, nyp, "ny").unwrap();
        let body_end = self.builder.get_insert_block().unwrap();
        self.builder.build_unconditional_branch(loop_bb).unwrap();
        cx.add_incoming(&[(&nx as &dyn BasicValue, body_end)]);
        cy.add_incoming(&[(&ny as &dyn BasicValue, body_end)]);

        self.builder.position_at_end(done_bb);
        Ok(self.builder.build_load(ptr_t, result, "map2").unwrap())
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
        let a_cell = self.cons_cell(&a_elem);
        let i64_t = self.ctx.i64_type();
        let ptr_t = self.ctx.ptr_type(inkwell::AddressSpace::default());

        let result = self.entry_alloca(ptr_t.into(), "result");
        self.builder.build_store(result, ptr_t.const_null()).unwrap();
        let slot = self.entry_alloca(ptr_t.into(), "slot");
        self.builder.build_store(slot, result).unwrap();

        let entry = self.builder.get_insert_block().unwrap();
        let loop_bb = self.new_block("imap.loop");
        let body_bb = self.new_block("imap.body");
        let done_bb = self.new_block("imap.done");

        self.builder.build_unconditional_branch(loop_bb).unwrap();
        self.builder.position_at_end(loop_bb);
        let cur = self.builder.build_phi(ptr_t, "cur").unwrap();
        let idx = self.builder.build_phi(i64_t, "i").unwrap();
        cur.add_incoming(&[(&list as &dyn BasicValue, entry)]);
        let zero: BasicValueEnum = i64_t.const_zero().into();
        idx.add_incoming(&[(&zero as &dyn BasicValue, entry)]);
        let cur_ptr = cur.as_basic_value().into_pointer_value();
        let is_null = self.builder.build_is_null(cur_ptr, "end").unwrap();
        self.builder.build_conditional_branch(is_null, done_bb, body_bb).unwrap();

        self.builder.position_at_end(body_bb);
        let hp = self.builder.build_struct_gep(a_cell, cur_ptr, 0, "hp").unwrap();
        let head = self.builder.build_load(self.llvm_type(&a_elem), hp, "head").unwrap();
        let mapped = self.apply_fn_expr(&args[0], &[idx.as_basic_value(), head])?;
        let newcell = self.cons(&b_elem, mapped, ptr_t.const_null());
        let dest = self.builder.build_load(ptr_t, slot, "dest").unwrap().into_pointer_value();
        self.builder.build_store(dest, newcell).unwrap();
        let nf = self.builder.build_struct_gep(self.cons_cell(&b_elem), newcell, 1, "nf").unwrap();
        self.builder.build_store(slot, nf).unwrap();
        let tp = self.builder.build_struct_gep(a_cell, cur_ptr, 1, "tp").unwrap();
        let next = self.builder.build_load(ptr_t, tp, "next").unwrap();
        let idx2: BasicValueEnum = self
            .builder
            .build_int_add(idx.as_basic_value().into_int_value(), i64_t.const_int(1, false), "i2")
            .unwrap()
            .into();
        let body_end = self.builder.get_insert_block().unwrap();
        self.builder.build_unconditional_branch(loop_bb).unwrap();
        cur.add_incoming(&[(&next as &dyn BasicValue, body_end)]);
        idx.add_incoming(&[(&idx2 as &dyn BasicValue, body_end)]);

        self.builder.position_at_end(done_bb);
        Ok(self.builder.build_load(ptr_t, result, "imap").unwrap())
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
        let cell = self.cons_cell(elem);
        let elem_ty = self.llvm_type(elem);
        let ptr_t = self.ctx.ptr_type(inkwell::AddressSpace::default());

        let result = self.entry_alloca(ptr_t.into(), "result");
        self.builder.build_store(result, ptr_t.const_null()).unwrap();
        let slot = self.entry_alloca(ptr_t.into(), "slot");
        self.builder.build_store(slot, result).unwrap();

        let entry = self.builder.get_insert_block().unwrap();
        let loop_bb = self.new_block("app.loop");
        let body_bb = self.new_block("app.body");
        let done_bb = self.new_block("app.done");

        self.builder.build_unconditional_branch(loop_bb).unwrap();
        self.builder.position_at_end(loop_bb);
        let cur = self.builder.build_phi(ptr_t, "cur").unwrap();
        cur.add_incoming(&[(&left as &dyn BasicValue, entry)]);
        let cur_ptr = cur.as_basic_value().into_pointer_value();
        let is_null = self.builder.build_is_null(cur_ptr, "end").unwrap();
        self.builder
            .build_conditional_branch(is_null, done_bb, body_bb)
            .unwrap();

        self.builder.position_at_end(body_bb);
        let hp = self.builder.build_struct_gep(cell, cur_ptr, 0, "hp").unwrap();
        let head = self.builder.build_load(elem_ty, hp, "head").unwrap();
        let newcell = self.cons(elem, head, ptr_t.const_null());
        let dest = self.builder.build_load(ptr_t, slot, "dest").unwrap().into_pointer_value();
        self.builder.build_store(dest, newcell).unwrap();
        let nf = self.builder.build_struct_gep(cell, newcell, 1, "nf").unwrap();
        self.builder.build_store(slot, nf).unwrap();
        let tp = self.builder.build_struct_gep(cell, cur_ptr, 1, "tp").unwrap();
        let next = self.builder.build_load(ptr_t, tp, "next").unwrap();
        let body_end = self.builder.get_insert_block().unwrap();
        self.builder.build_unconditional_branch(loop_bb).unwrap();
        cur.add_incoming(&[(&next as &dyn BasicValue, body_end)]);

        self.builder.position_at_end(done_bb);
        // Point the tail (last cell's next, or `result` if left was empty) at
        // the right-hand list.
        let dest = self.builder.build_load(ptr_t, slot, "dest2").unwrap().into_pointer_value();
        self.builder.build_store(dest, right).unwrap();
        Ok(self.builder.build_load(ptr_t, result, "appended").unwrap())
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
        let eq = self.equals_vals(lv, rv, &layout)?;
        Ok(if negate {
            self.builder.build_not(eq, "neq").unwrap().into()
        } else {
            eq.into()
        })
    }

    /// Recursively test two values of a given layout for equality, yielding an
    /// i1.
    fn equals_vals(
        &mut self,
        a: BasicValueEnum<'ctx>,
        b: BasicValueEnum<'ctx>,
        layout: &Layout,
    ) -> Result<inkwell::values::IntValue<'ctx>, String> {
        match layout {
            Layout::Int | Layout::Char | Layout::Bool | Layout::Enum(_) => Ok(self
                .builder
                .build_int_compare(IntPredicate::EQ, a.into_int_value(), b.into_int_value(), "eq")
                .unwrap()),
            Layout::Float => Ok(self
                .builder
                .build_float_compare(FloatPredicate::OEQ, a.into_float_value(), b.into_float_value(), "eq")
                .unwrap()),
            Layout::Str => {
                let cmp = self.call_named("rt_eq", &[a, b]);
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
            // An opaque boxed reference (e.g. the phantom element type of an
            // empty list): fall back to pointer identity. This code is only
            // reached for values that are genuinely opaque; for `[] == []` the
            // element comparison is generated but never executed.
            Layout::Ref => {
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

    /// List equality: walk both lists in lockstep — equal iff same length and
    /// all elements equal.
    fn equals_lists(
        &mut self,
        a: BasicValueEnum<'ctx>,
        b: BasicValueEnum<'ctx>,
        elem: &Layout,
    ) -> Result<inkwell::values::IntValue<'ctx>, String> {
        let ptr_t = self.ctx.ptr_type(inkwell::AddressSpace::default());
        let i1_t = self.ctx.bool_type();
        let cell = self.cons_cell(elem);
        let elem_ty = self.llvm_type(elem);
        let entry = self.builder.get_insert_block().unwrap();
        let loop_bb = self.new_block("eql.loop");
        let body_bb = self.new_block("eql.body");
        let adv_bb = self.new_block("eql.adv");
        let null_bb = self.new_block("eql.null");
        let false_bb = self.new_block("eql.false");
        let done_bb = self.new_block("eql.done");

        self.builder.build_unconditional_branch(loop_bb).unwrap();
        self.builder.position_at_end(loop_bb);
        let ca = self.builder.build_phi(ptr_t, "ca").unwrap();
        let cb = self.builder.build_phi(ptr_t, "cb").unwrap();
        ca.add_incoming(&[(&a as &dyn BasicValue, entry)]);
        cb.add_incoming(&[(&b as &dyn BasicValue, entry)]);
        let cap = ca.as_basic_value().into_pointer_value();
        let cbp = cb.as_basic_value().into_pointer_value();
        let anull = self.builder.build_is_null(cap, "an").unwrap();
        let bnull = self.builder.build_is_null(cbp, "bn").unwrap();
        let anyn = self.builder.build_or(anull, bnull, "anyn").unwrap();
        self.builder.build_conditional_branch(anyn, null_bb, body_bb).unwrap();

        // One or both ended: equal iff both ended together.
        self.builder.position_at_end(null_bb);
        let both = self.builder.build_and(anull, bnull, "both").unwrap();
        self.builder.build_unconditional_branch(done_bb).unwrap();

        // Compare heads.
        self.builder.position_at_end(body_bb);
        let hap = self.builder.build_struct_gep(cell, cap, 0, "hap").unwrap();
        let ha = self.builder.build_load(elem_ty, hap, "ha").unwrap();
        let hbp = self.builder.build_struct_gep(cell, cbp, 0, "hbp").unwrap();
        let hb = self.builder.build_load(elem_ty, hbp, "hb").unwrap();
        let heq = self.equals_vals(ha, hb, elem)?;
        let after_head = self.builder.get_insert_block().unwrap();
        self.builder.build_conditional_branch(heq, adv_bb, false_bb).unwrap();
        let _ = after_head;

        self.builder.position_at_end(adv_bb);
        let nap = self.builder.build_struct_gep(cell, cap, 1, "nap").unwrap();
        let na = self.builder.build_load(ptr_t, nap, "na").unwrap();
        let nbp = self.builder.build_struct_gep(cell, cbp, 1, "nbp").unwrap();
        let nb = self.builder.build_load(ptr_t, nbp, "nb").unwrap();
        self.builder.build_unconditional_branch(loop_bb).unwrap();
        ca.add_incoming(&[(&na as &dyn BasicValue, adv_bb)]);
        cb.add_incoming(&[(&nb as &dyn BasicValue, adv_bb)]);

        self.builder.position_at_end(false_bb);
        self.builder.build_unconditional_branch(done_bb).unwrap();

        self.builder.position_at_end(done_bb);
        let phi = self.builder.build_phi(i1_t, "listeq").unwrap();
        let fals: BasicValueEnum = i1_t.const_zero().into();
        let both_v: BasicValueEnum = both.into();
        phi.add_incoming(&[(&both_v as &dyn BasicValue, null_bb), (&fals as &dyn BasicValue, false_bb)]);
        Ok(phi.as_basic_value().into_int_value())
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

    /// Closure-convert a lambda: capture the free variables in scope, lift the
    /// body to a top-level function taking the captured environment as its
    /// first parameter, and allocate a closure `{fn_ptr, captures...}`.
    fn gen_closure(
        &mut self,
        params: &[(crate::ast::canonical::Pattern, crate::ast::canonical::Type)],
        body: &TypedExpr,
        result_layout: &Layout,
    ) -> Result<BasicValueEnum<'ctx>, String> {
        // Parameter names (simple variables only for now).
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

        // Free variables = referenced locals not bound within the lambda.
        let mut bound: std::collections::HashSet<String> =
            param_names.iter().cloned().collect();
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
        self.cur_fn = Some(lifted);
        let entry = self.ctx.append_basic_block(lifted, "entry");
        self.builder.position_at_end(entry);
        let env = lifted.get_nth_param(0).unwrap().into_pointer_value();
        for (i, (n, _)) in captures.iter().enumerate() {
            let fp = self
                .builder
                .build_struct_gep(clos_ty, env, (i + 1) as u32, "capp")
                .unwrap();
            let v = self.builder.build_load(struct_fields[i + 1], fp, n).unwrap();
            self.locals.insert(n.clone(), v);
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

        let capture_vals: Vec<BasicValueEnum> = captures.iter().map(|(_, v)| *v).collect();
        Ok(self
            .build_closure_value(lifted.as_global_value().as_pointer_value(), &capture_vals)
            .into())
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

    /// A closure wrapping a named function used as a first-class value: an
    /// env-ignoring trampoline `wrap(env, args...) = f(args...)`, cached.
    fn wrap_global(&mut self, mangled: &str) -> inkwell::values::PointerValue<'ctx> {
        if let Some(w) = self.wrappers.get(mangled) {
            return self.build_closure_value(w.as_global_value().as_pointer_value(), &[]);
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
        let entry = self.ctx.append_basic_block(wrapper, "entry");
        self.builder.position_at_end(entry);
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
        self.wrappers.insert(mangled.to_string(), wrapper);
        self.build_closure_value(wrapper.as_global_value().as_pointer_value(), &[])
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
        let layout = self.layouts.layout_of(&l.tipe);
        // Comparisons on boxed values (strings) must compare contents, not the
        // pointer words — route through the runtime's value comparison.
        if matches!(layout, Layout::Str) {
            let sym = match op {
                "==" => "rt_eq",
                "/=" => "rt_neq",
                "<" => "rt_lt",
                "<=" => "rt_le",
                ">" => "rt_gt",
                ">=" => "rt_ge",
                _ => return Err(format!("typed backend: `{}` is not supported on strings", op)),
            };
            let lv = self.gen(l)?;
            let rv = self.gen(r)?;
            let cmp = self.call_named(sym, &[lv, rv]);
            return Ok(self.call_named("rt_is_true", &[cmp]));
        }
        let is_float = matches!(layout, Layout::Float);
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
