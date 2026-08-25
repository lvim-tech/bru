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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::term_compose::{Frame, Layer, Layout, LayoutRequest, Rect, Surface, SurfaceKind, layout};
use crate::term_paint::{IMAGE_ID_VIEW, Painter, Placement};
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
    args[1..before]
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
    /// The popup's own pixels. CEF paints it as a **second surface** with its own buffer and its own
    /// size, which is why it cannot share the page's slot: a `<select>` dropdown is 200x300 over a
    /// page that is 990x1200, and writing one into the other's buffer is not compositing, it is
    /// corruption.
    popup_pixels: Option<Painted>,
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
/// Its height is whatever the page measured — see [`layout_for`]. The inspector is absent for the
/// reason `shell.rs` gives.
const SURFACES: [SurfaceKind; 4] =
    [SurfaceKind::Page, SurfaceKind::Top, SurfaceKind::Bottom, SurfaceKind::Panel];

fn index_of(kind: SurfaceKind) -> usize {
    SURFACES.iter().position(|&k| k == kind).unwrap_or(0)
}

/// Start the terminal frontend. Called from `on_context_initialized` instead of `window::create`.
pub fn start(state: &crate::tabs::SharedState, url: &str) -> Result<(), String> {
    let session = TerminalSession::enter()?;
    let size = session.size();
    let layout = layout_for(size);
    let frame = Frame::new(layout.pane.width, layout.pane.height)
        .ok_or("the pane is too small to draw a browser in")?;
    let placement = Placement::new(IMAGE_ID_VIEW, 1, 1, size.rows, size.cols);
    let painter = Painter::new(placement, session.in_tmux());

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
    };
    if TERM.set(Mutex::new(term)).is_err() {
        return Err("the terminal frontend is already running".to_string());
    }
    ACTIVE.store(true, Ordering::Relaxed);
    // What the terminal actually agreed to, in the log rather than guessed at later. The mouse in
    // particular has three ways to be wrong and they are indistinguishable from the outside: not
    // forwarded at all, forwarded in cells, forwarded in pixels.
    {
        let guard = TERM.get().and_then(|term| term.lock().ok());
        if let Some(guard) = guard {
            eprintln!(
                "bru[term]: pane {}x{} px, {}x{} cells ({}x{} per cell) via {:?}; tmux={}; \
                 kitty-keyboard={:?}; mouse-pixels={}",
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
            );
        }
    }

    // **A window slot, even with no window in it.** Everything in `state.rs` is keyed by one —
    // which mode keys are in, which browsers are chrome, which tab is showing — and none of that is
    // about a `Window` handle. The slot is the window; the handle was only ever how Views drew it.
    state.lock().expect("state mutex poisoned").open_window_slot();

    for kind in SURFACES {
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
fn layout_for(size: PaneSize) -> Layout {
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
        inspector_height: 0,
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
        let next = layout_for(guard.size);
        if next.rect_of(SurfaceKind::Panel) == guard.layout.rect_of(SurfaceKind::Panel) {
            return;
        }
        guard.layout = next;
        // The page's height moved with the panel's, so its last frame is the wrong shape. Dropping
        // it means one frame of that surface missing rather than one frame of it stretched.
        guard.surfaces[index_of(SurfaceKind::Page)] = None;
        guard.surfaces[index_of(SurfaceKind::Panel)] = None;
        guard.of_browser.clone()
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

/// Show the page of a browser that already exists — the terminal's whole notion of "select a tab".
pub fn show_page(identifier: i32) {
    let Some(term) = TERM.get() else {
        return;
    };
    let Ok(mut guard) = term.lock() else {
        return;
    };
    // Exactly one browser is the page at a time. Everything else keeps painting into nothing, which
    // costs a `memcpy` a frame and buys a tab that is still alive when it is switched back to.
    guard.of_browser.retain(|(id, kind)| *kind != SurfaceKind::Page || *id == identifier);
    if !guard.of_browser.iter().any(|(id, _)| *id == identifier) {
        guard.of_browser.push((identifier, SurfaceKind::Page));
    }
    guard.surfaces[index_of(SurfaceKind::Page)] = None;
    drop(guard);
    crate::term_input::aim_at(identifier);
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
    let damaged = crate::term_compose::compose(&mut term.frame, &layers);
    if damaged.is_none() {
        return;
    }
    let mut out = std::io::stdout();
    let _ = term.session.begin_sync(&mut out);
    let _ = term.painter.paint(
        &mut out,
        term.frame.rgb(),
        term.frame.width() as u32,
        term.frame.height() as u32,
    );
    let _ = term.session.end_sync(&mut out);
    let _ = out.flush();
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
        term.layout = layout_for(size);
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

/// Put the terminal back and stop. Idempotent.
pub fn leave() {
    ACTIVE.store(false, Ordering::Relaxed);
    crate::term_input::stop();
    if let Some(term) = TERM.get() {
        let mut term = term.lock().expect("terminal state poisoned");
        let mut out = std::io::stdout();
        let _ = term.painter.clear(&mut out);
        let _ = out.flush();
        term.session.leave();
    }
}

/// The render handler bru's one `Client` hands out, when this run draws into a terminal.
///
/// **`None` in a Views run, and that is what keeps the two frontends from touching.** A client that
/// always returned a render handler would put CEF into windowless bookkeeping for browsers that
/// have windows; asking `is_active()` costs one relaxed load on a path that runs once per browser.
pub fn render_handler() -> Option<RenderHandler> {
    is_active().then(TermRenderHandler::new)
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
                present(&mut term);
                return;
            }
            let Some(kind) = surface_of(&term, browser) else {
                return;
            };
            let slot = index_of(kind);
            term.surfaces[slot] = Some(Painted { bgra: bgra.to_vec(), width, height });
            present(&mut term);
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
