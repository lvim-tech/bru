//! What bru does when a page asks for a window.
//!
//! Clicking a `target="_blank"` link on `home.abv.bg` opened an **operating-system window**, not a
//! tab. `LifeSpanHandler::on_before_popup` (bindings 20755) was never implemented; its default
//! returns 0, and 0 is "go ahead", so CEF made a top-level browser bru did not know existed —
//! against DESIGN.md's settled shape, "One window, many tabs, switched in place". The answer is 1,
//! which cancels the popup, and then bru opens the URL itself.
//!
//! **Except for the one request that needs the window it asked for.** Reported 2026-09-12:
//! "Continue with Google" on claude.ai answered "There was an error logging you in", while the
//! email login worked. `chrome_debug.log` had the reason, three times in a minute:
//!
//! ```text
//! [GSI_LOGGER]: Failed to open popup window on url: https://accounts.google.com/o/oauth2/v2/auth?…
//!   origin=https://claude.ai…display=popup…response_mode=form_post… Maybe blocked by the browser?
//! ```
//!
//! Google Identity Services calls `window.open(url, name, "width=…,height=…")` and **works with
//! the handle it gets back**: it drives the popup, waits for a `postMessage` from it, reads the
//! result out of it. Cancelling that popup and opening its URL as a tab gives the tab everything
//! and the script nothing — `window.open()` returns `null`, GSI gives up before the tab has loaded,
//! and the sign-in the user then completes in that tab has nobody left to report to. The tab was
//! not a degraded answer; it was no answer. So a `NEW_POPUP` is now allowed as a real CEF window,
//! held by this module and by nothing else in bru — see [`Where::Popup`] and "The popup window"
//! below. Every other disposition is exactly the tab it was.
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
//! one that is already the right shape to switch on. That last line is also what separates the
//! popup that needs its window from the link that does not: a page that passes window features is
//! a page that means to hold the window, and Chromium says so in the disposition.
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
//!
//! # The popup window
//!
//! An allowed popup is a browser CEF creates, not bru, and everything about it follows from that.
//!
//! **It is not a tab, and it is not one of bru's windows.** `BruState.windows` is the list of tab
//! strips; a popup has no strip, no status line, no mode, no place in a session and no `U` to bring
//! it back. It is registered in `BruState.browsers` — through [`PopupLifeSpanHandler`] — so that the
//! quit can count it and `browser_with_id` can find it, and nowhere else. `BruState::do_close`
//! already says the right thing for a browser that is not a tab: 0, "close it the way you would",
//! and for a Views-hosted browser the way CEF would is to close the window that hosts it. That is
//! the sentence commit 7c68a33 had to *stop* applying to tabs; for a popup it is exactly what
//! `window.close()` after a sign-in means.
//!
//! **It has its own `Client`, without bru's keyboard.** `j` in a Google sign-in form has to type a
//! `j` — the argument `term_frontend::InspectorClient` already makes for the inspector. What
//! [`PopupClient`] carries is the life-span handler that registers the browser and lets it go, a
//! display handler for the window's title, and nothing else: no request handler, no download
//! handler, no prompts. The popup is plain Chromium that bru merely holds.
//!
//! **CEF builds it through the Views callbacks, not through `window_info`.** From the header,
//! `cef_life_span_handler_capi.h:88`: "Any modifications to |windowInfo| will be ignored if the
//! parent browser is wrapped in a cef_browser_view_t." So the size asked for in the features is
//! carried across by hand: recorded here in [`on_before_popup`], read back in
//! `BrowserViewDelegate::delegate_for_popup_browser_view` and `on_popup_browser_view_created` —
//! both on the *opener's* delegate, in `window.rs`, which hand the popup view over to
//! [`view_delegate`] and [`place`]. `place` makes the top-level window; [`PopupWindowDelegate`]
//! puts the view in it and takes it out again.
//!
//! **A window of a fixed size, and that is the popup.** `can_resize`, `can_maximize` and
//! `can_minimize` answer 0 here, which `window.rs:1088` measured to make mango *float* the window
//! rather than tile it: "dwl-derived compositors float a window with a fixed size". For bru's own
//! window that was the bug; for a `width=500,height=600` sign-in dialog over the page that opened
//! it, it is the behaviour the page asked for and the one every other browser gives it.

use cef::*;
use std::cell::RefCell;
use std::sync::Mutex;

use crate::state::BruState;
use crate::tabs::SharedState;

