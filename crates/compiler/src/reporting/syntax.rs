//! Port of `Reporting.Error.Syntax`: the catalogue that turns a structured
//! parse failure into the official compiler's exact error report. Each variant
//! corresponds to one elm error and reproduces its title and prose verbatim so
//! `alm make` output matches `elm make` byte-for-byte.

use super::{
    blue, code_block, code_line, cyan, green, hint, marked, note, sentence, words, yellow, Doc,
    ElmBody,
    Region, Report, Section,
};

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
    /// A parenthesized expression with no closing `)`.
    UnfinishedParens { region: Region },
    /// A single-line string with no closing double quote before end of line.
    EndlessString { region: Region },
    /// A `\arg` lambda missing its `->` arrow.
    UnfinishedLambda { region: Region },
    /// A record literal with no closing `}`.
    UnfinishedRecord { region: Region },
    /// A record field name not followed by `=`.
    RecordEquals { region: Region },
    /// A binary operator with no expression after it.
    MissingExpression { region: Region, op: String },
    /// A `case` scrutinee followed by `->` instead of `of`.
    CaseOf { region: Region },
    /// A `case` branch pattern not followed by `->`.
    CaseArrow { region: Region },
    /// A `let` block with no `in` keyword.
    LetProblem { region: Region },
    /// A top-level line that does not start a valid declaration.
    WeirdDeclaration { region: Region },
    /// A definition `name =` with no expression body.
    DefBody { region: Region, name: String },
    /// A definition `name` with neither more args nor `=`.
    DefEquals { region: Region, name: String },
    /// A `module` declaration whose name is missing/lowercase.
    ExpectingModuleName { region: Region },
    /// A `module` declaration that stops before/at `exposing`.
    UnfinishedModuleDecl { region: Region },
    /// An `exposing (...)` list with no closing `)`.
    UnfinishedExposing { region: Region },
    /// An `import` declaration that got stuck.
    UnfinishedImport { region: Region },
    /// A `port` declaration missing its `:` and type.
    UnfinishedPort { region: Region },
    /// A `port` declaration in a non-`port module`.
    UnexpectedPorts { region: Region },
    /// A multi-line comment with no closing `-}`.
    EndlessComment { region: Region },
    /// A type annotation `name :` with no type after the colon.
    DefType { region: Region, name: String },
    /// A `type alias` with no type after `=`.
    TypeAliasBody { region: Region },
    /// A `type alias` name not followed by a type variable or `=`.
    TypeAliasEquals { region: Region },
    /// A custom `type` with no first variant after `=`.
    CustomEquals { region: Region },
    /// A custom `type` with no variant after `|`.
    CustomBar { region: Region },
    /// A reserved word (`in`) used where a name was expected.
    ReservedWord { region: Region, word: String },
    /// A `case` expression stuck expecting a pattern (e.g. `case x of` <eof>).
    UnfinishedCase { region: Region },
    /// A `let` expression stuck before any value is defined (e.g. `let` <eof>).
    UnfinishedLet { region: Region },
    /// A tuple stuck after a comma, expecting another expression.
    UnfinishedTuple { region: Region },
    /// An operator function `(+` with no closing `)`.
    OperatorFunction { region: Region },
    /// A record accessor `.` not followed by a lower-case field name.
    RecordAccessor { region: Region },
    /// An `import` whose module name is missing/invalid (a stray token present).
    ExpectingImportName { region: Region },
    /// An `import ... as` whose alias is missing/invalid (a stray token present).
    ExpectingImportAlias { region: Region },
    /// An `exposing (...)` list with an unparseable exposed value.
    ProblemInExposing { region: Region },
    /// `exposing (Type(x))` — trying to expose specific variants, not `(..)`.
    ExposingTypePrivacy { region: Region },
    /// A `port module` declaration missing the `module` keyword.
    UnfinishedPortModule { region: Region },
    /// A type annotation whose name differs from the following definition's name.
    NameMismatch {
        region: Region,
        highlight: Region,
        annotation: String,
        definition: String,
    },
    /// A definition stuck on a stray token that is neither an argument nor `=`.
    ProblemInDefinition { region: Region, name: String },
    /// A tuple type `(a,` with a comma but no following type (`TTupleIndentTypeN`).
    UnfinishedTupleType { region: Region },
    /// A record type that never reaches its closing `}` (`TRecordIndentEnd`).
    UnfinishedRecordType { region: Region },
    /// A record type expecting another field name after a `,` (`TRecordField`).
    ProblemInRecordType { region: Region },
    /// A custom `type` with a missing or lowercase name (`CT_Name`).
    ExpectingTypeName { region: Region },
    /// A `type alias` with a missing or lowercase name (`AliasName`).
    ExpectingTypeAliasName { region: Region },
    /// A `type alias` body where a type was expected but a bad token appeared
    /// (`AliasBody` delegating to `TStart` in the `TC_TypeAlias` context).
    ProblemInTypeAlias { region: Region },
    /// A custom `type` where a variant name was expected (`CT_Variant`).
    ProblemInCustomType { region: Region },
    /// A pattern position that starts with something that is not a pattern.
    PatternStart { region: Region },
    /// An `as` keyword in a pattern not followed by a variable name.
    PatternAlias { region: Region },
    /// A floating point literal used as a pattern.
    PatternFloat { region: Region },
    /// A list pattern that got stuck right after the opening `[`.
    ListPatternOpen { region: Region },
    /// A list pattern whose closing `]` is missing (unexpected token).
    ListPatternEnd { region: Region },
    /// A list pattern whose closing `]` is missing (dedented / end of input).
    ListPatternIndentEnd { region: Region },
    /// A list pattern with a trailing `,` but no following pattern.
    ListPatternExpr { region: Region },
    /// A record pattern position expecting a field name.
    RecordPatternField { region: Region },
    /// A record pattern whose closing `}` is missing.
    RecordPatternEnd { region: Region },
    /// A tuple pattern with a trailing `,` but no following pattern.
    TuplePatternExpr { region: Region },
    /// A number written with a leading zero (`00`, `05`).
    LeadingZeros { region: Region },
    /// A number ending with a dot and no fractional digits (`1.`).
    NumberDot { region: Region },
    /// A number that ran into unexpected trailing characters (`3e`, `12x`).
    NumberEnd { region: Region },
    /// A string/char escape that is not one of the recognised forms (`\q`).
    UnknownEscape { region: Region },
    /// A malformed `\u{...}` unicode escape.
    BadUnicodeEscape { region: Region, problem: BadUnicode },
    /// A literal tab character (tabs are not allowed in Elm source).
    NoTabs { region: Region },
    /// A declaration starting with a capital letter (`toDeclStartReport`'s
    /// `Upper` branch); `lower_name` is the suggested lower-cased name.
    UnexpectedCapital { region: Region, lower_name: String },
    /// A declaration starting with a stray symbol (`toDeclStartReport`'s `Other`
    /// branch); `symbol` is the offending character.
    UnexpectedSymbolDecl { region: Region, symbol: char },
    /// A leftover symbol (`#`, `@`, `~`) after a declaration.
    UnexpectedSymbolMid { region: Region },
    /// A leftover `$` after a declaration (`toWeirdEndReport`'s dollar branch).
    UnexpectedDollar { region: Region },
    /// A leftover backtick after a declaration (UNEXPECTED CHARACTER).
    UnexpectedBacktick { region: Region },
    /// A leftover `;` after a declaration.
    UnexpectedSemicolon { region: Region },
    /// A leftover `,` after a declaration.
    UnexpectedComma { region: Region },
    /// A leftover `=` after a declaration (`BadEquals`); `name` is the preceding
    /// definition's name, if any, for the indentation note.
    UnexpectedEquals { region: Region, name: Option<String> },
    /// A top-level keyword (`module`/`import`/`type`/`port`) with leading spaces.
    TooMuchIndentation { region: Region, keyword: String },
    /// A record whose closing `}` is on a later line but under-indented; `region`
    /// spans from the record start to the brace, `highlight` marks the brace.
    NeedMoreIndentationRecord { region: Region, highlight: Region },
    /// A module whose declared name does not match its file path.
    ModuleNameMismatch {
        region: Region,
        expected: String,
        actual: String,
    },
    /// An `effect module` declaration outside the @elm organization.
    InvalidEffectModule { region: Region },
    /// A `[glsl| ... ` shader with no closing `|]`.
    EndlessShader { region: Region },
    /// A GLSL shader block that failed to parse; `message` is the vendored GLSL
    /// parser's error (alm uses a Rust parser, so the text differs from elm's).
    ShaderProblem { region: Region, message: String },
    /// An application `port module` that declares no ports.
    NoPorts { region: Region },
    /// A port declaration or `port module` in a package. `port_keyword` selects
    /// the "remove the port keyword" wording (header) vs "remove this port
    /// declaration" (a `port` value).
    PackagesCannotHavePorts { region: Region, port_keyword: bool },
}

