//! The command line: its text, its cursor, its history, and the readline editing qutebrowser binds
//! in command mode.
//!
//! The file is two halves and the split is deliberate.
//!
//! [`CmdLine`] is **pure state**. It knows nothing of CEF, of the chrome, or of which browser has
//! focus; every readline operation is a method on a `Vec<char>` and a cursor index, ported from
//! qutebrowser 3.7.0's `components/readlinecommands.py` and `mainwindow/statusbar/command.py`.
//! That is what the tests at the bottom of this file exercise, and it is why they can.
//!
//! Everything under "The live one" is the single instance plus the CEF glue: entering the mode,
//! moving focus, and pushing the state into `chrome/bottom.js`. It is thin on purpose.
//!
//! ## Who owns the text
//!
//! The `<input id="cmdline">` in the bottom chrome is a real text field, and plain typing goes to
//! it — see [`types_into_cmdline`], which is the one narrow exception to CEF-NOTES.md trap 11. The
//! chrome reports every change back as `text-changed`, so Rust holds a *mirror*, one IPC hop
//! behind. That lag is the reason `command-accept` does not read the mirror:
//!
//! 1. `<Return>` is swallowed in the browser process and runs `command-accept`.
//! 2. Rust asks the chrome for the text (`bru.accept()`).
//! 3. The chrome answers `{"type":"accept","text":…}` with what the DOM actually holds.
//!
//! The keystroke before Enter travels browser → renderer → DOM → JS → browser; Enter travels
//! straight to the browser process and can overtake it. Reading the mirror would drop the last
//! character typed, sometimes, which is the worst kind of bug to own. The round trip costs one IPC
//! hop on a command that is about to load a page.
//!
//! Pushes in the other direction carry a revision number. Rust bumps it only when *Rust* changed
//! the text — `cmd-set-text`, a readline command, a history recall — so a render triggered by
//! something else (a URL change, a keystring update) never rewrites what the user is mid-way
//! through typing.

use crate::commands::Command;
use crate::modes::Mode;
use cef::*;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The characters a command line may start with. `modeparsers.STARTCHARS`.
pub const STARTCHARS: &str = ":/?";

// -----------------------------------------------------------------------------------------------
// Pure state
// -----------------------------------------------------------------------------------------------

/// One command line's text, cursor, kill buffer and history.
///
/// The cursor is a *character* index, not a byte one: every readline operation below is expressed
/// in characters, and a byte index would need a boundary check at each of them. The chrome is given
/// UTF-16 offsets instead, because that is what `HTMLInputElement.selectionStart` counts in — see
/// [`CmdLine::cursor_utf16`].
#[derive(Debug)]
pub struct CmdLine {
    chars: Vec<char>,
    cursor: usize,
    /// Accepted command lines, oldest first. `cmdhistory.History.history`.
    history: Vec<String>,
    /// Set while `<Ctrl-P>`/`<Ctrl-N>` are walking the history. `cmdhistory.History._tmphist`.
    browse: Option<Browse>,
    /// What the last killing operation removed, for `rl-yank`. `_ReadlineBridge._deleted`.
    deleted: Vec<char>,
    /// Bumped whenever this side changed the text, so the chrome can tell a push it must apply
    /// from a push that is only carrying other state.
    rev: u64,
}

/// A walk through the history, filtered by whatever was typed when it started.
///
/// `usertypes.NeighborList` in `Modes.exception`: running off either end is an error the caller
/// turns into "stay where you are", which is why both ends return `None` rather than clamping.
#[derive(Debug)]
struct Browse {
    items: Vec<String>,
    idx: usize,
}

impl Default for CmdLine {
    fn default() -> Self {
        Self::new()
    }
}

impl CmdLine {
    pub const fn new() -> CmdLine {
        CmdLine {
            chars: Vec::new(),
            cursor: 0,
            history: Vec::new(),
            browse: None,
            deleted: Vec::new(),
            rev: 0,
        }
    }

    // --- what it holds --------------------------------------------------------------------------

    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    /// The cursor, counted in characters.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// The cursor, counted in UTF-16 code units — the only unit `selectionStart` understands.
    pub fn cursor_utf16(&self) -> usize {
        self.chars[..self.cursor].iter().map(|c| c.len_utf16()).sum()
    }

    pub fn rev(&self) -> u64 {
        self.rev
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    /// The `:`, `/` or `?` this line starts with, if any. `Command.prefix`.
    // `/` and `?` are the search workstream's: it asks this to tell a search from a command.
    #[allow(dead_code)]
    pub fn prefix(&self) -> Option<char> {
        let first = *self.chars.first()?;
        STARTCHARS.contains(first).then_some(first)
    }

    /// The accepted lines, oldest first. What [`save`] writes to `cmd-history`.
    pub fn history(&self) -> &[String] {
        &self.history
    }

    // --- what changes it ------------------------------------------------------------------------

    /// Replace the whole line and put the cursor at the end. `Command.setText`.
    pub fn set_text(&mut self, text: &str) {
        self.chars = text.chars().collect();
        self.cursor = self.chars.len();
        self.rev += 1;
    }

    /// Empty the line and stop any history walk. `Command.on_mode_left`.
    pub fn clear(&mut self) {
        self.chars.clear();
        self.cursor = 0;
        self.browse = None;
        self.rev += 1;
    }

    /// Adopt what the chrome says the input holds.
    ///
    /// This does **not** bump the revision: the chrome is already showing this, and telling it to
    /// apply its own text back would fight the caret.
    pub fn sync(&mut self, text: &str, cursor_utf16: Option<usize>) {
        self.chars = text.chars().collect();
        self.cursor = match cursor_utf16 {
            Some(units) => utf16_to_char_index(&self.chars, units),
            None => self.chars.len(),
        };
    }

    /// Insert text at the cursor and leave the cursor after it. `QLineEdit.insert`.
    pub fn insert(&mut self, text: &str) {
        for c in text.chars() {
            self.chars.insert(self.cursor, c);
            self.cursor += 1;
        }
        self.rev += 1;
    }

    // --- readline: moving -----------------------------------------------------------------------

    /// `rl-backward-char`
    pub fn backward_char(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
        self.rev += 1;
    }

    /// `rl-forward-char`
    pub fn forward_char(&mut self) {
        if self.cursor < self.chars.len() {
            self.cursor += 1;
        }
        self.rev += 1;
    }

    /// `rl-backward-word` — "back to the start of the current or previous word".
    pub fn backward_word(&mut self) {
        self.cursor = self.word_start_before(self.cursor);
        self.rev += 1;
    }

    /// `rl-forward-word` — "forward to the end of the next word".
    pub fn forward_word(&mut self) {
        self.cursor = self.word_end_after(self.cursor);
        self.rev += 1;
    }

    /// `rl-beginning-of-line`
    pub fn beginning_of_line(&mut self) {
        self.cursor = 0;
        self.rev += 1;
    }

    /// `rl-end-of-line`
    pub fn end_of_line(&mut self) {
        self.cursor = self.chars.len();
        self.rev += 1;
    }

    // --- readline: killing ----------------------------------------------------------------------

    /// `rl-unix-line-discard` — everything from the start of the line to the cursor.
    pub fn unix_line_discard(&mut self) {
        self.cut(0, self.cursor);
    }

    /// `rl-kill-line` — everything from the cursor to the end.
    pub fn kill_line(&mut self) {
        self.cut(self.cursor, self.chars.len());
    }

    /// `rl-backward-kill-word`
    pub fn backward_kill_word(&mut self) {
        self.cut(self.word_start_before(self.cursor), self.cursor);
    }

    /// `rl-kill-word`
    pub fn kill_word(&mut self) {
        self.cut(self.cursor, self.word_end_after(self.cursor));
    }

    /// `rl-rubout <delims>` — delete backwards to the nearest of `delims`.
    ///
    /// A direct port of `_ReadlineBridge.rubout`, down to the two loops and the `-1`. With
    /// `delims == " "` it is readline's `unix-word-rubout`, which is what `<Ctrl-W>` is bound to;
    /// with `" /"` it is `unix-filename-rubout`. The subtlety the loops exist for is trailing
    /// boundaries: `/some/path//` must lose `path//`, not just the two slashes.
    pub fn rubout(&mut self, delims: &str) {
        let cursor = self.cursor as isize;
        let mut target = cursor;

        // Scan back over any run of boundaries the cursor is sitting after.
        let mut is_boundary = true;
        while is_boundary && target > 0 {
            is_boundary = delims.contains(self.chars[target as usize - 1]);
            target -= 1;
        }

        // Then back over everything that is not one.
        is_boundary = false;
        while !is_boundary && target > 0 {
            is_boundary = delims.contains(self.chars[target as usize - 1]);
            target -= 1;
        }

        // The loops stop *on* the boundary, which is kept. Reaching the start of the line without
        // finding one means there is nothing to keep, so the first character goes too.
        if !is_boundary {
            debug_assert_eq!(target, 0);
            target -= 1;
        }

        let moveby = cursor - target - 1;
        if moveby > 0 {
            self.cut(self.cursor - moveby as usize, self.cursor);
        }
    }

    /// `rl-filename-rubout` — the OS path separator alone, spaces ignored.
    pub fn filename_rubout(&mut self) {
        self.rubout(std::path::MAIN_SEPARATOR_STR);
    }

    /// `rl-delete-char` — the character after the cursor.
    ///
    /// `QLineEdit.del_()` through `_dispatch` with no `delete=True`, so unlike every method above
    /// it does **not** fill the yank buffer. Losing that distinction would make `<Ctrl-?>` clobber
    /// what `<Ctrl-U>` had saved for `<Ctrl-Y>`.
    pub fn delete_char(&mut self) {
        if self.cursor < self.chars.len() {
            self.chars.remove(self.cursor);
            self.rev += 1;
        }
    }

    /// `rl-backward-delete-char` — the character before the cursor. Also not a kill.
    pub fn backward_delete_char(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.chars.remove(self.cursor);
            self.rev += 1;
        }
    }

