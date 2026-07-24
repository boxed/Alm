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
            | SyntaxError::PatternStart { region } => *region,
            | SyntaxError::PatternStart { region }
            | SyntaxError::PatternAlias { region }
            | SyntaxError::PatternFloat { region }
            | SyntaxError::ListPatternOpen { region }
            | SyntaxError::ListPatternEnd { region }
            | SyntaxError::ListPatternIndentEnd { region }
            | SyntaxError::ListPatternExpr { region } => *region,
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
            SyntaxError::UnfinishedParens { region } => snippet(
                "UNFINISHED PARENTHESES",
                *region,
                "I was expecting to see a closing parenthesis next:",
                "Try adding a ) to see if that helps!",
                vec![Section::Para(
                    "Note: I can get confused by indentation in cases like this, so maybe \
                     you have a closing parenthesis but it is not indented enough?"
                        .to_string(),
                )],
            ),
            SyntaxError::EndlessString { region } => snippet(
                "ENDLESS STRING",
                *region,
                "I got to the end of the line without seeing the closing double quote:",
                "Strings look like \"this\" with double quotes on each end. Is the closing \
                 double quote missing in your code?",
                vec![
                    Section::Para(
                        "Note: For a string that spans multiple lines, you can use the \
                         multi-line string syntax like this:"
                            .to_string(),
                    ),
                    Section::Block(
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
                "The syntax for anonymous functions is (\\x -> x + 1) so I am missing the \
                 arrow and the body of the function.",
                // NB: elm's text contains the typo "indetation"; reproduced for byte-exact
                // output.
                vec![Section::Para(
                    "Note: It is possible that I am confused about indetation! I generally \
                     recommend switching to named functions if the definition cannot fit \
                     inline nicely, so either (1) try to fit the whole anonymous function \
                     on one line or (2) break the whole thing out into a named function. \
                     Things tend to be clearer that way!"
                        .to_string(),
                )],
            ),
            SyntaxError::UnfinishedRecord { region } => snippet(
                "UNFINISHED RECORD",
                *region,
                "I was partway through parsing a record, but I got stuck here:",
                "I was expecting to see a closing curly brace next. Try putting a } next \
                 and see if that helps?",
                vec![
                    Section::Para(
                        "Note: I may be confused by indentation. For example, if you are \
                         trying to define a record across multiple lines, I recommend using \
                         this format:"
                            .to_string(),
                    ),
                    Section::Block(RECORD_EXAMPLE.to_string()),
                    Section::Para(
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
                "You can just put anything for now, like 42 or \"hello\". Once there is \
                 something there, I can probably give a more specific hint!"
                    .to_string(),
                vec![Section::Para(format!(
                    "Note: I may be getting confused by your indentation? The easiest way to \
                     make sure this is not an indentation problem is to put the expression on \
                     the right of the {op} operator on the same line."
                ))],
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
                    Section::Para(
                        "Note: Sometimes I get confused by indentation, so try to make your \
                         `case` look something like this:"
                            .to_string(),
                    ),
                    Section::Block(CASE_EXAMPLE.to_string()),
                    Section::Para(
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
                "Based on the indentation, I was expecting to see the in keyword next. Is \
                 there a typo?"
                    .to_string(),
                vec![Section::Para(
                    "Note: This can also happen if you are trying to define another value \
                     within the `let` but it is not indented enough. Make sure each \
                     definition has exactly the same amount of spaces before it. They should \
                     line up exactly!"
                        .to_string(),
                )],
            ),
            SyntaxError::WeirdDeclaration { region } => snippet(
                "WEIRD DECLARATION",
                *region,
                "I am trying to parse a declaration, but I am getting stuck here:",
                "When a line has no spaces at the beginning, I expect it to be a declaration \
                 like one of these:",
                vec![
                    Section::Block(format!(
                        "{DEF_EXAMPLE}\n    \n    type User = Anonymous | LoggedIn String"
                    )),
                    Section::Para(
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
                    Section::Block(
                        "    module Dict exposing (..)\n    module Maybe exposing (..)\n    \
                         module Html.Attributes exposing (..)\n    module Json.Decode \
                         exposing (..)"
                            .to_string(),
                    ),
                    Section::Para(
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
                    Section::Block(
                        "    module Main exposing (..)\n    module Dict exposing (Dict, empty, \
                         get)"
                            .to_string(),
                    ),
                    Section::Para(
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
                "I was expecting a closing parenthesis. Try adding a ) right here?",
                vec![Section::Para(
                    "Note: I can get confused when there is not enough indentation, so if you \
                     already have a closing parenthesis, it probably just needs some spaces \
                     in front of it."
                        .to_string(),
                )],
            ),
            SyntaxError::UnfinishedImport { region } => snippet(
                "UNFINISHED IMPORT",
                *region,
                "I am partway through parsing an import, but I got stuck here:",
                "Here are some examples of valid `import` declarations:",
                vec![
                    Section::Block(
                        "    import Html\n    import Html as H\n    import Html as H exposing \
                         (..)\n    import Html exposing (Html, div, text)"
                            .to_string(),
                    ),
                    Section::Para(
                        "You are probably trying to import a different module, but try to \
                         make it look like one of these examples!"
                            .to_string(),
                    ),
                    Section::Para(
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
                    Section::Para(
                        "Note: Here are some example `port` declarations for reference:"
                            .to_string(),
                    ),
                    Section::Block(
                        "    port send : String -> Cmd msg\n    port receive : (String -> \
                         msg) -> Sub msg"
                            .to_string(),
                    ),
                    Section::Para(
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
                "Switch this to say port module instead, marking that this module contains \
                 port declarations.",
                vec![Section::Para(
                    "Note: Ports are not a traditional FFI for calling JS functions directly. \
                     They need a different mindset! Read \
                     <https://elm-lang.org/0.19.1/ports> to learn the syntax and how to use \
                     it effectively."
                        .to_string(),
                )],
            ),
            SyntaxError::EndlessComment { region } => snippet(
                "ENDLESS COMMENT",
                *region,
                "I cannot find the end of this multi-line comment:",
                "Add a -} somewhere after this to end the comment.",
                vec![Section::Para(
                    "Hint: Multi-line comments can be nested in Elm, so {- {- -} -} is a \
                     comment that happens to contain another comment. Like parentheses and \
                     curly braces, the start and end markers must always be balanced. Maybe \
                     that is the problem?"
                        .to_string(),
                )],
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
                "I was expecting to see a type next. Something as simple as Int or Float \
                 would work!",
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
                format!(
                    "The name `{word}` is reserved in Elm, so it cannot be used as an \
                     argument here:"
                ),
                "Try renaming it to something else.".to_string(),
                vec![Section::Para(format!(
                    "Note: The `{word}` keyword has a special meaning in Elm, so it can only \
                     be used in certain situations."
                ))],
            ),
            SyntaxError::RecordEquals { region } => snippet(
                "PROBLEM IN RECORD",
                *region,
                "I am partway through parsing a record, but I got stuck here:",
                "I just saw a field name, so I was expecting to see an equals sign next. So \
                 try putting an = sign here?",
                vec![
                    Section::Para(
                        "Note: If you are trying to define a record across multiple lines, I \
                         recommend using this format:"
                            .to_string(),
                    ),
                    Section::Block(RECORD_EXAMPLE.to_string()),
                    Section::Para(
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
                    Section::Para(
                        "Note: Here is an example with a valid `let` expression for reference:"
                            .to_string(),
                    ),
                    Section::Block(
                        "    viewPerson person =\n      let\n        fullName =\n          \
                         person.firstName ++ \" \" ++ person.lastName\n      in\n      div [] [ \
                         text fullName ]"
                            .to_string(),
                    ),
                    Section::Para(
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
                vec![Section::Para(
                    "Note: I can get confused by indentation in cases like this, so maybe you \
                     have an expression but it is not indented enough?"
                        .to_string(),
                )],
            ),
            SyntaxError::OperatorFunction { region } => snippet(
                "UNFINISHED OPERATOR FUNCTION",
                *region,
                "I was expecting a closing parenthesis here:",
                "Try adding a ) to see if that helps!",
                vec![Section::Para(
                    "Note: I think I am parsing an operator function right now, so I am \
                     expecting to see something like (+) or (&&) where an operator is \
                     surrounded by parentheses with no extra spaces."
                        .to_string(),
                )],
            ),
            SyntaxError::RecordAccessor { region } => snippet(
                "EXPECTING RECORD ACCESSOR",
                *region,
                "I am trying to parse a record accessor here:",
                "Something like .name or .price that accesses a value from a record.",
                vec![Section::Para(
                    "Note: Record field names must start with a lower case letter!".to_string(),
                )],
            ),
            SyntaxError::ExpectingImportName { region } => snippet(
                "EXPECTING IMPORT NAME",
                *region,
                "I was parsing an `import` until I got stuck here:",
                "I was expecting to see a module name next, like in these examples:",
                vec![
                    Section::Block(
                        "    import Dict\n    import Maybe\n    import Html.Attributes as A\n    \
                         import Json.Decode exposing (..)"
                            .to_string(),
                    ),
                    Section::Para(
                        "Notice that the module names all start with capital letters. That is \
                         required!"
                            .to_string(),
                    ),
                    Section::Para(
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
                    Section::Block(
                        "    import Html.Attributes as Attr\n    import WebGL.Texture as \
                         Texture\n    import Json.Decode as D"
                            .to_string(),
                    ),
                    Section::Para(
                        "Notice that the alias always starts with a capital letter. That is \
                         required!"
                            .to_string(),
                    ),
                    Section::Para(
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
                    Section::Block(
                        "    import Html exposing (..)\n    import Basics exposing (Int, Float, \
                         Bool(..), (+), not, sqrt)"
                            .to_string(),
                    ),
                    Section::Para(
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
                "You need to write something like Status(..) or Entity(..) though. It is all or \
                 nothing, otherwise `case` expressions could miss a variant and crash!",
                vec![Section::Para(
                    "Note: It is often best to keep the variants hidden! If someone pattern \
                     matches on the variants, it is a MAJOR change if any new variants are \
                     added. Suddenly their `case` expressions do not cover all variants! So if \
                     you do not need people to pattern match, keep the variants hidden and \
                     expose functions to construct values of this type. This way you can add \
                     new variants as a MINOR change!"
                        .to_string(),
                )],
            ),
            SyntaxError::UnfinishedPortModule { region } => snippet(
                "UNFINISHED PORT MODULE DECLARATION",
                *region,
                "I am parsing an `port module` declaration, but I got stuck here:",
                "Here are some examples of valid `port module` declarations:",
                vec![
                    Section::Block(
                        "    port module WebSockets exposing (send, listen, keepAlive)\n    \
                         port module Maps exposing (Location, goto)"
                            .to_string(),
                    ),
                    Section::Para(
                        "Note: Read <https://elm-lang.org/0.19.1/ports> for more help."
                            .to_string(),
                    ),
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
                vec![Section::Block(format!("    {definition} -> {annotation}"))],
            ),
            SyntaxError::ProblemInDefinition { region, name } => snippet_owned(
                "PROBLEM IN DEFINITION".to_string(),
                *region,
                format!("I got stuck while parsing the `{name}` definition:"),
                "I am not sure what is going wrong exactly, so here is a valid definition (with \
                 an optional type annotation) for reference:"
                    .to_string(),
                vec![
                    Section::Block(DEF_EXAMPLE.to_string()),
                    Section::Para("Try to use that format!".to_string()),
                ],
            ),
            SyntaxError::UnfinishedTupleType { region } => snippet(
                "UNFINISHED TUPLE TYPE",
                *region,
                "I think I am in the middle of parsing a tuple type. I just saw a comma, so \
                 I was expecting to see a type next.",
                "A tuple type looks like (Float,Float) or (String,Int), so I think there is \
                 a type missing here?",
                vec![Section::Para(
                    "Note: I can get confused by indentation in cases like this, so maybe \
                     you have an expression but it is not indented enough?"
                        .to_string(),
                )],
            ),
            SyntaxError::UnfinishedRecordType { region } => snippet(
                "UNFINISHED RECORD TYPE",
                *region,
                "I was partway through parsing a record type, but I got stuck here:",
                "I was expecting to see a closing curly brace next. Try putting a } next and \
                 see if that helps?",
                record_type_indent_notes(),
            ),
            SyntaxError::ProblemInRecordType { region } => snippet(
                "PROBLEM IN RECORD TYPE",
                *region,
                "I am partway through parsing a record type, but I got stuck here:",
                "I was expecting to see another record field defined next, so I am looking \
                 for a name like userName or plantHeight.",
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
                "I was expecting a name like Person or Point next. Just make sure it is a \
                 name that starts with a capital letter!",
                alias_notes(),
            ),
            SyntaxError::ProblemInTypeAlias { region } => snippet(
                "PROBLEM IN TYPE ALIAS",
                *region,
                "I was partway through parsing a type alias, but I got stuck here:",
                "I was expecting to see a type next. Try putting Int or String for now?",
                vec![],
            ),
            SyntaxError::ProblemInCustomType { region } => snippet(
                "PROBLEM IN CUSTOM TYPE",
                *region,
                "I am partway through parsing a custom type, but I got stuck here:",
                "I was expecting to see a variant name next. Something like Success or \
                 Sandwich. Any name that starts with a capital letter really!",
                custom_notes(),
            ),
            SyntaxError::PatternStart { region } => snippet(
                "PROBLEM IN PATTERN",
                *region,
                "I wanted to parse a pattern next, but I got stuck here:",
                "I am not sure why I am getting stuck exactly. I just know that I want a \
                 pattern next. Something as simple as maybeHeight or result would work!",
                vec![],
            ),
            SyntaxError::PatternAlias { region } => snippet(
                "UNFINISHED PATTERN",
                *region,
                "I was expecting to see a variable name after the `as` keyword:",
                "The `as` keyword lets you write patterns like ((x,y) as point) so you can \
                 refer to individual parts of the tuple with x and y or you refer to the \
                 whole thing with point.",
                vec![Section::Para(
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
                "Equality on floats can be unreliable, so you usually want to check that they \
                 are nearby with some sort of (abs (actual - expected) < 0.001) check.",
                vec![],
            ),
            SyntaxError::ListPatternOpen { region } => snippet(
                "UNFINISHED LIST PATTERN",
                *region,
                "I just saw an open square bracket, but then I got stuck here:",
                "Try adding a ] to see if that helps?",
                vec![Section::Para(
                    "Note: I can get confused by indentation in cases like this, so maybe \
                     there is something next, but it is not indented enough?"
                        .to_string(),
                )],
            ),
            SyntaxError::ListPatternEnd { region } => snippet(
                "UNFINISHED LIST PATTERN",
                *region,
                "I was expecting a closing square bracket to end this list pattern:",
                "Try adding a ] to see if that helps?",
                vec![],
            ),
            SyntaxError::ListPatternIndentEnd { region } => snippet(
                "UNFINISHED LIST PATTERN",
                *region,
                "I was expecting a closing square bracket to end this list pattern:",
                "Try adding a ] to see if that helps?",
                vec![Section::Para(
                    "Note: I can get confused by indentation in cases like this, so maybe \
                     you have a closing square bracket but it is not indented enough?"
                        .to_string(),
                )],
            ),
            SyntaxError::ListPatternExpr { region } => snippet(
                "UNFINISHED LIST PATTERN",
                *region,
                "I am partway through parsing a list pattern, but I got stuck here:",
                "I was expecting to see another pattern next. Maybe a variable name.",
                vec![Section::Para(
                    "Note: I can get confused by indentation in cases like this, so maybe \
                     there is more to this pattern but it is not indented enough?"
                        .to_string(),
                )],
            ),
        }
    }
}

/// The example + note shared by the `case` diagnostics (UNEXPECTED ARROW and
/// UNFINISHED CASE).
fn case_notes() -> Vec<Section> {
    vec![
        Section::Para(
            "Note: Here is an example of a valid `case` expression for reference.".to_string(),
        ),
        Section::Block(CASE_EXAMPLE.to_string()),
        Section::Para(
            "Notice the indentation. Each pattern is aligned, and each branch is indented a bit \
             more than the corresponding pattern. That is important!"
                .to_string(),
        ),
    ]
}

/// The multi-line record example elm shows in several record diagnostics.
const RECORD_EXAMPLE: &str =
    "    { name = \"Alice\"\n    , age = 42\n    , height = 1.75\n    }";

/// The multi-line `case` example elm shows in several case diagnostics.
const CASE_EXAMPLE: &str = "    case maybeWidth of\n      Just width ->\n        width + 200\n\n      Nothing ->\n        400";

/// The `greet` definition example elm shows in several declaration diagnostics.
const DEF_EXAMPLE: &str =
    "    greet : String -> String\n    greet name =\n      \"Hello \" ++ name ++ \"!\"";

/// The example + note shared by the UNFINISHED TYPE ALIAS errors.
fn alias_notes() -> Vec<Section> {
    vec![
        Section::Para(
            "Note: Here is an example of a valid `type alias` for reference:".to_string(),
        ),
        Section::Block(
            "    type alias Person =\n      { name : String\n      , age : Int\n      , height \
             : Float\n      }"
                .to_string(),
        ),
        Section::Para(
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
        Section::Para(
            "Note: Here is an example of a valid `type` declaration for reference:".to_string(),
        ),
        Section::Block(
            "    type Status\n      = Failure\n      | Waiting\n      | Success String"
                .to_string(),
        ),
        Section::Para(
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
        Section::Para(
            "Here is a valid definition (with a type annotation) for reference:".to_string(),
        ),
        Section::Block(DEF_EXAMPLE.to_string()),
        Section::Para(
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
    Section::Para(
        "Notice that each line starts with some indentation. Usually two or four spaces. \
         This is the stylistic convention in the Elm ecosystem."
            .to_string(),
    )
}

/// `noteForRecordTypeError`: shown when the parser is stuck on a definite token.
fn record_type_notes() -> Vec<Section> {
    vec![
        Section::Para(
            "Note: If you are trying to define a record type across multiple lines, I \
             recommend using this format:"
                .to_string(),
        ),
        Section::Block(RECORD_TYPE_EXAMPLE.to_string()),
        record_type_notice(),
    ]
}

/// `noteForRecordTypeIndentError`: shown when indentation may be the culprit.
fn record_type_indent_notes() -> Vec<Section> {
    vec![
        Section::Para(
            "Note: I may be confused by indentation. For example, if you are trying to \
             define a record type across multiple lines, I recommend using this format:"
                .to_string(),
        ),
        Section::Block(RECORD_TYPE_EXAMPLE.to_string()),
        record_type_notice(),
    ]
}

/// Build a `Report` from an elm snippet-style body.
fn snippet(title: &str, region: Region, before: &str, after: &str, notes: Vec<Section>) -> Report {
    snippet_owned(
        title.to_string(),
        region,
        before.to_string(),
        after.to_string(),
        notes,
    )
}

/// As [`snippet`] but taking owned strings, for diagnostics whose text is built
/// with runtime data (e.g. an operator name).
fn snippet_owned(
    title: String,
    region: Region,
    before: String,
    after: String,
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
    before: String,
    after: String,
    notes: Vec<Section>,
) -> Report {
    Report {
        title,
        region,
        // A searchable summary (used by substring-based diagnostics tests); the
        // byte-exact layout lives in `elm` below.
        message: format!("{before} {after}"),
        elm: Some(ElmBody {
            before,
            after,
            notes,
            region,
            highlight,
        }),
    }
}
