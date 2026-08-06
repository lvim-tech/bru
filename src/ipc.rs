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
    // --- src/editor.rs ----------------------------------------------------------------------
    // The answer to a question `editor.rs` asked the page — what the focused field holds. Same
    // shape as the scroll report above, and for the same reason: it is not a query the page could
    // ever have started, so it stays clear of the `bru://`-only cefQuery check below.
    if crate::editor::on_answer(message.as_deref()) {
        return 1;
    }
    // --- end src/editor.rs ------------------------------------------------------------------
    // `]]`'s answer: the page's links, collected by a script bru evaluated itself. Claimed here, so
    // the router never sees it and its `bru://`-only check has nothing to be exempted from.
    if crate::navigate::on_report(browser.as_deref(), message.as_deref()) {
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
        browser: Option<Browser>,
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

        // --- M12 --------------------------------------------------------------------------------
        // The one thing a web page is allowed to say, and only because bru injected the script that
        // says it. `chrome/hints.js` runs in the page's own world to see the page's elements, so it
        // cannot be a bru:// frame and cannot pass the check below. `hints::on_page_query` is what
        // makes that safe: it answers false unless a hint session bru itself started is open, the
        // query came from that session's browser, and it carries the token that session minted.
        if json_field(request, "type").as_deref() == Some("hints") {
            if crate::hints::on_page_query(browser.as_ref(), request) {
                succeed(&callback, "");
            } else {
                eprintln!("bru: refused a hint answer from {url:?}");
                fail(&callback, -6, "not an answer to a hint session bru started");
            }
            return true;
        }

        // --- src/caret.rs -----------------------------------------------------------------------
        // The second thing a web page is allowed to say, and again only because bru injected the
        // script that says it. `chrome/caret.js` runs in the page's own world to see the page's
        // document, so it cannot be a bru:// frame either. `caret::on_page_query` makes it safe the
        // same way: it answers false unless a request bru itself made is outstanding, the query came
        // from that request's browser, and it carries the token bru minted for it.
        if json_field(request, "type").as_deref() == Some("caret") {
            if crate::caret::on_page_query(browser.as_ref(), request) {
                succeed(&callback, "");
            } else {
                eprintln!("bru: refused a caret answer from {url:?}");
                fail(&callback, -7, "not an answer to a caret request bru made");
            }
            return true;
        }
        // --- end src/caret.rs -------------------------------------------------------------------

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
                        // The strip keeps the favicons it has been given rather than being handed
                        // them with every state push, so a strip that has just loaded — at startup,
                        // or after a theme reload — has to be given the ones already downloaded.
                        crate::favicon::push_all();
                        tabs_json()
                    }
                    "bottom" => {
                        chrome().lock().ok().map(|mut c| c.bottom = Some(frame));
                        // The command line cannot be driven before there is an input to drive, so
                        // its debug script starts here rather than in `on_context_initialized`.
                        start_cmdline_script();
                        crate::completers::start_script();
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
            // --- M9: the command line -----------------------------------------------------------
            // Three types, exactly as STAGE2-CONTRACTS.md specifies them. The `#cmdline` input is
            // the real editor for plain typing — see `cmdline::types_into_cmdline` — so this is
            // where Rust learns what it holds.
            // A click on the tab strip. The only pointer input bru accepts, and it is here rather
            // than on the key path because a mouse cannot cost the scrolling anything.
            //
            // It must not select the tab from inside this handler: selection focuses a browser
            // view, and CEF-NOTES trap 12 says a query handler is the wrong place for that — the
            // router holds a lock `on_before_browse` wants. So it is posted, like everything else
            // that acts on a browser from here.
            Some("tab-select") => {
                if let Some(index) = json_number_field(request, "index") {
                    crate::tabs::schedule_select(index as usize);
                }
                succeed(&callback, "");
            }
            Some("text-changed") => {
                let text = json_field(request, "text").unwrap_or_default();
                crate::cmdline::on_text_changed(&text, json_number_field(request, "cursor"));
                // The completion is derived from the command line rather than pushed into: every
                // push asks `completers::json` what the table is now. So this only has to push.
                push();
                succeed(&callback, "");
            }
            // The authoritative text, in answer to `bru.accept()`. Nothing else may run a command
            // line: the mirror above is one IPC hop behind and Enter can overtake it.
            Some("accept") => {
                crate::cmdline::on_accept(&json_field(request, "text").unwrap_or_default());
                succeed(&callback, "");
            }
            Some("cancel") => {
                crate::cmdline::on_cancel();
                succeed(&callback, "");
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
    search: String,
    /// `[dl 45%]` while a download is running, empty otherwise — `downloads::summary`. Pushed like
    /// the rest; the chrome has no element for it yet and ignores the key until it does.
    download: String,
}

fn bar() -> &'static Mutex<BarState> {
    static BAR: Mutex<BarState> = Mutex::new(BarState {
        url: String::new(),
        title: String::new(),
        mode: String::new(),
        keystring: String::new(),
        scroll: String::new(),
        tabindex: String::new(),
        search: String::new(),
        download: String::new(),
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
    // And onto the toplevel, which is the only place the compositor can read a window's name from.
    // Here rather than in the display handler because a tab *switch* also changes the title and
    // fires no display callback — both routes already come through this function.
    crate::window::set_title(&title);
    if let Ok(mut bar) = bar().lock() {
        bar.title = title;
    }
    push();
}

/// The current mode, spelled as qutebrowser spells it. The chrome colours the bar by it, and
/// `body.mode-command` is what reveals `#cmdline`.
pub fn set_mode(mode: String) {
    // Every route out of command mode passes through here — `mode-leave`, an accepted command, a
    // page focusing a field — so this is where the line is cleared and the page gets its focus
    // back. Hanging it off the mode change rather than off the `mode-leave` command is what keeps
    // the command line out of `exec.rs`.
    crate::cmdline::on_mode_changed(&mode);
    // A message and a command line share one cell, so opening the line takes the message's turn
    // away. Dropping it rather than letting the stylesheet hide it is what stops a message from
    // three seconds ago reappearing the moment `:` is cancelled.
    if mode == "command" {
        crate::message::clear();
    }
    if let Ok(mut bar) = bar().lock() {
        bar.mode = mode;
    }
    push();
}

/// The address of the tab that is showing, for `{url}` in a `cmd-set-text` — which is how `go` and
/// `gO` prefill the line with the current page.
pub fn current_url() -> String {
    bar().lock().map(|bar| bar.url.clone()).unwrap_or_default()
}

/// The title of the tab that is showing, for `yank title` and for `{title}` in a `yank inline`.
pub fn current_title() -> String {
    bar().lock().map(|bar| bar.title.clone()).unwrap_or_default()
}

/// src/clip.rs: one line of message on the status bar, or `""` to take it away.
///
/// Where qutebrowser's message area would be. The empty string is not a special case here — the
/// chrome hides `#message` when it holds nothing, the same rule every other status field follows —
/// and clearing it after a timeout is `clip.rs`'s business, not this file's.
/// One line of message on the status bar.
///
/// Two workstreams arrived at this at once: a bare string here, and `message.rs` with a level and a
/// three-second timeout. The richer one won and this is now its front door, so `clip.rs`'s "Yanked
/// URL to clipboard" keeps working and gains a level and an expiry it did not have.
pub fn set_message(message: String) {
    if message.is_empty() {
        crate::message::clear();
    } else {
        crate::message::info(&message);
    }
}

/// Push the bar again. The command line calls it after every edit it makes.
pub fn push_bar() {
    push();
}

/// Run one statement in the tab strip's frame.
///
/// The one caller is `favicon.rs`, and it is a separate call rather than another key in the pushed
/// state on purpose: an icon is a kilobyte of base64 that never changes once it has arrived, and
/// carrying every one of them in the object pushed on every keystring and scroll change would put
/// tens of kilobytes of JavaScript on paths that run constantly. This hands the strip one icon,
/// once, and the strip keeps it.
pub fn top_chrome_eval(code: &str) {
    let Ok(chrome) = chrome().lock() else {
        return;
    };
    if let Some(frame) = chrome.top.clone() {
        frame.execute_java_script(Some(&CefString::from(code)), None, 0);
    }
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

/// `Match [3/17]` from `find.rs`, or empty for no search. Chromium sends several updates per
/// search as it scans the page, so this is filtered the same way.
pub fn set_search_match(search: String) {
    let changed = match bar().lock() {
        Ok(mut bar) if bar.search != search => {
            bar.search = search;
            true
        }
        _ => false,
    };
    if changed {
        push();
    }
}

/// `[dl 45%]` from `downloads.rs`, or empty when nothing is running. Filtered like the two above:
/// `on_download_updated` arrives several times a second and the string it produces does not change
/// nearly that often.
pub fn set_download(download: String) {
    let changed = match bar().lock() {
        Ok(mut bar) if bar.download != download => {
            bar.download = download;
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

/// `pub(crate)` so a check can read the exact JSON the chrome is handed rather than assert about it.
pub(crate) fn bar_json() -> String {
    // Built before the lock is taken: `cmdline::json` reads the mode out of `BruState`, and a bar
    // lock held across that would order two mutexes against each other for no reason.
    let cmdline = crate::cmdline::json();
    // Derived from that line, and rebuilt only when it has moved — see `completers::json`. It is
    // asked here rather than pushed in so that a table can never be one edit behind the text it
    // is completing.
    let completion = crate::completers::json();
    // Outside the bar lock for the same reason as `cmdline` above: `message::json` takes its own.
    let message = crate::message::json();
    let Ok(bar) = bar().lock() else {
        return "{}".to_string();
    };
    format!(
        // Six workstreams have each added a key here, and every one is optional to the chrome,
        // which ignores a key it has no element for: `search` is the find handler's match count,
        // `download` a running download's progress, `message` one line with a level and its own
        // timeout, `cmdline` the command line's text and cursor, `completion` the table under it.
        "{{\"url\":\"{}\",\"title\":\"{}\",\"mode\":\"{}\",\"keystring\":\"{}\",\"scroll\":\"{}\",\"tabindex\":\"{}\",\"search\":\"{}\",\"download\":\"{}\",\"cmdline\":{cmdline},\"completion\":{completion},\"message\":{message}}}",
        json_escape(&bar.url),
        json_escape(&bar.title),
        json_escape(if bar.mode.is_empty() { "normal" } else { &bar.mode }),
        json_escape(&bar.keystring),
        json_escape(&bar.scroll),
        json_escape(&bar.tabindex),
        json_escape(&bar.search),
        json_escape(&bar.download),
    )
}

// -----------------------------------------------------------------------------------------------
// The bottom strip, addressed on its own
// -----------------------------------------------------------------------------------------------
//
// The command line lives in one of the two chrome browsers, and three things need to name *that*
// one: the focus calls, the `bru.accept()` round trip, and trap 11's exception in `keys.rs`, which
// must not widen to the tab strip. `BruState::is_chrome_browser` cannot tell them apart — it holds
// both identifiers in one list — but the frame that announced itself as `view: "bottom"` can.

fn bottom_frame() -> Option<Frame> {
    chrome().lock().ok().and_then(|chrome| chrome.bottom.clone())
}

/// The browser drawing the bottom strip, once it has announced itself.
pub fn bottom_chrome_browser() -> Option<Browser> {
    bottom_frame().and_then(|frame| frame.browser())
}

/// Whether a key that arrived at `identifier` arrived at the bottom strip. Used by trap 11's
/// exception, which must not apply to the tab strip.
pub fn is_bottom_chrome_browser(identifier: i32) -> bool {
    bottom_chrome_browser()
        .map(|browser| browser.identifier() == identifier)
        .unwrap_or(false)
}

/// What the bottom strip is showing. `bru://chrome/bottom.html` unless something has navigated it
/// away — which is exactly what trap 11 is about, and what the `<Ctrl-T>` check reads.
pub fn bottom_chrome_url() -> String {
    bottom_frame()
        .map(|frame| CefString::from(&frame.url()).to_string())
        .unwrap_or_default()
}

/// Give the bottom strip keyboard focus, so the `#cmdline` input receives what is typed.
///
/// Two levels are needed and both are here: CEF's, so the key is delivered to this browser at all,
/// and the DOM's, which the chrome does itself when the pushed state says `focus`.
pub fn focus_bottom_chrome() {
    if let Some(browser) = bottom_chrome_browser() {
        if let Some(host) = browser.host() {
            host.set_focus(1);
        }
    }
}

/// Ask the chrome to send one of its messages back. The only caller is `command-accept`, which
/// needs the text the DOM holds rather than the copy Rust has, one IPC hop behind.
pub fn ask_cmdline(what: &str) {
    let Some(frame) = bottom_frame() else {
        return;
    };
    let code = format!("window.bru && window.bru.{what} && window.bru.{what}();");
    frame.execute_java_script(Some(&CefString::from(code.as_str())), None, 0);
}

/// `--cmdline-script=…`, read once the bottom strip is up. See `cmdline::schedule_script`.
fn start_cmdline_script() {
    let Some(command_line) = command_line_get_global() else {
        return;
    };
    let script =
        CefString::from(&command_line.switch_value(Some(&CefString::from("cmdline-script"))))
            .to_string();
    if script.is_empty() {
        return;
    }
    let step_ms =
        CefString::from(&command_line.switch_value(Some(&CefString::from("cmdline-step-ms"))))
            .to_string()
            .parse::<i64>()
            .unwrap_or(700);
    crate::cmdline::schedule_script(&script, step_ms);
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
    // --- src/editor.rs ----------------------------------------------------------------------
    // The renderer half of the same channel: evaluate what the browser process asked for and send
    // the result back. Only the browser process can address the renderer, so a page cannot ask.
    if crate::editor::renderer_on_ask(frame.as_deref(), message.as_deref()) {
        return 1;
    }
    // --- end src/editor.rs ------------------------------------------------------------------
    // Same reasoning for `:navigate prev/next`: the collector runs in the page's world, and its
    // answer travels as a process message rather than as a query the page could have sent.
    if crate::navigate::renderer_on_query(frame.as_deref(), message.as_deref()) {
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

/// Read one *number* field. `cursor` is the only one, and it is a number rather than a string
/// because `selectionStart` is one — quoting it in the chrome to fit `json_field` would be the tail
/// wagging the dog.
fn json_number_field(src: &str, key: &str) -> Option<usize> {
    let needle = format!("\"{key}\"");
    let at = src.find(&needle)?;
    let after = src[at + needle.len()..].trim_start();
    let after = after.strip_prefix(':')?.trim_start();
    let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
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
