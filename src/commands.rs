//! Commands, and the parser that turns qutebrowser command strings into them.
//!
//! Bindings are written the way qutebrowser writes them — `scroll down`, `tab-next`,
//! `cmd-set-text -s :open` — because DESIGN.md settles that the command names stay identical.
//! Parsing happens once, at startup, when the binding table is built; a keypress looks up an
//! already-parsed [`Command`] and never sees a string.
//!
//! Not every command qutebrowser has is implemented in bru yet. Rather than dropping those
//! bindings, which would change the *shape* of the trie and make `;b` report NoMatch on the `;`
//! instead of PartialMatch, an unrecognised name parses to [`Command::Unimplemented`]. The binding
//! stays, the chain still resolves, and `src/config.rs` warns once at startup about how many there
//! were. A keypress never produces a parse error.
//!
//! Nothing in this file touches CEF or Lua.

use crate::modes::Mode;
use std::fmt;

/// A command bru can run.
///
/// Variants exist for what bru implements or is about to; everything else in qutebrowser's default
/// bindings lands in [`Command::Unimplemented`] with its original string.
#[derive(Clone, PartialEq, Debug)]
pub enum Command {
    /// `a ;; b ;; c` — run each in turn.
    Chain(Vec<Command>),
    /// `nop` — bound to `<Ctrl-Shift-Tab>` in normal mode purely to shadow the browser default.
    Nop,
    /// `clear-keychain`
    ClearKeychain,
    /// `mode-enter <mode>`
    ModeEnter(Mode),
    /// `mode-leave`
    ModeLeave,

    /// `scroll <direction>`. The reason bru exists: this goes through `send_mouse_wheel_event`.
    Scroll(ScrollDirection),
    /// `scroll-px <dx> <dy>`
    ScrollPx { dx: i32, dy: i32 },
    /// `scroll-page <x> <y>` — pages, not pixels; the count multiplies.
    ScrollPage { x: f64, y: f64 },
    /// `scroll-to-perc [perc] [-x]`. No percentage means the end of the page.
    ScrollToPerc { perc: Option<f64>, horizontal: bool },

    /// `tab-next`
    TabNext,
    /// `tab-prev`
    TabPrev,
    /// `tab-close [-o] [-f]`
    TabClose { opposite: bool, force: bool },
    /// `tab-only [-f]`
    TabOnly { force: bool },
    /// `tab-focus [index]`
    TabFocus { index: Option<TabIndex> },
    /// `tab-move [+|-|start|end|index]`
    TabMove { to: TabMove },
    /// `tab-clone [-b|-w|-p]`
    TabClone { bg: bool, window: bool, private: bool },
    /// `undo [-w]` — reopen the last closed tab.
    Undo { window: bool },

    /// `open [-t|-b|-w] [-p] [-r] [--] [url]`
    Open {
        url: Option<String>,
        tab: bool,
        bg: bool,
        window: bool,
        private: bool,
        related: bool,
    },
    /// `back [-t|-b|-w]`
    Back { tab: bool, bg: bool, window: bool },
    /// `forward [-t|-b|-w]`
    Forward { tab: bool, bg: bool, window: bool },
    /// `reload [-f]`
    Reload { force: bool },
    /// `stop`
    Stop,
    /// `home`
    Home,
    /// `quit [--save]`
    Quit { save: bool },
    /// `close` — this window, not the application.
    Close,

    /// `zoom [level]` — no level means the default, 100%.
    Zoom { level: Option<u32> },
    /// `zoom-in`
    ZoomIn,
    /// `zoom-out`
    ZoomOut,
    /// `fullscreen [--enter|--leave]`
    Fullscreen { enter: bool, leave: bool },

    /// `hint [group] [target]` — draw labels over the page and follow the one that is typed.
    ///
    /// `f` is a bare `hint`, `F` is `hint all tab`. The group and the targets bru does not
    /// implement yet parse into [`Command::Unimplemented`] rather than into a variant that would
    /// silently do the wrong thing.
    Hint { target: HintTarget },
    /// `hint-follow` — the `<Return>` binding in hint mode.
    HintFollow,

    /// `help [-t]` — bru's own key and command reference, generated from the live binding table.
    Help { tab: bool },

// --- src/downloads.rs --------------------------------------------------------------------------
    /// `download [url]` — `gd`. No URL means the page that is showing.
    Download { url: Option<String> },
    /// `download-cancel [--all]` — `ad`. A count picks which one; none means the last.
    DownloadCancel { all: bool },
    /// `download-clear` — `cd`. Forgets the finished ones; touches no file.
    DownloadClear,
    /// `download-open [cmdline] [-d]` — open the finished download, or its directory.
    DownloadOpen { cmdline: Option<String>, dir: bool },
    /// `download-delete` — remove the file from disk and the row from the list.
    DownloadDelete,
    /// `download-retry` — start a failed download again.
    DownloadRetry,
// --- end src/downloads.rs ----------------------------------------------------------------------

// --- src/history.rs --------------------------------------------------------
    /// `quickmark-save [name]` — `m`. With no name the command line is prefilled instead; see the
    /// arm in `exec.rs` for why bru does not prompt.
    QuickmarkSave { name: Option<String> },
    /// `quickmark-load [-t|-b|-w] <name>` — what `b`, `B` and `wb` prefill.
    QuickmarkLoad { name: Option<String>, tab: bool, bg: bool, window: bool },
    /// `quickmark-del [name]` — no name means the quickmark on the current page.
    QuickmarkDel { name: Option<String> },
    /// `bookmark-add [url] [title] [--toggle]` — `M`, which passes neither.
    BookmarkAdd { url: Option<String>, title: Option<String>, toggle: bool },
    /// `bookmark-load [-t|-b|-w] [-d] <url>` — what `gb`, `gB` and `wB` prefill.
    BookmarkLoad { url: Option<String>, tab: bool, bg: bool, window: bool, delete: bool },
    /// `bookmark-del [url]` — no URL means the current page.
    BookmarkDel { url: Option<String> },
    /// `bookmark-list [--jump] [-b]` — `Sq` and `Sb`. Always a new tab, as in qutebrowser
    /// (`commands.py:1347`, where `tab` defaults to True); `--jump` lands on the bookmarks heading.
    BookmarkList { jump: bool, bg: bool },
    /// `history [-b]` — `Sh`. A new tab for the same reason (`commands.py:1450`).
    History { bg: bool },
// --- end src/history.rs ----------------------------------------------------

// --- src/clip.rs -----------------------------------------------------------
    /// `yank [what] [-s]` — `yy`, `yY`, `yt`, `yT`, `yd`, `yD`, `yp`, `yP`, `ym`, `yM`.
    ///
    /// `sel` is `-s`/`--sel` and means the **primary selection**, not the clipboard.
    Yank { what: YankWhat, sel: bool },
// --- end src/clip.rs -------------------------------------------------------

// --- src/find.rs + src/navigate.rs ---------------------------------------------------------------
    /// `search [-r] [text]` — `/text`, and `?text` with `-r`. No text clears the search, which is
    /// what `<Escape>`'s `clear-keychain ;; search ;; fullscreen --leave` relies on.
    Search { text: String, reverse: bool },
    /// `search-next` — `n`, continuing in the direction the search was started in.
    SearchNext,
    /// `search-prev` — `N`.
    SearchPrev,
    /// `navigate <where> [-t|-b|-w]` — `[[`, `]]`, `{{`, `}}`, `gu`, `gU`, `<Ctrl-A>`, `<Ctrl-X>`.
    Navigate { to: NavigateTo, tab: bool, bg: bool, window: bool },
// --- end src/find.rs + src/navigate.rs ------------------------------------------------------------

