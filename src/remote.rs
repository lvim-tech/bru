//! The one door into a running bru from outside it.
//!
//! ```sh
//! bru --remote :open -t https://example.com     # a command, exactly as typed at `:`
//! bru --remote ping                             # is one running?
//! bru --remote tabs                             # what has it got open?
//! bru --remote js 0 'document.title'            # ask a page something
//! ```
//!
//! ## Why it exists, which is not the reason it was first asked for
//!
//! It came up because `themer` writes `~/.config/bru/theme.css` and had to tell bru. That is **not**
//! what justifies it and bru does not use it for that: `theme_watch.rs` watches the file with
//! `inotify`, which asks nothing of the writer — a `cp` by hand works as well as `themer` does.
//! Announcing a file change over a socket would have made every writer learn about the reader.
//!
//! What justifies it is that a browser you cannot speak to is a browser you cannot script, and two
//! plugins in this ecosystem already have the shape that needs it:
//!
//! - **`lvim-preview`** opens a browser at a URL it serves. Its `browser` option takes an argv list,
//!   and the URL is appended to it — so `{ "bru", "--remote", ":open", "-t" }` opens the preview in
//!   the browser that is already running instead of starting a second one. That is the whole of what
//!   it needs, and it needs it in one direction: the page talks back over `lvim-preview`'s own
//!   WebSocket, not through here.
//! - **`lvim-tex`** drives a PDF viewer through a seam every one of its backends implements —
//!   `available`, `open`, `is_alive`, `forward`, `position`, `close`. Three of those are **questions**,
//!   and `zathura.lua` answers them with flags while `preview.lua` answers them over a socket of its
//!   own. For bru to be a backend, this has to answer questions and not only take orders, which is
//!   why [`Reply`] carries lines and not just a word.
//!
//! The way back — the viewer telling the editor where to jump — needs nothing here. It is `:spawn`,
//! which bru already has, and it is exactly what `zathura`'s `--synctex-editor-command` does.
//!
//! ## The protocol
//!
//! One line in, one reply out, and a connection may carry several of each. The reply is a status
//! line, then any data lines, then a lone `.` — SMTP's shape, because it is the one a shell and a
//! Lua `uv.read_start` can both parse without a library:
//!
//! ```text
//! ok
//! 0\t0\t1\thttps://example.com/\tExample Domain
//! .
//! ```
//!
//! A failure is `error <why>` and then `.`. Data lines are tab-separated; a tab inside a value is
//! replaced by a space, because a field separator that can appear in a field is not one.
//!
//! ## What it is not
//!
//! **Not a security boundary.** The socket lives in `$XDG_RUNTIME_DIR`, which the kernel gives this
//! user alone at mode 0700, and the socket itself is 0600 — but within that, *anything that can
//! write it can drive the browser*, including `:spawn`, which starts programs. That is the same
//! promise qutebrowser's IPC makes, and it is worth saying rather than implying: the boundary is the
//! user account and there is no second one inside it.
//!
//! **`ok` on a command means accepted, not done.** Commands run from a posted task, because one that
//! opens a tab may not run inside the caller's stack (CEF-NOTES trap 12). `js` is the exception: it
//! waits for the page's answer, which is the only reason it can be a question at all.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

/// How long `js` waits for a page. A page that has not answered in this long is a page in a loop,
/// and a caller blocked for ever is worse than one told the truth.
const JS_TIMEOUT: Duration = Duration::from_secs(5);

/// `$XDG_RUNTIME_DIR/bru/ipc.sock`, or `None` where there is no runtime directory.
///
/// **One socket for the machine, not one per process.** A caller typing `bru --remote :open x` wants
/// the browser, not a browser, and a path with a pid in it would make every caller find the pid
/// first. A second bru finds the name taken, says so once, and runs without a socket — which is the
/// honest behaviour: two browsers and one address means one of them answers.
///
/// **`--socket=<path>` overrides it, and that switch exists because the default cost real damage.**
/// Measured 2026-08-07: a second bru was started to test a build, found the name taken, ran without
/// a socket exactly as designed — and every `--remote` in the session went to the *first* browser,
/// which was the user's. It changed their settings and navigated their tab, and nothing in either
/// process was wrong. The default is right for the common case and there was no way to say "not
/// that one"; now there is. Overriding `$XDG_RUNTIME_DIR` instead is not the workaround it looks
/// like — the Wayland display socket lives there too, so a private runtime directory starts a
/// browser that cannot open a window.
///
/// `$XDG_RUNTIME_DIR` rather than `/tmp`: it is mode 0700, owned by this user, and cleared by the
/// session rather than by anybody. A socket in `/tmp` is one whose name every account can see.
pub fn socket_path() -> Option<PathBuf> {
    let args: Vec<String> = std::env::args().collect();
    socket_from(&args, std::env::var_os("XDG_RUNTIME_DIR").as_deref())
}

