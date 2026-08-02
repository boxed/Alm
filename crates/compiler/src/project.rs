//! Port of the `builder/` half of the Elm compiler (much simplified):
//! find the project, resolve imports to files, and compile every module
//! in dependency order into one JavaScript file.
//!
//! Resolution is *per package*: each module's imports are resolved against
//! that module's own package's dependency list, not a single flat namespace.
//! This mirrors Elm, where two different packages may each define a module
//! with the same name (e.g. both `elm-community/html-extra` and
//! `arowM/html-extra` expose `Html.Extra`). A flat namespace would merge them
//! or pick one arbitrarily, producing wrong resolutions and false import
//! cycles. See `resolve_scopes`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::ast::canonical as can;
use crate::ast::source as src;
use crate::data::Name;
use crate::cache;
use crate::interface::Interfaces;
use crate::reporting::{Located, Region, Report};
use crate::{builtins, canonicalize, generate, nitpick, optimize, parse, typecheck};

/// Wall-clock time per compile phase, printed to stderr when `ALM_TIMING=1`.
///
/// Off by default and costing one atomic load per phase when off. Compile
/// speed is a headline property of this compiler, and it regressed once
/// without anyone noticing; being able to ask where the time goes without
/// rebuilding is worth the few lines.
pub mod timing {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;

    macro_rules! phases {
        ($($field:ident => $label:literal),* $(,)?) => {
            #[derive(Default)]
            struct Phases { $($field: AtomicU64,)* }
            static PHASES: Phases = Phases { $($field: AtomicU64::new(0),)* };
            /// Print the accumulated totals and reset them.
            pub fn report() {
                if !enabled() { return; }
                let mut total = 0u64;
                $(total += PHASES.$field.load(Ordering::Relaxed);)*
                eprintln!("── alm timing ──");
                $(
                    let ns = PHASES.$field.swap(0, Ordering::Relaxed);
                    eprintln!(
                        "  {:<14} {:>7.1} ms  {:>4.1}%",
                        $label, ns as f64 / 1e6,
                        if total == 0 { 0.0 } else { ns as f64 * 100.0 / total as f64 }
                    );
                )*
                eprintln!("  {:<14} {:>7.1} ms", "total", total as f64 / 1e6);
            }
            $(
                pub fn $field<T>(f: impl FnOnce() -> T) -> T {
                    if !enabled() { return f(); }
                    let start = Instant::now();
                    let out = f();
                    PHASES.$field.fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
                    out
                }
            )*
        };
    }

    phases! {
        read => "read",
        parse => "parse",
        canonicalize => "canonicalize",
        typecheck => "typecheck",
        nitpick => "nitpick",
        lint => "lint",
        simplify => "simplify",
        interface => "interface",
        cache => "cache i/o",
        mono => "monomorphize",
        generate => "generate",
        dce => "tree-shake",
    }

    pub fn enabled() -> bool {
        static ON: AtomicU64 = AtomicU64::new(u64::MAX);
        let cached = ON.load(Ordering::Relaxed);
        if cached != u64::MAX {
            return cached == 1;
        }
        let on = std::env::var_os("ALM_TIMING").is_some();
        ON.store(on as u64, Ordering::Relaxed);
        on
    }
}

pub struct BuildError {
    pub path: PathBuf,
    pub source: String,
    pub reports: Vec<Report>,
    /// The module's declared name, known once the file has parsed. Used only
    /// by the band drawn between two modules' reports.
    pub module: Option<String>,
}

impl BuildError {
    fn new(path: PathBuf, source: String, title: &str, region: Region, message: String) -> Self {
        BuildError {
            path,
            source,
            reports: vec![Report {
                title: title.to_string(),
                region,
                message,
                elm: None,
            }],
            module: None,
        }
    }

    /// A build error carrying already-built reports (e.g. a byte-exact parse
    /// diagnostic from the syntax catalogue).
    fn from_reports(path: PathBuf, source: String, reports: Vec<Report>) -> Self {
        BuildError { path, source, reports, module: None }
    }

    /// A whole-build failure that quotes no source (and so belongs to no file).
    fn without_source(report: Report) -> Self {
        BuildError {
            path: PathBuf::new(),
            source: String::new(),
            reports: vec![report],
            module: None,
        }
    }

    fn in_module(mut self, name: &Name) -> Self {
        self.module = Some(name.to_string());
        self
    }

    /// Render every report in this module. `root` is the project directory, so
    /// the header names the file the way elm does — `src/Main.elm`, not an
    /// absolute path.
    pub fn render_from(&self, root: Option<&Path>, color: bool) -> String {
        let displayed = root
            .and_then(|root| self.path.strip_prefix(root).ok())
            .unwrap_or(&self.path)
            .display()
            .to_string();
        self.render_named(&displayed, color)
    }

    /// Render with an explicit name in the header bar, for source that is not
    /// a file anyone has: the REPL's accumulated module is shown as `REPL`,
    /// because `Elm_Repl.elm` is an implementation detail.
    pub fn render_named(&self, displayed: &str, color: bool) -> String {
        self.reports
            .iter()
            .map(|r| {
                if color {
                    r.render_ansi(displayed, &self.source)
                } else {
                    r.render(displayed, &self.source)
                }
            })
            .collect::<String>()
    }

    pub fn render(&self) -> String {
        self.render_from(None, false)
    }

    /// Whether this is a whole-build failure rather than a problem in some
    /// module: it quotes no source and belongs to no file.
    pub fn is_whole_build(&self) -> bool {
        self.path.as_os_str().is_empty()
    }

    /// The `--report=json` form of a whole-build failure. elm gives these a
    /// different envelope from module errors: the title and message sit at the
    /// top level and `path` is null.
    pub fn to_json_error(&self) -> String {
        let mut out = String::from("{\"type\":\"error\",\"path\":null,\"title\":");
        let report = &self.reports[0];
        crate::reporting::json_str(&report.title, &mut out);
        out.push_str(",\"message\":");
        out.push_str(&report.message_json(&self.source));
        out.push('}');
        out
    }

    /// This module's entry in a `--report=json` `"errors"` array. elm names
    /// the file by its *absolute* path here, unlike the human-readable report.
    pub fn to_json(&self) -> String {
        let mut out = String::new();
        out.push_str("{\"path\":");
        // Always absolute. Modules reached through the loader already are, but
        // one that failed to parse still carries the path as it was typed.
        let path = std::fs::canonicalize(&self.path).unwrap_or_else(|_| self.path.clone());
        crate::reporting::json_str(&path.display().to_string(), &mut out);
        out.push_str(",\"name\":");
        crate::reporting::json_str(&self.module_name(), &mut out);
        out.push_str(",\"problems\":[");
        for (i, report) in self.reports.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&report.to_json(&self.source));
        }
        out.push_str("]}");
        out
    }

    /// The module's name, as the band between two modules' reports shows it.
    pub fn module_name(&self) -> String {
        self.module.clone().unwrap_or_else(|| {
            self.path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
        })
    }
}

/// A parsed module on disk, keyed in the loader by its (unique) file path so
/// that two same-named modules from different packages stay distinct.
struct LoadedModule {
    path: PathBuf,
    /// The name declared in `module <Name> exposing ...`.
    declared_name: Name,
    /// Resolved user imports: the name as written plus the file it resolved
    /// to, within this module's package scope. Builtin/kernel imports are not
    /// listed here.
    imports: Vec<(Name, PathBuf)>,
    /// The source dir the file was found in, which decides where its own
    /// imports are searched for.
    matched_dir: PathBuf,
    /// Absent when the graph cache said the file was untouched: a module the
    /// build is going to reuse needs its name and its edges and nothing else,
    /// and not reading it is most of what makes an incremental build fast.
    /// `read` materializes it for the modules that do have to be recompiled.
    parsed: Option<Parsed>,
}

struct Parsed {
    source: String,
    module: src::Module,
}

impl LoadedModule {
    fn import_paths(&self) -> impl Iterator<Item = &PathBuf> {
        self.imports.iter().map(|(_, path)| path)
    }

    /// The source text, for an error or warning that has to quote it. Empty for
    /// a module that was never read — which is only reachable if the module
    /// neither failed nor warned, since both of those paths read it first.
    fn source(&self) -> &str {
        self.parsed.as_ref().map_or("", |p| p.source.as_str())
    }
}

/// Everything the front half of the compiler produces: the canonical
/// modules in dependency order plus their interfaces. Backends (JS today,
/// native later) consume this.
pub struct CheckedProject {
    pub modules: Vec<can::Module>,
    pub interfaces: Interfaces,
    /// Per-module, the concrete type of every expression keyed by source
    /// region (regions are only unique within a module).
    ///
    /// Monomorphization consumes this, and so does the JS backend: it is what
    /// lets a comparison on scalars inline to a native `<` instead of
    /// `_Utils_cmp`. Building it is the most expensive thing the checker does,
    /// but leaving it out makes every compiled program slower, so only callers
    /// that generate no code at all (`generate_docs`) ask to skip it.
    pub node_types: HashMap<Name, HashMap<Region, can::Type>>,
    /// Per-module, the inferred type of every top-level definition.
    pub types: HashMap<Name, HashMap<Name, can::Type>>,
    /// Per-module, its source file path and text — retained for source maps
    /// (`sources`/`sourcesContent`). Keyed by the module's resolved name.
    pub sources: HashMap<Name, (PathBuf, String)>,
    /// The entry module's name.
    pub entry: Name,
}

pub fn compile_project(entry: &Path) -> Result<(String, Vec<crate::lint::Warning>), Vec<BuildError>> {
    compile_project_with(entry, false)
}

/// Compile to JavaScript. `optimize` is elm's `--optimize`: it refuses to build
/// while any `Debug` call survives, because the optimizations it enables strip
/// out the information `Debug` reports (field names, constructor identities).
///
/// The two code-size optimizations themselves — shortening record field names
/// and turning constructor tags into ints — are **not** implemented. Both are
/// blocked on the same thing: alm's runtime is hand-written JavaScript that
/// reads Elm records and constructors directly (`impl.init`, `impl.update`,
/// `node.$ === 'VNode'`), so renaming them means rewriting the runtime to use
/// the renamed forms, as elm does by marking its kernel sources. Worth perhaps
/// 1% of bundle size before minification, against a real chance of a silently
/// broken bundle, and alm already tree-shakes to well under elm's output size.
/// The `Debug` rule is enforced regardless, so a project that builds with
/// `--optimize` under elm builds here too.
///
/// What `optimize` does change about the output is comments: the hand-written
/// kernel is commented as source should be, and none of that belongs in a
/// production bundle, so an optimized build is stripped of it (see
/// [`generate::comments`]).
pub fn compile_project_with(
    entry: &Path,
    optimize: bool,
) -> Result<(String, Vec<crate::lint::Warning>), Vec<BuildError>> {
    // Reuse what the last build compiled, module by module. `ALM_NO_CACHE=1`
    // forces the full path — the kill-switch should a stale-cache bug ever be
    // suspected in the field.
    if std::env::var_os("ALM_NO_CACHE").is_none() {
        compile_project_cached(entry, optimize, dce_wanted())
    } else {
        compile_project_uncached(entry, optimize)
    }
}

