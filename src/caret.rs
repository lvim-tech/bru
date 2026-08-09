//! Caret mode — `v` and `V` — and marks — `` ` `` and `'`.
//!
//! A behavioural port of qutebrowser 3.7.0's `browser/webengine/webenginetab.py::WebEngineCaret`
//! and `javascript/caret.js` (movement), plus `mainwindow/tabbedbrowser.py`'s `set_mark`/`jump_mark`
//! (marks) and `modeparsers.RegisterKeyParser` (the single keystroke that names one).
//!
//! Four rules shape this file:
//!
//! - **The decisions are in Rust.** CEF offers nothing at document level — there is no "move the
//!   text cursor a word" — so the movement itself has to be `Selection.modify` inside the page. What
//!   does *not* have to be there is any of the reasoning: which of `move` and `extend` a keystroke
//!   means, how many times it repeats, what a line selection has to re-anchor after, and whether a
//!   mark exists. `chrome/caret.js` receives a list of primitives and applies them in order.
//! - **The page is not trusted.** Its answers arrive through the message router, which every page
//!   can reach. One is believed only while a request bru itself made is outstanding, only from that
//!   request's browser, and only carrying the token bru minted for it — the same three checks
//!   `src/hints.rs` makes.
//! - **Scrolling stays on the wheel.** Bringing the caret back into view, and jumping to a mark, are
//!   both `crate::scroll`'s `scroll_px`, which is `send_mouse_wheel_event`. Nothing here calls a
//!   scrolling function on the page; the page only ever *reports* where it is.
//! - **Yanking is not implemented here.** `y`/`Y`/`<Return>` in caret mode are `yank selection`,
//!   which needs the clipboard, and the clipboard is another workstream's. They stay
//!   `Command::Unimplemented` rather than becoming a variant that would collide with it. What that
//!   workstream needs from here is [`selection`], which answers the text the page last reported.

use cef::*;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use crate::bindings::{Key, KeyInfo, Modifiers, NamedKey};
use crate::commands::CaretMove;
use crate::modes::Mode;
use crate::state::BruState;
use crate::tabs::SharedState;

/// The page half, injected into the tab's main frame. Not served over `bru://`: it has to run in the
/// page's own world to see the page's document.
const CARET_JS: &str = include_str!("../chrome/caret.js");

// ------------------------------------------------------------------------------------------------
// State
// ------------------------------------------------------------------------------------------------

/// `browsertab.SelectionState`. Which of `move` and `extend` a movement means, and whether a line
/// selection has to be rebuilt after each one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SelectionState {
    /// The caret moves and nothing is selected.
    None,
    /// The caret moves and drags a selection behind it — `v`.
    Normal,
    /// As `Normal`, but each movement re-anchors to whole lines — `V`.
    Line,
}

impl SelectionState {
    /// `caret.js`'s `move`: `let action = "move"; if (selectionState !== NONE) { action = "extend"; }`
    fn alter(self) -> &'static str {
        match self {
            SelectionState::None => "move",
            SelectionState::Normal | SelectionState::Line => "extend",
        }
    }

    fn name(self) -> &'static str {
        match self {
            SelectionState::None => "none",
            SelectionState::Normal => "normal",
            SelectionState::Line => "line",
        }
    }
}

/// One run of caret mode: from `v` to `<Escape>`, `c`, or a tab switch.
struct Session {
    /// The tab this belongs to. Checked on every answer — a report from a background tab is not
    /// this session's.
    ///
    /// Still here now that the sessions are keyed by *window*: a window has many tabs and a session
    /// belongs to one of them. The window says which session an answer is for; this says whether the
    /// answer came from the tab the caret is in.
    browser_id: i32,
    /// Minted here, handed to the injected script, and required back on every answer.
    token: String,
    selection: SelectionState,
    /// What the page last reported as selected. The clipboard workstream reads this through
    /// [`selection`]; nothing else in bru keeps a copy.
    text: String,
    /// The caret's box in view coordinates, and the viewport it was measured against, from the last
    /// report. `None` when the page could not place the caret at all.
    caret: Option<(i32, i32, i32, i32)>,
    viewport: (i32, i32),
}

/// Every open caret session, keyed by the window it belongs to.
///
/// **Per window because the mode is.** qutebrowser's `AbstractCaret` is an attribute of
/// `AbstractTab` (`browser/browsertab.py:1026`), i.e. one per *tab*, and it can afford that because
/// the caret has no state of its own worth keeping — `CaretBrowsing` lives in the page and the
/// selection state is read back from it. bru keeps the state in Rust (the module docstring's first
/// rule), so it needs a place to put it, and the mode that decides whether `j` is a movement or an
/// extension is one `ModeManager` per window. A per-tab map would let a window hold two caret
/// sessions in two different selection states, which no mode can be in.
///
/// It was one `Mutex<Option<Session>>` for the process. `v` in a second window replaced the first
/// window's, and the first window stayed in caret mode with `selection()` answering the other
/// window's text.
fn sessions() -> &'static Mutex<HashMap<u32, Session>> {
    static SESSIONS: LazyLock<Mutex<HashMap<u32, Session>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    &SESSIONS
}

/// The window a browser is in, or `None` for a browser bru never put in one.
fn window_of(state: &SharedState, browser: &mut Browser) -> Option<u32> {
    let id = browser.identifier();
    state
        .lock()
        .expect("state mutex poisoned")
        .window_of_browser(id)
}

/// One outstanding question to the page that is not a caret state report.
struct Ask {
    browser_id: i32,
    token: String,
    what: AskWhat,
}

enum AskWhat {
    /// `` `x `` — the page is being asked where it is scrolled to, so the mark can be saved.
    SetMark(char),
    /// `'x` — the same question, so the jump's distance can be computed and `'` can be saved first.
    JumpMark(char),
    /// `selection-follow [-t]` — the page is being asked what link the selection is in.
    Follow { tab: bool },
}

/// The outstanding question per window.
///
/// **Keyed, and it is not only symmetry.** The token check already made a single slot *safe* — an
/// answer whose token does not match is refused — but it did not make it lossless: `` `a `` in one
/// window and `<Return>` in another a moment later minted a second token over the first, and the
/// first window's mark was then never set, with nothing said about it anywhere. One request per
/// window is the smallest key that cannot lose one, because a window can only be waiting for a mark
/// or a follow, never both: both are started by a key, and a key ends the register mode it came from
/// before it asks.
fn ask() -> &'static Mutex<HashMap<u32, Ask>> {
    static ASK: LazyLock<Mutex<HashMap<u32, Ask>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
    &ASK
}

