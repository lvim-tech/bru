//! Somewhere to say something.
//!
//! bru has had nowhere to tell the user anything. A yank that happened, a download that finished, a
//! command that failed — all of it went to stderr, where nobody running a browser is looking. The
//! theme has defined `--messages-error-fg` and its five siblings since the first generated
//! `theme.css`, and nothing has ever used them.
//!
//! This is the minimum that fills that gap and no more:
//!
//! - **One line, in the bar bru already has.** qutebrowser puts messages in the same slot as the
//!   command line — literally the same slot: `statusbar/bar.py:169` puts `cmd` and `txt` in one
//!   `QStackedLayout`, and `_show_cmd_widget`/`_hide_cmd_widget` swap between them. So the message
//!   goes in the command line's grid cell and is hidden while the line is open. DESIGN.md's bar
//!   shape is unchanged: one row, status on the right.
//! - **Three levels**, spelled as qutebrowser spells them, each with the colours the theme already
//!   carries.
//! - **It goes away by itself**, after `messages.timeout` — 3000 ms, `configdata.yml:2056`.
//!
//! What is deliberately *not* here: the `:messages` log page. qutebrowser keeps every message and
//! shows the lot at `qute://log`; that is a second chrome document and a ring buffer, and neither is
//! needed by anything that wants to say "yanked".
//!
//! Nothing in here takes a lock on `BruState`, so any workstream can call it from anywhere on the
//! UI thread without thinking about the mutex it is already holding.

use cef::*;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// How long a message stays, in milliseconds. qutebrowser's `messages.timeout` default.
const TIMEOUT_MS: i64 = 3000;

/// The three qutebrowser has, and the three the theme has colours for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    Info,
    Warning,
    Error,
}

impl Level {
    /// The class the bar puts on `#message`, which is what picks the colours out of `theme.css`.
    pub fn name(self) -> &'static str {
        match self {
            Level::Info => "info",
            Level::Warning => "warning",
            Level::Error => "error",
        }
    }
}

struct Shown {
    level: Level,
    text: String,
}

fn shown() -> &'static Mutex<Option<Shown>> {
    static SHOWN: Mutex<Option<Shown>> = Mutex::new(None);
    &SHOWN
}

/// Which message is on screen. A timer only clears the message it was started for, so a second
/// message arriving after two seconds gets its own three and is not cut short by the first one's.
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub fn info(text: &str) {
    show(Level::Info, text);
}

pub fn warning(text: &str) {
    show(Level::Warning, text);
}

pub fn error(text: &str) {
    show(Level::Error, text);
}

/// Say something in the bar.
///
/// Safe to call from any thread: the state is a plain mutex, and the push it triggers is
/// `execute_java_script`, which CEF forwards to the right thread itself. Only the timer needs the UI
/// thread, and `post_delayed_task` names it.
pub fn show(level: Level, text: &str) {
    if text.is_empty() {
        return;
    }
    // stderr as well as the bar. The bar shows one line for three seconds; a terminal keeps them
    // all, and every message this replaces was an eprintln to begin with.
    eprintln!("bru[{}]: {text}", level.name());

    if let Ok(mut shown) = shown().lock() {
        *shown = Some(Shown { level, text: to_one_line(text) });
    }
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed) + 1;
    crate::ipc::push_bar();

    let mut task = ClearMessage::new(sequence);
    post_delayed_task(ThreadId::UI, Some(&mut task), TIMEOUT_MS);
}

/// Take the message away now — what `Escape` does in qutebrowser, and what leaving command mode
/// should not have to wait three seconds for.
pub fn clear() {
    let had = match shown().lock() {
        Ok(mut shown) => shown.take().is_some(),
        Err(_) => false,
    };
    if had {
        // Nothing may clear a message that has not been shown yet, so move the sequence on.
        SEQUENCE.fetch_add(1, Ordering::Relaxed);
        crate::ipc::push_bar();
    }
}

/// What the bar is pushed: `{"level":"error","text":"…"}`, or `null` when there is nothing to say.
pub fn json() -> String {
    let Ok(shown) = shown().lock() else {
        return "null".to_string();
    };
    match shown.as_ref() {
        Some(shown) => format!(
            "{{\"level\":\"{}\",\"text\":\"{}\"}}",
            shown.level.name(),
            crate::ipc::json_escape(&shown.text),
        ),
        None => "null".to_string(),
    }
}

/// One line, however many the caller had. A message is one row of a 24px bar; a newline in it would
/// either be swallowed by the layout or push the bar's height around, and the second is worse.
/// qutebrowser does the same (`message.py`'s `_log_stack`/`replace` all work on single lines).
fn to_one_line(text: &str) -> String {
    let flattened: String = text
        .chars()
        .map(|c| if c == '\n' || c == '\r' || c == '\t' { ' ' } else { c })
        .collect();
    // Collapse the runs the flattening just made, so "a\n\nb" is "a b" rather than "a  b".
    let mut out = String::with_capacity(flattened.len());
    let mut last_was_space = false;
    for c in flattened.trim().chars() {
        if c == ' ' {
            if !last_was_space {
                out.push(c);
            }
            last_was_space = true;
        } else {
            out.push(c);
            last_was_space = false;
        }
    }
    out
}

wrap_task! {
    struct ClearMessage {
        sequence: u64,
    }

    impl Task {
        fn execute(&self) {
            // Only if nothing has been said since. Without this, three messages a second apart
            // would each take the next one's turn away.
            if SEQUENCE.load(Ordering::Relaxed) != self.sequence {
                return;
            }
            let had = match shown().lock() {
        Ok(mut shown) => shown.take().is_some(),
        Err(_) => false,
    };
            if had {
                crate::ipc::push_bar();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_to_say_is_null_and_not_an_empty_object() {
        // `state.message` is read with `if (state.message)` in bottom.js; an empty object there
        // would be truthy and would draw an empty coloured bar.
        *shown().lock().unwrap() = None;
        assert_eq!(json(), "null");
    }

    #[test]
    fn a_message_carries_its_level_and_is_escaped() {
        *shown().lock().unwrap() = Some(Shown {
            level: Level::Error,
            text: "he said \"no\"".to_string(),
        });
        assert_eq!(
            json(),
            "{\"level\":\"error\",\"text\":\"he said \\\"no\\\"\"}"
        );
        *shown().lock().unwrap() = None;
    }

    #[test]
    fn a_message_is_one_line_however_it_arrived() {
        assert_eq!(to_one_line("yanked"), "yanked");
        assert_eq!(to_one_line("  spaced  "), "spaced");
        assert_eq!(to_one_line("two\nlines"), "two lines");
        assert_eq!(to_one_line("blank\n\nline"), "blank line");
        assert_eq!(to_one_line("a\tb"), "a b");
        // A Rust error's Display is often several lines, and that is the common caller.
        assert_eq!(
            to_one_line("could not open\n  because: no such file\n"),
            "could not open because: no such file"
        );
    }

    #[test]
    fn the_three_levels_are_spelled_the_way_the_theme_spells_them() {
        // chrome.css reads `--messages-<name>-fg`; a fourth spelling here would be an unstyled bar.
        assert_eq!(Level::Info.name(), "info");
        assert_eq!(Level::Warning.name(), "warning");
        assert_eq!(Level::Error.name(), "error");
    }
}