/// The specific way a `\u{...}` unicode escape was malformed. Each maps to a
/// distinct body under the shared "BAD UNICODE ESCAPE" title.
#[derive(Debug, Clone)]
pub enum BadUnicode {
    /// Missing/misplaced curly braces around the code point.
    Format,
    /// The code point is empty or outside the `0..=0x10FFFF` range.
    Code,
    /// Fewer than four hex digits; `padded` is the 4-digit zero-padded form.
    TooShort { padded: String },
    /// More than six hex digits.
    TooLong,
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
            | SyntaxError::UnfinishedList { region }
            | SyntaxError::UnfinishedParens { region }
            | SyntaxError::EndlessString { region }
            | SyntaxError::UnfinishedLambda { region }
            | SyntaxError::UnfinishedRecord { region }
            | SyntaxError::RecordEquals { region }
            | SyntaxError::MissingExpression { region, .. }
            | SyntaxError::CaseOf { region }
            | SyntaxError::CaseArrow { region }
            | SyntaxError::LetProblem { region }
            | SyntaxError::WeirdDeclaration { region }
            | SyntaxError::DefBody { region, .. }
            | SyntaxError::DefEquals { region, .. }
            | SyntaxError::ExpectingModuleName { region }
            | SyntaxError::UnfinishedModuleDecl { region }
            | SyntaxError::UnfinishedExposing { region }
            | SyntaxError::UnfinishedImport { region }
            | SyntaxError::UnfinishedPort { region }
            | SyntaxError::UnexpectedPorts { region }
            | SyntaxError::EndlessComment { region }
            | SyntaxError::DefType { region, .. }
            | SyntaxError::TypeAliasBody { region }
            | SyntaxError::TypeAliasEquals { region }
            | SyntaxError::CustomEquals { region }
            | SyntaxError::CustomBar { region }
            | SyntaxError::ReservedWord { region, .. }
            | SyntaxError::UnfinishedCase { region }
            | SyntaxError::UnfinishedLet { region }
            | SyntaxError::UnfinishedTuple { region }
            | SyntaxError::OperatorFunction { region }
            | SyntaxError::RecordAccessor { region }
            | SyntaxError::ExpectingImportName { region }
            | SyntaxError::ExpectingImportAlias { region }
            | SyntaxError::ProblemInExposing { region }
            | SyntaxError::ExposingTypePrivacy { region }
            | SyntaxError::UnfinishedPortModule { region }
            | SyntaxError::NameMismatch { region, .. }
            | SyntaxError::ProblemInDefinition { region, .. } => *region,
            | SyntaxError::UnfinishedTupleType { region }
            | SyntaxError::UnfinishedRecordType { region }
            | SyntaxError::ProblemInRecordType { region }
            | SyntaxError::ExpectingTypeName { region }
            | SyntaxError::ExpectingTypeAliasName { region }
            | SyntaxError::ProblemInTypeAlias { region }
            | SyntaxError::ProblemInCustomType { region } => *region,
            | SyntaxError::PatternStart { region }
            | SyntaxError::PatternAlias { region }
            | SyntaxError::PatternFloat { region }
            | SyntaxError::ListPatternOpen { region }
            | SyntaxError::ListPatternEnd { region }
            | SyntaxError::ListPatternIndentEnd { region }
            | SyntaxError::ListPatternExpr { region }
            | SyntaxError::RecordPatternField { region }
            | SyntaxError::RecordPatternEnd { region }
            | SyntaxError::TuplePatternExpr { region } => *region,
            | SyntaxError::LeadingZeros { region }
            | SyntaxError::NumberDot { region }
            | SyntaxError::NumberEnd { region }
            | SyntaxError::UnknownEscape { region }
            | SyntaxError::BadUnicodeEscape { region, .. }
            | SyntaxError::NoTabs { region } => *region,
            | SyntaxError::UnexpectedCapital { region, .. }
            | SyntaxError::UnexpectedSymbolDecl { region, .. }
            | SyntaxError::UnexpectedSymbolMid { region }
            | SyntaxError::UnexpectedDollar { region }
            | SyntaxError::UnexpectedBacktick { region }
            | SyntaxError::UnexpectedSemicolon { region }
            | SyntaxError::UnexpectedComma { region }
            | SyntaxError::UnexpectedEquals { region, .. }
            | SyntaxError::TooMuchIndentation { region, .. }
            | SyntaxError::NeedMoreIndentationRecord { region, .. }
            | SyntaxError::ModuleNameMismatch { region, .. }
            | SyntaxError::InvalidEffectModule { region }
            | SyntaxError::EndlessShader { region }
            | SyntaxError::ShaderProblem { region, .. }
            | SyntaxError::NoPorts { region }
            | SyntaxError::PackagesCannotHavePorts { region, .. } => *region,
        }
    }

    /// Build the full diagnostic, matching `elm make`.
    pub fn to_report(&self) -> Report {
        match self {
            SyntaxError::IfThen { region } => snippet(
                "UNFINISHED IF",
                *region,
                "I was expecting to see more of this `if` expression, but I got stuck here:",
                marked("I was expecting to see the then keyword next.", &[("then", cyan)]),
                vec![],
            ),
            SyntaxError::IfElse { region } => snippet(
                "UNFINISHED IF",
                *region,
                "I was expecting to see an `else` branch after this:",
                marked(
                    "I know what to do when the condition is True, but what happens when it is \
                     False? Add an else branch to handle that scenario!",
                    &[("else", cyan)],
                ),
                vec![],
            ),
            SyntaxError::WeirdHex { region } => snippet(
                "WEIRD HEXIDECIMAL",
                *region,
                "I thought I was reading a hexidecimal number until I got here:",
                "Valid hexidecimal digits include 0123456789abcdefABCDEF, so I can only \
                 recognize things like this:",
                vec![example(&"    0x2B\n    0x002B\n    0x00ffb3".to_string())],
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
                    code_block(vec![code_line(
                        4,
                        vec![yellow("'this'"), Doc::text(" => "), green("\"this\"")],
                    )]),
                    note(words(
                        "Elm uses double quotes for strings like \"hello\", whereas it \
                         uses single quotes for individual characters like 'a' and 'ø'. This \
                         distinction helps with code like (String.any (\\c -> c == 'X') \
                         \"90210\") where you are inspecting individual characters."
                            ,
                    )),
                ],
            ),
            SyntaxError::UnfinishedList { region } => snippet(
                "UNFINISHED LIST",
                *region,
                "I cannot find the end of this list:",
                marked("You can just add a closing ] right here, and I will be all set!", &[("]", yellow)]),
                vec![
                    note(words(
                        "I may be confused by indentation. For example, if you are \
                         trying to define a list across multiple lines, I recommend using \
                         this format:"
                            ,
                    )),
                    code_block(vec![
                        code_line(4, vec![Doc::text("[ "), yellow("\"Alice\"")]),
                        code_line(4, vec![Doc::text(", "), yellow("\"Bob\"")]),
                        code_line(4, vec![Doc::text(", "), yellow("\"Chuck\"")]),
                        Doc::text("    ]"),
                    ]),
                    Section::para(
                        "Notice that each line starts with some indentation. Usually two or \
                         four spaces. This is the stylistic convention in the Elm ecosystem."
                            .to_string(),
                    ),
                ],
            ),
            SyntaxError::UnfinishedParens { region } => snippet(
                "UNFINISHED PARENTHESES",
                *region,
                "I was expecting to see a closing parenthesis next:",
                marked("Try adding a ) to see if that helps!", &[(")", yellow)]),
                vec![note(words(
                    "I can get confused by indentation in cases like this, so maybe \
                     you have a closing parenthesis but it is not indented enough?"
                        ,
                ))],
            ),
            SyntaxError::EndlessString { region } => snippet(
                "ENDLESS STRING",
                *region,
                "I got to the end of the line without seeing the closing double quote:",
                "Strings look like \"this\" with double quotes on each end. Is the closing \
                 double quote missing in your code?",
                vec![
                    note(words(
                        "For a string that spans multiple lines, you can use the \
                         multi-line string syntax like this:"
                            ,
                    )),
                    example(&
                        "    \"\"\"\n    # Multi-line Strings\n    \n    - start with triple \
                         double quotes\n    - write whatever you want\n    - no need to \
                         escape newlines or double quotes\n    - end with triple double \
                         quotes\n    \"\"\""
                            .to_string(),
                    ),
                ],
            ),
            SyntaxError::UnfinishedLambda { region } => snippet(
                "UNFINISHED ANONYMOUS FUNCTION",
                *region,
                "I just saw the beginning of an anonymous function, so I was expecting to \
                 see an arrow next:",
                marked(
                    "The syntax for anonymous functions is (\\x -> x + 1) so I am missing the \
                     arrow and the body of the function.",
                    &[("(\\x -> x + 1)", yellow)],
                ),
                // NB: elm's text contains the typo "indetation"; reproduced for byte-exact
                // output.
                vec![note(words(
                    "It is possible that I am confused about indetation! I generally \
                     recommend switching to named functions if the definition cannot fit \
                     inline nicely, so either (1) try to fit the whole anonymous function \
                     on one line or (2) break the whole thing out into a named function. \
                     Things tend to be clearer that way!"
                        ,
                ))],
            ),
            SyntaxError::UnfinishedRecord { region } => snippet(
                "UNFINISHED RECORD",
                *region,
                "I was partway through parsing a record, but I got stuck here:",
                marked(
                    "I was expecting to see a closing curly brace next. Try putting a } next \
                     and see if that helps?",
                    &[("}", green)],
                ),
                vec![
                    note(words(
                        "I may be confused by indentation. For example, if you are \
                         trying to define a record across multiple lines, I recommend using \
                         this format:"
                            ,
                    )),
                    record_example(),
                    Section::para(
                        "Notice that each line starts with some indentation. Usually two or \
                         four spaces. This is the stylistic convention in the Elm ecosystem!"
                            .to_string(),
                    ),
                ],
            ),
            SyntaxError::MissingExpression { region, op } => snippet_owned(
                "MISSING EXPRESSION".to_string(),
                *region,
                format!("I was expecting to see an expression after this {op} operator:"),
                marked(
                    "You can just put anything for now, like 42 or \"hello\". Once there is \
                     something there, I can probably give a more specific hint!",
                    &[("42", yellow), ("\"hello\"", yellow)],
                ),
                vec![note(words(&format!(
                    "I may be getting confused by your indentation? The easiest way to \
                     make sure this is not an indentation problem is to put the expression on \
                     the right of the {op} operator on the same line."
                )))],
            ),
            SyntaxError::CaseOf { region } => snippet(
                "UNEXPECTED ARROW",
                *region,
                "I am parsing a `case` expression right now, but this arrow is confusing me:",
                "Maybe the `of` keyword is missing on a previous line?",
                case_notes(),
            ),
            SyntaxError::CaseArrow { region } => snippet_spanned(
                "MISSING ARROW".to_string(),
                *region,
                Region::new(region.end, region.end),
                "I am partway through parsing a `case` expression, but I got stuck here:"
                    .to_string(),
                "I was expecting to see an arrow next.".to_string(),
                vec![
                    note(words(
                        "Sometimes I get confused by indentation, so try to make your \
                         `case` look something like this:"
                            ,
                    )),
                    case_example(),
                    Section::para(
                        "Notice the indentation! Patterns are aligned with each other. Same \
                         indentation. The expressions after each arrow are all indented a bit \
                         more than the patterns. That is important!"
                            .to_string(),
                    ),
                ],
            ),
            SyntaxError::LetProblem { region } => snippet_spanned(
                "LET PROBLEM".to_string(),
                *region,
                Region::new(region.end, region.end),
                "I was partway through parsing a `let` expression, but I got stuck here:"
                    .to_string(),
                marked(
                    "Based on the indentation, I was expecting to see the in keyword next. Is \
                     there a typo?",
                    &[("in", cyan)],
                ),
                vec![note(words(
                    "This can also happen if you are trying to define another value \
                     within the `let` but it is not indented enough. Make sure each \
                     definition has exactly the same amount of spaces before it. They should \
                     line up exactly!"
                        ,
                ))],
            ),
            SyntaxError::WeirdDeclaration { region } => snippet(
                "WEIRD DECLARATION",
                *region,
                "I am trying to parse a declaration, but I am getting stuck here:",
                "When a line has no spaces at the beginning, I expect it to be a declaration \
                 like one of these:",
                vec![
                    declaration_examples(),
                    Section::para(
                        "Try to make your declaration look like one of those? Or if this is \
                         not supposed to be a declaration, try adding some spaces before it?"
                            .to_string(),
                    ),
                ],
            ),
            SyntaxError::DefBody { region, name } => snippet_owned(
                "UNFINISHED DEFINITION".to_string(),
                *region,
                format!("I got stuck while parsing the `{name}` definition:"),
                "I was expecting to see an expression next. What is it equal to?".to_string(),
                def_notes(),
            ),
            SyntaxError::DefEquals { region, name } => snippet_owned(
                "UNFINISHED DEFINITION".to_string(),
                *region,
                format!("I got stuck while parsing the `{name}` definition:"),
                "I was expecting to see an argument or an equals sign next.".to_string(),
                def_notes(),
            ),
            SyntaxError::ExpectingModuleName { region } => snippet(
                "EXPECTING MODULE NAME",
                *region,
                "I was parsing an `module` declaration until I got stuck here:",
                "I was expecting to see the module name next, like in these examples:",
                vec![
                    example(&
                        "    module Dict exposing (..)\n    module Maybe exposing (..)\n    \
                         module Html.Attributes exposing (..)\n    module Json.Decode \
                         exposing (..)"
                            .to_string(),
                    ),
                    Section::para(
                        "Notice that the module names all start with capital letters. That is \
                         required!"
                            .to_string(),
                    ),
                ],
            ),
            SyntaxError::UnfinishedModuleDecl { region } => snippet(
                "UNFINISHED MODULE DECLARATION",
                *region,
                "I am parsing an `module` declaration, but I got stuck here:",
                "Here are some examples of valid `module` declarations:",
                vec![
                    example(&
                        "    module Main exposing (..)\n    module Dict exposing (Dict, empty, \
                         get)"
                            .to_string(),
                    ),
                    Section::para(
                        "I generally recommend using an explicit exposing list. I can skip \
                         compiling a bunch of files when the public interface of a module \
                         stays the same, so exposing fewer values can help improve compile \
                         times!"
                            .to_string(),
                    ),
                ],
            ),
            SyntaxError::UnfinishedExposing { region } => snippet(
                "UNFINISHED EXPOSING",
                *region,
                "I was partway through parsing exposed values, but I got stuck here:",
                marked(
                    "I was expecting a closing parenthesis. Try adding a ) right here?",
                    &[(")", green)],
                ),
                vec![note(words(
                    "I can get confused when there is not enough indentation, so if you \
                     already have a closing parenthesis, it probably just needs some spaces \
                     in front of it."
                        ,
                ))],
            ),
            SyntaxError::UnfinishedImport { region } => snippet(
                "UNFINISHED IMPORT",
                *region,
                "I am partway through parsing an import, but I got stuck here:",
                "Here are some examples of valid `import` declarations:",
                vec![
                    example(&
                        "    import Html\n    import Html as H\n    import Html as H exposing \
                         (..)\n    import Html exposing (Html, div, text)"
                            .to_string(),
                    ),
                    Section::para(
                        "You are probably trying to import a different module, but try to \
                         make it look like one of these examples!"
                            .to_string(),
                    ),
                    Section::para(
                        "Read <https://elm-lang.org/0.19.1/imports> to learn more.".to_string(),
                    ),
                ],
            ),
            SyntaxError::UnfinishedPort { region } => snippet(
                "UNFINISHED PORT",
                *region,
                "I just saw the start of a `port` declaration, but then I got stuck here:",
                "I was expecting to see a colon next. And then a type that tells me what type \
                 of values are going to flow through.",
                vec![
                    note(words(
                        "Here are some example `port` declarations for reference:"
                            ,
                    )),
                    example(&
                        "    port send : String -> Cmd msg\n    port receive : (String -> \
                         msg) -> Sub msg"
                            .to_string(),
                    ),
                    Section::para(
                        "The first line defines a `send` port so you can send strings out to \
                         JavaScript. Maybe you send them on a WebSocket or put them into \
                         IndexedDB. The second line defines a `receive` port so you can \
                         receive strings from JavaScript. Maybe you get receive messages when \
                         new WebSocket messages come in or when the IndexedDB is changed for \
                         some external reason."
                            .to_string(),
                    ),
                ],
            ),
            SyntaxError::UnexpectedPorts { region } => snippet(
                "UNEXPECTED PORTS",
                *region,
                "You are declaring ports in a normal module.",
                marked(
                    "Switch this to say port module instead, marking that this module contains \
                     port declarations.",
                    &[("port module", cyan)],
                ),
                vec![note(words(
                    "Ports are not a traditional FFI for calling JS functions directly. \
                     They need a different mindset! Read \
                     <https://elm-lang.org/0.19.1/ports> to learn the syntax and how to use \
                     it effectively."
                        ,
                ))],
            ),
            SyntaxError::EndlessComment { region } => snippet(
                "ENDLESS COMMENT",
                *region,
                "I cannot find the end of this multi-line comment:",
                "Add a -} somewhere after this to end the comment.",
                vec![hint(words(
                    "Multi-line comments can be nested in Elm, so {- {- -} -} is a \
                     comment that happens to contain another comment. Like parentheses and \
                     curly braces, the start and end markers must always be balanced. Maybe \
                     that is the problem?"
                        ,
                ))],
            ),
            SyntaxError::DefType { region, name } => snippet_owned(
                "UNFINISHED DEFINITION".to_string(),
                *region,
                format!("I got stuck while parsing the `{name}` type annotation:"),
                "I just saw a colon, so I am expecting to see a type next.".to_string(),
                def_notes(),
            ),
            SyntaxError::TypeAliasBody { region } => snippet(
                "UNFINISHED TYPE ALIAS",
                *region,
                "I am partway through parsing a type alias, but I got stuck here:",
                marked(
                    "I was expecting to see a type next. Something as simple as Int or Float \
                     would work!",
                    &[("Int", yellow), ("Float", yellow)],
                ),
                alias_notes(),
            ),
            SyntaxError::TypeAliasEquals { region } => snippet(
                "UNFINISHED TYPE ALIAS",
                *region,
                "I am partway through parsing a type alias, but I got stuck here:",
                "I was expecting to see a type variable or an equals sign next.",
                alias_notes(),
            ),
            SyntaxError::CustomEquals { region } => snippet(
                "UNFINISHED CUSTOM TYPE",
                *region,
                "I am partway through parsing a custom type, but I got stuck here:",
                "I just saw an equals sign, so I was expecting to see the first variant \
                 defined next.",
                custom_notes(),
            ),
            SyntaxError::CustomBar { region } => snippet(
                "UNFINISHED CUSTOM TYPE",
                *region,
                "I am partway through parsing a custom type, but I got stuck here:",
                "I just saw a vertical bar, so I was expecting to see another variant defined \
                 next.",
                custom_notes(),
            ),
            SyntaxError::ReservedWord { region, word } => snippet_owned(
                "RESERVED WORD".to_string(),
                *region,
                sentence(
                    [
                        words("The name"),
                        vec![Doc::concat(vec![
                            Doc::text("`"),
                            cyan(word.to_string()),
                            Doc::text("`"),
                        ])],
                        words("is reserved in Elm, so it cannot be used as an argument here:"),
                    ]
                    .concat(),
                ),
                "Try renaming it to something else.".to_string(),
                vec![note(words(&format!(
                    "The `{word}` keyword has a special meaning in Elm, so it can only \
                     be used in certain situations."
                )))],
            ),
            SyntaxError::LeadingZeros { region } => snippet(
                "LEADING ZEROS",
                *region,
                "I do not accept numbers with leading zeros:",
                "Just delete the leading zeros and it should work!",
                vec![note(words(
                    "Some languages let you to specify octal numbers by adding a leading \
                     zero. So in C, writing 0111 is the same as writing 73. Some people are used \
                     to that, but others probably want it to equal 111. Either path is going to \
                     surprise people from certain backgrounds, so Elm tries to avoid this whole \
                     situation."
                        ,
                ))],
            ),
            SyntaxError::NumberDot { region } => snippet(
                "WEIRD NUMBER",
                *region,
                "Numbers cannot end with a dot like this:",
                marked("Switching to 1 or 1.0 will work though!", &[("1", green), ("1.0", green)]),
                vec![],
            ),
            SyntaxError::NumberEnd { region } => snippet(
                "WEIRD NUMBER",
                *region,
                "I thought I was reading a number, but I ran into some weird stuff here:",
                "I recognize numbers in the following formats:",
                vec![
                    example(&"    42\n    3.14\n    6.022e23\n    0x002B".to_string()),
                    Section::para("So is there a way to write it like one of those?".to_string()),
                ],
            ),
            SyntaxError::UnknownEscape { region } => snippet(
                "UNKNOWN ESCAPE",
                *region,
                "Backslashes always start escaped characters, but I do not recognize this one:",
                "Valid escape characters include:",
                vec![
                    example(&
                        "    \\n\n    \\r\n    \\t\n    \\\"\n    \\'\n    \\\\\n    \\u{003D}"
                            .to_string(),
                    ),
                    Section::para(
                        "Do you want one of those instead? Maybe you need \\\\ to escape a \
                         backslash?"
                            .to_string(),
                    ),
                    note(words(
                        "The last style lets encode ANY character by its Unicode code \
                         point. That means \\u{0009} and \\t are the same. You can use that style \
                         for anything not covered by the other six escapes!"
                            ,
                    )),
                ],
            ),
            SyntaxError::BadUnicodeEscape { region, problem } => {
                let (before, after, notes): (&str, String, Vec<Section>) = match problem {
                    BadUnicode::Format => (
                        "I ran into an invalid Unicode escape:",
                        "Here are some examples of valid Unicode escapes:".to_string(),
                        vec![
                            example(&
                                "    \\u{0041}\n    \\u{03BB}\n    \\u{6728}\n    \\u{1F60A}"
                                    .to_string(),
                            ),
                            Section::para(
                                "Notice that the code point is always surrounded by curly braces. \
                                 Maybe you are missing the opening or closing curly brace?"
                                    .to_string(),
                            ),
                        ],
                    ),
                    BadUnicode::Code => (
                        "This is not a valid code point:",
                        "The valid code points are between 0 and 10FFFF inclusive.".to_string(),
                        vec![],
                    ),
                    BadUnicode::TooShort { padded } => (
                        "Every code point needs at least four digits:",
                        format!("Try \\u{{{padded}}} instead?"),
                        vec![],
                    ),
                    BadUnicode::TooLong => (
                        "This code point has too many digits:",
                        "Valid code points are between \\u{0000} and \\u{10FFFF}, so try trimming \
                         any leading zeros until you have between four and six digits."
                            .to_string(),
                        vec![],
                    ),
                };
                snippet_owned(
                    "BAD UNICODE ESCAPE".to_string(),
                    *region,
                    before.to_string(),
                    after,
                    notes,
                )
            }
            SyntaxError::NoTabs { region } => snippet(
                "NO TABS",
                *region,
                "I ran into a tab, but tabs are not allowed in Elm files.",
                "Replace the tab with spaces.",
                vec![],
            ),
            SyntaxError::RecordEquals { region } => snippet(
                "PROBLEM IN RECORD",
                *region,
                "I am partway through parsing a record, but I got stuck here:",
                marked(
                    "I just saw a field name, so I was expecting to see an equals sign next. So \
                     try putting an = sign here?",
                    &[("=", green)],
                ),
                vec![
                    note(words(
                        "If you are trying to define a record across multiple lines, I \
                         recommend using this format:"
                            ,
                    )),
                    record_example(),
                    Section::para(
                        "Notice that each line starts with some indentation. Usually two or \
                         four spaces. This is the stylistic convention in the Elm ecosystem."
                            .to_string(),
                    ),
                ],
            ),
            SyntaxError::UnfinishedCase { region } => snippet(
                "UNFINISHED CASE",
                *region,
                "I was partway through parsing a `case` expression, but I got stuck here:",
                "I was expecting to see a pattern next.",
                case_notes(),
            ),
            SyntaxError::UnfinishedLet { region } => snippet(
                "UNFINISHED LET",
                *region,
                "I was partway through parsing a `let` expression, but I got stuck here:",
                "I was expecting a value to be defined here.",
                vec![
                    note(words(
                        "Here is an example with a valid `let` expression for reference:"
                            ,
                    )),
                    example(&
                        "    viewPerson person =\n      let\n        fullName =\n          \
                         person.firstName ++ \" \" ++ person.lastName\n      in\n      div [] [ \
                         text fullName ]"
                            .to_string(),
                    ),
                    Section::para(
                        "Here we defined a `viewPerson` function that turns a person into some \
                         HTML. We use a `let` expression to define the `fullName` we want to \
                         show. Notice the indentation! The `fullName` is indented more than the \
                         `let` keyword, and the actual value of `fullName` is indented a bit \
                         more than that. That is important!"
                            .to_string(),
                    ),
                ],
            ),
            SyntaxError::UnfinishedTuple { region } => snippet(
                "UNFINISHED TUPLE",
                *region,
                "I think I am in the middle of parsing a tuple. I just saw a comma, so I was \
                 expecting to see an expression next.",
                "A tuple looks like (3,4) or (\"Tom\",42), so I think there is an expression \
                 missing here?",
                vec![note(words(
                    "I can get confused by indentation in cases like this, so maybe you \
                     have an expression but it is not indented enough?"
                        ,
                ))],
            ),
            SyntaxError::OperatorFunction { region } => snippet(
                "UNFINISHED OPERATOR FUNCTION",
                *region,
                "I was expecting a closing parenthesis here:",
                marked("Try adding a ) to see if that helps!", &[(")", yellow)]),
                vec![note(words(
                    "I think I am parsing an operator function right now, so I am \
                     expecting to see something like (+) or (&&) where an operator is \
                     surrounded by parentheses with no extra spaces."
                        ,
                ))],
            ),
            SyntaxError::RecordAccessor { region } => snippet(
                "EXPECTING RECORD ACCESSOR",
                *region,
                "I am trying to parse a record accessor here:",
                marked(
                    "Something like .name or .price that accesses a value from a record.",
                    &[(".name", yellow), (".price", yellow)],
                ),
                vec![note(words(
                    "Record field names must start with a lower case letter!",
                ))],
            ),
            SyntaxError::ExpectingImportName { region } => snippet(
                "EXPECTING IMPORT NAME",
                *region,
                "I was parsing an `import` until I got stuck here:",
                "I was expecting to see a module name next, like in these examples:",
                vec![
                    example(&
                        "    import Dict\n    import Maybe\n    import Html.Attributes as A\n    \
                         import Json.Decode exposing (..)"
                            .to_string(),
                    ),
                    Section::para(
                        "Notice that the module names all start with capital letters. That is \
                         required!"
                            .to_string(),
                    ),
                    Section::para(
                        "Read <https://elm-lang.org/0.19.1/imports> to learn more.".to_string(),
                    ),
                ],
            ),
            SyntaxError::ExpectingImportAlias { region } => snippet(
                "EXPECTING IMPORT ALIAS",
                *region,
                "I was parsing an `import` until I got stuck here:",
                "I was expecting to see an alias next, like in these examples:",
                vec![
                    example(&
                        "    import Html.Attributes as Attr\n    import WebGL.Texture as \
                         Texture\n    import Json.Decode as D"
                            .to_string(),
                    ),
                    Section::para(
                        "Notice that the alias always starts with a capital letter. That is \
                         required!"
                            .to_string(),
                    ),
                    Section::para(
                        "Read <https://elm-lang.org/0.19.1/imports> to learn more.".to_string(),
                    ),
                ],
            ),
            SyntaxError::ProblemInExposing { region } => snippet(
                "PROBLEM IN EXPOSING",
                *region,
                "I got stuck while parsing these exposed values:",
                "I do not have an exact recommendation, so here are some valid examples of \
                 `exposing` for reference:",
                vec![
                    example(&
                        "    import Html exposing (..)\n    import Basics exposing (Int, Float, \
                         Bool(..), (+), not, sqrt)"
                            .to_string(),
                    ),
                    Section::para(
                        "These examples show how to expose types, variants, operators, and \
                         functions. Everything should be some permutation of these examples, \
                         just with different names."
                            .to_string(),
                    ),
                ],
            ),
            SyntaxError::ExposingTypePrivacy { region } => snippet(
                "PROBLEM EXPOSING CUSTOM TYPE VARIANTS",
                *region,
                "It looks like you are trying to expose the variants of a custom type:",
                marked(
                    "You need to write something like Status(..) or Entity(..) though. It is all \
                     or nothing, otherwise `case` expressions could miss a variant and crash!",
                    &[("Status(..)", yellow), ("Entity(..)", yellow)],
                ),
                vec![note(words(
                    "It is often best to keep the variants hidden! If someone pattern \
                     matches on the variants, it is a MAJOR change if any new variants are \
                     added. Suddenly their `case` expressions do not cover all variants! So if \
                     you do not need people to pattern match, keep the variants hidden and \
                     expose functions to construct values of this type. This way you can add \
                     new variants as a MINOR change!"
                        ,
                ))],
            ),
            SyntaxError::UnfinishedPortModule { region } => snippet(
                "UNFINISHED PORT MODULE DECLARATION",
                *region,
                "I am parsing an `port module` declaration, but I got stuck here:",
                "Here are some examples of valid `port module` declarations:",
                vec![
                    example(&
                        "    port module WebSockets exposing (send, listen, keepAlive)\n    \
                         port module Maps exposing (Location, goto)"
                            .to_string(),
                    ),
                    note(words(
                        "Read <https://elm-lang.org/0.19.1/ports> for more help."
                            ,
                    )),
                ],
            ),
            SyntaxError::NameMismatch {
                region,
                highlight,
                annotation,
                definition,
            } => snippet_spanned(
                "NAME MISMATCH".to_string(),
                *region,
                *highlight,
                format!(
                    "I just saw a type annotation for `{annotation}`, but it is followed by a \
                     definition for `{definition}`:"
                ),
                "These names do not match! Is there a typo?".to_string(),
                vec![code_block(vec![code_line(
                    4,
                    vec![
                        yellow(definition.to_string()),
                        Doc::text(" -> "),
                        green(annotation.to_string()),
                    ],
                )])],
            ),
            SyntaxError::ProblemInDefinition { region, name } => snippet_owned(
                "PROBLEM IN DEFINITION".to_string(),
                *region,
                format!("I got stuck while parsing the `{name}` definition:"),
                "I am not sure what is going wrong exactly, so here is a valid definition (with \
                 an optional type annotation) for reference:"
                    .to_string(),
                vec![
                    def_example(),
                    Section::para("Try to use that format!".to_string()),
                ],
            ),
            SyntaxError::UnfinishedTupleType { region } => snippet(
                "UNFINISHED TUPLE TYPE",
                *region,
                "I think I am in the middle of parsing a tuple type. I just saw a comma, so \
                 I was expecting to see a type next.",
                "A tuple type looks like (Float,Float) or (String,Int), so I think there is \
                 a type missing here?",
                vec![note(words(
                    "I can get confused by indentation in cases like this, so maybe \
                     you have an expression but it is not indented enough?"
                        ,
                ))],
            ),
            SyntaxError::UnfinishedRecordType { region } => snippet(
                "UNFINISHED RECORD TYPE",
                *region,
                "I was partway through parsing a record type, but I got stuck here:",
                marked(
                    "I was expecting to see a closing curly brace next. Try putting a } next and \
                     see if that helps?",
                    &[("}", green)],
                ),
                record_type_indent_notes(),
            ),
            SyntaxError::ProblemInRecordType { region } => snippet(
                "PROBLEM IN RECORD TYPE",
                *region,
                "I am partway through parsing a record type, but I got stuck here:",
                marked(
                    "I was expecting to see another record field defined next, so I am looking \
                     for a name like userName or plantHeight.",
                    &[("userName", yellow), ("plantHeight", yellow)],
                ),
                record_type_notes(),
            ),
            SyntaxError::ExpectingTypeName { region } => snippet(
                "EXPECTING TYPE NAME",
                *region,
                "I think I am parsing a type declaration, but I got stuck here:",
                "I was expecting a name like Status or Style next. Just make sure it is a \
                 name that starts with a capital letter!",
                custom_notes(),
            ),
            SyntaxError::ExpectingTypeAliasName { region } => snippet(
                "EXPECTING TYPE ALIAS NAME",
                *region,
                "I am partway through parsing a type alias, but I got stuck here:",
                marked(
                    "I was expecting a name like Person or Point next. Just make sure it is a \
                     name that starts with a capital letter!",
                    &[("Person", yellow), ("Point", yellow)],
                ),
                alias_notes(),
            ),
            SyntaxError::ProblemInTypeAlias { region } => snippet(
                "PROBLEM IN TYPE ALIAS",
                *region,
                "I was partway through parsing a type alias, but I got stuck here:",
                marked(
                    "I was expecting to see a type next. Try putting Int or String for now?",
                    &[("Int", yellow), ("String", yellow)],
                ),
                vec![],
            ),
            SyntaxError::ProblemInCustomType { region } => snippet(
                "PROBLEM IN CUSTOM TYPE",
                *region,
                "I am partway through parsing a custom type, but I got stuck here:",
                marked(
                    "I was expecting to see a variant name next. Something like Success or \
                     Sandwich. Any name that starts with a capital letter really!",
                    &[("Success", yellow), ("Sandwich", yellow)],
                ),
                custom_notes(),
            ),
            SyntaxError::PatternStart { region } => snippet(
                "PROBLEM IN PATTERN",
                *region,
                "I wanted to parse a pattern next, but I got stuck here:",
                marked(
                    "I am not sure why I am getting stuck exactly. I just know that I want a \
                     pattern next. Something as simple as maybeHeight or result would work!",
                    &[("maybeHeight", yellow), ("result", yellow)],
                ),
                vec![],
            ),
            SyntaxError::PatternAlias { region } => snippet(
                "UNFINISHED PATTERN",
                *region,
                "I was expecting to see a variable name after the `as` keyword:",
                as_pattern_advice(),
                vec![Section::para(
                    "So I was expecting to see a variable name after the `as` keyword here. \
                     Sometimes people just want to use `as` as a variable name though. Try \
                     using a different name in that case!"
                        .to_string(),
                )],
            ),
            SyntaxError::PatternFloat { region } => snippet(
                "UNEXPECTED PATTERN",
                *region,
                "I cannot pattern match with floating point numbers:",
                marked(
                    "Equality on floats can be unreliable, so you usually want to check that \
                     they are nearby with some sort of (abs (actual - expected) < 0.001) check.",
                    &[("(abs (actual - expected) < 0.001)", yellow)],
                ),
                vec![],
            ),
            SyntaxError::ListPatternOpen { region } => snippet(
                "UNFINISHED LIST PATTERN",
                *region,
                "I just saw an open square bracket, but then I got stuck here:",
                marked("Try adding a ] to see if that helps?", &[("]", yellow)]),
                vec![note(words(
                    "I can get confused by indentation in cases like this, so maybe \
                     there is something next, but it is not indented enough?"
                        ,
                ))],
            ),
            SyntaxError::ListPatternEnd { region } => snippet(
                "UNFINISHED LIST PATTERN",
                *region,
                "I was expecting a closing square bracket to end this list pattern:",
                marked("Try adding a ] to see if that helps?", &[("]", yellow)]),
                vec![],
            ),
            SyntaxError::ListPatternIndentEnd { region } => snippet(
                "UNFINISHED LIST PATTERN",
                *region,
                "I was expecting a closing square bracket to end this list pattern:",
                marked("Try adding a ] to see if that helps?", &[("]", yellow)]),
                vec![note(words(
                    "I can get confused by indentation in cases like this, so maybe \
                     you have a closing square bracket but it is not indented enough?"
                        ,
                ))],
            ),
            SyntaxError::ListPatternExpr { region } => snippet(
                "UNFINISHED LIST PATTERN",
                *region,
                "I am partway through parsing a list pattern, but I got stuck here:",
                "I was expecting to see another pattern next. Maybe a variable name.",
                vec![note(words(
                    "I can get confused by indentation in cases like this, so maybe \
                     there is more to this pattern but it is not indented enough?"
                        ,
                ))],
            ),
            SyntaxError::RecordPatternField { region } => snippet(
                "UNFINISHED RECORD PATTERN",
                *region,
                "I was partway through parsing a record pattern, but I got stuck here:",
                "I was expecting to see a field name next.",
                vec![record_pattern_hint()],
            ),
            SyntaxError::RecordPatternEnd { region } => snippet(
                "UNFINISHED RECORD PATTERN",
                *region,
                "I was partway through parsing a record pattern, but I got stuck here:",
                marked(
                    "I was expecting to see a closing curly brace next. Try adding a } here?",
                    &[("}", yellow)],
                ),
                vec![record_pattern_hint()],
            ),
            SyntaxError::TuplePatternExpr { region } => snippet(
                "UNFINISHED TUPLE PATTERN",
                *region,
                "I am partway through parsing a tuple pattern, but I got stuck here:",
                "I was expecting to see a pattern next. I am expecting the final result to be \
                 something like (x,y) or (name, _).",
                vec![note(words(
                    "I can get confused by indentation in cases like this, so the \
                     problem may be that the next part is not indented enough?"
                        ,
                ))],
            ),
            SyntaxError::UnexpectedCapital { region, lower_name } => snippet_owned(
                "UNEXPECTED CAPITAL LETTER".to_string(),
                *region,
                "Declarations always start with a lower-case letter, so I am getting stuck \
                 here:"
                    .to_string(),
                marked(&format!("Try a name like {lower_name} instead?"), &[(&lower_name, green)]),
                vec![
                    note(words(
                        "Here are a couple valid declarations for reference:",
                    )),
                    declaration_examples(),
                    Section::para(
                        "Notice that they always start with a lower-case letter. \
                         Capitalization matters!"
                            .to_string(),
                    ),
                ],
            ),
            SyntaxError::UnexpectedSymbolDecl { region, symbol } => snippet_owned(
                "UNEXPECTED SYMBOL".to_string(),
                *region,
                format!("I am getting stuck because this line starts with the {symbol} symbol:"),
                "When a line has no spaces at the beginning, I expect it to be a declaration \
                 like one of these:"
                    .to_string(),
                vec![
                    declaration_examples(),
                    Section::para(
                        "If this is not supposed to be a declaration, try adding some spaces \
                         before it?"
                            .to_string(),
                    ),
                ],
            ),
            SyntaxError::UnexpectedSymbolMid { region } => snippet(
                "UNEXPECTED SYMBOL",
                *region,
                "I got stuck on this symbol:",
                "It is not used for anything in Elm syntax. Try removing it?",
                vec![],
            ),
            SyntaxError::UnexpectedDollar { region } => snippet(
                "UNEXPECTED SYMBOL",
                *region,
                "I got stuck on this dollar sign:",
                "It is not used for anything in Elm syntax. Are you coming from a language \
                 where dollar signs can be used in variable names? If so, try a name that (1) \
                 starts with a letter and (2) only contains letters, numbers, and underscores.",
                vec![],
            ),
            SyntaxError::UnexpectedBacktick { region } => snippet(
                "UNEXPECTED CHARACTER",
                *region,
                "I got stuck on this character:",
                "It is not used for anything in Elm syntax. It is used for multi-line strings \
                 in some languages though, so if you want a string that spans multiple lines, \
                 you can use Elm's multi-line string syntax like this:",
                vec![
                    example(&
                        "    \"\"\"\n    # Multi-line Strings\n    \n    - start with triple \
                         double quotes\n    - write whatever you want\n    - no need to \
                         escape newlines or double quotes\n    - end with triple double \
                         quotes\n    \"\"\""
                            .to_string(),
                    ),
                    Section::para(
                        "Otherwise I do not know what is going on! Try removing the character?"
                            .to_string(),
                    ),
                ],
            ),
            SyntaxError::UnexpectedSemicolon { region } => snippet(
                "UNEXPECTED SEMICOLON",
                *region,
                "I got stuck on this semicolon:",
                "Try removing it?",
                vec![note(words(
                    "Some languages require semicolons at the end of each statement. \
                     These are often called C-like languages, and they usually share a lot of \
                     language design choices. (E.g. side-effects, for loops, etc.) Elm manages \
                     effects with commands and subscriptions instead, so there is no special \
                     syntax for \"statements\" and therefore no need to use semicolons to \
                     separate them. I think this will make more sense as you work through \
                     <https://guide.elm-lang.org> though!"
                        ,
                ))],
            ),
            SyntaxError::UnexpectedComma { region } => snippet(
                "UNEXPECTED COMMA",
                *region,
                "I got stuck on this comma:",
                "I do not think I am parsing a list or tuple right now. Try deleting the comma?",
                vec![note(words(
                    "If this is supposed to be part of a list, the problem may be a bit \
                     earlier. Perhaps the opening [ is missing? Or perhaps some value in the \
                     list has an extra closing ] that is making me think the list ended \
                     earlier? The same kinds of things could be going wrong if this is \
                     supposed to be a tuple."
                        ,
                ))],
            ),
            SyntaxError::UnexpectedEquals { region, name } => {
                let note = match name {
                    Some(n) => format!(
                        "Note: I may be getting confused by your indentation. I think I am \
                         still parsing the `{n}` definition. Is this supposed to be part of a \
                         definition after that? If so, the problem may be a bit before the \
                         equals sign. I need all definitions to be indented exactly the same \
                         amount, so the problem may be that this new definition has too many \
                         spaces in front of it."
                    ),
                    None => "Note: I may be getting confused by your indentation. I need all \
                             definitions to be indented exactly the same amount, so if this is \
                             meant to be a new definition, it may have too many spaces in \
                             front of it."
                        .to_string(),
                };
                snippet_owned(
                    "UNEXPECTED EQUALS".to_string(),
                    *region,
                    "I was not expecting to see this equals sign:".to_string(),
                    "Maybe you want == instead? To check if two values are equal?".to_string(),
                    vec![Section::para(note)],
                )
            }
            SyntaxError::TooMuchIndentation { region, keyword } => snippet_owned(
                "TOO MUCH INDENTATION".to_string(),
                *region,
                format!("This `{keyword}` should not have any spaces before it:"),
                format!("Delete the spaces before `{keyword}` until there are none left!"),
                vec![],
            ),
            SyntaxError::ModuleNameMismatch {
                region,
                expected,
                actual,
            } => snippet_owned(
                "MODULE NAME MISMATCH".to_string(),
                *region,
                "It looks like this module name is out of sync:".to_string(),
                format!(
                    "I need it to match the file path, so I was expecting to see `{expected}` \
                     here. Make the following change, and you should be all set!"
                ),
                vec![
                    example(&format!("    {actual} -> {expected}")),
                    note(words(
                        "I require that module names correspond to file paths. This \
                         makes it much easier to explore unfamiliar codebases! So if you want \
                         to keep the current module name, try renaming the file instead."
                            ,
                    )),
                ],
            ),
            SyntaxError::InvalidEffectModule { region } => snippet(
                "INVALID EFFECT MODULE",
                *region,
                "It is not possible to declare an `effect module` outside the @elm \
                 organization, so I am getting stuck here:",
                "Switch to a normal module declaration.",
                vec![note(words(
                    "Effect modules are designed to allow certain core functionality to \
                     be defined separately from the compiler. So the @elm organization has \
                     access to this so that certain changes, extensions, and fixes can be \
                     introduced without needing to release new Elm binaries. For example, we \
                     want to make it possible to test effects, but this may require changes to \
                     the design of effect modules. By only having them defined in the @elm \
                     organization, that kind of design work can proceed much more smoothly."
                        ,
                ))],
            ),
            SyntaxError::EndlessShader { region } => snippet(
                "ENDLESS SHADER",
                *region,
                "I cannot find the end of this shader:",
                "Add a |] somewhere after this to end the shader.",
                vec![],
            ),
            SyntaxError::ShaderProblem { region, message } => {
                // Mirror elm's layout: the fixed wrapper text, then the vendored
                // parser's message indented by four spaces (blank lines dropped).
                let block = message
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .map(|l| format!("    {l}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                snippet_owned(
                    "SHADER PROBLEM".to_string(),
                    *region,
                    "I ran into a problem while parsing this GLSL block.".to_string(),
                    "I use a 3rd party GLSL parser for now, and I did my best to extract \
                     their error message:"
                        .to_string(),
                    vec![example(&block)],
                )
            }
            SyntaxError::NoPorts { region } => snippet(
                "NO PORTS",
                *region,
                "This module does not declare any ports, but it says it will:",
                marked("Switch this to module and you should be all set!", &[("module", cyan)]),
                vec![],
            ),
            SyntaxError::PackagesCannotHavePorts {
                region,
                port_keyword,
            } => snippet(
                "PACKAGES CANNOT HAVE PORTS",
                *region,
                "Packages cannot declare any ports, so I am getting stuck here:",
                if *port_keyword {
                    "Remove the port keyword and I should be able to continue."
                } else {
                    "Remove this port declaration."
                },
                vec![Section::para(PORTS_IN_PACKAGE_NOTE.to_string())],
            ),
            SyntaxError::NeedMoreIndentationRecord { region, highlight } => snippet_spanned(
                "NEED MORE INDENTATION".to_string(),
                *region,
                *highlight,
                "I was partway through parsing a record, but I got stuck here:".to_string(),
                "I need this curly brace to be indented more. Try adding some spaces before \
                 it!"
                    .to_string(),
                vec![
                    note(words(
                        "If you are trying to define a record across multiple lines, I \
                         recommend using this format:"
                            ,
                    )),
                    record_example(),
                    Section::para(
                        "Notice that each line starts with some indentation. Usually two or \
                         four spaces. This is the stylistic convention in the Elm ecosystem."
                            .to_string(),
                    ),
                ],
            ),
        }
    }
}

