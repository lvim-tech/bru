//! `bru://chrome/dial` — the start page: the sites you go to, in groups, as tiles.
//!
//! Asked for by name: *"плъгин за начална страница който да е списък с сайтове — нещо като
//! разпределител"*. It is not a plugin, and the reason is the whole architecture: a plugin is Lua
//! and reaches bru through `bru.cmd`, which can only compose commands that already exist, and a
//! `file://` page it wrote itself could never call `cefQuery` — `ipc.rs`'s security check refuses
//! every frame that is not `bru://`. A page that adds and removes its own tiles has to be one of
//! bru's own.
//!
//! ## The page is rendered in Rust, and that is the difference from `bru://chrome/cookies`
//!
//! `cookies.rs` serves a **shell** with not one cookie in it, because its data is asynchronous:
//! `visit_all_cookies` returns immediately and calls back later, while a scheme handler runs on the
//! **IO** thread and must answer there and then. None of that is true here. The tiles are in
//! `Data`, behind a mutex, and `history::marks_page` already reads exactly that from the IO thread
//! to build `bru://chrome/bookmarks`.
//!
//! So this page is built the way `/help`, `/history`, `/settings` and `/version` are built —
//! generated per request from what the browser knows — and for the two reasons those are:
//!
//! - **It is the start page.** It is drawn on every launch and every new tab, and a round trip to
//!   Rust before the first tile appears is a frame the user watches go by. Rendered here, the tiles
//!   are in the document that arrives.
//! - **One renderer cannot drift from itself.** A shell that draws tiles in JavaScript and a Rust
//!   side that knows what a tile is are two descriptions of one thing, and this project's answer to
//!   that is on `/help`: *"a page written separately from what it describes drifts"*.
//!
//! `chrome/dial.js` therefore draws nothing. It **changes** things — delete, add, edit, reorder,
//! undo — through `cefQuery`, and then reloads. That is cheap: the document is served from memory,
//! and a mutation is something a person does once, not something that happens per frame.
//!
//! ## Grouping, and why it is not a sort
//!
//! The file order is the page order. Groups are collected **by first appearance**: the first time a
//! group name is seen, it takes a heading, and every tile carrying that name is drawn under it in
//! file order. Nothing is sorted alphabetically, because the arrangement is the user's own — the
//! whole point of a dial is that the thing you reach for most is where you left it.
//!
//! Tiles with an empty group come first and get no heading. A dial nobody has organised is
//! therefore just a grid, which is what it should look like before anyone has asked for groups.
//!
//! ## Colour
//!
//! **Not one colour is written here or in the markup.** A tile's accent is a class, `a0`..`a8`, and
//! `chrome/chrome.css` maps those onto the nine accents the theme already defines —
//! `--blue --cyan --green --magenta --orange --purple --red --teal --yellow`. So the dial repaints
//! with lvim-colorscheme like the rest of the chrome, `chrome_css_carries_not_one_colour` covers
//! every rule it draws with, and the class is stable for a site because it is a hash of the host
//! rather than a position in the grid — a tile keeps its colour when its neighbour is deleted.
//!
//! ## The icon
//!
//! `favicon.rs` keeps `origin -> data:` **in memory only**, filled from `on_favicon_urlchange`. A
//! browser that has just started therefore knows no icons at all — and this is the first page it
//! draws. So a tile always renders a letter, and the icon is laid over it for the origins bru
//! happens to know. Nothing new is downloaded and nothing new is written to disk.
//!
//! That map lives in this process and `chrome::asset` runs in it, so the `<img>` goes into the
//! document here rather than being fetched by the page — asking over `cefQuery` for what is already
//! in hand would be the shell arrangement this comment has just argued against.
//!
//! ## Why there is no `:dial-add` button on the page
//!
//! Because the page *is* the current page: a button on it cannot know what you were looking at. The
//! honest shapes are a command typed where you are — `:dial-add`, the way `M` bookmarks the page
//! you are on — and a form on the page for typing an address you are not on. Both exist. A "add the
//! previous URL" button was considered and dropped: its answer depends on whether the dial was
//! opened in a new tab, over the current one, or as the start page of a browser that has no
//! previous URL at all, and in the third case there is no honest answer to give.

