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
use std::path::{Path, PathBuf};
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
    // Every search bru runs comes through here — `/`, `?` and a typed `:search` alike, because
    // `Cmdline::accept` turns the first two into the third — so this is the one place a term has to
    // be remembered from. `n` and `N` go through `step` and add nothing: they are the same search.
    remember(text);
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

/// Forget the search because the page under it has been replaced — the load handler's call, and the
/// only one with a browser to hand.
///
/// **Chromium does not drop its find session on a navigation, whatever this file used to say.**
/// Measured 2026-08-06 on a `file://` page: with `search Kestrel` displayed, `:open` to a second
/// page fired `on_find_result` three more times as the *new* document was scanned — `count=1`, then
/// `count=2` — and the bar read `Match [1/2]` for a search the user never ran on that page, with
/// those matches highlighted. Forgetting bru's own copy is therefore only half of it; the session
/// has to be ended as well.
///
/// `stop_finding(0)` — the selection is left alone. On a document that has just started loading
/// there is nothing of ours selected to clear, and clearing is `<Escape>`'s job, not a load's.
pub fn forget_for(browser: &mut Browser) {
    forget();
    if let Some(host) = browser.host() {
        host.stop_finding(0);
    }
}

/// Forget the search because the page under it is gone — a tab switch, where the browser that owns
/// the find session is the one being left and keeps it.
pub fn forget() {
    if let Ok(mut cell) = cell().lock() {
        *cell = None;
    }
    report(String::new());
}

// -----------------------------------------------------------------------------------------------
// The search history
// -----------------------------------------------------------------------------------------------
//
// **This is a departure from qutebrowser, asked for by the user on 2026-08-07.** qutebrowser has no
// search history and no search completion: `completer.py:213-219` sets no model for a prefix that
// is not `:`, over the comment "FIXME complete searches" and its issue #32. So there was nothing to
// port and nothing to copy — what is below is bru's, and it is shaped after the one thing bru
// already writes that is nearest to it, `cmdline.rs`'s `cmd-history`: a plain text file in bru's own
// data directory, one entry per line, oldest first, bounded, written through a temp file and a
// rename, read once and lazily.
//
// Three decisions, each of which could have gone the other way:
//
// 1. **`/` and `?` share one history.** They are two directions through one search, not two
//    searches: `n` already treats direction as a property of the walk and not of the term
//    (`step`, above), and `?foo` then `n` goes *up* through the same matches `/foo` would have gone
//    down through. Vim is the precedent that settles it — `/` and `?` share one history there, and
//    `bru` is a browser for a user whose fingers are vim's.
// 2. **A repeat moves rather than duplicates.** `cmdline.rs`'s rule is qutebrowser's `History.append`
//    — a repeat *of the newest entry* is not a new entry — which leaves `foo bar foo` holding two
//    `foo`s. In a list whose whole purpose is to be offered back, that is one row of the bar spent
//    saying something the row above it already said. Vim again: `:h history` — "If the same string
//    is entered twice the older one is removed".
// 3. **A `--private` run reads it and does not write it.** The same line `cmd-history` draws, for
//    the same reason: a list of what was searched for names the page as plainly as the visit log
//    would. The terms typed in the private run are still offered back *within* it, because they are
//    in memory; they do not outlive it.

/// How many terms the file keeps.
///
/// The same 100 `cmdline.rs::HISTORY_MAX` keeps, and the same argument: the file is read in full at
/// startup, so it has to be bounded, and 100 is four screens of a bar that shows fifteen rows.
///
/// **It is a `const` and not a setting, deliberately.** The obvious argument for a setting is that
/// qutebrowser has `completion.cmd_history_max_items`; the argument against is that bru already has
/// the identical cap on the identical kind of file one module away and it is a `const` there. A
/// setting for the newer of two caps and a constant for the older would be the inconsistency, not
/// the omission — and `settings.rs`'s own rule is that a name in the table moves something the user
/// asked to move. If it is ever wanted it is one setting covering *both* files, which is a change to
/// `cmdline.rs` and `settings.rs` and belongs to whoever owns those.
const HISTORY_MAX: usize = 100;

