//! Prompt mode: the two modes in which **bru is asking and the browser is waiting**.
//!
//! A behavioural port of qutebrowser 3.7.0's `mainwindow/prompt.py`, with its `PromptQueue` and its
//! five `prompt-*` commands. Everything that a page, the network or a download wants to be asked
//! comes through [`ask`]; everything the user can answer with comes through [`run_command`].
//!
//! ## The shape, and why it is not qutebrowser's
//!
//! qutebrowser draws a prompt as a widget stack above the status bar. bru's chrome is an HTML page
//! (DESIGN.md), so the prompt is a block inside `chrome/bottom.html` and the bottom strip grows to
//! hold it — the same mechanism the completion table already uses, through
//! `window::set_prompt_height` beside `set_completion_height`. It is a second *slot*, not a second
//! mechanism: the two are added together in `BruChromeViewDelegate::preferred_size`, so a prompt
//! raised while the completion is open cannot be drawn inside the completion's height.
//!
//! ## Modal, and what that means when it is done carelessly
//!
//! A prompt blocks the **mode**, never the process — `modeman`'s arrangement and the only safe one
//! for a keyboard browser. Concretely, and each of these is a decision rather than a side effect:
//!
//! - **`:` while a prompt is open does nothing.** `cmd-set-text` is a normal-mode binding and
//!   prompt mode does not bind it, so the key never reaches it. `<Escape>` is always bound, in both
//!   prompt modes, and always leaves — a question can be walked away from, and walking away is
//!   *cancel*, not a default answer. `mode-leave` is the one guaranteed exit and it is the reason a
//!   prompt cannot wedge the browser.
//! - **A second prompt queues; it never replaces the first.** `ModeManager::enter` ignores a prompt
//!   mode requested while another prompt mode is current (`modeman.py:365-366`), and [`ask`] puts
//!   the new question at the back of that window's queue. This is not tidiness: without it the
//!   keystroke the user had already decided on for question one answers question two.
//! - **The tab that raised it closing cancels it.** CEF says so itself through
//!   `on_reset_dialog_state`, which fires when a browser navigates away from or discards the page
//!   that opened the dialog; [`on_reset_dialog_state`] cancels every question that browser raised
//!   and moves on to the next in the queue.
//! - **A background window's question is asked in that window.** bru keeps a mode per window and a
//!   hint session per window, so a prompt is per window too: the queue below is keyed by window,
//!   the mode change is `enter_mode_in(window, …)`, and the bar it is drawn in is that window's.
//!   The window the user is typing in never changes mode because a page in another window called
//!   `confirm()`. qutebrowser is the other way round here — one global queue shown in every window
//!   — and that is a consequence of its prompts being global widgets, not a property worth copying.
//!
//! ## What a page's own text may not do
//!
//! `on_jsdialog` is a page asking bru to draw something with the page's words in it, and those
//! words are hostile input. `src/message.rs` already paid for this lesson once: a userscript's
//! console line reached the bar by matching on wording, and a page could forge the wording. So:
//!
//! 1. **The title is bru's, never the page's.** "JavaScript confirm", "Permission request",
//!    "Authentication required" — one of a fixed list in [`Kind::title`], chosen by which callback
//!    fired. A page cannot supply, extend or suppress it.
//! 2. **The origin is bru's too**, taken from the CEF callback's `origin_url` and drawn in its own
//!    element. It is not concatenated into the page's string, so no amount of punctuation in the
//!    page's text can make it look like a different site is asking.
//! 3. **The page's text is one line, capped, and drawn with `textContent`.** [`one_line`] flattens
//!    every newline, tab and carriage return, so a page cannot draw extra rows; [`MAX_TEXT`] caps
//!    the length, so it cannot push the strip to its 300 px ceiling and hide the key hints under it;
//!    and `chrome/prompt.js` never touches `innerHTML`, so it cannot contribute markup. qutebrowser
//!    deliberately does *not* escape prompt text (`prompt.py:539-540`, "the text can be formatted")
//!    — it can afford to, because its callers are all internal. bru's caller here is a web page.
//! 4. **The key hints are built in Rust from the live binding table.** A page cannot add a row that
//!    says `<Return>  Trust this certificate` to a question that has no such answer.
//!
//! What is left, and is worth saying out loud: a page can still put arbitrary words in the bar, and
//! those words can *claim* to be bru. Nothing can prevent that — an alert is a channel for text by
//! definition. What the four rules above buy is that the claim never gets bru's own frame around it:
//! the title above it, the origin beside it and the keys below it are all bru's, and all three
//! disagree with a page pretending to be a browser.

use cef::*;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Mutex;

use crate::cmdline::CmdLine;
use crate::commands::{Command, FocusWhich};
use crate::modes::Mode;

/// How much of a page's own string is drawn. Longer than any honest `confirm()` and short enough
/// that the block below it cannot be pushed out of the strip.
const MAX_TEXT: usize = 400;

/// How many rows of a filename prompt's directory listing are drawn at once.
const MAX_ITEMS: usize = 6;

/// One row of the prompt block, in logical pixels. `chrome/chrome.css`'s `--row-h`, the same
/// constant `completers::resize_bar` counts with, and the reason the arithmetic in [`wanted_height`]
/// can be exact rather than a guess.
const ROW_H: i32 = 20;

/// The tallest the prompt block may make the strip, matching `--completion-max-h`. Past it the
/// block scrolls inside itself rather than eating the page.
const MAX_H: i32 = 300;

// -----------------------------------------------------------------------------------------------
// What can be asked
// -----------------------------------------------------------------------------------------------

/// The five prompt shapes qutebrowser has (`prompt.py:314-320`), and bru has the same five.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// `alert()`. Nothing to answer; `<Return>` dismisses it. `AlertPrompt`.
    Alert,
    /// `confirm()`, "leave site?", a permission, a certificate. `YesNoPrompt`.
    YesNo,
    /// `prompt()`. One line of text. `LineEditPrompt`.
    Text,
    /// Where a download goes. A line of text over a path, with the directory listed under it.
    /// `DownloadFilenamePrompt`.
    Download,
    /// HTTP basic auth: two lines, the second of them starred. `AuthenticationPrompt`.
    UserPwd,
}

impl Kind {
    /// The key mode this shape is answered in. Only [`Kind::YesNo`] is `yesno`; every other shape
    /// has something to type into, which is what `prompt` mode's `passthrough=True` is for.
    pub fn mode(self) -> Mode {
        match self {
            Kind::YesNo => Mode::YesNo,
            Kind::Alert | Kind::Text | Kind::Download | Kind::UserPwd => Mode::Prompt,
        }
    }

    /// Whether this shape has a line to type into. `alert` and `yesno` do not, and that is what
    /// `keys.rs` asks before letting a letter through to the bar's input.
    fn has_input(self) -> bool {
        matches!(self, Kind::Text | Kind::Download | Kind::UserPwd)
    }

    fn name(self) -> &'static str {
        match self {
            Kind::Alert => "alert",
            Kind::YesNo => "yesno",
            Kind::Text => "text",
            Kind::Download => "download",
            Kind::UserPwd => "user_pwd",
        }
    }
}

/// What the answer was. Handed to the question's own callback, which is the only thing that knows
/// what to do with it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Answer {
    /// `<Escape>`, the tab going away, the window closing. Never a default: a question walked away
    /// from is a question that was not answered, and every caller below treats it as the safe side.
    Cancelled,
    Yes,
    No,
    Text(String),
    UserPwd { user: String, password: String },
    /// A download's filename question answered with "somewhere temporary, and open it when it
    /// lands". `cmdline` is `prompt-open-download`'s argument.
    OpenDownload { cmdline: Option<String> },
}

/// One question, and the one callback that closes it.
pub struct Question {
    pub kind: Kind,
    /// bru's own words. Never a page's — see the module docs.
    pub title: &'static str,
    /// Who is asking, as bru resolved it. Drawn in its own element.
    pub origin: String,
    /// The page's own string, or bru's explanation for a question no page asked. Flattened and
    /// capped by [`ask`]; nothing here trusts it.
    pub text: String,
    /// What the line edit starts with.
    pub default: String,
    /// The answer `<Return>` means for a yes/no question, or `None` for one that insists on `y` or
    /// `n`. `YesNoPrompt.accept` raises "No default value was set for this question!" for the
    /// second case (`prompt.py:975-977`) and so does [`accept_open`].
    pub default_yes: Option<bool>,
    /// What `prompt-yank` copies — the one thing a modal question otherwise puts out of reach.
    pub url: String,
    /// The browser that raised it, so that browser going away can cancel it.
    pub browser: i32,
    answer: Option<Box<dyn FnOnce(Answer) + Send>>,
}

impl Question {
    pub fn new(kind: Kind, title: &'static str, answer: impl FnOnce(Answer) + Send + 'static) -> Question {
        Question {
            kind,
            title,
            origin: String::new(),
            text: String::new(),
            default: String::new(),
            default_yes: None,
            url: String::new(),
            browser: 0,
            answer: Some(Box::new(answer)),
        }
    }