/// The switch that names a different socket: `--socket=<path>`.
pub const SOCKET_SWITCH: &str = "--socket=";

/// [`socket_path`] as a function of its inputs, so it can be tested.
///
/// **`--socket=` is read only from the arguments BEFORE `--remote`,** and that is not tidiness.
/// `--remote` takes the whole rest of the line as the message — that is what makes
/// `bru --remote :open -t https://x` work — so anything after it is text, not a switch, and a URL
/// with `--socket=` in its query string is a URL. The spelling is therefore
/// `bru --socket=/tmp/x --remote :open https://y`, switch first.
///
/// The last one wins, which is Chromium's rule for its own switches and the one a person who
/// appends to a wrapper script expects.
///
/// **A named socket gives up the directory's protection, and only the directory's.** The default
/// sits in `$XDG_RUNTIME_DIR`, which the kernel gives this user alone at mode 0700; a path in
/// `/tmp` sits somewhere every account can list. What does not change is the socket itself —
/// `listen` chmods it 0600, and the module header already says that is the belt and the directory
/// is the braces. So `--socket=/tmp/x.sock` is fine for a test browser and is not the place to put
/// the one holding a logged-in session.
fn socket_from(args: &[String], runtime_dir: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    let before = args.iter().position(|arg| arg == "--remote").unwrap_or(args.len());
    let named = args[..before]
        .iter()
        .filter_map(|arg| arg.strip_prefix(SOCKET_SWITCH))
        .rfind(|path| !path.is_empty());
    if let Some(path) = named {
        return Some(PathBuf::from(path));
    }
    let dir = runtime_dir.filter(|dir| !dir.is_empty())?;
    Some(PathBuf::from(dir).join("bru").join("ipc.sock"))
}

// -----------------------------------------------------------------------------------------------
// The caller's side
// -----------------------------------------------------------------------------------------------

/// Send one line and print the reply. `bru --remote <line…>`.
///
/// The whole rest of the command line is the line, joined with spaces, because that is the shape
/// `lvim-preview`'s `browser` option produces: an argv list with the URL appended. Taking a single
/// argument would have made `bru --remote ':open -t' <url>` the only spelling that worked, and it is
/// the one nobody writes.
pub fn send(line: &str) -> Result<(), String> {
    let Some(path) = socket_path() else {
        return Err("no $XDG_RUNTIME_DIR, so there is nowhere for the socket to be".to_string());
    };
    let stream = UnixStream::connect(&path)
        .map_err(|e| format!("no bru is listening on {}: {e}", path.display()))?;
    stream
        .set_read_timeout(Some(JS_TIMEOUT + Duration::from_secs(2)))
        .map_err(|e| format!("could not set a timeout: {e}"))?;
    let mut writer = stream.try_clone().map_err(|e| format!("{e}"))?;
    // One line, and the newline is the frame. A command with a newline in it is not a command.
    writeln!(writer, "{}", line.replace('\n', " ")).map_err(|e| format!("could not send: {e}"))?;
    writer.flush().map_err(|e| format!("could not send: {e}"))?;

    let mut reader = BufReader::new(stream);
    let mut status = String::new();
    reader
        .read_line(&mut status)
        .map_err(|e| format!("no answer: {e}"))?;
    let status = status.trim().to_string();

    // The data lines, up to the lone `.`. Printed rather than returned: this is a command-line tool
    // for the length of one call and its output is somebody's pipe.
    loop {
        let mut data = String::new();
        if reader.read_line(&mut data).map_err(|e| format!("{e}"))? == 0 {
            break;
        }
        if data.trim_end_matches(['\r', '\n']) == "." {
            break;
        }
        print!("{data}");
    }
    match status.strip_prefix("error ") {
        Some(why) => Err(why.to_string()),
        None => Ok(()),
    }
}

// -----------------------------------------------------------------------------------------------
// The browser's side
// -----------------------------------------------------------------------------------------------

