//! Keyboard macros: `q` records into a register, `@` replays it.
//!
//! A behavioural port of qutebrowser 3.7.0's `keyinput/macros.py`, and the one thing worth knowing
//! before reading it is what a macro *is* there:
//!
//! > **qutebrowser does not record keystrokes.** `commands/runners.py:187` calls
//! > `macros.macro_recorder.record_command(text, count)` — the resolved command string and the
//! > count that reached it — and `run_macro` replays through `CommandRunner.run_safely`, never
//! > through a key parser.
//!
//! Every consequence of that is a consequence worth having.
//!
//! - `3j` is stored **once**, as `("scroll down", 3)`. The count machinery has already run.
//! - `o example.com<Return>` is stored as `open example.com`, not as the eleven keys that typed it,
//!   because `cmd-set-text` is one of the four commands never recorded and the accepted command
//!   line is recorded in its place (`runners.py:181-182`; command mode is left *before* the text is
//!   run, `statusbar/command.py:193-198`, so the mode seen at record time is normal).
//! - A recorded `f` does not record the hint label that follows it, because recording only happens
//!   in normal mode and a label is pressed in hint mode.
//! - **`j` pays almost nothing.** With no recording in progress [`record`] is one relaxed atomic
//!   load and a return — measured, see the report and `the_key_path_cost_of_not_recording`. Had
//!   this recorded keys instead, every keystroke in the browser would have had to be copied into a
//!   growing vector behind a mutex.
//!
//! The register half — "the next keystroke names a register" — is not here. It is
//! `RegisterKeyParser` (modeparsers.py:245), which bru already had for `` ` `` and `'`, and
//! `record_macro`/`run_macro` are two more arms in `caret.rs::handle_mark_key`'s match.
//!
//! Nothing in this file touches Lua. It touches CEF only to dispatch a replayed command.

use cef::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use crate::commands::Command;
use crate::modes::Mode;
use crate::tabs::SharedState;

/// One recorded step: the command a binding resolved to, and the count that reached it.
///
/// `(text, count)` in qutebrowser. bru stores the parsed [`Command`] rather than its string for the
/// same reason `config.rs` parses bindings once at startup — a replay must not re-enter the parser
/// — and it is the same object either way: `commands::parse` is total over recorded commands,
/// because a command that did not parse never ran.
type Step = (Command, Option<u32>);

/// A ceiling on `<count>@<register>`. qutebrowser has none; `999999@q` would wedge the UI thread.
const MAX_COUNT: u32 = 1000;

#[derive(Default)]
struct Recorder {
    /// What each register holds. `macros.py`'s `_macros`.
    macros: HashMap<char, Vec<Step>>,
    /// The register being recorded into, if any. `_recording_macro`.
    recording: Option<char>,
    /// The count `macro-run` carried, stashed for the register keystroke that follows.
    /// `_macro_count`, which is a dict per window; bru has one window.
    count: u32,
    /// The register `@@` means. `_last_register`.
    last: Option<char>,
}

/// The one recorder. Locked once per keystroke *while recording* and never otherwise — see
/// [`RECORDING`].
fn recorder() -> &'static Mutex<Recorder> {
    static RECORDER: std::sync::OnceLock<Mutex<Recorder>> = std::sync::OnceLock::new();
    RECORDER.get_or_init(|| Mutex::new(Recorder::default()))
}

/// Whether a recording is in progress.
///
/// **This is the whole of what an ordinary keystroke pays.** It is a separate atomic rather than a
/// `recorder().lock()` because `j` must not take a mutex it will find empty 99.9% of the time; it
/// is only ever written under that same lock, so the two cannot disagree for longer than the
/// instruction between them, and the loser of that race is a keystroke at the exact moment `q` was
/// pressed.
static RECORDING: AtomicBool = AtomicBool::new(false);

// ------------------------------------------------------------------------------------------------
// Recording — the key path
// ------------------------------------------------------------------------------------------------

