//! Noticing that `~/.config/bru/theme.css` has been rewritten.
//!
//! `themer` writes that file and then has to tell whatever is wearing it. bru serves
//! `bru://chrome/theme.css` by reading it on **every** request, so the file being new is already
//! enough — what is missing is anything that asks again, because a chrome document fetches its
//! stylesheets when it loads and never afterwards.
//!
//! ## What was tried, and why this is what is left
//!
//! - **A signal**, the way `themer`'s `waybar` target does it. `SIGUSR1` is free — `kill -USR1` on a
//!   running bru killed it, which is the default action for a signal nobody handles and therefore
//!   also the proof that nothing else wants it. Blocking it process-wide and waiting in `sigwait`
//!   should have worked; it does not. Measured 2026-08-07: `pthread_sigmask` reports it blocked
//!   before `initialize` and **unblocked after**, because CEF resets the mask on the threads it
//!   starts. A signal delivered to any thread that does not block it kills the process, and bru does
//!   not own most of its threads.
//! - **A command**, the way the `dunst` target does it. bru takes commands on its own command line
//!   at startup and has no way to be spoken to afterwards. Giving it one is a socket and a protocol
//!   — worth doing, and named in `.claude/DECISIONS.md` as its own job, because `greasemonkey`
//!   userscripts want the same channel (qutebrowser's talk back through `$QUTE_FIFO`) and so does a
//!   `bru --remote`. It is not worth doing so that a stylesheet can be re-read.
//! - **Polling.** Written first, and it worked: one `stat` every two seconds against the file's
//!   modification time. It was a patch and reads as one — it costs a syscall for ever so that
//!   something which happens once a week is noticed, and it is late by an arbitrary number chosen to
//!   be small enough. Replaced by this.
//!
//! ## The shape
//!
//! **The directory is watched, not the file.** A writer that renames a temporary into place — which
//! is what `data.rs` does for bru's own files and what any careful writer does — leaves the name
//! pointing at a new inode, and a watch on the old one never fires again. `IN_MOVED_TO` and
//! `IN_CLOSE_WRITE` on the directory catch both spellings.
//!
//! `~/.config/bru/` may not exist: **bru never creates it** — that is the browser's own rule, and it
//! is why this cannot simply make the directory and watch it. So `~/.config/` is watched too, for
//! the directory turning up, and the real watch is added when it does. One `inotify` file
//! descriptor carries both.
//!
//! Nothing here is on any hot path: a thread sits blocked in `read` and costs nothing until the
//! kernel has something to say.

use cef::*;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

/// The file whose rewriting is worth noticing.
const THEME: &str = "theme.css";
/// The directory it is in, under `~/.config` or `$XDG_CONFIG_HOME`.
const DIR: &str = "bru";

/// Start watching, on a thread of its own.
///
/// Called from `app.rs` once the UI thread exists, because the task an event posts has nowhere to go
/// before that. Answers nothing: a platform or a permission that will not allow the watch leaves bru
/// working exactly as it did, with `:colorscheme --reload` as the way to ask by hand.
pub fn start() {
    let Some(dir) = crate::chrome::config_dir() else {
        return;
    };
    let Some(parent) = dir.parent().map(PathBuf::from) else {
        return;
    };
    std::thread::Builder::new()
        .name("bru-theme-watch".to_string())
        .spawn(move || run(parent, dir))
        .map(|_| ())
        .unwrap_or_else(|e| eprintln!("bru[theme]: could not start the watch: {e}"));
}