use cef::wrapper::message_router::BrowserSideCallback;
use cef::Browser;
use std::sync::{Arc, Mutex};

use crate::data::{Data, DialEntry};
use crate::tabs::SharedState;

/// The address the page lives at. `src/chrome.rs` maps the path; this is what `:dial` navigates to,
/// spelled once so the two cannot drift.
pub const DIAL_URL: &str = "bru://chrome/dial";

/// How many accents `chrome.css` defines for tiles. Nine because that is how many the theme has —
/// see the module comment.
const ACCENTS: u32 = 9;

// The two marks on a tile, drawn rather than typed.
//
// **They were `&#9998;` and `&times;` and the pencil came out wrong**: U+270E is a glyph, so what it
// looks like is whatever font on the machine happens to own it, and next to a `×` from a different
// face the pair did not read as a pair. Fourteen units square, `currentColor`, one stroke width —
// so they inherit the button's colour, they match each other, and no font can change either. No
// colour is written in them, which is the rule this project holds `chrome.css` to and there is no
// reason for markup to be looser.
const PENCIL: &str = "<svg viewBox=\"0 0 14 14\" width=\"14\" height=\"14\" fill=\"none\"      stroke=\"currentColor\" stroke-width=\"1.4\" stroke-linecap=\"round\"      stroke-linejoin=\"round\" aria-hidden=\"true\">     <path d=\"M9.4 1.9l2.7 2.7-7 7-3.4.7.7-3.4z\"/><path d=\"M8.2 3.1l2.7 2.7\"/></svg>";

const CROSS: &str = "<svg viewBox=\"0 0 14 14\" width=\"14\" height=\"14\" fill=\"none\"      stroke=\"currentColor\" stroke-width=\"1.4\" stroke-linecap=\"round\" aria-hidden=\"true\">     <path d=\"M3.4 3.4l7.2 7.2M10.6 3.4l-7.2 7.2\"/></svg>";

fn with_data<T>(f: impl FnOnce(&mut Data) -> T) -> Option<T> {
    let data = crate::data::instance()?;
    let mut guard = data.lock().ok()?;
    Some(f(&mut guard))
}

// ------------------------------------------------------------------------------------------------
// The commands
// ------------------------------------------------------------------------------------------------

/// `dial [-b]` — open the page.
///
/// A new tab, like `:cookies`, `:history` and `:bookmark-list`, and for the same reason those are:
/// a page of places to go is somewhere you go, not something that replaces what you were reading.
/// As a `start_page` it is loaded by navigation and never reaches this function.
pub fn show(state: &SharedState, browser: &mut Browser, bg: bool) {
    crate::open::open(state, browser, Some(DIAL_URL), true, bg);
}

/// `dial-add [-g <group>] [title]` — put the showing tab on the dial.
///
/// The shape of `bookmark-add`/`M`: the URL and the title come from `BruState`, which is what
/// `on_address_change` and `on_title_change` have been filling in, rather than from CEF — a second
/// source could disagree with the address the status line is showing.
///
/// With no title given, the tab's own is used; with neither, the host. A tile with the raw URL on it
/// is legible but it is not a name, and the host is the name a person would have typed anyway.
pub fn add(state: &SharedState, group: Option<&str>, title: Option<&str>) {
    let Some((url, tab_title)) = current_page(state) else {
        crate::message::error("dial-add: no page to add");
        return;
    };
    if url.starts_with(DIAL_URL) {
        crate::message::error("dial-add: that is the dial itself");
        return;
    }
    let title = title
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(str::to_string)
        .or_else(|| Some(tab_title.trim().to_string()).filter(|title| !title.is_empty()))
        .unwrap_or_else(|| host_of(&url).unwrap_or_else(|| url.clone()));
    let group = group.unwrap_or("").trim().to_string();

    match with_data(|data| data.dial_add(&url, &title, &group)) {
        Some(Ok(true)) => crate::message::info(&format!("Added {title} to the dial")),
        Some(Ok(false)) => crate::message::info(&format!("Updated {title} on the dial")),
        Some(Err(error)) => crate::message::error(&format!("dial-add: {error}")),
        None => {}
    }
}

