//! C3: the terminal itself — raw mode, the kitty keyboard protocol, mouse reporting, sizing,
//! resize, and putting the terminal back on every exit path.
//!
//! ## The one promise this module makes
//!
//! **A browser that leaves the terminal in raw mode has failed at its one job as a citizen of that
//! terminal.** Everything else here is a feature; this is an obligation. So entering and leaving
//! are not two functions a caller is trusted to pair — they are one object, and the leaving happens
//! on *four* paths, three of which are not a return:
//!
//! | path                     | what runs it                                    |
//! |--------------------------|-------------------------------------------------|
//! | normal return, `?`, break| [`Drop for TerminalSession`]                    |
//! | a panic anywhere         | a `std::panic::set_hook` that **chains** the one it replaced |
//! | `kill`, `kill -INT`, ^C  | a `sigaction` handler that restores and re-raises with the default disposition |
//! | the terminal going away  | the same handler, on `SIGHUP`                   |
//!
//! The signal path is why the state the restore needs lives in `static` atomics and in **leaked**
//! allocations rather than in the session object: a handler may run at any instruction, including
//! one inside `Drop`, and the only things it is allowed to call are `write(2)` and `tcsetattr(3)`.
//! Both are async-signal-safe; a `Mutex` and a `Box::drop` are not. Leaking sixty bytes of `termios`
//! and one short escape string per process is the price of a restore that cannot read freed memory,
//! and it is the right price.
//!
//! ## `cfmakeraw` is not what this wants, and the difference has already cost a day
//!
//! Measured 2026-08-25 in the `--term-probe` spike (`term.rs`): with the pane drowning in queued
//! frames, `Ctrl-C` did not reach a process that had used `cfmakeraw` — because `cfmakeraw` clears
//! `ISIG`, which turns `Ctrl-C` from a signal into the byte `0x03` that nobody was reading. So this
//! module clears `ECHO` and `ICANON` in the local flags and the two flow-control bits in the input
//! flags, and leaves `ISIG` on. That is not a
//! compromise: with the kitty keyboard protocol pushed (below), the terminal sends `Ctrl-C` as an
//! escape sequence and no `SIGINT` is generated at all, so `ISIG` costs nothing in normal running
//! and is the escape hatch for every moment when the protocol is *not* active — before the push,
//! after the pop, and in whatever state a wedged renderer leaves things.
//!
//! What is deliberately *not* cleared, and what each one costs, is written down at [`raw_termios`].
//!
//! ## tmux: which escapes are for the multiplexer and which are for the terminal under it
//!
//! Under tmux there are two interpreters stacked, and sending an escape to the wrong one is a bug
//! that looks like a terminal that lacks a feature.
//!
//! - **Everything in this module is for tmux.** The alternate screen, the cursor, mouse reporting,
//!   synchronised output, in-band resize, the kitty keyboard protocol: tmux implements these, and
//!   they must apply to *this pane*. Wrapping them in a passthrough would set them on the outer
//!   terminal — for every pane at once, and they would still be set after bru exits.
//! - **kitty graphics (`ESC _ G`) is for the terminal underneath**, because tmux does not implement
//!   it and only forwards it. That belongs to C4, and [`passthrough`] is here for it to use.
//!
//! The one query that straddles the line is the cell size — see [`query_plan`].
//!
//! ## What the pane measures, on this machine
//!
//! Measured 2026-08-25, kitty 0.48.2 inside tmux next-3.8 with `allow-passthrough on`: the pane is
//! 990x1350 px and 110x54 cells, and **`TIOCGWINSZ` answers with the pixel numbers even under
//! tmux** — which the plan did not assume. 990/110 and 1350/54 are 9 and 25 exactly, so the cell is
//! 9x25. The `CSI 14t`/`CSI 16t` queries stay because a terminal that reports zero pixels is
//! ordinary (every non-kitty terminal did until recently), and because the ioctl is the only one of
//! the three that cannot be wrong about which pane it describes.

use std::io::Write;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Once, mpsc};

// -----------------------------------------------------------------------------------------------
// The escapes, as constants, so that enter and leave are visibly each other's inverse
// -----------------------------------------------------------------------------------------------

/// `DECSET 1049`: the alternate screen, which is also what gives the user their shell back with the
/// scrollback intact when bru exits. `1049` and not `47` or `1047`: it saves the cursor and clears
/// the alternate buffer as one atomic thing, which is what every terminal written this century
/// implements.
const ALT_SCREEN_ON: &str = "\x1b[?1049h";
const ALT_SCREEN_OFF: &str = "\x1b[?1049l";

/// The cursor bru does not own. It is hidden for the whole session: nothing in a rendered page
/// corresponds to a text cursor, and a blinking block parked wherever the last write left it is the
/// single most obvious sign of a program that is drawing pictures into a grid it does not
/// understand.
const CURSOR_HIDE: &str = "\x1b[?25l";
const CURSOR_SHOW: &str = "\x1b[?25h";

/// Mouse reporting, **encoding first and then the mode**.
///
/// `1016` is SGR-pixel encoding: reports carry pixel coordinates rather than cell coordinates, which
/// is the whole reason this is worth having — CEF wants a pixel inside a 990x1350 surface, and a
/// cell number would have to be multiplied back up by the cell size and would land the click up to
/// 9 px left and 25 px above where the user pointed.
///
/// **`1003` and not `1002`, and it was the other way round until a browser needed hover.** `1002`
/// reports motion only while a button is held, which is enough to drag and not enough to *point*:
/// Chromium never learns that the pointer moved, so `:hover` never fires, the cursor never changes
/// over a link, and a click on an element that expects a `mousemove` first does nothing at all.
/// Measured 2026-08-25 — clicks that "sometimes worked" were clicks on elements that did not need
/// the move. The cost is a report per pixel of pointer travel, and `term_input` answers that by
/// dropping the ones that did not change anything. The old reasoning follows, kept because the
/// trade it describes is real and is now simply decided the other way. `1003` was deliberately not
/// used: it is a report per mouse-move at the terminal's polling rate, and `:hover` is not worth a
/// flood of escape sequences through a pty.
///
/// The order matters and is not cosmetic. Setting `1016` first means the very first report is
/// already in the new encoding; the other way round leaves a window in which a click is delivered
/// in the old one and is parsed as a cell coordinate at pixel scale.
/// **`1006` first, and it is not redundant.** `1016` is an extension of SGR encoding and a terminal
/// that has never heard of it ignores the escape silently — leaving reports in the default X10 form,
/// which caps its coordinates at 223 and which this codebase does not read. Measured 2026-08-25 in
/// tmux, which speaks `1006` and not `1016`: with only `1016` asked for, not one click arrived in a
/// form bru could use. Asking for both means SGR either way and pixels where they are on offer.
const MOUSE_ON: &str = "\x1b[?1006h\x1b[?1016h\x1b[?1003h";
const MOUSE_OFF: &str = "\x1b[?1003l\x1b[?1016l\x1b[?1006l";

/// Ask whether the pixel encoding actually took: `CSI ? 1016 $ p`.
///
/// **The two encodings look identical on the wire** — `CSI < b ; x ; y M` either way — so the only
/// thing that tells cells from pixels is whether the terminal accepted the mode. Guessing from the
/// magnitude of the numbers works until somebody clicks in the top-left corner.
const MOUSE_PIXELS_QUERY: &str = "\x1b[?1016$p";

/// Synchronised output. C4 wraps a whole batch of surface uploads in these so the terminal commits
/// the frame in one go instead of showing the page updated and the chrome strips not yet. The
/// session only turns the *mode* off on the way out, in case a frame was interrupted mid-batch by
/// the exit — a terminal left with updates suspended is a terminal that has stopped drawing.
const SYNC_BEGIN: &str = "\x1b[?2026h";
const SYNC_END: &str = "\x1b[?2026l";

/// In-band resize reports (mode 2048): the terminal sends `CSI 48 ; rows ; cols ; height ; width t`
/// when the pane changes size, instead of the program finding out from `SIGWINCH` and then having
/// to ask how big it now is. Two things make it worth asking for even though `SIGWINCH` works:
/// the report carries the pixel size, which the signal does not, and it arrives *in order* with
/// respect to the bytes around it, so there is no window in which a frame is composed for the old
/// size and painted into the new one.
const IN_BAND_RESIZE_ON: &str = "\x1b[?2048h";
const IN_BAND_RESIZE_OFF: &str = "\x1b[?2048l";

/// Pop whatever this session pushed onto the terminal's keyboard-protocol stack.
///
/// The protocol is a *stack* on purpose: a program that pushes and pops cannot leave a terminal in a
/// mode its shell does not understand, however it exits, and nested programs compose. `CSI < u` pops
/// one entry, which is exactly the one `CSI > flags u` pushed.
const KITTY_POP: &str = "\x1b[<u";

/// The kitty keyboard protocol flags this frontend needs, as the sum the push escape carries.
///
/// - `1` **disambiguate escape codes** — `Esc` becomes a sequence of its own, so it is no longer
///   indistinguishable from the first byte of every other key. Without this, telling `<Esc>` from
///   `<Alt-x>` means a timeout, and a browser whose "leave insert mode" key has a timeout on it
///   feels broken.
/// - `2` **report event types** — press, repeat and release. CEF's `KeyEvent` has `KEYEVENT_KEYDOWN`
///   and `KEYEVENT_KEYUP` as distinct things, and a page that watches `keyup` is a page bru would
///   otherwise lie to.
/// - `4` **report alternate keys** — the shifted key and the *base-layout* key come with the event.
///   The base-layout key is the one C6 maps to `VKEY_*`, which is how a Cyrillic layout still hits
///   the same binding table as a Latin one without bru owning a layout database.
/// - `8` **report all keys as escape codes** — every key, including the ones that would otherwise
///   arrive as plain control bytes. This is what makes `Ctrl-I` distinguishable from `Tab` and
///   `Ctrl-M` from `Enter`, and it is also what stops `Ctrl-C` from becoming a `SIGINT` while it is
///   in force (see the module header).
///
/// `16` (report associated text) is **not** asked for: flag `4` already delivers the shifted
/// codepoint, which is the text for every key a binding cares about, and the associated-text field
/// duplicates it in a second place that C6 would then have to decide between.
const KITTY_FLAGS: u8 = 1 | 2 | 4 | 8;

