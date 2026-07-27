//! `alm reactor` — port of elm's `Develop.hs`.
//!
//! A dev server for the current directory. Browsing to an `.elm` file compiles
//! it and serves the result as a page; browsing to a directory lists it;
//! anything else is served as a static file.
//!
//! elm's reactor renders its own pages with Elm apps compiled into the binary
//! (`Index.elm`, `Errors.elm`, `NotFound.elm`) and serves them from `/_elm/`.
//! alm renders the same pages on the server instead. The routing, the compile
//! behaviour and the page a compiled program is served in are elm's; the
//! chrome around them is alm's own, and there is no `/_elm/` asset route.

mod http;
mod index;

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use http::{Request, Response};

const DEFAULT_PORT: u16 = 8000;

pub fn run(args: &[String], color: bool) -> ExitCode {
    let mut port = DEFAULT_PORT;
    for arg in args {
        let Some(value) = arg.strip_prefix("--port=") else {
            eprintln!("Unknown flag `{arg}`.\n\nUsage: alm reactor [--port=8000]");
            return ExitCode::FAILURE;
        };
        match value.parse() {
            Ok(parsed) => port = parsed,
            Err(_) => {
                eprintln!("`{value}` is not a port number.");
                return ExitCode::FAILURE;
            }
        }
    }

    let root = match std::env::current_dir() {
        Ok(dir) => dir,
        Err(err) => {
            eprintln!("I could not work out what directory I am in: {err}");
            return ExitCode::FAILURE;
        }
    };

    // Loopback only. The reactor compiles and serves whatever is under the
    // current directory, which is not something to expose to a network.
    let listener = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("I could not listen on port {port}: {err}");
            return ExitCode::FAILURE;
        }
    };
    println!("Go to http://localhost:{port} to see your project dashboard.");
    http::serve(listener, move |request| handle(&root, request, color));
}

/// elm's route order: an existing file first (compiled if it is Elm, served by
/// its mime type if it has one, shown as source if it does not), then a
/// directory listing, then 404.
fn handle(root: &Path, request: &Request, color: bool) -> Response {
    let Some(path) = safe_path(root, &request.path) else {
        return Response::not_found(index::not_found(&request.path));
    };
    if path.is_file() {
        if path.extension().is_some_and(|e| e == "elm") {
            return serve_elm(root, &path, color);
        }
        return match mime_type(&path) {
            Some(mime) => match std::fs::metadata(&path) {
                Ok(meta) => Response::file(&path, mime, meta.len()),
                Err(_) => Response::not_found(index::not_found(&request.path)),
            },
            None => Response::html(index::source(&request.path, &path)),
        };
    }
    if path.is_dir() {
        return Response::html(index::directory(root, &path, &request.path));
    }
    Response::not_found(index::not_found(&request.path))
}

/// Resolve a request path under `root`, refusing anything that escapes it.
/// `..` is rejected outright and the resolved path is checked against the root
/// as well, so a symlink pointing outside cannot be followed either.
fn safe_path(root: &Path, request_path: &str) -> Option<PathBuf> {
    let mut path = root.to_path_buf();
    for part in request_path.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." || part.contains('\\') {
            return None;
        }
        path.push(part);
    }
    let resolved = std::fs::canonicalize(&path).ok()?;
    let root = std::fs::canonicalize(root).ok()?;
    resolved.starts_with(&root).then_some(resolved)
}

/// Compile the module and serve it in elm's page: the program mounted on a
/// `<pre id="elm">`, with initialization errors surfaced rather than swallowed.
fn serve_elm(root: &Path, path: &Path, color: bool) -> Response {
    let name = declared_module_name(path)
        .unwrap_or_else(|| path.file_stem().unwrap_or_default().to_string_lossy().into_owned());
    match alm_compiler::project::compile_project(path) {
        Ok((javascript, _warnings)) => Response::html(sandwich(&name, &javascript)),
        Err(errors) => {
            let reports: String =
                errors.iter().map(|e| e.render_from(Some(root), false)).collect();
            let _ = color;
            Response::html(index::errors(&name, &reports))
        }
    }
}

/// The name in `module X exposing (…)`, which is what the generated bundle
/// exposes as `Elm.X` — not the file name, which can differ from it.
fn declared_module_name(path: &Path) -> Option<String> {
    let source = std::fs::read_to_string(path).ok()?;
    // A file may open with comments, so this scans rather than reading the
    // first line. `effect module X where { … } exposing (…)` still names X
    // in the same position.
    source.lines().find_map(|line| {
        let line = line.trim_start();
        let rest = line
            .strip_prefix("module ")
            .or_else(|| line.strip_prefix("port module "))
            .or_else(|| line.strip_prefix("effect module "))?;
        Some(rest.split_whitespace().next()?.to_string())
    })
}