/// `tabbedbrowser._local_marks` and `_global_marks`.
///
/// Lower case is per page and holds a scroll position; upper case is global and holds a position
/// **and** the URL it belongs to, which is what makes `'A` a navigation. Both are keyed by a URL
/// with its fragment stripped, "as it may interfere with scrolling" (tabbedbrowser.py:1084).
///
/// **One table for the process, and that is a deliberate divergence.** Both of qutebrowser's live on
/// `TabbedBrowser`, which is `objreg.register('tabbed-browser', …, scope='window')`
/// (`mainwindow/mainwindow.py:230`) — so in qutebrowser marks are per *window*, and `'A` set in one
/// window is simply not set in another. The local table is keyed by URL and would read the same
/// either way, since two windows on the same page would each hold their own `a` for it and the last
/// writer in a window is the one that window reads. The global one would not, and the name is the
/// argument: `tabbedbrowser.py:1078` says "capital indicates a global mark", and a mark whose whole
/// point is that it survives a change of page reading differently in two windows is a distinction
/// nobody typing `'A` means. bru's windows are one browsing session, not two profiles.
/// Unlike the two maps above, nothing here is keyed by window, so nothing here changed.
#[derive(Default)]
struct Marks {
    local: HashMap<(String, char), (i32, i32)>,
    global: HashMap<char, (i32, i32, String)>,
}

fn marks() -> &'static Mutex<Marks> {
    static MARKS: LazyLock<Mutex<Marks>> = LazyLock::new(|| Mutex::new(Marks::default()));
    &MARKS
}

/// A jump to a global mark whose page had to be loaded first, waiting for that page to be tall
/// enough to hold the position. `(key, x, y, attempts left)`, per window.
///
/// **Keyed, and this one fixes a bug rather than a collision.** `JumpToMark` and `PlaceMark` used
/// `active_browser()`, which is the window in *front*: a `'A` typed in a background window loaded
/// the page into whichever window happened to be current, and scrolled that one. They take the
/// window with them now and ask `active_browser_in`. The collision is real too — the retry runs for
/// up to 2.4 s, which is long enough for a second `'A` in another window to land inside it and take
/// the first one's remaining attempts.
fn pending_jump() -> &'static Mutex<HashMap<u32, (char, i32, i32, u32)>> {
    static PENDING: LazyLock<Mutex<HashMap<u32, (char, i32, i32, u32)>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    &PENDING
}

/// `BRU_DEBUG_CARET=1` traces every op list and every report. Off by default: it is a line per
/// keystroke, which is a line too many in a real session.
fn debug(message: &str) {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("BRU_DEBUG_CARET").is_some()) {
        eprintln!("bru[caret]: {message}");
    }
}

// ------------------------------------------------------------------------------------------------
// Entering and leaving
// ------------------------------------------------------------------------------------------------

/// The mode has changed. Called from the two `mode-enter`/`mode-leave` arms of `src/exec.rs`, and
/// from `handle_mark_key` below, with the mode bru was in and the mode it is in now.
///
/// Unlike hint mode, caret mode is entered *synchronously*: qutebrowser's `v` enters it and the
/// page's caret is placed afterwards. It has to be, because `V` is
/// `mode-enter caret ;; selection-toggle --line` — a chain whose second half would run before the
/// first had finished if entry waited for the page.
pub fn on_mode_change(state: &SharedState, browser: &mut Browser, from: Mode, to: Mode) {
    if from == to {
        return;
    }
    // The window the tab is in, which is the window whose mode just changed: `exec::run` was handed
    // this browser by `keys.rs`, which had already made its window current.
    let Some(window) = window_of(state, browser) else {
        return;
    };
    if to == Mode::Caret {
        enter(window, browser);
    } else if from == Mode::Caret {
        leave(window, browser);
    }
}

fn enter(window: u32, browser: &mut Browser) {
    let Some(frame) = browser.main_frame() else {
        return;
    };
    let token = mint_token();
    sessions().lock().expect("caret sessions mutex poisoned").insert(
        window,
        Session {
            browser_id: browser.identifier(),
            token: token.clone(),
            selection: SelectionState::None,
            text: String::new(),
            caret: None,
            viewport: (0, 0),
        },
    );

    // Injected on every `v` rather than once per page load, for the same reason `src/hints.rs`
    // injects on every `f`: a navigation throws the world away and there is no cheap way to know
    // from here whether this one still has the script.
    let code = format!("{CARET_JS}\nwindow.__bru_caret.enter(\"{token}\");");
    frame.execute_java_script(Some(&CefString::from(code.as_str())), None, 0);
    debug(&format!("window {window} entered"));
}

fn leave(window: u32, browser: &mut Browser) {
    sessions()
        .lock()
        .expect("caret sessions mutex poisoned")
        .remove(&window);
    let Some(frame) = browser.main_frame() else {
        return;
    };
    // Its own script, so it runs even though the session is already gone from Rust's side.
    frame.execute_java_script(
        Some(&CefString::from(
            "window.__bru_caret && window.__bru_caret.leave();",
        )),
        None,
        0,
    );
    crate::ipc::set_search_match_for(window, String::new());
    debug(&format!("window {window} left"));
}

/// The text the page last reported as selected, and the state that selection is in.
///
/// **This is the call the clipboard workstream needs.** `y` / `Y` / `<Return>` in caret mode are
/// `yank selection` and `yank selection -s`, which are `Command::Unimplemented` here on purpose —
/// adding a `Command::Yank` variant would collide with the module that owns `wl-copy`. That module
/// asks here for the text and does the copying; nothing in this file touches a clipboard. That
/// module is `src/clip.rs`, and it calls this.
///
/// **The current window's**, and it takes no argument because its two callers have none to give: `yy`
/// in `clip.rs` and the `{selection}` variable in `spawn.rs` are both running a command in the window
/// it was typed in, and `keys.rs` has made that window current before either is reached. A caret
/// selection in a window that is not in front is not what `y` means.
pub fn selection() -> Option<(SelectionState, String)> {
    let window = BruState::instance()?
        .lock()
        .expect("state mutex poisoned")
        .current_window_id()?;
    let guard = sessions().lock().expect("caret sessions mutex poisoned");
    guard.get(&window).map(|s| (s.selection, s.text.clone()))
}

// ------------------------------------------------------------------------------------------------
// The movements — javascript/caret.js's `move` and `moveToBlock`, decided here
// ------------------------------------------------------------------------------------------------

/// One primitive the page can apply. The whole vocabulary; there is nothing else it can be told.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Op {
    /// `Selection.modify(alter, direction, granularity)`.
    Modify(&'static str, &'static str, &'static str),
    /// Swap anchor and focus, so the other end of the selection is the one that moves.
    Reverse,
    /// Collapse the selection to the caret.
    Drop,
}

impl Op {
    fn to_json(self) -> String {
        match self {
            Op::Modify(alter, direction, granularity) => {
                format!("[\"modify\",\"{alter}\",\"{direction}\",\"{granularity}\"]")
            }
            Op::Reverse => "[\"reverse\"]".to_string(),
            Op::Drop => "[\"drop\"]".to_string(),
        }
    }
}