/// Record a command that is about to run, if a macro is being recorded.
///
/// Port of the tail of `runners.CommandRunner.run` (runners.py:181-188), which is two independent
/// conditions and not one:
///
/// ```python
/// if result.cmdline[0] in ['macro-record', 'macro-run', 'set-cmd-text', 'cmd-set-text']:
///     record_macro = False
/// ...
/// if record_macro and cur_mode == usertypes.KeyMode.normal:
///     macros.macro_recorder.record_command(text, count)
/// ```
///
/// **Called before the command runs, never after**, because the mode is the mode the key was
/// pressed in — `cur_mode` is read at the top of `run`, and `mode-enter caret` would otherwise look
/// like a command pressed in caret mode and go unrecorded.
pub fn record(state: &SharedState, command: &Command, count: Option<u32>) {
    if !RECORDING.load(Ordering::Relaxed) {
        return;
    }
    if !recordable(command) {
        return;
    }
    if state.lock().expect("state mutex poisoned").mode() != Mode::Normal {
        return;
    }
    let mut guard = recorder().lock().expect("macro recorder mutex poisoned");
    if let Some(register) = guard.recording {
        guard
            .macros
            .entry(register)
            .or_default()
            .push((command.clone(), count));
    }
}

/// The recursion guard, by name in qutebrowser and by variant here.
///
/// `macro-record` and `macro-run` are excluded or `q` would be the first step of its own macro and
/// `@a` inside `a` would never end. `cmd-set-text` is excluded for a different reason and it is the
/// one that makes macros useful: `o` opens the command line, and what belongs in the macro is the
/// `open …` that comes out of it, not the keystroke that opened it.
///
/// A chain is recordable only if every link is — `clear-keychain ;; cmd-set-text :open` would
/// otherwise be recorded whole and replay the half qutebrowser refuses.
fn recordable(command: &Command) -> bool {
    match command {
        Command::Chain(parts) => parts.iter().all(recordable),
        Command::MacroRecord { .. } | Command::MacroRun { .. } | Command::CmdSetText { .. } => false,
        _ => true,
    }
}

// ------------------------------------------------------------------------------------------------
// The two commands
// ------------------------------------------------------------------------------------------------

/// `macro-record [register]` — `q`. Port of `MacroRecorder.macro_record` (macros.py:41).
///
/// Recording already? Stop, and say which register it went to. Otherwise start, either into the
/// named register or into whatever the next keystroke names.
pub fn macro_record(state: &SharedState, register: Option<char>) {
    let recording = recorder()
        .lock()
        .expect("macro recorder mutex poisoned")
        .recording;

    if let Some(open) = recording {
        let steps = {
            let mut guard = recorder().lock().expect("macro recorder mutex poisoned");
            guard.recording = None;
            RECORDING.store(false, Ordering::Relaxed);
            guard.macros.get(&open).map(Vec::len).unwrap_or(0)
        };
        crate::message::info(&format!("Macro '{open}' recorded."));
        debug(&format!("recorded {steps} step(s) into '{open}'"));
        return;
    }

    match register {
        Some(register) => start(register),
        None => enter(state, Mode::RecordMacro),
    }
}

/// `macro-run [register]`, with the count — `@`. Port of `MacroRecorder.macro_run` (macros.py:66).
///
/// The count is stashed *now*, before the register keystroke, which is why `3@q` runs three times
/// even though `run_macro` mode reads no counts at all.
pub fn macro_run(
    state: &SharedState,
    browser: &mut Browser,
    register: Option<char>,
    count: Option<u32>,
) {
    recorder().lock().expect("macro recorder mutex poisoned").count =
        count.unwrap_or(1).clamp(1, MAX_COUNT);

    match register {
        Some(register) => replay(state, browser, register),
        None => enter(state, Mode::RunMacro),
    }
}

/// Begin recording into `register`, discarding whatever it held. `MacroRecorder.record_macro`.
fn start(register: char) {
    begin(register);
    crate::message::info(&format!("Recording macro '{register}'..."));
}

