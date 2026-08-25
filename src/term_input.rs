//! C7's other half: bytes off the terminal, keys into the browser.
//!
//! **The parsing is not here.** `term_keys.rs` turns bytes into a `TermKey` and is pure; this is the
//! loop that feeds it and the hop onto the UI thread that CEF requires. Keeping the two apart is
//! what lets the whole of the key grammar be tested without a terminal, which is where its 48 tests
//! come from.
//!
//! ## Why a press becomes three events
//!
//! CEF wants `KEYDOWN`, then `CHAR` for a keystroke that types something, then `KEYUP` —
//! `editor.rs` sends exactly that trio for `:fake-key`, and for the same reason: Chromium's key
//! handling and its text input are two paths, and a `KEYDOWN` alone moves the caret without writing
//! a letter. The terminal reports releases only when asked and not always then, so a press here is
//! the whole trio rather than half of one waiting for a release that may never arrive.

use cef::*;
use std::io::Read;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use crate::term_keys::{Step, TermKey, TermMouse, next_event, next_event_at_timeout};

/// The page's browser. Where keys go unless a mode says otherwise.
static TARGET: AtomicI32 = AtomicI32::new(0);

/// Set when the loop should stop, so `leave` does not have to kill a thread.
static STOPPING: AtomicBool = AtomicBool::new(false);

/// Aim the keyboard at a browser.
pub fn aim_at(identifier: i32) {
    TARGET.store(identifier, Ordering::Relaxed);
}

/// Stop reading. The loop notices within one read timeout.
pub fn stop() {
    STOPPING.store(true, Ordering::Relaxed);
}

/// Read the terminal until told to stop, beginning with bytes somebody else already read.
pub fn start_with(pushback: Vec<u8>) {
    STOPPING.store(false, Ordering::Relaxed);
    let _ = std::thread::Builder::new().name("bru-term-input".to_string()).spawn(move || {
        let mut buffer: Vec<u8> = Vec::with_capacity(256);
        buffer.extend_from_slice(&pushback);
        let mut chunk = [0u8; 256];
        let mut stdin = std::io::stdin();
        while !STOPPING.load(Ordering::Relaxed) {
            let read = match stdin.read(&mut chunk) {
                Ok(0) => 0,
                Ok(n) => {
                    buffer.extend_from_slice(&chunk[..n]);
                    n
                }
                // A read that fails is a terminal that is gone; there is nothing to recover to.
                Err(_) => break,
            };
            // **A read that returned nothing is the timeout, and the timeout is information.** A
            // lone `ESC` is Escape only once it is known that nothing followed it; until then it is
            // the start of a sequence. `term_keys` splits those two answers, and this is the only
            // place that can tell them apart, because only here is it known that the terminal went
            // quiet.
            drain(&mut buffer, read == 0);
        }
    });
}

/// Turn as much of the buffer into events as it currently holds.
fn drain(buffer: &mut Vec<u8>, quiet: bool) {
    loop {
        if buffer.is_empty() {
            return;
        }
        let step = if quiet { next_event_at_timeout(buffer) } else { next_event(buffer) };
        match step {
            Step::Key(key, n) => {
                buffer.drain(..n);
                if key.is_press() {
                    post(key);
                }
            }
            Step::Mouse(mouse, n) => {
                buffer.drain(..n);
                post_mouse(mouse);
            }
            // Focus changes, query replies, paste markers. Quietly, because they are not errors and
            // there will be many.
            Step::Ignored(n) | Step::Invalid(n) => {
                buffer.drain(..n);
            }
            // Nothing whole yet. On a quiet read there is nothing more coming either, so the buffer
            // is left alone rather than spun on.
            Step::Incomplete => return,
        }
    }
}

/// SGR's wheel buttons. The protocol reports a scroll as a press of button 64 or 65 rather than as
/// a scroll of its own, which is why they are named here instead of read as buttons.
const WHEEL_UP: u16 = 64;
const WHEEL_DOWN: u16 = 65;

/// How many mouse reports to log before going quiet.
///
/// **Three, because the question they answer is binary and asking it forever would be a log per
/// pointer movement.** Either reports arrive or they do not; if they do, three of them show whether
/// the numbers are cells or pixels.
static MOUSE_SEEN: AtomicI32 = AtomicI32::new(0);

