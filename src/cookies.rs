//! `bru://chrome/cookies` — every cookie Chromium is holding, searchable by domain and deletable.
//!
//! Asked for by name: *"a page like /help — say /cookies — where I can search by domain, look at
//! every cookie and remove them, all at once or particular ones."* This is that page.
//!
//! ## What it is not
//!
//! **It cannot block a cookie, and no amount of work here would change that.** Measured
//! 2026-08-06: `on_before_resource_load` really does see the outgoing `Cookie` header and really
//! does let you remove it — the log read `before="bruck=yes" after=""` — and the server received
//! the cookie anyway, because Chromium's network service re-attaches it downstream of every client
//! hook. So this page lists and deletes, and says nothing about blocking. `bru://chrome/help`
//! already marks the twelve `content.cookies.accept` bindings **refused** for the same reason.
//!
//! ## The three things that shaped the code
//!
//! **1. The visitor is asynchronous, and the page therefore cannot be generated the way
//! `bru://chrome/help` is.** `help.rs` builds its whole document inside the scheme handler, from
//! tables that are already in memory. Cookies are not: `CookieManager::visit_all_cookies` returns
//! immediately and calls the visitor later, and a scheme handler's `create` runs on the **IO**
//! thread and must hand back a `ResourceHandler` there and then (`cef_scheme_capi.h:86-88`, and
//! CEF-NOTES trap 15 is what happens when you take an answer on the wrong thread). So what
//! `chrome.rs` serves is a **shell** — markup, style and script, and not one cookie — and the shell
//! asks for the rows with `window.cefQuery`. That is also what makes deleting possible at all: a
//! page that can ask can also tell.
//!
//! **2. There is no "done" callback, and with an empty cookie jar there is no callback at all.**
//! `cef_cookie_capi.h:158`: *"This function may never be called if no cookies are found."* There is
//! no completion structure beside `cef_cookie_visitor_t`, so a `count == total - 1` test answers a
//! query with 400 cookies in it and hangs forever on a fresh profile. What is left is the visitor's
//! **lifetime**: CEF holds a reference for exactly as long as it is visiting, and cef-rs's
//! `RcImpl::release` (`cef/src/rc.rs:370`) `Box::from_raw`s the wrapper when the last one goes,
//! which drops the Rust struct inside it. So the completion signal is `Drop`, on [`Job`], and it
//! fires on the empty jar too. See [`Job::drop`].
//!
//! Measured 2026-08-07 with the plausible wrong signal in place of the `Drop`: a jar with one
//! cookie in it rendered, and deleting that cookie left the page frozen on the pre-delete rows
//! forever — the re-list found nothing, `visit` was never called, and nothing ever answered. The
//! debug listing printed no line at all for the same reason. Restored, the same run reads
//! `visit finished: 0 rows, 0 deleted, visit() ran on []`.
//!
//! **Which thread the visitor runs on is measured, not assumed.** `cef_cookie_capi.h:143` says the
//! visitor's functions "will always be called on the UI thread", and that is one of the claims
//! CEF-NOTES trap 15 exists because of — `get_content_setting` does not fail on the wrong thread,
//! it answers `default`. So `visit` asks `currently_on` and `BRU_DEBUG_COOKIES=1` prints the
//! answer: `visit() ran on [UI]`, every time, whether the walk was started from a query handler or
//! from a posted task. It is **not** the IO thread, which is where a scheme handler runs and where
//! anyone reading `chrome.rs` would expect this to be too.
//!
//! **3. `delete_cookies(url, name)` is the wrong tool for a row on a page.** It is documented in
//! terms of *hosts* and *domains* — "if only |url| is specified all host cookies (but not domain
//! cookies) irrespective of path will be deleted" — so deleting the one row a person is pointing at
//! means reconstructing a URL that matches that cookie and only that cookie, which for a domain
//! cookie with a path is a guess. The visitor's own `deleteCookie` out-parameter is exact: it
//! deletes the cookie currently being visited, which is the row, keyed by the triple Chromium keys
//! cookies by — domain, path, name. One visit does the whole batch and hands back the full records
//! on the way past, which is what makes undo possible.
//!
//! ## Deleting everything is reversible, and that is deliberate
//!
//! bru has no prompt mode and DESIGN.md gives it no dialogs, so a confirmation cannot be a modal.
//! Two things stand in for one:
//!
//! - the bulk button **arms first** — the first activation turns it into "press again to delete
//!   N", the second does it, and it disarms itself after five seconds;
//! - every delete, bulk or single, **stashes the full cookie records** and the page grows an
//!   `Undo (N)` control that puts them back with `CookieManager::set_cookie`.
//!
//! Undo is the honest half. A confirmation only asks whether you meant it; being able to put the
//! cookies back is the thing that makes "delete every cookie in the browser" a decision a person
//! can afford to get wrong. What it does not survive is the process — the stash is memory, and
//! `Undo` says so on the page.
//!
//! ## The address is deliberately bare
//!
//! `:cookies mybank.example` does **not** navigate to `bru://chrome/cookies?domain=mybank.example`.
//! Measured 2026-08-07: load that URL by hand and Chromium's own `Default/History` holds
//! `bru://chrome/cookies?domain=mybank.example` on disk afterwards — beside `bru://chrome/top.html`
//! and `bru://chrome/bottom.html`, which it records too. bru's own history never holds any of them
//! (`data::is_excluded` lists `bru://`), so the leak would have been Chromium's alone, and it would
//! still have been a record of which bank's cookies someone went looking at. The filter travels
//! beside the navigation instead, keyed by the window — see [`show`]. What reaches either history
//! is `bru://chrome/cookies`, which says only that the page was opened.

use cef::wrapper::message_router::BrowserSideCallback;
use cef::*;
use std::sync::{Arc, Mutex, OnceLock};

use crate::tabs::SharedState;

/// The address the page lives at. `src/chrome.rs` maps the path; this is what `:cookies` navigates
/// to, spelled once so the two cannot drift.
pub const COOKIES_URL: &str = "bru://chrome/cookies";

/// How the page and Rust spell one cookie's identity in a request.
///
/// Chromium keys a cookie by (domain, path, name) — that triple is what `deleteCookie` acts on and
/// what makes two rows different rows. It travels as one JSON string with `` between the
/// parts rather than as three fields, because the request carries a *list* of them and `ipc.rs`'s
/// hand-written reader knows only flat string fields. `` is safe: RFC 6265 forbids control
/// characters in a cookie name, and a domain and a path cannot hold one either.
const SEP: char = '\u{1}';

