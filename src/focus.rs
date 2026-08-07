//! Which mode a focused text field puts bru in — decided when the **focus** changes, not when a key
//! is pressed.
//!
//! ## The bug this exists to end
//!
//! `keys.rs` used to read `KeyEvent::focus_on_editable_field` and enter insert mode from it. That
//! flag rides on a key event, so it carries two faults at once, and both are one fact:
//!
//! - **The bar lagged by a keypress.** Nothing told bru a field was focused until a key was pressed,
//!   so the mode indicator said `NORMAL` while the caret was already blinking in a search box.
//! - **The first key was eaten.** Opening `https://start.duckduckgo.com/`, which focuses its own
//!   search box, and pressing `:` entered insert mode *and then* looked the key up in insert mode,
//!   where `:` is not bound — so the colon went into the page's field and the command line never
//!   opened. Reported by the user, 2026-08-07.
//!
//! - **It could not tell a field the page focused from one the user clicked into.** One flag, two
//!   causes, and qutebrowser treats them oppositely.
//!
//! ## What qutebrowser does, which is the model
//!
//! Three settings, read here by their qutebrowser names (`configdata.yml:1905-1917`):
//!
//! | | | |
//! |---|---|---|
//! | `input.insert_mode.auto_load` | **false** | a field the *page* focused after loading does not enter insert |
//! | `input.insert_mode.auto_enter` | **true** | a field the *user* clicked into does |
//! | `input.insert_mode.auto_leave` | **true** | focusing something that is not editable leaves insert |
//!
//! qutebrowser splits them by *mechanism*: `auto_load` is decided in `browsertab.py:886` when a load
//! finishes, and `auto_enter` in `eventfilter.py:206` on a **mouse press**, by hit-testing the
//! element under the cursor. bru cannot copy the second half — its pages are Chrome-style
//! `BrowserView`s, so a real click goes from the compositor into Chromium without passing through
//! any handler bru owns. There is no `MouseHandler` to hook.
//!
//! ## So the two are told apart by *user activation*, and the obvious alternative was measured wrong
//!
//! The first thing tried here was timing: a page focuses its box while it is still loading, a person
//! cannot click into a box before it is on the screen, so `on_loading_state_change` should separate
//! them. **It does not, and the page in the bug report is the page that proves it.** Measured
//! 2026-08-07 against `https://start.duckduckgo.com/`, which fires three focus changes, not one:
//!
//! ```text
//! editable=true  readyState=loading   browser-is-loading=true    (the autofocus attribute)
//! editable=false readyState=complete  browser-is-loading=false   (its script blurs the box)
//! editable=true  readyState=complete  browser-is-loading=false   (its script focuses it again)
//! ```
//!
//! The third is the one the user sees, and by then nothing is loading by any measure bru has. A
//! local `<input autofocus>` page with no script at all reported `readyState=complete` and
//! `is_loading=false` too, because Blink flushes autofocus candidates after parsing rather than
//! during it. A rule built on timing would have passed a unit test and failed on the user's screen.
//!
//! What separates them exactly is **`navigator.userActivation.isActive`** — Blink's own record of
//! whether the person has interacted with this document recently. Same measurement, same build:
//!
//! ```text
//! start.duckduckgo.com autofocus, all three changes   isActive=0
//! a local <input autofocus>                           isActive=0
//! `f` then the hint's label onto an input             isActive=1
//! ```
//!
//! So `auto_load` governs `isActive=0` and `auto_enter` governs `isActive=1`, and the loading state
//! is not consulted at all. It is asked in the renderer, where the focus change happens, so there is
//! no race with anything the browser process learns separately.
//!
//! **Say plainly what that costs.** Transient activation lasts about five seconds and *any* key
//! press renews it, so a page that focuses a field within five seconds of a keystroke is treated as
//! a click and does enter insert mode. qutebrowser is not fooled there, because it hit-tests the
//! element under the mouse instead. The trade buys the click half of the behaviour — which is the
//! half bru could not otherwise have at all — and the case it loses needs a page that steals focus
//! seconds after the fact, which is the case `input.insert_mode.auto_enter false` is for.
//!
//! ## bru's own chrome pages are an exception, deliberately
//!
//! `bru://chrome/cookies` has `autofocus` on its filter box so that opening the page and typing a
//! domain is one motion, and its author wrote a test that says so. `auto_load = false` exists
//! because a *web page* must not be able to take the next keystroke; bru's own pages are bru's UI,
//! and the autofocus on them is bru's own decision rather than a stranger's. The renderer marks a
//! report from a `bru://` frame trusted and those enter insert mode whatever `auto_load` says.
//!
//! ## Where the pieces run
//!
//! `on_focused_node_changed` is a **render process** callback (bindings 32600), and `Domnode`'s
//! `is_editable` (6032) can only be asked there. The verdict has to reach the browser process, so
//! this is the fourth module with the shape `scroll.rs`, `editor.rs` and `navigate.rs` already have:
//! a `ProcessMessage` claimed in `ipc.rs` before the message router sees it, so it never goes near
//! the router's `bru://`-only check.
//!
//! **Nothing in the renderer half may touch `BruState`.** It exists in that process and is empty.

