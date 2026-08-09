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
    // --- src/scrollbar.rs ---------------------------------------------------------------------
    // A renderer asking what the scrollbar settings are, because it was made after the last time
    // anybody said. Same shape and the same reason as the others here: not a query a page could
    // have started, so it never reaches the router's `bru://`-only check.
    if crate::scrollbar::on_ask(frame.as_deref(), message.as_deref()) {
        return 1;
    }
    // --- end src/scrollbar.rs -----------------------------------------------------------------
    // --- src/userstyles.rs ---------------------------------------------------------------------
    // The same question about the per-site stylesheets, and the same reason it is claimed here.
    if crate::userstyles::on_ask(frame.as_deref(), message.as_deref()) {
        return 1;
    }
    // --- end src/userstyles.rs -----------------------------------------------------------------
    // --- src/focus.rs -------------------------------------------------------------------------
    // A page moved its focus, and the renderer says whether what has it now is editable. Same
    // shape and the same reason as the three above: it is not a query a page could have started,
    // so it never reaches the router's `bru://`-only check.
    if crate::focus::on_report(browser.as_deref(), message.as_deref()) {
        return 1;
    }
    // --- end src/focus.rs ---------------------------------------------------------------------
    browser_router().on_process_message_received(
        browser.cloned(),
        frame.cloned(),
        source_process,
        message.cloned(),
    ) as ::std::os::raw::c_int
}

