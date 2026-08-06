//! In-page search: `/`, `?`, `n`, `N`.
//!
//! `host.find` (bindings 12646) and `host.stop_finding` (12654) are the whole engine — Chromium's
//! own find-in-page, the same one Brave draws a bar for, so highlighting, wrapping and the match
//! count come for free. `wrap_find_handler!` (bindings 19403) is how the count comes back, and it
//! is what fills `Match [3/17]` in the status bar.
//!
//! **The search text arrives from the command line as a `&str`.** `/` and `?` are bound to
//! `cmd-set-text /` and `cmd-set-text ?`, so what the user types is the command line's business
//! (M9's workstream); this module is handed the text and never reads a key.
//!
//! What is remembered here is what qutebrowser remembers, and for the same reason: `n` continues a
//! search in the direction it was started in, so `?foo` then `n` goes *up*
//! (`browser/commands.py`:1650, `webenginetab.py`'s `_flags`). Chromium needs the text again on
//! every call, and it needs to be told whether this is a fresh search or a continuation — passing
//! `find_next = 0` twice restarts from the top and the page stops advancing.

use cef::*;
use std::sync::Mutex;

/// The search the page is currently showing, or `None` when nothing is displayed.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Search {
    text: String,
    /// True for `?`, which searches backwards.
    reverse: bool,
}

fn cell() -> &'static Mutex<Option<Search>> {
    static SEARCH: Mutex<Option<Search>> = Mutex::new(None);
    &SEARCH
}

/// The counter as the bar last showed it, kept so something other than the chrome can read it —
/// the M11 script does, and `:search` will want it for "Text not found on page!".
fn counter() -> &'static Mutex<String> {
    static COUNTER: Mutex<String> = Mutex::new(String::new());
    &COUNTER
}

/// `Match [3/17]`, or empty when no search is showing.
pub fn matches() -> String {
    counter().lock().map(|counter| counter.clone()).unwrap_or_default()
}

/// One place sets both the cached counter and the bar, so they cannot disagree.
fn report(text: String) {
    if let Ok(mut counter) = counter().lock() {
        counter.clone_from(&text);
    }
    crate::ipc::set_search_match(text);
}

/// A ceiling on `<count>n`, the same one the movements use.
const MAX_COUNT: u32 = 1000;

/// `search [text]` — `/text` and, with `reverse`, `?text`.
///
/// Empty text clears, which is what `<Escape>`'s `clear-keychain ;; search ;; fullscreen --leave`
/// binding relies on (`config.rs`, qutebrowser's `configdata.yml`).
pub fn search(browser: &mut Browser, text: &str, reverse: bool) {
    if text.is_empty() {
        clear(browser);
        return;
    }

    if let Ok(mut cell) = cell().lock() {
        *cell = Some(Search { text: text.to_string(), reverse });
    }
    report(String::new());

    // Two calls, and the second is not a mistake. Measured 2026-08-06 with `BRU_DEBUG_FIND=1`:
    // `find` with `find_next = 0` starts the session and *counts* — it reported `count=5
    // active=0` and left the page where it was. It is the first `find_next = 1` that selects a
    // match and scrolls to it. So `/foo` on its own would highlight five matches and go to none of
    // them, which is not what typing into Chrome's find bar or qutebrowser's does. `find_next = 0`
    // still has to happen exactly once per text, or the search restarts from the top on every `n`.
    find(browser, text, !reverse, false);
    find(browser, text, !reverse, true);
    // A search scrolls the page to its match, so the percentage in the bar is now wrong. Nothing in
    // CEF says the page moved; asking is the only way to find out.
    crate::scroll::request_position(browser);
}

/// `search-next` — `n`. Continues in the direction the search was started in.
pub fn search_next(browser: &mut Browser, count: Option<u32>) {
    step(browser, false, count);
}

/// `search-prev` — `N`. The opposite direction to the one the search was started in.
pub fn search_prev(browser: &mut Browser, count: Option<u32>) {
    step(browser, true, count);
}

fn step(browser: &mut Browser, flip: bool, count: Option<u32>) {
    let Some(search) = cell().lock().ok().and_then(|cell| cell.clone()) else {
        // qutebrowser raises "No search done yet." here. bru has no message line yet, so this is
        // silent — and deliberately not a crash on a key that is bound by default.
        return;
    };
    // `?foo` then `n` goes up; `?foo` then `N` goes down.
    let forward = search.reverse == flip;

    let repeat = count.unwrap_or(1).clamp(1, MAX_COUNT);
    for _ in 0..repeat {
        find(browser, &search.text, forward, true);
    }
    crate::scroll::request_position(browser);
}

/// `search` with no text, and `<Escape>`.
pub fn clear(browser: &mut Browser) {
    if let Ok(mut cell) = cell().lock() {
        *cell = None;
    }
    report(String::new());
    if let Some(host) = browser.host() {
        // Clearing the selection as well as the highlight: `<Escape>` in qutebrowser leaves no
        // trace of the search on the page, and a left-behind selection would be the next thing
        // `y` yanked.
        host.stop_finding(1);
    }
    crate::scroll::request_position(browser);
}

