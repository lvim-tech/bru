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

fn session() -> &'static Mutex<Option<Session>> {
    static SESSION: Mutex<Option<Session>> = Mutex::new(None);
    &SESSION
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

fn ask() -> &'static Mutex<Option<Ask>> {
    static ASK: Mutex<Option<Ask>> = Mutex::new(None);
    &ASK
}

/// `tabbedbrowser._local_marks` and `_global_marks`.
///
/// Lower case is per page and holds a scroll position; upper case is global and holds a position
/// **and** the URL it belongs to, which is what makes `'A` a navigation. Both are keyed by a URL
/// with its fragment stripped, "as it may interfere with scrolling" (tabbedbrowser.py:1084).
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
/// enough to hold the position. `(key, x, y, attempts left)`.
fn pending_jump() -> &'static Mutex<Option<(char, i32, i32, u32)>> {
    static PENDING: Mutex<Option<(char, i32, i32, u32)>> = Mutex::new(None);
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
pub fn on_mode_change(browser: &mut Browser, from: Mode, to: Mode) {
    if from == to {
        return;
    }
    if to == Mode::Caret {
        enter(browser);
    } else if from == Mode::Caret {
        leave(browser);
    }
}

fn enter(browser: &mut Browser) {
    let Some(frame) = browser.main_frame() else {
        return;
    };
    let token = mint_token();
    *session().lock().expect("caret session mutex poisoned") = Some(Session {
        browser_id: browser.identifier(),
        token: token.clone(),
        selection: SelectionState::None,
        text: String::new(),
        caret: None,
        viewport: (0, 0),
    });

    // Injected on every `v` rather than once per page load, for the same reason `src/hints.rs`
    // injects on every `f`: a navigation throws the world away and there is no cheap way to know
    // from here whether this one still has the script.
    let code = format!("{CARET_JS}\nwindow.__bru_caret.enter(\"{token}\");");
    frame.execute_java_script(Some(&CefString::from(code.as_str())), None, 0);
    debug("entered");
}

fn leave(browser: &mut Browser) {
    *session().lock().expect("caret session mutex poisoned") = None;
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
    crate::ipc::set_search_match(String::new());
    debug("left");
}

