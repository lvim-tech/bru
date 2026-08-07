//! The Lua state that outlives `Config::load`, and the handles a setting keeps into it.
//!
//! **This is the second file in bru that may mention `mlua`**, and the first is `config.rs`. It
//! exists because of one sentence in DESIGN.md: `~/.config/bru/config.lua` is Lua "so that a
//! setting can hold a function, not only a scalar". A function is not a value that can be copied
//! out of the interpreter and kept — it *is* the interpreter — so the moment a setting may hold one,
//! the state has to live as long as the setting does.
//!
//! What the rest of bru sees is [`FnRef`], which is opaque: an id and the place the function was
//! written. `settings.rs` stores one in a `Value` and never names `mlua`, which is what keeps the
//! type that is neither `Send` nor `Sync` out of a `static Mutex` it could not live in anyway.
//!
//! **Nothing here is on the key path.** `j` reaches `scroll::step` and an atomic; the one thing that
//! reaches this file is a strip rebuild, which happens when a tab is opened, closed, selected or
//! retitled.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

/// One Lua state and the functions settings and plugins hold into it.
///
/// They are one struct rather than three thread-locals because they have to die together. An
/// `mlua::Function` is a weak reference into the state; calling one whose `Lua` has been dropped is
/// a panic inside mlua, not an error bru could report.
struct Runtime {
    /// The functions handed over so far, by [`FnRef`]'s id.
    functions: HashMap<u64, mlua::Function>,
    /// The next id. Never reused, so a stale [`FnRef`] answers "gone" rather than somebody else's
    /// function.
    next_id: u64,
    lua: mlua::Lua,
}

thread_local! {
    /// The state, on whichever thread first asked for one. In the browser that is the CEF UI thread,
    /// because `Config::load` is called from `on_context_initialized` and nothing else builds a
    /// config. A renderer process never runs a config, so it never has one — which is the whole of
    /// why [`with`] can answer `None`.
    static RUNTIME: RefCell<Option<Runtime>> = const { RefCell::new(None) };
}

/// A handle to a Lua function bru is holding — a setting's value, or a plugin's command handler.
///
/// Opaque on purpose: `settings.rs` must not name `mlua`, and this is the type that lets it not.
/// `Clone`, `Send` and `Sync` because a [`crate::settings::Value`] is all three — the store is a
/// `static Mutex` reached from several threads. None of that makes the *function* reachable from
/// another thread: [`call_string`] looks the id up in this thread's [`RUNTIME`] and finds nothing on
/// a thread that never loaded a config, which is the renderer case and the unit-test case at once.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FnRef {
    id: u64,
    origin: Arc<str>,
}

/// One argument handed to a Lua function.
///
/// A closed enum rather than `mlua::Value` because the callers are `plugins.rs`, `tabs.rs` and
/// `editor.rs`, and none of them may name `mlua`.
#[derive(Clone, Debug)]
pub enum Arg {
    Text(String),
    Int(i64),
    Bool(bool),
}

/// Run `f` against the shared Lua state. `None` when this thread has none.
///
/// **`None` is an answer, not a failure.** A renderer process and a `cargo test` binary both look
/// like a thread that never loaded a config, and both have to get something back rather than a
/// panic — CEF-NOTES trap 13. Every reader in `settings.rs` falls back to bru's compiled-in default
/// when this answers `None`, which is what makes a function-valued setting safe to read from a
/// process that has no interpreter.
///
/// The `Lua` is **cloned out and the borrow released** before `f` runs. `mlua::Lua` is an `Rc` to
/// the real state, so the clone is the same interpreter; holding the `RefCell` borrow across `f`
/// would make `bru.set` inside a config file re-enter this file and panic on the borrow.
// Nothing on this branch calls it: `settings.rs` reaches Lua through `call_string_fields`, which
// is the whole of P2's need. It is here because it is the contract — `PLUGIN-CONTRACTS.md` names
// `with` as what workstreams B and C may assume — and because deleting the one function the other
// two branches are written against would make the merge look like a decision.
#[allow(dead_code)]
pub fn with<R>(f: impl FnOnce(&mlua::Lua) -> R) -> Option<R> {
    let lua = RUNTIME.with(|cell| cell.borrow().as_ref().map(|state| state.lua.clone()))?;
    Some(f(&lua))
}

