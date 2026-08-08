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
///
// --- unhardcoded -------------------------------------------------------------------------------
/// **Both are settings now**, `zoom.levels` and `zoom.default`, with these exact values as their
/// defaults. `ZOOM_DEFAULT` is still what a browser with no host answers with, because a fallback
/// that took the settings mutex to say "there is no page" would be a lock for nothing.
// --- end unhardcoded ---------------------------------------------------------------------------
const ZOOM_LEVELS: [u32; 16] = [25, 33, 50, 67, 75, 90, 100, 110, 125, 150, 175, 200, 250, 300, 400, 500];
const ZOOM_DEFAULT: u32 = 100;

// --- unhardcoded -------------------------------------------------------------------------------
/// `zoom.levels`, sorted and deduplicated, as `zoom-in`/`zoom-out` step along them.
///
/// **The sort is here and not in the store**, and that is what makes an appending override work:
/// a bru list layers the user's entries on the end of bru's own (see `settings::ListShape`), so
/// `:config-list-add zoom.levels 133%` would otherwise land after `500%` and make `+` jump from
/// 500 to 133. Sorting at read time turns the list into a set of levels, which is what a reader of
/// `zoom.levels` means by it.
///
/// Off the key path: `+` and `-` are typed by hand, one lock per press, and the list is sixteen
/// entries long.
fn zoom_levels() -> Vec<u32> {
    let mut levels: Vec<u32> = crate::settings::list_of("zoom.levels")
        .iter()
        .filter_map(|entry| crate::settings::percent_of(entry))
        .collect();
    levels.sort_unstable();
    levels.dedup();
    if levels.is_empty() {
        // A process with no settings store — a renderer, or a unit test. bru's own sixteen.
        return ZOOM_LEVELS.to_vec();
    }
    levels
}

/// `zoom.default`, the level `:zoom` with no argument and `=` return to.
fn zoom_default() -> u32 {
    match crate::settings::int_of("zoom.default") {
        percent if percent > 0 => percent as u32,
        _ => ZOOM_DEFAULT,
    }
}
// --- end unhardcoded ---------------------------------------------------------------------------

/// Run one command against the browser the key arrived at.
///
/// `browser` is always a tab, never a chrome strip: `keys.rs` redirects a key that landed on a strip
/// at the showing tab before calling here (CEF-NOTES trap 11).
pub fn run(state: &SharedState, browser: &mut Browser, command: &Command, count: Option<u32>) {
    // The command line takes its own commands first. `cmd-set-text`, `command-accept` and every
    // `rl-*` / `command-history-*` binding are handled inside `cmdline.rs` — the last two groups
    // arrive as `Command::Unimplemented` and are matched there by name, so they never reach the
    // match below. `false` means it was none of them.
    if crate::cmdline::run_command(command, count) {
        return;
    }

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
        // `-f` is what gets past a pinned tab — qutebrowser prompts instead, and bru has no yes/no
        // mode to prompt in (src/tabs.rs).
        Command::TabClose { force, .. } => crate::tabs::close_current(state, *force),
        Command::TabOnly { force } => crate::tabs::close_others(state, *force),

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
// --- src/window.rs ---------------------------------------------------------
                // `-w` is a window of its own. It was a tab standing in for one.
                if *window {
                    crate::window::open(state, &url);
                    return;
                }
// --- end src/window.rs -----------------------------------------------------
                crate::tabs::new_tab(state, &url, *bg);
            }
        }

        // The URL of a closed tab is all that is kept, for the same reason. `2u` reaches one
        // further down the stack rather than reopening two tabs — the count is a depth.
        Command::Undo { window } => {
// --- src/window.rs ---------------------------------------------------------
            // `U` — the last closed *window*, with every tab it held, in the order it held them.
            // qutebrowser refuses a count here (`commands.py:851`): a window's undo entry is the
            // whole window, so there is nothing for a depth to mean.
            if *window {
                let urls = state
                    .lock()
                    .expect("state mutex poisoned")
                    .take_closed_window();
                let Some(urls) = urls else {
                    crate::message::error("undo: no closed window to reopen");
                    return;
                };
                let mut urls = urls.into_iter();
                let Some(first) = urls.next() else {
                    return;
                };
                let reopened = crate::window::open(state, &first);
                // The rest in the background, or each one would steal the selection from the last.
                for url in urls {
                    crate::tabs::new_tab_in(state, reopened, &url, true);
                }
                return;
            }
// --- end src/window.rs -----------------------------------------------------
            let url = state
                .lock()
                .expect("state mutex poisoned")
                .take_closed_tab(repeat as usize);
            if let Some(url) = url.filter(|url| !url.is_empty()) {
                crate::tabs::new_tab(state, &url, false);
            }
        }

// --- src/session.rs --------------------------------------------------------
        Command::TabPin => crate::tabs::toggle_pin(state),
        Command::TabMute => crate::tabs::toggle_mute(state),
// --- src/window.rs ---------------------------------------------------------
        // `gD` — the showing tab moves, whole, to another window. A count overrides the argument
        // and is one-based (`commands.py:475`), so `2gD` gives to window 1 and `gD` alone detaches
        // into a new one.
        //
        // The tab is re-parented rather than recreated: the same `BrowserView`, the same browser,
        // the same navigation history, so `H` still works in the window it arrived in. See
        // `tabs::give_tab` for why holding the reference across the move is the whole trick.
        Command::TabGive { win_id } => {
            let target = count.map(|c| c.saturating_sub(1)).or(*win_id);
            crate::tabs::give_tab(state, target);
        }
// --- end src/window.rs -----------------------------------------------------

        Command::SessionSave { name, .. } => {
            let name = name.as_deref().unwrap_or(crate::session::DEFAULT_NAME);
            match crate::session::save(state, name) {
                Ok(path) => eprintln!("bru: saved session to {}", path.display()),
                Err(e) => eprintln!("bru: could not save session {name:?}: {e}"),
            }
        }
        Command::SessionLoad { name, clear, history } => {
            match crate::session::load(state, name, *clear, *history) {
                Ok(opened) => eprintln!("bru: loaded {opened} tabs from session {name:?}"),
                Err(e) => eprintln!("bru: could not load session {name:?}: {e}"),
            }
        }
        Command::SessionDelete { name } => match crate::session::delete(name) {
            Ok(path) => eprintln!("bru: deleted {}", path.display()),
            Err(e) => eprintln!("bru: could not delete session {name:?}: {e}"),
        },
// --- end src/session.rs ----------------------------------------------------

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
// --- src/clip.rs -----------------------------------------------------------
            // `pp`, `pP`, `Pp`, `PP`, `wp`, `wP` are `open -- {clipboard}` / `{primary}`, and this
            // is where the substitution happens — at run time, not at parse time. qutebrowser
            // substitutes in `commands/runners.py::replace_variables`, between parsing a command
            // and running it; bru parses its bindings once at startup, so doing it there would
            // paste whatever was on the clipboard when bru launched. An empty selection aborts the
            // command, as `ClipboardError` does in qutebrowser: opening the literal `{clipboard}`
            // would search for it.
            let url = match crate::clip::expand(url.as_deref()) {
                Ok(url) => url,
                Err(error) => {
                    crate::clip::message(error);
                    return;
                }
            };
// --- end src/clip.rs -------------------------------------------------------
// --- src/window.rs ---------------------------------------------------------
            // `wo`, `wO`, `wp`, `wP` — a window of its own. The address is decided by the same
            // function `open` uses, so `:open -w ddg rust` searches exactly as `:open ddg rust`
            // does; a window is *created around* a URL, so there is no browser to hand over.
            if *window {
                if let Some(target) = crate::open::resolve(url.as_deref()) {
                    crate::window::open(state, &target);
                }
                return;
            }
