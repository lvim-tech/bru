//! Where a page visit becomes a row, and the two `bru://` pages that read those rows back.
//!
//! `src/data.rs` has had `record_visit` since the milestone that built it and **nothing has ever
//! called it**, so `history.sqlite` held zero rows and the completion's History category could not
//! appear however much a user browsed. This file is the missing caller.
//!
//! ## The seam it uses, and the one it deliberately does not
//!
//! bru learns that a page loaded from `DisplayHandler`, not from a `LoadHandler`:
//!
//! | | |
//! |---|---|
//! | `on_address_change` | the main frame committed an address → [`visited`] |
//! | `on_title_change` | the document has a title → [`retitled`] |
//!
//! That is not a compromise, it is the pair `data.rs` was written against — its `update_title` doc
//! says so in as many words: "CEF reports the address before the title — `on_address_change` fires
//! at commit, `on_title_change` when the document has one — so the row written at navigation
//! frequently holds the old page's title or none at all." Both callbacks were already implemented in
//! `src/keys.rs`, keyed by browser identifier and filtered down to real tabs, which is exactly the
//! filtering a recorder needs; a `LoadHandler` would have had to redo it. So this module adds **two
//! lines** to a shared file and leaves `wrap_load_handler!` entirely to the workstream that wants it.
//!
//! One thing is lost with `DisplayHandler` and it is worth naming: Chromium only reports an address
//! that **commits**, so the intermediate URLs of a redirect chain are never seen and every visit is
//! recorded with `redirect = false`. qutebrowser gets them from Qt's `requestedUrl`
//! (`history.py:378-394`). The column and the code path exist in `data.rs`; nothing fills them in
//! today. A `LoadHandler` would not fix it either — `on_load_start` also only fires on commit; what
//! would is `RequestHandler::on_resource_redirect`, and that is a milestone of its own.
//!
//! ## The pages
//!
//! `bru://chrome/history` and `bru://chrome/bookmarks`, generated per request from the database and
//! the two mark files — the same rule `src/help.rs` follows, and for the same reason: a page written
//! separately from the thing it describes drifts, and a history page that disagrees with the history
//! is worse than none. Unlike the help page these link only `theme.css`, not `chrome.css`: the
//! stylesheet is the two strips' and sets `overflow: hidden` and `user-select: none` on `body`,
//! which a document meant to be read and copied out of must not inherit.

use crate::data::{self, Data};
use crate::tabs::SharedState;
// The glob is what `wrap_task!` needs in scope — it names `Task`, `WrapTask` and `ImplTask`
// unqualified. Every other CEF type here is spelled out.
use cef::*;
use std::sync::Mutex;

/// How many visits `bru://chrome/history` lists.
///
/// The log grows without bound — one row per page load, forever, which is the whole reason
/// `CompletionHistory` exists beside it. A page that rendered all of it would be a 9,000-row table
/// nobody scrolls to the end of. Five hundred is a few weeks of this user's browsing and renders in
/// one frame; the count of what is *not* shown is printed at the top so the number is never a
/// silent truncation.
const PAGE_VISITS: usize = 500;

/// The URL each browser was last reported to be on, so that a title arriving afterwards can be
/// attached to the right row.
///
/// Keyed by CEF browser identifier rather than by tab index: an index moves when `gJ` reorders the
/// strip, and a title that arrived in between would then correct another tab's history entry. A
/// `Vec` because bru has a handful of tabs, not a thousand.
static LAST_URL: Mutex<Vec<(i32, String)>> = Mutex::new(Vec::new());

/// A tab's main frame committed an address. Called from `DisplayHandler::on_address_change` in
/// `src/keys.rs`, after it has established that this browser is a tab and not a chrome strip.
///
/// The title is deliberately empty: at commit the tab's title is still the *previous* page's, and
/// writing that would be worse than writing nothing. [`retitled`] fixes the row a moment later.
pub fn visited(browser_id: i32, url: &str) {
    // Remembered whatever the database says, and before the write is even attempted. A repeat visit
    // is not recorded (`Data::record_visit` dedupes against the last URL) but the browser is still
    // *on* that page, so a title arriving now belongs to it.
    remember(browser_id, url);

    match with_data(|data| data.record_visit(url, "", false)) {
        Some(Ok(_)) => {}
        Some(Err(error)) => eprintln!("bru: could not record a visit to {url}: {error}"),
        // No data directory. `data::instance` has already said why, once.
        None => {}
    }
}

