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
//!
//! ## The one thing that cannot happen at request time, and why
//!
//! **`RequestContext::get_content_setting` must be called on the browser process UI thread**
//! (`cef_request_context_capi.h:287-291`) and **a scheme handler factory's functions "will always
//! be called on the IO thread"** (`cef_scheme_capi.h:86-88`). So the one thing this page cannot do
//! where it is built is ask Chromium.
//!
//! It does not fail loudly. Measured 2026-08-06, on a tab on `https://example.com/` after
//! `config-cycle -u *://*.example.com/* content.images` had written the rule: `--settings-probe`,
//! which runs the *same* `chromium_value` from a posted UI task, answered `block`, and this page —
//! calling it from the resource handler — printed `default` for the same setting at the same URL.
//! The wrong answer is the enum's "nothing is configured", which is exactly the answer a reader
//! would believe.
//!
//! So the reading is taken on the UI thread and kept in [`SNAPSHOT`]: synchronously by
//! [`refresh`], which `exec.rs` calls in the `SettingsPage` arm *before* it navigates, and again
//! from a task this module posts each time the page is built, so that a reload is never more than
//! one build behind. The page prints how long ago its reading was taken rather than implying it is
//! this instant's.

use std::sync::Mutex;
use std::time::Instant;

use cef::*;

use crate::settings::{Kind, Scopes, REFUSED, SETTINGS};

/// The URL whose content settings the page reports, when there is no page to ask about.
///
/// A content setting has no value except at a URL — `RequestContext::content_setting` takes one —
/// so a page with no tab behind it still has to name something. `example.com` is the documentation
/// host reserved by RFC 2606 and reaches no network here: `content_setting` is a lookup in
/// Chromium's own rule table, not a request.
const NO_PAGE: &str = "https://example.com/";

