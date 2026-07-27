//! Watch the sources, tell the browser when they change.
//!
//! A background thread polls the project's `.elm` files; when one moves, every
//! page that has the client shim loaded is sent an event over Server-Sent
//! Events. Polling rather than an OS watch API is a deliberate trade: alm has
//! no dependencies, and a dev-sized tree costs well under a millisecond to
//! stat.
//!
//! SSE rather than WebSockets for the same reason — it is a plain HTTP
//! response that never ends, so the hand-rolled server in [`super::http`]
//! already knows how to send one, and `EventSource` reconnects on its own when
//! the server restarts.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

/// How often the watcher looks for changes. Short enough to feel immediate,
/// long enough that a large tree costs nothing measurable.
const POLL: Duration = Duration::from_millis(250);

/// How often a quiet connection gets a comment frame. Without it a dropped
/// browser is never noticed, and the sender leaks.
const HEARTBEAT: Duration = Duration::from_secs(15);

/// What the page should do when the sources change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Swap the new program in and keep the model where that is safe, falling
    /// back to a reload when it is not.
    HotSwap,
    /// Reload the page. Simpler, and never wrong.
    Reload,
}

/// The set of connected pages.
#[derive(Default)]
pub struct Live {
    subscribers: Mutex<Vec<Sender<String>>>,
}

impl Live {
    pub fn new() -> Arc<Live> {
        Arc::new(Live::default())
    }

    /// Attach a page. The receiver is handed to the HTTP layer as the body of
    /// a never-ending response.
    pub fn subscribe(&self) -> Receiver<String> {
        let (tx, rx) = std::sync::mpsc::channel();
        // An initial comment flushes the response headers, so `EventSource`
        // reports the connection open rather than waiting for the first
        // change.
        let _ = tx.send(": connected\n\n".to_string());
        self.subscribers.lock().unwrap_or_else(|e| e.into_inner()).push(tx);
        rx
    }

    /// Send an event to every page, dropping the ones that have gone away.
    pub fn broadcast(&self, event: &str, data: &str) {
        let frame = format!("event: {event}\ndata: {data}\n\n");
        let mut subscribers = self.subscribers.lock().unwrap_or_else(|e| e.into_inner());
        subscribers.retain(|tx| tx.send(frame.clone()).is_ok());
    }

    pub fn connections(&self) -> usize {
        self.subscribers.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

/// Poll `root` for changes to Elm sources and call `on_change` when they
/// settle. Returns immediately; the watching happens on its own thread.
///
/// `on_change` runs on that thread, so a rebuild there does not block the
/// server from answering requests.
pub fn watch<F>(root: &Path, on_change: F)
where
    F: Fn() + Send + 'static,
{
    let root = root.to_path_buf();
    std::thread::spawn(move || {
        let mut seen = snapshot(&root);
        loop {
            std::thread::sleep(POLL);
            let now = snapshot(&root);
            if now != seen {
                seen = now;
                on_change();
            }
        }
    });
}

/// Send a heartbeat to every page periodically, so a browser that closed is
/// noticed and its sender dropped.
pub fn heartbeat(live: Arc<Live>) {
    std::thread::spawn(move || loop {
        std::thread::sleep(HEARTBEAT);
        let mut subscribers = live.subscribers.lock().unwrap_or_else(|e| e.into_inner());
        subscribers.retain(|tx| tx.send(": ping\n\n".to_string()).is_ok());
    });
}

/// Every Elm source under `root`, with its size and modification time.
/// Comparing two of these catches edits, additions and deletions alike.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, (u64, Option<SystemTime>)> {
    let mut out = BTreeMap::new();
    collect(root, &mut out, 0);
    out
}

fn collect(dir: &Path, out: &mut BTreeMap<PathBuf, (u64, Option<SystemTime>)>, depth: u32) {
    // Deep enough for any real source tree, and a stop for a symlink loop.
    if depth > 32 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // Nothing here is a source: skipping them keeps the poll cheap on a
        // tree with a big build directory in it.
        if name.starts_with('.') || name == "elm-stuff" || name == "node_modules" {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            collect(&path, out, depth + 1);
        } else if path.extension().is_some_and(|e| e == "elm") {
            let meta = entry.metadata().ok();
            let len = meta.as_ref().map(|m| m.len()).unwrap_or(0);
            let modified = meta.and_then(|m| m.modified().ok());
            out.insert(path, (len, modified));
        }
    }
}

/// The endpoint the client shim connects to.
pub const ENDPOINT: &str = "/_alm/live";