/// `CaretBrowsing.selectLine` (javascript/caret.js:1136): grow the selection to the whole line by
/// walking each end out to its line boundary, swapping which end moves in between.
fn select_line() -> Vec<Op> {
    vec![
        Op::Modify("extend", "right", "lineboundary"),
        Op::Reverse,
        Op::Modify("extend", "left", "lineboundary"),
        Op::Reverse,
    ]
}

/// `CaretBrowsing.move(direction, granularity, count)`.
///
/// The `LINE` branch is qutebrowser's and is kept as it is: `updateLineSelection` acts only for
/// granularities coarser than a word, so in a `V` selection `h`, `l`, `w`, `b` and `e` do nothing at
/// all. That is not an oversight here — it is `javascript/caret.js`:1145.
fn mv(state: SelectionState, direction: &'static str, granularity: &'static str, count: u32) -> Vec<Op> {
    let mut ops = Vec::new();
    for _ in 0..count {
        if state == SelectionState::Line {
            if granularity != "character" && granularity != "word" {
                ops.push(Op::Modify("extend", direction, granularity));
                ops.extend(select_line());
            }
        } else {
            ops.push(Op::Modify(state.alter(), direction, granularity));
        }
    }
    ops
}

/// `CaretBrowsing.moveToBlock(paragraph, boundary, count)`. Note that this one does **not** take the
/// `move`'s LINE branch: it always modifies and then rebuilds the line selection.
fn move_to_block(
    state: SelectionState,
    paragraph: &'static str,
    boundary: &'static str,
    count: u32,
) -> Vec<Op> {
    let mut ops = Vec::new();
    for _ in 0..count {
        ops.push(Op::Modify(state.alter(), paragraph, "paragraph"));
        ops.push(Op::Modify(state.alter(), boundary, "paragraphboundary"));
        if state == SelectionState::Line {
            ops.extend(select_line());
        }
    }
    ops
}

/// The op list for one `move-to-…` command, from `funcs.*` in javascript/caret.js:1345–1420.
///
/// The three that take no count there — `moveToStartOfLine`, `moveToEndOfLine`,
/// `moveToStartOfDocument`, `moveToEndOfDocument` — are called with no argument, so a count on them
/// is ignored here too.
fn ops_for(mv_kind: CaretMove, state: SelectionState, count: u32) -> Vec<Op> {
    match mv_kind {
        CaretMove::NextChar => mv(state, "right", "character", count),
        CaretMove::PrevChar => mv(state, "left", "character", count),
        CaretMove::NextLine => mv(state, "forward", "line", count),
        CaretMove::PrevLine => mv(state, "backward", "line", count),
        CaretMove::EndOfWord => mv(state, "forward", "word", count),
        // `w` is `e` plus one character: Chromium's "forward word" lands on the *end* of the word,
        // and vim's `w` is the start of the next one.
        CaretMove::NextWord => {
            let mut ops = mv(state, "forward", "word", count);
            ops.extend(mv(state, "right", "character", 1));
            ops
        }
        CaretMove::PrevWord => mv(state, "backward", "word", count),
        CaretMove::StartOfLine => mv(state, "left", "lineboundary", 1),
        CaretMove::EndOfLine => mv(state, "right", "lineboundary", 1),
        CaretMove::StartOfNextBlock => move_to_block(state, "forward", "backward", count),
        CaretMove::StartOfPrevBlock => move_to_block(state, "backward", "backward", count),
        CaretMove::EndOfNextBlock => move_to_block(state, "forward", "forward", count),
        CaretMove::EndOfPrevBlock => move_to_block(state, "backward", "forward", count),
        CaretMove::StartOfDocument => mv(state, "backward", "documentboundary", 1),
        CaretMove::EndOfDocument => mv(state, "forward", "documentboundary", 1),
    }
}

/// `move-to-…`. The dispatcher's whole caret-movement arm.
pub fn move_to(state: &SharedState, browser: &mut Browser, kind: CaretMove, count: Option<u32>) {
    let count = count.unwrap_or(1).clamp(1, 1000);
    let Some((_, token, selection)) = current(state, browser) else {
        return;
    };
    run_ops(browser, &token, ops_for(kind, selection, count));
}

/// `selection-toggle [--line]` — `v`, `<Space>` and `V`.
///
/// `funcs.toggleSelection` (javascript/caret.js:1427) with the state moved into Rust: `--line`
/// always ends in `Line` and selects the caret's line; a bare toggle goes to `Normal` from anything
/// that is not already `Normal`, and from `Normal` back to `None`.
pub fn selection_toggle(state: &SharedState, browser: &mut Browser, line: bool) {
    let Some((window, token, was)) = current(state, browser) else {
        return;
    };
    let now = if line {
        SelectionState::Line
    } else if was != SelectionState::Normal {
        SelectionState::Normal
    } else {
        SelectionState::None
    };
    set_state(window, now);

    let ops = if line { select_line() } else { Vec::new() };
    // Even with no ops the page is asked to report, so the status line and `selection()` learn that
    // the state changed under an unmoved caret.
    run_ops(browser, &token, ops);
    debug(&format!(
        "window {window} selection {} -> {}",
        was.name(),
        now.name()
    ));
}

/// `selection-drop` — `<Ctrl-Space>`.
pub fn selection_drop(state: &SharedState, browser: &mut Browser) {
    let Some((window, token, _)) = current(state, browser) else {
        return;
    };
    set_state(window, SelectionState::None);
    run_ops(browser, &token, vec![Op::Drop]);
}

/// `selection-reverse` — `o`.
pub fn selection_reverse(state: &SharedState, browser: &mut Browser) {
    let Some((_, token, _)) = current(state, browser) else {
        return;
    };
    run_ops(browser, &token, vec![Op::Reverse]);
}

/// `selection-follow [-t]` — `<Return>` and `<Ctrl-Return>` in normal mode.
///
/// Not a caret-mode command: qutebrowser binds it in `normal:` (configdata.yml:3846), where it
/// follows whatever the page has selected — a caret-mode selection, a search match turned into one,
/// or the focused link. So it does not need a caret session, only a token of its own.
pub fn selection_follow(state: &SharedState, browser: &mut Browser, tab: bool) {
    let Some(window) = window_of(state, browser) else {
        return;
    };
    let Some(frame) = browser.main_frame() else {
        return;
    };
    let token = mint_token();
    ask().lock().expect("caret ask mutex poisoned").insert(
        window,
        Ask {
            browser_id: browser.identifier(),
            token: token.clone(),
            what: AskWhat::Follow { tab },
        },
    );
    let code = format!(
        "{CARET_JS}\nwindow.__bru_caret.follow(\"{token}\",\"{}\");",
        i32::from(tab)
    );
    frame.execute_java_script(Some(&CefString::from(code.as_str())), None, 0);
}

