//! Plugins — `~/.local/share/bru/plugins/<name>/init.lua`, and the commands they add.
//!
//! A plugin is a directory with an `init.lua` in it. The file runs once, registers what it wants
//! through the `bru` table (`src/lua.rs`), and returns nothing. Loading order is by directory name,
//! so two plugins that both want the same command name resolve the same way on every start.
//!
//! **This module reads directories and keeps a registry. It contains no Lua.** The state, the `bru`
//! table and every call back into a handler are `src/lua.rs`'s, which is the file that may name
//! `mlua`. What is here is bookkeeping, and it is bookkeeping a unit test can drive.
//!
//! # Where a plugin lives, and where a shipped one lives
//!
//! At runtime bru loads `~/.local/share/bru/plugins/`, which is **bru's own directory** — the same
//! place `greasemonkey/` already is. `plugins/<name>/init.lua` *in the repository* is a shipped
//! example: source, the way `chrome/*.js` is source. Nothing in it is read from the user's home, and
//! **nothing here ever writes to `~/.config/bru/`**, which is configer's.
//!
//! `--plugin-dir=<path>` replaces the search directory outright. It exists so a shipped example, or
//! a fixture in a scratch directory, can be run without installing anything — and because `wtype`
//! segfaults CEF on this machine, a check that a plugin works has to be driven by a switch rather
//! than by a keystroke.
//!
//! # What happens when a plugin is wrong
//!
//! This is the argument for Lua over a native plugin, and PLUGINS.md has it as a measurement: the
//! same bug — index 99 of a three-element list — printed `attempt to index a nil value` in Lua and
//! left the process running, and in a `.so` it aborted the host, because a panic cannot cross
//! `extern "C"`. So:
//!
//! - **A plugin that throws while loading is named, with its file, and skipped.** The rest load.
//! - **A handler that throws prints once** through `message::error`, and the browser goes on.
//! - **Three throws in one session disables the plugin**, with a line saying so. `:plugin-reload`
//!   is how a person says they have fixed it.
//!
//! The number is three because one is a typo you want to see again on the next key press and an
//! unbounded count is a plugin that fills the bar with the same sentence for the rest of the day.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

use crate::lua::FnRef;

/// How many times one plugin may throw before it is switched off for the session.
pub const THROW_LIMIT: u32 = 3;

// ------------------------------------------------------------------------------------------------
// Where they live
// ------------------------------------------------------------------------------------------------

/// `--plugin-dir=<path>`, once, from `app.rs`.
static DIR_OVERRIDE: OnceLock<PathBuf> = OnceLock::new();

/// Replace the search directory. Called from `app.rs` before anything is loaded.
pub fn set_dir_override(path: &str) {
    let _ = DIR_OVERRIDE.set(PathBuf::from(path));
}

/// `~/.local/share/bru/plugins`, or whatever `--plugin-dir` named.
///
/// **The directory is never created**, exactly as `greasemonkey::scripts_dir` never creates its
/// own: a browser that makes a directory the user did not ask for is a browser that writes to
/// `~/.local/share` on every start. `:plugin-list` names the path when it is missing, which is the
/// same information without the side effect.
pub fn plugins_dir() -> Option<PathBuf> {
    if let Some(dir) = DIR_OVERRIDE.get() {
        return Some(dir.clone());
    }
    Some(crate::data::data_dir()?.join("plugins"))
}

// ------------------------------------------------------------------------------------------------
// The registry
// ------------------------------------------------------------------------------------------------

/// One plugin, as the registry knows it.
#[derive(Debug, Default)]
struct Plugin {
    dir: PathBuf,
    /// How many times a handler of this plugin has thrown this session.
    throws: u32,
    /// Off, either because it threw [`THROW_LIMIT`] times or because `:plugin-disable` said so.
    disabled: bool,
    /// Why it is off, for `:plugin-list`.
    reason: Option<String>,
    /// What went wrong while loading, if anything. A plugin with this set registered nothing.
    load_error: Option<String>,
}

#[derive(Debug, Default)]
struct Registry {
    plugins: BTreeMap<String, Plugin>,
    /// Command name → the plugin that registered it, and the handler.
    commands: BTreeMap<String, (String, FnRef)>,
}

fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(Registry::default()))
}

