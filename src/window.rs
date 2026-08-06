//! The top-level window.
//!
//! Built through CEF's Views framework rather than a native X11/Wayland surface: Views gives a
//! window CEF owns and lays out itself, which is what a tab strip will eventually be added to.

use cef::*;
use std::cell::RefCell;

wrap_window_delegate! {
    pub struct BruWindowDelegate {
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
            window.show();
        }

        fn on_window_destroyed(&self, _window: Option<&mut Window>) {
            // Drop the view here, or the window outlives the browser it holds.
            *self.browser_view.borrow_mut() = None;
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
