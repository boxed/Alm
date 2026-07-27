//! alm — a port of the Elm compiler from Haskell to Rust.
//!
//! Pipeline: parse → canonicalize → type check → generate JavaScript,
//! mirroring the architecture of `elm/compiler`.

pub mod ast;
pub mod builtins;
pub mod canonicalize;
pub mod data;
pub mod decision;
pub mod generate;
pub mod interface;
pub mod ir;
pub mod lint;
pub mod nitpick;
pub mod debug_uses;
pub mod docs;
pub mod api_diff;
pub mod docs_json;
pub mod json;
pub mod optimize;
pub mod packages;
pub mod parse;
pub mod project;
pub mod reporting;
pub mod typecheck;

use reporting::Report;

/// Compile one Elm module to JavaScript, or produce friendly error reports.
pub fn compile(source: &str) -> Result<String, Vec<Report>> {
    Ok(generate::generate(&check(source)?))
}

/// Like [`compile`], but first checks that the declared module name matches
/// `expected` (the module's file path), reporting MODULE NAME MISMATCH if not.
pub fn compile_named(source: &str, expected: &str) -> Result<String, Vec<Report>> {
    compile_named_typed(source, expected, false)
}

/// As [`compile_named`], but validating ports/effects against the project type
/// (a package cannot declare ports).
pub fn compile_named_typed(
    source: &str,
    expected: &str,
    is_package: bool,
) -> Result<String, Vec<Report>> {
    if let Some(report) = module_name_mismatch(source, expected) {
        return Err(vec![report]);
    }
    Ok(generate::generate(&check_typed(source, is_package)?))
}

/// The MODULE NAME MISMATCH report if `source` parses to a module whose declared
/// name differs from `expected`; otherwise `None`. Parse failures are left for
/// the normal pipeline to surface.
pub fn module_name_mismatch(source: &str, expected: &str) -> Option<Report> {
    let name = parse::parse_module(source).ok()?.name?;
    if name.value.as_str() == expected {
        return None;
    }
    Some(
        reporting::syntax::SyntaxError::ModuleNameMismatch {
            region: name.region,
            expected: expected.to_string(),
            actual: name.value.as_str().to_string(),
        }
        .to_report(),
    )
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
    check_typed(source, false)
}

/// As [`check`], but validating ports/effects against the project type (a
/// package cannot declare ports).
fn check_typed(source: &str, is_package: bool) -> Result<ast::canonical::Module, Vec<Report>> {
    let module = parse::parse_module_typed(source, is_package).map_err(|e| {
        vec![match e.syntax {
            Some(se) => se.to_report(),
            None => Report {
                title: "SYNTAX PROBLEM".to_string(),
                region: e.region,
                message: e.message,
                elm: None,
            },
        }]
    })?;

    let canonical = canonicalize::canonicalize(&module).map_err(|errors| {
        errors
            .into_iter()
            .map(|e| Report {
                title: "NAMING PROBLEM".to_string(),
                region: e.region,
                message: e.message,
                elm: None,
            })
            .collect::<Vec<_>>()
    })?;

    typecheck::check(&canonical).map_err(|errors| {
        errors
            .into_iter()
            .map(|e| match e.report {
                Some(report) => report,
                None => Report {
                    title: "TYPE MISMATCH".to_string(),
                    region: e.region,
                    message: e.message,
                    elm: None,
                },
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
                elm: None,
            })
            .collect::<Vec<_>>()
    })?;

    Ok(canonical)
}