fn run(parent: PathBuf, dir: PathBuf) {
    // SAFETY: `inotify_init1` takes only flags and answers a file descriptor or -1.
    let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
    if fd < 0 {
        eprintln!("bru[theme]: inotify is not available; :colorscheme --reload is the way to ask");
        return;
    }

    // The directory itself, when it is already there. `IN_CLOSE_WRITE` is a writer that wrote in
    // place; `IN_MOVED_TO` is one that renamed a temporary over the name.
    let on_dir = add(fd, &dir, libc::IN_CLOSE_WRITE | libc::IN_MOVED_TO);
    // And its parent, for the directory turning up later. bru may not create it, so a first run on a
    // machine where `themer` has never written is a run with nothing to watch yet.
    let on_parent = if on_dir < 0 {
        add(fd, &parent, libc::IN_CREATE | libc::IN_MOVED_TO)
    } else {
        -1
    };
    if on_dir < 0 && on_parent < 0 {
        // SAFETY: `fd` came from `inotify_init1` and nothing else holds it.
        unsafe { libc::close(fd) };
        return;
    }

    // `inotify_event` is followed by a name, so the buffer holds several of both — which is why it is
    // bytes and not the event type: the names are variable-length, so no array of events has the
    // right shape. A `[u8]` has alignment 1 and `inotify_event` needs 4, so the header is *copied*
    // out with `read_unaligned` below rather than borrowed where it lies; forming a
    // `&inotify_event` into this buffer would be undefined behaviour whether or not the address
    // happened to be even.
    const SLOTS: usize = 64;
    let mut buffer = [0u8; SLOTS * (std::mem::size_of::<libc::inotify_event>() + libc::NAME_MAX as usize + 1)];
    let mut on_dir = on_dir;
    loop {
        // SAFETY: the kernel writes at most `buffer.len()` bytes into a buffer this thread owns.
        let read = unsafe {
            libc::read(fd, buffer.as_mut_ptr() as *mut libc::c_void, buffer.len())
        };
        if read <= 0 {
            // EINTR is the only failure worth going round again for, and a `read` that keeps failing
            // would spin. Saying so once and stopping is the honest answer.
            if read < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            eprintln!("bru[theme]: the watch ended; :colorscheme --reload is the way to ask");
            break;
        }

        let mut at = 0usize;
        let mut changed = false;
        while at + std::mem::size_of::<libc::inotify_event>() <= read as usize {
            // SAFETY: the loop condition put a whole event's worth of what the kernel wrote at `at`,
            // and `read_unaligned` copies it out, so the byte buffer's alignment 1 is enough.
            let event = unsafe {
                std::ptr::read_unaligned(buffer.as_ptr().add(at) as *const libc::inotify_event)
            };
            let name_len = event.len as usize;
            let name_at = at + std::mem::size_of::<libc::inotify_event>();
            let name = if name_len > 0 && name_at + name_len <= read as usize {
                // The name is NUL-padded to a multiple of the event's alignment.
                let bytes = &buffer[name_at..name_at + name_len];
                let end = bytes.iter().position(|byte| *byte == 0).unwrap_or(bytes.len());
                std::str::from_utf8(&bytes[..end]).unwrap_or("")
            } else {
                ""
            };

            if event.wd == on_dir && name == THEME {
                changed = true;
            } else if on_dir < 0 && name == DIR {
                // The directory has just appeared; watch it from now on. The file it was made for
                // may already be in it, so this counts as a change.
                on_dir = add(fd, &dir, libc::IN_CLOSE_WRITE | libc::IN_MOVED_TO);
                changed = on_dir >= 0;
            }
            at = name_at + name_len;
        }

        if changed {
            let mut task = ThemeChanged::new();
            post_task(ThreadId::UI, Some(&mut task));
        }
    }
    // SAFETY: `fd` came from `inotify_init1` and this is the only owner.
    unsafe { libc::close(fd) };
}

/// One watch, or a negative number. A path that is not there is not an error worth a line: the
/// caller decides what an absent directory means.
fn add(fd: i32, path: &std::path::Path, mask: u32) -> i32 {
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return -1;
    };
    // SAFETY: `c_path` is NUL-terminated and outlives the call; `fd` is this module's.
    unsafe { libc::inotify_add_watch(fd, c_path.as_ptr(), mask) }
}

wrap_task! {
    struct ThemeChanged {}

    impl Task {
        fn execute(&self) {
            debug_assert_ne!(currently_on(ThreadId::UI), 0);
            crate::chrome::warn_if_incomplete();
            // The chrome **and** the two pieces of the theme a page carries — see
            // `ipc::reapply_theme_everywhere` for what those are and what reloading only the
            // chrome left behind.
            crate::ipc::reapply_theme_everywhere();
            eprintln!("bru[theme]: ~/.config/bru/theme.css was rewritten — re-read");
        }
    }
}
