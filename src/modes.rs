//! Keyboard modes and the transitions between them.
//!
//! A behavioural port of qutebrowser 3.7.0's `keyinput/modeman.py`. Stage 1's four modes, stage 2's
//! `hint`, stage 3's `caret`, `set_mark`, `jump_mark`, `record_macro` and `run_macro`, and now
//! `prompt` and `yesno` — every mode `usertypes.KeyMode` has — are here, and the transition rules
//! are the real ones, so adding a mode is a variant and a row in each `match` rather than a rewrite.
//! `hint` was exactly that, and so were the six below it.
//!
//! **The two prompt modes are the first that are entered by something that is not a keypress**, and
//! they are why `prev_mode` below stopped being a placeholder: a question that arrives while a page
//! is being typed into has to hand insert mode back when it is answered, or a `confirm()` from a
//! script drops the user out of the field they were in and the next letter scrolls.
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
    /// The next keystroke names the register a macro is recorded into. Entered by `q` with no
    /// argument. `src/macros.rs` owns everything it does.
    RecordMacro,
    /// The next keystroke names the register a macro is replayed from. Entered by `@` with no
    /// argument. `src/macros.rs` owns everything it does.
    RunMacro,
    /// A question with a text answer is waiting in the bar — a page's `prompt()`, a login, a
    /// download's filename. Never entered by a key: something asked. `src/prompt.rs` owns it.
    Prompt,
    /// A question with a yes/no answer is waiting — a page's `confirm()`, "leave site?", a
    /// permission, a certificate. `src/prompt.rs` owns it.
    YesNo,
}

impl Mode {
    /// Every mode bru implements, in a stable order. Used to build one key parser per mode.
    pub const ALL: [Mode; 12] = [
        Mode::Normal,
        Mode::Insert,
        Mode::Command,
        Mode::Passthrough,
        Mode::Hint,
        Mode::Caret,
        Mode::SetMark,
        Mode::JumpMark,
        Mode::RecordMacro,
        Mode::RunMacro,
        Mode::Prompt,
        Mode::YesNo,
    ];

