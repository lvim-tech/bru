//! The dispatcher: the one place a [`Command`] becomes an action.
//!
//! It lives in its own file because eight workstreams add arms to it. `keys.rs` translates a
//! keypress into a `Command` and calls [`run`]; nothing else in bru turns a command into an effect.
//!
//! **The match in [`run`] has no `_` arm, and neither does [`is_live`].** That is what keeps the two
//! honest: a new [`Command`] variant fails to compile until both have been told what it does and
//! whether it does anything, and the count of live bindings at the bottom of this file cannot
//! quietly go stale.

use cef::*;
use std::sync::{Arc, Mutex};

use crate::commands::{Command, TabIndex, TabMove};
use crate::tabs::SharedState;


/// A ceiling on `<count><command>`. qutebrowser has none, but a typo like `99999j` should not lock
/// the UI thread up sending wheel events.
const MAX_COUNT: u32 = 1000;

/// qutebrowser's `zoom.levels` (configdata.yml:2700-2722), in percent, and its `zoom.default` of
/// 100 — the level `=` returns to.
const ZOOM_LEVELS: [u32; 16] = [25, 33, 50, 67, 75, 90, 100, 110, 125, 150, 175, 200, 250, 300, 400, 500];
const ZOOM_DEFAULT: u32 = 100;

