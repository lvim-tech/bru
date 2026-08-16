//! The twenty-two commands qutebrowser has that bru had no arm for.
//!
//! Named after `misc/utilcmds.py`, which holds seven of them; the rest come from
//! `browser/commands.py`, `components/misccommands.py`, `browser/downloads.py`, `browser/history.py`
//! and `mainwindow/statusbar/command.py`. Every name, flag and argument shape is transcribed from
//! there rather than invented — DESIGN.md settles that the command names stay identical, and a
//! command that takes different arguments under the same name is worse than one that is missing.
//!
//! **None of the twenty-two is bound to a key by qutebrowser's defaults**, and so none of them
//! raises `exec::how_many_default_bindings_are_live`. The one that comes closest is `tab-select`:
//! `gt` is `cmd-set-text -s :tab-select`, which prefills the command line and was already live —
//! what changes is that what you type after it now does something.
//!
//! ## What is here and what is somewhere else
//!
//! The bookkeeping and the argument handling are here; anything that needs a module's private state
//! is a call into that module, in one fenced line each:
//!
//! | | |
//! |---|---|
//! | `download-remove` | `downloads::remove` — the list is that module's |
//! | `history-clear` | `data::Data::clear_history` |
//! | `quickmarks-reload`, `bookmarks-reload` | `data::Data::reload_marks` |
//! | `clear-messages`, `messages` | `message::clear`, `message::logged` |
//! | `edit-url`, `edit-command` | `editor::edit_externally` — the same `$EDITOR` run `edit-text` uses |
//! | `process` | `spawn` reports every child it starts to [`process_started`] |
//!
//! ## The three that run other commands
//!
//! `later`, `repeat` and `run-with-count` carry a command of their own, and qutebrowser registers
//! all three `no_cmd_split=True` (`utilcmds.py:28,60,82`) so that the `;;` in
//! `:later 1s tab-close ;; tab-close` belongs to the carried command. bru parses the carried command
//! **at parse time** rather than keeping the string: a binding is parsed once at startup, so a typo
//! inside one is reported then instead of a second after the key is pressed.
//!
//! ## Two DevTools calls, and why the ids start high
//!
//! `screenshot` is `Page.captureScreenshot`, the same protocol `downloads.rs` reaches for
//! `download --mhtml`. A `DevToolsMessageObserver` sees **every** result in the process, so the two
//! must not use the same message ids: `downloads::next_message_id` counts up from 1, and this counts
//! up from [`FIRST_MESSAGE_ID`]. Without that, a screenshot's answer would be taken for a snapshot's
//! and written into an MHTML file.

use cef::*;
use std::path::PathBuf;
use std::sync::Mutex;

use crate::commands::{ClickTarget, ElementFilter, ProcessAction};
// This module had a copy of its own, and it was the copy that still sliced the `str` — see
// `ipc::percent_decode`.
use crate::ipc::percent_decode;
use crate::tabs::SharedState;

/// `BRU_DEBUG_CMDS=1` traces what these commands resolved to, which is the only way to tell a
/// `click-element` that found nothing from one that found something and clicked the wrong place.
fn debug(text: &str) {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("BRU_DEBUG_CMDS").is_some()) {
        eprintln!("bru[cmds]: {text}");
    }
}

// ------------------------------------------------------------------------------------------------
// tab-select, tab-take — a tab named across every window
// ------------------------------------------------------------------------------------------------

/// One open tab, anywhere. What `[win-id/]index` and a title/URL fragment both resolve to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabRef {
    pub window: u32,
    /// 0-based, as `tabs::select_in` counts. The spelling the user types is 1-based.
    pub index: usize,
    pub url: String,
    pub title: String,
}

/// Every tab in every window, in window order — the rows `miscmodels._tabs` builds
/// (`miscmodels.py:91-155`), without the renderer pid, which CEF does not offer per browser.
pub fn all_tabs() -> Vec<TabRef> {
    let Some(state) = crate::state::BruState::instance() else {
        return Vec::new();
    };
    let Ok(state) = state.lock() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for window in state.window_ids() {
        for index in 0..state.tab_count_in(window) {
            out.push(TabRef {
                window,
                index,
                url: state.tab_url_in(window, index).unwrap_or_default(),
                title: state.tab_title_in(window, index).unwrap_or_default(),
            });
        }
    }
    out
}

/// `[win-id/]index`, or a substring of a title or a URL — `_resolve_tab_index`
/// (`commands.py:895-935`).
///
/// The two halves are qutebrowser's, in its order: if every `/`-separated part is a number it is an
/// address, and otherwise it is a pattern run through the same match the completion uses, whose
/// first row wins. So `:tab-select 2` is the second tab of this window, `:tab-select 1/3` is the
/// third of window 1, and `:tab-select rust` is whichever tab says rust.
///
/// `here` is the window an unqualified index belongs to.
pub fn resolve_tab(tabs: &[TabRef], here: u32, index: &str) -> Result<TabRef, String> {
    let parts: Vec<&str> = index.splitn(2, '/').collect();
    if !parts.iter().all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit())) {
        let found = matching_tabs(tabs, index);
        let Some(first) = found.first() else {
            return Err(format!("No matching tab for: {index}"));
        };
        return Ok((*first).clone());
    }

    let (window, wanted) = match parts.as_slice() {
        [window, index] => (
            window.parse::<u32>().map_err(|_| format!("no window {window}"))?,
            index.parse::<usize>().map_err(|_| format!("no tab {index}"))?,
        ),
        [index] => (here, index.parse::<usize>().map_err(|_| format!("no tab {index}"))?),
        _ => unreachable!("splitn(2) gives one part or two"),
    };
    if !tabs.iter().any(|tab| tab.window == window) {
        return Err(format!("There's no window with id {window}!"));
    }
    // 1-based, and `0` is not a tab — `_resolve_tab_index` refuses it with the same words.
    tabs.iter()
        .find(|tab| tab.window == window && tab.index + 1 == wanted)
        .cloned()
        .ok_or_else(|| format!("There's no tab with index {wanted}!"))
}

/// The tabs a pattern names, in the order they would appear in the completion.
///
/// `listcategory.ListCategory`'s filter (`completion/models/listcategory.py:44-60`): the pattern is
/// split on whitespace and every word has to appear, case-insensitively, in one of the columns.
fn matching_tabs<'a>(tabs: &'a [TabRef], pattern: &str) -> Vec<&'a TabRef> {
    let words: Vec<String> = pattern.split_whitespace().map(|w| w.to_lowercase()).collect();
    tabs.iter()
        .filter(|tab| {
            let haystack = format!("{} {}", tab.url, tab.title).to_lowercase();
            words.iter().all(|word| haystack.contains(word))
        })
        .collect()
}

/// `tab-select [[win-id/]index]` — what `gt` prefills.
///
/// With neither an index nor a count qutebrowser opens `qute://tabs/`; bru has no such page and does
/// not pretend to. It says which spellings there are, which is the one thing a bare `:tab-select`
/// can usefully do.
pub fn tab_select(state: &SharedState, index: Option<&str>, count: Option<u32>) {
    // "If both index and count are given, use count" — `commands.py:962`.
    let spelling = match count {
        Some(count) => count.to_string(),
        None => match index {
            Some(index) => index.to_string(),
            None => {
                crate::message::error(
                    "tab-select: name a tab — an index, a window/index, or part of a title",
                );
                return;
            }
        },
    };

    let here = current_window(state);
    let tabs = all_tabs();
    let found = match resolve_tab(&tabs, here, &spelling) {
        Ok(found) => found,
        Err(problem) => {
            crate::message::error(&format!("tab-select: {problem}"));
            return;
        }
    };
    debug(&format!("tab-select {spelling:?} -> window {} tab {}", found.window, found.index));

    // The window first, then the tab in it: `tab_select` raises the window before setting the
    // current widget (`commands.py:971-973`), and bru's `select_in` does not raise one.
    if found.window != here {
        crate::tabs::focus(state, found.window);
    }
    crate::tabs::select_in(state, found.window, found.index);
}

