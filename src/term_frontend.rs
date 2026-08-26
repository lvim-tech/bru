//! C7: where the four halves meet, and where `--term` actually starts a browser.
//!
//! `term_session.rs` owns the terminal, `term_keys.rs` reads what is typed into it, `term_compose.rs`
//! lays the surfaces CEF paints into one picture, and `term_paint.rs` puts that picture on the
//! screen. None of them knows about the others. **This module is the only place that does**, which
//! is why it is the one phase that could not be built in parallel with the rest.
//!
//! ## One `RenderHandler` for every surface, not one each
//!
//! bru's four browsers share a single `Client` (`window.rs`), so they share its render handler too.
//! That is not a compromise: `on_paint` is handed the browser it is painting, and a browser knows
//! its own identifier — so the handler dispatches on that, and there is one place where a surface's
//! pixels are written instead of four that must agree.
//!
//! The awkward moment is creation: CEF asks `view_rect` **while** the browser is being made, before
//! anything can have recorded which surface it is. [`TermState::pending`] answers for exactly that
//! window — the browsers are created one at a time and synchronously, so "the one being made" is a
//! single value and not a guess.
//!
//! ## What the terminal does not have to be told
//!
//! Keys reach the page through `send_key_event`, which funnels into the same
//! `ForwardKeyboardEvent` a real keypress does — so `keys.rs`'s `on_pre_key_event` runs, the tries
//! in `bindings.rs` match, and **every binding bru has works here without knowing it is in a
//! terminal**. The same is true of the smooth scroll: `scroll.rs` synthesises wheel events itself,
//! so `j` is as smooth in a pane as it is in a window.

use cef::*;
// CEF's own rectangle, which the compositor's `Rect` shadows in this file.
use cef::Rect as CefRect;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::term_compose::{Frame, Layer, Layout, LayoutRequest, Rect, Surface, SurfaceKind, layout};
use crate::term_paint::{IMAGE_ID_VIEW, Painter, Placement, place_for_host};
use crate::term_session::{PaneSize, TerminalSession};

/// `--term`: draw this browser into the terminal it was started from.
pub const SWITCH: &str = "--term";

/// Whether this run was asked to be a terminal one.
///
/// Read from the raw argv with `--socket=`'s rule — **only before `--remote`**, because `--remote`
/// takes the rest of the line as its message and a `--term` inside a URL is a URL.
pub fn requested(args: &[String]) -> bool {
    let before = args.iter().position(|arg| arg == "--remote").unwrap_or(args.len());
    args[..before].iter().any(|arg| arg == SWITCH)
}

/// The page `--term` opens with: the first bare argument, as everywhere else in bru.
pub fn url_from(args: &[String]) -> Option<&str> {
    let before = args.iter().position(|arg| arg == "--remote").unwrap_or(args.len());
    // `get(1..before)` rather than `[1..before]`: an argv with nothing before `--remote` — empty,
    // or an argv[0] that *is* `--remote` — makes the range backwards, and indexing a backwards
    // range is a panic where "no page" is the answer.
    args.get(1..before)?
        .iter()
        .find(|arg| !arg.starts_with('-') && !arg.trim().is_empty())
        .map(|url| url.trim())
}

/// Whether the terminal this run needs is actually there.
///
/// **Checked before `initialize`, because the alternative is a browser with nowhere to draw.** A
/// `--term` run whose stdout is a pipe would open no window, paint into a file and read keys from
/// nothing; saying so in one line beats starting Chromium to find out.
pub fn terminal_is_there() -> bool {
    // SAFETY: `isatty` reads a descriptor number and touches nothing.
    unsafe { libc::isatty(libc::STDOUT_FILENO) == 1 }
}

/// Everything the terminal frontend holds, for the one window it has.
struct TermState {
    session: TerminalSession,
    painter: Painter,
    frame: Frame,
    layout: Layout,
    size: PaneSize,
    /// One buffer per surface, in `SurfaceKind` order. `None` until CEF has painted it once.
    surfaces: Vec<Option<Painted>>,
    /// Which surface each browser is. Filled in as each one is created.
    of_browser: Vec<(i32, SurfaceKind)>,
    /// The surface currently being created — see the module header.
    pending: Option<SurfaceKind>,
    /// The popup layer's rectangle in the page's coordinates, while one is showing.
    popup: Option<Rect>,
    /// Where the inspector browser is to be sent once its frontend has been prepared — see
    /// `InspectorLoadHandler`. The id is `0` until the browser exists.
    inspector_target: Option<(i32, String)>,
    /// The popup's own pixels. CEF paints it as a **second surface** with its own buffer and its own
    /// size, which is why it cannot share the page's slot: a `<select>` dropdown is 200x300 over a
    /// page that is 990x1200, and writing one into the other's buffer is not compositing, it is
    /// corruption.
    popup_pixels: Option<Painted>,
    /// The docked inspector's browser, while `:devtools` has one open — a windowless browser on
    /// the `devtoolsFrontendUrl` the DevTools port hands out, composited like any other surface.
    /// `Some` is also what makes [`layout_for`] reserve the inspector's rectangle, read under the
    /// lock the caller already holds for the reason that function's comment gives.
    inspector: Option<i32>,
    /// What an empty part of the pane is painted with — the chrome background, **read once**.
    ///
    /// It used to be asked of the theme inside `present`, which meant `theme.css` read off the disk
    /// and the settings store locked on *every frame*, under this module's own lock, inside a CEF
    /// paint callback — a disk read per keystroke at the pace the command line repaints. The theme
    /// changes when a person changes it; [`refresh_background`] is on that path, and nothing else
    /// moves this.
    background: [u8; 3],
    /// How many bands the terminal is holding pixels for, so that a layout with fewer of them can
    /// take the surplus back.
    bands_drawn: usize,
    /// Every band must be sent on the next frame, whatever the damage says.
    ///
    /// **Set whenever the bands themselves changed.** After [`forget_bands`] the terminal is holding
    /// no picture at all, and a small damage rectangle would put one band back onto an otherwise
    /// empty pane.
    redraw_all: bool,
}

/// A surface's last frame, kept because a repaint of one surface has to redraw all of them.
struct Painted {
    bgra: Vec<u8>,
    width: u32,
    height: u32,
}

static TERM: OnceLock<Mutex<TermState>> = OnceLock::new();

/// Set once the frontend is running, so paths shared with the Views frontend can ask cheaply and
/// without taking the lock.
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Whether this process is drawing into a terminal.
pub fn is_active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// The surfaces the frontend draws, in the order they are created and composed.
///
/// The panel is here even though it is zero-height most of the time: it is where the completion
/// table and the prompt are drawn, and a `:` with no completion under it is half a command line.
/// Its height is whatever the page measured — see [`layout_for`]. The inspector is
/// [`open_inspector`]'s, made when `:devtools` asks; the divider's rectangle exists in the layout
/// but nothing paints it yet — it shows as a band of chrome background between page and panel.
const SURFACES: [SurfaceKind; 6] = [
    SurfaceKind::Page,
    SurfaceKind::Top,
    SurfaceKind::Bottom,
    SurfaceKind::Panel,
    SurfaceKind::Divider,
    SurfaceKind::Inspector,
];

/// The surfaces made when the frontend starts. The other two are made when `:devtools` asks.
const AT_STARTUP: [SurfaceKind; 4] =
    [SurfaceKind::Page, SurfaceKind::Top, SurfaceKind::Bottom, SurfaceKind::Panel];

fn index_of(kind: SurfaceKind) -> usize {
    SURFACES.iter().position(|&k| k == kind).unwrap_or(0)
}

/// Start the terminal frontend. Called from `on_context_initialized` instead of `window::create`.
pub fn start(state: &crate::tabs::SharedState, url: &str) -> Result<(), String> {
    let session = TerminalSession::enter()?;
    let size = session.size();
    // Nothing is docked in a frontend that has not started yet.
    let layout = layout_for(size, false);
    let frame = Frame::new(layout.pane.width, layout.pane.height)
        .ok_or("the pane is too small to draw a browser in")?;
    let placement = Placement::new(IMAGE_ID_VIEW, 1, 1, size.rows, size.cols);
    let painter =
        Painter::new(placement, session.in_tmux(), place_for_host(), session.transport());

    let term = TermState {
        session,
        painter,
        frame,
        layout,
        size,
        surfaces: (0..SURFACES.len()).map(|_| None).collect(),
        of_browser: Vec::new(),
        pending: None,
        popup: None,
        popup_pixels: None,
        inspector_target: None,
        inspector: None,
        background: background_from_theme(),
        redraw_all: true,
        bands_drawn: 0,
    };
    if TERM.set(Mutex::new(term)).is_err() {
        return Err("the terminal frontend is already running".to_string());
    }
    ACTIVE.store(true, Ordering::Relaxed);
    // What the terminal actually agreed to, in the log rather than guessed at later. The mouse in
    // particular has three ways to be wrong and they are indistinguishable from the outside: not
    // forwarded at all, forwarded in cells, forwarded in pixels.
    if crate::term_input::debug() {
        let guard = TERM.get().and_then(|term| term.lock().ok());
        if let Some(guard) = guard {
            eprintln!(
                "bru[term]: pane {}x{} px, {}x{} cells ({}x{} per cell) via {:?}; tmux={}; \
                 kitty-keyboard={:?}; mouse-pixels={}; place={:?}; transport={:?}",
                guard.size.width,
                guard.size.height,
                guard.size.cols,
                guard.size.rows,
                guard.size.cell_width,
                guard.size.cell_height,
                guard.size.source,
                guard.session.in_tmux(),
                guard.session.kitty_flags_before(),
                guard.session.mouse_in_pixels(),
                place_for_host(),
                guard.painter.transport(),
            );
        }
    }

    // **A window slot, even with no window in it.** Everything in `state.rs` is keyed by one —
    // which mode keys are in, which browsers are chrome, which tab is showing — and none of that is
    // about a `Window` handle. The slot is the window; the handle was only ever how Views drew it.
    state.lock().expect("state mutex poisoned").open_window_slot();

    for kind in AT_STARTUP {
        create_surface(state, kind, url)?;
    }
    // **Whatever was typed while the terminal was being asked questions.** `enter` reads the
    // replies to its own queries off the same stdin the keyboard uses, and anything that was not a
    // reply is kept rather than thrown away. Handing it to the input loop before that loop starts
    // is what keeps a keystroke made during startup from being swallowed.
    let pushback = TERM
        .get()
        .and_then(|term| term.lock().ok().map(|mut guard| guard.session.take_pushback()))
        .unwrap_or_default();
    crate::term_input::start_with(pushback);
    watch_size();
    install_interrupt();
    Ok(())
}

