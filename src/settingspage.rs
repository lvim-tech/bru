//! `bru://chrome/settings` — every setting bru has, every setting it refuses, and what Chromium is
//! actually enforcing.
//!
//! This is what a bare `:set` opens, which is what `Ss` is bound to: qutebrowser sends it to
//! `qute://settings` (`configcommands.py:95-99`) and bru sends it here.
//!
//! Built at request time from [`crate::settings::SETTINGS`] and [`crate::settings::REFUSED`], for
//! the reason `src/help.rs` and `src/history.rs` are: a page written separately from the table it
//! describes drifts, and a settings page that disagrees with the browser is worse than none. Adding
//! a setting to `settings.rs` adds a row here with no edit to this file, and deleting one deletes
//! the row.
//!
//! ## Where the "in force" column comes from, and why it is not bru's own store
//!
//! `settings.rs` keeps the live values in a private `static LIVE`, and nothing public reads it —
//! `run_set` prints and returns `()`. The public reader it does expose is
//! [`crate::settings::chromium_value`], which asks **Chromium** what the setting is at a URL. That
//! is the better column anyway, and `settings.rs` says why in its own words: "It asks Chromium
//! rather than bru's own store on purpose: a store that agrees with itself proves nothing."
//!
//! The cost is that the answer is per-URL, so the page probes one URL — the tab bru was showing
//! when the page was asked for — and names it in the heading rather than implying the value is
//! global. A `-u` rule written for some *other* host is invisible here, and the page says so rather
//! than leaving a reader to assume the table is complete.
//!
//! `start_page` has no Chromium side at all (`chromium_value` answers `None` for it), so its row
//! carries [`crate::open::start_page`], which is the value `:open` with no argument would use.

use crate::settings::{Kind, Scopes, REFUSED, SETTINGS};

/// The URL whose content settings the page reports, when there is no page to ask about.
///
/// A content setting has no value except at a URL — `RequestContext::content_setting` takes one —
/// so a page with no tab behind it still has to name something. `example.com` is the documentation
/// host reserved by RFC 2606 and reaches no network here: `content_setting` is a lookup in
/// Chromium's own rule table, not a request.
const NO_PAGE: &str = "https://example.com/";

/// The page, as HTML. `probe` is the URL the "in force" column is read at.
pub fn page(probe: &str) -> String {
    let mut out = String::with_capacity(8 * 1024);
    out.push_str(
        r#"<!doctype html>
<meta charset="utf-8">
<title>bru — settings</title>
<link rel="stylesheet" href="chrome.css">
<link rel="stylesheet" href="theme.css">
<body data-view="help">
<main id="help">
"#,
    );

    out.push_str(&format!(
        "<h1>bru</h1>\n<p class=\"summary\">{} settings, {} refused. bru writes no configuration \
         file: <code>:set</code> changes the running browser and nothing on disk. The last column \
         is what <em>Chromium</em> answers for <code>{}</code>, not what bru's own store holds — a \
         store that agrees with itself proves nothing. A rule written with <code>-u</code> for some \
         other host does not show here.</p>\n",
        SETTINGS.len(),
        REFUSED.len(),
        escape(probe),
    ));

    out.push_str("<h2>settings</h2>\n<table>\n");
    for def in SETTINGS {
        out.push_str(&format!(
            "<tr class=\"live\"><td class=\"keys\">{}</td><td class=\"cmd\">{} · default {} · {}</td>\
             <td class=\"state\">{}</td></tr>\n",
            escape(def.name),
            escape(kind(def.kind)),
            escape(def.default.unwrap_or("unset")),
            escape(scope(def.scopes)),
            escape(&in_force(def.name, probe)),
        ));
    }
    out.push_str("</table>\n");

    out.push_str("<h2>refused</h2>\n<table>\n");
    for (name, why) in REFUSED {
        out.push_str(&format!(
            "<tr class=\"todo\"><td class=\"keys\">{}</td><td class=\"cmd\">{}</td>\
             <td class=\"state\">not a setting</td></tr>\n",
            escape(name),
            escape(why),
        ));
    }
    out.push_str("</table>\n</main>\n");
    out
}

