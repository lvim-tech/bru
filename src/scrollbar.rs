//! The scrollbar on the right of a page, drawn in bru's theme instead of Chromium's.
//!
//! Chromium's own is a grey thumb on a grey track with a **stepper arrow at each end**, drawn by
//! `NativeThemeAura` and owned by the compositor rather than by the page. Next to a bru whose every
//! other surface comes out of one generated `theme.css`, it is the one piece of interface that
//! belongs to something else.
//!
//! ## The three ways to move it, measured 2026-08-07
//!
//! All three work; only one does what was asked. Screenshots were taken with `:screenshot` and the
//! right-hand 30×120 px cropped and scaled 6×, because the difference is four pixels wide and a
//! whole-screen comparison cannot see it.
//!
//! | | arrows | colours | always visible |
//! |---|---|---|---|
//! | `scrollbar-color: <thumb> <track>` on `:root` | **stay** (recoloured) | yes | yes |
//! | `--enable-features=OverlayScrollbar` | gone | no | **no** |
//! | `::-webkit-scrollbar*` | **gone** | yes | yes |
//!
//! `scrollbar-color` is the standard property and it does apply — `getComputedStyle` answered
//! `rgb(167, 192, 128) rgb(43, 51, 57)` for a green-on-dark pair, and the thumb came out green. It
//! has no say over the stepper arrows at all, so the arrow simply turned green with the rest.
//!
//! `OverlayScrollbar` removes the arrows by removing the scrollbar: a thin bar that fades in while
//! the page is moving and is not there otherwise. That is a change to what the scrollbar *is*, not
//! to how it looks, and nothing here asked for it.
//!
//! So it is the `::-webkit-scrollbar` pseudo-elements, and the one that does the work is
//! `::-webkit-scrollbar-button { display: none }`. **Naming any `::-webkit-scrollbar` rule at all
//! switches Chromium out of the native scrollbar and into a CSS-drawn one**, which is why the same
//! block has to name the track and the thumb too: a custom scrollbar with no `background` declared
//! is drawn transparent, not drawn native.
//!
//! No `border-radius`. The thumb reaches the top and bottom edges square, which is what the strips
//! bru already draws do — `chrome.css` has carried exactly these five rules for the completion and
//! the prompt since they were written, with a width and two colours and nothing else.
//!
//! ## Why the page wins, and why that is a setting
//!
//! GitHub, Discord and Reddit style their own scrollbars, and these are pseudo-elements of the same
//! element: either bru's rules win everywhere or the page's do, and there is no useful middle. The
//! choice is [`crate::settings`]'s `scrollbar.page_overrides`, **true** by default, and the whole
//! mechanism is document order — the `<style>` goes in as the **first child of `<html>`**, before
//! anything the page's own `<head>` will hold, with no `!important` anywhere. A page rule of equal
//! specificity therefore comes later and wins, which is what the default means. Setting it false
//! adds `!important` to every declaration and bru wins instead.
//!
//! **One limit no CSS can get past.** `scrollbar-color` and `::-webkit-scrollbar` are mutually
//! exclusive in Chromium: a page that sets `scrollbar-color` on `:root` makes the whole block below
//! inert, and its arrows come back. That is true whichever way `scrollbar.page_overrides` is set,
//! because `!important` decides which of two competing declarations applies and not which of two
//! scrollbar *mechanisms* Chromium chooses.
//!
//! ## Where it goes in, and where it must not
//!
//! `RenderProcessHandler::on_context_created`, in the **renderer**, through the keeper in
//! `chrome/userstyle.js` — the same door and the same keeper `userstyles.rs` uses.
//!
//! `LoadHandler::on_load_start` was where this started, and it is subtly wrong in a way a
//! screenshot found and no test would have: a browser that has only just been created **has no V8
//! context**, so the script for the first page a window shows was handed over and evaporated. The
//! start page was the one page in bru with Chromium's scrollbar on it.
//!
//! The objection that sent it to `on_load_start` in the first place was real and is answered
//! elsewhere now. Measured 2026-08-07: a userscript that did nothing but append a `<style>` at
//! context creation threw `TypeError: Cannot read properties of null (reading 'appendChild')`,
//! because at that moment `document.head` and `document.documentElement` are **both null**. That is
//! the keeper's first job — it waits for the root with a `MutationObserver` instead of assuming
//! one. Two more failures come free with it: a single-page application that re-renders takes any
//! `<style>` put into it with it, and the element has to be kept at the end of the cascade it was
//! asked for. `bruKeep(id, "first")` is what this module asks for; see the file.
//!
//! bru's own `bru://chrome/*` pages are **not** served by this. They link `chrome.css`, which
//! carries the same five rules as ordinary CSS, so the help page and the settings page get the
//! scrollbar for the same reason the completion does and without a script running on them.