/// Where the pointer was last reported, so a report that moved it nowhere costs nothing.
///
/// **Mode 1003 reports every motion, and most of them are the same cell twice.** A terminal reports
/// in whatever resolution it has; two reports that resolve to one position are one position, and
/// posting a task for the second is a task that tells Chromium what it already knows. Packed into
/// one atomic so the reader thread needs no lock on the busiest path it has.
static LAST_POINTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(u64::MAX);

fn post_mouse(mouse: TermMouse) {
    let seen = MOUSE_SEEN.fetch_add(1, Ordering::Relaxed);
    if seen < 3 {
        eprintln!(
            "bru[term]: mouse report {seen}: button={} at ({},{}) pressed={} motion={} mods={}",
            mouse.button, mouse.x, mouse.y, mouse.pressed, mouse.motion, mouse.modifiers
        );
    }
    if mouse.motion {
        let packed = (u64::from(mouse.x as u32) << 32) | u64::from(mouse.y as u32);
        if LAST_POINTER.swap(packed, Ordering::Relaxed) == packed {
            return;
        }
    }
    let mut task = MouseTask::new(
        mouse.button,
        mouse.x,
        mouse.y,
        mouse.modifiers,
        mouse.pressed,
        mouse.motion,
    );
    post_task(ThreadId::UI, Some(&mut task));
}

wrap_task! {
    struct MouseTask {
        button: u16,
        x: i32,
        y: i32,
        modifiers: u32,
        pressed: bool,
        motion: bool,
    }

    impl Task {
        fn execute(&self) {
            let Some(state) = crate::state::BruState::instance() else {
                return;
            };
            // A click is always the page's. The chrome strips have nothing clickable in them —
            // there is no tab to close with a pointer and no button on the status bar — and a
            // click that fell through to one would be a press in a document with nothing to press.
            let Some(host) = page_browser(&state).and_then(|browser| browser.host()) else {
                return;
            };
            let Some(page) = crate::term_frontend::page_rect() else {
                return;
            };
            // SGR counts from 1 and reports in the pane's coordinates; CEF wants 0-based
            // coordinates inside the surface it is painting. **And the units are not always
            // pixels** — a terminal that does not know mode 1016 reports cells, and multiplying is
            // the difference between clicking where the user pointed and clicking in the top-left
            // corner of the page. See `term_frontend::mouse_scale`.
            let (scale_x, scale_y) = crate::term_frontend::mouse_scale();
            // **The middle of the cell, not its corner.** A cell-resolution report says which cell
            // the pointer was in and nothing about where in it — and this terminal's cell is 9x25
            // px, so the top-left corner can be most of a line of text above what was pointed at.
            // Measured 2026-08-25: clicks on links "sometimes" worked, and the ones that failed
            // were the ones whose cell corner fell outside the link's box. The centre is the best
            // estimate of a point that is known only to be somewhere in the cell, and it halves the
            // worst case in both axes. With pixel reports the scale is 1 and this adds nothing.
            let x = (self.x - 1) * scale_x + scale_x / 2 - page.x;
            let y = (self.y - 1) * scale_y + scale_y / 2 - page.y;
            if x < 0 || y < 0 || x >= page.width || y >= page.height {
                return;
            }
            let event = MouseEvent { x, y, modifiers: self.modifiers };

            // **Pointing is its own event and most of them are only that.** Without it Chromium
            // never learns the pointer moved: no `:hover`, no cursor over a link, and a click on
            // anything that waits for a `mousemove` first does nothing.
            if self.motion {
                host.send_mouse_move_event(Some(&event), 0);
                return;
            }

            if matches!(self.button, WHEEL_UP | WHEEL_DOWN) {
                // Only on the press: SGR reports a wheel notch once, and acting on the release too
                // would scroll twice per notch. The step is the one `j` uses, so a wheel and a key
                // move the page by the same amount.
                if self.pressed {
                    let step = crate::scroll::step();
                    let delta = if self.button == WHEEL_UP { step } else { -step };
                    host.send_mouse_wheel_event(Some(&event), 0, delta);
                }
                return;
            }

            let button = match self.button {
                0 => MouseButtonType::LEFT,
                1 => MouseButtonType::MIDDLE,
                2 => MouseButtonType::RIGHT,
                // A button bru has no name for is a button bru does not press.
                _ => return,
            };
            // The move first, so the page knows where the pointer is before it is told it was
            // pressed — a click delivered to a page that thinks the pointer is elsewhere hits
            // whatever was under the old position.
            host.send_mouse_move_event(Some(&event), 0);
            host.send_mouse_click_event(Some(&event), button, i32::from(!self.pressed), 1);
        }
    }
}