/// How long a query gets before the terminal is taken to have no answer, in tenths of a second —
/// this is `VTIME`, so it is the gap between bytes and not a total.
///
/// All the queries go out in one write and are read back as one stream terminated by a `CSI 5n`
/// whose answer every terminal gives, so on a terminal that answers this costs nothing at all; the
/// timeout is only the tail for one that does not. 200 ms is chosen against an ssh session rather
/// than a local pty: 100 ms is a round trip that a busy link loses.
const QUERY_VTIME: libc::cc_t = 2;

// -----------------------------------------------------------------------------------------------
// Pure: building escape sequences
// -----------------------------------------------------------------------------------------------

/// Wrap a payload so tmux hands it to the terminal underneath instead of interpreting it.
///
/// The rule is a DCS `tmux;` wrapper with **every `\x1b` in the payload doubled**, terminated by
/// `\x1b\\`. Without `allow-passthrough on` set in tmux the whole thing is dropped, which looks
/// exactly like a terminal that does not support the feature — so a caller that gets no answer
/// cannot tell those two apart and must not report one as the other.
///
/// Nothing this module sends goes through here; see the module header for why. It is `pub(crate)`
/// for C4's graphics escapes, which are the only ones that must.
#[allow(dead_code)] // C4 is the caller and has not been written yet.
pub(crate) fn passthrough(payload: &str) -> String {
    let mut wrapped = String::with_capacity(payload.len() + payload.len() / 8 + 16);
    wrapped.push_str("\x1bPtmux;");
    for ch in payload.chars() {
        if ch == '\x1b' {
            wrapped.push('\x1b');
        }
        wrapped.push(ch);
    }
    wrapped.push_str("\x1b\\");
    wrapped
}

/// The push escape for a set of kitty keyboard flags: `CSI > flags u`.
#[allow(dead_code)] // Called by `enter`, which nothing calls until C2 wires `--term`.
pub(crate) fn kitty_push(flags: u8) -> String {
    format!("\x1b[>{flags}u")
}

/// Which modes a session actually turned on, so that leaving can be the exact inverse of entering
/// rather than a hopeful list of everything.
///
/// **This is a record and not a plan.** Turning a mode off that was never on is usually harmless and
/// occasionally not — popping a keyboard-protocol stack entry this process did not push takes away
/// the mode belonging to whatever program is underneath — so each field is set only once the escape
/// that sets it has been written.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(dead_code)] // Whole struct waits on C2/C5; the fields are read by `leave_sequence`.
pub(crate) struct Modes {
    pub(crate) alt_screen: bool,
    pub(crate) cursor_hidden: bool,
    pub(crate) mouse: bool,
    pub(crate) kitty_keyboard: bool,
    pub(crate) in_band_resize: bool,
}

/// Everything that has to be written to put the terminal back, in the reverse order of entering.
///
/// **Order is the content of this function.** The keyboard stack is popped before the alternate
/// screen goes away, because a pop that arrives after the shell has its screen back is a pop the
/// shell sees; the cursor is shown before the alternate screen is left, because the saved cursor
/// that `1049` restores is the one from before the session and showing it afterwards would be a
/// second, later decision about somebody else's cursor. `SYNC_END` is unconditional and first: it
/// costs one escape on a terminal that does not know the mode, and it is the difference between a
/// clean exit and a terminal frozen mid-frame if the exit interrupted a synchronised batch.
#[allow(dead_code)] // Used by `arm_restore` and the tests; the caller chain waits on C2.
pub(crate) fn leave_sequence(modes: Modes) -> String {
    let mut out = String::with_capacity(64);
    out.push_str(SYNC_END);
    if modes.kitty_keyboard {
        out.push_str(KITTY_POP);
    }
    if modes.in_band_resize {
        out.push_str(IN_BAND_RESIZE_OFF);
    }
    if modes.mouse {
        out.push_str(MOUSE_OFF);
    }
    if modes.cursor_hidden {
        out.push_str(CURSOR_SHOW);
    }
    if modes.alt_screen {
        out.push_str(ALT_SCREEN_OFF);
    }
    out
}

/// The queries, as one write.
///
/// One write and not four, because each query that goes out alone pays its own round trip, and a
/// terminal that answers none of them would then cost four timeouts of startup instead of one. The
/// last one is the trick that makes the tail disappear on a terminal that *does* answer: `CSI 5n`
/// (device status report) is answered by everything, including terminals that know none of the
/// four above it, so the reader stops as soon as `CSI 0 n` arrives rather than waiting for a gap.
///
/// The order is the order the replies arrive in, which the parsers do not depend on — they scan for
/// their own final byte — but which makes a captured session readable when one of them is wrong.
#[allow(dead_code)] // Used by `enter` and the tests.
pub(crate) fn query_sequence() -> String {
    let mut out = String::with_capacity(48);
    out.push_str("\x1b[?u"); // kitty keyboard: what flags are in force?
    out.push_str("\x1b[?2048$p"); // DECRQM: is in-band resize a mode you know?
    out.push_str("\x1b[14t"); // text area, in pixels
    out.push_str("\x1b[16t"); // one cell, in pixels
    out.push_str("\x1b[5n"); // and answer this last, whatever you did with the rest
    out
}

/// How to ask for the cell size a second time when the first ask went unanswered.
///
/// **This is the one place where the tmux rule from the module header bends, and only for `16t`.**
/// `CSI 14t` asks how many pixels the text area has, and the answer from the terminal *underneath*
/// tmux describes the whole window — every pane, plus tmux's own status line — so passing it
/// through would produce a number that is confidently wrong about this pane. `CSI 16t` asks how big
/// one cell is, which is a property of the font and is identical in every pane of every client of
/// that terminal, so the outer terminal's answer is this pane's answer.
///
/// With the cell size and the grid from `TIOCGWINSZ`, the pane's pixel size follows by
/// multiplication — which is why this fallback is worth the extra round trip at startup.
#[allow(dead_code)] // Used by `enter` and the tests.
pub(crate) fn query_plan(in_tmux: bool) -> Vec<String> {
    let mut plan = vec![query_sequence()];
    if in_tmux {
        plan.push(format!("{}\x1b[5n", passthrough("\x1b[16t")));
    }
    plan
}

// -----------------------------------------------------------------------------------------------
// Pure: reading the answers back
// -----------------------------------------------------------------------------------------------

/// Take the first complete `CSI ... <final>` sequence out of a buffer, and give back what was left.
///
/// **Every prefix of a valid sequence has to be a legal state here**, because a reply can be split
/// across reads at any byte and because a terminal is entitled to send things this module has never
/// heard of. An incomplete sequence is `None` with the buffer untouched, never a panic and never a
/// half-parsed answer; a *complete* sequence with the wrong final byte is skipped over and the scan
/// continues past it, so four replies can be pulled out of one buffer in any order.
///
/// The grammar is ECMA-48's: after `ESC [` come parameter bytes (`0x30`–`0x3F`), then intermediate
/// bytes (`0x20`–`0x2F`), then exactly one final byte (`0x40`–`0x7E`). Anything that leaves that
/// grammar is not a CSI and the scan restarts after the `ESC`.
#[allow(dead_code)] // Used by `enter` and the tests.
pub(crate) fn take_csi_reply(buf: &[u8], final_byte: u8) -> Option<(String, Vec<u8>)> {
    let mut at = 0;
    while at + 1 < buf.len() {
        if buf[at] != 0x1b || buf[at + 1] != b'[' {
            at += 1;
            continue;
        }
        let mut end = at + 2;
        while end < buf.len() && (0x20..=0x3f).contains(&buf[end]) {
            end += 1;
        }
        if end >= buf.len() {
            // A prefix: the parameters have not finished arriving. Not an error, not a match.
            return None;
        }
        if !(0x40..=0x7e).contains(&buf[end]) {
            // Not a CSI after all — an `ESC [` followed by something outside the grammar.
            at += 1;
            continue;
        }
        if buf[end] == final_byte {
            let reply = String::from_utf8_lossy(&buf[at..=end]).into_owned();
            let mut rest = Vec::with_capacity(buf.len() - (end - at + 1));
            rest.extend_from_slice(&buf[..at]);
            rest.extend_from_slice(&buf[end + 1..]);
            return Some((reply, rest));
        }
        at = end + 1;
    }
    None
}

/// The numeric parameters of a CSI reply, with the frame stripped off.
///
/// The frame is `ESC [`, an optional private-marker `?`, the parameters, an optional intermediate
/// `$`, and the final byte — which covers every reply this module asks for: `CSI ? 15 u`,
/// `CSI ? 2048 ; 1 $ y`, `CSI 4 ; 1350 ; 990 t` and `CSI 48 ; 54 ; 110 ; 1350 ; 990 t`.
///
/// Returns `None` rather than an empty vector for anything that is not shaped like a reply at all,
/// so that a caller cannot mistake "the terminal said nothing useful" for "the terminal said zero".
#[allow(dead_code)] // Used by the parsers below and the tests.
pub(crate) fn csi_params(reply: &str, final_byte: char) -> Option<Vec<u32>> {
    let body = reply.strip_prefix("\x1b[")?;
    let body = body.strip_suffix(final_byte)?;
    let body = body.strip_prefix('?').unwrap_or(body);
    let body = body.strip_suffix('$').unwrap_or(body);
    if body.is_empty() {
        return None;
    }
    body.split(';').map(|part| part.parse::<u32>().ok()).collect()
}

/// `CSI ? flags u` — which kitty keyboard flags are currently in force.
///
/// A terminal that does not implement the protocol answers nothing at all, which is `None` here and
/// is the *only* honest signal that the protocol is unavailable. There is no negative reply to wait
/// for, which is why this query travels with a `CSI 5n` behind it.
#[allow(dead_code)] // Used by `enter` and the tests.
pub(crate) fn parse_kitty_flags(reply: &str) -> Option<u8> {
    let params = csi_params(reply, 'u')?;
    let flags = *params.first()?;
    u8::try_from(flags).ok()
}

