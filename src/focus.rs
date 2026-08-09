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
//! Four settings, read here by their qutebrowser names and shipped with qutebrowser's defaults:
//!
//! | | | |
//! |---|---|---|
//! | `input.insert_mode.auto_load` | **false** | a field the *page* focused does not enter insert (`configdata.yml:1905`) |
//! | `input.insert_mode.auto_enter` | **true** | a field the *user* clicked into does (`:1911`) |
//! | `input.insert_mode.auto_leave` | **true** | clicking something that is not editable leaves insert (`:1916`) |
//! | `input.insert_mode.leave_on_load` | **true** | a new page load leaves insert (`:1926`) |
//!
//! qutebrowser splits them by *mechanism*: `auto_load` is decided in `browsertab.py:886` when a load
//! finishes, and `auto_enter` in `eventfilter.py:196-215` on a **mouse press**, by hit-testing the
//! element under the cursor. bru cannot copy the second half as written — its pages are Chrome-style
//! `BrowserView`s, so a click the *person* makes goes from the compositor into Chromium without
//! passing through any handler bru owns. There is no `MouseHandler` to hook.
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
//! That measurement stands, and `auto_load` still reads `isActive` — but **user activation no
//! longer decides `auto_enter`, because the cost written down here came due.** What the paragraph
//! below used to say was that transient activation lasts about five seconds and *any* key press
//! renews it, so a page that focuses a field just after a keystroke is read as a click. It named
//! the losing case as "a page that steals focus seconds after the fact". The real losing case was
//! smaller and far more common, and it is **Escape**.
//!
//! ## Why entering insert is now only ever bru's own click
//!
//! Reported by the user 2026-08-09 on `https://accounts.google.com/`: Escape did not leave insert
//! mode, and pressing it twice did. `BRU_DEBUG_KEYS=1` showed the key was never lost —
//! `pending=27` on the key-up says bru swallowed the press and ran `mode-leave` every time:
//!
//! ```text
//! bru[keys]:  KEYUP code=27 pending=27                      Escape, swallowed: insert was left
//! bru[focus]: editable=true gesture=true keyboard=false -> Enter      and immediately re-entered
//! ```
//!
//! Four presses in that log, four identical pairs. bru left insert mode and the page put it
//! straight back, because **pressing Escape is itself what renews the activation** that makes the
//! page's refocus look like a person clicking into a field. The gesture bit cannot see the
//! difference: that `keyboard=false` is a page refocusing itself, and a hint's click reports the
//! same two values.
//!
//! So the bit was replaced by the fact. The report now carries **which path it came from**, and
//! entering insert is the answer to [`ASK`] — a click bru sent itself — and nothing else. That is
//! qutebrowser's rule exactly: `auto_enter` fires from `_mousepress_insertmode_cb`
//! (`eventfilter.py:196-215`), a real mouse press, and never from a focus event.
//!
//! **Say plainly what it costs**, because this is the second time this file has traded here and the
//! trade was the user's to make (chosen 2026-08-09):
//!
//! - Tab onto a text field no longer enters insert mode. Neither does a click the *person* makes
//!   with the mouse. Both did, through activation, and both are now `i`.
//! - qutebrowser has the same behaviour for Tab and the opposite for the mouse, and it can only
//!   have it because it hit-tests the element under the cursor — which bru has no hook for.
//!
//! Leaving insert still reads activation, and may: `auto_leave` erring towards normal mode costs a
//! keystroke, where `auto_enter` erring towards insert costs the Escape above.
//!
//! ## A focus change is not enough on its own, and the missing case is the start page's
//!
//! `on_focused_node_changed` fires when the focused node *changes*. Click a field that already has
//! the focus and nothing changes, so nothing fires. That is not a corner: it is the page in the bug
//! report. Measured 2026-08-07 on `https://start.duckduckgo.com/` — its script focuses its search
//! box, bru correctly stays in normal, and `hint inputs` then `a` clicked the box at (729, 539) with
//! **no focus change reported and the mode still normal**. The user would have hinted their own
//! search box and then found that typing scrolled.
//!
//! So the click says so itself: `hints::click` calls [`after_click`], which asks the renderer what
//! has the focus now. Same page, same script, after that line: `after a click, editable=true` →
//! `-> Enter` → `mode=insert`. Since the Escape above, this is not merely the case that *also*
//! works — it is the only way a web page's field enters insert mode at all.
//!
//! **This covers the click bru makes, not the click the person makes.** A real mouse click into a
//! field that already had the focus still does nothing, and cannot be made to: see the paragraph
//! above about there being no mouse hook. For a browser driven by hints that is the small half.
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
//! the router's `bru://`-only check. Two names, one each way: [`REPORT`] and [`ASK`].
//!
//! The one thing this file asks in JavaScript is [`PROBE`], because Blink exposes user activation
//! nowhere else. **Editable is never asked in JavaScript** — both paths go through CEF's own
//! `Domnode::is_editable`, the focus one from the node the callback hands over and the click one
//! through `Frame::visit_dom`. One definition of editable, not two that can disagree.
//!
//! **Nothing in the renderer half may touch `BruState`.** It exists in that process and is empty.

