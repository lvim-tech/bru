//! Network-level ad and tracker blocking, on Brave's engine.
//!
//! The same engine qutebrowser uses. qutebrowser reaches it through `python-adblock`, a binding
//! around the `adblock` crate; bru links the crate directly, so there is no FFI and no interpreter
//! between a request and the decision about it.
//!
//! **Where a blocked request dies.** Chromium asks
//! `RequestHandler::get_resource_request_handler` (bindings 26846) once per resource request, on the
//! browser process IO thread, *before* the request is initiated. bru answers `None` for everything
//! it allows — which is the overwhelming majority, and answering `None` costs CEF nothing — and for
//! a match it answers a [`BlockedResource`], whose
//! `ResourceRequestHandler::on_before_resource_load` (bindings 25548) returns
//! [`ReturnValue::CANCEL`]. Nothing goes on the wire: no connection, no DNS lookup, no cookie.
//!
//! Doing the matching in `get_resource_request_handler` rather than in `on_before_resource_load` is
//! deliberate. The second callback only exists once the first has handed CEF an object, so deciding
//! in the first means one refcounted CEF object is allocated per *blocked* request instead of one
//! per request.
//!
//! **Lists are data, not configuration.** They live under `~/.local/share/bru/adblock/`, beside the
//! history and the quickmarks, and the compiled form is cached next to them. bru ships none and
//! downloads none by itself; `:adblock-update` is the one thing that fetches, and it fetches because
//! somebody typed it.

use cef::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use adblock::Engine;
use adblock::lists::{FilterSet, ParseOptions, RuleTypes};
use adblock::request::Request as AdRequest;

/// qutebrowser's `content.blocking.adblock.lists` default (configdata.yml:886-889).
pub const DEFAULT_LISTS: [&str; 2] = [
    "https://easylist.to/easylist/easylist.txt",
    "https://easylist.to/easylist/easyprivacy.txt",
];

/// The compiled engine, or `None` until one has been loaded — and `None` forever if there are no
/// lists, which is the state of a fresh `~/.local/share/bru`.
///
/// A `RwLock` rather than a `Mutex` because every request takes it shared and only `:adblock-update`
/// takes it exclusively. `Engine` is `Send + Sync` as long as the crate's `single-thread` feature is
/// off — it swaps an `Rc` for an `Arc` and a `RefCell` for a `Mutex` inside — which is why
/// `Cargo.toml` turns default features off and names the two it wants.
static ENGINE: RwLock<Option<Engine>> = RwLock::new(None);

/// Whether blocking is on at all. Off while the engine loads, and `:adblock-toggle` moves it.
static ENABLED: AtomicBool = AtomicBool::new(true);

/// `BRU_ADBLOCK_DRYRUN=1`: match every request, count every match, cancel nothing.
///
/// It exists because "how much does this block, and what does it cost" is not answerable by running
/// bru twice with different code — the two runs would load different pages. In dry run the request
/// path is the same path down to the last instruction; only the `CANCEL` is missing. The difference
/// between the two runs is then the blocking and nothing else.
static DRY_RUN: AtomicBool = AtomicBool::new(false);

/// Cumulative matcher cost, so "what does this add per request" is a number bru can print about
/// itself rather than a claim. `MATCH_*` times the engine alone — building the `adblock::Request`
/// and checking it; `HOOK_*` times everything the CEF callback does, the two string round trips
/// through `CefString` included.
static MATCH_NS: AtomicU64 = AtomicU64::new(0);
static MATCH_N: AtomicU64 = AtomicU64::new(0);
static HOOK_NS: AtomicU64 = AtomicU64::new(0);
static HOOK_N: AtomicU64 = AtomicU64::new(0);

/// Requests seen and blocked, per page. Reset when a tab starts a new main-frame request, which is
/// how the numbers stay per-page rather than per-session.
static PAGES: Mutex<Option<HashMap<i32, Page>>> = Mutex::new(None);

/// Session totals, which survive the per-page reset.
static TOTAL_SEEN: AtomicU64 = AtomicU64::new(0);
static TOTAL_BLOCKED: AtomicU64 = AtomicU64::new(0);

/// The URL requests are still being downloaded for, and how many are outstanding. Only
/// `:adblock-update` touches it.
static UPDATE: Mutex<Option<Update>> = Mutex::new(None);

