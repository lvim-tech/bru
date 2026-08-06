//! `:open` — and the decision underneath it: is what was typed a URL, a path, or a search?
//!
//! The navigation is three lines. The decision is the milestone. Every browser gets it wrong in its
//! own way, and qutebrowser's way is the one this user has lived with for years, so it is *ported*
//! here rather than reinvented — from `utils/urlutils.py` (`fuzzy_url` :233, `is_url` :300,
//! `_is_url_naive` :180, `_parse_search_term` :115, `get_path_if_valid` :390) and from the Qt
//! function all four lean on, `QUrl::fromUserInput`.
//!
//! The rules, in the order qutebrowser applies them:
//!
//! 1. An **absolute path that exists** wins outright — `/etc/hosts` opens the file, never a search.
//! 2. A string with an **explicit scheme** and no space is a URL: `http://x`, `about:blank`, and
//!    also `localhost:8080`, where Qt reads `localhost` as the scheme and `8080` as the path.
//! 3. `localhost`, `127.0.0.1` and `::1` are URLs by name.
//! 4. Otherwise the **naive** check (`url.auto_search` defaults to `naive`, configdata.yml :2564):
//!    a host that is a literal IP address, or a host whose last label looks like a TLD.
//! 5. Anything else is a **search**. If the first word names a search engine it is the engine and
//!    the rest is the term; if it does not, the whole string goes to `DEFAULT`.
//!
//! Search engines come from `~/.config/bru/config.lua` — DESIGN.md: "Search engines are a table in
//! `config.lua`, not a copied file." Nothing of qutebrowser's is read at runtime.
//!
//! Qt is not a dependency here and neither is a URL crate. What [`Parts`] implements is not a URL
//! parser for general use: it is exactly as much of `QUrl(s, TolerantMode)` and
//! `QUrl::fromUserInput` as the five rules above consult, and every case it is asked about is in the
//! tests at the bottom.

use cef::*;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use crate::tabs::SharedState;

/// qutebrowser's `url.searchengines` default (configdata.yml :2591).
pub const DEFAULT_ENGINE_URL: &str = "https://duckduckgo.com/?q={}";

/// The name of the engine a search with no prefix falls through to.
///
/// **DECISIONS.md item 4**, which asks what a bare `:open words` should do. This is qutebrowser's
/// answer and therefore bru's: the whole string is the term and `DEFAULT` is the engine.
pub const DEFAULT_ENGINE: &str = "DEFAULT";

// -- what a typed string turns out to mean ------------------------------------------------------

/// What [`decide`] made of the string.
///
/// The variants exist so a test can say *why* a string resolved as it did — asserting only on the
/// final URL would pass just as happily if `example.com` reached DuckDuckGo as a search term.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// An address. Already carries a scheme, whether or not the user typed one.
    Url(String),
    /// A local file that exists, as a `file://` URL.
    File(String),
    /// A search, and which engine answered it.
    Search { engine: String, term: String, url: String },
}

impl Target {
    /// The URL to load, whichever kind of target it turned out to be.
    pub fn url(&self) -> &str {
        match self {
            Target::Url(url) | Target::File(url) => url,
            Target::Search { url, .. } => url,
        }
    }
}

// -- the search engine table --------------------------------------------------------------------

/// `name -> url template`, from `bru.search(name, template)` in `config.lua`.
///
/// `{}` in a template is the search term, percent-encoded. `{quoted}`, `{semiquoted}` and
/// `{unquoted}` are qutebrowser's other spellings and mean what they do there; `{{` and `}}` are
/// literal braces.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchEngines {
    /// Ordered so the completion category and any error message list them the same way twice.
    engines: BTreeMap<String, String>,
}

impl Default for SearchEngines {
    /// Just `DEFAULT`, pointing where qutebrowser's does. A config that sets nothing still searches.
    fn default() -> SearchEngines {
        let mut engines = BTreeMap::new();
        engines.insert(DEFAULT_ENGINE.to_string(), DEFAULT_ENGINE_URL.to_string());
        SearchEngines { engines }
    }
}

impl SearchEngines {
    /// Add or replace an engine. The error string is what `bru.search` raises into Lua, so it has to
    /// read as a message to whoever wrote `config.lua`.
    pub fn set(&mut self, name: &str, template: &str) -> Result<(), String> {
        if name.is_empty() {
            return Err("a search engine needs a name".to_string());
        }
        // qutebrowser forbids spaces in the key (configdata.yml :2597) for the reason bru needs
        // too: the first *word* of `:open` is what is looked up, so a name with a space in it could
        // never be typed.
        if name.chars().any(char::is_whitespace) {
            return Err(format!("search engine name {name:?} contains whitespace"));
        }
        if !template.contains('{') {
            return Err(format!(
                "search engine {name:?} has no {{}} in {template:?}, so the term would be dropped"
            ));
        }
        // Fail here rather than at the first search: a stray `{title}` is a typo, and startup is
        // where the config author is looking.
        format_template(template, "probe").map_err(|e| format!("search engine {name:?}: {e}"))?;
        self.engines.insert(name.to_string(), template.to_string());
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.engines.get(name).map(String::as_str)
    }

    pub fn contains(&self, name: &str) -> bool {
        self.engines.contains_key(name)
    }