/// Where a popup request goes.
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
    /// A real window, made by CEF, so that the `window.open()` that asked gets a handle back. The
    /// one arm that does not cancel the popup — see "The popup window" at the head of this file.
    Popup,
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
    // `window.open(url, name, "width=400,height=300")`. The page wants a window it can hold — to
    // script, to be told by, to close — and a handle is the only thing that makes the request mean
    // anything. Decided before the empty-URL check on purpose: `window.open("", name, features)`
    // followed by a `location =` is how some sign-in libraries dodge popup blockers, and a blank
    // window *with a handle* is exactly what they need. The refusal below is for the tabs, which
    // have no handle to give.
    if disposition == WindowOpenDisposition::NEW_POPUP {
        return Where::Popup;
    }

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

        // A shift-click, or `window.open` asking for a full browser window with no features to
        // hold it by. qutebrowser gives `WebBrowserWindow` its own `MainWindow`
        // (`webview.py:103-109`); bru has one window by DESIGN.md, so this is a foreground tab —
        // the same collapse `hints.rs`'s `Target::Window` and the dispatcher's `:open -w` already
        // make. It is deliberately *not* the arm above: nothing in a shift-click needs a handle,
        // and a page that wanted one would have passed features and arrived as `NEW_POPUP`.
        WindowOpenDisposition::NEW_WINDOW => Where::Foreground,

        // `NEW_POPUP` is answered at the top of this function, before the URL is looked at.
        WindowOpenDisposition::NEW_POPUP => Where::Popup,

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

/// Whether the popup decisions are narrated on stderr. `BRU_DEBUG_POPUPS=1`.
fn debugging() -> bool {
    std::env::var_os("BRU_DEBUG_POPUPS").is_some()
}

/// `LifeSpanHandler::on_before_popup` — 1, "cancel the popup", for everything but a [`Where::Popup`],
/// which is answered 0 with a client of its own.
///
/// Everything that follows from a cancellation is posted; see the head of this file for what
/// happened when it was not. The one allowed case posts nothing: CEF goes on to create the popup
/// browser itself, and the rest of this module hears about it through the Views callbacks.
pub fn on_before_popup(
    browser: Option<&mut Browser>,
    popup_id: ::std::os::raw::c_int,
    target_url: Option<&CefString>,
    target_disposition: WindowOpenDisposition,
    user_gesture: ::std::os::raw::c_int,
    popup_features: Option<&PopupFeatures>,
    client: Option<&mut Option<Client>>,
) -> ::std::os::raw::c_int {
    debug_assert_ne!(currently_on(ThreadId::UI), 0);

    let url = target_url.map(CefString::to_string).unwrap_or_default();
    let opener = browser.map(|browser| browser.identifier()).unwrap_or(-1);
    let mut decision = decide(&url, target_disposition);

    // --- src/term_frontend.rs -------------------------------------------------------------------
    // A terminal run has no Views and no desktop to put a window on: its browsers are windowless,
    // and a popup allowed here would be a native window CEF tries to open beside the terminal.
    // The tab is the same answer `window::create` gives `:open -w` there, for the same reason.
    if decision == Where::Popup && crate::term_frontend::is_active() {
        decision = Where::Foreground;
    }
    // --- end src/term_frontend.rs ---------------------------------------------------------------

    if debugging() {
        eprintln!(
            "bru[popups]: {} url={url:?} opener={opener} popup_id={popup_id} gesture={user_gesture} is_popup={} -> {decision:?}",
            disposition_name(target_disposition),
            popup_features.map(|features| features.is_popup).unwrap_or(-1),
        );
    }

    if decision == Where::Popup {
        // The client slot is CEF's to offer; without it the popup would inherit the opener's
        // client, keyboard handler and all, and be registered as a tab that is in no window. The
        // state is what the popup's handlers report to. Missing either, the tab is the honest
        // fallback — it is what every popup got until today.
        match (client, BruState::instance()) {
            (Some(slot), Some(state)) => {
                *slot = Some(PopupClient::new(state));
                remember_pending(opener, popup_id, requested_size(popup_features));
                return 0;
            }
            _ => decision = Where::Foreground,
        }
    }

    let mut task = OpenPopup::new(opener, url, decision);
    post_task(ThreadId::UI, Some(&mut task));

    1
}

