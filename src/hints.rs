//! Hint mode — `f` and `F`.
//!
//! A behavioural port of qutebrowser 3.7.0's `browser/hints.py` (label generation, filtering) and
//! `javascript/webelem.js` (which elements are hintable, see `chrome/hints.js`).
//!
//! Three rules shape this file, and none of them is a preference:
//!
//! - **The follow is a real click.** `host.send_mouse_click_event` at the element's centre, on
//!   Chromium's own input path — the same reasoning as the wheel rule that bru exists for. A
//!   synthetic `element.click()` skips hover, focus and every handler that checks `isTrusted`;
//!   a real click does not.
//! - **Keys are matched in Rust.** [`BindingTrie`] was made generic over its value so that hint mode
//!   could put labels in it (`modeparsers.py:135`, `HintKeyParser.update_bindings`). The page draws
//!   what it is told and decides nothing; a keystroke never crosses into it to be matched.
//! - **The page is not trusted.** `chrome/hints.js` answers through the message router, which every
//!   page can reach. The answer is accepted only while a session bru itself started is open, only
//!   from that session's browser, and only carrying the token that session minted.

use cef::*;
use std::sync::Mutex;

use crate::bindings::{BindingTrie, Key, KeyInfo, Match, NamedKey};
use crate::modes::Mode;
use crate::state::BruState;
use crate::tabs::SharedState;

/// `hints.chars`, configdata.yml:1723. qutebrowser's default and bru's only value for now — it is
/// the home row, and DESIGN.md's "same keys" makes changing it a config question, not a code one.
pub const CHARS: &str = "asdfghjkl";

/// `hints.min_chars`, configdata.yml:1752.
const MIN_CHARS: usize = 1;

/// The page half, injected into the tab's main frame. Not served over `bru://`: it has to run in
/// the page's own world to see the page's elements.
const HINTS_JS: &str = include_str!("../chrome/hints.js");

/// What following a hint does. qutebrowser's `hints.Target`, cut to what M12 implements.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Target {
    /// `f` — click the element where it sits.
    Normal,
    /// `F` — take the element's URL and open it in a background tab.
    TabBg,
}

/// One run of hint mode: from `f` to a follow, an `<Escape>`, or a page that reported nothing.
struct Session {
    /// The tab this belongs to. A hint session survives no tab switch — checked on every answer.
    browser_id: i32,
    target: Target,
    /// Minted here, handed to the injected script, and required back on every answer.
    token: String,
    /// Click points in view coordinates, in the order the page reported them.
    points: Vec<(i32, i32)>,
    /// Labels, in the same order as `points`.
    labels: Vec<String>,
    /// label → index into `points`. The generic [`BindingTrie`] doing hint mode's half of its job.
    trie: BindingTrie<usize>,
    /// The hint characters typed so far.
    sequence: Vec<KeyInfo>,
    /// When `start` injected the script, so collection can be timed rather than asserted.
    started: std::time::Instant,
}

fn session() -> &'static Mutex<Option<Session>> {
    static SESSION: Mutex<Option<Session>> = Mutex::new(None);
    &SESSION
}

/// `BRU_DEBUG_HINTS=1` traces the labels and the keys that reach hint mode. Off by default: the
/// label list is one line per `f`, which is one line too many in a real session.
fn debug(message: &str) {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("BRU_DEBUG_HINTS").is_some()) {
        eprintln!("bru[hints]: {message}");
    }
}

// ------------------------------------------------------------------------------------------------
// Label generation — hints.py `_hint_strings` and below
// ------------------------------------------------------------------------------------------------

/// `utils.ceil_log`: `max(1, ceil(log(number, base)))`, in integer arithmetic so that the answer
/// does not depend on a float's last bit.
fn ceil_log(number: usize, base: usize) -> usize {
    assert!(number >= 1 && base >= 2, "math domain error");
    let mut result = 1;
    let mut accum = base;
    while accum < number {
        result += 1;
        accum *= base;
    }
    result
}

/// `_number_to_hint_str`: 8 becomes `jk`, padded to `digits` with the first character.
fn number_to_hint_str(mut number: usize, chars: &[char], digits: usize) -> String {
    let base = chars.len();
    let mut out: Vec<char> = Vec::new();
    loop {
        let remainder = number % base;
        out.insert(0, chars[remainder]);
        number -= remainder;
        number /= base;
        if number == 0 {
            break;
        }
    }
    while out.len() < digits {
        out.insert(0, chars[0]);
    }
    out.into_iter().collect()
}

