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

/// One top-level window: its CEF handles, its chrome, and its tabs.
///
/// Everything in here used to be a field of [`BruState`], because there was one window. Splitting
/// it out is what makes `gD`, `U` and every `-w` spelling mean something — and it is also what keeps
/// two windows from pushing into each other's tab strip, which a single `active` index could not.
pub struct WindowState {
    /// bru's own identifier for the window, and the one `:tab-give 1` names. Zero-based, like
    /// qutebrowser's `win_id`, so a count of `n` means window `n - 1` (`commands.py:475`).
    pub(crate) id: u32,
    /// The top-level window, kept from `on_window_created` so views can be added to it later.
    window: Option<Window>,
    /// The window's vertical box layout, kept so a tab opened later can be given flex 1 like the
    /// ones that were there when the window was built.
    layout: Option<BoxLayout>,
    /// Identifiers of the browsers behind *this* window's two chrome strips. Keys that reach those
    /// must not be read as page movements — `j` in the command line is a letter, not a scroll — and
    /// they must be aimed at the tab showing in the window they arrived at, not in whichever window
    /// happened to be current (CEF-NOTES trap 11).
    chrome_browsers: Vec<i32>,
    /// The tabs, in strip order, and which one is showing. `tabs.rs` owns every operation on
    /// these; the fields are visible to it and to nothing outside the crate.
    pub(crate) tabs: Vec<crate::tabs::Tab>,
    pub(crate) active: usize,
    /// The tab that was showing before the current one, which is what `tab-focus last` (`<Ctrl-Tab>`,
    /// `<Ctrl-^>`) goes back to. An index, so it survives nothing — a tab closed in between leaves
    /// it pointing at whatever took that place, which is qutebrowser's behaviour too.
    pub(crate) last_active: Option<usize>,
    /// Which mode keys pressed in *this* window are routed to.
    ///
    /// qutebrowser keeps one `ModeManager` per window and reaches it as `modeman.instance(win_id)`
    /// (`keyinput/modeman.py:34-41`); its `entered`/`left` signals both carry the `win_id` they
    /// happened in. bru had one for the process, which is why `:` opened the command line in
    /// whichever window was in front and left the other window's bar showing `insert` from ten
    /// minutes ago. The manager itself was already written as "one per window" — nothing in it is
    /// shared — so this is where it always belonged.
    modes: crate::modes::ModeManager,
}

pub struct BruState {
    /// Every live browser, in creation order, across every window. Emptying this list ends the
    /// process — which is why it stays flat rather than moving into [`WindowState`]: the message
    /// loop belongs to the application, not to a window.
    browsers: Vec<Browser>,
    /// The single `Client`, handed to every browser so they share handlers. It holds an `Arc` back
    /// to this state, so the two keep each other alive for the life of the process — deliberate:
    /// both are wanted until CEF shuts down, and CEF holds its own reference to the client anyway.
    client: Option<Client>,
    /// Every open window, in the order they were opened. Visible to `tabs.rs`, which owns every
    /// operation on a window's tabs, and to nothing outside the crate.
    pub(crate) windows: Vec<WindowState>,
    /// Which of them a command acts on. Set from the browser a key arrived at (`keys.rs`) and from
    /// `on_window_activation_changed` (`window.rs`), so "the current window" is the one the user is
    /// actually looking at rather than the one opened last.
    current: usize,
    /// The next window identifier. It only ever goes up: reusing the id of a closed window would
    /// make a `:tab-give 1` typed a moment too late land somewhere surprising.
    next_window_id: u32,
    /// URLs of closed tabs, newest last: `u` (`undo`) pops one and opens it again. Only the URL is
    /// kept — CEF exposes no way to serialise a tab's navigation list, so the reopened tab starts
    /// with an empty history.
    ///
    /// One stack for the process, where qutebrowser keeps one per window
    /// (`tabbedbrowser.py:159`): a `u` in a window that has closed nothing still reopens the tab
    /// you actually closed last, which is what someone who has just closed a tab means.
    pub(crate) closed: Vec<String>,
    /// URLs of the tabs each closed *window* held, newest last — what `U` (`undo -w`) reopens. A
    /// window is one entry however many tabs it had, so `U` brings the whole window back
    /// (`commands.py:831-861`, `windowundo.undo_last_window_close`).
    pub(crate) closed_windows: Vec<Vec<String>>,
    /// The binding tries, one per mode, built once at startup from the compiled-in qutebrowser
    /// defaults and whatever `config.lua` changed. `None` until then — and permanently so in the
    /// renderer and GPU processes, which construct this struct and never fill it in.
    ///
    /// Nothing Lua survives in here: `Config::into_parsers` returns plain tries of parsed
    /// `Command`s, and the `Lua` state is dropped before this is set. Pressing `j` must not enter
    /// an interpreter.
    parsers: Option<crate::bindings::KeyParsers>,
    /// The same bindings the parsers were built from, kept whole so `bru://help` can list them.
    /// A trie answers "what does this key do"; the help page asks the opposite question.
    bindings: Option<crate::config::Bindings>,
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
                windows: Vec::new(),
                current: 0,
                next_window_id: 0,
                closed: Vec::new(),
                closed_windows: Vec::new(),
                parsers: None,
                bindings: None,
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

