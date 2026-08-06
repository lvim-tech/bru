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

// -----------------------------------------------------------------------------------------------
// The page's console
// -----------------------------------------------------------------------------------------------

/// The one line `chrome/greasemonkey.js:264` writes when a userscript throws:
/// ``console.error(`bru: userscript ${bru_gm_name} threw: ${e}`)``.
///
/// Matched on the *source* first: `greasemonkey.rs::evaluate` now gives the injection a script URL
/// of `bru-userscript:<name>`, so the message carries one. The text match stays beside it, because
/// the two halves of this can land in separate commits and a gap between them loses the one console
/// line a userscript author needs.
///
/// **This is not a security boundary, and it was measured rather than assumed.** A page can put the
/// same string in its own source with `//# sourceURL=bru-userscript:…` inside an `eval` — measured
/// 2026-08-06, the forged source came back byte for byte. What the source match buys is that it
/// takes a deliberate act instead of a coincidence of wording; the worst either way is a wrong line
/// in the bar for three seconds. Anything that must be unforgeable cannot come from the console.
const USERSCRIPT_SOURCE: &str = "bru-userscript:";

/// The one line `chrome/greasemonkey.js:264` writes when a userscript throws. Kept as the second
/// half of the match above — the wrapper catches every userscript error itself, so `V8Exception`
/// never sees one and the console is the only channel there is.
const USERSCRIPT_THREW: &str = "bru: userscript ";

/// Where one console message goes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Route {
    /// Into the bar, at this level. Rare on purpose — see [`route`].
    Bar(Level),
    /// stderr only, where a terminal keeps it and a browsing session never sees it.
    Stderr,
    /// Nowhere. `BRU_DEBUG_CONSOLE=1` still prints it.
    Drop,
}

/// What to do with a `console.log`, `.warn` or `.error` from a page.
///
/// **The open web logs constantly, and a bar that flashes every warning is a bar the user learns
/// to ignore.** So this is deliberately almost entirely `Drop`, and it is qutebrowser's own default
/// rather than an invention: `content.javascript.log_message.levels` (configdata.yml:1011) ships as
///
/// ```text
/// "qute:*":         ["error"]
/// "userscript:GM-*": []
/// "userscript:*":   ["error"]
/// ```
///
/// — errors from qutebrowser's *own* pages and its *own* injected scripts reach the UI, a page's
/// errors never do, and third-party greasemonkey scripts are excluded by name. `content.javascript.log`
/// then sends all four levels to the `debug` logger, which is off screen.
///
/// bru transcribes that with two changes, both argued:
///
/// - `bru://*` stands for `qute:*`. If bru's own chrome throws, the bar is the only thing that can
///   say so, and the thing that broke is the UI itself.
/// - **The greasemonkey wrapper's own catch reaches the bar, where qutebrowser's `GM-*` line
///   excludes it.** qutebrowser can afford to exclude it because every message it drops is still in
///   `qute://log`, which `:messages` shows; bru has no log page and this module says so at the top,
///   so "excluded from the UI" here means "gone". And the wrapper's line is not a page's
///   `console.error`: it fires once, from a `try`/`catch` around a script the *user* installed, at
///   exactly the moment that script stops working. Measured below: neither of the two busiest pages
///   on this machine produced a single one.
pub fn route(level: LogSeverity, text: &str, source: &str) -> Route {
    // console.error and console.assert; FATAL is what an uncaught exception in a worker reports.
    let severe = level == LogSeverity::ERROR || level == LogSeverity::FATAL;
    if !severe {
        return Route::Drop;
    }
    if source.starts_with(USERSCRIPT_SOURCE)
        || text.starts_with(USERSCRIPT_THREW)
        || source.starts_with("bru://")
    {
        return Route::Bar(Level::Error);
    }
    Route::Stderr
}