use cef::*;

use crate::modes::Mode;

/// The renderer→browser message. One name, five booleans: is what has the focus now editable, did
/// the person do it, was it the keyboard, is the frame it happened in one of bru's own pages, and
/// **is this the answer to a click bru sent itself**.
///
/// The fifth is the one that decides insert mode, and it is a fact about which code path filled the
/// message in rather than anything read off the document — see the module header, and the Escape
/// that the fourth-and-a-half guess at it could not survive.
const REPORT: &str = "bru.focus.changed";

/// The browser→renderer message: "bru has just clicked; say what has the focus now". See
/// [`after_click`], and the case it exists for — a click onto a field that was already focused
/// changes no focus and fires no callback.
const ASK: &str = "bru.focus.ask";

/// How long after the click to ask.
///
/// **Say what was measured and what was not.** Measured 2026-08-07, hinting a field the page had
/// already focused, five runs each: at **0 ms** the answer described the document after the click
/// 5/5 times, and at **60 ms** 5/5 as well. So this delay is not currently buying anything on this
/// machine, and the honest reason it is here is a hazard rather than a symptom: the mouse events and
/// this message reach the renderer on different channels and *nothing orders them*, so a busy
/// renderer could answer about the document before the click. 60 ms after following a hint is not
/// perceptible, which makes it a cheap way to not depend on that.
///
/// (One of the five 0 ms runs ended in normal mode all the same, and it was not this: the ask
/// answered `editable=true` and insert was entered, then a *real* focus change arrived saying the
/// field had been blurred. The hint's click point had missed the input. That is `hints.rs`'s aim,
/// not this delay.)
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
        // The focus moved on its own account. Whoever caused it — the page, Tab, or a real mouse —
        // this is not bru clicking, and so it is not what enters insert mode.
        arguments.set_bool(4, 0);
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

            // qutebrowser's `_move_text_cursor`, and this is the only place in bru that can host
            // it: see [`MOVE_CURSOR_TO_END`] for why the *browser* half must not.
            if editable {
                self.frame.execute_java_script(
                    Some(&CefString::from(MOVE_CURSOR_TO_END)),
                    None,
                    0,
                );
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
                // The one path that answers for a click bru sent, and therefore the one path that
                // may enter insert mode.
                arguments.set_bool(4, 1);
            }
            self.frame.send_process_message(ProcessId::BROWSER, Some(&mut message));
        }
    }
}

/// Put the caret after the text a field already holds, once bru's own click has landed in one.
///
/// **This is the belt now, not the whole fix.** The same move is armed *before* the click by
/// [`arm_caret_js`], which runs it inside the click's own dispatch so no frame paints with the
/// caret in the wrong place; this copy runs [`AFTER_CLICK_MS`] later and moves nothing when the
/// arm already did — both end at the same position. It stays for the arm that could not run at
/// all — a V8 context that refused, whose echo releases the click regardless (see
/// [`renderer_on_arm`]) — and because this path is the one that also decides insert mode.
///
/// **qutebrowser's `_move_text_cursor`** (`browser/webelem.py:364`, fired on a zero-millisecond
/// timer after every synthetic click, and again from `_click_editable`), which calls
/// `javascript/webelem.js:405`:
///
/// ```js
/// elem.selectionStart = elem.value.length;
/// elem.selectionEnd = elem.value.length;
/// ```
///
/// Reported by the user 2026-08-09: with `test` already typed in a field, following a hint back
/// onto it dropped the caret wherever the click had landed — in the middle of the word — so the
/// next letter went into the middle of what was there.
///
/// **Why it runs here and not in the browser half.** The obvious home is `apply`, beside the
/// `Verdict::Enter` this always accompanies. It would be wrong. `decide` cannot tell bru's own
/// click from a **real mouse click** — bru's tabs are Chrome-style `BrowserView`s and there is no
/// mouse hook (see the module header), so both arrive as `editable=true gesture=true keyboard=0`.
/// A person who clicks halfway along a filled-in field means the caret to go *there*, and moving it
/// to the end would be a new bug in place of this one. [`ASK`] is the one path that only ever
/// follows a click bru sent itself, which is exactly qutebrowser's condition.
///
/// The guard is qutebrowser's `is_text_input` (`webelem.py:266`) — `input`, `textarea`, or the
/// `combobox`/`textbox` roles. **Editable is not re-asked here**: the caller has it from
/// `Domnode::is_editable`, and the module header's rule is that there is one definition of it.
///
/// `try`/`catch` because Chromium *throws* on `selectionStart` for `input type=email` and
/// `type=number`, which do not support selection; qutebrowser wears that as a logged exception and
/// there is no reason to. Wrapped, those fields simply keep the caret the click gave them.
const MOVE_CURSOR_TO_END: &str = "(function(){try{\
var e=document.activeElement;if(!e)return;\
var t=(e.tagName||'').toLowerCase(),r=e.getAttribute&&e.getAttribute('role');\
if(t!=='input'&&t!=='textarea'&&r!=='combobox'&&r!=='textbox')return;\
var n=(e.value||'').length;e.selectionStart=n;e.selectionEnd=n\
}catch(x){}})()";

