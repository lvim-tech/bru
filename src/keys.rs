//! Keyboard handling: a CEF key event in, a [`crate::commands::Command`] out, and one call to the
//! dispatcher.
//!
//! What a command then *does* lives in `src/exec.rs`, which is the only place a command becomes an
//! action. This file translates and routes; it decides nothing about any individual command.

use cef::*;
use std::sync::{Arc, Mutex};

use crate::state::BruState;

/// The key code of a press whose RAWKEYDOWN bru swallowed, kept until that press is over.
///
/// One key press delivers RAWKEYDOWN, KEYDOWN, CHAR and KEYUP, in that order, and CEF sends each to
/// whichever view holds focus when it is dispatched. bru decides on the RAWKEYDOWN; the other three
/// carry no decision and must not be allowed to act as input somewhere else. `i32::MIN` is "no press
/// is pending" — a real `windows_key_code` is never that.
///
/// Process-wide rather than per-handler because a press is: every key event arrives on the UI
/// thread, one at a time, so there is only ever one press in flight.
static PRESS_TO_SWALLOW: std::sync::atomic::AtomicI32 =
    std::sync::atomic::AtomicI32::new(i32::MIN);

/// Record on a RAWKEYDOWN whether the rest of this press should be swallowed too, and pass the
/// verdict straight back so this can wrap a `return`.
fn remember(event: &KeyEvent, verdict: ::std::os::raw::c_int) -> ::std::os::raw::c_int {
    let code = if verdict == 1 { event.windows_key_code } else { i32::MIN };
    PRESS_TO_SWALLOW.store(code, std::sync::atomic::Ordering::Relaxed);
    verdict
}

/// KEYDOWN, CHAR and KEYUP: 1 when they belong to a press whose RAWKEYDOWN was swallowed.
///
/// KEYUP is let through and ends the press either way — a swallowed key-up can leave a page holding
/// a modifier down that was never released. CHAR ends it too, because it is the last event that
/// could type anything and holding the code past it would swallow the *next* press of the same key.
fn swallow_rest_of_press(event: &KeyEvent) -> ::std::os::raw::c_int {
    use std::sync::atomic::Ordering::Relaxed;
    if std::env::var_os("BRU_DEBUG_KEYS").is_some() {
        eprintln!(
            "bru[keys]: {:?} code={} char={} pending={}",
            event.type_,
            event.windows_key_code,
            event.character,
            PRESS_TO_SWALLOW.load(Relaxed),
        );
    }
    let pending = PRESS_TO_SWALLOW.load(Relaxed) != i32::MIN;
    match event.type_ {
        // The character. **Not matched on the key code.** The first version of this paired the four
        // events by `windows_key_code`; it passed its own harness and the doubled colon survived it
        // on the keyboard, which means the CHAR's code is not the RAWKEYDOWN's. *Why* it differs was
        // not measured — the header promises only that the field is "used by the DOM specification"
        // — and this deliberately no longer depends on the answer. The harness agreed with the wrong
        // fix because `inject_key` set one code on all four events, so it was reproducing the
        // assumption rather than the keyboard.
        //
        // One swallowed press consumes one character. Which key produced it is not a question this
        // has to answer, and asking it was the whole of the first mistake.
        KeyEventType::CHAR => {
            PRESS_TO_SWALLOW.store(i32::MIN, Relaxed);
            pending as ::std::os::raw::c_int
        }
        // Key-up ends the press and is always let through: swallowing it can leave a page holding a
        // modifier that was never released.
        KeyEventType::KEYUP => {
            PRESS_TO_SWALLOW.store(i32::MIN, Relaxed);
            0
        }
        // KEYDOWN still carries the physical key, so it can be matched properly.
        _ => (pending && PRESS_TO_SWALLOW.load(Relaxed) == event.windows_key_code)
            as ::std::os::raw::c_int,
    }
}