/// Tree-shake by default; `ALM_NO_DCE=1` emits the whole runtime kernel as a
/// field kill-switch should DCE ever drop something an app needs.
fn dce_wanted() -> bool {
    std::env::var_os("ALM_NO_DCE").is_none()
}

/// Compile to JavaScript from scratch, reading and reusing nothing. This is
/// what the incremental path has to agree with exactly, so it is callable on
/// its own rather than only reachable through an environment variable.
pub fn compile_project_uncached(
    entry: &Path,
    optimize: bool,
) -> Result<(String, Vec<crate::lint::Warning>), Vec<BuildError>> {
    let dce = dce_wanted();
    let checked = check_project_cached(entry, true, false)?;
    if optimize {
        let offenders = crate::debug_uses::modules_using_debug(&checked.modules);
        if !offenders.is_empty() {
            return Err(vec![BuildError::without_source(
                crate::debug_uses::debug_remnants_report(&offenders),
            )]);
        }
    }
    // Lint walks the already-checked AST — a cheap single traversal, not a
    // second front end — so `alm make` prints hints without re-type-checking.
    let warnings = timing::lint(|| crate::lint::lint(&checked.modules, &checked.sources));
    Ok((
        timing::generate(|| {
            let js = generate::generate_project_typed(&checked.modules, checked.node_types, dce);
            for_release(js, optimize)
        }),
        warnings,
    ))
}

/// The last thing that happens to an optimized bundle: out go the comments.
///
/// This runs on the assembled, already tree-shaken bundle rather than on the
/// kernel source, so it sees generated code too, and so the cached and full
/// build paths — which share nothing but the bundle they end up with — are
/// stripped by the same code at the same point.
fn for_release(javascript: String, optimize: bool) -> String {
    if optimize {
        generate::comments::strip(&javascript)
    } else {
        javascript
    }
}

/// What the live-reload server gets back from a build.
pub struct LiveBuild {
    pub javascript: String,
    /// The Source Map v3 for `javascript`, when the caller asked for one.
    pub source_map: Option<String>,
    /// A fingerprint of the program's `Model` type, when there is one.
    pub model_fingerprint: Option<String>,
    pub warnings: Vec<crate::lint::Warning>,
}

/// Compile for the live-reload server: the JavaScript, plus a fingerprint of
/// the program's `Model` type when there is one.
///
/// The fingerprint is what makes a hot swap safe to attempt. Carrying a
/// running model into a new build is only sound if the new build still means
/// the same thing by `Model`; comparing before and after is a cheap,
/// conservative way to know. When it cannot be determined the caller gets
/// `None` and should reload rather than guess.
pub fn compile_project_live(
    entry: &Path,
    optimize: bool,
    source_maps: bool,
) -> Result<LiveBuild, Vec<BuildError>> {
    let checked = check_project(entry)?;
    if optimize {
        let offenders = crate::debug_uses::modules_using_debug(&checked.modules);
        if !offenders.is_empty() {
            return Err(vec![BuildError::without_source(
                crate::debug_uses::debug_remnants_report(&offenders),
            )]);
        }
    }
    let model_fingerprint = model_type_of(&checked);
    let warnings = timing::lint(|| crate::lint::lint(&checked.modules, &checked.sources));
    // A mapped build keeps its comments: the map is built against the bundle as
    // generated, and stripping would move every line out from under it. Asking
    // for source maps is asking to read the output, so that is the right way
    // round.
    let (javascript, source_map) = if source_maps {
        let sources: HashMap<Name, (String, String)> = checked
            .sources
            .iter()
            .map(|(name, (path, src))| (name.clone(), (path.display().to_string(), src.clone())))
            .collect();
        let (js, map) =
            generate::generate_project_typed_mapped(&checked.modules, checked.node_types, &sources);
        (js, Some(map))
    } else {
        let dce = std::env::var_os("ALM_NO_DCE").is_none();
        let js = generate::generate_project_typed(&checked.modules, checked.node_types, dce);
        (for_release(js, optimize), None)
    };
    Ok(LiveBuild { javascript, source_map, model_fingerprint, warnings })
}

/// A fingerprint of the `model` in the entry module's
/// `main : Program flags model msg`.
///
/// The type is hashed rather than sent as written for two reasons: a real
/// application's `Model` renders to kilobytes, and it may contain characters
/// no HTTP header should carry (Elm allows non-ASCII field names, and header
/// values are Latin-1). A consumer only ever asks whether two builds agree, so
/// a fixed-width hex digest answers the question exactly as well.
fn model_type_of(checked: &CheckedProject) -> Option<String> {
    let main = checked.types.get(&checked.entry)?.get(&Name::from("main"))?;
    let can::Type::Type(_, name, args) = main else {
        return None;
    };
    // `Program flags model msg`. A `main` that is not a program (the reactor
    // will happily serve a `main : String`) has no model to preserve.
    if name.as_str() != "Program" || args.len() != 3 {
        return None;
    }
    Some(fingerprint(&crate::docs::render_type(&args[1])))
}

/// FNV-1a, 128-bit. Not a cryptographic hash and does not need to be: it
/// distinguishes two builds of one program minutes apart, where the width is
/// what rules out an accidental collision.
fn fingerprint(text: &str) -> String {
    const OFFSET: u128 = 0x6c62272e07bb014262b821756295c58d;
    const PRIME: u128 = 0x0000000001000000000000000000013b;
    let mut hash = OFFSET;
    for byte in text.as_bytes() {
        hash ^= *byte as u128;
        hash = hash.wrapping_mul(PRIME);
    }
    format!("{hash:032x}")
}

/// Generate a package's `docs.json` (elm's `--docs`). Returns the JSON, or the
/// build errors that stopped it.
pub fn generate_docs(entry: &Path) -> Result<String, Vec<BuildError>> {
    let root = project_root(entry);
    let elm_json = std::fs::read_to_string(root.join("elm.json")).unwrap_or_default();
    let exposed = exposed_modules(&elm_json);

    // Every exposed module is documented, not just the ones the entry file
    // happens to import — `elm make --docs` compiles the whole published API.
    // Each is checked from its own file and the results merged; a module
    // compiles the same whichever root reached it.
    let scopes = resolve_scopes(entry);
    let mut roots: Vec<PathBuf> = Vec::new();
    for name in &exposed {
        if let Some((path, _)) = find_module_file(name, &scopes.app_search) {
            roots.push(path);
        }
    }
    if roots.is_empty() {
        roots.push(entry.to_path_buf());
    }

    let mut modules: Vec<can::Module> = Vec::new();
    let mut interfaces: std::collections::BTreeMap<Name, crate::interface::Interface> =
        Default::default();
    let mut sources: std::collections::BTreeMap<Name, String> = Default::default();
    for root_path in &roots {
        // Docs need each definition's type, never a per-expression one, and
        // no code is generated here at all.
        let checked = check_project_with(root_path, false)?;
        for module in checked.modules {
            if !modules.iter().any(|m| m.name == module.name) {
                modules.push(module);
            }
        }
        for (name, interface) in checked.interfaces.iter() {
            interfaces.entry(name.clone()).or_insert_with(|| interface.clone());
        }
        for (name, (_, text)) in checked.sources {
            sources.entry(name).or_insert(text);
        }
    }

    let borrowed: std::collections::BTreeMap<Name, &crate::interface::Interface> =
        interfaces.iter().map(|(name, i)| (name.clone(), i)).collect();
    Ok(crate::docs::generate(&modules, &borrowed, &sources, &exposed))
}

/// Compile to JS with a Source Map v3. Returns `(javascript, source_map_json)`.
/// Tree-shaking runs as usual and the map is remapped onto the shaken bundle, so
/// the JS is the same size as an ordinary build. The caller writes the `.map`
/// file and appends the `//# sourceMappingURL` comment.
pub fn compile_project_source_maps(
    entry: &Path,
) -> Result<(String, String, Vec<crate::lint::Warning>), Vec<BuildError>> {
    let checked = check_project(entry)?;
    let sources: HashMap<Name, (String, String)> = checked
        .sources
        .iter()
        .map(|(name, (path, src))| {
            (name.clone(), (path.display().to_string(), src.clone()))
        })
        .collect();
    let warnings = timing::lint(|| crate::lint::lint(&checked.modules, &checked.sources));
    let (js, map) =
        generate::generate_project_typed_mapped(&checked.modules, checked.node_types, &sources);
    Ok((js, map, warnings))
}

/// Record the module graph as this build saw it: each file's timestamp, length,
/// declared name and resolved imports. Written every build, since a module that
/// was reloaded has a new timestamp to remember.
fn record_graph(
    dir: &Path,
    modules: &HashMap<PathBuf, LoadedModule>,
    order: &[PathBuf],
    previous: Option<&cache::Graph>,
) {
    cache::store_graph(
        dir,
        cache::fingerprint(&[]),
        &cache::Graph {
            modules: order
                .iter()
                .filter_map(|path| {
                    let loaded = &modules[path];
                    let (mtime_ns, size) = match &loaded.parsed {
                        // Read this build: take its stamp from disk now.
                        Some(_) => cache::stamp(path)?,
                        // Not read, so the stamp the graph vouched for still
                        // stands — it is why the file was not read.
                        None => {
                            let record = previous?.modules.get(path)?;
                            (record.mtime_ns, record.size)
                        }
                    };
                    Some((
                        path.clone(),
                        cache::GraphRecord {
                            mtime_ns,
                            size,
                            declared_name: loaded.declared_name.clone(),
                            matched_dir: loaded.matched_dir.clone(),
                            imports: loaded.imports.clone(),
                        },
                    ))
                })
                .collect(),
        },
    );
}

/// Whether this exact build has already been done and its output is still
/// there — every source unchanged since the graph was recorded, the same
/// compiler, the same target, the same output path.
///
/// Worth its own check because monomorphization and code generation are
/// whole-program: no per-module cache reaches them, so a save that changed
/// nothing costs a wasm-gc build a second of work to arrive at the file already
/// on disk. Costs ~360 `stat` calls on a 360-module project.
///
/// Deliberately conservative. Anything it cannot account for — a file it has no
/// record of, a missing output, source maps — falls through to the ordinary
/// build, which is only slower.
fn nothing_to_do(entry: &Path, output: &Path, target: &str) -> bool {
    if !output.is_file() {
        return false;
    }
    let dir = cache::dir_for(&project_root(entry));
    let Some(graph) = cache::load_graph(&dir, cache::fingerprint(&[])) else {
        return false;
    };
    let Some(closure) = cache::unchanged_closure(&graph, entry) else {
        return false;
    };
    let stamp = cache::build_stamp(
        &graph,
        &closure,
        cache::fingerprint(&[("target", target), ("output", &output.display().to_string())]),
    );
    let key = format!("{target}:{}", output.display());
    cache::load_build_stamp(&dir, &key) == Some(stamp)
}

