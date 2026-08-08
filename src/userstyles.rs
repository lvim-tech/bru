//! Per-site CSS: `~/.config/bru/styles/<domain>/*.css`.
//!
//! ```text
//! ~/.config/bru/styles/
//!     duckduckgo.com/
//!         colours.css
//!     github.com/
//!         narrower.css
//! ```
//!
//! A page on `start.duckduckgo.com` gets everything in `duckduckgo.com/`. That is the whole rule:
//! **the folder's name is the pattern**, and there is no metadata block, no `@match`, no language to
//! learn. `greasemonkey.rs` has a pattern language because a userscript is a thing people download
//! from strangers who wrote it for the whole web; a stylesheet a person writes for the two search
//! engines they use every day needs a folder with the right name and nothing else.
//!
//! ## Why bru and not a plugin
//!
//! Timing. Anything injected after the document has been drawn arrives after the first paint — a
//! flash of the site's own colours, then yours. That is the class of fault a whole afternoon went
//! into removing from the completion panel, and it is not worth reintroducing on every navigation.
//! This runs from `LoadHandler::on_load_start`, where the document element exists and the page's own
//! `<head>` has not been parsed yet.
//!
//! ## The cascade
//!
//! Every folder that the host ends in applies, **least specific first**, so that
//! `duckduckgo.com/base.css` sets the palette and `html.duckduckgo.com/tweak.css` corrects the one
//! page that needs it. A host of `a.b.example.com` looks for `example.com`, `b.example.com` and
//! `a.b.example.com` in that order. The public suffix itself is never a folder: `com/` would be a
//! rule for a third of the web written by accident.
//!
//! ## What it is not
//!
//! **Not a dark-mode engine.** A stylesheet written by hand for one site is the only way to get a
//! site to wear *your* palette; Chromium's force-dark inversion takes thresholds, not colours, and
//! cannot be told about everforest. This is the layer that gives a design and the reason it is worth
//! the work is that it is written for one page rather than guessed for all of them.
//!
//! **Not maintenance-free.** A site that redesigns breaks the stylesheet written for it. That is the
//! cost, and it is why this is worth doing for the handful of sites read every day and not for the
//! web.
//!
//! **Read per navigation, never cached.** Editing a file and reloading the page is the whole of the
//! edit cycle — there is no `:styles-reload` because there is nothing holding a stale copy. A page
//! load already does a `stat` and a read for `theme.css`; this is the same cost against a directory
//! that is usually empty.

use cef::*;

/// `~/.config/bru/styles`, or `None` where there is no config directory.
///
/// **`~/.config/bru/`, not `~/.local/share/bru/`**, and the difference is who writes it. The
/// greasemonkey directory is under `share` because a userscript is usually downloaded; these are
/// hand-written, they are read and never generated, and they belong beside `config.lua`. bru does
/// not create this directory any more than it creates the one above it.
fn styles_dir() -> Option<std::path::PathBuf> {
    Some(crate::chrome::config_dir()?.join("styles"))
}

/// The host out of a URL, lowercased, without a port or a leading `www.`.
///
/// Deliberately not a URL parser: what is wanted is the text between `://` and the next `/`, and
/// every other part of a URL is somebody else's question. `www.` is dropped because a folder called
/// `www.github.com` is a folder that stops working the day a link omits it, and nobody means the
/// distinction.
fn host_of(url: &str) -> Option<String> {
    let after_scheme = url.split_once("://")?.1;
    let host = after_scheme.split(['/', '?', '#']).next()?;
    // Strip credentials and a port: `user:pass@host:443`.
    let host = host.rsplit_once('@').map(|(_, host)| host).unwrap_or(host);
    let host = host.split_once(':').map(|(host, _)| host).unwrap_or(host);
    let host = host.trim().to_ascii_lowercase();
    if host.is_empty() {
        return None;
    }
    Some(host.strip_prefix("www.").unwrap_or(&host).to_string())
}

/// The folder names to look for, least specific first.
///
/// `a.b.example.com` answers `["example.com", "b.example.com", "a.b.example.com"]`. The last label
/// on its own is never included: a folder called `com` would be a rule for a third of the web,
/// written by somebody who meant one site.
///
/// A bare label — `localhost` — is its own only candidate, because it is a host and not a suffix of
/// anything.
fn candidates(host: &str) -> Vec<String> {
    let labels: Vec<&str> = host.split('.').filter(|label| !label.is_empty()).collect();
    if labels.len() < 2 {
        return if labels.is_empty() { Vec::new() } else { vec![host.to_string()] };
    }
    // From two labels up to the whole host.
    (2..=labels.len()).map(|take| labels[labels.len() - take..].join(".")).collect()
}