/// The example + note shared by the `case` diagnostics (UNEXPECTED ARROW and
/// UNFINISHED CASE).
fn case_notes() -> Vec<Section> {
    vec![
        note(words(
            "Here is an example of a valid `case` expression for reference.",
        )),
        case_example(),
        Section::para(
            "Notice the indentation. Each pattern is aligned, and each branch is indented a bit \
             more than the corresponding pattern. That is important!"
                .to_string(),
        ),
    ]
}
/// The hint shared by the UNFINISHED RECORD PATTERN errors.
fn record_pattern_hint() -> Section {
    hint(
        [
            words("A record pattern looks like"),
            vec![yellow("{x,y}"), Doc::text("or"), yellow("{name,age}")],
            words("where you list the field names you want to access."),
        ]
        .concat(),
    )
}

/// The note shared by both PACKAGES CANNOT HAVE PORTS diagnostics (two
/// paragraphs; `reflow` wraps each independently).
const PORTS_IN_PACKAGE_NOTE: &str = "Note: One of the major goals of the package ecosystem is to be completely written in Elm. This means when you install an Elm package, you can be sure you are safe from security issues on install and that you are not going to get any runtime exceptions coming from your new dependency. This design also sets the ecosystem up to target other platforms more easily (like mobile phones, WebAssembly, etc.) since no community code explicitly depends on JavaScript even existing.\n\nGiven that overall goal, allowing ports in packages would lead to some pretty surprising behavior. If ports were allowed in packages, you could install a package but not realize that it brings in an indirect dependency that defines a port. Now you have a program that does not work and the fix is to realize that some JavaScript needs to be added for a dependency you did not even know about. That would be extremely frustrating! \"So why not allow the package author to include the necessary JS code as well?\" Now we are back in conflict with our overall goal to keep all community packages free from runtime exceptions.";

