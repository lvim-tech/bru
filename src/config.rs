//! Startup configuration: the compiled-in default bindings, and the Lua that overrides them.
//!
//! **This is the only file in bru that may mention `mlua`.** DESIGN.md: "Config is Lua, through
//! mlua. It rebinds keys; it does not run on the key path." The Lua state is created here, run
//! here, and dropped here — before any browser exists. What leaves is [`Bindings`], a plain
//! `HashMap`, which becomes the [`BindingTrie`]s that [`crate::bindings::KeyParsers`] owns.
//! Pressing `j` cannot reach an interpreter because by then there is no interpreter to reach.
//!
//! The defaults are qutebrowser 3.7.0's, transcribed from `config/configdata.yml`
//! (`bindings.default:` at line 3676) with no changes to either the keys or the command names —
//! DESIGN.md: "same keys, same command names".

// M7 delivers this module and its tests; `src/main.rs` calls `Config::load()` when the two are
// merged. **Delete this line once that call exists** — after that, an unused item is a real one.
#![allow(dead_code)]

use crate::bindings::{BindingTrie, KeyInfo, KeyParsers, parse_key_sequence, sequence_to_string};
use crate::commands::{self, Command};
use crate::modes::Mode;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// qutebrowser's compiled-in bindings, as `(mode, keys, command)`.
///
/// Generated from `/usr/lib/python3.13/site-packages/qutebrowser/config/configdata.yml`,
/// `bindings.default:` — normal 3679–3878, insert 3879, passthrough 3890, command 3892–3924.
/// The `hint`, `prompt`, `yesno`, `caret` and `register` sections are left out because bru has no
/// such modes yet; they come back with the modes.
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
    ("normal", "wi", "devtools"),
    ("normal", "wIh", "devtools left"),
    ("normal", "wIj", "devtools bottom"),
    ("normal", "wIk", "devtools top"),
    ("normal", "wIl", "devtools right"),
    ("normal", "wIw", "devtools window"),
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
    ("normal", "tph", "config-cycle -p -t -u *://{url:host}/* content.plugins ;; reload"),
    ("normal", "tPh", "config-cycle -p -u *://{url:host}/* content.plugins ;; reload"),
    ("normal", "tpH", "config-cycle -p -t -u *://*.{url:host}/* content.plugins ;; reload"),
    ("normal", "tPH", "config-cycle -p -u *://*.{url:host}/* content.plugins ;; reload"),
    ("normal", "tpu", "config-cycle -p -t -u {url} content.plugins ;; reload"),
    ("normal", "tPu", "config-cycle -p -u {url} content.plugins ;; reload"),
    ("normal", "tih", "config-cycle -p -t -u *://{url:host}/* content.images ;; reload"),
    ("normal", "tIh", "config-cycle -p -u *://{url:host}/* content.images ;; reload"),
    ("normal", "tiH", "config-cycle -p -t -u *://*.{url:host}/* content.images ;; reload"),
    ("normal", "tIH", "config-cycle -p -u *://*.{url:host}/* content.images ;; reload"),
    ("normal", "tiu", "config-cycle -p -t -u {url} content.images ;; reload"),
    ("normal", "tIu", "config-cycle -p -u {url} content.images ;; reload"),
    ("normal", "tch", "config-cycle -p -t -u *://{url:host}/* content.cookies.accept all no-3rdparty never ;; reload"),
    ("normal", "tCh", "config-cycle -p -u *://{url:host}/* content.cookies.accept all no-3rdparty never ;; reload"),
    ("normal", "tcH", "config-cycle -p -t -u *://*.{url:host}/* content.cookies.accept all no-3rdparty never ;; reload"),
    ("normal", "tCH", "config-cycle -p -u *://*.{url:host}/* content.cookies.accept all no-3rdparty never ;; reload"),
    ("normal", "tcu", "config-cycle -p -t -u {url} content.cookies.accept all no-3rdparty never ;; reload"),
    ("normal", "tCu", "config-cycle -p -u {url} content.cookies.accept all no-3rdparty never ;; reload"),
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
    pub fn command_for(&self, mode: Mode, keys: &str) -> Option<&str> {
        let sequence = parse_key_sequence(keys).ok()?;
        self.per_mode.get(&mode)?.get(&sequence).map(String::as_str)
    }

    /// How many sequences are bound in `mode`.
    pub fn len(&self, mode: Mode) -> usize {
        self.per_mode.get(&mode).map_or(0, HashMap::len)
    }

    pub fn is_empty(&self) -> bool {
        self.per_mode.values().all(HashMap::is_empty)
    }

    /// Parse every command string and build one trie per mode.
    ///
    /// **This is where "unknown strings warn at startup, never at keypress" is enforced.** A
    /// binding whose command cannot be parsed is reported and dropped; a binding naming a command
    /// bru has not implemented is kept as [`Command::Unimplemented`] and counted, because dropping
    /// it would change how partial matches resolve.
    pub fn into_tries(self) -> HashMap<Mode, BindingTrie<Command>> {
        let mut tries: HashMap<Mode, BindingTrie<Command>> = HashMap::new();
        let mut unimplemented = 0usize;

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
                        if !parsed.is_implemented() {
                            unimplemented += 1;
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

        if unimplemented > 0 {
            eprintln!(
                "bru: {unimplemented} bindings name commands that are not implemented yet; \
                 pressing them will say so rather than do nothing silently"
            );
        }
        tries
    }
}

