//! Sessions — the tab list, written down and read back.
//!
//! `:session-save`, `:session-load`, `:session-delete`, `:session-list`, and `--restore=<name>` at
//! startup, which is the switch qutebrowser spells the same way. Files live in
//! `~/.local/share/bru/sessions/<name>.bru`, beside history and the marks: they are bru's own data,
//! rewritten by bru and never hand-edited, so they belong under the data directory and not in
//! `~/.config/bru/`, which is configer's (DESIGN.md; STAGE3-CONTRACTS.md, "Data and config").
//!
//! The directory is asked for through one call into `src/data.rs` — `data::data_dir()` — and
//! nothing else here touches that module.
//!
//! ## What a session can and cannot hold
//!
//! **CEF exposes no way to serialise a navigation list.** `BrowserHost::navigation_entries`
//! (bindings 12682) *reads* one, and this module reads it, so the URLs behind a tab's `H` and `L`
//! can be written down. There is no matching call to hand a list back — no `LoadHistory`, no
//! `CefNavigationEntry` constructor — so the only way to put history back into a tab is to navigate
//! it there again, once per entry.
//!
//! Measured 2026-08-06, on three local pages, with `--session-script`:
//!
//! - reading works and is exact: a tab that had walked A → B → C reported all three entries and
//!   `current=2`, and the file held them in order;
//! - replaying works: a restored tab whose file said `current=2` came back with three entries and
//!   sat on the third, so `H` reached B and `L` came back to C;
//! - it costs a real load per entry. Each one has to be fetched again, and this module polls the
//!   navigation list rather than guessing at a delay, so a five-entry tab is five sequential page
//!   loads before it is the tab it was.
//!
//! That cost is why replay is not what a bare restore does. `session_load` and `--restore` put back
//! the entry the tab was *on* — one load per tab, all of them in parallel — and `--restore-history`
//! (or `:session-load --history`) asks for the full walk. The file holds every URL either way, so
//! the choice is made when it is read and never when it is written.
//!
//! ## The format
//!
//! Line-oriented, because bru has no serialisation crate and a session is a list of URLs:
//!
//! ```text
//! bru-session 1
//! tab active=1 current=2 pinned=0 muted=0
//! entry https://a.example/
//! entry https://b.example/
//! entry https://c.example/
//! ```
//!
//! A URL cannot contain a newline, which is the only character the format would care about. Unknown
//! keys and unknown line types are skipped rather than rejected, so a session written by a later bru
//! still opens in this one.

use cef::*;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::tabs::SharedState;

/// The file extension. Not `.yml`: this is not qutebrowser's format and calling it one would invite
/// someone to feed it one.
const EXTENSION: &str = "bru";

/// The name `:session-save` writes when none is given — qutebrowser's `session.default_name`
/// fallback (`sessions.py:_get_session_name`).
pub const DEFAULT_NAME: &str = "default";

/// How long a single entry of a replayed history is given to load before the replay gives up on it
/// and moves to the next. A dead link in an old session must not stall the tab for ever.
const REPLAY_TIMEOUT_MS: i64 = 8000;

/// How often the replay looks at the navigation list to see whether the entry it asked for arrived.
const REPLAY_POLL_MS: i64 = 50;

// -------------------------------------------------------------------------------------------------
// The data
// -------------------------------------------------------------------------------------------------

/// One tab in a saved session.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionTab {
    /// Every URL in the tab's navigation list, oldest first. Never empty in a file this module
    /// wrote; a tab with nothing to say is not saved.
    pub history: Vec<String>,
    /// Which entry the tab was on, as an index into `history`.
    pub current: usize,
    /// Whether this is the tab that was showing.
    pub active: bool,
    pub pinned: bool,
    pub muted: bool,
}