    /// `cmd-set-text [-s] [-a] [-r] <text>` — the machinery behind `o`, `O`, `go`, `b`, `T`, …
    CmdSetText { text: String, space: bool, append: bool, run_on_count: bool },
    /// `command-accept [--rapid]`
    CommandAccept { rapid: bool },

// --- src/spawn.rs, src/editor.rs -----------------------------------------------------------
    /// `spawn [-u] [-d] [-m] [-v] cmd [args…]` — run a program, optionally as a userscript.
    ///
    /// `cmdline` is the whole rest of the line, unsplit: `spawn` is a `maxsplit=0` command and
    /// `src/spawn.rs` splits it with its own `shlex`, because the quotes have to survive
    /// (`spawn -u qute-pass -u "login: (.+)"` is three arguments, not four).
    Spawn { cmdline: String, userscript: bool, detach: bool, messages: bool, verbose: bool },
    /// `edit-text` (`<Ctrl-E>` in insert mode), and `open-editor`, its pre-1.0 spelling.
    EditText,
    /// `insert-text <text>` — `<Shift-Ins>` is `insert-text -- {primary}`.
    InsertText { text: String },
    /// `fake-key <keystring>` — `<Shift-Escape>` in insert mode is `fake-key <Escape>`.
    FakeKey { keystring: String },
// --- end src/spawn.rs, src/editor.rs -------------------------------------------------------

    /// A command qutebrowser has and bru does not implement yet, kept verbatim so the binding
    /// still occupies its place in the trie.
    Unimplemented(String),
}

/// The argument of `scroll`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ScrollDirection {
    Up,
    Down,
    Left,
    Right,
    Top,
    Bottom,
    PageUp,
    PageDown,
}

/// The `hint` targets M12 implements. `hints.Target` has sixteen; the rest — yank, download,
/// userscript, fill, hover, … — arrive with the commands they depend on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HintTarget {
    /// `hint` (no target): click the element where it is.
    Normal,
    /// `hint all tab` / `hint all tab-bg`: open the element's URL in a background tab.
    ///
    /// `tab` and `tab-bg` differ by `tabs.background`, which bru has no setting for yet; both open
    /// in the background, which is `tabs.background = true` and is what `F` does here today.
    TabBg,
}

// --- src/clip.rs -----------------------------------------------------------
/// The `what` of `yank` (`commands.py:710`).
///
/// `selection` — the caret-mode one — is deliberately absent: bru has no caret mode, and a `yank
/// selection` that quietly yanked the URL instead would be worse than an unimplemented binding.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum YankWhat {
    /// `url`, the default: the address fully encoded, without a password or a tracking parameter.
    Url,
    /// `pretty-url`: the same address with its spaces and its Unicode readable.
    PrettyUrl,
    /// `title`: the page's title.
    Title,
    /// `domain`: scheme, host, and the port when the URL states one.
    Domain,
    /// `inline <text>`: the text itself, with `{title}` and `{url:yank}` filled in when it runs.
    Inline(String),
}
// --- end src/clip.rs -------------------------------------------------------

// --- src/navigate.rs ------------------------------------------------------------------------------
/// The argument of `navigate`. `commands.py:607` names all six and refuses anything else, so an
/// unknown destination is a parse error rather than a binding that quietly does nothing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NavigateTo {
    /// A "previous page" link on the page.
    Prev,
    /// A "next page" link on the page.
    Next,
    /// One segment up the URL's path.
    Up,
    /// The last number in the URL, plus the count.
    Increment,
    /// The last number in the URL, minus the count.
    Decrement,
    /// The URL without its query and fragment.
    Strip,
}
// --- end src/navigate.rs --------------------------------------------------------------------------

/// The argument of `tab-focus`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TabIndex {
    /// 1-based; negative counts from the end, so -1 is the last tab.
    Number(i32),
    /// The previously focused tab.
    Last,
}

/// The argument of `tab-move`.
///
/// `commands.py:1025-1065`: `+`/`-` move by [count] places from where the tab is now; everything
/// else is an absolute destination, and a [count] overrides it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TabMove {
    /// `+` (1) or `-` (-1) — the direction; how far is the count's business.
    Relative(i32),
    /// 1-based; negative counts from the end.
    Index(i32),
    Start,
    End,
}

impl Command {
    /// Whether this command (and every link of a chain) does something.
    pub fn is_implemented(&self) -> bool {
        match self {
            Command::Unimplemented(_) => false,
            Command::Chain(parts) => parts.iter().all(Command::is_implemented),
            _ => true,
        }
    }
}

/// A command string that could not be parsed at all — a known command with an argument that makes
/// no sense, or an empty string. An *unknown* command is not an error; it becomes
/// [`Command::Unimplemented`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ParseError(pub String);

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Parse a qutebrowser command string.
///
/// Handles `;;` chaining, quoted arguments (`rl-rubout " "`), `--` as end-of-flags, and the short
/// and long flags the implemented commands take.
pub fn parse(s: &str) -> Result<Command, ParseError> {
    let parts = split_chain(s);
    if parts.len() > 1 {
        let mut out = Vec::with_capacity(parts.len());
        for part in parts {
            out.push(parse_one(&part)?);
        }
        return Ok(Command::Chain(out));
    }
    parse_one(parts.first().map(String::as_str).unwrap_or(""))
}

/// Split on `;;`, ignoring separators inside quotes.
fn split_chain(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                current.push(c);
                if c == q {
                    quote = None;
                }
            }
            None => {
                if c == '"' || c == '\'' {
                    quote = Some(c);
                    current.push(c);
                } else if c == ';' && chars.peek() == Some(&';') {
                    chars.next();
                    parts.push(current.trim().to_string());
                    current = String::new();
                } else {
                    current.push(c);
                }
            }
        }
    }
    parts.push(current.trim().to_string());
    parts.retain(|p| !p.is_empty());
    if parts.is_empty() {
        parts.push(String::new());
    }
    parts
}

/// Split a single command into tokens, honouring `"` and `'`.
fn tokenize(s: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut has_token = false;
    let mut quote: Option<char> = None;
    for c in s.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => current.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                has_token = true;
            }
            None if c.is_whitespace() => {
                if has_token {
                    tokens.push(std::mem::take(&mut current));
                    has_token = false;
                }
            }
            None => {
                current.push(c);
                has_token = true;
            }
        }
    }
    if has_token {
        tokens.push(current);
    }
    tokens
}