/// The text the page last reported as selected, and the state that selection is in.
///
/// **This is the call the clipboard workstream needs.** `y` / `Y` / `<Return>` in caret mode are
/// `yank selection` and `yank selection -s`, which are `Command::Unimplemented` here on purpose —
/// adding a `Command::Yank` variant would collide with the module that owns `wl-copy`. That module
/// asks here for the text and does the copying; nothing in this file touches a clipboard.
// Unused inside this crate until that module lands, which is exactly the hole this documents.
#[allow(dead_code)]
pub fn selection() -> Option<(SelectionState, String)> {
    let guard = session().lock().expect("caret session mutex poisoned");
    guard.as_ref().map(|s| (s.selection, s.text.clone()))
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
    let _ = state;
    let count = count.unwrap_or(1).clamp(1, 1000);
    let Some((token, selection)) = current(browser) else {
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
    let _ = state;
    let Some((token, was)) = current(browser) else {
        return;
    };
    let now = if line {
        SelectionState::Line
    } else if was != SelectionState::Normal {
        SelectionState::Normal
    } else {
        SelectionState::None
    };
    set_state(now);

    let ops = if line { select_line() } else { Vec::new() };
    // Even with no ops the page is asked to report, so the status line and `selection()` learn that
    // the state changed under an unmoved caret.
    run_ops(browser, &token, ops);
    debug(&format!("selection {} -> {}", was.name(), now.name()));
}

/// `selection-drop` — `<Ctrl-Space>`.
pub fn selection_drop(state: &SharedState, browser: &mut Browser) {
    let _ = state;
    let Some((token, _)) = current(browser) else {
        return;
    };
    set_state(SelectionState::None);
    run_ops(browser, &token, vec![Op::Drop]);
}

/// `selection-reverse` — `o`.
pub fn selection_reverse(state: &SharedState, browser: &mut Browser) {
    let _ = state;
    let Some((token, _)) = current(browser) else {
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
    let _ = state;
    let Some(frame) = browser.main_frame() else {
        return;
    };
    let token = mint_token();
    *ask().lock().expect("caret ask mutex poisoned") = Some(Ask {
        browser_id: browser.identifier(),
        token: token.clone(),
        what: AskWhat::Follow { tab },
    });
    let code = format!(
        "{CARET_JS}\nwindow.__bru_caret.follow(\"{token}\",\"{}\");",
        i32::from(tab)
    );
    frame.execute_java_script(Some(&CefString::from(code.as_str())), None, 0);
}

/// The session's token and selection state, or `None` when caret mode is not open on this browser.
fn current(browser: &mut Browser) -> Option<(String, SelectionState)> {
    let id = browser.identifier();
    let guard = session().lock().expect("caret session mutex poisoned");
    let open = guard.as_ref()?;
    if open.browser_id != id {
        return None;
    }
    Some((open.token.clone(), open.selection))
}

fn set_state(selection: SelectionState) {
    if let Some(open) = session()
        .lock()
        .expect("caret session mutex poisoned")
        .as_mut()
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
    let mode = state.lock().expect("state mutex poisoned").mode();
    if !mode.names_a_register() {
        return None;
    }

    // The `register:` bindings are consulted first (`super().handle(e)`), and they hold exactly one
    // entry: `<Escape>: mode-leave`.
    if info.key == Key::Named(NamedKey::Escape) {
        leave_register_mode(state);
        return Some(true);
    }

    // "this is not a proper register key, let it pass and keep going" — `info.is_special()`, which is
    // a modifier beyond Shift or a key that types nothing. The mode stays open waiting for a real
    // one.
    let Some(key) = register_char(info) else {
        return Some(true);
    };

    leave_register_mode(state);
    match mode {
        Mode::SetMark => request_mark(browser, AskWhat::SetMark(key)),
        Mode::JumpMark => request_mark(browser, AskWhat::JumpMark(key)),
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

fn leave_register_mode(state: &SharedState) {
    let now = {
        let mut guard = state.lock().expect("state mutex poisoned");
        guard.leave_mode();
        guard.mode()
    };
    crate::ipc::set_mode(now.name().to_string());
    crate::ipc::set_keystring(String::new());
}

/// Ask the page where it is scrolled to, so a mark can be saved or a jump measured.
fn request_mark(browser: &mut Browser, what: AskWhat) {
    let Some(frame) = browser.main_frame() else {
        return;
    };
    let (verb, key) = match what {
        AskWhat::SetMark(key) => ("set", key),
        AskWhat::JumpMark(key) => ("jump", key),
        AskWhat::Follow { .. } => return,
    };
    let token = mint_token();
    *ask().lock().expect("caret ask mutex poisoned") = Some(Ask {
        browser_id: browser.identifier(),
        token: token.clone(),
        what,
    });
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
    let (Some(token), Some(kind), Some(data)) = (
        field(request, "token"),
        field(request, "kind"),
        field(request, "data").map(|d| percent_decode(&d)),
    ) else {
        return false;
    };
    let text = field(request, "text")
        .map(|t| percent_decode(&t))
        .unwrap_or_default();

    let Some(state) = BruState::instance() else {
        return false;
    };
    let mut browser = browser.clone();
    debug(&format!("answer kind={kind} from browser {id} token {token}"));

    match kind.as_str() {
        "state" => {
            {
                let guard = session().lock().expect("caret session mutex poisoned");
                let Some(open) = guard.as_ref() else {
                    debug("refused: no caret session is open");
                    return false;
                };
                if open.browser_id != id || open.token != token {
                    debug(&format!(
                        "refused: session is browser {} token {}",
                        open.browser_id, open.token
                    ));
                    return false;
                }
            }
            on_state(&state, &mut browser, &data, text);
        }
        "mark" | "follow" => {
            let asked = {
                let mut guard = ask().lock().expect("caret ask mutex poisoned");
                match guard.as_ref() {
                    Some(open) if open.browser_id == id && open.token == token => {
                        guard.take().map(|open| open.what)
                    }
                    _ => None,
                }
            };
            let Some(what) = asked else {
                return false;
            };
            match what {
                AskWhat::SetMark(key) | AskWhat::JumpMark(key) => {
                    on_mark(&state, &mut browser, what_is_jump(&data), key, &data)
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
fn on_state(state: &SharedState, browser: &mut Browser, data: &str, text: String) {
    // `<box>|<collapsed>|<viewport>`, where box is `x,y,w,h` or empty.
    let mut parts = data.split('|');
    let caret = parse_four(parts.next().unwrap_or(""));
    let collapsed = parts.next().unwrap_or("1") != "0";
    let viewport = parse_two(parts.next().unwrap_or("")).unwrap_or((0, 0));

    {
        let mut guard = session().lock().expect("caret session mutex poisoned");
        let Some(open) = guard.as_mut() else {
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
        "bru[caret]: caret {} collapsed={} selection {:?}",
        caret.map(|(x, y, w, h)| format!("{x},{y} {w}x{h}")).unwrap_or_else(|| "?".into()),
        i32::from(collapsed),
        elide(&text),
    );
    // The selection is what the search-match slot of the bar is free to show while caret mode is
    // open; it is the only visible confirmation that a movement did anything.
    crate::ipc::set_search_match(if text.is_empty() {
        String::new()
    } else {
        format!("[{} chars]", text.chars().count())
    });

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
fn on_mark(state: &SharedState, browser: &mut Browser, jump: bool, key: char, data: &str) {
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
            *pending_jump().lock().expect("pending jump mutex poisoned") =
                Some((key, target_x, target_y, 6));
            let mut task = JumpToMark::new(target, target_x, target_y);
            post_task(ThreadId::UI, Some(&mut task));
        }
        _ => {
            eprintln!("bru[caret]: jumping to mark {key} at {target_x},{target_y} from {x},{y}");
            crate::scroll::scroll_px(state, browser, target_x - x, target_y - y, None);
        }
    }
}

wrap_task! {
    struct JumpToMark {
        url: String,
        x: i32,
        y: i32,
    }

    impl Task {
        fn execute(&self) {
            let Some(state) = BruState::instance() else {
                return;
            };
            let browser = state.lock().expect("state mutex poisoned").active_browser();
            let Some(mut browser) = browser else {
                return;
            };
            eprintln!("bru[caret]: loading {} for a global mark", self.url);
            crate::open::open(&state, &mut browser, Some(&self.url), false, false);
            let mut task = PlaceMark::new();
            post_delayed_task(ThreadId::UI, Some(&mut task), 600);
        }
    }
}

wrap_task! {
    struct PlaceMark;

    impl Task {
        fn execute(&self) {
            let Some((key, x, y, left)) = *pending_jump().lock().expect("pending jump mutex poisoned")
            else {
                return;
            };
            let Some(state) = BruState::instance() else {
                return;
            };
            let browser = state.lock().expect("state mutex poisoned").active_browser();
            let Some(mut browser) = browser else {
                return;
            };

            // The page has to be tall enough to hold the position before the jump means anything;
            // a load that is still laying out reports a max_y of zero and the scroll is thrown away.
            let position = crate::scroll::position();
            let tall_enough = position.map(|p| p.max_y as i32 >= y).unwrap_or(false);
            if tall_enough {
                let at = position.map(|p| p.y as i32).unwrap_or(0);
                eprintln!("bru[caret]: placing global mark {key} at {x},{y} (page is at {at})");
                crate::scroll::scroll_px(&state, &mut browser, x, y - at, None);
                *pending_jump().lock().expect("pending jump mutex poisoned") = None;
                return;
            }

            crate::scroll::request_position(&mut browser);
            if left == 0 {
                eprintln!("bru: mark {key}'s page never grew tall enough to hold {y}");
                *pending_jump().lock().expect("pending jump mutex poisoned") = None;
                return;
            }
            *pending_jump().lock().expect("pending jump mutex poisoned") =
                Some((key, x, y, left - 1));
            let mut task = PlaceMark::new();
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
/// Not a cryptographic RNG — the same reasoning as `src/hints.rs::mint_token`: this is a nonce
/// against a page volunteering a selection, combined with two other checks.
fn mint_token() -> String {
    use std::hash::{BuildHasher, Hasher, RandomState};
    let a = RandomState::new().build_hasher().finish();
    let b = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!("{a:016x}{b:016x}")
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
    let bytes = src.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&src[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
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

            let mode = state.lock().expect("state mutex poisoned").mode();
            let guard = session().lock().expect("caret session mutex poisoned");
            eprintln!(
                "caret-script: after {:?} -> mode {mode}, selection {}, text {}",
                self.step,
                guard.as_ref().map(|s| s.selection.name()).unwrap_or("-"),
                guard.as_ref().map(|s| elide(&s.text)).unwrap_or_else(|| "-".into()),
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
}