/// The showing tab's address and title. `history::current_page`'s twin, and deliberately a copy of
/// four lines rather than a `pub` on that one: it reaches into `BruState` and belongs to whichever
/// module is asking, the way `with_data` above is a copy of the same four lines in three modules.
fn current_page(state: &SharedState) -> Option<(String, String)> {
    let state = state.lock().expect("state mutex poisoned");
    let index = state.active_tab();
    let url = state.tab_url(index).filter(|url| !url.is_empty())?;
    Some((url, state.tab_title(index).unwrap_or_default()))
}

// ------------------------------------------------------------------------------------------------
// Undo
// ------------------------------------------------------------------------------------------------

/// Tiles deleted this session, with the place each came from.
///
/// Memory, exactly as `cookies.rs`'s stash is, and the page says so. Deleting a tile is a small
/// loss and an easy misclick, and being able to put it back is worth more than a confirmation that
/// only asks whether you meant it. What it does not survive is the process.
fn stash() -> &'static Mutex<Vec<(usize, DialEntry)>> {
    static STASH: Mutex<Vec<(usize, DialEntry)>> = Mutex::new(Vec::new());
    &STASH
}

fn stash_push(at: usize, tile: DialEntry) -> usize {
    let Ok(mut stash) = stash().lock() else {
        return 0;
    };
    stash.push((at, tile));
    stash.len()
}

fn stash_pop() -> Option<(usize, DialEntry)> {
    stash().lock().ok()?.pop()
}

fn stash_len() -> usize {
    stash().lock().map(|stash| stash.len()).unwrap_or(0)
}

// ------------------------------------------------------------------------------------------------
// The page
// ------------------------------------------------------------------------------------------------

/// What `chrome.rs` serves for `/dial`. Generated per request — see the module comment.
pub fn page() -> String {
    let tiles = with_data(|data| data.dial().to_vec()).unwrap_or_default();
    render(&tiles, stash_len())
}

/// The whole document, from the tiles and how much undo is left.
///
/// A pure function of its arguments, which is what lets the tests below read the markup without a
/// browser, a data directory or a Lua state behind them.
pub fn render(tiles: &[DialEntry], undo: usize) -> String {
    let mut out = String::with_capacity(tiles.len() * 320 + 2048);
    out.push_str(
        r#"<!doctype html>
<meta charset="utf-8">
<title>bru — dial</title>
<link rel="stylesheet" href="chrome.css">
<link rel="stylesheet" href="theme.css">
<link rel="stylesheet" href="user.css">
<body data-view="dial">
<main id="dial">
"#,
    );

    if tiles.is_empty() {
        out.push_str(
            "<p class=\"empty\">The dial is empty. <code>:dial-add</code> puts the page you are \
             on here, and the form below takes an address you are not on.</p>\n",
        );
    }

    for (group, members) in grouped(tiles) {
        out.push_str("<section class=\"group\">\n");
        if !group.is_empty() {
            out.push_str(&format!("<h2>{}</h2>\n", crate::help::escape(&group)));
        }
        out.push_str("<div class=\"tiles\">\n");
        for tile in members {
            out.push_str(&tile_html(tile));
        }
        out.push_str("</div>\n</section>\n");
    }

    out.push_str(&controls(undo));
    out.push_str("</main>\n<script src=\"dial.js\"></script>\n");
    out
}

/// One tile. Every string in it is the user's own, so every one of them is escaped.
///
/// The address is written twice on purpose and the two are not the same string: `href` is
/// [`crate::history::safe_href`]'s answer, which is an **allowlist** of schemes that cannot run
/// anything, and `data-url` is the raw key the request sends back to name this tile. A tile whose
/// scheme is refused is still a tile that can be deleted, which is exactly what somebody who has
/// found a `javascript:` line in their dial file wants to do with it.
fn tile_html(tile: &DialEntry) -> String {
    let host = host_of(&tile.url).unwrap_or_default();
    format!(
        "<div class=\"tile a{accent}\" data-url=\"{url}\" draggable=\"true\">\
         <a class=\"go\" href=\"{href}\">\
         <span class=\"icon\">{letter}{icon}</span>\
         <span class=\"name\">{title}</span>\
         <span class=\"host\">{host}</span></a>\
         <span class=\"acts\">\
         <button class=\"edit\" type=\"button\" title=\"rename or regroup\">{PENCIL}</button>\
         <button class=\"del\" type=\"button\" title=\"remove this tile\">{CROSS}</button>\
         </span>\
         </div>\n",
        accent = accent_of(&host, &tile.url),
        url = crate::help::escape(&tile.url),
        href = crate::help::escape(&crate::history::safe_href(&tile.url)),
        letter = crate::help::escape(&letter_of(&tile.title)),
        icon = icon_html(&tile.url),
        PENCIL = PENCIL,
        CROSS = CROSS,
        title = crate::help::escape(&tile.title),
        host = crate::help::escape(&host),
    )
}

