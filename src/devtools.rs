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

/// The browser these commands act on: **the tab that is showing, never the browser the key arrived
/// at.**
///
/// A key can reach bru from any browser in the window that has focus, and after `:devtools` docks a
/// panel the thing holding focus is often the divider strip beside it — a `bru://` page with a
/// browser of its own. Measured 2026-08-09 by pressing `wi` and then `wIl`: the first arrived with
/// the tab (browser 2), the second with the divider (browser 5). Every "is one already docked?"
/// question compares against the browser being *inspected*, so it answered no, and `wIl` went down
/// the **open** path — asking for an inspector of the divider, whose view carries a chrome delegate,
/// so CEF gave it a window of its own. What the user saw was a new window where a move was asked
/// for, and nothing in the trace looked wrong until the browser ids were read.
///
/// An inspector is a thing you open on a **page**, so that is what these resolve to. Trap 11 makes
/// the same correction for scrolling, for the same reason: a key that lands on chrome still means
/// the page.
fn for_page(browser: &mut Browser) -> Option<Browser> {
    let state = crate::state::BruState::instance()?;
    let mut guard = state.lock().expect("state mutex poisoned");
    // `window_of_browser` already answers for a chrome browser as well as a tab — the divider is
    // one — so this finds the right window whichever of the two the key came from. The fallback is
    // the current window, for a browser that belongs to no window at all.
    let window = guard
        .window_of_browser(browser.identifier())
        .or_else(|| guard.current_window_id())?;
    guard.active_browser_in(window)
}