/// An indented code example with Elm keywords colored, as elm hand-colors the
/// examples in its syntax diagnostics. Only keywords: constructors and literals
/// vary per example and are written out at the site that needs them.
fn example(text: &str) -> Section {
    code_block(text.split('\n').map(example_line).collect())
}

fn example_line(line: &str) -> Doc {
    const KEYWORDS: &[&str] = &[
        "module", "import", "exposing", "as", "port", "type", "alias", "case", "of", "if",
        "then", "else", "let", "in",
    ];
    let mut pieces: Vec<Doc> = Vec::new();
    let mut plain = String::new();
    let mut rest = line;
    while !rest.is_empty() {
        // Keep leading whitespace with the plain run.
        let word_start = rest.find(|c: char| !c.is_whitespace()).unwrap_or(rest.len());
        plain.push_str(&rest[..word_start]);
        rest = &rest[word_start..];
        if rest.is_empty() {
            break;
        }
        let word_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let (word, tail) = rest.split_at(word_end);
        match KEYWORDS.contains(&word).then_some(word) {
            Some(text) => {
                if !plain.is_empty() {
                    pieces.push(Doc::text(std::mem::take(&mut plain)));
                }
                pieces.push(cyan(text));
                rest = tail;
            }
            None => {
                plain.push_str(word);
                rest = tail;
            }
        }
    }
    if !plain.is_empty() {
        pieces.push(Doc::text(plain));
    }
    Doc::concat(pieces)
}

