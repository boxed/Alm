//! A thin C-ABI shim over `fancy-regex` (a superset of the `regex` crate that
//! adds backreferences and look-around, so it matches JavaScript's `RegExp`
//! closely). The native runtime links this as a static library and drives the
//! elm/regex kernels through it; the runtime builds the Elm `Match` values.
//!
//! All offsets returned are CODEPOINT indices (not bytes), so they line up with
//! Elm's `String.slice` / `Match.index`, which are codepoint-based.

use fancy_regex::Regex;
use std::os::raw::c_void;

unsafe fn as_str<'a>(p: *const u8, len: usize) -> &'a str {
    // Elm strings are valid UTF-8.
    std::str::from_utf8_unchecked(std::slice::from_raw_parts(p, len))
}

fn byte_to_char(s: &str, b: usize) -> i64 {
    s[..b.min(s.len())].chars().count() as i64
}

/// Rewrite the ASCII-only character-class escapes to match JavaScript's
/// `RegExp` (which alm's JS backend uses), not `fancy-regex`'s Unicode
/// defaults. In JS (no `u` flag) `\w`/`\d` are ASCII `[A-Za-z0-9_]`/`[0-9]`,
/// whereas `fancy-regex` treats them as Unicode (so `\w` matches `ě`, and
/// `[^\w]` then fails to strip it). We translate to Unicode-mode ASCII classes
/// (`\d`→`[0-9]`, `\w`→`[0-9A-Za-z_]`, and their negations), which stay
/// char-oriented (never splitting a multi-byte codepoint). `\b`/`\B` become
/// ASCII word boundaries via `(?-u:…)` — zero-width, so byte-mode is safe. `\s`
/// is left as Unicode whitespace, matching JS's `\s`. Escapes are only rewritten
/// when they are real escapes (an even run of backslashes precedes them); inside
/// a `[...]` class only the positive forms translate (the negations aren't
/// expressible there and are left as-is — a rare case).
fn js_translate(pat: &str) -> String {
    let mut out = String::with_capacity(pat.len() + 16);
    let mut chars = pat.chars().peekable();
    let mut in_class = false;
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.peek().copied() {
                Some(n) => {
                    let repl: Option<&str> = match n {
                        'd' => Some(if in_class { "0-9" } else { "[0-9]" }),
                        'w' => Some(if in_class { "0-9A-Za-z_" } else { "[0-9A-Za-z_]" }),
                        'D' if !in_class => Some("[^0-9]"),
                        'W' if !in_class => Some("[^0-9A-Za-z_]"),
                        'b' if !in_class => Some("(?-u:\\b)"),
                        'B' if !in_class => Some("(?-u:\\B)"),
                        _ => None,
                    };
                    match repl {
                        Some(s) => {
                            out.push_str(s);
                            chars.next();
                        }
                        None => {
                            // Keep the escape verbatim (handles `\\`, `\.`, …).
                            out.push('\\');
                            out.push(n);
                            chars.next();
                        }
                    }
                }
                None => out.push('\\'),
            },
            '[' if !in_class => {
                in_class = true;
                out.push('[');
            }
            ']' if in_class => {
                in_class = false;
                out.push(']');
            }
            _ => out.push(c),
        }
    }
    out
}

/// Compile `pat` with JS flags: `i` (case-insensitive), `m` (multiline).
/// Returns an opaque `*mut Regex`, or null if the pattern is invalid.
#[no_mangle]
pub unsafe extern "C" fn alm_rx_compile(pat: *const u8, plen: usize, ci: bool, ml: bool) -> *mut c_void {
    std::panic::catch_unwind(|| {
        let p = js_translate(as_str(pat, plen));
        let mut flags = String::new();
        if ci {
            flags.push('i');
        }
        if ml {
            flags.push('m');
        }
        let full = if flags.is_empty() {
            p.to_string()
        } else {
            format!("(?{}){}", flags, p)
        };
        match Regex::new(&full) {
            Ok(re) => Box::into_raw(Box::new(re)) as *mut c_void,
            Err(_) => std::ptr::null_mut(),
        }
    })
    .unwrap_or(std::ptr::null_mut())
}

#[no_mangle]
pub unsafe extern "C" fn alm_rx_contains(re: *const c_void, txt: *const u8, tlen: usize) -> i32 {
    let re = &*(re as *const Regex);
    match re.is_match(as_str(txt, tlen)) {
        Ok(true) => 1,
        _ => 0,
    }
}