#[derive(Default, Clone)]
struct Page {
    url: String,
    seen: u64,
    blocked: u64,
}

struct Update {
    pending: usize,
    written: usize,
    /// The live `Urlrequest` objects. CEF starts them on creation and they must outlive the call
    /// that made them, so they are parked here until their client reports completion.
    requests: Vec<Urlrequest>,
}

/// `$XDG_DATA_HOME/bru/adblock`, where the `.txt` lists live.
///
/// The **one** thing this module asks of `src/data.rs`, which owns `~/.local/share/bru` and belongs
/// to another workstream: the directory itself. Everything below this point is this module's.
pub fn lists_dir() -> Option<PathBuf> {
    crate::data::data_dir().map(|dir| dir.join("adblock"))
}

/// The compiled engine, cached so that a second start does not re-parse 143,000 lines.
///
/// qutebrowser calls its equivalent `adblock-cache.dat` and keeps it directly in the data directory;
/// bru keeps it beside the lists it was built from, because the two are only meaningful together.
fn cache_path() -> Option<PathBuf> {
    lists_dir().map(|dir| dir.join("cache.dat"))
}

/// Start loading the engine, once per process, off the UI thread.
///
/// Called from `keys.rs` when the client is built — the earliest point in the browser process that
/// this module is reachable from without another workstream's file having to know about it. The
/// load is a second of work on a big list, so it does not happen on the thread that draws.
///
/// Until it finishes, [`resource_request_handler`] sees an empty `ENGINE` and blocks nothing. That
/// is the right failure: a few requests on the very first page go through unfiltered, rather than
/// the window not appearing until EasyList has been parsed.
pub fn ensure_loaded() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        if std::env::var_os("BRU_ADBLOCK_DISABLE").is_some() {
            ENABLED.store(false, Ordering::Relaxed);
            eprintln!("bru[adblock]: disabled by BRU_ADBLOCK_DISABLE");
            return;
        }
        if std::env::var_os("BRU_ADBLOCK_DRYRUN").is_some() {
            DRY_RUN.store(true, Ordering::Relaxed);
            eprintln!("bru[adblock]: dry run — matching and counting, cancelling nothing");
        }
        std::thread::spawn(|| {
            load();
            if let Some(ms) = report_interval_ms() {
                loop {
                    std::thread::sleep(std::time::Duration::from_millis(ms));
                    eprintln!("bru[adblock]: {}", session_info());
                }
            }
        });
    });
}

fn report_interval_ms() -> Option<u64> {
    std::env::var("BRU_ADBLOCK_REPORT_MS").ok()?.parse().ok()
}

/// Read the cache if it is usable, otherwise compile the lists, otherwise say why there is nothing.
fn load() {
    let Some(dir) = lists_dir() else {
        eprintln!("bru[adblock]: no data directory — nothing is being blocked");
        return;
    };
    let lists = list_files(&dir);

    if lists.is_empty() {
        // qutebrowser prints the same thing, for the same reason: an empty list directory is what a
        // first run looks like, and it is not an error.
        eprintln!(
            "bru[adblock]: no filter lists in {} — run :adblock-update to fetch them",
            dir.display()
        );
        return;
    }

    if let Some(cache) = cache_path().filter(|cache| is_cache_fresh(cache, &lists)) {
        let started = Instant::now();
        match std::fs::read(&cache) {
            Ok(bytes) => {
                let mut engine = Engine::default();
                match engine.deserialize(&bytes) {
                    Ok(()) => {
                        install(engine);
                        eprintln!(
                            "bru[adblock]: {} bytes of cache read in {:.0} ms",
                            bytes.len(),
                            started.elapsed().as_secs_f64() * 1000.0
                        );
                        return;
                    }
                    // The cache format carries a version and the crate makes no promise across
                    // releases of itself. A stale one is not corruption; recompiling is the answer,
                    // and it is what the next few lines do anyway.
                    Err(e) => eprintln!("bru[adblock]: cache unusable ({e:?}), recompiling"),
                }
            }
            Err(e) => eprintln!("bru[adblock]: cache unreadable ({e}), recompiling"),
        }
    }

    compile(&lists);
}

