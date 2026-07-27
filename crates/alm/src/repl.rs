//! `alm repl` — port of elm's `Repl.hs`.
//!
//! Each entry is added to an accumulated module and the whole thing is
//! recompiled, exactly as elm does it. That sounds wasteful and is: it is also
//! why a definition can refer to anything typed before it, and why the type
//! shown beside a value is the real inferred type rather than an
//! approximation.
//!
//! Two deliberate differences from elm. There is no line editing — elm uses
//! haskeline for history and arrow keys, and alm reads plain lines, so a
//! terminal's own history is all there is. And `:reset`/`:help`/`:exit` are the
//! whole command set here too, but `elm repl`'s tab completion is not
//! reproduced.

use std::collections::{BTreeMap, HashSet};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use alm_compiler::reporting::doc::{Color, Doc};

/// The module every entry is accumulated into.
const MODULE: &str = "Elm_Repl";

/// The binding an expression is given so it can be printed.
const VALUE_TO_PRINT: &str = "repl_input_value_";

pub fn run(args: &[String], color: bool) -> ExitCode {
    let mut interpreter = "node".to_string();
    let mut color = color;
    for arg in args {
        if arg == "--no-colors" {
            color = false;
        } else if let Some(path) = arg.strip_prefix("--interpreter=") {
            interpreter = path.to_string();
        } else {
            eprintln!("Unknown flag `{arg}`.\n\nUsage: alm repl [--interpreter=node] [--no-colors]");
            return ExitCode::FAILURE;
        }
    }
    if which(&interpreter).is_none() {
        eprintln!(
            "I could not find `{interpreter}` on your PATH, and I need it to run the code you \
             type. Install Node, or point me at something else with --interpreter=<path>."
        );
        return ExitCode::FAILURE;
    }

    let root = match scratch_root() {
        Ok(root) => root,
        Err(err) => {
            eprintln!("I could not set up a place to work: {err}");
            return ExitCode::FAILURE;
        }
    };
    // Everything compiled here is throwaway, and leaving it behind would show
    // up in the user's project.
    let scratch = root.join(".alm-repl");
    if std::fs::create_dir_all(&scratch).is_err() {
        eprintln!("I could not create {}.", scratch.display());
        return ExitCode::FAILURE;
    }
    let _cleanup = Cleanup(scratch.clone());

    print!("{}", welcome(color));
    let _ = std::io::stdout().flush();

    let mut state = State::default();
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    loop {
        match read(&mut lines, color) {
            Input::Exit => return ExitCode::SUCCESS,
            Input::Skip => {}
            Input::Reset => {
                println!("<reset>");
                state = State::default();
            }
            Input::Help(unknown) => print!("{}", help(unknown.as_deref())),
            Input::Port => println!("I cannot handle port declarations."),
            entry => state = evaluate(&scratch, &interpreter, state, entry, color),
        }
    }
}

