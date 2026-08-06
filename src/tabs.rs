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
        self.active = index;
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
        if self.active >= self.tabs.len() && !self.tabs.is_empty() {
            self.active = self.tabs.len() - 1;
        }
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

    // The bar's scroll percentage belongs to the page that was showing, and the new tab is
    // somewhere else in a document of its own. Clearing it is what stops `[73%]` sitting over a tab
    // that is at the top; the new tab's own position arrives as soon as it is scrolled.
    crate::scroll::forget();
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
