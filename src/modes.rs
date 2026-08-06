//! Keyboard modes and the transitions between them.
//!
//! A behavioural port of qutebrowser 3.7.0's `keyinput/modeman.py`. Stage 1's four modes, stage 2's
//! `hint` and stage 3's `caret`, `set_mark` and `jump_mark` are here — `prompt`, `yesno`,
//! `record_macro` and `run_macro` are still to come — but the transition rules are the real ones, so
//! adding a mode is a variant and a row in each `match` rather than a rewrite. `hint` was exactly
//! that, and so were the three below it.
//!
//! Nothing in this file touches CEF or Lua. It is a state machine over an enum.

use std::fmt;

/// A keyboard mode.
///
/// Names match qutebrowser's `usertypes.KeyMode`, because they are what `config.lua` writes:
/// `bru.bind("normal", "j", "scroll down")`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Mode {
    /// Vim-style command keys. The default.
    Normal,
    /// Typing into the page. Entered by `i`, or automatically when the page focuses a text field
    /// (`input.insert_mode.auto_enter`, default true).
    Insert,
    /// The `:` command line has focus.
    Command,
    /// Everything goes to the page. Only `<Shift-Escape>` gets out.
    Passthrough,
    /// Labels are drawn over the page's links and a keypress names one. Entered by `f`/`F`, left by
    /// `<Escape>` or by following a hint. `src/hints.rs` owns everything it does.
    Hint,
    /// A text cursor moves through the page's document and can drag a selection behind it. Entered
    /// by `v` (and by `V`, which also starts a line selection), left by `<Escape>` or `c`.
    /// `src/caret.rs` owns everything it does.
    Caret,
    /// The next keystroke names a mark to save the scroll position under. Entered by `` ` ``.
    SetMark,
    /// The next keystroke names a mark to jump to. Entered by `'`.
    JumpMark,
}

impl Mode {
    /// Every mode bru implements, in a stable order. Used to build one key parser per mode.
    pub const ALL: [Mode; 8] = [
        Mode::Normal,
        Mode::Insert,
        Mode::Command,
        Mode::Passthrough,
        Mode::Hint,
        Mode::Caret,
        Mode::SetMark,
        Mode::JumpMark,
    ];

