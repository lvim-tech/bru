//! The dispatcher: the one place a [`Command`] becomes an action.
//!
//! It lives in its own file because eight workstreams add arms to it. `keys.rs` translates a
//! keypress into a `Command` and calls [`run`]; nothing else in bru turns a command into an effect.
//!
//! **The match in [`run`] has no `_` arm, and neither does [`is_live`].** That is what keeps the two
//! honest: a new [`Command`] variant fails to compile until both have been told what it does and
//! whether it does anything, and the count of live bindings below cannot quietly go stale.

use cef::*;

use crate::commands::{Command, ScrollDirection};
use crate::tabs::SharedState;

/// Pixels per press. Chromium's wheel notch is 40 on Linux, so this is three notches — what a mouse
/// delivers per click, and near enough to qutebrowser's step for the two to be compared.
const STEP: i32 = 120;

/// A ceiling on `<count><command>`. qutebrowser has none, but a typo like `99999j` should not lock
/// the UI thread up sending wheel events.
const MAX_COUNT: u32 = 1000;

/// Run one command against the browser the key arrived at.
///
/// `browser` is always a tab, never a chrome strip: `keys.rs` redirects a key that landed on a strip
/// at the showing tab before calling here (CEF-NOTES trap 11).
pub fn run(state: &SharedState, browser: &mut Browser, command: &Command, count: Option<u32>) {
    // `3j` is three steps of `j`, not one big one — qutebrowser repeats the command.
    let repeat = count.unwrap_or(1).clamp(1, MAX_COUNT);

    match command {
        // --- chains -------------------------------------------------------------------------
        Command::Chain(parts) => {
            for part in parts {
                run(state, browser, part, count);
            }
        }

        // --- scrolling ----------------------------------------------------------------------
        // The reason bru exists. Through `send_mouse_wheel_event`, never `window.scrollBy`: the
        // wheel path is Chromium's real input path, animation included.
        Command::Scroll(direction) => {
            let (dx, dy) = match direction {
                ScrollDirection::Down => (0, -STEP),
                ScrollDirection::Up => (0, STEP),
                ScrollDirection::Left => (STEP, 0),
                ScrollDirection::Right => (-STEP, 0),
                // Top/Bottom/PageUp/PageDown need the page height — src/scroll.rs.
                ScrollDirection::Top
                | ScrollDirection::Bottom
                | ScrollDirection::PageUp
                | ScrollDirection::PageDown => return,
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
        // SLOT: src/scroll.rs — `scroll-page`, `scroll-to-perc`, and the four directions above.
        Command::ScrollPage { .. } | Command::ScrollToPerc { .. } => {}

        // --- tabs ---------------------------------------------------------------------------
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
        Command::TabOnly { .. } | Command::TabFocus { .. } => {}

        // --- opening ------------------------------------------------------------------------
        // `open` is M9's command, and most of it needs the command line to type a URL into. The
        // part that does not is worth having early: `ga` and `<Ctrl-T>` are bound to a bare
        // `open -t`, so without this there is no way to reach a second tab from the keyboard at
        // all, and `J`/`K`/`d` cannot be exercised. A URL only arrives here from a binding that
        // carries one; the interactive path is M9's.
        //
        // SLOT: src/open.rs replaces the body of this arm with the URL-vs-search version.
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

        // --- navigation ---------------------------------------------------------------------
        Command::Back { .. }
        | Command::Forward { .. }
        | Command::Reload { .. }
        | Command::Stop
        | Command::Home => {}

        // --- lifetime -----------------------------------------------------------------------
        Command::Quit { .. } | Command::Close => {}

        // --- modes --------------------------------------------------------------------------
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

        // --- the command line ---------------------------------------------------------------
        // SLOT: src/cmdline.rs.
        Command::CmdSetText { .. } | Command::CommandAccept { .. } => {}

        // Nothing to do, and that is the point: `nop` exists to shadow a Chromium default, and
        // clear-keychain is already done by the parser reporting the key.
        Command::Nop | Command::ClearKeychain => {}

        // A command qutebrowser has and bru's parser does not know. It kept its place in the trie
        // so `;` still reports a partial match; running it does nothing.
        Command::Unimplemented(_) => {}
    }
}

/// Whether [`run`] does anything for this command — the *only* thing that may disagree with the
/// match above, and the reason both are exhaustive.
///
/// Used to count how many of qutebrowser's 226 default bindings are live, which is the number each
/// milestone of stage 2 is measured by.
#[cfg(test)]
pub fn is_live(command: &Command) -> bool {
    match command {
        // A chain is live when every link is: `clear-keychain ;; search` half-works, and half is
        // not what the binding means.
        Command::Chain(parts) => parts.iter().all(is_live),

        Command::Scroll(direction) => matches!(
            direction,
            ScrollDirection::Up
                | ScrollDirection::Down
                | ScrollDirection::Left
                | ScrollDirection::Right
        ),
        Command::ScrollPx { .. } => true,
        Command::ScrollPage { .. } | Command::ScrollToPerc { .. } => false,

        Command::TabNext | Command::TabPrev | Command::TabClose { .. } => true,
        Command::TabOnly { .. } | Command::TabFocus { .. } => false,

        Command::Open { .. } => true,

        Command::Back { .. }
        | Command::Forward { .. }
        | Command::Reload { .. }
        | Command::Stop
        | Command::Home => false,

        Command::Quit { .. } | Command::Close => false,

        Command::ModeEnter(_) | Command::ModeLeave => true,

        Command::CmdSetText { .. } | Command::CommandAccept { .. } => false,

        Command::Nop | Command::ClearKeychain => true,

        Command::Unimplemented(_) => false,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands;
    use crate::config::DEFAULT_BINDINGS;

    /// The three-way split of qutebrowser's 226 default bindings, printed rather than only
    /// asserted: the headline number of every stage-2 milestone is "how many are live", and a
    /// number that is not printed is a number nobody checks.
    fn split() -> (usize, usize, usize) {
        let (mut live, mut ignored, mut unparsed) = (0, 0, 0);
        for (_mode, _keys, cmd) in DEFAULT_BINDINGS {
            let parsed = commands::parse(cmd).expect("a default binding must parse");
            if !parsed.is_implemented() {
                unparsed += 1;
            } else if is_live(&parsed) {
                live += 1;
            } else {
                ignored += 1;
            }
        }
        (live, ignored, unparsed)
    }

    #[test]
    fn how_many_default_bindings_are_live() {
        let (live, ignored, unparsed) = split();
        println!(
            "default bindings: {live} live, {ignored} parsed but ignored, {unparsed} unparsed, \
             {} total",
            live + ignored + unparsed
        );
        assert_eq!(live + ignored + unparsed, DEFAULT_BINDINGS.len());
        assert_eq!(DEFAULT_BINDINGS.len(), 226);
    }
}