/// Forget the search because the page under it is gone — a load, or a tab switch. Chromium drops
/// its own find session on navigation, so keeping ours would make `n` report matches that are not
/// there.
pub fn forget() {
    if let Ok(mut cell) = cell().lock() {
        *cell = None;
    }
    report(String::new());
}

/// qutebrowser's `search.ignore_case = smart` (configdata.yml:53): case-insensitive until the
/// pattern contains a capital, and then the capital is taken to mean it.
fn match_case(text: &str) -> bool {
    text.chars().any(char::is_uppercase)
}

fn find(browser: &mut Browser, text: &str, forward: bool, find_next: bool) {
    let Some(host) = browser.host() else {
        return;
    };
    host.find(
        Some(&CefString::from(text)),
        forward as ::std::os::raw::c_int,
        match_case(text) as ::std::os::raw::c_int,
        find_next as ::std::os::raw::c_int,
    );
}

// -----------------------------------------------------------------------------------------------
// The match count in the bar
// -----------------------------------------------------------------------------------------------

// `Match [3/17]`, spelled the way qutebrowser's searchmatch widget spells it
// (`mainwindow/statusbar/searchmatch.py`:31), and cleared when there is nothing to show.
//
// Chromium sends this handler several updates per search as it works through the document, with
// `final_update` set on the last. Every one of them is worth showing: the count climbs while a long
// page is scanned, which is the same thing Chrome's own find bar does.
//
// (The `wrap_` macros take no doc comment on the struct they declare — CEF-NOTES.md trap 8.)
wrap_find_handler! {
    pub struct BruFindHandler;

    impl FindHandler {
        fn on_find_result(
            &self,
            browser: Option<&mut Browser>,
            _identifier: ::std::os::raw::c_int,
            count: ::std::os::raw::c_int,
            _selection_rect: Option<&Rect>,
            active_match_ordinal: ::std::os::raw::c_int,
            _final_update: ::std::os::raw::c_int,
        ) {
            // Only the tab on screen may write the status bar; a background tab's find would
            // otherwise overwrite it.
            let is_active = match (browser, crate::state::BruState::instance()) {
                (Some(browser), Some(state)) => {
                    let id = browser.identifier();
                    state
                        .lock()
                        .map(|state| state.is_active_browser(id))
                        .unwrap_or(false)
                }
                _ => false,
            };
            if !is_active {
                return;
            }
            trace(count, active_match_ordinal, _final_update);
            report(match_text(count, active_match_ordinal));
        }
    }
}

/// Set `BRU_DEBUG_FIND=1` to see every update Chromium sends, the way `BRU_DEBUG_IPC` traces the
/// router. It is what showed that a fresh `find` counts before it selects, and the next surprise in
/// find-in-page should cost one environment variable rather than a rebuild.
fn trace(count: i32, active: i32, final_update: i32) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("BRU_DEBUG_FIND").is_some()) {
        eprintln!("bru[find]: count={count} active={active} final={final_update}");
    }
}

fn match_text(count: i32, active: i32) -> String {
    if count <= 0 {
        // qutebrowser's SearchMatch(0, 0).is_null() — the widget shows nothing rather than
        // "Match [0/0]". A search that found nothing is a message, not a counter.
        return String::new();
    }
    // `active` is 0 while Chromium has counted the matches but selected none of them — which is the
    // state a bare `find_next = 0` leaves the page in. Showing it as 0 rather than rounding it up
    // to 1 is what qutebrowser's `SearchMatch.__str__` does, and it is also true.
    format!("Match [{}/{}]", active.max(0), count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smart_case_is_qutebrowsers_smart_case() {
        assert!(!match_case("foo"));
        assert!(!match_case("foo bar 42"));
        assert!(match_case("Foo"));
        assert!(match_case("fooBar"));
        // Non-ASCII counts, the way Python's str.islower() does for qutebrowser.
        assert!(match_case("Über"));
        assert!(!match_case("über"));
    }

    #[test]
    fn the_counter_reads_like_qutebrowsers() {
        assert_eq!(match_text(17, 3), "Match [3/17]");
        assert_eq!(match_text(1, 1), "Match [1/1]");
        // Nothing found shows nothing at all, not a zero.
        assert_eq!(match_text(0, 0), "");
        // Chromium counts before it selects: a fresh `find_next = 0` reports the total with no
        // active match, and the counter says so rather than pretending it is on the first.
        assert_eq!(match_text(5, 0), "Match [0/5]");
    }

    #[test]
    fn n_keeps_the_direction_the_search_was_started_in() {
        // The rule `step` applies, stated as data so it is checkable without a browser:
        // forward == (reverse == flip).
        let direction = |reverse: bool, flip: bool| reverse == flip;
        // `/foo` then n → forward, N → backward.
        assert!(direction(false, false));
        assert!(!direction(false, true));
        // `?foo` then n → backward, N → forward.
        assert!(!direction(true, false));
        assert!(direction(true, true));
    }
}
