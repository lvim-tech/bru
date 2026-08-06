//! Tabs.
//!
//! CEF's Views framework has no tab control, and bru does not want one — DESIGN.md draws the strip
//! itself. A tab is a `BrowserView` that is a child of the window like any other; switching is
//! `set_visible`, so the renderer of the tab you left stays alive and coming back to it is
//! instant. Nothing is reloaded and no state is serialised.
//!
//! **No CEF call is made while the state mutex is held**, and that is not a style preference.
//! Measured 2026-08-06: `window.add_child_view_at` on a browser view makes CEF create the browser
//! *synchronously*, which calls straight back into `LifeSpanHandler::on_after_created`, which takes
//! this same mutex — bru hung on its first frame with the window never mapped. So each operation
//! here reads what it needs under a short lock, lets go, and only then talks to CEF.

use cef::*;
use std::sync::{Arc, Mutex};

use crate::state::BruState;
use crate::window::BruBrowserViewDelegate;

pub type SharedState = Arc<Mutex<BruState>>;

pub struct Tab {
    pub(crate) view: BrowserView,
    /// Learned from `BrowserViewDelegate::on_browser_created`, not at creation: `browser_view_create`
    /// returns before the browser exists, so this is `None` for the moment in between.
    pub(crate) browser_id: Option<i32>,
    pub(crate) title: String,
    pub(crate) url: String,
    /// `<Ctrl-p>` — a tab that keeps its place and does not get closed by accident. It is bru's own
    /// flag and nothing in CEF knows about it; what it changes is `close_current`, `close_others`
    /// and the class the strip draws.
    pub(crate) pinned: bool,
    /// `<Alt-m>` — the mirror of `host.is_audio_muted()`. Kept here as well as asked of CEF because
    /// the strip is rendered from this struct and a tab's browser may not exist yet, and because a
    /// restored session has to re-apply it to a browser that has not been made.
    pub(crate) muted: bool,
}

/// The plain state operations. None of these touch CEF.
///
/// Every one of them that names no window acts on the **current** one — `state.rs` keeps which that
/// is. That is what let a second window arrive without `session.rs`, `hints.rs`, `completers.rs`,
/// `spawn.rs`, `settings.rs`, `scroll.rs` or `history.rs` changing a line: they all ask about "the
/// tabs" and mean the ones in front of the user. The three that key off a *browser* instead —
/// [`BruState::set_tab_url`], [`BruState::set_tab_title`] and [`BruState::is_active_browser`] —
/// search every window, because a page in a background window still reports its title.
impl BruState {
    /// The current window's tab views.
    pub fn tab_views(&self) -> Vec<BrowserView> {
        self.current_slot()
            .map(|slot| slot.tabs.iter().map(|tab| tab.view.clone()).collect())
            .unwrap_or_default()
    }

    pub fn tab_views_in(&self, window: u32) -> Vec<BrowserView> {
        self.slot(window)
            .map(|slot| slot.tabs.iter().map(|tab| tab.view.clone()).collect())
            .unwrap_or_default()
    }

    /// Ties a browser to the tab whose view it was made for. Called once per tab, and across every
    /// window: a view knows nothing about which window it was created for.
    pub fn note_tab_browser(&mut self, view: &mut BrowserView, identifier: i32) {
        for slot in &mut self.windows {
            for tab in &mut slot.tabs {
                if tab.view.is_same(Some(&mut View::from(&*view))) != 0 {
                    tab.browser_id = Some(identifier);
                    return;
                }
            }
        }
    }

    /// Which window a browser's tab is in, and where in that window's strip. Searches every window.
    fn locate_tab(&self, identifier: i32) -> Option<(u32, usize)> {
        self.windows.iter().find_map(|slot| {
            slot.tabs
                .iter()
                .position(|tab| tab.browser_id == Some(identifier))
                .map(|index| (slot.id, index))
        })
    }