use cef::*;

use crate::modes::Mode;

/// The renderer→browser message. One name, four booleans: is what has the focus now editable, did
/// the person do it, was it the keyboard, and is the frame it happened in one of bru's own pages.
const REPORT: &str = "bru.focus.changed";

/// The browser→renderer message: "bru has just clicked; say what has the focus now". See
/// [`after_click`], and the case it exists for — a click onto a field that was already focused
/// changes no focus and fires no callback.
const ASK: &str = "bru.focus.ask";

/// How long after the click to ask. The mouse events and this message travel to the renderer on
/// different channels, so nothing orders them; the delay is what makes the answer be about the
/// document *after* the click rather than before it.
///
/// **60 ms, and it is measured rather than picked.** At 0 ms the answer came back describing the
/// focus as it was before the click on 3 of 5 runs. See the module header for the run.
const AFTER_CLICK_MS: i64 = 60;

// -----------------------------------------------------------------------------------------------
// The renderer half
// -----------------------------------------------------------------------------------------------

/// `ImplRenderProcessHandler::on_focused_node_changed`, forwarded from `app.rs`.
///
/// `node` is `None` when the document has no focused element any more — a blur, or a navigation
/// taking the old document away. That is reported too, and is what `auto_leave` acts on.
///
/// One expression of JavaScript runs here, [`PROBE`], and it is the whole reason the decision is
/// right: it asks Blink whether the person has interacted with this document recently. It is a read
/// with no side effect, and this is not the key path — a focus change happens when a page loads or
/// when something is clicked, never per keystroke.
pub fn renderer_on_focus_changed(frame: Option<&mut Frame>, node: Option<&mut Domnode>) {
    let Some(frame) = frame else {
        return;
    };
    // `is_editable` is true for `<input type=text>`, `<textarea>` and anything `contenteditable` —
    // and false for `<input type=checkbox>` and for a link, which is the distinction that matters.
    let editable = node.map(|node| node.is_editable() != 0).unwrap_or(false);
    let url = CefString::from(&frame.url()).to_string();
    let trusted = is_bru_url(&url);
    // Two bits, one round trip. `?` — no V8 context, which is a frame being torn down — reads as
    // "the page did this, with the keyboard", the pair that changes nothing.
    let probe = evaluate(frame, PROBE).unwrap_or_else(|| "01".to_string());
    let gesture = probe.starts_with('1');
    let keyboard = probe.ends_with('1');

    if std::env::var_os("BRU_DEBUG_FOCUS").is_some() {
        eprintln!(
            "bru[focus,renderer]: editable={editable} gesture={gesture} keyboard={keyboard} \
             trusted={trusted} probe={:?} url={url:?}",
            evaluate(frame, DIAGNOSTIC).unwrap_or_default(),
        );
    }

    let Some(mut message) = process_message_create(Some(&CefString::from(REPORT))) else {
        return;
    };
    if let Some(arguments) = message.argument_list() {
        arguments.set_bool(0, editable as ::std::os::raw::c_int);
        arguments.set_bool(1, gesture as ::std::os::raw::c_int);
        arguments.set_bool(2, keyboard as ::std::os::raw::c_int);
        arguments.set_bool(3, trusted as ::std::os::raw::c_int);
    }
    frame.send_process_message(ProcessId::BROWSER, Some(&mut message));
}

