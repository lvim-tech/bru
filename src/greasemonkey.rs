//! Greasemonkey — the user's own scripts, injected into the pages they name.
//!
//! A port of qutebrowser 3.7.0's `browser/greasemonkey.py` and
//! `javascript/greasemonkey_wrapper.js`. The metadata block, the `@match`/`@include`/`@exclude`
//! rules, the `@run-at` moments and the thirteen `GM_*` names are all qutebrowser's, so a script
//! written for qutebrowser finds what it expects here.
//!
//! # Why this is not a privilege escalation
//!
//! The obvious worry about userscripts is a script with `@match *://*/*` running on a bank with
//! powers the page itself does not have. Tampermonkey really does grant those — its
//! `GM_xmlhttpRequest` is a privileged fetch that ignores CORS. qutebrowser's does not, and this is
//! the line that settles it (`javascript/greasemonkey_wrapper.js:67`):
//!
//! ```js
//! const oXhr = new XMLHttpRequest();
//! ```
//!
//! An ordinary `XMLHttpRequest`, in the page's own world. bru copies that exactly. **Every one of
//! the thirteen names is built on something the page could already do**: `localStorage` for the
//! value store, a `<style>` element for `GM_addStyle`, `window.open` for `GM_openInTab`, the page's
//! own `copy` event for `GM_setClipboard`. Two are honest stubs that log to the console rather than
//! lie — `GM_getResourceText` and `GM_registerMenuCommand`, exactly as in qutebrowser.
//!
//! Nothing here reaches bru itself. A userscript sees `window.cefQuery` the way any hostile page
//! does, and `ipc.rs`'s `bru://`-only check refuses it the way it refuses any hostile page.
//!
//! # What bru refuses to do that qutebrowser does
//!
//! - **`@require` is never fetched.** qutebrowser downloads required scripts over the network.
//!   bru never fetches a script or a resource by itself, so a `@require` is resolved out of
//!   `<data>/greasemonkey/requires/` and a script whose requirement is not on disk **is not
//!   loaded**. `:greasemonkey-reload` names the file it wanted.
//! - **`@qute-js-world` other than `main` is refused.** CEF exposes no isolated world to run a
//!   script in, so honouring `user` or `application` is impossible. Running such a script in the
//!   main world anyway would be quietly giving it *less* isolation than it asked for, so the
//!   script is not loaded and the reason is printed.
//! - **A `/regex/` `@include`/`@exclude` whose pattern bru cannot compile is refused, and takes its
//!   script with it.** See [`Rule`] for which patterns those are. Dropping just the pattern would be
//!   worse than refusing the script: a dropped `@exclude /bank/` would *widen* where the script
//!   runs, which is the exact failure this module is written to avoid. The refusal names the
//!   pattern and quotes the engine's own reason.
//!
//! # Where the work happens
//!
//! Scripts are read in the **renderer** process, once per renderer, and injected from
//! `RenderProcessHandler::on_context_created` — the moment the new document's V8 context exists and
//! before any of the page's own scripts have run. That is `@run-at document-start`; the other two
//! moments are reached from inside the wrapper by waiting for `DOMContentLoaded` and `load`, which
//! is what makes them right in subframes and on a document that was already complete.
//!
//! Reading the directory in the renderer rather than shipping the scripts down from the browser
//! process is deliberate: document-start has to be **synchronous** with the context being created,
//! and anything that had to ask the browser process first would arrive after the page's own
//! scripts. `:greasemonkey-reload` drops the renderer's cache with a process message.

use cef::*;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// The wrapper handed to the page, once per script. See the head of that file for its sentinels.
const WRAPPER_JS: &str = include_str!("../chrome/greasemonkey.js");

/// Renderer ← browser: drop the cache, the scripts on disk have changed.
const RELOAD: &str = "bru-gm-reload";
/// Renderer ← browser: evaluate this expression in the page and print what it comes to. `--gm-probe`.
const PROBE: &str = "bru-gm-probe";

// ------------------------------------------------------------------------------------------------
// The metadata block
// ------------------------------------------------------------------------------------------------

/// `@run-at`. qutebrowser defaults anything it does not recognise to `document-end`, and says so on
/// the log; so does [`RunAt::parse`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RunAt {
    Start,
    End,
    Idle,
}

impl RunAt {
    pub fn parse(value: &str) -> Option<RunAt> {
        match value {
            "document-start" => Some(RunAt::Start),
            "document-end" => Some(RunAt::End),
            "document-idle" => Some(RunAt::Idle),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            RunAt::Start => "document-start",
            RunAt::End => "document-end",
            RunAt::Idle => "document-idle",
        }
    }
}

/// One userscript, parsed and ready to wrap. Everything that could be refused has been refused by
/// the time one of these exists — see [`Script::parse`].
#[derive(Clone, Debug)]
pub struct Script {
    pub name: String,
    pub namespace: Option<String>,
    pub description: Option<String>,
    /// The raw `@include` strings. They go into `GM_info` verbatim; [`Script::include_rules`] is
    /// the same list parsed, and is what does the matching.
    pub includes: Vec<String>,
    pub include_rules: Vec<Rule>,
    /// The raw `@match` strings, beside the patterns parsed from them.
    pub match_strings: Vec<String>,
    pub matches: Vec<UrlPattern>,
    pub excludes: Vec<String>,
    pub exclude_rules: Vec<Rule>,
    pub requires: Vec<String>,
    pub grants: Vec<String>,
    /// The declared value, `None` when the script did not say. The *effective* one is
    /// [`Script::effective_run_at`], which is what the wrapper is given.
    pub run_at: Option<String>,
    /// `@noframes` — main frame only.
    pub runs_on_sub_frames: bool,
    /// The metadata block verbatim, for `GM_info.scriptMetaStr`.
    pub script_meta: String,
    /// The script's own source, with any `@require` sources already prepended.
    pub code: String,
}

impl Script {
    /// `namespace/name`, the same string qutebrowser builds for `_qute_script_id`. It is the
    /// localStorage prefix a script's `GM_setValue` writes under, so it must not change lightly.
    pub fn qualified_name(&self) -> String {
        format!("{}/{}", self.namespace.clone().unwrap_or_default(), self.name)
    }

    /// What `:greasemonkey-reload` prints. qutebrowser's `full_name`, minus the dedup counter it
    /// never increments.
    pub fn full_name(&self) -> String {
        match &self.namespace {
            Some(namespace) => format!("GM-{namespace}/{}", self.name),
            None => format!("GM-{}", self.name),
        }
    }

    pub fn effective_run_at(&self) -> RunAt {
        match self.run_at.as_deref() {
            Some(value) => RunAt::parse(value).unwrap_or(RunAt::End),
            None => RunAt::End,
        }
    }

    /// `GM_info`, as JSON. qutebrowser's `_meta_json` plus `namespace`, `grants` and `requires`,
    /// which Tampermonkey also exposes and which cost nothing to fill in honestly.
    fn info_json(&self) -> String {
        format!(
            "{{\"script\":{{\"name\":{},\"namespace\":{},\"description\":{},\"matches\":{},\
             \"includes\":{},\"excludes\":{},\"run-at\":{},\"grants\":{},\"requires\":{}}},\
             \"scriptMetaStr\":{},\"scriptWillUpdate\":false,\"version\":\"0.0.1\",\
             \"scriptHandler\":\"Tampermonkey\"}}",
            json_string(&self.name),
            json_opt(self.namespace.as_deref()),
            json_opt(self.description.as_deref()),
            json_array(&self.match_strings),
            json_array(&self.includes),
            json_array(&self.excludes),
            json_opt(self.run_at.as_deref()),
            json_array(&self.grants),
            json_array(&self.requires),
            json_string(&self.script_meta),
        )
    }