/// `((x,y) as point)` — elm colors the bound names yellow and `as` cyan, with
/// the punctuation between them left plain, so the pieces are written out.
fn as_pattern_advice() -> Doc {
    sentence(
        [
            words("The `as` keyword lets you write patterns like"),
            vec![
                Doc::concat(vec![
                    Doc::text("(("),
                    yellow("x"),
                    Doc::text(","),
                    yellow("y"),
                    Doc::text(")"),
                ]),
                Doc::concat(vec![cyan("as"), yellow(" point"), Doc::text(")")]),
            ],
            words("so you can refer to individual parts of the tuple with"),
            vec![yellow("x"), Doc::text("and"), yellow("y")],
            words("or you refer to the whole thing with"),
            vec![Doc::concat(vec![yellow("point"), Doc::text(".")])],
        ]
        .concat(),
    )
}

/// The multi-line record example elm shows in several record diagnostics.
fn record_example() -> Section {
    code_block(vec![
        code_line(4, vec![Doc::text("{ name = "), yellow("\"Alice\"")]),
        code_line(4, vec![Doc::text(", age = "), yellow("42")]),
        code_line(4, vec![Doc::text(", height = "), yellow("1.75")]),
        Doc::text("    }"),
    ])
}

/// The multi-line `case` example elm shows in several case diagnostics.
fn case_example() -> Section {
    code_block(vec![
        code_line(4, vec![cyan("case"), Doc::text(" maybeWidth "), cyan("of")]),
        code_line(6, vec![blue("Just"), Doc::text(" width ->")]),
        code_line(8, vec![Doc::text("width + "), yellow("200")]),
        Doc::text(""),
        code_line(6, vec![blue("Nothing"), Doc::text(" ->")]),
        code_line(8, vec![yellow("400")]),
    ])
}