/// Record that this build happened, so an identical one can be skipped.
fn built(entry: &Path, output: &Path, target: &str) {
    let dir = cache::dir_for(&project_root(entry));
    let Some(graph) = cache::load_graph(&dir, cache::fingerprint(&[])) else {
        return;
    };
    let Some(closure) = cache::unchanged_closure(&graph, entry) else {
        return;
    };
    let stamp = cache::build_stamp(
        &graph,
        &closure,
        cache::fingerprint(&[("target", target), ("output", &output.display().to_string())]),
    );
    cache::store_build_stamp(&dir, &format!("{target}:{}", output.display()), stamp);
}

/// Compile a project to a native binary or wasm module at `output` via the
/// LLVM backend.
pub fn compile_project_native(
    entry: &Path,
    output: &Path,
    opt: generate::native::OptLevel,
) -> Result<Vec<crate::lint::Warning>, Vec<BuildError>> {
    let use_cache = std::env::var_os("ALM_NO_CACHE").is_none();
    if use_cache && nothing_to_do(entry, output, "native") {
        return Ok(Vec::new());
    }
    // The native backend shares the front end, so it caches the same way.
    let checked = check_project_cached(entry, true, use_cache)?;
    let warnings = timing::lint(|| crate::lint::lint(&checked.modules, &checked.sources));
    let program = timing::mono(|| crate::ir::lower::lower_project(&checked.modules));
    timing::generate(|| generate::native::build(&program, output, opt))
        .map(|()| {
            if use_cache {
                built(entry, output, "native");
            }
            warnings
        })
        .map_err(|message| {
            vec![BuildError::new(
                entry.to_path_buf(),
                String::new(),
                "NATIVE BACKEND",
                Region::ZERO,
                message,
            )]
        })
}

/// Compile a project with the experimental WasmGC backend (see
/// `generate::wasmgc`). Shares the front end and monomorphizer (`ir::mono`)
/// with the other backends; only code generation differs.
pub fn compile_project_wasmgc(
    entry: &Path,
    output: &Path,
    source_maps: bool,
) -> Result<Vec<crate::lint::Warning>, Vec<BuildError>> {
    compile_project_wasmgc_with(entry, output, source_maps,
                                std::env::var_os("ALM_NO_CACHE").is_none())
}

/// As `compile_project_wasmgc`, with the front-end cache switchable — so the
/// differential tests can compare an incremental build against a real full one
/// without an environment variable, which tests sharing a process cannot do
/// safely.
pub fn compile_project_wasmgc_with(
    entry: &Path,
    output: &Path,
    source_maps: bool,
    use_cache: bool,
) -> Result<Vec<crate::lint::Warning>, Vec<BuildError>> {
    if use_cache && !source_maps && nothing_to_do(entry, output, "wasm-gc") {
        return Ok(Vec::new());
    }
    let checked = check_project_cached(entry, true, use_cache)?;
    let warnings = timing::lint(|| crate::lint::lint(&checked.modules, &checked.sources));
    let empty_types = HashMap::new();
    let empty_nodes = HashMap::new();
    // wasm-gc does NOT shunt any module to a native kernel: it has no
    // `deque_*` kernels, so shunting would only turn every
    // `Deque.*` into an unsupported-kernel error. Compile the module from source
    // instead — folkertdev/elm-deque is a regular type that monomorphizes fine
    // (robinheghan/elm-deque's non-regular finger-tree still can't, but it failed
    // here either way).
    let infos: Vec<crate::ir::mono::ModuleInfo> = checked
        .modules
        .iter()
        .map(|module| crate::ir::mono::ModuleInfo {
            name: module.name.clone(),
            module,
            types: checked.types.get(&module.name).unwrap_or(&empty_types),
            node_types: checked.node_types.get(&module.name).unwrap_or(&empty_nodes),
        })
        .collect();
    let mut program = timing::mono(|| crate::ir::mono::specialize_project(&infos, &checked.entry));
    if let Some(message) = &program.error {
        return Err(vec![BuildError::new(
            entry.to_path_buf(),
            String::new(),
            "NATIVE BACKEND LIMITATION",
            Region::ZERO,
            message.clone(),
        )]);
    }
    // Function inlining (post-mono). OFF by default: benchmarking showed it
    // regresses wasm-gc (defeats the scalar-unboxing ABI); opt in with
    // ALM_INLINE=1. See crate::ir::inline.
    crate::ir::inline::inline(&mut program);
    // Ports: name -> outgoing? (outgoing = `payload -> Cmd msg`). For outgoing
    // ports also record the payload type, so the WasmGC backend can convert the
    // `out payload` argument to a `Json.Value` before serializing it.
    let mut ports: HashMap<String, bool> = HashMap::new();
    let mut port_types: HashMap<String, can::Type> = HashMap::new();
    for module in &checked.modules {
        for port in &module.ports {
            let outgoing = matches!(
                &port.tipe,
                can::Type::Lambda(_, r)
                    if matches!(&**r, can::Type::Type(_, n, _) if n.as_str() == "Cmd")
            );
            if outgoing {
                if let can::Type::Lambda(payload, _) = &port.tipe {
                    port_types.insert(port.name.to_string(), (**payload).clone());
                }
            }
            ports.insert(port.name.to_string(), outgoing);
        }
    }
    // Constructor argument types, keyed by (home, union, ctor-index): lets the
    // WasmGC backend give a record sub-pattern in a ctor-arg position its type.
    let mut ctor_arg_types: HashMap<(String, String, u32), Vec<can::Type>> = HashMap::new();
    for module in &checked.modules {
        for union in &module.unions {
            for ctor in &union.ctors {
                ctor_arg_types.insert(
                    (module.name.to_string(), union.name.to_string(), ctor.index),
                    ctor.args.clone(),
                );
            }
        }
    }
    // Full union info, keyed by (home, union): the type variables (for arg-type
    // substitution) and each constructor's (name, tag/index, declared arg types).
    // Powers type-directed `Debug.toString` rendering of custom types.
    let mut unions: HashMap<(String, String), generate::wasmgc::UnionInfo> = HashMap::new();
    for module in &checked.modules {
        for union in &module.unions {
            let ctors = union
                .ctors
                .iter()
                .map(|c| (c.name.to_string(), c.index, c.args.clone()))
                .collect();
            unions.insert(
                (module.name.to_string(), union.name.to_string()),
                generate::wasmgc::UnionInfo {
                    vars: union.vars.clone(),
                    ctors,
                },
            );
        }
    }
    let sources: Option<HashMap<String, (String, String)>> = source_maps.then(|| {
        checked
            .sources
            .iter()
            .map(|(name, (path, src))| {
                (name.to_string(), (path.display().to_string(), src.clone()))
            })
            .collect()
    });
    timing::generate(|| {
        generate::wasmgc::build(
            &program,
            output,
            &ports,
            &port_types,
            &ctor_arg_types,
            &unions,
            sources.as_ref(),
        )
    })
    .map(|()| {
        // Only after the output is actually written: a stamp recorded for a
        // build that failed would skip the retry.
        if use_cache && !source_maps {
            built(entry, output, "wasm-gc");
        }
        warnings
    })
    .map_err(|message| {
        vec![BuildError::new(
            entry.to_path_buf(),
            String::new(),
            "WASMGC BACKEND",
            Region::ZERO,
            message,
        )]
    })
}

/// Compile to JavaScript, reusing what the last build already compiled.
///
/// The unit of reuse is a module: if its source is unchanged and every
/// interface it was checked against is unchanged, its JavaScript comes back
/// from `.alm-stuff` and it is not read, canonicalized, type checked or
/// generated at all. Everything is still *parsed* every build — that is how
/// the import graph is known, and it costs about 4% of a full build.
///
/// The result is byte-for-byte what a full build produces. That is the whole
/// contract, and `incremental_matches_full_build` in the test suite is what
/// holds it: a cache that is merely *nearly* right is worse than none, because
/// the difference shows up as a bug in the user's program.
pub fn compile_project_cached(
    entry: &Path,
    optimize: bool,
    dce: bool,
) -> Result<(String, Vec<crate::lint::Warning>), Vec<BuildError>> {
    let scopes = resolve_scopes(entry);
    let dir = cache::dir_for(&project_root(entry));
    // Anything that changes what a module compiles to, but is not the module's
    // own text or its dependencies' interfaces, belongs in the fingerprint.
    let fingerprint = cache::fingerprint(&[
        ("target", "js"),
        ("dce", if dce { "1" } else { "0" }),
        ("optimize", if optimize { "1" } else { "0" }),
    ]);

    // The graph cache lets an untouched file skip being read, parsed and
    // re-resolved. On a 360-module project that is most of what an incremental
    // build would otherwise spend its time on.
    let entries = cache::entries_dir(&dir, "js");
    let graph = timing::cache(|| cache::load_graph(&dir, cache::fingerprint(&[])));
    let Loaded { modules, order, unique_names, entry_key: _ } =
        load_and_order_with(entry, &scopes, graph.as_ref())?;

    let mut interfaces = Interfaces::new();
    // Interfaces read from the cache, still encoded. They are only needed to
    // check *other* modules against, so a build where nothing changed never
    // decodes one; the first module that has to be re-checked drains these.
    let mut pending: Vec<(Name, cache::LazyInterface)> = Vec::new();
    let mut interface_hashes: HashMap<Name, u64> = HashMap::new();
    let mut chunks: Vec<(Name, String, Vec<Name>)> = Vec::new();
    let mut warnings: Vec<crate::lint::Warning> = Vec::new();
    let mut debug_users: Vec<Name> = Vec::new();
    let mut build_errors: Vec<BuildError> = Vec::new();
    let mut failed: HashSet<PathBuf> = HashSet::new();
    let mut reused = 0usize;

    for path in &order {
        let source_module = &modules[path];
        if source_module.imports.iter().any(|(_, dep)| failed.contains(dep)) {
            failed.insert(path.clone());
            continue;
        }
        let name = unique_names[path].clone();

        // What this module was, or would be, compiled against. Sorted so the
        // comparison does not depend on the order imports happen to be listed.
        let mut deps: Vec<(Name, u64)> = source_module
            .imports
            .iter()
            .filter_map(|(_, dep)| {
                let dep_name = unique_names.get(dep)?;
                Some((dep_name.clone(), *interface_hashes.get(dep_name)?))
            })
            .collect();
        deps.sort();
        deps.dedup();
        let hit = timing::cache(|| cache::load(&entries, path, fingerprint));
        // A module the graph cache vouched for was never read, so there is no
        // text to hash: its timestamp and length already stood in for that.
        // Hashing every source every build was itself a measurable cost.
        let source_unchanged = match (&source_module.parsed, &hit) {
            (None, Some(_)) => true,
            (Some(parsed), Some(h)) => h.source_hash == cache::hash_str(&parsed.source),
            _ => false,
        };

        if let Some(hit) = hit {
            if source_unchanged && hit.deps == deps {
                pending.push((name.clone(), hit.interface));
                interface_hashes.insert(name.clone(), hit.interface_hash);
                chunks.push((name.clone(), hit.javascript, hit.exports));
                if hit.uses_debug {
                    debug_users.push(name.clone());
                }
                warnings.extend(hit.warnings.into_iter().map(|report| crate::lint::Warning {
                    path: source_module.path.clone(),
                    source: source_module.source().to_string(),
                    report,
                }));
                reused += 1;
                continue;
            }
        }

        // About to check a module, so the interfaces held back until now are
        // needed. A decode failure here means an entry that read cleanly does
        // not parse on second look, which should be impossible — fall back to a
        // full build rather than reason about a half-trusted cache.
        for (pending_name, lazy) in pending.drain(..) {
            match lazy.decode() {
                Some(interface) => {
                    interfaces.insert(pending_name, interface);
                }
                None => return compile_project_uncached(entry, optimize),
            }
        }

        // This module has to be recompiled. If the graph cache vouched for its
        // text it was never read — unchanged text is not unchanged *meaning*,
        // and a dependency moved under it — so read it now.
        let materialized;
        let source_module = match source_module.parsed {
            Some(_) => source_module,
            None => match read_and_parse(source_module, &scopes) {
                Ok(loaded) => {
                    materialized = loaded;
                    &materialized
                }
                Err(error) => {
                    build_errors.push(error.in_module(&name));
                    failed.insert(path.clone());
                    continue;
                }
            },
        };
        let source_hash = cache::hash_str(source_module.source());

        let Some((canonical, interface, node_types)) =
            check_one(source_module, &name, &unique_names, &interfaces, &mut build_errors)
        else {
            failed.insert(path.clone());
            continue;
        };

        let module_warnings = timing::lint(|| {
            crate::lint::lint(
                std::slice::from_ref(&canonical),
                &HashMap::from([(
                    name.clone(),
                    (source_module.path.clone(), source_module.source().to_string()),
                )]),
            )
        });
        let uses_debug = !crate::debug_uses::modules_using_debug(std::slice::from_ref(&canonical))
            .is_empty();
        let (javascript, exports) =
            timing::generate(|| generate::module_chunk(&canonical, node_types));

        let hash = cache::interface_hash(&interface);
        timing::cache(|| cache::store(
            &entries,
            path,
            fingerprint,
            &cache::Stored {
                source_hash,
                deps: &deps,
                interface_hash: hash,
                interface: &interface,
                javascript: &javascript,
                exports: &exports,
                uses_debug,
                warnings: &module_warnings.iter().map(|w| w.report.clone()).collect::<Vec<_>>(),
            },
        ));

        interfaces.insert(name.clone(), interface);
        interface_hashes.insert(name.clone(), hash);
        chunks.push((name.clone(), javascript, exports));
        warnings.extend(module_warnings);
        if uses_debug {
            debug_users.push(name);
        }
    }

    if !build_errors.is_empty() {
        return Err(build_errors);
    }
    if optimize && !debug_users.is_empty() {
        return Err(vec![BuildError::without_source(
            crate::debug_uses::debug_remnants_report(&debug_users),
        )]);
    }
    timing::cache(|| record_graph(&dir, &modules, &order, graph.as_ref()));

    if timing::enabled() {
        eprintln!("── alm cache ── {reused}/{} modules reused", order.len());
    }
    Ok((
        timing::generate(|| for_release(generate::assemble(&chunks, dce), optimize)),
        warnings,
    ))
}

