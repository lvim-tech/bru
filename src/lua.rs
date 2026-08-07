//! The one long-lived Lua state, and the `bru` table plugins register against.
//!
//! **This file belongs to workstream A (`lua runtime`).** Everything in it is fenced as
//! `plugin events` because workstream B needed a state, a `FnRef` and a `call_unit` to compile and
//! to be measured against a running browser, and A's version did not exist in B's tree. When the two
//! are merged, only the `bru.on` block and the `Arg` type need to survive from here — see this
//! workstream's report, which names the lines.

// --- plugin events ------------------------------------------------------------------------------
// SCAFFOLDING. If A's `src/lua.rs` exists, take A's file and lift only:
//   * `pub enum Arg` and its `debug`/`into_lua` (the payload type `events.rs` speaks),
//   * `fn call_unit`'s table-building body, if A's takes a different argument shape,
//   * the `bru.on` entry in the `bru` table.
// Everything else below is a minimum stand-in for A's state, loader and `--plugin-dir`.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// A value handed to a Lua handler. **The boundary type**: `events.rs` builds these and never names
/// `mlua`, exactly as `settings.rs` must not.
#[derive(Clone, Debug, PartialEq)]
pub enum Arg {
    Text(String),
    Int(i64),
    Bool(bool),
    /// A field bru genuinely does not know — the window of a download whose browser has gone. It is
    /// `nil` and never a fabricated `0`, because window 0 is a real window.
    Nil,
}

impl Arg {
    /// For `BRU_DEBUG_EVENTS=1`. Quoted like Lua would write it, so a trace line can be pasted back.
    pub fn debug(&self) -> String {
        match self {
            Arg::Text(text) => format!("{text:?}"),
            Arg::Int(n) => n.to_string(),
            Arg::Bool(b) => b.to_string(),
            Arg::Nil => "nil".to_string(),
        }
    }

    fn into_lua(self, lua: &mlua::Lua) -> mlua::Result<mlua::Value> {
        Ok(match self {
            Arg::Text(text) => mlua::Value::String(lua.create_string(&text)?),
            Arg::Int(n) => mlua::Value::Integer(n),
            Arg::Bool(b) => mlua::Value::Boolean(b),
            Arg::Nil => mlua::Value::Nil,
        })
    }
}

/// An opaque handle to a Lua function. Nothing outside this file may see what is inside it.
///
/// `Rc` rather than a bare key because `events.rs` clones the handler list out of its registry
/// before dispatching — holding the registry's borrow across a Lua call would turn a handler that
/// calls `bru.on` into a panic.
#[derive(Clone)]
pub struct FnRef(Rc<mlua::RegistryKey>);

thread_local! {
    /// `mlua::Lua` is neither `Send` nor `Sync` with bru's features, so this cannot be a
    /// `static Mutex<Lua>` — the type system decides where it lives. It is created on the CEF UI
    /// thread and is `None` everywhere else, which is also what makes `with` answer `None` in a
    /// renderer process and in a unit test (CEF-NOTES trap 13) rather than reaching for libcef to
    /// ask which thread it is on.
    static LUA: RefCell<Option<mlua::Lua>> = const { RefCell::new(None) };

    /// The plugin whose `init.lua` is running, so `bru.on` can record who registered a handler.
    static LOADING: RefCell<Option<(String, PathBuf)>> = const { RefCell::new(None) };
}

/// Run `f` against the shared state. `None` off the UI thread and before `boot`.
pub fn with<T>(f: impl FnOnce(&mlua::Lua) -> T) -> Option<T> {
    LUA.with(|lua| lua.borrow().as_ref().map(f))
}