/// The document has a title. Called from `DisplayHandler::on_title_change` in `src/keys.rs`.
pub fn retitled(browser_id: i32, title: &str) {
    let Some(url) = last_url(browser_id) else {
        return;
    };
    if is_placeholder_title(&url, title) {
        return;
    }
    if let Some(Err(error)) = with_data(|data| data.update_title(&url, title)) {
        eprintln!("bru: could not retitle {url}: {error}");
    }
}

/// Whether a title is Chromium's stand-in rather than the document's own.
///
/// A tab's title is its address until the document has a `<title>`, so `on_title_change` fires with
/// the address on nearly every load — as the full URL first, then as the display form with the
/// scheme dropped and the trailing slash gone. Storing either would put a URL in the title column of
/// half the completion, and that reads as a bug in the completion rather than as a missing title.
///
/// The cost is a real page whose `<title>` genuinely *is* its own address: it keeps an empty title
/// instead, and the completion shows its URL — which is what it would have shown anyway.
fn is_placeholder_title(url: &str, title: &str) -> bool {
    if title.is_empty() || title == url {
        return true;
    }
    fn bare(s: &str) -> &str {
        s.trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_end_matches('/')
    }
    bare(url) == bare(title)
}

fn remember(browser_id: i32, url: &str) {
    let Ok(mut last) = LAST_URL.lock() else {
        return;
    };
    match last.iter_mut().find(|(id, _)| *id == browser_id) {
        Some((_, stored)) => {
            stored.clear();
            stored.push_str(url);
        }
        None => last.push((browser_id, url.to_string())),
    }
}

fn last_url(browser_id: i32) -> Option<String> {
    LAST_URL
        .lock()
        .ok()?
        .iter()
        .find(|(id, _)| *id == browser_id)
        .map(|(_, url)| url.clone())
}

/// Run `f` against the one open `Data`. `None` when there is no data directory — the browser goes on
/// browsing without a history rather than putting an error in the user's way, which is the rule
/// `data::instance` sets and `completion.rs` already follows.
fn with_data<T>(f: impl FnOnce(&mut Data) -> T) -> Option<T> {
    let data = data::instance()?;
    let mut guard = data.lock().ok()?;
    Some(f(&mut guard))
}

// --- the commands -------------------------------------------------------------------------------
//
// Every arm `src/exec.rs` gained is one call into this section, so the fenced block in that file
// stays short enough to merge beside eleven others. The data layer already had every function;
// what was missing was the argument handling and the answer to "which page am I on".

/// The showing tab's address and title, for `quickmark-save` and `bookmark-add`.
///
/// Read out of `BruState` rather than out of CEF: the state is what `on_address_change` and
/// `on_title_change` have been filling in all along, and asking the browser would mean a second
/// source that can disagree with the URL the status line is showing.
fn current_page(state: &SharedState) -> Option<(String, String)> {
    let state = state.lock().expect("state mutex poisoned");
    let index = state.active_tab();
    let url = state.tab_url(index).filter(|url| !url.is_empty())?;
    Some((url, state.tab_title(index).unwrap_or_default()))
}

/// `quickmark-save [name]` — `m`.
///
/// **qutebrowser prompts here and bru does not.** `m` opens a prompt-mode line asking for a name
/// (`urlmarks.py`'s `prompt_save` → `message.ask`), and prompt mode is a whole mode bru has not
/// built: its own key parser, its own chrome, and a modal stack under the mode manager. Rather than
/// build a one-off prompt that the real one would have to replace, `m` with no name prefills the
/// command line with `:quickmark-save `, which is the *same* machinery `b`, `B`, `gb`, `gB`, `wb`
/// and `wB` already use — every one of them is a `cmd-set-text -s :…`. So `m` now behaves like its
/// six siblings instead of like the one key that opens a different kind of line.
///
/// What is therefore **not** built, and is a prompt-mode milestone's: the confirmation qutebrowser
/// asks before overwriting an existing quickmark. `Data::quickmark_save` overwrites silently and
/// says which it did.
pub fn quickmark_save(state: &SharedState, name: Option<&str>) {
    let Some(name) = name else {
        crate::cmdline::cmd_set_text(":quickmark-save", true, false, false, None);
        return;
    };
    let Some((url, _)) = current_page(state) else {
        eprintln!("bru: quickmark-save: no page to save");
        return;
    };
    match with_data(|data| data.quickmark_save(name, &url)) {
        Some(Ok(replaced)) => eprintln!(
            "bru: {} quickmark {name} -> {url}",
            if replaced { "replaced" } else { "saved" }
        ),
        Some(Err(error)) => eprintln!("bru: quickmark-save: {error}"),
        None => {}
    }
}