/// The browser→renderer half of [`click_through`]: "arm the caret move, then say so". Carries the
/// click's view coordinates, which come back unchanged on [`ARMED`] — the message is the state, so
/// nothing has to be remembered across the round trip.
const ARM: &str = "bru.caret.arm";

/// The renderer→browser half: the listener is installed, the click may leave now.
const ARMED: &str = "bru.caret.armed";

/// How long an armed listener stays live, in the page's own clock.
///
/// With the round trip below the click cannot overtake the arm, so in the ordinary run the gap the
/// listener waits is one browser→renderer hop — measured 2026-08-09 in the page itself
/// (`performance.now()` from install to click event): **0.9–2.0 ms over ten follows**. The window
/// is for the run that never finishes: a click sent at a browser that is torn down before the
/// events land leaves the listener armed, and one with no expiry would then move the caret of
/// whatever the person clicked next, minutes later. Within the window that mistake is still
/// possible and accepted — it is the same move the [`AFTER_CLICK_MS`] path makes on every hint
/// into a field — and past it the listener is inert.
const ARM_WINDOW_MS: u32 = 500;

/// The same move as [`MOVE_CURSOR_TO_END`], armed **before** the click instead of asked for after
/// it — the difference between a caret that appears at the end and one that visibly jumps there.
///
/// Reported by the user 2026-08-09: with `test` already in a field, following a hint back onto it
/// painted the caret where the click landed and then moved it, and the step was visible. It was:
/// the move ran from [`ASK`]'s DOM visitor, [`AFTER_CLICK_MS`] = 60 ms after the click, which is
/// several painted frames of the caret sitting in the wrong place. Measured 2026-08-09 with a
/// capture listener sampling `selectionStart` at the first `requestAnimationFrame` after the
/// click, on a field holding `test` whose centre lands at offset 1: **5/5 first frames painted the
/// caret at 1**, and it reached 4 only afterwards.
///
/// The listener is one-shot and runs **inside the click's own dispatch**, synchronously, so no
/// frame is ever painted between the click's caret and the corrected one — same trials, armed:
/// **10/10 first frames at 4**. Re-arming replaces the previous listener rather than stacking a
/// second, because a rapid hint session clicks many times and each arm is for exactly one click.
/// `__bru_caret_gap` is the one diagnostic it leaves: how long the listener waited, which is the
/// number [`ARM_WINDOW_MS`] is judged against.
fn arm_caret_js() -> String {
    format!(
        "(function(){{try{{\
        if(window.__bru_caret_arm)window.removeEventListener('click',window.__bru_caret_arm,true);\
        var t0=performance.now();\
        var h=function(){{\
        window.removeEventListener('click',h,true);window.__bru_caret_arm=null;\
        window.__bru_caret_gap=performance.now()-t0;\
        if(window.__bru_caret_gap>{ARM_WINDOW_MS})return;\
        {MOVE_CURSOR_TO_END};\
        }};\
        window.__bru_caret_arm=h;\
        window.addEventListener('click',h,true);\
        }}catch(x){{}}}})()"
    )
}

