//! `bru://help` — every key and every command, generated from the tables bru actually runs on.
//!
//! It is built at request time from [`crate::config::Bindings`], [`crate::exec::is_live`] and
//! [`crate::exec::refusal`], not written by hand, for one reason: a help page maintained separately
//! drifts, and a help page that disagrees with the browser is worse than no help page. If
//! `config.lua` rebinds a key, this shows the user's key. If a milestone implements a command, its
//! rows stop saying "not yet".
//!
//! **A row is in one of three states, and the third one is why this file was touched again.** Live
//! and "not yet" are not enough: thirteen of the 264 default bindings name something CEF 151 cannot
//! do at all, and marking those "not yet" reads as a promise. They say **refused**, with the reason
//! the module that measured it wrote — see [`crate::exec::refusal`]. The distinction is the
//! difference between a key waiting for work and a key waiting for nothing.
//!
//! Served like the rest of the chrome, over the `bru://` scheme — see `src/chrome.rs`.
//!
//! # The second half: every command, not only the bound ones
//!
//! The key table above answers "what does this key do". It cannot answer "what can I type", and by
//! 2026-08-07 that had become the larger question. Measured that morning: 160 commands under 166
//! names, 298 bound keys, and **53 commands no key reaches at all** — `screenshot`, `jseval`,
//! `config-diff`, `tab-take` and the rest of the thirty-two that landed that day. Every one of them
//! existed, worked, and was written down nowhere. A command nobody can find is a command nobody
//! uses, and every workstream that lands makes the gap wider.
//!
//! So [`COMMANDS`] is the other half of the page, and the column that joins the two is **which key
//! calls it**. That join is the whole reason both halves are one document: `:tab-select` reads very
//! differently once the page says `gt` types it, and `j` reads differently once the page says the
//! command behind it is `scroll` and lists the seven other directions it takes.
//!
//! ## How the list stays true, and what that cannot catch
//!
//! There is no runtime list of commands to build this from: [`crate::commands::parse`] is a `match`
//! over string literals and nothing enumerates it. So [`COMMANDS`] **is** a second list, and the
//! only question worth answering is what stops it drifting. Four guards, none of which is a comment
//! asking the next person to remember:
//!
//! 1. **The names are read out of the source.** `every_command_bru_understands_has_a_row` reads
//!    `src/commands.rs` and `src/cmdline.rs` as text and extracts the two places a command name is
//!    written down — the arms of `parse_one`'s `match name.as_str()`, and the `matches!` in
//!    `cmdline::is_named` — then asserts **set equality** with the names below. A command added to
//!    the parser and not to this table fails, and so does a name here the parser has never heard
//!    of. That match holds **463 string literals, 317 of them distinct**, and only 146 are command
//!    names; the rest are argument values (`down`, `links`, `pretty-url`). They are told apart by
//!    **brace depth**: an arm head of that one `match` sits at depth 0 of its own block, and every
//!    nested `match dir { "up" => … }` is inside an arm body at depth 1 or more. Ignoring the depth
//!    was measured on 2026-08-07 and put 35 argument values on the page as commands.
//! 2. **The `Command` enum is covered exhaustively.** `every_command_variant_is_reachable_by_name`
//!    matches over [`crate::commands::Command`] with **no `_` arm**, the same trick
//!    `exec::run` uses, and asserts every variant is produced by parsing something this table
//!    names. A new variant does not compile until it is listed there and does not pass until a row
//!    below reaches it.
//! 3. **Every row is executed against the parser.** Each row carries a spelling that must parse,
//!    and its state comes from [`crate::exec::is_live`] and [`crate::exec::refusal`] — the same two
//!    authorities the key table asks, so the halves cannot contradict each other about `hint-follow`.
//! 4. **The flags are read out of the source too**, for the arms that parse them inline:
//!    `every_flag_the_parser_reads_is_on_the_page` collects the literals each arm passes to
//!    `has` / `any` / `value` / `Flagged::new` and asserts set equality with the row's flags.
//!
//! **What none of that catches**, said plainly because the next person will otherwise have to find
//! it out:
//!
//! - **A consistent rename.** Renaming `screenshot` to `capture` in both the parser and this table
//!   passes every guard, as it should — but so does renaming it in the parser and *here* while the
//!   binding table, the completion and the user's fingers still say `screenshot`. Source-reading
//!   tests guard against deletion, not against a rename; `chrome.rs`'s colour test says the same
//!   thing about itself.
//! - **A wrong sentence.** Nothing checks that `what` describes what the command does. A row that
//!   says `:screenshot` reloads the page passes all four guards.
//! - **Nine commands' flags.** `set`, the six other `config-*` commands that share
//!   `parse_config_command`, `bind` and `unbind` parse their flags in hand-written functions with
//!   no `has`/`any` call to find, so guard 4 sees nothing for them and asserts nothing. The list of
//!   nine is itself asserted, so a *tenth* is a failure rather than a silent hole — but the flags of
//!   those nine are checked only by the weaker "the parser accepts it" test, and those parsers
//!   reject an unknown flag, which is what makes even that worth something.
//! - **A flag added to an inline arm and documented wrongly.** Guard 4 compares names, not
//!   meanings: writing `-f` where the parser reads `-f/--force` fails, but describing `--force` as
//!   "ask first" does not.

use std::collections::BTreeMap;

use crate::commands;
use crate::config::Bindings;
use crate::modes::Mode;

/// What a row says about its binding.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    /// Pressing it does something.
    Live,
    /// Bound, parsed, and waiting for a milestone.
    NotYet,
    /// Bound, parsed, and waiting for nothing — with the reason.
    Refused(&'static str),
}

impl State {
    /// The one place a command string is turned into a row's state, so the page and the count
    /// cannot disagree about a binding.
    fn of(cmd: &str) -> State {
        let Ok(command) = commands::parse(cmd) else {
            // A `config.lua` naming a command bru has never heard of. Not refused — bru may grow
            // it — and not live.
            return State::NotYet;
        };
        if crate::exec::is_live(&command) {
            return State::Live;
        }
        match crate::exec::refusal(&command) {
            Some(why) => State::Refused(why),
            None => State::NotYet,
        }
    }

    /// The `<tr>` class, which is what `chrome.css` styles by.
    fn class(self) -> &'static str {
        match self {
            State::Live => "live",
            State::NotYet => "todo",
            State::Refused(_) => "refused",
        }
    }

    /// The right-hand column.
    fn label(self) -> &'static str {
        match self {
            State::Live => "",
            State::NotYet => "not yet",
            State::Refused(_) => "refused",
        }
    }
}

/// One command bru understands, as the page prints it.
///
/// Everything here is written by hand and everything here is checked against the parser — see the
/// four guards in the module docs. The fields are what a reader of a help page asks for in order:
/// what is it called, what does it take, what does it do.
pub struct Doc {
    /// Every spelling the parser accepts for **this one command**. The first is the name the page
    /// prints; the rest are aliases and are printed beside it.
    ///
    /// Aliases only. Where one arm of the parser answers to several *different* commands —
    /// `message-info` and `message-error`, `macro-record` and `macro-run` — each gets its own row,
    /// because the arm they share is an implementation detail and the reader is looking up one of
    /// them. The name test compares sets of names and does not care how the arms are grouped.
    pub names: &'static [&'static str],
    /// The positional arguments, written as they would be typed. Empty when there are none.
    pub args: &'static str,
    /// Every flag the parser reads, each written as it is typed and short/long spellings joined
    /// with `/`. A flag the parser reads only in order to **refuse** the command is here too — it
    /// is a flag you can type and get an answer from — and [`Doc::what`] says so.
    pub flags: &'static [&'static str],
    /// One line. What it does, not how.
    pub what: &'static str,
    /// A spelling that parses, so the page can ask [`crate::exec::is_live`] and
    /// [`crate::exec::refusal`] about a real [`crate::commands::Command`] rather than about a name.
    ///
    /// It is the command's own name wherever the name alone parses, and the smallest thing that
    /// parses otherwise. It is never shown; it is the row's connection to the browser.
    pub example: &'static str,
}