/// The one form, the undo control and the one line of help. Below the tiles, because the tiles are
/// what the page is for and a start page should open on them.
///
/// **Adding and editing are the same three fields**, and that is the whole of the editing UI: `✎`
/// fills this form from the tile and turns `Add` into `Edit`. An earlier version laid a second,
/// separate editor over the tile itself; two sets of inputs for one job is two things to style, two
/// to hint and two to keep in step, and the argument for putting the question "on the thing it is
/// about" is answered instead by `chrome.css`'s `.tile.editing`, which marks the tile the form is
/// currently pointed at.
///
/// `#add-cancel` is rendered rather than created by the script, so that every control on this page
/// exists in the document bru serves — a button the page invents is a button `chrome/hints.js`
/// cannot label until something has already been clicked.
fn controls(undo: usize) -> String {
    let undo_hidden = if undo == 0 { " hidden" } else { "" };
    format!(
        "<section id=\"controls\">\n\
         <form id=\"add\" autocomplete=\"off\">\
         <input id=\"add-url\" type=\"text\" placeholder=\"https://\" spellcheck=\"false\">\
         <input id=\"add-title\" type=\"text\" placeholder=\"name\" spellcheck=\"false\">\
         <input id=\"add-group\" type=\"text\" placeholder=\"group\" spellcheck=\"false\">\
         <button id=\"add-go\" type=\"submit\">Add</button>\
         <button id=\"add-cancel\" type=\"button\" hidden>Cancel</button>\
         </form>\n\
         <button id=\"undo\" type=\"button\"{undo_hidden}>Undo ({undo})</button>\n\
         <p class=\"summary\">\
         <kbd>gh</kbd> opens this in a tab &middot; \
         <kbd>f</kbd> hints every tile and every control &middot; \
         <kbd class=\"mark\">{PENCIL}</kbd> fills the form above &middot; \
         drag a tile to move it &middot; \
         <code>:dial-add</code> adds the page you are on &middot; \
         <code>~/.local/share/bru/dial</code> is the file\
         </p>\n\
         </section>\n",
        PENCIL = PENCIL,
    )
}

/// Tiles collected under their group, by **first appearance** — see the module comment for why this
/// is not a sort. The ungrouped run comes first whatever the file says, because it is drawn without
/// a heading and a headingless run in the middle of the page would read as belonging to the heading
/// above it.
fn grouped(tiles: &[DialEntry]) -> Vec<(String, Vec<&DialEntry>)> {
    let mut order: Vec<String> = Vec::new();
    for tile in tiles {
        if !order.contains(&tile.group) {
            order.push(tile.group.clone());
        }
    }
    if let Some(at) = order.iter().position(|group| group.is_empty()) {
        let ungrouped = order.remove(at);
        order.insert(0, ungrouped);
    }
    order
        .into_iter()
        .map(|group| {
            let members = tiles.iter().filter(|tile| tile.group == group).collect();
            (group, members)
        })
        .collect()
}

/// The letter a tile draws when there is no favicon: the first character of its name, uppercased.
///
/// Characters and not bytes — `Чете` must answer `Ч` and not half of a two-byte sequence, and
/// `to_uppercase` is the Unicode one, so it is the right glyph in Cyrillic as well as ASCII.
fn letter_of(title: &str) -> String {
    title
        .chars()
        .find(|c| !c.is_whitespace())
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_else(|| "?".to_string())
}