/// A real click at a point bru located, with the caret move armed in the page **first** — the
/// callers are `hints::click` and `utilcmds::click`, with `x`/`y` already in view coordinates.
/// (`caret.rs`'s `selection-follow` click is deliberately not one: it follows a link out of the
/// selection, and a link has no caret to move.)
///
/// **Why this is a round trip and not a fire-and-forget script.** Mouse events and
/// `execute_java_script` reach the renderer on different mojo channels and nothing orders them —
/// the hazard [`AFTER_CLICK_MS`] is written against. The first version of this arm ignored that
/// and sent the script immediately before the click, and the race is not theoretical: measured
/// 2026-08-09, the first painted frame was correct in only **6 of 10** follows — the other four
/// times the click arrived first and the old visible jump stood. So the ordering is causal now:
/// the renderer installs the listener while handling [`ARM`], answers [`ARMED`], and only then do
/// the mouse events leave the browser process. The click cannot arrive before a listener whose
/// installation it waited on; same trials, **10/10**. What it costs is one message round trip
/// before the click — the gap measured in the page is 0.9–2.0 ms (see [`ARM_WINDOW_MS`]) — on a
/// path a person triggers at most once per hint.
pub fn click_through(browser: &mut Browser, x: i32, y: i32) {
    let Some(frame) = browser.main_frame() else {
        // No frame to arm in; a click into nothing still behaves like one.
        send_click(browser, x, y);
        return;
    };
    let Some(mut message) = process_message_create(Some(&CefString::from(ARM))) else {
        send_click(browser, x, y);
        return;
    };
    if let Some(arguments) = message.argument_list() {
        arguments.set_int(0, x);
        arguments.set_int(1, y);
    }
    frame.send_process_message(ProcessId::RENDERER, Some(&mut message));
}

/// Renderer side of [`ARM`]. Called from `ipc::renderer_on_process_message_received`; answers true
/// when the message was ours.
///
/// The echo is unconditional: a frame whose V8 context refuses the script — one being torn down —
/// still answers, because the click this is holding up must happen regardless. What is guaranteed
/// is only the order: whatever arming did happen is done before the browser process learns it may
/// click.
pub fn renderer_on_arm(frame: Option<&Frame>, message: Option<&ProcessMessage>) -> bool {
    let Some(message) = message else {
        return false;
    };
    if CefString::from(&message.name()).to_string() != ARM {
        return false;
    }
    let Some(frame) = frame else {
        return true;
    };
    let (x, y) = match message.argument_list() {
        Some(arguments) => (arguments.int(0), arguments.int(1)),
        None => return true,
    };
    evaluate(frame, &arm_caret_js());
    let Some(mut echo) = process_message_create(Some(&CefString::from(ARMED))) else {
        return true;
    };
    if let Some(arguments) = echo.argument_list() {
        arguments.set_int(0, x);
        arguments.set_int(1, y);
    }
    frame.send_process_message(ProcessId::BROWSER, Some(&mut echo));
    true
}

/// Browser side of [`ARMED`]: the listener is in place, so the click leaves now. Called from
/// `ipc::on_process_message_received` before the message router sees the message; answers true
/// when the message was ours.
///
/// Nothing here navigates or creates a browser, so CEF-NOTES trap 12 does not apply — the mouse
/// events are input injection, the same calls the old inline click made.
pub fn on_armed(browser: Option<&Browser>, message: Option<&ProcessMessage>) -> bool {
    let Some(message) = message else {
        return false;
    };
    if CefString::from(&message.name()).to_string() != ARMED {
        return false;
    }
    let Some(browser) = browser else {
        return true;
    };
    let (x, y) = match message.argument_list() {
        Some(arguments) => (arguments.int(0), arguments.int(1)),
        None => return true,
    };
    if std::env::var_os("BRU_DEBUG_FOCUS").is_some() {
        eprintln!("bru[focus]: armed — clicking at ({x}, {y})");
    }
    send_click(&mut browser.clone(), x, y);
    true
}

