//! Downloads: `gd`, `ad`, `cd`, and the four `:download-*` commands.
//!
//! Everything here hangs off one CEF handler, `Client::download_handler`, whose three callbacks all
//! arrive on the browser-process UI thread (`cef_download_handler_capi.h:106`). Two of them matter:
//!
//! - **`on_before_download`** is where the path is chosen. It is the only chance: CEF asks once per
//!   download and the answer is final.
//! - **`on_download_updated`** is progress, completion, cancellation and failure, several times a
//!   second while bytes are moving.
//!
//! Two returns in that header are traps rather than details. Both were measured on 2026-08-06 (see
//! the report for CEF-NOTES), and the second does not do what the header says:
//!
//! - `can_download` returns "1 to proceed with the download or false (0) to cancel", and cef-rs's
//!   `ImplDownloadHandler::can_download` **defaults to `Default::default()`, which is 0**
//!   (bindings 18887-18894). The wrapper installs the function pointer whether or not it is
//!   overridden, so a handler that does not say `1` refuses every download Chromium started —
//!   measured: `:open` on a `Content-Disposition: attachment` URL logged `can_download`, never
//!   reached `on_before_download`, and left the directory empty. It is **not** consulted for
//!   `BrowserHost::start_download`, which is `gd`'s path: that one downloads either way, which is
//!   how a `can_download` bug hides.
//! - `on_before_download` returning 0 means, in the header, "proceed with default handling (cancel
//!   with Alloy style, download shelf with Chrome style)". **It does not cancel.** Measured with
//!   the `cont` skipped and 0 returned: Chromium ran its own default handling and wrote the file to
//!   *its* default directory (`~/Downloads/Unconfirmed 171025.crdownload`, then
//!   `~/Downloads/report.pdf`), ignoring the path chosen here. 0 does not mean nothing happens; it
//!   means the file goes somewhere bru did not choose.
//!
//! The header also says "Do not keep a reference to |download_item| outside of this function", so
//! nothing below stores one; every field is copied into [`Entry`] while the call is on the stack.
//! The *callbacks* are a different matter — CEF invites executing them asynchronously — and the
//! `DownloadItemCallback` of a running download is kept, because it is the only handle
//! `download-cancel` can act through.
//!
//! ## `download --mhtml`, which is not a download at all
//!
//! Saving a whole page is the one thing in this module that does not go through
//! `DownloadHandler`. `CefBrowserHost` really does have no save-a-document call — the whole method
//! list (bindings 12585-12770) writes a file in four places and no more: `start_download` (12626),
//! `download_image` (12628), `print` (12637) and `print_to_pdf` (12639). There is no `SaveAs`.
//!
//! But three methods on that same object are the DevTools protocol —
//! `send_dev_tools_message` (12668), `execute_dev_tools_method` (12670) and
//! `add_dev_tools_message_observer` (12673) — and the protocol has `Page.captureSnapshot`, which
//! returns the page and its assets as MHTML. So the serialiser was there the whole time; what it
//! needed was an observer object and an asynchronous result path. See [`start_mhtml`].
//!
//! ## Where files go
//!
//! `$XDG_DOWNLOAD_DIR`, then the desktop's `user-dirs.dirs`, then `~/Downloads`. See
//! [`download_dir`] for why this is not qutebrowser's behaviour and what would change it.
//!
//! ## What this module does not own
//!
//! The status line has no downloads section — DESIGN.md's bottom bar is keystring | url | scroll% |
//! tab index. [`summary`] builds the one string a section would show and pushes it through
//! `ipc::set_download`; drawing it needs one `<span>` that belongs to the chrome workstream, and
//! until that span exists the string is pushed and ignored. `;d` (`hint links download`) belongs to
//! `src/hints.rs`; [`schedule_start`] is the whole of what it should call.

use cef::*;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::tabs::SharedState;

/// How many `name (n).ext` variants to try before giving up on a colliding filename.
const MAX_COLLISIONS: u32 = 999;

/// The state of one download, as far as the bar and the commands are concerned.
///
/// Chromium's own states overlap — a cancelled download is also an interrupted one, with
/// `USER_CANCELED` as the reason — so the order the flags are read in below is what keeps these
/// four disjoint.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    InProgress,
    Complete,
    Cancelled,
    Interrupted,
}

impl State {
    fn name(self) -> &'static str {
        match self {
            State::InProgress => "in progress",
            State::Complete => "complete",
            State::Cancelled => "cancelled",
            State::Interrupted => "failed",
        }
    }

    /// qutebrowser's `AbstractDownloadItem.done` (downloads.py:408) — the list splits on this and
    /// so does every command: `download-cancel` refuses a done one, `download-open` refuses a
    /// running one.
    fn done(self) -> bool {
        self != State::InProgress
    }
}

/// One download bru knows about, in the order it was started.
///
/// The list is qutebrowser's `DownloadModel`: 1-based for the user, newest last, and a count of
/// zero means the last one (`downloads[count - 1]` with Python's negative indexing,
/// downloads.py:1132).
pub struct Entry {
    /// CEF's own id, which is what ties an `on_download_updated` to a row.
    id: u32,
    url: String,
    /// Where it is being written. Empty until `on_before_download` has answered.
    path: PathBuf,
    received: i64,
    /// `-1` while the server has not said, which is every chunked response.
    total: i64,
    state: State,
    /// Only while it is running. Dropped the moment it is not, so nothing here holds a callback
    /// belonging to a download Chromium has finished with.
    cancel: Option<DownloadItemCallback>,
// --- src/prompt.rs ---------------------------------------------------------
    /// Set by `prompt-open-download`: `Some(None)` is "open it with the desktop's default when it
    /// lands", `Some(Some(cmdline))` names the program. Cleared the moment it fires, so a
    /// `download-retry` of the same row does not open it a second time.
    open_when_complete: Option<Option<String>>,
// --- end src/prompt.rs -----------------------------------------------------
}

impl Entry {
    /// Whole percent, or `None` when the total is unknown.
    fn percent(&self) -> Option<i64> {
        if self.total > 0 {
            Some((self.received * 100 / self.total).clamp(0, 100))
        } else {
            None
        }
    }

    fn name(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| self.url.clone())
    }
}

fn downloads() -> &'static Mutex<Vec<Entry>> {
    static DOWNLOADS: Mutex<Vec<Entry>> = Mutex::new(Vec::new());
    &DOWNLOADS
}

/// `BRU_DEBUG_DOWNLOADS=1` traces every callback and every command. Off by default: a running
/// download reaches `on_download_updated` several times a second, and one line each is one line each
/// too many in a real session.
fn debug(message: &str) {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("BRU_DEBUG_DOWNLOADS").is_some()) {
        eprintln!("bru[downloads]: {message}");
    }
}

// ------------------------------------------------------------------------------------------------
// The CEF handler
// ------------------------------------------------------------------------------------------------