    /// Every engine, name first, in a stable order.
    // Nothing calls it until the completion lands; the tests do, which is what keeps it honest.
    #[allow(dead_code)]
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.engines.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// The "Search engines" completion category: `(name, template)`, sorted, without `DEFAULT`.
    ///
    /// Shaped to `data.rs`'s `Sources::search_engines`, which is what `completion.rs` calls.
    /// `DEFAULT` is left out because it is not a name anyone types — it is what a search with no
    /// prefix falls through to, so offering it as a completion would be offering a prefix that
    /// changes nothing.
    pub fn for_completion(&self) -> Vec<(String, String)> {
        self.engines
            .iter()
            .filter(|(name, _)| name.as_str() != DEFAULT_ENGINE)
            .map(|(name, template)| (name.clone(), template.clone()))
            .collect()
    }

    /// The URL `engine` gives for `term`, or `None` if there is no such engine.
    pub fn search_url(&self, engine: &str, term: &str) -> Option<String> {
        let template = self.get(engine)?;
        // The template was checked when it went in, so this cannot fail; if it somehow does, a
        // search that does nothing is better than a panic in a browser.
        format_template(template, term).ok()
    }
}

/// qutebrowser's `_parse_search_term` (urlutils.py :115).
///
/// `("ddg", "python dict")` when the first word names an engine, `(DEFAULT, whole string)` when it
/// does not — **DECISIONS item 4**. `None` for a string with nothing in it.
///
/// `url.open_base_url` (configdata.yml :2583, default false) is not implemented: a lone `ddg` is a
/// search for the word "ddg", not DuckDuckGo's front page. Adding it is one branch here.
fn parse_search_term<'a>(s: &'a str, engines: &SearchEngines) -> Option<(String, &'a str)> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    match s.split_once(char::is_whitespace) {
        Some((first, rest)) if engines.contains(first) => {
            Some((first.to_string(), rest.trim_start()))
        }
        _ => Some((DEFAULT_ENGINE.to_string(), s)),
    }
}

/// Python's `str.format` as far as a search template uses it.
///
/// `{}`/`{semiquoted}` quote everything but `/`, `{quoted}` quotes `/` too, `{unquoted}` quotes
/// nothing, `{{`/`}}` are literal braces. Anything else in braces is an error the config author
/// sees at startup.
fn format_template(template: &str, term: &str) -> Result<String, String> {
    let mut out = String::with_capacity(template.len() + term.len());
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                out.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                out.push('}');
            }
            '{' => {
                let mut name = String::new();
                let mut closed = false;
                for c in chars.by_ref() {
                    if c == '}' {
                        closed = true;
                        break;
                    }
                    name.push(c);
                }
                if !closed {
                    return Err(format!("unclosed {{{name}"));
                }
                match name.as_str() {
                    "" | "semiquoted" => out.push_str(&percent_encode(term, "/")),
                    "quoted" => out.push_str(&percent_encode(term, "")),
                    "unquoted" => out.push_str(term),
                    other => return Err(format!("unknown placeholder {{{other}}}")),
                }
            }
            '}' => return Err("a lone } — write }} for a literal brace".to_string()),
            c => out.push(c),
        }
    }
    Ok(out)
}

/// `urllib.parse.quote`: everything but `A-Za-z0-9_.-~` and the characters in `safe` becomes `%XX`,
/// over the UTF-8 bytes.
fn percent_encode(s: &str, safe: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        let c = byte as char;
        let unreserved = byte.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-' | '~');
        if unreserved || (byte.is_ascii() && safe.contains(c)) {
            out.push(c);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

// -- the decision -------------------------------------------------------------------------------

/// What `input` means, against the real filesystem. qutebrowser's `fuzzy_url` (urlutils.py :233).
///
/// `None` when there is nothing to open — an empty string, or one that is only whitespace.
pub fn decide(input: &str, engines: &SearchEngines) -> Option<Target> {
    decide_with(input, engines, |path| path.exists())
}

/// [`decide`] with the filesystem handed in, so a test can ask about a path that is not on this
/// machine — and so the one rule that reads the disk is visible rather than buried.
pub fn decide_with(
    input: &str,
    engines: &SearchEngines,
    file_exists: impl Fn(&Path) -> bool,
) -> Option<Target> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }

    // 1. An absolute path that exists. Before everything, so a file called `example.com` in the
    //    root of the disk still opens as a file when named absolutely.
    if let Some(path) = path_if_valid(input, file_exists) {
        return Some(Target::File(file_url(&path)));
    }

    // 2-4. A URL.
    if is_url(input) {
        return from_user_input(input).map(|parts| Target::Url(parts.render()));
    }

    // 5. A search.
    let (engine, term) = parse_search_term(input, engines)?;
    match engines.search_url(&engine, term) {
        Some(url) => Some(Target::Search { engine, term: term.to_string(), url }),
        // No DEFAULT engine at all — a config that deleted it. qutebrowser falls back to opening
        // the text as an address rather than doing nothing (urlutils.py :262).
        None => from_user_input(input).map(|parts| Target::Url(parts.render())),
    }
}

/// qutebrowser's `get_path_if_valid` with `relative=False, check_exists=True` (urlutils.py :390).
///
/// Absolute only. A relative path would make `:open news` depend on which directory bru was started
/// from, and qutebrowser does not do that either.
fn path_if_valid(input: &str, file_exists: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    let expanded = expand_user(input);
    let path = PathBuf::from(&expanded);
    if !path.is_absolute() {
        return None;
    }
    file_exists(&path).then_some(path)
}