/// `tab-take [win-id/]index [--keep]` — the other end of `gD`.
///
/// qutebrowser opens the tab's URL here and closes it there (`commands.py:435-457`), which loses the
/// tab's navigation list. bru **re-parents the view** instead, exactly as `:tab-give` does — the same
/// browser, the same history, so `H` still works on the tab that arrived. `--keep` is the spelling
/// that does copy the URL, because that is what keeping the old tab means.
pub fn tab_take(state: &SharedState, browser: &mut Browser, index: &str, keep: bool) {
    let here = current_window(state);
    let tabs = all_tabs();
    let found = match resolve_tab(&tabs, here, index) {
        Ok(found) => found,
        Err(problem) => {
            crate::message::error(&format!("tab-take: {problem}"));
            return;
        }
    };
    if found.window == here {
        crate::message::error("tab-take: that tab is already in this window");
        return;
    }
    debug(&format!("tab-take {index:?} -> window {} tab {}", found.window, found.index));

    if keep {
        crate::open::open(state, browser, Some(&found.url), true, false);
        return;
    }

    // `give_tab` moves the *current* window's showing tab, so the tab being taken is made both:
    // its window is focused and it is selected there, and the give hands it back. Two calls rather
    // than a second re-parenting path, because the tricky half — holding the view's last reference
    // across `remove_child_view` — is already written once and measured.
    crate::tabs::focus(state, found.window);
    crate::tabs::select_in(state, found.window, found.index);
    crate::tabs::give_tab(state, Some(here));
}

/// `window-only` — close every window except this one (`utilcmds.py:237-248`).
pub fn window_only(state: &SharedState) {
    let (here, others) = {
        let guard = state.lock().expect("state mutex poisoned");
        (guard.current_window_id(), guard.window_ids())
    };
    let Some(here) = here else {
        return;
    };
    let mut closed = 0;
    for window in others {
        if window != here {
            crate::window::close(state, window);
            closed += 1;
        }
    }
    match closed {
        0 => crate::message::info("window-only: this is the only window"),
        1 => crate::message::info("Closed 1 window"),
        n => crate::message::info(&format!("Closed {n} windows")),
    }
}

fn current_window(state: &SharedState) -> u32 {
    state
        .lock()
        .expect("state mutex poisoned")
        .current_window_id()
        .unwrap_or(0)
}

// ------------------------------------------------------------------------------------------------
// The marks and the history
// ------------------------------------------------------------------------------------------------

/// `quickmark-add <url> <name>` (`urlmarks.py:159-190`).
///
/// The one quickmark command that takes the URL as well as the name, and therefore the only one that
/// can bookmark a page that is not showing. qutebrowser asks before overwriting an existing name;
/// bru's `Data::quickmark_save` overwrites and says which it did, the same as `:quickmark-save`.
pub fn quickmark_add(url: &str, name: &str) {
    // qutebrowser's two errors, word for word (`urlmarks.py:172-178`).
    if name.trim().is_empty() {
        crate::message::error("Can't set mark with empty name!");
        return;
    }
    if url.trim().is_empty() {
        crate::message::error("Can't set mark with empty URL!");
        return;
    }
    // Through the same fuzzy parse `:bookmark-add <url>` uses, so `:quickmark-add example.com ex`
    // stores a URL rather than a word.
    let Some(target) = crate::open::decide(url, &crate::open::engines()) else {
        crate::message::error(&format!("quickmark-add: nothing to save in {url:?}"));
        return;
    };
    let target = target.url().to_string();

    match with_data(|data| data.quickmark_save(name, &target)) {
        Some(Ok(replaced)) => crate::message::info(&format!(
            "{} quickmark {name} -> {target}",
            if replaced { "Replaced" } else { "Added" }
        )),
        Some(Err(problem)) => crate::message::error(&format!("quickmark-add: {problem}")),
        None => {}
    }
}

/// `quickmarks-reload` / `bookmarks-reload` (`commands.py:1190-1196`, `:1361-1367`).
///
/// Both re-read their file from disk, which is what makes editing `~/.local/share/bru/quickmarks` in
/// an editor a thing a running bru notices. `which` is the word the message uses.
pub fn reload_marks(which: Marks) {
    match with_data(|data| data.reload_marks(which)) {
        Some(Ok(count)) => crate::message::info(&format!("{} reloaded ({count}).", which.name())),
        Some(Err(problem)) => {
            crate::message::error(&format!("{}-reload: {problem}", which.file()))
        }
        None => {}
    }
}

/// Which of the two mark files a command means. In `commands.rs`'s vocabulary rather than a `bool`,
/// because `reload_marks(true)` at a call site says nothing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Marks {
    Quickmarks,
    Bookmarks,
}

impl Marks {
    fn name(self) -> &'static str {
        match self {
            Marks::Quickmarks => "Quickmarks",
            Marks::Bookmarks => "Bookmarks",
        }
    }

    /// The file's own name, which is also the command's prefix.
    pub fn file(self) -> &'static str {
        match self {
            Marks::Quickmarks => "quickmarks",
            Marks::Bookmarks => "bookmarks",
        }
    }
}

/// `history-clear [--force]` (`history.py:437-452`).
///
/// Without `--force` qutebrowser asks, through `message.confirm_async`, and bru asks the same
/// question in the same place — `prompt.rs` has yes/no questions and this is exactly what they are
/// for. The wording is qutebrowser's title.
///
/// **Only bru's own visit log and completion table.** Not cookies, not a tab's back/forward list,
/// not Chromium's cache: those live in the profile directory and `--private` is what covers them.
pub fn history_clear(state: &SharedState, force: bool) {
    if force {
        clear_history_now();
        return;
    }
    let window = current_window(state);
    crate::prompt::ask(
        window,
        crate::prompt::Question::new(
            crate::prompt::Kind::YesNo,
            "Clear all browsing history?",
            |answer| {
                if answer == crate::prompt::Answer::Yes {
                    clear_history_now();
                }
            },
        )
        .saying("Every page bru has recorded, and the completion built from it. Quickmarks, bookmarks and sessions are untouched.")
        .default_answer(false),
    );
}

fn clear_history_now() {
    match with_data(|data| data.clear_history()) {
        Some(Ok(removed)) => crate::message::info(&format!("Cleared {removed} history entries")),
        Some(Err(problem)) => crate::message::error(&format!("history-clear: {problem}")),
        None => {}
    }
}

fn with_data<T>(f: impl FnOnce(&mut crate::data::Data) -> T) -> Option<T> {
    let data = crate::data::instance()?;
    let mut guard = data.lock().ok()?;
    Some(f(&mut guard))
}

// ------------------------------------------------------------------------------------------------
// The page, scrolled and clicked
// ------------------------------------------------------------------------------------------------

/// `scroll-to-anchor <name>` (`scrollcommands.py:101-110`).
///
/// **Not a scroll, and that is not a shortcut taken.** qutebrowser's own implementation for the
/// QtWebEngine backend is `url.setFragment(name); load_url(url)` (`webenginetab.py:566-569`) — the
/// anchor is a navigation, and the engine does the scrolling as part of it. So this leaves
/// `send_mouse_wheel_event` alone for the reason DESIGN.md gives: there is no wheel event that could
/// find a named anchor, and asking the page to `scrollIntoView` would put JavaScript on a path the
/// engine already owns.
pub fn scroll_to_anchor(browser: &mut Browser, name: &str) {
    let url = crate::ipc::current_url();
    if url.is_empty() {
        crate::message::error("scroll-to-anchor: no page");
        return;
    }
    // The fragment is replaced, not appended: `#a` then `#b` must go to b, and `…#a#b` is one
    // fragment named `a#b`.
    let base = url.split('#').next().unwrap_or(&url);
    let target = format!("{base}#{}", percent_encode_fragment(name));
    debug(&format!("scroll-to-anchor {name:?} -> {target}"));
    if let Some(frame) = browser.main_frame() {
        frame.load_url(Some(&CefString::from(target.as_str())));
    }
}

