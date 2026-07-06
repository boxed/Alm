//! Native backend: compile the lowered IR to a binary via LLVM (inkwell).
//!
//! The codegen builds an LLVM module directly through inkwell's typed
//! builder, verifies it, and emits an object file with the host target
//! machine. The object is linked against the runtime (`native_runtime.c`
//! for now) into an executable.
//!
//! Representation matches the JS backend's conventions: every value is an
//! opaque boxed pointer (`ptr`), numbers and comparisons are runtime
//! calls, `if`/`case` compile to basic blocks, and tail-recursive
//! functions compile to a loop over stack slots. Memory is not yet
//! reclaimed — reference counting is a later pass.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use inkwell::builder::Builder;
use inkwell::context::Context;
use inkwell::module::{Linkage, Module};
use inkwell::targets::{
    CodeModel, FileType, InitializationConfig, RelocMode, Target as LlvmTarget, TargetMachine,
    TargetTriple,
};
use inkwell::types::{FloatType, IntType, PointerType};
use inkwell::values::{
    BasicMetadataValueEnum, FunctionValue, GlobalValue, IntValue, PointerValue,
};
use inkwell::{AddressSpace, OptimizationLevel};

use crate::ir::{Branch, Expr, Function, PrimOp, Program, Step, Test};

/// The native runtime, compiled from Rust to a static library by
/// `build.rs`. Linked into every native binary — the Rust twin of the JS
/// backend's embedded `runtime.js`.
pub const RUNTIME_LIB: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/libalm_runtime.a"));

/// The same runtime as optimized LLVM bitcode. Merged into each program
/// module (as `available_externally`) so LLVM can inline the runtime's hot
/// primitives into generated code; the real symbols still come from the
/// static library at link time.
pub const RUNTIME_BC: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/alm_runtime.bc"));

/// The same runtime and wasm link inputs, built for wasm32-wasi.
pub const RUNTIME_LIB_WASM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/libalm_runtime_wasm.a"));
pub const RUNTIME_BC_WASM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/alm_runtime_wasm.bc"));
const WASM_CRT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/crt1-command.o"));
const WASM_LIBC: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/libc.a"));

/// Which machine target to emit.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// A native executable for the host.
    Native,
    /// A `wasm32-wasi` module (runnable with wasmtime or node's WASI).
    Wasm,
}

/// Compile `program` into an executable (native) or `.wasm` module at `output`.
pub fn build(program: &Program, output: &Path, target: Target) -> Result<(), String> {
    let context = Context::create();
    let mut cg = Codegen::new(&context);
    cg.emit_program(program);

    cg.module
        .verify()
        .map_err(|e| format!("internal error: generated invalid LLVM IR:\n{}", e))?;

    finish(&cg.module, &context, output, target)
}