/// Notice the pane changing size, on the thread CEF requires.
///
/// **The receiver is drained on a thread and acted on from a posted task.** A resize arrives from a
/// signal handler's pipe or from an in-band report, neither of which is the UI thread, and
/// `was_resized` is not a call that may be made from anywhere else.
fn watch_size() {
    let events = TERM
        .get()
        .and_then(|term| term.lock().ok().and_then(|mut term| term.session.resize_events()));
    let Some(events) = events else {
        return;
    };
    let _ = std::thread::Builder::new().name("bru-term-size".to_string()).spawn(move || {
        for size in events {
            let mut task = ResizeTask::new(size);
            post_task(ThreadId::UI, Some(&mut task));
        }
    });
}

/// An in-band resize report (mode 2048) the input thread pulled off stdin.
///
/// **Routed through the session's channel rather than acted on here**, because that channel is
/// where `SIGWINCH`'s reports already arrive and where the two are deduplicated against each
/// other: a real resize under a terminal that speaks mode 2048 produces both, and the compositor
/// must recompute a layout once per size, not once per notification. The mode was being switched
/// on and its reports dropped as ignored sequences — a mode turned on for nothing, and the pixel
/// numbers it carries (which the signal's `TIOCGWINSZ` round trip can lag behind mid-drag) thrown
/// away with it.
pub fn note_resize_report(rows: u16, cols: u16, width: u32, height: u32) {
    let Some(term) = TERM.get() else {
        return;
    };
    let Ok(guard) = term.lock() else {
        return;
    };
    guard.session.note_in_band_resize(rows, cols, width, height);
}

/// Stop the browser, without touching a screen that has already been given back.
///
/// **Not the spike's `quit_soon`, and the difference is one escape.** That one deletes the images
/// and clears the screen on its way out, which is right while a picture is on it — and wrong after
/// a failure, where the last thing written is the reason and clearing wipes it. Measured
/// 2026-08-25: the message survived the leave sequence and was then erased by the quit.
pub fn quit_soon() {
    let mut task = QuitTask::new();
    post_task(ThreadId::UI, Some(&mut task));
}

wrap_task! {
    struct QuitTask;

    impl Task {
        fn execute(&self) {
            quit_message_loop();
        }
    }
}

wrap_task! {
    struct ResizeTask {
        size: PaneSize,
    }

    impl Task {
        fn execute(&self) {
            resized(self.size);
        }
    }
}

/// Send every diagnostic somewhere other than the picture.
///
/// **Called before `initialize`, and the first version was not.** Redirecting stderr from
/// `on_context_initialized` moves bru's own `eprintln!` and nothing else: by then CEF is up and
/// Chromium's logging already holds the descriptor it was given. Measured 2026-08-25 on vesti.bg —
/// `ERROR:net/socket/ssl_client_socket_impl.cc:964 handshake failed` painted across the page, from
/// a logger bru never calls. The redirect has to happen before the library that inherits it starts.
///
///
/// **Every `eprintln!` in bru lands on top of the page.** Measured 2026-08-25 with abv.bg drawing
/// correctly in the pane: `bru[adblock]: blocked 0 of 1 requests` and `bru: hint: this tab is in no
/// window` were painted across the middle of it, because stderr is the same terminal the image is
/// in. In a window they go to whatever shell started bru and bother nobody; here they are graffiti
/// on the one surface that matters.
///
/// They are not silenced, they are moved: `$XDG_RUNTIME_DIR/bru/term.log`, truncated per run, so
/// `tail -f` in another pane is the same diagnostic it always was. If the file cannot be opened the
/// messages stay where they are — a browser that refused to start because it could not open a log
/// would be worse than one that draws over itself.
pub fn divert_diagnostics() {
    let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") else {
        return;
    };
    let path = std::path::PathBuf::from(dir).join("bru");
    if std::fs::create_dir_all(&path).is_err() {
        return;
    }
    let path = path.join("term.log");
    let Ok(file) = std::fs::File::create(&path) else {
        return;
    };
    use std::os::fd::AsRawFd;
    // SAFETY: `dup2` takes two descriptor numbers. `file` is open for the duration of the call, and
    // stderr is a descriptor this process owns. Leaking the `File` afterwards is deliberate: the
    // duplicate must outlive it, and closing it here would close the log out from under stderr.
    unsafe {
        libc::dup2(file.as_raw_fd(), libc::STDERR_FILENO);
    }
    std::mem::forget(file);
    eprintln!("bru: --term: diagnostics go here, because the terminal is holding the page");
}

/// The layout for a pane of this size, at this scale.
///
/// **`inspector` is a parameter and not a question this asks**, and that is not style. It used to
/// call `inspector_open()`, which takes the state lock this module keeps — and both callers already
/// hold it. `std::sync::Mutex` is not reentrant, so the second lock never returned: `:` opened the
/// command line, the completion's new height reached `relayout`, and the browser stopped. Measured
/// 2026-08-26. Every value this needs is now handed to it by someone holding the lock already.
fn layout_for(size: PaneSize, inspector: bool) -> Layout {
    layout(&LayoutRequest {
        // The session measures in `u32` because a pixel count cannot be negative; the compositor
        // works in `i32` because a rectangle's corner can be. `try_into` rather than `as`: a pane
        // wider than `i32::MAX` is not a pane, and silently wrapping it would lay out a frame at a
        // negative width.
        width: i32::try_from(size.width).unwrap_or(i32::MAX),
        height: i32::try_from(size.height).unwrap_or(i32::MAX),
        // One device pixel per logical pixel. The terminal has no scale of its own to inherit, and
        // `get_screen_info` below reports the same number — the two must agree or CEF and the
        // compositor disagree about what a pixel is.
        scale: 1.0,
        top_visible: true,
        bottom_visible: true,
        // **The page's own measurement, not a number computed here.** `chrome/panel.js` reports
        // `prompt.offsetHeight + completion.offsetHeight` after it has drawn both, and that is what
        // sizes the panel in a window too (`window.rs`'s `preferred_size`). A height reported after
        // the draw cannot disagree with what was drawn; one guessed before it can.
        panel_height: crate::window::completion_height(0),
        // The same number the Views delegate answers with, so a height dragged in one frontend
        // means the same thing in the other.
        // **The same numbers the Views delegate answers with**, so a size set in one frontend
        // means the same thing in the other: `inspector_size` gives a height for a bottom dock and
        // a width for a side one, and `place_of` is what says which.
        inspector_height: if inspector {
            let size = crate::devtools::inspector_size(0);
            if crate::devtools::place_of(0).is_side() { size.width } else { size.height }
        } else {
            0
        },
        inspector_side: inspector && crate::devtools::place_of(0).is_side(),
    })
}

