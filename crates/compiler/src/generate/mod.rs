//! Port of `Generate.JavaScript` — compile the canonical AST to JavaScript.
//!
//! Uses the same runtime conventions as Elm's kernel: `F2`/`A2` helpers for
//! curried functions, `{ $: 'Ctor', a: ..., b: ... }` objects for custom
//! types, cons cells for lists, and plain objects for records.

pub mod native;
pub mod sourcemap;
pub mod wasmgc;

use std::collections::{HashMap, HashSet};
use std::fmt::Write;

use crate::ast::canonical as can;
use crate::data::Name;
use crate::generate::sourcemap::{region_start, SourceMap};
use crate::reporting::Region;

pub const RUNTIME: &str = include_str!("runtime.js");

/// The runtime source to embed. `ALM_RUNTIME_JS` overrides the compiled-in
/// kernel — used by the mutation test harness to inject mutated runtimes
/// without rebuilding the compiler.
fn runtime_source() -> String {
    match std::env::var("ALM_RUNTIME_JS") {
        Ok(path) => std::fs::read_to_string(path).expect("ALM_RUNTIME_JS must be readable"),
        Err(_) => RUNTIME.to_string(),
    }
}

pub fn generate(module: &can::Module) -> String {
    generate_project(std::slice::from_ref(module))
}

/// Generate a single JavaScript file from all the modules of a project,
/// given in dependency order (dependencies first).
pub fn generate_project(modules: &[can::Module]) -> String {
    generate_project_typed(modules, HashMap::new(), true)
}

/// Like [`generate_project_typed`] but also builds a Source Map v3. Tree-shaking
/// runs (honoring `ALM_NO_DCE`) and the map is remapped onto the shaken bundle,
/// so mapped JS is the same size as an ordinary build. Returns
/// `(javascript, source_map_json)`.
pub fn generate_project_typed_mapped(
    modules: &[can::Module],
    node_types: HashMap<Name, HashMap<Region, can::Type>>,
    sources: &HashMap<Name, (String, String)>,
) -> (String, String) {
    let dce = std::env::var_os("ALM_NO_DCE").is_none();
    let (js, map) = gen_bundle(modules, node_types, dce, Some(sources), None);
    (js, map.unwrap_or_default())
}

/// Generate a bundle with dead-code elimination disabled — the whole runtime
/// kernel is emitted verbatim. Used by tests that reach into kernel internals
/// the app itself never references (e.g. `Elm.Kernel.HtmlAsJson`).
pub fn generate_no_dce(module: &can::Module) -> String {
    generate_project_typed(std::slice::from_ref(module), HashMap::new(), false)
}

/// Like `generate_project`, but with per-module expression types so comparison
/// operators can inline to native JS operators on scalar comparables. `dce`
/// tree-shakes unreachable definitions out of the bundle (see `tree_shake`).
pub fn generate_project_typed(
    modules: &[can::Module],
    node_types: HashMap<Name, HashMap<Region, can::Type>>,
    dce: bool,
) -> String {
    gen_bundle(modules, node_types, dce, None, None).0
}

/// What the REPL wants printed after the bundle runs: a top-level binding, and
/// the type to show beside it.
pub struct ReplPrint {
    pub module: Name,
    pub value: Name,
    /// The type as it should read on screen, already laid out.
    pub type_text: String,
    pub ansi: bool,
}

/// The bundle plus a statement that prints one value with its type — elm's
/// `JS.generateForRepl`. The statement goes *inside* the bundle so it can see
/// the value's mangled name and the runtime's printer directly.
pub fn generate_repl(
    modules: &[can::Module],
    node_types: HashMap<Name, HashMap<Region, can::Type>>,
    print: ReplPrint,
) -> String {
    gen_bundle(modules, node_types, false, None, Some(print)).0
}

/// The bundle generator. With `sources` present, builds a source map alongside
/// the JS (and the caller forces `dce` off, since tree-shaking would move the
/// positions the map records). Returns `(javascript, Option<source_map_json>)`.
fn gen_bundle(
    modules: &[can::Module],
    mut node_types: HashMap<Name, HashMap<Region, can::Type>>,
    dce: bool,
    sources: Option<&HashMap<Name, (String, String)>>,
    repl: Option<ReplPrint>,
) -> (String, Option<String>) {
    let maps_on = sources.is_some();
    let mut gen = Generator {
        out: String::new(),
        module_name: None,
        temp_counter: 0,
        node_types: HashMap::new(),
        cyclic_values: HashSet::new(),
        maps_on,
        sm: SourceMap::new(""),
        cur_line: 0,
        cur_col: 0,
        scanned: 0,
        src_idx: None,
    };

    gen.out.push_str("(function () {\n'use strict';\n\n");
    gen.out.push_str(&runtime_source());
    gen.out.push_str("\n// HIGHER-ARITY CURRY HELPERS\n");
    for n in 8..=64 {
        let params: Vec<String> = (0..n).map(|i| format!("v{}", i)).collect();
        writeln!(
            gen.out,
            "var F{} = function (fun) {{ return _Fn({}, fun); }};",
            n, n
        )
        .unwrap();
        writeln!(
            gen.out,
            "var A{} = function (f, {}) {{ return _An(f, [{}]); }};",
            n,
            params.join(", "),
            params.join(", ")
        )
        .unwrap();
    }
    gen.out.push_str("\n// BUILTIN UNION CONSTRUCTORS\n");
    for union in crate::builtins::UNIONS {
        // Bool/Order/Maybe/Result constructors are hand-written in the
        // runtime kernel.
        if matches!(union.module, "Basics" | "Maybe" | "Result") {
            continue;
        }
        let module_var = mangle_module(&Name::from(union.module));
        for (ctor_name, args) in union.ctors {
            emit_ctor(&mut gen.out, &module_var, ctor_name, args.len());
        }
    }
    gen.out.push_str("\n// HTML HELPERS (generated from the builtin tables)\n");
    for tag in crate::builtins::HTML_TAGS {
        let dom_tag = tag.trim_end_matches('_');
        writeln!(
            gen.out,
            "var $Html${} = _VDom_node('{}');",
            sanitize(tag),
            dom_tag
        )
        .unwrap();
    }
    // String properties. `href`/`src`/`action` carry a URI, which elm screens
    // for `javascript:` (and `data:text/html`) before setting — see _VDom_prop.
    for (attr, prop) in crate::builtins::HTML_STRING_PROPS {
        let sanitizer = match *attr {
            "href" | "action" => "_VDom_noJavaScriptUri",
            "src" => "_VDom_noJavaScriptOrHtmlUri",
            _ => "",
        };
        if sanitizer.is_empty() {
            writeln!(gen.out, "var $Html$Attributes${} = _VDom_prop('{}');", sanitize(attr), prop)
        } else {
            writeln!(
                gen.out,
                "var $Html$Attributes${} = function (v) {{ return {{ $: 'AProp', key: '{}', val: {}(v) }}; }};",
                sanitize(attr),
                prop,
                sanitizer
            )
        }
        .unwrap();
    }
    for (attr, name) in crate::builtins::HTML_STRING_ATTRS {
        writeln!(
            gen.out,
            "var $Html$Attributes${} = function (v) {{ return {{ $: 'AAttr', key: '{}', val: v }}; }};",
            sanitize(attr),
            name
        )
        .unwrap();
    }
    for (attr, prop) in crate::builtins::HTML_BOOL_ATTRS {
        writeln!(gen.out, "var $Html$Attributes${} = _VDom_prop('{}');", sanitize(attr), prop).unwrap();
    }
    for (attr, prop) in crate::builtins::HTML_CHAR_ATTRS {
        // A Char is a boxed one-character string; `+ ''` is String.fromChar.
        writeln!(
            gen.out,
            "var $Html$Attributes${} = function (c) {{ return {{ $: 'AProp', key: '{}', val: c + '' }}; }};",
            sanitize(attr),
            prop
        )
        .unwrap();
    }
    for (attr, name) in crate::builtins::HTML_INT_ATTRS {
        writeln!(
            gen.out,
            "var $Html$Attributes${} = function (n) {{ return {{ $: 'AAttr', key: '{}', val: String(n) }}; }};",
            sanitize(attr),
            name
        )
        .unwrap();
    }
    // elm's two off-pattern helpers: a Bool rendered "on"/"off", and an Int
    // rendered as a *string* property.
    writeln!(
        gen.out,
        "var $Html$Attributes$autocomplete = function (b) {{ return {{ $: 'AProp', key: 'autocomplete', val: b ? 'on' : 'off' }}; }};"
    )
    .unwrap();
    writeln!(
        gen.out,
        "var $Html$Attributes$start = function (n) {{ return {{ $: 'AProp', key: 'start', val: String(n) }}; }};"
    )
    .unwrap();
    writeln!(
        gen.out,
        "var $Html$Attributes$classList = function (pairs) {{ var names = []; for (var xs = pairs; xs.$ === '::'; xs = xs.b) {{ var d = xs.d; for (var i = xs.o, n = d.length; i < n; i++) {{ if (d[i].b) {{ names.push(d[i].a); }} }} }} return {{ $: 'AProp', key: 'className', val: names.join(' ') }}; }};"
    )
    .unwrap();
    writeln!(
        gen.out,
        "var $Html$Attributes$property = $VirtualDom$property;"
    )
    .unwrap();
    for tag in crate::builtins::SVG_TAGS {
        let dom_tag = tag.trim_end_matches('_');
        writeln!(
            gen.out,
            "var $Svg${} = _VDom_nodeNS('{}');",
            sanitize(tag),
            dom_tag
        )
        .unwrap();
    }
    for (attr, dom_name) in crate::builtins::SVG_ATTRS {
        writeln!(
            gen.out,
            "var $Svg$Attributes${} = function (v) {{ return {{ $: 'AAttr', key: '{}', val: v }}; }};",
            sanitize(attr),
            dom_name
        )
        .unwrap();
    }

    let mut all_exports: Vec<(Name, Vec<Name>)> = Vec::new();
    for module in modules {
        gen.module_name = Some(module.name.clone());
        gen.node_types = node_types.remove(&module.name).unwrap_or_default();
        gen.src_idx = sources
            .and_then(|s| s.get(&module.name))
            .map(|(path, content)| gen.sm.add_source(path, content));
        gen.out.push_str("\n// MODULE ");
        gen.out.push_str(module.name.as_str());
        gen.out.push_str("\n\n");

        for union in &module.unions {
            gen.union(union);
        }
        for port in &module.ports {
            gen.port_decl(port);
        }

        let mut exports = Vec::new();
        for group in &module.decls {
            match group {
                can::DeclGroup::Value(def) => {
                    gen.top_level_def(def);
                    exports.push(def.name.value.clone());
                }
                can::DeclGroup::Recursive(defs) => {
                    gen.recursive_group(defs);
                    for def in defs {
                        exports.push(def.name.value.clone());
                    }
                }
            }
        }
        gen.manager_decl(module);
        all_exports.push((module.name.clone(), exports));
    }

    let mut module_objects = String::new();
    for (i, (module_name, exports)) in all_exports.iter().enumerate() {
        if i > 0 {
            module_objects.push_str(", ");
        }
        let module_var = mangle_module(module_name);
        let mut export_fields = String::new();
        for (j, name) in exports.iter().enumerate() {
            if j > 0 {
                export_fields.push_str(", ");
            }
            write!(
                export_fields,
                "'{}': _Platform_wrap({}${})",
                name,
                module_var,
                sanitize(name)
            )
            .unwrap();
        }
        // elm publishes a module as `{init: …}` — the program's initializer
        // sits directly on the module object. alm exports every top-level
        // binding besides, so `_Platform_module` layers elm's `init` on top
        // (last, so it wins over a binding that happens to be named `init`).
        if exports.iter().any(|n| n.as_str() == "main") {
            write!(
                module_objects,
                "'{}': _Platform_module({{ {} }}, {}$main)",
                module_name, export_fields, module_var
            )
        } else {
            write!(module_objects, "'{}': {{ {} }}", module_name, export_fields)
        }
        .unwrap();
    }
    // `.call(this)` makes `this` the CommonJS `module.exports` under node and
    // the global object in a browser, so — exactly as in elm's output — the
    // bundle publishes itself as `Elm` in whichever scope loaded it.
    write!(
        gen.out,
        "\nvar Elm = {{ {} }};\n\
         _Platform_export(this, Elm);\n",
        module_objects
    )
    .unwrap();
    if let Some(print) = repl {
        gen.out.push_str(&repl_print(&print));
    }
    gen.out.push_str("}).call(this);\n");

    if dce {
        // Tree-shake, then remap the source map onto the shaken bundle: live
        // units keep their columns and only shift lines, and dead units' bodies
        // (with their mappings) are dropped.
        let (js, line_map) = tree_shake(&gen.out);
        let map = maps_on.then(|| {
            gen.sm.remap_generated_lines(&line_map);
            gen.sm.to_json()
        });
        (js, map)
    } else {
        let map = maps_on.then(|| gen.sm.to_json());
        (gen.out, map)
    }
}