fn lock() -> MutexGuard<'static, Registry> {
    registry().lock().expect("the plugin registry mutex")
}

// Which plugin's code is running now — its `init.lua` during load, or one of its handlers.
//
// A thread-local rather than a field of the registry because it is a *stack* discipline and because
// the only thread that ever has a Lua state is the UI thread. `bru.plugin_dir()` reads it. (A `///`
// here would be a doc comment on the macro invocation, which nothing documents.)
thread_local! {
    static CURRENT: std::cell::RefCell<Vec<(String, PathBuf)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// `bru.plugin_dir()` — the directory of the plugin whose code is running, or "" outside one.
pub fn current_plugin_dir() -> String {
    CURRENT.with(|stack| {
        stack
            .borrow()
            .last()
            .map(|(_, dir)| dir.display().to_string())
            .unwrap_or_default()
    })
}

/// The plugin whose code is running, if any.
fn current_plugin() -> Option<String> {
    CURRENT.with(|stack| stack.borrow().last().map(|(name, _)| name.clone()))
}

/// Run `f` with `plugin` on the stack, so `bru.plugin_dir()` and `bru.command` know who is asking.
fn inside<T>(name: &str, dir: PathBuf, f: impl FnOnce() -> T) -> T {
    CURRENT.with(|stack| stack.borrow_mut().push((name.to_string(), dir)));
    let out = f();
    CURRENT.with(|stack| {
        stack.borrow_mut().pop();
    });
    out
}

/// `bru.command(name, fn)`. Called from `lua.rs`, only ever while a plugin's code is running.
///
/// The error string is what is raised back into Lua, so it reads as a message to whoever wrote the
/// plugin. A name that is one of bru's own is **refused**: `commands::parse` matches bru's own arms
/// first, so such a registration could never be reached, and a registration that silently does
/// nothing is worse than one that says why.
pub fn register_command(name: &str, handler: FnRef) -> Result<(), String> {
    let Some(plugin) = current_plugin() else {
        return Err("bru.command may only be called while a plugin is loading".to_string());
    };
    let name = name.trim();
    if name.is_empty() || name.split_whitespace().count() != 1 {
        return Err(format!("{name:?} is not a command name"));
    }
    if crate::help::is_a_name_bru_ships(name) {
        return Err(format!(
            "{name} is one of bru's own commands and a plugin may not take it over"
        ));
    }
    let mut registry = lock();
    if let Some((owner, _)) = registry.commands.get(name) {
        if owner != &plugin {
            return Err(format!("{name} is already registered by the plugin {owner}"));
        }
    }
    registry.commands.insert(name.to_string(), (plugin, handler));
    Ok(())
}

/// Whether `name` is a command some plugin registered — what `commands::parse` asks.
///
/// **Not on the key path.** Every command *name* reaches it once, at parse time, which is startup
/// and `:`-typing; a keystroke matches an already-parsed `Command` in a trie. A `BTreeMap` lookup
/// under a mutex is what that costs.
pub fn is_registered(name: &str) -> bool {
    registry()
        .lock()
        .map(|registry| registry.commands.contains_key(name))
        .unwrap_or(false)
}

/// Every plugin command and the plugin behind it, sorted — what `bru://chrome/help` lists under its
/// own heading.
pub fn command_names() -> Vec<(String, String)> {
    registry()
        .lock()
        .map(|registry| {
            registry
                .commands
                .iter()
                .map(|(name, (plugin, _))| (name.clone(), plugin.clone()))
                .collect()
        })
        .unwrap_or_default()
}

/// Every handler the registry is holding — what `:config-source --clear` must **not** forget.
///
/// See `lua::forget_functions_except`: `--clear` clears the configuration, and a plugin is not
/// configuration.
pub fn handles() -> Vec<FnRef> {
    registry()
        .lock()
        .map(|registry| {
            registry
                .commands
                .values()
                .map(|(_, handler)| handler.clone())
                .collect()
        })
        .unwrap_or_default()
}

// ------------------------------------------------------------------------------------------------
// Loading
// ------------------------------------------------------------------------------------------------

/// What one pass over the directory came to. Errors are kept rather than printed, so that the
/// command and the tests can say the same thing — `greasemonkey::LoadResults`' shape.
#[derive(Default, Debug)]
pub struct LoadResults {
    pub loaded: Vec<String>,
    /// `(plugin, what went wrong)`, where the second names the file.
    pub errors: Vec<(String, String)>,
}

impl LoadResults {
    pub fn successful_str(&self) -> String {
        if self.loaded.is_empty() {
            return "No plugins loaded".to_string();
        }
        format!("Loaded plugins: {}", self.loaded.join(", "))
    }

    pub fn error_str(&self) -> Option<String> {
        if self.errors.is_empty() {
            return None;
        }
        let lines: Vec<String> = self
            .errors
            .iter()
            .map(|(plugin, error)| format!("{plugin}: {error}"))
            .collect();
        Some(format!("Plugins not loaded — {}", lines.join("; ")))
    }
}

/// Every `<dir>/<name>/init.lua`, in directory-name order. A directory without one is not a plugin
/// and is passed over in silence — which is how a `requires/` or a `.git/` beside them behaves.
pub fn discover(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<(String, PathBuf)> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .filter(|path| path.join("init.lua").is_file())
        .filter_map(|path| {
            let name = path.file_name()?.to_string_lossy().to_string();
            Some((name, path))
        })
        .collect();
    found.sort();
    found
}

/// Read every plugin under `dir` into the registry, replacing whatever was there.
///
/// `only` narrows it to one plugin, which is `:plugin-reload <name>`. Everything a reload takes
/// away — the commands, the throw count, a `:plugin-disable` — goes with it, because a person who
/// types `:plugin-reload` has just edited the file and is asking for the plugin they now have.
pub fn load_from(dir: &Path, only: Option<&str>) -> LoadResults {
    let mut results = LoadResults::default();
    forget(only);
    // What the globals looked like before any of these files ran. Whatever they add is the
    // browser's and not the configuration's, so it joins the set `:config-source --clear` clears
    // back to — see `lua::add_to_baseline`, which carries the measurement for why the difference is
    // taken here rather than snapshotted afterwards.
    let before = crate::lua::global_names_now();

    for (name, path) in discover(dir) {
        if only.is_some_and(|wanted| wanted != name) {
            continue;
        }
        let file = path.join("init.lua");
        let source = match std::fs::read_to_string(&file) {
            Ok(source) => source,
            Err(e) => {
                let message = format!("{}: {e}", file.display());
                results.errors.push((name.clone(), message.clone()));
                remember_load_error(&name, &path, &message);
                continue;
            }
        };

        lock().plugins.insert(
            name.clone(),
            Plugin {
                dir: path.clone(),
                ..Plugin::default()
            },
        );

        // The leading `@` is Lua's "this is a file name": with it a throw reads
        // `…/plugins/x/init.lua:2: halfway`, without it `[string "…/plugins/x/ini..."]:2:`, where
        // Lua has truncated the middle of the path away. Naming the file is the whole requirement
        // for a plugin that will not load.
        let chunk_name = format!("@{}", file.display());
        let outcome = inside(&name, path.clone(), || {
            crate::lua::with(|lua| lua.load(&source).set_name(&chunk_name).exec())
        });
        match outcome {
            // No Lua state on this thread: a renderer, or a unit test. Nothing loaded, and nothing
            // is claimed to have — CEF-NOTES trap 13.
            None => {
                let message = "there is no Lua state on this thread".to_string();
                results.errors.push((name.clone(), message.clone()));
                remember_load_error(&name, &path, &message);
            }
            Some(Ok(())) => results.loaded.push(name.clone()),
            Some(Err(e)) => {
                let message = e.to_string();
                results.errors.push((name.clone(), message.clone()));
                remember_load_error(&name, &path, &message);
            }
        }
    }
    if let (Some(before), Some(after)) = (before, crate::lua::global_names_now()) {
        crate::lua::add_to_baseline(after.difference(&before).cloned().collect());
    }
    results
}

/// Drop what a load is about to replace: one plugin's registrations, or every plugin's.
fn forget(only: Option<&str>) {
    let mut registry = lock();
    match only {
        Some(name) => {
            registry.plugins.remove(name);
            registry.commands.retain(|_, (owner, _)| owner != name);
        }
        None => {
            registry.plugins.clear();
            registry.commands.clear();
        }
    }
}

fn remember_load_error(name: &str, dir: &Path, error: &str) {
    let mut registry = lock();
    // Whatever the half-run file registered before it threw goes with it: a plugin whose load did
    // not finish is a plugin whose commands were only half declared, and half is worse than none.
    registry.commands.retain(|_, (owner, _)| owner != name);
    let plugin = registry.plugins.entry(name.to_string()).or_default();
    plugin.dir = dir.to_path_buf();
    plugin.load_error = Some(error.to_string());
}

/// Startup: read the directory, unless `plugins.enabled` is off.
///
/// Reads the setting off the store `config.lua` has already filled — `app.rs` installs it before
/// this is called, so a `bru.set("plugins.enabled", false)` in the user's own file is honoured on
/// the same start it is written on rather than on the next one.
pub fn load_at_startup() {
    if !crate::settings::is_on("plugins.enabled") {
        eprintln!("bru[plugins]: plugins.enabled is off; no plugin was read");
        return;
    }
    let Some(dir) = plugins_dir() else {
        eprintln!("bru[plugins]: no data directory to read plugins from");
        return;
    };
    if !dir.is_dir() {
        // Not an error and not a message in the bar: an empty `~/.local/share/bru/plugins` is what
        // a bru that has never had a plugin looks like, and that is the normal case.
        eprintln!("bru[plugins]: {} does not exist", dir.display());
        return;
    }
    let results = load_from(&dir, None);
    eprintln!("bru[plugins]: {}", results.successful_str());
    if let Some(errors) = results.error_str() {
        eprintln!("bru[plugins]: {errors}");
    }
}

// ------------------------------------------------------------------------------------------------
// Running a plugin command
// ------------------------------------------------------------------------------------------------

/// What [`run`] decided, split off so a test can drive the bookkeeping without a browser behind it.
///
/// `macros::start`/`macros::begin` is the shape, and the reason is CEF-NOTES trap 13: `message::
/// error` posts a delayed task, and a unit test that reached it would take the whole test binary
/// down rather than fail.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The handler answered, and this is what to show. Empty means it said nothing.
    Said(String),
    /// It threw. The message, and whether this throw is the one that switched the plugin off.
    Threw { error: String, disabled: bool },
    /// It was not run: no such command, or the plugin is off.
    Refused(String),
}