/// Run one command against the browser the key arrived at.
///
/// `browser` is always a tab, never a chrome strip: `keys.rs` redirects a key that landed on a strip
/// at the showing tab before calling here (CEF-NOTES trap 11).
pub fn run(state: &SharedState, browser: &mut Browser, command: &Command, count: Option<u32>) {
    // `3j` is three steps of `j`, not one big one — qutebrowser repeats the command.
    let repeat = count.unwrap_or(1).clamp(1, MAX_COUNT);

    match command {
        // --- chains -------------------------------------------------------------------------
        Command::Chain(parts) => {
            for part in parts {
                run(state, browser, part, count);
            }
        }

        // --- scrolling ----------------------------------------------------------------------
        // The reason bru exists. Through `send_mouse_wheel_event`, never `window.scrollBy`: the
        // wheel path is Chromium's real input path, animation included.
        // All four go through `scroll.rs`, which knows two things this arm did not. A single wheel
        // event moves at most one viewport whatever the delta says — measured 10,000,000 px moving
        // 1,256 — so anything longer than a screen has to be several events or it is silently
        // truncated. And both axes are negated, not just the vertical one: `scroll-px 2000 0` used
        // to move *left*.
        Command::Scroll(direction) => crate::scroll::scroll(state, browser, *direction, count),
        Command::ScrollPx { dx, dy } => crate::scroll::scroll_px(state, browser, *dx, *dy, count),
        Command::ScrollPage { x, y } => crate::scroll::scroll_page(state, browser, *x, *y, count),
        Command::ScrollToPerc { perc, horizontal } => {
            crate::scroll::scroll_to_perc(state, browser, *perc, *horizontal, count)
        }

        // --- tabs ---------------------------------------------------------------------------
        Command::TabNext => {
            for _ in 0..repeat {
                crate::tabs::next_tab(state);
            }
        }
        Command::TabPrev => {
            for _ in 0..repeat {
                crate::tabs::prev_tab(state);
            }
        }
        Command::TabClose { .. } => crate::tabs::close_current(state),
        Command::TabOnly { .. } => crate::tabs::close_others(state),

        // `commands.py:978-1018`. A count beats the argument; a negative index counts from the end;
        // and asking for the tab you are already on hops to the last-focused one instead, which is
        // what makes `<Alt-1>` a toggle when you are on tab 1.
        Command::TabFocus { index } => {
            let index = count.map(|c| TabIndex::Number(c as i32)).or(*index);
            let (active, total, last) = tab_positions(state);
            if total == 0 {
                return;
            }
            match index {
                None => crate::tabs::next_tab(state),
                Some(TabIndex::Last) => {
                    if let Some(last) = last {
                        crate::tabs::select(state, last);
                    }
                }
                Some(TabIndex::Number(n)) => {
                    let n = if n < 0 { total as i32 + n + 1 } else { n };
                    if n == active as i32 + 1 {
                        if let Some(last) = last {
                            crate::tabs::select(state, last);
                        }
                    } else if n >= 1 && n <= total as i32 {
                        crate::tabs::select(state, (n - 1) as usize);
                    }
                }
            }
        }

        // `commands.py:1025-1065`. `+`/`-` move by the count (default 1) and wrap, because
        // `tabs.wrap` defaults to true; everything else is absolute, and a count overrides it.
        Command::TabMove { to } => {
            let (active, total, _) = tab_positions(state);
            if total == 0 {
                return;
            }
            let total = total as i32;
            let new = match (to, count) {
                (TabMove::Relative(sign), _) => {
                    (active as i32 + repeat as i32 * sign).rem_euclid(total)
                }
                (_, Some(c)) => c as i32 - 1,
                (TabMove::Start, None) => 0,
                (TabMove::End, None) => total - 1,
                (TabMove::Index(i), None) => {
                    if *i >= 0 {
                        i - 1
                    } else {
                        i + total
                    }
                }
            };
            if (0..total).contains(&new) {
                crate::tabs::move_current(state, new as usize);
            }
        }

        // CEF exposes no way to serialise a tab's navigation list, so a clone is the same *page*
        // rather than the same tab: a new tab on the current URL, with an empty history.
        Command::TabClone { bg, window, .. } => {
            if let Some(url) = active_tab_url(state) {
                // `-w` has no window management behind it yet; a tab is closer than nothing.
                let _ = window;
                crate::tabs::new_tab(state, &url, *bg);
            }
        }

        // The URL of a closed tab is all that is kept, for the same reason. `2u` reaches one
        // further down the stack rather than reopening two tabs — the count is a depth.
        Command::Undo { window } => {
            // `undo -w` reopens a closed *window*, and bru has one window that outlives its tabs.
            if *window {
                return;
            }
            let url = state
                .lock()
                .expect("state mutex poisoned")
                .take_closed_tab(repeat as usize);
            if let Some(url) = url.filter(|url| !url.is_empty()) {
                crate::tabs::new_tab(state, &url, false);
            }
        }

        // --- opening ------------------------------------------------------------------------
        // `open` is M9's command, and most of it needs the command line to type a URL into. The
        // part that does not is worth having early: `ga` and `<Ctrl-T>` are bound to a bare
        // `open -t`, so without this there is no way to reach a second tab from the keyboard at
        // all, and `J`/`K`/`d` cannot be exercised. A URL only arrives here from a binding that
        // carries one; the interactive path is M9's.
        //
        // `open.rs` decides what the string *is* before anything is loaded: a URL, a file, or a
        // search — and with which engine. `-w` still behaves as a tab, because bru has one window.
        Command::Open { url, tab, bg, window, .. } => {
            crate::open::open(state, browser, url.as_deref(), *tab || *window, *bg)
        }

        // --- navigation ---------------------------------------------------------------------
        Command::Back { tab, bg, window } => {
            back_forward(state, browser, false, repeat, *tab || *bg || *window, *bg)
        }
        Command::Forward { tab, bg, window } => {
            back_forward(state, browser, true, repeat, *tab || *bg || *window, *bg)
        }
        Command::Reload { force } => {
            if *force {
                browser.reload_ignore_cache();
            } else {
                browser.reload();
            }
        }
        Command::Stop => browser.stop_load(),
        // Through `open` so that a `start_page` set in config.lua is honoured here too, and is
        // fuzzy-parsed the same way — `bru.set("start_page", "example.com")` has to work.
        Command::Home => crate::open::open(state, browser, None, false, false),

        // --- zoom ---------------------------------------------------------------------------
        // A count beats the argument, as everywhere else (`zoomcommands.py:64`).
        Command::Zoom { level } => {
            set_zoom_percent(browser, count.or(*level).unwrap_or(ZOOM_DEFAULT));
        }
        Command::ZoomIn => zoom_by(browser, repeat as i32),
        Command::ZoomOut => zoom_by(browser, -(repeat as i32)),

        // --- the window ---------------------------------------------------------------------
        // `--leave` in qutebrowser means "leave the fullscreen the *page* asked for"; bru has no
        // page-fullscreen handler yet, so it is the plain "not fullscreen" of the two.
        Command::Fullscreen { enter, leave } => {
            let window = state.lock().expect("state mutex poisoned").window();
            if let Some(window) = window {
                let to = match (enter, leave) {
                    (true, _) => 1,
                    (_, true) => 0,
                    _ => i32::from(window.is_fullscreen() == 0),
                };
                window.set_fullscreen(to);
            }
        }

        // --- lifetime -----------------------------------------------------------------------
        // Closing the window is the whole teardown: `can_close` → `do_close` → `on_before_close`
        // → `quit_message_loop`, the path `--close-after-ms` already exercises. There is one
        // window, so `close` and `quit` do the same thing; `--save` waits for sessions.
        Command::Quit { .. } | Command::Close => {
            let window = state.lock().expect("state mutex poisoned").window();
            if let Some(window) = window {
                window.close();
            }
        }

        // --- modes --------------------------------------------------------------------------
        Command::ModeEnter(mode) => {
            let entered = state
                .lock()
                .expect("state mutex poisoned")
                .enter_mode(*mode, false);
            if entered {
                crate::ipc::set_mode(mode.name().to_string());
            }
        }
        Command::ModeLeave => {
            let mut guard = state.lock().expect("state mutex poisoned");
            if guard.leave_mode() {
                let now = guard.mode();
                drop(guard);
                crate::ipc::set_mode(now.name().to_string());
                // Leaving insert mode should also give the page's text field up, or the next `j`
                // is typed into it rather than scrolling.
                blur(browser);
            }
        }

        // --- the command line ---------------------------------------------------------------
        // SLOT: src/cmdline.rs.
        Command::CmdSetText { .. } | Command::CommandAccept { .. } => {}

        // Nothing to do, and that is the point: `nop` exists to shadow a Chromium default, and
        // clear-keychain is already done by the parser reporting the key.
        Command::Nop | Command::ClearKeychain => {}

        // A command qutebrowser has and bru's parser does not know. It kept its place in the trie
        // so `;` still reports a partial match; running it does nothing.
        Command::Unimplemented(_) => {}
    }
}