// -----------------------------------------------------------------------------------------------
// The data, in plain Rust
// -----------------------------------------------------------------------------------------------

/// One cookie, with nothing of CEF left in it.
///
/// `cef::Cookie` holds `CefString`s, which are not something to put in a process-wide `Mutex` — and
/// the undo stash is exactly that. Everything here is `String`, `bool` and `i64`, so the stash is
/// plain memory and every function below this line can be tested without a CEF anywhere.
///
/// The three `_us` fields are raw `cef_basetime_t` values: **microseconds since 1601-01-01 UTC**,
/// which is Chromium's `base::Time` internal representation. They are kept raw rather than
/// converted because `set_cookie` wants them back exactly as they were — an undo that rounded the
/// creation time would restore a cookie that is not the one that was deleted.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CookieRow {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    pub secure: bool,
    pub httponly: bool,
    pub has_expires: bool,
    pub expires_us: i64,
    pub creation_us: i64,
    pub last_access_us: i64,
    pub same_site: i32,
    pub priority: i32,
}

/// Seconds between 1601-01-01 and 1970-01-01, the offset between `base::Time` and Unix time.
///
/// Written down rather than looked up because there is nothing in `cef_time.h` that converts a
/// `cef_basetime_t` to a Unix time in one call. It is not asserted: `--cookies-script=epoch` asks
/// CEF for `basetime_now()` and the system clock for the same instant and prints the difference, so
/// the constant is checked against the running Chromium rather than against memory.
pub const BASETIME_EPOCH_OFFSET_SECS: i64 = 11_644_473_600;

impl CookieRow {
    /// The identity triple, in the spelling the page and Rust share.
    pub fn key(&self) -> String {
        format!("{}{SEP}{}{SEP}{}", self.domain, self.path, self.name)
    }

    /// The expiry as Unix seconds, or `None` for a session cookie.
    ///
    /// The page does the formatting — `new Date(secs * 1000)` knows the user's timezone and the
    /// calendar, and neither is worth reimplementing in Rust for one column.
    pub fn expires_unix(&self) -> Option<i64> {
        if !self.has_expires || self.expires_us == 0 {
            return None;
        }
        Some(self.expires_us / 1_000_000 - BASETIME_EPOCH_OFFSET_SECS)
    }

    /// The URL a cookie has to be handed back through when it is restored.
    ///
    /// `set_cookie` takes a URL and checks the cookie's domain against it, so undo has to rebuild
    /// one. A leading dot means a domain cookie and is dropped for the host; `secure` decides the
    /// scheme, because a `Secure` cookie set through `http://` is refused.
    pub fn restore_url(&self) -> String {
        let host = self.domain.strip_prefix('.').unwrap_or(&self.domain);
        let scheme = if self.secure { "https" } else { "http" };
        let path = if self.path.starts_with('/') { self.path.as_str() } else { "/" };
        format!("{scheme}://{host}{path}")
    }
}

/// Whether a row is shown for a filter.
///
/// **Domain only, and a substring, against the domain exactly as Chromium spells it.** The user
/// asked to search *by domain*; matching the name as well would mean typing `session` brought up a
/// hundred unrelated sites, which is the opposite of narrowing. The comparison is case-insensitive
/// because a domain is.
///
/// The leading dot of a domain cookie is deliberately **not** stripped, and the first version of
/// this did strip it — which broke the one filter a person is most likely to type, because the page
/// shows `.github.com` and `"github.com".contains(".github.com")` is false. Nothing is lost by
/// leaving it on: every substring of the stripped form is a substring of the dotted one, so `github`
/// still matches and `.github.com` now does too.
///
/// `chrome/cookies.js` implements the same rule for the live filtering — the page must not make a
/// round trip per keystroke — so this exists in two languages on purpose. What keeps them together
/// is not discipline: `--cookies-script=filter:<text>` asks Rust for its count and the page prints
/// its own over the same jar.
pub fn matches(row: &CookieRow, filter: &str) -> bool {
    let filter = filter.trim().to_lowercase();
    if filter.is_empty() {
        return true;
    }
    row.domain.to_lowercase().contains(&filter)
}

// -----------------------------------------------------------------------------------------------
// The page, and what the command does
// -----------------------------------------------------------------------------------------------

/// `cookies [-b] [domain]` — open the page, optionally with the filter box already filled in.
///
/// A new tab, like `:history` and `:bookmark-list`, and for the same reason: qutebrowser's own
/// `tab` argument defaults to True on both of those (`commands.py:1347`, `:1450`) and a page of
/// cookies is a place you go rather than something that replaces what you were reading.
pub fn show(state: &SharedState, browser: &mut Browser, filter: Option<&str>, bg: bool) {
    // **The filter never goes into the URL.** `bru://chrome/cookies?domain=mybank.com` would be
    // recorded by `history::visited` and by Chromium's own history, and the whole point of looking
    // at a domain's cookies is often that the domain is nobody else's business. It is handed over
    // out of band instead, keyed by the window the command was typed in, and taken by the first
    // `list` query that window's page sends.
    let window = state
        .lock()
        .expect("state mutex poisoned")
        .current_window_id();
    if let Some(window) = window {
        set_pending_filter(window, filter.unwrap_or("").to_string());
    }
    crate::open::open(state, browser, Some(COOKIES_URL), true, bg);
}

/// The filter `:cookies <domain>` asked for, waiting for that window's page to load.
fn pending() -> &'static Mutex<Vec<(u32, String)>> {
    static PENDING: Mutex<Vec<(u32, String)>> = Mutex::new(Vec::new());
    &PENDING
}

fn set_pending_filter(window: u32, filter: String) {
    if let Ok(mut pending) = pending().lock() {
        pending.retain(|(w, _)| *w != window);
        pending.push((window, filter));
    }
}

/// Take the filter for a window, if one is waiting. One-shot: reloading the page keeps whatever the
/// user has typed since, rather than jumping back to what the command asked for.
fn take_pending_filter(window: Option<u32>) -> String {
    let Some(window) = window else {
        return String::new();
    };
    let Ok(mut pending) = pending().lock() else {
        return String::new();
    };
    match pending.iter().position(|(w, _)| *w == window) {
        Some(at) => pending.remove(at).1,
        None => String::new(),
    }
}

