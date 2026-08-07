//! Putting text into the page: `edit-text`, `insert-text` and `fake-key`.
//!
//! All three end at the same place — the field the page has focused — and two of them need to know
//! what is in it before they can change it, so the small browser↔renderer question channel they
//! share lives here too.
//!
//! ## Asking the page a question
//!
//! CEF gives the browser process no way to read the DOM: `execute_java_script` returns nothing, and
//! `cefQuery` is refused for every frame that is not `bru://` (`ipc.rs`, and that check is the only
//! thing standing between a web page and bru's command set). The way through is the one
//! `scroll.rs` already uses for the scroll position: a `ProcessMessage` to the renderer, evaluated
//! there between `V8Context::enter` and `exit`, answered with another `ProcessMessage`. A page
//! cannot send one — only the browser process can address the renderer — so this channel adds
//! nothing a page can reach.
//!
//! ## `fake-key` and bru's own key handler
//!
//! `host.send_key_event` is the real input path, which is exactly why CEF-NOTES recommends it and
//! exactly what makes `fake-key` awkward: the event arrives at bru's own `on_pre_key_event` before
//! the page sees it, so a faked `<Escape>` would be read as the `mode-leave` binding instead of
//! being delivered. qutebrowser has the opposite problem and solves it by bypassing its mode
//! manager (`tab.send_event`). Here the bypass is [`is_faking`], which `keys.rs` checks first and
//! which is true only for the few microseconds an injection is in flight.

use cef::*;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

// -----------------------------------------------------------------------------------------------
// The question channel
// -----------------------------------------------------------------------------------------------

/// Browser → renderer: "evaluate this and tell me what it came to."
const ASK: &str = "bru-ask";
/// Renderer → browser: the answer, tagged with the id it was asked under.
const ANSWER: &str = "bru-answer";

type Answered = Box<dyn FnOnce(String) + Send>;

/// Questions asked and not yet answered. Small and short-lived: one entry per `edit-text`, plus
/// whatever `--ask-script` is driving.
fn pending() -> &'static Mutex<Vec<(u64, Answered)>> {
    static PENDING: Mutex<Vec<(u64, Answered)>> = Mutex::new(Vec::new());
    &PENDING
}

/// Evaluate `script` in the page's main frame and hand the result to `then`, **on the UI thread**.
///
/// The result is whatever the script's last expression is, converted to a string by V8. An answer
/// that never arrives — a renderer that died mid-question — leaves one closure in the map and
/// nothing else; there is no timeout, because the alternative is a callback that fires twice.
pub fn ask(browser: &mut Browser, script: &str, then: impl FnOnce(String) + Send + 'static) {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    let Some(frame) = browser.main_frame() else {
        return;
    };
    let id = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut pending) = pending().lock() {
        pending.push((id, Box::new(then)));
    }

    let Some(mut message) = process_message_create(Some(&CefString::from(ASK))) else {
        return;
    };
    if let Some(arguments) = message.argument_list() {
        arguments.set_string(0, Some(&CefString::from(id.to_string().as_str())));
        arguments.set_string(1, Some(&CefString::from(script)));
    }
    frame.send_process_message(ProcessId::RENDERER, Some(&mut message));
}

/// Browser side of the answer. Called from `ipc::on_process_message_received`; returns true when
/// the message was ours.
pub fn on_answer(message: Option<&ProcessMessage>) -> bool {
    let Some(message) = message else {
        return false;
    };
    if CefString::from(&message.name()).to_string() != ANSWER {
        return false;
    }
    let Some(arguments) = message.argument_list() else {
        return true;
    };
    let id: u64 = match CefString::from(&arguments.string(0)).to_string().parse() {
        Ok(id) => id,
        Err(_) => return true,
    };
    let text = CefString::from(&arguments.string(1)).to_string();

    let then = pending().lock().ok().and_then(|mut pending| {
        pending
            .iter()
            .position(|(waiting, _)| *waiting == id)
            .map(|at| pending.remove(at).1)
    });
    if let Some(then) = then {
        then(text);
    }
    true
}