/// What may go in a fragment verbatim (RFC 3986's `fragment`), with everything else percent-encoded.
/// An anchor with a space in it is what a heading id looks like on plenty of pages.
fn percent_encode_fragment(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for byte in name.bytes() {
        let keep = byte.is_ascii_alphanumeric()
            || b"-._~!$&'()*+,;=:@/?".contains(&byte)
            // Already-encoded input stays as it is, so `:scroll-to-anchor a%20b` works too.
            || byte == b'%';
        if keep {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// `click-element <filter> [value] [--target …] [--force-event] [--select-first]`
/// (`misccommands.py:228-300`).
///
/// The filter chooses the element and the target says what to do with it, which is the same split
/// `:hint` has — so the targets are `hints::Target`'s spellings and mean the same things. The click
/// itself is the one `hints.rs` sends: a real move, press and release at the element's centre, on
/// Chromium's own input path. `--force-event` is qutebrowser's escape hatch for an element that only
/// answers to a synthetic `.click()`, and it is the one case where the page's own event is used.
pub fn click_element(
    state: &SharedState,
    browser: &mut Browser,
    filter: ElementFilter,
    value: Option<&str>,
    target: ClickTarget,
    force_event: bool,
    select_first: bool,
) {
    let value = value.unwrap_or_default().to_string();
    let script = find_element_script(filter, &value, select_first);
    let state = state.clone();
    let mut answering = browser.clone();
    let what = describe(filter, &value);

    crate::editor::ask(browser, &script, move |answer| {
        debug(&format!("click-element {what} -> {answer:?}"));
        let found = match Found::parse(&answer) {
            Ok(found) => found,
            Err(problem) => {
                crate::message::error(&format!("{problem} {what}!"));
                return;
            }
        };
        match target {
            ClickTarget::Normal if force_event => {
                // The page's own `.click()`, on the element the search already found. Marked by the
                // same token so a second element cannot be clicked between the two calls.
                let script = format!("window.__bru_click && window.__bru_click({});", found.index);
                if let Some(frame) = answering.main_frame() {
                    frame.execute_java_script(Some(&CefString::from(script.as_str())), None, 0);
                }
            }
            ClickTarget::Normal => click(&mut answering, found.x, found.y),
            ClickTarget::Hover => hover(&mut answering, found.x, found.y),
            ClickTarget::TabBg | ClickTarget::TabFg | ClickTarget::Window => {
                if found.href.is_empty() {
                    crate::message::error("click-element: that element has no address to open");
                    return;
                }
                match target {
                    ClickTarget::Window => {
                        crate::window::open(&state, &found.href);
                    }
                    ClickTarget::TabFg => {
                        crate::tabs::new_tab(&state, &found.href, false);
                    }
                    _ => crate::tabs::new_tab(&state, &found.href, true),
                }
            }
        }
    });
}

/// What the page answered: where the element is, its address, and which of the matches it was.
struct Found {
    x: i32,
    y: i32,
    href: String,
    index: usize,
}

impl Found {
    /// `x,y,index,href`, or one of the two failures. Deliberately not JSON: the answer channel
    /// hands back one string, and a URL cannot contain a comma before the fourth field because
    /// nothing is read after it.
    fn parse(answer: &str) -> Result<Found, &'static str> {
        match answer {
            "" | "none" => return Err("No element found"),
            "many" => return Err("Multiple elements found"),
            _ => {}
        }
        let mut parts = answer.splitn(4, ',');
        let (Some(x), Some(y), Some(index)) = (parts.next(), parts.next(), parts.next()) else {
            return Err("The page answered nothing usable for");
        };
        let (Ok(x), Ok(y), Ok(index)) = (x.parse(), y.parse(), index.parse()) else {
            return Err("The page answered nothing usable for");
        };
        Ok(Found { x, y, index, href: parts.next().unwrap_or_default().to_string() })
    }
}

fn describe(filter: ElementFilter, value: &str) -> String {
    // `_FILTER_ERRORS` (`misccommands.py:220-225`), which is what makes "No element found with ID
    // "x"!" read as a sentence.
    match filter {
        ElementFilter::Id => format!("with ID \"{value}\""),
        ElementFilter::Css => format!("matching CSS selector \"{value}\""),
        ElementFilter::Focused => "with focus".to_string(),
        ElementFilter::Position => format!("at position {value}"),
    }
}

/// The script that finds the element and reports where it is.
///
/// It leaves `window.__bru_click` behind so that `--force-event` can click the *same* element
/// without searching again — a second search could find a different one on a page that is changing
/// under it.
fn find_element_script(filter: ElementFilter, value: &str, select_first: bool) -> String {
    let value = crate::ipc::json_escape(value);
    let find = match filter {
        ElementFilter::Id => format!("var m = document.getElementById(\"{value}\"); m = m ? [m] : [];"),
        ElementFilter::Css => {
            format!("var m = Array.prototype.slice.call(document.querySelectorAll(\"{value}\"));")
        }
        ElementFilter::Focused => "var m = document.activeElement ? [document.activeElement] : [];".to_string(),
        ElementFilter::Position => format!(
            "var p = \"{value}\".split(\",\"); var e = document.elementFromPoint(+p[0], +p[1]); var m = e ? [e] : [];"
        ),
    };
    let many = if select_first { "" } else { "if (m.length > 1) { return \"many\"; }" };
    format!(
        r#"(function () {{
    {find}
    if (!m.length) {{ return "none"; }}
    {many}
    var e = m[0];
    window.__bru_click = function (i) {{ var el = window.__bru_clicked; if (el) {{ el.click(); }} }};
    window.__bru_clicked = e;
    var r = e.getBoundingClientRect();
    var x = Math.round(r.left + r.width / 2);
    var y = Math.round(r.top + r.height / 2);
    var href = e.href || (e.getAttribute && e.getAttribute("href")) || "";
    return x + "," + y + ",0," + href;
}})();"#
    )
}

/// A real click, the way `hints.rs` sends one: a move first, because hover state is what a page's
/// own handlers look at, then press and release.
fn click(browser: &mut Browser, x: i32, y: i32) {
    // CSS pixels from the page's `getBoundingClientRect` into view coordinates — `exec::view_point`.
    let (x, y) = crate::exec::view_point(browser, x, y);
    // The same three ordered pieces as `hints::click`: arm the caret move in the page, wait for
    // the renderer's echo, then move-press-release and ask what the click landed on. See
    // `focus::click_through` for the ordering and the measured race behind it.
    crate::focus::click_through(browser, x, y);
}

fn hover(browser: &mut Browser, x: i32, y: i32) {
    let (x, y) = crate::exec::view_point(browser, x, y);
    if let Some(host) = browser.host() {
        host.send_mouse_move_event(Some(&MouseEvent { x, y, modifiers: 0 }), 0);
    }
}

// ------------------------------------------------------------------------------------------------
// jseval
// ------------------------------------------------------------------------------------------------

/// How much of a result reaches the bar. qutebrowser's own cap (`commands.py:1700-1710`).
const JSEVAL_MAX: usize = 5000;

/// `jseval [--file] [--url] [--quiet] <js>` (`commands.py:1714-1780`).
///
/// The value comes back through `editor::ask`, which is the channel `edit-text` already uses: a
/// `ProcessMessage` to the renderer, `V8Context::eval` between `enter` and `exit`, and another
/// `ProcessMessage` back. **No third mechanism, and none is needed** — what this adds is the
/// wrapper, which is what makes an exception readable: `eval` through that channel answers with the
/// empty string both for `undefined` and for a throw, and "no output" is the wrong thing to say
/// about a `ReferenceError`.
///
/// The user's code is run by an *indirect* `eval` so that `var x = 1` declares a global, which is
/// what typing it in a console does and what a `:jseval` is usually for.
pub fn jseval(browser: &mut Browser, code: &str, quiet: bool) {
    let script = format!(
        r#"(function () {{
    var out;
    try {{ out = (0, eval)("{}"); }}
    catch (e) {{ return "!" + (e && e.stack ? e.stack : String(e)); }}
    if (out === undefined) {{ return ""; }}
    try {{ return "=" + JSON.stringify(out); }}
    catch (e) {{ return "=" + String(out); }}
}})();"#,
        crate::ipc::json_escape(code)
    );
    crate::editor::ask(browser, &script, move |answer| {
        debug(&format!("jseval -> {answer:?}"));
        match answer.chars().next() {
            // A throw, whatever `--quiet` says: qutebrowser shows JS errors through the page's
            // console handler and bru's console policy drops a page's own errors, so this is the
            // only place a `:jseval` typo can be reported at all.
            Some('!') => crate::message::error(&trim(&answer[1..])),
            _ if quiet => {}
            Some('=') => crate::message::info(&trim(&answer[1..])),
            // The empty answer: `undefined`, and also a renderer that never ran the code (trap 16).
            // qutebrowser's own words for the first (`commands.py:1706`).
            _ => crate::message::info("No output or error"),
        }
    });
}

fn trim(text: &str) -> String {
    if text.chars().count() <= JSEVAL_MAX {
        return text.to_string();
    }
    let head: String = text.chars().take(JSEVAL_MAX).collect();
    format!("{head} [...trimmed...]")
}