/// `_shuffle_hints`: spread labels starting with the same character evenly through the list, so
/// that neighbouring links do not all begin with `a`.
fn shuffle_hints(hints: Vec<String>, length: usize) -> Vec<String> {
    let mut buckets: Vec<Vec<String>> = vec![Vec::new(); length];
    for (i, hint) in hints.into_iter().enumerate() {
        let bucket = i % length;
        buckets[bucket].push(hint);
    }
    buckets.into_iter().flatten().collect()
}

/// `_hint_scattered`, which is what `hints.scatter` (default true) selects.
///
/// Variable-length labels, Vimium-style: as many links as will fit get a label one character
/// shorter than the worst case. The short ones are never a prefix of a long one, because the long
/// ones start at `short_count * len(chars)` — which is what makes an exact match final and lets a
/// hint follow the moment it is complete.
pub fn hint_strings(count: usize) -> Vec<String> {
    if count == 0 {
        return Vec::new();
    }
    let chars: Vec<char> = CHARS.chars().collect();
    let needed = MIN_CHARS.max(ceil_log(count, chars.len()));

    let short_count = if needed > MIN_CHARS && needed > 1 {
        let total_space = chars.len().pow(needed as u32);
        total_space.saturating_sub(count) / (chars.len() - 1)
    } else {
        0
    };
    let long_count = count - short_count;

    let mut strings = Vec::with_capacity(count);
    if needed > 1 {
        for i in 0..short_count {
            strings.push(number_to_hint_str(i, &chars, needed - 1));
        }
    }
    let start = short_count * chars.len();
    for i in start..start + long_count {
        strings.push(number_to_hint_str(i, &chars, needed));
    }

    shuffle_hints(strings, chars.len())
}

// ------------------------------------------------------------------------------------------------
// Starting and ending a session
// ------------------------------------------------------------------------------------------------

/// `f` / `F` — start hint mode on `browser`.
///
/// Returns after asking the page for its elements; hint mode is not entered until the page answers,
/// which is what stops `f` on a blank tab from trapping the keyboard in a mode with nothing in it.
pub fn start(state: &SharedState, browser: &mut Browser, target: Target) {
    let Some(frame) = browser.main_frame() else {
        return;
    };
    let token = mint_token();

    // Whatever session was open is replaced. Its labels come off this page in `collect`, which
    // begins with the script's own `clear()`; on any other page they went when it was navigated.
    *session().lock().expect("hint session mutex poisoned") = Some(Session {
        browser_id: browser.identifier(),
        target,
        token: token.clone(),
        points: Vec::new(),
        labels: Vec::new(),
        trie: BindingTrie::new(),
        sequence: Vec::new(),
        started: std::time::Instant::now(),
    });

    // The script is injected on every `f` rather than once per page load: a navigation throws the
    // world away, and there is no cheap way to know from here whether this one still has it.
    let code = format!("{HINTS_JS}\nwindow.__bru_hints.collect(\"{token}\");");
    frame.execute_java_script(Some(&CefString::from(code.as_str())), None, 0);

    // Nothing enters hint mode here — `on_collected` does, once there is something to hint. `f` on
    // a page with no links must not trap the keyboard in a mode with nothing in it.
    let _ = state;
}

/// `<Escape>` in hint mode, and every path that ends a session.
pub fn cancel(state: &SharedState, browser: &mut Browser) {
    let had = session().lock().expect("hint session mutex poisoned").take().is_some();
    if had {
        clear_labels(browser);
    }
    leave_mode(state);
}

/// Take the labels off the page. Sent as its own script so that it runs even when the session is
/// already gone from Rust's side.
fn clear_labels(browser: &mut Browser) {
    let Some(frame) = browser.main_frame() else {
        return;
    };
    frame.execute_java_script(
        Some(&CefString::from(
            "window.__bru_hints && window.__bru_hints.clear();",
        )),
        None,
        0,
    );
}