/// `:<name> <args>` — one plugin command, with the error isolation around it.
pub fn run(name: &str, args: &str) {
    match run_inner(name, args) {
        Outcome::Said(text) if text.is_empty() => {}
        Outcome::Said(text) => crate::message::info(&text),
        Outcome::Threw { error, disabled } => {
            crate::message::error(&error);
            if disabled {
                crate::message::error(&format!(
                    "{name}: its plugin threw {THROW_LIMIT} times and is switched off for this \
                     session — fix it and run :plugin-reload"
                ));
            }
        }
        Outcome::Refused(why) => crate::message::error(&why),
    }
}

/// The half of [`run`] that touches no CEF.
pub fn run_inner(name: &str, args: &str) -> Outcome {
    let found = {
        let registry = lock();
        let Some((plugin, handler)) = registry.commands.get(name) else {
            return Outcome::Refused(format!("{name}: no plugin registers that command"));
        };
        if let Some(state) = registry.plugins.get(plugin) {
            if state.disabled {
                let why = state.reason.clone().unwrap_or_else(|| "disabled".to_string());
                return Outcome::Refused(format!("{name}: the plugin {plugin} is off — {why}"));
            }
        }
        let dir = registry
            .plugins
            .get(plugin)
            .map(|state| state.dir.clone())
            .unwrap_or_default();
        (plugin.clone(), handler.clone(), dir)
    };
    let (plugin, handler, dir) = found;

    match inside(&plugin, dir, || crate::lua::call_text(&handler, args)) {
        Ok(text) => Outcome::Said(text),
        Err(error) => {
            let disabled = record_throw(&plugin);
            Outcome::Threw {
                error: format!("{name}: {error}"),
                disabled,
            }
        }
    }
}