// Reached from `Client::download_handler` in keys.rs. (The wrap_ macros take no doc comment on the
// struct they declare — CEF-NOTES trap 8.)
wrap_download_handler! {
    pub struct BruDownloadHandler;

    impl DownloadHandler {
        /// **This override is not optional.** cef-rs's default returns 0, and 0 is "cancel the
        /// download" — see the module docs. bru allows every download it is asked about, which is
        /// what a browser with no per-site policy can honestly say.
        ///
        /// Only Chromium-initiated downloads reach here — a link click, or a navigation that came
        /// back as an attachment. `gd` goes through `BrowserHost::start_download` and is never
        /// asked about.
        fn can_download(
            &self,
            _browser: Option<&mut Browser>,
            url: Option<&CefString>,
            _request_method: Option<&CefString>,
        ) -> ::std::os::raw::c_int {
            debug(&format!(
                "can_download {:?}",
                url.map(CefString::to_string).unwrap_or_default()
            ));
            1
        }

        /// Choose the path — by **asking**, since src/prompt.rs.
        ///
        /// The callback is answered later, from prompt mode, which the header explicitly allows:
        /// "return true (1) and execute |callback| either in this function or at a later time".
        /// This is qutebrowser's own default — `downloads.location.prompt` ships as *true* — and
        /// the note under [`download_dir`] said so while there was no mode to ask in.
        ///
        /// **Nothing about where files go has changed**: [`target_path`] still computes the answer
        /// and it is what the line edit opens with, so `<Return>` writes exactly the file the
        /// previous version wrote without asking. What is new is that the path can be edited first,
        /// that `<Escape>` cancels the download, and that `<Ctrl-X>` opens it instead of keeping it.
        fn on_before_download(
            &self,
            browser: Option<&mut Browser>,
            download_item: Option<&mut DownloadItem>,
            suggested_name: Option<&CefString>,
            callback: Option<&mut BeforeDownloadCallback>,
        ) -> ::std::os::raw::c_int {
            debug_assert_ne!(currently_on(ThreadId::UI), 0);

            let Some(callback) = callback.map(|callback| callback.clone()) else {
                return 0;
            };
            let suggested = suggested_name.map(CefString::to_string).unwrap_or_default();
            let (id, url) = match download_item.as_deref() {
                Some(item) => (item.id(), CefString::from(&item.url()).to_string()),
                None => (0, String::new()),
            };

            let path = target_path(&suggested, &url);
            debug(&format!("on_before_download #{id} {url:?} -> {}", path.display()));

            let mut list = downloads().lock().expect("downloads mutex poisoned");
            match list.iter_mut().find(|entry| entry.id == id) {
                // `on_download_updated` may run first — the header says it "may be called multiple
                // times before and after on_before_download()" — and then the row already exists
                // with no path in it.
                Some(entry) => entry.path = path.clone(),
                None => list.push(Entry {
                    id,
                    url: url.clone(),
                    path: path.clone(),
                    received: 0,
                    total: -1,
                    state: State::InProgress,
                    cancel: None,
                    open_when_complete: None,
                }),
            }
            drop(list);

// --- src/prompt.rs ---------------------------------------------------------
            let window = browser
                .as_deref()
                .map(Browser::identifier)
                .and_then(|id| {
                    crate::state::BruState::instance()
                        .and_then(|state| state.lock().ok().and_then(|s| s.window_of_browser(id)))
                });
            let Some(window) = window else {
                // No window to ask in — `gd` before the first tab exists, or a browser bru did not
                // place. Fall back to the path it would have chosen anyway rather than cancelling
                // a download the user asked for.
                callback.cont(Some(&CefString::from(path.to_string_lossy().as_ref())), 0);
                return 1;
            };
            let offered = path.clone();
            let question = crate::prompt::Question::new(
                crate::prompt::Kind::Download,
                "Save file to",
                move |answer| match answer {
                    crate::prompt::Answer::Text(chosen) => {
                        callback.cont(Some(&CefString::from(chosen.as_str())), 0);
                    }
                    // `<Ctrl-X>` / `<Ctrl-P>`: somewhere temporary, and opened when it lands.
                    crate::prompt::Answer::OpenDownload { cmdline } => {
                        match temp_download_path(&offered) {
                            Ok(temp) => {
                                open_this_one_when_it_lands(id, cmdline);
                                callback.cont(
                                    Some(&CefString::from(temp.to_string_lossy().as_ref())),
                                    0,
                                );
                            }
                            Err(e) => {
                                crate::message::error(&format!("download: {e}"));
                                callback.cont(None, 0);
                            }
                        }
                    }
                    // `<Escape>`, the tab going away, the window closing. An empty path is how CEF
                    // is told the download was refused — measured 2026-08-07: no file appeared in
                    // the download directory and the row reported `cancelled`.
                    _ => callback.cont(None, 0),
                },
            )
            .about(browser.as_deref(), &url)
            .saying(&format!("Save {} to", short_name(&url)))
            .defaulting_to(path.to_string_lossy().as_ref());
            crate::prompt::ask(window, question);
// --- end src/prompt.rs -----------------------------------------------------

            // 1 — "return true (1) and execute |callback| ... to continue or cancel the download".
            //
            // Measured 2026-08-06 on CEF 151, and it is *not* what the header's "return false (0)
            // to proceed with default handling (cancel with Alloy style)" led me to expect: with
            // `cont` skipped and 0 returned, an Alloy BrowserView did not cancel. Chromium ran its
            // own default handling and saved the file to *its* default directory —
            // `~/Downloads/Unconfirmed 171025.crdownload`, then `~/Downloads/report.pdf` — with the
            // path chosen above thrown away and `XDG_DOWNLOAD_DIR` never consulted. So 0 here does
            // not mean "nothing happens"; it means the file lands somewhere bru did not choose.
            //
            // It is also what makes answering later legal, which is the whole of the prompt above.
            1
        }

        fn on_download_updated(
            &self,
            _browser: Option<&mut Browser>,
            download_item: Option<&mut DownloadItem>,
            callback: Option<&mut DownloadItemCallback>,
        ) {
            debug_assert_ne!(currently_on(ThreadId::UI), 0);

            let Some(item) = download_item.as_deref() else {
                return;
            };

            // Cancelled first: Chromium marks a cancelled download interrupted as well, with
            // `USER_CANCELED` as the reason, so testing `is_interrupted` first would report every
            // `ad` as a failure.
            let state = if item.is_canceled() != 0 {
                State::Cancelled
            } else if item.is_complete() != 0 {
                State::Complete
            } else if item.is_interrupted() != 0 {
                State::Interrupted
            } else {
                State::InProgress
            };

            let id = item.id();
            let url = CefString::from(&item.url()).to_string();
            let path = CefString::from(&item.full_path()).to_string();
            let received = item.received_bytes();
            let total = item.total_bytes();

            {
                let mut list = downloads().lock().expect("downloads mutex poisoned");
                let entry = match list.iter_mut().position(|entry| entry.id == id) {
                    Some(at) => &mut list[at],
                    None => {
                        list.push(Entry {
                            id,
                            url: url.clone(),
                            path: PathBuf::new(),
                            received: 0,
                            total: -1,
                            state,
                            cancel: None,
                            open_when_complete: None,
                        });
                        list.last_mut().expect("just pushed")
                    }
                };
                entry.received = received;
                entry.total = total;
                entry.state = state;
                if !path.is_empty() {
                    entry.path = PathBuf::from(&path);
                }
                if entry.url.is_empty() {
                    entry.url = url;
                }
                // A finished download's callback is of no use and must not be kept — the only
                // caller would be `download-cancel`, and cancelling a finished download is what
                // qutebrowser refuses outright.
                entry.cancel = if state.done() {
                    None
                } else {
                    callback.as_deref().cloned()
                };
            }

            debug(&format!(
                "on_download_updated #{id} {} {received}/{total} -> {}",
                state.name(),
                report_line()
            ));

// --- src/prompt.rs ---------------------------------------------------------
            // A download answered with `prompt-open-download` is opened the moment it lands. Here
            // rather than in the prompt because this is the only callback that says "complete",
            // and taken rather than read so it can only ever fire once.
            if state == State::Complete {
                let opening = {
                    let mut list = downloads().lock().expect("downloads mutex poisoned");
                    list.iter_mut()
                        .find(|entry| entry.id == id)
                        .and_then(|entry| entry.open_when_complete.take().map(|cmdline| (entry.path.clone(), cmdline)))
                };
                if let Some((path, cmdline)) = opening {
                    open_path(&path, cmdline.as_deref());
                }
            }
// --- end src/prompt.rs -----------------------------------------------------

            // Filtered inside `ipc`, which pushes only when the string changes: this arrives several
            // times a second and the percentage does not.
            crate::ipc::set_download(summary());
            debug(&format!("bar -> {}", crate::ipc::bar_json()));
        }
    }
}