/// Every term searched for, oldest last — the order `cmd-history` is written in.
static HISTORY: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// `$XDG_DATA_HOME/bru/search-history`, or `~/.local/share/bru/search-history`.
///
/// Through `data::data_dir` rather than a second copy of the XDG rules, exactly as
/// `cmdline::history_path` does — so a scratch `XDG_DATA_HOME` moves this with everything else bru
/// owns, and a test can point it somewhere harmless.
fn history_path() -> Option<PathBuf> {
    Some(crate::data::data_dir()?.join("search-history"))
}

/// Read the file into memory, once, the first time anything wants it.
///
/// Lazily rather than from `app.rs` for `ensure_history_loaded`'s reason: no other workstream's file
/// has to be edited to install it, and a data directory that cannot be read costs the completion its
/// older terms and nothing else.
fn ensure_loaded() {
    static LOADED: std::sync::Once = std::sync::Once::new();
    LOADED.call_once(|| {
        let Some(path) = history_path() else { return };
        let entries = read_history(&path);
        if entries.is_empty() {
            return;
        }
        if let Ok(mut history) = HISTORY.lock() {
            *history = entries;
        }
    });
}

/// Remember a term and write the file.
///
/// Written on every search rather than on `:save`, which is where `cmd-history` is written from.
/// The two are not the same shape: `cmd-history` is a hundred command lines flushed once, and this
/// is at most a hundred short terms — rewriting the whole file per search is what `data.rs` does for
/// `quickmarks` on every `:quickmark-save`, and it is the reason a term is still there after a
/// browser that was killed rather than quit.
fn remember(text: &str) {
    // A term with a newline in it would come back as two terms; the `<input>` cannot make one, but
    // the file is read again on the next start and this is the cheap end to guard.
    if text.is_empty() || text.contains('\n') {
        return;
    }
    ensure_loaded();
    let entries = {
        let Ok(mut history) = HISTORY.lock() else {
            return;
        };
        push(&mut history, text);
        history.clone()
    };
    // Outside the lock, and refused by a private run.
    if crate::profile::is_private() {
        return;
    }
    let Some(path) = history_path() else { return };
    if let Err(error) = write_history(&path, &entries) {
        // Once, where it happened, and never on the search path again: the term is in memory and
        // the search itself has not failed.
        eprintln!("bru: search history not saved to {}: {error}", path.display());
    }
}

/// Decision 2, as a function so it can be tested without a disk: the term goes on the end, and any
/// older copy of it comes out.
fn push(history: &mut Vec<String>, text: &str) {
    history.retain(|held| held != text);
    history.push(text.to_string());
    let over = history.len().saturating_sub(HISTORY_MAX);
    history.drain(..over);
}

/// What has been searched for, **newest first** — which is the order the completion offers them in,
/// the same way `:open`'s History category is `last_atime DESC`.
pub fn history() -> Vec<String> {
    ensure_loaded();
    let Ok(history) = HISTORY.lock() else {
        return Vec::new();
    };
    history.iter().rev().cloned().collect()
}

fn read_history(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// The bytes, the cap and the atomic rename. Takes a path so it is testable without an
/// `XDG_DATA_HOME` the whole process shares.
fn write_history(path: &Path, entries: &[String]) -> std::io::Result<usize> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let kept: Vec<&String> = entries
        .iter()
        .filter(|entry| !entry.is_empty() && !entry.contains('\n'))
        .rev()
        .take(HISTORY_MAX)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();

    let mut body = String::new();
    for entry in &kept {
        body.push_str(entry);
        body.push('\n');
    }

    // Temp file and rename, so an interrupted write leaves the previous history rather than half of
    // this one — `data::write_atomically`'s shape, and `cmdline::write_history`'s.
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)?;
    Ok(kept.len())
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
            selection_rect: Option<&Rect>,
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
            trace(count, active_match_ordinal, _final_update, selection_rect);
            report(match_text(count, active_match_ordinal));
        }
    }
}

