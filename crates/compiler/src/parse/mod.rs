//! Port of `Parse.Primitives`, `Parse.Space`, `Parse.Variable`,
//! `Parse.Symbol`, `Parse.Number`, `Parse.String`, and `Parse.Keyword`.
//!
//! The Haskell parser is CPS-based with explicit commit semantics. Here we
//! use plain recursive descent with save/restore backtracking, and we track
//! the furthest error seen so failures point at the most useful spot.

pub mod expr;
pub mod module;
pub mod pattern;
pub mod type_;

use crate::data::Name;
use crate::reporting::syntax::SyntaxError;
use crate::reporting::{Located, Position, Region};

pub use module::parse_module;

#[derive(Debug, Clone)]
pub struct ParseError {
    pub message: String,
    pub region: Region,
    /// When present, render this parse error using the byte-exact elm catalogue
    /// (`Reporting.Error.Syntax`) instead of the legacy terse message.
    pub syntax: Option<SyntaxError>,
}

impl ParseError {
    pub fn new(message: impl Into<String>, region: Region) -> ParseError {
        ParseError {
            message: message.into(),
            region,
            syntax: None,
        }
    }

    /// A structured parse error carrying its exact elm report.
    pub fn from_syntax(e: SyntaxError) -> ParseError {
        ParseError {
            message: String::new(),
            region: e.region(),
            syntax: Some(e),
        }
    }
}

pub type PResult<T> = Result<T, ParseError>;

/// Which indentation check to run at the top of each iteration of
/// [`Parser::sep_until`]: some callers re-chomp whitespace first, others expect
/// the cursor to already be positioned on the next significant token.
pub enum IndentCheck {
    /// `chomp_and_check_indent`: consume whitespace, then require deeper indent.
    Chomp,
    /// `check_indent`: require deeper indent without consuming whitespace.
    NoChomp,
}

/// Mirrors `Parse.Primitives.State`: byte offset plus editor coordinates
/// plus the current indentation context.
#[derive(Debug, Clone, Copy)]
pub struct Snapshot {
    pos: usize,
    row: u32,
    col: u32,
    indent: u32,
}

pub struct Parser<'a> {
    src: &'a [u8],
    pub pos: usize,
    pub row: u32,
    pub col: u32,
    pub indent: u32,
}

