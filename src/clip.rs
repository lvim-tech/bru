//! The clipboard: `yank` in its five spellings, and the `{clipboard}` / `{primary}` substitutions
//! that `pp`, `pP`, `Pp`, `PP`, `wp` and `wP` open with.
//!
//! **The mechanism is `wl-copy` / `wl-paste`, DECISIONS.md item 5, decided 2026-08-06.** No crate:
//! `wl-copy` forks a daemon that goes on serving the selection after bru exits, and an in-process
//! implementation cannot, because on Wayland the selection is served by the process that owns it.
//! A yanked URL that dies with the browser is not a yanked URL.
//!
//! Measured on this machine 2026-08-06: **ten `wl-copy` runs in 12 ms wall clock (1.2 ms each), ten
//! `wl-paste` runs in 24 ms (2.4 ms each)**. Both therefore run **on the CEF UI thread**, inline in
//! the dispatcher, on the same turn as the key that asked for them. That is deliberate: it keeps
//! the yanked text and the status message in the same event, and 1.2 ms on a keystroke typed by
//! hand is not a budget worth an extra thread. It is *not* the scroll path — nothing here is ever
//! reached by `j`. CEF-NOTES trap 12 does not apply either: spawning a process creates no browser
//! and starts no navigation, so this is safe even from inside a message-router query handler.
//!
//! `wl-copy -n` is not optional. Measured: `wl-copy "abc"` and `printf abc | wl-copy` both store
//! `abc\n` — a yanked URL with a newline on the end is a line a terminal will *run* when it is
//! pasted. `-n` stores the bytes and nothing else, verified by reading `wl-paste --no-newline`
//! back through `xxd`.
//!
//! # The two URL spellings, and why they differ
//!
//! `yy` yanks `{url:yank}` and `yp` yanks `pretty-url`; qutebrowser builds both in
//! `urlutils.get_url_yank_text` (:688) out of `QUrl::toString` flags:
//!
//! | | flags |
//! |---|---|
//! | `yank` | `FullyEncoded \| RemovePassword` |
//! | `pretty-url` | `DecodeReserved \| RemovePassword` |
//!
//! bru has no Qt. Rather than recall what those flags do, the rules below were **measured against
//! Qt 6.11.1 itself** — the same Qt qutebrowser 3.7.0 runs on here — with a C++ program that calls
//! `toString` with each flag for every byte 0-127 and for a set of real URLs. What came out:
//!
//! - A percent escape of an **unreserved** byte (`A-Za-z0-9-._~`) decodes in both forms.
//! - A percent escape of a **delimiter** (`!#$%&'()*+,/:;=?@[]`) or a control byte stays encoded in
//!   both. This is why `100%25` is still `100%25` in a pretty URL, and `a%2Fb` still `a%2Fb`.
//! - Everything else — **space, and every non-ASCII byte** — is the difference: encoded in `yank`,
//!   decoded in `pretty-url`. That is the whole of it, and it is why this user's Cyrillic is the
//!   test that matters:
//!
//!   ```text
//!   yank  : https://example.com/%D0%BF%D1%8A%D1%82%20%D0%BA%D1%8A%D0%BC/100%25
//!   pretty: https://example.com/път към/100%25
//!   ```
//!
//! - A **literal** space, `"`, `<`, `>`, `\`, `^`, `` ` ``, `{`, `|`, `}` or `%` is encoded by
//!   `yank`; every other literal ASCII character is left alone.
//! - A component holding an **invalid** escape (`%zz`, `%2`, a bare `%`) has every one of its `%`
//!   treated as a literal, and no escape in it is honoured at all: measured, Qt turns
//!   `x%zz%D0%BFy` into `x%25zz%25D0%25BFy` in *both* forms.
//! - `mailto:` loses its scheme, a password is dropped from the authority, and the query parameters
//!   in [`IGNORED_QUERY_PARAMETERS`] are removed — `configdata.yml:2631`. A query that ends up
//!   empty takes its `?` with it.
//!
//! Nothing here re-encodes the authority beyond the password: the address bru holds comes from
//! Chromium's `GetURL()`, which has already punycoded an IDN host, and decoding it back would be
//! bru's own idea rather than qutebrowser's.
//!
//! # Where `{clipboard}` is expanded
//!
//! In [`expand`], called from **`exec.rs`'s `Command::Open` arm** — at *run* time, not at parse
//! time. qutebrowser substitutes in `commands/runners.py::replace_variables`, between parsing a
//! command line and running it, so the selection is read at the moment the command runs. bru parses
//! its bindings **once, at startup**, so parse time is the wrong place by a whole session: `pp`
//! would open whatever was on the clipboard when bru launched. The dispatcher is the first place
//! that runs per keypress, so that is where it goes.
//!
//! `cmdline.rs` has its own `replace_variables` for `{url}` and `{url:pretty}`, which is a
//! different job — it fills the command *line* in for `go` and `gO`. No default binding puts
//! `{clipboard}` in a `cmd-set-text`, so the two do not need to meet.

