//! Startup configuration: the compiled-in default bindings, and the Lua that overrides them.
//!
//! **This and `src/lua.rs` are the only two files in bru that may mention `mlua`.** This line used
//! to say "the only file", and it was true: the Lua state was created here, run here and dropped
//! here, before any browser existed. P1 changed the **lifetime** and not the rule. DESIGN.md chose
//! Lua for the config file "so that a setting can hold a function", and a function cannot be copied
//! out of an interpreter and kept — it *is* the interpreter — so the state now lives in
//! `src/lua.rs`, on the CEF UI thread, for as long as the process does. Everything else in bru still
//! reaches it through `lua.rs`'s four functions and through an opaque `FnRef`.
//!
//! DESIGN.md: "Config is Lua, through mlua. It rebinds keys; it does not run on the key path." What
//! leaves this file is still [`Bindings`], a plain `HashMap`, which becomes the [`BindingTrie`]s
//! that [`crate::bindings::KeyParsers`] owns. **Pressing `j` still cannot reach an interpreter** —
//! not because there is none left, but because nothing on the key path holds a handle to one.
//! `grep -n mlua src/keys.rs` is empty and is the review-level check of it.
//!
//! The defaults are qutebrowser 3.7.0's, transcribed from `config/configdata.yml`
//! (`bindings.default:` at line 3676) with no changes to either the keys or the command names —
//! DESIGN.md: "same keys, same command names".