/// Two characters, `<gesture><keyboard>`.
///
/// - **gesture** — `navigator.userActivation.isActive`, Blink's own record of whether the person has
///   interacted with this document recently. It is what tells a click from a page focusing itself.
/// - **keyboard** — `:focus-visible` on the element that has just taken the focus. Chromium applies
///   it to a button reached with Tab and **not** to one reached with the mouse, which is the only
///   way from here to tell those two apart; `on_focused_node_changed` says nothing about how.
///
/// Written to answer two characters and never to throw: a page can shadow almost anything, and an
/// exception here would be an exception on every focus change on that page.
const PROBE: &str = "(function(){try{\
var u=navigator.userActivation,a=document.activeElement;\
return ((u&&u.isActive)?'1':'0')+((a&&a.matches&&a.matches(':focus-visible'))?'1':'0')\
}catch(e){return '01'}})()";

/// What `BRU_DEBUG_FOCUS=1` prints beside the verdict:
/// `<isActive>,<hasBeenActive>,<readyState>,<:focus-visible>`.
///
/// `readyState` is in it because it is the signal that *looked* right and measured wrong — see the
/// module header — and the next person to reach for it should see it saying `complete`.
const DIAGNOSTIC: &str = "(function(){try{var u=navigator.userActivation||{},a=document.activeElement;\
return (u.isActive?1:0)+','+(u.hasBeenActive?1:0)+','+document.readyState+','+\
((a&&a.matches&&a.matches(':focus-visible'))?1:0)}catch(e){return '?'}})()";

/// Run an expression in the frame's own V8 context. The same shape as `scroll.rs::evaluate`.
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

/// Whether a frame URL is one of bru's own pages. Kept as a function because the renderer and the
/// tests both ask it, and because "starts with bru://" is exactly the check that must not quietly
/// become "contains".
pub fn is_bru_url(url: &str) -> bool {
    url.starts_with("bru://")
}

/// Renderer side of [`ASK`]. Called from `ipc::renderer_on_process_message_received`; answers true
/// when the message was ours.
///
/// The answer comes from CEF's own `Domnode::is_editable` rather than from a JavaScript guess at
/// what "editable" means, which is why it goes through `visit_dom` instead of another `eval`: one
/// definition of editable in this file, not two that can disagree.
pub fn renderer_on_ask(frame: Option<&Frame>, message: Option<&ProcessMessage>) -> bool {
    let Some(message) = message else {
        return false;
    };
    if CefString::from(&message.name()).to_string() != ASK {
        return false;
    }
    let Some(frame) = frame else {
        return true;
    };
    let mut visitor = FocusedNode::new(frame.clone());
    frame.visit_dom(Some(&mut visitor));
    true
}

wrap_domvisitor! {
    struct FocusedNode {
        frame: Frame,
    }

    impl Domvisitor {
        fn visit(&self, document: Option<&mut Domdocument>) {
            let editable = document
                .and_then(|document| document.focused_node())
                .map(|node| node.is_editable() != 0)
                .unwrap_or(false);
            let trusted = is_bru_url(&CefString::from(&self.frame.url()).to_string());

            if std::env::var_os("BRU_DEBUG_FOCUS").is_some() {
                eprintln!("bru[focus,renderer]: after a click, editable={editable} trusted={trusted}");
            }

            let Some(mut message) = process_message_create(Some(&CefString::from(REPORT))) else {
                return;
            };
            if let Some(arguments) = message.argument_list() {
                arguments.set_bool(0, editable as ::std::os::raw::c_int);
                // bru sent the click itself, on behalf of a key the user pressed, and it was a
                // *mouse* click — which is exactly the pair `auto_enter` and `auto_leave` want.
                arguments.set_bool(1, 1);
                arguments.set_bool(2, 0);
                arguments.set_bool(3, trusted as ::std::os::raw::c_int);
            }
            self.frame.send_process_message(ProcessId::BROWSER, Some(&mut message));
        }
    }
}

