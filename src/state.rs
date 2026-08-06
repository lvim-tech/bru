//! The state every browser-process handler shares.
//!
//! CEF constructs a fresh handler object for most of its callbacks, so nothing can be stored in a
//! handler and found again later. The browser list, the one `Client` and the window live here
//! instead, behind a single mutex that every handler holds an `Arc` to.
//!
//! Only the browser process ever fills this in. The renderer, GPU and zygote processes re-execute
//! the same binary and construct it too, where it stays empty — see `main.rs`.

use cef::*;
use std::sync::{Arc, Mutex, OnceLock, Weak};

/// A weak handle to the one instance, for callbacks that arrive with no way of being handed the
/// `Arc` — the delayed-close task below is one. The strong reference lives in `BruApp`.
static INSTANCE: OnceLock<Weak<Mutex<BruState>>> = OnceLock::new();

pub struct BruState {
    /// Every live browser, in creation order. Emptying this list ends the process.
    browsers: Vec<Browser>,
    /// The single `Client`, handed to every browser so they share handlers. It holds an `Arc` back
    /// to this state, so the two keep each other alive for the life of the process — deliberate:
    /// both are wanted until CEF shuts down, and CEF holds its own reference to the client anyway.
    client: Option<Client>,
    /// The top-level window, kept from `on_window_created` so views can be added to it later.
    window: Option<Window>,
    /// Identifiers of the browsers behind the two chrome strips. Keys that reach those must not be
    /// read as page movements — `j` in the command line is a letter, not a scroll.
    chrome_browsers: Vec<i32>,
}

impl BruState {
    pub fn new() -> Arc<Mutex<Self>> {
        // `new_cyclic` so the global weak handle is in place before the Arc exists, and therefore
        // before any callback can go looking for it.
        Arc::new_cyclic(|weak| {
            if INSTANCE.set(weak.clone()).is_err() {
                let previous = INSTANCE.get().expect("set failed but nothing is stored");
                assert_eq!(
                    previous.strong_count(),
                    0,
                    "a second BruState while the first is still alive"
                );
            }

            Mutex::new(Self {
                browsers: Vec::new(),
                client: None,
                window: None,
                chrome_browsers: Vec::new(),
            })
        })
    }

    pub fn instance() -> Option<Arc<Mutex<Self>>> {
        INSTANCE.get().and_then(Weak::upgrade)
    }

    pub fn set_client(&mut self, client: Client) {
        self.client = Some(client);
    }

    pub fn client(&self) -> Option<Client> {
        self.client.clone()
    }

    pub fn set_window(&mut self, window: Window) {
        self.window = Some(window);
    }

    pub fn window(&self) -> Option<Window> {
        self.window.clone()
    }

    /// Learned from the chrome views' own delegate, which CEF hands both the view and the browser
    /// it made for it. Reading the frame URL instead would race the first load.
    pub fn note_chrome_browser(&mut self, identifier: i32) {
        if !self.chrome_browsers.contains(&identifier) {
            self.chrome_browsers.push(identifier);
        }
    }

    pub fn is_chrome_browser(&self, identifier: i32) -> bool {
        self.chrome_browsers.contains(&identifier)
    }

    /// A browser has come up. Every browser bru creates goes through here.
    pub fn on_after_created(&mut self, browser: Option<&mut Browser>) {
        debug_assert_ne!(currently_on(ThreadId::UI), 0);

        let Some(browser) = browser.cloned() else {
            return;
        };
        self.browsers.push(browser);
    }

    /// Allow the close. Returning 1 here would mean "I will close it myself later", which is the
    /// windowless path bru does not use.
    pub fn do_close(&mut self, _browser: Option<&mut Browser>) -> ::std::os::raw::c_int {
        debug_assert_ne!(currently_on(ThreadId::UI), 0);
        0
    }

    /// A browser is gone for good. When the last one goes, so does the message loop — without this
    /// the window disappears and the process stays alive with nothing to show for it.
    pub fn on_before_close(&mut self, browser: Option<&mut Browser>) {
        debug_assert_ne!(currently_on(ThreadId::UI), 0);

        if let Some(mut browser) = browser.cloned() {
            if let Some(index) = self
                .browsers
                .iter()
                .position(|elem| elem.is_same(Some(&mut browser)) != 0)
            {
                self.browsers.remove(index);
            }
        }

        if self.browsers.is_empty() {
            self.window = None;
            self.client = None;
            quit_message_loop();
        }
    }
}

/// `--close-after-ms=N` closes the window N milliseconds after it opens.
///
/// It is here because this machine has no way to close one window from a script: the compositor is
/// mango, whose `mmsg -s -d killclient` acts on whatever happens to be focused. Without this the
/// close path — `can_close` → `try_close_browser` → `do_close` → `on_before_close` →
/// `quit_message_loop` — can only ever be exercised by hand, which is to say never in a check that
/// runs twice. Inert unless the switch is passed.
pub fn schedule_close(delay_ms: i64) {
    let mut task = CloseWindow::new();
    post_delayed_task(ThreadId::UI, Some(&mut task), delay_ms);
}

wrap_task! {
    struct CloseWindow;

    impl Task {
        fn execute(&self) {
            debug_assert_ne!(currently_on(ThreadId::UI), 0);

            let Some(state) = BruState::instance() else {
                return;
            };
            // Take a handle to the window and drop the lock before closing it: the close runs the
            // window delegate's callbacks, and those lock this same mutex.
            let window = state.lock().expect("state mutex poisoned").window();
            if let Some(window) = window {
                window.close();
            }
        }
    }
}