/// elm's REPL print statement: render the value, then put the type after it —
/// on the same line if it fits in 80 columns, indented underneath if not.
fn repl_print(print: &ReplPrint) -> String {
    let value = format!("{}${}", mangle_module(&print.module), sanitize(&print.value));
    let ansi = if print.ansi { "true" } else { "false" };
    format!(
        "var _value = _Debug_toAnsiString({ansi}, {value});\n\
         var _type = {};\n\
         function _print(t) {{ console.log(_value + ({ansi} ? '\\x1b[90m' + t + '\\x1b[0m' : t)); }}\n\
         if (_value.length + 3 + _type.length >= 80 || _type.indexOf('\\n') >= 0) {{\n\
         \x20   _print('\\n    : ' + _type.split('\\n').join('\\n      '));\n\
         }} else {{\n\
         \x20   _print(' : ' + _type);\n\
         }}\n",
        js_string(&print.type_text)
    )
}

/// Dead-code elimination over the fully-assembled bundle.
///
/// Unlike elm, alm's standard library is a single hand-written kernel
/// (`runtime.js`) that we would otherwise emit verbatim into every bundle —
/// hundreds of stdlib functions the app never touches. This pass tree-shakes
/// them out.
///
/// The bundle is a flat sequence of top-level *units*. Most units are a single
/// definition beginning at column 0 with `var NAME` or `function NAME`, running
/// until the next such line — bodies are always indented, and neither the
/// hand-written runtime nor generated code has top-level IIFEs, multi-line
/// template strings, or block comments that could hide a column-0
/// `var`/`function`, so this split is exact.
///
/// The one multi-name unit is the cyclic-value force block emitted by
/// `recursive_group`: a column-0 `try { var $M$x = $M$cyclic$x(); ... } catch`.
/// Because `var` is function-scoped, those indented bindings are real top-level
/// definitions, so the whole `try` block is one unit that *defines* every
/// `var NAME` inside it and is kept if any of them is reachable.
///
/// We build a reference graph over units by scanning each unit's text for
/// identifier tokens, then keep only units reachable from `Elm`, the program's
/// single entry/export object. The effect runtime dispatches on Cmd/Sub
/// *variant tags* inside `_Platform_initialize` (reached transitively from `Elm`
/// via `_Platform_wrap`) and names every executor directly — there is no
/// string-keyed manager registry — so a textual reference graph is a sound
/// over-approximation: over-matching a name inside a comment or string only
/// keeps extra code, it never drops something live. Set `ALM_NO_DCE=1` to emit
/// the whole kernel.
/// Returns the shaken bundle and an old-line → new-line map (indexed by
/// pre-shake 0-based line number; `None` for a line in a dropped unit). Live
/// units are re-emitted verbatim, so columns are unchanged and only line numbers
/// shift — which lets a source map built against the pre-shake bundle be
/// remapped onto the shaken one.
fn tree_shake(bundle: &str) -> (String, Vec<Option<u32>>) {
    // The name bound by a column-0 `var NAME`/`function NAME`, if any.
    fn def_name(line: &str) -> Option<&str> {
        let rest = line
            .strip_prefix("var ")
            .or_else(|| line.strip_prefix("function "))?;
        let end = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '$'))
            .unwrap_or(rest.len());
        (end > 0).then(|| &rest[..end])
    }

    let lines: Vec<&str> = bundle.lines().collect();

    // Unit starts: column-0 `var`/`function` defs, and cyclic-value `try` blocks.
    let starts: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter_map(|(i, l)| (def_name(l).is_some() || *l == "try {").then_some(i))
        .collect();
    if starts.is_empty() {
        let identity = (0..lines.len() as u32).map(Some).collect();
        return (bundle.to_string(), identity);
    }
    let span = |k: usize| (starts[k], starts.get(k + 1).copied().unwrap_or(lines.len()));

    // The names each unit defines, and a name -> owning-unit map. A single-def
    // unit binds one name; a `try` block binds every function-scoped `var NAME`
    // inside it (found at any indentation).
    let mut name_to_unit: HashMap<&str, usize> = HashMap::new();
    for k in 0..starts.len() {
        let (s, e) = span(k);
        if let Some(n) = def_name(lines[s]) {
            name_to_unit.insert(n, k);
        } else {
            for line in &lines[s..e] {
                if let Some(n) = def_name(line.trim_start()) {
                    name_to_unit.insert(n, k);
                }
            }
        }
    }

    // Reference graph: which units each unit names in its body.
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); starts.len()];
    for k in 0..starts.len() {
        let (s, e) = span(k);
        let mut token = String::new();
        let flush = |token: &mut String, adj: &mut Vec<usize>| {
            if !token.is_empty() {
                if let Some(&t) = name_to_unit.get(token.as_str()) {
                    if t != k {
                        adj.push(t);
                    }
                }
                token.clear();
            }
        };
        for line in &lines[s..e] {
            for c in line.chars() {
                if c.is_ascii_alphanumeric() || c == '_' || c == '$' {
                    token.push(c);
                } else {
                    flush(&mut token, &mut adj[k]);
                }
            }
            flush(&mut token, &mut adj[k]);
        }
    }

    // Reachability from `Elm` — the object the bundle exports and the loader
    // boots from (`Elm.Main.main.init(...)`).
    let mut live = vec![false; starts.len()];
    let mut stack: Vec<usize> = Vec::new();
    if let Some(&root) = name_to_unit.get("Elm") {
        live[root] = true;
        stack.push(root);
    }
    while let Some(k) = stack.pop() {
        for &t in &adj[k] {
            if !live[t] {
                live[t] = true;
                stack.push(t);
            }
        }
    }

    // Re-emit prologue (before the first unit) + live units, in source order,
    // recording where each surviving old line lands.
    let mut out = String::with_capacity(bundle.len());
    let mut old_to_new: Vec<Option<u32>> = vec![None; lines.len()];
    let mut new_line: u32 = 0;
    for i in 0..starts[0] {
        old_to_new[i] = Some(new_line);
        out.push_str(lines[i]);
        out.push('\n');
        new_line += 1;
    }
    for k in 0..starts.len() {
        if !live[k] {
            continue;
        }
        let (s, e) = span(k);
        for i in s..e {
            old_to_new[i] = Some(new_line);
            out.push_str(lines[i]);
            out.push('\n');
            new_line += 1;
        }
    }
    (out, old_to_new)
}

