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
use std::sync::{Mutex, OnceLock};

/// Trace the popup path. `BRU_DEBUG_DEVTOOLS=1` turns it on.
///
/// The failure this exists for is a **silent** one — a build where nothing docked and a close that
/// ended the process with no panic and nothing on stderr — and neither can be reasoned about from
/// the outside. Every step of CEF's popup handshake prints here, in order, so the sequence itself
/// is the evidence.
pub fn trace(what: &str) {
    if std::env::var_os("BRU_DEBUG_DEVTOOLS").is_some() {
        eprintln!("bru[devtools]: {what}");
    }
}

/// `devtools [position]` — open the inspector, or close it if it is already open.
///
/// qutebrowser's is a toggle (`browsertab.py:946`, "Show/hide (and if needed, create) the web
/// inspector for this tab"), so this is too.
pub fn toggle(browser: &mut Browser, place: Place) {
    let Some(host) = browser.host() else {
        return;
    };
    // **A docked inspector is hidden, never destroyed, and that is the whole of M2's answer.**
    //
    // `close_dev_tools` on a docked inspector ends the process. Measured 2026-08-08 across four
    // arrangements: removing the view from `LifeSpanHandler::on_before_close` (silent death);
    // removing nothing at all (dies anyway, so it is not bru's removal); removing it just before
    // `close_dev_tools` (dies); and removing it from the view delegate's `on_browser_destroyed`,
    // which is the hook cefclient uses — **exit 139, SIGSEGV** — and again with that removal posted
    // to the next turn of the UI loop, **exit 133, SIGTRAP**, a Chromium CHECK. Taking a live
    // BrowserView out of a window while the browser behind it is being torn down is not something
    // this CEF supports, and there is no upstream example of a same-window docked inspector to
    // copy: cefclient gives every DevTools popup a window of its own.
    //
    // So the toggle stops destroying anything. The inspector is hidden and shown, which is exactly
    // what bru already does to tabs, and a `BoxLayout` skips an invisible child so the page gets
    // the room back. The browser stays alive behind it — the same trade qutebrowser makes, whose
    // inspector is a widget it hides — and it goes when its tab or its window goes, on CEF's own
    // teardown path rather than on one bru drives.
    if let Some(state) = crate::state::BruState::instance() {
        let window = state
            .lock()
            .expect("state mutex poisoned")
            .window_of_browser(browser.identifier());
        if let Some(window) = window {
            if let Some(shown) = flip(&state, window, browser.identifier()) {
                trace(&format!("toggle: docked inspector now visible={shown}"));
                return;
            }
        }
    }
    if host.has_dev_tools() != 0 {
        // A windowed inspector, which CEF owns entirely: closing one has never been a problem.
        host.close_dev_tools();
        return;
    }
    // **Said before `show_dev_tools`, because the answer is wanted inside it.** CEF creates the
    // DevTools browser synchronously enough that `on_popup_browser_view_created` is reached from
    // this call, and that callback is handed the popup view and nothing else about why it exists —
    // not which window asked, not whether the user typed `window`. Both are known here and nowhere
    // else, so they are left where the callback will look.
    *pending().lock().expect("devtools mutex poisoned") =
        Some(Pending { place, inspects: browser.identifier() });
    trace(&format!("toggle: pending set, place={place:?}, inspects={}", browser.identifier()));
    open(&host);
    trace("toggle: show_dev_tools returned");
    // Cleared whether or not the callback fired, so a placement can never be inherited by the next
    // inspector opened anywhere.
    pending().lock().expect("devtools mutex poisoned").take();
    trace("toggle: pending cleared");
}

/// Where an inspector goes. The command's own vocabulary, minus what CEF cannot draw.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Place {
    /// Under the pages of the window it was opened from, above the status strips.
    Bottom,
    /// A window of its own — CEF's default, and what returning 0 from the view hook means.
    Window,
}

/// What `toggle` leaves for `on_popup_browser_view_created` to read.
struct Pending {
    place: Place,
    /// The browser being inspected, so a tab switch can tell whether this inspector belongs to what
    /// is showing.
    inspects: i32,
}

fn pending() -> &'static Mutex<Option<Pending>> {
    static PENDING: OnceLock<Mutex<Option<Pending>>> = OnceLock::new();
    PENDING.get_or_init(|| Mutex::new(None))
}