    /// `rl-yank` — put back whatever the last kill removed.
    pub fn yank(&mut self) {
        if self.deleted.is_empty() {
            return;
        }
        let text: String = self.deleted.iter().collect();
        self.insert(&text);
    }

    // --- history --------------------------------------------------------------------------------

    /// Remember an accepted line. `cmdhistory.History.append`: a repeat of the newest entry is not
    /// a new entry.
    pub fn push_history(&mut self, text: &str) {
        if self.history.last().map(String::as_str) != Some(text) {
            self.history.push(text.to_string());
        }
    }

    /// Replace the history wholesale — how `cmd-history` is loaded. See [`ensure_history_loaded`].
    pub fn set_history(&mut self, history: Vec<String>) {
        self.history = history;
        self.browse = None;
    }

    /// `command-history-prev`. `None` means "nothing to show" and the line stays as it is.
    ///
    /// The first press starts a walk over the entries beginning with what is currently typed and
    /// lands on the newest of them; later presses step further back.
    pub fn history_prev(&mut self) -> Option<String> {
        if self.browse.is_none() {
            let prefix = self.text().trim().to_string();
            let items: Vec<String> = self
                .history
                .iter()
                .filter(|entry| prefix.is_empty() || entry.starts_with(&prefix))
                .cloned()
                .collect();
            if items.is_empty() {
                return None;
            }
            let idx = items.len() - 1;
            let item = items[idx].clone();
            self.browse = Some(Browse { items, idx });
            return Some(item);
        }

        let browse = self.browse.as_mut()?;
        if browse.idx == 0 {
            return None;
        }
        browse.idx -= 1;
        Some(browse.items[browse.idx].clone())
    }

    /// `command-history-next`. Does nothing unless a walk is already under way.
    pub fn history_next(&mut self) -> Option<String> {
        let browse = self.browse.as_mut()?;
        if browse.idx + 1 >= browse.items.len() {
            return None;
        }
        browse.idx += 1;
        Some(browse.items[browse.idx].clone())
    }

    /// Whether `<Ctrl-P>` has been pressed and not yet abandoned.
    // Both of these are the walk's own state, exercised by the tests; `clear` and `accept` are
    // what end a walk in the running browser.
    #[allow(dead_code)]
    pub fn is_browsing(&self) -> bool {
        self.browse.is_some()
    }

    #[allow(dead_code)]
    pub fn stop_browsing(&mut self) {
        self.browse = None;
    }

    // --- the two commands that are not readline -------------------------------------------------

    /// `cmd-set-text [-s] [-a]`: what the command line should become.
    ///
    /// `Err` carries qutebrowser's own message. The `-r`/`--run-on-count` flag is not decided here
    /// — it changes whether this is called at all, not what it produces.
    pub fn cmd_set_text(&self, text: &str, space: bool, append: bool) -> Result<String, String> {
        let mut text = text.to_string();
        if space {
            text.push(' ');
        }
        if append {
            if self.chars.is_empty() {
                return Err("No current text!".to_string());
            }
            text = format!("{}{}", self.text(), text);
        }
        match text.chars().next() {
            Some(first) if STARTCHARS.contains(first) => Ok(text),
            _ => Err(format!("Invalid command text '{text}'.")),
        }
    }

    /// `command-accept`: remember the line and hand back what should run.
    ///
    /// `:` followed by a space is deliberately not remembered — qutebrowser's way of running
    /// something without it turning up in the history afterwards. `None` means there is nothing to
    /// run, which is what an empty line or a bare prefix is.
    pub fn accept(&mut self, text: &str) -> Option<String> {
        let starts_with_space = text.starts_with(": ");
        if !starts_with_space {
            self.push_history(text);
        }
        self.browse = None;

        let mut chars = text.chars();
        let first = chars.next()?;
        if !STARTCHARS.contains(first) {
            return None;
        }
        let rest: String = chars.collect();
        if rest.trim().is_empty() {
            return None;
        }
        // `/` and `?` are searches, not commands. qutebrowser makes the same split in
        // `mainwindow/statusbar/command.py:83-87`, where those two emit `got_search` while `:`
        // emits `got_cmd`. Without it `/Kestrel` runs a *command* named `Kestrel`, which parses to
        // `Unimplemented` and is dropped in silence — measured 2026-08-06, and the reason `/` did
        // nothing at all while `:search Kestrel` worked.
        //
        // `--` matters: `search` is maxsplit-0, so a term beginning with `-` stays a term.
        Some(match first {
            '/' => format!("search -- {rest}"),
            '?' => format!("search -r -- {rest}"),
            _ => rest,
        })
    }

    // --- the small print ------------------------------------------------------------------------

    /// Delete `from..to`, keeping what was removed for `rl-yank`.
    fn cut(&mut self, from: usize, to: usize) {
        let from = from.min(self.chars.len());
        let to = to.min(self.chars.len());
        if from >= to {
            return;
        }
        self.deleted = self.chars[from..to].to_vec();
        self.chars.drain(from..to);
        self.cursor = from;
        self.rev += 1;
    }

    fn word_start_before(&self, mut at: usize) -> usize {
        while at > 0 && !is_word_char(self.chars[at - 1]) {
            at -= 1;
        }
        while at > 0 && is_word_char(self.chars[at - 1]) {
            at -= 1;
        }
        at
    }

    fn word_end_after(&self, mut at: usize) -> usize {
        let end = self.chars.len();
        while at < end && !is_word_char(self.chars[at]) {
            at += 1;
        }
        while at < end && is_word_char(self.chars[at]) {
            at += 1;
        }
        at
    }
}

/// What counts as inside a word.
///
/// qutebrowser documents `rl-backward-kill-word` as treating "any non-alphanumeric character" as a
/// delimiter, and Qt's own word finder (UAX #29) additionally keeps `_` inside a word. Both agree
/// that `/`, `.` and `-` are boundaries, which is what makes the word commands useful on a URL.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Turn a `selectionStart` into a character index. Out of range clamps to the end rather than
/// panicking: the value came from another process.
fn utf16_to_char_index(chars: &[char], units: usize) -> usize {
    let mut seen = 0;
    for (index, c) in chars.iter().enumerate() {
        if seen >= units {
            return index;
        }
        seen += c.len_utf16();
    }
    chars.len()
}

// -----------------------------------------------------------------------------------------------
// The live one
// -----------------------------------------------------------------------------------------------

/// The single command line. There is one window, so there is one of these.
fn cmdline() -> &'static Mutex<CmdLine> {
    static CMDLINE: Mutex<CmdLine> = Mutex::new(CmdLine::new());
    &CMDLINE
}

/// What an accepted command line is handed to.
///
/// The command *set* is another workstream's; this module only gets the text out of the user and
/// says when it is ready. Installed once at startup with [`set_runner`] rather than called by name
/// so that neither module has to know the other exists.
pub type Runner = fn(&str, Option<u32>);

static RUNNER: Mutex<Option<Runner>> = Mutex::new(None);

