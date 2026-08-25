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

use crate::term_keys::{Step, TermKey, next_event, next_event_at_timeout};

/// The browser keys are aimed at. Set once the page's browser exists.
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

/// Read the terminal until told to stop.
pub fn start() {
    STOPPING.store(false, Ordering::Relaxed);
    let _ = std::thread::Builder::new().name("bru-term-input".to_string()).spawn(|| {
        let mut buffer: Vec<u8> = Vec::with_capacity(256);
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
            // Mouse reports, focus changes, query replies, paste markers. Quietly, because they are
            // not errors and there will be many.
            Step::Ignored(n) | Step::Invalid(n) => {
                buffer.drain(..n);
            }
            // Nothing whole yet. On a quiet read there is nothing more coming either, so the buffer
            // is left alone rather than spun on.
            Step::Incomplete => return,
        }
    }
}

fn post(key: TermKey) {
    let mut task = KeyTask::new(key.windows_key_code, key.modifiers, key.character, key.text);
    post_task(ThreadId::UI, Some(&mut task));
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
            let identifier = TARGET.load(Ordering::Relaxed);
            if identifier == 0 {
                return;
            }
            let Some(state) = crate::state::BruState::instance() else {
                return;
            };
            let browser = state
                .lock()
                .expect("state mutex poisoned")
                .browser_with_id(identifier);
            let Some(host) = browser.and_then(|browser| browser.host()) else {
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