/// What a `DECRQM` reply says about a mode. The numbers are the standard's, and the distinction
/// that matters is `NotRecognised` — a terminal that has never heard of mode 2048 answers `0`, and
/// setting it anyway would be a mode nobody turns off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)] // Consumed by `enter`; the variants are matched in the tests.
pub(crate) enum ModeState {
    NotRecognised,
    Set,
    Reset,
    PermanentlySet,
    PermanentlyReset,
}

/// `CSI ? mode ; state $ y` — the answer to `CSI ? mode $ p`.
#[allow(dead_code)] // Used by `enter` and the tests.
pub(crate) fn parse_decrqm(reply: &str) -> Option<(u32, ModeState)> {
    let params = csi_params(reply, 'y')?;
    let mode = *params.first()?;
    let state = match *params.get(1)? {
        0 => ModeState::NotRecognised,
        1 => ModeState::Set,
        2 => ModeState::Reset,
        3 => ModeState::PermanentlySet,
        4 => ModeState::PermanentlyReset,
        _ => return None,
    };
    Some((mode, state))
}

/// `CSI 4 ; height ; width t` — the text area in pixels, the answer to `CSI 14t`.
///
/// **Height comes before width**, which is the opposite of the order everything else in this module
/// uses and is the standard's, not a choice. Returned as `(width, height)` so that no caller has to
/// remember that.
#[allow(dead_code)] // Used by `enter` and the tests.
pub(crate) fn parse_text_area_pixels(reply: &str) -> Option<(u32, u32)> {
    let params = csi_params(reply, 't')?;
    if *params.first()? != 4 || params.len() != 3 {
        return None;
    }
    let (height, width) = (params[1], params[2]);
    if width == 0 || height == 0 {
        return None;
    }
    Some((width, height))
}

/// `CSI 6 ; height ; width t` — one cell in pixels, the answer to `CSI 16t`. Height first again.
#[allow(dead_code)] // Used by `enter` and the tests.
pub(crate) fn parse_cell_size(reply: &str) -> Option<(u32, u32)> {
    let params = csi_params(reply, 't')?;
    if *params.first()? != 6 || params.len() != 3 {
        return None;
    }
    let (height, width) = (params[1], params[2]);
    if width == 0 || height == 0 {
        return None;
    }
    Some((width, height))
}

// The `CSI 48 … t` report itself is parsed in `term_keys.rs` (`Step::Resize`), because the byte
// stream it arrives in is the keyboard's and the parser that owns how many bytes a sequence took
// must be the one that reads it. What lives here is what to *do* with one — `size_from_in_band`
// and `note_in_band_resize` — so there is exactly one parser and exactly one piece of arithmetic.

// -----------------------------------------------------------------------------------------------
// Pure: what the pane is, from whatever the three sources managed to say
// -----------------------------------------------------------------------------------------------

/// Which of the three ways produced the pixel numbers. A reader of a size is owed this, because the
/// three disagree under a multiplexer and only one of them is certain to be about *this pane*.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)] // Recorded now, printed by C2's startup line and C5's diagnostics later.
pub(crate) enum SizeSource {
    /// `TIOCGWINSZ` said so. Always about this pty and nothing else — the only one that cannot be
    /// wrong about which pane it describes. Measured 2026-08-25: tmux fills these in.
    Ioctl,
    /// `CSI 14t` said so. About whatever answered, which under a multiplexer that forwards the
    /// query rather than answering it is the outer window.
    TextAreaQuery,
    /// Neither knew, so the grid from the ioctl was multiplied by the cell size. Exact when the
    /// terminal has no padding around the text area, and low by up to a cell when it has.
    CellGrid,
    /// A mode 2048 report said so. The best of the four: it is about this pane, it carries the grid
    /// and the pixels in the same message, and it describes the size *at the instant of the resize*
    /// rather than whenever the reader got round to asking.
    InBandReport,
}

/// What the pane is: the grid, the pixels, and the cell that relates them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)] // The fields are read by C4/C5; the struct is built and compared here.
pub(crate) struct PaneSize {
    pub(crate) rows: u16,
    pub(crate) cols: u16,
    /// The text area in pixels — what a frame may be, and what CEF's surface is sized to.
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) cell_width: u32,
    pub(crate) cell_height: u32,
    pub(crate) source: SizeSource,
}

/// One size out of the three sources, in the order of how much each can be trusted.
///
/// The order is `TIOCGWINSZ` pixels, then `CSI 14t`, then the grid times the cell — and it is that
/// order because it is the order of *certainty about which pane is being described*, not the order
/// of precision. The ioctl is asked of this process's own pty by the kernel; the query is answered
/// by whoever is listening, which under tmux may be the outer terminal describing a window several
/// times this size.
///
/// **The cell size, when it has to be derived, is derived and never used backwards.** `width / cols`
/// throws away a remainder — 993 px over 110 cells is 9 with 3 px of the terminal's own padding
/// lost — and multiplying that 9 back up would report a pane 3 px narrower than it is. So a derived
/// cell is recorded for laying out text-shaped things, and `width`/`height` keep whatever the
/// source actually said.
///
/// `None` means there is no grid, which means this is not a terminal and there is nothing to
/// negotiate.
#[allow(dead_code)] // Used by `enter`, the resize thread and the tests.
pub(crate) fn compose_size(
    rows: u16,
    cols: u16,
    ioctl_pixels: (u16, u16),
    text_area: Option<(u32, u32)>,
    cell: Option<(u32, u32)>,
) -> Option<PaneSize> {
    if rows == 0 || cols == 0 {
        return None;
    }
    let (xpixel, ypixel) = ioctl_pixels;
    let (width, height, source) = if xpixel > 0 && ypixel > 0 {
        (u32::from(xpixel), u32::from(ypixel), SizeSource::Ioctl)
    } else if let Some((width, height)) = text_area.filter(|(w, h)| *w > 0 && *h > 0) {
        (width, height, SizeSource::TextAreaQuery)
    } else if let Some((cell_width, cell_height)) = cell.filter(|(w, h)| *w > 0 && *h > 0) {
        (cell_width * u32::from(cols), cell_height * u32::from(rows), SizeSource::CellGrid)
    } else {
        return None;
    };
    // A reported cell is believed over a derived one even when the pane's pixels came from the
    // ioctl: the terminal knows its font metrics, and the division only agrees with it when there
    // is no padding.
    let (cell_width, cell_height) = match cell.filter(|(w, h)| *w > 0 && *h > 0) {
        Some(reported) => reported,
        None => ((width / u32::from(cols)).max(1), (height / u32::from(rows)).max(1)),
    };
    Some(PaneSize { rows, cols, width, height, cell_width, cell_height, source })
}

/// The pane, out of a mode 2048 report and the cell size already known.
///
/// Separate from [`compose_size`] rather than another argument to it, because the report is the one
/// source that needs no reconciling: it carries the grid and the pixels together and both are about
/// this pane. A report that carries no pixels — the escape's last two parameters are allowed to be
/// zero — falls back to the grid times the cell, which is [`compose_size`]'s last resort and is
/// reached through it so there is one implementation of that arithmetic.
#[allow(dead_code)] // Called by `note_in_band_resize`, which C-input reaches in its own phase.
pub(crate) fn size_from_in_band(
    rows: u16,
    cols: u16,
    width: u32,
    height: u32,
    cell: Option<(u32, u32)>,
) -> Option<PaneSize> {
    if width > 0 && height > 0 {
        let mut size = compose_size(rows, cols, (0, 0), Some((width, height)), cell)?;
        size.source = SizeSource::InBandReport;
        return Some(size);
    }
    compose_size(rows, cols, (0, 0), None, cell)
}

/// Whether a newly measured size is worth waking the compositor for.
///
/// **Source is deliberately not part of the comparison.** The same pane can be reported by the
/// ioctl on one event and by a `CSI 14t` fallback on the next, and a layout recomputed because the
/// provenance changed is a layout recomputed for nothing — while a *real* resize under mode 2048
/// and `SIGWINCH` both arrive, and this is what collapses them into one event.
#[allow(dead_code)] // Used by the resize thread, `note_in_band_resize` and the tests.
pub(crate) fn size_changed(last: Option<PaneSize>, next: PaneSize) -> bool {
    match last {
        None => true,
        Some(last) => {
            last.rows != next.rows
                || last.cols != next.cols
                || last.width != next.width
                || last.height != next.height
        }
    }
}

// -----------------------------------------------------------------------------------------------
// termios
// -----------------------------------------------------------------------------------------------

/// The two bits that come off, and the list of the ones that deliberately do not.
///
/// `cfmakeraw` would clear ten. Each of the eight left alone is left alone for a reason, and the
/// reasons are worth writing down because "raw mode" is otherwise a spell rather than a decision:
///
/// - **`ISIG`** — the one the spike got wrong on 2026-08-25 and the module header explains. Stays.
/// - **`IEXTEN`** — `Ctrl-V`'s literal-next quoting. Harmless with `ICANON` off, and turning it off
///   would be a change nobody can name a consequence of.
/// - **`IXON`/`IXOFF`** — software flow control. **Cleared, and the deferred decision this comment
///   used to describe is now made.** `<Ctrl-Q>` is bound to `quit` in `config.rs`, and a key the
///   line discipline swallows as XON can never reach a binding: it starts the terminal's output
///   again and goes no further. `Ctrl-S` stops being a way to make the pane look hung, which is the
///   other half of the same bargain and no loss — it is bound to nothing, so it now does nothing.
///   The frontend is a full-screen application and does not want the terminal deciding when it may
///   write.
/// - **`ICRNL`** — CR to NL on input. Same argument: with flag 8 pushed, `Enter` is a sequence.
/// - **`OPOST`** — output post-processing, so a lone `\n` still gets its carriage return. Every
///   escape this module and C4 write is absolutely positioned or self-terminating, so post
///   processing has nothing to do to them, and leaving it on means a `println!` on an error path
///   still lands where a person can read it instead of staircasing off the right edge.
/// - **`CSIZE`/`PARENB`** — the byte width and parity of a *serial line*. This is a pty.
///
/// `VMIN = 0` with `VTIME` set means a read waits at most that long and then returns nothing, which
/// is what keeps a terminal that never answers a query from hanging startup.
#[allow(dead_code)] // Used by `enter` and by the termios test.
pub(crate) fn raw_termios(saved: libc::termios, vtime: libc::cc_t) -> libc::termios {
    let mut raw = saved;
    raw.c_lflag &= !(libc::ECHO | libc::ICANON);
    raw.c_iflag &= !(libc::IXON | libc::IXOFF);
    raw.c_cc[libc::VMIN] = 0;
    raw.c_cc[libc::VTIME] = vtime;
    raw
}