/// Install what runs an accepted command. Called once, at startup.
pub fn set_runner(runner: Runner) {
    if let Ok(mut slot) = RUNNER.lock() {
        *slot = Some(runner);
    }
}

fn run_text(text: &str, count: Option<u32>) {
    let runner = RUNNER.lock().ok().and_then(|slot| *slot);
    match runner {
        Some(runner) => runner(text, count),
        // Until the command set is wired up this is the only sign the round trip completed, and it
        // is what the `--cmdline-script` checks read.
        None => eprintln!("bru: no command runner installed, would have run {text:?} ({count:?})"),
    }
}

/// Whether a rapid accept is in flight, i.e. whether the mode should survive the answer.
static PENDING_RAPID: Mutex<bool> = Mutex::new(false);

// --- the dispatcher's one arm -------------------------------------------------------------------

/// Run a command the command line owns. `false` means it is not one of ours.
///
/// This is the whole seam with `src/exec.rs`: one call at the top of its `run`, before its own
/// match, and every readline key, the history keys, `cmd-set-text` and `command-accept` are
/// handled. The readline commands arrive as [`Command::Unimplemented`] because `commands.rs` has
/// no variant for them — deliberately, so that this module can be merged without touching a file
/// three other workstreams are also adding variants to.
/// Whether this module would claim `command` — the same question [`run_command`] answers, without
/// doing it. `exec::is_live` asks so that the live-binding count includes the readline and history
/// bindings, which reach here as [`Command::Unimplemented`] and are matched by name.
pub fn claims(command: &Command) -> bool {
    match command {
        Command::CmdSetText { .. } | Command::CommandAccept { .. } => true,
        Command::Unimplemented(text) => is_named(text),
        _ => false,
    }
}

pub fn run_command(command: &Command, count: Option<u32>) -> bool {
    match command {
        Command::CmdSetText { text, space, append, run_on_count } => {
            cmd_set_text(text, *space, *append, *run_on_count, count);
            true
        }
        Command::CommandAccept { rapid } => {
            command_accept(*rapid);
            true
        }
        Command::Unimplemented(text) => run_named(text),
        _ => false,
    }
}

/// The readline and history commands, matched by name.
///
/// Registered in qutebrowser for command *and* prompt mode; bru has no prompt mode, so outside
/// command mode they are accepted and do nothing, which is what `cmdutils` does with a command
/// whose `modes=` does not include the current one.
fn run_named(text: &str) -> bool {
    let mut parts = text.splitn(2, char::is_whitespace);
    let name = parts.next().unwrap_or_default();
    let arg = parts.next().map(unquote).unwrap_or_default();

    if !is_named(text) {
        return false;
    }
    run_named_inner(name, arg)
}

/// The command names this module answers to, as written in `configdata.yml`'s `command:` section.
fn is_named(text: &str) -> bool {
    let name = text.split(char::is_whitespace).next().unwrap_or_default();
    matches!(
        name,
        "rl-backward-char"
            | "rl-forward-char"
            | "rl-backward-word"
            | "rl-forward-word"
            | "rl-beginning-of-line"
            | "rl-end-of-line"
            | "rl-unix-line-discard"
            | "rl-kill-line"
            | "rl-rubout"
            | "rl-filename-rubout"
            | "rl-unix-word-rubout"
            | "rl-unix-filename-rubout"
            | "rl-backward-kill-word"
            | "rl-kill-word"
            | "rl-yank"
            | "rl-delete-char"
            | "rl-backward-delete-char"
            | "command-history-prev"
            | "command-history-next"
    )
}

fn run_named_inner(name: &str, arg: String) -> bool {
    if mode() != Mode::Command {
        return true;
    }

    // History recall goes through `cmd_set_text`, exactly as `command_history_prev` does, so a
    // recalled line arrives with the cursor at its end and the chrome is told to apply it.
    match name {
        "command-history-prev" | "command-history-next" => ensure_history_loaded(),
        _ => {}
    }
    match name {
        "command-history-prev" => {
            if let Some(item) = with(|cmd| cmd.history_prev()) {
                with(|cmd| cmd.set_text(&item));
            }
            push();
            return true;
        }
        "command-history-next" => {
            if let Some(item) = with(|cmd| cmd.history_next()) {
                with(|cmd| cmd.set_text(&item));
            }
            push();
            return true;
        }
        _ => {}
    }

    with(|cmd| match name {
        "rl-backward-char" => cmd.backward_char(),
        "rl-forward-char" => cmd.forward_char(),
        "rl-backward-word" => cmd.backward_word(),
        "rl-forward-word" => cmd.forward_word(),
        "rl-beginning-of-line" => cmd.beginning_of_line(),
        "rl-end-of-line" => cmd.end_of_line(),
        "rl-unix-line-discard" => cmd.unix_line_discard(),
        "rl-kill-line" => cmd.kill_line(),
        "rl-rubout" => cmd.rubout(&arg),
        "rl-filename-rubout" => cmd.filename_rubout(),
        "rl-unix-word-rubout" => cmd.rubout(" "),
        "rl-unix-filename-rubout" => cmd.rubout(" /"),
        "rl-backward-kill-word" => cmd.backward_kill_word(),
        "rl-kill-word" => cmd.kill_word(),
        "rl-yank" => cmd.yank(),
        "rl-delete-char" => cmd.delete_char(),
        "rl-backward-delete-char" => cmd.backward_delete_char(),
        _ => unreachable!("run_named matched a name it does not handle"),
    });
    push();
    true
}

/// `rl-rubout " "` reaches here with the quotes still on: `commands.rs` keeps an unimplemented
/// command verbatim, quotes included, because it never had to look inside one.
fn unquote(arg: &str) -> String {
    let arg = arg.trim();
    for quote in ['"', '\''] {
        if arg.len() >= 2 && arg.starts_with(quote) && arg.ends_with(quote) {
            return arg[1..arg.len() - 1].to_string();
        }
    }
    arg.to_string()
}

// --- the two commands with side effects -----------------------------------------------------

/// `cmd-set-text [-s] [-a] [-r] <text>` — how every one of `o`, `O`, `go`, `gO`, `b`, `T` and `:`
/// itself works.
///
/// `-r` with a count does not touch the command line at all: `T` is `cmd-set-text -sr :tab-focus`,
/// so `3T` runs `tab-focus` with count 3 while a bare `T` opens the line for typing.
pub fn cmd_set_text(text: &str, space: bool, append: bool, run_on_count: bool, count: Option<u32>) {
    let text = replace_variables(text);

    let text = match with(|cmd| cmd.cmd_set_text(&text, space, append)) {
        Ok(text) => text,
        Err(message) => {
            eprintln!("bru: cmd-set-text: {message}");
            return;
        }
    };

    if run_on_count {
        if let Some(count) = count {
            run_text(text.trim_start_matches(|c| STARTCHARS.contains(c)), Some(count));
            return;
        }
    }

    with(|cmd| cmd.set_text(&text));
    enter_command_mode();
    push();
}

/// `command-accept [--rapid]`.
///
/// Nothing is read out of the mirror here: the chrome is asked what the input really holds and the
/// work continues in [`on_accept`]. See the module comment for why.
pub fn command_accept(rapid: bool) {
    if mode() != Mode::Command {
        return;
    }
    if let Ok(mut pending) = PENDING_RAPID.lock() {
        *pending = rapid;
    }
    crate::ipc::ask_cmdline("accept");
}

// --- what the chrome sends back -------------------------------------------------------------

/// `{"type":"text-changed","text":…,"cursor":…}` — the user typed into `#cmdline`.
pub fn on_text_changed(text: &str, cursor: Option<usize>) {
    with(|cmd| cmd.sync(text, cursor));
}

/// `{"type":"accept","text":…}` — the authoritative text, in answer to `bru.accept()`.
pub fn on_accept(text: &str) {
    if mode() != Mode::Command {
        // A stray accept, e.g. the second of a double Enter. The mode is the interlock.
        return;
    }
    let rapid = PENDING_RAPID.lock().map(|pending| *pending).unwrap_or(false);

    // Before the line is appended, or `:save` would write this session's history over last
    // session's rather than after it.
    ensure_history_loaded();
    let to_run = with(|cmd| cmd.accept(text));

    if rapid {
        // `command-accept --rapid` keeps the line open and its text, so several commands can be
        // run without retyping the prefix.
        push();
    } else {
        leave_command_mode();
    }

    if let Some(to_run) = to_run {
        run_text(&to_run, None);
    }
}