    pub fn about(mut self, browser: Option<&Browser>, origin: &str) -> Question {
        self.browser = browser.map(Browser::identifier).unwrap_or(0);
        self.origin = one_line(origin, MAX_TEXT);
        self.url = origin.to_string();
        self
    }

    pub fn saying(mut self, text: &str) -> Question {
        self.text = one_line(text, MAX_TEXT);
        self
    }

    pub fn defaulting_to(mut self, default: &str) -> Question {
        self.default = default.to_string();
        self
    }

    pub fn default_answer(mut self, yes: bool) -> Question {
        self.default_yes = Some(yes);
        self
    }
}

/// One line, capped, with the runs the flattening made collapsed — `message::to_one_line` with a
/// length limit bolted on, and here rather than shared because the input there is bru's own and the
/// input here is a web page's.
fn one_line(text: &str, max: usize) -> String {
    let mut out = String::with_capacity(text.len().min(max + 1));
    let mut last_was_space = false;
    for c in text.chars() {
        let c = if c.is_control() || c == '\u{2028}' || c == '\u{2029}' { ' ' } else { c };
        if c == ' ' {
            if !last_was_space && !out.is_empty() {
                out.push(c);
            }
            last_was_space = true;
        } else {
            out.push(c);
            last_was_space = false;
        }
        if out.chars().count() >= max {
            out.push('…');
            break;
        }
    }
    out.trim_end().to_string()
}

// -----------------------------------------------------------------------------------------------
// The queue — one per window
// -----------------------------------------------------------------------------------------------

/// The question that is being answered, and the state of answering it.
struct Open {
    question: Question,
    /// The line edit. `CmdLine` is reused whole rather than reimplemented: it is a plain
    /// CEF-free editor with every readline operation on it, and its history half simply goes
    /// unused here. That is also what makes the fifteen `rl-*` bindings in `bindings.default.prompt`
    /// mean the same thing they mean in command mode — see [`run_readline`].
    line: CmdLine,
    /// The second field of a login. `on_password` says which of the two the keys are going to.
    password: CmdLine,
    on_password: bool,
    /// A filename prompt's directory listing, and the row `prompt-item-focus` is on.
    items: Vec<String>,
    selected: Option<usize>,
    /// The key hints, resolved against the live binding table **once**, when the question opened.
    ///
    /// Not on every push: `Bindings` is behind `BruState` and reading it means cloning the whole
    /// 298-row table, and a push happens on every keystroke typed into the prompt's own line. What
    /// a key is bound to cannot change while a question is open — `:bind` is not reachable from
    /// prompt mode — so once is also correct and not only cheap.
    keys: Vec<(String, &'static str)>,
}

impl Open {
    fn field(&mut self) -> &mut CmdLine {
        if self.on_password {
            &mut self.password
        } else {
            &mut self.line
        }
    }
}

struct Windowed {
    window: u32,
    open: Option<Open>,
    queue: VecDeque<Question>,
}

fn prompts() -> &'static Mutex<Vec<Windowed>> {
    static PROMPTS: Mutex<Vec<Windowed>> = Mutex::new(Vec::new());
    &PROMPTS
}

fn with_window<R>(window: u32, f: impl FnOnce(&mut Windowed) -> R) -> Option<R> {
    let mut prompts = prompts().lock().ok()?;
    if !prompts.iter().any(|entry| entry.window == window) {
        prompts.push(Windowed { window, open: None, queue: VecDeque::new() });
    }
    let entry = prompts.iter_mut().find(|entry| entry.window == window)?;
    Some(f(entry))
}

/// `BRU_DEBUG_PROMPT=1` traces every question, answer and queue move.
fn debug(message: &str) {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("BRU_DEBUG_PROMPT").is_some()) {
        eprintln!("bru[prompt]: {message}");
    }
}

/// Ask a question in one window. Runs on the UI thread.
///
/// If that window already has one open the new question joins the back of its queue, exactly as
/// `PromptQueue.ask_question` does for a non-blocking question (`prompt.py:151-156`). bru has no
/// *blocking* case at all — qutebrowser's spins a nested `QEventLoop`, and every one of bru's
/// callers is a CEF callback that is allowed to be answered later instead.
pub fn ask(window: u32, question: Question) {
    debug(&format!(
        "ask window {window} {:?} {:?} {:?}",
        question.kind, question.title, question.text
    ));
    let show = with_window(window, |entry| {
        if entry.open.is_some() {
            entry.queue.push_back(question);
            return false;
        }
        entry.open = Some(open_of(question));
        true
    });
    if show == Some(true) {
        enter_mode(window);
    }
}

/// The editing state a question starts in.
fn open_of(question: Question) -> Open {
    let mut line = CmdLine::new();
    line.set_text(&question.default);
    let items = if question.kind == Kind::Download {
        list_dir(&question.default)
    } else {
        Vec::new()
    };
    let keys = key_hints(&question);
    Open {
        question,
        line,
        password: CmdLine::new(),
        on_password: false,
        items,
        selected: None,
        keys,
    }
}

/// Put a window into the mode the open question is answered in, and draw it.
fn enter_mode(window: u32) {
    let Some(kind) = with_window(window, |entry| entry.open.as_ref().map(|open| open.question.kind))
        .flatten()
    else {
        return;
    };
    let Some(state) = crate::state::BruState::instance() else {
        return;
    };
    let entered = state
        .lock()
        .expect("state mutex poisoned")
        .enter_mode_in(window, kind.mode(), false);
    if entered {
        // The one funnel every mode change goes through, so the bar recolours, the command line is
        // cleared and `on_mode_changed` below sees it.
        crate::ipc::set_mode_for(window, kind.mode().name().to_string());
        // And the strip takes the keyboard. **Unconditionally, for every shape** — not only for
        // the three with an `<input>`, which is what this said first and what a whole afternoon
        // was spent finding out is wrong.
        //
        // Measured 2026-08-07 on a real `confirm()`: while a renderer is blocked in a JS dialog,
        // Chromium delivers **no key events at all** to that browser. `send_key_event` at the
        // page's host produced not one call to `on_pre_key_event` — `BRU_DEBUG_KEYS=1` printed
        // nothing — and the same key sent at the bottom strip's host answered the prompt on the
        // first try. So a question drawn while the page view holds focus is a question the
        // keyboard cannot reach, and `y` looks like a key that has stopped working.
        //
        // It is also **after** `set_mode_for`, because `cmdline::on_mode_changed` runs inside it
        // and gives the focus back to the page when it clears a command line that had text in it —
        // a question raised over a half-typed `:open` would otherwise open unanswerable.
        //
        // Nothing has to give the focus back: `keys.rs` aims every key that lands on a chrome
        // strip at the showing tab anyway (trap 11), so the strip keeping it after the answer
        // costs nothing.
        crate::ipc::focus_bottom_chrome(window);
    } else {
        // The window was already in the other prompt mode, which `ModeManager::enter` refuses. That
        // cannot happen from `ask` — a window with an open prompt queues instead — but it can from
        // the queue drain, and a push is still owed so the new question is drawn.
        crate::ipc::push_bar();
    }
}

/// Every mode change in a window passes here, from `ipc::set_mode_for`.
///
/// This is the whole of "a prompt can be left". Whatever takes the mode away from the open
/// question — `<Escape>`, a page focusing a field, a command — cancels it, which is
/// `PromptQueue._on_mode_left` (`prompt.py:195-211`). The question is taken out *before* the
/// callback runs, so an answer that changes the mode itself cannot re-enter this and cancel the
/// question it has just answered.
pub fn on_mode_changed(window: u32, mode: &str) {
    let still_prompting = matches!(mode, "prompt" | "yesno");
    let cancelled = with_window(window, |entry| {
        if still_prompting {
            return None;
        }
        entry.open.take().map(|open| open.question)
    })
    .flatten();
    if let Some(question) = cancelled {
        debug(&format!("window {window} left the mode; cancelling {:?}", question.title));
        deliver(question, Answer::Cancelled);
    }
    if !still_prompting {
        schedule_pop(window);
    }
}

/// Drop a window's questions. From `ipc::forget_window`; a push into a window that has gone would
/// run a script in a dead renderer, and a question nobody can answer must not keep a page waiting.
pub fn forget_window(window: u32) {
    let left = match prompts().lock() {
        Ok(mut prompts) => {
            let mut left: Vec<Question> = Vec::new();
            prompts.retain_mut(|entry| {
                if entry.window != window {
                    return true;
                }
                left.extend(entry.open.take().map(|open| open.question));
                left.extend(entry.queue.drain(..));
                false
            });
            left
        }
        Err(_) => return,
    };
    for question in left {
        deliver(question, Answer::Cancelled);
    }
}