    // --- windows ---------------------------------------------------------------------------
    //
    // Every accessor below that names no window acts on the *current* one, which is what keeps the
    // rest of bru — `tabs.rs`, `session.rs`, `hints.rs`, `completers.rs`, `spawn.rs` — reading the
    // same as it did when there was only ever one.

    pub(crate) fn slot(&self, id: u32) -> Option<&WindowState> {
        self.windows.iter().find(|slot| slot.id == id)
    }

    pub(crate) fn slot_mut(&mut self, id: u32) -> Option<&mut WindowState> {
        self.windows.iter_mut().find(|slot| slot.id == id)
    }

    /// The current window's slot. `None` in every process that is not the browser process, and for
    /// the moment between the last window closing and the message loop stopping.
    pub(crate) fn current_slot(&self) -> Option<&WindowState> {
        self.windows.get(self.current)
    }

    pub(crate) fn current_slot_mut(&mut self) -> Option<&mut WindowState> {
        self.windows.get_mut(self.current)
    }

    /// Makes room for a window before CEF has made one, and makes it current.
    ///
    /// The order matters: at startup the first tab is created *before* `window_create_top_level`,
    /// so there has to be somewhere to put it. `window.rs` allocates the slot, hands the id to the
    /// two chrome delegates and to the window delegate, and `on_window_created` fills in the CEF
    /// handles later.
    pub fn open_window_slot(&mut self) -> u32 {
        let id = self.next_window_id;
        self.next_window_id += 1;
        self.windows.push(WindowState {
            id,
            window: None,
            layout: None,
            chrome_browsers: Vec::new(),
            tabs: Vec::new(),
            active: 0,
            last_active: None,
            modes: crate::modes::ModeManager::new(),
        });
        self.current = self.windows.len() - 1;
        id
    }

    pub fn window_count(&self) -> usize {
        self.windows.len()
    }

    pub fn window_ids(&self) -> Vec<u32> {
        self.windows.iter().map(|slot| slot.id).collect()
    }

    /// The window a command acts on. `None` once every window has gone.
    pub fn current_window_id(&self) -> Option<u32> {
        self.current_slot().map(|slot| slot.id)
    }

    pub fn focus_window(&mut self, id: u32) -> bool {
        match self.windows.iter().position(|slot| slot.id == id) {
            Some(index) => {
                self.current = index;
                true
            }
            None => false,
        }
    }

    /// Which window a browser belongs to — a tab of it, or one of its two chrome strips.
    ///
    /// This is what makes trap 11 survive a second window: a key that lands on a strip is aimed at
    /// the tab showing in *that* window, and a title arriving for a background window's page is
    /// pushed into that window's bar rather than into whichever one is focused.
    pub fn window_of_browser(&self, identifier: i32) -> Option<u32> {
        self.windows
            .iter()
            .find(|slot| {
                slot.chrome_browsers.contains(&identifier)
                    || slot.tabs.iter().any(|tab| tab.browser_id == Some(identifier))
            })
            .map(|slot| slot.id)
    }