/// Count a throw, and answer whether this one is the one that switched the plugin off.
fn record_throw(plugin: &str) -> bool {
    let mut registry = lock();
    let Some(state) = registry.plugins.get_mut(plugin) else {
        return false;
    };
    state.throws += 1;
    if state.throws >= THROW_LIMIT && !state.disabled {
        state.disabled = true;
        state.reason = Some(format!("it threw {THROW_LIMIT} times"));
        return true;
    }
    false
}

// ------------------------------------------------------------------------------------------------
// The three commands
// ------------------------------------------------------------------------------------------------

/// `:plugin-list` — what is loaded, what each one registered, and what is wrong with the rest.
pub fn list() {
    let text = list_text();
    eprintln!("bru[plugins]: {text}");
    crate::message::info(&text);
}

/// [`list`] without the CEF half.
pub fn list_text() -> String {
    let registry = lock();
    if registry.plugins.is_empty() {
        return match plugins_dir() {
            Some(dir) => format!("No plugins. They are read from {}", dir.display()),
            None => "No plugins, and no data directory to read them from".to_string(),
        };
    }
    let lines: Vec<String> = registry
        .plugins
        .iter()
        .map(|(name, state)| {
            let mut commands: Vec<&str> = registry
                .commands
                .iter()
                .filter(|(_, (owner, _))| owner == name)
                .map(|(command, _)| command.as_str())
                .collect();
            commands.sort_unstable();
            let what = if commands.is_empty() {
                "no commands".to_string()
            } else {
                format!(":{}", commands.join(" :"))
            };
            match (&state.load_error, state.disabled) {
                (Some(error), _) => format!("{name} — not loaded: {error}"),
                (None, true) => format!(
                    "{name} — off ({}) — {what}",
                    state.reason.as_deref().unwrap_or("disabled")
                ),
                (None, false) => format!("{name} — {what}"),
            }
        })
        .collect();
    lines.join("; ")
}