/// `quickmark-load [-t|-b|-w] <name>` — what `b`, `B` and `wb` prefill the command line with.
pub fn quickmark_load(
    state: &SharedState,
    browser: &mut cef::Browser,
    name: Option<&str>,
    tab: bool,
    bg: bool,
    window: bool,
) {
    let Some(name) = name else {
        eprintln!("bru: quickmark-load: which quickmark?");
        return;
    };
    match with_data(|data| data.quickmark_load(name)) {
        // `wb` — a window of its own, not the tab it stood in for while bru had one window.
        Some(Ok(url)) if window => {
            crate::window::open(state, &url);
        }
        Some(Ok(url)) => crate::open::open(state, browser, Some(&url), tab, bg),
        Some(Err(error)) => eprintln!("bru: {error}"),
        None => {}
    }
}

/// `quickmark-del [name]`. With no name, the quickmark pointing at the current page — qutebrowser
/// picks one arbitrarily when several do (`commands.py:1225-1247`), and so does this.
pub fn quickmark_del(state: &SharedState, name: Option<&str>) {
    let name = match name {
        Some(name) => name.to_string(),
        None => {
            let Some((url, _)) = current_page(state) else {
                eprintln!("bru: quickmark-del: no page, and no name given");
                return;
            };
            let found = with_data(|data| {
                data.quickmarks().iter().find(|q| q.url == url).map(|q| q.name.clone())
            })
            .flatten();
            let Some(found) = found else {
                eprintln!("bru: quickmark-del: no quickmark for {url}");
                return;
            };
            found
        }
    };
    match with_data(|data| data.quickmark_del(&name)) {
        Some(Ok(true)) => eprintln!("bru: deleted quickmark {name}"),
        Some(Ok(false)) => eprintln!("bru: quickmark '{name}' does not exist"),
        Some(Err(error)) => eprintln!("bru: quickmark-del: {error}"),
        None => {}
    }
}

/// `bookmark-add [url] [title] [--toggle]` — `M`, which passes neither argument.
///
/// The default binding is a bare `bookmark-add` (`configdata.yml:3776`), **not** `--toggle`: `M` on
/// a page that is already bookmarked refreshes its title and says so, rather than removing it. Only
/// an explicit `:bookmark-add --toggle` removes.
pub fn bookmark_add(state: &SharedState, url: Option<&str>, title: Option<&str>, toggle: bool) {
    let (url, title) = match url {
        // A URL typed at `:bookmark-add` goes through the same fuzzy parse as `:open`
        // (`commands.py:1283`), so `:bookmark-add example.com Example` works.
        Some(url) => match crate::open::decide(url, &crate::open::engines()) {
            Some(target) => (target.url().to_string(), title.unwrap_or_default().to_string()),
            None => {
                eprintln!("bru: bookmark-add: nothing to bookmark in {url:?}");
                return;
            }
        },
        None => match current_page(state) {
            Some((url, page_title)) => (url, title.unwrap_or(&page_title).to_string()),
            None => {
                eprintln!("bru: bookmark-add: no page to bookmark");
                return;
            }
        },
    };

    match with_data(|data| data.bookmark_add(&url, &title, toggle)) {
        Some(Ok(true)) => eprintln!("bru: bookmarked {url}"),
        Some(Ok(false)) if toggle => eprintln!("bru: removed bookmark {url}"),
        Some(Ok(false)) => eprintln!("bru: bookmark {url} already exists"),
        Some(Err(error)) => eprintln!("bru: bookmark-add: {error}"),
        None => {}
    }
}