/// Bind the socket and answer on it, on a thread of its own.
///
/// Called from `app.rs` once the UI thread exists — a command has nowhere to be posted before that.
/// A failure to bind is a line and nothing else: a browser without a remote is a browser, and one
/// that refused to start because a socket was taken would be worse than one that cannot be scripted.
pub fn listen() {
    let Some(path) = socket_path() else {
        return;
    };
    let Some(dir) = path.parent().map(PathBuf::from) else {
        return;
    };
    // `$XDG_RUNTIME_DIR/bru/`, which is bru's own. Not `~/.config/bru/`, the directory the browser
    // may not create: a runtime socket is state, not configuration.
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("bru[remote]: no {}: {e}", dir.display());
        return;
    }

    let listener = match bind(&path) {
        Ok(listener) => listener,
        Err(why) => {
            eprintln!(
                "bru[remote]: {why} — this browser has no remote, and every `--remote` on this \
                 machine will reach the one that has it. Start this browser with \
                 `{SOCKET_SWITCH}<path>` and pass the same switch to `--remote` to script this one."
            );
            return;
        }
    };
    // 0600 on top of the directory's 0700. Belt and braces, and the belt is the one that matters: a
    // socket is created with the process umask, and a umask of 0 would leave it open to this user's
    // group.
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    eprintln!("bru[remote]: listening on {}", path.display());

    std::thread::Builder::new()
        .name("bru-remote".to_string())
        .spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    // One connection at a time, on this thread. A caller that holds the socket open
                    // holds up the next one — which is a real limit and the right trade here: the
                    // callers are an editor plugin and a shell, the work per line is a posted task
                    // or a five-second `js`, and a thread per connection would be a thread per
                    // keystroke of somebody's `while read` loop.
                    Ok(stream) => serve(stream),
                    Err(e) => eprintln!("bru[remote]: {e}"),
                }
            }
        })
        .map(|_| ())
        .unwrap_or_else(|e| eprintln!("bru[remote]: could not start the listener: {e}"));
}

/// Bind, taking the name from a bru that is no longer there.
///
/// A Unix socket is a file, and a process that died without unlinking it leaves the name bound to
/// nothing. Telling the two apart is a connect: if something answers, the address really is in use;
/// if nothing does, the file is a corpse and removing it is safe. Doing it the other way round —
/// unlink first, bind after — would take the socket out from under a browser that was using it.
fn bind(path: &std::path::Path) -> Result<UnixListener, String> {
    match UnixListener::bind(path) {
        Ok(listener) => return Ok(listener),
        Err(e) if e.kind() != std::io::ErrorKind::AddrInUse => {
            return Err(format!("could not bind {}: {e}", path.display()));
        }
        Err(_) => {}
    }
    if UnixStream::connect(path).is_ok() {
        return Err(format!("another bru is already listening on {}", path.display()));
    }
    std::fs::remove_file(path)
        .map_err(|e| format!("could not clear the stale socket {}: {e}", path.display()))?;
    UnixListener::bind(path).map_err(|e| format!("could not bind {}: {e}", path.display()))
}

/// What a verb answers with: a status and any number of data lines.
struct Reply {
    status: String,
    lines: Vec<String>,
}

impl Reply {
    fn ok() -> Self {
        Reply { status: "ok".to_string(), lines: Vec::new() }
    }
    fn with(lines: Vec<String>) -> Self {
        Reply { status: "ok".to_string(), lines }
    }
    fn error(why: impl std::fmt::Display) -> Self {
        Reply { status: format!("error {why}"), lines: Vec::new() }
    }
}

fn serve(stream: UnixStream) {
    let Ok(clone) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(clone);
    let mut writer = stream;
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let reply = answer(line.trim());
        if writeln!(writer, "{}", reply.status).is_err() {
            return;
        }
        for line in reply.lines {
            // The field separator may not appear in a field.
            if writeln!(writer, "{}", line.replace('\n', " ")).is_err() {
                return;
            }
        }
        if writeln!(writer, ".").is_err() || writer.flush().is_err() {
            return;
        }
    }
}

