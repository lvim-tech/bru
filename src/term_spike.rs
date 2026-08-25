//! The spike: one windowless browser, painted into the terminal.
//!
//! **Throwaway, and labelled so.** It exists to put a real page in a real pane before the plan's
//! C1–C7 are built on the belief that one can go there — no tabs, no chrome, no seam, one window,
//! `j`/`k`/`q`. What it proves is meant to survive it; what it contains is not.
//!
//! Everything about the transport was settled by `term.rs`'s probe first: shared memory, 78 fps at
//! full pane, through tmux. This module's only new question is whether **real** pixels behave like
//! the synthetic ones — a page is text and flat colour where the probe drew a gradient, and CEF
//! hands over damage rectangles the probe had no equivalent of.

use cef::*;
use std::io::Write;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};

/// The pane, measured once before CEF is told how big the page is.
static PANE: OnceLock<crate::term::Pane> = OnceLock::new();

/// The browser the keys are aimed at. An id and not a handle: CEF's handles are not `Send`, and the
/// reader lives on a thread of its own — which is exactly why `csp.rs` and `editor.rs` do the same.
static BROWSER: AtomicI32 = AtomicI32::new(0);

/// Frame counter, so each shared memory object has a name of its own.
static SEQUENCE: AtomicU32 = AtomicU32::new(0);

/// `--term-spike=<url>`.
pub const SWITCH: &str = "--term-spike=";

/// The url `--term-spike=` names, read from the raw argv with `--socket=`'s rule.
pub fn url_from(args: &[String]) -> Option<&str> {
    let before = args.iter().position(|arg| arg == "--remote").unwrap_or(args.len());
    args[..before]
        .iter()
        .filter_map(|arg| arg.strip_prefix(SWITCH))
        .rfind(|url| !url.trim().is_empty())
        .map(str::trim)
}

wrap_render_handler! {
    pub struct SpikeRenderHandler {}

    impl RenderHandler {
        /// How big the page thinks it is. Device scale is left at 1, so a logical pixel here is a
        /// pixel in the pane and `on_paint` hands back exactly this many.
        fn view_rect(&self, _browser: Option<&mut Browser>, rect: Option<&mut Rect>) {
            let Some(rect) = rect else {
                return;
            };
            let pane = PANE.get().copied().unwrap_or(crate::term::Pane {
                width: 800,
                height: 600,
                cols: 80,
                rows: 24,
                source: "fallback",
            });
            rect.x = 0;
            rect.y = 0;
            rect.width = pane.width as i32;
            rect.height = pane.height as i32;
        }

        /// A frame. **This is the whole spike.**
        fn on_paint(
            &self,
            _browser: Option<&mut Browser>,
            type_: PaintElementType,
            _dirty_rects: Option<&[Rect]>,
            buffer: *const u8,
            width: ::std::os::raw::c_int,
            height: ::std::os::raw::c_int,
        ) {
            // The popup layer (a `<select>` dropdown) arrives as a second surface to be composited
            // over the first. The spike does not do that, and says so rather than drawing it in the
            // wrong place: C5 owns it.
            if type_.get_raw() != PaintElementType::VIEW.get_raw() {
                return;
            }
            if buffer.is_null() || width <= 0 || height <= 0 {
                return;
            }
            let (width, height) = (width as u32, height as u32);
            let bytes = (width as usize) * (height as usize) * 4;
            // SAFETY: CEF's contract for `on_paint` is that `buffer` points at `width * height * 4`
            // bytes of BGRA for the duration of this call and no longer. The size is **not** carried
            // by the type — this is the one place in the terminal path where a wrong number is a
            // read out of bounds rather than a wrong picture, which is why the two dimensions are
            // checked above and the length is computed from them here and nowhere else.
            let bgra = unsafe { std::slice::from_raw_parts(buffer, bytes) };

            // BGRA to RGB: three bytes out for four in, so the copy into shared memory is a quarter
            // smaller than the frame CEF produced. kitty is told `f=24`.
            let mut rgb = vec![0u8; (width as usize) * (height as usize) * 3];
            for (out, pixel) in rgb.as_chunks_mut::<3>().0.iter_mut().zip(bgra.as_chunks::<4>().0) {
                out[0] = pixel[2];
                out[1] = pixel[1];
                out[2] = pixel[0];
            }

            let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let mut out = std::io::stdout();
            let _ = out.write_all(b"\x1b[H");
            // `quiet`: this runs on the UI thread and cannot stop to read an answer, so it does
            // not ask for one — see `write_image_shm`.
            if let Ok(Some(name)) =
                crate::term::write_image_shm(&mut out, &rgb, width, height, sequence, true)
            {
                    // kitty unlinks what it reads; a terminal that ignored the escape would leave
                    // the megabytes behind, so the name is remembered for one frame and removed
                    // when the next one replaces it. Nothing here waits for an acknowledgement —
                    // this is the UI thread, and blocking it is blocking the browser.
                if let Some(previous) = LAST_SHM.lock().ok().and_then(|mut slot| slot.replace(name)) {
                    crate::term::unlink_shm(&previous);
                }
            }
        }
    }
}

/// The name of the last shared memory object handed to the terminal. See the comment in `on_paint`.
static LAST_SHM: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

wrap_client! {
    pub struct SpikeClient {}

    impl Client {
        fn render_handler(&self) -> Option<RenderHandler> {
            Some(SpikeRenderHandler::new())
        }
    }
}