/// `bookmark-load [-t|-b|-w] [-d] <url>` — what `gb`, `gB` and `wB` prefill.
pub fn bookmark_load(
    state: &SharedState,
    browser: &mut cef::Browser,
    url: Option<&str>,
    tab: bool,
    bg: bool,
    window: bool,
    delete: bool,
) {
    let Some(url) = url else {
        eprintln!("bru: bookmark-load: which bookmark?");
        return;
    };
    // The completion inserts the bookmark's URL verbatim, so this is normally already a URL; the
    // fuzzy parse is what lets one be typed by hand (`commands.py:1310`).
    let Some(target) = crate::open::decide(url, &crate::open::engines()) else {
        eprintln!("bru: bookmark-load: nothing to open in {url:?}");
        return;
    };
    let target = target.url().to_string();
    // `wB` — a window of its own.
    if window {
        crate::window::open(state, &target);
    } else {
        crate::open::open(state, browser, Some(&target), tab, bg);
    }
    if delete {
        bookmark_del(state, Some(&target));
    }
}

/// `bookmark-del [url]`. With no URL, the current page's.
pub fn bookmark_del(state: &SharedState, url: Option<&str>) {
    let url = match url {
        Some(url) => url.to_string(),
        None => match current_page(state) {
            Some((url, _)) => url,
            None => {
                eprintln!("bru: bookmark-del: no page, and no URL given");
                return;
            }
        },
    };
    match with_data(|data| data.bookmark_del(&url)) {
        Some(Ok(true)) => eprintln!("bru: removed bookmark {url}"),
        Some(Ok(false)) => eprintln!("bru: bookmark '{url}' does not exist"),
        Some(Err(error)) => eprintln!("bru: bookmark-del: {error}"),
        None => {}
    }
}

/// `bookmark-list [--jump] [-b]` — `Sq` and `Sb`.
pub fn bookmark_list(state: &SharedState, browser: &mut cef::Browser, jump: bool, bg: bool) {
    let url = if jump { MARKS_URL_JUMP } else { MARKS_URL };
    crate::open::open(state, browser, Some(url), true, bg);
}

/// `history [-b]` — `Sh`.
pub fn show(state: &SharedState, browser: &mut cef::Browser, bg: bool) {
    crate::open::open(state, browser, Some(HISTORY_URL), true, bg);
}

/// The two addresses this module serves. `src/chrome.rs` maps the paths; these are what the commands
/// navigate to, spelled once so the two cannot drift apart.
pub const HISTORY_URL: &str = "bru://chrome/history";
const MARKS_URL: &str = "bru://chrome/bookmarks";
/// `bookmark-list --jump` — the fragment is the anchor on the Bookmarks heading, which is the whole
/// difference between `Sq` and `Sb`.
const MARKS_URL_JUMP: &str = "bru://chrome/bookmarks#bookmarks";

// --- bru://chrome/history -----------------------------------------------------------------------

/// The history page, as HTML. Served by `src/chrome.rs`, generated per request.
///
/// The database read and the rendering are separate so that the rendering can be tested: opening
/// the one `Data` means opening — and on a first run *creating* — the user's real
/// `~/.local/share/bru`, which no test may do.
pub fn history_page() -> String {
    match with_data(|data| {
        (
            data.recent_visits(PAGE_VISITS).unwrap_or_default(),
            data.history_counts().unwrap_or((0, 0)),
        )
    }) {
        Some((visits, counts)) => render_history(&visits, counts),
        None => format!(
            "{}<h1>History</h1>\n<p class=\"summary\">There is no history: bru could not open its data directory. The reason was printed once at startup.</p>\n</main>\n",
            head("bru — history", "history")
        ),
    }
}