/// The shared state, made if this thread has not got one yet.
///
/// `config.rs` used to write `mlua::Lua::new()` and drop it at the end of `apply_lua`. The clone is
/// an `Rc` clone of the same interpreter and its `Drop` does not collect, so the state survives the
/// caller by design rather than by luck.
pub fn shared() -> mlua::Lua {
    RUNTIME.with(|cell| {
        let mut cell = cell.borrow_mut();
        cell.get_or_insert_with(|| Runtime {
            functions: HashMap::new(),
            next_id: 1,
            lua: mlua::Lua::new(),
        })
        .lua
        .clone()
    })
}

/// Keep a Lua function and answer the handle bru stores.
///
/// `None` when this thread has no state, which cannot happen from a caller that has just run a
/// chunk in it, but is answered rather than asserted so that a second caller cannot make it a panic.
pub fn register(function: &mlua::Function) -> Option<FnRef> {
    // `info()` before the borrow, because it locks the Lua and a callback could in principle reach
    // back in here. It cannot today; the ordering costs nothing and removes the question.
    let info = function.info();
    let origin = match (info.short_src.as_deref().map(unwrap_chunk), info.line_defined) {
        (Some(source), Some(line)) => format!("{source}:{line}"),
        (Some(source), None) => source.to_string(),
        _ => "?".to_string(),
    };
    RUNTIME.with(|cell| {
        let mut cell = cell.borrow_mut();
        let state = cell.as_mut()?;
        let id = state.next_id;
        state.next_id += 1;
        state.functions.insert(id, function.clone());
        Some(FnRef { id, origin: origin.into() })
    })
}

/// `[string "config.lua"]` as `config.lua`.
///
/// **Lua's own wrapper, and it has to come off here rather than at the load.** A chunk loaded from a
/// string gets a `short_src` of `[string "<name>"]` unless its name starts with `@` or `=`, which is
/// the convention `luaL_loadfile` uses and `Lua::load(&str).set_name(path)` does not. Changing the
/// name to `@path` in `config.rs` would fix this *and* rewrite every error message a config file has
/// ever produced, which is another workstream's file and a user-visible string. Taking the wrapper
/// off one value that one place prints is the smaller change, and the error text keeps Lua's own
/// spelling — so `bru[error]: tabs.title.format: <function config.lua:12>: [string
/// "config.lua"]:13: attempt to index a nil value` names the same file twice in the two vocabularies
/// a person will meet it in.
fn unwrap_chunk(source: &str) -> &str {
    source
        .strip_prefix("[string \"")
        .and_then(|rest| rest.strip_suffix("\"]"))
        .unwrap_or(source)
}

/// The function behind a handle, cloned out with the borrow released.
///
/// The release is not tidiness: a handler is allowed to call `bru.set` and a setting's function is
/// allowed to call `bru.cmd`, and both reach this file again.
fn taken(handle: &FnRef) -> Result<(mlua::Lua, mlua::Function), String> {
    RUNTIME
        .with(|cell| {
            let cell = cell.borrow();
            let state = cell.as_ref()?;
            let function = state.functions.get(&handle.id)?.clone();
            Some((state.lua.clone(), function))
        })
        .ok_or_else(|| {
            format!(
                "{handle} is not reachable from this process — it was written in a config this \
                 process never loaded"
            )
        })
}

/// What a Lua function answered, as the string bru wanted.
///
/// **A number is accepted and a table is not.** Lua concatenates a number into a string without
/// being asked, so a function that answers `42` for a tab title means the tab title `42`; a function
/// that answers a table means the config is wrong, and the error names the type. Neither is a panic:
/// the whole point of Lua over a native plugin (PLUGINS.md's first table) is that a mistake here is
/// a message and the browser goes on.
fn answered(handle: &FnRef, answer: mlua::Result<mlua::Value>) -> Result<String, String> {
    match answer {
        Ok(mlua::Value::String(text)) => text
            .to_str()
            .map(|text| text.to_string())
            .map_err(|e| format!("{handle}: its answer is not text bru can read: {e}")),
        Ok(mlua::Value::Integer(number)) => Ok(number.to_string()),
        Ok(mlua::Value::Number(number)) => Ok(number.to_string()),
        Ok(mlua::Value::Nil) => Ok(String::new()),
        Ok(other) => Err(format!(
            "{handle}: returned {}, and what asked for it wanted a string",
            other.type_name()
        )),
        Err(e) => Err(format!("{handle}: {e}")),
    }
}