/// Shared back half of every native/wasm build: pick the target machine,
/// merge the runtime bitcode for cross-module inlining, run the optimizer,
/// emit an object, and link it against the runtime. Used by both the uniform
/// backend above and the typed (monomorphized) backend.
pub(crate) fn finish<'ctx>(
    module: &Module<'ctx>,
    context: &'ctx Context,
    output: &Path,
    target: Target,
) -> Result<(), String> {
    let (triple, machine) = match target {
        Target::Native => {
            LlvmTarget::initialize_native(&InitializationConfig::default())?;
            let triple = TargetMachine::get_default_triple();
            let t = LlvmTarget::from_triple(&triple).map_err(|e| e.to_string())?;
            // Host cpu/features so generated functions' target attributes
            // match the runtime bitcode's (built with target-cpu=native),
            // letting LLVM inline the runtime into generated code.
            let cpu = TargetMachine::get_host_cpu_name();
            let features = TargetMachine::get_host_cpu_features();
            let m = t
                .create_target_machine(
                    &triple,
                    cpu.to_str().unwrap(),
                    features.to_str().unwrap(),
                    OptimizationLevel::Default,
                    RelocMode::PIC,
                    CodeModel::Default,
                )
                .ok_or("could not create target machine")?;
            (triple, m)
        }
        Target::Wasm => {
            LlvmTarget::initialize_webassembly(&InitializationConfig::default());
            let triple = TargetTriple::create("wasm32-wasi");
            let t = LlvmTarget::from_triple(&triple).map_err(|e| e.to_string())?;
            let m = t
                .create_target_machine(
                    &triple,
                    "generic",
                    "",
                    OptimizationLevel::Default,
                    RelocMode::Static,
                    CodeModel::Default,
                )
                .ok_or("could not create wasm target machine")?;
            (triple, m)
        }
    };
    module.set_triple(&triple);
    module
        .set_data_layout(&machine.get_target_data().get_data_layout());

    let (runtime_bc, runtime_lib) = match target {
        Target::Native => (RUNTIME_BC, RUNTIME_LIB),
        Target::Wasm => (RUNTIME_BC_WASM, RUNTIME_LIB_WASM),
    };

    let build_dir = output.with_extension("build");
    std::fs::create_dir_all(&build_dir)
        .map_err(|e| format!("could not create {}: {}", build_dir.display(), e))?;

    // Merge the runtime bitcode so the optimizer can inline the runtime's
    // hot primitives (rt_add, rt_ctor, rt_apply, the list kernels …) into
    // generated code. Every definition it brings in is marked
    // `available_externally`: available for inlining but not emitted, so the
    // real symbols still resolve to the static library at link time (and no
    // symbol is duplicated).
    let bc_path = build_dir.join("alm_runtime.bc");
    std::fs::write(&bc_path, runtime_bc).map_err(|e| e.to_string())?;
    let runtime_module = Module::parse_bitcode_from_path(&bc_path, context)
        .map_err(|e| format!("could not parse runtime bitcode: {}", e))?;
    // The typed backend emits DWARF debug info (line tables mapping generated
    // code back to `.elm` source) and sets the "Debug Info Version"/"Dwarf
    // Version" module flags. The runtime bitcode (from rustc) may carry its own
    // debug info and conflicting flag values, which makes `link_in_module`
    // fail. Strip the runtime's debug info so only the program's survives; the
    // program's line tables are preserved through the link and O2.
    runtime_module.strip_debug_info();
    // Only exported (external-linkage) definitions live in the static
    // library, so only those may become `available_externally` (inline-only,
    // resolved to the .a). Internal helpers (value_eq, the allocator, …) are
    // not exported, so keep them as internal definitions merged into the
    // object.
    for function in runtime_module.get_functions() {
        if function.count_basic_blocks() > 0 && function.get_linkage() == Linkage::External {
            function.set_linkage(Linkage::AvailableExternally);
        }
    }
    let mut global = runtime_module.get_first_global();
    while let Some(g) = global {
        global = g.get_next_global();
        if g.get_initializer().is_some() && g.get_linkage() == Linkage::External {
            g.set_linkage(Linkage::AvailableExternally);
        }
    }
    module
        .link_in_module(runtime_module)
        .map_err(|e| format!("could not merge runtime bitcode: {}", e))?;

    // Run LLVM's optimization pipeline (inlining, mem2reg, GVN, …). With the
    // runtime merged in, this inlines it into the generated code.
    module
        .run_passes(
            "default<O2>",
            &machine,
            inkwell::passes::PassBuilderOptions::create(),
        )
        .map_err(|e| e.to_string())?;

    if std::env::var("ALM_DUMP_IR").is_ok() {
        let _ = module.print_to_file(build_dir.join("program.ll"));
    }

    let object = build_dir.join("program.o");
    machine
        .write_to_file(&module, FileType::Object, &object)
        .map_err(|e| e.to_string())?;

    let runtime = build_dir.join("runtime.a");
    std::fs::write(&runtime, runtime_lib).map_err(|e| e.to_string())?;

    match target {
        Target::Native => {
            // The C compiler driver is used purely as a linker — no C is compiled.
            run_linker(
                "cc",
                &[&object, &runtime, Path::new("-o"), output],
            )
        }
        Target::Wasm => {
            // Link a WASI command module: crt + program + runtime + libc.
            // The runtime and libc are mutually dependent (the runtime calls
            // libc's fd_write/clock; libc's crt calls the runtime's `main`),
            // so wrap them in a group so wasm-ld re-scans to resolve both.
            let crt = build_dir.join("crt1-command.o");
            let libc = build_dir.join("libc.a");
            std::fs::write(&crt, WASM_CRT).map_err(|e| e.to_string())?;
            std::fs::write(&libc, WASM_LIBC).map_err(|e| e.to_string())?;
            // `--undefined=main` forces `main` to be pulled from the runtime
            // archive during its scan, so libc's crt (`__main_void`, listed
            // after) resolves it without needing archive re-scanning.
            let result = Command::new(env!("ALM_WASM_LD"))
                .arg("--undefined=main")
                .arg(&crt)
                .arg(&object)
                .arg(&runtime)
                .arg(&libc)
                .arg("-o")
                .arg(output)
                .output()
                .map_err(|e| format!("could not run wasm-ld: {}", e))?;
            if !result.status.success() {
                return Err(format!(
                    "linking failed:\n{}",
                    String::from_utf8_lossy(&result.stderr)
                ));
            }
            Ok(())
        }
    }
}

fn run_linker(linker: &str, args: &[&Path]) -> Result<(), String> {
    let result = Command::new(linker)
        .args(args)
        .output()
        .map_err(|e| format!("could not run {}: {}", linker, e))?;
    if !result.status.success() {
        return Err(format!(
            "linking failed:\n{}",
            String::from_utf8_lossy(&result.stderr)
        ));
    }
    Ok(())
}

/// A runtime function's return shape.
enum Ret {
    /// A value word (i64).
    Value,
    Void,
    Bool,
}

/// A runtime function parameter type.
#[derive(Clone, Copy)]
enum Ty {
    /// A value word (i64) — the universal representation of an Elm value.
    Val,
    /// A real pointer (string data, ctor/field name, closure fn, arg array).
    P,
    I64,
    I32,
    F64,
    I1,
}

