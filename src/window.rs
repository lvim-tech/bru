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
// --- src/window.rs: the panel ------------------------------------------------------------------
/// The completion table and the prompt, in the one chrome view that changes size.
const PANEL_URL: &str = "bru://chrome/panel.html";
/// The panel is nothing when nothing is open, and `preferred_size` adds the two blocks' heights to
/// this. Zero rather than a band the box layout has to be told to hide.
const PANEL_HEIGHT: i32 = 0;

/// The tab strip. Hidden by `tabs.show`, never grows.
pub const KIND_TABS: i32 = 0;
/// The status bar. Hidden by `statusbar.show`, and **never grows** — that is the whole reason the
/// panel is its own view.
pub const KIND_BAR: i32 = 1;
/// The completion table and the prompt. Grows; hidden with the bar, because a panel over a hidden
/// status line would be a block floating on the page with nothing under it.
pub const KIND_PANEL: i32 = 2;
// --- end src/window.rs: the panel ---------------------------------------------------------------
// --- src/devtools.rs ----------------------------------------------------------------------------
/// The strip between the pages and the docked inspector, and the only chrome view that is not made
/// with the window: it exists exactly as long as an inspector is docked. See [`divider_view`].
pub const KIND_DIVIDER: i32 = 3;
/// Twelve logical pixels, of which the eye sees two.
///
/// **The other ten are the drag's headroom, and the drag does not work without them.** A pointer
/// that leaves the strip stops being reported: measured 2026-08-09, positions floor at the strip's
/// top edge, so a hand moving upward faster than the strip can follow reports no movement at all and
/// the panel never catches up — it could be shrunk and not grown. Downward there is room to spare,
/// which is why only one direction was broken. With the line drawn in the middle, each frame has
/// half the strip's height to play with in either direction and the strip re-centres under the
/// pointer as it follows.
///
/// Pointer lock would have removed the need for any of this — it reports movement rather than
/// position — but this CEF refuses it: `UnknownError`, Chromium's own catch-all, from
/// `requestPointerLock` on a chrome page. See `chrome/divider.js`.
pub const DIVIDER_HEIGHT: i32 = 12;
const DIVIDER_URL: &str = "bru://chrome/divider.html";
// --- end src/devtools.rs ------------------------------------------------------------------------

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

    // --- src/chrome.rs: the blink on the panel's top edge ---------------------------------------
    // **A view that has just been made taller hands the renderer pixels it has never painted**, and
    // CEF fills them with `background_color` until the renderer catches up. At the default that
    // field's alpha is 0 — fully transparent — so what showed through was **the page behind**. The
    // completion panel grows upward, so the band was along its top edge, exactly where its category
    // header is, and on an orange page it flashed orange. Photographed 2026-08-07; the paint
    // ordering had already been fixed by then and the band stayed, which is what pointed here.
    //
    // The page's own browser is created in `tabs.rs` with CEF's defaults and is left alone: its
    // pre-load colour is a different decision, seen on every navigation rather than on a resize.
    let mut chrome_settings = BrowserSettings::default();
    if let Some(colour) = crate::chrome::chrome_background() {
        chrome_settings.background_color = colour;
    }
    // --- end src/chrome.rs: the blink on the panel's top edge -----------------------------------
    let mut client = state.lock().expect("state mutex poisoned").client();

    // All three views share one Client, so one set of handlers serves the page and the chrome.
    // Which browser an event came from is answered by its identifier, not by its handler. The last
    // argument is `grows` — see `COMPLETION_HEIGHT`. Only the bottom strip does; the tab strip's
    // height is its own.
    let mut top_delegate =
        BruChromeViewDelegate::new(state.clone(), window_id, TOP_HEIGHT, KIND_TABS);
    let top_view = browser_view_create(
        client.as_mut(),
        Some(&CefString::from(TOP_URL)),
        Some(&chrome_settings),
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

    // --- src/window.rs: the panel ---------------------------------------------------------------
    // **`false`, and that is the fix for the bar jumping.** The bottom strip used to carry the
    // completion table and the prompt and grow with them; the frame a panel opened on had the
    // browser process holding the view at its new height while the renderer was still painting at
    // the old one, and the bar landed at the top of the new area for exactly one frame. It is 24px
    // now and never anything else, so there is no frame in which its size is in question.
    let mut bottom_delegate =
        BruChromeViewDelegate::new(state.clone(), window_id, BOTTOM_HEIGHT, KIND_BAR);
    // The view that does grow. Its own document, its own frame, and nothing in it that has to stay
    // still: the frame where its size and its content disagree is a frame of empty panel.
    let mut panel_delegate =
        BruChromeViewDelegate::new(state.clone(), window_id, PANEL_HEIGHT, KIND_PANEL);
    let panel_view = browser_view_create(
        client.as_mut(),
        Some(&CefString::from(PANEL_URL)),
        Some(&chrome_settings),
        None,
        None,
        Some(&mut panel_delegate),
    );
    // Invisible until something opens it. A `BoxLayout` skips an invisible child, which is what
    // keeps the pages whole on a window that has never had a completion table in it — the first
    // `apply_height` above zero is what shows it.
    if let Some(view) = panel_view.as_ref() {
        View::from(view).set_visible(0);
    }
    // --- end src/window.rs: the panel -------------------------------------------------------------
    let bottom_view = browser_view_create(
        client.as_mut(),
        Some(&CefString::from(BOTTOM_URL)),
        Some(&chrome_settings),
        None,
        None,
        Some(&mut bottom_delegate),
    );

    let mut window_delegate = BruWindowDelegate::new(
        state.clone(),
        window_id,
        RefCell::new(top_view),
        RefCell::new(bottom_view),
        RefCell::new(panel_view),
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
    set_height_in(&COMPLETION_HEIGHTS, window, height)
}

fn completion_height(window: u32) -> i32 {
    height_in(&COMPLETION_HEIGHTS, window)
}
// --- end src/completers.rs -----------------------------------------------------------------

// --- src/prompt.rs -------------------------------------------------------------------------
/// The same, for the prompt block — `src/prompt.rs`.
///
/// **A second slot rather than a second writer of the first**, and deliberately belt and braces.
/// The completion table and a prompt are the only two things that make the bottom strip grow, and
/// today they cannot be open at the same time: the completion answers `null` outside command mode,
/// and a prompt takes the mode away from command. So one shared slot would work — until it did not.
/// `bar_json_for` asks the completion and then the prompt on **every** push, so with one slot the
/// order of those two calls silently decides the strip's height, and the day something opens a
/// completion in a mode a question can also be raised in, whichever ran last wins and the other is
/// drawn inside a height that is not its own. Two slots added together in
/// [`BruChromeViewDelegate::preferred_size`] cannot do that, neither module has to know the other
/// exists, and the cost is one `Vec` lookup per layout pass. **Not measured, because it cannot be
/// today** — it is here so that it never has to be.
static PROMPT_HEIGHTS: std::sync::Mutex<Vec<(u32, i32)>> = std::sync::Mutex::new(Vec::new());

/// Store a window's prompt height and answer what it was, so `prompt::resize_bar` relayouts only
/// when it moved.
pub fn set_prompt_height(window: u32, height: i32) -> i32 {
    set_height_in(&PROMPT_HEIGHTS, window, height)
}

/// What the prompt block last asked for. **Nothing reads this to size the panel any more** — see the
/// delegate's `preferred_size`, which takes the page's own measurement of both blocks. It is kept as
/// the record `prompt::resize_bar` compares against so that a redraw which moves nothing costs no
/// relayout.
#[allow(dead_code)]
fn prompt_height(window: u32) -> i32 {
    height_in(&PROMPT_HEIGHTS, window)
}
// --- end src/prompt.rs ---------------------------------------------------------------------

// --- src/completers.rs ---------------------------------------------------------------------
fn set_height_in(slot: &std::sync::Mutex<Vec<(u32, i32)>>, window: u32, height: i32) -> i32 {
    let Ok(mut heights) = slot.lock() else {
        return height;
    };
    match heights.iter_mut().find(|(id, _)| *id == window) {
        Some(found) => std::mem::replace(&mut found.1, height),
        None => {
            heights.push((window, height));
            0
        }
    }
}

fn height_in(slot: &std::sync::Mutex<Vec<(u32, i32)>>, window: u32) -> i32 {
    slot.lock()
        .ok()
        .and_then(|heights| {
            heights
                .iter()
                .find(|(id, _)| *id == window)
                .map(|(_, height)| *height)
        })
        .unwrap_or(0)
}

/// `BRU_DEBUG_BAR=1` prints the height each window's bottom strip asks for, every time CEF asks.
///
/// It is the only way to measure this without a screenshot, and a screenshot is not a measurement
/// here: `grim` photographs the whole output and six brus share this compositor. Off by default —
/// a layout pass is frequent enough that this is a line per relayout.
fn debug_bar(window: u32, height: i32) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("BRU_DEBUG_BAR").is_some()) {
        eprintln!("bru[bar]: window {window} asks for {height} px");
    }
}