/// The window, token and selection state of the session this browser belongs to, or `None` when
/// caret mode is not open on it.
///
/// Both checks are kept: the window says which session, and `browser_id` says whether this is the tab
/// that session is in — a `j` that reached another tab of the hinting window is not a caret movement.
fn current(state: &SharedState, browser: &mut Browser) -> Option<(u32, String, SelectionState)> {
    let id = browser.identifier();
    let window = window_of(state, browser)?;
    let guard = sessions().lock().expect("caret sessions mutex poisoned");
    let open = guard.get(&window)?;
    if open.browser_id != id {
        return None;
    }
    Some((window, open.token.clone(), open.selection))
}

fn set_state(window: u32, selection: SelectionState) {
    if let Some(open) = sessions()
        .lock()
        .expect("caret sessions mutex poisoned")
        .get_mut(&window)
    {
        open.selection = selection;
    }
}

fn run_ops(browser: &mut Browser, token: &str, ops: Vec<Op>) {
    let Some(frame) = browser.main_frame() else {
        return;
    };
    let list = ops.iter().map(|op| op.to_json()).collect::<Vec<_>>().join(",");
    debug(&format!("ops [{list}]"));
    frame.execute_java_script(
        Some(&CefString::from(
            format!("window.__bru_caret && window.__bru_caret.run(\"{token}\",[{list}]);").as_str(),
        )),
        None,
        0,
    );
}

// ------------------------------------------------------------------------------------------------
// Marks — tabbedbrowser.set_mark / jump_mark, and the parser that names one
// ------------------------------------------------------------------------------------------------

/// One key while `set_mark` or `jump_mark` is current. Port of `RegisterKeyParser.handle`
/// (modeparsers.py:262).
///
/// `None` means bru is in neither mode and `src/keys.rs` should carry on to the ordinary parser.
/// `Some(swallow)` is what `on_pre_key_event` returns, and it is always `true`: these modes are
/// built with `BaseKeyParser`'s `passthrough=False`, so no key on the way to a mark name reaches the
/// page.
pub fn handle_mark_key(state: &SharedState, browser: &mut Browser, info: KeyInfo) -> Option<bool> {
    // The mode of the window this key's browser is in, by name rather than as "the current mode".
    // `keys.rs` has already made that window current so the two agree there, but `macros.rs` also
    // calls this from a replayed key, and a macro run in a background window must read that window's
    // mode. `None` is a browser bru never put in a window; there is nothing to route.
    let (window, mode) = {
        let guard = state.lock().expect("state mutex poisoned");
        let window = guard.window_of_browser(browser.identifier())?;
        (window, guard.mode_in(window))
    };
    if !mode.names_a_register() {
        return None;
    }

    // The `register:` bindings are consulted first (`super().handle(e)`), and they hold exactly one
    // entry: `<Escape>: mode-leave`.
    if info.key == Key::Named(NamedKey::Escape) {
        leave_register_mode(state, window);
        return Some(true);
    }

    // "this is not a proper register key, let it pass and keep going" — `info.is_special()`, which is
    // a modifier beyond Shift or a key that types nothing. The mode stays open waiting for a real
    // one.
    let Some(key) = register_char(info) else {
        return Some(true);
    };

    leave_register_mode(state, window);
    match mode {
        Mode::SetMark => request_mark(window, browser, AskWhat::SetMark(key)),
        Mode::JumpMark => request_mark(window, browser, AskWhat::JumpMark(key)),
// --- src/macros.rs -------------------------------------------------------------------------------
        // The other two modes `RegisterKeyParser` is built with (modeparsers.py:294-297). They are
        // two arms here and not a parser of their own for the reason this function exists at all:
        // "the next keystroke is a register name" is one behaviour, and four modes share it.
        Mode::RecordMacro => crate::macros::name_recording(key),
        Mode::RunMacro => crate::macros::run_named(state, browser, key),
// --- end src/macros.rs ---------------------------------------------------------------------------
        _ => {}
    }
    Some(true)
}

/// The character a keystroke names a register with, or `None` if it names none.
///
/// `keyutils.KeyInfo.is_special` is "a modifier other than Shift is held, or the key has no text".
/// Shift is exactly what tells `a` from `A`, and therefore a local mark from a global one.
fn register_char(info: KeyInfo) -> Option<char> {
    // Every modifier but Shift disqualifies it. Spelled as three `contains` calls rather than
    // `mods.without(SHIFT).is_empty()` because `without` is `bindings.rs`'s own, and that file
    // belongs to nobody this round. The keypad bit is deliberately not here: `` `<Keypad-3> `` is
    // still the mark named 3.
    if info.mods.contains(Modifiers::CTRL)
        || info.mods.contains(Modifiers::ALT)
        || info.mods.contains(Modifiers::META)
    {
        return None;
    }
    match info.key {
        Key::Char(c) => {
            // `Key::Char` is canonically upper case; the Shift bit is what says which was typed.
            if info.mods.contains(Modifiers::SHIFT) {
                Some(c)
            } else {
                c.to_lowercase().next()
            }
        }
        _ => None,
    }
}

fn leave_register_mode(state: &SharedState, window: u32) {
    let now = {
        let mut guard = state.lock().expect("state mutex poisoned");
        guard.leave_mode_in(window);
        guard.mode_in(window)
    };
    crate::ipc::set_mode_for(window, now.name().to_string());
    crate::ipc::set_keystring_for(window, String::new());
}

/// Ask the page where it is scrolled to, so a mark can be saved or a jump measured.
fn request_mark(window: u32, browser: &mut Browser, what: AskWhat) {
    let Some(frame) = browser.main_frame() else {
        return;
    };
    let (verb, key) = match what {
        AskWhat::SetMark(key) => ("set", key),
        AskWhat::JumpMark(key) => ("jump", key),
        AskWhat::Follow { .. } => return,
    };
    let token = mint_token();
    ask().lock().expect("caret ask mutex poisoned").insert(
        window,
        Ask {
            browser_id: browser.identifier(),
            token: token.clone(),
            what,
        },
    );
    let code = format!(
        "{CARET_JS}\nwindow.__bru_caret.mark(\"{token}\",\"{verb}\",\"{key}\");"
    );
    frame.execute_java_script(Some(&CefString::from(code.as_str())), None, 0);
}

/// A URL with its fragment stripped — the key both mark tables use. "strip the fragment as it may
/// interfere with scrolling", tabbedbrowser.py:1084.
fn mark_url(browser: &mut Browser) -> String {
    let Some(frame) = browser.main_frame() else {
        return String::new();
    };
    let url = CefString::from(&frame.url()).to_string();
    match url.split_once('#') {
        Some((head, _)) => head.to_string(),
        None => url,
    }
}

/// `tabbedbrowser.set_mark`. Upper case is global and remembers its URL; lower case is this page's.
fn save_mark(url: &str, key: char, x: i32, y: i32) {
    let mut guard = marks().lock().expect("marks mutex poisoned");
    if key.is_uppercase() {
        guard.global.insert(key, (x, y, url.to_string()));
    } else {
        guard.local.insert((url.to_string(), key), (x, y));
    }
    eprintln!("bru[caret]: mark {key} set at {x},{y}");
}

