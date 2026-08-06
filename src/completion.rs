//! The completion model: which items `:open <text>` offers, in which order, and which parts of
//! each one matched.
//!
//! No UI and no disk. The chrome renders what [`to_json`] emits; the files under
//! `~/.local/share/bru/` belong to `src/data.rs`, which hands them over through [`Sources`].
//!
//! Every rule here is qutebrowser 3.7.0's, cited to
//! `/usr/lib/python3.13/site-packages/qutebrowser/completion/`:
//!
//! - **Category order and headers** — `models/urlmodel.py:63-79`: Search engines, Quickmarks,
//!   Bookmarks, History, in that order, with those exact names.
//! - **Columns** — `urlmodel.py:52-58`. Search engines are `(name, url-template)`; quickmarks are
//!   `(url, name)`; bookmarks are `(url, title)`; history is `(url, title, atime)`
//!   (`models/histcategory.py:97`). Column 0 is what Tab inserts into the command line
//!   (`completer.py:162-192`), which is why the engine's *name* comes first and everything else
//!   leads with its URL.
//! - **Two filters, not one.** qutebrowser uses a different rule for the list categories than for
//!   history, and bru keeps both:
//!   - list categories (`models/listcategory.py:55-59`): the text is split on whitespace and turned
//!     into `^(?=.*a)(?=.*b)`, case-insensitively, against every column
//!     (`setFilterKeyColumn(-1)`, :34) — so all terms have to be found, in any order, but **within
//!     one single column**.
//!   - history (`models/histcategory.py:75-83`): the text is split on `' '` and each word becomes
//!     `(url LIKE '%w%' OR title LIKE '%w%')`, AND-ed — so each term may land on **either** field
//!     independently.
//! - **Ordering** — history is `ORDER BY last_atime DESC` (`histcategory.py:103`); the list
//!   categories get `sort=False` (`urlmodel.py:65,70,73`) and so keep source order, except that
//!   `lessThan` (`listcategory.py:90-100`) floats every item whose column 0 starts with the whole
//!   raw pattern — case-sensitively — to the front. Qt sorts with `std::stable_sort`, so that is a
//!   stable partition, not a reshuffle.
//! - **Highlighting** — `completiondelegate.py:30-34`: the terms are whitespace-split and joined
//!   into one alternation, and *every* occurrence of any of them is marked, case-insensitively.
//!
//! Two things here are deliberately not qutebrowser's, and both are stated rather than hidden:
//!
//! 1. **Case folding is Unicode, not ASCII.** SQLite's `LIKE` folds ASCII only, so qutebrowser's
//!    history completion cannot match `СТРАНИЦА` from `страница`. Folding with
//!    [`char::to_lowercase`] only ever adds matches, never removes one, and this user reads
//!    Cyrillic.
//! 2. **A category with no matching item is dropped** rather than shown as a bare header.
//!    `urlmodel.py:63,67,71` already drops a category whose *source* is empty, and the chrome
//!    collapses the bar through `#completion:empty` (`chrome/chrome.css:198`), which an always-four-
//!    headers payload would defeat.

use std::sync::OnceLock;

/// How many items each category offers at most.
///
/// Measured, not guessed: `chrome/chrome.css:49-50` sets `--row-h: 20px` and
/// `--completion-max-h: 300px`, so fifteen rows are on screen at once and four of those go to the
/// category headers — eleven visible items. 25 is a little over two screenfuls inside any one
/// category, which is as far as anyone scrolls before retyping, and it bounds the worst-case
/// payload at four headers and a hundred items instead of at the size of the history table.
pub const MAX_ITEMS: usize = 25;

/// A category as the chrome draws it: a coloured header and its rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Category {
    /// One of "Search engines", "Quickmarks", "Bookmarks", "History".
    pub name: &'static str,
    pub items: Vec<Item>,
}