/// The `greet` definition example elm shows in several declaration diagnostics.
fn def_example() -> Section {
    code_block(def_example_lines())
}

/// The `greet` definition plus a `type` declaration, shown together where elm
/// lists what a declaration can look like.
fn declaration_examples() -> Section {
    let mut lines = def_example_lines();
    lines.push(Doc::text("    "));
    lines.push(code_line(4, vec![cyan("type"), Doc::text(" User = Anonymous | LoggedIn String")]));
    code_block(lines)
}

fn def_example_lines() -> Vec<Doc> {
    vec![
        Doc::text("    greet : String -> String"),
        Doc::text("    greet name ="),
        code_line(6, vec![yellow("\"Hello \""), Doc::text(" ++ name ++ "), yellow("\"!\"")]),
    ]
}

/// The example + note shared by the UNFINISHED TYPE ALIAS errors.
fn alias_notes() -> Vec<Section> {
    vec![
        note(words(
            "Here is an example of a valid `type alias` for reference:",
        )),
        example(&
            "    type alias Person =\n      { name : String\n      , age : Int\n      , height \
             : Float\n      }"
                .to_string(),
        ),
        Section::para(
            "This would let us use `Person` as a shorthand for that record type. Using this \
             shorthand makes type annotations much easier to read, and makes changing code \
             easier if you decide later that there is more to a person than age and height!"
                .to_string(),
        ),
    ]
}