/// Every stylesheet that applies to `url`, concatenated in cascade order.
///
/// Empty when nothing applies, which is the usual answer and costs one `read_dir` that fails.
pub fn css_for(url: &str) -> String {
    let Some(dir) = styles_dir() else {
        return String::new();
    };
    let Some(host) = host_of(url) else {
        return String::new();
    };
    let mut out = String::new();
    // **The theme's own custom properties go in first**, so a stylesheet written for a site can say
    // `background: var(--bg)` and follow the theme rather than freezing one.
    //
    // Without this the only way to colour a site is to write hex into the file, and then switching
    // the theme leaves every hand-written stylesheet wearing the old one — which is the failure the
    // whole `theme.css` arrangement exists to prevent everywhere else. A page never loads
    // `theme.css`, so the properties have to travel with the rules that use them.
    //
    // Prepended only when something applies: `css_for` answers empty for a site with no folder, and
    // the 9KB below is not put into a page nobody has styled.
    let theme = String::from_utf8_lossy(&crate::chrome::theme_css()).into_owned();
    for candidate in candidates(&host) {
        let folder = dir.join(&candidate);
        let Ok(entries) = std::fs::read_dir(&folder) else {
            continue;
        };
        // Sorted, so that two files in one folder apply in an order a person can predict from their
        // names rather than from the order the filesystem happens to hand them back.
        let mut files: Vec<std::path::PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "css"))
            .collect();
        files.sort();
        for file in files {
            if let Ok(css) = std::fs::read_to_string(&file) {
                if out.is_empty() {
                    out.push_str("/* the theme in force, so a rule below can name a colour */\n");
                    out.push_str(&theme);
                }
                out.push_str(&format!("\n/* {} */\n", file.display()));
                out.push_str(&css);
            }
        }
    }
    out
}

/// The keeper that lives in the page. See `chrome/userstyle.js` for the three failures it exists to
/// survive, and qutebrowser's `javascript/stylesheet.js`, which is where its shape comes from.
const KEEPER_JS: &str = include_str!("../chrome/userstyle.js");

/// Whether the styles are switched on. **Read in the renderer, so it cannot ask the settings
/// store** — that lives in the browser process. The value is pushed here by `settings.rs` through
/// `Backing::UserStyles`, and it starts at the shipped default so a renderer that has not been told
/// yet behaves as a configured one does.
static ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Whether this renderer has already asked. A bool rather than `Option<bool>` around `ENABLED`,
/// because the default is a real answer and not an absence: the question is only ever "has anybody
/// corrected it", and asking twice would be one message per document instead of one per process.
static ASKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The process message `settings.rs` sends when `content.user_styles` moves.
///
/// There was a `RELOAD` beside it, and a cache for it to drop. Both were dead — the cache was
/// written in one place, the `RELOAD` arm that cleared it, and **never read**; and nothing ever
/// sent `RELOAD`, because `:styles-reload` does not exist. It does not exist for a reason this
/// module's own header gives: the folder is read on every navigation, so there is never a stale
/// copy to drop. Removed 2026-08-07 rather than given the command it was waiting for.
pub const SET_ENABLED: &str = "bru.userstyles.enabled";

/// The process message a renderer sends when it has never been told whether the styles are on.
///
/// **`ENABLED` starts `true` in every renderer, and nothing told the new ones otherwise.** So
/// `:styles-toggle` off held for the pages that were open and was undone by the next site opened,
/// because a cross-site navigation makes a renderer process and that one started at the default.
/// `scrollbar.rs` had the identical hole and this is the identical answer: apply the default at
/// once, ask, and correct on the reply. One message per renderer process.
pub const ASK: &str = "bru.userstyles.ask";

/// **The renderer's hook, and the only place a page is styled.**
///
/// `RenderProcessHandler::on_context_created` — the same door `greasemonkey.rs` uses, and for the
/// same reason. It fires when, and only when, a V8 context exists for a document: by definition, for
/// every document, including the first one a window ever shows. The browser-side
/// `LoadHandler::on_load_start` was tried first and is subtly wrong — a browser that has only just
/// been created has no context yet, so the script was handed over and evaporated, and the start page
/// was never styled. Injecting a second time from `on_load_end` covered it up; this removes the
/// thing being covered.
///
/// The directory is read **here**, in the renderer, exactly as `greasemonkey.rs` reads its own.
/// There is no message carrying stylesheets across: the renderer can open a file, and a second
/// transport would be a second thing to keep in step.
pub fn renderer_on_context_created(frame: Option<&Frame>) {
    let Some(frame) = frame else {
        return;
    };
    if !styleable(frame) {
        return;
    }

    // The keeper first, always: it is what `off` needs a handle on, and installing it costs nothing
    // when there is no CSS to give it.
    crate::greasemonkey::evaluate(frame, KEEPER_JS, Some("bru://userstyle.js"));

    // Ask once per renderer process, for [`ASK`]'s reason. The default is applied below meanwhile,
    // which is the right answer for a bru nobody has configured and the wrong one only for the
    // moment between the question and its answer.
    if !ASKED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        if let Some(mut message) = process_message_create(Some(&CefString::from(ASK))) {
            frame.send_process_message(ProcessId::BROWSER, Some(&mut message));
        }
    }

    apply(frame);
}