/// Whether [`run`] does anything for this command — the *only* thing that may disagree with the
/// match above, and the reason both are exhaustive.
///
/// Used to count how many of qutebrowser's 226 default bindings are live, which is the number each
/// milestone of stage 2 is measured by, and printed by `--cmd` beside each step.
pub fn is_live(command: &Command) -> bool {
    match command {
        // A chain is live when every link is: `clear-keychain ;; search` half-works, and half is
        // not what the binding means.
        Command::Chain(parts) => parts.iter().all(is_live),

        // All four directions live now: `scroll.rs` reaches the top and the bottom by sending many
        // wheel events rather than one impossible one.
        Command::Scroll(_) => true,
        Command::ScrollPx { .. } | Command::ScrollPage { .. } | Command::ScrollToPerc { .. } => true,

        Command::TabNext
        | Command::TabPrev
        | Command::TabClose { .. }
        | Command::TabOnly { .. }
        | Command::TabFocus { .. }
        | Command::TabMove { .. }
        | Command::TabClone { .. } => true,
        // `undo -w` is the one spelling that does nothing: there is one window.
        Command::Undo { window } => !window,

        Command::Open { .. } => true,

        Command::Back { .. }
        | Command::Forward { .. }
        | Command::Reload { .. }
        | Command::Stop
        | Command::Home => true,

        Command::Zoom { .. } | Command::ZoomIn | Command::ZoomOut => true,
        Command::Fullscreen { .. } => true,

        Command::Quit { .. } | Command::Close => true,

        Command::ModeEnter(_) | Command::ModeLeave => true,

        Command::CmdSetText { .. } | Command::CommandAccept { .. } => false,

        Command::Nop | Command::ClearKeychain => true,

        Command::Unimplemented(_) => false,
    }
}