/// `Generate.Html.sandwich`, byte for byte.
fn sandwich(name: &str, javascript: &str) -> String {
    format!(
        "<!DOCTYPE HTML>\n\
         <html>\n\
         <head>\n\
         \x20 <meta charset=\"UTF-8\">\n\
         \x20 <title>{name}</title>\n\
         \x20 <style>body {{ padding: 0; margin: 0; }}</style>\n\
         </head>\n\
         \n\
         <body>\n\
         \n\
         <pre id=\"elm\"></pre>\n\
         \n\
         <script>\n\
         try {{\n\
         {javascript}\n\
         \n\
         \x20 var app = Elm.{name}.init({{ node: document.getElementById(\"elm\") }});\n\
         }}\n\
         catch (e)\n\
         {{\n\
         \x20 // display initialization errors (e.g. bad flags, infinite recursion)\n\
         \x20 var header = document.createElement(\"h1\");\n\
         \x20 header.style.fontFamily = \"monospace\";\n\
         \x20 header.innerText = \"Initialization Error\";\n\
         \x20 var pre = document.getElementById(\"elm\");\n\
         \x20 document.body.insertBefore(header, pre);\n\
         \x20 pre.innerText = e;\n\
         \x20 throw e;\n\
         }}\n\
         </script>\n\
         \n\
         </body>\n\
         </html>"
    )
}

/// elm's mime table. An extension that is not in it is shown as source rather
/// than downloaded, which is what makes browsing to a `.md` or a `.json` show
/// you the contents.
fn mime_type(path: &Path) -> Option<&'static str> {
    // Longest extension first, so `.tar.gz` beats `.gz`.
    let name = path.file_name()?.to_str()?;
    let lower = name.to_ascii_lowercase();
    MIME_TYPES
        .iter()
        .filter(|(ext, _)| lower.ends_with(ext))
        .max_by_key(|(ext, _)| ext.len())
        .map(|(_, mime)| *mime)
}

const MIME_TYPES: &[(&str, &str)] = &[
    (".asc", "text/plain"),
    (".asf", "video/x-ms-asf"),
    (".asx", "video/x-ms-asf"),
    (".avi", "video/x-msvideo"),
    (".bz2", "application/x-bzip"),
    (".css", "text/css"),
    (".dtd", "text/xml"),
    (".dvi", "application/x-dvi"),
    (".gif", "image/gif"),
    (".gz", "application/x-gzip"),
    (".htm", "text/html"),
    (".html", "text/html"),
    (".ico", "image/x-icon"),
    (".jpeg", "image/jpeg"),
    (".jpg", "image/jpeg"),
    (".js", "text/javascript"),
    (".json", "application/json"),
    (".m3u", "audio/x-mpegurl"),
    (".mov", "video/quicktime"),
    (".mp3", "audio/mpeg"),
    (".mp4", "video/mp4"),
    (".mpeg", "video/mpeg"),
    (".mpg", "video/mpeg"),
    (".ogg", "application/ogg"),
    (".otf", "font/otf"),
    (".pac", "application/x-ns-proxy-autoconfig"),
    (".pdf", "application/pdf"),
    (".png", "image/png"),
    (".qt", "video/quicktime"),
    (".sfnt", "font/sfnt"),
    (".sig", "application/pgp-signature"),
    (".spl", "application/futuresplash"),
    (".svg", "image/svg+xml"),
    (".swf", "application/x-shockwave-flash"),
    (".tar", "application/x-tar"),
    (".tar.bz2", "application/x-bzip-compressed-tar"),
    (".tar.gz", "application/x-tgz"),
    (".tbz", "application/x-bzip-compressed-tar"),
    (".text", "text/plain"),
    (".tgz", "application/x-tgz"),
    (".ttf", "font/ttf"),
    (".txt", "text/plain"),
    (".wav", "audio/x-wav"),
    (".wax", "audio/x-ms-wax"),
    (".webm", "video/webm"),
    (".webp", "image/webp"),
    (".wma", "audio/x-ms-wma"),
    (".wmv", "video/x-ms-wmv"),
    (".woff", "font/woff"),
    (".woff2", "font/woff2"),
    (".xbm", "image/x-xbitmap"),
    (".xml", "text/xml"),
    (".xpm", "image/x-xpixmap"),
    (".xwd", "image/x-xwindowdump"),
    (".zip", "application/zip"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_longest_matching_extension_wins() {
        assert_eq!(mime_type(Path::new("a/b.tar.gz")), Some("application/x-tgz"));
        assert_eq!(mime_type(Path::new("a/b.gz")), Some("application/x-gzip"));
        assert_eq!(mime_type(Path::new("a/b.PNG")), Some("image/png"));
        // No mime type: shown as source instead of downloaded.
        assert_eq!(mime_type(Path::new("README.md")), None);
        assert_eq!(mime_type(Path::new("elm.json")), Some("application/json"));
    }

    #[test]
    fn paths_cannot_escape_the_served_directory() {
        let dir = std::env::temp_dir().join(format!("alm-reactor-safe-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/Main.elm"), "module Main exposing (..)\n").unwrap();
        std::fs::write(dir.join("secret.txt"), "no").unwrap();

        let root = dir.join("src");
        assert!(safe_path(&root, "/Main.elm").is_some());
        assert!(safe_path(&root, "/./Main.elm").is_some());
        assert!(safe_path(&root, "/../secret.txt").is_none());
        assert!(safe_path(&root, "/nope.elm").is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The page a compiled program is served in is elm's, unchanged.
    #[test]
    fn the_sandwich_is_elms() {
        let page = sandwich("Main", "// code");
        assert!(page.starts_with("<!DOCTYPE HTML>\n<html>\n<head>\n  <meta charset=\"UTF-8\">\n  <title>Main</title>"));
        assert!(page.contains("<pre id=\"elm\"></pre>"));
        assert!(page
            .contains("  var app = Elm.Main.init({ node: document.getElementById(\"elm\") });"));
        assert!(page.contains("header.innerText = \"Initialization Error\";"));
        assert!(page.ends_with("</body>\n</html>"));
    }
}