/// Compile every `.txt` in the lists directory into one engine, and cache the result.
fn compile(lists: &[PathBuf]) {
    let started = Instant::now();
    // Network rules only. Cosmetic filtering is element hiding — a stylesheet injected into the
    // page — and bru has no path for that yet, so parsing the cosmetic half would cost time and
    // memory for rules nothing would ever ask about.
    let options = ParseOptions { rule_types: RuleTypes::NetworkOnly, ..ParseOptions::default() };
    let mut set = FilterSet::new(false);
    let mut lines = 0usize;
    for path in lists {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                lines += text.lines().count();
                set.add_filter_list(text, options);
            }
            Err(e) => eprintln!("bru[adblock]: {} unreadable: {e}", path.display()),
        }
    }
    let read_ms = started.elapsed().as_secs_f64() * 1000.0;

    let compiled_at = Instant::now();
    let engine = Engine::new_with_filter_set(set);
    let compile_ms = compiled_at.elapsed().as_secs_f64() * 1000.0;

    let serialized = engine.serialize();
    if let Some(cache) = cache_path() {
        if let Err(e) = std::fs::write(&cache, &serialized) {
            eprintln!("bru[adblock]: could not write {}: {e}", cache.display());
        }
    }
    install(engine);

    eprintln!(
        "bru[adblock]: {} lists, {lines} lines — read {read_ms:.0} ms, compiled {compile_ms:.0} ms, \
         cache {} bytes",
        lists.len(),
        serialized.len()
    );
}

fn install(engine: Engine) {
    if let Ok(mut slot) = ENGINE.write() {
        *slot = Some(engine);
    }
}

/// Every `.txt` in the lists directory, sorted so that a rebuild is reproducible.
fn list_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "txt"))
        .collect();
    files.sort();
    files
}

/// Whether the cache is newer than every list it was built from. An older cache is not wrong, only
/// stale — and a stale ad blocker is exactly the thing nobody notices.
fn is_cache_fresh(cache: &Path, lists: &[PathBuf]) -> bool {
    let Ok(cache_time) = std::fs::metadata(cache).and_then(|m| m.modified()) else {
        return false;
    };
    lists.iter().all(|list| {
        std::fs::metadata(list)
            .and_then(|m| m.modified())
            .map(|list_time| list_time <= cache_time)
            .unwrap_or(false)
    })
}

// ---------------------------------------------------------------------------------------------
// The request path
// ---------------------------------------------------------------------------------------------

/// One decision per resource request, on the IO thread.
///
/// `Some` means blocked: the returned handler cancels in `on_before_resource_load`. `None` means
/// CEF handles the request the way it would if bru had no opinion, which is also the answer when
/// the engine has not loaded, when blocking is off, and for every scheme that is not http(s).
pub fn resource_request_handler(
    browser: Option<&mut Browser>,
    request: Option<&mut Request>,
) -> Option<ResourceRequestHandler> {
    if !ENABLED.load(Ordering::Relaxed) {
        return None;
    }
    let request = request?;
    let started = Instant::now();
    let verdict = decide(browser, request);
    // Only requests the engine actually saw are timed, so that `us/request` and `us/match` are two
    // measurements of the same population and their difference is bru's own overhead — the two
    // `CefString` round trips and the counter — rather than an artefact of a different denominator.
    if verdict.is_some() {
        HOOK_NS.fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
        HOOK_N.fetch_add(1, Ordering::Relaxed);
    }

    if DRY_RUN.load(Ordering::Relaxed) {
        return None;
    }
    verdict.unwrap_or(false).then(BlockedResource::new)
}