/// `os.path.expanduser`: a leading `~` or `~/` becomes `$HOME`. `~user` is not expanded, as it
/// needs the password database and no keybinding types it.
fn expand_user(input: &str) -> String {
    let rest = match input.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => rest,
        _ => return input.to_string(),
    };
    match std::env::var("HOME") {
        Ok(home) => format!("{home}{rest}"),
        Err(_) => input.to_string(),
    }
}

/// A `file://` URL for an absolute path, with the bytes a URL cannot carry percent-encoded.
fn file_url(path: &Path) -> String {
    let path = path.to_string_lossy();
    format!("file://{}", percent_encode(&path, "/:@&=+$,-_.!~*'()"))
}

/// qutebrowser's `is_url` with `url.auto_search` at its default, `naive` (urlutils.py :300).
fn is_url(input: &str) -> bool {
    let input = input.trim();
    let strict = Parts::parse(input);
    // "This will also catch non-URLs containing spaces" — `ddg python dict` dies here, because
    // `http://ddg python dict` has a space in its host and no host may.
    let Some(user_input) = from_user_input(input) else {
        return false;
    };

    // A scheme settles it. `http://x`, and `localhost:8080` too — Qt reads `localhost` as the
    // scheme there, which is exactly why the port form works without one.
    if has_explicit_scheme(&strict) && !input.contains(' ') {
        return true;
    }
    if matches!(user_input.host.as_str(), "localhost" | "127.0.0.1" | "::1") {
        return true;
    }
    if is_special_url(&strict) {
        return true;
    }
    // naive. The userinfo check is qutebrowser's: without it `foo bar@example.com` would be a URL,
    // because the host still looks like one.
    !user_input.user_name().contains(' ') && is_url_naive(input)
}

/// qutebrowser's `_is_url_naive` (urlutils.py :180): a literal IP address, or a plausible TLD.
fn is_url_naive(input: &str) -> bool {
    let Some(url) = from_user_input(input) else {
        return false;
    };
    let host = &url.host;

    // `host in urlstr` is qutebrowser's guard against Qt turning `1337` or `0xDEAD` into an
    // address: the host it invented has to be one the user actually typed.
    if host.parse::<std::net::IpAddr>().is_ok() && input.contains(host.as_str()) {
        return true;
    }

    has_tld(host) && !host.chars().any(is_forbidden_in_host)
}

/// `\.([^.0-9_-]+|xn--[a-z0-9-]+)$` — the label after the last dot, and it may not be digits.
///
/// Equivalent to the regex because the character class excludes `.`, so a match can only ever be
/// the whole final label.
fn has_tld(host: &str) -> bool {
    let Some((_, tld)) = host.rsplit_once('.') else {
        return false;
    };
    if tld.is_empty() {
        return false;
    }
    if !tld
        .chars()
        .any(|c| c == '.' || c.is_ascii_digit() || c == '_' || c == '-')
    {
        return true;
    }
    // Punycode: `xn--` plus the encoded label, which is where the digits and dashes come from.
    tld.strip_prefix("xn--").is_some_and(|rest| {
        !rest.is_empty()
            && rest
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    })
}

/// `[\x00-,/:-`{-¶]` — everything a hostname has no business
/// (the first bound is written `\x00` rather than as the byte itself: a raw NUL makes
/// `file` call this source `data` and makes plain `grep -rn` skip it without a word,
/// which cost two people an afternoon each)
/// holding. Note what is *not* in it: `.`, `-`, and the digits.
fn is_forbidden_in_host(c: char) -> bool {
    matches!(c, '\u{0}'..='\u{2c}' | '\u{2f}' | '\u{3a}'..='\u{60}' | '\u{7b}'..='\u{b6}')
}

/// qutebrowser's `_has_explicit_scheme` (urlutils.py :273).
///
/// The `:` guard is the one that keeps `std::vector` a search rather than a visit to the `std`
/// scheme.
fn has_explicit_scheme(url: &Parts) -> bool {
    url.valid
        && !url.scheme.is_empty()
        && (!url.host.is_empty() || !url.path.is_empty())
        && !url.path.starts_with(':')
}

/// qutebrowser's `is_special_url` (urlutils.py :288), with bru's own scheme where `qute` was.
fn is_special_url(url: &Parts) -> bool {
    url.valid && matches!(url.scheme.as_str(), "about" | "bru" | "file")
}

// -- as much of QUrl as the rules above ask about ------------------------------------------------

/// The pieces of a URL the decision consults, and whether Qt would have called it valid.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Parts {
    valid: bool,
    /// Lowercased, empty when the string carried no scheme.
    scheme: String,
    /// Everything between `//` and `@`, password included.
    userinfo: String,
    /// Lowercased, and without the brackets an IPv6 literal is written in.
    host: String,
    port: Option<u32>,
    path: String,
    /// Query and fragment, kept verbatim so the URL can be put back together.
    tail: String,
    /// Whether there was a `//` — `about:blank` has no authority, `http://x` does.
    authority: bool,
}