/// `:plugin-reload [name]` — re-read one plugin or all of them.
pub fn reload(only: Option<&str>) {
    let Some(dir) = plugins_dir() else {
        crate::message::error("plugin-reload: no data directory to read plugins from");
        return;
    };
    if !dir.is_dir() {
        crate::message::info(&format!("No plugins: {} does not exist", dir.display()));
        return;
    }
    if let Some(name) = only {
        if !discover(&dir).iter().any(|(found, _)| found == name) {
            crate::message::error(&format!(
                "plugin-reload: there is no {name}/init.lua in {}",
                dir.display()
            ));
            return;
        }
    }
    // `load_from` moves the baseline itself, so a plugin that sets a global on a reload keeps that
    // global out of what `:config-source --clear` takes away.
    let results = load_from(&dir, only);
    crate::message::info(&results.successful_str());
    if let Some(errors) = results.error_str() {
        crate::message::error(&errors);
    }
    eprintln!("bru[plugins]: {}", results.successful_str());
}

/// `:plugin-disable <name>` — switch one off for the rest of the session.
///
/// Its commands stay registered and stay parseable, and say the plugin is off when they are typed.
/// Unregistering them instead would make `:hello` answer the sentence bru says about a name it has
/// never heard of — and this is a name it knows and is refusing.
pub fn disable(name: &str) {
    match disable_inner(name) {
        Ok(text) => crate::message::info(&text),
        Err(text) => crate::message::error(&text),
    }
}