/// Which of `chrome.css`'s nine accents this tile wears.
///
/// Keyed on the **host**, so every tile of a site is the same colour and a tile keeps its colour
/// when its neighbours change. FNV-1a because it is six lines and needs no crate; nothing here is
/// security, only a stable spread across nine buckets.
///
/// **The fold is not decoration.** `hash % 9` uses only the low bits, and FNV-1a mixes those least
/// — measured on the nine hosts in `a_tiles_accent_follows_its_host_and_not_its_position`, the
/// plain modulo put them in **four** accents (`[1, 5, 6, 8]`) and the fold puts them in **seven**
/// (`[0, 1, 2, 3, 5, 6, 7]`). Four accents across nine sites is a dial that looks like it has a
/// colour scheme it does not have. `hash ^ (hash >> 16)` is the usual remedy and it is one line.
fn accent_of(host: &str, url: &str) -> u32 {
    let key = if host.is_empty() { url } else { host };
    let mut hash: u32 = 0x811c_9dc5;
    for byte in key.as_bytes() {
        hash ^= *byte as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    (hash ^ (hash >> 16)) % ACCENTS
}

/// The `<img>` that covers the letter, for an origin bru has an icon for — and nothing at all
/// otherwise.
///
/// **Rendered here rather than fetched by the page**, because `favicon.rs`'s map lives in this
/// process and `chrome::asset` runs in it: a round trip to ask for what is already in hand would be
/// the shell arrangement this module's comment argues against. The key comes from
/// `favicon::origin_of`, which is the same function that filed the icon — two spellings of an origin
/// is an icon that is never found.
///
/// The bytes are a `data:` URL `favicon.rs` built and base64'd, so there is nothing in it to escape:
/// it is `[A-Za-z0-9+/=]` after a fixed prefix. It is escaped anyway, because the cost is nothing
/// and the alternative is a rule that holds until the day the encoder changes.
fn icon_html(url: &str) -> String {
    let Some(icon) =
        crate::favicon::origin_of(url).and_then(|origin| crate::favicon::icon_for(&origin))
    else {
        return String::new();
    };
    format!("<img class=\"favicon\" src=\"{}\" alt=\"\">", crate::help::escape(&icon))
}

/// The host a tile writes under its name, lowercased, without a port or a leading `www.`.
///
/// Deliberately not a URL parser — the text between `://` and the next `/`, which is the same
/// answer `userstyles::host_of` needs and reaches the same way. `www.` goes because a tile labelled
/// `www.github.com` says nothing `github.com` does not.
fn host_of(url: &str) -> Option<String> {
    let after_scheme = url.split_once("://")?.1;
    let host = after_scheme.split(['/', '?', '#']).next()?;
    let host = host.rsplit_once('@').map(|(_, host)| host).unwrap_or(host);
    let host = host.split_once(':').map(|(host, _)| host).unwrap_or(host);
    let host = host.trim().to_ascii_lowercase();
    if host.is_empty() {
        return None;
    }
    Some(host.strip_prefix("www.").unwrap_or(&host).to_string())
}

// ------------------------------------------------------------------------------------------------
// What the page asks for
// ------------------------------------------------------------------------------------------------

/// `bru://chrome/dial` asking Rust to change something. Called from `ipc.rs` for
/// `{"type":"dial", …}` and for nothing else.
///
/// Every action answers **now**: the tiles are in memory and the write is one `rename(2)`. Nothing
/// here creates a browser or starts a navigation, so CEF-NOTES trap 12 does not apply and no task
/// is posted — the page reloads itself once the answer arrives.
///
/// The answer is always `{"undo":N}`, so the page has one shape to read whatever it asked.
pub fn on_page_query(request: &str, callback: &Arc<Mutex<dyn BrowserSideCallback>>) -> bool {
    let action = crate::ipc::json_field(request, "action").unwrap_or_default();
    let url = crate::ipc::json_field(request, "url").unwrap_or_default();
    let title = crate::ipc::json_field(request, "title").unwrap_or_default();
    let group = crate::ipc::json_field(request, "group").unwrap_or_default();

    let outcome: Result<String, String> = match action.as_str() {
        "add" => match with_data(|data| data.dial_add(&url, &title, &group)) {
            Some(Ok(_)) => Ok(String::new()),
            Some(Err(error)) => Err(error.to_string()),
            None => Err("no data directory".to_string()),
        },
        "delete" => match with_data(|data| data.dial_del(&url)) {
            Some(Ok(Some((at, tile)))) => {
                let title = tile.title.clone();
                stash_push(at, tile);
                crate::message::info(&format!("Removed {title} — Undo puts it back"));
                Ok(String::new())
            }
            Some(Ok(None)) => Err(format!("{url} is not on the dial")),
            Some(Err(error)) => Err(error.to_string()),
            None => Err("no data directory".to_string()),
        },
        // `new_url` is the address the form is submitting, which may be a *different* one from the
        // `url` that names the tile — see `Data::dial_retarget`. Absent or empty means unchanged,
        // which is what a drag between groups sends: it changes the group of a tile it is not
        // otherwise touching.
        "edit" => {
            let new_url = crate::ipc::json_field(request, "new_url").unwrap_or_default();
            let new_url = if new_url.trim().is_empty() { url.clone() } else { new_url };
            match with_data(|data| data.dial_retarget(&url, &new_url, &title, &group)) {
                Some(Ok(true)) => Ok(String::new()),
                Some(Ok(false)) => Err(format!("{url} is not on the dial")),
                Some(Err(error)) => Err(error.to_string()),
                None => Err("no data directory".to_string()),
            }
        }
        "reorder" => {
            let urls = crate::cookies::json_string_array(request, "urls");
            match with_data(|data| data.dial_reorder(&urls)) {
                Some(Ok(())) => Ok(String::new()),
                Some(Err(error)) => Err(error.to_string()),
                None => Err("no data directory".to_string()),
            }
        }
        // Nothing to undo is not a failure — the button can be pressed once more than there is
        // stash, and saying so in the bar beats an error the page has to render.
        "undo" => match stash_pop() {
            Some((at, tile)) => match with_data(|data| data.dial_insert(at, tile)) {
                Some(Ok(())) => Ok(String::new()),
                Some(Err(error)) => Err(error.to_string()),
                None => Err("no data directory".to_string()),
            },
            None => Ok(String::new()),
        },
        other => Err(format!("unknown dial action {other:?}")),
    };

    match outcome {
        Ok(_) => {
            if let Ok(callback) = callback.lock() {
                callback.success_str(&format!("{{\"undo\":{}}}", stash_len()));
            }
        }
        Err(why) => {
            crate::message::error(&format!("dial: {why}"));
            if let Ok(callback) = callback.lock() {
                callback.failure(-12, &why);
            }
        }
    }
    true
}

// ------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn tile(group: &str, title: &str, url: &str) -> DialEntry {
        DialEntry { group: group.to_string(), title: title.to_string(), url: url.to_string() }
    }

    /// Groups take a heading in the order they first appear, and the ungrouped run is drawn first
    /// and without one. Not alphabetical — the arrangement is the user's own.
    #[test]
    fn groups_are_headed_in_first_appearance_order_and_the_ungrouped_run_leads() {
        let tiles = [
            tile("четене", "HN", "https://news.ycombinator.com"),
            tile("работа", "GitHub", "https://github.com"),
            tile("", "Начало", "https://example.com"),
            tile("четене", "Вести", "https://www.vesti.bg"),
        ];
        let grouped = grouped(&tiles);
        let names: Vec<&str> = grouped.iter().map(|(group, _)| group.as_str()).collect();
        assert_eq!(names, ["", "четене", "работа"]);
        // Both members of "четене" are under the one heading, in file order.
        let reading: Vec<&str> = grouped[1].1.iter().map(|tile| tile.title.as_str()).collect();
        assert_eq!(reading, ["HN", "Вести"]);
    }

    /// The whole reason a tile's accent is a class and not a colour: the class is stable for a
    /// site, so deleting a neighbour does not repaint the grid.
    #[test]
    fn a_tiles_accent_follows_its_host_and_not_its_position() {
        let a = accent_of("github.com", "https://github.com/one");
        let b = accent_of("github.com", "https://github.com/two/deeper?q=1");
        assert_eq!(a, b);
        assert!(a < ACCENTS);
        // And it spreads: nine hosts must not all land in one bucket.
        let hosts = [
            "github.com", "crates.io", "news.ycombinator.com", "vesti.bg", "example.com",
            "docs.rs", "lobste.rs", "wikipedia.org", "archlinux.org",
        ];
        let mut seen: Vec<u32> = hosts.iter().map(|host| accent_of(host, "")).collect();
        seen.sort_unstable();
        seen.dedup();
        // Seven of the nine, measured. The bar is six: this is a guard against the fold in
        // `accent_of` being dropped — without it these same hosts land in four — and not a claim
        // about hash quality that a tenth host could break.
        assert!(seen.len() >= 6, "nine hosts landed in only {} accents: {seen:?}", seen.len());
    }

    #[test]
    fn the_letter_is_a_character_and_not_a_byte() {
        assert_eq!(letter_of("Четене"), "Ч");
        assert_eq!(letter_of("  github"), "G");
        assert_eq!(letter_of(""), "?");
        assert_eq!(letter_of("   "), "?");
    }

    #[test]
    fn the_host_and_the_origin_are_read_the_way_the_rest_of_bru_reads_them() {
        assert_eq!(host_of("https://www.vesti.bg/nesto").as_deref(), Some("vesti.bg"));
        assert_eq!(host_of("https://user:pw@Example.COM:8443/x").as_deref(), Some("example.com"));
        assert_eq!(host_of("not a url"), None);
        // The icon key is `favicon.rs`'s own function. This only pins that the dial asks the
        // question that module answers; the spelling itself is tested there.
        assert_eq!(
            crate::favicon::origin_of("https://GitHub.com:443/a/b").as_deref(),
            Some("https://github.com")
        );
    }

    /// Every string on a tile is the user's own, and the file is one they can edit by hand.
    #[test]
    fn a_title_a_group_and_a_url_are_all_escaped() {
        let tiles = [tile(
            "<script>g",
            "<img src=x onerror=alert(1)>",
            "https://example.com/?a=1&b=\"2\"",
        )];
        let html = render(&tiles, 0);
        assert!(!html.contains("<script>g"), "the group reached the document unescaped");
        assert!(!html.contains("<img src=x"), "the title reached the document unescaped");
        assert!(html.contains("&lt;script&gt;g"));
        assert!(html.contains("&lt;img src=x onerror=alert(1)&gt;"));
        assert!(html.contains("a=1&amp;b=&quot;2&quot;"));
    }

    /// `href` is an allowlist and `data-url` is the key. A tile bru will not follow is still a tile
    /// that can be deleted — see `tile_html`.
    #[test]
    fn a_refused_scheme_loses_its_href_and_keeps_its_delete_button() {
        let tiles = [tile("", "Bad", "javascript:alert(1)")];
        let html = render(&tiles, 0);
        assert!(!html.contains("href=\"javascript:"), "a javascript: URL reached an href");
        assert!(html.contains("data-url=\"javascript:alert(1)\""));
        assert!(html.contains("class=\"del\""));
    }

    #[test]
    fn the_undo_control_appears_only_when_there_is_something_to_undo() {
        assert!(render(&[], 0).contains("id=\"undo\" type=\"button\" hidden"));
        let with_stash = render(&[], 2);
        assert!(with_stash.contains("Undo (2)"));
        assert!(!with_stash.contains("id=\"undo\" type=\"button\" hidden"));
    }

    /// **A regression test for a bug this file shipped.** The two marks on a tile are inline SVG,
    /// so a click lands on the `<svg>` or on the `<path>` inside it and *never* on the `<button>`
    /// that carries the class. `chrome/dial.js` read `event.target.classList` and both controls
    /// stopped working the moment the glyphs became drawings — nothing failed, nothing logged, the
    /// buttons simply did nothing.
    ///
    /// There is no JavaScript test runner here, so this reads the two files and pins the pairing
    /// that has to hold: the buttons contain an element, and the handler asks what the target is
    /// *inside* rather than what it is. `chrome::tests::chrome_css_carries_not_one_colour` is the
    /// same shape — a test that reads an asset and fails the build on a decision being undone.
    #[test]
    fn a_click_on_a_tile_control_is_resolved_by_closest_and_not_by_the_targets_own_class() {
        let markup = render(&[tile("", "T", "https://example.com")], 0);
        assert!(
            markup.contains("<svg"),
            "the marks are drawings; if they go back to being glyphs this test is moot"
        );
        // The `<svg>` really is inside the button, which is what makes `event.target` the wrong
        // question.
        assert!(markup.contains("class=\"edit\" type=\"button\" title=\"rename or regroup\"><svg"));
        assert!(markup.contains("class=\"del\" type=\"button\" title=\"remove this tile\"><svg"));

        let script = include_str!("../chrome/dial.js");
        assert!(
            script.contains("closest(\".del\")") || script.contains("within(\".del\")"),
            "chrome/dial.js must resolve a control click with closest()"
        );
        for wrong in [
            "target.classList.contains(\"del\")",
            "target.classList.contains(\"edit\")",
        ] {
            assert!(
                !script.contains(wrong),
                "chrome/dial.js is back to {wrong} — a click on the mark inside the button will \
                 miss it"
            );
        }
    }

    /// An empty dial says what to do about it rather than drawing nothing.
    /// The two names, and the shapes they have to take. Tested here rather than in `commands.rs`'s
    /// own test module so that this workstream's edits to that shared file stay the two fenced
    /// blocks — the arrangement `cookies.rs` already uses.
    #[test]
    fn the_commands_are_dial_and_dial_add() {
        use crate::commands::{parse, Command};
        assert_eq!(parse("dial").unwrap(), Command::Dial { bg: false });
        assert_eq!(parse("dial -b").unwrap(), Command::Dial { bg: true });
        assert_eq!(parse("dial --bg").unwrap(), Command::Dial { bg: true });

        assert_eq!(
            parse("dial-add").unwrap(),
            Command::DialAdd { group: None, title: None }
        );
        // maxsplit0: a title with spaces stays one argument rather than losing its tail.
        assert_eq!(
            parse("dial-add Hacker News").unwrap(),
            Command::DialAdd { group: None, title: Some("Hacker News".to_string()) }
        );
        // The whole reason this uses `Flagged` and not `Args`: `-g` eats the word after it, so the
        // group is the group and the title is everything left. With plain `Args` the title here
        // would have been "работа Кутии" and the group would have been lost.
        assert_eq!(
            parse("dial-add -g работа Кутии с код").unwrap(),
            Command::DialAdd {
                group: Some("работа".to_string()),
                title: Some("Кутии с код".to_string()),
            }
        );
        assert_eq!(
            parse("dial-add --group четене").unwrap(),
            Command::DialAdd { group: Some("четене".to_string()), title: None }
        );

        // And both run — a name that parses and does nothing is what `is_live` is for.
        assert!(crate::exec::is_live(&parse("dial").unwrap()));
        assert!(crate::exec::is_live(&parse("dial-add").unwrap()));
    }

    /// The one request whose shape crosses a module boundary: the page sends `urls` as an array
    /// and `cookies::json_string_array` is what reads it. Pinned here because a change to that
    /// reader would silently turn every reorder into a no-op — `dial_reorder` keeps what it is not
    /// given, so an empty list rewrites the file to exactly what it already was.
    #[test]
    fn the_reorder_request_is_read_the_way_the_page_writes_it() {
        // Byte for byte what chrome/dial.js's JSON.stringify produced in the harness.
        let request = r#"{"type":"dial","action":"reorder","urls":["https://example.com","https://lobste.rs","https://github.com"]}"#;
        assert_eq!(
            crate::cookies::json_string_array(request, "urls"),
            ["https://example.com", "https://lobste.rs", "https://github.com"]
        );
        // And an absent or malformed array is an empty list, never a panic.
        assert!(crate::cookies::json_string_array(r#"{"action":"reorder"}"#, "urls").is_empty());
    }

    #[test]
    fn an_empty_dial_explains_itself() {
        let html = render(&[], 0);
        assert!(html.contains("The dial is empty"));
        assert!(html.contains(":dial-add"));
    }

    /// The page must wear the chrome the theme paints, and nothing else. `chrome.css` is covered by
    /// `chrome::tests::chrome_css_carries_not_one_colour`; this is the other half — that the
    /// document actually asks for it, and carries no `<style>` block of its own to slip a colour
    /// past that test.
    #[test]
    fn the_document_is_styled_only_by_the_theme() {
        let html = render(&[tile("g", "T", "https://example.com")], 0);
        assert!(html.contains(r#"<link rel="stylesheet" href="chrome.css">"#));
        assert!(html.contains(r#"<link rel="stylesheet" href="theme.css">"#));
        assert!(html.contains(r#"<link rel="stylesheet" href="user.css">"#));
        assert!(!html.contains("<style"), "the dial must not carry its own stylesheet");
        assert!(html.contains(r#"data-view="dial""#));
    }
}
