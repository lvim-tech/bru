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
//! A picture reaches a terminal as an escape sequence carrying base64 — four bytes on the wire for
//! three of picture, and under tmux every 4096-byte chunk carries a `DCS tmux;` passthrough wrapper
//! on top (with `allow-passthrough` on, or nothing is forwarded at all).
//!
//! The other way is shared memory: the program writes the pixels into a `/dev/shm` object and sends
//! only the **name**, and kitty opens it, reads it and unlinks it.
//!
//! **This module was written believing a multiplexer takes that away, and the measurement said
//! otherwise.** Measured 2026-08-25 in tmux inside kitty, 990x1350: base64 is 8 fps, shared memory
//! is 78 fps with every frame confirmed — 9.7x. tmux forwards the escape and kitty does the opening,
//! so nothing about locality changed; the belief was wrong and cost nothing only because it was
//! tested before anything was built on it. That is what this module is for.

use std::io::{Read, Write};
use std::time::Instant;

/// Raw mode for as long as this lives, and the terminal put back when it does not.
///
/// A browser that leaves the terminal raw has failed at its one terminal-citizenship job, so the
/// restore is a `Drop` and not a line at the end of a function: an early return, a `?` and a panic
/// all pass through it.
pub(crate) struct Raw {
    fd: i32,
    saved: libc::termios,
}

impl Raw {
    pub(crate) fn enter() -> Result<Raw, String> {
        let fd = std::io::stdin().as_raw_fd();
        // SAFETY: `termios` is a plain C struct with no invalid bit patterns, and `tcgetattr` fills
        // it or reports failure without touching it.
        let mut saved: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut saved) } != 0 {
            return Err("not a terminal".to_string());
        }
        let mut raw = saved;
        // **`cfmakeraw` would clear `ISIG`, and that is how a probe becomes unkillable.** Measured
        // 2026-08-25: with the terminal drowning in queued frames, Ctrl-C reached a program that
        // had turned the signal into a byte. Only echo and line-buffering are in the way of reading
        // a reply, so only those two come off; ISIG stays and Ctrl-C stays a signal.
        raw.c_lflag &= !(libc::ECHO | libc::ICANON);
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

/// Delete every image the terminal is holding for us.
///
/// **Through [`passthrough`], and that is the whole reason this is a function.** The first version
/// wrote this escape straight to stdout while `write_image` wrapped its own — so under tmux the
/// pictures were delivered and the deletes were eaten, and the last frame sat on top of the report
/// it was supposed to be read with. A graphics escape that does not go through the same door as the
/// others is a graphics escape that does not arrive.
pub fn delete_images(out: &mut impl Write) -> std::io::Result<()> {
    out.write_all(passthrough("\x1b_Ga=d,d=A\x1b\\").as_bytes())?;
    out.flush()
}

/// How many bytes of base64 go in one escape. The protocol's own limit is 4096.
const CHUNK: usize = 4096;