/// Flags and positional arguments, separated the way qutebrowser's argparse does it.
struct Args {
    flags: Vec<String>,
    positional: Vec<String>,
}

impl Args {
    /// Split tokens into flags and positionals.
    ///
    /// A token is a flag if it starts with `-` and is neither a bare `-` (which `tab-move` takes
    /// as an argument) nor a negative number (`scroll-page 0 -1`). `--` ends flag parsing.
    fn new(tokens: &[String]) -> Args {
        let mut flags = Vec::new();
        let mut positional = Vec::new();
        let mut end_of_flags = false;
        for token in tokens {
            if end_of_flags {
                positional.push(token.clone());
                continue;
            }
            if token == "--" {
                end_of_flags = true;
                continue;
            }
            if is_flag(token) {
                if let Some(long) = token.strip_prefix("--") {
                    flags.push(long.to_string());
                } else {
                    // `-tb` is two short flags, as in argparse.
                    for c in token[1..].chars() {
                        flags.push(c.to_string());
                    }
                }
            } else {
                positional.push(token.clone());
            }
        }
        Args { flags, positional }
    }

    fn has(&self, flag: &str) -> bool {
        self.flags.iter().any(|f| f == flag)
    }

    /// A short flag or its long spelling; qutebrowser accepts both (`-t` / `--tab`).
    fn any(&self, names: &[&str]) -> bool {
        names.iter().any(|n| self.has(n))
    }

    /// Flags, then everything from the first non-flag token on as a single verbatim argument.
    ///
    /// This is `maxsplit=0`, which `open` and `cmd-set-text` are registered with. It is why
    /// `cmd-set-text :open -t -r {url:pretty}` sets the command line to `:open -t -r <url>` rather
    /// than passing `-t -r` to `cmd-set-text` itself — `commands/parser.py:177-205` finds the
    /// index of the first non-flag argument and re-splits with that as the limit.
    fn maxsplit0(tokens: &[String]) -> Args {
        let mut flags = Vec::new();
        let mut end_of_flags = false;
        for (i, token) in tokens.iter().enumerate() {
            if !end_of_flags {
                if token == "--" {
                    end_of_flags = true;
                    continue;
                }
                if is_flag(token) {
                    if let Some(long) = token.strip_prefix("--") {
                        flags.push(long.to_string());
                    } else {
                        for c in token[1..].chars() {
                            flags.push(c.to_string());
                        }
                    }
                    continue;
                }
            }
            return Args { flags, positional: vec![tokens[i..].join(" ")] };
        }
        Args { flags, positional: Vec::new() }
    }

    fn arg(&self, i: usize) -> Option<&str> {
        self.positional.get(i).map(String::as_str)
    }
}

fn is_flag(token: &str) -> bool {
    if !token.starts_with('-') || token == "-" || token == "--" {
        return false;
    }
    let after = &token[1..];
    let first = after.trim_start_matches('-').chars().next();
    // -1, -0.5: an argument, not a flag.
    !matches!(first, Some(c) if c.is_ascii_digit() || c == '.')
}

