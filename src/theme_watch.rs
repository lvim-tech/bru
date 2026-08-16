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
    let mut buffer = [0u8; SLOTS * (HEADER + libc::NAME_MAX as usize + 1)];
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

        // `min`: the kernel does not answer more than it was given room for, and a slice that
        // assumed otherwise would panic on the one line in this module that has no reason to.
        let filled = (read as usize).min(buffer.len());
        let mut changed = false;
        for seen in events_in(&buffer[..filled]) {
            match meaning(&seen, on_dir) {
                Some(Meant::ThemeRewritten) => changed = true,
                Some(Meant::DirAppeared) => {
                    // The directory has just appeared; watch it from now on. The file it was made for
                    // may already be in it, so this counts as a change.
                    on_dir = add(fd, &dir, libc::IN_CLOSE_WRITE | libc::IN_MOVED_TO);
                    changed = on_dir >= 0;
                }
                None => {}
            }
        }

        if changed {
            let mut task = ThemeChanged::new();
            post_task(ThreadId::UI, Some(&mut task));
        }
    }
    // SAFETY: `fd` came from `inotify_init1` and this is the only owner.
    unsafe { libc::close(fd) };
}

// ------------------------------------------------------------------------------------------------
// Reading what the kernel wrote
// ------------------------------------------------------------------------------------------------

/// The size of one event's header — the four fields, without the name that follows them.
const HEADER: usize = std::mem::size_of::<libc::inotify_event>();

/// One event out of what a `read` returned: the watch it belongs to, and the name it carries.
///
/// The name is `""` when there is none to read — most events on a watched directory carry one, the
/// `IN_Q_OVERFLOW` the kernel sends when it has dropped events carries none — and also when what is
/// there is not a name this module will act on. See [`name_in`].
#[derive(PartialEq, Eq, Debug)]
struct Seen<'a> {
    wd: i32,
    name: &'a str,
}

/// Walk the events packed into what one `read` returned.
///
/// **Every bound is taken from `buffer`, never from what an event claims about itself.** These bytes
/// come from the kernel, so a wrong `len` is not something a page can arrange — but it is the one
/// input this module does not write, the walk runs on a thread whose whole job is to still be there
/// tomorrow, and a panic on it costs the feature silently. So a header is read only when a whole one
/// fits, a name only when the whole name fits, and an event whose `len` runs off the end contributes
/// its header with no name and *ends* the walk: nothing after a length that cannot be trusted can be
/// trusted to begin an event either. Fail-closed, and it answers what it could read.
fn events_in(buffer: &[u8]) -> Vec<Seen<'_>> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at + HEADER <= buffer.len() {
        // SAFETY: the loop condition put a whole event's worth of what the kernel wrote at `at`, and
        // `read_unaligned` copies it out, so the byte buffer's alignment 1 is enough.
        let event = unsafe {
            std::ptr::read_unaligned(buffer.as_ptr().add(at) as *const libc::inotify_event)
        };
        let name_at = at + HEADER;
        // `checked_add` and not `+`: `len` is a `u32` read out of the buffer, and while that cannot
        // overflow a `usize` on any machine bru runs on, this is not the place to depend on the width
        // of a pointer for it.
        match name_at.checked_add(event.len as usize) {
            Some(end) if end <= buffer.len() => {
                out.push(Seen { wd: event.wd, name: name_in(&buffer[name_at..end]) });
                at = end;
            }
            _ => {
                out.push(Seen { wd: event.wd, name: "" });
                break;
            }
        }
    }
    out
}

/// The name inside one event's name field.
///
/// **Two rules and both are refusals.** The field is NUL-padded up to the event's alignment, so the
/// name is what precedes the first NUL and the padding is dropped — `inotify(7)`: `len` "includes the
/// null bytes".
///
/// Anything left holding a control character is then refused outright rather than cleaned up into
/// something that might match. That direction matters: a file really called `theme.css\n` is a
/// *different file* from `theme.css`, and stripping the newline would have bru reload its theme
/// because a neighbour was written. Bytes that are not UTF-8 go the same way — a name bru cannot read
/// is a name bru will not act on.
fn name_in(field: &[u8]) -> &str {
    let end = field.iter().position(|byte| *byte == 0).unwrap_or(field.len());
    let Ok(name) = std::str::from_utf8(&field[..end]) else {
        return "";
    };
    if name.chars().any(char::is_control) {
        return "";
    }
    name
}