/// Renderer side. Called from `ipc::renderer_on_process_message_received`; returns true when the
/// message was ours. **Nothing here may touch `BruState`** — different process, empty struct.
pub fn renderer_on_ask(frame: Option<&Frame>, message: Option<&ProcessMessage>) -> bool {
    let Some(message) = message else {
        return false;
    };
    if CefString::from(&message.name()).to_string() != ASK {
        return false;
    }
    let (Some(frame), Some(arguments)) = (frame, message.argument_list()) else {
        return true;
    };
    let id = CefString::from(&arguments.string(0)).to_string();
    let script = CefString::from(&arguments.string(1)).to_string();
    let text = evaluate(frame, &script).unwrap_or_default();

    let Some(mut reply) = process_message_create(Some(&CefString::from(ANSWER))) else {
        return true;
    };
    if let Some(arguments) = reply.argument_list() {
        arguments.set_string(0, Some(&CefString::from(id.as_str())));
        arguments.set_string(1, Some(&CefString::from(text.as_str())));
    }
    frame.send_process_message(ProcessId::BROWSER, Some(&mut reply));
    true
}

/// Run an expression in the frame's own V8 context and return it as a string.
///
/// `eval` only works between `enter` and `exit` — outside that scope there is no context for the
/// script to belong to and CEF refuses. Same shape as `scroll.rs::evaluate`, which is where the
/// pattern was measured.
fn evaluate(frame: &Frame, code: &str) -> Option<String> {
    let context = frame.v8_context()?;
    if context.enter() == 0 {
        return None;
    }
    let mut value: Option<V8Value> = None;
    let mut exception: Option<V8Exception> = None;
    let ok = context.eval(
        Some(&CefString::from(code)),
        None,
        0,
        Some(&mut value),
        Some(&mut exception),
    );
    let text = (ok != 0)
        .then_some(value)
        .flatten()
        .map(|value| CefString::from(&value.string_value()).to_string());
    context.exit();
    text
}

/// Run a script in a frame from the UI thread, with no answer wanted.
fn post_script(frame: Frame, script: String) {
    let mut task = RunScript::new(frame, script);
    post_task(ThreadId::UI, Some(&mut task));
}

wrap_task! {
    struct RunScript {
        frame: Frame,
        script: String,
    }

    impl Task {
        fn execute(&self) {
            self.frame.execute_java_script(
                Some(&CefString::from(self.script.as_str())),
                None,
                0,
            );
        }
    }
}

// -----------------------------------------------------------------------------------------------
// edit-text / open-editor
// -----------------------------------------------------------------------------------------------

/// What the focused field holds, as `<caret>\n<value>`, or the empty string when nothing editable
/// is focused. The caret is on its own first line because the value may contain newlines and the
/// caret may not.
const READ_FOCUSED: &str = r#"(function () {
    var e = document.activeElement;
    if (!e) { return ""; }
    var tag = e.tagName ? e.tagName.toLowerCase() : "";
    var value = null;
    if (tag === "textarea") { value = e.value; }
    else if (tag === "input" && !/^(button|submit|reset|checkbox|radio|file|image|range|color)$/i.test(e.type || "text")) { value = e.value; }
    else if (e.isContentEditable) { value = e.innerText; }
    if (value === null) { return ""; }
    var caret = (typeof e.selectionStart === "number") ? e.selectionStart : value.length;
    return caret + "\n" + value;
})();"#;

/// `edit-text` (`<Ctrl-E>` in insert mode), and `open-editor`, which is what the same command was
/// called before qutebrowser 1.0 and what this user's fingers may still spell.
///
/// Three hops, none of them on the UI thread for longer than it takes to post: read the field,
/// run `$EDITOR` on a thread, write the result back from a posted task. The editor is a real
/// program that will sit there for minutes; blocking the UI thread on it would freeze the window.
pub fn edit_text(browser: &mut Browser) {
    let Some(frame) = browser.main_frame() else {
        return;
    };
    ask(browser, READ_FOCUSED, move |answer| {
        if answer.is_empty() {
            crate::spawn::message("edit-text: no editable field is focused");
            return;
        }
        let (caret, text) = match answer.split_once('\n') {
            Some((caret, text)) => (caret.parse::<usize>().unwrap_or(0), text.to_string()),
            None => (0, String::new()),
        };
        std::thread::spawn(move || match run_editor(&text, caret) {
            Ok(edited) => {
                if edited == text {
                    return;
                }
                post_script(frame, write_back_script(&edited));
            }
            Err(error) => crate::spawn::message(&format!("edit-text: {error}")),
        });
    });
}

