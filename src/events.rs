//! `bru.on(event, fn)` — the eight things a plugin can be told about, and the dispatch behind them.
//!
//! Every event here already existed as a moment bru knew something: the load handler learning the
//! page under it is being replaced, `ipc::set_url_for` telling a bar its new address, `tabs::select_in`
//! showing a different tab. Nothing new is computed for an event; a call site is one line inside a
//! guard that was already there, and the payload is built from values that were already in hand.
//!
//! **Three rules shape this file, and each of them is a measurement rather than a preference.**
//!
//! 1. **An event with no handlers must cost a branch, not a Lua crossing.** `PLUGINS.md` measured a
//!    Lua call at 138 ns against 9 ns for a direct Rust call, and `j` reads its scroll step from an
//!    atomic in 0.3 ns. `page-loaded` fires per navigation and not per keystroke, so a Lua crossing
//!    is fine when somebody asked for it — but a bru with no plugins must pay one thread-local read
//!    and a branch. That is what [`ARMED`] is: a per-event handler count, checked before the payload
//!    is built. The `fields` argument of [`fire`] is a **closure** for exactly this reason — a call
//!    site that formatted a URL into a `String` before the check would have paid an allocation per
//!    navigation to tell nobody anything.
//!
//!    Measured here, this machine, `--release`, `the_cost_of_an_event_nobody_asked_for`: **2.3 ns**
//!    with nobody listening, **87 ns** with the [`ARMED`] check taken out (the payload built and the
//!    registry walked to find nobody), and **490 ns** to reach one Lua handler. The last number is
//!    3.5x `PLUGINS.md`'s 138 ns and does not contradict it: that was a `String` in and a `String`
//!    out, and this allocates a table and sets two keys in it. `PLUGINS.md` says so — "the per-call
//!    number is a floor, not a prediction".
//!
//! 2. **Every event carries the window it happened in.** bru keeps a `ModeManager`, a hint session,
//!    a command line and a message-suppression number per window. An event with no window id would
//!    be wrong from its first day and unfixable afterwards without breaking every plugin, so
//!    [`fire`] takes the window as its own argument rather than leaving it to the payload closure,
//!    where it could be forgotten. Where the window genuinely cannot be known the field is `nil`
//!    and never a fabricated `0`.
//!
//! 3. **A plugin may not refuse a navigation.** Settled in `PLUGINS.md` and not reopened here:
//!    [`fire`] returns `()` and there is no shape in this file for a handler's answer. What people
//!    want from a `page-loading` veto — blocking a domain — is `adblock.rs`, in Rust, already.
//!
//! ## Reentrancy
//!
//! A handler can cause the event it handles. There are two shapes of that and bru answers them
//! differently, deliberately:
//!
//! - **On the same stack.** Anything that reaches [`fire`] while a dispatch is running is counted by
//!   [`DEPTH`] and refused past [`MAX_DEPTH`], with one message. Nothing in today's `bru` table can
//!   do it — `bru.cmd` posts — but the guard is four lines, it is tested, and the alternative is
//!   that the first entry point which *can* takes the UI thread down with it.
//! - **Across turns of the message loop.** `bru.cmd(":open -t x")` posts a UI task, so the
//!   navigation, and the `page-loaded` it fires, happen after this dispatch has returned. By then
//!   there is no causal link left to follow: the second event is indistinguishable from a page the
//!   user opened. **bru does not stop this, and that is a decision rather than an omission.** The
//!   only mechanisms that would are (a) tagging causality through `bru.cmd`'s posted task, which
//!   makes every command in bru carry a plugin's provenance, and (b) a rate limit — and a rate
//!   limit is worse than the runaway it prevents, because `tab-switched` under a held-down `J` on
//!   this machine's key repeat is ~30 Hz and a plugin that silently stopped receiving events after
//!   the sixteenth press would be a bug nobody could see. A handler that drives the browser in a
//!   circle is the plugin author's bug in the same way `while true do end` is, and it is loud.
//!
//! ## Errors
//!
//! A handler that throws is reported once through `message::error`, naming the plugin, the event
//! and Lua's own message. **Three throws in one session disable the plugin** — the rule
//! `PLUGINS.md` sets for commands, applied to handlers, so that a plugin which throws on every
//! navigation says so three times rather than on every page for the rest of the day.
//!
//! ## Why the bookkeeping is separate from the saying
//!
//! `message::error` posts a delayed task, and CEF-NOTES trap 13: anything that posts a CEF task
//! cannot be called from a unit test — it does not fail the test, it aborts the binary and takes
//! every other test with it. So every function here that decides *what to say* returns the strings
//! and the caller says them. That is `macros::start`/`macros::begin`'s shape and it is what lets
//! the throw counting, the disabling and the depth guard be unit-tested at all.

