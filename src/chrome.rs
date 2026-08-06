//! The `bru://` scheme, and the chrome assets it serves.
//!
//! The tab strip and the status line are HTML pages, so they need an origin. A custom scheme gives
//! them one that is not `file://` and not a `data:` URI: a real, secure, CORS-enabled origin that
//! the message router's security check can name in one comparison (`bru://`).
//!
//! Everything except the theme is compiled in — the binary carries its own chrome, as DESIGN.md
//! requires ("bru ships a binary and generated themes, nothing a person edits"). `theme.css` is the
//! one exception: it is read from `~/.config/bru/theme.css` on every request, so `themer` can swap a
//! palette under a running browser and a reload is enough to see it.

use cef::wrapper::byte_read_handler::{ByteReadHandler, ByteStream};
use cef::wrapper::stream_resource_handler::StreamResourceHandler;
use cef::*;
use std::sync::{Arc, Mutex};

/// The scheme name, spelled once.
const SCHEME: &str = "bru";

/// Everything is served under one host, so `bru://chrome/top.html` is a URL and not a hostname
/// guess. A STANDARD scheme is parsed like http, which means relative `<link href="chrome.css">`
/// resolves the way an author expects.
const ORIGIN: &str = "bru://chrome";

const TOP_HTML: &[u8] = include_bytes!("../chrome/top.html");
const BOTTOM_HTML: &[u8] = include_bytes!("../chrome/bottom.html");
const CHROME_CSS: &[u8] = include_bytes!("../chrome/chrome.css");
const TOP_JS: &[u8] = include_bytes!("../chrome/top.js");
const BOTTOM_JS: &[u8] = include_bytes!("../chrome/bottom.js");

/// The fallback palette, used until `lvim-colorscheme` has written a real one to
/// `~/.config/bru/theme.css`. It is not a theme — it exists so the chrome is legible on a machine
/// that has not been themed yet, and so a missing file is never a blank bar.
const DEFAULT_THEME_CSS: &str = "\
:root {
  --bg: #1e222a;
  --fg: #b6bdca;
  --font: monospace;
  --tabs-bg: #1e222a;
  --tabs-fg: #6b7089;
  --tabs-separator: #2a2f3a;
  --tabs-selected-bg: #2a2f3a;
  --tabs-selected-fg: #b6bdca;
  --statusbar-bg: #1e222a;
  --statusbar-fg: #b6bdca;
  --statusbar-keystring-fg: #d19a66;
}
";

/// Called from `App::on_register_custom_schemes`, which runs in **every** process — browser,
/// renderer, GPU, zygote. A renderer that never heard of `bru://` treats it as an opaque origin and
/// refuses to load the page, which is why the App has to reach `execute_process` too.
pub fn register_scheme(registrar: Option<&mut SchemeRegistrar>) {
    let Some(registrar) = registrar else {
        return;
    };

    // SchemeOptions implements no BitOr — verified against the bindings on 2026-08-06 — so the
    // flags are combined on the raw values.
    let options = (SchemeOptions::STANDARD.get_raw()
        | SchemeOptions::SECURE.get_raw()
        | SchemeOptions::CORS_ENABLED.get_raw()) as i32;

    registrar.add_custom_scheme(Some(&CefString::from(SCHEME)), options);
}

/// Called from `on_context_initialized`, before any browser exists. Registering the factory after a
/// browser has already asked for a `bru://` URL is a load error nobody can see the cause of.
pub fn register_factory() {
    let mut factory = BruSchemeHandlerFactory::new();
    register_scheme_handler_factory(Some(&CefString::from(SCHEME)), None, Some(&mut factory));
}

wrap_scheme_handler_factory! {
    pub struct BruSchemeHandlerFactory;

    impl SchemeHandlerFactory {
        fn create(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _scheme_name: Option<&CefString>,
            request: Option<&mut Request>,
        ) -> Option<ResourceHandler> {
            let url = request
                .map(|request| CefString::from(&request.url()).to_string())
                .unwrap_or_default();

            let (mime, bytes) = asset(&url)?;
            serve(mime, bytes)
        }
    }
}

/// Map a `bru://` URL to the bytes behind it. `None` means 404 — CEF shows its own error page,
/// which is the right answer for a typo in a `<link>`.
fn asset(url: &str) -> Option<(&'static str, Vec<u8>)> {
    // Chromium hands over the full URL including query and fragment; neither addresses an asset.
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let path = path.strip_prefix(ORIGIN)?;

    match path {
        "/top.html" => Some(("text/html", TOP_HTML.to_vec())),
        "" | "/" | "/bottom.html" => Some(("text/html", BOTTOM_HTML.to_vec())),
        "/chrome.css" => Some(("text/css", CHROME_CSS.to_vec())),
        "/top.js" => Some(("text/javascript", TOP_JS.to_vec())),
        "/bottom.js" => Some(("text/javascript", BOTTOM_JS.to_vec())),
        "/theme.css" => Some(("text/css", theme_css())),
        _ => None,
    }
}

/// The one asset that is not compiled in. Read per request rather than cached: the file belongs to
/// themer, which rewrites it whenever the desktop theme changes, and a cache would mean bru had to
/// be restarted to notice.
fn theme_css() -> Vec<u8> {
    match theme_path().and_then(|path| std::fs::read(path).ok()) {
        Some(bytes) => bytes,
        None => DEFAULT_THEME_CSS.as_bytes().to_vec(),
    }
}

fn theme_path() -> Option<std::path::PathBuf> {
    // XDG, hand-rolled rather than pulled in as a dependency: two environment variables.
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").filter(|dir| !dir.is_empty()) {
        return Some(std::path::PathBuf::from(dir).join("bru/theme.css"));
    }
    let home = std::env::var_os("HOME").filter(|home| !home.is_empty())?;
    Some(std::path::PathBuf::from(home).join(".config/bru/theme.css"))
}

/// Hand a `Vec<u8>` to CEF as a resource. The crate ships every piece of this: a ReadHandler over a
/// byte slice, a StreamReader over the handler, and a ResourceHandler over the stream.
fn serve(mime: &str, bytes: Vec<u8>) -> Option<ResourceHandler> {
    let mut read_handler = ByteReadHandler::new(Arc::new(Mutex::new(ByteStream::new(bytes))));
    let stream = stream_reader_create_for_handler(Some(&mut read_handler))?;
    Some(StreamResourceHandler::new_with_stream(
        mime.to_string(),
        stream,
    ))
}
