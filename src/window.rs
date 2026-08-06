//! Top-level windows.
//!
//! Built through CEF's Views framework rather than a native X11/Wayland surface: Views gives a
//! window CEF owns and lays out itself, and that is what the tab strip and the status bar are added
//! to.
//!
//! There is more than one of them. [`create`] is the only way to make one, and it is the same
//! function at startup and at `:open -w`, so a second window is not a special case of the first —
//! which is what stops the two drifting apart. Everything a window owns lives in
//! `state::WindowState`; nothing here keeps a window in a field.
//!
//! **The order in [`create`] is not arbitrary.** The slot is allocated first, because the two chrome
//! delegates and the window delegate all need its id before they are constructed. The chrome views
//! are made next, and the first tab after them, because `browser_view_create` makes no browser —
//! `window.add_child_view` does, synchronously, reaching `LifeSpanHandler::on_after_created` before
//! it returns (CEF-NOTES). So no lock is held across any call below.

use cef::*;
use std::cell::RefCell;
use std::sync::{Arc, Mutex};

use crate::state::BruState;
use crate::tabs::{self, SharedState};

/// The two chrome documents, served by `chrome.rs` over the scheme registered in every process.
const TOP_URL: &str = "bru://chrome/top.html";
const BOTTOM_URL: &str = "bru://chrome/bottom.html";

/// Chrome strip heights, in logical pixels.
const TOP_HEIGHT: i32 = 40;
const BOTTOM_HEIGHT: i32 = 24;