/// Tell every renderer whether the styles are on.
///
/// **The browser process's side.** `Backing::UserStyles` calls it; the renderers act on it — and
/// *act* is the word, since `renderer_on_message` applies to the frame the message arrived on.
/// There used to be a `browser.reload()` here, to make the toggle change the page in front of the
/// user rather than the next one; it is gone with the same reasoning `scrollbar.rs` used, plus one
/// of its own: reloading every tab to change a colour throws away whatever was typed into a form on
/// any of them.
pub fn push_enabled(on: bool) {
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
        send_enabled(&frame, on);
    }
}

/// Re-apply the per-site stylesheets in every open page, because the theme underneath them moved.
///
/// **A page carries the theme, and the theme is not the page's.** `css_for` prepends `theme.css` to
/// whatever the site's folder holds, so a stylesheet that says `var(--bg)` is resolved against the
/// theme **as it was when the document loaded**. `theme_watch.rs` re-reads the file and reloads
/// bru's own chrome, and until this existed that was the whole of applying a theme: the strips
/// changed colour and the page under them kept the old one until it was reloaded by hand. Measured
/// 2026-08-08 — `--bg` still `#292f33` on a page whose theme file had been rewritten to Gruvbox,
/// with bru's own log line saying it had noticed.
///
/// It is `push_enabled` with the value unchanged, and that is not a trick: the renderer's handler
/// already re-reads the folder and hands the result to the keeper already in the page, which is
/// exactly "apply the current answer again". Nothing reloads.
pub fn repaint() {
    push_enabled(crate::settings::is_on("content.user_styles"));
}

/// One `SET_ENABLED` message, to one frame. The only place that message is built.
fn send_enabled(frame: &Frame, on: bool) {
    let Some(mut message) = process_message_create(Some(&CefString::from(SET_ENABLED))) else {
        return;
    };
    if let Some(arguments) = message.argument_list() {
        arguments.set_bool(0, i32::from(on));
    }
    frame.send_process_message(ProcessId::RENDERER, Some(&mut message));
}

/// Answer one renderer's [`ASK`]. The browser process's side, called from `ipc.rs`.
pub fn on_ask(frame: Option<&Frame>, message: Option<&ProcessMessage>) -> bool {
    let Some(message) = message else {
        return false;
    };
    if CefString::from(&message.name()).to_string() != ASK {
        return false;
    }
    if let Some(frame) = frame {
        send_enabled(frame, crate::settings::is_on("content.user_styles"));
    }
    true
}

/// The renderer's side of the two messages. Answers whether it was one of them.
///
/// **It applies as well as remembers.** Storing alone is what made the first version of [`ASK`]
/// useless: the renderer asked, put the default in — 34,158 characters of stylesheet — and then
/// learned the answer was `false` and did nothing with it. Measured 2026-08-07 on exactly that
/// path, `:styles-toggle` off followed by a cross-site navigation back. The keeper is already in
/// the page and takes both `set` and `off` at any time, so the correction lands on the document
/// that is in front of the user.
pub fn renderer_on_message(frame: Option<&Frame>, message: Option<&ProcessMessage>) -> bool {
    let Some(message) = message else {
        return false;
    };
    let name = CefString::from(&message.name()).to_string();
    if name == SET_ENABLED {
        let on = message.argument_list().map(|arguments| arguments.bool(0) != 0).unwrap_or(true);
        ENABLED.store(on, std::sync::atomic::Ordering::Relaxed);
        if let Some(frame) = frame {
            apply(frame);
        }
        return true;
    }
    false
}