    /// True when `identifier` is the browser of the tab showing **in its own window** — which is the
    /// only tab whose address and title belong in that window's status line. A background window's
    /// showing tab answers true, and rightly: it is what that window's bar has to say.
    pub fn is_active_browser(&self, identifier: i32) -> bool {
        match self.locate_tab(identifier) {
            Some((window, index)) => {
                self.slot(window).map(|slot| slot.active) == Some(index)
            }
            None => false,
        }
    }

    /// The browser id of the showing tab, if it has one yet.
    pub fn active_tab_browser_id(&self) -> Option<i32> {
        self.current_slot()
            .and_then(|slot| slot.tabs.get(slot.active))
            .and_then(|tab| tab.browser_id)
    }

    /// Records a tab's address. Answers **which window** the tab is in, and `None` when the browser
    /// is not a tab at all — which is how a chrome strip reporting its own bru:// URL is kept out of
    /// the status line, and how the display handler knows whose bar to push into.
    pub fn set_tab_url(&mut self, identifier: i32, url: String) -> Option<u32> {
        let (window, index) = self.locate_tab(identifier)?;
        self.slot_mut(window)?.tabs[index].url = url;
        Some(window)
    }

    pub fn set_tab_title(&mut self, identifier: i32, title: String) -> Option<u32> {
        let (window, index) = self.locate_tab(identifier)?;
        self.slot_mut(window)?.tabs[index].title = title;
        Some(window)
    }

    /// What the current window's tab strip renders, in strip order.
    pub fn tabs_json(&self) -> String {
        match self.current_window_id() {
            Some(window) => self.tabs_json_in(window),
            None => "{\"tabs\":[]}".to_string(),
        }
    }