/// What Chromium last answered, the URL it was asked at, and when.
#[derive(Debug)]
pub struct Snapshot {
    url: String,
    at: Instant,
    values: Vec<(&'static str, String)>,
}

impl Snapshot {
    fn value(&self, name: &str) -> Option<&str> {
        self.values
            .iter()
            .find(|(known, _)| *known == name)
            .map(|(_, value)| value.as_str())
    }
}

static SNAPSHOT: Mutex<Option<Snapshot>> = Mutex::new(None);

/// Ask Chromium what every content setting is, and keep the answer.
///
/// **UI thread only** — see the module docs for the two header lines that make that true and for
/// the measurement of what happens when it is not.
///
/// The URL asked about is the tab that is showing, unless that is the settings page itself, in
/// which case the previous reading's URL is kept: `bru://` is not a URL Chromium's content-settings
/// patterns can name (measured in `settings.rs`), and switching the probe to `example.com` on a
/// reload would make the column change under a reader who had only pressed `r`.
pub fn refresh() {
    debug_assert_ne!(currently_on(ThreadId::UI), 0);

    let mut guard = SNAPSHOT.lock().expect("the settings page mutex is never poisoned");
    let page = crate::ipc::current_url();
    let url = if page.starts_with("http://") || page.starts_with("https://") {
        page
    } else {
        guard
            .as_ref()
            .map(|snapshot| snapshot.url.clone())
            .unwrap_or_else(|| NO_PAGE.to_string())
    };

    let values = SETTINGS
        .iter()
        .filter_map(|def| crate::settings::chromium_value(def.name, &url).map(|v| (def.name, v)))
        .collect();

    *guard = Some(Snapshot { url, at: Instant::now(), values });
}

/// The page, as HTML, against a reading Chromium has already given — `None` before any has been
/// taken, which is what a unit test and a browser that has not started yet both look like.
pub fn page(snapshot: Option<&Snapshot>) -> String {
    let probe = snapshot.map_or(NO_PAGE, |snapshot| snapshot.url.as_str());
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
         is what <em>Chromium</em> answered for <code>{}</code> {}, not what bru's own store holds \
         — a store that agrees with itself proves nothing. It is read on the UI thread and this \
         page is built on the IO thread, which is why it carries an age. A rule written with \
         <code>-u</code> for some other host does not show here.</p>\n",
        SETTINGS.len(),
        REFUSED.len(),
        escape(probe),
        match snapshot {
            Some(snapshot) => format!("{} ms ago", snapshot.at.elapsed().as_millis()),
            None => "— nothing has been read yet".to_string(),
        },
    ));

    out.push_str("<h2>settings</h2>\n<table>\n");
    for def in SETTINGS {
        out.push_str(&format!(
            "<tr class=\"live\"><td class=\"keys\">{}</td><td class=\"cmd\">{} · default {} · {}</td>\
             <td class=\"state\">{}</td></tr>\n",
            escape(def.name),
            escape(kind(def.kind)),
            escape(&default_of(def)),
            escape(scope(def.scopes)),
            // Not escaped as one string: a dictionary's cell is a line per pair, and the `<br>`s
            // between them are the page's own markup. Every *value* inside it went through
            // `escape` in `in_force_cell`.
            in_force_cell(def, snapshot),
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

/// What the "default" column says. A dictionary's default is not a value but a table, so the column
/// says how big it is and the value column shows what is in it — printing one of twelve labels
/// there would be picking one at random.
fn default_of(def: &crate::settings::Def) -> String {
    match def.kind {
        Kind::Dict(shape) => format!("{} entries", shape.defaults.len()),
        // --- config commands ----------------------------------------------------------------
        // A list's default is a table too, and for the same reason: naming one of the two filter
        // lists here would be picking one at random.
        Kind::List(shape) => format!("{} entries", shape.defaults.len()),
        // --- end config commands ------------------------------------------------------------
        _ => def.default.unwrap_or("unset").to_string(),
    }
}

/// **What the value column shows for a dictionary**: one line per pair, `key value`, in the store's
/// own sorted order, with the pairs the user has moved marked.
///
/// A dict is not one line and this is the cell that says so. The same question is answered for
/// `:set` in `settings::describe_dict`, and the two are deliberately different shapes: the terminal
/// gets `option[key] = value` because a line there has to carry which option it belongs to, and
/// this cell is already inside the option's row, so repeating the name nine times would be nine
/// columns of noise. What they share is the *marking* — a reader of either can see which pairs are
/// bru's and which are not without holding the defaults in their head.
fn in_force_cell(def: &crate::settings::Def, snapshot: Option<&Snapshot>) -> String {
    // --- config commands --------------------------------------------------------------------
    // A list is the same argument in the other container: `2 entries` in this cell would be a row
    // that names a setting and shows none of it, which is the thing this function exists to avoid.
    if let Kind::List(shape) = def.kind {
        let entries = crate::settings::list_of(def.name);
        if entries.is_empty() {
            return "empty".to_string();
        }
        return entries
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let note = if shape.defaults.contains(&entry.as_str()) {
                    ""
                } else {
                    " ·  added"
                };
                format!("<code>{index}</code> {}{}", escape(entry), note)
            })
            .collect::<Vec<_>>()
            .join("<br>");
    }
    // --- end config commands ----------------------------------------------------------------
    let Kind::Dict(shape) = def.kind else {
        return escape(&in_force(def.name, snapshot));
    };
    let pairs = crate::settings::dict_of(def.name);
    if pairs.is_empty() {
        return "empty".to_string();
    }
    pairs
        .iter()
        .map(|(key, value)| {
            let ships = shape
                .defaults
                .iter()
                .find(|(known, _)| known == key)
                .map(|(_, value)| *value);
            let note = match ships {
                Some(default) if default == value => "",
                Some(_) => " ·  changed",
                None => " ·  added",
            };
            format!("<code>{}</code> {}{}", escape(key), escape(value), note)
        })
        .collect::<Vec<_>>()
        .join("<br>")
}

/// What the setting is, in one cell.
fn in_force(name: &str, snapshot: Option<&Snapshot>) -> String {
    // No Chromium side at all, and no thread problem either: `open.rs` resolves it in Rust, through
    // the same fuzzy parse `:open` uses, so a `start_page` written `example.com` reads back here as
    // the URL it will actually load.
    if name == "start_page" {
        return crate::open::start_page();
    }
    // The same for every setting bru answers itself. "not read yet" is a statement about
    // *Chromium's* copy, and printing it against a setting Chromium has never heard of says the
    // browser does not know its own value — which it does. `settings::value_of` is where it lives,
    // and `focus.rs`'s four `input.insert_mode.*` are answered through it too.
    if let Some(value) = crate::settings::value_of(name) {
        return value;
    }
    match snapshot.and_then(|snapshot| snapshot.value(name)) {
        Some(value) => value.to_string(),
        // Before the first reading — a unit test, or a browser that has not run `refresh` yet.
        None => "not read yet".to_string(),
    }
}

fn kind(kind: Kind) -> &'static str {
    match kind {
        Kind::Bool => "true or false",
        Kind::Text => "text",
        // Leaked as a `&'static str` because this returns one and the list is compiled in: one
        // allocation per settings-page render, of a string that would have been a literal anyway.
        Kind::Choice(choices) => Box::leak(choices.join(" or ").into_boxed_str()),
        Kind::Dict(shape) if shape.open_keys => "a dictionary, any key",
        Kind::Dict(_) => "a dictionary, fixed keys",
        Kind::List(_) => "a list",
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

/// The page as it stands. `chrome.rs` calls this on every request, **on the IO thread**.
///
/// The table is built here and now, from `settings.rs`'s own statics, so it can no more drift from
/// them than `bru://chrome/help` can from the binding table. The one column that cannot be read
/// here is Chromium's, so this serves the last reading and posts a task to take the next one — see
/// the module docs. `exec.rs` refreshes synchronously before it navigates, so the first view after
/// `Ss` is a reading taken microseconds earlier; a reload is at most one build behind, and the page
/// says how old it is either way.
pub fn current_page() -> String {
    let html = {
        let guard = SNAPSHOT.lock().expect("the settings page mutex is never poisoned");
        page(guard.as_ref())
    };
    let mut task = RefreshSnapshot::new();
    post_task(ThreadId::UI, Some(&mut task));
    html
}

wrap_task! {
    struct RefreshSnapshot;

    impl Task {
        fn execute(&self) {
            refresh();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One row per setting and one per refusal, and no more: the page is the table, not a copy of
    /// it that someone has to remember to update.
    /// A reading of the kind `refresh` takes, without a browser to take it from.
    fn snapshot(url: &str) -> Snapshot {
        Snapshot {
            url: url.to_string(),
            at: Instant::now(),
            values: vec![
                ("content.javascript.enabled", "allow".to_string()),
                ("content.images", "block".to_string()),
            ],
        }
    }

    #[test]
    fn every_setting_and_every_refusal_has_a_row() {
        let taken = snapshot("https://example.com/");
        let html = page(Some(&taken));
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
        assert!(html.contains("Chromium 151 has nothing behind this name"));
    }

// --- config commands -------------------------------------------------------------------------
    /// The list setting's row shows its entries rather than counting them, which is the same claim
    /// `a_dictionary_setting_shows_a_line_per_pair` makes for the dictionaries.
    #[test]
    fn a_list_setting_shows_a_line_per_entry() {
        let html = page(Some(&snapshot("https://example.com/")));
        assert!(html.contains("content.blocking.adblock.lists"), "the row is missing");
        for entry in crate::adblock::DEFAULT_LISTS {
            assert!(html.contains(entry), "{entry} is not on the page");
        }
        assert!(html.contains("a list"), "the kind column does not say what it is");
        // The count alone would be a row that names a setting and shows none of it.
        assert!(
            !html.contains("<td>2 entries</td>"),
            "the value column is counting instead of listing"
        );
    }
// --- end config commands -----------------------------------------------------------------------

    /// The scope rule that keeps bru's own chrome alive has to be visible, or the page invites a
    /// global `content.javascript.enabled false` that `settings.rs` will refuse.
    #[test]
    fn the_page_says_which_settings_take_a_pattern() {
        let html = page(Some(&snapshot("https://example.com/")));
        assert!(html.contains("-u &lt;pattern&gt; only"));
        assert!(html.contains("global only"));
    }

    /// The column Chromium answers, and the two states it can be in. It is the reason this module
    /// keeps a snapshot at all — see the module docs and the measurement in them.
    #[test]
    fn the_last_reading_chromium_gave_is_what_the_column_shows() {
        let taken = snapshot("https://example.com/");
        let html = page(Some(&taken));
        assert!(html.contains("https://example.com/"), "the URL it was read at is named");
        assert!(html.contains("<td class=\"state\">block</td>"), "content.images was blocked");
        assert!(html.contains("<td class=\"state\">allow</td>"));
        assert!(html.contains("ms ago"), "the reading carries its age");

        // Before any reading, and that is what a browser that has not opened this page yet looks
        // like. It must not print a default and call it what Chromium is enforcing.
        let html = page(None);
        assert!(html.contains("nothing has been read yet"));
// --- content settings ----------------------------------------------------------------------------
        // Seventeen on this branch: the two content settings bru started with, twelve more, and the
        // three whose value lives in a Chromium *preference* — which is Chromium's copy too, so
        // "not read yet" is the honest cell for them before a reading. **This number conflicts with
        // the other two settings workstreams**; +15 is this one's.
        assert_eq!(html.matches("<td class=\"state\">not read yet</td>").count(), 16);
// --- end content settings ------------------------------------------------------------------------
        // `start_page` and `statusbar.mode.style` are answered in Rust and need no reading, so
        // neither is ever one of them — the two above are the content settings.
        assert!(html.contains(&escape(&crate::open::start_page())));
        // Nor are the four insert-mode settings: they are bru's own and always in force. Their
        // rows are the only `true`/`false` cells on the page, which is what this counts.
        assert_eq!(html.matches("<td class=\"state\">false</td>").count(), 1);
        assert_eq!(html.matches("<td class=\"state\">true</td>").count(), 4);
    }

    /// **A dict is not one line, and this is what the value column does about it.** One row per
    /// setting still, but the cell inside it is a line per pair.
    #[test]
    fn a_dictionary_setting_shows_a_line_per_pair() {
        let html = page(Some(&snapshot("https://example.com/")));
        // Still one row per setting — the pairs are inside the row, not extra rows.
        assert_eq!(html.matches("<tr class=").count(), SETTINGS.len() + REFUSED.len());
        // Nine engines and twelve labels, each as `<code>key</code> value`.
        for (name, template) in crate::open::DEFAULT_ENGINES {
            assert!(
                html.contains(&format!("<code>{name}</code> {}", escape(template))),
                "{name} is missing from the value column"
            );
        }
        assert!(html.contains("<code>search_forward</code> search"));
        assert!(html.contains("<code>set_mark</code> set mark"));
        // The lines are separated by the page's own markup, which is the reason this cell is not
        // put through `escape` as one string.
        assert!(html.contains("<code>aw</code> https://wiki.archlinux.org/?search={}<br>"));
        // ...and the last pair of a dict ends the cell rather than trailing one.
        assert!(html.contains(
            "<code>yt</code> https://www.youtube.com/results?search_query={}</td></tr>"
        ));
        // The kind column says which of the two dicts will take a key it does not ship.
        assert!(html.contains("a dictionary, any key"));
        assert!(html.contains("a dictionary, fixed keys"));
        // A dictionary's "default" is a count, not one of its values.
        assert!(html.contains("default 9 entries"));
        assert!(html.contains("default 14 entries"));
    }

    /// `REFUSED`'s reasons are prose with angle brackets in them, and `settings.rs` is not this
    /// module's to police. Nothing it holds may reach the page as markup.
    #[test]
    fn nothing_from_the_settings_table_can_escape_its_cell() {
        let html = page(Some(&snapshot("<script>alert(1)</script>")));
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

}