fn render_history(visits: &[data::Visit], counts: (usize, usize)) -> String {
    let mut out = String::with_capacity(64 * 1024);
    out.push_str(&head("bru — history", "history"));

    let (logged, sites) = counts;
    out.push_str("<h1>History</h1>\n");
    if logged == 0 {
        // The state this whole module exists to end. Worth saying plainly rather than showing an
        // empty table that reads as a broken page.
        out.push_str("<p class=\"summary\">Nothing visited yet.</p>\n</main>\n");
        return out;
    }
    out.push_str(&format!(
        "<p class=\"summary\">{logged} visits to {sites} pages. {}</p>\n",
        if logged > visits.len() {
            format!("The most recent {} are below.", visits.len())
        } else {
            "All of them are below.".to_string()
        }
    ));

    // Grouped by local date, newest day first, which is how anyone looks for "the thing from
    // yesterday". The rows already arrive newest first, so a group breaks whenever the date changes
    // and nothing has to be sorted again.
    let mut day = String::new();
    let mut open = false;
    for visit in visits {
        if visit.date != day {
            if open {
                out.push_str("</table>\n");
            }
            out.push_str(&format!("<h2>{}</h2>\n<table>\n", escape(&visit.date)));
            day = visit.date.clone();
            open = true;
        }
        out.push_str(&format!(
            "<tr><td class=\"when\">{}</td><td class=\"what\"><a href=\"{}\">{}</a></td><td class=\"where\">{}</td></tr>\n",
            escape(&visit.time),
            escape_attr(&visit.url),
            escape(if visit.title.is_empty() { &visit.url } else { &visit.title }),
            escape(&visit.url),
        ));
    }
    if open {
        out.push_str("</table>\n");
    }
    out.push_str("</main>\n");
    out
}

// --- bru://chrome/bookmarks ---------------------------------------------------------------------

/// What `:bookmark-list` opens: quickmarks and bookmarks, in that order, each with an anchor so
/// `bookmark-list --jump` can land on the second.
pub fn marks_page() -> String {
    match with_data(|data| (data.quickmarks().to_vec(), data.bookmarks().to_vec())) {
        Some((quickmarks, bookmarks)) => render_marks(&quickmarks, &bookmarks),
        None => format!(
            "{}<h1>Marks</h1>\n<p class=\"summary\">bru could not open its data directory. The reason was printed once at startup.</p>\n</main>\n",
            head("bru — bookmarks", "bookmarks")
        ),
    }
}

fn render_marks(quickmarks: &[data::Quickmark], bookmarks: &[data::Bookmark]) -> String {
    let mut out = String::with_capacity(8 * 1024);
    out.push_str(&head("bru — bookmarks", "bookmarks"));
    out.push_str("<h1>Marks</h1>\n");

    out.push_str(&format!(
        "<p class=\"summary\">{} quickmarks, {} bookmarks, in <code>~/.local/share/bru/</code>.</p>\n",
        quickmarks.len(),
        bookmarks.len()
    ));

    out.push_str("<h2 id=\"quickmarks\">Quickmarks</h2>\n");
    if quickmarks.is_empty() {
        out.push_str("<p class=\"summary\">None yet. <code>m</code> saves one.</p>\n");
    } else {
        out.push_str("<table>\n");
        for mark in quickmarks {
            out.push_str(&format!(
                "<tr><td class=\"when\">{}</td><td class=\"what\"><a href=\"{}\">{}</a></td><td class=\"where\"></td></tr>\n",
                escape(&mark.name),
                escape_attr(&mark.url),
                escape(&mark.url),
            ));
        }
        out.push_str("</table>\n");
    }

    out.push_str("<h2 id=\"bookmarks\">Bookmarks</h2>\n");
    if bookmarks.is_empty() {
        out.push_str("<p class=\"summary\">None yet. <code>M</code> saves one.</p>\n");
    } else {
        out.push_str("<table>\n");
        for mark in bookmarks {
            out.push_str(&format!(
                "<tr><td class=\"when\"></td><td class=\"what\"><a href=\"{}\">{}</a></td><td class=\"where\">{}</td></tr>\n",
                escape_attr(&mark.url),
                escape(if mark.title.is_empty() { &mark.url } else { &mark.title }),
                escape(&mark.url),
            ));
        }
        out.push_str("</table>\n");
    }

    out.push_str("</main>\n");
    out
}

