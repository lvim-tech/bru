//! The bridge between the HTML chrome and Rust.
//!
//! Two directions, and deliberately only two:
//!
//! - **chrome → Rust** is `window.cefQuery`, CEF's message router. The chrome pages use it exactly
//!   once each, on load, to say they exist. Nothing on the key path goes through here.
//! - **Rust → chrome** is `frame.execute_java_script("bru.render({...})")`. Rust owns every piece of
//!   state and pushes it; the pages never ask.
//!
//! The router injects `window.cefQuery` into *every* frame, ordinary web pages included, so the
//! handler below refuses any query whose frame is not a `bru://` page. A page that could call
//! cefQuery could drive the browser.
//!
//! The router itself is a pair: `BrowserSideRouter` in the browser process, `RendererSideRouter` in
//! each renderer, built from the same `MessageRouterConfig`. Four browser-side callbacks have to be
//! forwarded "exactly as documented" (see `cef::wrapper::message_router`); a missing
//! `on_before_browse` leaks pending queries with no error anywhere.

use cef::wrapper::message_router::*;
use cef::*;
use std::sync::{Arc, Mutex, OnceLock};

// -----------------------------------------------------------------------------------------------
// Browser side
// -----------------------------------------------------------------------------------------------

/// The browser half of the router, built once per browser process. It is reached through a
/// singleton rather than carried in a handler field because four unrelated CEF callbacks have to
/// hand it the same instance, and two routers would each answer half the queries.
fn browser_router() -> &'static Arc<BrowserSideRouter> {
    static ROUTER: OnceLock<Arc<BrowserSideRouter>> = OnceLock::new();
    ROUTER.get_or_init(|| {
        let router = BrowserSideRouter::new(MessageRouterConfig::default());
        router.add_handler(Arc::new(BruQueryHandler), false);
        log("browser-side message router constructed");
        router
    })
}

/// Forward from `Client::on_process_message_received`.
pub fn on_process_message_received(
    browser: Option<&mut Browser>,
    frame: Option<&mut Frame>,
    source_process: ProcessId,
    message: Option<&mut ProcessMessage>,
) -> ::std::os::raw::c_int {
    // bru's own renderer→browser messages first. The router only recognises its own names, but
    // handing it a message that is not a query is work on the path a scroll takes.
    if crate::scroll::on_report(browser.as_deref(), message.as_deref()) {
        return 1;
    }
    browser_router().on_process_message_received(
        browser.cloned(),
        frame.cloned(),
        source_process,
        message.cloned(),
    ) as ::std::os::raw::c_int
}

/// Forward from `LifeSpanHandler::on_before_close`.
pub fn on_before_close(browser: Option<&mut Browser>) {
    forget_chrome_frames_of(browser.as_deref());
    browser_router().on_before_close(browser.cloned());
}

/// Forward from `RequestHandler::on_before_browse`, only when the navigation is allowed.
pub fn on_before_browse(browser: Option<&mut Browser>, frame: Option<&mut Frame>) {
    browser_router().on_before_browse(browser.cloned(), frame.cloned());
}

/// Forward from `RequestHandler::on_render_process_terminated`.
pub fn on_render_process_terminated(browser: Option<&mut Browser>) {
    browser_router().on_render_process_terminated(browser.cloned());
}

/// The one query handler. Everything the chrome can ask for arrives here as a JSON string.
struct BruQueryHandler;

impl BrowserSideHandler for BruQueryHandler {
    fn on_query_str(
        &self,
        _browser: Option<Browser>,
        frame: Option<Frame>,
        _query_id: i64,
        request: &str,
        _persistent: bool,
        callback: Arc<Mutex<dyn BrowserSideCallback>>,
    ) -> bool {
        let url = frame
            .as_ref()
            .map(|frame| CefString::from(&frame.url()).to_string())
            .unwrap_or_default();

        // The security check, and it is the whole reason this function starts here. cefQuery is
        // registered on the window object of every V8 context the renderer creates, which includes
        // every page bru ever visits. Only bru's own chrome may use it.
        if !url.starts_with("bru://") {
            eprintln!("bru: refused a cefQuery from {url:?}");
            fail(&callback, -2, "cefQuery is for bru:// chrome pages only");
            return true;
        }

        log(&format!("query from {url}: {request}"));

        match json_field(request, "type").as_deref() {
            Some("ready") => {
                let view = json_field(request, "view").unwrap_or_default();
                let Some(frame) = frame else {
                    fail(&callback, -3, "ready without a frame");
                    return true;
                };
                let response = match view.as_str() {
                    "top" => {
                        chrome().lock().ok().map(|mut c| c.top = Some(frame));
                        tabs_json()
                    }
                    "bottom" => {
                        chrome().lock().ok().map(|mut c| c.bottom = Some(frame));
                        bar_json()
                    }
                    other => {
                        fail(&callback, -4, &format!("unknown view {other:?}"));
                        return true;
                    }
                };
                // The answer is the current state, so the page is never blank between load and the
                // first push.
                succeed(&callback, &response);
            }
            // A round trip with no side effect, for proving the router is wired end to end.
            Some("echo") => {
                succeed(&callback, &json_field(request, "text").unwrap_or_default());
            }
            other => {
                fail(&callback, -5, &format!("unknown query type {other:?}"));
            }
        }

        true
    }
}