wrap_keyboard_handler! {
    pub struct BruKeyboardHandler {
        state: Arc<Mutex<BruState>>,
    }

    impl KeyboardHandler {
        fn on_pre_key_event(
            &self,
            browser: Option<&mut Browser>,
            event: Option<&KeyEvent>,
            // The X11 event, named so even on a Wayland session. It lives in the sys crate; the cef
            // crate does not re-export it.
            _os_event: Option<&mut sys::XEvent>,
            _is_keyboard_shortcut: Option<&mut ::std::os::raw::c_int>,
        ) -> ::std::os::raw::c_int {
            let (Some(browser), Some(event)) = (browser, event) else {
                return 0;
            };

            // --- src/editor.rs ------------------------------------------------------------------
            // `fake-key` means "give this key to the page, not to bru". It arrives here because
            // `send_key_event` is the real input path, so the one thing this handler may do with it
            // is nothing — otherwise `<Shift-Escape>`, which is `fake-key <Escape>`, would run the
            // `mode-leave` binding instead of reaching the page. True only while an injection is in
            // flight; see `editor::fake_key`.
            if crate::editor::is_faking() {
                return 0;
            }
            // --- end src/editor.rs --------------------------------------------------------------

            // RAWKEYDOWN only. One press also delivers KEYDOWN and CHAR, and acting on all three
            // scrolls three times per keystroke — which reads as "too fast", not as a bug.
            //
            // Acting once is not the same as *consuming* once, and letting the other three through
            // was a bug of its own. CEF dispatches each of the four to whichever view holds focus at
            // that instant, and a command can move focus between them: `:` runs `cmd-set-text :`,
            // which calls `host.set_focus(1)` on the bottom strip synchronously, so the RAWKEYDOWN
            // is delivered to the page and the CHAR of the same press is delivered to the strip that
            // has just taken focus — where `#cmdline` is a real text input and types it. Measured
            // 2026-08-06: the line read `::` with the cursor at 2. It was invisible for as long as
            // it was because `run_text` strips every leading `:`, so `::open x` still opened x.
            //
            // So a press bru has swallowed is swallowed whole. What pairs the events is the press
            // itself and not the key code — see `swallow_rest_of_press`, where pairing them by code
            // is the mistake that made the first version of this fix pass its own harness and change
            // nothing on the keyboard. `o`, `O`, `go`, `gO`, `b`, `T`, `/` and `?` all open the line
            // the same way and all had the same second character.
            if event.type_ != KeyEventType::RAWKEYDOWN {
                return swallow_rest_of_press(event);
            }
            // A new press: whatever the last one left behind is over. Without this line a press that
            // returns 0 through one of the several paths that do not go past `remember` — a bare
            // modifier, a letter forwarded to the command line, no bindings loaded yet — inherits the
            // previous press's verdict and eats its character. Measured 2026-08-06: typing `:open`
            // produced `:p`, because `o` was forwarded and then `o`'s own CHAR was swallowed by the
            // `:` that had been swallowed before it.
            PRESS_TO_SWALLOW.store(i32::MIN, std::sync::atomic::Ordering::Relaxed);

            // CEF delivers a key to whichever view holds focus, and that is not always the page:
            // this desktop runs `sloppyfocus`, so moving the pointer over a chrome strip is enough.
            //
            // A key arriving at a strip must never be *forwarded* to it. Chromium's own shortcuts
            // are live inside any browser, so an unswallowed `<Ctrl-T>` there navigates the strip
            // itself to `chrome://newtab/` — measured 2026-08-06: the status bar went blank and its
            // renderer logged "Requested load of chrome://newtab/ for incorrect profile type". The
            // chrome is not a page the user browses; nothing it holds should answer a keystroke.
            //
            // So the key is handled as usual and simply aimed at the tab that is showing. From M9
            // command mode becomes the one exception, because then the bottom strip really does
            // want the letters.
            let browser_id = browser.identifier();
            // With more than one window, "the showing tab" is the showing tab *of the window this
            // key arrived at*. CEF hands the event to the browser that has focus, and that browser
            // names its window — so the aim below resolves in the right one. Without this line
            // trap 11 sends a strip's key to whichever window was last made current.
            // The same call also names the window for the *mode*: bru keeps one `ModeManager` per
            // window, and `BruState::mode` reads the current one by index because this line has
            // already made it current. `None` is a browser that belongs to no window — nothing bru
            // put in a window, and there is nothing to route.
            let (window, chrome_key) = {
                let mut state = self.state.lock().expect("state mutex poisoned");
                let window = state.focus_window_of_browser(browser_id);
                (window, state.is_chrome_browser(browser_id))
            };

            let mut redirected;
            let target: &mut Browser = if chrome_key {
                redirected = match self
                    .state
                    .lock()
                    .expect("state mutex poisoned")
                    .active_browser()
                {
                    Some(browser) => browser,
                    // No tab to aim at. Swallow anyway: letting it through reaches Chromium.
                    None => return 1,
                };
                &mut redirected
            } else {
                browser
            };

            // --- src/focus.rs -------------------------------------------------------------------
            // **Nothing about focus is decided here any more, and that is the fix.** This used to
            // read `event.focus_on_editable_field` and enter insert mode from it. That flag rides on
            // a key event, so bru learned a field was focused one keypress *late*: the bar said
            // NORMAL while a caret blinked in a search box, and the key that told it — `:` on
            // `https://start.duckduckgo.com/`, which focuses its own box — entered insert mode and
            // was then looked up in insert mode, where `:` is not bound, so the colon went into the
            // page and the command line never opened. Reported by the user 2026-08-07.
            //
            // The same flag also could not tell a field the *page* focused from one the *user*
            // clicked into, and qutebrowser treats those oppositely (`auto_load` false,
            // `auto_enter` true). One flag cannot answer two questions.
            //
            // Focus is now decided when the focus changes, in `focus.rs`, from
            // `on_focused_node_changed` in the render process. Two things follow that are worth
            // saying out loud: the mode indicator is never a keypress behind, and **this is one
            // less branch and one less mutex acquisition on the path a held `j` takes**.
            // --- end src/focus.rs -----------------------------------------------------------------

            // Translate the CEF event into qutebrowser's own key spelling. `None` is a bare
            // modifier press, which is never a binding on its own.
            let Some(info) = crate::bindings::KeyInfo::from_cef(
                event.windows_key_code,
                event.modifiers,
                event.character,
            ) else {
                return 0;
            };

            // Hint mode has its own parser, over a trie of hint labels rather than of commands
            // (modeparsers.py:135). It answers None in every other mode, so the ordinary path below
            // is untouched.
            if let Some(swallow) = crate::hints::handle_key(&self.state, target, info) {
                return remember(event, swallow as ::std::os::raw::c_int);
            }

            // --- src/caret.rs ---------------------------------------------------------------
            // `set_mark` and `jump_mark` are the same shape: `RegisterKeyParser` consults the one
            // binding those modes have (`<Escape>: mode-leave`) and then takes the next ordinary
            // key as a register name rather than as the start of a chain. Answers None in every
            // other mode, so the ordinary path below is untouched.
            if let Some(swallow) = crate::caret::handle_mark_key(&self.state, target, info) {
                return remember(event, swallow as ::std::os::raw::c_int);
            }
            // --- end src/caret.rs -----------------------------------------------------------

            let Some(outcome) = self
                .state
                .lock()
                .expect("state mutex poisoned")
                .handle_key(info)
            else {
                // No bindings loaded: not the browser process, or before startup finished.
                return 0;
            };

            // The half-typed chain and count, the way qutebrowser's keystring widget shows them.
            crate::ipc::set_keystring(outcome.keystring.clone());

            if let crate::bindings::KeyAction::Run { command, count } = outcome.action {
                // --- src/macros.rs ----------------------------------------------------------
                // A macro records the command a binding resolved to and the count that reached it,
                // never the keys — `runners.py:187`. Before the dispatch, not after: the mode this
                // key was pressed in is what decides, and `mode-enter caret` would otherwise look
                // like a caret-mode command. With no recording in progress this is one relaxed
                // atomic load, which is the whole of what `j` pays for macros existing.
                crate::macros::record(&self.state, &command, count);
                // --- end src/macros.rs ------------------------------------------------------
                // What `.` repeats — `runners.py:184-185`, recorded under the mode the key was
                // pressed in, and before the command runs so that the mode is the one it was run
                // *from*. See `cmdline::record_last_command`; `cmd-repeat-last` excludes itself.
                crate::cmdline::record_last_command(&command, count);
                crate::exec::run(&self.state, target, &command, count);
            }

            // A key that came in on a chrome strip is always swallowed, matched or not — see above
            // — with exactly one exception, and this is it: in command mode the bottom strip is a
            // real text input and has to receive plain typing. The exception is deliberately narrow
            // — command mode only, the bottom strip only, and only keys that type — because
            // widening it is how Chromium's own shortcuts get to navigate bru's UI away (trap 11).
            if chrome_key {
                // The mode of the window this strip belongs to, asked by name rather than as "the
                // current mode": window 1's bottom strip must take the letters when window 1 is the
                // one with the command line open, whatever window 0 is doing.
                let in_command_mode = window.is_some_and(|window| {
                    self.state
                        .lock()
                        .expect("state mutex poisoned")
                        .mode_in(window)
                        == crate::modes::Mode::Command
                });
                let forward = in_command_mode
                    && crate::ipc::is_bottom_chrome_browser(browser_id)
                    && crate::cmdline::types_into_cmdline(&info);
                if std::env::var_os("BRU_DEBUG_KEYS").is_some() {
                    eprintln!(
                        "bru[keys]: {info:?} on chrome browser {browser_id} -> {}",
                        if forward { "FORWARDED to the chrome" } else { "swallowed" },
                    );
                }
                if forward {
                    // The bottom strip really does want this letter. Everything else — every
                    // modifier chord, and every key command mode binds — has already been handled
                    // above and is swallowed here.
                    return 0;
                }
                return remember(event, 1);
            }
            remember(event, outcome.swallow as ::std::os::raw::c_int)
        }
    }
}