fn leave_mode(state: &SharedState) {
    let now = {
        let mut guard = state.lock().expect("state mutex poisoned");
        if guard.mode() != Mode::Hint {
            return;
        }
        guard.leave_mode();
        guard.mode()
    };
    crate::ipc::set_mode(now.name().to_string());
    crate::ipc::set_keystring(String::new());
}

/// A token no page can guess, so that the only `hints` answer bru believes is the one it asked for.
///
/// Not a cryptographic RNG — bru has no dependency for one and this is not a key. It is a nonce
/// against a page volunteering coordinates, and it is combined with two other checks (an open
/// session, and the right browser).
fn mint_token() -> String {
    use std::hash::{BuildHasher, Hasher, RandomState};
    let a = RandomState::new().build_hasher().finish();
    let b = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!("{a:016x}{b:016x}")
}

// ------------------------------------------------------------------------------------------------
// What the page answers
// ------------------------------------------------------------------------------------------------

/// A `{"type":"hints"}` query from a web page. Called by `src/ipc.rs` **before** its `bru://` check,
/// and it is the only thing a page may say to bru.
///
/// Returns false for anything that is not an answer to a session bru started, which `ipc.rs` turns
/// into a failed query. Three things have to line up: a session is open, it belongs to the browser
/// the query came from, and it carries the token bru minted for it.
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

    {
        let guard = session().lock().expect("hint session mutex poisoned");
        let Some(open) = guard.as_ref() else {
            return false;
        };
        if open.browser_id != id || open.token != token {
            return false;
        }
    }

    let Some(state) = BruState::instance() else {
        return false;
    };
    let mut browser = browser.clone();

    match kind.as_str() {
        "elems" => on_collected(&state, &mut browser, &data),
        "href" => on_href(&state, &mut browser, &data),
        _ => return false,
    }
    true
}

/// The page has reported its hintable elements. Generate the labels, draw them, enter hint mode.
fn on_collected(state: &SharedState, browser: &mut Browser, data: &str) {
    let points: Vec<(i32, i32)> = data
        .split('|')
        .filter(|part| !part.is_empty())
        .filter_map(|part| {
            let mut fields = part.split(',');
            let x = fields.next()?.parse().ok()?;
            let y = fields.next()?.parse().ok()?;
            Some((x, y))
        })
        .collect();

    let elapsed = {
        let guard = session().lock().expect("hint session mutex poisoned");
        guard.as_ref().map(|s| s.started.elapsed())
    };
    eprintln!(
        "bru[hints]: {} elements in {:.1} ms ({} bytes of payload)",
        points.len(),
        elapsed.map(|e| e.as_secs_f64() * 1000.0).unwrap_or(0.0),
        data.len(),
    );

    if points.is_empty() {
        // qutebrowser: message.error("No elements found."), and no mode change.
        *session().lock().expect("hint session mutex poisoned") = None;
        eprintln!("bru: no hintable elements found");
        return;
    }

    let labels = hint_strings(points.len());
    let mut trie = BindingTrie::new();
    for (index, label) in labels.iter().enumerate() {
        match crate::bindings::parse_key_sequence(label) {
            Ok(sequence) => {
                trie.insert(&sequence, index);
            }
            Err(e) => eprintln!("bru: hint label {label:?} is not a key sequence: {e}"),
        }
    }

    {
        let mut guard = session().lock().expect("hint session mutex poisoned");
        let Some(open) = guard.as_mut() else {
            return;
        };
        open.points = points;
        open.labels = labels.clone();
        open.trie = trie;
        open.sequence.clear();
    }

    debug(&format!("labels {}", labels.join(" ")));

    let list = labels
        .iter()
        .map(|label| format!("\"{label}\""))
        .collect::<Vec<_>>()
        .join(",");
    show(browser, &format!("window.__bru_hints.show([{list}]);"));

    let entered = state
        .lock()
        .expect("state mutex poisoned")
        .enter_mode(Mode::Hint, false);
    if entered {
        crate::ipc::set_mode(Mode::Hint.name().to_string());
        crate::ipc::set_keystring(String::new());
    }
}