fn parse_one(s: &str) -> Result<Command, ParseError> {
    let tokens = tokenize(s);
    let Some(name) = tokens.first() else {
        return Err(ParseError("empty command".to_string()));
    };
    let args = Args::new(&tokens[1..]);

    let bad = |what: &str| ParseError(format!("{name}: {what}"));

    let cmd = match name.as_str() {
        "nop" => Command::Nop,
        "clear-keychain" => Command::ClearKeychain,

        "mode-enter" => {
            let Some(mode) = args.arg(0) else {
                return Err(bad("needs a mode"));
            };
            match Mode::from_name(mode) {
                Some(mode) => Command::ModeEnter(mode),
                // hint, caret, set_mark, … — real qutebrowser modes bru has not built yet.
                None => Command::Unimplemented(s.trim().to_string()),
            }
        }
        "mode-leave" => Command::ModeLeave,

        "scroll" => {
            let Some(dir) = args.arg(0) else {
                return Err(bad("needs a direction"));
            };
            let dir = match dir {
                "up" => ScrollDirection::Up,
                "down" => ScrollDirection::Down,
                "left" => ScrollDirection::Left,
                "right" => ScrollDirection::Right,
                "top" => ScrollDirection::Top,
                "bottom" => ScrollDirection::Bottom,
                "page-up" => ScrollDirection::PageUp,
                "page-down" => ScrollDirection::PageDown,
                other => return Err(bad(&format!("invalid direction {other:?}"))),
            };
            Command::Scroll(dir)
        }
        "scroll-px" => {
            let (Some(dx), Some(dy)) = (args.arg(0), args.arg(1)) else {
                return Err(bad("needs dx and dy"));
            };
            Command::ScrollPx {
                dx: dx.parse().map_err(|_| bad("dx is not a number"))?,
                dy: dy.parse().map_err(|_| bad("dy is not a number"))?,
            }
        }
        "scroll-page" => {
            let (Some(x), Some(y)) = (args.arg(0), args.arg(1)) else {
                return Err(bad("needs x and y"));
            };
            Command::ScrollPage {
                x: x.parse().map_err(|_| bad("x is not a number"))?,
                y: y.parse().map_err(|_| bad("y is not a number"))?,
            }
        }
        "scroll-to-perc" => Command::ScrollToPerc {
            perc: match args.arg(0) {
                Some(p) => Some(p.parse().map_err(|_| bad("perc is not a number"))?),
                None => None,
            },
            horizontal: args.any(&["x", "horizontal"]),
        },

        "tab-next" => Command::TabNext,
        "tab-prev" => Command::TabPrev,
        "tab-close" => Command::TabClose {
            opposite: args.any(&["o", "opposite"]),
            force: args.any(&["f", "force"]),
        },
        "tab-only" => Command::TabOnly { force: args.any(&["f", "force"]) },
        "tab-focus" => Command::TabFocus {
            index: match args.arg(0) {
                None => None,
                Some("last") => Some(TabIndex::Last),
                Some(n) => Some(TabIndex::Number(
                    n.parse().map_err(|_| bad(&format!("invalid index {n:?}")))?,
                )),
            },
        },
        // `-` and `+` reach here as positionals: `is_flag` refuses a bare `-`, and `+` never
        // looked like one.
        "tab-move" => Command::TabMove {
            to: match args.arg(0) {
                // "If neither is given, move it to the first position."
                None => TabMove::Start,
                Some("+") => TabMove::Relative(1),
                Some("-") => TabMove::Relative(-1),
                Some("start") => TabMove::Start,
                Some("end") => TabMove::End,
                Some(n) => TabMove::Index(
                    n.parse().map_err(|_| bad(&format!("invalid index {n:?}")))?,
                ),
            },
        },
        "tab-clone" => Command::TabClone {
            bg: args.any(&["b", "bg"]),
            window: args.any(&["w", "window"]),
            private: args.any(&["p", "private"]),
        },
        "undo" => Command::Undo { window: args.any(&["w", "window"]) },

        // maxsplit=0: the URL is whatever follows the flags, verbatim.
        "open" => {
            let args = Args::maxsplit0(&tokens[1..]);
            Command::Open {
                url: args.arg(0).filter(|u| !u.is_empty()).map(str::to_string),
                tab: args.any(&["t", "tab"]),
                bg: args.any(&["b", "bg"]),
                window: args.any(&["w", "window"]),
                private: args.any(&["p", "private"]),
                related: args.any(&["r", "related"]),
            }
        }
        "back" => Command::Back {
            tab: args.any(&["t", "tab"]),
            bg: args.any(&["b", "bg"]),
            window: args.any(&["w", "window"]),
        },
        "forward" => Command::Forward {
            tab: args.any(&["t", "tab"]),
            bg: args.any(&["b", "bg"]),
            window: args.any(&["w", "window"]),
        },
        "reload" => Command::Reload { force: args.any(&["f", "force"]) },
        "stop" => Command::Stop,
        "home" => Command::Home,
        "quit" => Command::Quit { save: args.has("save") },
        "close" => Command::Close,

        // `zoom 150%` and `zoom 150` are the same thing — `zoomcommands.py:68` strips the sign.
        "zoom" => Command::Zoom {
            level: match args.arg(0) {
                None => None,
                Some(l) => Some(
                    l.trim_end_matches('%')
                        .parse()
                        .map_err(|_| bad(&format!("invalid zoom level {l:?}")))?,
                ),
            },
        },
        "zoom-in" => Command::ZoomIn,
        "zoom-out" => Command::ZoomOut,
        "fullscreen" => Command::Fullscreen {
            enter: args.has("enter"),
            leave: args.has("leave"),
        },

        // `hint [group] [target] [args…]`, with qutebrowser's own defaults of group=all,
        // target=normal — so a bare `hint`, which is what `f` is bound to, is the click case.
        //
        // Only the `all` group is accepted. `links`, `images`, `inputs` and the rest name different
        // `hints.selectors` entries, and answering them with the `all` selector would hint the wrong
        // elements quietly; unimplemented says so instead.
        "hint" => {
            let group = args.arg(0).unwrap_or("all");
            let target = args.arg(1).unwrap_or("normal");
            let has_args = args.arg(2).is_some();
            let flags = args.any(&["rapid", "first", "mode", "add-history"]);
            match (group, target, has_args || flags) {
                ("all", "normal" | "current", false) => {
                    Command::Hint { target: HintTarget::Normal }
                }
                ("all", "tab" | "tab-fg" | "tab-bg", false) => {
                    Command::Hint { target: HintTarget::TabBg }
                }
                _ => Command::Unimplemented(s.trim().to_string()),
            }
        }
        "hint-follow" => Command::HintFollow,

// --- src/downloads.rs --------------------------------------------------------------------------
        // maxsplit=0, as `open` is: a URL is whatever follows the flags, verbatim. `--mhtml` and
        // `--dest` stay unimplemented rather than becoming a near miss — one needs a page
        // serialiser and the other needs the prompt bru does not have, and a `:download --dest x`
        // that quietly saved somewhere else would be worse than one that does nothing.
        "download" => {
            let args = Args::maxsplit0(&tokens[1..]);
            if args.any(&["m", "mhtml"]) || args.any(&["dest"]) {
                Command::Unimplemented(s.trim().to_string())
            } else {
                Command::Download {
                    url: args.arg(0).filter(|u| !u.is_empty()).map(str::to_string),
                }
            }
        }
        "download-cancel" => Command::DownloadCancel { all: args.any(&["a", "all"]) },
        "download-clear" => Command::DownloadClear,
        // Also maxsplit=0 in qutebrowser, and for the same reason: the command to open with may
        // carry its own flags.
        "download-open" => {
            let args = Args::maxsplit0(&tokens[1..]);
            Command::DownloadOpen {
                cmdline: args.arg(0).filter(|c| !c.is_empty()).map(str::to_string),
                dir: args.any(&["d", "dir"]),
            }
        }
        "download-delete" => Command::DownloadDelete,
        "download-retry" => Command::DownloadRetry,
// --- end src/downloads.rs ----------------------------------------------------------------------

// --- src/clip.rs -----------------------------------------------------------
        // `yank [what] [inline-text] [-s]`. Ordinary argparse, not maxsplit=0: `ym` is
        // `yank inline [{title}]({url:yank})` and `yM` puts its `-s` *after* the block, so the
        // flag has to be recognised wherever it appears.
        "yank" => {
            let what = match args.arg(0).unwrap_or("url") {
                "url" => YankWhat::Url,
                "pretty-url" => YankWhat::PrettyUrl,
                "title" => YankWhat::Title,
                "domain" => YankWhat::Domain,
                "inline" => {
                    let Some(text) = args.arg(1).filter(|t| !t.is_empty()) else {
                        return Err(bad("inline needs a block of text"));
                    };
                    YankWhat::Inline(text.to_string())
                }
                // A real qutebrowser argument bru has no caret mode for — unbuilt, not invalid.
                "selection" => return Ok(Command::Unimplemented(s.trim().to_string())),
                other => return Err(bad(&format!("cannot yank {other:?}"))),
            };
            Command::Yank { what, sel: args.any(&["s", "sel"]) }
        }
// --- end src/clip.rs -------------------------------------------------------


// --- src/find.rs + src/navigate.rs ---------------------------------------------------------------
        // maxsplit=0 (`commands.py:1621`), so the search text is everything after the flags,
        // verbatim: `search -r foo bar` searches for "foo bar" backwards, and a `-` inside the text
        // is text. A bare `search` has no text and clears.
        "search" => {
            let args = Args::maxsplit0(&tokens[1..]);
            Command::Search {
                text: args.arg(0).unwrap_or("").to_string(),
                reverse: args.any(&["r", "reverse"]),
            }
        }
        "search-next" => Command::SearchNext,
        "search-prev" => Command::SearchPrev,

        "navigate" => {
            let Some(to) = args.arg(0) else {
                return Err(bad("needs a destination"));
            };
            Command::Navigate {
                to: match to {
                    "prev" => NavigateTo::Prev,
                    "next" => NavigateTo::Next,
                    "up" => NavigateTo::Up,
                    "increment" => NavigateTo::Increment,
                    "decrement" => NavigateTo::Decrement,
                    "strip" => NavigateTo::Strip,
                    other => return Err(bad(&format!("invalid destination {other:?}"))),
                },
                tab: args.any(&["t", "tab"]),
                bg: args.any(&["b", "bg"]),
                window: args.any(&["w", "window"]),
            }
        }
// --- end src/find.rs + src/navigate.rs ------------------------------------------------------------
        // qutebrowser's `:help` opens its manual; bru's opens the only reference it has, which is
        // the one generated from the bindings it is running on.
        "help" => Command::Help { tab: args.has("t") || args.has("tab") },

// --- src/history.rs --------------------------------------------------------
        // maxsplit=0 on the four that take a name or a URL, which is how qutebrowser registers them
        // (`commands.py:1204`, `:1222`, `:1295`, `:1317`) — a quickmark name may contain spaces, and
        // a URL may contain anything.
        //
        // `quickmark-save` is the exception in both directions: qutebrowser's takes no argument at
        // all because it opens a prompt, and bru's takes an optional name because it has no prompt
        // mode. A bare `quickmark-save` prefills the command line; see the arm in `exec.rs`.
        "quickmark-save" => Command::QuickmarkSave {
            name: Args::maxsplit0(&tokens[1..]).arg(0).filter(|n| !n.is_empty()).map(str::to_string),
        },
        "quickmark-load" => {
            let args = Args::maxsplit0(&tokens[1..]);
            Command::QuickmarkLoad {
                name: args.arg(0).filter(|n| !n.is_empty()).map(str::to_string),
                tab: args.any(&["t", "tab"]),
                bg: args.any(&["b", "bg"]),
                window: args.any(&["w", "window"]),
            }
        }
        "quickmark-del" => Command::QuickmarkDel {
            name: Args::maxsplit0(&tokens[1..]).arg(0).filter(|n| !n.is_empty()).map(str::to_string),
        },
        // Two positionals, so *not* maxsplit=0 (`commands.py:1256`). `:bookmark-add <url> <title>`
        // needs the two split apart, and qutebrowser rejects a URL with no title.
        "bookmark-add" => {
            if args.arg(0).is_some() && args.arg(1).is_none() {
                return Err(bad("a title must be given with a URL"));
            }
            Command::BookmarkAdd {
                url: args.arg(0).map(str::to_string),
                title: args.positional.get(1..).map(|rest| rest.join(" ")).filter(|t| !t.is_empty()),
                toggle: args.has("toggle"),
            }
        }
        "bookmark-load" => {
            let args = Args::maxsplit0(&tokens[1..]);
            Command::BookmarkLoad {
                url: args.arg(0).filter(|u| !u.is_empty()).map(str::to_string),
                tab: args.any(&["t", "tab"]),
                bg: args.any(&["b", "bg"]),
                window: args.any(&["w", "window"]),
                delete: args.any(&["d", "delete"]),
            }
        }
        "bookmark-del" => Command::BookmarkDel {
            url: Args::maxsplit0(&tokens[1..]).arg(0).filter(|u| !u.is_empty()).map(str::to_string),
        },
        // `tab` defaults to True on both of these in qutebrowser, so there is no in-place spelling
        // to parse; `-t` is accepted and means what it already does.
        "bookmark-list" => Command::BookmarkList {
            jump: args.has("jump"),
            bg: args.any(&["b", "bg"]),
        },
        "history" => Command::History { bg: args.any(&["b", "bg"]) },
// --- end src/history.rs ----------------------------------------------------

        // maxsplit=0: `cmd-set-text :open -t` prefills the command line with `:open -t`, so the
        // `-t` belongs to the text and not to cmd-set-text.
        "cmd-set-text" => {
            let args = Args::maxsplit0(&tokens[1..]);
            let Some(text) = args.arg(0).filter(|t| !t.is_empty()) else {
                return Err(bad("needs text"));
            };
            Command::CmdSetText {
                text: text.to_string(),
                space: args.any(&["s", "space"]),
                append: args.any(&["a", "append"]),
                run_on_count: args.any(&["r", "run-on-count"]),
            }
        }
        "command-accept" => Command::CommandAccept { rapid: args.has("rapid") },

// --- src/spawn.rs, src/editor.rs -----------------------------------------------------------
        // `spawn` cannot use `Args::maxsplit0`: that joins already-tokenized words back together
        // with spaces, and the quotes are gone by then. `spawn_tail` re-reads the original string.
        "spawn" => {
            let (flags, cmdline) = spawn_tail(s);
            let has = |names: &[&str]| names.iter().any(|n| flags.iter().any(|f| f == n));
            let userscript = has(&["u", "userscript"]);
            let detach = has(&["d", "detach"]);
            if cmdline.is_empty() {
                return Err(bad("needs something to run"));
            }
            // `cmdutils.check_exclusive((userscript, detach), 'ud')`.
            if userscript && detach {
                return Err(bad("--userscript and --detach are mutually exclusive"));
            }
            // `-o` shows the output in a new tab, which needs a `bru://process/<pid>` page bru
            // does not have. Unimplemented rather than silently dropped: a `:spawn -o` that ran
            // the program and showed nothing would look like the program failed.
            if has(&["o", "output"]) {
                Command::Unimplemented(s.trim().to_string())
            } else {
                Command::Spawn {
                    cmdline,
                    userscript,
                    detach,
                    messages: has(&["m", "output-messages"]),
                    verbose: has(&["v", "verbose"]),
                }
            }
        }
        // qutebrowser renamed `open-editor` to `edit-text` in 1.0 and kept no alias; bru answers to
        // both, because both are in this user's fingers and neither can mean anything else.
        "edit-text" | "open-editor" => Command::EditText,
        // maxsplit=0: `insert-text -- {primary}` inserts the primary selection, `--` and all.
        "insert-text" => {
            let args = Args::maxsplit0(&tokens[1..]);
            match args.arg(0).filter(|text| !text.is_empty()) {
                Some(text) => Command::InsertText { text: text.to_string() },
                None => return Err(bad("needs text")),
            }
        }
        "fake-key" => {
            let Some(keystring) = args.arg(0) else {
                return Err(bad("needs a keystring"));
            };
            // `--global` posts to qutebrowser's own window rather than to the page. bru's UI *is*
            // browsers, so "the focused window" is not a thing it can name yet.
            if args.any(&["g", "global"]) {
                Command::Unimplemented(s.trim().to_string())
            } else {
                Command::FakeKey { keystring: keystring.to_string() }
            }
        }
// --- end src/spawn.rs, src/editor.rs -------------------------------------------------------

        _ => Command::Unimplemented(s.trim().to_string()),
    };
    Ok(cmd)
}

