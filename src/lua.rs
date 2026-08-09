//! The one Lua state, for the whole process, for as long as the process lives.
//!
//! **This and `src/config.rs` are the only two files in bru that may mention `mlua`.** Everything
//! else reaches Lua through the functions below and through [`FnRef`], which is opaque on purpose:
//! `settings.rs` holds handles to Lua functions and `plugins.rs` holds handles to command handlers,
//! and neither may name a Lua type — or the rule that keeps the interpreter off the key path
//! becomes a matter of discipline instead of a matter of what compiles.
//!
//! It exists because of one sentence in DESIGN.md: `~/.config/bru/config.lua` is Lua "so that a
//! setting can hold a function, not only a scalar". A function is not a value that can be copied
//! out of the interpreter and kept — it *is* the interpreter — so the moment a setting may hold one,
//! the state has to live as long as the setting does. Until P1 `config.rs` built a `Lua`, ran the
//! config file and dropped it before any browser existed; what changed is the **lifetime** and
//! nothing else.
//!
//! # Why a `thread_local!` and not a `static Mutex<Lua>`
//!
//! `mlua::Lua` is **neither `Send` nor `Sync`** with bru's features (`lua54`, `vendored`) — its
//! inner handle is an `Rc<ReentrantMutex<RawLua>>`. Checked by compiling `fn is_send<T: Send>()`
//! against it, which does not compile. So the obvious global does not exist, and that is the good
//! kind of constraint: it cannot be worked around by accident. The state lives on **the CEF UI
//! thread** and nowhere else.
//!
//! Every other thread — CEF's IO thread, a renderer process, a `cargo test` binary — finds the
//! thread-local empty and gets `None` back. That is deliberate and it is CEF-NOTES **trap 13**: a
//! unit test must get an *answer* rather than an abort, and nothing on that path calls into libcef,
//! so there is no libcef call to abort in. [`init`] is the one place that asks CEF anything, once,
//! where CEF is up.
//!
//! # Why `Lua::unsafe_new`
//!
//! `.claude/PLUGINS.md`, decided by the user 2026-08-07: **`debug` is in.** mlua's safe constructor
//! refuses `StdLib::DEBUG`, and `debug.getinfo` is how a pure-Lua module written for neovim finds
//! its own directory. The sandbox that refusing it would protect does not exist — `bru.cmd(":spawn
//! …")` starts processes and `bru.set` reaches Chromium — and a half sandbox is more dangerous than
//! none, because it gets trusted. A plugin is code the user put in their own data directory, with
//! the powers that implies, and this file does not pretend otherwise.
//!
//! # What is *not* here
//!
//! **Nothing on the key path.** `j` reaches `scroll::step`, which reads an atomic in 0.3 ns, and a
//! plugin registers a **command** — the key still goes through the trie in Rust and the same
//! dispatcher. One crossing per command, never per keystroke. `grep -n mlua src/keys.rs` is empty
//! and is the review-level check of exactly this.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use cef::{currently_on, ThreadId};

/// One Lua state and the functions the rest of bru is holding into it.
///
/// They are one struct rather than two thread-locals because they have to die together. An
/// `mlua::Function` is a weak reference into the state; calling one whose `Lua` has been dropped is
/// a panic inside mlua (`WeakLua::lock` is `.expect("Lua instance is destroyed")`), not an error
/// bru could report.
struct Runtime {
    /// Every function handed over — by `bru.set("x", function() … end)`, by `bru.command(…)` and by
    /// `bru.on(…)` — keyed by [`FnRef`]'s id.
    functions: HashMap<u64, mlua::Function>,
    /// The next id. Never reused, so a stale [`FnRef`] answers "gone" rather than somebody else's
    /// function.
    next_id: u64,
    lua: mlua::Lua,
}

thread_local! {
    /// The state. `None` everywhere except the CEF UI thread of the browser process, after
    /// [`init`] — which is `on_context_initialized`, where `Config::load` is called and where the
    /// state used to be created and dropped inside one function call.
    static RUNTIME: RefCell<Option<Runtime>> = const { RefCell::new(None) };

    /// The global names that were there before any *config* ran — the standard library, `bru`, and
    /// whatever the plugins left behind. This is what `:config-source --clear` clears back to; see
    /// [`mark_baseline`].
    static BASELINE: RefCell<Option<BTreeSet<String>>> = const { RefCell::new(None) };
}