/// The shell, as HTML. Not one cookie is in it — see the module comment.
///
/// Styled from `chrome/chrome.css` like `bru://chrome/help`, rather than from a `<style>` block of
/// its own like `bru://chrome/history`. That is the stricter of the two arrangements and it is
/// chosen on purpose: `chrome.rs::chrome_css_carries_not_one_colour` reads that file and fails the
/// build if a colour is written in it, so every rule this page draws with is covered by a test that
/// already exists. A `<style>` block here would be outside it.
pub fn page() -> String {
    String::from(
        r#"<!doctype html>
<meta charset="utf-8">
<title>bru — cookies</title>
<link rel="stylesheet" href="chrome.css">
<link rel="stylesheet" href="theme.css">
<body data-view="cookies">
<main id="cookies">
<h1>Cookies</h1>
<p class="summary" id="summary">Reading the cookie jar&hellip;</p>
<div id="controls">
  <input id="filter" type="text" autofocus autocomplete="off" spellcheck="false"
         placeholder="domain">
  <button id="wipe" type="button" disabled>Delete the 0 shown</button>
  <button id="undo" type="button" hidden>Undo</button>
</div>
<p class="summary" id="keys">Type to narrow by domain &middot;
  <kbd>&darr;</kbd><kbd>&uarr;</kbd> pick a row &middot;
  <kbd>Enter</kbd> delete it &middot;
  <kbd>Tab</kbd> the buttons &middot;
  <kbd>Esc</kbd> back to normal mode, where <kbd>j</kbd>/<kbd>k</kbd> scroll and <kbd>f</kbd>
  hints every <span class="x">&times;</span></p>
<div id="rows"></div>
</main>
<script src="cookies.js"></script>
"#,
    )
}

// -----------------------------------------------------------------------------------------------
// The answer the page is given
// -----------------------------------------------------------------------------------------------

/// One `{...}` per cookie, in the order the visitor produced them.
///
/// Hand-written like every other JSON in bru — the producer and the consumer are both in this
/// repository and a JSON crate for eleven keys would be a dependency to audit.
pub fn rows_json(rows: &[CookieRow]) -> String {
    let mut out = String::with_capacity(rows.len() * 160 + 2);
    out.push('[');
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            "{{\"k\":\"{}\",\"n\":\"{}\",\"v\":\"{}\",\"d\":\"{}\",\"p\":\"{}\",\"s\":{},\"h\":{},\"e\":{}}}",
            crate::ipc::json_escape(&row.key()),
            crate::ipc::json_escape(&row.name),
            crate::ipc::json_escape(&row.value),
            crate::ipc::json_escape(&row.domain),
            crate::ipc::json_escape(&row.path),
            row.secure as u8,
            row.httponly as u8,
            row.expires_unix().unwrap_or(0),
        ));
    }
    out.push(']');
    out
}

/// The whole answer to a `list`: the rows, the filter the command asked for, and how much undo is
/// left. One object, so the page renders once rather than three times.
fn list_json(rows: &[CookieRow], filter: &str, undo: usize) -> String {
    format!(
        "{{\"filter\":\"{}\",\"undo\":{undo},\"rows\":{}}}",
        crate::ipc::json_escape(filter),
        rows_json(rows),
    )
}

// -----------------------------------------------------------------------------------------------
// Reading the request
// -----------------------------------------------------------------------------------------------

/// Read `"keys": ["a", "b", …]` out of a flat request object.
///
/// `ipc.rs::json_field` reads a string and stops; this is the one array bru's chrome sends. It is a
/// reader for exactly the shape `JSON.stringify` produces from an array of strings and nothing
/// else: anything it does not recognise yields an empty list, which deletes nothing. Failing that
/// way round is the whole reason it is written by hand rather than made permissive.
pub fn json_string_array(src: &str, key: &str) -> Vec<String> {
    let needle = format!("\"{key}\"");
    let Some(at) = src.find(&needle) else {
        return Vec::new();
    };
    let rest = src[at + needle.len()..].trim_start();
    let Some(rest) = rest.strip_prefix(':') else {
        return Vec::new();
    };
    let rest = rest.trim_start();
    let Some(rest) = rest.strip_prefix('[') else {
        return Vec::new();
    };

    let mut out = Vec::new();
    let mut chars = rest.chars().peekable();
    loop {
        // Between elements: whitespace, a comma, or the end of the array.
        while matches!(chars.peek(), Some(c) if c.is_whitespace() || *c == ',') {
            chars.next();
        }
        match chars.peek() {
            Some('"') => {
                chars.next();
            }
            // `]` ends it; anything else is not the shape this reads, and an empty list is the
            // safe answer.
            _ => return out,
        }
        let mut item = String::new();
        loop {
            match chars.next() {
                None => return out,
                Some('"') => break,
                Some('\\') => match chars.next() {
                    Some('n') => item.push('\n'),
                    Some('r') => item.push('\r'),
                    Some('t') => item.push('\t'),
                    Some('b') => item.push('\u{8}'),
                    Some('f') => item.push('\u{c}'),
                    Some('u') => {
                        let hex: String = (0..4).filter_map(|_| chars.next()).collect();
                        if let Some(c) =
                            u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32)
                        {
                            item.push(c);
                        }
                    }
                    Some(other) => item.push(other),
                    None => return out,
                },
                Some(other) => item.push(other),
            }
        }
        out.push(item);
    }
}

// -----------------------------------------------------------------------------------------------
// The undo stash
// -----------------------------------------------------------------------------------------------

/// The last deletion, kept so it can be put back.
///
/// One deletion deep, not a stack. A stack would make "Undo" ambiguous on a page whose only
/// affordance is one button, and the thing this protects against — deleting four hundred cookies
/// with the filter box empty — is a single act.
fn stash() -> &'static Mutex<Vec<CookieRow>> {
    static STASH: Mutex<Vec<CookieRow>> = Mutex::new(Vec::new());
    &STASH
}

fn stash_len() -> usize {
    stash().lock().map(|stash| stash.len()).unwrap_or(0)
}

fn stash_put(rows: Vec<CookieRow>) {
    if let Ok(mut stash) = stash().lock() {
        *stash = rows;
    }
}

fn stash_take() -> Vec<CookieRow> {
    match stash().lock() {
        Ok(mut stash) => std::mem::take(&mut *stash),
        Err(_) => Vec::new(),
    }
}

// -----------------------------------------------------------------------------------------------
// The query handler
// -----------------------------------------------------------------------------------------------