/// Lay the surfaces out again at the same pane size, because something above changed height.
///
/// **The panel is the only thing that does this**, and it does it often: every keystroke in the
/// command line can grow or shrink the completion table under it. In a window CEF notices, because
/// the strip answers a new `preferred_size` and the box layout runs; here nothing notices unless it
/// is told.
pub fn relayout() {
    let Some(term) = TERM.get() else {
        return;
    };
    let browsers = {
        let Ok(mut guard) = term.lock() else {
            return;
        };
        let next = layout_for(guard.size, inspector_docked(&guard));
        if crate::term_input::debug() {
            let (was, now) =
                (guard.layout.rect_of(SurfaceKind::Inspector), next.rect_of(SurfaceKind::Inspector));
            eprintln!(
                "bru[term]: relayout: place={:?} docked={} inspector {}x{} at {},{} -> {}x{} at \
                 {},{} (page {}x{} -> {}x{})",
                crate::devtools::place_of(0),
                inspector_docked(&guard),
                was.width,
                was.height,
                was.x,
                was.y,
                now.width,
                now.height,
                now.x,
                now.y,
                guard.layout.rect_of(SurfaceKind::Page).width,
                guard.layout.rect_of(SurfaceKind::Page).height,
                next.rect_of(SurfaceKind::Page).width,
                next.rect_of(SurfaceKind::Page).height,
            );
        }
        if next.rect_of(SurfaceKind::Panel) == guard.layout.rect_of(SurfaceKind::Panel)
            && next.rect_of(SurfaceKind::Inspector) == guard.layout.rect_of(SurfaceKind::Inspector)
        {
            return;
        }
        // **Only the surfaces whose rectangle actually moved.** `was_resized` is not a repaint
        // request, it is a size change: Chromium re-lays out the whole document behind it. Telling
        // every browser meant re-laying out the *page* — a real one, with hundreds of elements —
        // on every keystroke in the command line, because every keystroke changes the completion
        // table's height. Measured 2026-08-25: typing in `:` stopped being typing.
        let moved: Vec<(i32, SurfaceKind)> = guard
            .of_browser
            .iter()
            .filter(|(_, kind)| next.rect_of(*kind) != guard.layout.rect_of(*kind))
            .copied()
            .collect();
        // **The last frame is kept, even though its shape is now wrong.** Dropping it was the
        // tidy-looking choice and it is what made the page vanish: nothing composites a surface
        // that has no picture, the frame is wiped to the background, and a windowless browser does
        // not paint again until something changes it — so the page came back only when it was
        // scrolled. Measured 2026-08-26, along with the blinking that is the same thing at speed.
        // A frame of the wrong size is clipped to its rectangle and looks like a moment of
        // stretching; a missing one looks like a browser that has crashed.
        guard.layout = next;
        forget_bands(&mut guard);
        moved
    };
    resize_browsers(&browsers);
}

/// Tell every surface's browser that its rectangle moved.
fn resize_browsers(browsers: &[(i32, SurfaceKind)]) {
    let Some(state) = crate::state::BruState::instance() else {
        return;
    };
    for (identifier, _) in browsers {
        let browser = state.lock().expect("state mutex poisoned").browser_with_id(*identifier);
        if let Some(host) = browser.and_then(|browser| browser.host()) {
            host.was_resized();
            // **And a frame, now.** `was_resized` tells a browser its size changed; whether that
            // produces a paint is Chromium's business and its timing is not this module's to
            // assume. Asking outright is what keeps the wrong-shaped frame that is being shown
            // meanwhile from being shown for long.
            host.invalidate(PaintElementType::VIEW);
        }
    }
    // **And once more, after the renderer has had time to lay the new size out.** The invalidate
    // above answers with whatever the renderer has *committed*, and right after `was_resized`
    // that is the old layout — for the panel growing from its zero-height rectangle, a document
    // laid out one pixel tall, which is a tall panel with nothing in it. Reported 2026-08-26:
    // the completion opened empty and filled in only when the next keystroke forced a fresh
    // paint. Nothing here can be told when the renderer's own commit lands, so the frame is asked
    // for again when it plausibly has — twice, cheap insurance against a slow layout — and a
    // surface that was already right repaints identically for the cost of one frame.
    let identifiers: Vec<i32> = browsers.iter().map(|(identifier, _)| *identifier).collect();
    for delay_ms in [50i64, 250] {
        let mut task = SettleTask::new(identifiers.clone());
        post_delayed_task(ThreadId::UI, Some(&mut task), delay_ms);
    }
}

wrap_task! {
    struct SettleTask {
        identifiers: Vec<i32>,
    }

    impl Task {
        fn execute(&self) {
            let Some(state) = crate::state::BruState::instance() else {
                return;
            };
            for identifier in &self.identifiers {
                let browser =
                    state.lock().expect("state mutex poisoned").browser_with_id(*identifier);
                if let Some(host) = browser.and_then(|browser| browser.host()) {
                    host.invalidate(PaintElementType::VIEW);
                }
            }
        }
    }
}

/// Make one windowless browser for one surface.
fn create_surface(
    state: &crate::tabs::SharedState,
    kind: SurfaceKind,
    url: &str,
) -> Result<(), String> {
    let page_url = match kind {
        SurfaceKind::Page => url.to_string(),
        SurfaceKind::Top => "bru://chrome/top.html".to_string(),
        SurfaceKind::Bottom => "bru://chrome/bottom.html".to_string(),
        SurfaceKind::Panel => "bru://chrome/panel.html".to_string(),
        SurfaceKind::Divider => "bru://chrome/divider.html".to_string(),
        other => return Err(format!("no terminal surface for {other:?}")),
    };
    if let Some(term) = TERM.get() {
        term.lock().expect("terminal state poisoned").pending = Some(kind);
    }

    let window_info = WindowInfo::default().set_as_windowless(0);
    let settings = BrowserSettings {
        // The probe measured this terminal drawing at 78 fps; CEF's default is 30. Asking for 60 is
        // asking for what the pane can take, and CEF treats it as a ceiling rather than a promise.
        windowless_frame_rate: 60,
        ..Default::default()
    };
    let mut client = crate::keys::BruClient::new(state.clone());
    let browser = browser_host_create_browser_sync(
        Some(&window_info),
        Some(&mut client),
        Some(&CefString::from(page_url.as_str())),
        Some(&settings),
        None,
        None,
    )
    .ok_or_else(|| format!("CEF would not make a windowless browser for {kind:?}"))?;

    let identifier = browser.identifier();
    if let Some(term) = TERM.get() {
        let mut term = term.lock().expect("terminal state poisoned");
        term.of_browser.push((identifier, kind));
        term.pending = None;
    }
    if kind == SurfaceKind::Page {
        crate::term_input::aim_at(identifier);
        // **A windowless browser is not focused by anything, because there is no window manager
        // over it.** Without this the page receives keys and does nothing with the ones that are
        // text: Chromium routes typing to the focused frame, and a browser nobody focused has none.
        if let Some(host) = browser.host() {
            host.set_focus(1);
        }
        // **A tab, or the page is a browser nothing in bru knows about.** `hints.rs` answers "this
        // tab is in no window", the strip has nothing to draw and the status bar has no url —
        // measured 2026-08-25, with the page rendering perfectly and every one of those true at
        // once.
        state.lock().expect("state mutex poisoned").push_term_tab_in(0, identifier);
    } else {
        // The chrome strips have to be known as chrome, or a `j` typed into the command line would
        // be read as a page movement — `state.rs`'s `chrome_browsers`, and the reason it exists.
        state
            .lock()
            .expect("state mutex poisoned")
            .note_chrome_browser(0, identifier);
    }
    Ok(())
}

/// Make a page browser for a new tab, and show it.
///
/// **A terminal window has one page rectangle, so the tab that is showing is the browser mapped to
/// it.** There is no panel to add a second view to and no visibility to toggle: switching tabs is
/// changing which browser the compositor reads the page surface from, and the others go on living
/// with their paints dropped. That is the whole of tab switching here, and it is why
/// `TabSurface::Term` holds nothing.
pub fn new_page(state: &crate::tabs::SharedState, url: &str) -> Option<i32> {
    let window_info = WindowInfo::default().set_as_windowless(0);
    let settings = BrowserSettings { windowless_frame_rate: 60, ..Default::default() };
    let mut client = crate::keys::BruClient::new(state.clone());
    {
        let term = TERM.get()?;
        term.lock().ok()?.pending = Some(SurfaceKind::Page);
    }
    let browser = browser_host_create_browser_sync(
        Some(&window_info),
        Some(&mut client),
        Some(&CefString::from(url)),
        Some(&settings),
        None,
        None,
    );
    let term = TERM.get()?;
    let mut guard = term.lock().ok()?;
    guard.pending = None;
    let browser = browser?;
    let identifier = browser.identifier();
    guard.of_browser.push((identifier, SurfaceKind::Page));
    drop(guard);
    if let Some(host) = browser.host() {
        host.set_focus(1);
    }
    show_page(identifier);
    Some(identifier)
}

/// Close one page browser and forget the surface it was painting into.
///
/// The browser is asked to close rather than dropped: a windowless browser has no view holding the
/// last reference, so `close_browser` is the only thing that ends it. `force` is `1` because the
/// page has already left bru's bookkeeping and a `beforeunload` prompt would have nowhere to be
/// answered — the same call `:quit` makes, for the same reason.
pub fn close_page(identifier: i32) {
    if let Some(term) = TERM.get() {
        if let Ok(mut guard) = term.lock() {
            // The page slot's picture goes only if this browser was the one painting it.
            // `of_browser` holds exactly one page entry — the tab that is showing — so closing a
            // background tab must not blank the surface the visible one is drawn from.
            let was_showing = guard
                .of_browser
                .iter()
                .any(|(id, kind)| *id == identifier && *kind == SurfaceKind::Page);
            guard.of_browser.retain(|(id, _)| *id != identifier);
            if was_showing {
                guard.surfaces[index_of(SurfaceKind::Page)] = None;
            }
        }
    }
    let Some(state) = crate::state::BruState::instance() else {
        return;
    };
    let browser = state.lock().expect("state mutex poisoned").browser_with_id(identifier);
    if let Some(host) = browser.and_then(|browser| browser.host()) {
        host.close_browser(1);
    }
}