    /// Parse a userscript. `filename` stands in for a missing `@name`, as in qutebrowser.
    ///
    /// `requires` is asked for the source of each `@require`; it returns `Err` with the path it
    /// wanted when the file is not there. Injected rather than read here so the parser stays a pure
    /// function and the tests never touch a disk.
    pub fn parse(
        source: &str,
        filename: &str,
        requires: &mut dyn FnMut(&str) -> Result<String, String>,
    ) -> Result<Script, String> {
        let (meta, _) = split_metadata(source);
        let properties = parse_properties(&meta);

        let mut script = Script {
            name: String::new(),
            namespace: None,
            description: None,
            includes: Vec::new(),
            include_rules: Vec::new(),
            match_strings: Vec::new(),
            matches: Vec::new(),
            excludes: Vec::new(),
            exclude_rules: Vec::new(),
            requires: Vec::new(),
            grants: Vec::new(),
            run_at: None,
            runs_on_sub_frames: true,
            script_meta: meta.clone(),
            code: source.to_string(),
        };
        let mut js_world = "main".to_string();

        for (key, value) in &properties {
            match key.as_str() {
                "name" => script.name = value.clone(),
                "namespace" => script.namespace = Some(value.clone()),
                "description" => script.description = Some(value.clone()),
                "include" => script.includes.push(value.clone()),
                "match" => script.match_strings.push(value.clone()),
                "exclude" | "exclude_match" => script.excludes.push(value.clone()),
                "run-at" => script.run_at = Some(value.clone()),
                "noframes" => script.runs_on_sub_frames = false,
                "require" => script.requires.push(value.clone()),
                "grant" => script.grants.push(value.clone()),
                "qute-js-world" => js_world = value.clone(),
                _ => {}
            }
        }

        if script.name.is_empty() {
            script.name = filename.to_string();
        }

        // CEF has no isolated world to offer. Refused rather than silently run in the main one —
        // see the head of this file.
        if js_world != "main" {
            return Err(format!(
                "asks for @qute-js-world {js_world:?}; bru can only run a script in the page's own \
                 world (`main`), so it was not loaded"
            ));
        }

        // qutebrowser: a script with neither rule runs everywhere.
        if script.includes.is_empty() && script.match_strings.is_empty() {
            script.includes.push("*".to_string());
        }

        // Every pattern is checked now, so that a script that is loaded is a script that matches
        // correctly. A `/regex/` rule bru cannot compile takes the whole script with it — dropping
        // an @exclude would widen where the script runs.
        for pattern in &script.includes {
            script.include_rules.push(Rule::parse(pattern)?);
        }
        for pattern in &script.excludes {
            script.exclude_rules.push(Rule::parse(pattern)?);
        }
        for pattern in &script.match_strings {
            match UrlPattern::parse(pattern) {
                Ok(parsed) => script.matches.push(parsed),
                Err(e) => return Err(format!("has an unparseable @match {pattern:?}: {e}")),
            }
        }

        // `@require`, from disk or not at all. Prepended in reverse so that the first `@require`
        // ends up first in the file, which is qutebrowser's order.
        for url in script.requires.iter().rev() {
            let required = requires(url)?;
            script.code = format!("{}\n{}", indent(&required), script.code);
        }

        Ok(script)
    }

    /// The text handed to a frame: the wrapper with this script's four sentinels filled in.
    pub fn wrapped(&self) -> String {
        let name = self.qualified_name();
        let info = self.info_json();
        // One pass, so that a sentinel appearing inside a substituted value — a userscript that
        // contains the literal `@@BRU_SCRIPT_NAME@@`, say — cannot be substituted again.
        fill(
            WRAPPER_JS,
            &[
                ("/*@@BRU_SOURCE@@*/", self.code.as_str()),
                ("@@BRU_SCRIPT_NAME@@", &crate::ipc::json_escape(&name)),
                ("@@BRU_SCRIPT_INFO@@", &crate::ipc::json_escape(&info)),
                ("@@BRU_RUN_AT@@", self.effective_run_at().name()),
            ],
        )
    }
}

/// Replace every sentinel exactly once per occurrence, in a single left-to-right pass.
fn fill(template: &str, pairs: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len() * 2);
    let mut rest = template;
    'outer: while !rest.is_empty() {
        // The earliest sentinel that starts anywhere in what is left.
        let mut best: Option<(usize, usize, &str)> = None;
        for (index, (sentinel, value)) in pairs.iter().enumerate() {
            if let Some(at) = rest.find(sentinel) {
                if best.map(|(b, _, _)| at < b).unwrap_or(true) {
                    best = Some((at, index, value));
                }
            }
        }
        match best {
            Some((at, index, value)) => {
                out.push_str(&rest[..at]);
                out.push_str(value);
                rest = &rest[at + pairs[index].0.len()..];
            }
            None => {
                out.push_str(rest);
                break 'outer;
            }
        }
    }
    out
}

/// Split a source into its metadata block and the rest.
///
/// qutebrowser splits on the regex `// ==UserScript==|\n+// ==/UserScript==\n`. This does the same
/// line by line: everything between the two markers is the block, and a source with no block gets
/// an empty one.
fn split_metadata(source: &str) -> (String, ()) {
    let mut inside = false;
    let mut meta = String::new();
    for line in source.lines() {
        let trimmed = line.trim();
        if trimmed == "// ==UserScript==" {
            inside = true;
            continue;
        }
        if trimmed == "// ==/UserScript==" {
            break;
        }
        if inside {
            meta.push_str(line);
            meta.push('\n');
        }
    }
    (meta, ())
}

/// qutebrowser's `PROPS_REGEX`, `// @(?P<prop>[^\s]+)\s*(?P<val>.*)`, by hand.
///
/// The value is everything after the run of whitespace that follows the key, to the end of the
/// line, with trailing whitespace kept off — `.*` does not cross a newline and a trailing `\r`
/// from a CRLF file would otherwise become part of a URL.
fn parse_properties(meta: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in meta.lines() {
        let Some(at) = line.find("// @") else {
            continue;
        };
        let rest = &line[at + "// @".len()..];
        let key_len = rest.find(char::is_whitespace).unwrap_or(rest.len());
        if key_len == 0 {
            continue;
        }
        let (key, value) = rest.split_at(key_len);
        out.push((key.to_string(), value.trim().to_string()));
    }
    out
}

/// Four spaces on every line, as `textwrap.indent` does — a required script may carry a metadata
/// block of its own, and an unindented one would be found by the parser as if it were this
/// script's.
fn indent(source: &str) -> String {
    source
        .lines()
        .map(|line| if line.is_empty() { String::new() } else { format!("    {line}") })
        .collect::<Vec<_>>()
        .join("\n")
}