fn mangle_module(name: &Name) -> String {
    format!("${}", name.as_str().replace('.', "$"))
}

/// JavaScript reserved words that are legal Elm identifiers.
fn sanitize(name: &str) -> String {
    // Mirrors elm's list (Generate.JavaScript.Name.reservedWords). Includes the
    // strict-mode "future reserved words" (interface/implements/package/private/
    // protected/public) and the literals false/true/null — all legal lowercase
    // Elm identifiers but reserved in the strict-mode bundle we emit. elm mangles
    // these the same way: prefix with `_`.
    match name {
        "arguments" | "await" | "break" | "case" | "catch" | "class" | "const" | "continue"
        | "debugger" | "default" | "delete" | "do" | "else" | "enum" | "eval" | "export"
        | "extends" | "false" | "finally" | "for" | "function" | "if" | "implements" | "import"
        | "in" | "instanceof" | "interface" | "let" | "new" | "null" | "package" | "private"
        | "protected" | "public" | "return" | "static" | "super" | "switch" | "this" | "throw"
        | "true" | "try" | "typeof" | "var" | "void" | "while" | "with" | "yield" => {
            format!("_{}", name)
        }
        _ => name.to_string(),
    }
}

/// Generated code plus the source mappings inside it, as byte offsets into
/// `text` (rebased to absolute output positions when flushed). This lets the
/// expression emitters build strings compositionally while carrying each
/// (sub-)expression's source region along — turned into real mappings only when
/// a top-level definition's value is written to the output.
#[derive(Default)]
struct Mapped {
    text: String,
    maps: Vec<(usize, Region)>,
}

impl Mapped {
    fn raw(s: impl Into<String>) -> Mapped {
        Mapped {
            text: s.into(),
            maps: Vec::new(),
        }
    }
    fn push_str(&mut self, s: &str) {
        self.text.push_str(s);
    }
    /// Append `child`, rebasing its maps by the current text length.
    fn push(&mut self, child: Mapped) {
        let base = self.text.len();
        self.text.push_str(&child.text);
        self.maps
            .extend(child.maps.into_iter().map(|(o, r)| (base + o, r)));
    }
    /// Record that this whole piece starts at `region` (offset 0).
    fn mark(&mut self, region: Region) {
        self.maps.push((0, region));
    }
    /// Like [`mark`], but takes priority at offset 0 over any inner mapping
    /// already there (stable sort + dedup keep the first-inserted). Used so a
    /// generated definition's start maps to the definition, not to its body's
    /// first sub-expression.
    fn lead(&mut self, region: Region) {
        self.maps.insert(0, (0, region));
    }
}

struct Generator {
    out: String,
    /// The module whose declarations are being emitted; set before any
    /// definition is generated.
    module_name: Option<Name>,
    temp_counter: usize,
    /// Inferred type of every expression in the current module, keyed by
    /// region. Lets comparison operators inline to native JS `<` etc. when
    /// the operands are scalar comparables (the common, hot case). Empty when
    /// types are unavailable — then comparisons fall back to `_Utils_cmp`.
    node_types: HashMap<Region, can::Type>,
    /// Names of the *value* members of the cyclic top-level group currently
    /// being emitted. References to these are compiled to lazy thunk calls
    /// (`$Module$cyclic$x()`) because the value may not be initialized yet.
    /// Empty except while emitting the bodies of such a group.
    cyclic_values: HashSet<Name>,
    // --- source maps (inactive unless `maps_on`) ---
    maps_on: bool,
    sm: SourceMap,
    /// Lazy cursor over `out`: `(line, col)` reached and how far `out` has been
    /// scanned. Advanced on demand at each `map_here`, so ordinary emission
    /// stays untouched.
    cur_line: u32,
    cur_col: u32,
    scanned: usize,
    /// Source index of the module currently being emitted; `None` for a module
    /// with no retained source (mappings for its defs are skipped).
    src_idx: Option<u32>,
}

impl Generator {
    fn module_name(&self) -> &Name {
        self.module_name
            .as_ref()
            .expect("module context is set before emitting declarations")
    }

    fn global(&self, name: &Name) -> String {
        format!("{}${}", mangle_module(self.module_name()), sanitize(name))
    }

    /// The name of the lazy thunk for a cyclic value: `$Module$cyclic$name`.
    fn cyclic_global(&self, name: &Name) -> String {
        format!(
            "{}$cyclic${}",
            mangle_module(self.module_name()),
            sanitize(name)
        )
    }

    fn fresh_temp(&mut self) -> String {
        self.temp_counter += 1;
        format!("_v{}", self.temp_counter)
    }

    /// Advance the lazy `(line, col)` cursor over any output appended since the
    /// last sync. Counts chars (≈ UTF-16 units for the BMP, which the source-map
    /// format wants) — an approximation on astral chars in string literals.
    fn sync_cursor(&mut self) {
        for ch in self.out[self.scanned..].chars() {
            if ch == '\n' {
                self.cur_line += 1;
                self.cur_col = 0;
            } else {
                self.cur_col += 1;
            }
        }
        self.scanned = self.out.len();
    }

    /// Append a [`Mapped`] to the output, recording each of its offset→region
    /// entries as an absolute mapping. A single left-to-right scan of the text
    /// advances the position, so this is O(text). No-op recording when maps are
    /// off or the module has no source (the text is still emitted).
    fn flush(&mut self, m: Mapped) {
        if !self.maps_on {
            self.out.push_str(&m.text);
            return;
        }
        let Some(src) = self.src_idx else {
            self.out.push_str(&m.text);
            return;
        };
        self.sync_cursor();
        let mut maps = m.maps;
        maps.sort_by_key(|(off, _)| *off);
        let (mut line, mut col, mut pos) = (self.cur_line, self.cur_col, 0usize);
        for (off, region) in maps {
            for ch in m.text[pos..off].chars() {
                if ch == '\n' {
                    line += 1;
                    col = 0;
                } else {
                    col += 1;
                }
            }
            pos = off;
            if let Some((src_line, src_col)) = region_start(&region) {
                self.sm.add(line, col, src, src_line, src_col);
            }
        }
        self.out.push_str(&m.text);
    }

    // UNIONS

    fn union(&mut self, union: &can::Union) {
        let module_var = mangle_module(self.module_name());
        for ctor in &union.ctors {
            emit_ctor(&mut self.out, &module_var, ctor.name.as_str(), ctor.args.len());
        }
        self.out.push('\n');
    }

    // PORTS

    fn port_decl(&mut self, port: &can::PortDecl) {
        let var = self.global(&port.name);
        match &port.tipe {
            // Outgoing: `name : payload -> Cmd msg`
            can::Type::Lambda(payload, result)
                if matches!(&**result, can::Type::Type(_, n, _) if n.as_str() == "Cmd") =>
            {
                writeln!(
                    self.out,
                    "var {} = _Platform_outgoingPort('{}', {});",
                    var,
                    port.name,
                    js_converter(payload, PortDir::ToJs)
                )
                .unwrap();
            }
            // Incoming: `name : (payload -> msg) -> Sub msg`
            can::Type::Lambda(handler, result)
                if matches!(&**result, can::Type::Type(_, n, _) if n.as_str() == "Sub") =>
            {
                let payload = match &**handler {
                    can::Type::Lambda(payload, _) => js_converter(payload, PortDir::FromJs),
                    _ => "function (v) { return v; }".to_string(),
                };
                writeln!(
                    self.out,
                    "var {} = _Platform_incomingPort('{}', {});",
                    var, port.name, payload
                )
                .unwrap();
            }
            _ => {
                // The type checker enforces port shapes are one of the two
                // above; anything else would be an alm bug.
                writeln!(
                    self.out,
                    "var {} = function () {{ throw new Error('bad port {}'); }};",
                    var, port.name
                )
                .unwrap();
            }
        }
    }

    // EFFECT MANAGERS

