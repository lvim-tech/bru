//! `:navigate` — `[[`, `]]`, `{{`, `}}`, `gu`, `gU`, `<Ctrl-A>`, `<Ctrl-X>`.
//!
//! A port of qutebrowser 3.7.0's `browser/navigate.py`. Six destinations, and they split in two:
//!
//! - **`up`, `increment`, `decrement`, `strip` are string surgery on the current URL.** They need
//!   nothing from the page, run entirely here, and are the reason this file has a table of tests
//!   rather than a screenshot: `incdec`'s behaviour is a regex and four lines of zero-padding
//!   arithmetic, and every one of its corners (a number in the query rather than the path, several
//!   numbers, `009` → `010`, a decrement that would go negative) is checkable without a browser.
//! - **`prev` and `next` have to look at the page's links.** That means injected JavaScript —
//!   `chrome/navigate.js`, evaluated in the tab's own world — and the answer comes back as a
//!   process message. **The page reports; Rust decides.** The heuristic (qutebrowser's
//!   `hints.prev_regexes` / `hints.next_regexes`, and the `rel=`/`class=` pass before them) lives
//!   in [`find_prevnext`] below, where it can be tested against a table.
//!
//! The transport is `scroll.rs`'s, not `ipc.rs`'s: a browser→renderer `ProcessMessage`, answered by
//! evaluating the script in the frame's V8 context and sending the result back. It deliberately
//! stays clear of the message router, whose `cefQuery` is injected into every page and is guarded by
//! a `bru://`-only check — a script bru evaluates itself is not the page asking for anything, and
//! nothing here has to be exempted from that check.

use cef::*;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::commands::NavigateTo;
use crate::tabs::SharedState;

/// The page half. Not served over `bru://` — it has to run in the page's own world.
const NAVIGATE_JS: &str = include_str!("../chrome/navigate.js");

/// `navigate <where> [-t|-b|-w]`.
///
/// `window` is a destination of its own now rather than a spelling of `-t`: `wu` puts the parent
/// directory in a window of its own, which is what qutebrowser's `wu` does.
pub fn navigate(
    state: &SharedState,
    browser: &mut Browser,
    to: NavigateTo,
    tab: bool,
    bg: bool,
    window: bool,
    count: Option<u32>,
) {
    let count = count.unwrap_or(1).max(1);

    if matches!(to, NavigateTo::Prev | NavigateTo::Next) {
        // The page has to be asked first; the navigation happens in `on_report`, a round trip later.
        request_links(browser, matches!(to, NavigateTo::Prev), tab, bg, window);
        return;
    }

    let url = current_url(browser);
    let next = match to {
        NavigateTo::Up => path_up(&url, count),
        NavigateTo::Increment => incdec(&url, count, true),
        NavigateTo::Decrement => incdec(&url, count, false),
        NavigateTo::Strip => strip(&url, count),
        NavigateTo::Prev | NavigateTo::Next => unreachable!("handled above"),
    };

    match next {
        // qutebrowser opens with `related=True`; bru has no tab ordering to relate to yet, so the
        // flags are the ones `:open` already understands.
        Ok(url) if window => {
            crate::window::open(state, &url);
        }
        Ok(url) => crate::open::open(state, browser, Some(&url), tab, bg),
        // qutebrowser raises a CommandError, which its message line shows. bru has no message line
        // yet — stderr is where every other refusal in the dispatcher goes.
        Err(message) => eprintln!("bru: navigate: {message}"),
    }
}

/// The address of the page this command was aimed at.
///
/// Read off the frame rather than out of `BruState`: the display handler's copy is one callback
/// behind a redirect, and `<Ctrl-A>` on the URL bru *was* showing would increment the wrong number.
fn current_url(browser: &mut Browser) -> String {
    browser
        .main_frame()
        .map(|frame| CefString::from(&frame.url()).to_string())
        .unwrap_or_default()
}

// -----------------------------------------------------------------------------------------------
// The URL, taken apart
// -----------------------------------------------------------------------------------------------

/// A hierarchical URL, split where `navigate` needs to cut it.
///
/// qutebrowser has `QUrl` for this and reaches for its `FullyEncoded` getters everywhere — the
/// comment in `navigate.py` warns that a decoded getter loses information. bru has no URL crate, and
/// operating on the URL string as it stands *is* the fully-encoded form, which is the same
/// guarantee arrived at from the other side: nothing here decodes, so nothing here can re-encode
/// differently from how the page was served.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Parts {
    /// `https://example.com:8080` — scheme, authority, and nothing else.
    origin: String,
    /// `/a/b`, or empty.
    path: String,
    /// What followed `?`, without it. `None` when there was no `?` at all, which is not the same as
    /// an empty query: `http://x/?` must not become `http://x/`.
    query: Option<String>,
    /// What followed `#`, without it.
    fragment: Option<String>,
}

