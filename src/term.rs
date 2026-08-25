//! The terminal frontend's ground floor: what the pane is, and how a picture gets into it.
//!
//! **This is a spike and it is labelled one.** It exists to answer a question the plan could not
//! answer by reading — *is a browser in this terminal watchable at all* — before six weeks are spent
//! on the assumption that it is. Nothing above it depends on it, `--term-probe` is its only entry
//! point, and the parts of it that survive into the real terminal layer (`term_session.rs` in the
//! plan's C3) will be the ones this proves, not the ones this happens to contain.
//!
//! ## The kitty graphics protocol, and the tax tmux puts on it
//!
//! A picture reaches a terminal as an escape sequence carrying base64. The fast way is shared
//! memory — the program writes the pixels to a `/dev/shm` object and sends the *name* — and it is
//! not available here: bru's user runs tmux inside kitty, and tmux is the one reading the escape.
//! What works through a multiplexer is passthrough: tmux forwards a `DCS tmux;` wrapper to the
//! terminal underneath, with every escape byte doubled, and `allow-passthrough` must be on for it
//! to forward anything at all.
//!
//! So every frame goes down the pty as base64 — four bytes on the wire for three of picture — and
//! every 4096-byte chunk of it carries its own wrapper. **That is the number this module measures**,
//! and it is the number that decides whether the plan's C4 can use the transport it assumes.

use std::io::{Read, Write};
use std::time::Instant;

/// Raw mode for as long as this lives, and the terminal put back when it does not.
///
/// A browser that leaves the terminal raw has failed at its one terminal-citizenship job, so the
/// restore is a `Drop` and not a line at the end of a function: an early return, a `?` and a panic
/// all pass through it.
struct Raw {
    fd: i32,
    saved: libc::termios,
}

impl Raw {
    fn enter() -> Result<Raw, String> {
        let fd = std::io::stdin().as_raw_fd();
        // SAFETY: `termios` is a plain C struct with no invalid bit patterns, and `tcgetattr` fills
        // it or reports failure without touching it.
        let mut saved: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut saved) } != 0 {
            return Err("not a terminal".to_string());
        }
        let mut raw = saved;
        // SAFETY: `raw` is a termios this function owns and has just filled from the terminal.
        unsafe { libc::cfmakeraw(&mut raw) };
        // A query's answer arrives in one read or not at all: VMIN 0 with VTIME in tenths means a
        // read that waits at most that long and then gives up, which is what keeps a terminal that
        // does not answer from hanging the probe.
        raw.c_cc[libc::VMIN] = 0;
        raw.c_cc[libc::VTIME] = 3;
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return Err("could not put the terminal in raw mode".to_string());
        }
        Ok(Raw { fd, saved })
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        // SAFETY: `saved` is what `tcgetattr` gave us for this same descriptor.
        unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.saved) };
    }
}

use std::os::fd::AsRawFd;

/// What the terminal will give a picture: pixels, and the cell grid they sit on.
#[derive(Clone, Copy, Debug)]
pub struct Pane {
    pub width: u32,
    pub height: u32,
    pub cols: u16,
    pub rows: u16,
    /// How the answer was arrived at, because the two ways disagree under a multiplexer and a
    /// reader of the numbers is owed which one produced them.
    pub source: &'static str,
}

/// The kernel's answer. Authoritative for the cell grid, and often zero for pixels under tmux.
fn winsize() -> Option<(u16, u16, u16, u16)> {
    // SAFETY: `winsize` is a plain C struct; the ioctl fills it or fails.
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let fd = std::io::stdout().as_raw_fd();
    if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) } != 0 {
        return None;
    }
    Some((ws.ws_row, ws.ws_col, ws.ws_xpixel, ws.ws_ypixel))
}

/// Whether this process is talking to tmux rather than to the terminal.
fn in_tmux() -> bool {
    std::env::var_os("TMUX").is_some_and(|value| !value.is_empty())
}

