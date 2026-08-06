//! Which completion answers which command, the selection that moves through it, and what `<Tab>`
//! writes back into the command line.
//!
//! `src/completion.rs` builds tables; this decides *which* table, remembers which row is on, and
//! puts that row's text where the user can accept it. Without the second half the first is a list
//! that watches you type.
//!
//! Every rule is qutebrowser 3.7.0's, cited to `/usr/lib/python3.13/site-packages/qutebrowser/`:
//!
//! - **Which model** — `completion/completer.py:73-104` (`_get_new_completion`). The text is cut
//!   into the parts before the cursor, the part under it and the parts after
//!   (`completer.py:119-155`, `_partition`); the first part names a command, the number of
//!   non-flag parts before the cursor names which of its arguments is being completed, and that
//!   argument's declared completion is the model. A part starting with `-`, or anything after a
//!   `--`, completes nothing.
//! - **Moving the selection** — `completion/completionwidget.py:170-213` (`_next_idx`),
//!   `:258-286` (`_next_category_idx`), `:215-256` (`_next_page`). Both ends wrap; category
//!   headers are never selectable; with nothing selected `next` takes the first item and `prev`
//!   the last.
//! - **What that does to the line** — `completer.py:157-186` (`on_selection_changed`) and
//!   `:274-305` (`_change_completed_part`): column 0 of the selected row replaces the part under
//!   the cursor, and the model is *not* rebuilt, so tabbing walks the list the pattern produced
//!   rather than a list that keeps shrinking under its own answer.
//! - **Deleting** — `completionwidget.py:465-471` and the `delete_func` each category is built
//!   with (`models/miscmodels.py:42-56`, `models/urlmodel.py:14-28`).
//!
//! ## Two things here are deliberately not qutebrowser's
//!
//! 1. **`completion.quick` is not implemented.** qutebrowser applies a lone match immediately,
//!    with a trailing space, and moves on to the next argument (`completer.py:172-186`). It reads
//!    as the line typing ahead of you and it depends on a per-command `maxsplit` bru's parser does
//!    not expose; a selection here always waits to be accepted.
//! 2. **The caret lands at the end of the line**, not at the end of the completed part. bru's
//!    command line has one public setter and it puts the cursor at the end (`cmdline::set_text`).
//!    It only shows when something follows the completed part, which no default binding produces —
//!    `cmdline::set_text_at(text, cursor)` is the two lines that would close it.

use cef::*;
use std::sync::Mutex;

use crate::commands::{Command, FocusWhich};
use crate::completion::{self, Category};
use crate::modes::Mode;

/// How far `<PgDown>` moves, in rows.
///
/// Measured off the stylesheet rather than guessed: `chrome/chrome.css:49-50` sets `--row-h: 20px`
/// and `--completion-max-h: 300px`, so fifteen rows are on screen. qutebrowser moves a pageful
/// less one, "leave one old line visible" (`completionwidget.py:236`).
const PAGE: usize = 14;

// -----------------------------------------------------------------------------------------------
// Which model
// -----------------------------------------------------------------------------------------------

/// The models bru has. `src/completion.rs` builds `Url`; the rest are below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Which {
    /// `:open` — search engines, quickmarks, bookmarks, history.
    Url,
    /// `:tab-select`, `:tab-focus`. `special` adds qutebrowser's Special category, which is where
    /// `tab-focus last` comes from (`miscmodels.py:176-189`).
    Tabs { special: bool },
    /// `:quickmark-load` and friends — the name is what the command takes, so it leads.
    Quickmark,
    /// `:bookmark-load` and friends — a URL is what the command takes.
    Bookmark,
}

/// One row of the table that turns a command into a model.
///
/// `argpos` is which positional argument, counting from zero after the command name, this model
/// completes; `maxsplit0` is qutebrowser's `maxsplit=0` — everything after the flags is one
/// argument, verbatim, spaces included. `:open` has it, which is the only reason
/// `:open rust vec` can be two search terms rather than two arguments.
struct Spec {
    name: &'static str,
    argpos: usize,
    which: Which,
    maxsplit0: bool,
}

/// Every command bru completes. Adding a model is a line here and an arm in [`build`].
///
/// What is deliberately absent, and why, is in the report and in this file's tests: `:bind` and
/// `:set` want a registry of command names and of settings that bru does not have yet, and the
/// command-name model that `:` alone would open wants the same list.
const SPECS: &[Spec] = &[
    Spec { name: "open", argpos: 0, which: Which::Url, maxsplit0: true },
    Spec { name: "tab-select", argpos: 0, which: Which::Tabs { special: false }, maxsplit0: false },
    Spec { name: "tab-focus", argpos: 0, which: Which::Tabs { special: true }, maxsplit0: false },
    Spec { name: "quickmark-load", argpos: 0, which: Which::Quickmark, maxsplit0: false },
    Spec { name: "quickmark-del", argpos: 0, which: Which::Quickmark, maxsplit0: false },
    Spec { name: "bookmark-load", argpos: 0, which: Which::Bookmark, maxsplit0: false },
    Spec { name: "bookmark-del", argpos: 0, which: Which::Bookmark, maxsplit0: false },
];

/// The command line cut into the part being completed and the parts around it.
///
/// `completer.py:119-155`. Held between keystrokes so that moving the selection can rebuild the
/// line without re-running the parser.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Partition {
    /// `:` — the only prefix that completes. `/` and `?` are a search (`completer.py:213-219`).
    prefix: char,
    /// The command name and any flags before the cursor, in order.
    before: Vec<String>,
    /// The pattern, as typed.
    center: String,
    /// Everything after the completed part. Empty for every default binding.
    after: Vec<String>,
    which: Which,
    maxsplit0: bool,
}