/// Drop a window's entry, from `on_window_destroyed`. Without it the list grows by one row for
/// every window ever opened, and a later window that happened to reuse the id — nothing does today,
/// `next_window_id` only goes up — would inherit a height it never asked for.
fn forget_completion_height(window: u32) {
    if let Ok(mut heights) = COMPLETION_HEIGHTS.lock() {
        heights.retain(|(id, _)| *id != window);
    }
    // --- src/prompt.rs ---------------------------------------------------------------------
    if let Ok(mut heights) = PROMPT_HEIGHTS.lock() {
        heights.retain(|(id, _)| *id != window);
    }
    // --- end src/prompt.rs -----------------------------------------------------------------
}
// --- end src/completers.rs -----------------------------------------------------------------

// --- tabs and statusbar ------------------------------------------------------------------------
//
// Where the two strips go and whether they are drawn at all. Four settings decide it —
// `tabs.position`, `tabs.show`, `statusbar.position`, `statusbar.show` — and this is the only place
// that reads them, so `tabs.rs` asks one question (`leading_strip_count`) and nothing else in bru
// has an opinion about child order.
//
// **Placement and visibility are separate, and that is not a style choice.** A hidden strip stays a
// child of the window; only its `set_visible` flag and its preferred height change. If hiding
// removed it, every tab's index in `Window::add_child_view_at` would shift underneath
// `tabs::attach`, and `xt` would renumber the strip it was hiding.

