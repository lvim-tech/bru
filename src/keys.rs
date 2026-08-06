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
            self.state
                .lock()
                .expect("state mutex poisoned")
                .on_before_close(browser);
        }
    }
}
