//! Strip comments from a generated JavaScript bundle.
//!
//! Most of alm's bundle is the hand-written kernel (`runtime.js`), which is
//! commented the way source is meant to be — hundreds of lines explaining why
//! the vdom does what it does. That is right for the kernel and wrong for what
//! `alm make --optimize` ships, so a production build runs the assembled bundle
//! through here.
//!
//! This is a scanner, not a parser: it walks the bundle a character at a time,
//! copying strings, template literals and regex literals through verbatim so a
//! `//` inside `'http://…'` or a `/*` inside a regex is never mistaken for a
//! comment. The one genuinely ambiguous character in JavaScript's lexical
//! grammar is `/` — division or the start of a regex literal — and that is
//! decided from the previous token, exactly as a real lexer does.
//!
//! Legal notices stay: a block comment carrying a copyright or license line is
//! copied through untouched (the bundled `marked` build is MIT and says so).

/// The bundle with its comments removed.
///
/// Lines left empty by a removed comment go away with it; a comment between two
/// tokens on one line becomes a single space, so nothing is ever glued
/// together. Everything else — including every line break that a semicolon-less
/// statement might depend on — is preserved character for character.
pub fn strip(source: &str) -> String {
    let chars: Vec<char> = source.chars().collect();
    let mut out: Vec<char> = Vec::with_capacity(chars.len());
    scan(&chars, 0, &mut out, false);
    out.into_iter().collect()
}

/// Copy `chars[from..]` into `out` without comments, stopping at the `}` that
/// closes the enclosing `${` when `in_substitution`. Returns where it stopped.
///
/// Template substitutions re-enter here rather than being copied blindly,
/// because a `${…}` holds ordinary code: braces, strings and comments alike.
fn scan(chars: &[char], from: usize, out: &mut Vec<char>, in_substitution: bool) -> usize {
    let mut i = from;
    // Braces opened since the substitution began, so the `}` that ends it can
    // be told from one that merely closes an object literal inside it.
    let mut depth = 0usize;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '}' if in_substitution && depth == 0 => return i,
            '}' if in_substitution => {
                depth -= 1;
                out.push(c);
                i += 1;
            }
            '{' if in_substitution => {
                depth += 1;
                out.push(c);
                i += 1;
            }
            '\n' => {
                end_line(out);
                i += 1;
            }
            '\'' | '"' => i = copy_string(chars, i, out),
            '`' => i = copy_template(chars, i, out),
            '/' if chars.get(i + 1) == Some(&'/') => {
                // A line comment runs to the newline, which the next turn of
                // the loop handles — so the line break itself survives.
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '/' if chars.get(i + 1) == Some(&'*') => i = drop_block(chars, i, out),
            '/' if regex_allowed(out) => i = copy_regex(chars, i, out),
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    i
}

/// Finish the current output line: drop the trailing space a removed comment
/// left behind, and drop the line break too if nothing else is on the line.
fn end_line(out: &mut Vec<char>) {
    let start = match out.iter().rposition(|&c| c == '\n') {
        Some(nl) => nl + 1,
        None => 0,
    };
    while out.len() > start && matches!(out[out.len() - 1], ' ' | '\t') {
        out.pop();
    }
    if out.len() > start {
        out.push('\n');
    }
}

/// Copy a `'…'` or `"…"` literal, starting at its opening quote.
fn copy_string(chars: &[char], from: usize, out: &mut Vec<char>) -> usize {
    let quote = chars[from];
    out.push(quote);
    let mut i = from + 1;
    while i < chars.len() {
        let c = chars[i];
        out.push(c);
        i += 1;
        if c == '\\' {
            if let Some(&escaped) = chars.get(i) {
                out.push(escaped);
                i += 1;
            }
        } else if c == quote {
            break;
        }
    }
    i
}

/// Copy a template literal, scanning each `${…}` for comments as it goes.
fn copy_template(chars: &[char], from: usize, out: &mut Vec<char>) -> usize {
    out.push('`');
    let mut i = from + 1;
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' {
            out.push(c);
            i += 1;
            if let Some(&escaped) = chars.get(i) {
                out.push(escaped);
                i += 1;
            }
        } else if c == '`' {
            out.push(c);
            return i + 1;
        } else if c == '$' && chars.get(i + 1) == Some(&'{') {
            out.push('$');
            out.push('{');
            i = scan(chars, i + 2, out, true);
            if chars.get(i) == Some(&'}') {
                out.push('}');
                i += 1;
            }
        } else {
            out.push(c);
            i += 1;
        }
    }
    i
}

/// Copy a regex literal, starting at its opening `/`. A `/` inside a `[…]`
/// character class does not end it.
fn copy_regex(chars: &[char], from: usize, out: &mut Vec<char>) -> usize {
    out.push('/');
    let mut i = from + 1;
    let mut in_class = false;
    while i < chars.len() {
        let c = chars[i];
        out.push(c);
        i += 1;
        match c {
            '\\' => {
                if let Some(&escaped) = chars.get(i) {
                    out.push(escaped);
                    i += 1;
                }
            }
            '[' => in_class = true,
            ']' => in_class = false,
            '/' if !in_class => break,
            // An unterminated regex is impossible in a bundle we generated; if
            // one somehow appears, stop at the line end rather than swallow the
            // rest of the file.
            '\n' => break,
            _ => {}
        }
    }
    // Flags.
    while i < chars.len() && chars[i].is_ascii_alphabetic() {
        out.push(chars[i]);
        i += 1;
    }
    i
}