// -----------------------------------------------------------------------------------------------
// The restore, and the three paths that are not a return
// -----------------------------------------------------------------------------------------------

/// Whether there is a terminal to put back. Swapped to `false` by whichever path restores first, so
/// that a `Drop` running after a signal handler already restored does not write escapes into a
/// terminal that has moved on.
static ARMED: AtomicBool = AtomicBool::new(false);

/// The descriptor `tcsetattr` is called on. `-1` when nothing is armed.
static RESTORE_TTY: AtomicI32 = AtomicI32::new(-1);

/// The descriptor the leave escapes are written to.
static RESTORE_OUT: AtomicI32 = AtomicI32::new(-1);

/// The saved `termios`, **leaked on purpose**.
///
/// A signal can arrive between any two instructions, including instructions inside `Drop`. If this
/// were freed on the way out, a `SIGTERM` landing in that window would hand `tcsetattr` a pointer
/// into freed memory — and the whole point of this machinery is that the restore is the one thing
/// that cannot go wrong. One `termios` is sixty bytes and a process enters a terminal session once.
static RESTORE_TERMIOS: AtomicPtr<libc::termios> = AtomicPtr::new(std::ptr::null_mut());

/// The bytes that undo the modes, republished (and leaked, for the reason above) each time another
/// mode is turned on, so that a signal arriving halfway through `enter` still undoes exactly the
/// modes that were on at that instant.
static LEAVE_BYTES: AtomicPtr<u8> = AtomicPtr::new(std::ptr::null_mut());
static LEAVE_LEN: AtomicUsize = AtomicUsize::new(0);

/// The write end of the `SIGWINCH` self-pipe. `-1` when there is no session.
static WINCH_PIPE: AtomicI32 = AtomicI32::new(-1);

/// Installed once per process, however many sessions come and go.
static HOOKS: Once = Once::new();

/// Put the terminal back. Safe to call from a signal handler, from a panic hook and from `Drop`,
/// and it does its work at most once.
///
/// **Everything it touches is async-signal-safe and nothing it touches allocates.** `write(2)` and
/// `tcsetattr(3)` are both on POSIX's list; the pointers come out of atomics; the memory they point
/// at is leaked and therefore cannot be freed underneath the handler.
fn restore_now() {
    if !ARMED.swap(false, Ordering::AcqRel) {
        return;
    }
    let out = RESTORE_OUT.load(Ordering::Acquire);
    // Length first and pointer second — the opposite order to [`publish_leave`]'s stores, and see
    // the note there for why that is what keeps this from reading past the end of a buffer that was
    // being replaced at the instant the signal arrived.
    let len = LEAVE_LEN.load(Ordering::Acquire);
    let bytes = LEAVE_BYTES.load(Ordering::Acquire);
    if out >= 0 && !bytes.is_null() {
        let mut written = 0usize;
        while written < len {
            // SAFETY: `bytes` points at a leaked allocation of `len` bytes that is never freed and
            // never written after publication, and `written < len` keeps the offset inside it.
            let wrote = unsafe {
                libc::write(out, bytes.add(written).cast::<libc::c_void>(), len - written)
            };
            if wrote <= 0 {
                break; // A terminal that will not take bytes is one there is nothing more to do for.
            }
            written += wrote as usize;
        }
    }
    let tty = RESTORE_TTY.load(Ordering::Acquire);
    let termios = RESTORE_TERMIOS.load(Ordering::Acquire);
    if tty >= 0 && !termios.is_null() {
        // SAFETY: `termios` points at a leaked copy of what `tcgetattr` filled in for this same
        // descriptor, and `TCSANOW` reads it without retaining it.
        unsafe { libc::tcsetattr(tty, libc::TCSANOW, termios) };
    }
}

/// Put the terminal back from a signal handler that lives outside this module.
///
/// `term_frontend`'s interrupt handler takes over `SIGINT`/`SIGTERM` so the first signal can ask
/// for a clean shutdown — and its *second*-signal path re-raises with the default disposition,
/// which kills the process. Re-raising without restoring first would leave the shell in raw mode
/// on the alternate screen: exactly the failure this module exists to make impossible, reachable
/// again through a handler it did not install. [`restore_now`] is async-signal-safe (see its
/// comment), so handing it out costs nothing and closes that path.
pub(crate) fn restore_for_signal() {
    restore_now();
}

/// The handler for the signals that mean "this process is ending".
///
/// It restores and then **re-raises with the default disposition**, rather than calling `exit`. That
/// is the difference between a process that died of `SIGTERM` and one that chose to stop: the shell
/// that started bru, and anything scripting it, reads the wait status, and a `_exit(0)` here would
/// tell it the browser exited cleanly when it was killed.
extern "C" fn on_terminating_signal(signum: libc::c_int) {
    restore_now();
    // SAFETY: `signal` and `raise` are both async-signal-safe. Restoring the default disposition
    // first is what makes the re-raise terminate rather than re-enter this handler; the signal is
    // blocked for the duration of the handler, so it is delivered on return.
    unsafe {
        libc::signal(signum, libc::SIG_DFL);
        libc::raise(signum);
    }
}

/// The `SIGWINCH` handler: one byte down a pipe and nothing else.
///
/// **The self-pipe is the whole design.** A handler may not lock, allocate, or call into `mpsc`, so
/// it cannot deliver a resize event; what it can do is `write(2)` one byte, which is on POSIX's
/// safe list, and let an ordinary thread blocked on the read end do the work. The pipe's write end
/// is non-blocking, so a burst of resizes during a window drag fills the buffer and the extra
/// writes fail with `EAGAIN` — which is the correct outcome and not a lost event: the reader asks
/// the kernel for the current size, so one byte and fifty bytes mean the same thing.
extern "C" fn on_winch(_signum: libc::c_int) {
    let fd = WINCH_PIPE.load(Ordering::Acquire);
    if fd < 0 {
        return;
    }
    let byte = b"\x01";
    // SAFETY: `write` is async-signal-safe; the buffer is a static one byte long; a closed or
    // reused descriptor can only make this fail, which is ignored deliberately.
    unsafe { libc::write(fd, byte.as_ptr().cast::<libc::c_void>(), 1) };
}

/// Install a handler for one signal.
fn install_handler(signum: libc::c_int, handler: extern "C" fn(libc::c_int)) {
    // SAFETY: `sigaction` is a plain C struct with no invalid bit patterns; `sigemptyset` fills the
    // mask; the handler is an `extern "C" fn` with the signature the kernel calls. The old
    // disposition is discarded on purpose — this is a process-wide decision made once.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = handler as usize;
        libc::sigemptyset(&mut action.sa_mask);
        // `SA_RESTART` so that a `SIGWINCH` during a blocking read restarts the read instead of
        // handing every caller in this process an `EINTR` to think about.
        action.sa_flags = libc::SA_RESTART;
        libc::sigaction(signum, &action, std::ptr::null_mut());
    }
}

/// Publish the bytes that undo `modes`, so the signal path has them.
///
/// Called again after every mode is turned on. The previous buffer is leaked rather than freed for
/// the reason [`RESTORE_TERMIOS`] gives: a handler may already be reading it. Five leaks of under
/// forty bytes, once per process.
fn publish_leave(modes: Modes) {
    let bytes: Box<[u8]> = leave_sequence(modes).into_bytes().into_boxed_slice();
    let len = bytes.len();
    let ptr = Box::into_raw(bytes).cast::<u8>();
    // **The pointer is published before the length, and [`restore_now`] reads them the other way
    // round.** That pairing is what makes a signal landing between the two stores safe, and it
    // relies on one invariant: while a session is armed, this is only ever called with *more* modes
    // than last time, so the buffer only grows. A handler then sees one of three things — the old
    // pair, the new pair, or the old (shorter) length against the new (longer) buffer, which writes
    // a truncated escape into a terminal that is about to be gone. What it can never see is a
    // length longer than the buffer it is reading, which is the only one of the four that is a
    // read past the end.
    LEAVE_BYTES.store(ptr, Ordering::Release);
    LEAVE_LEN.store(len, Ordering::Release);
}

// -----------------------------------------------------------------------------------------------
// The session
// -----------------------------------------------------------------------------------------------

/// The terminal, for as long as this object lives.
///
/// Created by [`TerminalSession::enter`] and undone by every path out of the process. Nothing else
/// in bru writes an escape to stdout while one of these exists — C4's presenter goes through it,
/// because the modes it depends on are the ones recorded here.
#[allow(dead_code)] // The whole type waits on C2 wiring `--term`; the fields are read internally.
pub(crate) struct TerminalSession {
    tty: RawFd,
    out: RawFd,
    in_tmux: bool,
    modes: Modes,
    size: PaneSize,
    /// The flags the terminal reported *before* the push, kept so that a diagnostic can say whether
    /// the protocol was already in use by something outside bru — a tmux with its own extended-keys
    /// setting, for instance.
    kitty_flags_before: Option<u8>,
    /// Whether mouse reports carry pixels rather than cells — see [`MOUSE_PIXELS_QUERY`].
    mouse_pixels: bool,
    /// The way of handing over a frame that the terminal agreed to.
    transport: crate::term_paint::Transport,
    /// Bytes read while waiting for query answers that were not answers.
    ///
    /// **These are keystrokes and they are not to be thrown away.** A person who typed while bru was
    /// starting typed at bru; the query reader has stdin at that moment and would otherwise eat
    /// them. C-input takes this and feeds it to the parser before its first `read`.
    pushback: Vec<u8>,
    last_size: Arc<Mutex<Option<PaneSize>>>,
    resize_tx: mpsc::Sender<PaneSize>,
    resize_rx: Option<mpsc::Receiver<PaneSize>>,
    winch_read: RawFd,
    winch_write: RawFd,
    stop: Arc<AtomicBool>,
    winch_thread: Option<std::thread::JoinHandle<()>>,
}