/// What the setting is right now, in one cell.
fn in_force(name: &str, probe: &str) -> String {
    // No Chromium side at all. `open.rs` resolves it through the same fuzzy parse `:open` uses, so
    // a `start_page` written `example.com` reads back here as the URL it will actually load.
    if name == "start_page" {
        return crate::open::start_page();
    }
    // `chromium_value` calls `request_context_get_global_context()`, which is a call into libcef
    // and has no answer before `initialize` — a unit test, or the window between `initialize` and
    // the first tab. `BruState::instance()` is `None` in exactly that window, and asking it first
    // is what keeps this file testable without a browser behind it.
    if crate::state::BruState::instance().is_none() {
        return "not readable without a browser".to_string();
    }
    crate::settings::chromium_value(name, probe)
        .unwrap_or_else(|| "Chromium has no value for this".to_string())
}

fn kind(kind: Kind) -> &'static str {
    match kind {
        Kind::Bool => "true or false",
        Kind::Text => "text",
    }
}

/// Written with real angle brackets; [`escape`] is what puts them on the page.
fn scope(scopes: Scopes) -> &'static str {
    match scopes {
        Scopes::GlobalOnly => "global only",
        Scopes::UrlOnly => "-u <pattern> only",
        Scopes::Both => "global or -u <pattern>",
    }
}

/// The same escape `src/help.rs` has, and here for the same reason: every cell above is built out
/// of a string from `settings.rs`, and [`REFUSED`]'s reasons are prose that already contains `<`
/// and `>` in `cef_content_setting_types_t`-shaped names.
///
/// It is duplicated rather than shared because `help.rs` belongs to another workstream and a
/// twelve-line pure function is a cheaper thing to repeat than a cross-module dependency is to
/// merge.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
    out
}

/// The page for whatever the browser is on right now. `chrome.rs` calls this on every request.
///
/// The probe URL is the tab bru was showing when the request arrived: the resource handler runs
/// before the navigation commits, so `ipc::current_url` still holds the page `Ss` was pressed on,
/// which is the page whose per-URL settings a reader wants. A `bru://` URL is not probeable —
/// Chromium's content-settings patterns cannot name a custom scheme, measured in `settings.rs` —
/// so it falls back to [`NO_PAGE`].
pub fn current_page() -> String {
    let url = crate::ipc::current_url();
    let probe = if url.starts_with("http://") || url.starts_with("https://") {
        url
    } else {
        NO_PAGE.to_string()
    };
    page(&probe)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One row per setting and one per refusal, and no more: the page is the table, not a copy of
    /// it that someone has to remember to update.
    #[test]
    fn every_setting_and_every_refusal_has_a_row() {
        let html = page("https://example.com/");
        let rows = html.matches("<tr class=").count();
        assert_eq!(rows, SETTINGS.len() + REFUSED.len());
        for def in SETTINGS {
            assert!(html.contains(def.name), "{} is missing", def.name);
        }
        for (name, _) in REFUSED {
            assert!(html.contains(name), "{name} is missing");
        }
        // The refusal's *reason* is the point of listing it — a name with no reason beside it is
        // the "someone forgot" the whole list exists to deny.
        assert!(html.contains("Chromium 151 has no plugins content setting"));
    }

    /// The scope rule that keeps bru's own chrome alive has to be visible, or the page invites a
    /// global `content.javascript.enabled false` that `settings.rs` will refuse.
    #[test]
    fn the_page_says_which_settings_take_a_pattern() {
        let html = page("https://example.com/");
        assert!(html.contains("-u &lt;pattern&gt; only"));
        assert!(html.contains("global only"));
    }

    /// `REFUSED`'s reasons are prose with angle brackets in them, and `settings.rs` is not this
    /// module's to police. Nothing it holds may reach the page as markup.
    #[test]
    fn nothing_from_the_settings_table_can_escape_its_cell() {
        let html = page("<script>alert(1)</script>");
        assert!(!html.contains("<script>alert(1)"), "the probe URL is text");
        assert!(html.contains("&lt;script&gt;alert(1)"));
        // Every reason arrives through `escape`. None of the two currently written happens to
        // contain markup, so this only bites when one does — which is the point of asserting the
        // escaped form rather than the absence of the raw one.
        for (_, why) in REFUSED {
            assert!(html.contains(&escape(why)));
        }
        assert_eq!(escape("a & b"), "a &amp; b");
        assert_eq!(escape("\"x\""), "&quot;x&quot;");
    }

    /// Outside a running browser there is no Chromium to ask, and the cell says that rather than
    /// printing the compiled-in default as though it were in force.
    #[test]
    fn a_value_that_cannot_be_read_says_so() {
        let html = page("https://example.com/");
        assert!(html.contains("not readable without a browser"));
        // `start_page` never had a Chromium side, so it answers from `open.rs` even here.
        assert!(html.contains(&escape(&crate::open::start_page())));
    }
}