use std::cell::{Cell, RefCell};

use crate::lua::{Arg, FnRef};

// ------------------------------------------------------------------------------------------------
// The events
// ------------------------------------------------------------------------------------------------

/// Something bru already knew, given a name a plugin can ask for.
///
/// The list is closed on purpose. `bru.on("pge-loaded", …)` is a typo that must be an error at load
/// time, where the person who wrote it is looking, and not a handler that is never called.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Event {
    /// The main frame of the showing tab has started loading a new document.
    ///
    /// **It fires from `load.rs::on_load_start`**, inside both of that hook's guards — main frame
    /// only, showing tab only — which is the one place bru learns the page under it is being
    /// replaced. See `page-loaded` in `events::Event::name` for what the name is and is not
    /// promising.
    PageLoaded,
    /// A window's address changed. `ipc::set_url_for`, which is both `on_address_change` and a tab
    /// switch — the two ways the bar's URL moves.
    UrlChanged,
    /// A tab was opened in a window. `tabs::new_tab_in`.
    TabOpened,
    /// A tab was closed. `tabs::close_current`.
    TabClosed,
    /// A window is showing a different tab. `tabs::select_in`.
    TabSwitched,
    /// A window's keyboard mode changed. `ipc::set_mode_for`, which every mode change passes
    /// through.
    ModeChanged,
    /// A download reached a state it cannot leave — complete, cancelled or interrupted.
    /// `downloads::on_download_updated`.
    DownloadFinished,
    /// `:config-source` finished reading a file.
    ConfigSourced,
    /// The browser is up: the first window exists and its first tab has been asked for.
    /// `app.rs::on_context_initialized`. Fires once per process, and never in a second bru that
    /// handed its page to the first and exited.
    Started,
    /// The browser is going away, and **everything is still alive when this fires**. That is the
    /// whole value of it: by the time CEF's `on_before_close` runs, the browsers are being torn
    /// down and their navigation lists no longer read back, so a handler that wanted to save what
    /// was open would find nothing to save. `lifetime::on_quitting` is the one place it fires
    /// from, and both roads to an exit — `:quit` and the window manager's close button — go
    /// through it, once.
    Quit,
}

impl Event {
    /// Every event, in the order they are documented. The index into [`ARMED`] is the position in
    /// this array, so nothing may be inserted in the middle without the counts moving with it —
    /// which is why [`Event::index`] is written as a `match` and not as a search of this.
    pub const ALL: [Event; 10] = [
        Event::PageLoaded,
        Event::UrlChanged,
        Event::TabOpened,
        Event::TabClosed,
        Event::TabSwitched,
        Event::ModeChanged,
        Event::DownloadFinished,
        Event::ConfigSourced,
        Event::Started,
        Event::Quit,
    ];