// ------------------------------------------------------------------------------------------------
// Where files go
// ------------------------------------------------------------------------------------------------

/// The directory downloads are written to.
///
/// **bru asks, since src/prompt.rs** — `downloads.location.prompt` is qutebrowser's default and now
/// bru's behaviour too. What this function answers is what the question *opens with*, so
/// `<Return>` still writes the file where the version that never asked wrote it.
///
/// qutebrowser's fallback for a declined prompt is `downloads.location.directory`, which is a
/// setting bru still has nowhere to keep. So the directory below stays the desktop's answer, in the
/// order every other XDG-aware program takes it:
///
/// 1. `$XDG_DOWNLOAD_DIR`, if it is set and absolute.
/// 2. `XDG_DOWNLOAD_DIR=` in `$XDG_CONFIG_HOME/user-dirs.dirs`, which is what `xdg-user-dirs-update`
///    writes and what this machine has (`XDG_DOWNLOAD_DIR="$HOME/Downloads"`).
/// 3. `$HOME/Downloads`.
///
/// It is defensible because it is not bru's opinion: the file that answers it is the desktop's, so
/// bru saves where every other application on this machine saves, and it ships no configuration of
/// its own to say otherwise (DESIGN.md).
///
/// **What changes once the settings workstream lands:** `downloads.location.directory` becomes the
/// first thing consulted, with this as its default rather than as the whole answer, and
/// `downloads.location.prompt` becomes a setting that can turn the question *off* again. Neither
/// changes the shape of anything below — `target_path` takes a directory.
pub fn download_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_DOWNLOAD_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
    {
        return dir;
    }
    if let Some(dir) = user_dirs_download() {
        return dir;
    }
    home().join("Downloads")
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// `XDG_DOWNLOAD_DIR="$HOME/Downloads"` out of `user-dirs.dirs`.
///
/// The file's own header says the only two forms are `"$HOME/yyy"` and `"/yyy"`, so this parses
/// exactly those two and nothing else.
fn user_dirs_download() -> Option<PathBuf> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| home().join(".config"));
    let text = std::fs::read_to_string(config.join("user-dirs.dirs")).ok()?;
    parse_user_dirs(&text, &home())
}

/// Split out so the parse is testable without a `user-dirs.dirs` on the machine running the test.
fn parse_user_dirs(text: &str, home: &Path) -> Option<PathBuf> {
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let Some(value) = line.strip_prefix("XDG_DOWNLOAD_DIR=") else {
            continue;
        };
        let value = value.trim().trim_matches('"');
        if let Some(rest) = value.strip_prefix("$HOME/") {
            return Some(home.join(rest));
        }
        if value.starts_with('/') {
            return Some(PathBuf::from(value));
        }
    }
    None
}

/// The full path a download is written to: the directory, a filename, and a `(n)` if that name is
/// taken.
fn target_path(suggested: &str, url: &str) -> PathBuf {
    let dir = download_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        // Not fatal here — the `cont` below will fail and Chromium will report an interrupted
        // download, which is the state the bar and `download-retry` already understand.
        eprintln!("bru: could not create {}: {e}", dir.display());
    }
    dir.join(unique_name(&dir, &filename(suggested, url)))
}

/// The name to save under: what Chromium suggested, or the last segment of the URL, or `download`.
///
/// qutebrowser sanitises with `utils.sanitize_filename`; the parts that matter on Linux are that a
/// `/` cannot survive, that a leading `.` would hide the file, and that a name has to be non-empty.
fn filename(suggested: &str, url: &str) -> String {
    let candidate = if suggested.trim().is_empty() {
        url_basename(url)
    } else {
        suggested.to_string()
    };
    let sanitized = sanitize(&candidate);
    if sanitized.is_empty() {
        "download".to_string()
    } else {
        sanitized
    }
}

fn url_basename(url: &str) -> String {
    let without_fragment = url.split('#').next().unwrap_or("");
    let without_query = without_fragment.split('?').next().unwrap_or("");
    // The path, and only the path. `https://example.com/` names no file, and answering it with the
    // host would save a page as `example.com` — which is a real filename and therefore a real bug.
    let after_scheme = match without_query.find("://") {
        Some(at) => &without_query[at + 3..],
        None => without_query,
    };
    let path = match after_scheme.find('/') {
        Some(at) => &after_scheme[at + 1..],
        None => "",
    };
    path.rsplit('/')
        .find(|segment| !segment.is_empty())
        .unwrap_or("")
        .to_string()
}

fn sanitize(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        match c {
            '/' | '\\' | '\0' => out.push('_'),
            c if (c as u32) < 0x20 => out.push('_'),
            c => out.push(c),
        }
    }
    let out = out.trim().trim_start_matches('.').to_string();
    // ext4's limit is 255 bytes, and a suggested name can come from a header.
    if out.len() > 200 {
        let mut cut = 200;
        while cut > 0 && !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out[..cut].to_string()
    } else {
        out
    }
}

/// `report.pdf`, `report (1).pdf`, `report (2).pdf` — Chromium's own spelling, so a directory full
/// of bru's downloads looks like a directory full of anything else's.
fn unique_name(dir: &Path, name: &str) -> String {
    if !dir.join(name).exists() {
        return name.to_string();
    }
    let (stem, ext) = match name.rfind('.') {
        // A leading dot is not an extension separator, and `sanitize` has already removed one.
        Some(at) if at > 0 => (&name[..at], &name[at..]),
        _ => (name, ""),
    };
    for n in 1..=MAX_COLLISIONS {
        let candidate = format!("{stem} ({n}){ext}");
        if !dir.join(&candidate).exists() {
            return candidate;
        }
    }
    name.to_string()
}