/// Either a real problem with the project, or a signal that the cached graph
/// has moved on and the load should be redone without it.
enum LoadError {
    Build(BuildError),
    StaleGraph,
}

impl From<BuildError> for LoadError {
    fn from(error: BuildError) -> LoadError {
        LoadError::Build(error)
    }
}

/// Read and parse a module the graph cache described but did not load.
///
/// Its name and edges are already known and stay as they were: the graph only
/// vouches for a file whose timestamp and length match what it recorded, so the
/// text it holds is the text those edges came from.
fn read_and_parse(known: &LoadedModule, scopes: &Scopes) -> Result<LoadedModule, BuildError> {
    let source = timing::read(|| std::fs::read_to_string(&known.path)).map_err(|err| {
        BuildError::new(
            known.path.clone(),
            String::new(),
            "FILE PROBLEM",
            Region::ZERO,
            format!("I could not read {}: {}", known.path.display(), err),
        )
    })?;
    let module = timing::parse(|| parse::parse_module_typed(&source, scopes.is_package))
        .map_err(|e| match e.syntax {
            Some(se) => BuildError::from_reports(
                known.path.clone(),
                source.clone(),
                vec![se.to_report()],
            ),
            None => BuildError::new(
                known.path.clone(),
                source.clone(),
                "SYNTAX PROBLEM",
                e.region,
                e.message,
            ),
        })?;
    Ok(LoadedModule {
        path: known.path.clone(),
        declared_name: known.declared_name.clone(),
        imports: known.imports.clone(),
        matched_dir: known.matched_dir.clone(),
        parsed: Some(Parsed { source, module }),
    })
}

/// Canonicalize, type check and exhaustiveness check one module, returning
/// what the rest of the build needs from it. Errors are pushed onto
/// `build_errors` and `None` returned, matching `check_project_with`: a build
/// reports every module it could check rather than stopping at the first.
fn check_one(
    source_module: &LoadedModule,
    name: &Name,
    unique_names: &HashMap<PathBuf, Name>,
    interfaces: &Interfaces,
    build_errors: &mut Vec<BuildError>,
) -> Option<(can::Module, crate::interface::Interface, HashMap<Region, can::Type>)> {
    let rewritten = rewrite_module(source_module, unique_names);
    let (mut canonical, mut interface) =
        match timing::canonicalize(|| canonicalize::canonicalize_module(&rewritten, interfaces)) {
            Ok(pair) => pair,
            Err(errors) => {
                build_errors.extend(errors.into_iter().map(|e| {
                    BuildError::new(
                        source_module.path.clone(),
                        source_module.source().to_string(),
                        "NAMING PROBLEM",
                        e.region,
                        e.message,
                    )
                    .in_module(name)
                }));
                return None;
            }
        };

    let checked = match timing::typecheck(|| {
        typecheck::check_module_with(&canonical, interfaces, true)
    }) {
        Ok(checked) => checked,
        Err(errors) => {
            let reports = errors
                .into_iter()
                .map(|e| match e.report {
                    Some(report) => report,
                    None => crate::reporting::Report {
                        title: "TYPE MISMATCH".to_string(),
                        region: e.region,
                        message: e.message,
                        elm: None,
                    },
                })
                .collect::<Vec<_>>();
            build_errors.push(
                BuildError::from_reports(
                    source_module.path.clone(),
                    source_module.source().to_string(),
                    reports,
                )
                .in_module(name),
            );
            return None;
        }
    };

    if let Err(errors) = timing::nitpick(|| nitpick::check(&canonical, interfaces)) {
        build_errors.extend(errors.into_iter().map(|e| {
            BuildError::new(
                source_module.path.clone(),
                source_module.source().to_string(),
                "MISSING PATTERNS",
                e.region,
                e.message,
            )
            .in_module(name)
        }));
        return None;
    }

    timing::simplify(|| optimize::simplify_module(&mut canonical));

    for value in interface.value_names.clone() {
        if let Some(tipe) = checked.types.get(&value) {
            interface.values.insert(value, tipe.clone());
        }
    }
    for def in interface.binops.values_mut() {
        def.tipe = checked.types.get(&def.function).cloned();
    }
    Some((canonical, interface, checked.node_types))
}

/// Run the whole front end — load, parse, canonicalize, type check, and
/// exhaustiveness check every module — without generating any code.
pub fn check_project(entry: &Path) -> Result<CheckedProject, Vec<BuildError>> {
    check_project_with(entry, true)
}

/// `want_node_types` builds the per-expression type table monomorphization
/// consumes. Only the native and WasmGC backends read it; the JS backend does
/// not, and building it costs more than everything else in the front end put
/// together, so a JS build leaves it empty.
pub fn check_project_with(
    entry: &Path,
    want_node_types: bool,
) -> Result<CheckedProject, Vec<BuildError>> {
    check_project_cached(entry, want_node_types, std::env::var_os("ALM_NO_CACHE").is_none())
}