    /// Make the window a browser belongs to the current one, and answer which one that was.
    ///
    /// Called from `keys.rs` on every keypress, before anything reads "the showing tab" **or the
    /// mode**. It answers the id rather than a bool so that the caller does not have to search the
    /// window list a second time to ask which mode this key is in: after this call `self.current`
    /// *is* that window, so [`BruState::mode`] is an index rather than a scan.
    pub fn focus_window_of_browser(&mut self, identifier: i32) -> Option<u32> {
        let id = self.window_of_browser(identifier)?;
        self.focus_window(id).then_some(id)
    }

    pub fn set_window_for(&mut self, id: u32, window: Window, layout: Option<BoxLayout>) {
        if let Some(slot) = self.slot_mut(id) {
            slot.window = Some(window);
            slot.layout = layout;
        }
    }

    pub fn window_handle(&self, id: u32) -> Option<Window> {
        self.slot(id).and_then(|slot| slot.window.clone())
    }

    pub fn layout_handle(&self, id: u32) -> Option<BoxLayout> {
        self.slot(id).and_then(|slot| slot.layout.clone())
    }

    /// The current window's CEF handle.
    pub fn window(&self) -> Option<Window> {
        self.current_slot().and_then(|slot| slot.window.clone())
    }

    /// Every open window's handle — what `:quit` closes and what a shutdown walks.
    pub fn window_handles(&self) -> Vec<Window> {
        self.windows
            .iter()
            .filter_map(|slot| slot.window.clone())
            .collect()
    }

    /// A window is gone. Its slot goes with it, and the URLs it held are pushed onto the
    /// closed-window stack so `U` can bring the whole thing back.
    ///
    /// Called from `on_window_destroyed`, which is late enough that the tabs are still listed here:
    /// `can_close` asked each of their browsers to close, and the slot is what named them.
    pub fn forget_window(&mut self, id: u32) {
        let Some(index) = self.windows.iter().position(|slot| slot.id == id) else {
            return;
        };
        let slot = self.windows.remove(index);
        let urls: Vec<String> = slot
            .tabs
            .iter()
            .map(|tab| tab.url.clone())
            .filter(|url| !url.is_empty())
            .collect();
        if !urls.is_empty() {
            self.closed_windows.push(urls);
        }
        if self.current >= self.windows.len() {
            self.current = self.windows.len().saturating_sub(1);
        }
    }

    /// The `depth`-th most recently closed window's tabs, removed from the undo stack.
    pub fn take_closed_window(&mut self) -> Option<Vec<String>> {
        self.closed_windows.pop()
    }

    /// Learned from the chrome views' own delegate, which CEF hands both the view and the browser
    /// it made for it. Reading the frame URL instead would race the first load.
    pub fn note_chrome_browser(&mut self, window: u32, identifier: i32) {
        if let Some(slot) = self.slot_mut(window) {
            if !slot.chrome_browsers.contains(&identifier) {
                slot.chrome_browsers.push(identifier);
            }
        }
    }

    /// Whether a browser is drawing chrome — in *any* window. `keys.rs` asks this to decide whether
    /// a key may be forwarded, and the answer must not depend on which window is current.
    pub fn is_chrome_browser(&self, identifier: i32) -> bool {
        self.windows
            .iter()
            .any(|slot| slot.chrome_browsers.contains(&identifier))
    }

    /// Installed once, from `on_context_initialized` — the browser process only, and after the Lua
    /// state that may have edited the bindings has been dropped.
    pub fn set_parsers(&mut self, parsers: crate::bindings::KeyParsers) {
        self.parsers = Some(parsers);
    }

    pub fn set_bindings(&mut self, bindings: crate::config::Bindings) {
        self.bindings = Some(bindings);
    }

