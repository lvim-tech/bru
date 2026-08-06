//! `:spawn`, the userscript protocol, and the `{url}`-style substitution every command that takes
//! one shares.
//!
//! # The boundary: what can start a process
//!
//! This is the one module in bru that runs other people's programs, so the list of things that can
//! reach it is short on purpose and is the most important thing in this file.
//!
//! **A process starts only when a [`Command::Spawn`](crate::commands::Command::Spawn) reaches
//! [`crate::exec::run`], and that happens from exactly three places:**
//!
//! 1. A key whose binding is `spawn …`. Bindings come from the compiled-in qutebrowser defaults —
//!    none of which is a `spawn` — and from `~/.config/bru/config.lua`, which is a file the user
//!    wrote. A page cannot add a binding: the Lua state is read once at startup and dropped before
//!    any browser exists (`config.rs`).
//! 2. `:spawn …` typed into the command line. That text comes from the `#cmdline` input in bru's
//!    own `bru://chrome/bottom.html`, and `ipc.rs` refuses a `cefQuery` from any frame whose URL
//!    does not start with `bru://`.
//! 3. A line a **running userscript** writes into the FIFO bru handed it. That is the script the
//!    user installed and bound, talking back through a pipe whose path is only in its own
//!    environment, in `$XDG_RUNTIME_DIR/bru/`, created `0600`.
//!
//! **What cannot start a process, and why:**
//!
//! - **A web page.** `window.cefQuery` is injected into every frame Chromium creates, bru's chrome
//!   and every site alike; `ipc::BruQueryHandler` answers `failure(-2)` to any frame that is not
//!   `bru://`. Nothing else in bru turns page-supplied text into a command.
//! - **A hint.** qutebrowser has `hint links spawn mpv {hint-url}`, where a string the *page*
//!   chose becomes an argv element. bru's parser answers `Command::Unimplemented` for every hint
//!   target but plain click and background-tab (`commands.rs`, the `"hint"` arm), so that spelling
//!   is inert. It must stay inert until someone has thought about page-chosen argv.
//! - **A substituted variable.** `{url}` and `{title}` *are* page-chosen — a site picks its own
//!   address and title. Two things keep that from mattering: substitution is **single-pass**, so a
//!   URL containing `{clipboard}` does not expand (qutebrowser's `re.sub`-with-callback property,
//!   and [`replace_variables`] reproduces it); and **nothing here runs a shell**, so `;`, `|`,
//!   backticks and `$(…)` inside a substituted value are ordinary bytes in one argv element.
//!
//! # Threads
//!
//! [`spawn`] is called on the CEF **UI thread** — it is reached from `exec::run`, which is reached
//! from a key event or from a posted task. It must not block there, so it does the environment
//! gathering (which reads `BruState`) inline and hands everything else to plain std threads: one
//! that reads the FIFO, one that waits for the child. **If the program never exits, those two
//! threads park forever and bru is otherwise unaffected** — the UI thread never waited on it, the
//! window keeps painting, keys keep working, and the FIFO is left in `$XDG_RUNTIME_DIR/bru/` when
//! bru quits. Measured; see the report for this milestone.
//!
//! A command that arrives on the FIFO reader thread is handed to
//! [`crate::exec::run_from_cmdline`], which posts it to the UI thread. That is CEF-NOTES trap 12:
//! a navigation must not start on a thread that is not the UI thread, and a userscript's
//! `open -t …` is exactly a navigation.

use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// The line the waiter thread writes into the FIFO to wake the reader once the child is gone.
///
/// The FIFO is held open read-*and*-write by bru so that it never reports EOF when the script's
/// shell closes its end — the same reason qutebrowser opens it `O_RDWR` — which means the reader
/// can only be stopped by something arriving on it. A NUL is not a byte any command line contains.
const SENTINEL: &str = "\u{0}bru-userscript-finished";

/// How `:spawn` was spelled.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Opts {
    /// `-u` / `--userscript`: look the command up in the userscript directories, give it a FIFO and
    /// the `BRU_*` environment.
    pub userscript: bool,
    /// `-d` / `--detach`: put the child in its own process group with its standard streams on
    /// `/dev/null`, so it outlives bru and does not hold bru's terminal.
    pub detach: bool,
    /// `-m` / `--output-messages`: show what the program printed as a message.
    pub output_messages: bool,
    /// `-v` / `--verbose`: say when it started and when it exited.
    pub verbose: bool,
}

// -----------------------------------------------------------------------------------------------
// Messages
// -----------------------------------------------------------------------------------------------

/// Where a message goes once something wants to show one.
///
/// A hook rather than a call into `message.rs` by name, because this module was written before there
/// was a message line and had to say something either way. `app.rs` installs `message::info` at
/// startup; with nothing installed — every unit test, and the moment before
/// `on_context_initialized` — messages go to stderr rather than nowhere.
///
/// One level, not three: `spawn::message` says "started …" and "could not run …" through the same
/// call, so everything here arrives as info. Splitting the failures out to `message::error` means
/// giving this module a second entry point, which is a change to make deliberately rather than
/// while wiring the sink up.
static SINK: Mutex<Option<fn(&str)>> = Mutex::new(None);