// --- end src/window.rs -----------------------------------------------------
            crate::open::open(state, browser, url.as_deref(), *tab, *bg)
        }

        // --- navigation ---------------------------------------------------------------------
        Command::Back { tab, bg, window } => {
            back_forward(state, browser, false, repeat, *tab || *bg, *bg, *window)
        }
        Command::Forward { tab, bg, window } => {
            back_forward(state, browser, true, repeat, *tab || *bg, *bg, *window)
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
            // --- unhardcoded ---------------------------------------------------------------
            set_zoom_percent(browser, count.or(*level).unwrap_or_else(zoom_default));
            // --- end unhardcoded -----------------------------------------------------------
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
        // Closing a window is `can_close` → `do_close` → `on_before_close`, the path
        // `--close-after-ms` already exercises; `quit_message_loop` comes at the end of the *last*
        // one, because `BruState::on_before_close` counts browsers across every window.
        //
        // So `close` and `quit` are two different things now, which is what their qutebrowser
        // spellings always meant: `<Ctrl-Shift-W>` closes the window in front of you and
        // `<Ctrl-Q>`/`ZZ`/`ZQ` end the application.
        Command::Quit { save } => {
            // `:quit --save` (qutebrowser's `wq` alias) writes the open tabs before the window
            // goes. It has to happen here rather than in `on_before_close`: by the time that runs
            // the browsers are being torn down and their navigation lists no longer read back.
            if *save {
                match crate::session::save(state, crate::session::DEFAULT_NAME) {
                    Ok(path) => eprintln!("bru: saved session to {}", path.display()),
                    Err(e) => eprintln!("bru: could not save the session: {e}"),
                }
            }
            crate::window::close_all(state);
        }
        Command::Close => close_window(state),

        // --- modes --------------------------------------------------------------------------
        Command::ModeEnter(mode) => {
            // --- src/caret.rs ---------------------------------------------------------------
            // The mode that is being left, read before the transition. Caret mode has a page half
            // to set up and to tear down, and `mode-enter normal` (caret's `c`) is spelled as a
            // *leave*, so both directions are visible only from the pair.
            let before = state.lock().expect("state mutex poisoned").mode();
            // --- end src/caret.rs -----------------------------------------------------------
            let entered = state
                .lock()
                .expect("state mutex poisoned")
                .enter_mode(*mode, false);
            if entered {
                crate::ipc::set_mode(mode.name().to_string());
                // --- src/caret.rs -----------------------------------------------------------
                let now = state.lock().expect("state mutex poisoned").mode();
                crate::caret::on_mode_change(state, browser, before, now);
                // --- end src/caret.rs -------------------------------------------------------
            }
        }
        Command::ModeLeave => {
            let mut guard = state.lock().expect("state mutex poisoned");
            // --- src/caret.rs ---------------------------------------------------------------
            let before = guard.mode();
            // --- end src/caret.rs -----------------------------------------------------------
            if guard.leave_mode() {
                let now = guard.mode();
                drop(guard);
                crate::ipc::set_mode(now.name().to_string());
                // --- src/caret.rs -----------------------------------------------------------
                // `state` as well as `browser`, so caret mode can name the window this browser is
                // in: its session belongs to one window now, the way the mode already does.
                crate::caret::on_mode_change(state, browser, before, now);
                // --- end src/caret.rs -------------------------------------------------------
                // Leaving insert mode should also give the page's text field up, or the next `j`
                // is typed into it rather than scrolling.
                blur(browser);
            }
        }

// --- src/hints.rs -----------------------------------------------------------------------
        // `f`, `F` and the fourteen `;` bindings. The labels are drawn by injected JS, but every
        // keystroke that follows is matched in Rust by a `BindingTrie<usize>` in hint mode —
        // nothing typed reaches the page.
        Command::Hint { group, target, rapid, first } => {
            use crate::commands::{HintGroup, HintTarget};
            let group = match group {
                HintGroup::All => crate::hints::Group::All,
                HintGroup::Links => crate::hints::Group::Links,
                HintGroup::Images => crate::hints::Group::Images,
                HintGroup::Media => crate::hints::Group::Media,
                HintGroup::Url => crate::hints::Group::Url,
                HintGroup::Inputs => crate::hints::Group::Inputs,
            };
            let target = match target {
                HintTarget::Normal => crate::hints::Target::Normal,
                HintTarget::TabBg => crate::hints::Target::TabBg,
                HintTarget::TabFg => crate::hints::Target::TabFg,
                HintTarget::Window => crate::hints::Target::Window,
                HintTarget::Hover => crate::hints::Target::Hover,
                HintTarget::Yank => crate::hints::Target::Yank,
                HintTarget::YankPrimary => crate::hints::Target::YankPrimary,
                HintTarget::Download => crate::hints::Target::Download,
                HintTarget::Fill(text) => crate::hints::Target::Fill(text.clone()),
            };
            crate::hints::start(state, browser, group, target, *rapid, *first);
        }
// --- end src/hints.rs -------------------------------------------------------------------
// --- unhardcoded ---------------------------------------------------------------------------
        // **`<Return>` in hint mode has a job now, and the thing that gave it one is a setting.**
        //
        // This arm held forty lines saying the key could never do anything, and the fourth of the
        // four measurements ended: "What would give the key a job is `hints.auto_follow = never`,
        // and bru does not have the option: DESIGN.md gives it no configuration of its own … If
        // that option is ever added, this arm and `hints::auto_follow` are the two places to
        // change, and the row stops being refused." The premise was the reading of DESIGN.md the
        // user corrected on 2026-08-06 — bru holds every setting and its default — and the option
        // is `hints.auto_follow`, one of the fourteen values unhardcoded this round. So this is
        // that change, at the two places that comment named.
        //
        // Nothing about the other three values moved: under `always`, `unique-match` and
        // `full-match` a label that could be followed has been followed before the key arrives, and
        // `hints::follow_current` says so rather than doing nothing. The four measurements that
        // establish that are not deleted, they are `hints::auto_follow`'s documentation and the two
        // tests it names.
        Command::HintFollow => crate::hints::follow_current(state, browser),
// --- end unhardcoded -----------------------------------------------------------------------

// --- src/downloads.rs --------------------------------------------------------------------------
        // `gd`, `ad`, `cd` and the four `:download-*` commands. The count means the same thing in
        // all of them — which download, 1-based, with none meaning the last — so it is passed
        // through rather than turned into a repeat: `2ad` cancels download 2, it does not cancel
        // twice.
        Command::Download { url } => crate::downloads::start(state, browser, url.as_deref()),
        // Not a download: `Page.captureSnapshot` over the DevTools protocol, written by bru. It
        // still lands in the same list and the same bar section — see the head of downloads.rs.
        Command::DownloadMhtml => crate::downloads::start_mhtml(browser),
        Command::DownloadCancel { all } => crate::downloads::cancel(count, *all),
        Command::DownloadClear => crate::downloads::clear(),
        Command::DownloadOpen { cmdline, dir } => {
            crate::downloads::open_file(count, cmdline.as_deref(), *dir)
        }
        Command::DownloadDelete => crate::downloads::delete(count),
        Command::DownloadRetry => crate::downloads::retry(state, browser, count),
// --- end src/downloads.rs ----------------------------------------------------------------------

// --- src/clip.rs -----------------------------------------------------------
        // `yy`, `yY`, `yt`, `yT`, `yd`, `yD`, `yp`, `yP`, `ym`, `yM`. `wl-copy` runs here, on the
        // UI thread, on this same turn — measured at 1.2 ms, and this is not the scroll path.
        Command::Yank { what, sel } => crate::clip::yank(what, *sel),
// --- end src/clip.rs -------------------------------------------------------

// --- src/find.rs + src/navigate.rs ---------------------------------------------------------------
        // `/`, `?`, `n`, `N`. The text is remembered in `find.rs` so `n` knows what to repeat, and
        // the direction with it: `?foo` then `n` goes up. A count is a repeat of the step, which is
        // what `3n` means.
        Command::Search { text, reverse } => crate::find::search(browser, text, *reverse),
        Command::SearchNext => crate::find::search_next(browser, count),
        Command::SearchPrev => crate::find::search_prev(browser, count),

        // `[[`, `]]`, `{{`, `}}`, `gu`, `gU`, `<Ctrl-A>`, `<Ctrl-X>`. `-w` is its own destination
        // now — `wu` is a window, not the tab it folded into.
        Command::Navigate { to, tab, bg, window } => {
            crate::navigate::navigate(state, browser, *to, *tab, *bg, *window, count)
        }
// --- end src/find.rs + src/navigate.rs ------------------------------------------------------------

        // --- src/caret.rs -------------------------------------------------------------------
        // Caret mode's movements and its selection. `v` moves a text cursor through the page's
        // document, which CEF has no notion of, so the move itself is `Selection.modify` inside the
        // page — but *what* to modify, how far, and what a line selection re-anchors to are all
        // decided in `caret.rs` and sent as a list of primitives. `selection-follow` is normal
        // mode's `<Return>` and needs no caret session.
        Command::SelectionToggle { line } => crate::caret::selection_toggle(state, browser, *line),
        Command::SelectionDrop => crate::caret::selection_drop(state, browser),
        Command::SelectionReverse => crate::caret::selection_reverse(state, browser),
        Command::SelectionFollow { tab } => crate::caret::selection_follow(state, browser, *tab),
        Command::MoveTo(kind) => crate::caret::move_to(state, browser, *kind, count),
        // --- end src/caret.rs ---------------------------------------------------------------

        // Generated from the live binding table on every request — see src/help.rs.
        Command::Help { tab } => {
            crate::open::open(state, browser, Some("bru://chrome/help"), *tab, false)
        }

// --- src/history.rs --------------------------------------------------------
        // Quickmarks, bookmarks and the history page. Every one of these is one call: the argument
        // handling and the "which page am I on" question live in `history.rs`, so that this block
        // stays small enough to merge beside eleven others.
        Command::QuickmarkSave { name } => {
            crate::history::quickmark_save(state, name.as_deref())
        }
        Command::QuickmarkLoad { name, tab, bg, window } => {
            crate::history::quickmark_load(state, browser, name.as_deref(), *tab, *bg, *window)
        }
        Command::QuickmarkDel { name } => crate::history::quickmark_del(state, name.as_deref()),
        Command::BookmarkAdd { url, title, toggle } => {
            crate::history::bookmark_add(state, url.as_deref(), title.as_deref(), *toggle)
        }
        Command::BookmarkLoad { url, tab, bg, window, delete } => crate::history::bookmark_load(
            state,
            browser,
            url.as_deref(),
            *tab,
            *bg,
            *window,
            *delete,
        ),
        Command::BookmarkDel { url } => crate::history::bookmark_del(state, url.as_deref()),
        Command::BookmarkList { jump, bg } => {
            crate::history::bookmark_list(state, browser, *jump, *bg)
        }
        Command::History { bg } => crate::history::show(state, browser, *bg),
// --- end src/history.rs ----------------------------------------------------

// --- src/cookies.rs --------------------------------------------------------
        Command::Cookies { filter, bg } => {
            crate::cookies::show(state, browser, filter.as_deref(), *bg)
        }
// --- end src/cookies.rs ----------------------------------------------------

// --- src/settings.rs -------------------------------------------------------
        // `:set` and the 24 `config-cycle` bindings. The `-u` pattern still holds `{url:host}` at
        // this point — `commands::parse` ran at startup, when there was no page to ask — so it is
        // expanded inside `settings.rs` against the tab that is showing.
        //
        // Neither reloads: the bindings are `config-cycle … ;; reload`, and the chain arm above
        // runs the second half. `:set content.javascript.enabled false` on its own leaves the page
        // as it is, which is qutebrowser's behaviour too.
        Command::Set { option, value, pattern, print } => {
            crate::settings::run_set(option.as_deref(), value.as_deref(), pattern.as_deref(), *print)
        }
        Command::ConfigCycle { option, values, pattern, print } => {
            crate::settings::run_cycle(option, values, pattern.as_deref(), *print)
        }
        // The two dictionary commands. No default binding names either — they are typed — so
        // neither changes the live-binding count; what they change is that a dict setting can be
        // edited at all while bru runs, which merging alone cannot do for a removal.
        Command::ConfigDictAdd { option, key, value, replace, print } => {
            crate::settings::run_dict_add(option, key, value, *replace, *print)
        }
        Command::ConfigDictRemove { option, key, print } => {
            crate::settings::run_dict_remove(option, key, *print)
        }
// --- end src/settings.rs ---------------------------------------------------

// --- config commands ---------------------------------------------------------------------------
        // The list twins of the two dict commands, against bru's one list setting.
        Command::ConfigListAdd { option, value, print } => {
            crate::settings::run_list_add(option, value, *print)
        }
        Command::ConfigListRemove { option, value, print } => {
            crate::settings::run_list_remove(option, value, *print)
        }
        // Back to bru's own default — which exists, now that bru ships every setting's.
        Command::ConfigUnset { option, pattern } => {
            crate::settings::run_unset(option, pattern.as_deref())
        }
        Command::ConfigClear { save } => crate::settings::run_clear(*save),
        // The settings half and the bindings half, printed together: two commands' worth of state
        // in one answer, because "what have I changed" is one question.
        Command::ConfigDiff => {
            let mut lines = crate::settings::diff_lines();
            lines.extend(crate::config::binding_diff());
            if lines.is_empty() {
                crate::message::info("config-diff: nothing is customized");
            } else {
                let count = lines.len();
                for line in &lines {
                    eprintln!("bru: {line}");
                }
                crate::message::info(&format!(
                    "config-diff: {count} line(s) on stderr — the Lua that would reproduce this"
                ));
            }
        }
        // A second Lua state, at runtime. `config::source_over` carries the argument for why that
        // respects "Lua is never on the key path" rather than breaking it.
        Command::ConfigSource { filename, clear } => {
            crate::config::run_source(filename.as_deref(), *clear)
        }
        Command::ConfigEdit { no_source } => crate::config::run_edit(*no_source),
        // **Refused**, with the reason, rather than left to say "not implemented yet" — see
        // `Command::ConfigWritePy`, which carries it. It acts: it explains, and names the command
        // that does the half bru can do.
        Command::ConfigWritePy => crate::message::error(
            "config-write-py: bru never writes ~/.config/bru/config.lua — that file is \
             hand-written and belongs to configer, and bru holds only the defaults it is layered \
             on. `:config-diff` prints the Lua this browser would need, for you to paste there.",
        ),
        // `:bind` with no key is the page listing every binding — `qute://bindings` in qutebrowser,
        // and here the page bru already generates from the table it is running on. It is done in
        // this file because navigating is this file's job; the other three shapes are `config.rs`'s.
        Command::Bind { mode, keys, command, default } => match keys {
            None => crate::open::open(state, browser, Some("bru://chrome/help"), true, false),
            Some(keys) => {
                crate::config::run_bind(mode, Some(keys.as_str()), command.as_deref(), *default)
            }
        },
        Command::Unbind { mode, keys } => crate::config::run_unbind(mode, keys),
// --- end config commands -----------------------------------------------------------------------

        // --- the command line ---------------------------------------------------------------
        // Unreachable: `cmdline::run_command` at the top of this function claims both. The arms
        // stay because the match has no `_`, and they document where the two actually go.
        Command::CmdSetText { .. } | Command::CommandAccept { .. } => {}

        // --- src/spawn.rs, src/editor.rs ----------------------------------------------------
        // The one place in bru that runs another program. Everything about *when* that is allowed
        // is in `spawn.rs`'s module docs; what matters here is that a `Command::Spawn` can only be
        // built by `commands::parse`, and the three things that call it are a binding, the command
        // line, and a line a running userscript wrote back. A page reaches none of them.
        Command::Spawn { cmdline, userscript, detach, messages, verbose } => {
            crate::spawn::spawn(
                // The browser, because a `--userscript` has to ask the page what it has selected
                // before it can build `BRU_SELECTED_TEXT`. It is the tab the key was aimed at, not
                // `active_browser()`, so a command dispatched at a background window reads that
                // window's page.
                browser,
                cmdline,
                crate::spawn::Opts {
                    userscript: *userscript,
                    detach: *detach,
                    output_messages: *messages,
                    verbose: *verbose,
                },
                count,
            )
        }
        Command::EditText => crate::editor::edit_text(browser),
        Command::InsertText { text } => crate::editor::insert_text(browser, text),
        Command::FakeKey { keystring } => crate::editor::fake_key(browser, keystring),
        // --- end src/spawn.rs, src/editor.rs ------------------------------------------------

// --- src/completers.rs ---------------------------------------------------------------------
        // The completion's own three, and they need no browser: the table is built from the
        // command line and from `src/data.rs`, and what they change is the command line.
        Command::CompletionItemFocus { .. }
        | Command::CompletionItemDel
        | Command::CompletionItemYank { .. } => {
            crate::completers::run_command(command);
        }
// --- end src/completers.rs -----------------------------------------------------------------

// --- src/devtools.rs, src/message.rs (the polish workstream) -------------------------------------
        // Chromium's own `view-source:` scheme, in a tab of its own — which is where qutebrowser's
        // `gf` puts it too ("Show the source of the current page in a new tab",
        // `commands.py:1423`).
        //
        // qutebrowser refuses to view the source of a source view (`commands.py:1440`); bru cannot,
        // and says so here rather than pretending. **A `view-source:` tab does not report itself as
        // one.** Measured 2026-08-06 on a tab showing the source of
        // `http://127.0.0.1:18443/msg.html`: `on_address_change` reported that inner address, and
        // so did `main_frame().url()` — the only place the prefix appeared was the *title*, which
        // the toplevel duly wore as `view-source:127.0.0.1:18443/msg.html - bru`. qutebrowser's
        // check works because the flag is its own (`tab.data.viewing_source`), and nothing bru can
        // ask CEF is equivalent. So `gf` on a source view opens the same source again in another
        // tab: a duplicate, never a nesting, because the URL it builds from is the inner one.
        Command::ViewSource => match active_tab_url(state) {
            Some(url) => crate::tabs::new_tab(state, &format!("view-source:{url}"), false),
            None => crate::message::error("no page to view the source of"),
        },
        Command::Print => {
            if let Some(host) = browser.host() {
                host.print();
            }
        }
        Command::DevTools => crate::devtools::toggle(browser),
        Command::DevToolsFocus => crate::devtools::focus(browser),
        // Through the three named entry points rather than through `show`, because those are what
        // every other workstream will call — `message::info("yanked")` reads as what it does.
        Command::Message { level, text } => match level {
            crate::message::Level::Info => crate::message::info(text),
            crate::message::Level::Warning => crate::message::warning(text),
            crate::message::Level::Error => crate::message::error(text),
        },
// --- end src/devtools.rs, src/message.rs ---------------------------------------------------------

// --- src/settingspage.rs -------------------------------------------------------------------
        // `sf`. Not the page — see `Command::Save`: qutebrowser's `:save` walks its saveables, and
        // the one bru has that is not already on disk is the command line's history.
        Command::Save { what } => crate::cmdline::save(what),

        // `.`. The count on the `.` beats the one the command was first given, which is
        // `utilcmds.py:201` — `count if count is not None else cmd[1]`, so `3.` repeats a bare `j`
        // three times and a plain `.` repeats `10j` as ten.
        //
        // This reaches `run` again, one level down, and that recursion is safe for the same reason
        // a chain's is: `cmd-repeat-last` is never itself recorded (`runners.py:177-179`), so the
        // command found here can never be another repeat.
        Command::CmdRepeatLast => match crate::cmdline::last_command() {
            Some((last, last_count)) => run(state, browser, &last, count.or(last_count)),
            // qutebrowser's own words, `utilcmds.py:198`.
            None => crate::message::error("You didn't do anything yet."),
        },

        // `Ss`, and a bare `:set`. Same tab, as `configcommands.py:97` has it (`newtab=False`).
        //
        // The reading has to be taken *here*, before the navigation: this is the UI thread, which
        // is the only thread `RequestContext::get_content_setting` may be called on, and the page
        // is built on the IO thread where the same call silently answers "default" for everything.
        // Measured — see `settingspage`'s module docs.
        Command::SettingsPage => {
            crate::settingspage::refresh();
            crate::open::open(state, browser, Some("bru://chrome/settings"), false, false)
        }
// --- end src/settingspage.rs ---------------------------------------------------------------

// --- src/utilcmds.rs -------------------------------------------------------
        // The twenty-two commands qutebrowser has that had no arm here. None of them is bound by
        // qutebrowser's defaults, so none raises the live-binding count — what `gt` prefills now
        // does something, which is a different thing from a key becoming live.
        Command::TabSelect { index } => {
            crate::utilcmds::tab_select(state, index.as_deref(), count)
        }
        Command::TabTake { index, keep } => {
            crate::utilcmds::tab_take(state, browser, index, *keep)
        }
        Command::WindowOnly => crate::utilcmds::window_only(state),

        Command::Screenshot { filename, rect, force } => {
            crate::utilcmds::screenshot(browser, filename, rect.as_deref(), *force)
        }
        // --- src/chrome.rs: themes -------------------------------------------------------------
        Command::Colorscheme { name, reload } if *reload => {
            // **Re-read `~/.config/bru/theme.css` without changing anything.**
            //
            // `chrome.rs` serves the stylesheet by reading that file on every request, so a theme
            // written while bru is running is already the answer to the next fetch — and nothing
            // ever asks for one, because a chrome document fetches its stylesheets when it loads.
            // `themer` writes exactly that path, which is why this flag exists: it is the one thing
            // between "the file on disk changed" and "the browser is wearing it".
            //
            // **It is also automatic now**, and this flag stayed rather than being taken out with
            // the argument that justified it. `chrome::watch_theme_file` looks at the file's
            // modification time every two seconds and re-reads it when it moves, which is what
            // makes `themer` work with no second operation in its target. This is the same thing
            // asked for by hand: for the two seconds before the tick, and for a file whose mtime a
            // copy did not move.
            let _ = name;
            crate::chrome::warn_if_incomplete();
            crate::ipc::reapply_theme_everywhere();
            crate::message::info("re-read ~/.config/bru/theme.css");
        }
        Command::Colorscheme { name, .. } => match name {
            Some(name) => {
                // Through `:set`'s own path rather than beside it: `colors.scheme` is a setting
                // like any other, and a second way to write it is a second way to forget the
                // reload and the completeness check that `Backing::Chrome` does.
                crate::settings::run_set(Some("colors.scheme"), Some(name), None, false);
            }
            None => {
                let names = crate::chrome::theme_names();
                let live = crate::settings::text_of("colors.scheme").unwrap_or_default();
                if names.is_empty() {
                    crate::message::info(
                        "no themes in ~/.config/bru/themes/ — a theme is one .css file there, and \
                         bru://chrome/theme-default.css is the list of properties it has to set",
                    );
                } else {
                    let listed: Vec<String> = names
                        .iter()
                        .map(|name| {
                            if *name == live { format!("[{name}]") } else { name.clone() }
                        })
                        .collect();
                    crate::message::info(&format!("themes: {}", listed.join(" ")));
                }
            }
        },
        // --- end src/chrome.rs: themes ---------------------------------------------------------

        // `--file` and `--url` are read here rather than at parse time: a binding naming a file
        // should not fail to load because the file is missing at startup.
        Command::JsEval { code, file, url, quiet } => {
            let code = match (file, url) {
                (true, _) => crate::utilcmds::jseval_file(code),
                (_, true) => crate::utilcmds::jseval_url(code),
                _ => Ok(code.clone()),
            };
            match code {
                Ok(code) => crate::utilcmds::jseval(browser, &code, *quiet),
                Err(problem) => crate::message::error(&format!("jseval: {problem}")),
            }
        }

        Command::EditUrl { url, tab, bg, window } => {
            crate::utilcmds::edit_url(state, url.as_deref(), *tab, *bg, *window)
        }
        Command::EditCommand { run } => crate::utilcmds::edit_command(state, *run),

        Command::QuickmarkAdd { url, name } => crate::utilcmds::quickmark_add(url, name),
        Command::HistoryClear { force } => crate::utilcmds::history_clear(state, *force),
        Command::MarksReload { which } => crate::utilcmds::reload_marks(*which),

        // The three that run other commands. `later` posts; the other two recurse straight away,
        // and that recursion is bounded for the same reason a chain's is — the carried command was
        // parsed once, at parse time, and cannot grow another level while it runs.
        Command::Later { ms, command } => crate::utilcmds::later(*ms, command, count),
        // "count: Multiplies with 'times' when given" (`utilcmds.py:64-79`). The ceiling is bru's:
        // `:repeat 99999 scroll down` would hold the UI thread for the same reason `99999j` would,
        // which is what `MAX_COUNT` is already here for.
        Command::Repeat { times, command } => {
            let times = (*times as u64 * count.unwrap_or(1) as u64).min(MAX_COUNT as u64);
            for _ in 0..times {
                run(state, browser, command, None);
            }
        }
        // "If cmd_run_with_count itself is run with a count, it multiplies count_arg"
        // (`utilcmds.py:86-97`).
        Command::RunWithCount { count: given, command } => {
            let given = (*given as u64 * count.unwrap_or(1) as u64).min(MAX_COUNT as u64);
            run(state, browser, command, Some(given as u32));
        }

        Command::Restart => crate::utilcmds::restart(state),
        Command::Version => crate::utilcmds::version(state, browser),
        Command::Messages { level, plain, tab, bg, window } => {
            crate::utilcmds::messages(state, browser, level, *plain, *tab, *bg, *window)
        }
        Command::Process { pid, action } => {
            crate::utilcmds::process(state, browser, *pid, *action)
        }

        Command::ClickElement { filter, value, target, force_event, select_first } => {
            crate::utilcmds::click_element(
                state,
                browser,
                *filter,
                value.as_deref(),
                *target,
                *force_event,
                *select_first,
            )
        }
        // A navigation to a fragment, which is what an anchor is — see the function, and note that
        // it therefore does not go near `send_mouse_wheel_event` or `window.scrollBy`.
        Command::ScrollToAnchor { name } => crate::utilcmds::scroll_to_anchor(browser, name),

        // The count names which download, as it does for the other seven.
        Command::DownloadRemove { all } => crate::downloads::remove(count, *all),
        Command::ClearMessages => crate::message::clear(),
// --- end src/utilcmds.rs ---------------------------------------------------

        // Nothing to do, and that is the point: `nop` exists to shadow a Chromium default, and
        // clear-keychain is already done by the parser reporting the key.
        Command::Nop | Command::ClearKeychain => {}

// --- src/macros.rs -------------------------------------------------------------------------------
        // `q` and `@`. Neither acts on the browser: one starts or stops a recording, the other
        // replays one — and a replay is `run` again, once per recorded step, which is why this arm
        // hands `browser` straight on.
        Command::MacroRecord { register } => crate::macros::macro_record(state, *register),
        Command::MacroRun { register } => {
            crate::macros::macro_run(state, browser, *register, count)
        }
// --- end src/macros.rs ---------------------------------------------------------------------------

// --- adblock ---------------------------------------------------------------------------------
        // None of the three is bound to a key, in qutebrowser or here: they are typed, rarely, and
        // `:adblock-update` in particular is the one thing in bru that reaches the network of its
        // own accord — it should take a decision, not a keystroke.
        Command::AdblockUpdate => crate::adblock::update(),
        // --- src/userstyles.rs -------------------------------------------------------------------
        Command::StylesToggle => {
            // Through `:set`'s own path rather than beside it: `Backing::UserStyles` is what
            // re-runs the injection in every tab, and a second way to write the value is a second
            // way to forget that.
            let on = crate::settings::is_on("content.user_styles");
            crate::settings::run_set(
                Some("content.user_styles"),
                Some(if on { "false" } else { "true" }),
                None,
                false,
            );
            crate::message::info(if on {
                "per-site styles off"
            } else {
                "per-site styles on"
            });
        }
        // --- end src/userstyles.rs ---------------------------------------------------------------
        Command::AdblockToggle => {
            let on = crate::adblock::toggle();
            eprintln!("bru[adblock]: blocking {}", if on { "on" } else { "off" });
        }
        Command::AdblockInfo => {
            eprintln!("bru[adblock]: {}", crate::adblock::info(browser.identifier()));
        }
// --- end adblock -----------------------------------------------------------------------------

// --- src/greasemonkey.rs -----------------------------------------------------------------------
        // Not bound to a key here or in qutebrowser: it is typed, after a script has been edited.
        Command::GreasemonkeyReload { quiet } => crate::greasemonkey::reload(*quiet),
// --- end src/greasemonkey.rs -------------------------------------------------------------------

// --- lua runtime -------------------------------------------------------------------------------
        // **One arm for every plugin command there will ever be**, which is the whole shape of the
        // registry: `commands.rs` turns a registered name into this, and `plugins::run` does the
        // lookup, the call and the error isolation. A handler that throws prints once and the
        // browser goes on; three throws in one session switch the plugin off.
        Command::Plugin { name, args } => crate::plugins::run(name, args),
        Command::PluginList => crate::plugins::list(),
        Command::PluginReload { name } => crate::plugins::reload(name.as_deref()),
        Command::PluginDisable { name } => crate::plugins::disable(name),
// --- end lua runtime ---------------------------------------------------------------------------

// --- src/prompt.rs -------------------------------------------------------------------------
        // The five `prompt-*` commands, all of which act on the question open in the window the
        // key was pressed in. They take no browser: a prompt belongs to a window, and the browser
        // that raised it may be a background tab.
        Command::PromptAccept { .. }
        | Command::PromptItemFocus { .. }
        | Command::PromptOpenDownload { .. }
        | Command::PromptYank { .. }
        | Command::PromptFileselectExternal => {
            crate::prompt::run_command(command);
        }
// --- end src/prompt.rs ---------------------------------------------------------------------

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

        // --- src/chrome.rs: themes -------------------------------------------------------------
        Command::Colorscheme { .. } => true,
        Command::StylesToggle => true,
        // --- end src/chrome.rs: themes ---------------------------------------------------------

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
// --- src/window.rs ---------------------------------------------------------
        // Both spellings act now. `undo -w` reopens the last closed window with its tabs; it was
        // the one spelling that did nothing, because there was one window and it outlived them.
        Command::Undo { .. } => true,
// --- end src/window.rs -----------------------------------------------------

// --- src/session.rs --------------------------------------------------------
        Command::TabPin | Command::TabMute => true,
// --- src/window.rs ---------------------------------------------------------
        // `gD` moves the showing tab to another window, or detaches it into a new one. It was inert
        // because there was nowhere to give a tab to.
        Command::TabGive { .. } => true,
// --- end src/window.rs -----------------------------------------------------
        Command::SessionSave { .. } | Command::SessionLoad { .. } | Command::SessionDelete { .. } => {
            true
        }
// --- end src/session.rs ----------------------------------------------------

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

        // Claimed by `cmdline.rs` before this match ever runs.
        Command::CmdSetText { .. } | Command::CommandAccept { .. } => true,

// --- src/downloads.rs --------------------------------------------------------------------------
        // All seven act. `gd`, `ad` and `cd` are the three default bindings this turns on; the
        // other four are `:` commands qutebrowser binds to nothing either — `download --mhtml`
        // included, which is why making it live raises no binding count.
        Command::Download { .. }
        | Command::DownloadMhtml
        | Command::DownloadCancel { .. }
        | Command::DownloadClear
        | Command::DownloadOpen { .. }
        | Command::DownloadDelete
        | Command::DownloadRetry => true,
// --- end src/downloads.rs ----------------------------------------------------------------------

// --- src/settings.rs -------------------------------------------------------
        // `config-cycle` is only built for a setting bru implements — `content.plugins` and
        // `content.cookies.accept` parse to `Unimplemented` and are counted with the rest of what
        // does nothing, which is what keeps the twelve refused bindings honest.
        Command::ConfigCycle { .. } => true,
        // `:set` is live for any option, because naming one bru refuses is answered with the
        // reason. Nothing is bound to `set <option>`, so this adds no binding to the count; the
        // one binding that is bare `set` (`Ss`) parses to `Unimplemented` and stays inert.
        Command::Set { option, .. } => option.is_some(),
        // Live, and bound to nothing — see the note on `Command::ConfigDictAdd`. The 264-row
        // default table names neither, so the live-binding count is untouched by both.
        Command::ConfigDictAdd { .. } | Command::ConfigDictRemove { .. } => true,
// --- end src/settings.rs ---------------------------------------------------

// --- config commands ---------------------------------------------------------------------------
        // All ten act, and none of them is bound by default, so the live-binding count is
        // untouched by the whole group — checked in `the_config_commands_raise_no_binding`.
        //
        // `config-write-py` is live and **refused** at the same time, which no other command in bru
        // is, and it is deliberate: `exec::refusal` is for a *binding* that will never act, and
        // nothing binds this. What it does when typed is explain itself, which is doing something.
        // Claiming it inert would send it to the dispatcher's "not implemented yet", which is the
        // one answer that is false.
        Command::ConfigUnset { .. }
        | Command::ConfigClear { .. }
        | Command::ConfigDiff
        | Command::ConfigListAdd { .. }
        | Command::ConfigListRemove { .. }
        | Command::ConfigSource { .. }
        | Command::ConfigEdit { .. }
        | Command::ConfigWritePy
        | Command::Bind { .. }
        | Command::Unbind { .. } => true,
// --- end config commands -----------------------------------------------------------------------

// --- src/completers.rs ---------------------------------------------------------------------
        Command::CompletionItemFocus { .. } | Command::CompletionItemDel => true,
        // Bound and reachable, and it says what it would have copied — but there is no clipboard
        // in bru yet, so claiming it as live would be claiming `<Ctrl-C>` copies something.
        // `clip::yank_plain` is installed at startup, so `<Ctrl-C>` copies the selected cell.
        Command::CompletionItemYank { .. } => true,
// --- end src/completers.rs -----------------------------------------------------------------

// --- src/hints.rs -----------------------------------------------------------------------
        // Every target acts. Two combinations do not, and they are the two `hints.py:1027` names
        // in `no_rapid_targets`: `--rapid` with `tab-fg` opens a tab and switches to it, so the
        // second label would be typed at the tab that was just left, and `--rapid` with `fill`
        // leaves hint mode by entering command mode. `window` was a third for as long as it
        // opened a foreground tab; it opens a window now, so `;R` is live — see `hints.rs`.
        Command::Hint { target, rapid, .. } => {
            use crate::commands::HintTarget;
            match target {
                // Introduced to `clip.rs` and `downloads.rs` at startup — `app.rs` installs both.
                HintTarget::Yank | HintTarget::YankPrimary | HintTarget::Download => true,
                HintTarget::TabFg | HintTarget::Fill(_) => !rapid,
                _ => true,
            }
        }
// --- end src/hints.rs -------------------------------------------------------------------
        Command::Help { .. } => true,

// --- src/history.rs --------------------------------------------------------
        // All eight act. `quickmark-save` with no name acts by opening the command line rather than
        // by saving, which is still something happening when the key is pressed — see the arm.
        Command::QuickmarkSave { .. }
        | Command::QuickmarkLoad { .. }
        | Command::QuickmarkDel { .. }
        | Command::BookmarkAdd { .. }
        | Command::BookmarkLoad { .. }
        | Command::BookmarkDel { .. }
        | Command::BookmarkList { .. }
        | Command::History { .. } => true,
// --- end src/history.rs ----------------------------------------------------

// --- src/cookies.rs --------------------------------------------------------
        // No default binding names it — qutebrowser has no cookie command, and DESIGN.md keeps the
        // key table 1:1 with qutebrowser's — so this raises no binding count. It is live all the
        // same: `:cookies` opens the page and the page deletes.
        Command::Cookies { .. } => true,
// --- end src/cookies.rs ----------------------------------------------------

// --- src/clip.rs -----------------------------------------------------------
        // All five spellings reach `wl-copy`. `open -- {clipboard}` was already counted live
        // before this milestone — it opened the literal string `{clipboard}` as a search — so the
        // six paste bindings raise no number here even though they only now do the right thing.
        Command::Yank { .. } => true,
// --- end src/clip.rs -------------------------------------------------------


// --- src/find.rs + src/navigate.rs ---------------------------------------------------------------
        // All four spellings do something, including a bare `search`, which clears — that is what
        // makes `<Escape>`'s chain live.
        Command::Search { .. } | Command::SearchNext | Command::SearchPrev => true,
        // Every destination, in every tab flag: `-w` is folded into `-t` rather than ignored.
        Command::Navigate { .. } => true,
// --- end src/find.rs + src/navigate.rs ------------------------------------------------------------


        // --- src/caret.rs -------------------------------------------------------------------
        Command::SelectionToggle { .. }
        | Command::SelectionDrop
        | Command::SelectionReverse
        | Command::SelectionFollow { .. }
        | Command::MoveTo(_) => true,
        // --- end src/caret.rs ---------------------------------------------------------------

// --- src/prompt.rs -------------------------------------------------------------------------
        // All five act, and each of the three that can meet the wrong kind of question does what
        // qutebrowser does with it rather than nothing: `prompt-accept` says why a value is
        // refused, `prompt-item-focus` is `UnsupportedOperationError`, caught and passed
        // (`prompt.py:433-435`), and `prompt-fileselect-external` refuses by name. The other
        // fifteen prompt bindings are `rl-*` rows that reach `cmdline.rs` by name, and
        // `Command::Unimplemented` at the bottom of this match is what asks about those.
        Command::PromptAccept { .. }
        | Command::PromptItemFocus { .. }
        | Command::PromptOpenDownload { .. }
        | Command::PromptYank { .. }
        | Command::PromptFileselectExternal => true,
// --- end src/prompt.rs ---------------------------------------------------------------------

// --- unhardcoded ---------------------------------------------------------------------------
        // Live since `hints.auto_follow` became a setting: `never` is the value it acts under, and
        // under the other three it answers with why there is nothing to follow. See the arm in
        // `run`.
        Command::HintFollow => true,
// --- end unhardcoded -----------------------------------------------------------------------

// --- src/settingspage.rs -------------------------------------------------------------------
        // `sf` writes a file, or says why there was nothing to write. See `cmdline::save`.
        Command::Save { .. } => true,
        // `.` runs a command or says there is none to run; both are something happening.
        Command::CmdRepeatLast => true,
        // `Ss` loads `bru://chrome/settings`, generated from the live table on every request.
        Command::SettingsPage => true,
// --- end src/settingspage.rs ---------------------------------------------------------------

        // --- src/spawn.rs, src/editor.rs ----------------------------------------------------
        Command::Spawn { .. } => true,
        Command::EditText | Command::InsertText { .. } | Command::FakeKey { .. } => true,
        // --- end src/spawn.rs, src/editor.rs ------------------------------------------------

        Command::Nop | Command::ClearKeychain => true,

// --- src/utilcmds.rs -------------------------------------------------------
        // All twenty-two act, and **not one of them is bound by qutebrowser's defaults**, so this
        // block moves the live-binding count by nothing. `gt` is the near miss: it is
        // `cmd-set-text -s :tab-select`, which was live because prefilling the line is something
        // happening, and what changed is that the line now has a command behind it.
        //
        // `later`, `repeat` and `run-with-count` are live only when what they carry is: a
        // `:repeat 3 <something inert>` does nothing three times, and claiming it acts would be
        // claiming the inert command does. Same rule as a chain's, one link long.
        Command::Later { command, .. }
        | Command::Repeat { command, .. }
        | Command::RunWithCount { command, .. } => is_live(command),

        Command::TabSelect { .. }
        | Command::TabTake { .. }
        | Command::WindowOnly
        | Command::Screenshot { .. }
        | Command::JsEval { .. }
        | Command::EditUrl { .. }
        | Command::EditCommand { .. }
        | Command::QuickmarkAdd { .. }
        | Command::HistoryClear { .. }
        | Command::Restart
        | Command::Version
        | Command::Messages { .. }
        | Command::Process { .. }
        | Command::ClickElement { .. }
        | Command::ScrollToAnchor { .. }
        | Command::DownloadRemove { .. }
        | Command::ClearMessages
        | Command::MarksReload { .. } => true,
// --- end src/utilcmds.rs ---------------------------------------------------

// --- src/macros.rs -------------------------------------------------------------------------------
        // Both act in every spelling: bare (`q`, `@`) they open the mode that names a register,
        // and with one they record or replay straight away. A register that holds nothing says so
        // rather than doing nothing, which is still the key having an effect.
        Command::MacroRecord { .. } | Command::MacroRun { .. } => true,
// --- end src/macros.rs ---------------------------------------------------------------------------

// --- adblock ---------------------------------------------------------------------------------
        // Live, and not part of the default-binding count: qutebrowser binds none of them either.
        Command::AdblockUpdate | Command::AdblockToggle | Command::AdblockInfo => true,
// --- end adblock -----------------------------------------------------------------------------

// --- src/greasemonkey.rs -----------------------------------------------------------------------
        // Live, and not part of the default-binding count: qutebrowser binds no key to it either.
        Command::GreasemonkeyReload { .. } => true,
// --- end src/greasemonkey.rs -------------------------------------------------------------------

// --- lua runtime -------------------------------------------------------------------------------
        // Live, and none of them is bound to a key: they are typed, after a plugin has been edited.
        // A `Command::Plugin` is live by construction — it only exists because a handler is
        // registered for it — and whether that handler *does* anything is the plugin's business,
        // which is exactly the line bru cannot and should not draw.
        Command::Plugin { .. }
        | Command::PluginList
        | Command::PluginReload { .. }
        | Command::PluginDisable { .. } => true,
// --- end lua runtime ---------------------------------------------------------------------------

// --- src/devtools.rs, src/message.rs (the polish workstream) -------------------------------------
        Command::ViewSource | Command::Print => true,
        // Every `devtools <position>` is live, and every one of them opens a window: CEF has no
        // docked inspector to give a BrowserView. See `devtools.rs`.
        Command::DevTools | Command::DevToolsFocus => true,
        // No default binding names these; they are here so a workstream can say something and so
        // `:message-error x` can be typed. They cost the live count nothing either way.
        Command::Message { .. } => true,
// --- end src/devtools.rs, src/message.rs ---------------------------------------------------------

        // Almost all of these are still waiting for a milestone — but the readline and history
        // bindings reach `cmdline.rs` by name rather than as a variant, so it is the only thing
        // that can say whether one of them does something. Asking is what keeps the count honest.
        Command::Unimplemented(_) => crate::cmdline::claims(command),
    }
}

