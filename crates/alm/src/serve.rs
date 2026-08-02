//! `alm make --live` — build one program, serve it, and keep it up to date.
//!
//! Where `alm reactor` browses a whole directory and compiles on request, this
//! serves a single entry module at `/` and rebuilds it the moment its sources
//! change. Rebuilding ahead of the request is what makes a failed build
//! reportable without a reload: the page is told, and shows the errors over
//! the running program until the next build succeeds.
//!
//! With `--output` it also writes the program out on every build. That is for
//! the case where the page is not alm's to serve — the program is one piece of
//! a larger app, embedded in its template. The written bundle carries the
//! live-reload client with it, so that page hot-swaps without knowing anything
//! about alm; see [`Writer::bundle`].

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use crate::server::http::{self, Request, Response};
use crate::server::live::{self, Client, Live, Mode};
use crate::server::pages;

/// The latest build, and how it went.
struct Build {
    /// The compiled program, kept from the last build that worked so a broken
    /// edit does not take the page down with it.
    javascript: Option<String>,
    /// The Source Map v3 for it, under `--source-maps`.
    source_map: Option<String>,
    /// A fingerprint of the program's `Model` type, which decides whether a
    /// running model can be carried across a swap.
    model_fingerprint: Option<String>,
    /// Reports from the most recent build, if it failed.
    errors: Option<String>,
}

pub struct Options {
    pub entry: PathBuf,
    pub port: u16,
    pub updating: bool,
    pub color: bool,
    /// Where to write the program, for embedding it in a larger app's page.
    /// `None` serves it and nothing more.
    pub output: Option<PathBuf>,
    pub source_maps: bool,
    pub optimize: bool,
}

pub fn run(options: Options) -> ExitCode {
    let root = alm_compiler::project::project_root(&options.entry);
    let module = crate::reactor::declared_module_name(&options.entry).unwrap_or_else(|| {
        options.entry.file_stem().unwrap_or_default().to_string_lossy().into_owned()
    });

    // The port is bound before the first build, so a `--output` build can never
    // write a bundle pointing at a port that turns out to be taken.
    let listener = match TcpListener::bind(("127.0.0.1", options.port)) {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("I could not listen on port {}: {err}", options.port);
            return ExitCode::FAILURE;
        }
    };

    let mode = if options.updating { Mode::HotSwap } else { Mode::Reload };
    let writer = options.output.clone().map(|path| Writer {
        path,
        module: module.clone(),
        mode,
        origin: format!("http://127.0.0.1:{}", options.port),
        updating: options.updating,
        source_maps: options.source_maps,
    });
    let rebuild = {
        let entry = options.entry.clone();
        let root = root.clone();
        let color = options.color;
        let optimize = options.optimize;
        let source_maps = options.source_maps;
        move || compile(&entry, &root, color, optimize, source_maps)
    };

    let build = Arc::new(Mutex::new(rebuild()));
    {
        let first = build.lock().unwrap();
        if let Some(errors) = &first.errors {
            // Report the first build the way `alm make` would; the server still
            // comes up, so fixing the error is a save away.
            eprint!("{errors}");
        }
        if let Some(writer) = &writer {
            writer.write(&first);
        }
    }

    println!("Go to http://localhost:{} to see {module}.", options.port);
    if let Some(writer) = &writer {
        print!("Writing {} on every build.", writer.path.display());
        if writer.updating {
            print!(
                " It is a development bundle: it carries the\nlive-reload client and talks to {}, \
                 so it is not one to ship.",
                writer.origin
            );
        }
        println!();
    }

    let live = options.updating.then(|| {
        let live = Live::new();
        live::heartbeat(live.clone());
        live
    });

    // Rebuild on every change if there is anyone to rebuild for: a page to tell
    // about it, an output file to keep current, or both. `--no-hot-reload` with
    // `--output` is the second on its own — the file stays up to date while no
    // page is touched, which is what you want when the app around it has a
    // reloader of its own.
    if options.updating || writer.is_some() {
        let watched = live.clone();
        let build = build.clone();
        let watching = root.clone();
        live::watch(&root, move |moved| {
            // Announced before the build rather than after it, so a save is
            // acknowledged the moment it is noticed: a watching server is
            // otherwise silent, and a change that was picked up looks exactly
            // like one that was not until the build is over.
            print!("Recompiling {}... ", naming(moved, &watching));
            let _ = std::io::Write::flush(&mut std::io::stdout());

            let started = std::time::Instant::now();
            let fresh = rebuild();
            let failed = fresh.errors.clone();
            {
                let mut held = build.lock().unwrap_or_else(|e| e.into_inner());
                match failed {
                    // A broken edit must not take the served program away:
                    // only the reports change, and the page goes on running
                    // the last build that worked.
                    Some(_) => held.errors = fresh.errors,
                    None => *held = fresh,
                }
                // Written under the lock, so what lands on disk is always one of
                // the builds the server is serving, never a torn mixture.
                if let Some(writer) = &writer {
                    writer.write(&held);
                }
            }
            // Finishes the line the change opened. Both endings go to stdout,
            // so the line stays whole; the reports themselves are the page's
            // to show.
            let took = milliseconds(started.elapsed());
            match &failed {
                Some(_) => println!("it does not compile ({took})."),
                None => println!("done ({took})."),
            }

            let Some(watched) = &watched else { return };
            match failed {
                Some(errors) => watched.broadcast("failed", &json_string(&errors)),
                None => watched.broadcast("changed", "null"),
            }
        });
    }

    let state = State { root, module, build, live, mode, updating: options.updating };
    http::serve(listener, move |request| state.handle(request));
}

