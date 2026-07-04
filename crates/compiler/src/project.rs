//! Port of the `builder/` half of the Elm compiler (much simplified):
//! find the project, resolve imports to files, and compile every module
//! in dependency order into one JavaScript file.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::ast::canonical as can;
use crate::ast::source as src;
use crate::data::Name;
use crate::interface::Interfaces;
use crate::reporting::{Region, Report};
use crate::{builtins, canonicalize, generate, nitpick, parse, typecheck};

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
            }],
        }
    }

    pub fn render(&self) -> String {
        self.reports
            .iter()
            .map(|r| r.render(&self.path.display().to_string(), &self.source))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

struct SourceModule {
    path: PathBuf,
    source: String,
    module: src::Module,
    imports: Vec<Name>,
}

/// Everything the front half of the compiler produces: the canonical
/// modules in dependency order plus their interfaces. Backends (JS today,
/// native later) consume this.
pub struct CheckedProject {
    pub modules: Vec<can::Module>,
    pub interfaces: Interfaces,
}

pub fn compile_project(entry: &Path) -> Result<String, Vec<BuildError>> {
    let checked = check_project(entry)?;
    Ok(generate::generate_project(&checked.modules))
}

/// Compile a project to a native binary at `output` via the LLVM backend.
pub fn compile_project_native(entry: &Path, output: &Path) -> Result<(), Vec<BuildError>> {
    let checked = check_project(entry)?;
    let program = crate::ir::lower::lower_project(&checked.modules);
    generate::native::build(&program, output).map_err(|message| {
        vec![BuildError::new(
            entry.to_path_buf(),
            String::new(),
            "NATIVE BACKEND",
            Region::ZERO,
            message,
        )]
    })
}

/// Run the whole front end — load, parse, canonicalize, type check, and
/// exhaustiveness check every module — without generating any code.
pub fn check_project(entry: &Path) -> Result<CheckedProject, Vec<BuildError>> {
    let source_dirs = find_source_directories(entry);

    // Load the entry module and, transitively, everything it imports.
    let mut modules: HashMap<Name, SourceModule> = HashMap::new();
    let entry_name = load_module_file(entry, &source_dirs, &mut modules)
        .map_err(|e| vec![e])?;

    // Topologically sort (dependencies first), detecting import cycles.
    let order = sort_modules(&modules, &entry_name).map_err(|cycle| {
        let module = &modules[&cycle];
        vec![BuildError::new(
            module.path.clone(),
            module.source.clone(),
            "IMPORT CYCLE",
            Region::ZERO,
            format!(
                "The module `{}` is part of an import cycle. Elm does not allow cyclic imports.",
                cycle
            ),
        )]
    })?;

    // Compile each module against the interfaces of its dependencies.
    let mut interfaces = Interfaces::new();
    let mut canonical_modules = Vec::new();
    for name in &order {
        let source_module = &modules[name];
        let (canonical, mut interface) =
            canonicalize::canonicalize_module(&source_module.module, &interfaces).map_err(
                |errors| {
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
                },
            )?;

        let types = typecheck::check_module(&canonical, &interfaces).map_err(|errors| {
            errors
                .into_iter()
                .map(|e| {
                    BuildError::new(
                        source_module.path.clone(),
                        source_module.source.clone(),
                        "TYPE MISMATCH",
                        e.region,
                        e.message,
                    )
                })
                .collect::<Vec<_>>()
        })?;

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

        for name in interface.value_names.clone() {
            if let Some(tipe) = types.get(&name) {
                interface.values.insert(name, tipe.clone());
            }
        }
        for def in interface.binops.values_mut() {
            def.tipe = types.get(&def.function).cloned();
        }
        interfaces.insert(name.clone(), interface);
        canonical_modules.push(canonical);
    }

    Ok(CheckedProject {
        modules: canonical_modules,
        interfaces,
    })
}

/// Walk up from the entry file looking for elm.json; fall back to treating
/// the entry file's directory as the only source directory. Package
/// dependencies listed in elm.json are added from the ELM_HOME cache so
/// pure Elm packages compile from their real sources.
fn find_source_directories(entry: &Path) -> Vec<PathBuf> {
    let entry_dir = entry
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let mut dir = entry_dir.clone();
    loop {
        let elm_json = dir.join("elm.json");
        if elm_json.is_file() {
            let mut dirs = Vec::new();
            if let Ok(contents) = std::fs::read_to_string(&elm_json) {
                let sources = parse_source_directories(&contents);
                if sources.is_empty() {
                    dirs.push(dir.join("src"));
                } else {
                    dirs.extend(sources.iter().map(|d| dir.join(d)));
                }
                dirs.extend(package_directories(&contents));
            } else {
                dirs.push(dir.join("src"));
            }
            return dirs;
        }
        match dir.parent() {
            Some(parent) => dir = parent.to_path_buf(),
            None => return vec![entry_dir],
        }
    }
}

/// Source directories of every package mentioned in elm.json, from the
/// ELM_HOME cache (~/.elm/0.19.1/packages/<author>/<name>/<version>/src).
fn package_directories(elm_json: &str) -> Vec<PathBuf> {
    let home = std::env::var("ELM_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let user = std::env::var("HOME").unwrap_or_default();
            PathBuf::from(user).join(".elm")
        });
    let packages = home.join("0.19.1").join("packages");

    let mut dirs = Vec::new();
    // Scan for `"author/name": "1.2.3"` pairs anywhere in elm.json.
    let bytes = elm_json.as_bytes();
    let mut i = 0;
    while let Some(quote) = elm_json[i..].find('"') {
        let start = i + quote + 1;
        let Some(end_rel) = elm_json[start..].find('"') else { break };
        let key = &elm_json[start..start + end_rel];
        i = start + end_rel + 1;
        if !key.contains('/') {
            continue;
        }
        // Value: skip whitespace and colon, expect a quoted version.
        let mut j = i;
        while j < bytes.len() && (bytes[j] == b':' || bytes[j].is_ascii_whitespace()) {
            j += 1;
        }
        if j >= bytes.len() || bytes[j] != b'"' {
            continue;
        }
        let vstart = j + 1;
        let Some(vend_rel) = elm_json[vstart..].find('"') else { break };
        let version = &elm_json[vstart..vstart + vend_rel];
        if version.chars().all(|c| c.is_ascii_digit() || c == '.') {
            let (author, name) = key.split_once('/').unwrap();
            let src = packages.join(author).join(name).join(version).join("src");
            if src.is_dir() {
                dirs.push(src);
            }
        }
    }
    dirs
}