/// Put the right stylesheet — or none — into a frame that already has the keeper.
///
/// Split out of [`renderer_on_context_created`] so that an answer arriving after the page was drawn
/// takes the same path as the page being drawn. Assumes the keeper is installed; every caller has
/// installed it or is answering a message on a frame where it was.
fn apply(frame: &Frame) {
    if !styleable(frame) {
        return;
    }
    if !ENABLED.load(std::sync::atomic::Ordering::Relaxed) {
        crate::greasemonkey::evaluate(
            frame,
            "window.bruKeep && window.bruKeep(\"bru-userstyle\",\"last\").off();",
            None,
        );
        return;
    }
    let Some(wears) = dressed_as(frame) else {
        return;
    };
    let css = css_for(&wears);
    if css.trim().is_empty() {
        crate::greasemonkey::evaluate(
            frame,
            "window.bruKeep && window.bruKeep(\"bru-userstyle\",\"last\").off();",
            None,
        );
        return;
    }
    let code = format!(
        "window.bruKeep && window.bruKeep(\"bru-userstyle\",\"last\").set(\"{}\");",
        crate::ipc::json_escape(&css)
    );
    crate::greasemonkey::evaluate(frame, &code, None);
}

/// Whether this frame is a page bru styles at all: the main frame of a real document.
///
/// An advert in an iframe is not the site the folder was named after, and `bru://` chrome links
/// `chrome.css`, which already carries the theme.
fn styleable(frame: &Frame) -> bool {
    dressed_as(frame).is_some()
}

/// The URL whose folder this frame should wear, or `None` when it should wear nothing.
///
/// **A subframe of the same site is the site.** Until 2026-08-08 this answered for the main frame
/// only, with the argument that "an advert in an iframe is not the site the folder was named
/// after". That argument is right about adverts and wrong about a site's own panels: Google puts
/// its settings panel in an `about:blank` iframe, 436x539, which has no host of its own and so
/// matched no folder — and no selector in any stylesheet can cross a frame boundary, so it stayed
/// unthemed while everything around it was themed. Found 2026-08-08 while covering google.com.
///
/// So the rule is **same host**, and it keeps the advert out by construction:
///
/// - the main frame wears its own URL, as before;
/// - a subframe with no host of its own — `about:blank`, `about:srcdoc`, a `data:` document — is
///   filled by the page that made it, so it wears the page's;
/// - a subframe with a host wears its own only when that host is the page's;
/// - anything else, which is every third-party embed, wears nothing.
fn dressed_as(frame: &Frame) -> Option<String> {
    let url = CefString::from(&frame.url()).to_string();
    if url.starts_with("bru://") {
        return None;
    }
    if frame.is_main() != 0 {
        return url.contains("://").then_some(url);
    }
    // The page this frame belongs to. `browser()` exists on a renderer-side frame, and the main
    // frame is the one whose folder the page was styled from a moment ago.
    let page = frame
        .browser()
        .and_then(|browser| browser.main_frame())
        .map(|main| CefString::from(&main.url()).to_string())?;
    same_site(&url, &page)
}