/// `--file`: a path, absolute or relative to `~/.local/share/bru/js/`, which is where qutebrowser
/// looks too (`commands.py:1760-1768`, `standarddir.data()/js`).
pub fn jseval_file(path: &str) -> Result<String, String> {
    let expanded = crate::spawn::expand_user(path);
    let full = if expanded.is_absolute() {
        expanded
    } else {
        match crate::data::data_dir() {
            Some(dir) => dir.join("js").join(expanded),
            None => return Err("there is no data directory to look for it in".to_string()),
        }
    };
    std::fs::read_to_string(&full).map_err(|e| format!("{}: {e}", full.display()))
}

/// `--url`: a `javascript:…` URL, whose body is percent-encoded.
pub fn jseval_url(url: &str) -> Result<String, String> {
    let body = url
        .strip_prefix("javascript:")
        .ok_or_else(|| format!("{url:?} is not a javascript: URL"))?;
    Ok(percent_decode(body))
}

// ------------------------------------------------------------------------------------------------
// The editor: edit-url and edit-command
// ------------------------------------------------------------------------------------------------

/// `edit-url [-t|-b|-w] [-p] [-r] [url]` (`commands.py:1809-1840`).
///
/// The editor runs on a thread of its own — it is a program that will sit there for minutes — and
/// the open happens back on the UI thread, because opening is a navigation and a navigation started
/// from anywhere else is CEF-NOTES trap 12 waiting to happen.
///
/// qutebrowser opens only *if the text changed* (`_open_if_changed`), and so does this: quitting the
/// editor without saving is how you cancel.
pub fn edit_url(state: &SharedState, url: Option<&str>, tab: bool, bg: bool, window: bool) {
    let old = match url {
        Some(url) => url.to_string(),
        None => crate::ipc::current_url(),
    };
    if old.is_empty() {
        crate::message::error("edit-url: no page, and no URL given");
        return;
    }
    let _ = state;
    std::thread::spawn(move || match crate::editor::edit_externally(&old) {
        Ok(edited) => {
            let edited = edited.trim().to_string();
            if edited.is_empty() || edited == old {
                return;
            }
            let mut task = OpenEdited::new(edited, tab, bg, window);
            post_task(ThreadId::UI, Some(&mut task));
        }
        Err(problem) => crate::message::error(&format!("edit-url: {problem}")),
    });
}

wrap_task! {
    struct OpenEdited {
        url: String,
        tab: bool,
        bg: bool,
        window: bool,
    }

    impl Task {
        fn execute(&self) {
            debug_assert_ne!(currently_on(ThreadId::UI), 0);
            let Some(state) = crate::state::BruState::instance() else {
                return;
            };
            if self.window {
                if let Some(target) = crate::open::resolve(Some(&self.url)) {
                    crate::window::open(&state, &target);
                }
                return;
            }
            let browser = state.lock().expect("state mutex poisoned").active_browser();
            let Some(mut browser) = browser else {
                return;
            };
            crate::open::open(&state, &mut browser, Some(&self.url), self.tab, self.bg);
        }
    }
}

/// `edit-command [--run]`, `cmd-edit` since 2.0 (`statusbar/command.py:200-221`).
///
/// The command line's own text goes to the editor and comes back. It has to start with one of
/// `:`, `/` or `?` — `modeparsers.STARTCHARS`, which `cmdline.rs` spells `STARTCHARS` too — because
/// what comes back is set with `cmd-set-text`, and a line with no prefix is not a command line.
pub fn edit_command(state: &SharedState, run: bool) {
    let window = current_window(state);
    let text = crate::cmdline::text_in(window);
    // Typed rather than bound, the line has already been accepted and cleared by the time this
    // runs; a colon is what the editor should then open on.
    let text = if text.is_empty() { ":".to_string() } else { text };

    std::thread::spawn(move || match crate::editor::edit_externally(&text) {
        Ok(edited) => {
            let edited = edited.trim_end_matches('\n').to_string();
            let mut task = SetEditedCommand::new(edited, run);
            post_task(ThreadId::UI, Some(&mut task));
        }
        Err(problem) => crate::message::error(&format!("edit-command: {problem}")),
    });
}

wrap_task! {
    struct SetEditedCommand {
        text: String,
        run: bool,
    }

    impl Task {
        fn execute(&self) {
            debug_assert_ne!(currently_on(ThreadId::UI), 0);
            let starts = self
                .text
                .chars()
                .next()
                .map(|c| crate::cmdline::STARTCHARS.contains(c))
                .unwrap_or(false);
            if !starts {
                // qutebrowser's own words (`command.py:212-214`).
                crate::message::error(&format!(
                    "command must start with one of {}",
                    crate::cmdline::STARTCHARS
                ));
                return;
            }
            crate::cmdline::cmd_set_text(&self.text, false, false, false, None);
            if self.run {
                crate::cmdline::command_accept(false);
            }
        }
    }
}

// ------------------------------------------------------------------------------------------------
// later / repeat / run-with-count
// ------------------------------------------------------------------------------------------------

/// `later <duration> <command>`, `cmd-later` since 2.0 (`utilcmds.py:28-57`).
///
/// The command is already parsed — see the module docs — so all that is left is a delayed task. It
/// runs against whatever tab is showing *then*, which is what a timer means.
pub fn later(ms: i64, command: &crate::commands::Command, count: Option<u32>) {
    debug(&format!("later {ms}ms {command:?}"));
    let mut task = LaterCommand::new(format!("{ms}"), command.clone(), count);
    post_delayed_task(ThreadId::UI, Some(&mut task), ms);
}

wrap_task! {
    struct LaterCommand {
        after: String,
        command: crate::commands::Command,
        count: Option<u32>,
    }

    impl Task {
        fn execute(&self) {
            debug_assert_ne!(currently_on(ThreadId::UI), 0);
            let Some(state) = crate::state::BruState::instance() else {
                return;
            };
            let browser = state.lock().expect("state mutex poisoned").active_browser();
            let Some(mut browser) = browser else {
                crate::message::error("later: there is no tab left to run it against");
                return;
            };
            debug(&format!("later {}ms fired", self.after));
            crate::exec::run(&state, &mut browser, &self.command, self.count);
        }
    }
}

// ------------------------------------------------------------------------------------------------
// restart
// ------------------------------------------------------------------------------------------------

/// The session `:restart` leaves behind, and `--restore` picks up. qutebrowser's own name for it
/// (`quitter.py:291-300`).
const RESTART_SESSION: &str = "_restart";

/// `restart` — save the open tabs, start bru again, and go.
///
/// **The new process cannot simply be spawned.** CEF takes a process-singleton lock on the profile
/// directory (CEF-NOTES trap 10), so a second bru starting while this one still holds
/// `~/.local/share/bru/cef` either dies on `initialize` or, because `profile::choose` is careful,
/// quietly takes a scratch profile — and a restart that came back with different cookies and no
/// logins would be worse than one that failed. So the replacement waits for this pid to be gone
/// before it starts, which is what qutebrowser's `ipc.server.shutdown()` before `Popen` achieves by
/// another route.
///
/// The waiter is `sh`, which is on every machine this could run on and is what `:spawn` would use
/// anyway. Nothing is passed through a shell as text: the argument vector is handed to `sh -c` as
/// `"$@"`, so a `--user-data-dir` with a space in it survives.
pub fn restart(state: &SharedState) {
    match crate::session::save(state, RESTART_SESSION) {
        Ok(path) => debug(&format!("restart: session saved to {}", path.display())),
        Err(problem) => {
            crate::message::error(&format!("restart: could not save the session: {problem}"));
            return;
        }
    }

    let argv = restart_argv();
    debug(&format!("restart: {argv:?}"));
    let waiter = format!(
        "while kill -0 {pid} 2>/dev/null; do sleep 0.1; done; exec \"$@\"",
        pid = std::process::id()
    );
    let spawned = std::process::Command::new("sh")
        .arg("-c")
        .arg(&waiter)
        // `$0` for the `sh -c`; the real argv follows as `"$@"`.
        .arg("bru-restart")
        .args(&argv)
        .spawn();
    match spawned {
        Ok(child) => {
            crate::message::info(&format!("Restarting (pid {})", child.id()));
            crate::window::close_all(state);
        }
        Err(problem) => crate::message::error(&format!("restart: could not start sh: {problem}")),
    }
}