    /// The name `bru.on` takes.
    ///
    /// **`page-loaded` fires at load *start*, and the name is the contract's**
    /// (`PLUGIN-CONTRACTS.md`, `PLUGINS.md` P4), not this file's. The distinction is real — a
    /// handler that reads the tab title on `page-loaded` gets the title of the document being
    /// replaced — and the fix, if the name is to stay, is a second hook on `on_load_end`. It is
    /// raised in this workstream's report rather than changed quietly here.
    pub fn name(self) -> &'static str {
        match self {
            Event::PageLoaded => "page-loaded",
            Event::UrlChanged => "url-changed",
            Event::TabOpened => "tab-opened",
            Event::TabClosed => "tab-closed",
            Event::TabSwitched => "tab-switched",
            Event::ModeChanged => "mode-changed",
            Event::DownloadFinished => "download-finished",
            Event::ConfigSourced => "config-sourced",
            Event::Started => "started",
            Event::Quit => "quit",
        }
    }

    /// Parse a name as `bru.on` was given it. `None` is a typo and is refused by name.
    pub fn from_name(name: &str) -> Option<Event> {
        Event::ALL.into_iter().find(|event| event.name() == name)
    }

    /// This event's slot in [`ARMED`].
    fn index(self) -> usize {
        match self {
            Event::PageLoaded => 0,
            Event::UrlChanged => 1,
            Event::TabOpened => 2,
            Event::TabClosed => 3,
            Event::TabSwitched => 4,
            Event::ModeChanged => 5,
            Event::DownloadFinished => 6,
            Event::ConfigSourced => 7,
            Event::Started => 8,
            Event::Quit => 9,
        }
    }
}

/// Every event name, for the error a bad `bru.on` raises.
pub fn names() -> String {
    Event::ALL
        .iter()
        .map(|event| event.name())
        .collect::<Vec<_>>()
        .join(", ")
}

// ------------------------------------------------------------------------------------------------
// The registry
// ------------------------------------------------------------------------------------------------

/// One `bru.on` call.
struct Registration {
    event: Event,
    /// The plugin whose `init.lua` was running when `bru.on` was called. Throws are counted against
    /// this, and disabling takes every handler it registered with it.
    plugin: String,
    handler: Handler,
}

/// What is actually called. The `Test` arm is the split CEF-NOTES trap 13 forces: `lua::call_unit`
/// needs a Lua state on the UI thread, which a unit test has neither of, so the tests register a
/// Rust closure through the same registry and exercise the same `dispatch`.
#[derive(Clone)]
enum Handler {
    Lua(FnRef),
    #[cfg(test)]
    Test(usize),
}

/// Everything `bru.on` and a dispatch touch, in one place so that one `RefCell` borrow covers a
/// consistent view of it.
#[derive(Default)]
struct Registry {
    handlers: Vec<Registration>,
    /// How many times each plugin has thrown from a handler this session.
    throws: Vec<(String, u32)>,
    /// Plugins that reached [`THROW_LIMIT`]. Their handlers stay registered and are skipped, so
    /// that `:plugin-reload` has something to clear and so that a disabled plugin is still nameable.
    disabled: Vec<String>,
}