    /// For `bru://help`, which is built on a CEF IO thread and must not hold this lock while it
    /// renders 231 rows.
    pub fn bindings_snapshot(&self) -> Option<crate::config::Bindings> {
        self.bindings.clone()
    }

    /// Feed one keypress to the parser for the current window's mode. `None` before the bindings
    /// are loaded, which is every process that is not the browser process.
    ///
    /// "The current window" is not a guess on this path: `keys.rs` calls
    /// [`BruState::focus_window_of_browser`] with the browser CEF delivered the key to, under this
    /// same lock, before anything below runs. So the mode read here is the mode of the window the
    /// key was pressed in, and reading it costs one bounds-checked index — see [`BruState::mode`].
    pub fn handle_key(
        &mut self,
        info: crate::bindings::KeyInfo,
    ) -> Option<crate::bindings::KeyOutcome> {
        let mode = self.mode();
        self.parsers
            .as_mut()
            .map(|parsers| parsers.handle(mode, info))
    }

    /// The mode of the window a command acts on.
    ///
    /// **This is on the key path**, so it is an index into `windows` and not a search of it: the
    /// window a key belongs to has already been made current by `focus_window_of_browser`, which
    /// pays for the one lookup either way. `Normal` when there is no window at all — every process
    /// that is not the browser process, and the moment between the last window closing and the
    /// message loop stopping.
    pub fn mode(&self) -> crate::modes::Mode {
        match self.windows.get(self.current) {
            Some(slot) => slot.modes.mode(),
            None => crate::modes::Mode::Normal,
        }
    }

    /// The mode of a named window — for everything that arrives with a window rather than with the
    /// focus: a chrome strip answering a query, a push aimed at a background bar.
    pub fn mode_in(&self, window: u32) -> crate::modes::Mode {
        self.slot(window)
            .map(|slot| slot.modes.mode())
            .unwrap_or(crate::modes::Mode::Normal)
    }

    /// The `Browser` of the tab currently showing.
    ///
    /// Needed because a key does not always arrive at the page: CEF delivers it to whichever view
    /// holds focus, and with `sloppyfocus` on this desktop that is easily a chrome strip. Commands
    /// still have to act on the page, so the strip's key is dispatched against this instead.
    pub fn active_browser(&mut self) -> Option<Browser> {
        let id = self.active_tab_browser_id()?;
        self.browser_with_id(id)
    }

    /// The `Browser` of the tab showing in a named window.
    ///
    /// The same question as [`BruState::active_browser`] asked of a window that may not be the
    /// current one — which is what the command line needs when the chrome answers `bru.accept()`
    /// from a background window's strip: the focus has to go back to *that* window's page.
    pub fn active_browser_in(&mut self, window: u32) -> Option<Browser> {
        let id = self
            .slot(window)
            .and_then(|slot| slot.tabs.get(slot.active))
            .and_then(|tab| tab.browser_id)?;
        self.browser_with_id(id)
    }

    /// Any live browser by identifier. `active_browser` is the common case; saving a session needs
    /// every tab's browser, because a navigation list can only be read from the browser that holds
    /// it.
    pub fn browser_with_id(&mut self, id: i32) -> Option<Browser> {
        self.browsers
            .iter_mut()
            .find(|browser| browser.identifier() == id)
            .cloned()
    }

    /// Enter a mode in the current window, clearing the pending chain of the one left behind.
    /// `only_if_normal` is what stops a page's focus event dragging you out of passthrough.
    pub fn enter_mode(&mut self, mode: crate::modes::Mode, only_if_normal: bool) -> bool {
        match self.current_window_id() {
            Some(window) => self.enter_mode_in(window, mode, only_if_normal),
            None => false,
        }
    }

