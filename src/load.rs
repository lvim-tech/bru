//! The load handler — the one place bru learns that the page under it is being replaced.
//!
//! It exists because several things bru keeps are *about* a document rather than about a tab: the
//! scroll position the status bar shows, the search `n` would repeat. A tab switch already tells
//! them to forget (`tabs::select`); a navigation did not, so `/foo` on one page left `n` repeating a
//! search Chromium had already dropped, and `[73%]` sat over a page that had just started loading.
//!
//! **Two guards, and both are load-bearing.** `on_load_start` fires for every frame of every
//! browser, and three browsers share one `Client` — the tab and the two chrome strips:
//!
//! - **Main frame only.** An advert in an iframe finishing a load is not the page changing.
//! - **The showing tab only.** A background tab navigating must not clear the bar belonging to the
//!   tab on screen, and `is_active_browser` answers false for a chrome strip as well, which is what
//!   keeps `bru://chrome/bottom.html` reloading out of this.
//!
//! Other workstreams want this hook. Add a call inside the guards below rather than a second
//! `LoadHandler`: CEF asks the client for one handler, and a second one would replace this.

use cef::*;
use std::sync::{Arc, Mutex};

use crate::state::BruState;

// (The `wrap_` macros take no doc comment on the struct they declare — CEF-NOTES.md trap 8.)
wrap_load_handler! {
    pub struct BruLoadHandler {
        state: Arc<Mutex<BruState>>,
    }

    impl LoadHandler {
        fn on_load_start(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            _transition_type: TransitionType,
        ) {
            // Subframes navigate constantly and none of it is the page changing.
            if !frame.map(|frame| frame.is_main() != 0).unwrap_or(false) {
                return;
            }
            let Some(browser) = browser else {
                return;
            };
            let id = browser.identifier();
            let (is_active, window) = {
                let state = self.state.lock().expect("state mutex poisoned");
                (state.is_active_browser(id), state.window_of_browser(id))
            };
            if !is_active {
                return;
            }

            trace(id);

            // The position belongs to the document that is going away; the next one reports its own
            // as soon as it is scrolled.
            crate::scroll::forget();
            // The search goes with it — both bru's memory of what `n` repeats and Chromium's own
            // find session, which measurably does *not* end on its own. See `find::forget_for`.
            crate::find::forget_for(browser);
            // --- src/focus.rs -----------------------------------------------------------------
            // And so does insert mode: qutebrowser's `input.insert_mode.leave_on_load`, which is
            // true by default. The field that was being typed into belongs to the document this
            // navigation is replacing, so the mode that named it has to go with the document.
            if let Some(window) = window {
                crate::focus::on_load_started(window);
            }
            // --- end src/focus.rs -------------------------------------------------------------
        }
    }
}

/// `BRU_DEBUG_LOAD=1` prints one line per navigation that got past both guards. It is how "did the
/// hook fire at all" stops being a question that needs a rebuild.
fn trace(browser_id: i32) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("BRU_DEBUG_LOAD").is_some()) {
        eprintln!("bru[load]: main frame of the showing tab (browser {browser_id}) started loading");
    }
}