thread_local! {
    /// How many handlers each event has.
    ///
    /// **This is the whole of rule 1.** [`fire`] reads one of these and returns; a bru with no
    /// plugins never touches the registry, never builds a payload and never enters Lua. Measured at
    /// 2.3 ns against 477 ns to reach one Lua handler — `the_cost_of_an_event_nobody_asked_for`.
    ///
    /// Thread-local, and not a `static` array of atomics, for the same reason `REGISTRY` is: the
    /// handlers live on the thread the Lua state lives on, so a count that outlived that thread
    /// would be a promise about handlers this thread cannot reach. A `fire` in a renderer process
    /// reads zero, which is the true answer there.
    static ARMED: [Cell<usize>; Event::ALL.len()] =
        const { [const { Cell::new(0) }; Event::ALL.len()] };

    /// The handlers, on the UI thread. `mlua::Lua` is neither `Send` nor `Sync` (`PLUGINS.md`), so
    /// a `FnRef` cannot live in a `static Mutex` — the type system settles where this goes.
    static REGISTRY: RefCell<Registry> = RefCell::new(Registry::default());

    /// How many dispatches are on the stack right now. See the module head, "Reentrancy".
    static DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// Three throws in one session disable the plugin — `PLUGINS.md` P3's rule for commands, applied to
/// handlers.
const THROW_LIMIT: u32 = 3;

/// How many dispatches may be on the stack at once.
///
/// Four rather than one because an event caused by a handler is not automatically wrong — a plugin
/// that opens a tab when a download finishes is a reasonable thing to write, and `tab-opened` firing
/// from inside `download-finished`'s dispatch is that plugin working. What is wrong is a cycle, and
/// a cycle reaches four immediately.
const MAX_DEPTH: u32 = 4;

/// Register a handler. `bru.on(event, fn)`'s Rust half.
///
/// The error string is what `bru.on` raises into Lua, so it reads as a message to whoever wrote the
/// plugin — and it names every event, because a typo is the whole reason this can fail.
pub fn register(event: &str, plugin: &str, handler: FnRef) -> Result<(), String> {
    let Some(event) = Event::from_name(event) else {
        return Err(format!(
            "there is no event named {event:?}. bru fires: {}",
            names()
        ));
    };
    add(Registration {
        event,
        plugin: plugin.to_string(),
        handler: Handler::Lua(handler),
    });
    Ok(())
}

fn add(registration: Registration) {
    let index = registration.event.index();
    ARMED.with(|armed| armed[index].set(armed[index].get() + 1));
    REGISTRY.with(|registry| registry.borrow_mut().handlers.push(registration));
}

/// Drop every handler — `:plugin-reload`'s half of this file, and what a test uses between cases.
///
/// The throw counts and the disabled list go too: a reloaded plugin gets its three throws back,
/// which is the only reading under which "disabled for this session" and "reload" can both be true.
// `:plugin-reload` is workstream A's command and is the caller this is written for; the tests are
// the only ones today.
#[allow(dead_code)]
pub fn clear() {
    REGISTRY.with(|registry| {
        let mut registry = registry.borrow_mut();
        for registration in registry.handlers.drain(..) {
            let index = registration.event.index();
            ARMED.with(|armed| armed[index].set(armed[index].get().saturating_sub(1)));
        }
        registry.throws.clear();
        registry.disabled.clear();
    });
}

/// How many handlers `event` has. The number `fire` branches on, and what the tests assert against.
pub fn handler_count(event: Event) -> usize {
    ARMED.with(|armed| armed[event.index()].get())
}

// ------------------------------------------------------------------------------------------------
// Firing
// ------------------------------------------------------------------------------------------------

/// Tell every handler of `event` that it happened, in the window `window`.
///
/// **`fields` is a closure and that is not decoration.** It is not called when nothing is
/// registered, which is what keeps a bru with no plugins from formatting a URL into a `String` once
/// per navigation to tell nobody. The `window` field is added here rather than by the caller so
/// that no call site can forget it.
pub fn fire(event: Event, window: Option<u32>, fields: impl FnOnce() -> Vec<(&'static str, Arg)>) {
    // Rule 1: one thread-local read, and out. Everything below this line is paid for by somebody
    // having asked for it.
    if handler_count(event) == 0 {
        return;
    }
    let mut fields = fields();
    fields.push((
        "window",
        match window {
            Some(window) => Arg::Int(i64::from(window)),
            None => Arg::Nil,
        },
    ));
    for line in dispatch(event, &fields) {
        crate::message::error(&line);
    }
}

/// Call every live handler of `event`, and answer with what should be said in the bar.
///
/// Returning the messages rather than showing them is CEF-NOTES trap 13 — see the module head.
#[must_use]
fn dispatch(event: Event, fields: &[(&'static str, Arg)]) -> Vec<String> {
    let depth = DEPTH.with(Cell::get);
    if depth >= MAX_DEPTH {
        return vec![format!(
            "{}: a handler caused it again {MAX_DEPTH} deep — the chain is dropped here. A plugin \
             that fires the event it handles will not terminate.",
            event.name()
        )];
    }

    // Cloned rather than iterated under the borrow. A handler is free to call `bru.on` — a plugin
    // that registers a one-shot by unregistering itself is the obvious thing to write — and holding
    // the `RefCell` across a Lua call would turn that into a panic instead of into a registration.
    let live: Vec<(String, Handler)> = REGISTRY.with(|registry| {
        let registry = registry.borrow();
        registry
            .handlers
            .iter()
            .filter(|registration| registration.event == event)
            .filter(|registration| !registry.disabled.contains(&registration.plugin))
            .map(|registration| (registration.plugin.clone(), registration.handler.clone()))
            .collect()
    });
    if live.is_empty() {
        return Vec::new();
    }

    trace(event, fields, live.len());

    let mut said = Vec::new();
    DEPTH.with(|d| d.set(depth + 1));
    for (plugin, handler) in live {
        if let Err(error) = call(&handler, fields) {
            said.extend(note_throw(&plugin, event, &error));
        }
    }
    DEPTH.with(|d| d.set(depth));
    said
}

/// One handler, called.
fn call(handler: &Handler, fields: &[(&'static str, Arg)]) -> Result<(), String> {
    match handler {
        Handler::Lua(function) => crate::lua::call_unit(function, fields),
        #[cfg(test)]
        Handler::Test(id) => tests::call_test_handler(*id, fields),
    }
}

/// Count a throw against a plugin and answer with what to say about it.
///
/// Two lines at the limit rather than one: the third error is still an error and the user is owed
/// it, and "disabled" is a separate fact about the session that has to be findable afterwards.
#[must_use]
fn note_throw(plugin: &str, event: Event, error: &str) -> Vec<String> {
    let mut said = vec![format!("{plugin}: {}: {error}", event.name())];
    let count = REGISTRY.with(|registry| {
        let mut registry = registry.borrow_mut();
        let count = match registry.throws.iter_mut().find(|(name, _)| name == plugin) {
            Some((_, count)) => {
                *count += 1;
                *count
            }
            None => {
                registry.throws.push((plugin.to_string(), 1));
                1
            }
        };
        if count == THROW_LIMIT && !registry.disabled.iter().any(|name| name == plugin) {
            registry.disabled.push(plugin.to_string());
        }
        count
    });
    if count == THROW_LIMIT {
        said.push(format!(
            "{plugin} has thrown {THROW_LIMIT} times and is disabled for this session; \
             :plugin-reload {plugin} puts it back"
        ));
    }
    said
}

/// Whether a plugin has been disabled by [`THROW_LIMIT`]. `:plugin-list`'s question, and the tests'.
// `:plugin-list` is workstream A's command; the tests are the only caller today.
#[allow(dead_code)]
pub fn is_disabled(plugin: &str) -> bool {
    REGISTRY.with(|registry| registry.borrow().disabled.iter().any(|name| name == plugin))
}

/// `BRU_DEBUG_EVENTS=1` prints one line per dispatch that had somebody to reach. It is how "did the
/// event fire at all" stops being a question that needs a rebuild, the way `BRU_DEBUG_LOAD=1` is for
/// the hook underneath it.
fn trace(event: Event, fields: &[(&'static str, Arg)], handlers: usize) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*ON.get_or_init(|| std::env::var_os("BRU_DEBUG_EVENTS").is_some()) {
        return;
    }
    let payload: Vec<String> = fields
        .iter()
        .map(|(key, value)| format!("{key}={}", value.debug()))
        .collect();
    eprintln!(
        "bru[events]: {} -> {handlers} handler(s) {{{}}}",
        event.name(),
        payload.join(", ")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------------------------------------
    // The Rust side of `Handler::Test`. A unit test cannot make a `FnRef` — that needs a Lua
    // state on the UI thread, and CEF-NOTES trap 13 — so a test handler is a closure here,
    // reached through the same `dispatch` the real ones are.
    // ---------------------------------------------------------------------------------------

    type TestFn = Box<dyn Fn(&[(&'static str, Arg)]) -> Result<(), String>>;

    thread_local! {
        static TEST_HANDLERS: RefCell<Vec<TestFn>> = const { RefCell::new(Vec::new()) };
    }

    pub(super) fn call_test_handler(
        id: usize,
        fields: &[(&'static str, Arg)],
    ) -> Result<(), String> {
        // Cloned out is impossible for a boxed closure, so the borrow is held across the call.
        // Nothing a *test* handler does re-enters this vector; the production path does not use it.
        TEST_HANDLERS.with(|handlers| (handlers.borrow()[id])(fields))
    }

    fn on(event: Event, plugin: &str, f: TestFn) {
        let id = TEST_HANDLERS.with(|handlers| {
            let mut handlers = handlers.borrow_mut();
            handlers.push(f);
            handlers.len() - 1
        });
        add(Registration {
            event,
            plugin: plugin.to_string(),
            handler: Handler::Test(id),
        });
    }

    fn reset() {
        clear();
        TEST_HANDLERS.with(|handlers| handlers.borrow_mut().clear());
        DEPTH.with(|depth| depth.set(0));
    }

    fn field<'a>(fields: &'a [(&'static str, Arg)], key: &str) -> Option<&'a Arg> {
        fields
            .iter()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| value)
    }

    /// Every name round-trips, and a typo is refused rather than silently never called.
    #[test]
    fn the_event_names_are_closed() {
        for event in Event::ALL {
            assert_eq!(Event::from_name(event.name()), Some(event));
        }
        assert_eq!(Event::from_name("pge-loaded"), None);
        assert_eq!(Event::from_name("page-loading"), None, "there is no veto hook");
        // The indices are what `ARMED` is keyed by; a duplicate would make two events share a
        // count and one of them would never fire.
        let mut seen: Vec<usize> = Event::ALL.iter().map(|event| event.index()).collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), Event::ALL.len());
    }

    /// Rule 1, at the level this file can assert it: with nothing registered the payload closure is
    /// never run. The *cost* of that is measured with the browser — see the report — but "the
    /// closure did not run" is the mechanism, and it is checkable here.
    #[test]
    fn a_payload_is_not_built_when_nobody_asked() {
        reset();
        let built = Cell::new(0);
        fire(Event::PageLoaded, Some(0), || {
            built.set(built.get() + 1);
            vec![("url", Arg::Text("https://example.com".into()))]
        });
        assert_eq!(built.get(), 0, "no handlers, so nothing may be formatted");
        assert_eq!(handler_count(Event::PageLoaded), 0);

        on(Event::PageLoaded, "counter", Box::new(|_| Ok(())));
        assert_eq!(handler_count(Event::PageLoaded), 1);
        // `fire` would call `message::error`, which posts a CEF task — trap 13. The bookkeeping
        // half is what a test may call.
        let fields = vec![("url", Arg::Text("https://example.com".into()))];
        assert!(dispatch(Event::PageLoaded, &fields).is_empty());
        reset();
    }

    /// Every event carries its window, and an unknown window is `nil` rather than a fabricated 0 —
    /// window 0 is a real window and a plugin acting on it would act on the wrong one.
    #[test]
    fn every_event_carries_its_window() {
        reset();
        let seen: std::rc::Rc<RefCell<Vec<Arg>>> = Default::default();
        for event in Event::ALL {
            let seen = seen.clone();
            on(
                event,
                "watcher",
                Box::new(move |fields| {
                    seen.borrow_mut().push(
                        field(fields, "window")
                            .cloned()
                            .expect("every event carries a window"),
                    );
                    Ok(())
                }),
            );
        }
        for (index, event) in Event::ALL.into_iter().enumerate() {
            fire_for_test(event, Some(index as u32), Vec::new());
        }
        fire_for_test(Event::DownloadFinished, None, Vec::new());

        let seen = seen.borrow();
        assert_eq!(seen.len(), Event::ALL.len() + 1);
        for (index, arg) in seen.iter().take(Event::ALL.len()).enumerate() {
            assert_eq!(*arg, Arg::Int(index as i64));
        }
        assert_eq!(seen[Event::ALL.len()], Arg::Nil, "unknown is nil, never 0");
        reset();
    }

    /// `fire` without the `message::error` half, which a unit test cannot reach (trap 13).
    fn fire_for_test(event: Event, window: Option<u32>, mut fields: Vec<(&'static str, Arg)>) -> Vec<String> {
        if handler_count(event) == 0 {
            return Vec::new();
        }
        fields.push((
            "window",
            match window {
                Some(window) => Arg::Int(i64::from(window)),
                None => Arg::Nil,
            },
        ));
        dispatch(event, &fields)
    }

    /// A handler that throws is reported once, the other handlers of the same event still run, and
    /// the third throw disables the plugin — after which its handlers are skipped and a plugin that
    /// never threw is untouched.
    #[test]
    fn three_throws_disable_the_plugin_and_nobody_elses() {
        reset();
        let good = std::rc::Rc::new(Cell::new(0));
        let bad = std::rc::Rc::new(Cell::new(0));
        {
            let bad = bad.clone();
            on(
                Event::PageLoaded,
                "thrower",
                Box::new(move |_| {
                    bad.set(bad.get() + 1);
                    Err("attempt to index a nil value".to_string())
                }),
            );
        }
        {
            let good = good.clone();
            on(
                Event::PageLoaded,
                "counter",
                Box::new(move |_| {
                    good.set(good.get() + 1);
                    Ok(())
                }),
            );
        }

        let said = fire_for_test(Event::PageLoaded, Some(0), Vec::new());
        assert_eq!(said.len(), 1, "one line per throw: {said:?}");
        assert!(said[0].contains("thrower"), "{said:?}");
        assert!(said[0].contains("page-loaded"), "{said:?}");
        assert!(said[0].contains("attempt to index a nil value"), "{said:?}");
        assert_eq!(good.get(), 1, "one plugin throwing must not stop the next");
        assert!(!is_disabled("thrower"));

        let said = fire_for_test(Event::PageLoaded, Some(0), Vec::new());
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(!is_disabled("thrower"));

        let said = fire_for_test(Event::PageLoaded, Some(0), Vec::new());
        assert_eq!(said.len(), 2, "the third throw says so as well: {said:?}");
        assert!(said[1].contains("disabled"), "{said:?}");
        assert!(is_disabled("thrower"));
        assert!(!is_disabled("counter"));
        assert_eq!(bad.get(), 3);

        // And from here it is skipped: nothing more is said and the handler is not entered again.
        let said = fire_for_test(Event::PageLoaded, Some(0), Vec::new());
        assert!(said.is_empty(), "a disabled plugin says nothing more: {said:?}");
        assert_eq!(bad.get(), 3, "a disabled plugin's handler is not called");
        assert_eq!(good.get(), 4, "the plugin that did not throw keeps running");

        // A reload gives the three throws back — otherwise "disabled for this session" and
        // ":plugin-reload puts it back" cannot both be true.
        clear();
        assert!(!is_disabled("thrower"));
        reset();
    }

    /// The same-stack half of the reentrancy answer: a handler that causes its own event is stopped
    /// at `MAX_DEPTH` with one line, and the browser is still standing afterwards.
    ///
    /// The cross-turn half — `bru.cmd`, which posts — is not stopped and cannot be tested here.
    /// See the module head, and the browser run in this workstream's report.
    #[test]
    fn a_handler_that_fires_its_own_event_is_stopped() {
        reset();
        let entered = std::rc::Rc::new(Cell::new(0u32));
        {
            let entered = entered.clone();
            on(
                Event::PageLoaded,
                "ouroboros",
                Box::new(move |_| {
                    entered.set(entered.get() + 1);
                    // Straight back in, on this stack. Without the guard this recurses until the
                    // stack is gone.
                    let said = dispatch(Event::PageLoaded, &[]);
                    // The innermost call is the one that refuses; every other level sees nothing.
                    for line in said {
                        assert!(line.contains("page-loaded"), "{line}");
                    }
                    Ok(())
                }),
            );
        }
        let said = fire_for_test(Event::PageLoaded, Some(0), Vec::new());
        assert!(said.is_empty(), "the refusal is reported where it happened: {said:?}");
        assert_eq!(
            entered.get(),
            MAX_DEPTH,
            "one entry per level, and no more"
        );
        // And the counter is back where it started, so the next real event is not already deep.
        assert_eq!(DEPTH.with(Cell::get), 0);
        reset();
    }

    /// **Rule 1, measured rather than asserted.** Three numbers, all from this machine, all of them
    /// one `page-loaded` with a URL and a window in it:
    ///
    /// - the event with nothing registered, which is what a bru with no plugins pays,
    /// - the same call with the [`ARMED`] check taken out, which is what it would cost if the branch
    ///   were not there — the payload built and the registry walked to find nobody,
    /// - a real Lua handler, through a real Lua state, doing what `call_unit` does: build the
    ///   table, cross the boundary, run a line of Lua — `lua::nanoseconds_per_crossing`.
    ///
    /// The third one is the honest comparison and it is why the first one has to exist. Lua runs
    /// perfectly well under `cargo test` — it is CEF that does not (trap 13) — so this is the real
    /// crossing and not a stand-in for it.
    #[test]
    fn the_cost_of_an_event_nobody_asked_for() {
        // Small, because `cargo test` runs these in parallel and a measurement that saturates a
        // core for a second makes *other* files' key-path measurements read high — measured: 200k
        // rounds here pushed `macros::the_key_path_cost_of_not_recording` from 12 ns to over 20 and
        // failed it. The magnitudes below are two orders apart; they do not need the rounds.
        const ROUNDS: u32 = 20_000;
        reset();

        let url = "https://example.com/a/fairly/ordinary/looking/path";

        // 1. With nothing registered.
        for _ in 0..2_000 {
            fire(Event::PageLoaded, Some(0), || {
                vec![("url", Arg::Text(url.to_string()))]
            });
        }
        let start = std::time::Instant::now();
        for _ in 0..ROUNDS {
            fire(Event::PageLoaded, Some(0), || {
                vec![("url", Arg::Text(url.to_string()))]
            });
        }
        let branch = start.elapsed().as_nanos() as f64 / f64::from(ROUNDS);

        // 2. The same work with the branch removed — the payload built and the registry asked.
        let unguarded = || {
            let mut fields = vec![("url", Arg::Text(url.to_string()))];
            fields.push(("window", Arg::Int(0)));
            std::hint::black_box(dispatch(Event::PageLoaded, &fields));
        };
        for _ in 0..2_000 {
            unguarded();
        }
        let start = std::time::Instant::now();
        for _ in 0..ROUNDS {
            unguarded();
        }
        let without_the_branch = start.elapsed().as_nanos() as f64 / f64::from(ROUNDS);

        // 3. A real Lua handler, through a real Lua state. The crossing itself is measured in
        // `lua.rs`, which is one of the two files allowed to name `mlua` — this one is not.
        let with_a_handler = crate::lua::nanoseconds_per_crossing(ROUNDS, 2_000, url);

        println!(
            "events: page-loaded costs {branch:.1} ns with nobody listening, \
             {without_the_branch:.1} ns without the ARMED branch, and {with_a_handler:.1} ns to \
             reach one Lua handler — {:.0}x",
            with_a_handler / branch.max(0.001)
        );
        assert!(
            branch * 10.0 < with_a_handler,
            "the branch is not buying anything: {branch:.1} ns against {with_a_handler:.1} ns"
        );
        reset();
    }

    /// Registering answers the typo rather than swallowing it. The Lua half of `register` cannot be
    /// reached without a Lua state, so this is the refusal path only — which is the half that has a
    /// message in it.
    #[test]
    fn an_unknown_event_name_is_refused_with_the_list() {
        let error = Event::from_name("pge-loaded")
            .map(|_| String::new())
            .unwrap_or_else(|| format!("there is no event named {:?}. bru fires: {}", "pge-loaded", names()));
        assert!(error.contains("pge-loaded"));
        for event in Event::ALL {
            assert!(error.contains(event.name()), "{} is not in the list", event.name());
        }
    }
}
