//! What bru does when a page asks for a window.
//!
//! Clicking a `target="_blank"` link on `home.abv.bg` opened an **operating-system window**, not a
//! tab. `LifeSpanHandler::on_before_popup` (bindings 20755) was never implemented; its default
//! returns 0, and 0 is "go ahead", so CEF made a top-level browser bru did not know existed —
//! against DESIGN.md's settled shape, "One window, many tabs, switched in place". The answer is 1,
//! which cancels the popup, and then bru opens the URL itself.
//!
//! **`target_disposition` is the whole decision.** Chromium has already worked out what the user
//! meant before this callback runs. Four of the cases are named by the enum's own documentation
//! (`cef_window_open_disposition_t`, sys bindings 2043): `NEW_BACKGROUND_TAB` is "Middle mouse
//! button or meta/ctrl key while clicking", `NEW_FOREGROUND_TAB` is "Shift key + Middle mouse
//! button or meta/ctrl key while clicking", `NEW_WINDOW` is "Shift key while clicking" and
//! `SAVE_TO_DISK` is "Alt key while clicking" — none of them reproducible on this machine, which has
//! no key or pointer injector CEF survives (CEF-NOTES, "Injecting keys on this machine"), so those
//! four arms rest on that documentation and on the unit tests below rather than on a run. So bru does
//! not re-derive any of it from modifiers or from `PopupFeatures` — reading `popup_features.is_popup`
//! to tell a `window.open()` from a `_blank` link would be answering a question the disposition has
//! already answered. Measured 2026-08-06 through the real click path (`--hint-script='hint,a'`):
//! `target="_blank"` arrived as `NEW_FOREGROUND_TAB` with `is_popup=0`, `window.open('x.html')` as
//! `NEW_FOREGROUND_TAB` with `is_popup=0`, and `window.open('x.html', '_blank', 'width=400,
//! height=300')` as `NEW_POPUP` with `is_popup=1` — the two fields agree and the disposition is the
//! one that is already the right shape to switch on.
//!
//! qutebrowser reads the same cases out of Qt's much coarser `QWebEnginePage::WebWindowType`
//! (`browser/webengine/webview.py:72`) and inverts two of them against `tabs.background`, because
//! Qt's wintypes do not carry the config — its own source carries a `FIXME:qtwebengine this also
//! affects target=_blank links...` on that arm. Chromium's dispositions do carry it: with
//! qutebrowser's defaults (`tabs.background: true`, `configdata.yml:2217`) a middle-click lands in
//! the background and a `target="_blank"` link lands in the foreground, which is exactly what the
//! direct mapping in [`decide`] gives. When bru grows a `tabs.background` setting it swaps the two
//! tab arms of [`decide`] and touches nothing else.
//!
//! **`user_gesture` is logged and not acted on.** qutebrowser refuses an automatic popup through
//! `content.javascript.can_open_tabs_automatically`, whose default is false. Measured 2026-08-06
//! with a fixture whose only `window.open` runs from a `setTimeout`, no click anywhere: **this
//! callback was never reached** — not one `bru[popups]` line, and the tab count stayed at 1.
//! Chromium refuses an unactivated popup upstream of `on_before_popup`, so a gesture check here
//! would be a second copy of a rule bru is already getting for free. Every popup that reached this
//! handler in every run carried `gesture=1`. The check is one `if` at the top of [`decide`] if a
//! case ever turns up that Chromium lets through and qutebrowser would not.
//!
//! **Posted, not inline, and the reason is a number rather than a deadlock.** `on_before_popup` runs
//! on the UI thread, so `tabs::new_tab` *can* be called from it directly, and — measured 2026-08-06,
//! 3/3 runs against `case-blank.html` — it works: no deadlock, the tab opens, bru exits cleanly.
//! That is worth writing down, because the shape looks exactly like CEF-NOTES trap 12 and is not it:
//! a popup is a renderer→browser IPC, so this callback is never on the stack of a message-router
//! query handler, and nothing here holds `browser_query_info_map`. What separates the two is cost.
//! Chromium blocks the renderer that called `window.open()` until this returns, and the inline form
//! holds it for **5.116 ms and 5.247 ms** — `add_child_view_at` creating and navigating a browser
//! synchronously (CEF-NOTES, Tabs) — where posting holds it for **0.005 ms and 0.006 ms**, a
//! thousandfold less, and lands the identical tab (`tabs=2 active=1` in both). Three orders of
//! magnitude off the page's critical path, for one turn of the message loop, is the trade; and doing
//! it the way `hints.rs`, `cmdline.rs` and `tabs::schedule_select` already do it means the trap-12
//! shape cannot come back through this door when something upstream of it changes.
//!
//! **One window is an assumption this file does not make.** The tab is opened through
//! [`install_opener`]'s hook, which is handed the *opener browser's* identifier. `app.rs` installs
//! one at startup that turns that identifier into a window with `BruState::window_of_browser` and
//! calls `tabs::new_tab_in`, so a link clicked in a background window opens its tab **in that
//! window** rather than in whichever one is in front. The fallback below is only reachable before
//! that startup wiring runs.