wrap_client! {
    pub struct BruClient {
        state: Arc<Mutex<BruState>>,
    }

    impl Client {
        fn keyboard_handler(&self) -> Option<KeyboardHandler> {
            Some(BruKeyboardHandler::new(self.state.clone()))
        }

        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(BruLifeSpanHandler::new(self.state.clone()))
        }

        fn display_handler(&self) -> Option<DisplayHandler> {
            Some(BruDisplayHandler::new(self.state.clone()))
        }

        // M11: where `/`'s match count comes from. src/find.rs owns what it does with it.
        fn find_handler(&self) -> Option<FindHandler> {
            Some(crate::find::BruFindHandler::new())
        }

        // Without this bru cannot save a file at all: with no download handler CEF has nowhere to
        // ask where a file should go. src/downloads.rs owns everything past this call.
        fn download_handler(&self) -> Option<DownloadHandler> {
            Some(crate::downloads::BruDownloadHandler::new())
        }

        // Where bru learns that the page under it is being replaced — see src/load.rs. One handler
        // serves every workstream that needs it; a second would replace this one.
        fn load_handler(&self) -> Option<LoadHandler> {
            Some(crate::load::BruLoadHandler::new(self.state.clone()))
        }

        // bru has a request handler only because the message router demands two of its callbacks.
        // The message router demands two of its callbacks, and the ad blocker asks for a third.
        fn request_handler(&self) -> Option<RequestHandler> {
            // --- adblock -----------------------------------------------------------------------
            // The first point in the browser process the ad blocker is reachable from. It starts a
            // background thread and returns; nothing here waits for a filter list.
            crate::adblock::ensure_loaded();
            // --- end adblock -------------------------------------------------------------------
            Some(BruRequestHandler::new())
        }

        // One of the four calls the message router documents as mandatory.
        fn on_process_message_received(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            source_process: ProcessId,
            message: Option<&mut ProcessMessage>,
        ) -> ::std::os::raw::c_int {
            crate::ipc::on_process_message_received(browser, frame, source_process, message)
        }
    }
}

