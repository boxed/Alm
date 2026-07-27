//! A Wadler/Leijen pretty-printer, enough of one to lay types out exactly as
//! `Reporting.Doc` does.
//!
//! elm renders a type by building a document out of `align`/`sep`/`hang`/`cat`
//! and printing it at width 80, so where a long record or function type breaks
//! across lines is decided by this algorithm rather than by any rule we could
//! approximate. Groups lay out flat when the flattened group *plus whatever
//! follows on that line* fits, and vertically otherwise.

/// A document. `Line` renders as a space when flat and a newline (indented to
/// the current level) when broken.
#[derive(Debug, Clone)]
pub enum Doc {
    Empty,
    Text(String),
    /// A soft line break: a space when flat, a newline when broken.
    Line,
    /// A break that renders as nothing when flat (`D.cat`'s separator).
    LineEmpty,
    Cat(Box<Doc>, Box<Doc>),
    /// Increase the indent of nested line breaks by n.
    Nest(isize, Box<Doc>),
    /// Set the indent of nested line breaks to the current column.
    Align(Box<Doc>),
    /// Lay out flat if it fits on the rest of the line, else broken.
    Group(Box<Doc>),
    /// Style everything inside. Contributes no width.
    Styled(Style, Box<Doc>),
}

/// How a run of text is decorated. elm's reports use one attribute at a time,
/// but the JSON report shape carries all three, so model all three.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Style {
    pub bold: bool,
    pub underline: bool,
    pub color: Option<Color>,
}

/// The colors `Reporting.Doc` exposes. Terminals get two intensities of each;
/// elm names the dull one in lower case and the vivid one in upper case, which
/// is also how they appear in `--report=json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    Red,
    RedVivid,
    Magenta,
    MagentaVivid,
    Yellow,
    YellowVivid,
    Green,
    GreenVivid,
    Cyan,
    CyanVivid,
    Blue,
    BlueVivid,
    Black,
    BlackVivid,
    White,
    WhiteVivid,
}

impl Color {
    /// The SGR parameter: dull foregrounds are 30-37, vivid 90-97.
    fn ansi(self) -> u8 {
        use Color::*;
        match self {
            Black => 30,
            Red => 31,
            Green => 32,
            Yellow => 33,
            Blue => 34,
            Magenta => 35,
            Cyan => 36,
            White => 37,
            BlackVivid => 90,
            RedVivid => 91,
            GreenVivid => 92,
            YellowVivid => 93,
            BlueVivid => 94,
            MagentaVivid => 95,
            CyanVivid => 96,
            WhiteVivid => 97,
        }
    }

    /// The name `--report=json` uses: dull lower case, vivid upper case.
    pub fn json_name(self) -> &'static str {
        use Color::*;
        match self {
            Red => "red",
            RedVivid => "RED",
            Magenta => "magenta",
            MagentaVivid => "MAGENTA",
            Yellow => "yellow",
            YellowVivid => "YELLOW",
            Green => "green",
            GreenVivid => "GREEN",
            Cyan => "cyan",
            CyanVivid => "CYAN",
            Blue => "blue",
            BlueVivid => "BLUE",
            Black => "black",
            BlackVivid => "BLACK",
            White => "white",
            WhiteVivid => "WHITE",
        }
    }
}

impl Style {
    pub fn color(color: Color) -> Style {
        Style { color: Some(color), ..Style::default() }
    }

    pub fn underline() -> Style {
        Style { underline: true, ..Style::default() }
    }

    fn is_plain(self) -> bool {
        self == Style::default()
    }
}

/// A rendered run of text carrying one style. Adjacent runs of the same style
/// are merged, so a plain document is a single chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub style: Style,
    pub text: String,
}

impl Doc {
    pub fn text(s: impl Into<String>) -> Doc {
        Doc::Text(s.into())
    }

    pub fn cat2(a: Doc, b: Doc) -> Doc {
        Doc::Cat(Box::new(a), Box::new(b))
    }

    /// `a <+> b` — joined by a space.
    pub fn space(a: Doc, b: Doc) -> Doc {
        Doc::cat2(a, Doc::cat2(Doc::text(" "), b))
    }

    pub fn concat(docs: Vec<Doc>) -> Doc {
        docs.into_iter().fold(Doc::Empty, Doc::cat2)
    }

    /// `D.sep` — the parts on one line if they fit, else one per line.
    pub fn sep(docs: Vec<Doc>) -> Doc {
        Doc::Group(Box::new(interleave(docs, Doc::Line)))
    }