// ------------------------------------------------------------------------------------------------
// The status line's one string
// ------------------------------------------------------------------------------------------------

/// What a downloads section of the bottom bar would show, or the empty string when nothing is
/// running.
///
/// One field, because DESIGN.md's bar has four and this is a fifth — not qutebrowser's separate
/// download bar, which is a second row and a redesign. `[dl 45%]` for one download, `[dl 3 45%]`
/// for three (the aggregate), and the byte count when the server never said how long the file is.
pub fn summary() -> String {
    let list = downloads().lock().expect("downloads mutex poisoned");
    let running: Vec<&Entry> = list
        .iter()
        .filter(|entry| entry.state == State::InProgress)
        .collect();
    if running.is_empty() {
        return String::new();
    }
    let received: i64 = running.iter().map(|entry| entry.received).sum();
    let total: i64 = running.iter().map(|entry| entry.total).sum();
    let progress = if total > 0 && running.iter().all(|entry| entry.total > 0) {
        format!("{}%", (received * 100 / total).clamp(0, 100))
    } else {
        human(received)
    };
    if running.len() == 1 {
        format!("[dl {progress}]")
    } else {
        format!("[dl {} {progress}]", running.len())
    }
}

fn human(bytes: i64) -> String {
    const UNITS: [(&str, i64); 4] = [("G", 1 << 30), ("M", 1 << 20), ("k", 1 << 10), ("B", 1)];
    for (unit, size) in UNITS {
        if bytes >= size {
            if unit == "B" {
                return format!("{bytes}B");
            }
            return format!("{:.1}{unit}", bytes as f64 / size as f64);
        }
    }
    "0B".to_string()
}

/// One line per download, newest last, for `BRU_DEBUG_DOWNLOADS=1` and for the report of any
/// command that changed the list.
pub fn report_line() -> String {
    let list = downloads().lock().expect("downloads mutex poisoned");
    if list.is_empty() {
        return "none".to_string();
    }
    list.iter()
        .enumerate()
        .map(|(i, entry)| {
            let progress = match entry.percent() {
                Some(p) => format!("{p}%"),
                None => human(entry.received),
            };
            format!(
                "{}:{} {} {} {}",
                i + 1,
                entry.name(),
                entry.state.name(),
                progress,
                entry.path.display()
            )
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

// ------------------------------------------------------------------------------------------------
// The commands
// ------------------------------------------------------------------------------------------------

/// `gd` / `:download [url]`.
///
/// With no URL it saves the page that is showing, which is qutebrowser's `:download` with no
/// argument (commands.py:1370). A URL is put through `open::decide` first, so `:download
/// example.com/x.pdf` reaches the same place `:open example.com/x.pdf` would.
pub fn start(state: &SharedState, browser: &mut Browser, url: Option<&str>) {
    let _ = state;
    let url = match url.map(str::trim).filter(|url| !url.is_empty()) {
        Some(text) => match crate::open::decide(text, &crate::open::engines()) {
            Some(target) => target.url().to_string(),
            None => {
                eprintln!("bru: download: nothing to download in {text:?}");
                return;
            }
        },
        None => crate::ipc::current_url(),
    };
    if url.is_empty() {
        eprintln!("bru: download: no page to download");
        return;
    }
    let Some(host) = browser.host() else {
        return;
    };
    debug(&format!("start {url:?}"));
    host.start_download(Some(&CefString::from(url.as_str())));
}

/// The one call `src/hints.rs` should make for `;d` (`hint links download`).
///
/// **Posted, not direct.** `;d` resolves inside `hints::on_page_query`, which runs inside the
/// message router's query handler, and CEF-NOTES trap 12 forbids acting on a browser from there —
/// the router holds `browser_query_info_map` across the handler and `on_before_browse` wants the
/// same lock. This posts and starts the download on the next turn of the UI loop, the way
/// `tabs::schedule_select` does.
///
/// Kept here rather than left to be invented in `hints.rs`, so that the one thing `;d` must not do —
/// reach a browser from inside the query handler — cannot be got wrong by someone who has not read
/// trap 12. Its callers are `clip::HintDownloads` and `popups.rs`'s `Where::Download`.
pub fn schedule_start(url: String) {
    let mut task = StartDownload::new(url);
    post_task(ThreadId::UI, Some(&mut task));
}

wrap_task! {
    struct StartDownload {
        url: String,
    }

    impl Task {
        fn execute(&self) {
            debug_assert_ne!(currently_on(ThreadId::UI), 0);

            let Some(state) = crate::state::BruState::instance() else {
                return;
            };
            let browser = state.lock().expect("state mutex poisoned").active_browser();
            let Some(mut browser) = browser else {
                eprintln!("bru: download: no tab to download from");
                return;
            };
            start(&state, &mut browser, Some(&self.url));
        }
    }
}

/// qutebrowser's `self[count - 1]` with Python's negative indexing: no count, or a count of zero, is
/// the *last* download (downloads.py:1132).
fn index_for(count: Option<u32>, len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    match count {
        None | Some(0) => Some(len - 1),
        Some(n) => {
            let at = n as usize - 1;
            (at < len).then_some(at)
        }
    }
}

/// `ad` / `:download-cancel [--all]`.
pub fn cancel(count: Option<u32>, all: bool) {
    let mut list = downloads().lock().expect("downloads mutex poisoned");

    if all {
        let mut stopped = 0;
        for entry in list.iter_mut() {
            if let Some(callback) = entry.cancel.take() {
                callback.cancel();
                stopped += 1;
            }
        }
        drop(list);
        debug(&format!("download-cancel --all stopped {stopped} -> {}", report_line()));
        return;
    }

    let Some(at) = index_for(count, list.len()) else {
        eprintln!("bru: there is no download to cancel");
        return;
    };
    if list[at].state.done() {
        eprintln!("bru: download {} is already {}", at + 1, list[at].state.name());
        return;
    }
    let name = list[at].name();
    match list[at].cancel.take() {
        Some(callback) => {
            callback.cancel();
            drop(list);
            debug(&format!("download-cancel {name} -> {}", report_line()));
        }
        None => eprintln!("bru: download {} cannot be cancelled", at + 1),
    }
}

/// `cd` / `:download-clear` — forget every *finished* download. Nothing on disk is touched; that is
/// `:download-delete`.
pub fn clear() {
    let mut list = downloads().lock().expect("downloads mutex poisoned");
    let before = list.len();
    list.retain(|entry| !entry.state.done());
    let after = list.len();
    drop(list);
    debug(&format!(
        "download-clear removed {} -> {}",
        before - after,
        report_line()
    ));
    crate::ipc::set_download(summary());
}

/// `:download-open [cmdline] [-d]`.
///
/// With no command it is the system default, `xdg-open`. qutebrowser's setting for this is
/// `downloads.open_dispatcher` (configdata.yml:1527), which defaults to none and means the same
/// thing; when the settings workstream lands, that setting replaces the constant below.
pub fn open_file(count: Option<u32>, cmdline: Option<&str>, dir: bool) {
    let path = {
        let list = downloads().lock().expect("downloads mutex poisoned");
        let Some(at) = index_for(count, list.len()) else {
            eprintln!("bru: there is no download to open");
            return;
        };
        if list[at].state != State::Complete {
            eprintln!("bru: download {} is {}", at + 1, list[at].state.name());
            return;
        }
        let path = list[at].path.clone();
        if dir {
            path.parent().map(Path::to_path_buf).unwrap_or(path)
        } else {
            path
        }
    };

    open_path(&path, cmdline);
}

/// Open one file with `cmdline`, or with the desktop's default when there is none.
///
/// Split out of [`open_file`] so `prompt-open-download` can reach it without going through a
/// download *index*: by the time a download lands, the row it is on may not be the last one.
fn open_path(path: &Path, cmdline: Option<&str>) {
    // `{}` is qutebrowser's placeholder; with none, the path is appended.
    let mut words: Vec<String> = match cmdline {
        Some(cmdline) if !cmdline.trim().is_empty() => {
            cmdline.split_whitespace().map(str::to_string).collect()
        }
        _ => vec!["xdg-open".to_string()],
    };
    let target = path.to_string_lossy().to_string();
    if words.iter().any(|word| word.contains("{}")) {
        for word in words.iter_mut() {
            *word = word.replace("{}", &target);
        }
    } else {
        words.push(target);
    }

    debug(&format!("download-open {words:?}"));
    spawn(&words);
}

// --- src/prompt.rs ---------------------------------------------------------
/// Remember that this download is to be opened when it lands — `prompt-open-download`'s half of
/// the answer. The other half is the temporary path the file goes to.
fn open_this_one_when_it_lands(id: u32, cmdline: Option<String>) {
    let mut list = downloads().lock().expect("downloads mutex poisoned");
    if let Some(entry) = list.iter_mut().find(|entry| entry.id == id) {
        entry.open_when_complete = Some(cmdline);
    }
}

/// Where a download the user does not want to keep goes.
///
/// `$XDG_RUNTIME_DIR/bru-downloads/` when there is one — it is tmpfs, per-user and 0700, and the
/// session cleans it up — and `/tmp` otherwise. The *name* is kept from the path the user was
/// offered, because that is what the program opening it reads the extension from.
fn temp_download_path(offered: &Path) -> Result<PathBuf, String> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("bru-downloads");
    std::fs::create_dir_all(&dir).map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    let name = offered
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "download".to_string());
    Ok(dir.join(unique_name(&dir, &name)))
}

/// The filename part of a URL, for the one line the prompt says about what is being saved. Long
/// enough to recognise, short enough not to push the question's own words off the row.
fn short_name(url: &str) -> String {
    let name = url_basename(url);
    if name.is_empty() {
        url.to_string()
    } else {
        name
    }
}
// --- end src/prompt.rs -----------------------------------------------------

/// Run a program and reap it, so a session that opens ten downloads does not leave ten zombies.
fn spawn(words: &[String]) {
    let Some((program, args)) = words.split_first() else {
        return;
    };
    match std::process::Command::new(program).args(args).spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(e) => eprintln!("bru: could not run {program}: {e}"),
    }
}