/// Show the page of a browser that already exists — the terminal's whole notion of "select a tab".
pub fn show_page(identifier: i32) {
    let Some(term) = TERM.get() else {
        return;
    };
    let Ok(mut guard) = term.lock() else {
        return;
    };
    // Exactly one browser is the page at a time.
    let leaving: Vec<i32> = guard
        .of_browser
        .iter()
        .filter(|(id, kind)| *kind == SurfaceKind::Page && *id != identifier)
        .map(|(id, _)| *id)
        .collect();
    guard.of_browser.retain(|(id, kind)| *kind != SurfaceKind::Page || *id == identifier);
    if !guard.of_browser.iter().any(|(id, _)| *id == identifier) {
        guard.of_browser.push((identifier, SurfaceKind::Page));
    }
    guard.surfaces[index_of(SurfaceKind::Page)] = None;
    drop(guard);
    crate::term_input::aim_at(identifier);

    // **A windowless browser that has not changed does not paint, and switching tabs changes
    // nothing about the page being switched to.** Measured 2026-08-25: the tab strip and the status
    // bar updated, the keyboard followed, and the pane went on showing the tab that had just been
    // left — because the only thing that had happened to the incoming browser was that bru started
    // reading it again. `was_hidden` is the pair CEF offers for exactly this, and `invalidate` is
    // what asks for the frame that makes the switch visible.
    let Some(state) = crate::state::BruState::instance() else {
        return;
    };
    for id in leaving {
        let browser = state.lock().expect("state mutex poisoned").browser_with_id(id);
        if let Some(host) = browser.and_then(|browser| browser.host()) {
            // A background tab that is told it is hidden stops painting frames nobody composites —
            // which is the CPU a terminal frontend would otherwise spend on tabs that are not shown.
            host.was_hidden(1);
        }
    }
    let browser = state.lock().expect("state mutex poisoned").browser_with_id(identifier);
    if let Some(host) = browser.and_then(|browser| browser.host()) {
        host.was_hidden(0);
        host.was_resized();
        host.invalidate(PaintElementType::VIEW);
        host.set_focus(1);
    }
}

/// Show the pointer shape the page is asking for.
///
/// **kitty takes a CSS pointer name over OSC 22**, which is the only way a program in a terminal can
/// say anything about the mouse pointer — it is drawn by the terminal and bru never touches it.
/// Without this the pointer stays an I-beam over every link, which is the one piece of feedback a
/// person uses to tell a link from text.
///
/// Wrapped for tmux like every other escape meant for the terminal underneath: tmux has no idea what
/// OSC 22 is and would eat it.
pub fn set_pointer_shape(cursor: CursorType) {
    if !is_active() {
        return;
    }
    // CEF's `cef_cursor_type_t` against the CSS names kitty answers to. Only the shapes a page
    // actually asks for are named; everything else is the default, because a pointer that guessed
    // would be a pointer that lies about what is under it.
    let shape = match cursor.get_raw() {
        x if x == CursorType::HAND.get_raw() => "pointer",
        x if x == CursorType::IBEAM.get_raw() => "text",
        x if x == CursorType::WAIT.get_raw() => "wait",
        x if x == CursorType::CROSS.get_raw() => "crosshair",
        x if x == CursorType::HELP.get_raw() => "help",
        x if x == CursorType::EASTWESTRESIZE.get_raw()
            || x == CursorType::EASTRESIZE.get_raw()
            || x == CursorType::WESTRESIZE.get_raw() =>
        {
            "ew-resize"
        }
        x if x == CursorType::NORTHSOUTHRESIZE.get_raw()
            || x == CursorType::NORTHRESIZE.get_raw()
            || x == CursorType::SOUTHRESIZE.get_raw() =>
        {
            "ns-resize"
        }
        _ => "default",
    };
    if LAST_SHAPE.swap(shape.as_ptr() as usize, Ordering::Relaxed) == shape.as_ptr() as usize {
        // The same shape twice is the same shape. A page reports a cursor per mouse move, and an
        // escape per move would be a write to the terminal for every pixel of pointer travel.
        return;
    }
    let payload = format!("\x1b]22;{shape}\x1b\\");
    let mut out = std::io::stdout();
    let _ = out.write_all(crate::term_session::passthrough(&payload).as_bytes());
    let _ = out.flush();
}

/// The last shape sent, by the address of its `&'static str` — the names are compile-time constants,
/// so comparing pointers compares the names without a lock or an allocation.
static LAST_SHAPE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// How many size mismatches have been reported. Bounded for the reason every counter here is.
static MISMATCH: AtomicI32 = AtomicI32::new(0);
/// How many frames have been drawn, for the timing line in [`present`].
static PRESENTS: AtomicI32 = AtomicI32::new(0);

/// How many pointer routings have been reported. Bounded like every counter here.
static POINTED: AtomicI32 = AtomicI32::new(0);

/// What an empty part of the pane is painted with.
///
/// The chrome's own background, so a gap where a surface has not painted yet reads as part of the
/// browser rather than as a hole in it. Black if the theme has no answer — the one colour that is
/// never mistaken for content. Called at startup and on a theme change, never per frame — see
/// [`TermState::background`].
fn background_from_theme() -> [u8; 3] {
    match crate::chrome::chrome_background() {
        // The chrome colour is `0xAARRGGBB` and opaque by construction; the alpha is dropped here
        // for the same reason the compositor drops it — the terminal shows what is under nothing.
        Some(colour) => [(colour >> 16) as u8, (colour >> 8) as u8, colour as u8],
        None => [0, 0, 0],
    }
}

/// The theme changed: read the chrome background again and keep it for the frames to come.
///
/// Called from `ipc::reapply_theme_everywhere`, which is the one place a theme change funnels
/// through — `:colorscheme`, the inotify watch and `--reload` all end up there. The theme is read
/// **before** the lock is taken, because reading it is a disk read and a settings-store lock, and
/// neither belongs inside this module's mutex.
pub fn refresh_background() {
    if !is_active() {
        return;
    }
    let fresh = background_from_theme();
    if let Some(term) = TERM.get() {
        if let Ok(mut guard) = term.lock() {
            guard.background = fresh;
        }
    }
}

/// Where the page sits in the pane, in device pixels.
///
/// The terminal reports a click in the pane's coordinates; the page is a rectangle inside it, under
/// the tab strip. Without this a click near the top of the page would land in the strip's rows and
/// the page would be told about a press somewhere above its own first row.
/// How a mouse report's coordinates turn into pixels in the pane.
///
/// `(1, 1)` when the terminal reports pixels; the cell size when it reports cells. Multiplying by
/// the cell puts the pointer at that cell's top-left corner, which is the best a cell-resolution
/// report can say and is what every terminal application does with one.
pub fn mouse_scale() -> (i32, i32) {
    let Some(term) = TERM.get() else {
        return (1, 1);
    };
    let Ok(guard) = term.lock() else {
        return (1, 1);
    };
    if guard.session.mouse_in_pixels() {
        (1, 1)
    } else {
        (
            i32::try_from(guard.size.cell_width).unwrap_or(1).max(1),
            i32::try_from(guard.size.cell_height).unwrap_or(1).max(1),
        )
    }
}

pub fn page_rect() -> Option<Rect> {
    let term = TERM.get()?;
    let guard = term.lock().ok()?;
    Some(guard.layout.rect_of(SurfaceKind::Page))
}

/// Which surface a browser paints into.
fn surface_of(term: &TermState, browser: Option<&mut Browser>) -> Option<SurfaceKind> {
    let identifier = browser.map(|browser| browser.identifier());
    match identifier {
        Some(id) => term
            .of_browser
            .iter()
            .find(|(known, _)| *known == id)
            .map(|(_, kind)| *kind)
            .or(term.pending),
        None => term.pending,
    }
}

/// Whether a present is already on its way to the UI queue. See [`schedule_present`].
static PRESENT_PENDING: AtomicBool = AtomicBool::new(false);

/// Ask for one present, however many paints ask.
///
/// **One frame per batch of paints, not one per surface — reported 2026-08-26 as flickering.**
/// With the completion open, a single keystroke repaints the panel, the page and the bottom strip,
/// and presenting from inside each `on_paint` put three full frames on the terminal per key — the
/// middle ones with a tall panel against a page still painted at its old height, which is a band
/// of background that comes and goes at typing speed. CEF delivers those paints in the same pump
/// of the UI loop; a task posted by the first of them runs after the rest have stored their
/// pixels, so the frame that is transmitted is the one where the surfaces agree. It is also a
/// third of the composites and transmits per keystroke.
///
/// The flag is cleared *before* the present, so a paint that lands mid-present schedules the next
/// one instead of being folded into a picture that no longer holds it.
fn schedule_present() {
    if PRESENT_PENDING.swap(true, Ordering::AcqRel) {
        return;
    }
    let mut task = PresentTask::new();
    post_task(ThreadId::UI, Some(&mut task));
}

wrap_task! {
    struct PresentTask;

    impl Task {
        fn execute(&self) {
            PRESENT_PENDING.store(false, Ordering::Release);
            let Some(term) = TERM.get() else {
                return;
            };
            let Ok(mut guard) = term.lock() else {
                return;
            };
            present(&mut guard);
        }
    }
}