/// The bookkeeping half of [`start`], with nothing that needs a running browser.
///
/// Split out because `message::info` posts a CEF task to expire the message, and a posted task in a
/// unit test aborts the process — "CefTask_0_CToCpp called with invalid version -1", measured
/// 2026-08-06 when every macro test that started a recording brought the whole binary down with
/// SIGTRAP rather than failing. The tests drive this; the browser drives `start`.
fn begin(register: char) {
    {
        let mut guard = recorder().lock().expect("macro recorder mutex poisoned");
        guard.macros.insert(register, Vec::new());
        guard.recording = Some(register);
    }
    // Written after the map, so no keystroke can arrive at `record` before there is somewhere to
    // put it.
    RECORDING.store(true, Ordering::Relaxed);
}

/// Run what `register` holds, `count` times. `MacroRecorder.run_macro` (macros.py:81).
fn replay(state: &SharedState, browser: &mut Browser, register: char) {
    let (steps, times) = {
        let mut guard = recorder().lock().expect("macro recorder mutex poisoned");

        // `@@` is the last macro run — including the last one `@@` itself resolved to.
        let register = if register == '@' {
            match guard.last {
                Some(last) => last,
                None => {
                    drop(guard);
                    crate::message::error("No previous macro");
                    return;
                }
            }
        } else {
            register
        };
        guard.last = Some(register);

        let times = guard.count.max(1);
        match guard.macros.get(&register) {
            Some(steps) => (steps.clone(), times),
            None => {
                drop(guard);
                crate::message::error(&format!("No macro recorded in '{register}'!"));
                return;
            }
        }
    };

    debug(&format!("replaying {} step(s) x{times}", steps.len()));
    for _ in 0..times {
        for (command, count) in &steps {
            // qutebrowser replays through `CommandRunner.run_safely`, which is the same `run` that
            // records — so a macro run while another is being recorded is copied into it. Faithful,
            // and the reason `macro-run` itself is excluded is exactly that this is not a loop.
            record(state, command, *count);
            crate::exec::run(state, browser, command, *count);
        }
    }
}

/// Enter one of the two register modes, and tell the chrome. Same shape as
/// `caret::leave_register_mode` in reverse.
fn enter(state: &SharedState, mode: Mode) {
    let entered = state
        .lock()
        .expect("state mutex poisoned")
        .enter_mode(mode, false);
    if entered {
        crate::ipc::set_mode(mode.name().to_string());
    }
}

// ------------------------------------------------------------------------------------------------
// The register keystroke — reached from caret.rs::handle_mark_key
// ------------------------------------------------------------------------------------------------

/// `record_macro` mode's register key: start recording into it.
///
/// `modeparsers.py:294`. The mode has already been left by the time this is called, exactly as
/// `set_mark`'s arm is.
pub fn name_recording(key: char) {
    start(key);
}

/// `run_macro` mode's register key: replay it, `count` times, where the count was stashed by
/// `macro-run` before the mode opened.
///
/// `modeparsers.py:296`.
pub fn run_named(state: &SharedState, browser: &mut Browser, key: char) {
    replay(state, browser, key);
}

fn debug(what: &str) {
    if std::env::var_os("BRU_DEBUG_MACROS").is_some() {
        eprintln!("bru[macros]: {what}");
    }
}

// ------------------------------------------------------------------------------------------------
// The debug switch
// ------------------------------------------------------------------------------------------------