use crate::bindings::{BindingTrie, KeyInfo, KeyParsers, parse_key_sequence, sequence_to_string};
use crate::commands::{self, Command};
use crate::modes::Mode;
use crate::open::SearchEngines;
/// The typed settings store lives in `settings.rs`, where the behaviour behind each name lives too.
/// It is re-exported here because `bru.set` is a config-time entry point and `Config` carries it.
pub use crate::settings::Settings;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// qutebrowser's compiled-in bindings, as `(mode, keys, command)`.
///
/// Generated from `/usr/lib/python3.13/site-packages/qutebrowser/config/configdata.yml`,
/// `bindings.default:` — normal 3679–3878, insert 3879, hint 3884, passthrough 3890,
/// command 3892–3924, prompt 3925–3950, yesno 3951–3959, caret 3961–3989, and register 3991
/// written out under all four of the modes that read it. Every section `configdata.yml` has is now
/// here — the `prompt` and `yesno` sections came back with `src/prompt.rs`, which is the milestone
/// their note here was waiting for.
///
/// Command strings that bru does not implement are kept verbatim rather than removed: dropping
/// them would change the shape of the trie, so `;` would report NoMatch instead of PartialMatch.
/// See [`crate::commands::Command::Unimplemented`].
#[rustfmt::skip]
pub const DEFAULT_BINDINGS: &[(&str, &str, &str)] = &[
    // -- normal --------------------------------------------------------------------------------
    ("normal", "<Escape>", "clear-keychain ;; search ;; fullscreen --leave"),
    ("normal", "o", "cmd-set-text -s :open"),
    ("normal", "go", "cmd-set-text :open {url:pretty}"),
    ("normal", "O", "cmd-set-text -s :open -t"),
    ("normal", "gO", "cmd-set-text :open -t -r {url:pretty}"),
    ("normal", "xo", "cmd-set-text -s :open -b"),
    ("normal", "xO", "cmd-set-text :open -b -r {url:pretty}"),
    ("normal", "wo", "cmd-set-text -s :open -w"),
    ("normal", "wO", "cmd-set-text :open -w {url:pretty}"),
    ("normal", "/", "cmd-set-text /"),
    ("normal", "?", "cmd-set-text ?"),
    ("normal", ":", "cmd-set-text :"),
    ("normal", "ga", "open -t"),
    ("normal", "<Ctrl-T>", "open -t"),
    ("normal", "<Ctrl-N>", "open -w"),
    ("normal", "<Ctrl-Shift-N>", "open -p"),
    ("normal", "d", "tab-close"),
    ("normal", "<Ctrl-W>", "tab-close"),
    ("normal", "<Ctrl-Shift-W>", "close"),
    ("normal", "D", "tab-close -o"),
    ("normal", "co", "tab-only"),
    ("normal", "T", "cmd-set-text -sr :tab-focus"),
    ("normal", "gm", "tab-move"),
    ("normal", "gK", "tab-move -"),
    ("normal", "gJ", "tab-move +"),
    ("normal", "J", "tab-next"),
    ("normal", "<Ctrl-PgDown>", "tab-next"),
    ("normal", "K", "tab-prev"),
    ("normal", "<Ctrl-PgUp>", "tab-prev"),
    ("normal", "gC", "tab-clone"),
    ("normal", "r", "reload"),
    ("normal", "<F5>", "reload"),
    ("normal", "R", "reload -f"),
    ("normal", "<Ctrl-F5>", "reload -f"),
    ("normal", "H", "back"),
    ("normal", "<back>", "back"),
    // --- src/userstyles.rs -----------------------------------------------------------------------
    // `ts` — the per-site stylesheets. bru's own key for bru's own command; `t` is qutebrowser's
    // prefix for "toggle something" and `s` was the only letter in it nothing had taken.
    ("normal", "ts", "styles-toggle"),
    // --- end src/userstyles.rs -------------------------------------------------------------------
    ("normal", "th", "back -t"),
    ("normal", "wh", "back -w"),
    ("normal", "L", "forward"),
    ("normal", "<forward>", "forward"),
    ("normal", "tl", "forward -t"),
    ("normal", "wl", "forward -w"),
    ("normal", "<F11>", "fullscreen"),
    ("normal", "f", "hint"),
    ("normal", "F", "hint all tab"),
    ("normal", "wf", "hint all window"),
    ("normal", ";b", "hint all tab-bg"),
    ("normal", ";f", "hint all tab-fg"),
    ("normal", ";h", "hint all hover"),
    ("normal", ";i", "hint images"),
    ("normal", ";I", "hint images tab"),
    ("normal", ";o", "hint links fill :open {hint-url}"),
    ("normal", ";O", "hint links fill :open -t -r {hint-url}"),
    ("normal", ";y", "hint links yank"),
    ("normal", ";Y", "hint links yank-primary"),
    ("normal", ";r", "hint --rapid links tab-bg"),
    ("normal", ";R", "hint --rapid links window"),
    ("normal", ";d", "hint links download"),
    ("normal", ";t", "hint inputs"),
    ("normal", "gi", "hint inputs --first"),
    ("normal", "h", "scroll left"),
    ("normal", "j", "scroll down"),
    ("normal", "k", "scroll up"),
    ("normal", "l", "scroll right"),
    ("normal", "u", "undo"),
    ("normal", "U", "undo -w"),
    ("normal", "<Ctrl-Shift-T>", "undo"),
    ("normal", "gg", "scroll-to-perc 0"),
    ("normal", "G", "scroll-to-perc"),
    ("normal", "n", "search-next"),
    ("normal", "N", "search-prev"),
    ("normal", "i", "mode-enter insert"),
    ("normal", "v", "mode-enter caret"),
    ("normal", "V", "mode-enter caret ;; selection-toggle --line"),
    ("normal", "`", "mode-enter set_mark"),
    ("normal", "'", "mode-enter jump_mark"),
    ("normal", "yy", "yank"),
    ("normal", "yY", "yank -s"),
    ("normal", "yt", "yank title"),
    ("normal", "yT", "yank title -s"),
    ("normal", "yd", "yank domain"),
    ("normal", "yD", "yank domain -s"),
    ("normal", "yp", "yank pretty-url"),
    ("normal", "yP", "yank pretty-url -s"),
    ("normal", "ym", "yank inline [{title}]({url:yank})"),
    ("normal", "yM", "yank inline [{title}]({url:yank}) -s"),
    ("normal", "pp", "open -- {clipboard}"),
    ("normal", "pP", "open -- {primary}"),
    ("normal", "Pp", "open -t -- {clipboard}"),
    ("normal", "PP", "open -t -- {primary}"),
    ("normal", "wp", "open -w -- {clipboard}"),
    ("normal", "wP", "open -w -- {primary}"),
    ("normal", "m", "quickmark-save"),
    ("normal", "b", "cmd-set-text -s :quickmark-load"),
    ("normal", "B", "cmd-set-text -s :quickmark-load -t"),
    ("normal", "wb", "cmd-set-text -s :quickmark-load -w"),
    ("normal", "M", "bookmark-add"),
    ("normal", "gb", "cmd-set-text -s :bookmark-load"),
    ("normal", "gB", "cmd-set-text -s :bookmark-load -t"),
    ("normal", "wB", "cmd-set-text -s :bookmark-load -w"),
    ("normal", "sf", "save"),
    ("normal", "ss", "cmd-set-text -s :set"),
    ("normal", "sl", "cmd-set-text -s :set -t"),
    ("normal", "sk", "cmd-set-text -s :bind"),
    ("normal", "-", "zoom-out"),
    ("normal", "+", "zoom-in"),
    ("normal", "=", "zoom"),
    ("normal", "[[", "navigate prev"),
    ("normal", "]]", "navigate next"),
    ("normal", "{{", "navigate prev -t"),
    ("normal", "}}", "navigate next -t"),
    ("normal", "gu", "navigate up"),
    ("normal", "gU", "navigate up -t"),
    ("normal", "<Ctrl-A>", "navigate increment"),
    ("normal", "<Ctrl-X>", "navigate decrement"),
    // **Six keys for the inspector, where qutebrowser has seven, and the missing one is missing on
    // purpose.**
    //
    // There was one key here for a while. All seven went when it turned out CEF handed bru no docked
    // inspector at all: five of them named a placement that could not happen, and a key that
    // promises a position and opens a window is a key that lies. That reading was wrong — the
    // inspector *is* handed over as a view and bru had only to place it (CEF-NOTES traps 24–26) — so
    // `bottom`, `right` and `window` all draw now, and the keys that name them can come back.
    //
    // `wIh` (left) and `wIk` (top) do **not** come back. They are the same two pieces of arithmetic
    // as the two that work — an insertion index and a sign — and the nested panel makes them
    // possible, but neither has been built or run, and this table does not bind what has not been
    // tried.
    //
    // `wIc` is bru's own: qutebrowser has no close command, so it has no key for one. The letter was
    // free in this family and reads as what it does.
    ("normal", "wi", "devtools"),
    ("normal", "wIj", "devtools bottom"),
    ("normal", "wIl", "devtools right"),
    ("normal", "wIw", "devtools window"),
    ("normal", "wIc", "devtools-close"),
    ("normal", "wIf", "devtools-focus"),
    ("normal", "gd", "download"),
    ("normal", "ad", "download-cancel"),
    ("normal", "cd", "download-clear"),
    ("normal", "gf", "view-source"),
    ("normal", "gt", "cmd-set-text -s :tab-select"),
    ("normal", "<Ctrl-Tab>", "tab-focus last"),
    ("normal", "<Ctrl-Shift-Tab>", "nop"),
    ("normal", "<Ctrl-^>", "tab-focus last"),
    ("normal", "<Ctrl-V>", "mode-enter passthrough"),
    ("normal", "<Ctrl-Q>", "quit"),
    ("normal", "ZQ", "quit"),
    ("normal", "ZZ", "quit --save"),
    ("normal", "<Ctrl-F>", "scroll-page 0 1"),
    ("normal", "<Ctrl-B>", "scroll-page 0 -1"),
    ("normal", "<Ctrl-D>", "scroll-page 0 0.5"),
    ("normal", "<Ctrl-U>", "scroll-page 0 -0.5"),
    ("normal", "<Alt-1>", "tab-focus 1"),
    ("normal", "g0", "tab-focus 1"),
    ("normal", "g^", "tab-focus 1"),
    ("normal", "<Alt-2>", "tab-focus 2"),
    ("normal", "<Alt-3>", "tab-focus 3"),
    ("normal", "<Alt-4>", "tab-focus 4"),
    ("normal", "<Alt-5>", "tab-focus 5"),
    ("normal", "<Alt-6>", "tab-focus 6"),
    ("normal", "<Alt-7>", "tab-focus 7"),
    ("normal", "<Alt-8>", "tab-focus 8"),
    ("normal", "<Alt-9>", "tab-focus -1"),
    ("normal", "g$", "tab-focus -1"),
    ("normal", "<Ctrl-h>", "home"),
    ("normal", "<Ctrl-s>", "stop"),
    ("normal", "<Ctrl-Alt-p>", "print"),
    ("normal", "Ss", "set"),
    ("normal", "Sb", "bookmark-list --jump"),
    ("normal", "Sq", "bookmark-list"),
    ("normal", "Sh", "history"),
    ("normal", "<Return>", "selection-follow"),
    ("normal", "<Ctrl-Return>", "selection-follow -t"),
    ("normal", ".", "cmd-repeat-last"),
    ("normal", "<Ctrl-p>", "tab-pin"),
    ("normal", "<Alt-m>", "tab-mute"),
    ("normal", "gD", "tab-give"),
    ("normal", "q", "macro-record"),
    ("normal", "@", "macro-run"),
    ("normal", "tsh", "config-cycle -p -t -u *://{url:host}/* content.javascript.enabled ;; reload"),
    ("normal", "tSh", "config-cycle -p -u *://{url:host}/* content.javascript.enabled ;; reload"),
    ("normal", "tsH", "config-cycle -p -t -u *://*.{url:host}/* content.javascript.enabled ;; reload"),
    ("normal", "tSH", "config-cycle -p -u *://*.{url:host}/* content.javascript.enabled ;; reload"),
    ("normal", "tsu", "config-cycle -p -t -u {url} content.javascript.enabled ;; reload"),
    ("normal", "tSu", "config-cycle -p -u {url} content.javascript.enabled ;; reload"),
    ("normal", "tih", "config-cycle -p -t -u *://{url:host}/* content.images ;; reload"),
    ("normal", "tIh", "config-cycle -p -u *://{url:host}/* content.images ;; reload"),
    ("normal", "tiH", "config-cycle -p -t -u *://*.{url:host}/* content.images ;; reload"),
    ("normal", "tIH", "config-cycle -p -u *://*.{url:host}/* content.images ;; reload"),
    ("normal", "tiu", "config-cycle -p -t -u {url} content.images ;; reload"),
    ("normal", "tIu", "config-cycle -p -u {url} content.images ;; reload"),
// --- src/caret.rs --------------------------------------------------------------------------
    // -- caret ---------------------------------------------------------------------------------
    // configdata.yml:3961–3989, transcribed in its own order.
    ("caret", "v", "selection-toggle"),
    ("caret", "V", "selection-toggle --line"),
    ("caret", "<Space>", "selection-toggle"),
    ("caret", "<Ctrl-Space>", "selection-drop"),
    ("caret", "c", "mode-enter normal"),
    ("caret", "j", "move-to-next-line"),
    ("caret", "k", "move-to-prev-line"),
    ("caret", "l", "move-to-next-char"),
    ("caret", "h", "move-to-prev-char"),
    ("caret", "e", "move-to-end-of-word"),
    ("caret", "w", "move-to-next-word"),
    ("caret", "b", "move-to-prev-word"),
    ("caret", "o", "selection-reverse"),
    ("caret", "]", "move-to-start-of-next-block"),
    ("caret", "[", "move-to-start-of-prev-block"),
    ("caret", "}", "move-to-end-of-next-block"),
    ("caret", "{", "move-to-end-of-prev-block"),
    ("caret", "0", "move-to-start-of-line"),
    ("caret", "$", "move-to-end-of-line"),
    ("caret", "gg", "move-to-start-of-document"),
    ("caret", "G", "move-to-end-of-document"),
    // The three that wait for the clipboard. They parse to Unimplemented on purpose: `yank` is the
    // clipboard workstream's command, and a variant added here would collide with it.
    ("caret", "Y", "yank selection -s"),
    ("caret", "y", "yank selection"),
    ("caret", "<Return>", "yank selection"),
    ("caret", "H", "scroll left"),
    ("caret", "J", "scroll down"),
    ("caret", "K", "scroll up"),
    ("caret", "L", "scroll right"),
    ("caret", "<Escape>", "mode-leave"),
    // -- set_mark / jump_mark ------------------------------------------------------------------
    // configdata.yml has no section for either: `RegisterKeyParser` passes `mode=KeyMode.register`
    // to `BaseKeyParser` (modeparsers.py:250), so both modes read the one-line `register:` section
    // at 3991 and every other key names a mark. bru has no `register` mode object to hang that on,
    // so the section is written out once per mode that uses it.
    ("set_mark", "<Escape>", "mode-leave"),
    ("jump_mark", "<Escape>", "mode-leave"),
// --- end src/caret.rs ----------------------------------------------------------------------
// --- src/macros.rs -------------------------------------------------------------------------------
    // The other two modes built on `RegisterKeyParser`, and so the other two readers of the same
    // one-line `register:` section at 3991. Written out per mode for the reason above.
    ("record_macro", "<Escape>", "mode-leave"),
    ("run_macro", "<Escape>", "mode-leave"),
// --- end src/macros.rs ---------------------------------------------------------------------------
    // -- hint ----------------------------------------------------------------------------------
    ("hint", "<Return>", "hint-follow"),
    ("hint", "<Ctrl-R>", "hint --rapid links tab-bg"),
    ("hint", "<Ctrl-F>", "hint links"),
    ("hint", "<Ctrl-B>", "hint all tab-bg"),
    ("hint", "<Escape>", "mode-leave"),
    // -- insert --------------------------------------------------------------------------------
    ("insert", "<Ctrl-E>", "edit-text"),
    ("insert", "<Shift-Ins>", "insert-text -- {primary}"),
    ("insert", "<Escape>", "mode-leave"),
    ("insert", "<Shift-Escape>", "fake-key <Escape>"),
    // -- command -------------------------------------------------------------------------------
    ("command", "<Ctrl-P>", "command-history-prev"),
    ("command", "<Ctrl-N>", "command-history-next"),
    ("command", "<Up>", "completion-item-focus --history prev"),
    ("command", "<Down>", "completion-item-focus --history next"),
    ("command", "<Shift-Tab>", "completion-item-focus prev"),
    ("command", "<Tab>", "completion-item-focus next"),
    ("command", "<Ctrl-Tab>", "completion-item-focus next-category"),
    ("command", "<Ctrl-Shift-Tab>", "completion-item-focus prev-category"),
    ("command", "<PgDown>", "completion-item-focus next-page"),
    ("command", "<PgUp>", "completion-item-focus prev-page"),
    ("command", "<Ctrl-D>", "completion-item-del"),
    ("command", "<Shift-Delete>", "completion-item-del"),
    ("command", "<Ctrl-C>", "completion-item-yank"),
    ("command", "<Ctrl-Shift-C>", "completion-item-yank --sel"),
    ("command", "<Return>", "command-accept"),
    ("command", "<Ctrl-Return>", "command-accept --rapid"),
    ("command", "<Ctrl-B>", "rl-backward-char"),
    ("command", "<Ctrl-F>", "rl-forward-char"),
    ("command", "<Alt-B>", "rl-backward-word"),
    ("command", "<Alt-F>", "rl-forward-word"),
    ("command", "<Ctrl-A>", "rl-beginning-of-line"),
    ("command", "<Ctrl-E>", "rl-end-of-line"),
    ("command", "<Ctrl-U>", "rl-unix-line-discard"),
    ("command", "<Ctrl-K>", "rl-kill-line"),
    ("command", "<Alt-D>", "rl-kill-word"),
    ("command", "<Ctrl-W>", "rl-rubout \" \""),
    ("command", "<Ctrl-Shift-W>", "rl-filename-rubout"),
    ("command", "<Alt-Backspace>", "rl-backward-kill-word"),
    ("command", "<Ctrl-Y>", "rl-yank"),
    ("command", "<Ctrl-?>", "rl-delete-char"),
    ("command", "<Ctrl-H>", "rl-backward-delete-char"),
    ("command", "<Escape>", "mode-leave"),
    // -- passthrough ---------------------------------------------------------------------------
    ("passthrough", "<Shift-Escape>", "mode-leave"),
// --- src/prompt.rs -------------------------------------------------------------------------
    // -- prompt --------------------------------------------------------------------------------
    // configdata.yml:3925-3950, transcribed in its own order. Fifteen of these twenty-six are
    // `rl-*` rows: qutebrowser registers the readline commands for command **and** prompt mode,
    // and so does bru — `cmdline::run_named_inner` offers each one to `prompt::run_readline`
    // first, and the prompt edits the same `CmdLine` the command line does.
    ("prompt", "<Return>", "prompt-accept"),
    ("prompt", "<Ctrl-X>", "prompt-open-download"),
    ("prompt", "<Ctrl-P>", "prompt-open-download --pdfjs"),
    ("prompt", "<Shift-Tab>", "prompt-item-focus prev"),
    ("prompt", "<Up>", "prompt-item-focus prev"),
    ("prompt", "<Tab>", "prompt-item-focus next"),
    ("prompt", "<Down>", "prompt-item-focus next"),
    ("prompt", "<Alt-Y>", "prompt-yank"),
    ("prompt", "<Alt-Shift-Y>", "prompt-yank --sel"),
    ("prompt", "<Alt-E>", "prompt-fileselect-external"),
    ("prompt", "<Ctrl-B>", "rl-backward-char"),
    ("prompt", "<Ctrl-F>", "rl-forward-char"),
    ("prompt", "<Alt-B>", "rl-backward-word"),
    ("prompt", "<Alt-F>", "rl-forward-word"),
    ("prompt", "<Ctrl-A>", "rl-beginning-of-line"),
    ("prompt", "<Ctrl-E>", "rl-end-of-line"),
    ("prompt", "<Ctrl-U>", "rl-unix-line-discard"),
    ("prompt", "<Ctrl-K>", "rl-kill-line"),
    ("prompt", "<Alt-D>", "rl-kill-word"),
    ("prompt", "<Ctrl-W>", "rl-rubout \" \""),
    ("prompt", "<Ctrl-Shift-W>", "rl-filename-rubout"),
    ("prompt", "<Alt-Backspace>", "rl-backward-kill-word"),
    ("prompt", "<Ctrl-?>", "rl-delete-char"),
    ("prompt", "<Ctrl-H>", "rl-backward-delete-char"),
    ("prompt", "<Ctrl-Y>", "rl-yank"),
    ("prompt", "<Escape>", "mode-leave"),
    // -- yesno ---------------------------------------------------------------------------------
    // configdata.yml:3951-3959. `Y` and `N` are `--save`, which bru answers with the reason it
    // cannot: there is nowhere for it to write the answer — `~/.config/bru/` is configer's file
    // and bru must not create it (DESIGN.md). They are still bound, and still act, because a key
    // that says why is a key that did something and because unbinding them here would be bru
    // quietly disagreeing with `configdata.yml`.
    ("yesno", "<Return>", "prompt-accept"),
    ("yesno", "y", "prompt-accept yes"),
    ("yesno", "n", "prompt-accept no"),
    ("yesno", "Y", "prompt-accept --save yes"),
    ("yesno", "N", "prompt-accept --save no"),
    ("yesno", "<Alt-Y>", "prompt-yank"),
    ("yesno", "<Alt-Shift-Y>", "prompt-yank --sel"),
    ("yesno", "<Escape>", "mode-leave"),
// --- end src/prompt.rs ---------------------------------------------------------------------
];

/// Key bindings as a flat, owned, Lua-free table: mode → key sequence → command string.
///
/// Keyed by the *parsed* sequence, so `<Ctrl-A>` and `<ctrl-a>` are recognised as the same binding
/// no matter which spelling the config used — which is how `bru.unbind` can undo a default that
/// was written differently.
#[derive(Debug, Clone, Default)]
pub struct Bindings {
    per_mode: HashMap<Mode, HashMap<Vec<KeyInfo>, String>>,
}

impl Bindings {
    /// qutebrowser's compiled-in bindings.
    ///
    /// Panics if [`DEFAULT_BINDINGS`] contains something unparseable — that is a bug in this file,
    /// not in the user's config, and it is caught by `defaults_are_all_parseable`.
    pub fn defaults() -> Bindings {
        let mut bindings = Bindings::default();
        for (mode, keys, command) in DEFAULT_BINDINGS {
            bindings
                .bind(mode, keys, command)
                .unwrap_or_else(|e| panic!("built-in default {keys:?} in {mode}: {e}"));
        }
        bindings
    }