/// One window's two chrome views, kept so a `:set` can reorder and hide them without going through
/// the delegate that made them.
///
/// A `BrowserView` in a `Mutex` is the same move `ipc.rs` makes with `Frame` and for the same
/// reason: `RefGuard<T>` is `unsafe impl Send + Sync` (CEF-NOTES). Everything below runs on the UI
/// thread anyway; the mutex is here because the list is reached from `settings::apply`, which is not
/// a window callback.
struct Strips {
    window: u32,
    top: BrowserView,
    bottom: BrowserView,
    /// What was last handed to CEF. `apply_chrome_layout` compares against it and returns without
    /// touching a view when nothing moved — `ipc::set_tabs_for` calls it on every address change,
    /// and reordering a Views tree on each of those would be a relayout per navigation.
    applied: Option<Layout>,
    /// Which tab switch this window is currently showing the strip for, under `tabs.show
    /// switching`. Bumped on every switch so that a second switch inside the delay does not get its
    /// strip taken away by the first one's task.
    switch_generation: u64,
    /// Whether that delay is still running.
    switching: bool,
}

/// Where the two strips are and whether they are drawn.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Layout {
    tabs_at_top: bool,
    status_at_top: bool,
    tabs_visible: bool,
    status_visible: bool,
}

static STRIPS: std::sync::Mutex<Vec<Strips>> = std::sync::Mutex::new(Vec::new());

/// How long `tabs.show switching` keeps the strip up. qutebrowser's own
/// `tabs.show_switching_delay` default (`configdata.yml:2354`), compiled in rather than settable:
/// bru has no integer setting kind, and a millisecond count spelled as text would be a number
/// validated nowhere. See REFUSED in `settings.rs`.
const SWITCHING_DELAY_MS: i64 = 800;

/// How many strips sit above the pages — 0, 1 or 2.
///
/// `tabs::attach` adds a tab's view at `index + this`, so it is the offset that used to be the
/// literal `1` meaning "the tab strip is child 0".
pub fn leading_strip_count() -> i32 {
    let places = places();
    i32::from(places.tabs_at_top) + i32::from(places.status_at_top)
}

/// Only the placement half, which is a pure function of two settings and the same for every window.
fn places() -> Layout {
    Layout {
        tabs_at_top: crate::settings::choice_of("tabs.position") != "bottom",
        status_at_top: crate::settings::choice_of("statusbar.position") == "top",
        // Not asked here — `wanted` fills these in per window.
        tabs_visible: true,
        status_visible: true,
    }
}

/// The whole layout one window should be in right now.
fn wanted(window: u32) -> Layout {
    let mut layout = places();
    layout.tabs_visible = strip_shown(window, true);
    layout.status_visible = strip_shown(window, false);
    layout
}

/// Whether a strip should be drawn. `tabs` picks which of the two settings answers.
///
/// **`statusbar.show never` still shows the bar while something is being typed into it**, and that
/// is measured rather than preferred. qutebrowser's `StatusBar.maybe_hide` hides it outright, and it
/// can: its command line is a `QLineEdit` it shows on demand. bru's is an `<input>` inside a
/// `BrowserView`, and a Views child that is not visible cannot take focus — so the literal reading
/// would make `:` a key that changes the mode, moves the focus nowhere, and leaves the user with no
/// way to type the `:set statusbar.show always` that undoes it. Three modes keep it: command, and
/// the two prompt modes, which are the three that own the strip's own inputs.
fn strip_shown(window: u32, tabs: bool) -> bool {
    if tabs {
        return match crate::settings::choice_of("tabs.show") {
            "never" => false,
            "multiple" => tab_count_in(window) > 1,
            "switching" => is_switching(window),
            // `always`, and anything a later CEF or a typo could produce: a strip that is there is
            // the safe answer, because a strip that is not cannot be brought back by clicking it.
            _ => true,
        };
    }
    match crate::settings::choice_of("statusbar.show") {
        "never" => strip_holds_the_keyboard(window),
        "in-mode" => {
            crate::state::BruState::instance()
                .and_then(|state| state.lock().ok().map(|state| state.mode_in(window)))
                .unwrap_or(crate::modes::Mode::Normal)
                != crate::modes::Mode::Normal
        }
        _ => true,
    }
}

/// Whether this window's bottom strip is the thing keys are going into — command mode, or one of
/// the two prompt modes.
fn strip_holds_the_keyboard(window: u32) -> bool {
    let mode = crate::state::BruState::instance()
        .and_then(|state| state.lock().ok().map(|state| state.mode_in(window)))
        .unwrap_or(crate::modes::Mode::Normal);
    matches!(
        mode,
        crate::modes::Mode::Command | crate::modes::Mode::Prompt | crate::modes::Mode::YesNo
    )
}

fn tab_count_in(window: u32) -> usize {
    crate::state::BruState::instance()
        .and_then(|state| state.lock().ok().map(|state| state.tab_count_in(window)))
        .unwrap_or(0)
}

/// Remember one window's two strips. Called from `on_window_created`, before anything can ask.
fn remember_strips(window: u32, top: &BrowserView, bottom: &BrowserView) {
    if let Ok(mut strips) = STRIPS.lock() {
        strips.retain(|entry| entry.window != window);
        strips.push(Strips {
            window,
            top: top.clone(),
            bottom: bottom.clone(),
            applied: None,
            switch_generation: 0,
            switching: false,
        });
    }
}

