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
// **`ResourceType` is not imported, and must not be.** `use cef::*` above brings CEF's own
// `ResourceType` — the one this file already matches `MAIN_FRAME` and `STYLESHEET` on — into scope,
// and the adblock crate has an unrelated enum of the same name. Naming the scriptlet one in full at
// its three uses is what keeps the request path's spelling untouched.
use adblock::resources::{PermissionMask, Resource};

/// qutebrowser's `content.blocking.adblock.lists` default (configdata.yml:886-889).
pub const DEFAULT_LISTS: [&str; 2] = [
    "https://easylist.to/easylist/easylist.txt",
    "https://easylist.to/easylist/easyprivacy.txt",
];

// -----------------------------------------------------------------------------------------------
// Scriptlets: the third mechanism, and the one that reaches inside a video
// -----------------------------------------------------------------------------------------------

/// The permission a filter list must carry before a `trusted-` scriptlet written in it will run.
///
/// **Only a file the user wrote by hand gets it**, and the check is the *path* rather than a flag:
/// [`trusted_filters_path`] is in `~/.config/bru/`, which is the directory a person edits, and every
/// downloaded list is in `~/.local/share/bru/adblock/`, which is where things bru fetched go. There
/// is deliberately no setting that marks a subscription trusted — that is the setting whose whole
/// purpose would be to let somebody be talked into ticking it.
///
/// What the permission buys is real: a `trusted-` scriptlet can rewrite **any** network response the
/// page receives, not only an advertisement. uBlock draws the same line in the same place, granting
/// it to "My filters" and to no subscribed list, including its own.
const TRUSTED: PermissionMask = PermissionMask::from_bits(1);

/// bru's scriptlet library, as the engine wants it.
///
/// A filter list's `##+js(name, arg, …)` rule names one of these; the engine substitutes the
/// arguments into `{{1}}`, `{{2}}`, … and hands the result back as `injected_script`, which the
/// renderer runs at document-start. Without this the field is always empty — which is exactly what
/// bru shipped until 2026-08-09, and why YouTube's advertisement survived both other layers.
///
/// **It is a handful and not uBlock's two hundred.** Each one is code bru executes in every page a
/// rule names, so each is here because something bru's user actually subscribes to needs it, and
/// each was read before it was shipped. The names and aliases are uBlock's, because the rules that
/// call them are written against uBlock's.
fn resources() -> Vec<Resource> {
    vec![
        Resource {
            name: "set-constant.js".to_string(),
            // **`set.js`, not `set`.** A rule writes `+js(set, …)` and the engine canonicalises the
            // name by appending `.js` before it looks anything up — `with_js_extension`. An alias
            // spelled the way the rule spells it therefore matches nothing, silently: measured
            // 2026-08-09, four `set` rules resolved to **zero** scriptlets while the two whose name
            // already ended in `.js` resolved fine, and the only symptom was that YouTube still
            // played its advertisement.
            aliases: vec!["set.js".to_string()],
            kind: adblock::resources::ResourceType::Template,
            content: base64_of(include_str!("../chrome/scriptlets/set-constant.js")),
            dependencies: Vec::new(),
            permission: PermissionMask::default(),
        },
        Resource {
            name: "trusted-replace-fetch-response.js".to_string(),
            aliases: vec!["trusted-replace-fetch-response".to_string()],
            kind: adblock::resources::ResourceType::Template,
            content: base64_of(include_str!(
                "../chrome/scriptlets/trusted-replace-fetch-response.js"
            )),
            dependencies: Vec::new(),
            permission: TRUSTED,
        },
    ]
}

/// `Resource::content` is base64, and this is the whole of what bru needs of it — a dependency for
/// one encode of two files at startup would be a dependency for nothing.
fn base64_of(text: &str) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = text.as_bytes();
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