/// Cancel every question a browser raised — a tab that navigated away from or discarded the page
/// that asked. CEF's own `on_reset_dialog_state` is what says so.
fn cancel_for_browser(id: i32) {
    let (windows, cancelled) = match prompts().lock() {
        Ok(mut prompts) => {
            let mut windows = Vec::new();
            let mut cancelled = Vec::new();
            for entry in prompts.iter_mut() {
                if entry.open.as_ref().is_some_and(|open| open.question.browser == id) {
                    cancelled.extend(entry.open.take().map(|open| open.question));
                    windows.push(entry.window);
                }
                while let Some(at) = entry.queue.iter().position(|q| q.browser == id) {
                    cancelled.extend(entry.queue.remove(at));
                }
            }
            (windows, cancelled)
        }
        Err(_) => return,
    };
    for question in cancelled {
        deliver(question, Answer::Cancelled);
    }
    for window in windows {
        // Leaving the mode also runs `on_mode_changed`, which finds nothing open and pops the next.
        leave_mode(window);
    }
}

/// Take a window out of prompt mode. The one exit every answer and every cancellation shares.
fn leave_mode(window: u32) {
    let Some(state) = crate::state::BruState::instance() else {
        return;
    };
    let now = {
        let mut state = state.lock().expect("state mutex poisoned");
        if !state.mode_in(window).is_prompt() {
            return;
        }
        state.leave_mode_in(window);
        state.mode_in(window)
    };
    // `Mode::leave` restores insert or passthrough when the question interrupted one, so this is
    // not always `normal` — see `modes::ModeManager::leave`.
    crate::ipc::set_mode_for(window, now.name().to_string());
}

// -----------------------------------------------------------------------------------------------
// Answers leave on the next turn of the loop
// -----------------------------------------------------------------------------------------------

/// Answers waiting to be handed to their callbacks.
///
/// **Nothing is delivered inline**, and that is CEF-NOTES trap 12. A jsdialog callback answered
/// `true` for `onbeforeunload` starts the navigation that was waiting on it, and `prompt-accept`
/// can be reached from `chrome/prompt.js`'s answer — i.e. from inside a message-router query
/// handler, which is precisely where a navigation deadlocks. Posting also keeps this file's
/// bookkeeping callable from a unit test: [`take_answer`] is the half the tests exercise, and only
/// [`deliver`] posts (trap 13).
static PENDING: Mutex<Vec<(Box<dyn FnOnce(Answer) + Send>, Answer)>> = Mutex::new(Vec::new());

/// Windows owed a look at their queue once the current answer is out of the way. `_pop_later`
/// (`prompt.py:100-102`), and scheduled rather than immediate for qutebrowser's own reason: the
/// order of what happens stays clear, and the mode change of the question being finished cannot
/// land after the next question has already set one up.
static POP: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// The bookkeeping half of an answer: take the callback out of the question. Pure.
fn take_answer(mut question: Question) -> Option<Box<dyn FnOnce(Answer) + Send>> {
    question.answer.take()
}

/// Hand an answer to its callback, on the next turn of the UI loop.
fn deliver(question: Question, answer: Answer) {
    let Some(callback) = take_answer(question) else {
        return;
    };
    if let Ok(mut pending) = PENDING.lock() {
        pending.push((callback, answer));
    }
    let mut task = RunPending::new();
    post_task(ThreadId::UI, Some(&mut task));
}

fn schedule_pop(window: u32) {
    if let Ok(mut pop) = POP.lock() {
        if !pop.contains(&window) {
            pop.push(window);
        }
    }
    let mut task = RunPending::new();
    post_task(ThreadId::UI, Some(&mut task));
}

wrap_task! {
    struct RunPending;

    impl Task {
        fn execute(&self) {
            let answers: Vec<(Box<dyn FnOnce(Answer) + Send>, Answer)> = match PENDING.lock() {
                Ok(mut pending) => std::mem::take(&mut *pending),
                Err(_) => Vec::new(),
            };
            for (callback, answer) in answers {
                debug(&format!("answering {answer:?}"));
                callback(answer);
            }

            let windows: Vec<u32> = match POP.lock() {
                Ok(mut pop) => std::mem::take(&mut *pop),
                Err(_) => Vec::new(),
            };
            for window in windows {
                let popped = with_window(window, |entry| {
                    if entry.open.is_some() {
                        return false;
                    }
                    match entry.queue.pop_front() {
                        Some(question) => {
                            entry.open = Some(open_of(question));
                            true
                        }
                        None => false,
                    }
                });
                if popped == Some(true) {
                    enter_mode(window);
                }
            }
        }
    }
}

// -----------------------------------------------------------------------------------------------
// What the chrome is handed
// -----------------------------------------------------------------------------------------------

/// The `prompt` key of one window's bottom-view state, and the only caller is `ipc::bar_json_for`.
///
/// It also sets that window's share of the strip height, the way `completers::json_for` does: the
/// two are separate slots in `window.rs` and are added, so neither can be drawn inside the other's
/// space.
pub fn json_for(window: u32) -> String {
    let json = with_window(window, |entry| match entry.open.as_ref() {
        Some(open) => Some(json_of(open, entry.queue.len())),
        None => None,
    })
    .flatten();
    let Some((json, rows)) = json else {
        resize_bar(window, 0);
        return "null".to_string();
    };
    resize_bar(window, rows);
    json
}

/// The JSON and the number of rows it will draw. Split so the arithmetic is exact rather than a
/// guess: every element `chrome/prompt.js` emits is one `--row-h` row, and the two blocks that
/// could be any height — a page's text and a directory listing — are given a fixed allowance and
/// scroll inside it.
fn json_of(open: &Open, queued: usize) -> (String, i32) {
    let q = &open.question;
    let e = crate::ipc::json_escape;

    let mut fields = String::from("[");
    if q.kind == Kind::UserPwd {
        fields.push_str(&format!(
            "{{\"label\":\"Username:\",\"value\":\"{}\",\"cursor\":{},\"secret\":false,\"focused\":{}}},",
            e(&open.line.text()),
            open.line.cursor_utf16(),
            !open.on_password
        ));
        fields.push_str(&format!(
            "{{\"label\":\"Password:\",\"value\":\"{}\",\"cursor\":{},\"secret\":true,\"focused\":{}}}",
            e(&open.password.text()),
            open.password.cursor_utf16(),
            open.on_password
        ));
    } else if q.kind.has_input() {
        fields.push_str(&format!(
            "{{\"label\":\"\",\"value\":\"{}\",\"cursor\":{},\"secret\":false,\"focused\":true}}",
            e(&open.line.text()),
            open.line.cursor_utf16(),
        ));
    }
    fields.push(']');

    let shown: Vec<&String> = open.items.iter().take(MAX_ITEMS).collect();
    let items = shown
        .iter()
        .map(|item| format!("\"{}\"", e(item)))
        .collect::<Vec<_>>()
        .join(",");

    let keys = open
        .keys
        .iter()
        .map(|(key, what)| format!("[\"{}\",\"{}\"]", e(key), e(what)))
        .collect::<Vec<_>>()
        .join(",");

    let field_rows = if q.kind == Kind::UserPwd {
        2
    } else if q.kind.has_input() {
        1
    } else {
        0
    };
    // title + origin + the text's own allowance + fields + listing + one row per key hint.
    let rows = 2
        + if q.text.is_empty() { 0 } else { TEXT_ROWS }
        + field_rows
        + shown.len() as i32
        + open.keys.len() as i32;

    let json = format!(
        "{{\"kind\":\"{}\",\"title\":\"{}\",\"origin\":\"{}\",\"text\":\"{}\",\
          \"fields\":{fields},\"items\":[{items}],\"selected\":{},\"keys\":[{keys}],\
          \"queued\":{queued},\"rev\":{}}}",
        q.kind.name(),
        e(q.title),
        e(&q.origin),
        e(&q.text),
        match open.selected {
            Some(at) if at < MAX_ITEMS => at as i64,
            _ => -1,
        },
        open.line.rev() + open.password.rev(),
    );
    (json, rows)
}

/// How many rows a page's string is allowed. It scrolls inside them — see `chrome/chrome.css` — so
/// this is a fixed allowance and not an estimate of how tall the text really is, which cannot be
/// computed on this side without knowing the strip's width.
const TEXT_ROWS: i32 = 3;