use cef::*;
use std::sync::Mutex;

use crate::tabs::SharedState;

/// Where a refused popup's URL goes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Where {
    /// A new tab, switched to.
    Foreground,
    /// A new tab, left behind the one showing — qutebrowser's `:open -b`.
    Background,
    /// The tab that asked, navigated in place.
    CurrentTab,
    /// To disk. `SAVE_TO_DISK` is "Save link as", and a tab showing a file is not that.
    Download,
    /// Nothing at all, for the reason given. Said in the bar as well as on stderr, because a click
    /// that produces no window and no tab is otherwise indistinguishable from bru ignoring it.
    Nothing(&'static str),
}

/// What each `WindowOpenDisposition` (bindings 47012) means for bru.
///
/// Every arm is deliberate; the enum has thirteen values and `NUM_VALUES` is a sentinel.
pub fn decide(target_url: &str, disposition: WindowOpenDisposition) -> Where {
    // --- tabs and statusbar ---------------------------------------------------------------------
    // `tabs.background`, the setting the head of this file said would swap two arms and touch
    // nothing else. It is read here rather than passed in because `decide` is called from one place
    // and tested from six, and `decide_with` is what the tests drive.
    decide_with(target_url, disposition, crate::settings::is_on("tabs.background"))
}

/// [`decide`] with the setting spelled out, so the tests can put it either way without a store.
pub fn decide_with(
    target_url: &str,
    disposition: WindowOpenDisposition,
    background: bool,
) -> Where {
    // --- end tabs and statusbar -----------------------------------------------------------------
    // `window.open()` with no URL wants a blank document the opener then scripts through the handle
    // it gets back. bru cancels the popup, so there is no handle to give and nothing would ever be
    // written into that tab. An empty tab the opener cannot reach is worse than no tab: the page has
    // already failed either way, and only one of the two leaves something to close.
    //
    // **It does not arrive empty.** Measured 2026-08-06: a bare `window.open()` reaches this
    // callback with `target_url` already resolved to the string `"about:blank"`, not to `""`. So the
    // `about:blank` half of this test is the half that fires, and the empty-string half is the guard
    // for a CEF that stops filling it in. `window.open("about:blank")` spelled out is the same
    // request and gets the same answer.
    let url = target_url.trim();
    if url.is_empty() || url.eq_ignore_ascii_case("about:blank") {
        return Where::Nothing("window.open() with no URL — bru has no blank tab to hand back");
    }

    match disposition {
        // --- tabs and statusbar -----------------------------------------------------------------
        // The two that matter, and the two the reported bug is about — and the two `tabs.background`
        // swaps, which is exactly what qutebrowser's `webview.py:113-121` does with the same
        // setting: `WebBrowserTab` becomes a background tab and `WebBrowserBackgroundTab` a
        // foreground one when it is false. With the default (true) this is the direct mapping the
        // head of this file argued for, so nothing about the measured behaviour changes.
        WindowOpenDisposition::NEW_FOREGROUND_TAB if !background => Where::Background,
        WindowOpenDisposition::NEW_BACKGROUND_TAB if !background => Where::Foreground,
        WindowOpenDisposition::NEW_FOREGROUND_TAB => Where::Foreground,
        WindowOpenDisposition::NEW_BACKGROUND_TAB => Where::Background,
        // --- end tabs and statusbar -------------------------------------------------------------

        // A real window request. `NEW_POPUP` is `window.open(url, name, "width=400,height=300")`;
        // `NEW_WINDOW` is a shift-click or `window.open` asking for a full browser window.
        // qutebrowser gives `WebBrowserWindow` its own `MainWindow` and answers `WebDialog` with
        // "requested, but we don't support that!" and a tab (`webview.py:103-109`). bru has one
        // window by DESIGN.md, so both are a foreground tab — the same collapse `hints.rs`'s
        // `Target::Window` and the dispatcher's `:open -w` already make. This is the one arm the
        // multi-window workstream may want to revisit; the URL is the same either way, only the
        // window it lands in changes, and that is [`install_opener`]'s question.
        WindowOpenDisposition::NEW_POPUP | WindowOpenDisposition::NEW_WINDOW => Where::Foreground,

        // `_self` from a context bru is not the opener of. Chromium normally handles this without
        // asking, so it is not expected here; navigating in place is what the name says and costs
        // one arm.
        WindowOpenDisposition::CURRENT_TAB => Where::CurrentTab,

        // "Save link as". A download, not a page — `downloads.rs` already owns the whole path.
        WindowOpenDisposition::SAVE_TO_DISK => Where::Download,

        // "Open in incognito". bru has no private profile: every browser shares the one
        // `RequestContext` under `--user-data-dir`. Opening it as an ordinary tab would answer a
        // request for privacy by quietly not providing it, so it is refused instead.
        WindowOpenDisposition::OFF_THE_RECORD => {
            Where::Nothing("bru has no private window to open this in")
        }

        // A floating video window Chrome's own UI hosts. bru's window is a box layout of
        // BrowserViews and has nowhere to put one, and a PiP page opened as a tab is not what was
        // asked for.
        WindowOpenDisposition::NEW_PICTURE_IN_PICTURE => {
            Where::Nothing("bru cannot open a picture-in-picture window")
        }

        // Chromium's own "do nothing".
        WindowOpenDisposition::IGNORE_ACTION => Where::Nothing("the page asked for nothing to open"),

        // `SINGLETON_TAB` and `SWITCH_TO_TAB` mean "this URL, in the tab that already has it, or a
        // new one" — Chrome UI machinery for its settings pages. bru has no tab-reuse concept and
        // qutebrowser has no target for one either; a foreground tab is the half of the request bru
        // can honour. `UNKNOWN`, `NEW_SPLIT_VIEW` and the `NUM_VALUES` sentinel land here too: a
        // disposition bru does not recognise still carries a URL a user clicked, and losing it
        // silently is the one outcome that has no defence.
        _ => Where::Foreground,
    }
}