/// [`disable`] without the CEF half.
pub fn disable_inner(name: &str) -> Result<String, String> {
    let mut registry = lock();
    let Some(state) = registry.plugins.get_mut(name) else {
        return Err(format!("plugin-disable: no plugin called {name} is loaded"));
    };
    if state.disabled {
        return Ok(format!("{name} was already off"));
    }
    state.disabled = true;
    state.reason = Some("switched off by :plugin-disable".to_string());
    Ok(format!(
        "{name} is off for this session; :plugin-reload {name} puts it back"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// **The registry is one static for the process and `cargo test` runs tests in parallel.**
    ///
    /// Every test below that puts something in it takes this first, so two of them cannot each see
    /// half of the other's registry. Without it these pass alone and fail together, which is the
    /// worst shape a test can have.
    fn serial() -> MutexGuard<'static, ()> {
        static SERIAL: Mutex<()> = Mutex::new(());
        SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A plugin tree in a scratch directory of this test run.
    struct TempPlugins {
        dir: PathBuf,
    }

    impl TempPlugins {
        fn new(name: &str) -> TempPlugins {
            let dir =
                std::env::temp_dir().join(format!("bru-plugin-test-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("could not create the test directory");
            TempPlugins { dir }
        }

        fn add(&self, name: &str, source: &str) {
            let dir = self.dir.join(name);
            std::fs::create_dir_all(&dir).expect("could not create the plugin directory");
            let mut f =
                std::fs::File::create(dir.join("init.lua")).expect("could not write init.lua");
            f.write_all(source.as_bytes()).expect("could not write init.lua");
        }

        /// A directory that is not a plugin, because it has no `init.lua`.
        fn add_bare(&self, name: &str) {
            std::fs::create_dir_all(self.dir.join(name)).expect("could not create the directory");
        }
    }

    impl Drop for TempPlugins {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// Discovery is `<dir>/<name>/init.lua`, in name order, and nothing else is a plugin.
    #[test]
    fn a_plugin_is_a_directory_with_an_init_lua_and_they_load_in_name_order() {
        let tree = TempPlugins::new("discover");
        tree.add("zeta", "");
        tree.add("alpha", "");
        tree.add_bare("requires");
        // A loose file beside them is not a plugin either.
        std::fs::write(tree.dir.join("notes.txt"), "x").unwrap();

        let found = discover(&tree.dir);
        let names: Vec<&str> = found.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["alpha", "zeta"], "sorted, and nothing else is a plugin");
        assert_eq!(found[0].1, tree.dir.join("alpha"));

        // A directory that does not exist is no plugins, not an error: that is what a bru which has
        // never had one looks like.
        assert!(discover(&tree.dir.join("nowhere")).is_empty());
    }

    /// **The registry answers before there is a Lua state, and that is what `commands::parse`
    /// needs.** Every unit test in bru parses command strings; if `is_registered` panicked or
    /// blocked off the UI thread, every one of them would.
    #[test]
    fn nothing_is_registered_when_no_plugin_has_loaded() {
        let _serial = serial();
        forget(None);
        assert!(!is_registered("hello"));
        assert!(!is_registered(""));
        assert_eq!(
            run_inner("hello", ""),
            Outcome::Refused("hello: no plugin registers that command".to_string())
        );
    }

    /// **The whole of P3, driven through the real registry.**
    ///
    /// A ten-line plugin registering `:hello`, a second that throws, and the third throw switching
    /// it off. The `--plugin-dir` and `--cmd` versions of these are in the report; this is the half
    /// that can be checked without a browser.
    #[test]
    fn a_plugin_registers_a_command_and_three_throws_switch_it_off() {
        let _serial = serial();
        let tree = TempPlugins::new("registers");
        tree.add(
            "hello",
            "bru.command('hello', function(args)\n  return 'hello ' .. args\nend)\n",
        );
        tree.add("boom", "bru.command('boom', function() error('no') end)\n");
        // The state, as `config::apply_lua` makes it — this thread has one from here on.
        let _ = crate::lua::shared();

        let results = load_from(&tree.dir, None);
        assert_eq!(results.loaded, ["boom", "hello"], "{results:?}");
        assert!(results.errors.is_empty(), "{results:?}");
        assert!(is_registered("hello"));
        assert_eq!(
            command_names(),
            [
                ("boom".to_string(), "boom".to_string()),
                ("hello".to_string(), "hello".to_string())
            ]
        );

        // The handler runs, and what it returns is what the bar would show.
        assert_eq!(run_inner("hello", "world"), Outcome::Said("hello world".to_string()));

        // A throw is a message, and the second one is another message, and the third switches the
        // plugin off. Nothing here takes the process down, which is the claim.
        for expected in [false, false, true] {
            match run_inner("boom", "") {
                Outcome::Threw { error, disabled } => {
                    assert!(error.starts_with("boom: "), "{error}");
                    assert!(error.contains("no"), "{error}");
                    assert_eq!(disabled, expected, "{error}");
                }
                other => panic!("expected a throw, got {other:?}"),
            }
        }
        // And once it is off it is refused rather than run again.
        match run_inner("boom", "") {
            Outcome::Refused(why) => assert!(why.contains("is off"), "{why}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
        // The other plugin is untouched — the isolation is per plugin and not per process.
        assert_eq!(run_inner("hello", "again"), Outcome::Said("hello again".to_string()));
        assert!(list_text().contains("boom — off (it threw 3 times)"), "{}", list_text());

        // `:plugin-disable` on the working one, and what it says when it is already off.
        assert!(disable_inner("hello").unwrap().contains("is off for this session"));
        assert_eq!(disable_inner("hello"), Ok("hello was already off".to_string()));
        match run_inner("hello", "") {
            Outcome::Refused(why) => assert!(why.contains("switched off by :plugin-disable"), "{why}"),
            other => panic!("expected a refusal, got {other:?}"),
        }

        // A reload puts both back, throw count and disable and all.
        let results = load_from(&tree.dir, None);
        assert_eq!(results.loaded, ["boom", "hello"]);
        assert_eq!(run_inner("hello", "x"), Outcome::Said("hello x".to_string()));
        forget(None);
    }

    /// A plugin that throws while loading is named with its file and skipped, and the ones beside it
    /// still load.
    #[test]
    fn a_plugin_that_throws_while_loading_is_named_and_the_others_load() {
        let _serial = serial();
        let _ = crate::lua::shared();
        let tree = TempPlugins::new("load-throws");
        tree.add("aaa", "bru.command('aaa', function() return 'a' end)\n");
        // Registers one command and *then* throws: what it half-declared must not survive.
        tree.add(
            "bbb",
            "bru.command('bbb', function() return 'b' end)\nerror('halfway')\n",
        );
        tree.add("ccc", "bru.command('ccc', function() return 'c' end)\n");

        let results = load_from(&tree.dir, None);
        assert_eq!(results.loaded, ["aaa", "ccc"], "{results:?}");
        assert_eq!(results.errors.len(), 1, "{results:?}");
        assert_eq!(results.errors[0].0, "bbb");
        assert!(
            results.errors[0].1.contains("init.lua"),
            "the error names the file: {:?}",
            results.errors[0].1
        );
        assert!(is_registered("aaa") && is_registered("ccc"));
        assert!(!is_registered("bbb"), "a half-loaded plugin registers nothing");
        assert!(list_text().contains("bbb — not loaded:"), "{}", list_text());
        forget(None);
    }

    /// A plugin may not take over a name bru ships, because `commands::parse` would never reach it.
    #[test]
    fn a_plugin_cannot_take_over_one_of_brus_own_commands() {
        let _serial = serial();
        let _ = crate::lua::shared();
        let tree = TempPlugins::new("collide");
        tree.add("greedy", "bru.command('open', function() end)\n");
        let results = load_from(&tree.dir, None);
        assert_eq!(results.errors.len(), 1, "{results:?}");
        assert!(
            results.errors[0].1.contains("may not take it over"),
            "{:?}",
            results.errors[0].1
        );
        forget(None);
    }

    /// A load with no Lua state on this thread reports that rather than claiming success — CEF-NOTES
    /// trap 13 from the other side: nothing here reaches libcef, and nothing here lies about it.
    #[test]
    fn loading_without_a_lua_state_names_the_plugin_and_registers_nothing() {
        let _serial = serial();
        let tree = TempPlugins::new("no-state");
        tree.add("hello", "bru.command('hello', function() return 'hi' end)");
        // A thread that never asked for a state, which is what a renderer process is.
        let results = std::thread::spawn({
            let dir = tree.dir.clone();
            move || load_from(&dir, None)
        })
        .join()
        .expect("the thread must not panic");

        assert!(results.loaded.is_empty(), "{results:?}");
        assert_eq!(results.errors.len(), 1);
        assert_eq!(results.errors[0].0, "hello");
        assert!(!is_registered("hello"));
        assert!(results.error_str().unwrap().starts_with("Plugins not loaded — hello: "));
        assert!(list_text().contains("hello — not loaded:"), "{}", list_text());
        forget(None);
    }

    /// `:plugin-disable` on a name nothing registered says so rather than inventing a plugin.
    #[test]
    fn disabling_something_that_is_not_loaded_says_so() {
        let _serial = serial();
        forget(None);
        assert_eq!(
            disable_inner("nope"),
            Err("plugin-disable: no plugin called nope is loaded".to_string())
        );
    }

    /// The empty case of `:plugin-list` names the directory it looked in, which is the same
    /// information as creating it would give and has no side effect.
    #[test]
    fn the_empty_list_names_the_directory() {
        let _serial = serial();
        forget(None);
        let text = list_text();
        assert!(text.starts_with("No plugins"), "{text}");
    }
}