    /// Emit the `command`/`subscription` leaf helpers and register the effect
    /// manager, mirroring elm's `generateManager`. `command`/`subscription`
    /// wrap a user value into a `_Platform_leaf(home)` bag; `createManager`
    /// stores the manager's `init`/`onEffects`/`onSelfMsg`/`cmdMap`/`subMap`.
    fn manager_decl(&mut self, module: &can::Module) {
        let Some(manager) = &module.manager else {
            return;
        };
        let home = module.name.as_str();
        let mv = mangle_module(&module.name);
        let leaf = |out: &mut String, kind: &str| {
            writeln!(out, "var {mv}${kind} = _Platform_leaf('{home}');").unwrap();
        };
        // createManager(init, onEffects, onSelfMsg, cmdMap, subMap). A Sub-only
        // manager passes 0 for the absent cmdMap so the runtime's dispatch picks
        // the subs argument (matches elm's generateManagerHelp).
        let args = match manager {
            can::Manager::Cmd(_) => {
                leaf(&mut self.out, "command");
                format!("{mv}$init, {mv}$onEffects, {mv}$onSelfMsg, {mv}$cmdMap")
            }
            can::Manager::Sub(_) => {
                leaf(&mut self.out, "subscription");
                format!("{mv}$init, {mv}$onEffects, {mv}$onSelfMsg, 0, {mv}$subMap")
            }
            can::Manager::Fx(_, _) => {
                leaf(&mut self.out, "command");
                leaf(&mut self.out, "subscription");
                format!("{mv}$init, {mv}$onEffects, {mv}$onSelfMsg, {mv}$cmdMap, {mv}$subMap")
            }
        };
        writeln!(
            self.out,
            "_Platform_effectManagers['{home}'] = _Platform_createManager({args});"
        )
        .unwrap();
    }

    // DEFINITIONS

    fn top_level_def(&mut self, def: &can::Def) {
        let var = self.global(&def.name.value);
        let mut value = self.def_value(def, SelfRef::TopLevel);
        // The value's start maps to the definition; flush records that plus
        // every sub-expression mapping the body carries. Bytes are identical to
        // `writeln!("var {} = {};")`.
        value.lead(def.name.region);
        write!(self.out, "var {} = ", var).unwrap();
        self.flush(value);
        self.out.push_str(";\n");
    }