/// The script injected into a page that only ever reloads — the reactor,
/// which recompiles on request, so there is nothing to fetch.
pub fn client_js(mode: Mode) -> String {
    client_js_for(mode, "", "", None, None)
}

/// The script injected into a served page.
///
/// On a change it fetches the freshly built program and swaps it in, keeping
/// the model when the new build still means the same thing by `Model`.
/// Anything it cannot do cleanly ends in a reload, which is always correct.
///
/// `bundle` empty means there is nothing to fetch, so every change reloads.
pub fn client_js_for(
    mode: Mode,
    module: &str,
    bundle: &str,
    model_type: Option<&str>,
    errors: Option<&str>,
) -> String {
    let hot = mode == Mode::HotSwap && !bundle.is_empty();
    format!(
        r#"(function () {{
  var HOT = {hot};
  var MODULE = {module:?};
  var BUNDLE = {bundle:?};
  var MODEL_TYPE = {model_type};
  var source = new EventSource({endpoint:?});
  var reloading = false;

  function reload() {{
    if (reloading) return;
    reloading = true;
    window.location.reload();
  }}

  source.addEventListener('failed', function (e) {{
    // The rebuild did not compile. The running program is left alone and the
    // reports go over the top of it; the next good build clears them.
    showErrors(JSON.parse(e.data));
  }});

  source.addEventListener('changed', function () {{
    hideErrors();
    if (!HOT) {{ reload(); return; }}
    swap().catch(function (err) {{
      console.warn('[alm] hot swap not possible, reloading:', err && err.message || err);
      reload();
    }});
  }});

  function swap() {{
    var app = window.__alm_app__;
    if (!app || !app.__alm_teardown) {{
      return Promise.reject(new Error('the running program has no swap hooks'));
    }}
    return fetch(BUNDLE, {{ cache: 'no-store' }}).then(function (res) {{
      if (!res.ok) throw new Error('the new build is not available');
      var freshType = res.headers.get({header:?});
      return res.text().then(function (js) {{
        // Evaluate the new bundle against its own scope, so its runtime is
        // separate from the one still running.
        var scope = {{}};
        new Function(js).call(scope);
        var mod = scope.Elm && scope.Elm[MODULE];
        if (!mod || !mod.init) throw new Error('the new build has no ' + MODULE + '.init');

        // Only carry the model across if the new build still agrees about
        // what a Model is. Restoring one into a program that reads it
        // differently is how a hot reload corrupts your session.
        var keep = MODEL_TYPE !== null && freshType !== null && freshType === MODEL_TYPE;
        var model = keep ? app.__alm_model() : undefined;

        // Mounting replaced the original node with the program's own root, so
        // the new program goes exactly where the old root is standing.
        var oldRoot = app.__alm_root && app.__alm_root();
        var ownsPage = app.__alm_kind === 'document' || app.__alm_kind === 'application';
        app.__alm_teardown();

        var opts = {{}};
        if (keep) opts.__alm_model = model;
        if (ownsPage) {{
          // These mount themselves into <body>; just clear the old one out.
          if (oldRoot && oldRoot.parentNode) oldRoot.parentNode.removeChild(oldRoot);
        }} else {{
          var slot = document.createElement('div');
          if (oldRoot && oldRoot.parentNode) {{
            oldRoot.parentNode.replaceChild(slot, oldRoot);
          }} else {{
            document.body.appendChild(slot);
          }}
          opts.node = slot;
        }}
        window.__alm_app__ = mod.init(opts);
        MODEL_TYPE = freshType;
        if (!keep) console.info('[alm] Model changed, started fresh');
      }});
    }});
  }}

  var box = null;
  function showErrors(text) {{
    if (!box) {{
      box = document.createElement('pre');
      box.setAttribute('data-alm-errors', '');
      box.style.cssText = 'position:fixed;inset:0;z-index:2147483647;margin:0;'
        + 'padding:1.5rem;overflow:auto;background:#1e1e1e;color:#eee;'
        + 'font:13px/1.45 SFMono-Regular,Consolas,monospace;white-space:pre-wrap';
      document.body.appendChild(box);
    }}
    box.textContent = text;
  }}
  function hideErrors() {{
    if (box) {{ box.remove(); box = null; }}
  }}
{initial}}})();"#,
        hot = if hot { "true" } else { "false" },
        module = module,
        bundle = bundle,
        model_type = match model_type {
            Some(t) => format!("{t:?}"),
            None => "null".to_string(),
        },
        endpoint = ENDPOINT,
        header = crate::serve::MODEL_TYPE_HEADER,
        // A page served while the build is broken shows the reports at once,
        // rather than looking fine until the next save.
        initial = match errors {
            Some(errors) => format!("  showErrors({});\n", crate::serve::json_string(errors)),
            None => String::new(),
        },
    )
}