/// `Some(true)` if this request must not leave, `Some(false)` if the engine looked and said no, and
/// `None` if it never reached the engine at all.
fn decide(browser: Option<&mut Browser>, request: &mut Request) -> Option<bool> {
    let url = CefString::from(&request.url()).to_string();
    if !is_web(&url) {
        return None;
    }

    // Which tab this is being counted against. A request with no browser — a service worker's, or
    // one the network service makes on its own — is counted under 0.
    let id = browser.as_ref().map(|browser| browser.identifier()).unwrap_or(0);

    // Chromium's site-for-cookies is the same notion as qutebrowser's `first_party_url`: the page
    // the resource is being loaded *for*, which is what decides whether a request is third party.
    // It is empty on some requests — a top-level navigation is its own first party, and workers
    // have none — and blocking with no first party is what qutebrowser explicitly refuses to do
    // (braveadblock.py:186-193), because every URL matches when the engine believes everything is
    // third party.
    let mut first_party = CefString::from(&request.first_party_for_cookies()).to_string();
    if first_party.is_empty() {
        first_party = browser
            .and_then(|browser| browser.main_frame())
            .map(|frame| CefString::from(&frame.url()).to_string())
            .unwrap_or_default();
    }
    if !is_web(&first_party) {
        return None;
    }

    let resource_type = request.resource_type();
    let type_name = resource_type_name(resource_type);

    let started = Instant::now();
    let blocked = match ENGINE.read() {
        Ok(engine) => match engine.as_ref() {
            Some(engine) => match AdRequest::new(&url, &first_party, type_name, "GET") {
                Ok(ad_request) => engine.check_network_request(&ad_request).should_block(),
                // A URL the engine's own parser rejects. Never block on a parse failure.
                Err(_) => false,
            },
            None => false,
        },
        Err(_) => false,
    };
    MATCH_NS.fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
    MATCH_N.fetch_add(1, Ordering::Relaxed);

    count(id, resource_type, &url, &first_party, blocked);

    // Tab-separated and one line per request, so the log is also the request set: this is what the
    // offline benchmark of the matcher is replayed against, and a real page's mix of types and
    // third parties is not something worth inventing by hand.
    if std::env::var_os("BRU_DEBUG_ADBLOCK").is_some() {
        eprintln!(
            "bru[adblock]\t{}\t{type_name}\t{url}\t{first_party}",
            if blocked { "BLOCK" } else { "ALLOW" }
        );
    }
    Some(blocked)
}

/// Book-keeping. The one lock on the request path, and it is held for a handful of instructions.
///
/// A new main-frame request is a new page: whatever was counted against that tab belongs to the
/// page being left, so it is reported and the counters start again. Doing it here rather than on
/// load-end is what keeps this module from needing anything forwarded to it.
fn count(id: i32, resource_type: ResourceType, url: &str, first_party: &str, blocked: bool) {
    TOTAL_SEEN.fetch_add(1, Ordering::Relaxed);
    if blocked {
        TOTAL_BLOCKED.fetch_add(1, Ordering::Relaxed);
    }
    let Ok(mut guard) = PAGES.lock() else {
        return;
    };
    let pages = guard.get_or_insert_with(HashMap::new);

    // A main-frame request that is going ahead is the start of a new page. Report the old one and
    // start again — this is what makes "blocked 41 of 288" a sentence about a page.
    if resource_type == ResourceType::MAIN_FRAME && !blocked {
        if let Some(previous) = pages.get(&id).filter(|page| page.seen > 0) {
            eprintln!(
                "bru[adblock]: blocked {} of {} requests on {}",
                previous.blocked, previous.seen, previous.url
            );
        }
        pages.insert(id, Page { url: url.to_string(), seen: 0, blocked: 0 });
    }

    let page = pages.entry(id).or_insert_with(|| Page {
        url: first_party.to_string(),
        seen: 0,
        blocked: 0,
    });
    page.seen += 1;
    if blocked {
        page.blocked += 1;
    }
}

/// Only http and https are worth asking the engine about. `bru://`, `data:`, `blob:` and
/// `devtools://` are bru's own furniture or the page's own bytes, and the filter syntax has nothing
/// to say about them.
fn is_web(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

/// CEF's resource type in the spelling the filter syntax uses.
///
/// qutebrowser keeps the same table (`braveadblock.py:77-102`); this is it, against CEF's enum
/// rather than Qt's. The names are ABP's `$type` options, and a name the engine does not know makes
/// every `$type`-qualified rule miss — which is a silent under-block, so the fallback is `other`
/// exactly as qutebrowser's is.
fn resource_type_name(resource_type: ResourceType) -> &'static str {
    match resource_type {
        ResourceType::MAIN_FRAME => "main_frame",
        ResourceType::SUB_FRAME => "sub_frame",
        ResourceType::STYLESHEET => "stylesheet",
        ResourceType::SCRIPT => "script",
        ResourceType::IMAGE => "image",
        ResourceType::FONT_RESOURCE => "font",
        ResourceType::SUB_RESOURCE => "sub_frame",
        ResourceType::OBJECT => "object",
        ResourceType::MEDIA => "media",
        ResourceType::FAVICON => "image",
        ResourceType::XHR => "xhr",
        ResourceType::PING => "ping",
        ResourceType::CSP_REPORT => "csp_report",
        _ => "other",
    }
}

