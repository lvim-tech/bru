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

// --- src/session.rs --------------------------------------------------------
        Command::TabPin => crate::tabs::toggle_pin(state),
        Command::TabMute => crate::tabs::toggle_mute(state),
        // `gD`. There is one window, and a tab cannot be given to a window that does not exist —
        // see the report: this needs `window_create_top_level` a second time, a `Vec<Window>` in
        // `BruState` where there is one `Option<Window>`, and a rule for which window a key
        // belongs to. Faking it by cloning the tab would lose the original's history and look
        // like a bug rather than a gap.
        Command::TabGive => {}

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
            crate::open::open(state, browser, url.as_deref(), *tab || *window, *bg)
// --- end src/clip.rs -------------------------------------------------------
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
        // window, so `close` and `quit` do the same thing.
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
            close_window(state);
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
                crate::caret::on_mode_change(browser, before, now);
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
                crate::caret::on_mode_change(browser, before, now);
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
        // `<Return>` in hint mode. Labels are prefix-free, so an exact match has already followed
        // itself by the time this could run — it exists because the binding does.
        Command::HintFollow => {}

// --- src/downloads.rs --------------------------------------------------------------------------
        // `gd`, `ad`, `cd` and the four `:download-*` commands. The count means the same thing in
        // all of them — which download, 1-based, with none meaning the last — so it is passed
        // through rather than turned into a repeat: `2ad` cancels download 2, it does not cancel
        // twice.
        Command::Download { url } => crate::downloads::start(state, browser, url.as_deref()),
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

        // `[[`, `]]`, `{{`, `}}`, `gu`, `gU`, `<Ctrl-A>`, `<Ctrl-X>`. `-w` folds into `-t`, as it
        // does for `open`: bru has one window.
        Command::Navigate { to, tab, bg, window } => {
            crate::navigate::navigate(state, browser, *to, *tab || *window, *bg, count)
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
            crate::history::quickmark_load(state, browser, name.as_deref(), *tab || *window, *bg)
        }
        Command::QuickmarkDel { name } => crate::history::quickmark_del(state, name.as_deref()),
        Command::BookmarkAdd { url, title, toggle } => {
            crate::history::bookmark_add(state, url.as_deref(), title.as_deref(), *toggle)
        }
        Command::BookmarkLoad { url, tab, bg, window, delete } => crate::history::bookmark_load(
            state,
            browser,
            url.as_deref(),
            *tab || *window,
            *bg,
            *delete,
        ),
        Command::BookmarkDel { url } => crate::history::bookmark_del(state, url.as_deref()),
        Command::BookmarkList { jump, bg } => {
            crate::history::bookmark_list(state, browser, *jump, *bg)
        }
        Command::History { bg } => crate::history::show(state, browser, *bg),
// --- end src/history.rs ----------------------------------------------------

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
// --- end src/settings.rs ---------------------------------------------------

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

        // Nothing to do, and that is the point: `nop` exists to shadow a Chromium default, and
        // clear-keychain is already done by the parser reporting the key.
        Command::Nop | Command::ClearKeychain => {}

// --- adblock ---------------------------------------------------------------------------------
        // None of the three is bound to a key, in qutebrowser or here: they are typed, rarely, and
        // `:adblock-update` in particular is the one thing in bru that reaches the network of its
        // own accord — it should take a decision, not a keystroke.
        Command::AdblockUpdate => crate::adblock::update(),
        Command::AdblockToggle => {
            let on = crate::adblock::toggle();
            eprintln!("bru[adblock]: blocking {}", if on { "on" } else { "off" });
        }
        Command::AdblockInfo => {
            eprintln!("bru[adblock]: {}", crate::adblock::info(browser.identifier()));
        }
// --- end adblock -----------------------------------------------------------------------------

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

// --- src/session.rs --------------------------------------------------------
        Command::TabPin | Command::TabMute => true,
        // `gD` needs a second window to give the tab to, and bru has one. Inert on purpose.
        Command::TabGive => false,
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
        // All six act. `gd`, `ad` and `cd` are the three default bindings this turns on; the other
        // three are `:` commands qutebrowser binds to nothing either.
        Command::Download { .. }
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
// --- end src/settings.rs ---------------------------------------------------

// --- src/completers.rs ---------------------------------------------------------------------
        Command::CompletionItemFocus { .. } | Command::CompletionItemDel => true,
        // Bound and reachable, and it says what it would have copied — but there is no clipboard
        // in bru yet, so claiming it as live would be claiming `<Ctrl-C>` copies something.
        // `clip::yank_plain` is installed at startup, so `<Ctrl-C>` copies the selected cell.
        Command::CompletionItemYank { .. } => true,