impl Partition {
    /// The command line with `item` in place of the pattern. `_change_completed_part`
    /// (`completer.py:274-292`), without the `immediate` half — see the module comment.
    fn line_with(&self, item: &str) -> String {
        // A model whose command takes the rest of the line verbatim needs no quoting; anything
        // else is going back through a tokeniser that would split it (`completer.py:170`).
        let item = if self.maxsplit0 { item.to_string() } else { quote(item) };

        let mut parts = self.before.clone();
        parts.push(item);
        let mut out = String::from(self.prefix);
        out.push_str(&parts.join(" "));
        if !self.after.is_empty() {
            out.push(' ');
            out.push_str(&self.after.join(" "));
        }
        out
    }
}

/// `completer.py:106-117`. Not `shlex.quote`: only what bru's own tokeniser would split on.
fn quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    if s.chars().any(|c| " \"'\t\n\\".contains(c)) {
        return format!("'{}'", s.replace('\'', "'\"'\"'"));
    }
    s.to_string()
}

/// Cut `text` around `cursor` and say which model answers, or `None` for a line that completes
/// nothing: a search, an empty line, a flag, anything after a `--`, an unknown command, or an
/// argument position no model claims.
fn partition(text: &str, cursor: usize) -> Option<Partition> {
    let chars: Vec<char> = text.chars().collect();
    let prefix = *chars.first()?;
    if prefix != ':' {
        return None;
    }
    // Positions from here on are into the body, which is what the cursor is measured against too.
    let body = &chars[1..];
    let at = cursor.saturating_sub(1).min(body.len());

    // Runs of non-whitespace, with where each starts and ends.
    let mut tokens: Vec<(usize, usize)> = Vec::new();
    let mut start: Option<usize> = None;
    for (i, c) in body.iter().enumerate() {
        match (c.is_whitespace(), start) {
            (false, None) => start = Some(i),
            (true, Some(s)) => {
                tokens.push((s, i));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        tokens.push((s, body.len()));
    }
    let text_of = |(a, b): (usize, usize)| body[a..b].iter().collect::<String>();

    // `:` alone, or `:   `. qutebrowser opens the command-name model here; bru has no list of
    // command names to open it from, so the bar stays closed. Said out loud in the report.
    let (&first, rest) = tokens.split_first()?;
    let name = text_of(first);
    let spec = SPECS.iter().find(|spec| spec.name == name)?;

    // maxsplit=0: the flags belong to the command and everything from the first non-flag token to
    // the end of the line is one argument. `commands/parser.py:177-205`.
    if spec.maxsplit0 {
        let mut before = vec![name];
        let mut from = body.len();
        for &token in rest {
            let part = text_of(token);
            let flag = part.starts_with('-');
            if at <= token.1 {
                // The cursor is on this token, so this token is the part being completed — and a
                // flag completes nothing (`completer.py:83`).
                if flag {
                    return None;
                }
                from = token.0;
                break;
            }
            if flag {
                // An explicit end of flags stops the completion outright.
                if part == "--" {
                    return None;
                }
                before.push(part);
                continue;
            }
            from = token.0;
            break;
        }
        // The cursor has to be inside the argument, not back in the command's own name.
        if at < from {
            return None;
        }
        let center: String = body[from..].iter().collect();
        return Some(Partition {
            prefix,
            before,
            center,
            after: Vec::new(),
            which: spec.which,
            maxsplit0: true,
        });
    }

    // Everything else: the token under the cursor is the pattern. A cursor sitting in the
    // whitespace between two tokens completes an empty pattern there, as `_partition` does by
    // inserting an empty part (`completer.py:143-146`).
    let mut before: Vec<String> = vec![name];
    let mut center = String::new();
    let mut after: Vec<String> = Vec::new();
    let mut placed = false;
    for &token in rest {
        let part = text_of(token);
        if placed {
            after.push(part);
            continue;
        }
        if at < token.0 {
            // In the gap before this token.
            placed = true;
            after.push(part);
            continue;
        }
        if at <= token.1 {
            center = part;
            placed = true;
            continue;
        }
        before.push(part);
    }
    if before.iter().any(|part| part == "--") || center.starts_with('-') {
        return None;
    }
    // `argpos` counts positionals only, the command name included, less one (`completer.py:99`).
    let positionals = before.iter().filter(|part| !part.starts_with('-')).count();
    if positionals.saturating_sub(1) != spec.argpos {
        return None;
    }

    Some(Partition {
        prefix,
        before,
        center,
        after,
        which: spec.which,
        maxsplit0: false,
    })
}

// -----------------------------------------------------------------------------------------------
// The models this module adds
// -----------------------------------------------------------------------------------------------

/// The tabs that are open, as `(index, url, title)` — `miscmodels.py:91-155`, without the window
/// id (bru has one window, which is `add_win_id=False`) and without the renderer pid, which CEF
/// does not offer per browser.
fn tabs_rows() -> Vec<Vec<String>> {
    let Some(state) = crate::state::BruState::instance() else {
        return Vec::new();
    };
    let Ok(state) = state.lock() else {
        return Vec::new();
    };
    (0..state.tab_count())
        .map(|i| {
            vec![
                // 1-based, because that is what `:tab-focus 2` means.
                (i + 1).to_string(),
                state.tab_url(i).unwrap_or_default(),
                state.tab_title(i).unwrap_or_default(),
            ]
        })
        .collect()
}

/// `miscmodels.py:110` — `column_widths=(6, 40, 46, 8)`, less the pid column.
const TAB_WIDTHS: &[u8] = &[6, 40, 54];
/// `miscmodels.py:49,68` — `column_widths=(30, 70, 0)`.
const MARK_WIDTHS: &[u8] = &[30, 70];

fn build(which: Which, pattern: &str) -> Vec<Category> {
    match which {
        Which::Url => completion::categories(pattern),

        Which::Tabs { special } => {
            let mut out = Vec::with_capacity(2);
            if let Some(cat) = completion::list_category("Tabs", TAB_WIDTHS, tabs_rows(), pattern) {
                out.push(cat);
            }
            if special {
                // `miscmodels.py:180-185`. `stack-next` and `stack-prev` are left out: bru's
                // `tab-focus` implements `last` and an index and nothing else, and offering a
                // value the command ignores is worse than not offering it.
                let rows = vec![vec![
                    "last".to_string(),
                    "Focus the last-focused tab".to_string(),
                ]];
                if let Some(cat) = completion::list_category("Special", TAB_WIDTHS, rows, pattern) {
                    out.push(cat);
                }
            }
            out
        }

        // `(name, url)` — the other way round from `:open`'s quickmark category, because
        // `:quickmark-load` takes the name and column 0 is what is inserted (`miscmodels.py:40`).
        Which::Quickmark => {
            let rows = completion::sources()
                .map(|sources| sources.quickmarks())
                .unwrap_or_default()
                .into_iter()
                .map(|(url, name)| vec![name, url])
                .collect();
            completion::list_category("Quickmarks", MARK_WIDTHS, rows, pattern)
                .into_iter()
                .collect()
        }

        Which::Bookmark => {
            let rows = completion::sources()
                .map(|sources| sources.bookmarks())
                .unwrap_or_default()
                .into_iter()
                .map(|(url, title)| vec![url, title])
                .collect();
            completion::list_category("Bookmarks", MARK_WIDTHS, rows, pattern)
                .into_iter()
                .collect()
        }
    }
}

// -----------------------------------------------------------------------------------------------
// The live table and its selection
// -----------------------------------------------------------------------------------------------

/// What is on screen. One window, one command line, one of these.
///
/// The whole thing is derived from the command line: [`sync`] rebuilds when the text or the cursor
/// has moved and does nothing when neither has, so a push carrying a scroll percentage costs a
/// string comparison rather than a database query.
#[derive(Default)]
struct Live {
    /// The line this table was built from. Moving the selection writes the *new* line here before
    /// setting it, which is how tabbing through the list does not rebuild the list — qutebrowser
    /// blocks the signal for the same reason (`completer.py:293-305`).
    text: String,
    cursor: usize,
    part: Option<Partition>,
    cats: Vec<Category>,
    selected: Option<(usize, usize)>,
}

fn live() -> &'static Mutex<Live> {
    static LIVE: Mutex<Live> = Mutex::new(Live {
        text: String::new(),
        cursor: 0,
        part: None,
        cats: Vec::new(),
        selected: None,
    });
    &LIVE
}

impl Live {
    fn sync(&mut self, text: &str, cursor: usize) {
        if self.text == text && self.cursor == cursor {
            return;
        }
        self.text = text.to_string();
        self.cursor = cursor;
        self.part = partition(text, cursor);
        self.cats = match &self.part {
            Some(part) => build(part.which, &part.center),
            None => Vec::new(),
        };
        // `set_pattern` clears the selection (`completionwidget.py:394`): what was selected was a
        // row of a list that no longer exists.
        self.selected = None;
    }

    /// Every item, in the order the eye reads them, as `(category, item)`.
    fn flat(&self) -> Vec<(usize, usize)> {
        self.cats
            .iter()
            .enumerate()
            .flat_map(|(c, cat)| (0..cat.items.len()).map(move |i| (c, i)))
            .collect()
    }

    /// Where `which` moves to, or `None` when there is nowhere to move.
    fn step(&self, which: FocusWhich) -> Option<(usize, usize)> {
        let flat = self.flat();
        if flat.is_empty() {
            return None;
        }
        let last = flat.len() - 1;
        let at = self.selected.and_then(|sel| flat.iter().position(|&f| f == sel));

        Some(match (which, at) {
            // `_next_idx` with nothing selected: the first item going down, the last going up
            // (`completionwidget.py:180-184`).
            (FocusWhich::Next, None) => flat[0],
            (FocusWhich::Prev, None) => flat[last],
            // And both ends wrap (`:203-206`).
            (FocusWhich::Next, Some(at)) => flat[if at == last { 0 } else { at + 1 }],
            (FocusWhich::Prev, Some(at)) => flat[if at == 0 { last } else { at - 1 }],

            // `_next_category_idx` (`:258-286`). With nothing selected it is `_next_idx` moved to
            // its category's first row, which going down is the first item there is and going up
            // is the first item of the *last* category.
            (FocusWhich::NextCategory, None) => flat[0],
            (FocusWhich::PrevCategory, None) => (self.cats.len() - 1, 0),
            (FocusWhich::NextCategory, Some(_)) => {
                let cat = self.selected.map(|(c, _)| c).unwrap_or(0);
                (if cat + 1 >= self.cats.len() { 0 } else { cat + 1 }, 0)
            }
            (FocusWhich::PrevCategory, Some(_)) => {
                let cat = self.selected.map(|(c, _)| c).unwrap_or(0);
                (if cat == 0 { self.cats.len() - 1 } else { cat - 1 }, 0)
            }

            // `_next_page` (`:215-256`): a pageful, but stopping at the border first and only
            // wrapping if that is where it already was.
            (FocusWhich::NextPage, None) => flat[0],
            (FocusWhich::PrevPage, None) => flat[last],
            (FocusWhich::NextPage, Some(at)) => match at + PAGE {
                to if to <= last => flat[to],
                _ if at == last => flat[0],
                _ => flat[last],
            },
            (FocusWhich::PrevPage, Some(at)) => match at.checked_sub(PAGE) {
                Some(to) => flat[to],
                None if at == 0 => flat[last],
                None => flat[0],
            },
        })
    }

    fn item(&self, at: (usize, usize)) -> Option<&crate::completion::Item> {
        self.cats.get(at.0)?.items.get(at.1)
    }
}

// --- per-window mode -----------------------------------------------------------------------
/// The `completion` field of one window's bottom view state, and the only caller is
/// `ipc::bar_json_for`.
///
/// Answering `null` outside command mode is what collapses the bar: there is no line to complete.
///
/// It takes the window because a push does. bru keeps a `ModeManager` per window, so a bar built
/// for window 0 has to ask what *window 0* is in — asking "the current mode" meant that any push
/// aimed at a background bar, a title change or a download tick, answered `null` for a window that
/// had the table open, and took its height down with it on the way past.
pub fn json_for(window: u32) -> String {
    if mode_in(window) != Mode::Command {
        if let Ok(mut live) = live().lock() {
            *live = Live::default();
        }
        resize_bar(window, &[]);
        return "null".to_string();
    }
    let (text, cursor, _) = crate::cmdline::state_for_completion_in(window);
    let (json, cats) = {
        let Ok(mut guard) = live().lock() else {
            return "null".to_string();
        };
        guard.sync(&text, cursor);
        let json = completion::to_json(&guard.cats, guard.selected);
        (json, std::mem::take(&mut guard.cats))
    };
    // Outside the lock: the relayout this asks for renders the strip, which pushes, which comes
    // back here.
    resize_bar(window, &cats);
    if let Ok(mut guard) = live().lock() {
        guard.cats = cats;
    }
    json
}
// --- end per-window mode -------------------------------------------------------------------

/// Ask one window's bottom strip to be as tall as the table it is drawing.
///
/// The arithmetic is `chrome/chrome.css:186-191`'s, and it is the stylesheet's rather than a guess:
/// `--row-h: 20px` per header and per row, one pixel for `#completion`'s bottom border, capped at
/// `--completion-max-h: 300px` because past that the table scrolls inside itself.
// --- per-window mode -----------------------------------------------------------------------
// The window is named at every step now: the height is stored under it, the strip whose layout is
// invalidated is that window's, and so is the `Window` handle the relayout is asked of. All three
// used to mean "whichever window is current", which is not the window a push is for.
fn resize_bar(window: u32, cats: &[Category]) {
    const ROW_H: i32 = 20;
    const MAX_H: i32 = 300;

    let rows: i32 = cats.iter().map(|cat| cat.items.len() as i32).sum();
    let wanted = if rows == 0 {
        0
    } else {
        (ROW_H * (rows + cats.len() as i32)).min(MAX_H) + 1
    };

    let was = crate::window::set_completion_height(window, wanted);
    if was == wanted {
        // Every push reaches here and most leave the height alone; a relayout each time would
        // resize a Views tree on every scroll report.
        return;
    }
    let Some(mut browser) = crate::ipc::bottom_chrome_browser_for(window) else {
        return;
    };
    let Some(view) = browser_view_get_for_browser(Some(&mut browser)) else {
        return;
    };
    // `invalidate_layout` on the strip alone leaves the box layout that *placed* it holding the
    // old height, so the window is asked too — it is the panel that does the laying out.
    View::from(&view).invalidate_layout();
    let handle = crate::state::BruState::instance()
        .and_then(|state| state.lock().ok().and_then(|state| state.window_handle(window)));
    if let Some(handle) = handle {
        View::from(&handle).invalidate_layout();
    }
}
// --- end per-window mode -------------------------------------------------------------------

// -----------------------------------------------------------------------------------------------
// The three commands
// -----------------------------------------------------------------------------------------------

/// Run one of this module's commands. `false` means it is not one of ours — the same shape
/// `cmdline::run_command` has, and it is called from the one arm `exec::run` gives it.
pub fn run_command(command: &Command) -> bool {
    match command {
        Command::CompletionItemFocus { which, history } => {
            focus(*which, *history);
            true
        }
        Command::CompletionItemDel => {
            del();
            true
        }
        Command::CompletionItemYank { sel } => {
            yank(*sel);
            true
        }
        _ => false,
    }
}

/// `completion-item-focus [--history] <which>`.
///
/// Moving the selection is half of it; the other half is that the selected text goes into the
/// command line, which is what makes `<Tab>` feel like completion rather than like a torch being
/// shone along a list.
pub fn focus(which: FocusWhich, history: bool) {
    if mode() != Mode::Command {
        return;
    }

    let (text, cursor, browsing) = crate::cmdline::state_for_completion();

    // `--history` is `<Up>` and `<Down>`, and on a bare `:` or a line already recalled from the
    // history they walk the history instead (`completionwidget.py:305-317`).
    if history {
        let empty = live().lock().map(|live| live.cats.is_empty()).unwrap_or(true);
        if text == ":" || browsing || empty {
            let name = match which {
                FocusWhich::Next => "command-history-next",
                FocusWhich::Prev => "command-history-prev",
                other => {
                    eprintln!("bru: completion-item-focus: can't combine --history with {other}");
                    return;
                }
            };
            // By name, because that is how `cmdline.rs` registers the two — see its `is_named`.
            crate::cmdline::run_command(&Command::Unimplemented(name.to_string()), None);
            return;
        }
    }

    let line = {
        let Ok(mut live) = live().lock() else {
            return;
        };
        live.sync(&text, cursor);
        let Some(to) = live.step(which) else {
            return;
        };
        let Some(item) = live.item(to) else {
            return;
        };
        let Some(part) = live.part.as_ref() else {
            return;
        };
        let line = part.line_with(&item.cols[0]);
        live.selected = Some(to);
        // Claim the line before setting it: the push that `cmd_set_text` ends with comes straight
        // back into `json` → `sync`, and a line it does not recognise as its own would rebuild the
        // table and throw the selection away. Measured with the two lines commented out: the
        // second `<Tab>` re-selected `[0,0]` because the first had left `selected` at None and the
        // pattern at "duckduckgo" — the completion answering its own answer.
        live.text = line.clone();
        live.cursor = line.chars().count();
        line
    };

    // Outside the lock. `cmd_set_text` pushes, and a push reads this module.
    crate::cmdline::cmd_set_text(&line, false, false, false, None);
}

/// `completion-item-del` — `<Ctrl-D>`. What "delete" means belongs to the category the row is in
/// (`completionwidget.py:465-471` over each category's `delete_func`).
pub fn del() {
    if mode() != Mode::Command {
        return;
    }
    let (text, cursor, _) = crate::cmdline::state_for_completion();

    let deleted = {
        let Ok(mut live) = live().lock() else {
            return;
        };
        live.sync(&text, cursor);
        let Some(at) = live.selected else {
            // qutebrowser raises "No item selected!" into the message line, which bru has not
            // built yet.
            eprintln!("bru: completion-item-del: no item selected");
            return;
        };
        let Some(item) = live.item(at) else {
            return;
        };
        let name = live.cats[at.0].name;
        let key = item.cols[0].clone();
        if !delete_from(name, &key) {
            return;
        }

        // The row goes out of the table in place — `delete_cur_item` edits the model, it does not
        // ask for a new one (`completionwidget.py:470`). Rebuilding instead would be rebuilding
        // from a command line that the selection has already rewritten, so `<Ctrl-D>` would leave
        // the completed URL as the new pattern and the table would empty itself.
        live.cats[at.0].items.remove(at.1);
        if live.cats[at.0].items.is_empty() {
            live.cats.remove(at.0);
        }
        // The row below slides up into the selection, which is what makes `<Ctrl-D><Ctrl-D>` walk
        // down a run of stale history entries. Nothing left below means nothing selected.
        live.selected = live
            .cats
            .get(at.0)
            .map(|cat| (at.0, at.1.min(cat.items.len().saturating_sub(1))));
        true
    };

    if deleted {
        crate::ipc::push_bar();
    }
}

/// The `delete_func` of each category bru has one for. `true` means the source changed.
fn delete_from(category: &str, key: &str) -> bool {
    match category {
        // `urlmodel.py:14-19` — the history row, by URL. `data.rs::forget_url` is the same call
        // `:history-clear` would make.
        "History" => with_data_mut(|data| data.forget_url(key).is_ok()),
        // `urlmodel.py:22-28` and `miscmodels.py:42-47`. `:open`'s quickmark category leads with
        // the URL and the quickmark model with the name, and `quickmark_del` wants the name, so
        // which column is the key depends on which model built the row.
        "Quickmarks" => {
            let name = quickmark_name_for(key);
            with_data_mut(|data| data.quickmark_del(&name).unwrap_or(false))
        }
        "Bookmarks" => with_data_mut(|data| data.bookmark_del(key).unwrap_or(false)),
        // Search engines are config, and bru never writes config (DESIGN.md). Tabs would be
        // `delete_tab` (`miscmodels.py:100-108`) and want a close-by-index the tab workstream owns.
        other => {
            eprintln!("bru: completion-item-del: nothing to delete for a {other} row");
            false
        }
    }
}

/// A quickmark row's name, whichever column the model put it in.
fn quickmark_name_for(key: &str) -> String {
    let marks = completion::sources()
        .map(|sources| sources.quickmarks())
        .unwrap_or_default();
    // `Sources::quickmarks` is `(url, name)`.
    marks
        .iter()
        .find(|(url, _)| url == key)
        .map(|(_, name)| name.clone())
        .unwrap_or_else(|| key.to_string())
}

/// `src/data.rs`'s one instance, mutably. The module is another workstream's and is not touched:
/// this is its own public API — `instance()` plus the `&mut self` methods on `Data`.
fn with_data_mut<T: Default>(f: impl FnOnce(&mut crate::data::Data) -> T) -> T {
    let Some(data) = crate::data::instance() else {
        return T::default();
    };
    let Ok(mut data) = data.lock() else {
        return T::default();
    };
    f(&mut data)
}

// --- the clipboard this module does not own ------------------------------------------------------

/// What `completion-item-yank` needs, and the whole of it: the text, and whether it goes to the
/// primary selection rather than the clipboard.
///
/// `wl-copy` is the decided implementation (STAGE3-CONTRACTS.md) and it belongs to the yank
/// workstream, not here. Until that module installs one, `<Ctrl-C>` says what it would have
/// copied — and `exec::is_live` reports the two `completion-item-yank` bindings as not live, which
/// is the honest answer while there is nothing behind them.
pub type Clipboard = fn(text: &str, selection: bool);

static CLIPBOARD: Mutex<Option<Clipboard>> = Mutex::new(None);

/// Install the clipboard, once, at startup. `app.rs` hands it `clip::yank_plain`.
pub fn install_clipboard(clipboard: Clipboard) {
    if let Ok(mut slot) = CLIPBOARD.lock() {
        *slot = Some(clipboard);
    }
}

/// `completion-item-yank [--sel]` — `<Ctrl-C>` and `<Ctrl-Shift-C>`.
pub fn yank(selection: bool) {
    if mode() != Mode::Command {
        return;
    }
    let (text, cursor, _) = crate::cmdline::state_for_completion();
    let yanked = {
        let Ok(mut live) = live().lock() else {
            return;
        };
        live.sync(&text, cursor);
        // qutebrowser yanks the command line's *selected text* first and falls back to the row
        // (`completionwidget.py:474-489`); an `<input>` bru never puts a selection into has none.
        match live.selected.and_then(|at| live.item(at)) {
            Some(item) => item.cols[0].clone(),
            None => {
                eprintln!("bru: completion-item-yank: no item selected");
                return;
            }
        }
    };
    match CLIPBOARD.lock().ok().and_then(|slot| *slot) {
        Some(clipboard) => clipboard(&yanked, selection),
        None => eprintln!(
            "bru: no clipboard installed, would have yanked {yanked:?} \
             (selection={selection})"
        ),
    }
}

fn mode() -> Mode {
    crate::state::BruState::instance()
        .and_then(|state| state.lock().ok().map(|state| state.mode()))
        .unwrap_or(Mode::Normal)
}

// --- per-window mode -----------------------------------------------------------------------
/// A named window's mode. `mode()` above stays "the window a command was typed in", which is what
/// the three completion commands mean; this is what a *push* means, and the two are not the same
/// window once there is more than one.
fn mode_in(window: u32) -> Mode {
    crate::state::BruState::instance()
        .and_then(|state| state.lock().ok().map(|state| state.mode_in(window)))
        .unwrap_or(Mode::Normal)
}
// --- end per-window mode -------------------------------------------------------------------

// -----------------------------------------------------------------------------------------------
// Driving it without a keyboard
// -----------------------------------------------------------------------------------------------

/// `--completion-script='set::open ;type:du;cmd:completion-item-focus next;dump'
///  --completion-step-ms=400`
///
/// The same reason `--cmdline-script` exists: `wtype` segfaults CEF on this machine, so the check
/// has to drive the functions a key drives. This one exists beside it because the two halves have
/// to interleave — set the text, move the selection, look at both — and two switches with two
/// clocks cannot be interleaved without arithmetic nobody should have to redo.
///
/// | | |
/// |---|---|
/// | `set:<text>` | `cmd-set-text <text>` — enters command mode and prefills |
/// | `type:<text>` | what the chrome reports after typing `<text>` at the cursor |
/// | `cmd:<command>` | parse and run one command, through this module or the command line |
/// | `key:<key>` | a real `<Tab>`, sent at the bottom strip through `send_key_event` |
/// | `accept` | `command-accept` |
/// | `dump` | print the line, the selection, and every row of the table |
pub fn schedule_script(steps: &str, interval_ms: i64) {
    for (i, step) in steps.split(';').filter(|s| !s.is_empty()).enumerate() {
        let mut task = ScriptStep::new(step.to_string());
        post_delayed_task(ThreadId::UI, Some(&mut task), interval_ms * (i as i64 + 1));
    }
}

/// Read once the bottom strip has announced itself, because there is no command line before then.
pub fn start_script() {
    let Some(command_line) = command_line_get_global() else {
        return;
    };
    let script =
        CefString::from(&command_line.switch_value(Some(&CefString::from("completion-script"))))
            .to_string();
    if script.is_empty() {
        return;
    }
    let step_ms =
        CefString::from(&command_line.switch_value(Some(&CefString::from("completion-step-ms"))))
            .to_string()
            .parse::<i64>()
            .unwrap_or(600);
    schedule_script(&script, step_ms);
}

wrap_task! {
    struct ScriptStep {
        step: String,
    }

    impl Task {
        fn execute(&self) {
            let (verb, arg) = match self.step.split_once(':') {
                Some((verb, arg)) => (verb, arg),
                None => (self.step.as_str(), ""),
            };
            match verb {
                "set" => crate::cmdline::cmd_set_text(arg, false, false, false, None),
                "type" => {
                    let (text, cursor, _) = crate::cmdline::state_for_completion();
                    let mut chars: Vec<char> = text.chars().collect();
                    for (offset, c) in arg.chars().enumerate() {
                        chars.insert(cursor + offset, c);
                    }
                    let text: String = chars.iter().collect();
                    let at: usize = chars[..cursor + arg.chars().count()]
                        .iter()
                        .map(|c| c.len_utf16())
                        .sum();
                    // Exactly what `chrome/bottom.js` sends on an input event.
                    crate::cmdline::on_text_changed_here(&text, Some(at));
                    crate::ipc::push_bar();
                }
                "cmd" => match crate::commands::parse(arg) {
                    Ok(command) => {
                        if !run_command(&command) && !crate::cmdline::run_command(&command, None) {
                            eprintln!("completion-script: {arg:?} is not a command line command");
                        }
                    }
                    Err(e) => eprintln!("completion-script: {arg:?} does not parse: {e}"),
                },
                "key" => inject_named_key(arg),
                "accept" => crate::cmdline::command_accept(false),
                "dump" => dump(),
                other => eprintln!("completion-script: no step named {other:?}"),
            }
        }
    }
}

/// Send one **named** key at the bottom strip, the way a keyboard would.
///
/// `cmdline.rs`'s injector refuses these — it was written for the letters — and every key this
/// module binds is a named one, so the only way to answer "does pressing `<Tab>` reach the
/// binding, or is it swallowed on the way?" is to send a real `<Tab>`. `wtype` is forbidden
/// (STAGE3-CONTRACTS.md), and CEF-NOTES is explicit about the shape: `KEYDOWN`, `CHAR`, `KEYUP`,
/// and **never** an explicit `RAWKEYDOWN` beside them.
fn inject_named_key(spec: &str) {
    use crate::bindings::{Key, Modifiers, NamedKey};

    let Some(info) = crate::bindings::parse_key_sequence(spec)
        .ok()
        .and_then(|sequence| sequence.first().copied())
    else {
        eprintln!("completion-script: {spec:?} is not a key");
        return;
    };
    // Only the ones this module's bindings use; `bindings.rs` owns the table going the other way
    // and this is a debug path, so the inverse is spelled here rather than made public there.
    let code = match info.key {
        Key::Named(NamedKey::Tab) => 0x09,
        Key::Named(NamedKey::PgUp) => 0x21,
        Key::Named(NamedKey::PgDown) => 0x22,
        Key::Named(NamedKey::Up) => 0x26,
        Key::Named(NamedKey::Down) => 0x28,
        Key::Named(NamedKey::Delete) => 0x2E,
        other => {
            eprintln!("completion-script: {other:?} is not one this step can send");
            return;
        }
    };
    let mut modifiers = 0u32;
    for (mods, flag) in [
        (Modifiers::SHIFT, 2),
        (Modifiers::CTRL, 4),
        (Modifiers::ALT, 8),
        (Modifiers::META, 128),
    ] {
        if info.mods.contains(mods) {
            modifiers |= flag;
        }
    }

    let Some(host) = crate::ipc::bottom_chrome_browser().and_then(|browser| browser.host()) else {
        eprintln!("completion-script: no bottom strip to inject into yet");
        return;
    };
    for type_ in [KeyEventType::KEYDOWN, KeyEventType::CHAR, KeyEventType::KEYUP] {
        let event = KeyEvent {
            type_,
            modifiers,
            windows_key_code: code,
            native_key_code: 0,
            // A named key types nothing, so there is no character to carry.
            ..Default::default()
        };
        host.send_key_event(Some(&event));
    }
}

/// What the bar holds, in the form a screenshot has to agree with.
fn dump() {
    let (text, cursor, _) = crate::cmdline::state_for_completion();
    let Ok(live) = live().lock() else {
        return;
    };
    // The active tab, because `:tab-focus` completed and accepted is only proved by the tab that
    // is showing afterwards.
    let (active, tabs) = crate::state::BruState::instance()
        .and_then(|state| state.lock().ok().map(|state| (state.active_tab(), state.tab_count())))
        .unwrap_or((0, 0));
    eprintln!(
        "completion: line={text:?} cursor={cursor} model={:?} pattern={:?} selected={:?} \
         active={active}/{tabs}",
        live.part.as_ref().map(|part| part.which),
        live.part.as_ref().map(|part| part.center.clone()),
        live.selected,
    );
    for (c, cat) in live.cats.iter().enumerate() {
        for (i, item) in cat.items.iter().enumerate() {
            eprintln!(
                "completion:   {} [{c},{i}] {} | {}",
                if live.selected == Some((c, i)) { ">" } else { " " },
                cat.name,
                item.cols.join(" | "),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::completion::{Flow, HistoryRow, Item, Sources};

    // ---- which model, and what the pattern is ----

    fn part(text: &str) -> Option<Partition> {
        partition(text, text.chars().count())
    }

    #[test]
    fn a_command_with_no_model_completes_nothing() {
        assert!(part(":").is_none(), "the command-name model is not built");
        assert!(part(":scroll down").is_none());
        assert!(part("/duck").is_none(), "a search is not a command");
        assert!(part("").is_none());
    }

    #[test]
    fn open_takes_the_rest_of_the_line_as_one_pattern() {
        // maxsplit=0 is why `:open rust vec` filters history on two terms rather than completing a
        // second argument.
        let part = part(":open rust vec").unwrap();
        assert_eq!(part.which, Which::Url);
        assert_eq!(part.center, "rust vec");
        assert_eq!(part.before, [":open".trim_start_matches(':')]);
        assert!(part.maxsplit0);
    }

    #[test]
    fn opens_flags_stay_with_the_command_and_survive_a_completion() {
        // `O` is `cmd-set-text -s :open -t`, so this is what the line looks like when the user
        // starts typing after it.
        let part = part(":open -t duck").unwrap();
        assert_eq!(part.before, ["open", "-t"]);
        assert_eq!(part.center, "duck");
        assert_eq!(part.line_with("https://duckduckgo.com/"), ":open -t https://duckduckgo.com/");
    }

    #[test]
    fn a_bare_open_completes_everything() {
        let spaced = part(":open ").unwrap();
        assert_eq!(spaced.center, "");
        assert_eq!(spaced.line_with("https://a/"), ":open https://a/");
        // And with no trailing space at all, which is what `:open` alone is.
        assert_eq!(part(":open").unwrap().center, "");
    }

    #[test]
    fn a_flag_and_an_explicit_end_of_flags_complete_nothing() {
        // `completer.py:83`.
        assert!(part(":open -").is_none());
        assert!(part(":open --").is_none());
        assert!(part(":tab-focus -").is_none());
    }

    #[test]
    fn the_cursor_decides_which_part_is_being_completed() {
        // Cursor inside `open`, not in its argument: the argument is not the part under it.
        assert!(partition(":open duck", 3).is_none());
        // Cursor at the end of the first argument.
        assert_eq!(partition(":tab-focus 1", 12).unwrap().center, "1");
        // And in the gap after it, which is a second argument nothing claims.
        assert!(partition(":tab-focus 1 ", 13).is_none());
    }

    #[test]
    fn every_model_is_reachable_by_the_command_that_wants_it() {
        assert_eq!(part(":tab-select 2").unwrap().which, Which::Tabs { special: false });
        assert_eq!(part(":tab-focus 2").unwrap().which, Which::Tabs { special: true });
        assert_eq!(part(":quickmark-load g").unwrap().which, Which::Quickmark);
        assert_eq!(part(":bookmark-load h").unwrap().which, Which::Bookmark);
    }

    #[test]
    fn a_value_with_a_space_in_it_is_quoted_unless_the_command_takes_the_rest_of_the_line() {
        // `completer.py:106-117,170`.
        let open = part(":open x").unwrap();
        assert_eq!(open.line_with("rust vec"), ":open rust vec");
        let tab = part(":tab-focus x").unwrap();
        assert_eq!(tab.line_with("a b"), ":tab-focus 'a b'");
        assert_eq!(tab.line_with("it's"), ":tab-focus 'it'\"'\"'s'");
    }

    #[test]
    fn text_after_the_completed_part_is_kept() {
        let part = partition(":tab-focus x y", 12).unwrap();
        assert_eq!(part.center, "x");
        assert_eq!(part.after, ["y"]);
        assert_eq!(part.line_with("2"), ":tab-focus 2 y");
    }

    // ---- moving the selection ----

    /// A table with the shape the three hard cases need: two categories, the first with three
    /// rows and the second with two.
    fn table() -> Live {
        let cat = |name: &'static str, rows: &[&str]| Category {
            name,
            widths: TAB_WIDTHS,
            items: rows
                .iter()
                .map(|text| Item { cols: vec![text.to_string()], matches: Vec::new() })
                .collect(),
        };
        Live {
            cats: vec![cat("A", &["a1", "a2", "a3"]), cat("B", &["b1", "b2"])],
            ..Live::default()
        }
    }

    #[test]
    fn next_and_prev_start_at_opposite_ends() {
        // `completionwidget.py:180-184`: nothing selected yet.
        assert_eq!(table().step(FocusWhich::Next), Some((0, 0)));
        assert_eq!(table().step(FocusWhich::Prev), Some((1, 1)));
    }

    #[test]
    fn the_selection_wraps_at_both_ends() {
        // `:203-206`.
        let mut live = table();
        live.selected = Some((1, 1));
        assert_eq!(live.step(FocusWhich::Next), Some((0, 0)));
        live.selected = Some((0, 0));
        assert_eq!(live.step(FocusWhich::Prev), Some((1, 1)));
    }

    #[test]
    fn next_crosses_a_category_boundary_without_stopping_on_the_header() {
        // `:207-210` — a header is never a selection.
        let mut live = table();
        live.selected = Some((0, 2));
        assert_eq!(live.step(FocusWhich::Next), Some((1, 0)));
        live.selected = Some((1, 0));
        assert_eq!(live.step(FocusWhich::Prev), Some((0, 2)));
    }

    #[test]
    fn category_movement_lands_on_the_first_row_and_wraps() {
        // `_next_category_idx`, `:258-286`.
        let mut live = table();
        live.selected = Some((0, 2));
        assert_eq!(live.step(FocusWhich::NextCategory), Some((1, 0)));
        live.selected = Some((1, 1));
        assert_eq!(live.step(FocusWhich::NextCategory), Some((0, 0)));
        live.selected = Some((1, 1));
        assert_eq!(live.step(FocusWhich::PrevCategory), Some((0, 0)));
        live.selected = Some((0, 1));
        assert_eq!(live.step(FocusWhich::PrevCategory), Some((1, 0)));
        // With nothing selected: down is the first category, up is the last one's first row.
        assert_eq!(table().step(FocusWhich::NextCategory), Some((0, 0)));
        assert_eq!(table().step(FocusWhich::PrevCategory), Some((1, 0)));
    }

    #[test]
    fn a_page_stops_at_the_border_before_it_wraps() {
        // `_next_page`, `:246-254`. Five rows is less than a page, so the first `<PgDown>` goes to
        // the last row and only the second one wraps.
        let mut live = table();
        live.selected = Some((0, 1));
        assert_eq!(live.step(FocusWhich::NextPage), Some((1, 1)));
        live.selected = Some((1, 1));
        assert_eq!(live.step(FocusWhich::NextPage), Some((0, 0)));
        live.selected = Some((1, 0));
        assert_eq!(live.step(FocusWhich::PrevPage), Some((0, 0)));
        live.selected = Some((0, 0));
        assert_eq!(live.step(FocusWhich::PrevPage), Some((1, 1)));
    }

    #[test]
    fn a_page_is_a_page_when_there_is_one_to_move() {
        let cat = Category {
            name: "A",
            widths: TAB_WIDTHS,
            items: (0..40)
                .map(|i| Item { cols: vec![i.to_string()], matches: Vec::new() })
                .collect(),
        };
        let mut live = Live { cats: vec![cat], ..Live::default() };
        live.selected = Some((0, 0));
        assert_eq!(live.step(FocusWhich::NextPage), Some((0, PAGE)));
        live.selected = Some((0, 20));
        assert_eq!(live.step(FocusWhich::PrevPage), Some((0, 20 - PAGE)));
    }

    #[test]
    fn an_empty_table_moves_nowhere() {
        // The case that panics an implementation that indexes before it looks.
        let live = Live::default();
        for which in [
            FocusWhich::Next,
            FocusWhich::Prev,
            FocusWhich::NextCategory,
            FocusWhich::PrevCategory,
            FocusWhich::NextPage,
            FocusWhich::PrevPage,
        ] {
            assert_eq!(live.step(which), None, "{which} moved in an empty table");
        }
    }

    // ---- the whole way through: a pattern, a table, a selection, a line ----

    struct Fixture;

    impl Sources for Fixture {
        fn search_engines(&self) -> Vec<(String, String)> {
            vec![("duckduckgo".into(), "https://duckduckgo.com/?q={}".into())]
        }
        fn quickmarks(&self) -> Vec<(String, String)> {
            vec![("https://duckduckgo.com/".into(), "du".into())]
        }
        fn bookmarks(&self) -> Vec<(String, String)> {
            Vec::new()
        }
        fn history(&self, _pattern: &str, visit: &mut dyn FnMut(HistoryRow<'_>) -> Flow) {
            let _ = visit(HistoryRow {
                url: "https://duckduckgo.com/?q=rust",
                title: "rust at DuckDuckGo",
                atime: "2026-08-06 13:51",
            });
        }
    }

    /// [`build`] against an explicit source, since `install` is never called in the test binary.
    fn table_for(text: &str) -> (Partition, Vec<Category>) {
        let part = part(text).unwrap();
        let cats = match part.which {
            Which::Url => crate::completion::categories_from(&part.center, &Fixture),
            other => build(other, &part.center),
        };
        (part, cats)
    }

    #[test]
    fn tabbing_through_open_writes_each_row_into_the_line() {
        let (part, cats) = table_for(":open du");
        let mut live = Live { cats, part: Some(part), ..Live::default() };
        // The order the eye reads: the engine, then the quickmark, then the history row.
        let mut lines = Vec::new();
        for _ in 0..4 {
            let to = live.step(FocusWhich::Next).unwrap();
            live.selected = Some(to);
            let item = live.item(to).unwrap();
            lines.push(live.part.as_ref().unwrap().line_with(&item.cols[0]));
        }
        assert_eq!(
            lines,
            [
                ":open duckduckgo",
                ":open https://duckduckgo.com/",
                ":open https://duckduckgo.com/?q=rust",
                // And round again.
                ":open duckduckgo",
            ]
        );
    }

    #[test]
    fn the_tab_model_offers_an_index_a_url_and_a_title() {
        // Built by hand rather than through `build`, which needs a live BruState.
        let rows = vec![
            vec!["1".into(), "https://a.example/".into(), "A".into()],
            vec!["2".into(), "https://b.example/".into(), "B".into()],
        ];
        let cat = completion::list_category("Tabs", TAB_WIDTHS, rows, "b").unwrap();
        assert_eq!(cat.items.len(), 1);
        assert_eq!(cat.items[0].cols[0], "2");
        // The filter is the list one: all terms inside a single column.
        assert_eq!(cat.widths, TAB_WIDTHS);
    }

    #[test]
    fn the_special_category_is_only_offered_where_the_command_takes_it() {
        // `:tab-focus last` is real; `:tab-select last` is not.
        assert_eq!(part(":tab-focus l").unwrap().which, Which::Tabs { special: true });
        let cats = build(Which::Tabs { special: true }, "last");
        assert_eq!(cats.last().map(|cat| cat.name), Some("Special"));
        let cats = build(Which::Tabs { special: false }, "last");
        assert!(cats.iter().all(|cat| cat.name != "Special"));
    }
}