fn lookup_mark(url: &str, key: char) -> Option<(i32, i32, Option<String>)> {
    let guard = marks().lock().expect("marks mutex poisoned");
    if key.is_uppercase() {
        guard
            .global
            .get(&key)
            .map(|(x, y, url)| (*x, *y, Some(url.clone())))
    } else {
        guard
            .local
            .get(&(url.to_string(), key))
            .map(|(x, y)| (*x, *y, None))
    }
}

// ------------------------------------------------------------------------------------------------
// What the page answers
// ------------------------------------------------------------------------------------------------

/// A `{"type":"caret"}` query from a web page. Called by `src/ipc.rs` **before** its `bru://` check,
/// and the second thing (after a hint answer) a page may say to bru.
///
/// Returns false for anything that is not an answer to a request bru made, which `ipc.rs` turns into
/// a failed query.
pub fn on_page_query(browser: Option<&Browser>, request: &str) -> bool {
    let Some(browser) = browser else {
        return false;
    };
    let id = browser.identifier();
    // **Raw here; decoded inside each arm, after that arm's token check has passed.** Both fields
    // are page-controlled bytes and `percent_decode` is a parser running on hostile input, so it runs
    // only for an answer bru has already decided to believe. It is panic-free today; the ordering is
    // what stops a future edit to it turning a forged query into an abort of the whole process.
    // `hints.rs::on_page_query` does the same, one check earlier — it has a single guard, this has
    // one per kind.
    let (Some(token), Some(kind), Some(raw_data)) = (
        field(request, "token"),
        field(request, "kind"),
        field(request, "data"),
    ) else {
        return false;
    };
    let raw_text = field(request, "text").unwrap_or_default();

    let Some(state) = BruState::instance() else {
        return false;
    };
    // Which window's session or request this answer is for. `None` is a browser bru never put in a
    // window, which no page bru asked anything of can be.
    let Some(window) = state
        .lock()
        .expect("state mutex poisoned")
        .window_of_browser(id)
    else {
        debug(&format!("refused: browser {id} is in no window"));
        return false;
    };
    let mut browser = browser.clone();
    debug(&format!(
        "answer kind={kind} from browser {id} in window {window} token {token}"
    ));

    match kind.as_str() {
        "state" => {
            {
                let guard = sessions().lock().expect("caret sessions mutex poisoned");
                let Some(open) = guard.get(&window) else {
                    debug(&format!("refused: no caret session is open in window {window}"));
                    return false;
                };
                if open.browser_id != id || open.token != token {
                    debug(&format!(
                        "refused: window {window}'s session is browser {} token {}",
                        open.browser_id, open.token
                    ));
                    return false;
                }
            }
            let data = percent_decode(&raw_data);
            let text = percent_decode(&raw_text);
            on_state(&state, window, &mut browser, &data, text);
        }
        "mark" | "follow" => {
            let asked = {
                let mut guard = ask().lock().expect("caret ask mutex poisoned");
                match guard.get(&window) {
                    Some(open) if open.browser_id == id && open.token == token => {
                        guard.remove(&window).map(|open| open.what)
                    }
                    _ => None,
                }
            };
            let Some(what) = asked else {
                return false;
            };
            let data = percent_decode(&raw_data);
            let text = percent_decode(&raw_text);
            match what {
                AskWhat::SetMark(key) | AskWhat::JumpMark(key) => {
                    on_mark(&state, window, &mut browser, what_is_jump(&data), key, &data)
                }
                AskWhat::Follow { tab } => on_follow(&mut browser, tab, &data, &text),
            }
        }
        _ => return false,
    }
    true
}

fn what_is_jump(data: &str) -> bool {
    data.starts_with("jump|")
}

/// The page has applied an op list and is reporting what it left behind.
fn on_state(state: &SharedState, window: u32, browser: &mut Browser, data: &str, text: String) {
    // `<box>|<collapsed>|<viewport>`, where box is `x,y,w,h` or empty.
    let mut parts = data.split('|');
    let caret = parse_four(parts.next().unwrap_or(""));
    let collapsed = parts.next().unwrap_or("1") != "0";
    let viewport = parse_two(parts.next().unwrap_or("")).unwrap_or((0, 0));

    {
        let mut guard = sessions().lock().expect("caret sessions mutex poisoned");
        let Some(open) = guard.get_mut(&window) else {
            return;
        };
        open.caret = caret;
        open.viewport = viewport;
        open.text = text.clone();
        // **The selection state is not touched here.** A report only ever informs; it never writes.
        // Measured 2026-08-06: `V` from normal mode is `mode-enter caret ;; selection-toggle
        // --line`, and the report `enter` asks for arrives *after* the toggle has set `Line`. An
        // earlier version downgraded `Line` to `Normal` whenever a report said the selection was
        // collapsed, and that stale first report did exactly that — the next `j` came out as a plain
        // extend with no re-anchoring, so `V` behaved as `v` from the second line on. qutebrowser
        // does not downgrade either: `CaretBrowsing.move` never assigns `selectionState`.
    }

    eprintln!(
        "bru[caret]: window {window} caret {} collapsed={} selection {:?}",
        caret.map(|(x, y, w, h)| format!("{x},{y} {w}x{h}")).unwrap_or_else(|| "?".into()),
        i32::from(collapsed),
        elide(&text),
    );
    // The selection is what the search-match slot of the bar is free to show while caret mode is
    // open; it is the only visible confirmation that a movement did anything. Aimed at the window
    // whose page reported it — a report arrives asynchronously and can easily land while another
    // window is in front.
    crate::ipc::set_search_match_for(
        window,
        if text.is_empty() {
            String::new()
        } else {
            format!("[{} chars]", text.chars().count())
        },
    );

    keep_caret_in_view(state, browser, caret, viewport);
}

/// `CaretBrowsing.updateCaretOrSelection`'s scrolling half (javascript/caret.js:1115), decided here
/// and performed on the wheel.
///
/// qutebrowser calls `window.scroll` from the page. bru cannot: DESIGN.md's one non-negotiable rule
/// is that movement goes through `send_mouse_wheel_event`. So the page reports the caret's box and
/// the viewport it was measured in, and the distance is computed here.
fn keep_caret_in_view(
    state: &SharedState,
    browser: &mut Browser,
    caret: Option<(i32, i32, i32, i32)>,
    viewport: (i32, i32),
) {
    /// The margin caret.js leaves above and below when it scrolls the caret back into view.
    const MARGIN: i32 = 100;

    let (Some((_, y, _, h)), (_, height)) = (caret, viewport) else {
        return;
    };
    if height <= 0 {
        return;
    }
    let dy = if y + h > height {
        y + h - height + MARGIN
    } else if y < 0 {
        y - MARGIN
    } else {
        return;
    };
    debug(&format!("caret at y={y} h={h} in {height}: scrolling {dy}"));
    crate::scroll::scroll_px(state, browser, 0, dy, None);
}