/// [`dressed_as`]'s decision for a subframe, as a function of two strings so it can be tested.
fn same_site(frame_url: &str, page_url: &str) -> Option<String> {
    let page_host = host_of(page_url)?;
    match host_of(frame_url) {
        // Its own host: only when it is the page's. A third-party embed answers `None` here.
        Some(host) => (host == page_host).then_some(frame_url.to_string()),
        // No host at all — the page wrote this document, so it is the page's.
        None => Some(page_url.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_host_is_what_is_between_the_scheme_and_the_path() {
        assert_eq!(host_of("https://duckduckgo.com/?q=x").as_deref(), Some("duckduckgo.com"));
        assert_eq!(host_of("https://www.github.com/a/b").as_deref(), Some("github.com"));
        assert_eq!(host_of("http://user:pw@example.com:8080/x").as_deref(), Some("example.com"));
        assert_eq!(host_of("HTTPS://Example.COM/").as_deref(), Some("example.com"));
        // Not a URL with a host: a page bru serves itself, and a file.
        assert_eq!(host_of("bru://chrome/help").as_deref(), Some("chrome"));
        assert_eq!(host_of("about:blank"), None);
    }

    /// **The cascade is least specific first**, so a folder for the site sets the palette and a
    /// folder for one of its subdomains corrects it.
    #[test]
    fn every_folder_the_host_ends_in_applies_in_order() {
        assert_eq!(
            candidates("a.b.example.com"),
            vec!["example.com", "b.example.com", "a.b.example.com"]
        );
        assert_eq!(candidates("duckduckgo.com"), vec!["duckduckgo.com"]);
    }

    /// **The public suffix is never a folder.** `com/` would be a rule for a third of the web,
    /// written by somebody who meant one site — so a one-label tail is not a candidate and the walk
    /// starts at two.
    #[test]
    fn a_bare_suffix_is_not_a_rule() {
        assert!(!candidates("example.com").contains(&"com".to_string()));
        assert!(!candidates("a.b.example.com").contains(&"com".to_string()));
        // A host with no dot in it is its own only candidate: it is a host, not a suffix.
        assert_eq!(candidates("localhost"), vec!["localhost"]);
    }

    /// **The CSS reaches the page as a JS string literal**, and `ipc::json_escape` writes the
    /// escapes but not the quotes around them. Without them 12KB of stylesheet arrived as bare
    /// source: a syntax error that did nothing, said nothing, and was found by looking at a page.
    /// The same omission was made in `scrollbar.rs` first; a guard in each is cheaper than
    /// remembering.
    #[test]
    fn the_css_arrives_inside_a_string() {
        let css = "body { content: \"x\" }\nb { }";
        let code = format!(
            "window.bruKeep && window.bruKeep(\"bru-userstyle\",\"last\").set(\"{}\");",
            crate::ipc::json_escape(css)
        );
        assert!(code.contains(".set(\""), "the CSS is not quoted: {code}");
        // A newline in the stylesheet must not end the literal, which is what multi-line CSS does.
        assert!(!code.contains("}\nb {"), "a newline survived into the source: {code}");
        assert!(code.contains("\\\"x\\\""), "a quote in the CSS was not escaped: {code}");
    }

    /// Nothing to apply is the usual answer, and it must not be a `<style>` with nothing in it.
    #[test]
    fn a_site_with_no_folder_gets_nothing() {
        assert!(css_for("https://a-site-nobody-has-styled.example/").trim().is_empty());
    }

    /// **A subframe of the same site is the site; a third party is not.**
    ///
    /// The rule that decides it, without a browser: Google's settings panel is an `about:blank`
    /// frame the page fills itself, and it went unthemed for as long as this answered for the main
    /// frame only. An advert has a host of its own, and that is exactly what keeps it out.
    #[test]
    fn a_frame_wears_the_page_it_belongs_to_only_when_it_is_the_page() {
        let page = "https://www.google.com/search?q=x";
        // No host of its own: the page wrote this document, so it wears the page's folder.
        assert_eq!(same_site("about:blank", page), Some(page.to_string()));
        assert_eq!(same_site("about:srcdoc", page), Some(page.to_string()));
        assert_eq!(same_site("", page), Some(page.to_string()));
        // Its own host, and it is the page's — `www.` is stripped by `host_of` on both sides.
        let own = "https://google.com/inner";
        assert_eq!(same_site(own, page), Some(own.to_string()));
        // A third party. This is the advert the old rule existed to keep out, and it still is.
        assert_eq!(same_site("https://doubleclick.net/ad", page), None);
        assert_eq!(same_site("https://evil.example/", page), None);
    }

    /// A page with no host cannot lend one. `about:blank` inside `about:blank` is nobody's site.
    #[test]
    fn a_page_without_a_host_dresses_nothing() {
        assert_eq!(same_site("about:blank", "about:blank"), None);
        assert_eq!(same_site("https://google.com/", ""), None);
    }

    /// A subdomain is not the same host, and it does not need to be: `css_for` walks the folders the
    /// host ends in, so `mail.google.com` in a `google.com` page already gets `google.com/`.
    #[test]
    fn a_subdomain_frame_wears_its_own_url_and_the_walk_does_the_rest() {
        let page = "https://google.com/search";
        assert_eq!(same_site("https://mail.google.com/x", page), None);
        assert_eq!(candidates("mail.google.com"), vec!["google.com", "mail.google.com"]);
    }

    /// **The keeper is the thing that survives a document being replaced**, and it has to be in the
    /// binary for that to be true. A `chrome/*` file is `include_str!`d, so a rename that missed
    /// this would be a compile error — but an emptied one would not.
    ///
    /// It is a **factory** since `scrollbar.rs` started asking for one too: `bruKeep(id, where)`
    /// hands out one keeper per id, so the two never fight over an element, and `where` is which end
    /// of the cascade that id wants. This file asks for `"last"`, where the user is meant to win.
    #[test]
    fn the_keeper_is_there_and_owns_one_element() {
        assert!(KEEPER_JS.contains("MutationObserver"), "no observer: the root swap is not handled");
        assert!(KEEPER_JS.contains("window.bruKeep"), "nothing to call set() on");
        assert!(KEEPER_JS.contains("appendChild"), "the style is never put anywhere");
        // Both ends, because one keeper that only ever appends would silently give the scrollbar
        // the user's place in the cascade.
        assert!(KEEPER_JS.contains("insertBefore"), "\"first\" cannot be honoured");
    }
}