/// One row. `cols` is two or three columns, per `chrome/chrome.css:182`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub cols: Vec<String>,
    /// `(start, len)` ranges into `cols[0]`, so the chrome highlights without re-running the
    /// filter. **Offsets are UTF-16 code units**, which is what JavaScript's `String.substring`
    /// counts — byte offsets would cut a non-ASCII title in half in the renderer.
    pub matches: Vec<(usize, usize)>,
}

/// One row of `CompletionHistory(url PRIMARY KEY, title, last_atime)`.
///
/// `atime` arrives already formatted, because that is where qutebrowser formats it too — in the
/// query, `strftime('%Y-%m-%d %H:%M', last_atime, 'unixepoch', 'localtime')`
/// (`histcategory.py:86-88`, format default at `config/configdata.yml:1406`). SQLite knows the
/// local timezone; this module would have to be told it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryRow<'a> {
    pub url: &'a str,
    pub title: &'a str,
    pub atime: &'a str,
}

/// Whether [`Sources::history`] should keep walking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Continue,
    Stop,
}

/// What this module needs from `src/data.rs`. Implemented there, installed once by `main`.
///
/// The three small lists are returned by value: there are tens of them, and cloning tens of short
/// strings per keystroke costs nothing. History is thousands of rows and is walked instead, so a
/// keystroke never copies the table — and because the walk is in `last_atime` order the filter can
/// stop at the first [`MAX_ITEMS`] hits.
pub trait Sources: Send + Sync {
    /// `(name, url-template)`, sorted by name, without the `DEFAULT` entry (`urlmodel.py:57-59`).
    /// From `config.lua`, per DESIGN.md — search engines are config, not a data file.
    fn search_engines(&self) -> Vec<(String, String)>;

    /// `(url, name)` — that order, note (`urlmodel.py:54-55`). From `~/.local/share/bru/quickmarks`.
    fn quickmarks(&self) -> Vec<(String, String)>;

    /// `(url, title)`. From `~/.local/share/bru/bookmarks`.
    fn bookmarks(&self) -> Vec<(String, String)>;