/// One [`Arg`] as the Lua value it becomes.
fn into_lua(lua: &mlua::Lua, handle: &FnRef, arg: &Arg) -> Result<mlua::Value, String> {
    match arg {
        Arg::Text(text) => lua
            .create_string(text.as_str())
            .map(mlua::Value::String)
            .map_err(|e| format!("{handle}: could not pass {text:?}: {e}")),
        Arg::Int(number) => Ok(mlua::Value::Integer(*number)),
        Arg::Bool(flag) => Ok(mlua::Value::Boolean(*flag)),
    }
}

// --- setting functions ---------------------------------------------------------------------------
// **Workstream C's block.** Everything above belongs to workstream A's P1 and P3; what is here is
// what P2 — a setting whose value is a Lua function — needs and nothing else. Two additions:
//
// - `call_string_fields`, which passes **one table of named fields** rather than positional
//   arguments. A plugin command takes one string and `call_string` is right for it; a setting's
//   function takes a *record* — a tab has an index, a title, a URL and two flags — and the shape
//   that survives a sixth field being added is a table, not a sixth parameter.
// - `Display` for `FnRef`, which is what `:set` and `bru://chrome/settings` print. See it below for
//   why they print that and not a value.
//
// Both are additive: nothing above changes, and `plugins.rs` reaches none of it.

impl std::fmt::Display for FnRef {
    /// **What a function-valued setting prints**, everywhere one line is wanted.
    ///
    /// It is not the value and does not pretend to be. A function has no value until it is called,
    /// and for `tabs.title.format` what it answers depends on which tab is being drawn — so there
    /// is no single answer to print, and printing one call's result would be printing a string that
    /// was never used by anything. The two things that can honestly be said are that it is a
    /// function and where it was written, and the second is the only one a person who wants to
    /// change it needs.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<function {}>", self.origin)
    }
}