/// `{"type":"cancel"}` — Escape reached the input. Rust normally handles Escape itself through the
/// `mode-leave` binding, so this is the belt to that braces; leaving a mode we have already left
/// is a no-op in `ModeManager`.
pub fn on_cancel() {
    leave_command_mode();
}

/// Called from `ipc::set_mode` whenever the mode changes, so that leaving command mode by *any*
/// route — `mode-leave`, an accept, a page focusing a field — clears the line and gives the page
/// its focus back. Wiring it to the mode change rather than to the `mode-leave` command is what
/// keeps this out of the dispatcher.
pub fn on_mode_changed(mode: &str) {
    if mode == "command" {
        return;
    }
    let had_text = with(|cmd| {
        let had_text = !cmd.is_empty();
        cmd.clear();
        had_text
    });
    if had_text {
        focus_page();
    }
}

// -----------------------------------------------------------------------------------------------
// `.` — cmd-repeat-last
// -----------------------------------------------------------------------------------------------

/// The last command run in each mode, and the count it was given.
///
/// A direct port of `commands/runners.py:25` — `last_command: dict[KeyMode, tuple[str, int]]` — and
/// it is keyed by mode for the reason qutebrowser keys it that way: `.` is a *normal-mode* binding,
/// and a command typed at the command line while in command mode must not be what a later `.`
/// repeats in normal mode. bru stores the parsed [`Command`] rather than the text because bru parses
/// its bindings once at startup and a keypress never has a string to record; the two are the same
/// thing one step apart, and re-parsing would only add a way for them to disagree.
///
/// `BTreeMap::new` is const and `HashMap::new` is not, which is the whole reason this is ordered.
static LAST_COMMAND: Mutex<BTreeMap<Mode, (Command, Option<u32>)>> = Mutex::new(BTreeMap::new());

/// Whether running `command` should make it the thing `.` repeats.
///
/// `runners.py:177-179`: the **only** exclusion is the repeat command itself, under either of its
/// two names. Nothing else is filtered — a `:quit` that has just run is recorded like anything
/// else, and there is no list of "dangerous" commands here, because qutebrowser has none and the
/// point of `.` is that it repeats what was actually done.
///
/// The list four lines further down (`runners.py:181-182`) is a *different* one: `macro-record`,
/// `macro-run`, `set-cmd-text` and `cmd-set-text` are excluded from **macro recording**, not from
/// `.`. Two commands would recurse into themselves and two would record the opening of a command
/// line rather than what was typed into it. bru has no macros yet, so that list has nothing to
/// filter here — but it is not this list, and conflating them would quietly stop `.` repeating `o`.
fn records_as_last(command: &Command) -> bool {
    match command {
        Command::CmdRepeatLast => false,
        // `a ;; b` is recorded whole in qutebrowser, and skipped whole if any link is the repeat
        // command — the flag is cleared inside the loop over the parsed chain.
        Command::Chain(parts) => parts.iter().all(records_as_last),
        _ => true,
    }
}

/// Remember a command as the one `.` will repeat, under the mode it was run in.
///
/// Called from the two places a command enters the dispatcher from a person — `src/keys.rs`, for a
/// binding, and `exec.rs`'s command-line task, for a typed line. Deliberately **not** from
/// `exec::run` itself: that function recurses for a chain, so recording there would remember the
/// last *link* rather than the chain, which is not what `a ;; b` means.
pub fn record_last_command(command: &Command, count: Option<u32>) {
    if !records_as_last(command) {
        return;
    }
    // The mode as it was *before* the command ran, which is what `cur_mode` is in `runners.py:159`.
    let mode = mode();
    if let Ok(mut last) = LAST_COMMAND.lock() {
        last.insert(mode, (command.clone(), count));
    }
}

/// What `.` repeats in the mode bru is in now, if anything.
pub fn last_command() -> Option<(Command, Option<u32>)> {
    let last = LAST_COMMAND.lock().ok()?;
    last.get(&mode()).cloned()
}

// -----------------------------------------------------------------------------------------------
// `sf` — save
// -----------------------------------------------------------------------------------------------

/// The saveables bru has, in the sense `misc/savemanager.py` means: things held in memory that a
/// file on disk has to be brought up to date with.
///
/// qutebrowser registers six (`add_saveable` calls): `command-history`, `quickmark-manager`,
/// `bookmark-manager`, `state-config`, `yaml-config` and, on the QtWebKit backend only, `cookies`.
/// bru has exactly one, and the other five are absent for reasons rather than by omission:
///
/// - **`yaml-config` / `state-config`** are qutebrowser writing configuration. DESIGN.md gives
///   `~/.config/bru/config.lua` to configer and `theme.css` to themer; bru writes neither, ever, so
///   there is nothing here to flush and `:save config` is refused rather than made a no-op.
/// - **quickmarks and bookmarks** are already on disk. `data.rs` rewrites the file inside
///   `quickmark_save`, `quickmark_del`, `bookmark_add` and `bookmark_del`, atomically, before the
///   command returns — so a saveable would be a saveable that is never dirty.
/// - **history** is a SQLite database written per visit; `cookies` are Chromium's, under
///   `--user-data-dir`, and no bru code owns them.
///
/// That leaves the command line, which until now was the one thing bru held and never wrote: an
/// accepted `:open …` lived in a `Vec<String>` and went with the process. The file is
/// `cmd-history` in bru's data directory, which is the name and the shape qutebrowser's
/// `LimitLineParser(standarddir.data(), 'cmd-history')` uses (`cmdhistory.py:126-131`).
///
/// Under `--private` this one is refused. `cmd-history` is a list of the commands the user accepted,
/// which for the command that matters is `open <url>` — the same fact the visit log holds, in the
/// user's own words. It is history, not a mark saved by name, so it follows `history.sqlite` and not
/// `quickmarks`; see `profile::is_private` for where that line is drawn and why.
const SAVEABLES: &[&str] = &["command-history"];

/// What `data.rs` writes without being asked, and therefore what `:save` has nothing to do for.
const ALREADY_ON_DISK: &[&str] = &["quickmarks", "bookmarks", "history", "cookies"];

/// The description [`save`] gives these instead, in a `--private` run, where "written when it
/// changes" would be false for one of them and beside the point for the other.
const PRIVATE_NOTES: &[(&str, &str)] = &[
    ("history", "not written by a --private run, so there is nothing to flush"),
    ("cookies", "Chromium's, in a profile deleted when bru exits, so there is nothing to flush"),
];

/// What bru refuses to write at all, and the reason, in the shape `settings::REFUSED` uses.
const NOT_BRUS_TO_WRITE: &[(&str, &str)] = &[
    (
        "config",
        "bru writes no configuration: ~/.config/bru/config.lua belongs to configer and theme.css \
         to themer, and a browser that rewrote either would be a second author of a hand-written \
         file",
    ),
    ("key-config", "keys are bound in config.lua, which is configer's file"),
    ("yaml-config", "bru has no autoconfig.yml — :set changes the running browser and nothing else"),
    ("state-config", "bru keeps no state file; the session is :session-save's, under sessions/"),
];

/// `completion.cmd_history_max_items` (configdata.yml:1347), whose default is 100. The file is
/// bounded for the same reason qutebrowser bounds it: it is read in full at startup.
const HISTORY_MAX: usize = 100;

/// `save [what…]` — `sf`, and `:save` typed.
///
/// With no argument everything is saved and one line reports it, which is `save_command`'s
/// `what = tuple(self.saveables)` (savemanager.py:178-180). With names, each is looked up and an
/// unknown one is an error naming what can be saved — qutebrowser's own wording.
pub fn save(what: &[String]) {
    ensure_history_loaded();

    let names: Vec<&str> = if what.is_empty() {
        SAVEABLES.to_vec()
    } else {
        what.iter().map(String::as_str).collect()
    };

    let (mut done, mut errors) = (Vec::new(), Vec::new());
    for name in names {
        match name {
            "command-history" => match write_command_history() {
                Ok((path, lines)) => done.push(format!("{lines} lines to {}", path.display())),
                Err(error) => errors.push(format!("command-history: {error}")),
            },
            name if crate::profile::is_private()
                && PRIVATE_NOTES.iter().any(|(what, _)| *what == name) =>
            {
                let why = PRIVATE_NOTES.iter().find(|(what, _)| *what == name).map(|(_, why)| *why);
                done.push(format!("{name} ({})", why.unwrap_or_default()))
            }
            name if ALREADY_ON_DISK.contains(&name) => {
                done.push(format!("{name} (written when it changes; nothing to flush)"))
            }
            name => match NOT_BRUS_TO_WRITE.iter().find(|(refused, _)| *refused == name) {
                Some((_, why)) => errors.push(format!("{name}: {why}")),
                None => errors.push(format!(
                    "{name} is nothing which can be saved; bru saves {}",
                    SAVEABLES.join(", ")
                )),
            },
        }
    }

    if !done.is_empty() {
        crate::message::info(&format!("saved {}", done.join(", ")));
    }
    for error in errors {
        crate::message::error(&error);
    }
}