/// Install the message sink. Called once, at startup, by `app.rs`.
pub fn set_message_sink(sink: fn(&str)) {
    if let Ok(mut slot) = SINK.lock() {
        *slot = Some(sink);
    }
}

/// Show a message. Safe from any thread — the waiter threads below are the main callers.
pub fn message(text: &str) {
    let sink = SINK.lock().ok().and_then(|sink| *sink);
    match sink {
        Some(sink) => sink(text),
        None => eprintln!("bru: {text}"),
    }
}

// -----------------------------------------------------------------------------------------------
// Variable substitution
// -----------------------------------------------------------------------------------------------

/// Everything `{…}` can stand for, read once per substitution.
///
/// Held as a struct rather than read lazily so that a command line with three `{url}`s asks the
/// state once, and — more to the point — so that [`replace_variables_in`] is a pure function the
/// tests can drive without a browser.
#[derive(Clone, Default, Debug)]
pub struct Values {
    pub url: String,
    pub title: String,
    /// `wl-paste`, read only when a `{clipboard}` is actually present.
    pub clipboard: Option<String>,
    /// `wl-paste --primary`, same.
    pub primary: Option<String>,
}

impl Values {
    /// What the tab that is showing has, plus the clipboard if `text` asks for it.
    ///
    /// Reads `BruState`, so it must be called on the UI thread.
    pub fn current(text: &str) -> Values {
        let (url, title) = match crate::state::BruState::instance() {
            Some(state) => match state.lock() {
                Ok(state) => {
                    let active = state.active_tab();
                    (
                        state.tab_url(active).unwrap_or_default(),
                        state.tab_title(active).unwrap_or_default(),
                    )
                }
                Err(_) => (String::new(), String::new()),
            },
            None => (String::new(), String::new()),
        };
        Values {
            url,
            title,
            // Reading the clipboard forks `wl-paste`, so it happens only when something asks.
            clipboard: text.contains("{clipboard}").then(|| wl_paste(false)),
            primary: text.contains("{primary}").then(|| wl_paste(true)),
        }
    }

    /// The value of one variable name, or `None` for a name bru does not know — which is left in
    /// the string verbatim, exactly as qutebrowser's regex leaves an unmatched `{…}` alone.
    fn get(&self, name: &str) -> Option<String> {
        let url = Url::parse(&self.url);
        let value = match name {
            // bru holds one URL string per tab, the one Chromium reported through
            // `on_address_change`. qutebrowser distinguishes `{url}` (fully encoded) from
            // `{url:pretty}` (reserved characters decoded) by re-rendering a QUrl; with no URL
            // parser in the tree both spell the address bru was given.
            "url" | "url:pretty" | "url:yank" => self.url.clone(),
            "url:scheme" => url.scheme.to_string(),
            "url:username" => url.username.to_string(),
            "url:password" => url.password.to_string(),
            "url:auth" => {
                if url.username.is_empty() {
                    String::new()
                } else {
                    format!("{}:{}@", url.username, url.password)
                }
            }
            "url:host" => url.host.to_string(),
            "url:port" => url.port.to_string(),
            "url:domain" => {
                if url.scheme.is_empty() && url.host.is_empty() {
                    String::new()
                } else if url.port.is_empty() {
                    format!("{}://{}", url.scheme, url.host)
                } else {
                    format!("{}://{}:{}", url.scheme, url.host, url.port)
                }
            }
            "url:path" => url.path.to_string(),
            "url:query" => url.query.to_string(),
            "title" => self.title.clone(),
            "clipboard" => self.clipboard.clone().unwrap_or_default(),
            "primary" => self.primary.clone().unwrap_or_default(),
            _ => return None,
        };
        Some(value)
    }
}

/// `{url}`, `{title}`, `{clipboard}` and the rest, against the tab that is showing.
///
/// **This is the shared one.** `commands/runners.py::replace_variables` is one function in
/// qutebrowser and every command that takes a `{…}` argument goes through it; the same is meant to
/// be true here. `cmdline.rs` has a two-variable copy of its own that predates this and should be
/// pointed at [`replace_variables_in`] when the two files are next in one hand — see the report.
///
/// Reads `BruState` and possibly forks `wl-paste`, so: UI thread.
pub fn replace_variables(text: &str) -> String {
    if !text.contains('{') {
        return text.to_string();
    }
    replace_variables_in(text, &Values::current(text))
}

/// The pure half: one pass, left to right, no value ever rescanned.
///
/// **Single-pass is a security property, not a performance one.** `{url}` is a string the *page*
/// chose. If the output were scanned again, a page at `http://example.com/#{clipboard}` would have
/// the user's clipboard pasted into an argv element. qutebrowser gets this from `re.sub` with a
/// callback ("avoids expansion of nested variables"); here it falls out of only ever appending to
/// the output.
///
/// `{{url}}` is the escape for a literal `{url}`, as in qutebrowser, where every replacement name
/// is also registered wrapped in a second pair of braces and maps to itself.
pub fn replace_variables_in(text: &str, values: &Values) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != '{' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        // `{{name}}` — the escape. Checked before the plain form so the inner braces are not read
        // as a name of their own.
        if let Some((name, end)) = braced(&bytes, i + 1) {
            if bytes.get(end) == Some(&'}') && values.get(&name).is_some() {
                out.push('{');
                out.push_str(&name);
                out.push('}');
                i = end + 1;
                continue;
            }
        }
        match braced(&bytes, i) {
            Some((name, end)) => match values.get(&name) {
                Some(value) => {
                    out.push_str(&value);
                    i = end;
                }
                // Not a name bru knows: `{}` and `{foo}` reach the program unchanged.
                None => {
                    out.push('{');
                    i += 1;
                }
            },
            None => {
                out.push('{');
                i += 1;
            }
        }
    }
    out
}