impl Parts {
    /// `QUrl(s, QUrl::TolerantMode)`: parse what is there and add nothing.
    fn parse(input: &str) -> Parts {
        let mut parts = Parts { valid: true, ..Parts::default() };
        let mut rest = input;

        if let Some(scheme_end) = scheme_end(input) {
            parts.scheme = input[..scheme_end].to_ascii_lowercase();
            rest = &input[scheme_end + 1..];
        }

        if let Some(after) = rest.strip_prefix("//") {
            parts.authority = true;
            let end = after
                .find(['/', '?', '#'])
                .unwrap_or(after.len());
            let (authority, remainder) = after.split_at(end);
            rest = remainder;

            let hostport = match authority.rsplit_once('@') {
                Some((userinfo, hostport)) => {
                    parts.userinfo = userinfo.to_string();
                    hostport
                }
                None => authority,
            };

            let (host, port, bracketed) = split_port(hostport);
            parts.host = host.to_ascii_lowercase();
            match port {
                // `host:` is a host with no port, which Qt accepts. `host:nonsense` is not a URL.
                Some("") | None => {}
                Some(digits) => match digits.parse::<u32>() {
                    Ok(port) if port <= 65535 => parts.port = Some(port),
                    _ => parts.valid = false,
                },
            }
            // A bracketed host is an IPv6 literal and is *made* of colons, so the character rule
            // cannot apply to it; being an address is the rule instead.
            if bracketed {
                if parts.host.parse::<std::net::Ipv6Addr>().is_err() {
                    parts.valid = false;
                }
            } else if parts.host.chars().any(is_illegal_host_char) {
                parts.valid = false;
            }
        }

        let end = rest.find(['?', '#']).unwrap_or(rest.len());
        parts.path = rest[..end].to_string();
        parts.tail = rest[end..].to_string();
        parts
    }

    /// The user name, which is the userinfo up to the password. Only ever asked whether it has a
    /// space in it.
    fn user_name(&self) -> &str {
        match self.userinfo.split_once(':') {
            Some((user, _password)) => user,
            None => &self.userinfo,
        }
    }

    /// Back to a string CEF can be handed.
    fn render(&self) -> String {
        let mut out = String::new();
        if !self.scheme.is_empty() {
            out.push_str(&self.scheme);
            out.push(':');
        }
        if self.authority {
            out.push_str("//");
            if !self.userinfo.is_empty() {
                out.push_str(&self.userinfo);
                out.push('@');
            }
            // An IPv6 literal goes back inside its brackets or the port would be unreadable.
            if self.host.contains(':') {
                out.push('[');
                out.push_str(&self.host);
                out.push(']');
            } else {
                out.push_str(&self.host);
            }
            if let Some(port) = self.port {
                out.push(':');
                out.push_str(&port.to_string());
            }
        }
        out.push_str(&self.path);
        out.push_str(&self.tail);
        out
    }
}

/// The index of the `:` ending a scheme, if the string starts with one. `ALPHA *(ALPHA / DIGIT /
/// "+" / "-" / ".")`, which is why `example.com:8080` has the scheme `example.com`.
fn scheme_end(input: &str) -> Option<usize> {
    let colon = input.find(':')?;
    let scheme = &input[..colon];
    let mut chars = scheme.chars();
    if !chars.next()?.is_ascii_alphabetic() {
        return None;
    }
    chars
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        .then_some(colon)
}

/// Split `host:port`, leaving an IPv6 literal's own colons alone. The third value says whether the
/// host arrived in brackets, which is the only thing that makes a host full of colons legal.
fn split_port(hostport: &str) -> (&str, Option<&str>, bool) {
    if let Some(close) = hostport.rfind(']') {
        let host = hostport[..=close].trim_start_matches('[').trim_end_matches(']');
        return match hostport[close + 1..].strip_prefix(':') {
            Some(port) => (host, Some(port), true),
            None => (host, None, true),
        };
    }
    match hostport.rsplit_once(':') {
        Some((host, port)) => (host, Some(port), false),
        None => (hostport, None, false),
    }
}

/// Characters Qt refuses to put in a hostname. A space among them is what makes `ddg python dict`
/// fail to parse as a URL, which is what makes it a search.
fn is_illegal_host_char(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '\0' | '#' | '%' | '/' | ':' | '?' | '@' | '[' | ']' | '\\' | '<' | '>' | '"' | '^'
                | '|' | '{' | '}' | '`'
        )
}

/// `QUrl::fromUserInput`: what a browser's address bar does with a string that may have no scheme.
///
/// Qt's own order, and each step earns its place in the tests: an IP literal first, then the string
/// as it stands if it parses with a scheme *and* prepending `http://` does not turn its first
/// component into a port, then `http://` in front of it.
fn from_user_input(input: &str) -> Option<Parts> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }

    // A bare address, which would otherwise be read as host-and-port or as a scheme.
    if let Ok(ip) = trimmed.parse::<std::net::IpAddr>() {
        return Some(Parts {
            valid: true,
            scheme: "http".to_string(),
            host: ip.to_string(),
            authority: true,
            ..Parts::default()
        });
    }

    let as_typed = Parts::parse(trimmed);
    let prepended = Parts::parse(&format!("http://{trimmed}"));

    // `urlPrepended.port() == -1` is Qt's test for host:port written without a scheme —
    // `localhost:8080` parses as scheme `localhost`, and this is what stops it being read that way.
    if as_typed.valid
        && !as_typed.scheme.is_empty()
        && (!as_typed.host.is_empty() || !as_typed.path.is_empty())
        && prepended.port.is_none()
    {
        return Some(as_typed);
    }

    if prepended.valid && (!prepended.host.is_empty() || !prepended.path.is_empty()) {
        let mut url = prepended;
        // Qt's one guess at a scheme from the name: `ftp.example.com` is not http.
        let first_label = trimmed.split('.').next().unwrap_or(trimmed);
        if first_label.eq_ignore_ascii_case("ftp") {
            url.scheme = "ftp".to_string();
        }
        return Some(url);
    }

    None
}

