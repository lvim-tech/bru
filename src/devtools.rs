//! The web inspector — qutebrowser's `:devtools` and `:devtools-focus`.
//!
//! `BrowserHost::show_dev_tools` (bindings 12656) opens DevTools "in its own browser", associated
//! with the browser it was asked of; `close_dev_tools` (12664) closes it and `has_dev_tools` (12666)
//! answers whether it is open, which is what makes qutebrowser's toggle possible without keeping any
//! state here.
//!
//! **Every position opens a window — today, and not because CEF refuses.** `show_dev_tools`'s
//! `windowInfo` is ignored for a browser inside a `cef_browser_view_t`, which every tab here is, so
//! nothing this file passes can place the inspector. That was once read as "CEF has no docked
//! inspector to offer", and **that reading is wrong**: CEF hands the inspector over as a
//! BrowserView and asks where to put it.
//!
//! `BrowserViewDelegate::on_popup_browser_view_created(browser_view, popup_browser_view,
//! is_devtools)` — "Optionally add |popup_browser_view| to the views hierarchy yourself and return
//! true (1). Otherwise return false (0) and a default cef_window_t will be created for the popup."
//! Measured 2026-08-08 with a probe in `window.rs`'s delegate: `:devtools` on an ordinary tab fires
//! it with **`is_devtools=1` and a live view**, under `RuntimeStyle::ALLOY`. Returning 0 is what
//! makes the window; docking is returning 1 and putting the view in the layout.
//!
//! So the four side positions are accepted and open the same window `wIw` used to, and bru's
//! defaults no longer bind them — a key that names a placement nothing performs is a key that lies.
//! What docking actually needs is written up in CEF-NOTES trap 24, including the part CEF does not
//! give: there is no splitter and no resize area, and a `ViewDelegate` is never handed a pointer
//! event, so a draggable divider has to be a `bru://` page like the rest of bru's chrome.

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
