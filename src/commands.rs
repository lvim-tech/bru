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

// --- src/hints.rs --------------------------------------------------------------------------
    /// `hint [--rapid] [--first] [group] [target] [args…]` — draw labels over the page and follow
    /// the one that is typed.
    ///
    /// `f` is a bare `hint`, `F` is `hint all tab`, `;i` is `hint images`. The targets bru does not
    /// implement — `run`, `spawn`, `userscript`, `delete`, `right-click` — parse into
    /// [`Command::Unimplemented`] rather than into a variant that would silently do the wrong
    /// thing.
    Hint { group: HintGroup, target: HintTarget, rapid: bool, first: bool },
    /// `hint-follow` — the `<Return>` binding in hint mode.
    HintFollow,
// --- end src/hints.rs ----------------------------------------------------------------------

    /// `help [-t]` — bru's own key and command reference, generated from the live binding table.
    Help { tab: bool },

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

// --- src/hints.rs --------------------------------------------------------------------------
/// The `hints.selectors` group a `hint` command names — which elements get a label.
///
/// A parallel of `crate::hints::Group`, which is the same set. The two are kept apart because this
/// file is about what a command *string* means and knows nothing about CEF; `exec.rs` maps between
/// them in one match, the way it already does for [`ScrollDirection`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HintGroup {
    All,
    Links,
    Images,
    Media,
    Url,
    Inputs,
}

impl HintGroup {
    /// The six keys of `hints.selectors`' default. A name that is not one of them is a group only
    /// a `config.lua` could have added, and bru has no `hints.selectors` setting to add it in.
    fn parse(name: &str) -> Option<HintGroup> {
        Some(match name {
            "all" => HintGroup::All,
            "links" => HintGroup::Links,
            "images" => HintGroup::Images,
            "media" => HintGroup::Media,
            "url" => HintGroup::Url,
            "inputs" => HintGroup::Inputs,
            _ => return None,
        })
    }
}

/// The `hint` targets bru implements. `hints.Target` has sixteen; `run`, `spawn`, `userscript`,
/// `delete` and `right-click` are the five that arrive with the commands they depend on.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum HintTarget {
    /// `hint` / `hint all current` (no target): click the element where it is.
    Normal,
    /// `hint all tab` / `hint all tab-bg`: the element's URL in a background tab.
    ///
    /// `tab` and `tab-bg` differ by `tabs.background`, whose default is **true**
    /// (configdata.yml:2217), so both open in the background until bru has a setting for it.
    TabBg,
    /// `hint all tab-fg`: a tab that is switched to.
    TabFg,
    /// `hint all window`: a new window in qutebrowser, a foreground tab in bru — one window.
    Window,
    /// `hint all hover`
    Hover,
    /// `hint links yank` — **the clipboard is another workstream's**; see `hints::Clipboard`.
    Yank,
    /// `hint links yank-primary` — same.
    YankPrimary,
    /// `hint links download` — **downloads are another workstream's**; see `hints::Downloads`.
    Download,
    /// `hint links fill :open {hint-url}` — the text to put in the command line, `{hint-url}` and
    /// all. Substitution happens when a hint is followed, because only then is there a URL.
    Fill(String),
}
// --- end src/hints.rs ----------------------------------------------------------------------

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

// --- src/hints.rs --------------------------------------------------------------------------
    /// `maxsplit=2`, which `hint` is registered with (hints.py:743).
    ///
    /// The same trick as [`Args::maxsplit0`] and the same source (`parser.py:_split_args`): find
    /// the first non-flag token, re-split with `that index + 2` as the limit, and everything past
    /// it is one verbatim argument. It is what keeps `;O` working —
    /// `hint links fill :open -t -r {hint-url}` must reach `fill` as the single string
    /// `:open -t -r {hint-url}`, and a plain flag split would eat the `-t` and the `-r` as `hint`'s
    /// own. Flags *before* the group are still flags, which is how `hint --rapid links tab-bg`
    /// parses, and argparse's ordinary interspersing is why `hint inputs --first` does too.
    fn maxsplit2(tokens: &[String]) -> Args {
        let Some(first) = tokens.iter().position(|t| t != "--" && !is_flag(t)) else {
            // Only flags: nothing to hold back, and the first split was already right.
            return Args::new(tokens);
        };
        let limit = first + 2;
        let mut pieces: Vec<String> = tokens[..limit.min(tokens.len())].to_vec();
        if tokens.len() > limit {
            pieces.push(tokens[limit..].join(" "));
        }
        Args::new(&pieces)
    }