    /// Bind `keys` to `command` in `mode`, replacing whatever was bound there.
    ///
    /// The error string is what `bru.bind` raises into Lua, so it has to read as a message to
    /// whoever wrote `config.lua`.
    pub fn bind(&mut self, mode: &str, keys: &str, command: &str) -> Result<(), String> {
        let mode = Mode::from_name(mode).ok_or_else(|| format!("unknown mode {mode:?}"))?;
        let sequence = parse_key_sequence(keys).map_err(|e| e.to_string())?;
        if command.trim().is_empty() {
            return Err(format!("{keys:?} was bound to an empty command"));
        }
        self.per_mode.entry(mode).or_default().insert(sequence, command.to_string());
        Ok(())
    }

    /// Unbind `keys` in `mode`. Returns whether anything was bound there.
    pub fn unbind(&mut self, mode: &str, keys: &str) -> Result<bool, String> {
        let mode = Mode::from_name(mode).ok_or_else(|| format!("unknown mode {mode:?}"))?;
        let sequence = parse_key_sequence(keys).map_err(|e| e.to_string())?;
        Ok(self
            .per_mode
            .get_mut(&mode)
            .and_then(|m| m.remove(&sequence))
            .is_some())
    }

    /// The command bound to `keys` in `mode`, if any.
    // What a key is bound to, before the command string is parsed. The tests check the
    // qutebrowser defaults through it; `:bind` will show it to the user.
    #[allow(dead_code)]
    pub fn command_for(&self, mode: Mode, keys: &str) -> Option<&str> {
        let sequence = parse_key_sequence(keys).ok()?;
        self.per_mode.get(&mode)?.get(&sequence).map(String::as_str)
    }

    /// How many sequences are bound in `mode`.
    // How many sequences a mode ended up with — what the tests assert against the 226 defaults.
    #[allow(dead_code)]
    /// Every binding, as `(mode, keys, command)`, sorted — what `bru://help` lists.
    ///
    /// Built from the live table rather than from `DEFAULT_BINDINGS`, so a `config.lua` that
    /// rebinds a key shows the user their key, not qutebrowser's. A help page that can disagree
    /// with the browser is worse than none.
    pub fn all(&self) -> Vec<(Mode, String, String)> {
        let mut out: Vec<(Mode, String, String)> = self
            .per_mode
            .iter()
            .flat_map(|(mode, table)| {
                table
                    .iter()
                    .map(move |(seq, cmd)| (*mode, sequence_to_string(seq), cmd.clone()))
            })
            .collect();
        out.sort_by(|a, b| (a.0, a.1.to_lowercase(), &a.1).cmp(&(b.0, b.1.to_lowercase(), &b.1)));
        out
    }

    // How many sequences a mode ended up with — what the tests assert against the defaults.
    #[allow(dead_code)]
    pub fn len(&self, mode: Mode) -> usize {
        self.per_mode.get(&mode).map_or(0, HashMap::len)
    }

// --- config commands -----------------------------------------------------------------------
    /// Every binding that is not one of bru's own, as the Lua that would reproduce it.
    ///
    /// The bindings half of `:config-diff`; the settings half is `settings::Settings::diff`. Three
    /// shapes, and the third is the one a diff against a *table of defaults* has to have and a
    /// diff against an empty config does not: a key bru ships and the user has taken away is a
    /// difference, and `bru.unbind` is how a config file says it.
    pub fn diff(&self) -> Vec<String> {
        let defaults = Bindings::defaults();
        let mut out = Vec::new();
        for (mode, keys, command) in self.all() {
            match defaults.command_for(mode, &keys) {
                Some(default) if default == command => {}
                _ => out.push(format!(
                    "bru.bind({:?}, {:?}, {:?})",
                    mode.name(),
                    keys,
                    command
                )),
            }
        }
        for (mode, keys, _) in defaults.all() {
            if self.command_for(mode, &keys).is_none() {
                out.push(format!("bru.unbind({:?}, {:?})", mode.name(), keys));
            }
        }
        out.sort();
        out
    }

    /// What bru's own table binds `keys` to in `mode` — `:bind --default`'s destination.
    ///
    /// Built from [`DEFAULT_BINDINGS`] rather than from a copy taken at startup, because the
    /// default is a fact about bru and not about the config that was loaded over it.
    pub fn default_command_for(mode: Mode, keys: &str) -> Option<String> {
        Bindings::defaults()
            .command_for(mode, keys)
            .map(str::to_string)
    }
// --- end config commands ---------------------------------------------------------------------

    /// Parse every command string and build one trie per mode.
    ///
    /// **This is where "unknown strings warn at startup, never at keypress" is enforced.** A
    /// binding whose command cannot be parsed is reported and dropped; a binding naming a command
    /// bru has not implemented is kept as [`Command::Unimplemented`] and counted, because dropping
    /// it would change how partial matches resolve.
    pub fn into_tries(self) -> HashMap<Mode, BindingTrie<Command>> {
        let mut tries: HashMap<Mode, BindingTrie<Command>> = HashMap::new();
        let mut waiting = 0usize;

        let mut modes: Vec<_> = self.per_mode.into_iter().collect();
        modes.sort_by_key(|(mode, _)| *mode);

        for (mode, bindings) in modes {
            let trie = tries.entry(mode).or_default();
            // Sorted so that a warning about binding N always names the same binding.
            let mut bindings: Vec<_> = bindings.into_iter().collect();
            bindings.sort();
            for (sequence, command) in bindings {
                match commands::parse(&command) {
                    Ok(parsed) => {
                        // `exec::is_live`, never `is_implemented`. A binding is live if pressing it
                        // does something and nothing else is the question: `command-history-prev`
                        // and every `rl-*` parse to `Unimplemented` and act all the same, because
                        // they reach `cmdline.rs` by name rather than as a `Command`. Asking the
                        // parser here made this line report 29 while `bru://chrome/help` — which
                        // asks the dispatcher — reported 13, and the same mistake once undercounted
                        // the live-binding total by 17 for a whole stage.
                        if !crate::exec::is_live(&parsed) {
                            waiting += 1;
                        }
                        trie.insert(&sequence, parsed);
                    }
                    Err(e) => eprintln!(
                        "bru: {mode} binding {} -> {command:?} dropped: {e}",
                        sequence_to_string(&sequence)
                    ),
                }
            }
        }

        if waiting > 0 {
            eprintln!(
                "bru: {waiting} bindings name commands that are not implemented yet; \
                 pressing them will say so rather than do nothing silently"
            );
        }
        tries
    }
}

/// Everything read at startup: the bindings and the settings `bru.set` and `bru.search` name.
///
/// **There is no `search` field.** There was one until dict-valued settings landed; the engines are
/// now the `url.searchengines` setting, and `bru.search(name, template)` writes one pair of it. Two
/// stores would have been two answers to `:open gh rust` the moment `:config-dict-add` changed one
/// of them.
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub bindings: Bindings,
    /// `bru.set(key, value)` and `bru.search(name, template)`.
    pub settings: Settings,
}

impl Config {
    /// The engine table this config would search with — the `url.searchengines` setting, rendered
    /// as the type `open.rs` uses. bru's nine defaults with the config's overrides merged in.
    pub fn search(&self) -> SearchEngines {
        self.settings.search_engines()
    }
}

impl Config {
    /// Load the defaults, then `~/.config/bru/config.lua` if it exists.
    ///
    /// Never fails: a missing, unreadable or broken config leaves the defaults standing and prints
    /// why. A browser that will not start because of a typo in a keybinding is worse than a
    /// browser with the default keybindings.
    pub fn load() -> Config {
        Config::load_from(config_path().as_deref())
    }

    /// [`Config::load`] against an explicit path. `None` means "no config file".
    pub fn load_from(path: Option<&Path>) -> Config {
        let config = Config::default_config();
        let Some(path) = path else {
            return config;
        };
        if !path.exists() {
            return config;
        }
        let source = match std::fs::read_to_string(path) {
            Ok(source) => source,
            Err(e) => {
                eprintln!("bru: could not read {}: {e}", path.display());
                return config;
            }
        };
        apply_lua(config, &source, &path.display().to_string())
    }

    /// What bru is before `config.lua` says anything: qutebrowser's bindings, and its DEFAULT
    /// search engine.
    fn default_config() -> Config {
        Config {
            bindings: Bindings::defaults(),
            settings: Settings::default(),
        }
    }

    /// Hand the config over to the rest of bru. After this call nothing in bru can reach Lua.
    ///
    /// The bindings become tries for the key path; the search engines and the start page go to
    /// `open.rs`, which is the only thing that reads them. That install happens here, rather than
    /// in `app.rs`, so that the one existing call site needs no second line — and so that it cannot
    /// be forgotten, because a `:open` against an empty engine table would silently search
    /// DuckDuckGo and look almost right.
    ///
    /// The settings go the same way, to `settings.rs`, so that `:set` at runtime is changing the
    /// same store `config.lua` filled. That install is pure — pushing a value into Chromium is
    /// `settings::apply_at_startup`, which `app.rs` calls once CEF is up, because this function is
    /// also run by unit tests with no browser process behind them.
    pub fn into_parsers(self) -> KeyParsers {
        self.install_stores();
        KeyParsers::new(self.bindings.into_tries())
    }

// --- lua runtime -------------------------------------------------------------------------------
    /// The two stores, without parsing the bindings — the first half of [`Config::into_parsers`].
    ///
    /// Split out because **the plugins have to load between the halves**, and each half needs a
    /// different side of it:
    ///
    /// - a plugin's `bru.get("x")` must read the user's value, so the settings store has to be
    ///   installed *before* the plugins load;
    /// - `into_tries` is where a binding's command string is parsed, and `commands::parse` asks
    ///   `plugins::is_registered`, so the bindings must be parsed *after*.
    ///
    /// `app.rs` therefore calls this, then `plugins::load_at_startup`, then `into_parsers`, which
    /// runs this a second time. Installing twice is idempotent and costs a `SearchEngines` rebuild.
    ///
    /// Built from the settings store rather than carried beside it — see `Config`. It is done here
    /// rather than left to `settings::apply_at_startup` because that only walks what has been
    /// *set*: a config that names no engine at all must still reach `open.rs` with bru's nine, and
    /// a bru with no `~/.config/bru/` is exactly that config.
    pub fn install_stores(&self) {
        crate::open::install(self.search(), self.settings.start_page());
        crate::settings::install(self.settings.clone());
    }
// --- end lua runtime ---------------------------------------------------------------------------
}

/// `$XDG_CONFIG_HOME/bru/config.lua`, or `~/.config/bru/config.lua`.
///
/// DESIGN.md: this file "belongs to configer", like every other hand-written file on this machine.
/// bru reads it and never writes it.
pub fn config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("bru").join("config.lua"))
}