fn forget_strips(window: u32) {
    if let Ok(mut strips) = STRIPS.lock() {
        strips.retain(|entry| entry.window != window);
    }
}

/// Whether this window is inside a `tabs.show switching` delay.
fn is_switching(window: u32) -> bool {
    STRIPS
        .lock()
        .ok()
        .and_then(|strips| {
            strips
                .iter()
                .find(|entry| entry.window == window)
                .map(|entry| entry.switching)
        })
        .unwrap_or(false)
}

/// A tab was selected in this window — `tabs::select_in` says so.
///
/// Under `tabs.show switching` it puts the strip up and posts the task that takes it away again;
/// under every other value it does nothing at all, so the common path is one string compare.
pub fn note_tab_switch(window: u32) {
    debug_assert_ne!(currently_on(ThreadId::UI), 0);
    if crate::settings::choice_of("tabs.show") != "switching" {
        return;
    }
    let generation = {
        let Ok(mut strips) = STRIPS.lock() else {
            return;
        };
        let Some(entry) = strips.iter_mut().find(|entry| entry.window == window) else {
            return;
        };
        entry.switching = true;
        entry.switch_generation += 1;
        entry.switch_generation
    };
    apply_chrome_layout(window);
    let mut task = HideSwitchingStrip::new(window, generation);
    post_delayed_task(ThreadId::UI, Some(&mut task), SWITCHING_DELAY_MS);
}

wrap_task! {
    struct HideSwitchingStrip {
        window: u32,
        generation: u64,
    }

    impl Task {
        fn execute(&self) {
            let stale = {
                let Ok(mut strips) = STRIPS.lock() else { return };
                let Some(entry) = strips.iter_mut().find(|entry| entry.window == self.window) else {
                    return;
                };
                // A switch that happened after this task was posted owns the strip now, and its own
                // task will take it away 800 ms after *it* happened. Without this, holding `J` would
                // leave the strip hidden while the tabs were still changing.
                if entry.switch_generation != self.generation {
                    true
                } else {
                    entry.switching = false;
                    false
                }
            };
            if !stale {
                apply_chrome_layout(self.window);
            }
        }
    }
}

/// Put one window's strips where the settings say, and hide the ones the settings say to hide.
///
/// Cheap when nothing moved, which is almost always: it is called from `ipc::set_tabs_for` (every
/// address change) and `ipc::set_mode_for` (every mode change) so that `tabs.show multiple` and
/// `statusbar.show in-mode` are true of the moment rather than of the last `:set`.
pub fn apply_chrome_layout(window: u32) {
    debug_assert_ne!(currently_on(ThreadId::UI), 0);

    let want = wanted(window);
    let (top, bottom, was) = {
        let Ok(mut strips) = STRIPS.lock() else {
            return;
        };
        let Some(entry) = strips.iter_mut().find(|entry| entry.window == window) else {
            return;
        };
        if entry.applied == Some(want) {
            return;
        }
        let was = entry.applied.replace(want);
        (entry.top.clone(), entry.bottom.clone(), was)
    };

    let handle = crate::state::BruState::instance()
        .and_then(|state| state.lock().ok().and_then(|state| state.window_handle(window)));
    let Some(handle) = handle else {
        return;
    };

    // Order first, then visibility: reordering an invisible view is legal and reordering a visible
    // one is one repaint, so doing it in this order costs at most one extra frame of the old
    // arrangement and never a frame of a strip drawn twice.
    let reordered = was.map(|was| (was.tabs_at_top, was.status_at_top))
        != Some((want.tabs_at_top, want.status_at_top));
    if reordered {
        reorder(&handle, &top, &bottom, want);
    }

    View::from(&top).set_visible(i32::from(want.tabs_visible));
    View::from(&bottom).set_visible(i32::from(want.status_visible));
    // Belt and braces, and it is not redundant: `set_visible(0)` is what stops a strip being
    // painted, and `preferred_size` answering 0 is what stops the box layout keeping its 40 px. A
    // `BoxLayout` skips an invisible child, so either alone should do — this way a CEF that changes
    // its mind about one of them cannot leave a blank band where the strip was.
    View::from(&top).invalidate_layout();
    View::from(&bottom).invalidate_layout();
    View::from(&handle).invalidate_layout();

    debug_strips(window, want, reordered);
}

/// `Panel::reorder_child_view` for both strips, which is what makes `tabs.position` live rather than
/// something that needs a new window.
///
/// The leading strips are moved to 0 and 1 in order; the trailing ones are each moved to the last
/// index in turn, so applying them in order leaves them in that order at the end. The pages in
/// between are never touched — a reorder is a remove-and-insert, and everything else slides.
fn reorder(handle: &Window, top: &BrowserView, bottom: &BrowserView, want: Layout) {
    // qutebrowser's own order, `mainwindow.py::_add_widgets`: a status bar asked for the top is
    // inserted at 0, which puts it *above* the tab strip; one asked for the bottom is appended,
    // which puts it below.
    let mut leading: Vec<&BrowserView> = Vec::new();
    let mut trailing: Vec<&BrowserView> = Vec::new();
    if want.status_at_top {
        leading.push(bottom);
    } else {
        trailing.push(bottom);
    }
    if want.tabs_at_top {
        leading.push(top);
    } else {
        trailing.insert(0, top);
    }

    for (index, view) in leading.iter().enumerate() {
        handle.reorder_child_view(Some(&mut View::from(*view)), index as i32);
    }
    let last = handle.child_view_count() as i32 - 1;
    for view in trailing {
        handle.reorder_child_view(Some(&mut View::from(view)), last);
    }
}