    /// The name used in `configdata.yml` and in `config.lua`.
    ///
    /// `set_mark` and `jump_mark` are spelled with an underscore because that is how
    /// `usertypes.KeyMode` spells them and therefore how `mode-enter set_mark` — a real qutebrowser
    /// default binding, configdata.yml:3838 — has to be written.
    pub fn name(self) -> &'static str {
        match self {
            Mode::Normal => "normal",
            Mode::Insert => "insert",
            Mode::Command => "command",
            Mode::Passthrough => "passthrough",
            Mode::Hint => "hint",
            Mode::Caret => "caret",
            Mode::SetMark => "set_mark",
            Mode::JumpMark => "jump_mark",
        }
    }

    /// Parse a mode name as written in `config.lua` or in a `mode-enter` command.
    ///
    /// Returns `None` for names qutebrowser knows but bru does not implement yet (`prompt`,
    /// `yesno`, `register`, `record_macro`, `run_macro`) as well as for nonsense, so the caller
    /// warns once at startup either way.
    pub fn from_name(name: &str) -> Option<Mode> {
        match name {
            "normal" => Some(Mode::Normal),
            "insert" => Some(Mode::Insert),
            "command" => Some(Mode::Command),
            "passthrough" => Some(Mode::Passthrough),
            "hint" => Some(Mode::Hint),
            "caret" => Some(Mode::Caret),
            "set_mark" => Some(Mode::SetMark),
            "jump_mark" => Some(Mode::JumpMark),
            _ => None,
        }
    }

    /// Whether a single keystroke in this mode names a register rather than starting a binding.
    ///
    /// `modeparsers.RegisterKeyParser` (:245) is what `set_mark`, `jump_mark`, `record_macro` and
    /// `run_macro` are built with: it consults the bindings first — which for these modes are the
    /// `register:` section, i.e. `<Escape>: mode-leave` alone — and then takes the next ordinary key
    /// as the register's name. `src/caret.rs::handle_mark_key` is that second half.
    pub fn names_a_register(self) -> bool {
        matches!(self, Mode::SetMark | Mode::JumpMark)
    }

    /// Whether unbound keys reach the page.
    ///
    /// `modeman.init` constructs the insert, command, passthrough **and caret** parsers with
    /// `passthrough=True` (modeman.py:145–151 for caret); normal and hint are the ones that eat what
    /// they do not recognise (subject to `input.forward_unbound_keys`, see
    /// [`Mode::swallows_unmatched`]). `HintKeyParser` is built with `BaseKeyParser`'s default of
    /// `passthrough=False` — modeman.py:90 — because a stray letter while labels are up would be
    /// typed into whatever the last click focused. `RegisterKeyParser` takes the same default, and
    /// for the same reason: while `` ` `` is waiting for a mark name, no key belongs to the page.
    pub fn passthrough(self) -> bool {
        match self {
            Mode::Normal | Mode::Hint | Mode::SetMark | Mode::JumpMark => false,
            Mode::Insert | Mode::Command | Mode::Passthrough | Mode::Caret => true,
        }
    }

    /// Whether a digit prefix is read as a count.
    ///
    /// `modeman.init` passes `supports_count=False` to the insert, command, passthrough, set_mark
    /// and jump_mark parsers, so `3` in insert mode is the character `3` and nothing else, and `` `3 ``
    /// names the mark `3`. In hint mode a digit is a hint label under `hints.mode = number`, never a
    /// count. **Caret mode does count**: its parser is a plain `CommandKeyParser` (modeman.py:145)
    /// and `CommandKeyParser.__init__` defaults `supports_count=True`, which is what makes `3j`
    /// three lines and `3w` three words.
    pub fn supports_count(self) -> bool {
        match self {
            Mode::Normal | Mode::Caret => true,
            Mode::Insert
            | Mode::Command
            | Mode::Passthrough
            | Mode::Hint
            | Mode::SetMark
            | Mode::JumpMark => false,
        }
    }

    /// Whether an unmatched key should be swallowed rather than forwarded to the page.
    ///
    /// This is the tail of `modeman._handle_keypress`, with `input.forward_unbound_keys` fixed at
    /// its default of `auto`:
    ///
    /// ```text
    /// if match != NoMatch:                                    filter
    /// elif passthrough or forward == 'all'
    ///      or (forward == 'auto' and is_non_alnum):            pass
    /// else:                                                   filter
    /// ```
    ///
    /// `is_non_alnum` is "a modifier beyond Shift is held, or the key types nothing visible" —
    /// [`crate::bindings::KeyInfo::is_non_alnum`] computes it.
    pub fn swallows_unmatched(self, is_non_alnum: bool) -> bool {
        !(self.passthrough() || is_non_alnum)
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Asked to leave a mode we are not in, without `maybe` set.
///
/// `modeman.NotInModeError`.
#[derive(Debug, PartialEq, Eq)]
pub struct NotInModeError(pub Mode);

impl fmt::Display for NotInModeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Not in mode {}!", self.0)
    }
}

/// What a transition asks the caller to do besides changing the mode.
///
/// `ModeManager` is pure — it cannot blur a text field or focus the command line — so it reports
/// what happened and `src/keys.rs` does the CEF part.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct Transition {
    /// The mode that was left, if any. `None` when the request was ignored.
    pub left: Option<Mode>,
    /// The mode now current, if it changed. `None` when the request was ignored.
    pub entered: Option<Mode>,
    /// The pending key chain of the mode that was left must be cleared.
    ///
    /// `modeman.leave` clears it unconditionally — see qutebrowser issue 1805.
    pub clear_keychain: bool,
}

impl Transition {
    /// Nothing happened; the request was a no-op.
    pub const IGNORED: Transition = Transition { left: None, entered: None, clear_keychain: false };

    /// Whether the mode actually changed.
    // The callers all read `entered` directly; this is the same question spelled for the tests.
    #[allow(dead_code)]
    pub fn changed(&self) -> bool {
        self.entered.is_some()
    }
}

/// The current mode and the rules for changing it.
///
/// One per window. `src/state.rs` owns it.
#[derive(Debug)]
pub struct ModeManager {
    mode: Mode,
    /// The mode to restore after a prompt. Always `Normal` while bru has no prompt modes; kept so
    /// that adding them is a change to `enter`/`leave` and not to the struct.
    prev_mode: Mode,
}

impl Default for ModeManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ModeManager {
    pub fn new() -> Self {
        ModeManager { mode: Mode::Normal, prev_mode: Mode::Normal }
    }

