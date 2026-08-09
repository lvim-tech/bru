//! The two moments a browser has that are not about any page: it started, and it is going away.
//!
//! # Why "going away" needs a place of its own
//!
//! CEF already has `on_before_close`, and it is the wrong hook for anything that wants to *read*
//! the browser. `exec.rs` learned this when `:quit --save` was written: by the time
//! `on_before_close` runs the browsers are being torn down and their navigation lists no longer
//! read back, so a handler asking what was open finds nothing. Saving had to move earlier, into the
//! command itself.
//!
//! That left a hole this module fills. `:quit --save` covers one road to an exit — the one where
//! the user typed the command. The window manager's close button is the other, and it went straight
//! to teardown with nothing given a chance to look. So the answer is a **choke point above both**:
//!
//! ```text
//!   :quit          ─┐
//!                   ├─→  on_quitting()  ─→  the `quit` event, then session.auto_save
//!   window close   ─┘         (once)
//! ```
//!
//! `can_close` is where the second road passes, and it is early enough by construction: CEF asks it
//! *whether* the window may close, so nothing has been torn down when it is asked.
//!
//! # Once, and why that is not merely tidiness
//!
//! Closing three windows calls `can_close` three times, and `:quit` calls it once itself after
//! `close_all`. A session saved three times is only wasteful; a `quit` handler run three times is a
//! plugin's bug report. The flag is the contract: **`quit` fires once per process, or not at all.**

use std::sync::atomic::{AtomicBool, Ordering};

use crate::tabs::SharedState;

/// Whether [`on_quitting`] has already run. See the module header for why once is a promise.
static QUITTING: AtomicBool = AtomicBool::new(false);

/// The browser is up. Fires the `started` event, once, from `app.rs::on_context_initialized`.
///
/// After the first window is created rather than before, so that a handler asking `bru.cmd("tabs")`
/// is answered by a browser that has one.
pub fn on_started() {
    crate::events::fire(crate::events::Event::Started, None, Vec::new);
}

/// The browser is going away, and everything is still readable. Both roads to an exit call this;
/// the first one to arrive does the work.
///
/// **The event fires before the session is written**, so a handler may still open, close or reorder
/// tabs and have that be what is saved. It is the last moment anything can.
pub fn on_quitting(state: &SharedState) {
    if !claim() {
        return;
    }

    crate::events::fire(crate::events::Event::Quit, None, Vec::new);

    // **A session loaded by hand is not written over.** Restore `default`, `:session-load работа`,
    // then quit: without this the work tabs would be saved as `default`, damaging the session that
    // was restored and leaving the one that was loaded untouched. See `session::loaded_by_hand`.
    if crate::session::loaded_by_hand() {
        eprintln!(
            "bru: session.auto_save: a session was loaded by hand, so nothing was written \
             automatically — :session-save writes it when you say so"
        );
        return;
    }
    autosave(state);
}

/// **Leaving a session by loading another is leaving it**, so this runs there too.
///
/// `:session-load` calls it *before* it opens anything, while the tabs it is about to replace are
/// still there to be written. Without it the sequence "work all morning in `default`, load
/// `работа` at noon" loses the morning: the automatic save at exit stands down because a session
/// was loaded by hand, and nothing ever wrote what was open before the load.
///
/// It answers whether it wrote, so the caller can say so beside its own line.
pub fn autosave(state: &SharedState) -> bool {
    // `session.auto_save` names the session to write, or says nothing and writes none. `true` is
    // the short way of saying `default`; see `session::auto_name` for the three answers.
    let Some(name) =
        crate::session::auto_name(crate::settings::text_of("session.auto_save").as_deref())
    else {
        return false;
    };
    match crate::session::save(state, &name) {
        Ok(path) => {
            eprintln!("bru: session.auto_save: saved to {}", path.display());
            true
        }
        Err(e) => {
            eprintln!("bru: session.auto_save: could not save: {e}");
            false
        }
    }
}

/// Take the exit, if nobody has. `true` to the first caller and `false` to every one after it.
///
/// Separated from [`on_quitting`] so that "once" can be a test rather than a paragraph: the rest of
/// that function needs a `BruState`, and there is one of those per process — a test that made a
/// second would trip the singleton assert that exists to catch exactly that mistake in earnest.
fn claim() -> bool {
    !QUITTING.swap(true, Ordering::SeqCst)
}

/// Whether the exit has already been announced — read by `exec.rs` so that `:quit --save` does not
/// write the same session twice when the setting has already written it.
pub fn is_quitting() -> bool {
    QUITTING.load(Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The promise the module header makes, and the whole of what a plugin is owed.
    #[test]
    fn quitting_is_announced_once_however_many_windows_close() {
        assert!(!is_quitting(), "nothing has quit yet");

        assert!(claim(), "the first caller does the work");
        assert!(is_quitting(), "and says so");

        // Three windows closing is three calls, and a plugin must see one `quit`. Remove the
        // `swap` and answer `true` every time, and these two fail.
        assert!(!claim(), "the second is refused");
        assert!(!claim(), "and the third");
    }
}
