//! The top-level window.
//!
//! Built through CEF's Views framework rather than a native X11/Wayland surface: Views gives a
//! window CEF owns and lays out itself, which is what a tab strip will eventually be added to.

use cef::*;
use std::cell::RefCell;
use std::sync::{Arc, Mutex};

use crate::state::BruState;

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
        browser_view: RefCell<Option<BrowserView>>,
    }

    impl ViewDelegate {
        fn preferred_size(&self, _view: Option<&mut View>) -> Size {
            Size { width: 1280, height: 800 }
        }
    }

    impl PanelDelegate {}

    impl WindowDelegate {
        fn on_window_created(&self, window: Option<&mut Window>) {
            let browser_view = self.browser_view.borrow();
            let (Some(window), Some(browser_view)) = (window, browser_view.as_ref()) else {
                return;
            };
            let mut view = View::from(browser_view);
            window.add_child_view(Some(&mut view));

            // Nothing else ever gets handed the window; keep it where views can be added later.
            self.state
                .lock()
                .expect("state mutex poisoned")
                .set_window(window.clone());

            window.show();
        }

        fn on_window_destroyed(&self, _window: Option<&mut Window>) {
            // Drop the view here, or the window outlives the browser it holds.
            *self.browser_view.borrow_mut() = None;
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
            // Ask the browser first: it may need to run beforeunload handlers.
            let browser_view = self.browser_view.borrow();
            let Some(browser_view) = browser_view.as_ref() else {
                return 1;
            };
            match browser_view.browser() {
                Some(browser) => match browser.host() {
                    Some(host) => host.try_close_browser(),
                    None => 1,
                },
                None => 1,
            }
        }
    }
}

wrap_browser_view_delegate! {
    // The empty-struct shorthand the other wrap_ macros accept is not a rule this one has; it needs
    // the braces even with no fields.
    pub struct BruBrowserViewDelegate {}

    impl ViewDelegate {}

    impl BrowserViewDelegate {}
}