/// This process's own command line, with the switches that would fight the restore taken out and
/// `--restore` put in.
///
/// `/proc/self/exe` rather than `argv[0]`: bru may have been started through `$PATH` or a relative
/// path from a directory that no longer exists, and the executable is the one thing that is certain.
fn restart_argv() -> Vec<String> {
    let exe = std::fs::read_link("/proc/self/exe")
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| std::env::args().next().unwrap_or_else(|| "bru".to_string()));

    let mut argv = vec![exe];
    for arg in std::env::args().skip(1) {
        let name = arg.split('=').next().unwrap_or(&arg);
        // `--url` would open a second copy of the start page beside the restored tabs, and an old
        // `--restore` would restore the wrong session. Every other switch — `--user-data-dir`,
        // `--private`, `--enable-…` — is part of what this run *is* and has to survive.
        if matches!(name, "--url" | "--restore" | "--restore-history") {
            continue;
        }
        argv.push(arg);
    }
    argv.push(format!("--restore={RESTART_SESSION}"));
    argv
}

// ------------------------------------------------------------------------------------------------
// The processes :spawn started
// ------------------------------------------------------------------------------------------------

/// One child `:spawn` started. qutebrowser keeps a `GUIProcess` per pid (`guiprocess.py:20-30`);
/// this is the same list with the fields a `bru://chrome/process` page can show.
struct Proc {
    pid: u32,
    what: String,
    /// `None` while it is running, then the exit status as it printed.
    outcome: Option<String>,
    /// When it started, for the hour-long cleanup qutebrowser's docstring promises.
    started: std::time::Instant,
}

fn processes() -> &'static Mutex<Vec<Proc>> {
    static PROCESSES: Mutex<Vec<Proc>> = Mutex::new(Vec::new());
    &PROCESSES
}

/// The pid of the last process started, which is what a bare `:process` means.
static LAST_PID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Called by `spawn.rs` for every child it starts, whatever the flags were.
pub fn process_started(pid: u32, what: &str) {
    LAST_PID.store(pid, std::sync::atomic::Ordering::Relaxed);
    let Ok(mut list) = processes().lock() else {
        return;
    };
    // "processes with a successful exit get cleaned up after 1h" (`guiprocess.py:32`). A failure is
    // kept, because a failure is the one a person comes looking for.
    list.retain(|proc| {
        proc.outcome.as_deref().map(|status| !status.contains("exit status: 0")) != Some(false)
            || proc.started.elapsed() < std::time::Duration::from_secs(3600)
    });
    list.push(Proc { pid, what: what.to_string(), outcome: None, started: std::time::Instant::now() });
}

/// Called by `spawn.rs` when that child is reaped.
pub fn process_finished(pid: u32, outcome: &str) {
    let Ok(mut list) = processes().lock() else {
        return;
    };
    if let Some(proc) = list.iter_mut().find(|proc| proc.pid == pid) {
        proc.outcome = Some(outcome.to_string());
    }
}

/// `process [pid] [show|terminate|kill]` (`guiprocess.py:27-64`).
pub fn process(state: &SharedState, browser: &mut Browser, pid: Option<u32>, action: ProcessAction) {
    let pid = match pid.or_else(|| match LAST_PID.load(std::sync::atomic::Ordering::Relaxed) {
        0 => None,
        pid => Some(pid),
    }) {
        Some(pid) => pid,
        None => {
            crate::message::error("No process executed yet!");
            return;
        }
    };
    let known = processes()
        .lock()
        .map(|list| list.iter().any(|proc| proc.pid == pid))
        .unwrap_or(false);
    if !known {
        crate::message::error(&format!("No process found with pid {pid}"));
        return;
    }

    match action {
        ProcessAction::Show => {
            crate::open::open(state, browser, Some(&format!("{PROCESS_URL}/{pid}")), false, false)
        }
        // `kill(2)` through the program of the same name, for the same reason `yy` shells out to
        // `wl-copy`: it is one process for something typed by hand, and the alternative is a libc
        // dependency for two lines.
        ProcessAction::Terminate => signal(pid, "TERM"),
        ProcessAction::Kill => signal(pid, "KILL"),
    }
}

fn signal(pid: u32, name: &str) {
    match std::process::Command::new("kill")
        .arg(format!("-{name}"))
        .arg(pid.to_string())
        .status()
    {
        Ok(status) if status.success() => {
            crate::message::info(&format!("Sent SIG{name} to {pid}"))
        }
        Ok(status) => crate::message::error(&format!("kill -{name} {pid}: {status}")),
        Err(problem) => crate::message::error(&format!("kill -{name} {pid}: {problem}")),
    }
}

// ------------------------------------------------------------------------------------------------
// The three generated pages
// ------------------------------------------------------------------------------------------------

pub const VERSION_URL: &str = "bru://chrome/version";
pub const MESSAGES_URL: &str = "bru://chrome/messages";
pub const PROCESS_URL: &str = "bru://chrome/process";

/// `version` (`utilcmds.py:250-264`). qutebrowser opens `qute://version/` in a new tab and so does
/// this, with `bru://chrome/version`.
///
/// `--paste` is refused rather than accepted-and-ignored: it uploads the report to a pastebin, and
/// bru reaches the network of its own accord in exactly one place (`:adblock-update`).
pub fn version(state: &SharedState, browser: &mut Browser) {
    crate::open::open(state, browser, Some(VERSION_URL), true, false);
}

/// The `bru://chrome/version` page. Generated per request, like every other `bru://` page.
///
/// **Chromium's own version is filled in by the page, not by Rust.** CEF exposes no
/// "what Chromium is this" call to the browser process — only `api_version()`, which is the API
/// level the binary was compiled against — and the page is a real document in that engine, so
/// `navigator.userAgent` is both the honest answer and the cheap one.
pub fn version_page() -> String {
    let mut out = crate::history::chrome_head("bru — version", "version");
    out.push_str("<h1>bru</h1>\n");
    out.push_str(&format!(
        "<p class=\"summary\">bru {}, {} build.</p>\n",
        env!("CARGO_PKG_VERSION"),
        if cfg!(debug_assertions) { "debug" } else { "release" }
    ));

    out.push_str("<h2>Versions</h2>\n<table>\n");
    row(&mut out, "bru", env!("CARGO_PKG_VERSION"));
    row(&mut out, "cef crate", CEF_CRATE_VERSION);
    row(&mut out, "CEF API", &api_version().to_string());
    out.push_str(
        "<tr><td class=\"when\">Chromium</td><td class=\"what\" id=\"ua\">(asking the engine)</td><td class=\"where\"></td></tr>\n",
    );
    out.push_str("</table>\n");

    out.push_str("<h2>Where bru keeps things</h2>\n<table>\n");
    row(
        &mut out,
        "data",
        &crate::data::data_dir().map(|p| p.display().to_string()).unwrap_or_default(),
    );
    row(&mut out, "downloads", &crate::downloads::download_dir().display().to_string());
    row(&mut out, "config", &config_dir_display());
    for (i, dir) in crate::spawn::userscript_dirs().iter().enumerate() {
        row(
            &mut out,
            if i == 0 { "userscripts" } else { "" },
            &dir.display().to_string(),
        );
    }
    out.push_str("</table>\n");

    // The one line of script on any bru page, and it reads nothing it was not given: the engine's
    // own user agent, into a cell of this document.
    out.push_str(
        "<script>document.getElementById(\"ua\").textContent = navigator.userAgent;</script>\n",
    );
    out.push_str("</main>\n");
    out
}

/// The `cef` dependency's version, as `Cargo.toml` states it. Spelled here so the page can show it,
/// and checked against the file by `the_version_page_states_the_cef_version_cargo_builds_against`.
const CEF_CRATE_VERSION: &str = "151.2";

fn config_dir_display() -> String {
    // The same precedence `chrome::theme_path` uses, and it deliberately does not create anything:
    // `~/.config/bru/` is configer's and bru must not make it (ROUND5-CONTRACTS).
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").filter(|dir| !dir.is_empty()) {
        return PathBuf::from(dir).join("bru").display().to_string();
    }
    match std::env::var_os("HOME") {
        Some(home) => PathBuf::from(home).join(".config/bru").display().to_string(),
        None => String::new(),
    }
}