/// `devtools [position]` — open the inspector, or close it if it is already open.
///
/// qutebrowser's is a toggle (`browsertab.py:946`, "Show/hide (and if needed, create) the web
/// inspector for this tab"), so this is too.
pub fn toggle(browser: &mut Browser, wanted: Option<Place>) {
    let Some(mut page) = for_page(browser) else {
        return;
    };
    let browser = &mut page;
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
    // So nothing here destroys anything. The inspector is hidden and shown, which is exactly what
    // bru already does to tabs, and a `BoxLayout` skips an invisible child so the page gets the room
    // back. The browser stays alive behind it — the same trade qutebrowser makes, whose inspector is
    // a widget it hides — and it goes when its tab or its window goes, on CEF's own teardown path.
    if let Some(state) = crate::state::BruState::instance() {
        let window = state
            .lock()
            .expect("state mutex poisoned")
            .window_of_browser(browser.identifier());
        if let Some(window) = window {
            let docked = state
                .lock()
                .expect("state mutex poisoned")
                .inspector_in(window)
                .map(|(_, inspects)| inspects)
                == Some(browser.identifier());
            if docked {
                match wanted {
                    // **A named position moves it; a bare command toggles it.** This is the whole of
                    // what the argument was for, and it used to be thrown away: `toggle` flipped
                    // visibility and returned before looking at it, so `:devtools right` on a panel
                    // open at the bottom hid the bottom one and reported nothing. A key that names a
                    // position and does something else is the kind of key this project deletes.
                    // **A window is the one place a docked panel cannot be moved to**, and the
                    // message says why rather than suggesting something that does not help.
                    //
                    // CEF decides where a DevTools popup lives at the moment it creates it, so this
                    // browser's home is settled. Changing it means destroying it and asking again —
                    // and destroying a docked inspector is the measured SIGSEGV above. Nor does
                    // `devtools-close` open the road: closing a docked panel *hides* it, for the
                    // same reason, so the browser is still there and still docked. The first draft
                    // of this message told the user to run `devtools-close` first, and then said the
                    // same thing again when they had — a message that lies is worse than a refusal.
                    //
                    // What does free it is the panel's browser actually going, which happens when
                    // its tab or its window closes. That is the honest answer and it is what this
                    // says.
                    Some(Place::Window) => {
                        crate::message::error(
                            "devtools window: this inspector is docked, and CEF fixed where it \
                             lives when it made it. A window of its own needs a new inspector — \
                             close the tab, or open one on another tab.",
                        );
                        return;
                    }
                    Some(place) if place != place_of(window) => {
                        move_docked(&state, window, place);
                        return;
                    }
                    // Already where it was asked to be: show it rather than toggling it away. A
                    // position is a statement about where, not a request to flip.
                    Some(_) => {
                        show_docked(&state, window);
                        return;
                    }
                    None => {
                        if let Some(shown) = flip(&state, window, browser.identifier()) {
                            trace(&format!("toggle: docked inspector now visible={shown}"));
                            return;
                        }
                    }
                }
            }
        }
    }
    if host.has_dev_tools() != 0 {
        // A windowed inspector, which CEF owns entirely: closing one has never been a problem, and
        // it is also how a windowed panel becomes a docked one — CEF decides where a popup goes when
        // it creates it, so the only way to change that is to let this one go and ask again.
        host.close_dev_tools();
        match wanted {
            Some(Place::Window) | None => return,
            Some(_) => {}
        }
    }
    let place = wanted.unwrap_or(Place::Bottom);
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

/// `devtools-close` — put the inspector away, wherever it is, and say nothing if there is none.
///
/// **Not `close_dev_tools` for a docked one**, for the reason `toggle` gives at length: that call on
/// a panel inside a window is the SIGSEGV this whole design is arranged around. Closing a docked
/// panel is hiding it; the browser behind it goes when its tab or its window does. A panel in its
/// own window is CEF's entirely and closes the ordinary way.
pub fn close(browser: &mut Browser) {
    let Some(mut page) = for_page(browser) else {
        return;
    };
    let browser = &mut page;
    let Some(host) = browser.host() else {
        return;
    };
    if let Some(state) = crate::state::BruState::instance() {
        let window = state
            .lock()
            .expect("state mutex poisoned")
            .window_of_browser(browser.identifier());
        if let Some(window) = window {
            let showing = {
                let guard = state.lock().expect("state mutex poisoned");
                guard
                    .inspector_in(window)
                    .filter(|(_, inspects)| *inspects == browser.identifier())
                    .map(|(view, _)| View::from(&view).is_visible() != 0)
            };
            if let Some(visible) = showing {
                if visible {
                    flip(&state, window, browser.identifier());
                }
                trace("close: docked inspector hidden");
                return;
            }
        }
    }
    if host.has_dev_tools() != 0 {
        host.close_dev_tools();
    }
}

/// Make sure the docked panel is on screen, without toggling it off if it already is.
fn show_docked(state: &crate::tabs::SharedState, window_id: u32) {
    let (view, divider, window) = {
        let guard = state.lock().expect("state mutex poisoned");
        let Some((view, _)) = guard.inspector_in(window_id) else {
            return;
        };
        (view, guard.divider_in(window_id), guard.window_handle(window_id))
    };
    let Some(window) = window else {
        return;
    };
    let child = View::from(&view);
    if child.is_visible() != 0 {
        return;
    }
    child.set_visible(1);
    if let Some(divider) = divider.as_ref() {
        View::from(divider).set_visible(1);
    }
    child.invalidate_layout();
    View::from(&window).invalidate_layout();
    window.layout();
    focus_page(state, window_id);
}

/// Put the keyboard back on the page after the layout has moved.
///
/// **A docked inspector is a browser with no client, so a key pressed while it holds focus never
/// reaches bru at all.** Measured 2026-08-09: `wi` docked at the bottom, `wIl` moved it to the right
/// — and `wIj` after that did nothing whatsoever, no trace line, because re-parenting the views had
/// left focus inside the panel. The command was never called; the key went into the DevTools
/// front-end, which is exactly what having no client is *for* when you are typing in the console,
/// and exactly wrong the instant the panel is placed rather than used.
///
/// So every placement ends with the page holding focus. The divider's drag already does this at
/// `pointerup` for the same reason.
fn focus_page(state: &crate::tabs::SharedState, window_id: u32) {
    let page = state
        .lock()
        .expect("state mutex poisoned")
        .active_view_in(window_id);
    if let Some(page) = page {
        View::from(&page).request_focus();
    }
}

/// Move a docked inspector from one container to the other — `bottom` to `right` and back.
///
/// **The browser is never touched, only its view is re-parented**, and that is the only way this can
/// be done here: closing a docked inspector to reopen it elsewhere is the measured segfault, and CEF
/// decides where a popup lives when it creates it, so there is nothing to ask a second time. Taking
/// a live `BrowserView` out of one container and putting it in another is not new — `tabs::give`
/// does exactly that to hand a tab to another window, and its comment says the browser follows its
/// view.
fn move_docked(state: &crate::tabs::SharedState, window_id: u32, place: Place) {
    let (view, divider, window, layout, pages, pages_layout) = {
        let guard = state.lock().expect("state mutex poisoned");
        let Some((view, _)) = guard.inspector_in(window_id) else {
            return;
        };
        let (pages, pages_layout) = guard.pages_of(window_id);
        (
            view,
            guard.divider_in(window_id),
            guard.window_handle(window_id),
            guard.layout_handle(window_id),
            pages,
            pages_layout,
        )
    };
    let (Some(window), Some(pages)) = (window, pages) else {
        return;
    };
    let was = place_of(window_id);
    // Out of the old container first. `remove_child_view` on a view whose browser is alive and well
    // is the safe half of the pair; it is removal *during teardown* that is not.
    let old: Panel = if was.is_side() {
        pages.clone()
    } else {
        Panel::from(&window)
    };
    old.remove_child_view(Some(&mut View::from(&view)));
    if let Some(divider) = divider.as_ref() {
        old.remove_child_view(Some(&mut View::from(divider)));
    }

    // Recorded before the views go back in, so the delegates answer for the new axis on the very
    // first layout rather than one pass later.
    set_place(window_id, Some(place));
    let (new, new_layout, index) = if place.is_side() {
        (pages.clone(), pages_layout, pages.child_view_count() as i32)
    } else {
        (
            Panel::from(&window),
            layout,
            crate::window::leading_strip_count() + 1,
        )
    };
    if let Some(divider) = divider.as_ref() {
        let mut strip = View::from(divider);
        if let Some(layout) = new_layout.as_ref() {
            layout.set_flex_for_view(Some(&mut strip), 0);
        }
        new.add_child_view_at(Some(&mut strip), index);
    }
    let mut child = View::from(&view);
    if let Some(layout) = new_layout.as_ref() {
        layout.set_flex_for_view(Some(&mut child), INSPECTOR_FLEX);
    }
    new.add_child_view_at(Some(&mut child), index + i32::from(divider.is_some()));
    child.set_visible(1);
    if let Some(divider) = divider.as_ref() {
        View::from(divider).set_visible(1);
    }
    child.invalidate_layout();
    View::from(&window).invalidate_layout();
    window.layout();
    focus_page(state, window_id);
    trace(&format!("move: {was:?} -> {place:?}"));
}

/// Where an inspector goes. The command's own vocabulary, minus what CEF cannot draw.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Place {
    /// Under the pages of the window it was opened from, above the status strips.
    Bottom,
    /// Beside the pages, in the panel that holds them. `left` and `top` are the same two pieces of
    /// arithmetic — an insertion index and a sign — and are not built.
    Right,
    /// A window of its own — CEF's default, and what returning 0 from the view hook means.
    Window,
}