/// The head both pages share.
///
/// `theme.css` is linked and `chrome.css` is not — see the module comment. The rules below are the
/// help page's, narrowed to what a two- or three-column list needs, and not one colour is written
/// here: every one is a `var(--…)` the theme defines, which is `chrome/chrome.css`'s own rule and
/// the reason switching the theme is swapping one file.
fn head(title: &str, view: &str) -> String {
    format!(
        r#"<!doctype html>
<meta charset="utf-8">
<title>{}</title>
<link rel="stylesheet" href="theme.css">
<style>
* {{ box-sizing: border-box; }}
html, body {{ margin: 0; padding: 0; }}
body {{
    font-family: monospace;
    font-size: 13px;
    background: var(--bg);
    color: var(--fg);
}}
main {{ max-width: 70rem; margin: 0 auto; padding: 2rem 1.5rem 4rem; }}
h1 {{ margin: 0 0 0.25rem; font-size: 1.6rem; color: var(--completion-category-fg); }}
h2 {{
    margin: 2rem 0 0.5rem;
    font-size: 1rem;
    color: var(--completion-category-fg);
    border-bottom: 1px solid var(--completion-category-border-bottom);
    padding-bottom: 0.25rem;
    scroll-margin-top: 1rem;
}}
.summary {{ margin: 0 0 1rem; color: var(--comment); }}
table {{ width: 100%; border-collapse: collapse; table-layout: fixed; }}
tr:nth-child(odd)  {{ background: var(--completion-odd-bg); }}
tr:nth-child(even) {{ background: var(--completion-even-bg); }}
td {{ padding: 0.15rem 0.5rem; vertical-align: top; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }}
/* The timestamp is the column the eye scans down, so it leads and it is fixed. */
.when  {{ width: 14ch; color: var(--completion-match-fg); }}
.what  {{ width: 45%; }}
.where {{ color: var(--comment); }}
a {{ color: var(--fg); text-decoration: none; }}
a:hover {{ text-decoration: underline; }}
code {{ color: var(--completion-match-fg); }}
</style>
<body data-view="{}">
<main>
"#,
        escape(title),
        escape_attr(view),
    )
}

// --- src/utilcmds.rs -------------------------------------------------------
/// [`head`] and [`escape`], for the pages `src/utilcmds.rs` generates — `bru://chrome/version`,
/// `/messages` and `/process`.
///
/// Two wrappers rather than making the originals public: the stylesheet above is what makes every
/// generated `bru://` page look like the same browser, and a second copy of it in another module
/// would drift the first time a colour moved. Everything here writes `var(--…)` and not one colour,
/// which is `chrome/chrome.css`'s rule and the reason swapping a theme is swapping one file.
pub fn chrome_head(title: &str, view: &str) -> String {
    head(title, view)
}

pub fn chrome_escape(s: &str) -> String {
    escape(s)
}
// --- end src/utilcmds.rs ---------------------------------------------------

/// Text going into an element. A page title is the page's own, and a URL is whatever a site put in
/// the address bar; both are one `<` away from becoming markup.
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

/// The same, for a value going inside `href="…"`, where a single quote closes nothing but is
/// escaped anyway so the function is safe wherever it is used next.
fn escape_attr(s: &str) -> String {
    escape(s).replace('\'', "&#39;")
}

// --- the debug hook -----------------------------------------------------------------------------

/// `--history-script='counts;completion:exa;page:history' --history-step-ms=1000` runs from posted
/// UI tasks against the database the running browser is actually writing to.
///
/// It exists for one thing this project's usual harnesses cannot reach. `--cmd` runs commands and
/// `--cmdline-script` drives the command line, but **neither can show what the completion contains**:
/// `ipc::set_completion_for` is called only from the chrome's own `text-changed` query, and the
/// script's `type:` step does not go through it. `--cmdline-script`'s `key:` step does — it injects a
/// real key at the bottom strip and the chrome reports back — but what that produces is *pixels*, and
/// pixels cannot be photographed dependably here: eleven agents share this compositor and a
/// whole-screen `grim` catches whichever bru happens to be tiled on top. Measured 2026-08-06, three
/// times, and each frame held another workstream's window.
///
/// So this prints the model instead — the same two calls `ipc::set_completion_for` makes, in the same
/// order, on the same data — and `--cmdline-script`'s `dump` proves the typing reached the input that
/// triggers them. Inert unless the switch is passed.
///
/// | | |
/// |---|---|
/// | `counts` | rows in `History` and in `CompletionHistory` |
/// | `completion:<pattern>` | the categories `:open <pattern>` offers, as the chrome is handed them |
/// | `page:history` / `page:marks` | the generated `bru://` page, headings and row count |
pub fn schedule_script(steps: &str, interval_ms: i64) {
    for (i, step) in steps.split(';').filter(|s| !s.is_empty()).enumerate() {
        let mut task = ScriptStep::new(step.to_string());
        post_delayed_task(
            ThreadId::UI,
            Some(&mut task),
            interval_ms * (i as i64 + 1),
        );
    }
}