use cef::*;
use std::io::Write;
use std::process::{Command as Process, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::commands::YankWhat;

/// `url.yank_ignored_parameters` (configdata.yml:2631) — tracking parameters a yanked URL drops.
pub const IGNORED_QUERY_PARAMETERS: [&str; 7] = [
    "ref",
    "utm_source",
    "utm_medium",
    "utm_campaign",
    "utm_term",
    "utm_content",
    "utm_name",
];

/// How long a yank's confirmation stays on the status line. qutebrowser's `messages.timeout`.
const MESSAGE_TIMEOUT_MS: i64 = 2000;

/// Which of the two selections a command means.
///
/// `-s` on `yank` is the **primary selection**, not the clipboard — `yY`, `yT`, `yD`, `yP` and `yM`
/// are all the `-s` halves of their pairs. On the paste side the same distinction is spelled by the
/// substitution: `{clipboard}` for `pp`, `{primary}` for `pP`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Selection {
    Clipboard,
    Primary,
}

impl Selection {
    /// `-s` / `--sel` chooses the primary selection.
    pub fn from_sel_flag(sel: bool) -> Selection {
        if sel {
            Selection::Primary
        } else {
            Selection::Clipboard
        }
    }

    /// How qutebrowser names it in the message it prints.
    pub fn name(self) -> &'static str {
        match self {
            Selection::Clipboard => "clipboard",
            Selection::Primary => "primary selection",
        }
    }
}

// -----------------------------------------------------------------------------------------------
// The two processes
// -----------------------------------------------------------------------------------------------

/// Put `text` on a selection, through `wl-copy`.
///
/// The text goes on **stdin**, not in argv: a URL can be any length and can start with `-`, and
/// stdin has neither problem. `-n` keeps the trailing newline off — see the module comment.
pub fn set(selection: Selection, text: &str) -> Result<(), String> {
    let mut command = Process::new("wl-copy");
    command.arg("-n").arg("--type").arg("text/plain;charset=utf-8");
    if selection == Selection::Primary {
        command.arg("--primary");
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("wl-copy could not be run: {error}"))?;

    // Taken rather than borrowed: the pipe has to be closed before `wait`, or wl-copy blocks
    // reading a stdin that is still open and bru blocks waiting for it.
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(text.as_bytes())
            .map_err(|error| format!("wl-copy would not take the text: {error}"))?;
    }

    let status = child
        .wait()
        .map_err(|error| format!("wl-copy did not finish: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("wl-copy failed ({status})"))
    }
}