/// Wrap an escape sequence so tmux hands it to the terminal underneath instead of eating it.
///
/// The rule is the DCS `tmux;` wrapper with **every `\x1b` in the payload doubled**, terminated by
/// `\x1b\\`. Without `allow-passthrough on` in tmux, this is dropped and nothing is drawn — which
/// looks exactly like a terminal that does not support graphics, so the probe says which it was.
fn passthrough(payload: &str) -> String {
    if !in_tmux() {
        return payload.to_string();
    }
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

/// Ask the terminal how many pixels the text area has: `CSI 14 t`, answered as `CSI 4 ; h ; w t`.
fn query_pixels() -> Option<(u32, u32)> {
    let raw = Raw::enter().ok()?;
    let mut out = std::io::stdout();
    out.write_all(passthrough("\x1b[14t").as_bytes()).ok()?;
    out.flush().ok()?;

    let mut answer = Vec::new();
    let mut byte = [0u8; 1];
    let mut stdin = std::io::stdin();
    // The reply ends with `t`. VTIME bounds each read, and the loop is bounded too: a terminal that
    // answers something else entirely must not be able to spin here.
    for _ in 0..64 {
        match stdin.read(&mut byte) {
            Ok(1) => {
                answer.push(byte[0]);
                if byte[0] == b't' {
                    break;
                }
            }
            _ => break,
        }
    }
    drop(raw);

    let text = String::from_utf8_lossy(&answer);
    let body = text.trim_start_matches(['\x1b', '[']).trim_end_matches('t');
    let parts: Vec<&str> = body.split(';').collect();
    if parts.len() != 3 || parts[0] != "4" {
        return None;
    }
    let height = parts[1].parse().ok()?;
    let width = parts[2].parse().ok()?;
    if width == 0 || height == 0 {
        return None;
    }
    Some((width, height))
}

/// What this pane is, by the kernel where it knows and by asking the terminal where it does not.
pub fn pane() -> Result<Pane, String> {
    let (rows, cols, xpixel, ypixel) = winsize().ok_or("no window size: not a terminal")?;
    if xpixel > 0 && ypixel > 0 {
        return Ok(Pane {
            width: u32::from(xpixel),
            height: u32::from(ypixel),
            cols,
            rows,
            source: "TIOCGWINSZ",
        });
    }
    // tmux reports no pixel size of its own, so the question goes to the terminal underneath.
    let (width, height) = query_pixels().ok_or(
        "the terminal did not answer CSI 14t and the kernel reports no pixel size, so there is no \
         way to know how big a picture may be",
    )?;
    Ok(Pane { width, height, cols, rows, source: "CSI 14t" })
}

/// How many bytes of base64 go in one escape. The protocol's own limit is 4096.
const CHUNK: usize = 4096;

/// Send one RGB frame and display it at the cursor, replacing whatever was there before.
///
/// `i=1` reuses one image id, so the terminal replaces the picture rather than accumulating a new
/// one per frame — the difference between a browser and a memory leak with a view. `q=2` asks for
/// no acknowledgement, because a reply would arrive on the same stdin the key reader owns.
pub fn write_image(out: &mut impl Write, rgb: &[u8], width: u32, height: u32) -> std::io::Result<()> {
    let encoded = base64(rgb);
    let mut sent = 0;
    let mut first = true;
    while sent < encoded.len() {
        let end = (sent + CHUNK).min(encoded.len());
        let more = i32::from(end < encoded.len());
        let control = if first {
            format!("a=T,q=2,i=1,f=24,s={width},v={height},C=1,m={more}")
        } else {
            format!("m={more}")
        };
        let payload = format!("\x1b_G{control};{}\x1b\\", &encoded[sent..end]);
        out.write_all(passthrough(&payload).as_bytes())?;
        sent = end;
        first = false;
    }
    out.flush()
}

/// Base64, written here rather than pulled in: the whole of it is one table and three shifts, and a
/// dependency for that would be a dependency for that.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { ALPHABET[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHABET[n as usize & 63] as char } else { '=' });
    }
    out
}