// --- src/devtools.rs: the pages panel ------------------------------------------------------------
// The container the tab views live in. Its only job is to have a horizontal layout.
//
// **The size it answers matters and the numbers in it do not.** It has to be non-empty, because CEF
// discards an empty one unread and falls back to the view's current size — the whole of CEF-NOTES
// trap 25. Beyond that it is a basis for flex 1 to grow from, and every real dimension comes from
// the window.
wrap_panel_delegate! {
    struct BruPagesPanelDelegate {}

    impl ViewDelegate {
        fn preferred_size(&self, _view: Option<&mut View>) -> Size {
            Size { width: 1280, height: 300 }
        }
    }

    impl PanelDelegate {}
}
// --- end src/devtools.rs: the pages panel --------------------------------------------------------

// --- src/devtools.rs ----------------------------------------------------------------------------
/// Make the divider strip for `window`, ready to be put above a docked inspector.
///
/// **The one chrome view made after its window.** The other three are built in [`create`] because a
/// window always has them; this one exists only while an inspector is docked, and making it with
/// every window would be a browser per window for a panel most windows never open.
///
/// It is `BruChromeViewDelegate` like the rest — a fixed height and flex 0 is exactly what a strip
/// is — and its `on_browser_created` registers the browser as chrome, which is what stops `j` in the
/// divider being read as a scroll of the page above it (CEF-NOTES trap 11).
///
/// Creates no browser by itself: `add_child_view` does that, synchronously (CEF-NOTES). So this may
/// not be called from inside a query handler — see trap 12 — and `devtools::dock`, its only caller,
/// runs on a CEF callback.
pub fn divider_view(state: &SharedState, window_id: u32) -> Option<BrowserView> {
    let mut settings = BrowserSettings::default();
    if let Some(colour) = crate::chrome::chrome_background() {
        settings.background_color = colour;
    }
    let mut client = state.lock().expect("state mutex poisoned").client();
    let mut delegate =
        BruChromeViewDelegate::new(state.clone(), window_id, DIVIDER_HEIGHT, KIND_DIVIDER);
    browser_view_create(
        client.as_mut(),
        Some(&CefString::from(DIVIDER_URL)),
        Some(&settings),
        None,
        None,
        Some(&mut delegate),
    )
}
// --- end src/devtools.rs ------------------------------------------------------------------------

/// Whether a strip is currently hidden, for the preferred height its delegate answers with.
fn strip_hidden(window: u32, kind: i32) -> bool {
    STRIPS
        .lock()
        .ok()
        .and_then(|strips| {
            strips
                .iter()
                .find(|entry| entry.window == window)
                .and_then(|entry| entry.applied)
                .map(|applied| {
                    // The panel goes with the bar: a completion table over a hidden status line
                    // would be a block floating on the page with nothing under it.
                    if kind == KIND_TABS {
                        !applied.tabs_visible
                    // --- src/devtools.rs ---------------------------------------------------
                    // The divider goes with the inspector and with nothing else. `statusbar.show`
                    // says whether bru draws a status line, which is no statement about a strip
                    // that only exists while a panel is docked — and hiding the divider while the
                    // panel it resizes stayed would leave a panel nothing can move.
                    } else if kind == KIND_DIVIDER {
                        false
                    // --- end src/devtools.rs -----------------------------------------------
                    } else {
                        !applied.status_visible
                    }
                })
        })
        .unwrap_or(false)
}

/// `BRU_DEBUG_STRIPS=1` prints what each window's strips were just put into.
///
/// It is the only way to read the decision without a screenshot, and a screenshot of this is not
/// self-evident: six brus share this compositor, and "the strip is not there" and "the window is not
/// the one in the picture" look the same. Off by default; a line per change, not per layout pass.
fn debug_strips(window: u32, layout: Layout, reordered: bool) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("BRU_DEBUG_STRIPS").is_some()) {
        eprintln!(
            "bru[strips]: window {window} tabs={} at {} status={} at {} reordered={reordered}",
            if layout.tabs_visible { "shown" } else { "hidden" },
            if layout.tabs_at_top { "top" } else { "bottom" },
            if layout.status_visible { "shown" } else { "hidden" },
            if layout.status_at_top { "top" } else { "bottom" },
        );
    }
}

