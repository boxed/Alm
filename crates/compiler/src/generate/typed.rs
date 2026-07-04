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
            _ => self.ctx.i64_type().into(),
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
        let f = self.module.get_function(name).unwrap();
        self.builder
            .build_call(f, &[arg.into()], "box")
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
            TypedKind::Call(func, args) => self.gen_call(func, args),
            TypedKind::Binop(op, _, _, l, r) => self.gen_binop(op.as_str(), l, r),
            TypedKind::If(branches, otherwise) => self.gen_if(branches, otherwise, expr),
            TypedKind::Let(decls, body) => self.gen_let(decls, body),
            TypedKind::Case(scrutinee, branches) => self.gen_case(scrutinee, branches, expr),
            TypedKind::Negate(inner) => self.gen_negate(inner),
            other => Err(format!(
                "typed backend: unsupported expression {:?}",
                std::mem::discriminant(other)
            )),
        }
    }

    fn gen_call(
        &mut self,
        func: &TypedExpr,
        args: &[TypedExpr],
    ) -> Result<BasicValueEnum<'ctx>, String> {
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
                    match simple_param_name(pattern) {
                        Some(name) => {
                            self.locals.insert(name, v);
                        }
                        None => {
                            return Err(
                                "typed backend: only simple `let` destructures are supported"
                                    .to_string(),
                            )
                        }
                    }
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
        use crate::ast::canonical::Pattern_::*;
        let subject = self.gen(scrutinee)?;
        let result_ty = self.llvm_type(&self.layouts.layout_of(&whole.tipe));
        let merge = self.new_block("case.end");
        let mut incoming: Vec<(BasicValueEnum<'ctx>, inkwell::basic_block::BasicBlock<'ctx>)> =
            Vec::new();
        let mut matched_all = false;

        for (pattern, body) in branches {
            let test: Option<inkwell::values::IntValue<'ctx>> = match &pattern.value {
                Var(name) => {
                    self.locals.insert(name.to_string(), subject);
                    None
                }
                Anything => None,
                Alias(inner, name) if matches!(inner.value, Anything | Var(_)) => {
                    self.locals.insert(name.value.to_string(), subject);
                    None
                }
                Int(n) => Some(
                    self.builder
                        .build_int_compare(
                            IntPredicate::EQ,
                            subject.into_int_value(),
                            self.ctx.i64_type().const_int(*n as u64, true),
                            "casei",
                        )
                        .unwrap(),
                ),
                Chr(c) => Some(
                    self.builder
                        .build_int_compare(
                            IntPredicate::EQ,
                            subject.into_int_value(),
                            self.ctx.i32_type().const_int(*c as u64, false),
                            "casec",
                        )
                        .unwrap(),
                ),
                _ => {
                    return Err(
                        "typed backend: only scalar/wildcard case patterns are supported yet"
                            .to_string(),
                    )
                }
            };

            match test {
                None => {
                    // Catch-all: this branch always matches; stop here.
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
