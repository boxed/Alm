pub mod annotation;
pub mod doc;
pub mod syntax;
pub mod type_error;

pub use annotation::{Located, Position, Region};
pub use doc::{Chunk, Color, Doc, Style};

/// A rendered compiler error, in the spirit of Elm's friendly error messages.
///
/// Legacy diagnostics (naming/type/pattern) carry a plain `message`. Parse
/// diagnostics additionally carry `elm`, a structured body that reproduces the
/// official compiler's output byte-for-byte (`Reporting.Error.Syntax`).
#[derive(Debug, Clone)]
pub struct Report {
    pub title: String,
    pub region: Region,
    pub message: String,
    pub elm: Option<ElmBody>,
}

/// The structured body of a parse diagnostic, mirroring the `(before, after)`
/// snippet plus trailing notes that `Reporting.Error.Syntax` builds.
#[derive(Debug, Clone)]
pub struct ElmBody {
    /// Paragraph shown before the source snippet.
    pub before: Doc,
    /// Paragraph shown immediately after the snippet (no blank line).
    pub after: Doc,
    /// Extra sections after the snippet block, each separated by a blank line.
    pub notes: Vec<Section>,
    /// The span whose source lines are shown (`Render.Code` `region`).
    pub region: Region,
    /// The sub-region underlined with carets (`Render.Code` highlight); drawn
    /// only when single-line and on the last shown row, matching elm.
    pub highlight: Region,
}

/// One element of a diagnostic body below the snippet.
#[derive(Debug, Clone)]
pub enum Section {
    /// A word-wrapped paragraph (filled to 80 columns), possibly with styled
    /// words inside it.
    Para(Doc),
    /// A verbatim block (indented code examples), emitted as-is.
    Block(String),
}

impl Section {
    /// A plain word-wrapped paragraph.
    pub fn para(text: impl AsRef<str>) -> Section {
        Section::Para(Doc::reflow(text.as_ref()))
    }
}

// ---------------------------------------------------------- styled prose bits
//
// elm's reports color individual words: the thing at fault in dull yellow, the
// thing to use instead in vivid green, an Elm keyword in vivid cyan, and an
// underlined `Hint`/`Note` label. These build those pieces.

/// A word in the color elm uses for "this is the thing at fault".
pub fn yellow(text: impl Into<String>) -> Doc {
    Doc::color(Color::Yellow, Doc::text(text))
}

/// A word in the color elm uses for "use this instead".
pub fn green(text: impl Into<String>) -> Doc {
    Doc::color(Color::GreenVivid, Doc::text(text))
}

/// The color elm uses for Elm keywords quoted inside prose.
pub fn cyan(text: impl Into<String>) -> Doc {
    Doc::color(Color::CyanVivid, Doc::text(text))
}

/// The color elm uses for constructors in example code.
pub fn blue(text: impl Into<String>) -> Doc {
    Doc::color(Color::BlueVivid, Doc::text(text))
}

/// The dimmed color elm uses for illustrative example output.
pub fn grey(text: impl Into<String>) -> Doc {
    Doc::color(Color::BlackVivid, Doc::text(text))
}

/// Plain prose split into words, for mixing with styled ones.
pub fn words(text: &str) -> Vec<Doc> {
    text.split_whitespace().map(Doc::text).collect()
}

/// Build a filled paragraph out of alternating plain and styled pieces.
pub fn sentence(parts: Vec<Doc>) -> Doc {
    Doc::fill_sep(parts)
}

/// `D.toFancyHint` — an underlined `Hint` label, then the words. The colon sits
/// outside the underline.
pub fn hint(parts: Vec<Doc>) -> Section {
    Section::Para(sentence(labeled("Hint", parts)))
}

/// `D.toFancyNote`.
pub fn note(parts: Vec<Doc>) -> Section {
    Section::Para(sentence(labeled("Note", parts)))
}

pub fn labeled(label: &str, parts: Vec<Doc>) -> Vec<Doc> {
    let mut out =
        vec![Doc::cat2(Doc::styled(Style::underline(), Doc::text(label)), Doc::text(":"))];
    out.extend(parts);
    out
}