    /// The same for a named window. Two windows draw two strips, and a push into the wrong one is
    /// how a background window ends up listing the tabs of the one in front of it.
    pub fn tabs_json_in(&self, window: u32) -> String {
        let Some(slot) = self.slot(window) else {
            return "{\"tabs\":[]}".to_string();
        };
        let entries: Vec<String> = slot
            .tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| {
                format!(
                    "{{\"title\":\"{}\",\"url\":\"{}\",\"active\":{},\"pinned\":{},\"muted\":{}}}",
                    crate::ipc::json_escape(&tab.title),
                    crate::ipc::json_escape(&tab.url),
                    index == slot.active,
                    tab.pinned,
                    tab.muted,
                )
            })
            .collect();
        format!("{{\"tabs\":[{}]}}", entries.join(","))
    }

    /// Whether the tab at `index` keeps its place — what `tab-close` and `tab-only` consult before
    /// they take a tab away.
    pub fn tab_pinned(&self, index: usize) -> bool {
        self.current_slot()
            .and_then(|slot| slot.tabs.get(index))
            .map(|tab| tab.pinned)
            .unwrap_or(false)
    }

    pub fn tab_muted(&self, index: usize) -> bool {
        self.current_slot()
            .and_then(|slot| slot.tabs.get(index))
            .map(|tab| tab.muted)
            .unwrap_or(false)
    }

    /// Flip the pin on the tab at `index` and answer what it became. `tab-pin` is a toggle in
    /// qutebrowser (`commands.py:278`), not a setter.
    pub fn toggle_tab_pinned(&mut self, index: usize) -> bool {
        match self.current_slot_mut().and_then(|slot| slot.tabs.get_mut(index)) {
            Some(tab) => {
                tab.pinned = !tab.pinned;
                tab.pinned
            }
            None => false,
        }
    }

    pub fn set_tab_pinned(&mut self, index: usize, pinned: bool) {
        if let Some(tab) = self.current_slot_mut().and_then(|slot| slot.tabs.get_mut(index)) {
            tab.pinned = pinned;
        }
    }

    /// Flip the mute flag and answer what it became. The CEF call that acts on it is
    /// [`toggle_mute`], outside the lock.
    pub fn toggle_tab_muted(&mut self, index: usize) -> bool {
        match self.current_slot_mut().and_then(|slot| slot.tabs.get_mut(index)) {
            Some(tab) => {
                tab.muted = !tab.muted;
                tab.muted
            }
            None => false,
        }
    }

    pub fn set_tab_muted(&mut self, index: usize, muted: bool) {
        if let Some(tab) = self.current_slot_mut().and_then(|slot| slot.tabs.get_mut(index)) {
            tab.muted = muted;
        }
    }

    /// The browser identifier of each tab, in strip order. `None` for a tab whose browser CEF has
    /// not made yet. Sessions need every tab's browser, not only the showing one's.
    pub fn tab_browser_ids(&self) -> Vec<Option<i32>> {
        self.current_slot()
            .map(|slot| slot.tabs.iter().map(|tab| tab.browser_id).collect())
            .unwrap_or_default()
    }

    pub fn tab_count(&self) -> usize {
        self.current_slot().map(|slot| slot.tabs.len()).unwrap_or(0)
    }

    pub fn tab_count_in(&self, window: u32) -> usize {
        self.slot(window).map(|slot| slot.tabs.len()).unwrap_or(0)
    }

    pub fn active_tab(&self) -> usize {
        self.current_slot().map(|slot| slot.active).unwrap_or(0)
    }

    pub fn active_tab_in(&self, window: u32) -> usize {
        self.slot(window).map(|slot| slot.active).unwrap_or(0)
    }

    pub fn set_active_in(&mut self, window: u32, index: usize) {
        if let Some(slot) = self.slot_mut(window) {
            if index != slot.active {
                slot.last_active = Some(slot.active);
            }
            slot.active = index;
        }
    }

    /// Where `tab-focus last` goes. `None` until a second tab has been shown.
    pub fn last_active_tab(&self) -> Option<usize> {
        self.current_slot()
            .and_then(|slot| slot.last_active.filter(|index| *index < slot.tabs.len()))
    }

    /// The address of a tab, as the display handler last reported it.
    /// The title of the tab at `index`, for the status line on a switch.
    pub fn tab_title(&self, index: usize) -> Option<String> {
        self.current_slot()
            .and_then(|slot| slot.tabs.get(index))
            .map(|tab| tab.title.clone())
    }

    pub fn tab_url(&self, index: usize) -> Option<String> {
        self.current_slot()
            .and_then(|slot| slot.tabs.get(index))
            .map(|tab| tab.url.clone())
    }

    pub fn tab_url_in(&self, window: u32, index: usize) -> Option<String> {
        self.slot(window)
            .and_then(|slot| slot.tabs.get(index))
            .map(|tab| tab.url.clone())
    }

    pub fn tab_title_in(&self, window: u32, index: usize) -> Option<String> {
        self.slot(window)
            .and_then(|slot| slot.tabs.get(index))
            .map(|tab| tab.title.clone())
    }

    /// The `depth`-th most recently closed tab's URL, removed from the undo stack. Depth 1 is the
    /// tab closed last, which is what a bare `u` wants; `2u` reaches the one before it
    /// (`commands.py:831-861`, where the count *is* the depth).
    pub fn take_closed_tab(&mut self, depth: usize) -> Option<String> {
        let index = self.closed.len().checked_sub(depth.max(1))?;
        Some(self.closed.remove(index))
    }

    /// Moves the showing tab to `to`, keeping it the showing one.
    ///
    /// Only bru's own order changes; the tabs' order among the window's children does not, and does
    /// not need to. One tab is visible at a time, so the window stacks nothing — what the strip
    /// draws comes from [`BruState::tabs_json`], which reads this vector.
    pub fn move_tab(&mut self, from: usize, to: usize) {
        let Some(slot) = self.current_slot_mut() else {
            return;
        };
        if from >= slot.tabs.len() || to >= slot.tabs.len() || from == to {
            return;
        }
        let tab = slot.tabs.remove(from);
        slot.tabs.insert(to, tab);
        slot.active = to;
        slot.last_active = None;
    }

    /// Removes every tab but the showing one and hands their views back to be dropped.
    ///
    /// A pinned tab survives unless `force`. qutebrowser's `:tab-only` takes `--pinned
    /// prompt|close|keep` and defaults to `prompt` (`commands.py:780-826`); bru has no yes/no mode
    /// to prompt in, so the default is the answer a prompt would most likely get — keep it — and
    /// `-f` is the way to say otherwise.
    pub fn take_other_tabs(&mut self, force: bool) -> Vec<BrowserView> {
        let Some(slot) = self.current_slot_mut() else {
            return Vec::new();
        };
        if slot.tabs.is_empty() {
            return Vec::new();
        }
        let active = slot.active;
        let mut taken = Vec::new();
        let mut kept = Vec::new();
        let mut closed = Vec::new();
        let mut new_active = 0;
        for (index, tab) in std::mem::take(&mut slot.tabs).into_iter().enumerate() {
            if index == active {
                new_active = kept.len();
                kept.push(tab);
            } else if tab.pinned && !force {
                kept.push(tab);
            } else {
                closed.push(tab.url.clone());
                taken.push(tab.view.clone());
            }
        }
        slot.tabs = kept;
        slot.active = new_active;
        slot.last_active = None;
        self.closed.extend(closed);
        taken
    }

    /// Appends a tab to a named window and answers its index in that window's strip. `None` when
    /// there is no such window — a `:tab-give 7` typed at a window that has since closed.
    pub fn push_tab_in(&mut self, window: u32, view: BrowserView) -> Option<usize> {
        let slot = self.slot_mut(window)?;
        slot.tabs.push(Tab {
            view,
            browser_id: None,
            title: String::new(),
            url: String::new(),
            pinned: false,
            muted: false,
        });
        Some(slot.tabs.len() - 1)
    }

    /// Removes the showing tab and moves the selection to the one that takes its place.
    pub fn take_active_tab(&mut self) -> Option<BrowserView> {
        let window = self.current_window_id()?;
        let tab = self.detach_active_tab_in(window)?;
        // Kept so `u` can open it again. Only the URL — see `BruState::closed`.
        self.closed.push(tab.url.clone());
        Some(tab.view)
    }

    /// The same removal without the undo entry: the tab is not being closed, it is being handed to
    /// another window. Whole, because `tab-give` has to put it back somewhere — a `BrowserView`
    /// alone would lose the pin, the mute and the browser id.
    ///
    /// **The window is named rather than assumed**, and that is not tidiness: `window::create` makes
    /// the window it opens current, so a `gD` that detaches into a new one would otherwise take the
    /// showing tab of the window it had just made. Measured — the first run of `gD` reported
    /// `windows=[0:2 1:0]`, the tab still in the window it was supposed to leave.
    pub(crate) fn detach_active_tab_in(&mut self, window: u32) -> Option<Tab> {
        let slot = self.slot_mut(window)?;
        if slot.tabs.is_empty() {
            return None;
        }
        let tab = slot.tabs.remove(slot.active);
        if slot.active >= slot.tabs.len() && !slot.tabs.is_empty() {
            slot.active = slot.tabs.len() - 1;
        }
        slot.last_active = None;
        Some(tab)
    }

    /// Puts a whole tab into a named window, keeping everything it carried, and answers its index.
    pub(crate) fn attach_tab_in(&mut self, window: u32, tab: Tab) -> Option<usize> {
        let slot = self.slot_mut(window)?;
        slot.tabs.push(tab);
        Some(slot.tabs.len() - 1)
    }
}