    /// Walks matching `CompletionHistory` rows in **`last_atime` descending order** — that
    /// ordering is the contract, and the index on `last_atime` is what makes it free. Stops early
    /// when `visit` returns [`Flow::Stop`].
    ///
    /// `pattern` is the raw text after `:open `, passed down so the implementation can filter in
    /// SQL. That is where qutebrowser filters history too (`histcategory.py:75-83`), and it is
    /// measurably the right place: filtering 9,000 rows in SQL costs 0.74 ms on the worst realistic
    /// keystroke, against walking every row into Rust. The walk below still applies the same rule,
    /// which makes an implementation that ignores `pattern` correct but slow — the test fixtures do
    /// exactly that.
    fn history(&self, pattern: &str, visit: &mut dyn FnMut(HistoryRow<'_>) -> Flow);
}

static SOURCES: OnceLock<Box<dyn Sources>> = OnceLock::new();

/// Hands `src/data.rs`'s implementation to this module, once, at startup. Later calls are ignored.
pub fn install(sources: Box<dyn Sources>) {
    let _ = SOURCES.set(sources);
}

/// The promised entry point: what `:open <text>` should offer.
///
/// Before [`install`] has run there is no data and the answer is nothing — the completion bar is
/// simply closed, which is the right behaviour for a startup with no history file yet.
pub fn categories(text: &str) -> Vec<Category> {
    match SOURCES.get() {
        Some(sources) => categories_from(text, sources.as_ref()),
        None => Vec::new(),
    }
}

/// [`categories`] against an explicit source. This is what the tests drive, and what a caller with
/// its own source uses.
pub fn categories_from(text: &str, sources: &dyn Sources) -> Vec<Category> {
    // Whitespace-split terms drive the list-category filter (listcategory.py:55) and all of the
    // highlighting (completiondelegate.py:30).
    let terms = split_whitespace(text);
    // History splits on a literal space instead (histcategory.py:75), so "a  b" really does produce
    // an empty word there, which becomes '%%' and matches everything.
    let hist_terms = split_space(text);

    let mut out = Vec::with_capacity(4);

    push_list(
        &mut out,
        "Search engines",
        sources.search_engines(),
        text,
        &terms,
    );
    push_list(&mut out, "Quickmarks", sources.quickmarks(), text, &terms);
    push_list(&mut out, "Bookmarks", sources.bookmarks(), text, &terms);

    let mut items: Vec<Item> = Vec::new();
    sources.history(text, &mut |row| {
        // Each term on url OR title, independently (histcategory.py:81-83).
        let url = fold(row.url);
        let title = fold(row.title);
        let hit = hist_terms
            .iter()
            .all(|t| url.lower.contains(t.as_str()) || title.lower.contains(t.as_str()));
        if hit {
            items.push(Item {
                matches: match_ranges(row.url, &terms),
                cols: vec![
                    row.url.to_string(),
                    row.title.to_string(),
                    row.atime.to_string(),
                ],
            });
        }
        // The walk is already in last_atime DESC order, so the first MAX_ITEMS hits are the newest
        // MAX_ITEMS hits and there is nothing left to look at.
        if items.len() >= MAX_ITEMS {
            Flow::Stop
        } else {
            Flow::Continue
        }
    });
    if !items.is_empty() {
        out.push(Category {
            name: "History",
            items,
        });
    }

    out
}

/// Serialises for the chrome, as STAGE2-CONTRACTS.md spells it:
/// `{categories:[{name, items:[{cols, match}]}], selected:[cat, item]}`.
///
/// `selected` is `null` when nothing is selected. Selection lives in Rust, never in the chrome.
pub fn to_json(cats: &[Category], selected: Option<(usize, usize)>) -> String {
    let mut out = String::from("{\"categories\":[");
    for (i, cat) in cats.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("{\"name\":\"");
        out.push_str(&crate::ipc::json_escape(cat.name));
        out.push_str("\",\"items\":[");
        for (j, item) in cat.items.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            out.push_str("{\"cols\":[");
            for (k, col) in item.cols.iter().enumerate() {
                if k > 0 {
                    out.push(',');
                }
                out.push('"');
                out.push_str(&crate::ipc::json_escape(col));
                out.push('"');
            }
            out.push_str("],\"match\":[");
            for (k, (start, len)) in item.matches.iter().enumerate() {
                if k > 0 {
                    out.push(',');
                }
                out.push_str(&format!("[{start},{len}]"));
            }
            out.push_str("]}");
        }
        out.push_str("]}");
    }
    out.push_str("],\"selected\":");
    match selected {
        Some((cat, item)) => out.push_str(&format!("[{cat},{item}]")),
        None => out.push_str("null"),
    }
    out.push('}');
    out
}

/// A list category: search engines, quickmarks, bookmarks.
fn push_list(
    out: &mut Vec<Category>,
    name: &'static str,
    rows: Vec<(String, String)>,
    raw: &str,
    terms: &[String],
) {
    // urlmodel.py:63,67,71 — a category with no source at all is not added.
    if rows.is_empty() {
        return;
    }

    let mut kept: Vec<(String, String)> = rows
        .into_iter()
        .filter(|(a, b)| list_matches(&[a.as_str(), b.as_str()], terms))
        .collect();

    // listcategory.py:90-100 with _sort false: a stable partition putting everything whose column 0
    // starts with the whole raw pattern first. startswith is Python's, so it is case-sensitive and
    // it does not split on spaces.
    kept.sort_by_key(|(a, _)| !a.starts_with(raw));

    let items: Vec<Item> = kept
        .into_iter()
        .take(MAX_ITEMS)
        .map(|(a, b)| Item {
            matches: match_ranges(&a, terms),
            cols: vec![a, b],
        })
        .collect();

    if !items.is_empty() {
        out.push(Category { name, items });
    }
}