wrap_task! {
    struct ScriptStep {
        step: String,
    }

    impl Task {
        fn execute(&self) {
            let (verb, arg) = match self.step.split_once(':') {
                Some((verb, arg)) => (verb, arg),
                None => (self.step.as_str(), ""),
            };
            match verb {
                "counts" => match with_data(|data| data.history_counts()) {
                    Some(Ok((visits, sites))) => {
                        eprintln!("history-script: {visits} visits over {sites} pages")
                    }
                    Some(Err(error)) => eprintln!("history-script: {error}"),
                    None => eprintln!("history-script: no data directory"),
                },
                // The two calls `ipc::set_completion_for` makes, on the same data, in the same order.
                "completion" => {
                    let categories = crate::completion::categories(arg);
                    let selected = if categories.is_empty() { None } else { Some((0, 0)) };
                    eprintln!(
                        "history-script: `:open {arg}` offers {} -> {}",
                        categories
                            .iter()
                            .map(|c| format!("{} ({})", c.name, c.items.len()))
                            .collect::<Vec<_>>()
                            .join(", "),
                        crate::completion::to_json(&categories, selected),
                    );
                }
                "page" => {
                    let html = match arg {
                        "marks" => marks_page(),
                        _ => history_page(),
                    };
                    let headings: Vec<&str> = html
                        .match_indices("<h2")
                        .map(|(at, _)| {
                            let rest = &html[at..];
                            let start = rest.find('>').map(|i| at + i + 1).unwrap_or(at);
                            let end = html[start..].find("</h2>").map(|i| start + i).unwrap_or(start);
                            &html[start..end]
                        })
                        .collect();
                    eprintln!(
                        "history-script: {arg} page is {} bytes, {} rows, headings [{}]",
                        html.len(),
                        html.matches("<tr>").count(),
                        headings.join(", "),
                    );
                }
                other => eprintln!("history-script: no step named {other:?}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Nothing in this module's tests opens the one `Data`.** `data::instance` opens — and on a
    /// first run creates — the user's real `~/.local/share/bru`, and STAGE3-CONTRACTS is explicit
    /// that a test may not touch it. So [`visited`] and [`retitled`] are exercised only through
    /// [`remember`] and [`last_url`], which is the whole of their logic that is not `data.rs`'s, and
    /// the pages are exercised through the renderers with rows written here.
    fn visit(url: &str, title: &str, date: &str, time: &str) -> data::Visit {
        data::Visit {
            url: url.to_string(),
            title: title.to_string(),
            date: date.to_string(),
            time: time.to_string(),
        }
    }

    #[test]
    fn the_history_page_groups_by_day_newest_first() {
        let visits = [
            visit("https://a.com/", "A", "2026-08-06", "14:02"),
            visit("https://b.com/", "B", "2026-08-06", "09:11"),
            visit("https://c.com/", "C", "2026-08-05", "23:40"),
        ];
        let html = render_history(&visits, (3, 3));

        assert!(html.starts_with("<!doctype html>"));
        assert!(html.contains("<title>bru — history</title>"));
        assert!(html.ends_with("</main>\n"));
        assert_eq!(html.matches("<tr>").count(), 3, "one row per visit");
        // Two days, so two tables — a single table would put yesterday under today's heading.
        assert_eq!(html.matches("<table>").count(), 2);
        assert_eq!(html.matches("</table>").count(), 2);
        let today = html.find("<h2>2026-08-06</h2>").expect("today's heading");
        let yesterday = html.find("<h2>2026-08-05</h2>").expect("yesterday's heading");
        assert!(today < yesterday, "the newest day has to come first");
        assert!(html.contains("<td class=\"when\">14:02</td>"));
        assert!(html.contains("<a href=\"https://a.com/\">A</a>"));
    }

    /// The summary line is what says the page is a window onto the log rather than the whole of it.
    #[test]
    fn the_history_page_says_how_much_it_is_not_showing() {
        let visits = [visit("https://a.com/", "A", "2026-08-06", "14:02")];
        assert!(render_history(&visits, (1, 1)).contains("All of them are below."));
        assert!(render_history(&visits, (9000, 1200)).contains("9000 visits to 1200 pages."));
        assert!(render_history(&visits, (9000, 1200)).contains("The most recent 1 are below."));
    }

    /// The state this whole module exists to end, and the one a user sees on a first run.
    #[test]
    fn an_empty_history_says_so_instead_of_drawing_an_empty_table() {
        let html = render_history(&[], (0, 0));
        assert!(html.contains("Nothing visited yet."));
        assert!(!html.contains("<table>"));
    }

    /// A visit with no title falls back to its URL, or the row would be a blank link.
    #[test]
    fn a_titleless_visit_still_has_something_to_click() {
        let visits = [visit("https://a.com/", "", "2026-08-06", "14:02")];
        assert!(render_history(&visits, (1, 1)).contains("<a href=\"https://a.com/\">https://a.com/</a>"));
    }

    #[test]
    fn the_marks_page_has_the_anchor_jump_needs() {
        let quickmarks = [data::Quickmark { name: "go".into(), url: "https://www.google.com/".into() }];
        let bookmarks = [data::Bookmark { url: "https://example.com/".into(), title: "Example".into() }];
        let html = render_marks(&quickmarks, &bookmarks);

        assert!(html.contains("<h2 id=\"quickmarks\">Quickmarks</h2>"));
        // The anchor `bookmark-list --jump` lands on. Without it `Sb` and `Sq` are the same key.
        assert!(html.contains("<h2 id=\"bookmarks\">Bookmarks</h2>"));
        assert!(html.contains("1 quickmarks, 1 bookmarks"));
        assert!(html.contains("<td class=\"when\">go</td>"));
        assert!(html.contains("<a href=\"https://example.com/\">Example</a>"));

        let empty = render_marks(&[], &[]);
        assert!(empty.contains("<code>m</code> saves one"));
        assert!(empty.contains("<code>M</code> saves one"));
    }

    /// A page title is written by whoever owns the site, and a URL is whatever a site put in the
    /// address bar. Both reach these pages.
    #[test]
    fn markup_in_a_title_cannot_escape_its_cell() {
        let visits = [visit(
            "https://a.com/?q=<img>",
            "<script>alert(1)</script>",
            "2026-08-06",
            "14:02",
        )];
        let html = render_history(&visits, (1, 1));
        assert!(!html.contains("<script>alert(1)"), "it must arrive as text");
        assert!(html.contains("&lt;script&gt;alert(1)"));
        assert!(!html.contains("?q=<img>"));

        assert_eq!(escape_attr("\" onload=\"x"), "&quot; onload=&quot;x");
        assert_eq!(escape("a & b"), "a &amp; b");
    }

    /// The whole point of keying by browser identifier rather than by tab index: two tabs loading at
    /// once must not hand each other's titles to the database, and `gJ` reordering the strip must
    /// not either.
    #[test]
    fn a_title_goes_to_the_url_its_own_browser_is_on() {
        remember(9001, "https://example.com/one");
        remember(9002, "https://example.com/two");
        assert_eq!(last_url(9001).as_deref(), Some("https://example.com/one"));
        assert_eq!(last_url(9002).as_deref(), Some("https://example.com/two"));

        // A second navigation replaces the entry rather than adding one.
        remember(9001, "https://example.com/three");
        assert_eq!(last_url(9001).as_deref(), Some("https://example.com/three"));
        assert_eq!(last_url(9002).as_deref(), Some("https://example.com/two"));
        assert_eq!(last_url(9003), None);
    }

    /// Every load fires `on_title_change` with the address before the document has a `<title>`.
    /// Measured on this machine — the run in the report shows `example.com` arriving as a title for
    /// `https://example.com/` before "Example Domain" does.
    #[test]
    fn chromiums_stand_in_title_is_not_a_title() {
        for (url, title) in [
            ("https://example.com/", ""),
            ("https://example.com/", "https://example.com/"),
            ("https://example.com/", "example.com"),
            ("https://example.com/a/b", "example.com/a/b"),
            ("http://example.com/", "example.com"),
        ] {
            assert!(is_placeholder_title(url, title), "{title:?} for {url} is a stand-in");
        }
        for (url, title) in [
            ("https://example.com/", "Example Domain"),
            ("https://example.com/a/b", "example.com/a"),
            ("https://news.ycombinator.com/", "Hacker News"),
        ] {
            assert!(!is_placeholder_title(url, title), "{title:?} for {url} is a real title");
        }
    }
}