/// How a refused popup gets its tab, when bru has more than one window to choose between.
///
/// `opener` is the identifier of the browser whose page asked for the popup — the one fact that
/// decides which window the tab belongs in, and the one fact `tabs::new_tab` has no parameter for
/// today.
pub type OpenTab = fn(state: &SharedState, opener: i32, url: &str, background: bool);

fn opener_hook() -> &'static Mutex<Option<OpenTab>> {
    static HOOK: Mutex<Option<OpenTab>> = Mutex::new(None);
    &HOOK
}

/// Called once at startup by whoever owns windows, before any page can ask for one.
///
/// Installed in `app.rs` with a function that turns the opener's browser id into its window through
/// `BruState::window_of_browser`. It is here rather than reached into from `window.rs` so that
/// `on_before_popup` stays a decision and a `post_task`, for the reason at the head of this file.
pub fn install_opener(open: OpenTab) {
    *opener_hook().lock().expect("popup opener mutex poisoned") = Some(open);
}

fn open_tab(state: &SharedState, opener: i32, url: &str, background: bool) {
    let hook = *opener_hook().lock().expect("popup opener mutex poisoned");
    match hook {
        Some(open) => open(state, opener, url, background),
        // No hook installed — only reachable before `app.rs` runs its startup wiring.
        None => crate::tabs::new_tab(state, url, background),
    }
}

/// `LifeSpanHandler::on_before_popup` — always 1, which is "cancel the popup".
///
/// Everything that follows from the cancellation is posted; see the head of this file for what
/// happened when it was not.
pub fn on_before_popup(
    browser: Option<&mut Browser>,
    target_url: Option<&CefString>,
    target_disposition: WindowOpenDisposition,
    user_gesture: ::std::os::raw::c_int,
    popup_features: Option<&PopupFeatures>,
) -> ::std::os::raw::c_int {
    debug_assert_ne!(currently_on(ThreadId::UI), 0);

    let url = target_url.map(CefString::to_string).unwrap_or_default();
    let opener = browser.map(|browser| browser.identifier()).unwrap_or(-1);
    let decision = decide(&url, target_disposition);

    if std::env::var_os("BRU_DEBUG_POPUPS").is_some() {
        eprintln!(
            "bru[popups]: {} url={url:?} opener={opener} gesture={user_gesture} is_popup={} -> {decision:?}",
            disposition_name(target_disposition),
            popup_features.map(|features| features.is_popup).unwrap_or(-1),
        );
    }

    let mut task = OpenPopup::new(opener, url, decision);
    post_task(ThreadId::UI, Some(&mut task));

    1
}