/// Writes the program out on every build, for a page alm does not serve.
struct Writer {
    path: PathBuf,
    module: String,
    mode: Mode,
    /// Absolute origin of this server, baked into the written client so it can
    /// be reached from a page served by something else.
    origin: String,
    updating: bool,
    source_maps: bool,
}

impl Writer {
    fn write(&self, build: &Build) {
        // A failed build leaves the last good file in place, for the same reason
        // the server goes on serving the last good program: a broken edit should
        // not take the surrounding app down with it.
        let Some(javascript) = &build.javascript else { return };
        let bundle = self.bundle(javascript, build.model_fingerprint.as_deref());
        if let Err(err) = std::fs::write(&self.path, bundle) {
            eprintln!("I could not write {}: {err}", self.path.display());
            return;
        }
        if let Some(map) = &build.source_map {
            let map_path = self.map_path();
            if let Err(err) = std::fs::write(&map_path, map) {
                eprintln!("I could not write {}: {err}", map_path.display());
            }
        }
    }

    fn map_path(&self) -> PathBuf {
        self.path.with_extension("js.map")
    }

    /// The program as written: the bundle, then the live-reload client.
    ///
    /// The client goes *in the bundle* rather than in a page, because the page
    /// belongs to another app — a Django template, a Vite entry — which should
    /// not have to know this file came from alm. Loading it is enough.
    ///
    /// **Everything is appended; nothing is ever prepended.** A source map
    /// addresses generated *lines*, so a single line added above the program puts
    /// every mapping one line out, and a debugger then refuses the breakpoints it
    /// cannot place — silently. Appending is free: the program occupies the same
    /// lines it was compiled at. The registry the client installs is read when
    /// `init` runs, not when the bundle evaluates, which is what lets it come
    /// afterwards.
    ///
    /// What `/_alm/bundle.js` serves for a swap to fetch is deliberately *not*
    /// this, but the bare program: a swap that pulled in another copy of the
    /// client would open a second event stream on every save.
    fn bundle(&self, javascript: &str, model_fingerprint: Option<&str>) -> String {
        let mut out = String::with_capacity(javascript.len() + 4096);
        out.push_str(javascript);
        if self.updating {
            out.push_str(
                "\n// Live reload, from `alm make --live`; not a bundle to ship.\n\
                 // `--no-hot-reload` leaves all of this out.\n",
            );
            out.push_str(live::registry_js());
            out.push('\n');
            out.push_str(&live::client_js_for(&Client {
                mode: self.mode,
                module: &self.module,
                bundle: BUNDLE,
                origin: &self.origin,
                model_fingerprint,
                // Reports go to the terminal and to the pages already open. A
                // file written while the build is broken is the previous good
                // one, so it has nothing to show on arrival.
                errors: None,
            }));
            out.push('\n');
        }
        // Last, as the convention expects — and it has to be after the client
        // anyway, since a comment only applies to what precedes it.
        if self.source_maps {
            let name = self
                .map_path()
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            out.push_str(&format!("//# sourceMappingURL={name}\n"));
        }
        out
    }
}