// --- src/help.rs -----------------------------------------------------------
/// Why a bound command will **never** act, or `None` if it might.
///
/// [`is_live`] answers "does pressing this do something today". This answers "and is that ever
/// going to change", and it exists because the answer is no for thirteen of the 298 default
/// bindings. A row that says "not yet" about a key nothing can implement is a lie of a different
/// kind from a row that says it about a key waiting for a milestone: it invites the same
/// investigation every few months, and the second one costs as much as the first.
///
/// The strings live with the module that measured them, not here — `settings::REFUSED`. This is
/// only the dispatch, and `help.rs` is the only caller. It is deliberately *not* consulted by `is_live`: a refused command is inert, and both
/// halves of the count would otherwise depend on one function.
pub fn refusal(command: &Command) -> Option<&'static str> {
    debug_assert!(!is_live(command), "a live command cannot also be refused");
    match command {
        // The twelve `t**` rows, all of them `config-cycle … content.plugins` or
        // `… content.cookies.accept`. `commands.rs` will not build a `ConfigCycle` for a setting
        // `settings.rs` does not have, so they arrive here as `Unimplemented` carrying the text
        // the setting's name is still in.
        Command::Unimplemented(text) => crate::settings::refusal_in(text),
        // All twelve are `config-cycle … ;; reload`, so they arrive as a chain and never as the
        // bare command. `is_live` on a chain is "every part acts", so one refused part is enough
        // to refuse the whole row — and it is the part the reader pressed the key for.
        //
        // The live parts are filtered out before the recursion rather than skipped inside it, and
        // that is not tidiness: without it the `debug_assert` above fires on `reload`. It did —
        // `every_binding_appears` aborted with "a live command cannot also be refused" the moment
        // `settings::refusal_in` was stubbed out and `find_map` walked past the `config-cycle` to
        // the second half of the chain. A `config.lua` writing `reload ;; config-cycle …` would
        // have reached it with nothing stubbed at all.
        Command::Chain(parts) => parts.iter().filter(|part| !is_live(part)).find_map(refusal),
// --- src/utilcmds.rs -------------------------------------------------------
        // The three that carry a command are exactly a chain one link long, so they answer the same
        // way: `:later 1s config-cycle … content.plugins` is refused for the reason the carried
        // command is refused for, and not "not yet". No default binding is any of these — the
        // reader who reaches this line wrote it in `config.lua`, which is precisely the case the
        // three-state page exists for.
        Command::Later { command, .. }
        | Command::Repeat { command, .. }
        | Command::RunWithCount { command, .. } => refusal(command),
// --- end src/utilcmds.rs ---------------------------------------------------
        _ => None,
    }
}
// --- end src/help.rs -------------------------------------------------------

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
    new_window: bool,
) {
    let offset = if forward { steps as i32 } else { -(steps as i32) };

// --- src/window.rs ---------------------------------------------------------
    // `wh` / `wl` — the entry the command would have moved to, in a window of its own. Same
    // limitation as the tab spelling and for the same reason: CEF serialises no navigation list, so
    // the new window starts on that page with an empty history.
    if new_window {
        if let Some(url) = entry_at_offset(browser, offset) {
            crate::window::open(state, &url);
        }
        return;
    }
// --- end src/window.rs -----------------------------------------------------

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

/// Close the window in front — `:close`. The process only ends if it was the last one.
fn close_window(state: &SharedState) {
    let window = state
        .lock()
        .expect("state mutex poisoned")
        .current_window_id();
    if let Some(window) = window {
        crate::window::close(state, window);
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
    // --- unhardcoded -------------------------------------------------------------------------
    // `BRU_DEBUG_ZOOM=1`, in the shape of `BRU_DEBUG_KEYS` and its siblings. Nothing in bru
    // reports the zoom level: `:zoom` is silent, the status bar has no field for it and a session
    // does not record it, so `zoom.levels` and `zoom.default` had no way to be *measured* rather
    // than asserted. Off by default — it is one line per `+`.
    {
        use std::sync::OnceLock;
        static ON: OnceLock<bool> = OnceLock::new();
        if *ON.get_or_init(|| std::env::var_os("BRU_DEBUG_ZOOM").is_some()) {
            eprintln!(
                "bru[zoom]: {percent}% (levels {:?}, default {}%)",
                zoom_levels(),
                zoom_default()
            );
        }
    }
    // --- end unhardcoded -----------------------------------------------------------------------
    host.set_zoom_level((percent as f64 / 100.0).ln() / 1.2f64.ln());
}

/// `zoom-in`/`zoom-out`: `offset` places along qutebrowser's list of levels, stopping at the ends
/// (`AbstractZoom.apply_offset`, over a NeighborList in `edge` mode).
fn zoom_by(browser: &mut Browser, offset: i32) {
    let current = zoom_percent(browser);
    // --- unhardcoded -------------------------------------------------------------------------
    let levels = zoom_levels();
    // --- end unhardcoded -----------------------------------------------------------------------
    // The nearest level to where the page actually is, so a zoom set by `:zoom 137` still steps.
    let nearest = levels
        .iter()
        .enumerate()
        .min_by_key(|(_, level)| level.abs_diff(current))
        .map(|(index, _)| index as i32)
        .unwrap_or(0);
    let index = (nearest + offset).clamp(0, levels.len() as i32 - 1) as usize;
    set_zoom_percent(browser, levels[index]);
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
// --- per-window mode -----------------------------------------------------------------------
/// A step may also be `win<id>:<step>`, which runs it in that window rather than in the one in
/// front. It is the only way a script can say anything about a second window's mode.
// --- end per-window mode -------------------------------------------------------------------
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

// --- per-window mode -----------------------------------------------------------------------
            // `win<id>:<step>` runs `<step>` in that window. Without it every `--cmd` script drives
            // whichever window is in front, and a claim about window 1's mode could only ever be
            // measured in window 0 — which is not a measurement. bru has no `:window-focus` command
            // for a script to use instead, and inventing one for a debug switch would change the
            // default-binding count.
            //
            // The focus and the command are taken in the **same** posted task on purpose. Focusing
            // in a step of its own does not hold: measured 2026-08-06 under mango, a script that
            // focused window 1 and ran the next step 900 ms later reported `win=Some(0)` again by
            // then — the compositor gives the focus back, and `on_window_activation_changed` is
            // wired to `focus_window`. Nothing between these two lines can run.
            let step: &str = match self
                .step
                .strip_prefix("win")
                .and_then(|rest| rest.split_once(':'))
                .filter(|(id, _)| !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()))
            {
                Some((id, rest)) => {
                    crate::tabs::focus(&state, id.parse().unwrap_or_default());
                    if rest.is_empty() {
                        return;
                    }
                    rest
                }
                None => self.step.as_str(),
            };
// --- end per-window mode -------------------------------------------------------------------

            let (count, text) = match step.split_once(':') {
                Some((count, rest)) => match count.parse::<u32>() {
                    Ok(count) => (Some(count), rest),
                    // `:open http://x` — a colon that is not a count prefix.
                    Err(_) => (None, step),
                },
                None => (None, step),
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
// --- src/settingspage.rs -------------------------------------------------------------------
            // `--cmd` stands in for a person pressing keys, so it records what `.` repeats for the
            // same reason `keys.rs` does — without it a `--cmd` script could never exercise `.`.
            crate::cmdline::record_last_command(&command, count);
// --- end src/settingspage.rs ---------------------------------------------------------------
            run(&state, &mut browser, &command, count);
        }
    }
}