/// `--macro-script=qa,3j,q,pos,@a,pos --macro-step-ms=900` drives macros from posted UI tasks: each
/// step is a key sequence fed to the real key parser for whatever mode bru is in, so `q` goes
/// through the `macro-record` binding, `a` through the register parser, and `3j` through the count
/// machinery and `send_mouse_wheel_event`.
///
/// It exists for the same reason as `--caret-script` and `--hint-script`. The only key-injection
/// tool on this machine is `wtype`, which attaches a virtual keyboard, and CEF segfaults in
/// `xkb_state_update_mask` when the keymap arrives. Inert unless the switch is passed.
///
/// Three steps press nothing. `pos` asks the page where it is scrolled to — which is the only way
/// to see that a replayed `3j` actually moved the page — `report` prints the recorder, and a step
/// beginning with `:` goes through [`crate::exec::run_from_cmdline`], the function the command line
/// itself posts to. That last one is not decoration: whether `o example.com<Return>` records as
/// `open example.com` rather than as the keystroke that opened the line is a question about *that*
/// path, and nothing else in bru can drive it without a keyboard.
pub fn schedule_macro_script(steps: &str, interval_ms: i64) {
    for (i, step) in steps.split(',').filter(|s| !s.is_empty()).enumerate() {
        let mut task = MacroStep::new(step.to_string());
        post_delayed_task(ThreadId::UI, Some(&mut task), interval_ms * (i as i64 + 1));
    }
}

wrap_task! {
    struct MacroStep {
        step: String,
    }

    impl Task {
        fn execute(&self) {
            let Some(state) = crate::state::BruState::instance() else {
                return;
            };
            let browser = state.lock().expect("state mutex poisoned").active_browser();
            let Some(mut browser) = browser else {
                eprintln!("macro-script: no tab to aim at");
                return;
            };

            let keys = match self.step.as_str() {
                "pos" | "report" => String::new(),
                "esc" => "<Escape>".to_string(),
                // What the command line does with an accepted `:` line, posted the same way.
                text if text.starts_with(':') => {
                    crate::exec::run_from_cmdline(&text[1..], None);
                    String::new()
                }
                other => other.to_string(),
            };

            if !keys.is_empty() {
                match crate::bindings::parse_key_sequence(&keys) {
                    Ok(sequence) => {
                        for info in sequence {
                            press(&state, &mut browser, info);
                        }
                    }
                    Err(e) => eprintln!("macro-script: {keys:?}: {e}"),
                }
            }

            // The position that was last reported, then a fresh request for the next step: the
            // answer comes back over the message router and is never ready on this task.
            let where_ = match crate::scroll::position() {
                Some(p) => format!("y={:.0}/{:.0}", p.y, p.max_y),
                None => "y=?".to_string(),
            };
            crate::scroll::request_position(&mut browser);

            let mode = state.lock().expect("state mutex poisoned").mode();
            let guard = recorder().lock().expect("macro recorder mutex poisoned");
            let mut registers: Vec<String> = guard
                .macros
                .iter()
                .map(|(register, steps)| format!("{register}={}", steps.len()))
                .collect();
            registers.sort();
            eprintln!(
                "macro-script: after {:<6} -> mode {mode}, {where_}, recording {}, last {}, registers [{}]",
                self.step,
                guard.recording.map(String::from).unwrap_or_else(|| "-".into()),
                guard.last.map(String::from).unwrap_or_else(|| "-".into()),
                registers.join(" "),
            );
        }
    }
}