impl Parts {
    /// `None` for anything that is not `scheme://…` — `about:blank`, `data:…`, `javascript:…`. Those
    /// have no path to walk up and no query to increment, and qutebrowser's `ensure_valid` refuses
    /// them a step later anyway.
    fn split(url: &str) -> Option<Parts> {
        let scheme_end = url.find("://")?;
        let after_scheme = scheme_end + 3;
        let rest = &url[after_scheme..];
        let authority_len = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let origin = url[..after_scheme + authority_len].to_string();
        let mut rest = &rest[authority_len..];

        let fragment = rest.find('#').map(|at| {
            let fragment = rest[at + 1..].to_string();
            rest = &rest[..at];
            fragment
        });
        let query = rest.find('?').map(|at| {
            let query = rest[at + 1..].to_string();
            rest = &rest[..at];
            query
        });

        Some(Parts { origin, path: rest.to_string(), query, fragment })
    }

    fn join(&self) -> String {
        let mut out = String::with_capacity(self.origin.len() + self.path.len() + 16);
        out.push_str(&self.origin);
        out.push_str(&self.path);
        if let Some(query) = &self.query {
            out.push('?');
            out.push_str(query);
        }
        if let Some(fragment) = &self.fragment {
            out.push('#');
            out.push_str(fragment);
        }
        out
    }
}

// -----------------------------------------------------------------------------------------------
// increment / decrement
// -----------------------------------------------------------------------------------------------

/// `navigate increment` / `navigate decrement` — `<Ctrl-A>` and `<Ctrl-X>`.
///
/// `url.incdec_segments` defaults to `[path, query]` (configdata.yml:2575), and `navigate.py` walks
/// `_URL_SEGMENTS` **reversed**, so the query is tried before the path. That ordering is not a
/// detail: on `…/page/1?p=2` it is `p=2` that moves.
fn incdec(url: &str, count: u32, increment: bool) -> Result<String, String> {
    let Some(mut parts) = Parts::split(url) else {
        return Err("No number found in URL!".to_string());
    };

    if let Some(query) = parts.query.clone() {
        if let Some(changed) = incdec_segment(&query, count, increment)? {
            parts.query = Some(changed);
            return Ok(parts.join());
        }
    }
    if let Some(changed) = incdec_segment(&parts.path.clone(), count, increment)? {
        parts.path = changed;
        return Ok(parts.join());
    }

    Err("No number found in URL!".to_string())
}

/// One segment of `incdec`: qutebrowser's `re.fullmatch(r'(.*\D|^)(?<!%)(?<!%.)(0*)(\d+)(.*)', s)`
/// and `_get_incdec_value` together.
///
/// `Ok(None)` is the regex not matching — no number in this segment, try the next one. `Err` is
/// `_get_incdec_value` refusing, which stops the whole command; the two are different answers and
/// collapsing them would make `<Ctrl-X>` on `…/0` silently walk up to the path instead of saying it
/// cannot count below zero.
///
/// The regex reads as one thing: **the last run of digits in the segment that is not inside a `%XX`
/// escape.** `(.*\D|^)` is greedy, so the rightmost run is tried first; the two lookbehinds are what
/// stop `%2F` from being read as the number 2, and when they reject a run the engine backtracks to
/// the run before it.
fn incdec_segment(segment: &str, count: u32, increment: bool) -> Result<Option<String>, String> {
    let chars: Vec<char> = segment.chars().collect();
    let is_digit = |c: char| c.is_ascii_digit();

    // Every position a digit run starts at: `(.*\D|^)` can only end where the next character is a
    // digit and the one before it is not.
    let starts: Vec<usize> = (0..chars.len())
        .filter(|&i| is_digit(chars[i]) && (i == 0 || !is_digit(chars[i - 1])))
        .collect();

    // Rightmost first, which is what the greedy `.*` asks for.
    for &start in starts.iter().rev() {
        // `(?<!%)(?<!%.)`: not immediately after a `%`, and not one character after one.
        if (start >= 1 && chars[start - 1] == '%') || (start >= 2 && chars[start - 2] == '%') {
            continue;
        }
        let end = chars[start..]
            .iter()
            .position(|&c| !is_digit(c))
            .map(|len| start + len)
            .unwrap_or(chars.len());

        // `(0*)(\d+)`: the zeroes are greedy but must leave at least one digit behind.
        let run: String = chars[start..end].iter().collect();
        let zeroes_len = run.len() - 1 - run[..run.len() - 1].trim_start_matches('0').len();
        let (zeroes, number) = run.split_at(zeroes_len);

        let pre: String = chars[..start].iter().collect();
        let post: String = chars[end..].iter().collect();
        return incdec_value(&pre, zeroes, number, &post, count, increment).map(Some);
    }

    Ok(None)
}