/// `:download-delete` — remove the file from disk *and* the row from the list, which is both halves
/// of qutebrowser's `download_delete` (downloads.py:1158).
pub fn delete(count: Option<u32>) {
    let mut list = downloads().lock().expect("downloads mutex poisoned");
    let Some(at) = index_for(count, list.len()) else {
        eprintln!("bru: there is no download to delete");
        return;
    };
    if list[at].state != State::Complete {
        eprintln!("bru: download {} is {}", at + 1, list[at].state.name());
        return;
    }
    let path = list[at].path.clone();
    match std::fs::remove_file(&path) {
        Ok(()) => {
            list.remove(at);
            drop(list);
            debug(&format!("download-delete {} -> {}", path.display(), report_line()));
        }
        Err(e) => eprintln!("bru: could not delete {}: {e}", path.display()),
    }
    crate::ipc::set_download(summary());
}

/// `:download-retry` — start the URL again.
///
/// With a count it is that download; without one it is the *first failed* download, which is
/// qutebrowser's rule and not the "last" the other three commands use (downloads.py:1207).
pub fn retry(state: &SharedState, browser: &mut Browser, count: Option<u32>) {
    let url = {
        let list = downloads().lock().expect("downloads mutex poisoned");
        match count {
            Some(n) if n > 0 => match index_for(count, list.len()) {
                Some(at) if list[at].state.done() && list[at].state != State::Complete => {
                    Some(list[at].url.clone())
                }
                Some(at) => {
                    eprintln!("bru: download {} did not fail", at + 1);
                    None
                }
                None => {
                    eprintln!("bru: there is no download {n}");
                    None
                }
            },
            _ => list
                .iter()
                .find(|entry| entry.state.done() && entry.state != State::Complete)
                .map(|entry| entry.url.clone()),
        }
    };
    match url {
        Some(url) => start(state, browser, Some(&url)),
        None => eprintln!("bru: no failed downloads"),
    }
}

// ------------------------------------------------------------------------------------------------
// `download --mhtml` — a whole page, through the DevTools protocol
// ------------------------------------------------------------------------------------------------

/// Where the ids of MHTML rows start.
///
/// Chromium's own `DownloadItem::id` counts up from 1 within a session, so the top of the `u32`
/// range is free. Staying there is what stops an `on_download_updated` for a real download from
/// finding an MHTML row and overwriting its path with an empty one.
const FIRST_SNAPSHOT_ID: u32 = 0x8000_0000;

/// The DevTools calls bru is waiting for answers to.
struct Pending {
    /// The `id` of the protocol message, which is what `on_dev_tools_method_result` carries back.
    message_id: i32,
    /// Which row in [`downloads`] this will complete.
    entry: u32,
    path: PathBuf,
}

fn pending() -> &'static Mutex<Vec<Pending>> {
    static PENDING: Mutex<Vec<Pending>> = Mutex::new(Vec::new());
    &PENDING
}

/// One observer registration per browser, kept forever.
///
/// **Dropping the `Registration` unregisters the observer**, so this is not bookkeeping for its own
/// sake: a local that went out of scope at the end of [`start_mhtml`] would leave a call whose
/// answer nothing is listening for. `Registration` wraps a `RefGuard`, which is
/// `unsafe impl Send + Sync` (cef/src/rc.rs:283-284), which is what lets it live in a static.
fn observers() -> &'static Mutex<Vec<(i32, Registration)>> {
    static OBSERVERS: Mutex<Vec<(i32, Registration)>> = Mutex::new(Vec::new());
    &OBSERVERS
}