/// The front end, optionally reusing type-checker output for modules that have
/// not changed.
///
/// This is what makes the wasm-gc and native builds incremental. They cannot
/// reuse a module wholesale the way a JavaScript build does — monomorphization
/// is whole-program and reads every module's canonical AST — but type checking
/// is ~76% of a cold build and it is per module, so it is the half worth
/// keeping. The AST is rebuilt from source every time: parsing and
/// canonicalizing the whole project is ~74 ms against type checking's ~711 ms,
/// and not serializing it avoids the entire canonical AST as a format to
/// maintain.
///
/// `use_cache` is off for the reference path the differential tests compare
/// against, so "full build" means one.
pub fn check_project_cached(
    entry: &Path,
    want_node_types: bool,
    use_cache: bool,
) -> Result<CheckedProject, Vec<BuildError>> {
    let scopes = resolve_scopes(entry);
    // Only the code-generating paths ask for node types, and they are most of
    // what is being cached, so a build that does not want them (`--docs`) takes
    // the plain path.
    let use_cache = use_cache && want_node_types;
    let cache_dir = cache::entries_dir(&cache::dir_for(&project_root(entry)), "check");
    let fingerprint = cache::fingerprint(&[("target", "check")]);
    let mut interface_hashes: HashMap<Name, u64> = HashMap::new();
    let Loaded { modules, order, unique_names, entry_key } = load_and_order(entry, &scopes)?;

    // Compile each module against the interfaces of its dependencies.
    let mut interfaces = Interfaces::new();
    let mut canonical_modules = Vec::new();
    let mut all_node_types: HashMap<Name, HashMap<Region, can::Type>> = HashMap::new();
    let mut all_types: HashMap<Name, HashMap<Name, can::Type>> = HashMap::new();
    let mut all_sources: HashMap<Name, (PathBuf, String)> = HashMap::new();
    // Keep going after a module fails: elm reports every module it could
    // check, and only skips the ones whose imports never produced an interface.
    let mut build_errors: Vec<BuildError> = Vec::new();
    let mut failed: HashSet<PathBuf> = HashSet::new();
    for path in &order {
        let source_module = &modules[path];
        if source_module.imports.iter().any(|(_, dep)| failed.contains(dep)) {
            failed.insert(path.clone());
            continue;
        }
        let name = unique_names[path].clone();
        all_sources.insert(
            name.clone(),
            (source_module.path.clone(), source_module.source().to_string()),
        );
        // Rewrite the parsed module so its declared name and its imports point
        // at the resolved, unique names. Downstream code is unchanged.
        let rewritten = rewrite_module(source_module, &unique_names);

        let canonicalized = timing::canonicalize(|| canonicalize::canonicalize_module(&rewritten, &interfaces)).map_err(
            |errors| {
                errors
                    .into_iter()
                    .map(|e| {
                        BuildError::new(
                            source_module.path.clone(),
                            source_module.source().to_string(),
                            "NAMING PROBLEM",
                            e.region,
                            e.message,
                        )
                    })
                    .collect::<Vec<_>>()
            },
        );
        let (mut canonical, mut interface) = match canonicalized {
            Ok(pair) => pair,
            Err(errors) => {
                build_errors.extend(errors.into_iter().map(|e| e.in_module(&name)));
                failed.insert(path.clone());
                continue;
            }
        };

        // What this module was, or would be, checked against. Sorted, so the
        // comparison does not depend on the order imports are listed in.
        let mut deps: Vec<(Name, u64)> = source_module
            .imports
            .iter()
            .filter_map(|(_, dep)| {
                let dep_name = unique_names.get(dep)?;
                Some((dep_name.clone(), *interface_hashes.get(dep_name)?))
            })
            .collect();
        deps.sort();
        deps.dedup();
        let source_hash = cache::hash_str(source_module.source());

        let cached = use_cache
            .then(|| timing::cache(|| cache::load_check(&cache_dir, path, fingerprint)))
            .flatten()
            .filter(|hit| hit.source_hash == source_hash && hit.deps == deps);

        // A hit means this module and everything it was checked against are
        // unchanged, so type checking would reach the same answer — and so
        // would the exhaustiveness check, which is why that is skipped too.
        if let Some(hit) = cached {
            timing::simplify(|| optimize::simplify_module(&mut canonical));
            for value in interface.value_names.clone() {
                if let Some(tipe) = hit.types.get(&value) {
                    interface.values.insert(value, tipe.clone());
                }
            }
            for def in interface.binops.values_mut() {
                def.tipe = hit.types.get(&def.function).cloned();
            }
            interface_hashes.insert(name.clone(), hit.interface_hash);
            interfaces.insert(name.clone(), interface);
            all_node_types.insert(name.clone(), hit.node_types);
            all_types.insert(name.clone(), hit.types);
            canonical_modules.push(canonical);
            continue;
        }

        let type_checked = timing::typecheck(|| {
            typecheck::check_module_with(&canonical, &interfaces, want_node_types)
        })
        .map_err(|errors| {
            // All of a module's type errors belong to one report block.
            let reports = errors
                .into_iter()
                .map(|e| match e.report {
                    Some(report) => report,
                    None => crate::reporting::Report {
                        title: "TYPE MISMATCH".to_string(),
                        region: e.region,
                        message: e.message,
                        elm: None,
                    },
                })
                .collect::<Vec<_>>();
            vec![BuildError::from_reports(
                source_module.path.clone(),
                source_module.source().to_string(),
                reports,
            )]
        });
        let checked = match type_checked {
            Ok(checked) => checked,
            Err(errors) => {
                build_errors.extend(errors.into_iter().map(|e| e.in_module(&name)));
                failed.insert(path.clone());
                continue;
            }
        };
        let types = checked.types;
        all_node_types.insert(name.clone(), checked.node_types);

        let nitpicked = timing::nitpick(|| nitpick::check(&canonical, &interfaces)).map_err(|errors| {
            errors
                .into_iter()
                .map(|e| {
                    BuildError::new(
                        source_module.path.clone(),
                        source_module.source().to_string(),
                        "MISSING PATTERNS",
                        e.region,
                        e.message,
                    )
                })
                .collect::<Vec<_>>()
        });
        if let Err(errors) = nitpicked {
            build_errors.extend(errors.into_iter().map(|e| e.in_module(&name)));
            failed.insert(path.clone());
            continue;
        }

        // Local constant folding / simplification (after nitpick so it sees the
        // original patterns; before codegen so every backend benefits).
        timing::simplify(|| optimize::simplify_module(&mut canonical));

        for name in interface.value_names.clone() {
            if let Some(tipe) = types.get(&name) {
                interface.values.insert(name, tipe.clone());
            }
        }
        for def in interface.binops.values_mut() {
            def.tipe = types.get(&def.function).cloned();
        }
        if use_cache {
            let hash = cache::interface_hash(&interface);
            interface_hashes.insert(name.clone(), hash);
            timing::cache(|| {
                cache::store_check(
                    &cache_dir,
                    path,
                    fingerprint,
                    &cache::CheckStored {
                        source_hash,
                        deps: &deps,
                        interface_hash: hash,
                        types: &types,
                        node_types: &all_node_types[&name],
                    },
                )
            });
        }
        interfaces.insert(name.clone(), interface);
        all_types.insert(name.clone(), types);
        canonical_modules.push(canonical);
    }

    if !build_errors.is_empty() {
        return Err(build_errors);
    }
    // The back ends that build from this need the graph to tell whether
    // anything moved since last time. Every module here was read and parsed,
    // so the stamps are current.
    if use_cache {
        timing::cache(|| {
            record_graph(&cache::dir_for(&project_root(entry)), &modules, &order, None)
        });
    }

    Ok(CheckedProject {
        modules: canonical_modules,
        interfaces,
        node_types: all_node_types,
        types: all_types,
        sources: all_sources,
        entry: unique_names[&entry_key].clone(),
    })
}

/// Every module a build touches, parsed and put in dependency order. Shared by
/// the full and incremental paths so they can never disagree about what the
/// project *is* — only about how much of it needs recompiling.
struct Loaded {
    modules: HashMap<PathBuf, LoadedModule>,
    /// Dependencies first.
    order: Vec<PathBuf>,
    unique_names: HashMap<PathBuf, Name>,
    entry_key: PathBuf,
}

fn load_and_order(entry: &Path, scopes: &Scopes) -> Result<Loaded, Vec<BuildError>> {
    load_and_order_with(entry, scopes, None)
}

/// As `load_and_order`, but allowed to take a module's name and edges from the
/// graph cache instead of reading and parsing it. If the cached graph turns out
/// not to describe this project any more, the whole load is redone without it —
/// simpler than patching a graph that has moved, and it happens once.
fn load_and_order_with(
    entry: &Path,
    scopes: &Scopes,
    graph: Option<&cache::Graph>,
) -> Result<Loaded, Vec<BuildError>> {
    match load_and_order_inner(entry, scopes, graph) {
        Err(LoadError::StaleGraph) => load_and_order_inner(entry, scopes, None).map_err(unwrap_build),
        other => other.map_err(unwrap_build),
    }
}

fn unwrap_build(error: LoadError) -> Vec<BuildError> {
    match error {
        LoadError::Build(error) => vec![error],
        // Only produced with a graph in hand, and that load is retried without
        // one before this can be reached. Reported rather than dropped, because
        // an empty error list would fail the build while printing nothing.
        LoadError::StaleGraph => vec![BuildError::new(
            PathBuf::new(),
            String::new(),
            "BUILD CACHE PROBLEM",
            Region::ZERO,
            "The build cache described a project layout that no longer matches, and \
             reloading without it did not clear the condition. Delete .alm-stuff, or \
             set ALM_NO_CACHE=1."
                .to_string(),
        )],
    }
}

fn load_and_order_inner(
    entry: &Path,
    scopes: &Scopes,
    graph: Option<&cache::Graph>,
) -> Result<Loaded, LoadError> {
    // Load the entry module and, transitively, everything it imports. Modules
    // are keyed by file path so two same-named modules from different packages
    // do not clobber each other.
    let mut modules: HashMap<PathBuf, LoadedModule> = HashMap::new();
    let entry_key =
        load_module_file(entry, &scopes.app_search, scopes, &mut modules, graph)?;

    // The entry module's declared name must match its file path.
    if let Some(e) = entry_name_mismatch(&modules[&entry_key], &scopes.app_search) {
        return Err(e.into());
    }

    // Topologically sort (dependencies first), detecting import cycles.
    let order = sort_modules(&modules, &entry_key).map_err(|cycle| {
        let module = &modules[&cycle];
        LoadError::Build(BuildError::new(
            module.path.clone(),
            module.source().to_string(),
            "IMPORT CYCLE",
            Region::ZERO,
            format!(
                "The module `{}` is part of an import cycle. Elm does not allow cyclic imports.",
                module.declared_name
            ),
        ))
    })?;

    // Give every loaded file a unique module name. When a name is declared by
    // just one file (the overwhelmingly common case) that file keeps it. When
    // several files share a name they are disambiguated so every downstream
    // map (interfaces, canonical modules, types) can stay keyed by `Name`.
    let unique_names = assign_unique_names(&modules, &order);
    Ok(Loaded { modules, order, unique_names, entry_key })
}

/// The per-package search scopes for a project.
struct Scopes {
    /// Where to look for imports appearing in the app's own modules: the app's
    /// source directories plus the source dirs of its direct dependencies.
    app_search: Vec<PathBuf>,
    /// For every source directory we know about (each app source dir and each
    /// package `src`), the directories to search when resolving imports found
    /// in a module located there. This is what makes resolution per-package:
    /// a package's imports see only its own `src` plus its declared
    /// dependencies' `src` dirs.
    dir_search: HashMap<PathBuf, Vec<PathBuf>>,
    /// Extra source dirs used only when resolving a *bundled* effect module's
    /// imports (currently elm/bytes' `src`): the bundled `Http` module needs
    /// `Bytes`/`Bytes.Decode` even when the app itself does not declare
    /// elm/bytes. Empty if elm/bytes is not in the package cache.
    bundled_search: Vec<PathBuf>,
    /// Whether the project's `elm.json` declares `"type": "package"`. Packages
    /// may not declare ports.
    is_package: bool,
}

impl Scopes {
    /// Search dirs for imports appearing in a module found in `dir`.
    fn search_for(&self, dir: &Path) -> &[PathBuf] {
        self.dir_search
            .get(dir)
            .map(Vec::as_slice)
            .unwrap_or(&self.app_search)
    }
}

/// Walk up from the entry file looking for elm.json; fall back to treating
/// the entry file's directory as the only source directory. Package
/// dependencies listed in elm.json are resolved from the ELM_HOME cache so
/// pure Elm packages compile from their real sources, each scoped to its own
/// declared dependencies.
/// The directory holding the project's `elm.json`, which is what error headers
/// are shown relative to. Falls back to the current directory.
pub fn project_root(entry: &Path) -> PathBuf {
    let mut dir = canonical(
        &entry
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(".")),
    );
    loop {
        if dir.join("elm.json").is_file() {
            return dir;
        }
        match dir.parent() {
            Some(parent) => dir = parent.to_path_buf(),
            None => return std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        }
    }
}

fn resolve_scopes(entry: &Path) -> Scopes {
    // Canonicalize first. Module paths are canonicalized when loaded, so the
    // source directories have to be too or `strip_prefix` cannot line them up.
    // It also makes the walk terminate correctly for a bare `alm make Main.elm`:
    // `Path::parent` of a lone file name is `""`, not `None`, so an uncanonical
    // walk neither finds `elm.json` in the current directory's ancestors nor
    // stops — it settles on an empty source dir, against which every absolute
    // module path "matches", and the expected module name comes out as the
    // whole path with the separators turned into dots.
    let entry_dir = canonical(
        &entry
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(".")),
    );
    let mut dir = entry_dir.clone();
    loop {
        let elm_json = dir.join("elm.json");
        if elm_json.is_file() {
            if let Ok(contents) = std::fs::read_to_string(&elm_json) {
                return build_scopes(&dir, &contents);
            }
            return single_dir_scope(dir.join("src"));
        }
        match dir.parent() {
            Some(parent) => dir = parent.to_path_buf(),
            None => return single_dir_scope(entry_dir),
        }
    }
}