/// `_get_incdec_value`, including the zero-padding rule: the padding shrinks when the number grows a
/// digit and grows when it loses one, so `009` + 1 is `010` and `010` − 1 is `009` — the width the
/// author chose survives. A number with no leading zeroes is never padded, which is why `100` − 1 is
/// `99` and not `099`.
fn incdec_value(
    pre: &str,
    zeroes: &str,
    number: &str,
    post: &str,
    count: u32,
    increment: bool,
) -> Result<String, String> {
    // `int(number)` cannot fail on `\d+`, but it can overflow a u64 on a long enough run of digits,
    // which Python would have carried in a bignum. Leaving such a URL alone is the honest answer.
    let Ok(value) = number.parse::<u64>() else {
        return Err(format!("Number {number} in the URL is too long to change!"));
    };

    let value = if increment {
        value.saturating_add(count as u64)
    } else {
        if value < count as u64 {
            return Err(format!("Can't decrement {value} by {count}!"));
        }
        value - count as u64
    };

    let mut zeroes = zeroes.to_string();
    if !zeroes.is_empty() {
        let grown = value.to_string().len();
        if number.len() < grown {
            zeroes.remove(0);
        } else if number.len() > grown {
            zeroes.push('0');
        }
    }

    Ok(format!("{pre}{zeroes}{value}{post}"))
}

// -----------------------------------------------------------------------------------------------
// up / strip
// -----------------------------------------------------------------------------------------------

/// `navigate up` — `gu` and `gU`. The query and the fragment go with it, which is `path_up`'s
/// `adjusted(RemoveFragment | RemoveQuery)`.
fn path_up(url: &str, count: u32) -> Result<String, String> {
    let Some(mut parts) = Parts::split(url) else {
        return Err("Can't go up!".to_string());
    };
    parts.query = None;
    parts.fragment = None;

    if parts.path.is_empty() || parts.path == "/" {
        return Err("Can't go up!".to_string());
    }

    // `for _i in range(0, min(count, path.count('/'))): path = posixpath.join(path, '..')`, then one
    // `normpath` over the lot. A trailing slash costs one of those levels, exactly as it does in
    // posixpath: `/a/b/` goes up to `/a`.
    let levels = count.min(parts.path.matches('/').count() as u32);
    let mut path = parts.path.clone();
    for _ in 0..levels {
        path.push_str("/..");
    }
    parts.path = normpath(&path);
    Ok(parts.join())
}

/// `posixpath.normpath` for an absolute path: `.` and empty components collapse, `..` pops, and a
/// `..` at the root is the root.
fn normpath(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for component in path.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            component => out.push(component),
        }
    }
    format!("/{}", out.join("/"))
}

/// `navigate strip` — no default binding, and part of the command all the same.
fn strip(url: &str, count: u32) -> Result<String, String> {
    if count != 1 {
        return Err("Count is not supported when stripping URL components".to_string());
    }
    let Some(mut parts) = Parts::split(url) else {
        return Ok(url.to_string());
    };
    parts.query = None;
    parts.fragment = None;
    Ok(parts.join())
}

// -----------------------------------------------------------------------------------------------
// prev / next — the heuristic
// -----------------------------------------------------------------------------------------------

/// One link, as `chrome/navigate.js` reports it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Link {
    tag: String,
    rel: String,
    class: String,
    text: String,
    href: String,
}

/// `hints.next_regexes`, configdata.yml:1766. One entry per regex, in qutebrowser's order, each as
/// the literals its alternation accepts — every one of them is `\b…\b` around a fixed string, so a
/// regex engine is not needed to answer them, only [`word_search`].
///
/// The order is the whole point: qutebrowser tries each regex against *every* link before moving to
/// the next regex, so a link saying "next" wins over one saying "»" no matter where either sits in
/// the document.
const NEXT_REGEXES: &[&[&str]] = &[
    &["next"],
    &["more"],
    &["newer"],
    &[">", "\u{2192}", "\u{226b}"],
    &[">>", "\u{bb}"],
    &["continue"],
];

/// `hints.prev_regexes`, configdata.yml:1781. `\bprev(ious)?\b` is the two literals it accepts.
const PREV_REGEXES: &[&[&str]] = &[
    &["previous", "prev"],
    &["back"],
    &["older"],
    &["<", "\u{2190}", "\u{226a}"],
    &["<<", "\u{ab}"],
];