/// Every command bru understands, in the order `src/commands.rs` implements them — which groups
/// them by the module that owns them, and is the order a reader of the source would find them in.
///
/// The tail of the list is `src/cmdline.rs`'s: nineteen names that never become a
/// [`crate::commands::Command`] variant at all and are matched as text by `cmdline::is_named`.
/// They are commands all the same, they are bound to keys, and leaving them off would make this
/// page disagree with the table above it about `<Ctrl-A>` in command mode.
pub const COMMANDS: &[Doc] = &[
    Doc { names: &["nop"], args: "", flags: &[],
        what: "Do nothing. Bound where a key must reach neither the page nor a browser default.",
        example: "nop" },
    Doc { names: &["clear-keychain"], args: "", flags: &[],
        what: "Forget a half-typed key sequence.", example: "clear-keychain" },
    Doc { names: &["mode-enter"], args: "<mode>", flags: &[],
        what: "Enter a mode by name. The modes a question puts you in cannot be entered this way.",
        example: "mode-enter insert" },
    Doc { names: &["mode-leave"], args: "", flags: &[],
        what: "Leave the mode you are in for normal mode.", example: "mode-leave" },

    Doc { names: &["scroll"], args: "<up|down|left|right|top|bottom|page-up|page-down>", flags: &[],
        what: "Scroll the page. A count repeats it. This is the wheel event bru was built for.",
        example: "scroll down" },
    Doc { names: &["scroll-px"], args: "<dx> <dy>", flags: &[],
        what: "Scroll by a number of pixels.", example: "scroll-px 0 100" },
    Doc { names: &["scroll-page"], args: "<x> <y>", flags: &[],
        what: "Scroll by a fraction of a page. A count multiplies both.", example: "scroll-page 0 1" },
    Doc { names: &["scroll-to-perc"], args: "[percentage]", flags: &["-x/--horizontal"],
        what: "Jump to a percentage of the page; with no percentage, to its end.",
        example: "scroll-to-perc 50" },

    Doc { names: &["tab-next"], args: "", flags: &[], what: "Show the next tab.", example: "tab-next" },
    Doc { names: &["tab-prev"], args: "", flags: &[], what: "Show the previous tab.", example: "tab-prev" },
    Doc { names: &["tab-close"], args: "", flags: &["-o/--opposite", "-f/--force"],
        what: "Close the showing tab, or with -o every tab on the other side of it.",
        example: "tab-close" },
    Doc { names: &["tab-only"], args: "", flags: &["-f/--force"],
        what: "Close every tab in this window except the one showing.", example: "tab-only" },
    Doc { names: &["tab-focus"], args: "[index]", flags: &[],
        what: "Show a tab by its number; negative counts from the end, and last returns to the \
               tab you came from.",
        example: "tab-focus 1" },
    Doc { names: &["tab-move"], args: "[+|-|start|end|index]", flags: &[],
        what: "Move the showing tab along the strip.", example: "tab-move end" },
    Doc { names: &["tab-clone"], args: "", flags: &["-b/--bg", "-w/--window", "-p/--private"],
        what: "Open the showing page a second time.", example: "tab-clone" },
    Doc { names: &["undo"], args: "", flags: &["-w/--window"],
        what: "Reopen the last closed tab, or with -w the last closed window and its tabs.",
        example: "undo" },
    Doc { names: &["tab-pin"], args: "", flags: &[],
        what: "Pin the showing tab, or unpin it: a pinned tab keeps its place and asks before it closes.",
        example: "tab-pin" },
    Doc { names: &["tab-mute"], args: "", flags: &[],
        what: "Mute the showing tab's audio, or unmute it.", example: "tab-mute" },
    Doc { names: &["tab-give"], args: "[window-id]", flags: &[],
        what: "Move the showing tab to another window, or with no id to a new one.",
        example: "tab-give" },

    Doc { names: &["session-save"], args: "[name]", flags: &["-f/--force"],
        what: "Write the open windows and tabs to a session file.", example: "session-save" },
    Doc { names: &["session-load"], args: "<name>", flags: &["-c/--clear", "--history"],
        what: "Open the tabs a session file holds. --history refetches each tab's back list, \
               which bru does not restore otherwise.",
        example: "session-load default" },
    Doc { names: &["session-delete"], args: "<name>", flags: &[],
        what: "Delete a session file.", example: "session-delete default" },

    Doc { names: &["open"], args: "[url]",
        flags: &["-t/--tab", "-b/--bg", "-w/--window", "-p/--private", "-r/--related"],
        what: "Open a URL, a file or a search. What a bare word means is decided by src/open.rs.",
        example: "open example.com" },
    Doc { names: &["back"], args: "", flags: &["-t/--tab", "-b/--bg", "-w/--window"],
        what: "Go back in this tab's history. A count goes back that many entries.", example: "back" },
    Doc { names: &["forward"], args: "", flags: &["-t/--tab", "-b/--bg", "-w/--window"],
        what: "Go forward in this tab's history.", example: "forward" },
    Doc { names: &["reload"], args: "", flags: &["-f/--force"],
        what: "Reload the page; with -f, ignoring the cache.", example: "reload" },
    Doc { names: &["stop"], args: "", flags: &[], what: "Stop loading the page.", example: "stop" },
    Doc { names: &["home"], args: "", flags: &[],
        what: "Open the start page in this tab.", example: "home" },
    Doc { names: &["quit"], args: "", flags: &["--save"],
        what: "Close every window and exit.", example: "quit" },
    Doc { names: &["close"], args: "", flags: &[],
        what: "Close this window. The last one closing exits.", example: "close" },

    Doc { names: &["zoom"], args: "[percentage]", flags: &[],
        what: "Set the page's zoom; with no value, back to 100%.", example: "zoom" },
    Doc { names: &["zoom-in"], args: "", flags: &[], what: "Zoom in one step.", example: "zoom-in" },
    Doc { names: &["zoom-out"], args: "", flags: &[], what: "Zoom out one step.", example: "zoom-out" },
    Doc { names: &["fullscreen"], args: "", flags: &["--enter", "--leave"],
        what: "Toggle the window's fullscreen, or force it either way.", example: "fullscreen" },

    Doc { names: &["hint"], args: "[group] [target] [text]",
        flags: &["--mode", "--add-history", "-r/--rapid", "-f/--first"],
        what: "Label the page's elements and act on the one whose label is typed. --mode and \
               --add-history name features bru has not built, and the command does nothing when \
               either is given rather than hinting the wrong way.",
        example: "hint" },
    Doc { names: &["hint-follow"], args: "", flags: &[],
        what: "Follow the hint being typed. Bound to <Return> in hint mode and refused — see the \
               reason in the row.",
        example: "hint-follow" },

    Doc { names: &["help"], args: "", flags: &["-t/--tab"],
        what: "Open this page.", example: "help" },

    Doc { names: &["download"], args: "[url]", flags: &["--dest", "-m/--mhtml"],
        what: "Download a URL, or the showing page. --dest names a destination bru cannot yet \
               honour and the command does nothing when it is given; --mhtml saves the whole page \
               and its assets as one file.",
        example: "download" },
    Doc { names: &["download-cancel"], args: "", flags: &["-a/--all"],
        what: "Cancel a download the count names, or the last one.", example: "download-cancel" },
    Doc { names: &["download-clear"], args: "", flags: &[],
        what: "Forget the finished downloads. No file is touched.", example: "download-clear" },
    Doc { names: &["download-open"], args: "[command]", flags: &["-d/--dir"],
        what: "Open a finished download, or with -d the directory it landed in.",
        example: "download-open" },
    Doc { names: &["download-delete"], args: "", flags: &[],
        what: "Delete a finished download's file and its row.", example: "download-delete" },
    Doc { names: &["download-retry"], args: "", flags: &[],
        what: "Start a failed download again.", example: "download-retry" },

    Doc { names: &["quickmark-save"], args: "[name]", flags: &[],
        what: "Save the showing page as a quickmark; with no name the command line is prefilled \
               instead of a question being asked.",
        example: "quickmark-save" },
    Doc { names: &["quickmark-load"], args: "<name>", flags: &["-t/--tab", "-b/--bg", "-w/--window"],
        what: "Open a quickmark.", example: "quickmark-load x" },
    Doc { names: &["quickmark-del"], args: "[name]", flags: &[],
        what: "Delete a quickmark; with no name, the one on the showing page.",
        example: "quickmark-del" },
    Doc { names: &["quickmark-add"], args: "<url> <name>", flags: &[],
        what: "Save a URL as a quickmark under a name, naming both.",
        example: "quickmark-add https://example.com/ x" },
    Doc { names: &["quickmarks-reload"], args: "", flags: &[],
        what: "Re-read the quickmarks file from disk.", example: "quickmarks-reload" },
    Doc { names: &["bookmark-add"], args: "[url] [title]", flags: &["--toggle"],
        what: "Bookmark a URL, or the showing page.", example: "bookmark-add" },
    Doc { names: &["bookmark-load"], args: "<url>",
        flags: &["-t/--tab", "-b/--bg", "-w/--window", "-d/--delete"],
        what: "Open a bookmark, and with -d delete it as it opens.",
        example: "bookmark-load https://example.com/" },
    Doc { names: &["bookmark-del"], args: "[url]", flags: &[],
        what: "Delete a bookmark; with no URL, the showing page's.", example: "bookmark-del" },
    Doc { names: &["bookmark-list"], args: "", flags: &["--jump", "-b/--bg"],
        what: "Open the page that lists the bookmarks and quickmarks.", example: "bookmark-list" },
    Doc { names: &["bookmarks-reload"], args: "", flags: &[],
        what: "Re-read the bookmarks file from disk.", example: "bookmarks-reload" },
    Doc { names: &["history"], args: "", flags: &["-b/--bg"],
        what: "Open the page that lists what bru has visited.", example: "history" },
    Doc { names: &["history-clear"], args: "", flags: &["-f/--force"],
        what: "Empty bru's visit log and the completion built from it.", example: "history-clear" },

    Doc { names: &["cookies"], args: "[domain]", flags: &["-b/--bg"],
        what: "Open the cookie page, filtered to a domain when one is named. Deleting is done on \
               the page.",
        example: "cookies" },

    Doc { names: &["yank"], args: "[url|pretty-url|title|domain|selection|inline <text>]",
        flags: &["-s/--sel"],
        what: "Copy something about the page to the clipboard, or with -s to the primary selection.",
        example: "yank url" },

    Doc { names: &["search"], args: "[text]", flags: &["-r/--reverse"],
        what: "Find text on the page. With no text the search is cleared.", example: "search" },
    Doc { names: &["search-next"], args: "", flags: &[],
        what: "Go to the next match, in the direction the search was started in.",
        example: "search-next" },
    Doc { names: &["search-prev"], args: "", flags: &[],
        what: "Go to the previous match.", example: "search-prev" },
    Doc { names: &["navigate"], args: "<prev|next|up|increment|decrement|strip>",
        flags: &["-t/--tab", "-b/--bg", "-w/--window"],
        what: "Follow a next/previous link, walk up the path, or step the last number in the URL.",
        example: "navigate up" },
    Doc { names: &["scroll-to-anchor"], args: "<name>", flags: &[],
        what: "Go to a fragment on the page. A navigation, not a wheel event.",
        example: "scroll-to-anchor top" },

    Doc { names: &["selection-toggle"], args: "", flags: &["--line"],
        what: "Start or stop selecting from the caret.", example: "selection-toggle" },
    Doc { names: &["selection-drop"], args: "", flags: &[],
        what: "Drop the selection and keep the caret.", example: "selection-drop" },
    Doc { names: &["selection-reverse"], args: "", flags: &[],
        what: "Swap which end of the selection the caret is on.", example: "selection-reverse" },
    Doc { names: &["selection-follow"], args: "", flags: &["-t/--tab"],
        what: "Follow the link the selection is on.", example: "selection-follow" },
    Doc { names: &["move-to-next-char"], args: "", flags: &[],
        what: "Caret: one character right.", example: "move-to-next-char" },
    Doc { names: &["move-to-prev-char"], args: "", flags: &[],
        what: "Caret: one character left.", example: "move-to-prev-char" },
    Doc { names: &["move-to-next-line"], args: "", flags: &[],
        what: "Caret: one line down.", example: "move-to-next-line" },
    Doc { names: &["move-to-prev-line"], args: "", flags: &[],
        what: "Caret: one line up.", example: "move-to-prev-line" },
    Doc { names: &["move-to-end-of-word"], args: "", flags: &[],
        what: "Caret: to the end of this word.", example: "move-to-end-of-word" },
    Doc { names: &["move-to-next-word"], args: "", flags: &[],
        what: "Caret: to the start of the next word.", example: "move-to-next-word" },
    Doc { names: &["move-to-prev-word"], args: "", flags: &[],
        what: "Caret: to the start of the previous word.", example: "move-to-prev-word" },
    Doc { names: &["move-to-start-of-line"], args: "", flags: &[],
        what: "Caret: to the start of the line.", example: "move-to-start-of-line" },
    Doc { names: &["move-to-end-of-line"], args: "", flags: &[],
        what: "Caret: to the end of the line.", example: "move-to-end-of-line" },
    Doc { names: &["move-to-start-of-next-block"], args: "", flags: &[],
        what: "Caret: to the start of the next block.", example: "move-to-start-of-next-block" },
    Doc { names: &["move-to-start-of-prev-block"], args: "", flags: &[],
        what: "Caret: to the start of the previous block.", example: "move-to-start-of-prev-block" },
    Doc { names: &["move-to-end-of-next-block"], args: "", flags: &[],
        what: "Caret: to the end of the next block.", example: "move-to-end-of-next-block" },
    Doc { names: &["move-to-end-of-prev-block"], args: "", flags: &[],
        what: "Caret: to the end of the previous block.", example: "move-to-end-of-prev-block" },
    Doc { names: &["move-to-start-of-document"], args: "", flags: &[],
        what: "Caret: to the top of the document.", example: "move-to-start-of-document" },
    Doc { names: &["move-to-end-of-document"], args: "", flags: &[],
        what: "Caret: to the bottom of the document.", example: "move-to-end-of-document" },

    Doc { names: &["cmd-set-text"], args: "<text>",
        flags: &["-s/--space", "-a/--append", "-r/--run-on-count"],
        what: "Open the command line with text already in it. This is what most of the keys that \
               look like they open a URL actually run.",
        example: "cmd-set-text :open" },
    Doc { names: &["command-accept"], args: "", flags: &["--rapid"],
        what: "Run what the command line holds; with --rapid, and stay in command mode.",
        example: "command-accept" },
    Doc { names: &["cmd-repeat-last", "repeat-command"], args: "", flags: &[],
        what: "Run the last command again.", example: "cmd-repeat-last" },
    Doc { names: &["edit-command", "cmd-edit"], args: "", flags: &["--run"],
        what: "Edit the command line in $EDITOR.", example: "edit-command" },
    Doc { names: &["edit-url"], args: "[url]", flags: &["-t/--tab", "-b/--bg", "-w/--window"],
        what: "Edit the page's URL in $EDITOR and open it if it changed.", example: "edit-url" },
    Doc { names: &["edit-text", "open-editor"], args: "", flags: &[],
        what: "Edit the focused text field in $EDITOR.", example: "edit-text" },
    Doc { names: &["insert-text"], args: "<text>", flags: &[],
        what: "Type text into the focused field.", example: "insert-text x" },
    Doc { names: &["fake-key"], args: "<keystring>", flags: &["-g/--global"],
        what: "Send a keypress to the page as if it had been typed.", example: "fake-key <Escape>" },
    Doc { names: &["spawn"], args: "<command> [arguments…]",
        flags: &["-u/--userscript", "-d/--detach", "-o/--output", "-m/--output-messages",
                 "-v/--verbose"],
        what: "Run a program, or with -u a userscript. -o would show the output in a tab bru has \
               no page for, and the command does nothing when it is given rather than running the \
               program and showing nothing.",
        example: "spawn true" },
    Doc { names: &["process"], args: "[pid] [show|terminate|kill]", flags: &[],
        what: "Look at what :spawn started, or stop it.", example: "process" },
    Doc { names: &["restart"], args: "", flags: &[],
        what: "Save the open tabs, start bru again and reopen them.", example: "restart" },

    Doc { names: &["set"], args: "[option] [value]",
        flags: &["-p/--print", "-t/--temp", "-u/--pattern/--url"],
        what: "Print a setting, or change it. With no option at all it opens the settings page. \
               -t is accepted and does nothing: bru writes no configuration file, so every :set is \
               already temporary.",
        example: "set content.images" },
    Doc { names: &["config-cycle"], args: "<option> [values…]",
        flags: &["-p/--print", "-t/--temp", "-u/--pattern/--url"],
        what: "Step a setting through a list of values, or through true and false.",
        example: "config-cycle content.images" },
    Doc { names: &["config-dict-add"], args: "<option> <key> <value>",
        flags: &["-p/--print", "-t/--temp", "-u/--pattern/--url", "--replace"],
        what: "Put one pair into a dictionary setting.",
        example: "config-dict-add url.searchengines zz https://example.com/?q={}" },
    Doc { names: &["config-dict-remove"], args: "<option> <key>",
        flags: &["-p/--print", "-t/--temp", "-u/--pattern/--url"],
        what: "Take one pair out of a dictionary setting. The only way to make an entry bru ships \
               stop existing, because an override merges rather than replaces.",
        example: "config-dict-remove url.searchengines zz" },
    Doc { names: &["config-list-add"], args: "<option> <value>",
        flags: &["-p/--print", "-t/--temp", "-u/--pattern/--url"],
        what: "Append one entry to a list setting.",
        example: "config-list-add content.blocking.adblock.lists https://example.com/list.txt" },
    Doc { names: &["config-list-remove"], args: "<option> <value>",
        flags: &["-p/--print", "-t/--temp", "-u/--pattern/--url"],
        what: "Take one entry out of a list setting.",
        example: "config-list-remove content.blocking.adblock.lists https://example.com/list.txt" },
    Doc { names: &["config-unset"], args: "<option>",
        flags: &["-p/--print", "-t/--temp", "-u/--pattern/--url"],
        what: "Put one setting back to bru's own value.", example: "config-unset content.images" },
    Doc { names: &["config-clear"], args: "", flags: &["--save"],
        what: "Put every setting back to bru's own, in one move. It leaves the bindings alone. \
               --save is refused out loud rather than ignored: the file it would empty is not bru's.",
        example: "config-clear" },
    Doc { names: &["config-diff"], args: "", flags: &[],
        what: "Print everything this browser is running that is not bru's own, as the Lua that \
               would reproduce it.",
        example: "config-diff" },
    Doc { names: &["config-source"], args: "[filename]", flags: &["--clear"],
        what: "Re-read config.lua over the running browser.", example: "config-source" },
    Doc { names: &["config-edit"], args: "", flags: &["--no-source/--no_source"],
        what: "Open config.lua in $EDITOR and re-read it afterwards. bru creates neither the file \
               nor its directory.",
        example: "config-edit" },
    Doc { names: &["config-write-py"], args: "", flags: &[],
        what: "Refused. Writing config.lua is not bru's to do; :config-diff prints the same text \
               for you to put there yourself.",
        example: "config-write-py" },
    Doc { names: &["bind"], args: "[key] [command]", flags: &["-m/--mode", "-d/--default"],
        what: "Bind a key in the running browser, print what one is bound to, put bru's own \
               binding back with --default, or with no key open this page. Nothing is written to \
               disk.",
        example: "bind" },
    Doc { names: &["unbind"], args: "<key>", flags: &["-m/--mode"],
        what: "Take a binding out of the running browser.", example: "unbind ZZ" },

    Doc { names: &["completion-item-focus"],
        args: "<next|prev|next-category|prev-category|next-page|prev-page>",
        flags: &["-H/--history"],
        what: "Move the highlight in the completion; with -H, walk the command history when there \
               is no completion.",
        example: "completion-item-focus next" },
    Doc { names: &["completion-item-del"], args: "", flags: &[],
        what: "Delete the completion entry that is highlighted.", example: "completion-item-del" },
    Doc { names: &["completion-item-yank"], args: "", flags: &["--sel"],
        what: "Copy the highlighted completion entry.", example: "completion-item-yank" },

    Doc { names: &["prompt-accept"], args: "[value]", flags: &["--save"],
        what: "Answer the question that is open; --save remembers the answer for this site.",
        example: "prompt-accept" },
    Doc { names: &["prompt-item-focus"], args: "<next|prev>", flags: &[],
        what: "Move through a question's file list, or between a login's two fields.",
        example: "prompt-item-focus next" },
    Doc { names: &["prompt-open-download"], args: "[command]", flags: &["--pdfjs"],
        what: "Answer a download's filename question with somewhere temporary, and open it when it \
               lands.",
        example: "prompt-open-download" },
    Doc { names: &["prompt-yank"], args: "", flags: &["--sel"],
        what: "Copy the URL the open question is about.", example: "prompt-yank" },
    Doc { names: &["prompt-fileselect-external"], args: "", flags: &[],
        what: "Hand a file question to a real file browser.", example: "prompt-fileselect-external" },

    Doc { names: &["adblock-update"], args: "", flags: &[],
        what: "Fetch the filter lists and recompile them.", example: "adblock-update" },
    Doc { names: &["adblock-toggle"], args: "", flags: &[],
        what: "Turn blocking on or off for this session.", example: "adblock-toggle" },
    Doc { names: &["adblock-info"], args: "", flags: &[],
        what: "What is loaded, what it has blocked, and what it costs per request.",
        example: "adblock-info" },
    Doc { names: &["greasemonkey-reload"], args: "", flags: &["-f/--force", "-q/--quiet"],
        what: "Re-read the user scripts and tell every renderer to do the same. -f would \
               re-download a script's requirements, which bru never fetches, and the command does \
               nothing when it is given.",
        example: "greasemonkey-reload" },

    Doc { names: &["view-source"], args: "", flags: &["-e/--edit", "--pygments"],
        what: "Show the page's own source in a tab. --pygments names a highlighter bru does not \
               ship and the command does nothing when it is given.",
        example: "view-source" },
    Doc { names: &["print"], args: "", flags: &[],
        what: "Hand the page to Chromium's print dialog.", example: "print" },
    Doc { names: &["devtools"], args: "[position]", flags: &[],
        what: "Open the web inspector, or close it. Every position opens a window — CEF has no \
               docked inspector to give a view.",
        example: "devtools" },
    Doc { names: &["devtools-focus"], args: "", flags: &[],
        what: "Bring the inspector forward.", example: "devtools-focus" },
    Doc { names: &["message-info"], args: "<text>", flags: &[],
        what: "Say something in the status bar.", example: "message-info x" },
    Doc { names: &["message-warning"], args: "<text>", flags: &[],
        what: "Say something in the status bar, as a warning.", example: "message-warning x" },
    Doc { names: &["message-error"], args: "<text>", flags: &[],
        what: "Say something in the status bar, as an error.", example: "message-error x" },
    Doc { names: &["messages"], args: "[level]",
        flags: &["-f/--logfilter", "--plain", "-t/--tab", "-b/--bg", "-w/--window"],
        what: "Open the page holding everything the status bar has said.", example: "messages" },
    Doc { names: &["clear-messages"], args: "", flags: &[],
        what: "Take whatever the status bar is saying away now.", example: "clear-messages" },

    Doc { names: &["macro-record"], args: "[register]", flags: &[],
        what: "Start recording keys into a register, or stop the recording that is running.",
        example: "macro-record" },
    Doc { names: &["macro-run"], args: "[register]", flags: &[],
        what: "Replay a register. A count replays it that many times; @ means the last one run.",
        example: "macro-run" },

    Doc { names: &["save"], args: "[what…]", flags: &[],
        what: "Write bru's own files — history, quickmarks, bookmarks — to disk now. Not the page.",
        example: "save" },
    Doc { names: &["tab-select"], args: "[[window-id/]index or text]", flags: &[],
        what: "Show a tab by address, or by a word in its title or URL.", example: "tab-select" },
    Doc { names: &["tab-take"], args: "<[window-id/]index>", flags: &["-k/--keep"],
        what: "Take a tab from another window into this one.", example: "tab-take 1/1" },
    Doc { names: &["window-only"], args: "", flags: &[],
        what: "Close every window except this one.", example: "window-only" },
    Doc { names: &["screenshot"], args: "<filename>", flags: &["--rect", "-f/--force"],
        what: "Write the showing page to a PNG; --rect WxH+X+Y takes part of it.",
        example: "screenshot /tmp/x.png" },
    Doc { names: &["jseval"], args: "<javascript>",
        flags: &["-f/--file", "-u/--url", "--world", "-q/--quiet"],
        what: "Run JavaScript in the page. --world names an isolated world bru does not offer and \
               the command does nothing when it is given.",
        example: "jseval 1" },
    Doc { names: &["click-element"], args: "<id|css|position|focused> [value]",
        flags: &["--target", "--force-event", "--select-first"],
        what: "Click an element the page holds, chosen without a hint label.",
        example: "click-element focused" },
    Doc { names: &["download-remove"], args: "", flags: &["-a/--all"],
        what: "Take a finished download off the list, keeping its file.", example: "download-remove" },
    Doc { names: &["later", "cmd-later"], args: "<duration> <command>", flags: &[],
        what: "Run a command after a delay. Everything after the duration is the command, \
               separators included.",
        example: "later 1s reload" },
    Doc { names: &["repeat", "cmd-repeat"], args: "<times> <command>", flags: &[],
        what: "Run a command several times. A count multiplies the number.", example: "repeat 2 reload" },
    Doc { names: &["run-with-count", "cmd-run-with-count"], args: "<count> <command>", flags: &[],
        what: "Run a command as though a count had been typed before it.",
        example: "run-with-count 2 scroll down" },
    Doc { names: &["version"], args: "", flags: &["-p/--paste"],
        what: "Open the page naming this build and the Chromium under it.", example: "version" },

    // `src/cmdline.rs`'s eighteen, matched as text rather than parsed into a variant.
    Doc { names: &["command-history-prev"], args: "", flags: &[],
        what: "Recall the previous line from the command history.", example: "command-history-prev" },
    Doc { names: &["command-history-next"], args: "", flags: &[],
        what: "Recall the next line from the command history.", example: "command-history-next" },
    Doc { names: &["rl-backward-char"], args: "", flags: &[],
        what: "Command line: one character left.", example: "rl-backward-char" },
    Doc { names: &["rl-forward-char"], args: "", flags: &[],
        what: "Command line: one character right.", example: "rl-forward-char" },
    Doc { names: &["rl-backward-word"], args: "", flags: &[],
        what: "Command line: one word left.", example: "rl-backward-word" },
    Doc { names: &["rl-forward-word"], args: "", flags: &[],
        what: "Command line: one word right.", example: "rl-forward-word" },
    Doc { names: &["rl-beginning-of-line"], args: "", flags: &[],
        what: "Command line: to the start.", example: "rl-beginning-of-line" },
    Doc { names: &["rl-end-of-line"], args: "", flags: &[],
        what: "Command line: to the end.", example: "rl-end-of-line" },
    Doc { names: &["rl-unix-line-discard"], args: "", flags: &[],
        what: "Command line: delete back to the start.", example: "rl-unix-line-discard" },
    Doc { names: &["rl-kill-line"], args: "", flags: &[],
        what: "Command line: delete forward to the end.", example: "rl-kill-line" },
    Doc { names: &["rl-rubout"], args: "<delimiters>", flags: &[],
        what: "Command line: delete back to one of the characters given.", example: "rl-rubout \" \"" },
    Doc { names: &["rl-filename-rubout"], args: "", flags: &[],
        what: "Command line: delete back one path segment.", example: "rl-filename-rubout" },
    Doc { names: &["rl-unix-word-rubout"], args: "", flags: &[],
        what: "Command line: delete back one whitespace-separated word.",
        example: "rl-unix-word-rubout" },
    Doc { names: &["rl-unix-filename-rubout"], args: "", flags: &[],
        what: "Command line: delete back one path or word.", example: "rl-unix-filename-rubout" },
    Doc { names: &["rl-backward-kill-word"], args: "", flags: &[],
        what: "Command line: delete the word before the cursor.", example: "rl-backward-kill-word" },
    Doc { names: &["rl-kill-word"], args: "", flags: &[],
        what: "Command line: delete the word after the cursor.", example: "rl-kill-word" },
    Doc { names: &["rl-yank"], args: "", flags: &[],
        what: "Command line: put back what was last deleted.", example: "rl-yank" },
    Doc { names: &["rl-delete-char"], args: "", flags: &[],
        what: "Command line: delete the character under the cursor.", example: "rl-delete-char" },
    Doc { names: &["rl-backward-delete-char"], args: "", flags: &[],
        what: "Command line: delete the character before the cursor.",
        example: "rl-backward-delete-char" },
];