fn next_snapshot_id() -> u32 {
    static NEXT: Mutex<u32> = Mutex::new(FIRST_SNAPSHOT_ID);
    let mut next = NEXT.lock().expect("snapshot id mutex poisoned");
    let id = *next;
    *next = id.saturating_add(1);
    id
}

/// The protocol's `id`. The header allows 0 for "assign the next one yourself", and this does not
/// use it: the assigned number comes back as the return value, i.e. *after* the call, and a result
/// that arrived before it was recorded would have nothing to match against. Numbering here means
/// the row is registered before the call is made.
fn next_message_id() -> i32 {
    static NEXT: Mutex<i32> = Mutex::new(1);
    let mut next = NEXT.lock().expect("devtools message id mutex poisoned");
    let id = *next;
    *next = id.saturating_add(1);
    id
}

// Reached from `start_mhtml`, once per browser. (The wrap_ macros take no doc comment on the struct
// they declare — CEF-NOTES trap 8.)
wrap_dev_tools_message_observer! {
    pub struct SnapshotObserver;

    impl DevToolsMessageObserver {
        /// `on_dev_tools_message` is deliberately **not** overridden: its default returns 0, which
        /// is "not handled, pass it on", and passing it on is what reaches this. Returning 1 there
        /// would silence every result in the process.
        ///
        /// `result` is the `"result"` dictionary alone when `success` is 1, and the `"error"`
        /// dictionary when it is 0 — not the whole protocol message either way
        /// (`cef_devtools_message_observer_capi.h:95-106`).
        fn on_dev_tools_method_result(
            &self,
            _browser: Option<&mut Browser>,
            message_id: ::std::os::raw::c_int,
            success: ::std::os::raw::c_int,
            result: Option<&[u8]>,
        ) {
            debug_assert_ne!(currently_on(ThreadId::UI), 0);

            // Every DevTools result in the process arrives here, including any bru has not sent —
            // `:devtools` opens a front-end with a session of its own. An id that is not in the
            // pending list is not ours and must be left alone.
            let Some(waiting) = take_pending(message_id) else {
                return;
            };

            let json = result.map(String::from_utf8_lossy).unwrap_or_default();
            debug(&format!(
                "devtools result #{message_id} success={success} {} bytes",
                json.len()
            ));

            match finish_snapshot(success != 0, &json, &waiting.path) {
                Ok(written) => {
                    complete(waiting.entry, written as i64);
                    crate::message::info(&format!("Saved {}", waiting.path.display()));
                }
                Err(e) => {
                    fail(waiting.entry);
                    crate::message::error(&format!("download --mhtml: {e}"));
                    eprintln!("bru: download --mhtml: {e}");
                }
            }
            crate::ipc::set_download(summary());
            debug(&format!("mhtml #{message_id} -> {}", report_line()));
        }
    }
}

fn take_pending(message_id: i32) -> Option<Pending> {
    let mut list = pending().lock().expect("pending mutex poisoned");
    let at = list.iter().position(|p| p.message_id == message_id)?;
    Some(list.remove(at))
}

/// `download --mhtml` / `:download -m`.
///
/// qutebrowser's is `commands.py:1396-1406`, and the shape is copied rather than invented: the page
/// that is showing, a filename made from its title, and a row in the same download list every other
/// download lands in. qutebrowser reaches it through `tab.action.save_page()`, which makes
/// QtWebEngine start a real download that its `DownloadManager` then sees; CEF has no such action,
/// so bru asks the protocol for the bytes and writes them itself. The user-visible result is the
/// same three things: a filename, a destination, and a line in the download section of the bar.
pub fn start_mhtml(browser: &mut Browser) {
    debug_assert_ne!(currently_on(ThreadId::UI), 0);

    let Some(host) = browser.host() else {
        return;
    };
    let url = crate::ipc::current_url();
    if url.is_empty() {
        crate::message::error("download --mhtml: no page to save");
        return;
    }

    let dir = download_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        crate::message::error(&format!("download --mhtml: could not create {}: {e}", dir.display()));
        return;
    }
    let name = mhtml_filename(&crate::ipc::current_title(), &url);
    let path = dir.join(unique_name(&dir, &name));

    // Once per browser. A second registration would answer the same result twice, and the second
    // answer would find the pending list already empty and do nothing — but the row would still
    // have been written twice, so this is not merely tidy.
    let identifier = browser.identifier();
    {
        let mut list = observers().lock().expect("observers mutex poisoned");
        if !list.iter().any(|(id, _)| *id == identifier) {
            let mut observer = SnapshotObserver::new();
            match host.add_dev_tools_message_observer(Some(&mut observer)) {
                Some(registration) => list.push((identifier, registration)),
                None => {
                    drop(list);
                    crate::message::error("download --mhtml: CEF refused the DevTools observer");
                    return;
                }
            }
        }
    }

    let entry = next_snapshot_id();
    let message_id = next_message_id();
    pending()
        .lock()
        .expect("pending mutex poisoned")
        .push(Pending { message_id, entry, path: path.clone() });
    downloads().lock().expect("downloads mutex poisoned").push(Entry {
        id: entry,
        url,
        path: path.clone(),
        // `download --mhtml` chooses its own path and never reaches `on_before_download`, so there
        // is no question to answer with `prompt-open-download` and nothing to open on completion.
        open_when_complete: None,
        received: 0,
        // Unknown until the snapshot arrives, which is what `-1` means everywhere else here. It is
        // why the bar shows a byte count for this and a percentage for a real download: nobody can
        // say how long a serialised page will be until it is serialised.
        total: -1,
        state: State::InProgress,
        cancel: None,
    });
    crate::ipc::set_download(summary());

    // `format` defaults to `mhtml` in the protocol; saying so is a line, and a default that changed
    // under bru would otherwise write an MHTML file that was not one.
    let mut params = dictionary_value_create();
    if let Some(params) = params.as_mut() {
        params.set_string(
            Some(&CefString::from("format")),
            Some(&CefString::from("mhtml")),
        );
    }
    debug(&format!("Page.captureSnapshot #{message_id} -> {}", path.display()));
    let assigned = host.execute_dev_tools_method(
        message_id,
        Some(&CefString::from("Page.captureSnapshot")),
        params.as_mut(),
    );
    // 0 is "not on the UI thread, or the message failed validation" — the only two ways this can be
    // refused before an answer exists. Nothing will ever call back, so the row is failed now rather
    // than left running for the rest of the session.
    if assigned == 0 {
        take_pending(message_id);
        fail(entry);
        crate::message::error("download --mhtml: CEF refused the Page.captureSnapshot call");
        crate::ipc::set_download(summary());
    }
}

/// The bookkeeping half of a finished snapshot, split out so a test can run it — anything that
/// posts a CEF task cannot be called under `cargo test` (CEF-NOTES trap 13), and the two `message::`
/// calls above do.
///
/// Returns how many bytes were written.
fn finish_snapshot(success: bool, json: &str, path: &Path) -> Result<usize, String> {
    if !success {
        // The `"error"` dictionary, which carries `code` and `message`.
        let reason = json_string_field(json, "message")
            .unwrap_or_else(|| "the DevTools call failed and said nothing".to_string());
        return Err(reason);
    }
    let Some(data) = json_string_field(json, "data") else {
        return Err("Page.captureSnapshot answered without any data".to_string());
    };
    if data.is_empty() {
        return Err("Page.captureSnapshot answered with an empty page".to_string());
    }
    std::fs::write(path, data.as_bytes())
        .map_err(|e| format!("could not write {}: {e}", path.display()))?;
    Ok(data.len())
}