/// `LifeSpanHandler::on_before_popup_aborted` — a popup that was allowed and never came to be.
///
/// Reachable now that [`on_before_popup`] can answer 0. The only state a pending popup holds is
/// its size in the queue below, and this is where the header says to clear it.
pub fn on_before_popup_aborted(browser: Option<&mut Browser>, popup_id: ::std::os::raw::c_int) {
    let opener = browser.map(|browser| browser.identifier()).unwrap_or(-1);
    forget_pending(opener, popup_id);
    if debugging() {
        eprintln!("bru[popups]: popup {popup_id} of opener {opener} aborted before it was created");
    }
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
        // Never posted: `on_before_popup` returns 0 for it and CEF makes the window. If one arrives
        // here anyway, the URL is still a URL somebody asked for.
        Where::Popup => open_tab(&state, opener, url, false),
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

// --- the popup window ----------------------------------------------------------------------------

/// A size a page asked for, in the DIPs `window.open`'s features are written in. Either axis may be
/// unset; [`popup_bounds`] fills it in.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Requested {
    pub width: Option<i32>,
    pub height: Option<i32>,
}

/// What the page put in its features, axis by axis. `window.open(url, name, "popup")` sets neither
/// and still arrives as `NEW_POPUP`.
fn requested_size(features: Option<&PopupFeatures>) -> Requested {
    let Some(features) = features else {
        return Requested::default();
    };
    Requested {
        width: (features.width_set != 0).then_some(features.width),
        height: (features.height_set != 0).then_some(features.height),
    }
}

/// What a popup is when the page named no size. Chromium uses the opener's window; bru's opener is
/// a whole tiled screen, and a sign-in dialog the size of the screen is not a dialog.
const DEFAULT_WIDTH: i32 = 1024;
const DEFAULT_HEIGHT: i32 = 768;
/// Chromium's own floor for a popup, `WebContentsImpl`'s minimum window size. Below it a window
/// cannot show its own close button.
const MIN_SIDE: i32 = 100;

/// The window a request turns into, inside the work area it has to fit.
///
/// Clamped both ways: a `width=10` is a window nothing can be read in, and a `height=5000` on a
/// 1080-row screen is a window whose bottom — where a sign-in form keeps its button — is off it.
/// Centred in the work area, which on Wayland is a suggestion the compositor is free to ignore and
/// on X11 is where it opens; either way the number is not zero, which would pin it to the corner
/// on the one platform that listens.
pub fn popup_bounds(requested: Requested, work_area: &Rect) -> Rect {
    let fit = |asked: Option<i32>, fallback: i32, room: i32| -> i32 {
        let wanted = asked.unwrap_or(fallback).max(MIN_SIDE);
        if room > 0 { wanted.min(room) } else { wanted }
    };
    let width = fit(requested.width, DEFAULT_WIDTH, work_area.width);
    let height = fit(requested.height, DEFAULT_HEIGHT, work_area.height);
    Rect {
        x: work_area.x + (work_area.width - width).max(0) / 2,
        y: work_area.y + (work_area.height - height).max(0) / 2,
        width,
        height,
    }
}

/// The work area of the display bru is on, or an empty rectangle when CEF cannot say — which
/// [`popup_bounds`] reads as "no limit".
fn work_area() -> Rect {
    display_get_primary()
        .map(|display| display.work_area())
        .unwrap_or(Rect { x: 0, y: 0, width: 0, height: 0 })
}

/// A popup that was allowed and whose browser CEF has not made yet.
///
/// The size lives here between `on_before_popup`, which is the only callback handed the features,
/// and the two Views callbacks that need it, which are handed the views and nothing about why they
/// exist. Keyed by the opener because that is what a popup view can answer —
/// `host.opener_identifier()` — and by `popup_id` so that an abort takes out the right one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Pending {
    opener: i32,
    popup_id: i32,
    size: Requested,
}

/// More than this many popups in flight from one page at once is a page misbehaving, and the
/// oldest entry is the one most likely to belong to a popup that will never be created.
const PENDING_LIMIT: usize = 16;

fn pending() -> &'static Mutex<Vec<Pending>> {
    static PENDING: Mutex<Vec<Pending>> = Mutex::new(Vec::new());
    &PENDING
}

fn remember_pending(opener: i32, popup_id: i32, size: Requested) {
    let mut queue = pending().lock().expect("pending popups mutex poisoned");
    push_pending(&mut queue, Pending { opener, popup_id, size });
}

fn forget_pending(opener: i32, popup_id: i32) {
    let mut queue = pending().lock().expect("pending popups mutex poisoned");
    drop_pending(&mut queue, opener, popup_id);
}

/// The queue's three rules, without the mutex so a test can drive them.
fn drop_pending(queue: &mut Vec<Pending>, opener: i32, popup_id: i32) {
    queue.retain(|entry| !(entry.opener == opener && entry.popup_id == popup_id));
}

