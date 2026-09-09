//! `::selection` on every page, in bru's theme — but only where the site has no opinion of its own.
//!
//! Asked for by name: *"ако няма даден сайт бг и фг цвят за селект - да използва цветове от
//! темата"*.
//!
//! ## The cascade does the checking, and that is the whole design
//!
//! Nothing here detects whether a site styles its own selection, and nothing needs to. The rule
//! goes in as the **first child of `<html>`**, before the page's own `<head>` is parsed, with **no
//! `!important` anywhere** — so a site rule of equal specificity comes later in document order and
//! wins. "Use the theme's colours unless the site gave its own" is not a branch; it is what the
//! cascade already does with two rules in that order.
//!
//! That is `scrollbar.rs`'s mechanism, its wording and its setting: `selection.page_overrides` is
//! **true** by default and flipping it adds `!important` to both declarations so bru wins instead.
//!
//! ## Which two colours, and why these
//!
//! A selection has to work on **every** site, light and dark, which rules out most of a palette
//! built for one background. Measured (WCAG contrast, `#ffffff` standing for a light page and
//! `--bg-dark` for a dark one):
//!
//! | | text on the selection | seen on a white page | on a dark page |
//! |---|---|---|---|
//! | **`--statusbar-caret-selection-bg`/`-fg`** (`--magenta` #bb755e on `--bg`) | **4.10:1** | **3.61:1** | **4.36:1** |
//! | `--hints-bg`/`--hints-fg` (#c3ab58 / #232929) | 6.54:1 | 2.26:1 | 6.95:1 |
//! | `--blue` on white | 5.24:1 | 5.24:1 | 3.00:1 |
//! | `--bg-light` on `--ui-fg` | 4.26:1 | 13.85:1 | **1.14:1** |
//!
//! The caret pair is taken, for a number and for a name:
//!
//! - **It is the only one that clears 3.5:1 against both a white page and a dark one** while
//!   keeping its own text readable. The hints pair reads better and is nearly invisible on a white
//!   background, which is most of the web; the last row is invisible on a dark one.
//! - **It is the theme's own name for a selection.** `--statusbar-caret-selection-bg` is what the
//!   status bar already draws when caret mode holds a selection, so the selection on the page and
//!   the indicator bru shows for it become one colour rather than two.
//!
//! ## Where the machinery is
//!
//! **In `scrollbar.rs`, and deliberately not duplicated here.** That module carries `PUSHED`,
//! `SET_RULES`, `ASK`, the keeper in `chrome/userstyle.js`, `renderer_on_context_created` and
//! `push_rules` — all of which is written for *bru's stylesheet inside somebody else's page* and
//! none of which is about scrollbars. A second copy for two declarations would be ~250 lines of
//! duplicated cross-process coordination and a second `<style>` element to fight over.
//!
//! So this file owns the colours, the rule and its tests; `scrollbar::rules` appends what
//! [`css`] returns to the stylesheet it already sends.

/// The two settings that can override the theme, as they stand.
///
/// Not `Default`, which is what a store-less process gets — the same split `scrollbar::Look` makes,
/// and for the same reason: a renderer has no settings store and reads bru's compiled-in defaults.
pub struct Look {
    /// `selection.bg`, or `None` for whatever the theme says.
    pub bg: Option<String>,
    /// `selection.fg`, or `None` for whatever the theme says.
    pub fg: Option<String>,
}

impl Look {
    fn in_force() -> Self {
        Look {
            bg: crate::settings::text_of("selection.bg"),
            fg: crate::settings::text_of("selection.fg"),
        }
    }
}

/// The rule against the theme and the settings in force, or nothing at all when `selection.style`
/// is off.
///
/// Called from `scrollbar::rules`, which has already read the theme — it is passed in rather than
/// re-read so that one page load reads `~/.config/bru/theme.css` once.
pub fn css(theme: &str) -> String {
    if !crate::settings::is_on("selection.style") {
        return String::new();
    }
    css_with(
        theme,
        crate::settings::is_on("selection.page_overrides"),
        &Look::in_force(),
    )
}