/// A frame that changes every time, so nothing downstream can cheat by noticing it has not.
///
/// Deliberately expensive to compress and easy to see: a moving vertical band over a gradient, so a
/// dropped frame shows up as a stutter a person can watch rather than as a number.
fn test_frame(width: u32, height: u32, phase: u32) -> Vec<u8> {
    let mut rgb = vec![0u8; (width * height * 3) as usize];
    let band = (phase * 17) % width.max(1);
    for y in 0..height {
        for x in 0..width {
            let i = ((y * width + x) * 3) as usize;
            // **`.max(1)` and not tidiness.** At 16 px wide the band was `width / 24 == 0` pixels
            // across, so it never drew and every phase produced the same frame — a probe that
            // measures a picture nothing changes measures the terminal's ability to do nothing.
            // Found by the test below, which is why it compares two phases rather than one frame.
            let near = x.abs_diff(band) < (width / 24).max(1);
            rgb[i] = if near { 255 } else { (x * 255 / width.max(1)) as u8 };
            rgb[i + 1] = (y * 255 / height.max(1)) as u8;
            rgb[i + 2] = if near { 255 } else { 128 };
        }
    }
    rgb
}

/// `bru --term-probe`: draw into this pane and say what it cost.
pub fn probe() -> Result<(), String> {
    let pane = pane()?;
    let bytes_per_frame = (pane.width * pane.height * 3) as usize;
    println!(
        "pane: {}x{} px, {}x{} cells, via {}",
        pane.width, pane.height, pane.cols, pane.rows, pane.source
    );
    println!(
        "one frame: {:.2} MB of pixels, {:.2} MB as base64{}",
        bytes_per_frame as f64 / 1e6,
        bytes_per_frame as f64 * 4.0 / 3.0 / 1e6,
        if in_tmux() { " (plus tmux's passthrough wrapper per 4 KB chunk)" } else { "" }
    );
    if in_tmux() {
        println!("tmux: yes — shared memory is unavailable, every frame goes down the pty");
    }
    println!("drawing 30 frames, watch the band move…");

    let mut out = std::io::stdout();
    // Home the cursor and hide it, so the picture lands in the same place every time.
    let _ = out.write_all(b"\x1b[H\x1b[?25l");
    let started = Instant::now();
    let mut encode = std::time::Duration::ZERO;
    for phase in 0..30u32 {
        let at = Instant::now();
        let frame = test_frame(pane.width, pane.height, phase);
        encode += at.elapsed();
        write_image(&mut out, &frame, pane.width, pane.height)
            .map_err(|e| format!("could not write the image: {e}"))?;
        let _ = out.write_all(b"\x1b[H");
    }
    let elapsed = started.elapsed();
    let _ = out.write_all(b"\x1b[?25h\n");
    let _ = out.flush();

    let fps = 30.0 / elapsed.as_secs_f64();
    println!(
        "30 frames in {:.2}s -> {:.1} fps ({:.2} MB/s of base64 down the pty)",
        elapsed.as_secs_f64(),
        fps,
        bytes_per_frame as f64 * 4.0 / 3.0 * 30.0 / elapsed.as_secs_f64() / 1e6
    );
    println!(
        "of which {:.2}s was making the pixels, so the terminal's share is {:.2}s",
        encode.as_secs_f64(),
        (elapsed - encode).as_secs_f64()
    );
    println!(
        "verdict: {}",
        if fps >= 30.0 {
            "watchable — a browser can be driven in this pane"
        } else if fps >= 12.0 {
            "usable for reading and scrolling, not for video"
        } else {
            "too slow for a browser in this pane at this size"
        }
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_examples_from_rfc_4648() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    /// Every escape byte doubled, wrapped in the DCS, terminated. Getting this wrong does not fail
    /// loudly — tmux simply eats the sequence and nothing is drawn.
    #[test]
    fn the_tmux_wrapper_doubles_every_escape() {
        // The function reads `$TMUX`, so the shape is asserted on the pieces it builds from.
        let payload = "\x1b_Ga=T;AAAA\x1b\\";
        let doubled: String = payload
            .chars()
            .flat_map(|c| if c == '\x1b' { vec!['\x1b', '\x1b'] } else { vec![c] })
            .collect();
        assert_eq!(doubled.matches('\x1b').count(), payload.matches('\x1b').count() * 2);
        assert!(doubled.starts_with("\x1b\x1b_G"));
    }

    /// The frame is the size the protocol will be told it is, and it changes with the phase.
    #[test]
    fn a_test_frame_is_rgb_and_moves() {
        let a = test_frame(16, 8, 0);
        let b = test_frame(16, 8, 1);
        assert_eq!(a.len(), 16 * 8 * 3);
        assert_ne!(a, b, "a frame that does not change measures nothing");
    }
}