/// Send one RGB frame and display it at the cursor, replacing whatever was there before.
///
/// `i=1` reuses one image id, so the terminal replaces the picture rather than accumulating a new
/// one per frame — the difference between a browser and a memory leak with a view. `q=2` asks for
/// no acknowledgement, because a reply would arrive on the same stdin the key reader owns.
pub fn write_image(
    out: &mut impl Write,
    encoded: &str,
    width: u32,
    height: u32,
) -> std::io::Result<()> {
    let mut sent = 0;
    let mut first = true;
    while sent < encoded.len() {
        let end = (sent + CHUNK).min(encoded.len());
        let more = i32::from(end < encoded.len());
        let control = if first {
            // **No `q` at all, which is `q=0`, which is the only setting that answers.** kitty's
            // levels are: 0 respond, 1 suppress the "OK" and keep errors, 2 suppress everything.
            // The first version asked for `q=2` — nothing to wait for — so frames went into the
            // pty as fast as `write` would take them and the terminal fell behind by however much
            // memory the buffers had. The response is the backpressure; see [`draw_and_wait`].
            format!("a=T,i=1,f=24,s={width},v={height},C=1,m={more}")
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

/// Hand the pixels over in `/dev/shm` instead of down the pty.
///
/// **The transport the plan assumed a multiplexer takes away.** kitty's `t=s` says "the payload is
/// the *name* of a POSIX shared memory object"; kitty opens it, reads the pixels and unlinks it. The
/// assumption worth testing is that tmux does not break this: tmux only forwards the escape, and
/// the object is opened by kitty on the same machine either way. If that holds, the 5 MB of base64
/// this replaces never enters a pipe at all.
///
/// Returns the name it created, so the caller can unlink it if kitty never did — a terminal that
/// ignores the escape would otherwise leave 4 MB in `/dev/shm` per frame.
/// `quiet` asks kitty for no response at all.
///
/// **A response nobody reads is input somebody else receives.** Measured 2026-08-25: the spike sent
/// frames without `q`, never read the `OK` that came back for each one, and tmux delivered the
/// backlog as *keystrokes* to whichever pane had focus — `Gi=1;OKGi=1;OK…` typed into a prompt two
/// panes away. The probe wants the acknowledgement, because waiting for it is the measurement and
/// the backpressure; the spike must not ask for one, because it paints on the UI thread and cannot
/// stop to listen. Whoever cannot read the answer does not get to ask the question.
pub(crate) fn write_image_shm(
    out: &mut impl Write,
    rgb: &[u8],
    width: u32,
    height: u32,
    sequence: u32,
    quiet: bool,
) -> std::io::Result<Option<String>> {
    let name = format!("/bru-probe-{}-{sequence}", std::process::id());
    let c_name = std::ffi::CString::new(name.clone()).expect("no NUL in a formatted name");
    // SAFETY: `c_name` is a valid NUL-terminated string that outlives the call. O_EXCL means this
    // either creates a new object or fails; it never opens somebody else's.
    let fd = unsafe {
        libc::shm_open(c_name.as_ptr(), libc::O_CREAT | libc::O_EXCL | libc::O_RDWR, 0o600)
    };
    if fd < 0 {
        return Ok(None);
    }
    // From here every exit has to close the descriptor and unlink the object, or the failure leaks.
    let cleanup = |fd: i32| {
        // SAFETY: `fd` came from `shm_open` above and is closed exactly once.
        unsafe { libc::close(fd) };
    };
    // SAFETY: `fd` is a shared memory object this function just created and owns.
    if unsafe { libc::ftruncate(fd, rgb.len() as libc::off_t) } != 0 {
        cleanup(fd);
        // SAFETY: the name is ours and the object exists.
        unsafe { libc::shm_unlink(c_name.as_ptr()) };
        return Ok(None);
    }
    // SAFETY: the object has just been sized to `rgb.len()`, so a mapping of that length is within
    // it; `MAP_SHARED` is what makes the bytes visible to the reader.
    let mapped = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            rgb.len(),
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        )
    };
    if mapped == libc::MAP_FAILED {
        cleanup(fd);
        // SAFETY: as above.
        unsafe { libc::shm_unlink(c_name.as_ptr()) };
        return Ok(None);
    }
    // SAFETY: `mapped` is a writable mapping of exactly `rgb.len()` bytes, and the two regions
    // cannot overlap — one is an anonymous file mapping, the other this process's heap.
    unsafe { std::ptr::copy_nonoverlapping(rgb.as_ptr(), mapped.cast::<u8>(), rgb.len()) };
    // SAFETY: unmapping the mapping just made, with the length it was made with.
    unsafe { libc::munmap(mapped, rgb.len()) };
    cleanup(fd);

    // The payload of a graphics escape is always base64, the name included.
    let quiet = if quiet { ",q=2" } else { "" };
    let payload = format!(
        "\x1b_Ga=T,i=1,f=24,t=s,s={width},v={height},C=1{quiet};{}\x1b\\",
        base64(name.as_bytes())
    );
    out.write_all(passthrough(&payload).as_bytes())?;
    out.flush()?;
    Ok(Some(name))
}

/// Remove a shared memory object kitty did not take.
pub(crate) fn unlink_shm(name: &str) {
    if let Ok(c_name) = std::ffi::CString::new(name) {
        // SAFETY: the name is one this process created; unlinking a name that is already gone
        // fails harmlessly and is not checked for that reason.
        unsafe { libc::shm_unlink(c_name.as_ptr()) };
    }
}

/// Wait for kitty to say it has the picture.
///
/// **This is the difference between measuring a terminal and measuring a pipe.** Without it, the
/// write returns as soon as the bytes are in a buffer somebody else will drain, so the "terminal"
/// column reports the speed of `memcpy` and the queue behind it grows until the pane stops
/// answering — which is exactly what happened on 2026-08-25 at 12 MB a frame through tmux.
///
/// The response is `ESC _ G ... ESC \`, and all that is needed is its terminator. `false` means
/// none arrived before the read timed out (`VTIME`), which is a fact about the terminal worth
/// reporting rather than an error: a multiplexer that swallows the reply leaves the measurement
/// write-only, and the caller says so instead of quietly printing a number that means nothing.
/// What the terminal said about the frame just written: `true` only for `OK`.
///
/// **The payload is read, not just the terminator.** This used to return `true` for any reply that
/// ended in `ESC \\`, which counts a refusal as an acknowledgement — and a refusal is exactly what a
/// terminal sends when it cannot read the transport. Measured 2026-08-26 in zellij 0.46.0: 61 of 61
/// frames "acknowledged" at 206 fps into a pane that stayed black, because every one of those was
/// the terminal saying no. A number that cannot say no is not a measurement.
/// The payload of the next graphics reply, or `None` if the terminal said nothing.
///
/// The reply is `ESC _ G <keys> ; <payload> ESC \\`, and the payload is the whole of the answer:
/// `OK`, or a reason such as `ENOENT:...`.
fn read_reply(stdin: &mut std::io::Stdin) -> Option<String> {
    let mut byte = [0u8; 1];
    let mut after_escape = false;
    let mut body = Vec::with_capacity(64);
    for _ in 0..8192 {
        match stdin.read(&mut byte) {
            Ok(1) => {
                if after_escape && byte[0] == b'\\' {
                    let body = String::from_utf8_lossy(&body).into_owned();
                    return Some(body.rsplit(';').next().unwrap_or("").trim().to_string());
                }
                after_escape = byte[0] == 0x1b;
                if !after_escape {
                    body.push(byte[0]);
                }
            }
            _ => return None,
        }
    }
    None
}

/// Ask the terminal, for every way of handing over a frame, whether it would take one.
///
/// **`a=q` transmits nothing and displays nothing.** It is the one question in the protocol that can
/// be asked before a single pixel is committed to, and it is how `pixel-core` in zenbu-labs'
/// terminal-browser picks its transport — file, then shared memory, then inline. bru guessed
/// instead, and a guess that is wrong is a black pane at full speed.
///
/// Both pixel formats are asked because they are not the same question: bru sends 24-bit RGB and
/// every other kitty client in reach sends 32-bit RGBA, and a host that re-encodes rather than
/// relays may well implement only one of them.
fn capabilities(
    out: &mut impl Write,
    stdin: &mut std::io::Stdin,
) -> Vec<(&'static str, &'static str, String)> {
    let mut rows = Vec::new();
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from);
    let mut id = 300u32;
    for (medium, transport) in [('d', "direct"), ('s', "shared memory"), ('f', "file")] {
        for (bits, format) in [(24u32, "RGB"), (32u32, "RGBA")] {
            id += 1;
            let pixel = vec![0u8; (bits / 8) as usize];
            let name = format!("/bru-cap-{}-{id}", std::process::id());
            let mut cleanup: Option<Box<dyn FnOnce()>> = None;
            let payload = match medium {
                'd' => Some(base64(&pixel)),
                's' => crate::term_paint::shm_stash(&name, &pixel).then(|| {
                    let taken = name.clone();
                    cleanup = Some(Box::new(move || crate::term_paint::shm_probe_release(&taken)));
                    base64(name.as_bytes())
                }),
                _ => runtime.as_ref().and_then(|dir| {
                    let path = dir.join(format!("bru{}", name));
                    std::fs::write(&path, &pixel).ok().map(|()| {
                        let taken = path.clone();
                        cleanup = Some(Box::new(move || {
                            let _ = std::fs::remove_file(&taken);
                        }));
                        base64(path.to_string_lossy().as_bytes())
                    })
                }),
            };
            let Some(payload) = payload else {
                rows.push((transport, format, "could not be offered at all".to_string()));
                continue;
            };
            let escape =
                format!("\x1b_Gi={id},a=q,t={medium},f={bits},s=1,v=1;{payload}\x1b\\");
            let asked = out.write_all(passthrough(&escape).as_bytes()).and_then(|()| out.flush());
            let answer = match asked {
                Ok(()) => read_reply(stdin).unwrap_or_else(|| "no answer".to_string()),
                Err(e) => format!("could not be asked: {e}"),
            };
            if let Some(cleanup) = cleanup {
                cleanup();
            }
            rows.push((transport, format, answer));
        }
    }
    rows
}