use cef::*;
use std::sync::Mutex;

/// The width and the two colours, as [`css_with`] takes them.
///
/// A struct rather than three parameters because the three come from the same place and go to the
/// same place, and because `css_with(theme, true, 12, None, None)` says nothing at the call site.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Look {
    /// `scrollbar.width`, in CSS pixels.
    pub width: u32,
    /// `scrollbar.thumb`, or `None` for whatever the theme says.
    pub thumb: Option<String>,
    /// `scrollbar.track`, or `None` for whatever the theme says.
    pub track: Option<String>,
}

impl Look {
    /// The four settings as they stand. Not `Default`, which is what a store-less process gets.
    fn in_force() -> Self {
        Look {
            width: crate::settings::int_of("scrollbar.width").clamp(1, 64) as u32,
            thumb: crate::settings::text_of("scrollbar.thumb"),
            track: crate::settings::text_of("scrollbar.track"),
        }
    }
}

/// The theme's value for one custom property, following `var(--other)` until it reaches a colour.
///
/// `theme.css` defines the scrollbar's two colours as references — `--completion-scrollbar-bg:
/// var(--bg)` — so reading the property is not enough; something has to resolve them. A browser does
/// this for `chrome.css` and there is no browser here: this string is going into a *page*, which
/// never loads `theme.css` and has its own meaning for `--bg` if it has one at all.
///
/// The walk is bounded at eight hops, which is a cycle guard and not a depth: `themer` generates
/// this file and nothing stops a hand-edited one from pointing two properties at each other.
pub(crate) fn resolve<'a>(theme: &'a str, name: &str) -> Option<&'a str> {
    let mut name = name.to_string();
    for _ in 0..8 {
        let value = declared(theme, &name)?;
        match value.strip_prefix("var(").and_then(|rest| rest.strip_suffix(')')) {
            Some(inner) => name = inner.trim().to_string(),
            None => return Some(value),
        }
    }
    None
}

/// The right-hand side of `--name: value;` in the theme, untrimmed of nothing but space.
///
/// Deliberately not a CSS parser. `theme.css` is generated, one declaration per line, and
/// `chrome.rs`'s own test asserts it "carries not one rule" — so a line that starts with the
/// property name and holds a colon is the declaration, and there is no selector nesting to get lost
/// in. A theme that stopped being generated would be the thing to fix, not this.
fn declared<'a>(theme: &'a str, name: &str) -> Option<&'a str> {
    theme.lines().find_map(|line| {
        let line = line.trim();
        let rest = line.strip_prefix(name)?;
        let rest = rest.trim_start().strip_prefix(':')?;
        Some(rest.trim().trim_end_matches(';').trim())
    })
}

