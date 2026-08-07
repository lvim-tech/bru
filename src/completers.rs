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
//! ## Three things here are deliberately not qutebrowser's
//!
//! 1. **`/` and `?` complete.** qutebrowser refuses them — `completer.py:213-219` sets no model for
//!    any prefix but `:`, over a comment reading "FIXME complete searches" and its issue #32 — and
//!    bru offers what has been searched for before. Asked for by the user on 2026-08-07. The store
//!    is `src/find.rs`'s and the reasoning for its shape is there; what is here is the branch in
//!    [`partition`] that used to be the refusal.
//! 2. **`completion.quick` is not implemented.** qutebrowser applies a lone match immediately,
//!    with a trailing space, and moves on to the next argument (`completer.py:172-186`). It reads
//!    as the line typing ahead of you and it depends on a per-command `maxsplit` bru's parser does
//!    not expose; a selection here always waits to be accepted.
//! 3. **The caret lands at the end of the line**, not at the end of the completed part. bru's
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
    /// `:` alone, `:m`, and the second argument of `:bind` — every command bru understands, with
    /// what it does and the keys that reach it (`miscmodels.py:17-23`, `util.py:16-43`). The rows
    /// are `src/help.rs`'s, because the join to the key table is already written there.
    Commands,
    /// `:open` — search engines, quickmarks, bookmarks, history.
    Url,
    /// `:tab-select`, `:tab-focus`. `special` adds qutebrowser's Special category, which is where
    /// `tab-focus last` comes from (`miscmodels.py:176-189`).
    Tabs { special: bool },
// --- src/utilcmds.rs -------------------------------------------------------
    /// `:tab-select` — every window's tabs, one category per window, each row addressed
    /// `<win-id>/<index>` (`miscmodels.py:91-155`, `add_win_id=True`).
    ///
    /// A separate variant from [`Which::Tabs`] rather than another flag on it: the rows are
    /// *addressed differently*, and column 0 is what `<Tab>` writes into the line. `:tab-focus 2`
    /// and `:tab-select 0/2` are two different commands' spellings of the same tab.
    AllTabs,
    /// `:tab-take` — the same, minus this window's own tabs (`miscmodels.py:168-174`,
    /// `other_tabs`). A tab in this window is the one thing `:tab-take` refuses.
    OtherTabs,
// --- end src/utilcmds.rs ---------------------------------------------------
    /// `:quickmark-load` and friends — the name is what the command takes, so it leads.
    Quickmark,
    /// `:bookmark-load` and friends — a URL is what the command takes.
    Bookmark,
    /// The option argument of `:set` and the six `config-*` commands that take one
    /// (`configmodel.py:13-47`). [`Only`] is which of qutebrowser's four option models this is.
    Setting(Only),
    /// `:set <option> <value>` and `:config-cycle <option> <values…>` — `configmodel.py:66-94`.
    /// Which values are offered depends on the option named in the argument before this one.
    SettingValue,
    /// `:config-dict-add <option> <key>` and `:config-dict-remove <option> <key>`.
    ///
    /// **bru's own, with no qutebrowser counterpart**: `configcommands.py:310,370` declare a
    /// completion for the *option* of both and none for the key. The key is finite and knowable —
    /// `statusbar.mode.labels` has exactly the twelve labels the pill can draw and refuses a
    /// thirteenth — and a `:config-dict-remove url.searchengines <Tab>` that cannot name the nine
    /// engines is a completion that stops one word short of the word you needed.
    DictKey,
    /// `:config-list-add <option> <value>` and `:config-list-remove <option> <value>`, for the
    /// same reason as [`Which::DictKey`]: the entries a list holds are a list.
    ListEntry,
    /// `/` and `?` — everything searched for before, newest first. See [`partition`] for why this
    /// exists at all, and `src/find.rs` for the store behind it.
    SearchHistory,
    /// `:mode-enter <mode>` — the modes a command may put a window into, which is eight of the
    /// twelve. bru's own: qutebrowser declares no completion for `:mode-enter`, and the four it
    /// would have to leave out are the four `Mode::can_be_entered_by_command` already names.
    ModeName,
    /// `:session-save`, `:session-load`, `:session-delete` — the files under `sessions/`
    /// (`miscmodels.py:76-87`).
    Session,
}

/// Which settings an option model offers — `configmodel.py`'s four, less the customized one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Only {
    /// `configmodel.option` — all of them. `:set`, `:config-cycle`, and `:config-unset`.
    ///
    /// `:config-unset` is qutebrowser's `customized_option`, which lists only the options that
    /// have been changed (`configcommands.py:235`), and bru offers all of them instead. The store
    /// knows which have moved — `Settings::diff` is built out of exactly that — but it answers in
    /// `config.lua` syntax rather than in names, and `Settings::entries` is private to
    /// `settings.rs`, which is another workstream's file. The cost of the wider list is that
    /// unsetting something already at its default does nothing, which is what `:config-unset`
    /// means anyway.
    Any,
    /// `configmodel.dict_option` — the two dictionaries, for the two `config-dict-*` commands.
    Dicts,
    /// `configmodel.list_option` — the list settings, for the two `config-list-*` commands.
    Lists,
}

/// One row of the table that turns a command into a model.
///
/// `argpos` is which positional argument, counting from zero after the command name, this model
/// completes; `rest_from` is qutebrowser's `maxsplit` — the argument index at which the rest of the
/// line stops being split and becomes one argument, verbatim, spaces included. `:open` is
/// `Some(0)`, which is the only reason `:open rust vec` can be two search terms rather than two
/// arguments; `:bind` is `Some(1)`, so the key is a word and the command it is bound to is
/// everything after it (`commands/parser.py:177-205`).
///
/// `rest_from` is a property of the *command*, not of one of its arguments, so every row sharing a
/// name has to agree about it — `every_command_agrees_with_itself_about_maxsplit` says so.
struct Spec {
    name: &'static str,
    argpos: usize,
    which: Which,
    rest_from: Option<usize>,
    /// `*args`: this row answers `argpos` and every position after it, the way
    /// `Command.get_pos_arg_info` clamps a position to the last parameter when there is a vararg
    /// (`commands/command.py:165-170`). `:config-cycle option v1 v2 v3` completes a value at every
    /// one of the three.
    vararg: bool,
}