/// The page has reported the URL behind a followed hint (`F`).
fn on_href(state: &SharedState, browser: &mut Browser, url: &str) {
    *session().lock().expect("hint session mutex poisoned") = None;
    clear_labels(browser);
    leave_mode(state);

    if url.is_empty() {
        eprintln!("bru: no URL for this element");
        return;
    }
    eprintln!("bru[hints]: opening {url} in a background tab");

    // **Not `tabs::new_tab` from here.** This runs inside the message router's query handler, and
    // the router holds `browser_query_info_map` across that call. Opening a tab is
    // `window.add_child_view_at`, which creates the browser *synchronously* (CEF-NOTES, Tabs) and
    // navigates it, which reaches `RequestHandler::on_before_browse`, which bru must forward to the
    // router, which takes that same lock. Measured 2026-08-06: bru froze with its window still
    // painted, and `eu-stack` showed the whole ring —
    //   on_process_message_received → on_query_str → hints::on_page_query → tabs::new_tab
    //   → add_child_view_at → …Navigate → on_before_browse → cancel_pending_for → BrowserInfoMap
    //   → lock_contended.
    // Posting it puts the tab on the next turn of the message loop, by which time the router has
    // let go.
    let mut task = OpenBackgroundTab::new(url.to_string());
    post_task(ThreadId::UI, Some(&mut task));
}