/// The rule, against one theme, with the cascade pointed the way the setting says.
///
/// `page_wins` is `selection.page_overrides`, a parameter rather than a read of the store so the
/// tests can put it either way in a process that has no store — `scrollbar::css_with`'s shape.
///
/// **Each source is filtered and a rejected value falls through to the next**, exactly as
/// `scrollbar::css_with` does and for the reason it gives: these strings are interpolated into a
/// rule and then into a JS string literal, and a value carrying a `}` or a `;` would close the rule
/// it is in and write its own. Falling through rather than refusing leaves a selection in the wrong
/// shade; refusing leaves one drawn transparent, which is worse.
///
/// The last fallbacks are everforest's own and are reached only by a `theme.css` that has lost a
/// property.
pub fn css_with(theme: &str, page_wins: bool, look: &Look) -> String {
    let safe = |value: &&str| crate::chrome::is_safe_colour(value);
    let bg = look
        .bg
        .as_deref()
        .filter(safe)
        .or_else(|| crate::scrollbar::resolve(theme, "--statusbar-caret-selection-bg").filter(safe))
        .unwrap_or("#bb755e");
    let fg = look
        .fg
        .as_deref()
        .filter(safe)
        .or_else(|| crate::scrollbar::resolve(theme, "--statusbar-caret-selection-fg").filter(safe))
        .unwrap_or("#232929");
    let bang = if page_wins { "" } else { " !important" };
    // `::selection` only. `::-moz-selection` is Firefox's and this is Chromium; an unknown
    // pseudo-element in a selector list would invalidate the whole rule, so it is not merely
    // useless here, it would be harmful.
    format!("::selection{{background:{bg}{bang};color:{fg}{bang}}}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const THEME: &str = "\
:root {
    --bg: #232929;
    --magenta: #bb755e;
    --statusbar-caret-selection-fg: var(--bg);
    --statusbar-caret-selection-bg: var(--magenta);
}";

    fn nothing() -> Look {
        Look { bg: None, fg: None }
    }

    /// The two colours come out of the theme, through the `var()` indirection — the file defines
    /// them as references, so reading the property is not enough and `scrollbar::resolve` is what
    /// follows them to a colour.
    #[test]
    fn the_colours_are_the_themes_own_selection_pair() {
        let css = css_with(THEME, true, &nothing());
        assert_eq!(css, "::selection{background:#bb755e;color:#232929}");
    }

    /// **The measurement in the module comment, as a test.** These two are not a preference: they
    /// are the only pair measured to stay visible on a white page *and* a dark one while keeping
    /// their own text readable. A silent swap to a prettier pair is what this catches.
    #[test]
    fn it_reads_the_pair_the_measurement_chose_and_not_another() {
        assert!(css_with(THEME, true, &nothing()).contains("#bb755e"));
        // The hints pair reads better on its own and is 2.26:1 against a white page. If someone
        // points this at `--hints-bg`, the theme above will not resolve it and the fallback shows.
        let without = "\
:root {
    --hints-bg: #c3ab58;
}";
        assert_eq!(
            css_with(without, true, &nothing()),
            "::selection{background:#bb755e;color:#232929}",
            "a theme with no selection pair falls back to the measured colours, not to a hint"
        );
    }

    /// The default is that the site wins, which is the whole feature: the theme's colours are what
    /// a page without its own `::selection` gets, and nothing more.
    #[test]
    fn nothing_is_important_by_default_and_everything_is_when_the_setting_flips() {
        assert!(!css_with(THEME, true, &nothing()).contains("!important"));
        let bru_wins = css_with(THEME, false, &nothing());
        assert_eq!(bru_wins.matches("!important").count(), 2, "both declarations, or neither");
        assert_eq!(
            bru_wins,
            "::selection{background:#bb755e !important;color:#232929 !important}"
        );
    }

    #[test]
    fn a_setting_beats_the_theme() {
        let look = Look { bg: Some("#112233".into()), fg: Some("#ffffff".into()) };
        assert_eq!(
            css_with(THEME, true, &look),
            "::selection{background:#112233;color:#ffffff}"
        );
    }

    /// A value that would close the rule and write its own falls **through to the theme** rather
    /// than being refused — `scrollbar::css_with`'s rule, and the reason is the same: a selection
    /// in the wrong shade is recoverable and one drawn transparent is not.
    #[test]
    fn an_unsafe_value_falls_through_to_the_theme() {
        let look = Look {
            bg: Some("red;} body{display:none".into()),
            fg: Some("blue}".into()),
        };
        let css = css_with(THEME, true, &look);
        assert_eq!(css, "::selection{background:#bb755e;color:#232929}");
        assert!(!css.contains("display:none"));
    }

    /// **Against the theme bru actually ships**, not a fixture. `resolve` follows
    /// `--statusbar-caret-selection-bg: var(--magenta)` to a colour, and a generated theme that
    /// stopped emitting the pair would silently fall back to the hard-coded pair above — which
    /// would be the right colours by luck rather than because the theme said so. `hints.rs` runs
    /// its own `label_style_json_from(include_str!(…))` for the same reason.
    #[test]
    fn the_shipped_theme_really_carries_the_pair() {
        let shipped = include_str!("../chrome/theme.css");
        let bg = crate::scrollbar::resolve(shipped, "--statusbar-caret-selection-bg");
        let fg = crate::scrollbar::resolve(shipped, "--statusbar-caret-selection-fg");
        assert_eq!(bg, Some("#bb755e"), "the theme no longer resolves a selection background");
        assert_eq!(fg, Some("#232929"), "the theme no longer resolves a selection foreground");
        assert_eq!(
            css_with(shipped, true, &nothing()),
            "::selection{background:#bb755e;color:#232929}"
        );
    }

    /// **The other half, and the one that can silently drift.** `scrollbar::styleable` refuses a
    /// `bru://` frame — bru's own pages link `chrome.css` and must not also be injected into — so
    /// the selection colours reach them through that stylesheet instead. Two places, one decision;
    /// this is what stops one of them being changed alone.
    ///
    /// It was found the hard way: the injected rule shipped first, and the dial — bru's own page —
    /// still selected in Chromium's blue, because nothing had ever reached it.
    #[test]
    fn brus_own_pages_get_the_same_pair_through_chrome_css() {
        let chrome = include_str!("../chrome/chrome.css");
        assert!(
            chrome.contains("::selection {"),
            "chrome/chrome.css no longer styles a selection, so bru's own pages fall back to \
             Chromium's blue while every web page wears the theme"
        );
        // The same two custom properties this module resolves, by name. A different pair here
        // would mean a selection that changes colour when you move from a site to `bru://`.
        for property in [
            "var(--statusbar-caret-selection-bg)",
            "var(--statusbar-caret-selection-fg)",
        ] {
            assert!(chrome.contains(property), "chrome/chrome.css does not use {property}");
        }
    }

    /// One rule, one pseudo-element, nothing else. `::-moz-selection` in the same selector list
    /// would invalidate the whole rule in Chromium.
    #[test]
    fn it_is_one_rule_and_names_no_other_pseudo_element() {
        let css = css_with(THEME, true, &nothing());
        assert_eq!(css.matches('{').count(), 1);
        assert!(!css.contains("-moz-"));
        assert!(css.starts_with("::selection{"));
    }
}