// --- setting functions -------------------------------------------------------------------------
// [`FnRef`], [`Arg`], [`register`] and [`call_string`] are workstream C's, kept here **signature for
// signature** from the stand-in that workstream landed against this file's contract. Two things
// about the shapes are worth stating rather than rediscovering:
//
// - `FnRef` is `{ id, origin }` and not an `mlua::RegistryKey`, so it is `Send + Sync` without
//   qualification and can sit in `settings.rs`'s `static Mutex` store and in `plugins.rs`'s
//   registry. The function itself never leaves this file.
// - `call_string` hands the fields over as **one table**. `function(tab)` reading `tab.title`
//   survives a field being added beside it; `function(index, title, url, pinned, muted)` does not.

/// A handle to a Lua function the rest of bru is holding.
///
/// Opaque on purpose: `settings.rs` and `plugins.rs` must not name `mlua`, and this is the type that
/// lets them not. `Clone`, `Send` and `Sync` because a [`crate::settings::Value`] is all three — the
/// store is a `static Mutex` reached from several threads. None of that makes the *function*
/// reachable from another thread: [`call_string`] looks the id up in this thread's [`RUNTIME`] and
/// finds nothing on a thread that never loaded a config, which is the renderer case and the
/// unit-test case at once.
///
/// `origin` is what `:set` and `bru://chrome/settings` print. It is captured once, when the function
/// is registered, from `mlua::Function::info` — `short_src` and `line_defined`, which come from
/// `lua_getinfo` and so do not need the `debug` library to be loaded.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FnRef {
    id: u64,
    origin: Arc<str>,
}

impl FnRef {
    /// Where the function was written — `config.lua:12`.
    // Unreached on this branch: `settings.rs` prints it beside a function-valued setting, which is
    // P2's, and `Display` below is what the rest of bru uses meanwhile.
    #[allow(dead_code)]
    pub fn origin(&self) -> &str {
        &self.origin
    }
}

impl std::fmt::Display for FnRef {
    /// **What a function-valued setting prints**, everywhere one line is wanted.
    ///
    /// It is not the value and does not pretend to be: a function has no value until it is called,
    /// and what it answers depends on which tab is being drawn. Printing one call's result would be
    /// printing an answer that was never used. What can honestly be said is that it is a function
    /// and where it was written, which is the one thing a person who wants to change it needs.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<function {}>", self.origin)
    }
}

/// One field handed to a Lua function.
///
/// A closed enum rather than `mlua::Value` for the same reason [`FnRef`] is opaque: the callers are
/// `tabs.rs`, `editor.rs` and the event hooks, and none of them may name `mlua`. Three shapes is
/// what they need — a tab is text, numbers and flags — and a fourth can be added when something
/// wants one.
// Unreached on this branch, with the rest of the table-shaped calls: `Arg` is built by `tabs.rs`
// for P2's function-valued settings and by the event hooks for P4. The two callers that exist today
// — a plugin command handler and a config file — take a string and nothing.
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq)]
pub enum Arg {
    Text(String),
    Int(i64),
    Bool(bool),
    // --- plugin events ---------------------------------------------------------------------
    /// No value. A payload field that is genuinely absent — a `download-finished` with no browser
    /// behind it, a `page-loaded` before a window is known — arrives as Lua `nil` rather than as an
    /// empty string, so a handler can tell "not known" from "known to be empty".
    Nil,
    // --- end plugin events -----------------------------------------------------------------
}

// --- plugin events -------------------------------------------------------------------------
impl Arg {
    /// What `BRU_DEBUG_EVENTS=1` prints. Not `Debug`: this is the value as a plugin would see it,
    /// which is what a line meant for reading an event payload has to show.
    pub fn debug(&self) -> String {
        match self {
            Arg::Text(text) => text.clone(),
            Arg::Int(number) => number.to_string(),
            Arg::Bool(flag) => flag.to_string(),
            Arg::Nil => "nil".to_string(),
        }
    }
}
// --- end plugin events ---------------------------------------------------------------------
// --- end setting functions ---------------------------------------------------------------------

// -----------------------------------------------------------------------------------------------
// The state
// -----------------------------------------------------------------------------------------------

/// Stand the state up, on the UI thread, before any browser exists.
///
/// Called from `app.rs::on_context_initialized`. Calling it twice is a no-op, so a second window or
/// a re-entered callback cannot lose what is registered.
pub fn init() {
    // The one libcef call in this file. It is here rather than in [`with`] because `with` is
    // reached from unit tests, where libcef was never initialised — CEF-NOTES trap 13.
    // `currently_on` itself is safe there: measured 2026-08-07 under `cargo test`, it answers 0 and
    // logs "No task runner for threadId 0". What is not safe is anything that *posts*, and nothing
    // on this path posts.
    debug_assert_ne!(currently_on(ThreadId::UI), 0);
    let _ = shared();
}