struct State {
    root: PathBuf,
    module: String,
    build: Arc<Mutex<Build>>,
    live: Option<Arc<Live>>,
    mode: Mode,
    updating: bool,
}

impl State {
    fn handle(&self, request: &Request) -> Response {
        if request.path == live::ENDPOINT {
            return match &self.live {
                Some(live) => allow_any_origin(Response::events(live.subscribe())),
                None => Response::text(404, "Not Found", "Live updating is off."),
            };
        }
        // The compiled program on its own, for a hot swap to fetch. The model
        // fingerprint rides along in a header so the page can tell whether the
        // model it is holding still fits.
        if request.path == BUNDLE {
            let build = self.build.lock().unwrap_or_else(|e| e.into_inner());
            let Some(javascript) = &build.javascript else {
                return Response::text(503, "Service Unavailable", "The build is broken.");
            };
            let mut response = allow_any_origin(Response {
                status: 200,
                reason: "OK",
                content_type: "text/javascript;charset=utf-8".to_string(),
                headers: Vec::new(),
                body: http::Body::Bytes(javascript.clone().into_bytes()),
            });
            if let Some(fingerprint) = &build.model_fingerprint {
                response
                    .headers
                    .push((MODEL_HEADER.to_string(), fingerprint.clone()));
            }
            return response;
        }
        if request.path == "/" || request.path == "/index.html" {
            return self.page();
        }
        // Anything else is served from the project directory, so a program can
        // load its own assets.
        match crate::reactor::static_file(&self.root, &request.path) {
            Some(response) => response,
            None => Response::not_found(pages::not_found(&request.path)),
        }
    }

    fn page(&self) -> Response {
        let build = self.build.lock().unwrap_or_else(|e| e.into_inner());
        let html = match (&build.javascript, &build.errors) {
            (Some(javascript), _) => crate::reactor::sandwich(&self.module, javascript, self.updating),
            (None, Some(errors)) => pages::errors(&self.module, errors),
            (None, None) => pages::errors(&self.module, "The build produced nothing."),
        };
        if !self.updating {
            return Response::html(html);
        }
        let shim = live::client_js_for(&Client {
            mode: self.mode,
            module: &self.module,
            bundle: BUNDLE,
            // alm serves this page, so relative URLs are right: they work over
            // `localhost`, `127.0.0.1` and a LAN address alike.
            origin: "",
            model_fingerprint: build.model_fingerprint.as_deref(),
            errors: build.errors.as_deref(),
        });
        Response::html(live::inject(&html, &shim))
    }
}

/// Where the bare program is served for a swap to fetch.
const BUNDLE: &str = "/_alm/bundle.js";

/// Open the two live endpoints to any origin.
///
/// A program embedded in a larger app is loaded by a page on another origin —
/// Django on `:8000`, Vite on `:5173` — and both the event stream and the
/// bundle fetch are cross-origin from there. Custom response headers are hidden
/// from a cross-origin reader unless named explicitly, which is what would
/// otherwise silently cost every swap its model. `*` rather than an echoed
/// Origin because this server binds to loopback and holds nothing private:
/// everything it serves, it serves to anyone who asks.
fn allow_any_origin(mut response: Response) -> Response {
    response.headers.push(("Access-Control-Allow-Origin".to_string(), "*".to_string()));
    response
        .headers
        .push(("Access-Control-Expose-Headers".to_string(), MODEL_HEADER.to_string()));
    response
}

