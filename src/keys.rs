//! Keyboard handling: a CEF key event in, a [`crate::commands::Command`] out, and one call to the
//! dispatcher.
//!
//! What a command then *does* lives in `src/exec.rs`, which is the only place a command becomes an
//! action. This file translates and routes; it decides nothing about any individual command.

use cef::*;
use std::sync::{Arc, Mutex};

use crate::state::BruState;

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

            // RAWKEYDOWN only. One press also delivers KEYDOWN and CHAR, and acting on all three
            // scrolls three times per keystroke — which reads as "too fast", not as a bug.
            if event.type_ != KeyEventType::RAWKEYDOWN {
                return 0;
            }

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
            let chrome_key = self
                .state
                .lock()
                .expect("state mutex poisoned")
                .is_chrome_browser(browser_id);

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

            // A focused text field means insert mode, which is qutebrowser's
            // `input.insert_mode.auto_enter` and defaults to true. `only_if_normal` is what keeps a
            // page's focus event from stealing passthrough out from under the user.
            if event.focus_on_editable_field != 0 {
                let entered = self
                    .state
                    .lock()
                    .expect("state mutex poisoned")
                    .enter_mode(crate::modes::Mode::Insert, true);
                if entered {
                    crate::ipc::set_mode("insert".to_string());
                }
            }

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
                return swallow as ::std::os::raw::c_int;
            }

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
                crate::exec::run(&self.state, target, &command, count);
            }

            // A key that came in on a chrome strip is always swallowed, matched or not — see above
            // — with exactly one exception, and this is it: in command mode the bottom strip is a
            // real text input and has to receive plain typing. The exception is deliberately narrow
            // — command mode only, the bottom strip only, and only keys that type — because
            // widening it is how Chromium's own shortcuts get to navigate bru's UI away (trap 11).
            if chrome_key {
                let in_command_mode = self
                    .state
                    .lock()
                    .expect("state mutex poisoned")
                    .mode()
                    == crate::modes::Mode::Command;
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
                return 1;
            }
            outcome.swallow as ::std::os::raw::c_int
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

            let (is_tab, is_active, tabs) = {
                let mut state = self.state.lock().expect("state mutex poisoned");
                let is_tab = state.set_tab_url(id, url.clone());
                (is_tab, state.is_active_browser(id), state.tabs_json())
            };
            if !is_tab {
                return;
            }
            if is_active {
                crate::ipc::set_url(url);
            }
            crate::ipc::set_tabs(tabs);
        }

        fn on_title_change(&self, browser: Option<&mut Browser>, title: Option<&CefString>) {
            let Some(id) = browser.map(|browser| browser.identifier()) else {
                return;
            };
            let title = title.map(CefString::to_string).unwrap_or_default();

            let (is_tab, is_active, tabs) = {
                let mut state = self.state.lock().expect("state mutex poisoned");
                let is_tab = state.set_tab_title(id, title.clone());
                (is_tab, state.is_active_browser(id), state.tabs_json())
            };
            if !is_tab {
                return;
            }
            if is_active {
                crate::ipc::set_title(title);
            }
            crate::ipc::set_tabs(tabs);
        }
    }
}