/// Every command bru completes. Adding a model is a line here and an arm in [`build_which`].
///
/// What is deliberately absent, and why, is in the report and in this file's tests.
const SPECS: &[Spec] = &[
    Spec { name: "open", argpos: 0, which: Which::Url, rest_from: Some(0), vararg: false },
// --- src/utilcmds.rs -------------------------------------------------------
    // The rest of the line on both, because both are registered that way (`commands.py:430`,
    // `:930`) and because their argument may be a title fragment with a space in it.
    Spec { name: "tab-select", argpos: 0, which: Which::AllTabs, rest_from: Some(0), vararg: false },
    Spec { name: "tab-take", argpos: 0, which: Which::OtherTabs, rest_from: Some(0), vararg: false },
// --- end src/utilcmds.rs ---------------------------------------------------
    Spec { name: "tab-focus", argpos: 0, which: Which::Tabs { special: true }, rest_from: None, vararg: false },
    Spec { name: "quickmark-load", argpos: 0, which: Which::Quickmark, rest_from: None, vararg: false },
    Spec { name: "quickmark-del", argpos: 0, which: Which::Quickmark, rest_from: None, vararg: false },
    Spec { name: "bookmark-load", argpos: 0, which: Which::Bookmark, rest_from: None, vararg: false },
    Spec { name: "bookmark-del", argpos: 0, which: Which::Bookmark, rest_from: None, vararg: false },

    // --- the settings, `configmodel.py` ---------------------------------------------------------
    // `:set` takes an option and a value, and neither is the rest of the line: qutebrowser
    // registers it with no `maxsplit` (`configcommands.py:69`), so a value with a space in it is
    // quoted rather than swallowed. `:config-cycle` is `*values`, which is `vararg`.
    Spec { name: "set", argpos: 0, which: Which::Setting(Only::Any), rest_from: None, vararg: false },
    Spec { name: "set", argpos: 1, which: Which::SettingValue, rest_from: None, vararg: false },
    Spec { name: "config-cycle", argpos: 0, which: Which::Setting(Only::Any), rest_from: None, vararg: false },
    Spec { name: "config-cycle", argpos: 1, which: Which::SettingValue, rest_from: None, vararg: true },
    Spec { name: "config-unset", argpos: 0, which: Which::Setting(Only::Any), rest_from: None, vararg: false },
    Spec { name: "config-dict-add", argpos: 0, which: Which::Setting(Only::Dicts), rest_from: None, vararg: false },
    Spec { name: "config-dict-add", argpos: 1, which: Which::DictKey, rest_from: None, vararg: false },
    Spec { name: "config-dict-remove", argpos: 0, which: Which::Setting(Only::Dicts), rest_from: None, vararg: false },
    Spec { name: "config-dict-remove", argpos: 1, which: Which::DictKey, rest_from: None, vararg: false },
    Spec { name: "config-list-add", argpos: 0, which: Which::Setting(Only::Lists), rest_from: None, vararg: false },
    Spec { name: "config-list-add", argpos: 1, which: Which::ListEntry, rest_from: None, vararg: false },
    Spec { name: "config-list-remove", argpos: 0, which: Which::Setting(Only::Lists), rest_from: None, vararg: false },
    Spec { name: "config-list-remove", argpos: 1, which: Which::ListEntry, rest_from: None, vararg: false },
    // --- end the settings -----------------------------------------------------------------------

    // `:bind <key> <command…>` — the key is a word and the command is everything after it, which is
    // `maxsplit=1` (`configcommands.py:119`). The **key** is deliberately not completed: a `:bind`
    // usually names a key that is not bound yet, so there is no set to offer, and qutebrowser
    // offers the current binding for it as part of the *command* model instead
    // (`configmodel._bind_current_default`) — which bru does not, because that model is one
    // category of 166 rows here and a two-row Current/Default in front of it would push the list
    // down for something the status bar can say.
    Spec { name: "bind", argpos: 1, which: Which::Commands, rest_from: Some(1), vararg: false },

    Spec { name: "mode-enter", argpos: 0, which: Which::ModeName, rest_from: None, vararg: false },
    Spec { name: "session-save", argpos: 0, which: Which::Session, rest_from: None, vararg: false },
    Spec { name: "session-load", argpos: 0, which: Which::Session, rest_from: None, vararg: false },
    Spec { name: "session-delete", argpos: 0, which: Which::Session, rest_from: None, vararg: false },
];

