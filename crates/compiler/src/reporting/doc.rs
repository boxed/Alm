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

    /// Render at `width` columns.
    pub fn render(&self, width: usize) -> String {
        let mut out = String::new();
        let mut column = 0usize;
        // Work list of (indent, flat?, doc), innermost first.
        let mut work: Vec<(usize, bool, &Doc)> = vec![(0, false, self)];
        while let Some((indent, flat, doc)) = work.pop() {
            match doc {
                Doc::Empty => {}
                Doc::Text(s) => {
                    out.push_str(s);
                    column += s.chars().count();
                }
                Doc::Line | Doc::LineEmpty => {
                    if flat {
                        if matches!(doc, Doc::Line) {
                            out.push(' ');
                            column += 1;
                        }
                    } else {
                        out.push('\n');
                        out.push_str(&" ".repeat(indent));
                        column = indent;
                    }
                }
                Doc::Cat(a, b) => {
                    work.push((indent, flat, b));
                    work.push((indent, flat, a));
                }
                Doc::Nest(n, inner) => {
                    let next = (indent as isize + n).max(0) as usize;
                    work.push((next, flat, inner));
                }
                Doc::Align(inner) => work.push((column, flat, inner)),
                Doc::Group(inner) => {
                    // Flat if the flattened group and everything queued after
                    // it up to the next break still fits on this line.
                    let fits = flat || fits_flat(width, column, inner, &work);
                    work.push((indent, fits, inner));
                }
            }
        }
        out
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
fn fits_flat(width: usize, column: usize, doc: &Doc, rest: &[(usize, bool, &Doc)]) -> bool {
    let mut budget = width as isize - column as isize;
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
                Some((_, f, d)) => {
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
            Doc::Nest(_, inner) | Doc::Align(inner) => stack.push((flat, inner)),
            Doc::Group(inner) => stack.push((true, inner)),
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