/// A local variable: an SSA value (i64), or a stack slot (alloca) loaded on
/// each use (tail-loop parameters).
#[derive(Clone, Copy)]
enum Binding<'ctx> {
    Reg(IntValue<'ctx>),
    Slot(PointerValue<'ctx>),
}

struct Codegen<'ctx> {
    ctx: &'ctx Context,
    module: Module<'ctx>,
    builder: Builder<'ctx>,
    /// A second builder kept at the current function's entry block, so all
    /// `alloca`s land there and never grow the stack inside a tail loop.
    entry_builder: Builder<'ctx>,

    ptr_t: PointerType<'ctx>,
    i64_t: IntType<'ctx>,
    i32_t: IntType<'ctx>,
    i1_t: IntType<'ctx>,
    f64_t: FloatType<'ctx>,

    runtime: HashMap<&'static str, FunctionValue<'ctx>>,
    functions: HashMap<String, FunctionValue<'ctx>>,
    arities: HashMap<String, usize>,
    globals: HashMap<String, GlobalValue<'ctx>>,
    externs: HashMap<String, GlobalValue<'ctx>>,
    strings: HashMap<String, PointerValue<'ctx>>,
    cstrings: HashMap<String, PointerValue<'ctx>>,

    // Per-function state.
    cur_fn: Option<FunctionValue<'ctx>>,
    locals: HashMap<String, Binding<'ctx>>,
    slots: Vec<PointerValue<'ctx>>,
    loop_bb: Option<inkwell::basic_block::BasicBlock<'ctx>>,
    blk_id: usize,
}

impl<'ctx> Codegen<'ctx> {
    fn new(ctx: &'ctx Context) -> Self {
        let module = ctx.create_module("alm");
        let ptr_t = ctx.ptr_type(AddressSpace::default());
        let mut cg = Codegen {
            ctx,
            module,
            builder: ctx.create_builder(),
            entry_builder: ctx.create_builder(),
            ptr_t,
            i64_t: ctx.i64_type(),
            i32_t: ctx.i32_type(),
            i1_t: ctx.bool_type(),
            f64_t: ctx.f64_type(),
            runtime: HashMap::new(),
            functions: HashMap::new(),
            arities: HashMap::new(),
            globals: HashMap::new(),
            externs: HashMap::new(),
            strings: HashMap::new(),
            cstrings: HashMap::new(),
            cur_fn: None,
            locals: HashMap::new(),
            slots: Vec::new(),
            loop_bb: None,
            blk_id: 0,
        };
        cg.declare_runtime();
        cg
    }

    fn declare_runtime(&mut self) {
        use Ret::*;
        use Ty::*;
        let table: &[(&'static str, Ret, &[Ty])] = &[
            ("rt_int", Value, &[I64]),
            ("rt_float", Value, &[F64]),
            ("rt_chr", Value, &[I32]),
            ("rt_str", Value, &[P, I64]),
            ("rt_ctor", Value, &[P, I32, I32, P]),
            ("rt_list", Value, &[I32, P]),
            ("rt_tuple", Value, &[I32, P]),
            ("rt_closure", Value, &[P, I32, I32, P]),
            ("rt_closure_set", Void, &[Val, I32, Val]),
            ("rt_apply", Value, &[Val, I32, P]),
            ("rt_record_new", Value, &[I32]),
            ("rt_record_set", Void, &[Val, I32, P, Val]),
            ("rt_record_clone", Value, &[Val]),
            ("rt_record_replace", Void, &[Val, P, Val]),
            ("rt_access", Value, &[Val, P]),
            ("rt_ctor_arg", Value, &[Val, I32]),
            ("rt_tuple_item", Value, &[Val, I32]),
            ("rt_list_head", Value, &[Val]),
            ("rt_list_tail", Value, &[Val]),
            ("rt_is_true", Bool, &[Val]),
            ("rt_is_ctor", Bool, &[Val, I32]),
            ("rt_is_bool", Bool, &[Val, I1]),
            ("rt_is_int", Bool, &[Val, I64]),
            ("rt_is_chr", Bool, &[Val, I32]),
            ("rt_is_str", Bool, &[Val, P, I64]),
            ("rt_is_cons", Bool, &[Val]),
            ("rt_is_nil", Bool, &[Val]),
            ("rt_add", Value, &[Val, Val]),
            ("rt_sub", Value, &[Val, Val]),
            ("rt_mul", Value, &[Val, Val]),
            ("rt_fdiv", Value, &[Val, Val]),
            ("rt_idiv", Value, &[Val, Val]),
            ("rt_pow", Value, &[Val, Val]),
            ("rt_neg", Value, &[Val]),
            ("rt_eq", Value, &[Val, Val]),
            ("rt_neq", Value, &[Val, Val]),
            ("rt_lt", Value, &[Val, Val]),
            ("rt_le", Value, &[Val, Val]),
            ("rt_gt", Value, &[Val, Val]),
            ("rt_ge", Value, &[Val, Val]),
            ("rt_append", Value, &[Val, Val]),
            ("rt_cons", Value, &[Val, Val]),
            ("rt_crash", Void, &[P]),
        ];
        for (name, ret, params) in table {
            let param_types: Vec<_> = params.iter().map(|t| self.meta_type(*t)).collect();
            let fn_type = match ret {
                Ret::Value => self.i64_t.fn_type(&param_types, false),
                Ret::Bool => self.i1_t.fn_type(&param_types, false),
                Ret::Void => self.ctx.void_type().fn_type(&param_types, false),
            };
            let f = self.module.add_function(name, fn_type, Some(Linkage::External));
            self.runtime.insert(name, f);
        }
        // Singleton values loaded directly by generated code (i64 words).
        for name in ["rt_true_v", "rt_false_v", "rt_unit_v"] {
            let g = self.module.add_global(self.i64_t, None, name);
            self.externs.insert(name.to_string(), g);
        }
    }

    fn meta_type(&self, t: Ty) -> inkwell::types::BasicMetadataTypeEnum<'ctx> {
        match t {
            Ty::Val => self.i64_t.into(),
            Ty::P => self.ptr_t.into(),
            Ty::I64 => self.i64_t.into(),
            Ty::I32 => self.i32_t.into(),
            Ty::F64 => self.f64_t.into(),
            Ty::I1 => self.i1_t.into(),
        }
    }