/// Opens a tab on `url`. `background` leaves the current one showing, the way qutebrowser's
/// `:open -b` does.
pub fn new_tab(state: &SharedState, url: &str, background: bool) {
    let target = state
        .lock()
        .expect("state mutex poisoned")
        .current_window_id();
    if let Some(target) = target {
        new_tab_in(state, target, url, background);
    }
}

/// Opens a tab on `url` in a named window.
///
/// This is the entry point the popup workstream wants: a `on_before_popup` that has decided which
/// window a `target="_blank"` belongs to says so here rather than assuming the current one. It is
/// safe to call from a posted UI task and **not** from inside a message-router query handler — it
/// creates a browser (CEF-NOTES trap 12).
pub fn new_tab_in(state: &SharedState, window_id: u32, url: &str, background: bool) {
    let (client, window, layout) = {
        let state = state.lock().expect("state mutex poisoned");
        (
            state.client(),
            state.window_handle(window_id),
            state.layout_handle(window_id),
        )
    };
    let Some(mut client) = client else {
        return;
    };

    let mut delegate = BruBrowserViewDelegate::new(state.clone());
    let settings = BrowserSettings::default();
    let Some(view) = browser_view_create(
        Some(&mut client),
        Some(&CefString::from(url)),
        Some(&settings),
        None,
        None,
        Some(&mut delegate),
    ) else {
        return;
    };

    let Some(index) = state
        .lock()
        .expect("state mutex poisoned")
        .push_tab_in(window_id, view.clone())
    else {
        return;
    };

    // At startup the first tab is made before there is a window to put it in; `attach_all_in` picks
    // it up once the window exists.
    if let Some(window) = window {
        attach(&window, layout.as_ref(), &view, index);
    }

    if !background {
        select_in(state, window_id, index);
        return;
    }

    // A background tab is in the strip, and in the count the bar reports, the moment it is opened —
    // not when its page eventually commits an address. `select_in` is what pushes for a foreground
    // tab and this branch pushed nothing at all, so until now the only thing that told the chrome
    // about an `:open -b` was `on_address_change` in `keys.rs`, one network round trip later.
    let tabs = state
        .lock()
        .expect("state mutex poisoned")
        .tabs_json_in(window_id);
    crate::ipc::set_tabs_for(window_id, tabs);
}