/// Draw everything that has been painted at least once.
///
/// **Every surface, on every frame, and not only the one that changed.** The picture is one image
/// as far as the terminal is concerned; a partial redraw would need the transport to patch a
/// sub-rectangle, and kitty has no such thing under `a=t` (checked against the protocol, not
/// assumed). At 78 fps for a full pane over shared memory there is nothing to buy by trying.
fn present(term: &mut TermState) {
    let mut layers: Vec<Layer<'_>> = Vec::new();
    let held: Vec<(SurfaceKind, &Painted)> = SURFACES
        .iter()
        .filter_map(|&kind| term.surfaces[index_of(kind)].as_ref().map(|painted| (kind, painted)))
        .collect();
    for (kind, painted) in &held {
        let (Ok(width), Ok(height)) =
            (i32::try_from(painted.width), i32::try_from(painted.height))
        else {
            continue;
        };
        let Some(surface) = Surface::new(&painted.bgra, width, height) else {
            continue;
        };
        let rect = term.layout.rect_of(*kind);
        // **A surface painted at a size that is not its rectangle is drawn clipped or small**, and
        // that is what a stale frame looks like on screen. It is legal for one frame after a
        // resize and a bug if it lasts; saying so is the difference between the two.
        if (painted.width as i32 != rect.width || painted.height as i32 != rect.height)
            && crate::term_input::debug()
        {
            let seen = MISMATCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if seen < 8 {
                eprintln!(
                    "bru[term]: {kind:?} painted {}x{} into a {}x{} rectangle at {},{}",
                    painted.width, painted.height, rect.width, rect.height, rect.x, rect.y
                );
            }
        }
        layers.push(Layer {
            surface,
            x: rect.x,
            y: rect.y,
            clip: rect,
            dirty: None,
        });
    }
    if layers.is_empty() {
        return;
    }
    // **The frame is wiped before the surfaces go back into it.** It is kept across frames, so a
    // surface that shrank — or one whose last picture was dropped because its rectangle moved —
    // leaves its old pixels exactly where they were. Measured 2026-08-26: the completion table drew
    // over itself three times as it grew, each earlier and shorter render still legible under the
    // current one. Filling costs one pass over 4 MB against a frame that already costs a composite
    // and a transmit, and it is the difference between a picture and a palimpsest.
    term.frame.fill(term.layout.pane, term.background);
    // **The popup goes on last, because it goes on top.** `compose` treats the order of the layers
    // as the z-order, and a `<select>` dropdown that drew under its own page would be a menu you
    // could see the edge of and never read.
    let popup = term.popup.zip(term.popup_pixels.as_ref());
    let popup_layer = popup.and_then(|(rect, painted)| {
        let (Ok(width), Ok(height)) =
            (i32::try_from(painted.width), i32::try_from(painted.height))
        else {
            return None;
        };
        let surface = Surface::new(&painted.bgra, width, height)?;
        Some(crate::term_compose::popup_layer(surface, rect, term.layout.rect_of(SurfaceKind::Page)))
    });
    if let Some(layer) = popup_layer {
        layers.push(layer);
    }
    let started = crate::term_input::debug().then(std::time::Instant::now);
    let damaged = crate::term_compose::compose(&mut term.frame, &layers);
    if damaged.is_none() && !term.redraw_all {
        return;
    }
    let composed = started.map(|at| at.elapsed());
    let mut out = std::io::stdout();
    let _ = term.session.begin_sync(&mut out);
    // **Only the bands the damage touched.** The picture is one image per band, at a fixed place
    // and with a fixed id, so re-sending one says nothing about the others. A keystroke in the
    // command line changes the bottom row and costs that row — measured 2026-08-27, a full pane
    // costs zellij enough per frame to fall behind a typist, and a small pane does not.
    let damaged =
        if term.redraw_all { term.layout.pane } else { damaged.unwrap_or(term.layout.pane) };
    term.redraw_all = false;
    let width = term.frame.width();
    let stride = (width as usize) * 3;
    let cols = term.size.cols;
    let bands = crate::term_compose::bands(&term.layout, term.size.cell_height as i32);
    for (index, band) in bands.iter().enumerate() {
        if band.height <= 0 || band.y + band.height <= damaged.y || band.y >= damaged.y + damaged.height
        {
            continue;
        }
        let (Ok(row), Ok(rows), Ok(id)) = (
            u16::try_from(band.row + 1),
            u16::try_from(band.rows),
            u32::try_from(index).map(|index| crate::term_paint::IMAGE_ID_BAND + index),
        ) else {
            continue;
        };
        let (start, end) = ((band.y as usize) * stride, ((band.y + band.height) as usize) * stride);
        let Some(pixels) = term.frame.rgb().get(start..end) else {
            continue;
        };
        let placement = Placement::new(id, row, 1, rows, cols);
        let _ = term.painter.paint_at(&mut out, pixels, width as u32, band.height as u32, &placement);
    }
    // The bands that existed last time and do not now — the layout left fewer of them. Taken back
    // here, inside the synchronised update, so nothing is ever seen missing.
    for index in bands.len()..term.bands_drawn {
        if let Ok(index) = u32::try_from(index) {
            let _ = term.painter.forget(&mut out, crate::term_paint::IMAGE_ID_BAND + index);
        }
    }
    term.bands_drawn = bands.len();
    let _ = term.session.end_sync(&mut out);
    let _ = out.flush();
    // **Where a frame's time actually goes, split at the one seam that matters.** Composing is
    // bru's own cost and is the same everywhere; the write is the terminal's, and a host that
    // decodes each frame and re-encodes it for the terminal underneath charges for it here. Every
    // thirtieth frame, because a line per frame is its own delay.
    if let (Some(at), Some(composed)) = (started, composed) {
        let seen = PRESENTS.fetch_add(1, Ordering::Relaxed);
        if seen % 30 == 0 {
            eprintln!(
                "bru[term]: present {seen}: composed in {}ms, written in {}ms ({:?})",
                composed.as_millis(),
                at.elapsed().saturating_sub(composed).as_millis(),
                term.painter.transport()
            );
        }
    }
}

/// Say that the cuts have moved, so every band goes out again on the next frame.
///
/// **It does not delete anything, and that is the point.** Deleting the images and letting the next
/// frame put them back leaves the pane blank in between — and `relayout` runs on *every keystroke*
/// in the command line, because the completion table changes height under it. Measured 2026-08-27:
/// typing made the whole pane blink. Re-transmitting a band's id replaces it in place with nothing
/// blank in between, so a moved band needs no deletion; only a band that has stopped existing does,
/// and [`present`] takes those back itself.
fn forget_bands(term: &mut TermState) {
    term.redraw_all = true;
}

/// A new pane size: re-lay everything out and tell every browser.
pub fn resized(size: PaneSize) {
    let Some(term) = TERM.get() else {
        return;
    };
    let browsers = {
        let mut term = term.lock().expect("terminal state poisoned");
        if !crate::term_session::size_changed(Some(term.size), size) {
            return;
        }
        term.size = size;
        let docked = inspector_docked(&term);
        term.layout = layout_for(size, docked);
        forget_bands(&mut term);
        match Frame::new(term.layout.pane.width, term.layout.pane.height) {
            // A frame is remade rather than reinterpreted: a new stride over old bytes is a torn
            // copy of the last picture, which looks like a rendering bug and is not one.
            Some(frame) => term.frame = frame,
            None => return,
        }
        term.painter
            .set_placement(Placement::new(IMAGE_ID_VIEW, 1, 1, size.rows, size.cols));
        term.of_browser.clone()
    };
    resize_browsers(&browsers);
}

/// Close the browsers and let the message loop end, so CEF shuts down properly.
///
/// **The difference between this and `quit_soon` is everything Chromium writes on the way out.**
/// `quit_message_loop` alone ends the loop and `main` then calls `shutdown()`, which is what flushes
/// cookies, history and saved logins to disk — but only for browsers that have been closed. So the
/// browsers go first, and the loop ends when the last of them is gone (`state.rs` does that already,
/// on the last `on_before_close`). If none of them can be found, the loop is ended directly rather
/// than leaving a browser that cannot be quit.
pub fn shut_down() {
    let Some(state) = crate::state::BruState::instance() else {
        quit_soon();
        return;
    };
    // **Every browser bru has registered, not the compositor's surfaces.** `of_browser` holds the
    // page that is *showing* plus the chrome — `show_page` keeps exactly one page entry, which is
    // its job — so a background tab's browser is in neither list. Closing only `of_browser` left
    // those alive, `BruState::on_before_close` never saw its list empty, the message loop never
    // ended, and a `:quit` with two tabs was a browser that hangs instead of one that saves its
    // cookies. The state's registry is the one list every created browser passes through.
    let browsers: Vec<i32> = state.lock().expect("state mutex poisoned").browser_ids();
    let mut closed = false;
    for identifier in browsers {
        let browser = state.lock().expect("state mutex poisoned").browser_with_id(identifier);
        if let Some(host) = browser.and_then(|browser| browser.host()) {
            host.close_browser(1);
            closed = true;
        }
    }
    if !closed {
        quit_soon();
    }
}

/// Ctrl-C, `kill`, and the terminal going away.
///
/// **A browser hit with Ctrl-C should stop the way one whose window was closed stops.** The session
/// layer's own handler restores the terminal and re-raises with the default disposition, which is
/// the honest thing for a process that was killed — and the wrong thing for this one, because being
/// killed is exactly what loses the cookies. So the first signal asks for a clean shutdown and the
/// second one takes the honest path: a browser that will not stop must still be stoppable.
fn install_interrupt() {
    // SAFETY: `pipe2` fills two descriptors or reports failure.
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return;
    }
    let (read, write) = (fds[0], fds[1]);
    INTERRUPT_PIPE.store(write, Ordering::Release);
    for signum in [libc::SIGINT, libc::SIGTERM] {
        // SAFETY: `sigaction` is filled entirely before it is installed, and the handler below is
        // async-signal-safe — it writes one byte and returns.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_interrupt as extern "C" fn(libc::c_int) as usize;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(signum, &action, std::ptr::null_mut());
        }
    }
    let _ = std::thread::Builder::new().name("bru-term-quit".to_string()).spawn(move || {
        let mut byte = [0u8; 1];
        // SAFETY: reading one byte into a local buffer from a descriptor this process owns.
        while unsafe { libc::read(read, byte.as_mut_ptr().cast(), 1) } == 1 {
            let mut task = ShutdownTask::new();
            post_task(ThreadId::UI, Some(&mut task));
        }
    });
}