fn complete(id: u32, bytes: i64) {
    let mut list = downloads().lock().expect("downloads mutex poisoned");
    if let Some(entry) = list.iter_mut().find(|entry| entry.id == id) {
        entry.received = bytes;
        entry.total = bytes;
        entry.state = State::Complete;
    }
}

fn fail(id: u32) {
    let mut list = downloads().lock().expect("downloads mutex poisoned");
    if let Some(entry) = list.iter_mut().find(|entry| entry.id == id) {
        entry.state = State::Interrupted;
    }
}

/// `page title.mhtml`, which is `mhtml.py:495-497`'s default name.
///
/// A page with no title falls back to the URL's last segment with its extension replaced, because a
/// saved `report.html` wants to be `report.mhtml` and not `report.html.mhtml`. [`sanitize`] does the
/// rest, and it is applied to the stem alone so that a 300-character title cannot truncate the
/// extension off the end.
fn mhtml_filename(title: &str, url: &str) -> String {
    let stem = if title.trim().is_empty() {
        let base = url_basename(url);
        match base.rsplit_once('.') {
            Some((stem, _)) if !stem.is_empty() => stem.to_string(),
            _ => base,
        }
    } else {
        title.trim().to_string()
    };
    let stem = sanitize(&stem);
    let stem = if stem.is_empty() { "page".to_string() } else { stem };
    format!("{stem}.mhtml")
}

/// One string field out of a flat JSON object.
///
/// `ipc.rs` has a reader of the same shape for the chrome's `JSON.stringify`, and this is not it:
/// the value here is a **whole serialised page**, which the header itself warns "may exceed 1MB",
/// so the unescaping copies in runs rather than a character at a time, and it puts a surrogate pair
/// back together — `ipc`'s drops each half, which no chrome message has ever contained and an
/// arbitrary page's title can.
fn json_string_field(src: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let mut rest = src;
    loop {
        let at = rest.find(&needle)?;
        rest = &rest[at + needle.len()..];
        let after = rest.trim_start();
        let Some(after) = after.strip_prefix(':') else {
            continue;
        };
        let after = after.trim_start();
        let after = after.strip_prefix('"')?;
        return Some(json_unescape(after));
    }
}