/// The example + note shared by the UNFINISHED CUSTOM TYPE errors.
fn custom_notes() -> Vec<Section> {
    vec![
        note(words(
            "Here is an example of a valid `type` declaration for reference:",
        )),
        example(&
            "    type Status\n      = Failure\n      | Waiting\n      | Success String"
                .to_string(),
        ),
        Section::para(
            "This defines a new `Status` type with three variants. This could be useful if we \
             are waiting for an HTTP request. Maybe we start with `Waiting` and then switch \
             to `Failure` or `Success \"message from server\"` depending on how things go. \
             Notice that the Success variant has some associated data, allowing us to store a \
             String if the request goes well!"
                .to_string(),
        ),
    ]
}

/// The example + type-annotation note shared by the UNFINISHED DEFINITION errors.
fn def_notes() -> Vec<Section> {
    vec![
        Section::para(
            "Here is a valid definition (with a type annotation) for reference:".to_string(),
        ),
        def_example(),
        Section::para(
            "The top line (called a \"type annotation\") is optional. You can leave it off if \
             you want. As you get more comfortable with Elm and as your project grows, it \
             becomes more and more valuable to add them though! They work great as \
             compiler-verified documentation, and they often improve error messages!"
                .to_string(),
        ),
    ]
}

/// The multi-line record-type example shown in the record-type diagnostics.
const RECORD_TYPE_EXAMPLE: &str =
    "    { name : String\n    , age : Int\n    , height : Float\n    }";