/// One request.
fn answer(line: &str) -> Reply {
    let line = line.trim();
    if line.is_empty() {
        return Reply::error("the line was empty");
    }
    let (verb, rest) = match line.split_once(char::is_whitespace) {
        Some((verb, rest)) => (verb, rest.trim()),
        None => (line, ""),
    };
    match verb {
        // Is one running, and which. `lvim-tex`'s `available` and half of its `is_alive`.
        "ping" => Reply::with(vec![format!("bru\t{}", env!("CARGO_PKG_VERSION"))]),
        // Every tab of every window: window, index, whether it is the one showing, url, title.
        // The other half of `is_alive` — a viewer asks whether *its* URL is still open.
        "tabs" => tabs(),
        "windows" => windows(),
        // A setting's value, so a caller does not have to guess what bru is configured to do.
        "get" if !rest.is_empty() => match crate::settings::value_of(rest) {
            Some(value) => Reply::with(vec![value]),
            None => Reply::error(format!("{rest} is not a setting bru answers for")),
        },
        "get" => Reply::error("get needs a setting's name"),
        // Evaluate in one tab and answer with what it came to. This is the primitive `lvim-tex`'s
        // `forward` and `position` are both written in terms of: scrolling a PDF page to a line and
        // reading back where it is are both a line of JavaScript in the tab showing it.
        "js" => js(rest),
        // Everything else is a command line, with or without its colon — `bru://chrome/help` writes
        // them with one and a caller typing from memory will not.
        _ => command(line.trim_start_matches(':').trim()),
    }
}

fn tabs() -> Reply {
    let Some(state) = crate::state::BruState::instance() else {
        return Reply::error("no browser state");
    };
    let Ok(state) = state.lock() else {
        return Reply::error("the state is locked");
    };
    let mut out = Vec::new();
    for window in state.window_ids() {
        let active = state.active_tab_in(window);
        for (index, tab) in state.tabs_in(window).iter().enumerate() {
            out.push(format!(
                "{window}\t{index}\t{}\t{}\t{}",
                i32::from(index == active),
                tab.0.replace('\t', " "),
                tab.1.replace('\t', " "),
            ));
        }
    }
    Reply::with(out)
}

fn windows() -> Reply {
    let Some(state) = crate::state::BruState::instance() else {
        return Reply::error("no browser state");
    };
    let Ok(state) = state.lock() else {
        return Reply::error("the state is locked");
    };
    let out = state
        .window_ids()
        .into_iter()
        .map(|window| {
            format!("{window}\t{}\t{}", state.tab_count_in(window), state.active_tab_in(window))
        })
        .collect();
    Reply::with(out)
}

/// `js <tab> <code…>` — evaluate in that tab of the current window and wait for the answer.
///
/// **The one verb that blocks**, and it is what makes this a question interface rather than an order
/// interface. The evaluation has to happen on the UI thread and the answer comes back on it too, so
/// this posts a task and waits on a channel with a timeout — a page in a loop leaves the caller told
/// rather than hanging.
fn js(rest: &str) -> Reply {
    let Some((index, code)) = rest.split_once(char::is_whitespace) else {
        return Reply::error("js needs a tab index and some code");
    };
    let Ok(index) = index.trim().parse::<usize>() else {
        return Reply::error("js's first argument is a tab index");
    };
    let code = code.trim().to_string();
    if code.is_empty() {
        return Reply::error("js needs some code");
    }

    let (send, recv) = mpsc::channel::<Result<String, String>>();
    let mut task = Evaluate::new(index, code, send);
    post_task(ThreadId::UI, Some(&mut task));
    match recv.recv_timeout(JS_TIMEOUT) {
        Ok(Ok(answer)) => Reply::with(vec![answer]),
        Ok(Err(why)) => Reply::error(why),
        Err(_) => Reply::error("the page did not answer in five seconds"),
    }
}

fn command(line: &str) -> Reply {
    if line.is_empty() {
        return Reply::error("the line was empty");
    }
    // **Parsed here, run there.** Parsing is pure and "that is not a command" is the one useful
    // thing this can say at once; running is a posted task, because a command that opens a tab may
    // not run inside a caller's stack (CEF-NOTES trap 12).
    match crate::commands::parse(line) {
        // **Parsing is not enough**, and that is not a nicety: `commands::parse` answers
        // `Command::Unimplemented` for a name it does not know, because the binding trie has to keep
        // its shape whether or not bru implements what a key names. So `:nosuchcommand` parsed, ran
        // nothing, and this said `ok` — a caller checking the exit code was told a lie. `is_live` is
        // the same question `bru://chrome/help` asks of every row.
        Ok(command) if !crate::exec::is_live(&command) => {
            Reply::error(format!("{line:?} is not a command bru runs"))
        }
        Ok(_) => {
            crate::exec::run_from_cmdline(line, None);
            Reply::ok()
        }
        Err(why) => Reply::error(why),
    }
}