/// `navigate.py::_find_prevnext`, in two passes and in that order.
///
/// The first pass is the reliable one — `<link rel="next">` and Hugo's `class="nav-next"` — and it
/// is checked against every link before any text is looked at, because a page that says which link
/// is next should be believed over one that happens to contain the word.
fn find_prevnext(links: &[Link], prev: bool) -> Option<&Link> {
    let rel_values: &[&str] = if prev { &["prev", "previous"] } else { &["next"] };
    let classes: &[&str] = if prev { &["nav-prev"] } else { &["nav-next"] };

    for link in links {
        if link.tag != "link" && link.tag != "a" {
            continue;
        }
        // Compared as they came, not case-folded: `set(e['rel'].split(' ')) & rel_values` in
        // qutebrowser is a case-sensitive set intersection, and DESIGN.md's "1:1 with qutebrowser"
        // covers the times it is stricter than HTML as well as the times it is kinder.
        if link.rel.split(' ').any(|rel| rel_values.contains(&rel)) {
            return Some(link);
        }
        if link.class.split_whitespace().any(|class| classes.contains(&class)) {
            return Some(link);
        }
    }

    // `elems = [e for e in elems if e.tag_name() != 'link']` — a <link rel> in the document head has
    // no text to match, and matching one on its href would follow something invisible.
    let regexes = if prev { PREV_REGEXES } else { NEXT_REGEXES };
    for regex in regexes {
        for link in links.iter().filter(|link| link.tag != "link") {
            if link.text.is_empty() {
                continue;
            }
            // `flags: IGNORECASE` on every one of the patterns.
            let text = link.text.to_lowercase();
            if regex.iter().any(|literal| word_search(&text, literal)) {
                return Some(link);
            }
        }
    }

    None
}

/// `\b<literal>\b`, with Python's `\b`.
///
/// A word boundary is a position with a word character on exactly one side, and that cuts both ways
/// here: `\bnext\b` needs *non*-word characters around it, and `\b[>]\b` — whose literal is not a
/// word character — needs word characters around it, which is why `a>b` matches and `a > b` does
/// not. Getting that backwards would make `»` match on every page with a `»` anywhere in a link.
fn word_search(haystack: &str, literal: &str) -> bool {
    let Some(first) = literal.chars().next() else {
        return false;
    };
    let last = literal.chars().next_back().unwrap_or(first);

    let mut from = 0;
    while let Some(at) = haystack[from..].find(literal) {
        let at = from + at;
        let end = at + literal.len();
        let before = haystack[..at].chars().next_back();
        let after = haystack[end..].chars().next();
        if boundary(before, Some(first)) && boundary(Some(last), after) {
            return true;
        }
        // Advance by one character, not one byte: `»` is two bytes and slicing between them panics.
        from = at + first.len_utf8();
    }
    false
}

fn boundary(before: Option<char>, after: Option<char>) -> bool {
    // Python's `\w` for a str pattern is Unicode-aware, so `Ü` is a word character here too.
    let is_word = |c: Option<char>| c.map(|c| c.is_alphanumeric() || c == '_').unwrap_or(false);
    is_word(before) != is_word(after)
}

// -----------------------------------------------------------------------------------------------
// prev / next — the round trip
// -----------------------------------------------------------------------------------------------

/// Browser → renderer: "run the collector in this frame".
const QUERY: &str = "bru.navigate.query";
/// Renderer → browser: the links, and the sequence number the query carried.
const REPORT: &str = "bru.navigate.report";

/// What a report, when it comes back, is an answer to.
struct Pending {
    sequence: u64,
    browser_id: i32,
    prev: bool,
    tab: bool,
    bg: bool,
    window: bool,
    asked: std::time::Instant,
}

fn pending() -> &'static Mutex<Option<Pending>> {
    static PENDING: Mutex<Option<Pending>> = Mutex::new(None);
    &PENDING
}

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// `[[` / `]]` / `{{` / `}}` — ask the page for its links.
fn request_links(browser: &mut Browser, prev: bool, tab: bool, bg: bool, window: bool) {
    let Some(frame) = browser.main_frame() else {
        return;
    };
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed) + 1;

    if let Ok(mut pending) = pending().lock() {
        *pending = Some(Pending {
            sequence,
            browser_id: browser.identifier(),
            prev,
            tab,
            bg,
            window,
            asked: std::time::Instant::now(),
        });
    }

    let Some(mut message) = process_message_create(Some(&CefString::from(QUERY))) else {
        return;
    };
    if let Some(arguments) = message.argument_list() {
        arguments.set_string(0, Some(&CefString::from(sequence.to_string().as_str())));
    }
    frame.send_process_message(ProcessId::RENDERER, Some(&mut message));
}

/// Renderer side. Called from `ipc::renderer_on_process_message_received`; true when the message was
/// ours. **Nothing here may touch `BruState`** — this runs in the render process, where that struct
/// exists and is empty.
pub fn renderer_on_query(frame: Option<&Frame>, message: Option<&ProcessMessage>) -> bool {
    let Some(message) = message else {
        return false;
    };
    if CefString::from(&message.name()).to_string() != QUERY {
        return false;
    }
    let Some(frame) = frame else {
        return true;
    };
    let sequence = message
        .argument_list()
        .map(|arguments| CefString::from(&arguments.string(0)).to_string())
        .unwrap_or_default();

    let links = evaluate(frame, NAVIGATE_JS).unwrap_or_default();

    let Some(mut reply) = process_message_create(Some(&CefString::from(REPORT))) else {
        return true;
    };
    if let Some(arguments) = reply.argument_list() {
        arguments.set_string(0, Some(&CefString::from(links.as_str())));
        arguments.set_string(1, Some(&CefString::from(sequence.as_str())));
    }
    frame.send_process_message(ProcessId::BROWSER, Some(&mut reply));
    true
}