fn push_pending(queue: &mut Vec<Pending>, entry: Pending) {
    queue.push(entry);
    if queue.len() > PENDING_LIMIT {
        queue.remove(0);
    }
}

/// The oldest request from this opener — popups are created in the order they were asked for.
fn take_pending(queue: &mut Vec<Pending>, opener: i32) -> Option<Pending> {
    let index = queue.iter().position(|entry| entry.opener == opener)?;
    Some(queue.remove(index))
}

/// The size the oldest pending popup from `opener` asked for, left in the queue for [`place`].
fn peek_pending(opener: i32) -> Requested {
    let queue = pending().lock().expect("pending popups mutex poisoned");
    queue
        .iter()
        .find(|entry| entry.opener == opener)
        .map(|entry| entry.size)
        .unwrap_or_default()
}

/// A popup window bru is holding, by the browser inside it.
struct PopupWindow {
    browser: i32,
    window: Window,
}

/// Every popup window open right now. The same shape as `prompt.rs`'s `prompts()`: a list behind a
/// mutex, touched a few times per popup and never on a key path.
fn popups() -> &'static Mutex<Vec<PopupWindow>> {
    static POPUPS: Mutex<Vec<PopupWindow>> = Mutex::new(Vec::new());
    &POPUPS
}

fn remember_window(browser: i32, window: Window) {
    let mut list = popups().lock().expect("popup windows mutex poisoned");
    list.retain(|entry| entry.browser != browser);
    list.push(PopupWindow { browser, window });
}

fn forget_window(browser: i32) {
    let mut list = popups().lock().expect("popup windows mutex poisoned");
    list.retain(|entry| entry.browser != browser);
}

fn window_of(browser: i32) -> Option<Window> {
    let list = popups().lock().expect("popup windows mutex poisoned");
    list.iter()
        .find(|entry| entry.browser == browser)
        .map(|entry| entry.window.clone())
}

/// How many popup windows are open. For the debug line and the tests of the registry.
pub fn count() -> usize {
    popups().lock().expect("popup windows mutex poisoned").len()
}

/// Close every popup window. `:quit` closes every window in `BruState.windows` and the popups are
/// in none of them; without this a `quit` typed while a sign-in dialog is open would leave the
/// process alive with the dialog as its last browser.
///
/// The handles are cloned out and the lock let go before any of them is closed: `window.close()`
/// reaches `PopupWindowDelegate::can_close` and, on the way out, `on_window_destroyed`, which takes
/// this same lock to remove the entry.
pub fn close_all() {
    let windows: Vec<Window> = {
        let list = popups().lock().expect("popup windows mutex poisoned");
        list.iter().map(|entry| entry.window.clone()).collect()
    };
    for window in windows {
        window.close();
    }
}

/// `close_all`, one turn of the loop later — for a caller inside a CEF callback of another window.
pub fn schedule_close_all() {
    let mut task = CloseAllPopups::new();
    post_task(ThreadId::UI, Some(&mut task));
}

wrap_task! {
    struct CloseAllPopups;

    impl Task {
        fn execute(&self) {
            close_all();
        }
    }
}

/// `BrowserViewDelegate::delegate_for_popup_browser_view`, for a popup that is not an inspector.
///
/// Asked of the *opener's* view delegate before the popup view exists; `opener` is that view's
/// browser. The delegate carries the size, because a `BoxLayout` is not what a popup window has
/// — it has a fill layout and the window's own bounds — but a view with no `preferred_size` is
/// what CEF-NOTES trap 25 is about, and answering the requested size keeps it non-empty.
pub fn view_delegate(state: &SharedState, opener: i32) -> BrowserViewDelegate {
    let size = peek_pending(opener);
    let bounds = popup_bounds(size, &work_area());
    PopupViewDelegate::new(state.clone(), bounds.width, bounds.height)
}