/// What one visit is for.
enum Plan {
    /// Collect everything.
    ///
    /// `narrow` is what separates the page from the harness. **The page is always given every
    /// row**, whatever `:cookies github.com` asked for, because the filter box has to be able to be
    /// cleared — a list that had already been narrowed in Rust would show nothing when it was. The
    /// filter travels as text for the box and is applied in JavaScript. `--cookies-script=filter:x`
    /// sets `narrow`, so [`matches`] is asked the same question the page's own filter is asked and
    /// the two answers can be compared over a real jar.
    List { narrow: bool },
    /// Collect and delete the rows whose key is in the set.
    Delete(Vec<String>),
}

/// One `visit_all_cookies` in flight, and the thing that answers the page when it is over.
///
/// Held behind an `Arc` by the visitor object CEF owns, so the visitor's release is this struct's
/// `Drop` — see the module comment for why that is the only completion signal there is.
struct Job {
    plan: Plan,
    filter: String,
    rows: Mutex<Vec<CookieRow>>,
    deleted: Mutex<Vec<CookieRow>>,
    /// Which CEF thread `visit` ran on, recorded rather than assumed. `cef_cookie_capi.h:143` says
    /// UI; CEF-NOTES trap 15 is what a wrong answer about a thread costs, so this is measured and
    /// printed under `BRU_DEBUG_COOKIES=1` instead of being taken on trust.
    threads: Mutex<Vec<&'static str>>,
    callback: Arc<Mutex<dyn BrowserSideCallback>>,
}

impl Drop for Job {
    /// The completion signal. CEF drops its reference to the visitor when it has finished visiting
    /// — including when it never called `visit` at all, which is what an empty cookie jar looks
    /// like — and cef-rs's `release` drops the Rust struct with it.
    fn drop(&mut self) {
        let rows = std::mem::take(&mut *self.rows.lock().expect("cookie rows poisoned"));
        let deleted = std::mem::take(&mut *self.deleted.lock().expect("deleted cookies poisoned"));
        let threads = self.threads.lock().expect("cookie threads poisoned").clone();
        log(&format!(
            "visit finished: {} rows, {} deleted, visit() ran on [{}]",
            rows.len(),
            deleted.len(),
            threads.join(", "),
        ));

        let answer = match &self.plan {
            Plan::List { narrow } => {
                let shown: Vec<CookieRow> = if *narrow {
                    rows.into_iter().filter(|row| matches(row, &self.filter)).collect()
                } else {
                    rows
                };
                list_json(&shown, &self.filter, stash_len())
            }
            Plan::Delete(_) => {
                let count = deleted.len();
                if count > 0 {
                    stash_put(deleted);
                }
                // The bar says it too, because the page may be scrolled away from the controls and
                // a delete that says nothing anywhere is a delete you cannot be sure of.
                crate::message::info(&format!(
                    "Deleted {count} cookie{} — Undo on the page puts them back",
                    if count == 1 { "" } else { "s" },
                ));
                format!("{{\"deleted\":{count},\"undo\":{}}}", stash_len())
            }
        };
        if let Ok(callback) = self.callback.lock() {
            callback.success_str(&answer);
        }
    }
}

wrap_cookie_visitor! {
    struct BruCookieVisitor {
        job: Arc<Job>,
    }

    impl CookieVisitor {
        fn visit(
            &self,
            cookie: Option<&Cookie>,
            _count: ::std::os::raw::c_int,
            _total: ::std::os::raw::c_int,
            delete_cookie: Option<&mut ::std::os::raw::c_int>,
        ) -> ::std::os::raw::c_int {
            // Where this actually runs, asked of CEF rather than of the header. One entry per
            // visit is one string per cookie and this is not on any path a key takes.
            let thread = if currently_on(ThreadId::UI) != 0 {
                "UI"
            } else if currently_on(ThreadId::IO) != 0 {
                "IO"
            } else {
                "other"
            };
            if let Ok(mut threads) = self.job.threads.lock() {
                if !threads.contains(&thread) {
                    threads.push(thread);
                }
            }

            let Some(cookie) = cookie else {
                return 1;
            };
            let row = row_of(cookie);

            let doomed = match &self.job.plan {
                Plan::List { .. } => false,
                Plan::Delete(keys) => keys.iter().any(|key| *key == row.key()),
            };
            if doomed {
                if let Some(flag) = delete_cookie {
                    *flag = 1;
                }
                if let Ok(mut deleted) = self.job.deleted.lock() {
                    deleted.push(row.clone());
                }
            }
            if let Ok(mut rows) = self.job.rows.lock() {
                rows.push(row);
            }
            // Keep going. Returning 0 stops the walk, which would leave the rest of a batch delete
            // undone and the page showing rows that were never looked at.
            1
        }
    }
}

/// A `cef::Cookie` as plain Rust.
fn row_of(cookie: &Cookie) -> CookieRow {
    CookieRow {
        name: cookie.name.to_string(),
        value: cookie.value.to_string(),
        domain: cookie.domain.to_string(),
        path: cookie.path.to_string(),
        secure: cookie.secure != 0,
        httponly: cookie.httponly != 0,
        has_expires: cookie.has_expires != 0,
        expires_us: cookie.expires.val,
        creation_us: cookie.creation.val,
        last_access_us: cookie.last_access.val,
        same_site: cookie.same_site.get_raw() as i32,
        priority: cookie.priority.get_raw(),
    }
}

/// `bru://chrome/cookies` asking Rust for something. Called from `ipc.rs` for
/// `{"type":"cookies", …}` and for nothing else.
///
/// Returns `true` when it has taken responsibility for the callback, which is the message router's
/// contract — every path below either answers now or hands the callback to a [`Job`] that will.
///
/// Nothing here creates a browser or starts a navigation, so CEF-NOTES trap 12 does not apply and
/// no task is posted: `visit_all_cookies` is already asynchronous, and adding a hop would only put
/// one more turn of the loop between the keystroke and the rows.
pub fn on_page_query(
    browser: Option<&Browser>,
    request: &str,
    callback: &Arc<Mutex<dyn BrowserSideCallback>>,
) -> bool {
    let action = json_field(request, "action").unwrap_or_default();
    log(&format!("{action:?} {request}"));

    match action.as_str() {
        "list" => {
            let window = browser.and_then(|browser| {
                let id = browser.identifier();
                crate::state::BruState::instance()
                    .and_then(|state| state.lock().ok().and_then(|s| s.window_of_browser(id)))
            });
            start(
                Plan::List { narrow: false },
                take_pending_filter(window),
                callback.clone(),
            )
        }
        "delete" => {
            let keys = json_string_array(request, "keys");
            if keys.is_empty() {
                answer(callback, "{\"deleted\":0,\"undo\":0}");
                return true;
            }
            start(Plan::Delete(keys), String::new(), callback.clone())
        }
        "restore" => {
            let rows = stash_take();
            let count = restore(&rows);
            crate::message::info(&format!(
                "Restored {count} cookie{}",
                if count == 1 { "" } else { "s" },
            ));
            answer(callback, &format!("{{\"restored\":{count},\"undo\":0}}"));
            true
        }
        other => {
            if let Ok(callback) = callback.lock() {
                callback.failure(-10, &format!("unknown cookie action {other:?}"));
            }
            true
        }
    }
}