    /// `D.cat` — like `sep` but with no space between the parts when flat.
    pub fn cat(docs: Vec<Doc>) -> Doc {
        Doc::Group(Box::new(interleave(docs, Doc::LineEmpty)))
    }

    /// `D.vcat` — always one per line.
    pub fn vcat(docs: Vec<Doc>) -> Doc {
        interleave(docs, Doc::Line)
    }

    pub fn align(doc: Doc) -> Doc {
        Doc::Align(Box::new(doc))
    }

    pub fn hang(indent: isize, doc: Doc) -> Doc {
        Doc::Align(Box::new(Doc::Nest(indent, Box::new(doc))))
    }

    pub fn indent(n: usize, doc: Doc) -> Doc {
        Doc::cat2(Doc::text(" ".repeat(n)), Doc::Nest(n as isize, Box::new(Doc::align(doc))))
    }

    /// `D.fillSep` — words separated by breaks that are decided one at a time,
    /// so the text fills each line greedily rather than breaking all at once
    /// like `sep`.
    pub fn fill_sep(docs: Vec<Doc>) -> Doc {
        let mut out = Doc::Empty;
        for (i, doc) in docs.into_iter().enumerate() {
            if i > 0 {
                out = Doc::cat2(out, Doc::Group(Box::new(Doc::Line)));
            }
            out = Doc::cat2(out, doc);
        }
        out
    }

    /// Word-wrap plain text, the way `D.reflow` does. A blank line in the
    /// input separates paragraphs; each wraps on its own and the blank line
    /// survives.
    pub fn reflow(text: &str) -> Doc {
        let paragraphs: Vec<Doc> = text
            .split("\n\n")
            .map(|para| Doc::fill_sep(para.split_whitespace().map(Doc::text).collect()))
            .collect();
        if paragraphs.len() == 1 {
            return paragraphs.into_iter().next().unwrap();
        }
        let mut parts = Vec::new();
        for (i, para) in paragraphs.into_iter().enumerate() {
            if i > 0 {
                parts.push(Doc::Empty);
            }
            parts.push(para);
        }
        Doc::vcat(parts)
    }

    pub fn styled(style: Style, doc: Doc) -> Doc {
        Doc::Styled(style, Box::new(doc))
    }

    pub fn color(color: Color, doc: Doc) -> Doc {
        Doc::styled(Style::color(color), doc)
    }

    /// Render at `width` columns, discarding styles.
    pub fn render(&self, width: usize) -> String {
        self.chunks(width).into_iter().map(|c| c.text).collect()
    }

    /// Render with ANSI escapes around each styled run.
    pub fn render_ansi(&self, width: usize) -> String {
        let mut out = String::new();
        for chunk in self.chunks(width) {
            if chunk.style.is_plain() {
                out.push_str(&chunk.text);
                continue;
            }
            // A styled run that spans a line break has to re-open the style on
            // each line, or the escape leaks across the newline and terminals
            // that clear to end-of-line lose it. elm's renderer emits the
            // escapes inside the run, which comes to the same thing.
            let mut codes = Vec::new();
            if chunk.style.bold {
                codes.push("1".to_string());
            }
            if chunk.style.underline {
                codes.push("4".to_string());
            }
            if let Some(color) = chunk.style.color {
                codes.push(color.ansi().to_string());
            }
            let open = format!("\u{1b}[{}m", codes.join(";"));
            out.push_str(&open);
            out.push_str(&chunk.text);
            out.push_str("\u{1b}[0m");
        }
        out
    }

    /// Render into styled runs, merging adjacent runs that share a style.
    pub fn chunks(&self, width: usize) -> Vec<Chunk> {
        let mut chunks: Vec<Chunk> = Vec::new();
        let mut column = 0usize;
        let mut push = |style: Style, text: &str| {
            if text.is_empty() {
                return;
            }
            match chunks.last_mut() {
                Some(last) if last.style == style => last.text.push_str(text),
                _ => chunks.push(Chunk { style, text: text.to_string() }),
            }
        };
        // Work list of (indent, flat?, style, doc), innermost first.
        let mut work: Vec<(usize, bool, Style, &Doc)> = vec![(0, false, Style::default(), self)];
        while let Some((indent, flat, style, doc)) = work.pop() {
            match doc {
                Doc::Empty => {}
                Doc::Text(s) => {
                    push(style, s);
                    column += s.chars().count();
                }
                Doc::Line | Doc::LineEmpty => {
                    if flat {
                        if matches!(doc, Doc::Line) {
                            push(style, " ");
                            column += 1;
                        }
                    } else {
                        // The newline and its indent are never styled: a style
                        // that spanned them would color the left margin.
                        push(Style::default(), "\n");
                        push(Style::default(), &" ".repeat(indent));
                        column = indent;
                    }
                }
                Doc::Cat(a, b) => {
                    work.push((indent, flat, style, b));
                    work.push((indent, flat, style, a));
                }
                Doc::Nest(n, inner) => {
                    let next = (indent as isize + n).max(0) as usize;
                    work.push((next, flat, style, inner));
                }
                Doc::Align(inner) => work.push((column, flat, style, inner)),
                Doc::Styled(inner_style, inner) => work.push((indent, flat, *inner_style, inner)),
                Doc::Group(inner) => {
                    // Flat if the flattened group and everything queued after
                    // it up to the next break still fits on this line.
                    let fits = flat || fits_flat(width, column, inner, &work);
                    work.push((indent, fits, style, inner));
                }
            }
        }
        chunks
    }
}