/// Ask, after bru has clicked somewhere itself. The one caller is `hints::click`.
///
/// **This is not a general mouse hook and there is no such thing here.** bru's tabs are Chrome-style
/// `BrowserView`s, so a click the *person* makes with the mouse goes from the compositor into
/// Chromium without passing through any handler bru owns. What this covers is the click bru makes
/// on the user's behalf when a hint is followed, which is how a keyboard-driven browser gets into a
/// text field at all.
pub fn after_click(browser: &mut Browser) {
    let Some(frame) = browser.main_frame() else {
        return;
    };
    let mut task = AskFocused::new(frame);
    post_delayed_task(ThreadId::UI, Some(&mut task), AFTER_CLICK_MS);
}

wrap_task! {
    struct AskFocused {
        frame: Frame,
    }

    impl Task {
        fn execute(&self) {
            let Some(mut message) = process_message_create(Some(&CefString::from(ASK))) else {
                return;
            };
            self.frame.send_process_message(ProcessId::RENDERER, Some(&mut message));
        }
    }
}

// -----------------------------------------------------------------------------------------------
// The browser half: the decision
// -----------------------------------------------------------------------------------------------

/// What a focus change should do to the mode. Pure, and separated from the CEF call so the table of
/// cases can be a unit test rather than a paragraph of prose — CEF-NOTES trap 13 says a test may not
/// post a task, and everything below this line does.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verdict {
    /// Enter insert mode, if the window is in normal mode.
    Enter,
    /// Leave insert mode, if the window is in it.
    Leave,
    /// Change nothing.
    Nothing,
}

/// The settings, as one value, so [`decide`] can be tested without the live store.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rules {
    pub auto_load: bool,
    pub auto_enter: bool,
    pub auto_leave: bool,
}

/// The four names, in one place, so the table in `settings.rs` and the reads here cannot drift
/// apart without a test noticing.
pub const NAMES: [&str; 4] = [
    "input.insert_mode.auto_load",
    "input.insert_mode.auto_enter",
    "input.insert_mode.auto_leave",
    "input.insert_mode.leave_on_load",
];

impl Rules {
    /// What `config.lua` and `:set` have made of them, over bru's own compiled-in defaults.
    ///
    /// **There is no second copy of the defaults here.** They live once, in `settings.rs`'s table,
    /// which is what `config.lua` overrides; a `const` beside this function would be a value that
    /// could disagree with the one the browser actually runs on.
    fn live() -> Rules {
        Rules {
            auto_load: crate::settings::is_on(NAMES[0]),
            auto_enter: crate::settings::is_on(NAMES[1]),
            auto_leave: crate::settings::is_on(NAMES[2]),
        }
    }
}

/// The rule, whole, in one function.
///
/// `gesture` is `navigator.userActivation.isActive` as the renderer read it at the moment the focus
/// moved: true when the person did this, false when the page did it to itself. See the module
/// header for the measurement that chose it over the loading state, and for what it costs.
///
/// `trusted` is a `bru://` page. It is not a licence to bypass `auto_enter` — a chrome page the user
/// deliberately left with `auto_enter false` still obeys it — only to bypass `auto_load`, which
/// exists to keep *strangers* out of the keyboard.
pub fn decide(rules: Rules, editable: bool, gesture: bool, keyboard: bool, trusted: bool) -> Verdict {
    if !editable {
        // `auto_leave`, and it is **narrower than "nothing editable has the focus"**, which is what
        // this did first. qutebrowser leaves insert only from `_mousepress_insertmode_cb`
        // (`eventfilter.py:196-215`) — a *mouse press* on a non-editable element — and the reason
        // shows up the moment it is widened. Measured 2026-08-07 on `bru://chrome/cookies`, whose
        // own instructions say "Tab reaches the two buttons": Tab off the filter box onto a
        // `<button>` left insert mode, so Enter on that button became bru's key instead of the
        // page's and the button could not be pressed. The same widening breaks Tab-then-Space on a
        // checkbox in any form on the web.
        //
        // A page blurring its own field is not a leave either, for the same reason it is not an
        // enter: the page does not get to move bru's mode. What a *navigation* does is
        // `leave_on_load`, below, which is where that case belongs.
        return if rules.auto_leave && gesture && !keyboard {
            Verdict::Leave
        } else {
            Verdict::Nothing
        };
    }
    if gesture {
        // A click, or a hint followed onto an input — `hints.rs` follows with a real
        // `send_mouse_click_event`, so Blink counts it, and this is why hinting an input enters
        // insert mode without `hints.rs` knowing this file exists. Measured 2026-08-07.
        return if rules.auto_enter {
            Verdict::Enter
        } else {
            Verdict::Nothing
        };
    }
    // The page did it to itself. `auto_load` is false by default, and that is the whole of the fix:
    // the start page's search box no longer holds the next key the user presses.
    //
    // bru's own pages are the exception. `bru://chrome/cookies` focuses its filter box so that
    // opening it and typing a domain is one motion; refusing that would leave Chromium blinking a
    // caret under a bar that says NORMAL, which is the same contradiction on bru's own page.
    if rules.auto_load || trusted {
        Verdict::Enter
    } else {
        Verdict::Nothing
    }
}