// --- src/spawn.rs, src/editor.rs -----------------------------------------------------------

/// Split `spawn -u -m qute-pass -u "login: (.+)"` into its flags and everything after them,
/// **verbatim from the original string**.
///
/// [`Args::maxsplit0`] cannot do this. It works from [`tokenize`]'s output, which has already
/// thrown the quotes away, and rejoining with spaces turns one argument containing a space into
/// two. `spawn` is the only command whose tail is re-split by something other than this file, so it
/// is the only one that needs its quotes intact.
///
/// Returns the long spellings of the flags (`-u` as `u`, `--userscript` as `userscript`) and the
/// untouched remainder.
fn spawn_tail(s: &str) -> (Vec<String>, String) {
    let mut flags = Vec::new();
    let mut rest = s.trim_start();

    // The command name.
    rest = match rest.find(char::is_whitespace) {
        Some(at) => &rest[at..],
        None => "",
    };

    loop {
        rest = rest.trim_start();
        let word = match rest.find(char::is_whitespace) {
            Some(at) => &rest[..at],
            None => rest,
        };
        if word == "--" {
            return (flags, rest[word.len()..].trim().to_string());
        }
        if word.is_empty() || !is_flag(word) {
            return (flags, rest.to_string());
        }
        match word.strip_prefix("--") {
            Some(long) => flags.push(long.to_string()),
            // `-um` is two short flags, as in argparse.
            None => flags.extend(word[1..].chars().map(|c| c.to_string())),
        }
        rest = &rest[word.len()..];
    }
}