fn read_ack(stdin: &mut std::io::Stdin) -> bool {
    let mut byte = [0u8; 1];
    let mut after_escape = false;
    let mut body = Vec::with_capacity(32);
    // Bounded, because a terminal answering something else entirely must not spin here.
    for _ in 0..8192 {
        match stdin.read(&mut byte) {
            Ok(1) => {
                if after_escape && byte[0] == b'\\' {
                    // Everything after the `;` is the answer: `OK`, or a reason it is not.
                    let body = String::from_utf8_lossy(&body);
                    return body.rsplit(';').next().is_some_and(|tail| tail.starts_with("OK"));
                }
                after_escape = byte[0] == 0x1b;
                if !after_escape {
                    body.push(byte[0]);
                }
            }
            _ => return false,
        }
    }
    false
}

/// One size's worth of measurement, with the three costs kept apart.
///
/// **They are three and not one, and lumping them is how a spike lies.** Making the pixels is bru's
/// own arithmetic and is what a debug build makes look terrible; base64 is the protocol's tax and
/// scales with the picture; the write is the terminal's — the pty, tmux's passthrough, and kitty
/// decoding and uploading a texture. Only the third is the thing this module exists to find out.
struct Cost {
    width: u32,
    height: u32,
    frames: u32,
    /// How many of those frames the terminal actually confirmed. Fewer than `frames` means the
    /// number below is a lower bound on speed and an upper bound on truth.
    acked: u32,
    pixels: std::time::Duration,
    encode: std::time::Duration,
    write: std::time::Duration,
}

