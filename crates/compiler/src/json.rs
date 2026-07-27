//! A minimal JSON reader.
//!
//! alm reads two JSON documents it did not write: `elm.json`, and the
//! `docs.json` of a published package. `elm.json` is edited in place by
//! `install`, so that one is handled with the string surgery in
//! [`crate::packages`] — rewriting a block leaves fields alm does not model
//! untouched. `docs.json` is only ever read, and it is deep enough (arrays of
//! objects of arrays of pairs) that scanning for keys does not work; hence
//! this.
//!
//! Only what those files contain is supported. That is all of JSON except
//! nothing — but the error reporting is deliberately thin: a malformed
//! `docs.json` in the package cache is a broken cache, not a user mistake to
//! be diagnosed.

use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Json>),
    Object(BTreeMap<String, Json>),
}

impl Json {
    /// The value at `key`, if this is an object that has one.
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object(fields) => fields.get(key),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::String(text) => Some(text),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Json]> {
        match self {
            Json::Array(items) => Some(items),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Number(n) => Some(*n),
            _ => None,
        }
    }

    /// `get(key)` as a string, defaulting to empty — the shape docs.json
    /// readers want, where a missing comment is just no comment.
    pub fn string(&self, key: &str) -> String {
        self.get(key).and_then(Json::as_str).unwrap_or_default().to_string()
    }

    /// `get(key)` as a slice, defaulting to empty.
    pub fn array(&self, key: &str) -> &[Json] {
        self.get(key).and_then(Json::as_array).unwrap_or(&[])
    }
}

pub fn parse(text: &str) -> Option<Json> {
    let mut p = Parser { chars: text.as_bytes(), at: 0, text };
    p.spaces();
    let value = p.value()?;
    p.spaces();
    p.at.eq(&p.chars.len()).then_some(value)
}

struct Parser<'a> {
    chars: &'a [u8],
    at: usize,
    text: &'a str,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<u8> {
        self.chars.get(self.at).copied()
    }

    fn eat(&mut self, byte: u8) -> Option<()> {
        (self.peek()? == byte).then(|| self.at += 1)
    }

    fn spaces(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn value(&mut self) -> Option<Json> {
        match self.peek()? {
            b'{' => self.object(),
            b'[' => self.array(),
            b'"' => Some(Json::String(self.string()?)),
            b't' => self.literal("true").map(|()| Json::Bool(true)),
            b'f' => self.literal("false").map(|()| Json::Bool(false)),
            b'n' => self.literal("null").map(|()| Json::Null),
            _ => self.number(),
        }
    }

    fn literal(&mut self, word: &str) -> Option<()> {
        self.text[self.at..].starts_with(word).then(|| self.at += word.len())
    }

    fn object(&mut self) -> Option<Json> {
        self.eat(b'{')?;
        let mut fields = BTreeMap::new();
        self.spaces();
        if self.eat(b'}').is_some() {
            return Some(Json::Object(fields));
        }
        loop {
            self.spaces();
            let key = self.string()?;
            self.spaces();
            self.eat(b':')?;
            self.spaces();
            fields.insert(key, self.value()?);
            self.spaces();
            if self.eat(b',').is_none() {
                break;
            }
        }
        self.eat(b'}')?;
        Some(Json::Object(fields))
    }

    fn array(&mut self) -> Option<Json> {
        self.eat(b'[')?;
        let mut items = Vec::new();
        self.spaces();
        if self.eat(b']').is_some() {
            return Some(Json::Array(items));
        }
        loop {
            self.spaces();
            items.push(self.value()?);
            self.spaces();
            if self.eat(b',').is_none() {
                break;
            }
        }
        self.eat(b']')?;
        Some(Json::Array(items))
    }

    fn string(&mut self) -> Option<String> {
        self.eat(b'"')?;
        let mut out = String::new();
        loop {
            match self.peek()? {
                b'"' => {
                    self.at += 1;
                    return Some(out);
                }
                b'\\' => {
                    self.at += 1;
                    let escape = self.peek()?;
                    self.at += 1;
                    match escape {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => out.push(self.escaped_char()?),
                        _ => return None,
                    }
                }
                _ => {
                    // Copy the whole UTF-8 sequence, not the leading byte.
                    let rest = &self.text[self.at..];
                    let c = rest.chars().next()?;
                    self.at += c.len_utf8();
                    out.push(c);
                }
            }
        }
    }

    /// A `\uXXXX` escape, joining a surrogate pair if one follows — the
    /// encoder emits astral characters that way.
    fn escaped_char(&mut self) -> Option<char> {
        let high = self.hex4()?;
        if (0xd800..0xdc00).contains(&high) {
            self.eat(b'\\')?;
            self.eat(b'u')?;
            let low = self.hex4()?;
            let scalar = 0x10000 + ((high - 0xd800) << 10) + (low - 0xdc00);
            return char::from_u32(scalar);
        }
        char::from_u32(high)
    }

    fn hex4(&mut self) -> Option<u32> {
        let digits = self.text.get(self.at..self.at + 4)?;
        self.at += 4;
        u32::from_str_radix(digits, 16).ok()
    }

    fn number(&mut self) -> Option<Json> {
        let start = self.at;
        if self.peek() == Some(b'-') {
            self.at += 1;
        }
        while matches!(self.peek(), Some(b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-')) {
            self.at += 1;
        }
        self.text[start..self.at].parse().ok().map(Json::Number)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_documents_round_trip_into_values() {
        let value = parse(r#"{"a": [1, {"b": true}], "c": null}"#).unwrap();
        let a = value.get("a").unwrap().as_array().unwrap();
        assert_eq!(a[0].as_f64(), Some(1.0));
        assert_eq!(a[1].get("b"), Some(&Json::Bool(true)));
        assert_eq!(value.get("c"), Some(&Json::Null));
    }

    #[test]
    fn escapes_are_decoded_including_surrogate_pairs() {
        let value = parse(r#""a\nb\t\"c\\å😀""#).unwrap();
        assert_eq!(value.as_str(), Some("a\nb\t\"c\\å😀"));
    }

    /// A docs.json comment is a long string with newlines and backticks in it.
    #[test]
    fn comments_survive_intact() {
        let value = parse(r#"{"comment": " Send HTTP requests.\n\n# Requests\n@docs get\n"}"#)
            .unwrap();
        assert_eq!(value.string("comment"), " Send HTTP requests.\n\n# Requests\n@docs get\n");
    }

    #[test]
    fn trailing_junk_is_rejected_rather_than_ignored() {
        assert!(parse("{} {}").is_none());
        assert!(parse("[1,]").is_none());
        assert!(parse("{\"a\" 1}").is_none());
    }
}