// Browser lifetime. Without this nothing tells the message loop to stop, so closing the window
// leaves the process running with no window. (The wrap_ macros take no doc comment on the struct.)
wrap_life_span_handler! {
    struct BruLifeSpanHandler {
        state: Arc<Mutex<BruState>>,
    }

    impl LifeSpanHandler {
        // --- src/popups.rs ------------------------------------------------------------------
        // A page asking for a window. Without this the default runs, which is 0 — "go ahead" — and
        // CEF makes a top-level browser bru does not know exists: a `target="_blank"` link opened an
        // operating-system window instead of a tab, against DESIGN.md's "one window, many tabs".
        // `popups.rs` owns the decision and returns 1, which cancels the popup.
        //
        // `on_before_popup_aborted` (bindings 20774) is deliberately not implemented: it fires only
        // for a popup that *was* allowed and then failed to be created, and this returns 1 for every
        // one of them, so it cannot be reached. There is no pending-popup state here to clear either
        // — the decision leaves the callback as a posted task and keeps nothing keyed by popup_id.
        fn on_before_popup(
            &self,
            browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _popup_id: ::std::os::raw::c_int,
            target_url: Option<&CefString>,
            _target_frame_name: Option<&CefString>,
            target_disposition: WindowOpenDisposition,
            user_gesture: ::std::os::raw::c_int,
            popup_features: Option<&PopupFeatures>,
            _window_info: Option<&mut WindowInfo>,
            _client: Option<&mut Option<Client>>,
            _settings: Option<&mut BrowserSettings>,
            _extra_info: Option<&mut Option<DictionaryValue>>,
            _no_javascript_access: Option<&mut ::std::os::raw::c_int>,
        ) -> ::std::os::raw::c_int {
            crate::popups::on_before_popup(
                browser,
                target_url,
                target_disposition,
                user_gesture,
                popup_features,
            )
        }
        // --- end src/popups.rs --------------------------------------------------------------

        fn on_after_created(&self, browser: Option<&mut Browser>) {
            self.state
                .lock()
                .expect("state mutex poisoned")
                .on_after_created(browser);
        }

        fn do_close(&self, browser: Option<&mut Browser>) -> ::std::os::raw::c_int {
            self.state
                .lock()
                .expect("state mutex poisoned")
                .do_close(browser)
        }

        fn on_before_close(&self, browser: Option<&mut Browser>) {
            // The router has to hear about the close before the state does: this is one of its four
            // mandatory forwards, and skipping it leaks that browser's pending queries silently.
            // Once the state has removed the last browser it quits the message loop, so nothing
            // after that call is guaranteed to run.
            crate::ipc::on_before_close(browser.as_deref().cloned().as_mut());

            self.state
                .lock()
                .expect("state mutex poisoned")
                .on_before_close(browser);
        }
    }
}