/// All terms inside one and the same column — `setFilterKeyColumn(-1)` plus a lookahead chain
/// (listcategory.py:34,55). An empty term list is `^`, which matches anything.
fn list_matches(cols: &[&str], terms: &[String]) -> bool {
    cols.iter().any(|col| {
        let folded = fold(col);
        terms.iter().all(|t| folded.lower.contains(t.as_str()))
    })
}

/// Python's `str.split()`: runs of whitespace, no empty pieces. Lowercased for folding.
fn split_whitespace(text: &str) -> Vec<String> {
    text.split_whitespace().map(|w| w.to_lowercase()).collect()
}

/// Python's `str.split(' ')`: a literal single space, empty pieces kept. `""` gives `[""]`, which is
/// LIKE `'%%'` and matches everything — the same as an empty filter.
fn split_space(text: &str) -> Vec<String> {
    text.split(' ').map(|w| w.to_lowercase()).collect()
}

/// A lowercased copy plus the way back: `map[i]` is the byte offset, in the original, of the
/// character that produced byte `i` of `lower`. Needed because lowercasing can change a string's
/// length (`İ` is one character and lowercases to two), so an offset found in the folded copy is
/// not an offset into the original.
struct Folded {
    lower: String,
    map: Vec<usize>,
}

fn fold(s: &str) -> Folded {
    let mut lower = String::with_capacity(s.len());
    let mut map = Vec::with_capacity(s.len() + 1);
    for (i, c) in s.char_indices() {
        let before = lower.len();
        for lc in c.to_lowercase() {
            lower.push(lc);
        }
        for _ in before..lower.len() {
            map.push(i);
        }
    }
    map.push(s.len());
    Folded { lower, map }
}

/// Every occurrence of any term in `col`, case-insensitively, as `(start, len)` in **UTF-16 code
/// units** — completiondelegate.py:30-34 alternates the terms and highlights each hit.
fn match_ranges(col: &str, terms: &[String]) -> Vec<(usize, usize)> {
    let terms: Vec<&String> = terms.iter().filter(|t| !t.is_empty()).collect();
    if terms.is_empty() {
        return Vec::new();
    }

    let folded = fold(col);
    let mut bytes: Vec<(usize, usize)> = Vec::new();
    let mut p = 0usize;
    while p < folded.lower.len() {
        if !folded.lower.is_char_boundary(p) {
            p += 1;
            continue;
        }
        // Alternation order is the order the terms were written, as in the regex.
        let hit = terms
            .iter()
            .find(|t| folded.lower[p..].starts_with(t.as_str()))
            .map(|t| t.len());
        match hit {
            Some(len) => {
                bytes.push((folded.map[p], folded.map[p + len]));
                p += len;
            }
            None => p += 1,
        }
    }

    // Byte offsets in the original are not what the renderer counts.
    let units = utf16_offsets(col);
    bytes
        .into_iter()
        .map(|(a, b)| {
            let (ua, ub) = (utf16_at(&units, a), utf16_at(&units, b));
            (ua, ub - ua)
        })
        .collect()
}

/// `(byte offset, UTF-16 offset)` for every character boundary, plus the end of the string.
fn utf16_offsets(s: &str) -> Vec<(usize, usize)> {
    let mut v = Vec::with_capacity(s.len() + 1);
    let mut u = 0usize;
    for (i, c) in s.char_indices() {
        v.push((i, u));
        u += c.len_utf16();
    }
    v.push((s.len(), u));
    v
}

