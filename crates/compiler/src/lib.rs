//! alm — a port of the Elm compiler from Haskell to Rust.
//!
//! Pipeline: parse → canonicalize → type check → generate JavaScript,
//! mirroring the architecture of `elm/compiler`.

pub mod ast;
pub mod builtins;
pub mod canonicalize;
pub mod data;
pub mod generate;
pub mod parse;
pub mod reporting;
pub mod typecheck;

use reporting::Report;

/// Compile one Elm module to JavaScript, or produce friendly error reports.
pub fn compile(source: &str) -> Result<String, Vec<Report>> {
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

    Ok(generate::generate(&canonical))
}