/// Browser side of the report. Called from `ipc::on_process_message_received` before the message
/// router sees the message; answers true when the message was ours.
pub fn on_report(browser: Option<&Browser>, message: Option<&ProcessMessage>) -> bool {
    let Some(message) = message else {
        return false;
    };
    if CefString::from(&message.name()).to_string() != REPORT {
        return false;
    }
    let Some(browser) = browser else {
        return true;
    };
    let id = browser.identifier();

    let (editable, gesture, keyboard, trusted) = match message.argument_list() {
        Some(arguments) => (
            arguments.bool(0) != 0,
            arguments.bool(1) != 0,
            arguments.bool(2) != 0,
            arguments.bool(3) != 0,
        ),
        None => return true,
    };

    // Only the tab that is showing, in its own window. A background tab focusing a field must not
    // move the mode of the tab being typed in, and `is_active_browser` answers false for a chrome
    // strip too — which is what keeps the command line's own `#cmdline` input, focused every time
    // `:` is pressed, out of this entirely.
    let Some(state) = crate::state::BruState::instance() else {
        return true;
    };
    let (window, active) = {
        let guard = state.lock().expect("state mutex poisoned");
        (guard.window_of_browser(id), guard.is_active_browser(id))
    };
    let Some(window) = window else {
        return true;
    };

    let verdict = if active {
        decide(Rules::live(), editable, gesture, keyboard, trusted)
    } else {
        Verdict::Nothing
    };

    if std::env::var_os("BRU_DEBUG_FOCUS").is_some() {
        eprintln!(
            "bru[focus]: browser {id} window {window} editable={editable} gesture={gesture} \
             keyboard={keyboard} trusted={trusted} active={active} -> {verdict:?}",
        );
    }

    apply(window, verdict);
    true
}

/// `LoadHandler::on_load_start` for the main frame of the showing tab, forwarded from `load.rs`.
///
/// qutebrowser's `input.insert_mode.leave_on_load`, default true (`configdata.yml:1926`). It is the
/// other half of the narrowed `auto_leave` above: a page bru was typing into is gone, so insert mode
/// is a statement about a document that no longer exists. Without it the bar would say INSERT over a
/// page nobody has focused anything on.
pub fn on_load_started(window: u32) {
    if !crate::settings::is_on(NAMES[3]) {
        return;
    }
    if std::env::var_os("BRU_DEBUG_FOCUS").is_some() {
        eprintln!("bru[focus]: window {window} started loading -> leave_on_load");
    }
    apply(window, Verdict::Leave);
}

/// Move the window's mode, and tell its bar in the same breath.
///
/// The push is the point of the whole file: the mode indicator now changes when the focus changes,
/// which is the moment the fact became true, rather than on the next keypress.
fn apply(window: u32, verdict: Verdict) {
    let Some(state) = crate::state::BruState::instance() else {
        return;
    };
    match verdict {
        Verdict::Nothing => {}
        Verdict::Enter => {
            // `only_if_normal` is what keeps a page's focus event from pulling the user out of
            // passthrough or out of a half-typed command line.
            let entered = state
                .lock()
                .expect("state mutex poisoned")
                .enter_mode_in(window, Mode::Insert, true);
            if entered {
                crate::ipc::set_mode_for(window, Mode::Insert.name().to_string());
            }
        }
        Verdict::Leave => {
            // Insert mode only. Leaving whatever happens to be current would pop command mode when
            // a page blurred a field under an open command line, and pop passthrough — which the
            // user asked for by hand — when a page moved its own focus.
            let now = {
                let mut guard = state.lock().expect("state mutex poisoned");
                if guard.mode_in(window) != Mode::Insert {
                    return;
                }
                if !guard.leave_mode_in(window) {
                    return;
                }
                guard.mode_in(window)
            };
            crate::ipc::set_mode_for(window, now.name().to_string());
        }
    }
}