/// The keys this question can be answered with, and what each does — `_allowed_commands`
/// (`prompt.py:608-1046`), with the key each is bound to looked up in the live table.
///
/// **Built here and never by the chrome.** The rows are a promise about what the keyboard will do;
/// a page that could add one could promise anything.
fn key_hints(q: &Question) -> Vec<(String, &'static str)> {
    let mut cmds: Vec<(&str, &'static str)> = Vec::new();
    match q.kind {
        Kind::Alert => cmds.push(("prompt-accept", "Hide")),
        Kind::YesNo => {
            cmds.push(("prompt-accept yes", "Yes"));
            cmds.push(("prompt-accept no", "No"));
            if q.default_yes.is_some() {
                cmds.push(("prompt-accept", "Use the default"));
            }
            cmds.push(("mode-leave", "Abort"));
            cmds.push(("prompt-yank", "Yank URL"));
        }
        Kind::Download => {
            cmds.push(("prompt-accept", "Accept"));
            cmds.push(("mode-leave", "Abort"));
            cmds.push(("rl-filename-rubout", "Go to parent directory"));
            cmds.push(("prompt-open-download", "Open download"));
            cmds.push(("prompt-yank", "Yank URL"));
            cmds.push(("prompt-fileselect-external", "Launch external file selector"));
        }
        Kind::Text | Kind::UserPwd => {
            cmds.push(("prompt-accept", "Accept"));
            cmds.push(("mode-leave", "Abort"));
        }
    }
    let mode = q.kind.mode();
    // The live table, read once per question. `None` in a unit test and before startup finishes,
    // which `binding_in` turns into the same "unbound" row qutebrowser draws.
    let table = crate::state::BruState::instance()
        .and_then(|state| state.lock().ok().and_then(|state| state.bindings_snapshot()))
        .map(|bindings| bindings.all())
        .unwrap_or_default();
    cmds.into_iter()
        .map(|(cmd, what)| (binding_in(&table, mode, cmd), what))
        .collect()
}

/// The key `cmd` is bound to in `mode`, preferring `<Return>` and `<Escape>` when it is bound to
/// several — `_init_key_label` (`prompt.py:550-568`), including its "unbound" case, because a
/// `config.lua` that unbinds `y` must say so in the one place the user is looking.
///
/// Built from the *live* table and never from `DEFAULT_BINDINGS`, for `help.rs`'s reason: a hint
/// that can disagree with the keyboard is worse than no hint.
fn binding_in(table: &[(Mode, String, String)], mode: Mode, cmd: &str) -> String {
    let found: Vec<&String> = table
        .iter()
        .filter(|(bound_mode, _, bound)| *bound_mode == mode && bound == cmd)
        .map(|(_, keys, _)| keys)
        .collect();
    for preferred in ["<Return>", "<Escape>"] {
        if let Some(key) = found.iter().find(|key| key.eq_ignore_ascii_case(preferred)) {
            return (*key).clone();
        }
    }
    match found.first() {
        Some(key) => (*key).clone(),
        None => format!("unbound ({cmd})"),
    }
}

/// Ask one window's bottom strip to be as tall as the prompt it is drawing.
///
/// Deliberately the same shape as `completers::resize_bar`, and deliberately a *different* slot in
/// `window.rs`: the two were one slot in the first version of this, and a prompt raised while the
/// completion table was open was drawn inside whichever of the two had written the height last.
fn resize_bar(window: u32, rows: i32) {
    let wanted = wanted_height(rows);
    let was = crate::window::set_prompt_height(window, wanted);
    if was == wanted {
        return;
    }
    let Some(mut browser) = crate::ipc::bottom_chrome_browser_for(window) else {
        return;
    };
    let Some(view) = browser_view_get_for_browser(Some(&mut browser)) else {
        return;
    };
    View::from(&view).invalidate_layout();
    let handle = crate::state::BruState::instance()
        .and_then(|state| state.lock().ok().and_then(|state| state.window_handle(window)));
    if let Some(handle) = handle {
        View::from(&handle).invalidate_layout();
    }
}

/// Split out of [`resize_bar`] so the arithmetic can be tested without a window: everything else in
/// that function is a CEF call.
fn wanted_height(rows: i32) -> i32 {
    if rows <= 0 {
        0
    } else {
        // One pixel for `#prompt`'s own border, exactly as the completion table counts it.
        (ROW_H * rows).min(MAX_H) + 1
    }
}

/// Whether this window's open question has a line to type into — trap 11's exception in `keys.rs`
/// asks it before letting a letter through to the bottom strip.
pub fn takes_typing(window: u32) -> bool {
    with_window(window, |entry| {
        entry.open.as_ref().is_some_and(|open| open.question.kind.has_input())
    })
    .unwrap_or(false)
}

/// What the chrome says its input holds, from `{"type":"prompt-text"}`. The mirror, exactly like
/// the command line's: one IPC hop behind, and never what an answer is built from.
pub fn on_text_changed(window: u32, text: &str, cursor: Option<usize>) {
    with_window(window, |entry| {
        if let Some(open) = entry.open.as_mut() {
            open.field().sync(text, cursor);
        }
    });
}

// -----------------------------------------------------------------------------------------------
// The five commands
// -----------------------------------------------------------------------------------------------

/// Run one of this module's commands. `false` means it is not one of ours — the shape
/// `completers::run_command` has, called from the one arm `exec::run` gives it.
pub fn run_command(command: &Command) -> bool {
    let Some(window) = current_window() else {
        return matches!(
            command,
            Command::PromptAccept { .. }
                | Command::PromptItemFocus { .. }
                | Command::PromptOpenDownload { .. }
                | Command::PromptYank { .. }
                | Command::PromptFileselectExternal
        );
    };
    match command {
        Command::PromptAccept { value, save } => {
            accept(window, value.as_deref(), *save);
            true
        }
        Command::PromptItemFocus { which } => {
            item_focus(window, *which);
            true
        }
        Command::PromptOpenDownload { cmdline, pdfjs } => {
            open_download(window, cmdline.as_deref(), *pdfjs);
            true
        }
        Command::PromptYank { sel } => {
            yank(window, *sel);
            true
        }
        Command::PromptFileselectExternal => {
            fileselect_external(window);
            true
        }
        _ => false,
    }
}

fn current_window() -> Option<u32> {
    crate::state::BruState::instance()
        .and_then(|state| state.lock().ok().and_then(|state| state.current_window_id()))
}

/// The `rl-*` bindings of `bindings.default.prompt`, routed here by `cmdline::run_named_inner`.
///
/// Fifteen of prompt mode's twenty-six bindings are readline commands, and qutebrowser registers
/// every one of them for `command` **and** `prompt` mode (`commands/cmdutils` in
/// `misc/cmdhistory`'s `_ReadlineBridge`). They edit the same kind of line here as there, so they
/// go through the same `CmdLine` — the alternative was a second readline implementation, which is a
/// second place for `rl-filename-rubout` to disagree with itself.
///
/// `false` means "not a prompt's business", and the command line takes it as usual.
pub fn run_readline(name: &str, arg: &str) -> bool {
    let Some(window) = current_window() else {
        return false;
    };
    let changed = with_window(window, |entry| {
        let Some(open) = entry.open.as_mut() else {
            return false;
        };
        if !open.question.kind.has_input() {
            return false;
        }
        let field = open.field();
        match name {
            "rl-backward-char" => field.backward_char(),
            "rl-forward-char" => field.forward_char(),
            "rl-backward-word" => field.backward_word(),
            "rl-forward-word" => field.forward_word(),
            "rl-beginning-of-line" => field.beginning_of_line(),
            "rl-end-of-line" => field.end_of_line(),
            "rl-unix-line-discard" => field.unix_line_discard(),
            "rl-kill-line" => field.kill_line(),
            "rl-rubout" => field.rubout(arg),
            "rl-filename-rubout" => field.filename_rubout(),
            "rl-unix-word-rubout" => field.rubout(" "),
            "rl-unix-filename-rubout" => field.rubout(" /"),
            "rl-backward-kill-word" => field.backward_kill_word(),
            "rl-kill-word" => field.kill_word(),
            "rl-yank" => field.yank(),
            "rl-delete-char" => field.delete_char(),
            "rl-backward-delete-char" => field.backward_delete_char(),
            // The two history commands. A prompt has no history to walk, and qutebrowser's are
            // registered for command mode alone — so they are not claimed here and fall through to
            // the command line, which ignores them outside its own mode.
            _ => return false,
        }
        // A filename prompt's listing follows the line it is completing.
        if open.question.kind == Kind::Download {
            let text = open.line.text();
            open.items = list_dir(&text);
            open.selected = None;
        }
        true
    })
    .unwrap_or(false);
    if changed {
        crate::ipc::push_bar();
    }
    changed
}

/// `prompt-accept [--save] [<value>]`.
///
/// With no value and a line to read, the *chrome* is asked what that line holds and the work
/// continues in [`on_accept`] — the same round trip `command-accept` makes, for the same reason:
/// the mirror this side keeps is one IPC hop behind, and `<Return>` reaches the browser process
/// directly while the last letter typed is still travelling renderer → browser. Reading the mirror
/// would lose that letter, which for a filename prompt means writing the file somewhere else.
pub fn accept(window: u32, value: Option<&str>, save: bool) {
    let ask_the_chrome = value.is_none()
        && !save
        && with_window(window, |entry| {
            entry.open.as_ref().is_some_and(|open| open.question.kind.has_input())
        })
        .unwrap_or(false);
    if ask_the_chrome {
        crate::ipc::ask_prompt(window, "promptAccept");
        return;
    }
    accept_now(window, value, save);
}

/// The authoritative text, in answer to `bru.promptAccept()`. Nothing else may finish a prompt
/// that has a line in it.
pub fn on_accept(window: u32, text: &str) {
    with_window(window, |entry| {
        if let Some(open) = entry.open.as_mut() {
            open.field().set_text(text);
        }
    });
    accept_now(window, None, false);
}

fn accept_now(window: u32, value: Option<&str>, save: bool) {
    let taken = with_window(window, |entry| {
        let open = entry.open.as_mut()?;
        Some(accept_open(open, value, save))
    })
    .flatten();
    let Some(outcome) = taken else {
        crate::message::error("prompt-accept: nothing is being asked");
        return;
    };
    match outcome {
        Ok(answer) => {
            let question = with_window(window, |entry| entry.open.take().map(|open| open.question))
                .flatten();
            if let Some(question) = question {
                deliver(question, answer);
            }
            leave_mode(window);
        }
        // `AuthenticationPrompt.accept` returning False is not an error: `<Return>` on the username
        // moves to the password (`prompt.py:932-936`). Nothing is answered and the prompt stays.
        Err(None) => crate::ipc::push_bar(),
        Err(Some(why)) => crate::message::error(&format!("prompt-accept: {why}")),
    }
}

/// The pure half: what `value` and `save` mean for this question, without touching the queue, the
/// mode or CEF. `Err(None)` is "accepted, but not finished" — the login's field switch.
fn accept_open(
    open: &mut Open,
    value: Option<&str>,
    save: bool,
) -> Result<Answer, Option<String>> {
    // `_check_save_support` (`prompt.py:590-593`): only a yes/no question can be saved, and only
    // one that names a setting. bru has neither a per-domain store a prompt may write nor a
    // settings file it is allowed to create (DESIGN.md — `~/.config/bru/` is configer's), so
    // `Y`/`N` are refused with the reason rather than silently behaving like `y`/`n`.
    if save {
        return Err(Some(if open.question.kind == Kind::YesNo {
            "bru has nowhere to save an answer — ~/.config/bru/ is configer's file, not bru's"
                .to_string()
        } else {
            "saving answers is only possible with yes/no prompts".to_string()
        }));
    }

    match open.question.kind {
        Kind::Alert => {
            if value.is_some() {
                return Err(Some("no value is permitted with alert prompts".to_string()));
            }
            Ok(Answer::Cancelled)
        }
        Kind::YesNo => match value {
            None => match open.question.default_yes {
                Some(true) => Ok(Answer::Yes),
                Some(false) => Ok(Answer::No),
                None => Err(Some("no default value was set for this question".to_string())),
            },
            Some("yes") => Ok(Answer::Yes),
            Some("no") => Ok(Answer::No),
            Some(other) => Err(Some(format!("invalid value {other} — expected yes/no"))),
        },
        Kind::Text => Ok(Answer::Text(
            value.map(str::to_string).unwrap_or_else(|| open.line.text()),
        )),
        Kind::Download => {
            let text = value.map(str::to_string).unwrap_or_else(|| open.line.text());
            let path = expand_path(&text);
            if path.as_os_str().is_empty() {
                return Err(Some("invalid filename".to_string()));
            }
            Ok(Answer::Text(path.to_string_lossy().to_string()))
        }
        Kind::UserPwd => {
            if let Some(value) = value {
                let Some((user, password)) = value.split_once(':') else {
                    return Err(Some(format!(
                        "value needs to be in the format username:password, but {value} was given"
                    )));
                };
                return Ok(Answer::UserPwd {
                    user: user.to_string(),
                    password: password.to_string(),
                });
            }
            if !open.on_password {
                // `<Return>` on the username field moves to the password one, which is what
                // qutebrowser does so that the old `<Tab>: prompt-accept` binding still works.
                open.on_password = true;
                return Err(None);
            }
            Ok(Answer::UserPwd {
                user: open.line.text(),
                password: open.password.text(),
            })
        }
    }
}

/// `prompt-item-focus <next|prev>`. A filename prompt walks its directory listing; a login walks
/// its two fields. Every other shape ignores it — `UnsupportedOperationError`, caught and passed
/// (`prompt.py:433-435`).
fn item_focus(window: u32, which: FocusWhich) {
    let changed = with_window(window, |entry| {
        let Some(open) = entry.open.as_mut() else {
            return false;
        };
        match open.question.kind {
            Kind::UserPwd => {
                let to_password = which == FocusWhich::Next;
                if open.on_password == to_password {
                    return false;
                }
                open.on_password = to_password;
                true
            }
            Kind::Download => {
                if open.items.is_empty() {
                    return false;
                }
                let last = open.items.len() - 1;
                open.selected = Some(match (which, open.selected) {
                    (FocusWhich::Prev, None) => last,
                    (_, None) => 0,
                    (FocusWhich::Prev, Some(0)) => last,
                    (FocusWhich::Prev, Some(at)) => at - 1,
                    (_, Some(at)) if at == last => 0,
                    (_, Some(at)) => at + 1,
                });
                // Selecting a row puts it in the line, the way `_insert_path` does — the point of
                // `<Tab>` is completion, not a torch shone along a list.
                if let Some(at) = open.selected {
                    let joined = join_dir(&open.line.text(), &open.items[at]);
                    open.line.set_text(&joined);
                }
                true
            }
            _ => false,
        }
    });
    if changed == Some(true) {
        crate::ipc::push_bar();
    }
}

/// `prompt-yank [--sel]` — the URL the question is about.
fn yank(window: u32, sel: bool) {
    let url = with_window(window, |entry| {
        entry.open.as_ref().map(|open| open.question.url.clone())
    })
    .flatten()
    .unwrap_or_default();
    if url.is_empty() {
        crate::message::error("No URL found.");
        return;
    }
    let text = crate::clip::yank_text(&url, false);
    crate::clip::yank_plain(&text, sel);
}

/// `prompt-open-download [--pdfjs] [<cmdline>]`.
///
/// **`--pdfjs` is not PDF.js here, and cannot be**: qutebrowser ships a copy of PDF.js and serves
/// it from `qute://pdfjs`, which is a whole second chrome document. bru has no in-browser viewer,
/// so `<Ctrl-P>` means the desktop's own PDF viewer — `xdg-open` with no command line — which is
/// the same *intent* (look at this without saving it somewhere I will forget) reached the way this
/// machine reaches everything else.
fn open_download(window: u32, cmdline: Option<&str>, pdfjs: bool) {
    let is_download = with_window(window, |entry| {
        entry.open.as_ref().is_some_and(|open| open.question.kind == Kind::Download)
    })
    .unwrap_or(false);
    if !is_download {
        // `UnsupportedOperationError` — caught and passed (`prompt.py:419-420`). A key that does
        // nothing on the wrong kind of prompt is qutebrowser's own behaviour.
        return;
    }
    let cmdline = if pdfjs { None } else { cmdline };
    let question = with_window(window, |entry| entry.open.take().map(|open| open.question)).flatten();
    if let Some(question) = question {
        deliver(question, Answer::OpenDownload { cmdline: cmdline.map(str::to_string) });
    }
    leave_mode(window);
}

/// `prompt-fileselect-external` — choose a directory with a real file browser.
///
/// qutebrowser's `fileselect.folder.command` defaults to `['xterm', '-e', 'ranger',
/// '--choosedir={}']`: a terminal, a file manager, and a file the manager writes the chosen path
/// into. bru has no settings file of its own to keep that in, so it follows `editor.rs`'s
/// precedent exactly — `$BRU_FILESELECT` first, qutebrowser's own default after it — rather than
/// adding a setting to a file another workstream owns this round.
fn fileselect_external(window: u32) {
    let is_download = with_window(window, |entry| {
        entry
            .open
            .as_ref()
            .is_some_and(|open| matches!(open.question.kind, Kind::Download | Kind::Text))
    })
    .unwrap_or(false);
    if !is_download {
        crate::message::error(
            "prompt-fileselect-external: this question is not asking for a file",
        );
        return;
    }

    let spec = std::env::var("BRU_FILESELECT")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "xterm -e ranger --choosedir={}".to_string());

    let path = match crate::spawn::temp_path("fileselect", "txt") {
        Ok(path) => path,
        Err(e) => {
            crate::message::error(&format!("prompt-fileselect-external: {e}"));
            return;
        }
    };
    let argv = match crate::spawn::shlex(&spec) {
        Ok(argv) if !argv.is_empty() => argv
            .iter()
            .map(|arg| arg.replace("{}", &path.display().to_string()))
            .collect::<Vec<_>>(),
        Ok(_) => {
            crate::message::error("prompt-fileselect-external: the command is empty");
            return;
        }
        Err(e) => {
            crate::message::error(&format!("prompt-fileselect-external: {e}"));
            return;
        }
    };

    // On a thread of its own, like `editor::run_editor`: the picker is a terminal program and the
    // UI thread is what draws the prompt it will answer. If it never exits, this thread parks and
    // bru is otherwise untouched.
    std::thread::spawn(move || {
        let status = std::process::Command::new(&argv[0]).args(&argv[1..]).status();
        let chosen = match status {
            Ok(status) if status.success() => std::fs::read_to_string(&path).unwrap_or_default(),
            Ok(status) => {
                crate::message::error(&format!("the file selector exited with {status}"));
                String::new()
            }
            Err(e) => {
                crate::message::error(&format!("could not run {:?}: {e}", argv[0]));
                String::new()
            }
        };
        let _ = std::fs::remove_file(&path);
        let chosen = chosen.trim().to_string();
        if chosen.is_empty() {
            crate::message::info("No folder chosen.");
            return;
        }
        // Back on the UI thread to touch the prompt, and by name rather than by handle: the
        // question may have been answered or cancelled while the picker was open.
        let mut task = FileChosen::new(window, chosen);
        post_task(ThreadId::UI, Some(&mut task));
    });
}