    /// The same for a named window. Every mode change is one window's: a `:` in the window in front
    /// must not put the window behind it into command mode, and an editable field focusing itself
    /// in a background window must not drop the window being typed in out of passthrough.
    pub fn enter_mode_in(
        &mut self,
        window: u32,
        mode: crate::modes::Mode,
        only_if_normal: bool,
    ) -> bool {
        let Some(slot) = self.slot_mut(window) else {
            return false;
        };
        let transition = slot.modes.enter(mode, only_if_normal);
        self.apply(transition)
    }

    pub fn leave_mode(&mut self) -> bool {
        match self.current_window_id() {
            Some(window) => self.leave_mode_in(window),
            None => false,
        }
    }

    pub fn leave_mode_in(&mut self, window: u32) -> bool {
        let Some(slot) = self.slot_mut(window) else {
            return false;
        };
        match slot.modes.leave_current() {
            Ok(transition) => self.apply(transition),
            Err(_) => false,
        }
    }

    /// The half of a transition that is not the mode itself.
    ///
    /// `self.parsers` is still one set for the process, so the pending key chain a mode is holding
    /// is shared between windows and this clears it for all of them. That is a smaller version of
    /// the bug this workstream fixed and it is deliberately not fixed here: the chain lives inside
    /// `bindings::KeyParser`, which is another workstream's file. See the report.
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

        // The *last* browser in the process, not the last in a window: closing one of two windows
        // leaves the other's three browsers here and the loop keeps running. Measured — see the
        // report; without the window list this test was "is `browsers` empty", which was the same
        // question only because there was one window.
        if self.browsers.is_empty() {
            self.windows.clear();
            self.client = None;
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

/// `--open="ddg python dict" [--open-tab] [--open-after-ms=N]` runs one `:open` from a posted UI
/// task and then reports, twice, what came of it.
///
/// It exists because `:open` has no way in from a script otherwise: the command line is another
/// milestone's, and the only key injector on this machine is `wtype`, which segfaults CEF (see
/// `schedule_tab_script`). Without it the end-to-end claim for M9 would rest on a hand-typed URL,
/// which is to say on nothing that runs twice.
///
/// Two reports, because they say different things: the first is bru's own decision, the second is
/// the address **Chromium** ended up at, learned from `on_address_change`. A decision that is right
/// and a navigation that fails look identical without the second.
pub fn schedule_open(text: &str, tab: bool, bg: bool, delay_ms: i64) {
    let mut task = OpenStep::new(text.to_string(), tab, bg);
    post_delayed_task(ThreadId::UI, Some(&mut task), delay_ms);
}

wrap_task! {
    struct OpenStep {
        text: String,
        tab: bool,
        bg: bool,
    }

    impl Task {
        fn execute(&self) {
            let Some(state) = BruState::instance() else {
                return;
            };
            let engines = crate::open::engines();
            eprintln!(
                "open-script: {:?} -> {:?}",
                self.text,
                crate::open::decide(&self.text, &engines)
            );

            let browser = state.lock().expect("state mutex poisoned").active_browser();
            let Some(mut browser) = browser else {
                eprintln!("open-script: no tab to open into");
                return;
            };
            crate::open::open(&state, &mut browser, Some(&self.text), self.tab, self.bg);

            // Chromium has not navigated yet; ask again once it has.
            let mut task = OpenReport::new();
            post_delayed_task(ThreadId::UI, Some(&mut task), 3000);
        }
    }
}

wrap_task! {
    struct OpenReport;

    impl Task {
        fn execute(&self) {
            let Some(state) = BruState::instance() else {
                return;
            };
            let state = state.lock().expect("state mutex poisoned");
            eprintln!("open-script: chromium is at {}", state.tabs_json());
        }
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
                "d" => crate::tabs::close_current(&state, false),
                other => eprintln!("tab-script: no step named {other}"),
            }
            let state = state.lock().expect("state mutex poisoned");
            // The window count is here so the `t,t,J,K,d,d,d` regression can also say that the
            // window it ran in was the only one — the counts alone would not notice a step that
            // quietly opened a second.
            eprintln!(
                "tab-script: after {} -> {} tabs, showing {}, {} window(s)",
                self.step,
                state.tab_count(),
                state.active_tab(),
                state.window_count(),
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
            // Take the handles and drop the lock before closing anything: the close runs the
            // window delegate's callbacks, and those lock this same mutex.
            // Every window, not only the current one — the switch means "exit after N ms", and one
            // window left open would keep the message loop running.
            let windows = state.lock().expect("state mutex poisoned").window_handles();
            for window in windows {
                window.close();
            }
        }
    }
}

// -----------------------------------------------------------------------------------------------
// Tests — the window/mode bookkeeping only. Everything else here needs a live CEF.
// -----------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modes::Mode;