/// Resolve a directory to its canonical absolute form, leaving it as-is if it
/// does not exist (an unresolvable path still has to produce a report, not a
/// panic).
fn canonical(dir: &Path) -> PathBuf {
    std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf())
}

/// A project with no (readable) elm.json: one source directory, no packages.
fn single_dir_scope(source: PathBuf) -> Scopes {
    let source = canonical(&source);
    let app_search = vec![source.clone()];
    let mut dir_search = HashMap::new();
    dir_search.insert(source, app_search.clone());
    let bundled_search = register_bundled_deps(&mut dir_search);
    Scopes {
        app_search,
        dir_search,
        bundled_search,
        is_package: false,
    }
}

/// Register the search scope for bundled effect modules' non-builtin imports
/// (elm/bytes) so `Bytes.Decode` can find `Bytes`, and return the extra dirs to
/// search when resolving a bundled module's imports. elm/bytes depends only on
/// elm/core (a builtin), so its own scope is just its `src`.
fn register_bundled_deps(dir_search: &mut HashMap<PathBuf, Vec<PathBuf>>) -> Vec<PathBuf> {
    match elm_bytes_src() {
        Some(src) => {
            dir_search.insert(src.clone(), vec![src.clone()]);
            vec![src]
        }
        None => Vec::new(),
    }
}

/// The modules a package `elm.json` lists under `"exposed-modules"`. elm
/// allows either a flat array or an object grouping them under headings; both
/// shapes reduce to the same set of names.
pub fn exposed_modules(elm_json: &str) -> Vec<Name> {
    let Some(i) = elm_json.find("\"exposed-modules\"") else {
        return Vec::new();
    };
    let rest = &elm_json[i + "\"exposed-modules\"".len()..];
    let Some(open) = rest.find(['[', '{']) else {
        return Vec::new();
    };
    let closer = if rest.as_bytes()[open] == b'[' { b']' } else { b'}' };
    let mut depth = 0i32;
    let mut end = open;
    for (k, byte) in rest.as_bytes()[open..].iter().enumerate() {
        match byte {
            b'[' | b'{' => depth += 1,
            b']' | b'}' => {
                depth -= 1;
                if depth == 0 {
                    end = open + k;
                    break;
                }
            }
            _ => {}
        }
    }
    let _ = closer;
    quoted_strings(&rest[open..=end])
        // In the grouped shape the headings are quoted too, but a heading is
        // never a module name: module names start with an upper-case letter
        // and contain only identifier characters and dots.
        .filter(|s| {
            s.starts_with(char::is_uppercase)
                && s.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '.')
        })
        .map(Name::from)
        .collect()
}

/// Whether an `elm.json` declares `"type": "package"`.
fn is_package_project(elm_json: &str) -> bool {
    if let Some(i) = elm_json.find("\"type\"") {
        if let Some(colon) = elm_json[i..].find(':') {
            let rest = elm_json[i + colon + 1..].trim_start();
            return rest.starts_with("\"package\"");
        }
    }
    false
}

/// Build the per-package search scopes from a project's elm.json.
fn build_scopes(project_dir: &Path, elm_json: &str) -> Scopes {
    // The app's own source directories.
    let source_names = parse_source_directories(elm_json);
    let app_source_dirs: Vec<PathBuf> = if source_names.is_empty() {
        vec![canonical(&project_dir.join("src"))]
    } else {
        source_names.iter().map(|d| canonical(&project_dir.join(d))).collect()
    };

    // Every installed package and its `src` dir, keyed by "author/name". The
    // exact versions come from the pinned application elm.json.
    let installed = installed_packages(elm_json);

    // The app resolves imports against its *direct* dependencies (like Elm).
    // If we cannot identify the direct set, fall back to every installed
    // package so we never regress a project that used to compile.
    let direct = direct_dependency_names(elm_json)
        .unwrap_or_else(|| installed.keys().cloned().collect());

    let mut app_search = app_source_dirs.clone();
    for name in &direct {
        if let Some(src) = installed.get(name) {
            app_search.push(src.clone());
        }
    }

    let mut dir_search: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    for dir in &app_source_dirs {
        dir_search.insert(dir.clone(), app_search.clone());
    }
    // Each package's imports see its own src plus its declared dependencies'.
    for (_, src) in &installed {
        let mut search = vec![src.clone()];
        for dep in package_dependency_names(src) {
            if let Some(dep_src) = installed.get(&dep) {
                search.push(dep_src.clone());
            }
        }
        dir_search.insert(src.clone(), search);
    }

    let bundled_search = register_bundled_deps(&mut dir_search);

    Scopes {
        app_search,
        dir_search,
        bundled_search,
        is_package: is_package_project(elm_json),
    }
}

/// The ELM_HOME packages directory (~/.elm/0.19.1/packages).
fn packages_root() -> PathBuf {
    let home = std::env::var("ELM_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let user = std::env::var("HOME").unwrap_or_default();
            PathBuf::from(user).join(".elm")
        });
    home.join("0.19.1").join("packages")
}

/// Map every `"author/name": "1.2.3"` pinned in elm.json to its `src` dir on
/// disk, plus — for a package outline, which pins nothing — the versions its
/// ranges resolve to in the local cache.
fn installed_packages(elm_json: &str) -> HashMap<String, PathBuf> {
    let packages = packages_root();
    let mut installed = HashMap::new();
    for (key, version) in quoted_pairs(elm_json) {
        if !key.contains('/') {
            continue;
        }
        if !version.chars().all(|c| c.is_ascii_digit() || c == '.') || version.is_empty() {
            continue;
        }
        let (author, name) = key.split_once('/').unwrap();
        let src = packages.join(author).join(name).join(version).join("src");
        if src.is_dir() {
            installed.insert(key.to_string(), src);
        }
    }

    // A package elm.json states ranges rather than versions: which version to
    // build against is settled when something depends on it. Compiling the
    // package itself — for `alm make`, `--docs` or `alm diff` — has to settle
    // them here instead, by the same resolution `alm install` uses and against
    // the same local cache.
    if is_package_project(elm_json) {
        if let Some(deps) = object_block(elm_json, "dependencies") {
            let roots: std::collections::BTreeMap<String, crate::packages::Constraint> =
                crate::packages::pairs(deps)
                    .into_iter()
                    .filter_map(|(name, range)| {
                        Some((name.to_string(), crate::packages::Constraint::parse(range)?))
                    })
                    .collect();
            if let Ok(solution) = crate::packages::solve(&roots) {
                for (key, version) in solution {
                    let Some((author, name)) = key.split_once('/') else { continue };
                    let src =
                        packages.join(author).join(name).join(version.to_string()).join("src");
                    if src.is_dir() {
                        installed.insert(key.clone(), src);
                    }
                }
            }
        }
    }
    installed
}

/// The `"author/name"` keys a project imports directly: `dependencies.direct`
/// for an application, and all of `dependencies` for a package, where every
/// listed dependency is one the package's own modules may import. Returns
/// `None` when there is no dependencies block at all.
fn direct_dependency_names(elm_json: &str) -> Option<Vec<String>> {
    let deps = object_block(elm_json, "dependencies")?;
    let listed = object_block(deps, "direct").unwrap_or(deps);
    Some(
        quoted_strings(listed)
            .filter(|s| s.contains('/'))
            .map(str::to_string)
            .collect(),
    )
}

/// The dependency package names of the package whose sources live in `src`
/// (read from `<pkg>/elm.json`). A package elm.json lists dependencies as
/// `"author/name": "<version range>"`.
fn package_dependency_names(src: &Path) -> Vec<String> {
    let Some(pkg_dir) = src.parent() else {
        return Vec::new();
    };
    let Ok(contents) = std::fs::read_to_string(pkg_dir.join("elm.json")) else {
        return Vec::new();
    };
    match object_block(&contents, "dependencies") {
        Some(deps) => quoted_strings(deps)
            .filter(|s| s.contains('/'))
            .map(str::to_string)
            .collect(),
        None => Vec::new(),
    }
}

/// Slice out the `{ ... }` object that follows `"key"` in `json`, matching
/// braces so nested objects are included.
fn object_block<'a>(json: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("\"{}\"", key);
    let key_pos = json.find(&needle)?;
    let rest = &json[key_pos..];
    let open = rest.find('{')?;
    let bytes = rest.as_bytes();
    let mut depth = 0i32;
    let mut i = open;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&rest[open..=i]);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Every double-quoted string in `json`, in order.
fn quoted_strings(json: &str) -> impl Iterator<Item = &str> {
    let mut i = 0;
    std::iter::from_fn(move || {
        let quote = json[i..].find('"')?;
        let start = i + quote + 1;
        let end_rel = json[start..].find('"')?;
        i = start + end_rel + 1;
        Some(&json[start..start + end_rel])
    })
}

/// Every `"a": "b"` string→string pair in `json`, as (a, b).
fn quoted_pairs(json: &str) -> Vec<(&str, &str)> {
    let bytes = json.as_bytes();
    let mut pairs = Vec::new();
    let mut i = 0;
    while let Some(quote) = json[i..].find('"') {
        let start = i + quote + 1;
        let Some(end_rel) = json[start..].find('"') else {
            break;
        };
        let key = &json[start..start + end_rel];
        i = start + end_rel + 1;
        // Value: skip whitespace and a colon, then expect a quoted string.
        let mut j = i;
        while j < bytes.len() && (bytes[j] == b':' || bytes[j].is_ascii_whitespace()) {
            j += 1;
        }
        if j >= bytes.len() || bytes[j] != b'"' {
            continue;
        }
        let vstart = j + 1;
        let Some(vend_rel) = json[vstart..].find('"') else {
            break;
        };
        let value = &json[vstart..vstart + vend_rel];
        pairs.push((key, value));
        i = vstart + vend_rel + 1;
    }
    pairs
}

/// Extract `"source-directories": [ ... ]` from elm.json without a JSON
/// dependency.
fn parse_source_directories(json: &str) -> Vec<String> {
    let Some(key_pos) = json.find("\"source-directories\"") else {
        return vec![];
    };
    let rest = &json[key_pos..];
    let Some(open) = rest.find('[') else { return vec![] };
    let Some(close) = rest[open..].find(']') else {
        return vec![];
    };
    let array = &rest[open + 1..open + close];
    array
        .split(',')
        .filter_map(|item| {
            let item = item.trim();
            item.strip_prefix('"')?.strip_suffix('"').map(str::to_string)
        })
        .collect()
}

fn user_imports(module: &src::Module) -> Vec<Name> {
    module
        .imports
        .iter()
        .filter(|i| {
            let name = i.name.value.as_str();
            !builtins::is_builtin_module(name) && !name.starts_with("Elm.Kernel.")
        })
        .map(|i| i.name.value.clone())
        .collect()
}