fn answer(callback: &Arc<Mutex<dyn BrowserSideCallback>>, response: &str) {
    if let Ok(callback) = callback.lock() {
        callback.success_str(response);
    }
}

/// Start one visit. The `Job` is dropped by CEF when the walk is over, and that is what answers.
fn start(plan: Plan, filter: String, callback: Arc<Mutex<dyn BrowserSideCallback>>) -> bool {
    let Some(manager) = cookie_manager_get_global_manager(None) else {
        if let Ok(callback) = callback.lock() {
            callback.failure(-11, "no cookie manager");
        }
        return true;
    };
    let job = Arc::new(Job {
        plan,
        filter,
        rows: Mutex::new(Vec::new()),
        deleted: Mutex::new(Vec::new()),
        threads: Mutex::new(Vec::new()),
        callback,
    });
    let mut visitor = BruCookieVisitor::new(job);
    // The local `visitor` holds one reference and `visit_all_cookies` takes another. Dropping the
    // local at the end of this function leaves CEF's, and the `Job` lives exactly as long as the
    // walk does — including the zero-cookie case, where CEF releases its reference without ever
    // calling `visit` and `Job::drop` answers with an empty list rather than hanging.
    if manager.visit_all_cookies(Some(&mut visitor)) == 0 {
        log("visit_all_cookies refused; the Job's Drop will answer with nothing");
    }
    true
}

/// Put a batch of cookies back, one `set_cookie` each. Returns how many CEF accepted.
///
/// `set_cookie` answers 0 only for an invalid URL or an inaccessible store; whether the cookie was
/// really written arrives later on the `SetCookieCallback`, which is not asked for here. The count
/// is therefore "how many were handed over", and the page reloads its list straight afterwards, so
/// what is really there is what the user sees a moment later either way.
fn restore(rows: &[CookieRow]) -> usize {
    let Some(manager) = cookie_manager_get_global_manager(None) else {
        return 0;
    };
    let mut count = 0;
    for row in rows {
        let cookie = Cookie {
            name: CefString::from(row.name.as_str()),
            value: CefString::from(row.value.as_str()),
            domain: CefString::from(row.domain.as_str()),
            path: CefString::from(row.path.as_str()),
            secure: row.secure as ::std::os::raw::c_int,
            httponly: row.httponly as ::std::os::raw::c_int,
            creation: Basetime { val: row.creation_us },
            last_access: Basetime { val: row.last_access_us },
            has_expires: row.has_expires as ::std::os::raw::c_int,
            expires: Basetime { val: row.expires_us },
            same_site: same_site_of(row.same_site),
            priority: priority_of(row.priority),
            ..Default::default()
        };
        let url = CefString::from(row.restore_url().as_str());
        if manager.set_cookie(Some(&url), Some(&cookie), None) != 0 {
            count += 1;
        } else {
            log(&format!("set_cookie refused {} for {}", row.name, row.restore_url()));
        }
    }
    count
}

/// The enums have `get_raw` and no way back, so the four and the three are spelled out. An
/// unrecognised value restores as the default, which is what Chromium treats a missing attribute
/// as.
fn same_site_of(raw: i32) -> CookieSameSite {
    for value in [
        CookieSameSite::NO_RESTRICTION,
        CookieSameSite::LAX_MODE,
        CookieSameSite::STRICT_MODE,
    ] {
        if value.get_raw() as i32 == raw {
            return value;
        }
    }
    CookieSameSite::UNSPECIFIED
}

fn priority_of(raw: i32) -> CookiePriority {
    for value in [CookiePriority::LOW, CookiePriority::HIGH] {
        if value.get_raw() == raw {
            return value;
        }
    }
    CookiePriority::MEDIUM
}

/// `BRU_DEBUG_COOKIES=1`. Off by default, in the shape the rest of bru's debug switches take.
fn log(message: &str) {
    static ON: OnceLock<bool> = OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("BRU_DEBUG_COOKIES").is_some()) {
        eprintln!("bru[cookies]: {message}");
    }
}

/// `ipc.rs::json_field` is private to that module and this needs the same three lines. Reading one
/// string out of a flat object the chrome's own `JSON.stringify` produced.
fn json_field(src: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let at = src.find(&needle)?;
    let after = src[at + needle.len()..].trim_start();
    let after = after.strip_prefix(':')?.trim_start();
    let after = after.strip_prefix('"')?;
    Some(after.chars().take_while(|c| *c != '"').collect())
}

// -----------------------------------------------------------------------------------------------
// The debug harness
// -----------------------------------------------------------------------------------------------

/// `--cookies-script=set:http://127.0.0.1:8931/:a=1,list,epoch --cookies-step-ms=800`.
///
/// Every step runs from a posted UI task and prints one line. It exists because the only key
/// injector on this machine segfaults CEF (CEF-NOTES) and because a cookie jar cannot otherwise be
/// filled, read and emptied from a script that runs twice.
pub fn schedule_script(steps: &str, interval_ms: i64) {
    for (i, step) in steps.split(',').filter(|s| !s.is_empty()).enumerate() {
        let mut task = CookieStep::new(step.to_string());
        post_delayed_task(ThreadId::UI, Some(&mut task), interval_ms * (i as i64 + 1));
    }
}