wrap_task! {
    struct FileChosen {
        window: u32,
        chosen: String,
    }

    impl Task {
        fn execute(&self) {
            let there = with_window(self.window, |entry| {
                let Some(open) = entry.open.as_mut() else {
                    return false;
                };
                if !open.question.kind.has_input() {
                    return false;
                }
                open.line.set_text(&self.chosen);
                open.items = list_dir(&self.chosen);
                open.selected = None;
                true
            });
            if there == Some(true) {
                // `self.prompt_accept(folders[0])` — qutebrowser accepts straight away
                // (`prompt.py:483`), and so does this: the picker *is* the answer.
                accept(self.window, Some(&self.chosen), false);
            }
        }
    }
}

// -----------------------------------------------------------------------------------------------
// Paths, for the filename prompt
// -----------------------------------------------------------------------------------------------

/// `~` and `$HOME`, so a typed `~/tmp/x.pdf` is a real path.
fn expand_path(text: &str) -> PathBuf {
    let text = text.trim();
    if let Some(rest) = text.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(text)
}

/// The directories under whatever `text` names, sorted, with `..` first — `_set_fileview_root`
/// plus `DownloadFilenamePrompt`'s `QDir.Filter.AllDirs` (`prompt.py:863-864`). Directories only:
/// the thing being chosen is where a file goes, and the file's own name is typed.
fn list_dir(text: &str) -> Vec<String> {
    let path = expand_path(text);
    let dir = if path.is_dir() {
        path
    } else {
        match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
            _ => return Vec::new(),
        }
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| !name.starts_with('.'))
        .collect();
    out.sort();
    out.insert(0, "..".to_string());
    out
}