/// Find up to `limit` matches (negative = unlimited). Returns a heap buffer of
/// codepoint offsets, layout:
///   [ nmatches,
///     (match_start, match_end, ngroups, (grp_start, grp_end)*ngroups)* ]
/// A non-participating group is `(-1, -1)`. `*out_len` gets the buffer length.
/// Free with `alm_rx_free`.
#[no_mangle]
pub unsafe extern "C" fn alm_rx_find(
    re: *const c_void,
    txt: *const u8,
    tlen: usize,
    limit: i64,
    out_len: *mut usize,
) -> *mut i64 {
    let re = &*(re as *const Regex);
    let t = as_str(txt, tlen);
    let mut buf: Vec<i64> = vec![0];
    let mut n = 0i64;
    for cap in re.captures_iter(t) {
        if limit >= 0 && n >= limit {
            break;
        }
        let cap = match cap {
            Ok(c) => c,
            Err(_) => break,
        };
        let whole = match cap.get(0) {
            Some(m) => m,
            None => continue,
        };
        buf.push(byte_to_char(t, whole.start()));
        buf.push(byte_to_char(t, whole.end()));
        let ngroups = cap.len().saturating_sub(1);
        buf.push(ngroups as i64);
        for gi in 1..cap.len() {
            match cap.get(gi) {
                Some(m) => {
                    buf.push(byte_to_char(t, m.start()));
                    buf.push(byte_to_char(t, m.end()));
                }
                None => {
                    buf.push(-1);
                    buf.push(-1);
                }
            }
        }
        n += 1;
    }
    buf[0] = n;
    *out_len = buf.len();
    let ptr = buf.as_mut_ptr();
    std::mem::forget(buf);
    ptr
}

/// Split on up to `limit` matches (negative = unlimited), producing the pieces
/// as codepoint spans. Buffer layout: [ npieces, (start, end)*npieces ].
#[no_mangle]
pub unsafe extern "C" fn alm_rx_split(
    re: *const c_void,
    txt: *const u8,
    tlen: usize,
    limit: i64,
    out_len: *mut usize,
) -> *mut i64 {
    let re = &*(re as *const Regex);
    let t = as_str(txt, tlen);
    let mut buf: Vec<i64> = vec![0];
    let mut pieces = 0i64;
    let mut splits = 0i64;
    let mut last = 0usize;
    for mm in re.find_iter(t) {
        if limit >= 0 && splits >= limit {
            break;
        }
        let mm = match mm {
            Ok(m) => m,
            Err(_) => break,
        };
        buf.push(byte_to_char(t, last));
        buf.push(byte_to_char(t, mm.start()));
        pieces += 1;
        splits += 1;
        last = mm.end();
    }
    buf.push(byte_to_char(t, last));
    buf.push(byte_to_char(t, t.len()));
    pieces += 1;
    buf[0] = pieces;
    *out_len = buf.len();
    let ptr = buf.as_mut_ptr();
    std::mem::forget(buf);
    ptr
}

#[no_mangle]
pub unsafe extern "C" fn alm_rx_free(ptr: *mut i64, len: usize) {
    if !ptr.is_null() {
        drop(Vec::from_raw_parts(ptr, len, len));
    }
}

#[cfg(test)]
mod tests {
    use super::js_translate;

    #[test]
    fn ascii_word_and_digit_classes() {
        // JS `\w`/`\d` are ASCII; translate to Unicode-mode ASCII classes.
        assert_eq!(js_translate(r"\w"), "[0-9A-Za-z_]");
        assert_eq!(js_translate(r"\d+"), "[0-9]+");
        assert_eq!(js_translate(r"\D"), "[^0-9]");
        assert_eq!(js_translate(r"\W"), "[^0-9A-Za-z_]");
        // Inside a class, only the atoms are emitted (no nested brackets).
        assert_eq!(js_translate(r"[^\w-]"), "[^0-9A-Za-z_-]");
        assert_eq!(js_translate(r"[\d]"), "[0-9]");
    }

    #[test]
    fn boundaries_and_untouched_escapes() {
        assert_eq!(js_translate(r"\bfoo\B"), r"(?-u:\b)foo(?-u:\B)");
        // A word boundary inside a class is a backspace in JS; left as-is.
        assert_eq!(js_translate(r"[\b]"), r"[\b]");
        // Non-class escapes and escaped backslashes pass through unchanged.
        assert_eq!(js_translate(r"\.\n\\w"), r"\.\n\\w");
        assert_eq!(js_translate(r"\s\S"), r"\s\S");
    }
}