    /// Emit a group of mutually recursive top-level definitions.
    ///
    /// Function members never run at initialization, so they are emitted as
    /// ordinary `var f = function ...` bindings. Value members would run
    /// eagerly, so a legal cycle among them is broken exactly as Elm does:
    /// each value becomes a lazy thunk `function $M$cyclic$x() { return ...; }`
    /// whose references to sibling values are thunk calls; the values are then
    /// forced in order inside a `try`, and each thunk is replaced by one that
    /// returns the now-computed value (memoization). A genuine infinite
    /// recursion surfaces as the caught stack overflow.
    fn recursive_group(&mut self, defs: &[can::Def]) {
        let is_function = |def: &can::Def| {
            !def.args.is_empty() || matches!(def.body.value, can::Expr_::Lambda(..))
        };
        let values: Vec<&can::Def> = defs.iter().filter(|d| !is_function(d)).collect();

        // Purely mutually-recursive functions need no special handling: their
        // bodies are deferred, so emission order is irrelevant.
        if values.is_empty() {
            for def in defs {
                self.top_level_def(def);
            }
            return;
        }

        self.cyclic_values = values.iter().map(|d| d.name.value.clone()).collect();

        // Function members first: a value's thunk may call them while forcing.
        for def in defs {
            if is_function(def) {
                self.top_level_def(def);
            }
        }
        // Lazy thunks for the value members.
        for def in &values {
            let thunk = self.cyclic_global(&def.name.value);
            let mut body = self.expr(&def.body);
            body.lead(def.name.region);
            write!(self.out, "function {}() {{ return ", thunk).unwrap();
            self.flush(body);
            self.out.push_str("; }\n");
        }
        // Force the values in order, memoizing each thunk once computed.
        self.out.push_str("try {\n");
        for def in &values {
            let var = self.global(&def.name.value);
            let thunk = self.cyclic_global(&def.name.value);
            writeln!(self.out, "  var {} = {}();", var, thunk).unwrap();
            writeln!(self.out, "  {} = function () {{ return {}; }};", thunk, var).unwrap();
        }
        let names = values
            .iter()
            .map(|d| d.name.value.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let module = self.module_name().clone();
        writeln!(
            self.out,
            "}} catch ($) {{ throw new Error('Some top-level definitions from `{}` are causing infinite recursion: {}'); }}",
            module,
            names
        )
        .unwrap();

        self.cyclic_values.clear();
    }

    /// The JS expression for a definition: a function wrapper when it has
    /// arguments, otherwise its body.
    fn def_value(&mut self, def: &can::Def, self_ref: SelfRef) -> Mapped {
        if !def.args.is_empty() {
            return self.function_named(Some((&def.name.value, self_ref)), &def.args, &def.body);
        }
        // `f = \a b -> ...` still gets tail-call optimization.
        if let can::Expr_::Lambda(args, body) = &def.body.value {
            return self.function_named(Some((&def.name.value, self_ref)), args, body);
        }
        self.expr(&def.body)
    }

    fn function(&mut self, args: &[can::Pattern], body: &can::Expr) -> Mapped {
        self.function_named(None, args, body)
    }

    /// Generate a function. When `self_ref` is given and the body contains
    /// tail calls to itself, compile the recursion into a `while` loop —
    /// the port of Elm's TailDef optimization.
    fn function_named(
        &mut self,
        self_ref: Option<(&Name, SelfRef)>,
        args: &[can::Pattern],
        body: &can::Expr,
    ) -> Mapped {
        let mut params = Vec::new();
        let mut prelude = String::new();
        for arg in args {
            match &arg.value {
                can::Pattern_::Var(name) => params.push(sanitize(name)),
                _ => {
                    let temp = self.fresh_temp();
                    let mut bindings = Vec::new();
                    destructure(arg, &temp, &mut bindings);
                    for (name, path) in bindings {
                        write!(prelude, "var {} = {}; ", sanitize(&name), path).unwrap();
                    }
                    params.push(temp);
                }
            }
        }
        let arity = params.len();

        let is_tail_recursive = self_ref
            .as_ref()
            .is_some_and(|(name, self_ref)| {
                has_self_tail_call(name, *self_ref, arity, body)
            });

        // Build the body as a Mapped so sub-expression positions survive.
        let mut body_js = Mapped::default();
        if is_tail_recursive {
            let (name, self_kind) = self_ref.unwrap();
            let tail = Tail::Loop {
                name: name.clone(),
                self_kind,
                params: params.clone(),
            };
            body_js.push_str("while (true) { ");
            body_js.push_str(&prelude);
            body_js.push(self.stmts(body, &tail));
            body_js.push_str(" }");
        } else {
            body_js.push_str(&prelude);
            body_js.push(self.stmts(body, &Tail::Return));
        }

        let mut inner = Mapped::raw(format!("function ({}) {{ ", params.join(", ")));
        inner.push(body_js);
        inner.push_str(" }");
        if arity == 1 {
            inner
        } else {
            let mut wrapped = Mapped::raw(format!("F{}(", arity));
            wrapped.push(inner);
            wrapped.push_str(")");
            wrapped
        }
    }

    // STATEMENTS — function bodies are statements so that `if`, `case`,
    // and `let` in tail position produce plain returns (and tail recursion
    // can `continue` the surrounding loop).

    fn stmts(&mut self, expr: &can::Expr, tail: &Tail) -> Mapped {
        use can::Expr_::*;
        match &expr.value {
            If(branches, otherwise) => {
                let mut out = Mapped::default();
                for (condition, branch) in branches {
                    out.push_str("if (");
                    out.push(self.expr(condition));
                    out.push_str(") { ");
                    out.push(self.stmts(branch, tail));
                    out.push_str(" } else ");
                }
                out.push_str("{ ");
                out.push(self.stmts(otherwise, tail));
                out.push_str(" }");
                out
            }
            Let(decls, body) => {
                let mut out = Mapped::default();
                for decl in decls {
                    self.let_decl_stmts(decl, &mut out);
                }
                out.push(self.stmts(body, tail));
                out
            }
            Case(scrutinee, branches) => {
                let temp = self.fresh_temp();
                let mut out = Mapped::raw(format!("var {} = ", temp));
                out.push(self.expr(scrutinee));
                out.push_str("; ");
                // `ALM_NO_CASE_OPT=1` forces the plain sequential if-chain
                // (jump tables + decision trees off) — a safety switch and the
                // baseline for benchmarking the optimization.
                if case_opt_enabled() {
                    if let Some(sw) = self.case_switch(&temp, branches, tail) {
                        out.push(sw);
                        return out;
                    }
                    let patterns: Vec<can::Pattern> =
                        branches.iter().map(|(p, _)| p.clone()).collect();
                    if let Some(tree) = crate::decision::compile(&patterns) {
                        out.push(self.emit_decision(&tree, &temp, branches, tail));
                        return out;
                    }
                }
                for (pattern, branch) in branches {
                    let mut tests = Vec::new();
                    let mut bindings = Vec::new();
                    pattern_tests(pattern, &temp, &mut tests, &mut bindings);
                    let mut body = Mapped::default();
                    for (name, path) in bindings {
                        body.push_str(&format!("var {} = {}; ", sanitize(&name), path));
                    }
                    body.push(self.stmts(branch, tail));
                    if tests.is_empty() {
                        out.push(body);
                        return out;
                    }
                    out.push_str(&format!("if ({}) {{ ", tests.join(" && ")));
                    out.push(body);
                    out.push_str(" } ");
                }
                out.push_str(
                    "throw new Error('Missing case branch (compiler bug: exhaustiveness checking should have caught this)');",
                );
                out
            }
            Call(func, call_args) => {
                if let Tail::Loop {
                    name,
                    self_kind,
                    params,
                } = tail
                {
                    if is_self_ref(func, name, *self_kind) && call_args.len() == params.len() {
                        // Compute all new arguments before reassigning.
                        let mut out = Mapped::default();
                        let mut temps: Vec<String> = Vec::new();
                        for arg in call_args {
                            let temp = self.fresh_temp();
                            out.push_str(&format!("var {} = ", temp));
                            out.push(self.expr(arg));
                            out.push_str("; ");
                            temps.push(temp);
                        }
                        for (param, temp) in params.iter().zip(temps) {
                            out.push_str(&format!("{} = {}; ", param, temp));
                        }
                        out.push_str("continue;");
                        return out;
                    }
                }
                let mut out = Mapped::raw("return ");
                out.push(self.expr(expr));
                out.push_str(";");
                out
            }
            _ => {
                let mut out = Mapped::raw("return ");
                out.push(self.expr(expr));
                out.push_str(";");
                out
            }
        }
    }

    /// Compile a `case` as a `switch` when every branch dispatches on one shared
    /// discriminant — a constructor tag or an int/string/char literal — with an
    /// optional trailing catch-all. This gives V8 a jump table instead of a
    /// linear `if`/`else if` chain. Returns `None` (fall back to the chain) for
    /// anything else: mixed discriminant kinds, refutable sub-patterns, repeated
    /// labels, a non-final catch-all, or fewer than two discriminant branches.
    /// The scrutinee is already evaluated into `temp`.
    fn case_switch(
        &mut self,
        temp: &str,
        branches: &[(can::Pattern, can::Expr)],
        tail: &Tail,
    ) -> Option<Mapped> {
        // Which kind of discriminant (all branches must agree), and the label +
        // branch for each, plus at most one trailing catch-all.
        #[derive(PartialEq)]
        enum Kind {
            Ctor,
            Int,
            Str,
            Chr,
        }
        let kind_of = |d: &Disc| match d {
            Disc::Ctor(_) => Kind::Ctor,
            Disc::Int(_) => Kind::Int,
            Disc::Str(_) => Kind::Str,
            Disc::Chr(_) => Kind::Chr,
        };
        let label_of = |d: &Disc| match d {
            Disc::Ctor(n) => format!("'{}'", n),
            Disc::Int(n) => n.to_string(),
            Disc::Str(s) => js_string(s),
            Disc::Chr(c) => js_string(&c.to_string()),
        };

        let mut kind: Option<Kind> = None;
        let mut labels: Vec<String> = Vec::new();
        let mut cases: Vec<(String, &can::Pattern, &can::Expr)> = Vec::new();
        let mut catch_all: Option<(&can::Pattern, &can::Expr)> = None;
        for (i, (pattern, branch)) in branches.iter().enumerate() {
            if catch_all.is_some() {
                // A catch-all must be the last branch; anything after it means
                // this is not a clean switch (and later branches are dead).
                return None;
            }
            if let Some(disc) = top_discriminant(pattern) {
                let k = kind_of(&disc);
                match &kind {
                    Some(existing) if *existing != k => return None,
                    _ => kind = Some(k),
                }
                let label = label_of(&disc);
                if labels.contains(&label) {
                    return None; // repeated label — needs fall-through handling
                }
                labels.push(label.clone());
                cases.push((label, pattern, branch));
            } else if is_irrefutable(pattern) {
                catch_all = Some((pattern, branch));
            } else {
                let _ = i;
                return None; // a refutable non-discriminant pattern
            }
        }
        if cases.len() < 2 {
            return None;
        }
        let kind = kind?;

        // `switch` on the discriminant expression.
        let disc_expr = match kind {
            Kind::Ctor => format!("{}.$", temp),
            Kind::Int | Kind::Str => temp.to_string(),
            Kind::Chr => format!("{}.valueOf()", temp),
        };
        let mut out = Mapped::raw(format!("switch ({}) {{ ", disc_expr));
        for (label, pattern, branch) in cases {
            out.push_str(&format!("case {}: {{ ", label));
            let mut tests = Vec::new();
            let mut bindings = Vec::new();
            pattern_tests(pattern, temp, &mut tests, &mut bindings);
            for (name, path) in bindings {
                out.push_str(&format!("var {} = {}; ", sanitize(&name), path));
            }
            // Bodies are in tail position, so they end in `return`/`continue`;
            // no `break` is needed and cases never fall through.
            out.push(self.stmts(branch, tail));
            out.push_str(" } ");
        }
        out.push_str("default: { ");
        match catch_all {
            Some((pattern, branch)) => {
                let mut tests = Vec::new();
                let mut bindings = Vec::new();
                pattern_tests(pattern, temp, &mut tests, &mut bindings);
                for (name, path) in bindings {
                    out.push_str(&format!("var {} = {}; ", sanitize(&name), path));
                }
                out.push(self.stmts(branch, tail));
            }
            None => out.push_str(
                "throw new Error('Missing case branch (compiler bug: exhaustiveness checking should have caught this)');",
            ),
        }
        out.push_str(" } }");
        Some(out)
    }

    /// Emit a decision tree (see `crate::decision`). A branch reachable from
    /// several leaves is emitted once as a shared *join point*: a labeled block
    /// whose body sits just after it, entered by `break` from each of those
    /// leaves. (A branch's variables bind to the same paths at every leaf, so the
    /// shared body is identical.) Branches reached from a single leaf inline.
    fn emit_decision(
        &mut self,
        tree: &crate::decision::Tree,
        root: &str,
        branches: &[(can::Pattern, can::Expr)],
        tail: &Tail,
    ) -> Mapped {
        let counts = crate::decision::leaf_counts(tree, branches.len());
        let shared: Vec<usize> = (0..branches.len()).filter(|&b| counts[b] > 1).collect();
        if shared.is_empty() {
            return self.emit_dtree(tree, root, branches, tail, &std::collections::HashMap::new());
        }
        // A label per shared branch, plus its (identical) leaf bindings.
        let mut labels: std::collections::HashMap<usize, String> = std::collections::HashMap::new();
        let mut binds: std::collections::HashMap<usize, Vec<(crate::data::Name, crate::decision::Path)>> =
            std::collections::HashMap::new();
        for &b in &shared {
            labels.insert(b, self.fresh_temp());
            binds.insert(b, first_leaf_binds(tree, b).unwrap_or_default());
        }
        // Nested labeled blocks: shared[0] innermost (wraps the decider), each
        // shared branch's body placed just after its block's `}`.
        let mut out = Mapped::default();
        for &b in shared.iter().rev() {
            out.push_str(&format!("{}: {{ ", labels[&b]));
        }
        out.push(self.emit_dtree(tree, root, branches, tail, &labels));
        for &b in &shared {
            out.push_str(" } "); // close this branch's block; its body follows
            for (name, path) in &binds[&b] {
                out.push_str(&format!("var {} = {}; ", sanitize(name), path_expr(root, path)));
            }
            out.push(self.stmts(&branches[b].1, tail));
        }
        out
    }

    /// Recursive decision-tree walker. `labels` maps a shared branch to the block
    /// it jumps to (via `break`); a leaf whose branch is not in `labels` inlines.
    fn emit_dtree(
        &mut self,
        tree: &crate::decision::Tree,
        root: &str,
        branches: &[(can::Pattern, can::Expr)],
        tail: &Tail,
        labels: &std::collections::HashMap<usize, String>,
    ) -> Mapped {
        use crate::decision::{Test, Tree};
        match tree {
            Tree::Leaf { branch, binds } => {
                if let Some(label) = labels.get(branch) {
                    // Shared: jump to the join point (bindings + body live there).
                    return Mapped::raw(format!("break {};", label));
                }
                let mut out = Mapped::default();
                for (name, path) in binds {
                    out.push_str(&format!(
                        "var {} = {}; ",
                        sanitize(name),
                        path_expr(root, path)
                    ));
                }
                out.push(self.stmts(&branches[*branch].1, tail));
                out
            }
            Tree::Fail => Mapped::raw(
                "throw new Error('Missing case branch (compiler bug: exhaustiveness checking should have caught this)');",
            ),
            Tree::Switch { path, edges, default } => {
                // Bind the tested sub-value to a temp — unless it is the
                // scrutinee itself (empty path), already in `root`.
                let (d, mut out) = if path.is_empty() {
                    (root.to_string(), Mapped::default())
                } else {
                    let d = self.fresh_temp();
                    (d.clone(), Mapped::raw(format!("var {} = {}; ", d, path_expr(root, path))))
                };
                // Bool dispatches on the raw value; everything else on a tag or a
                // primitive, which a `switch` turns into a jump table.
                let is_bool = matches!(
                    edges.first(),
                    Some((Test::Ctor { home, name, .. }, _))
                        if home.as_str() == "Basics" && matches!(name.as_str(), "True" | "False")
                );
                if is_bool {
                    // Exactly True/False — an if/else on the raw boolean.
                    let (true_sub, false_sub) = bool_edges(edges, default);
                    out.push_str(&format!("if ({}) {{ ", d));
                    if let Some(t) = true_sub {
                        out.push(self.emit_dtree(t, root, branches, tail, labels));
                    }
                    out.push_str(" } else { ");
                    if let Some(fsub) = false_sub {
                        out.push(self.emit_dtree(fsub, root, branches, tail, labels));
                    }
                    out.push_str(" }");
                    return out;
                }
                let disc = match edges.first() {
                    Some((Test::Chr(_), _)) => format!("{}.valueOf()", d),
                    Some((Test::Int(_), _)) | Some((Test::Str(_), _)) => d.clone(),
                    _ => format!("{}.$", d), // ctor tag, or list '::' / '[]'
                };
                out.push_str(&format!("switch ({}) {{ ", disc));
                for (test, sub) in edges {
                    out.push_str(&format!("case {}: {{ ", test_label(test)));
                    out.push(self.emit_dtree(sub, root, branches, tail, labels));
                    out.push_str(" } ");
                }
                out.push_str("default: { ");
                match default {
                    Some(d) => out.push(self.emit_dtree(d, root, branches, tail, labels)),
                    None => out.push_str(
                        "throw new Error('Missing case branch (compiler bug: exhaustiveness checking should have caught this)');",
                    ),
                }
                out.push_str(" } }");
                out
            }
        }
    }

    fn let_decl_stmts(&mut self, decl: &can::LetDecl, out: &mut Mapped) {
        match decl {
            can::LetDecl::Def(def) => {
                let value = self.def_value(def, SelfRef::Local);
                out.push_str(&format!("var {} = ", sanitize(&def.name.value)));
                out.push(value);
                out.push_str("; ");
            }
            can::LetDecl::Recursive(defs) => {
                for def in defs {
                    let value = self.def_value(def, SelfRef::Local);
                    out.push_str(&format!("var {} = ", sanitize(&def.name.value)));
                    out.push(value);
                    out.push_str("; ");
                }
            }
            can::LetDecl::Destruct(pattern, value) => {
                let temp = self.fresh_temp();
                out.push_str(&format!("var {} = ", temp));
                out.push(self.expr(value));
                out.push_str("; ");
                let mut bindings = Vec::new();
                destructure(pattern, &temp, &mut bindings);
                for (name, path) in bindings {
                    out.push_str(&format!("var {} = {}; ", sanitize(&name), path));
                }
            }
        }
    }

    // EXPRESSIONS

    fn expr(&mut self, expr: &can::Expr) -> Mapped {
        use can::Expr_::*;
        let mut m = match &expr.value {
            Chr(c) => Mapped::raw(format!("_Utils_chr({})", js_string(&c.to_string()))),
            Str(s) => Mapped::raw(js_string(s)),
            Int(n) => Mapped::raw(n.to_string()),
            Float(f) => {
                let s = f.to_string();
                Mapped::raw(
                    if s.contains('.') || s.contains('e') || s.contains("Infinity") {
                        s
                    } else {
                        format!("{}.0", s)
                    },
                )
            }
            VarLocal(name) => Mapped::raw(sanitize(name)),
            VarTopLevel(name) => {
                // Inside a value cycle, a reference to another value member is
                // a call to its lazy thunk so it is forced on demand.
                if self.cyclic_values.contains(name) {
                    Mapped::raw(format!("{}()", self.cyclic_global(name)))
                } else {
                    Mapped::raw(self.global(name))
                }
            }
            VarForeign(module, name) => Mapped::raw(foreign(module, name)),
            VarCtor(home, _union, ctor) => Mapped::raw(self.ctor_ref(home, ctor)),
            List(items) => {
                if items.is_empty() {
                    Mapped::raw("_List_Nil")
                } else {
                    let mut out = Mapped::raw("_List_fromArray([");
                    for (i, e) in items.iter().enumerate() {
                        if i > 0 {
                            out.push_str(", ");
                        }
                        out.push(self.expr(e));
                    }
                    out.push_str("])");
                    out
                }
            }
            Negate(inner) => {
                let mut out = Mapped::raw("-(");
                out.push(self.expr(inner));
                out.push_str(")");
                out
            }
            Binop(op, home, function, left, right) => {
                self.binop(op, home, function, left, right)
            }
            Lambda(args, body) => match record_ctor_fields(args, body) {
                // A record type-alias constructor used as a value: emit a shared,
                // memoized constructor so `(==)` matches elm (see _Record_ctor).
                Some(fields) => Mapped::raw(format!("_Record_ctor('{}')", fields)),
                None => self.function(args, body),
            },
            Call(func, args) => {
                let func_js = self.expr(func);
                let arg_js: Vec<Mapped> = args.iter().map(|a| self.expr(a)).collect();
                let mut out = Mapped::default();
                if arg_js.len() == 1 {
                    let mut arg_iter = arg_js.into_iter();
                    out.push(callable(func_js));
                    out.push_str("(");
                    out.push(arg_iter.next().unwrap());
                    out.push_str(")");
                } else {
                    out.push_str(&format!("A{}(", arg_js.len()));
                    out.push(func_js);
                    for a in arg_js {
                        out.push_str(", ");
                        out.push(a);
                    }
                    out.push_str(")");
                }
                out
            }
            If(branches, otherwise) => {
                let mut out = Mapped::default();
                for (condition, branch) in branches {
                    out.push_str("(");
                    out.push(self.expr(condition));
                    out.push_str(" ? ");
                    out.push(self.expr(branch));
                    out.push_str(" : ");
                }
                out.push(self.expr(otherwise));
                out.push_str(&")".repeat(branches.len()));
                out
            }
            Let(..) | Case(..) => {
                let mut out = Mapped::raw("(function () { ");
                out.push(self.stmts(expr, &Tail::Return));
                out.push_str(" })()");
                out
            }
            Accessor(field) => Mapped::raw(format!("function ($) {{ return $.{}; }}", field)),
            Access(record, field) => {
                let mut out = self.expr(record);
                out.push_str(&format!(".{}", field.value));
                out
            }
            Update(record, fields) => {
                let mut out = Mapped::raw("_Utils_update(");
                out.push(self.expr(record));
                out.push_str(", { ");
                for (i, (field, value)) in fields.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    out.push_str(&format!("{}: ", field.value));
                    out.push(self.expr(value));
                }
                out.push_str(" })");
                out
            }
            Record(fields) => {
                let mut out = Mapped::raw("{ ");
                for (i, (field, value)) in fields.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    out.push_str(&format!("{}: ", field.value));
                    out.push(self.expr(value));
                }
                out.push_str(" }");
                out
            }
            Unit => Mapped::raw("_Utils_Tuple0"),
            Shader(shader) => {
                // Same object shape Elm's kernel expects: the GLSL source plus
                // a name->name map for each attribute and uniform.
                let obj = |names: &[Name]| -> String {
                    let entries: Vec<String> = names
                        .iter()
                        .map(|n| format!("{}: {}", n, js_string(n.as_str())))
                        .collect();
                    format!("{{{}}}", entries.join(", "))
                };
                Mapped::raw(format!(
                    "{{ src: {}, attributes: {}, uniforms: {} }}",
                    js_string(&shader.src),
                    obj(&shader.attributes),
                    obj(&shader.uniforms)
                ))
            }
            Tuple(a, b, rest) => {
                let mut out = match rest.first() {
                    None => Mapped::raw("{ $: '#2', a: "),
                    Some(_) => Mapped::raw("{ $: '#3', a: "),
                };
                out.push(self.expr(a));
                out.push_str(", b: ");
                out.push(self.expr(b));
                if let Some(c) = rest.first() {
                    out.push_str(", c: ");
                    out.push(self.expr(c));
                }
                out.push_str(" }");
                out
            }
        };
        // Every expression records a mapping at its generated start.
        m.mark(expr.region);
        m
    }

    fn ctor_ref(&mut self, home: &Name, ctor: &can::Ctor) -> String {
        match (home.as_str(), ctor.name.as_str()) {
            ("Basics", "True") => "true".to_string(),
            ("Basics", "False") => "false".to_string(),
            _ if home == self.module_name() => self.global(&ctor.name),
            _ => foreign(home, &ctor.name),
        }
    }

    /// Whether a comparison whose left operand is `left` can use native JS
    /// comparison operators. True only when the operand's inferred type is a
    /// scalar comparable (Int/Float/Char/String); native `<` on those matches
    /// `_Utils_cmp`, but on lists/tuples it does not, so those must stay on the
    /// kernel. Conservative: unknown/absent type => false.
    fn is_scalar_comparison(&self, left: &can::Expr) -> bool {
        matches!(
            self.node_types.get(&left.region),
            Some(can::Type::Type(module, name, args))
                if args.is_empty()
                    && matches!(
                        (module.as_str(), name.as_str()),
                        ("Basics", "Int")
                            | ("Basics", "Float")
                            | ("Char", "Char")
                            | ("String", "String")
                    )
        )
    }

    /// Inline the hot-path operators exactly like Generate/JavaScript does;
    /// fall back to the kernel functions otherwise.
    fn binop(
        &mut self,
        op: &Name,
        home: &Name,
        function: &Name,
        left: &can::Expr,
        right: &can::Expr,
    ) -> Mapped {
        let l = self.expr(left);
        let r = self.expr(right);
        // `pre l mid r suf` — the shape almost every operator takes.
        let bin = |pre: &str, mid: &str, suf: &str, l: Mapped, r: Mapped| {
            let mut m = Mapped::raw(pre);
            m.push(l);
            m.push_str(mid);
            m.push(r);
            m.push_str(suf);
            m
        };
        // Inline `<`, `<=`, `>`, `>=` to native JS operators when the operands
        // are scalar comparables (Int/Float/Char/String) — the hot case. For
        // lists/tuples (or unknown types) fall back to `_Utils_cmp`, which is
        // the only correct choice there. Matches Elm's --optimize codegen.
        let scalar = self.is_scalar_comparison(left);
        match op.as_str() {
            "+" => bin("(", " + ", ")", l, r),
            "-" => bin("(", " - ", ")", l, r),
            "*" => bin("(", " * ", ")", l, r),
            "/" => bin("(", " / ", ")", l, r),
            "//" => bin("((", " / ", ") | 0)", l, r),
            "^" => bin("Math.pow(", ", ", ")", l, r),
            "==" => bin("_Utils_eq(", ", ", ")", l, r),
            "/=" => bin("(!_Utils_eq(", ", ", "))", l, r),
            "<" if scalar => bin("(", " < ", ")", l, r),
            ">" if scalar => bin("(", " > ", ")", l, r),
            "<=" if scalar => bin("(", " <= ", ")", l, r),
            ">=" if scalar => bin("(", " >= ", ")", l, r),
            "<" => bin("(_Utils_cmp(", ", ", ") < 0)", l, r),
            ">" => bin("(_Utils_cmp(", ", ", ") > 0)", l, r),
            "<=" => bin("(_Utils_cmp(", ", ", ") < 1)", l, r),
            ">=" => bin("(_Utils_cmp(", ", ", ") > -1)", l, r),
            "&&" => bin("(", " && ", ")", l, r),
            "||" => bin("(", " || ", ")", l, r),
            "++" => bin("_Utils_ap(", ", ", ")", l, r),
            "::" => bin("_List_Cons(", ", ", ")", l, r),
            "|>" => {
                let mut m = callable(r);
                m.push_str("(");
                m.push(l);
                m.push_str(")");
                m
            }
            "<|" => {
                let mut m = callable(l);
                m.push_str("(");
                m.push(r);
                m.push_str(")");
                m
            }
            _ => {
                let mut m = Mapped::raw(format!("A2({}, ", foreign(home, function)));
                m.push(l);
                m.push_str(", ");
                m.push(r);
                m.push_str(")");
                m
            }
        }
    }
}