/// The click itself: a move first, because hover state is what a page's own handlers look at and a
/// press with no preceding move arrives at an element that was never entered; then press and
/// release; then [`after_click`], which is how a hint onto an already-focused field still enters
/// insert mode.
fn send_click(browser: &mut Browser, x: i32, y: i32) {
    let Some(host) = browser.host() else {
        return;
    };
    let event = MouseEvent { x, y, modifiers: 0 };
    host.send_mouse_move_event(Some(&event), 0);
    host.send_mouse_click_event(Some(&event), MouseButtonType::LEFT, 0, 1);
    host.send_mouse_click_event(Some(&event), MouseButtonType::LEFT, 1, 1);
    after_click(browser);
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
///
/// `from_click` is the answer to [`ASK`]: bru sent a click and this says what it landed on. It is
/// **the only thing that enters insert mode on a web page**, and the module header has the Escape
/// that made it so. Note what it is not: it is not read off the document, it is which of the two
/// code paths built the message, so no page can produce it and no keystroke can fake it.
pub fn decide(
    rules: Rules,
    editable: bool,
    from_click: bool,
    gesture: bool,
    keyboard: bool,
    trusted: bool,
) -> Verdict {
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
    if from_click {
        // A hint followed onto an input, or `:click` — both go through a real
        // `send_mouse_click_event` and then ask what it landed on. This is qutebrowser's
        // `_mousepress_insertmode_cb` and it is the whole of `auto_enter` now.
        return if rules.auto_enter {
            Verdict::Enter
        } else {
            Verdict::Nothing
        };
    }
    // Nobody clicked — the focus simply moved. `auto_load` is false by default, and that is the
    // whole of the fix: the start page's search box no longer holds the next key the user presses.
    //
    // **Tab and a real mouse click land here too, and are refused with the page.** They used to be
    // told apart by `gesture` and to enter insert; the module header has the Escape that cost, and
    // the user's answer to it on 2026-08-09. `gesture` is not consulted in this direction at all
    // any more, which is why it now reaches this function only for `auto_leave` above.
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

    let (editable, gesture, keyboard, trusted, from_click) = match message.argument_list() {
        Some(arguments) => (
            arguments.bool(0) != 0,
            arguments.bool(1) != 0,
            arguments.bool(2) != 0,
            arguments.bool(3) != 0,
            arguments.bool(4) != 0,
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
        decide(Rules::live(), editable, from_click, gesture, keyboard, trusted)
    } else {
        Verdict::Nothing
    };

    if std::env::var_os("BRU_DEBUG_FOCUS").is_some() {
        eprintln!(
            "bru[focus]: browser {id} window {window} editable={editable} from_click={from_click} \
             gesture={gesture} keyboard={keyboard} trusted={trusted} active={active} \
             -> {verdict:?}",
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
    /// The fifth bit: which path filled the report in. `CLICKED` is the answer to [`ASK`], which
    /// only ever follows a `send_mouse_click_event` bru sent; `MOVED` is a focus change, whoever
    /// caused it.
    const CLICKED: bool = true;
    const MOVED: bool = false;

    /// The bug, as a table. **This is the third of the three focus changes
    /// `https://start.duckduckgo.com/` fires**, the one its own script does after the load has
    /// finished — the one a rule built on the loading state gets wrong. Measured 2026-08-07:
    /// `editable=true gesture=false readyState=complete`.
    #[test]
    fn a_page_focusing_its_own_search_box_does_not_enter_insert() {
        let rules = shipped();
        assert_eq!(decide(rules, true, MOVED, PAGE, TAB, WEB), Verdict::Nothing);
        // With auto_load on it does — which is what the setting is for, and it is the switch that
        // puts the old behaviour back for anyone who wants it.
        let opted_in = Rules { auto_load: true, ..rules };
        assert_eq!(decide(opted_in, true, MOVED, PAGE, TAB, WEB), Verdict::Enter);
    }

    /// The half that must not regress: following a hint onto a search box and typing. The click is
    /// a real `send_mouse_click_event` and [`ASK`] reports what it landed on.
    #[test]
    fn a_hint_followed_onto_a_field_enters_insert() {
        let rules = shipped();
        assert_eq!(decide(rules, true, CLICKED, PERSON, MOUSE, WEB), Verdict::Enter);
        let opted_out = Rules { auto_enter: false, ..rules };
        assert_eq!(decide(opted_out, true, CLICKED, PERSON, MOUSE, WEB), Verdict::Nothing);
    }

    /// **The Escape that changed this rule.** Reported by the user 2026-08-09 on
    /// `https://accounts.google.com/`: Escape left insert mode and the page put it straight back,
    /// so it took two presses. `BRU_DEBUG_KEYS=1` proved the key was never lost — `pending=27` says
    /// bru swallowed the press and ran `mode-leave` — and the line after it every time was:
    ///
    /// ```text
    /// bru[focus]: editable=true gesture=true keyboard=false -> Enter
    /// ```
    ///
    /// Pressing Escape renews `navigator.userActivation`, so the page's own refocus reads as a
    /// person clicking into a field. **This case and the one above are indistinguishable by every
    /// bit except the path**, which is exactly why the path is now what is asked. Give this the
    /// old rule — enter whenever `gesture` — and it turns back into `Enter` and Escape needs two
    /// presses again.
    #[test]
    fn a_page_taking_the_focus_back_after_escape_does_not_re_enter_insert() {
        let rules = shipped();
        assert_eq!(decide(rules, true, MOVED, PERSON, MOUSE, WEB), Verdict::Nothing);
        // Both spellings of it: the same refocus reported the other way round on the keyboard bit.
        assert_eq!(decide(rules, true, MOVED, PERSON, TAB, WEB), Verdict::Nothing);
    }

    /// What the user was told this costs when they chose it, 2026-08-09, written down as a test so
    /// that it is a decision and not a regression somebody quietly restores. Neither Tab nor a real
    /// mouse click enters insert mode; `i` does. qutebrowser agrees about Tab and differs about the
    /// mouse, and it can only differ because it hit-tests the element under the cursor.
    #[test]
    fn tab_and_a_real_mouse_click_no_longer_enter_insert() {
        let rules = shipped();
        assert_eq!(decide(rules, true, MOVED, PERSON, TAB, WEB), Verdict::Nothing);
        assert_eq!(decide(rules, true, MOVED, PERSON, MOUSE, WEB), Verdict::Nothing);
        // And `auto_load` is still the switch that puts automatic insert back for anyone who
        // wants it, in the one direction it ever governed.
        let opted_in = Rules { auto_load: true, ..rules };
        assert_eq!(decide(opted_in, true, MOVED, PERSON, TAB, WEB), Verdict::Enter);
    }

    /// `bru://chrome/cookies` focuses its filter box on load and its author documented that typing a
    /// domain is the first thing that happens. bru's own pages are not web pages, and a caret
    /// blinking under a bar that says NORMAL is the contradiction this avoids.
    #[test]
    fn brus_own_pages_may_focus_their_own_field() {
        let rules = shipped();
        assert_eq!(decide(rules, true, MOVED, PAGE, TAB, CHROME), Verdict::Enter);
        // The exception is over `auto_load` only. Someone who turned `auto_enter` off wants no
        // automatic insert mode anywhere, and a chrome page does not overrule that.
        let opted_out = Rules { auto_enter: false, ..rules };
        assert_eq!(decide(opted_out, true, CLICKED, PERSON, MOUSE, CHROME), Verdict::Nothing);

        assert!(is_bru_url("bru://chrome/cookies"));
        assert!(!is_bru_url("https://start.duckduckgo.com/"));
        // Not "contains": a page may put bru:// in its own query string.
        assert!(!is_bru_url("https://example.com/?x=bru://chrome/cookies"));
    }

    /// `auto_leave`, and it is qutebrowser's exactly: a **mouse press** on a non-editable element,
    /// `eventfilter.py:196-215`. The two neighbouring cases are the ones that measured wrong when
    /// this was written wider.
    ///
    /// **Leaving still reads `gesture`, and deliberately.** Erring towards normal mode costs a
    /// keystroke; erring towards insert cost the Escape above.
    #[test]
    fn clicking_something_that_is_not_editable_leaves_insert() {
        let rules = shipped();
        assert_eq!(decide(rules, false, MOVED, PERSON, MOUSE, WEB), Verdict::Leave);
        // And bru's own click onto something that is not editable, which is `;h` and a hint onto a
        // button — the same verdict by the other path.
        assert_eq!(decide(rules, false, CLICKED, PERSON, MOUSE, WEB), Verdict::Leave);

        // **Tab must not.** `bru://chrome/cookies` says "Tab reaches the two buttons"; leaving
        // insert there makes Enter on the focused button bru's key and the button unpressable.
        // Measured 2026-08-07 on that page: `editable=false gesture=true :focus-visible=1`.
        assert_eq!(decide(rules, false, MOVED, PERSON, TAB, WEB), Verdict::Nothing);
        assert_eq!(decide(rules, false, MOVED, PERSON, TAB, CHROME), Verdict::Nothing);

        // **A page blurring its own field must not either.** It is the second of duckduckgo's
        // three focus changes, and a page does not get to move bru's mode in either direction.
        assert_eq!(decide(rules, false, MOVED, PAGE, MOUSE, WEB), Verdict::Nothing);

        let opted_out = Rules { auto_leave: false, ..rules };
        assert_eq!(decide(opted_out, false, MOVED, PERSON, MOUSE, WEB), Verdict::Nothing);
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
