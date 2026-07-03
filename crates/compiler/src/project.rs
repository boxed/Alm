//! Port of the `builder/` half of the Elm compiler (much simplified):
//! find the project, resolve imports to files, and compile every module
//! in dependency order into one JavaScript file.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

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

pub fn compile_project(entry: &Path) -> Result<String, Vec<BuildError>> {
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
        interfaces.insert(name.clone(), interface);
        canonical_modules.push(canonical);
    }

    Ok(generate::generate_project(&canonical_modules))
}

/// Walk up from the entry file looking for elm.json; fall back to treating
/// the entry file's directory as the only source directory.
fn find_source_directories(entry: &Path) -> Vec<PathBuf> {
    let entry_dir = entry
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let mut dir = entry_dir.clone();
    loop {
        let elm_json = dir.join("elm.json");
        if elm_json.is_file() {
            if let Ok(contents) = std::fs::read_to_string(&elm_json) {
                let dirs = parse_source_directories(&contents);
                if !dirs.is_empty() {
                    return dirs.iter().map(|d| dir.join(d)).collect();
                }
            }
            return vec![dir.join("src")];
        }
        match dir.parent() {
            Some(parent) => dir = parent.to_path_buf(),
            None => return vec![entry_dir],
        }
    }
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
        .filter(|i| !builtins::is_builtin_module(i.name.value.as_str()))
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
    let mut order = Vec::new();
    let mut state: HashMap<Name, u8> = HashMap::new(); // 1 = visiting, 2 = done
    visit(modules, entry, &mut state, &mut order)?;
    // Any modules not reachable from the entry (possible when several files
    // were loaded eagerly) — include them too for completeness.
    let mut names: Vec<&Name> = modules.keys().collect();
    names.sort();
    for name in names {
        if !state.contains_key(name) {
            visit(modules, name, &mut state, &mut order)?;
        }
    }
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
