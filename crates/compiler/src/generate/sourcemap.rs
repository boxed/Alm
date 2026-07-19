//! Source Map v3 construction, shared by the JS and wasm-gc backends.
//!
//! A [`SourceMap`] interns the source files a build touches, accumulates
//! `(generated position) -> (source position)` mappings, and serializes to the
//! standard v3 JSON (`{ version, file, sources, sourcesContent, names,
//! mappings }`). The `mappings` field is base64-VLQ, delta-encoded per the spec.
//!
//! Positions in this module are 0-based (as the source-map format requires);
//! callers convert from the compiler's 1-based [`Region`] via [`region_start`].

use std::collections::HashMap;

use crate::reporting::Region;

const BASE64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Append the base64-VLQ encoding of `value` (signed, sign in the LSB) to `out`.
fn vlq_encode(value: i64, out: &mut String) {
    // Move the sign into the least-significant bit, then emit 5-bit groups
    // little-endian with bit 6 (0x20) as the continuation flag.
    let mut v: u64 = if value < 0 {
        ((-value as u64) << 1) | 1
    } else {
        (value as u64) << 1
    };
    loop {
        let mut digit = (v & 0x1f) as usize;
        v >>= 5;
        if v != 0 {
            digit |= 0x20;
        }
        out.push(BASE64[digit] as char);
        if v == 0 {
            break;
        }
    }
}

/// 0-based generated column mapped to a 0-based source position.
#[derive(Clone, Copy)]
struct Mapping {
    gen_line: u32,
    gen_col: u32,
    src: u32,
    src_line: u32,
    src_col: u32,
}

/// Accumulates source files and generated→source mappings for one output.
pub struct SourceMap {
    /// The generated file this map describes (the `file` field).
    file: String,
    sources: Vec<String>,
    sources_content: Vec<String>,
    index: HashMap<String, u32>,
    mappings: Vec<Mapping>,
}

impl SourceMap {
    pub fn new(file: impl Into<String>) -> SourceMap {
        SourceMap {
            file: file.into(),
            sources: Vec::new(),
            sources_content: Vec::new(),
            index: HashMap::new(),
            mappings: Vec::new(),
        }
    }

    /// Intern a source file (by path), returning its stable index. Idempotent —
    /// the first `content` seen for a path is kept.
    pub fn add_source(&mut self, path: &str, content: &str) -> u32 {
        if let Some(&i) = self.index.get(path) {
            return i;
        }
        let i = self.sources.len() as u32;
        self.sources.push(path.to_string());
        self.sources_content.push(content.to_string());
        self.index.insert(path.to_string(), i);
        i
    }

    /// Record that generated `(gen_line, gen_col)` (0-based) comes from source
    /// index `src` at `(src_line, src_col)` (0-based).
    pub fn add(&mut self, gen_line: u32, gen_col: u32, src: u32, src_line: u32, src_col: u32) {
        self.mappings.push(Mapping {
            gen_line,
            gen_col,
            src,
            src_line,
            src_col,
        });
    }

    /// Whether any mapping has been recorded.
    pub fn is_empty(&self) -> bool {
        self.mappings.is_empty()
    }

    /// Serialize to Source Map v3 JSON.
    pub fn to_json(&self) -> String {
        let mut out = String::from("{\"version\":3,\"file\":");
        json_str(&self.file, &mut out);
        out.push_str(",\"sources\":[");
        for (i, s) in self.sources.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            json_str(s, &mut out);
        }
        out.push_str("],\"sourcesContent\":[");
        for (i, s) in self.sources_content.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            json_str(s, &mut out);
        }
        out.push_str("],\"names\":[],\"mappings\":\"");
        out.push_str(&self.encode_mappings());
        out.push_str("\"}");
        out
    }

    /// Build the VLQ `mappings` string: segments grouped by generated line
    /// (`;`-separated), comma-separated within a line, each field delta-encoded.
    fn encode_mappings(&self) -> String {
        let mut ms = self.mappings.clone();
        ms.sort_by(|a, b| (a.gen_line, a.gen_col).cmp(&(b.gen_line, b.gen_col)));

        let mut out = String::new();
        // Running previous values; only gen_col resets at each new line.
        let mut prev_gen_col: i64 = 0;
        let mut prev_src: i64 = 0;
        let mut prev_src_line: i64 = 0;
        let mut prev_src_col: i64 = 0;
        let mut cur_line: u32 = 0;
        let mut first_in_line = true;
        let mut last_gen: Option<(u32, u32)> = None;

        for m in &ms {
            // At most one mapping per generated position — a consumer resolves a
            // position to a single source, and stable sort keeps the outermost
            // (first-recorded) expression at a shared position.
            if last_gen == Some((m.gen_line, m.gen_col)) {
                continue;
            }
            last_gen = Some((m.gen_line, m.gen_col));
            while cur_line < m.gen_line {
                out.push(';');
                cur_line += 1;
                prev_gen_col = 0;
                first_in_line = true;
            }
            if !first_in_line {
                out.push(',');
            }
            first_in_line = false;
            vlq_encode(m.gen_col as i64 - prev_gen_col, &mut out);
            vlq_encode(m.src as i64 - prev_src, &mut out);
            vlq_encode(m.src_line as i64 - prev_src_line, &mut out);
            vlq_encode(m.src_col as i64 - prev_src_col, &mut out);
            prev_gen_col = m.gen_col as i64;
            prev_src = m.src as i64;
            prev_src_line = m.src_line as i64;
            prev_src_col = m.src_col as i64;
        }
        out
    }
}