fn succeed(callback: &Arc<Mutex<dyn BrowserSideCallback>>, response: &str) {
    if let Ok(callback) = callback.lock() {
        callback.success_str(response);
    }
}

fn fail(callback: &Arc<Mutex<dyn BrowserSideCallback>>, code: i32, message: &str) {
    if let Ok(callback) = callback.lock() {
        callback.failure(code, message);
    }
}

// -----------------------------------------------------------------------------------------------
// The chrome frames, and the state pushed into them
// -----------------------------------------------------------------------------------------------

#[derive(Default)]
struct ChromeFrames {
    top: Option<Frame>,
    bottom: Option<Frame>,
}

fn chrome() -> &'static Mutex<ChromeFrames> {
    static CHROME: Mutex<ChromeFrames> = Mutex::new(ChromeFrames {
        top: None,
        bottom: None,
    });
    &CHROME
}

/// What the bottom bar shows. `mode`, `keystring`, `scroll` and `tabindex` are part of the pushed
/// object from the start so the chrome renders against its final shape; the code that fills them is
/// the mode machine, which is a later milestone.
struct BarState {
    url: String,
    title: String,
    mode: String,
    keystring: String,
    scroll: String,
    tabindex: String,
}

fn bar() -> &'static Mutex<BarState> {
    static BAR: Mutex<BarState> = Mutex::new(BarState {
        url: String::new(),
        title: String::new(),
        mode: String::new(),
        keystring: String::new(),
        scroll: String::new(),
        tabindex: String::new(),
    });
    &BAR
}

/// From `DisplayHandler::on_address_change` on the main frame.
pub fn set_url(url: String) {
    if let Ok(mut bar) = bar().lock() {
        bar.url = url;
    }
    push();
}

/// From `DisplayHandler::on_title_change`.
pub fn set_title(title: String) {
    if let Ok(mut bar) = bar().lock() {
        bar.title = title;
    }
    push();
}

/// The current mode, spelled as qutebrowser spells it. The chrome colours the bar by it.
pub fn set_mode(mode: String) {
    if let Ok(mut bar) = bar().lock() {
        bar.mode = mode;
    }
    push();
}

/// The pending key chain and count — `g` after `g`, `3` after `3`, empty once something ran. This
/// is qutebrowser's keystring widget, and it is what makes a half-typed `gg` visible.
pub fn set_keystring(keystring: String) {
    let changed = match bar().lock() {
        Ok(mut bar) if bar.keystring != keystring => {
            bar.keystring = keystring;
            true
        }
        _ => false,
    };
    // Every keypress reaches here, and most leave the string as it was. Pushing regardless would
    // run a script in the chrome renderer on every `j` — on the one path this project exists to
    // keep fast.
    if changed {
        push();
    }
}

/// The scroll percentage, spelled as qutebrowser's percentage widget spells it: `[top]`, `[42%]`,
/// `[bot]`. Built by `scroll.rs` from what the page reports, and pushed only when it changes — a
/// held `j` reaches this on every settled position and a push each time would run a script in the
/// chrome renderer for a string that is already right.
pub fn set_scroll(scroll: String) {
    let changed = match bar().lock() {
        Ok(mut bar) if bar.scroll != scroll => {
            bar.scroll = scroll;
            true
        }
        _ => false,
    };
    if changed {
        push();
    }
}

/// Push the current state into whichever chrome frames have announced themselves.
fn push() {
    let (top, bottom) = match chrome().lock() {
        Ok(chrome) => (chrome.top.clone(), chrome.bottom.clone()),
        Err(_) => return,
    };

    if let Some(frame) = top {
        render(&frame, &tabs_json());
    }
    if let Some(frame) = bottom {
        render(&frame, &bar_json());
    }
}

fn render(frame: &Frame, state_json: &str) {
    // Guarded on `window.bru`: a push can land between navigation start and the script running.
    let code = format!("window.bru && window.bru.render({state_json});");
    frame.execute_java_script(Some(&CefString::from(code.as_str())), None, 0);
}

/// Drop frames belonging to a browser that is going away, or a push would run against a dead frame.
fn forget_chrome_frames_of(browser: Option<&Browser>) {
    let Some(browser) = browser else {
        return;
    };
    let Ok(mut chrome) = chrome().lock() else {
        return;
    };
    let id = browser.identifier();
    let chrome = &mut *chrome;
    for slot in [&mut chrome.top, &mut chrome.bottom] {
        let is_gone = slot
            .as_ref()
            .and_then(|frame| frame.browser())
            .map(|frame_browser| frame_browser.identifier() == id)
            .unwrap_or(false);
        if is_gone {
            *slot = None;
        }
    }
}