/// `~/.config/bru/filters.txt` — the user's own filter rules, and the only list bru trusts.
///
/// In the **config** directory rather than beside the downloaded lists, and that is the whole
/// mechanism: it is hand-written, so it is the user's, so it may say things a stranger's list may
/// not. bru never creates it and never writes to it, exactly as it never writes `config.lua`.
pub fn trusted_filters_path() -> Option<PathBuf> {
    crate::config::config_path()?.parent().map(|dir| dir.join("filters.txt"))
}

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
///
/// **The name carries what is in it, and that is the whole of the migration.** The cache used to be
/// `cache.dat` and held network rules only. Nothing inside a serialized engine says which rule types
/// built it, and [`is_cache_fresh`] compares timestamps — so a `cache.dat` newer than the lists
/// would have been loaded happily on the day cosmetic filtering landed, and the new layer would
/// have hidden nothing at all, silently, on every machine that had ever run `:adblock-update`. That
/// is the exact failure this file's own comment calls "a stale ad blocker is exactly the thing
/// nobody notices". A cache built the old way now simply is not this file, so it is not read.
fn cache_path() -> Option<PathBuf> {
    lists_dir().map(|dir| dir.join("cache.all.dat"))
}

/// The name the cache had before it held the cosmetic half. Read by nothing; removed once, so that
/// a five-megabyte file nobody will ever open again does not sit in the user's data directory for
/// ever.
fn legacy_cache_path() -> Option<PathBuf> {
    lists_dir().map(|dir| dir.join("cache.dat"))
}

