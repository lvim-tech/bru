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

// --- src/find.rs + src/navigate.rs ---------------------------------------------------------------
    /// `search [-r] [text]` — `/text`, and `?text` with `-r`. No text clears the search, which is
    /// what `<Escape>`'s `clear-keychain ;; search ;; fullscreen --leave` relies on.
    Search { text: String, reverse: bool },
    /// `search-next` — `n`, continuing in the direction the search was started in.
    SearchNext,
    /// `search-prev` — `N`.
    SearchPrev,
// --- end src/find.rs + src/navigate.rs ------------------------------------------------------------

    /// `cmd-set-text [-s] [-a] [-r] <text>` — the machinery behind `o`, `O`, `go`, `b`, `T`, …
    CmdSetText { text: String, space: bool, append: bool, run_on_count: bool },
    /// `command-accept [--rapid]`
    CommandAccept { rapid: bool },

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
// --- end src/find.rs + src/navigate.rs ------------------------------------------------------------
        // qutebrowser's `:help` opens its manual; bru's opens the only reference it has, which is
        // the one generated from the bindings it is running on.
        "help" => Command::Help { tab: args.has("t") || args.has("tab") },

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

        _ => Command::Unimplemented(s.trim().to_string()),
    };
    Ok(cmd)
}

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
        let cmd = parse("yank pretty-url").unwrap();
        assert_eq!(cmd, Command::Unimplemented("yank pretty-url".to_string()));
        assert!(!cmd.is_implemented());
    }

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