/// The five rules, against one theme, with the cascade pointed the way the setting says.
///
/// `page_wins` is `scrollbar.page_overrides`. It is a parameter rather than a read of the store so
/// that the tests can put it either way in a process that has no store — the same shape
/// `popups::decide_with` uses for `tabs.background`.
pub fn css_with(theme: &str, page_wins: bool, look: &Look) -> String {
    // Unset falls back to the two colours the completion's own scrollbar already uses, so that every
    // scrollbar in front of this user is one decision in one file until somebody says otherwise. The
    // last fallbacks are everforest's, and they are reached only by a theme.css that has lost a
    // property — a wrong colour is recoverable and a scrollbar drawn transparent because
    // `background` came out empty is not.
    //
    // **Each source is filtered, and a value that fails falls through to the next.** These two
    // strings are interpolated into a rule and then into a JS string literal (`json_escape` below),
    // and a value carrying a `}` or a `;` closes the rule it is in and writes its own. `hints.rs`
    // filtered its label colours from the first day and this did not, from the same settings and the
    // same `theme.css` — see `chrome::is_safe_colour`. Falling through rather than refusing is what
    // `label_style_json_from` does: a rejected colour leaves a scrollbar the wrong shade, and the
    // alternative is one drawn transparent.
    let safe = |value: &&str| crate::chrome::is_safe_colour(value);
    let track = look
        .track
        .as_deref()
        .filter(safe)
        .or_else(|| resolve(theme, "--completion-scrollbar-bg").filter(safe))
        .unwrap_or("#232929");
    let thumb = look
        .thumb
        .as_deref()
        .filter(safe)
        .or_else(|| resolve(theme, "--completion-scrollbar-fg").filter(safe))
        .unwrap_or("#849380");
    let bang = if page_wins { "" } else { " !important" };
    let w = if look.width == 0 { 12 } else { look.width };
    // **`border-radius:0` is declared rather than left out**, and it is the one rule here that was
    // added after a measurement rather than before. With it absent, `scrollbar.page_overrides false`
    // took a page's colour and its width and left its `border-radius:10px` standing — because
    // `!important` beats a competing declaration and there was no competing declaration to beat.
    // The thumb came out bru's green with the page's rounded cap, which is neither answer.
    format!(
        "::-webkit-scrollbar{{width:{w}px{bang};height:{w}px{bang}}}\
         ::-webkit-scrollbar-button{{display:none{bang}}}\
         ::-webkit-scrollbar-track{{background:{track}{bang};border-radius:0{bang}}}\
         ::-webkit-scrollbar-thumb{{background:{thumb}{bang};border-radius:0{bang}}}\
         ::-webkit-scrollbar-corner{{background:{track}{bang}}}"
    )
}

/// The `<style>` element's id. The keeper takes it as a name, so that the scrollbar's element and
/// the per-site stylesheet's are two elements and not one fought over.
const STYLE_ID: &str = "bru-scrollbar";

/// The keeper that lives in the page — the same one `userstyles.rs` installs. See
/// `chrome/userstyle.js`.
const KEEPER_JS: &str = include_str!("../chrome/userstyle.js");

/// [`css_with`] against the theme and the settings actually in force, or nothing at all when
/// `scrollbar.style` is off.
///
/// The theme is re-read per call rather than cached: `~/.config/bru/theme.css` is what `themer`
/// rewrites to change the colours, and `chrome.rs` already re-reads it on every request for the same
/// reason. A page load is not a hot path.
///
/// **It answers correctly in the renderer as well, and that is not luck.** `settings::get` falls
/// back to the compiled-in `Def::default_value` when nothing has been stored, and nothing is ever
/// stored in a renderer — so this reads bru's shipped defaults there and the user's values in the
/// browser process, from one body of code. What it must not do is grow a reader that needs Lua: a
/// `Value::Fn` lives in the browser process's `mlua` state and there is no such state here. None of
/// these four settings can hold a function.
fn rules() -> String {
    if !crate::settings::is_on("scrollbar.style") {
        return String::new();
    }
    let theme = String::from_utf8_lossy(&crate::chrome::theme_css()).into_owned();
    css_with(&theme, crate::settings::is_on("scrollbar.page_overrides"), &Look::in_force())
}

// --- the renderer -----------------------------------------------------------------------------
/// The rules the browser process last sent, or `None` if it has not spoken yet.
///
/// **The renderer has no settings store, and it needs four of them.** So the split is between what
/// is true at startup and what a person has typed:
///
/// - Nothing pushed yet: [`rules`] runs here and answers with bru's compiled-in defaults against
///   the theme off disk. That is exactly right for a bru nobody has configured, and it is what a
///   renderer has before anybody has said anything.
/// - `Backing::Scrollbar` sends the real rules over when a setting moves, and they are kept here
///   until the next one.
static PUSHED: Mutex<Option<String>> = Mutex::new(None);