/// What a new window opens with.
pub enum FirstTab<'a> {
    /// Exactly this page — `:open -w`, `gD`'s receiving window when it is given a URL, `U`.
    Url(&'a str),
    /// Whatever `--restore` says, and this page when it says nothing. The first window only.
    Startup(&'a str),
    /// Nothing at all. `tab-give` uses it: the tab about to be handed over is the window's first,
    /// and loading a page only to hide it a moment later would be a flash of the wrong site.
    None,
}

/// Make a window, and answer bru's own identifier for it.
///
/// Runs on the UI thread, holds no lock across a CEF call, and must not be called from inside a
/// message-router query handler — it creates browsers (CEF-NOTES trap 12).
///
/// # What a popup handler should call
///
/// `LifeSpanHandler::on_before_popup` gets the browser the popup was opened *from*, and that browser
/// names its window: `BruState::window_of_browser(id) -> Option<u32>`. From there,
///
/// - a popup that should be a tab of the window it came from is
///   [`crate::tabs::new_tab_in`]`(state, window, url, background)`;
/// - a popup that should be a window of its own is [`open`]`(state, url) -> u32`, which creates it
///   and brings it to the front, or [`create`] with [`FirstTab::None`] when the tab is going to be
///   supplied some other way;
/// - bringing an existing window forward is [`crate::tabs::focus`]`(state, window)`.
///
/// None of these may run inside `on_before_popup` itself if that callback is reached from a query
/// handler — post to `ThreadId::UI` first. `on_before_popup` proper is a CEF callback on the UI
/// thread and is fine.
pub fn create(state: &SharedState, first: FirstTab<'_>) -> u32 {
    debug_assert_ne!(currently_on(ThreadId::UI), 0);

    // Before anything else: the three delegates below all carry this id, and the first tab needs
    // somewhere to be pushed.
    let window_id = state
        .lock()
        .expect("state mutex poisoned")
        .open_window_slot();

    let settings = BrowserSettings::default();
    let mut client = state.lock().expect("state mutex poisoned").client();

    // All three views share one Client, so one set of handlers serves the page and the chrome.
    // Which browser an event came from is answered by its identifier, not by its handler. The last
    // argument is `grows` — see `COMPLETION_HEIGHT`. Only the bottom strip does; the tab strip's
    // height is its own.
    let mut top_delegate =
        BruChromeViewDelegate::new(state.clone(), window_id, TOP_HEIGHT, false);
    let top_view = browser_view_create(
        client.as_mut(),
        Some(&CefString::from(TOP_URL)),
        Some(&settings),
        None,
        None,
        Some(&mut top_delegate),
    );

    match first {
        // --- sessions (merge: this arm belongs to src/session.rs's workstream) ------------------
        // `--restore=<name>` opens a saved session's tabs instead of the start page, the way
        // `qutebrowser --restore` does. It is decided here, before the first tab is made: opening
        // the start page and then closing it would flash the wrong site on every restore.
        FirstTab::Startup(url) => {
            if !crate::session::restore_at_startup(state) {
                tabs::new_tab_in(state, window_id, url, false);
            }
        }
        // --- end sessions -----------------------------------------------------------------------
        FirstTab::Url(url) => tabs::new_tab_in(state, window_id, url, false),
        FirstTab::None => {}
    }

    let mut bottom_delegate =
        BruChromeViewDelegate::new(state.clone(), window_id, BOTTOM_HEIGHT, true);
    let bottom_view = browser_view_create(
        client.as_mut(),
        Some(&CefString::from(BOTTOM_URL)),
        Some(&settings),
        None,
        None,
        Some(&mut bottom_delegate),
    );

    let mut window_delegate = BruWindowDelegate::new(
        state.clone(),
        window_id,
        RefCell::new(top_view),
        RefCell::new(bottom_view),
    );
    window_create_top_level(Some(&mut window_delegate));

    window_id
}

/// Open a window on `url` and bring it to the front — every `-w` spelling, and `U`.
pub fn open(state: &SharedState, url: &str) -> u32 {
    let window_id = create(state, FirstTab::Url(url));
    tabs::focus(state, window_id);
    window_id
}

/// Close one window. `:close` (`<Ctrl-Shift-W>`) — the window, not the application.
pub fn close(state: &SharedState, window_id: u32) {
    let window = state
        .lock()
        .expect("state mutex poisoned")
        .window_handle(window_id);
    if let Some(window) = window {
        window.close();
    }
}

/// Close every window, which is what ends the process: `BruState::on_before_close` stops the message
/// loop when the last browser in the last window has gone.
pub fn close_all(state: &SharedState) {
    let windows = state
        .lock()
        .expect("state mutex poisoned")
        .window_handles();
    for window in windows {
        window.close();
    }
}

// --- src/completers.rs ---------------------------------------------------------------------
/// How much taller than its own 24 px each window's **bottom** strip is asking to be, because the
/// completion table is open above that window's command line.
///
/// The strip is a CEF view with a fixed preferred height, so however tall the table's HTML grows
/// it is drawn inside 24 logical pixels and cannot be seen. Measured 2026-08-06 with a screenshot
/// of `:open du`: the command line read `:open du`, the payload had three categories in it, and
/// the bar was one row tall. `completers::resize_bar` writes here and invalidates the layout;
/// [`completion_height`] below adds it, and only for the strip built with `grows`, so the tab strip
/// keeps its own height.
///
/// It was one `AtomicI32` for every strip in the process, which held only because a relayout was
/// asked of the current window alone. Now that a window has its own mode, two of them can have a
/// command line open at once, and the second one to open would have set the height the first is
/// drawn at. A `Vec` behind a mutex rather than an atomic: this is read once per layout pass and
/// written once per change of the table, and neither is anywhere near the key path.
static COMPLETION_HEIGHTS: std::sync::Mutex<Vec<(u32, i32)>> = std::sync::Mutex::new(Vec::new());

/// Store a window's extra height and answer what it was. `completers::resize_bar` compares the two
/// and relayouts only when they differ.
pub fn set_completion_height(window: u32, height: i32) -> i32 {
    let Ok(mut heights) = COMPLETION_HEIGHTS.lock() else {
        return height;
    };
    match heights.iter_mut().find(|(id, _)| *id == window) {
        Some(slot) => std::mem::replace(&mut slot.1, height),
        None => {
            heights.push((window, height));
            0
        }
    }
}

fn completion_height(window: u32) -> i32 {
    COMPLETION_HEIGHTS
        .lock()
        .ok()
        .and_then(|heights| {
            heights
                .iter()
                .find(|(id, _)| *id == window)
                .map(|(_, height)| *height)
        })
        .unwrap_or(0)
}

/// Drop a window's entry, from `on_window_destroyed`. Without it the list grows by one row for
/// every window ever opened, and a later window that happened to reuse the id — nothing does today,
/// `next_window_id` only goes up — would inherit a height it never asked for.
fn forget_completion_height(window: u32) {
    if let Ok(mut heights) = COMPLETION_HEIGHTS.lock() {
        heights.retain(|(id, _)| *id != window);
    }
}
// --- end src/completers.rs -----------------------------------------------------------------

/// A `CefString` that survives being written into a struct CEF reads back.
///
/// `CefString::from(&str)` allocates and marks itself owned, and cef-rs's conversion back to the C
/// struct answers `None` for anything owned — `impl From<CefStringUtf16> for _cef_string_utf16_t`
/// keeps only the borrowed case and zeroes the rest. An out-parameter filled in the obvious way
/// therefore reaches CEF empty: measured 2026-08-06, `WAYLAND_DEBUG=1` showed `set_app_id("")`
/// after assigning `CefString::from("bru")`. Round-tripping through the raw pointer produces the
/// borrowed form, which does survive; the buffer is deliberately leaked, because CEF owns the
/// string once it has it and frees it through the dtor recorded inside. Four small strings, once
/// per window.
fn handover(text: &str) -> CefString {
    let owned = CefString::from(text);
    let raw: *const sys::_cef_string_utf16_t = (&owned).into();
    let borrowed = CefString::from(raw);
    std::mem::forget(owned);
    borrowed
}

/// What the toplevel is called when no tab has a title yet — at startup, and on a page that sets
/// none. It is also the suffix of every other title.
const APP_NAME: &str = "bru";

/// Put the showing tab's title on the window.
///
/// `window.set_title` was never called, so the compositor showed an empty toplevel: measured
/// 2026-08-06, `mmsg -g` reported `title ` with nothing after it. Wayland has no separate "browser
/// name" field — the toplevel title is the only string a taskbar or a window switcher has — so bru's
/// own name belongs in it, which is what every browser does and what qutebrowser's
/// `window.title_format` default (`configdata.yml:2675`) says: `{current_title} - qutebrowser`.
///
/// qutebrowser's default also opens with `{perc}`, the scroll percentage. bru's does not, and
/// deliberately: that would put a CEF call and a compositor round trip on every settled scroll
/// position, which is the one path this project exists to keep fast.
///
/// The one caller is [`crate::ipc::set_title_for`], which is reached from exactly the two places
/// that mean "the tab now showing has a title": the display handler, for a page that names itself,
/// and `tabs::select_in`, for a switch — a switch is not a navigation and fires no display callback.
///
/// `window_id` is not optional, and that is the point: a page loading in a background window used to
/// have nowhere else to put its title but the one toplevel, so a download finishing in window 2
/// renamed window 1.
pub fn set_title(window_id: u32, title: &str) {
    debug_assert_ne!(currently_on(ThreadId::UI), 0);

    let Some(state) = BruState::instance() else {
        return;
    };
    // The handle is taken and the lock let go before the Views call, like every other CEF call in
    // bru: a window callback taking the same mutex would deadlock.
    let window = state
        .lock()
        .expect("state mutex poisoned")
        .window_handle(window_id);
    let Some(window) = window else {
        return;
    };

    let title = title.trim();
    let title = if title.is_empty() {
        APP_NAME.to_string()
    } else {
        format!("{title} - {APP_NAME}")
    };
    window.set_title(Some(&CefString::from(title.as_str())));
}

wrap_window_delegate! {
    pub struct BruWindowDelegate {
        state: Arc<Mutex<BruState>>,
        // Which of bru's windows this delegate belongs to. Allocated by `create` *before* any of
        // the three views is made, because the two chrome delegates need it too — a strip has to be
        // able to say which window a key landed in (CEF-NOTES trap 11). A `///` here does not
        // compile: the wrap_ macros have no rule for `#[doc]` (trap 8).
        window_id: u32,
        top_view: RefCell<Option<BrowserView>>,
        bottom_view: RefCell<Option<BrowserView>>,
    }

    impl ViewDelegate {
        fn preferred_size(&self, _view: Option<&mut View>) -> Size {
            Size { width: 1280, height: 800 }
        }
    }

    impl PanelDelegate {}

    impl WindowDelegate {
        fn on_window_created(&self, window: Option<&mut Window>) {
            let (top, bottom) = (self.top_view.borrow(), self.bottom_view.borrow());
            let (Some(window), Some(top), Some(bottom)) = (window, top.as_ref(), bottom.as_ref())
            else {
                return;
            };

            // A vertical stack: tab strip, pages, status line. `horizontal: 0` is the vertical
            // orientation, and STRETCH on the cross axis is what makes each strip span the full
            // width — the default, START, would leave them at their preferred width instead.
            let settings = BoxLayoutSettings {
                horizontal: 0,
                cross_axis_alignment: AxisAlignment::STRETCH,
                ..Default::default()
            };
            let layout = window.set_to_box_layout(Some(&settings));

            let mut top = View::from(top);
            window.add_child_view(Some(&mut top));

            // Nothing else is ever handed the window or its layout; keep both where a tab opened
            // later can find them. The lock is let go before any tab is placed — see tabs.rs.
            self.state
                .lock()
                .expect("state mutex poisoned")
                .set_window_for(self.window_id, window.clone(), layout);

            // Tabs that already exist in *this* window: at startup, the one the command line asked
            // for, created before there was a window to put it in; at runtime, whatever `create`
            // opened before handing the slot over.
            tabs::attach_all_in(&self.state, self.window_id);

            let mut bottom = View::from(bottom);
            window.add_child_view(Some(&mut bottom));

            // A name before the first page has one. Without it the toplevel is mapped with an empty
            // title for as long as the first load takes, and a compositor that reads the title once
            // keeps the empty one.
            window.set_title(Some(&CefString::from(APP_NAME)));

            window.show();

            // Selecting shows the tab and takes focus. Without it the tab strip, as the first
            // child added, keeps focus and every key goes to a chrome page.
            let active = self
                .state
                .lock()
                .expect("state mutex poisoned")
                .active_tab_in(self.window_id);
            tabs::select_in(&self.state, self.window_id, active);
        }

        // Focus follows the compositor. `keys.rs` also names the window from the browser a key
        // arrived at, and the two agree — but a click on a page, or a `:` typed straight after
        // alt-tabbing, reaches this first.
        fn on_window_activation_changed(&self, _window: Option<&mut Window>, active: i32) {
            if active != 0 {
                self.state
                    .lock()
                    .expect("state mutex poisoned")
                    .focus_window(self.window_id);
            }
        }

        fn on_window_destroyed(&self, _window: Option<&mut Window>) {
            // The slot goes with the window, and the URLs it held go onto the closed-window stack
            // for `U`. Before the views are dropped, while the tabs are still listed.
            self.state
                .lock()
                .expect("state mutex poisoned")
                .forget_window(self.window_id);
            crate::ipc::forget_window(self.window_id);
            forget_completion_height(self.window_id);
            // Drop the views here, or the window outlives the browsers it holds.
            *self.top_view.borrow_mut() = None;
            *self.bottom_view.borrow_mut() = None;
        }

        // CEF's defaults for these three are 0, and a window that says it cannot be resized
        // advertises an xdg-toplevel whose minimum and maximum size are equal. dwl-derived
        // compositors — mango is one — float a window with a fixed size, so bru opened floating
        // over the tiled layout until these said 1. Measured 2026-08-06: `mmsg -g` reported
        // `floating 1` without them and `floating 0` with them.
        fn can_resize(&self, _window: Option<&mut Window>) -> i32 {
            1
        }

        fn can_maximize(&self, _window: Option<&mut Window>) -> i32 {
            1
        }

        fn can_minimize(&self, _window: Option<&mut Window>) -> i32 {
            1
        }

        // Without this the xdg-toplevel carries no app_id, so nothing on this desktop can name bru
        // in a window rule. CEF asks for these once, before it creates the toplevel.
        fn linux_window_properties(
            &self,
            _window: Option<&mut Window>,
            properties: Option<&mut LinuxWindowProperties>,
        ) -> i32 {
            let Some(properties) = properties else {
                return 0;
            };
            properties.wayland_app_id = handover("bru");
            properties.wm_class_class = handover("bru");
            properties.wm_class_name = handover("bru");
            properties.wm_role_name = handover("browser");
            1
        }

        fn can_close(&self, _window: Option<&mut Window>) -> i32 {
            // Ask every browser first: each may need to run beforeunload handlers. try_close_browser
            // both answers and starts the close, so all three have to be asked — short-circuiting on
            // the first 0 would leave the others open.
            // The views are collected under the lock and the lock is dropped before any of them is
            // asked to close — try_close_browser reaches bru's own life-span handler.
            let mut views = self
                .state
                .lock()
                .expect("state mutex poisoned")
                .tab_views_in(self.window_id);
            views.extend(self.top_view.borrow().clone());
            views.extend(self.bottom_view.borrow().clone());

            let mut closable = 1;
            for view in views {
                let ready = match view.browser() {
                    Some(browser) => match browser.host() {
                        Some(host) => host.try_close_browser(),
                        None => 1,
                    },
                    None => 1,
                };
                closable &= ready;
            }
            closable
        }
    }
}

// Every BrowserView bru puts in the window is Alloy style, and it has to be. From the CEF header
// documentation of cef_runtime_style_t: "a Chrome style Window can host at most one Chrome style
// BrowserView but potentially multiple Alloy style BrowserViews." Chrome style is the default, so
// the first attempt at the three-view layout drew only the tab strip — the page and the status line
// were created, added and never painted. Alloy is what bru wants anyway: the content layer with
// none of Chrome's own UI, which DESIGN.md rules out.
const VIEW_STYLE: RuntimeStyle = RuntimeStyle::ALLOY;

wrap_browser_view_delegate! {
    // The empty-struct shorthand the other wrap_ macros accept is not a rule this one has; it needs
    // the braces even with no fields.
    pub struct BruBrowserViewDelegate {
        state: Arc<Mutex<BruState>>,
    }

    impl ViewDelegate {}

    impl BrowserViewDelegate {
        fn browser_runtime_style(&self) -> RuntimeStyle {
            VIEW_STYLE
        }

        // Which browser ended up behind this tab. Asked here rather than after
        // `browser_view_create`, which returns before the browser exists at all, and rather than by
        // matching the frame URL, which races the first load. Without it the status line cannot tell
        // the page's address from a chrome strip's own bru:// URL.
        fn on_browser_created(
            &self,
            browser_view: Option<&mut BrowserView>,
            browser: Option<&mut Browser>,
        ) {
            let (Some(browser_view), Some(browser)) = (browser_view, browser) else {
                return;
            };
            self.state
                .lock()
                .expect("state mutex poisoned")
                .note_tab_browser(browser_view, browser.identifier());
        }
    }
}

// A chrome strip: a browser view that asks for a fixed height and tells the state which browser
// ended up behind it. `on_browser_created` is the reliable place for that — it hands over the
// browser the moment CEF makes it, where matching on a frame URL would race the first load.
wrap_browser_view_delegate! {
    pub struct BruChromeViewDelegate {
        state: Arc<Mutex<BruState>>,
        // Which window this strip draws. Without it a key landing on a strip could only be aimed
        // at "the showing tab", and with two windows that is a guess — trap 11 resolved against the
        // wrong window.
        window_id: u32,
        height: i32,
        // --- src/completers.rs ---------------------------------------------------------------
        // Whether this strip grows with the completion table. The bottom one does; the tab strip
        // must not, or an open completion would make the tabs 300px tall.
        grows: bool,
        // --- end src/completers.rs -----------------------------------------------------------
    }

    impl ViewDelegate {
        fn preferred_size(&self, _view: Option<&mut View>) -> Size {
            // Width is ignored: the box layout stretches the strip across the window. Only the
            // height is a real request.
// --- src/completers.rs ---------------------------------------------------------------------
            // This strip's own window, not whichever one is current: a layout pass runs for the
            // window being relayouted, and reading a shared value would make window 0's bar as tall
            // as window 1's table.
            let extra = if self.grows {
                completion_height(self.window_id)
            } else {
                0
            };
            Size { width: 1280, height: self.height + extra }
// --- end src/completers.rs -----------------------------------------------------------------
        }
    }

    impl BrowserViewDelegate {
        fn on_browser_created(
            &self,
            _browser_view: Option<&mut BrowserView>,
            browser: Option<&mut Browser>,
        ) {
            let Some(browser) = browser else {
                return;
            };
            self.state
                .lock()
                .expect("state mutex poisoned")
                .note_chrome_browser(self.window_id, browser.identifier());
        }

        fn browser_runtime_style(&self) -> RuntimeStyle {
            VIEW_STYLE
        }
    }
}