/// One `@include` or `@exclude`, in the form it will be matched in.
///
/// qutebrowser's `GreasemonkeyMatcher._match_pattern` (greasemonkey.py:262-270) is two lines and
/// this is the same two: a pattern that **starts and ends with `/`** is an ECMAScript regular
/// expression, searched case-insensitively (`re.search(pattern[1:-1], url, flags=re.I)`); anything
/// else is an `fnmatch` glob. Note what that first test does *not* say — there is no minimum length,
/// so a bare `/` is the empty regex, which matches every URL. Read as a glob it would match none,
/// and an `@exclude /` that matched nothing would let the script run where it was told not to, so
/// the empty case follows qutebrowser rather than looking sensible.
///
/// # What bru's engine will and will not compile
///
/// The `regex` crate is not JavaScript-compatible. It is a **strict subset**: deliberately
/// linear-time, and therefore with no backreferences and no lookaround, because both are what make
/// a backtracking engine super-linear. Measured on this machine 2026-08-06 against the shapes a
/// real `@include /…/` is written in:
///
/// ```text
/// OK    https?://(www\.)?example\.com/.*
/// OK    ^https?:\/\/(www\.)?example\.com\/
/// OK    https:\/\/[^\/]+\.example\.com\/watch\?v=.*
/// OK    ^https?://(?:[a-z0-9-]+\.)*wikipedia\.org/wiki/[^?]+$
/// OK    ^file:///.*\.html$        \bexample\b       a{2,}      \p{L}+
/// ERR   ^http://(?!secure\.)example\.com     look-around ... is not supported
/// ERR   .../r/(\w+)/comments/\1              backreferences are not supported
/// ```
///
/// The line that mattered most is the second: **`\/` compiles**. A regex written as a JavaScript
/// literal has to escape the delimiter, so `\/` is in almost every real rule, and a crate that
/// rejected unknown escapes would have refused nearly all of them.
///
/// Two divergences from a JavaScript engine that are worth knowing rather than fixing:
///
/// - `\w`, `\d`, `\b` and `.` are **Unicode-aware** here and ASCII-only in a JavaScript regex
///   without `/u`. On a URL, which is percent-encoded ASCII by the time it reaches
///   [`Matcher::url_string`], the two agree.
/// - A pattern outside the subset is **refused by name, with the engine's own message**, and takes
///   its script with it — the same answer this module gives `@qute-js-world user` and a missing
///   `@require`. That is the honest one: running the script with the rule dropped is what would be
///   dangerous, and running it with the rule *approximated* would be worse still.
#[derive(Clone, Debug)]
pub enum Rule {
    Glob(String),
    Regex(regex::Regex),
}

impl Rule {
    pub fn parse(pattern: &str) -> Result<Rule, String> {
        let Some(source) = regex_source(pattern) else {
            return Ok(Rule::Glob(pattern.to_string()));
        };
        // `re.I`, as qutebrowser passes. Nothing else: `.` must not cross a newline and `^`/`$` must
        // mean the ends of the URL, which are this builder's defaults and JavaScript's too.
        match regex::RegexBuilder::new(source).case_insensitive(true).build() {
            Ok(compiled) => Ok(Rule::Regex(compiled)),
            Err(e) => Err(format!(
                "has the regular-expression rule {pattern}, which bru cannot compile: {}. Dropping \
                 the rule could widen where the script runs, so it was not loaded",
                first_line_of(&e.to_string())
            )),
        }
    }

    fn matches(&self, url_string: &str) -> bool {
        match self {
            Rule::Glob(pattern) => glob_match(url_string, pattern),
            // `is_match` is unanchored, which is `re.search` and not `re.match`.
            Rule::Regex(compiled) => compiled.is_match(url_string),
        }
    }
}

/// The inside of a `/…/`, or `None` when the pattern is a glob.
fn regex_source(pattern: &str) -> Option<&str> {
    if !(pattern.starts_with('/') && pattern.ends_with('/')) {
        return None;
    }
    // A lone `/` is `pattern[1:-1]` == `""` in Python, not a slice that runs backwards.
    match pattern.len() < 2 {
        true => Some(""),
        false => Some(&pattern[1..pattern.len() - 1]),
    }
}

/// The regex crate's `Display` is a four-line block with the pattern and a caret rule in it; the
/// error message is the last line. One line is what fits in `Greasemonkey scripts not loaded — …`,
/// and the pattern is already named beside it.
fn first_line_of(message: &str) -> String {
    message
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .next_back()
        .unwrap_or("the pattern is not valid")
        .trim_start_matches("error: ")
        .to_string()
}

// ------------------------------------------------------------------------------------------------
// Matching: `@include`/`@exclude` globs and `@match` URL patterns
// ------------------------------------------------------------------------------------------------

/// Python's `fnmatch` on a URL string: `*` matches anything including `/`, `?` matches one
/// character, `[abc]`/`[a-z]`/`[!abc]` a set. Case-sensitive, which is what `fnmatch` is on POSIX.
pub fn glob_match(text: &str, pattern: &str) -> bool {
    let text: Vec<char> = text.chars().collect();
    let pattern: Vec<char> = pattern.chars().collect();
    glob_at(&text, 0, &pattern, 0)
}

fn glob_at(text: &[char], mut t: usize, pattern: &[char], mut p: usize) -> bool {
    while p < pattern.len() {
        match pattern[p] {
            '*' => {
                // Collapse a run, then try every split. The patterns here are short.
                while p < pattern.len() && pattern[p] == '*' {
                    p += 1;
                }
                if p == pattern.len() {
                    return true;
                }
                for split in t..=text.len() {
                    if glob_at(text, split, pattern, p) {
                        return true;
                    }
                }
                return false;
            }
            '?' => {
                if t >= text.len() {
                    return false;
                }
                t += 1;
                p += 1;
            }
            '[' => {
                let Some((set, negated, next)) = glob_class(pattern, p) else {
                    // An unterminated `[` is a literal one, as in fnmatch.
                    if t >= text.len() || text[t] != '[' {
                        return false;
                    }
                    t += 1;
                    p += 1;
                    continue;
                };
                if t >= text.len() {
                    return false;
                }
                if set.contains(&text[t]) == negated {
                    return false;
                }
                t += 1;
                p = next;
            }
            c => {
                if t >= text.len() || text[t] != c {
                    return false;
                }
                t += 1;
                p += 1;
            }
        }
    }
    t == text.len()
}

/// Read `[...]` starting at `p`. Returns the characters it accepts, whether it was negated, and the
/// index just past the closing bracket.
fn glob_class(pattern: &[char], p: usize) -> Option<(Vec<char>, bool, usize)> {
    let mut i = p + 1;
    let negated = matches!(pattern.get(i), Some('!') | Some('^'));
    if negated {
        i += 1;
    }
    // A `]` immediately after the (optional) negation is a literal `]`.
    let mut set = Vec::new();
    if pattern.get(i) == Some(&']') {
        set.push(']');
        i += 1;
    }
    while i < pattern.len() && pattern[i] != ']' {
        if i + 2 < pattern.len() && pattern[i + 1] == '-' && pattern[i + 2] != ']' {
            let (from, to) = (pattern[i], pattern[i + 2]);
            for c in from..=to {
                set.push(c);
            }
            i += 3;
        } else {
            set.push(pattern[i]);
            i += 1;
        }
    }
    if i >= pattern.len() {
        return None;
    }
    Some((set, negated, i + 1))
}

/// The pieces of a URL this module matches against. Enough of a parser for `scheme://host:port/path`
/// and no more — a full one would be a dependency, and every field here is one `find` away.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Url {
    pub scheme: String,
    pub host: String,
    /// `None` when the URL carried no explicit port, as `QUrl::port()`'s -1 means.
    pub port: Option<u16>,
    pub path: String,
}

impl Url {
    pub fn parse(url: &str) -> Option<Url> {
        let at = url.find(':')?;
        let scheme = url[..at].to_ascii_lowercase();
        if scheme.is_empty() || !scheme.chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c)) {
            return None;
        }
        let rest = &url[at + 1..];
        let (authority, rest) = match rest.strip_prefix("//") {
            Some(rest) => {
                let end = rest
                    .find(['/', '?', '#'])
                    .unwrap_or(rest.len());
                (&rest[..end], &rest[end..])
            }
            None => ("", rest),
        };
        let path_end = rest.find(['?', '#']).unwrap_or(rest.len());
        let path = rest[..path_end].to_string();

        // Strip userinfo, then split the port off. An IPv6 literal keeps its brackets so the colons
        // inside it are not read as a port separator.
        let authority = match authority.rfind('@') {
            Some(at) => &authority[at + 1..],
            None => authority,
        };
        let (host, port) = if let Some(end) = authority.find(']') {
            match authority[end + 1..].strip_prefix(':') {
                Some(port) => (&authority[..=end], port.parse::<u16>().ok()),
                None => (&authority[..=end], None),
            }
        } else {
            match authority.rfind(':') {
                Some(at) => (&authority[..at], authority[at + 1..].parse::<u16>().ok()),
                None => (authority, None),
            }
        };

        Some(Url {
            scheme,
            host: host.to_ascii_lowercase(),
            port,
            path,
        })
    }

    /// The port the pattern will be compared against: the explicit one, or the scheme's default.
    fn effective_port(&self) -> Option<u16> {
        self.port.or(match self.scheme.as_str() {
            "https" => Some(443),
            "http" => Some(80),
            "ftp" => Some(21),
            _ => None,
        })
    }
}