// --- config commands -----------------------------------------------------------------------
/// Run `$EDITOR` on a file that already exists on disk, and say when it is done.
///
/// [`edit_text`]'s sibling, and deliberately not built on it: `edit_text` owns a temporary file it
/// writes, reads back and deletes, and the one caller of this one — `:config-edit` — is editing a
/// file that is **not bru's**. So nothing here writes, creates or deletes the path; the editor is
/// pointed at it and the callback is told whether the editor exited cleanly.
///
/// The thread is the same shape and for the same reason: a real editor sits there for minutes, and
/// blocking the UI thread on `wait` freezes the window. The callback is handed back **on the UI
/// thread** — it re-reads the config and installs it over the running browser, which touches
/// `BruState` and Chromium and may not happen on a random thread.
pub fn edit_file(path: std::path::PathBuf, then: impl FnOnce(bool) + Send + 'static) {
    std::thread::spawn(move || {
        let argv = match editor_argv(&editor_spec(), &path.display().to_string(), 1, 1) {
            Ok(argv) => argv,
            Err(error) => {
                crate::spawn::error(&format!("config-edit: {error}"));
                return;
            }
        };
        let status = std::process::Command::new(&argv[0]).args(&argv[1..]).status();
        let ok = match status {
            Ok(status) if status.success() => true,
            Ok(status) => {
                crate::spawn::error(&format!("config-edit: the editor exited with {status}"));
                false
            }
            Err(error) => {
                crate::spawn::error(&format!("config-edit: could not run {:?}: {error}", argv[0]));
                false
            }
        };
        let mut task = AfterEditor::new(std::sync::Arc::new(Mutex::new(Some(
            Box::new(move || then(ok)) as EditorDone,
        ))));
        post_task(ThreadId::UI, Some(&mut task));
    });
}

type EditorDone = Box<dyn FnOnce() + Send>;

// `wrap_task!` takes no doc comment on the struct it declares — CEF-NOTES trap 8, and its fields
// have to be `Clone`, which is why the closure is behind an `Arc` rather than a bare `Mutex`.
wrap_task! {
    struct AfterEditor {
        done: std::sync::Arc<Mutex<Option<EditorDone>>>,
    }

    impl Task {
        fn execute(&self) {
            let done = self.done.lock().ok().and_then(|mut done| done.take());
            if let Some(done) = done {
                done();
            }
        }
    }
}
// --- end config commands ---------------------------------------------------------------------

/// Put `text` into the focused field and tell the page's own handlers about it.
///
/// The `input` event is what makes a React or Vue field notice — qutebrowser dispatches the same
/// one for the same reason (`on_file_updated`, "kick off js handlers to trick them into thinking
/// there was input").
fn write_back_script(text: &str) -> String {
    format!(
        r#"(function () {{
    var e = document.activeElement;
    if (!e) {{ return; }}
    var value = "{}";
    if ("value" in e) {{ e.value = value; }}
    else if (e.isContentEditable) {{ e.innerText = value; }}
    else {{ return; }}
    e.dispatchEvent(new Event("input", {{ bubbles: true }}));
    e.dispatchEvent(new Event("change", {{ bubbles: true }}));
}})();"#,
        crate::ipc::json_escape(text)
    )
}

/// Write `text` to a temporary file, run the editor over it, and read it back.
///
/// Runs on a thread of its own. **If the editor never exits this thread parks in `wait` forever**
/// and bru is otherwise untouched — no lock is held across the call and the UI thread never sees
/// it.
fn run_editor(text: &str, caret: usize) -> Result<String, String> {
    let path = crate::spawn::temp_path("edit", "txt")?;
    std::fs::write(&path, text)
        .map_err(|error| format!("could not write {}: {error}", path.display()))?;

    let (line, column) = line_and_column(text, caret);
    let argv = editor_argv(&editor_spec(), &path.display().to_string(), line, column)?;

    let status = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .status()
        .map_err(|error| format!("could not run {:?}: {error}", argv[0]))?;

    if !status.success() {
        // qutebrowser keeps the file when the editor exits abnormally rather than throwing the
        // text away, and says where it left it. Same here.
        return Err(format!(
            "the editor exited with {status}; the text is still in {}",
            path.display()
        ));
    }

    let edited = std::fs::read_to_string(&path)
        .map_err(|error| format!("could not read {} back: {error}", path.display()))?;
    let _ = std::fs::remove_file(&path);
    Ok(edited)
}