/// What the command line runs when Enter is pressed. Installed once at startup by `app.rs`.
///
/// **It must not dispatch here.** This is reached from inside the message router's query handler —
/// the chrome answers `bru.accept()` with the typed text — and CEF-NOTES trap 12 says no call that
/// creates a browser or starts a navigation may run inside one: the router holds
/// `browser_query_info_map` across the handler, and `on_before_browse`, which bru is obliged to
/// forward to the router, wants that same lock. `:open` would deadlock bru with its window still
/// painted. So the work is posted and happens on the next turn of the UI loop.
pub fn run_from_cmdline(text: &str, count: Option<u32>) {
    let mut task = CmdLineCommand::new(text.to_string(), count);
    post_task(ThreadId::UI, Some(&mut task));
}

wrap_task! {
    struct CmdLineCommand {
        text: String,
        count: Option<u32>,
    }

    impl Task {
        fn execute(&self) {
            debug_assert_ne!(currently_on(ThreadId::UI), 0);

            let Some(state) = crate::state::BruState::instance() else {
                return;
            };
            // Both of these used to be an eprintln, where nobody running a browser was looking:
            // a mistyped `:` command simply did nothing. They are the bar's first real caller.
            let command = match crate::commands::parse(&self.text) {
                Ok(command) => command,
                Err(error) => {
                    crate::message::error(&format!("{}: {error}", self.text));
                    return;
                }
            };
            if !is_live(&command) {
                crate::message::warning(&format!("{}: not implemented yet", self.text));
                return;
            }
            let Some(mut browser) = state.lock().expect("state mutex poisoned").active_browser()
            else {
                return;
            };
// --- src/macros.rs -------------------------------------------------------------------------------
            // What a macro keeps of `o example.com<Return>`. Command mode is left before the text
            // runs (`cmdline::on_accept` → `leave_command_mode`, and `statusbar/command.py:193-198`
            // in qutebrowser), so the mode `record` sees here is normal and the accepted command is
            // recorded — while the `cmd-set-text` that opened the line was refused. That pairing is
            // the whole reason a macro can contain a URL that was typed after `q` was pressed.
            crate::macros::record(&state, &command, self.count);
// --- end src/macros.rs ---------------------------------------------------------------------------
// --- src/settingspage.rs -------------------------------------------------------------------
            // A typed line is one of the two things `.` can repeat, and it is recorded here rather
            // than in `run` because `run` recurses for a chain: `a ;; b` must be remembered whole.
            crate::cmdline::record_last_command(&command, self.count);
// --- end src/settingspage.rs ---------------------------------------------------------------
            run(&state, &mut browser, &command, self.count);
        }
    }
}