/// `:back`/`:forward`, with a count and with `-t`/`-b`/`-w`.
///
/// In place, `go_back`/`go_forward` are one step each and a count of one is the overwhelming case.
/// For a count above one they are not repeated — Chromium computes each from the *committed* index,
/// which has not moved yet, so a second call only replaces the first pending navigation. The page's
/// own `history.go(-n)` is the same navigation Chromium performs internally and does move n entries;
/// measured 2026-08-06 with `--cmd`, and recorded in the report for CEF-NOTES.
///
/// With `-t`/`-b`/`-w` there is no history to clone (CEF exposes no serialisation of a navigation
/// list), so the new tab opens on the entry the command would have moved to — read out of
/// `navigation_entries`.
fn back_forward(
    state: &SharedState,
    browser: &mut Browser,
    forward: bool,
    steps: u32,
    new_tab: bool,
    background: bool,
) {
    let offset = if forward { steps as i32 } else { -(steps as i32) };

    if new_tab {
        if let Some(url) = entry_at_offset(browser, offset) {
            crate::tabs::new_tab(state, &url, background);
        }
        return;
    }

    if steps == 1 {
        if forward {
            browser.go_forward();
        } else {
            browser.go_back();
        }
        return;
    }

    if let Some(frame) = browser.main_frame() {
        frame.execute_java_script(
            Some(&CefString::from(format!("history.go({offset});").as_str())),
            None,
            0,
        );
    }
}

/// The URL `offset` entries away from the current one in this tab's history, or `None` if there is
/// no such entry.
///
/// `navigation_entries` visits synchronously on the UI thread, which is the thread a key is handled
/// on, so the answer is ready by the time the call returns.
fn entry_at_offset(browser: &mut Browser, offset: i32) -> Option<String> {
    let host = browser.host()?;
    let entries: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let current: Arc<Mutex<Option<i32>>> = Arc::new(Mutex::new(None));

    let mut visitor = HistoryVisitor::new(entries.clone(), current.clone());
    host.navigation_entries(Some(&mut visitor), 0);

    let current = (*current.lock().expect("history mutex poisoned"))?;
    let wanted = current + offset;
    if wanted < 0 {
        return None;
    }
    let entries = entries.lock().expect("history mutex poisoned");
    entries.get(wanted as usize).cloned()
}

// A tab's whole navigation list, and which entry it is on. CEF visits in index order and marks one
// entry `current`, so the vector's own indices are the history's.
wrap_navigation_entry_visitor! {
    struct HistoryVisitor {
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
            // Keep going: the wanted entry may be further along than the current one.
            1
        }
    }
}

/// Where the showing tab sits, how many there are, and where `tab-focus last` would go.
fn tab_positions(state: &SharedState) -> (usize, usize, Option<usize>) {
    let state = state.lock().expect("state mutex poisoned");
    (
        state.active_tab(),
        state.tab_count(),
        state.last_active_tab(),
    )
}

fn active_tab_url(state: &SharedState) -> Option<String> {
    let state = state.lock().expect("state mutex poisoned");
    state
        .tab_url(state.active_tab())
        .filter(|url| !url.is_empty())
}

/// CEF's zoom level is logarithmic: a factor of 1.2 per step, so 100% is level 0.
fn zoom_percent(browser: &mut Browser) -> u32 {
    let Some(host) = browser.host() else {
        return ZOOM_DEFAULT;
    };
    (1.2f64.powf(host.zoom_level()) * 100.0).round() as u32
}

fn set_zoom_percent(browser: &mut Browser, percent: u32) {
    let Some(host) = browser.host() else {
        return;
    };
    let percent = percent.max(1);
    host.set_zoom_level((percent as f64 / 100.0).ln() / 1.2f64.ln());
}

/// `zoom-in`/`zoom-out`: `offset` places along qutebrowser's list of levels, stopping at the ends
/// (`AbstractZoom.apply_offset`, over a NeighborList in `edge` mode).
fn zoom_by(browser: &mut Browser, offset: i32) {
    let current = zoom_percent(browser);
    // The nearest level to where the page actually is, so a zoom set by `:zoom 137` still steps.
    let nearest = ZOOM_LEVELS
        .iter()
        .enumerate()
        .min_by_key(|(_, level)| level.abs_diff(current))
        .map(|(index, _)| index as i32)
        .unwrap_or(0);
    let index = (nearest + offset).clamp(0, ZOOM_LEVELS.len() as i32 - 1) as usize;
    set_zoom_percent(browser, ZOOM_LEVELS[index]);
}

/// Drop focus from whatever the page had focused. One-off script rather than a CEF call because
/// CEF has no "blur the focused element" — and this runs on leaving insert mode, not on the key
/// path proper.
fn blur(browser: &mut Browser) {
    let Some(frame) = browser.main_frame() else {
        return;
    };
    frame.execute_java_script(
        Some(&CefString::from(
            "document.activeElement && document.activeElement.blur();",
        )),
        None,
        0,
    );
}