/// The text between the `{` at `start` and the next `}`, and the index one past that `}`.
fn braced(chars: &[char], start: usize) -> Option<(String, usize)> {
    if chars.get(start) != Some(&'{') {
        return None;
    }
    let close = chars[start + 1..].iter().position(|c| *c == '}')? + start + 1;
    Some((chars[start + 1..close].iter().collect(), close + 1))
}

/// The pieces of a URL, without a URL parser. Enough for `{url:host}` and its siblings; anything it
/// cannot find is empty, which is what qutebrowser's QUrl accessors give for the same input.
#[derive(Default)]
struct Url<'a> {
    scheme: &'a str,
    username: &'a str,
    password: &'a str,
    host: &'a str,
    port: &'a str,
    path: &'a str,
    query: &'a str,
}

impl<'a> Url<'a> {
    fn parse(url: &'a str) -> Url<'a> {
        let Some((scheme, rest)) = url.split_once("://") else {
            return Url::default();
        };
        let rest = rest.split_once('#').map(|(before, _)| before).unwrap_or(rest);
        let (authority, after) = match rest.find('/') {
            Some(at) => (&rest[..at], &rest[at..]),
            None => (rest, ""),
        };
        let (path, query) = match after.split_once('?') {
            Some((path, query)) => (path, query),
            None => (after, ""),
        };
        let (userinfo, hostport) = match authority.rsplit_once('@') {
            Some((userinfo, hostport)) => (userinfo, hostport),
            None => ("", authority),
        };
        let (username, password) = match userinfo.split_once(':') {
            Some((username, password)) => (username, password),
            None => (userinfo, ""),
        };
        // `[::1]:8080` — the colon inside the brackets is not the port separator.
        let (host, port) = match hostport.rfind(':') {
            Some(at) if !hostport[at..].contains(']') => (&hostport[..at], &hostport[at + 1..]),
            _ => (hostport, ""),
        };
        Url { scheme, username, password, host, port, path, query }
    }
}

/// `wl-paste`, which STAGE3-CONTRACTS.md settles as the clipboard on this machine.
///
/// `--no-newline` because wl-paste appends one otherwise and a URL with a trailing newline is not a
/// URL. An empty clipboard exits non-zero; that is not an error worth a message.
fn wl_paste(primary: bool) -> String {
    let mut command = Command::new("wl-paste");
    command.arg("--no-newline");
    if primary {
        command.arg("--primary");
    }
    match command.output() {
        Ok(output) => String::from_utf8_lossy(&output.stdout).to_string(),
        Err(error) => {
            message(&format!("could not read the clipboard: {error}"));
            String::new()
        }
    }
}

// -----------------------------------------------------------------------------------------------
// :spawn
// -----------------------------------------------------------------------------------------------

/// `:spawn [-u] [-d] [-m] [-v] cmd [args…]`.
///
/// UI thread. Everything that could block is on a thread of its own by the time this returns.
pub fn spawn(cmdline: &str, opts: Opts, count: Option<u32>) {
    let parts = match shlex(cmdline) {
        Ok(parts) => parts,
        Err(error) => {
            message(&format!("spawn: {error}"));
            return;
        }
    };
    let Some((command, args)) = parts.split_first() else {
        message("spawn: nothing to run");
        return;
    };

    // Variables are replaced in the *arguments* only, never in the program name — qutebrowser does
    // the same (`args = runners.replace_variables(win_id, args)`), and it means a `{url}` can never
    // decide which binary runs.
    let args: Vec<String> = args.iter().map(|arg| replace_variables(arg)).collect();

    if opts.userscript {
        run_userscript(command, &args, opts, count);
    } else {
        run_plain(command, &args, opts);
    }
}

/// A plain `:spawn`: no FIFO, no `BRU_*` environment, the process inherits bru's own.
fn run_plain(command: &str, args: &[String], opts: Opts) {
    let path = expand_user(command);
    let mut child = Command::new(&path);
    child.args(args);
    configure_stdio(&mut child, opts);

    let child = match child.spawn() {
        Ok(child) => child,
        Err(error) => {
            message(&format!("spawn: could not run {}: {error}", path.display()));
            return;
        }
    };
    if opts.verbose {
        message(&format!("started {} (pid {})", path.display(), child.id()));
    }
    watch(child, path.display().to_string(), opts, None);
}

/// `:spawn --userscript`: the qutebrowser protocol, with bru's names.
fn run_userscript(command: &str, args: &[String], opts: Opts, count: Option<u32>) {
    let path = match resolve_userscript(command) {
        Ok(path) => path,
        Err(error) => {
            message(&format!("spawn: {error}"));
            return;
        }
    };

    let fifo = match make_fifo() {
        Ok(fifo) => fifo,
        Err(error) => {
            message(&format!("spawn: {error}"));
            return;
        }
    };

    // Opened read-*and*-write, and opened here rather than on the reader thread. Read-write so the
    // pipe never reports EOF between one `echo >> $BRU_FIFO` and the next (qutebrowser opens it the
    // same way and links the reason); here rather than on the thread so that the child, which may
    // write to it the instant it starts, never finds it without a reader and blocks on `open`.
    let handle = OpenOptions::new().read(true).write(true).open(&fifo);
    let handle = match handle {
        Ok(handle) => handle,
        Err(error) => {
            message(&format!("spawn: could not open {}: {error}", fifo.display()));
            let _ = std::fs::remove_file(&fifo);
            return;
        }
    };
    let waker = match handle.try_clone() {
        Ok(waker) => waker,
        Err(error) => {
            message(&format!("spawn: could not clone the fifo handle: {error}"));
            let _ = std::fs::remove_file(&fifo);
            return;
        }
    };

    let mut child = Command::new(&path);
    child.args(args);
    for (key, value) in environment(&fifo, count) {
        child.env(key, value);
    }
    configure_stdio(&mut child, opts);

    let child = match child.spawn() {
        Ok(child) => child,
        Err(error) => {
            message(&format!("spawn: could not run {}: {error}", path.display()));
            let _ = std::fs::remove_file(&fifo);
            return;
        }
    };
    if opts.verbose {
        message(&format!("started {} (pid {})", path.display(), child.id()));
    }

    // The reader. One command per line, each posted to the UI thread by `run_from_cmdline` —
    // CEF-NOTES trap 12, and the whole point of the protocol.
    std::thread::spawn(move || {
        for line in BufReader::new(handle).lines() {
            let Ok(line) = line else {
                break;
            };
            if line == SENTINEL {
                break;
            }
            let text = line.trim();
            // qutebrowser's userscripts write `#` comments into the FIFO in their examples, and a
            // blank line is what a `echo >> "$QUTE_FIFO"` with no argument leaves.
            if text.is_empty() || text.starts_with('#') {
                continue;
            }
            // A script may write `open -t x` or `:open -t x`; qutebrowser's CommandRunner accepts
            // both, because its `run` strips the prefix the command line would have shown.
            let text = text.strip_prefix(':').unwrap_or(text);
            if std::env::var_os("BRU_DEBUG_SPAWN").is_some() {
                eprintln!("bru[spawn]: fifo -> {text:?}");
            }
            crate::exec::run_from_cmdline(text, None);
        }
        if std::env::var_os("BRU_DEBUG_SPAWN").is_some() {
            eprintln!("bru[spawn]: fifo reader finished");
        }
    });

    watch(child, path.display().to_string(), opts, Some((fifo, waker)));
}

/// Wait for the child on a thread of its own, then report, wake the FIFO reader and delete the FIFO.
///
/// `wait_with_output` rather than `wait` because `-m` pipes both streams: a child that fills the
/// pipe while nobody drains it stops, and `wait` would then never return.
fn watch(
    child: std::process::Child,
    what: String,
    opts: Opts,
    fifo: Option<(PathBuf, std::fs::File)>,
) {
    std::thread::spawn(move || {
        let output = child.wait_with_output();

        if let Some((path, mut waker)) = fifo {
            // The reader is blocked in `read`; it cannot notice the child is gone on its own,
            // because bru holds the write end open itself. One line ends it.
            let _ = writeln!(waker, "{SENTINEL}");
            let _ = std::fs::remove_file(&path);
        }

        match output {
            Ok(output) => {
                if opts.verbose {
                    message(&format!("{what} exited with {}", output.status));
                }
                if opts.output_messages {
                    let mut text = String::from_utf8_lossy(&output.stdout).to_string();
                    text.push_str(&String::from_utf8_lossy(&output.stderr));
                    let text = text.trim_end();
                    if text.is_empty() {
                        message(&format!("{what}: no output"));
                    } else {
                        message(text);
                    }
                }
                if !output.status.success() && !opts.verbose {
                    message(&format!("{what} exited with {}", output.status));
                }
            }
            Err(error) => message(&format!("{what}: {error}")),
        }
    });
}

/// `-m` needs both streams in hand; `-d` needs them nowhere, so a detached program cannot hold
/// bru's terminal open. Everything else inherits, which is what makes a plain `:spawn` printable.
fn configure_stdio(command: &mut Command, opts: Opts) {
    if opts.output_messages {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
    } else if opts.detach {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
    }
    if opts.detach {
        // Its own process group, so a Ctrl-C in the terminal bru was started from does not reach
        // it, and so it survives bru. `process_group` is std's, stable since 1.64 — no `libc`
        // dependency and no `pre_exec`, which would have to be `unsafe`.
        std::os::unix::process::CommandExt::process_group(command, 0);
    }
}

// -----------------------------------------------------------------------------------------------
// The userscript environment
// -----------------------------------------------------------------------------------------------

/// Every variable a bru userscript is given.
///
/// qutebrowser's names with `QUTE_` swapped for `BRU_`. Which of its variables are missing, and
/// why, is in the report for this milestone rather than here — but the short version is that
/// `BRU_SELECTED_TEXT`, `BRU_SELECTED_HTML`, `BRU_TEXT` and `BRU_HTML` all need the page's DOM,
/// which is a renderer round trip that would have to delay the spawn, and `BRU_USER_AGENT`,
/// `BRU_DOWNLOAD_DIR` and `BRU_COMMANDLINE_TEXT` name things bru either cannot ask CEF for or does
/// not have yet. Setting a variable to a plausible wrong value is worse than not setting it: a
/// script tests `if [ -n "$BRU_TEXT" ]`.
///
/// Every name here is also exported under qutebrowser's `QUTE_` prefix — see [`alias_as_qute`],
/// which is what lets this user's existing userscripts run unmodified.
///
/// UI thread — it reads `BruState` for the URL, the title and the tab index.
fn environment(fifo: &Path, count: Option<u32>) -> Vec<(String, String)> {
    let mut env = vec![
        ("BRU_FIFO".to_string(), fifo.display().to_string()),
        // qutebrowser's `QUTE_MODE` is `command` for `:spawn --userscript` and `hints` for a
        // userscript hint target. bru has no userscript hint target, so it is always `command`.
        ("BRU_MODE".to_string(), "command".to_string()),
        ("BRU_VERSION".to_string(), env!("CARGO_PKG_VERSION").to_string()),
    ];

    if let Some(dir) = config_dir() {
        env.push(("BRU_CONFIG_DIR".to_string(), dir.display().to_string()));
    }
    if let Some(dir) = crate::data::data_dir() {
        env.push(("BRU_DATA_DIR".to_string(), dir.display().to_string()));
    }
    if let Some(count) = count {
        env.push(("BRU_COUNT".to_string(), count.to_string()));
    }

    if let Some(state) = crate::state::BruState::instance() {
        if let Ok(state) = state.lock() {
            let active = state.active_tab();
            if state.tab_count() > 0 {
                env.push(("BRU_TAB_INDEX".to_string(), (active + 1).to_string()));
                if let Some(title) = state.tab_title(active) {
                    env.push(("BRU_TITLE".to_string(), title));
                }
                if let Some(url) = state.tab_url(active).filter(|url| !url.is_empty()) {
                    env.push(("BRU_URL".to_string(), url));
                }
            }
        }
    }

    alias_as_qute(&mut env);
    env
}

/// Add a `QUTE_…` twin for every `BRU_…` variable, so this user's existing qutebrowser userscripts
/// run under bru without being edited.
///
/// **Measured, not assumed.** The five userscripts this machine actually has were read, and between
/// them they name exactly five of qutebrowser's variables:
///
/// | variable | read by |
/// |---|---|
/// | `QUTE_FIFO` | all five |
/// | `QUTE_URL` | all five |
/// | `QUTE_DATA_DIR` | `qutebrowser_dmenu`, `qutebrowser_tab_dmenu`, `qute-keepassxc` |
/// | `QUTE_CONFIG_DIR` | `qutebrowser_dmenu`, `qutebrowser_tab_dmenu` |
/// | `QUTE_SELECTED_TEXT` | `qutebrowser_translate`, and only in its `--text` mode |
///
/// Four of the five are `BRU_*` already, so aliasing is the whole fix for four of the five scripts.
/// The fifth — `QUTE_SELECTED_TEXT` — needs the page's DOM and is deliberately unset; see
/// [`environment`]. `<Ctrl+Shift+T>` in this user's `config.py` is the one binding that wants it,
/// and under bru it would translate an empty string. `<Ctrl+T>`, the same script without `--text`,
/// reads `QUTE_URL` and works.
///
/// Derived from the `BRU_*` list rather than written out again: a variable added to [`environment`]
/// gets its alias for free, and there is no second list to forget to update. One name is held back:
/// `QUTE_VERSION`. Every other alias says "here is bru's answer to that question", which is true;
/// `QUTE_VERSION=0.1.0` would instead claim to be a qutebrowser version, and a script comparing it
/// against one would be misled. That is this module's own rule — a plausible wrong value is worse
/// than no value — applied to the one variable it bites.
///
/// **What aliasing does not fix.** `qutebrowser_dmenu` and `qutebrowser_tab_dmenu` read
/// `$QUTE_CONFIG_DIR/quickmarks`, which is where *qutebrowser* keeps quickmarks; bru keeps them in
/// its data directory (DESIGN.md), so that `cat` fails and prints to stderr. Neither script sets
/// `-e`, so the rest of the pipeline still runs and the menu is history plus the current URL —
/// degraded, not broken. `$QUTE_DATA_DIR/history.sqlite` needs no such caveat: bru's schema is
/// qutebrowser's, `CompletionHistory(url, title, last_atime)` included (`data.rs`), so their
/// `sqlite3` query runs verbatim.
fn alias_as_qute(env: &mut Vec<(String, String)>) {
    let aliases: Vec<(String, String)> = env
        .iter()
        .filter_map(|(key, value)| {
            let rest = key.strip_prefix("BRU_").filter(|rest| *rest != "VERSION")?;
            Some((format!("QUTE_{rest}"), value.clone()))
        })
        .collect();
    env.extend(aliases);
}

/// `$XDG_CONFIG_HOME/bru`, or `~/.config/bru`. Reported to userscripts, never created here —
/// everything under it is configer's.
fn config_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(base.join("bru"))
}

