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
use crate::interface::Interfaces;
use crate::reporting::{Located, Region, Report};
use crate::{builtins, canonicalize, generate, nitpick, optimize, parse, typecheck};

pub struct BuildError {
    pub path: PathBuf,
    pub source: String,
    pub reports: Vec<Report>,
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
        }
    }

    /// A build error carrying already-built reports (e.g. a byte-exact parse
    /// diagnostic from the syntax catalogue).
    fn from_reports(path: PathBuf, source: String, reports: Vec<Report>) -> Self {
        BuildError { path, source, reports }
    }

    pub fn render(&self) -> String {
        self.reports
            .iter()
            .map(|r| r.render(&self.path.display().to_string(), &self.source))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// A parsed module on disk, keyed in the loader by its (unique) file path so
/// that two same-named modules from different packages stay distinct.
struct LoadedModule {
    path: PathBuf,
    source: String,
    module: src::Module,
    /// The name declared in `module <Name> exposing ...`.
    declared_name: Name,
    /// Resolved user imports: the name as written plus the file it resolved
    /// to, within this module's package scope. Builtin/kernel imports are not
    /// listed here.
    imports: Vec<(Name, PathBuf)>,
}

impl LoadedModule {
    fn import_paths(&self) -> impl Iterator<Item = &PathBuf> {
        self.imports.iter().map(|(_, path)| path)
    }
}

/// Everything the front half of the compiler produces: the canonical
/// modules in dependency order plus their interfaces. Backends (JS today,
/// native later) consume this.
pub struct CheckedProject {
    pub modules: Vec<can::Module>,
    pub interfaces: Interfaces,
    /// Per-module, the concrete type of every expression keyed by source
    /// region (regions are only unique within a module). Monomorphization
    /// consumes this; other backends ignore it.
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
    let checked = check_project(entry)?;
    // Tree-shake by default; `ALM_NO_DCE=1` emits the whole runtime kernel as a
    // field kill-switch should DCE ever drop something an app needs.
    let dce = std::env::var_os("ALM_NO_DCE").is_none();
    // Lint walks the already-checked AST — a cheap single traversal, not a
    // second front end — so `alm make` prints hints without re-type-checking.
    let warnings = crate::lint::lint(&checked.modules, &checked.sources);
    Ok((
        generate::generate_project_typed(&checked.modules, checked.node_types, dce),
        warnings,
    ))
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
    let warnings = crate::lint::lint(&checked.modules, &checked.sources);
    let (js, map) =
        generate::generate_project_typed_mapped(&checked.modules, checked.node_types, &sources);
    Ok((js, map, warnings))
}

/// Compile a project to a native binary or wasm module at `output` via the
/// LLVM backend.
pub fn compile_project_native(
    entry: &Path,
    output: &Path,
    opt: generate::native::OptLevel,
) -> Result<Vec<crate::lint::Warning>, Vec<BuildError>> {
    let checked = check_project(entry)?;
    let warnings = crate::lint::lint(&checked.modules, &checked.sources);
    let program = crate::ir::lower::lower_project(&checked.modules);
    generate::native::build(&program, output, opt)
        .map(|()| warnings)
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
    let checked = check_project(entry)?;
    let warnings = crate::lint::lint(&checked.modules, &checked.sources);
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
    let mut program = crate::ir::mono::specialize_project(&infos, &checked.entry);
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
    generate::wasmgc::build(
        &program,
        output,
        &ports,
        &port_types,
        &ctor_arg_types,
        &unions,
        sources.as_ref(),
    )
    .map(|()| warnings)
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

/// Run the whole front end — load, parse, canonicalize, type check, and
/// exhaustiveness check every module — without generating any code.
pub fn check_project(entry: &Path) -> Result<CheckedProject, Vec<BuildError>> {
    let scopes = resolve_scopes(entry);

    // Load the entry module and, transitively, everything it imports. Modules
    // are keyed by file path so two same-named modules from different packages
    // do not clobber each other.
    let mut modules: HashMap<PathBuf, LoadedModule> = HashMap::new();
    let entry_key = load_module_file(entry, &scopes.app_search, &scopes, &mut modules)
        .map_err(|e| vec![e])?;

    // The entry module's declared name must match its file path.
    if let Some(e) = entry_name_mismatch(&modules[&entry_key], &scopes.app_search) {
        return Err(vec![e]);
    }

    // Topologically sort (dependencies first), detecting import cycles.
    let order = sort_modules(&modules, &entry_key).map_err(|cycle| {
        let module = &modules[&cycle];
        vec![BuildError::new(
            module.path.clone(),
            module.source.clone(),
            "IMPORT CYCLE",
            Region::ZERO,
            format!(
                "The module `{}` is part of an import cycle. Elm does not allow cyclic imports.",
                module.declared_name
            ),
        )]
    })?;

    // Give every loaded file a unique module name. When a name is declared by
    // just one file (the overwhelmingly common case) that file keeps it. When
    // several files share a name they are disambiguated so every downstream
    // map (interfaces, canonical modules, types) can stay keyed by `Name`.
    let unique_names = assign_unique_names(&modules, &order);

    // Compile each module against the interfaces of its dependencies.
    let mut interfaces = Interfaces::new();
    let mut canonical_modules = Vec::new();
    let mut all_node_types: HashMap<Name, HashMap<Region, can::Type>> = HashMap::new();
    let mut all_types: HashMap<Name, HashMap<Name, can::Type>> = HashMap::new();
    let mut all_sources: HashMap<Name, (PathBuf, String)> = HashMap::new();
    for path in &order {
        let source_module = &modules[path];
        let name = unique_names[path].clone();
        all_sources.insert(
            name.clone(),
            (source_module.path.clone(), source_module.source.clone()),
        );
        // Rewrite the parsed module so its declared name and its imports point
        // at the resolved, unique names. Downstream code is unchanged.
        let rewritten = rewrite_module(source_module, &unique_names);

        let (mut canonical, mut interface) =
            canonicalize::canonicalize_module(&rewritten, &interfaces).map_err(|errors| {
                errors
                    .into_iter()
                    .map(|e| {
                        BuildError::new(
                            source_module.path.clone(),
                            source_module.source.clone(),
                            "NAMING PROBLEM",
                            e.region,
                            e.message,
                        )
                    })
                    .collect::<Vec<_>>()
            })?;

        let checked = typecheck::check_module(&canonical, &interfaces).map_err(|errors| {
            errors
                .into_iter()
                .map(|e| match e.report {
                    Some(report) => BuildError::from_reports(
                        source_module.path.clone(),
                        source_module.source.clone(),
                        vec![report],
                    ),
                    None => BuildError::new(
                        source_module.path.clone(),
                        source_module.source.clone(),
                        "TYPE MISMATCH",
                        e.region,
                        e.message,
                    ),
                })
                .collect::<Vec<_>>()
        })?;
        let types = checked.types;
        all_node_types.insert(name.clone(), checked.node_types);

        nitpick::check(&canonical, &interfaces).map_err(|errors| {
            errors
                .into_iter()
                .map(|e| {
                    BuildError::new(
                        source_module.path.clone(),
                        source_module.source.clone(),
                        "MISSING PATTERNS",
                        e.region,
                        e.message,
                    )
                })
                .collect::<Vec<_>>()
        })?;

        // Local constant folding / simplification (after nitpick so it sees the
        // original patterns; before codegen so every backend benefits).
        optimize::simplify_module(&mut canonical);

        for name in interface.value_names.clone() {
            if let Some(tipe) = types.get(&name) {
                interface.values.insert(name, tipe.clone());
            }
        }
        for def in interface.binops.values_mut() {
            def.tipe = types.get(&def.function).cloned();
        }
        interfaces.insert(name.clone(), interface);
        all_types.insert(name.clone(), types);
        canonical_modules.push(canonical);
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
/// disk. Version *ranges* (as used in a package elm.json) are ignored — we
/// only pin exact versions.
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
    installed
}

/// The `"author/name"` keys listed under `dependencies.direct` of an
/// application elm.json. Returns `None` if there is no such block (e.g. a
/// package elm.json, or a minimal test elm.json).
fn direct_dependency_names(elm_json: &str) -> Option<Vec<String>> {
    let deps = object_block(elm_json, "dependencies")?;
    let direct = object_block(deps, "direct")?;
    Some(
        quoted_strings(direct)
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

/// Load a bundled effect-module source into the module graph under a synthetic
/// key, recursing into any other bundled modules it imports (e.g. Random → Time).
fn load_bundled_module(
    name: &Name,
    source: &'static str,
    scopes: &Scopes,
    modules: &mut HashMap<PathBuf, LoadedModule>,
) -> Result<PathBuf, BuildError> {
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
            source: source.to_string(),
            module,
            declared_name,
            imports: Vec::new(),
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
            let child = load_bundled_module(&import, bsrc, scopes, modules)?;
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
            let child_search = scopes.search_for(&matched_dir);
            let child = load_module_file(&import_path, child_search, scopes, modules)?;
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
) -> Result<PathBuf, BuildError> {
    let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if modules.contains_key(&key) {
        return Ok(key);
    }

    let source = std::fs::read_to_string(path).map_err(|err| {
        BuildError::new(
            path.to_path_buf(),
            String::new(),
            "FILE PROBLEM",
            Region::ZERO,
            format!("I could not read {}: {}", path.display(), err),
        )
    })?;

    let module = parse::parse_module_typed(&source, scopes.is_package).map_err(|e| match e.syntax {
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
    modules.insert(
        key.clone(),
        LoadedModule {
            path: key.clone(),
            source: source.clone(),
            module,
            declared_name: declared_name.clone(),
            imports: Vec::new(),
        },
    );

    let mut resolved: Vec<(Name, PathBuf)> = Vec::new();
    for import in import_names {
        if let Some(bsrc) = bundled_source(import.as_str()) {
            let child_key = load_bundled_module(&import, bsrc, scopes, modules)?;
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
        let child_search = scopes.search_for(&matched_dir);
        let child_key = load_module_file(&import_path, child_search, scopes, modules)?;
        let found_name = modules[&child_key].declared_name.clone();
        if found_name != import {
            return Err(BuildError::new(
                import_path.clone(),
                modules[&child_key].source.clone(),
                "MODULE NAME MISMATCH",
                Region::ZERO,
                format!(
                    "This file is named {} so I expected it to declare `module {}`, but it declares `module {}`.",
                    import_path.display(),
                    import,
                    found_name
                ),
            ));
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
    let name = m.module.name.as_ref()?;
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
        m.source.clone(),
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
    let mut module = loaded.module.clone();
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