fn row(out: &mut String, name: &str, value: &str) {
    out.push_str(&format!(
        "<tr><td class=\"when\">{}</td><td class=\"what\">{}</td><td class=\"where\"></td></tr>\n",
        crate::history::chrome_escape(name),
        crate::history::chrome_escape(value),
    ));
}

/// `messages [-t|-b|-w] [--plain] [level]` (`commands.py:1497-1540`).
///
/// qutebrowser's `qute://log?level=…`; bru's `bru://chrome/messages`, built from the ring buffer
/// `message.rs` keeps.
pub fn messages(
    state: &SharedState,
    browser: &mut Browser,
    level: &str,
    plain: bool,
    tab: bool,
    bg: bool,
    window: bool,
) {
    let url = format!("{MESSAGES_URL}?level={level}{}", if plain { "&plain" } else { "" });
    if window {
        crate::window::open(state, &url);
        return;
    }
    crate::open::open(state, browser, Some(&url), tab, bg);
}

/// The `bru://chrome/messages` page, for the query string it was asked with.
///
/// **The times are formatted by the page.** bru has no date crate and SQLite is the only thing in
/// the build that knows this machine's timezone — and it is not available when the data directory
/// could not be opened. The document is running in Chromium, which knows; each row carries its epoch
/// seconds and one line of script turns them into local clock times.
pub fn messages_page(query: &str) -> String {
    let level = query_value(query, "level").unwrap_or_else(|| "info".to_string());
    let plain = query_has(query, "plain");
    let minimum = threshold(&level).unwrap_or(crate::message::Level::Info);
    let logged = crate::message::logged(minimum);

    let mut out = crate::history::chrome_head("bru — messages", "messages");
    out.push_str("<h1>Messages</h1>\n");
    out.push_str(&format!(
        "<p class=\"summary\">{} of {} kept, {} and above.</p>\n",
        logged.len(),
        crate::message::logged(crate::message::Level::Info).len(),
        crate::history::chrome_escape(&level),
    ));
    if logged.is_empty() {
        out.push_str("<p class=\"summary\">Nothing has been said yet.</p>\n</main>\n");
        return out;
    }

    if plain {
        // `--plain`: one line each, no table. The timestamps are still filled in by the script
        // below, which is why they are spans rather than text.
        out.push_str("<pre>\n");
        for entry in &logged {
            out.push_str(&format!(
                "<span class=\"at\" data-at=\"{}\"></span> {:<7} {}\n",
                entry.at,
                entry.level.name(),
                crate::history::chrome_escape(&entry.text),
            ));
        }
        out.push_str("</pre>\n");
    } else {
        out.push_str("<table>\n");
        for entry in &logged {
            out.push_str(&format!(
                "<tr><td class=\"when at\" data-at=\"{}\"></td><td class=\"what\">{}</td><td class=\"where\">{}</td></tr>\n",
                entry.at,
                crate::history::chrome_escape(&entry.text),
                entry.level.name(),
            ));
        }
        out.push_str("</table>\n");
    }

    out.push_str(
        r#"<script>
for (const el of document.querySelectorAll(".at")) {
    el.textContent = new Date(Number(el.dataset.at) * 1000).toLocaleTimeString();
}
</script>
"#,
    );
    out.push_str("</main>\n");
    out
}

/// The six names `:messages` takes (`log.LOG_LEVELS`), mapped onto the three bru has.
///
/// bru's messages carry no `debug` or `critical` level — `message.rs` has three and the theme has
/// colours for three — so `vdebug` and `debug` mean "everything there is" and `critical` means the
/// errors. A name outside the six is a parse error, which is where qutebrowser raises it too.
pub fn threshold(level: &str) -> Option<crate::message::Level> {
    Some(match level {
        "vdebug" | "debug" | "info" => crate::message::Level::Info,
        "warning" => crate::message::Level::Warning,
        "error" | "critical" => crate::message::Level::Error,
        _ => return None,
    })
}

/// The `bru://chrome/process[/<pid>]` page.
pub fn process_page(path: &str) -> String {
    let wanted: Option<u32> = path.trim_start_matches('/').parse().ok();
    let mut out = crate::history::chrome_head("bru — processes", "process");
    out.push_str("<h1>Processes</h1>\n");

    let Ok(list) = processes().lock() else {
        out.push_str("<p class=\"summary\">The process list could not be read.</p>\n</main>\n");
        return out;
    };
    let shown: Vec<&Proc> = list
        .iter()
        .filter(|proc| wanted.is_none_or(|pid| proc.pid == pid))
        .collect();
    out.push_str(&format!(
        "<p class=\"summary\">{} of {} started by <code>:spawn</code>.</p>\n",
        shown.len(),
        list.len()
    ));
    if shown.is_empty() {
        out.push_str("<p class=\"summary\">Nothing to show.</p>\n</main>\n");
        return out;
    }

    out.push_str("<table>\n");
    for proc in shown {
        out.push_str(&format!(
            "<tr><td class=\"when\">{}</td><td class=\"what\">{}</td><td class=\"where\">{}</td></tr>\n",
            proc.pid,
            crate::history::chrome_escape(&proc.what),
            crate::history::chrome_escape(proc.outcome.as_deref().unwrap_or("running")),
        ));
    }
    out.push_str("</table>\n</main>\n");
    out
}

/// `?level=info&plain` — one value out of a query string, with no dependency to parse it.
fn query_value(query: &str, name: &str) -> Option<String> {
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix(name)?.strip_prefix('='))
        .map(percent_decode)
}

fn query_has(query: &str, name: &str) -> bool {
    query.split('&').any(|pair| pair == name)
}

// ------------------------------------------------------------------------------------------------
// screenshot
// ------------------------------------------------------------------------------------------------

/// Where this module's DevTools message ids start. See the module docs: `downloads.rs` counts from 1
/// and one observer sees every result in the process.
const FIRST_MESSAGE_ID: i32 = 0x4000_0000;

fn next_message_id() -> i32 {
    static NEXT: Mutex<i32> = Mutex::new(FIRST_MESSAGE_ID);
    let mut next = NEXT.lock().expect("screenshot id mutex poisoned");
    let id = *next;
    *next = id.saturating_add(1);
    id
}

struct PendingShot {
    message_id: i32,
    path: PathBuf,
}

fn pending() -> &'static Mutex<Vec<PendingShot>> {
    static PENDING: Mutex<Vec<PendingShot>> = Mutex::new(Vec::new());
    &PENDING
}

/// One observer registration per browser, kept forever — dropping the `Registration` unregisters it,
/// and a call whose answer nothing listens for never completes. `downloads.rs` holds its own for the
/// same reason.
fn observers() -> &'static Mutex<Vec<(i32, Registration)>> {
    static OBSERVERS: Mutex<Vec<(i32, Registration)>> = Mutex::new(Vec::new());
    &OBSERVERS
}

wrap_dev_tools_message_observer! {
    pub struct ScreenshotObserver;

    impl DevToolsMessageObserver {
        fn on_dev_tools_method_result(
            &self,
            _browser: Option<&mut Browser>,
            message_id: ::std::os::raw::c_int,
            success: ::std::os::raw::c_int,
            result: Option<&[u8]>,
        ) {
            debug_assert_ne!(currently_on(ThreadId::UI), 0);

            // Every result in the process arrives here, `:devtools`'s own front-end included. An id
            // this module did not hand out belongs to someone else.
            let Some(waiting) = take_pending(message_id) else {
                return;
            };
            let json = result.map(String::from_utf8_lossy).unwrap_or_default();
            match finish_screenshot(success != 0, &json, &waiting.path) {
                Ok(bytes) => crate::message::info(&format!(
                    "Screenshot saved to {} ({bytes} bytes)",
                    waiting.path.display()
                )),
                Err(problem) => crate::message::error(&format!("screenshot: {problem}")),
            }
        }
    }
}

fn take_pending(message_id: i32) -> Option<PendingShot> {
    let mut list = pending().lock().expect("screenshot pending mutex poisoned");
    let at = list.iter().position(|shot| shot.message_id == message_id)?;
    Some(list.remove(at))
}