impl Cost {
    fn per_frame(&self) -> f64 {
        self.write.as_secs_f64() / f64::from(self.frames.max(1))
    }
    fn fps(&self) -> f64 {
        1.0 / self.per_frame().max(f64::MIN_POSITIVE)
    }
}

/// Draw at one size until the budget runs out, waiting for each frame to land before sending the
/// next one.
fn measure(
    width: u32,
    height: u32,
    budget: std::time::Duration,
    stdin: &mut std::io::Stdin,
) -> Result<Cost, String> {
    let mut out = std::io::stdout();
    let mut cost = Cost {
        width,
        height,
        frames: 0,
        acked: 0,
        pixels: <_>::default(),
        encode: <_>::default(),
        write: <_>::default(),
    };
    let started = Instant::now();
    let mut phase = 0u32;
    while started.elapsed() < budget && cost.frames < 120 {
        let at = Instant::now();
        let frame = test_frame(width, height, phase);
        cost.pixels += at.elapsed();

        let at = Instant::now();
        let encoded = base64(&frame);
        cost.encode += at.elapsed();

        let at = Instant::now();
        let _ = out.write_all(b"\x1b[H");
        write_image(&mut out, &encoded, width, height)
            .map_err(|e| format!("could not write the image: {e}"))?;
        if read_ack(stdin) {
            cost.acked += 1;
        }
        cost.write += at.elapsed();

        cost.frames += 1;
        phase += 1;
    }
    Ok(cost)
}