/// `BrowserViewDelegate::on_popup_browser_view_created`, for a popup that is not an inspector:
/// make the window, put the view in it, and answer "handled".
///
/// `false` is "make a default window", which CEF would do with a delegate of its own — no title,
/// no app id, and no way for bru to close it at `:quit`. It is answered only when the view has no
/// browser to name, which the header says cannot happen here: "This function will be called after
/// on_after_created() and on_browser_created() are called for the new popup browser."
pub fn place(view: &BrowserView) -> bool {
    debug_assert_ne!(currently_on(ThreadId::UI), 0);

    let Some(browser) = view.browser() else {
        return false;
    };
    let browser_id = browser.identifier();
    let opener = browser.host().map(|host| host.opener_identifier()).unwrap_or(-1);
    let size = {
        let mut queue = pending().lock().expect("pending popups mutex poisoned");
        take_pending(&mut queue, opener).map(|entry| entry.size).unwrap_or_default()
    };
    let bounds = popup_bounds(size, &work_area());
    if debugging() {
        eprintln!(
            "bru[popups]: popup browser {browser_id} of opener {opener} gets a window {}x{} at {},{}",
            bounds.width, bounds.height, bounds.x, bounds.y
        );
    }

    let mut delegate = PopupWindowDelegate::new(
        RefCell::new(Some(view.clone())),
        browser_id,
        bounds.x,
        bounds.y,
        bounds.width,
        bounds.height,
    );
    // `on_window_created` runs inside this call, the same as for bru's own window in `window.rs`.
    window_create_top_level(Some(&mut delegate)).is_some()
}

/// `DisplayHandler::on_title_change` for a popup: the page's title on the window that holds it,
/// suffixed the way `window::set_window_title` suffixes bru's own.
fn set_title(browser: i32, title: &str) {
    let Some(window) = window_of(browser) else {
        return;
    };
    let title = title.trim();
    let title = if title.is_empty() {
        crate::window::APP_NAME.to_string()
    } else {
        format!("{title} - {}", crate::window::APP_NAME)
    };
    window.set_title(Some(&CefString::from(title.as_str())));
}

wrap_browser_view_delegate! {
    pub struct PopupViewDelegate {
        state: SharedState,
        width: i32,
        height: i32,
    }

    impl ViewDelegate {
        fn preferred_size(&self, _view: Option<&mut View>) -> Size {
            Size { width: self.width, height: self.height }
        }
    }

    impl BrowserViewDelegate {
        // Alloy, like every view bru makes, and for the reason `window.rs` gives above `VIEW_STYLE`:
        // the content layer with none of Chrome's own UI. A popup with a Chrome-style location bar
        // would be the one window in bru drawn by Brave's chrome.
        fn browser_runtime_style(&self) -> RuntimeStyle {
            crate::window::VIEW_STYLE
        }

        // A popup asking for a popup — the same two answers as a tab asking for one, so a sign-in
        // that opens a second dialog gets a second window and a `_blank` link in it gets a tab.
        fn delegate_for_popup_browser_view(
            &self,
            browser_view: Option<&mut BrowserView>,
            _settings: Option<&BrowserSettings>,
            _client: Option<&mut Client>,
            is_devtools: ::std::os::raw::c_int,
        ) -> Option<BrowserViewDelegate> {
            if is_devtools != 0 {
                return None;
            }
            let opener = browser_view
                .and_then(|view| view.browser())
                .map(|browser| browser.identifier())
                .unwrap_or(-1);
            Some(view_delegate(&self.state, opener))
        }

        fn on_popup_browser_view_created(
            &self,
            _browser_view: Option<&mut BrowserView>,
            popup_browser_view: Option<&mut BrowserView>,
            is_devtools: ::std::os::raw::c_int,
        ) -> ::std::os::raw::c_int {
            let Some(view) = popup_browser_view else {
                return 0;
            };
            if is_devtools != 0 {
                return 0;
            }
            i32::from(place(view))
        }

        fn on_browser_destroyed(
            &self,
            _browser_view: Option<&mut BrowserView>,
            browser: Option<&mut Browser>,
        ) {
            if debugging() {
                let id = browser.map(|browser| browser.identifier()).unwrap_or(-1);
                eprintln!("bru[popups]: popup browser {id} destroyed");
            }
        }
    }
}