impl<'a> Parser<'a> {
    pub fn new(src: &'a str) -> Parser<'a> {
        Parser {
            src: src.as_bytes(),
            pos: 0,
            row: 1,
            col: 1,
            indent: 1,
        }
    }

    // STATE

    pub fn save(&self) -> Snapshot {
        Snapshot {
            pos: self.pos,
            row: self.row,
            col: self.col,
            indent: self.indent,
        }
    }

    pub fn restore(&mut self, s: Snapshot) {
        self.pos = s.pos;
        self.row = s.row;
        self.col = s.col;
        self.indent = s.indent;
    }

    pub fn position(&self) -> Position {
        Position::new(self.row, self.col)
    }

    pub fn region_here(&self) -> Region {
        let p = self.position();
        Region::new(p, p)
    }

    pub fn error(&self, message: impl Into<String>) -> ParseError {
        ParseError::new(message, self.region_here())
    }

    pub fn is_at_end(&self) -> bool {
        self.pos >= self.src.len()
    }

    pub(crate) fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    pub(crate) fn peek_at(&self, offset: usize) -> Option<u8> {
        self.src.get(self.pos + offset).copied()
    }

    /// Decode the (possibly multi-byte) UTF-8 character at the cursor.
    pub(crate) fn peek_char(&self) -> Option<char> {
        self.char_at(0)
    }

    /// Decode the UTF-8 character starting `byte_offset` bytes past the cursor.
    pub(crate) fn char_at(&self, byte_offset: usize) -> Option<char> {
        let at = self.pos + byte_offset;
        let b = *self.src.get(at)?;
        let n = utf8_len(b);
        std::str::from_utf8(self.src.get(at..at + n)?)
            .ok()?
            .chars()
            .next()
    }

    /// True if the cursor is on a character that can start a lower identifier
    /// (variable/field): any alphabetic char that is not uppercase. Covers
    /// ASCII `a`-`z` and Unicode letters like `σ`.
    pub(crate) fn starts_lower(&self) -> bool {
        self.peek_char().is_some_and(is_lower_start)
    }

    /// True if the cursor is on a character that can start an upper identifier
    /// (type/constructor/module): any uppercase alphabetic char.
    pub(crate) fn starts_upper(&self) -> bool {
        self.peek_char().is_some_and(is_upper_start)
    }

    pub(crate) fn src_from_here(&self) -> &[u8] {
        &self.src[self.pos..]
    }

    /// Advance over a byte that is known not to be a newline.
    pub(crate) fn bump(&mut self, n: usize) {
        self.pos += n;
        self.col += n as u32;
    }

    fn bump_newline(&mut self) {
        self.pos += 1;
        self.row += 1;
        self.col = 1;
    }

    /// Advance over one char (multi-byte aware), not a newline.
    fn bump_char(&mut self) {
        if let Some(b) = self.peek() {
            let n = utf8_len(b);
            self.pos += n;
            self.col += 1;
        }
    }

    // EXACT MATCHES

    pub fn eat_byte(&mut self, byte: u8, what: &str) -> PResult<()> {
        if self.peek() == Some(byte) {
            self.bump(1);
            Ok(())
        } else {
            Err(self.error(format!("Expecting {}", what)))
        }
    }

    pub fn eat_word(&mut self, word: &str, what: &str) -> PResult<()> {
        if self.src[self.pos..].starts_with(word.as_bytes()) {
            self.bump(word.len());
            Ok(())
        } else {
            Err(self.error(format!("Expecting {}", what)))
        }
    }

    /// Whether the upcoming source is exactly `kw` as a keyword — matching the
    /// text and not followed by an identifier character, so `type_` and
    /// `typeToString` are not mistaken for the `type` keyword. Does not consume.
    pub(crate) fn peek_keyword(&self, kw: &str) -> bool {
        self.src[self.pos..].starts_with(kw.as_bytes())
            && !self.peek_at(kw.len()).is_some_and(is_inner_char)
    }

    /// Port of `Parse.Keyword`: the keyword must not be followed by an
    /// identifier character (`letting` is not `let`).
    pub fn keyword(&mut self, kw: &str) -> PResult<()> {
        if self.src[self.pos..].starts_with(kw.as_bytes())
            && !self
                .peek_at(kw.len())
                .is_some_and(|b| is_inner_char(b))
        {
            self.bump(kw.len());
            Ok(())
        } else {
            Err(self.error(format!("Expecting keyword `{}`", kw)))
        }
    }

    // SPACE — port of Parse.Space

    /// Chomp whitespace, line comments, and (nested) multi-line comments.
    pub fn chomp_space(&mut self) -> PResult<()> {
        loop {
            match self.peek() {
                Some(b' ') => self.bump(1),
                Some(b'\n') => self.bump_newline(),
                Some(b'\r') => {
                    self.pos += 1;
                }
                Some(b'\t') => {
                    return Err(self.error(
                        "I ran into a tab. Elm files must use spaces for indentation.",
                    ))
                }
                Some(b'-') if self.peek_at(1) == Some(b'-') => {
                    self.bump(2);
                    while let Some(b) = self.peek() {
                        if b == b'\n' {
                            break;
                        }
                        self.bump_char();
                    }
                }
                Some(b'{') if self.peek_at(1) == Some(b'-') => {
                    self.chomp_multi_comment()?;
                }
                _ => return Ok(()),
            }
        }
    }

    fn chomp_multi_comment(&mut self) -> PResult<()> {
        let start = self.region_here();
        self.bump(2);
        let mut depth = 1;
        loop {
            match self.peek() {
                None => {
                    return Err(ParseError::new(
                        "I got to the end of the file while looking for the closing `-}` of a multi-line comment.",
                        start,
                    ))
                }
                Some(b'\n') => self.bump_newline(),
                Some(b'-') if self.peek_at(1) == Some(b'}') => {
                    self.bump(2);
                    depth -= 1;
                    if depth == 0 {
                        return Ok(());
                    }
                }
                Some(b'{') if self.peek_at(1) == Some(b'-') => {
                    self.bump(2);
                    depth += 1;
                }
                _ => self.bump_char(),
            }
        }
    }

    /// `Space.chompAndCheckIndent`: whitespace, then require col > indent.
    pub fn chomp_and_check_indent(&mut self, problem: &str) -> PResult<()> {
        self.chomp_space()?;
        self.check_indent(problem)
    }

    /// `Space.checkIndent`: the next token must be more indented than the
    /// surrounding construct (or on the same line).
    pub fn check_indent(&mut self, problem: &str) -> PResult<()> {
        if self.col > self.indent {
            Ok(())
        } else {
            Err(self.error(problem.to_string()))
        }
    }

    /// `Space.checkAligned`: next token exactly at the current indent column.
    pub fn is_aligned(&self) -> bool {
        self.col == self.indent && !self.is_at_end()
    }

    /// `Space.checkFreshLine`: next token at column 1.
    pub fn check_fresh_line(&mut self, problem: &str) -> PResult<()> {
        if self.col == 1 {
            Ok(())
        } else {
            Err(self.error(problem.to_string()))
        }
    }

    /// Port of `withIndent`: run `f` with the indent set to the current column.
    pub fn with_indent<T>(&mut self, f: impl FnOnce(&mut Parser<'a>) -> PResult<T>) -> PResult<T> {
        let old = self.indent;
        self.indent = self.col;
        let result = f(self);
        self.indent = old;
        result
    }

    /// Port of `withBacksetIndent`: like `withIndent` but backed up N columns
    /// (used for `let` where defs align 3 columns after the keyword start).
    pub fn with_backset_indent<T>(
        &mut self,
        backset: u32,
        f: impl FnOnce(&mut Parser<'a>) -> PResult<T>,
    ) -> PResult<T> {
        let old = self.indent;
        self.indent = self.col.saturating_sub(backset);
        let result = f(self);
        self.indent = old;
        result
    }

    // VARIABLES — port of Parse.Variable

    pub fn lower_name(&mut self, what: &str) -> PResult<Name> {
        if self.starts_lower() {
            let name = self.chomp_inner_chars();
            if is_reserved(&name) {
                Err(self.error(format!(
                    "It looks like you are trying to use `{}` as a variable name, but it is a reserved word.",
                    name
                )))
            } else {
                Ok(Name::from(name))
            }
        } else {
            Err(self.error(format!("Expecting {}", what)))
        }
    }

    pub fn upper_name(&mut self, what: &str) -> PResult<Name> {
        if self.starts_upper() {
            let name = self.chomp_inner_chars();
            Ok(Name::from(name))
        } else {
            Err(self.error(format!("Expecting {}", what)))
        }
    }

    fn chomp_inner_chars(&mut self) -> String {
        let start = self.pos;
        while self.peek().is_some_and(is_inner_char) {
            self.bump(1);
        }
        String::from_utf8_lossy(&self.src[start..self.pos]).into_owned()
    }

    /// Port of `Var.foreignUpper` / `Var.foreignAlpha`: parse a possibly
    /// dotted name like `List.map`, `Json.Decode.field`, or `Maybe.Just`.
    /// Returns (qualifier, name, name_is_upper).
    pub fn qualified_name(&mut self, what: &str) -> PResult<(Option<Name>, Name, bool)> {
        if self.starts_lower() {
            let name = self.lower_name(what)?;
            return Ok((None, name, false));
        }
        match self.peek() {
            _ if self.starts_upper() => {
                let mut qualifier = String::new();
                loop {
                    let part = self.chomp_inner_chars();
                    if self.peek() == Some(b'.')
                        && self.char_at(1).is_some_and(|c| c.is_alphabetic())
                    {
                        self.bump(1); // the dot
                        if !qualifier.is_empty() {
                            qualifier.push('.');
                        }
                        qualifier.push_str(&part);
                        if self.starts_lower() {
                            let name = self.lower_name(what)?;
                            return Ok((Some(Name::from(qualifier)), name, false));
                        }
                        // else: another Upper segment, keep looping
                    } else {
                        let qual = if qualifier.is_empty() {
                            None
                        } else {
                            Some(Name::from(qualifier))
                        };
                        return Ok((qual, Name::from(part), true));
                    }
                }
            }
            _ => Err(self.error(format!("Expecting {}", what))),
        }
    }

    // OPERATORS — port of Parse.Symbol

    pub fn operator(&mut self) -> PResult<Name> {
        let start = self.pos;
        while self.peek().is_some_and(is_binop_char) {
            self.bump(1);
        }
        if start == self.pos {
            return Err(self.error("Expecting an operator"));
        }
        let op = String::from_utf8_lossy(&self.src[start..self.pos]).into_owned();
        match op.as_str() {
            "." | "|" | "->" | "=" | ":" | ".." => {
                Err(self.error(format!("The `{}` symbol is reserved here.", op)))
            }
            _ => Ok(Name::from(op)),
        }
    }

    // NUMBERS — port of Parse.Number

    pub fn number(&mut self) -> PResult<NumberLit> {
        let first = match self.peek() {
            Some(b) if b.is_ascii_digit() => b,
            _ => return Err(self.error("Expecting a number")),
        };
        if first == b'0' && self.peek_at(1) == Some(b'x') {
            self.bump(2);
            let start = self.pos;
            while self.peek().is_some_and(|b| b.is_ascii_hexdigit()) {
                self.bump(1);
            }
            if start == self.pos {
                return Err(ParseError::from_syntax(SyntaxError::WeirdHex {
                    region: self.region_here(),
                }));
            }
            let text = std::str::from_utf8(&self.src[start..self.pos]).unwrap();
            let n = i64::from_str_radix(text, 16)
                .map_err(|_| self.error("This hexadecimal number is out of range"))?;
            return Ok(NumberLit::Int(n));
        }

        let start = self.pos;
        while self.peek().is_some_and(|b| b.is_ascii_digit()) {
            self.bump(1);
        }
        let mut is_float = false;
        if self.peek() == Some(b'.') && self.peek_at(1).is_some_and(|b| b.is_ascii_digit()) {
            is_float = true;
            self.bump(1);
            while self.peek().is_some_and(|b| b.is_ascii_digit()) {
                self.bump(1);
            }
        }
        if self.peek() == Some(b'e') || self.peek() == Some(b'E') {
            let mut ahead = 1;
            if self.peek_at(ahead) == Some(b'+') || self.peek_at(ahead) == Some(b'-') {
                ahead += 1;
            }
            if self.peek_at(ahead).is_some_and(|b| b.is_ascii_digit()) {
                is_float = true;
                self.bump(ahead);
                while self.peek().is_some_and(|b| b.is_ascii_digit()) {
                    self.bump(1);
                }
            }
        }
        let text = std::str::from_utf8(&self.src[start..self.pos]).unwrap();
        if is_float {
            Ok(NumberLit::Float(text.parse().map_err(|_| {
                self.error("This is not a valid floating point number")
            })?))
        } else {
            Ok(NumberLit::Int(text.parse().map_err(|_| {
                self.error("This integer is out of the range I can represent")
            })?))
        }
    }

    // STRINGS — port of Parse.String

    /// Parse a `[glsl| ... |]` WebGL shader literal. The cursor is on `[`.
    /// Captures the raw GLSL source between `[glsl|` and `|]`, tracking
    /// newlines so later regions stay accurate.
    pub fn shader(&mut self) -> PResult<String> {
        let open = self.region_here();
        debug_assert!(self.src_from_here().starts_with(b"[glsl|"));
        self.bump(6); // `[glsl|`
        let start = self.pos;
        loop {
            match self.peek() {
                None => {
                    return Err(ParseError::new(
                        "I got to the end of the file without seeing the closing `|]` of this GLSL block.",
                        open,
                    ))
                }
                Some(b'|') if self.peek_at(1) == Some(b']') => {
                    let src = std::str::from_utf8(&self.src[start..self.pos])
                        .map_err(|_| self.error("This GLSL block contains invalid UTF-8"))?
                        .to_string();
                    self.bump(2); // `|]`
                    return Ok(src);
                }
                Some(b'\n') => self.bump_newline(),
                Some(b) => {
                    let n = utf8_len(b);
                    self.pos += n;
                    self.col += 1;
                }
            }
        }
    }

    pub fn string(&mut self) -> PResult<String> {
        if self.peek() != Some(b'"') {
            return Err(self.error("Expecting a string"));
        }
        if self.peek_at(1) == Some(b'"') && self.peek_at(2) == Some(b'"') {
            return self.multiline_string();
        }
        let open = self.region_here();
        self.bump(1);
        let mut out = String::new();
        loop {
            match self.peek() {
                None | Some(b'\n') => {
                    return Err(ParseError::new(
                        "I got to the end of the line without seeing the closing double quote of this string.",
                        open,
                    ))
                }
                Some(b'"') => {
                    self.bump(1);
                    return Ok(out);
                }
                Some(b'\\') => out.push(self.escape()?),
                Some(b) => {
                    let n = utf8_len(b);
                    out.push_str(std::str::from_utf8(&self.src[self.pos..self.pos + n]).map_err(
                        |_| self.error("This string contains invalid UTF-8"),
                    )?);
                    self.bump_char();
                }
            }
        }
    }

    fn multiline_string(&mut self) -> PResult<String> {
        let open = self.region_here();
        self.bump(3);
        let mut out = String::new();
        loop {
            match self.peek() {
                None => {
                    return Err(ParseError::new(
                        "I got to the end of the file without seeing the closing `\"\"\"` of this multi-line string.",
                        open,
                    ))
                }
                Some(b'"') if self.peek_at(1) == Some(b'"') && self.peek_at(2) == Some(b'"') => {
                    self.bump(3);
                    return Ok(out);
                }
                Some(b'\n') => {
                    out.push('\n');
                    self.bump_newline();
                }
                Some(b'\\') => out.push(self.escape()?),
                Some(b) => {
                    let n = utf8_len(b);
                    out.push_str(std::str::from_utf8(&self.src[self.pos..self.pos + n]).map_err(
                        |_| self.error("This string contains invalid UTF-8"),
                    )?);
                    self.bump_char();
                }
            }
        }
    }

    pub fn character(&mut self) -> PResult<char> {
        if self.peek() != Some(b'\'') {
            return Err(self.error("Expecting a character"));
        }
        let quote_start = self.position();
        self.bump(1);
        let c = match self.peek() {
            Some(b'\'') => {
                // `''` — elm points at both quotes and asks for double quotes.
                let end = Position::new(self.row, self.col + 1);
                return Err(ParseError::from_syntax(SyntaxError::CharDoubleQuotes {
                    region: Region::new(quote_start, end),
                }));
            }
            None | Some(b'\n') => {
                return Err(ParseError::from_syntax(SyntaxError::CharEnd {
                    region: self.region_here(),
                }))
            }
            Some(b'\\') => self.escape()?,
            Some(b) => {
                let n = utf8_len(b);
                let s = std::str::from_utf8(&self.src[self.pos..self.pos + n])
                    .map_err(|_| self.error("Invalid UTF-8 in character literal"))?;
                let c = s.chars().next().unwrap();
                self.bump_char();
                c
            }
        };
        self.eat_byte(b'\'', "the closing single quote of a character literal")
            .map_err(|_| {
                ParseError::from_syntax(SyntaxError::CharEnd {
                    region: self.region_here(),
                })
            })?;
        Ok(c)
    }

    fn escape(&mut self) -> PResult<char> {
        debug_assert_eq!(self.peek(), Some(b'\\'));
        self.bump(1);
        match self.peek() {
            Some(b'n') => {
                self.bump(1);
                Ok('\n')
            }
            Some(b'r') => {
                self.bump(1);
                Ok('\r')
            }
            Some(b't') => {
                self.bump(1);
                Ok('\t')
            }
            Some(b'"') => {
                self.bump(1);
                Ok('"')
            }
            Some(b'\'') => {
                self.bump(1);
                Ok('\'')
            }
            Some(b'\\') => {
                self.bump(1);
                Ok('\\')
            }
            Some(b'u') if self.peek_at(1) == Some(b'{') => {
                let code = self.unicode_escape_code()?;
                // Elm sources may write astral-plane characters as UTF-16
                // surrogate pairs: `\u{D835}\u{DD04}`. Combine a high surrogate
                // with a following low one into a single scalar value.
                if (0xD800..=0xDBFF).contains(&code)
                    && self.peek() == Some(b'\\')
                    && self.peek_at(1) == Some(b'u')
                    && self.peek_at(2) == Some(b'{')
                {
                    let (save_pos, save_col) = (self.pos, self.col);
                    self.bump(1); // the backslash
                    let low = self.unicode_escape_code()?;
                    if (0xDC00..=0xDFFF).contains(&low) {
                        let combined = 0x10000 + ((code - 0xD800) << 10) + (low - 0xDC00);
                        return char::from_u32(combined).ok_or_else(|| {
                            self.error("This surrogate pair is not a valid code point")
                        });
                    }
                    // The second escape was not a low surrogate; rewind so it
                    // is lexed on its own next iteration.
                    self.pos = save_pos;
                    self.col = save_col;
                }
                // A lone surrogate (high or low). Elm's strings are UTF-16 and
                // can hold these (e.g. the regex ranges in wolfadex/elm-ansi),
                // so smuggle it through as a private-use scalar; JS codegen
                // turns it back into a `\uXXXX` escape.
                if (0xD800..=0xDFFF).contains(&code) {
                    return Ok(encode_lone_surrogate(code));
                }
                char::from_u32(code)
                    .ok_or_else(|| self.error("This is not a valid unicode code point"))
            }
            _ => Err(self.error(
                "This is not a valid escape. Valid escapes are \\n, \\r, \\t, \\\", \\', \\\\, and \\u{003D}.",
            )),
        }
    }

    /// Parse `u{1F4A9}` (the parser is positioned at the `u`), returning
    /// the raw code point without validating it.
    fn unicode_escape_code(&mut self) -> PResult<u32> {
        debug_assert_eq!(self.peek(), Some(b'u'));
        self.bump(2); // `u{`
        let start = self.pos;
        while self.peek().is_some_and(|b| b.is_ascii_hexdigit()) {
            self.bump(1);
        }
        let text = std::str::from_utf8(&self.src[start..self.pos]).unwrap();
        let code = u32::from_str_radix(text, 16)
            .map_err(|_| self.error("Expecting hex digits in a `\\u{...}` escape"))?;
        self.eat_byte(b'}', "a closing `}` for this unicode escape")?;
        Ok(code)
    }

    // LOCATED HELPERS

    pub fn located<T>(
        &mut self,
        f: impl FnOnce(&mut Parser<'a>) -> PResult<T>,
    ) -> PResult<Located<T>> {
        let start = self.position();
        let value = f(self)?;
        let end = self.position();
        Ok(Located::at(start, end, value))
    }

    /// The shared comma-separated-list tail: repeatedly parse `item`s separated
    /// by `,` until the `close` byte, appending each to `items`. The first item
    /// is parsed by the caller; this drives the loop after it. On success the
    /// `close` byte has been consumed and the caller wraps `items` into its own
    /// AST node.
    pub fn sep_until<T>(
        &mut self,
        close: u8,
        top_check: IndentCheck,
        mut item: impl FnMut(&mut Parser<'a>) -> PResult<T>,
        items: &mut Vec<T>,
        middle_msg: &str,
        after_sep_msg: &str,
        close_or_sep_err: &str,
    ) -> PResult<()> {
        loop {
            match top_check {
                IndentCheck::Chomp => self.chomp_and_check_indent(middle_msg)?,
                IndentCheck::NoChomp => self.check_indent(middle_msg)?,
            }
            match self.peek() {
                Some(b',') => {
                    self.bump(1);
                    self.chomp_and_check_indent(after_sep_msg)?;
                    items.push(item(self)?);
                }
                Some(b) if b == close => {
                    self.bump(1);
                    return Ok(());
                }
                _ => return Err(self.error(close_or_sep_err.to_string())),
            }
        }
    }

    /// The shared closing loop for a parenthesized single value or tuple: the
    /// caller has already parsed `first`; this parses any `, item` repetitions
    /// up to the closing `)`. Returns the position just past the `)`, `first`
    /// back, and the remaining items (empty for a plain parenthesized value).
    /// `after_item` selects the indentation check applied after each subsequent
    /// item, matching the per-caller behaviour.
    pub fn chomp_tuple_items<T>(
        &mut self,
        first: T,
        mut item: impl FnMut(&mut Parser<'a>) -> PResult<T>,
        after_item: IndentCheck,
        after_sep_msg: &str,
        post_item_msg: &str,
        close_or_sep_err: &str,
    ) -> PResult<(Position, T, Vec<T>)> {
        let mut rest = Vec::new();
        loop {
            match self.peek() {
                Some(b',') => {
                    self.bump(1);
                    self.chomp_and_check_indent(after_sep_msg)?;
                    rest.push(item(self)?);
                    match after_item {
                        IndentCheck::Chomp => self.chomp_and_check_indent(post_item_msg)?,
                        IndentCheck::NoChomp => self.check_indent(post_item_msg)?,
                    }
                }
                Some(b')') => {
                    self.bump(1);
                    let end = self.position();
                    return Ok((end, first, rest));
                }
                _ => return Err(self.error(close_or_sep_err.to_string())),
            }
        }
    }

    /// Parse zero or more `item`s that follow on the same line or a deeper
    /// indent (argument application: `Ctor a b`, `List a b`, ...). Stops at the
    /// first item that fails to parse or that is not more indented than the
    /// current construct, restoring the cursor to before that item.
    pub fn chomp_indented_terms<T>(
        &mut self,
        mut item: impl FnMut(&mut Parser<'a>) -> PResult<T>,
    ) -> Vec<T> {
        let mut items = Vec::new();
        loop {
            let snapshot = self.save();
            if self.chomp_space().is_err() || self.col <= self.indent || self.is_at_end() {
                self.restore(snapshot);
                break;
            }
            match item(self) {
                Ok(arg) => items.push(arg),
                Err(_) => {
                    self.restore(snapshot);
                    break;
                }
            }
        }
        items
    }
}

#[derive(Debug, Clone, Copy)]
pub enum NumberLit {
    Int(i64),
    Float(f64),
}

/// Lone UTF-16 surrogate code points (U+D800..=U+DFFF) are not Unicode scalar
/// values, so they cannot be stored in a Rust `char`/`String`. Elm's strings
/// are UTF-16 and *can* hold them, so we smuggle each lone surrogate through
/// the compiler as a plane-16 private-use scalar (SPUA-B, U+10F800..=U+10FFFF).
/// JS codegen (`generate::js_string`) reverses the mapping and emits the
/// original surrogate as a `\uXXXX` escape, matching stock elm's output.
pub(crate) const SURROGATE_PUA_BASE: u32 = 0x10_F800;

/// Map a lone surrogate code point to its private-use stand-in scalar.
pub(crate) fn encode_lone_surrogate(code: u32) -> char {
    debug_assert!((0xD800..=0xDFFF).contains(&code));
    char::from_u32(SURROGATE_PUA_BASE + (code - 0xD800)).expect("valid private-use scalar")
}

/// Inverse of [`encode_lone_surrogate`]: if `c` stands in for a lone surrogate,
/// return the original surrogate code point (U+D800..=U+DFFF), else `None`.
pub(crate) fn decode_lone_surrogate(c: char) -> Option<u32> {
    let v = c as u32;
    (SURROGATE_PUA_BASE..=SURROGATE_PUA_BASE + 0x7FF).contains(&v).then(|| 0xD800 + (v - SURROGATE_PUA_BASE))
}

fn utf8_len(first_byte: u8) -> usize {
    match first_byte {
        b if b < 0x80 => 1,
        b if b < 0xE0 => 2,
        b if b < 0xF0 => 3,
        _ => 4,
    }
}

pub(crate) fn is_inner_char(b: u8) -> bool {
    // Any non-ASCII byte (>= 0x80) is a UTF-8 lead/continuation byte and is
    // treated as part of an identifier, so Unicode letters/digits are chomped.
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

/// A character that can start a lower identifier (variable/field name).
pub(crate) fn is_lower_start(c: char) -> bool {
    c.is_alphabetic() && !c.is_uppercase()
}

/// A character that can start an upper identifier (type/ctor/module name).
pub(crate) fn is_upper_start(c: char) -> bool {
    c.is_uppercase()
}

fn is_binop_char(b: u8) -> bool {
    matches!(
        b,
        b'+' | b'-' | b'/' | b'*' | b'=' | b'.' | b'<' | b'>' | b':' | b'&' | b'|' | b'^' | b'?' | b'%' | b'!'
    )
}

fn is_reserved(name: &str) -> bool {
    matches!(
        name,
        "if" | "then"
            | "else"
            | "case"
            | "of"
            | "let"
            | "in"
            | "type"
            | "module"
            | "where"
            | "import"
            | "exposing"
            | "as"
            | "port"
    )
}

/// Try alternatives in order, restoring parser state after each failure.
/// Reports the error that got the furthest, which is usually the useful one.
pub fn one_of<'a, T>(
    p: &mut Parser<'a>,
    alternatives: &mut [&mut dyn FnMut(&mut Parser<'a>) -> PResult<T>],
) -> PResult<T> {
    let snapshot = p.save();
    let mut best: Option<ParseError> = None;
    for alt in alternatives.iter_mut() {
        match alt(p) {
            Ok(value) => return Ok(value),
            Err(err) => {
                if best
                    .as_ref()
                    .is_none_or(|b| err.region.start > b.region.start)
                {
                    best = Some(err);
                }
                p.restore(snapshot);
            }
        }
    }
    Err(best.unwrap_or_else(|| p.error("Unexpected syntax")))
}
