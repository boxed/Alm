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

/// Install the registry that programs put themselves in as they start.
///
/// This has to run *before* any bundle evaluates, which is why it is a separate
/// snippet rather than part of the shim: the shim can go anywhere, but a
/// program that starts before the registry exists is invisible to it.
/// Idempotent, so a page carrying two live bundles shares one registry.
pub fn registry_js() -> &'static str {
    "window.__alm_hot__ = window.__alm_hot__ || { apps: [] };"
}

/// How the client shim is reached and what it should swap.
pub struct Client<'a> {
    pub mode: Mode,
    /// The entry module's name — for diagnostics only; which programs get
    /// swapped comes from the registry.
    pub module: &'a str,
    /// Where to fetch a new build. Empty means there is nothing to fetch, so
    /// every change reloads.
    pub bundle: &'a str,
    /// Absolute origin of the alm server, e.g. `http://127.0.0.1:8413`. Empty
    /// keeps the URLs relative, which is right when alm serves the page too:
    /// it then works over `localhost`, `127.0.0.1` or a LAN address alike.
    /// A bundle embedded in another app's page needs the absolute form,
    /// because that page comes from another origin.
    pub origin: &'a str,
    pub model_fingerprint: Option<&'a str>,
    pub errors: Option<&'a str>,
}

/// The script injected into a page that only ever reloads — the reactor,
/// which recompiles on request, so there is nothing to fetch.
pub fn client_js(mode: Mode) -> String {
    client_js_for(&Client {
        mode,
        module: "",
        bundle: "",
        origin: "",
        model_fingerprint: None,
        errors: None,
    })
}