    /// The mode keys are currently routed to.
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Enter a mode. Port of `ModeManager.enter`.
    ///
    /// - Entering `Normal` is spelled as leaving whatever is current.
    /// - Entering the mode we are already in is ignored.
    /// - `only_if_normal` makes the request conditional; it is how
    ///   `input.insert_mode.auto_enter` avoids yanking you out of passthrough when a page focuses
    ///   a field.
    /// - Entering from a non-normal mode overrides it: the old mode is left first.
    pub fn enter(&mut self, mode: Mode, only_if_normal: bool) -> Transition {
        if mode == Mode::Normal {
            return self.leave(self.mode, true).unwrap_or(Transition::IGNORED);
        }

        if self.mode == mode {
            return Transition::IGNORED;
        }

        let mut left = None;
        if self.mode != Mode::Normal {
            if only_if_normal {
                return Transition::IGNORED;
            }
            left = Some(self.mode);
        }

        // With no prompt modes there is nothing to restore, so this is always Normal. The branch
        // in modeman.py exists for `mode in PROMPT_MODES and self.mode in INPUT_MODES`.
        self.prev_mode = Mode::Normal;
        self.mode = mode;
        Transition { left, entered: Some(mode), clear_keychain: left.is_some() }
    }

    /// Leave a mode, returning to `Normal`. Port of `ModeManager.leave`.
    ///
    /// `maybe` turns "we are not in that mode" from an error into a no-op — used when something
    /// asynchronous (a page blurring its own field) asks to leave insert mode.
    pub fn leave(&mut self, mode: Mode, maybe: bool) -> Result<Transition, NotInModeError> {
        if self.mode != mode {
            if maybe {
                return Ok(Transition::IGNORED);
            }
            return Err(NotInModeError(mode));
        }

        if mode == Mode::Normal {
            // modeman.leave would set mode to normal again and emit; nothing observable happens,
            // and `mode-leave` refuses this case separately (see `leave_current`).
            return Ok(Transition {
                left: Some(Mode::Normal),
                entered: Some(Mode::Normal),
                clear_keychain: true,
            });
        }

        self.mode = Mode::Normal;
        Ok(Transition {
            left: Some(mode),
            entered: Some(Mode::Normal),
            clear_keychain: true,
        })
    }

    /// The `mode-leave` command. Port of `ModeManager.mode_leave`, which is registered with
    /// `not_modes=[normal]` and raises if called there anyway.
    pub fn leave_current(&mut self) -> Result<Transition, NotInModeError> {
        if self.mode == Mode::Normal {
            return Err(NotInModeError(Mode::Normal));
        }
        self.leave(self.mode, false)
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for mode in Mode::ALL {
            assert_eq!(Mode::from_name(mode.name()), Some(mode));
        }
        assert_eq!(Mode::from_name("hint"), Some(Mode::Hint));
        assert_eq!(Mode::from_name("caret"), Some(Mode::Caret));
        // The two register modes keep `usertypes.KeyMode`'s underscored spelling, because
        // `mode-enter set_mark` is a qutebrowser default binding and has to parse verbatim.
        assert_eq!(Mode::from_name("set_mark"), Some(Mode::SetMark));
        assert_eq!(Mode::from_name("jump_mark"), Some(Mode::JumpMark));
        assert_eq!(Mode::from_name("set-mark"), None);
        // Modes qutebrowser has and bru does not, yet.
        assert_eq!(Mode::from_name("prompt"), None);
        assert_eq!(Mode::from_name("yesno"), None);
        assert_eq!(Mode::from_name("record_macro"), None);
        assert_eq!(Mode::from_name("nonsense"), None);
    }

    #[test]
    fn caret_counts_and_forwards_but_the_mark_modes_do_neither() {
        // modeman.py:145 builds caret's parser as a plain CommandKeyParser with passthrough=True and
        // the default supports_count=True. Both matter: `3j` is three lines, and a key caret mode
        // does not bind reaches the page rather than being eaten.
        assert!(Mode::Caret.supports_count());
        assert!(Mode::Caret.passthrough());
        assert!(!Mode::Caret.swallows_unmatched(false));

        // RegisterKeyParser (modeparsers.py:245) passes supports_count=False and leaves passthrough
        // at BaseKeyParser's default of False. While `` ` `` waits for a mark name, `3` is the mark
        // named 3 and nothing reaches the page.
        for mode in [Mode::SetMark, Mode::JumpMark] {
            assert!(!mode.supports_count(), "{mode} must not read counts");
            assert!(!mode.passthrough(), "{mode} must not forward to the page");
            assert!(mode.names_a_register(), "{mode} takes the next key as a register name");
            assert!(mode.swallows_unmatched(false));
        }
        for mode in [Mode::Normal, Mode::Insert, Mode::Command, Mode::Passthrough, Mode::Hint, Mode::Caret] {
            assert!(!mode.names_a_register(), "{mode} is not a register mode");
        }
    }