/// Run a `config.lua` against a config and return the result.
///
/// **The state is not dropped when this returns — that is P1, and it is the one behaviour change.**
/// What still holds is the sentence that mattered: nothing that escapes this function has a Lua
/// type. A [`Bindings`] is a `HashMap` and a `Settings` value is a scalar or, since P2, an opaque
/// `crate::lua::FnRef`. What is *new* is that the four config-time names have an outside now: they
/// are installed on the shared `bru` table for the length of this one chunk and swapped back for
/// stubs that say so, whether the chunk finished or threw. Before P1 there was no "afterwards" for
/// them to be in.
///
/// A syntax error, or a runtime error, is printed once and whatever was applied before it stands —
/// which for a syntax error is all of the defaults and none of the config, since nothing ran.
///
/// The surface, all of it:
///
/// - `bru.bind(mode, keys, command)` / `bru.unbind(mode, keys)`
/// - `bru.search(name, url_template)` — `{}` in the template is the term, percent-encoded
/// - `bru.set(key, value)` — one of `crate::settings::SETTINGS`, and nothing else; an unknown key
///   raises with the list, so `start_pgae` is an error at startup rather than a line that does
///   nothing. The same store is what `:set` and `config-cycle` change while bru runs.
///
/// The rest of the `bru` table — `bru.command`, `bru.cmd`, `bru.get`, `bru.message`, `bru.error`,
/// `bru.data_dir`, `bru.plugin_dir` — is `lua.rs`'s and is there whether or not a config is being
/// read.
fn apply_lua(config: Config, source: &str, chunk_name: &str) -> Config {
    let shared = Arc::new(Mutex::new(config));

    // The shared state, made on first use. In the browser `app.rs` has already stood it up on the
    // UI thread; under `cargo test` this is what makes one, which is deliberate — the tests then
    // exercise the same shared state a running bru has, collisions and all.
    let lua = crate::lua::shared();

    let result = (|| -> mlua::Result<()> {
        // **The state outlives this function**, and it has to: a setting whose value is a Lua
        // function keeps a handle into the interpreter, so a `Lua` dropped at the end of this body
        // would leave that handle pointing at nothing — which mlua answers with a panic
        // (`WeakLua::lock` is `.expect("Lua instance is destroyed")`) rather than an error. `lua`
        // is `crate::lua::shared()`, bound above.
        let bru = crate::lua::bru_table(&lua)?;
        let target = Arc::clone(&shared);
        bru.set(
            "bind",
            lua.create_function(move |_, (mode, keys, command): (String, String, String)| {
                target
                    .lock()
                    .expect("the config.lua mutex is never poisoned")
                    .bindings
                    .bind(&mode, &keys, &command)
                    .map_err(mlua::Error::RuntimeError)
            })?,
        )?;

        let target = Arc::clone(&shared);
        bru.set(
            "unbind",
            lua.create_function(move |_, (mode, keys): (String, String)| {
                target
                    .lock()
                    .expect("the config.lua mutex is never poisoned")
                    .bindings
                    .unbind(&mode, &keys)
                    .map_err(mlua::Error::RuntimeError)
            })?,
        )?;

        // M9. `bru.search("ddg", "https://duckduckgo.com/?q={}")`. DESIGN.md settles that the
        // engines are a table here and never a file copied from qutebrowser.
        //
        // It writes one pair of the `url.searchengines` **setting**, which is where the engines
        // live now — `bru.search("gh", …)` and `bru.set("url.searchengines", { gh = … })` are the
        // same store reached two ways, and the file's last word about a name wins. `replace: true`
        // because this spelling has always overwritten and `bru.search("DEFAULT", …)` replacing
        // DuckDuckGo is a documented thing to do; `--replace` is `:config-dict-add`'s guard against
        // a typo at a terminal, and there is no terminal here.
        let target = Arc::clone(&shared);
        bru.set(
            "search",
            lua.create_function(move |_, (name, template): (String, String)| {
                target
                    .lock()
                    .expect("the config.lua mutex is never poisoned")
                    .settings
                    .dict_add("url.searchengines", &name, &template, true)
                    .map(|_| ())
                    .map_err(mlua::Error::RuntimeError)
            })?,
        )?;

        // M9. `bru.set("start_page", "https://start.duckduckgo.com/")` — and, since dict settings
        // landed, `bru.set("statusbar.mode.labels", { normal = "NOR" })`.
        //
        // **This is the one place a setting's value is more than a scalar, and it is the reason the
        // config file is Lua.** DESIGN.md: "Lua is the language of that file so a setting can hold
        // a function, not only a scalar. Any settings design that assumes a value is a string, a
        // number or a boolean is wrong for this project." A table arrives as a table; nothing is
        // serialised to a string and parsed back, and there is no JSON anywhere in the path.
        //
        // A table **merges** into bru's defaults rather than replacing them — see
        // `settings::DictShape`, which carries the argument. `mlua::Value` rather than `String` so
        // that the two shapes are told apart here instead of by looking at the key; a number or a
        // boolean is still accepted and stringified, because `bru.set("content.images", false)`
        // reads better in Lua than the quoted spelling and means the same thing.
        let target = Arc::clone(&shared);
        bru.set(
            "set",
            lua.create_function(move |_, (key, value): (String, mlua::Value)| {
                let mut config = target
                    .lock()
                    .expect("the config.lua mutex is never poisoned");
                match value {
                    // --- config commands ---------------------------------------------------
                    // A list setting's table is a Lua *array* — `{ "https://…", "https://…" }` —
                    // and an array is a table whose keys are 1..n. Which one a table is cannot be
                    // read off the table (an empty one is both), so the setting is asked. See
                    // `settings::is_list`.
                    mlua::Value::Table(table) if crate::settings::is_list(&key) => {
                        let values = lua_table_sequence(&key, &table)?;
                        config.settings.set_list(&key, &values).map(|_| ())
                    }
                    // --- end config commands -----------------------------------------------
                    mlua::Value::Table(table) => {
                        let pairs = lua_table_pairs(&key, &table)?;
                        config.settings.set_dict(&key, &pairs).map(|_| ())
                    }
                    // --- setting functions -------------------------------------------------
                    // **The line this file's own comment above has been promising since M9**: "Lua
                    // is the language of that file so a setting can hold a function, not only a
                    // scalar." Until now the paragraph was about tables, which is the smaller half.
                    //
                    // The function is kept in the shared state (`src/lua.rs`) and what the settings
                    // store holds is an opaque `FnRef` — an id and a `config.lua:12`. That is what
                    // keeps `settings.rs` free of `mlua` and keeps a type that is neither `Send`
                    // nor `Sync` out of a store that lives in a `static Mutex`.
                    //
                    // A setting that does not take one is refused **by name**, with the list of the
                    // ones that do — see `Settings::set_fn`. `scalar_to_string`'s "expected a
                    // string, got function" would have been true and useless.
                    mlua::Value::Function(function) => match crate::lua::register(&function) {
                        Some(handle) => config.settings.set_fn(&key, handle).map(|_| ()),
                        None => Err(format!(
                            "{key}: there is no Lua state to keep the function in, so it could not \
                             be stored"
                        )),
                    },
                    // --- end setting functions ---------------------------------------------
                    other => {
                        let text = scalar_to_string(&key, &other)?;
                        config.settings.set(&key, &text)
                    }
                }
                .map_err(mlua::Error::RuntimeError)
            })?,
        )?;

        // **The `@` is Lua's own "this is a file name" marker**, and without it every error from a
        // config file reads `[string "/home/…/config.lua"]:12:` — Lua quotes the chunk as a literal
        // and truncates it in the middle. With it the message is `/home/…/config.lua:12:`, which is
        // what an editor's error format expects and what `FnRef`'s `origin` prints beside a
        // function-valued setting. `mlua::Chunk::set_name` passes the string straight through.
        lua.load(source).set_name(format!("@{chunk_name}")).exec()
    })();

    // **Off again, whatever happened.** The four closures above hold an `Arc` to a `Config` that
    // stops meaning anything the moment this function returns, and with a state that outlives the
    // call there is now somewhere for them to be called from — a plugin handler, an hour later.
    // Replacing them with the stubs both releases the `Arc` and gives that caller a sentence rather
    // than a silent mutation of a config nobody is holding.
    if let Err(e) = crate::lua::install_config_stubs(&lua) {
        eprintln!("bru: could not put the config-time `bru` functions away: {e}");
    }
    // Replacing a table entry only makes the old function garbage; collecting is what actually drops
    // the Rust closure and with it its `Arc<Mutex<Config>>`. Without this the `try_unwrap` below
    // always fails and the config is cloned out instead of moved — correct either way, and the
    // collection is what makes the ownership readable rather than accidental. `:config-source` is a
    // typed command, so a full collection here costs nothing anybody can feel.
    lua.gc_collect().ok();

    if let Err(e) = result {
        eprintln!("bru: {chunk_name}: {e}");
        eprintln!("bru: continuing with the configuration that loaded before the error");
    }

    Arc::try_unwrap(shared)
        .map(|m| m.into_inner().expect("the config mutex is never poisoned"))
        .unwrap_or_else(|arc| arc.lock().expect("the config mutex is never poisoned").clone())
}

/// A Lua table as the `(key, value)` pairs a dict setting takes.
///
/// Sorted, so a table written in one order and a table written in the other reach `settings.rs` the
/// same way and a printed dictionary does not depend on Lua's hash order. Every key and every value
/// must be a string (or a number, which Lua writes without quotes readily enough); anything else —
/// a nested table, a function, `nil` in a value — is an error naming the key, at startup, where the
/// person who wrote it is looking.
fn lua_table_pairs(option: &str, table: &mlua::Table) -> mlua::Result<Vec<(String, String)>> {
    let mut pairs: Vec<(String, String)> = Vec::new();
    for pair in table.pairs::<mlua::Value, mlua::Value>() {
        let (key, value) = pair?;
        let key = scalar_to_string(option, &key)?;
        let value = scalar_to_string(&format!("{option}[{key}]"), &value)?;
        pairs.push((key, value));
    }
    pairs.sort();
    Ok(pairs)
}

// --- config commands -----------------------------------------------------------------------
/// A Lua array as the entries a list setting takes, **in the order they were written**.
///
/// Order is the difference between this and [`lua_table_pairs`], which sorts: a dictionary has no
/// order and sorting it makes two spellings of one table print alike, while a list's order is the
/// order its entries are fetched and compiled in. `Table::sequence_values` walks 1..n and stops at
/// the first hole, which is Lua's own definition of an array; a table with a string key in it is
/// therefore not silently half-read — the length check below refuses it by name.
fn lua_table_sequence(option: &str, table: &mlua::Table) -> mlua::Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for (index, value) in table.clone().sequence_values::<mlua::Value>().enumerate() {
        let value = value?;
        out.push(scalar_to_string(&format!("{option}[{}]", index + 1), &value)?);
    }
    // `pairs` counts every key; `sequence_values` counts 1..n. A difference means the table has
    // something in it that a list cannot hold — `{ "a", nope = "b" }` — and reading only the array
    // half would drop the rest without a word.
    let total = table.pairs::<mlua::Value, mlua::Value>().count();
    if total != out.len() {
        return Err(mlua::Error::RuntimeError(format!(
            "{option}: expected a list of values like {{ \"a\", \"b\" }}, got a table with \
             {} entry/entries that are not in the list part",
            total - out.len()
        )));
    }
    Ok(out)
}
// --- end config commands -------------------------------------------------------------------

/// A Lua scalar as the string a setting stores. Booleans keep Lua's spelling, which is also the
/// spelling `settings.rs` parses.
fn scalar_to_string(what: &str, value: &mlua::Value) -> mlua::Result<String> {
    match value {
        mlua::Value::String(text) => Ok(text.to_str()?.to_string()),
        mlua::Value::Integer(n) => Ok(n.to_string()),
        mlua::Value::Number(n) => Ok(n.to_string()),
        mlua::Value::Boolean(b) => Ok(b.to_string()),
        other => Err(mlua::Error::RuntimeError(format!(
            "{what}: expected a string, got {}",
            other.type_name()
        ))),
    }
}

// -----------------------------------------------------------------------------------------------
// --- config commands ---------------------------------------------------------------------------
// The four commands that reach this file while bru is running: `:config-source`, `:config-edit`,
// `:bind` and `:unbind`.
// -----------------------------------------------------------------------------------------------