// --- end src/hints.rs ----------------------------------------------------------------------
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

// --- src/hints.rs --------------------------------------------------------------------------
        // `hint [--rapid] [--first] [group] [target] [args…]`, with qutebrowser's own defaults of
        // group=all, target=normal — so a bare `hint`, which is what `f` is bound to, is the click
        // case.
        //
        // `maxsplit=2` (hints.py:743), which is not a detail: `;O` is
        // `hint links fill :open -t -r {hint-url}`, and its `-t -r` belong to the `:open` that ends
        // up in the command line, not to `hint`. See [`Args::maxsplit2`].
        "hint" => {
            let args = Args::maxsplit2(&tokens[1..]);
            // `--mode number|word` and `--add-history` are real flags of real features bru has not
            // built. Silently ignoring either would give the wrong hints or a wrong history.
            if args.any(&["mode", "add-history"]) {
                return Ok(Command::Unimplemented(s.trim().to_string()));
            }

            let Some(group) = HintGroup::parse(args.arg(0).unwrap_or("all")) else {
                return Ok(Command::Unimplemented(s.trim().to_string()));
            };
            let target = args.arg(1).unwrap_or("normal");
            let rest = args.arg(2);

            let target = match (target, rest) {
                // `current` also removes `target="_blank"` in qutebrowser. bru clicks the element
                // where it is either way, which is what both spellings mean here.
                ("normal" | "current", None) => HintTarget::Normal,
                ("tab" | "tab-bg", None) => HintTarget::TabBg,
                ("tab-fg", None) => HintTarget::TabFg,
                ("window", None) => HintTarget::Window,
                ("hover", None) => HintTarget::Hover,
                ("yank", None) => HintTarget::Yank,
                ("yank-primary", None) => HintTarget::YankPrimary,
                ("download", None) => HintTarget::Download,
                // `_check_args`, hints.py:565: fill is the one target here that *requires* an
                // argument, and one with no command line text to set is an error, not a no-op.
                ("fill", Some(text)) => HintTarget::Fill(text.to_string()),
                ("fill", None) => return Err(bad("hint fill needs the command line text")),
                // `run`, `spawn`, `userscript`, `delete`, `right-click`, and any target with
                // arguments it has no use for.
                _ => return Ok(Command::Unimplemented(s.trim().to_string())),
            };

            Command::Hint {
                group,
                target,
                rapid: args.any(&["r", "rapid"]),
                first: args.any(&["f", "first"]),
            }
        }
        "hint-follow" => Command::HintFollow,