/// What one event means to this watch, or nothing.
#[derive(PartialEq, Eq, Debug)]
enum Meant {
    /// The theme was rewritten under the watch on `~/.config/bru`.
    ThemeRewritten,
    /// `~/.config/bru` has just appeared under the watch on its parent.
    DirAppeared,
}

/// **`on_dir` is a watch descriptor or it is a sentinel, and the two must not be compared as one.**
/// [`add`] answers `-1` for a directory it could not watch, and the kernel's own `IN_Q_OVERFLOW`
/// carries `wd = -1` — so a bare `event.wd == on_dir` asks "is this event for the watch that does not
/// exist" and can be answered yes. It cannot fire as the code stands, because an overflow event
/// carries no name and so matches neither string; but nothing about that is load-bearing, it is one
/// comparison to stop depending on it, and the test below is what says so.
fn meaning(seen: &Seen, on_dir: i32) -> Option<Meant> {
    if on_dir >= 0 && seen.wd == on_dir && seen.name == THEME {
        return Some(Meant::ThemeRewritten);
    }
    if on_dir < 0 && seen.name == DIR {
        return Some(Meant::DirAppeared);
    }
    None
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

#[cfg(test)]
mod tests {
    use super::*;

    /// What the kernel pads the name field up to, and what the header needs to sit on.
    const ALIGN: usize = std::mem::align_of::<libc::inotify_event>();

    /// **The layout these tests build by hand, asserted rather than assumed.** Every buffer below is
    /// four little-endian words and then a name, which is `struct inotify_event` on every platform bru
    /// runs on. A libc where it is not would make the rest of this module wrong too, and this is the
    /// line that would say so instead of the tests quietly checking something else.
    #[test]
    fn the_header_is_the_four_words_these_tests_write() {
        assert_eq!(HEADER, 16, "inotify_event is not four 32-bit fields on this platform");
        assert_eq!(ALIGN, 4);
    }

    /// One event, laid out the way the kernel lays one out. `len` is passed in rather than derived so
    /// that a test can lie about it, which is most of the point below.
    fn event(wd: i32, name_field: &[u8], len: u32) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER + name_field.len());
        out.extend_from_slice(&wd.to_ne_bytes());
        out.extend_from_slice(&libc::IN_CLOSE_WRITE.to_ne_bytes());
        out.extend_from_slice(&0u32.to_ne_bytes()); // cookie
        out.extend_from_slice(&len.to_ne_bytes());
        out.extend_from_slice(name_field);
        out
    }

    /// The name field as the kernel writes one: the bytes, a NUL, then NUL padding up to a multiple of
    /// the alignment — and `len` counts all of it.
    fn field(name: &str) -> Vec<u8> {
        let mut field = name.as_bytes().to_vec();
        field.push(0);
        while !field.len().is_multiple_of(ALIGN) {
            field.push(0);
        }
        field
    }

    /// One honest event: a name, padded, with the `len` that describes it.
    fn honest(wd: i32, name: &str) -> Vec<u8> {
        let field = field(name);
        event(wd, &field, field.len() as u32)
    }

    #[test]
    fn one_event_carries_its_watch_and_its_name() {
        let buffer = honest(3, THEME);
        assert_eq!(events_in(&buffer), vec![Seen { wd: 3, name: THEME }]);
    }

    /// The kernel packs as many as fit into one `read`, and a walk that stopped at the first would
    /// miss the rewrite it is here for — `themer` writing a temporary and renaming it over the name is
    /// two events, and the second one is the one that matters.
    #[test]
    fn several_events_packed_into_one_buffer_are_all_read() {
        let mut buffer = Vec::new();
        buffer.extend_from_slice(&honest(3, "theme.css.tmp"));
        buffer.extend_from_slice(&honest(3, THEME));
        buffer.extend_from_slice(&honest(7, "something-else"));
        assert_eq!(
            events_in(&buffer),
            vec![
                Seen { wd: 3, name: "theme.css.tmp" },
                Seen { wd: 3, name: THEME },
                Seen { wd: 7, name: "something-else" },
            ]
        );
    }

    /// **A name that ends exactly at the last byte.** The bound is `<=` and not `<`, and an off-by-one
    /// here would drop the last event of every full buffer — the one case a test finds and a running
    /// browser hides, because the event it dropped is the one nobody saw arrive.
    #[test]
    fn a_name_ending_exactly_at_the_end_of_the_buffer_is_read() {
        // No padding at all: `len` is the name and one NUL, and the buffer stops there.
        let mut name_field = THEME.as_bytes().to_vec();
        name_field.push(0);
        let buffer = event(3, &name_field, name_field.len() as u32);
        assert_eq!(buffer.len(), HEADER + THEME.len() + 1);
        assert_eq!(events_in(&buffer), vec![Seen { wd: 3, name: THEME }]);

        // And with no NUL either — `len` covering exactly the name's bytes.
        let buffer = event(3, THEME.as_bytes(), THEME.len() as u32);
        assert_eq!(events_in(&buffer), vec![Seen { wd: 3, name: THEME }]);
    }

    /// `len` of zero is what an event about the watched thing itself carries — and what
    /// `IN_Q_OVERFLOW` carries. The header is still there and still has to be stepped over.
    #[test]
    fn a_zero_length_name_is_no_name_and_the_walk_carries_on() {
        let mut buffer = event(3, &[], 0);
        buffer.extend_from_slice(&honest(3, THEME));
        assert_eq!(
            events_in(&buffer),
            vec![Seen { wd: 3, name: "" }, Seen { wd: 3, name: THEME }]
        );
    }

    /// The longest name a filesystem can hand over, which is what the buffer is sized for.
    #[test]
    fn the_longest_name_the_kernel_can_send_is_read_whole() {
        let longest = "n".repeat(libc::NAME_MAX as usize);
        let buffer = honest(3, &longest);
        assert_eq!(events_in(&buffer), vec![Seen { wd: 3, name: longest.as_str() }]);
        // One event of it fits in a slot of the real buffer, which is what sizing it by NAME_MAX + 1
        // was for.
        assert!(buffer.len() <= HEADER + libc::NAME_MAX as usize + 1);
    }

    /// **The padding is `len`'s business and not the name's.** `len` counts the NUL and the alignment
    /// bytes after it, so a walk that took the field for the name would answer `"bru\0"` and match
    /// nothing, and a walk that stepped by the *name's* length would land mid-header on the next one.
    /// Both halves are asserted: the name is clean, and the event after it is found.
    #[test]
    fn the_nul_padding_belongs_to_the_length_and_not_to_the_name() {
        // "bru" is three bytes, so the field is four: the name, a NUL, and no more.
        assert_eq!(field(DIR).len(), 4);
        // "theme.css" is nine, so the field is twelve: nine, a NUL, and two bytes of padding.
        assert_eq!(field(THEME).len(), 12);

        let mut buffer = honest(3, THEME);
        buffer.extend_from_slice(&honest(3, DIR));
        let seen = events_in(&buffer);
        assert_eq!(seen, vec![Seen { wd: 3, name: THEME }, Seen { wd: 3, name: DIR }]);
        for one in &seen {
            assert!(!one.name.contains('\0'), "padding reached the name: {:?}", one.name);
        }
    }

    /// **A buffer cut mid-event reads what is whole and stops.** Every prefix of a good buffer is
    /// walked, and not one of them may panic — which is the property, because the alternative is a
    /// thread that dies and a theme that stops being noticed with no line saying why.
    #[test]
    fn a_truncated_buffer_stops_the_walk_rather_than_reading_past_it() {
        let mut whole = honest(3, THEME);
        whole.extend_from_slice(&honest(3, DIR));

        for cut in 0..whole.len() {
            let seen = events_in(&whole[..cut]);
            // Nothing invented, and nothing read that was not there.
            assert!(seen.len() <= 2, "{cut}: {seen:?}");
            if cut < HEADER {
                assert!(seen.is_empty(), "{cut}: a header was read out of {cut} bytes");
            }
        }

        // The two interesting cuts, named: a header with only part of its name behind it, and a whole
        // first event with half a header after it.
        let seen = events_in(&whole[..HEADER + 4]);
        assert_eq!(seen, vec![Seen { wd: 3, name: "" }], "a partial name is no name");
        let first = HEADER + field(THEME).len();
        let seen = events_in(&whole[..first + HEADER - 1]);
        assert_eq!(seen, vec![Seen { wd: 3, name: THEME }], "half a header is not an event");
    }

    /// **A `len` that points past the end of what was read.** Corrupt, or a buffer that was not
    /// written by the kernel at all. The name must not be read — that would be out of bounds — the
    /// walk must stop, and it must do both without panicking. `u32::MAX` is the case that would
    /// overflow an unchecked add on a 32-bit machine, which is why the add is checked.
    #[test]
    fn a_length_pointing_beyond_the_end_reads_nothing_past_it() {
        for lie in [u32::MAX, u32::MAX - 3, 4096, 13] {
            let buffer = event(3, field(THEME).as_slice(), lie);
            let seen = events_in(&buffer);
            assert_eq!(
                seen,
                vec![Seen { wd: 3, name: "" }],
                "len {lie} was honoured instead of refused",
            );
        }

        // And a lie in the *first* of two events takes the second with it: after a length that cannot
        // be trusted, nothing is known about where the next event starts.
        let mut buffer = event(3, &[], u32::MAX);
        buffer.extend_from_slice(&honest(3, THEME));
        assert_eq!(events_in(&buffer), vec![Seen { wd: 3, name: "" }]);

        // A `len` one byte too long is the boundary, and it is refused like any other.
        let padded = field(THEME);
        let buffer = event(3, &padded, padded.len() as u32 + 1);
        assert_eq!(events_in(&buffer), vec![Seen { wd: 3, name: "" }]);
    }

    /// **`wd = -1` is not "the watch that was never added".** It is what `IN_Q_OVERFLOW` carries, and
    /// `-1` is also what [`add`] answers for a directory it could not watch — so before this was
    /// guarded, an event with `wd = -1` naming `theme.css` and no watch on the directory compared
    /// equal and reported a rewrite that had not happened.
    #[test]
    fn an_impossible_watch_descriptor_is_not_the_watch_that_was_never_added() {
        let overflow = Seen { wd: -1, name: THEME };
        assert_eq!(meaning(&overflow, -1), None, "no watch, so nothing is for it");
        assert_eq!(meaning(&overflow, 3), None, "and it is not the watch that exists either");

        // The real thing still works: the watch that was added, and the name it was added for.
        assert_eq!(meaning(&Seen { wd: 3, name: THEME }, 3), Some(Meant::ThemeRewritten));
        // Another watch's event with the same name is another directory's `theme.css`.
        assert_eq!(meaning(&Seen { wd: 4, name: THEME }, 3), None);
        // The parent's watch, before the directory exists: the directory turning up is the event.
        assert_eq!(meaning(&Seen { wd: 9, name: DIR }, -1), Some(Meant::DirAppeared));
        // And once it is watched, a directory called `bru` inside it is not that event again.
        assert_eq!(meaning(&Seen { wd: 9, name: DIR }, 3), None);
    }

    /// **A name carrying a control character is refused, not repaired.**
    ///
    /// The direction is the point. `theme.css\n` is a *different file* — a legal one on Linux — and a
    /// parser that trimmed the newline would have bru re-read its theme because a neighbour was
    /// written. Same for an embedded NUL, which ends the name where it sits, and for bytes that are
    /// not UTF-8: a name bru cannot read is a name bru will not act on.
    #[test]
    fn a_name_carrying_a_control_character_is_refused_rather_than_repaired() {
        for hostile in ["theme.css\n", "theme.css\r", "theme\t.css", "\u{1b}theme.css"] {
            let buffer = honest(3, hostile);
            let seen = events_in(&buffer);
            assert_eq!(seen, vec![Seen { wd: 3, name: "" }], "{hostile:?} was let through");
            assert_eq!(meaning(&seen[0], 3), None, "{hostile:?} reported a rewrite");
        }

        // An embedded NUL ends the name, which is the padding rule and not a refusal: what precedes
        // it is the name. `theme.css\0anything` is `theme.css`, exactly as the kernel means it.
        let mut name_field = b"theme.css\0and-more".to_vec();
        while !name_field.len().is_multiple_of(ALIGN) {
            name_field.push(0);
        }
        let buffer = event(3, &name_field, name_field.len() as u32);
        assert_eq!(events_in(&buffer), vec![Seen { wd: 3, name: THEME }]);

        // Bytes that are not UTF-8 at all.
        let mut name_field = vec![0xff, 0xfe, b'x', 0];
        while !name_field.len().is_multiple_of(ALIGN) {
            name_field.push(0);
        }
        let buffer = event(3, &name_field, name_field.len() as u32);
        assert_eq!(events_in(&buffer), vec![Seen { wd: 3, name: "" }]);
    }

    /// An empty buffer is not an event, and neither is a buffer of zeroes shorter than a header.
    #[test]
    fn nothing_is_read_out_of_nothing() {
        assert!(events_in(&[]).is_empty());
        for short in 1..HEADER {
            assert!(events_in(&vec![0u8; short]).is_empty(), "{short} bytes became an event");
        }
        // A header of zeroes *is* an event — wd 0, no name — and stepping over it is what keeps a
        // buffer of zeroes from being walked for ever.
        assert_eq!(events_in(&[0u8; HEADER]), vec![Seen { wd: 0, name: "" }]);
        assert_eq!(events_in(&[0u8; HEADER * 3]).len(), 3);
    }
}
