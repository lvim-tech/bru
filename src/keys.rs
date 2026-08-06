//! Keyboard handling.
//!
//! Scrolling is sent as a synthetic WHEEL event rather than run as JavaScript, and that choice is
//! the reason bru exists. `send_mouse_wheel_event` goes through Chromium's real input path,
//! animation included; `window.scrollBy` is what qutebrowser does, and it is the reason its
//! scrolling never felt like Brave's. Measured on 2026-08-06: through the wheel path it does.

use cef::*;

/// Pixels per press. Chromium's wheel notch is 40 on Linux, so this is three notches — what a mouse
/// delivers per click, and near enough to qutebrowser's step for the two to be compared.
const STEP: i32 = 120;

wrap_keyboard_handler! {
    pub struct BruKeyboardHandler;

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
    pub struct BruClient;

    impl Client {
        fn keyboard_handler(&self) -> Option<KeyboardHandler> {
            Some(BruKeyboardHandler::new())
        }

        // --- M4 --------------------------------------------------------------------------------
        fn display_handler(&self) -> Option<DisplayHandler> {
            Some(BruDisplayHandler::new())
        }

        fn request_handler(&self) -> Option<RequestHandler> {
            Some(BruRequestHandler::new())
        }

        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(BruLifeSpanHandler::new())
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

// --- M4 ----------------------------------------------------------------------------------------
// The fourth mandatory forward. M1 gives BruClient a real life-span handler that also tracks
// browsers and quits the message loop; when the two meet, this one line moves into that one and
// this block goes away.
wrap_life_span_handler! {
    pub struct BruLifeSpanHandler;

    impl LifeSpanHandler {
        fn on_before_close(&self, browser: Option<&mut Browser>) {
            crate::ipc::on_before_close(browser);
        }
    }
}

// --- M4 ----------------------------------------------------------------------------------------
// Where the status line's URL and title come from. Chromium tells us; we keep it and push it.
wrap_display_handler! {
    pub struct BruDisplayHandler;

    impl DisplayHandler {
        fn on_address_change(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            url: Option<&CefString>,
        ) {
            // Subframes navigate constantly and none of it is the page's address.
            if frame.map(|frame| frame.is_main() != 0).unwrap_or(false) {
                crate::ipc::set_url(url.map(CefString::to_string).unwrap_or_default());
            }
        }

        fn on_title_change(&self, _browser: Option<&mut Browser>, title: Option<&CefString>) {
            crate::ipc::set_title(title.map(CefString::to_string).unwrap_or_default());
        }
    }
}