#[allow(dead_code)] // See the note on the struct.
impl TerminalSession {
    /// Take the terminal.
    ///
    /// The order is chosen so that nothing a user can see happens on the terminal they were using:
    /// raw mode and the restore are armed first, the alternate screen goes up second, and only then
    /// are the queries written — so the answers a terminal does not send, and the junk one sends
    /// instead, land on a screen that is thrown away rather than in the middle of their shell
    /// history.
    pub(crate) fn enter() -> Result<TerminalSession, String> {
        let tty = std::io::stdin().as_raw_fd();
        let out = std::io::stdout().as_raw_fd();
        // SAFETY: `isatty` only inspects the descriptor.
        if unsafe { libc::isatty(tty) } != 1 || unsafe { libc::isatty(out) } != 1 {
            return Err("stdin and stdout are not a terminal, so there is no terminal to run in"
                .to_string());
        }
        if ARMED.load(Ordering::Acquire) {
            return Err("a terminal session is already running in this process".to_string());
        }

        // SAFETY: `termios` is a plain C struct with no invalid bit patterns, and `tcgetattr` either
        // fills it or reports failure without touching it.
        let mut saved: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(tty, &mut saved) } != 0 {
            return Err("could not read the terminal's settings".to_string());
        }
        let raw = raw_termios(saved, QUERY_VTIME);
        // SAFETY: `raw` was derived from what `tcgetattr` gave us for this descriptor.
        if unsafe { libc::tcsetattr(tty, libc::TCSANOW, &raw) } != 0 {
            return Err("could not put the terminal in raw mode".to_string());
        }

        // Armed before a single escape is written, and before anything below can fail: from here on,
        // every way out of this process goes through `restore_now`.
        RESTORE_TERMIOS.store(Box::into_raw(Box::new(saved)), Ordering::Release);
        RESTORE_TTY.store(tty, Ordering::Release);
        RESTORE_OUT.store(out, Ordering::Release);
        let mut modes = Modes::default();
        publish_leave(modes);
        ARMED.store(true, Ordering::Release);
        install_hooks();

        let in_tmux = std::env::var_os("TMUX").is_some_and(|value| !value.is_empty());
        let mut stdout = std::io::stdout();
        let mut write = |text: &str| -> Result<(), String> {
            stdout
                .write_all(text.as_bytes())
                .and_then(|()| stdout.flush())
                .map_err(|e| format!("could not write to the terminal: {e}"))
        };

        // **From here on a failure has to put the terminal back before it reports itself.**
        // Measured 2026-08-25: `enter` switched to the alternate screen, failed on the size two
        // dozen lines later, and returned the reason to a caller whose `eprintln!` went to that
        // alternate screen — which was then cleared, and never left. The user saw a terminal that
        // had gone quiet and said nothing. A constructor that fails half-built owes the unwinding,
        // and `restore_now` is exactly the unwinding every other exit path already uses.
        let unwind = |why: String| -> String {
            restore_now();
            why
        };

        write(ALT_SCREEN_ON).map_err(unwind)?;
        modes.alt_screen = true;
        publish_leave(modes);
        write(CURSOR_HIDE).map_err(unwind)?;
        modes.cursor_hidden = true;
        publish_leave(modes);

        // --- ask everything at once, then read one stream -----------------------------------------
        let mut answers = Vec::new();
        for round in query_plan(in_tmux) {
            write(&round).map_err(unwind)?;
            answers.extend_from_slice(&read_answers(tty));
        }

        let (kitty_reply, answers) = split_reply(answers, b'u');
        let (decrqm_reply, answers) = split_reply(answers, b'y');
        let (first_t, answers) = split_reply(answers, b't');
        let (second_t, leftover) = split_reply(answers, b't');
        let (third_t, leftover) = split_reply(leftover, b't');

        let kitty_flags_before = kitty_reply.as_deref().and_then(parse_kitty_flags);
        let in_band_supported = decrqm_reply
            .as_deref()
            .and_then(parse_decrqm)
            .is_some_and(|(mode, state)| mode == 2048 && state != ModeState::NotRecognised);
        // The two `t` replies are told apart by their first parameter and not by their order, so a
        // terminal that answers `16t` before `14t` — or answers only one of them — is read
        // correctly. The third slot exists for the tmux passthrough round's extra cell-size answer.
        let replies: Vec<&str> =
            [&first_t, &second_t, &third_t].iter().filter_map(|r| r.as_deref()).collect();
        let text_area = replies.iter().find_map(|r| parse_text_area_pixels(r));
        let cell = replies.iter().find_map(|r| parse_cell_size(r));

        let (rows, cols, xpixel, ypixel) = winsize(out)
            .ok_or_else(|| unwind("the kernel reports no window size for this terminal".to_string()))?;
        let size = compose_size(rows, cols, (xpixel, ypixel), text_area, cell).ok_or_else(|| {
            unwind(
                "the terminal reports no size in pixels and no cell size, so there is no way to \
                 know how big a page may be drawn"
                    .to_string(),
            )
        })?;

        // --- the modes that depend on the answers -------------------------------------------------
        if kitty_flags_before.is_some() {
            write(&kitty_push(KITTY_FLAGS)).map_err(unwind)?;
            modes.kitty_keyboard = true;
            publish_leave(modes);
        }
        write(MOUSE_ON).map_err(unwind)?;
        modes.mouse = true;
        publish_leave(modes);
        // **With a `CSI 5n` behind it, like every other query round.** Alone, the DECRQM reply ends
        // in `y` and `read_answers` stops only on `n` — so every startup paid one full `VTIME`
        // timeout here waiting for an answer that had already arrived. The status report is what
        // ends the read the moment the terminal is done talking.
        write(&format!("{MOUSE_PIXELS_QUERY}\x1b[5n")).map_err(unwind)?;
        // **And the leftover is keystrokes, the same as the first rounds'.** This round's residue
        // used to be dropped, which re-opened — for the milliseconds this query takes — exactly the
        // hole `pushback` exists to close: a key typed while the terminal was being asked questions
        // vanished. Everything that was not the reply joins the pushback.
        let (pixels_reply, mouse_leftover) = split_reply(read_answers(tty), b'y');
        let mut pushback = leftover;
        pushback.extend_from_slice(&mouse_leftover);
        let mouse_pixels = pixels_reply
            .as_deref()
            .and_then(parse_decrqm)
            .is_some_and(|(mode, state)| {
                mode == 1016 && matches!(state, ModeState::Set | ModeState::PermanentlySet)
            });
        if in_band_supported {
            write(IN_BAND_RESIZE_ON).map_err(unwind)?;
            modes.in_band_resize = true;
            publish_leave(modes);
        }

        // --- which frame will this terminal take? -------------------------------------------------
        // **Asked, because a real frame never asks.** Frames carry `q=2` and get silence by design,
        // so a host that refuses a transport refuses all of them without bru ever hearing it — a
        // black pane at full frame rate. Measured 2026-08-26: zellij 0.46.0 answers
        // `ENOTSUPPORTED:shared memory transfer is not supported` and takes a file instead.
        //
        // The order is by cost: shared memory is a `memcpy` and sixty bytes, a file in tmpfs is the
        // same write through a path, and base64 is ten times the bytes on the wire and a tenth of
        // the frame rate. See `term_paint::medium_query_escape`.
        let mut ask = |medium: char, id: u32, name: &str| -> Option<bool> {
            let escape = crate::term_paint::medium_query_escape(medium, id, name, in_tmux);
            if write(&format!("{escape}\x1b[5n")).is_err() {
                return None;
            }
            let answered = read_answers(tty);
            let verdict = crate::term_paint::query_answer(&answered, id);
            // Everything that was not the answer is somebody's keystrokes, as in every round.
            pushback.extend_from_slice(&answered);
            verdict
        };
        let shared = crate::term_paint::shm_probe_publish().and_then(|name| {
            let verdict = ask('s', crate::term_paint::image_id_probe_shm(), &name);
            crate::term_paint::shm_probe_release(&name);
            verdict
        });
        // Silence says nothing, and the transport this frontend was written on is the better guess
        // than one chosen from an absence. Only a refusal moves on to the next question.
        let transport = if shared != Some(false) {
            crate::term_paint::Transport::SharedMemory
        } else {
            let file = crate::term_paint::file_probe_publish().and_then(|path| {
                let verdict =
                    ask('f', crate::term_paint::image_id_probe_file(), &path.to_string_lossy());
                crate::term_paint::file_probe_release(&path);
                verdict
            });
            if file == Some(true) {
                crate::term_paint::Transport::File
            } else {
                crate::term_paint::Transport::Base64
            }
        };

        // --- resize, from either direction, into one channel --------------------------------------
        let (winch_read, winch_write) = self_pipe().map_err(unwind)?;
        WINCH_PIPE.store(winch_write, Ordering::Release);
        install_handler(libc::SIGWINCH, on_winch);

        let (resize_tx, resize_rx) = mpsc::channel();
        let last_size = Arc::new(Mutex::new(Some(size)));
        let stop = Arc::new(AtomicBool::new(false));
        let winch_thread = spawn_winch_thread(
            winch_read,
            out,
            cell,
            Arc::clone(&last_size),
            resize_tx.clone(),
            Arc::clone(&stop),
        );

