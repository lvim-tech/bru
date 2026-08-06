//! The top-level window.
//!
//! Built through CEF's Views framework rather than a native X11/Wayland surface: Views gives a
//! window CEF owns and lays out itself, which is what a tab strip will eventually be added to.

use cef::*;
use std::cell::RefCell;
use std::sync::{Arc, Mutex};

use crate::state::BruState;
use crate::tabs;

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

wrap_window_delegate! {
    pub struct BruWindowDelegate {
        state: Arc<Mutex<BruState>>,
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
            {
                let mut state = self.state.lock().expect("state mutex poisoned");
                state.set_window(window.clone());
                state.set_layout(layout);
            }

            // Tabs that already exist: at startup, the one the command line asked for, created
            // before there was a window to put it in.
            tabs::attach_all(&self.state);

            let mut bottom = View::from(bottom);
            window.add_child_view(Some(&mut bottom));

            window.show();

            // Selecting shows the tab and takes focus. Without it the tab strip, as the first
            // child added, keeps focus and every key goes to a chrome page.
            let active = self
                .state
                .lock()
                .expect("state mutex poisoned")
                .active_tab();
            tabs::select(&self.state, active);
        }

        fn on_window_destroyed(&self, _window: Option<&mut Window>) {
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
                .tab_views();
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
        height: i32,
    }

    impl ViewDelegate {
        fn preferred_size(&self, _view: Option<&mut View>) -> Size {
            // Width is ignored: the box layout stretches the strip across the window. Only the
            // height is a real request.
            Size { width: 1280, height: self.height }
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
                .note_chrome_browser(browser.identifier());
        }

        fn browser_runtime_style(&self) -> RuntimeStyle {
            VIEW_STYLE
        }
    }
}