wrap_task! {
    struct CookieStep {
        step: String,
    }

    impl Task {
        fn execute(&self) {
            let (verb, arg) = match self.step.split_once(':') {
                Some((verb, arg)) => (verb, arg),
                None => (self.step.as_str(), ""),
            };
            match verb {
                // `set:<url>|<name>=<value>` — put a cookie in the jar without a server.
                "set" => {
                    let Some((url, pair)) = arg.split_once('|') else {
                        eprintln!("cookies-script: set needs <url>|<name>=<value>");
                        return;
                    };
                    let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
                    let host = url
                        .split("://")
                        .nth(1)
                        .and_then(|rest| rest.split('/').next())
                        .unwrap_or("")
                        .split(':')
                        .next()
                        .unwrap_or("");
                    let row = CookieRow {
                        name: name.to_string(),
                        value: value.to_string(),
                        domain: host.to_string(),
                        path: "/".to_string(),
                        creation_us: basetime_now().val,
                        last_access_us: basetime_now().val,
                        ..Default::default()
                    };
                    eprintln!(
                        "cookies-script: set {name}={value} for {url} -> {}",
                        restore(std::slice::from_ref(&row))
                    );
                }
                // `fill:<n>` — n cookies over n/5 invented domains, in one step.
                //
                // A jar with four cookies in it never fills a screen, so it cannot say whether the
                // page scrolls, whether the domain headings group anything, or whether a filter
                // that keeps *some* rows keeps the right ones. `set:` one at a time costs one
                // posted task each and the whole point is to have a hundred of them.
                "fill" => {
                    let count: usize = arg.parse().unwrap_or(0);
                    let rows: Vec<CookieRow> = (0..count)
                        .map(|i| CookieRow {
                            name: format!("fill_{i}"),
                            value: format!("value-of-{i}"),
                            // `hostN.example` and not `hostN.fill.example`, and the difference is
                            // Chromium's, not this file's: its cookie store caps one **eTLD+1** at
                            // 180 cookies and purges 30 when that is passed. Under
                            // `*.fill.example` all 200 share one eTLD+1, and the next process
                            // found 169 of them — Chromium evicting correctly and a harness that
                            // would have blamed the page.
                            domain: format!("host{}.example", i / 5),
                            path: "/".to_string(),
                            creation_us: basetime_now().val,
                            last_access_us: basetime_now().val,
                            // Persistent, so a filled jar survives into the next process the way a
                            // real one does. A session cookie would not: measured 2026-08-07, 200
                            // of them were accepted by `set_cookie` and the next process found
                            // none, which is Chromium behaving correctly and a harness lying.
                            has_expires: true,
                            expires_us: basetime_now().val + 86_400 * 1_000_000,
                            ..Default::default()
                        })
                        .collect();
                    eprintln!("cookies-script: fill {count} -> {} accepted", restore(&rows));
                }
                // `list` is the whole jar; `filter:<text>` is what [`matches`] keeps of it, which
                // is the number `chrome/cookies.js`'s own filter has to agree with.
                "list" => report(String::new(), false),
                "filter" => report(arg.to_string(), true),
                // The one constant in this file, checked against the running Chromium.
                "epoch" => {
                    let cef = basetime_now().val;
                    let unix = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_micros() as i64)
                        .unwrap_or(0);
                    eprintln!(
                        "cookies-script: basetime_now={cef} unix_now_us={unix} \
                         difference={}s, constant={}s",
                        (cef - unix) / 1_000_000,
                        BASETIME_EPOCH_OFFSET_SECS,
                    );
                }
                // `key:Down`, `key:Return`, `key:Tab` — one named key at the showing tab.
                //
                // `cmdline.rs::inject_key` refuses these ("only character keys can be injected so
                // far"), and this page is driven by exactly the keys it refuses: Down and Up pick a
                // row, Enter deletes it, Tab reaches the buttons. So the injector for them lives
                // here rather than in another workstream's file.
                //
                // **It is not the keyboard, and the difference bites this page specifically.**
                // `send_key_event` aims every event of a press at one browser (CEF-NOTES trap 18),
                // and it leaves `focus_on_editable_field` at 0 — which is the flag `keys.rs` reads
                // to enter insert mode when a page focuses a field. A real press into the filter
                // box therefore enters insert mode by itself and an injected one does not, so a
                // script has to run `mode-enter insert` first. What is injected after that travels
                // the ordinary path.
                "key" => inject_named(arg),
                other => eprintln!("cookies-script: no step named {other:?}"),
            }
        }
    }
}

/// The named keys this page is driven by, as Windows virtual key codes —
/// `bindings::named_key_for_vkey`'s table read the other way round.
fn inject_named(name: &str) {
    // The second value is the character the key types, or 0 for the ones that type nothing.
    //
    // **Enter needs its CHAR or a focused `<button>` is not activated.** Measured 2026-08-07: two
    // injected Tabs moved the focus onto `#undo` — the probe read `focus=undo` — and an injected
    // Return with only KEYDOWN and KEYUP left `Undo (1)` on the screen. Chromium runs a button's
    // default action off the character event, not off the key-down, which is the same fact
    // CEF-NOTES records for typing into an input: "without the CHAR the key reaches
    // `on_pre_key_event` and types nothing".
    let (code, character) = match name.to_ascii_lowercase().as_str() {
        "tab" => (0x09, 9u16),
        "return" | "enter" => (0x0D, 13u16),
        "escape" | "esc" => (0x1B, 27u16),
        "up" => (0x26, 0u16),
        "down" => (0x28, 0u16),
        other => {
            eprintln!("cookies-script: {other:?} is not a named key this can inject");
            return;
        }
    };
    let browser = crate::state::BruState::instance()
        .and_then(|state| state.lock().ok().and_then(|mut state| state.active_browser()));
    let Some(host) = browser.and_then(|browser| browser.host()) else {
        eprintln!("cookies-script: no tab to inject into");
        return;
    };
    // KEYDOWN, then the CHAR when the key types one, then KEYUP — the order a real press delivers
    // them in. No explicit RAWKEYDOWN: CEF synthesises it from the KEYDOWN, and sending both
    // delivers two (CEF-NOTES).
    let mut types = vec![KeyEventType::KEYDOWN];
    if character != 0 {
        types.push(KeyEventType::CHAR);
    }
    types.push(KeyEventType::KEYUP);
    for type_ in types {
        let event = KeyEvent {
            type_,
            windows_key_code: code,
            // **Zero, and never the Windows code.** This field is the *platform's* key code —
            // an X11 keycode on this machine — and Chromium builds the DomKey from it when it is
            // set. Filling it with the Windows virtual key made an injected `<Down>` type a
            // character: measured 2026-08-07 with the `bg` layout active, `key:Down` put `т` in
            // the filter box and `key:Return` put `Э`, because VKEY_DOWN is 0x28 and X11 keycode
            // 0x28 is a letter key. `cmdline.rs::inject_key` has always written 0 here.
            native_key_code: 0,
            character,
            unmodified_character: character,
            ..Default::default()
        };
        host.send_key_event(Some(&event));
    }
    eprintln!("cookies-script: injected {name} at the page");
}

