//! End-to-end source-map tests for the JS backend: compile a module with a
//! Source Map v3 and assert that generated definition positions decode back to
//! the correct Elm source line.

const B64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Decode a base64-VLQ segment into its integer fields.
fn vlq_decode(s: &str) -> Vec<i64> {
    let idx = |c: u8| B64.iter().position(|&b| b == c).unwrap() as u64;
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let (mut shift, mut acc) = (0u32, 0u64);
        loop {
            let d = idx(b[i]);
            i += 1;
            acc |= (d & 0x1f) << shift;
            shift += 5;
            if d & 0x20 == 0 {
                break;
            }
        }
        out.push(if acc & 1 == 1 {
            -((acc >> 1) as i64)
        } else {
            (acc >> 1) as i64
        });
    }
    out
}

/// A decoded mapping: generated (line, col) 0-based → source (index, line, col).
#[derive(Debug, Clone, Copy)]
struct Seg {
    gen_line: u32,
    gen_col: i64,
    src: i64,
    src_line: i64,
    src_col: i64,
}

fn decode_mappings(mappings: &str) -> Vec<Seg> {
    let (mut si, mut sl, mut sc) = (0i64, 0i64, 0i64);
    let mut out = Vec::new();
    for (gl, line) in mappings.split(';').enumerate() {
        let mut gc = 0i64;
        for seg in line.split(',').filter(|s| !s.is_empty()) {
            let v = vlq_decode(seg);
            gc += v[0];
            if v.len() >= 4 {
                si += v[1];
                sl += v[2];
                sc += v[3];
                out.push(Seg {
                    gen_line: gl as u32,
                    gen_col: gc,
                    src: si,
                    src_line: sl,
                    src_col: sc,
                });
            }
        }
    }
    out
}

/// Minimal JSON field pluck for the flat maps we emit (no nested objects in the
/// values we read). Good enough for tests without a JSON dependency.
fn json_string_field<'a>(json: &'a str, key: &str) -> &'a str {
    let needle = format!("\"{}\":\"", key);
    let start = json.find(&needle).expect("field present") + needle.len();
    let rest = &json[start..];
    // find the closing unescaped quote
    let mut end = 0;
    let bytes = rest.as_bytes();
    while end < bytes.len() {
        if bytes[end] == b'"' && (end == 0 || bytes[end - 1] != b'\\') {
            break;
        }
        end += 1;
    }
    &rest[..end]
}

#[test]
fn js_source_map_resolves_definitions() {
    let source = "\
module Main exposing (main)


add : Int -> Int -> Int
add a b =
    a + b


number : Int
number =
    add 1 2


main : Int
main =
    number
";
    let (js, map) = alm_compiler::compile_with_source_map(source).expect("compile");

    // The map is v3 with the source registered and its content embedded.
    assert!(map.contains("\"version\":3"), "v3");
    assert!(map.contains("\"sources\":[\"Main.elm\"]"), "source path: {map}");
    assert!(
        map.contains("add a b"),
        "sourcesContent embedded (should contain the source text)"
    );

    let mappings = json_string_field(&map, "mappings");
    let segs = decode_mappings(mappings);
    assert!(!segs.is_empty(), "some mappings recorded");

    // Every mapping is into source 0 (the only source) at a real line.
    let src_lines: Vec<&str> = source.lines().collect();
    for s in &segs {
        assert_eq!(s.src, 0, "single source");
        assert!(
            (s.src_line as usize) < src_lines.len(),
            "src line in range: {s:?}"
        );
    }

    // A generated definition's start maps to the definition. `main`'s value
    // (`var $Main$main = $Main$number;`) maps to `main =`.
    let gen_lines: Vec<&str> = js.lines().collect();
    let main_gen_line = gen_lines
        .iter()
        .position(|l| l.contains("$main = ") && l.trim_start().starts_with("var "))
        .expect("generated `main` definition") as u32;
    let main_seg = segs
        .iter()
        .filter(|s| s.gen_line == main_gen_line)
        .min_by_key(|s| s.gen_col)
        .unwrap_or_else(|| panic!("a mapping on the `main` line {main_gen_line}"));
    assert!(
        src_lines[main_seg.src_line as usize].starts_with("main"),
        "generated `main` maps to its definition, got line {}: {:?}",
        main_seg.src_line + 1,
        src_lines[main_seg.src_line as usize]
    );

    // Sub-expression granularity: more mappings than top-level definitions, and
    // at least one mapping lands mid-value (a sub-expression, not a def start).
    let def_lines = gen_lines
        .iter()
        .filter(|l| l.trim_start().starts_with("var $Main$"))
        .count();
    assert!(
        segs.len() > def_lines,
        "expected sub-expression mappings ({} mappings vs {} defs)",
        segs.len(),
        def_lines
    );

    // `number = add 1 2` compiles to `A2($Main$add, 1, 2)`; the literal `2` is a
    // sub-expression that must map to the `add 1 2` line (source line 12).
    let number_gen_line = gen_lines
        .iter()
        .position(|l| l.contains("$number = ") && l.trim_start().starts_with("var "))
        .expect("generated `number` definition") as u32;
    let on_number: Vec<&Seg> = segs.iter().filter(|s| s.gen_line == number_gen_line).collect();
    assert!(
        on_number.len() >= 3,
        "the `add 1 2` line carries mappings for its sub-expressions, got {}",
        on_number.len()
    );
    // Sub-expressions of `number` map into the `add 1 2` source line.
    let add_call_line = src_lines
        .iter()
        .position(|l| l.trim() == "add 1 2")
        .expect("`add 1 2` in source") as i64;
    assert!(
        on_number.iter().any(|s| s.src_line == add_call_line),
        "a sub-expression of `add 1 2` maps to its source line {}; got {:?}",
        add_call_line + 1,
        on_number.iter().map(|s| s.src_line + 1).collect::<Vec<_>>()
    );
}