/// The signature a row prints: the flags and then the arguments, as they would be typed.
///
/// The name is not in it — the name is the first column, and repeating it would push the
/// arguments off the fold on the rows where there are five flags.
fn signature(doc: &Doc) -> String {
    let mut out = String::new();
    for flag in doc.flags {
        out.push_str(&format!("[{flag}] "));
    }
    out.push_str(doc.args);
    out.trim_end().to_string()
}

/// The commands a binding's text actually names, in order.
///
/// A binding is not one command: `<Escape>` is `clear-keychain ;; search ;; fullscreen --leave`
/// and names three, and `:later 1s reload` names two. Both shapes are walked, because a key column
/// that only saw the first word would say `<Escape>` does not reach `search` — which is the one
/// thing that binding is famous for.
///
/// `bind` is deliberately **not** walked into: `bind j scroll down` runs `bind`, and counting it as
/// a key that scrolls would be counting the wrong thing.
fn commands_named_in(text: &str, out: &mut Vec<String>) {
    /// The three commands whose last argument is another command, and how many words come first.
    const CARRIERS: [(&str, usize); 6] = [
        ("later", 1),
        ("cmd-later", 1),
        ("repeat", 1),
        ("cmd-repeat", 1),
        ("run-with-count", 1),
        ("cmd-run-with-count", 1),
    ];
    for part in split_on_chain(text) {
        let mut words = part.split_whitespace();
        let Some(name) = words.next() else { continue };
        out.push(name.to_string());
        if let Some((_, skip)) = CARRIERS.iter().find(|(carrier, _)| *carrier == name) {
            let rest: Vec<&str> = part.split_whitespace().skip(1 + skip).collect();
            if !rest.is_empty() {
                commands_named_in(&rest.join(" "), out);
            }
        }
    }
}

