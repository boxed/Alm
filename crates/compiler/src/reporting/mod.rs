pub mod annotation;
pub mod syntax;

pub use annotation::{Located, Position, Region};

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
    /// Reflowed paragraph shown before the source snippet.
    pub before: String,
    /// Reflowed paragraph shown immediately after the snippet (no blank line).
    pub after: String,
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
    /// A word-wrapped paragraph (reflowed to 80 columns).
    Para(String),
    /// A verbatim block (indented code examples), emitted as-is.
    Block(String),
}

const WIDTH: usize = 80;

impl Report {
    pub fn render(&self, path: &str, source: &str) -> String {
        match &self.elm {
            Some(body) => render_elm(&self.title, body, path, source),
            None => self.render_legacy(path, source),
        }
    }

    fn render_legacy(&self, path: &str, source: &str) -> String {
        let mut out = String::new();
        out.push_str(&header(&self.title, path));
        out.push_str("\n\n");
        out.push_str(&render_code_snippet(source, self.region));
        out.push('\n');
        out.push_str(&self.message);
        out.push('\n');
        out
    }
}

/// `-- TITLE --------- path`, padded to 80 columns (`Reporting.Report.toDoc`).
fn header(title: &str, path: &str) -> String {
    // "-- " + title + " " + dashes + " " + path  == WIDTH
    let fixed = 3 + title.len() + 1 + 1 + path.len();
    let dashes = WIDTH.saturating_sub(fixed).max(2);
    format!("-- {} {} {}", title, "-".repeat(dashes), path)
}

fn render_elm(title: &str, body: &ElmBody, path: &str, source: &str) -> String {
    let mut out = String::new();
    out.push_str(&header(title, path));
    out.push_str("\n\n");
    out.push_str(&reflow(&body.before));
    out.push_str("\n\n");
    out.push_str(&render_snippet(source, body.region, body.highlight));
    out.push_str(&reflow(&body.after));
    for note in &body.notes {
        out.push_str("\n\n");
        match note {
            Section::Para(p) => out.push_str(&reflow(p)),
            Section::Block(b) => out.push_str(b),
        }
    }
    out.push_str("\n\n");
    out
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
fn render_snippet(source: &str, region: Region, highlight: Region) -> String {
    let lines: Vec<&str> = source.split('\n').collect();
    let start_row = region.start.row.max(1);
    let end_row = region.end.row.max(start_row);
    let gutter = end_row.to_string().len();
    let mut out = String::new();
    for row in start_row..=end_row {
        let idx = (row - 1) as usize;
        let text = lines.get(idx).copied().unwrap_or("");
        out.push_str(&format!("{:>gutter$}| {}\n", row, text, gutter = gutter));
    }
    if highlight.start.row == highlight.end.row && highlight.end.row == end_row {
        let from = highlight.start.col.max(1) as usize;
        let to = (highlight.end.col as usize).max(from + 1);
        out.push_str(&" ".repeat(gutter + 2 + (from - 1)));
        out.push_str(&"^".repeat(to - from));
        out.push('\n');
    }
    out
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
