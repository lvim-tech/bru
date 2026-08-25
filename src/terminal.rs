//! Which terminal bru was launched from, and how to ask it for a pane of its own.
//!
//! **Not part of `spawn.rs`, and the line between them is the one that matters.** `spawn.rs` runs a
//! user's command against a *browser*: it substitutes `{url}` and `{title}` from the page the key
//! was aimed at (`Values::current`), and everything it knows comes from that tab. Which terminal
//! bru was started from is not a fact about a page — it is a fact about the process, fixed before
//! the first window existed, and the terminal frontend will need exactly the same answer with no
//! browser in hand at all. So this module knows nothing about tabs and `spawn.rs` calls into it.
//!
//! Everything here is a pure function of the environment it is handed, so the tests drive it with a
//! synthetic one rather than by setting variables in a process that other tests share.
//!
//! ## Which terminals, and why only these three
//!
//! One remote-control command each, all three already shipped by the terminals themselves:
//!
//! | terminal | how a pane is asked for |
//! |---|---|
//! | kitty   | `kitten @ launch --location=vsplit` |
//! | tmux    | `tmux split-window -h` |
//! | wezterm | `wezterm cli split-pane --right` |
//!
//! **Ghostty is deliberately absent.** It splits through keybind actions and has no remote-control
//! CLI to drive from another process, so there is nothing here to build against — not an oversight
//! and not a gap that waiting will close. Checked 2026-08-25.

use std::ffi::OsStr;

/// A terminal bru can ask for a pane, or the honest absence of one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Terminal {
    Kitty,
    Tmux,
    Wezterm,
    /// Something else, or no terminal at all — a `.desktop` launch has neither.
    Unknown,
}

/// What terminal this process is running under.
pub fn detect() -> Terminal {
    detect_from(
        std::env::var_os("TMUX").as_deref(),
        std::env::var_os("KITTY_WINDOW_ID").as_deref(),
        std::env::var_os("WEZTERM_PANE").as_deref(),
    )
}

/// [`detect`] as a function of its inputs, so it can be tested.
///
/// **`$TMUX` is checked first and that ordering is the whole content of this function.** A kitty
/// running tmux sets both `KITTY_WINDOW_ID` and `TMUX`, and asking *kitty* to split would put the
/// new window beside the terminal — outside the multiplexer, in a pane tmux does not manage and the
/// user is not looking at. The innermost thing that owns panes is the one that must be asked.
///
/// An empty value is not a terminal. tmux sets `$TMUX` to a socket path, kitty to a window id, and
/// wezterm to a pane id; a variable exported empty by a shell profile is not a report of anything.
fn detect_from(
    tmux: Option<&OsStr>,
    kitty: Option<&OsStr>,
    wezterm: Option<&OsStr>,
) -> Terminal {
    let set = |value: Option<&OsStr>| value.is_some_and(|value| !value.is_empty());
    if set(tmux) {
        return Terminal::Tmux;
    }
    if set(kitty) {
        return Terminal::Kitty;
    }
    if set(wezterm) {
        return Terminal::Wezterm;
    }
    Terminal::Unknown
}

/// The command line that runs `argv` in a new pane beside this one, or `None` when there is no
/// terminal to ask.
///
/// The split is vertical — a new pane to the right — because that is the shape a browser beside a
/// shell wants on a wide screen, and it is what every one of these terminals spells with one flag.
///
/// **`--` before `argv` everywhere it is accepted.** The command being run is a user's, and a
/// program name beginning with a dash would otherwise be read by the terminal as a flag of its own.
/// tmux is the exception: its `split-window` takes `[shell-command [argument ...]]` positionally
/// with no `--` in its grammar, so the guard there is that the words go in as separate arguments
/// rather than as one string a shell would re-split.
pub fn split_command(terminal: Terminal, argv: &[String]) -> Option<Vec<String>> {
    if argv.is_empty() {
        return None;
    }
    let mut line: Vec<String> = match terminal {
        // `--cwd=current` so the pane starts where bru was started, which is what a person
        // splitting a shell expects. `--location=vsplit` is honoured by the `splits` layout and
        // ignored by the others, in which case kitty places the window its own way — a worse
        // position, never a failure.
        Terminal::Kitty => ["kitten", "@", "launch", "--location=vsplit", "--cwd=current", "--"]
            .iter()
            .map(|word| word.to_string())
            .collect(),
        Terminal::Tmux => ["tmux", "split-window", "-h"].iter().map(|w| w.to_string()).collect(),
        Terminal::Wezterm => ["wezterm", "cli", "split-pane", "--right", "--"]
            .iter()
            .map(|w| w.to_string())
            .collect(),
        Terminal::Unknown => return None,
    };
    line.extend(argv.iter().cloned());
    Some(line)
}

