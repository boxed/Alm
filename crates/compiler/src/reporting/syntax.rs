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
    /// `0x` with no hexadecimal digits following.
    WeirdHex { region: Region },
    /// A character literal with no closing single quote before end of line.
    CharEnd { region: Region },
    /// An empty/single-quoted string (`''`) — elm wants double quotes.
    CharDoubleQuotes { region: Region },
    /// A list literal with no closing `]`.
    UnfinishedList { region: Region },
}

impl SyntaxError {
    /// The region used to order competing parse errors ("furthest wins").
    pub fn region(&self) -> Region {
        match self {
            SyntaxError::IfThen { region }
            | SyntaxError::IfElse { region }
            | SyntaxError::WeirdHex { region }
            | SyntaxError::CharEnd { region }
            | SyntaxError::CharDoubleQuotes { region }
            | SyntaxError::UnfinishedList { region } => *region,
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
            SyntaxError::WeirdHex { region } => snippet(
                "WEIRD HEXIDECIMAL",
                *region,
                "I thought I was reading a hexidecimal number until I got here:",
                "Valid hexidecimal digits include 0123456789abcdefABCDEF, so I can only \
                 recognize things like this:",
                vec![Section::Block("    0x2B\n    0x002B\n    0x00ffb3".to_string())],
            ),
            SyntaxError::CharEnd { region } => snippet(
                "MISSING SINGLE QUOTE",
                *region,
                "I thought I was parsing a character, but I got to the end of the line \
                 without seeing the closing single quote:",
                "Add a closing single quote here!",
                vec![],
            ),
            SyntaxError::CharDoubleQuotes { region } => snippet(
                "NEEDS DOUBLE QUOTES",
                *region,
                "The following string uses single quotes:",
                "Please switch to double quotes instead:",
                vec![
                    Section::Block("    'this' => \"this\"".to_string()),
                    Section::Para(
                        "Note: Elm uses double quotes for strings like \"hello\", whereas it \
                         uses single quotes for individual characters like 'a' and 'ø'. This \
                         distinction helps with code like (String.any (\\c -> c == 'X') \
                         \"90210\") where you are inspecting individual characters."
                            .to_string(),
                    ),
                ],
            ),
            SyntaxError::UnfinishedList { region } => snippet(
                "UNFINISHED LIST",
                *region,
                "I cannot find the end of this list:",
                "You can just add a closing ] right here, and I will be all set!",
                vec![
                    Section::Para(
                        "Note: I may be confused by indentation. For example, if you are \
                         trying to define a list across multiple lines, I recommend using \
                         this format:"
                            .to_string(),
                    ),
                    Section::Block(
                        "    [ \"Alice\"\n    , \"Bob\"\n    , \"Chuck\"\n    ]".to_string(),
                    ),
                    Section::Para(
                        "Notice that each line starts with some indentation. Usually two or \
                         four spaces. This is the stylistic convention in the Elm ecosystem."
                            .to_string(),
                    ),
                ],
            ),
        }
    }
}

/// Build a `Report` from an elm snippet-style body.
fn snippet(title: &str, region: Region, before: &str, after: &str, notes: Vec<Section>) -> Report {
    Report {
        title: title.to_string(),
        region,
        // A searchable summary (used by substring-based diagnostics tests); the
        // byte-exact layout lives in `elm` below.
        message: format!("{before} {after}"),
        elm: Some(ElmBody {
            before: before.to_string(),
            after: after.to_string(),
            notes,
            highlight: region,
        }),
    }
}