// --- end src/completers.rs -----------------------------------------------------------------

// --- src/hints.rs -----------------------------------------------------------------------
        // Three targets and one combination are inert on purpose, and each for a different
        // reason. Raise this when the reason goes away, in the commit that removes it.
        //
        // - `yank` / `yank-primary` (`;y`, `;Y`) collect the URL and have nowhere to put it:
        //   the clipboard is another workstream's, and `hints::Clipboard` is the whole of what
        //   this one needs from it. Live the moment `hints::install_clipboard` is called.
        // - `download` (`;d`) is the same shape against `hints::Downloads`.
        // - `--rapid` with `window` (`;R`) is refused, as qutebrowser refuses `--rapid tab-fg`:
        //   bru has one window, so `window` *is* a foreground tab, and a rapid session cannot
        //   survive the tab it is drawn on being switched away from.
        Command::Hint { target, rapid, .. } => {
            use crate::commands::HintTarget;
            match target {
                // Introduced to `clip.rs` and `downloads.rs` at startup — `app.rs` installs both.
                HintTarget::Yank | HintTarget::YankPrimary | HintTarget::Download => true,
                HintTarget::Window | HintTarget::TabFg | HintTarget::Fill(_) => !rapid,
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

        // Bound, reachable, and deliberately a no-op — see the arm in `run`.
        Command::HintFollow => false,

        // --- src/spawn.rs, src/editor.rs ----------------------------------------------------
        Command::Spawn { .. } => true,
        Command::EditText | Command::InsertText { .. } | Command::FakeKey { .. } => true,
        // --- end src/spawn.rs, src/editor.rs ------------------------------------------------

        Command::Nop | Command::ClearKeychain => true,

// --- adblock ---------------------------------------------------------------------------------
        // Live, and not part of the default-binding count: qutebrowser binds none of them either.
        Command::AdblockUpdate | Command::AdblockToggle | Command::AdblockInfo => true,
// --- end adblock -----------------------------------------------------------------------------

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

/// Close the one window, which is bru's whole teardown.
fn close_window(state: &SharedState) {
    let window = state.lock().expect("state mutex poisoned").window();
    if let Some(window) = window {
        window.close();
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
            run(&state, &mut browser, &command, self.count);
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
        // `register:` (3991).
        assert_eq!(DEFAULT_BINDINGS.len(), 262);
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
        // Raise this when a milestone raises the number, never to make a failing build pass.
        assert_eq!(live, 241, "the live-binding count moved");
    }

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
        // The two spellings that need a prompt or a page serialiser bru has not got.
        assert!(!is_live(&commands::parse("download --mhtml").unwrap()));
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
        // `gD` is tab-give, and bru has one window. It parses and stays inert deliberately.
        assert_eq!(commands::parse("tab-give").unwrap(), Command::TabGive);
        assert!(!is_live(&commands::parse("tab-give").unwrap()));

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
            // `set` with no option means qute://settings, and bru has no settings page
            "Ss",
        ];
        let command_for = |keys: &str| {
            DEFAULT_BINDINGS
                .iter()
                .find(|(mode, k, _)| *mode == "normal" && *k == keys)
                .unwrap_or_else(|| panic!("no default binding for {keys}"))
                .2
        };

        for keys in live_now {
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
        assert_eq!(live_now.len() + still_inert.len(), 25);
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

        // And a bare `:set` — the `Ss` binding — is inert either way: it means qute://settings.
        assert!(!is_live(&commands::parse("set").unwrap()));
    }
// --- end src/settings.rs ---------------------------------------------------

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
        // The fourth stays inert, and its reason is structural rather than a missing module: bru
        // has one window, so `window` *is* a foreground tab, and a rapid session cannot survive
        // the tab it is drawn on being switched away from. qutebrowser refuses the same pair.
        assert!(!is_live(&commands::parse("hint --rapid links window").unwrap()));
        // …but the same targets without `--rapid`, and `--rapid` with a target that survives it,
        // are live. Otherwise the four above would pass with the whole feature switched off.
        assert!(is_live(&commands::parse("hint links window").unwrap()));
        assert!(is_live(&commands::parse("hint --rapid links tab-bg").unwrap()));
        assert!(is_live(&commands::parse("hint --rapid all hover").unwrap()));
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
        // `U` is `undo -w`, and bru has one window: it parses and stays inert deliberately.
        assert!(!is_live(&commands::parse("undo -w").unwrap()));
    }

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