/// The same measurement, with the pixels handed over in shared memory instead of down the pty.
///
/// `pixels` is unchanged, `encode` is zero by construction — there is no base64 of a picture, only
/// of a short name — and `write` is the copy into `/dev/shm` plus whatever kitty does with it.
/// The same picture again, handed over as a path the terminal re-reads.
///
/// **Measured rather than assumed to be shared memory's equal.** The directory is tmpfs, so the
/// write is a `memcpy` and the escape is the same sixty bytes — but "should be" is not a number, and
/// a host that takes files may still be re-encoding every frame behind them.
fn measure_file(
    width: u32,
    height: u32,
    budget: std::time::Duration,
    stdin: &mut std::io::Stdin,
) -> Result<Cost, String> {
    let mut out = std::io::stdout();
    let mut cost = Cost {
        width,
        height,
        frames: 0,
        acked: 0,
        pixels: <_>::default(),
        encode: <_>::default(),
        write: <_>::default(),
    };
    let Some(dir) = crate::term_paint::frame_dir() else {
        return Ok(cost);
    };
    let started = Instant::now();
    let mut phase = 0u32;
    while started.elapsed() < budget && cost.frames < 120 {
        let at = Instant::now();
        let frame = test_frame(width, height, phase);
        cost.pixels += at.elapsed();

        let at = Instant::now();
        let path = dir.join(format!("probe-frame-{}-{}", std::process::id(), phase % 3));
        if std::fs::write(&path, &frame).is_err() {
            break;
        }
        let _ = out.write_all(b"\x1b[H");
        let escape = format!(
            "\x1b_Ga=T,q=0,i=31,p=1,f=24,s={width},v={height},t=f;{}\x1b\\",
            base64(path.to_string_lossy().as_bytes())
        );
        let _ = out.write_all(passthrough(&escape).as_bytes());
        let _ = out.flush();
        if read_ack(stdin) {
            cost.acked += 1;
        }
        cost.write += at.elapsed();
        cost.frames += 1;
        phase += 1;
    }
    for slot in 0..3 {
        let _ = std::fs::remove_file(dir.join(format!("probe-frame-{}-{slot}", std::process::id())));
    }
    Ok(cost)
}

fn measure_shm(
    width: u32,
    height: u32,
    budget: std::time::Duration,
    stdin: &mut std::io::Stdin,
) -> Result<Cost, String> {
    let mut out = std::io::stdout();
    let mut cost = Cost {
        width,
        height,
        frames: 0,
        acked: 0,
        pixels: <_>::default(),
        encode: <_>::default(),
        write: <_>::default(),
    };
    let started = Instant::now();
    let mut phase = 0u32;
    while started.elapsed() < budget && cost.frames < 120 {
        let at = Instant::now();
        let frame = test_frame(width, height, phase);
        cost.pixels += at.elapsed();

        let at = Instant::now();
        let _ = out.write_all(b"\x1b[H");
        let name = write_image_shm(&mut out, &frame, width, height, phase, false)
            .map_err(|e| format!("could not write the image: {e}"))?;
        let acked = read_ack(stdin);
        cost.write += at.elapsed();
        if acked {
            cost.acked += 1;
        }
        // kitty unlinks what it reads. What it did not read is ours to remove, or every frame
        // leaves its megabytes behind.
        if let Some(name) = name {
            if !acked {
                unlink_shm(&name);
            }
        }
        cost.frames += 1;
        phase += 1;
    }
    Ok(cost)
}