/// Run an expression in the frame's own V8 context and return it as a string.
///
/// `eval` has to be called between `enter` and `exit`; outside that scope there is no context for
/// the script to belong to and CEF refuses. (`scroll.rs` has the same six lines for its position
/// probe — deliberately not shared, because that file belongs to another workstream this round and
/// a helper moved out of it would be a merge conflict on the one path this project exists to keep
/// fast. Worth folding into one when both have settled.)
fn evaluate(frame: &Frame, code: &str) -> Option<String> {
    let context = frame.v8_context()?;
    if context.enter() == 0 {
        return None;
    }
    let mut value: Option<V8Value> = None;
    let mut exception: Option<V8Exception> = None;
    let ok = context.eval(
        Some(&CefString::from(code)),
        None,
        0,
        Some(&mut value),
        Some(&mut exception),
    );
    let text = (ok != 0)
        .then_some(value)
        .flatten()
        .map(|value| CefString::from(&value.string_value()).to_string());
    context.exit();
    text
}

/// Browser side of the reply. Called from `ipc::on_process_message_received` before the message
/// router sees the message; true when it was ours.
pub fn on_report(browser: Option<&Browser>, message: Option<&ProcessMessage>) -> bool {
    let Some(message) = message else {
        return false;
    };
    if CefString::from(&message.name()).to_string() != REPORT {
        return false;
    }

    let Some(arguments) = message.argument_list() else {
        return true;
    };
    let payload = CefString::from(&arguments.string(0)).to_string();
    let sequence = CefString::from(&arguments.string(1)).to_string();

    // Whose answer this is. A second `]]` before the first came back replaces the pending request,
    // and the stale reply is dropped rather than followed in the direction it is no longer for.
    let Some(request) = pending().lock().ok().and_then(|mut pending| {
        let matches = pending
            .as_ref()
            .map(|request| {
                request.sequence.to_string() == sequence
                    && Some(request.browser_id) == browser.map(|browser| browser.identifier())
            })
            .unwrap_or(false);
        if matches { pending.take() } else { None }
    }) else {
        return true;
    };

    let links = parse_links(&payload);
    let found = find_prevnext(&links, request.prev);
    debug(&format!(
        "{} links in {:.1} ms; {} -> {:?}",
        links.len(),
        request.asked.elapsed().as_secs_f64() * 1000.0,
        if request.prev { "prev" } else { "next" },
        found.map(|link| link.href.as_str()),
    ));

    let Some(link) = found else {
        // qutebrowser: message.error("No prev links found!") / "No forward links found!".
        eprintln!(
            "bru: navigate: no {} links found!",
            if request.prev { "prev" } else { "forward" }
        );
        return true;
    };

    // The one decision the page could otherwise have made for bru. `href` came out of the document,
    // and a document is free to say `javascript:…`: following it here would run script in the page
    // from a keypress that means "go to the next page". qutebrowser is protected from this by
    // QUrl's own scheme handling; bru says it out loud.
    if !is_followable(&link.href) {
        eprintln!("bru: navigate: refusing to follow {:?}", link.href);
        return true;
    }

    // CEF-NOTES trap 12: this is a message callback, and a navigation started from inside one can
    // deadlock against the router's lock. The wait is one turn of the UI loop.
    let mut task = NavOpen::new(link.href.clone(), request.tab, request.bg, request.window);
    post_task(ThreadId::UI, Some(&mut task));
    true
}

wrap_task! {
    struct NavOpen {
        url: String,
        tab: bool,
        bg: bool,
        window: bool,
    }

    impl Task {
        fn execute(&self) {
            debug_assert_ne!(currently_on(ThreadId::UI), 0);

            let Some(state) = crate::state::BruState::instance() else {
                return;
            };
            // `]]` with `-w`. Decided here rather than at the keypress because the link is only
            // known a round trip later, and a window is created around a URL rather than told to
            // load one.
            if self.window {
                crate::window::open(&state, &self.url);
                return;
            }
            let Some(mut browser) = state.lock().expect("state mutex poisoned").active_browser()
            else {
                return;
            };
            crate::open::open(&state, &mut browser, Some(&self.url), self.tab, self.bg);
        }
    }
}