/// Where selecting `item` takes the line. `..` goes up; anything else goes down.
fn join_dir(text: &str, item: &str) -> String {
    let path = expand_path(text);
    let dir = if path.is_dir() { path } else { path.parent().map(PathBuf::from).unwrap_or_default() };
    let joined = if item == ".." {
        dir.parent().map(PathBuf::from).unwrap_or(dir)
    } else {
        dir.join(item)
    };
    format!("{}/", joined.display())
}

// -----------------------------------------------------------------------------------------------
// Where questions come from
// -----------------------------------------------------------------------------------------------

/// The window a browser is in, for a CEF callback that arrives with one.
fn window_of(browser: Option<&Browser>) -> Option<u32> {
    let id = browser?.identifier();
    crate::state::BruState::instance()
        .and_then(|state| state.lock().ok().and_then(|state| state.window_of_browser(id)))
}

// A page's `alert()`, `confirm()` and `prompt()`, and "leave site?".
//
// Returning 1 means bru has taken the dialog and will answer the callback — possibly much later,
// which is exactly what a prompt in the bar is. Returning 0 would let CEF draw its own dialog,
// which is a piece of browser chrome DESIGN.md does not have.
// (The wrap_ macros take no doc comment on the struct they declare — CEF-NOTES trap 8.)
wrap_jsdialog_handler! {
    pub struct BruJsdialogHandler;

    impl JsdialogHandler {
        fn on_jsdialog(
            &self,
            browser: Option<&mut Browser>,
            origin_url: Option<&CefString>,
            dialog_type: JsdialogType,
            message_text: Option<&CefString>,
            default_prompt_text: Option<&CefString>,
            callback: Option<&mut JsdialogCallback>,
            _suppress_message: Option<&mut ::std::os::raw::c_int>,
        ) -> ::std::os::raw::c_int {
            debug_assert_ne!(currently_on(ThreadId::UI), 0);
            let Some(callback) = callback.map(|callback| callback.clone()) else {
                return 0;
            };
            let origin = origin_url.map(CefString::to_string).unwrap_or_default();
            let text = message_text.map(CefString::to_string).unwrap_or_default();
            let default = default_prompt_text.map(CefString::to_string).unwrap_or_default();
            let browser = browser.as_deref();
            let Some(window) = window_of(browser) else {
                // A browser bru did not put in a window. Nothing can draw the question, so the
                // page is told "cancelled" rather than left waiting for ever.
                callback.cont(0, None);
                return 1;
            };

            let (kind, title) = match dialog_type {
                JsdialogType::CONFIRM => (Kind::YesNo, "JavaScript confirm"),
                JsdialogType::PROMPT => (Kind::Text, "JavaScript prompt"),
                // ALERT, and anything CEF adds later: an alert is the shape with no answer, which
                // is the safe thing to draw for a dialog type bru does not recognise.
                _ => (Kind::Alert, "JavaScript alert"),
            };

            let question = Question::new(kind, title, move |answer| match answer {
                Answer::Yes => callback.cont(1, None),
                Answer::Text(text) => callback.cont(1, Some(&CefString::from(text.as_str()))),
                // Cancelled *is* the answer for an alert: `alert()` returns nothing and the page
                // only needs to be let go of.
                _ => callback.cont(0, None),
            })
            .about(browser, &origin)
            .saying(&text)
            .defaulting_to(&default);
            ask(window, question);
            1
        }

        fn on_before_unload_dialog(
            &self,
            browser: Option<&mut Browser>,
            message_text: Option<&CefString>,
            is_reload: ::std::os::raw::c_int,
            callback: Option<&mut JsdialogCallback>,
        ) -> ::std::os::raw::c_int {
            debug_assert_ne!(currently_on(ThreadId::UI), 0);
            let Some(callback) = callback.map(|callback| callback.clone()) else {
                return 0;
            };
            let browser = browser.as_deref();
            let Some(window) = window_of(browser) else {
                // No window to ask in. **`0`, not `1`** — the page stays, which is the side that
                // cannot lose typed-in work.
                callback.cont(0, None);
                return 1;
            };
            let origin = crate::ipc::current_url();
            let what = if is_reload != 0 { "Reload the page?" } else { "Leave the page?" };
            let page = message_text.map(CefString::to_string).unwrap_or_default();
            let text = if page.trim().is_empty() {
                what.to_string()
            } else {
                format!("{what} The page says: {page}")
            };

            // `default_answer` is deliberately *not* set: `<Return>` on its own must not throw the
            // page away. `y` says so explicitly, and `<Escape>` — the reflex — keeps the page.
            let question = Question::new(Kind::YesNo, "Leave this page?", move |answer| {
                callback.cont((answer == Answer::Yes) as ::std::os::raw::c_int, None);
            })
            .about(browser, &origin)
            .saying(&text);
            ask(window, question);
            1
        }

        fn on_reset_dialog_state(&self, browser: Option<&mut Browser>) {
            // A page that navigated away from or discarded whatever it had open. Its question can
            // no longer be answered by anybody, so it is cancelled and the queue moves on. This is
            // also bru's answer to "what happens when the tab that raised it is closed": a closing
            // browser resets its dialog state.
            if let Some(browser) = browser {
                cancel_for_browser(browser.identifier());
            }
        }
    }
}

// Camera, microphone, notifications, location, and everything else Chromium asks permission for.
//
// `on_show_permission_prompt` is the one that covers all of them; returning 1 means bru has taken
// the prompt. `on_request_media_access_permission` is deliberately left at its default of 0, which
// is "no opinion, use the ordinary permission flow" — implementing both would ask twice.
wrap_permission_handler! {
    pub struct BruPermissionHandler;

    impl PermissionHandler {
        fn on_show_permission_prompt(
            &self,
            browser: Option<&mut Browser>,
            _prompt_id: u64,
            requesting_origin: Option<&CefString>,
            requested_permissions: u32,
            callback: Option<&mut PermissionPromptCallback>,
        ) -> ::std::os::raw::c_int {
            debug_assert_ne!(currently_on(ThreadId::UI), 0);
            let Some(callback) = callback.map(|callback| callback.clone()) else {
                return 0;
            };
            let origin = requesting_origin.map(CefString::to_string).unwrap_or_default();
            let browser = browser.as_deref();
            let Some(window) = window_of(browser) else {
                callback.cont(PermissionRequestResult::DENY);
                return 1;
            };

            let question = Question::new(Kind::YesNo, "Permission request", move |answer| {
                callback.cont(match answer {
                    Answer::Yes => PermissionRequestResult::ACCEPT,
                    Answer::No => PermissionRequestResult::DENY,
                    // A question walked away from is not a refusal Chromium should remember, and
                    // DISMISS is the value that says so.
                    _ => PermissionRequestResult::DISMISS,
                });
            })
            .about(browser, &origin)
            .saying(&format!(
                "Allow this site to {}?",
                permission_names(requested_permissions)
            ));
            ask(window, question);
            1
        }
    }
}

