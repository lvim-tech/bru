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
    view: BrowserView,
    /// Learned from `BrowserViewDelegate::on_browser_created`, not at creation: `browser_view_create`
    /// returns before the browser exists, so this is `None` for the moment in between.
    browser_id: Option<i32>,
    title: String,
    url: String,
}

/// The plain state operations. None of these touch CEF.
impl BruState {
    pub fn tab_views(&self) -> Vec<BrowserView> {
        self.tabs.iter().map(|tab| tab.view.clone()).collect()
    }

    /// Ties a browser to the tab whose view it was made for. Called once per tab.
    pub fn note_tab_browser(&mut self, view: &mut BrowserView, identifier: i32) {
        for tab in &mut self.tabs {
            if tab.view.is_same(Some(&mut View::from(&*view))) != 0 {
                tab.browser_id = Some(identifier);
                return;
            }
        }
    }

    fn tab_index_of(&self, identifier: i32) -> Option<usize> {
        self.tabs
            .iter()
            .position(|tab| tab.browser_id == Some(identifier))
    }

    /// True when `identifier` is the browser of the tab currently showing — which is the only tab
    /// whose address and title belong in the status line.
    pub fn is_active_browser(&self, identifier: i32) -> bool {
        self.tab_index_of(identifier) == Some(self.active)
    }

    /// The browser id of the showing tab, if it has one yet.
    pub fn active_tab_browser_id(&self) -> Option<i32> {
        self.tabs.get(self.active).and_then(|tab| tab.browser_id)
    }

    /// Records a tab's address. Returns false when the browser is not a tab at all, which is how a
    /// chrome strip reporting its own bru:// URL is kept out of the status line.
    pub fn set_tab_url(&mut self, identifier: i32, url: String) -> bool {
        match self.tab_index_of(identifier) {
            Some(index) => {
                self.tabs[index].url = url;
                true
            }
            None => false,
        }
    }

    pub fn set_tab_title(&mut self, identifier: i32, title: String) -> bool {
        match self.tab_index_of(identifier) {
            Some(index) => {
                self.tabs[index].title = title;
                true
            }
            None => false,
        }
    }

    /// What the tab strip renders, in strip order.
    pub fn tabs_json(&self) -> String {
        let entries: Vec<String> = self
            .tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| {
                format!(
                    "{{\"title\":\"{}\",\"url\":\"{}\",\"active\":{}}}",
                    crate::ipc::json_escape(&tab.title),
                    crate::ipc::json_escape(&tab.url),
                    index == self.active,
                )
            })
            .collect();
        format!("{{\"tabs\":[{}]}}", entries.join(","))
    }

    pub fn tab_count(&self) -> usize {
        self.tabs.len()
    }

    pub fn active_tab(&self) -> usize {
        self.active
    }

    pub fn set_active(&mut self, index: usize) {
        if index != self.active {
            self.last_active = Some(self.active);
        }
        self.active = index;
    }

    /// Where `tab-focus last` goes. `None` until a second tab has been shown.
    pub fn last_active_tab(&self) -> Option<usize> {
        self.last_active.filter(|index| *index < self.tabs.len())
    }

    /// The address of a tab, as the display handler last reported it.
    /// The title of the tab at `index`, for the status line on a switch.
    pub fn tab_title(&self, index: usize) -> Option<String> {
        self.tabs.get(index).map(|tab| tab.title.clone())
    }

    pub fn tab_url(&self, index: usize) -> Option<String> {
        self.tabs.get(index).map(|tab| tab.url.clone())
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
        if from >= self.tabs.len() || to >= self.tabs.len() || from == to {
            return;
        }
        let tab = self.tabs.remove(from);
        self.tabs.insert(to, tab);
        self.active = to;
        self.last_active = None;
    }

    /// Removes every tab but the showing one and hands their views back to be dropped.
    pub fn take_other_tabs(&mut self) -> Vec<BrowserView> {
        if self.tabs.is_empty() {
            return Vec::new();
        }
        let active = self.active;
        let mut taken = Vec::new();
        let mut kept = Vec::new();
        for (index, tab) in std::mem::take(&mut self.tabs).into_iter().enumerate() {
            if index == active {
                kept.push(tab);
            } else {
                self.closed.push(tab.url.clone());
                taken.push(tab.view.clone());
            }
        }
        self.tabs = kept;
        self.active = 0;
        self.last_active = None;
        taken
    }

    pub fn push_tab(&mut self, view: BrowserView) -> usize {
        self.tabs.push(Tab {
            view,
            browser_id: None,
            title: String::new(),
            url: String::new(),
        });
        self.tabs.len() - 1
    }

    /// Removes the showing tab and moves the selection to the one that takes its place.
    pub fn take_active_tab(&mut self) -> Option<BrowserView> {
        if self.tabs.is_empty() {
            return None;
        }
        let tab = self.tabs.remove(self.active);
        // Kept so `u` can open it again. Only the URL — see `BruState::closed`.
        self.closed.push(tab.url.clone());
        if self.active >= self.tabs.len() && !self.tabs.is_empty() {
            self.active = self.tabs.len() - 1;
        }
        self.last_active = None;
        Some(tab.view)
    }
}