/// Split a binding's text on `;;`, ignoring separators inside quotes.
///
/// The same rule `commands::split_chain` follows, written again here rather than made public:
/// this one may only ever be read from, and a `pub` on the parser's own splitter would be an
/// invitation to route a second caller through it.
fn split_on_chain(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                current.push(c);
                if c == q {
                    quote = None;
                }
            }
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                current.push(c);
            }
            None if c == ';' && chars.peek() == Some(&';') => {
                chars.next();
                parts.push(std::mem::take(&mut current).trim().to_string());
            }
            None => current.push(c),
        }
    }
    parts.push(current.trim().to_string());
    parts.retain(|p| !p.is_empty());
    parts
}

/// Which keys reach a command: the ones that **run** it, and the ones that only **type** it.
///
/// The second list is the one that took a decision. `o` is `cmd-set-text -s :open`: it runs
/// `cmd-set-text` and puts `:open` in the command line for you to finish. Listing `o` against
/// `open` with no distinction would be false, and listing it against nothing would leave the most
/// used key in the browser looking unbound on the row a reader most wants it on. So both, and the
/// page says which is which.
#[derive(Default)]
struct Reached {
    runs: Vec<String>,
    types: Vec<String>,
}

/// Every command named by every binding, with the keys that name it.
fn reached(bindings: &Bindings) -> BTreeMap<String, Reached> {
    let mut out: BTreeMap<String, Reached> = BTreeMap::new();
    for (mode, keys, text) in bindings.all() {
        let label = if mode == Mode::Normal {
            keys.clone()
        } else {
            format!("{keys} ({})", mode.name())
        };
        let mut named = Vec::new();
        commands_named_in(&text, &mut named);
        for name in &named {
            let entry = out.entry(name.clone()).or_default();
            if !entry.runs.contains(&label) {
                entry.runs.push(label.clone());
            }
        }
        // The command line's own text, for the keys that prefill rather than run.
        if named.iter().any(|name| name == "cmd-set-text") {
            for part in split_on_chain(&text) {
                if part.split_whitespace().next() != Some("cmd-set-text") {
                    continue;
                }
                let typed = part
                    .split_whitespace()
                    .skip(1)
                    .find_map(|word| word.strip_prefix(':'))
                    .unwrap_or_default();
                if typed.is_empty() {
                    continue;
                }
                let entry = out.entry(typed.to_string()).or_default();
                if !entry.types.contains(&label) {
                    entry.types.push(label.clone());
                }
            }
        }
    }
    out
}

/// The page, as HTML. Styled from `chrome.css` and the theme, like the strips.
pub fn page(bindings: &Bindings) -> String {
    let rows = bindings.all();

    // No total: the summary counts what acts and what is refused, and does not divide one by the
    // other or by anything else.
    let (live, refused) = rows.iter().fold((0usize, 0usize), |(live, refused), (_, _, cmd)| {
        match State::of(cmd) {
            State::Live => (live + 1, refused),
            State::Refused(_) => (live, refused + 1),
            State::NotYet => (live, refused),
        }
    });

    let mut out = String::with_capacity(64 * 1024);
    out.push_str(
        r#"<!doctype html>
<meta charset="utf-8">
<title>bru — keys and commands</title>
<link rel="stylesheet" href="chrome.css">
<link rel="stylesheet" href="theme.css">
<body data-view="help">
<main id="help">
"#,
    );

    // **A count, not a fraction, and no machinery.** This said "251 of 264 bindings do something
    // today. The rest are bound and parsed so that a chain like `gg` still works…" — a sentence
    // that explained bru's trie to someone who opened the page to find out what a key does. Both
    // halves were wrong for the reader: the "x of y" reads as a score against somebody else's
    // total, and *why* a dead key stays in the table is an implementation detail. It is a real
    // reason and it belongs where it is acted on — dropping those rows would make `t` answer
    // NoMatch and eat a pending chain — but it belongs in `config.rs`, not here.
    //
    // What is left is the two numbers and the promise that each dead key says why.
    //
    // **The commands half adds two more counts and no fraction.** "160 commands, 107 of them bound"
    // is the same "x of y" that was taken out of the first sentence by hand this morning: it reads
    // as a score, and the number it scores against is not one a reader has any use for. Two
    // sentences of one count each say the same thing without inviting the division.
    let by_key = reached(bindings);
    let bound = COMMANDS
        .iter()
        .filter(|doc| doc.names.iter().any(|name| by_key.contains_key(*name)))
        .count();
    out.push_str(&format!(
        "<h1>bru</h1>\n<p class=\"summary\">{live} keys do something. \
         {refused} say why they do not. {} commands can be typed. {bound} answer to a key.</p>\n",
        COMMANDS.len(),
    ));

    for mode in Mode::ALL {
        let in_mode: Vec<_> = rows.iter().filter(|(m, _, _)| *m == mode).collect();
        if in_mode.is_empty() {
            continue;
        }
        out.push_str(&format!("<h2>{}</h2>\n<table>\n", escape(mode.name())));
        for (_, keys, cmd) in in_mode {
            let state = State::of(cmd);
            // The reason goes inside the command cell rather than into a row of its own: one `<tr>`
            // per binding is what `every_binding_appears` counts, and what keeps the striping from
            // pairing a row with its own explanation.
            let why = match state {
                State::Refused(why) => format!("<span class=\"why\">{}</span>", escape(why)),
                _ => String::new(),
            };
            out.push_str(&format!(
                "<tr class=\"{}\"><td class=\"keys\">{}</td><td class=\"cmd\">{}{why}</td><td class=\"state\">{}</td></tr>\n",
                state.class(),
                escape(keys),
                escape(cmd),
                state.label(),
            ));
        }
        out.push_str("</table>\n");
    }

    // **The commands, and the column that joins them to the keys above.**
    //
    // The rows carry `data-row="command"` and the key rows do not, which is not decoration: the
    // key table's own test counts `<tr class=` and would have counted every command as a binding
    // the moment this section landed. One attribute keeps the two countable apart, and keeps that
    // test measuring what it was written to measure.
    out.push_str(
        "<h2>commands</h2>\n<p class=\"note\">A key on the right <em>runs</em> the command. \
         A dimmer one only <em>types</em> it: it opens the command line with the command already \
         in it, for you to finish.</p>\n<table>\n",
    );
    for doc in COMMANDS {
        let state = State::of(doc.example);
        let why = match state {
            State::Refused(why) => format!("<span class=\"why\">{}</span>", escape(why)),
            _ => String::new(),
        };
        let aliases = doc.names[1..]
            .iter()
            .map(|alias| format!("<span class=\"alias\">{}</span>", escape(alias)))
            .collect::<String>();
        let reached: Reached = doc.names.iter().fold(Reached::default(), |mut all, name| {
            if let Some(found) = by_key.get(*name) {
                all.runs.extend(found.runs.iter().cloned());
                all.types.extend(found.types.iter().cloned());
            }
            all
        });
        let mut keys = reached.runs.iter().map(|key| escape(key)).collect::<Vec<_>>().join(" ");
        if !reached.types.is_empty() {
            if !keys.is_empty() {
                keys.push(' ');
            }
            keys.push_str(&format!(
                "<span class=\"typed\">{}</span>",
                reached.types.iter().map(|key| escape(key)).collect::<Vec<_>>().join(" ")
            ));
        }
        out.push_str(&format!(
            "<tr data-row=\"command\" class=\"{}\"><td class=\"name\">{}{aliases}</td>\
             <td class=\"cmd\"><code class=\"sig\">{}</code><span class=\"what\">{}</span>{why}</td>\
             <td class=\"bound\">{keys}</td><td class=\"state\">{}</td></tr>\n",
            state.class(),
            escape(doc.names[0]),
            escape(&signature(doc)),
            escape(doc.what),
            state.label(),
        ));
    }
    out.push_str("</table>\n");

    out.push_str("</main>\n");
    out
}