    /// The name used in `configdata.yml` and in `config.lua`.
    ///
    /// `set_mark`, `jump_mark`, `record_macro` and `run_macro` are spelled with an underscore
    /// because that is how `usertypes.KeyMode` spells them and therefore how `mode-enter set_mark` —
    /// a real qutebrowser default binding, configdata.yml:3838 — has to be written.
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
            Mode::RecordMacro => "record_macro",
            Mode::RunMacro => "run_macro",
            Mode::Prompt => "prompt",
            Mode::YesNo => "yesno",
        }
    }

    /// Parse a mode name as written in `config.lua` or in a `mode-enter` command.
    ///
    /// Returns `None` for `register` and for nonsense, so the caller warns once at startup either
    /// way. `register` stays `None` on purpose even though all four modes built on
    /// `RegisterKeyParser` exist: it is the name of the *bindings section* those modes read
    /// (configdata.yml:3991), not a mode `mode-enter` can be given.
    ///
    /// `prompt` and `yesno` **do** parse — `bindings.default.prompt` and `.yesno` are real sections
    /// of `configdata.yml` and a `config.lua` has to be able to rebind inside them. What refuses
    /// them is `mode-enter`, which is a different question and is answered in `commands.rs`:
    /// `modeman.mode_enter` (modeman.py:401-405) raises for hint, command, yesno, prompt and
    /// register alike, because those five are entered by something happening and never by asking.
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
            "record_macro" => Some(Mode::RecordMacro),
            "run_macro" => Some(Mode::RunMacro),
            "prompt" => Some(Mode::Prompt),
            "yesno" => Some(Mode::YesNo),
            _ => None,
        }
    }

    /// Whether a question is waiting in this mode. `modeman.PROMPT_MODES` (modeman.py:25).
    ///
    /// Three rules hang off it, and all three are in [`ModeManager`]: one prompt mode never
    /// *overrides* the other (a `confirm()` arriving while a `prompt()` is open is queued, not
    /// swapped in under the user's fingers), entering one remembers an input mode to give back, and
    /// leaving one gives it back.
    pub fn is_prompt(self) -> bool {
        matches!(self, Mode::Prompt | Mode::YesNo)
    }

    /// Whether keys in this mode belong to the page. `modeman.INPUT_MODES` (modeman.py:24).
    ///
    /// The only thing that reads it is the prompt restore: these are the two modes worth handing
    /// back, because they are the two a user is *in the middle of* rather than passing through.
    pub fn is_input(self) -> bool {
        matches!(self, Mode::Insert | Mode::Passthrough)
    }

    /// Whether `mode-enter <name>` may put the window into this mode.
    ///
    /// `modeman.mode_enter` (modeman.py:401-405) refuses hint, command, yesno, prompt and register:
    /// each is entered by something happening — a question, a `:`, an `f` — and a bare `mode-enter
    /// yesno` would leave the bar in a mode with nothing behind it and no way out but `<Escape>`.
    /// `register` is not a `Mode` here at all, so the list below is four names and not five.
    pub fn can_be_entered_by_command(self) -> bool {
        !matches!(self, Mode::Hint | Mode::Command | Mode::Prompt | Mode::YesNo)
    }

    /// Whether a single keystroke in this mode names a register rather than starting a binding.
    ///
    /// `modeparsers.RegisterKeyParser` (:245) is what `set_mark`, `jump_mark`, `record_macro` and
    /// `run_macro` are built with: it consults the bindings first — which for these modes are the
    /// `register:` section, i.e. `<Escape>: mode-leave` alone — and then takes the next ordinary key
    /// as the register's name. `src/caret.rs::handle_mark_key` is that second half.
    pub fn names_a_register(self) -> bool {
        matches!(
            self,
            Mode::SetMark | Mode::JumpMark | Mode::RecordMacro | Mode::RunMacro
        )
    }

    /// Whether a key this mode does not bind is typed into a text field bru is drawing.
    ///
    /// Only `command` and `prompt` — the two modes with a real `<input>` in the bottom strip — and
    /// `prompt` only when the open question has a line to type into, which `src/prompt.rs` answers
    /// separately. `yesno` has no input at all: `y`, `n`, `Y` and `N` are bindings, and every other
    /// letter is thrown away rather than typed anywhere.
    pub fn types_into_the_bar(self) -> bool {
        matches!(self, Mode::Command | Mode::Prompt)
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
    ///
    /// **The two prompt modes disagree with each other, and that is qutebrowser's own arrangement.**
    /// `prompt`'s parser is built with `passthrough=True` (modeman.py:127-135) and `yesno`'s is not
    /// (modeman.py:137-143): a `prompt()` has a line edit, so a letter it does not bind is a letter
    /// being typed, while in `yesno` there is nothing to type into and a stray key must go nowhere.
    /// For `prompt` this only decides what happens to a key bru does not claim — `keys.rs` aims the
    /// forwarding at the bar's own input, not at the page, because the page is not what has focus.
    pub fn passthrough(self) -> bool {
        match self {
            Mode::Normal
            | Mode::Hint
            | Mode::SetMark
            | Mode::JumpMark
            | Mode::RecordMacro
            | Mode::RunMacro
            | Mode::YesNo => false,
            Mode::Insert | Mode::Command | Mode::Passthrough | Mode::Caret | Mode::Prompt => true,
        }
    }

    /// Whether a digit prefix is read as a count.
    ///
    /// `modeman.init` passes `supports_count=False` to the insert, command, passthrough and every
    /// register parser, so `3` in insert mode is the character `3` and nothing else, and `` `3 ``
    /// names the mark `3`. In hint mode a digit is a hint label under `hints.mode = number`, never a
    /// count. **Caret mode does count**: its parser is a plain `CommandKeyParser` (modeman.py:145)
    /// and `CommandKeyParser.__init__` defaults `supports_count=True`, which is what makes `3j`
    /// three lines and `3w` three words.
    ///
    /// `3@q` still runs the macro three times, and that is not a contradiction: the `3` is read in
    /// *normal* mode, by `@`'s own parser, and `macro-run` stashes it before `run_macro` mode opens
    /// (`macros.py:79`, `self._macro_count[win_id] = count`). By the time the register key is
    /// pressed the count is already spent.
    pub fn supports_count(self) -> bool {
        match self {
            Mode::Normal | Mode::Caret => true,
            Mode::Insert
            | Mode::Command
            | Mode::Passthrough
            | Mode::Hint
            | Mode::SetMark
            | Mode::JumpMark
            | Mode::RecordMacro
            | Mode::RunMacro
            // Both prompt parsers are built with `supports_count=False` (modeman.py:127-143), and
            // in `prompt` mode that is the difference between typing `3` into a filename and
            // waiting for a command to give it to.
            | Mode::Prompt
            | Mode::YesNo => false,
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
    /// The mode to restore after a prompt — `modeman.ModeManager._prev_mode`.
    ///
    /// It was a placeholder for as long as there were no prompt modes. Now it is the whole of
    /// "answering a question puts you back where you were": a `confirm()` raised while a page's
    /// text field had focus leaves insert mode behind, and without this the answer would drop the
    /// window into normal mode with the caret still blinking in the field.
    ///
    /// Only [`Mode::is_input`] modes are remembered. Everything else restores to `Normal`, which is
    /// what qutebrowser does and what stops a question raised during a hint session from putting
    /// the labels' mode back with the labels long gone.
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
    /// - **One prompt mode never overrides the other.** `modeman.enter`'s condition is
    ///   `self.mode == mode or (self.mode in PROMPT_MODES and mode in PROMPT_MODES)`
    ///   (modeman.py:365-366), so a `confirm()` arriving while a `prompt()` is open is ignored here
    ///   and queued by `src/prompt.rs` instead. Without it the second question would take the
    ///   first's place under the user's fingers, and the keystroke aimed at the first would answer
    ///   the second.
    pub fn enter(&mut self, mode: Mode, only_if_normal: bool) -> Transition {
        if mode == Mode::Normal {
            return self.leave(self.mode, true).unwrap_or(Transition::IGNORED);
        }

        if self.mode == mode || (self.mode.is_prompt() && mode.is_prompt()) {
            return Transition::IGNORED;
        }

        let mut left = None;
        if self.mode != Mode::Normal {
            if only_if_normal {
                return Transition::IGNORED;
            }
            left = Some(self.mode);
        }

        // `if mode in PROMPT_MODES and self.mode in INPUT_MODES` (modeman.py:379-382), and the
        // `else` really is unconditional: every other transition forgets whatever was remembered,
        // so a question answered long after an unrelated mode change cannot restore it.
        self.prev_mode = if mode.is_prompt() && self.mode.is_input() {
            self.mode
        } else {
            Mode::Normal
        };
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

        // `if mode in PROMPT_MODES: self.enter(self._prev_mode, ...)` — modeman.py:436-438. The
        // restore is spelled as a real `enter`, not as an assignment, so that entering `Normal`
        // stays the no-op it is everywhere else and so that `prev_mode` is cleared by the same
        // rule that sets it. What the caller is told is the mode it is now in, which for a prompt
        // raised over a text field is `insert` and not `normal`.
        if mode.is_prompt() {
            let restore = self.prev_mode;
            self.enter(restore, false);
        }

        Ok(Transition {
            left: Some(mode),
            entered: Some(self.mode),
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
        assert_eq!(Mode::from_name("record_macro"), Some(Mode::RecordMacro));
        assert_eq!(Mode::from_name("run_macro"), Some(Mode::RunMacro));
        assert_eq!(Mode::from_name("set-mark"), None);
        assert_eq!(Mode::from_name("record-macro"), None);
        // The two prompt modes parse, because `bindings.default.prompt` and `.yesno` are real
        // sections a `config.lua` must be able to rebind inside. What they may not be is the
        // argument of `mode-enter` — a different question, asked below.
        assert_eq!(Mode::from_name("prompt"), Some(Mode::Prompt));
        assert_eq!(Mode::from_name("yesno"), Some(Mode::YesNo));
        assert!(!Mode::Prompt.can_be_entered_by_command());
        assert!(!Mode::YesNo.can_be_entered_by_command());
        assert!(!Mode::Hint.can_be_entered_by_command());
        assert!(!Mode::Command.can_be_entered_by_command());
        for mode in [Mode::Insert, Mode::Passthrough, Mode::Caret, Mode::SetMark, Mode::JumpMark] {
            assert!(mode.can_be_entered_by_command(), "{mode} is a mode-enter argument");
        }
        // Not a mode at all: `register` names the bindings section the four register modes read.
        assert_eq!(Mode::from_name("register"), None);
        assert_eq!(Mode::from_name("nonsense"), None);
    }

    #[test]
    fn caret_counts_and_forwards_but_the_register_modes_do_neither() {
        // modeman.py:145 builds caret's parser as a plain CommandKeyParser with passthrough=True and
        // the default supports_count=True. Both matter: `3j` is three lines, and a key caret mode
        // does not bind reaches the page rather than being eaten.
        assert!(Mode::Caret.supports_count());
        assert!(Mode::Caret.passthrough());
        assert!(!Mode::Caret.swallows_unmatched(false));

        // RegisterKeyParser (modeparsers.py:245) passes supports_count=False and leaves passthrough
        // at BaseKeyParser's default of False. While `` ` `` waits for a mark name, `3` is the mark
        // named 3 and nothing reaches the page. All four modes built on it answer the same, which
        // is the whole reason `q` and `@` were two arms in an existing match and not a new parser.
        for mode in [Mode::SetMark, Mode::JumpMark, Mode::RecordMacro, Mode::RunMacro] {
            assert!(!mode.supports_count(), "{mode} must not read counts");
            assert!(!mode.passthrough(), "{mode} must not forward to the page");
            assert!(mode.names_a_register(), "{mode} takes the next key as a register name");
            assert!(mode.swallows_unmatched(false));
        }
        for mode in [Mode::Normal, Mode::Insert, Mode::Command, Mode::Passthrough, Mode::Hint, Mode::Caret] {
            assert!(!mode.names_a_register(), "{mode} is not a register mode");
        }
    }

    /// The two prompt parsers disagree about passthrough, and it is not an oversight in either
    /// this file or qutebrowser's: `prompt` is built with `passthrough=True` (modeman.py:127-135)
    /// because it has a line edit, `yesno` is not (modeman.py:137-143) because it has nothing to
    /// type into. Neither counts.
    #[test]
    fn a_prompt_forwards_what_it_does_not_bind_and_a_yesno_eats_it() {
        assert!(Mode::Prompt.passthrough());
        assert!(!Mode::Prompt.swallows_unmatched(false));
        assert!(Mode::Prompt.types_into_the_bar());

        assert!(!Mode::YesNo.passthrough());
        assert!(Mode::YesNo.swallows_unmatched(false));
        assert!(
            !Mode::YesNo.types_into_the_bar(),
            "a yes/no question has no input, so a stray letter must go nowhere at all"
        );

        for mode in [Mode::Prompt, Mode::YesNo] {
            assert!(!mode.supports_count(), "{mode} must not read counts");
            assert!(!mode.names_a_register());
            assert!(mode.is_prompt());
            assert!(!mode.is_input());
        }
        // Command mode is the other half of the typing exception, and the only other half.
        assert!(Mode::Command.types_into_the_bar());
        for mode in [Mode::Normal, Mode::Insert, Mode::Passthrough, Mode::Hint, Mode::Caret] {
            assert!(!mode.types_into_the_bar(), "{mode} has no input in the bar");
            assert!(!mode.is_prompt(), "{mode} is not a prompt mode");
        }
        assert!(Mode::Insert.is_input() && Mode::Passthrough.is_input());
    }

    /// A second question does not take the first one's place. `modeman.enter` ignores the request
    /// when both modes are prompt modes (modeman.py:365-366), and `prompt.rs` queues instead.
    ///
    /// Without this the keystroke aimed at the first question answers the second, which is the
    /// worst thing a modal dialog can do: the user reads "delete everything?" and presses the `y`
    /// they had already decided on for "allow notifications?".
    #[test]
    fn one_prompt_mode_never_overrides_the_other() {
        let mut m = ModeManager::new();
        assert!(m.enter(Mode::YesNo, false).changed());
        assert_eq!(m.enter(Mode::Prompt, false), Transition::IGNORED);
        assert_eq!(m.mode(), Mode::YesNo);
        // ...and the same the other way round.
        let mut m = ModeManager::new();
        m.enter(Mode::Prompt, false);
        assert_eq!(m.enter(Mode::YesNo, false), Transition::IGNORED);
        assert_eq!(m.mode(), Mode::Prompt);
    }

    /// Answering a question puts the window back where the question found it, but only when that
    /// was insert or passthrough — `modeman`'s `INPUT_MODES`.
    #[test]
    fn a_prompt_hands_an_input_mode_back_and_anything_else_back_to_normal() {
        // The case this exists for: a page's `confirm()` while a field is being typed into.
        let mut m = ModeManager::new();
        m.enter(Mode::Insert, false);
        let t = m.enter(Mode::YesNo, false);
        assert_eq!(t.left, Some(Mode::Insert));
        assert_eq!(t.entered, Some(Mode::YesNo));
        let t = m.leave(Mode::YesNo, false).unwrap();
        assert_eq!(t.left, Some(Mode::YesNo));
        assert_eq!(
            t.entered,
            Some(Mode::Insert),
            "the caret is still in the page's field, so the mode has to be insert again"
        );
        assert_eq!(m.mode(), Mode::Insert);

        // Passthrough is the other input mode, and is restored the same way.
        let mut m = ModeManager::new();
        m.enter(Mode::Passthrough, false);
        m.enter(Mode::Prompt, false);
        assert_eq!(m.leave(Mode::Prompt, false).unwrap().entered, Some(Mode::Passthrough));

        // Anything else restores to normal: a question raised during a hint session must not put
        // hint mode back with the labels gone.
        for from in [Mode::Normal, Mode::Hint, Mode::Caret, Mode::Command] {
            let mut m = ModeManager::new();
            if from != Mode::Normal {
                m.enter(from, false);
            }
            m.enter(Mode::Prompt, false);
            assert_eq!(m.mode(), Mode::Prompt);
            let t = m.leave(Mode::Prompt, false).unwrap();
            assert_eq!(t.entered, Some(Mode::Normal), "a prompt over {from} restores normal");
            assert_eq!(m.mode(), Mode::Normal);
        }

        // And the memory is not kept for a second question: entering insert, prompting, answering,
        // then prompting again from normal must not resurrect insert.
        let mut m = ModeManager::new();
        m.enter(Mode::Insert, false);
        m.enter(Mode::Prompt, false);
        m.leave(Mode::Prompt, false).unwrap();
        m.leave(Mode::Insert, false).unwrap();
        m.enter(Mode::YesNo, false);
        assert_eq!(m.leave(Mode::YesNo, false).unwrap().entered, Some(Mode::Normal));
        assert_eq!(m.mode(), Mode::Normal);
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