/// Every window's, for a `:set` that changed one of the four.
pub fn apply_chrome_layout_everywhere() {
    let windows: Vec<u32> = match STRIPS.lock() {
        Ok(strips) => strips.iter().map(|entry| entry.window).collect(),
        Err(_) => return,
    };
    for window in windows {
        apply_chrome_layout(window);
    }
}
// --- end tabs and statusbar --------------------------------------------------------------------

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
        // --- src/window.rs: the panel -------------------------------------------------------
        panel_view: RefCell<Option<BrowserView>>,
        // --- end src/window.rs: the panel ---------------------------------------------------
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
            // --- src/window.rs: the panel ---------------------------------------------------
            let panel = self.panel_view.borrow();
            // --- end src/window.rs: the panel -----------------------------------------------
            let (Some(window), Some(top), Some(bottom), Some(panel)) =
                (window, top.as_ref(), bottom.as_ref(), panel.as_ref())
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

            // --- src/window.rs: the panel ---------------------------------------------------
            // **Flex 0 on all three strips, said out loud rather than left to the default.**
            //
            // `tabs.rs` gives every page view flex 1, which is what makes the pages absorb the
            // window. Nothing ever said what the strips' flex was, and while there were two of them
            // — one fixed at 28px and one that asked for its own height — that never showed. Adding
            // a third that asks for **0** did: the panel came up occupying the bottom half of the
            // window with nothing in it, because a leftover being shared out has to land somewhere
            // and a child that has not said `0` is a child that can take some.
            //
            // Set here, on the layout, before any of the three is added: `set_flex_for_view` is the
            // panel's own record of the child and does not need the child to be placed yet.
            if let Some(layout) = layout.as_ref() {
                for view in [top, bottom, panel] {
                    let mut view = View::from(view);
                    layout.set_flex_for_view(Some(&mut view), 0);
                }
            }
            // --- end src/window.rs: the panel -----------------------------------------------

            // --- tabs and statusbar --------------------------------------------------------
            // The two strips are remembered before either is added: `leading_strip_count`, which
            // `tabs::attach_all_in` is about to ask, is a pure function of the settings and does
            // not need them — but `apply_chrome_layout` at the end of this function does.
            remember_strips(self.window_id, top, bottom);
            // Which strips go above the pages, and in what order. `tabs.position` and
            // `statusbar.position` decide it; the default is the arrangement this function had
            // hard-coded — tab strip, pages, status bar.
            let places = places();
            let mut leading: Vec<View> = Vec::new();
            let mut trailing: Vec<View> = Vec::new();
            // --- src/devtools.rs: the pages panel -------------------------------------------
            // **The tab views do not go in the window any more; they go in here.** A vertical box
            // layout can only stack, so an inspector beside the pages needs a container of its own
            // with a horizontal layout in it — and that container has to exist before the first tab
            // is attached, because re-parenting live tab views later is the operation `tabs.rs` has
            // measured as fatal in the neighbouring case.
            //
            // A `Panel` from `panel_create` was once recorded as measuring "0 tall however it is
            // asked". That record was taken while every delegate here answered a zero width, which
            // CEF discards unread (CEF-NOTES trap 25) — so the panel was never being asked at all.
            // Probed again 2026-08-09 with a non-empty size: **1942x300 requested and given**, its
            // inner horizontal layout live, and a browser view inside it laid out. The cost of the
            // old reading was `devtools.position right` being refused for a year.
            let mut pages_delegate = BruPagesPanelDelegate::new();
            let pages = panel_create(Some(&mut pages_delegate));
            let pages_layout = pages.as_ref().and_then(|pages| {
                // STRETCH on the cross axis, or a tab view is laid out at its own preferred height
                // in the middle of the panel — measured in the probe as a child 12px tall in a
                // 300px panel.
                let inner = BoxLayoutSettings {
                    horizontal: 1,
                    cross_axis_alignment: AxisAlignment::STRETCH,
                    ..Default::default()
                };
                pages.set_to_box_layout(Some(&inner))
            });
            // --- end src/devtools.rs: the pages panel ---------------------------------------
            // --- src/window.rs: the panel ---------------------------------------------------
            // **The panel goes between the bar and the pages, on whichever side the bar is.** It
            // opens *towards* the page — that is where the room is — so with the bar at the bottom
            // it is directly above it, and with `statusbar.position top` directly below. Adjacent
            // and on the page side either way, which is the one arrangement that reads as one
            // block growing rather than two strips swapping places.
            if places.status_at_top {
                leading.push(View::from(bottom));
                leading.push(View::from(panel));
            } else {
                trailing.push(View::from(panel));
                trailing.push(View::from(bottom));
            }
            // --- end src/window.rs: the panel -----------------------------------------------
            if places.tabs_at_top {
                leading.push(View::from(top));
            } else {
                trailing.insert(0, View::from(top));
            }
            for mut view in leading {
                window.add_child_view(Some(&mut view));
            }
            // --- end tabs and statusbar ----------------------------------------------------

            // --- src/devtools.rs: the pages panel -------------------------------------------
            // After the leading strips and before the trailing ones, which is exactly where the
            // tab views used to be added one by one.
            if let Some(pages) = pages.as_ref() {
                window.add_child_view(Some(&mut View::from(pages)));
                // **Flex 1, and set after the panel is a child.** Said before it was added — the way
                // the strips say theirs — the layout kept no record of it and the panel came up at
                // its preferred 300 with the rest of the window left empty below the status bar.
                // The strips get away with it because they ask for the height they want; this one
                // has to be told to grow.
                if let Some(layout) = layout.as_ref() {
                    layout.set_flex_for_view(Some(&mut View::from(pages)), 1);
                }
            }
            // --- end src/devtools.rs: the pages panel ---------------------------------------

            // Nothing else is ever handed the window or its layout; keep both where a tab opened
            // later can find them. The lock is let go before any tab is placed — see tabs.rs.
            {
                let mut guard = self.state.lock().expect("state mutex poisoned");
                guard.set_window_for(self.window_id, window.clone(), layout);
                if let Some(pages) = pages.clone() {
                    guard.set_pages_for(self.window_id, pages, pages_layout);
                }
            }

            // Tabs that already exist in *this* window: at startup, the one the command line asked
            // for, created before there was a window to put it in; at runtime, whatever `create`
            // opened before handing the slot over.
            tabs::attach_all_in(&self.state, self.window_id);

            // --- tabs and statusbar --------------------------------------------------------
            for mut view in trailing {
                window.add_child_view(Some(&mut view));
            }
            // --- end tabs and statusbar ----------------------------------------------------

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

            // --- tabs and statusbar --------------------------------------------------------
            // Now that the tabs are in and the window knows its own mode: `tabs.show never` has to
            // hide the strip a window has only just grown, and `tabs.show multiple` has to count.
            apply_chrome_layout(self.window_id);
            // --- end tabs and statusbar ----------------------------------------------------
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
            // --- tabs and statusbar --------------------------------------------------------
            // Before the views are dropped below: the list holds clones of both of them, and a row
            // left behind would keep a window's chrome alive after the window had gone.
            forget_strips(self.window_id);
            // --- end tabs and statusbar ----------------------------------------------------
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
            // --- src/lifetime.rs ------------------------------------------------------------
            // **The window manager's close button reaches an exit through here, and this is the
            // last moment anything is readable.** CEF asks whether the window may close, so
            // nothing has been torn down yet; by `on_before_close` the browsers' navigation lists
            // no longer read back, which is the fact that moved `:quit --save` into the command
            // in the first place. `on_quitting` runs once however many windows pass through.
            crate::lifetime::on_quitting(&self.state);
            // --- end src/lifetime.rs --------------------------------------------------------
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
        // --- src/devtools.rs ---------------------------------------------------------------
        // Which window this tab belongs to. Added for the inspector: the popup hook below is
        // handed the view and nothing else, and a docked inspector has to go into the window its
        // tab is in — with two windows open, "the current one" is a guess.
        window_id: u32,
        // --- end src/devtools.rs -----------------------------------------------------------
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

        // --- src/devtools.rs ---------------------------------------------------------------
        // **Where a docked inspector is placed.** CEF hands over the DevTools browser as a view
        // and asks what to do with it: "Optionally add |popup_browser_view| to the views hierarchy
        // yourself and return true (1). Otherwise return false (0) and a default cef_window_t will
        // be created for the popup."
        //
        // Anything that is not an inspector keeps CEF's own answer. bru has no docked place for a
        // page popup, and returning 1 without adding the view would lose it.
        // **A delegate for the inspector, which is what makes it a panel rather than a guest.**
        // CEF asks the parent view's delegate what delegate the popup should have; without one the
        // popup view has no `preferred_size` to give a BoxLayout and no `on_browser_destroyed` to
        // be taken out on. Both were measured missing before this existed: the inspector laid out
        // at whatever CEF preferred, which on a 1399px page left it **20 pixels**, and closing it
        // killed the process outright — a silent exit, no panic, three times out of three.
        fn delegate_for_popup_browser_view(
            &self,
            _browser_view: Option<&mut BrowserView>,
            _settings: Option<&BrowserSettings>,
            _client: Option<&mut Client>,
            is_devtools: ::std::os::raw::c_int,
        ) -> Option<BrowserViewDelegate> {
            crate::devtools::trace(&format!(
                "delegate_for_popup: entered, is_devtools={is_devtools}"
            ));
            if is_devtools == 0 || !crate::devtools::docking() {
                crate::devtools::trace("delegate_for_popup: declining");
                return None;
            }
            crate::devtools::trace("delegate_for_popup: handing over an inspector delegate");
            Some(BruInspectorViewDelegate::new(self.state.clone(), self.window_id))
        }

        fn on_popup_browser_view_created(
            &self,
            _browser_view: Option<&mut BrowserView>,
            popup_browser_view: Option<&mut BrowserView>,
            is_devtools: ::std::os::raw::c_int,
        ) -> ::std::os::raw::c_int {
            crate::devtools::trace(&format!(
                "on_popup_created (parent delegate): is_devtools={is_devtools}"
            ));
            let Some(view) = popup_browser_view else {
                return 0;
            };
            if is_devtools == 0 {
                return 0;
            }
            let placed = crate::devtools::on_popup_view(&self.state, self.window_id, view);
            crate::devtools::trace(&format!("on_popup_created: placed={placed}"));
            i32::from(placed)
        }
        // --- end src/devtools.rs -----------------------------------------------------------
    }
}