/// The live-reload client.
///
/// On a change it fetches the freshly built program and swaps every running
/// program that came from this bundle, keeping their models when the new build
/// still means the same thing by `Model`. Anything it cannot do cleanly ends in
/// a reload, which is always correct.
pub fn client_js_for(client: &Client<'_>) -> String {
    let Client { mode, module, bundle, origin, model_fingerprint, errors } = *client;
    let hot = mode == Mode::HotSwap && !bundle.is_empty();
    format!(
        r#"(function () {{
  {registry}
  var HOT = {hot};
  var MODULE = {module:?};
  var BUNDLE = {bundle};
  var MODEL = {model_fingerprint};
  var source = new EventSource({endpoint});
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
    var hot = window.__alm_hot__;
    var running = (hot && hot.apps || []).slice();
    if (!running.length) {{
      return Promise.reject(new Error('no running program registered'));
    }}
    return fetch(BUNDLE, {{ cache: 'no-store' }}).then(function (res) {{
      if (!res.ok) throw new Error('the new build is not available');
      var fresh = res.headers.get({header:?});
      return res.text().then(function (js) {{
        // Evaluate the new bundle against its own scope, so its runtime is
        // separate from the one still running.
        var scope = {{}};
        new Function(js).call(scope);
        if (!scope.Elm) throw new Error('the new build published no Elm');

        // A page can be running several programs — a few Elm widgets embedded
        // in a larger app — and each one this bundle exports needs replacing.
        // The ones it does not export belong to another bundle: left alone.
        var mine = running.filter(function (entry) {{
          var mod = scope.Elm[entry.module];
          return mod && typeof mod.init === 'function';
        }});
        if (!mine.length) throw new Error('the new build has no ' + MODULE + '.init');

        // Drop them before re-initializing: `init` registers each new program
        // as it starts, so leaving the old entries in would grow the registry
        // by a torn-down program per save.
        hot.apps = hot.apps.filter(function (entry) {{ return mine.indexOf(entry) === -1; }});

        // Only carry a model across if the new build still agrees about what a
        // Model is. Restoring one into a program that reads it differently is
        // how a hot reload corrupts your session.
        var keep = MODEL !== null && fresh !== null && fresh === MODEL;
        mine.forEach(function (entry) {{
          var app = entry.app;
          if (!app || !app.__alm_teardown) throw new Error('a running program has no swap hooks');
          var model = keep ? app.__alm_model() : undefined;

          // Mounting replaced the original node with the program's own root, so
          // the new program goes exactly where the old root is standing.
          var oldRoot = app.__alm_root && app.__alm_root();
          var ownsPage = app.__alm_kind === 'document' || app.__alm_kind === 'application';
          app.__alm_teardown();

          // Start from the options it was given, so `flags` come along. A fresh
          // page load would pass them; a swap that dropped them would fail the
          // program's flags decoder instead. The model is the one thing not
          // carried over that way: these options may be a previous swap's, and
          // reusing that model when the `Model` has since changed is exactly
          // what the fingerprint check is there to prevent.
          var opts = {{}};
          for (var k in entry.opts) {{ opts[k] = entry.opts[k]; }}
          delete opts.__alm_model;
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
          scope.Elm[entry.module].init(opts);
        }});
        MODEL = fresh;
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
        registry = registry_js(),
        hot = if hot { "true" } else { "false" },
        module = module,
        // Both URLs are absolute when the shim rides in a bundle another app's
        // page loads, and relative when alm serves the page itself.
        bundle = format!("{:?}", format!("{origin}{bundle}")),
        model_fingerprint = match model_fingerprint {
            Some(f) => format!("{f:?}"),
            None => "null".to_string(),
        },
        endpoint = format!("{:?}", format!("{origin}{ENDPOINT}")),
        header = crate::serve::MODEL_HEADER,
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

    /// A shim for a page alm serves: relative URLs, no fingerprint by default.
    fn shim(mode: Mode, bundle: &str, fingerprint: Option<&str>) -> String {
        client_js_for(&Client {
            mode,
            module: "Main",
            bundle,
            origin: "",
            model_fingerprint: fingerprint,
            errors: None,
        })
    }

    /// Swapping needs both an intent to swap and somewhere to fetch the new
    /// build from. The reactor has no bundle endpoint — it recompiles on
    /// request — so its pages always reload.
    #[test]
    fn the_shim_only_swaps_when_it_can() {
        let swapping = shim(Mode::HotSwap, "/_alm/bundle.js", Some("abc123"));
        assert!(swapping.contains("var HOT = true"), "{swapping}");
        assert!(swapping.contains(r#"var MODEL = "abc123""#), "{swapping}");

        let reloading = shim(Mode::Reload, "/_alm/bundle.js", Some("abc123"));
        assert!(reloading.contains("var HOT = false"));

        // No bundle to fetch, so no swap however the mode is set.
        assert!(client_js(Mode::HotSwap).contains("var HOT = false"));
        assert!(client_js(Mode::Reload).contains("var HOT = false"));

        // An unknown fingerprint must not be mistaken for a matching one.
        let unknown = shim(Mode::HotSwap, "/b.js", None);
        assert!(unknown.contains("var MODEL = null"), "{unknown}");
    }

    /// A page served while the build is broken shows the reports at once.
    #[test]
    fn a_broken_build_is_shown_on_arrival() {
        let reported = client_js_for(&Client {
            mode: Mode::HotSwap,
            module: "Main",
            bundle: "/b.js",
            origin: "",
            model_fingerprint: None,
            errors: Some("-- OOPS --\nbad"),
        });
        assert!(reported.contains(r#"showErrors("-- OOPS --\nbad")"#), "{reported}");
        assert!(!shim(Mode::HotSwap, "/b.js", None).contains("showErrors(\""));
    }

    /// A bundle embedded in another app's page is loaded from another origin, so
    /// its shim must reach back with absolute URLs. The shim alm injects into
    /// its own page keeps them relative.
    #[test]
    fn an_embedded_shim_calls_home_by_absolute_url() {
        let embedded = client_js_for(&Client {
            mode: Mode::HotSwap,
            module: "Main",
            bundle: "/_alm/bundle.js",
            origin: "http://127.0.0.1:8413",
            model_fingerprint: Some("abc123"),
            errors: None,
        });
        assert!(
            embedded.contains(r#"var BUNDLE = "http://127.0.0.1:8413/_alm/bundle.js""#),
            "{embedded}"
        );
        assert!(
            embedded.contains(r#"EventSource("http://127.0.0.1:8413/_alm/live")"#),
            "{embedded}"
        );

        let served = shim(Mode::HotSwap, "/_alm/bundle.js", Some("abc123"));
        assert!(served.contains(r#"var BUNDLE = "/_alm/bundle.js""#), "{served}");
        assert!(served.contains(r#"EventSource("/_alm/live")"#), "{served}");
    }

    /// The shim can only find a program through the registry, so it must install
    /// one itself — a page that loads it late still gets later programs.
    #[test]
    fn the_shim_carries_the_registry() {
        assert!(shim(Mode::HotSwap, "/b.js", None).contains("__alm_hot__"));
        assert!(registry_js().contains("window.__alm_hot__ = window.__alm_hot__ ||"));
    }
}