/// A filled paragraph in which the given tokens are colored. elm colors
/// exactly these — a keyword, a literal, a suggested name — and leaves the rest
/// of the sentence plain.
///
/// A token may be several words (`(\x -> x + 1)` is one yellow run in elm, not
/// five), and may be a prefix of a word, so `Sandwich` colors without dragging
/// in the sentence's full stop. Longest match wins.
pub fn marked(text: &str, marks: &[(&str, fn(String) -> Doc)]) -> Doc {
    let words: Vec<&str> = text.split_whitespace().collect();
    let mut ordered: Vec<&(&str, fn(String) -> Doc)> = marks.iter().collect();
    ordered.sort_by_key(|(token, _)| std::cmp::Reverse(token.len()));

    let mut pieces: Vec<Doc> = Vec::new();
    let mut i = 0;
    'outer: while i < words.len() {
        for (token, style) in &ordered {
            let parts: Vec<&str> = token.split_whitespace().collect();
            // A run of words. The last of them may carry trailing punctuation
            // that stays plain — `(name, _).` colors `(name, _)` and not the
            // sentence's full stop.
            if parts.len() > 1 && i + parts.len() <= words.len() {
                let (last, leading) = parts.split_last().unwrap();
                if words[i..i + leading.len()] == *leading {
                    let tail_word = words[i + leading.len()];
                    if tail_word.starts_with(last)
                        && is_trailing_punctuation(&tail_word[last.len()..])
                    {
                        pieces.push(Doc::cat2(
                            style(token.to_string()),
                            Doc::text(tail_word[last.len()..].to_string()),
                        ));
                        i += parts.len();
                        continue 'outer;
                    }
                }
            }
            if parts.len() == 1 && words[i] == *token {
                pieces.push(style(token.to_string()));
                i += 1;
                continue 'outer;
            }
            // The token opens this word and only punctuation follows, so the
            // sentence's own comma or full stop stays plain. Requiring
            // punctuation is what keeps `in` from matching "indentation".
            if parts.len() == 1 && words[i].starts_with(token) && is_trailing_punctuation(&words[i][token.len()..]) {
                pieces.push(Doc::cat2(
                    style(token.to_string()),
                    Doc::text(words[i][token.len()..].to_string()),
                ));
                i += 1;
                continue 'outer;
            }
        }
        pieces.push(Doc::text(words[i]));
        i += 1;
    }
    sentence(pieces)
}

/// Whether a matched token ends the word, or is followed only by punctuation
/// the sentence owns rather than by more of a longer word. Requiring this is
/// what stops `in` from matching inside "indentation".
fn is_trailing_punctuation(rest: &str) -> bool {
    rest.chars().all(|c| c.is_ascii_punctuation())
}

/// One line of a code example, indented `indent` spaces: the pieces butted
/// together with no wrapping, so the layout elm chose is preserved exactly.
pub fn code_line(indent: usize, pieces: Vec<Doc>) -> Doc {
    let mut all = vec![Doc::text(" ".repeat(indent))];
    all.extend(pieces);
    Doc::concat(all)
}

/// A code example emitted as one colored run, newlines and all — elm colors
/// some examples wholesale rather than token by token.
pub fn colored_block(style: fn(String) -> Doc, text: &str) -> Section {
    Section::Para(style(text.to_string()))
}

/// An indented code example whose keywords, constructors and literals are
/// colored. elm hand-colors each of these — inside an example a type name
/// stays plain, while the same name quoted in prose is yellow — so they are
/// written out rather than run through a highlighter.
pub fn code_block(lines: Vec<Doc>) -> Section {
    Section::Para(Doc::vcat(lines))
}

/// `D.makeLink`.
pub fn link(page: &str) -> Doc {
    Doc::text(format!("<https://elm-lang.org/0.19.1/{page}>"))
}

const WIDTH: usize = 80;

impl Report {
    /// The report as plain text, the way a redirected `alm make` prints it.
    pub fn render(&self, path: &str, source: &str) -> String {
        self.chunks(path, source).into_iter().map(|c| c.text).collect()
    }

    /// The report with ANSI escapes, for a terminal.
    pub fn render_ansi(&self, path: &str, source: &str) -> String {
        let mut out = String::new();
        for chunk in self.chunks(path, source) {
            out.push_str(&chunk.render_ansi());
        }
        out
    }

    /// The report as styled runs — the form `--report=json` encodes and the
    /// other two renderings are folded down from.
    pub fn chunks(&self, path: &str, source: &str) -> Vec<Chunk> {
        let mut out = Chunks::new();
        out.doc(&header(&self.title, path));
        out.plain("\n\n");
        out.extend(self.body_chunks(source));
        out.plain("\n\n");
        out.finish()
    }