wrap_task! {
    struct OpenBackgroundTab {
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

fn show(browser: &mut Browser, code: &str) {
    let Some(frame) = browser.main_frame() else {
        return;
    };
    frame.execute_java_script(Some(&CefString::from(code)), None, 0);
}

// ------------------------------------------------------------------------------------------------
// The key parser — HintKeyParser, over the same trie as every other mode
// ------------------------------------------------------------------------------------------------

/// One key while hint mode is current.
///
/// `None` means bru is not in hint mode and `src/keys.rs` should carry on to the ordinary parser.
/// `Some(swallow)` is what `on_pre_key_event` returns; it is always `true`, because hint mode
/// consumes everything — a key that reached the page here would be typed into whatever the last
/// click focused.
pub fn handle_key(state: &SharedState, browser: &mut Browser, info: KeyInfo) -> Option<bool> {
    if state.lock().expect("state mutex poisoned").mode() != Mode::Hint {
        return None;
    }
    debug(&format!("key {info}"));

    // `hint:` bindings, configdata.yml:3884. `<Escape>` is the one bru implements; `<Return>`
    // (hint-follow) has nothing to follow while a match follows itself, and the three `hint …`
    // rebindings are other targets, which are stage 3.
    if info.key == Key::Named(NamedKey::Escape) {
        cancel(state, browser);
        return Some(true);
    }

    // `HintKeyParser._handle_filter_key`: backspace walks the chain back rather than clearing it.
    if info.key == Key::Named(NamedKey::Backspace) {
        let mut guard = session().lock().expect("hint session mutex poisoned");
        if let Some(open) = guard.as_mut() {
            open.sequence.pop();
        }
        drop(guard);
        redraw(browser);
        return Some(true);
    }

    let outcome = {
        let mut guard = session().lock().expect("hint session mutex poisoned");
        let Some(open) = guard.as_mut() else {
            // In hint mode with no session: nothing can match, and the keys must not reach the page.
            return Some(true);
        };
        open.sequence.push(info);
        match open.trie.matches(&open.sequence) {
            Match::Exact(index) => Outcome::Follow(*index),
            Match::Partial => Outcome::Pending,
            Match::NoMatch => {
                // `BaseKeyParser.handle` clears the chain on a no-match. In letter mode there is
                // nothing else to do with the key: it names no hint.
                open.sequence.clear();
                Outcome::NoMatch
            }
        }
    };

    match outcome {
        Outcome::Follow(index) => follow(state, browser, index),
        Outcome::Pending | Outcome::NoMatch => redraw(browser),
    }
    Some(true)
}

enum Outcome {
    Follow(usize),
    Pending,
    NoMatch,
}

/// Push the current chain to the status bar and to the page's labels.
///
/// The visible set is computed here, in Rust, and sent as a list of indices. The page is told which
/// labels to show; it is never asked which ones match.
fn redraw(browser: &mut Browser) {
    let (keystring, visible, matched_len) = {
        let guard = session().lock().expect("hint session mutex poisoned");
        let Some(open) = guard.as_ref() else {
            return;
        };
        let typed = crate::bindings::sequence_to_string(&open.sequence);
        let visible: Vec<String> = open
            .labels
            .iter()
            .enumerate()
            .filter(|(_, label)| label.starts_with(&typed))
            .map(|(index, _)| index.to_string())
            .collect();
        (typed.clone(), visible, typed.chars().count())
    };

    crate::ipc::set_keystring(keystring);
    show(
        browser,
        &format!(
            "window.__bru_hints && window.__bru_hints.filter([{}],{matched_len});",
            visible.join(",")
        ),
    );
}

/// A hint matched. `f` clicks it; `F` asks the page for its URL and opens a background tab.
fn follow(state: &SharedState, browser: &mut Browser, index: usize) {
    let (target, point, token) = {
        let guard = session().lock().expect("hint session mutex poisoned");
        let Some(open) = guard.as_ref() else {
            return;
        };
        let Some(point) = open.points.get(index).copied() else {
            return;
        };
        (open.target, point, open.token.clone())
    };

    match target {
        Target::Normal => {
            *session().lock().expect("hint session mutex poisoned") = None;
            clear_labels(browser);
            leave_mode(state);
            eprintln!("bru[hints]: clicking hint {index} at ({}, {})", point.0, point.1);
            click(browser, point.0, point.1);
        }
        Target::TabBg => {
            // Ask before clearing, and clear in `on_href`. `clear()` drops the element array along
            // with the labels — it is the teardown, not a repaint — so a `clear_labels` here would
            // leave `href` looking up an index in an empty array and reporting no URL for every
            // link on the page. Measured 2026-08-06: it did exactly that.
            //
            // The session stays open until the page answers; `on_href` closes it.
            show(
                browser,
                &format!("window.__bru_hints.href(\"{token}\",{index});"),
            );
        }
    }
}

/// The follow, on Chromium's real input path.
///
/// A move first, because hover state is what a page's own handlers look at and a press with no
/// preceding move arrives at an element that was never entered. Then press and release, one click.
fn click(browser: &mut Browser, x: i32, y: i32) {
    let Some(host) = browser.host() else {
        return;
    };
    let event = MouseEvent { x, y, modifiers: 0 };
    host.send_mouse_move_event(Some(&event), 0);
    host.send_mouse_click_event(Some(&event), MouseButtonType::LEFT, 0, 1);
    host.send_mouse_click_event(Some(&event), MouseButtonType::LEFT, 1, 1);
}

// ------------------------------------------------------------------------------------------------
// Reading the page's answer
// ------------------------------------------------------------------------------------------------

/// One field out of the flat object `chrome/hints.js` sends. Every value it writes is either hex or
/// percent-encoded, so no value can contain a quote and this needs no escape handling.
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

// ------------------------------------------------------------------------------------------------
// The debug switch
// ------------------------------------------------------------------------------------------------

/// `--hint-script=f,a,s --hint-step-ms=2000` drives hint mode from posted UI tasks: `f`/`F` start a
/// session, `esc` cancels one, and every other step is fed to the key parser one character at a
/// time.
///
/// It exists for the same reason as `--tab-script`. The only key-injection tool on this machine is
/// `wtype`, which attaches a virtual keyboard, and CEF segfaults in `xkb_state_update_mask` when
/// the keymap arrives — measured 2026-08-06, 2/3 runs, with the leftover keystrokes landing in
/// whatever the compositor focuses next. So keys cannot drive an unattended check here, and this
/// drives the very functions the keys call instead. Inert unless the switch is passed.
pub fn schedule_hint_script(steps: &str, interval_ms: i64) {
    for (i, step) in steps.split(',').filter(|s| !s.is_empty()).enumerate() {
        let mut task = HintStep::new(step.to_string());
        post_delayed_task(ThreadId::UI, Some(&mut task), interval_ms * (i as i64 + 1));
    }
}

wrap_task! {
    struct HintStep {
        step: String,
    }

    impl Task {
        fn execute(&self) {
            let Some(state) = BruState::instance() else {
                return;
            };
            let browser = state.lock().expect("state mutex poisoned").active_browser();
            let Some(mut browser) = browser else {
                eprintln!("hint-script: no tab to aim at");
                return;
            };

            match self.step.as_str() {
                // Collection is a round trip through the page, so there is nothing to report here
                // yet; `on_collected` prints the count and the timing when the answer lands.
                "f" => return start(&state, &mut browser, Target::Normal),
                "F" => return start(&state, &mut browser, Target::TabBg),
                "esc" => cancel(&state, &mut browser),
                keys => {
                    for c in keys.chars() {
                        match crate::bindings::parse_key_sequence(&c.to_string()) {
                            Ok(sequence) => {
                                for info in sequence {
                                    handle_key(&state, &mut browser, info);
                                }
                            }
                            Err(e) => eprintln!("hint-script: {c:?}: {e}"),
                        }
                    }
                }
            }

            let mode = state.lock().expect("state mutex poisoned").mode();
            let guard = session().lock().expect("hint session mutex poisoned");
            eprintln!(
                "hint-script: after {:?} -> mode {mode}, {} hints, chain {:?}",
                self.step,
                guard.as_ref().map(|s| s.labels.len()).unwrap_or(0),
                guard
                    .as_ref()
                    .map(|s| crate::bindings::sequence_to_string(&s.sequence))
                    .unwrap_or_default(),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chars() -> Vec<char> {
        CHARS.chars().collect()
    }

    #[test]
    fn ceil_log_matches_qutebrowsers() {
        // utils.ceil_log's own docstring: max(1, ceil(log(number, base))).
        assert_eq!(ceil_log(1, 9), 1);
        assert_eq!(ceil_log(9, 9), 1);
        assert_eq!(ceil_log(10, 9), 2);
        assert_eq!(ceil_log(81, 9), 2);
        assert_eq!(ceil_log(82, 9), 3);
        assert_eq!(ceil_log(729, 9), 3);
        assert_eq!(ceil_log(730, 9), 4);
    }

    #[test]
    fn number_to_hint_str_counts_in_base_nine() {
        let chars = chars();
        // The first nine numbers are the alphabet itself.
        assert_eq!(number_to_hint_str(0, &chars, 0), "a");
        assert_eq!(number_to_hint_str(8, &chars, 0), "l");
        // Then it carries, and the carry digit is a real digit rather than a pad: 9 is "10" in
        // base nine, which is "sa" and not "aa".
        assert_eq!(number_to_hint_str(9, &chars, 0), "sa");
        assert_eq!(number_to_hint_str(10, &chars, 0), "ss");
        assert_eq!(number_to_hint_str(80, &chars, 0), "ll");
        assert_eq!(number_to_hint_str(81, &chars, 0), "saa");
        // Padding to `digits` uses the first character, as "0" would in base 10.
        assert_eq!(number_to_hint_str(0, &chars, 2), "aa");
        assert_eq!(number_to_hint_str(8, &chars, 3), "aal");
    }

    #[test]
    fn labels_are_unique_and_prefix_free() {
        // Prefix-freedom is what makes an exact match final: with `a` and `as` both bound, `a`
        // could never follow without waiting for a second key. `_hint_scattered` guarantees it by
        // starting the long labels at short_count * len(chars), and this is the property the whole
        // follow-on-exact-match path rests on.
        for count in [1usize, 2, 8, 9, 10, 25, 80, 81, 82, 200, 729, 730, 1000] {
            let labels = hint_strings(count);
            assert_eq!(labels.len(), count, "{count} elements must get {count} labels");

            let mut sorted = labels.clone();
            sorted.sort();
            let unique = {
                let mut u = sorted.clone();
                u.dedup();
                u
            };
            assert_eq!(sorted, unique, "duplicate label with {count} elements");

            for (i, a) in sorted.iter().enumerate() {
                if let Some(b) = sorted.get(i + 1) {
                    assert!(
                        !b.starts_with(a.as_str()),
                        "{a:?} is a prefix of {b:?} with {count} elements"
                    );
                }
            }

            for label in &labels {
                assert!(
                    label.chars().all(|c| CHARS.contains(c)),
                    "{label:?} is not spelled out of hints.chars"
                );
            }
        }
    }

    #[test]
    fn short_labels_are_used_before_long_ones() {
        // 10 elements do not need two characters each: nine of them fit in one, and Vimium's trick
        // — which qutebrowser copies — is to hand those out first.
        let labels = hint_strings(10);
        let short = labels.iter().filter(|l| l.chars().count() == 1).count();
        assert_eq!(short, 8, "10 elements: 8 one-character labels and 2 two-character ones");
        assert_eq!(labels.iter().filter(|l| l.chars().count() == 2).count(), 2);

        // Nine or fewer never needs more than one character at all.
        for count in 1..=9 {
            assert!(
                hint_strings(count).iter().all(|l| l.chars().count() == 1),
                "{count} elements should all get single-character labels"
            );
        }
    }

    #[test]
    fn hints_are_scattered_rather_than_run_in_order() {
        // The whole output for 27 elements, taken from running qutebrowser's own `_hint_strings`
        // (hints.py:424) against the same input — chars=asdfghjkl, min_chars=1, scatter=true. A
        // reference value rather than a property, because every step of the algorithm is visible in
        // it: six short labels, twenty-one long ones, and `_shuffle_hints` interleaving them.
        assert_eq!(
            hint_strings(27),
            vec![
                "a", "jf", "kf", "s", "jg", "kg", "d", "jh", "kh", "f", "jj", "kj", "g", "jk",
                "kk", "h", "jl", "kl", "ja", "ka", "la", "js", "ks", "ls", "jd", "kd", "ld",
            ]
        );
        assert_eq!(hint_strings(9), vec!["a", "s", "d", "f", "g", "h", "j", "k", "l"]);
        assert_eq!(
            hint_strings(10),
            vec!["a", "ls", "s", "d", "f", "g", "h", "j", "k", "la"]
        );

        // What the shuffle is for: neighbouring links must not all begin with the same letter, or
        // a page of 90 links has `a?` on the first nine and nothing else anywhere near them.
        let labels = hint_strings(90);
        let runs = labels
            .windows(2)
            .filter(|pair| pair[0].chars().next() == pair[1].chars().next())
            .count();
        assert!(runs < 10, "{runs} adjacent pairs share a first character out of 89");
    }

    #[test]
    fn the_trie_answers_hint_labels_the_way_it_answers_bindings() {
        // The point of BindingTrie being generic: hint mode is another value type, not another
        // structure. modeparsers.py:135, HintKeyParser.update_bindings.
        let labels = hint_strings(20);
        let mut trie: BindingTrie<usize> = BindingTrie::new();
        for (index, label) in labels.iter().enumerate() {
            trie.insert(&crate::bindings::parse_key_sequence(label).unwrap(), index);
        }
        assert_eq!(trie.len(), 20);

        for (index, label) in labels.iter().enumerate() {
            let sequence = crate::bindings::parse_key_sequence(label).unwrap();
            match trie.matches(&sequence) {
                Match::Exact(found) => assert_eq!(*found, index),
                other => panic!("{label:?} should match exactly, got {:?}", other.match_type()),
            }
            // Every prefix short of the whole label is partial, never exact.
            for cut in 1..sequence.len() {
                assert_eq!(
                    trie.matches(&sequence[..cut]).match_type(),
                    crate::bindings::MatchType::PartialMatch,
                    "{label:?} truncated to {cut} keys"
                );
            }
        }

        // A character outside hints.chars names nothing.
        let z = crate::bindings::parse_key_sequence("z").unwrap();
        assert_eq!(trie.matches(&z).match_type(), crate::bindings::MatchType::NoMatch);
    }

    #[test]
    fn the_page_payload_is_read_back_exactly() {
        let request = "{\"type\":\"hints\",\"token\":\"deadbeef\",\"kind\":\"elems\",\
                       \"data\":\"12%2C34%7C56%2C78\"}";
        assert_eq!(field(request, "token").as_deref(), Some("deadbeef"));
        assert_eq!(field(request, "kind").as_deref(), Some("elems"));
        assert_eq!(
            percent_decode(&field(request, "data").unwrap()),
            "12,34|56,78"
        );
        assert_eq!(field(request, "missing"), None);

        // A URL, which is what `F` gets back. encodeURIComponent escapes the separators bru would
        // otherwise have to unpick, and leaves the rest alone.
        assert_eq!(
            percent_decode("https%3A%2F%2Fwww.vesti.bg%2Fa%3Fb%3D1%26c%3D%C3%A4"),
            "https://www.vesti.bg/a?b=1&c=ä"
        );
    }

    #[test]
    fn a_token_is_not_guessable_and_not_reused() {
        let a = mint_token();
        let b = mint_token();
        assert_eq!(a.len(), 32);
        assert_ne!(a, b, "two sessions must not share a token");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