fn utf16_at(units: &[(usize, usize)], byte: usize) -> usize {
    match units.binary_search_by_key(&byte, |(b, _)| *b) {
        Ok(i) => units[i].1,
        // Only reachable if a byte offset landed mid-character, which fold's map prevents; take the
        // boundary before it rather than panicking in the middle of a keystroke.
        Err(i) => units[i.saturating_sub(1)].1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In-memory stand-in for `src/data.rs`. History is held newest-first, which is the ordering
    /// the trait requires of the real sqlite walk.
    struct Fixture {
        engines: Vec<(String, String)>,
        quickmarks: Vec<(String, String)>,
        bookmarks: Vec<(String, String)>,
        history: Vec<(String, String, String)>,
    }

    fn pairs(rows: &[(&str, &str)]) -> Vec<(String, String)> {
        rows.iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    impl Fixture {
        /// Deliberately small and deliberately awkward: a title-only hit, a url-only hit, mixed
        /// case, and a Cyrillic title.
        fn new() -> Self {
            Fixture {
                engines: pairs(&[
                    ("duckduckgo", "https://duckduckgo.com/?q={}"),
                    ("google", "https://www.google.com/search?q={}"),
                    ("wiki", "https://en.wikipedia.org/w/index.php?search={}"),
                ]),
                quickmarks: pairs(&[
                    ("https://www.google.com/", "go"),
                    ("https://duckduckgo.com/", "du"),
                    ("https://news.ycombinator.com/", "hn"),
                ]),
                bookmarks: pairs(&[
                    ("https://doc.rust-lang.org/std/", "Rust standard library"),
                    ("https://github.com/tauri-apps/cef-rs", "cef-rs"),
                ]),
                history: [
                    // Newest first.
                    (
                        "https://duckduckgo.com/?q=rust",
                        "rust at DuckDuckGo",
                        "2026-08-06 13:51",
                    ),
                    (
                        "https://example.com/a",
                        "Всяка страница",
                        "2026-08-06 13:40",
                    ),
                    (
                        "https://doc.rust-lang.org/std/vec/",
                        "std::vec - Rust",
                        "2026-08-05 09:02",
                    ),
                    ("https://news.ycombinator.com/", "Hacker News", "2026-08-04 21:11"),
                    ("https://DUCKling.example/", "nothing here", "2026-08-01 08:00"),
                ]
                .iter()
                .map(|(a, b, c)| (a.to_string(), b.to_string(), c.to_string()))
                .collect(),
            }
        }
    }

    impl Sources for Fixture {
        fn search_engines(&self) -> Vec<(String, String)> {
            self.engines.clone()
        }
        fn quickmarks(&self) -> Vec<(String, String)> {
            self.quickmarks.clone()
        }
        fn bookmarks(&self) -> Vec<(String, String)> {
            self.bookmarks.clone()
        }
        fn history(&self, _pattern: &str, visit: &mut dyn FnMut(HistoryRow<'_>) -> Flow) {
            for (url, title, atime) in &self.history {
                let flow = visit(HistoryRow {
                    url,
                    title,
                    atime: atime.as_str(),
                });
                if flow == Flow::Stop {
                    return;
                }
            }
        }
    }

    fn run(text: &str) -> Vec<Category> {
        categories_from(text, &Fixture::new())
    }

    fn names(cats: &[Category]) -> Vec<&str> {
        cats.iter().map(|c| c.name).collect()
    }

    fn col0(cats: &[Category], name: &str) -> Vec<String> {
        cats.iter()
            .find(|c| c.name == name)
            .map(|c| c.items.iter().map(|i| i.cols[0].clone()).collect())
            .unwrap_or_default()
    }

    // ---- category order and headers, urlmodel.py:63-79 ----

    #[test]
    fn empty_text_offers_every_category_in_qutebrowsers_order() {
        let cats = run("");
        assert_eq!(
            names(&cats),
            ["Search engines", "Quickmarks", "Bookmarks", "History"]
        );
        // Nothing is filtered out and nothing is highlighted.
        assert_eq!(cats[0].items.len(), 3);
        assert_eq!(cats[3].items.len(), 5);
        assert!(cats.iter().all(|c| c.items.iter().all(|i| i.matches.is_empty())));
    }

    #[test]
    fn columns_are_the_ones_the_chrome_draws() {
        let cats = run("");
        // Search engines lead with the name, everything else with the url (urlmodel.py:52-58).
        assert_eq!(
            cats[0].items[0].cols,
            ["duckduckgo", "https://duckduckgo.com/?q={}"]
        );
        assert_eq!(cats[1].items[0].cols, ["https://www.google.com/", "go"]);
        assert_eq!(
            cats[2].items[1].cols,
            ["https://github.com/tauri-apps/cef-rs", "cef-rs"]
        );
        // History is three columns, the third being the formatted atime (histcategory.py:97).
        assert_eq!(
            cats[3].items[0].cols,
            [
                "https://duckduckgo.com/?q=rust",
                "rust at DuckDuckGo",
                "2026-08-06 13:51"
            ]
        );
    }

    #[test]
    fn a_category_with_nothing_in_it_is_dropped() {
        let cats = run("zzzznothing");
        assert!(cats.is_empty(), "got {:?}", names(&cats));
    }

    // ---- the fuzzy rule ----

    #[test]
    fn every_term_must_match_not_just_one() {
        // "rust" is in the url and the title; "vec" is only in this one.
        assert_eq!(
            col0(&run("rust vec"), "History"),
            ["https://doc.rust-lang.org/std/vec/"]
        );
        // The same first term alone reaches a second row, so the second term is doing real work.
        assert_eq!(
            col0(&run("rust"), "History"),
            [
                "https://duckduckgo.com/?q=rust",
                "https://doc.rust-lang.org/std/vec/"
            ]
        );
    }

    #[test]
    fn a_term_may_match_the_title_alone() {
        // histcategory.py:81-83 — url LIKE OR title LIKE.
        assert_eq!(
            col0(&run("hacker"), "History"),
            ["https://news.ycombinator.com/"]
        );
    }

    #[test]
    fn a_term_may_match_the_url_alone() {
        assert_eq!(
            col0(&run("ycombinator"), "History"),
            ["https://news.ycombinator.com/"]
        );
    }

    #[test]
    fn terms_may_land_on_different_fields_in_history() {
        // "ycombinator" is url-only, "hacker" is title-only; together they still match, because
        // each word gets its own OR (histcategory.py:81-83).
        assert_eq!(
            col0(&run("ycombinator hacker"), "History"),
            ["https://news.ycombinator.com/"]
        );
    }

    #[test]
    fn a_list_category_needs_all_terms_in_one_column() {
        // listcategory.py:34 filters column by column, so "cef-rs" (column 1) and "github"
        // (column 0) never meet, even though each matches on its own.
        assert_eq!(col0(&run("github"), "Bookmarks").len(), 1);
        assert_eq!(col0(&run("cef-rs"), "Bookmarks").len(), 1);
        assert!(run("github standard").iter().all(|c| c.name != "Bookmarks"));
    }

    #[test]
    fn matching_is_case_insensitive() {
        // sqlite LIKE folds ASCII (histcategory.py:79) and Qt's regex is CaseInsensitiveOption
        // (listcategory.py:57).
        assert_eq!(col0(&run("DUCK"), "History"), col0(&run("duck"), "History"));
        assert_eq!(col0(&run("DuCkDuCkGo"), "Search engines"), ["duckduckgo"]);
        assert_eq!(col0(&run("HACKER"), "History").len(), 1);
    }

    #[test]
    fn case_folding_reaches_beyond_ascii() {
        // The stated divergence: sqlite's LIKE could not do this one.
        assert_eq!(
            col0(&run("СТРАНИЦА"), "History"),
            ["https://example.com/a"]
        );
    }

    #[test]
    fn a_term_matching_nothing_kills_the_whole_row() {
        assert!(run("rust zzzz").is_empty());
    }

    #[test]
    fn repeated_spaces_are_not_a_term() {
        // histcategory.py:75 splits on a literal ' ', so the empty word is LIKE '%%' and matches
        // everything; the result must be the same as with one space.
        assert_eq!(
            col0(&run("rust  vec"), "History"),
            col0(&run("rust vec"), "History")
        );
        // And leading/trailing spaces likewise change nothing.
        assert_eq!(col0(&run(" rust "), "History"), col0(&run("rust"), "History"));
    }

    // ---- ordering ----

    #[test]
    fn history_comes_back_newest_first() {
        assert_eq!(
            col0(&run("duck"), "History"),
            [
                "https://duckduckgo.com/?q=rust", // 2026-08-06 13:51
                "https://DUCKling.example/",      // 2026-08-01 08:00
            ]
        );
    }

    #[test]
    fn a_list_category_floats_prefix_matches_but_keeps_source_order_otherwise() {
        // listcategory.py:90-100. "com" is in every quickmark url; none of them starts with it, so
        // the source order survives untouched.
        assert_eq!(
            col0(&run("com"), "Quickmarks"),
            [
                "https://www.google.com/",
                "https://duckduckgo.com/",
                "https://news.ycombinator.com/"
            ]
        );
        // "go" is inside "duckduckgo" (source order 0) and at the front of "google" (order 1), so
        // google jumps the queue while duckduckgo keeps its place behind it.
        assert_eq!(col0(&run("go"), "Search engines"), ["google", "duckduckgo"]);
    }

    #[test]
    fn the_prefix_preference_is_case_sensitive_like_pythons_startswith() {
        // Both rows still match — folding is case-insensitive — but neither starts with the
        // upper-case pattern, so the order is the source's.
        assert_eq!(
            col0(&run("GO"), "Search engines"),
            ["duckduckgo", "google"]
        );
    }

    // ---- highlighting ----

    #[test]
    fn match_ranges_point_into_the_first_column() {
        let cats = run("duck");
        let engine = &cats.iter().find(|c| c.name == "Search engines").unwrap().items[0];
        assert_eq!(engine.cols[0], "duckduckgo");
        // completiondelegate.py:38 highlights every occurrence, not the first.
        assert_eq!(engine.matches, [(0, 4), (4, 4)]);
    }

    #[test]
    fn every_term_is_highlighted_separately() {
        let cats = run("news hacker");
        let item = &cats.iter().find(|c| c.name == "History").unwrap().items[0];
        assert_eq!(item.cols[0], "https://news.ycombinator.com/");
        // Only "news" is in column 0; "hacker" matched the title and has nothing to mark here.
        assert_eq!(item.matches, [(8, 4)]);
    }

    #[test]
    fn ranges_are_utf16_units_not_bytes() {
        struct One;
        impl Sources for One {
            fn search_engines(&self) -> Vec<(String, String)> {
                Vec::new()
            }
            fn quickmarks(&self) -> Vec<(String, String)> {
                // Column 0 is 'ЯЯ' — four bytes, two UTF-16 units — then the term.
                pairs(&[("ЯЯsite", "q")])
            }
            fn bookmarks(&self) -> Vec<(String, String)> {
                Vec::new()
            }
            fn history(&self, _pattern: &str, _visit: &mut dyn FnMut(HistoryRow<'_>) -> Flow) {}
        }
        let cats = categories_from("site", &One);
        // Byte offset would be 4; UTF-16 offset is 2.
        assert_eq!(cats[0].items[0].matches, [(2, 4)]);
    }

    // ---- limits and serialisation ----

    #[test]
    fn each_category_stops_at_max_items() {
        struct Many;
        impl Sources for Many {
            fn search_engines(&self) -> Vec<(String, String)> {
                Vec::new()
            }
            fn quickmarks(&self) -> Vec<(String, String)> {
                (0..100)
                    .map(|i| (format!("https://q{i}.example/"), format!("q{i}")))
                    .collect()
            }
            fn bookmarks(&self) -> Vec<(String, String)> {
                Vec::new()
            }
            fn history(&self, _pattern: &str, visit: &mut dyn FnMut(HistoryRow<'_>) -> Flow) {
                for i in 0..100 {
                    let url = format!("https://h{i}.example/");
                    if visit(HistoryRow {
                        url: &url,
                        title: "t",
                        atime: "2026-08-06 00:00",
                    }) == Flow::Stop
                    {
                        return;
                    }
                }
            }
        }
        let cats = categories_from("example", &Many);
        assert_eq!(cats[0].items.len(), MAX_ITEMS);
        assert_eq!(cats[1].items.len(), MAX_ITEMS);
        // Newest first still holds: the walk stopped at the first MAX_ITEMS, not the last.
        assert_eq!(cats[1].items[0].cols[0], "https://h0.example/");
    }

    #[test]
    fn a_full_payload_stays_small_enough_to_push_on_every_keystroke() {
        // Measured, not guessed: four full categories of three 80-character columns, the largest
        // thing MAX_ITEMS can produce. 22,061 bytes on 2026-08-06 — a keystroke's worth of
        // ExecuteJavaScript, and the reason MAX_ITEMS is 25 and not "everything that matched".
        let long = "x".repeat(80);
        let items: Vec<Item> = (0..MAX_ITEMS)
            .map(|_| Item {
                cols: vec![long.clone(), long.clone(), "2026-08-06 13:51".into()],
                matches: vec![(0, 3), (10, 3)],
            })
            .collect();
        let cats: Vec<Category> = ["Search engines", "Quickmarks", "Bookmarks", "History"]
            .iter()
            .map(|name| Category {
                name,
                items: items.clone(),
            })
            .collect();
        let size = to_json(&cats, Some((3, 0))).len();
        assert!(size < 32 * 1024, "worst-case payload is {size} bytes");
    }

    #[test]
    fn json_is_the_shape_the_contract_names() {
        let cats = vec![Category {
            name: "History",
            items: vec![Item {
                cols: vec!["https://a/\"x\"".into(), "T".into()],
                matches: vec![(0, 2)],
            }],
        }];
        assert_eq!(
            to_json(&cats, Some((0, 0))),
            "{\"categories\":[{\"name\":\"History\",\"items\":[{\"cols\":[\"https://a/\\\"x\\\"\",\"T\"],\"match\":[[0,2]]}]}],\"selected\":[0,0]}"
        );
        assert_eq!(to_json(&[], None), "{\"categories\":[],\"selected\":null}");
    }

    #[test]
    fn the_milestone_example() {
        // PLAN.md M10: ":open du shows DuckDuckGo engine + history hits". The whole payload, byte
        // for byte, so a change to any rule shows up here as well as in the rule's own test.
        // Bookmarks are absent because neither of them contains "du" in either column; the second
        // history row is there because DUCKling folds; both "du"s in "duckduckgo" are marked.
        assert_eq!(
            to_json(&run("du"), Some((0, 0))),
            concat!(
                r#"{"categories":["#,
                r#"{"name":"Search engines","items":[{"cols":["duckduckgo","https://duckduckgo.com/?q={}"],"match":[[0,2],[4,2]]}]},"#,
                r#"{"name":"Quickmarks","items":[{"cols":["https://duckduckgo.com/","du"],"match":[[8,2],[12,2]]}]},"#,
                r#"{"name":"History","items":["#,
                r#"{"cols":["https://duckduckgo.com/?q=rust","rust at DuckDuckGo","2026-08-06 13:51"],"match":[[8,2],[12,2]]},"#,
                r#"{"cols":["https://DUCKling.example/","nothing here","2026-08-01 08:00"],"match":[[8,2]]}"#,
                r#"]}],"selected":[0,0]}"#,
            )
        );
    }

    #[test]
    fn without_a_source_the_completion_is_simply_closed() {
        // install() is never called in the test binary, so this is the pre-startup answer.
        assert!(categories("anything").is_empty());
    }
}