// --- end src/hints.rs ----------------------------------------------------------------------
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
        assert_eq!(parts[1], Command::Unimplemented("search".to_string()));
        assert_eq!(parts[2], Command::Fullscreen { enter: false, leave: true });
        // Still not implemented as a whole: `search` is src/find.rs's, and a chain is only as
        // implemented as its least-implemented link.
        assert!(!Command::Chain(parts).is_implemented());
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
        let hint = |group, target| Command::Hint { group, target, rapid: false, first: false };

        // `f: hint` — no group, no target, so qutebrowser's defaults: all, normal.
        assert_eq!(parse("hint").unwrap(), hint(HintGroup::All, HintTarget::Normal));
        // `F: hint all tab`, and `;b: hint all tab-bg` reaches the same place — `tabs.background`
        // defaults to true (configdata.yml:2217).
        assert_eq!(parse("hint all tab").unwrap(), hint(HintGroup::All, HintTarget::TabBg));
        assert_eq!(parse("hint all tab-bg").unwrap(), hint(HintGroup::All, HintTarget::TabBg));
        assert_eq!(parse("hint-follow").unwrap(), Command::HintFollow);

        // Every one of the fifteen bindings around `f`, spelled as configdata.yml:3723-3739 spells
        // them. A total is not enough: `;i` and `;I` differ by one word and by everything.
        assert_eq!(parse("hint all window").unwrap(), hint(HintGroup::All, HintTarget::Window));
        assert_eq!(parse("hint all tab-fg").unwrap(), hint(HintGroup::All, HintTarget::TabFg));
        assert_eq!(parse("hint all hover").unwrap(), hint(HintGroup::All, HintTarget::Hover));
        assert_eq!(parse("hint images").unwrap(), hint(HintGroup::Images, HintTarget::Normal));
        assert_eq!(parse("hint images tab").unwrap(), hint(HintGroup::Images, HintTarget::TabBg));
        assert_eq!(parse("hint links yank").unwrap(), hint(HintGroup::Links, HintTarget::Yank));
        assert_eq!(
            parse("hint links yank-primary").unwrap(),
            hint(HintGroup::Links, HintTarget::YankPrimary)
        );
        assert_eq!(
            parse("hint links download").unwrap(),
            hint(HintGroup::Links, HintTarget::Download)
        );
        assert_eq!(parse("hint inputs").unwrap(), hint(HintGroup::Inputs, HintTarget::Normal));

        // `--rapid` and `--first`, before the group and after it. argparse takes optionals in
        // either place, and `;r` and `gi` are one of each.
        assert_eq!(
            parse("hint --rapid links tab-bg").unwrap(),
            Command::Hint {
                group: HintGroup::Links,
                target: HintTarget::TabBg,
                rapid: true,
                first: false
            }
        );
        assert_eq!(
            parse("hint inputs --first").unwrap(),
            Command::Hint {
                group: HintGroup::Inputs,
                target: HintTarget::Normal,
                rapid: false,
                first: true
            }
        );
        assert_eq!(
            parse("hint --rapid links window").unwrap(),
            Command::Hint {
                group: HintGroup::Links,
                target: HintTarget::Window,
                rapid: true,
                first: false
            }
        );

        // `;o` and `;O`. maxsplit=2 is the whole point: the `-t` and the `-r` belong to the
        // `:open` that ends up in the command line, and a plain flag split would eat them as
        // `hint`'s own and leave `:open {hint-url}` behind.
        assert_eq!(
            parse("hint links fill :open {hint-url}").unwrap(),
            hint(HintGroup::Links, HintTarget::Fill(":open {hint-url}".to_string()))
        );
        assert_eq!(
            parse("hint links fill :open -t -r {hint-url}").unwrap(),
            hint(HintGroup::Links, HintTarget::Fill(":open -t -r {hint-url}".to_string()))
        );
        // …and those flags must not have been read as `hint`'s.
        assert!(matches!(
            parse("hint links fill :open -t -r {hint-url}").unwrap(),
            Command::Hint { rapid: false, first: false, .. }
        ));
        // `fill` with nothing to fill is an error, not a hint session that ends in silence
        // (`_check_args`, hints.py:565).
        assert!(parse("hint links fill").is_err());

        // What bru still does not do stays unimplemented rather than becoming a near miss: a
        // target it answered with a click would look like a bug in the follow, not a missing
        // feature.
        for cmd in [
            "hint all run :later 500 scroll down",
            "hint all spawn mpv {hint-url}",
            "hint all userscript view_in_mpv",
            "hint all delete",
            "hint all right-click",
            "hint --mode number links",
            "hint links yank --add-history",
            "hint whatever",
        ] {
            assert_eq!(
                parse(cmd).unwrap(),
                Command::Unimplemented(cmd.to_string()),
                "{cmd:?} should not be mistaken for something bru implements"
            );
        }
    }

    #[test]
    fn malformed_arguments_are_errors() {
        assert!(parse("scroll sideways").is_err());
        assert!(parse("scroll").is_err());
        assert!(parse("scroll-page 0 sideways").is_err());
        assert!(parse("tab-focus middle").is_err());
        assert!(parse("").is_err());
    }
}