/// The page has reported where it is scrolled to, for `` ` `` or `'`.
fn on_mark(
    state: &SharedState,
    window: u32,
    browser: &mut Browser,
    jump: bool,
    key: char,
    data: &str,
) {
    let Some(position) = data.rsplit('|').next().and_then(parse_three) else {
        return;
    };
    let (x, y, _max_y) = position;
    let url = mark_url(browser);

    if !jump {
        save_mark(&url, key, x, y);
        return;
    }

    let Some((target_x, target_y, target_url)) = lookup_mark(&url, key) else {
        eprintln!("bru: mark {key} is not set");
        return;
    };

    // "save the pre-jump position in the special ' mark. this has to happen after we read the mark,
    // otherwise jump_mark ' would just jump to the current position every time"
    // (tabbedbrowser.py:1133).
    save_mark(&url, '\'', x, y);

    match target_url {
        // A global mark on another page: navigate first, then place the position once the page is
        // tall enough to hold it. **Posted**, because this runs inside the message router's query
        // handler and starting a navigation there deadlocks (CEF-NOTES trap 12).
        Some(target) if target != url => {
            pending_jump()
                .lock()
                .expect("pending jump mutex poisoned")
                .insert(window, (key, target_x, target_y, 6));
            let mut task = JumpToMark::new(window, target, target_x, target_y);
            post_task(ThreadId::UI, Some(&mut task));
        }
        _ => {
            eprintln!(
                "bru[caret]: window {window} jumping to mark {key} at \
                 {target_x},{target_y} from {x},{y}"
            );
            crate::scroll::scroll_px(state, browser, target_x - x, target_y - y, None);
        }
    }
}

wrap_task! {
    struct JumpToMark {
        window: u32,
        url: String,
        x: i32,
        y: i32,
    }

    impl Task {
        fn execute(&self) {
            let Some(state) = BruState::instance() else {
                return;
            };
            // The window the `'A` was typed in, not the one in front. `active_browser` was here
            // before, and a jump typed in a background window loaded the mark's page into whichever
            // window happened to be current — the one place a caret request could act on somebody
            // else's tab.
            let browser = state
                .lock()
                .expect("state mutex poisoned")
                .active_browser_in(self.window);
            let Some(mut browser) = browser else {
                return;
            };
            eprintln!(
                "bru[caret]: window {} loading {} for a global mark",
                self.window, self.url
            );
            crate::open::open(&state, &mut browser, Some(&self.url), false, false);
            let mut task = PlaceMark::new(self.window);
            post_delayed_task(ThreadId::UI, Some(&mut task), 600);
        }
    }
}

wrap_task! {
    struct PlaceMark {
        window: u32,
    }

    impl Task {
        fn execute(&self) {
            let pending = pending_jump()
                .lock()
                .expect("pending jump mutex poisoned")
                .get(&self.window)
                .copied();
            let Some((key, x, y, left)) = pending else {
                return;
            };
            let Some(state) = BruState::instance() else {
                return;
            };
            let browser = state
                .lock()
                .expect("state mutex poisoned")
                .active_browser_in(self.window);
            let Some(mut browser) = browser else {
                return;
            };

            // The page has to be tall enough to hold the position before the jump means anything;
            // a load that is still laying out reports a max_y of zero and the scroll is thrown away.
            let position = crate::scroll::position();
            let tall_enough = position.map(|p| p.max_y as i32 >= y).unwrap_or(false);
            if tall_enough {
                let at = position.map(|p| p.y as i32).unwrap_or(0);
                eprintln!(
                    "bru[caret]: window {} placing global mark {key} at {x},{y} (page is at {at})",
                    self.window
                );
                crate::scroll::scroll_px(&state, &mut browser, x, y - at, None);
                pending_jump()
                    .lock()
                    .expect("pending jump mutex poisoned")
                    .remove(&self.window);
                return;
            }

            crate::scroll::request_position(&mut browser);
            if left == 0 {
                eprintln!("bru: mark {key}'s page never grew tall enough to hold {y}");
                pending_jump()
                    .lock()
                    .expect("pending jump mutex poisoned")
                    .remove(&self.window);
                return;
            }
            pending_jump()
                .lock()
                .expect("pending jump mutex poisoned")
                .insert(self.window, (key, x, y, left - 1));
            let mut task = PlaceMark::new(self.window);
            post_delayed_task(ThreadId::UI, Some(&mut task), 400);
        }
    }
}

/// The page has reported the link the selection is in, for `selection-follow`.
fn on_follow(browser: &mut Browser, tab: bool, data: &str, url: &str) {
    let point = data.split('|').nth(1).and_then(parse_two);

    if tab {
        if url.is_empty() {
            eprintln!("bru: nothing to follow in the selection");
            return;
        }
        eprintln!("bru[caret]: following {url} into a background tab");
        // Trap 12 again: a tab cannot be opened from inside the query handler.
        let mut task = FollowInTab::new(url.to_string());
        post_task(ThreadId::UI, Some(&mut task));
        return;
    }

    let Some((x, y)) = point else {
        eprintln!("bru: nothing to follow in the selection");
        return;
    };
    eprintln!("bru[caret]: following the selected link at {x},{y}");
    // A real click, for the same reason `src/hints.rs` uses one: a synthetic `element.click()` skips
    // hover, focus and every handler that checks `isTrusted`.
    let Some(host) = browser.host() else {
        return;
    };
    let event = MouseEvent { x, y, modifiers: 0 };
    host.send_mouse_move_event(Some(&event), 0);
    host.send_mouse_click_event(Some(&event), MouseButtonType::LEFT, 0, 1);
    host.send_mouse_click_event(Some(&event), MouseButtonType::LEFT, 1, 1);
}

wrap_task! {
    struct FollowInTab {
        url: String,
    }

    impl Task {
        fn execute(&self) {
            if let Some(state) = BruState::instance() {
                crate::tabs::new_tab(&state, &self.url, true);
            }
        }
    }
}

// ------------------------------------------------------------------------------------------------
// Reading the page's answer
// ------------------------------------------------------------------------------------------------

/// A token no page can guess, so that the only caret answer bru believes is the one it asked for.
///
/// **The same function `hints.rs` uses, called rather than copied.** It was a copy — byte for byte
/// identical — and the two would have had to be fixed twice; the reasoning for what the token is
/// and is not lives with it there.
fn mint_token() -> String {
    crate::hints::mint_token()
}