    // MODULE ASSEMBLY

    fn emit_program(&mut self, program: &Program) {
        for f in &program.functions {
            self.arities.insert(f.name.clone(), f.params.len());
        }

        // Forward-declare every user function so calls can reference them.
        // Values are i64 words.
        for f in &program.functions {
            let params = vec![self.i64_t.into(); f.params.len()];
            let fn_type = self.i64_t.fn_type(&params, false);
            let fv = self
                .module
                .add_function(&f.name, fn_type, Some(Linkage::External));
            self.functions.insert(f.name.clone(), fv);
        }

        // Declare the globals holding zero-argument top-level values.
        for value in &program.values {
            let g = self.module.add_global(self.i64_t, None, &value.name);
            g.set_initializer(&self.i64_t.const_zero());
            g.set_linkage(Linkage::Internal);
            self.globals.insert(value.name.clone(), g);
        }

        for f in &program.functions {
            self.emit_function(f);
        }

        // Each global value initializes in its own function.
        let mut init_fns = Vec::new();
        for (i, value) in program.values.iter().enumerate() {
            let name = format!("init.{}", i);
            let fv = self.module.add_function(
                &name,
                self.i64_t.fn_type(&[], false),
                Some(Linkage::Internal),
            );
            self.emit_body(fv, &value.body, &[], 0, false);
            init_fns.push((fv, value.name.clone()));
        }

        self.emit_init(&init_fns);
        self.emit_main(program.main.as_deref());
    }