/// `BrowserViewDelegate::on_popup_browser_view_created`, for a popup that is an inspector.
///
/// Answers whether bru placed the view itself. `false` leaves CEF to make it a window, which is
/// both the `window` placement and the fallback for every case this cannot handle.
pub fn on_popup_view(state: &crate::tabs::SharedState, window_id: u32, view: &BrowserView) -> bool {
    let Some(pending) = pending().lock().expect("devtools mutex poisoned").take() else {
        // An inspector nobody asked for through `:devtools` — a context menu, or a CEF that opens
        // one on its own. A window is the answer that surprises nobody.
        trace("on_popup_view: no pending record, leaving it to CEF");
        return false;
    };
    if pending.place == Place::Window {
        trace("on_popup_view: pending says window");
        return false;
    }
    dock(state, window_id, view, pending.inspects)
}

/// Put the inspector under the pages of its window.
///
/// **The index is where the pages end**, which is the same arithmetic `tabs::attach` does: however
/// many strips sit above the pages, plus one per tab of this window. That leaves the status line
/// and the completion panel below it, which is where they are without an inspector — the inspector
/// takes room from the page, not from the chrome.
fn dock(
    state: &crate::tabs::SharedState,
    window_id: u32,
    view: &BrowserView,
    inspects: i32,
) -> bool {
    let (window, layout, tabs) = {
        let state = state.lock().expect("state mutex poisoned");
        (
            state.window_handle(window_id),
            state.layout_handle(window_id),
            state.tab_count_in(window_id),
        )
    };
    let Some(window) = window else {
        trace("dock: no window handle");
        return false;
    };
    trace(&format!("dock: window found, {tabs} tabs"));
    let mut child = View::from(view);
    trace(&format!("dock: layout handle present={}", layout.is_some()));
    // **Flex before the child is placed**, which is what `on_window_created` does for the three
    // strips and says why: the layout keeps its own record of a view's flex and does not need the
    // view to be a child yet.
    if let Some(layout) = layout.as_ref() {
        layout.set_flex_for_view(Some(&mut child), INSPECTOR_FLEX);
    }
    window.add_child_view_at(
        Some(&mut child),
        crate::window::leading_strip_count() + tabs as i32,
    );
    if let Some(layout) = layout.as_ref() {
        // **Flex 0, exactly as the chrome strips have it**, which is what makes `devtools.height`
        // the height rather than a suggestion: a flex-0 child of a BoxLayout is laid out at its
        // preferred size, and the inspector's delegate answers that with the setting. The pages
        // keep flex 1 and absorb what is left.
        //
        // Said out loud rather than left to the default, for the reason the panel's own note gives:
        // a child that has not said 0 is a child that can be handed a share of the leftover.
        layout.set_flex_for_view(Some(&mut child.clone()), INSPECTOR_FLEX);
    }
    state
        .lock()
        .expect("state mutex poisoned")
        .set_inspector(window_id, view.clone(), inspects);
    // **`invalidate_layout` on the child and on the window, in that order** — the recipe
    // `prompt.rs::resize_bar` already uses to change a strip's height while the window is up. A
    // bare `window.layout()` lays out with sizes that were cached before this child existed.
    child.invalidate_layout();
    View::from(&window).invalidate_layout();
    window.layout();
    for i in 0..window.child_view_count() {
        if let Some(kid) = window.child_view_at(i as i32) {
            let b = kid.bounds();
            trace(&format!(
                "  child {i}: {}x{} @{},{} visible={} id={}",
                b.width, b.height, b.x, b.y, kid.is_visible(), kid.id()
            ));
        }
    }
    let bounds = View::from(view).bounds();
    trace(&format!(
        "dock: added at {}, bounds {}x{}",
        crate::window::leading_strip_count() + tabs as i32,
        bounds.width,
        bounds.height
    ));
    true
}