/// A Chromium-style match pattern — `*://*.example.com/*` — ported from qutebrowser's
/// `utils/urlmatch.py`, which is itself a port of Chromium's `URLPattern`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UrlPattern {
    match_all: bool,
    match_subdomains: bool,
    /// `None` matches any scheme.
    scheme: Option<String>,
    /// `None` matches any host.
    host: Option<String>,
    /// `None` matches any path.
    path: Option<String>,
    port: Option<u16>,
}

/// Schemes a pattern may leave hostless.
const SCHEMES_WITHOUT_HOST: [&str; 4] = ["about", "file", "data", "javascript"];

impl UrlPattern {
    pub fn parse(pattern: &str) -> Result<UrlPattern, String> {
        let mut out = UrlPattern {
            match_all: false,
            match_subdomains: false,
            scheme: None,
            host: None,
            path: None,
            port: None,
        };
        if pattern == "<all_urls>" {
            out.match_all = true;
            return Ok(out);
        }
        if pattern.contains('\0') {
            return Err("may not contain a NUL byte".to_string());
        }

        let fixed = fixup_pattern(pattern);
        let at = fixed.find(':').ok_or("missing scheme")?;
        let scheme = fixed[..at].to_ascii_lowercase();
        if scheme.is_empty() {
            return Err("missing scheme".to_string());
        }
        let rest = &fixed[at + 1..];
        let (netloc, rest) = match rest.strip_prefix("//") {
            Some(rest) => {
                let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
                (&rest[..end], &rest[end..])
            }
            None => ("", rest),
        };
        let path_end = rest.find(['?', '#']).unwrap_or(rest.len());
        let path = &rest[..path_end];

        out.scheme = (scheme != "any").then_some(scheme);

        // Host. Only `*` and `*.foo` are legal wildcards, as in Chromium.
        let hostname = match netloc.rfind('@') {
            Some(at) => &netloc[at + 1..],
            None => netloc,
        };
        let hostname = if hostname.starts_with('[') {
            match hostname.find(']') {
                Some(end) => &hostname[..=end],
                None => return Err("unterminated IPv6 host".to_string()),
            }
        } else {
            match hostname.rfind(':') {
                Some(at) => &hostname[..at],
                None => hostname,
            }
        };
        let hostname = hostname.to_ascii_lowercase();
        if hostname.trim().is_empty() {
            if !SCHEMES_WITHOUT_HOST.contains(&out.scheme.as_deref().unwrap_or("any")) {
                return Err("pattern without host".to_string());
            }
        } else if hostname == "*" {
            out.match_subdomains = true;
        } else if let Some(bare) = hostname.strip_prefix("*.") {
            if bare.is_empty() {
                return Err("pattern without host".to_string());
            }
            if bare.contains('*') {
                return Err("invalid host wildcard".to_string());
            }
            out.match_subdomains = true;
            out.host = Some(bare.trim_end_matches('.').to_string());
        } else if hostname.chars().all(|c| c == '.' || c == ' ') {
            return Err("invalid host".to_string());
        } else if hostname.contains('*') {
            return Err("invalid host wildcard".to_string());
        } else {
            out.host = Some(hostname.trim_end_matches('.').to_string());
        }

        // Port. `:*` is "any", a bare `:` is an error, anything else has to be a number.
        if netloc.ends_with(":*") {
            out.port = None;
        } else if netloc.ends_with(':') {
            return Err("invalid port: port is empty".to_string());
        } else if let Some(at) = netloc.rfind(':') {
            if !netloc.starts_with('[') || netloc[at..].starts_with("]:") || netloc.find(']').map(|end| at > end).unwrap_or(true) {
                let digits = &netloc[at + 1..];
                match digits.parse::<u16>() {
                    Ok(port) => out.port = Some(port),
                    Err(e) => return Err(format!("invalid port: {e}")),
                }
            }
        }
        let scheme_has_port = matches!(out.scheme.as_deref(), None | Some("http") | Some("https") | Some("ftp"));
        if out.port.is_some() && !scheme_has_port {
            return Err(format!(
                "ports are unsupported with the {} scheme",
                out.scheme.as_deref().unwrap_or("any")
            ));
        }

        // Path. `/*` and "nothing at all" both mean any path.
        if out.scheme.as_deref() == Some("about") && path.trim().is_empty() {
            return Err("pattern without path".to_string());
        }
        if path != "/*" && !path.is_empty() {
            out.path = Some(path.to_string());
        }

        Ok(out)
    }

    pub fn matches(&self, url: &Url) -> bool {
        if self.match_all {
            return true;
        }
        if let Some(scheme) = &self.scheme {
            if scheme != &url.scheme {
                return false;
            }
        }
        if !self.matches_host(&url.host) {
            return false;
        }
        if self.port.is_some() && self.port != url.effective_port() {
            return false;
        }
        self.matches_path(&url.path)
    }

    fn matches_host(&self, host: &str) -> bool {
        let host = host.trim_end_matches('.');
        let Some(want) = &self.host else {
            return true;
        };
        if host == want {
            return true;
        }
        if !self.match_subdomains {
            return false;
        }
        // Chromium does not do subdomain matching against an IP address, and neither does this.
        if is_ip_address(host) {
            return false;
        }
        if host.len() <= want.len() + 1 {
            return false;
        }
        host.ends_with(want) && host.as_bytes()[host.len() - want.len() - 1] == b'.'
    }

    fn matches_path(&self, path: &str) -> bool {
        let Some(want) = &self.path else {
            return true;
        };
        // Match 'google.com' with 'google.com/'.
        if format!("{path}/*") == *want {
            return true;
        }
        glob_match(path, want)
    }
}

/// qutebrowser's `_fixup_pattern`: make the pattern parseable as a URL.
fn fixup_pattern(pattern: &str) -> String {
    let mut pattern = pattern.to_string();
    if let Some(rest) = pattern.strip_prefix("*:") {
        pattern = format!("any:{rest}");
    }
    let hostless = SCHEMES_WITHOUT_HOST
        .iter()
        .any(|scheme| pattern.starts_with(&format!("{scheme}:")));
    if !pattern.contains("://") && !hostless {
        pattern = format!("any://{pattern}");
    }
    if pattern.starts_with("file://") && !pattern.starts_with("file:///") {
        pattern = format!("file:///{}", &pattern["file://".len()..]);
    }
    pattern
}

/// Enough of an IP test for the subdomain rule: a dotted quad, or anything in brackets.
fn is_ip_address(host: &str) -> bool {
    if host.starts_with('[') {
        return true;
    }
    let parts: Vec<&str> = host.split('.').collect();
    parts.len() == 4
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()) && part.parse::<u16>().map(|n| n < 256).unwrap_or(false))
}

/// The schemes a userscript may run on. qutebrowser's list, and it exists because the alternative —
/// running on `bru://` or `chrome://` — is exactly the escalation this module refuses.
const GREASEABLE_SCHEMES: [&str; 4] = ["http", "https", "ftp", "file"];

/// One URL, and the question "does this script want to run here".
pub struct Matcher {
    url: Url,
    /// The URL as the page reports it, for the `@include`/`@exclude` globs, which match the whole
    /// string rather than its parts.
    url_string: String,
}