wrap_window_delegate! {
    pub struct PopupWindowDelegate {
        // Dropped in `on_window_destroyed`, the way `BruWindowDelegate` drops its strips: a view
        // held past its window keeps the browser in it alive.
        view: RefCell<Option<BrowserView>>,
        browser_id: i32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    }

    impl ViewDelegate {}

    impl PanelDelegate {}

    impl WindowDelegate {
        fn on_window_created(&self, window: Option<&mut Window>) {
            let view = self.view.borrow();
            let (Some(window), Some(view)) = (window, view.as_ref()) else {
                return;
            };
            // One child that is the whole window. Said rather than assumed.
            window.set_to_fill_layout();
            window.add_child_view(Some(&mut View::from(view)));
            // A name before the page has one, for the reason `window.rs` gives: a compositor that
            // reads the title once keeps the empty one.
            window.set_title(Some(&CefString::from(crate::window::APP_NAME)));
            remember_window(self.browser_id, window.clone());
            window.show();
            // The page the user is about to type into, not the window frame.
            View::from(view).request_focus();
        }

        fn initial_bounds(&self, _window: Option<&mut Window>) -> Rect {
            Rect { x: self.x, y: self.y, width: self.width, height: self.height }
        }

        // 0, all three, and this is what makes it a popup on this desktop: a toplevel whose minimum
        // and maximum size are equal floats over the tiled layout on mango (`window.rs:1088`,
        // measured). A window the page sized is a window the page meant to be that size.
        fn can_resize(&self, _window: Option<&mut Window>) -> i32 {
            0
        }

        fn can_maximize(&self, _window: Option<&mut Window>) -> i32 {
            0
        }

        fn can_minimize(&self, _window: Option<&mut Window>) -> i32 {
            0
        }

        // The same app id as bru's own window, so a rule written for `bru` covers its dialogs; a
        // role of its own, so a rule that wants to tell them apart can.
        fn linux_window_properties(
            &self,
            _window: Option<&mut Window>,
            properties: Option<&mut LinuxWindowProperties>,
        ) -> i32 {
            let Some(properties) = properties else {
                return 0;
            };
            properties.wayland_app_id = crate::window::handover("bru");
            properties.wm_class_class = crate::window::handover("bru");
            properties.wm_class_name = crate::window::handover("bru");
            properties.wm_role_name = crate::window::handover("popup");
            1
        }

        // The window manager's close button, and `close_all`. One browser to ask; it may have a
        // `beforeunload`. A window whose browser has already gone — the page closed itself, and
        // CEF is closing the window on the way out — has nothing left to ask and may go.
        fn can_close(&self, _window: Option<&mut Window>) -> i32 {
            let view = self.view.borrow();
            match view.as_ref().and_then(|view| view.browser()).and_then(|browser| browser.host()) {
                Some(host) => host.try_close_browser(),
                None => 1,
            }
        }

        fn on_window_destroyed(&self, _window: Option<&mut Window>) {
            forget_window(self.browser_id);
            *self.view.borrow_mut() = None;
            if debugging() {
                eprintln!(
                    "bru[popups]: popup window of browser {} destroyed, {} left",
                    self.browser_id,
                    count()
                );
            }
        }
    }
}

wrap_client! {
    pub struct PopupClient {
        state: SharedState,
    }

    impl Client {
        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(PopupLifeSpanHandler::new(self.state.clone()))
        }

        fn display_handler(&self) -> Option<DisplayHandler> {
            Some(PopupDisplayHandler::new())
        }
    }
}

wrap_life_span_handler! {
    pub struct PopupLifeSpanHandler {
        state: SharedState,
    }

    impl LifeSpanHandler {
        // The same decision as for a tab. A `_blank` link in the popup is a tab — in the current
        // window, since `window_of_browser` has no window for a popup and `app.rs`'s hook falls
        // back to that — and a popup from the popup is a window of its own.
        fn on_before_popup(
            &self,
            browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            popup_id: ::std::os::raw::c_int,
            target_url: Option<&CefString>,
            _target_frame_name: Option<&CefString>,
            target_disposition: WindowOpenDisposition,
            user_gesture: ::std::os::raw::c_int,
            popup_features: Option<&PopupFeatures>,
            _window_info: Option<&mut WindowInfo>,
            client: Option<&mut Option<Client>>,
            _settings: Option<&mut BrowserSettings>,
            _extra_info: Option<&mut Option<DictionaryValue>>,
            _no_javascript_access: Option<&mut ::std::os::raw::c_int>,
        ) -> ::std::os::raw::c_int {
            on_before_popup(
                browser,
                popup_id,
                target_url,
                target_disposition,
                user_gesture,
                popup_features,
                client,
            )
        }

        fn on_before_popup_aborted(
            &self,
            browser: Option<&mut Browser>,
            popup_id: ::std::os::raw::c_int,
        ) {
            on_before_popup_aborted(browser, popup_id);
        }

        // Into `BruState.browsers`, for the reason `term_frontend::InspectorLifeSpanHandler` gives:
        // a browser that is not registered is one the quit cannot count and `browser_with_id`
        // cannot find.
        fn on_after_created(&self, browser: Option<&mut Browser>) {
            if debugging() {
                if let Some(browser) = browser.as_deref() {
                    let opener = browser.host().map(|host| host.opener_identifier()).unwrap_or(-1);
                    eprintln!(
                        "bru[popups]: popup browser {} created (is_popup={}, opener={opener})",
                        browser.identifier(),
                        browser.is_popup()
                    );
                }
            }
            self.state
                .lock()
                .expect("state mutex poisoned")
                .on_after_created(browser);
        }

        // 0, through `BruState::do_close`, which answers 0 for any browser that is not a tab — and
        // for a Views-hosted browser CEF's 0 closes the window that hosts it. That is the whole
        // close path for `window.close()` after a sign-in: the page asks, the window goes.
        fn do_close(&self, browser: Option<&mut Browser>) -> ::std::os::raw::c_int {
            self.state
                .lock()
                .expect("state mutex poisoned")
                .do_close(browser)
        }

        // A browser that enters the registry has to leave it, or the quit waits for a browser that
        // is already gone. The router's forward first, for the same reason and in the same order as
        // `keys.rs`.
        fn on_before_close(&self, browser: Option<&mut Browser>) {
            if debugging() {
                let id = browser.as_deref().map(|browser| browser.identifier()).unwrap_or(-1);
                eprintln!("bru[popups]: popup browser {id} closing");
            }
            crate::ipc::on_before_close(browser.as_deref().cloned().as_mut());
            self.state
                .lock()
                .expect("state mutex poisoned")
                .on_before_close(browser);
        }
    }
}