    /// A `BruState` with no CEF anywhere in it, and `n` empty windows.
    ///
    /// Built by hand rather than through `BruState::new`, which registers the process-wide
    /// `INSTANCE` and asserts that no second one is alive — `cargo test` runs these on several
    /// threads at once and they would fight over it. Nothing below has to be reachable from a
    /// callback, so nothing below needs that registration.
    fn bare(windows: u32) -> BruState {
        let mut state = BruState {
            browsers: Vec::new(),
            client: None,
            windows: Vec::new(),
            current: 0,
            next_window_id: 0,
            closed: Vec::new(),
            closed_windows: Vec::new(),
            parsers: None,
            bindings: None,
        };
        for _ in 0..windows {
            state.open_window_slot();
        }
        // `open_window_slot` makes each new window current; start from the first, as a session does.
        state.current = 0;
        state
    }

    /// The whole of this workstream in one test: a mode is one window's.
    ///
    /// qutebrowser reaches its per-window manager as `modeman.instance(win_id)`
    /// (`keyinput/modeman.py:34-41`); bru had one for the process, so `:` in the window in front
    /// put the window behind it into command mode too, and every `mode-leave` took both out.
    #[test]
    fn a_mode_belongs_to_one_window() {
        let mut state = bare(2);
        assert_eq!(state.mode(), Mode::Normal);
        assert_eq!(state.mode_in(0), Mode::Normal);
        assert_eq!(state.mode_in(1), Mode::Normal);

        // `:` in window 0.
        assert!(state.enter_mode_in(0, Mode::Command, false));
        assert_eq!(state.mode_in(0), Mode::Command);
        assert_eq!(state.mode_in(1), Mode::Normal, "window 1 keeps its own mode");

        // An unnamed call means the current window, and follows the focus.
        assert_eq!(state.mode(), Mode::Command);
        assert!(state.focus_window(1));
        assert_eq!(state.mode(), Mode::Normal);

        // Both at once, in different modes. This is the case one manager could not hold.
        assert!(state.enter_mode_in(1, Mode::Insert, false));
        assert_eq!(state.mode_in(0), Mode::Command);
        assert_eq!(state.mode_in(1), Mode::Insert);

        // And leaving one leaves exactly one.
        assert!(state.leave_mode_in(1));
        assert_eq!(state.mode_in(1), Mode::Normal);
        assert_eq!(state.mode_in(0), Mode::Command, "window 0 was not asked to leave");

        // A window that is not there answers Normal rather than panicking: a push can outlive the
        // window it was aimed at by one turn of the loop.
        assert_eq!(state.mode_in(9), Mode::Normal);
        assert!(!state.enter_mode_in(9, Mode::Insert, false));
    }

    /// The routing `keys.rs` depends on: the browser CEF delivered the key to names the window, and
    /// the mode read afterwards is that window's.
    ///
    /// Chrome browsers rather than tabs because a strip is the case trap 11 is about — a key landing
    /// on window 1's status bar must be answered with window 1's mode, whatever window 0 is doing.
    #[test]
    fn a_key_resolves_the_mode_of_the_window_it_arrived_at() {
        let mut state = bare(2);
        state.note_chrome_browser(0, 100);
        state.note_chrome_browser(1, 200);
        state.enter_mode_in(1, Mode::Command, false);

        assert_eq!(state.focus_window_of_browser(100), Some(0));
        assert_eq!(state.mode(), Mode::Normal, "window 0's key, window 0's mode");

        assert_eq!(state.focus_window_of_browser(200), Some(1));
        assert_eq!(state.mode(), Mode::Command, "window 1's key, window 1's mode");
        assert_eq!(state.mode_in(0), Mode::Normal);

        // A browser bru never put in a window names none, and the focus stays where it was.
        assert_eq!(state.focus_window_of_browser(999), None);
        assert_eq!(state.current_window_id(), Some(1));
    }