/// Take the inspector out of its window and forget it.
///
/// **Called from the inspector delegate's `on_browser_destroyed` and from nowhere else**, and the
/// two places it was called from first are why. `remove_child_view` from
/// `LifeSpanHandler::on_before_close` killed the process; removing it in `toggle` just before
/// `close_dev_tools` killed it too. Both were silent exits — no panic, nothing on stderr, three
/// times out of three. A view whose browser Chromium is still tearing down is not a view to take
/// out of a layout by hand; `on_browser_destroyed` is CEF saying it is finished with it.
pub fn undock(state: &crate::tabs::SharedState, window_id: u32) {
    let (window, view) = {
        let mut state = state.lock().expect("state mutex poisoned");
        (state.window_handle(window_id), state.take_inspector(window_id))
    };
    let (Some(window), Some(view)) = (window, view) else {
        return;
    };
    // **Posted, not done here.** `on_browser_destroyed` is inside Chromium's teardown of the
    // browser this view wraps, and taking the view out of its parent from there segfaults —
    // measured 2026-08-08 as **exit 139**, which is what turned "silent death" into "SIGSEGV" and
    // ruled out the clean-shutdown explanation. One turn of the UI loop later the teardown has
    // finished and the same three calls are safe. The view is refcounted and the task holds a
    // clone, so it cannot be freed underneath the task.
    let mut task = Undock::new(window, view);
    post_task(ThreadId::UI, Some(&mut task));
}

wrap_task! {
    struct Undock {
        window: Window,
        view: BrowserView,
    }

    impl Task {
        fn execute(&self) {
            trace("undock: removing the view");
            self.window.remove_child_view(Some(&mut View::from(&self.view)));
            View::from(&self.window).invalidate_layout();
            self.window.layout();
        }
    }
}

/// Show or hide the inspector docked in `window` for `inspects`, if there is one.
///
/// Answers the new visibility, or `None` when this window has no inspector for that browser — which
/// is the signal to open one.
fn flip(state: &crate::tabs::SharedState, window_id: u32, inspects: i32) -> Option<bool> {
    let (view, window) = {
        let state = state.lock().expect("state mutex poisoned");
        let (view, for_browser) = state.inspector_in(window_id)?;
        if for_browser != inspects {
            return None;
        }
        (view, state.window_handle(window_id)?)
    };
    let child = View::from(&view);
    let showing = child.is_visible() != 0;
    child.set_visible(i32::from(!showing));
    child.invalidate_layout();
    View::from(&window).invalidate_layout();
    window.layout();
    Some(!showing)
}

/// Show the docked inspector only while the tab it inspects is the one on screen.
///
/// An inspector belongs to one browser; leaving that tab would otherwise leave its panel open under
/// a page it knows nothing about. Called from `tabs::select`, after the tab views themselves have
/// been shown and hidden.
pub fn follow_tab(state: &crate::tabs::SharedState, window_id: u32, showing: Option<i32>) {
    let (view, inspects) = {
        let state = state.lock().expect("state mutex poisoned");
        match state.inspector_in(window_id) {
            Some((view, inspects)) => (view, inspects),
            None => return,
        }
    };
    View::from(&view).set_visible(i32::from(showing == Some(inspects)));
}

/// **Flex 1, and it is not the 0 every other fixed-height child of this window carries.**
///
/// Measured 2026-08-08 on one build, three values, with `invalidate_layout` already in place:
///
///     flex 0  ->  inspector 800   the DevTools browser's own preferred size; the delegate ignored
///     flex 1  ->  inspector 400   exactly `devtools.height`
///     flex 3  ->  inspector 200
///     flex 9  ->  inspector 80
///
/// So here flex is the weight by which a child is allowed to **shrink** from its preferred size
/// when the box is over-subscribed, and 0 means "never shrink" — which for this child means it
/// keeps the 800 CEF gave the browser and the delegate is never consulted. 1 is the value at which
/// the delegate's answer is the height, which is the whole point of having one. The strips get away
/// with 0 because their preferred size *is* what bru asked for at creation; this view was created
/// by CEF, and 800 is what CEF asked for.
const INSPECTOR_FLEX: i32 = 1;

/// How tall a docked inspector is, from `devtools.height`.
pub fn height() -> i32 {
    crate::settings::int_of("devtools.height") as i32
}

/// Whether the inspector about to be created is one bru is going to place itself.
///
/// Asked by `delegate_for_popup_browser_view`, which runs **before**
/// `on_popup_browser_view_created` and so cannot take the pending record — it only reads it.
pub fn docking() -> bool {
    let answer = matches!(
        pending().lock().expect("devtools mutex poisoned").as_ref(),
        Some(Pending { place: Place::Bottom, .. })
    );
    trace(&format!("docking() -> {answer}"));
    answer
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