/// Call a handler with one table argument built from `fields`.
///
/// The error is Lua's own message, flattened to a string, because that is what reaches the bar and
/// nothing above this file should have to know an `mlua::Error` to print it.
pub fn call_unit(function: &FnRef, fields: &[(&'static str, Arg)]) -> Result<(), String> {
    let result = with(|lua| -> mlua::Result<()> {
        let handler: mlua::Function = lua.registry_value(&function.0)?;
        let table = lua.create_table()?;
        for (key, value) in fields {
            table.set(*key, value.clone().into_lua(lua)?)?;
        }
        handler.call::<()>(table)
    });
    match result {
        Some(Ok(())) => Ok(()),
        Some(Err(error)) => Err(error.to_string()),
        None => Err("there is no Lua state on this thread".to_string()),
    }
}

// ------------------------------------------------------------------------------------------------
// Standing the state up, and loading plugins
// ------------------------------------------------------------------------------------------------

/// `~/.local/share/bru/plugins`, or whatever `--plugin-dir` named.
///
/// The directory is never created, for `greasemonkey::scripts_dir`'s reason: a browser that makes a
/// directory the user did not ask for writes to `~/.local/share` on every start.
pub fn plugins_dir(override_path: Option<&str>) -> Option<PathBuf> {
    match override_path {
        Some(path) if !path.is_empty() => Some(PathBuf::from(path)),
        _ => Some(crate::data::data_dir()?.join("plugins")),
    }
}

/// Create the state, build the `bru` table and load every plugin, in directory-name order.
///
/// Called from `app.rs::on_context_initialized`, before the first window exists, so that a plugin
/// watching `page-loaded` sees the start page load.
pub fn boot(override_path: Option<&str>) {
    // `unsafe_new` because `debug` is in — PLUGINS.md's second settled decision. Not because it is
    // safe, but because the sandbox it would protect does not exist: `bru.cmd(":spawn …")` starts
    // processes.
    let lua = unsafe { mlua::Lua::unsafe_new() };
    if let Err(error) = install_bru_table(&lua) {
        eprintln!("bru: could not build the bru table: {error}");
        return;
    }
    LUA.with(|slot| *slot.borrow_mut() = Some(lua));

    let Some(dir) = plugins_dir(override_path) else {
        return;
    };
    load_all(&dir);
}

/// Every `<dir>/<name>/init.lua`, in name order. A plugin that throws during load is named and
/// skipped.
fn load_all(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        // Not an error: a bru with no plugins is the normal case.
        return;
    };
    let mut names: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect();
    names.sort();

    for name in names {
        let init = dir.join(&name).join("init.lua");
        let Ok(source) = std::fs::read_to_string(&init) else {
            continue;
        };
        LOADING.with(|loading| *loading.borrow_mut() = Some((name.clone(), dir.join(&name))));
        let result = with(|lua| {
            lua.load(&source)
                .set_name(init.display().to_string())
                .exec()
        });
        LOADING.with(|loading| *loading.borrow_mut() = None);
        match result {
            Some(Ok(())) => eprintln!("bru[plugins]: loaded {name}"),
            Some(Err(error)) => eprintln!("bru[plugins]: {} skipped: {error}", init.display()),
            None => eprintln!("bru[plugins]: no Lua state to load {name} into"),
        }
    }
}

/// The plugin `bru.on` is being called from. Anything registered outside a plugin's own load — from
/// `config.lua`, say — is attributed to `config`, so the throw counter always has a name to count
/// against.
fn current_plugin() -> String {
    LOADING
        .with(|loading| loading.borrow().as_ref().map(|(name, _)| name.clone()))
        .unwrap_or_else(|| "config".to_string())
}

fn install_bru_table(lua: &mlua::Lua) -> mlua::Result<()> {
    let bru = lua.create_table()?;

    // --- the event registration, workstream B's own -------------------------------------------
    // `bru.on(event, fn)`. The function is put in the registry and only its key leaves this file.
    bru.set(
        "on",
        lua.create_function(|lua, (event, handler): (String, mlua::Function)| {
            let key = lua.create_registry_value(handler)?;
            crate::events::register(&event, &current_plugin(), FnRef(Rc::new(key)))
                .map_err(mlua::Error::RuntimeError)
        })?,
    )?;
    // --- end the event registration -------------------------------------------------------------

    // `bru.cmd(line)` — **posts** a UI task rather than running inline. CEF-NOTES trap 12: no call
    // that creates a browser or starts a navigation may run inside a message-router query handler,
    // and a plugin handler can be reached from one.
    bru.set(
        "cmd",
        lua.create_function(|_, line: String| {
            crate::exec::run_from_cmdline(line.trim_start_matches(':'), None);
            Ok(())
        })?,
    )?;

    bru.set(
        "message",
        lua.create_function(|_, text: String| {
            crate::message::info(&text);
            Ok(())
        })?,
    )?;

    bru.set(
        "error",
        lua.create_function(|_, text: String| {
            crate::message::error(&text);
            Ok(())
        })?,
    )?;

    bru.set(
        "get",
        lua.create_function(|_, name: String| Ok(crate::settings::value_of(&name)))?,
    )?;

    bru.set(
        "data_dir",
        lua.create_function(|_, ()| {
            Ok(crate::data::data_dir()
                .map(|dir| dir.display().to_string())
                .unwrap_or_default())
        })?,
    )?;

    bru.set(
        "plugin_dir",
        lua.create_function(|_, ()| {
            Ok(LOADING
                .with(|loading| {
                    loading
                        .borrow()
                        .as_ref()
                        .map(|(_, dir)| dir.display().to_string())
                })
                .unwrap_or_default())
        })?,
    )?;

    lua.globals().set("bru", bru)?;
    Ok(())
}

/// One real crossing of the Lua boundary, of exactly the shape `call_unit` makes: a fresh table
/// with a URL and a window in it, handed to a Lua function. Answers the nanoseconds per call.
///
/// It lives here because this is one of the two files allowed to name `mlua`, and
/// `events::the_cost_of_an_event_nobody_asked_for` needs the number to compare its branch against.
/// A stand-in would have been the thing this project calls a harness that reproduces its own
/// assumption; `mlua` runs perfectly well under `cargo test` — it is CEF that does not.
#[cfg(test)]
pub fn nanoseconds_per_crossing(rounds: u32, warmup: u32, url: &str) -> f64 {
    let lua = unsafe { mlua::Lua::unsafe_new() };
    let handler: mlua::Function = lua
        .load("local n = 0 return function(e) n = n + 1 return nil end")
        .eval()
        .expect("the handler compiles");
    let once = || {
        let table = lua.create_table().expect("a table");
        table.set("url", url).expect("url");
        table.set("window", 0i64).expect("window");
        handler.call::<()>(table).expect("the handler runs");
    };
    for _ in 0..warmup {
        once();
    }
    let start = std::time::Instant::now();
    for _ in 0..rounds {
        once();
    }
    start.elapsed().as_nanos() as f64 / f64::from(rounds)
}
// --- end plugin events --------------------------------------------------------------------------