/// One field out of the flat object `chrome/caret.js` sends. Both of its values are
/// percent-encoded, so neither can contain a quote and this needs no escape handling.
fn field(src: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":\"");
    let at = src.find(&needle)? + needle.len();
    let rest = &src[at..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Undo `encodeURIComponent`. Only the bytes it escapes appear, and it always emits `%XX` pairs.
fn percent_decode(src: &str) -> String {
    // **Bytes throughout, and never a slice of the `str`.** This read `&src[i + 1..i + 3]` and
    // parsed that, which panics the moment the two bytes after a `%` are not a character boundary —
    // `percent_decode("%aé")` aborted the process, because index 3 lands inside the two bytes of
    // `é`. The input is a page-controlled field on the one query a web page is allowed to send, and
    // a panic on the UI thread ends the browser: any site could have closed bru with one string.
    // Measured 2026-08-09, standalone, before the fix. Indexing the byte array cannot panic on a
    // boundary because a byte array has none.
    let bytes = src.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    let hex = |byte: u8| (byte as char).to_digit(16);
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((high * 16 + low) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    // Whatever the bytes turn out to be, this is where malformed UTF-8 stops: a page can send any
    // sequence and gets replacement characters rather than an error path.
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_two(src: &str) -> Option<(i32, i32)> {
    let mut parts = src.split(',');
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

fn parse_three(src: &str) -> Option<(i32, i32, i32)> {
    let mut parts = src.split(',');
    Some((
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    ))
}

fn parse_four(src: &str) -> Option<(i32, i32, i32, i32)> {
    let mut parts = src.split(',');
    Some((
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    ))
}

/// A selection, short enough for one line of stderr and with its newlines shown.
fn elide(text: &str) -> String {
    const KEEP: usize = 60;
    let flat: String = text
        .chars()
        .map(|c| if c == '\n' { '⏎' } else { c })
        .collect();
    if flat.chars().count() <= KEEP {
        return format!("{flat:?}");
    }
    let head: String = flat.chars().take(KEEP).collect();
    format!("{head:?}… ({} chars)", text.chars().count())
}

// ------------------------------------------------------------------------------------------------
// The debug switch
// ------------------------------------------------------------------------------------------------

/// `--caret-script=v,3j,w,report,esc --caret-step-ms=1200` drives caret mode and marks from posted
/// UI tasks: each step is a key sequence fed to the real key parser for whatever mode bru is in, so
/// `v` enters caret mode through `mode-enter caret` and `3j` goes through the count machinery. `esc`
/// is `<Escape>` and `report` prints the session without pressing anything.
///
/// It exists for the same reason as `--hint-script`. The only key-injection tool on this machine is
/// `wtype`, which attaches a virtual keyboard, and CEF segfaults in `xkb_state_update_mask` when the
/// keymap arrives — measured 2026-08-06, 2/3 runs, with the leftover keystrokes landing in whatever
/// the compositor focuses next. Inert unless the switch is passed.
pub fn schedule_caret_script(steps: &str, interval_ms: i64) {
    for (i, step) in steps.split(',').filter(|s| !s.is_empty()).enumerate() {
        let mut task = CaretStep::new(step.to_string());
        post_delayed_task(ThreadId::UI, Some(&mut task), interval_ms * (i as i64 + 1));
    }
}

wrap_task! {
    struct CaretStep {
        step: String,
    }

    impl Task {
        fn execute(&self) {
            let Some(state) = BruState::instance() else {
                return;
            };
            let browser = state.lock().expect("state mutex poisoned").active_browser();
            let Some(mut browser) = browser else {
                eprintln!("caret-script: no tab to aim at");
                return;
            };

            let keys = match self.step.as_str() {
                "report" => String::new(),
                "esc" => "<Escape>".to_string(),
                other => other.to_string(),
            };

            if !keys.is_empty() {
                match crate::bindings::parse_key_sequence(&keys) {
                    Ok(sequence) => {
                        for info in sequence {
                            press(&state, &mut browser, info);
                        }
                    }
                    Err(e) => eprintln!("caret-script: {keys:?}: {e}"),
                }
            }

            // Every window's mode and every window's session, for the same reason `--hint-script`
            // prints all of them: two open at once is the claim, and one line about the current
            // window could not tell that state from the one session this replaced.
            let modes = {
                let guard = state.lock().expect("state mutex poisoned");
                guard
                    .window_ids()
                    .into_iter()
                    .map(|window| format!("win{window} {}", guard.mode_in(window)))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let open = {
                let guard = sessions().lock().expect("caret sessions mutex poisoned");
                let mut windows: Vec<u32> = guard.keys().copied().collect();
                windows.sort_unstable();
                windows
                    .into_iter()
                    .map(|window| {
                        let session = &guard[&window];
                        format!(
                            "win{window}: selection {} on browser {} text {}",
                            session.selection.name(),
                            session.browser_id,
                            elide(&session.text),
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("; ")
            };
            eprintln!(
                "caret-script: after {:?} -> modes [{modes}], sessions [{open}]",
                self.step,
            );
        }
    }
}

/// One key, down the same path `src/keys.rs` takes: the register parser first, then the mode's own
/// trie, then the dispatcher.
fn press(state: &SharedState, browser: &mut Browser, info: KeyInfo) {
    if handle_mark_key(state, browser, info).is_some() {
        return;
    }
    let Some(outcome) = state.lock().expect("state mutex poisoned").handle_key(info) else {
        return;
    };
    crate::ipc::set_keystring(outcome.keystring.clone());
    if let crate::bindings::KeyAction::Run { command, count } = outcome.action {
        crate::exec::run(state, browser, &command, count);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(ops: &[Op]) -> String {
        ops.iter().map(|op| op.to_json()).collect::<Vec<_>>().join(",")
    }

    #[test]
    fn a_move_without_a_selection_moves_and_with_one_extends() {
        // javascript/caret.js:1153 — the whole of what selection state changes about a movement.
        assert_eq!(
            json(&ops_for(CaretMove::NextLine, SelectionState::None, 1)),
            "[\"modify\",\"move\",\"forward\",\"line\"]"
        );
        assert_eq!(
            json(&ops_for(CaretMove::NextLine, SelectionState::Normal, 1)),
            "[\"modify\",\"extend\",\"forward\",\"line\"]"
        );
    }

    #[test]
    fn a_count_repeats_the_primitive() {
        // `3j` is three modifies, not one three times as long: Selection.modify takes no count, and
        // qutebrowser's own `move` loops (javascript/caret.js:1160).
        let ops = ops_for(CaretMove::NextLine, SelectionState::None, 3);
        assert_eq!(ops.len(), 3);
        assert!(ops.iter().all(|op| *op == Op::Modify("move", "forward", "line")));

        // The four that take no count in qutebrowser take none here either.
        for kind in [
            CaretMove::StartOfLine,
            CaretMove::EndOfLine,
            CaretMove::StartOfDocument,
            CaretMove::EndOfDocument,
        ] {
            assert_eq!(ops_for(kind, SelectionState::None, 5).len(), 1, "{kind:?} ignores a count");
        }
    }

    #[test]
    fn w_is_e_plus_a_character() {
        // Chromium's "forward word" lands on the end of the word; vim's `w` is the start of the
        // next one. javascript/caret.js:1370.
        assert_eq!(
            json(&ops_for(CaretMove::EndOfWord, SelectionState::None, 1)),
            "[\"modify\",\"move\",\"forward\",\"word\"]"
        );
        assert_eq!(
            json(&ops_for(CaretMove::NextWord, SelectionState::None, 1)),
            "[\"modify\",\"move\",\"forward\",\"word\"],[\"modify\",\"move\",\"right\",\"character\"]"
        );
        // The extra character is one, whatever the count: `3w` is three words and then one step.
        assert_eq!(ops_for(CaretMove::NextWord, SelectionState::None, 3).len(), 4);
    }

    #[test]
    fn a_line_selection_re_anchors_and_ignores_the_fine_granularities() {
        // javascript/caret.js:1145: updateLineSelection acts only for granularities coarser than a
        // word, so in a `V` selection h/l/w/b/e do nothing at all. Bug-compatible on purpose.
        for kind in [
            CaretMove::NextChar,
            CaretMove::PrevChar,
            CaretMove::NextWord,
            CaretMove::PrevWord,
            CaretMove::EndOfWord,
        ] {
            assert!(
                ops_for(kind, SelectionState::Line, 1).is_empty(),
                "{kind:?} moves a line selection in qutebrowser, and it should not"
            );
        }
        // `j` does move it, and rebuilds the line each time.
        assert_eq!(
            json(&ops_for(CaretMove::NextLine, SelectionState::Line, 1)),
            "[\"modify\",\"extend\",\"forward\",\"line\"],\
             [\"modify\",\"extend\",\"right\",\"lineboundary\"],[\"reverse\"],\
             [\"modify\",\"extend\",\"left\",\"lineboundary\"],[\"reverse\"]"
        );
    }

    #[test]
    fn a_block_move_is_a_paragraph_then_its_boundary() {
        // javascript/caret.js:1186. `]` is start-of-next-block: forward a paragraph, then back to
        // that paragraph's start.
        assert_eq!(
            json(&ops_for(CaretMove::StartOfNextBlock, SelectionState::None, 1)),
            "[\"modify\",\"move\",\"forward\",\"paragraph\"],\
             [\"modify\",\"move\",\"backward\",\"paragraphboundary\"]"
        );
        assert_eq!(
            json(&ops_for(CaretMove::EndOfPrevBlock, SelectionState::None, 1)),
            "[\"modify\",\"move\",\"backward\",\"paragraph\"],\
             [\"modify\",\"move\",\"forward\",\"paragraphboundary\"]"
        );
        // Unlike `move`, a block move in a line selection is *not* skipped — it modifies and then
        // rebuilds the line.
        assert_eq!(ops_for(CaretMove::StartOfNextBlock, SelectionState::Line, 1).len(), 6);
    }

    #[test]
    fn a_register_key_is_the_character_that_was_typed() {
        let key = |s: &str| crate::bindings::parse_key_sequence(s).unwrap()[0];
        // Shift is what tells a local mark from a global one, so it is the one modifier that does
        // not disqualify a key.
        assert_eq!(register_char(key("a")), Some('a'));
        assert_eq!(register_char(key("A")), Some('A'));
        assert_eq!(register_char(key("'")), Some('\''));
        assert_eq!(register_char(key("3")), Some('3'));
        // `is_special`: a modifier beyond Shift, or a key that types nothing.
        assert_eq!(register_char(key("<Ctrl-a>")), None);
        assert_eq!(register_char(key("<Escape>")), None);
        assert_eq!(register_char(key("<F5>")), None);
    }

    #[test]
    fn a_lower_case_mark_is_per_page_and_an_upper_case_one_is_global() {
        // tabbedbrowser.py:1078 — "capital indicates a global mark". The local table is keyed by
        // the URL as well, so `a` on one page is not `a` on another.
        marks().lock().unwrap().local.clear();
        marks().lock().unwrap().global.clear();

        save_mark("https://a.example/", 'a', 0, 100);
        save_mark("https://b.example/", 'a', 0, 200);
        save_mark("https://a.example/", 'A', 0, 300);

        assert_eq!(lookup_mark("https://a.example/", 'a'), Some((0, 100, None)));
        assert_eq!(lookup_mark("https://b.example/", 'a'), Some((0, 200, None)));
        assert_eq!(lookup_mark("https://c.example/", 'a'), None);
        // A global mark answers from any page, and carries the URL it belongs to.
        assert_eq!(
            lookup_mark("https://c.example/", 'A'),
            Some((0, 300, Some("https://a.example/".to_string())))
        );
    }

    #[test]
    fn the_page_payload_is_read_back_exactly() {
        let request = "{\"type\":\"caret\",\"token\":\"deadbeef\",\"kind\":\"state\",\
                       \"data\":\"10%2C20%2C2%2C16%7C0%7C1928%2C1257\",\
                       \"text\":\"a%20b%0Ac\"}";
        assert_eq!(field(request, "token").as_deref(), Some("deadbeef"));
        assert_eq!(field(request, "kind").as_deref(), Some("state"));
        let data = percent_decode(&field(request, "data").unwrap());
        assert_eq!(data, "10,20,2,16|0|1928,1257");
        assert_eq!(percent_decode(&field(request, "text").unwrap()), "a b\nc");

        let mut parts = data.split('|');
        assert_eq!(parse_four(parts.next().unwrap()), Some((10, 20, 2, 16)));
        assert_eq!(parts.next(), Some("0"));
        assert_eq!(parse_two(parts.next().unwrap()), Some((1928, 1257)));

        // The mark payload, which is what `` ` `` and `'` come back as.
        assert!(what_is_jump("jump|a|0,1200,8000"));
        assert!(!what_is_jump("set|a|0,1200,8000"));
        assert_eq!(parse_three("0,1200,8000"), Some((0, 1200, 8000)));

        // A selection containing the separators the payload uses survives, which is the whole
        // reason it travels in a field of its own.
        assert_eq!(percent_decode("a%7Cb%2Cc%22d"), "a|b,c\"d");
    }
    /// A page cannot end the browser with a percent sign in front of a multi-byte character.
    ///
    /// **This aborted the process before 2026-08-09.** The decoder parsed `&src[i + 1..i + 3]` — a
    /// slice of the `str` by byte index — and `%aé` puts index 3 inside the two bytes of `é`, which
    /// is a panic, on the UI thread, from the one field a web page is allowed to put bytes in.
    /// Every case below returns the input unchanged because none of them is a valid escape; what is
    /// being asserted is that they *return*.
    #[test]
    fn a_malformed_escape_is_left_alone_rather_than_ending_the_process() {
        assert_eq!(percent_decode("%aé"), "%aé");
        assert_eq!(percent_decode("%é"), "%é");
        assert_eq!(percent_decode("%"), "%");
        assert_eq!(percent_decode("%A"), "%A");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_decode("%%41"), "%A");
        // And a well-formed one still decodes, so the guard did not swallow the feature.
        assert_eq!(percent_decode("a%7Cb%2Cc%22d"), "a|b,c\"d");
        assert_eq!(percent_decode("%E2%9C%93"), "\u{2713}");
    }

}