/// One key, down the same path `src/keys.rs` takes: the register parser first, then the mode's own
/// trie, then the recording hook, then the dispatcher.
///
/// It is deliberately a copy of `caret::press` with the one line this workstream adds, so that what
/// the script drives and what a keyboard drives cannot drift apart in the half that matters.
fn press(state: &SharedState, browser: &mut Browser, info: crate::bindings::KeyInfo) {
    if crate::caret::handle_mark_key(state, browser, info).is_some() {
        return;
    }
    let Some(outcome) = state.lock().expect("state mutex poisoned").handle_key(info) else {
        return;
    };
    crate::ipc::set_keystring(outcome.keystring.clone());
    if let crate::bindings::KeyAction::Run { command, count } = outcome.action {
        record(state, &command, count);
        crate::exec::run(state, browser, &command, count);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{self, ScrollDirection};

    /// Every test in this file shares one process-wide recorder, so they share one lock too.
    /// Without it `cargo test`'s threads interleave two recordings into one register.
    fn exclusively() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn reset() {
        let mut guard = recorder().lock().expect("macro recorder mutex poisoned");
        *guard = Recorder::default();
        RECORDING.store(false, Ordering::Relaxed);
    }

    /// What a register holds, as command strings — the shape qutebrowser stores.
    fn stored(register: char) -> Vec<(Command, Option<u32>)> {
        recorder()
            .lock()
            .expect("macro recorder mutex poisoned")
            .macros
            .get(&register)
            .cloned()
            .unwrap_or_default()
    }

    /// The half of `record` that does not need a `BruState`: everything but the mode check.
    fn record_here(command: &Command, count: Option<u32>) {
        if !RECORDING.load(Ordering::Relaxed) || !recordable(command) {
            return;
        }
        let mut guard = recorder().lock().expect("macro recorder mutex poisoned");
        if let Some(register) = guard.recording {
            guard.macros.entry(register).or_default().push((command.clone(), count));
        }
    }

    #[test]
    fn a_recording_keeps_commands_and_their_counts_not_keys() {
        let _lock = exclusively();
        reset();

        begin('a');
        record_here(&commands::parse("scroll down").unwrap(), Some(3));
        record_here(&commands::parse("tab-next").unwrap(), None);

        // `3j` is one step carrying a count of three, not three steps — the count machinery ran
        // at record time, which is the whole point of recording commands (runners.py:187).
        assert_eq!(
            stored('a'),
            vec![
                (Command::Scroll(ScrollDirection::Down), Some(3)),
                (Command::TabNext, None),
            ]
        );
        assert_eq!(stored('a').len(), 2);
    }

    #[test]
    fn the_four_commands_that_are_never_recorded() {
        // runners.py:181. Two of them would recurse; `cmd-set-text` is what makes `o example.com`
        // record as `open example.com` rather than as the key that opened the command line.
        for text in ["macro-record", "macro-run", "macro-record a", "cmd-set-text -s :open"] {
            let cmd = commands::parse(text).expect("a real command");
            assert!(!recordable(&cmd), "{text:?} must never be recorded");
        }
        // And the chain rule: one unrecordable link poisons the whole chain.
        assert!(!recordable(&commands::parse("clear-keychain ;; cmd-set-text :open").unwrap()));

        // Everything else is fair game, including the command line's *result*.
        for text in ["scroll down", "open example.com", "tab-close", "mode-enter insert"] {
            let cmd = commands::parse(text).expect("a real command");
            assert!(recordable(&cmd), "{text:?} should be recorded");
        }
    }

    #[test]
    fn nothing_is_recorded_until_q_and_nothing_after_the_second_q() {
        let _lock = exclusively();
        reset();

        // Before `q`: the atomic is false and `record` returns without touching the map.
        record_here(&Command::TabNext, None);
        assert!(stored('a').is_empty());

        begin('a');
        record_here(&Command::TabNext, None);
        assert_eq!(stored('a').len(), 1);

        // The second `q`. `macro_record` needs no state to stop, only to start.
        {
            let mut guard = recorder().lock().expect("macro recorder mutex poisoned");
            guard.recording = None;
            RECORDING.store(false, Ordering::Relaxed);
        }
        record_here(&Command::TabPrev, None);
        assert_eq!(stored('a').len(), 1, "a stopped recording records nothing");
    }

    #[test]
    fn recording_into_a_register_twice_replaces_it() {
        let _lock = exclusively();
        reset();

        begin('a');
        record_here(&Command::TabNext, None);
        assert_eq!(stored('a').len(), 1);

        // macros.py:62 — `self._macros[register] = []`. A second `qa` starts from empty, it does
        // not append to what was there.
        begin('a');
        assert!(stored('a').is_empty());
        record_here(&Command::TabPrev, None);
        assert_eq!(stored('a'), vec![(Command::TabPrev, None)]);
    }

    #[test]
    fn at_at_means_the_last_register_and_says_so_when_there_is_none() {
        let _lock = exclusively();
        reset();

        // `@@` with nothing run before is an error, and `_last_register` stays unset.
        assert_eq!(
            recorder().lock().expect("macro recorder mutex poisoned").last,
            None
        );

        // Running `a` sets it; `@@` then resolves to `a`. Both halves are `replay`'s bookkeeping,
        // which is all that can be checked without a Browser to dispatch against.
        begin('a');
        record_here(&Command::TabNext, None);
        {
            let mut guard = recorder().lock().expect("macro recorder mutex poisoned");
            guard.recording = None;
            RECORDING.store(false, Ordering::Relaxed);
            guard.last = Some('a');
        }
        let resolved = {
            let guard = recorder().lock().expect("macro recorder mutex poisoned");
            guard.last
        };
        assert_eq!(resolved, Some('a'));
        assert_eq!(stored('a').len(), 1);
    }

    #[test]
    fn a_count_on_at_is_stashed_before_the_register_key_arrives() {
        let _lock = exclusively();
        reset();

        // This is why `run_macro` mode can have `supports_count = false` and `3@q` still run three
        // times: `macro-run` stores the count while the `3` is still normal mode's (macros.py:79).
        recorder().lock().expect("macro recorder mutex poisoned").count =
            Some(3u32).unwrap_or(1).clamp(1, MAX_COUNT);
        assert_eq!(recorder().lock().expect("macro recorder mutex poisoned").count, 3);

        // And the ceiling is real: a typo cannot wedge the UI thread.
        recorder().lock().expect("macro recorder mutex poisoned").count =
            Some(999_999u32).unwrap_or(1).clamp(1, MAX_COUNT);
        assert_eq!(
            recorder().lock().expect("macro recorder mutex poisoned").count,
            MAX_COUNT
        );
    }

    /// **What recording costs a key that is not being recorded**, which is the question this design
    /// was chosen to answer, and what a key that *is* being recorded costs beside it. Both printed
    /// rather than only asserted; the report quotes the numbers.
    ///
    /// The two are what the choice between recording commands and recording keys comes down to.
    /// The first is what `j` pays for macros existing at all; the second is what it would have paid
    /// on **every** keystroke had a macro been a list of keys, because then there is no state to
    /// check before copying — every key is the payload.
    ///
    /// The assertion is deliberately loose: this runs under `cargo test`, unoptimised, on a machine
    /// with five other agents on it. It is still not nothing — the recording path below is an order
    /// of magnitude slower, and that gap is the whole argument.
    #[test]
    fn the_key_path_cost_of_not_recording() {
        let _lock = exclusively();
        reset();

        let command = commands::parse("scroll down").unwrap();
        const ROUNDS: u32 = 1_000_000;

        // Warm the branch predictor and the cache line the way a browser would.
        for _ in 0..10_000 {
            std::hint::black_box(RECORDING.load(Ordering::Relaxed));
        }

        // Not recording: the load is the whole of it. `record` takes the state lock only after
        // this, for the mode check, and never reaches it.
        let start = std::time::Instant::now();
        for _ in 0..ROUNDS {
            if std::hint::black_box(RECORDING.load(Ordering::Relaxed)) {
                std::hint::black_box(recordable(std::hint::black_box(&command)));
            }
        }
        let idle = start.elapsed().as_nanos() as f64 / ROUNDS as f64;

        // Recording: the guard, the mutex, the clone and the push. `record_here` is `record`
        // without the state lock, which no unit test can hold.
        begin('z');
        let start = std::time::Instant::now();
        for _ in 0..ROUNDS {
            record_here(std::hint::black_box(&command), Some(3));
        }
        let busy = start.elapsed().as_nanos() as f64 / ROUNDS as f64;
        assert_eq!(stored('z').len(), ROUNDS as usize);
        reset();

        println!(
            "macros: a keystroke costs {idle:.3} ns with no recording in progress, \
             {busy:.1} ns while one is — {:.0}x",
            busy / idle.max(f64::MIN_POSITIVE)
        );
        assert!(idle < 20.0, "the key path must stay free: {idle:.3} ns");
        assert!(
            busy > idle * 3.0,
            "if these are the same, the fast path is not being taken"
        );
    }
}