/// Opens a tab on `url`. `background` leaves the current one showing, the way qutebrowser's
/// `:open -b` does.
pub fn new_tab(state: &SharedState, url: &str, background: bool) {
    let (client, window, layout) = {
        let state = state.lock().expect("state mutex poisoned");
        (state.client(), state.window(), state.layout())
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

    let index = state
        .lock()
        .expect("state mutex poisoned")
        .push_tab(view.clone());

    // At startup the first tab is made before there is a window to put it in; `attach_all` picks
    // it up once the window exists.
    if let Some(window) = window {
        attach(&window, layout.as_ref(), &view, index);
    }

    if !background {
        select(state, index);
    }
}

/// Puts every tab into the window. Called once, from `on_window_created`.
pub fn attach_all(state: &SharedState) {
    let (views, window, layout) = {
        let state = state.lock().expect("state mutex poisoned");
        (state.tab_views(), state.window(), state.layout())
    };
    let Some(window) = window else {
        return;
    };
    for (index, view) in views.iter().enumerate() {
        attach(&window, layout.as_ref(), view, index);
    }
}

/// Child order in the window is: tab strip, one view per tab, status bar. Offsetting by one keeps
/// the strip on top and the bar at the bottom whatever happens to the tabs in between.
fn attach(window: &Window, layout: Option<&BoxLayout>, view: &BrowserView, index: usize) {
    let mut view = View::from(view);
    window.add_child_view_at(Some(&mut view), index as i32 + 1);
    if let Some(layout) = layout {
        layout.set_flex_for_view(Some(&mut view), 1);
    }
    view.set_visible(0);
}

/// Shows one tab and hides the rest.
pub fn select(state: &SharedState, index: usize) {
    let views = {
        let mut state = state.lock().expect("state mutex poisoned");
        if index >= state.tab_count() {
            return;
        }
        state.set_active(index);
        state.tab_views()
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
    let (url, title) = {
        let state = state.lock().expect("state mutex poisoned");
        (
            state.tab_url(index).unwrap_or_default(),
            state.tab_title(index).unwrap_or_default(),
        )
    };
    crate::ipc::set_url(url);
    crate::ipc::set_title(title);
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
pub fn close_others(state: &SharedState) {
    let (closed, window, tabs) = {
        let mut state = state.lock().expect("state mutex poisoned");
        let closed = state.take_other_tabs();
        (closed, state.window(), state.tabs_json())
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

    crate::ipc::set_tabs(tabs);
    select(state, 0);
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
pub fn close_current(state: &SharedState) {
    let (closed, remaining, window, active) = {
        let mut state = state.lock().expect("state mutex poisoned");
        let closed = state.take_active_tab();
        (
            closed,
            state.tab_count(),
            state.window(),
            state.active_tab(),
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

    if remaining == 0 {
        if let Some(window) = window {
            window.close();
        }
        return;
    }

    select(state, active);
}

/// Select a tab on the next turn of the UI loop.
///
/// The one caller is a click on the tab strip, which arrives inside the message router's query
/// handler — and CEF-NOTES trap 12 forbids touching a browser from there: `select` focuses a view,
/// the router holds `browser_query_info_map` across the handler, and `on_before_browse` wants that
/// same lock. Posting steps outside it.
pub fn schedule_select(index: usize) {
    let mut task = SelectTab::new(index);
    post_task(ThreadId::UI, Some(&mut task));
}

wrap_task! {
    struct SelectTab {
        index: usize,
    }

    impl Task {
        fn execute(&self) {
            debug_assert_ne!(currently_on(ThreadId::UI), 0);
            if let Some(state) = BruState::instance() {
                select(&state, self.index);
            }
        }
    }
}
