//! alm — a port of the Elm compiler from Haskell to Rust.
//!
//! Pipeline: parse → canonicalize → type check → generate JavaScript,
//! mirroring the architecture of `elm/compiler`.

pub mod ast;
pub mod builtins;
pub mod canonicalize;
pub mod data;
pub mod generate;
pub mod interface;
pub mod ir;
pub mod nitpick;
pub mod parse;
pub mod project;
pub mod reporting;
pub mod typecheck;

use reporting::Report;

/// Compile one Elm module to JavaScript, or produce friendly error reports.
pub fn compile(source: &str) -> Result<String, Vec<Report>> {
    Ok(generate::generate(&check(source)?))
}

/// Like [`compile`], but without dead-code elimination — the whole runtime
/// kernel is emitted. Only for tests that reach into kernel internals the app
/// itself never references.
pub fn compile_no_dce(source: &str) -> Result<String, Vec<Report>> {
    Ok(generate::generate_no_dce(&check(source)?))
}

/// Compile one module to JS with a Source Map v3, returning `(js, map_json)`.
/// The single source is recorded as `Main.elm`. DCE is off (see
/// [`generate::generate_project_typed_mapped`]).
pub fn compile_with_source_map(source: &str) -> Result<(String, String), Vec<Report>> {
    let module = check(source)?;
    let mut sources = std::collections::HashMap::new();
    sources.insert(
        module.name.clone(),
        ("Main.elm".to_string(), source.to_string()),
    );
    Ok(generate::generate_project_typed_mapped(
        std::slice::from_ref(&module),
        std::collections::HashMap::new(),
        &sources,
    ))
}

/// Parse, canonicalize, type-check and nitpick a single module.
fn check(source: &str) -> Result<ast::canonical::Module, Vec<Report>> {
    let module = parse::parse_module(source).map_err(|e| {
        vec![Report {
            title: "SYNTAX PROBLEM".to_string(),
            region: e.region,
            message: e.message,
        }]
    })?;

    let canonical = canonicalize::canonicalize(&module).map_err(|errors| {
        errors
            .into_iter()
            .map(|e| Report {
                title: "NAMING PROBLEM".to_string(),
                region: e.region,
                message: e.message,
            })
            .collect::<Vec<_>>()
    })?;

    typecheck::check(&canonical).map_err(|errors| {
        errors
            .into_iter()
            .map(|e| Report {
                title: "TYPE MISMATCH".to_string(),
                region: e.region,
                message: e.message,
            })
            .collect::<Vec<_>>()
    })?;

    let interfaces = interface::Interfaces::new();
    nitpick::check(&canonical, &interfaces).map_err(|errors| {
        errors
            .into_iter()
            .map(|e| Report {
                title: "MISSING PATTERNS".to_string(),
                region: e.region,
                message: e.message,
            })
            .collect::<Vec<_>>()
    })?;

    Ok(canonical)
}