/// The process message the browser sends when a `scrollbar.*` setting moves.
pub const SET_RULES: &str = "bru.scrollbar.rules";

/// The process message a renderer sends when it has never been told anything.
///
/// **A renderer that was never told used bru's compiled-in defaults and kept them.** Measured
/// 2026-08-07, three ways: `:set scrollbar.style false` emptied the page under the cursor, and then
/// the next site opened — a new renderer process — drew the scrollbar again, 251 characters of it,
/// as did a new tab. `push_rules` reaches the renderers that exist; nothing reached the ones made
/// afterwards, and a cross-site navigation makes one.
///
/// So the renderer asks. It applies what it has straight away — the defaults, which is the right
/// answer for a bru nobody has configured — and the reply corrects it if a person has said
/// otherwise. Asking costs one message per renderer process, once.
pub const ASK: &str = "bru.scrollbar.ask";

/// Send the rules to every renderer. The browser process's side, called by `Backing::Scrollbar`.
///
/// **No reload.** The rules used to be followed by `browser.reload()`, because they were applied at
/// context creation and a document that already existed had had its. The keeper in the page takes
/// CSS at any time, so the renderer re-applies on the message instead — and a `:set` that reloads
/// every tab is a `:set` that loses whatever was typed into a form on any of them.
pub fn push_rules() {
    debug_assert_ne!(currently_on(ThreadId::UI), 0);
    let rules = rules();
    let Some(state) = crate::state::BruState::instance() else {
        return;
    };
    let views = {
        let Ok(guard) = state.lock() else {
            return;
        };
        guard
            .window_ids()
            .into_iter()
            .flat_map(|window| guard.tab_views_in(window))
            .collect::<Vec<_>>()
    };
    for view in views {
        let Some(frame) = view.browser().and_then(|browser| browser.main_frame()) else {
            continue;
        };
        send_rules(&frame, &rules);
    }
}

/// Answer one renderer's [`ASK`]. The browser process's side, called from `ipc.rs`.
///
/// Returns whether it was that message.
pub fn on_ask(frame: Option<&Frame>, message: Option<&ProcessMessage>) -> bool {
    let Some(message) = message else {
        return false;
    };
    if CefString::from(&message.name()).to_string() != ASK {
        return false;
    }
    let Some(frame) = frame else {
        return true;
    };
    send_rules(frame, &rules());
    true
}

/// One `SET_RULES` message, to one frame. The only place that message is built.
fn send_rules(frame: &Frame, rules: &str) {
    let Some(mut message) = process_message_create(Some(&CefString::from(SET_RULES))) else {
        return;
    };
    if let Some(arguments) = message.argument_list() {
        arguments.set_string(0, Some(&CefString::from(rules)));
    }
    frame.send_process_message(ProcessId::RENDERER, Some(&mut message));
}

/// The renderer's side of that message. Answers whether it was that message.
///
/// **It applies as well as remembers**, which is what lets `push_rules` drop its reload: the keeper
/// already in the page is handed the new CSS, so the document under the user changes rather than
/// the next one they open.
pub fn renderer_on_message(frame: Option<&Frame>, message: Option<&ProcessMessage>) -> bool {
    let Some(message) = message else {
        return false;
    };
    if CefString::from(&message.name()).to_string() != SET_RULES {
        return false;
    }
    let css = message
        .argument_list()
        .map(|arguments| CefString::from(&arguments.string(0)).to_string())
        .unwrap_or_default();
    *PUSHED.lock().expect("the scrollbar rules mutex is never poisoned") = Some(css.clone());
    if let Some(frame) = frame {
        if styleable(frame) {
            crate::greasemonkey::evaluate(frame, &keeper_call(&css), None);
        }
    }
    true
}

/// Whether this frame is a page bru dresses at all: the main frame of a real document.
///
/// `bru://` chrome links `chrome.css`, which has carried these five rules for the completion since
/// they were written; `about:blank` and `data:` are not a site. A subframe's scrollbar belongs to
/// the advert in it.
fn styleable(frame: &Frame) -> bool {
    if frame.is_main() == 0 {
        return false;
    }
    let url = CefString::from(&frame.url()).to_string();
    !url.starts_with("bru://") && url.contains("://")
}