/// `screenshot [--rect WxH+X+Y] [--force] <filename>` (`misccommands.py:149-186`).
///
/// `CefBrowserHost` has no screenshot call — `grab_pixmap`, which qutebrowser uses, is a Qt widget
/// method with no CEF counterpart — but the DevTools protocol has `Page.captureScreenshot`, and
/// `downloads.rs` already drives that protocol for `download --mhtml`. This is the same shape: an
/// observer registered once per browser, an id assigned before the call so the answer has something
/// to match, and the bytes written here rather than by Chromium.
///
/// The file format comes from the extension, as qutebrowser's docstring promises; the protocol
/// offers png, jpeg and webp, and an extension outside those three is refused by name rather than
/// silently written as a PNG.
pub fn screenshot(browser: &mut Browser, filename: &str, rect: Option<&str>, force: bool) {
    debug_assert_ne!(currently_on(ThreadId::UI), 0);

    let path = crate::spawn::expand_user(filename);
    let format = match image_format(&path) {
        Some(format) => format,
        None => {
            crate::message::error(
                "screenshot: name the file .png, .jpg, .jpeg or .webp — those are the three the \
                 engine can encode",
            );
            return;
        }
    };
    if path.exists() && !force {
        crate::message::error(&format!(
            "File {} already exists (use --force to overwrite)",
            path.display()
        ));
        return;
    }
    let clip = match rect.map(parse_rect) {
        Some(Ok(clip)) => Some(clip),
        Some(Err(problem)) => {
            crate::message::error(&format!("screenshot: {problem}"));
            return;
        }
        None => None,
    };

    let Some(host) = browser.host() else {
        return;
    };
    let identifier = browser.identifier();
    {
        let mut list = observers().lock().expect("screenshot observers mutex poisoned");
        if !list.iter().any(|(id, _)| *id == identifier) {
            let mut observer = ScreenshotObserver::new();
            match host.add_dev_tools_message_observer(Some(&mut observer)) {
                Some(registration) => list.push((identifier, registration)),
                None => {
                    drop(list);
                    crate::message::error("screenshot: CEF refused the DevTools observer");
                    return;
                }
            }
        }
    }

    let message_id = next_message_id();
    pending()
        .lock()
        .expect("screenshot pending mutex poisoned")
        .push(PendingShot { message_id, path: path.clone() });

    let mut params = dictionary_value_create();
    if let Some(params) = params.as_mut() {
        params.set_string(Some(&CefString::from("format")), Some(&CefString::from(format)));
        if let Some((x, y, width, height)) = clip {
            let mut viewport = dictionary_value_create();
            if let Some(viewport) = viewport.as_mut() {
                viewport.set_double(Some(&CefString::from("x")), x as f64);
                viewport.set_double(Some(&CefString::from("y")), y as f64);
                viewport.set_double(Some(&CefString::from("width")), width as f64);
                viewport.set_double(Some(&CefString::from("height")), height as f64);
                viewport.set_double(Some(&CefString::from("scale")), 1.0);
            }
            params.set_dictionary(Some(&CefString::from("clip")), viewport.as_mut());
        }
    }

    debug(&format!("Page.captureScreenshot #{message_id} -> {}", path.display()));
    let assigned = host.execute_dev_tools_method(
        message_id,
        Some(&CefString::from("Page.captureScreenshot")),
        params.as_mut(),
    );
    if assigned == 0 {
        take_pending(message_id);
        crate::message::error("screenshot: CEF refused the Page.captureScreenshot call");
    }
}

/// The bookkeeping half, split out so a test can run it — anything that posts a CEF task cannot be
/// called under `cargo test` (CEF-NOTES trap 13), and `message::info` does.
///
/// Returns how many bytes were written.
fn finish_screenshot(success: bool, json: &str, path: &std::path::Path) -> Result<usize, String> {
    if !success {
        return Err(json_string_field(json, "message")
            .unwrap_or_else(|| "the DevTools call failed and said nothing".to_string()));
    }
    let Some(data) = json_string_field(json, "data") else {
        return Err("Page.captureScreenshot answered without any data".to_string());
    };
    let bytes = base64_decode(&data).ok_or_else(|| {
        "Page.captureScreenshot answered with something that is not base64".to_string()
    })?;
    if bytes.is_empty() {
        return Err("Page.captureScreenshot answered with an empty image".to_string());
    }
    std::fs::write(path, &bytes).map_err(|e| format!("could not write {}: {e}", path.display()))?;
    Ok(bytes.len())
}

/// `WxH+X+Y` — qutebrowser's `utils.parse_rect` (`utils.py:730-750`), which is X11's geometry
/// spelling and what `--rect` documents.
fn parse_rect(rect: &str) -> Result<(i32, i32, i32, i32), String> {
    let bad = || format!("invalid rectangle {rect:?} - expected WxH+X+Y");
    let (size, offset) = rect.split_once('+').ok_or_else(bad)?;
    let (width, height) = size.split_once('x').ok_or_else(bad)?;
    let (x, y) = offset.split_once('+').ok_or_else(bad)?;
    let numbers: Vec<i32> = [width, height, x, y]
        .iter()
        .map(|part| part.parse::<i32>().map_err(|_| bad()))
        .collect::<Result<_, _>>()?;
    if numbers[0] <= 0 || numbers[1] <= 0 {
        return Err(format!("invalid rectangle {rect:?} - width and height must be positive"));
    }
    Ok((numbers[2], numbers[3], numbers[0], numbers[1]))
}

/// The three formats `Page.captureScreenshot` encodes, chosen by extension.
fn image_format(path: &std::path::Path) -> Option<&'static str> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match extension.as_str() {
        "png" => "png",
        "jpg" | "jpeg" => "jpeg",
        "webp" => "webp",
        _ => return None,
    })
}

/// One string field out of a flat JSON object. The same reader `downloads.rs` has, because the two
/// answers have the same shape and neither is worth a JSON crate.
fn json_string_field(json: &str, field: &str) -> Option<String> {
    let needle = format!("\"{field}\"");
    let at = json.find(&needle)? + needle.len();
    let rest = json[at..].trim_start().strip_prefix(':')?.trim_start();
    let body = rest.strip_prefix('"')?;

    let mut out = String::with_capacity(body.len());
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                'u' => {
                    let hex: String = chars.by_ref().take(4).collect();
                    let code = u32::from_str_radix(&hex, 16).ok()?;
                    out.push(char::from_u32(code)?);
                }
                other => out.push(other),
            },
            other => out.push(other),
        }
    }
    None
}