// --- end src/spawn.rs, src/editor.rs -------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_keys_that_already_work() {
        assert_eq!(parse("scroll down").unwrap(), Command::Scroll(ScrollDirection::Down));
        assert_eq!(parse("scroll up").unwrap(), Command::Scroll(ScrollDirection::Up));
        assert_eq!(parse("scroll left").unwrap(), Command::Scroll(ScrollDirection::Left));
        assert_eq!(parse("scroll right").unwrap(), Command::Scroll(ScrollDirection::Right));
        assert_eq!(parse("tab-next").unwrap(), Command::TabNext);
        assert_eq!(parse("tab-prev").unwrap(), Command::TabPrev);
        assert_eq!(
            parse("tab-close").unwrap(),
            Command::TabClose { opposite: false, force: false }
        );
        assert_eq!(
            parse("tab-close -o").unwrap(),
            Command::TabClose { opposite: true, force: false }
        );
    }

    #[test]
    fn flags_before_a_positional() {
        // The exact string PLAN.md names. `-s` is a flag, `:open` is the text.
        assert_eq!(
            parse("cmd-set-text -s :open").unwrap(),
            Command::CmdSetText {
                text: ":open".to_string(),
                space: true,
                append: false,
                run_on_count: false,
            }
        );
        // `T: cmd-set-text -sr :tab-focus` — `-sr` is two short flags, as in argparse.
        assert_eq!(
            parse("cmd-set-text -sr :tab-focus").unwrap(),
            Command::CmdSetText {
                text: ":tab-focus".to_string(),
                space: true,
                append: false,
                run_on_count: true,
            }
        );
        // `gO: cmd-set-text :open -t -r {url:pretty}` — maxsplit=0 stops flag parsing at the first
        // positional, so `-t -r` belong to the *text*. That matters: pressing gO must prefill the
        // command line with ":open -t -r <url>", not run cmd-set-text with -r.
        assert_eq!(
            parse("cmd-set-text :open -t -r {url:pretty}").unwrap(),
            Command::CmdSetText {
                text: ":open -t -r {url:pretty}".to_string(),
                space: false,
                append: false,
                run_on_count: false,
            }
        );
        // `O: cmd-set-text -s :open -t` — a leading flag *and* text containing a flag.
        assert_eq!(
            parse("cmd-set-text -s :open -t").unwrap(),
            Command::CmdSetText {
                text: ":open -t".to_string(),
                space: true,
                append: false,
                run_on_count: false,
            }
        );
    }

    #[test]
    fn open_with_its_flags() {
        assert_eq!(
            parse("open -t").unwrap(),
            Command::Open {
                url: None,
                tab: true,
                bg: false,
                window: false,
                private: false,
                related: false
            }
        );
        assert_eq!(
            parse("open -w").unwrap(),
            Command::Open {
                url: None,
                tab: false,
                bg: false,
                window: true,
                private: false,
                related: false
            }
        );
        // `pp: open -- {clipboard}` — `--` ends the flags so a URL starting with `-` survives.
        assert_eq!(
            parse("open -- {clipboard}").unwrap(),
            Command::Open {
                url: Some("{clipboard}".to_string()),
                tab: false,
                bg: false,
                window: false,
                private: false,
                related: false
            }
        );
    }

    #[test]
    fn negative_numbers_are_arguments_not_flags() {
        // <Ctrl-B>: scroll-page 0 -1
        assert_eq!(parse("scroll-page 0 -1").unwrap(), Command::ScrollPage { x: 0.0, y: -1.0 });
        assert_eq!(parse("scroll-page 0 0.5").unwrap(), Command::ScrollPage { x: 0.0, y: 0.5 });
        assert_eq!(parse("scroll-page 0 -0.5").unwrap(), Command::ScrollPage { x: 0.0, y: -0.5 });
        // <Alt-9>: tab-focus -1
        assert_eq!(
            parse("tab-focus -1").unwrap(),
            Command::TabFocus { index: Some(TabIndex::Number(-1)) }
        );
        assert_eq!(
            parse("tab-focus last").unwrap(),
            Command::TabFocus { index: Some(TabIndex::Last) }
        );
    }

    #[test]
    fn chains() {
        // <Escape> in normal mode.
        let cmd = parse("clear-keychain ;; search ;; fullscreen --leave").unwrap();
        let Command::Chain(parts) = cmd else { panic!("expected a chain") };
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0], Command::ClearKeychain);
        // A bare `search` is `search` with no text, which is how `<Escape>` clears one.
        assert_eq!(parts[1], Command::Search { text: String::new(), reverse: false });
        assert_eq!(parts[2], Command::Fullscreen { enter: false, leave: true });
        // Implemented as a whole now that the middle link is: a chain is only as implemented as its
        // least-implemented link, and this one was held back by `search` until src/find.rs was
        // wired to the dispatcher.
        assert!(Command::Chain(parts).is_implemented());
    }

    #[test]
    fn quoted_arguments_survive() {
        // <Ctrl-W> in command mode: rl-rubout " " — the argument is one space.
        assert_eq!(
            parse("rl-rubout \" \"").unwrap(),
            Command::Unimplemented("rl-rubout \" \"".to_string())
        );
        // A `;;` inside quotes is not a chain separator.
        let cmd = parse("cmd-set-text \"a ;; b\"").unwrap();
        let Command::CmdSetText { text, .. } = cmd else { panic!("expected cmd-set-text") };
        assert_eq!(text, "a ;; b");
    }

    #[test]
    fn modes() {
        assert_eq!(parse("mode-enter insert").unwrap(), Command::ModeEnter(Mode::Insert));
        assert_eq!(
            parse("mode-enter passthrough").unwrap(),
            Command::ModeEnter(Mode::Passthrough)
        );
        assert_eq!(parse("mode-leave").unwrap(), Command::ModeLeave);
        // caret is a real qutebrowser mode bru has not built; it is not an error, it is unbuilt.
        assert_eq!(
            parse("mode-enter caret").unwrap(),
            Command::Unimplemented("mode-enter caret".to_string())
        );
    }

    #[test]
    fn unknown_commands_are_kept_verbatim_not_rejected() {
        // `<Ctrl-Alt-p>` in the default table. Printing is in no milestone; the binding still has
        // to keep its place in the trie.
        let cmd = parse("print").unwrap();
        assert_eq!(cmd, Command::Unimplemented("print".to_string()));
        assert!(!cmd.is_implemented());
    }