/// Where a userscript named without a path is looked for, in order.
///
/// qutebrowser's three (`userscripts.py::_lookup_path`) with bru's directories: its own data
/// directory first, then the packaged one, then the config directory — which is where this user's
/// qutebrowser userscripts already live, one level up.
pub fn userscript_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(data) = crate::data::data_dir() {
        dirs.push(data.join("userscripts"));
    }
    dirs.push(PathBuf::from("/usr/share/bru/userscripts"));
    if let Some(config) = config_dir() {
        dirs.push(config.join("userscripts"));
    }
    dirs
}

fn resolve_userscript(command: &str) -> Result<PathBuf, String> {
    let path = expand_user(command);
    if path.is_absolute() {
        return if path.exists() {
            Ok(path)
        } else {
            Err(format!("userscript {} not found", path.display()))
        };
    }
    let dirs = userscript_dirs();
    for dir in &dirs {
        let candidate = dir.join(command);
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    let searched: Vec<String> = dirs.iter().map(|dir| dir.display().to_string()).collect();
    Err(format!(
        "userscript {command:?} not found in {}",
        searched.join(", ")
    ))
}

/// A fresh FIFO in `$XDG_RUNTIME_DIR/bru/`, mode 600.
///
/// `mkfifo(3)` needs `libc`; `mkfifo(1)` is in coreutils and is already on this machine. One fork
/// per userscript run, against a dependency in `Cargo.toml` forever — and this module is the one
/// place in bru where forking a program is not a thing to avoid.
fn make_fifo() -> Result<PathBuf, String> {
    let dir = runtime_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|error| format!("could not create {}: {error}", dir.display()))?;

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let name = format!(
        "userscript-{}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    let path = dir.join(name);
    let _ = std::fs::remove_file(&path);

    let status = Command::new("mkfifo")
        .arg("-m")
        .arg("600")
        .arg(&path)
        .status()
        .map_err(|error| format!("could not run mkfifo: {error}"))?;
    if !status.success() {
        return Err(format!("mkfifo {} failed: {status}", path.display()));
    }
    Ok(path)
}

/// A path nothing else will take, in the same runtime directory the FIFOs live in.
///
/// Used by `editor.rs` for the file `$EDITOR` opens. Not `std::env::temp_dir` directly: the runtime
/// directory is per-user and mode 700, and the text of a form field is not something to leave in a
/// world-readable `/tmp`.
pub fn temp_path(what: &str, extension: &str) -> Result<PathBuf, String> {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    let dir = runtime_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|error| format!("could not create {}: {error}", dir.display()))?;
    Ok(dir.join(format!(
        "{what}-{}-{}.{extension}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )))
}

/// `$XDG_RUNTIME_DIR/bru`, falling back to `/tmp/bru`.
///
/// Not `~/.local/share/bru` on purpose: that is bru's *data*, the three files DESIGN.md lists, and
/// a pipe that exists for the length of one command is not data.
fn runtime_dir() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR").filter(|dir| !dir.is_empty()) {
        Some(dir) => PathBuf::from(dir).join("bru"),
        None => std::env::temp_dir().join("bru"),
    }
}

/// `~` and `~/…` against `$HOME`. Everything else is returned as it was.
pub fn expand_user(path: &str) -> PathBuf {
    expand_user_with(path, std::env::var_os("HOME").as_deref())
}

/// The pure half, so the test does not have to set `$HOME` on a process other tests share.
fn expand_user_with(path: &str, home: Option<&std::ffi::OsStr>) -> PathBuf {
    let Some(rest) = path.strip_prefix('~') else {
        return PathBuf::from(path);
    };
    let Some(home) = home else {
        return PathBuf::from(path);
    };
    match rest.strip_prefix('/') {
        Some(rest) => PathBuf::from(home).join(rest),
        None if rest.is_empty() => PathBuf::from(home),
        // `~otheruser/…` is not something bru resolves.
        None => PathBuf::from(path),
    }
}

// -----------------------------------------------------------------------------------------------
// Splitting a command line
// -----------------------------------------------------------------------------------------------

/// `shlex.split(posix=True)`, which is what qutebrowser's `:spawn` splits its argument with.
///
/// It has to happen here and not in the command parser: `:spawn` is a `maxsplit=0` command, so
/// everything after the flags is one string, quotes and all — `spawn --userscript qute-pass -u
/// "login: (.+)"` has to reach the script as three arguments, and it only does if the quotes
/// survive the trip.
pub fn shlex(text: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut has_token = false;
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                has_token = true;
                match chars.next() {
                    Some(escaped) => current.push(escaped),
                    None => return Err("trailing backslash".to_string()),
                }
            }
            '\'' => {
                has_token = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => current.push(c),
                        None => return Err("unbalanced '".to_string()),
                    }
                }
            }
            '"' => {
                has_token = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        // Inside double quotes only these four are special, as in POSIX sh.
                        Some('\\') => match chars.next() {
                            Some(c @ ('"' | '\\' | '$' | '`')) => current.push(c),
                            Some(c) => {
                                current.push('\\');
                                current.push(c);
                            }
                            None => return Err("unbalanced \"".to_string()),
                        },
                        Some(c) => current.push(c),
                        None => return Err("unbalanced \"".to_string()),
                    }
                }
            }
            c if c.is_whitespace() => {
                if has_token {
                    out.push(std::mem::take(&mut current));
                    has_token = false;
                }
            }
            c => {
                has_token = true;
                current.push(c);
            }
        }
    }
    if has_token {
        out.push(current);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values() -> Values {
        Values {
            url: "https://user:pw@example.com:8443/a/b?q=1#frag".to_string(),
            title: "A Page".to_string(),
            clipboard: Some("CLIP".to_string()),
            primary: Some("PRIM".to_string()),
        }
    }

    #[test]
    fn the_variables_qutebrowser_replaces() {
        let v = values();
        let sub = |text: &str| replace_variables_in(text, &v);
        assert_eq!(sub("{url}"), "https://user:pw@example.com:8443/a/b?q=1#frag");
        assert_eq!(sub("{url:pretty}"), v.url);
        assert_eq!(sub("{url:yank}"), v.url);
        assert_eq!(sub("{url:scheme}"), "https");
        assert_eq!(sub("{url:host}"), "example.com");
        assert_eq!(sub("{url:port}"), "8443");
        assert_eq!(sub("{url:domain}"), "https://example.com:8443");
        assert_eq!(sub("{url:path}"), "/a/b");
        assert_eq!(sub("{url:query}"), "q=1");
        assert_eq!(sub("{url:username}"), "user");
        assert_eq!(sub("{url:password}"), "pw");
        assert_eq!(sub("{url:auth}"), "user:pw@");
        assert_eq!(sub("{title}"), "A Page");
        assert_eq!(sub("{clipboard}"), "CLIP");
        assert_eq!(sub("{primary}"), "PRIM");
    }

    #[test]
    fn a_url_without_userinfo_or_port() {
        let v = Values { url: "http://example.com/".to_string(), ..Default::default() };
        assert_eq!(replace_variables_in("{url:host}", &v), "example.com");
        assert_eq!(replace_variables_in("{url:port}", &v), "");
        assert_eq!(replace_variables_in("{url:domain}", &v), "http://example.com");
        assert_eq!(replace_variables_in("{url:auth}", &v), "");
        assert_eq!(replace_variables_in("{url:path}", &v), "/");
        // An IPv6 literal: the colons inside the brackets are not a port.
        let v = Values { url: "http://[::1]/x".to_string(), ..Default::default() };
        assert_eq!(replace_variables_in("{url:host}", &v), "[::1]");
        assert_eq!(replace_variables_in("{url:port}", &v), "");
    }

    /// The reason substitution is one pass. A page picks its own address; if the output were
    /// scanned again, visiting `http://x/{clipboard}` would put the clipboard on a command line.
    #[test]
    fn a_substituted_value_is_never_substituted_again() {
        let v = Values {
            url: "http://evil.example/{clipboard}".to_string(),
            title: "{primary}".to_string(),
            clipboard: Some("hunter2".to_string()),
            primary: Some("secret".to_string()),
        };
        assert_eq!(replace_variables_in("{url}", &v), "http://evil.example/{clipboard}");
        assert_eq!(replace_variables_in("{title}", &v), "{primary}");
        assert!(!replace_variables_in("{url} {title}", &v).contains("hunter2"));
        assert!(!replace_variables_in("{url} {title}", &v).contains("secret"));
    }

    #[test]
    fn unknown_braces_are_left_alone() {
        let v = values();
        // `{hint-url}` is qutebrowser's, and bru's hint parser refuses the targets that produce it.
        assert_eq!(replace_variables_in("{hint-url}", &v), "{hint-url}");
        assert_eq!(replace_variables_in("{}", &v), "{}");
        assert_eq!(replace_variables_in("a { b", &v), "a { b");
        assert_eq!(replace_variables_in("{file}", &v), "{file}");
    }

    #[test]
    fn double_braces_escape() {
        let v = values();
        assert_eq!(replace_variables_in("{{url}}", &v), "{url}");
        assert_eq!(replace_variables_in("x {{title}} y", &v), "x {title} y");
    }

    #[test]
    fn substitution_in_context() {
        let v = values();
        assert_eq!(
            replace_variables_in("--url={url:host}--", &v),
            "--url=example.com--"
        );
        assert_eq!(replace_variables_in("{title}: {url:host}", &v), "A Page: example.com");
    }

    #[test]
    fn splitting_a_command_line() {
        assert_eq!(shlex("mpv --no-video x").unwrap(), ["mpv", "--no-video", "x"]);
        // The spelling in this user's qutebrowser config: a quoted argument with a space in it.
        assert_eq!(
            shlex(r#"qute-pass -U secret -u "login: (.+)" -d dmenu"#).unwrap(),
            ["qute-pass", "-U", "secret", "-u", "login: (.+)", "-d", "dmenu"]
        );
        assert_eq!(shlex("a 'b c' d").unwrap(), ["a", "b c", "d"]);
        assert_eq!(shlex(r"a b\ c").unwrap(), ["a", "b c"]);
        assert_eq!(shlex(r#""a\"b""#).unwrap(), [r#"a"b"#]);
        // Empty arguments survive, which `''` is the only way to write.
        assert_eq!(shlex("a '' b").unwrap(), ["a", "", "b"]);
        assert_eq!(shlex("   ").unwrap(), Vec::<String>::new());
        assert!(shlex("a \"b").is_err());
        assert!(shlex("a 'b").is_err());
        assert!(shlex(r"a b\").is_err());
    }

    /// The four variables this user's own userscripts read and bru can answer. Named one by one,
    /// because the point of the aliases is those five scripts and nothing more general.
    #[test]
    fn a_qutebrowser_userscript_finds_the_variables_it_reads() {
        let mut env = vec![
            ("BRU_FIFO".to_string(), "/run/user/1000/bru/userscript-1-0".to_string()),
            ("BRU_MODE".to_string(), "command".to_string()),
            ("BRU_VERSION".to_string(), "0.1.0".to_string()),
            ("BRU_CONFIG_DIR".to_string(), "/home/x/.config/bru".to_string()),
            ("BRU_DATA_DIR".to_string(), "/home/x/.local/share/bru".to_string()),
            ("BRU_URL".to_string(), "https://example.com/a".to_string()),
        ];
        alias_as_qute(&mut env);
        let get = |name: &str| {
            env.iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        };

        // qute-pass, ql-pass, qute-keepassxc, qutebrowser_dmenu, qutebrowser_tab_dmenu,
        // qutebrowser_translate — every one of them reads these two.
        assert_eq!(get("QUTE_FIFO").as_deref(), Some("/run/user/1000/bru/userscript-1-0"));
        assert_eq!(get("QUTE_URL").as_deref(), Some("https://example.com/a"));
        // The two dmenu scripts, and qute-keepassxc for its key file.
        assert_eq!(get("QUTE_DATA_DIR").as_deref(), Some("/home/x/.local/share/bru"));
        assert_eq!(get("QUTE_CONFIG_DIR").as_deref(), Some("/home/x/.config/bru"));
        assert_eq!(get("QUTE_MODE").as_deref(), Some("command"));

        // bru's own names are untouched: a bru userscript is not asked to learn qutebrowser's.
        assert_eq!(get("BRU_FIFO").as_deref(), Some("/run/user/1000/bru/userscript-1-0"));

        // The one deliberate omission. `QUTE_VERSION=0.1.0` would claim to be a qutebrowser
        // version; bru's own version is still there under its own name.
        assert_eq!(get("QUTE_VERSION"), None);
        assert_eq!(get("BRU_VERSION").as_deref(), Some("0.1.0"));

        // Nothing the page's DOM would be needed for is invented. `qutebrowser_translate --text`
        // reads this one and gets nothing, which is the honest answer — not an empty string
        // dressed up as a selection.
        assert_eq!(get("QUTE_SELECTED_TEXT"), None);
        assert_eq!(get("QUTE_SELECTED_HTML"), None);
        assert_eq!(get("QUTE_TEXT"), None);
        assert_eq!(get("QUTE_HTML"), None);
    }

    /// The alias list is derived, never written twice: an unrelated variable in the environment is
    /// not given a `QUTE_` twin, and neither is a `QUTE_` name given a second one.
    #[test]
    fn only_brus_own_variables_are_aliased() {
        let mut env = vec![
            ("BRU_TAB_INDEX".to_string(), "3".to_string()),
            ("EDITOR".to_string(), "nvim".to_string()),
            ("QUTE_URL".to_string(), "https://already".to_string()),
        ];
        alias_as_qute(&mut env);
        assert_eq!(env.len(), 4, "one alias added, and only one");
        assert!(env.contains(&("QUTE_TAB_INDEX".to_string(), "3".to_string())));
        assert!(!env.iter().any(|(key, _)| key == "QUTE_EDITOR"));
        assert!(!env.iter().any(|(key, _)| key == "QUTE_QUTE_URL"));
    }

    #[test]
    fn tilde_expansion() {
        let home = std::ffi::OsStr::new("/home/tester");
        let expand = |path: &str| expand_user_with(path, Some(home));
        assert_eq!(expand("~/bin/x"), PathBuf::from("/home/tester/bin/x"));
        assert_eq!(expand("~"), PathBuf::from("/home/tester"));
        assert_eq!(expand("/abs/x"), PathBuf::from("/abs/x"));
        assert_eq!(expand("relative"), PathBuf::from("relative"));
        assert_eq!(expand("~other/x"), PathBuf::from("~other/x"));
        // No `$HOME`: nothing is expanded, and nothing panics.
        assert_eq!(expand_user_with("~/x", None), PathBuf::from("~/x"));
    }
}