/// The editor to run, with qutebrowser's placeholders filled in.
///
/// `editor.command` is a config option in qutebrowser and this user sets it to `["kitty", "-e",
/// "nvim", "{file}"]` — a terminal, because a terminal editor started from a GUI browser with no
/// terminal does nothing visible. bru has no such setting yet and this module deliberately did not
/// add one to `config.rs`, which no workstream owns this round; until it does, the command comes
/// from the environment, most specific first:
///
/// | | |
/// |---|---|
/// | `$BRU_EDITOR` | a whole command line, with `{file}` / `{line}` / `{column}` |
/// | `$VISUAL` | the Unix "editor that wants a terminal of its own" variable |
/// | `$EDITOR` | |
/// | `nvim` | this machine's editor, and qutebrowser's own default |
///
/// A command with no placeholder gets the filename appended, which is what `$EDITOR file` means
/// everywhere else.
fn editor_spec() -> String {
    ["BRU_EDITOR", "VISUAL", "EDITOR"]
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|value| !value.trim().is_empty()))
        .unwrap_or_else(|| "nvim".to_string())
}

fn editor_argv(spec: &str, file: &str, line: usize, column: usize) -> Result<Vec<String>, String> {
    let argv = crate::spawn::shlex(spec)?;
    if argv.is_empty() {
        return Err("the editor command is empty".to_string());
    }

    let substituted: Vec<String> = argv
        .iter()
        .map(|arg| {
            arg.replace("{file}", file)
                .replace("{line0}", &(line.saturating_sub(1)).to_string())
                .replace("{column0}", &(column.saturating_sub(1)).to_string())
                .replace("{line}", &line.to_string())
                .replace("{column}", &column.to_string())
                .replace("{}", file)
        })
        .collect();

    if substituted == argv {
        // No placeholder anywhere: `$EDITOR` is a program, not a command line.
        let mut argv = substituted;
        argv.push(file.to_string());
        return Ok(argv);
    }
    Ok(substituted)
}

/// 1-based line and column of `caret` in `text`, as every editor counts them.
///
/// `misc/editor.py::_calc_line_and_column`, whose docstring lists the editors it matches.
fn line_and_column(text: &str, caret: usize) -> (usize, usize) {
    let caret = caret.min(text.chars().count());
    let before: String = text.chars().take(caret).collect();
    let line = before.matches('\n').count() + 1;
    let column = match before.rfind('\n') {
        Some(at) => before[at + 1..].chars().count() + 1,
        None => before.chars().count() + 1,
    };
    (line, column)
}

// -----------------------------------------------------------------------------------------------
// insert-text
// -----------------------------------------------------------------------------------------------

/// `insert-text <text>` — `<Shift-Ins>` in insert mode, which is bound to `insert-text -- {primary}`.
///
/// `document.execCommand("insertText")` rather than assigning `value`, because it inserts *at the
/// caret* and leaves an undo entry, which is what `components/misccommands.py::insert_text` reaches
/// through `javascript/webelem.js`. Deprecated in the standard and still the only thing that does
/// this in Chromium.
pub fn insert_text(browser: &mut Browser, text: &str) {
    let text = crate::spawn::replace_variables(text);
    let Some(frame) = browser.main_frame() else {
        return;
    };
    let script = format!(
        r#"(function () {{
    var e = document.activeElement;
    if (!e) {{ return; }}
    e.focus();
    document.execCommand("insertText", false, "{}");
}})();"#,
        crate::ipc::json_escape(&text)
    );
    frame.execute_java_script(Some(&CefString::from(script.as_str())), None, 0);
}

// -----------------------------------------------------------------------------------------------
// fake-key
// -----------------------------------------------------------------------------------------------

/// True while a `fake-key` injection is in flight. `keys.rs` returns immediately when it is, so the
/// key reaches the page instead of bru's bindings — which is the whole meaning of the command.
static FAKING: AtomicBool = AtomicBool::new(false);

/// Checked at the top of `on_pre_key_event`. See the module docs.
pub fn is_faking() -> bool {
    let faking = FAKING.load(Ordering::SeqCst);
    if faking && std::env::var_os("BRU_DEBUG_KEYS").is_some() {
        eprintln!("bru[editor]: passing a faked key through to the page");
    }
    faking
}

/// `fake-key <keystring>` — `<Shift-Escape>` in insert mode is `fake-key <Escape>`.
///
/// The keystring is qutebrowser's own spelling and is parsed by bru's own parser, so `xy`,
/// `<Ctrl-x>` and `<Escape>` all work and mean what they do in a binding.
pub fn fake_key(browser: &mut Browser, keystring: &str) {
    let sequence = match crate::bindings::parse_key_sequence(keystring) {
        Ok(sequence) => sequence,
        Err(error) => {
            crate::spawn::message(&format!("fake-key: {error}"));
            return;
        }
    };
    let Some(host) = browser.host() else {
        return;
    };

    // Set before the first event and cleared from a posted task rather than straight after the
    // loop: whether CEF delivers an injected key synchronously inside `send_key_event` or posts it
    // is not something this code should depend on, and a task posted now runs after anything CEF
    // queued during the loop. The window in which a *real* key would also be passed through is one
    // turn of the UI loop.
    FAKING.store(true, Ordering::SeqCst);
    for info in sequence {
        send_key(&host, info);
    }
    let mut task = StopFaking::new();
    post_task(ThreadId::UI, Some(&mut task));
}