/// The page is built from command strings, which come from `config.lua` — the user's own file, but
/// still a file, and one typo away from putting a `<` where markup begins.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use crate::commands::Command;

    use super::*;

    fn bindings() -> Bindings {
        crate::config::Config::load_from(None).bindings
    }

    /// **The page never names qutebrowser.** Asked for by the user 2026-08-07: bru's own help is
    /// not the place to measure bru against anything — it states bru's numbers and bru's reasons.
    ///
    /// The refusals are where this went wrong. They were argued by comparison — "qutebrowser drives
    /// it through QWebEngineSettings::PluginsEnabled", "the key is dead in qutebrowser 3.7.0 too" —
    /// which is right in a commit message and wrong on a page someone opens to find out what a key
    /// does. The measurements stayed; the comparisons went.
    ///
    /// Comments and commit messages are not covered and should not be: `.claude/` and the source
    /// are where the port is documented, and naming what a behaviour was ported from is how the
    /// next person checks it.
    #[test]
    fn the_page_measures_bru_against_nothing() {
        let html = page(&bindings());
        let text = html.to_lowercase();
        assert!(
            !text.contains("qutebrowser"),
            "the help page names qutebrowser; it states bru's own numbers and reasons only"
        );
        // `qute://` and `qutebrowser`'s own file names are the same mistake wearing a shorter word.
        for needle in ["qute://", "configdata.yml", "qwebengine"] {
            assert!(!text.contains(needle), "the help page still points at {needle:?}");
        }
    }

    #[test]
    fn every_binding_appears() {
        let b = bindings();
        let html = page(&b);
        let rows = html.matches("<tr class=").count();
        assert_eq!(rows, b.all().len(), "one row per binding, no more and no less");
        assert!(rows > 200, "the qutebrowser defaults are there: {rows} rows");
    }

    #[test]
    fn the_page_says_which_keys_work() {
        let html = page(&bindings());
        // `j` scrolls today, and so does `yy` since src/clip.rs; `q` records a macro since
        // src/macros.rs.
        assert!(html.contains(r#"<tr class="live"><td class="keys">j</td><td class="cmd">scroll down</td>"#));
        assert!(html.contains(r#"<tr class="live"><td class="keys">yy</td><td class="cmd">yank</td>"#));
        assert!(html.contains(r#"<tr class="live"><td class="keys">q</td><td class="cmd">macro-record</td>"#));

        // **No default binding says "not yet" any more.** Every one of qutebrowser's 264 either
        // acts or is refused, and this is where that stops being a claim: the filter is `is_live`
        // and not "does it parse to a variant" — `command-history-prev` and every `rl-*` parse to
        // `Unimplemented` and are live all the same, because they reach `cmdline.rs` by name.
        // Asking the parser here marked them "not yet" and is the same mistake that undercounted
        // the live bindings by 17 for a whole stage.
        let waiting: Vec<&str> = crate::config::DEFAULT_BINDINGS
            .iter()
            .map(|(_mode, _keys, text)| *text)
            .filter(|text| State::of(text) == State::NotYet)
            .collect();
        assert!(waiting.is_empty(), "these still say \"not yet\": {waiting:?}");
        assert!(!html.contains(">not yet<"), "and so the page must not print the words");

        // The state is still reachable, and still rendered — a `config.lua` may name a command
        // qutebrowser has and bru has not built. Without this the branch would be untested the
        // moment the defaults stopped using it.
        //
        // It was `click-element id foo` until `src/utilcmds.rs` implemented that command. The
        // stand-in is a `debug-*` command on purpose: qutebrowser's debug commands are the one
        // group bru has decided not to port at all, so this example cannot go live under someone
        // else's milestone the way the last one did.
        let mut b = bindings();
        b.bind("normal", "ZW", "debug-dump-page /tmp/x").expect("a valid binding");
        let html = page(&b);
        assert_eq!(State::of("debug-dump-page /tmp/x"), State::NotYet);
        assert!(html.contains(
            r#"<tr class="todo"><td class="keys">ZW</td><td class="cmd">debug-dump-page /tmp/x</td><td class="state">not yet</td></tr>"#
        ));
    }

    /// The third state, and the thirteen rows that are in it.
    ///
    /// "not yet" against a key nothing can ever implement invites the same investigation every few
    /// months; three of them have been paid for already. These rows say **refused** and carry the
    /// reason the module that measured it wrote.
    #[test]
    fn a_binding_nothing_can_implement_says_refused_and_why() {
        let html = page(&bindings());

        // The twelve `t**` rows: six `content.plugins`, six `content.cookies.accept`.
        let twelve: Vec<&str> = crate::config::DEFAULT_BINDINGS
            .iter()
            .filter(|(mode, keys, _)| *mode == "normal" && keys.starts_with('t') && keys.len() == 3)
            .filter(|(_, _, cmd)| {
                cmd.contains("content.plugins") || cmd.contains("content.cookies.accept")
            })
            .map(|(_, keys, _)| *keys)
            .collect();
        assert_eq!(twelve.len(), 12, "the t** rows moved: {twelve:?}");
        for keys in &twelve {
            let row = format!(r#"<td class="keys">{keys}</td>"#);
            let at = html.find(&row).unwrap_or_else(|| panic!("no row for {keys}"));
            let row = &html[..at];
            assert!(row.ends_with(r#"<tr class="refused">"#), "{keys} is not marked refused");
        }
        assert!(html.contains("NPAPI and PPAPI are gone"), "the plugins reason is not on the page");
        assert!(html.contains("no-3rdparty cannot be written per URL"), "the cookies reason is not");

        // And `<Return>` in hint mode, whose reason is `hints.rs`'s.
        assert!(html.contains(r#"<tr class="refused"><td class="keys">&lt;Return&gt;</td><td class="cmd">hint-follow"#));
        assert!(html.contains("there is never a hint waiting to be followed"));

        // Thirteen rows, and the summary says so rather than only the table.
        assert_eq!(html.matches(r#"<tr class="refused">"#).count(), 13);
        assert!(html.contains("13 say why they do not"), "the summary undercounts");
        // And it states counts rather than a fraction of anything — the user asked for that
        // 2026-08-07, and a helpful-looking "x of y" is exactly what crept back last time.
        assert!(!html.contains(" of 264"), "the summary is measuring against a total again");

        // A refused row is not a "not yet" row, or the third state is decoration.
        assert!(!html.contains(r#"<td class="cmd">hint-follow</td><td class="state">not yet</td>"#));

// --- src/utilcmds.rs -------------------------------------------------------
        // A command carried by `:later`, `:repeat` or `:run-with-count` keeps its own state, which
        // for a refused one is the reason and not "not yet". Only reachable from a `config.lua`;
        // the default table binds none of the three.
        assert!(matches!(
            State::of("later 1s config-cycle -p -u *://x/* content.plugins"),
            State::Refused(_)
        ));
        assert_eq!(State::of("repeat 2 scroll down"), State::Live);
        assert_eq!(State::of("repeat 2 debug-dump-page /tmp/x"), State::NotYet);
// --- end src/utilcmds.rs ---------------------------------------------------

        // The twelve are chains — `config-cycle … ;; reload` — and the reason must come from the
        // half that is refused whichever half is written first. Asking `exec::refusal` for the
        // reversed spelling is what proves it does not depend on the order, which it did until a
        // deliberate break walked it past the `config-cycle` and into `reload`.
        let forwards = "config-cycle -p -u *://x/* content.plugins ;; reload";
        let backwards = "reload ;; config-cycle -p -u *://x/* content.plugins";
        assert!(matches!(State::of(forwards), State::Refused(_)));
        assert_eq!(State::of(forwards), State::of(backwards));
    }

    /// A reason is prose written by a person and lands inside a table cell. It goes through the
    /// same escape as everything else, and the page carries the escaped form.
    #[test]
    fn a_refusal_reason_cannot_escape_its_cell() {
        let b = bindings();
        let html = page(&b);
// --- tabs and statusbar ----------------------------------------------------
        // Only the refusals a binding names reach this page, and that is what the page is: one row
        // per binding, with the reason inside the row. `REFUSED` used to hold exactly the two the
        // twelve `t**` bindings name, so walking all of it was walking those two; it now also holds
        // `tabs.*` and `statusbar.*` names, which no default binding types, and those are printed by
        // `bru://chrome/settings` instead — `settingspage.rs::every_refused_reason_is_escaped` is
        // this same assertion over the page that does show them.
        //
        // Which ones those are is asked of the bindings rather than listed here, so that a binding
        // added or taken away moves this check with it instead of leaving a hard-coded two.
        let commands = b.all();
        let named: Vec<&(&str, &str)> = crate::settings::REFUSED
            .iter()
            .filter(|(name, _)| commands.iter().any(|(_, _, cmd)| cmd.contains(name)))
            .collect();
        assert!(
            named.len() >= 2,
            "the two content settings the t** bindings name must still reach the page"
        );
        for (_, why) in named {
// --- end tabs and statusbar ------------------------------------------------
            assert!(html.contains(&escape(why)), "the escaped reason is not on the page");
            // The plugins reason contains ASCII quotes, so this is not a vacuous check: the raw
            // string must *not* be there.
            if escape(why) != *why {
                assert!(!html.contains(*why), "an unescaped reason reached the page");
            }
        }
        assert_ne!(
            escape(crate::settings::REFUSED[0].1),
            crate::settings::REFUSED[0].1,
            "if no reason needs escaping this test asserts nothing"
        );
        assert_eq!(escape("a <b> & \"c\""), "a &lt;b&gt; &amp; &quot;c&quot;");
    }

    /// A `config.lua` that rebinds a key must change the page, or it is documentation of something
    /// other than this browser.
    #[test]
    fn it_describes_the_users_bindings_and_not_qutebrowsers() {
        let mut b = bindings();
        b.bind("normal", "ZX", "scroll down").expect("a valid binding");
        let html = page(&b);
        assert!(html.contains("ZX"), "a rebound key has to show up");
    }

    // -- the commands half -----------------------------------------------------------------------

    /// One token of Rust source: a string literal, or a single character, each carrying the bracket
    /// depth it sits at and where it starts.
    ///
    /// Depth is the whole trick. `parse_one`'s command match holds 463 string literals, 317 of them
    /// distinct, and only 146 are command names; `"down"`, `"links"` and `"pretty-url"` are argument
    /// values, and every one of them sits inside an arm's body — depth 1 or more — while an arm
    /// *head* of the one match that dispatches on the command name sits at depth 0 of that match's
    /// own block. Scraping by regex would list `up` as a command; taking the `0` out of the pattern
    /// below was measured and listed 35 of them.
    enum Tok {
        Lit(String),
        Ch(char),
    }

    /// Scan a slice of Rust source into tokens, with a masked copy in which every line comment has
    /// become spaces.
    ///
    /// Line comments go because a doc comment naming `has("force")` would otherwise be read as a
    /// flag the parser reads. Char literals are skipped whole so that `'%'` — and, one day, `'{'` —
    /// cannot move the depth; a lifetime has no closing quote two characters along and falls
    /// through to the ordinary path.
    ///
    /// **Every offset here is a character index, not a byte offset**, and the masked copy is left
    /// as characters for the same reason. Byte offsets were the first spelling and were wrong:
    /// masking a comment replaces its em-dashes — three bytes each, and `commands.rs`'s comments
    /// are full of them — with one-byte spaces, so every offset after the first comment pointed at
    /// the wrong place and three arms' bodies came back empty.
    fn scan(src: &str) -> (Vec<(i32, Tok, usize)>, Vec<char>) {
        let chars: Vec<char> = src.chars().collect();
        let mut masked = chars.clone();
        let mut out = Vec::new();
        let mut depth = 0i32;
        let mut i = 0usize;
        while i < chars.len() {
            let c = chars[i];
            if c == '/' && chars.get(i + 1) == Some(&'/') {
                while i < chars.len() && chars[i] != '\n' {
                    masked[i] = ' ';
                    i += 1;
                }
                continue;
            }
            if c == '\'' && chars.get(i + 2) == Some(&'\'') {
                i += 3;
                continue;
            }
            if c == '"' {
                let mut text = String::new();
                let mut j = i + 1;
                while j < chars.len() && chars[j] != '"' {
                    if chars[j] == '\\' {
                        j += 1;
                    }
                    if j < chars.len() {
                        text.push(chars[j]);
                    }
                    j += 1;
                }
                out.push((depth, Tok::Lit(text), i));
                i = j + 1;
                continue;
            }
            match c {
                '{' | '(' | '[' => depth += 1,
                '}' | ')' | ']' => depth -= 1,
                _ => {}
            }
            out.push((depth, Tok::Ch(c), i));
            i += 1;
        }
        (out, masked)
    }

    /// The arm heads of a `match` at depth 0, each with the byte offset the head starts at and the
    /// one its body starts at.
    ///
    /// An arm head is one or more string literals joined by `|` and followed by `=>`, which is
    /// exactly how `parse_one` writes `"edit-text" | "open-editor" =>`.
    fn arms(tokens: &[(i32, Tok, usize)]) -> Vec<(Vec<String>, usize, usize)> {
        let non_space = |mut k: usize| {
            while let Some((_, Tok::Ch(c), _)) = tokens.get(k) {
                if !c.is_whitespace() {
                    break;
                }
                k += 1;
            }
            k
        };
        let mut out = Vec::new();
        let mut k = 0usize;
        while k < tokens.len() {
            let (0, Tok::Lit(first), head_at) = &tokens[k] else {
                k += 1;
                continue;
            };
            let head_at = *head_at;
            let mut names = vec![first.clone()];
            let mut m = non_space(k + 1);
            while matches!(tokens.get(m), Some((_, Tok::Ch('|'), _))) {
                m = non_space(m + 1);
                match tokens.get(m) {
                    Some((0, Tok::Lit(next), _)) => {
                        names.push(next.clone());
                        m = non_space(m + 1);
                    }
                    _ => break,
                }
            }
            let arrow = matches!(tokens.get(m), Some((_, Tok::Ch('='), _)))
                && matches!(tokens.get(m + 1), Some((_, Tok::Ch('>'), _)));
            if arrow {
                out.push((names, head_at, tokens[m + 1].2 + 1));
                k = m + 2;
                continue;
            }
            k += 1;
        }
        out
    }

    /// Every string literal in a slice of source, empty ones dropped.
    ///
    /// `""` is dropped because it is never a command name — `parse_one` rejects an empty command —
    /// and `parse`'s body ends in `unwrap_or("")`, which would otherwise arrive here as a command
    /// bru understands and nothing documents.
    fn literals_in(src: &str) -> Vec<String> {
        scan(src)
            .0
            .into_iter()
            .filter_map(|(_, tok, _)| match tok {
                Tok::Lit(text) if !text.is_empty() => Some(text),
                _ => None,
            })
            .collect()
    }

    /// The region of `src/commands.rs` that dispatches on a command name, and the region of
    /// `pub fn parse` that runs before it.
    ///
    /// Two regions because there are two places: `parse` short-circuits `:bind` to its own parser
    /// before the match ever runs, so `bind` is a command name that is not an arm head. Reading
    /// `parse`'s whole body rather than looking for the word means a *second* short-circuit lands
    /// in the scrape instead of quietly outside it.
    fn parser_regions() -> (String, String) {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let src = std::fs::read_to_string(root.join("src/commands.rs"))
            .expect("src/commands.rs is readable");
        let open = "let cmd = match name.as_str() {";
        let start = src.find(open).expect("parse_one still dispatches on the command name") + open.len();
        let end = src[start..]
            .find("_ => Command::Unimplemented(s.trim().to_string()),")
            .expect("the match still ends in the catch-all")
            + start;
        let head = "pub fn parse(s: &str) -> Result<Command, ParseError> {";
        let head_at = src.find(head).expect("parse is still spelled this way") + head.len();
        let head_end = src[head_at..].find("\nfn split_chain").expect("split_chain still follows parse")
            + head_at;
        (src[start..end].to_string(), src[head_at..head_end].to_string())
    }

    /// Every command name the source writes down, from the three places it writes them.
    fn names_the_parser_accepts() -> BTreeSet<String> {
        let (dispatch, before) = parser_regions();
        let (tokens, _) = scan(&dispatch);
        let mut out: BTreeSet<String> =
            arms(&tokens).into_iter().flat_map(|(names, _, _)| names).collect();
        out.extend(literals_in(&before));

        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let cmdline = std::fs::read_to_string(root.join("src/cmdline.rs"))
            .expect("src/cmdline.rs is readable");
        let head = "fn is_named(text: &str) -> bool {";
        let at = head.len() + cmdline.find(head).expect("cmdline still names its commands here");
        let end = cmdline[at..].find("\n}\n").expect("is_named still ends") + at;
        out.extend(literals_in(&cmdline[at..end]));
        out
    }

    /// **The guard that makes the list a list of this browser's commands.** It reads the two files
    /// that decide what a command name is and asserts set equality with [`COMMANDS`], in both
    /// directions: a command added to the parser and not written down here fails, and a name here
    /// the parser has never heard of fails too.
    ///
    /// It does not survive a rename — renaming `screenshot` in the parser and here together passes,
    /// and so it should, but so does renaming it in both while every other mention of it stays. The
    /// colour test in `src/chrome.rs` says the same thing about itself, and it is the honest limit
    /// of reading source as text.
    #[test]
    fn every_command_bru_understands_has_a_row() {
        let source = names_the_parser_accepts();
        let written: BTreeSet<String> =
            COMMANDS.iter().flat_map(|doc| doc.names.iter().map(|n| n.to_string())).collect();

        let missing: Vec<&String> = source.difference(&written).collect();
        assert!(missing.is_empty(), "the parser knows these and the page does not: {missing:?}");
        let invented: Vec<&String> = written.difference(&source).collect();
        assert!(invented.is_empty(), "the page names these and the parser does not: {invented:?}");

        // A number, so that a scrape which silently starts finding nothing is a failure rather than
        // a vacuous pass. It went from 146 arm heads plus `bind` plus cmdline's nineteen.
        assert_eq!(source.len(), 166, "the scrape found a different number of commands");
        // And the depth rule did its job: these are argument values written as literals inside an
        // arm body, and a regex over the same file would have listed all four as commands.
        for value in ["up", "links", "pretty-url", "next-category"] {
            assert!(!source.contains(value), "{value:?} is an argument, not a command");
        }
    }

    /// Every name is spelled the way it is typed. A leading `:` or a space would parse into
    /// something else entirely and read as a command on the page.
    #[test]
    fn a_row_names_a_command_and_not_a_command_line() {
        for doc in COMMANDS {
            for name in doc.names {
                assert!(!name.is_empty(), "an empty name");
                assert!(!name.starts_with(':'), "{name}: the colon is the command line's, not the name's");
                assert!(!name.contains(' '), "{name}: a name is one word");
            }
            assert_eq!(
                doc.example.split_whitespace().next(),
                Some(doc.names[0]),
                "{}: the example does not start with the name", doc.names[0]
            );
            assert!(!doc.what.is_empty(), "{}: no description", doc.names[0]);
            assert!(doc.what.ends_with('.'), "{}: the description is a sentence", doc.names[0]);
        }
    }

    /// **Every row is asked the same two questions the key table asks**, so the halves of the page
    /// cannot disagree with each other about a command.
    ///
    /// And no row says "not yet": every command bru's parser accepts either acts or says why it
    /// never will. That is a claim worth failing on rather than a coincidence — the moment a
    /// command is added that parses and does nothing, this is where it is noticed.
    #[test]
    fn every_row_parses_and_either_acts_or_says_why_not() {
        for doc in COMMANDS {
            let parsed = commands::parse(doc.example);
            assert!(parsed.is_ok(), "{}: {:?} does not parse: {parsed:?}", doc.names[0], doc.example);
            match State::of(doc.example) {
                State::Live => {}
                State::Refused(why) => assert!(!why.is_empty(), "{}: refused with no reason", doc.names[0]),
                State::NotYet => panic!("{}: neither acts nor says why not", doc.names[0]),
            }
        }
        // `hint-follow` is the one, and it carries `hints.rs`'s own words.
        assert_eq!(
            COMMANDS
                .iter()
                .filter(|doc| matches!(State::of(doc.example), State::Refused(_)))
                .map(|doc| doc.names[0])
                .collect::<Vec<_>>(),
            vec!["hint-follow"]
        );
    }

    /// **The `Command` enum, covered with no `_` arm** — the trick `exec::run` uses, aimed at this
    /// list instead of at the dispatcher.
    ///
    /// A variant added to `commands.rs` does not compile until it is named below, and does not pass
    /// until parsing something [`COMMANDS`] names produces it. That is the half the source scrape
    /// cannot do: a new *shape* of an existing command — `download --mhtml` is one — adds no arm
    /// head to find and would otherwise be documented by nobody.
    ///
    /// `Chain` and `Unimplemented` are the two exemptions and they are not commands: `;;` is
    /// punctuation, and `Unimplemented` is what the match's catch-all builds out of a name it does
    /// not know.
    #[test]
    fn every_command_variant_is_reachable_by_name() {
        fn variant(command: &Command) -> &'static str {
            match command {
                Command::Chain(_) => "Chain",
                Command::Nop => "Nop",
                Command::ClearKeychain => "ClearKeychain",
                Command::ModeEnter(_) => "ModeEnter",
                Command::ModeLeave => "ModeLeave",
                Command::Scroll(_) => "Scroll",
                Command::ScrollPx { .. } => "ScrollPx",
                Command::ScrollPage { .. } => "ScrollPage",
                Command::ScrollToPerc { .. } => "ScrollToPerc",
                Command::TabNext => "TabNext",
                Command::TabPrev => "TabPrev",
                Command::TabClose { .. } => "TabClose",
                Command::TabOnly { .. } => "TabOnly",
                Command::TabFocus { .. } => "TabFocus",
                Command::TabMove { .. } => "TabMove",
                Command::TabClone { .. } => "TabClone",
                Command::Undo { .. } => "Undo",
                Command::TabPin => "TabPin",
                Command::TabMute => "TabMute",
                Command::TabGive { .. } => "TabGive",
                Command::SessionSave { .. } => "SessionSave",
                Command::SessionLoad { .. } => "SessionLoad",
                Command::SessionDelete { .. } => "SessionDelete",
                Command::Open { .. } => "Open",
                Command::Back { .. } => "Back",
                Command::Forward { .. } => "Forward",
                Command::Reload { .. } => "Reload",
                Command::Stop => "Stop",
                Command::Home => "Home",
                Command::Quit { .. } => "Quit",
                Command::Close => "Close",
                Command::Zoom { .. } => "Zoom",
                Command::ZoomIn => "ZoomIn",
                Command::ZoomOut => "ZoomOut",
                Command::Fullscreen { .. } => "Fullscreen",
                Command::Hint { .. } => "Hint",
                Command::HintFollow => "HintFollow",
                Command::Help { .. } => "Help",
                Command::Download { .. } => "Download",
                Command::DownloadMhtml => "DownloadMhtml",
                Command::DownloadCancel { .. } => "DownloadCancel",
                Command::DownloadClear => "DownloadClear",
                Command::DownloadOpen { .. } => "DownloadOpen",
                Command::DownloadDelete => "DownloadDelete",
                Command::DownloadRetry => "DownloadRetry",
                Command::QuickmarkSave { .. } => "QuickmarkSave",
                Command::QuickmarkLoad { .. } => "QuickmarkLoad",
                Command::QuickmarkDel { .. } => "QuickmarkDel",
                Command::BookmarkAdd { .. } => "BookmarkAdd",
                Command::BookmarkLoad { .. } => "BookmarkLoad",
                Command::BookmarkDel { .. } => "BookmarkDel",
                Command::BookmarkList { .. } => "BookmarkList",
                Command::History { .. } => "History",
                Command::Cookies { .. } => "Cookies",
                Command::Yank { .. } => "Yank",
                Command::Search { .. } => "Search",
                Command::SearchNext => "SearchNext",
                Command::SearchPrev => "SearchPrev",
                Command::Navigate { .. } => "Navigate",
                Command::SelectionToggle { .. } => "SelectionToggle",
                Command::SelectionDrop => "SelectionDrop",
                Command::SelectionReverse => "SelectionReverse",
                Command::SelectionFollow { .. } => "SelectionFollow",
                Command::MoveTo(_) => "MoveTo",
                Command::CmdSetText { .. } => "CmdSetText",
                Command::CommandAccept { .. } => "CommandAccept",
                Command::Spawn { .. } => "Spawn",
                Command::EditText => "EditText",
                Command::InsertText { .. } => "InsertText",
                Command::FakeKey { .. } => "FakeKey",
                Command::Set { .. } => "Set",
                Command::ConfigCycle { .. } => "ConfigCycle",
                Command::ConfigDictAdd { .. } => "ConfigDictAdd",
                Command::ConfigDictRemove { .. } => "ConfigDictRemove",
                Command::ConfigUnset { .. } => "ConfigUnset",
                Command::ConfigClear { .. } => "ConfigClear",
                Command::ConfigDiff => "ConfigDiff",
                Command::ConfigListAdd { .. } => "ConfigListAdd",
                Command::ConfigListRemove { .. } => "ConfigListRemove",
                Command::ConfigSource { .. } => "ConfigSource",
                Command::ConfigEdit { .. } => "ConfigEdit",
                Command::ConfigWritePy => "ConfigWritePy",
                Command::Bind { .. } => "Bind",
                Command::Unbind { .. } => "Unbind",
                Command::CompletionItemFocus { .. } => "CompletionItemFocus",
                Command::CompletionItemDel => "CompletionItemDel",
                Command::CompletionItemYank { .. } => "CompletionItemYank",
                Command::PromptAccept { .. } => "PromptAccept",
                Command::PromptItemFocus { .. } => "PromptItemFocus",
                Command::PromptOpenDownload { .. } => "PromptOpenDownload",
                Command::PromptYank { .. } => "PromptYank",
                Command::PromptFileselectExternal => "PromptFileselectExternal",
                Command::AdblockUpdate => "AdblockUpdate",
                Command::AdblockToggle => "AdblockToggle",
                Command::AdblockInfo => "AdblockInfo",
                Command::GreasemonkeyReload { .. } => "GreasemonkeyReload",
                Command::ViewSource => "ViewSource",
                Command::Print => "Print",
                Command::DevTools => "DevTools",
                Command::DevToolsFocus => "DevToolsFocus",
                Command::Message { .. } => "Message",
                Command::MacroRecord { .. } => "MacroRecord",
                Command::MacroRun { .. } => "MacroRun",
                Command::Save { .. } => "Save",
                Command::CmdRepeatLast => "CmdRepeatLast",
                Command::SettingsPage => "SettingsPage",
                Command::TabSelect { .. } => "TabSelect",
                Command::TabTake { .. } => "TabTake",
                Command::WindowOnly => "WindowOnly",
                Command::Screenshot { .. } => "Screenshot",
                Command::JsEval { .. } => "JsEval",
                Command::EditUrl { .. } => "EditUrl",
                Command::EditCommand { .. } => "EditCommand",
                Command::QuickmarkAdd { .. } => "QuickmarkAdd",
                Command::HistoryClear { .. } => "HistoryClear",
                Command::Later { .. } => "Later",
                Command::Repeat { .. } => "Repeat",
                Command::RunWithCount { .. } => "RunWithCount",
                Command::Restart => "Restart",
                Command::Version => "Version",
                Command::Messages { .. } => "Messages",
                Command::Process { .. } => "Process",
                Command::ClickElement { .. } => "ClickElement",
                Command::ScrollToAnchor { .. } => "ScrollToAnchor",
                Command::DownloadRemove { .. } => "DownloadRemove",
                Command::ClearMessages => "ClearMessages",
                Command::MarksReload { .. } => "MarksReload",
                Command::Unimplemented(_) => "Unimplemented",
            }
        }

        // Everything a row can be parsed into: the name alone, the example, and the example with
        // each flag added. The third is what reaches `DownloadMhtml`, which is `:download` wearing
        // `-m` and has no arm head of its own to find. A spelling that does not parse is skipped —
        // `--rect` with no value is an error on purpose.
        let mut reached: BTreeSet<&'static str> = BTreeSet::new();
        for doc in COMMANDS {
            let mut tries: Vec<String> = doc.names.iter().map(|n| n.to_string()).collect();
            tries.push(doc.example.to_string());
            for flag in doc.flags {
                let first = flag.split('/').next().unwrap_or(flag);
                tries.push(format!("{} {first}", doc.example));
            }
            for try_ in tries {
                if let Ok(command) = commands::parse(&try_) {
                    reached.insert(variant(&command));
                }
            }
        }
        for exempt in ["Chain", "Unimplemented"] {
            reached.insert(exempt);
        }

        // The list of every variant, taken from the same match above by asking it about one value
        // of each. There is no reflection in Rust, so the second list is the match itself: anything
        // it names and nothing below reaches is a variant no row documents.
        let named: BTreeSet<&'static str> = ALL_VARIANTS.iter().copied().collect();
        let undocumented: Vec<&&str> = named.difference(&reached).collect();
        assert!(undocumented.is_empty(), "no row on the page reaches these: {undocumented:?}");
        let unknown: Vec<&&str> = reached.difference(&named).collect();
        assert!(unknown.is_empty(), "the variant list is out of date: {unknown:?}");
    }

    /// The names `variant` above answers with. Kept beside it rather than derived, because Rust
    /// gives no way to enumerate an enum's variants — and it is the *match* that fails to compile
    /// when a variant is added, so this list only has to be corrected once the compiler has already
    /// pointed at the place.
    const ALL_VARIANTS: &[&str] = &[
        "Chain", "Nop", "ClearKeychain", "ModeEnter", "ModeLeave", "Scroll", "ScrollPx",
        "ScrollPage", "ScrollToPerc", "TabNext", "TabPrev", "TabClose", "TabOnly", "TabFocus",
        "TabMove", "TabClone", "Undo", "TabPin", "TabMute", "TabGive", "SessionSave", "SessionLoad",
        "SessionDelete", "Open", "Back", "Forward", "Reload", "Stop", "Home", "Quit", "Close",
        "Zoom", "ZoomIn", "ZoomOut", "Fullscreen", "Hint", "HintFollow", "Help", "Download",
        "DownloadMhtml", "DownloadCancel", "DownloadClear", "DownloadOpen", "DownloadDelete",
        "DownloadRetry", "QuickmarkSave", "QuickmarkLoad", "QuickmarkDel", "BookmarkAdd",
        "BookmarkLoad", "BookmarkDel", "BookmarkList", "History", "Cookies", "Yank", "Search",
        "SearchNext", "SearchPrev", "Navigate", "SelectionToggle", "SelectionDrop",
        "SelectionReverse", "SelectionFollow", "MoveTo", "CmdSetText", "CommandAccept", "Spawn",
        "EditText", "InsertText", "FakeKey", "Set", "ConfigCycle", "ConfigDictAdd",
        "ConfigDictRemove", "ConfigUnset", "ConfigClear", "ConfigDiff", "ConfigListAdd",
        "ConfigListRemove", "ConfigSource", "ConfigEdit", "ConfigWritePy", "Bind", "Unbind",
        "CompletionItemFocus", "CompletionItemDel", "CompletionItemYank", "PromptAccept",
        "PromptItemFocus", "PromptOpenDownload", "PromptYank", "PromptFileselectExternal",
        "AdblockUpdate", "AdblockToggle", "AdblockInfo", "GreasemonkeyReload", "ViewSource",
        "Print", "DevTools", "DevToolsFocus", "Message", "MacroRecord", "MacroRun", "Save",
        "CmdRepeatLast", "SettingsPage", "TabSelect", "TabTake", "WindowOnly", "Screenshot",
        "JsEval", "EditUrl", "EditCommand", "QuickmarkAdd", "HistoryClear", "Later", "Repeat",
        "RunWithCount", "Restart", "Version", "Messages", "Process", "ClickElement",
        "ScrollToAnchor", "DownloadRemove", "ClearMessages", "MarksReload", "Unimplemented",
    ];

    /// Every flag a documented spelling names, as the parser would see it: `-u/--pattern/--url`
    /// is the three names `u`, `pattern` and `url`.
    fn flag_names(doc: &Doc) -> BTreeSet<String> {
        doc.flags
            .iter()
            .flat_map(|flag| flag.split('/'))
            .map(|one| one.trim_start_matches('-').to_string())
            .collect()
    }

    /// **The flags, read out of the source the same way the names are.**
    ///
    /// Each arm's body is scanned for the literals it hands to `has`, `any`, `value` and
    /// `Flagged::new`, which is every way the parser asks whether a flag was given, and the set is
    /// compared with the row's. A flag added to an arm and not written down fails; so does a flag
    /// written down that the arm never reads.
    ///
    /// **Nine commands are outside it** and the test says which: `set` and the six other `config-*`
    /// commands share `parse_config_command`, and `bind` and `unbind` have parsers of their own,
    /// all four hand-written with a `match` on the flag's own letters and no call to find. The nine
    /// are pinned as a list, so a tenth command drifting out of reach is a failure rather than a
    /// hole nobody notices — and the flags of those nine are covered by
    /// `the_hand_written_flag_parsers_take_the_flags_the_page_names`.
    #[test]
    fn every_flag_the_parser_reads_is_on_the_page() {
        let (dispatch, _) = parser_regions();
        let (tokens, masked) = scan(&dispatch);
        let found = arms(&tokens);
        assert!(found.len() > 100, "the arm scrape found {} arms", found.len());

        let mut unchecked: Vec<String> = Vec::new();
        for (index, (names, _, body_at)) in found.iter().enumerate() {
            // An arm's body runs to the next arm's head, or to the end of the match.
            let body_end = found.get(index + 1).map_or(masked.len(), |(_, head_at, _)| *head_at);
            let body: String = masked[*body_at..body_end].iter().collect();
            let body = body.as_str();

            let mut read: BTreeSet<String> = BTreeSet::new();
            for opener in ["has(", "any(", "value(", "Flagged::new(", "Flagged::maxsplit0("] {
                let mut from = 0usize;
                while let Some(at) = body[from..].find(opener) {
                    let start = from + at + opener.len();
                    let end = body[start..].find(')').map_or(body.len(), |e| start + e);
                    read.extend(literals_in(&body[start..end]));
                    from = start;
                }
            }

            let written: BTreeSet<String> = COMMANDS
                .iter()
                .filter(|doc| doc.names.iter().any(|name| names.contains(&name.to_string())))
                .flat_map(|doc| flag_names(doc))
                .collect();

            if read.is_empty() {
                if !written.is_empty() {
                    unchecked.extend(names.iter().cloned());
                }
                continue;
            }
            assert_eq!(read, written, "the flags of {names:?} have drifted");
        }

        // The eight arms that parse their flags somewhere this cannot see, and `bind`, which is
        // short-circuited before the match and has no arm at all.
        assert_eq!(
            unchecked,
            vec![
                "set", "config-cycle", "config-dict-add", "config-dict-remove",
                "config-unset", "config-list-add", "config-list-remove", "unbind",
            ],
            "a command's flags have drifted out of reach of this test"
        );
        assert!(
            !found.iter().any(|(names, _, _)| names.iter().any(|n| n == "bind")),
            "bind has an arm now and can be checked like the rest"
        );
    }

    /// The nine `every_flag_the_parser_reads_is_on_the_page` cannot reach, asked directly.
    ///
    /// Their parsers **reject** an unknown flag rather than ignoring it, which is what makes this
    /// worth writing: `:set --nosuch x` is an error, so a flag that parses is a flag the parser
    /// really has, and a flag the page invented would fail here.
    #[test]
    fn the_hand_written_flag_parsers_take_the_flags_the_page_names() {
        use crate::commands::Command;

        // -p/--print, -t/--temp and -u/--pattern/--url, on the seven that share the parser.
        for name in [
            "set", "config-cycle", "config-dict-add", "config-dict-remove",
            "config-unset", "config-list-add", "config-list-remove",
        ] {
            let doc = COMMANDS.iter().find(|doc| doc.names[0] == name).expect("a row");
            let tail = doc.example.strip_prefix(name).expect("the example starts with the name");
            for flag in ["-p", "--print", "-t", "--temp"] {
                let line = format!("{name} {flag}{tail}");
                assert!(commands::parse(&line).is_ok(), "{line:?} was refused");
            }
            for flag in ["-u", "--pattern", "--url"] {
                let line = format!("{name} {flag} *://x/*{tail}");
                assert!(commands::parse(&line).is_ok(), "{line:?} was refused");
            }
            // ...and a flag nobody documented is an error, which is what gives the four above
            // their meaning.
            let line = format!("{name} --nosuch{tail}");
            assert!(commands::parse(&line).is_err(), "{line:?} was accepted");
        }
        assert!(matches!(
            commands::parse("set -p -u *://x/* content.images"),
            Ok(Command::Set { print: true, pattern: Some(_), .. })
        ));
        // `--replace` is `config-dict-add`'s alone, and the parser knows it by the command's name.
        assert!(matches!(
            commands::parse("config-dict-add --replace url.searchengines zz https://x/{}"),
            Ok(Command::ConfigDictAdd { replace: true, .. })
        ));
        assert!(commands::parse("config-dict-remove --replace url.searchengines zz").is_err());

        // `bind` and `unbind`, whose flags are `--mode` with a value and `--default`.
        assert!(matches!(
            commands::parse("bind --mode caret --default j"),
            Ok(Command::Bind { default: true, .. })
        ));
        assert!(matches!(
            commands::parse("bind -m caret -d j"),
            Ok(Command::Bind { default: true, .. })
        ));
        assert!(commands::parse("bind --nosuch j").is_err());
        assert!(matches!(commands::parse("unbind --mode caret j"), Ok(Command::Unbind { .. })));
        assert!(matches!(commands::parse("unbind -m caret j"), Ok(Command::Unbind { .. })));
        assert!(commands::parse("unbind --nosuch j").is_err());
    }

    /// **The join, which is the reason both halves are one page.**
    ///
    /// A key that *runs* a command and a key that only *types* it are different claims and the page
    /// makes both, separately: `j` runs `scroll`, `o` runs `cmd-set-text` and types `:open`.
    #[test]
    fn the_page_says_which_key_calls_a_command() {
        let by_key = reached(&bindings());
        let of = |name: &str| {
            by_key.get(name).unwrap_or_else(|| panic!("no key reaches {name} at all"))
        };

        // Runs.
        assert!(of("scroll").runs.contains(&"j".to_string()));
        assert!(of("scroll").runs.contains(&"k".to_string()));
        assert!(of("scroll").types.is_empty(), "nothing prefills `:scroll`");
        // A chain: `<Escape>` is `clear-keychain ;; search ;; fullscreen --leave` and names three.
        for name in ["clear-keychain", "search", "fullscreen"] {
            assert!(
                of(name).runs.contains(&"<Escape>".to_string()),
                "{name} does not know about <Escape>"
            );
        }
        // A mode that is not normal is named, because `j` is two different commands in two modes.
        assert!(of("move-to-next-line").runs.contains(&"j (caret)".to_string()));

        // Types.
        assert!(of("cmd-set-text").runs.contains(&"o".to_string()), "`o` runs cmd-set-text");
        for key in ["o", "O", "go"] {
            assert!(of("open").types.contains(&key.to_string()), "`{key}` types :open");
        }
        assert!(!of("open").runs.contains(&"o".to_string()), "`o` does not run :open");
        assert!(of("tab-select").types.contains(&"gt".to_string()), "`gt` types :tab-select");

        // And a command no key reaches at all has no cell to fill, which is the hole this whole
        // section was built for: `:screenshot` existed and was written down nowhere.
        assert!(!by_key.contains_key("screenshot"));
        assert!(!by_key.contains_key("config-diff"));

        // `bind` is not walked into: `bind j scroll down` binds, it does not scroll.
        let mut named = Vec::new();
        commands_named_in("bind j scroll down", &mut named);
        assert_eq!(named, vec!["bind"]);
        // A carried command is: `later 1s reload` reaches `reload`.
        let mut named = Vec::new();
        commands_named_in("later 1s reload ;; tab-close", &mut named);
        assert_eq!(named, vec!["later", "reload", "tab-close"]);

        // Every command any default binding names has a row. A `config.lua` may name anything;
        // bru's own table may not.
        let written: BTreeSet<String> =
            COMMANDS.iter().flat_map(|doc| doc.names.iter().map(|n| n.to_string())).collect();
        let orphans: Vec<&String> = by_key.keys().filter(|name| !written.contains(*name)).collect();
        assert!(orphans.is_empty(), "these are bound and undocumented: {orphans:?}");
    }

    /// The commands are on the page, with their flags, their arguments and their keys — and the
    /// hole this was built for is filled.
    #[test]
    fn the_commands_are_on_the_page() {
        let html = page(&bindings());
        assert_eq!(
            html.matches("<tr data-row=\"command\"").count(),
            COMMANDS.len(),
            "one row per command"
        );
        // Two of the thirty-two that landed on 2026-08-07 and appeared nowhere until now.
        assert!(html.contains("<td class=\"name\">screenshot</td>"));
        assert!(html.contains("<td class=\"name\">config-diff</td>"));
        assert!(html.contains("[--rect] [-f/--force] &lt;filename&gt;"), "the signature is printed");
        // **The join, in the markup**, and it is the column that pays for putting the two halves on
        // one page. `scroll` collects the four normal-mode keys and the four caret-mode ones, each
        // named with the mode it is in because `J` is a different command in each.
        assert!(
            html.contains("<td class=\"bound\">h j k l H (caret) J (caret) K (caret) L (caret)</td>"),
            "scroll's keys"
        );
        // `:open` is run by the paste keys and by `ga`, and *typed* by the eight that prefill it.
        assert!(html.contains("&lt;Ctrl+t&gt; ga PP"), "open's running keys");
        assert!(
            html.contains("<span class=\"typed\">gO go O o wO wo xO xo</span>"),
            "open's prefilling keys"
        );
        // And a command reached only by prefilling has the quieter list and nothing else.
        assert!(html.contains("<td class=\"bound\"><span class=\"typed\">gt</span></td>"));
        // An alias is printed under the name rather than being lost.
        assert!(html.contains("<span class=\"alias\">cmd-edit</span>"));
        // The command rows are countable apart from the binding rows, which is what keeps
        // `every_binding_appears` measuring bindings.
        assert!(!html.contains("<tr class=\"live\"><td class=\"name\">"));
    }

    /// The summary grew by two sentences and by no fraction. An "x of y" was taken out of the first
    /// half by hand on 2026-08-07 and this is where it would creep back.
    #[test]
    fn the_summary_counts_the_commands_without_dividing_them() {
        let html = page(&bindings());
        assert!(html.contains(&format!("{} commands can be typed.", COMMANDS.len())));
        assert!(html.contains("answer to a key."));
        for shape in [" of 264", " of 298", &format!(" of {}", COMMANDS.len())] {
            assert!(!html.contains(shape), "the summary is measuring against a total: {shape:?}");
        }
    }

    /// Everything in a row is prose written by a person and lands in a table cell, exactly as a
    /// refusal reason does. It goes through the same escape.
    #[test]
    fn nothing_in_a_row_can_escape_its_cell() {
        let html = page(&bindings());
        // `<` and `>` are everywhere in the signatures — `<filename>`, `<mode>`, `<text>` — so this
        // is not a vacuous check: not one of them may reach the page as markup.
        assert!(html.contains("&lt;filename&gt;"));
        assert!(!html.contains("<filename>"));
        for doc in COMMANDS {
            assert!(html.contains(&escape(doc.what)), "{}: the description is not on the page", doc.names[0]);
            let sig = signature(doc);
            if !sig.is_empty() {
                assert!(html.contains(&escape(&sig)), "{}: the signature is not", doc.names[0]);
            }
        }
    }

    #[test]
    fn markup_in_a_command_string_cannot_escape_its_cell() {
        let mut b = bindings();
        b.bind("normal", "ZY", "open <script>alert(1)</script>")
            .expect("a valid binding");
        let html = page(&b);
        assert!(!html.contains("<script>alert(1)"), "it must arrive as text");
        assert!(html.contains("&lt;script&gt;alert(1)"));
    }
}

/// The page for whatever bindings are loaded right now.
///
/// `chrome.rs` serves this on every request, so a `config.lua` reloaded in a later milestone is
/// reflected without a restart.
pub fn current_page() -> String {
    match crate::state::BruState::instance() {
        Some(state) => {
            let bindings = state.lock().expect("state mutex poisoned").bindings_snapshot();
            match bindings {
                Some(bindings) => page(&bindings),
                None => "<!doctype html><meta charset=\"utf-8\"><body>no bindings loaded".to_string(),
            }
        }
        None => "<!doctype html><meta charset=\"utf-8\"><body>no browser state".to_string(),
    }
}