/// `$XDG_DATA_HOME/bru/cmd-history`, or `~/.local/share/bru/cmd-history`.
///
/// Through `data::data_dir` rather than a second copy of the XDG rules, so that a scratch
/// `XDG_DATA_HOME` moves the command history with everything else bru owns.
fn history_path() -> Option<PathBuf> {
    Some(crate::data::data_dir()?.join("cmd-history"))
}

/// Write the accepted lines out, newest last, and answer where they went and how many there were.
///
/// The `--private` refusal is here rather than in [`save`]'s match so that it is on the only path to
/// the file: a later caller — a save on quit, a timer — gets it without having to remember it. The
/// lines are still kept in memory and `<Ctrl-P>` still walks them; what a private run refuses is the
/// disk.
fn write_command_history() -> std::io::Result<(PathBuf, usize)> {
    if crate::profile::is_private() {
        return Err(std::io::Error::other(
            "not saved: this is a --private run, and an accepted `:open …` names the page as \
             plainly as the history would",
        ));
    }
    let Some(path) = history_path() else {
        return Err(std::io::Error::other(
            "neither XDG_DATA_HOME nor HOME is set, so there is nowhere to save",
        ));
    };
    let entries = with(|cmd| cmd.history().to_vec());
    let lines = write_history(&path, &entries)?;
    Ok((path, lines))
}

/// The pure half: the bytes, the cap, and the atomic rename. Takes a path so that it is testable
/// without an `XDG_DATA_HOME` the whole process shares.
fn write_history(path: &Path, entries: &[String]) -> std::io::Result<usize> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let kept: Vec<&String> = entries
        .iter()
        // A line with a newline in it would come back as two commands; the `<input>` cannot produce
        // one, but the file is read again on the next start and this is the cheap end to guard.
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

    // Temp file and rename, so an interrupted save leaves the previous history rather than half of
    // this one — the same shape `data::write_atomically` uses for the two mark files.
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)?;
    Ok(kept.len())
}

fn read_history(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// Read `cmd-history` into the live command line, once, the first time something needs it.
///
/// Lazily rather than from `app.rs` so that no other workstream's file has to be edited to install
/// it, and so that a data directory that cannot be read costs `<Ctrl-P>` its older entries and
/// nothing else. It is called from the live half only — `on_accept`, the two history commands, and
/// [`save`] — so the unit tests below never touch a real directory.
fn ensure_history_loaded() {
    static LOADED: std::sync::Once = std::sync::Once::new();
    LOADED.call_once(|| {
        let Some(path) = history_path() else { return };
        let entries = read_history(&path);
        if entries.is_empty() {
            return;
        }
        cmdline()
            .lock()
            .expect("command line mutex poisoned")
            .set_history(entries);
    });
}

// --- mode and focus ---------------------------------------------------------------------------

fn mode() -> Mode {
    crate::state::BruState::instance()
        .and_then(|state| state.lock().ok().map(|state| state.mode()))
        .unwrap_or(Mode::Normal)
}

fn enter_command_mode() {
    let Some(state) = crate::state::BruState::instance() else {
        return;
    };
    let entered = state
        .lock()
        .expect("state mutex poisoned")
        .enter_mode(Mode::Command, false);
    if entered {
        // This is what puts `body.mode-command` on the chrome; bottom.js reads the mode off the
        // pushed state and nothing else off it.
        crate::ipc::set_mode("command".to_string());
    }
    crate::ipc::focus_bottom_chrome();
}

fn leave_command_mode() {
    let Some(state) = crate::state::BruState::instance() else {
        return;
    };
    let left = state.lock().expect("state mutex poisoned").leave_mode();
    if left {
        // `set_mode` calls back into `on_mode_changed`, which clears the line and refocuses.
        crate::ipc::set_mode(Mode::Normal.name().to_string());
    }
}

/// Hand keyboard focus back to the tab that is showing. Without this the bottom strip keeps it and
/// the next `j` is typed into an invisible input instead of scrolling.
fn focus_page() {
    let Some(state) = crate::state::BruState::instance() else {
        return;
    };
    let browser = state.lock().expect("state mutex poisoned").active_browser();
    if let Some(browser) = browser {
        if let Some(host) = browser.host() {
            host.set_focus(1);
        }
    }
}

// --- trap 11's one exception ------------------------------------------------------------------

/// Whether a key that landed on the bottom chrome browser, in command mode, may be **forwarded**
/// to the `#cmdline` input instead of being swallowed.
///
/// CEF-NOTES.md trap 11 is why the default is "never": Chromium's own shortcuts are live inside any
/// browser, and an unswallowed `<Ctrl-T>` on a chrome strip navigates *that strip* to
/// `chrome://newtab/`, which fails and leaves the bar blank. Command mode is the one place the
/// strip genuinely needs the letters, so the exception is drawn as narrowly as it can be:
///
/// - a character key with **no Ctrl, Alt or Meta** — the typing, and nothing else;
/// - a handful of named editing keys a `QLineEdit` would handle itself and qutebrowser therefore
///   does not bind: Space, Backspace, Delete, Left, Right, Home, End.
///
/// Everything else stays in Rust. That covers every Chromium chord (all of them carry a modifier),
/// and it covers the keys command mode *does* bind — `<Return>`, `<Escape>`, `<Tab>`, `<Up>`,
/// `<Down>`, `<PgUp>`, `<PgDown>` — so the binding table stays the thing that decides what they do.
pub fn types_into_cmdline(info: &crate::bindings::KeyInfo) -> bool {
    use crate::bindings::{Key, Modifiers, NamedKey};

    let latched = info.mods.contains(Modifiers::CTRL)
        || info.mods.contains(Modifiers::ALT)
        || info.mods.contains(Modifiers::META);
    if latched {
        return false;
    }

    match info.key {
        Key::Char(_) => true,
        Key::Named(
            NamedKey::Space
            | NamedKey::Backspace
            | NamedKey::Delete
            | NamedKey::Left
            | NamedKey::Right
            | NamedKey::Home
            | NamedKey::End,
        ) => true,
        Key::Named(_) => false,
    }
}

// --- pushing to the chrome ----------------------------------------------------------------------

fn with<T>(f: impl FnOnce(&mut CmdLine) -> T) -> T {
    let mut guard = cmdline().lock().expect("command line mutex poisoned");
    f(&mut guard)
}

// --- src/completers.rs ---------------------------------------------------------------------

/// What the command line holds, for the completion: the text, the cursor **in characters**, and
/// whether `<Ctrl-P>` is currently walking the history (`cmdhistory.History.is_browsing`).
///
/// The completion is derived from all three — which model, which pattern, and whether `<Up>` means
/// "previous command" or "previous item" — and it is rebuilt on pushes that never went through
/// `on_text_changed`, so it cannot simply be handed them. Read-only; nothing here changes the line.
pub fn state_for_completion() -> (String, usize, bool) {
    with(|cmd| (cmd.text(), cmd.cursor(), cmd.browse.is_some()))
}

// --- end src/completers.rs -----------------------------------------------------------------

/// The `cmdline` field of the bottom view's state. Called by `ipc::bar_json`.
///
/// `rev` is what stops a push triggered by something else from rewriting a half-typed line: the
/// chrome applies `text` only when the revision is one it has not seen.
pub fn json() -> String {
    let focus = mode() == Mode::Command;
    with(|cmd| {
        format!(
            "{{\"text\":\"{}\",\"cursor\":{},\"rev\":{},\"focus\":{}}}",
            crate::ipc::json_escape(&cmd.text()),
            cmd.cursor_utf16(),
            cmd.rev(),
            focus,
        )
    })
}

fn push() {
    crate::ipc::push_bar();
}

/// `{url}` and friends, as `commands/runners.py::replace_variables` does them. Only the URL
/// variables are here: `{clipboard}` needs a decision that DECISIONS.md still lists as open, and
/// `{title}` has no caller among the default bindings.
///
/// This is how `go` and `gO` prefill — `cmd-set-text :open {url:pretty}` — and not, as it is easy
/// to misread, the `-r` flag, which is `open`'s own `--related` sitting inside the text because
/// `cmd-set-text` is registered with `maxsplit=0`.
fn replace_variables(text: &str) -> String {
    if !text.contains('{') {
        return text.to_string();
    }
    let url = crate::ipc::current_url();
    text.replace("{url:pretty}", &url).replace("{url}", &url)
}

// -----------------------------------------------------------------------------------------------
// Driving it without a keyboard
// -----------------------------------------------------------------------------------------------

/// `--cmdline-script='set::open ;type:duck;cmd:rl-unix-line-discard;accept' --cmdline-step-ms=500`
///
/// The steps run from posted UI tasks, one every `--cmdline-step-ms`, against the same functions a
/// keypress reaches. It exists because `wtype` is the only key injector on this machine and CEF
/// segfaults when its virtual keyboard attaches (CEF-NOTES.md, "Injecting keys on this machine"),
/// so the alternative to this is a check nobody runs twice.
///
/// Steps, separated by `;`:
///
/// | | |
/// |---|---|
/// | `set:<text>` | `cmd-set-text <text>` — enters command mode and prefills |
/// | `type:<text>` | what the chrome would report after typing `<text>` at the cursor |
/// | `cmd:<command>` | parse and run one command through [`run_command`] |
/// | `key:<key>` | inject a real key event at the **bottom chrome** browser — trap 11's check |
/// | `accept` | `command-accept` |
/// | `dump` | print text, cursor, mode and the bottom strip's own URL |
pub fn schedule_script(steps: &str, interval_ms: i64) {
    for (i, step) in steps.split(';').filter(|s| !s.is_empty()).enumerate() {
        let mut task = ScriptStep::new(step.to_string());
        post_delayed_task(
            ThreadId::UI,
            Some(&mut task),
            interval_ms * (i as i64 + 1),
        );
    }
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
                "set" => cmd_set_text(arg, false, false, false, None),
                "type" => {
                    let (text, at) = with(|cmd| {
                        let mut chars: Vec<char> = cmd.text().chars().collect();
                        let at = cmd.cursor();
                        for (offset, c) in arg.chars().enumerate() {
                            chars.insert(at + offset, c);
                        }
                        let text: String = chars.iter().collect();
                        let at: usize = chars[..at + arg.chars().count()]
                            .iter()
                            .map(|c| c.len_utf16())
                            .sum();
                        (text, at)
                    });
                    on_text_changed(&text, Some(at));
                    push();
                }
                "cmd" => match crate::commands::parse(arg) {
                    Ok(command) => {
                        if !run_command(&command, None) {
                            eprintln!("cmdline-script: {arg:?} is not a command line command");
                        }
                    }
                    Err(e) => eprintln!("cmdline-script: {arg:?} does not parse: {e}"),
                },
                "key" => inject_key(arg),
                "accept" => command_accept(false),
                "dump" => {
                    let (text, cursor) = with(|cmd| (cmd.text(), cmd.cursor()));
                    // The bottom strip's own URL is what trap 11 is measured by: a `<Ctrl-T>` that
                    // was forwarded navigates *it* to chrome://newtab/, and the tab count says
                    // whether the key reached the page instead.
                    let tabs = crate::state::BruState::instance()
                        .and_then(|state| state.lock().ok().map(|state| state.tab_count()))
                        .unwrap_or(0);
                    eprintln!(
                        "cmdline-script: mode={} text={text:?} cursor={cursor} tabs={tabs} bottom-url={:?}",
                        mode().name(),
                        crate::ipc::bottom_chrome_url(),
                    );
                }
                other => eprintln!("cmdline-script: no step named {other:?}"),
            }
        }
    }
}