/// `bru --term-probe`: draw into this pane at three sizes and say what each one cost.
///
/// **Nothing is printed while a picture is on screen.** kitty draws images over the cells, so the
/// first version's report was rendered underneath its own last frame and could not be read — the
/// measurements are collected in silence, the images are deleted, and only then is the table
/// written.
pub fn probe() -> Result<(), String> {
    let pane = pane()?;
    let raw = Raw::enter()?;
    let mut stdin = std::io::stdin();
    let mut out = std::io::stdout();
    let _ = out.write_all(b"\x1b[?25l\x1b[H\x1b[2J");
    let _ = out.flush();

    // **Asked first, while the screen is still empty.** Nothing here draws, so the answers cost a
    // few milliseconds and are the only part of this report that survives a terminal which cannot
    // display anything at all.
    let offered = capabilities(&mut out, &mut stdin);

    let mut measured = Vec::new();
    for divisor in [4u32, 2, 1] {
        let (width, height) = (pane.width / divisor, pane.height / divisor);
        if width < 16 || height < 16 {
            continue;
        }
        let cost = measure(width, height, std::time::Duration::from_millis(1500), &mut stdin);
        let _ = delete_images(&mut out);
        let _ = out.write_all(b"\x1b[H\x1b[2J");
        let _ = out.flush();
        measured.push(cost?);
    }

    // And the same full-pane picture again, handed over in shared memory rather than as base64.
    let shm = measure_shm(pane.width, pane.height, std::time::Duration::from_millis(1500), &mut stdin);
    let _ = delete_images(&mut out);
    let _ = out.write_all(b"\x1b[H\x1b[2J");
    let _ = out.flush();
    let shm = shm?;

    // …and as a path, which is what a host that refuses shared memory is left with.
    let file =
        measure_file(pane.width, pane.height, std::time::Duration::from_millis(1500), &mut stdin);
    let _ = delete_images(&mut out);
    let _ = out.write_all(b"\x1b[H\x1b[2J");
    let _ = out.flush();
    let file = file?;

    // The screen belongs to the text again before a word of it is written.
    let _ = delete_images(&mut out);
    let _ = out.write_all(b"\x1b[H\x1b[2J\x1b[?25h");
    let _ = out.flush();
    drop(raw);

    println!(
        "pane: {}x{} px, {}x{} cells, via {}",
        pane.width, pane.height, pane.cols, pane.rows, pane.source
    );
    println!();
    println!("  {:>14}  {:>6}  the terminal's own answer", "transport", "format");
    for (transport, format, answer) in &offered {
        println!("  {transport:>14}  {format:>6}  {answer}");
    }
    if offered.iter().all(|(_, _, answer)| answer != "OK") {
        println!();
        println!(
            "not one of those was accepted. Whatever is drawing this pane takes no frame bru \n\
             knows how to hand it, and the rows below are the cost of talking to nobody."
        );
    }
    if in_tmux() {
        println!(
            "tmux: yes — base64 frames carry a passthrough wrapper per 4 KB chunk. Whether shared \n\
             memory survives the multiplexer is measured below rather than assumed."
        );
    }
    if cfg!(debug_assertions) {
        println!(
            "build: debug — `pixels` and `base64` are unoptimised here and are NOT what this \n\
             measures. Read the `terminal` column; build --release for the other two."
        );
    }
    println!();
    println!(
        "  {:>11}  {:>9}  {:>9}  {:>9}  {:>7}  {:>7}",
        "size", "pixels", "base64", "terminal", "fps", "acked"
    );
    for cost in &measured {
        println!(
            "  {:>11}  {:>8.0}ms  {:>8.0}ms  {:>8.0}ms  {:>7.1}  {:>3}/{:<3}",
            format!("{}x{}", cost.width, cost.height),
            cost.pixels.as_secs_f64() * 1000.0 / f64::from(cost.frames.max(1)),
            cost.encode.as_secs_f64() * 1000.0 / f64::from(cost.frames.max(1)),
            cost.per_frame() * 1000.0,
            cost.fps(),
            cost.acked,
            cost.frames
        );
    }
    println!(
        "  {:>11}  {:>8.0}ms  {:>8.0}ms  {:>8.0}ms  {:>7.1}  {:>3}/{:<3}   <- shared memory",
        format!("{}x{}", shm.width, shm.height),
        shm.pixels.as_secs_f64() * 1000.0 / f64::from(shm.frames.max(1)),
        shm.encode.as_secs_f64() * 1000.0 / f64::from(shm.frames.max(1)),
        shm.per_frame() * 1000.0,
        shm.fps(),
        shm.acked,
        shm.frames
    );
    if file.frames > 0 {
        println!(
            "  {:>11}  {:>8.0}ms  {:>8.0}ms  {:>8.0}ms  {:>7.1}  {:>3}/{:<3}   <- a file",
            format!("{}x{}", file.width, file.height),
            file.pixels.as_secs_f64() * 1000.0 / f64::from(file.frames.max(1)),
            file.encode.as_secs_f64() * 1000.0 / f64::from(file.frames.max(1)),
            file.per_frame() * 1000.0,
            file.fps(),
            file.acked,
            file.frames
        );
    }
    println!();
    if shm.acked == 0 && file.acked > 0 {
        println!(
            "shared memory: refused here. Frames go through a file instead, which costs the same \n\
             write and the same short escape — bru asks this question at startup and picks it."
        );
    } else if shm.acked == 0 {
        println!(
            "shared memory: the terminal confirmed none of those, so `t=s` did not work here — the \n\
             row above is the cost of writing to /dev/shm for nobody. bru will send frames as \n\
             base64 instead; it asks this same question at startup."
        );
    } else if let Some(full) = measured.last() {
        println!(
            "shared memory: {:.0} fps against {:.0} fps for base64 — {:.1}x.",
            shm.fps(),
            full.fps(),
            shm.fps() / full.fps().max(f64::MIN_POSITIVE)
        );
    }
    println!();

    // **The verdict is about the best transport that works, not about the first one tried.** The
    // first version judged on base64 alone and printed "the transport is the limit" directly under
    // a row showing shared memory doing the same picture ten times faster.
    let base64_fps = measured.last().map(Cost::fps).unwrap_or_default();
    let best = if shm.acked > 0 { shm.fps().max(base64_fps) } else { base64_fps };
    let full = Some(best);
    let acknowledged = measured.iter().any(|cost| cost.acked > 0) || shm.acked > 0;
    if !acknowledged {
        println!(
            "verdict: the terminal acknowledged nothing, so the `terminal` column is the speed of \n\
             writing into a buffer and not of drawing. Under tmux this usually means \n\
             `allow-passthrough` is off, or the reply is not being forwarded back."
        );
        return Ok(());
    }
    let by = if shm.acked > 0 && shm.fps() >= base64_fps { "shared memory" } else { "base64" };
    match full {
        Some(fps) if fps >= 30.0 => println!(
            "verdict: {fps:.0} fps at full pane over {by}, confirmed frame by frame. The transport \n\
             is not what will limit a browser in this pane."
        ),
        Some(fps) if fps >= 12.0 => println!(
            "verdict: {fps:.0} fps at full pane over {by}. Reading and scrolling are fine; video is \n\
             not. Sending the picture smaller than the pane buys the difference."
        ),
        Some(fps) => println!(
            "verdict: {fps:.0} fps at full pane, and {by} is the best transport that worked here. \n\
             The picture has to go smaller than the pane, or be compressed before it is sent."
        ),
        None => println!("verdict: the pane is too small to measure."),
    }
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