/// Removes the scratch directory when the REPL ends.
struct Cleanup(PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ----------------------------------------------------------------------- state

/// Everything typed so far. Entries are keyed by name so redefining something
/// replaces it rather than producing a duplicate definition.
#[derive(Default)]
struct State {
    imports: BTreeMap<String, String>,
    types: BTreeMap<String, String>,
    decls: BTreeMap<String, String>,
}

impl State {
    /// The accumulated module, plus the binding to print.
    fn module(&self, output: &Output) -> String {
        let mut out = format!("module {MODULE} exposing (..)\n");
        for source in self.imports.values() {
            out.push_str(source);
            out.push('\n');
        }
        for source in self.types.values() {
            out.push_str(source);
            out.push('\n');
        }
        for source in self.decls.values() {
            out.push_str(source);
            out.push('\n');
        }
        out.push_str(&format!("{VALUE_TO_PRINT} ="));
        match output {
            Output::Nothing | Output::Decl(_) => out.push_str(" ()\n"),
            Output::Expr(expr) => {
                for line in expr.lines() {
                    out.push_str(&format!("\n  {line}"));
                }
                out.push('\n');
            }
        }
        out
    }
}

/// Elm's default imports, and the type names each brings in unqualified. A
/// type from anywhere else prints with its module in front — `Dict.Dict`,
/// until something exposes the bare name.
const DEFAULT_UNQUALIFIED: &[&str] = &[
    "Basics", "List", "Maybe", "Result", "String", "Char", "Platform", "Platform.Cmd",
    "Platform.Sub",
];

impl State {
    /// The modules whose type names are in scope without their prefix: elm's
    /// defaults, plus anything the session imported with an `exposing` list.
    fn unqualified(&self) -> HashSet<String> {
        let mut out: HashSet<String> =
            DEFAULT_UNQUALIFIED.iter().map(|s| s.to_string()).collect();
        // A type declared at the prompt is local, so it never shows a prefix.
        out.insert(MODULE.to_string());
        for (name, source) in &self.imports {
            // `import Dict` qualifies; `import Dict exposing (Dict)` does not.
            // Which *names* were exposed does not have to be worked out here:
            // a module is only asked about for a type it actually defines.
            if source.contains(" exposing ") {
                out.insert(name.clone());
            }
        }
        out
    }
}

enum Output {
    Nothing,
    Decl(String),
    Expr(String),
}

impl Output {
    /// Which binding, if any, should be printed after the module runs.
    fn print_name(&self) -> Option<&str> {
        match self {
            Output::Nothing => None,
            Output::Decl(name) => Some(name),
            Output::Expr(_) => Some(VALUE_TO_PRINT),
        }
    }
}

// ------------------------------------------------------------------ evaluating

/// Compile the module the entry produces and run it. A failed compile leaves
/// the old state alone, so a mistake does not poison the session.
fn evaluate(
    scratch: &Path,
    interpreter: &str,
    state: State,
    entry: Input,
    color: bool,
) -> State {
    let (next, output) = match entry {
        Input::Import(name, source) => {
            let mut next = state;
            next.imports.insert(name, source);
            (next, Output::Nothing)
        }
        Input::Type(name, source) => {
            let mut next = state;
            next.types.insert(name, source);
            (next, Output::Nothing)
        }
        Input::Decl(name, source) => {
            let mut next = state;
            next.decls.insert(name.clone(), source);
            (next, Output::Decl(name))
        }
        Input::Expr(source) => {
            let output = Output::Expr(source);
            (state, output)
        }
        Input::Exit | Input::Skip | Input::Reset | Input::Help(_) | Input::Port => {
            unreachable!("handled in the loop")
        }
    };

    let path = scratch.join(format!("{MODULE}.elm"));
    if std::fs::write(&path, next.module(&output)).is_err() {
        eprintln!("I could not write to {}.", path.display());
        return State::default();
    }
    match alm_compiler::project::compile_repl(
        &path,
        output.print_name(),
        color,
        &next.unqualified(),
    ) {
        Err(errors) => {
            // The accumulated module is an implementation detail, so the
            // header says `REPL` rather than naming the scratch file.
            for error in &errors {
                eprint!("{}", error.render_named("REPL", color));
            }
            // A failed entry is discarded; the previous state is what the
            // reports were checked against and stays valid.
            rollback(next, &output)
        }
        Ok(None) => next,
        Ok(Some(javascript)) => {
            if interpret(interpreter, &javascript) {
                next
            } else {
                rollback(next, &output)
            }
        }
    }
}

/// Undo the entry that just failed.
fn rollback(mut state: State, output: &Output) -> State {
    if let Output::Decl(name) = output {
        state.decls.remove(name);
    }
    state
}

/// Run the generated JavaScript, piping it in on stdin as elm does.
fn interpret(interpreter: &str, javascript: &str) -> bool {
    let Ok(mut child) = Command::new(interpreter).stdin(Stdio::piped()).spawn() else {
        eprintln!("I could not start `{interpreter}`.");
        return false;
    };
    if let Some(mut stdin) = child.stdin.take() {
        // A program that dies before reading it all is reported by its exit
        // status, not by the broken pipe here.
        let _ = stdin.write_all(javascript.as_bytes());
    }
    child.wait().map(|status| status.success()).unwrap_or(false)
}

// -------------------------------------------------------------------- reading

enum Input {
    Import(String, String),
    Type(String, String),
    Decl(String, String),
    Expr(String),
    Port,
    Reset,
    Exit,
    Skip,
    Help(Option<String>),
}

/// Read one entry, continuing onto `|` lines while it is unfinished.
fn read(lines: &mut std::io::Lines<impl BufRead>, color: bool) -> Input {
    print!("{}", prompt("> ", color));
    let _ = std::io::stdout().flush();
    let Some(Ok(first)) = lines.next() else {
        println!();
        return Input::Exit;
    };
    if first.trim().is_empty() {
        return Input::Skip;
    }
    if let Some(command) = first.trim().strip_prefix(':') {
        return match command.trim() {
            "exit" | "quit" => Input::Exit,
            "reset" => Input::Reset,
            "help" => Input::Help(None),
            other => Input::Help(Some(other.to_string())),
        };
    }

    // elm's rule, from `ifDone`/`ifFail`: an entry that parses is finished if
    // it is a single line or ends with a blank one; an entry that does not
    // parse is finished only when a blank line says so. So the moment an entry
    // goes multi-line, a blank line is what ends it — which is why a `let`
    // block needs one even after the body is typed.
    let mut entry = first;
    let mut single_line = true;
    let mut ends_blank = false;
    loop {
        let parses = is_complete(&entry);
        if (parses && (single_line || ends_blank)) || ends_blank {
            return categorize_final(&entry);
        }
        print!("{}", prompt("| ", color));
        let _ = std::io::stdout().flush();
        let Some(Ok(line)) = lines.next() else {
            println!();
            return Input::Skip;
        };
        ends_blank = line.trim().is_empty();
        single_line = false;
        entry.push('\n');
        entry.push_str(&line);
    }
}

fn categorize_final(entry: &str) -> Input {
    let trimmed = entry.trim_start();
    if trimmed.starts_with("import ") {
        let name = trimmed["import ".len()..]
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_string();
        return Input::Import(name, entry.to_string());
    }
    if trimmed.starts_with("port ") {
        return Input::Port;
    }
    if let Some(rest) = trimmed.strip_prefix("type ") {
        let rest = rest.strip_prefix("alias ").unwrap_or(rest);
        let name = rest.split_whitespace().next().unwrap_or("").to_string();
        return Input::Type(name, entry.to_string());
    }
    match declaration_name(trimmed) {
        Some(name) => Input::Decl(name, entry.to_string()),
        None => Input::Expr(entry.to_string()),
    }
}

/// The name a top-level declaration binds: `x = …`, `f a b = …` or a type
/// annotation `x : …`. Returns `None` for anything else, which is an
/// expression.
fn declaration_name(entry: &str) -> Option<String> {
    let first = entry.lines().next()?;
    let mut chars = first.char_indices();
    let (_, start) = chars.next()?;
    if !(start.is_lowercase() || start == '_') {
        return None;
    }
    let mut end = first.len();
    for (i, c) in first.char_indices() {
        if !(c.is_alphanumeric() || c == '_' || c == '\'') {
            end = i;
            break;
        }
    }
    let name = &first[..end];
    if name.is_empty() || KEYWORDS.contains(&name) {
        return None;
    }
    // What follows the name (and any arguments) has to be a binding, not an
    // application: `f x = …` declares, `f x` evaluates.
    let rest = first[end..].trim_start();
    let is_annotation = rest.starts_with(':') && !rest.starts_with("::");
    if is_annotation {
        return Some(name.to_string());
    }
    // Arguments up to an `=` that is not part of `==`, `>=`, `/=` and so on.
    let mut scan = rest;
    loop {
        let scan_trimmed = scan.trim_start();
        if let Some(after) = scan_trimmed.strip_prefix('=') {
            return (!after.starts_with('=')).then(|| name.to_string());
        }
        let word_end = scan_trimmed
            .find(|c: char| c.is_whitespace())
            .unwrap_or(scan_trimmed.len());
        if word_end == 0 {
            return None;
        }
        let word = &scan_trimmed[..word_end];
        // Only argument patterns may appear before the `=`.
        if !word.chars().all(|c| c.is_alphanumeric() || "_'(),{}".contains(c)) {
            return None;
        }
        scan = &scan_trimmed[word_end..];
    }
}

const KEYWORDS: &[&str] = &[
    "if", "then", "else", "case", "of", "let", "in", "type", "module", "where", "import",
    "exposing", "as", "port",
];

/// Whether an entry looks finished: brackets balanced outside strings and
/// comments, and not ending on something that must be followed by more.
fn is_complete(entry: &str) -> bool {
    let mut depth = 0i32;
    let mut in_string = false;
    let mut in_multiline_string = false;
    let mut in_char = false;
    let mut comment = 0i32;
    let chars: Vec<char> = entry.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if comment > 0 {
            if c == '{' && next == Some('-') {
                comment += 1;
                i += 2;
                continue;
            }
            if c == '-' && next == Some('}') {
                comment -= 1;
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        if in_multiline_string {
            if c == '"' && next == Some('"') && chars.get(i + 2) == Some(&'"') {
                in_multiline_string = false;
                i += 3;
                continue;
            }
            i += if c == '\\' { 2 } else { 1 };
            continue;
        }
        if in_string {
            if c == '\\' {
                i += 2;
                continue;
            }
            if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if in_char {
            if c == '\\' {
                i += 2;
                continue;
            }
            if c == '\'' {
                in_char = false;
            }
            i += 1;
            continue;
        }
        match c {
            '-' if next == Some('-') => {
                // A line comment runs to the end of the line.
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            '{' if next == Some('-') => {
                comment = 1;
                i += 2;
                continue;
            }
            '"' if next == Some('"') && chars.get(i + 2) == Some(&'"') => {
                in_multiline_string = true;
                i += 3;
                continue;
            }
            '"' => in_string = true,
            '\'' => in_char = true,
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            _ => {}
        }
        i += 1;
    }
    if depth > 0 || in_string || in_char || in_multiline_string || comment > 0 {
        return false;
    }

    // A trailing keyword or operator means the entry is still going.
    let last_line = entry.lines().next_back().unwrap_or("").trim_end();
    let tail = last_line.trim_start();
    if tail.is_empty() {
        return false;
    }
    let last_word = last_line.split_whitespace().next_back().unwrap_or("");
    if matches!(last_word, "=" | "->" | "let" | "in" | "of" | "if" | "then" | "else" | "case" | "|")
        || last_line.ends_with(',')
    {
        return false;
    }
    // A type annotation on its own is not a declaration — the definition has
    // to follow. Taking it as one would store it under the name it annotates
    // and then let the definition replace it, silently dropping the types
    // just asked for. elm keeps reading here too.
    !is_bare_annotation(entry)
}

/// Whether the entry opens with `name : …` and never goes on to define
/// `name`. A definition starts in the first column; anything indented is
/// still part of the annotation.
fn is_bare_annotation(entry: &str) -> bool {
    let mut lines = entry.lines();
    let Some(first) = lines.next() else { return false };
    if first.starts_with(char::is_whitespace) {
        return false;
    }
    let Some((name, rest)) = first.split_once(':') else { return false };
    let name = name.trim();
    // `x :: xs` is an operator, not an annotation, and only a lower-case
    // simple name can be annotated.
    if rest.starts_with(':') || name.contains(char::is_whitespace) {
        return false;
    }
    if !name.starts_with(|c: char| c.is_lowercase() || c == '_') {
        return false;
    }
    !lines.any(|line| {
        line.strip_prefix(name)
            .is_some_and(|rest| rest.starts_with(|c: char| c.is_whitespace() || c == '='))
    })
}

// -------------------------------------------------------------------- printing

/// elm leaves the prompt uncolored, so the `color` flag is only about what a
/// value is printed in.
fn prompt(text: &str, _color: bool) -> String {
    text.to_string()
}

fn render(doc: Doc, color: bool) -> String {
    if color {
        doc.render_ansi(usize::MAX)
    } else {
        doc.render(usize::MAX)
    }
}

fn welcome(color: bool) -> String {
    let version = env!("CARGO_PKG_VERSION");
    let title = format!("alm {version}");
    let dashes = "-".repeat(74usize.saturating_sub(title.chars().count()));
    let doc = Doc::vcat(vec![
        // The spaces around the title sit outside the colored runs, as elm's
        // do — `<grey>----</grey> <cyan>alm 0.1.0</cyan> <grey>---…</grey>`.
        Doc::concat(vec![
            Doc::color(Color::BlackVivid, Doc::text("----")),
            Doc::text(" "),
            Doc::color(Color::Cyan, Doc::text(title)),
            Doc::text(" "),
            Doc::color(Color::BlackVivid, Doc::text(dashes)),
        ]),
        Doc::color(
            Color::BlackVivid,
            Doc::text("Say :help for help and :exit to exit!"),
        ),
        Doc::color(Color::BlackVivid, Doc::text("-".repeat(80))),
        Doc::text(""),
    ]);
    format!("{}\n", render(doc, color))
}

fn help(unknown: Option<&str>) -> String {
    let head = match unknown {
        Some(command) => format!("I do not recognize the :{command} command. "),
        None => String::new(),
    };
    format!(
        "{head}Valid commands include:\n\
         \n\
         \x20 :exit    Exit the REPL\n\
         \x20 :help    Show this information\n\
         \x20 :reset   Clear all previous imports and definitions\n\
         \n"
    )
}

// --------------------------------------------------------------------- scratch

/// Where to compile. A project directory gives the REPL that project's
/// dependencies; outside one, a scratch application under `~/.elm` gives it
/// the defaults, which is what elm does too.
fn scratch_root() -> std::io::Result<PathBuf> {
    let here = std::env::current_dir()?;
    let found = alm_compiler::project::project_root(Path::new("elm.json"));
    if found.join("elm.json").is_file() {
        return Ok(found);
    }
    let _ = here;
    let root = alm_compiler::packages::packages_root()
        .parent()
        .unwrap_or(Path::new("."))
        .join("repl");
    std::fs::create_dir_all(root.join("src"))?;
    if !root.join("elm.json").is_file() {
        std::fs::write(root.join("elm.json"), default_outline())?;
    }
    Ok(root)
}

/// The dependency set `elm repl` starts with outside a project.
fn default_outline() -> String {
    let versions = |name: &str| {
        alm_compiler::packages::cached_versions(name)
            .pop()
            .map(|v| v.to_string())
            .unwrap_or_else(|| "1.0.0".to_string())
    };
    let direct = ["elm/core", "elm/json", "elm/html"]
        .iter()
        .map(|name| format!("            \"{name}\": \"{}\"", versions(name)))
        .collect::<Vec<_>>()
        .join(",\n");
    format!(
        "{{\n    \"type\": \"application\",\n    \"source-directories\": [\n        \"src\"\n    ],\n\
         \x20   \"elm-version\": \"0.19.1\",\n    \"dependencies\": {{\n        \"direct\": {{\n{direct}\n        }},\n\
         \x20       \"indirect\": {{}}\n    }},\n    \"test-dependencies\": {{\n        \"direct\": {{}},\n\
         \x20       \"indirect\": {{}}\n    }}\n}}\n"
    )
}

fn which(program: &str) -> Option<PathBuf> {
    if program.contains(std::path::MAIN_SEPARATOR) {
        return Path::new(program).is_file().then(|| PathBuf::from(program));
    }
    std::env::var_os("PATH")?
        .to_str()?
        .split(':')
        .map(|dir| Path::new(dir).join(program))
        .find(|path| path.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify(entry: &str) -> &'static str {
        match categorize_final(entry) {
            Input::Import(_, _) => "import",
            Input::Type(_, _) => "type",
            Input::Decl(_, _) => "decl",
            Input::Expr(_) => "expr",
            Input::Port => "port",
            _ => "other",
        }
    }

    #[test]
    fn entries_are_sorted_into_the_right_bucket() {
        assert_eq!(classify("import Html exposing (text)"), "import");
        assert_eq!(classify("type alias Point = { x : Int }"), "type");
        assert_eq!(classify("type Color = Red | Blue"), "type");
        assert_eq!(classify("port send : String -> Cmd msg"), "port");
        assert_eq!(classify("x = 1"), "decl");
        assert_eq!(classify("f a b = a + b"), "decl");
        assert_eq!(classify("x : Int"), "decl");
        assert_eq!(classify("1 + 1"), "expr");
        assert_eq!(classify("List.map"), "expr");
    }

    /// The hard cases: an application is not a declaration, and an operator
    /// that merely contains `=` does not make one either.
    #[test]
    fn an_application_is_an_expression_not_a_declaration() {
        assert_eq!(classify("f x"), "expr");
        assert_eq!(classify("x == 1"), "expr");
        assert_eq!(classify("x /= 1"), "expr");
        assert_eq!(classify("x >= 1"), "expr");
        assert_eq!(classify("String.toUpper \"a\""), "expr");
        assert_eq!(classify("if True then 1 else 2"), "expr");
        assert_eq!(classify("case x of\n  _ -> 1"), "expr");
        assert_eq!(classify("let y = 1 in y"), "expr");
    }

    #[test]
    fn declaration_names_come_out_intact() {
        let name = |s: &str| match categorize_final(s) {
            Input::Decl(name, _) => name,
            Input::Type(name, _) => name,
            Input::Import(name, _) => name,
            _ => String::from("<none>"),
        };
        assert_eq!(name("greet name = name"), "greet");
        assert_eq!(name("x' = 1"), "x'");
        assert_eq!(name("type alias Point = {}"), "Point");
        assert_eq!(name("import Json.Decode as D"), "Json.Decode");
    }

    #[test]
    fn an_unbalanced_entry_asks_for_more() {
        assert!(!is_complete("[1, 2"));
        assert!(!is_complete("{ x ="));
        assert!(!is_complete("case x of"));
        assert!(!is_complete("f x =")); // a body has to follow
        assert!(!is_complete("\"unterminated"));
        assert!(is_complete("[1, 2]"));
        assert!(is_complete("f x = x + 1"));
        assert!(is_complete("case x of\n  _ -> 1"));
        assert!(is_complete("\"a string with ( in it\""));
        assert!(is_complete("'('"));
    }

    /// Brackets inside comments and strings must not be counted.
    #[test]
    fn brackets_in_comments_and_strings_do_not_count() {
        assert!(is_complete("1 -- (\n"));
        assert!(is_complete("1 {- ( -}"));
        assert!(is_complete("\"\"\" ( \"\"\""));
        assert!(!is_complete("1 {- ("));
    }

    #[test]
    fn the_module_accumulates_everything_typed_so_far() {
        let mut state = State::default();
        state.imports.insert("Html".to_string(), "import Html".to_string());
        state.decls.insert("x".to_string(), "x = 1".to_string());
        let source = state.module(&Output::Expr("x + 1".to_string()));
        assert_eq!(
            source,
            format!(
                "module {MODULE} exposing (..)\nimport Html\nx = 1\n{VALUE_TO_PRINT} =\n  x + 1\n"
            )
        );
        // With nothing to print, the binding still exists so the module builds.
        assert!(state.module(&Output::Nothing).ends_with(&format!("{VALUE_TO_PRINT} = ()\n")));
    }

    #[test]
    fn redefining_a_name_replaces_it() {
        let mut state = State::default();
        state.decls.insert("x".to_string(), "x = 1".to_string());
        state.decls.insert("x".to_string(), "x = 2".to_string());
        let source = state.module(&Output::Nothing);
        assert!(source.contains("x = 2"));
        assert!(!source.contains("x = 1"));
    }
}