// The object CEF gets for a request bru has already decided against. Its only job is to say no —
// every other callback on the trait keeps its default. (The `wrap_` macros take no doc comment on
// the struct they declare: CEF-NOTES trap 8.)
wrap_resource_request_handler! {
    pub struct BlockedResource;

    impl ResourceRequestHandler {
        fn on_before_resource_load(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _request: Option<&mut Request>,
            _callback: Option<&mut Callback>,
        ) -> ReturnValue {
            ReturnValue::CANCEL
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------------------------

/// `:adblock-info` for the tab a key arrived at — what is loaded, what it has cost, what it caught.
pub fn info(browser_id: i32) -> String {
    let page = PAGES
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().and_then(|pages| pages.get(&browser_id).cloned()))
        .unwrap_or_default();
    format!(
        "{}; this page {}/{} blocked",
        session_info(),
        page.blocked,
        page.seen
    )
}

/// The part of [`info`] that is not about one page: whether there is an engine, whether it is being
/// asked, what it has caught, and **what it costs per request** — the number that decides whether
/// this feature is worth having at all, measured by bru on itself rather than asserted.
pub fn session_info() -> String {
    let loaded = ENGINE.read().map(|e| e.is_some()).unwrap_or(false);
    let (seen, blocked) = (
        TOTAL_SEEN.load(Ordering::Relaxed),
        TOTAL_BLOCKED.load(Ordering::Relaxed),
    );
    let match_n = MATCH_N.load(Ordering::Relaxed).max(1);
    let hook_n = HOOK_N.load(Ordering::Relaxed).max(1);
    let match_us = MATCH_NS.load(Ordering::Relaxed) as f64 / match_n as f64 / 1000.0;
    let hook_us = HOOK_NS.load(Ordering::Relaxed) as f64 / hook_n as f64 / 1000.0;

    format!(
        "engine {}, {} — session {blocked}/{seen} blocked, \
         {match_us:.2} us/match, {hook_us:.2} us/request",
        if loaded { "loaded" } else { "empty" },
        if ENABLED.load(Ordering::Relaxed) { "blocking" } else { "off" },
    )
}

/// `:adblock-toggle` — blocking on or off for the session, and the new state.
pub fn toggle() -> bool {
    let now = !ENABLED.load(Ordering::Relaxed);
    ENABLED.store(now, Ordering::Relaxed);
    now
}

/// `:adblock-update` — fetch [`DEFAULT_LISTS`] and recompile.
///
/// **This is the only thing in bru that reaches the network on its own account**, and it does it
/// because somebody typed the command. Nothing downloads a filter list on first run.
///
/// The fetch is CEF's own `Urlrequest` rather than an HTTP crate: bru already links a complete
/// network stack, and adding a second one — with its own TLS, its own root store and its own idea
/// of a proxy — to download two text files would be a strange trade.
pub fn update() {
    let mut task = UpdateTask::new();
    post_task(ThreadId::UI, Some(&mut task));
}

wrap_task! {
    struct UpdateTask;

    impl Task {
        fn execute(&self) {
            start_update();
        }
    }
}

fn start_update() {
    let Some(dir) = lists_dir() else {
        eprintln!("bru[adblock]: no data directory to write lists into");
        return;
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("bru[adblock]: could not create {}: {e}", dir.display());
        return;
    }

    // --- config commands (merge: this block belongs to the config-commands workstream) --------
    // [`DEFAULT_LISTS`] is now the `content.blocking.adblock.lists` setting's defaults rather than
    // the list itself, so `:config-list-add content.blocking.adblock.lists <url>` and a
    // `bru.set(…, { … })` in `config.lua` both reach this loop. With nothing set it answers exactly
    // the two lists this array holds, which is the state of a bru with no `~/.config/bru/`.
    let lists = crate::settings::list_of("content.blocking.adblock.lists");
    if lists.is_empty() {
        eprintln!("bru[adblock]: no filter lists configured — nothing to fetch");
        return;
    }
    // --- end config commands ------------------------------------------------------------------

    {
        let Ok(mut guard) = UPDATE.lock() else { return };
        if guard.is_some() {
            eprintln!("bru[adblock]: an update is already running");
            return;
        }
        *guard = Some(Update { pending: lists.len(), written: 0, requests: Vec::new() });
    }

    eprintln!("bru[adblock]: fetching {} lists", lists.len());
    for url in &lists {
        let url = url.as_str();
        let path = dir.join(list_filename(url));
        let Some(mut request) = request_create() else {
            finish_one(false);
            continue;
        };
        request.set_url(Some(&CefString::from(url)));
        request.set_method(Some(&CefString::from("GET")));

        let mut client = ListDownload::new(
            path,
            url.to_string(),
            Arc::new(Mutex::new(Vec::new())),
        );
        let created = urlrequest_create(Some(&mut request), Some(&mut client), None);
        match created {
            Some(created) => {
                if let Ok(mut guard) = UPDATE.lock() {
                    if let Some(update) = guard.as_mut() {
                        update.requests.push(created);
                    }
                }
            }
            None => finish_one(false),
        }
    }
}

/// `easylist.txt` out of `https://easylist.to/easylist/easylist.txt`, and something harmless out of
/// anything else. The name has to be a plain file name: it is joined onto bru's data directory.
fn list_filename(url: &str) -> String {
    let tail = url.rsplit('/').next().unwrap_or("");
    let stem: String = tail
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '-' || *c == '_')
        .collect();
    if stem.is_empty() || stem.starts_with('.') {
        return "list.txt".to_string();
    }
    if stem.ends_with(".txt") { stem } else { format!("{stem}.txt") }
}

/// One download finished. When the last one has, recompile — off the UI thread, because compiling
/// EasyList is a second of CPU and the UI thread is the one that draws.
fn finish_one(ok: bool) {
    let done = {
        let Ok(mut guard) = UPDATE.lock() else { return };
        let Some(update) = guard.as_mut() else { return };
        update.pending -= 1;
        if ok {
            update.written += 1;
        }
        if update.pending > 0 {
            return;
        }
        let written = update.written;
        *guard = None;
        written
    };

    if done == 0 {
        eprintln!("bru[adblock]: no lists were downloaded — nothing changed");
        return;
    }
    eprintln!("bru[adblock]: {done} lists downloaded, recompiling");
    std::thread::spawn(|| {
        if let Some(dir) = lists_dir() {
            let lists = list_files(&dir);
            if !lists.is_empty() {
                compile(&lists);
            }
        }
    });
}

wrap_urlrequest_client! {
    struct ListDownload {
        path: PathBuf,
        url: String,
        body: Arc<Mutex<Vec<u8>>>,
    }

    impl UrlrequestClient {
        fn on_download_data(
            &self,
            _request: Option<&mut Urlrequest>,
            data: *const u8,
            data_length: usize,
        ) {
            if data.is_null() || data_length == 0 {
                return;
            }
            // CEF hands over a borrowed buffer that is valid for this call only.
            let chunk = unsafe { std::slice::from_raw_parts(data, data_length) };
            if let Ok(mut body) = self.body.lock() {
                body.extend_from_slice(chunk);
            }
        }

        fn on_request_complete(&self, request: Option<&mut Urlrequest>) {
            let status = request
                .as_ref()
                .and_then(|request| request.response())
                .map(|response| response.status())
                .unwrap_or(0);
            let body = self.body.lock().map(|body| body.clone()).unwrap_or_default();

            // A 404 page is still bytes, and writing it over a good list would leave bru quietly
            // blocking nothing. An HTML error body is also not a filter list, so the first line has
            // to look like one.
            let looks_like_a_list = body
                .iter()
                .position(|b| *b == b'\n')
                .map(|end| &body[..end])
                .map(|line| line.starts_with(b"[") || line.starts_with(b"!"))
                .unwrap_or(false);

            if status != 200 || !looks_like_a_list {
                eprintln!(
                    "bru[adblock]: {} gave status {status} and {} bytes — not stored",
                    self.url,
                    body.len()
                );
                finish_one(false);
                return;
            }

            match std::fs::write(&self.path, &body) {
                Ok(()) => {
                    eprintln!(
                        "bru[adblock]: {} -> {} ({} bytes)",
                        self.url,
                        self.path.display(),
                        body.len()
                    );
                    finish_one(true);
                }
                Err(e) => {
                    eprintln!("bru[adblock]: could not write {}: {e}", self.path.display());
                    finish_one(false);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mapping CEF's enum gets into the filter syntax. `sub_resource` deliberately answers
    /// `sub_frame` and `favicon` answers `image`, the way qutebrowser's table does — the names are
    /// ABP's, and the two CEF types have no name of their own there.
    #[test]
    fn resource_types_are_spelled_the_way_the_filter_syntax_spells_them() {
        assert_eq!(resource_type_name(ResourceType::MAIN_FRAME), "main_frame");
        assert_eq!(resource_type_name(ResourceType::SCRIPT), "script");
        assert_eq!(resource_type_name(ResourceType::IMAGE), "image");
        assert_eq!(resource_type_name(ResourceType::FONT_RESOURCE), "font");
        assert_eq!(resource_type_name(ResourceType::SUB_RESOURCE), "sub_frame");
        assert_eq!(resource_type_name(ResourceType::FAVICON), "image");
        assert_eq!(resource_type_name(ResourceType::XHR), "xhr");
        assert_eq!(resource_type_name(ResourceType::WORKER), "other");
        assert_eq!(resource_type_name(ResourceType::PREFETCH), "other");
    }

    #[test]
    fn only_http_urls_reach_the_engine() {
        assert!(is_web("http://example.com/a.js"));
        assert!(is_web("https://example.com/a.js"));
        assert!(!is_web("bru://chrome/bottom.html"));
        assert!(!is_web("data:text/html;base64,AAA"));
        assert!(!is_web("blob:https://example.com/1234"));
        assert!(!is_web("devtools://devtools/bundled/x.js"));
        assert!(!is_web("file:///home/x/index.html"));
    }

    #[test]
    fn a_list_url_becomes_a_plain_file_name() {
        assert_eq!(list_filename("https://easylist.to/easylist/easylist.txt"), "easylist.txt");
        assert_eq!(list_filename("https://easylist.to/easylist/easyprivacy.txt"), "easyprivacy.txt");
        // No path separator survives, so a hostile list URL cannot write outside the directory.
        assert_eq!(list_filename("https://example.com/../../etc/passwd"), "passwd.txt");
        assert_eq!(list_filename("https://example.com/"), "list.txt");
        assert_eq!(list_filename("https://example.com/rules"), "rules.txt");
    }

    /// The engine, against rules written here rather than downloaded — a test that needs the
    /// network is a test that fails on a train.
    #[test]
    fn the_engine_blocks_what_the_rule_says_and_nothing_else() {
        let engine = Engine::new_with_list_text("||ads.example.com^\n@@||ads.example.com/allowed^");

        let blocked = AdRequest::new(
            "https://ads.example.com/banner.png",
            "https://news.example.org/",
            "image",
            "GET",
        )
        .unwrap();
        assert!(engine.check_network_request(&blocked).should_block());

        // An exception rule matched: there was a hit, and the request still goes through.
        let excepted = AdRequest::new(
            "https://ads.example.com/allowed/pixel.png",
            "https://news.example.org/",
            "image",
            "GET",
        )
        .unwrap();
        assert!(!engine.check_network_request(&excepted).should_block());

        let unrelated = AdRequest::new(
            "https://news.example.org/style.css",
            "https://news.example.org/",
            "stylesheet",
            "GET",
        )
        .unwrap();
        assert!(!engine.check_network_request(&unrelated).should_block());
    }

    /// `$type` qualifiers are the reason [`resource_type_name`] exists: get the name wrong and the
    /// rule misses, silently.
    #[test]
    fn the_type_name_is_what_makes_a_typed_rule_match() {
        let engine = Engine::new_with_list_text("||tracker.example.com^$script");
        let as_script = AdRequest::new(
            "https://tracker.example.com/t.js",
            "https://news.example.org/",
            "script",
            "GET",
        )
        .unwrap();
        assert!(engine.check_network_request(&as_script).should_block());

        let as_image = AdRequest::new(
            "https://tracker.example.com/t.js",
            "https://news.example.org/",
            "image",
            "GET",
        )
        .unwrap();
        assert!(!engine.check_network_request(&as_image).should_block());
    }

    /// Serialize/deserialize is the cache, and a cache that does not answer the same way is worse
    /// than no cache.
    #[test]
    fn the_cache_round_trips() {
        let engine = Engine::new_with_list_text("||ads.example.com^");
        let bytes = engine.serialize();
        let mut restored = Engine::default();
        restored.deserialize(&bytes).expect("the cache we just wrote must deserialize");

        let request = AdRequest::new(
            "https://ads.example.com/banner.png",
            "https://news.example.org/",
            "image",
            "GET",
        )
        .unwrap();
        assert!(restored.check_network_request(&request).should_block());
    }
}