/// Plain prose becomes a filled paragraph. Lets a call site pass either a
/// `String` or an already-styled `Doc` wherever prose is wanted.
impl From<String> for Doc {
    fn from(text: String) -> Doc {
        Doc::reflow(&text)
    }
}

impl From<&str> for Doc {
    fn from(text: &str) -> Doc {
        Doc::reflow(text)
    }
}

fn interleave(docs: Vec<Doc>, separator: Doc) -> Doc {
    let mut out = Doc::Empty;
    for (i, doc) in docs.into_iter().enumerate() {
        if i > 0 {
            out = Doc::cat2(out, separator.clone());
        }
        out = Doc::cat2(out, doc);
    }
    out
}

/// Would `doc` flattened, followed by the pending work, reach the next line
/// break within the width budget?
fn fits_flat(width: usize, column: usize, doc: &Doc, rest: &[(usize, bool, Style, &Doc)]) -> bool {
    // Clamp before the signed conversion: callers pass `usize::MAX` to mean
    // "never wrap", and `usize::MAX as isize` is -1, which would make every
    // group break instead.
    let mut budget = width.min(isize::MAX as usize) as isize - column as isize;
    if budget < 0 {
        return false;
    }
    // Scan the flattened group, then the already-queued work (which `render`
    // pops back to front, so walk it in that order too).
    let mut stack: Vec<(bool, &Doc)> = vec![(true, doc)];
    let mut queued = rest.iter().rev();
    loop {
        let Some((flat, current)) = stack.pop() else {
            match queued.next() {
                Some((_, f, _, d)) => {
                    stack.push((*f, d));
                    continue;
                }
                None => return true,
            }
        };
        match current {
            Doc::Empty => {}
            Doc::Text(s) => {
                budget -= s.chars().count() as isize;
                if budget < 0 {
                    return false;
                }
            }
            Doc::Line => {
                if flat {
                    budget -= 1;
                    if budget < 0 {
                        return false;
                    }
                } else {
                    // Reached a real break: everything so far fits.
                    return true;
                }
            }
            Doc::LineEmpty => {
                if !flat {
                    return true;
                }
            }
            Doc::Cat(a, b) => {
                stack.push((flat, b));
                stack.push((flat, a));
            }
            Doc::Nest(_, inner) | Doc::Align(inner) | Doc::Styled(_, inner) => {
                stack.push((flat, inner))
            }
            // Inherit rather than force flat. A group that has already been
            // decided passes its own mode down; one still queued is undecided,
            // and the earliest it could break is at its first `Line` — which is
            // exactly where measuring should stop.
            Doc::Group(inner) => stack.push((flat, inner)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Doc;

    #[test]
    fn a_group_that_fits_stays_flat() {
        let doc = Doc::sep(vec![Doc::text("a"), Doc::text("->"), Doc::text("b")]);
        assert_eq!(doc.render(80), "a -> b");
    }

    #[test]
    fn a_group_that_does_not_fit_breaks_at_every_separator() {
        let long = Doc::text("x".repeat(30));
        let doc = Doc::sep(vec![long.clone(), long.clone(), long]);
        assert_eq!(doc.render(80), format!("{0}\n{0}\n{0}", "x".repeat(30)));
    }

    #[test]
    fn align_indents_continuation_lines_to_the_current_column() {
        let long = Doc::text("y".repeat(40));
        let doc = Doc::cat2(Doc::text("ab"), Doc::align(Doc::sep(vec![long.clone(), long])));
        assert_eq!(doc.render(50), format!("ab{0}\n  {0}", "y".repeat(40)));
    }

    /// A group's fit is judged including what follows it on the line, so a
    /// trailing `)` can be what pushes a type onto several lines.
    #[test]
    fn a_group_accounts_for_trailing_text() {
        let inner = Doc::sep(vec![Doc::text("a".repeat(38)), Doc::text("b".repeat(38))]);
        let doc = Doc::cat2(inner, Doc::text(" trailing"));
        assert_eq!(doc.render(80), format!("{}\n{} trailing", "a".repeat(38), "b".repeat(38)));
    }
}

#[cfg(test)]
mod fill_tests {
    use super::{Color, Doc, Style};

    /// The greedy word wrap `reflow` used to do by hand, kept here as the
    /// reference `fill_sep` has to reproduce.
    fn greedy(text: &str, width: usize) -> String {
        let words: Vec<&str> = text.split_whitespace().collect();
        if words.is_empty() {
            return String::new();
        }
        let mut lines: Vec<String> = Vec::new();
        let mut line = String::from(words[0]);
        for w in &words[1..] {
            if line.chars().count() + 1 + w.chars().count() <= width {
                line.push(' ');
                line.push_str(w);
            } else {
                lines.push(std::mem::take(&mut line));
                line = String::from(*w);
            }
        }
        lines.push(line);
        lines.join("\n")
    }

    #[test]
    fn fill_sep_matches_greedy_word_wrap() {
        let samples = [
            "Hint: Everything in a list must be the same type of value. This way, we never \
             run into unexpected values partway through a List.map, List.foldl, etc. Read \
             <https://elm-lang.org/0.19.1/custom-types> to learn how to “mix” types.",
            "short",
            "",
            "exactly eighty columns of text here to check the boundary case works right ok",
            "supercalifragilisticexpialidocious-and-then-some-more-to-overflow-a-single-line ok",
        ];
        for text in samples {
            assert_eq!(
                Doc::reflow(text).render(80),
                greedy(text, 80),
                "fill_sep diverged from greedy wrap on: {text:?}"
            );
        }
    }

    /// A styled run renders plainly under `render` and with escapes under
    /// `render_ansi`, and the escapes add no width — so wrapping is unchanged.
    #[test]
    fn styles_do_not_affect_layout() {
        let words = vec![
            Doc::text("But"),
            Doc::text("I"),
            Doc::text("need"),
            Doc::text("a"),
            Doc::color(Color::Yellow, Doc::text("Bool")),
            Doc::text("value."),
        ];
        let doc = Doc::fill_sep(words);
        assert_eq!(doc.render(80), "But I need a Bool value.");
        assert_eq!(
            doc.render_ansi(80),
            "But I need a \u{1b}[33mBool\u{1b}[0m value."
        );
        // Same text, same break points, whatever the styling.
        assert_eq!(doc.render(12), "But I need a\nBool value.");
    }

    /// Each word carries its own style and the separator between them does
    /// not — matching elm, which emits `<green>Int</green> and <green>Float`
    /// rather than coloring the " and " too.
    #[test]
    fn separators_between_styled_words_stay_plain() {
        let doc = Doc::fill_sep(vec![
            Doc::color(Color::Yellow, Doc::text("aaaa")),
            Doc::color(Color::Yellow, Doc::text("bbbb")),
        ]);
        let flat: Vec<_> = doc.chunks(80).into_iter().map(|c| (c.style, c.text)).collect();
        assert_eq!(
            flat,
            vec![
                (Style::color(Color::Yellow), "aaaa".to_string()),
                (Style::default(), " ".to_string()),
                (Style::color(Color::Yellow), "bbbb".to_string()),
            ]
        );
        // Broken, the newline is likewise unstyled.
        assert_eq!(doc.chunks(5)[1].text, "\n");
        assert_eq!(doc.chunks(5)[1].style, Style::default());
    }

    /// Adjacent text in the same style becomes one run, so `Hint` + `:` do not
    /// split a chunk when they share styling.
    /// `usize::MAX` means "never wrap" — used to render a report's searchable
    /// one-line summary. A naive `as isize` makes that -1 and breaks every
    /// line instead.
    #[test]
    fn a_huge_width_never_wraps() {
        let doc = Doc::reflow("one two three four five six seven eight nine ten");
        assert_eq!(doc.render(usize::MAX), "one two three four five six seven eight nine ten");
    }

    #[test]
    fn adjacent_runs_of_one_style_merge() {
        let doc = Doc::cat2(
            Doc::color(Color::Green, Doc::text("abc")),
            Doc::color(Color::Green, Doc::text("def")),
        );
        let chunks = doc.chunks(80);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "abcdef");
    }
}