// -- what config.lua installed, and what bru does with it ----------------------------------------

/// The search engines and the start page, put here by [`crate::config::Config::into_parsers`] once
/// the Lua state has been dropped.
///
/// A global rather than a field on `BruState` on purpose: the decision is pure and its inputs are
/// read-only after startup, so threading them through every caller would buy nothing. `None` until
/// startup runs — and permanently so in the renderer and GPU processes, which never load a config.
static INSTALLED: RwLock<Option<Installed>> = RwLock::new(None);

#[derive(Clone, Debug)]
struct Installed {
    engines: SearchEngines,
    /// Raw, as `config.lua` wrote it. Resolved through [`decide`] when it is used, so
    /// `bru.set("start_page", "example.com")` reaches `http://example.com`.
    start_page: Option<String>,
}

/// Hand the config over. Called once, at startup, from `config.rs`.
pub fn install(engines: SearchEngines, start_page: Option<String>) {
    *INSTALLED
        .write()
        .expect("the open.rs config lock is never poisoned") = Some(Installed { engines, start_page });
}

/// The engines `config.lua` set, or just `DEFAULT` if it set none.
///
/// Cloned rather than borrowed: the table has a handful of entries and this is on the `:open` path,
/// not the key path. `completion.rs` wants this too, for the "Search engines" category.
pub fn engines() -> SearchEngines {
    INSTALLED
        .read()
        .expect("the open.rs config lock is never poisoned")
        .as_ref()
        .map(|installed| installed.engines.clone())
        .unwrap_or_default()
}

/// Where bru goes with no argument, and what `:open -t` with no URL opens.
///
/// **DECISIONS.md item 7**: `bru.set("start_page", ...)` in `config.lua`, falling back to
/// [`crate::app::HOME`] — which is qutebrowser's `url.default_page` default (configdata.yml :2569).
/// The value goes through the same decision as anything typed at `:open`, so it may be written
/// `example.com`; qutebrowser's `FuzzyUrl` type does the same.
pub fn start_page() -> String {
    let configured = INSTALLED
        .read()
        .expect("the open.rs config lock is never poisoned")
        .as_ref()
        .and_then(|installed| installed.start_page.clone());

    match configured {
        Some(page) => decide(&page, &engines())
            .map(|target| target.url().to_string())
            .unwrap_or_else(|| crate::app::HOME.to_string()),
        None => crate::app::HOME.to_string(),
    }
}

/// `:open [-t|-b] [url]` — the whole command.
///
/// `url` is `None` for a bare `:open -t`, which qutebrowser sends to `url.default_page`
/// (commands.py :303). `tab` opens a new tab, `bg` opens it without leaving the current one; the
/// dispatcher passes `-w` in as `tab` until windows mean something.
///
/// Quickmarks are looked up *before* this in qutebrowser (commands.py :348) — that lookup belongs to
/// `data.rs` and to the dispatcher, not here.
/// The address `:open` would load for `url`, decided but not opened: the engine chosen, the file
/// resolved, the search built. `None` is the same refusal `open` prints.
///
/// `-w` needs this and `open` cannot give it: a window is *created around* a URL — the first tab is
/// made before the window exists, so there is no browser to hand to `open` — where a tab or an
/// in-place load is told to go somewhere. One decision function, so `:open -w ddg rust` searches
/// exactly as `:open ddg rust` does.
pub fn resolve(url: Option<&str>) -> Option<String> {
    let engines = engines();
    match url.map(str::trim).filter(|url| !url.is_empty()) {
        Some(url) => match decide(url, &engines) {
            Some(target) => Some(target.url().to_string()),
            None => {
                eprintln!("bru: open: nothing to open in {url:?}");
                None
            }
        },
        None => Some(start_page()),
    }
}

