//! The reactor's own pages: the directory index, the source view, the error
//! page and the 404.
//!
//! elm builds these from Elm apps compiled into the binary and served out of
//! `/_elm/`. alm renders them here instead, so there is no asset route and no
//! second Elm program to keep working — but the information on each page, and
//! which page you get where, is the same.

use std::path::Path;

use super::http::{encode_path, escape, read};

/// One stylesheet for every page, inlined so the server has no assets to
/// serve and the pages work with no network at all.
const STYLE: &str = "\
  :root { color-scheme: light dark; --fg: #293c4b; --bg: #ffffff;
          --dim: #7f8fa4; --rule: #e0e6ed; --link: #1293d8; }
  @media (prefers-color-scheme: dark) {
    :root { --fg: #eeeeee; --bg: #1e1e1e; --dim: #9aa7b6; --rule: #333333; }
  }
  body { margin: 0; color: var(--fg); background: var(--bg);
         font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto,
                      Helvetica, Arial, sans-serif; }
  main { max-width: 60rem; margin: 0 auto; padding: 2rem 1.5rem 4rem; }
  h1 { font-size: 1.5rem; font-weight: 500; margin: 0 0 1.5rem; }
  a { color: var(--link); text-decoration: none; }
  a:hover { text-decoration: underline; }
  nav { color: var(--dim); }
  ul { list-style: none; margin: 0; padding: 0;
       border-top: 1px solid var(--rule); }
  li { border-bottom: 1px solid var(--rule); }
  li a { display: block; padding: 0.6rem 0.25rem; }
  .dim { color: var(--dim); }
  section { margin-top: 2.5rem; }
  h2 { font-size: 1rem; font-weight: 600; margin: 0 0 0.75rem; }
  pre { overflow-x: auto; padding: 1rem; background: rgba(127,143,164,0.12);
        border-radius: 4px; line-height: 1.4; }
  code, pre { font-family: 'Source Code Pro', SFMono-Regular, Consolas,
              monospace; font-size: 0.875rem; }
  table { border-collapse: collapse; }
  td { padding: 0.15rem 1.5rem 0.15rem 0; }
";

fn page(title: &str, body: &str) -> String {
    format!(
        "<!DOCTYPE HTML>\n<html>\n<head>\n  <meta charset=\"UTF-8\">\n  \
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n  \
         <title>{}</title>\n  <style>\n{STYLE}  </style>\n</head>\n<body>\n\
         <main>\n{body}</main>\n</body>\n</html>\n",
        escape(title)
    )
}

/// The directory listing: where you are, what is in it, the README, and the
/// project's dependencies.
pub fn directory(root: &Path, dir: &Path, request_path: &str) -> String {
    let shown = if request_path == "/" { "/" } else { request_path.trim_end_matches('/') };
    let mut body = format!("<h1>{}</h1>\n{}\n", escape(shown), breadcrumbs(shown));

    let (mut dirs, mut files) = (Vec::new(), Vec::new());
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            // elm's index hides nothing, but a listing full of dotfiles is
            // noise in a project directory.
            if name.starts_with('.') {
                continue;
            }
            if entry.path().is_dir() {
                dirs.push(name);
            } else {
                files.push(name);
            }
        }
    }
    dirs.sort();
    files.sort();

    let base = if shown == "/" { String::new() } else { shown.to_string() };
    body.push_str("<ul>\n");
    for name in &dirs {
        body.push_str(&format!(
            "  <li><a href=\"{}/{}/\">{}/</a></li>\n",
            encode_path(&base),
            encode_path(name),
            escape(name)
        ));
    }
    for name in &files {
        // An .elm file is a link that runs it; everything else just opens.
        let note = if name.ends_with(".elm") { " <span class=\"dim\">— runnable</span>" } else { "" };
        body.push_str(&format!(
            "  <li><a href=\"{}/{}\">{}</a>{note}</li>\n",
            encode_path(&base),
            encode_path(name),
            escape(name)
        ));
    }
    if dirs.is_empty() && files.is_empty() {
        body.push_str("  <li class=\"dim\" style=\"padding: 0.6rem 0.25rem\">\
                       This directory is empty.</li>\n");
    }
    body.push_str("</ul>\n");

    if let Some(readme) = read_text(&dir.join("README.md")) {
        body.push_str(&format!(
            "<section>\n<h2>README.md</h2>\n<pre>{}</pre>\n</section>\n",
            escape(&readme)
        ));
    }
    if let Some(outline) = read_text(&root.join("elm.json")) {
        body.push_str(&project_summary(&outline));
    }
    page(shown, &body)
}