    /// `alm_init` runs each global value's initializer in order and stores
    /// the result into its global.
    fn emit_init(&mut self, inits: &[(FunctionValue<'ctx>, String)]) {
        let fv = self.module.add_function(
            "alm_init",
            self.ctx.void_type().fn_type(&[], false),
            Some(Linkage::External),
        );
        let block = self.ctx.append_basic_block(fv, "entry");
        self.builder.position_at_end(block);
        for (init_fn, global_name) in inits {
            let value = self
                .builder
                .build_call(*init_fn, &[], "v")
                .unwrap()
                .try_as_basic_value()
                .left()
                .unwrap()
                .into_int_value();
            let global = self.globals[global_name];
            self.builder
                .build_store(global.as_pointer_value(), value)
                .unwrap();
        }
        self.builder.build_return(None).unwrap();
    }

    /// `alm_main` returns the entry module's `main` value, or 0.
    fn emit_main(&mut self, main: Option<&str>) {
        let fv = self.module.add_function(
            "alm_main",
            self.i64_t.fn_type(&[], false),
            Some(Linkage::External),
        );
        let block = self.ctx.append_basic_block(fv, "entry");
        self.builder.position_at_end(block);
        let result = match main {
            Some(name) => self
                .builder
                .build_load(self.i64_t, self.globals[name].as_pointer_value(), "main")
                .unwrap()
                .into_int_value(),
            None => self.i64_t.const_zero(),
        };
        self.builder.build_return(Some(&result)).unwrap();
    }

    fn emit_function(&mut self, function: &Function) {
        let fv = self.functions[&function.name];
        let params: Vec<IntValue> = (0..function.params.len())
            .map(|i| fv.get_nth_param(i as u32).unwrap().into_int_value())
            .collect();
        self.emit_body(fv, &function.body, &params_named(function, &params), function.captures, function.tail_recursive);
    }

    /// Emit a function body. `params` pairs each parameter name with its
    /// incoming value; the first `captures` are closure captures. When
    /// `tail_recursive`, the non-capture parameters get stack slots and
    /// the body runs in a loop block that `TailCall` re-enters.
    fn emit_body(
        &mut self,
        fv: FunctionValue<'ctx>,
        body: &Expr,
        params: &[(String, IntValue<'ctx>)],
        captures: usize,
        tail_recursive: bool,
    ) {
        self.cur_fn = Some(fv);
        self.locals.clear();
        self.slots.clear();
        self.loop_bb = None;

        let entry = self.ctx.append_basic_block(fv, "entry");
        self.entry_builder.position_at_end(entry);

        let first = self.ctx.append_basic_block(fv, "start");
        if tail_recursive {
            self.loop_bb = Some(first);
        }

        for (i, (name, value)) in params.iter().enumerate() {
            if !tail_recursive || i < captures {
                self.locals.insert(name.clone(), Binding::Reg(*value));
            } else {
                let slot = self.entry_builder.build_alloca(self.i64_t, name).unwrap();
                self.entry_builder.build_store(slot, *value).unwrap();
                self.slots.push(slot);
                self.locals.insert(name.clone(), Binding::Slot(slot));
            }
        }

        self.builder.position_at_end(first);
        if let Some(value) = self.gen(body) {
            self.builder.build_return(Some(&value)).unwrap();
        }

        // Close the entry block now that every alloca has been placed.
        self.entry_builder.build_unconditional_branch(first).unwrap();
    }

    // HELPERS

    fn block(&mut self, base: &str) -> inkwell::basic_block::BasicBlock<'ctx> {
        self.blk_id += 1;
        self.ctx
            .append_basic_block(self.cur_fn.unwrap(), &format!("{}{}", base, self.blk_id))
    }

    /// An entry-block alloca for an i64 value slot.
    fn alloca(&self, name: &str) -> PointerValue<'ctx> {
        self.entry_builder.build_alloca(self.i64_t, name).unwrap()
    }

    /// Call a runtime function returning a value word (i64).
    fn call_val(&self, name: &str, args: &[BasicMetadataValueEnum<'ctx>]) -> IntValue<'ctx> {
        self.builder
            .build_call(self.runtime[name], args, "c")
            .unwrap()
            .try_as_basic_value()
            .left()
            .unwrap()
            .into_int_value()
    }

    fn call_bool(&self, name: &str, args: &[BasicMetadataValueEnum<'ctx>]) -> IntValue<'ctx> {
        self.builder
            .build_call(self.runtime[name], args, "c")
            .unwrap()
            .try_as_basic_value()
            .left()
            .unwrap()
            .into_int_value()
    }

    fn call_void(&self, name: &str, args: &[BasicMetadataValueEnum<'ctx>]) {
        self.builder.build_call(self.runtime[name], args, "").unwrap();
    }

    /// A pointer to a stack array of the given value words (or null when
    /// empty), for the runtime's variadic-by-array entry points.
    fn value_array(&self, values: &[IntValue<'ctx>]) -> PointerValue<'ctx> {
        if values.is_empty() {
            return self.ptr_t.const_null();
        }
        let array_type = self.i64_t.array_type(values.len() as u32);
        let array = self.entry_builder.build_alloca(array_type, "args").unwrap();
        for (i, value) in values.iter().enumerate() {
            let slot = unsafe {
                self.builder
                    .build_in_bounds_gep(
                        array_type,
                        array,
                        &[self.i64_t.const_zero(), self.i64_t.const_int(i as u64, false)],
                        "arg",
                    )
                    .unwrap()
            };
            self.builder.build_store(slot, *value).unwrap();
        }
        array
    }

    /// A pointer to string literal data, plus its byte length.
    fn string_data(&mut self, text: &str) -> (PointerValue<'ctx>, u64) {
        let ptr = *self.strings.entry(text.to_string()).or_insert_with(|| {
            self.builder
                .build_global_string_ptr(text, "str")
                .unwrap()
                .as_pointer_value()
        });
        (ptr, text.len() as u64)
    }

    /// A pointer to a nul-terminated C string (field names, ctor names,
    /// crash messages).
    fn cstring(&mut self, text: &str) -> PointerValue<'ctx> {
        *self.cstrings.entry(text.to_string()).or_insert_with(|| {
            self.builder
                .build_global_string_ptr(text, "cstr")
                .unwrap()
                .as_pointer_value()
        })
    }

    /// Load a foreign global holding a value word (i64).
    fn foreign(&mut self, symbol: &str) -> IntValue<'ctx> {
        let global = *self
            .externs
            .entry(symbol.to_string())
            .or_insert_with(|| self.module.add_global(self.i64_t, None, symbol));
        self.builder
            .build_load(self.i64_t, global.as_pointer_value(), "f")
            .unwrap()
            .into_int_value()
    }

    /// Declare (once) an exported kernel builtin `symbol` taking `argc`
    /// value words and returning a value word, for a direct call.
    fn builtin_fn(&mut self, symbol: &str, argc: usize) -> FunctionValue<'ctx> {
        if let Some(f) = self.module.get_function(symbol) {
            return f;
        }
        let params = vec![self.i64_t.into(); argc];
        let fn_type = self.i64_t.fn_type(&params, false);
        self.module
            .add_function(symbol, fn_type, Some(Linkage::External))
    }

    fn i32c(&self, n: u32) -> IntValue<'ctx> {
        self.i32_t.const_int(n as u64, false)
    }

    // EXPRESSIONS
    //
    // Returns `None` when the expression diverged (tail call or crash) and
    // the current block is already terminated.

    fn gen(&mut self, expr: &Expr) -> Option<IntValue<'ctx>> {
        match expr {
            Expr::Int(n) => {
                Some(self.call_val("rt_int", &[self.i64_t.const_int(*n as u64, true).into()]))
            }
            Expr::Float(f) => {
                Some(self.call_val("rt_float", &[self.f64_t.const_float(*f).into()]))
            }
            Expr::Chr(c) => Some(self.call_val("rt_chr", &[self.i32c(*c as u32).into()])),
            Expr::Str(s) => {
                let (ptr, len) = self.string_data(s);
                Some(self.call_val(
                    "rt_str",
                    &[ptr.into(), self.i64_t.const_int(len, false).into()],
                ))
            }
            Expr::Bool(b) => {
                let name = if *b { "rt_true_v" } else { "rt_false_v" };
                Some(self.foreign(name))
            }
            Expr::Unit => Some(self.foreign("rt_unit_v")),
            Expr::Local(name) => match self.locals[name] {
                Binding::Reg(value) => Some(value),
                Binding::Slot(slot) => Some(
                    self.builder
                        .build_load(self.i64_t, slot, name)
                        .unwrap()
                        .into_int_value(),
                ),
            },
            Expr::GlobalValue(name) => Some(
                self.builder
                    .build_load(self.i64_t, self.globals[name].as_pointer_value(), "g")
                    .unwrap()
                    .into_int_value(),
            ),
            Expr::Foreign { module, name } => {
                let symbol = format!("${}${}", module.as_str().replace('.', "$"), name);
                Some(self.foreign(&symbol))
            }
            Expr::Closure { function, captures } => {
                let arity = self.arities[function] as u32;
                let caps = self.gen_all(captures)?;
                let array = self.value_array(&caps);
                let fn_ptr = self.functions[function].as_global_value().as_pointer_value();
                Some(self.call_val(
                    "rt_closure",
                    &[
                        fn_ptr.into(),
                        self.i32c(arity).into(),
                        self.i32c(caps.len() as u32).into(),
                        array.into(),
                    ],
                ))
            }
            Expr::CallDirect { function, args } => {
                let args = self.gen_all(args)?;
                let metas: Vec<BasicMetadataValueEnum> = args.iter().map(|a| (*a).into()).collect();
                Some(
                    self.builder
                        .build_call(self.functions[function], &metas, "d")
                        .unwrap()
                        .try_as_basic_value()
                        .left()
                        .unwrap()
                        .into_int_value(),
                )
            }
            Expr::CallClosure { function, args } => {
                let f = self.gen(function)?;
                let args = self.gen_all(args)?;
                let array = self.value_array(&args);
                Some(self.call_val(
                    "rt_apply",
                    &[f.into(), self.i32c(args.len() as u32).into(), array.into()],
                ))
            }
            Expr::CallBuiltin { symbol, args } => {
                let function = self.builtin_fn(symbol, args.len());
                let args = self.gen_all(args)?;
                let metas: Vec<BasicMetadataValueEnum> = args.iter().map(|a| (*a).into()).collect();
                Some(
                    self.builder
                        .build_call(function, &metas, "b")
                        .unwrap()
                        .try_as_basic_value()
                        .left()
                        .unwrap()
                        .into_int_value(),
                )
            }
            Expr::Prim { op, args } => self.prim(*op, args),
            Expr::Ctor { name, index, args } => {
                let name_ptr = self.cstring(name.as_str());
                let args = self.gen_all(args)?;
                let array = self.value_array(&args);
                Some(self.call_val(
                    "rt_ctor",
                    &[
                        name_ptr.into(),
                        self.i32c(*index).into(),
                        self.i32c(args.len() as u32).into(),
                        array.into(),
                    ],
                ))
            }
            Expr::List(items) => {
                let items = self.gen_all(items)?;
                let array = self.value_array(&items);
                Some(self.call_val(
                    "rt_list",
                    &[self.i32c(items.len() as u32).into(), array.into()],
                ))
            }
            Expr::Tuple(items) => {
                let items = self.gen_all(items)?;
                let array = self.value_array(&items);
                Some(self.call_val(
                    "rt_tuple",
                    &[self.i32c(items.len() as u32).into(), array.into()],
                ))
            }
            Expr::Record(fields) => {
                let record =
                    self.call_val("rt_record_new", &[self.i32c(fields.len() as u32).into()]);
                for (i, (name, value)) in fields.iter().enumerate() {
                    let v = self.gen(value)?;
                    let name_ptr = self.cstring(name.as_str());
                    self.call_void(
                        "rt_record_set",
                        &[record.into(), self.i32c(i as u32).into(), name_ptr.into(), v.into()],
                    );
                }
                Some(record)
            }
            Expr::Update { record, fields } => {
                let base = self.gen(record)?;
                let clone = self.call_val("rt_record_clone", &[base.into()]);
                for (name, value) in fields {
                    let v = self.gen(value)?;
                    let name_ptr = self.cstring(name.as_str());
                    self.call_void(
                        "rt_record_replace",
                        &[clone.into(), name_ptr.into(), v.into()],
                    );
                }
                Some(clone)
            }
            Expr::Access { record, field } => {
                let r = self.gen(record)?;
                let name_ptr = self.cstring(field.as_str());
                Some(self.call_val("rt_access", &[r.into(), name_ptr.into()]))
            }
            Expr::GetField { of, step } => {
                let v = self.gen(of)?;
                Some(match step {
                    Step::CtorArg(i) => {
                        self.call_val("rt_ctor_arg", &[v.into(), self.i32c(*i).into()])
                    }
                    Step::TupleField(i) => {
                        self.call_val("rt_tuple_item", &[v.into(), self.i32c(*i).into()])
                    }
                    Step::ListHead => self.call_val("rt_list_head", &[v.into()]),
                    Step::ListTail => self.call_val("rt_list_tail", &[v.into()]),
                })
            }
            Expr::Let { name, value, body } => {
                let v = self.gen(value)?;
                let saved = self.locals.insert(name.clone(), Binding::Reg(v));
                let result = self.gen(body);
                match saved {
                    Some(binding) => self.locals.insert(name.clone(), binding),
                    None => self.locals.remove(name),
                };
                result
            }
            Expr::LetRec { bindings, body } => {
                // Allocate every closure first (captures null), bind the
                // names, then patch the capture slots — they may reference
                // any of the bound names.
                let mut patches = Vec::new();
                for (name, value) in bindings {
                    match value {
                        Expr::Closure { function, captures } => {
                            let arity = self.arities[function] as u32;
                            let fn_ptr =
                                self.functions[function].as_global_value().as_pointer_value();
                            let closure = self.call_val(
                                "rt_closure",
                                &[
                                    fn_ptr.into(),
                                    self.i32c(arity).into(),
                                    self.i32c(captures.len() as u32).into(),
                                    self.ptr_t.const_null().into(),
                                ],
                            );
                            self.locals.insert(name.clone(), Binding::Reg(closure));
                            patches.push(Some((closure, captures.clone())));
                        }
                        other => {
                            let v = self.gen(other)?;
                            self.locals.insert(name.clone(), Binding::Reg(v));
                            patches.push(None);
                        }
                    }
                }
                for patch in patches {
                    if let Some((closure, captures)) = patch {
                        for (i, capture) in captures.iter().enumerate() {
                            let v = self.gen(capture)?;
                            self.call_void(
                                "rt_closure_set",
                                &[closure.into(), self.i32c(i as u32).into(), v.into()],
                            );
                        }
                    }
                }
                self.gen(body)
            }
            Expr::If {
                branches,
                otherwise,
            } => self.gen_if(branches, otherwise),
            Expr::Case {
                scrutinee,
                temp,
                branches,
            } => self.gen_case(scrutinee, temp, branches),
            Expr::TailCall { args } => {
                let args = self.gen_all(args)?;
                for (slot, value) in self.slots.clone().iter().zip(args) {
                    self.builder.build_store(*slot, value).unwrap();
                }
                self.builder
                    .build_unconditional_branch(self.loop_bb.unwrap())
                    .unwrap();
                None
            }
            Expr::Crash(message) => {
                let msg = self.cstring(message);
                self.call_void("rt_crash", &[msg.into()]);
                self.builder.build_unreachable().unwrap();
                None
            }
        }
    }

    fn gen_all(&mut self, exprs: &[Expr]) -> Option<Vec<IntValue<'ctx>>> {
        let mut out = Vec::with_capacity(exprs.len());
        for expr in exprs {
            out.push(self.gen(expr)?);
        }
        Some(out)
    }

    fn gen_if(
        &mut self,
        branches: &[(Expr, Expr)],
        otherwise: &Expr,
    ) -> Option<IntValue<'ctx>> {
        let slot = self.alloca("if");
        let merge = self.block("merge");
        let mut merged = false;

        for (condition, branch) in branches {
            let cond = self.gen(condition)?;
            let flag = self.call_bool("rt_is_true", &[cond.into()]);
            let then_bb = self.block("then");
            let else_bb = self.block("else");
            self.builder
                .build_conditional_branch(flag, then_bb, else_bb)
                .unwrap();

            self.builder.position_at_end(then_bb);
            if let Some(value) = self.gen(branch) {
                self.builder.build_store(slot, value).unwrap();
                self.builder.build_unconditional_branch(merge).unwrap();
                merged = true;
            }
            self.builder.position_at_end(else_bb);
        }

        if let Some(value) = self.gen(otherwise) {
            self.builder.build_store(slot, value).unwrap();
            self.builder.build_unconditional_branch(merge).unwrap();
            merged = true;
        }

        self.finish_merge(merge, slot, merged)
    }

    fn gen_case(
        &mut self,
        scrutinee: &Expr,
        temp: &str,
        branches: &[Branch],
    ) -> Option<IntValue<'ctx>> {
        let value = self.gen(scrutinee)?;
        self.locals.insert(temp.to_string(), Binding::Reg(value));

        let slot = self.alloca("case");
        let merge = self.block("merge");
        let mut merged = false;

        for branch in branches {
            let next = self.block("next");
            for (subject, test) in &branch.tests {
                let s = self.gen(subject)?;
                let flag = self.gen_test(s, test);
                let pass = self.block("pass");
                self.builder
                    .build_conditional_branch(flag, pass, next)
                    .unwrap();
                self.builder.position_at_end(pass);
            }

            let saved = self.locals.clone();
            for (name, path) in &branch.bindings {
                let bound = self.gen(path)?;
                self.locals.insert(name.clone(), Binding::Reg(bound));
            }
            if let Some(result) = self.gen(&branch.body) {
                self.builder.build_store(slot, result).unwrap();
                self.builder.build_unconditional_branch(merge).unwrap();
                merged = true;
            }
            self.locals = saved;
            self.builder.position_at_end(next);
        }

        // The lowered branch list always ends in a catch-all, so this
        // trailing fall-through is unreachable.
        self.builder.build_unreachable().unwrap();
        self.finish_merge(merge, slot, merged)
    }

    /// Wrap up an `if`/`case`: if any arm reached the merge block, load the
    /// result there; otherwise every arm diverged and the merge is dead.
    fn finish_merge(
        &mut self,
        merge: inkwell::basic_block::BasicBlock<'ctx>,
        slot: PointerValue<'ctx>,
        merged: bool,
    ) -> Option<IntValue<'ctx>> {
        self.builder.position_at_end(merge);
        if !merged {
            self.builder.build_unreachable().unwrap();
            return None;
        }
        Some(
            self.builder
                .build_load(self.i64_t, slot, "r")
                .unwrap()
                .into_int_value(),
        )
    }