/// Set `BRU_DEBUG_FIND=1` to see every update Chromium sends, the way `BRU_DEBUG_IPC` traces the
/// router. It is what showed that a fresh `find` counts before it selects, and the next surprise in
/// find-in-page should cost one environment variable rather than a rebuild.
///
/// The selection rectangle is in it because it is the one thing here that says *where* the active
/// match is, in the page's own coordinates and from Chromium rather than from bru. On a machine
/// where twelve agents share one compositor and a screenshot may catch someone else's window, that
/// is the measurement that shows `n` moved to a different match rather than re-finding the same one.
fn trace(count: i32, active: i32, final_update: i32, rect: Option<&Rect>) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("BRU_DEBUG_FIND").is_some()) {
        let rect = rect
            .map(|rect| format!("{},{} {}x{}", rect.x, rect.y, rect.width, rect.height))
            .unwrap_or_else(|| "none".to_string());
        eprintln!("bru[find]: count={count} active={active} final={final_update} rect=[{rect}]");
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

    // ---- the search history ----

    /// A scratch `~/.local/share/bru`-shaped directory, and never the user's real one. `TempData`
    /// in `src/data.rs` is the same shape.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> TempDir {
            static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("bru-find-test-{}-{n}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            TempDir(dir)
        }
        fn path(&self) -> PathBuf {
            self.0.join("search-history")
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_repeated_search_moves_to_the_front_instead_of_being_written_twice() {
        // Decision 2, and it is *not* `cmdline.rs`'s rule: that one only refuses a repeat of the
        // newest entry, which would leave two `foo`s here. `:h history` — "If the same string is
        // entered twice the older one is removed".
        let mut history = Vec::new();
        for term in ["foo", "bar", "foo"] {
            push(&mut history, term);
        }
        assert_eq!(history, ["bar", "foo"]);
    }

    #[test]
    fn the_history_is_bounded_and_drops_the_oldest() {
        let mut history = Vec::new();
        for i in 0..HISTORY_MAX + 10 {
            push(&mut history, &format!("term {i}"));
        }
        assert_eq!(history.len(), HISTORY_MAX);
        assert_eq!(history[0], format!("term {}", 10));
        assert_eq!(history[HISTORY_MAX - 1], format!("term {}", HISTORY_MAX + 9));
    }

    #[test]
    fn the_file_survives_a_round_trip_and_is_written_atomically() {
        let dir = TempDir::new("round-trip");
        let path = dir.path();
        // The directory does not exist yet, which is what a first run looks like.
        assert!(!dir.0.exists());
        let entries: Vec<String> = ["Kestrel", "rust vec", "страница"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(write_history(&path, &entries).unwrap(), 3);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "Kestrel\nrust vec\nстраница\n");
        assert_eq!(read_history(&path), entries);
        // Nothing is left behind by the rename.
        assert!(!path.with_extension("tmp").exists());
        // A file that is not there reads as an empty history rather than as an error.
        assert!(read_history(&dir.0.join("never-written")).is_empty());
    }

    #[test]
    fn only_the_last_hundred_reach_the_file() {
        let dir = TempDir::new("bounded");
        let entries: Vec<String> = (0..HISTORY_MAX + 5).map(|i| format!("t{i}")).collect();
        assert_eq!(write_history(&dir.path(), &entries).unwrap(), HISTORY_MAX);
        let back = read_history(&dir.path());
        assert_eq!(back.len(), HISTORY_MAX);
        // The newest are the ones kept, and their order is unchanged.
        assert_eq!(back[0], "t5");
        assert_eq!(back[HISTORY_MAX - 1], format!("t{}", HISTORY_MAX + 4));
    }

    #[test]
    fn a_term_with_a_newline_in_it_never_reaches_the_file() {
        // It cannot come from the `<input>`, but the file is read again on the next start and two
        // lines there would be two terms.
        let dir = TempDir::new("newline");
        let entries = vec!["ok".to_string(), "two\nlines".to_string()];
        assert_eq!(write_history(&dir.path(), &entries).unwrap(), 1);
        assert_eq!(read_history(&dir.path()), ["ok"]);
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