/// Forward from `LifeSpanHandler::on_before_close`.
pub fn on_before_close(browser: Option<&mut Browser>) {
    // --- src/csp.rs ---------------------------------------------------------------------------
    // Identifiers are reused, and the CSP answer is remembered against one — a new tab that took
    // this number back would inherit a bypass nothing had set on it.
    if let Some(browser) = browser.as_deref() {
        crate::csp::forget(browser.identifier());
    }
    // --- end src/csp.rs -----------------------------------------------------------------------
    // --- src/devtools.rs ------------------------------------------------------------------------
    // A docked inspector closed by its own button, not by `:devtools`. Nothing else hears about
    // that, and the view would stay in the window holding the space of a browser that is gone.
    if let Some(browser) = browser.as_deref() {
        let id = browser.identifier();
        if let Some(state) = crate::state::BruState::instance() {
            let window = state
                .lock()
                .expect("state mutex poisoned")
                .window_of_inspector(id);
            if let Some(window) = window {
                crate::devtools::undock(&state, window);
            }
        }
    }
    // --- end src/devtools.rs --------------------------------------------------------------------
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
            // --- the panel's height ---------------------------------------------------------
            // What `bottom.js` measured after it drew, which is now the only thing that sizes the
            // strip. See that file for the flicker this replaced: Rust computing the height from a
            // row count relaid the view out before the render had crossed to the renderer, so the
            // panel grew while the words in it were still the previous frame's.
            Some("height") => {
                // `json_number_field`, not `json_field`: `px` is a number in the JSON because
                // `offsetHeight` is one, and the string reader answers `None` for an unquoted
                // value — which arrived here as a silent 0 and left the panel at 24px.
                let px = json_number_field(request, "px").unwrap_or(0) as i32;
                let Some(window) = frame.and_then(|frame| frame.browser()).and_then(|browser| {
                    let mut browser = browser;
                    window_of(Some(&mut browser))
                }) else {
                    fail(&callback, -9, "a height from a strip that belongs to no window");
                    return true;
                };
                crate::completers::apply_height(window, px);
                succeed(&callback, "");
                return true;
            }
            // --- end the panel's height -------------------------------------------------------
            // --- src/devtools.rs: the divider -------------------------------------------------
            // A press, a move or a release on the six pixels between the pages and the docked
            // inspector. All three arrive here because CEF's Views delegates are never handed a
            // pointer event — see `chrome/divider.js` for why the strip is a page at all.
            //
            // Relayouting from inside a query handler is what trap 12 does **not** forbid: it names
            // creating browsers and starting navigations, and `prompt.rs::resize_bar` has been
            // changing a strip's height from a query handler since the command line was written.
            Some("divider") => {
                let phase = json_field(request, "phase").unwrap_or_default();
                // **Where the boundary ended up, in the expanded strip's own pixels — not a delta.**
                //
                // This comment used to say the field was a signed distance the hand had moved, and
                // it was, for the design that chased the pointer with a six-pixel strip. That design
                // was replaced (CEF-NOTES trap 26): the strip now grows to cover the whole shared
                // area for the duration of a drag and reports the line, which `divider.js` clamps to
                // `[min, max]` before sending and `min` is the protected page height. So the value
                // is never negative, and `json_number_field` — which cannot represent a negative,
                // see its own note — is the right reader rather than an accident.
                //
                // Said out loud because the stale sentence promised a property the reader could not
                // deliver, and the next person to add a genuinely signed field here would have
                // trusted it.
                let y = json_number_field(request, "y").unwrap_or(0) as i32;
                let Some(id) = browser.as_ref().map(|browser| browser.identifier()) else {
                    fail(&callback, -10, "a divider event with no browser behind it");
                    return true;
                };
                let Some(state) = crate::state::BruState::instance() else {
                    fail(&callback, -10, "a divider event before there is any state");
                    return true;
                };
                match crate::devtools::on_divider_query(&state, id, &phase, y) {
                    // The press is answered with the geometry the page draws its preview from; the
                    // release is answered with nothing, because there is nothing left to say.
                    Some(answer) => succeed(&callback, &answer),
                    None => fail(&callback, -10, "a divider event bru could not place"),
                }
                return true;
            }
            // --- end src/devtools.rs: the divider ---------------------------------------------
            Some("ready") => {
                let view = json_field(request, "view").unwrap_or_default();
                let Some(frame) = frame else {
                    fail(&callback, -3, "ready without a frame");
                    return true;
                };
                // Which window's strip this is. Asked of `BruState` rather than of the page,
                // because `BruChromeViewDelegate::on_browser_created` has already recorded it — the
                // strip's own URL is the same in every window and could never say. This takes the
                // state lock inside a query handler, which trap 12 allows: it creates no browser and
                // starts no navigation.
                let window = browser
                    .as_ref()
                    .map(|browser| browser.identifier())
                    .and_then(|id| {
                        crate::state::BruState::instance().and_then(|state| {
                            state.lock().ok().and_then(|state| state.window_of_browser(id))
                        })
                    });
                let Some(window) = window else {
                    fail(&callback, -8, "a chrome view that belongs to no window");
                    return true;
                };
                let response = match view.as_str() {
                    "top" => {
                        with_window(window, |entry| entry.frames.top = Some(frame));
                        // The strip keeps the favicons it has been given rather than being handed
                        // them with every state push, so a strip that has just loaded — at startup,
                        // or after a theme reload — has to be given the ones already downloaded.
                        crate::favicon::push_all();
                        with_window(window, |entry| tabs_json_of(&entry.tabs))
                            .unwrap_or_else(|| "{\"tabs\":[]}".to_string())
                    }
                    // --- src/window.rs: the panel ---------------------------------------
                    // Answered with the same state object the bar is given: one JSON is built per
                    // push and both documents read the keys they draw. See `panel.js`.
                    "panel" => {
                        with_window(window, |entry| entry.frames.panel = Some(frame));
                        bar_json_for(window)
                    }
                    // --- end src/window.rs: the panel -----------------------------------
                    "bottom" => {
                        with_window(window, |entry| entry.frames.bottom = Some(frame));
                        // The command line cannot be driven before there is an input to drive, so
                        // its debug script starts here rather than in `on_context_initialized`.
                        start_cmdline_script();
                        crate::completers::start_script();
                        bar_json_for(window)
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
                    // Which window's strip was clicked. A click is the one input that does not go
                    // through `keys.rs`, so nothing else has named the window; the browser id is
                    // carried into the posted task and resolved there, so this handler still takes
                    // no lock of bru's own.
                    crate::tabs::schedule_select(
                        browser.as_ref().map(|browser| browser.identifier()),
                        index as usize,
                    );
                }
                succeed(&callback, "");
            }
            Some("text-changed") => {
                let text = json_field(request, "text").unwrap_or_default();
                // Which window's command line is reporting. Not "the current window": these three
                // answers travel chrome → renderer → browser and can arrive after the focus has
                // moved, and window 1's typing must not be written into window 0's line.
                let Some(window) = window_of(browser.as_ref()) else {
                    fail(&callback, -9, "a command line that belongs to no window");
                    return true;
                };
                crate::cmdline::on_text_changed(
                    window,
                    &text,
                    json_number_field(request, "cursor"),
                );
                // The completion is derived from the command line rather than pushed into: every
                // push asks `completers::json` what the table is now. So this only has to push.
                push_for(window);
                succeed(&callback, "");
            }
            // The authoritative text, in answer to `bru.accept()`. Nothing else may run a command
            // line: the mirror above is one IPC hop behind and Enter can overtake it.
            Some("accept") => {
                let Some(window) = window_of(browser.as_ref()) else {
                    fail(&callback, -9, "a command line that belongs to no window");
                    return true;
                };
                crate::cmdline::on_accept(
                    window,
                    &json_field(request, "text").unwrap_or_default(),
                );
                succeed(&callback, "");
            }
            Some("cancel") => {
                let Some(window) = window_of(browser.as_ref()) else {
                    fail(&callback, -9, "a command line that belongs to no window");
                    return true;
                };
                crate::cmdline::on_cancel(window);
                succeed(&callback, "");
            }

            // --- src/prompt.rs ------------------------------------------------------------------
            // The mirror of the prompt's own `<input>`, exactly like `text-changed` above and for
            // the same reason: the DOM is where the letters land and Rust has to be told. It is
            // only ever a *mirror* — `prompt-accept` reads the DOM's text through `bru.accept()`'s
            // sibling rather than this, so the last character typed cannot be lost to a race with
            // the `<Return>` after it.
            Some("prompt-text") => {
                let Some(window) = window_of(browser.as_ref()) else {
                    fail(&callback, -9, "a prompt that belongs to no window");
                    return true;
                };
                crate::prompt::on_text_changed(
                    window,
                    &json_field(request, "text").unwrap_or_default(),
                    json_number_field(request, "cursor"),
                );
                succeed(&callback, "");
            }
            // The answer to `bru.promptAccept()`, and the only thing that may finish a prompt with
            // a line in it. Everything it leads to is posted — the callbacks a question closes can
            // start a navigation, and this is a query handler (trap 12).
            Some("prompt-accept") => {
                let Some(window) = window_of(browser.as_ref()) else {
                    fail(&callback, -9, "a prompt that belongs to no window");
                    return true;
                };
                crate::prompt::on_accept(
                    window,
                    &json_field(request, "text").unwrap_or_default(),
                );
                succeed(&callback, "");
            }
            // --- end src/prompt.rs --------------------------------------------------------------

            // --- src/cookies.rs -----------------------------------------------------------------
            // `bru://chrome/cookies` reading the cookie jar and deleting from it. It is below the
            // `bru://` check on purpose and is the first thing that check has ever had to refuse
            // for real: everything else here drives bru's own chrome, and this one empties the
            // browser's cookies. The handler answers the callback later — the cookie visitor is
            // asynchronous — which is what the message router's contract allows and why it takes
            // the callback rather than a response.
            Some("cookies") => {
                crate::cookies::on_page_query(browser.as_ref(), request, &callback);
            }
            // --- end src/cookies.rs -------------------------------------------------------------

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
    // --- src/window.rs: the panel ---------------------------------------------------------------
    /// The completion table and the prompt, in the one chrome view that changes size. See
    /// `chrome/panel.html` for the frames that made it a view of its own.
    panel: Option<Frame>,
    // --- end src/window.rs: the panel -----------------------------------------------------------
}

/// Everything one window's chrome is and shows.
///
/// It was three process-wide `static`s — the frames, the bar and the tab strip's JSON — and they
/// were `static` rather than fields of `BruState` because `RefGuard<T>` is `unsafe impl Send + Sync`
/// (CEF-NOTES), so a `Frame` may live in a `Mutex`. That is still true and still why they are here;
/// what changed is that there is now a row per window instead of one of each. Two windows sharing
/// one `BarState` pushed into each other's status line, and a `render` aimed at "the" bottom frame
/// reached whichever window loaded last.
struct Chrome {
    window: u32,
    frames: ChromeFrames,
    bar: BarState,
    /// The message number this window's command line took the cell from. See
    /// [`crate::message::sequence`] — while it equals the current one, this window draws no
    /// message, and every other window still draws it.
    message_taken: u64,
    /// The tab strip's payload, cached so a push triggered by something other than a tab change
    /// still has it. Held as JSON rather than as a borrow of the state so that a push never has to
    /// take the state mutex — pushes run on the UI thread, and so does everything holding that lock.
    tabs: String,
}

fn chrome() -> &'static Mutex<Vec<Chrome>> {
    static CHROME: Mutex<Vec<Chrome>> = Mutex::new(Vec::new());
    &CHROME
}