impl Place {
    /// Whether this placement divides the window's width rather than its height. The one question
    /// the layout, the delegate and the divider each have to ask, so it is asked in one place.
    pub fn is_side(self) -> bool {
        matches!(self, Place::Right)
    }
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
    dock(state, window_id, view, pending.inspects, pending.place)
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
    place: Place,
) -> bool {
    // **Recorded before the view is placed**, because placing it is what makes CEF ask the delegate
    // for a size, and the delegate answers a width or a height depending on this.
    set_place(window_id, Some(place));
    let (window, layout, pages, pages_layout) = {
        let state = state.lock().expect("state mutex poisoned");
        let (pages, pages_layout) = state.pages_of(window_id);
        (
            state.window_handle(window_id),
            state.layout_handle(window_id),
            pages,
            pages_layout,
        )
    };
    let Some(window) = window else {
        trace("dock: no window handle");
        return false;
    };
    // **Two containers, one arrangement.** `bottom` divides the window's height, so the inspector is
    // a child of the window's vertical layout, just past the pages panel. `right` divides the pages
    // panel's width, so it is a child of that panel instead, just past the tab views. Everything
    // else — the divider going in first, the flex, the relayout — is the same either way, which is
    // why this is one function with a container in a variable rather than two.
    let side = place.is_side();
    let divider = crate::window::divider_view(state, window_id);
    let (holder, holder_layout, index) = if side {
        let Some(pages) = pages.as_ref() else {
            trace("dock: no pages panel");
            return false;
        };
        (pages.clone(), pages_layout, pages.child_view_count() as i32)
    } else {
        // The window's children are the leading strips, the pages panel, then the trailing strips —
        // one child for the pages however many tabs there are, which is what the panel changed.
        // A `Window` **is** a `Panel` in CEF's hierarchy, so both branches hand back the same type
        // and the placement below is written once.
        (
            Panel::from(&window),
            layout,
            crate::window::leading_strip_count() + 1,
        )
    };
    trace(&format!("dock: {} at index {index}", if side { "right" } else { "bottom" }));
    if let Some(divider) = divider.as_ref() {
        let mut strip = View::from(divider);
        if let Some(layout) = holder_layout.as_ref() {
            layout.set_flex_for_view(Some(&mut strip), 0);
        }
        holder.add_child_view_at(Some(&mut strip), index);
    }
    let mut child = View::from(view);
    // **Flex 0, exactly as the chrome strips have it**, which is what makes the setting the size
    // rather than a suggestion: a flex-0 child of a BoxLayout is laid out at its preferred size, and
    // the inspector's delegate answers that from `devtools.height` or `devtools.width`. The pages
    // keep flex 1 and absorb what is left.
    if let Some(layout) = holder_layout.as_ref() {
        layout.set_flex_for_view(Some(&mut child), INSPECTOR_FLEX);
    }
    holder.add_child_view_at(Some(&mut child), index + i32::from(divider.is_some()));
    if let Some(layout) = holder_layout.as_ref() {
        layout.set_flex_for_view(Some(&mut child.clone()), INSPECTOR_FLEX);
    }
    state.lock().expect("state mutex poisoned").set_inspector(
        window_id,
        view.clone(),
        inspects,
        divider,
    );
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
        index, bounds.width, bounds.height
    ));
    // The page keeps the keyboard, so the next key still reaches bru — see `focus_page`. Typing
    // *into* the panel is a click away, and that is `devtools-focus`'s job rather than an accident
    // of where a view landed.
    focus_page(state, window_id);
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
    // **Out of whatever it was put into.** A `bottom` inspector is a child of the window and a
    // `right` one of the pages panel; removing from the wrong one is a no-op that leaves a live view
    // holding a share of the layout for ever.
    let holder = if place_of(window_id).is_side() {
        state
            .lock()
            .expect("state mutex poisoned")
            .pages_of(window_id)
            .0
    } else {
        state
            .lock()
            .expect("state mutex poisoned")
            .window_handle(window_id)
            .map(|window| Panel::from(&window))
    };
    let (window, view, divider) = {
        let mut state = state.lock().expect("state mutex poisoned");
        // Read before the take: `take_inspector` drops the record the divider is held in.
        let divider = state.divider_in(window_id);
        (state.window_handle(window_id), state.take_inspector(window_id), divider)
    };
    set_place(window_id, None);
    let (Some(window), Some(view), Some(holder)) = (window, view, holder) else {
        return;
    };
    // **Posted, not done here.** `on_browser_destroyed` is inside Chromium's teardown of the
    // browser this view wraps, and taking the view out of its parent from there segfaults —
    // measured 2026-08-08 as **exit 139**, which is what turned "silent death" into "SIGSEGV" and
    // ruled out the clean-shutdown explanation. One turn of the UI loop later the teardown has
    // finished and the same three calls are safe. The view is refcounted and the task holds a
    // clone, so it cannot be freed underneath the task.
    let mut task = Undock::new(window, holder, view, divider);
    post_task(ThreadId::UI, Some(&mut task));
}