/// What one document is told: hand the keeper the rules, or take the element away when there are
/// none.
///
/// **`ipc::json_escape` escapes the *contents* of a string literal and does not write the quotes
/// around it.** Left off, the CSS arrived as bare source — `set(::-webkit-scrollbar{…})` — which is
/// a syntax error, so the whole script did nothing and did it silently. The quotes below are the
/// fix and [`tests::a_quote_in_the_theme_cannot_close_the_string`] is what holds them there.
///
/// Empty is [`off`](chrome/userstyle.js) rather than `set("")`, and the difference is real: `set`
/// with nothing in it leaves an empty `<style>` in the page with a `MutationObserver` still holding
/// it in place. `:set scrollbar.style false` means the element goes.
fn keeper_call(css: &str) -> String {
    let keeper = format!("window.bruKeep && window.bruKeep(\"{STYLE_ID}\",\"first\")");
    if css.is_empty() {
        return format!("{keeper}.off();");
    }
    format!("{keeper}.set(\"{}\");", crate::ipc::json_escape(css))
}

/// Draw the scrollbar in this document. The renderer's hook.
pub fn renderer_on_context_created(frame: Option<&Frame>) {
    let Some(frame) = frame else {
        return;
    };
    if !styleable(frame) {
        return;
    }
    // Nothing pushed means this renderer has never been told anything. Draw bru's own defaults —
    // [`rules`] answers with them in a process that has no store, which is what a renderer is — and
    // ask, so that a setting somebody changed before this process existed reaches it. See [`ASK`].
    let known = PUSHED.lock().ok().and_then(|pushed| pushed.clone());
    if known.is_none() {
        if let Some(mut message) = process_message_create(Some(&CefString::from(ASK))) {
            frame.send_process_message(ProcessId::BROWSER, Some(&mut message));
        }
    }
    let css = known.unwrap_or_else(rules);
    // **Through the same keeper the per-site stylesheets use, and `first` is the whole difference.**
    // A document that is re-rendered takes any style put into it with it — measured on DuckDuckGo,
    // whose results arrive after the style did — so the element has to be watched rather than
    // inserted once. `first` puts it before everything the page brings, which is the whole of
    // `scrollbar.page_overrides true`; `userstyles.rs` asks for `last`, where the user is meant to
    // win. Both keepers are installed by the same file and cost one parse between them.
    crate::greasemonkey::evaluate(frame, KEEPER_JS, Some("bru://userstyle.js"));
    crate::greasemonkey::evaluate(frame, &keeper_call(&css), None);
}
// --- end the renderer -------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// The four settings at their shipped defaults, which is what a process with no store answers
    /// with — `Look::in_force` cannot run under `cargo test` (CEF-NOTES trap 13).
    fn shipped() -> Look {
        Look { width: 12, thumb: None, track: None }
    }

    /// The theme bru ships, which is what these tests resolve against.
    fn theme() -> String {
        String::from_utf8_lossy(&crate::chrome::theme_css()).into_owned()
    }

    /// **The one rule that removes the arrows**, and the reason the module exists.
    ///
    /// Measured before it was written: with `scrollbar-color` alone the stepper arrow was still
    /// drawn, recoloured. Deleting this declaration is the break that brings it back, and nothing
    /// else in the block does.
    #[test]
    fn the_stepper_arrow_is_switched_off() {
        assert!(css_with(&theme(), true, &shipped()).contains("::-webkit-scrollbar-button{display:none"));
    }

    /// **The thumb is square**, asked for outright, and it is what the completion's already is.
    ///
    /// Declared as `border-radius:0` rather than left out: measured 2026-08-07, a page whose own
    /// thumb carries `border-radius:10px` kept its rounded cap under `page_overrides false` while
    /// bru's colour and width won, because there was no declaration of bru's for the `!important`
    /// to attach to. So the assertion is that the only radius named is zero, not that none is.
    #[test]
    fn nothing_is_rounded() {
        let css = css_with(&theme(), true, &shipped());
        assert_eq!(css.matches("border-radius:0").count(), 2, "{css}");
        assert!(!css.contains("border-radius:1"), "{css}");
    }

    /// **A colour that would close the rule never reaches the page.**
    ///
    /// Both spellings of the hole: a `scrollbar.thumb` setting, and a `theme.css` property. Neither
    /// is a page's to write, so what this stops is malformed CSS rather than script — but `hints.rs`
    /// filtered the same values from the same two files and this did not, and one of the two was
    /// wrong. The rejected value falls through to the shipped colour, so the scrollbar still draws.
    #[test]
    fn a_colour_that_would_break_out_of_the_rule_is_refused_and_not_interpolated() {
        let look = Look {
            width: 12,
            thumb: Some("red}*{display:none".to_string()),
            track: Some("#111;background-image:url(x)".to_string()),
        };
        let css = css_with(&theme(), true, &look);
        // `display:none` is in this stylesheet legitimately — it is what switches the stepper arrow
        // off — so what is asserted is the *selector* the injected value brought with it.
        assert!(!css.contains("*{display:none"), "a thumb colour wrote its own rule: {css}");
        assert!(!css.contains("url("), "a track colour wrote its own declaration: {css}");
        assert!(css.contains("background:#849380"), "the thumb lost its fallback: {css}");
        assert!(css.contains("background:#232929"), "the track lost its fallback: {css}");

        // A theme that carries the same thing, for the source the setting is not.
        let poisoned = format!("{}\n--completion-scrollbar-fg: yellow}}*{{display:none;\n", theme());
        let css = css_with(&poisoned, true, &shipped());
        assert!(!css.contains("*{display:none"), "a theme property wrote its own rule: {css}");
        assert!(!css.contains("yellow"), "the refused value was interpolated anyway: {css}");

        // And an ordinary value still goes through, so the guard did not swallow the feature.
        let look = Look { width: 12, thumb: Some("rgba(255, 247, 133, 0.9)".to_string()), track: None };
        assert!(css_with(&theme(), true, &look).contains("background:rgba(255, 247, 133, 0.9)"));
    }

    /// The colours come out of the theme and are real colours, not the `var(--bg)` the file writes.
    ///
    /// A page never loads `theme.css`, so a `var()` left in here would resolve against the *page's*
    /// custom properties — which is either nothing, and the scrollbar is drawn transparent, or the
    /// page's own `--bg`, which is worse because it would look deliberate.
    #[test]
    fn the_colours_are_resolved_rather_than_referenced() {
        let css = css_with(&theme(), true, &shipped());
        assert!(!css.contains("var("), "a var() reached the page: {css}");
        assert!(css.contains("background:#"), "no literal colour in {css}");
    }

    /// `--completion-scrollbar-bg` is `var(--bg)` in the shipped theme, which is the indirection
    /// [`resolve`] exists for. If themer ever writes the colour there directly this still passes;
    /// what it guards is the hop existing at all.
    #[test]
    fn a_var_chain_is_followed_to_a_colour() {
        let theme = ":root{\n--a: var(--b);\n--b: var(--c);\n--c: #010203;\n}";
        assert_eq!(resolve(theme, "--a"), Some("#010203"));
        // Bounded, so a theme that points two properties at each other does not hang the browser.
        let cycle = ":root{\n--a: var(--b);\n--b: var(--a);\n}";
        assert_eq!(resolve(cycle, "--a"), None);
    }

    /// **Which way the cascade points is the setting**, and `!important` is the whole of it.
    #[test]
    fn the_page_wins_by_default_and_bru_wins_when_told_to() {
        assert!(!css_with(&theme(), true, &shipped()).contains("!important"));
        let ours = css_with(&theme(), false, &shipped());
        // Every declaration, not merely one: a block where the colours are `!important` and
        // `display:none` is not would let a page put its arrows back while keeping bru's colours.
        // Eight — two widths, the button, two backgrounds, two radii, the corner.
        assert_eq!(ours.matches("!important").count(), 8, "{ours}");
    }

    /// **The three that make it configurable**, each overriding a different fallback.
    ///
    /// `width` is the number, and the two colours replace the theme's rather than being appended to
    /// it — a scrollbar cannot have two backgrounds, so an unset colour is the theme's and a set one
    /// is the user's, with nothing in between.
    #[test]
    fn the_width_and_the_colours_come_from_the_settings() {
        let look = Look {
            width: 30,
            thumb: Some("rebeccapurple".to_string()),
            track: Some("transparent".to_string()),
        };
        let css = css_with(&theme(), true, &look);
        assert!(css.contains("width:30px"), "{css}");
        assert!(css.contains("background:rebeccapurple"), "{css}");
        assert!(css.contains("background:transparent"), "{css}");
        // The theme's own colour is gone rather than sitting behind the override as a second
        // declaration the cascade would have to settle.
        let theme = theme();
        let themed = css_with(&theme, true, &shipped());
        let thumb = resolve(&theme, "--completion-scrollbar-fg").expect("the theme has one");
        assert!(themed.contains(thumb), "{themed}");
        assert!(!css.contains(thumb), "the theme's colour survived an override: {css}");
    }

    /// Off is not "hand over an empty stylesheet" — it is "take out what a previous run left".
    ///
    /// `set("")` would leave an empty `<style>` in the page with the keeper's observer still holding
    /// it there, which is a scrollbar setting switched off and a mutation observer still running on
    /// every page in the browser.
    #[test]
    fn turning_it_off_takes_the_element_away_rather_than_emptying_it() {
        let off = keeper_call("");
        assert!(off.contains(".off()"), "{off}");
        assert!(!off.contains(".set("), "{off}");
    }

    /// The stylesheet goes in **first**, which is what makes the default mean anything: a page rule
    /// of equal specificity beats ours by coming later, and only document order decides that.
    ///
    /// The insertion itself is `chrome/userstyle.js`'s, so what this holds is the argument that asks
    /// for it — the other keeper in the same file is handed `"last"` and must stay handed it.
    #[test]
    fn the_stylesheet_is_asked_for_at_the_front_of_the_cascade() {
        let code = keeper_call("::-webkit-scrollbar{width:12px}");
        assert!(code.contains("\"first\""), "{code}");
        assert!(!code.contains("\"last\""), "{code}");
        assert!(KEEPER_JS.contains("insertBefore"), "the keeper cannot honour \"first\"");
    }

    /// The two keepers own two elements. One id for both would be the scrollbar and the user's CSS
    /// overwriting each other, last writer winning, and on most pages the user's runs second.
    #[test]
    fn the_scrollbar_and_the_user_styles_are_not_the_same_element() {
        assert_ne!(STYLE_ID, "bru-userstyle");
        assert!(keeper_call("x").contains(STYLE_ID));
    }

    /// The CSS reaches the page as a JS string literal, so a quote or a backslash in it must not end
    /// that literal. **Two independent guards, and this is the second one.**
    ///
    /// A quote can no longer get this far through a colour — `is_safe_colour` refuses it at
    /// `css_with` and the test above pins that — so the escaping is asserted against `keeper_call`
    /// directly, which is the function that owns it. Written this way round on purpose: the colour
    /// filter is a check on one source, and if a later rule interpolates something this filter never
    /// sees, the string it lands in still has to hold.
    #[test]
    fn a_quote_in_the_css_cannot_close_the_string() {
        let code = keeper_call("::-webkit-scrollbar{background:#fff\"; } body { display:none}");
        // The quote must arrive backslashed. Asserting the *absence* of `"; } body` would pass on a
        // script that never quoted the CSS at all, which is the bug this was written after.
        assert!(code.contains("#fff\\\"; } body"), "the quote was not escaped: {code}");
        assert!(code.contains(".set(\""), "the CSS is not inside a string at all: {code}");

        // And the colour filter is the first guard: the same quote, arriving as a theme's colour,
        // never reaches the CSS to need escaping.
        let theme = ":root{\n--completion-scrollbar-bg: #fff\"; } body { display:none;\n}";
        assert!(!css_with(theme, true, &shipped()).contains('"'));
    }
}