/// The write end of the pipe the handler pokes. `0` before [`install_interrupt`] has run.
static INTERRUPT_PIPE: AtomicI32 = AtomicI32::new(0);

/// How many interrupts have arrived. The second one is not asked politely.
static INTERRUPTS: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_interrupt(signum: libc::c_int) {
    if INTERRUPTS.fetch_add(1, Ordering::Relaxed) > 0 {
        // **The terminal goes back before the process goes down.** This handler replaced the
        // session layer's own, whose whole job was to restore before re-raising — so the second
        // signal used to kill the process with the termios still raw and the alternate screen
        // still up, and the shell that got the terminal back got it broken. The restore is
        // async-signal-safe by construction (`term_session::restore_now`) and does its work at
        // most once, so a shutdown that already restored makes this a no-op.
        crate::term_session::restore_for_signal();
        // SAFETY: both are async-signal-safe. Restoring the default disposition first is what makes
        // the re-raise terminate rather than re-enter this handler.
        unsafe {
            libc::signal(signum, libc::SIG_DFL);
            libc::raise(signum);
        }
        return;
    }
    let fd = INTERRUPT_PIPE.load(Ordering::Acquire);
    if fd != 0 {
        let byte = b"q";
        // SAFETY: `write` is async-signal-safe; a full pipe means a shutdown is already pending,
        // and the short write that follows is ignored for exactly that reason.
        unsafe {
            libc::write(fd, byte.as_ptr().cast(), 1);
        }
    }
}

wrap_task! {
    struct ShutdownTask;

    impl Task {
        fn execute(&self) {
            let Some(state) = crate::state::BruState::instance() else {
                quit_message_loop();
                return;
            };
            // The same road `:quit` takes, so that whatever `auto_save.session` and the plugin
            // events do on the way out happens here too.
            crate::lifetime::on_quitting(&state);
            shut_down();
        }
    }
}

/// Whether [`leave`] has done its work. What makes the second call a no-op rather than a repeat.
static LEFT: AtomicBool = AtomicBool::new(false);

/// Put the terminal back and stop. Idempotent — and the idempotence is escapes, not only state.
///
/// **`main` calls this twice on purpose** — once before `shutdown()` so the terminal is not
/// hostage to a library teardown, and once after as the belt to that brace. The first version made
/// the second call repeat the whole sequence: `d=A` deleted every image the *restored* terminal
/// was holding, and `CSI 2J` cleared the screen the user had just been handed back — the shell
/// prompt and everything above it, wiped by the browser on its way out. The session's own
/// `shut_down` was guarded; the escapes written around it were not.
pub fn leave() {
    ACTIVE.store(false, Ordering::Relaxed);
    crate::term_input::stop();
    if LEFT.swap(true, Ordering::AcqRel) {
        return;
    }
    let Some(term) = TERM.get() else {
        return;
    };
    let Ok(mut term) = term.lock() else {
        return;
    };
    let mut out = std::io::stdout();
    // The presenter takes back its own image by id. That is the tidy half.
    let _ = term.painter.clear(&mut out);
    // **And then everything, by hand, because the tidy half was not enough.** Measured 2026-08-25:
    // after `:quit` the pane still showed the tab strip and a band of the page under the shell's
    // prompt. `d=I` removes the image this presenter knows about; anything a previous presenter
    // left — a frame in flight when the size changed, an id from a run that crashed — is not its to
    // know about. `d=A` is, and the cost of being thorough on the way out is one escape.
    let _ = out.write_all(
        crate::term_session::passthrough("\x1b_Ga=d,d=A,q=2\x1b\\").as_bytes(),
    );
    // The placeholder cells are text, and text belongs to the screen rather than to the image. The
    // screen is cleared before it is handed back, so nothing is left holding cells that used to
    // mean a picture.
    let _ = out.write_all(b"\x1b[H\x1b[2J");
    let _ = out.flush();
    term.session.leave();
}

/// The render handler bru's one `Client` hands out, when this run draws into a terminal.
///
/// **`None` in a Views run, and that is what keeps the two frontends from touching.** A client that
/// always returned a render handler would put CEF into windowless bookkeeping for browsers that
/// have windows; asking `is_active()` costs one relaxed load on a path that runs once per browser.
pub fn render_handler() -> Option<RenderHandler> {
    is_active().then(TermRenderHandler::new)
}

// The client the docked inspector is created with.
//
// **A render handler and a life-span handler, and nothing else — the nothing else is the point.**
// `devtools.rs` passes `None` for the inspector's client in a window, deliberately: bru's own
// client carries the keyboard handler, and `j` in a DevTools console has to type a `j` rather than
// scroll the page behind it. A windowless inspector cannot have `None` — with no render handler
// CEF has nowhere to paint and the panel is a browser nobody can see. The life-span handler is
// what registers the browser in `BruState` like every other, so `browser_with_id` can find it,
// the quit counts it, and closing it is the ordinary close.
//
// The comment is out here because `wrap_client!` matches the struct itself, and a doc comment on it
// expands to an attribute the macro has no rule for — the same note `csp.rs` leaves on `wrap_task!`.
wrap_client! {
    pub struct InspectorClient {
        state: crate::tabs::SharedState,
    }

    impl Client {
        fn render_handler(&self) -> Option<RenderHandler> {
            render_handler()
        }

        // **Not bru's, and that distinction was an endless row of tabs.** bru's handler answers a
        // popup by cancelling it and opening a *tab* — right for a page, and a loop for this: the
        // frontend's own navigation was taken for a popup, became a tab, whose frontend navigated,
        // and so on until the strip had no end. Measured 2026-08-26. The inspector is not a page
        // and nothing it asks for belongs in the tab strip, so its popups are refused and dropped.
        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(InspectorLifeSpanHandler::new(self.state.clone()))
        }

        // Where the frontend is told it is undocked, before it is told what to connect to.
        fn load_handler(&self) -> Option<LoadHandler> {
            Some(InspectorLoadHandler::new())
        }
    }
}

wrap_life_span_handler! {
    pub struct InspectorLifeSpanHandler {
        state: crate::tabs::SharedState,
    }

    impl LifeSpanHandler {
        /// `1` is "cancel", and cancelling is the whole of it: nothing the inspector asks to open
        /// is a page bru should show, and the frontend has no use for a window it did not get.
        fn on_before_popup(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut cef::Frame>,
            _popup_id: ::std::os::raw::c_int,
            _target_url: Option<&CefString>,
            _target_frame_name: Option<&CefString>,
            _target_disposition: WindowOpenDisposition,
            _user_gesture: ::std::os::raw::c_int,
            _popup_features: Option<&PopupFeatures>,
            _window_info: Option<&mut WindowInfo>,
            _client: Option<&mut Option<Client>>,
            _settings: Option<&mut BrowserSettings>,
            _extra_info: Option<&mut Option<DictionaryValue>>,
            _no_javascript_access: Option<&mut ::std::os::raw::c_int>,
        ) -> ::std::os::raw::c_int {
            1
        }

        /// **Into `BruState.browsers`, or no click can find its host.** `MouseTask` and `KeyTask`
        /// both reach a browser through `state.browser_with_id`, which searches the registry every
        /// created browser passes through — and this handler, made to refuse popups, was skipping
        /// the registration bru's own handler does. The route was right the whole way: the log
        /// showed `inspector=Some(id) … (hit=true)` for every press in the panel, and the event
        /// was dropped one line later, on a `browser_with_id` that had never heard of the browser
        /// it was asked for. Measured 2026-08-26. Registered, the inspector is also a browser
        /// `shut_down` can close and the quit can count.
        fn on_after_created(&self, browser: Option<&mut Browser>) {
            self.state
                .lock()
                .expect("state mutex poisoned")
                .on_after_created(browser);
        }

        /// Allow the close, the same answer bru's handler gives for every browser.
        fn do_close(&self, browser: Option<&mut Browser>) -> ::std::os::raw::c_int {
            self.state
                .lock()
                .expect("state mutex poisoned")
                .do_close(browser)
        }

        /// **A browser that enters the registry has to leave it**, or the quit waits for a browser
        /// that is already gone: `on_before_close` is where `BruState` removes it and, when it was
        /// the last, ends the message loop.
        fn on_before_close(&self, browser: Option<&mut Browser>) {
            // The frontend's own bookkeeping first: when the close did not come from
            // `close_inspector` — the frontend's close box, a crashed panel — the pane would keep
            // holding an inspector rectangle with no browser painting into it. A no-op for the
            // ordinary close, which has already taken the identifier.
            if let Some(browser) = browser.as_deref() {
                note_inspector_closed(browser.identifier());
            }
            // The router's mandatory forward, before the state removal that may end the message
            // loop — same order, same reason as bru's own handler in `keys.rs`.
            crate::ipc::on_before_close(browser.as_deref().cloned().as_mut());
            self.state
                .lock()
                .expect("state mutex poisoned")
                .on_before_close(browser);
        }
    }
}