/// Extract `"source-directories": [ ... ]` from elm.json without a JSON
/// dependency. (The rest of elm.json — package versions — is not used.)
fn parse_source_directories(json: &str) -> Vec<String> {
    let Some(key_pos) = json.find("\"source-directories\"") else {
        return vec![];
    };
    let rest = &json[key_pos..];
    let Some(open) = rest.find('[') else { return vec![] };
    let Some(close) = rest[open..].find(']') else { return vec![] };
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

/// Parse a module file and recursively load everything it imports.
/// Returns the module's name.
fn load_module_file(
    path: &Path,
    source_dirs: &[PathBuf],
    modules: &mut HashMap<Name, SourceModule>,
) -> Result<Name, BuildError> {
    let source = std::fs::read_to_string(path).map_err(|err| {
        BuildError::new(
            path.to_path_buf(),
            String::new(),
            "FILE PROBLEM",
            Region::ZERO,
            format!("I could not read {}: {}", path.display(), err),
        )
    })?;

    let module = parse::parse_module(&source).map_err(|e| {
        BuildError::new(
            path.to_path_buf(),
            source.clone(),
            "SYNTAX PROBLEM",
            e.region,
            e.message,
        )
    })?;

    let name = module.get_name();
    let imports = user_imports(&module);
    modules.insert(
        name.clone(),
        SourceModule {
            path: path.to_path_buf(),
            source: source.clone(),
            module,
            imports: imports.clone(),
        },
    );

    for import in imports {
        if modules.contains_key(&import) {
            continue;
        }
        let import_path = find_module_file(&import, source_dirs).ok_or_else(|| {
            BuildError::new(
                path.to_path_buf(),
                source.clone(),
                "MODULE NOT FOUND",
                Region::ZERO,
                format!(
                    "The `{}` module imports `{}`, but I cannot find it. I looked for {} in: {}",
                    name,
                    import,
                    module_file_name(&import),
                    source_dirs
                        .iter()
                        .map(|d| d.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            )
        })?;
        let found_name = load_module_file(&import_path, source_dirs, modules)?;
        if found_name != import {
            let found = &modules[&found_name];
            return Err(BuildError::new(
                found.path.clone(),
                found.source.clone(),
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
    }
    Ok(name)
}

fn module_file_name(name: &Name) -> String {
    format!("{}.elm", name.as_str().replace('.', "/"))
}

fn find_module_file(name: &Name, source_dirs: &[PathBuf]) -> Option<PathBuf> {
    let relative = module_file_name(name);
    source_dirs
        .iter()
        .map(|dir| dir.join(&relative))
        .find(|path| path.is_file())
}

/// Depth-first topological sort; returns Err(name) on a cycle.
fn sort_modules(
    modules: &HashMap<Name, SourceModule>,
    entry: &Name,
) -> Result<Vec<Name>, Name> {
    // Every module in the map was reached from the entry, so one DFS
    // covers them all.
    let mut order = Vec::new();
    let mut state: HashMap<Name, u8> = HashMap::new(); // 1 = visiting, 2 = done
    visit(modules, entry, &mut state, &mut order)?;
    Ok(order)
}

fn visit(
    modules: &HashMap<Name, SourceModule>,
    name: &Name,
    state: &mut HashMap<Name, u8>,
    order: &mut Vec<Name>,
) -> Result<(), Name> {
    match state.get(name) {
        Some(2) => return Ok(()),
        Some(_) => return Err(name.clone()),
        None => {}
    }
    state.insert(name.clone(), 1);
    if let Some(module) = modules.get(name) {
        for import in &module.imports {
            visit(modules, import, state, order)?;
        }
    }
    state.insert(name.clone(), 2);
    order.push(name.clone());
    Ok(())
}