/// The command line cut into the part being completed and the parts around it.
///
/// `completer.py:119-155`. Held between keystrokes so that moving the selection can rebuild the
/// line without re-running the parser.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Partition {
    /// `:`, `/` or `?` — the three prefixes bru's command line takes, and all three complete.
    prefix: char,
    /// The command name and any flags before the cursor, in order.
    before: Vec<String>,
    /// The pattern, as typed.
    center: String,
    /// Everything after the completed part. Empty for every default binding.
    after: Vec<String>,
    which: Which,
    /// Whether the completed part is the rest of the line, verbatim — see [`Spec::rest_from`].
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

    /// The first positional argument, which for every command in [`SPECS`] that has a second one is
    /// the option being set. `configmodel.value(optname, …)` is handed it the same way — from the
    /// parts before the cursor (`completer.py:236-241`).
    ///
    /// It skips flags by their leading `-` and therefore mistakes the *value* of `-u` for a
    /// positional, exactly as `_get_new_completion` does — see the note on `argpos` in
    /// [`partition`]. `:set -u https://example.com/ content.images ` completes the values of
    /// nothing, rather than of `content.images`.
    fn option(&self) -> Option<&str> {
        self.before
            .iter()
            .skip(1)
            .map(String::as_str)
            .find(|part| !part.starts_with('-'))
    }

    /// The arguments already typed after the option, which `configmodel.value` takes as `*values`
    /// and leaves out of what it offers (`configmodel.py:88,92`). One value per `:set`; a list of
    /// them per `:config-cycle`.
    fn values_before(&self) -> Vec<&str> {
        self.before
            .iter()
            .skip(1)
            .map(String::as_str)
            .filter(|part| !part.starts_with('-'))
            .skip(1)
            .collect()
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
/// nothing: an empty line, a flag, anything after a `--`, an unknown command, or an argument
/// position no model claims.
fn partition(text: &str, cursor: usize) -> Option<Partition> {
    let chars: Vec<char> = text.chars().collect();
    let prefix = *chars.first()?;
    if !matches!(prefix, ':' | '/' | '?') {
        return None;
    }
    // Positions from here on are into the body, which is what the cursor is measured against too.
    let body = &chars[1..];
    let at = cursor.saturating_sub(1).min(body.len());

    // **`/` and `?` complete, and this is the departure.** The line these two replace said
    // "`/` and `?` are a search (`completer.py:213-219`)" and refused, which was a true reading of
    // qutebrowser: `_update_completion` sets no model for a prefix that is not `:`, over a comment
    // reading "FIXME complete searches" and its issue #32. The user asked for the opposite on
    // 2026-08-07 — typing `/` should offer what has been searched for before — so bru keeps a
    // search history and offers it here. `src/find.rs` owns the store and says why it is shaped the
    // way it is.
    //
    // The whole line after the prefix is one part, never split, and that is not a shortcut: a
    // search term is the rest of the line, spaces included, exactly as `Cmdline::accept` hands it
    // to `search -- <rest>`. `maxsplit0` is what stops `<Tab>` quoting a two-word term into
    // something `search` would then look for literally.
    if prefix != ':' {
        return Some(Partition {
            prefix,
            before: Vec::new(),
            center: body.iter().collect(),
            after: Vec::new(),
            which: Which::SearchHistory,
            maxsplit0: true,
        });
    }

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

    // **Nothing before the cursor is the command-name model** — `_get_new_completion`'s first
    // branch, `if not before_cursor: return miscmodels.command` (`completer.py:87-90`), with the
    // comment `'|' or 'set|'` naming both cases. `:` alone and `:   ` are the first; a cursor
    // still inside the command's own name is the second, and its pattern is that half-typed name.
    // This is where the file used to say "bru has no list of command names to open it from"; it
    // has two now, `help.rs::COMMANDS` and the key table it is joined to.
    let Some((&first, rest)) = tokens.split_first() else {
        return Some(command_name_partition(prefix, String::new(), Vec::new()));
    };
    let name = text_of(first);
    if at <= first.1 {
        // Whatever follows is kept, so completing `:ope|n duck` leaves `duck` where it was —
        // `_partition` returns it as the postfix (`completer.py:154`).
        let after = rest.iter().map(|&token| text_of(token)).collect();
        return Some(command_name_partition(prefix, name, after));
    }

    let named: Vec<&Spec> = SPECS.iter().filter(|spec| spec.name == name).collect();
    let first_spec = *named.first()?;

    // The token under the cursor is the pattern. A cursor sitting in the whitespace between two
    // tokens completes an empty pattern there, as `_partition` does by inserting an empty part
    // (`completer.py:143-146`).
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
    // A flag's *value* is counted as a positional here, because `_get_new_completion` filters on
    // nothing but the leading `-` and so miscounts `:set -u <pattern> …` in exactly the same way.
    let mut argpos = before.iter().filter(|part| !part.starts_with('-')).count().saturating_sub(1);
    let mut maxsplit0 = false;

    // qutebrowser's `maxsplit`: from argument `rest_from` on, the line stops being split and the
    // whole remainder is one part. Re-cut rather than special-cased above, so that the walk that
    // decides *which* argument the cursor is in is written once.
    if let Some(from_arg) = first_spec.rest_from {
        if argpos >= from_arg {
            let mut kept: Vec<String> = vec![before[0].clone()];
            let mut seen = 0usize;
            let mut from = body.len();
            for &token in rest {
                let part = text_of(token);
                if part.starts_with('-') {
                    // An explicit end of flags stops the completion outright.
                    if part == "--" {
                        return None;
                    }
                    kept.push(part);
                    continue;
                }
                if seen == from_arg {
                    from = token.0;
                    break;
                }
                seen += 1;
                kept.push(part);
            }
            // The cursor has to be inside the argument, not back in an earlier one.
            if at < from {
                return None;
            }
            before = kept;
            center = body[from..].iter().collect();
            after = Vec::new();
            argpos = from_arg;
            maxsplit0 = true;
        }
    }

    let spec = named
        .iter()
        .find(|spec| spec.argpos == argpos || (spec.vararg && argpos >= spec.argpos))?;

    Some(Partition {
        prefix,
        before,
        center,
        after,
        which: spec.which,
        maxsplit0,
    })
}

/// `_get_new_completion`'s first branch: the command-name model, with nothing before the cursor.
fn command_name_partition(prefix: char, center: String, after: Vec<String>) -> Partition {
    Partition {
        prefix,
        before: Vec::new(),
        center,
        after,
        which: Which::Commands,
        // A command name has no spaces in it, so quoting is a no-op — but it is the honest value:
        // this part is one token and not the rest of the line.
        maxsplit0: false,
    }
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

// --- src/utilcmds.rs -------------------------------------------------------
/// One window's rows, filtered by the pattern, as a category named after the window. Called with an
/// empty `rows` at the start and at the end, where it does nothing.
fn push_window(out: &mut Vec<Category>, window: Option<u32>, rows: &mut Vec<Vec<String>>, pattern: &str) {
    let Some(window) = window else {
        rows.clear();
        return;
    };
    let taken = std::mem::take(rows);
    if let Some(category) = completion::list_category(window_name(window), TAB_WIDTHS, taken, pattern)
    {
        out.push(category);
    }
}

/// A window id as a `&'static str`, which is what `Category::name` is.
///
/// Interned rather than leaked per call, because [`build`] runs on **every keystroke** of a
/// `:tab-select …` and a leak there would grow with the typing. What this does leak is one short
/// string per window id the session has ever shown a completion for — a handful of bytes, once, for
/// a number that never comes back.
fn window_name(id: u32) -> &'static str {
    static NAMES: Mutex<Vec<(u32, &'static str)>> = Mutex::new(Vec::new());
    let Ok(mut names) = NAMES.lock() else {
        return "window";
    };
    if let Some((_, name)) = names.iter().find(|(known, _)| *known == id) {
        return name;
    }
    let name: &'static str = Box::leak(id.to_string().into_boxed_str());
    names.push((id, name));
    name
}
// --- end src/utilcmds.rs ---------------------------------------------------
/// `miscmodels.py:49,68` — `column_widths=(30, 70, 0)`.
const MARK_WIDTHS: &[u8] = &[30, 70];

/// `miscmodels.py:19` — `column_widths=(20, 60, 20)`: the name, what it does, the keys.
const COMMAND_WIDTHS: &[u8] = &[20, 60, 20];

/// The command rows, and the key table they were joined against.
///
/// Cached because the join is not free — measured 2026-08-07, debug build: `reached` over the 298
/// default bindings costs 1.46 ms, which would be paid on **every keystroke** of a `:` line, and
/// the answer only changes when `:bind` or `:unbind` changes the table. The key it is cached under
/// is the binding table itself, so a rebind invalidates it without anything having to remember to:
/// `Bindings::all()` costs 0.53 ms of that 1.46 and comparing two of them is a memcmp.
///
/// What a keystroke costs with the cache warm, measured the same day and the same way: **0.89 ms**
/// for the whole list and **2.3 ms** once a pattern is filtering it — the second is larger because
/// a filter makes `list_matches` fold all three columns of all 166 rows instead of short-circuiting
/// on the first. The settings model beside it is 0.30 ms. **None of this is on the key path**: the
/// completion is built when the text of a command line changes, and `j` in normal mode reads an
/// atomic in 0.3 ns and never comes near here.
///
/// Before `BruState` exists — a unit test, or a renderer process — the defaults are the truth, the
/// same fallback `hints.rs` and `prompt.rs` take.
fn command_rows() -> Vec<Vec<String>> {
    static CACHE: Mutex<Option<(Vec<(Mode, String, String)>, Vec<Vec<String>>)>> = Mutex::new(None);
    // `Bindings::defaults` builds the whole 298-row table from its source text every call, which is
    // most of a millisecond; the live one is already built and is only cloned. Neither is worth
    // paying twice, so the fallback is built once as well.
    static DEFAULTS: std::sync::OnceLock<crate::config::Bindings> = std::sync::OnceLock::new();

    let live = crate::state::BruState::instance()
        .and_then(|state| state.lock().ok().and_then(|state| state.bindings_snapshot()));
    let bindings = match &live {
        Some(bindings) => bindings,
        None => DEFAULTS.get_or_init(crate::config::Bindings::defaults),
    };
    let key = bindings.all();

    let Ok(mut cache) = CACHE.lock() else {
        return crate::help::completion_rows(bindings);
    };
    if let Some((known, rows)) = cache.as_ref() {
        if *known == key {
            return rows.clone();
        }
    }
    let rows = crate::help::completion_rows(bindings);
    *cache = Some((key, rows.clone()));
    rows
}

// -----------------------------------------------------------------------------------------------
// The settings, `configmodel.py`
// -----------------------------------------------------------------------------------------------

/// `configmodel.py:60` — `column_widths=(20, 70, 10)`: the name, what it takes, what it is.
const OPTION_WIDTHS: &[u8] = &[20, 70, 10];

/// `configmodel.py:73` — `column_widths=(30, 70, 0)`.
const VALUE_WIDTHS: &[u8] = &[30, 70];

/// What a setting takes, in one line.
///
/// **This is the same question `settingspage::kind` answers, and it is written twice on purpose.**
/// That module says why in its own words about `escape`: "duplicated rather than shared because
/// `help.rs` belongs to another workstream and a twelve-line pure function is a cheaper thing to
/// repeat than a cross-module dependency is to merge" — and `settings.rs` had three workstreams'
/// uncommitted edits in it on the day this was written, from two other workstreams at once, and one
/// of them was adding a [`Kind`].
///
/// **Which is why the last arm here is `_` and the settings page's is not.** The exhaustive match —
/// the one a new kind cannot get past without somebody deciding what it takes — belongs in the
/// module that owns the page settings are read on, and it is there. Here a new kind falls through
/// to `text`, which is not a guess: `:set` takes its value as a string whatever the kind parses it
/// into, so "text" is true of every kind bru could add and merely less than the whole truth about
/// some of them. A short answer in a completion cell costs a reader one word; a build that cannot
/// be run until two workstreams have merged costs an afternoon.
fn takes(kind: crate::settings::Kind) -> String {
    use crate::settings::Kind;
    match kind {
        Kind::Bool => "true or false".to_string(),
        Kind::Choice(choices) => choices.join(" or "),
        Kind::Dict(shape) if shape.open_keys => "a dictionary, any key".to_string(),
        Kind::Dict(_) => "a dictionary, fixed keys".to_string(),
        Kind::List(_) => "a list".to_string(),
        Kind::Int(shape) => format!("a whole number, {} to {} {}", shape.min, shape.max, shape.unit),
        Kind::Chars => "at least two different characters, no spaces".to_string(),
        // `Kind::Text`, and anything added after this was written.
        _ => "text".to_string(),
    }
}

/// What a setting is set to, as the option model's third column.
///
/// bru's own store, not Chromium's, and that is qutebrowser's choice too:
/// `info.config.get_str(opt.name)` is the config's value and not what the engine is enforcing
/// (`configmodel.py:61`). It is also the only one available here — `chromium_value` has to be
/// called on the UI thread and needs a URL to be asked about, and the completion has neither to
/// hand. `Settings::get` is the same reader `:set <option>?` prints through.
///
/// A [`crate::settings::Scopes::UrlOnly`] setting has no global value at all and `get` says so
/// with an error; what is in force at a URL with no rule written for it is bru's own default, so
/// that is what the column shows.
fn in_force(store: &crate::settings::Settings, def: &crate::settings::Def) -> String {
    match store.get(def.name, None) {
        Ok(Some(value)) => value.to_string(),
        _ => def.default.unwrap_or("unset").to_string(),
    }
}

/// The first sentence of a refusal, which is as much of one as a completion row can hold.
///
/// The whole reason is prose — `content.plugins`'s is 90 words — and it is printed in full on
/// `bru://chrome/settings` and by `:set` itself when the name is typed. What a row here has to do
/// is stop the reader typing the rest of the name, and the first sentence does that.
fn first_sentence(why: &str) -> String {
    match why.split_once(". ") {
        Some((first, _)) => format!("{first}."),
        None => why.to_string(),
    }
}

/// `:set <option> <value>` — what the value may be.
///
/// `configmodel.value` (`configmodel.py:66-94`), which is two categories and not one: what the
/// option is set to now and what bru ships it as, then the values the *kind* allows. Both drop a
/// value that is already on the command line.
///
/// **A number offers no completions and this says so rather than inventing a range.** `Kind::Int`
/// carries a minimum and a maximum and could spell out every value between them; `messages.timeout`
/// is 0 to 86,400,000. What it gets instead is the Current/Default category, which is the two
/// numbers anyone actually wants to type — the one it is on and the one to put it back to.
/// `Kind::Text` and `Kind::Chars` are the same. `qutebrowser`'s `typ.complete()` answers `None` for
/// all three.
///
/// **A dict or a list offers nothing at all, not even the current value.** `:set` takes one value
/// and those two are a table; the current "value" of `url.searchengines` is the string `9 entries`,
/// which is a count and not something to put in a command line. The commands that *do* change one
/// pair at a time are `:config-dict-add` and friends, and their arguments are completed above.
fn setting_values(option: Option<&str>, already: &[&str], pattern: &str) -> Vec<Category> {
    use crate::settings::Kind;

    let Some(def) = option.and_then(crate::settings::def) else {
        return Vec::new();
    };
    if matches!(def.kind, Kind::Dict(_) | Kind::List(_)) {
        return Vec::new();
    }

    let mut out = Vec::with_capacity(2);

    let store = crate::settings::snapshot();
    let current = in_force(&store, def);
    let default = def.default.unwrap_or_default().to_string();
    let mut rows: Vec<Vec<String>> = Vec::with_capacity(2);
    if !already.contains(&current.as_str()) && !current.is_empty() {
        rows.push(vec![current.clone(), "Current value".to_string()]);
    }
    if !already.contains(&default.as_str()) && !default.is_empty() && default != current {
        rows.push(vec![default, "Default value".to_string()]);
    }
    if let Some(cat) = completion::list_category("Current/Default", VALUE_WIDTHS, rows, pattern) {
        out.push(cat);
    }

    let values: &[&str] = match def.kind {
        // `configtypes.Bool.complete()` — the two canonical spellings, though the parser also takes
        // yes/no/on/off/1/0.
        Kind::Bool => &["true", "false"],
        Kind::Choice(choices) => choices,
        // Every other kind, including any added after this was written: a value it can be told is
        // not one of a set anybody can list. `Kind::Int`'s range is 2^64 wide and `Kind::Text`'s is
        // every string, and `typ.complete()` answers `None` for both. What is left is the
        // Current/Default category above, which is the two values a person actually types.
        _ => &[],
    };
    let rows: Vec<Vec<String>> = values
        .iter()
        .filter(|value| !already.contains(*value))
        .map(|value| vec![value.to_string(), String::new()])
        .collect();
    if let Some(cat) = completion::list_category_max(
        "Completions",
        VALUE_WIDTHS,
        rows,
        pattern,
        usize::MAX,
    ) {
        out.push(cat);
    }
    out
}

fn build(part: &Partition) -> Vec<Category> {
    match part.which {
        Which::SettingValue => setting_values(part.option(), &part.values_before(), &part.center),
        Which::DictKey => {
            let rows = part
                .option()
                .map(crate::settings::dict_of)
                .unwrap_or_default()
                .into_iter()
                .map(|(key, value)| vec![key, value])
                .collect();
            completion::list_category_max("Keys", VALUE_WIDTHS, rows, &part.center, usize::MAX)
                .into_iter()
                .collect()
        }
        Which::ListEntry => {
            let rows = part
                .option()
                .map(crate::settings::list_of)
                .unwrap_or_default()
                .into_iter()
                .map(|entry| vec![entry])
                .collect();
            completion::list_category_max("Entries", &[100], rows, &part.center, usize::MAX)
                .into_iter()
                .collect()
        }
        which => build_which(which, &part.center),
    }
}

fn build_which(which: Which, pattern: &str) -> Vec<Category> {
    match which {
        // **Two categories, because a refused setting is not a setting.** The names in
        // `settings::REFUSED` are ones bru will not implement and has a measured reason for; `:set`
        // answers one with that reason instead of a value. Leaving them out of the completion would
        // make `:set content.pl<Tab>` answer with silence, which reads as "bru forgot" — and the
        // reason is the one thing the reader of a name that does nothing needs. Leaving them in the
        // *same* category would be worse: it would offer them as things to set.
        //
        // This is the same rule the command model follows one screen up, applied to the other
        // table: what bru knows a name for is offered, and a name that does nothing says so where
        // it is offered.
        Which::Setting(only) => {
            let store = crate::settings::snapshot();
            let rows: Vec<Vec<String>> = crate::settings::SETTINGS
                .iter()
                .filter(|def| match only {
                    Only::Any => true,
                    Only::Dicts => matches!(def.kind, crate::settings::Kind::Dict(_)),
                    Only::Lists => matches!(def.kind, crate::settings::Kind::List(_)),
                })
                .map(|def| {
                    vec![def.name.to_string(), takes(def.kind), in_force(&store, def)]
                })
                .collect();
            let mut out = Vec::with_capacity(2);
            if let Some(cat) =
                completion::list_category_max("Settings", OPTION_WIDTHS, rows, pattern, usize::MAX)
            {
                out.push(cat);
            }
            // Only where every setting is on offer: `:config-dict-add` takes a dictionary, and none
            // of the refused names is one, so listing them under it would be listing them as
            // dictionaries.
            if only == Only::Any {
                let rows: Vec<Vec<String>> = crate::settings::REFUSED
                    .iter()
                    .map(|(name, why)| {
                        vec![name.to_string(), first_sentence(why), "refused".to_string()]
                    })
                    .collect();
                if let Some(cat) = completion::list_category_max(
                    "Refused",
                    OPTION_WIDTHS,
                    rows,
                    pattern,
                    usize::MAX,
                ) {
                    out.push(cat);
                }
            }
            out
        }

        // Answered by `build`, which has the option named in the argument before this one.
        Which::SettingValue | Which::DictKey | Which::ListEntry => Vec::new(),

        // One column, because a search term is one thing and there is nothing to put beside it: no
        // page it was found on (the same term is searched for on many), no count (Chromium's is per
        // page and per session), no time (bru would have to keep one to show one). A second column
        // holding nothing would take 60% of the bar to say it.
        //
        // Capped at `MAX_ITEMS` unlike the two catalogue models above, and for `:open`'s History
        // reason rather than a new one: this *is* a history, it is offered newest first, and the
        // newest 25 that match are the answer. The file's own bound is 100 — see
        // `find.rs::HISTORY_MAX` — which is what stops the list growing without end.
        // One column each, for `Which::SearchHistory`'s reason: there is nothing to put beside them.
        // A mode's name is the whole of what `mode-enter` takes, and what each mode *is* belongs on
        // `bru://chrome/help`, which lists every mode's bindings under its own heading. A session is
        // a file name; its size and its date are `ls`'s answer, not a completion's.
        Which::ModeName => {
            let rows = Mode::ALL
                .iter()
                .filter(|mode| mode.can_be_entered_by_command())
                .map(|mode| vec![mode.name().to_string()])
                .collect();
            completion::list_category_max("Modes", &[100], rows, pattern, usize::MAX)
                .into_iter()
                .collect()
        }

        Which::Session => {
            let rows = crate::session::list().into_iter().map(|name| vec![name]).collect();
            completion::list_category_max("Sessions", &[100], rows, pattern, usize::MAX)
                .into_iter()
                .collect()
        }

        Which::SearchHistory => {
            let rows = crate::find::history().into_iter().map(|term| vec![term]).collect();
            completion::list_category("Search history", &[100], rows, pattern)
                .into_iter()
                .collect()
        }

        // **Uncapped, unlike every other category here.** `completion::MAX_ITEMS` bounds a source
        // that has no bound of its own — the history table grows with every page load — and 25 of
        // those is "the newest 25 that matched", which is the answer. The command list is 166 rows
        // compiled into the binary, and a `:` that showed 25 of them would answer "what can I
        // type" with a lie. The payload it produces is measured in
        // `the_whole_command_list_still_fits_in_one_push`.
        Which::Commands => completion::list_category_max(
            "Commands",
            COMMAND_WIDTHS,
            command_rows(),
            pattern,
            usize::MAX,
        )
        .into_iter()
        .collect(),

        Which::Url => completion::categories(pattern),

// --- src/utilcmds.rs -------------------------------------------------------
        // One category per window, titled with the window's id, exactly as `_tabs` does when
        // `add_win_id` is on (`miscmodels.py:143-147`) — the id has to be visible somewhere, and a
        // heading is where qutebrowser puts it.
        Which::AllTabs | Which::OtherTabs => {
            let here = crate::state::BruState::instance()
                .and_then(|state| state.lock().ok().and_then(|state| state.current_window_id()));
            let mut out = Vec::new();
            let mut window = None;
            let mut rows: Vec<Vec<String>> = Vec::new();
            // `all_tabs` is in window order, so a category breaks whenever the window changes and
            // nothing has to be grouped again.
            for tab in crate::utilcmds::all_tabs() {
                if which == Which::OtherTabs && Some(tab.window) == here {
                    continue;
                }
                if window != Some(tab.window) {
                    push_window(&mut out, window, &mut rows, pattern);
                    window = Some(tab.window);
                }
                rows.push(vec![
                    format!("{}/{}", tab.window, tab.index + 1),
                    tab.url,
                    tab.title,
                ]);
            }
            push_window(&mut out, window, &mut rows, pattern);
            out
        }
// --- end src/utilcmds.rs ---------------------------------------------------

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
            Some(part) => build(part),
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
/// | `shot:<path>` | write **the bottom strip itself** to a PNG |
pub fn schedule_script(steps: &str, interval_ms: i64) {
    for (i, step) in steps.split(';').filter(|s| !s.is_empty()).enumerate() {
        let mut task = ScriptStep::new(step.to_string());
        post_delayed_task(ThreadId::UI, Some(&mut task), interval_ms * (i as i64 + 1));
    }
}

/// Read once the bottom strip has announced itself, because there is no command line before then.
///
/// **Once for the process, not once per strip**, which this did not do until 2026-08-07. It is the
/// bug `ipc::start_cmdline_script` was already fixed for, in the line above the call to this one:
/// the `ready` answer comes from *a* bottom strip and there is one per window, so a script that
/// opened a second window scheduled itself a second time and every step after that ran twice. The
/// two schedules also interleave on one clock, so the doubling is not even a clean repeat — it is
/// two runs of the same steps offset by however long the second window took to come up. Left
/// unfixed it would have made the two-window half of the checks below unreadable.
pub fn start_script() {
    static STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if STARTED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return;
    }
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
                "shot" => shoot(arg),
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

/// Write the bottom strip to a PNG — the bar as it is actually drawn, and nothing else.
///
/// `:screenshot` captures the *page*, which is a different browser, so it cannot show the bar at
/// all. This aims the same DevTools capture at the chrome browser the bar is, and that is the whole
/// reason it exists: a completion table 166 rows long is a claim about a **height**, and a height in
/// a debug line is not the same claim as a bar on screen.
///
/// It is also the only capture on this machine that cannot photograph the wrong window. A
/// compositor screenshot has to find bru's toplevel, and there is routinely more than one bru
/// running here — measured 2026-08-07, when `grim` over the focused window's geometry returned
/// another bru showing `bru://chrome/help`, which looks exactly like a real answer. This one is
/// taken by the process being checked, from the view being checked.
///
/// `Page.captureScreenshot` captures the browser's viewport, so the image is as tall as CEF has
/// actually made the strip. A `resize_bar` that asked for nothing would produce a 24-pixel-tall
/// picture, which is the failure this is looking for.
fn shoot(path: &str) {
    let Some(mut browser) = crate::ipc::bottom_chrome_browser() else {
        eprintln!("completion-script: no bottom strip to photograph yet");
        return;
    };
    crate::utilcmds::screenshot(&mut browser, path, None, true);
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
        // A command that has no model for the argument being typed.
        assert!(part(":scroll down").is_none());
        assert!(part("").is_none());
        // A prefix bru's command line cannot be in.
        assert!(part("xduck").is_none());
    }

    // ---- the search history ----

    #[test]
    fn a_search_completes_against_what_was_searched_for_before() {
        // This is the departure: on master both of these were `None`, with the comment "`/` and `?`
        // are a search" citing the qutebrowser line that refuses them.
        for prefix in ['/', '?'] {
            let typed = part(&format!("{prefix}duck")).unwrap();
            assert_eq!(typed.prefix, prefix);
            assert_eq!(typed.which, Which::SearchHistory);
            assert_eq!(typed.center, "duck");
            assert_eq!(typed.line_with("duckling"), format!("{prefix}duckling"));
            // And the bare prefix opens the whole history.
            assert_eq!(part(&prefix.to_string()).unwrap().center, "");
        }
        // A term is the rest of the line, spaces and leading dashes included, and completing it
        // never quotes: `search` is maxsplit-0 and `accept` hands it `search -- <rest>`.
        let spaced = part("/rust vec").unwrap();
        assert_eq!(spaced.center, "rust vec");
        assert_eq!(spaced.line_with("two words"), "/two words");
        assert_eq!(part("/-x").unwrap().center, "-x");
    }

    // ---- the command-name model ----

    #[test]
    fn a_bare_colon_opens_the_command_name_model() {
        // `completer.py:87-90` — nothing before the cursor is the command model, and its pattern is
        // empty, so the whole list opens.
        let bare = part(":").unwrap();
        assert_eq!(bare.which, Which::Commands);
        assert_eq!(bare.center, "");
        assert_eq!(bare.line_with("scroll"), ":scroll");
        // `:   ` is the same case — `_partition` returns `[], '', []` for a body that is all space.
        assert_eq!(part(":   ").unwrap().which, Which::Commands);
        assert_eq!(part(":   ").unwrap().center, "");
    }

    #[test]
    fn a_half_typed_command_name_completes_itself() {
        let typed = part(":m").unwrap();
        assert_eq!(typed.which, Which::Commands);
        assert_eq!(typed.center, "m");
        assert_eq!(typed.line_with("macro-record"), ":macro-record");
        // A name that is also a whole command still completes the *name* until a space follows it,
        // which is `'set|'` in `_get_new_completion`'s own comment.
        let whole = part(":open").unwrap();
        assert_eq!(whole.which, Which::Commands);
        assert_eq!(whole.center, "open");
    }

    #[test]
    fn the_command_name_model_offers_every_name_with_its_sentence_and_its_keys() {
        let cats = build_which(Which::Commands, "");
        assert_eq!(cats.len(), 1);
        assert_eq!(cats[0].name, "Commands");
        // One row per *name*: six of the 163 commands have a second spelling. **169, not 166** —
        // the plugin workstream added `plugin-list`, `plugin-reload` and `plugin-disable`, and this
        // number moves with `COMMANDS` by design. The assertion above it is the one that matters;
        // this one exists so that a table which silently stopped growing is noticed.
        let names: usize = crate::help::COMMANDS.iter().map(|doc| doc.names.len()).sum();
        assert_eq!(cats[0].items.len(), names);
        assert_eq!(names, 169);

        let row = |name: &str| {
            cats[0]
                .items
                .iter()
                .find(|item| item.cols[0] == name)
                .unwrap_or_else(|| panic!("no row for {name}"))
                .cols
                .clone()
        };
        // Three columns: the name, what it does, the keys that reach it.
        assert_eq!(
            row("scroll"),
            [
                "scroll".to_string(),
                "Scroll the page. A count repeats it. This is the wheel event bru was built for."
                    .to_string(),
                // Every default binding of `scroll`, in the order `reached` collects them.
                row("scroll")[2].clone(),
            ]
        );
        assert!(row("scroll")[2].contains('j'), "j is not on the scroll row: {:?}", row("scroll")[2]);
        // A command no key reaches has an empty key column rather than being left out.
        assert_eq!(row("screenshot")[2], "");
        // The keys that only *type* a command are marked, not silently mixed in with the ones that
        // run it: `ga` runs `open -t`, while `o` is `cmd-set-text -s :open` and only prefills it.
        let open = row("open")[2].clone();
        let (runs, types) = open.split_once(" types ").expect("both kinds are on the open row");
        assert!(runs.split(' ').any(|key| key == "ga"), "{runs:?}");
        assert!(types.split(' ').any(|key| key == "o"), "{types:?}");
        assert!(!runs.split(' ').any(|key| key == "o"), "o only types :open: {runs:?}");

        // Both spellings of an aliased command are their own row, and they agree about everything
        // except the name.
        assert_eq!(row("later")[1], row("cmd-later")[1]);
        assert_eq!(row("later")[2], row("cmd-later")[2]);

        // Sorted by name, so the prefix-float in `list_category` is the only reordering.
        let sorted: Vec<String> = {
            let mut names: Vec<String> = cats[0].items.iter().map(|i| i.cols[0].clone()).collect();
            names.sort();
            names
        };
        assert_eq!(
            sorted,
            cats[0].items.iter().map(|i| i.cols[0].clone()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn the_command_name_model_filters_and_floats_the_prefix() {
        let cats = build_which(Which::Commands, "m");
        let names: Vec<&str> = cats[0].items.iter().map(|i| i.cols[0].as_str()).collect();
        // Every name that starts with `m` comes first, in name order...
        assert_eq!(
            names.iter().take_while(|n| n.starts_with('m')).count(),
            names.iter().filter(|n| n.starts_with('m')).count(),
            "a prefix match is stranded behind a substring one: {names:?}"
        );
        assert_eq!(names[0], "macro-record");
        // ...and the substring matches are still there behind them.
        assert!(names.contains(&"bookmark-add"), "{names:?}");
        // A pattern nothing matches closes the bar rather than showing an empty header.
        assert!(build_which(Which::Commands, "zzzznothing").is_empty());
    }

    /// The one cap this model does not take, measured rather than assumed — see
    /// `completion::list_category_max`.
    #[test]
    fn the_whole_command_list_still_fits_in_one_push() {
        let cats = build_which(Which::Commands, "");
        let size = completion::to_json(&cats, Some((0, 0))).len();
        // 19,730 bytes on 2026-08-07, against the 32 KB the worst-case `:open` payload is held
        // under in `completion.rs`. It is one `ExecuteJavaScript` per keystroke of a `:` line, and
        // that line is typed by a person.
        assert!(size < 32 * 1024, "the whole command list is {size} bytes");
        assert!(size > 16 * 1024, "the list got shorter, not the payload: {size} bytes");
    }

    /// The height `resize_bar` asks for with the whole list open, which is the one number a bar
    /// with 166 rows in it could get absurdly wrong.
    #[test]
    fn the_whole_command_list_does_not_ask_for_an_absurd_bar() {
        let cats = build_which(Which::Commands, "");
        let rows: i32 = cats.iter().map(|cat| cat.items.len() as i32).sum();
        assert_eq!(rows, 169);
        // `resize_bar`'s arithmetic, which is `chrome.css:186-191`'s: 20px per row and per header,
        // capped at --completion-max-h and one pixel for the border. 166 rows want 3,340px and get
        // 301, because past the cap the table scrolls inside itself.
        let wanted = (20 * (rows + cats.len() as i32)).min(300) + 1;
        assert_eq!(wanted, 301);
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
        assert_eq!(spaced.which, Which::Url);
        assert_eq!(spaced.center, "");
        assert_eq!(spaced.line_with("https://a/"), ":open https://a/");
        // **The space is what moves the completion from the name to the argument**, and this used
        // to say the opposite: `:open` with no space answered the URL model with an empty pattern.
        // `_partition` puts the cursor inside `open` itself there and `_get_new_completion` opens
        // the command model, which is only observable once there is one to open.
        assert_eq!(part(":open").unwrap().which, Which::Commands);
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
        // Cursor inside `open`, not in its argument: the part under it is the command's own name,
        // so the command model answers and `duck` is kept where it is.
        let inside = partition(":open duck", 3).unwrap();
        assert_eq!(inside.which, Which::Commands);
        assert_eq!(inside.center, "open");
        assert_eq!(inside.after, ["duck"]);
        assert_eq!(inside.line_with("open"), ":open duck");
        // Cursor at the end of the first argument.
        assert_eq!(partition(":tab-focus 1", 12).unwrap().center, "1");
        // And in the gap after it, which is a second argument nothing claims.
        assert!(partition(":tab-focus 1 ", 13).is_none());
    }

    // ---- the settings ----

    #[test]
    fn set_completes_its_option_and_then_that_options_values() {
        // Two models on one command, chosen by which argument the cursor is in.
        assert_eq!(part(":set ").unwrap().which, Which::Setting(Only::Any));
        let value = part(":set scrollbar.width ").unwrap();
        assert_eq!(value.which, Which::SettingValue);
        assert_eq!(value.option(), Some("scrollbar.width"));
        // The option is still the option after a flag, and `-p` stays on the line.
        let flagged = part(":set -p statusbar.mode.style ").unwrap();
        assert_eq!(flagged.which, Which::SettingValue);
        assert_eq!(flagged.option(), Some("statusbar.mode.style"));
        assert_eq!(flagged.line_with("short"), ":set -p statusbar.mode.style short");
    }

    #[test]
    fn the_option_model_lists_every_setting_with_what_it_takes_and_what_it_is() {
        let cats = build_which(Which::Setting(Only::Any), "");
        assert_eq!(
            cats.iter().map(|cat| cat.name).collect::<Vec<_>>(),
            ["Settings", "Refused"]
        );
        assert_eq!(cats[0].items.len(), crate::settings::SETTINGS.len());
        assert_eq!(cats[1].items.len(), crate::settings::REFUSED.len());

        let row = |name: &str| {
            cats[0]
                .items
                .iter()
                .find(|item| item.cols[0] == name)
                .unwrap_or_else(|| panic!("no row for {name}"))
                .cols
                .clone()
        };
        // A choice prints its choices; a boolean says so; a number carries its range and unit.
        assert_eq!(row("statusbar.mode.style")[1], "full or short");
        assert_eq!(row("statusbar.mode.style")[2], "full");
        assert_eq!(row("url.open_base_url")[1], "true or false");
        assert_eq!(row("url.open_base_url")[2], "true");
        assert_eq!(row("messages.timeout")[1], "a whole number, 0 to 86400000 milliseconds");
        assert_eq!(row("messages.timeout")[2], "3000");
        // A dictionary's value column is a count, because a dict is not one line — the row that
        // shows the pairs is `bru://chrome/settings`'s.
        assert_eq!(row("url.searchengines")[1], "a dictionary, any key");
        assert_eq!(row("url.searchengines")[2], "9 entries");
        // A setting that can only be written per URL still shows what is in force where no rule has
        // been written, which is bru's own default.
        assert_eq!(row("content.javascript.enabled")[2], "true");

        // A refused name is offered with its reason and never as something to set.
        let refused = &cats[1].items.iter().find(|i| i.cols[0] == "content.plugins").unwrap().cols;
        assert_eq!(refused[1], "Chromium 151 has nothing behind this name.");
        assert_eq!(refused[2], "refused");
    }

    #[test]
    fn the_dict_and_list_commands_are_offered_only_the_options_they_work_on() {
        let dicts = build_which(Which::Setting(Only::Dicts), "");
        let names: Vec<&str> = dicts[0].items.iter().map(|i| i.cols[0].as_str()).collect();
        assert_eq!(names, ["statusbar.mode.labels", "url.searchengines"]);
        // No Refused category here: none of the refused names is a dictionary, so offering them
        // under `:config-dict-add` would be offering them as dictionaries.
        assert_eq!(dicts.len(), 1);

        let lists = build_which(Which::Setting(Only::Lists), "");
        assert!(
            lists[0].items.iter().all(|item| matches!(
                crate::settings::def(&item.cols[0]).map(|def| def.kind),
                Some(crate::settings::Kind::List(_))
            )),
            "a non-list is offered to :config-list-add"
        );
        assert!(lists[0].items.iter().any(|item| item.cols[0] == "zoom.levels"));
    }

    #[test]
    fn what_a_value_may_be_depends_on_the_option_named_before_it() {
        let names = |cats: &[Category]| -> Vec<(String, Vec<String>)> {
            cats.iter()
                .map(|cat| {
                    (
                        cat.name.to_string(),
                        cat.items.iter().map(|i| i.cols[0].clone()).collect(),
                    )
                })
                .collect()
        };

        // A boolean offers the two spellings, and the current and default value above them.
        assert_eq!(
            names(&setting_values(Some("url.open_base_url"), &[], "")),
            [
                ("Current/Default".to_string(), vec!["true".to_string()]),
                ("Completions".to_string(), vec!["true".to_string(), "false".to_string()]),
            ]
        );
        // A choice offers its list, in the order `config-cycle` walks it.
        assert_eq!(
            names(&setting_values(Some("content.notifications.enabled"), &[], ""))[1].1,
            ["true", "false", "ask"]
        );
        // A number offers no completions at all — only what it is and what to put it back to.
        assert_eq!(
            names(&setting_values(Some("messages.timeout"), &[], "")),
            [("Current/Default".to_string(), vec!["3000".to_string()])]
        );
        // Free text is the same.
        assert_eq!(names(&setting_values(Some("hints.chars"), &[], "")).len(), 1);
        // A dict and a list are not a single value, so there is nothing to offer.
        assert!(setting_values(Some("url.searchengines"), &[], "").is_empty());
        assert!(setting_values(Some("zoom.levels"), &[], "").is_empty());
        // An option bru has never heard of, which is what a typo is.
        assert!(setting_values(Some("no.such.setting"), &[], "").is_empty());
        assert!(setting_values(None, &[], "").is_empty());
    }

    #[test]
    fn a_value_already_on_the_line_is_not_offered_again() {
        // `configmodel.py:88,92` — a three-valued option with two of them already typed has one
        // left to offer, and neither of the two it has.
        let cats = setting_values(Some("content.notifications.enabled"), &["true", "false"], "");
        let offered: Vec<String> = cats
            .iter()
            .flat_map(|cat| cat.items.iter().map(|i| i.cols[0].clone()))
            .collect();
        // Twice, and that is `configmodel.value`'s own shape rather than an oversight: the two
        // categories answer different questions — "what is it now" and "what may it be" — and only
        // the second is filtered by what is already typed (`configmodel.py:92`). The first is
        // filtered too, and `ask` survives it because `ask` is not one of the two typed values.
        assert_eq!(offered, ["ask", "ask"]);
        // And that is what the command line produces, not only what the helper does.
        let typed = part(":config-cycle content.notifications.enabled true false ").unwrap();
        assert_eq!(typed.which, Which::SettingValue);
        assert_eq!(typed.option(), Some("content.notifications.enabled"));
        assert_eq!(typed.values_before(), ["true", "false"]);
        // `*values` means every position after the first answers the same model.
        assert_eq!(
            part(":config-cycle content.notifications.enabled true ").unwrap().which,
            Which::SettingValue
        );
    }

    #[test]
    fn a_dicts_keys_and_a_lists_entries_are_offered_to_the_commands_that_take_them() {
        let keys = build(&part(":config-dict-remove url.searchengines ").unwrap());
        let names: Vec<&str> = keys[0].items.iter().map(|i| i.cols[0].as_str()).collect();
        assert!(names.contains(&"aw"), "{names:?}");
        assert_eq!(names.len(), crate::open::DEFAULT_ENGINES.len());
        // The template is beside the key, which is what tells two engines apart.
        assert!(keys[0].items[0].cols[1].contains("{}"));

        let entries = build(&part(":config-list-remove zoom.levels ").unwrap());
        let names: Vec<&str> = entries[0].items.iter().map(|i| i.cols[0].as_str()).collect();
        assert!(names.contains(&"100%"), "{names:?}");
    }

    // ---- the last three finite sets ----

    #[test]
    fn bind_completes_the_command_it_is_binding_and_not_the_key() {
        // `maxsplit=1`: the key is a word, the command is everything after it.
        assert!(part(":bind j").is_none(), "a key that is not bound yet is not a set");
        let command = part(":bind j scroll do").unwrap();
        assert_eq!(command.which, Which::Commands);
        assert_eq!(command.center, "scroll do");
        assert_eq!(command.before, ["bind", "j"]);
        // Column 0 is the command's name, so the arguments the user was part-way through typing go
        // — which is what `_change_completed_part` does to a maxsplit part in qutebrowser too.
        assert_eq!(command.line_with("scroll"), ":bind j scroll");
        // A flag before the key stays with the command.
        let moded = part(":bind -d j scroll down").unwrap();
        assert_eq!(moded.which, Which::Commands);
        assert_eq!(moded.center, "scroll down");
        assert_eq!(moded.line_with("scroll"), ":bind -d j scroll");

        // **And the flaw, stated rather than hidden.** A flag's *value* is counted as a positional,
        // because the only thing either completion knows about a flag is its leading `-`
        // (`completer.py:97`) — so `--mode caret` shifts everything one place and the part under
        // the cursor comes out one argument early. It is qutebrowser's behaviour to the letter, and
        // the `=` spelling is the one that keeps the count: it is one token and it starts with `-`.
        assert_eq!(part(":bind --mode caret j move-to-next-line").unwrap().center, "j move-to-next-line");
        assert_eq!(part(":bind --mode=caret j move-to-next-line").unwrap().center, "move-to-next-line");
    }

    #[test]
    fn mode_enter_offers_the_modes_a_command_may_enter() {
        assert_eq!(part(":mode-enter ").unwrap().which, Which::ModeName);
        let cats = build_which(Which::ModeName, "");
        let names: Vec<&str> = cats[0].items.iter().map(|i| i.cols[0].as_str()).collect();
        assert_eq!(
            names,
            ["normal", "insert", "passthrough", "caret", "set_mark", "jump_mark", "record_macro", "run_macro"]
        );
        // The four a question or a keypress puts you in are not on offer, because `commands.rs`
        // refuses them at parse time — offering one would offer a line that does not parse.
        for refused in ["command", "hint", "prompt", "yesno"] {
            assert!(!names.contains(&refused), "{refused} can be typed but not entered");
            assert!(crate::commands::parse(&format!("mode-enter {refused}")).is_err());
        }
        // And every one that is offered does parse.
        for name in &names {
            assert!(
                crate::commands::parse(&format!("mode-enter {name}")).is_ok(),
                "mode-enter {name} is offered and does not parse"
            );
        }
    }

    #[test]
    fn the_three_session_commands_offer_the_files_that_are_there() {
        for text in [":session-load ", ":session-delete ", ":session-save "] {
            assert_eq!(part(text).unwrap().which, Which::Session, "{text}");
        }
        // `session::list` reads a directory, so what it answers under `cargo test` is whatever the
        // scratch data directory holds — which is nothing. An empty source is a closed bar, and
        // that is the whole claim testable without a disk here.
        let cats = build_which(Which::Session, "");
        assert!(cats.iter().all(|cat| !cat.items.is_empty()), "a bare header would be drawn");
    }

    /// `rest_from` is the command's `maxsplit` and not one argument's, so two rows for one command
    /// that disagreed about it would cut the same line two ways depending on where the cursor was.
    #[test]
    fn every_command_agrees_with_itself_about_maxsplit() {
        for spec in SPECS {
            let same: Vec<Option<usize>> = SPECS
                .iter()
                .filter(|other| other.name == spec.name)
                .map(|other| other.rest_from)
                .collect();
            assert!(
                same.iter().all(|rest_from| *rest_from == spec.rest_from),
                "{} disagrees with itself about maxsplit: {same:?}",
                spec.name
            );
        }
        // And no two rows claim the same argument of the same command.
        for spec in SPECS {
            let claims = SPECS
                .iter()
                .filter(|other| other.name == spec.name && other.argpos == spec.argpos)
                .count();
            assert_eq!(claims, 1, "{} claims argument {} twice", spec.name, spec.argpos);
        }
    }

    #[test]
    fn every_model_is_reachable_by_the_command_that_wants_it() {
        assert_eq!(part(":tab-focus 2").unwrap().which, Which::Tabs { special: true });
        assert_eq!(part(":quickmark-load g").unwrap().which, Which::Quickmark);
        assert_eq!(part(":bookmark-load h").unwrap().which, Which::Bookmark);
// --- src/utilcmds.rs -------------------------------------------------------
        // `:tab-select` moved off `Which::Tabs` when the command was implemented: it takes
        // `[win-id/]index`, so it needs every window's tabs and rows addressed with the window id.
        // `:tab-focus` is the one that stays — it is this window's tabs and a bare index.
        assert_eq!(part(":tab-select 2").unwrap().which, Which::AllTabs);
        assert_eq!(part(":tab-take 0/2").unwrap().which, Which::OtherTabs);
        // Both are `maxsplit0`, so a title fragment with a space in it is one pattern and the
        // completion sees all of it.
        assert_eq!(part(":tab-select rust std").unwrap().center, "rust std");
        assert_eq!(part(":tab-take rust std").unwrap().center, "rust std");
// --- end src/utilcmds.rs ---------------------------------------------------
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
            other => build_which(other, &part.center),
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
        let cats = build_which(Which::Tabs { special: true }, "last");
        assert_eq!(cats.last().map(|cat| cat.name), Some("Special"));
        let cats = build_which(Which::Tabs { special: false }, "last");
        assert!(cats.iter().all(|cat| cat.name != "Special"));
    }
}