/// Send one key straight at the bottom chrome browser, through `send_key_event` — the real input
/// path, and the only way to ask "does `<Ctrl-T>` typed in command mode still get swallowed?"
/// without a virtual keyboard.
///
/// The spelling is qutebrowser's, so `<Ctrl-T>` and `j` both work.
fn inject_key(spec: &str) {
    // `key:page:j` aims at the tab instead, so the keys stage 1 measured by hand can be measured
    // again without a keyboard.
    let (at_page, spec) = match spec.strip_prefix("page:") {
        Some(spec) => (true, spec),
        None => (false, spec),
    };
    // `key:char:x` sends the CHAR alone, at the bottom strip. That is not a press anyone can make —
    // it is the *second half* of one. A real keystroke's four events are dispatched one at a time to
    // whichever view holds focus at that instant, so a key whose command moves focus has its
    // RAWKEYDOWN delivered to the page and its CHAR delivered to the strip that just took focus.
    // `send_key_event` aims at one browser and cannot reproduce that split, which is why the bug
    // this exists to measure was invisible to every harness bru had.
    let (char_only, spec) = match spec.strip_prefix("char:") {
        Some(spec) => (true, spec),
        None => (false, spec),
    };

    let Some(info) = crate::bindings::parse_key_sequence(spec)
        .ok()
        .and_then(|sequence| sequence.first().copied())
    else {
        eprintln!("cmdline-script: {spec:?} is not a key");
        return;
    };
    let browser = if at_page {
        crate::state::BruState::instance()
            .and_then(|state| state.lock().ok().and_then(|mut state| state.active_browser()))
    } else {
        crate::ipc::bottom_chrome_browser()
    };
    let Some(browser) = browser else {
        eprintln!("cmdline-script: no browser to inject into yet");
        return;
    };
    let Some(host) = browser.host() else {
        return;
    };

    let mut shift_for_the_key = false;
    let (code, character) = match info.key {
        // `Key::Char` is canonical — qutebrowser stores letters uppercase whether or not Shift was
        // held — so the character a real keyboard would have produced comes back from the modifier.
        crate::bindings::Key::Char(c) => {
            let typed = if info.mods.contains(crate::bindings::Modifiers::SHIFT) {
                c
            } else {
                c.to_ascii_lowercase()
            };
            // The virtual key code names a *physical key*, not the character it types, so it cannot
            // be the character's own value. For a letter the two happen to coincide — `'J'` is 74
            // and so is `VKEY_J` — which is why this read correctly for every key anyone had
            // injected until the first punctuation one. Measured 2026-08-06: `:` went in as code 58,
            // which is no VKEY at all, `us_layout_char` answered None, and the press was dropped
            // before it reached a binding — the injector reported success and nothing happened.
            //
            // So the US table is searched backwards for the physical key that types this character,
            // which also says whether Shift is part of typing it: `:` is `VKEY_OEM_1` **shifted**.
            let found = (0x08..=0xDF)
                .flat_map(|vkey| [(vkey, false), (vkey, true)])
                .find(|(vkey, shift)| crate::bindings::us_layout_char(*vkey, *shift) == Some(typed));
            match found {
                Some((vkey, shift)) => {
                    shift_for_the_key = shift;
                    (vkey, typed as u16)
                }
                None => (c.to_ascii_uppercase() as i32, typed as u16),
            }
        }
        crate::bindings::Key::Named(_) => {
            eprintln!("cmdline-script: only character keys can be injected so far");
            return;
        }
    };
    // `cef_event_flags_t`, sys bindings x86_64_unknown_linux_gnu.rs:2699–2724 — the same values
    // `Modifiers::from_cef` reads, spelled here rather than made public there because this is a
    // debug path and `bindings.rs` belongs to the key work.
    let mut modifiers = 0u32;
    for (mods, flag) in [
        (crate::bindings::Modifiers::SHIFT, 2),
        (crate::bindings::Modifiers::CTRL, 4),
        (crate::bindings::Modifiers::ALT, 8),
        (crate::bindings::Modifiers::META, 128),
    ] {
        if info.mods.contains(mods) {
            modifiers |= flag;
        }
    }
    // Shift is part of typing the character on this layout even when the keystring does not spell
    // it: `:` is Shift plus the `;` key, and `us_layout_char` is asked with Shift held on the way
    // back in, so the flag has to be on the wire.
    if shift_for_the_key {
        modifiers |= 2;
    }

    // All four, in the order a real press delivers them. CHAR is the one that inserts the character
    // into a focused field — trap 1 in CEF-NOTES.md is about not acting on it three times, and its
    // mirror image is that leaving CHAR out injects a key that types nothing. Measured 2026-08-06:
    // without CHAR, `d` reached `on_pre_key_event` and the input stayed empty.
    let types: &[KeyEventType] = if char_only {
        &[KeyEventType::CHAR]
    } else {
        &[KeyEventType::KEYDOWN, KeyEventType::CHAR, KeyEventType::KEYUP]
    };
    for &type_ in types {
        let event = KeyEvent {
            type_,
            modifiers,
            windows_key_code: code,
            native_key_code: 0,
            character,
            unmodified_character: character,
            ..Default::default()
        };
        host.send_key_event(Some(&event));
    }
    eprintln!(
        "cmdline-script: injected {spec} at the {}",
        if char_only { "bottom chrome, CHAR only" } else if at_page { "page" } else { "bottom chrome" }
    );
}