impl Matcher {
    /// `None` when nothing may ever run on this URL — a `bru://` chrome page, a `data:` URI, an
    /// unparseable address.
    pub fn new(url: &str) -> Option<Matcher> {
        let parsed = Url::parse(url)?;
        if !GREASEABLE_SCHEMES.contains(&parsed.scheme.as_str()) {
            return None;
        }
        Some(Matcher {
            url: parsed,
            url_string: url.to_string(),
        })
    }

    pub fn matches(&self, script: &Script) -> bool {
        let including = script
            .include_rules
            .iter()
            .any(|rule| rule.matches(&self.url_string));
        let matching = script.matches.iter().any(|pattern| pattern.matches(&self.url));
        let excluding = script
            .exclude_rules
            .iter()
            .any(|rule| rule.matches(&self.url_string));
        (including || matching) && !excluding
    }
}

// ------------------------------------------------------------------------------------------------
// Reading the directory
// ------------------------------------------------------------------------------------------------

/// `~/.local/share/bru/greasemonkey`. bru's own, through `data::data_dir` — nothing here reads
/// qutebrowser's.
///
/// The directory is never created. A browser that makes a directory the user did not ask for is a
/// browser that writes to `~/.local/share` on every start, and `:greasemonkey-reload` names the
/// path when it is missing, which is the same information without the side effect.
pub fn scripts_dir() -> Option<PathBuf> {
    Some(crate::data::data_dir()?.join("greasemonkey"))
}

/// What one pass over the directory came to. Errors are kept rather than printed so that both the
/// command and the tests can say the same thing.
#[derive(Default)]
pub struct LoadResults {
    pub scripts: Vec<Script>,
    pub errors: Vec<(String, String)>,
}

impl LoadResults {
    pub fn successful_str(&self) -> String {
        if self.scripts.is_empty() {
            return "No greasemonkey scripts loaded".to_string();
        }
        let mut names: Vec<String> = self.scripts.iter().map(Script::full_name).collect();
        names.sort();
        format!("Loaded greasemonkey scripts: {}", names.join(", "))
    }

    pub fn error_str(&self) -> Option<String> {
        if self.errors.is_empty() {
            return None;
        }
        let mut lines: Vec<String> = self
            .errors
            .iter()
            .map(|(script, error)| format!("{script}: {error}"))
            .collect();
        lines.sort();
        Some(format!("Greasemonkey scripts not loaded — {}", lines.join("; ")))
    }
}

/// Read every `*.js` in `dir`, in file-name order so that two runs inject in the same order.
pub fn load_from(dir: &Path) -> LoadResults {
    let mut results = LoadResults::default();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return results;
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().map(|e| e == "js").unwrap_or(false))
        .filter(|path| path.is_file())
        .collect();
    paths.sort();

    for path in paths {
        let filename = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        let source = match std::fs::read_to_string(&path) {
            Ok(source) => source,
            Err(e) => {
                results.errors.push((filename, e.to_string()));
                continue;
            }
        };
        // `utf-8-sig` in qutebrowser: a BOM would otherwise land in front of `// ==UserScript==`.
        let source = source.strip_prefix('\u{feff}').unwrap_or(&source).to_string();

        let requires_dir = dir.join("requires");
        let mut resolve = |url: &str| resolve_require(&requires_dir, url);
        match Script::parse(&source, &filename, &mut resolve) {
            Ok(script) => results.scripts.push(script),
            Err(e) => results.errors.push((filename, e)),
        }
    }
    results
}

/// A `@require`, from disk or not at all — **bru never fetches one**.
///
/// Two names are tried, in this order: the URL with `/` replaced by `_`, which is the file
/// qutebrowser's own downloader would have written (`utils.sanitize_filename`, POSIX branch), and
/// the last path segment, which is what a person copying a file in by hand would name it.
fn resolve_require(requires_dir: &Path, url: &str) -> Result<String, String> {
    let sanitized = url.replace('/', "_");
    let basename = url.rsplit('/').next().unwrap_or(url);
    for name in [sanitized.as_str(), basename] {
        if name.is_empty() {
            continue;
        }
        if let Ok(source) = std::fs::read_to_string(requires_dir.join(name)) {
            return Ok(source);
        }
    }
    Err(format!(
        "@require {url} is not on disk and bru never fetches one — put it at {}",
        requires_dir.join(&sanitized).display()
    ))
}

// ------------------------------------------------------------------------------------------------
// The renderer side: the cache, and the injection
// ------------------------------------------------------------------------------------------------

/// The scripts this renderer process has read, or `None` before the first page and after a reload.
fn cache() -> &'static Mutex<Option<Vec<Script>>> {
    static CACHE: Mutex<Option<Vec<Script>>> = Mutex::new(None);
    &CACHE
}

/// The scripts that want to run on `matcher`, wrapped and ready. Split out from the CEF call so a
/// test can ask the same question without a browser.
fn wrapped_for(scripts: &[Script], matcher: &Matcher, is_main_frame: bool) -> Vec<(String, String)> {
    scripts
        .iter()
        .filter(|script| is_main_frame || script.runs_on_sub_frames)
        .filter(|script| matcher.matches(script))
        .map(|script| (script.full_name(), script.wrapped()))
        .collect()
}

/// From `RenderProcessHandler::on_context_created` — the renderer process, and the one injection
/// point.
///
/// This is `@run-at document-start`: the context exists and no script of the page's own has run.
/// The other two moments are waited for inside the wrapper.
pub fn renderer_on_context_created(frame: Option<&Frame>) {
    let Some(frame) = frame else {
        return;
    };
    let url = CefString::from(&frame.url()).to_string();
    // `bru://` chrome, `about:blank`, `data:` — nothing runs there, and the scheme check is what
    // keeps a userscript away from the pages that drive bru.
    let Some(matcher) = Matcher::new(&url) else {
        return;
    };
    let is_main_frame = frame.is_main() != 0;

    let mut guard = cache().lock().expect("greasemonkey cache mutex poisoned");
    if guard.is_none() {
        let scripts = match scripts_dir() {
            Some(dir) => {
                let results = load_from(&dir);
                for (script, error) in &results.errors {
                    eprintln!("bru[greasemonkey]: {script}: {error}");
                }
                results.scripts
            }
            None => Vec::new(),
        };
        log(&format!("read {} script(s) from disk", scripts.len()));
        *guard = Some(scripts);
    }
    let wrapped = wrapped_for(guard.as_deref().unwrap_or(&[]), &matcher, is_main_frame);
    drop(guard);

    for (name, code) in wrapped {
        log(&format!("injecting {name} into {url}"));
        // `V8Context::eval`, not `Frame::execute_java_script`. Measured 2026-08-06: from inside
        // `on_context_created` the latter runs nothing at all — the six scripts below were handed
        // to the frame, the log said so, and the page was untouched a full seven seconds later
        // (`gm-probe: … order:["page-script"]`, banner unchanged). The renderer-side `CefFrameImpl`
        // queues an `ExecuteJavaScript` until the frame is attached to the browser process, which
        // for a context this new has not happened yet. `eval` runs in the context that was just
        // created, synchronously, which is the whole point of injecting here.
        // A `None` here is not "nothing to report" — it is `v8_context()` answering nothing or
        // `enter()` refusing, and the script did not run at all. Logging only the exception left
        // that case silent, which reads on the terminal exactly like a script that ran and chose to
        // do nothing: the `injecting …` line above is printed either way.
        match evaluate(frame, &code) {
            Some(value) if value.starts_with("<exception:") => {
                eprintln!("bru[greasemonkey]: {name} threw on injection: {value}");
            }
            Some(_) => {}
            None => eprintln!(
                "bru[greasemonkey]: {name} did not run — the frame gave up no V8 context to run it in"
            ),
        }
    }
}