fn post(key: TermKey) {
    let mut task = KeyTask::new(key.windows_key_code, key.modifiers, key.character, key.text);
    post_task(ThreadId::UI, Some(&mut task));
}

/// Which browser this keystroke is for.
///
/// **In a window CEF answers this and here nobody does.** Views delivers a key to whichever view
/// holds focus, and bru moves that focus itself: `:` runs `cmd-set-text`, which calls
/// `host.set_focus(1)` on the bottom strip (`ipc.rs`), and every following keystroke lands in the
/// `#cmdline` input without anything else being told. Windowless browsers have no such arrangement
/// between them — there is no focus manager over four surfaces, only four hosts and whichever one
/// is handed the event. So the routing bru gets for free in a window is written out here.
///
/// Measured 2026-08-25: with every key going to the page, `:` opened the command line, the status
/// bar said `COMMAND`, and nothing could be typed into it. The mode was right; the keystrokes were
/// going to the wrong browser.
///
/// The mode is read rather than a focus flag being kept, because the mode is what bru already keeps
/// and a second copy of "where is focus" is a second thing to get out of step.
/// How many key routings to log before going quiet. Enough to see a `:` and a word after it.
static ROUTED: AtomicI32 = AtomicI32::new(0);

fn target_for(state: &crate::tabs::SharedState) -> Option<Browser> {
    let mode = state.lock().expect("state mutex poisoned").mode_in(0);
    let chosen = route(state, mode);
    let seen = ROUTED.fetch_add(1, Ordering::Relaxed);
    if seen < 12 {
        eprintln!(
            "bru[term]: key {seen}: mode={mode:?} -> browser={:?} (page={}, bottom={:?})",
            chosen.as_ref().map(|browser| browser.identifier()),
            TARGET.load(Ordering::Relaxed),
            crate::ipc::bottom_chrome_browser_for(0).map(|browser| browser.identifier()),
        );
    }
    chosen
}

fn route(state: &crate::tabs::SharedState, mode: crate::modes::Mode) -> Option<Browser> {
    match mode {
        // The command line is `#cmdline` in `bottom.html`.
        crate::modes::Mode::Command => crate::ipc::bottom_chrome_browser_for(0),
        // A question is `#prompt` in `panel.html`. The panel is not a terminal surface yet, so
        // these fall through to the page rather than to a browser that does not exist — the keys
        // are lost either way, and this way they are lost somewhere that can be seen.
        crate::modes::Mode::Prompt | crate::modes::Mode::YesNo => page_browser(state),
        _ => page_browser(state),
    }
}

fn page_browser(state: &crate::tabs::SharedState) -> Option<Browser> {
    let identifier = TARGET.load(Ordering::Relaxed);
    if identifier == 0 {
        return None;
    }
    state.lock().expect("state mutex poisoned").browser_with_id(identifier)
}

wrap_task! {
    struct KeyTask {
        code: i32,
        modifiers: u32,
        character: u16,
        text: Option<char>,
    }

    impl Task {
        fn execute(&self) {
            let Some(state) = crate::state::BruState::instance() else {
                return;
            };
            let Some(host) = target_for(&state).and_then(|browser| browser.host()) else {
                return;
            };

            // The character CEF is told about. `text` is what the keystroke types, which is not
            // always what the key is called — a dead key composing an accent types one character
            // from two presses, and the protocol reports it in its own parameter.
            let typed = self
                .text
                .and_then(|ch| {
                    let mut units = [0u16; 2];
                    let encoded = ch.encode_utf16(&mut units);
                    (encoded.len() == 1).then_some(units[0])
                })
                .unwrap_or(self.character);

            for type_ in [KeyEventType::KEYDOWN, KeyEventType::CHAR, KeyEventType::KEYUP] {
                if type_ == KeyEventType::CHAR && typed == 0 {
                    continue;
                }
                let event = KeyEvent {
                    type_,
                    modifiers: self.modifiers,
                    windows_key_code: self.code,
                    native_key_code: 0,
                    character: typed,
                    unmodified_character: typed,
                    ..Default::default()
                };
                host.send_key_event(Some(&event));
            }
        }
    }
}
