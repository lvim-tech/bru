//! The web inspector — qutebrowser's `:devtools` and `:devtools-focus`.
//!
//! `BrowserHost::show_dev_tools` (bindings 12656) opens DevTools "in its own browser", associated
//! with the browser it was asked of; `close_dev_tools` (12664) closes it and `has_dev_tools` (12666)
//! answers whether it is open, which is what makes qutebrowser's toggle possible without keeping any
//! state here.
//!
//! **Every position opens a window.** qutebrowser binds five of them — `wIh`, `wIj`, `wIk`, `wIl`
//! dock the inspector to a side and `wIw` gives it a window — and CEF has no docked inspector to
//! offer: from the C API's own documentation, "the |windowInfo| parameter will be ignored if this
//! browser is wrapped in a cef_browser_view_t", which every tab in bru is. Docking would mean a
//! second BrowserView in the window's box layout, which is a layout feature and not this. So the
//! four side positions open the same window `wIw` does, and are live rather than inert: the
//! inspector is what the binding is for, and where it sits is not what it is for.

use cef::*;

/// `devtools [position]` — open the inspector, or close it if it is already open.
///
/// qutebrowser's is a toggle (`browsertab.py:946`, "Show/hide (and if needed, create) the web
/// inspector for this tab"), so this is too.
pub fn toggle(browser: &mut Browser) {
    let Some(host) = browser.host() else {
        return;
    };
    if host.has_dev_tools() != 0 {
        host.close_dev_tools();
    } else {
        open(&host);
    }
}

/// `devtools-focus` — bring the inspector forward, never close it.
///
/// CEF has no "focus the DevTools window" call, and does not need one: "if the DevTools browser is
/// already open then it will be focused", which is exactly what this asks for. A first
/// `devtools-focus` on a tab whose inspector was never opened therefore opens it, which is the
/// friendlier of the two readings of a binding whose whole purpose is to get you there.
pub fn focus(browser: &mut Browser) {
    let Some(host) = browser.host() else {
        return;
    };
    open(&host);
}

fn open(host: &BrowserHost) {
    // All four arguments are optional to the C API — the generated shim lists them as "unverified
    // params" and guards each with a null check — and all four are the right thing to leave out.
    // `window_info` is ignored for a browser inside a BrowserView; a `client` would give the
    // inspector bru's own key handler, and `j` in a DevTools console must type a `j`. The settings
    // are the defaults and `inspect_element_at` belongs to a context menu bru does not have.
    let settings = BrowserSettings::default();
    host.show_dev_tools(None, None, Some(&settings), None);
}