    /// The body only — no `-- TITLE ---` bar and no trailing blank line.
    /// `--report=json` carries the title as its own field, so the message must
    /// not repeat it.
    pub fn body_chunks(&self, source: &str) -> Vec<Chunk> {
        let mut out = Chunks::new();
        match &self.elm {
            Some(body) => {
                out.doc(&body.before);
                out.plain("\n\n");
                out.extend(render_snippet(source, body.region, body.highlight));
                out.doc(&body.after);
                for note in &body.notes {
                    out.plain("\n\n");
                    match note {
                        Section::Para(p) => out.doc(p),
                        Section::Block(b) => out.plain(b),
                    }
                }
            }
            None => {
                out.plain(&render_code_snippet(source, self.region));
                out.plain("\n");
                out.plain(&self.message);
            }
        }
        out.finish()
    }
}

impl Chunk {
    /// This run wrapped in its ANSI escapes (nothing if it is unstyled).
    pub fn render_ansi(&self) -> String {
        if self.style == Style::default() {
            return self.text.clone();
        }
        Doc::styled(self.style, Doc::text(self.text.clone())).render_ansi(usize::MAX)
    }
}

/// Accumulates styled runs, merging neighbours that share a style.
struct Chunks(Vec<Chunk>);

impl Chunks {
    fn new() -> Chunks {
        Chunks(Vec::new())
    }

    fn push(&mut self, style: Style, text: &str) {
        if text.is_empty() {
            return;
        }
        match self.0.last_mut() {
            Some(last) if last.style == style => last.text.push_str(text),
            _ => self.0.push(Chunk { style, text: text.to_string() }),
        }
    }

    fn plain(&mut self, text: &str) {
        self.push(Style::default(), text);
    }

    fn doc(&mut self, doc: &Doc) {
        self.extend(doc.chunks(WIDTH));
    }

    fn extend(&mut self, chunks: Vec<Chunk>) {
        for chunk in chunks {
            self.push(chunk.style, &chunk.text);
        }
    }

    fn finish(self) -> Vec<Chunk> {
        self.0
    }
}

/// `-- TITLE --------- path`, padded to 80 columns (`Reporting.Report.toDoc`),
/// in dull cyan.
fn header(title: &str, path: &str) -> Doc {
    // "-- " + title + " " + dashes + " " + path  == WIDTH
    let fixed = 3 + title.len() + 1 + 1 + path.len();
    let dashes = WIDTH.saturating_sub(fixed).max(2);
    Doc::color(Color::Cyan, Doc::text(format!("-- {} {} {}", title, "-".repeat(dashes), path)))
}

/// Greedy word wrap to 80 columns, matching `Reporting.Doc.reflow`. Blank lines
/// in the input separate paragraphs; each paragraph wraps independently.
pub fn reflow(text: &str) -> String {
    let mut paras: Vec<String> = Vec::new();
    for para in text.split("\n\n") {
        let words: Vec<&str> = para.split_whitespace().collect();
        if words.is_empty() {
            paras.push(String::new());
            continue;
        }
        let mut lines: Vec<String> = Vec::new();
        let mut line = String::from(words[0]);
        for w in &words[1..] {
            if line.chars().count() + 1 + w.chars().count() <= WIDTH {
                line.push(' ');
                line.push_str(w);
            } else {
                lines.push(std::mem::take(&mut line));
                line = String::from(*w);
            }
        }
        lines.push(line);
        paras.push(lines.join("\n"));
    }
    paras.join("\n\n")
}

/// Render a source snippet as elm's `Render.Code` does: show every line in
/// `region`, with an `n| ` gutter; underline `highlight` with carets, but only
/// when it is single-line and sits on the last shown row (otherwise elm draws no
/// caret line).
fn render_snippet(source: &str, region: Region, highlight: Region) -> Vec<Chunk> {
    let lines: Vec<&str> = source.split('\n').collect();
    let start_row = region.start.row.max(1);
    let end_row = region.end.row.max(start_row);
    let gutter = end_row.to_string().len();
    // Carets can only point at one line, and only the last one shown. When the
    // highlight is anywhere else, elm marks each highlighted line by turning
    // its gutter into `n|>` instead of underlining.
    let underlined = highlight.start.row == highlight.end.row && highlight.end.row == end_row;
    let marker = Style::color(Color::RedVivid);
    let mut out = Chunks::new();
    for row in start_row..=end_row {
        let idx = (row - 1) as usize;
        let text = lines.get(idx).copied().unwrap_or("");
        out.plain(&format!("{:>gutter$}|", row, gutter = gutter));
        if !underlined && highlight.start.row <= row && row <= highlight.end.row {
            out.push(marker, ">");
        } else {
            out.plain(" ");
        }
        out.plain(text);
        out.plain("\n");
    }
    if underlined {
        let from = highlight.start.col.max(1) as usize;
        let to = (highlight.end.col as usize).max(from + 1);
        out.plain(&" ".repeat(gutter + 2 + (from - 1)));
        out.push(marker, &"^".repeat(to - from));
        out.plain("\n");
    } else {
        // elm's snippet ends with an empty final line where the carets would
        // have gone, which separates the code from the text that follows.
        out.plain("\n");
    }
    out.finish()
}