/// The trailing "Notice that each line..." paragraph shared by both record-type
/// notes.
fn record_type_notice() -> Section {
    Section::para(
        "Notice that each line starts with some indentation. Usually two or four spaces. \
         This is the stylistic convention in the Elm ecosystem."
            .to_string(),
    )
}

/// `noteForRecordTypeError`: shown when the parser is stuck on a definite token.
fn record_type_notes() -> Vec<Section> {
    vec![
        note(words(
            "If you are trying to define a record type across multiple lines, I \
             recommend using this format:"
                ,
        )),
        example(&RECORD_TYPE_EXAMPLE.to_string()),
        record_type_notice(),
    ]
}

/// `noteForRecordTypeIndentError`: shown when indentation may be the culprit.
fn record_type_indent_notes() -> Vec<Section> {
    vec![
        note(words(
            "I may be confused by indentation. For example, if you are trying to \
             define a record type across multiple lines, I recommend using this format:"
                ,
        )),
        example(&RECORD_TYPE_EXAMPLE.to_string()),
        record_type_notice(),
    ]
}

/// Build a `Report` from an elm snippet-style body. `before`/`after` take
/// either plain prose (reflowed) or an already-styled `Doc`.
fn snippet(
    title: &str,
    region: Region,
    before: impl Into<Doc>,
    after: impl Into<Doc>,
    notes: Vec<Section>,
) -> Report {
    snippet_owned(title.to_string(), region, before, after, notes)
}

/// As [`snippet`] but taking an owned title, for diagnostics whose heading is
/// built with runtime data (e.g. an operator name).
fn snippet_owned(
    title: String,
    region: Region,
    before: impl Into<Doc>,
    after: impl Into<Doc>,
    notes: Vec<Section>,
) -> Report {
    // Single-line errors: the shown region and the underline coincide.
    snippet_spanned(title, region, region, before, after, notes)
}

/// As [`snippet_owned`] but with a distinct `region` (lines to show) and
/// `highlight` (sub-region to underline), for multi-line diagnostics.
fn snippet_spanned(
    title: String,
    region: Region,
    highlight: Region,
    before: impl Into<Doc>,
    after: impl Into<Doc>,
    notes: Vec<Section>,
) -> Report {
    let (before, after) = (before.into(), after.into());
    Report {
        title,
        // A searchable summary (used by substring-based diagnostics tests); the
        // byte-exact layout lives in `elm` below. Rendered unwrapped so it stays
        // one line whatever the paragraph would do at 80 columns.
        message: format!("{} {}", before.render(usize::MAX), after.render(usize::MAX)),
        region,
        elm: Some(ElmBody { before, after, notes, region, highlight }),
    }
}