/// The 0-based `(line, col)` of a region's start, or `None` for a synthetic
/// region (`Region::ZERO`, produced for desugared nodes) that shouldn't map.
pub fn region_start(region: &Region) -> Option<(u32, u32)> {
    if region.start.row == 0 || region.start.col == 0 {
        return None;
    }
    Some((region.start.row - 1, region.start.col - 1))
}

/// Append a JSON string literal for `s` (minimal escaping) to `out`.
fn json_str(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vlq(v: i64) -> String {
        let mut s = String::new();
        vlq_encode(v, &mut s);
        s
    }

    // Decode a base64-VLQ string back to integers (test-only inverse).
    fn vlq_decode(s: &str) -> Vec<i64> {
        let idx = |c: u8| BASE64.iter().position(|&b| b == c).unwrap() as u64;
        let mut out = Vec::new();
        let bytes = s.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            let mut shift = 0u32;
            let mut acc: u64 = 0;
            loop {
                let d = idx(bytes[i]);
                i += 1;
                acc |= (d & 0x1f) << shift;
                shift += 5;
                if d & 0x20 == 0 {
                    break;
                }
            }
            let neg = acc & 1 == 1;
            let val = (acc >> 1) as i64;
            out.push(if neg { -val } else { val });
        }
        out
    }

    #[test]
    fn vlq_known_vectors() {
        assert_eq!(vlq(0), "A");
        assert_eq!(vlq(1), "C");
        assert_eq!(vlq(-1), "D");
        assert_eq!(vlq(2), "E");
        assert_eq!(vlq(16), "gB");
    }

    #[test]
    fn vlq_roundtrips() {
        for v in [-1000, -33, -1, 0, 1, 15, 16, 17, 123, 4096, 1_000_000] {
            let mut s = String::new();
            vlq_encode(v, &mut s);
            assert_eq!(vlq_decode(&s), vec![v], "roundtrip {v}");
        }
    }

    #[test]
    fn mappings_delta_encode_and_group_by_line() {
        let mut sm = SourceMap::new("out.js");
        let s = sm.add_source("Main.elm", "x = 1\n");
        assert_eq!(s, 0);
        // two mappings on generated line 0, one on line 2 (line 1 empty)
        sm.add(0, 0, 0, 0, 0);
        sm.add(0, 4, 0, 0, 4);
        sm.add(2, 2, 0, 1, 0);
        let json = sm.to_json();
        let m = json
            .split("\"mappings\":\"")
            .nth(1)
            .unwrap()
            .trim_end_matches("\"}");
        // line 0: "AAAA" then delta genCol 4 → "IAAI"; ";;" for lines 1&2 start;
        // line 2 first seg genCol 2, src 0, srcLine +1, srcCol -4.
        let lines: Vec<&str> = m.split(';').collect();
        assert_eq!(lines.len(), 3, "three generated lines");
        assert_eq!(vlq_decode(lines[0].split(',').next().unwrap()), vec![0, 0, 0, 0]);
        assert_eq!(vlq_decode(lines[0].split(',').nth(1).unwrap()), vec![4, 0, 0, 4]);
        assert_eq!(lines[1], "");
        assert_eq!(vlq_decode(lines[2]), vec![2, 0, 1, -4]);
    }

    #[test]
    fn json_has_sources_and_content() {
        let mut sm = SourceMap::new("out.js");
        sm.add_source("A.elm", "module A\n");
        let json = sm.to_json();
        assert!(json.contains("\"version\":3"));
        assert!(json.contains("\"sources\":[\"A.elm\"]"));
        assert!(json.contains("\"sourcesContent\":[\"module A\\n\"]"));
    }

    #[test]
    fn region_start_skips_synthetic() {
        assert_eq!(region_start(&Region::ZERO), None);
        let r = Region::new(
            crate::reporting::annotation::Position::new(3, 5),
            crate::reporting::annotation::Position::new(3, 9),
        );
        assert_eq!(region_start(&r), Some((2, 4)));
    }
}