// -----------------------------------------------------------------------------------------------
// Tests — the pure half only. Everything above `The live one` is reachable from here.
// -----------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a line with the cursor written as `|`, the way the readline docstrings draw it.
    fn line(spec: &str) -> CmdLine {
        let mut cmd = CmdLine::new();
        let at = spec.find('|').expect("the spec needs a cursor");
        cmd.set_text(&spec.replace('|', ""));
        cmd.cursor = spec[..at].chars().count();
        cmd
    }

    /// Read one back in the same notation.
    fn show(cmd: &CmdLine) -> String {
        let mut out: String = cmd.chars[..cmd.cursor].iter().collect();
        out.push('|');
        out.extend(cmd.chars[cmd.cursor..].iter());
        out
    }

    #[test]
    fn set_text_puts_the_cursor_at_the_end() {
        let mut cmd = CmdLine::new();
        cmd.set_text(":open ");
        assert_eq!(show(&cmd), ":open |");
        assert_eq!(cmd.prefix(), Some(':'));
    }

    #[test]
    fn moving_by_character_stops_at_both_ends() {
        let mut cmd = line(":open|");
        cmd.backward_char();
        assert_eq!(show(&cmd), ":ope|n");
        cmd.beginning_of_line();
        assert_eq!(show(&cmd), "|:open");
        cmd.backward_char();
        assert_eq!(show(&cmd), "|:open", "backward-char at the start must not wrap");
        cmd.end_of_line();
        cmd.forward_char();
        assert_eq!(show(&cmd), ":open|", "forward-char at the end must not wrap");
    }

    #[test]
    fn moving_by_word_treats_punctuation_as_a_boundary() {
        // The point of the word commands on a command line: they step through a URL's parts.
        let mut cmd = line(":open https://example.com/a/b|");
        cmd.backward_word();
        assert_eq!(show(&cmd), ":open https://example.com/a/|b");
        cmd.backward_word();
        assert_eq!(show(&cmd), ":open https://example.com/|a/b");
        cmd.backward_word();
        assert_eq!(show(&cmd), ":open https://example.|com/a/b");

        let mut cmd = line("|:open https://example.com");
        cmd.forward_word();
        assert_eq!(show(&cmd), ":open| https://example.com");
        cmd.forward_word();
        assert_eq!(show(&cmd), ":open https|://example.com");
    }

    #[test]
    fn unix_line_discard_and_kill_line() {
        // <Ctrl-U> — the one everybody uses, and the reason rl-yank has anything to paste.
        let mut cmd = line(":open duckduck|go");
        cmd.unix_line_discard();
        assert_eq!(show(&cmd), "|go");
        cmd.yank();
        assert_eq!(show(&cmd), ":open duckduck|go");

        // <Ctrl-K>
        let mut cmd = line(":open duck|duckgo");
        cmd.kill_line();
        assert_eq!(show(&cmd), ":open duck|");
        cmd.beginning_of_line();
        cmd.yank();
        assert_eq!(show(&cmd), "duckgo|:open duck");
    }

    #[test]
    fn rubout_eats_trailing_boundaries_with_the_word() {
        // The three cases the loops in readlinecommands.rubout are drawn for.
        let mut cmd = line("/some/path//|");
        cmd.rubout("/");
        assert_eq!(show(&cmd), "/some/|");

        // One word, and the space before it stays — that is the boundary the loops keep.
        let mut cmd = line(":open abc def|");
        cmd.rubout(" ");
        assert_eq!(show(&cmd), ":open abc |");

        // No boundary at all: the first character goes too, which is what the `-1` is for.
        let mut cmd = line("somepath|");
        cmd.rubout(" ");
        assert_eq!(show(&cmd), "|");

        // <Ctrl-W> twice on a command line, which is what it is bound for.
        let mut cmd = line(":open one two|");
        cmd.rubout(" ");
        cmd.rubout(" ");
        assert_eq!(show(&cmd), ":open |");
    }

    #[test]
    fn kill_word_and_backward_kill_word() {
        let mut cmd = line(":open example.com|");
        cmd.backward_kill_word();
        assert_eq!(show(&cmd), ":open example.|");
        cmd.backward_kill_word();
        assert_eq!(show(&cmd), ":open |");

        let mut cmd = line(":open |example.com");
        cmd.kill_word();
        assert_eq!(show(&cmd), ":open |.com");
    }

    #[test]
    fn delete_char_does_not_touch_the_yank_buffer() {
        // _dispatch('del_') is called without delete=True, unlike every kill above. Losing that
        // makes <Ctrl-?> quietly clobber what <Ctrl-U> saved.
        let mut cmd = line(":open duck|go");
        cmd.unix_line_discard();
        assert_eq!(show(&cmd), "|go");
        cmd.delete_char();
        assert_eq!(show(&cmd), "|o");
        cmd.backward_delete_char();
        assert_eq!(show(&cmd), "|o", "backspace at the start must do nothing");
        cmd.end_of_line();
        cmd.backward_delete_char();
        assert_eq!(show(&cmd), "|");
        cmd.yank();
        assert_eq!(show(&cmd), ":open duck|", "the kill buffer must have survived both");
    }

    #[test]
    fn history_walks_back_and_forward_and_stops_at_the_ends() {
        let mut cmd = CmdLine::new();
        for entry in [":open a", ":open b", ":quit"] {
            cmd.push_history(entry);
        }
        // A repeat of the newest entry is not a new entry.
        cmd.push_history(":quit");
        assert_eq!(cmd.history().len(), 3);

        assert_eq!(cmd.history_prev().as_deref(), Some(":quit"));
        assert_eq!(cmd.history_prev().as_deref(), Some(":open b"));
        assert_eq!(cmd.history_prev().as_deref(), Some(":open a"));
        assert_eq!(cmd.history_prev(), None, "the far end must not wrap");
        assert_eq!(cmd.history_next().as_deref(), Some(":open b"));
        assert_eq!(cmd.history_next().as_deref(), Some(":quit"));
        assert_eq!(cmd.history_next(), None);
    }

    #[test]
    fn history_starts_filtered_by_what_is_typed() {
        let mut cmd = CmdLine::new();
        for entry in [":open a", ":quit", ":open b"] {
            cmd.push_history(entry);
        }
        cmd.set_text(":open");
        assert_eq!(cmd.history_prev().as_deref(), Some(":open b"));
        assert_eq!(cmd.history_prev().as_deref(), Some(":open a"), ":quit must be filtered out");
        assert_eq!(cmd.history_prev(), None);

        // Nothing typed means the whole history.
        cmd.stop_browsing();
        cmd.clear();
        assert_eq!(cmd.history_prev().as_deref(), Some(":open b"));
        assert_eq!(cmd.history_prev().as_deref(), Some(":quit"));
    }

    #[test]
    fn history_next_alone_does_nothing() {
        let mut cmd = CmdLine::new();
        cmd.push_history(":quit");
        assert!(!cmd.is_browsing());
        assert_eq!(cmd.history_next(), None);
        assert!(!cmd.is_browsing());
    }

    #[test]
    fn cmd_set_text_flags() {
        let cmd = CmdLine::new();
        // `o` is `cmd-set-text -s :open`.
        assert_eq!(cmd.cmd_set_text(":open", true, false), Ok(":open ".to_string()));
        // `go` is `cmd-set-text :open {url:pretty}` — no space flag, the text carries it.
        assert_eq!(
            cmd.cmd_set_text(":open https://a/", false, false),
            Ok(":open https://a/".to_string())
        );
        // A command line has to start with one of : / ?
        assert!(cmd.cmd_set_text("open", false, false).is_err());
        // -a with nothing to append to is qutebrowser's own error.
        assert_eq!(cmd.cmd_set_text("x", false, true), Err("No current text!".to_string()));

        let mut cmd = CmdLine::new();
        cmd.set_text(":open");
        assert_eq!(cmd.cmd_set_text(" -t", false, true), Ok(":open -t".to_string()));
    }

    /// `/` and `?` are searches. Without this bru ran a *command* named after the search term,
    /// which parses to `Unimplemented` and is dropped without a word — `/` did nothing at all.
    #[test]
    fn slash_and_question_mark_search_rather_than_running_a_command() {
        let mut cmd = CmdLine::new();
        assert_eq!(cmd.accept("/Kestrel").as_deref(), Some("search -- Kestrel"));
        assert_eq!(cmd.accept("?Kestrel").as_deref(), Some("search -r -- Kestrel"));
        // `--` is why a term may begin with a dash: `search` is maxsplit-0.
        assert_eq!(cmd.accept("/-v").as_deref(), Some("search -- -v"));
        // `:` is still a command, and must not grow a `search` in front of it.
        assert_eq!(cmd.accept(":open example.com").as_deref(), Some("open example.com"));
    }

    #[test]
    fn accept_strips_the_prefix_and_remembers_the_line() {
        let mut cmd = CmdLine::new();
        assert_eq!(cmd.accept(":open duckduckgo.com").as_deref(), Some("open duckduckgo.com"));
        assert_eq!(cmd.history(), [":open duckduckgo.com"]);

        // A leading space after the colon means "do not remember this".
        assert_eq!(cmd.accept(": open secret.example").as_deref(), Some(" open secret.example"));
        assert_eq!(cmd.history().len(), 1, "': ' must not reach the history");

        // Nothing to run.
        assert_eq!(cmd.accept(":"), None);
        assert_eq!(cmd.accept(""), None);
    }

    #[test]
    fn accept_ends_a_history_walk() {
        let mut cmd = CmdLine::new();
        cmd.push_history(":quit");
        cmd.history_prev();
        assert!(cmd.is_browsing());
        cmd.accept(":open a");
        assert!(!cmd.is_browsing(), "the next <Ctrl-P> must start a fresh walk");
    }

    #[test]
    fn the_chrome_cursor_arrives_in_utf16() {
        // An emoji is two UTF-16 code units and one char; a cursor after it is 2, not 1. Getting
        // this wrong puts the caret inside a character and every readline command after it is off.
        let mut cmd = CmdLine::new();
        cmd.sync(":open 🦀x", Some(8));
        assert_eq!(cmd.cursor(), 7, "7 chars: ':open ' is 6, the crab is 1");
        assert_eq!(cmd.cursor_utf16(), 8);
        cmd.backward_delete_char();
        assert_eq!(cmd.text(), ":open x");
    }

    #[test]
    fn a_push_from_elsewhere_does_not_look_like_an_edit() {
        // The revision is what tells the chrome "apply this text". Only this side's edits bump it,
        // or a status-bar update mid-typing would overwrite the input.
        let mut cmd = CmdLine::new();
        cmd.set_text(":open ");
        let rev = cmd.rev();
        cmd.sync(":open d", Some(7));
        assert_eq!(cmd.rev(), rev, "a report from the chrome is not an edit");
        cmd.backward_delete_char();
        assert!(cmd.rev() > rev, "a readline command is");
    }

    #[test]
    fn only_typing_keys_reach_the_command_line() {
        use crate::bindings::{Key, KeyInfo, Modifiers, NamedKey};

        // Trap 11's exception, and the whole of it.
        for key in ['j', 'o', ':', '/', '1'] {
            assert!(
                types_into_cmdline(&KeyInfo::new(Key::Char(key), Modifiers::NONE)),
                "{key} must reach the input"
            );
        }
        assert!(types_into_cmdline(&KeyInfo::new(Key::Char('A'), Modifiers::SHIFT)));
        for named in [
            NamedKey::Space,
            NamedKey::Backspace,
            NamedKey::Delete,
            NamedKey::Left,
            NamedKey::Right,
            NamedKey::Home,
            NamedKey::End,
        ] {
            assert!(types_into_cmdline(&KeyInfo::new(Key::Named(named), Modifiers::NONE)));
        }

        // The one this exception could get wrong, and the reason trap 11 is in CEF-NOTES.md:
        // forwarded to a chrome browser, <Ctrl-T> navigates that strip to chrome://newtab/.
        assert!(
            !types_into_cmdline(&KeyInfo::new(Key::Char('T'), Modifiers::CTRL)),
            "<Ctrl-T> must never reach a chrome browser"
        );
        // Every other Chromium chord goes the same way, by carrying a modifier at all.
        for (key, mods) in [
            ('W', Modifiers::CTRL),
            ('N', Modifiers::CTRL),
            ('L', Modifiers::CTRL),
            ('R', Modifiers::CTRL),
            ('D', Modifiers::ALT),
            ('T', Modifiers::META),
        ] {
            assert!(!types_into_cmdline(&KeyInfo::new(Key::Char(key), mods)));
        }
        // And the keys command mode binds stay Rust's, or the binding table stops deciding.
        for named in [
            NamedKey::Return,
            NamedKey::Enter,
            NamedKey::Escape,
            NamedKey::Tab,
            NamedKey::Up,
            NamedKey::Down,
            NamedKey::PgUp,
            NamedKey::PgDown,
        ] {
            assert!(
                !types_into_cmdline(&KeyInfo::new(Key::Named(named), Modifiers::NONE)),
                "{named:?} is bound in command mode and must stay in Rust"
            );
        }
    }

    /// `.` must not repeat itself, or one press is an infinite loop and every press after the first
    /// repeats nothing. `runners.py:177-179` is the whole rule, and it is the whole rule here.
    #[test]
    fn the_repeat_command_is_the_only_thing_that_is_not_recorded() {
        use crate::commands::parse;

        assert!(records_as_last(&parse("tab-close").unwrap()));
        assert!(records_as_last(&parse("scroll down").unwrap()));
        // A `:quit` is recorded like anything else. qutebrowser excludes nothing but the repeat
        // command, and inventing a second exclusion here would make `.` disagree with it.
        assert!(records_as_last(&parse("quit").unwrap()));
        // The four names at `runners.py:181-182` are excluded from *macro recording*, not from
        // this — `cmd-set-text` is how `o` opens the command line, and `.` after `o` has to repeat
        // it or the binding stops being repeatable.
        assert!(records_as_last(&parse("cmd-set-text -s :open").unwrap()));

        assert!(!records_as_last(&parse("cmd-repeat-last").unwrap()));
        // The pre-2.0 spelling parses to the same variant, so it is excluded by the same arm.
        assert!(!records_as_last(&parse("repeat-command").unwrap()));
        // A chain is recorded whole or not at all: the flag is cleared inside the loop over the
        // parsed chain, so one repeat command anywhere in it is enough.
        assert!(!records_as_last(&parse("clear-keychain ;; cmd-repeat-last").unwrap()));
        assert!(records_as_last(&parse("clear-keychain ;; search").unwrap()));
    }

    /// The file `:save` writes, and the cap it writes under.
    #[test]
    fn the_command_history_survives_a_round_trip_through_the_file() {
        let dir = std::env::temp_dir().join(format!("bru-cmdline-{}", std::process::id()));
        let path = dir.join("cmd-history");

        let entries: Vec<String> = vec![
            ":open example.com".to_string(),
            ":quit".to_string(),
            // Neither of these may reach the file: an empty line is not a command, and a line with
            // a newline in it would come back as two.
            String::new(),
            ":open a\n:quit".to_string(),
        ];
        assert_eq!(write_history(&path, &entries).unwrap(), 2);
        assert_eq!(read_history(&path), [":open example.com", ":quit"]);

        // The cap keeps the newest, because that is what `<Ctrl-P>` reaches first.
        let many: Vec<String> = (0..HISTORY_MAX + 20).map(|i| format!(":open {i}")).collect();
        assert_eq!(write_history(&path, &many).unwrap(), HISTORY_MAX);
        let back = read_history(&path);
        assert_eq!(back.len(), HISTORY_MAX);
        assert_eq!(back.last().unwrap(), &format!(":open {}", HISTORY_MAX + 19));
        assert_eq!(back.first().unwrap(), ":open 20");

        // A missing file is an empty history, not an error: the first run of bru has none.
        assert!(read_history(&dir.join("nothing-here")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn quoted_arguments_arrive_from_the_binding_intact() {
        // <Ctrl-W> is bound to `rl-rubout " "`, and an unimplemented command keeps its quotes.
        assert_eq!(unquote("\" \""), " ");
        assert_eq!(unquote("' /'"), " /");
        assert_eq!(unquote("/"), "/");
        assert_eq!(unquote(""), "");
    }
}