    fn gen_test(&mut self, subject: IntValue<'ctx>, test: &Test) -> IntValue<'ctx> {
        match test {
            Test::IsCtor { index, .. } => {
                self.call_bool("rt_is_ctor", &[subject.into(), self.i32c(*index).into()])
            }
            Test::IsBool(b) => self.call_bool(
                "rt_is_bool",
                &[subject.into(), self.i1_t.const_int(*b as u64, false).into()],
            ),
            Test::IsInt(n) => self.call_bool(
                "rt_is_int",
                &[subject.into(), self.i64_t.const_int(*n as u64, true).into()],
            ),
            Test::IsChr(c) => {
                self.call_bool("rt_is_chr", &[subject.into(), self.i32c(*c as u32).into()])
            }
            Test::IsStr(s) => {
                let (ptr, len) = self.string_data(s);
                self.call_bool(
                    "rt_is_str",
                    &[subject.into(), ptr.into(), self.i64_t.const_int(len, false).into()],
                )
            }
            Test::IsCons => self.call_bool("rt_is_cons", &[subject.into()]),
            Test::IsNil => self.call_bool("rt_is_nil", &[subject.into()]),
        }
    }

    fn prim(&mut self, op: PrimOp, args: &[Expr]) -> Option<IntValue<'ctx>> {
        // `&&` and `||` short-circuit: the right side is only evaluated
        // when the left does not already decide the result.
        if matches!(op, PrimOp::And | PrimOp::Or) {
            let slot = self.alloca("bool");
            let left = self.gen(&args[0])?;
            let flag = self.call_bool("rt_is_true", &[left.into()]);
            let eval_right = self.block("right");
            let decided = self.block("decided");
            let merge = self.block("merge");
            let (on_true, on_false) = match op {
                PrimOp::And => (eval_right, decided),
                _ => (decided, eval_right),
            };
            self.builder
                .build_conditional_branch(flag, on_true, on_false)
                .unwrap();

            self.builder.position_at_end(decided);
            self.builder.build_store(slot, left).unwrap();
            self.builder.build_unconditional_branch(merge).unwrap();

            self.builder.position_at_end(eval_right);
            if let Some(right) = self.gen(&args[1]) {
                self.builder.build_store(slot, right).unwrap();
                self.builder.build_unconditional_branch(merge).unwrap();
            }

            self.builder.position_at_end(merge);
            return Some(
                self.builder
                    .build_load(self.i64_t, slot, "b")
                    .unwrap()
                    .into_int_value(),
            );
        }

        let name = match op {
            PrimOp::Add => "rt_add",
            PrimOp::Sub => "rt_sub",
            PrimOp::Mul => "rt_mul",
            PrimOp::FDiv => "rt_fdiv",
            PrimOp::IDiv => "rt_idiv",
            PrimOp::Pow => "rt_pow",
            PrimOp::Neg => "rt_neg",
            PrimOp::Eq => "rt_eq",
            PrimOp::NotEq => "rt_neq",
            PrimOp::Lt => "rt_lt",
            PrimOp::Le => "rt_le",
            PrimOp::Gt => "rt_gt",
            PrimOp::Ge => "rt_ge",
            PrimOp::Append => "rt_append",
            PrimOp::Cons => "rt_cons",
            PrimOp::And | PrimOp::Or => unreachable!(),
        };
        let args = self.gen_all(args)?;
        let metas: Vec<BasicMetadataValueEnum> = args.iter().map(|a| (*a).into()).collect();
        Some(self.call_val(name, &metas))
    }
}

/// Pair a function's parameter names with their incoming SSA values.
fn params_named<'ctx>(
    function: &Function,
    values: &[IntValue<'ctx>],
) -> Vec<(String, IntValue<'ctx>)> {
    function
        .params
        .iter()
        .cloned()
        .zip(values.iter().copied())
        .collect()
}