// -----------------------------------------------------------------------------------------------
// Tests — the pure half. Nothing here posts a CEF task (trap 13).
// -----------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// The rules bru ships, read out of the one place they are written down. Every case below runs
    /// against the table `config.lua` overrides, not against a copy of it.
    fn shipped() -> Rules {
        let of = |name: &str| {
            crate::settings::def(name)
                .unwrap_or_else(|| panic!("{name} is not in SETTINGS"))
                .default
                == Some("true")
        };
        Rules {
            auto_load: of(NAMES[0]),
            auto_enter: of(NAMES[1]),
            auto_leave: of(NAMES[2]),
        }
    }

    /// `decide` reads better at the call site under the names of the facts it takes. Every case
    /// below is one measured `BRU_DEBUG_FOCUS=1` line, not an invented one.
    const PAGE: bool = false;
    const PERSON: bool = true;
    const MOUSE: bool = false;
    const TAB: bool = true;
    const WEB: bool = false;
    const CHROME: bool = true;

    /// The bug, as a table. **This is the third of the three focus changes
    /// `https://start.duckduckgo.com/` fires**, the one its own script does after the load has
    /// finished — the one a rule built on the loading state gets wrong. Measured 2026-08-07:
    /// `editable=true gesture=false readyState=complete`.
    #[test]
    fn a_page_focusing_its_own_search_box_does_not_enter_insert() {
        let rules = shipped();
        assert_eq!(decide(rules, true, PAGE, TAB, WEB), Verdict::Nothing);
        // With auto_load on it does — which is what the setting is for, and it is the switch that
        // puts the old behaviour back for anyone who wants it.
        let opted_in = Rules { auto_load: true, ..rules };
        assert_eq!(decide(opted_in, true, PAGE, TAB, WEB), Verdict::Enter);
    }

    /// The half that must not regress: clicking a search box and typing still works. Measured
    /// through a hint follow, which is a real `send_mouse_click_event`: `gesture=true`.
    #[test]
    fn clicking_into_a_field_enters_insert() {
        let rules = shipped();
        assert_eq!(decide(rules, true, PERSON, MOUSE, WEB), Verdict::Enter);
        // And Tab onto a field, which is a gesture too and is just as much the user's doing.
        assert_eq!(decide(rules, true, PERSON, TAB, WEB), Verdict::Enter);
        let opted_out = Rules { auto_enter: false, ..rules };
        assert_eq!(decide(opted_out, true, PERSON, MOUSE, WEB), Verdict::Nothing);
    }

    /// `bru://chrome/cookies` focuses its filter box on load and its author documented that typing a
    /// domain is the first thing that happens. bru's own pages are not web pages, and a caret
    /// blinking under a bar that says NORMAL is the contradiction this avoids.
    #[test]
    fn brus_own_pages_may_focus_their_own_field() {
        let rules = shipped();
        assert_eq!(decide(rules, true, PAGE, TAB, CHROME), Verdict::Enter);
        // The exception is over `auto_load` only. Someone who turned `auto_enter` off wants no
        // automatic insert mode anywhere, and a chrome page does not overrule that.
        let opted_out = Rules { auto_enter: false, ..rules };
        assert_eq!(decide(opted_out, true, PERSON, MOUSE, CHROME), Verdict::Nothing);

        assert!(is_bru_url("bru://chrome/cookies"));
        assert!(!is_bru_url("https://start.duckduckgo.com/"));
        // Not "contains": a page may put bru:// in its own query string.
        assert!(!is_bru_url("https://example.com/?x=bru://chrome/cookies"));
    }

    /// `auto_leave`, and it is qutebrowser's exactly: a **mouse press** on a non-editable element,
    /// `eventfilter.py:196-215`. The two neighbouring cases are the ones that measured wrong when
    /// this was written wider.
    #[test]
    fn clicking_something_that_is_not_editable_leaves_insert() {
        let rules = shipped();
        assert_eq!(decide(rules, false, PERSON, MOUSE, WEB), Verdict::Leave);

        // **Tab must not.** `bru://chrome/cookies` says "Tab reaches the two buttons"; leaving
        // insert there makes Enter on the focused button bru's key and the button unpressable.
        // Measured 2026-08-07 on that page: `editable=false gesture=true :focus-visible=1`.
        assert_eq!(decide(rules, false, PERSON, TAB, WEB), Verdict::Nothing);
        assert_eq!(decide(rules, false, PERSON, TAB, CHROME), Verdict::Nothing);

        // **A page blurring its own field must not either.** It is the second of duckduckgo's
        // three focus changes, and a page does not get to move bru's mode in either direction.
        assert_eq!(decide(rules, false, PAGE, MOUSE, WEB), Verdict::Nothing);

        let opted_out = Rules { auto_leave: false, ..rules };
        assert_eq!(decide(opted_out, false, PERSON, MOUSE, WEB), Verdict::Nothing);
    }

    /// **What the key path stopped paying**, which is the other half of this change and the half
    /// that is easy to forget to measure.
    ///
    /// `keys.rs` used to run this on **every keystroke**: take the `BruState` mutex and ask it to
    /// enter insert mode, whenever `focus_on_editable_field` was set. On a page with a focused
    /// search box — the start page, every time — that was the cost of pressing `j`. It is now zero,
    /// because `on_pre_key_event` does not mention focus at all; the same work happens once per
    /// focus change instead of once per key.
    ///
    /// Timed here rather than asserted, in the shape `state.rs::the_key_path_cost_of_a_per_window_mode`
    /// uses. The bound is loose on purpose: this runs unoptimised under `cargo test` on a machine
    /// with other agents on it, and what it pins is that the removed work was not free.
    #[test]
    fn the_key_path_no_longer_pays_for_a_focused_field() {
        const ROUNDS: u32 = 200_000;

        let state = crate::state::BruState::new();
        let window = state
            .lock()
            .expect("state mutex poisoned")
            .open_window_slot();

        // What every keystroke used to cost while a field had the focus.
        let per_key = {
            for _ in 0..10_000 {
                let _ = state
                    .lock()
                    .expect("state mutex poisoned")
                    .enter_mode_in(window, Mode::Insert, true);
            }
            let start = std::time::Instant::now();
            for _ in 0..ROUNDS {
                std::hint::black_box(
                    state
                        .lock()
                        .expect("state mutex poisoned")
                        .enter_mode_in(window, Mode::Insert, true),
                );
            }
            start.elapsed().as_nanos() as f64 / ROUNDS as f64
        };

        println!(
            "focus: the removed per-keystroke lock-and-enter cost {per_key:.1} ns; the key path \
             now pays 0 of it, because keys.rs no longer reads focus_on_editable_field"
        );
        assert!(
            per_key > 0.0,
            "if this measures nothing, the measurement is broken rather than the work free"
        );
        // Nothing this file does is reachable from `on_pre_key_event`. That is a fact about the
        // call graph rather than a clock, and it is asserted where it can be: the three functions
        // the key path could have called are `on_report`, `on_load_started` and `decide`, and
        // `keys.rs` names none of them. See the commit that removed the block.
    }

    /// bru's defaults are qutebrowser's — `configdata.yml:1905`, `:1911`, `:1916` and `:1926`, read
    /// there rather than recalled. Written as an assertion so that changing the table is changing a
    /// test.
    #[test]
    fn the_defaults_are_qutebrowsers() {
        assert!(!shipped().auto_load, "auto_load ships false");
        assert!(shipped().auto_enter, "auto_enter ships true");
        assert!(shipped().auto_leave, "auto_leave ships true");
        assert_eq!(
            crate::settings::def(NAMES[3]).and_then(|def| def.default),
            Some("true"),
            "leave_on_load ships true",
        );
        // And all three are settings `:set` and `config.lua` can name, which is what makes the
        // paragraph above an option rather than a decision taken on the user's behalf.
        for name in NAMES {
            assert!(crate::settings::is_known(name), "{name} is not settable");
        }
    }
}