/// Effect-module packages (Time/Random/Http) that alm compiles from a thin
/// source bundled with the compiler rather than from a builtin shim or the
/// `~/.elm` cache. Their pure helpers delegate to `Elm.Kernel.*` intrinsics; the
/// effect part is a real `effect module` manager driven by `_Platform`.
fn bundled_source(name: &str) -> Option<&'static str> {
    match name {
        "Time" => Some(include_str!("builtin_src/Time.elm")),
        "Random" => Some(include_str!("builtin_src/Random.elm")),
        "Http" => Some(include_str!("builtin_src/Http.elm")),
        _ => None,
    }
}

/// The `src` directory of the newest `elm/bytes` in the package cache, if
/// installed. The bundled `Http` effect module imports `Bytes`/`Bytes.Decode`
/// (for `bytesBody`/`expectBytes`), which are the elm/bytes package — NOT
/// builtins. So that `Http.get` compiles in an app that declares only
/// elm/core + elm/http, bundled modules resolve elm/bytes from the cache
/// unconditionally rather than from the app's declared dependencies.
fn elm_bytes_src() -> Option<PathBuf> {
    let dir = packages_root().join("elm").join("bytes");
    let mut versions: Vec<PathBuf> = std::fs::read_dir(&dir)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.join("src").is_dir())
        .collect();
    versions.sort();
    versions.pop().map(|p| p.join("src"))
}

/// The module name behind a bundled module's synthetic key, if that is what this
/// path is: `<builtin>/Http.elm` -> `Http`.
fn bundled_key(path: &Path) -> Option<Name> {
    let text = path.to_str()?.strip_prefix("<builtin>/")?.strip_suffix(".elm")?;
    Some(Name::from(text.replace(['/', '\\'], ".")))
}

/// Load a bundled effect-module source into the module graph under a synthetic
/// key, recursing into any other bundled modules it imports (e.g. Random → Time).
fn load_bundled_module(
    name: &Name,
    source: &'static str,
    scopes: &Scopes,
    modules: &mut HashMap<PathBuf, LoadedModule>,
    graph: Option<&cache::Graph>,
) -> Result<PathBuf, LoadError> {
    let key = PathBuf::from(format!("<builtin>/{}.elm", name.as_str().replace('.', "/")));
    if modules.contains_key(&key) {
        return Ok(key);
    }
    let module = parse::parse_module_typed(source, true).map_err(|e| match e.syntax {
        Some(se) => BuildError::from_reports(key.clone(), source.to_string(), vec![se.to_report()]),
        None => BuildError::new(key.clone(), source.to_string(), "SYNTAX PROBLEM", e.region, e.message),
    })?;
    let declared_name = module.get_name();
    let import_names = user_imports(&module);
    modules.insert(
        key.clone(),
        LoadedModule {
            path: key.clone(),
            declared_name,
            imports: Vec::new(),
            matched_dir: PathBuf::new(),
            parsed: Some(Parsed { source: source.to_string(), module }),
        },
    );
    // A bundled module's non-builtin imports resolve against the app's search
    // dirs plus the bundled-dependency dirs (elm/bytes), so `Http` finds
    // `Bytes`/`Bytes.Decode` even when the app never declared elm/bytes.
    let bundled_import_search: Vec<PathBuf> = scopes
        .app_search
        .iter()
        .chain(scopes.bundled_search.iter())
        .cloned()
        .collect();
    let mut resolved: Vec<(Name, PathBuf)> = Vec::new();
    for import in import_names {
        if let Some(bsrc) = bundled_source(import.as_str()) {
            let child = load_bundled_module(&import, bsrc, scopes, modules, graph)?;
            resolved.push((import, child));
        } else {
            let (import_path, matched_dir) =
                find_module_file(&import, &bundled_import_search).ok_or_else(|| {
                    BuildError::new(
                        key.clone(),
                        source.to_string(),
                        "MODULE NOT FOUND",
                        Region::ZERO,
                        format!("The bundled `{name}` module imports `{import}`, but I cannot find it."),
                    )
                })?;
            let child_search = scopes.search_for(&matched_dir).to_vec();
            let child = load_module_file(&import_path, &child_search, scopes, modules, graph)?;
            resolved.push((import, child));
        }
    }
    modules.get_mut(&key).unwrap().imports = resolved;
    Ok(key)
}