/// Drop the pre-cosmetic cache if it is still there. Called once, from [`load`].
fn forget_legacy_cache() {
    let Some(old) = legacy_cache_path() else {
        return;
    };
    if !old.exists() {
        return;
    }
    match std::fs::remove_file(&old) {
        Ok(()) => eprintln!(
            "bru[adblock]: removed {}, which held network rules only",
            old.display()
        ),
        // Not being able to delete it costs nothing but the space: it is never read either way.
        Err(e) => eprintln!("bru[adblock]: could not remove {}: {e}", old.display()),
    }
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

    forget_legacy_cache();

    // **The user's own file is watched too, or editing it would change nothing until a list was
    // re-downloaded.** `is_cache_fresh` compares timestamps, and `filters.txt` is the one input
    // that changes without `:adblock-update` being run — it is edited by hand, which is the whole
    // point of it.
    let mut watched = lists.clone();
    if let Some(own) = trusted_filters_path().filter(|path| path.exists()) {
        watched.push(own);
    }

    if let Some(cache) = cache_path().filter(|cache| is_cache_fresh(cache, &watched)) {
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
    // **Both halves of every list, since the cosmetic layer landed.** This said `NetworkOnly` and
    // gave the reason: "bru has no path for that yet". It has one now — the renderer installs a
    // stylesheet at `on_context_created`, the same door `userstyles.rs` and `scrollbar.rs` use, and
    // [`cosmetic_css`] is what fills it.
    //
    // Say what the second half costs, measured on this machine over the two lists bru ships —
    // EasyList and EasyPrivacy, 143,933 lines — on 2026-08-09:
    //
    // ```text
    //               compile      cache.dat
    //   NetworkOnly   134 ms      5,069,637 bytes
    //   All           145 ms      6,372,973 bytes
    // ```
    //
    // Eleven milliseconds and 1.3 MB. The compile happens once per list change, on a thread of its
    // own, so those eleven milliseconds are not on any path a person waits at.
    //
    // **Nothing on the request path changed**, which is the number that would have mattered: the
    // network matcher is the same matcher over the same rules, and the cosmetic rules sit in their
    // own structure that only [`cosmetic_css`] reads, once per document rather than once per
    // request.
    let options = ParseOptions { rule_types: RuleTypes::All, ..ParseOptions::default() };
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

    // The user's own rules, and the one list that carries [`TRUSTED`]. Added last so that reading
    // the count above still describes what was downloaded.
    if let Some(path) = trusted_filters_path() {
        if let Ok(text) = std::fs::read_to_string(&path) {
            let own = text.lines().filter(|l| !l.trim().is_empty() && !l.starts_with('!')).count();
            lines += text.lines().count();
            set.add_filter_list(
                text,
                ParseOptions { permissions: TRUSTED, ..options },
            );
            eprintln!("bru[adblock]: {own} rule(s) of your own from {}", path.display());
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

    // The other half of the answer: compiling takes about a second and happens on a thread of its
    // own, so "downloaded" and "blocking now" are two different moments and both are worth saying.
    crate::message::info(&format!(
        "adblock: {} lists compiled, {lines} rules — blocking",
        lists.len()
    ));
    eprintln!(
        "bru[adblock]: {} lists, {lines} lines — read {read_ms:.0} ms, compiled {compile_ms:.0} ms, \
         cache {} bytes",
        lists.len(),
        serialized.len()
    );
}

fn install(mut engine: Engine) {
    // **Every path installs the library, including the cached one.** The cache holds the compiled
    // rules; the scriptlets are bru's own code and travel with the binary, so a build that adds one
    // must not need the cache thrown away to have it.
    engine.use_resources(resources());
    if let Ok(mut slot) = ENGINE.write() {
        *slot = Some(engine);
    }
}

// -----------------------------------------------------------------------------------------------
// The cosmetic layer: the half of a filter list that hides what the network layer let through
// -----------------------------------------------------------------------------------------------

/// Renderer → browser: "this frame is at <url>; what should it hide?"
const COSMETIC_ASK: &str = "bru.cosmetic.ask";

/// Browser → renderer: the stylesheet, or an empty string for a page with nothing to hide.
const COSMETIC_CSS: &str = "bru.cosmetic.css";

/// **What this layer is for, and what the network layer cannot do.**
///
/// Cancelling a request stops the advert from loading. It does not stop the *hole* — the empty
/// 300x250 box, the reserved column that shifts the text sideways, the cookie wall, the "turn off
/// your ad blocker" sheet. None of those is a request; they are the page's own markup, and only
/// hiding an element removes them. The rules are already in the two lists bru downloads; until now
/// they were parsed away — see [`compile`].
///
/// **The rules this asks for are the host's own.** `url_cosmetic_resources` answers with the
/// selectors written against *this hostname* (`example.com##.promo`), and deliberately not with the
/// generic ones (`##.ad-banner`). There are hundreds of thousands of those and Brave's engine hands
/// them over the other way round: the page reports the classes and ids it actually contains and
/// asks `hidden_class_id_selectors` about those. That needs a `MutationObserver` in the page,
/// because at [`renderer_on_context_created`] — which is document-start — the document is still
/// empty and has no classes to report. **It is not implemented yet**, and this comment is the note
/// saying which half is missing rather than a silence implying there is none.
///
/// **The scriptlet comes back with it, which is the third mechanism.** `injected_script` is
/// assembled by resolving each `##+js(name, …)` rule against [`resources`]; it was always empty
/// before bru had a library to resolve against, and reading it was pointless. It is the only one of
/// the three layers that can reach an advertisement *inside* a video: that one arrives over the
/// same connection as the video and is announced in the player's own response, so there is no
/// request to cancel and no element to hide — only the response to edit before the player reads it.
fn cosmetic_for(url: &str) -> (String, String) {
    if !ENABLED.load(Ordering::Relaxed) {
        return (String::new(), String::new());
    }
    let Ok(guard) = ENGINE.read() else {
        return (String::new(), String::new());
    };
    let Some(engine) = guard.as_ref() else {
        return (String::new(), String::new());
    };
    let resources = engine.url_cosmetic_resources(url);
    // `$generichide` is an exception rule a list writer put on a page that cosmetic filtering
    // breaks. It only ever governs the generic half, which is the half not implemented above — so
    // it changes nothing today, and reading it here is what makes that true rather than luck.
    (
        stylesheet(resources.hide_selectors.iter().map(String::as_str)),
        resources.injected_script,
    )
}

/// The selectors as one CSS rule, or the empty string when there are none.
///
/// Separated from the engine so the shape of the sheet is a test rather than a claim — nothing here
/// touches CEF or the engine.
///
/// **A selector that could end the rule early is dropped.** The parser that produced these accepts
/// only what it recognises as a selector, so a brace should never arrive; the guard costs one scan
/// of a string bru is already copying, and what it prevents is one malformed list turning the whole
/// sheet into whatever follows the brace. One list line must not be able to rewrite the others.
fn stylesheet<'a>(selectors: impl Iterator<Item = &'a str>) -> String {
    let mut kept: Vec<&str> = selectors
        .filter(|selector| {
            !selector.is_empty()
                && !selector.contains(['{', '}', '\n', '\r'])
        })
        .collect();
    if kept.is_empty() {
        return String::new();
    }
    // Sorted so that the same page produces the same sheet twice — `hide_selectors` is a `HashSet`,
    // whose order is its own business, and a sheet that differs between two loads of one page is a
    // sheet nobody can diff when something goes wrong.
    kept.sort_unstable();
    let mut css = String::with_capacity(kept.iter().map(|s| s.len() + 1).sum::<usize>() + 32);
    for (index, selector) in kept.iter().enumerate() {
        if index > 0 {
            css.push(',');
        }
        css.push_str(selector);
    }
    css.push_str("{display:none!important}");
    css
}

/// Whether this frame is one the cosmetic layer speaks for: a real web page.
///
/// `bru://` is bru's own UI and no filter list has an opinion about it; `about:blank` and the like
/// have no hostname for a rule to be written against.
fn hideable(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

/// Renderer side, at document start. Installs the keeper and asks the browser process what to hide.
///
/// **Why the question travels rather than the engine.** The engine is 6 MB of compiled rules in the
/// browser process; there is one of it and there are as many renderers as Chromium decides to make.
/// A round trip per document is the cheap direction.
///
/// The keeper is `userstyles.rs`'s, installed here too rather than depended upon: it refuses to
/// install twice, and a layer that only works when another module happened to run first is a layer
/// with a bug waiting in it. It is what makes the hiding survive a page that deletes the element —
/// which is exactly what a page fighting an ad blocker does.
pub fn renderer_on_context_created(frame: Option<&Frame>) {
    let Some(frame) = frame else {
        return;
    };
    if !hideable(&CefString::from(&frame.url()).to_string()) {
        return;
    }
    crate::greasemonkey::evaluate(frame, crate::userstyles::KEEPER_JS, Some("bru://userstyle.js"));
    let Some(mut message) = process_message_create(Some(&CefString::from(COSMETIC_ASK))) else {
        return;
    };
    frame.send_process_message(ProcessId::BROWSER, Some(&mut message));
}

/// Browser side of [`COSMETIC_ASK`]. Answers true when the message was ours.
///
/// The URL is read from the **frame the message arrived on**, not from an argument the renderer
/// filled in: the frame is the browser process's own record of where that frame is, and a renderer
/// naming its own URL is a renderer that could name someone else's.
pub fn on_cosmetic_ask(frame: Option<&Frame>, message: Option<&ProcessMessage>) -> bool {
    let Some(message) = message else {
        return false;
    };
    if CefString::from(&message.name()).to_string() != COSMETIC_ASK {
        return false;
    }
    let Some(frame) = frame else {
        return true;
    };
    let url = CefString::from(&frame.url()).to_string();
    let (css, script) = if hideable(&url) {
        cosmetic_for(&url)
    } else {
        (String::new(), String::new())
    };

    if std::env::var_os("BRU_DEBUG_ADBLOCK").is_some() {
        eprintln!(
            "bru[adblock]: cosmetic for {url}: {} bytes of CSS, {} of scriptlet",
            css.len(),
            script.len()
        );
    }

    let Some(mut reply) = process_message_create(Some(&CefString::from(COSMETIC_CSS))) else {
        return true;
    };
    if let Some(arguments) = reply.argument_list() {
        arguments.set_string(0, Some(&CefString::from(css.as_str())));
        arguments.set_string(1, Some(&CefString::from(script.as_str())));
    }
    frame.send_process_message(ProcessId::RENDERER, Some(&mut reply));
    true
}

/// Renderer side of [`COSMETIC_CSS`]: put the sheet in, or take it out again.
///
/// An empty sheet calls `off()` rather than setting nothing, so that `:adblock-toggle` followed by a
/// reload leaves no stylesheet behind on a page that had one.
pub fn renderer_on_cosmetic_css(frame: Option<&Frame>, message: Option<&ProcessMessage>) -> bool {
    let Some(message) = message else {
        return false;
    };
    if CefString::from(&message.name()).to_string() != COSMETIC_CSS {
        return false;
    }
    let Some(frame) = frame else {
        return true;
    };
    let (css, script) = message
        .argument_list()
        .map(|arguments| {
            (
                CefString::from(&arguments.string(0)).to_string(),
                CefString::from(&arguments.string(1)).to_string(),
            )
        })
        .unwrap_or_default();

    // **The scriptlet first, and as its own script.** It is the only one of the two that races the
    // page — it has to have run before the page's own code reads the object it edits — while a
    // stylesheet arriving a millisecond later changes nothing anybody sees. Running it separately
    // also keeps a throw inside one rule from taking the stylesheet down with it.
    //
    // **The ask round trip wins that race, and by how much is measured, not hoped.** The question
    // leaves at `on_context_created` and this answer is a posted task, so on paper the page's own
    // inline script could run first. Measured 2026-08-09 on `youtube.com/watch`, instrumented
    // inside the scriptlet: it ran at 166 ms in page time on a cold load and 212 ms on a reload,
    // and the page assigned `ytInitialPlayerResponse` at 1,258 ms and 1,421 ms — over a second of
    // margin both times, because the answer crosses two processes on one machine while the
    // assignment sits behind 1.5 MB of HTML that YouTube serves uncached even on reload
    // (`transferSize` 1,537,456). If a page ever turns up whose first script beats this answer,
    // the payload will have to be *pushed* — computed in `on_before_browse`, stashed in the
    // renderer, injected synchronously at `on_context_created` — rather than asked for; it was
    // not built now because no page that loses the race could be produced to fail against it.
    if !script.is_empty() {
        crate::greasemonkey::evaluate(frame, &script, Some("bru://scriptlet.js"));
    }

    // **`any`, and never an end of the root.** "first" is the scrollbar's and "last" is the user's
    // own CSS; a second keeper holding an end livelocks against the one already there — see
    // `chrome/userstyle.js` for the two pages it was measured on. Hiding needs no position, because
    // `display: none !important` outranks whatever sits after it.
    let code = if css.is_empty() {
        "window.bruKeep && window.bruKeep(\"bru-cosmetic\",\"any\").off();".to_string()
    } else {
        format!(
            "window.bruKeep && window.bruKeep(\"bru-cosmetic\",\"any\").set(\"{}\");",
            crate::ipc::json_escape(&css)
        )
    };
    crate::greasemonkey::evaluate(frame, &code, None);
    true
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

    // **The user is told, in the bar.** `:adblock-update` is asynchronous — it returns the instant
    // it is typed and the fetch lands seconds later — so without this the command looked like it
    // did nothing at all, and the only sign it had worked was a line on a stderr the user may not
    // be watching. The compile that follows is reported by `compile` on stderr, which is right:
    // that is a diagnostic with milliseconds in it, not an answer to a keystroke.
    if done == 0 {
        crate::message::error("adblock: no lists were downloaded — nothing changed");
        eprintln!("bru[adblock]: no lists were downloaded — nothing changed");
        return;
    }
    crate::message::info(&format!(
        "adblock: {done} list{} downloaded, compiling",
        if done == 1 { "" } else { "s" }
    ));
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

    // --- the cosmetic layer ---------------------------------------------------------------------

    /// An engine over one list, built the way [`compile`] builds the real one — same
    /// `ParseOptions`, so a test cannot pass under rules the browser does not run on.
    fn engine_over(list: &str, rule_types: RuleTypes) -> Engine {
        let mut set = FilterSet::new(false);
        set.add_filter_list(
            list.to_string(),
            ParseOptions { rule_types, ..ParseOptions::default() },
        );
        Engine::new_with_filter_set(set)
    }

    /// **The line that turns the layer on, as a test.** `example.com##.promo` is a hostname cosmetic
    /// rule — the shape EasyList writes thousands of. Parsed as `NetworkOnly`, which is what
    /// `compile` did until 2026-08-09, it is thrown away and the page hides nothing; parsed as
    /// `All` it comes back. Put `RuleTypes::NetworkOnly` back in `compile` and the second half of
    /// this fails.
    #[test]
    fn a_hostname_hiding_rule_only_survives_when_the_cosmetic_half_is_parsed() {
        const LIST: &str = "example.com##.promo";

        let dropped = engine_over(LIST, RuleTypes::NetworkOnly);
        assert!(
            dropped.url_cosmetic_resources("https://example.com/").hide_selectors.is_empty(),
            "NetworkOnly must throw the cosmetic half away — that was the old behaviour",
        );

        let kept = engine_over(LIST, RuleTypes::All);
        let selectors = kept.url_cosmetic_resources("https://example.com/").hide_selectors;
        assert!(selectors.contains(".promo"), "the rule for this host must come back: {selectors:?}");

        // And it is *this host's* rule: another site gets nothing from it.
        assert!(
            kept.url_cosmetic_resources("https://other.example.org/").hide_selectors.is_empty(),
            "a hostname rule must not leak onto a hostname it was not written for",
        );
    }

    /// The sheet's shape, which is what the page actually receives.
    #[test]
    fn the_stylesheet_is_one_rule_over_sorted_selectors() {
        assert_eq!(
            stylesheet([".b", ".a"].into_iter()),
            ".a,.b{display:none!important}",
            "sorted, comma-joined, and hidden with a declaration a site's own CSS cannot outrank",
        );
        // Nothing to hide is no stylesheet at all, not an empty rule — `renderer_on_cosmetic_css`
        // reads the empty string as "take the sheet out again".
        assert_eq!(stylesheet(std::iter::empty()), "");
        assert_eq!(stylesheet([""].into_iter()), "");
    }

    /// One list line must not be able to rewrite the rest of the sheet. The parser should never
    /// hand a brace over; this is what makes "should never" not matter.
    #[test]
    fn a_selector_that_could_end_the_rule_early_is_dropped() {
        assert_eq!(
            stylesheet([".ok", ".evil}html{display:none"].into_iter()),
            ".ok{display:none!important}",
        );
        assert_eq!(stylesheet([".a\nb"].into_iter()), "");
    }

    /// **Two keepers may not hold the same end of the root.** This is the livelock of 2026-08-09:
    /// the cosmetic sheet was added as `"last"`, which `bru-userstyle` already holds, and each
    /// observer appended its own element back over the other's for ever. youtube.com and
    /// start.duckduckgo.com — the pages with a user stylesheet, so there was a second element to
    /// fight — stopped painting entirely and their renderers stopped answering.
    ///
    /// Asserted over the source of both halves, because the bug is not in either one alone: it is
    /// in two callers choosing the same position.
    #[test]
    fn the_cosmetic_sheet_does_not_hold_an_end_of_the_root() {
        let this = include_str!("adblock.rs");
        assert!(
            this.contains("\\\"bru-cosmetic\\\",\\\"any\\\""),
            "the cosmetic keeper must ask for `any`; `first` is the scrollbar's and `last` is the \
             user's own CSS, and a second holder of either livelocks the renderer",
        );
        // And the keeper has to have that third position to give.
        let keeper = crate::userstyles::KEEPER_JS;
        assert!(
            keeper.contains("where === \"last\"") && keeper.contains("styleElem.parentNode !== rootElem"),
            "userstyle.js must branch on `last` explicitly and treat anything else as presence-only",
        );
    }

    // --- the scriptlets -------------------------------------------------------------------------

    /// **The alias must carry the `.js`.** A rule writes `+js(set, …)` and the engine appends
    /// `.js` before looking the name up, so an alias spelled `set` matches nothing, silently —
    /// four rules resolved to zero scriptlets on 2026-08-09 and the only symptom was the
    /// advertisement they were written to remove. Change the alias in [`resources`] back to
    /// `set` and this fails.
    #[test]
    fn a_set_rule_resolves_through_the_alias_the_engine_canonicalises() {
        let mut engine = engine_over(
            "www.youtube.com##+js(set, ytInitialPlayerResponse.adPlacements, undefined)",
            RuleTypes::All,
        );
        engine.use_resources(resources());
        let script =
            engine.url_cosmetic_resources("https://www.youtube.com/watch?v=x").injected_script;
        assert!(
            script.contains("function bruSetConstant("),
            "the library function must travel with the invocation: {script:?}",
        );
        assert!(
            script.contains("bruSetConstant(\"ytInitialPlayerResponse.adPlacements\", \"undefined\")"),
            "the rule's arguments must arrive as a function call, quoted by the engine: {script:?}",
        );
    }

    /// **Trust comes from how the list was parsed, and from nowhere else.** The same rule text
    /// resolves to the trusted scriptlet when parsed with [`TRUSTED`] — `filters.txt`'s path —
    /// and to nothing when parsed the way every downloaded list is.
    #[test]
    fn a_trusted_scriptlet_answers_only_a_trusted_list() {
        const RULE: &str = "www.youtube.com##+js(trusted-replace-fetch-response, \
                            /\"(adPlacements|adSlots|playerAds)\"/, '\"no_ads\"', get_watch)";

        let mut untrusted = engine_over(RULE, RuleTypes::All);
        untrusted.use_resources(resources());
        let script =
            untrusted.url_cosmetic_resources("https://www.youtube.com/watch?v=x").injected_script;
        assert!(
            !script.contains("bruTrustedReplaceFetchResponse"),
            "a downloaded list must not be able to rewrite responses: {script:?}",
        );

        let mut set = FilterSet::new(false);
        set.add_filter_list(
            RULE.to_string(),
            ParseOptions { rule_types: RuleTypes::All, permissions: TRUSTED, ..ParseOptions::default() },
        );
        let mut trusted = Engine::new_with_filter_set(set);
        trusted.use_resources(resources());
        let script =
            trusted.url_cosmetic_resources("https://www.youtube.com/watch?v=x").injected_script;
        assert!(
            script.contains("bruTrustedReplaceFetchResponse("),
            "the user's own file must resolve the same rule: {script:?}",
        );
    }

    /// **Each scriptlet file must *begin* with `function <name>(`.** `extract_function_name` is
    /// anchored at the start of the file; a comment above the function demotes the resource to a
    /// text template, whose substitution cannot carry arguments with quotes in them — that is the
    /// syntax error that once killed the whole injected script, silently. The prose lives inside
    /// the bodies for exactly this reason.
    #[test]
    fn the_scriptlet_files_begin_with_the_function_the_engine_extracts() {
        for (text, name) in [
            (include_str!("../chrome/scriptlets/set-constant.js"), "bruSetConstant"),
            (
                include_str!("../chrome/scriptlets/trusted-replace-fetch-response.js"),
                "bruTrustedReplaceFetchResponse",
            ),
        ] {
            assert!(
                text.starts_with(&format!("function {name}(")),
                "{name}: the engine reads the callable name off the first characters of the file",
            );
        }
    }

    /// Only real web pages. bru's own UI is not something a filter list has an opinion about, and a
    /// page with no hostname has no rule written against it.
    #[test]
    fn only_web_pages_are_hidden_on() {
        assert!(hideable("https://example.com/"));
        assert!(hideable("http://example.com/"));
        assert!(!hideable("bru://chrome/settings"));
        assert!(!hideable("about:blank"));
        assert!(!hideable("file:///run/user/1000/test.html"));
    }
}


