pub mod annotation;

pub use annotation::{Located, Position, Region};

/// A rendered compiler error, in the spirit of Elm's friendly error messages.
/// The full `Reporting.Error.*` hierarchy renders rich docs; we start with a
/// title, a message, and a source excerpt with a caret marker.
#[derive(Debug, Clone)]
pub struct Report {
    pub title: String,
    pub region: Region,
    pub message: String,
}

impl Report {
    pub fn render(&self, path: &str, source: &str) -> String {
        let mut out = String::new();
        let dashes = 60usize.saturating_sub(self.title.len() + path.len());
        out.push_str(&format!(
            "-- {} {} {}\n\n",
            self.title,
            "-".repeat(dashes.max(2)),
            path
        ));
        out.push_str(&render_code_snippet(source, self.region));
        out.push('\n');
        out.push_str(&self.message);
        out.push('\n');
        out
    }
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
            // caret line under the offending columns
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