/// Carry out a decision. The UI thread, one turn of the message loop after the callback.
fn act(opener: i32, url: &str, decision: Where) {
    debug_assert_ne!(currently_on(ThreadId::UI), 0);

    let Some(state) = crate::state::BruState::instance() else {
        return;
    };
    match decision {
        Where::Foreground => open_tab(&state, opener, url, false),
        Where::Background => open_tab(&state, opener, url, true),
        Where::CurrentTab => {
            let browser = state
                .lock()
                .expect("state mutex poisoned")
                .browser_with_id(opener);
            match browser.and_then(|browser| browser.main_frame()) {
                Some(frame) => frame.load_url(Some(&CefString::from(url))),
                // The opener went away between the click and this task. A tab is the closest thing
                // left to what was asked for.
                None => open_tab(&state, opener, url, false),
            }
        }
        // Posted a second time, by `downloads.rs`'s own scheduler. One extra turn of the loop, and
        // the alternative is a second copy of the "which browser downloads this" question here.
        Where::Download => crate::downloads::schedule_start(url.to_string()),
        Where::Nothing(reason) => crate::message::warning(reason),
    }
}

wrap_task! {
    struct OpenPopup {
        opener: i32,
        url: String,
        decision: Where,
    }

    impl Task {
        fn execute(&self) {
            act(self.opener, &self.url, self.decision);
        }
    }
}