/// What one window's bottom bar shows. `mode`, `keystring` and `scroll` are part of the pushed
/// object from the start so the chrome renders against its final shape.
///
/// **`tabindex` is deliberately not a field here.** It was one, from the first commit that drew a
/// bar, and nothing ever wrote it: every other key on this struct has a `set_…` and that one had
/// none, so `<span id="tabindex">` stayed empty and bru drew no `[3/7]` where qutebrowser does. It
/// is a pure function of the window's own tab list — `bar_json_for` asks `tabindex_of` on every
/// push — because storing a copy means six call sites that each have to remember to update it, and
/// that is precisely the arrangement that left it blank.
#[derive(Default)]
struct BarState {
    url: String,
    title: String,
    mode: String,
    keystring: String,
    scroll: String,
    search: String,
    /// `[dl 45%]` while a download is running, empty otherwise — `downloads::summary`. Pushed like
    /// the rest; the chrome has no element for it yet and ignores the key until it does.
    download: String,
}

/// Which window a browser is in — for the chrome answers that arrive from a strip rather than from
/// a keypress, and therefore name their own window instead of relying on the focus.
///
/// Takes the state lock inside a query handler, which CEF-NOTES trap 12 allows: it creates no
/// browser and starts no navigation. The `ready` arm above asks the same question inline and
/// predates this.
fn window_of(browser: Option<&Browser>) -> Option<u32> {
    let id = browser?.identifier();
    crate::state::BruState::instance()
        .and_then(|state| state.lock().ok().and_then(|state| state.window_of_browser(id)))
}

/// The window a command with nothing else to go on acts against. `None` in the renderer process and
/// once the last window has closed.
fn current_window() -> Option<u32> {
    crate::state::BruState::instance()
        .and_then(|state| state.lock().ok().and_then(|state| state.current_window_id()))
}

/// Run `f` against one window's chrome, making the row if this is the first thing said about it.
/// The lock is held only for `f`, which must not call into CEF.
fn with_window<R>(window: u32, f: impl FnOnce(&mut Chrome) -> R) -> Option<R> {
    let mut chrome = chrome().lock().ok()?;
    if !chrome.iter().any(|entry| entry.window == window) {
        chrome.push(Chrome {
            window,
            frames: ChromeFrames::default(),
            bar: BarState::default(),
            // No message has been taken from a window that has just appeared, and `sequence` starts
            // at 0 — so this has to be a number that cannot be the current one. `u64::MAX` is the
            // only value `SEQUENCE` will never reach.
            message_taken: u64::MAX,
            tabs: String::new(),
        });
    }
    let entry = chrome.iter_mut().find(|entry| entry.window == window)?;
    Some(f(entry))
}

/// Drop a window's chrome. Called from `on_window_destroyed`; without it a push after a window has
/// gone would run a script in a dead renderer.
pub fn forget_window(window: u32) {
    if let Ok(mut chrome) = chrome().lock() {
        chrome.retain(|entry| entry.window != window);
    }
    // The command line that window was holding goes with it — there is one per window now, and a
    // closed window's half-typed text has nowhere left to be shown.
    crate::cmdline::forget_window(window);
// --- src/prompt.rs ---------------------------------------------------------
    // And its questions, which are cancelled rather than dropped: something is waiting on each of
    // them — a page, a login, a download — and a window closing must not leave it waiting for ever.
    crate::prompt::forget_window(window);
// --- end src/prompt.rs -----------------------------------------------------
}

/// From `DisplayHandler::on_address_change` on the main frame, for the window the tab is in.
pub fn set_url_for(window: u32, url: String) {
// --- plugin events ---------------------------------------------------------
    // `url-changed`, where the bar is told — which is `on_address_change` **and** a tab switch, the
    // two ways a window's address moves. The guard is "the bar did not already hold this": both
    // routes push unconditionally, and a plugin asked to be told when the address changed must not
    // be told again because something else pushed the same string.
    //
    // The whole block is behind the handler count, not just `fire`: the freshness test takes the
    // chrome mutex, and a bru with no plugins must not pay a lock per navigation for an answer
    // nobody wants.
    //
    // An **empty** address is not one. A tab switch pushes whatever the new tab has recorded, and a
    // tab whose page has not committed yet has recorded nothing; measured, that put a
    // `url-changed` with `url = ""` between the switch and the real address. A plugin asked to be
    // told the address changed is owed an address.
    if crate::events::handler_count(crate::events::Event::UrlChanged) != 0
        && !url.is_empty()
        && with_window(window, |entry| entry.bar.url != url).unwrap_or(false)
    {
        crate::events::fire(crate::events::Event::UrlChanged, Some(window), || {
            vec![("url", crate::lua::Arg::Text(url.clone()))]
        });
    }
// --- end plugin events -----------------------------------------------------
    with_window(window, |entry| entry.bar.url = url);
    push_for(window);
}

/// From `DisplayHandler::on_title_change`, for the window the tab is in.
pub fn set_title_for(window: u32, title: String) {
    // And onto that window's toplevel, which is the only place the compositor can read a window's
    // name from. Here rather than in the display handler because a tab *switch* also changes the
    // title and fires no display callback — both routes already come through this function.
    crate::window::set_title(window, &title);
    with_window(window, |entry| entry.bar.title = title);
    push_for(window);
}