/// Why a split could not be asked for, in the words of the terminal that would have to allow it.
///
/// kitty refuses remote control unless it was told to allow it, and the failure looks like the
/// command not existing rather than like a permission — so the name of the setting is the whole
/// message. tmux and wezterm need no such permission.
pub fn split_hint(terminal: Terminal) -> &'static str {
    match terminal {
        Terminal::Kitty => {
            "kitty refuses remote control unless `allow_remote_control yes` is in kitty.conf \
             (or kitty was started with --listen-on)"
        }
        Terminal::Tmux => "tmux is set but `tmux split-window` did not run",
        Terminal::Wezterm => "wezterm is set but `wezterm cli split-pane` did not run",
        Terminal::Unknown => {
            "no terminal bru can ask for a pane: $TMUX, $KITTY_WINDOW_ID and $WEZTERM_PANE are all \
             unset, which is what a .desktop launch looks like"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn v(value: &str) -> OsString {
        OsString::from(value)
    }

    /// **The case the ordering exists for.** Both are set inside tmux inside kitty, and splitting
    /// the kitty window would put the pane where the multiplexer is not.
    #[test]
    fn tmux_inside_kitty_is_tmux() {
        let (tmux, kitty) = (v("/tmp/tmux-1000/default,123,0"), v("1"));
        assert_eq!(detect_from(Some(&tmux), Some(&kitty), None), Terminal::Tmux);
    }

    #[test]
    fn each_terminal_is_recognised_on_its_own() {
        let value = v("1");
        assert_eq!(detect_from(Some(&value), None, None), Terminal::Tmux);
        assert_eq!(detect_from(None, Some(&value), None), Terminal::Kitty);
        assert_eq!(detect_from(None, None, Some(&value)), Terminal::Wezterm);
        assert_eq!(detect_from(None, None, None), Terminal::Unknown);
    }

    /// A variable exported empty says nothing, and reading it as a terminal would build a command
    /// line for a pane owner that is not there.
    #[test]
    fn an_empty_variable_is_not_a_terminal() {
        let empty = v("");
        assert_eq!(detect_from(Some(&empty), None, None), Terminal::Unknown);
        assert_eq!(detect_from(Some(&empty), Some(&empty), Some(&empty)), Terminal::Unknown);
        // …and an empty inner one does not hide a real outer one.
        let kitty = v("3");
        assert_eq!(detect_from(Some(&empty), Some(&kitty), None), Terminal::Kitty);
    }

    #[test]
    fn the_three_command_lines_are_what_the_terminals_document() {
        let argv = vec!["htop".to_string(), "-d".to_string(), "5".to_string()];
        assert_eq!(
            split_command(Terminal::Kitty, &argv).unwrap(),
            [
                "kitten", "@", "launch", "--location=vsplit", "--cwd=current", "--", "htop", "-d",
                "5"
            ]
        );
        assert_eq!(
            split_command(Terminal::Tmux, &argv).unwrap(),
            ["tmux", "split-window", "-h", "htop", "-d", "5"]
        );
        assert_eq!(
            split_command(Terminal::Wezterm, &argv).unwrap(),
            ["wezterm", "cli", "split-pane", "--right", "--", "htop", "-d", "5"]
        );
        assert_eq!(split_command(Terminal::Unknown, &argv), None);
    }

    /// **The guard against a program name that begins with a dash**, which is a user's command and
    /// so is not bru's to trust. Two of the three terminals take `--`; tmux's grammar has none, and
    /// what stands in for it there is that the words never become one string.
    #[test]
    fn a_dashed_program_name_is_not_read_as_a_flag() {
        let argv = vec!["--not-a-flag".to_string()];
        let kitty = split_command(Terminal::Kitty, &argv).unwrap();
        assert_eq!(kitty[kitty.len() - 2], "--");
        let wezterm = split_command(Terminal::Wezterm, &argv).unwrap();
        assert_eq!(wezterm[wezterm.len() - 2], "--");
        assert_eq!(
            split_command(Terminal::Tmux, &argv).unwrap(),
            ["tmux", "split-window", "-h", "--not-a-flag"]
        );
    }

    /// Nothing to run is not a pane worth opening.
    #[test]
    fn an_empty_command_asks_for_no_pane() {
        assert_eq!(split_command(Terminal::Kitty, &[]), None);
    }
}
