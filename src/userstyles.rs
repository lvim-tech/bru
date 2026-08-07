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
                out.push_str(&format!("\n/* {} */\n", file.display()));
                out.push_str(&css);
            }
        }
    }
    out
}

/// The `<style>` element's id, so a second injection replaces the first rather than stacking.
const STYLE_ID: &str = "bru-userstyle";

/// The script one frame is handed.
///
/// **Last child of `<head>` if there is one, otherwise last of `<html>`** — the opposite of where
/// `scrollbar.rs` puts its own. That one goes in first on purpose, so a page's own scrollbar rules
/// win; this one is the user overriding the page and has to come after everything the page will
/// bring. At `on_load_start` the page's `<head>` is empty, so "last" is a claim about the cascade
/// and not about the DOM as it stands: a rule of equal specificity added later still loses to one
/// the page adds afterwards, which is why a stylesheet written here will sometimes need
/// `!important` and `scrollbar.rs`'s deliberately never does.
pub fn script_for(url: &str) -> Option<String> {
    let css = css_for(url);
    if css.trim().is_empty() {
        return None;
    }
    Some(format!(
        "(function(){{var d=document,o=d.getElementById({id});if(o)o.remove();\
         var s=d.createElement('style');s.id={id};s.textContent={css};\
         (d.head||d.documentElement).appendChild(s);}})()",
        id = crate::ipc::json_escape(STYLE_ID),
        css = crate::ipc::json_escape(&css),
    ))
}

/// Take the stylesheet out again — what the toggle does when it is switched off.
///
/// A separate script rather than [`script_for`] answering `None`, because those are two different
/// facts: "this site has no folder" leaves the document alone, and "the user turned it off" has to
/// remove what a previous load put in.
fn removal_script() -> String {
    format!(
        "(function(){{var o=document.getElementById({id});if(o)o.remove();}})()",
        id = crate::ipc::json_escape(STYLE_ID),
    )
}

/// Re-run the injection in every tab of every window — what `Backing::UserStyles` calls.
///
/// **UI thread**, like every other `Backing` arm's push. `:styles-toggle` has to change the page the
/// user is looking at rather than the next one they open, and each tab is asked about its own URL:
/// a window with a styled site and an unstyled one in it must not have both answered the same way.
pub fn reinject_everywhere() {
    debug_assert_ne!(currently_on(ThreadId::UI), 0);
    let on = crate::settings::is_on("content.user_styles");
    let Some(state) = crate::state::BruState::instance() else {
        return;
    };
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
        let url = CefString::from(&frame.url()).to_string();
        let code = if on {
            match script_for(&url) {
                Some(code) => code,
                // Off by absence rather than by the switch: nothing to put in, and nothing a
                // previous load could have left, so nothing to say.
                None => removal_script(),
            }
        } else {
            removal_script()
        };
        frame.execute_java_script(Some(&CefString::from(code.as_str())), None, 0);
    }
}

/// Put the user's stylesheets into one frame, if it has any. The `on_load_start` caller.
pub fn inject(frame: &mut Frame, url: &str) {
    if !crate::settings::is_on("content.user_styles") {
        return;
    }
    let Some(code) = script_for(url) else {
        return;
    };
    frame.execute_java_script(Some(&CefString::from(code.as_str())), None, 0);
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

    /// Nothing to apply is the usual answer, and it must not be a `<style>` with nothing in it.
    #[test]
    fn a_site_with_no_folder_gets_no_script() {
        assert!(script_for("https://a-site-nobody-has-styled.example/").is_none());
    }
}