/// One window's mode, spelled as qutebrowser spells it. The chrome colours the bar by it, and
/// `body.mode-command` is what reveals `#cmdline`.
///
/// Named for its window like `set_url_for`, `set_title_for` and `set_tabs_for`, and for the same
/// reason: bru keeps a `ModeManager` per window, so `insert` in window 1 is not a fact about
/// window 0 and must not be pushed into its bar.
pub fn set_mode_for(window: u32, mode: String) {
// --- plugin events ---------------------------------------------------------
    // `mode-changed`, with both ends of the transition.
    //
    // **It is not in `modes.rs`, and that is not where it could have been.** `ModeManager` is a
    // pure state machine over an enum — its own head says so — and it knows no window; the place
    // that has both ends *and* a window id is `state::BruState::apply`, which runs with the state
    // mutex held, and a Lua handler called under that lock is a deadlock waiting for the first
    // entry point that reaches back into `BruState`. This function is the funnel every mode change
    // in a window already passes through (`prompt.rs:416` says so in as many words), it is called
    // after the lock is released, and the mode the bar is holding *is* the mode being left. Same
    // ordering rule as the two blocks below it: the event is fired before anything is overwritten.
    //
    // A bar that has never been told a mode holds `""`; the window was in normal mode, which is
    // what bru starts every window in, so that is what `from` says.
    if crate::events::handler_count(crate::events::Event::ModeChanged) != 0 {
        let from = with_window(window, |entry| entry.bar.mode.clone()).unwrap_or_default();
        let from = if from.is_empty() { crate::modes::Mode::Normal.name().to_string() } else { from };
        if from != mode {
            crate::events::fire(crate::events::Event::ModeChanged, Some(window), || {
                vec![
                    ("from", crate::lua::Arg::Text(from)),
                    ("to", crate::lua::Arg::Text(mode.clone())),
                ]
            });
        }
    }
// --- end plugin events -----------------------------------------------------
    // Every route out of command mode passes through here — `mode-leave`, an accepted command, a
    // page focusing a field — so this is where the line is cleared and the page gets its focus
    // back. Hanging it off the mode change rather than off the `mode-leave` command is what keeps
    // the command line out of `exec.rs`.
    crate::cmdline::on_mode_changed(window, &mode);
// --- src/prompt.rs ---------------------------------------------------------
    // The same funnel, and for the prompt it is the whole of "a question can be left": whatever
    // takes the mode away from an open question cancels it. `PromptQueue._on_mode_left`.
    crate::prompt::on_mode_changed(window, &mode);
// --- end src/prompt.rs -----------------------------------------------------
    // A message and a command line share one cell, so opening the line takes the message's turn
    // away — **in this window**. This used to call `message::clear()`, which dropped the message
    // for the whole process, so `:` typed in one window emptied a message the other window was
    // showing. Remembering the number instead keeps the message alive everywhere else and still
    // stops the one from three seconds ago reappearing here the moment `:` is cancelled: a message
    // that arrives *after* the line was opened has a higher number and is not suppressed.
    if mode == "command" {
        let taken = crate::message::sequence();
        with_window(window, |entry| entry.message_taken = taken);
    }
    with_window(window, |entry| entry.bar.mode = mode);
    push_for(window);
// --- tabs and statusbar ----------------------------------------------------
    // `statusbar.show in-mode` shows the bar in every mode but normal, and `never` keeps it for as
    // long as the command line or a prompt holds the keyboard — both are answers about the mode, so
    // this is where they are asked. Cheap: it returns before it touches a view unless the answer
    // moved.
    crate::window::apply_chrome_layout(window);
// --- end tabs and statusbar ------------------------------------------------
}

/// The current window's mode. The convenience half of [`set_mode_for`], the way `set_tabs` is of
/// `set_tabs_for` — for the callers that are running a command in the window it was typed in.
pub fn set_mode(mode: String) {
    if let Some(window) = current_window() {
        set_mode_for(window, mode);
    }
}

/// The address of the tab that is showing, for `{url}` in a `cmd-set-text` — which is how `go` and
/// `gO` prefill the line with the current page.
pub fn current_url() -> String {
    let Some(window) = current_window() else {
        return String::new();
    };
    with_window(window, |entry| entry.bar.url.clone()).unwrap_or_default()
}