wrap_display_handler! {
    pub struct PopupDisplayHandler {}

    impl DisplayHandler {
        fn on_title_change(&self, browser: Option<&mut Browser>, title: Option<&CefString>) {
            let Some(browser) = browser else {
                return;
            };
            let title = title.map(CefString::to_string).unwrap_or_default();
            set_title(browser.identifier(), &title);
        }
    }
}

// --- end the popup window ------------------------------------------------------------------------

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
    /// goes must not move a download, a refusal or a popup window.
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

    /// **The sign-in that could not complete.** `window.open(url, name, "width=500,height=600")` is
    /// `NEW_POPUP`, and the page that made it needs the handle back — so it is the one request that
    /// gets a real window. A shift-click is `NEW_WINDOW`, needs no handle, and stays a tab: one
    /// window (DESIGN.md) for everything that can live in one.
    #[test]
    fn a_popup_with_features_is_a_window_and_a_shift_click_is_a_tab() {
        assert_eq!(decide(URL, WindowOpenDisposition::NEW_POPUP), Where::Popup);
        assert_eq!(
            decide(URL, WindowOpenDisposition::NEW_WINDOW),
            Where::Foreground
        );
    }

    /// The two dispositions a `window.open` without features arrives as — measured 2026-08-06:
    /// `window.open('x.html')` and `target="_blank"` are both `NEW_FOREGROUND_TAB` — are tabs
    /// exactly as before. This is the "one window, many tabs" half of the change, asserted so that
    /// the popup arm cannot widen without a test going red.
    #[test]
    fn a_plain_window_open_is_still_a_tab() {
        assert_eq!(
            decide(URL, WindowOpenDisposition::NEW_FOREGROUND_TAB),
            Where::Foreground
        );
        assert_ne!(decide(URL, WindowOpenDisposition::NEW_FOREGROUND_TAB), Where::Popup);
        assert_ne!(decide(URL, WindowOpenDisposition::NEW_BACKGROUND_TAB), Where::Popup);
        assert_ne!(decide(URL, WindowOpenDisposition::NEW_WINDOW), Where::Popup);
    }

    /// `window.open()` with nothing in it, as a tab: the opener wanted a handle to script;
    /// cancelling the popup means there is none, so an empty tab would be a tab nothing can ever
    /// fill. As a popup the handle exists, and a blank window that the page then navigates is the
    /// popup-blocker dodge some sign-in libraries make — so that one is allowed.
    #[test]
    fn window_open_with_no_url_opens_nothing_unless_it_is_a_popup() {
        assert!(matches!(
            decide("", WindowOpenDisposition::NEW_FOREGROUND_TAB),
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
        assert_eq!(decide("   ", WindowOpenDisposition::NEW_POPUP), Where::Popup);
        assert_eq!(decide("about:blank", WindowOpenDisposition::NEW_POPUP), Where::Popup);
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

    // --- the popup window -----------------------------------------------------------------------

    const SCREEN: Rect = Rect { x: 0, y: 0, width: 1920, height: 1080 };

    /// Google asks for roughly this. It gets it, centred.
    #[test]
    fn a_requested_size_is_honoured_and_centred() {
        let bounds = popup_bounds(
            Requested { width: Some(500), height: Some(600) },
            &SCREEN,
        );
        assert_eq!((bounds.width, bounds.height), (500, 600));
        assert_eq!((bounds.x, bounds.y), ((1920 - 500) / 2, (1080 - 600) / 2));
    }

    /// `window.open(url, name, "popup")` names no size. Not the whole screen — a dialog the size of
    /// the screen is not a dialog — and not nothing, which CEF-NOTES trap 25 says is discarded.
    #[test]
    fn an_unsized_popup_gets_the_default_and_a_half_sized_one_keeps_its_half() {
        let bounds = popup_bounds(Requested::default(), &SCREEN);
        assert_eq!((bounds.width, bounds.height), (DEFAULT_WIDTH, DEFAULT_HEIGHT));
        let bounds = popup_bounds(Requested { width: Some(640), height: None }, &SCREEN);
        assert_eq!((bounds.width, bounds.height), (640, DEFAULT_HEIGHT));
    }

    /// Too small to read and too tall for the screen are both windows the user cannot use. The
    /// second one matters most: a sign-in form keeps its button at the bottom.
    #[test]
    fn a_popup_is_clamped_to_the_screen_and_to_a_floor() {
        let bounds = popup_bounds(
            Requested { width: Some(10), height: Some(5000) },
            &SCREEN,
        );
        assert_eq!((bounds.width, bounds.height), (MIN_SIDE, 1080));
        assert_eq!(bounds.y, 0, "a window as tall as the screen starts at its top");
    }

    /// No display to ask — `display_get_primary` answered nothing — is an empty work area, which
    /// clamps nothing: the request is passed through rather than crushed to the floor.
    #[test]
    fn an_unknown_work_area_limits_nothing() {
        let none = Rect { x: 0, y: 0, width: 0, height: 0 };
        let bounds = popup_bounds(Requested { width: Some(3000), height: Some(2000) }, &none);
        assert_eq!((bounds.width, bounds.height), (3000, 2000));
        assert_eq!((bounds.x, bounds.y), (0, 0));
    }

    /// The queue between `on_before_popup` and the Views callbacks: oldest request from the same
    /// opener first, since that is the order CEF creates them in; another opener's entry is not in
    /// the way; an abort takes out exactly the popup it names.
    #[test]
    fn pending_popups_are_taken_oldest_first_per_opener() {
        let mut queue = Vec::new();
        let first = Requested { width: Some(1), height: None };
        let second = Requested { width: Some(2), height: None };
        let other = Requested { width: Some(9), height: None };
        push_pending(&mut queue, Pending { opener: 7, popup_id: 1, size: first });
        push_pending(&mut queue, Pending { opener: 8, popup_id: 1, size: other });
        push_pending(&mut queue, Pending { opener: 7, popup_id: 2, size: second });

        assert_eq!(take_pending(&mut queue, 7).map(|entry| entry.size), Some(first));
        assert_eq!(take_pending(&mut queue, 7).map(|entry| entry.size), Some(second));
        assert_eq!(take_pending(&mut queue, 7), None, "nothing left from that opener");
        assert_eq!(take_pending(&mut queue, 8).map(|entry| entry.size), Some(other));
        assert!(queue.is_empty());
    }

    /// A page that asks for popups faster than CEF creates them cannot grow the queue without
    /// bound; the oldest goes, since it is the one most likely never to be created.
    #[test]
    fn the_pending_queue_is_bounded() {
        let mut queue = Vec::new();
        for popup_id in 0..(PENDING_LIMIT as i32 + 5) {
            push_pending(&mut queue, Pending { opener: 1, popup_id, size: Requested::default() });
        }
        assert_eq!(queue.len(), PENDING_LIMIT);
        assert_eq!(queue[0].popup_id, 5, "the five oldest were dropped");
    }

    /// An abort removes the popup it names and only that one.
    #[test]
    fn an_abort_forgets_exactly_one_pending_popup() {
        let mut queue = vec![
            Pending { opener: 7, popup_id: 1, size: Requested::default() },
            Pending { opener: 7, popup_id: 2, size: Requested::default() },
            Pending { opener: 8, popup_id: 1, size: Requested::default() },
        ];
        drop_pending(&mut queue, 7, 1);
        assert_eq!(queue.len(), 2);
        assert_eq!((queue[0].opener, queue[0].popup_id), (7, 2));
        assert_eq!((queue[1].opener, queue[1].popup_id), (8, 1), "same id, other opener, kept");
    }
}