/// Walk the jar and print what is in it. Uses the same visitor the page does, through a callback
/// that prints instead of answering a query — so the thing being reported is the thing the page
/// gets, not a second implementation of it.
fn report(filter: String, narrow: bool) {
    struct Printer(String);
    impl BrowserSideCallback for Printer {
        fn success_str(&self, response: &str) {
            // The count first, because that is the number the page's own filter is compared
            // against, and the rows after it so a domain can be read off the same line.
            eprintln!(
                "cookies-script: {} -> {} row(s): {response}",
                self.0,
                response.matches("\"k\":").count(),
            );
        }
        fn success_binary(&self, _data: &[u8]) {}
        fn failure(&self, code: i32, message: &str) {
            eprintln!("cookies-script: {} failed ({code}): {message}", self.0);
        }
    }
    let label = if narrow { format!("filter:{filter}") } else { "list".to_string() };
    let callback: Arc<Mutex<dyn BrowserSideCallback>> = Arc::new(Mutex::new(Printer(label)));
    start(Plan::List { narrow }, filter, callback);
}

// -----------------------------------------------------------------------------------------------
// Tests — everything that is not a CEF call
// -----------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn row(domain: &str, name: &str) -> CookieRow {
        CookieRow {
            name: name.to_string(),
            value: "v".to_string(),
            domain: domain.to_string(),
            path: "/".to_string(),
            ..Default::default()
        }
    }

    /// The identity a row is deleted by, and the reason it is a triple. Two cookies with the same
    /// name on the same domain and different paths are two cookies, and a page that could only say
    /// "the one called `id`" would delete both.
    #[test]
    fn a_cookie_is_identified_by_domain_path_and_name() {
        let mut a = row(".github.com", "logged_in");
        let mut b = row(".github.com", "logged_in");
        b.path = "/settings".to_string();
        assert_ne!(a.key(), b.key());
        a.path = "/settings".to_string();
        assert_eq!(a.key(), b.key());
        // And the parts are separated by something a cookie cannot contain, so no domain, path or
        // name can forge another row's key.
        assert_eq!(b.key(), format!(".github.com{SEP}/settings{SEP}logged_in"));
    }

    /// The rule the filter box implements, in the language that has a test runner.
    #[test]
    fn the_filter_is_a_case_insensitive_domain_substring() {
        let r = row(".GitHub.com", "logged_in");
        assert!(matches(&r, ""));
        assert!(matches(&r, "github"));
        assert!(matches(&r, "GITHUB"));
        assert!(matches(&r, "hub.co"));
        // Both spellings of the domain, because both are things a person types: `.github.com` is
        // what the page shows and `github.com` is what they know the site as. Stripping the dot
        // before comparing broke the first of those, which is why the dot stays on.
        assert!(matches(&r, ".github.com"));
        assert!(matches(&r, "github.com"));
        assert!(!matches(&r, "gitlab"));
        // Deliberately domain-only: a cookie's *name* is not what "search by domain" means, and
        // matching it would make `session` list a hundred unrelated sites.
        assert!(!matches(&r, "logged_in"));
    }

    /// A session cookie has no expiry, and the page must be able to tell the difference — a row
    /// that said `1970-01-01` for every session cookie would read as a bug in the browser.
    #[test]
    fn a_session_cookie_has_no_expiry_and_a_persistent_one_has_a_real_date() {
        let mut r = row("example.com", "s");
        assert_eq!(r.expires_unix(), None);

        // 2026-08-07T00:00:00Z is 1786406400 in Unix seconds.
        r.has_expires = true;
        r.expires_us = (1_786_406_400 + BASETIME_EPOCH_OFFSET_SECS) * 1_000_000;
        assert_eq!(r.expires_unix(), Some(1_786_406_400));

        // `has_expires` set with a zero time is still a session cookie, which is what Chromium
        // hands over for one.
        r.expires_us = 0;
        assert_eq!(r.expires_unix(), None);
    }

    /// Undo hands a cookie back through a URL, and the URL has to be one the cookie is valid for or
    /// `set_cookie` refuses it.
    #[test]
    fn a_restored_cookie_goes_back_through_a_url_its_domain_covers() {
        let mut r = row(".github.com", "logged_in");
        r.secure = true;
        r.path = "/settings".to_string();
        assert_eq!(r.restore_url(), "https://github.com/settings");

        // Not secure, and a host cookie rather than a domain one.
        let mut r = row("127.0.0.1", "a");
        r.path = "/".to_string();
        assert_eq!(r.restore_url(), "http://127.0.0.1/");

        // A cookie with no path at all still gets a URL with one — an empty path is not a URL.
        let mut r = row("example.com", "a");
        r.path = String::new();
        assert_eq!(r.restore_url(), "http://example.com/");
    }

    #[test]
    fn the_rows_the_page_is_handed_carry_what_it_draws() {
        let mut r = row(".github.com", "logged_in");
        r.secure = true;
        r.httponly = true;
        let json = rows_json(std::slice::from_ref(&r));
        assert!(json.starts_with('['));
        assert!(json.ends_with(']'));
        assert!(json.contains("\"n\":\"logged_in\""));
        assert!(json.contains("\"d\":\".github.com\""));
        assert!(json.contains("\"s\":1"));
        assert!(json.contains("\"h\":1"));
        assert!(json.contains("\"e\":0"), "a session cookie's expiry is 0");
        assert_eq!(rows_json(&[]), "[]");
    }

    /// A cookie's value is whatever a server put in it, and it lands inside a JavaScript string
    /// literal that Rust wrote. This is the same escape `ipc.rs` uses for the status bar, and the
    /// reason it is used here rather than trusted.
    #[test]
    fn a_hostile_cookie_cannot_escape_the_json_it_travels_in() {
        let mut r = row("example.com", "x\",\"n\":\"y");
        r.value = "</script><script>alert(1)</script>".to_string();
        let json = rows_json(std::slice::from_ref(&r));
        assert!(json.contains(r#"\",\"n\":\"y"#), "the quote is escaped: {json}");
        assert_eq!(json.matches("\"n\":").count(), 1, "and cannot forge a second key");
        // The page builds every row with textContent rather than innerHTML, but the JSON itself
        // still has to survive being read.
        assert!(json.contains("alert(1)"), "the value is carried, not stripped");
    }

    /// The one array bru's chrome sends, and the shapes it must refuse.
    #[test]
    fn the_key_list_is_read_and_a_malformed_one_deletes_nothing() {
        assert_eq!(
            json_string_array(r#"{"type":"cookies","keys":["a","b"]}"#, "keys"),
            vec!["a".to_string(), "b".to_string()]
        );
        assert_eq!(json_string_array(r#"{"keys":[]}"#, "keys"), Vec::<String>::new());
        assert_eq!(json_string_array(r#"{"keys": [ "a" , "b" ]}"#, "keys"), ["a", "b"]);
        // The separator arrives escaped, because that is what JSON.stringify does with a control
        // character — and it has to come back out as one character or no key ever matches.
        assert_eq!(
            json_string_array(r#"{"keys":["x/n"]}"#, "keys"),
            [format!("x{SEP}/{SEP}n")]
        );
        // A quote inside a key, which is what a cookie named `"` produces.
        assert_eq!(json_string_array(r#"{"keys":["a\"b"]}"#, "keys"), [r#"a"b"#]);
        // And every way it can be wrong. Each of these must delete nothing rather than something.
        for bad in [
            r#"{"type":"cookies"}"#,
            r#"{"keys":"a"}"#,
            r#"{"keys":{"a":1}}"#,
            r#"{"keys":[1,2]}"#,
            r#"{"keys":["unterminated}"#,
        ] {
            assert!(json_string_array(bad, "keys").is_empty(), "{bad} should read as no keys");
        }
    }

    #[test]
    fn the_answer_to_a_list_carries_the_filter_and_the_undo_count() {
        let rows = [row("a.com", "x")];
        let json = list_json(&rows, "a.com", 3);
        assert!(json.contains("\"filter\":\"a.com\""));
        assert!(json.contains("\"undo\":3"));
        assert!(json.contains("\"rows\":[{"));
    }

    /// The shell, and the thing that makes it a shell.
    #[test]
    fn the_page_is_a_shell_with_no_cookie_in_it() {
        let html = page();
        assert!(html.starts_with("<!doctype html>"));
        assert!(html.contains("<body data-view=\"cookies\">"));
        assert!(html.contains(r#"<link rel="stylesheet" href="chrome.css">"#));
        assert!(html.contains(r#"<script src="cookies.js"></script>"#));
        // The filter box is focused on load, which is what makes typing a domain the first thing
        // that happens: `keys.rs` enters insert mode on a key that arrives with
        // `focus_on_editable_field` set, so the first character typed lands in the box.
        assert!(html.contains("id=\"filter\""));
        assert!(html.contains("autofocus"));
        // The destructive control starts disabled, so the page cannot delete anything before it
        // knows what is on it.
        assert!(html.contains("id=\"wipe\" type=\"button\" disabled"));
        // And it says how to use it without a mouse, because that is the only way it will be used.
        assert!(html.contains("<kbd>Enter</kbd> delete it"));
    }

    /// The stash is what makes deleting everything survivable. One deletion deep, and taking it
    /// empties it — a second Undo must not put the same cookies back twice.
    #[test]
    fn the_undo_stash_holds_one_deletion_and_is_emptied_by_taking_it() {
        stash_put(vec![row("a.com", "x"), row("b.com", "y")]);
        assert_eq!(stash_len(), 2);
        stash_put(vec![row("c.com", "z")]);
        assert_eq!(stash_len(), 1, "a second deletion replaces the first");
        assert_eq!(stash_take().len(), 1);
        assert_eq!(stash_len(), 0);
        assert!(stash_take().is_empty());
    }

    /// The name, and the shapes it has to take. Tested here rather than in `commands.rs`'s own test
    /// module so that this workstream's edits to that shared file stay the two fenced blocks.
    #[test]
    fn the_command_is_cookies_and_takes_a_domain() {
        use crate::commands::{parse, Command};
        assert_eq!(parse("cookies").unwrap(), Command::Cookies { filter: None, bg: false });
        assert_eq!(
            parse("cookies github.com").unwrap(),
            Command::Cookies { filter: Some("github.com".to_string()), bg: false }
        );
        assert_eq!(
            parse("cookies -b github.com").unwrap(),
            Command::Cookies { filter: Some("github.com".to_string()), bg: true }
        );
        // maxsplit0: a filter with a space in it stays one argument rather than losing its tail.
        assert_eq!(
            parse("cookies my domain").unwrap(),
            Command::Cookies { filter: Some("my domain".to_string()), bg: false }
        );
        // And the command runs — a name that parses and does nothing is what `is_live` is for.
        assert!(crate::exec::is_live(&parse("cookies").unwrap()));
    }

    /// The two enums make a raw integer and offer no way back, so the mapping is written out. A
    /// cookie restored with the wrong `SameSite` is a cookie a site will not send.
    #[test]
    fn the_cookie_enums_survive_a_round_trip_through_an_integer() {
        for value in [
            CookieSameSite::UNSPECIFIED,
            CookieSameSite::NO_RESTRICTION,
            CookieSameSite::LAX_MODE,
            CookieSameSite::STRICT_MODE,
        ] {
            assert_eq!(same_site_of(value.get_raw() as i32), value);
        }
        for value in [CookiePriority::LOW, CookiePriority::MEDIUM, CookiePriority::HIGH] {
            assert_eq!(priority_of(value.get_raw()), value);
        }
        // A value from a future CEF falls back to what Chromium treats a missing attribute as,
        // rather than to whichever constant happens to be first.
        assert_eq!(same_site_of(9999), CookieSameSite::UNSPECIFIED);
        assert_eq!(priority_of(9999), CookiePriority::MEDIUM);
    }

    /// `:cookies github.com` must not put `github.com` in a URL — see [`show`]. The filter travels
    /// beside the navigation, keyed by window, and is taken exactly once.
    #[test]
    fn the_filter_travels_outside_the_url_and_is_taken_once() {
        set_pending_filter(41, "mybank.example".to_string());
        assert_eq!(take_pending_filter(Some(41)), "mybank.example");
        assert_eq!(take_pending_filter(Some(41)), "", "one shot, so a reload keeps what was typed");
        assert_eq!(take_pending_filter(None), "");

        // A second `:cookies` in the same window replaces the first rather than queueing behind it.
        set_pending_filter(42, "a".to_string());
        set_pending_filter(42, "b".to_string());
        assert_eq!(take_pending_filter(Some(42)), "b");

        // And the URL itself never carries it.
        assert!(!COOKIES_URL.contains('?'));
        assert_eq!(COOKIES_URL, "bru://chrome/cookies");
    }
}