/// Put the shim just before `</body>`, or at the end if there is no such tag.
pub fn inject(html: &str, script: &str) -> String {
    let tag = format!("<script>\n{script}\n</script>\n");
    match html.rfind("</body>") {
        Some(at) => format!("{}{tag}{}", &html[..at], &html[at..]),
        None => format!("{html}{tag}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_subscriber_hears_a_broadcast_and_a_dead_one_is_dropped() {
        let live = Live::new();
        let rx = live.subscribe();
        assert_eq!(live.connections(), 1);
        // The greeting comes first, so the headers go out immediately.
        assert_eq!(rx.recv().unwrap(), ": connected\n\n");
        live.broadcast("changed", "null");
        assert_eq!(rx.recv().unwrap(), "event: changed\ndata: null\n\n");

        drop(rx);
        live.broadcast("changed", "null");
        assert_eq!(live.connections(), 0, "a closed page must not be kept");
    }

    #[test]
    fn the_snapshot_sees_elm_files_and_ignores_the_rest() {
        let dir = std::env::temp_dir().join(format!("alm-live-snap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src/Deep")).unwrap();
        std::fs::create_dir_all(dir.join("elm-stuff")).unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join("src/Main.elm"), "a").unwrap();
        std::fs::write(dir.join("src/Deep/Sub.elm"), "b").unwrap();
        std::fs::write(dir.join("src/notes.md"), "c").unwrap();
        std::fs::write(dir.join("elm-stuff/Cached.elm"), "d").unwrap();
        std::fs::write(dir.join(".git/Hook.elm"), "e").unwrap();

        let seen = snapshot(&dir);
        let names: Vec<String> = seen
            .keys()
            .map(|p| p.strip_prefix(&dir).unwrap().display().to_string())
            .collect();
        assert_eq!(names, vec!["src/Deep/Sub.elm", "src/Main.elm"]);

        // A change to a source shows up; the build directory does not.
        std::fs::write(dir.join("src/Main.elm"), "changed").unwrap();
        assert_ne!(snapshot(&dir), seen);
        let seen = snapshot(&dir);
        std::fs::write(dir.join("elm-stuff/Cached.elm"), "changed too").unwrap();
        assert_eq!(snapshot(&dir), seen);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_shim_goes_inside_the_body() {
        let page = "<html>\n<body>\n<p>hi</p>\n</body>\n</html>\n";
        let out = inject(page, "X");
        assert!(out.contains("<p>hi</p>\n<script>\nX\n</script>\n</body>"), "{out}");
        // A fragment with no body tag still gets it.
        assert!(inject("<p>hi</p>", "X").ends_with("<script>\nX\n</script>\n"));
    }

    /// Swapping needs both an intent to swap and somewhere to fetch the new
    /// build from. The reactor has no bundle endpoint — it recompiles on
    /// request — so its pages always reload.
    #[test]
    fn the_shim_only_swaps_when_it_can() {
        let swapping =
            client_js_for(Mode::HotSwap, "Main", "/_alm/bundle.js", Some("Int"), None);
        assert!(swapping.contains("var HOT = true"), "{swapping}");
        assert!(swapping.contains(r#"var MODEL_TYPE = "Int""#), "{swapping}");

        let reloading =
            client_js_for(Mode::Reload, "Main", "/_alm/bundle.js", Some("Int"), None);
        assert!(reloading.contains("var HOT = false"));

        // No bundle to fetch, so no swap however the mode is set.
        assert!(client_js(Mode::HotSwap).contains("var HOT = false"));
        assert!(client_js(Mode::Reload).contains("var HOT = false"));

        // An unknown model type must not be mistaken for a matching one.
        let unknown = client_js_for(Mode::HotSwap, "Main", "/b.js", None, None);
        assert!(unknown.contains("var MODEL_TYPE = null"), "{unknown}");
    }

    /// A page served while the build is broken shows the reports at once.
    #[test]
    fn a_broken_build_is_shown_on_arrival() {
        let shim = client_js_for(Mode::HotSwap, "Main", "/b.js", None, Some("-- OOPS --\nbad"));
        assert!(shim.contains(r#"showErrors("-- OOPS --\nbad")"#), "{shim}");
        assert!(!client_js_for(Mode::HotSwap, "Main", "/b.js", None, None).contains("showErrors(\""));
    }
}