/// **A second Lua state, at runtime, and why that does not break the rule.**
///
/// DESIGN.md's rule is "Lua rebinds keys; it is never on the key path", and this module's own first
/// paragraph puts it sharply: "Pressing `j` cannot reach an interpreter because by then there is no
/// interpreter to reach." Both are about the *key path*, and both stay true here for exactly the
/// reason they were true at startup: [`apply_lua`] creates the `Lua` inside its own body, runs the
/// file, and drops it before it returns. What comes out is a [`Config`] — plain `HashMap`s and a
/// plain settings store. There is no interpreter to reach after this function returns any more than
/// there was after `Config::load`, because it is the same function doing the same thing a second
/// time.
///
/// What *is* new is the cost, and it is worth stating rather than waving through: building an
/// `mlua::Lua` and running a config file takes as long as it takes, and it happens when somebody
/// types `:config-source`. That is a keypress that is allowed to be slow — it is a command, not a
/// movement — and it is on the UI thread, so a `config.lua` with a ten-second loop in it freezes the
/// window. So does one at startup; the difference is that at startup nothing is drawn yet.
///
/// Reads the file over what is **already live**, which is qutebrowser's own behaviour
/// (`configcommands.py:407-429`): the settings come from [`crate::settings::snapshot`] and the
/// bindings from the running table, so a `:set` typed five minutes ago survives a `:config-source`.
/// `clear` is the spelling that asks for the other thing.
pub fn source_over(current: Config, path: &Path, clear: bool) -> Result<Config, String> {
    if !path.exists() {
        return Err(format!("{} does not exist", path.display()));
    }
    let source = std::fs::read_to_string(path)
        .map_err(|e| format!("could not read {}: {e}", path.display()))?;
    let base = if clear { clear_back_to_brus_own() } else { current };
    let config = apply_lua(base, &source, &path.display().to_string());
    // **Sweep the Lua state's function registry down to what is still named**, and the list is the
    // union of two workstreams' holders.
    //
    // Every source, cleared or not: re-running a config registers a second function for each
    // function-valued setting it sets, the store then holds only the new handle, and the old one
    // would sit in the registry for the life of the process. One entry per function-valued setting
    // per `:config-source` is small and has **no bound**, and a thing with no bound is worth a line.
    //
    // **Anything else that registers a function has to be added to this list**, and being left out
    // is not a small bug: the id is never reused, so the holder reads as "not reachable" from then
    // on and quietly falls back to a default. There are two registrars today — `bru.set`, whose
    // handles the settings store carries, and `bru.command`, whose handles `plugins` carries — and
    // the merge of these two workstreams is exactly where forgetting one of them would have bitten.
    let mut still_held = crate::plugins::handles();
    still_held.extend(config.settings.function_handles());
    crate::lua::forget_functions_except(&still_held);
    Ok(config)
}

// --- lua runtime -------------------------------------------------------------------------------
/// `--clear`: bru's own configuration, **and bru's own Lua globals**.
///
/// **This is the behaviour P1 had to handle rather than discover.** Until the state was shared,
/// every `:config-source` built a fresh `Lua`, so a config file that defined a global could not
/// collide with its own previous run and `--clear` had nothing to clear but the `Config`. Sharing
/// one state ends that: source a file twice and its `local`-less variables, its memo tables and its
/// `if already_loaded then return end` guards are all still there from the first run. A `--clear`
/// that cleared the `Config` and left those standing would make a session that sourced twice differ
/// from one that sourced once, silently, and no test written before P1 could have seen it.
///
/// Two things go, and one deliberately does not:
///
/// - every global that is not in `lua::mark_baseline`'s set — the standard library, `bru`, and
///   whatever the **plugins** left, because a plugin is not configuration;
/// - every registered function except the ones a plugin's commands are holding, for the same
///   reason. A `Value::Fn` a config set reads as "not reachable" afterwards and falls back to bru's
///   own default, which is what `--clear` means.
fn clear_back_to_brus_own() -> Config {
    crate::lua::forget_functions_except(&crate::plugins::handles());
    let gone = crate::lua::clear_added_globals();
    if gone > 0 {
        eprintln!("bru[config]: --clear also took away {gone} Lua global(s) a config file set");
    }
    Config::default_config()
}
// --- end lua runtime ---------------------------------------------------------------------------

/// Resolve `:config-source`'s optional argument the way qutebrowser resolves it
/// (`configcommands.py:415-421`): no name is bru's own config file, a relative name is relative to
/// the directory that file is in, an absolute name is itself. `~` is expanded, because a command
/// line is not a shell and nothing else would.
pub fn source_path(filename: Option<&str>) -> Option<PathBuf> {
    let default = config_path()?;
    let Some(filename) = filename else {
        return Some(default);
    };
    let expanded = match filename.strip_prefix("~/") {
        Some(rest) => PathBuf::from(std::env::var_os("HOME")?).join(rest),
        None => PathBuf::from(filename),
    };
    if expanded.is_absolute() {
        return Some(expanded);
    }
    Some(default.parent()?.join(expanded))
}

/// Install a re-read config over the running browser: the engines, the start page, the settings and
/// the key tables, in that order.
///
/// **The bindings are replaced, not patched**, and the half-typed keychain goes with them: a `g`
/// pending against a table that no longer has `gg` in it would be waiting for a key that cannot
/// come. `KeyParsers::new` starts every mode empty, which is the same clearing `mode-leave` does.
fn install_over_the_running_browser(config: Config) {
    let bindings = config.bindings.clone();
    // Engines, start page and settings — the same call `app.rs` makes at startup, so there is one
    // install path rather than two that can drift.
    let parsers = config.into_parsers();
    if let Some(state) = crate::state::BruState::instance() {
        let mut state = state.lock().expect("state mutex poisoned");
        state.set_bindings(bindings);
        state.set_parsers(parsers);
        // Every mode's, because every mode's trie has just been replaced.
        state.clear_keychains(None);
    }
    // What the file set, pushed into Chromium. `into_parsers` only stores it.
    crate::settings::apply_at_startup();
    crate::ipc::push_bar_everywhere();
}

/// `:config-source [filename] [--clear]`.
pub fn run_source(filename: Option<&str>, clear: bool) {
    let Some(path) = source_path(filename) else {
        crate::message::error("config-source: there is no configuration directory to read from");
        return;
    };
    let current = Config {
        bindings: crate::state::BruState::instance()
            .and_then(|state| state.lock().ok().and_then(|state| state.bindings_snapshot()))
            .unwrap_or_else(Bindings::defaults),
        settings: crate::settings::snapshot(),
    };
    match source_over(current, &path, clear) {
        Ok(config) => {
            install_over_the_running_browser(config);
            crate::message::info(&format!("sourced {}", path.display()));
// --- plugin events (this file is workstream A's — one call, after the install) --------------
            // `config-sourced`, after the new bindings and settings are live, so a handler that
            // asks `bru.get` gets the file's answer and not the one it replaced. `:config-source`
            // is typed in a window and the window is the one it was typed in.
            let window = crate::state::BruState::instance()
                .and_then(|state| state.lock().ok().and_then(|state| state.current_window_id()));
            crate::events::fire(crate::events::Event::ConfigSourced, window, || {
                vec![
                    ("path", crate::lua::Arg::Text(path.display().to_string())),
                    ("cleared", crate::lua::Arg::Bool(clear)),
                ]
            });
// --- end plugin events ---------------------------------------------------------------------
        }
        Err(error) => crate::message::error(&format!("config-source: {error}")),
    }
}

/// `:config-edit [--no-source]` — open `config.lua` in `$EDITOR` and re-read it when the editor
/// exits.
///
/// **bru does not create the file and does not create its directory.** DESIGN.md gives
/// `~/.config/bru/config.lua` to configer and says bru writes neither it nor `theme.css`; an editor
/// started on a path inside a directory bru had just made would be bru creating configer's
/// directory with one extra step in between. So a missing directory is refused, by name, and a
/// missing *file* is not: the editor creates that when the user saves, which is a person
/// hand-writing a hand-written file and is exactly what is supposed to happen.
pub fn run_edit(no_source: bool) {
    let Some(path) = config_path() else {
        crate::message::error("config-edit: there is no configuration directory");
        return;
    };
    let Some(dir) = path.parent().map(Path::to_path_buf) else {
        crate::message::error("config-edit: the configuration path has no directory");
        return;
    };
    if !dir.is_dir() {
        crate::message::error(&format!(
            "config-edit: {} does not exist. That directory is configer's, and bru does not create \
             it — make it yourself and bru will read what you put in it.",
            dir.display()
        ));
        return;
    }
    let existed = path.exists();
    crate::editor::edit_file(path.clone(), move |ok| {
        if !ok {
            return;
        }
        if no_source {
            return;
        }
        if !path.exists() {
            // The editor was opened on a file that did not exist and was left that way. Nothing to
            // read, and nothing went wrong.
            return;
        }
        if !existed {
            crate::message::info(&format!("config-edit: {} is new", path.display()));
        }
        run_source(None, false);
    });
}

// -----------------------------------------------------------------------------------------------
// `:bind` and `:unbind`, at runtime
// -----------------------------------------------------------------------------------------------

/// Change the running browser's key table, both halves of it, under one lock.
///
/// The two halves are `BruState::bindings` — the whole table, which `bru://chrome/help`,
/// `hints.rs` and `prompt.rs` all read back — and `BruState::parsers`, the tries the key path
/// matches against. A change that reached one and not the other would be a help page that
/// disagreed with the browser, which `help.rs`'s own module docs call worse than no help page.
///
/// `command` is `None` for `:unbind`. This is [`crate::bindings::BindingTrie::remove`]'s first
/// production caller: it has been written and tested since the trie was, waiting for a command that
/// takes a binding away while bru runs, and `bru.unbind` in `config.lua` never needed it because a
/// config-time unbind happens before any trie exists.
fn change_live_binding(mode: Mode, keys: &str, command: Option<&str>) -> Result<bool, String> {
    let sequence = parse_key_sequence(keys).map_err(|e| e.to_string())?;
    let parsed = match command {
        Some(command) => Some(commands::parse(command).map_err(|e| e.to_string())?),
        None => None,
    };
    let Some(state) = crate::state::BruState::instance() else {
        return Err("there is no browser to rebind".to_string());
    };
    let mut state = state.lock().expect("state mutex poisoned");
    // The whole table first: if the mode name is wrong this is where it is refused, before
    // anything has been touched.
    let existed = match command {
        Some(command) => {
            state
                .bindings_mut()
                .ok_or("the bindings are not loaded yet")?
                .bind(mode.name(), keys, command)?;
            true
        }
        None => state
            .bindings_mut()
            .ok_or("the bindings are not loaded yet")?
            .unbind(mode.name(), keys)?,
    };
    if let Some(parsers) = state.parsers_mut() {
        let trie = parsers.parser_mut(mode).bindings_mut();
        match parsed {
            Some(parsed) => {
                trie.insert(&sequence, parsed);
            }
            None => {
                trie.remove(&sequence);
            }
        }
    }
    // A chain half-typed against the old table has nothing to complete against the new one, and it
    // lives in the *window* as well as in the parser. Measured: without this, `:bind Z open -t`
    // after a stray `Z` completed `ZZ` on the next press and quit bru. See
    // `BruState::clear_keychains`.
    state.clear_keychains(Some(mode));
    Ok(existed)
}

/// `:bind [--default] [--mode <mode>] [<key> [<command>]]`.
///
/// Four shapes, all qutebrowser's (`configcommands.py:124-169`), and one of them is bru's own
/// destination for the same idea: no key at all opens the page listing every binding, which is
/// `qute://bindings` there and `bru://chrome/help` here.
pub fn run_bind(mode: &str, keys: Option<&str>, command: Option<&str>, default: bool) {
    let Some(keys) = keys else {
        // Handled in `exec.rs`, which is the only place that can navigate. Reaching here means the
        // parser built a shape the dispatcher does not know about.
        crate::message::error("bind: no key given");
        return;
    };
    let Some(mode_enum) = Mode::from_name(mode) else {
        crate::message::error(&format!("bind: unknown mode {mode:?}"));
        return;
    };
    // Normalised through the parser, so `<ctrl-a>` and `<Ctrl-A>` name one binding and the message
    // prints the spelling the rest of bru uses.
    let spelled = match parse_key_sequence(keys) {
        Ok(sequence) => sequence_to_string(&sequence),
        Err(e) => {
            crate::message::error(&format!("bind: {e}"));
            return;
        }
    };

    if command.is_none() {
        if default {
            // `:bind --default <key>` — put bru's own binding back, or take the key away when bru
            // ships none. The second half is what makes this the exact inverse of `:bind <key>
            // <command>` on a key bru does not bind, rather than a no-op on one.
            let default_command = Bindings::default_command_for(mode_enum, keys);
            let outcome = change_live_binding(mode_enum, keys, default_command.as_deref());
            match (outcome, default_command) {
                (Ok(_), Some(command)) => crate::message::info(&format!(
                    "{spelled} is back to bru's own in {mode} mode: {command}"
                )),
                (Ok(_), None) => crate::message::info(&format!(
                    "bru binds nothing to {spelled} in {mode} mode, so it is unbound"
                )),
                (Err(e), _) => crate::message::error(&format!("bind: {e}")),
            }
            return;
        }
        // No command and no `--default`: show what is bound, which is what `sk` was for all along.
        let bound = crate::state::BruState::instance()
            .and_then(|state| state.lock().ok().and_then(|state| state.bindings_snapshot()))
            .and_then(|bindings| bindings.command_for(mode_enum, keys).map(str::to_string));
        match bound {
            Some(command) => {
                crate::message::info(&format!("{spelled} is bound to '{command}' in {mode} mode"))
            }
            None => crate::message::info(&format!("{spelled} is unbound in {mode} mode")),
        }
        return;
    }

    match change_live_binding(mode_enum, keys, command) {
        Ok(_) => crate::message::info(&format!(
            "{spelled} is bound to '{}' in {mode} mode",
            command.unwrap_or_default()
        )),
        Err(e) => crate::message::error(&format!("bind: {e}")),
    }
}

