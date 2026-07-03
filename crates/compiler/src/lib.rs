//! alm — a port of the Elm compiler from Haskell to Rust.
//!
//! Pipeline: parse → canonicalize → type check → generate JavaScript,
//! mirroring the architecture of `elm/compiler`.

pub mod ast;
pub mod builtins;
pub mod canonicalize;
pub mod data;
pub mod parse;
pub mod reporting;
pub mod typecheck;
