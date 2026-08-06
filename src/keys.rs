//! Keyboard handling.
//!
//! Scrolling is sent as a synthetic WHEEL event rather than run as JavaScript, and that choice is
//! the reason bru exists. `send_mouse_wheel_event` goes through Chromium's real input path,
//! animation included; `window.scrollBy` is what qutebrowser does, and it is the reason its
//! scrolling never felt like Brave's. Measured on 2026-08-06: through the wheel path it does.

use cef::*;
use std::sync::{Arc, Mutex};

use crate::commands::{Command, ScrollDirection};
use crate::state::BruState;

/// Pixels per press. Chromium's wheel notch is 40 on Linux, so this is three notches — what a mouse
/// delivers per click, and near enough to qutebrowser's step for the two to be compared.
const STEP: i32 = 120;

/// A ceiling on `<count><command>`. qutebrowser has none, but a typo like `99999j` should not lock
/// the UI thread up sending wheel events.
const MAX_COUNT: u32 = 1000;

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
            let chrome_key = self
                .state
                .lock()
                .expect("state mutex poisoned")
                .is_chrome_browser(browser.identifier());

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

            // --- M12 (merge: this is the mode-parser hook src/hints.rs asks for) -----------------
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
                run(&self.state, target, &command, count);
            }

            // A key that came in on a chrome strip is always swallowed, matched or not — see above.
            if chrome_key {
                return 1;
            }
            outcome.swallow as ::std::os::raw::c_int
        }
    }
}

/// Run one command against the browser the key arrived at.
///
/// Everything stage 1 implements is here; the rest of qutebrowser's command set arrives with the
/// command line in M9 and is deliberately inert rather than absent — an unimplemented command still
/// occupies its place in the trie, so `gg` does not become a NoMatch that eats the pending `g`.
fn run(
    state: &Arc<Mutex<BruState>>,
    browser: &mut Browser,
    command: &Command,
    count: Option<u32>,
) {
    // `3j` is three steps of `j`, not one big one — qutebrowser repeats the command.
    let repeat = count.unwrap_or(1).clamp(1, MAX_COUNT);

    match command {
        Command::Chain(parts) => {
            for part in parts {
                run(state, browser, part, count);
            }
        }

        // The reason bru exists. Through `send_mouse_wheel_event`, never `window.scrollBy`: the
        // wheel path is Chromium's real input path, animation included.
        Command::Scroll(direction) => {
            let (dx, dy) = match direction {
                ScrollDirection::Down => (0, -STEP),
                ScrollDirection::Up => (0, STEP),
                ScrollDirection::Left => (STEP, 0),
                ScrollDirection::Right => (-STEP, 0),
                // Top/Bottom/PageUp/PageDown need the page height, which is M11's work.
                _ => return,
            };
            for _ in 0..repeat {
                wheel(browser, dx, dy);
            }
        }
        Command::ScrollPx { dx, dy } => {
            for _ in 0..repeat {
                wheel(browser, *dx, -*dy);
            }
        }

        Command::TabNext => {
            for _ in 0..repeat {
                crate::tabs::next_tab(state);
            }
        }
        Command::TabPrev => {
            for _ in 0..repeat {
                crate::tabs::prev_tab(state);
            }
        }
        Command::TabClose { .. } => crate::tabs::close_current(state),

        // `open` is M9's command, and most of it needs the command line to type a URL into. The
        // part that does not is worth having early: `ga` and `<Ctrl-T>` are bound to a bare
        // `open -t`, so without this there is no way to reach a second tab from the keyboard at
        // all, and `J`/`K`/`d` cannot be exercised. A URL only arrives here from a binding that
        // carries one; the interactive path is M9's.
        Command::Open { url, tab, bg, window, .. } => {
            let target = url.as_deref().unwrap_or(crate::app::HOME);
            // `-w` has no window management behind it yet; treat it as a tab rather than silently
            // doing nothing, and say so once M9 gives windows a meaning.
            if *tab || *bg || *window {
                crate::tabs::new_tab(state, target, *bg);
            } else if let Some(frame) = browser.main_frame() {
                frame.load_url(Some(&CefString::from(target)));
            }
        }

        Command::ModeEnter(mode) => {
            let entered = state
                .lock()
                .expect("state mutex poisoned")
                .enter_mode(*mode, false);
            if entered {
                crate::ipc::set_mode(mode.name().to_string());
            }
        }
        Command::ModeLeave => {
            let mut guard = state.lock().expect("state mutex poisoned");
            if guard.leave_mode() {
                let now = guard.mode();
                drop(guard);
                crate::ipc::set_mode(now.name().to_string());
                // Leaving insert mode should also give the page's text field up, or the next `j`
                // is typed into it rather than scrolling.
                blur(browser);
            }
        }

        // --- M12 (merge: this arm belongs in src/exec.rs) ---------------------------------------
        Command::Hint { target } => {
            let target = match target {
                crate::commands::HintTarget::Normal => crate::hints::Target::Normal,
                crate::commands::HintTarget::TabBg => crate::hints::Target::TabBg,
            };
            crate::hints::start(state, browser, target);
        }
        Command::HintFollow => {}

        // Nothing to do, and that is the point: `nop` exists to shadow a Chromium default, and
        // clear-keychain is already done by the parser reporting the key.
        Command::Nop | Command::ClearKeychain => {}

        // Parsed, bound, and waiting for the milestone that implements it.
        _ => {}
    }
}

/// Chromium delivers a wheel event to whatever sits under the cursor, so it needs a position inside
/// the page rather than over a scrollable child.
fn wheel(browser: &mut Browser, dx: i32, dy: i32) {
    let Some(host) = browser.host() else {
        return;
    };
    let mouse = MouseEvent { x: 10, y: 10, modifiers: 0 };
    host.send_mouse_wheel_event(Some(&mouse), dx, dy);
}

/// Drop focus from whatever the page had focused. One-off script rather than a CEF call because
/// CEF has no "blur the focused element" — and this runs on leaving insert mode, not on the key
/// path proper.
fn blur(browser: &mut Browser) {
    let Some(frame) = browser.main_frame() else {
        return;
    };
    frame.execute_java_script(
        Some(&CefString::from(
            "document.activeElement && document.activeElement.blur();",
        )),
        None,
        0,
    );
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