/// `DisplayHandler::on_console_message`. Called on the UI thread, for every frame of every tab.
///
/// Always returns 0 — "not handled", so CEF carries on with its own logging. Measured 2026-08-06
/// with no handler at all: a full page load of vesti.bg put **zero** console lines on bru's stderr,
/// so there is nothing to suppress and returning 1 would only risk the DevTools console, which is
/// fed by a different path and is the one place a page's own log is worth reading in full.
pub fn on_console_message(
    level: LogSeverity,
    message: Option<&CefString>,
    source: Option<&CefString>,
    line: ::std::os::raw::c_int,
) -> ::std::os::raw::c_int {
    let text = message.map(CefString::to_string).unwrap_or_default();
    let source = source.map(CefString::to_string).unwrap_or_default();
    let route = route(level, &text, &source);

    if std::env::var_os("BRU_DEBUG_CONSOLE").is_some() {
        eprintln!(
            "bru[console]: {} {} [{source}:{line}] {}",
            match route {
                Route::Bar(_) => "bar",
                Route::Stderr => "stderr",
                Route::Drop => "drop",
            },
            severity_name(level),
            to_one_line(&text),
        );
    }

    match route {
        // qutebrowser prefixes the same way — `func(f"JS: {logstring}")` in shared.py's
        // `_js_log_to_ui` — so a message in the bar can never be mistaken for one of bru's own.
        Route::Bar(level) if bar_is_free() => show(level, &format!("JS: [{source}:{line}] {text}")),
        Route::Bar(_) => eprintln!("bru[console]: [{source}:{line}] {}", to_one_line(&text)),
        Route::Stderr => {
            if std::env::var_os("BRU_DEBUG_CONSOLE").is_none() {
                eprintln!("bru[console]: [{source}:{line}] {}", to_one_line(&text));
            }
        }
        Route::Drop => {}
    }
    0
}

/// True at most once per [`TIMEOUT_MS`], and the thing that stops a console message from being able
/// to hang bru.
///
/// **`bru://` is both a source this routes to the bar and the page the bar is drawn by.** A message
/// is shown by running `execute_java_script` in the chrome frame; if *that* script throws, the throw
/// is a `bru://` console error, which asks for a message, which runs the script again — an
/// unbounded loop with the UI thread in it. Nothing else in this file could break that cycle,
/// because the two ends are in different processes and neither knows about the other.
///
/// A rate limit breaks it and costs nothing real: the bar shows one line for three seconds, so a
/// second console message inside that window could only take the first one's place. It still goes
/// to stderr, which keeps every one of them.
fn bar_is_free() -> bool {
    use std::time::{Duration, Instant};
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);

    let Ok(mut last) = LAST.lock() else {
        return false;
    };
    let free = match *last {
        Some(at) => at.elapsed() >= Duration::from_millis(TIMEOUT_MS as u64),
        None => true,
    };
    if free {
        *last = Some(Instant::now());
    }
    free
}

fn severity_name(level: LogSeverity) -> &'static str {
    match level {
        LogSeverity::VERBOSE => "verbose",
        LogSeverity::INFO => "info",
        LogSeverity::WARNING => "warning",
        LogSeverity::ERROR => "error",
        LogSeverity::FATAL => "fatal",
        _ => "default",
    }
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

    /// The whole of the console policy, stated as the four cases it has.
    #[test]
    fn only_brus_own_javascript_reaches_the_bar() {
        // A page's errors — the common case, and the one that must never flash the bar.
        assert_eq!(
            route(LogSeverity::ERROR, "Uncaught TypeError: x", "https://vesti.bg/ad.js"),
            Route::Stderr
        );
        // Everything quieter than an error is gone, whoever logged it. This is the clause that
        // makes the open web survivable: warnings outnumber errors on every page measured.
        for level in [LogSeverity::VERBOSE, LogSeverity::INFO, LogSeverity::WARNING] {
            assert_eq!(route(level, "third-party cookie", "https://vesti.bg/"), Route::Drop);
            assert_eq!(route(level, "", "bru://chrome/bottom.html"), Route::Drop);
        }
        // bru's own chrome. If this throws the UI is what broke, and the bar is all there is.
        assert_eq!(
            route(LogSeverity::ERROR, "Uncaught ReferenceError", "bru://chrome/bottom.html"),
            Route::Bar(Level::Error)
        );
        // The greasemonkey wrapper's catch, wherever the page it ran on was.
        assert_eq!(
            route(
                LogSeverity::ERROR,
                "bru: userscript example.user.js threw: TypeError: undefined",
                "https://vesti.bg/"
            ),
            Route::Bar(Level::Error)
        );
    }

    /// The prefix this file matches on is the one the wrapper actually writes. Without this the
    /// two could drift apart in silence and a broken userscript would go back to being invisible.
    #[test]
    fn the_wrapper_still_writes_the_line_this_module_looks_for() {
        let wrapper = include_str!("../chrome/greasemonkey.js");
        assert!(
            wrapper.contains("console.error(`bru: userscript ${bru_gm_name} threw:"),
            "chrome/greasemonkey.js no longer starts its catch with {USERSCRIPT_THREW:?}"
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