/// Everything read at startup. Bindings today; the start page, search engines and the rest as the
/// milestones that need them land.
#[derive(Debug, Clone, Default)]
pub struct Config {
    pub bindings: Bindings,
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
        let bindings = Bindings::defaults();
        let Some(path) = path else {
            return Config { bindings };
        };
        if !path.exists() {
            return Config { bindings };
        }
        let source = match std::fs::read_to_string(path) {
            Ok(source) => source,
            Err(e) => {
                eprintln!("bru: could not read {}: {e}", path.display());
                return Config { bindings };
            }
        };
        Config { bindings: apply_lua(bindings, &source, &path.display().to_string()) }
    }

    /// Hand the bindings over to the key path. After this call nothing in bru can reach Lua.
    pub fn into_parsers(self) -> KeyParsers {
        KeyParsers::new(self.bindings.into_tries())
    }
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

/// Run a `config.lua` against a set of bindings and return the result.
///
/// The `Lua` value is confined to this function's body and dropped before it returns. That is the
/// enforcement of "Lua is never on the key path": there is no way to keep a handle to it, because
/// nothing that escapes has a Lua type.
///
/// A syntax error, or a runtime error, is printed once and the bindings applied before it stand —
/// which for a syntax error is all of the defaults and none of the config, since nothing ran.
fn apply_lua(bindings: Bindings, source: &str, chunk_name: &str) -> Bindings {
    let shared = Arc::new(Mutex::new(bindings));

    let result = (|| -> mlua::Result<()> {
        let lua = mlua::Lua::new();
        let bru = lua.create_table()?;

        let target = Arc::clone(&shared);
        bru.set(
            "bind",
            lua.create_function(move |_, (mode, keys, command): (String, String, String)| {
                target
                    .lock()
                    .expect("the config.lua bindings mutex is never poisoned")
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
                    .expect("the config.lua bindings mutex is never poisoned")
                    .unbind(&mode, &keys)
                    .map_err(mlua::Error::RuntimeError)
            })?,
        )?;

        lua.globals().set("bru", bru)?;
        lua.load(source).set_name(chunk_name).exec()
        // `lua` is dropped here, along with both closures and their Arcs.
    })();

    if let Err(e) = result {
        eprintln!("bru: {chunk_name}: {e}");
        eprintln!("bru: continuing with the bindings that loaded before the error");
    }

    Arc::try_unwrap(shared)
        .map(|m| m.into_inner().expect("the bindings mutex is never poisoned"))
        .unwrap_or_else(|arc| {
            arc.lock().expect("the bindings mutex is never poisoned").clone()
        })
}

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
        // 189 normal + 4 insert + 32 command + 1 passthrough, counted from configdata.yml.
        assert_eq!(total, 226, "the default table is not the one transcribed from configdata.yml");
        assert!(unimplemented > 0 && unimplemented < total);
    }

    #[test]
    fn defaults_are_all_parseable_and_none_collide() {
        let bindings = Bindings::defaults();
        // 226 rows in DEFAULT_BINDINGS; if any two normalised to the same key sequence within a
        // mode, one would have silently overwritten the other and the counts would not add up.
        // <Ctrl-A> and <ctrl-a> are the same binding, so this really is checking something.
        assert_eq!(bindings.len(Mode::Normal), 189);
        assert_eq!(bindings.len(Mode::Insert), 4);
        assert_eq!(bindings.len(Mode::Command), 32);
        assert_eq!(bindings.len(Mode::Passthrough), 1);
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
        assert_eq!(config.bindings.len(Mode::Normal), 189);
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
        assert!(b.bind("hint", "f", "hint").is_err(), "hint mode is not implemented yet");
        assert!(b.bind("nonsense", "f", "hint").is_err());
        assert!(b.bind("normal", "<Ctrl-Nonsense>", "tab-next").is_err());
        assert!(b.bind("normal", "j", "  ").is_err());
        assert!(b.unbind("nonsense", "j").is_err());
        assert_eq!(b.unbind("normal", "gg"), Ok(true));
        assert_eq!(b.unbind("normal", "gg"), Ok(false));
    }

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