wrap_task! {
    struct StopFaking;

    impl Task {
        fn execute(&self) {
            FAKING.store(false, Ordering::SeqCst);
        }
    }
}

/// One key, as the three events a real press delivers.
///
/// KEYDOWN, CHAR, KEYUP and **no explicit RAWKEYDOWN**: CEF synthesises that one from the KEYDOWN,
/// and sending both delivers two — measured for CEF-NOTES, where one injected `<Ctrl-T>` opened two
/// tabs. Without the CHAR the key arrives and types nothing.
fn send_key(host: &BrowserHost, info: crate::bindings::KeyInfo) {
    use crate::bindings::{Key, Modifiers};

    let (code, character) = match info.key {
        // `Key::Char` is canonical — letters are stored uppercase whether or not Shift was held —
        // so the character a real keyboard would have produced comes back from the modifier.
        Key::Char(c) => {
            let typed = if info.mods.contains(Modifiers::SHIFT) {
                c
            } else {
                c.to_ascii_lowercase()
            };
            (c.to_ascii_uppercase() as i32, typed as u16)
        }
        Key::Named(named) => match vkey_for_named(named) {
            Some(code) => (code, character_for_named(named)),
            None => {
                crate::spawn::message(&format!("fake-key: cannot send {named:?}"));
                return;
            }
        },
    };

    // `cef_event_flags_t`, sys bindings x86_64_unknown_linux_gnu.rs:2699–2724.
    let mut modifiers = 0u32;
    for (mods, flag) in [
        (Modifiers::SHIFT, 2),
        (Modifiers::CTRL, 4),
        (Modifiers::ALT, 8),
        (Modifiers::META, 128),
    ] {
        if info.mods.contains(mods) {
            modifiers |= flag;
        }
    }

    for type_ in [KeyEventType::KEYDOWN, KeyEventType::CHAR, KeyEventType::KEYUP] {
        let event = KeyEvent {
            type_,
            modifiers,
            windows_key_code: code,
            native_key_code: 0,
            character,
            unmodified_character: character,
            ..Default::default()
        };
        host.send_key_event(Some(&event));
    }
}

/// The inverse of `bindings::named_key_for_vkey`, which is private to the key work.
fn vkey_for_named(named: crate::bindings::NamedKey) -> Option<i32> {
    use crate::bindings::NamedKey;
    let vkey = match named {
        NamedKey::Backspace => 0x08,
        NamedKey::Tab => 0x09,
        NamedKey::Return | NamedKey::Enter => 0x0D,
        NamedKey::Escape => 0x1B,
        NamedKey::Space => 0x20,
        NamedKey::PgUp => 0x21,
        NamedKey::PgDown => 0x22,
        NamedKey::End => 0x23,
        NamedKey::Home => 0x24,
        NamedKey::Left => 0x25,
        NamedKey::Up => 0x26,
        NamedKey::Right => 0x27,
        NamedKey::Down => 0x28,
        NamedKey::Insert => 0x2D,
        NamedKey::Delete => 0x2E,
        NamedKey::Menu => 0x5D,
        NamedKey::BrowserBack => 0xA6,
        NamedKey::BrowserForward => 0xA7,
        NamedKey::F(n) if (1..=24).contains(&n) => 0x6F + n as i32,
        NamedKey::F(_) => return None,
    };
    Some(vkey)
}

/// The character a named key types, for the CHAR event. Most of them type nothing.
fn character_for_named(named: crate::bindings::NamedKey) -> u16 {
    use crate::bindings::NamedKey;
    match named {
        NamedKey::Space => b' ' as u16,
        NamedKey::Tab => b'\t' as u16,
        NamedKey::Return | NamedKey::Enter => b'\r' as u16,
        NamedKey::Backspace => 0x08,
        NamedKey::Escape => 0x1B,
        _ => 0,
    }
}

// -----------------------------------------------------------------------------------------------
// Driving it without a keyboard
// -----------------------------------------------------------------------------------------------