pub fn open(state: &SharedState, browser: &mut Browser, url: Option<&str>, tab: bool, bg: bool) {
    let engines = engines();
    let target = match url.map(str::trim).filter(|url| !url.is_empty()) {
        Some(url) => match decide(url, &engines) {
            Some(target) => target,
            None => {
                eprintln!("bru: open: nothing to open in {url:?}");
                return;
            }
        },
        None => Target::Url(start_page()),
    };

    let url = target.url();
    if tab || bg {
        crate::tabs::new_tab(state, url, bg);
    } else if let Some(frame) = browser.main_frame() {
        frame.load_url(Some(&CefString::from(url)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The table `config.lua` is expected to grow, and what the tests below decide against.
    fn engines() -> SearchEngines {
        let mut engines = SearchEngines::default();
        engines.set("ddg", "https://duckduckgo.com/?q={}").unwrap();
        engines.set("gh", "https://github.com/search?q={}").unwrap();
        engines
    }

    /// No file exists. The one path case that needs a real filesystem asks for one explicitly.
    fn decide_nofs(input: &str) -> Option<Target> {
        decide_with(input, &engines(), |_| false)
    }

    fn url_of(input: &str) -> String {
        decide_nofs(input)
            .unwrap_or_else(|| panic!("{input:?} decided to nothing"))
            .url()
            .to_string()
    }

    // -- the table the milestone names ----------------------------------------------------------

    #[test]
    fn every_input_the_milestone_names() {
        // A hostname with a TLD, no scheme.
        assert_eq!(
            decide_nofs("example.com"),
            Some(Target::Url("http://example.com".to_string()))
        );
        // Host and port, no scheme. Qt reads `localhost` as a scheme; prepending `http://` and
        // finding a port is what stops it.
        assert_eq!(
            decide_nofs("localhost:8080"),
            Some(Target::Url("http://localhost:8080".to_string()))
        );
        // A literal IPv4 address.
        assert_eq!(
            decide_nofs("192.168.1.1"),
            Some(Target::Url("http://192.168.1.1".to_string()))
        );
        // An explicit scheme, and a host too short to have a TLD.
        assert_eq!(decide_nofs("http://x"), Some(Target::Url("http://x".to_string())));

        // A search with an engine prefix.
        assert_eq!(
            decide_nofs("ddg python dict"),
            Some(Target::Search {
                engine: "ddg".to_string(),
                term: "python dict".to_string(),
                url: "https://duckduckgo.com/?q=python%20dict".to_string(),
            })
        );
        // A search without one — DECISIONS item 4: it goes to DEFAULT, whole and unsplit.
        assert_eq!(
            decide_nofs("python dict"),
            Some(Target::Search {
                engine: "DEFAULT".to_string(),
                term: "python dict".to_string(),
                url: "https://duckduckgo.com/?q=python%20dict".to_string(),
            })
        );
        // A bare word that is also a hostname: `localhost` is one of the three names that are URLs
        // by fiat...
        assert_eq!(
            decide_nofs("localhost"),
            Some(Target::Url("http://localhost".to_string()))
        );
        // ...while any other bare word is not, however resolvable it may be. `naive` does no DNS,
        // and this is the case that would need `url.auto_search = dns` to go the other way.
        assert_eq!(
            decide_nofs("github"),
            Some(Target::Search {
                engine: "DEFAULT".to_string(),
                term: "github".to_string(),
                url: "https://duckduckgo.com/?q=github".to_string(),
            })
        );
    }

    #[test]
    fn an_absolute_path_that_exists_is_a_file() {
        // The path case, against the real filesystem: /etc/hosts is there on this machine.
        assert!(Path::new("/etc/hosts").exists(), "this test needs /etc/hosts");
        assert_eq!(
            decide("/etc/hosts", &engines()),
            Some(Target::File("file:///etc/hosts".to_string()))
        );

        // The same string with nothing behind it is not a file, and is not a URL either — no host,
        // so nothing to visit. It becomes a search, which is qutebrowser's answer too.
        assert_eq!(
            decide_nofs("/etc/hosts"),
            Some(Target::Search {
                engine: "DEFAULT".to_string(),
                term: "/etc/hosts".to_string(),
                url: "https://duckduckgo.com/?q=/etc/hosts".to_string(),
            })
        );

        // A path wins over a hostname: a file named like a domain still opens as a file.
        assert_eq!(
            decide_with("/srv/example.com", &engines(), |p| p == Path::new("/srv/example.com")),
            Some(Target::File("file:///srv/example.com".to_string()))
        );
        // Relative paths are not paths — `:open news` must not depend on bru's cwd.
        assert_eq!(
            decide_with("news", &engines(), |_| true),
            Some(Target::Search {
                engine: "DEFAULT".to_string(),
                term: "news".to_string(),
                url: "https://duckduckgo.com/?q=news".to_string(),
            })
        );
        // A space in a path survives into the URL as %20.
        assert_eq!(
            decide_with("/tmp/a file", &engines(), |_| true),
            Some(Target::File("file:///tmp/a%20file".to_string()))
        );
    }

    #[test]
    fn a_tilde_expands_before_the_path_check() {
        let home = std::env::var("HOME").expect("this test needs $HOME");
        let expected = format!("file://{home}/.config");
        assert_eq!(
            decide_with("~/.config", &engines(), |p| p == Path::new(&format!("{home}/.config"))),
            Some(Target::File(expected))
        );
    }

    // -- URL or search, case by case ------------------------------------------------------------

    #[test]
    fn schemes_are_believed() {
        assert_eq!(url_of("https://example.com/a/b?c=d#e"), "https://example.com/a/b?c=d#e");
        assert_eq!(url_of("about:blank"), "about:blank");
        assert_eq!(url_of("file:///etc/hosts"), "file:///etc/hosts");
        assert_eq!(url_of("bru://chrome/top.html"), "bru://chrome/top.html");
        // A scheme bru has never heard of is still a scheme.
        assert_eq!(url_of("mailto:someone@example.com"), "mailto:someone@example.com");
        // ...but a C++ symbol is not, because the path starts with the second colon.
        assert!(matches!(decide_nofs("std::vector"), Some(Target::Search { .. })));
    }

    #[test]
    fn hosts_ports_and_addresses() {
        assert_eq!(url_of("example.com:8080"), "http://example.com:8080");
        assert_eq!(url_of("example.com/path"), "http://example.com/path");
        assert_eq!(url_of("www.example.com"), "http://www.example.com");
        assert_eq!(url_of("127.0.0.1"), "http://127.0.0.1");
        assert_eq!(url_of("127.0.0.1:8000"), "http://127.0.0.1:8000");
        assert_eq!(url_of("::1"), "http://[::1]");
        assert_eq!(url_of("[::1]:8000"), "http://[::1]:8000");
        assert_eq!(url_of("localhost/status"), "http://localhost/status");
        // Punycode is a TLD.
        assert_eq!(url_of("example.xn--p1ai"), "http://example.xn--p1ai");
        // An address Qt would invent out of a number is not one.
        assert!(matches!(decide_nofs("1337"), Some(Target::Search { .. })));
        assert!(matches!(decide_nofs("23.42"), Some(Target::Search { .. })));
        // A host whose last label is digits is not a TLD.
        assert!(matches!(decide_nofs("version.2"), Some(Target::Search { .. })));
        // Nor is one with a dash or an underscore in it.
        assert!(matches!(decide_nofs("foo.c-om"), Some(Target::Search { .. })));
        assert!(matches!(decide_nofs("foo.c_om"), Some(Target::Search { .. })));
    }

    #[test]
    fn spaces_make_it_a_search_unless_a_scheme_says_otherwise() {
        assert!(matches!(decide_nofs("what is a url"), Some(Target::Search { .. })));
        assert!(matches!(decide_nofs("example.com and more"), Some(Target::Search { .. })));
        // A scheme plus a space is still a URL, because the host is unambiguous. qutebrowser
        // reaches this through its naive check, not through the explicit-scheme branch.
        assert_eq!(url_of("http://example.com/a b"), "http://example.com/a b");
        // A space in the user name is not.
        assert!(matches!(decide_nofs("foo bar@example.com"), Some(Target::Search { .. })));
        // ...but a user name without one is fine.
        assert_eq!(url_of("user@example.com"), "http://user@example.com");
    }

    #[test]
    fn nothing_to_open() {
        assert_eq!(decide_nofs(""), None);
        assert_eq!(decide_nofs("   "), None);
        // Whitespace around a real input is trimmed, not searched for.
        assert_eq!(url_of("  example.com  "), "http://example.com");
    }

    // -- search engines -------------------------------------------------------------------------

    #[test]
    fn the_first_word_picks_the_engine_only_when_it_is_one() {
        assert_eq!(
            decide_nofs("gh rust cef"),
            Some(Target::Search {
                engine: "gh".to_string(),
                term: "rust cef".to_string(),
                url: "https://github.com/search?q=rust%20cef".to_string(),
            })
        );
        // `nosuchengine` is not an engine, so it is part of the term.
        assert_eq!(
            decide_nofs("nosuchengine rust cef"),
            Some(Target::Search {
                engine: "DEFAULT".to_string(),
                term: "nosuchengine rust cef".to_string(),
                url: "https://duckduckgo.com/?q=nosuchengine%20rust%20cef".to_string(),
            })
        );
        // An engine name on its own is a search for that word — url.open_base_url is false, as in
        // qutebrowser.
        assert_eq!(
            decide_nofs("ddg"),
            Some(Target::Search {
                engine: "DEFAULT".to_string(),
                term: "ddg".to_string(),
                url: "https://duckduckgo.com/?q=ddg".to_string(),
            })
        );
    }

    #[test]
    fn a_config_with_no_default_engine_does_not_panic() {
        // Not reachable through bru.search, which cannot delete an engine — but a decision that
        // panicked on a missing DEFAULT would be a browser that dies on a typo. qutebrowser makes
        // DEFAULT a required key (configdata.yml :2594) so it never meets this at all.
        let empty = SearchEngines { engines: BTreeMap::new() };
        // A word that is not a URL: with nothing to search with, it is opened as an address —
        // urlutils.py :262 does the same when the engine lookup fails.
        assert_eq!(
            decide_with("github", &empty, |_| false),
            Some(Target::Url("http://github".to_string()))
        );
        // And when it is not even that, there is nothing to open and `open` says so.
        assert_eq!(decide_with("hello there", &empty, |_| false), None);
        // A real URL never reaches the search at all, so the empty table is irrelevant to it.
        assert_eq!(
            decide_with("example.com", &empty, |_| false),
            Some(Target::Url("http://example.com".to_string()))
        );
    }

    #[test]
    fn placeholders_quote_the_way_qutebrowser_documents() {
        let term = "slash/and&amp";
        // {} and {semiquoted}: everything but the slash.
        assert_eq!(format_template("{}", term).unwrap(), "slash/and%26amp");
        assert_eq!(format_template("{semiquoted}", term).unwrap(), "slash/and%26amp");
        // {quoted}: the slash too.
        assert_eq!(format_template("{quoted}", term).unwrap(), "slash%2Fand%26amp");
        // {unquoted}: nothing.
        assert_eq!(format_template("{unquoted}", term).unwrap(), "slash/and&amp");
        // Doubled braces are literal.
        assert_eq!(format_template("a{{}}b{}", "x").unwrap(), "a{}bx");
        // Non-ASCII goes out as UTF-8 bytes.
        assert_eq!(format_template("{}", "щ").unwrap(), "%D1%89");
        // A typo in a template is an error the config author sees.
        assert!(format_template("{title}", "x").is_err());
        assert!(format_template("{", "x").is_err());
        assert!(format_template("}", "x").is_err());
    }

    #[test]
    fn a_search_engine_table_rejects_what_could_never_be_typed() {
        let mut engines = SearchEngines::default();
        assert!(engines.set("", "https://x/?q={}").is_err());
        assert!(engines.set("two words", "https://x/?q={}").is_err());
        assert!(engines.set("w", "https://x/?q=fixed").is_err(), "no placeholder");
        assert!(engines.set("w", "https://x/?q={title}").is_err(), "unknown placeholder");
        assert!(engines.set("w", "https://x/?q={}").is_ok());
        // DEFAULT is there without being set, and can be replaced.
        assert_eq!(engines.get("DEFAULT"), Some(DEFAULT_ENGINE_URL));
        assert!(engines.set("DEFAULT", "https://search.marginalia.nu/search?query={}").is_ok());
        assert_eq!(
            decide_with("hello", &engines, |_| false),
            Some(Target::Search {
                engine: "DEFAULT".to_string(),
                term: "hello".to_string(),
                url: "https://search.marginalia.nu/search?query=hello".to_string(),
            })
        );
        // Listed in a stable order, for the completion category.
        assert_eq!(
            engines.iter().map(|(name, _)| name).collect::<Vec<_>>(),
            vec!["DEFAULT", "w"]
        );
    }

    // -- the pieces underneath ------------------------------------------------------------------

    #[test]
    fn from_user_input_matches_qts_order() {
        // Prepending http:// is the last resort, not the first.
        assert_eq!(from_user_input("http://x").unwrap().scheme, "http");
        assert_eq!(from_user_input("example.com").unwrap().scheme, "http");
        // ftp is Qt's one guess from the name.
        assert_eq!(from_user_input("ftp.example.com").unwrap().scheme, "ftp");
        assert_eq!(from_user_input("ftp.example.com").unwrap().host, "ftp.example.com");
        // A host with a space in it is not a host, so there is no URL at all.
        assert_eq!(from_user_input("ddg python dict"), None);
        assert_eq!(from_user_input(""), None);
        // The host is lowercased, as Qt does.
        assert_eq!(from_user_input("EXAMPLE.COM").unwrap().host, "example.com");
    }

    #[test]
    fn parsing_the_pieces() {
        let url = Parts::parse("https://user:pw@example.com:8443/a/b?q=1#f");
        assert!(url.valid);
        assert_eq!(url.scheme, "https");
        assert_eq!(url.userinfo, "user:pw");
        assert_eq!(url.user_name(), "user");
        assert_eq!(url.host, "example.com");
        assert_eq!(url.port, Some(8443));
        assert_eq!(url.path, "/a/b");
        assert_eq!(url.tail, "?q=1#f");
        assert_eq!(url.render(), "https://user:pw@example.com:8443/a/b?q=1#f");

        // No authority: the path is everything after the colon.
        let url = Parts::parse("about:blank");
        assert_eq!(url.scheme, "about");
        assert_eq!(url.path, "blank");
        assert!(!url.authority);
        assert_eq!(url.render(), "about:blank");

        // A port that is not a number is not a URL.
        assert!(!Parts::parse("http://example.com:nope/").valid);
        // A host with a space in it is not a URL.
        assert!(!Parts::parse("http://a b.com/").valid);
        // A trailing colon with no port is fine, and Qt drops it.
        let url = Parts::parse("http://example.com:/x");
        assert!(url.valid);
        assert_eq!(url.port, None);
        assert_eq!(url.render(), "http://example.com/x");
    }

    #[test]
    fn the_tld_rule() {
        assert!(has_tld("example.com"));
        assert!(has_tld("a.b.example.museum"));
        assert!(has_tld("example.xn--p1ai"));
        assert!(!has_tld("example"));
        assert!(!has_tld("example."));
        assert!(!has_tld("version.2"));
        assert!(!has_tld("host.c-om"));
        assert!(!has_tld("host.c_om"));
        assert!(!has_tld(""));
    }

    /// A NUL byte in a source file makes `file` report `data` and makes plain `grep -rn` skip that
    /// file **silently** — no warning, no non-zero exit, just no matches. This file carried one
    /// inside a doc comment for weeks and cost two people an afternoon each, both of whom concluded
    /// the code they were looking for did not exist. Written here rather than anywhere else because
    /// this is the file that had it.
    #[test]
    fn no_source_file_carries_a_nul_byte() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut checked = 0;
        for entry in std::fs::read_dir(&src).expect("src/ is readable") {
            let path = entry.expect("a readable directory entry").path();
            if path.extension().is_none_or(|ext| ext != "rs") {
                continue;
            }
            let bytes = std::fs::read(&path).expect("a readable source file");
            assert!(
                !bytes.contains(&0),
                "{} carries a NUL byte, so grep will skip it without saying so",
                path.display()
            );
            checked += 1;
        }
        assert!(checked > 20, "only {checked} source files were checked");
    }
}