/// Renderer: the two process messages this module answers. Returns true when the message was ours.
///
/// Hooked in `app.rs` rather than in `ipc.rs` on purpose — neither of these is a query a page could
/// have started, so neither goes near the message router or its `bru://`-only check.
pub fn renderer_on_message(frame: Option<&Frame>, message: Option<&ProcessMessage>) -> bool {
    let Some(message) = message else {
        return false;
    };
    let name = CefString::from(&message.name()).to_string();
    if name == RELOAD {
        *cache().lock().expect("greasemonkey cache mutex poisoned") = None;
        log("cache dropped; the next page will re-read the directory");
        return true;
    }
    if name == PROBE {
        let Some(frame) = frame else {
            return true;
        };
        let expression = message
            .argument_list()
            .map(|arguments| CefString::from(&arguments.string(0)).to_string())
            .unwrap_or_default();
        let url = CefString::from(&frame.url()).to_string();
        match evaluate(frame, &expression) {
            Some(value) => eprintln!("gm-probe: {url} -> {value}"),
            None => eprintln!("gm-probe: {url} -> <the expression threw or has no value>"),
        }
        return true;
    }
    false
}

/// Run an expression in the frame's own V8 context and bring it back as a string. The same shape as
/// `scroll::evaluate` — `eval` is only legal between `enter` and `exit`.
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
    let text = if ok != 0 {
        value.map(|value| CefString::from(&value.string_value()).to_string())
    } else {
        exception.map(|e| format!("<exception: {}>", CefString::from(&e.message()).to_string()))
    };
    context.exit();
    text
}

// ------------------------------------------------------------------------------------------------
// The browser side: the command, and the two things it sends
// ------------------------------------------------------------------------------------------------

/// `:greasemonkey-reload [--quiet]` — re-read the directory and tell every renderer to do the same.
///
/// The scripts already running in a page stay running; a page has to be reloaded to pick the new
/// ones up, which is qutebrowser's behaviour too.
pub fn reload(quiet: bool) {
    let Some(dir) = scripts_dir() else {
        crate::message::error("greasemonkey: no data directory to read scripts from");
        return;
    };
    if !dir.is_dir() {
        crate::message::info(&format!(
            "No greasemonkey scripts: {} does not exist",
            dir.display()
        ));
        broadcast_reload();
        return;
    }
    let results = load_from(&dir);
    if !quiet {
        crate::message::info(&results.successful_str());
    }
    if let Some(errors) = results.error_str() {
        crate::message::error(&errors);
    }
    eprintln!("bru[greasemonkey]: {}", results.successful_str());
    if let Some(errors) = results.error_str() {
        eprintln!("bru[greasemonkey]: {errors}");
    }
    broadcast_reload();
}

/// Tell every tab's renderer to drop its cache. One message per tab, because a renderer is per site
/// and bru has no list of them — the frames it does have are the ones that reach them.
fn broadcast_reload() {
    send_to_every_tab(RELOAD, None);
}

/// `--gm-probe=<expression> [--gm-probe-after-ms=1000,4000]` — evaluate an expression in the
/// showing tab and print what it comes to, once per delay.
///
/// It is the measuring instrument for this module and it exists for the same reason `--tab-script`
/// does: `wtype` segfaults CEF, so nothing here can be driven by a keystroke, and a claim about
/// what a userscript did to a page needs to be read out of the page rather than asserted.
pub fn schedule_probe(expression: &str, delays: &str) {
    for delay in delays.split(',').filter(|d| !d.is_empty()) {
        let Ok(delay_ms) = delay.trim().parse::<i64>() else {
            eprintln!("gm-probe: {delay:?} is not a number of milliseconds");
            continue;
        };
        let mut task = ProbeStep::new(expression.to_string());
        post_delayed_task(ThreadId::UI, Some(&mut task), delay_ms);
    }
}

wrap_task! {
    struct ProbeStep {
        expression: String,
    }

    impl Task {
        fn execute(&self) {
            debug_assert_ne!(currently_on(ThreadId::UI), 0);
            send_to_every_tab(PROBE, Some(&self.expression));
        }
    }
}

/// Send one process message to the main frame of every tab.
///
/// Every tab rather than the showing one: a background tab is where `@run-at document-idle` is
/// least likely to be right, and a probe that could only see the front tab would never notice.
fn send_to_every_tab(name: &str, argument: Option<&str>) {
    let Some(state) = crate::state::BruState::instance() else {
        return;
    };
    // The views are cloned out and the lock released before any CEF call: `tabs.rs` learned the
    // hard way that a Views call made under this mutex can reach a handler that wants it back.
    let views = {
        let Ok(guard) = state.lock() else {
            return;
        };
        guard.tab_views()
    };
    for view in views {
        let Some(frame) = view.browser().and_then(|browser| browser.main_frame()) else {
            continue;
        };
        let Some(mut message) = process_message_create(Some(&CefString::from(name))) else {
            continue;
        };
        if let (Some(argument), Some(arguments)) = (argument, message.argument_list()) {
            arguments.set_string(0, Some(&CefString::from(argument)));
        }
        frame.send_process_message(ProcessId::RENDERER, Some(&mut message));
    }
}

/// `BRU_DEBUG_GM=1` traces what was read and what was injected where. Off by default: it is one
/// line per script per frame, and a page with a dozen iframes would drown the log.
fn log(message: &str) {
    static ON: OnceLock<bool> = OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("BRU_DEBUG_GM").is_some()) {
        eprintln!("bru[greasemonkey,pid {}]: {message}", std::process::id());
    }
}

// ------------------------------------------------------------------------------------------------
// JSON by hand, the way ipc.rs does it
// ------------------------------------------------------------------------------------------------

fn json_string(value: &str) -> String {
    format!("\"{}\"", crate::ipc::json_escape(value))
}

fn json_opt(value: Option<&str>) -> String {
    match value {
        Some(value) => json_string(value),
        None => "null".to_string(),
    }
}