/// What a `cef_permission_request_types_t` bitmask is asking for, in words.
///
/// The bits are Chromium's own and there is no cef-rs enum for them, so the names are taken from
/// `cef_permission_request_types_t` in `cef_types.h` and the ones this browser can actually meet
/// are spelled out; anything else is reported as its number rather than guessed at, because a
/// permission prompt that names the wrong permission is worse than one that names none.
fn permission_names(mask: u32) -> String {
    const KNOWN: &[(u32, &str)] = &[
        (1 << 1, "read your clipboard"),
        (1 << 6, "know your location"),
        (1 << 8, "use your camera"),
        (1 << 9, "use your microphone"),
        (1 << 12, "read MIDI devices"),
        (1 << 15, "show notifications"),
        (1 << 20, "play protected content"),
        (1 << 21, "store data on this device"),
        (1 << 24, "open windows without asking"),
    ];
    let mut named: Vec<&str> = KNOWN
        .iter()
        .filter(|(bit, _)| mask & bit != 0)
        .map(|(_, name)| *name)
        .collect();
    let unknown = mask & !KNOWN.iter().map(|(bit, _)| bit).sum::<u32>();
    let leftover;
    if unknown != 0 {
        leftover = format!("use permission {unknown:#x}");
        named.push(&leftover);
    }
    if named.is_empty() {
        return "do something it did not name".to_string();
    }
    named.join(", ")
}

/// `RequestHandler::auth_credentials` (bindings 26862) — HTTP basic auth and proxy auth.
///
/// Returning 1 means bru will answer `callback` and the request waits. Returning 0 cancels it,
/// which is what happens for a browser bru does not know.
pub fn on_auth_credentials(
    browser: Option<&mut Browser>,
    origin_url: Option<&CefString>,
    is_proxy: ::std::os::raw::c_int,
    host: Option<&CefString>,
    port: ::std::os::raw::c_int,
    realm: Option<&CefString>,
    callback: Option<&mut AuthCallback>,
) -> ::std::os::raw::c_int {
    debug_assert_ne!(currently_on(ThreadId::UI), 0);
    let Some(callback) = callback.map(|callback| callback.clone()) else {
        return 0;
    };
    let browser = browser.as_deref();
    let Some(window) = window_of(browser) else {
        callback.cancel();
        return 1;
    };
    let origin = origin_url.map(CefString::to_string).unwrap_or_default();
    let host = host.map(CefString::to_string).unwrap_or_default();
    let realm = realm.map(CefString::to_string).unwrap_or_default();
    let who = if is_proxy != 0 {
        format!("The proxy {host}:{port}")
    } else if host.is_empty() {
        "This site".to_string()
    } else {
        format!("{host}:{port}")
    };
    let text = if realm.trim().is_empty() {
        format!("{who} needs authentication")
    } else {
        // The realm is the server's own string and is treated exactly like a page's: one line,
        // capped, and never concatenated into anything bru claims for itself.
        format!("{who} says: {realm}")
    };

    let question = Question::new(Kind::UserPwd, "Authentication required", move |answer| {
        match answer {
            Answer::UserPwd { user, password } => callback.cont(
                Some(&CefString::from(user.as_str())),
                Some(&CefString::from(password.as_str())),
            ),
            _ => callback.cancel(),
        }
    })
    .about(browser, &origin)
    .saying(&text);
    ask(window, question);
    1
}

/// `RequestHandler::on_certificate_error` (bindings 26873).
///
/// **The default is to refuse**, and staying with it is what a browser must do: returning 0 lets
/// Chromium show its own interstitial and the connection is not made. Returning 1 means bru has
/// taken the decision and will answer `callback`, and bru only ever answers it with `cancel` unless
/// the user says otherwise in so many words.
pub fn on_certificate_error(
    browser: Option<&mut Browser>,
    cert_error: Errorcode,
    request_url: Option<&CefString>,
    callback: Option<&mut Callback>,
) -> ::std::os::raw::c_int {
    debug_assert_ne!(currently_on(ThreadId::UI), 0);
    let Some(callback) = callback.map(|callback| callback.clone()) else {
        return 0;
    };
    let browser = browser.as_deref();
    let Some(window) = window_of(browser) else {
        callback.cancel();
        return 1;
    };
    let url = request_url.map(CefString::to_string).unwrap_or_default();

    let question = Question::new(Kind::YesNo, "Certificate error", move |answer| {
        match answer {
            Answer::Yes => callback.cont(),
            // Anything else — `n`, `<Escape>`, the tab closing — refuses. There is deliberately no
            // default: `<Return>` alone must never accept a bad certificate.
            _ => callback.cancel(),
        }
    })
    .about(browser, &url)
    // **The default is no**, which is what makes `<Return>` — the reflex — refuse. It is the one
    // question in this file with a default at all: everywhere else a bare `<Return>` says "no
    // default value was set" and waits, and here waiting would be worse than the safe answer.
    .default_answer(false)
    .saying(&format!(
        "The certificate for this site is not valid ({}). Continue anyway?",
        cert_error.get_raw()
    ));
    ask(window, question);
    1
}

// A page asking for a file — `<input type=file>`, and the directory picker behind it.
//
// bru answers with a prompt rather than with CEF's own dialog for the same reason it answers a
// `confirm()` that way: a native file chooser is a second window with a second keyboard grammar,
// and this browser has one of each.
wrap_dialog_handler! {
    pub struct BruDialogHandler;

    impl DialogHandler {
        fn on_file_dialog(
            &self,
            browser: Option<&mut Browser>,
            _mode: FileDialogMode,
            title: Option<&CefString>,
            default_file_path: Option<&CefString>,
            _accept_filters: Option<&mut CefStringList>,
            _accept_extensions: Option<&mut CefStringList>,
            _accept_descriptions: Option<&mut CefStringList>,
            callback: Option<&mut FileDialogCallback>,
        ) -> ::std::os::raw::c_int {
            debug_assert_ne!(currently_on(ThreadId::UI), 0);
            let Some(callback) = callback.map(|callback| callback.clone()) else {
                return 0;
            };
            let browser = browser.as_deref();
            let Some(window) = window_of(browser) else {
                callback.cancel();
                return 1;
            };
            let origin = crate::ipc::current_url();
            // The page supplies this string, so it goes through the same flattening and cap as any
            // other page text — and into the *text*, never into the title.
            let asked = title.map(CefString::to_string).unwrap_or_default();
            let default = default_file_path.map(CefString::to_string).unwrap_or_default();

            let question = Question::new(Kind::Text, "File requested by the page", move |answer| {
                match answer {
                    Answer::Text(text) if !text.trim().is_empty() => {
                        let mut paths = CefStringList::new();
                        paths.append(expand_path(&text).to_string_lossy().as_ref());
                        callback.cont(Some(&mut paths));
                    }
                    _ => callback.cancel(),
                }
            })
            .about(browser, &origin)
            .saying(if asked.trim().is_empty() { "Choose a file" } else { &asked })
            .defaulting_to(&default);
            ask(window, question);
            1
        }
    }
}