/// How a definition refers to itself in its own body.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SelfRef {
    TopLevel,
    Local,
}

enum Tail {
    Return,
    Loop {
        name: Name,
        self_kind: SelfRef,
        params: Vec<String>,
    },
}

fn is_self_ref(expr: &can::Expr, name: &Name, self_kind: SelfRef) -> bool {
    match (&expr.value, self_kind) {
        (can::Expr_::VarTopLevel(n), SelfRef::TopLevel) => n == name,
        (can::Expr_::VarLocal(n), SelfRef::Local) => n == name,
        _ => false,
    }
}

/// Does the body contain a call to itself in tail position?
fn has_self_tail_call(name: &Name, self_kind: SelfRef, arity: usize, body: &can::Expr) -> bool {
    use can::Expr_::*;
    match &body.value {
        Call(func, args) => is_self_ref(func, name, self_kind) && args.len() == arity,
        If(branches, otherwise) => {
            branches
                .iter()
                .any(|(_, b)| has_self_tail_call(name, self_kind, arity, b))
                || has_self_tail_call(name, self_kind, arity, otherwise)
        }
        Let(decls, inner) => {
            // A shadowing let definition would capture the name.
            let shadowed = decls.iter().any(|decl| match decl {
                can::LetDecl::Def(def) => def.name.value == *name,
                can::LetDecl::Recursive(defs) => defs.iter().any(|d| d.name.value == *name),
                can::LetDecl::Destruct(..) => false,
            });
            !shadowed && has_self_tail_call(name, self_kind, arity, inner)
        }
        Case(_, branches) => branches
            .iter()
            .any(|(_, b)| has_self_tail_call(name, self_kind, arity, b)),
        _ => false,
    }
}