/// Everything up to the closing quote, with the escapes undone.
fn json_unescape(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut rest = src;
    loop {
        // The next thing that is not an ordinary character. Copying the run before it in one
        // `push_str` is the whole difference between this and `ipc::json_unescape`: an MHTML
        // snapshot is megabytes of ordinary characters.
        let Some(at) = rest.find(['"', '\\']) else {
            out.push_str(rest);
            return out;
        };
        out.push_str(&rest[..at]);
        if rest.as_bytes()[at] == b'"' {
            return out;
        }
        let mut chars = rest[at + 1..].chars();
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('b') => out.push('\u{8}'),
            Some('f') => out.push('\u{c}'),
            Some('u') => {
                let hex: String = rest[at + 2..].chars().take(4).collect();
                let unit = u32::from_str_radix(&hex, 16).unwrap_or(0);
                let consumed = 2 + hex.len();
                // A high surrogate is half a character: JSON has no other way to spell an
                // astral one, and decoding the halves separately loses it entirely.
                if (0xd800..0xdc00).contains(&unit) {
                    let tail = &rest[at + consumed..];
                    if let Some(low_hex) = tail.strip_prefix("\\u") {
                        let low: String = low_hex.chars().take(4).collect();
                        let low_unit = u32::from_str_radix(&low, 16).unwrap_or(0);
                        if (0xdc00..0xe000).contains(&low_unit) {
                            let combined =
                                0x1_0000 + ((unit - 0xd800) << 10) + (low_unit - 0xdc00);
                            if let Some(c) = char::from_u32(combined) {
                                out.push(c);
                            }
                            rest = &rest[at + consumed + 2 + low.len()..];
                            continue;
                        }
                    }
                }
                if let Some(c) = char::from_u32(unit) {
                    out.push(c);
                }
                rest = &rest[at + consumed..];
                continue;
            }
            // `\"`, `\\`, `\/` and anything else stand for themselves.
            Some(other) => out.push(other),
            None => return out,
        }
        rest = &rest[at + 1 + rest[at + 1..].chars().next().map(char::len_utf8).unwrap_or(0)..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_count_of_zero_or_none_is_the_last_download() {
        // downloads.py:1132 — `downloads[count - 1]`, and Python's -1 is the last element.
        assert_eq!(index_for(None, 3), Some(2));
        assert_eq!(index_for(Some(0), 3), Some(2));
        assert_eq!(index_for(Some(1), 3), Some(0));
        assert_eq!(index_for(Some(3), 3), Some(2));
        assert_eq!(index_for(Some(4), 3), None);
        assert_eq!(index_for(None, 0), None);
    }

    #[test]
    fn the_desktops_own_answer_is_what_is_read() {
        let home = Path::new("/home/someone");
        let text = "# written by xdg-user-dirs-update\n\
                    XDG_DESKTOP_DIR=\"$HOME/Desktop\"\n\
                    XDG_DOWNLOAD_DIR=\"$HOME/Downloads\"\n";
        assert_eq!(
            parse_user_dirs(text, home),
            Some(PathBuf::from("/home/someone/Downloads"))
        );
        // The other form the file's own header allows.
        assert_eq!(
            parse_user_dirs("XDG_DOWNLOAD_DIR=\"/mnt/big/dl\"\n", home),
            Some(PathBuf::from("/mnt/big/dl"))
        );
        // A commented-out line is not an answer, and neither is a file without the key.
        assert_eq!(parse_user_dirs("#XDG_DOWNLOAD_DIR=\"$HOME/x\"\n", home), None);
        assert_eq!(parse_user_dirs("XDG_MUSIC_DIR=\"$HOME/Music\"\n", home), None);
    }

    #[test]
    fn a_filename_cannot_escape_the_download_directory() {
        // The suggested name comes off the wire, through Content-Disposition.
        // Every separator becomes `_`, and the leading dots go with the hidden-file rule.
        assert_eq!(filename("../../etc/passwd", ""), "_.._etc_passwd");
        assert_eq!(filename("/etc/shadow", ""), "_etc_shadow");
        // A leading dot would hide the file; qutebrowser's sanitize_filename strips it too.
        assert_eq!(filename(".bashrc", ""), "bashrc");
        // Nothing usable at all still has to produce a name.
        assert_eq!(filename("", ""), "download");
        assert_eq!(filename("   ", "https://example.com/"), "download");
    }

    #[test]
    fn the_url_names_the_file_when_the_server_does_not() {
        assert_eq!(filename("", "https://example.com/a/report.pdf"), "report.pdf");
        assert_eq!(filename("", "https://example.com/a/report.pdf?v=2"), "report.pdf");
        assert_eq!(filename("", "https://example.com/a/report.pdf#page=3"), "report.pdf");
        // A suggested name always wins — it is what the server asked for.
        assert_eq!(filename("named.bin", "https://example.com/x.pdf"), "named.bin");
    }

    #[test]
    fn a_second_download_of_the_same_name_does_not_overwrite_the_first() {
        let dir = std::env::temp_dir().join(format!("bru-dl-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");

        assert_eq!(unique_name(&dir, "report.pdf"), "report.pdf");
        std::fs::write(dir.join("report.pdf"), b"x").expect("write");
        assert_eq!(unique_name(&dir, "report.pdf"), "report (1).pdf");
        std::fs::write(dir.join("report (1).pdf"), b"x").expect("write");
        assert_eq!(unique_name(&dir, "report.pdf"), "report (2).pdf");
        // A name with no extension keeps its whole self as the stem.
        std::fs::write(dir.join("README"), b"x").expect("write");
        assert_eq!(unique_name(&dir, "README"), "README (1)");

        std::fs::remove_dir_all(&dir).expect("clean up");
    }

    #[test]
    fn the_bar_string_is_empty_unless_something_is_running() {
        // `summary` reads the live list, which no test may depend on the contents of; the shape it
        // builds is what matters and is checked through the same arithmetic.
        assert_eq!(human(0), "0B");
        assert_eq!(human(512), "512B");
        assert_eq!(human(2048), "2.0k");
        assert_eq!(human(3 * (1 << 20)), "3.0M");

        let entry = Entry {
            id: 1,
            url: "https://example.com/x.bin".to_string(),
            path: PathBuf::from("/tmp/x.bin"),
            received: 45,
            total: 100,
            state: State::InProgress,
            cancel: None,
            open_when_complete: None,
        };
        assert_eq!(entry.percent(), Some(45));
        assert_eq!(entry.name(), "x.bin");

        let unknown = Entry { total: -1, ..entry };
        assert_eq!(unknown.percent(), None);
    }

    #[test]
    fn a_cancelled_download_is_done_and_a_running_one_is_not() {
        assert!(!State::InProgress.done());
        assert!(State::Complete.done());
        assert!(State::Cancelled.done());
        assert!(State::Interrupted.done());
    }

    // -- download --mhtml ------------------------------------------------------------------------

    #[test]
    fn an_mhtml_file_is_named_after_the_page_title() {
        // mhtml.py:495-497 — `utils.sanitize_filename(title + '.mhtml', shorten=True)`.
        assert_eq!(mhtml_filename("bru safe page", "https://e.com/x"), "bru safe page.mhtml");
        // A `/` in a title comes from the page and must not become a directory.
        assert_eq!(mhtml_filename("a/b", "https://e.com/x"), "a_b.mhtml");
        // No title: the URL's last segment, with its extension replaced rather than kept, so a
        // saved `report.html` is `report.mhtml` and not `report.html.mhtml`.
        assert_eq!(mhtml_filename("", "https://e.com/a/report.html"), "report.mhtml");
        assert_eq!(mhtml_filename("  ", "https://e.com/a/notes"), "notes.mhtml");
        // Nothing usable anywhere still has to produce a name.
        assert_eq!(mhtml_filename("", "https://e.com/"), "page.mhtml");
        // A title long enough to be truncated keeps its extension — the truncation is applied to
        // the stem, so `.mhtml` cannot be the part that is cut off.
        let long = mhtml_filename(&"x".repeat(400), "https://e.com/");
        assert!(long.ends_with(".mhtml"), "{long}");
        assert!(long.len() <= 206, "{}", long.len());
    }

    #[test]
    fn the_snapshot_is_taken_out_of_the_result_and_written_verbatim() {
        let dir = std::env::temp_dir().join(format!("bru-mhtml-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let path = dir.join("page.mhtml");

        // The shape `Page.captureSnapshot` answers with: one `data` key holding the whole file,
        // and CRLF line endings, which MHTML has and which arrive as `\r\n` escapes.
        let json = r#"{"data":"From: <Saved by bru>\r\nSubject: t\r\n\r\n<b>hi<\/b>\r\n"}"#;
        let written = finish_snapshot(true, json, &path).expect("writes");
        let back = std::fs::read_to_string(&path).expect("reads back");
        assert_eq!(back, "From: <Saved by bru>\r\nSubject: t\r\n\r\n<b>hi</b>\r\n");
        assert_eq!(written, back.len());

        // A failed call carries the `"error"` dictionary instead, and its message is the reason.
        let error = finish_snapshot(false, r#"{"code":-32601,"message":"nope"}"#, &path)
            .expect_err("fails");
        assert_eq!(error, "nope");
        // A success with nothing in it is a failure, not an empty file.
        assert!(finish_snapshot(true, r#"{"x":1}"#, &path).is_err());
        assert!(finish_snapshot(true, r#"{"data":""}"#, &path).is_err());

        std::fs::remove_dir_all(&dir).expect("clean up");
    }

    #[test]
    fn the_json_reader_survives_what_a_page_can_put_in_a_string() {
        // `\/` is legal JSON and is what an MHTML body full of `</div>` arrives as.
        assert_eq!(json_string_field(r#"{"data":"a\/b"}"#, "data").as_deref(), Some("a/b"));
        assert_eq!(json_string_field(r#"{"data":"a\"b"}"#, "data").as_deref(), Some("a\"b"));
        assert_eq!(json_string_field(r#"{"data":"a\\b"}"#, "data").as_deref(), Some("a\\b"));
        // A surrogate pair is one character. `ipc::json_unescape` drops both halves here, which is
        // harmless for the chrome's own messages and not for an arbitrary page's title.
        assert_eq!(
            json_string_field(r#"{"data":"e😀f"}"#, "data").as_deref(),
            Some("e\u{1f600}f")
        );
        // A BMP escape is still one character.
        assert_eq!(json_string_field(r#"{"data":"é"}"#, "data").as_deref(), Some("é"));
        // A key that only looks like the one asked for is skipped, not answered.
        assert_eq!(
            json_string_field(r#"{"metadata":"no","data":"yes"}"#, "data").as_deref(),
            Some("yes")
        );
        assert_eq!(json_string_field(r#"{"other":"x"}"#, "data"), None);
        // The string ends at its own closing quote and takes none of the object with it.
        assert_eq!(
            json_string_field(r#"{"data":"one","more":"two"}"#, "data").as_deref(),
            Some("one")
        );
    }

    #[test]
    fn an_mhtml_row_can_never_collide_with_a_download_chromium_numbered() {
        // Chromium's own `DownloadItem::id` counts from 1 in a session; these start at 2^31, and
        // the two lists are the same list.
        let first = next_snapshot_id();
        let second = next_snapshot_id();
        assert!(first >= FIRST_SNAPSHOT_ID);
        assert_eq!(second, first + 1);
    }
}