impl SessionTab {
    /// The URL the tab was on — what a restore without `--history` opens.
    pub fn current_url(&self) -> &str {
        self.history
            .get(self.current)
            .or_else(|| self.history.last())
            .map(String::as_str)
            .unwrap_or("")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Session {
    pub tabs: Vec<SessionTab>,
}

impl Session {
    /// Which tab was showing. Falls back to the first, so a hand-edited file with no `active=1`
    /// still restores something sensible.
    pub fn active_index(&self) -> usize {
        self.tabs.iter().position(|tab| tab.active).unwrap_or(0)
    }

    pub fn to_text(&self) -> String {
        let mut out = String::from("bru-session 1\n");
        for tab in &self.tabs {
            out.push_str(&format!(
                "tab active={} current={} pinned={} muted={}\n",
                u8::from(tab.active),
                tab.current,
                u8::from(tab.pinned),
                u8::from(tab.muted),
            ));
            for url in &tab.history {
                out.push_str("entry ");
                out.push_str(url);
                out.push('\n');
            }
        }
        out
    }

    /// Read a session back. Deliberately total: anything unrecognised is skipped, because a session
    /// file that fails to parse is a browser that will not start.
    pub fn parse(text: &str) -> Session {
        let mut session = Session::default();
        for line in text.lines() {
            let line = line.trim_end_matches('\r');
            if let Some(rest) = line.strip_prefix("tab") {
                let mut tab = SessionTab::default();
                for field in rest.split_whitespace() {
                    let Some((key, value)) = field.split_once('=') else {
                        continue;
                    };
                    match key {
                        "active" => tab.active = value == "1" || value == "true",
                        "pinned" => tab.pinned = value == "1" || value == "true",
                        "muted" => tab.muted = value == "1" || value == "true",
                        "current" => tab.current = value.parse().unwrap_or(0),
                        _ => {}
                    }
                }
                session.tabs.push(tab);
            } else if let Some(url) = line.strip_prefix("entry ") {
                // An `entry` before any `tab` line is a broken file, not a reason to lose the rest.
                if let Some(tab) = session.tabs.last_mut() {
                    tab.history.push(url.to_string());
                }
            }
        }
        // A `current` past the end would restore a blank tab and hide the reason.
        for tab in &mut session.tabs {
            if tab.current >= tab.history.len() {
                tab.current = tab.history.len().saturating_sub(1);
            }
        }
        session.tabs.retain(|tab| !tab.history.is_empty());
        session
    }
}

// -------------------------------------------------------------------------------------------------
// Where the files are
// -------------------------------------------------------------------------------------------------

/// What this module needs from `src/data.rs`, and the whole of it: the one directory bru owns.
///
/// It is a function rather than a `use` so the dependency is a single line to find, and so a test
/// can put sessions somewhere else by setting `XDG_DATA_HOME` — which is what `data_dir` reads.
fn session_dir() -> Option<PathBuf> {
    Some(crate::data::data_dir()?.join("sessions"))
}

/// `~/.local/share/bru/sessions/<name>.bru`, or `None` when there is no data directory at all.
///
/// A name with a path separator in it is refused: `:session-save ../../config.lua` must not be able
/// to name a file outside this directory.
pub fn path_for(name: &str) -> Option<PathBuf> {
    if name.is_empty() || name.contains('/') || name.contains('\\') || name.starts_with('.') {
        return None;
    }
    Some(session_dir()?.join(format!("{name}.{EXTENSION}")))
}

/// Every saved session, sorted, without their extension.
pub fn list() -> Vec<String> {
    let Some(dir) = session_dir() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension()?.to_str()? != EXTENSION {
                return None;
            }
            Some(path.file_stem()?.to_str()?.to_string())
        })
        .collect();
    names.sort();
    names
}