/// Puts every tab of one window into it. Called once per window, from `on_window_created`.
pub fn attach_all_in(state: &SharedState, window_id: u32) {
    let (views, window, layout) = {
        let state = state.lock().expect("state mutex poisoned");
        (
            state.tab_views_in(window_id),
            state.window_handle(window_id),
            state.layout_handle(window_id),
        )
    };
    let Some(window) = window else {
        return;
    };
    for (index, view) in views.iter().enumerate() {
        attach(&window, layout.as_ref(), view, index);
    }
}

/// Child order in each window is: tab strip, one view per tab of *that window*, status bar.
/// Offsetting by one keeps the strip on top and the bar at the bottom whatever happens to the tabs
/// in between. Every window has its own strip as child 0, so the `+ 1` still holds once there is
/// more than one — the index is into that window's tabs, not into a global list.
fn attach(window: &Window, layout: Option<&BoxLayout>, view: &BrowserView, index: usize) {
    let mut view = View::from(view);
    window.add_child_view_at(Some(&mut view), index as i32 + 1);
    if let Some(layout) = layout {
        layout.set_flex_for_view(Some(&mut view), 1);
    }
    view.set_visible(0);
}

/// Shows one tab of the current window and hides the rest.
pub fn select(state: &SharedState, index: usize) {
    let target = state
        .lock()
        .expect("state mutex poisoned")
        .current_window_id();
    if let Some(target) = target {
        select_in(state, target, index);
    }
}

/// Shows one tab of a named window. It does **not** make that window current: a background window
/// finishing a load and choosing a tab must not steal the keyboard from the one in front.
pub fn select_in(state: &SharedState, window_id: u32, index: usize) {
    let views = {
        let mut state = state.lock().expect("state mutex poisoned");
        if index >= state.tab_count_in(window_id) {
            return;
        }
        state.set_active_in(window_id, index);
        state.tab_views_in(window_id)
    };

    for (i, view) in views.iter().enumerate() {
        View::from(view).set_visible(i32::from(i == index));
    }

    // Visibility alone does not move focus, and a hidden view that keeps it swallows every key —
    // the new tab would look right and answer nothing.
    View::from(&views[index]).request_focus();

    // The bar's scroll percentage and match count belong to the page that was showing, and the new
    // tab is somewhere else in a document of its own. Clearing them is what stops `[73%]` sitting
    // over a tab that is at the top; the new tab's own position arrives as soon as it is scrolled.
    crate::scroll::forget();
    crate::find::forget();

    // And the address and title, which otherwise only move when a page navigates: the display
    // handler fires on navigation, and switching tabs is not one. Without this the status line keeps
    // the URL of the tab you just left — measured after the stage-2 merge, with the bar reading
    // example.com over a vesti.bg page.
    let (url, title, tabs) = {
        let state = state.lock().expect("state mutex poisoned");
        (
            state.tab_url_in(window_id, index).unwrap_or_default(),
            state.tab_title_in(window_id, index).unwrap_or_default(),
            state.tabs_json_in(window_id),
        )
    };
    crate::ipc::set_url_for(window_id, url);
    crate::ipc::set_title_for(window_id, title);
    // Which tab the strip draws as selected is per window too, and a switch fires no display
    // callback to push it.
    crate::ipc::set_tabs_for(window_id, tabs);
}