/// `--ask-script='<js>|<js>' --ask-step-ms=N` evaluates each snippet in the showing tab and prints
/// what it came to.
///
/// It is how this milestone is measured: `edit-text` writes into a field that only the page can
/// see, and without a way to read that field back the claim "the editor's text arrived" rests on a
/// screenshot of a text box. `wtype` is forbidden here (it segfaults CEF), so this is a posted task
/// against the same channel `edit-text` uses. Inert unless the switch is passed.
pub fn schedule_ask_script(steps: &str, interval_ms: i64) {
    for (i, step) in steps.split('|').filter(|s| !s.is_empty()).enumerate() {
        let mut task = AskStep::new(step.to_string());
        post_delayed_task(ThreadId::UI, Some(&mut task), interval_ms * (i as i64 + 1));
    }
}

wrap_task! {
    struct AskStep {
        script: String,
    }

    impl Task {
        fn execute(&self) {
            debug_assert_ne!(currently_on(ThreadId::UI), 0);

            let browser = crate::state::BruState::instance()
                .and_then(|state| state.lock().ok().and_then(|mut state| state.active_browser()));
            let Some(mut browser) = browser else {
                eprintln!("ask-script: no tab to ask");
                return;
            };
            let script = self.script.clone();
            ask(&mut browser, &self.script, move |answer| {
                eprintln!("ask-script: {script} -> {answer:?}");
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_and_column_are_one_based() {
        // The example in qutebrowser's own docstring: "aaa\nbb|bbb" is line 2, column 3.
        assert_eq!(line_and_column("aaa\nbbbbb", 6), (2, 3));
        assert_eq!(line_and_column("abc", 0), (1, 1));
        assert_eq!(line_and_column("abc", 3), (1, 4));
        assert_eq!(line_and_column("a\nb\nc", 4), (3, 1));
        // A caret past the end is what a page reports for a field bru has since changed.
        assert_eq!(line_and_column("abc", 99), (1, 4));
    }

    #[test]
    fn the_editor_command_takes_placeholders_or_gets_the_file_appended() {
        // No placeholder: the filename is appended, as `$EDITOR file` means everywhere.
        assert_eq!(editor_argv("nvim", "/tmp/x", 3, 5).unwrap(), ["nvim", "/tmp/x"]);
        // This user's qutebrowser spelling, which is a terminal and a placeholder.
        assert_eq!(
            editor_argv("kitty -e nvim {file}", "/tmp/x", 3, 5).unwrap(),
            ["kitty", "-e", "nvim", "/tmp/x"]
        );
        // Every placeholder qutebrowser's `_sub_placeholder` knows.
        assert_eq!(
            editor_argv("ed {} {line} {line0} {column} {column0}", "/tmp/x", 3, 5).unwrap(),
            ["ed", "/tmp/x", "3", "2", "5", "4"]
        );
        // A quoted argument survives, which matters for `nvim -c 'normal 3G'`.
        assert_eq!(
            editor_argv("nvim -c 'normal {line}G' {file}", "/tmp/x", 3, 5).unwrap(),
            ["nvim", "-c", "normal 3G", "/tmp/x"]
        );
        assert!(editor_argv("nvim \"", "/tmp/x", 1, 1).is_err());
    }

    #[test]
    fn every_named_key_can_be_injected() {
        use crate::bindings::NamedKey;
        for named in [
            NamedKey::Escape,
            NamedKey::Tab,
            NamedKey::Backspace,
            NamedKey::Return,
            NamedKey::Enter,
            NamedKey::Insert,
            NamedKey::Delete,
            NamedKey::Home,
            NamedKey::End,
            NamedKey::Left,
            NamedKey::Up,
            NamedKey::Right,
            NamedKey::Down,
            NamedKey::PgUp,
            NamedKey::PgDown,
            NamedKey::Space,
            NamedKey::Menu,
            NamedKey::BrowserBack,
            NamedKey::BrowserForward,
            NamedKey::F(1),
            NamedKey::F(24),
        ] {
            assert!(
                vkey_for_named(named).is_some(),
                "{named:?} has no virtual key code to send"
            );
        }
    }

    /// The write-back script has to survive a value with quotes, backslashes and newlines in it —
    /// which is most of what anyone opens an editor for.
    #[test]
    fn write_back_escapes_the_text() {
        let script = write_back_script("a\"b\\c\nd");
        assert!(script.contains(r#"var value = "a\"b\\c\nd";"#), "{script}");
        assert!(!script.contains("a\"b"));
    }
}