/// Schemes a link may send bru to. `http`/`https`/`file` are what a paging link ever is; everything
/// else — `javascript:`, `data:`, `blob:` — is a page trying to run something, not a page turning.
fn is_followable(url: &str) -> bool {
    let Some(scheme) = url.split(':').next() else {
        return false;
    };
    matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https" | "file" | "ftp")
}

/// `tag\trel\tclass\ttext\thref`, one link per line, in document order.
fn parse_links(payload: &str) -> Vec<Link> {
    payload
        .lines()
        .filter(|line| !line.is_empty())
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let mut next = || fields.next().map(str::to_string);
            Some(Link {
                tag: next()?,
                rel: next()?,
                class: next()?,
                text: next()?,
                href: next()?,
            })
        })
        .collect()
}

/// `BRU_DEBUG_NAVIGATE=1` prints what the page reported and which link won. Off by default: it is
/// one line per `]]`, and the answer is usually the address bar.
fn debug(message: &str) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("BRU_DEBUG_NAVIGATE").is_some()) {
        eprintln!("bru[navigate]: {message}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(tag: &str, rel: &str, class: &str, text: &str, href: &str) -> Link {
        Link {
            tag: tag.to_string(),
            rel: rel.to_string(),
            class: class.to_string(),
            text: text.to_string(),
            href: href.to_string(),
        }
    }

    // -- the URL, taken apart --------------------------------------------------------------------

    #[test]
    fn a_url_splits_where_navigate_cuts_it() {
        let parts = Parts::split("https://example.com:8080/a/b?x=1#top").expect("hierarchical");
        assert_eq!(parts.origin, "https://example.com:8080");
        assert_eq!(parts.path, "/a/b");
        assert_eq!(parts.query.as_deref(), Some("x=1"));
        assert_eq!(parts.fragment.as_deref(), Some("top"));
        assert_eq!(parts.join(), "https://example.com:8080/a/b?x=1#top");

        // A `?` with nothing after it is still a query, and joining must not drop it.
        let parts = Parts::split("http://x/?").expect("hierarchical");
        assert_eq!(parts.query.as_deref(), Some(""));
        assert_eq!(parts.join(), "http://x/?");

        // No path at all.
        let parts = Parts::split("http://example.com").expect("hierarchical");
        assert_eq!(parts.path, "");
        assert_eq!(parts.join(), "http://example.com");

        // Not hierarchical: nothing to walk, nothing to increment.
        assert_eq!(Parts::split("about:blank"), None);
        assert_eq!(Parts::split("javascript:alert(1)"), None);
    }

    // -- increment / decrement -------------------------------------------------------------------

    /// The table `navigate.py`'s own tests cover, case by case. Every expectation here was checked
    /// against qutebrowser's regex and `_get_incdec_value` running in Python — the algorithm, not a
    /// memory of it.
    #[test]
    fn incdec_moves_the_last_number_in_the_url() {
        let inc = |url: &str| incdec(url, 1, true).unwrap();
        let dec = |url: &str| incdec(url, 1, false).unwrap();

        // A number in the path.
        assert_eq!(inc("http://example.com/1"), "http://example.com/2");
        assert_eq!(dec("http://example.com/2"), "http://example.com/1");
        // In the middle of a path segment, and the extension left alone.
        assert_eq!(inc("http://example.com/index1.html"), "http://example.com/index2.html");
        // A number in a directory name, not the last segment.
        assert_eq!(inc("http://example.com/1/index.html"), "http://example.com/2/index.html");
        // Several numbers: the *last* one in the segment moves.
        assert_eq!(inc("http://example.com/1/2"), "http://example.com/1/3");
        assert_eq!(inc("http://example.com/v1/page2.html"), "http://example.com/v1/page3.html");
        // A number in the query.
        assert_eq!(inc("http://example.com/?page=1"), "http://example.com/?page=2");
        assert_eq!(inc("http://example.com/?a=1&b=2"), "http://example.com/?a=1&b=3");
        // The query is tried *before* the path — `_URL_SEGMENTS` is walked in reverse.
        assert_eq!(inc("http://example.com/page/1?p=2"), "http://example.com/page/1?p=3");
        // The fragment is not in `url.incdec_segments`, so it is neither read nor lost.
        assert_eq!(inc("http://example.com/1#c2"), "http://example.com/2#c2");
        // Zero padding keeps the width the author chose.
        assert_eq!(inc("http://example.com/009"), "http://example.com/010");
        assert_eq!(inc("http://example.com/099"), "http://example.com/100");
        assert_eq!(dec("http://example.com/010"), "http://example.com/009");
        assert_eq!(inc("http://example.com/00"), "http://example.com/01");
        // No padding to keep: 100 - 1 is 99, not 099.
        assert_eq!(dec("http://example.com/100"), "http://example.com/99");
        // A port is not in the default segments, so a hostname's digits are safe.
        assert_eq!(inc("http://example.com:8080/1"), "http://example.com:8080/2");
        // Nothing to move.
        assert_eq!(incdec("http://example.com/", 1, true), Err("No number found in URL!".to_string()));
        assert_eq!(
            incdec("http://example.com/foo.html", 1, true),
            Err("No number found in URL!".to_string())
        );
        // Below zero is refused rather than wrapped, and the message says the numbers.
        assert_eq!(
            incdec("http://example.com/0", 1, false),
            Err("Can't decrement 0 by 1!".to_string())
        );
        assert_eq!(
            incdec("http://example.com/2", 5, false),
            Err("Can't decrement 2 by 5!".to_string())
        );
        // A count is how much, not how many times.
        assert_eq!(incdec("http://example.com/1", 5, true).unwrap(), "http://example.com/6");
        assert_eq!(incdec("http://example.com/9", 2, false).unwrap(), "http://example.com/7");
    }

    #[test]
    fn a_percent_escape_is_not_a_number() {
        // `%2F` is an encoded slash. Reading its `2` would rewrite the escape into `%3F`, which is a
        // different character — the two lookbehinds in qutebrowser's regex exist for exactly this.
        assert_eq!(
            incdec("http://example.com/a%2Fb", 1, true),
            Err("No number found in URL!".to_string())
        );
        // With a real number further left, the escape is skipped and that one moves.
        assert_eq!(
            incdec("http://example.com/1/a%2F", 1, true).unwrap(),
            "http://example.com/2/a%2F"
        );
        // A digit that merely follows an escape at a distance is still a number.
        assert_eq!(
            incdec("http://example.com/a%2Fb1", 1, true).unwrap(),
            "http://example.com/a%2Fb2"
        );
    }

    // -- up / strip ------------------------------------------------------------------------------

    #[test]
    fn path_up_walks_one_segment_at_a_time() {
        let up = |url: &str, count: u32| path_up(url, count).unwrap();

        assert_eq!(up("http://example.com/a/b/c", 1), "http://example.com/a/b");
        assert_eq!(up("http://example.com/a/b/c", 2), "http://example.com/a");
        // A trailing slash is a level of its own, as it is in posixpath.
        assert_eq!(up("http://example.com/a/b/", 1), "http://example.com/a");
        assert_eq!(up("http://example.com/a", 1), "http://example.com/");
        // The count is capped by the number of slashes rather than running past the root.
        assert_eq!(up("http://example.com/a/b", 99), "http://example.com/");
        // The query and the fragment go with the segment they belonged to.
        assert_eq!(up("http://example.com/a/b?x=1#c", 1), "http://example.com/a");
        // Nothing above the root.
        assert_eq!(path_up("http://example.com/", 1), Err("Can't go up!".to_string()));
        assert_eq!(path_up("http://example.com", 1), Err("Can't go up!".to_string()));
    }

    #[test]
    fn strip_takes_the_query_and_the_fragment() {
        assert_eq!(strip("http://example.com/a?x=1#c", 1).unwrap(), "http://example.com/a");
        assert_eq!(strip("http://example.com/a", 1).unwrap(), "http://example.com/a");
        assert!(strip("http://example.com/a?x=1", 2).is_err());
    }

    // -- prev / next -----------------------------------------------------------------------------

    #[test]
    fn a_rel_attribute_beats_every_word_on_the_page() {
        let links = vec![
            link("a", "", "", "next page", "http://x/text"),
            link("link", "next", "", "", "http://x/rel"),
        ];
        // Even though the text match comes first in the document, the first pass runs over every
        // link before the second pass runs at all.
        assert_eq!(find_prevnext(&links, false).unwrap().href, "http://x/rel");
        // `rel="prev"` and `rel="previous"` are both accepted; `rel="nofollow next"` is a set.
        let links = vec![link("a", "nofollow next", "", "", "http://x/rel")];
        assert_eq!(find_prevnext(&links, false).unwrap().href, "http://x/rel");
        let links = vec![link("a", "previous", "", "", "http://x/p")];
        assert_eq!(find_prevnext(&links, true).unwrap().href, "http://x/p");
    }

    #[test]
    fn hugos_nav_next_class_is_the_other_half_of_the_first_pass() {
        let links = vec![
            link("a", "", "", "", "http://x/plain"),
            link("a", "", "button nav-next", "", "http://x/class"),
        ];
        assert_eq!(find_prevnext(&links, false).unwrap().href, "http://x/class");
        let links = vec![link("a", "", "nav-prev", "", "http://x/class")];
        assert_eq!(find_prevnext(&links, true).unwrap().href, "http://x/class");
        // A class that merely contains the word is not the class.
        let links = vec![link("a", "", "nav-nextish", "", "http://x/no")];
        assert_eq!(find_prevnext(&links, false), None);
    }

    #[test]
    fn the_regexes_are_tried_in_qutebrowsers_order() {
        // "»" is regex 5 and "next" is regex 1, so the later link wins even though the arrow comes
        // first in the document: each regex is tried against every link before the next regex runs.
        let links = vec![
            link("a", "", "", "1\u{bb}2", "http://x/arrow"),
            link("a", "", "", "Next", "http://x/next"),
        ];
        assert_eq!(find_prevnext(&links, false).unwrap().href, "http://x/next");
        // With no "next" anywhere, the arrow is the answer.
        let links = vec![link("a", "", "", "1\u{bb}2", "http://x/arrow")];
        assert_eq!(find_prevnext(&links, false).unwrap().href, "http://x/arrow");
        // And a spaced-out arrow is *not*, which is qutebrowser's own behaviour rather than a
        // shortcut taken here: `\b(>>|»)\b` puts word boundaries around a character that is not a
        // word character, so it only matches with letters or digits pressed against it. A link
        // reading "Next »" is found by `\bnext\b`; one reading only "»" is found by nothing.
        let links = vec![link("a", "", "", "page 2 \u{bb}", "http://x/spaced")];
        assert_eq!(find_prevnext(&links, false), None);
    }

    #[test]
    fn every_pattern_in_both_directions() {
        for (text, href) in [
            ("Next", "n"),
            ("read more", "n"),
            ("newer posts", "n"),
            ("continue reading", "n"),
            ("1>2", "n"),
            ("1\u{2192}2", "n"),
        ] {
            let links = vec![link("a", "", "", text, href)];
            assert!(
                find_prevnext(&links, false).is_some(),
                "{text:?} should read as a next link"
            );
        }
        for (text, href) in [
            ("Previous", "p"),
            ("prev", "p"),
            ("go back", "p"),
            ("older entries", "p"),
            ("1<2", "p"),
            ("1\u{ab}2", "p"),
        ] {
            let links = vec![link("a", "", "", text, href)];
            assert!(
                find_prevnext(&links, true).is_some(),
                "{text:?} should read as a prev link"
            );
        }
    }

    #[test]
    fn a_word_inside_another_word_is_not_a_match() {
        // `\bnext\b`: "nextdoor" is not next, and neither is "context".
        for text in ["nextdoor", "context", "moreish", "backwards", "prevention"] {
            let links = vec![link("a", "", "", text, "http://x/no")];
            assert_eq!(find_prevnext(&links, false), None, "{text:?} matched as next");
            assert_eq!(find_prevnext(&links, true), None, "{text:?} matched as prev");
        }
    }

    #[test]
    fn a_head_link_is_skipped_in_the_text_pass() {
        // `<link>` elements are dropped before the regexes run: they have no visible text, and the
        // first pass is the only place they can win.
        let links = vec![link("link", "", "", "next", "http://x/head")];
        assert_eq!(find_prevnext(&links, false), None);
    }

    #[test]
    fn word_boundaries_cut_both_ways() {
        // A word literal wants non-word characters around it.
        assert!(word_search("the next page", "next"));
        assert!(word_search("next", "next"));
        assert!(word_search("[next]", "next"));
        assert!(!word_search("nextdoor", "next"));
        // A symbol literal wants word characters around it — Python's `\b` is a boundary, not a
        // space, and `\b[>]\b` therefore matches inside `1>2` and not inside `1 > 2`.
        assert!(word_search("1>2", ">"));
        assert!(!word_search("1 > 2", ">"));
        assert!(!word_search(">", ">"));
        // Multi-byte literals are stepped over a character at a time, not a byte.
        assert!(word_search("a\u{bb}b", "\u{bb}"));
        assert!(!word_search("a \u{bb} b", "\u{bb}"));
    }

    // -- the wire --------------------------------------------------------------------------------

    #[test]
    fn the_payload_is_five_tab_separated_fields_a_line() {
        let links = parse_links(
            "a\tnext\tbtn\tNext page\thttp://x/2\nlink\t\t\t\thttp://x/style.css\n\n",
        );
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].tag, "a");
        assert_eq!(links[0].rel, "next");
        assert_eq!(links[0].text, "Next page");
        assert_eq!(links[0].href, "http://x/2");
        assert_eq!(links[1].tag, "link");
        assert_eq!(links[1].href, "http://x/style.css");
        // A short line is dropped rather than shifting every field along by one.
        assert_eq!(parse_links("a\tb\tc"), Vec::new());
    }

    #[test]
    fn only_a_page_turn_may_be_followed() {
        assert!(is_followable("http://example.com/2"));
        assert!(is_followable("https://example.com/2"));
        assert!(is_followable("file:///tmp/page2.html"));
        assert!(!is_followable("javascript:alert(1)"));
        assert!(!is_followable("data:text/html,<h1>x"));
        assert!(!is_followable("blob:https://example.com/uuid"));
        assert!(!is_followable(""));
    }
}