// --- src/devtools.rs ---------------------------------------------------------------------------
// The docked inspector's own delegate: a height, and the one callback that says when to take it out
// of the window. Both are the reason it exists — see `delegate_for_popup_browser_view` above.
wrap_browser_view_delegate! {
    pub struct BruInspectorViewDelegate {
        state: Arc<Mutex<BruState>>,
        window_id: u32,
    }

    impl ViewDelegate {
        fn preferred_size(&self, _view: Option<&mut View>) -> Size {
            // **The width is not ignored, and answering 0 threw the height away with it.**
            // `CefSize::IsEmpty()` is `width <= 0 || height <= 0` (installed distribution,
            // `include/internal/cef_types_wrappers.h:236`), and `CefViewView::CalculatePreferredSize`
            // discards an empty answer and falls back to the view's *current* size. So every height
            // this delegate answered was classified as "no answer" and never read. The strips hold
            // their 40 and 24 through every layout because they answer a nonzero width — 1280, at
            // line 1194 — and that number was the whole difference. Same number here, same reason.
            // Asked of `devtools.rs`, not computed here: which axis this number lands on depends on
            // where the inspector is docked, and while a drag is running it comes from the drag
            // rather than from the setting. Both answers are non-empty on both axes — see trap 25.
            let size = crate::devtools::inspector_size(self.window_id);
            crate::devtools::trace(&format!(
                "inspector preferred_size -> {}x{}",
                size.width, size.height
            ));
            size
        }

        // No `minimum_size`, no `maximum_size`. They were written to pin a height the layout kept
        // losing, and they could never have: `BoxLayout::GetMinimumSizeForView` returns 0 unless
        // the flex specification carries `use_min_size`, which CEF's `SetFlexForView` never passes,
        // and `GetMaximumSize` is not called anywhere in `box_layout.cc`. The preferred size, once
        // it is non-empty, is the only thing that speaks.

        fn on_layout_changed(&self, _view: Option<&mut View>, new_bounds: Option<&Rect>) {
            if let Some(b) = new_bounds {
                crate::devtools::trace(&format!(
                    "inspector laid out at {}x{} @{},{}",
                    b.width, b.height, b.x, b.y
                ));
            }
        }
    }

    impl BrowserViewDelegate {
        // NOTE(probe): `browser_runtime_style` deliberately not answered here — testing H3.

        // **Where a docked inspector is removed, and the only safe place for it.** Taking the view
        // out from `LifeSpanHandler::on_before_close` — or before calling `close_dev_tools` — ended
        // the process both ways, measured 2026-08-08. This callback is CEF saying the browser is
        // gone and the view is now bru's to dispose of.
        fn on_browser_destroyed(
            &self,
            _browser_view: Option<&mut BrowserView>,
            _browser: Option<&mut Browser>,
        ) {
            crate::devtools::trace("inspector on_browser_destroyed");
            crate::devtools::undock(&self.state, self.window_id);
        }
    }
}
// --- end src/devtools.rs -------------------------------------------------------------------------

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
        // --- src/window.rs: the panel ----------------------------------------------------------
        // **Which of the three strips this is**, as [`KIND_TABS`], [`KIND_BAR`] or [`KIND_PANEL`].
        //
        // It was a `grows: bool` — true for the strip that grew with the completion table, false
        // for the tab strip — and one boolean was enough while there were two strips and the one
        // that grew was the one with the bar in it. Splitting the panel out broke that in a way the
        // compiler could not see: `strip_hidden` read the same boolean to decide *which setting*
        // hides the strip, so a bar that no longer grew started answering to `tabs.show`. Three
        // strips need a name each.
        kind: i32,
        // --- end src/window.rs: the panel ------------------------------------------------------
    }

    impl ViewDelegate {
        fn preferred_size(&self, _view: Option<&mut View>) -> Size {
            // Width is ignored: the box layout stretches the strip across the window. Only the
            // height is a real request.
// --- src/completers.rs ---------------------------------------------------------------------
            // This strip's own window, not whichever one is current: a layout pass runs for the
            // window being relayouted, and reading a shared value would make window 0's bar as tall
            // as window 1's table.
            // **One number, and it is the one the page measured.** `chrome/panel.js` reports
            // `prompt.offsetHeight + completion.offsetHeight` — both blocks, after it has drawn them
            // — and that arrives here through `completers::apply_height`. This used to add
            // `prompt_height` on top, a second figure Rust computed from a row count, so the prompt
            // block was counted twice: the view came out as tall as its content plus the prompt
            // again, and the extra showed as a band of empty panel under the key hints.
            //
            // Photographed 2026-08-09 on a `Save file to` prompt: roughly 250 px of content in
            // roughly 490 px of panel. The measurement wins for the reason the strip's own comment
            // already gives — a height reported after the draw cannot disagree with what was drawn,
            // and a height guessed before it can.
            let extra = if self.kind == KIND_PANEL {
                completion_height(self.window_id)
            } else {
                0
            };
            if self.kind == KIND_PANEL {
                debug_bar(self.window_id, self.height + extra);
            }
// --- tabs and statusbar --------------------------------------------------------------------
            // `tabs.show` / `statusbar.show`. `set_visible(0)` is the first half and this is the
            // second: a `BoxLayout` skips an invisible child, so either alone should reclaim the
            // band — asking for nothing as well means a CEF that lays an invisible view out anyway
            // still lays it out zero pixels tall.
            if strip_hidden(self.window_id, self.kind) {
                return Size { width: 1280, height: 0 };
            }
// --- src/devtools.rs -----------------------------------------------------------------------
            // Six pixels of chrome, or the whole area the page and the inspector share — see
            // `devtools::divider_height_for`, which explains why a drag has to surround the pointer
            // rather than chase it.
            if self.kind == KIND_DIVIDER {
                return crate::devtools::divider_size(self.window_id);
            }
// --- end src/devtools.rs -------------------------------------------------------------------
// --- end tabs and statusbar ----------------------------------------------------------------
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