// --- M4 ----------------------------------------------------------------------------------------
// Two of the four mandatory router forwards. They are the only reason bru has a request handler at
// all; on_before_browse in particular must be called or pending queries leak with no error anywhere.
wrap_request_handler! {
    pub struct BruRequestHandler;

    impl RequestHandler {
        fn on_before_browse(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            _request: Option<&mut Request>,
            _user_gesture: ::std::os::raw::c_int,
            _is_redirect: ::std::os::raw::c_int,
        ) -> ::std::os::raw::c_int {
            // The router is told only about navigations that are allowed to proceed, so this call
            // has to come before the return, and the return has to be "allow".
            crate::ipc::on_before_browse(browser, frame);
            0
        }

        fn on_render_process_terminated(
            &self,
            browser: Option<&mut Browser>,
            _status: TerminationStatus,
            _error_code: ::std::os::raw::c_int,
            _error_string: Option<&CefString>,
        ) {
            crate::ipc::on_render_process_terminated(browser);
        }

        // --- adblock -------------------------------------------------------------------------
        // Once per resource request, on the browser process IO thread, before the request is
        // initiated. `None` — the answer for everything bru allows — is what CEF expects when
        // nobody has an opinion. `Some` is a request that will be cancelled and never sent.
        fn resource_request_handler(
            &self,
            browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            _is_navigation: ::std::os::raw::c_int,
            _is_download: ::std::os::raw::c_int,
            _request_initiator: Option<&CefString>,
            _disable_default_handling: Option<&mut ::std::os::raw::c_int>,
        ) -> Option<ResourceRequestHandler> {
            crate::adblock::resource_request_handler(browser, request)
        }
        // --- end adblock ---------------------------------------------------------------------
    }
}