/// Wrap a generated function expression in parens when required so it can
/// be called directly.
fn callable(js: Mapped) -> Mapped {
    if js.text.starts_with("function") {
        let mut m = Mapped::raw("(");
        m.push(js);
        m.push_str(")");
        m
    } else {
        js
    }
}

fn foreign(module: &Name, name: &Name) -> String {
    format!("${}${}", module.as_str().replace('.', "$"), sanitize(name))
}

/// Recognize a record type-alias constructor lambda as produced by
/// `record_alias_ctor` in canonicalization: args are `_r0.._r{n-1}` in order and
/// the body is a record whose i-th field's value is exactly `_r{i}`. Returns the
/// comma-joined field names, so codegen can emit a shared memoized constructor
/// (so equality of records built from the constructor matches elm's semantics).
fn record_ctor_fields(args: &[can::Pattern], body: &can::Expr) -> Option<String> {
    let n = args.len();
    if n == 0 {
        return None;
    }
    for (i, arg) in args.iter().enumerate() {
        match &arg.value {
            can::Pattern_::Var(name) if name.as_str() == format!("_r{}", i) => {}
            _ => return None,
        }
    }
    let fields = match &body.value {
        can::Expr_::Record(fields) => fields,
        _ => return None,
    };
    if fields.len() != n {
        return None;
    }
    let mut names = Vec::with_capacity(n);
    for (i, (fname, fexpr)) in fields.iter().enumerate() {
        match &fexpr.value {
            can::Expr_::VarLocal(vn) if vn.as_str() == format!("_r{}", i) => {}
            _ => return None,
        }
        names.push(fname.value.as_str().to_string());
    }
    Some(names.join(","))
}

/// Emit one union constructor: a value for arity 0, otherwise a curried
/// function building the tagged object.
fn emit_ctor(out: &mut String, module_var: &str, ctor_name: &str, arity: usize) {
    let var = format!("{}${}", module_var, sanitize(ctor_name));
    if arity == 0 {
        writeln!(out, "var {} = {{ $: '{}' }};", var, ctor_name).unwrap();
        return;
    }
    let params: Vec<String> = (0..arity).map(field_name).collect();
    let fields: Vec<String> = params.iter().map(|p| format!("{}: {}", p, p)).collect();
    let body = format!(
        "function ({}) {{ return {{ $: '{}', {} }}; }}",
        params.join(", "),
        ctor_name,
        fields.join(", ")
    );
    if arity == 1 {
        writeln!(out, "var {} = {};", var, body).unwrap();
    } else {
        writeln!(out, "var {} = F{}({});", var, arity, body).unwrap();
    }
}

/// The variable bindings of the first leaf for `branch` in `tree`. Every leaf
/// for a given branch binds the same names to the same paths, so a shared join
/// point can use these.
fn first_leaf_binds(
    tree: &crate::decision::Tree,
    branch: usize,
) -> Option<Vec<(crate::data::Name, crate::decision::Path)>> {
    use crate::decision::Tree;
    match tree {
        Tree::Leaf { branch: b, binds } if *b == branch => Some(binds.clone()),
        Tree::Leaf { .. } | Tree::Fail => None,
        Tree::Switch { edges, default, .. } => edges
            .iter()
            .find_map(|(_, sub)| first_leaf_binds(sub, branch))
            .or_else(|| default.as_deref().and_then(|d| first_leaf_binds(d, branch))),
    }
}

/// The JS expression reaching `path` from the scrutinee bound in `root`.
fn path_expr(root: &str, path: &[crate::decision::Step]) -> String {
    use crate::decision::Step;
    let mut s = root.to_string();
    for step in path {
        s = match step {
            // Constructor args and tuple elements are both `.a`/`.b`/... slots.
            Step::Arg(i) | Step::Elem(i) => format!("{}.{}", s, field_name(*i as usize)),
            Step::Field(f) => format!("{}.{}", s, f),
            Step::Head => format!("_List_head({})", s),
            Step::Tail => format!("_List_tail({})", s),
        };
    }
    s
}

/// The `case` label for a decision-tree test (constructor tag / list marker /
/// literal). Not used for bools, which dispatch on the raw value.
fn test_label(test: &crate::decision::Test) -> String {
    use crate::decision::Test;
    match test {
        Test::Ctor { name, .. } => format!("'{}'", name),
        Test::Cons => "'::'".to_string(),
        Test::Nil => "'[]'".to_string(),
        Test::Int(n) => n.to_string(),
        Test::Str(s) => js_string(s),
        Test::Chr(c) => js_string(&c.to_string()),
    }
}

/// Split a bool `Switch`'s edges into its `True` and `False` subtrees, filling a
/// missing side from the `default`.
fn bool_edges<'a>(
    edges: &'a [(crate::decision::Test, crate::decision::Tree)],
    default: &'a Option<Box<crate::decision::Tree>>,
) -> (Option<&'a crate::decision::Tree>, Option<&'a crate::decision::Tree>) {
    use crate::decision::Test;
    let find = |want: &str| {
        edges
            .iter()
            .find(|(t, _)| matches!(t, Test::Ctor { name, .. } if name.as_str() == want))
            .map(|(_, sub)| sub)
    };
    let dflt = default.as_deref();
    (find("True").or(dflt), find("False").or(dflt))
}

fn field_name(index: usize) -> String {
    // a, b, ..., z, a1, b1, ...
    let letter = (b'a' + (index % 26) as u8) as char;
    if index < 26 {
        letter.to_string()
    } else {
        format!("{}{}", letter, index / 26)
    }
}