    #[test]
    fn only_normal_mode_counts_and_only_normal_mode_eats_keys() {
        // modeman.init: every parser but normal's gets supports_count=False, and every one but
        // normal's and hint's gets passthrough=True.
        assert!(Mode::Normal.supports_count());
        assert!(!Mode::Normal.passthrough());
        for mode in [Mode::Insert, Mode::Command, Mode::Passthrough] {
            assert!(!mode.supports_count(), "{mode} must not read counts");
            assert!(mode.passthrough(), "{mode} must forward unbound keys");
        }
        // Hint mode counts nothing and forwards nothing: a key while labels are up names a hint or
        // is thrown away, and either way the page must not see it.
        assert!(!Mode::Hint.supports_count());
        assert!(!Mode::Hint.passthrough());
        assert!(Mode::Hint.swallows_unmatched(false));
    }

    #[test]
    fn passthrough_swallows_nothing_unmatched() {
        // The point of passthrough mode: an unmatched key always reaches the page, whether or not
        // it is alphanumeric. Only a match (i.e. <Shift-Escape>) is filtered — see the bindings
        // test `passthrough_binds_only_shift_escape`.
        for is_non_alnum in [false, true] {
            assert!(!Mode::Passthrough.swallows_unmatched(is_non_alnum));
        }
        // Normal mode, by contrast, eats unmatched alphanumerics and forwards the rest
        // (input.forward_unbound_keys = auto).
        assert!(Mode::Normal.swallows_unmatched(false));
        assert!(!Mode::Normal.swallows_unmatched(true));
    }

    #[test]
    fn enter_and_leave() {
        let mut m = ModeManager::new();
        assert_eq!(m.mode(), Mode::Normal);

        let t = m.enter(Mode::Insert, false);
        assert_eq!(t.entered, Some(Mode::Insert));
        assert_eq!(t.left, None);
        assert_eq!(m.mode(), Mode::Insert);

        // Entering the mode we are in is ignored.
        assert_eq!(m.enter(Mode::Insert, false), Transition::IGNORED);
        assert_eq!(m.mode(), Mode::Insert);

        // Leaving returns to normal and clears the keychain.
        let t = m.leave(Mode::Insert, false).unwrap();
        assert_eq!(t.left, Some(Mode::Insert));
        assert_eq!(t.entered, Some(Mode::Normal));
        assert!(t.clear_keychain);
        assert_eq!(m.mode(), Mode::Normal);
    }

    #[test]
    fn entering_normal_means_leaving() {
        let mut m = ModeManager::new();
        m.enter(Mode::Passthrough, false);
        let t = m.enter(Mode::Normal, false);
        assert_eq!(t.left, Some(Mode::Passthrough));
        assert_eq!(m.mode(), Mode::Normal);
    }

    #[test]
    fn entering_overrides_a_non_normal_mode() {
        let mut m = ModeManager::new();
        m.enter(Mode::Passthrough, false);
        let t = m.enter(Mode::Command, false);
        assert_eq!(t.left, Some(Mode::Passthrough));
        assert_eq!(t.entered, Some(Mode::Command));
        assert!(t.clear_keychain);
        assert_eq!(m.mode(), Mode::Command);
    }

    #[test]
    fn only_if_normal_does_not_override() {
        // This is what stops a page's focus event from dragging you out of passthrough mode.
        let mut m = ModeManager::new();
        m.enter(Mode::Passthrough, false);
        assert_eq!(m.enter(Mode::Insert, true), Transition::IGNORED);
        assert_eq!(m.mode(), Mode::Passthrough);

        // From normal it goes through.
        let mut m = ModeManager::new();
        assert!(m.enter(Mode::Insert, true).changed());
        assert_eq!(m.mode(), Mode::Insert);
    }

    #[test]
    fn leaving_a_mode_we_are_not_in() {
        let mut m = ModeManager::new();
        m.enter(Mode::Insert, false);
        assert_eq!(m.leave(Mode::Command, false), Err(NotInModeError(Mode::Command)));
        assert_eq!(m.mode(), Mode::Insert, "a failed leave must not change the mode");
        assert_eq!(m.leave(Mode::Command, true), Ok(Transition::IGNORED));
        assert_eq!(m.mode(), Mode::Insert);
    }

    #[test]
    fn mode_leave_refuses_in_normal_mode() {
        let mut m = ModeManager::new();
        assert_eq!(m.leave_current(), Err(NotInModeError(Mode::Normal)));
        m.enter(Mode::Insert, false);
        assert!(m.leave_current().unwrap().changed());
        assert_eq!(m.mode(), Mode::Normal);
    }
}