/// For the debug line only. `WindowOpenDisposition` is `Debug`, but it prints the raw C enum
/// variant, and a measurement is worth more when the log says which case it took.
fn disposition_name(disposition: WindowOpenDisposition) -> &'static str {
    match disposition {
        WindowOpenDisposition::UNKNOWN => "UNKNOWN",
        WindowOpenDisposition::CURRENT_TAB => "CURRENT_TAB",
        WindowOpenDisposition::SINGLETON_TAB => "SINGLETON_TAB",
        WindowOpenDisposition::NEW_FOREGROUND_TAB => "NEW_FOREGROUND_TAB",
        WindowOpenDisposition::NEW_BACKGROUND_TAB => "NEW_BACKGROUND_TAB",
        WindowOpenDisposition::NEW_POPUP => "NEW_POPUP",
        WindowOpenDisposition::NEW_WINDOW => "NEW_WINDOW",
        WindowOpenDisposition::SAVE_TO_DISK => "SAVE_TO_DISK",
        WindowOpenDisposition::OFF_THE_RECORD => "OFF_THE_RECORD",
        WindowOpenDisposition::IGNORE_ACTION => "IGNORE_ACTION",
        WindowOpenDisposition::SWITCH_TO_TAB => "SWITCH_TO_TAB",
        WindowOpenDisposition::NEW_PICTURE_IN_PICTURE => "NEW_PICTURE_IN_PICTURE",
        WindowOpenDisposition::NEW_SPLIT_VIEW => "NEW_SPLIT_VIEW",
        _ => "unnamed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "https://example.com/";

    /// The bug that started this: `home.abv.bg` has `target="_blank"` links, Chromium calls that
    /// `NEW_FOREGROUND_TAB`, and it must be a tab bru switches to — never a window.
    #[test]
    fn a_blank_link_is_a_foreground_tab() {
        assert_eq!(
            decide(URL, WindowOpenDisposition::NEW_FOREGROUND_TAB),
            Where::Foreground
        );
    }

    /// A middle-click. qutebrowser's `tabs.background` default is true, so this stays behind.
    #[test]
    fn a_middle_click_is_a_background_tab() {
        assert_eq!(
            decide(URL, WindowOpenDisposition::NEW_BACKGROUND_TAB),
            Where::Background
        );
    }

    // --- tabs and statusbar ---------------------------------------------------------------------
    /// `tabs.background false` swaps exactly the two arms the head of this file said it would, and
    /// nothing else. The four other dispositions are asserted with it: a setting about where a *tab*
    /// goes must not move a download or a refusal.
    #[test]
    fn tabs_background_false_swaps_the_two_tab_arms_and_only_those() {
        assert_eq!(
            decide_with(URL, WindowOpenDisposition::NEW_FOREGROUND_TAB, false),
            Where::Background
        );
        assert_eq!(
            decide_with(URL, WindowOpenDisposition::NEW_BACKGROUND_TAB, false),
            Where::Foreground
        );
        // And true is the mapping that was measured through the real click path.
        assert_eq!(
            decide_with(URL, WindowOpenDisposition::NEW_FOREGROUND_TAB, true),
            Where::Foreground
        );
        assert_eq!(
            decide_with(URL, WindowOpenDisposition::NEW_BACKGROUND_TAB, true),
            Where::Background
        );
        for disposition in [
            WindowOpenDisposition::NEW_WINDOW,
            WindowOpenDisposition::NEW_POPUP,
            WindowOpenDisposition::SAVE_TO_DISK,
            WindowOpenDisposition::OFF_THE_RECORD,
        ] {
            assert_eq!(
                decide_with(URL, disposition, false),
                decide_with(URL, disposition, true),
                "{} is not a tab and must not move with tabs.background",
                disposition_name(disposition)
            );
        }
    }
    // --- end tabs and statusbar -----------------------------------------------------------------

    /// `window.open(url, "_blank", "width=400")` and a shift-click. One window (DESIGN.md).
    #[test]
    fn a_window_request_is_a_tab_not_a_window() {
        assert_eq!(
            decide(URL, WindowOpenDisposition::NEW_POPUP),
            Where::Foreground
        );
        assert_eq!(
            decide(URL, WindowOpenDisposition::NEW_WINDOW),
            Where::Foreground
        );
    }

    /// `window.open()` with nothing in it. The opener wanted a handle to script; cancelling the
    /// popup means there is none, so an empty tab would be a tab nothing can ever fill.
    #[test]
    fn window_open_with_no_url_opens_nothing() {
        assert!(matches!(
            decide("", WindowOpenDisposition::NEW_FOREGROUND_TAB),
            Where::Nothing(_)
        ));
        assert!(matches!(
            decide("   ", WindowOpenDisposition::NEW_POPUP),
            Where::Nothing(_)
        ));
        assert!(matches!(
            decide("about:blank", WindowOpenDisposition::NEW_FOREGROUND_TAB),
            Where::Nothing(_)
        ));
        assert!(matches!(
            decide("about:BLANK", WindowOpenDisposition::NEW_WINDOW),
            Where::Nothing(_)
        ));
    }

    /// The three bru refuses, and the one it sends to disk. Each is a different answer from
    /// "foreground tab", which is what makes the enum worth having.
    #[test]
    fn the_dispositions_that_are_not_tabs() {
        assert_eq!(
            decide(URL, WindowOpenDisposition::SAVE_TO_DISK),
            Where::Download
        );
        assert_eq!(
            decide(URL, WindowOpenDisposition::CURRENT_TAB),
            Where::CurrentTab
        );
        for disposition in [
            WindowOpenDisposition::OFF_THE_RECORD,
            WindowOpenDisposition::NEW_PICTURE_IN_PICTURE,
            WindowOpenDisposition::IGNORE_ACTION,
        ] {
            assert!(
                matches!(decide(URL, disposition), Where::Nothing(_)),
                "{} should open nothing",
                disposition_name(disposition)
            );
        }
    }

    /// A disposition bru has no arm for still carries a URL somebody clicked.
    #[test]
    fn an_unrecognised_disposition_still_opens_the_url() {
        for disposition in [
            WindowOpenDisposition::UNKNOWN,
            WindowOpenDisposition::SINGLETON_TAB,
            WindowOpenDisposition::SWITCH_TO_TAB,
            WindowOpenDisposition::NEW_SPLIT_VIEW,
        ] {
            assert_eq!(
                decide(URL, disposition),
                Where::Foreground,
                "{} lost its URL",
                disposition_name(disposition)
            );
        }
    }

    /// Every named disposition has a name in the debug line — the log is the measurement, and
    /// "unnamed" in it would make a run unreadable after the fact.
    #[test]
    fn every_disposition_is_named() {
        for disposition in [
            WindowOpenDisposition::UNKNOWN,
            WindowOpenDisposition::CURRENT_TAB,
            WindowOpenDisposition::SINGLETON_TAB,
            WindowOpenDisposition::NEW_FOREGROUND_TAB,
            WindowOpenDisposition::NEW_BACKGROUND_TAB,
            WindowOpenDisposition::NEW_POPUP,
            WindowOpenDisposition::NEW_WINDOW,
            WindowOpenDisposition::SAVE_TO_DISK,
            WindowOpenDisposition::OFF_THE_RECORD,
            WindowOpenDisposition::IGNORE_ACTION,
            WindowOpenDisposition::SWITCH_TO_TAB,
            WindowOpenDisposition::NEW_PICTURE_IN_PICTURE,
            WindowOpenDisposition::NEW_SPLIT_VIEW,
        ] {
            assert_ne!(disposition_name(disposition), "unnamed", "{disposition:?}");
        }
    }
}