/// Carries a fingerprint of the `Model` type the bundle was built with. A
/// fingerprint rather than the type as written: a real `Model` renders to
/// kilobytes, and Elm allows field names that no header value may carry.
pub const MODEL_HEADER: &str = "X-Alm-Model-Fingerprint";

fn compile(entry: &Path, root: &Path, color: bool, optimize: bool, source_maps: bool) -> Build {
    match alm_compiler::project::compile_project_live(entry, optimize, source_maps) {
        Ok(built) => Build {
            javascript: Some(built.javascript),
            source_map: built.source_map,
            model_fingerprint: built.model_fingerprint,
            errors: None,
        },
        Err(errors) => Build {
            javascript: None,
            source_map: None,
            model_fingerprint: None,
            errors: Some(errors.iter().map(|e| e.render_from(Some(root), color)).collect()),
        },
    }
}

/// What to call the change that started a rebuild: the file that moved, said
/// the way you would have typed it — relative to the project, since that is
/// where the server was started.
///
/// A save is one file, which is the case worth reading well. Several at once is
/// a branch switch or a formatter run, and the count is all there is to say
/// about it.
fn naming(moved: &[PathBuf], root: &Path) -> String {
    match moved {
        [] => "the sources".to_string(),
        [one] => one.strip_prefix(root).unwrap_or(one).display().to_string(),
        [first, rest @ ..] => format!(
            "{} and {} other file{}",
            first.strip_prefix(root).unwrap_or(first).display(),
            rest.len(),
            if rest.len() == 1 { "" } else { "s" }
        ),
    }
}

/// How long a build took, as a line of a rebuild notice. A build that fails in
/// the parser is over in well under a millisecond, and "0 ms" reads like the
/// timing is broken, so anything that fast keeps a decimal.
fn milliseconds(took: std::time::Duration) -> String {
    let ms = took.as_secs_f64() * 1000.0;
    if ms < 10.0 {
        format!("{ms:.1} ms")
    } else {
        format!("{ms:.0} ms")
    }
}

/// A JSON string literal, for putting report text in an SSE `data:` field
/// (which cannot contain a raw newline).
pub fn json_string(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
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
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_text_survives_being_put_in_an_event() {
        assert_eq!(json_string("a\nb"), "\"a\\nb\"");
        assert_eq!(json_string("say \"hi\""), "\"say \\\"hi\\\"\"");
        assert_eq!(json_string("tab\there"), "\"tab\\there\"");
        // An SSE data field must not contain a raw newline.
        assert!(!json_string("-- TYPE MISMATCH --\n\nnope").contains('\n'));
    }

    #[test]
    fn a_change_is_named_by_the_file_that_moved() {
        let root = Path::new("/home/me/app");
        let one = [PathBuf::from("/home/me/app/src/Main.elm")];
        assert_eq!(naming(&one, root), "src/Main.elm");

        let several = [
            PathBuf::from("/home/me/app/src/Main.elm"),
            PathBuf::from("/home/me/app/src/View.elm"),
        ];
        assert_eq!(naming(&several, root), "src/Main.elm and 1 other file");
        let three = vec![several[0].clone(), several[1].clone(), several[0].clone()];
        assert_eq!(naming(&three, root), "src/Main.elm and 2 other files");

        // A file outside the project keeps the path as it stands, rather than
        // being reported as something it is not.
        let elsewhere = [PathBuf::from("/packages/elm/core/src/List.elm")];
        assert_eq!(naming(&elsewhere, root), "/packages/elm/core/src/List.elm");
    }

    #[test]
    fn a_build_too_fast_to_measure_in_whole_milliseconds_keeps_a_decimal() {
        use std::time::Duration;
        assert_eq!(milliseconds(Duration::from_micros(400)), "0.4 ms");
        assert_eq!(milliseconds(Duration::from_millis(134)), "134 ms");
    }
}
