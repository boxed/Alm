//! `alm make --live` — build one program, serve it, and keep it up to date.
//!
//! Where `alm reactor` browses a whole directory and compiles on request, this
//! serves a single entry module at `/` and rebuilds it the moment its sources
//! change. Rebuilding ahead of the request is what makes a failed build
//! reportable without a reload: the page is told, and shows the errors over
//! the running program until the next build succeeds.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use crate::server::http::{self, Request, Response};
use crate::server::live::{self, Live, Mode};
use crate::server::pages;

/// The latest build, and how it went.
struct Build {
    /// The compiled program, kept from the last build that worked so a broken
    /// edit does not take the page down with it.
    javascript: Option<String>,
    /// The type of the program's `Model`, which decides whether a running
    /// model can be carried across a swap.
    model_type: Option<String>,
    /// Reports from the most recent build, if it failed.
    errors: Option<String>,
}

pub struct Options {
    pub entry: PathBuf,
    pub port: u16,
    pub updating: bool,
    pub color: bool,
}

pub fn run(options: Options) -> ExitCode {
    let root = alm_compiler::project::project_root(&options.entry);
    let module = crate::reactor::declared_module_name(&options.entry).unwrap_or_else(|| {
        options.entry.file_stem().unwrap_or_default().to_string_lossy().into_owned()
    });

    let build = Arc::new(Mutex::new(compile(&options.entry, &root, options.color)));
    if let Some(errors) = &build.lock().unwrap().errors {
        // Report the first build the way `alm make` would; the server still
        // comes up, so fixing the error is a save away.
        eprint!("{errors}");
    }

    let listener = match TcpListener::bind(("127.0.0.1", options.port)) {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("I could not listen on port {}: {err}", options.port);
            return ExitCode::FAILURE;
        }
    };
    println!("Go to http://localhost:{} to see {module}.", options.port);

    let mode = if options.updating { Mode::HotSwap } else { Mode::Reload };
    let live = options.updating.then(|| {
        let live = Live::new();
        live::heartbeat(live.clone());
        let watched = live.clone();
        let build = build.clone();
        let entry = options.entry.clone();
        let watched_root = root.clone();
        let color = options.color;
        live::watch(&root, move || {
            let fresh = compile(&entry, &watched_root, color);
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
            }
            match failed {
                Some(errors) => watched.broadcast("failed", &json_string(&errors)),
                None => watched.broadcast("changed", "null"),
            }
        });
        live
    });

    let state = State { root, module, build, live, mode, updating: options.updating };
    http::serve(listener, move |request| state.handle(request));
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
                Some(live) => Response::events(live.subscribe()),
                None => Response::text(404, "Not Found", "Live updating is off."),
            };
        }
        // The compiled program on its own, for a hot swap to fetch. The model
        // type rides along in a header so the page can tell whether the model
        // it is holding still fits.
        if request.path == BUNDLE {
            let build = self.build.lock().unwrap_or_else(|e| e.into_inner());
            let Some(javascript) = &build.javascript else {
                return Response::text(503, "Service Unavailable", "The build is broken.");
            };
            let mut response = Response {
                status: 200,
                reason: "OK",
                content_type: "text/javascript;charset=utf-8".to_string(),
                headers: Vec::new(),
                body: http::Body::Bytes(javascript.clone().into_bytes()),
            };
            if let Some(model) = &build.model_type {
                response.headers.push((MODEL_TYPE_HEADER.to_string(), model.clone()));
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
        let shim = live::client_js_for(
            self.mode,
            &self.module,
            BUNDLE,
            build.model_type.as_deref(),
            build.errors.as_deref(),
        );
        Response::html(live::inject(&html, &shim))
    }
}

/// Where the bare program is served for a swap to fetch.
const BUNDLE: &str = "/_alm/bundle.js";

/// Carries the `Model` type of the build the bundle came from.
pub const MODEL_TYPE_HEADER: &str = "X-Alm-Model-Type";

fn compile(entry: &Path, root: &Path, color: bool) -> Build {
    match alm_compiler::project::compile_project_live(entry) {
        Ok((javascript, model_type, _warnings)) => {
            Build { javascript: Some(javascript), model_type, errors: None }
        }
        Err(errors) => Build {
            javascript: None,
            model_type: None,
            errors: Some(errors.iter().map(|e| e.render_from(Some(root), color)).collect()),
        },
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
}