        Ok(TerminalSession {
            tty,
            out,
            in_tmux,
            modes,
            size,
            kitty_flags_before,
            mouse_pixels,
            transport,
            pushback,
            last_size,
            resize_tx,
            resize_rx: Some(resize_rx),
            winch_read,
            winch_write,
            stop,
            winch_thread: Some(winch_thread),
        })
    }

    /// The pane as it was when the session started. C5 asks the channel for anything later.
    pub(crate) fn size(&self) -> PaneSize {
        self.size
    }

    pub(crate) fn in_tmux(&self) -> bool {
        self.in_tmux
    }

    /// The transport the terminal agreed to, rather than the one this machine could publish.
    pub(crate) fn transport(&self) -> crate::term_paint::Transport {
        self.transport
    }

    /// Which modes are actually in force. C4 reads `in_band_resize` and the sync flag through the
    /// helpers below rather than assuming a terminal took what it was offered.
    pub(crate) fn modes(&self) -> Modes {
        self.modes
    }

    /// The keyboard-protocol flags the terminal reported before this session pushed its own.
    /// `None` means the terminal never answered `CSI ? u`, which is how it says it has no such
    /// protocol — and C6 has to fall back to reading plain control bytes.
    /// Whether a mouse report's `x`/`y` are pixels. `false` means cells, and the caller multiplies
    /// by [`PaneSize::cell_width`] and [`PaneSize::cell_height`] to find out where the pointer is.
    pub(crate) fn mouse_in_pixels(&self) -> bool {
        self.mouse_pixels
    }

    pub(crate) fn kitty_flags_before(&self) -> Option<u8> {
        self.kitty_flags_before
    }

    /// Keystrokes that arrived while the queries were being answered. Empty in the ordinary case,
    /// and to be fed to the input parser before its first `read`. Takes them, so a second call is
    /// empty and nothing can deliver them twice.
    pub(crate) fn take_pushback(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pushback)
    }

    /// The one channel every "the size changed" event arrives on, whichever way it was noticed.
    /// Takeable once, because two receivers on one channel would each get half the resizes.
    pub(crate) fn resize_events(&mut self) -> Option<mpsc::Receiver<PaneSize>> {
        self.resize_rx.take()
    }

    /// An in-band report (mode 2048) the input parser pulled out of stdin, on its way to the same
    /// channel `SIGWINCH` feeds.
    ///
    /// **The two are deduplicated against each other and that is the point of routing them here.**
    /// A terminal that supports mode 2048 usually also sends `SIGWINCH`, so a single drag of a
    /// window edge produces both — and the compositor must recompute a layout once per size, not
    /// once per notification.
    pub(crate) fn note_in_band_resize(&self, rows: u16, cols: u16, width: u32, height: u32) {
        let cell = Some((self.size.cell_width, self.size.cell_height));
        if let Some(size) = size_from_in_band(rows, cols, width, height, cell) {
            emit_if_changed(&self.last_size, &self.resize_tx, size);
        }
    }

    /// Begin a synchronised frame. C4 wraps a whole batch of surface uploads in this pair so the
    /// terminal shows the page and both chrome strips updated together, rather than the page first.
    pub(crate) fn begin_sync(&self, out: &mut impl Write) -> std::io::Result<()> {
        out.write_all(SYNC_BEGIN.as_bytes())
    }

    /// End a synchronised frame and let the terminal draw it.
    pub(crate) fn end_sync(&self, out: &mut impl Write) -> std::io::Result<()> {
        out.write_all(SYNC_END.as_bytes())?;
        out.flush()
    }

    /// Read and throw away anything the terminal still has to say, before it is handed back.
    ///
    /// **A reply nobody reads is a reply the shell reads.** bru asks for silence on every frame
    /// (`q=2`), but a terminal that could not parse a command answers anyway — it has to, because
    /// the quiet flag it could not read is part of the command it could not read. That answer
    /// arrives on the same stdin the key reader owned, and if bru has already gone, the next thing
    /// holding the terminal gets it: `NVAL:invalid\svalue\sfor\skey\s'q'` printed at a shell
    /// prompt, reported 2026-08-27.
    ///
    /// Bounded twice over — by the read count and by `VTIME`, which is still in force here — so a
    /// terminal that says nothing costs one timeout and one that will not stop costs no more than
    /// this loop.
    pub(crate) fn drain_replies(&self) {
        let mut chunk = [0u8; 256];
        for _ in 0..16 {
            // SAFETY: reading into a buffer this function owns, from the terminal descriptor this
            // session has held since `enter` established it is a tty. `VMIN 0`/`VTIME` make this
            // return 0 on a timeout rather than block.
            let read = unsafe {
                libc::read(self.tty, chunk.as_mut_ptr().cast::<libc::c_void>(), chunk.len())
            };
            if read <= 0 {
                break;
            }
        }
    }

    /// Put the terminal back now, before the object goes away. Idempotent; `Drop` calls it too.
    pub(crate) fn leave(&mut self) {
        self.shut_down();
    }

    fn shut_down(&mut self) {
        if self.tty < 0 {
            return;
        }
        // The handler must stop writing to the pipe *before* the descriptors go, or a resize during
        // shutdown writes to a number the kernel has already handed to somebody else.
        WINCH_PIPE.store(-1, Ordering::Release);
        // SAFETY: restoring the default disposition for a signal this process installed a handler
        // for. Doing it before the thread is joined means a late `SIGWINCH` is ignored rather than
        // dispatched into a handler whose pipe is gone.
        unsafe { libc::signal(libc::SIGWINCH, libc::SIG_DFL) };
        self.stop.store(true, Ordering::Release);
        // One byte to wake the reader out of its blocking read so it can see the flag.
        let byte = b"\x01";
        // SAFETY: `winch_write` is this session's pipe and is still open here.
        unsafe { libc::write(self.winch_write, byte.as_ptr().cast::<libc::c_void>(), 1) };
        if let Some(thread) = self.winch_thread.take() {
            let _ = thread.join();
        }
        // SAFETY: both ends came from this session's `pipe2` and are closed exactly once — the
        // guard on `self.tty` below makes a second `shut_down` a no-op.
        unsafe {
            libc::close(self.winch_write);
            libc::close(self.winch_read);
        }
        restore_now();
        RESTORE_TTY.store(-1, Ordering::Release);
        RESTORE_OUT.store(-1, Ordering::Release);
        self.tty = -1;
    }
}

impl Drop for TerminalSession {
    /// The ordinary path, and the one an early `?` takes. The other three are in
    /// [`on_terminating_signal`] and the panic hook.
    fn drop(&mut self) {
        self.shut_down();
    }
}

/// Install the panic hook and the signal handlers, once per process.
///
/// **The panic hook chains rather than replaces.** Whatever was there before — the default one that
/// prints the message and the backtrace, or one a test harness installed — still runs, and it runs
/// *after* the restore, so its output lands on a terminal that has `ONLCR` and echo back and does
/// not staircase down the right-hand side of the screen. Replacing the hook instead of chaining it
/// would silently delete the panic message, which is the thing a person needs most at that moment.
fn install_hooks() {
    HOOKS.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_now();
            previous(info);
        }));
        // `SIGHUP` and `SIGQUIT` are in the list with the two that were asked for: a terminal
        // emulator that is closed sends `SIGHUP` to the foreground group, and `Ctrl-\` sends
        // `SIGQUIT` — both default to killing the process, and both would otherwise leave whatever
        // inherits this terminal with echo off.
        for signum in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP, libc::SIGQUIT] {
            install_handler(signum, on_terminating_signal);
        }
    });
}

/// `TIOCGWINSZ`: rows, columns, and the pixel size the kernel was told about.
fn winsize(fd: RawFd) -> Option<(u16, u16, u16, u16)> {
    // SAFETY: `winsize` is a plain C struct with no invalid bit patterns; the ioctl either fills it
    // or fails without touching it.
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) } != 0 {
        return None;
    }
    Some((ws.ws_row, ws.ws_col, ws.ws_xpixel, ws.ws_ypixel))
}

/// Read whatever the terminal has to say until the `CSI 5n` answer arrives or a read times out.
///
/// `VTIME` bounds every read, and the loop is bounded as well: a terminal that answers something
/// else entirely, forever, must not be able to spin here. The `n` that ends it is the device status
/// report's final byte, which is why [`query_sequence`] puts that query last.
/// **`libc::read` on the descriptor and not `std::io::stdin()`**, because taking the standard
/// input's lock here is taking it from the input thread that C-input is about to start, and a lock
/// held across a 200 ms timeout is a lock the key reader waits on to deliver its first keystroke.
fn read_answers(fd: RawFd) -> Vec<u8> {
    let mut collected = Vec::with_capacity(64);
    let mut chunk = [0u8; 64];
    for _ in 0..64 {
        // SAFETY: reading into a buffer this function owns, from the terminal descriptor the caller
        // has already established is a tty. `VMIN 0`/`VTIME` make this return 0 on a timeout rather
        // than block.
        let read =
            unsafe { libc::read(fd, chunk.as_mut_ptr().cast::<libc::c_void>(), chunk.len()) };
        if read <= 0 {
            break;
        }
        collected.extend_from_slice(&chunk[..read as usize]);
        if take_csi_reply(&collected, b'n').is_some() {
            break;
        }
    }
    // The status report itself is nobody's answer; taking it out here keeps it from being mistaken
    // for input by the caller, which hands everything left over to the key parser.
    match take_csi_reply(&collected, b'n') {
        Some((_, rest)) => rest,
        None => collected,
    }
}

/// [`take_csi_reply`] in the shape the enter path uses it: the reply if there was one, and the rest.
fn split_reply(buf: Vec<u8>, final_byte: u8) -> (Option<String>, Vec<u8>) {
    match take_csi_reply(&buf, final_byte) {
        Some((reply, rest)) => (Some(reply), rest),
        None => (None, buf),
    }
}

/// The `SIGWINCH` self-pipe: read end blocking, write end not.
///
/// **Only the write end is non-blocking, and the asymmetry is the design.** The handler must never
/// block — a full pipe during a window drag would stall the signal inside the handler — while the
/// reader wants to block, because a thread spinning on `EAGAIN` to notice a window resize is a
/// thread burning a core to do nothing.
fn self_pipe() -> Result<(RawFd, RawFd), String> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `pipe2` writes exactly two descriptors into the array. `O_CLOEXEC` so that the pipe
    // is not inherited by anything `:spawn` starts.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err("could not create the resize pipe".to_string());
    }
    // SAFETY: `fds[1]` was just created by `pipe2` and is owned here.
    let flags = unsafe { libc::fcntl(fds[1], libc::F_GETFL) };
    if flags >= 0 {
        // SAFETY: as above; adding `O_NONBLOCK` to the flags the descriptor already has.
        unsafe { libc::fcntl(fds[1], libc::F_SETFL, flags | libc::O_NONBLOCK) };
    }
    Ok((fds[0], fds[1]))
}

