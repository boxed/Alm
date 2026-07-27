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