/// `:unbind [--mode <mode>] <key>`.
pub fn run_unbind(mode: &str, keys: &str) {
    let Some(mode_enum) = Mode::from_name(mode) else {
        crate::message::error(&format!("unbind: unknown mode {mode:?}"));
        return;
    };
    let spelled = match parse_key_sequence(keys) {
        Ok(sequence) => sequence_to_string(&sequence),
        Err(e) => {
            crate::message::error(&format!("unbind: {e}"));
            return;
        }
    };
    match change_live_binding(mode_enum, keys, None) {
        // qutebrowser raises "Can't find binding … in … mode" here; bru says it plainly, because a
        // key that is already unbound has ended up where the command was asking it to be.
        Ok(false) => crate::message::info(&format!("{spelled} was already unbound in {mode} mode")),
        Ok(true) => crate::message::info(&format!("{spelled} is unbound in {mode} mode")),
        Err(e) => crate::message::error(&format!("unbind: {e}")),
    }
}

/// `:config-diff` — the bindings half, read out of the running table.
pub fn binding_diff() -> Vec<String> {
    crate::state::BruState::instance()
        .and_then(|state| state.lock().ok().and_then(|state| state.bindings_snapshot()))
        .map(|bindings| bindings.diff())
        .unwrap_or_default()
}
// --- end config commands -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bindings::{KeyAction, MatchType};
    use crate::commands::ScrollDirection;
    use std::io::Write;

    /// A `config.lua` in a fresh directory under the scratch space of this test run.
    struct TempConfig {
        dir: PathBuf,
    }

    impl TempConfig {
        fn new(name: &str, source: &str) -> TempConfig {
            let dir = std::env::temp_dir().join(format!("bru-config-test-{}-{name}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("could not create the test directory");
            let mut f = std::fs::File::create(dir.join("config.lua")).expect("could not write config.lua");
            f.write_all(source.as_bytes()).expect("could not write config.lua");
            TempConfig { dir }
        }

        fn path(&self) -> PathBuf {
            self.dir.join("config.lua")
        }
    }

    impl Drop for TempConfig {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn every_default_command_string_parses() {
        // Not one of qutebrowser's compiled-in bindings may fail to parse: a parse error drops the
        // binding, and a dropped binding changes the shape of the trie, which changes what a
        // partial match does.
        let mut unimplemented = 0;
        let mut total = 0;
        for (_mode, keys, cmd) in DEFAULT_BINDINGS {
            total += 1;
            let parsed = commands::parse(cmd)
                .unwrap_or_else(|e| panic!("default binding {keys:?} -> {cmd:?} failed: {e}"));
            if !parsed.is_implemented() {
                unimplemented += 1;
            }
        }
        // 189 normal + 4 insert + 5 hint + 32 command + 1 passthrough, from configdata.yml, plus
        // stage 3's 29 caret rows and the one-line `register:` section under each of the four modes
        // that read it — `set_mark`, `jump_mark`, `record_macro` and `run_macro`. 264 until
        // src/prompt.rs; **+34 with it**, which is `bindings.default.prompt`'s 26 rows and
        // `.yesno`'s 8 — the two sections this table's own comment said would come back with the
        // modes.
        // **299, not 298.** qutebrowser's table is 298; the extra one is bru's own `ts`
        // (`styles-toggle`), which qutebrowser has no command for. A number that grows is a number
        // that has to say which of the two it is counting.
        // **286 since the inspector got five of its keys back** — `wIj`, `wIl`, `wIw`, `wIc` and
        // `wIf`, once `bottom`, `right` and `window` all drew and there was a close command to bind.
        // Before that: 281, after the twelve `t**` rows went — six for `content.plugins` and six for
        // `content.cookies.accept`, two settings bru does not have and will not, so those keys said
        // why they did nothing rather than doing anything. 293 before those; 299 before the
        // inspector was cut to one key. A number that moves has to say why as plainly in both
        // directions.
        assert_eq!(total, 286, "the default table is not the one transcribed from configdata.yml");
        assert!(unimplemented > 0 && unimplemented < total);
    }

    #[test]
    fn defaults_are_all_parseable_and_none_collide() {
        let bindings = Bindings::defaults();
        // 298 rows in DEFAULT_BINDINGS; if any two normalised to the same key sequence within a
        // mode, one would have silently overwritten the other and the counts would not add up.
        // <Ctrl-A> and <ctrl-a> are the same binding, so this really is checking something. Caret
        // mode is where that matters most: `v` and `<Space>` are both `selection-toggle`, and `V`,
        // `Y`, `H`/`J`/`K`/`L` and `G` are the shifted spellings of keys the same mode also binds
        // unshifted — twenty-nine rows that collapse to twenty-eight if the Shift bit is ever lost.
        // **177: 172 plus the five inspector keys that came back.**
        assert_eq!(bindings.len(Mode::Normal), 177);
        assert_eq!(bindings.len(Mode::Insert), 4);
        assert_eq!(bindings.len(Mode::Hint), 5);
        assert_eq!(bindings.len(Mode::Command), 32);
        assert_eq!(bindings.len(Mode::Passthrough), 1);
        assert_eq!(bindings.len(Mode::Caret), 29);
        assert_eq!(bindings.len(Mode::SetMark), 1);
        assert_eq!(bindings.len(Mode::JumpMark), 1);
// --- src/macros.rs -------------------------------------------------------------------------------
        assert_eq!(bindings.len(Mode::RecordMacro), 1);
        assert_eq!(bindings.len(Mode::RunMacro), 1);
// --- end src/macros.rs ---------------------------------------------------------------------------
// --- src/prompt.rs -------------------------------------------------------------------------
        // The two sections that were left out until there were modes to hang them on. `yesno` is
        // where the collision check earns its keep twice over: `y`/`Y` and `n`/`N` are the same
        // two keys with and without Shift, so eight rows collapse to six if the Shift bit is lost.
        assert_eq!(bindings.len(Mode::Prompt), 26);
        assert_eq!(bindings.len(Mode::YesNo), 8);
// --- end src/prompt.rs ---------------------------------------------------------------------
    }

    #[test]
    fn no_config_file_means_qutebrowser_defaults() {
        let config = Config::load_from(None);
        assert_eq!(config.bindings.command_for(Mode::Normal, "j"), Some("scroll down"));
        assert_eq!(config.bindings.command_for(Mode::Normal, "k"), Some("scroll up"));
        assert_eq!(config.bindings.command_for(Mode::Normal, "J"), Some("tab-next"));
        assert_eq!(config.bindings.command_for(Mode::Normal, "K"), Some("tab-prev"));
        assert_eq!(config.bindings.command_for(Mode::Normal, "d"), Some("tab-close"));
        assert_eq!(config.bindings.command_for(Mode::Normal, "o"), Some("cmd-set-text -s :open"));
        assert_eq!(config.bindings.command_for(Mode::Passthrough, "<Shift-Escape>"), Some("mode-leave"));

        // A path that does not exist is the same as no path at all.
        let missing = std::env::temp_dir().join("bru-there-is-no-config-here.lua");
        assert!(!missing.exists());
        assert_eq!(
            Config::load_from(Some(&missing)).bindings.command_for(Mode::Normal, "j"),
            Some("scroll down")
        );
    }

    #[test]
    fn a_config_lua_rebinds_keys() {
        let cfg = TempConfig::new(
            "swap",
            r#"
                bru.bind("normal", "J", "tab-prev")
                bru.bind("normal", "K", "tab-next")
            "#,
        );
        let config = Config::load_from(Some(&cfg.path()));
        assert_eq!(config.bindings.command_for(Mode::Normal, "J"), Some("tab-prev"));
        assert_eq!(config.bindings.command_for(Mode::Normal, "K"), Some("tab-next"));
        // Everything else is untouched.
        assert_eq!(config.bindings.command_for(Mode::Normal, "j"), Some("scroll down"));

        // ...and the swap survives all the way into the trie the key path uses.
        let mut parsers = config.into_parsers();
        let j = crate::bindings::parse_key_sequence("J").unwrap()[0];
        assert_eq!(
            parsers.handle(Mode::Normal, j).action,
            KeyAction::Run { command: Command::TabPrev, count: None }
        );
    }

    #[test]
    fn a_config_lua_can_unbind() {
        let cfg = TempConfig::new(
            "unbind",
            r#"
                bru.unbind("normal", "d")
                bru.bind("normal", "gs", "scroll down")
            "#,
        );
        let config = Config::load_from(Some(&cfg.path()));
        assert_eq!(config.bindings.command_for(Mode::Normal, "d"), None);
        assert_eq!(config.bindings.command_for(Mode::Normal, "gs"), Some("scroll down"));

        let tries = config.bindings.into_tries();
        let normal = &tries[&Mode::Normal];
        let d = crate::bindings::parse_key_sequence("d").unwrap();
        assert_eq!(normal.matches(&d).match_type(), MatchType::NoMatch);
        let gs = crate::bindings::parse_key_sequence("gs").unwrap();
        assert_eq!(normal.matches(&gs).match_type(), MatchType::ExactMatch);
    }

    #[test]
    fn a_config_lua_adds_search_engines() {
        let cfg = TempConfig::new(
            "search",
            r#"
                bru.search("ddg", "https://duckduckgo.com/?q={}")
                bru.search("gh", "https://github.com/search?q={}")
                bru.set("start_page", "https://start.duckduckgo.com/")
            "#,
        );
        let config = Config::load_from(Some(&cfg.path()));
        assert_eq!(config.search().get("ddg"), Some("https://duckduckgo.com/?q={}"));
        assert_eq!(config.search().get("gh"), Some("https://github.com/search?q={}"));
        // DEFAULT is there whether or not the config mentioned it.
        assert_eq!(config.search().get("DEFAULT"), Some(crate::open::DEFAULT_ENGINE_URL));
        assert_eq!(
            config.settings.start_page().as_deref(),
            Some("https://start.duckduckgo.com/")
        );

        // ...and the table survives all the way into the decision `:open` makes.
        assert_eq!(
            crate::open::decide("gh rust cef", &config.search()),
            Some(crate::open::Target::Search {
                engine: "gh".to_string(),
                term: "rust cef".to_string(),
                url: "https://github.com/search?q=rust%20cef".to_string(),
            })
        );
    }

    /// **A bru with no `config.lua` at all searches with all nine.** DECISIONS.md item 4 as the
    /// user corrected it, asked of the thing `app.rs` actually builds rather than of a table a test
    /// filled in.
    #[test]
    fn no_config_file_still_ships_nine_search_engines_and_fourteen_mode_labels() {
        let config = Config::load_from(None);
        let engines = config.search();
        for (name, template) in crate::open::DEFAULT_ENGINES {
            assert_eq!(engines.get(name), Some(*template), "{name} is missing");
        }
        assert_eq!(engines.iter().count(), 9);
        // And each one is reachable as a prefix at `:open`, which is the claim that matters.
        assert_eq!(
            crate::open::decide("wiki CEF", &engines).map(|t| t.url().to_string()),
            Some("https://en.wikipedia.org/wiki/CEF".to_string())
        );
        assert_eq!(
            crate::open::decide("ub yeet", &engines).map(|t| t.url().to_string()),
            Some("https://www.urbandictionary.com/define.php?term=yeet".to_string())
        );
        assert_eq!(
            crate::open::decide("cef rust", &engines).map(|t| t.url().to_string()),
            Some("https://duckduckgo.com/?q=cef%20rust".to_string())
        );
        // Twelve labels, all bru's own.
        assert_eq!(
            config.settings.describe("statusbar.mode.labels", None).unwrap().lines().next(),
            Some("statusbar.mode.labels — 14 entries, 0 changed from bru's own")
        );
    }

    /// **The reason `config.lua` is Lua**: a setting whose value is a table, arriving as a table.
    #[test]
    fn a_lua_table_is_a_settings_value_and_it_merges() {
        let cfg = TempConfig::new(
            "dict",
            r#"
                bru.set("statusbar.mode.labels", { normal = "NOR", insert = "INS" })
                bru.set("url.searchengines", { gh = "https://github.com/search?q={}" })
            "#,
        );
        let config = Config::load_from(Some(&cfg.path()));
        let printed = config.settings.describe("statusbar.mode.labels", None).unwrap();
        assert!(printed.contains("statusbar.mode.labels[normal] = NOR"), "{printed}");
        assert!(printed.contains("statusbar.mode.labels[insert] = INS"), "{printed}");
        // The ten the file did not mention are still bru's.
        assert!(printed.contains("statusbar.mode.labels[caret] = caret\n"), "{printed}");
        assert!(printed.starts_with("statusbar.mode.labels — 14 entries, 2 changed"), "{printed}");

        // A tenth engine, and the nine still there.
        let engines = config.search();
        assert_eq!(engines.get("gh"), Some("https://github.com/search?q={}"));
        assert_eq!(engines.get("yt"), Some("https://www.youtube.com/results?search_query={}"));
        assert_eq!(engines.iter().count(), 10);
    }

    /// `bru.search` and `bru.set` are two doors to one table, and the file's last word wins.
    #[test]
    fn bru_search_and_the_setting_are_the_same_store() {
        let cfg = TempConfig::new(
            "two-doors",
            r#"
                bru.search("gh", "https://github.com/search?q={}")
                bru.set("url.searchengines", { sr = "https://sourcegraph.com/search?q={}" })
                bru.search("gh", "https://github.com/issues?q={}")
            "#,
        );
        let config = Config::load_from(Some(&cfg.path()));
        let engines = config.search();
        // Neither door threw the other's work away, and the second `bru.search` won.
        assert_eq!(engines.get("gh"), Some("https://github.com/issues?q={}"));
        assert_eq!(engines.get("sr"), Some("https://sourcegraph.com/search?q={}"));
        assert_eq!(engines.iter().count(), 11);
        // ...and one store means `:set url.searchengines` sees what `bru.search` wrote.
        let printed = config.settings.describe("url.searchengines", None).unwrap();
        assert!(printed.contains("url.searchengines[gh] = https://github.com/issues?q={}   (added)"));
    }

    /// A table with something in it that is not a string is an error at startup, naming the key.
    #[test]
    fn a_bad_table_is_an_error_the_config_author_can_see() {
        let cfg = TempConfig::new(
            "bad-table",
            r#"
                bru.search("ok", "https://x/?q={}")
                bru.set("statusbar.mode.labels", { normal = {} })
                bru.search("never", "https://y/?q={}")
            "#,
        );
        let config = Config::load_from(Some(&cfg.path()));
        assert_eq!(config.search().get("ok"), Some("https://x/?q={}"));
        assert_eq!(config.search().get("never"), None);
        // The label is untouched — the whole table is refused, not the pairs after the bad one.
        let printed = config.settings.describe("statusbar.mode.labels", None).unwrap();
        assert!(printed.contains("0 changed"), "{printed}");

        // A key that is not one of the twelve is refused too, with the list.
        let cfg = TempConfig::new("bad-key", r#"bru.set("statusbar.mode.labels", { nope = "X" })"#);
        let config = Config::load_from(Some(&cfg.path()));
        assert!(config
            .settings
            .describe("statusbar.mode.labels", None)
            .unwrap()
            .contains("0 changed"));

        // A non-table value still works for a scalar setting, and a boolean keeps Lua's spelling.
        let cfg = TempConfig::new(
            "scalars",
            r#"
                bru.set("start_page", "example.com")
                bru.set("content.images", false)
            "#,
        );
        let config = Config::load_from(Some(&cfg.path()));
        assert_eq!(config.settings.start_page().as_deref(), Some("example.com"));
        assert_eq!(
            config.settings.describe("content.images", None).unwrap(),
            "content.images = false"
        );
    }

    #[test]
    fn a_config_lua_can_replace_the_default_engine() {
        let cfg = TempConfig::new(
            "default-engine",
            r#"bru.search("DEFAULT", "https://search.marginalia.nu/search?query={}")"#,
        );
        let config = Config::load_from(Some(&cfg.path()));
        // DECISIONS item 4: a bare `:open words` goes to DEFAULT, whatever DEFAULT now is.
        assert_eq!(
            crate::open::decide("python dict", &config.search()).map(|t| t.url().to_string()),
            Some("https://search.marginalia.nu/search?query=python%20dict".to_string())
        );
    }

    #[test]
    fn a_bad_search_engine_or_setting_is_an_error_the_config_author_can_see() {
        let cfg = TempConfig::new(
            "bad-search",
            r#"
                bru.search("ok", "https://x/?q={}")
                bru.set("start_pgae", "https://typo/")
                bru.search("never", "https://y/?q={}")
            "#,
        );
        let config = Config::load_from(Some(&cfg.path()));
        // The call before the bad one took effect; the bad one raised; the one after did not run.
        assert_eq!(config.search().get("ok"), Some("https://x/?q={}"));
        assert_eq!(config.settings.start_page(), None);
        assert_eq!(config.search().get("never"), None);

        // A name with a space could never be typed at `:open`, so it is refused rather than kept.
        let mut engines = SearchEngines::default();
        assert!(engines.set("two words", "https://x/?q={}").is_err());
        // A template with no placeholder would drop the search term silently.
        assert!(engines.set("w", "https://x/").is_err());

        let mut settings = Settings::default();
        assert!(settings.set("start_page", "").is_err());
        assert!(settings.set("nonsense", "x").is_err());
        assert!(settings.set("start_page", "example.com").is_ok());
    }

    #[test]
    fn a_syntax_error_prints_once_and_the_defaults_still_load() {
        // `local` with nothing after it: the chunk never runs, so not even the first line of it
        // takes effect. What must not happen is bru failing to start.
        let cfg = TempConfig::new(
            "syntax-error",
            r#"
                bru.bind("normal", "J", "tab-prev")
                local
            "#,
        );
        let config = Config::load_from(Some(&cfg.path()));
        assert_eq!(
            config.bindings.command_for(Mode::Normal, "J"),
            Some("tab-next"),
            "a syntax error means nothing in the file ran, so J is still the default"
        );
        assert_eq!(config.bindings.command_for(Mode::Normal, "j"), Some("scroll down"));
        assert_eq!(config.bindings.len(Mode::Normal), 177);
    }

    #[test]
    fn a_runtime_error_keeps_what_ran_before_it() {
        let cfg = TempConfig::new(
            "runtime-error",
            r#"
                bru.bind("normal", "J", "tab-prev")
                bru.bind("normal", "<Ctrl-Nonsense>", "tab-next")
                bru.bind("normal", "K", "tab-next")
            "#,
        );
        let config = Config::load_from(Some(&cfg.path()));
        // The bind before the bad one took effect; the bad one raised; the one after did not run.
        assert_eq!(config.bindings.command_for(Mode::Normal, "J"), Some("tab-prev"));
        assert_eq!(config.bindings.command_for(Mode::Normal, "K"), Some("tab-prev"));
    }

    #[test]
    fn an_unknown_mode_or_key_is_an_error_the_config_author_can_see() {
        let mut b = Bindings::defaults();
        assert!(b.bind("hint", "<Ctrl-J>", "mode-leave").is_ok(), "hint is a mode as of M12");
        assert!(b.bind("caret", "f", "hint").is_ok(), "caret is a mode as of stage 3");
        assert!(b.bind("set_mark", "<Ctrl-J>", "mode-leave").is_ok(), "so are the two mark modes");
        assert!(b.bind("jump_mark", "<Ctrl-J>", "mode-leave").is_ok());
        // --- src/prompt.rs -------------------------------------------------------------------
        // Both prompt modes are real sections now, so a `config.lua` may rebind inside them.
        assert!(b.bind("prompt", "<Ctrl-J>", "prompt-accept").is_ok());
        assert!(b.bind("yesno", "j", "prompt-accept yes").is_ok());
        // What is still refused is entering one by command — `modeman.mode_enter`'s own rule,
        // enforced by `commands::parse` so a `config.lua` hears about it at startup.
        assert!(commands::parse("mode-enter prompt").is_err());
        assert!(commands::parse("mode-enter yesno").is_err());
        assert!(commands::parse("mode-enter hint").is_err());
        assert!(commands::parse("mode-enter command").is_err());
        assert!(commands::parse("mode-enter insert").is_ok());
        // --- end src/prompt.rs ---------------------------------------------------------------
        assert!(b.bind("nonsense", "f", "hint").is_err());
        assert!(b.bind("normal", "<Ctrl-Nonsense>", "tab-next").is_err());
        assert!(b.bind("normal", "j", "  ").is_err());
        assert!(b.unbind("nonsense", "j").is_err());
        assert_eq!(b.unbind("normal", "gg"), Ok(true));
        assert_eq!(b.unbind("normal", "gg"), Ok(false));
    }

// --- config commands -----------------------------------------------------------------------
    /// A list setting's value is a Lua **array**, and it appends.
    #[test]
    fn a_lua_array_is_a_list_settings_value_and_it_appends() {
        let cfg = TempConfig::new(
            "list",
            r#"
                bru.set("content.blocking.adblock.lists", {
                    "https://example.com/mine.txt",
                    "https://example.com/other.txt",
                })
            "#,
        );
        let config = Config::load_from(Some(&cfg.path()));
        let printed = config
            .settings
            .describe("content.blocking.adblock.lists", None)
            .unwrap();
        assert!(
            printed.starts_with("content.blocking.adblock.lists — 4 entries, 2 added"),
            "{printed}"
        );
        // Order is kept, which is the difference between reading an array and reading a map: the
        // two lists bru ships come first and the file's two follow in the order it wrote them.
        assert!(printed.contains("[2] = https://example.com/mine.txt"), "{printed}");
        assert!(printed.contains("[3] = https://example.com/other.txt"), "{printed}");

        // A table with a string key in it is not a list, and is refused by name rather than half
        // read — the array part alone would have dropped `nope` without a word.
        let cfg = TempConfig::new(
            "list-with-a-key",
            r#"bru.set("content.blocking.adblock.lists", { "https://x/a.txt", nope = "b" })"#,
        );
        let config = Config::load_from(Some(&cfg.path()));
        assert!(config
            .settings
            .describe("content.blocking.adblock.lists", None)
            .unwrap()
            .starts_with("content.blocking.adblock.lists — 2 entries, 0 added"));
    }

    /// **`:config-source` builds a second Lua state, and the rule it looks like it breaks is about
    /// the key path.**
    ///
    /// What is asserted here is the behaviour that makes the command worth having: it reads the
    /// file *over* what is already live, so a `:set` typed since startup survives it, and
    /// `--clear` is the spelling that does not.
    #[test]
    fn sourcing_a_file_again_reads_it_over_what_is_already_live() {
        let cfg = TempConfig::new(
            "source",
            r#"
                bru.bind("normal", "X", "tab-prev")
                bru.set("start_page", "https://sourced/")
            "#,
        );
        // A browser that has been running: one key rebound and one setting typed since startup.
        let mut live = Config::load_from(None);
        live.bindings.bind("normal", "Z", "tab-next").unwrap();
        live.settings.set("content.images", "false").unwrap();

        let after = source_over(live.clone(), &cfg.path(), false).expect("the file is there");
        // The file's own two took effect...
        assert_eq!(after.bindings.command_for(Mode::Normal, "X"), Some("tab-prev"));
        assert_eq!(after.settings.start_page().as_deref(), Some("https://sourced/"));
        // ...and what was live and unmentioned survived, which is the whole claim.
        assert_eq!(after.bindings.command_for(Mode::Normal, "Z"), Some("tab-next"));
        assert_eq!(
            after.settings.describe("content.images", None).unwrap(),
            "content.images = false"
        );

        // `--clear` is the other thing, and it is a different command spelling rather than a
        // surprise: bru's defaults, then the file.
        let after = source_over(live, &cfg.path(), true).expect("the file is there");
        assert_eq!(after.bindings.command_for(Mode::Normal, "X"), Some("tab-prev"));
        assert_eq!(after.bindings.command_for(Mode::Normal, "Z"), None);
        assert_eq!(
            after.settings.describe("content.images", None).unwrap(),
            "content.images = true"
        );

        // A file that is not there is an error the typist reads, not a silent no-op.
        let missing = cfg.dir.join("nothing.lua");
        assert!(source_over(Config::default_config(), &missing, false).is_err());
    }

// --- lua runtime -------------------------------------------------------------------------------
    /// **The behaviour P1 changed, and the test that has to exist because of it.**
    ///
    /// Until the Lua state was shared, every `:config-source` built a fresh `Lua`, so a config file
    /// could not collide with its own previous run. Sharing one state ends that: the second run of
    /// this file sees `already_ran` still set from the first and takes the other branch. A
    /// `--clear` that cleared the `Config` and left the globals standing would bind `X` to
    /// `tab-next` the second time and to `tab-prev` the first, and **no test written before P1
    /// could have seen it** — the difference is invisible from inside the `Config`.
    ///
    /// The guard shape here is not invented for the test: `if M then return end` at the top of a
    /// file is how a Lua module says "do not run me twice", and a config file grown past a hundred
    /// lines acquires one.
    #[test]
    fn sourcing_twice_with_clear_leaves_what_sourcing_once_leaves() {
        let cfg = TempConfig::new(
            "twice",
            r#"
                if already_ran then
                    bru.bind("normal", "X", "tab-next")
                else
                    already_ran = true
                    bru.bind("normal", "X", "tab-prev")
                end
            "#,
        );
        let once = source_over(Config::default_config(), &cfg.path(), true)
            .expect("the file is there");
        assert_eq!(once.bindings.command_for(Mode::Normal, "X"), Some("tab-prev"));

        let twice = source_over(once.clone(), &cfg.path(), true).expect("the file is there");
        assert_eq!(
            twice.bindings.command_for(Mode::Normal, "X"),
            Some("tab-prev"),
            "sourcing twice with --clear has to leave what sourcing once leaves; the global the \
             first run set was not cleared"
        );
        // ...and the whole table matches, not only the key the file mentions.
        assert_eq!(once.bindings.all(), twice.bindings.all());

        // **Without `--clear` it is the other thing, and that is the point of the flag.** The
        // globals are the browser's now, so the second run takes the second branch — which is what
        // a config file's own re-entry guard is *for*.
        let again = source_over(twice, &cfg.path(), false).expect("the file is there");
        assert_eq!(again.bindings.command_for(Mode::Normal, "X"), Some("tab-next"));
    }

    /// The four config-time names are put away when the chunk finishes, and they say so.
    ///
    /// Before P1 there was nowhere for them to be called from afterwards, because the whole state
    /// went with the function that made it. Now a plugin handler could reach one an hour later, and
    /// what it would reach is an `Arc` to a `Config` nobody is holding.
    #[test]
    fn the_config_time_functions_are_put_away_when_the_file_has_run() {
        let cfg = TempConfig::new("put-away", r#"bru.bind("normal", "X", "tab-prev")"#);
        let config = Config::load_from(Some(&cfg.path()));
        assert_eq!(config.bindings.command_for(Mode::Normal, "X"), Some("tab-prev"));

        for call in [
            r#"bru.bind("normal", "Y", "tab-next")"#,
            r#"bru.unbind("normal", "j")"#,
            r#"bru.search("x", "https://x/?q={}")"#,
            r#"bru.set("start_page", "https://nowhere/")"#,
        ] {
            let error = crate::lua::with(|lua| lua.load(call).exec().unwrap_err().to_string())
                .expect("the state is on this thread");
            assert!(
                error.contains("only means something while a config file is being read"),
                "{call}: {error}"
            );
        }
        // And the long-lived half is untouched by any of that.
        assert_eq!(
            crate::lua::with(|lua| lua.load("return type(bru.cmd)").eval::<String>().unwrap()),
            Some("function".to_string())
        );
    }

    /// `bru.command` is a plugin's, and a config file that calls it is told so by name rather than
    /// registering a command nothing would ever reach.
    #[test]
    fn a_config_file_cannot_register_a_plugin_command() {
        let cfg = TempConfig::new(
            "not-a-plugin",
            r#"
                bru.bind("normal", "X", "tab-prev")
                bru.command("hello", function() return "hi" end)
                bru.bind("normal", "Y", "tab-prev")
            "#,
        );
        let config = Config::load_from(Some(&cfg.path()));
        // The bind before it took effect; the call raised; the one after did not run — the same
        // shape as every other error in a config file.
        assert_eq!(config.bindings.command_for(Mode::Normal, "X"), Some("tab-prev"));
        assert_eq!(config.bindings.command_for(Mode::Normal, "Y"), None);
        assert!(!crate::plugins::is_registered("hello"));
    }
// --- end lua runtime ---------------------------------------------------------------------------

    /// `:config-source`'s argument, resolved the way qutebrowser resolves it.
    #[test]
    fn a_sourced_filename_is_resolved_against_the_config_directory() {
        let default = config_path().expect("this machine has a config directory");
        assert_eq!(source_path(None), Some(default.clone()));
        assert_eq!(
            source_path(Some("other.lua")),
            Some(default.parent().unwrap().join("other.lua"))
        );
        assert_eq!(
            source_path(Some("/etc/bru.lua")),
            Some(PathBuf::from("/etc/bru.lua"))
        );
        if let Some(home) = std::env::var_os("HOME") {
            assert_eq!(
                source_path(Some("~/x.lua")),
                Some(PathBuf::from(home).join("x.lua"))
            );
        }
    }

    /// `:config-diff`'s bindings half. Three shapes, and the third is the one only a browser that
    /// ships defaults can have: a key bru binds and the user has taken away.
    #[test]
    fn the_binding_diff_is_the_lua_that_would_reproduce_the_table() {
        let mut bindings = Bindings::defaults();
        assert!(bindings.diff().is_empty(), "bru's own table differs from itself in nothing");

        bindings.bind("normal", "J", "tab-prev").unwrap();
        bindings.bind("normal", "ZX", "scroll down").unwrap();
        bindings.unbind("normal", "d").unwrap();
        let diff = bindings.diff();
        assert!(diff.contains(&r#"bru.bind("normal", "J", "tab-prev")"#.to_string()), "{diff:?}");
        assert!(diff.contains(&r#"bru.bind("normal", "ZX", "scroll down")"#.to_string()), "{diff:?}");
        assert!(diff.contains(&r#"bru.unbind("normal", "d")"#.to_string()), "{diff:?}");
        assert_eq!(diff.len(), 3, "and nothing else: {diff:?}");

        // Pasting it back into a fresh bru reproduces the table it came from, which is the only
        // claim a diff makes. Run through the same `bru.bind`/`bru.unbind` a config.lua would.
        let mut rebuilt = Bindings::defaults();
        rebuilt.bind("normal", "J", "tab-prev").unwrap();
        rebuilt.bind("normal", "ZX", "scroll down").unwrap();
        rebuilt.unbind("normal", "d").unwrap();
        assert_eq!(rebuilt.all(), bindings.all());
    }

    /// `:bind --default` needs to know what bru's own table says, whatever has been loaded over it.
    #[test]
    fn brus_own_binding_for_a_key_is_read_from_brus_own_table() {
        assert_eq!(
            Bindings::default_command_for(Mode::Normal, "j").as_deref(),
            Some("scroll down")
        );
        // Spelling does not matter: the lookup is by parsed sequence.
        assert_eq!(
            Bindings::default_command_for(Mode::Normal, "<ctrl-t>").as_deref(),
            Some("open -t")
        );
        // A key bru binds nothing to has no default, which is what makes `:bind --default` on one
        // an unbind rather than a no-op.
        assert_eq!(Bindings::default_command_for(Mode::Normal, "ZX"), None);
        assert_eq!(Bindings::default_command_for(Mode::Caret, "j").as_deref(), Some("move-to-next-line"));
    }

    /// **`BindingTrie::remove` prunes, and `:unbind` is why that matters.**
    ///
    /// Taking `gg` away must leave `g` a partial match, because `ga`, `go` and the rest are still
    /// there; taking the last binding under a prefix away must leave the prefix answering
    /// `NoMatch`, or a key would sit waiting for a chain that cannot complete.
    #[test]
    fn unbinding_the_last_key_under_a_prefix_frees_the_prefix() {
        let mut trie: BindingTrie<Command> = BindingTrie::new();
        let seq = |s: &str| crate::bindings::parse_key_sequence(s).unwrap();
        trie.insert(&seq("gg"), Command::Nop);
        trie.insert(&seq("ga"), Command::Nop);
        assert_eq!(trie.matches(&seq("g")).match_type(), MatchType::PartialMatch);

        trie.remove(&seq("gg"));
        assert_eq!(trie.matches(&seq("gg")).match_type(), MatchType::NoMatch);
        assert_eq!(
            trie.matches(&seq("g")).match_type(),
            MatchType::PartialMatch,
            "ga is still there"
        );
        trie.remove(&seq("ga"));
        assert_eq!(
            trie.matches(&seq("g")).match_type(),
            MatchType::NoMatch,
            "the prefix has to go with its last child, or g waits for a key that cannot come"
        );
        assert!(trie.is_empty());
    }
// --- end config commands -----------------------------------------------------------------------

    #[test]
    fn the_defaults_survive_into_working_key_parsers() {
        // The end-to-end check of M6 + M7: default config in, keypresses out.
        let mut parsers = Config::load_from(None).into_parsers();
        let key = |s: &str| crate::bindings::parse_key_sequence(s).unwrap()[0];

        // 3j -> scroll down, count 3.
        assert_eq!(parsers.handle(Mode::Normal, key("3")).keystring, "3");
        assert_eq!(
            parsers.handle(Mode::Normal, key("j")).action,
            KeyAction::Run { command: Command::Scroll(ScrollDirection::Down), count: Some(3) }
        );
        // J / K are the tab keys.
        assert_eq!(
            parsers.handle(Mode::Normal, key("J")).action,
            KeyAction::Run { command: Command::TabNext, count: None }
        );
        assert_eq!(
            parsers.handle(Mode::Normal, key("K")).action,
            KeyAction::Run { command: Command::TabPrev, count: None }
        );
        // d closes a tab.
        assert_eq!(
            parsers.handle(Mode::Normal, key("d")).action,
            KeyAction::Run {
                command: Command::TabClose { opposite: false, force: false },
                count: None
            }
        );
        // <Ctrl-V> enters passthrough, and there <Shift-Escape> is the only key bru takes.
        assert_eq!(
            parsers.handle(Mode::Normal, key("<Ctrl-V>")).action,
            KeyAction::Run { command: Command::ModeEnter(Mode::Passthrough), count: None }
        );
        let out = parsers.handle(Mode::Passthrough, key("j"));
        assert!(!out.swallow, "passthrough must forward j to the page");
        let out = parsers.handle(Mode::Passthrough, key("<Shift-Escape>"));
        assert_eq!(out.action, KeyAction::Run { command: Command::ModeLeave, count: None });

        // g is a partial match (gg, ga, go, …) and the pending g shows in the bar; gx is nothing.
        assert_eq!(parsers.handle(Mode::Normal, key("g")).keystring, "g");
        let out = parsers.handle(Mode::Normal, key("x"));
        assert_eq!(out.action, KeyAction::NoMatch);
        assert_eq!(out.keystring, "");
    }
}