/// The `elm.json` boiled down to what the dashboard is for: what kind of
/// project this is and what it depends on.
fn project_summary(outline: &str) -> String {
    use alm_compiler::packages;
    let kind = packages::json_string(outline, "type").unwrap_or("project");
    let mut rows = Vec::new();
    if let Some(name) = packages::json_string(outline, "name") {
        rows.push(("name", name.to_string()));
    }
    if let Some(version) = packages::json_string(outline, "version") {
        rows.push(("version", version.to_string()));
    }
    if let Some(license) = packages::json_string(outline, "license") {
        rows.push(("license", license.to_string()));
    }

    let mut dependencies = Vec::new();
    if let Some(deps) = packages::object_block(outline, "dependencies") {
        // An application splits its dependencies into direct and indirect; a
        // package lists constraints in one block. Only the direct ones are
        // worth showing either way.
        let listed = packages::object_block(deps, "direct").unwrap_or(deps);
        for (name, value) in packages::pairs(listed) {
            if name.contains('/') {
                dependencies.push((name.to_string(), value.to_string()));
            }
        }
    }

    let mut out = format!("<section>\n<h2>{} — elm.json</h2>\n<table>\n", escape(kind));
    for (key, value) in rows {
        out.push_str(&format!(
            "  <tr><td class=\"dim\">{key}</td><td><code>{}</code></td></tr>\n",
            escape(&value)
        ));
    }
    out.push_str("</table>\n");
    if !dependencies.is_empty() {
        out.push_str("<h2 style=\"margin-top:1.5rem\">Dependencies</h2>\n<table>\n");
        for (name, value) in dependencies {
            out.push_str(&format!(
                "  <tr><td><code>{}</code></td><td class=\"dim\"><code>{}</code></td></tr>\n",
                escape(&name),
                escape(&value)
            ));
        }
        out.push_str("</table>\n");
    }
    out.push_str("</section>\n");
    out
}

/// Every ancestor of the current directory, as links.
fn breadcrumbs(shown: &str) -> String {
    let mut out = String::from("<nav><a href=\"/\">~</a>");
    let mut accumulated = String::new();
    for part in shown.split('/').filter(|p| !p.is_empty()) {
        accumulated.push('/');
        accumulated.push_str(part);
        out.push_str(&format!(
            " / <a href=\"{}/\">{}</a>",
            encode_path(&accumulated),
            escape(part)
        ));
    }
    out.push_str("</nav>");
    out
}

/// A file with no mime type is shown rather than downloaded, which is what
/// makes browsing to a README or an elm.json useful.
pub fn source(request_path: &str, path: &Path) -> String {
    let body = match read_text(path) {
        Some(text) => format!(
            "<h1>{}</h1>\n{}\n<pre>{}</pre>\n",
            escape(request_path),
            breadcrumbs(request_path),
            escape(&text)
        ),
        None => format!(
            "<h1>{}</h1>\n<p class=\"dim\">This file is not text, and I do not know a \
             content type for it.</p>\n",
            escape(request_path)
        ),
    };
    page(request_path, &body)
}

/// A failed compile. elm renders the report JSON with an Elm app; alm shows
/// the reports it would have printed to the terminal.
pub fn errors(name: &str, reports: &str) -> String {
    page(
        &format!("{name} — errors"),
        &format!("<h1>Compilation failed</h1>\n<pre>{}</pre>\n", escape(reports)),
    )
}

pub fn not_found(request_path: &str) -> String {
    page(
        "Not Found",
        &format!(
            "<h1>Page not found</h1>\n<p class=\"dim\">There is nothing at \
             <code>{}</code>.</p>\n<p><a href=\"/\">Back to the dashboard</a></p>\n",
            escape(request_path)
        ),
    )
}

/// A file's contents if it is valid UTF-8, so a binary never lands in a page.
fn read_text(path: &Path) -> Option<String> {
    String::from_utf8(read(path)?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn breadcrumbs_link_every_ancestor() {
        assert_eq!(breadcrumbs("/"), "<nav><a href=\"/\">~</a></nav>");
        assert_eq!(
            breadcrumbs("/src/Page"),
            "<nav><a href=\"/\">~</a> / <a href=\"/src/\">src</a> / \
             <a href=\"/src/Page/\">Page</a></nav>"
        );
    }

    #[test]
    fn the_listing_marks_elm_files_and_links_into_subdirectories() {
        let dir = std::env::temp_dir().join(format!("alm-reactor-index-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/Main.elm"), "module Main exposing (..)\n").unwrap();
        std::fs::write(dir.join("README.md"), "# Hi <there>\n").unwrap();
        std::fs::write(dir.join(".hidden"), "x").unwrap();
        std::fs::write(
            dir.join("elm.json"),
            r#"{ "type": "application", "dependencies": {
                   "direct": { "elm/core": "1.0.5" },
                   "indirect": { "elm/json": "1.1.4" } } }"#,
        )
        .unwrap();

        let html = directory(&dir, &dir, "/");
        assert!(html.contains("<a href=\"/src/\">src/</a>"), "{html}");
        assert!(html.contains("<a href=\"/README.md\">README.md</a>"), "{html}");
        assert!(!html.contains(".hidden"), "{html}");
        // The README is shown, escaped.
        assert!(html.contains("# Hi &lt;there&gt;"), "{html}");
        // Direct dependencies only.
        assert!(html.contains("elm/core"), "{html}");
        assert!(!html.contains("elm/json"), "{html}");

        let html = directory(&dir, &dir.join("src"), "/src");
        assert!(html.contains("<a href=\"/src/Main.elm\">Main.elm</a>"), "{html}");
        assert!(html.contains("runnable"), "{html}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn spaces_in_names_survive_into_the_links() {
        let dir = std::env::temp_dir().join(format!("alm-reactor-space-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("My File.elm"), "module Main exposing (..)\n").unwrap();
        let html = directory(&dir, &dir, "/");
        assert!(html.contains("href=\"/My%20File.elm\""), "{html}");
        assert!(html.contains(">My File.elm<"), "{html}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