wrap_load_handler! {
    pub struct InspectorLoadHandler {}

    impl LoadHandler {
        /// **The frontend is prepared on a page that has nothing to lose, then sent to work.**
        ///
        /// `can_dock=true` gives the inspector a window's frontend rather than the
        /// remote-debugging shell — but *where* it thinks it is docked is `currentDockState` in its
        /// own `localStorage`, and the default is `"right"`. A frontend that believes it is docked
        /// to the right of a host window draws itself in the right quarter of its viewport and
        /// leaves the rest empty, which in a terminal pane is three quarters of the inspector
        /// missing. Measured 2026-08-26, with a photograph of each spelling.
        ///
        /// It cannot be set before the first load — the value belongs to the `devtools://` origin,
        /// which only exists once something from it has loaded. So the browser is created on the
        /// bare frontend, which connects to nothing; the value is written; and only then is the URL
        /// with the WebSocket in it loaded. Setting it *after* connecting would need a reload, and a
        /// reload drops the connection it just made.
        fn on_load_end(
            &self,
            browser: Option<&mut Browser>,
            // `cef::Frame` by name: this file's `Frame` is the compositor's picture.
            frame: Option<&mut cef::Frame>,
            _http_status_code: ::std::os::raw::c_int,
        ) {
            let (Some(browser), Some(frame)) = (browser, frame) else {
                return;
            };
            if frame.is_main() != 1 {
                return;
            }
            let Some(target) = take_inspector_target(browser.identifier()) else {
                return;
            };
            // **The navigation is inside the script, and that is the whole of the fix.**
            // `execute_java_script` posts the script to the renderer and returns; a `load_url` on
            // the line after it starts a navigation that outruns the write. Measured 2026-08-26:
            // the value was set and the frontend still came up docked to the right, because it had
            // already read the old one. Written and then navigated *in one script*, the order is
            // the script's own and cannot be raced.
            //
            // The URL is a `devtools://` address bru built from a port number and a target id, and
            // the quotes are escaped anyway: a string that reaches JavaScript unescaped is a string
            // that decides what the script does.
            let escaped = target.replace('\\', "\\\\").replace('\'', "\\'");
            let script = format!(
                "try {{ localStorage.setItem('currentDockState', '\"undocked\"'); }} \
                 catch (e) {{}} location.replace('{escaped}');"
            );
            frame.execute_java_script(Some(&CefString::from(script.as_str())), None, 0);
        }
    }
}

/// The URL an inspector browser is to be sent to once its frontend has been prepared.
///
/// Taken rather than read: the second load must not prepare and navigate again, which would be a
/// browser that reloads itself for ever.
fn take_inspector_target(identifier: i32) -> Option<String> {
    let term = TERM.get()?;
    let mut guard = term.lock().ok()?;
    let (id, target) = guard.inspector_target.take()?;
    if id == identifier || id == 0 {
        Some(target)
    } else {
        guard.inspector_target = Some((id, target));
        None
    }
}

/// Whether the layout owes the inspector its rectangle: one is open, or one is being created and
/// `view_rect` is about to ask how big it is.
fn inspector_docked(term: &TermState) -> bool {
    term.inspector.is_some() || term.pending == Some(SurfaceKind::Inspector)
}

/// The docked inspector's browser, if `:devtools` has one open.
pub fn inspector_id() -> Option<i32> {
    let term = TERM.get()?;
    let guard = term.lock().ok()?;
    guard.inspector
}

/// Whether the keyboard is aimed at the inspector rather than the page.
///
/// **Focus follows the click, the way it does between windows.** The inspector's client carries no
/// keyboard handler — that is what lets `j` in its console type a `j` — so while it holds the keys
/// nothing reaches bru's bindings at all, exactly as with a focused DevTools window under a window
/// manager. The way back is the same as there: click the page (or the page regains it when the
/// panel closes).
static INSPECTOR_FOCUS: AtomicBool = AtomicBool::new(false);

pub fn inspector_focused() -> bool {
    INSPECTOR_FOCUS.load(Ordering::Relaxed)
}

/// Aim the keyboard at the docked inspector. `false` when there is none to aim at.
pub fn focus_inspector() -> bool {
    let Some(identifier) = inspector_id() else {
        return false;
    };
    INSPECTOR_FOCUS.store(true, Ordering::Relaxed);
    if let Some(state) = crate::state::BruState::instance() {
        let browser = state.lock().expect("state mutex poisoned").browser_with_id(identifier);
        if let Some(host) = browser.and_then(|browser| browser.host()) {
            host.set_focus(1);
        }
    }
    true
}

pub fn open_inspector(state: &crate::tabs::SharedState, url: &str) -> Result<(), String> {
    let term = TERM.get().ok_or("the terminal frontend is not running")?;
    let moved = {
        let mut guard = term.lock().map_err(|_| "terminal state poisoned".to_string())?;
        if guard.inspector.is_some() {
            return Err("an inspector is already docked".to_string());
        }
        guard.pending = Some(SurfaceKind::Inspector);
        let next = layout_for(guard.size, true);
        let moved: Vec<(i32, SurfaceKind)> = guard
            .of_browser
            .iter()
            .filter(|(_, kind)| next.rect_of(*kind) != guard.layout.rect_of(*kind))
            .copied()
            .collect();
        guard.layout = next;
        moved
    };
    resize_browsers(&moved);

    // **Created on the bare frontend, not on the URL that carries the WebSocket.** The dock state
    // has to be written before the frontend connects, and it can only be written once something
    // from the `devtools://` origin has loaded — see `InspectorLoadHandler`.
    if let Ok(mut guard) = term.lock() {
        guard.inspector_target = Some((0, url.to_string()));
    }
    let window_info = WindowInfo::default().set_as_windowless(0);
    let settings = BrowserSettings { windowless_frame_rate: 60, ..Default::default() };
    let mut client = InspectorClient::new(state.clone());
    let browser = browser_host_create_browser_sync(
        Some(&window_info),
        Some(&mut client),
        Some(&CefString::from("devtools://devtools/bundled/inspector.html")),
        Some(&settings),
        None,
        None,
    );
    let mut guard = term.lock().map_err(|_| "terminal state poisoned".to_string())?;
    guard.pending = None;
    let Some(browser) = browser else {
        // The rectangle goes back to the page — a band of empty pane where an inspector is not is
        // worse than no inspector.
        let next = layout_for(guard.size, false);
        let moved: Vec<(i32, SurfaceKind)> = guard
            .of_browser
            .iter()
            .filter(|(_, kind)| next.rect_of(*kind) != guard.layout.rect_of(*kind))
            .copied()
            .collect();
        guard.layout = next;
        drop(guard);
        resize_browsers(&moved);
        return Err("CEF would not make a windowless browser for the inspector".to_string());
    };
    let identifier = browser.identifier();
    guard.of_browser.push((identifier, SurfaceKind::Inspector));
    guard.inspector = Some(identifier);
    drop(guard);
    Ok(())
}

/// Take the inspector out of the frontend's bookkeeping and give its rectangle back: the pane's
/// half of closing, shared by `close_inspector` and a close that arrives from the browser's own
/// side. `only` narrows it to one identifier so a stale close cannot take a newer inspector's
/// rectangle; `None` back means there was nothing to forget.
fn forget_inspector(only: Option<i32>) -> Option<i32> {
    let term = TERM.get()?;
    let (identifier, moved) = {
        let mut guard = term.lock().ok()?;
        let identifier = guard.inspector?;
        if only.is_some_and(|id| id != identifier) {
            return None;
        }
        guard.inspector = None;
        guard.of_browser.retain(|(id, _)| *id != identifier);
        guard.surfaces[index_of(SurfaceKind::Inspector)] = None;
        let next = layout_for(guard.size, false);
        let moved: Vec<(i32, SurfaceKind)> = guard
            .of_browser
            .iter()
            .filter(|(_, kind)| next.rect_of(*kind) != guard.layout.rect_of(*kind))
            .copied()
            .collect();
        guard.layout = next;
        (identifier, moved)
    };
    INSPECTOR_FOCUS.store(false, Ordering::Relaxed);
    resize_browsers(&moved);
    Some(identifier)
}

/// The page takes the keyboard back, the way it does when a DevTools window closes.
fn refocus_page() {
    let Some(state) = crate::state::BruState::instance() else {
        return;
    };
    let page = TERM
        .get()
        .and_then(|term| term.lock().ok())
        .and_then(|guard| {
            guard
                .of_browser
                .iter()
                .find(|(_, kind)| *kind == SurfaceKind::Page)
                .map(|(id, _)| *id)
        });
    if let Some(identifier) = page {
        let browser = state.lock().expect("state mutex poisoned").browser_with_id(identifier);
        if let Some(host) = browser.and_then(|browser| browser.host()) {
            host.set_focus(1);
        }
    }
}

/// A docked inspector's browser is going away by its own doing — the frontend's close box, a
/// crashed panel — rather than through `close_inspector`. Called from the life-span handler's
/// `on_before_close`; a no-op when the ordinary close has already taken the identifier.
fn note_inspector_closed(identifier: i32) {
    if forget_inspector(Some(identifier)).is_some() {
        refocus_page();
    }
}