/// `--cmd='<step>|<step>|…' --cmd-step-ms=N` runs command strings through [`run`], one every N
/// milliseconds, and prints what each one left behind.
///
/// It exists because keys cannot drive an unattended check on this machine: the only injection tool
/// is `wtype`, which attaches a virtual keyboard, and CEF segfaults in `xkb_state_update_mask` when
/// the keymap arrives (CEF-NOTES, "Injecting keys on this machine"). This drives the same function
/// a key drives, one step past the key parser, from a posted UI task. Inert unless the switch is
/// passed.
///
/// A step is `[<count>:]<command string>`, so `3:tab-move +` is what `3gJ` would do. `|` separates
/// steps because `;;` is the command language's own chain separator and a URL may contain a comma.
pub fn schedule_cmd_script(steps: &str, interval_ms: i64) {
    for (i, step) in steps.split('|').filter(|s| !s.is_empty()).enumerate() {
        let at = interval_ms * (i as i64 + 1);
        let mut task = CmdStep::new(step.to_string());
        post_delayed_task(ThreadId::UI, Some(&mut task), at);

        // The report is its own task, two thirds of a step later. `load_url`, `go_back` and
        // `reload` all return before the navigation commits, so a report taken on the same task
        // describes the page the step replaced — measured 2026-08-06, and it reads as an
        // off-by-one that is not there.
        let mut report = CmdReport::new(step.to_string());
        post_delayed_task(ThreadId::UI, Some(&mut report), at + interval_ms * 2 / 3);
    }
}

wrap_task! {
    struct CmdReport {
        step: String,
    }

    impl Task {
        fn execute(&self) {
            if let Some(state) = crate::state::BruState::instance() {
                report(&state, &self.step);
            }
        }
    }
}

wrap_task! {
    struct CmdStep {
        step: String,
    }

    impl Task {
        fn execute(&self) {
            debug_assert_ne!(currently_on(ThreadId::UI), 0);

            let Some(state) = crate::state::BruState::instance() else {
                return;
            };

            let (count, text) = match self.step.split_once(':') {
                Some((count, rest)) => match count.parse::<u32>() {
                    Ok(count) => (Some(count), rest),
                    // `:open http://x` — a colon that is not a count prefix.
                    Err(_) => (None, self.step.as_str()),
                },
                None => (None, self.step.as_str()),
            };

            let command = match crate::commands::parse(text) {
                Ok(command) => command,
                Err(error) => {
                    eprintln!("cmd: {text:?} does not parse: {error}");
                    return;
                }
            };

            let Some(mut browser) = state.lock().expect("state mutex poisoned").active_browser()
            else {
                eprintln!("cmd: no tab to run {text:?} against");
                return;
            };

            if !is_live(&command) {
                eprintln!("cmd: {text:?} is not implemented — the dispatcher will ignore it");
            }
            run(&state, &mut browser, &command, count);
        }
    }
}

/// One line per step: what the command was, and the state a screenshot would have to agree with.
///
/// `order` is the strip, left to right, each tab shown by the tail of its URL — without it a
/// `tab-move` that did nothing and one that moved a tab back to where it was look identical.
fn report(state: &SharedState, step: &str) {
    let (active, total, url, order) = {
        let guard = state.lock().expect("state mutex poisoned");
        let active = guard.active_tab();
        let total = guard.tab_count();
        let url = guard.tab_url(active).unwrap_or_default();
        let order: Vec<String> = (0..total)
            .map(|i| tail(&guard.tab_url(i).unwrap_or_default()))
            .collect();
        (active, total, url, order.join(","))
    };
    // Re-fetched outside the lock: reading the zoom is a CEF call.
    let browser = state.lock().expect("state mutex poisoned").active_browser();
    let zoom = browser
        .map(|mut browser| zoom_percent(&mut browser))
        .unwrap_or(ZOOM_DEFAULT);

    eprintln!(
        "cmd: after {step:?} -> tabs={total} active={active} zoom={zoom}% order=[{order}] url={url}"
    );
}