pub fn next_tab(state: &SharedState) {
    let (active, count) = {
        let state = state.lock().expect("state mutex poisoned");
        (state.active_tab(), state.tab_count())
    };
    if count == 0 {
        return;
    }
    select(state, (active + 1) % count);
}

pub fn prev_tab(state: &SharedState) {
    let (active, count) = {
        let state = state.lock().expect("state mutex poisoned");
        (state.active_tab(), state.tab_count())
    };
    if count == 0 {
        return;
    }
    select(state, (active + count - 1) % count);
}

/// Closes every tab but the showing one — `co`, qutebrowser's `:tab-only`.
///
/// The views come out of the window and are dropped, exactly as [`close_current`] does it, and for
/// the same reason: `host.close_browser` on a Views browser closes the window it is parented to.
///
/// Pinned tabs stay unless `force` — see [`BruState::take_other_tabs`].
pub fn close_others(state: &SharedState, force: bool) {
    let (closed, window, tabs, active, window_id) = {
        let mut state = state.lock().expect("state mutex poisoned");
        let Some(window_id) = state.current_window_id() else {
            return;
        };
        let closed = state.take_other_tabs(force);
        (
            closed,
            state.window(),
            state.tabs_json(),
            state.active_tab(),
            window_id,
        )
    };
    if closed.is_empty() {
        return;
    }
    for view in &closed {
        if let Some(window) = &window {
            window.remove_child_view(Some(&mut View::from(view)));
        }
    }
    drop(closed);

    crate::ipc::set_tabs_for(window_id, tabs);
    // Not 0: pinned tabs may have stayed in front of the one that is showing.
    select_in(state, window_id, active);
}

/// `<Ctrl-p>` — pin or unpin the showing tab.
///
/// The flag is bru's; nothing in CEF has a concept of a pinned browser. What it buys is the two
/// close paths refusing to take the tab away without `-f`, and the `pinned` class on the strip,
/// which `chrome/chrome.css` already had colours for.
pub fn toggle_pin(state: &SharedState) {
    let tabs = {
        let mut state = state.lock().expect("state mutex poisoned");
        let index = state.active_tab();
        state.toggle_tab_pinned(index);
        state.tabs_json()
    };
    crate::ipc::set_tabs(tabs);
}

/// `<Alt-m>` — mute or unmute the showing tab.
///
/// `host.set_audio_muted` (bindings 12784) is the CEF side, and it is called after the lock is
/// dropped like every other CEF call in this file.
pub fn toggle_mute(state: &SharedState) {
    let (muted, browser, tabs) = {
        let mut state = state.lock().expect("state mutex poisoned");
        let index = state.active_tab();
        let muted = state.toggle_tab_muted(index);
        let browser = state.active_browser();
        (muted, browser, state.tabs_json())
    };
    if let Some(host) = browser.and_then(|browser| browser.host()) {
        host.set_audio_muted(i32::from(muted));
    }
    crate::ipc::set_tabs(tabs);
}

/// Re-apply a tab's mute flag to the browser CEF eventually made for it — the one thing a restored
/// session cannot do at the moment it creates the tab, because `browser_view_create` returns before
/// the browser exists.
pub fn apply_mute(state: &SharedState, index: usize) {
    let (muted, browser) = {
        let mut state = state.lock().expect("state mutex poisoned");
        let muted = state.tab_muted(index);
        let id = state.tab_browser_ids().get(index).copied().flatten();
        (muted, id.and_then(|id| state.browser_with_id(id)))
    };
    if !muted {
        return;
    }
    if let Some(host) = browser.and_then(|browser| browser.host()) {
        host.set_audio_muted(1);
    }
}