// --- src/clip.rs -----------------------------------------------------------
    #[test]
    fn yank_in_its_ten_spellings() {
        // The ten default bindings, in the order config.rs lists them: yy yY yt yT yd yD yp yP
        // ym yM. `-s` is the primary selection every time it appears.
        for (cmd, want) in [
            ("yank", Command::Yank { what: YankWhat::Url, sel: false }),
            ("yank -s", Command::Yank { what: YankWhat::Url, sel: true }),
            ("yank title", Command::Yank { what: YankWhat::Title, sel: false }),
            ("yank title -s", Command::Yank { what: YankWhat::Title, sel: true }),
            ("yank domain", Command::Yank { what: YankWhat::Domain, sel: false }),
            ("yank domain -s", Command::Yank { what: YankWhat::Domain, sel: true }),
            ("yank pretty-url", Command::Yank { what: YankWhat::PrettyUrl, sel: false }),
            ("yank pretty-url -s", Command::Yank { what: YankWhat::PrettyUrl, sel: true }),
            (
                "yank inline [{title}]({url:yank})",
                Command::Yank {
                    what: YankWhat::Inline("[{title}]({url:yank})".to_string()),
                    sel: false,
                },
            ),
            // `yM` puts the flag *after* the block of text, which is why this is not maxsplit=0.
            (
                "yank inline [{title}]({url:yank}) -s",
                Command::Yank {
                    what: YankWhat::Inline("[{title}]({url:yank})".to_string()),
                    sel: true,
                },
            ),
        ] {
            assert_eq!(parse(cmd).unwrap(), want, "{cmd:?}");
        }

        // `yank selection` is caret mode's, and bru has no caret mode.
        assert_eq!(
            parse("yank selection").unwrap(),
            Command::Unimplemented("yank selection".to_string())
        );
        assert!(parse("yank sideways").is_err());
        assert!(parse("yank inline").is_err());
    }
// --- end src/clip.rs -------------------------------------------------------

    #[test]
    fn hints() {
        // `f: hint` — no group, no target, so qutebrowser's defaults: all, normal.
        assert_eq!(parse("hint").unwrap(), Command::Hint { target: HintTarget::Normal });
        // `F: hint all tab`, and `;b: hint all tab-bg` reaches the same place.
        assert_eq!(parse("hint all tab").unwrap(), Command::Hint { target: HintTarget::TabBg });
        assert_eq!(parse("hint all tab-bg").unwrap(), Command::Hint { target: HintTarget::TabBg });
        assert_eq!(parse("hint-follow").unwrap(), Command::HintFollow);

        // Everything M12 does not do stays unimplemented rather than becoming a near miss. A
        // `links` group hinted with the `all` selector would draw labels on images and buttons and
        // look like a bug in the visibility test.
        for cmd in [
            "hint links",
            "hint images",
            "hint inputs",
            "hint all window",
            "hint all hover",
            "hint all yank",
            "hint --rapid links tab-bg",
            "hint inputs --first",
            "hint links fill :open {hint-url}",
        ] {
            assert_eq!(
                parse(cmd).unwrap(),
                Command::Unimplemented(cmd.to_string()),
                "{cmd:?} should not be mistaken for something bru implements"
            );
        }
    }

// --- src/history.rs --------------------------------------------------------
    #[test]
    fn quickmarks_and_bookmarks() {
        // `m`, and the line it prefills once a name has been typed.
        assert_eq!(parse("quickmark-save").unwrap(), Command::QuickmarkSave { name: None });
        assert_eq!(
            parse("quickmark-save go").unwrap(),
            Command::QuickmarkSave { name: Some("go".to_string()) }
        );
        // maxsplit=0: a quickmark name may contain spaces, and `data.rs` stores one that does.
        assert_eq!(
            parse("quickmark-save two words").unwrap(),
            Command::QuickmarkSave { name: Some("two words".to_string()) }
        );

        // What `b`, `B` and `wb` prefill, with a name typed after the flags.
        assert_eq!(
            parse("quickmark-load go").unwrap(),
            Command::QuickmarkLoad { name: Some("go".into()), tab: false, bg: false, window: false }
        );
        assert_eq!(
            parse("quickmark-load -t two words").unwrap(),
            Command::QuickmarkLoad {
                name: Some("two words".into()),
                tab: true,
                bg: false,
                window: false,
            }
        );
        assert_eq!(
            parse("quickmark-load -w go").unwrap(),
            Command::QuickmarkLoad { name: Some("go".into()), tab: false, bg: false, window: true }
        );
        // A bare `:quickmark-load` is what the line says before a name is typed; it must parse, and
        // the dispatcher says which quickmark it wanted.
        assert_eq!(
            parse("quickmark-load").unwrap(),
            Command::QuickmarkLoad { name: None, tab: false, bg: false, window: false }
        );

        // `M`. Not `--toggle`: the default binding is bare (configdata.yml:3776).
        assert_eq!(
            parse("bookmark-add").unwrap(),
            Command::BookmarkAdd { url: None, title: None, toggle: false }
        );
        assert_eq!(
            parse("bookmark-add --toggle").unwrap(),
            Command::BookmarkAdd { url: None, title: None, toggle: true }
        );
        // Two positionals, so *not* maxsplit=0 — the title is everything after the URL.
        assert_eq!(
            parse("bookmark-add https://example.com/ Example Domain").unwrap(),
            Command::BookmarkAdd {
                url: Some("https://example.com/".into()),
                title: Some("Example Domain".into()),
                toggle: false,
            }
        );
        // qutebrowser's own error, commands.py:1275-1277.
        assert!(parse("bookmark-add https://example.com/").is_err());

        assert_eq!(
            parse("bookmark-load -t https://example.com/").unwrap(),
            Command::BookmarkLoad {
                url: Some("https://example.com/".into()),
                tab: true,
                bg: false,
                window: false,
                delete: false,
            }
        );
        // A URL with a query that contains what would otherwise be a flag survives maxsplit=0.
        assert_eq!(
            parse("bookmark-load https://example.com/?a=-t").unwrap(),
            Command::BookmarkLoad {
                url: Some("https://example.com/?a=-t".into()),
                tab: false,
                bg: false,
                window: false,
                delete: false,
            }
        );

        assert_eq!(parse("quickmark-del go").unwrap(), Command::QuickmarkDel { name: Some("go".into()) });
        assert_eq!(parse("quickmark-del").unwrap(), Command::QuickmarkDel { name: None });
        assert_eq!(parse("bookmark-del").unwrap(), Command::BookmarkDel { url: None });

        // `Sq` and `Sb` — the one flag between them.
        assert_eq!(parse("bookmark-list").unwrap(), Command::BookmarkList { jump: false, bg: false });
        assert_eq!(
            parse("bookmark-list --jump").unwrap(),
            Command::BookmarkList { jump: true, bg: false }
        );
        // `Sh`.
        assert_eq!(parse("history").unwrap(), Command::History { bg: false });
        assert_eq!(parse("history -b").unwrap(), Command::History { bg: true });
    }
// --- end src/history.rs ----------------------------------------------------

