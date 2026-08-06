//! The state every browser-process handler shares.
//!
//! CEF constructs a fresh handler object for most of its callbacks, so nothing can be stored in a
//! handler and found again later. The browser list, the one `Client` and the window live here
//! instead, behind a single mutex that every handler holds an `Arc` to.
//!
//! Only the browser process ever fills this in. The renderer, GPU and zygote processes re-execute
//! the same binary and construct it too, where it stays empty — see `main.rs`.

use cef::*;
use std::sync::{Arc, Mutex, OnceLock, Weak};

/// A weak handle to the one instance, for callbacks that arrive with no way of being handed the
/// `Arc` — the delayed-close task below is one. The strong reference lives in `BruApp`.
static INSTANCE: OnceLock<Weak<Mutex<BruState>>> = OnceLock::new();

pub struct BruState {
    /// Every live browser, in creation order. Emptying this list ends the process.
    browsers: Vec<Browser>,
    /// The single `Client`, handed to every browser so they share handlers. It holds an `Arc` back
    /// to this state, so the two keep each other alive for the life of the process — deliberate:
    /// both are wanted until CEF shuts down, and CEF holds its own reference to the client anyway.
    client: Option<Client>,
    /// The top-level window, kept from `on_window_created` so views can be added to it later.
    window: Option<Window>,
    /// The window's vertical box layout, kept so a tab opened later can be given flex 1 like the
    /// ones that were there when the window was built.
    layout: Option<BoxLayout>,
    /// Identifiers of the browsers behind the two chrome strips. Keys that reach those must not be
    /// read as page movements — `j` in the command line is a letter, not a scroll.
    chrome_browsers: Vec<i32>,
    /// The tabs, in strip order, and which one is showing. `tabs.rs` owns every operation on
    /// these; the fields are visible to it and to nothing outside the crate.
    pub(crate) tabs: Vec<crate::tabs::Tab>,
    pub(crate) active: usize,
    /// The binding tries, one per mode, built once at startup from the compiled-in qutebrowser
    /// defaults and whatever `config.lua` changed. `None` until then — and permanently so in the
    /// renderer and GPU processes, which construct this struct and never fill it in.
    ///
    /// Nothing Lua survives in here: `Config::into_parsers` returns plain tries of parsed
    /// `Command`s, and the `Lua` state is dropped before this is set. Pressing `j` must not enter
    /// an interpreter.
    parsers: Option<crate::bindings::KeyParsers>,
    /// Which mode bru is in. Normal until something says otherwise.
    modes: crate::modes::ModeManager,
}

impl BruState {
    pub fn new() -> Arc<Mutex<Self>> {
        // `new_cyclic` so the global weak handle is in place before the Arc exists, and therefore
        // before any callback can go looking for it.
        Arc::new_cyclic(|weak| {
            if INSTANCE.set(weak.clone()).is_err() {
                let previous = INSTANCE.get().expect("set failed but nothing is stored");
                assert_eq!(
                    previous.strong_count(),
                    0,
                    "a second BruState while the first is still alive"
                );
            }

            Mutex::new(Self {
                browsers: Vec::new(),
                client: None,
                window: None,
                layout: None,
                chrome_browsers: Vec::new(),
                tabs: Vec::new(),
                active: 0,
                parsers: None,
                modes: crate::modes::ModeManager::new(),
            })
        })
    }

    pub fn instance() -> Option<Arc<Mutex<Self>>> {
        INSTANCE.get().and_then(Weak::upgrade)
    }

    pub fn set_client(&mut self, client: Client) {
        self.client = Some(client);
    }

    pub fn client(&self) -> Option<Client> {
        self.client.clone()
    }

    pub fn set_window(&mut self, window: Window) {
        self.window = Some(window);
    }

    pub fn window(&self) -> Option<Window> {
        self.window.clone()
    }

    pub fn set_layout(&mut self, layout: Option<BoxLayout>) {
        self.layout = layout;
    }

    pub fn layout(&self) -> Option<BoxLayout> {
        self.layout.clone()
    }

    /// Learned from the chrome views' own delegate, which CEF hands both the view and the browser
    /// it made for it. Reading the frame URL instead would race the first load.
    pub fn note_chrome_browser(&mut self, identifier: i32) {
        if !self.chrome_browsers.contains(&identifier) {
            self.chrome_browsers.push(identifier);
        }
    }

    pub fn is_chrome_browser(&self, identifier: i32) -> bool {
        self.chrome_browsers.contains(&identifier)
    }

    /// Installed once, from `on_context_initialized` — the browser process only, and after the Lua
    /// state that may have edited the bindings has been dropped.
    pub fn set_parsers(&mut self, parsers: crate::bindings::KeyParsers) {
        self.parsers = Some(parsers);
    }

    /// Feed one keypress to the parser for the current mode. `None` before the bindings are loaded,
    /// which is every process that is not the browser process.
    pub fn handle_key(
        &mut self,
        info: crate::bindings::KeyInfo,
    ) -> Option<crate::bindings::KeyOutcome> {
        let mode = self.modes.mode();
        self.parsers
            .as_mut()
            .map(|parsers| parsers.handle(mode, info))
    }

    pub fn mode(&self) -> crate::modes::Mode {
        self.modes.mode()
    }