/// Close the docked inspector and give its rectangle back. A no-op when none is open.
pub fn close_inspector() {
    let Some(identifier) = forget_inspector(None) else {
        return;
    };
    let Some(state) = crate::state::BruState::instance() else {
        return;
    };
    let browser = state.lock().expect("state mutex poisoned").browser_with_id(identifier);
    if let Some(host) = browser.and_then(|browser| browser.host()) {
        // Really closed, not hidden: this browser is bru's own windowless one, not the panel CEF
        // made — the SIGSEGV `devtools.rs` documents belongs to that panel and not to this.
        host.close_browser(1);
    }
    refocus_page();
}

/// Which surface is under a pointer position (in pane pixels), and the rectangle it fills.
///
/// The page and the inspector are the two surfaces a pointer means anything to; the chrome strips
/// have nothing clickable in them. Answered together with the browser's identifier so the mouse
/// task has one question to ask under one lock.
pub fn pointer_target(x: i32, y: i32, press: bool) -> Option<(i32, Rect, SurfaceKind)> {
    let term = TERM.get()?;
    let guard = term.lock().ok()?;
    let inside = |rect: &Rect| {
        x >= rect.x && y >= rect.y && x < rect.x + rect.width && y < rect.y + rect.height
    };
    // **Presses only.** The first version logged the first six routings of any kind, and mode 1003
    // reports every pixel of pointer travel — so the six were six motions over whatever the pointer
    // crossed first, and the clicks they were meant to explain were long past the cap.
    if crate::term_input::debug() && press {
        let seen = POINTED.fetch_add(1, Ordering::Relaxed);
        if seen < 8 {
            let inspector = guard.layout.rect_of(SurfaceKind::Inspector);
            let page = guard.layout.rect_of(SurfaceKind::Page);
            eprintln!(
                "bru[term]: pointer at {x},{y}: inspector={:?} rect {}x{} at {},{} (hit={}); \
                 page rect {}x{} at {},{} (hit={})",
                guard.inspector,
                inspector.width,
                inspector.height,
                inspector.x,
                inspector.y,
                inside(&inspector),
                page.width,
                page.height,
                page.x,
                page.y,
                inside(&page),
            );
        }
    }
    // **Every surface, not just the two that were thought to be clickable.** This used to answer
    // the inspector or the page and nothing else, on the reasoning that "the chrome strips have
    // nothing clickable in them". That is untrue and was untrue when it was written:
    // `chrome/top.js` carries a delegated click handler that sends `tab-select`, which is how a
    // pointer picks a tab in a window. Measured 2026-08-26 by a user clicking tabs that did not
    // answer.
    //
    // The rectangles tile the pane and do not overlap, so the first one that contains the point is
    // the one under the pointer and the order of the search does not decide anything.
    guard
        .of_browser
        .iter()
        .map(|(identifier, kind)| (*identifier, guard.layout.rect_of(*kind), *kind))
        .find(|(_, rect, _)| inside(rect))
}

/// A button went down on one of the pointer's surfaces: focus follows the click.
pub fn note_click(kind: SurfaceKind) {
    // A click on a chrome strip picks a tab; it does not move the keyboard there. bru's whole key
    // model is that a strip's keys are redirected to the page (`keys.rs`, trap 11), and a pointer
    // does not change that. Only the inspector and the page own a keyboard.
    if !matches!(kind, SurfaceKind::Inspector | SurfaceKind::Page) {
        return;
    }
    INSPECTOR_FOCUS.store(kind == SurfaceKind::Inspector, Ordering::Relaxed);
}



wrap_render_handler! {
    pub struct TermRenderHandler {}

    impl RenderHandler {
        /// How big this browser's surface is. Answered from the layout, so CEF paints exactly the
        /// rectangle the compositor will put it in.
        fn view_rect(&self, browser: Option<&mut Browser>, rect: Option<&mut CefRect>) {
            let Some(rect) = rect else {
                return;
            };
            rect.x = 0;
            rect.y = 0;
            rect.width = 800;
            rect.height = 600;
            let Some(term) = TERM.get() else {
                return;
            };
            let Ok(term) = term.lock() else {
                return;
            };
            let Some(kind) = surface_of(&term, browser) else {
                return;
            };
            let mine = term.layout.rect_of(kind);
            rect.width = mine.width.max(1);
            rect.height = mine.height.max(1);
        }

        /// **The scale CEF uses has to be the scale the compositor used.** They are both 1.0 here;
        /// saying so out loud is what keeps a future change to one from silently disagreeing with
        /// the other.
        fn screen_info(&self, _browser: Option<&mut Browser>, screen_info: Option<&mut ScreenInfo>) -> ::std::os::raw::c_int {
            let Some(info) = screen_info else {
                return 0;
            };
            info.device_scale_factor = 1.0;
            1
        }

        /// Where a `<select>` dropdown is going to be drawn, in the page's coordinates.
        fn on_popup_size(&self, _browser: Option<&mut Browser>, rect: Option<&CefRect>) {
            let (Some(rect), Some(term)) = (rect, TERM.get()) else {
                return;
            };
            if let Ok(mut term) = term.lock() {
                term.popup = Some(Rect {
                    x: rect.x,
                    y: rect.y,
                    width: rect.width,
                    height: rect.height,
                });
            }
        }

        fn on_popup_show(&self, _browser: Option<&mut Browser>, show: ::std::os::raw::c_int) {
            let Some(term) = TERM.get() else {
                return;
            };
            if let Ok(mut term) = term.lock() {
                if show == 0 {
                    // The rectangle and the pixels go together: a popup that has been hidden must
                    // not leave its last frame behind to be composited over the next page.
                    term.popup = None;
                    term.popup_pixels = None;
                }
            }
        }

        fn on_paint(
            &self,
            browser: Option<&mut Browser>,
            type_: PaintElementType,
            _dirty_rects: Option<&[CefRect]>,
            buffer: *const u8,
            width: ::std::os::raw::c_int,
            height: ::std::os::raw::c_int,
        ) {
            if buffer.is_null() || width <= 0 || height <= 0 {
                return;
            }
            let (width, height) = (width as u32, height as u32);
            let bytes = (width as usize) * (height as usize) * 4;
            // SAFETY: CEF's contract for `on_paint` is `width * height * 4` bytes of BGRA, valid
            // for the duration of this call and no longer. **The length is not in the type** — both
            // dimensions are checked positive above and the size is computed from them here and
            // nowhere else, which is what keeps this from being a read past the end.
            let bgra = unsafe { std::slice::from_raw_parts(buffer, bytes) };

            let Some(term) = TERM.get() else {
                return;
            };
            let Ok(mut term) = term.lock() else {
                return;
            };
            // The popup layer is a surface of its own, kept apart from the page's — see
            // `TermState::popup_pixels`.
            if type_.get_raw() == PaintElementType::POPUP.get_raw() {
                term.popup_pixels = Some(Painted { bgra: bgra.to_vec(), width, height });
                drop(term);
                schedule_present();
                return;
            }
            // **A browser met for the first time is recorded here**, and the inspector is why:
            // `show_dev_tools` creates its browser and hands nothing back, so the only place its
            // identifier can be learned is the first time it paints. `pending` says which surface
            // was being asked for; after this it is known by id like the rest.
            let identifier = browser.as_ref().map(|browser| browser.identifier());
            let Some(kind) = surface_of(&term, browser) else {
                return;
            };
            if let Some(identifier) = identifier {
                if !term.of_browser.iter().any(|(known, _)| *known == identifier) {
                    term.of_browser.push((identifier, kind));
                }
            }
            let slot = index_of(kind);
            term.surfaces[slot] = Some(Painted { bgra: bgra.to_vec(), width, height });
            // **A surface with no rectangle cannot change the picture, so it must not cost one.**
            // The panel is zero-height whenever nothing is open in it and CEF paints it a pixel
            // tall anyway — measured 2026-08-26, `Panel painted 990x1 into a 990x0 rectangle`.
            // Every one of those was composing the whole pane and transmitting it to say nothing.
            // The frame is kept, because the rectangle may be given back at any moment.
            if term.layout.rect_of(kind).is_empty() {
                return;
            }
            drop(term);
            schedule_present();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(rest: &[&str]) -> Vec<String> {
        std::iter::once("bru".to_string())
            .chain(rest.iter().map(|arg| arg.to_string()))
            .collect()
    }

    #[test]
    fn the_switch_asks_for_a_terminal_run() {
        assert!(requested(&args(&["--term"])));
        assert!(requested(&args(&["--term", "https://example.com/"])));
        assert!(!requested(&args(&["https://example.com/"])));
    }

    /// `--remote` takes the rest of the line, so a `--term` inside a URL is a URL.
    #[test]
    fn a_term_after_remote_is_part_of_the_message() {
        assert!(!requested(&args(&["--remote", ":open", "https://x/?q=--term"])));
    }

    #[test]
    fn the_page_is_the_first_bare_argument() {
        assert_eq!(url_from(&args(&["--term", "https://example.com/"])), Some("https://example.com/"));
        assert_eq!(url_from(&args(&["--term"])), None);
        // A switch is not a page, however much it looks like one.
        assert_eq!(url_from(&args(&["--term", "--private"])), None);
    }

    /// Every surface the frontend draws has a slot of its own, and the mapping is total.
    #[test]
    fn every_surface_has_its_own_slot() {
        let mut seen: Vec<usize> = SURFACES.iter().map(|&kind| index_of(kind)).collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), SURFACES.len(), "two surfaces share a slot");
    }
}