// --- src/spawn.rs, src/editor.rs -----------------------------------------------------------

    /// The exact strings in this user's `~/.config/qutebrowser/config.py`, which is what `:spawn`
    /// has to accept on the first day or the bindings that pay for this milestone do not move over.
    #[test]
    fn spawn_the_way_this_user_writes_it() {
        assert_eq!(
            parse("spawn --userscript ~/.config/bru/userscripts/qute-pass").unwrap(),
            Command::Spawn {
                cmdline: "~/.config/bru/userscripts/qute-pass".to_string(),
                userscript: true,
                detach: false,
                messages: false,
                verbose: false,
            }
        );
        assert_eq!(
            parse("spawn --userscript qute-pass --password-only").unwrap(),
            Command::Spawn {
                // `--password-only` is the *script's* flag: flag parsing stopped at the first
                // non-flag word, so everything after it belongs to the tail.
                cmdline: "qute-pass --password-only".to_string(),
                userscript: true,
                detach: false,
                messages: false,
                verbose: false,
            }
        );
        // The quoted argument is the whole reason `spawn_tail` exists.
        let Command::Spawn { cmdline, .. } =
            parse(r#"spawn -u qute-pass -U secret -u "login: (.+)" -d dmenu"#).unwrap()
        else {
            panic!("expected a spawn")
        };
        assert_eq!(cmdline, r#"qute-pass -U secret -u "login: (.+)" -d dmenu"#);
        assert_eq!(
            crate::spawn::shlex(&cmdline).unwrap(),
            ["qute-pass", "-U", "secret", "-u", "login: (.+)", "-d", "dmenu"]
        );
    }

    #[test]
    fn spawn_flags() {
        let flags = |s: &str| {
            let Command::Spawn { userscript, detach, messages, verbose, .. } = parse(s).unwrap()
            else {
                panic!("expected a spawn for {s:?}")
            };
            (userscript, detach, messages, verbose)
        };
        assert_eq!(flags("spawn echo hi"), (false, false, false, false));
        assert_eq!(flags("spawn -d mpv x"), (false, true, false, false));
        assert_eq!(flags("spawn -m echo hi"), (false, false, true, false));
        assert_eq!(flags("spawn -mv echo hi"), (false, false, true, true));
        assert_eq!(flags("spawn --detach --verbose mpv x"), (false, true, false, true));
        // `--` ends bru's flags; what follows is the program's, whatever it looks like.
        let Command::Spawn { cmdline, detach, .. } = parse("spawn -d -- mpv --no-video x").unwrap()
        else {
            panic!("expected a spawn")
        };
        assert_eq!((cmdline.as_str(), detach), ("mpv --no-video x", true));
    }

    #[test]
    fn spawn_refuses_what_it_cannot_do() {
        // qutebrowser's own `check_exclusive`.
        assert!(parse("spawn -u -d qute-pass").is_err());
        assert!(parse("spawn").is_err());
        assert!(parse("spawn -u").is_err());
        // `-o` opens the output in a tab, which needs a page bru has not got.
        assert_eq!(
            parse("spawn -o echo hi").unwrap(),
            Command::Unimplemented("spawn -o echo hi".to_string())
        );
    }

    #[test]
    fn the_three_insert_mode_bindings() {
        assert_eq!(parse("edit-text").unwrap(), Command::EditText);
        assert_eq!(parse("open-editor").unwrap(), Command::EditText);
        assert_eq!(
            parse("insert-text -- {primary}").unwrap(),
            Command::InsertText { text: "{primary}".to_string() }
        );
        assert_eq!(
            parse("fake-key <Escape>").unwrap(),
            Command::FakeKey { keystring: "<Escape>".to_string() }
        );
        assert_eq!(
            parse("fake-key <Ctrl-x>").unwrap(),
            Command::FakeKey { keystring: "<Ctrl-x>".to_string() }
        );
        // `--global` aims at qutebrowser's own window, which bru cannot name.
        assert_eq!(
            parse("fake-key --global <Escape>").unwrap(),
            Command::Unimplemented("fake-key --global <Escape>".to_string())
        );
        assert!(parse("fake-key").is_err());
        assert!(parse("insert-text").is_err());
    }

// --- end src/spawn.rs, src/editor.rs -------------------------------------------------------

// --- src/find.rs + src/navigate.rs ---------------------------------------------------------------
    #[test]
    fn search_takes_its_text_verbatim() {
        // `<Escape>`'s middle link, and `:search` with nothing to search for.
        assert_eq!(
            parse("search").unwrap(),
            Command::Search { text: String::new(), reverse: false }
        );
        // maxsplit=0: several words are one search, not a command with arguments.
        assert_eq!(
            parse("search foo bar").unwrap(),
            Command::Search { text: "foo bar".to_string(), reverse: false }
        );
        // `?text` — the command line's `?` prefix means `-r`.
        assert_eq!(
            parse("search -r foo").unwrap(),
            Command::Search { text: "foo".to_string(), reverse: true }
        );
        // Past the first positional, a `-` is text. Losing this would make `:search -r` unable to
        // find "-r" and `search foo -r` search backwards for "foo".
        assert_eq!(
            parse("search foo -r").unwrap(),
            Command::Search { text: "foo -r".to_string(), reverse: false }
        );
        assert_eq!(parse("search-next").unwrap(), Command::SearchNext);
        assert_eq!(parse("search-prev").unwrap(), Command::SearchPrev);
    }

    #[test]
    fn navigate_names_six_destinations_and_no_others() {
        let plain = |to| Command::Navigate { to, tab: false, bg: false, window: false };
        assert_eq!(parse("navigate prev").unwrap(), plain(NavigateTo::Prev));
        assert_eq!(parse("navigate next").unwrap(), plain(NavigateTo::Next));
        assert_eq!(parse("navigate up").unwrap(), plain(NavigateTo::Up));
        assert_eq!(parse("navigate increment").unwrap(), plain(NavigateTo::Increment));
        assert_eq!(parse("navigate decrement").unwrap(), plain(NavigateTo::Decrement));
        assert_eq!(parse("navigate strip").unwrap(), plain(NavigateTo::Strip));
        // `{{` and `}}`, and `gU`.
        assert_eq!(
            parse("navigate prev -t").unwrap(),
            Command::Navigate { to: NavigateTo::Prev, tab: true, bg: false, window: false }
        );
        assert_eq!(
            parse("navigate up -t").unwrap(),
            Command::Navigate { to: NavigateTo::Up, tab: true, bg: false, window: false }
        );
        // `choices=[...]` in qutebrowser: a destination it does not know is an error, not a
        // silently inert binding.
        assert!(parse("navigate sideways").is_err());
        assert!(parse("navigate").is_err());
    }
// --- end src/find.rs + src/navigate.rs ------------------------------------------------------------

    #[test]
    fn malformed_arguments_are_errors() {
        assert!(parse("scroll sideways").is_err());
        assert!(parse("scroll").is_err());
        assert!(parse("scroll-page 0 sideways").is_err());
        assert!(parse("tab-focus middle").is_err());
        assert!(parse("").is_err());
    }
}