/// The title of the tab that is showing, for `yank title` and for `{title}` in a `yank inline`.
pub fn current_title() -> String {
    let Some(window) = current_window() else {
        return String::new();
    };
    with_window(window, |entry| entry.bar.title.clone()).unwrap_or_default()
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

// --- src/prompt.rs ---------------------------------------------------------
/// The same for a named window. A question queued behind another one belongs to the window it was
/// raised in, which is not always the window in front — a page in a background window can raise a
/// `confirm()` while the user is typing in this one.
pub fn push_bar_for(window: u32) {
    push_for(window);
}
// --- end src/prompt.rs -----------------------------------------------------

/// The same for every window. A global setting the bar draws — `statusbar.mode.style` — changes what
/// all of them show at once, and a window that is not in front has no reason to wait for a keypress
/// to catch up.
pub fn push_bar_everywhere() {
    let windows: Vec<u32> = match chrome().lock() {
        Ok(chrome) => chrome.iter().map(|entry| entry.window).collect(),
        Err(_) => return,
    };
    for window in windows {
        push_for(window);
    }
}

/// Run one statement in the tab strip's frame.
///
/// The one caller is `favicon.rs`, and it is a separate call rather than another key in the pushed
/// state on purpose: an icon is a kilobyte of base64 that never changes once it has arrived, and
/// carrying every one of them in the object pushed on every keystring and scroll change would put
/// tens of kilobytes of JavaScript on paths that run constantly. This hands the strip one icon,
/// once, and the strip keeps it.
/// Every window's tab strip, not only the current one: a favicon is keyed by the origin of the page
/// it belongs to, and the same site open in two windows wants its icon in both strips.
pub fn top_chrome_eval(code: &str) {
    let frames: Vec<Frame> = match chrome().lock() {
        Ok(chrome) => chrome
            .iter()
            .filter_map(|entry| entry.frames.top.clone())
            .collect(),
        Err(_) => return,
    };
    for frame in frames {
        frame.execute_java_script(Some(&CefString::from(code)), None, 0);
    }
}

/// The pending key chain and count — `g` after `g`, `3` after `3`, empty once something ran. This
/// is qutebrowser's keystring widget, and it is what makes a half-typed `gg` visible.
pub fn set_keystring(keystring: String) {
    // Every keypress reaches here, and most leave the string as it was. Pushing regardless would
    // run a script in the chrome renderer on every `j` — on the one path this project exists to
    // keep fast. So the change is decided under the lock and the push happens outside it.
    set_bar_field(|bar| &mut bar.keystring, keystring);
}

/// The same for a named window — for a push that arrives knowing which window it is about rather
/// than which one is in front.
///
/// `hints.rs` and `caret.rs` both need it now that a session belongs to one window: `;R` deliberately
/// lets a key arrive at a window that is not the hinting one, and a caret report can land while
/// another window is current. Without it the chain typed into window 0's labels was written into
/// whichever bar happened to be showing.
pub fn set_keystring_for(window: u32, keystring: String) {
    set_bar_field_for(window, |bar| &mut bar.keystring, keystring);
}

/// One field of the current window's bar, pushed only if it moved. The four callers below are all
/// on paths that fire many times a second.
fn set_bar_field(field: impl Fn(&mut BarState) -> &mut String, value: String) {
    let Some(window) = current_window() else {
        return;
    };
    set_bar_field_for(window, field, value);
}

/// One field of a named window's bar, pushed only if it moved.
fn set_bar_field_for(
    window: u32,
    field: impl Fn(&mut BarState) -> &mut String,
    value: String,
) {
    let changed = with_window(window, |entry| {
        let slot = field(&mut entry.bar);
        if *slot == value {
            false
        } else {
            *slot = value;
            true
        }
    });
    if changed == Some(true) {
        push_for(window);
    }
}

/// The scroll percentage, spelled as qutebrowser's percentage widget spells it: `[top]`, `[42%]`,
/// `[bot]`. Built by `scroll.rs` from what the page reports, and pushed only when it changes — a
/// held `j` reaches this on every settled position and a push each time would run a script in the
/// chrome renderer for a string that is already right.
pub fn set_scroll(scroll: String) {
    set_bar_field(|bar| &mut bar.scroll, scroll);
}

/// `Match [3/17]` from `find.rs`, or empty for no search. Chromium sends several updates per
/// search as it scans the page, so this is filtered the same way.
pub fn set_search_match(search: String) {
    set_bar_field(|bar| &mut bar.search, search);
}

/// The same for a named window. Caret mode borrows this cell to show the size of the selection, and
/// a caret session belongs to one window — a report from a background window's page must not write
/// into the bar of the window in front.
pub fn set_search_match_for(window: u32, search: String) {
    set_bar_field_for(window, |bar| &mut bar.search, search);
}

/// `[dl 45%]` from `downloads.rs`, or empty when nothing is running. Filtered like the two above:
/// `on_download_updated` arrives several times a second and the string it produces does not change
/// nearly that often.
pub fn set_download(download: String) {
    set_bar_field(|bar| &mut bar.download, download);
}

/// Push the current window's state into whichever of its chrome frames have announced themselves.
fn push() {
    if let Some(window) = current_window() {
        push_for(window);
    }
}

/// The same for a named window. Every push is aimed at one window: a background window's bar keeps
/// what it was last shown, which is what its own tab is doing.
fn push_for(window: u32) {
    let (top, bottom, panel, tabs) = match chrome().lock() {
        Ok(chrome) => match chrome.iter().find(|entry| entry.window == window) {
            Some(entry) => (
                entry.frames.top.clone(),
                entry.frames.bottom.clone(),
                entry.frames.panel.clone(),
                entry.tabs.clone(),
            ),
            None => return,
        },
        Err(_) => return,
    };

    if let Some(frame) = top {
        render(&frame, &tabs_json_of(&tabs));
    }
    if let Some(frame) = bottom {
        // `BRU_DEBUG_IPC=1` traced only chrome → Rust, which is the half a page can be asked about
        // from its own console. This is the other half, and it is the only way to read the exact
        // object the bar was handed without a screenshot.
        let bar = bar_json_for(window);
        log(&format!("push to window {window}: {bar}"));
        render(&frame, &bar);
        // --- src/window.rs: the panel -----------------------------------------------------
        // **The same string, into the other document.** The completion table and the prompt are
        // drawn in a view of their own now; `panel.js` reads the two keys it draws and ignores the
        // rest, and `bottom.js` does the opposite. Built once rather than twice because a second
        // serialisation would be a second thing to keep in step, and neither document is confused
        // by a key it has no element for.
        if let Some(frame) = panel {
            render(&frame, &bar);
        }
        // --- end src/window.rs: the panel -------------------------------------------------
    }
}

fn render(frame: &Frame, state_json: &str) {
    // Guarded on `window.bru`: a push can land between navigation start and the script running.
    let code = format!("window.bru && window.bru.render({state_json});");
    frame.execute_java_script(Some(&CefString::from(code.as_str())), None, 0);
}

/// Drop frames belonging to a browser that is going away, or a push would run against a dead frame.
/// Every window is searched: the browser that died names no window by itself.
fn forget_chrome_frames_of(browser: Option<&Browser>) {
    let Some(browser) = browser else {
        return;
    };
    let Ok(mut chrome) = chrome().lock() else {
        return;
    };
    let id = browser.identifier();
    for entry in chrome.iter_mut() {
        for slot in [&mut entry.frames.top, &mut entry.frames.bottom] {
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
}

/// `pub(crate)` so a check can read the exact JSON the chrome is handed rather than assert about it.
pub(crate) fn bar_json() -> String {
    match current_window() {
        Some(window) => bar_json_for(window),
        None => "{}".to_string(),
    }
}

fn bar_json_for(window: u32) -> String {
    // Built before the lock is taken: `cmdline::json_for` reads the mode out of `BruState`, and a
    // bar lock held across that would order two mutexes against each other for no reason.
    // Both are asked about **this** window rather than the current one: a push aimed at a
    // background bar used to hand it the foreground window's half-typed line, and to collapse the
    // foreground window's completion table on the way past.
    let cmdline = crate::cmdline::json_for(window);
    // Derived from that line, and rebuilt only when it has moved — see `completers::json_for`. It
    // is asked here rather than pushed in so that a table can never be one edit behind the text it
    // is completing.
    let completion = crate::completers::json_for(window);
// --- src/prompt.rs ---------------------------------------------------------
    // The open question, or `null`. Outside the bar lock like the two above, and it sets this
    // window's *other* strip-height slot on the way past — see `window::PROMPT_HEIGHTS`.
    let prompt = crate::prompt::json_for(window);
// --- end src/prompt.rs -----------------------------------------------------
    // Outside the bar lock for the same reason as `cmdline` above: `message::json` takes its own.
    let message = if with_window(window, |entry| entry.message_taken)
        == Some(crate::message::sequence())
    {
        "null".to_string()
    } else {
        crate::message::json()
    };
    // Which tab of how many, asked of `BruState` rather than kept on `BarState` — see the note
    // there. Outside `with_window` for the same reason as the three above: the state mutex must
    // never be taken while the chrome mutex is held.
    let tabindex = tabindex_of(window);
    // `statusbar.mode.style` — `full` draws the mode's name, `short` its first letter. Read here
    // rather than kept on `BarState` for the same reason as `tabindex`: it is a pure function of a
    // setting, and a copy is a copy somebody has to remember to update.
    let mode_style = crate::settings::value_of("statusbar.mode.style").unwrap_or_default();
    // --- src/settings.rs: `statusbar.mode.labels`, already JSON --------------------------------
    let mode_labels = crate::settings::mode_labels_json();
// --- tabs and statusbar ----------------------------------------------------
    // `statusbar.widgets` — which fields the status line draws and in what order, already a JSON
    // array. Read here rather than kept on `BarState` for `tabindex`'s reason: it is a pure
    // function of a setting, and a copy is a copy somebody has to remember to update.
    let widgets = crate::settings::widgets_json();
// --- end tabs and statusbar ------------------------------------------------
    with_window(window, |entry| {
        let bar = &entry.bar;
        format!(
            // Six workstreams have each added a key here, and every one is optional to the chrome,
            // which ignores a key it has no element for: `search` is the find handler's match count,
            // `download` a running download's progress, `message` one line with a level and its own
            // timeout, `cmdline` the command line's text and cursor, `completion` the table under it.
            "{{\"url\":\"{}\",\"title\":\"{}\",\"mode\":\"{}\",\"keystring\":\"{}\",\"scroll\":\"{}\",\"tabindex\":\"{}\",\"modestyle\":\"{}\",\"modelabels\":{mode_labels},\"widgets\":{widgets},\"search\":\"{}\",\"download\":\"{}\",\"cmdline\":{cmdline},\"completion\":{completion},\"prompt\":{prompt},\"message\":{message}}}",
            json_escape(&bar.url),
            json_escape(&bar.title),
            json_escape(if bar.mode.is_empty() { "normal" } else { &bar.mode }),
            json_escape(&bar.keystring),
            json_escape(&bar.scroll),
            json_escape(&tabindex),
            json_escape(&mode_style),
            json_escape(&bar.search),
            json_escape(&bar.download),
        )
    })
    .unwrap_or_else(|| "{}".to_string())
}

/// `[3/7]` — which tab of how many is showing in one window, spelled as qutebrowser's tabindex
/// widget spells it: `'[{}/{}]'.format(current + 1, count)`, `statusbar/tabindex.py:19`. It is in
/// `statusbar.widgets`' default list (`configdata.yml:2212`), which is the order `bottom.html` lays
/// the spans out in.
///
/// **Named per window, never "the current one".** Every window draws its own bar, and a background
/// window's `[2/4]` is as true as the front one's; asking about the current window here would make
/// both windows show whichever was focused last. It is the same reason `set_url_for` and
/// `set_title_for` take a window rather than assume one.
///
/// Empty for a window with no tabs — the moment between `window::create(FirstTab::None)` and the
/// tab arriving, which `gD` goes through. `#statusline > span:empty` is `display: none`, so the bar
/// shows nothing there rather than a `[1/0]` that is true of nothing.
fn tabindex_of(window: u32) -> String {
    let Some(state) = crate::state::BruState::instance() else {
        return String::new();
    };
    let Ok(state) = state.lock() else {
        return String::new();
    };
    match state.tab_count_in(window) {
        0 => String::new(),
        count => format!("[{}/{}]", state.active_tab_in(window) + 1, count),
    }
}

// -----------------------------------------------------------------------------------------------
// The bottom strip, addressed on its own
// -----------------------------------------------------------------------------------------------
//
// The command line lives in one of the two chrome browsers, and three things need to name *that*
// one: the focus calls, the `bru.accept()` round trip, and trap 11's exception in `keys.rs`, which
// must not widen to the tab strip. `BruState::is_chrome_browser` cannot tell them apart — it holds
// both identifiers in one list — but the frame that announced itself as `view: "bottom"` can.

/// A named window's bottom frame. There is a `ModeManager` per window, so two windows can be in
/// command mode at once and every call that drives a command line has to say which one it means.
// --- src/chrome.rs: fonts --------------------------------------------------
/// Reload both strips of every window, so a stylesheet bru **serves** is fetched again.
///
/// The values pushed as JSON take effect on the next push; `user.css` does not, because a browser
/// reads a stylesheet when it loads the document and not when a setting moves. So the one thing that
/// applies a font change without a restart is a reload of the two documents that link it.
///
/// Cheap, and it is not the shape it looks like: these are `bru://` documents served from memory, no
/// network is touched, and each strip re-announces itself with a `ready` query on load — which is
/// what pulls its state back. So the bar is repopulated by the handshake it already has rather than
/// by anything added here.
///
/// The four generated pages need none of this: they are built per request, so opening
/// `bru://chrome/help` again is already a fresh fetch of `user.css`.
pub fn reload_chrome_everywhere() {
    debug_assert_ne!(currently_on(ThreadId::UI), 0);
    let Some(state) = crate::state::BruState::instance() else {
        return;
    };
    let windows: Vec<u32> = {
        let Ok(state) = state.lock() else {
            return;
        };
        state.window_ids()
    };
    for window in windows {
        for frame in [
            bottom_frame_for(window),
            top_frame_for(window),
            // The panel reads `user.css` too — `--completion-max-h` is `completion.height` — and it
            // was left out of this list when it was split off, so `:set completion.height` changed
            // a file the one document that reads it never re-fetched.
            panel_frame_for(window),
        ]
        .into_iter()
        .flatten()
        {
            if let Some(browser) = frame.browser() {
                // **`reload_ignore_cache`, not `reload`.** The stylesheet this exists to re-fetch is
                // served by bru itself with no cache headers, so Chromium keeps it — a plain reload
                // re-ran the document against the bytes it already had, and `:set completion.height
                // 150` left the panel capped at 300. Nothing here is a network fetch: all three
                // documents and every asset they name come out of the binary.
                browser.reload_ignore_cache();
            }
        }
    }
}

/// **The theme moved: re-apply it to the chrome AND to every page.**
///
/// `reload_chrome_everywhere` above is half of applying a theme, and for a long time it was the
/// whole of it. A page carries two pieces of the theme that its own reload never touches:
///
/// - the per-site stylesheet, which has `theme.css` prepended by `userstyles::css_for` so that a
///   rule written for a site can say `var(--bg)`;
/// - the page scrollbar, whose two colours `scrollbar::rules` resolves **out of** `theme.css`
///   before handing them over, because a page never loads that file.
///
/// Both are composed when the document loads. So a theme switch repainted the strips and left every
/// page wearing the old colours until it was reloaded by hand — measured 2026-08-08 with themer's
/// own path: `--bg` still `#292f33` on a page whose theme file had just been rewritten to Gruvbox,
/// with bru's log line saying it had noticed the rewrite.
///
/// **Three callers, and that is why this exists rather than two more lines at each.** The file
/// changing under a running browser, `:colorscheme --reload` by hand, and `:set colors.*`. The
/// first was fixed alone first; the other two would have been found later, one report at a time.
///
/// Nothing here reloads a page: both halves hand new CSS to a keeper that is already in it, so a
/// half-filled form survives a theme switch. Verified by marking the page and finding the mark
/// after.
pub fn reapply_theme_everywhere() {
    reload_chrome_everywhere();
    crate::userstyles::repaint();
    crate::scrollbar::push_rules();
}
// --- end src/chrome.rs: fonts ----------------------------------------------

/// The top strip's frame, [`bottom_frame_for`]'s twin. Both are recorded by the `ready` handshake.
fn top_frame_for(window: u32) -> Option<Frame> {
    let chrome = chrome().lock().ok()?;
    chrome
        .iter()
        .find(|entry| entry.window == window)
        .and_then(|entry| entry.frames.top.clone())
}

// --- src/window.rs: the panel -------------------------------------------------------------------
/// The panel's frame — the completion table and the prompt.
fn panel_frame_for(window: u32) -> Option<Frame> {
    let chrome = chrome().lock().ok()?;
    chrome
        .iter()
        .find(|entry| entry.window == window)
        .and_then(|entry| entry.frames.panel.clone())
}
// --- end src/window.rs: the panel ---------------------------------------------------------------

/// The browser drawing the panel, for the code that has to resize its view.
pub fn panel_chrome_browser_for(window: u32) -> Option<Browser> {
    panel_frame_for(window).and_then(|frame| frame.browser())
}

/// Give the panel keyboard focus, so the prompt's own `<input>` receives what is typed.
///
/// The sibling of [`focus_bottom_chrome`], and the two are not interchangeable: `#cmdline` is in
/// `bottom.html` and `#prompt` is in `panel.html`. Focusing the wrong one delivers every key to a
/// document with no field in it, which looks exactly like a browser that has stopped responding.
pub fn focus_panel_chrome(window: u32) {
    if let Some(browser) = panel_chrome_browser_for(window) {
        if let Some(host) = browser.host() {
            host.set_focus(1);
        }
    }
}

fn bottom_frame_for(window: u32) -> Option<Frame> {
    let chrome = chrome().lock().ok()?;
    chrome
        .iter()
        .find(|entry| entry.window == window)
        .and_then(|entry| entry.frames.bottom.clone())
}

/// The current window's bottom frame — for the callers reached from a command typed in it.
fn bottom_frame() -> Option<Frame> {
    bottom_frame_for(current_window()?)
}

/// The browser drawing the bottom strip, once it has announced itself.
pub fn bottom_chrome_browser() -> Option<Browser> {
    bottom_frame().and_then(|frame| frame.browser())
}

/// The same for a named window.
pub fn bottom_chrome_browser_for(window: u32) -> Option<Browser> {
    bottom_frame_for(window).and_then(|frame| frame.browser())
}

/// Whether a key that arrived at `identifier` arrived at *a* bottom strip — any window's.
///
/// Used by trap 11's exception, which must not apply to the tab strip. It asks about every window
/// rather than the current one on purpose: `keys.rs` has already made the key's own window current,
/// but a key that raced a window switch would otherwise be swallowed instead of typed.
pub fn is_bottom_chrome_browser(identifier: i32) -> bool {
    let Ok(chrome) = chrome().lock() else {
        return false;
    };
    chrome.iter().any(|entry| {
        entry
            .frames
            .bottom
            .as_ref()
            .and_then(|frame| frame.browser())
            .map(|browser| browser.identifier() == identifier)
            .unwrap_or(false)
    })
}

/// Whether `identifier` is a window's **panel** browser — the view holding the completion table and
/// the prompt's own `<input>`.
///
/// The sibling of [`is_bottom_chrome_browser`], and the two answer for two different documents:
/// `#cmdline` is in `bottom.html`, `#prompt` is in `panel.html`. Asking the wrong one is how every
/// key typed at a file prompt came to be swallowed — the key path forwarded letters only to the
/// bar, which no longer holds the field they were meant for.
pub fn is_panel_chrome_browser(identifier: i32) -> bool {
    let Ok(chrome) = chrome().lock() else {
        return false;
    };
    chrome.iter().any(|entry| {
        entry
            .frames
            .panel
            .as_ref()
            .and_then(|frame| frame.browser())
            .map(|browser| browser.identifier() == identifier)
            .unwrap_or(false)
    })
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
pub fn focus_bottom_chrome(window: u32) {
    if let Some(browser) = bottom_chrome_browser_for(window) {
        if let Some(host) = browser.host() {
            host.set_focus(1);
        }
    }
}

/// Ask a window's chrome to send one of its messages back. The only caller is `command-accept`,
/// which needs the text the DOM holds rather than the copy Rust has, one IPC hop behind.
pub fn ask_cmdline(window: u32, what: &str) {
    let Some(frame) = bottom_frame_for(window) else {
        return;
    };
    let code = format!("window.bru && window.bru.{what} && window.bru.{what}();");
    frame.execute_java_script(Some(&CefString::from(code.as_str())), None, 0);
}

// --- src/prompt.rs ---------------------------------------------------------
/// The same for the prompt's own `<input>`. `ask_cmdline` under the name of its caller: they are
/// two different inputs in the one strip, and a call site that read `ask_cmdline` while finishing a
/// prompt would be a line to misread later.
pub fn ask_prompt(window: u32, what: &str) {
    // **The panel's frame, not the bar's.** `#prompt` moved into `panel.html` with the completion
    // table when the bar was made a view that never resizes, so `ask_cmdline` — which is the bar's
    // by name and by target — no longer reaches the input this asks about.
    let Some(frame) = panel_frame_for(window) else {
        return;
    };
    let code = format!("window.bru && window.bru.{what} && window.bru.{what}();");
    frame.execute_java_script(Some(&CefString::from(code.as_str())), None, 0);
}
// --- end src/prompt.rs -----------------------------------------------------

/// `--cmdline-script=…`, read once the bottom strip is up. See `cmdline::schedule_script`.
///
/// **Once for the process, not once per strip.** This is called from the `ready` answer of a bottom
/// strip, and there is one of those per window — so a run that opens a second window scheduled the
/// whole script a second time and every step ran twice. Measured 2026-08-06: `key:win1:g` delivered
/// two `g`s, which completed `gg` on its own and made a two-window key-chain check report the very
/// bug it was written to disprove. The script is one script; the strip that happens to come up first
/// starts it.
fn start_cmdline_script() {
    static STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if STARTED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return;
    }
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

/// From the display handler, after `BruState` has updated the tab the browser belongs to — for the
/// window that tab is in.
pub fn set_tabs_for(window: u32, json: String) {
    with_window(window, |entry| entry.tabs = json);
    push_for(window);
// --- tabs and statusbar ----------------------------------------------------
    // `tabs.show multiple` counts tabs, and this is the one funnel every change to a window's tab
    // list goes through. It returns without touching a view when the answer has not moved.
    crate::window::apply_chrome_layout(window);
// --- end tabs and statusbar ------------------------------------------------
}

/// The current window's strip, for callers that act on it by definition.
pub fn set_tabs(json: String) {
    if let Some(window) = current_window() {
        set_tabs_for(window, json);
    }
}

fn tabs_json_of(cached: &str) -> String {
    if cached.is_empty() {
        "{\"tabs\":[]}".to_string()
    } else {
        cached.to_string()
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
    // --- src/focus.rs -------------------------------------------------------------------------
    // "bru has just clicked — what has the focus now?" Only the browser process can address this
    // renderer, so a page cannot ask it, and it never reaches the router's `bru://`-only check.
    if crate::focus::renderer_on_ask(frame.as_deref(), message.as_deref()) {
        return 1;
    }
    // --- end src/focus.rs ---------------------------------------------------------------------
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

/// Read one *number* field. Two use it — `cursor`, because `selectionStart` is a number, and `px`,
/// because `offsetHeight` is. Quoting either in the chrome to fit [`json_field`] would be the tail
/// wagging the dog, and reading one *with* `json_field` is a silent zero: it answers `None` for an
/// unquoted value, which is how the completion panel spent an afternoon 24 pixels tall.
/// A non-negative number out of a flat JSON object.
///
/// **It cannot represent a negative, and every caller must be a field that has none.** The digits
/// are taken with `take_while(is_ascii_digit)`, so a leading `-` yields an empty string and the
/// whole field reads `None` — which each caller turns into `0`. That is silent, and it is why this
/// note exists: the four fields read through it are `px` (`offsetHeight`), `cursor`
/// (`selectionStart`), `index` (an array index) and the divider's `y` (a boundary clamped to at
/// least the protected page height), and none of the four can be below zero. A field that can needs
/// a reader that says so rather than this one.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The reader's contract, pinned: it is for fields that cannot be negative.
    ///
    /// A `-40` does not parse as `-40` here, it parses as nothing — and every caller turns nothing
    /// into `0`, silently. That is safe only because all four callers read a quantity with no
    /// negative half, and this test is what fails the day somebody points it at a field that has
    /// one. The divider's `y` used to be documented as signed and was not; see the comment there.
    #[test]
    fn the_number_reader_has_no_negative_half() {
        assert_eq!(json_number_field("{\"y\":40}", "y"), Some(40));
        assert_eq!(json_number_field("{\"y\": 40 }", "y"), Some(40));
        assert_eq!(
            json_number_field("{\"y\":-40}", "y"),
            None,
            "a negative must read as absent, not as some other number"
        );
        assert_eq!(json_number_field("{\"y\":\"40\"}", "y"), None);
        assert_eq!(json_number_field("{\"x\":40}", "y"), None);
        // A fractional value keeps its whole part, which is what the callers want from a coordinate
        // a scaled display made fractional.
        assert_eq!(json_number_field("{\"y\":40.5}", "y"), Some(40));
    }
}