// Where the status line's URL and title come from. Chromium tells us; we keep it and push it.
//
// Both callbacks are keyed by browser identifier, and that is not a detail. Three browsers share one
// Client — the page and the two chrome strips — so an unkeyed handler lets the tab strip's own
// address overwrite the page's the moment it finishes loading, and the status line then reports
// bru://chrome/top.html for every site visited. The state answers which tab a browser is, and
// ignores anything that is not one.
wrap_display_handler! {
    pub struct BruDisplayHandler {
        state: Arc<Mutex<BruState>>,
    }

    impl DisplayHandler {
        fn on_address_change(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            url: Option<&CefString>,
        ) {
            // Subframes navigate constantly and none of it is the page's address.
            if !frame.map(|frame| frame.is_main() != 0).unwrap_or(false) {
                return;
            }
            let Some(id) = browser.map(|browser| browser.identifier()) else {
                return;
            };
            let url = url.map(CefString::to_string).unwrap_or_default();

            // Which window the tab is in, so a page loading in a background window pushes into that
            // window's bar and strip instead of into whichever one is in front. `None` is not a tab
            // at all — a chrome strip reporting its own bru:// URL.
            let (window, is_active, tabs) = {
                let mut state = self.state.lock().expect("state mutex poisoned");
                let window = state.set_tab_url(id, url.clone());
                let tabs = window.map(|w| state.tabs_json_in(w)).unwrap_or_default();
                (window, state.is_active_browser(id), tabs)
            };
            let Some(window) = window else {
                return;
            };
// --- src/history.rs --------------------------------------------------------
            // A tab's main frame committed an address: that is a visit, and this is the callback
            // `data.rs` was written against. Below the `is_tab` guard, so a chrome strip's own
            // bru:// URL can never reach the history.
            crate::history::visited(id, &url);
// --- end src/history.rs ----------------------------------------------------
            if is_active {
                crate::ipc::set_url_for(window, url);
            }
            crate::ipc::set_tabs_for(window, tabs);
        }

        fn on_title_change(&self, browser: Option<&mut Browser>, title: Option<&CefString>) {
            let Some(id) = browser.map(|browser| browser.identifier()) else {
                return;
            };
            let title = title.map(CefString::to_string).unwrap_or_default();

            // The window the tab is in — see `on_address_change` above.
            let (window, is_active, tabs) = {
                let mut state = self.state.lock().expect("state mutex poisoned");
                let window = state.set_tab_title(id, title.clone());
                let tabs = window.map(|w| state.tabs_json_in(w)).unwrap_or_default();
                (window, state.is_active_browser(id), tabs)
            };
            let Some(window) = window else {
                return;
            };
// --- src/history.rs --------------------------------------------------------
            // The row written at commit has no title yet — the address arrives first. This is what
            // fills it in, against the URL *this* browser was last reported to be on.
            crate::history::retitled(id, &title);
// --- end src/history.rs ----------------------------------------------------
            if is_active {
                crate::ipc::set_title_for(window, title);
            }
            crate::ipc::set_tabs_for(window, tabs);
        }

        // The `<link rel=icon>` URLs of the page. Not keyed by browser here, unlike the two above:
        // `favicon.rs` keys what it stores by the *origin* of the page the browser is on, so a
        // chrome strip — whose frame URL is `bru://` — is refused there and never reaches the map.
        fn on_favicon_urlchange(
            &self,
            browser: Option<&mut Browser>,
            icon_urls: Option<&mut CefStringList>,
        ) {
            crate::favicon::on_favicon_urls(browser, icon_urls);
        }

// --- src/spawn.rs ----------------------------------------------------------
        // Every `console.log`, `.warn` and `.error` in every frame of every tab. `message.rs`
        // decides which of them is worth a line in the bar; almost none is.
        fn on_console_message(
            &self,
            _browser: Option<&mut Browser>,
            level: LogSeverity,
            message: Option<&CefString>,
            source: Option<&CefString>,
            line: ::std::os::raw::c_int,
        ) -> ::std::os::raw::c_int {
            crate::message::on_console_message(level, message, source, line)
        }
// --- end src/spawn.rs ------------------------------------------------------
    }
}