/// Send a size on the channel if it is not the size that was sent last.
fn emit_if_changed(
    last: &Arc<Mutex<Option<PaneSize>>>,
    tx: &mpsc::Sender<PaneSize>,
    size: PaneSize,
) {
    let mut last = match last.lock() {
        Ok(guard) => guard,
        // A poisoned mutex here means a thread panicked holding it, and the panic hook has already
        // put the terminal back. Dropping the resize is the right thing to do with it.
        Err(_) => return,
    };
    if size_changed(*last, size) {
        *last = Some(size);
        let _ = tx.send(size);
    }
}

/// The ordinary thread that turns bytes on the self-pipe into resize events.
///
/// It asks the kernel rather than trusting the byte: a burst of five `SIGWINCH`es during a drag
/// leaves at most one wake-up's worth of work, because the answer to "how big is it now" is the
/// same for all five. The cell size is captured rather than re-queried — asking `CSI 16t` from a
/// thread that does not own stdin would take the answer out of the input parser's mouth, and the
/// cell size does not change with the pane in any case; it changes with the font, which is a
/// terminal event this frontend does not yet listen for.
fn spawn_winch_thread(
    read_fd: RawFd,
    tty_fd: RawFd,
    cell: Option<(u32, u32)>,
    last: Arc<Mutex<Option<PaneSize>>>,
    tx: mpsc::Sender<PaneSize>,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("bru-winch".to_string())
        .spawn(move || {
            let mut drain = [0u8; 64];
            loop {
                // SAFETY: reading into a buffer this thread owns, from a descriptor the session
                // keeps open until after this thread is joined.
                let read = unsafe {
                    libc::read(read_fd, drain.as_mut_ptr().cast::<libc::c_void>(), drain.len())
                };
                if stop.load(Ordering::Acquire) {
                    return;
                }
                if read <= 0 {
                    // 0 is the write end closed; a negative that is not `EINTR` is a pipe there is
                    // no recovering. `SA_RESTART` makes `EINTR` rare, and looping on it is free.
                    // SAFETY: reading this thread's own `errno`.
                    if read == 0 || unsafe { *libc::__errno_location() } != libc::EINTR {
                        return;
                    }
                    continue;
                }
                if let Some((rows, cols, xpixel, ypixel)) = winsize(tty_fd) {
                    if let Some(size) = compose_size(rows, cols, (xpixel, ypixel), None, cell) {
                        emit_if_changed(&last, &tx, size);
                    }
                }
            }
        })
        .expect("a thread with a name is a thread the OS can create")
}

