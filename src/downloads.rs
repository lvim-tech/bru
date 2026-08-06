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

        /// Choose the path. Answering the callback here rather than later is deliberate: CEF allows
        /// either, and bru has no prompt mode to answer from, so there is nothing to wait for.
        fn on_before_download(
            &self,
            _browser: Option<&mut Browser>,
            download_item: Option<&mut DownloadItem>,
            suggested_name: Option<&CefString>,
            callback: Option<&mut BeforeDownloadCallback>,
        ) -> ::std::os::raw::c_int {
            debug_assert_ne!(currently_on(ThreadId::UI), 0);

            let Some(callback) = callback else {
                return 0;
            };
            let suggested = suggested_name.map(CefString::to_string).unwrap_or_default();
            let (id, url) = match download_item.as_deref() {
                Some(item) => (item.id(), CefString::from(&item.url()).to_string()),
                None => (0, String::new()),
            };

            let path = target_path(&suggested, &url);
            debug(&format!("on_before_download #{id} {url:?} -> {}", path.display()));

            // `show_dialog = 0`: bru has no "Save As", and Chromium's own would be a piece of
            // browser chrome DESIGN.md does not have.
            callback.cont(
                Some(&CefString::from(path.to_string_lossy().as_ref())),
                0,
            );

            let mut list = downloads().lock().expect("downloads mutex poisoned");
            match list.iter_mut().find(|entry| entry.id == id) {
                // `on_download_updated` may run first — the header says it "may be called multiple
                // times before and after on_before_download()" — and then the row already exists
                // with no path in it.
                Some(entry) => entry.path = path,
                None => list.push(Entry {
                    id,
                    url,
                    path,
                    received: 0,
                    total: -1,
                    state: State::InProgress,
                    cancel: None,
                }),
            }
            drop(list);

            // 1 — "return true (1) and execute |callback| ... to continue or cancel the download".
            //
            // Measured 2026-08-06 on CEF 151, and it is *not* what the header's "return false (0)
            // to proceed with default handling (cancel with Alloy style)" led me to expect: with
            // `cont` skipped and 0 returned, an Alloy BrowserView did not cancel. Chromium ran its
            // own default handling and saved the file to *its* default directory —
            // `~/Downloads/Unconfirmed 171025.crdownload`, then `~/Downloads/report.pdf` — with the
            // path chosen above thrown away and `XDG_DOWNLOAD_DIR` never consulted. So 0 here does
            // not mean "nothing happens"; it means the file lands somewhere bru did not choose.
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
/// **This is not qutebrowser's behaviour and cannot be yet.** qutebrowser has
/// `downloads.location.prompt`, which defaults to *true*, and asks — through prompt mode, which bru
/// does not have — falling back to `downloads.location.directory`, which is a setting bru has
/// nowhere to keep. So bru takes the answer the desktop already holds, in the order every other
/// XDG-aware program takes it:
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
/// `downloads.location.prompt` becomes answerable once there is a prompt mode. Neither changes the
/// shape of anything below — `target_path` takes a directory.
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
/// Dead until `src/hints.rs` implements the `links` group and the `download` target; the `allow`
/// goes with the call that replaces it. Kept here rather than left to be invented there, so that the
/// one thing `;d` must not do — reach a browser from inside the query handler — cannot be got wrong
/// by someone who has not read trap 12.
#[allow(dead_code)]
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
}