/// Make the windowless browser and start reading keys. Called from `on_context_initialized`
/// **instead of** `window::create`.
pub fn start(url: &str) -> Result<(), String> {
    let pane = crate::term::pane()?;
    let _ = PANE.set(pane);
    eprintln!(
        "bru: --term-spike: {}x{} px in this pane, {url}",
        pane.width, pane.height
    );

    // **Everything printed so far is in the way of the picture, and under tmux it cannot be moved
    // out of it afterwards.** A kitty image is placed at *kitty's* cursor, and the escape that
    // carries it goes through passthrough — around tmux's screen model rather than through it — so
    // `\x1b[H` sent from here moves a cursor tmux owns and not the one the image lands at.
    // Measured 2026-08-25: with six lines of startup log above it, the frame began six lines down
    // and its bottom fell off the pane.
    //
    // Clearing the pane through the normal path makes tmux repaint, which is what puts kitty's own
    // cursor at the top-left. **It is a mitigation and not the fix**: anything that repaints the
    // pane afterwards moves the cursor again. The real answer is kitty's Unicode placeholders
    // (`U=1`), where the image is transmitted once and *placed* by drawing placeholder characters
    // through the ordinary text path, which tmux understands natively. That belongs to C4.
    let mut out = std::io::stdout();
    let _ = out.write_all(b"\x1b[2J\x1b[H\x1b[?25l");
    let _ = out.flush();

    let window_info = WindowInfo::default().set_as_windowless(0);
    let settings = BrowserSettings {
        // The default is 30 and the probe measured the terminal doing 78. Asking for 60 is asking
        // for what the pane can take; CEF treats it as a ceiling, not a promise.
        windowless_frame_rate: 60,
        ..Default::default()
    };
    let mut client = SpikeClient::new();
    let browser = browser_host_create_browser_sync(
        Some(&window_info),
        Some(&mut client),
        Some(&CefString::from(url)),
        Some(&settings),
        None,
        None,
    )
    .ok_or("CEF would not make a windowless browser")?;
    BROWSER.store(browser.identifier(), Ordering::Relaxed);

    read_keys();
    Ok(())
}

/// The reader thread: raw mode, one byte at a time, posted at the browser.
fn read_keys() {
    let _ = std::thread::Builder::new().name("bru-term-spike".to_string()).spawn(|| {
        let Ok(raw) = crate::term::Raw::enter() else {
            eprintln!("bru: --term-spike: not a terminal, so there is nothing to read keys from");
            return;
        };
        let mut stdin = std::io::stdin();
        let mut byte = [0u8; 1];
        loop {
            use std::io::Read;
            match stdin.read(&mut byte) {
                Ok(1) => match byte[0] {
                    b'q' => break,
                    b'j' => post(Action::Scroll(-crate::scroll::step())),
                    b'k' => post(Action::Scroll(crate::scroll::step())),
                    b'd' => post(Action::Scroll(-600)),
                    b'u' => post(Action::Scroll(600)),
                    _ => {}
                },
                // `VTIME` makes a quiet terminal look like this; it is not an end of input.
                Ok(_) => continue,
                Err(_) => break,
            }
        }
        drop(raw);
        post(Action::Quit);
    });
}

/// What a keystroke asks for, on the UI thread where CEF can be told about it.
enum Action {
    Scroll(i32),
    Quit,
}

/// Ask the browser to stop, from somewhere that is not yet allowed to.
///
/// **`quit_message_loop` before `run_message_loop` is a quit nobody hears.** Measured 2026-08-25:
/// `on_context_initialized` runs *inside* `initialize`, so a failure there that called it directly
/// left the loop to start afterwards and run forever — the process had to be killed. Posting it
/// puts the quit after the loop exists, which is the only place it means anything.
pub fn quit_soon() {
    post(Action::Quit);
}

fn post(action: Action) {
    let mut task = match action {
        Action::Scroll(delta) => SpikeTask::new(delta, false),
        Action::Quit => SpikeTask::new(0, true),
    };
    post_task(ThreadId::UI, Some(&mut task));
}

wrap_task! {
    struct SpikeTask {
        delta: i32,
        quit: bool,
    }

    impl Task {
        fn execute(&self) {
            if self.quit {
                let mut out = std::io::stdout();
                let _ = crate::term::delete_images(&mut out);
                let _ = out.write_all(b"\x1b[H\x1b[2J\x1b[?25h");
                let _ = out.flush();
                quit_message_loop();
                return;
            }
            let Some(state) = crate::state::BruState::instance() else {
                return;
            };
            let Some(browser) = state
                .lock()
                .expect("state mutex poisoned")
                .browser_with_id(BROWSER.load(Ordering::Relaxed))
            else {
                return;
            };
            let Some(host) = browser.host() else {
                return;
            };
            // **The same call `scroll.rs` makes, and that is the point of the spike.** bru's smooth
            // scroll is a wheel event it synthesises itself, so it is not something the terminal has
            // to be able to deliver — the terminal delivers `j`, and the browser does the rest at
            // whatever rate the compositor runs.
            let event = MouseEvent { x: 10, y: 10, modifiers: 0 };
            host.send_mouse_wheel_event(Some(&event), 0, self.delta);
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
    fn the_switch_names_the_page() {
        assert_eq!(url_from(&args(&["--term-spike=https://example.com/"])), Some("https://example.com/"));
        assert_eq!(url_from(&args(&["https://example.com/"])), None);
        assert_eq!(url_from(&args(&["--term-spike="])), None);
    }

    /// `--remote` takes the rest of the line, so a switch inside a URL is a URL.
    #[test]
    fn a_spike_url_after_remote_is_part_of_the_message() {
        let line = args(&["--remote", ":open", "https://x/?q=--term-spike=https://evil/"]);
        assert_eq!(url_from(&line), None);
    }
}