// -----------------------------------------------------------------------------------------------
// Tests
//
// Not one of these needs a terminal, which is the point of pushing everything that could be a pure
// function into one: the escapes, all five reply parsers, the size arithmetic, the deduplication
// and the termios mask are decided by functions whose inputs are values, and `cargo test` under a
// CI runner with no tty exercises every one of them.
//
// **What is NOT verified here, and what it will take to verify it.** The 2026-08-16 action's rule
// is that each restore path gets a test or a recorded manual attempt; these are the ones still
// owing the second kind, and they are owed against a real kitty, because a pty harness would prove
// the escapes were written and not that a terminal was put back:
//
//   1. `enter` against a live terminal at all — that the queries are answered in one round, that
//      `CSI 5n` ends the read, and that no keystroke typed during startup is lost.
//   2. `Drop` — run `bru --term`, quit normally, and check `stty -a` shows `echo` and `icanon` back.
//   3. The panic hook — panic on purpose behind a dev switch, and check the message is readable
//      (not staircased) and the terminal is cooked afterwards.
//   4. `kill -TERM`, `kill -INT`, `kill -HUP`, `kill -QUIT` from a second pane, one each, with
//      `stty -a` after every one, and `echo $?` showing the signal's own status rather than 0.
//   5. `SIGWINCH` — drag the pane's edge and confirm exactly one event per size, both with mode
//      2048 in force and with it forced off, which is the case this module dedups for.
//   6. That tmux really answers `CSI 16t` through the passthrough round — [`query_plan`]'s second
//      round is reasoned from the fact that a cell is a font property, and reasoning is not a
//      measurement.
// -----------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Getting this wrong does not fail loudly: tmux eats the sequence and the picture never
    /// appears, which reads as a terminal without graphics support.
    #[test]
    fn the_tmux_wrapper_doubles_every_escape_and_terminates() {
        let wrapped = passthrough("\x1b_Ga=T;AAAA\x1b\\");
        assert!(wrapped.starts_with("\x1bPtmux;"));
        assert!(wrapped.ends_with("\x1b\\"));
        assert!(wrapped.contains("\x1b\x1b_Ga=T;AAAA"));
    }

    /// The flags are the sum the protocol's push escape carries, and the four asked for are 15.
    #[test]
    fn the_kitty_push_carries_the_four_flags_this_frontend_needs() {
        assert_eq!(KITTY_FLAGS, 15);
        assert_eq!(kitty_push(KITTY_FLAGS), "\x1b[>15u");
    }

    /// Leaving undoes exactly what entering did and nothing more. A pop of a keyboard stack this
    /// process never pushed takes the mode away from whatever is underneath.
    #[test]
    fn leaving_undoes_only_the_modes_that_were_entered() {
        let none = leave_sequence(Modes::default());
        assert_eq!(none, SYNC_END, "an untouched terminal gets only the sync reset");
        assert!(!none.contains(KITTY_POP));

        let all = leave_sequence(Modes {
            alt_screen: true,
            cursor_hidden: true,
            mouse: true,
            kitty_keyboard: true,
            in_band_resize: true,
        });
        for escape in [KITTY_POP, IN_BAND_RESIZE_OFF, MOUSE_OFF, CURSOR_SHOW, ALT_SCREEN_OFF] {
            assert!(all.contains(escape), "leaving must undo {escape:?}");
        }
    }

    /// The order in the leave sequence is load-bearing: the keyboard stack is popped while bru still
    /// owns the screen, and the cursor comes back before the screen is handed over.
    #[test]
    fn the_leave_order_is_the_reverse_of_the_enter_order() {
        let all = leave_sequence(Modes {
            alt_screen: true,
            cursor_hidden: true,
            mouse: true,
            kitty_keyboard: true,
            in_band_resize: true,
        });
        let at = |needle: &str| all.find(needle).expect("present");
        assert!(at(SYNC_END) < at(KITTY_POP));
        assert!(at(KITTY_POP) < at(IN_BAND_RESIZE_OFF));
        assert!(at(IN_BAND_RESIZE_OFF) < at(MOUSE_OFF));
        assert!(at(MOUSE_OFF) < at(CURSOR_SHOW));
        assert!(at(CURSOR_SHOW) < at(ALT_SCREEN_OFF));
    }

    /// The encoding is set before the reporting mode, or the first click arrives in the old one.
    #[test]
    fn the_mouse_encoding_is_set_before_the_reporting_mode() {
        assert!(MOUSE_ON.find("1016h").unwrap() < MOUSE_ON.find("1003h").unwrap());
        // **`1006` before `1016`, and both before the tracking mode.** `1016` extends SGR and a
        // terminal that does not know it ignores the escape, leaving the default X10 encoding —
        // which is the failure this ordering exists to prevent. Off in the mirror order.
        assert!(MOUSE_ON.find("1006h").unwrap() < MOUSE_ON.find("1016h").unwrap());
        assert!(MOUSE_OFF.find("1016l").unwrap() < MOUSE_OFF.find("1006l").unwrap());
        // Every motion, so that pointing at something is something the page is told about.
        assert!(MOUSE_ON.contains("1003h") && MOUSE_OFF.contains("1003l"));
        // The tracking mode goes off first, so the last thing the terminal does with the mouse is
        // stop reporting rather than change the encoding of reports it is still sending.
        assert!(MOUSE_OFF.find("1003l").unwrap() < MOUSE_OFF.find("1016l").unwrap());
    }

    /// One write, and a device status report last so that the reader stops on an answer rather than
    /// on a timeout.
    #[test]
    fn every_query_goes_out_in_one_write_and_ends_with_a_status_report() {
        let queries = query_sequence();
        assert!(queries.ends_with("\x1b[5n"));
        for query in ["\x1b[?u", "\x1b[?2048$p", "\x1b[14t", "\x1b[16t"] {
            assert!(queries.contains(query), "missing {query:?}");
        }
    }

    /// Under tmux the *cell* size may be asked of the terminal underneath, because a cell is the
    /// same in every pane. The text area may not: the outer terminal's answer describes the whole
    /// window.
    #[test]
    fn only_the_cell_size_is_ever_asked_through_the_multiplexer() {
        let plain = query_plan(false);
        assert_eq!(plain.len(), 1);

        let tmux = query_plan(true);
        assert_eq!(tmux.len(), 2);
        let second = &tmux[1];
        assert!(second.starts_with("\x1bPtmux;"));
        assert!(second.contains("16t"));
        assert!(!second.contains("14t"), "the text area must never be asked through tmux");
    }

    /// A reply split across reads must leave the buffer alone rather than half-parse it. Every
    /// prefix is walked, which is the discipline the 2026-08-16 action put `theme_watch.rs` under.
    #[test]
    fn every_prefix_of_a_reply_is_a_legal_state() {
        let reply = b"\x1b[?2048;1$y";
        for cut in 0..reply.len() {
            assert_eq!(take_csi_reply(&reply[..cut], b'y'), None, "prefix of {cut} bytes matched");
        }
        let (found, rest) = take_csi_reply(reply, b'y').expect("the whole reply parses");
        assert_eq!(found, "\x1b[?2048;1$y");
        assert!(rest.is_empty());
    }

    /// Four replies arrive in one buffer and are pulled out by final byte, in any order, with the
    /// keystrokes between them surviving.
    #[test]
    fn replies_are_taken_out_by_final_byte_and_the_typing_between_them_survives() {
        let buf = b"\x1b[?15uj\x1b[6;25;9tk\x1b[4;1350;990t".to_vec();
        let (kitty, rest) = take_csi_reply(&buf, b'u').expect("the kitty reply");
        assert_eq!(kitty, "\x1b[?15u");
        let (first_t, rest) = take_csi_reply(&rest, b't').expect("the first t reply");
        assert_eq!(first_t, "\x1b[6;25;9t");
        let (second_t, rest) = take_csi_reply(&rest, b't').expect("the second t reply");
        assert_eq!(second_t, "\x1b[4;1350;990t");
        assert_eq!(rest, b"jk", "the keys typed during startup are still there");
    }

    /// Garbage must not match, must not panic, and must not consume anything.
    #[test]
    fn garbage_is_not_a_reply() {
        for junk in [
            &b""[..],
            &b"\x1b"[..],
            &b"\x1b["[..],
            &b"\x1b[\x07t"[..],
            &b"not an escape at all"[..],
            &b"\x1b]11;rgb:00/00/00\x07"[..],
        ] {
            assert_eq!(take_csi_reply(junk, b't'), None, "matched {junk:?}");
        }
    }

    /// The two `t` replies differ only in their first parameter, so each parser has to refuse the
    /// other's answer — otherwise a 9x25 cell is read as a 9x25 pixel pane.
    #[test]
    fn the_two_t_replies_refuse_each_other() {
        assert_eq!(parse_text_area_pixels("\x1b[4;1350;990t"), Some((990, 1350)));
        assert_eq!(parse_cell_size("\x1b[6;25;9t"), Some((9, 25)));
        assert_eq!(parse_text_area_pixels("\x1b[6;25;9t"), None);
        assert_eq!(parse_cell_size("\x1b[4;1350;990t"), None);
        // Height comes first in the escape and second in the answer, which is the mistake this
        // asserts against.
        assert_eq!(parse_text_area_pixels("\x1b[4;1350;990t").map(|(w, _)| w), Some(990));
    }

    /// A zero is not a size, however confidently it is reported.
    #[test]
    fn a_reported_zero_is_not_a_size() {
        assert_eq!(parse_text_area_pixels("\x1b[4;0;990t"), None);
        assert_eq!(parse_cell_size("\x1b[6;25;0t"), None);
    }

    #[test]
    fn the_kitty_reply_carries_the_flags_in_force() {
        assert_eq!(parse_kitty_flags("\x1b[?15u"), Some(15));
        assert_eq!(parse_kitty_flags("\x1b[?0u"), Some(0), "zero means the stack is empty");
        assert_eq!(parse_kitty_flags("\x1b[?u"), None);
    }

    /// `0` is the answer that matters: a terminal that has never heard of the mode must not have it
    /// set, because nothing would ever unset it.
    #[test]
    fn a_mode_the_terminal_does_not_recognise_is_told_apart_from_one_it_has_off() {
        assert_eq!(parse_decrqm("\x1b[?2048;0$y"), Some((2048, ModeState::NotRecognised)));
        assert_eq!(parse_decrqm("\x1b[?2048;2$y"), Some((2048, ModeState::Reset)));
        assert_eq!(parse_decrqm("\x1b[?2048;1$y"), Some((2048, ModeState::Set)));
        assert_eq!(parse_decrqm("\x1b[?2048;9$y"), None);
        assert_eq!(parse_decrqm("\x1b[?2048$y"), None, "a mode with no state is not an answer");
    }

    // The `CSI 48 … t` wire form is `term_keys.rs`'s to parse and to test — see the note above
    // `size_from_in_band`.

    /// The numbers measured on this machine on 2026-08-25, with the ioctl answering as it does here.
    #[test]
    fn the_pane_measured_on_this_machine_composes_as_it_was_measured() {
        let size = compose_size(54, 110, (990, 1350), None, Some((9, 25))).expect("a size");
        assert_eq!((size.width, size.height), (990, 1350));
        assert_eq!((size.cols, size.rows), (110, 54));
        assert_eq!((size.cell_width, size.cell_height), (9, 25));
        assert_eq!(size.source, SizeSource::Ioctl);
    }

    /// The order of trust: the ioctl is about this pane, the query is about whoever answered, and
    /// the multiplication is the last resort.
    #[test]
    fn the_three_sources_are_used_in_order_of_certainty() {
        let ioctl = compose_size(54, 110, (990, 1350), Some((1920, 1080)), None).expect("a size");
        assert_eq!((ioctl.width, ioctl.height), (990, 1350));
        assert_eq!(ioctl.source, SizeSource::Ioctl);

        let queried = compose_size(54, 110, (0, 0), Some((990, 1350)), None).expect("a size");
        assert_eq!((queried.width, queried.height), (990, 1350));
        assert_eq!(queried.source, SizeSource::TextAreaQuery);

        let derived = compose_size(54, 110, (0, 0), None, Some((9, 25))).expect("a size");
        assert_eq!((derived.width, derived.height), (990, 1350));
        assert_eq!(derived.source, SizeSource::CellGrid);

        assert_eq!(compose_size(54, 110, (0, 0), None, None), None, "nothing knew anything");
        assert_eq!(compose_size(0, 110, (990, 1350), None, None), None, "no grid, no terminal");
    }

    /// A cell derived by division loses the terminal's padding, and multiplying it back up would
    /// report a pane narrower than it is. The derived cell is recorded; the pane keeps what it was
    /// told.
    #[test]
    fn a_derived_cell_never_shrinks_the_pane_it_was_derived_from() {
        let size = compose_size(54, 110, (993, 1353), None, None).expect("a size");
        assert_eq!((size.cell_width, size.cell_height), (9, 25));
        assert_eq!((size.width, size.height), (993, 1353), "the pane keeps the ioctl's pixels");
    }

    /// A pane one cell wide must not produce a cell zero pixels wide, because everything downstream
    /// divides by it.
    #[test]
    fn a_derived_cell_is_never_zero() {
        let size = compose_size(1, 400, (100, 10), None, None).expect("a size");
        assert!(size.cell_width >= 1 && size.cell_height >= 1);
    }

    /// The terminal knows its own font. A reported cell wins over a division even when the pane's
    /// pixels came from the ioctl.
    #[test]
    fn a_reported_cell_beats_a_derived_one() {
        let size = compose_size(54, 110, (993, 1353), None, Some((9, 25))).expect("a size");
        assert_eq!((size.cell_width, size.cell_height), (9, 25));
    }

    /// A report says what it says, and says it about this pane. A report without pixels still gives
    /// a size, through the same grid-times-cell arithmetic everything else falls back to.
    #[test]
    fn an_in_band_report_is_its_own_source_and_survives_missing_pixels() {
        let full = size_from_in_band(54, 110, 990, 1350, Some((9, 25))).expect("a size");
        assert_eq!((full.width, full.height), (990, 1350));
        assert_eq!(full.source, SizeSource::InBandReport);

        let no_pixels = size_from_in_band(54, 110, 0, 0, Some((9, 25))).expect("a size");
        assert_eq!((no_pixels.width, no_pixels.height), (990, 1350));
        assert_eq!(no_pixels.source, SizeSource::CellGrid, "derived, and it says so");

        assert_eq!(size_from_in_band(54, 110, 0, 0, None), None, "no pixels and no cell is nothing");
    }

    /// Mode 2048 and `SIGWINCH` both fire for one drag. Only one layout may come of it.
    #[test]
    fn the_same_size_reported_twice_is_one_event() {
        let first = compose_size(54, 110, (990, 1350), None, Some((9, 25))).expect("a size");
        let again = compose_size(54, 110, (0, 0), Some((990, 1350)), Some((9, 25)))
            .expect("the same pane, noticed the other way");
        assert_ne!(first.source, again.source, "the two ways disagree about provenance");
        assert!(size_changed(None, first), "the first size is always news");
        assert!(!size_changed(Some(first), again), "provenance is not a resize");

        let bigger = compose_size(54, 111, (999, 1350), None, Some((9, 25))).expect("a size");
        assert!(size_changed(Some(first), bigger));
    }

    /// The bit that has already cost this project a day: `ISIG` survives raw mode, so `Ctrl-C`
    /// stays a signal for every moment the kitty protocol is not in force.
    #[test]
    fn raw_mode_keeps_the_signals_that_cfmakeraw_takes_away() {
        // SAFETY: a zeroed `termios` is a valid one to read fields out of; nothing here calls into
        // libc with it.
        let mut saved: libc::termios = unsafe { std::mem::zeroed() };
        saved.c_lflag = libc::ECHO | libc::ICANON | libc::ISIG | libc::IEXTEN;
        saved.c_iflag = libc::IXON | libc::IXOFF | libc::ICRNL;
        saved.c_oflag = libc::OPOST;

        let raw = raw_termios(saved, QUERY_VTIME);
        assert_eq!(raw.c_lflag & libc::ECHO, 0, "echo would print the escape sequences");
        assert_eq!(raw.c_lflag & libc::ICANON, 0, "line buffering would hold every key until Enter");
        assert_ne!(raw.c_lflag & libc::ISIG, 0, "Ctrl-C must stay a signal — see the module header");
        // A key the line discipline eats as flow control cannot reach a binding, and `<Ctrl-Q>` is
        // bound to `quit`.
        assert_eq!(raw.c_iflag & libc::IXON, 0, "Ctrl-Q must be a key, not XON");
        assert_eq!(raw.c_iflag & libc::IXOFF, 0, "and Ctrl-S must not be able to stop the pane");
        assert_ne!(raw.c_iflag & libc::ICRNL, 0, "the other input flags are left alone");
        assert_ne!(raw.c_oflag & libc::OPOST, 0, "an error message still has to be readable");
        assert_eq!(raw.c_cc[libc::VMIN], 0, "a read must be able to return nothing");
        assert_eq!(raw.c_cc[libc::VTIME], QUERY_VTIME);
    }

    /// The restore is armed and disarmed by exactly one path, whichever gets there first.
    #[test]
    fn only_the_first_restore_does_the_work() {
        // The statics belong to the process, and no terminal is armed under `cargo test`, so the
        // observable behaviour is that a restore with nothing armed is a no-op that does not touch
        // a descriptor. Repeated for the same reason a `Drop` after a signal handler must be one.
        assert!(!ARMED.load(Ordering::Acquire));
        restore_now();
        restore_now();
        assert!(!ARMED.load(Ordering::Acquire));
    }
}