/// Drop a `/* … */` comment, leaving a space in its place — or a line break, if
/// it spanned lines, since automatic semicolon insertion counts one there.
/// Copyright and license blocks are kept as they are.
fn drop_block(chars: &[char], from: usize, out: &mut Vec<char>) -> usize {
    let mut i = from + 2;
    let mut text = String::new();
    while i < chars.len() {
        if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
            i += 2;
            break;
        }
        text.push(chars[i]);
        i += 1;
    }
    if is_legal_notice(&text) {
        out.extend(chars[from..i].iter());
        return i;
    }
    if text.contains('\n') {
        end_line(out);
        out.push('\n');
    } else if !matches!(out.last(), Some(c) if c.is_whitespace()) {
        out.push(' ');
    }
    i
}

/// Whether a block comment is an attribution that has to survive the build.
fn is_legal_notice(text: &str) -> bool {
    let lower = text.to_lowercase();
    text.starts_with('!')
        || lower.contains("@license")
        || lower.contains("@preserve")
        || lower.contains("copyright")
        || lower.contains("licen") // "licence", "license", "MIT Licensed"
}

/// Whether a `/` here starts a regex literal rather than being division.
///
/// The decision is the previous token's: after a value — an identifier, a
/// number, a string, `)` or `]` — a `/` can only be division, and after
/// anything else (an operator, a comma, `{`, the start of the file) only a
/// regex can follow. The keywords that take an expression next (`return`,
/// `typeof`, `case`, …) look like identifiers but behave like operators.
fn regex_allowed(out: &[char]) -> bool {
    let mut end = out.len();
    while end > 0 && out[end - 1].is_whitespace() {
        end -= 1;
    }
    let Some(&last) = out.get(end.wrapping_sub(1)) else {
        return true;
    };
    if !is_word(last) {
        // `}` closes a block far more often than an object literal, and a
        // statement can be followed by a regex; `)` and `]` end a value.
        return !matches!(last, ')' | ']');
    }
    let mut start = end;
    while start > 0 && is_word(out[start - 1]) {
        start -= 1;
    }
    let word: String = out[start..end].iter().collect();
    matches!(
        word.as_str(),
        "return"
            | "typeof"
            | "instanceof"
            | "in"
            | "of"
            | "new"
            | "delete"
            | "void"
            | "throw"
            | "case"
            | "do"
            | "else"
            | "yield"
            | "await"
    )
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

#[cfg(test)]
mod tests {
    use super::strip;

    #[test]
    fn drops_line_comments_and_the_lines_they_had_to_themselves() {
        let js = "// a note\nvar x = 1; // trailing\nvar y = 2;\n";
        assert_eq!(strip(js), "var x = 1;\nvar y = 2;\n");
    }

    #[test]
    fn keeps_comment_markers_inside_strings() {
        let js = "var u = 'http://x'; // gone\nvar v = \"/* not a comment */\";\n";
        assert_eq!(strip(js), "var u = 'http://x';\nvar v = \"/* not a comment */\";\n");
    }

    #[test]
    fn keeps_comment_markers_inside_regexes() {
        let js = "var r = /https?:\\/\\/[^\\s]+/g; // gone\nvar s = /[/*]/;\n";
        assert_eq!(strip(js), "var r = /https?:\\/\\/[^\\s]+/g;\nvar s = /[/*]/;\n");
    }

    #[test]
    fn tells_division_from_a_regex() {
        let js = "var a = b / c / d; // gone\nvar e = (f) / 2;\nvar g = h[0] / 2;\n";
        assert_eq!(strip(js), "var a = b / c / d;\nvar e = (f) / 2;\nvar g = h[0] / 2;\n");
    }

    #[test]
    fn a_regex_may_follow_a_keyword() {
        let js = "function f() { return /a\\/b/.test(s); } // gone\n";
        assert_eq!(strip(js), "function f() { return /a\\/b/.test(s); }\n");
    }

    #[test]
    fn a_block_comment_between_tokens_leaves_a_space() {
        // The space the comment itself becomes only appears where there was no
        // whitespace already — the surrounding spacing is left as written.
        assert_eq!(strip("var x = a /* here */ + b;\n"), "var x = a  + b;\n");
        assert_eq!(strip("var x = a/*here*/+b;\n"), "var x = a +b;\n");
    }

    #[test]
    fn a_multi_line_block_comment_leaves_a_line_break() {
        // ASI counts a line terminator inside a block comment, so removing one
        // must not join the lines around it.
        assert_eq!(strip("var x = 1\n/* two\nlines */\nvar y = 2\n"), "var x = 1\n\nvar y = 2\n");
    }

    #[test]
    fn license_blocks_survive() {
        let js = "/**\n * Copyright (c) 2011, Someone. (MIT Licensed)\n */\nvar x = 1;\n";
        assert_eq!(strip(js), js);
    }

    #[test]
    fn template_literals_are_scanned_not_swallowed() {
        let js = "var t = `a ${ b /* gone */ + c } // still text`; // gone\n";
        assert_eq!(strip(js), "var t = `a ${ b  + c } // still text`;\n");
    }

    #[test]
    fn nested_braces_in_a_substitution_do_not_end_it() {
        let js = "var t = `${ f({ a: 1 }) }`; // gone\n";
        assert_eq!(strip(js), "var t = `${ f({ a: 1 }) }`;\n");
    }

    #[test]
    fn an_escaped_quote_does_not_end_a_string() {
        let js = "var s = 'it\\'s // fine'; // gone\n";
        assert_eq!(strip(js), "var s = 'it\\'s // fine';\n");
    }
}