/// The last few characters of a URL — enough to tell `<h1>A</h1>` from `<h1>B</h1>`.
fn tail(url: &str) -> String {
    const KEEP: usize = 12;
    let chars: Vec<char> = url.chars().collect();
    if chars.len() <= KEEP {
        return url.to_string();
    }
    chars[chars.len() - KEEP..].iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands;
    use crate::config::DEFAULT_BINDINGS;

    /// The three-way split of qutebrowser's 226 default bindings, printed rather than only
    /// asserted: the headline number of every stage-2 milestone is "how many are live", and a
    /// number that is not printed is a number nobody checks.
    fn split() -> (usize, usize, usize) {
        let (mut live, mut ignored, mut unparsed) = (0, 0, 0);
        for (_mode, _keys, cmd) in DEFAULT_BINDINGS {
            let parsed = commands::parse(cmd).expect("a default binding must parse");
            if !parsed.is_implemented() {
                unparsed += 1;
            } else if is_live(&parsed) {
                live += 1;
            } else {
                ignored += 1;
            }
        }
        (live, ignored, unparsed)
    }

    #[test]
    fn how_many_default_bindings_are_live() {
        let (live, ignored, unparsed) = split();
        println!(
            "default bindings: {live} live, {ignored} parsed but ignored, {unparsed} unparsed, \
             {} total",
            live + ignored + unparsed
        );
        assert_eq!(live + ignored + unparsed, DEFAULT_BINDINGS.len());
        assert_eq!(DEFAULT_BINDINGS.len(), 226);
        // Stage 2, as each workstream is wired in: 27 before any of it, 70 after the dispatcher,
        // 76 once `scroll.rs` made `gg`, `G` and the page keys real. Raise this when a milestone
        // raises the number, never to make a failing build pass.
        assert_eq!(live, 76, "the live-binding count moved");
    }

    /// The bindings this milestone made live, named one by one — a total is not enough to notice
    /// that `gJ` went live and `gK` did not.
    #[test]
    fn the_bindings_m9_turned_on() {
        for keys in [
            "H", "L", "th", "wh", "tl", "wl", // back/forward, in place and in a tab
            "r", "R", "<F5>", "<Ctrl-F5>", // reload
            "<Ctrl-s>", "<Ctrl-h>", // stop, home
            "<Ctrl-Q>", "ZQ", "ZZ", "<Ctrl-Shift-W>", // quit, close
            "co", // tab-only
            "<Ctrl-Tab>", "<Ctrl-^>", "<Alt-1>", "<Alt-9>", "g0", "g^", "g$", // tab-focus
            "gm", "gJ", "gK", // tab-move
            "gC", // tab-clone
            "u", "<Ctrl-Shift-T>", // undo
            "-", "+", "=",  // zoom
            "<F11>", // fullscreen
        ] {
            let (_, _, cmd) = DEFAULT_BINDINGS
                .iter()
                .find(|(mode, k, _)| *mode == "normal" && *k == keys)
                .unwrap_or_else(|| panic!("no default binding for {keys}"));
            let parsed = commands::parse(cmd).expect("a default binding must parse");
            assert!(is_live(&parsed), "{keys} -> {cmd:?} is still inert");
        }
        // `U` is `undo -w`, and bru has one window: it parses and stays inert deliberately.
        assert!(!is_live(&commands::parse("undo -w").unwrap()));
    }

    #[test]
    fn tab_move_arguments() {
        use crate::commands::TabMove;
        assert_eq!(commands::parse("tab-move").unwrap(), Command::TabMove { to: TabMove::Start });
        assert_eq!(
            commands::parse("tab-move +").unwrap(),
            Command::TabMove { to: TabMove::Relative(1) }
        );
        assert_eq!(
            commands::parse("tab-move -").unwrap(),
            Command::TabMove { to: TabMove::Relative(-1) }
        );
        assert_eq!(
            commands::parse("tab-move end").unwrap(),
            Command::TabMove { to: TabMove::End }
        );
        assert_eq!(
            commands::parse("tab-move 2").unwrap(),
            Command::TabMove { to: TabMove::Index(2) }
        );
        assert!(commands::parse("tab-move sideways").is_err());
    }

    #[test]
    fn zoom_arguments() {
        assert_eq!(commands::parse("zoom").unwrap(), Command::Zoom { level: None });
        assert_eq!(commands::parse("zoom 150%").unwrap(), Command::Zoom { level: Some(150) });
        assert_eq!(commands::parse("zoom 150").unwrap(), Command::Zoom { level: Some(150) });
        assert!(commands::parse("zoom huge").is_err());
    }
}