    /// Enter a mode, clearing the pending chain of the one left behind. `only_if_normal` is what
    /// stops a page's focus event dragging you out of passthrough.
    pub fn enter_mode(&mut self, mode: crate::modes::Mode, only_if_normal: bool) -> bool {
        let transition = self.modes.enter(mode, only_if_normal);
        self.apply(transition)
    }

    pub fn leave_mode(&mut self) -> bool {
        match self.modes.leave_current() {
            Ok(transition) => self.apply(transition),
            Err(_) => false,
        }
    }

    fn apply(&mut self, transition: crate::modes::Transition) -> bool {
        if transition.clear_keychain {
            if let (Some(left), Some(parsers)) = (transition.left, self.parsers.as_mut()) {
                parsers.clear(left);
            }
        }
        transition.entered.is_some()
    }

    /// A browser has come up. Every browser bru creates goes through here.
    pub fn on_after_created(&mut self, browser: Option<&mut Browser>) {
        debug_assert_ne!(currently_on(ThreadId::UI), 0);

        let Some(browser) = browser.cloned() else {
            return;
        };
        self.browsers.push(browser);
    }

    /// Allow the close. Returning 1 here would mean "I will close it myself later", which is the
    /// windowless path bru does not use.
    pub fn do_close(&mut self, _browser: Option<&mut Browser>) -> ::std::os::raw::c_int {
        debug_assert_ne!(currently_on(ThreadId::UI), 0);
        0
    }

    /// A browser is gone for good. When the last one goes, so does the message loop — without this
    /// the window disappears and the process stays alive with nothing to show for it.
    pub fn on_before_close(&mut self, browser: Option<&mut Browser>) {
        debug_assert_ne!(currently_on(ThreadId::UI), 0);

        if let Some(mut browser) = browser.cloned() {
            if let Some(index) = self
                .browsers
                .iter()
                .position(|elem| elem.is_same(Some(&mut browser)) != 0)
            {
                self.browsers.remove(index);
            }
        }

        if self.browsers.is_empty() {
            self.window = None;
            self.layout = None;
            self.client = None;
            self.tabs.clear();
            quit_message_loop();
        }
    }
}

/// `--close-after-ms=N` closes the window N milliseconds after it opens.
///
/// It is here because this machine has no way to close one window from a script: the compositor is
/// mango, whose `mmsg -s -d killclient` acts on whatever happens to be focused. Without this the
/// close path — `can_close` → `try_close_browser` → `do_close` → `on_before_close` →
/// `quit_message_loop` — can only ever be exercised by hand, which is to say never in a check that
/// runs twice. Inert unless the switch is passed.
pub fn schedule_close(delay_ms: i64) {
    let mut task = CloseWindow::new();
    post_delayed_task(ThreadId::UI, Some(&mut task), delay_ms);
}

/// `--tab-script=t,t,J,K,d,d,d --tab-step-ms=3000` runs the tab commands from posted UI tasks,
/// one every `tab-step-ms`, and reports the tab count and selection after each.
///
/// It is here for the same reason as `--close-after-ms`, and for one more: the only key-injection
/// tool on this machine is `wtype`, which attaches a virtual keyboard, and CEF segfaults in
/// `xkb_state_update_mask` when that arrives. Measured 2026-08-06 on a build predating all of the
/// tab work: 3/3 clean exits with no wtype, 2/3 segfaults with a single keystroke. So keys cannot
/// drive an unattended check here, and this drives the very functions the keys call instead. Inert
/// unless the switch is passed. It becomes redundant once M7 has a command table and a general
/// `--cmd=` hook can run real commands.
pub fn schedule_tab_script(steps: &str, interval_ms: i64) {
    for (i, step) in steps.split(',').filter(|s| !s.is_empty()).enumerate() {
        let mut task = TabStep::new(step.to_string());
        post_delayed_task(ThreadId::UI, Some(&mut task), interval_ms * (i as i64 + 1));
    }
}

wrap_task! {
    struct TabStep {
        step: String,
    }

    impl Task {
        fn execute(&self) {
            let Some(state) = BruState::instance() else {
                return;
            };
            match self.step.as_str() {
                "t" => {
                    let index = state.lock().expect("state mutex poisoned").tab_count();
                    crate::tabs::new_tab(&state, &crate::app::placeholder_tab(index), false);
                }
                "J" => crate::tabs::next_tab(&state),
                "K" => crate::tabs::prev_tab(&state),
                "d" => crate::tabs::close_current(&state),
                other => eprintln!("tab-script: no step named {other}"),
            }
            let state = state.lock().expect("state mutex poisoned");
            eprintln!(
                "tab-script: after {} -> {} tabs, showing {}",
                self.step,
                state.tab_count(),
                state.active_tab()
            );
        }
    }
}

wrap_task! {
    struct CloseWindow;

    impl Task {
        fn execute(&self) {
            debug_assert_ne!(currently_on(ThreadId::UI), 0);

            let Some(state) = BruState::instance() else {
                return;
            };
            // Take a handle to the window and drop the lock before closing it: the close runs the
            // window delegate's callbacks, and those lock this same mutex.
            let window = state.lock().expect("state mutex poisoned").window();
            if let Some(window) = window {
                window.close();
            }
        }
    }
}
