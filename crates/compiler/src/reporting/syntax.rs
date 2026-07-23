//! Port of `Reporting.Error.Syntax`: the catalogue that turns a structured
//! parse failure into the official compiler's exact error report. Each variant
//! corresponds to one elm error and reproduces its title and prose verbatim so
//! `alm make` output matches `elm make` byte-for-byte.

use super::{ElmBody, Region, Report, Section};

/// A structured parse error, produced by the parser at the point it got stuck.
#[derive(Debug, Clone)]
pub enum SyntaxError {
    /// An `if` expression missing its `then` keyword.
    IfThen { region: Region },
    /// An `if` expression missing its `else` branch.
    IfElse { region: Region },
}

impl SyntaxError {
    /// The region used to order competing parse errors ("furthest wins").
    pub fn region(&self) -> Region {
        match self {
            SyntaxError::IfThen { region } | SyntaxError::IfElse { region } => *region,
        }
    }

    /// Build the full diagnostic, matching `elm make`.
    pub fn to_report(&self) -> Report {
        match self {
            SyntaxError::IfThen { region } => snippet(
                "UNFINISHED IF",
                *region,
                "I was expecting to see more of this `if` expression, but I got stuck here:",
                "I was expecting to see the then keyword next.",
                vec![],
            ),
            SyntaxError::IfElse { region } => snippet(
                "UNFINISHED IF",
                *region,
                "I was expecting to see an `else` branch after this:",
                "I know what to do when the condition is True, but what happens when it is \
                 False? Add an else branch to handle that scenario!",
                vec![],
            ),
        }
    }
}

/// Build a `Report` from an elm snippet-style body.
fn snippet(title: &str, region: Region, before: &str, after: &str, notes: Vec<Section>) -> Report {
    Report {
        title: title.to_string(),
        region,
        message: after.to_string(),
        elm: Some(ElmBody {
            before: before.to_string(),
            after: after.to_string(),
            notes,
            highlight: region,
        }),
    }
}
