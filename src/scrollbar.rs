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
//! `LoadHandler::on_load_start`, through `Frame::execute_java_script`. Not
//! `RenderProcessHandler::on_context_created`, which is where `greasemonkey.rs` injects: measured
//! 2026-08-07 with a userscript that did nothing but append a `<style>`, the script ran and threw
//! `TypeError: Cannot read properties of null (reading 'appendChild')` — at that moment
//! `document.head` and `document.documentElement` are **both null**, so there is no document to put
//! a stylesheet into. `on_load_start` is after the document element exists and before the page's own
//! `<head>` has been parsed, which is both halves of what this needs.
//!
//! bru's own `bru://chrome/*` pages are **not** served by this. They link `chrome.css`, which
//! carries the same five rules as ordinary CSS, so the help page and the settings page get the
//! scrollbar for the same reason the completion does and without a script running on them.

use cef::*;

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
    let track = look
        .track
        .as_deref()
        .or_else(|| resolve(theme, "--completion-scrollbar-bg"))
        .unwrap_or("#232929");
    let thumb = look
        .thumb
        .as_deref()
        .or_else(|| resolve(theme, "--completion-scrollbar-fg"))
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

/// The `<style>` element's id, so that a second injection replaces the first rather than stacking.
///
/// `:set scrollbar.style false` needs something to take *out*, and a page that has been through
/// three `:set`s must not be carrying three stylesheets.
const STYLE_ID: &str = "bru-scrollbar";

/// The script one frame is handed: remove bru's stylesheet if it is there, then put the current one
/// in, unless there is nothing to put.
///
/// Written as an IIFE with no bindings that escape it, because it runs in the page's own world
/// alongside whatever the page has declared.
///
/// **`ipc::json_escape` escapes the *contents* of a string literal and does not write the quotes
/// around it.** Left off, the CSS arrived as bare source — `var c=::-webkit-scrollbar{…}` — which is
/// a syntax error, so the whole script did nothing and did it silently. The quotes below are the
/// fix and [`tests::a_quote_in_the_theme_cannot_close_the_string`] is what holds them there.
pub fn script_with(theme: &str, on: bool, page_wins: bool, look: &Look) -> String {
    let css = if on { css_with(theme, page_wins, look) } else { String::new() };
    format!(
        "(function(){{var d=document,o=d.getElementById(\"{id}\");if(o)o.remove();\
         var c=\"{css}\";if(!c)return;var r=d.documentElement;if(!r)return;\
         var s=d.createElement('style');s.id=\"{id}\";s.textContent=c;\
         r.insertBefore(s,r.firstChild);}})()",
        id = crate::ipc::json_escape(STYLE_ID),
        css = crate::ipc::json_escape(&css),
    )
}

/// [`script_with`] against the theme and the settings actually in force.
///
/// The theme is re-read per call rather than cached: `~/.config/bru/theme.css` is what `themer`
/// rewrites to change the colours, and `chrome.rs` already re-reads it on every request for the same
/// reason. A page load is not a hot path.
pub fn script() -> String {
    let theme = String::from_utf8_lossy(&crate::chrome::theme_css()).into_owned();
    script_with(
        &theme,
        crate::settings::is_on("scrollbar.style"),
        crate::settings::is_on("scrollbar.page_overrides"),
        &Look::in_force(),
    )
}

/// Put the stylesheet into one frame. The `on_load_start` caller, and the one below.
pub fn inject(frame: &mut Frame) {
    frame.execute_java_script(Some(&CefString::from(script().as_str())), None, 0);
}

/// Re-run the injection in every tab of every window — what `Backing::Scrollbar` calls.
///
/// **UI thread**, like every other `Backing` arm's push. A setting the user has just typed has to
/// change the page under them rather than the next page they open, and the script is written to
/// replace or remove what a previous run left, so calling it again is the whole of "apply".
///
/// The chrome strips are deliberately not in this walk. They are `bru://` documents that link
/// `chrome.css`, which carries the rules already; injecting a second copy into them would put the
/// page's answer where the stylesheet's belongs.
pub fn reinject_everywhere() {
    debug_assert_ne!(currently_on(ThreadId::UI), 0);
    let Some(state) = crate::state::BruState::instance() else {
        return;
    };
    let code = script();
    let Ok(mut state) = state.lock() else {
        return;
    };
    let ids: Vec<i32> = state
        .window_ids()
        .into_iter()
        .flat_map(|window| state.tab_browser_ids_in(window))
        .flatten()
        .collect();
    for id in ids {
        let Some(frame) = state.browser_with_id(id).and_then(|b| b.main_frame()) else {
            continue;
        };
        frame.execute_java_script(Some(&CefString::from(code.as_str())), None, 0);
    }
}

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

    /// Off is not "inject nothing" — it is "take out what a previous injection left".
    #[test]
    fn turning_it_off_still_removes_the_stylesheet() {
        let off = script_with(&theme(), false, true, &shipped());
        assert!(off.contains("o.remove()"), "{off}");
        assert!(!off.contains("webkit-scrollbar"), "{off}");
    }

    /// The stylesheet goes in **first**, which is what makes the default mean anything: a page rule
    /// of equal specificity beats ours by coming later, and only document order decides that.
    #[test]
    fn the_stylesheet_is_the_first_child_of_the_document_element() {
        assert!(script_with(&theme(), true, true, &shipped()).contains("r.insertBefore(s,r.firstChild)"));
    }

    /// The CSS reaches the page as a JS string literal, so a theme holding a quote or a backslash
    /// must not end it. `theme.css` is generated and holds neither today, which is exactly when a
    /// guard like this is cheap to write and impossible to remember later.
    #[test]
    fn a_quote_in_the_theme_cannot_close_the_string() {
        let theme = ":root{\n--completion-scrollbar-bg: #fff\"; } body { display:none;\n}";
        let script = script_with(theme, true, true, &shipped());
        // The quote must arrive backslashed. Asserting the *absence* of `"; } body` would pass on a
        // script that never quoted the CSS at all, which is the bug this was written after.
        assert!(script.contains("#fff\\\"; } body"), "the quote was not escaped: {script}");
        assert!(script.contains("var c=\""), "the CSS is not inside a string at all: {script}");
    }
}