/// Read a selection, through `wl-paste`.
///
/// `--no-newline` drops the one trailing newline the protocol's text targets conventionally carry;
/// everything else arrives verbatim, and trimming the rest is `open.rs`'s business, which already
/// does it the way `urlutils.fuzzy_url` does.
pub fn get(selection: Selection) -> Result<String, String> {
    let mut command = Process::new("wl-paste");
    command.arg("--no-newline");
    if selection == Selection::Primary {
        command.arg("--primary");
    }
    let output = command
        .stderr(Stdio::null())
        .output()
        .map_err(|error| format!("wl-paste could not be run: {error}"))?;

    // Measured: an unowned selection is not an error worth a stack trace — wl-paste prints
    // "Nothing is copied" and exits 1. qutebrowser says the same thing in its own words.
    if !output.status.success() {
        return Err(format!(
            "{} is empty.",
            capitalise(selection.name())
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    if text.trim().is_empty() {
        return Err(format!("{} is empty.", capitalise(selection.name())));
    }
    Ok(text)
}

fn capitalise(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

// -----------------------------------------------------------------------------------------------
// `yank`
// -----------------------------------------------------------------------------------------------

/// `yank [what] [-s]`, from the dispatcher.
///
/// The URL and the title are the showing tab's, read out of the status-line state the display
/// handler keeps — the same source `cmdline.rs` uses for `{url}`, so `yy` and `go` can never
/// disagree about what page is open.
pub fn yank(what: &YankWhat, sel: bool) {
    let selection = Selection::from_sel_flag(sel);
    let url = crate::ipc::current_url();
    let title = crate::ipc::current_title();

    let (text, what_name) = match what {
        // qutebrowser calls both of these "URL" in the message it prints (commands.py:743).
        YankWhat::Url => (yank_text(&url, false), "URL"),
        YankWhat::PrettyUrl => (yank_text(&url, true), "URL"),
        YankWhat::Title => (title.clone(), "title"),
        YankWhat::Domain => (domain(&url), "domain"),
        YankWhat::Inline(template) => (inline(template, &url, &title), "inline block"),
        // Caret mode's `y` / `Y` / `<Return>`. The text belongs to the caret session, which is the
        // only thing that knows what is selected; outside caret mode there is none, and the empty
        // string below reports "Nothing to yank" rather than yanking the page's URL by accident.
        YankWhat::Selection => (
            crate::caret::selection().map(|(_, text)| text).unwrap_or_default(),
            "selection",
        ),
    };

    if text.is_empty() {
        message("Nothing to yank".to_string());
        return;
    }

    match set(selection, &text) {
        Ok(()) => message(format!(
            "Yanked {what_name} to {}: {text}",
            selection.name()
        )),
        Err(error) => message(error),
    }
}

/// `[{title}]({url:yank})` — the `ym`/`yM` template, expanded the way `replace_variables` does.
///
/// The three variables the two default bindings can name are all that is here. `{url:yank}` is the
/// encoded spelling, which is the point of the binding: a Markdown link whose target has a literal
/// space in it is not a link.
pub fn inline(template: &str, url: &str, title: &str) -> String {
    if !template.contains('{') {
        return template.to_string();
    }
    template
        .replace("{url:yank}", &yank_text(url, false))
        .replace("{url:pretty}", &yank_text(url, true))
        .replace("{url}", url)
        .replace("{title}", title)
}

// -----------------------------------------------------------------------------------------------
// `{clipboard}` and `{primary}`
// -----------------------------------------------------------------------------------------------

/// Expand `{clipboard}` and `{primary}` in a command argument.
///
/// `Ok(None)` for a command with no argument at all — `open -t` with nothing after it. `Err` when a
/// named selection is empty, and then the caller must **not** open anything: qutebrowser raises
/// `ClipboardError` here, which aborts the command, and opening the literal string `{clipboard}` as
/// a search for it would be worse than doing nothing.
pub fn expand(arg: Option<&str>) -> Result<Option<String>, String> {
    let Some(arg) = arg else {
        return Ok(None);
    };
    expand_with(arg, get).map(Some)
}

/// [`expand`] with the reading handed in, so the substitution can be tested without a compositor.
///
/// Each selection is read **at most once**, as `replace_variables` caches them, and the result is
/// never rescanned — a clipboard holding the literal text `{primary}` expands to itself and stops.
pub fn expand_with(
    arg: &str,
    mut read: impl FnMut(Selection) -> Result<String, String>,
) -> Result<String, String> {
    if !arg.contains('{') {
        return Ok(arg.to_string());
    }

    let mut out = String::with_capacity(arg.len());
    let mut rest = arg;
    let mut cached: [Option<String>; 2] = [None, None];

    while let Some(at) = rest.find('{') {
        let (before, from) = rest.split_at(at);
        out.push_str(before);
        let (selection, len) = if from.starts_with("{clipboard}") {
            (Selection::Clipboard, "{clipboard}".len())
        } else if from.starts_with("{primary}") {
            (Selection::Primary, "{primary}".len())
        } else {
            // Not a variable this module knows. Copy the brace and carry on, so `{url}` — which is
            // cmdline.rs's — survives untouched.
            out.push('{');
            rest = &from[1..];
            continue;
        };
        let slot = match selection {
            Selection::Clipboard => 0,
            Selection::Primary => 1,
        };
        if cached[slot].is_none() {
            cached[slot] = Some(read(selection)?);
        }
        out.push_str(cached[slot].as_deref().unwrap_or_default());
        rest = &from[len..];
    }
    out.push_str(rest);
    Ok(out)
}

// -----------------------------------------------------------------------------------------------
// The URL spellings
// -----------------------------------------------------------------------------------------------

/// `urlutils.get_url_yank_text(url, pretty=…)` — see the module comment for what was measured.
pub fn yank_text(url: &str, pretty: bool) -> String {
    let Some(parts) = Parts::split(url) else {
        return url.to_string();
    };

    let mut out = String::with_capacity(url.len());
    // `mailto:` loses its scheme: `mailto:a@b` yanks as `a@b`, which is what goes in an address
    // field. Every other scheme is kept.
    if !parts.scheme.eq_ignore_ascii_case("mailto") {
        out.push_str(parts.scheme);
        out.push(':');
    }
    if let Some(authority) = parts.authority {
        out.push_str("//");
        out.push_str(&without_password(authority));
    }
    out.push_str(&recode(parts.path, pretty));
    if let Some(query) = parts.query {
        let query = strip_ignored_parameters(query);
        if !query.is_empty() {
            out.push('?');
            out.push_str(&recode(&query, pretty));
        }
    }
    if let Some(fragment) = parts.fragment {
        out.push('#');
        out.push_str(&recode(fragment, pretty));
    }
    out
}

/// `yank domain` — `commands.py:735`: scheme, host, and the port when the URL states one.
pub fn domain(url: &str) -> String {
    let Some(parts) = Parts::split(url) else {
        return String::new();
    };
    let authority = parts.authority.map(without_password).unwrap_or_default();
    // Everything after the last `@` is the host and port; a userinfo may itself contain one.
    let host_port = match authority.rsplit_once('@') {
        Some((_, host_port)) => host_port.to_string(),
        None => authority,
    };
    format!("{}://{}", parts.scheme, host_port)
}

/// A URL split into the pieces the two spellings recode separately.
///
/// Not a URL parser for general use — the same rule `open.rs` states about its own. It is exactly
/// as much of RFC 3986 as `get_url_yank_text` consults.
struct Parts<'a> {
    scheme: &'a str,
    /// `Some` only for a URL with `//` after the scheme; `mailto:a@b` has none.
    authority: Option<&'a str>,
    path: &'a str,
    query: Option<&'a str>,
    fragment: Option<&'a str>,
}

impl<'a> Parts<'a> {
    fn split(url: &'a str) -> Option<Parts<'a>> {
        let colon = url.find(':')?;
        let scheme = &url[..colon];
        if scheme.is_empty() || !scheme.starts_with(|c: char| c.is_ascii_alphabetic()) {
            return None;
        }
        if !scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        {
            return None;
        }

        let mut rest = &url[colon + 1..];
        let authority = match rest.strip_prefix("//") {
            Some(after) => {
                let end = after
                    .find(['/', '?', '#'])
                    .unwrap_or(after.len());
                let (authority, after) = after.split_at(end);
                rest = after;
                Some(authority)
            }
            None => None,
        };

        let (rest, fragment) = match rest.split_once('#') {
            Some((rest, fragment)) => (rest, Some(fragment)),
            None => (rest, None),
        };
        let (path, query) = match rest.split_once('?') {
            Some((path, query)) => (path, Some(query)),
            None => (rest, None),
        };

        Some(Parts { scheme, authority, path, query, fragment })
    }
}

/// `RemovePassword`: `user:secret@host` becomes `user@host`. Measured against Qt.
fn without_password(authority: &str) -> String {
    let Some((userinfo, host)) = authority.rsplit_once('@') else {
        return authority.to_string();
    };
    let user = match userinfo.split_once(':') {
        Some((user, _password)) => user,
        None => userinfo,
    };
    if user.is_empty() {
        return host.to_string();
    }
    format!("{user}@{host}")
}

/// Drop `url.yank_ignored_parameters` from a query string.
///
/// `urlutils.py:698` reads the query with `;` as the separator when there is no `&` in it, which is
/// the old HTML form convention; the answer is rebuilt with whichever separator it read.
fn strip_ignored_parameters(query: &str) -> String {
    if query.is_empty() {
        return String::new();
    }
    let separator = if !query.contains('&') && query.contains(';') {
        ';'
    } else {
        '&'
    };
    let kept: Vec<&str> = query
        .split(separator)
        .filter(|item| {
            let key = item.split_once('=').map(|(key, _)| key).unwrap_or(item);
            !IGNORED_QUERY_PARAMETERS.contains(&key)
        })
        .collect();
    kept.join(&separator.to_string())
}

// --- percent encoding, both directions ---------------------------------------------------------

/// Bytes that stay percent-encoded in a pretty URL and are never escaped when they arrive literal:
/// RFC 3986's gen-delims and sub-delims, plus `%` itself.
///
/// Measured byte by byte against `QUrl::toString(DecodeReserved)`; identical in the path, the query
/// and the fragment.
const DELIMITERS: &[u8] = b"!#$%&'()*+,/:;=?@[]";

/// Bytes that a `FullyEncoded` URL escapes when they arrive literal, and that a pretty URL decodes.
/// Measured against `QUrl::toString(FullyEncoded)` for every ASCII byte.
const UNSAFE: &[u8] = b" \"<>\\^`{|}";

fn is_unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')
}

/// Recode one URL component into the encoded (`pretty == false`) or the pretty spelling.
fn recode(component: &str, pretty: bool) -> String {
    let bytes = component.as_bytes();
    // Qt's whole-component fallback: one invalid escape and every `%` in the component is a
    // literal, with no escape in it honoured. Measured — `x%zz%D0%BFy` keeps its `%D0%BF` escaped
    // even in the pretty form.
    let literal_percent = has_invalid_escape(bytes);

    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && !literal_percent {
            let byte = hex_pair(bytes, i + 1).expect("checked by has_invalid_escape");
            if is_unreserved(byte) {
                // RFC 3986 normalisation: an escaped unreserved byte decodes in both spellings.
                out.push(byte);
                i += 3;
            } else if byte < 0x80 {
                if DELIMITERS.contains(&byte) || byte < 0x20 || byte == 0x7f || !pretty {
                    push_escape(&mut out, byte);
                } else {
                    // An escaped unsafe byte — a space above all — is what `pretty-url` decodes.
                    out.push(byte);
                }
                i += 3;
            } else if pretty {
                match utf8_run(bytes, i) {
                    Some((decoded, len)) => {
                        out.extend_from_slice(&decoded);
                        i += len;
                    }
                    // Not valid UTF-8 — `%FF`, a lone `%D0`. Qt leaves it encoded; so does bru.
                    None => {
                        push_escape(&mut out, byte);
                        i += 3;
                    }
                }
            } else {
                push_escape(&mut out, byte);
                i += 3;
            }
            continue;
        }

        let byte = bytes[i];
        let escape_it = if pretty {
            // A literal `%` in a component Qt gave up on is the one thing the pretty form still
            // escapes: `x%zzп y` is `x%25zzп y`, measured.
            byte == b'%'
        } else {
            byte >= 0x80 || byte < 0x20 || byte == 0x7f || UNSAFE.contains(&byte) || byte == b'%'
        };
        if escape_it {
            push_escape(&mut out, byte);
        } else {
            out.push(byte);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn push_escape(out: &mut Vec<u8>, byte: u8) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    out.push(b'%');
    out.push(HEX[(byte >> 4) as usize]);
    out.push(HEX[(byte & 0xf) as usize]);
}

/// The byte two hex digits at `at` spell, or `None` if they are not two hex digits.
fn hex_pair(bytes: &[u8], at: usize) -> Option<u8> {
    let high = hex_digit(*bytes.get(at)?)?;
    let low = hex_digit(*bytes.get(at + 1)?)?;
    Some(high << 4 | low)
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn has_invalid_escape(bytes: &[u8]) -> bool {
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if hex_pair(bytes, i + 1).is_none() {
                return true;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    false
}

/// The character a run of escapes starting at `at` spells, with how many bytes of input it took.
///
/// `%D0%BF` is one `п` and six bytes. `None` when the escapes do not form valid UTF-8.
fn utf8_run(bytes: &[u8], at: usize) -> Option<(Vec<u8>, usize)> {
    let lead = hex_pair(bytes, at + 1)?;
    let width = match lead {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return None,
    };
    let mut decoded = Vec::with_capacity(width);
    for n in 0..width {
        let offset = at + n * 3;
        if bytes.get(offset) != Some(&b'%') {
            return None;
        }
        decoded.push(hex_pair(bytes, offset + 1)?);
    }
    std::str::from_utf8(&decoded).ok()?;
    Some((decoded, width * 3))
}

// -----------------------------------------------------------------------------------------------
// Saying that it happened
// -----------------------------------------------------------------------------------------------

/// qutebrowser prints `Yanked URL to clipboard: …` in its message area. bru has no message area, so
/// this is the status line's left half — the space the command line takes when `:` is pressed, and
/// which is empty in every other mode. It is a new `#message` element in `chrome/bottom.html`, sent
/// through `ipc::set_message`, and it clears itself after two seconds, which is qutebrowser's
/// `messages.timeout`.
///
/// The generation counter is what keeps two yanks in a row from the first one's timer wiping the
/// second one's message: a clear only fires if nothing has been said since it was scheduled.
static MESSAGE_GENERATION: AtomicU64 = AtomicU64::new(0);

pub fn message(text: String) {
    let generation = MESSAGE_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    crate::ipc::set_message(text);
    let mut task = ClearMessage::new(generation);
    post_delayed_task(ThreadId::UI, Some(&mut task), MESSAGE_TIMEOUT_MS);
}

wrap_task! {
    struct ClearMessage {
        generation: u64,
    }

    impl Task {
        fn execute(&self) {
            if MESSAGE_GENERATION.load(Ordering::SeqCst) == self.generation {
                crate::ipc::set_message(String::new());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every expectation below came out of a C++ program calling `QUrl::toString` with
    /// qutebrowser's own flags, on the Qt 6.11.1 this machine has. They are transcribed, not
    /// remembered.
    #[test]
    fn the_two_url_spellings_match_qt() {
        let url = "https://example.com/%D0%BF%D1%8A%D1%82%20%D0%BA%D1%8A%D0%BC/100%25\
                   ?q=a%20b&ref=x&utm_source=nl&keep=1";
        assert_eq!(
            yank_text(url, false),
            "https://example.com/%D0%BF%D1%8A%D1%82%20%D0%BA%D1%8A%D0%BC/100%25?q=a%20b&keep=1"
        );
        assert_eq!(
            yank_text(url, true),
            "https://example.com/път към/100%25?q=a b&keep=1"
        );
    }

    #[test]
    fn cyrillic_is_the_difference_and_a_percent_is_not() {
        let url = "https://ru.wikipedia.org/wiki/%D0%9A%D0%BE%D1%88%D0%BA%D0%B0";
        assert_eq!(yank_text(url, false), url);
        assert_eq!(yank_text(url, true), "https://ru.wikipedia.org/wiki/Кошка");

        // A reserved byte that arrived encoded stays encoded in both — this is the trap in
        // "pretty means decoded".
        let url = "https://example.com/a%2Fb%25c";
        assert_eq!(yank_text(url, false), url);
        assert_eq!(yank_text(url, true), url);
    }

    #[test]
    fn a_file_url_with_a_space_a_percent_and_cyrillic() {
        // The exact shape the run against the real browser used.
        let url = "file:///tmp/%D0%BF%D1%8A%D1%82%20%D0%BA%D1%8A%D0%BC%20100%25.html";
        assert_eq!(yank_text(url, false), url);
        assert_eq!(yank_text(url, true), "file:///tmp/път към 100%25.html");
    }

    #[test]
    fn ignored_parameters_go_and_the_question_mark_goes_with_them() {
        assert_eq!(
            yank_text("https://example.com/?ref=x", false),
            "https://example.com/"
        );
        assert_eq!(
            yank_text("https://example.com/x?utm_source=a&utm_medium=b", false),
            "https://example.com/x"
        );
        assert_eq!(
            yank_text("https://example.com/?a=1&ref=x&b=2", false),
            "https://example.com/?a=1&b=2"
        );
        // The semicolon-separated form keeps its semicolons.
        assert_eq!(
            yank_text("https://example.com/?a=1;ref=x;b=2", false),
            "https://example.com/?a=1;b=2"
        );
        // A parameter whose *value* is one of the names is not a parameter of that name.
        assert_eq!(
            yank_text("https://example.com/?q=utm_source", false),
            "https://example.com/?q=utm_source"
        );
    }

    #[test]
    fn a_password_never_reaches_the_clipboard() {
        assert_eq!(
            yank_text("https://user:secret@example.com/x", false),
            "https://user@example.com/x"
        );
        assert_eq!(
            yank_text("https://user:secret@example.com/x", true),
            "https://user@example.com/x"
        );
        assert_eq!(domain("https://user:secret@example.com/x"), "https://example.com");
    }

    #[test]
    fn mailto_loses_its_scheme() {
        assert_eq!(yank_text("mailto:someone@example.com", false), "someone@example.com");
        assert_eq!(yank_text("mailto:someone@example.com", true), "someone@example.com");
    }

    #[test]
    fn an_invalid_escape_makes_every_percent_literal() {
        assert_eq!(
            yank_text("https://example.com/a%zz", false),
            "https://example.com/a%25zz"
        );
        assert_eq!(
            yank_text("https://example.com/a%zz", true),
            "https://example.com/a%25zz"
        );
        // Measured: the valid escapes beside it are not honoured either.
        assert_eq!(
            yank_text("https://e.com/x%zz%D0%BFy", false),
            "https://e.com/x%25zz%25D0%25BFy"
        );
        assert_eq!(
            yank_text("https://e.com/x%zz%D0%BFy", true),
            "https://e.com/x%25zz%25D0%25BFy"
        );
    }

    #[test]
    fn broken_utf8_stays_encoded_even_when_pretty() {
        assert_eq!(yank_text("https://e.com/x%FFy", true), "https://e.com/x%FFy");
        assert_eq!(yank_text("https://e.com/x%D0y", true), "https://e.com/x%D0y");
        assert_eq!(yank_text("https://e.com/x%C3%28y", true), "https://e.com/x%C3%28y");
        // Three bytes, one character.
        assert_eq!(yank_text("https://e.com/x%E2%82%ACy", true), "https://e.com/x€y");
    }

    #[test]
    fn a_literal_space_or_unicode_is_encoded_by_yank_and_left_by_pretty() {
        assert_eq!(
            yank_text("https://example.com/a b/c", false),
            "https://example.com/a%20b/c"
        );
        assert_eq!(
            yank_text("https://example.com/a b/c", true),
            "https://example.com/a b/c"
        );
        assert_eq!(
            yank_text("https://ru.wikipedia.org/wiki/Кошка", false),
            "https://ru.wikipedia.org/wiki/%D0%9A%D0%BE%D1%88%D0%BA%D0%B0"
        );
    }

    #[test]
    fn the_fragment_is_recoded_like_the_path() {
        assert_eq!(
            yank_text("https://example.com/x#%D1%84%D1%80%D0%B0%D0%B3", true),
            "https://example.com/x#фраг"
        );
        assert_eq!(
            yank_text("https://example.com/x#%D1%84%D1%80%D0%B0%D0%B3", false),
            "https://example.com/x#%D1%84%D1%80%D0%B0%D0%B3"
        );
    }

    #[test]
    fn domains() {
        assert_eq!(domain("https://example.com/a/b?c=1"), "https://example.com");
        assert_eq!(domain("https://example.com:8443/p?a=1"), "https://example.com:8443");
        assert_eq!(domain("http://localhost:8080/"), "http://localhost:8080");
        assert_eq!(domain("bru://chrome/help"), "bru://chrome");
        // Qt prints the empty authority rather than nothing at all.
        assert_eq!(domain("mailto:someone@example.com"), "mailto://");
        assert_eq!(domain(""), "");
    }

    #[test]
    fn a_string_that_is_not_a_url_is_yanked_as_it_stands() {
        assert_eq!(yank_text("", false), "");
        assert_eq!(yank_text("not a url", false), "not a url");
    }

    #[test]
    fn the_inline_template_is_the_ym_binding() {
        let url = "https://ru.wikipedia.org/wiki/%D0%9A%D0%BE%D1%88%D0%BA%D0%B0";
        assert_eq!(
            inline("[{title}]({url:yank})", url, "Кошка — Википедия"),
            "[Кошка — Википедия](https://ru.wikipedia.org/wiki/%D0%9A%D0%BE%D1%88%D0%BA%D0%B0)"
        );
        // The encoded spelling is the point: a space in the target would break the link.
        assert_eq!(
            inline("[{title}]({url:yank})", "https://e.com/a b", "T"),
            "[T](https://e.com/a%20b)"
        );
        assert_eq!(inline("plain text", url, "T"), "plain text");
    }

    #[test]
    fn expanding_the_two_selections() {
        let read = |selection: Selection| match selection {
            Selection::Clipboard => Ok("https://clip.example/".to_string()),
            Selection::Primary => Ok("https://prim.example/".to_string()),
        };
        assert_eq!(
            expand_with("{clipboard}", read).unwrap(),
            "https://clip.example/"
        );
        assert_eq!(
            expand_with("{primary}", read).unwrap(),
            "https://prim.example/"
        );
        // The two are not the same selection, and reading the wrong one is the whole bug this
        // guards against.
        assert_ne!(
            expand_with("{clipboard}", read).unwrap(),
            expand_with("{primary}", read).unwrap()
        );
        // A variable that belongs to another module is left for it.
        assert_eq!(expand_with("{url}", read).unwrap(), "{url}");
        assert_eq!(expand_with("no braces here", read).unwrap(), "no braces here");
        assert_eq!(
            expand_with("a {clipboard} b {primary} c", read).unwrap(),
            "a https://clip.example/ b https://prim.example/ c"
        );
    }

    #[test]
    fn a_selection_is_read_once_and_its_contents_are_not_rescanned() {
        let mut reads = 0;
        let expanded = expand_with("{clipboard}{clipboard}", |_| {
            reads += 1;
            Ok("{primary}".to_string())
        })
        .unwrap();
        assert_eq!(reads, 1, "the selection was read more than once");
        // `{primary}` came *out* of the clipboard; expanding it again would let a copied string
        // reach for a second selection.
        assert_eq!(expanded, "{primary}{primary}");
    }

    #[test]
    fn an_empty_selection_stops_the_command() {
        let error = expand_with("{clipboard}", |_| Err("Clipboard is empty.".to_string()));
        assert_eq!(error, Err("Clipboard is empty.".to_string()));
    }

    #[test]
    fn no_argument_is_not_an_error() {
        assert_eq!(expand(None), Ok(None));
        // Nothing to substitute means nothing is read, so this needs no compositor.
        assert_eq!(
            expand(Some("https://example.com")),
            Ok(Some("https://example.com".to_string()))
        );
    }

    #[test]
    fn dash_s_is_the_primary_selection() {
        assert_eq!(Selection::from_sel_flag(true), Selection::Primary);
        assert_eq!(Selection::from_sel_flag(false), Selection::Clipboard);
        assert_eq!(Selection::Primary.name(), "primary selection");
        assert_eq!(Selection::Clipboard.name(), "clipboard");
    }
}

// -----------------------------------------------------------------------------------------------
// What hint mode needs from the clipboard
// -----------------------------------------------------------------------------------------------

/// `;y` and `;Y`. `hints.rs` resolves the element's URL and hands it here.
///
/// The only piece hint mode knows and this module cannot work out is `first_run`: from the second
/// follow of a `--rapid` session onwards qutebrowser *appends*, newline-joined, so `;r`-style
/// repeated yanking collects a list rather than overwriting it each time
/// (`hints.py:HintActions.yank`).
pub struct HintClipboard;

impl crate::hints::Clipboard for HintClipboard {
    fn yank_url(&self, url: &str, selection: bool, first_run: bool) {
        let target = Selection::from_sel_flag(selection);

        let text = if first_run {
            url.to_string()
        } else {
            // Reading back is what makes the append work, and it can fail — a selection can be
            // taken away between two follows. Losing what was there is worse than losing the join,
            // so a failed read starts a fresh list rather than aborting the yank.
            match get(target) {
                Ok(existing) if !existing.is_empty() => format!("{existing}\n{url}"),
                _ => url.to_string(),
            }
        };

        match set(target, &text) {
            Ok(()) => message(format!("Yanked URL to {}: {url}", target.name())),
            Err(error) => message(error),
        }
    }
}

/// `;d`. `hints.rs` resolves the element's URL and hands it here.
pub struct HintDownloads;

impl crate::hints::Downloads for HintDownloads {
    fn download_url(&self, url: &str) {
        // `schedule_start` posts to the UI thread, which is what CEF-NOTES trap 12 requires: this
        // is reached from inside the message router's query handler, where starting a navigation
        // deadlocks on a lock `on_before_browse` also wants.
        crate::downloads::schedule_start(url.to_string());
    }
}

/// `completion-item-yank` — `<Ctrl-C>` and `<Ctrl-Shift-C>` in command mode.
///
/// A plain `fn` rather than a trait object because that is the shape `completers.rs` asks for, and
/// there is no state to carry: the completion has already decided which cell's text to copy.
pub fn yank_plain(text: &str, selection: bool) {
    let target = Selection::from_sel_flag(selection);
    if text.is_empty() {
        message("Nothing to yank".to_string());
        return;
    }
    match set(target, text) {
        Ok(()) => message(format!("Yanked to {}: {text}", target.name())),
        Err(error) => message(error),
    }
}
