//! Keyboard handling.
//!
//! Scrolling is sent as a synthetic WHEEL event rather than run as JavaScript, and that choice is
//! the reason bru exists. `send_mouse_wheel_event` goes through Chromium's real input path,
//! animation included; `window.scrollBy` is what qutebrowser does, and it is the reason its
//! scrolling never felt like Brave's. Measured on 2026-08-06: through the wheel path it does.

use cef::*;
use std::sync::{Arc, Mutex};

use crate::state::BruState;

/// Pixels per press. Chromium's wheel notch is 40 on Linux, so this is three notches — what a mouse
/// delivers per click, and near enough to qutebrowser's step for the two to be compared.
const STEP: i32 = 120;

/// `EVENTFLAG_SHIFT_DOWN`. Key codes do not distinguish `j` from `J`; the modifier bits do.
const SHIFT_DOWN: u32 = 1 << 1;

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

            // Leave the page alone while a text field has focus, or j and k cannot be typed into a
            // search box.
            if event.focus_on_editable_field != 0 {
                return 0;
            }

            // A key that reached a chrome strip is a key meant for the chrome — the command line
            // takes letters. Scrolling the tab strip with j would be nonsense anyway.
            if self
                .state
                .lock()
                .expect("state mutex poisoned")
                .is_chrome_browser(browser.identifier())
            {
                return 0;
            }

            let shift = event.modifiers & SHIFT_DOWN != 0;

            // ------------------------------------------------------------------------------
            // TEMPORARY (M5). Four keys wired straight to their commands. M6's key-sequence
            // parser and M7's binding table replace this whole block with a table lookup —
            // delete it entire, nothing outside it knows these key codes.
            // ------------------------------------------------------------------------------
            if shift && event.windows_key_code == 0x4A {
                crate::tabs::next_tab(&self.state);
                return 1;
            }
            if shift && event.windows_key_code == 0x4B {
                crate::tabs::prev_tab(&self.state);
                return 1;
            }
            if !shift && event.windows_key_code == 0x44 {
                // `d` — tab-close.
                crate::tabs::close_current(&self.state);
                return 1;
            }
            if !shift && event.windows_key_code == 0x54 {
                // `t`. A stand-in for `:open -t`, which arrives with the command line in M9;
                // without some way to make a second tab, none of the rest can be exercised. The
                // page is a placeholder so that switching is visible before M4 draws the strip.
                let index = self
                    .state
                    .lock()
                    .expect("state mutex poisoned")
                    .tab_count();
                crate::tabs::new_tab(&self.state, &crate::app::placeholder_tab(index), false);
                return 1;
            }
            // ------------------------------------------------------------------------------
            // End of the temporary block.
            // ------------------------------------------------------------------------------

            // Windows key codes: CEF normalises to them on every platform.
            let delta = match event.windows_key_code {
                0x4A => -STEP, // j — down. Wheel deltas run the other way.
                0x4B => STEP,  // k — up
                _ => return 0,
            };

            let Some(host) = browser.host() else {
                return 0;
            };

            // A wheel event carries a position, because Chromium delivers it to whatever sits under
            // the cursor. (10, 10) is inside the page rather than over a scrollable child.
            let mouse = MouseEvent { x: 10, y: 10, modifiers: 0 };
            host.send_mouse_wheel_event(Some(&mouse), 0, delta);

            1 // handled — the page never sees the key
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

        // bru has a request handler only because the message router demands two of its callbacks.
        fn request_handler(&self) -> Option<RequestHandler> {
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