use cef::*;

wrap_task! {
    struct Evaluate {
        index: usize,
        code: String,
        answer: mpsc::Sender<Result<String, String>>,
    }

    impl Task {
        fn execute(&self) {
            debug_assert_ne!(currently_on(ThreadId::UI), 0);
            let browser = crate::state::BruState::instance().and_then(|state| {
                state.lock().ok().and_then(|mut state| {
                    let window = state.current_window_id()?;
                    let id = (*state.tab_browser_ids_in(window).get(self.index)?)?;
                    state.browser_with_id(id)
                })
            });
            let Some(mut browser) = browser else {
                let _ = self.answer.send(Err(format!("there is no tab {}", self.index)));
                return;
            };
            let answer = self.answer.clone();
            crate::editor::ask(&mut browser, &self.code, move |value| {
                let _ = answer.send(Ok(value));
            });
        }
    }
}

/// Take the socket file away on the way out, so the next bru binds rather than clearing a corpse.
///
/// Best effort by design: a bru that was killed never reaches this, which is exactly the case
/// [`bind`] handles.
pub fn cleanup() {
    if let Some(path) = socket_path() {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod socket_tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|a| a.to_string()).collect()
    }

    fn rt(dir: &str) -> std::ffi::OsString {
        std::ffi::OsString::from(dir)
    }

    /// The default is the one address on the machine, and nothing about it changed.
    #[test]
    fn without_the_switch_it_is_the_runtime_directory() {
        let path = socket_from(&args(&["bru"]), Some(&rt("/run/user/1000"))).expect("a path");
        assert_eq!(path, PathBuf::from("/run/user/1000/bru/ipc.sock"));
    }

    /// No runtime directory is no socket, rather than a socket somewhere every account can see.
    #[test]
    fn without_a_runtime_directory_there_is_nowhere_for_it() {
        assert_eq!(socket_from(&args(&["bru"]), None), None);
        assert_eq!(socket_from(&args(&["bru"]), Some(&rt(""))), None);
    }

    /// The switch names another socket, which is the whole point of it.
    #[test]
    fn the_switch_wins_over_the_default() {
        let path = socket_from(&args(&["bru", "--socket=/tmp/a.sock"]), Some(&rt("/run/user/1000")))
            .expect("a path");
        assert_eq!(path, PathBuf::from("/tmp/a.sock"));
    }

    /// It works with no runtime directory at all: naming the socket is naming it.
    #[test]
    fn the_switch_does_not_need_a_runtime_directory() {
        let path = socket_from(&args(&["bru", "--socket=/tmp/a.sock"]), None).expect("a path");
        assert_eq!(path, PathBuf::from("/tmp/a.sock"));
    }

    /// **The trap this switch is most likely to fall into.** `--remote` takes the rest of the line
    /// as its message, so a `--socket=` after it is text — part of a command, or a URL's query
    /// string — and reading it as a switch would send the message somewhere nobody asked for.
    #[test]
    fn a_socket_after_remote_is_part_of_the_message() {
        let line = args(&["bru", "--remote", ":open", "https://x/?q=--socket=/tmp/evil.sock"]);
        let path = socket_from(&line, Some(&rt("/run/user/1000"))).expect("a path");
        assert_eq!(path, PathBuf::from("/run/user/1000/bru/ipc.sock"));
    }

    /// Before `--remote` it is a switch, which is the spelling the doc comment prescribes.
    #[test]
    fn a_socket_before_remote_is_the_switch() {
        let line = args(&["bru", "--socket=/tmp/a.sock", "--remote", ":open", "https://x"]);
        assert_eq!(socket_from(&line, None), Some(PathBuf::from("/tmp/a.sock")));
    }

    /// Last wins, so appending to a wrapper script overrides what the script already passed.
    #[test]
    fn the_last_switch_wins() {
        let line = args(&["bru", "--socket=/tmp/a.sock", "--socket=/tmp/b.sock"]);
        assert_eq!(socket_from(&line, None), Some(PathBuf::from("/tmp/b.sock")));
    }

    /// An empty value is not a path. Falling through to the default beats binding to "".
    #[test]
    fn an_empty_switch_is_ignored() {
        let path = socket_from(&args(&["bru", "--socket="]), Some(&rt("/run/user/1000")));
        assert_eq!(path, Some(PathBuf::from("/run/user/1000/bru/ipc.sock")));
    }
}
