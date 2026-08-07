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

            // --- src/scrollbar.rs and src/userstyles.rs ---------------------------------------
            // **Above the `is_active_browser` guard, and that is a fix rather than a tidy-up.**
            //
            // How a page is painted belongs to the page, not to which tab is in front of the user.
            // Below the guard, neither of these ran for the FIRST page a window shows: at startup
            // the tab's browser reaches this handler before `select_in` has marked it the showing
            // one, so the start page had no scrollbar of bru's and no user stylesheet until it was
            // reloaded by hand — measured 2026-08-07, and it had been that way since the scrollbar
            // was written. A tab opened with `:open -b` was the same: styled only once it was
            // looked at.
            //
            // Everything below the guard is about the tab on screen — the scroll position the bar
            // shows, the search `n` repeats, the mode a focused field put the window in — and those
            // are right to be guarded.
            dress(browser);
            // --- end src/scrollbar.rs and src/userstyles.rs -----------------------------------

            let (is_active, window) = {
                let state = self.state.lock().expect("state mutex poisoned");
                (state.is_active_browser(id), state.window_of_browser(id))
            };
            if !is_active {
                return;
            }

            trace(id);

// --- plugin events ---------------------------------------------------------
            // `page-loaded`, inside both guards — which is what this file's head asks another
            // workstream to do rather than declare a second `LoadHandler`. The URL is read from the
            // frame **inside the closure**, so a bru with nothing registered pays one relaxed
            // atomic load here and never touches CEF for it.
            crate::events::fire(crate::events::Event::PageLoaded, window, || {
                vec![(
                    "url",
                    crate::lua::Arg::Text(
                        browser
                            .main_frame()
                            .map(|frame| CefString::from(&frame.url()).to_string())
                            .unwrap_or_default(),
                    ),
                )]
            });
// --- end plugin events -----------------------------------------------------


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

// --- src/scrollbar.rs and src/userstyles.rs ---------------------------------------------------
/// Put bru's own scrollbar and the user's own CSS into one page.
///
/// Called from both ends of a load — see `on_load_end` for the one navigation that needs the second
/// call. Both scripts are written to be run more than once.
///
/// **Above the `is_active_browser` guard on purpose.** How a page is painted belongs to the page,
/// not to which tab is in front: below the guard, a tab opened with `:open -b` was styled only once
/// it was looked at.
fn dress(browser: &mut Browser) {
    let Some(mut frame) = browser.main_frame() else {
        return;
    };
    let url = CefString::from(&frame.url()).to_string();
    // bru's own pages link `chrome.css`, which already carries the scrollbar rules and the theme.
    // Injecting into them would be a second copy of what the stylesheet says, and a user stylesheet
    // for a `bru://` host is not a thing anybody means.
    if url.starts_with("bru://") {
        return;
    }
    let _ = &url;
    crate::scrollbar::inject(&mut frame);
    // The per-site stylesheets are **not** here. They are installed by the renderer at
    // `on_context_created` — see `src/userstyles.rs` for why this hook cannot do it for the first
    // page a window shows, and `chrome/userstyle.js` for the two other failures that answers.
}
// --- end src/scrollbar.rs and src/userstyles.rs -----------------------------------------------

/// `BRU_DEBUG_LOAD=1` prints one line per navigation that got past both guards. It is how "did the
/// hook fire at all" stops being a question that needs a rebuild.
fn trace(browser_id: i32) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("BRU_DEBUG_LOAD").is_some()) {
        eprintln!("bru[load]: main frame of the showing tab (browser {browser_id}) started loading");
    }
}