// The divider goes out with the inspector, in the same posted turn: a strip left behind would be
// six pixels of chrome between the page and the status bar with nothing under it to resize. No doc
// comments on these fields — `wrap_task!` takes none.
wrap_task! {
    struct Undock {
        window: Window,
        holder: Panel,
        view: BrowserView,
        divider: Option<BrowserView>,
    }

    impl Task {
        fn execute(&self) {
            trace("undock: removing the view");
            self.holder.remove_child_view(Some(&mut View::from(&self.view)));
            if let Some(divider) = self.divider.as_ref() {
                self.holder.remove_child_view(Some(&mut View::from(divider)));
            }
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
    let (view, divider, window) = {
        let state = state.lock().expect("state mutex poisoned");
        let (view, for_browser) = state.inspector_in(window_id)?;
        if for_browser != inspects {
            return None;
        }
        (view, state.divider_in(window_id), state.window_handle(window_id)?)
    };
    let child = View::from(&view);
    let showing = child.is_visible() != 0;
    child.set_visible(i32::from(!showing));
    // The strip goes with the panel it resizes, both ways.
    if let Some(divider) = divider.as_ref() {
        View::from(divider).set_visible(i32::from(!showing));
    }
    child.invalidate_layout();
    View::from(&window).invalidate_layout();
    window.layout();
    focus_page(state, window_id);
    Some(!showing)
}

/// Show the docked inspector only while the tab it inspects is the one on screen.
///
/// An inspector belongs to one browser; leaving that tab would otherwise leave its panel open under
/// a page it knows nothing about. Called from `tabs::select`, after the tab views themselves have
/// been shown and hidden.
pub fn follow_tab(state: &crate::tabs::SharedState, window_id: u32, showing: Option<i32>) {
    let (view, inspects, divider) = {
        let state = state.lock().expect("state mutex poisoned");
        match state.inspector_in(window_id) {
            Some((view, inspects)) => (view, inspects, state.divider_in(window_id)),
            None => return,
        }
    };
    // **Whether the panel is on screen is not the same question as whether it was open.** A panel
    // hidden by `:devtools` and a panel hidden by leaving its tab both answer 0 here; coming back to
    // the tab shows it again either way, which is the one place this differs from qutebrowser, whose
    // inspector remembers a closed state per tab. Left as it is: the toggle is per window here.
    let on_screen = i32::from(showing == Some(inspects));
    View::from(&view).set_visible(on_screen);
    if let Some(divider) = divider.as_ref() {
        View::from(divider).set_visible(on_screen);
    }
}

/// Lay out again every window that has an inspector docked, so a new `devtools.height` is on screen.
///
/// **The setting is not read here, and that is the point.** The height reaches the layout through
/// `BruInspectorViewDelegate::preferred_size`, which is asked afresh during a layout pass and reads
/// the store itself; all this has to do is cause the pass. The three calls are
/// `prompt.rs::resize_bar`'s, the same recipe that changes a strip's height under a running window.
///
/// Called from `Backing::Devtools`. A window with no inspector is skipped rather than laid out for
/// nothing, and a hidden inspector is relayed out too — it is the height it will have when its tab
/// comes back.
pub fn apply_height_everywhere() {
    let Some(state) = crate::state::BruState::instance() else {
        return;
    };
    let work: Vec<_> = {
        let Ok(state) = state.lock() else {
            return;
        };
        state
            .window_ids()
            .into_iter()
            .filter_map(|id| {
                let (view, _) = state.inspector_in(id)?;
                Some((view, state.window_handle(id)?))
            })
            .collect()
    };
    for (view, window) in work {
        let child = View::from(&view);
        child.invalidate_layout();
        View::from(&window).invalidate_layout();
        window.layout();
        trace(&format!(
            "apply_height: relaid out at {}x{}",
            child.bounds().width,
            child.bounds().height
        ));
    }
}

/// **Flex 0, the same as every other fixed-height child of this window.**
///
/// It was 1, on a reading of these measurements, taken 2026-08-08 on one build with
/// `invalidate_layout` already in place:
///
///     flex 0  ->  inspector 800
///     flex 1  ->  inspector 400
///     flex 3  ->  inspector 200
///     flex 9  ->  inspector 80
///
/// The reading was that flex is a shrink weight and 1 is the value at which the delegate is heard.
/// It is not. Those four numbers are `800 / (flex + 1)` exactly, and they say nothing about the
/// delegate: the delegate's answer was being discarded for its zero width (see the comment on
/// `BruInspectorViewDelegate::preferred_size`), so both flexed children fell back to their current
/// size as their preferred size. The page filled the content area and contributed no free space,
/// leaving the popup's own 800 as the entire deficit, shared between page and inspector as
/// `flex / (flex + 1)`. "Flex 1 gave exactly 400" was `800 / 2` — a coincidence, and the reason
/// every later attempt to hold that 400 failed. Flex is the documented symmetric grow/shrink
/// weight (`views::BoxLayout`, box_layout.h:145), and a child that must not move takes 0.
const INSPECTOR_FLEX: i32 = 0;

/// How tall a docked inspector is, from `devtools.height`.
pub fn height() -> i32 {
    crate::settings::int_of("devtools.height") as i32
}

/// The smallest a drag may leave the inspector. The same 80 the setting's range refuses to go under,
/// because a drag and a `:set` are the same number arriving by different roads.
const MIN_INSPECTOR: i32 = 80;
/// The smallest a drag may leave the **page**, which is the thing worth protecting: an inspector
/// dragged to the top of the window would hide what it is inspecting. 150 DIP is qutebrowser's
/// `_PROTECTED_MAIN_SIZE` (`miscwidgets.py:303`), doing the same job for its splitter.
const PROTECTED_PAGE: i32 = 150;

/// The height a drag is holding, per window, while the button is down.
///
/// **Not the setting, and deliberately.** A drag asks for a new height sixty times a second; writing
/// each one into the settings store would run the whole `:set` path — validation, the apply arm,
/// every listener — per frame, and would leave the store holding numbers the user never chose if the
/// drag were cancelled. The store is written once, at `end`, and until then this is what the
/// delegate answers with.
///
/// A plain `Mutex<Vec<_>>` read from inside `preferred_size`, exactly as `window.rs`'s `STRIPS` is:
/// the lock is never held across a CEF call, so a layout pass asking for it cannot meet a caller
/// holding it.
static DRAGGING: std::sync::Mutex<Vec<(u32, i32)>> = std::sync::Mutex::new(Vec::new());

/// Where each window's docked inspector is, so the delegates can answer the right axis.
///
/// Kept beside the layout rather than in the window's state because it is read from inside
/// `preferred_size`, and that is a place where taking the state mutex would meet whatever caller
/// asked for the layout. The same reasoning as `DRAGGING` above and `window.rs`'s `STRIPS`.
static PLACES: std::sync::Mutex<Vec<(u32, Place)>> = std::sync::Mutex::new(Vec::new());

/// Where the inspector docked in `window` is. `Bottom` when nothing is docked, which is the answer
/// that makes a delegate asked out of turn behave as it always did.
pub fn place_of(window: u32) -> Place {
    PLACES
        .lock()
        .ok()
        .and_then(|live| {
            live.iter()
                .find(|(id, _)| *id == window)
                .map(|(_, place)| *place)
        })
        .unwrap_or(Place::Bottom)
}

fn set_place(window: u32, place: Option<Place>) {
    let Ok(mut live) = PLACES.lock() else {
        return;
    };
    live.retain(|(id, _)| *id != window);
    if let Some(place) = place {
        live.push((window, place));
    }
}

/// How wide a side-docked inspector is, from `devtools.width`.
pub fn width() -> i32 {
    crate::settings::int_of("devtools.width") as i32
}

/// What a side-docked inspector's width should be right now — the drag's if one is running.
pub fn width_for(window: u32) -> i32 {
    DRAGGING
        .lock()
        .ok()
        .and_then(|live| {
            live.iter()
                .find(|(id, _)| *id == window)
                .map(|(_, size)| *size)
        })
        .unwrap_or_else(width)
}

/// The size a docked inspector's delegate answers, whichever axis it divides.
///
/// **Both numbers are non-empty, always.** A zero on either axis is a size CEF discards unread —
/// CEF-NOTES trap 25 — so the axis this placement does not decide is given a plausible number rather
/// than a zero, and the layout stretches it on the cross axis anyway.
pub fn inspector_size(window: u32) -> Size {
    if place_of(window).is_side() {
        Size { width: width_for(window), height: 300 }
    } else {
        Size { width: 1280, height: height_for(window) }
    }
}

/// What the inspector in `window` should be right now: the drag's height if one is in progress,
/// the setting otherwise. This is what the delegate answers.
pub fn height_for(window: u32) -> i32 {
    DRAGGING
        .lock()
        .ok()
        .and_then(|live| {
            live.iter()
                .find(|(id, _)| *id == window)
                .map(|(_, height)| *height)
        })
        .unwrap_or_else(height)
}

fn set_dragging(window: u32, height: Option<i32>) {
    let Ok(mut live) = DRAGGING.lock() else {
        return;
    };
    live.retain(|(id, _)| *id != window);
    if let Some(height) = height {
        live.push((window, height));
    }
}

/// How tall the divider view itself is: six pixels of chrome, or the whole area during a drag.
///
/// **The expansion is the drag.** A page is only told where the pointer is while the pointer is over
/// it, and a pointer that leaves a strip upward is not reported at all — CEF-NOTES trap 26, 410
/// events, not one of them upward. Chasing the pointer with a small strip therefore works in exactly
/// one direction. Surrounding it works in both: on `pointerdown` the strip becomes the whole area
/// the page and the inspector share, they are hidden behind it, and the pointer has nowhere to go
/// that is not this view.
static EXPANDED: std::sync::Mutex<Vec<(u32, i32)>> = std::sync::Mutex::new(Vec::new());

/// The size the divider's delegate answers: a thin strip across the split, or the whole shared area
/// while a drag is running. Which axis is which follows the placement.
pub fn divider_size(window: u32) -> Size {
    let across = divider_height_for(window);
    if place_of(window).is_side() {
        Size { width: across, height: 300 }
    } else {
        Size { width: 1280, height: across }
    }
}

/// What the divider spans across the split — its thickness at rest, the whole shared area during a
/// drag. Read from inside a layout pass, so the lock is taken and dropped without a CEF call in
/// between, exactly as `height_for` is.
pub fn divider_height_for(window: u32) -> i32 {
    EXPANDED
        .lock()
        .ok()
        .and_then(|live| {
            live.iter()
                .find(|(id, _)| *id == window)
                .map(|(_, height)| *height)
        })
        .unwrap_or(crate::window::DIVIDER_HEIGHT)
}

fn set_expanded(window: u32, height: Option<i32>) {
    let Ok(mut live) = EXPANDED.lock() else {
        return;
    };
    live.retain(|(id, _)| *id != window);
    if let Some(height) = height {
        live.push((window, height));
    }
}

/// A press or a release on a window's divider. Nothing arrives in between.
///
/// Called from `ipc.rs` with the divider's own browser, which is what names the window — the strip
/// has no idea which one it is in and does not need to. Answers `None` when the query cannot be
/// placed at all, which the caller reports back to the page as a failure; otherwise the JSON the
/// page is owed.
///
/// **The panel does not move while the button is down, and that is not a compromise.** The page has
/// the whole area to draw in, so it draws the split where the pointer is at its own refresh rate,
/// with no query per frame and no relayout per frame; Rust hears the number once, on release. What
/// the page needs to do that is the geometry it is given here at `start`.
///
/// `y` is the boundary the page ended on, in its own expanded coordinates. Meaningless at `start`.
pub fn on_divider_query(
    state: &crate::tabs::SharedState,
    browser_id: i32,
    phase: &str,
    y: i32,
) -> Option<String> {
    let window_id = state
        .lock()
        .expect("state mutex poisoned")
        .window_of_divider(browser_id)?;

    match phase {
        "start" => {
            let (page, inspector) = shared_views(state, window_id)?;
            // The axis the placement divides. Everything below is written once and reads the same
            // number out of a different field.
            let side = place_of(window_id).is_side();
            let along = |view: &BrowserView| {
                let b = View::from(view).bounds();
                if side { b.width } else { b.height }
            };
            let page_height = along(&page);
            let inspector_height = along(&inspector);
            let total = page_height + crate::window::DIVIDER_HEIGHT + inspector_height;
            // Hidden first, expanded second: a `BoxLayout` skips an invisible child, so the room
            // the two of them were holding is free by the time the divider asks for it.
            View::from(&page).set_visible(0);
            View::from(&inspector).set_visible(0);
            set_expanded(window_id, Some(total));
            state
                .lock()
                .expect("state mutex poisoned")
                .set_drag_from(window_id, Some(inspector_height));
            relayout_window(state, window_id);
            trace(&format!(
                "divider: expanded to {total}, boundary at {page_height}"
            ));
            // Where the line is now, and how far it may travel. The page clamps against these
            // itself so that what it draws is what it will get — a preview that shows a split the
            // release would refuse is a preview that lies.
            // `strip` so the page can do the same arithmetic Rust does at the release. Without it
            // the preview promised eight pixels more inspector than the release gave, because the
            // page had guessed the strip's thickness from how thick it drew the line.
            Some(format!(
                "{{\"top\":{page_height},\"min\":{},\"max\":{},\"strip\":{},\"total\":{total},\"axis\":\"{}\"}}",
                PROTECTED_PAGE,
                (total - crate::window::DIVIDER_HEIGHT - MIN_INSPECTOR).max(PROTECTED_PAGE),
                crate::window::DIVIDER_HEIGHT,
                if side { "x" } else { "y" }
            ))
        }
        "end" => {
            let started_at = state
                .lock()
                .expect("state mutex poisoned")
                .drag_from(window_id);
            let total = divider_height_for(window_id);
            // Back to a strip before anything else, so that a failure below leaves a window with a
            // page in it rather than one holding a twelve-hundred-pixel divider.
            set_expanded(window_id, None);
            if let Some((page, inspector)) = shared_views(state, window_id) {
                View::from(&page).set_visible(1);
                View::from(&inspector).set_visible(1);
            }
            state
                .lock()
                .expect("state mutex poisoned")
                .set_drag_from(window_id, None);

            // The boundary the page ended on, read as a height: everything below the line, less the
            // strip itself. A release with no movement answers the height it started at.
            let wanted = clamp_height(
                state,
                window_id,
                total - crate::window::DIVIDER_HEIGHT - y,
            );
            let held = match started_at {
                Some(was) if y <= 0 => was,
                _ => wanted,
            };
            set_dragging(window_id, Some(held));
            relayout_window(state, window_id);
            // **The store is written after the layout, not before.** `run_set` runs the apply arm,
            // which relayouts every window with an inspector; doing that first would lay this one
            // out while it was still expanded.
            if held != height() {
                // `run_set` and not some quieter writer: a dragged height is a `:set devtools.height`
                // that happened to be typed with a mouse, and it should validate, apply and be
                // visible in `:set devtools.height?` and in `config-diff` exactly like the typed one.
                // `print` is false — the drag is its own feedback, and a message per release would be
                // noise.
                crate::settings::run_set(
                    Some("devtools.height"),
                    Some(&held.to_string()),
                    None,
                    false,
                );
            }
            set_dragging(window_id, None);
            // The keyboard goes back to the page. Without this the next `j` is typed into the
            // divider — a document with nothing to scroll and nothing to type into.
            if let Some(page) = state
                .lock()
                .expect("state mutex poisoned")
                .active_view_in(window_id)
            {
                View::from(&page).request_focus();
            }
            trace(&format!("divider: drag ended at {held}"));
            Some(String::new())
        }
        // A phase this does not act on. Traced rather than failed — the page says nothing else
        // today, and a phase bru has not heard of is worth seeing in the log rather than swallowing.
        other => {
            trace(&format!("divider: {other}"));
            Some(String::new())
        }
    }
}

/// The two views a drag divides between: the tab on screen and the inspector under it.
fn shared_views(
    state: &crate::tabs::SharedState,
    window_id: u32,
) -> Option<(BrowserView, BrowserView)> {
    let state = state.lock().expect("state mutex poisoned");
    Some((
        state.active_view_in(window_id)?,
        state.inspector_in(window_id).map(|(view, _)| view)?,
    ))
}

/// Lay the window out with whatever the delegates now answer.
fn relayout_window(state: &crate::tabs::SharedState, window_id: u32) {
    let (window, divider) = {
        let state = state.lock().expect("state mutex poisoned");
        (state.window_handle(window_id), state.divider_in(window_id))
    };
    let Some(window) = window else {
        return;
    };
    if let Some(divider) = divider {
        View::from(&divider).invalidate_layout();
    }
    View::from(&window).invalidate_layout();
    window.layout();
}

/// Keep a dragged height inside what the window can actually show.
///
/// The floor is [`MIN_INSPECTOR`]; the ceiling is everything the page and the inspector share, less
/// [`PROTECTED_PAGE`]. That total is measured rather than computed from the window: the two views
/// are the only children whose height a drag moves, so their current heights added are exactly the
/// room there is, whatever chrome the user has turned on above and below them.
fn clamp_height(state: &crate::tabs::SharedState, window_id: u32, wanted: i32) -> i32 {
    let (page, inspector) = {
        let state = state.lock().expect("state mutex poisoned");
        (
            state.active_view_in(window_id),
            state.inspector_in(window_id).map(|(view, _)| view),
        )
    };
    let side = place_of(window_id).is_side();
    let along = |view: &BrowserView| {
        let b = View::from(view).bounds();
        if side { b.width } else { b.height }
    };
    let shared = match (page, inspector) {
        (Some(page), Some(inspector)) => along(&page) + along(&inspector),
        // No page to protect — a window mid-teardown. The floor still applies.
        _ => return wanted.max(MIN_INSPECTOR),
    };
    let ceiling = (shared - PROTECTED_PAGE).max(MIN_INSPECTOR);
    wanted.clamp(MIN_INSPECTOR, ceiling)
}


/// Whether the inspector about to be created is one bru is going to place itself.
///
/// Asked by `delegate_for_popup_browser_view`, which runs **before**
/// `on_popup_browser_view_created` and so cannot take the pending record — it only reads it.
pub fn docking() -> bool {
    let answer = matches!(
        pending().lock().expect("devtools mutex poisoned").as_ref(),
        Some(Pending { place: Place::Bottom | Place::Right, .. })
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
    let Some(page) = for_page(browser) else {
        return;
    };
    let Some(host) = page.host() else {
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