/// Base64, decoded here rather than pulled in as a crate: the alphabet is 64 characters and this is
/// the only place in bru that needs it. `cef::base64_encode` exists and has no inverse.
fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let mut accumulator: u32 = 0;
    let mut bits = 0u32;
    for c in text.chars() {
        let value = match c {
            'A'..='Z' => c as u32 - 'A' as u32,
            'a'..='z' => c as u32 - 'a' as u32 + 26,
            '0'..='9' => c as u32 - '0' as u32 + 52,
            '+' => 62,
            '/' => 63,
            '=' => break,
            // The protocol sends one long line, but a base64 blob with newlines in it is the shape
            // everyone expects, and skipping whitespace costs nothing.
            c if c.is_ascii_whitespace() => continue,
            _ => return None,
        };
        accumulator = (accumulator << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((accumulator >> bits) & 0xFF) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab(window: u32, index: usize, url: &str, title: &str) -> TabRef {
        TabRef { window, index, url: url.to_string(), title: title.to_string() }
    }

    fn three() -> Vec<TabRef> {
        vec![
            tab(0, 0, "https://example.com/", "Example Domain"),
            tab(0, 1, "https://doc.rust-lang.org/std/", "std - Rust"),
            tab(1, 0, "https://vesti.bg/", "Новини"),
        ]
    }

    /// The three spellings `_resolve_tab_index` accepts, and the three ways it refuses.
    #[test]
    fn a_tab_is_named_by_index_by_window_and_index_or_by_what_it_says() {
        let tabs = three();
        // 1-based, in the window the command was run in.
        assert_eq!(resolve_tab(&tabs, 0, "2").unwrap().index, 1);
        assert_eq!(resolve_tab(&tabs, 1, "1").unwrap().window, 1);
        // `win/index`, which is the only way to name a tab in another window.
        assert_eq!(resolve_tab(&tabs, 0, "1/1").unwrap().window, 1);
        // A fragment of the title or of the URL, in either case.
        assert_eq!(resolve_tab(&tabs, 0, "rust").unwrap().index, 1);
        assert_eq!(resolve_tab(&tabs, 0, "Example").unwrap().index, 0);
        assert_eq!(resolve_tab(&tabs, 0, "vesti").unwrap().window, 1);
        // Every word has to match, in any order and in any column.
        assert_eq!(resolve_tab(&tabs, 0, "std rust-lang").unwrap().index, 1);
        assert!(resolve_tab(&tabs, 0, "std example").is_err());

        assert!(resolve_tab(&tabs, 0, "9").unwrap_err().contains("no tab with index 9"));
        assert!(resolve_tab(&tabs, 0, "0").unwrap_err().contains("no tab with index 0"));
        assert!(resolve_tab(&tabs, 0, "7/1").unwrap_err().contains("no window with id 7"));
        assert!(resolve_tab(&tabs, 0, "nothing here").unwrap_err().contains("No matching tab"));
    }

    /// A count beats the argument everywhere else in bru, and `tab-select` says so out loud
    /// (`commands.py:962`). This is the resolution half of that, which is the half that can be
    /// tested without a browser.
    #[test]
    fn a_window_qualified_index_reaches_a_tab_the_current_window_does_not_have() {
        let tabs = three();
        // Window 1 has one tab, so `2` is out of range there and `0/2` is not.
        assert!(resolve_tab(&tabs, 1, "2").is_err());
        assert_eq!(resolve_tab(&tabs, 1, "0/2").unwrap().url, "https://doc.rust-lang.org/std/");
    }

    #[test]
    fn a_rectangle_is_the_x11_geometry_spelling() {
        // width, height, x, y — and what comes back is (x, y, width, height), which is the order
        // the protocol's `clip` wants.
        assert_eq!(parse_rect("100x200+10+20").unwrap(), (10, 20, 100, 200));
        assert_eq!(parse_rect("1x1+0+0").unwrap(), (0, 0, 1, 1));
        for bad in ["", "100x200", "100+200", "x+ +", "100x200+10", "0x10+0+0", "-1x2+0+0"] {
            assert!(parse_rect(bad).is_err(), "{bad:?} was accepted");
        }
    }

    #[test]
    fn the_file_extension_chooses_the_format() {
        let format = |name: &str| image_format(std::path::Path::new(name));
        assert_eq!(format("/tmp/x.png"), Some("png"));
        assert_eq!(format("/tmp/x.PNG"), Some("png"));
        assert_eq!(format("/tmp/x.jpg"), Some("jpeg"));
        assert_eq!(format("/tmp/x.jpeg"), Some("jpeg"));
        assert_eq!(format("/tmp/x.webp"), Some("webp"));
        // The two that would otherwise be written as a PNG under a name that lies about it.
        assert_eq!(format("/tmp/x.gif"), None);
        assert_eq!(format("/tmp/x"), None);
    }

    /// The decoder, against the vectors in RFC 4648 §10 plus one PNG header — which is the only
    /// thing this is ever handed.
    #[test]
    fn base64_round_trips_the_bytes_the_protocol_sends() {
        assert_eq!(base64_decode("").unwrap(), b"");
        assert_eq!(base64_decode("Zg==").unwrap(), b"f");
        assert_eq!(base64_decode("Zm8=").unwrap(), b"fo");
        assert_eq!(base64_decode("Zm9v").unwrap(), b"foo");
        assert_eq!(base64_decode("Zm9vYg==").unwrap(), b"foob");
        assert_eq!(base64_decode("Zm9vYmE=").unwrap(), b"fooba");
        assert_eq!(base64_decode("Zm9vYmFy").unwrap(), b"foobar");
        // A PNG signature, which is what tells the file command what the screenshot is.
        assert_eq!(
            base64_decode("iVBORw0KGgo=").unwrap(),
            [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]
        );
        // Newlines are skipped, and anything outside the alphabet is refused rather than dropped:
        // a decoder that ignored a stray byte would write a corrupt image and say it succeeded.
        assert_eq!(base64_decode("Zm9v\nYmFy").unwrap(), b"foobar");
        assert!(base64_decode("Zm9v*Ymfy").is_none());
    }

    /// The failure this whole id-range business exists to prevent. `downloads.rs` numbers its
    /// DevTools calls from 1; if this module did too, one observer would answer the other's call.
    #[test]
    fn screenshot_message_ids_cannot_collide_with_the_mhtml_ones() {
        let first = next_message_id();
        let second = next_message_id();
        assert!(first >= FIRST_MESSAGE_ID, "{first} is in downloads.rs's range");
        assert_eq!(second, first + 1);
        // A session would have to make 1,073,741,824 MHTML snapshots to reach this range.
        assert_eq!(FIRST_MESSAGE_ID, 0x4000_0000);
    }

    #[test]
    fn a_failed_screenshot_says_what_the_protocol_said() {
        let problem = finish_screenshot(
            false,
            r#"{"code":-32000,"message":"Unable to capture screenshot"}"#,
            std::path::Path::new("/nonexistent/x.png"),
        )
        .unwrap_err();
        assert_eq!(problem, "Unable to capture screenshot");
        // And an answer with no data at all is not silently a zero-byte file.
        assert!(finish_screenshot(true, "{}", std::path::Path::new("/nonexistent/x.png")).is_err());
    }

    #[test]
    fn a_screenshot_is_written_where_it_was_asked_for() {
        let dir = std::env::temp_dir().join(format!("bru-shot-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shot.png");
        let bytes = finish_screenshot(true, r#"{"data":"Zm9vYmFy"}"#, &path).unwrap();
        assert_eq!(bytes, 6);
        assert_eq!(std::fs::read(&path).unwrap(), b"foobar");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_fragment_is_encoded_and_an_encoded_one_is_left_alone() {
        assert_eq!(percent_encode_fragment("top"), "top");
        assert_eq!(percent_encode_fragment("a b"), "a%20b");
        assert_eq!(percent_encode_fragment("a%20b"), "a%20b");
        assert_eq!(percent_encode_fragment("раздел"), "%D1%80%D0%B0%D0%B7%D0%B4%D0%B5%D0%BB");
    }

    #[test]
    fn a_javascript_url_is_decoded_and_anything_else_is_refused() {
        assert_eq!(jseval_url("javascript:alert(1)").unwrap(), "alert(1)");
        assert_eq!(jseval_url("javascript:alert(%22x%22)").unwrap(), "alert(\"x\")");
        assert!(jseval_url("https://example.com/").is_err());
    }

    #[test]
    fn a_query_string_is_read_without_a_crate() {
        assert_eq!(query_value("level=warning", "level").as_deref(), Some("warning"));
        assert_eq!(query_value("level=warning&plain", "level").as_deref(), Some("warning"));
        assert_eq!(query_value("plain&level=error", "level").as_deref(), Some("error"));
        assert_eq!(query_value("plain", "level"), None);
        assert!(query_has("level=info&plain", "plain"));
        assert!(!query_has("level=info", "plain"));
    }

    /// The six names `:messages` takes, and what each means here.
    #[test]
    fn every_log_level_maps_onto_one_of_the_three_bru_has() {
        use crate::message::Level;
        assert_eq!(threshold("vdebug"), Some(Level::Info));
        assert_eq!(threshold("debug"), Some(Level::Info));
        assert_eq!(threshold("info"), Some(Level::Info));
        assert_eq!(threshold("warning"), Some(Level::Warning));
        assert_eq!(threshold("error"), Some(Level::Error));
        assert_eq!(threshold("critical"), Some(Level::Error));
        assert_eq!(threshold("nonsense"), None);
    }

    /// The restart argument vector, which is the one thing about `:restart` that can be checked
    /// without restarting. What must survive is the profile; what must not is anything that would
    /// fight the restore.
    #[test]
    fn the_restart_argv_keeps_the_profile_and_drops_the_start_page() {
        let argv = restart_argv();
        assert!(argv.len() >= 2, "{argv:?}");
        assert_eq!(argv.last().unwrap(), "--restore=_restart");
        assert!(!argv[1..].iter().any(|arg| arg.starts_with("--url")), "{argv:?}");
        assert_eq!(
            argv[1..].iter().filter(|arg| arg.starts_with("--restore")).count(),
            1,
            "{argv:?}"
        );
    }

    /// The version page names the crate `Cargo.toml` actually builds against. Two places, and the
    /// page is the one nobody would notice going stale.
    #[test]
    fn the_version_page_states_the_cef_version_cargo_builds_against() {
        let manifest = include_str!("../Cargo.toml");
        assert!(
            manifest.contains(&format!("cef = \"{CEF_CRATE_VERSION}\"")),
            "Cargo.toml no longer says cef = \"{CEF_CRATE_VERSION}\""
        );
    }
}