fn bar_json() -> String {
    let Ok(bar) = bar().lock() else {
        return "{}".to_string();
    };
    format!(
        "{{\"url\":\"{}\",\"title\":\"{}\",\"mode\":\"{}\",\"keystring\":\"{}\",\"scroll\":\"{}\",\"tabindex\":\"{}\"}}",
        json_escape(&bar.url),
        json_escape(&bar.title),
        json_escape(if bar.mode.is_empty() { "normal" } else { &bar.mode }),
        json_escape(&bar.keystring),
        json_escape(&bar.scroll),
        json_escape(&bar.tabindex),
    )
}

/// The tab strip's payload. It is built by `BruState`, which owns the tabs, and cached here so a
/// push triggered by something other than a tab change still has it. Held as JSON rather than as a
/// borrow of the state so that a push never has to take the state mutex — pushes run on the UI
/// thread, and so does everything that holds that lock.
fn tabs() -> &'static Mutex<String> {
    static TABS: Mutex<String> = Mutex::new(String::new());
    &TABS
}

/// From the display handler, after `BruState` has updated the tab the browser belongs to.
pub fn set_tabs(json: String) {
    if let Ok(mut tabs) = tabs().lock() {
        *tabs = json;
    }
    push();
}

fn tabs_json() -> String {
    match tabs().lock() {
        Ok(tabs) if !tabs.is_empty() => tabs.clone(),
        _ => "{\"tabs\":[]}".to_string(),
    }
}

// -----------------------------------------------------------------------------------------------
// Renderer side
// -----------------------------------------------------------------------------------------------

/// The renderer half, one per render process. Nothing in this section may touch the state above:
/// it is the same binary but a different process, and the browser's state does not exist here.
fn renderer_router() -> &'static Arc<RendererSideRouter> {
    static ROUTER: OnceLock<Arc<RendererSideRouter>> = OnceLock::new();
    ROUTER.get_or_init(|| {
        log("renderer-side message router constructed");
        RendererSideRouter::new(MessageRouterConfig::default())
    })
}

pub fn renderer_on_context_created(
    browser: Option<&mut Browser>,
    frame: Option<&mut Frame>,
    context: Option<&mut V8Context>,
) {
    renderer_router().on_context_created(browser.cloned(), frame.cloned(), context.cloned());
}

pub fn renderer_on_context_released(
    browser: Option<&mut Browser>,
    frame: Option<&mut Frame>,
    context: Option<&mut V8Context>,
) {
    renderer_router().on_context_released(browser.cloned(), frame.cloned(), context.cloned());
}

pub fn renderer_on_process_message_received(
    browser: Option<&mut Browser>,
    frame: Option<&mut Frame>,
    source_process: ProcessId,
    message: Option<&mut ProcessMessage>,
) -> ::std::os::raw::c_int {
    // The scroll probe is answered here rather than through the router, because the router's
    // cefQuery is injected into every page and only `bru://` frames may use it — see the security
    // check above. A process message the browser sent is not the page asking for anything.
    if crate::scroll::renderer_on_query(frame.as_deref(), message.as_deref()) {
        return 1;
    }
    renderer_router().on_process_message_received(
        browser.cloned(),
        frame.cloned(),
        Some(source_process),
        message.cloned(),
    ) as ::std::os::raw::c_int
}

// -----------------------------------------------------------------------------------------------
// Small change: JSON by hand, and a debug switch
// -----------------------------------------------------------------------------------------------

/// Set `BRU_DEBUG_IPC=1` to trace the router. It is off by default because the plan's first
/// debugging question for a silent router is "was the renderer side ever constructed", and that
/// answer should be one environment variable away rather than a rebuild.
fn log(message: &str) {
    static ON: OnceLock<bool> = OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("BRU_DEBUG_IPC").is_some()) {
        eprintln!("bru[ipc,pid {}]: {message}", std::process::id());
    }
}

/// Read one string field out of a flat JSON object. The only producer is the chrome's own
/// `JSON.stringify`, so this does not need to be a parser — and a JSON dependency for six keys
/// would be a dependency to audit.
fn json_field(src: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let mut rest = src;
    loop {
        let at = rest.find(&needle)?;
        rest = &rest[at + needle.len()..];
        let after = rest.trim_start();
        let Some(after) = after.strip_prefix(':') else {
            continue;
        };
        let after = after.trim_start();
        let Some(after) = after.strip_prefix('"') else {
            return None;
        };
        return Some(json_unescape(after));
    }
}

fn json_unescape(src: &str) -> String {
    let mut out = String::new();
    let mut chars = src.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => break,
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some('b') => out.push('\u{8}'),
                Some('f') => out.push('\u{c}'),
                Some('u') => {
                    let hex: String = chars.by_ref().take(4).collect();
                    if let Some(c) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                        out.push(c);
                    }
                }
                Some(other) => out.push(other),
                None => break,
            },
            other => out.push(other),
        }
    }
    out
}

pub fn json_escape(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    for c in src.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // Valid in a JSON string but not in a JavaScript one, and this lands inside a script.
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}