/// One line per step: what the command was, and the state a screenshot would have to agree with.
///
/// `order` is the strip, left to right, each tab shown by the tail of its URL — without it a
/// `tab-move` that did nothing and one that moved a tab back to where it was look identical.
fn report(state: &SharedState, step: &str) {
    let (active, total, url, order, windows, current, ids) = {
        let guard = state.lock().expect("state mutex poisoned");
        let active = guard.active_tab();
        let total = guard.tab_count();
        let url = guard.tab_url(active).unwrap_or_default();
        let order: Vec<String> = (0..total)
            .map(|i| tail(&guard.tab_url(i).unwrap_or_default()))
            .collect();
// --- src/window.rs ---------------------------------------------------------
        // How many windows there are, which one this is, and how many tabs each holds. Without it a
        // `:tab-give` that moved a tab and one that opened a window and lost it look identical —
        // the current window's count falls by one either way.
// --- per-window mode -----------------------------------------------------------------------
        // Each window's tab count *and its mode*. A mode is one window's now, so a report that
        // printed one mode could not tell "window 1 is in command mode" from "both windows are",
        // which is the whole of what this workstream changed.
        let windows: Vec<String> = guard
            .window_ids()
            .into_iter()
            .map(|id| format!("{id}:{}:{}", guard.tab_count_in(id), guard.mode_in(id)))
            .collect();
// --- end per-window mode -------------------------------------------------------------------
        let current = guard.current_window_id();
        (
            active,
            total,
            url,
            order.join(","),
            windows.join(" "),
            current,
            guard.window_ids(),
        )
// --- end src/window.rs -----------------------------------------------------
    };
    // Re-fetched outside the lock: reading the zoom is a CEF call.
    let browser = state.lock().expect("state mutex poisoned").active_browser();
    let zoom = browser
        .map(|mut browser| zoom_percent(&mut browser))
        .unwrap_or(ZOOM_DEFAULT);

// --- per-window mode -----------------------------------------------------------------------
    // What each window's command line is holding, printed only when one of them holds something.
    // There is a line per window now, and two windows half way through typing different things is
    // the case a single shared line could not hold at all — so it has to be visible from a script.
    //
    // Outside the state lock deliberately: `cmdline::text_in` takes the command-line lock, and
    // `cmdline` takes the state lock in other places. Two locks in one order are safe; the same two
    // in both orders are a deadlock waiting for the day a push lands mid-report.
    let lines: Vec<String> = ids
        .into_iter()
        .map(|id| (id, crate::cmdline::text_in(id)))
        .filter(|(_, text)| !text.is_empty())
        .map(|(id, text)| format!("{id}:{text:?}"))
        .collect();
    let lines = if lines.is_empty() {
        String::new()
    } else {
        format!(" lines=[{}]", lines.join(" "))
    };
// --- end per-window mode -------------------------------------------------------------------

    eprintln!(
        "cmd: after {step:?} -> windows=[{windows}] win={current:?} tabs={total} active={active} \
         zoom={zoom}% order=[{order}]{lines} url={url}"
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
    /// `(live, bound but inert, no command behind the name)`.
    ///
    /// **`is_live` is asked first, and that ordering is the whole point.** The obvious spelling —
    /// "does it parse into a real variant, and only then, does it act" — undercounted by 17 for a
    /// whole stage: the readline bindings (`<Ctrl-A>`, `<Ctrl-E>`, `<Ctrl-U>`, `<Ctrl-W>`, `<Alt-B>`
    /// and the rest of `command:`) reach `cmdline.rs` **by name**, as `Command::Unimplemented`, and
    /// they work. Classifying by shape put every one of them with the dead, and the headline number
    /// this file exists to publish was quietly wrong.
    ///
    /// A binding is live if pressing it does something. Nothing else is the question.
    fn split() -> (usize, usize, usize) {
        let (mut live, mut inert, mut nameless) = (0, 0, 0);
        for (_mode, _keys, cmd) in DEFAULT_BINDINGS {
            let parsed = commands::parse(cmd).expect("a default binding must parse");
            if is_live(&parsed) {
                live += 1;
            } else if parsed.is_implemented() {
                inert += 1;
            } else {
                nameless += 1;
            }
        }
        (live, inert, nameless)
    }

    #[test]
    fn how_many_default_bindings_are_live() {
        let (live, ignored, unparsed) = split();
        println!(
            "default bindings: {live} live, {ignored} bound but inert, {unparsed} with no command \
             behind the name, {} total",
            live + ignored + unparsed
        );
        assert_eq!(live + ignored + unparsed, DEFAULT_BINDINGS.len());
        // 226 through stage 1 and most of stage 2; 231 once hint mode existed and its five
        // `hint:` bindings (configdata.yml:3884) had a mode to belong to; 262 once caret mode
        // brought the 29 of `caret:` (3961) and the two mark modes each brought the one line of
        // `register:` (3991); 264 once macros brought the other two modes that read it.
        // 298 with src/prompt.rs, which brought `bindings.default.prompt`'s 26 rows and
        // `.yesno`'s 8 — the last two sections of `configdata.yml` bru had no mode for.
        // 293 since the inspector kept `wi` and lost `wIh`, `wIj`, `wIk`, `wIl`, `wIw` and `wIf`:
        // five of the six bound a docked position CEF cannot draw. The only fall in this number.
        assert_eq!(DEFAULT_BINDINGS.len(), 293);
        // The number this project measures itself by: how many of qutebrowser's own default keys
        // do something when pressed.
        //
        // 27 before stage 2 began. 70 after the dispatcher, 76 once `scroll.rs` made `gg`/`G` and
        // the page keys real, 100 once the command line claimed `cmd-set-text`, `command-accept`
        // and the readline bindings, 106 with hints. Then stage 3: 109 with downloads, 114 with
        // quickmarks and bookmarks, 124 with the clipboard, 138 with `n`/`N` and `navigate`, 152
        // with settings, 162 with the completion's own keys, 196 with caret mode and marks, 207
        // with hint groups and targets, 216 with the polish, 224 once hints, caret and the
        // completion were introduced to the clipboard and the downloads.
        //
        // 241 is the same tree measured correctly: `split` used to ask "is this a real variant"
        // before "does it act", which put the 17 readline bindings with the dead for a whole stage.
        // Nothing was fixed to reach it — the ruler was.
        //
        // 245 with macros, which is four and not the two `q` and `@` are worth on their own: adding
        // `record_macro` and `run_macro` adds their `register:` `<Escape>` row apiece to the table
        // as well, exactly as `set_mark` and `jump_mark` each did. Both new rows are live, so the
        // denominator and the numerator move together — 241/262 to 245/264.
        //
        // 242 with `sf`, whose `save` writes the command line's history to `cmd-history` — the one
        // saveable bru has that was not already on disk. 243 with `.`. 244 with `Ss`, a bare `set`,
        // which opens `bru://chrome/settings`. The fourth of that group, `<Return>` in hint mode,
        // stayed inert on purpose and raised nothing — until `hints.auto_follow` became a setting;
        // see the `HintFollow` arm in `run`, and the note at the assertion below.
        //
        // 243 with a second window: `gD` (`tab-give`) and `U` (`undo -w`), the two bindings whose
        // whole reason for being inert was that there was one window. The `-w` spellings of `open`,
        // `back`, `forward`, `navigate`, `quickmark-load` and `bookmark-load` raise no number here
        // — they counted as live while they quietly opened a tab — so what fixing them shows up in
        // is the run against the real browser, not this.
        //
        // 251 with `;R` (`hint --rapid links window`). It is the third binding the second window
        // is worth and the last one it was owed: `Target::Window` opened a foreground tab, which
        // is `tab-fg`'s own reason for refusing `--rapid`, and it opens a window now.
        //
        // **285 with prompt mode, and the denominator moved with it: 251/264 to 285/298.** All
        // thirty-four new rows are live, and they are live for two different reasons worth keeping
        // apart. Nineteen name one of the five `prompt-*` commands, which `src/prompt.rs`
        // implements and `is_live` claims outright. The other fifteen are `rl-*` rows: they parse
        // to `Unimplemented`, reach `cmdline.rs` by name, and are answered by `prompt::run_readline`
        // before the command line sees them — so they are counted by the same `cmdline::claims`
        // call that has counted command mode's seventeen since the ruler was fixed. Both halves
        // were checked by pressing them, not by reading this match.
        //
        // --- unhardcoded -----------------------------------------------------------------
        // **286 with `<Return>` in hint mode**, and it is the one row in this file's history that
        // moved from *refused* to live rather than from waiting: `hints.auto_follow` became a
        // setting, `never` is a value it can be set to, and that is the state the key exists for.
        // The denominator does not move — the row was always in the table.
        // --- end unhardcoded -------------------------------------------------------------
        //
        // **281 since the inspector kept one key of seven.** The six that went — `wIh`, `wIj`,
        // `wIk`, `wIl`, `wIw`, `wIf` — were all live, because `devtools` is implemented; what was
        // wrong was the docked placement five of them named, which CEF cannot draw. Here the
        // denominator moves with the numerator, since the rows left the table.
        //
        // Raise this when a milestone raises the number, never to make a failing build pass.
        assert_eq!(live, 281, "the live-binding count moved");
    }

// --- src/help.rs -----------------------------------------------------------
    /// **Every default binding now either acts or is refused. Nothing is merely waiting.**
    ///
    /// 286 and 12, and the twelve are named rather than counted: six `content.plugins` and six
    /// `content.cookies.accept`. Both groups were measured against CEF 151 rather than assumed —
    /// the reasons are in `settings::REFUSED`, and the arms above carry the numbers.
    ///
    // --- unhardcoded ---------------------------------------------------------------------------
    /// **It was 285 and 13, and the thirteenth was `<Return>` in hint mode.** It is live now, and
    /// what made it live is `hints.auto_follow` becoming a setting — the arm in `run` carries that.
    /// A refusal that stops being true is deleted rather than left standing: leaving it would be a
    /// sentence on `bru://chrome/help` telling the user a key can never work while it works.
    // --- end unhardcoded -----------------------------------------------------------------------
    ///
    /// If a milestone ever binds a key to a command it has not built, `waiting` stops being empty
    /// and `bru://chrome/help` grows a "not yet" row again. That is correct, and it is why this
    /// asserts the emptiness rather than deleting the state.
    #[test]
    fn nothing_bound_by_default_is_merely_waiting() {
        let (mut live, mut refused, mut waiting) = (0usize, Vec::new(), Vec::new());
        for (mode, keys, cmd) in DEFAULT_BINDINGS {
            let parsed = commands::parse(cmd).expect("a default binding must parse");
            if is_live(&parsed) {
                live += 1;
            } else if refusal(&parsed).is_some() {
                refused.push((*mode, *keys));
            } else {
                waiting.push((*mode, *keys, *cmd));
            }
        }
        assert!(waiting.is_empty(), "bound and waiting for a milestone: {waiting:?}");
        // 281: the six inspector keys that went were all live, since `devtools` is implemented —
        // what was wrong with them was the placement they promised, not the command behind them.
        assert_eq!(live, 281);
        assert_eq!(
            refused,
            [
                ("normal", "tph"), ("normal", "tPh"), ("normal", "tpH"),
                ("normal", "tPH"), ("normal", "tpu"), ("normal", "tPu"),
                ("normal", "tch"), ("normal", "tCh"), ("normal", "tcH"),
                ("normal", "tCH"), ("normal", "tcu"), ("normal", "tCu"),
            ]
        );
        // A live command is never also refused — `refusal` debug-asserts it, and this walks the
        // whole table past that assertion rather than trusting one call.
        for (_, _, cmd) in DEFAULT_BINDINGS {
            let parsed = commands::parse(cmd).expect("a default binding must parse");
            if !is_live(&parsed) {
                let _ = refusal(&parsed);
            }
        }
    }
// --- end src/help.rs -------------------------------------------------------

// --- src/settingspage.rs -------------------------------------------------------------------
    /// The three bindings this workstream turned on, named, and the one it did not.
    #[test]
    fn the_last_four_single_bindings() {
        for (keys, expected) in [("sf", "save"), ("Ss", "set"), (".", "cmd-repeat-last")] {
            let (_, _, cmd) = DEFAULT_BINDINGS
                .iter()
                .find(|(mode, k, _)| *mode == "normal" && *k == keys)
                .unwrap_or_else(|| panic!("no default binding for {keys}"));
            assert_eq!(*cmd, expected);
            let parsed = commands::parse(cmd).expect("a default binding must parse");
            assert!(is_live(&parsed), "{keys} -> {cmd:?} is still inert");
        }
// --- unhardcoded -----------------------------------------------------------------------
        // `<Return>` in hint mode was the fourth of that group, bound and deliberately inert. It
        // acts now, and the thing that made it act is `hints.auto_follow` becoming a setting — see
        // the arm in `run`. Kept as an assertion rather than deleted, because the row moving from
        // refused to live is the whole of what this workstream did to the count.
        let (_, _, cmd) = DEFAULT_BINDINGS
            .iter()
            .find(|(mode, k, _)| *mode == "hint" && *k == "<Return>")
            .expect("hint mode binds <Return>");
        assert_eq!(*cmd, "hint-follow");
        assert!(is_live(&commands::parse(cmd).unwrap()));
        // `refusal` is not asked here on purpose: it debug-asserts that its argument is *not* live,
        // so asking it about a live command is a panic rather than a `None`. That the row is no
        // longer refused is asserted where the refusals are counted — `help.rs`'s
        // `a_binding_nothing_can_implement_says_refused_and_why`.
// --- end unhardcoded -------------------------------------------------------------------

        // A bare `set` is the settings page; `set` with an option is still `settings.rs`'s.
        assert_eq!(commands::parse("set").unwrap(), Command::SettingsPage);
        assert!(is_live(&commands::parse("set content.images").unwrap()));
        // `config-cycle` has nothing to cycle without an option and stays inert.
        assert!(!is_live(&commands::parse("config-cycle").unwrap()));
    }
// --- end src/settingspage.rs ---------------------------------------------------------------

// --- hint-follow -----------------------------------------------------------------------------
    /// Measurement 1 behind the once-inert `hint-follow`: no label is a prefix of another, so a
    /// complete label can never be sitting unfollowed with Enter left to press.
    ///
    /// It is still true and still worth asserting — it is why `hints.auto_follow` at its default
    /// leaves `<Return>` nothing to do, which is what `hints::follow_current` says when it is
    /// pressed there. Under `never` the key has a job because `handle_key` stops following, not
    /// because this stopped holding.
    ///
    /// Over 1..600 elements rather than one page's worth, because the property is a claim about
    /// `hint_strings`' arithmetic — the short/long split at `short_count * len(chars)` — and one
    /// count would only prove it where it was easiest.
    #[test]
    fn hint_labels_are_prefix_free() {
        for count in 1..600usize {
            let labels = crate::hints::hint_strings(count);
            assert_eq!(labels.len(), count);
            for (i, a) in labels.iter().enumerate() {
                for (j, b) in labels.iter().enumerate() {
                    if i != j {
                        assert!(
                            !b.starts_with(a.as_str()),
                            "{count} labels: {a:?} is a prefix of {b:?}, so Enter would have a job"
                        );
                    }
                }
            }
        }
    }
// --- end hint-follow -------------------------------------------------------------------------

// --- src/downloads.rs --------------------------------------------------------------------------
    /// The three bindings downloads turned on, named — and the one it deliberately did not.
    #[test]
    fn the_bindings_downloads_turned_on() {
        for (keys, expected) in [
            ("gd", "download"),
            ("ad", "download-cancel"),
            ("cd", "download-clear"),
        ] {
            let (_, _, cmd) = DEFAULT_BINDINGS
                .iter()
                .find(|(mode, k, _)| *mode == "normal" && *k == keys)
                .unwrap_or_else(|| panic!("no default binding for {keys}"));
            assert_eq!(*cmd, expected);
            let parsed = commands::parse(cmd).expect("a default binding must parse");
            assert!(is_live(&parsed), "{keys} -> {cmd:?} is still inert");
        }
        // `;d` is `hint links download`. It parses now — src/hints.rs implemented the `links`
        // group — but it is still not live, and that is the honest state: hints resolves the URL
        // and has nowhere to send it until `hints::install_downloads` is called with
        // `downloads::schedule_start`. When that lands, this flips to `assert!(is_live(...))` and
        // the count rises by one, in the same commit.
        assert!(
            is_live(&commands::parse("hint links download").unwrap()),
            "`;d` is live once `hints::install_downloads` has been called with downloads.rs"
        );
        // `--mhtml` is live now: `CefBrowserHost` has no save-a-document call, but it has the
        // DevTools protocol, and `Page.captureSnapshot` is one. It is bound to nothing in
        // qutebrowser, so this raises the count by zero.
        assert!(is_live(&commands::parse("download --mhtml").unwrap()));
        assert_eq!(commands::parse("download -m").unwrap(), Command::DownloadMhtml);
        // qutebrowser's own refusal, commands.py:1390 — there is nothing to serialise about a URL
        // that is not open, so this is an error rather than a download of the wrong thing.
        assert!(commands::parse("download --mhtml https://e.com/x").is_err());
        // `--dest` still needs the prompt bru has not got.
        assert!(!is_live(&commands::parse("download --dest /tmp/x").unwrap()));
    }

    #[test]
    fn download_arguments() {
        use crate::commands::Command as C;
        assert_eq!(commands::parse("download").unwrap(), C::Download { url: None });
        assert_eq!(
            commands::parse("download https://e.com/a b.pdf").unwrap(),
            C::Download { url: Some("https://e.com/a b.pdf".to_string()) }
        );
        assert_eq!(
            commands::parse("download-cancel").unwrap(),
            C::DownloadCancel { all: false }
        );
        assert_eq!(
            commands::parse("download-cancel --all").unwrap(),
            C::DownloadCancel { all: true }
        );
        assert_eq!(commands::parse("download-clear").unwrap(), C::DownloadClear);
        assert_eq!(
            commands::parse("download-open").unwrap(),
            C::DownloadOpen { cmdline: None, dir: false }
        );
        assert_eq!(
            commands::parse("download-open -d").unwrap(),
            C::DownloadOpen { cmdline: None, dir: true }
        );
        // maxsplit=0: the command to open with keeps its own flags.
        assert_eq!(
            commands::parse("download-open mpv --no-audio {}").unwrap(),
            C::DownloadOpen { cmdline: Some("mpv --no-audio {}".to_string()), dir: false }
        );
        assert_eq!(commands::parse("download-delete").unwrap(), C::DownloadDelete);
        assert_eq!(commands::parse("download-retry").unwrap(), C::DownloadRetry);
    }
// --- end src/downloads.rs ----------------------------------------------------------------------

// --- src/history.rs --------------------------------------------------------
    /// The five bindings this workstream made live, named rather than counted — a total cannot
    /// notice that `Sq` went live and `Sb` did not.
    #[test]
    fn the_bindings_history_turned_on() {
        for keys in [
            "m",  // quickmark-save
            "M",  // bookmark-add
            "Sq", // bookmark-list
            "Sb", // bookmark-list --jump
            "Sh", // history
        ] {
            let (_, _, cmd) = DEFAULT_BINDINGS
                .iter()
                .find(|(mode, k, _)| *mode == "normal" && *k == keys)
                .unwrap_or_else(|| panic!("no default binding for {keys}"));
            let parsed = commands::parse(cmd).expect("a default binding must parse");
            assert!(is_live(&parsed), "{keys} -> {cmd:?} is still inert");
        }

        // The six that prefill the command line were live before this and still are — but what they
        // prefill has to parse to something the dispatcher runs, or pressing `b`, typing a name and
        // hitting Enter reaches "not implemented yet" instead of a page.
        for (keys, typed) in [
            ("b", "quickmark-load go"),
            ("B", "quickmark-load -t go"),
            ("wb", "quickmark-load -w go"),
            ("gb", "bookmark-load https://example.com/"),
            ("gB", "bookmark-load -t https://example.com/"),
            ("wB", "bookmark-load -w https://example.com/"),
        ] {
            assert!(
                DEFAULT_BINDINGS
                    .iter()
                    .any(|(mode, k, _)| *mode == "normal" && *k == keys),
                "no default binding for {keys}"
            );
            let parsed = commands::parse(typed).expect("the line the binding prefills must parse");
            assert!(is_live(&parsed), "{typed:?} is still inert");
        }
    }
// --- end src/history.rs ----------------------------------------------------

// --- src/clip.rs -----------------------------------------------------------
    /// The ten bindings the clipboard turned on, named one by one.
    ///
    /// The six paste bindings are *not* here, and that is the point: `open -- {clipboard}` counted
    /// as live before this milestone, because `Command::Open` is live and the parser had no
    /// opinion about its argument. It opened a search for the literal text `{clipboard}`. So this
    /// number cannot show that `pp` was fixed — only `clip::expand`'s own tests can, and the run
    /// against the real browser.
    #[test]
    fn the_bindings_the_clipboard_turned_on() {
        for keys in ["yy", "yY", "yt", "yT", "yd", "yD", "yp", "yP", "ym", "yM"] {
            let (_, _, cmd) = DEFAULT_BINDINGS
                .iter()
                .find(|(mode, k, _)| *mode == "normal" && *k == keys)
                .unwrap_or_else(|| panic!("no default binding for {keys}"));
            let parsed = commands::parse(cmd).expect("a default binding must parse");
            assert!(is_live(&parsed), "{keys} -> {cmd:?} is still inert");
        }
        // `-s` reached the command, or five of those ten yank to the wrong selection.
        let sel = ["yY", "yT", "yD", "yP", "yM"].map(|keys| {
            let (_, _, cmd) = DEFAULT_BINDINGS
                .iter()
                .find(|(mode, k, _)| *mode == "normal" && *k == keys)
                .expect("a default binding");
            matches!(commands::parse(cmd), Ok(Command::Yank { sel: true, .. }))
        });
        assert_eq!(sel, [true; 5], "an -s binding lost its primary selection");
    }
// --- end src/clip.rs -------------------------------------------------------

// --- src/session.rs --------------------------------------------------------
    /// The two bindings sessions turned on, named — and the one it deliberately did not.
    ///
    /// `session-save`, `session-load` and `session-delete` are typed, not bound: qutebrowser reaches
    /// them through the `w` and `wq` aliases (configdata.yml:5), and bru has no alias table. So they
    /// are live commands that move no binding count, and this test says so rather than leaving the
    /// unchanged number looking like nothing happened.
    #[test]
    fn the_bindings_sessions_turned_on() {
        for keys in ["<Ctrl-p>", "<Alt-m>"] {
            let (_, _, cmd) = DEFAULT_BINDINGS
                .iter()
                .find(|(mode, k, _)| *mode == "normal" && *k == keys)
                .unwrap_or_else(|| panic!("no default binding for {keys}"));
            let parsed = commands::parse(cmd).expect("a default binding must parse");
            assert!(is_live(&parsed), "{keys} -> {cmd:?} is still inert");
        }
// --- src/window.rs ---------------------------------------------------------
        // `gD` is tab-give. It was inert for as long as there was one window; it moves the tab now.
        assert_eq!(
            commands::parse("tab-give").unwrap(),
            Command::TabGive { win_id: None }
        );
        assert!(is_live(&commands::parse("tab-give").unwrap()));
// --- end src/window.rs -----------------------------------------------------

        assert!(is_live(&commands::parse("session-save").unwrap()));
        assert!(is_live(&commands::parse("session-load work").unwrap()));
        assert!(is_live(&commands::parse("session-delete work").unwrap()));
        // A session command with no name is a typo, not a default.
        assert!(commands::parse("session-load").is_err());
        assert!(commands::parse("session-delete").is_err());
    }

    #[test]
    fn session_command_flags() {
        use crate::commands::Command as C;
        assert_eq!(
            commands::parse("session-save").unwrap(),
            C::SessionSave { name: None, force: false }
        );
        assert_eq!(
            commands::parse("session-save -f work").unwrap(),
            C::SessionSave { name: Some("work".to_string()), force: true }
        );
        assert_eq!(
            commands::parse("session-load -c --history work").unwrap(),
            C::SessionLoad { name: "work".to_string(), clear: true, history: true }
        );
        assert_eq!(
            commands::parse("session-load work").unwrap(),
            C::SessionLoad { name: "work".to_string(), clear: false, history: false }
        );
        // `:quit --save` is the one spelling that writes a session on the way out; `:quit` and
        // `:close` must not.
        assert_eq!(commands::parse("quit --save").unwrap(), C::Quit { save: true });
        assert_eq!(commands::parse("quit").unwrap(), C::Quit { save: false });
    }
// --- end src/session.rs ----------------------------------------------------

// --- src/settings.rs -------------------------------------------------------
    /// The twelve `config-cycle` bindings a settings store turned on, and the twelve it did not.
    ///
    /// A total is not enough here: `config-cycle` accounts for 24 of the default bindings and half
    /// of them name settings bru refuses. If a later change quietly makes `content.plugins` "work",
    /// this is what says so.
    #[test]
    fn the_bindings_the_settings_store_turned_on_and_the_ones_it_refused() {
        let live_now = [
            // content.javascript.enabled, six spellings of the scope
            "tsh", "tSh", "tsH", "tSH", "tsu", "tSu",
            // content.images, the same six
            "tih", "tIh", "tiH", "tIH", "tiu", "tIu",
        ];
        let still_inert = [
            // content.plugins — Chromium 151 has no such content setting
            "tph", "tPh", "tpH", "tPH", "tpu", "tPu",
            // content.cookies.accept — no-3rdparty is not expressible through set_content_setting
            "tch", "tCh", "tcH", "tCH", "tcu", "tCu",
        ];
        // `Ss` — a bare `set` — was in the list above until `bru://chrome/settings` existed to
        // open. It is counted here so the arithmetic at the bottom still describes 25 bindings.
        let live_now_too = ["Ss"];
        let command_for = |keys: &str| {
            DEFAULT_BINDINGS
                .iter()
                .find(|(mode, k, _)| *mode == "normal" && *k == keys)
                .unwrap_or_else(|| panic!("no default binding for {keys}"))
                .2
        };

        for keys in live_now.iter().chain(live_now_too.iter()) {
            let cmd = command_for(keys);
            let parsed = commands::parse(cmd).expect("a default binding must parse");
            assert!(is_live(&parsed), "{keys} -> {cmd:?} is still inert");
        }
        for keys in still_inert {
            let cmd = command_for(keys);
            let parsed = commands::parse(cmd).expect("a default binding must parse");
            assert!(
                !is_live(&parsed),
                "{keys} -> {cmd:?} claims to work; if that is true, say which setting it moves"
            );
        }
        assert_eq!(live_now.len() + live_now_too.len() + still_inert.len(), 25);
    }

    /// `:set` answers for an option bru refuses; `config-cycle` on the same option does not.
    ///
    /// The asymmetry is the only reason the live count is allowed to stay at 118 while `:set`
    /// accepts anything — so it is asserted rather than left to the comment beside it.
    #[test]
    fn set_answers_for_a_refused_option_and_config_cycle_stays_inert() {
        let typed = commands::parse("set content.plugins false").unwrap();
        assert!(is_live(&typed), ":set has to reach settings.rs to explain the refusal");
        assert!(matches!(typed, Command::Set { .. }));

        let bound = commands::parse("config-cycle -p -t -u *://x/* content.plugins").unwrap();
        assert!(!is_live(&bound));
        assert_eq!(
            bound,
            Command::Unimplemented("config-cycle -p -t -u *://x/* content.plugins".to_string())
        );

        // A bare `:set` — the `Ss` binding — is neither of these: it means "open the settings
        // page", which is `Command::SettingsPage` and lives now. See `the_last_four_single_bindings`.
        assert_eq!(commands::parse("set").unwrap(), Command::SettingsPage);
    }
// --- end src/settings.rs ---------------------------------------------------

// --- config commands ---------------------------------------------------------------------------
    /// **The ten commands that reach the configuration raise the live-binding count by nothing**,
    /// and that is a fact about the default table rather than a shortfall.
    ///
    /// Not one of them is bound in `configdata.yml`, in any mode: they are typed. The one default
    /// binding that so much as mentions `:bind` is `sk`, which is `cmd-set-text -s :bind` — it puts
    /// the text in the command line and has been live since `cmdline.rs` existed, whether or not
    /// anything answered when the line was accepted. So the count is unmoved by them, and this is
    /// what says so rather than the absence of a change.
    #[test]
    fn the_config_commands_raise_no_binding() {
        let names = [
            "config-clear",
            "config-diff",
            "config-edit",
            "config-list-add",
            "config-list-remove",
            "config-source",
            "config-unset",
            "config-write-py",
            "bind",
            "unbind",
        ];
        for (mode, keys, cmd) in DEFAULT_BINDINGS {
            for name in names {
                let first = cmd.split_whitespace().next().unwrap_or("");
                assert_ne!(
                    first, name,
                    "{mode} {keys} is bound to {name}; the live count above has to move with it"
                );
            }
        }
        // `sk` is the near miss, and it was already live: what it does is type, not bind.
        let (_, _, sk) = DEFAULT_BINDINGS
            .iter()
            .find(|(mode, k, _)| *mode == "normal" && *k == "sk")
            .expect("sk is a default binding");
        assert_eq!(*sk, "cmd-set-text -s :bind");
        assert!(is_live(&commands::parse(sk).unwrap()));
        // And what it puts in the line is a command that now answers, which it did not before.
        assert!(is_live(&commands::parse("bind j").unwrap()));
    }
// --- end config commands -----------------------------------------------------------------------

// --- src/hints.rs -----------------------------------------------------------------------
    /// The bindings hint groups and targets turned on, and the four still inert, named one by one.
    /// 106 → 117 is not enough to notice that `;i` went live and `;I` did not.
    #[test]
    fn the_bindings_hint_groups_and_targets_turned_on() {
        let live_now = [
            ("normal", "wf"),  // hint all window
            // Live before this milestone too, and wrong: it opened a *background* tab. Named here
            // because "still live" is not the claim — `HintTarget::TabFg` is.
            ("normal", ";f"),  // hint all tab-fg
            ("normal", ";h"),  // hint all hover
            ("normal", ";i"),  // hint images
            ("normal", ";I"),  // hint images tab
            ("normal", ";o"),  // hint links fill :open {hint-url}
            ("normal", ";O"),  // hint links fill :open -t -r {hint-url}
            ("normal", ";r"),  // hint --rapid links tab-bg
            ("normal", ";t"),  // hint inputs
            ("normal", "gi"),  // hint inputs --first
            // The `hint:` table, reachable because `hints::handle_key` consults it before the
            // labels — modeparsers.py:196.
            ("hint", "<Ctrl-R>"), // hint --rapid links tab-bg
            ("hint", "<Ctrl-F>"), // hint links
        ];
        for (mode, keys) in live_now {
            let (_, _, cmd) = DEFAULT_BINDINGS
                .iter()
                .find(|(m, k, _)| *m == mode && *k == keys)
                .unwrap_or_else(|| panic!("no default binding for {keys} in {mode}"));
            let parsed = commands::parse(cmd).expect("a default binding must parse");
            assert!(is_live(&parsed), "{keys} -> {cmd:?} is still inert");
        }

        // Three of the four that were inert have somewhere to go now: `app.rs` installs
        // `clip::HintClipboard` and `clip::HintDownloads` at startup.
        for cmd in ["hint links yank", "hint links yank-primary", "hint links download"] {
            assert!(
                is_live(&commands::parse(cmd).unwrap()),
                "{cmd:?} has a clipboard and a download manager now"
            );
        }
        // The fourth was inert because `window` opened a foreground tab, which is `tab-fg`'s own
        // objection to `--rapid`. It opens a window now, and `;R` is live with it.
        assert!(is_live(&commands::parse("hint --rapid links window").unwrap()));
        // The two `hints.py:1027` refuses are still refused, and the same targets without
        // `--rapid` are live. Otherwise the four above would pass with the feature switched off.
        assert!(!is_live(&commands::parse("hint --rapid links tab-fg").unwrap()));
        assert!(!is_live(&commands::parse("hint --rapid links fill :open {hint-url}").unwrap()));
        assert!(is_live(&commands::parse("hint links window").unwrap()));
        assert!(is_live(&commands::parse("hint --rapid links tab-bg").unwrap()));
        assert!(is_live(&commands::parse("hint --rapid all hover").unwrap()));
    }

    /// The one binding a hint that opens a real window turned on, named: `;R`.
    ///
    /// `wf` is not in it, and that is the point of naming rather than counting. `hint all window`
    /// has been live since hint targets existed — it opened a *foreground tab*, which `is_live`
    /// cannot see and a total cannot show. What the number can show is `;R`, whose refusal was
    /// `Target::Window` being a tab under a different name.
    #[test]
    fn the_binding_a_window_hint_turned_on() {
        let (_, _, cmd) = DEFAULT_BINDINGS
            .iter()
            .find(|(mode, k, _)| *mode == "normal" && *k == ";R")
            .expect("normal mode binds ;R");
        assert_eq!(*cmd, "hint --rapid links window");
        assert!(is_live(&commands::parse(cmd).unwrap()));

        // `wf` was live before and is live now; it is here so that a reader looking for the second
        // window's hint bindings finds both, and so that deleting the `window` target fails twice.
        let (_, _, cmd) = DEFAULT_BINDINGS
            .iter()
            .find(|(mode, k, _)| *mode == "normal" && *k == "wf")
            .expect("normal mode binds wf");
        assert_eq!(*cmd, "hint all window");
        assert!(is_live(&commands::parse(cmd).unwrap()));
    }
// --- end src/hints.rs -------------------------------------------------------------------

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
// --- src/window.rs ---------------------------------------------------------
        // `U` is `undo -w`. It was the one spelling that did nothing, because there was one window;
        // it reopens the last closed one now, with the tabs it held.
        assert!(is_live(&commands::parse("undo -w").unwrap()));
// --- end src/window.rs -----------------------------------------------------
    }

// --- src/window.rs ---------------------------------------------------------
    /// The two bindings a second window turned on, named — and the two `-w` spellings that were
    /// already counted live and were quietly wrong.
    ///
    /// `wo`, `wh`, `wu`, `wb` and the rest have counted as live since stage 2, because
    /// `Command::Open`/`Back`/`Navigate` are live whatever their flags say and the dispatcher folded
    /// `-w` into `-t`. So this number cannot show that they were fixed — only the run against the
    /// real browser can, and it is in the report. What the number *can* show is `gD` and `U`.
    #[test]
    fn the_bindings_a_second_window_turned_on() {
        for keys in [
            "gD", // tab-give
            "U",  // undo -w
        ] {
            let (_, _, cmd) = DEFAULT_BINDINGS
                .iter()
                .find(|(mode, k, _)| *mode == "normal" && *k == keys)
                .unwrap_or_else(|| panic!("no default binding for {keys}"));
            let parsed = commands::parse(cmd).expect("a default binding must parse");
            assert!(is_live(&parsed), "{keys} -> {cmd:?} is still inert");
        }

        // A window id is an argument, and a count overrides it — `commands.py:475`. Parsing it is
        // what stops `:tab-give 1` detaching into a new window instead of moving to window 1.
        assert_eq!(
            commands::parse("tab-give 1").unwrap(),
            Command::TabGive { win_id: Some(1) }
        );
        assert!(commands::parse("tab-give nowhere").is_err());

        // `;R` was the one hint binding a second window did not turn on, because `Target::Window`
        // still opened a foreground tab in `hints.rs`. That line is a `window::open` now, so this
        // is the third binding the second window is worth — see `the_binding_a_window_hint_turned_on`.
        assert!(is_live(&commands::parse("hint --rapid links window").unwrap()));
    }
// --- end src/window.rs -----------------------------------------------------

// --- src/find.rs + src/navigate.rs ---------------------------------------------------------------
    /// The eleven bindings this milestone made live, named one by one. A total is not enough to
    /// notice that `[[` went live and `{{` did not.
    #[test]
    fn the_bindings_search_and_navigate_turned_on() {
        for keys in [
            "n", "N",  // search-next, search-prev
            "<Escape>", // clear-keychain ;; search ;; fullscreen --leave — held back by `search`
            "[[", "]]", "{{", "}}", // navigate prev/next, in place and in a tab
            "gu", "gU", // navigate up
            "<Ctrl-A>", "<Ctrl-X>", // navigate increment/decrement
        ] {
            let (_, _, cmd) = DEFAULT_BINDINGS
                .iter()
                .find(|(mode, k, _)| *mode == "normal" && *k == keys)
                .unwrap_or_else(|| panic!("no default binding for {keys}"));
            let parsed = commands::parse(cmd).expect("a default binding must parse");
            assert!(is_live(&parsed), "{keys} -> {cmd:?} is still inert");
        }
    }
// --- end src/find.rs + src/navigate.rs ------------------------------------------------------------

// --- src/completers.rs ---------------------------------------------------------------------
    /// The ten command-mode bindings the completion turned on, named one by one — a total is not
    /// enough to notice that `<Tab>` went live and `<Shift-Tab>` did not.
    #[test]
    fn the_bindings_the_completion_turned_on() {
        for keys in [
            "<Tab>", "<Shift-Tab>",             // next, prev
            "<Ctrl-Tab>", "<Ctrl-Shift-Tab>",   // next-category, prev-category
            "<PgDown>", "<PgUp>",               // next-page, prev-page
            "<Up>", "<Down>",                   // the same two, --history
            "<Ctrl-D>", "<Shift-Delete>",       // completion-item-del
        ] {
            let (_, _, cmd) = DEFAULT_BINDINGS
                .iter()
                .find(|(mode, k, _)| *mode == "command" && *k == keys)
                .unwrap_or_else(|| panic!("no command-mode binding for {keys}"));
            let parsed = commands::parse(cmd).expect("a default binding must parse");
            assert!(is_live(&parsed), "{keys} -> {cmd:?} is still inert");
        }
        // Both live now: `app.rs` installs `clip::yank_plain` as the completion's clipboard.
        assert!(is_live(&commands::parse("completion-item-yank").unwrap()));
        assert!(is_live(&commands::parse("completion-item-yank --sel").unwrap()));
    }
// --- end src/completers.rs -----------------------------------------------------------------

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