/// The shared state, made if this thread has not got one yet.
///
/// Two callers, and both are honest about it. [`init`] is the production one and asks CEF which
/// thread this is first. `config::apply_lua` is the other: reading a `config.lua` needs a state
/// whether or not one has been stood up, which is what makes `Config::load_from` work under
/// `cargo test` — and it means the unit tests exercise the **shared** state rather than a private
/// one, so the collision P1 introduces (two `:config-source`s over one set of globals) is reachable
/// from a test.
///
/// The clone is an `Rc` clone of the same interpreter and its `Drop` does not collect, so the state
/// survives the caller by design rather than by luck.
pub fn shared() -> mlua::Lua {
    let (lua, fresh) = RUNTIME.with(|cell| {
        let mut cell = cell.borrow_mut();
        let fresh = cell.is_none();
        let state = cell.get_or_insert_with(|| Runtime {
            functions: HashMap::new(),
            next_id: 1,
            // PLUGINS.md decision 2, taken by the user on 2026-08-07: `debug` is in, which means
            // `unsafe_new`. See the module docs for why a half sandbox would be worse than none.
            // SAFETY: what "unsafe" grants is loading a C module, which a plugin could already
            // reach with `bru.cmd(":spawn …")`.
            lua: unsafe { mlua::Lua::unsafe_new() },
        });
        (state.lua.clone(), fresh)
    });
    if fresh {
        if let Err(e) = install_bru_table(&lua) {
            eprintln!("bru: the Lua `bru` table could not be built: {e}");
        }
        // The standard library and `bru`, and nothing else yet. `app.rs` takes it again once the
        // plugins have loaded, so a global a plugin sets is part of what `--clear` clears *back to*.
        mark_baseline();
    }
    lua
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
/// would make `bru.set` — or `bru.command`, which reaches [`register`] — re-enter this file and
/// panic on the borrow.
pub fn with<R>(f: impl FnOnce(&mlua::Lua) -> R) -> Option<R> {
    let lua = RUNTIME.with(|cell| cell.borrow().as_ref().map(|state| state.lua.clone()))?;
    Some(f(&lua))
}

/// Whether the state exists on this thread. Cheap, and it names the question callers actually ask.
// Unreached outside this file's own tests today; `with` answering `None` is what the callers use.
#[allow(dead_code)]
pub fn is_up() -> bool {
    RUNTIME.with(|cell| cell.borrow().is_some())
}

/// Remember the globals that are there now, as the set `--clear` clears back to.
///
/// Called **once**, the moment the state is made, and therefore before any config file or plugin
/// has run: what it records is the standard library and `bru`. A plugin's globals are added
/// afterwards by [`add_to_baseline`], from the difference either side of a load.
///
/// The order matters and got this wrong once. Taking the baseline *after* `Config::load` — which is
/// the obvious place, since that is where the plugins load too — recorded the config file's own
/// globals as part of it, and a `:config-source --clear` then left them standing. Measured
/// 2026-08-07 against a config with an `if already_ran then` guard: `X` bound to `tab-prev` on the
/// first run and `tab-next` on every one after, with `--clear` given every time.
pub fn mark_baseline() {
    if let Some(names) = with(global_names) {
        BASELINE.with(|slot| *slot.borrow_mut() = Some(names));
    }
}

/// The global names right now, or `None` on a thread with no state.
///
/// `plugins.rs` takes one either side of a load and hands the difference to [`add_to_baseline`].
pub fn global_names_now() -> Option<BTreeSet<String>> {
    with(global_names)
}

/// Add names to the set `--clear` clears back to.
///
/// **A plugin's globals belong in it and a config file's do not**, which is the whole distinction
/// `:config-source --clear` draws: it clears the configuration, not the browser. The difference is
/// taken either side of the load rather than by snapshotting after it, because by then the config
/// file has already run and its globals are indistinguishable from anyone else's.
///
/// What this cannot do: a plugin that is reloaded and *stops* setting a global leaves the old name
/// in the baseline for the rest of the session, so a config file that happens to use that name is
/// not cleared. Nothing here can tell that case from a plugin that still owns it.
pub fn add_to_baseline(names: BTreeSet<String>) {
    if names.is_empty() {
        return;
    }
    BASELINE.with(|slot| {
        if let Some(baseline) = slot.borrow_mut().as_mut() {
            baseline.extend(names);
        }
    });
}

fn global_names(lua: &mlua::Lua) -> BTreeSet<String> {
    lua.globals()
        .pairs::<mlua::Value, mlua::Value>()
        .filter_map(Result::ok)
        .filter_map(|(key, _)| match key {
            mlua::Value::String(name) => name.to_str().ok().map(|name| name.to_string()),
            _ => None,
        })
        .collect()
}

/// Take away every global that is not in the baseline, and answer how many went.
///
/// **This is the half of `:config-source --clear` that only exists because the state is now
/// shared.** Until P1 each `:config-source` built a fresh `Lua`, so a config file that defined a
/// global could not collide with its own previous run; sharing one state ends that, and a `--clear`
/// that cleared the `Config` and left the globals standing would make sourcing twice differ from
/// sourcing once — in a way no test that existed before P1 could see.
pub fn clear_added_globals() -> usize {
    let Some(baseline) = BASELINE.with(|slot| slot.borrow().clone()) else {
        return 0;
    };
    with(|lua| {
        let globals = lua.globals();
        let added: Vec<String> = global_names(lua)
            .into_iter()
            .filter(|name| !baseline.contains(name))
            .collect();
        let mut gone = 0usize;
        for name in added {
            if globals.set(name.as_str(), mlua::Value::Nil).is_ok() {
                gone += 1;
            }
        }
        gone
    })
    .unwrap_or(0)
}

// -----------------------------------------------------------------------------------------------
// Keeping a function, and calling it back
// -----------------------------------------------------------------------------------------------

// --- setting functions -------------------------------------------------------------------------
/// Keep a Lua function and answer the handle the rest of bru stores.
///
/// `None` when this thread has no state, which cannot happen from the callers — `config.rs` has just
/// run a config file in it, and `bru.command` is only reachable from inside one — but is answered
/// rather than asserted so that a later caller cannot make it a panic.
pub fn register(function: &mlua::Function) -> Option<FnRef> {
    // `info()` before the borrow, because it locks the Lua and a callback could in principle reach
    // back in here. It cannot today; the ordering costs nothing and removes the question.
    let info = function.info();
    let origin = match (info.short_src.as_deref(), info.line_defined) {
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

/// Call a setting's function with named fields and take a string back.
///
/// The fields arrive as **one table**, not as positional arguments: `function(tab)` reading
/// `tab.title` survives a field being added beside it, and `function(index, title, url, pinned,
/// muted)` does not. It is also what makes the error message worth reading — a config that spells a
/// field wrong gets `nil`, and a `nil` in a concatenation says which line it was on.
///
/// **A number is accepted and a table is not.** Lua concatenates a number into a string without
/// being asked, so a function that answers `42` for a tab title means the tab title `42`; a function
/// that answers a table means the config is wrong, and the error names the type. Neither is a panic:
/// the whole point of Lua over a native plugin (PLUGINS.md's first table) is that a mistake here is
/// a message and the browser goes on.
// Unreached on this branch — `settings.rs` is its caller and that is P2's file. Kept because two
// other workstreams are written against exactly this signature.
#[allow(dead_code)]
pub fn call_string(handle: &FnRef, fields: &[(&'static str, Arg)]) -> Result<String, String> {
    let (lua, function) = look_up(handle)?;
    let table = fields_table(&lua, handle, fields)?;
    take_string(handle, function.call::<mlua::Value>(table))
}
// --- end setting functions ---------------------------------------------------------------------

/// Call a function with named fields and take back **either a command line or an argv** — the shape
/// `passwords.show` and `passwords.list` have.
///
/// The twin of [`call_string`], and the second answer is the reason it exists rather than a
/// convenience: a `keepassxc-cli` invocation carries a database path, and a path may contain a
/// space. A string would have to be split, and splitting is what the array form exists to refuse.
/// A string answer is still accepted and still split, so
/// `function(p) return "pass show " .. p.entry end` keeps working — a function is a way of choosing
/// the command line, not a second templating language.
pub fn call_argv(
    handle: &FnRef,
    fields: &[(&'static str, Arg)],
) -> Result<crate::passwords::Spec, String> {
    let (lua, function) = look_up(handle)?;
    let table = fields_table(&lua, handle, fields)?;
    match function.call::<mlua::Value>(table) {
        Ok(mlua::Value::String(text)) => text
            .to_str()
            .map(|text| crate::passwords::Spec::Line(text.to_string()))
            .map_err(|e| format!("{handle}: its answer is not text bru can read: {e}")),
        Ok(mlua::Value::Table(table)) => {
            let mut argv = Vec::new();
            for (index, value) in table.sequence_values::<mlua::Value>().enumerate() {
                match value {
                    Ok(mlua::Value::String(text)) => match text.to_str() {
                        Ok(text) => argv.push(text.to_string()),
                        Err(e) => {
                            return Err(format!("{handle}: element {} is not text: {e}", index + 1))
                        }
                    },
                    Ok(other) => {
                        return Err(format!(
                            "{handle}: element {} is {}, and every element of a command has to be \
                             a string",
                            index + 1,
                            other.type_name()
                        ))
                    }
                    Err(e) => return Err(format!("{handle}: {e}")),
                }
            }
            if argv.is_empty() {
                return Err(format!("{handle}: returned an empty command"));
            }
            Ok(crate::passwords::Spec::Argv(argv))
        }
        Ok(other) => Err(format!(
            "{handle}: returned {}, and what is wanted here is a string or an array of strings",
            other.type_name()
        )),
        Err(e) => Err(format!("{handle}: {e}")),
    }
}

/// Call a function with named fields and ignore whatever it answers — the shape an **event** hook
/// has (`bru.on(event, fn)`, `fn(table) -> nil`).
///
/// The twin of [`call_string`], and it exists so that a handler which returns nothing is not made to
/// look like a handler that returned the wrong type.
// Unreached on this branch — `events.rs` is its caller and that is P4's file.
#[allow(dead_code)]
pub fn call_unit(handle: &FnRef, fields: &[(&'static str, Arg)]) -> Result<(), String> {
    let (lua, function) = look_up(handle)?;
    let table = fields_table(&lua, handle, fields)?;
    function
        .call::<mlua::Value>(table)
        .map(|_| ())
        .map_err(|e| format!("{handle}: {e}"))
}

/// Call a function with **one string** and take a string back — the shape a **command** handler has
/// (`bru.command(name, fn)`, `fn(args: string) -> nil | string`).
///
/// A third calling convention rather than a third table, because that is what the contract in
/// `PLUGIN-CONTRACTS.md` spells and because it is what a plugin author will write without being
/// told: `bru.command("hello", function(args) … end)`. `nil` comes back as the empty string, which
/// is `plugins.rs`'s "the handler said nothing".
pub fn call_text(handle: &FnRef, argument: &str) -> Result<String, String> {
    let (_, function) = look_up(handle)?;
    // The `<function …>` prefix that [`call_string`] puts on a throw is left off here, and that is
    // measured rather than tidied: `plugins::run` already says which *command* threw, and Lua's own
    // message already says which file and line, so the third name made the bar read
    // `boom: <function …/boom/init.lua:1>: runtime error: …/boom/init.lua:3: attempt to index a nil
    // value`. A setting's function has no command name in front of it and still wants the origin.
    take_string(handle, function.call::<mlua::Value>(argument))
        .map_err(|e| e.strip_prefix(&format!("{handle}: ")).unwrap_or(&e).to_string())
}

/// The function, cloned out with the borrow released before it runs — see [`with`]. A handler is
/// allowed to call `bru.cmd`, and that reaches this file.
fn look_up(handle: &FnRef) -> Result<(mlua::Lua, mlua::Function), String> {
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

#[allow(dead_code)]
fn fields_table(
    lua: &mlua::Lua,
    handle: &FnRef,
    fields: &[(&'static str, Arg)],
) -> Result<mlua::Table, String> {
    let table = lua
        .create_table()
        .map_err(|e| format!("{handle}: could not build its argument: {e}"))?;
    for (name, value) in fields {
        let stored = match value {
            Arg::Text(text) => table.set(*name, text.as_str()),
            Arg::Int(number) => table.set(*name, *number),
            Arg::Bool(flag) => table.set(*name, *flag),
            // --- plugin events -----------------------------------------------------------
            // Set rather than skipped: a field left out of the table and a field set to `nil` read
            // the same from Lua, but `pairs()` over the payload shows the shape either way only if
            // every field is named. `Value::Nil` keeps the key list honest.
            Arg::Nil => table.set(*name, mlua::Value::Nil),
            // --- end plugin events -------------------------------------------------------
        };
        stored.map_err(|e| format!("{handle}: could not set {name}: {e}"))?;
    }
    Ok(table)
}

fn take_string(handle: &FnRef, answer: mlua::Result<mlua::Value>) -> Result<String, String> {
    match answer {
        Ok(mlua::Value::Nil) => Ok(String::new()),
        Ok(mlua::Value::String(text)) => text
            .to_str()
            .map(|text| text.to_string())
            .map_err(|e| format!("{handle}: its answer is not text bru can read: {e}")),
        Ok(mlua::Value::Integer(number)) => Ok(number.to_string()),
        Ok(mlua::Value::Number(number)) => Ok(number.to_string()),
        Ok(other) => Err(format!(
            "{handle}: returned {}, and what is wanted here is a string or nothing",
            other.type_name()
        )),
        Err(e) => Err(format!("{handle}: {e}")),
    }
}

// --- setting functions -------------------------------------------------------------------------
/// Forget every function except the ones named — `:config-source --clear`'s.
///
/// The ids are never reused, so a setting still holding an old handle reads as "not reachable" and
/// falls back to bru's own default rather than to somebody else's function. Nothing else in bru may
/// forget one, because nothing else can know that no `Value::Fn` still names it.
///
/// **A plugin's command handler is a registered function too**, and `--clear` must not take those:
/// `:config-source --clear` clears the *configuration*, and a plugin is not configuration. So this
/// takes the handles to keep, which `plugins.rs` fills in.
pub fn forget_functions_except(keep: &[FnRef]) {
    let keep: BTreeSet<u64> = keep.iter().map(|handle| handle.id).collect();
    RUNTIME.with(|cell| {
        if let Some(state) = cell.borrow_mut().as_mut() {
            state.functions.retain(|id, _| keep.contains(id));
        }
    });
}
// --- end setting functions ---------------------------------------------------------------------

// -----------------------------------------------------------------------------------------------
// The `bru` table
// -----------------------------------------------------------------------------------------------

/// The half of the `bru` table that exists for the whole process.
///
/// The other half — `bru.bind`, `bru.unbind`, `bru.search`, `bru.set` — belongs to a config file
/// being read, and `config.rs` installs it for the length of one chunk. Two halves rather than one
/// because they answer different questions: these work whenever there is a browser, and those only
/// mean something while a `Config` is being built.
fn install_bru_table(lua: &mlua::Lua) -> mlua::Result<()> {
    let bru = lua.create_table()?;

    // bru.command(name, fn) — the plugin registry's front door. `plugins.rs` owns what happens with
    // it; this is only the crossing.
    bru.set(
        "command",
        lua.create_function(|_, (name, handler): (String, mlua::Function)| {
            let handle = register(&handler).ok_or_else(|| {
                mlua::Error::RuntimeError("there is no Lua state to register into".to_string())
            })?;
            crate::plugins::register_command(&name, handle).map_err(mlua::Error::RuntimeError)
        })?,
    )?;

    // --- plugin events ---------------------------------------------------------------------
    // `bru.on(event, fn)`. The same crossing `bru.command` makes: the function goes into the
    // registry and only its handle leaves this file. `events.rs` owns which names exist and refuses
    // an unknown one with the list, so a typo is an error at registration rather than a handler
    // that is simply never called.
    bru.set(
        "on",
        lua.create_function(|_, (event, handler): (String, mlua::Function)| {
            let handle = register(&handler).ok_or_else(|| {
                mlua::Error::RuntimeError("there is no Lua state to register into".to_string())
            })?;
            crate::events::register(&event, &crate::plugins::current_plugin_name(), handle)
                .map_err(mlua::Error::RuntimeError)
        })?,
    )?;
    // --- end plugin events -------------------------------------------------------------------

    // bru.cmd(line) — **posts a UI task, never runs inline.**
    //
    // CEF-NOTES trap 12: no CEF call that creates a browser or starts a navigation may run inside a
    // message-router query handler, and a plugin command typed at `:` arrives through exactly that
    // path. `exec::run_from_cmdline` is the posting function `cmdline.rs` already installs as its
    // runner, so a plugin's `bru.cmd(":open -t …")` and a person's typed `:open -t …` reach the
    // dispatcher by the same road, one turn of the loop later.
    //
    // One leading `:` is stripped, because that is how a command line is written down and how the
    // contract spells it. `/` and `?` are not commands and are not taken here — a plugin that wants
    // to search says `bru.cmd("search foo")`.
    bru.set(
        "cmd",
        lua.create_function(|_, line: String| {
            let line = line.trim();
            let line = line.strip_prefix(':').unwrap_or(line).trim();
            if line.is_empty() {
                return Err(mlua::Error::RuntimeError(
                    "bru.cmd was given an empty command line".to_string(),
                ));
            }
            crate::exec::run_from_cmdline(line, None);
            Ok(())
        })?,
    )?;

    // bru.get(name) — one setting's value in force, as text. An unknown name raises with the same
    // refusal `bru.set` gives, so a typo is an error where it is written rather than a nil.
    bru.set(
        "get",
        lua.create_function(|_, name: String| {
            if !crate::settings::is_known(&name) {
                return Err(mlua::Error::RuntimeError(format!(
                    "bru.get: {name} is not one of bru's settings"
                )));
            }
            Ok(crate::settings::text_or_default(&name))
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

    // bru.data_dir() — `~/.local/share/bru`, expanded. bru's own directory, and the only one it
    // writes to: `~/.config/bru/` is configer's and nothing in bru creates or writes it.
    bru.set(
        "data_dir",
        lua.create_function(|_, ()| {
            Ok(crate::data::data_dir()
                .map(|dir| dir.display().to_string())
                .unwrap_or_default())
        })?,
    )?;

    // bru.plugin_dir() — the calling plugin's own directory, during its load and inside its
    // handlers. Empty anywhere else, because outside a plugin there is no honest answer.
    bru.set(
        "plugin_dir",
        lua.create_function(|_, ()| Ok(crate::plugins::current_plugin_dir()))?,
    )?;

    lua.globals().set("bru", bru)?;
    // The four config-time names exist from the start, refusing. Without them a plugin that calls
    // `bru.set` gets "attempt to call a nil value", which says nothing about why.
    install_config_stubs(lua)?;
    Ok(())
}

// --- plugin events -------------------------------------------------------------------------
/// One real crossing of the Lua boundary, of exactly the shape [`call_unit`] makes: a fresh table
/// with a URL and a window in it, handed to a Lua function. Answers the nanoseconds per call.
///
/// It lives here because this is the file allowed to name `mlua`, and
/// `events::the_cost_of_an_event_nobody_asked_for` needs a number to compare its branch against. A
/// stand-in would have been the thing this project calls *a harness that reproduces its own
/// assumption*; `mlua` runs perfectly well under `cargo test` — it is CEF that does not
/// (CEF-NOTES trap 13).
///
/// Its own state, not [`shared`]: the measurement must not depend on what a config or a plugin has
/// already put in the process's state, and standing one up here would leave it behind for every
/// test that ran afterwards.
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
// --- end plugin events ---------------------------------------------------------------------

/// The `bru` table, for the files that add to it.
pub(crate) fn bru_table(lua: &mlua::Lua) -> mlua::Result<mlua::Table> {
    lua.globals().get::<mlua::Table>("bru")
}

/// The four names that only mean something while a config file is being read, as functions that
/// say so.
///
/// `config.rs` swaps the real ones in for the length of one chunk and calls this to put the stubs
/// back — whether the chunk finished or threw. **That restoration is new with the shared state**:
/// until P1 the whole `Lua` was dropped when the chunk finished, so there was nothing left behind
/// and no way for a plugin handler to reach a `Config` nobody was holding any more.
pub(crate) fn install_config_stubs(lua: &mlua::Lua) -> mlua::Result<()> {
    let bru = bru_table(lua)?;
    for name in ["bind", "unbind", "search", "set"] {
        bru.set(
            name,
            lua.create_function(move |_, _: mlua::MultiValue| {
                Err::<(), _>(mlua::Error::RuntimeError(format!(
                    "bru.{name} only means something while a config file is being read — put it in \
                     config.lua, or say bru.cmd(\":set …\") / bru.cmd(\":bind …\")"
                )))
            })?,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a function in the shared state, the way `config.rs` does, and hand back its handle.
    fn register_source(source: &str) -> FnRef {
        let lua = shared();
        // `@` for the same reason `config.rs` uses it: it is what makes `short_src` the plain file
        // name rather than `[string "config.lua"]`.
        let function: mlua::Function = lua.load(source).set_name("@config.lua").eval().unwrap();
        register(&function).expect("the state was just made")
    }

// --- setting functions -------------------------------------------------------------------------
    /// The whole of the contract in one test: a function goes in, a string comes out, and the
    /// fields arrive as a table.
    #[test]
    fn a_function_takes_a_table_of_fields_and_answers_a_string() {
        let handle = register_source("return function(tab) return tab.title:upper() end");
        let answer = call_string(
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
        let answer = call_string(
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
        let error = call_string(&handle, &[]).unwrap_err();
        assert!(error.contains("config.lua:1"), "{error}");
        assert!(error.contains("no"), "{error}");
    }

    /// A function that answers something that is not a string. The two useful cases are told apart:
    /// a number is a title, a table is a mistake.
    #[test]
    fn a_number_is_taken_and_a_table_is_refused_by_name() {
        assert_eq!(
            call_string(&register_source("return function() return 42 end"), &[]),
            Ok("42".to_string())
        );
        let error = call_string(&register_source("return function() return {} end"), &[])
            .unwrap_err();
        assert!(error.contains("returned table"), "{error}");
    }

    /// **The case a renderer process is in**, and the one that must not panic: a handle registered
    /// on one thread, read on another that never loaded a config.
    #[test]
    fn a_handle_is_not_reachable_from_a_thread_with_no_state() {
        let handle = register_source("return function() return 'here' end");
        assert_eq!(call_string(&handle, &[]), Ok("here".to_string()));

        let elsewhere = std::thread::spawn(move || call_string(&handle, &[]))
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
// --- end setting functions ---------------------------------------------------------------------

    /// **A unit test is one of the two shapes that must get an answer rather than an abort.**
    ///
    /// CEF-NOTES trap 13. `init` is never called here — libcef was never initialised — so nothing
    /// on this path reaches for CEF at all, and a thread that has not asked for a state has none.
    #[test]
    fn a_thread_with_no_state_answers_rather_than_panics() {
        std::thread::spawn(|| {
            assert!(!is_up());
            assert_eq!(with(|_| 1u8), None);
            assert_eq!(clear_added_globals(), 0);
            // `mark_baseline` on a thread with no state leaves the baseline unset rather than
            // recording an empty one, which would make the next `--clear` wipe the standard library.
            mark_baseline();
            assert_eq!(clear_added_globals(), 0);
        })
        .join()
        .expect("the thread must not panic");
    }

    /// The three things `init` sets up, checked without `init`'s question to CEF.
    #[test]
    fn the_shared_state_has_debug_and_the_bru_table() {
        let lua = shared();
        // `debug` is loaded — PLUGINS.md decision 2, and exactly what `Lua::new()` refuses.
        assert_eq!(
            lua.load("return type(debug.getinfo)").eval::<String>().unwrap(),
            "function"
        );
        // The long-lived half of the `bru` table.
        for name in ["command", "on", "cmd", "get", "message", "error", "data_dir", "plugin_dir"] {
            assert_eq!(
                lua.load(format!("return type(bru.{name})")).eval::<String>().unwrap(),
                "function",
                "bru.{name}"
            );
        }
        // And the config-time half, refusing rather than missing. **This is the difference the
        // shared state makes**: before P1 there was no "outside a config file" for these to be in.
        let error = lua.load("bru.set('start_page', 'x')").exec().unwrap_err().to_string();
        assert!(error.contains("only means something while a config file"), "{error}");
    }

    /// The baseline, and what `--clear` does to a global a config file set.
    #[test]
    fn clearing_takes_away_what_a_config_added_and_leaves_the_standard_library() {
        let lua = shared();
        mark_baseline();
        lua.load("planted = 1").exec().unwrap();
        assert_eq!(lua.load("return planted").eval::<Option<i64>>().unwrap(), Some(1));

        assert_eq!(clear_added_globals(), 1);
        assert_eq!(lua.load("return planted").eval::<Option<i64>>().unwrap(), None);
        // ...and the standard library is still standing, which is the failure a baseline taken at
        // the wrong moment would have.
        assert_eq!(
            lua.load("return type(string.format)").eval::<String>().unwrap(),
            "function"
        );
        assert_eq!(lua.load("return type(bru.cmd)").eval::<String>().unwrap(), "function");
    }

    /// `call_text` is the command-handler shape: one string in, a string or nothing out.
    #[test]
    fn a_command_handler_takes_one_string_and_may_answer_nothing() {
        let handle = register_source("return function(args) return 'got ' .. args end");
        assert_eq!(call_text(&handle, "a b"), Ok("got a b".to_string()));

        let quiet = register_source("return function(_) end");
        assert_eq!(call_text(&quiet, ""), Ok(String::new()));

        let throws = register_source("return function() local t = nil; return t.nope end");
        let error = call_text(&throws, "").unwrap_err();
        assert!(error.contains("nil value"), "{error}");
    }

    /// `call_unit` is the event shape, and a handler that answers nothing is not an error.
    #[test]
    fn an_event_handler_takes_a_table_and_its_answer_is_ignored() {
        let handle = register_source("return function(e) seen = e.url; return 'ignored' end");
        assert_eq!(call_unit(&handle, &[("url", Arg::Text("https://x/".to_string()))]), Ok(()));
        assert_eq!(shared().load("return seen").eval::<String>().unwrap(), "https://x/");
    }

    /// `forget_functions_except` is `--clear`'s, and what it must not take is a plugin's handler.
    #[test]
    fn clearing_a_config_keeps_the_handlers_a_plugin_registered() {
        let config_side = register_source("return function() return 'config' end");
        let plugin_side = register_source("return function() return 'plugin' end");
        forget_functions_except(&[plugin_side.clone()]);
        assert_eq!(call_string(&plugin_side, &[]), Ok("plugin".to_string()));
        assert!(call_string(&config_side, &[]).unwrap_err().contains("not reachable"));
    }
}