/// Call a setting's function with **named fields**, as one table, and take a string back.
///
/// `function(tab) return tab.title:upper() end` reading `tab.title` survives a field being added
/// beside it; `function(index, title, url, pinned, muted)` does not, and a sixth field would silently
/// shift every config in the world by one. It is also what makes the error worth reading: a config
/// that spells a field wrong gets `nil`, and a `nil` in a concatenation names the line it was on.
///
/// The error case is [`answered`]'s and is shared with [`call_string`] on purpose — a setting's
/// function and a plugin's handler fail in exactly the same ways and should say so in the same
/// words.
pub fn call_string_fields(
    handle: &FnRef,
    fields: &[(&'static str, Arg)],
) -> Result<String, String> {
    let (lua, function) = taken(handle)?;
    let table = lua
        .create_table()
        .map_err(|e| format!("{handle}: could not build its argument: {e}"))?;
    for (name, value) in fields {
        let value = into_lua(&lua, handle, value)?;
        table
            .set(*name, value)
            .map_err(|e| format!("{handle}: could not set {name}: {e}"))?;
    }
    answered(handle, function.call::<mlua::Value>(table))
}

/// Forget every function except the ones still named by a handle somebody is holding.
///
/// **The caller has to know every live handle, and that is the whole of the danger.** Anything
/// registered and not in `live` is gone: its id is never reused, so a holder that was left out
/// reads as "not reachable" from then on and falls back to a default. So the rule is one sentence
/// and it belongs at every call site: *pass every handle every registrar is holding, not only your
/// own.* Today that is `Settings`'s function-valued values; when plugins land it is those and the
/// command registry's handlers.
///
/// Two callers want it. `:config-source --clear` means "back to bru's own", and a function nothing
/// names any more is part of what is being cleared. And **every** `:config-source`, with or without
/// `--clear`, because re-running a config registers a second function for each setting it sets and
/// the first one is unreachable the moment the store is overwritten — without this the map grows by
/// one entry per function-valued setting per source, for the life of the process.
pub fn forget_functions_except(live: &[FnRef]) {
    RUNTIME.with(|cell| {
        if let Some(state) = cell.borrow_mut().as_mut() {
            state
                .functions
                .retain(|id, _| live.iter().any(|handle| handle.id == *id));
        }
    });
}
// --- end setting functions -----------------------------------------------------------------------

// --- setting functions ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    /// Build a function in the shared state, the way `config.rs` does, and hand back its handle.
    fn register_source(source: &str) -> FnRef {
        let lua = shared();
        let function: mlua::Function = lua.load(source).set_name("config.lua").eval().unwrap();
        register(&function).expect("the state was just made")
    }

    /// The whole of the contract in one test: a function goes in, a string comes out, and the
    /// fields arrive as a table.
    #[test]
    fn a_function_takes_a_table_of_fields_and_answers_a_string() {
        let handle = register_source("return function(tab) return tab.title:upper() end");
        let answer = call_string_fields(
            &handle,
            &[("title", Arg::Text("example".to_string())), ("index", Arg::Int(1))],
        );
        assert_eq!(answer, Ok("EXAMPLE".to_string()));
    }

    /// Every field shape, and the numbers arriving as numbers rather than as text — `tab.index + 1`
    /// has to work, or the table is a table of strings with extra steps.
    #[test]
    fn the_fields_keep_their_lua_types() {
        let handle = register_source(
            "return function(t) return type(t.index) .. ',' .. type(t.pinned) .. ',' .. \
             type(t.title) .. ',' .. tostring(t.index + 1) end",
        );
        let answer = call_string_fields(
            &handle,
            &[
                ("index", Arg::Int(3)),
                ("pinned", Arg::Bool(true)),
                ("title", Arg::Text("x".to_string())),
            ],
        );
        assert_eq!(answer, Ok("number,boolean,string,4".to_string()));
    }

    /// **A throw is a message, not a crash.** This is the row that decided Lua over a native plugin
    /// in PLUGINS.md, checked at the boundary rather than quoted from the table.
    #[test]
    fn a_function_that_throws_is_an_error_naming_where_it_was_written() {
        let handle = register_source("return function() error('no') end");
        let error = call_string_fields(&handle, &[]).unwrap_err();
        // bru's own prefix says which function it was, in the spelling `:set` prints; Lua's own
        // message follows it, in Lua's spelling, and both are kept — see [`unwrap_chunk`].
        assert!(error.starts_with("<function config.lua:1>: "), "{error}");
        assert!(error.contains("no"), "{error}");
    }

    /// A function that answers something that is not a string. The two useful cases are told apart:
    /// a number is a title, a table is a mistake.
    #[test]
    fn a_number_is_taken_and_a_table_is_refused_by_name() {
        assert_eq!(
            call_string_fields(&register_source("return function() return 42 end"), &[]),
            Ok("42".to_string())
        );
        let error =
            call_string_fields(&register_source("return function() return {} end"), &[])
                .unwrap_err();
        assert!(error.contains("returned table"), "{error}");
    }

    /// **The case a renderer process is in**, and the one that must not panic: a handle registered
    /// on one thread, read on another that never loaded a config.
    #[test]
    fn a_handle_is_not_reachable_from_a_thread_with_no_state() {
        let handle = register_source("return function() return 'here' end");
        assert_eq!(call_string_fields(&handle, &[]), Ok("here".to_string()));

        let elsewhere = std::thread::spawn(move || call_string_fields(&handle, &[]))
            .join()
            .expect("the thread must not panic");
        let error = elsewhere.unwrap_err();
        assert!(error.contains("never loaded"), "{error}");
    }

    /// `with` answers `None` on a thread that has no state, which is the same claim one level up.
    #[test]
    fn with_answers_none_where_there_is_no_state() {
        let answer = std::thread::spawn(|| with(|_| 1_i32))
            .join()
            .expect("the thread must not panic");
        assert_eq!(answer, None);
    }

    /// The handle prints where the function was written, because it cannot print what it is.
    #[test]
    fn a_handle_prints_its_file_and_line() {
        let handle = register_source("\n\nreturn function() return 'x' end");
        assert_eq!(handle.to_string(), "<function config.lua:3>");
    }
}
// --- end setting functions -----------------------------------------------------------------------