/// Parse a module file and recursively load everything it imports, resolving
/// each import within `search_dirs` (this module's package scope). Returns the
/// module's canonical file-path key.
fn load_module_file(
    path: &Path,
    search_dirs: &[PathBuf],
    scopes: &Scopes,
    modules: &mut HashMap<PathBuf, LoadedModule>,
    graph: Option<&cache::Graph>,
) -> Result<PathBuf, LoadError> {
    let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if modules.contains_key(&key) {
        return Ok(key);
    }

    // The fast path: the graph cache described this file and its timestamp and
    // length still match, so its name and edges stand and it is not read or
    // parsed at all. Anything that does not line up — a recorded import missing
    // from the graph — abandons the cache and reloads the project properly,
    // rather than trying to patch a graph that has moved.
    if let Some(record) = graph.and_then(|g| g.unchanged(&key)) {
        let imports = record.imports.clone();
        modules.insert(
            key.clone(),
            LoadedModule {
                path: key.clone(),
                declared_name: record.declared_name.clone(),
                imports: imports.clone(),
                matched_dir: record.matched_dir.clone(),
                parsed: None,
            },
        );
        for (import, child) in &imports {
            let graph = graph.expect("checked above");
            // A bundled effect module has no file on disk, so it is never in the
            // graph — its source is compiled into alm, which the fingerprint
            // already covers. Load it the ordinary way.
            if let Some(source) = bundled_key(child).and_then(|n| bundled_source(n.as_str())) {
                load_bundled_module(import, source, scopes, modules, Some(graph))?;
                continue;
            }
            let child_search = match graph.modules.get(child) {
                Some(child_record) => scopes.search_for(&child_record.matched_dir).to_vec(),
                None => return Err(LoadError::StaleGraph),
            };
            load_module_file(child, &child_search, scopes, modules, Some(graph))?;
        }
        return Ok(key);
    }

    let read = timing::read(|| std::fs::read_to_string(path));
    // A file the cached graph pointed at that is no longer there means the graph
    // has moved on — the module was renamed, deleted, or now lives in another
    // source directory — not that the project is broken. Abandon the cache and
    // reload, which either resolves the import somewhere else or reports it
    // missing, exactly as a full build would. Checking every recorded edge up
    // front would cost a stat per import, which is the cost this exists to
    // avoid; noticing on the read costs nothing until it actually happens.
    if read.is_err() && graph.is_some() {
        return Err(LoadError::StaleGraph);
    }
    let source = read.map_err(|err| {
        BuildError::new(
            path.to_path_buf(),
            String::new(),
            "FILE PROBLEM",
            Region::ZERO,
            format!("I could not read {}: {}", path.display(), err),
        )
    })?;

    let module = timing::parse(|| parse::parse_module_typed(&source, scopes.is_package)).map_err(|e| match e.syntax {
        Some(se) => {
            BuildError::from_reports(path.to_path_buf(), source.clone(), vec![se.to_report()])
        }
        None => BuildError::new(
            path.to_path_buf(),
            source.clone(),
            "SYNTAX PROBLEM",
            e.region,
            e.message,
        ),
    })?;

    let declared_name = module.get_name();
    let import_names = user_imports(&module);

    // Insert a placeholder before recursing so an import cycle terminates
    // (a module already present is not reloaded).
    let matched_dir = search_dirs.first().cloned().unwrap_or_default();
    modules.insert(
        key.clone(),
        LoadedModule {
            path: key.clone(),
            declared_name: declared_name.clone(),
            imports: Vec::new(),
            matched_dir,
            parsed: Some(Parsed { source: source.clone(), module }),
        },
    );

    let mut resolved: Vec<(Name, PathBuf)> = Vec::new();
    for import in import_names {
        if let Some(bsrc) = bundled_source(import.as_str()) {
            let child_key = load_bundled_module(&import, bsrc, scopes, modules, graph)?;
            resolved.push((import, child_key));
            continue;
        }
        let (import_path, matched_dir) =
            find_module_file(&import, search_dirs).ok_or_else(|| {
                BuildError::new(
                    path.to_path_buf(),
                    source.clone(),
                    "MODULE NOT FOUND",
                    Region::ZERO,
                    format!(
                        "The `{}` module imports `{}`, but I cannot find it. I looked for {} in: {}",
                        declared_name,
                        import,
                        module_file_name(&import),
                        search_dirs
                            .iter()
                            .map(|d| d.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                )
            })?;
        let child_search = scopes.search_for(&matched_dir).to_vec();
        let child_key = load_module_file(&import_path, &child_search, scopes, modules, graph)?;
        modules.get_mut(&key).unwrap().matched_dir = search_dirs.first().cloned().unwrap_or_default();
        let found_name = modules[&child_key].declared_name.clone();
        if found_name != import {
            return Err(LoadError::Build(BuildError::new(
                import_path.clone(),
                modules[&child_key].source().to_string(),
                "MODULE NAME MISMATCH",
                Region::ZERO,
                format!(
                    "This file is named {} so I expected it to declare `module {}`, but it declares `module {}`.",
                    import_path.display(),
                    import,
                    found_name
                ),
            )));
        }
        resolved.push((import, child_key));
    }

    modules.get_mut(&key).unwrap().imports = resolved;
    Ok(key)
}

fn module_file_name(name: &Name) -> String {
    format!("{}.elm", name.as_str().replace('.', "/"))
}

/// Find `name` among `search_dirs`, returning the file and the source dir it
/// was found in (so the caller can recurse with that dir's package scope).
/// A MODULE NAME MISMATCH build error if the module's declared name differs from
/// the name implied by its file path. The expected name is derived only when the
/// path cleanly relativizes against a source directory; otherwise the check is
/// skipped (so an unusual path form never produces a false positive).
fn entry_name_mismatch(m: &LoadedModule, search_dirs: &[PathBuf]) -> Option<BuildError> {
    let name = m.parsed.as_ref()?.module.name.as_ref()?;
    let expected = search_dirs.iter().find_map(|d| {
        let rel = m.path.strip_prefix(d).ok()?;
        rel.to_str()?.strip_suffix(".elm").map(|s| s.replace(['/', '\\'], "."))
    })?;
    if name.value.as_str() == expected {
        return None;
    }
    let report = crate::reporting::syntax::SyntaxError::ModuleNameMismatch {
        region: name.region,
        expected,
        actual: name.value.as_str().to_string(),
    }
    .to_report();
    Some(BuildError::from_reports(
        m.path.clone(),
        m.source().to_string(),
        vec![report],
    ))
}

fn find_module_file(name: &Name, search_dirs: &[PathBuf]) -> Option<(PathBuf, PathBuf)> {
    let relative = module_file_name(name);
    for dir in search_dirs {
        let path = dir.join(&relative);
        if path.is_file() {
            return Some((path, dir.clone()));
        }
    }
    None
}

/// Assign each loaded file a unique module name. Files whose declared name is
/// unique keep it; genuine duplicates (same name, different package) are given
/// a distinct internal name so downstream `Name`-keyed maps do not collide.
fn assign_unique_names(
    modules: &HashMap<PathBuf, LoadedModule>,
    order: &[PathBuf],
) -> HashMap<PathBuf, Name> {
    // Group paths by declared name (only `order` — the reachable modules).
    let mut by_name: HashMap<Name, Vec<PathBuf>> = HashMap::new();
    for path in order {
        by_name
            .entry(modules[path].declared_name.clone())
            .or_default()
            .push(path.clone());
    }

    let mut used: HashSet<Name> = by_name.keys().cloned().collect();
    let mut names: HashMap<PathBuf, Name> = HashMap::new();
    for (declared, mut paths) in by_name {
        if paths.len() == 1 {
            names.insert(paths.pop().unwrap(), declared);
            continue;
        }
        // Deterministic: the lexicographically first file keeps the bare name.
        paths.sort();
        let mut counter = 0;
        for (i, path) in paths.into_iter().enumerate() {
            if i == 0 {
                names.insert(path, declared.clone());
                continue;
            }
            let name = loop {
                counter += 1;
                let candidate = Name::from(format!("{}_alm{}", declared, counter));
                if !used.contains(&candidate) {
                    break candidate;
                }
            };
            used.insert(name.clone());
            names.insert(path, name);
        }
    }
    names
}

/// Produce a parsed module whose declared name and imports refer to the
/// resolved, unique names, ready to hand to the (name-keyed) canonicalizer.
/// In the common, no-duplicate case this changes nothing.
fn rewrite_module(loaded: &LoadedModule, unique_names: &HashMap<PathBuf, Name>) -> src::Module {
    // Only ever called for a module being compiled, which is always one that
    // was read: the graph cache's fast path is used exactly for the modules
    // whose ASTs nobody needs.
    let mut module = loaded
        .parsed
        .as_ref()
        .expect("rewrite_module on a module that was never parsed")
        .module
        .clone();
    let my_name = unique_names[&loaded.path].clone();

    match &mut module.name {
        Some(located) => located.value = my_name.clone(),
        None => module.name = Some(Located::new(Region::ZERO, my_name.clone())),
    }

    // Written import name -> resolved unique name.
    let targets: HashMap<Name, Name> = loaded
        .imports
        .iter()
        .map(|(written, path)| (written.clone(), unique_names[path].clone()))
        .collect();

    for import in &mut module.imports {
        if let Some(target) = targets.get(&import.name.value) {
            let original = import.name.value.clone();
            if *target != original {
                import.name.value = target.clone();
                // Keep the qualifier the user wrote. `import Foo` (no alias)
                // becomes, in effect, `import <unique> as Foo`, so `Foo.bar`
                // still resolves. An existing alias already fixes the
                // qualifier, so leave it.
                if import.alias.is_none() {
                    import.alias = Some(original);
                }
            }
        }
    }
    module
}

/// Depth-first topological sort over file paths; returns Err(path) on a cycle.
fn sort_modules(
    modules: &HashMap<PathBuf, LoadedModule>,
    entry: &Path,
) -> Result<Vec<PathBuf>, PathBuf> {
    let mut order = Vec::new();
    let mut state: HashMap<PathBuf, u8> = HashMap::new(); // 1 = visiting, 2 = done
    visit(modules, &entry.to_path_buf(), &mut state, &mut order)?;
    Ok(order)
}

fn visit(
    modules: &HashMap<PathBuf, LoadedModule>,
    path: &PathBuf,
    state: &mut HashMap<PathBuf, u8>,
    order: &mut Vec<PathBuf>,
) -> Result<(), PathBuf> {
    match state.get(path) {
        Some(2) => return Ok(()),
        Some(_) => return Err(path.clone()),
        None => {}
    }
    state.insert(path.clone(), 1);
    if let Some(module) = modules.get(path) {
        for import in module.import_paths() {
            visit(modules, import, state, order)?;
        }
    }
    state.insert(path.clone(), 2);
    order.push(path.clone());
    Ok(())
}

/// Compile a REPL entry: a synthetic module written to `path`, plus the name
/// of the one binding whose value should be printed.
///
/// Returns the JavaScript to run (nothing to print means nothing to run), or
/// the build errors. `path` is a scratch file the caller owns; it has to be on
/// disk because the loader resolves modules by path, and it has to be inside
/// the project so the walk up to `elm.json` finds the right dependencies.
///
/// `unqualified` names the modules whose types may print without their module
/// prefix — see [`crate::docs::render_type_for`].
pub fn compile_repl(
    path: &Path,
    print: Option<&str>,
    ansi: bool,
    unqualified: &HashSet<String>,
) -> Result<Option<String>, Vec<BuildError>> {
    let checked = check_project(path)?;
    let Some(print) = print else {
        return Ok(None);
    };
    let entry = checked.entry.clone();
    // The inferred type of the binding, which is what the REPL shows beside
    // the value. A binding the checker did not record is not a failure worth a
    // report: nothing sensible could be printed, so nothing is.
    let Some(tipe) = checked.types.get(&entry).and_then(|m| m.get(&Name::from(print))) else {
        return Ok(None);
    };
    // Fold expanded aliases back to their names, so a record that is really a
    // `Point` prints as one — the same table `--docs` uses.
    let borrowed: std::collections::BTreeMap<Name, &crate::interface::Interface> =
        checked.interfaces.iter().map(|(name, i)| (name.clone(), i)).collect();
    let aliases = crate::docs::AliasNames::preferring(&borrowed, Some(&entry));
    let type_text =
        crate::docs::render_type_for(&normalize_vars(tipe), &aliases, unqualified);
    Ok(Some(generate::generate_repl(
        &checked.modules,
        checked.node_types,
        generate::ReplPrint {
            module: entry,
            value: Name::from(print),
            type_text,
            ansi,
        },
    )))
}

/// Rename a type's variables the way elm names them when it prints one
/// definition's type: each scheme is numbered from scratch, so the first
/// `number` is `number` however many the module already used.
///
/// Without this, a REPL session drifts — `x = 42` reports `number`, and then
/// `x + 1` reports `number2` purely because a variable of that name is already
/// in the accumulated module.
fn normalize_vars(tipe: &can::Type) -> can::Type {
    let mut names: HashMap<Name, Name> = HashMap::new();
    let mut used: HashMap<String, u32> = HashMap::new();
    rename_vars(tipe, &mut names, &mut used)
}

fn rename_vars(
    tipe: &can::Type,
    names: &mut HashMap<Name, Name>,
    used: &mut HashMap<String, u32>,
) -> can::Type {
    match tipe {
        can::Type::Var(name) => {
            if let Some(renamed) = names.get(name) {
                return can::Type::Var(renamed.clone());
            }
            // A variable the checker invented is renamed from `a` onwards, so
            // `Nothing` reads `Maybe a` however many variables the accumulated
            // module used before it. One that carries a name — a constraint
            // like `number`, or a name written in a type annotation — keeps it
            // and is only renumbered against others of the same name.
            let base = var_base(name.as_str());
            let renamed = if invented(&base) {
                let next = used.entry(String::new()).or_insert(0);
                *next += 1;
                letters(*next - 1)
            } else {
                let count = used.entry(base.clone()).or_insert(0);
                *count += 1;
                if *count == 1 { base.clone() } else { format!("{base}{}", *count) }
            };
            let renamed = Name::from(renamed.as_str());
            names.insert(name.clone(), renamed.clone());
            can::Type::Var(renamed)
        }
        can::Type::Lambda(arg, body) => can::Type::Lambda(
            std::rc::Rc::new(rename_vars(arg, names, used)),
            std::rc::Rc::new(rename_vars(body, names, used)),
        ),
        can::Type::Type(home, name, args) => can::Type::Type(
            home.clone(),
            name.clone(),
            std::rc::Rc::new(args.iter().map(|a| rename_vars(a, names, used)).collect()),
        ),
        can::Type::Record(fields, ext) => can::Type::Record(
            std::rc::Rc::new(
                fields.iter().map(|(f, t)| (f.clone(), rename_vars(t, names, used))).collect(),
            ),
            ext.clone(),
        ),
        can::Type::Unit => can::Type::Unit,
        can::Type::Tuple(a, b, c) => can::Type::Tuple(
            std::rc::Rc::new(rename_vars(a, names, used)),
            std::rc::Rc::new(rename_vars(b, names, used)),
            c.as_ref().map(|c| std::rc::Rc::new(rename_vars(c, names, used))),
        ),
    }
}

/// A variable's name without the digits used to tell two of them apart, so
/// `number3` and `value1` renumber from `number` and `value`.
fn var_base(name: &str) -> String {
    let base = name.trim_end_matches(|c: char| c.is_ascii_digit());
    if base.is_empty() { name.to_string() } else { base.to_string() }
}

/// Whether a name looks like one the checker made up rather than one someone
/// wrote. Single letters are the generated sequence; a word came from a type
/// annotation or is a constraint, and either way it is worth keeping.
fn invented(base: &str) -> bool {
    base.chars().count() == 1 && base.chars().all(|c| c.is_ascii_lowercase())
}

/// `a`, `b`, … `z`, `a1`, `b1`, …
fn letters(index: u32) -> String {
    let letter = (b'a' + (index % 26) as u8) as char;
    match index / 26 {
        0 => letter.to_string(),
        n => format!("{letter}{n}"),
    }
}

#[cfg(test)]
mod live_tests {
    use super::fingerprint;

    /// The fingerprint stands in for the type, so it has to separate types
    /// that differ and agree for ones that do not.
    #[test]
    fn a_fingerprint_tracks_the_type_it_came_from() {
        assert_eq!(fingerprint("Basics.Int"), fingerprint("Basics.Int"));
        assert_ne!(fingerprint("Basics.Int"), fingerprint("String.String"));
        // A field added, removed, renamed or retyped must all show up.
        let base = "{ a : Basics.Int }";
        for other in [
            "{ a : Basics.Int, b : Basics.Int }",
            "{ }",
            "{ b : Basics.Int }",
            "{ a : String.String }",
        ] {
            assert_ne!(fingerprint(base), fingerprint(other), "{other} looked unchanged");
        }
    }

    /// It also has to be safe to put in an HTTP header, which the rendered
    /// type is not: a real `Model` runs to kilobytes, and Elm allows field
    /// names outside ASCII.
    #[test]
    fn a_fingerprint_is_always_short_ascii_hex() {
        let huge: String = (0..500).map(|i| format!("{{ f{i} : Basics.Int }}")).collect();
        for text in ["Basics.Int", "{ nästa : String.String }", "{ 名前 : Int }", &huge] {
            let digest = fingerprint(text);
            assert_eq!(digest.len(), 32, "{text}");
            assert!(digest.chars().all(|c| c.is_ascii_hexdigit()), "{digest}");
        }
    }
}