// -----------------------------------------------------------------------------------------------
// Tests — the bookkeeping only. Anything that posts a CEF task cannot be called here (trap 13).
// -----------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn question(kind: Kind) -> Question {
        Question::new(kind, "test", |_| {})
    }

    fn opened(kind: Kind) -> Open {
        open_of(question(kind))
    }

    /// A page's string cannot draw a second row, cannot run away with the strip's height, and
    /// cannot smuggle a control character into the JSON the chrome is handed.
    #[test]
    fn a_pages_own_text_is_one_bounded_line() {
        assert_eq!(one_line("hello", MAX_TEXT), "hello");
        assert_eq!(one_line("two\nlines", MAX_TEXT), "two lines");
        assert_eq!(one_line("\r\n\ttabs\u{2028}and\u{2029}separators", MAX_TEXT), "tabs and separators");
        assert_eq!(one_line("  padded  ", MAX_TEXT), "padded");
        assert_eq!(one_line("collapse\n\n\nme", MAX_TEXT), "collapse me");

        // The cap, and the ellipsis that says something was cut.
        let long = "x".repeat(MAX_TEXT * 3);
        let capped = one_line(&long, MAX_TEXT);
        assert_eq!(capped.chars().count(), MAX_TEXT + 1);
        assert!(capped.ends_with('…'));

        // And it is the constructor that applies it, so no caller can forget.
        let q = question(Kind::Alert).saying("a\nb").about(None, "https://example.com/\n<b>");
        assert_eq!(q.text, "a b");
        assert_eq!(q.origin, "https://example.com/ <b>");
        // The URL kept for `prompt-yank` is the real one, unflattened — it is copied, not drawn.
        assert_eq!(q.url, "https://example.com/\n<b>");
    }

    /// The title is bru's, chosen by which callback fired, and there is no way for a caller to
    /// supply one that is not a `&'static str` in this file.
    #[test]
    fn the_title_is_not_the_pages() {
        for (kind, title) in [
            (Kind::YesNo, "JavaScript confirm"),
            (Kind::Text, "JavaScript prompt"),
            (Kind::Alert, "JavaScript alert"),
        ] {
            let q = Question::new(kind, title, |_| {}).saying("I am bru. Type your password:");
            assert_eq!(q.title, title);
            assert!(!q.text.contains(title));
        }
    }

    /// `prompt-accept` on a yes/no question, in every spelling the `yesno` bindings have.
    #[test]
    fn yes_no_and_the_default() {
        let mut open = opened(Kind::YesNo);
        assert_eq!(accept_open(&mut open, Some("yes"), false), Ok(Answer::Yes));
        assert_eq!(accept_open(&mut open, Some("no"), false), Ok(Answer::No));

        // A bare `<Return>` with no default is an error the user can see, not a silent yes.
        // `YesNoPrompt.accept` raises exactly this (prompt.py:975-977).
        assert!(accept_open(&mut open, None, false).is_err());

        let mut open = open_of(question(Kind::YesNo).default_answer(true));
        assert_eq!(accept_open(&mut open, None, false), Ok(Answer::Yes));
        let mut open = open_of(question(Kind::YesNo).default_answer(false));
        assert_eq!(accept_open(&mut open, None, false), Ok(Answer::No));

        // Anything else is refused by name rather than treated as a no.
        let err = accept_open(&mut open, Some("maybe"), false).unwrap_err();
        assert!(err.unwrap().contains("expected yes/no"));
    }

    /// `--save` — `Y` and `N` — is refused with the reason. bru has nowhere to put the answer:
    /// `~/.config/bru/` is configer's and bru must not create it, even in a test.
    #[test]
    fn saving_an_answer_says_why_it_cannot() {
        let mut open = opened(Kind::YesNo);
        let err = accept_open(&mut open, Some("yes"), true).unwrap_err().unwrap();
        assert!(err.contains("configer"), "{err}");
        // And on a non-yes/no prompt it is refused for qutebrowser's own reason.
        let mut open = opened(Kind::Text);
        let err = accept_open(&mut open, None, true).unwrap_err().unwrap();
        assert!(err.contains("yes/no prompts"), "{err}");
    }

    /// A login takes `<Return>` twice: the first moves to the password field, the second answers.
    /// `AuthenticationPrompt.accept` (prompt.py:932-940).
    #[test]
    fn a_login_answers_on_the_second_return() {
        let mut open = opened(Kind::UserPwd);
        open.line.set_text("someone");
        assert_eq!(accept_open(&mut open, None, false), Err(None), "the first one only moves");
        assert!(open.on_password);
        open.password.set_text("hunter2");
        assert_eq!(
            accept_open(&mut open, None, false),
            Ok(Answer::UserPwd { user: "someone".to_string(), password: "hunter2".to_string() })
        );

        // The `user:password` form, which is what a `prompt-accept` with a value means here.
        let mut open = opened(Kind::UserPwd);
        assert_eq!(
            accept_open(&mut open, Some("a:b:c"), false),
            Ok(Answer::UserPwd { user: "a".to_string(), password: "b:c".to_string() })
        );
        assert!(accept_open(&mut open, Some("nocolon"), false).is_err());
    }

    /// An alert has nothing to answer, so a value is refused rather than swallowed.
    #[test]
    fn an_alert_takes_no_value() {
        let mut open = opened(Kind::Alert);
        assert_eq!(accept_open(&mut open, None, false), Ok(Answer::Cancelled));
        assert!(accept_open(&mut open, Some("yes"), false).is_err());
    }

    /// The strip's height is a row count times the stylesheet's own row height, and nothing else —
    /// the same arithmetic `completers::resize_bar` does, against the same `--row-h`.
    #[test]
    fn the_strip_grows_by_whole_rows_and_stops() {
        assert_eq!(wanted_height(0), 0, "no prompt is no extra height at all");
        assert_eq!(wanted_height(1), ROW_H + 1);
        assert_eq!(wanted_height(5), ROW_H * 5 + 1);
        // A page cannot push past the cap, however much it says.
        assert_eq!(wanted_height(1000), MAX_H + 1);
    }

    /// Which shapes have something to type into. `keys.rs` asks this before letting a letter
    /// through to the bottom strip, so a wrong answer here is either an untypable prompt or a
    /// yes/no question quietly eating keystrokes into an input nobody can see.
    #[test]
    fn only_the_three_shapes_with_a_line_take_typing() {
        assert!(Kind::Text.has_input());
        assert!(Kind::Download.has_input());
        assert!(Kind::UserPwd.has_input());
        assert!(!Kind::YesNo.has_input());
        assert!(!Kind::Alert.has_input());

        // And the mode each is answered in.
        assert_eq!(Kind::YesNo.mode(), Mode::YesNo);
        for kind in [Kind::Alert, Kind::Text, Kind::Download, Kind::UserPwd] {
            assert_eq!(kind.mode(), Mode::Prompt, "{kind:?}");
        }
    }

    /// The permission mask is named where bru knows the name and reported as a number where it does
    /// not. A prompt that says the wrong permission is worse than one that says none.
    #[test]
    fn a_permission_is_named_or_numbered_but_never_guessed() {
        assert_eq!(permission_names(1 << 15), "show notifications");
        assert_eq!(permission_names(1 << 8 | 1 << 9), "use your camera, use your microphone");
        assert_eq!(permission_names(1 << 30), "use permission 0x40000000");
        assert_eq!(permission_names(0), "do something it did not name");
    }

    /// The key hints are bru's rows, and a command nothing is bound to says so in the words
    /// qutebrowser uses — a `config.lua` that unbinds `y` must not leave a prompt claiming `y`
    /// answers it. Built against a table rather than the live one so this needs no browser.
    #[test]
    fn a_key_hint_names_the_live_binding_or_admits_there_is_none() {
        let table = vec![
            (Mode::YesNo, "y".to_string(), "prompt-accept yes".to_string()),
            (Mode::YesNo, "<Return>".to_string(), "prompt-accept".to_string()),
            (Mode::YesNo, "<Escape>".to_string(), "mode-leave".to_string()),
            // The same command in the *other* prompt mode must not be borrowed for this one.
            (Mode::Prompt, "n".to_string(), "prompt-accept no".to_string()),
        ];
        assert_eq!(binding_in(&table, Mode::YesNo, "prompt-accept yes"), "y");
        assert_eq!(binding_in(&table, Mode::YesNo, "mode-leave"), "<Escape>");
        assert_eq!(
            binding_in(&table, Mode::YesNo, "prompt-accept no"),
            "unbound (prompt-accept no)"
        );
        // `<Return>` and `<Escape>` win over anything else bound to the same command.
        let table = vec![
            (Mode::YesNo, "<Return>".to_string(), "prompt-accept".to_string()),
            (Mode::YesNo, "z".to_string(), "prompt-accept".to_string()),
        ];
        assert_eq!(binding_in(&table, Mode::YesNo, "prompt-accept"), "<Return>");
    }

    /// The readline commands this module claims are exactly the ones `bindings.default.prompt`
    /// binds, and the two history commands are deliberately not among them: a prompt has no
    /// history, and claiming them would take `<Ctrl-P>`'s meaning away from the command line.
    #[test]
    fn the_prompt_claims_readline_but_not_the_history() {
        let claimed = [
            "rl-backward-char", "rl-forward-char", "rl-backward-word", "rl-forward-word",
            "rl-beginning-of-line", "rl-end-of-line", "rl-unix-line-discard", "rl-kill-line",
            "rl-rubout", "rl-filename-rubout", "rl-backward-kill-word", "rl-kill-word",
            "rl-yank", "rl-delete-char", "rl-backward-delete-char",
        ];
        let prompt_bindings: Vec<&str> = crate::config::DEFAULT_BINDINGS
            .iter()
            .filter(|(mode, _, _)| *mode == "prompt")
            .map(|(_, _, cmd)| *cmd)
            .collect();
        for name in claimed {
            assert!(
                prompt_bindings.iter().any(|cmd| cmd.split(' ').next() == Some(name)),
                "{name} is claimed here but not bound in prompt mode"
            );
        }
        assert!(
            !prompt_bindings.iter().any(|cmd| cmd.starts_with("command-history")),
            "the command history is command mode's, and prompt mode must not bind it"
        );
    }
}