fn render_code_snippet(source: &str, region: Region) -> String {
    let mut out = String::new();
    let start_row = region.start.row.max(1);
    let end_row = region.end.row.max(start_row);
    let width = (end_row + 1).to_string().len();
    for (i, line) in source.lines().enumerate() {
        let row = i as u32 + 1;
        if row + 1 < start_row || row > end_row {
            continue;
        }
        out.push_str(&format!("{:>width$}| {}\n", row, line, width = width));
        if row == end_row {
            let from = region.start.col.max(1) as usize;
            let to = if region.end.row == region.start.row {
                (region.end.col as usize).max(from + 1)
            } else {
                line.chars().count() + 1
            };
            out.push_str(&" ".repeat(width + 1 + from));
            out.push_str(&"^".repeat((to - from).max(1)));
            out.push('\n');
        }
    }
    out
}

// ------------------------------------------------------------ --report=json
//
// `Reporting.Error.toJson` plus the envelope from `Reporting.Exit.Help`. Editor
// plugins read this, so the shape is fixed: a message is an array mixing bare
// strings (unstyled runs) with `{bold, underline, color, string}` objects.

impl Report {
    /// One entry of a module's `"problems"` array.
    pub fn to_json(&self, source: &str) -> String {
        let mut out = String::new();
        out.push_str("{\"title\":");
        json_str(&self.title, &mut out);
        out.push_str(",\"region\":");
        out.push_str(&region_to_json(self.region));
        out.push_str(",\"message\":");
        chunks_to_json(&self.body_chunks(source), &mut out);
        out.push('}');
        out
    }
}

/// `D.encode` — the message as an array of runs.
///
/// elm's encoder flushes a chunk at every style transition and once more at the
/// end, so an empty *unstyled* run appears wherever two styled runs meet, and
/// at either end if the message begins or ends styled. Those empty strings are
/// part of the format, so reproduce them.
fn chunks_to_json(chunks: &[Chunk], out: &mut String) {
    let styled = |c: &Chunk| c.style != Style::default();
    let mut items: Vec<Option<&Chunk>> = Vec::new();
    for (i, chunk) in chunks.iter().enumerate() {
        let previous_styled = i.checked_sub(1).map(|j| styled(&chunks[j]));
        if styled(chunk) && previous_styled.unwrap_or(true) {
            items.push(None);
        }
        items.push(Some(chunk));
    }
    match chunks.last() {
        Some(last) if styled(last) => items.push(None),
        None => items.push(None),
        _ => {}
    }

    out.push('[');
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let Some(chunk) = item else {
            out.push_str("\"\"");
            continue;
        };
        if chunk.style == Style::default() {
            json_str(&chunk.text, out);
            continue;
        }
        out.push_str("{\"bold\":");
        out.push_str(if chunk.style.bold { "true" } else { "false" });
        out.push_str(",\"underline\":");
        out.push_str(if chunk.style.underline { "true" } else { "false" });
        out.push_str(",\"color\":");
        match chunk.style.color {
            Some(color) => json_str(color.json_name(), out),
            None => out.push_str("null"),
        }
        out.push_str(",\"string\":");
        json_str(&chunk.text, out);
        out.push('}');
    }
    out.push(']');
}

fn region_to_json(region: Region) -> String {
    format!(
        "{{\"start\":{{\"line\":{},\"column\":{}}},\"end\":{{\"line\":{},\"column\":{}}}}}",
        region.start.row, region.start.col, region.end.row, region.end.col
    )
}

/// Append a JSON string literal for `s` to `out`.
pub fn json_str(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}