fn json_array(values: &[String]) -> String {
    let items: Vec<String> = values.iter().map(|value| json_string(value)).collect();
    format!("[{}]", items.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_requires(url: &str) -> Result<String, String> {
        Err(format!("no @require here: {url}"))
    }

    fn parse(source: &str) -> Script {
        Script::parse(source, "test.js", &mut no_requires).expect("script should parse")
    }

    const SIMPLE: &str = "\
// ==UserScript==
// @name         bru test
// @namespace    bru
// @description  turns the page red
// @match        *://example.com/*
// @run-at       document-end
// @grant        none
// ==/UserScript==
document.body.style.background = 'red';
";

    #[test]
    fn the_metadata_block_is_read_the_way_qutebrowser_reads_it() {
        let script = parse(SIMPLE);
        assert_eq!(script.name, "bru test");
        assert_eq!(script.namespace.as_deref(), Some("bru"));
        assert_eq!(script.description.as_deref(), Some("turns the page red"));
        assert_eq!(script.match_strings, vec!["*://example.com/*"]);
        assert_eq!(script.grants, vec!["none"]);
        assert_eq!(script.effective_run_at(), RunAt::End);
        assert!(script.runs_on_sub_frames);
        // No @include and no @match would mean "everywhere"; this one has a @match, so it must not
        // have been given the catch-all as well.
        assert!(script.includes.is_empty());
    }

    #[test]
    fn a_script_with_no_metadata_block_runs_everywhere_and_is_named_after_its_file() {
        let script = parse("console.log('hi');\n");
        assert_eq!(script.name, "test.js");
        assert_eq!(script.includes, vec!["*"]);
    }

    #[test]
    fn an_unknown_run_at_falls_back_to_document_end() {
        let script = parse("// ==UserScript==\n// @run-at nonsense\n// ==/UserScript==\n");
        assert_eq!(script.run_at.as_deref(), Some("nonsense"));
        assert_eq!(script.effective_run_at(), RunAt::End);
    }

    #[test]
    fn noframes_is_read() {
        let script = parse("// ==UserScript==\n// @noframes\n// ==/UserScript==\n");
        assert!(!script.runs_on_sub_frames);
    }

    // -- the three refusals ----------------------------------------------------------------------

    #[test]
    fn a_foreign_js_world_is_refused_rather_than_run_in_the_main_one() {
        let source = "// ==UserScript==\n// @qute-js-world user\n// ==/UserScript==\n";
        let error = Script::parse(source, "x.js", &mut no_requires).unwrap_err();
        assert!(error.contains("qute-js-world"), "{error}");
        assert!(error.contains("not loaded"), "{error}");
    }

    #[test]
    fn a_regex_rule_is_honoured_rather_than_refused() {
        // The dangerous half: dropping this @exclude would make the script run on the bank. It is
        // now *obeyed*, which is stricter than dropping it and than refusing the script.
        let script = parse(
            "// ==UserScript==\n// @include *\n// @exclude /bank/\n// ==/UserScript==\nx();\n",
        );
        assert!(Matcher::new("https://example.com/x").unwrap().matches(&script));
        assert!(!Matcher::new("https://my.bank.example/x").unwrap().matches(&script));
        // `re.I` — qutebrowser passes it and so does bru, so the capitals do not get through.
        assert!(!Matcher::new("https://my.BANK.example/x").unwrap().matches(&script));
        // `re.search`, not `re.match`: the pattern is unanchored and may match anywhere.
        assert!(!Matcher::new("https://x.example/my-BankING/page").unwrap().matches(&script));
        // The raw string is what `GM_info` shows, unchanged.
        assert_eq!(script.excludes, vec!["/bank/"]);
    }

    #[test]
    fn a_regex_include_matches_the_way_a_javascript_literal_would() {
        // Every `\/` in here is what a regex written as a JS literal has to do with the delimiter,
        // and is the shape that would have made a stricter engine useless.
        let script = parse(
            "// ==UserScript==\n\
             // @include /^https?:\\/\\/(www\\.)?example\\.com\\/watch\\?v=/\n\
             // ==/UserScript==\n",
        );
        assert!(Matcher::new("https://example.com/watch?v=1").unwrap().matches(&script));
        assert!(Matcher::new("http://www.example.com/watch?v=1").unwrap().matches(&script));
        // `^` still anchors, so the same path under another host does not match.
        assert!(!Matcher::new("https://evil.test/https://example.com/watch?v=1").unwrap().matches(&script));
        assert!(!Matcher::new("https://example.com/other").unwrap().matches(&script));
    }

    #[test]
    fn a_pattern_outside_the_engines_subset_is_refused_by_name_with_the_reason() {
        for (pattern, wanted) in [
            ("/^http://(?!secure\\.)example\\.com/", "look-around"),
            ("/(\\w+)\\/comments\\/\\1/", "backreferences"),
            ("/[/", "character class"),
        ] {
            let source =
                format!("// ==UserScript==\n// @exclude {pattern}\n// ==/UserScript==\nx();\n");
            let error = Script::parse(&source, "x.js", &mut no_requires).unwrap_err();
            assert!(error.contains(pattern), "the pattern must be named: {error}");
            assert!(error.contains(wanted), "the engine's reason must be quoted: {error}");
            assert!(error.contains("not loaded"), "{error}");
            // One line, because it lands in a one-line `Greasemonkey scripts not loaded — …`.
            assert_eq!(error.lines().count(), 1, "{error}");
        }
    }

    #[test]
    fn a_bare_slash_is_the_empty_regex_and_not_a_glob() {
        // qutebrowser's test is `startswith('/') and endswith('/')` with no length rule, so `"/"`
        // is `re.search("", url)` — true for every URL. As a glob it would match none, and an
        // `@exclude /` that matched nothing would let the script run where it was told not to.
        assert!(matches!(Rule::parse("/").unwrap(), Rule::Regex(_)));
        assert!(Rule::parse("/").unwrap().matches("https://example.com/"));
        // Two characters is a `/…/` with an empty inside, which is the same thing.
        assert!(matches!(Rule::parse("//").unwrap(), Rule::Regex(_)));
        // And nothing that merely *contains* a slash is a regex.
        assert!(matches!(Rule::parse("https://example.com/*").unwrap(), Rule::Glob(_)));
        assert!(matches!(Rule::parse("/only-at-the-front").unwrap(), Rule::Glob(_)));
    }

    #[test]
    fn a_require_that_is_not_on_disk_refuses_the_script_and_never_reaches_the_network() {
        let source = "// ==UserScript==\n// @require https://code.jquery.com/jquery.js\n\
                      // ==/UserScript==\n";
        let error = Script::parse(source, "x.js", &mut no_requires).unwrap_err();
        assert!(error.contains("no @require here"), "{error}");
    }

    #[test]
    fn a_require_that_is_on_disk_is_prepended_in_declaration_order() {
        let source = "// ==UserScript==\n// @require a.js\n// @require b.js\n// ==/UserScript==\nmain();\n";
        let script = Script::parse(source, "x.js", &mut |url| Ok(format!("/* {url} */")))
            .expect("both requires resolve");
        let a = script.code.find("/* a.js */").expect("a.js is in the code");
        let b = script.code.find("/* b.js */").expect("b.js is in the code");
        let main = script.code.find("main();").expect("the body is in the code");
        assert!(a < b && b < main, "requires come first, in declaration order");
        // Indented, so a metadata block inside a required script is not read as this script's.
        assert!(script.code.contains("    /* a.js */"));
    }

    // -- matching --------------------------------------------------------------------------------

    #[test]
    fn globs_behave_like_fnmatch() {
        assert!(glob_match("https://example.com/a/b", "https://example.com/*"));
        assert!(glob_match("https://example.com/a/b", "*example*"));
        assert!(!glob_match("https://other.com/a", "https://example.com/*"));
        assert!(glob_match("abc", "a?c"));
        assert!(!glob_match("ac", "a?c"));
        assert!(glob_match("abc", "a[bd]c"));
        assert!(!glob_match("abc", "a[!bd]c"));
        assert!(glob_match("a-c", "a[a-z-]c"));
        // `*` crosses a slash, which is what makes `*://*/*` mean everything.
        assert!(glob_match("https://example.com/deep/path", "*"));
    }

    fn url(s: &str) -> Url {
        Url::parse(s).expect("url should parse")
    }

    #[test]
    fn a_url_is_split_into_the_four_parts_matching_needs() {
        let u = url("https://user:pw@example.com:8443/a/b?q=1#frag");
        assert_eq!(u.scheme, "https");
        assert_eq!(u.host, "example.com");
        assert_eq!(u.port, Some(8443));
        assert_eq!(u.path, "/a/b");

        let f = url("file:///home/x/page.html");
        assert_eq!(f.scheme, "file");
        assert_eq!(f.host, "");
        assert_eq!(f.path, "/home/x/page.html");

        let bare = url("https://example.com");
        assert_eq!(bare.path, "");
        assert_eq!(bare.effective_port(), Some(443));
    }

    #[test]
    fn match_patterns_behave_like_chromiums() {
        let all = UrlPattern::parse("*://*/*").expect("parses");
        assert!(all.matches(&url("https://anything.example/x")));
        assert!(all.matches(&url("http://localhost:8000/")));

        let host = UrlPattern::parse("https://example.com/*").expect("parses");
        assert!(host.matches(&url("https://example.com/a")));
        assert!(!host.matches(&url("https://sub.example.com/a")));
        assert!(!host.matches(&url("http://example.com/a")), "scheme is checked");

        let subdomains = UrlPattern::parse("*://*.example.com/*").expect("parses");
        assert!(subdomains.matches(&url("https://sub.example.com/a")));
        assert!(subdomains.matches(&url("https://example.com/a")), "the bare host too");
        assert!(!subdomains.matches(&url("https://notexample.com/a")));

        let path = UrlPattern::parse("https://example.com/only/*").expect("parses");
        assert!(path.matches(&url("https://example.com/only/this")));
        assert!(!path.matches(&url("https://example.com/other")));

        let port = UrlPattern::parse("http://example.com:8080/*").expect("parses");
        assert!(port.matches(&url("http://example.com:8080/x")));
        assert!(!port.matches(&url("http://example.com/x")), "80 is not 8080");

        assert!(UrlPattern::parse("<all_urls>").expect("parses").matches(&url("file:///x")));
    }

    #[test]
    fn a_pattern_without_a_host_is_an_error_unless_its_scheme_allows_one() {
        assert!(UrlPattern::parse("https:///x").is_err());
        assert!(UrlPattern::parse("file:///home/*").is_ok());
    }

    #[test]
    fn only_the_four_greaseable_schemes_can_be_matched_at_all() {
        assert!(Matcher::new("https://example.com/").is_some());
        assert!(Matcher::new("file:///home/x.html").is_some());
        // The two that matter: bru's own chrome, and a URI a page could navigate itself to.
        assert!(Matcher::new("bru://chrome/bottom.html").is_none());
        assert!(Matcher::new("data:text/html,<b>x</b>").is_none());
        assert!(Matcher::new("chrome://newtab/").is_none());
    }

    #[test]
    fn include_match_and_exclude_combine_the_way_qutebrowser_combines_them() {
        let script = parse(
            "// ==UserScript==\n// @include https://example.com/*\n\
             // @exclude https://example.com/secret*\n// ==/UserScript==\n",
        );
        assert!(Matcher::new("https://example.com/a").unwrap().matches(&script));
        assert!(!Matcher::new("https://example.com/secret/x").unwrap().matches(&script));
        assert!(!Matcher::new("https://other.com/a").unwrap().matches(&script));
    }

    #[test]
    fn a_match_rule_is_enough_on_its_own() {
        let script = parse(SIMPLE);
        assert!(Matcher::new("https://example.com/page").unwrap().matches(&script));
        assert!(!Matcher::new("https://example.org/page").unwrap().matches(&script));
    }

    // -- what reaches the page -------------------------------------------------------------------

    #[test]
    fn the_wrapper_carries_the_source_the_name_the_run_at_and_the_info() {
        let script = parse(SIMPLE);
        let wrapped = script.wrapped();
        assert!(wrapped.contains("document.body.style.background = 'red';"));
        assert!(wrapped.contains("const bru_gm_name = \"bru/bru test\";"));
        assert!(wrapped.contains("const bru_gm_run_at = \"document-end\";"));
        // GM_info arrives as a JSON string that the page parses, so the metadata cannot become
        // executable no matter what is in it.
        assert!(wrapped.contains("JSON.parse(\""));
        assert!(wrapped.contains("turns the page red"));
        // Every sentinel is gone.
        assert!(!wrapped.contains("@@BRU_"), "a sentinel survived substitution");
    }

    #[test]
    fn the_thirteen_names_are_all_defined_and_none_of_them_is_privileged() {
        let wrapped = parse(SIMPLE).wrapped();
        for name in [
            "function GM_xmlhttpRequest",
            "function GM_setValue",
            "function GM_getValue",
            "function GM_deleteValue",
            "function GM_listValues",
            "function GM_addStyle",
            "function GM_openInTab",
            "function GM_setClipboard",
            "function GM_registerMenuCommand",
            "function GM_getResourceText",
            "function GM_log",
            "const GM_info",
            "GM.info = GM_info",
        ] {
            assert!(wrapped.contains(name), "{name} is missing from the wrapper");
        }
        // The line the whole security argument rests on: an ordinary XHR in the page's world.
        assert!(wrapped.contains("const oXhr = new XMLHttpRequest();"));
        // And nothing that would be a way around it.
        assert!(!wrapped.contains("cefQuery"));
        assert!(!wrapped.contains("__bru_hints"));
        assert!(!wrapped.contains("__bru_caret"));
    }

    /// The bug this test exists for cost an hour on 2026-08-06. Every sentinel was also *named* in
    /// the wrapper's header comment, so `fill` substituted the header copy as well; the userscript
    /// source landed in the middle of a `//` line and the generated file died with
    /// `SyntaxError: Unexpected identifier 'userscript'`. `node --check chrome/greasemonkey.js`
    /// passed the whole time — the template was valid, only what was *made* from it was not — and
    /// the symptom was six scripts that were injected, logged, and did nothing.
    #[test]
    fn every_sentinel_appears_exactly_once_in_the_template() {
        for sentinel in [
            "@@BRU_SCRIPT_NAME@@",
            "@@BRU_SCRIPT_INFO@@",
            "@@BRU_RUN_AT@@",
            "@@BRU_SOURCE@@",
        ] {
            assert_eq!(
                WRAPPER_JS.matches(sentinel).count(),
                1,
                "{sentinel} must appear exactly once in chrome/greasemonkey.js — a second copy, \
                 even inside a comment, is substituted too and breaks the generated script"
            );
        }
    }

    #[test]
    fn a_source_containing_a_sentinel_is_not_substituted_twice() {
        let script = parse("// ==UserScript==\n// @name s\n// ==/UserScript==\nvar x = '@@BRU_SCRIPT_NAME@@';\n");
        let wrapped = script.wrapped();
        // The source's copy survives verbatim; only the template's own sentinel was replaced.
        assert!(wrapped.contains("var x = '@@BRU_SCRIPT_NAME@@';"));
        assert!(wrapped.contains("const bru_gm_name = \"/s\";"));
    }

    #[test]
    fn a_name_with_a_quote_in_it_cannot_break_out_of_the_wrapper() {
        let script = parse("// ==UserScript==\n// @name a\" + evil() + \"b\n// ==/UserScript==\n");
        let wrapped = script.wrapped();
        assert!(!wrapped.contains("const bru_gm_name = \"a\" + evil()"));
        assert!(wrapped.contains(r#"const bru_gm_name = "/a\" + evil() + \"b";"#));
    }

    // -- the frame filter ------------------------------------------------------------------------

    #[test]
    fn noframes_keeps_a_script_out_of_subframes_and_nothing_else_does() {
        let framed = parse("// ==UserScript==\n// @name f\n// @noframes\n// ==/UserScript==\n");
        let anywhere = parse("// ==UserScript==\n// @name a\n// ==/UserScript==\n");
        let scripts = vec![framed, anywhere];
        let matcher = Matcher::new("https://example.com/").expect("greaseable");

        let main: Vec<String> = wrapped_for(&scripts, &matcher, true).into_iter().map(|(n, _)| n).collect();
        assert_eq!(main, vec!["GM-f".to_string(), "GM-a".to_string()]);

        let sub: Vec<String> = wrapped_for(&scripts, &matcher, false).into_iter().map(|(n, _)| n).collect();
        assert_eq!(sub, vec!["GM-a".to_string()]);
    }

    // -- reading a directory ---------------------------------------------------------------------

    #[test]
    fn the_scripts_directory_is_brus_own_under_xdg_data_home() {
        // Scratch, never the user's: this asserts about a path and must not create one.
        let dir = scripts_dir().expect("HOME is set in any environment this runs in");
        assert!(dir.ends_with("bru/greasemonkey"), "{}", dir.display());
    }

    #[test]
    fn a_directory_that_is_not_there_is_no_scripts_rather_than_an_error() {
        let results = load_from(Path::new("/nonexistent/bru/greasemonkey"));
        assert!(results.scripts.is_empty());
        assert!(results.errors.is_empty());
        assert_eq!(results.successful_str(), "No greasemonkey scripts loaded");
    }
}