/// `:session-delete <name>`.
pub fn delete(name: &str) -> Result<PathBuf, String> {
    let path = path_for(name).ok_or_else(|| format!("{name:?} is not a usable session name"))?;
    std::fs::remove_file(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}

// -------------------------------------------------------------------------------------------------
// Saving
// -------------------------------------------------------------------------------------------------

/// Read the whole open window into a [`Session`].
///
/// Every CEF call here — `browser.host()`, `host.navigation_entries` — happens with the state mutex
/// let go, because `navigation_entries` visits synchronously and bru's own handlers take that lock.
pub fn snapshot(state: &SharedState) -> Session {
    let (ids, active, count) = {
        let guard = state.lock().expect("state mutex poisoned");
        (guard.tab_browser_ids(), guard.active_tab(), guard.tab_count())
    };

    let mut session = Session::default();
    for index in 0..count {
        let (pinned, muted, fallback_url, browser) = {
            let mut guard = state.lock().expect("state mutex poisoned");
            let browser = ids
                .get(index)
                .copied()
                .flatten()
                .and_then(|id| guard.browser_with_id(id));
            (
                guard.tab_pinned(index),
                guard.tab_muted(index),
                guard.tab_url(index).unwrap_or_default(),
                browser,
            )
        };

        let (mut history, mut current) = match browser {
            Some(mut browser) => read_history(&mut browser),
            None => (Vec::new(), 0),
        };
        // A tab whose browser has not answered yet still has an address the display handler
        // reported, and one entry is better than losing the tab.
        if history.is_empty() {
            if fallback_url.is_empty() {
                continue;
            }
            history = vec![fallback_url];
            current = 0;
        }

        session.tabs.push(SessionTab {
            history,
            current,
            active: index == active,
            pinned,
            muted,
        });
    }
    session
}

/// `:session-save [name]`. Answers the path it wrote, for the message the caller prints.
pub fn save(state: &SharedState, name: &str) -> Result<PathBuf, String> {
    let session = snapshot(state);
    if session.tabs.is_empty() {
        return Err("nothing to save: there are no tabs".to_string());
    }
    write(name, &session)
}

fn write(name: &str, session: &Session) -> Result<PathBuf, String> {
    let path = path_for(name).ok_or_else(|| format!("{name:?} is not a usable session name"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    std::fs::write(&path, session.to_text()).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}

/// A tab's navigation list and the index it is on, straight out of CEF.
///
/// The visitor runs synchronously on the calling thread — `exec.rs::entry_at_offset` relies on the
/// same thing — so the vectors are full by the time `navigation_entries` returns.
fn read_history(browser: &mut Browser) -> (Vec<String>, usize) {
    let Some(host) = browser.host() else {
        return (Vec::new(), 0);
    };
    let entries: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let current: Arc<Mutex<Option<i32>>> = Arc::new(Mutex::new(None));

    let mut visitor = SessionHistoryVisitor::new(entries.clone(), current.clone());
    host.navigation_entries(Some(&mut visitor), 0);

    let entries = entries.lock().expect("history mutex poisoned").clone();
    let current = current
        .lock()
        .expect("history mutex poisoned")
        .unwrap_or(0)
        .max(0) as usize;
    (entries, current)
}

// The same shape as `exec.rs`'s visitor and deliberately not shared with it: that one belongs to
// `H`/`L` and answers one question about one offset, this one takes the whole list. A visitor
// struct is six lines; a shared one across two workstreams' files is a seam.
wrap_navigation_entry_visitor! {
    struct SessionHistoryVisitor {
        entries: Arc<Mutex<Vec<String>>>,
        current: Arc<Mutex<Option<i32>>>,
    }

    impl NavigationEntryVisitor {
        fn visit(
            &self,
            entry: Option<&mut NavigationEntry>,
            current: ::std::os::raw::c_int,
            index: ::std::os::raw::c_int,
            _total: ::std::os::raw::c_int,
        ) -> ::std::os::raw::c_int {
            let url = entry
                .map(|entry| CefString::from(&entry.url()).to_string())
                .unwrap_or_default();
            self.entries
                .lock()
                .expect("history mutex poisoned")
                .push(url);
            if current != 0 {
                *self.current.lock().expect("history mutex poisoned") = Some(index);
            }
            1
        }
    }
}

// -------------------------------------------------------------------------------------------------
// Loading
// -------------------------------------------------------------------------------------------------

pub fn read(name: &str) -> Result<Session, String> {
    let path = path_for(name).ok_or_else(|| format!("{name:?} is not a usable session name"))?;
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let session = Session::parse(&text);
    if session.tabs.is_empty() {
        return Err(format!("{} holds no tabs", path.display()));
    }
    Ok(session)
}

/// `:session-load [--clear] [--history] <name>`.
///
/// `clear` closes what is open first, which is what makes it a session *switch* rather than an
/// import; without it the session's tabs are appended, the way qutebrowser's `:session-load` without
/// `--clear` leaves the old window alone. `history` asks for the full replay described at the top of
/// this file.
pub fn load(state: &SharedState, name: &str, clear: bool, history: bool) -> Result<usize, String> {
    let session = read(name)?;
    Ok(restore(state, &session, clear, history))
}

/// Put a [`Session`] into the window. Answers how many tabs it opened.
pub fn restore(state: &SharedState, session: &Session, clear: bool, history: bool) -> usize {
    let before = if clear {
        close_all_tabs(state);
        0
    } else {
        state.lock().expect("state mutex poisoned").tab_count()
    };

    for tab in &session.tabs {
        // Without a replay a tab opens on the entry it was on, which is the page the user was
        // looking at. With one it opens on the *first* entry instead, because the replay walks
        // forward from there — opening on the current entry and then loading entry 0 would append
        // it rather than start the list.
        let url = if history { tab.history[0].as_str() } else { tab.current_url() };
        if url.is_empty() {
            continue;
        }
        // Every tab opens in the background; which one shows is decided once, below. Selecting as
        // each one arrives would focus a view per tab and leave the last one showing.
        crate::tabs::new_tab(state, url, true);
    }

    let opened = state
        .lock()
        .expect("state mutex poisoned")
        .tab_count()
        .saturating_sub(before);

    // The flags, now that the tabs exist. The mute has to reach CEF as well as the strip, and the
    // browser behind a tab created a moment ago may not exist yet — `apply_mute` is called again
    // from the replay, and a tab restored without `--history` gets it on the next `<Alt-m>`.
    {
        let mut guard = state.lock().expect("state mutex poisoned");
        for (offset, tab) in session.tabs.iter().enumerate() {
            let index = before + offset;
            guard.set_tab_pinned(index, tab.pinned);
            guard.set_tab_muted(index, tab.muted);
        }
    }
    for offset in 0..opened {
        crate::tabs::apply_mute(state, before + offset);
    }

    let snapshot = state.lock().expect("state mutex poisoned").tabs_snapshot();
    let tabs = crate::tabs::render_tabs(&snapshot);
    crate::ipc::set_tabs(tabs);

    if opened > 0 {
        crate::tabs::select(state, before + session.active_index().min(opened - 1));
    }

    if history {
        for (offset, tab) in session.tabs.iter().enumerate() {
            schedule_replay(before + offset, tab);
        }
    }

    opened
}

/// Close every tab, without the pinned check and without letting the last one take the window with
/// it — a session switch replaces the tabs, it does not quit bru.
fn close_all_tabs(state: &SharedState) {
    let (views, window) = {
        let mut state = state.lock().expect("state mutex poisoned");
        let views = state.tab_views();
        let window = state.window();
        for _ in 0..views.len() {
            state.take_active_tab();
        }
        (views, window)
    };
    for view in &views {
        if let Some(window) = &window {
            window.remove_child_view(Some(&mut View::from(view)));
        }
    }
    drop(views);
}

// -------------------------------------------------------------------------------------------------
// Replaying a navigation list
// -------------------------------------------------------------------------------------------------
//
// The only way to give a tab its history back, because CEF has no call that takes one. The tab is
// navigated through its saved URLs in order and then walked back to the entry it was on.
//
// Progress is measured against the navigation list itself rather than against a timer: after each
// `load_url` the replay polls `navigation_entries` until the count reaches what it asked for. That
// is the structure the replay exists to build, so a poll that says "not yet" is the same fact a
// load handler would have reported, and it needs no callback wired into `keys.rs`.

/// Start replaying one tab's history. Inert for a single-entry tab: it is already on its only page,
/// and `restore` opened it there.
///
/// The walk starts at entry **1**. Entry 0 is the load `restore` already asked for.
fn schedule_replay(index: usize, tab: &SessionTab) {
    if tab.history.len() < 2 {
        return;
    }
    let mut task = ReplayStep::new(index, tab.history.clone(), tab.current, 1, false, 0, 0);
    post_task(ThreadId::UI, Some(&mut task));
}

// Navigate `index`'s tab to `urls[step]`, wait for the navigation list to grow to `step + 1`, and
// come back for the next one. (CEF-NOTES trap 8: the wrap_ macros take no doc comment on the
// struct they declare, so this is a plain comment.)
//
// `issued` says whether this step's `load_url` has gone out; `waited` is how long it has been
// waiting, in milliseconds, and is what stops a dead link stalling the tab for ever. `total` is
// every poll of the whole replay summed, so the line it prints at the end carries a number rather
// than an impression of how long putting a history back takes.
//
// (Trap 8 covers the fields too: a `///` on one of them expands to `#[doc = ...]` and the macro has
// no rule for it. Field comments therefore live here.)
wrap_task! {
    struct ReplayStep {
        index: usize,
        urls: Vec<String>,
        target: usize,
        step: usize,
        issued: bool,
        waited: i64,
        total: i64,
    }

    impl Task {
        fn execute(&self) {
            debug_assert_ne!(currently_on(ThreadId::UI), 0);

            let Some(state) = crate::state::BruState::instance() else {
                return;
            };
            let browser = {
                let mut guard = state.lock().expect("state mutex poisoned");
                let id = guard.tab_browser_ids().get(self.index).copied().flatten();
                id.and_then(|id| guard.browser_with_id(id))
            };
            let Some(mut browser) = browser else {
                // The browser is not there yet: `browser_view_create` returns before CEF makes it,
                // and `on_browser_created` is what fills the identifier in.
                if self.waited < REPLAY_TIMEOUT_MS {
                    self.again(self.step, self.issued, self.waited + REPLAY_POLL_MS);
                }
                return;
            };

            let (entries, _) = read_history(&mut browser);

            if !self.issued {
                // The previous entry has to be **committed** first, and the test for that is its
                // URL sitting in the list — not the list's length.
                //
                // Measured 2026-08-06, and it cost the first replay: `navigation_entries` answers
                // with one entry the instant the browser exists, before anything has loaded.
                // Chromium keeps an *initial* NavigationEntry, so a length of 1 means nothing, and
                // a `load_url` issued against it replaces the pending first navigation instead of
                // following it. The replay of a → b → c came back holding b alone.
                if !committed(&entries, self.step - 1, &self.urls[self.step - 1]) {
                    if self.waited < REPLAY_TIMEOUT_MS {
                        self.again(self.step, false, self.waited + REPLAY_POLL_MS);
                    } else {
                        eprintln!(
                            "session: tab {} never committed {}; stopping the replay at {entries:?}",
                            self.index,
                            self.urls[self.step - 1]
                        );
                    }
                    return;
                }
                if let Some(frame) = browser.main_frame() {
                    frame.load_url(Some(&CefString::from(self.urls[self.step].as_str())));
                }
                self.again(self.step, true, 0);
                return;
            }

            // This step has arrived when its URL is the entry at its own index. That is the
            // structure the replay exists to build, so the list is the honest progress signal —
            // a timer would only be a guess about the same thing.
            let arrived = committed(&entries, self.step, &self.urls[self.step]);
            if !arrived {
                if self.waited < REPLAY_TIMEOUT_MS {
                    self.again(self.step, true, self.waited + REPLAY_POLL_MS);
                    return;
                }
                eprintln!(
                    "session: gave up on {} after {REPLAY_TIMEOUT_MS} ms, list is {entries:?}",
                    self.urls[self.step]
                );
            }

            let next = self.step + 1;
            if next < self.urls.len() {
                self.again(next, false, 0);
                return;
            }

            // Every entry is in. Walk back to the one the tab was on, and re-apply the mute now
            // that there is certainly a browser to apply it to.
            let last = entries.len().saturating_sub(1);
            let back = last.saturating_sub(self.target.min(last));
            if back > 0 {
                if let Some(frame) = browser.main_frame() {
                    // `history.go(-n)` rather than n calls to `go_back`: Chromium computes each
                    // `go_back` from the committed index, which has not moved yet, so the second
                    // call only replaces the first pending navigation (measured in `exec.rs`).
                    frame.execute_java_script(
                        Some(&CefString::from(format!("history.go(-{back});").as_str())),
                        None,
                        0,
                    );
                }
            }
            crate::tabs::apply_mute(&state, self.index);
            eprintln!(
                "session: tab {} replayed {} of {} entries in {} ms, back to {}",
                self.index,
                entries.len(),
                self.urls.len(),
                self.total,
                self.target
            );
        }
    }
}

/// Whether `entries[index]` is the page `url` asked for — the replay's one test for "this
/// navigation has committed".
///
/// Compared loosely at the tail: Chromium normalises a URL on commit, and `https://x.example` comes
/// back as `https://x.example/`. A replay that insisted on the byte-for-byte string would stall on
/// every site that has no path.
fn committed(entries: &[String], index: usize, url: &str) -> bool {
    entries
        .get(index)
        .map(|entry| entry.trim_end_matches('/') == url.trim_end_matches('/'))
        .unwrap_or(false)
}

impl ReplayStep {
    /// Come back for `step` after `REPLAY_POLL_MS`. A fresh task each time, because a `wrap_task!`
    /// object carries its fields by value and this is the only way to change one.
    fn again(&self, step: usize, issued: bool, waited: i64) {
        let mut task = ReplayStep::new(
            self.index,
            self.urls.clone(),
            self.target,
            step,
            issued,
            waited,
            self.total + REPLAY_POLL_MS,
        );
        post_delayed_task(ThreadId::UI, Some(&mut task), REPLAY_POLL_MS);
    }
}

// -------------------------------------------------------------------------------------------------
// Startup and the debug switch
// -------------------------------------------------------------------------------------------------

/// `--restore=<name>` (and `--restore-history`), read once from `on_context_initialized`.
///
/// Answers whether it opened anything: `app.rs` skips the start page when it did, because a start
/// page opened and then closed is a flash of the wrong site on every restore.
pub fn restore_at_startup(state: &SharedState) -> bool {
    let Some(command_line) = command_line_get_global() else {
        return false;
    };
    let name = CefString::from(&command_line.switch_value(Some(&CefString::from("restore"))))
        .to_string();
    if name.is_empty() {
        return false;
    }
    let history = command_line.has_switch(Some(&CefString::from("restore-history"))) == 1;
    match read(&name) {
        Ok(session) => {
            let opened = restore(state, &session, false, history);
            eprintln!("session: restored {opened} tabs from {name:?}");
            opened > 0
        }
        Err(e) => {
            eprintln!("session: could not restore {name:?}: {e}");
            false
        }
    }
}

/// `--session-script='save:one|list|load:one' --session-step-ms=N` runs session commands from posted
/// UI tasks and prints what each one left behind.
///
/// The same reason as every other `--*-script` switch: `wtype` segfaults CEF, so nothing here can be
/// driven by a keypress in a check that runs twice. Two of the steps exist only to be measured —
/// `entries` prints every tab's navigation list, which is the fact the whole "can history be
/// restored" question turns on. Inert unless the switch is passed.
pub fn schedule_script(steps: &str, interval_ms: i64) {
    for (i, step) in steps.split('|').filter(|s| !s.is_empty()).enumerate() {
        let mut task = SessionStep::new(step.to_string());
        post_delayed_task(ThreadId::UI, Some(&mut task), interval_ms * (i as i64 + 1));
    }
}

wrap_task! {
    struct SessionStep {
        step: String,
    }

    impl Task {
        fn execute(&self) {
            debug_assert_ne!(currently_on(ThreadId::UI), 0);

            let Some(state) = crate::state::BruState::instance() else {
                return;
            };
            let (verb, argument) = match self.step.split_once(':') {
                Some((verb, argument)) => (verb, argument),
                None => (self.step.as_str(), ""),
            };
            match verb {
                "save" => match save(&state, name_or_default(argument)) {
                    Ok(path) => eprintln!("session: saved to {}", path.display()),
                    Err(e) => eprintln!("session: save failed: {e}"),
                },
                "load" => match load(&state, name_or_default(argument), false, false) {
                    Ok(n) => eprintln!("session: loaded {n} tabs"),
                    Err(e) => eprintln!("session: load failed: {e}"),
                },
                "load-clear" => match load(&state, name_or_default(argument), true, false) {
                    Ok(n) => eprintln!("session: loaded {n} tabs, cleared first"),
                    Err(e) => eprintln!("session: load failed: {e}"),
                },
                "load-history" => match load(&state, name_or_default(argument), true, true) {
                    Ok(n) => eprintln!("session: loaded {n} tabs with history, cleared first"),
                    Err(e) => eprintln!("session: load failed: {e}"),
                },
                "delete" => match delete(name_or_default(argument)) {
                    Ok(path) => eprintln!("session: deleted {}", path.display()),
                    Err(e) => eprintln!("session: delete failed: {e}"),
                },
                "list" => eprintln!("session: saved sessions {:?}", list()),
                "pin" => crate::tabs::toggle_pin(&state),
                "mute" => crate::tabs::toggle_mute(&state),
                // The measurement this module exists to report: every tab's whole navigation list,
                // straight out of `navigation_entries`, with the entry each tab is on.
                "entries" => {
                    let session = snapshot(&state);
                    for (index, tab) in session.tabs.iter().enumerate() {
                        eprintln!(
                            "session: tab {index} current={} pinned={} muted={} entries={:?}",
                            tab.current, tab.pinned, tab.muted, tab.history
                        );
                    }
                }
                "file" => {
                    match path_for(name_or_default(argument))
                        .ok_or_else(|| "no data directory".to_string())
                        .and_then(|path| {
                            std::fs::read_to_string(&path).map_err(|e| format!("{e}"))
                        }) {
                        Ok(text) => eprintln!("session: file is\n{text}"),
                        Err(e) => eprintln!("session: could not read the file: {e}"),
                    }
                }
                other => eprintln!("session: no step named {other:?}"),
            }
        }
    }
}

fn name_or_default(name: &str) -> &str {
    if name.is_empty() { DEFAULT_NAME } else { name }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn three_tabs() -> Session {
        Session {
            tabs: vec![
                SessionTab {
                    history: vec!["https://a/".into(), "https://b/".into()],
                    current: 1,
                    active: false,
                    pinned: true,
                    muted: false,
                },
                SessionTab {
                    history: vec!["https://c/".into()],
                    current: 0,
                    active: true,
                    pinned: false,
                    muted: true,
                },
                SessionTab {
                    history: vec!["https://d/".into(), "https://e/".into(), "https://f/".into()],
                    current: 0,
                    active: false,
                    pinned: false,
                    muted: false,
                },
            ],
        }
    }

    #[test]
    fn a_session_survives_the_round_trip() {
        let session = three_tabs();
        assert_eq!(Session::parse(&session.to_text()), session);
    }

    #[test]
    fn the_active_tab_and_the_current_entry_are_what_a_restore_opens() {
        let session = three_tabs();
        assert_eq!(session.active_index(), 1);
        // Tab 0 was on its *second* entry: restoring it on the first would silently lose a page.
        assert_eq!(session.tabs[0].current_url(), "https://b/");
        assert_eq!(session.tabs[2].current_url(), "https://d/");
    }

    #[test]
    fn the_written_format_is_the_one_the_docs_describe() {
        let session = Session {
            tabs: vec![SessionTab {
                history: vec!["https://a/".into(), "https://b/".into()],
                current: 1,
                active: true,
                pinned: false,
                muted: false,
            }],
        };
        assert_eq!(
            session.to_text(),
            "bru-session 1\n\
             tab active=1 current=1 pinned=0 muted=0\n\
             entry https://a/\n\
             entry https://b/\n"
        );
    }

    #[test]
    fn a_file_from_a_later_bru_still_opens() {
        // Unknown keys, an unknown line type, and a key order this bru does not write.
        let session = Session::parse(
            "bru-session 2\n\
             window geometry=nonsense\n\
             tab zoom=150 current=1 active=1 scroll=42 pinned=1\n\
             entry https://a/\n\
             entry https://b/\n",
        );
        assert_eq!(session.tabs.len(), 1);
        assert_eq!(session.tabs[0].current, 1);
        assert!(session.tabs[0].active);
        assert!(session.tabs[0].pinned);
        assert_eq!(session.tabs[0].history.len(), 2);
    }

    #[test]
    fn a_broken_file_loses_the_broken_part_and_nothing_else() {
        // `current` past the end, an entry with no tab, and a tab with no entries.
        let session = Session::parse(
            "entry https://orphan/\n\
             tab active=1 current=9\n\
             entry https://a/\n\
             tab active=0 current=0\n",
        );
        assert_eq!(session.tabs.len(), 1, "the empty tab is dropped");
        assert_eq!(session.tabs[0].current, 0, "current is clamped into the list");
        assert_eq!(session.tabs[0].history, vec!["https://a/".to_string()]);
    }

    #[test]
    fn a_session_name_cannot_reach_outside_its_directory() {
        // The value of this test is the refusal, not the path: `:session-save ../../config.lua`
        // would otherwise write into ~/.config/bru, which is configer's and which bru never writes.
        for name in ["../x", "a/b", "..", ".hidden", "", "a\\b"] {
            assert!(path_for(name).is_none(), "{name:?} should not be a session name");
        }
        // And the ordinary case still works, whatever the data directory happens to be.
        if let Some(path) = path_for("work") {
            assert!(path.ends_with("sessions/work.bru"));
        }
    }

    /// The test the replay turns on, and the reason it is a URL comparison rather than a count.
    ///
    /// Measured 2026-08-06: `navigation_entries` answers with **one** entry the instant a browser
    /// exists, before anything has loaded — Chromium's initial NavigationEntry. A replay that read
    /// that length as "entry 0 has arrived" issued its next `load_url` against a pending navigation
    /// and replaced it: a → b → c came back holding b alone.
    #[test]
    fn a_pending_first_navigation_is_not_a_committed_entry() {
        // What the list looks like before anything has loaded.
        assert!(!committed(&["".to_string()], 0, "https://a/"));
        assert!(!committed(&["about:blank".to_string()], 0, "https://a/"));
        // And after.
        assert!(committed(&["https://a/".to_string()], 0, "https://a/"));
        // Chromium normalises a bare host on commit; a byte-for-byte test would stall on it.
        assert!(committed(&["https://a.example/".to_string()], 0, "https://a.example"));
        // Past the end is not committed either — that is the "the list has not grown yet" case.
        assert!(!committed(&["https://a/".to_string()], 1, "https://b/"));
    }

    #[test]
    fn an_empty_session_is_not_written() {
        assert_eq!(Session::default().to_text(), "bru-session 1\n");
        assert!(Session::parse("bru-session 1\n").tabs.is_empty());
    }
}