    /// A closed window's mode goes with it, so a later window cannot start life in `insert`.
    #[test]
    fn forgetting_a_window_forgets_its_mode() {
        let mut state = bare(2);
        state.enter_mode_in(1, Mode::Insert, false);
        state.forget_window(1);
        assert_eq!(state.mode_in(1), Mode::Normal);
        assert_eq!(state.window_count(), 1);

        // The next window gets a fresh id and a fresh mode.
        let id = state.open_window_slot();
        assert_eq!(id, 2);
        assert_eq!(state.mode_in(id), Mode::Normal);
    }

    /// **What a per-window mode costs the key path**, measured rather than asserted, beside the one
    /// `ModeManager` it replaced and beside the shape that would have been too slow.
    ///
    /// The rule this test exists to keep is DESIGN.md's: `j` goes through `on_pre_key_event` →
    /// `BruState::handle_key` → this lookup, so anything that scans the window list on every
    /// keystroke is the wrong answer however correct it reads. It does not scan: `keys.rs` calls
    /// `focus_window_of_browser` first, which pays for the one lookup a key needs, and everything
    /// after it is `windows[current]`.
    ///
    /// `mode_in` is timed as the counterexample — it is the same question asked by *id*, which is a
    /// linear search of the window list, and it is deliberately not what the key path calls.
    ///
    /// The assertions are loose on purpose: this runs unoptimised under `cargo test`, on a machine
    /// with five other agents on it. What they pin is the shape, not the clock.
    #[test]
    fn the_key_path_cost_of_a_per_window_mode() {
        const ROUNDS: u32 = 1_000_000;

        // Four windows, the mode being read belonging to the last of them — so a search would have
        // the furthest to go and an index would not care.
        let mut state = bare(4);
        state.enter_mode_in(3, Mode::Insert, false);
        state.focus_window(3);
        let one = crate::modes::ModeManager::new();

        for _ in 0..10_000 {
            std::hint::black_box(std::hint::black_box(&state).mode());
        }

        let start = std::time::Instant::now();
        for _ in 0..ROUNDS {
            std::hint::black_box(std::hint::black_box(&state).mode());
        }
        let per_window = start.elapsed().as_nanos() as f64 / ROUNDS as f64;

        // What it replaced: a field of `BruState`, one deref.
        let start = std::time::Instant::now();
        for _ in 0..ROUNDS {
            std::hint::black_box(std::hint::black_box(&one).mode());
        }
        let process_wide = start.elapsed().as_nanos() as f64 / ROUNDS as f64;

        // What it must not be: the same answer found by searching the window list for an id.
        let start = std::time::Instant::now();
        for _ in 0..ROUNDS {
            std::hint::black_box(std::hint::black_box(&state).mode_in(3));
        }
        let by_search = start.elapsed().as_nanos() as f64 / ROUNDS as f64;

        println!(
            "modes: reading the mode costs {per_window:.3} ns per window against \
             {process_wide:.3} ns for the one this replaced, and {by_search:.3} ns if it is \
             searched for by id instead"
        );
        assert!(
            per_window < 40.0,
            "the key path must stay free: {per_window:.3} ns"
        );
        assert!(
            per_window < process_wide * 5.0,
            "an indexed read must stay within a small constant of a plain field read: \
             {per_window:.3} ns against {process_wide:.3} ns"
        );
        assert!(
            per_window < by_search,
            "if the indexed lookup is not faster than the search, it is not an index: \
             {per_window:.3} ns vs {by_search:.3} ns"
        );
    }
}