/// Compute variable bindings for an irrefutable pattern (function args and
/// destructuring lets).
fn destructure(pattern: &can::Pattern, path: &str, bindings: &mut Vec<(String, String)>) {
    let mut tests = Vec::new();
    pattern_tests(pattern, path, &mut tests, bindings);
    // Irrefutable patterns generate no tests (the type checker has made
    // sure of it, except for single-ctor unions which always match).
}

/// Compute the tests and bindings for matching `pattern` against `path`.
/// A single-value discriminant a `case` branch can be dispatched on with a
/// `switch` — the constructor tag, or an int/string/char literal — when the rest
/// of the branch's pattern is irrefutable (so the tag/literal is the only test).
enum Disc {
    Ctor(String),
    Int(i64),
    Str(String),
    Chr(char),
}

/// If `pattern`'s only refutable test is a top-level discriminant (constructor
/// tag or literal) with everything below it irrefutable, return that
/// discriminant. Such branches can share a `switch` whose label is the
/// discriminant and whose body needs no residual test.
fn top_discriminant(pattern: &can::Pattern) -> Option<Disc> {
    use can::Pattern_::*;
    match &pattern.value {
        Int(n) => Some(Disc::Int(*n)),
        Str(s) => Some(Disc::Str(s.clone())),
        Chr(c) => Some(Disc::Chr(*c)),
        Ctor(home, _union, ctor, args) => {
            // Bools compile to `=== true/false`, not a tag; single-ctor unions
            // have no tag test at all. Neither benefits from a switch.
            if (home.as_str() == "Basics" && matches!(ctor.name.as_str(), "True" | "False"))
                || ctor.num_ctors <= 1
            {
                return None;
            }
            if args.iter().all(is_irrefutable) {
                Some(Disc::Ctor(ctor.name.to_string()))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Whether `case` dispatch optimizations (jump tables, decision trees) are on.
/// `ALM_NO_CASE_OPT=1` turns them off, falling back to a sequential if-chain.
fn case_opt_enabled() -> bool {
    std::env::var("ALM_NO_CASE_OPT").is_err()
}

/// A pattern that always matches — it binds variables but performs no test.
fn is_irrefutable(pattern: &can::Pattern) -> bool {
    use can::Pattern_::*;
    match &pattern.value {
        Anything | Var(_) | Unit | Record(_) => true,
        Alias(inner, _) => is_irrefutable(inner),
        Tuple(a, b, rest) => {
            is_irrefutable(a) && is_irrefutable(b) && rest.iter().all(is_irrefutable)
        }
        // A single-constructor union has no tag to test, so it matches on shape
        // alone as long as its arguments do.
        Ctor(_, _, ctor, args) if ctor.num_ctors <= 1 => args.iter().all(is_irrefutable),
        _ => false,
    }
}

fn pattern_tests(
    pattern: &can::Pattern,
    path: &str,
    tests: &mut Vec<String>,
    bindings: &mut Vec<(String, String)>,
) {
    use can::Pattern_::*;
    match &pattern.value {
        Anything | Unit => {}
        Var(name) => bindings.push((name.to_string(), path.to_string())),
        Alias(inner, name) => {
            bindings.push((name.value.to_string(), path.to_string()));
            pattern_tests(inner, path, tests, bindings);
        }
        // A Char scrutinee is a boxed `new String(c)`; unwrap it to compare
        // against the primitive char literal (`new String('a') === "a"` is false).
        Chr(c) => tests.push(format!("{}.valueOf() === {}", path, js_string(&c.to_string()))),
        Str(s) => tests.push(format!("{} === {}", path, js_string(s))),
        Int(n) => tests.push(format!("{} === {}", path, n)),
        Record(fields) => {
            for field in fields {
                bindings.push((
                    field.value.to_string(),
                    format!("{}.{}", path, field.value),
                ));
            }
        }
        Tuple(a, b, rest) => {
            pattern_tests(a, &format!("{}.a", path), tests, bindings);
            pattern_tests(b, &format!("{}.b", path), tests, bindings);
            if let Some(c) = rest.first() {
                pattern_tests(c, &format!("{}.c", path), tests, bindings);
            }
        }
        Ctor(home, _union, ctor, args) => {
            match (home.as_str(), ctor.name.as_str()) {
                ("Basics", "True") => tests.push(format!("{} === true", path)),
                ("Basics", "False") => tests.push(format!("{} === false", path)),
                _ => {
                    if ctor.num_ctors > 1 {
                        tests.push(format!("{}.$ === '{}'", path, ctor.name));
                    }
                }
            }
            for (i, arg) in args.iter().enumerate() {
                pattern_tests(arg, &format!("{}.{}", path, field_name(i)), tests, bindings);
            }
        }
        // Lists are unrolled (chunked), so head/tail are `_List_head`/`_List_tail`
        // calls rather than `.a`/`.b` field accesses. Each is guarded by the
        // preceding `.$ === '::'` test (via `&&` short-circuiting), so the helper
        // is only invoked on a non-empty node.
        List(items) => {
            let mut current = path.to_string();
            for item in items {
                tests.push(format!("{}.$ === '::'", current));
                pattern_tests(item, &format!("_List_head({})", current), tests, bindings);
                current = format!("_List_tail({})", current);
            }
            tests.push(format!("{}.$ === '[]'", current));
        }
        Cons(head, tail) => {
            tests.push(format!("{}.$ === '::'", path));
            pattern_tests(head, &format!("_List_head({})", path), tests, bindings);
            pattern_tests(tail, &format!("_List_tail({})", path), tests, bindings);
        }
    }
}

// PORT CONVERTERS — JS expressions converting between Elm values and the
// plain JS values that flow through ports, driven by the port's type.

/// Direction of a port converter: Elm value -> JS (`ToJs`) or JS -> Elm
/// (`FromJs`).
#[derive(Clone, Copy)]
enum PortDir {
    ToJs,
    FromJs,
}

fn js_converter(tipe: &can::Type, dir: PortDir) -> String {
    use can::Type::*;
    match tipe {
        Type(_, name, args) => match name.as_str() {
            "Int" | "Float" | "Bool" | "String" | "Char" | "Value" => "_Port_id".to_string(),
            "List" => {
                let inner = js_converter(&args[0], dir);
                match dir {
                    PortDir::ToJs => {
                        format!("function (l) {{ return _List_toArray(l).map({}); }}", inner)
                    }
                    PortDir::FromJs => {
                        format!("function (a) {{ return _List_fromArray(a.map({})); }}", inner)
                    }
                }
            }
            "Array" => {
                let inner = js_converter(&args[0], dir);
                match dir {
                    PortDir::ToJs => {
                        format!("function (a) {{ return _Array_toJsArray(a).map({}); }}", inner)
                    }
                    PortDir::FromJs => {
                        format!("function (a) {{ return _Array_fromJsArray(a.map({})); }}", inner)
                    }
                }
            }
            "Maybe" => {
                let inner = js_converter(&args[0], dir);
                match dir {
                    PortDir::ToJs => {
                        format!("function (m) {{ return m.$ === 'Just' ? ({})(m.a) : null; }}", inner)
                    }
                    PortDir::FromJs => format!(
                        "function (v) {{ return v === null || v === undefined ? $Maybe$Nothing : $Maybe$Just(({})(v)); }}",
                        inner
                    ),
                }
            }
            _ => "_Port_id".to_string(),
        },
        Unit => match dir {
            PortDir::ToJs => "function (_v) { return null; }".to_string(),
            PortDir::FromJs => "function (_v) { return _Utils_Tuple0; }".to_string(),
        },
        Record(fields, _) => {
            let parts: Vec<String> = fields
                .iter()
                .map(|(name, t)| format!("{}: ({})(r.{})", name, js_converter(t, dir), name))
                .collect();
            format!("function (r) {{ return {{ {} }}; }}", parts.join(", "))
        }
        Tuple(a, b, c) => match dir {
            PortDir::ToJs => {
                let mut parts = vec![
                    format!("({})(t.a)", js_converter(a, dir)),
                    format!("({})(t.b)", js_converter(b, dir)),
                ];
                if let Some(c) = c {
                    parts.push(format!("({})(t.c)", js_converter(c, dir)));
                }
                format!("function (t) {{ return [{}]; }}", parts.join(", "))
            }
            PortDir::FromJs => match c {
                None => format!(
                    "function (t) {{ return {{ $: '#2', a: ({})(t[0]), b: ({})(t[1]) }}; }}",
                    js_converter(a, dir),
                    js_converter(b, dir)
                ),
                Some(c) => format!(
                    "function (t) {{ return {{ $: '#3', a: ({})(t[0]), b: ({})(t[1]), c: ({})(t[2]) }}; }}",
                    js_converter(a, dir),
                    js_converter(b, dir),
                    js_converter(c, dir)
                ),
            },
        },
        _ => "_Port_id".to_string(),
    }
}

fn js_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        match c {
            '\'' => out.push_str("\\'"),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                write!(out, "\\u{{{:x}}}", c as u32).unwrap();
            }
            c => match crate::parse::decode_lone_surrogate(c) {
                // A smuggled lone surrogate: emit the raw UTF-16 code unit,
                // exactly as stock elm does (e.g. `'\uD800'`).
                Some(surrogate) => write!(out, "\\u{:04X}", surrogate).unwrap(),
                None => out.push(c),
            },
        }
    }
    out.push('\'');
    out
}