/// Moves the showing tab to `to` in the strip — `gm`, `gJ`, `gK`.
pub fn move_current(state: &SharedState, to: usize) {
    let tabs = {
        let mut state = state.lock().expect("state mutex poisoned");
        let from = state.active_tab();
        state.move_tab(from, to);
        state.tabs_json()
    };
    crate::ipc::set_tabs(tabs);
}

/// Closes the showing tab. Closing the last one closes the window, which is what the plan settled
/// on — qutebrowser's `tabs.last_close` default keeps a blank tab instead, and that is
/// DECISIONS.md item 6, still open.
pub fn close_current(state: &SharedState, force: bool) {
    let (closed, remaining, window, active, window_id) = {
        let mut state = state.lock().expect("state mutex poisoned");
        // A pinned tab is not closed by a bare `d`. qutebrowser prompts here
        // (`tabbedbrowser.py:431`); bru has no yes/no mode, so it says why and does nothing, and
        // `:tab-close -f` is the way through.
        if state.tab_pinned(state.active_tab()) && !force {
            eprintln!("bru: tab is pinned — :tab-close -f to close it anyway");
            return;
        }
        let Some(window_id) = state.current_window_id() else {
            return;
        };
        let closed = state.take_active_tab();
        (
            closed,
            state.tab_count(),
            state.window(),
            state.active_tab(),
            window_id,
        )
    };
    let Some(closed) = closed else {
        return;
    };

    // A tab is closed by taking its view out of the window and letting the last reference to that
    // view go. **Not** by `host.close_browser`: a Views browser treats the window it is parented
    // to as its host widget, so closing the browser closes the whole window. Measured 2026-08-06 —
    // closing one of three tabs ran `on_before_close` five times, once per browser in the window,
    // and the process quit. Calling it *after* `remove_child_view` is worse still: CEF then CHECKs
    // in `CefBrowserPlatformDelegateViews::CloseHostWindow` on the widget the view no longer has,
    // and the process aborts with SIGTRAP.
    if let Some(window) = &window {
        window.remove_child_view(Some(&mut View::from(&closed)));
    }
    drop(closed);

    // The last tab of *this* window closes *this* window. The process only ends when the last
    // window's last browser does — `BruState::on_before_close` counts browsers, not tabs.
    if remaining == 0 {
        if let Some(window) = window {
            window.close();
        }
        return;
    }

    select_in(state, window_id, active);
}