// -----------------------------------------------------------------------------------------------
// Tests — the press bookkeeping only. Everything else here needs a live CEF.
// -----------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// `PRESS_TO_SWALLOW` is one value for the process, because a keyboard is one keyboard — but
    /// `cargo test` runs these three on three threads at once and they then read each other's
    /// press. The lock is the test harness matching the assumption, not a hole in it.
    static ONE_KEYBOARD: Mutex<()> = Mutex::new(());

    fn event(type_: KeyEventType, code: i32) -> KeyEvent {
        KeyEvent { type_, windows_key_code: code, ..Default::default() }
    }

    /// `:` is VKEY_OEM_1. Its RAWKEYDOWN is swallowed by the `cmd-set-text :` binding, and the
    /// command moves focus to the bottom strip before the CHAR of that same press is dispatched —
    /// so without this the strip's `#cmdline` typed the second colon the screenshot showed.
    #[test]
    fn the_rest_of_a_swallowed_press_is_swallowed_too() {
        let _one = ONE_KEYBOARD.lock().unwrap_or_else(|e| e.into_inner());
        PRESS_TO_SWALLOW.store(i32::MIN, std::sync::atomic::Ordering::Relaxed);
        const OEM_1: i32 = 0xBA;
        assert_eq!(remember(&event(KeyEventType::RAWKEYDOWN, OEM_1), 1), 1);
        assert_eq!(swallow_rest_of_press(&event(KeyEventType::KEYDOWN, OEM_1)), 1);
        assert_eq!(swallow_rest_of_press(&event(KeyEventType::CHAR, OEM_1)), 1);
        // Key-up is let through: swallowing it can leave a page holding a modifier that was never
        // released.
        assert_eq!(swallow_rest_of_press(&event(KeyEventType::KEYUP, OEM_1)), 0);
    }

    /// The other half, and the one that matters more: typing in command mode is *forwarded*, not
    /// swallowed, so nothing is remembered and the CHAR that carries the letter must arrive. A fix
    /// that swallowed every CHAR at the bottom strip would make the command line untypable.
    #[test]
    fn a_forwarded_press_keeps_its_character() {
        let _one = ONE_KEYBOARD.lock().unwrap_or_else(|e| e.into_inner());
        PRESS_TO_SWALLOW.store(i32::MIN, std::sync::atomic::Ordering::Relaxed);
        const O: i32 = 0x4F;
        assert_eq!(remember(&event(KeyEventType::RAWKEYDOWN, O), 0), 0);
        assert_eq!(swallow_rest_of_press(&event(KeyEventType::CHAR, O)), 0);
    }

    /// The pairing is by press order, not by key code, and the memory lasts exactly one character.
    ///
    /// It was by code first, and that is why the doubled colon survived the first fix: the pair
    /// never matched on a real keyboard, so the CHAR's code is not the RAWKEYDOWN's. It matched
    /// under the injector, which sets one code on all four events — the harness was reproducing the
    /// assumption rather than the keyboard, and agreed with the fix while both were wrong.
    #[test]
    fn the_memory_lasts_exactly_one_character() {
        let _one = ONE_KEYBOARD.lock().unwrap_or_else(|e| e.into_inner());
        PRESS_TO_SWALLOW.store(i32::MIN, std::sync::atomic::Ordering::Relaxed);
        const OEM_1: i32 = 0xBA;
        const COLON: i32 = 58;

        // What a real `:` looks like: the character event does not carry the physical key's code.
        remember(&event(KeyEventType::RAWKEYDOWN, OEM_1), 1);
        assert_eq!(swallow_rest_of_press(&event(KeyEventType::CHAR, COLON)), 1);

        // And one press pays for exactly one character — the next one is somebody else's.
        assert_eq!(swallow_rest_of_press(&event(KeyEventType::CHAR, COLON)), 0);
    }

    /// Nothing is swallowed when nothing was: with no press pending, every event passes. This is the
    /// state the command line spends all of its time in, since typing is forwarded rather than
    /// swallowed.
    #[test]
    fn an_unswallowed_keyboard_is_transparent() {
        let _one = ONE_KEYBOARD.lock().unwrap_or_else(|e| e.into_inner());
        PRESS_TO_SWALLOW.store(i32::MIN, std::sync::atomic::Ordering::Relaxed);
        for type_ in [KeyEventType::KEYDOWN, KeyEventType::CHAR, KeyEventType::KEYUP] {
            assert_eq!(swallow_rest_of_press(&event(type_, 0x4A)), 0, "{type_:?}");
        }
    }

}