/// `gD` — take the showing tab out of its window and put it in another one, whole.
///
/// `to` is `None` for qutebrowser's bare `:tab-give`, which detaches into a **new** window
/// (`commands.py:460-500`), and `Some(id)` for `:tab-give <win-id>`.
///
/// The move is a re-parent, not a clone: the same `BrowserView`, and therefore the same browser,
/// the same renderer and the same navigation history. Cloning the URL instead would silently lose
/// `H`/`L` on the moved tab, which reads as a bug rather than as a gap.
///
/// **The reference is held across the re-parent, and that is the whole trick.** CEF-NOTES says a
/// tab is closed by `remove_child_view` *and then dropping the view*; here the view is removed and
/// immediately added to another window while `tab` still owns it, so the drop that would close the
/// browser never happens. Measured — see the report.
pub fn give_tab(state: &SharedState, to: Option<u32>) {
    let (from_window, count) = {
        let state = state.lock().expect("state mutex poisoned");
        match state.current_window_id() {
            Some(id) => (id, state.tab_count()),
            None => return,
        }
    };

    if to == Some(from_window) {
        crate::message::error("tab-give: that is the window the tab is already in");
        return;
    }

    // qutebrowser refuses to detach the only tab of a window (`commands.py:483`): the window it
    // came from would close as the new one opened, which is a no-op with a flicker in it.
    if to.is_none() && count < 2 {
        crate::message::error("tab-give: cannot detach from a window with only one tab");
        return;
    }

    let target = match to {
        Some(id) => {
            if state.lock().expect("state mutex poisoned").slot(id).is_none() {
                crate::message::error(&format!("tab-give: there is no window with id {id}"));
                return;
            }
            id
        }
        // A window with no tab in it yet: the tab about to be handed over is its first, so opening
        // it on a URL would load a page only to hide it a moment later.
        None => crate::window::create(state, crate::window::FirstTab::None),
    };

    // Out of bru's book-keeping first, and out of the old window's children second. The `Tab` is
    // ours for the whole of the rest of this function, so the view's last reference is never
    // dropped and the browser is never closed.
    let (tab, old_window, remaining, old_active) = {
        let mut state = state.lock().expect("state mutex poisoned");
        let tab = state.detach_active_tab_in(from_window);
        (
            tab,
            state.window_handle(from_window),
            state.tab_count_in(from_window),
            state.active_tab_in(from_window),
        )
    };
    let Some(tab) = tab else {
        return;
    };

    if let Some(old_window) = &old_window {
        old_window.remove_child_view(Some(&mut View::from(&tab.view)));
    }

    let view = tab.view.clone();
    let (index, new_window, layout) = {
        let mut state = state.lock().expect("state mutex poisoned");
        let index = state.attach_tab_in(target, tab);
        (
            index,
            state.window_handle(target),
            state.layout_handle(target),
        )
    };
    let Some(index) = index else {
        return;
    };

    // Adding a browser view that already has a browser does *not* create a second one — the browser
    // follows its view to the new window. Nothing else in bru re-parents a view, so this is the one
    // `add_child_view_at` whose browser already exists.
    if let Some(new_window) = &new_window {
        attach(new_window, layout.as_ref(), &view, index);
    }

    select_in(state, target, index);
    if remaining > 0 {
        // The window it came from shows whatever took its place.
        select_in(state, from_window, old_active);
        let tabs = state
            .lock()
            .expect("state mutex poisoned")
            .tabs_json_in(from_window);
        crate::ipc::set_tabs_for(from_window, tabs);
    } else {
        // It gave away its last tab, so it goes — the same rule `close_current` follows. Only
        // reachable through `:tab-give <id>`; the detaching spelling refuses a single-tab window
        // above, because opening a window as another closes is a flicker and nothing else.
        crate::window::close(state, from_window);
    }

    // And the receiving window comes to the front, which is what "give" means when you are the one
    // pressing `gD`.
    focus(state, target);
}

/// Bring a window to the front and make it the one commands act on.
pub fn focus(state: &SharedState, window_id: u32) {
    let window = {
        let mut state = state.lock().expect("state mutex poisoned");
        if !state.focus_window(window_id) {
            return;
        }
        state.window_handle(window_id)
    };
    if let Some(window) = window {
        window.show();
        window.activate();
    }
}

/// Select a tab on the next turn of the UI loop.
///
/// The one caller is a click on the tab strip, which arrives inside the message router's query
/// handler — and CEF-NOTES trap 12 forbids touching a browser from there: `select` focuses a view,
/// the router holds `browser_query_info_map` across the handler, and `on_before_browse` wants that
/// same lock. Posting steps outside it.
///
/// `from_browser` is the chrome browser the click came from, and it is what says *which window's*
/// strip was clicked. It is resolved in the posted task rather than in the handler, so the handler
/// takes no lock at all.
pub fn schedule_select(from_browser: Option<i32>, index: usize) {
    let mut task = SelectTab::new(from_browser, index);
    post_task(ThreadId::UI, Some(&mut task));
}

wrap_task! {
    struct SelectTab {
        from_browser: Option<i32>,
        index: usize,
    }

    impl Task {
        fn execute(&self) {
            debug_assert_ne!(currently_on(ThreadId::UI), 0);
            let Some(state) = BruState::instance() else {
                return;
            };
            let window = {
                let guard = state.lock().expect("state mutex poisoned");
                self.from_browser
                    .and_then(|id| guard.window_of_browser(id))
                    .or_else(|| guard.current_window_id())
            };
            if let Some(window) = window {
                select_in(&state, window, self.index);
            }
        }
    }
}
