//! Favicons for the tab strip.
//!
//! DESIGN.md draws the strip as "thin, favicon and title", and `chrome/top.js` has always made a
//! `<span class="favicon">` for every tab. It was always empty: nothing downloaded an icon.
//!
//! Chromium finds the icon and says so through `DisplayHandler::on_favicon_urlchange` (bindings
//! 17656), which hands over the `<link rel=icon>` URLs of the page. `BrowserHost::download_image`
//! (12628) fetches one — `is_favicon = 1`, so no cookies are sent or accepted for it — and answers
//! on a `DownloadImageCallback` (12465) with a `cef_image_t`. `Image::as_png` (4153) turns that into
//! bytes, and the bytes become a `data:` URL, which is the only shape the chrome can draw: the strip
//! is an ordinary web page served from `bru://`, and it cannot fetch an image from a site.
//!
//! **Icons are keyed by origin, not by tab.** That is what a browser's favicon cache does, and here
//! it buys two things: a second tab on a site already visited draws its icon with no download and no
//! wait, and the map survives a tab moving, closing or being renumbered — the strip looks its icon
//! up by the URL it is already drawing, so nothing has to be kept in step with the tab list.

use cef::*;
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

/// Density-independent pixels. `chrome.css` draws the icon in a 16px box; asking for 32 leaves
/// something to downscale from on a HiDPI output rather than something to blur up. Anything larger
/// is filtered out by CEF, and a site that only ships a 256px icon has it resized down for us.
const MAX_SIZE: u32 = 32;

/// origin → `data:image/png;base64,…`.
fn icons() -> &'static Mutex<HashMap<String, String>> {
    static ICONS: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    ICONS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The icon URLs a download has already been started for. `on_favicon_urlchange` fires on every
/// navigation and often more than once per page, and without this a site visited in five tabs would
/// start five downloads for the same bytes.
fn asked() -> &'static Mutex<HashSet<String>> {
    static ASKED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    ASKED.get_or_init(|| Mutex::new(HashSet::new()))
}

/// From `DisplayHandler::on_favicon_urlchange`.
///
/// The list arrives as a `&mut CefStringList`, and cef-rs can only read one by consuming it —
/// `IntoIterator` is implemented for the owned form and a clone of a borrowed list reads as empty.
/// `mem::take` hands over the borrowed handle and leaves CEF's `&mut` holding a fresh empty list
/// that the callback wrapper frees on its way out; CEF's own list is never freed by bru.
pub fn on_favicon_urls(browser: Option<&mut Browser>, icon_urls: Option<&mut CefStringList>) {
    debug_assert_ne!(currently_on(ThreadId::UI), 0);

    let (Some(browser), Some(icon_urls)) = (browser, icon_urls) else {
        return;
    };

    let Some(origin) = page_origin(browser) else {
        return;
    };

    // The first that can actually be fetched. Chromium lists them in document order, and a page
    // with several sizes lists the small one first as often as not; `max_image_size` sorts that out.
    let urls: Vec<String> = std::mem::take(icon_urls).into_iter().collect();
    let Some(url) = urls.into_iter().find(|url| url.starts_with("http")) else {
        return;
    };

    // Keyed on the icon URL and the origin together: the same icon may legitimately be wanted for
    // two origins, and neither should have to wait for the other's download.
    let key = format!("{origin} {url}");
    let first_time = match asked().lock() {
        Ok(mut asked) => asked.insert(key),
        Err(_) => false,
    };
    if !first_time {
        return;
    }

    let Some(host) = browser.host() else {
        return;
    };
    let mut callback = FaviconCallback::new(origin);
    host.download_image(
        Some(&CefString::from(url.as_str())),
        1,
        MAX_SIZE,
        0,
        Some(&mut callback),
    );
}

/// Hand one icon to the strip, which keeps it. Called once per icon, when it arrives.
fn push_one(origin: &str, data_url: &str) {
    crate::ipc::top_chrome_eval(&format!(
        "window.bru && window.bru.favicon && window.bru.favicon(\"{}\", \"{}\");",
        crate::ipc::json_escape(origin),
        crate::ipc::json_escape(data_url),
    ));
}

/// Hand the strip every icon downloaded so far. Called when the strip announces itself, because a
/// page it never heard about is a page it will not draw an icon for.
pub fn push_all() {
    let Ok(icons) = icons().lock() else {
        return;
    };
    for (origin, data_url) in icons.iter() {
        push_one(origin, data_url);
    }
}

// The one download callback. CEF runs it on the UI thread, which is where the push has to happen
// anyway. (The wrap_ macros take no doc comment on the struct they declare — trap 8.)
wrap_download_image_callback! {
    struct FaviconCallback {
        origin: String,
    }

    impl DownloadImageCallback {
        fn on_download_image_finished(
            &self,
            _image_url: Option<&CefString>,
            _http_status_code: ::std::os::raw::c_int,
            image: Option<&mut Image>,
        ) {
            // The status code is deliberately not checked. A favicon served from the browser cache,
            // or one embedded in the page as a `data:` URL, reports 0 rather than 200 — and CEF
            // hands over no image at all when the download really failed, which is the honest test.
            let Some(image) = image else {
                return;
            };
            if image.is_empty() != 0 {
                return;
            }

            // Scale factor 1.0 and transparency kept: the strip's background is the theme's, and an
            // icon flattened onto white would sit in a white box on a dark bar.
            // Both out-parameters have to be given, and that is not optional in the way the
            // `Option<&mut _>` in the signature suggests. CEF's own C-to-C++ shim starts with
            // `DCHECK(pixel_width); if (!pixel_width) return NULL;`, so a `None` here is answered
            // with no image at all rather than with an image whose size you did not ask for.
            // Measured 2026-08-06: `as_png(1.0, 1, None, None)` on a 32×32 favicon that had just
            // downloaded with status 200 and reported `has_representation(1.0) == 1` returned
            // `None`; the same call with both sizes returned the bytes.
            let (mut width, mut height) = (0, 0);
            let Some(png) = image.as_png(1.0, 1, Some(&mut width), Some(&mut height)) else {
                return;
            };
            let mut bytes = vec![0u8; png.size()];
            let copied = png.data(Some(&mut bytes), 0);
            if copied == 0 {
                return;
            }
            bytes.truncate(copied);

            let encoded = CefString::from(&base64_encode(Some(&bytes))).to_string();
            let data_url = format!("data:image/png;base64,{encoded}");

            if let Ok(mut icons) = icons().lock() {
                icons.insert(self.origin.clone(), data_url.clone());
            }
            push_one(&self.origin, &data_url);
        }
    }
}

/// The origin of the page a browser is on, spelled the way `new URL(u).origin` spells it in the
/// strip — that string is the key on both sides and a mismatch is an icon that never appears.
fn page_origin(browser: &mut Browser) -> Option<String> {
    let url = CefString::from(&browser.main_frame()?.url()).to_string();
    origin_of(&url)
}

/// `https://User@Example.COM:443/a?b#c` → `https://example.com`.
///
/// Only http and https have favicons worth caching: `bru://` is the chrome's own, and a `data:` or
/// `file:` page has no origin to share an icon with. JavaScript answers `"null"` for those, and a
/// key that can never be looked up is worse than no key.
fn origin_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    // Authority only: everything up to the first `/`, `?` or `#`.
    let authority = rest.split(['/', '?', '#']).next()?;
    // Credentials are not part of an origin. `rsplit_once` so a password containing `@` is handled.
    let host = authority
        .rsplit_once('@')
        .map(|(_, host)| host)
        .unwrap_or(authority);
    // Nor is the default port. An IPv6 literal is bracketed, so the last `:` inside `[...]` is not
    // a port separator — only a `:` after the closing bracket is.
    let host = match host.rsplit_once(':') {
        Some((head, port)) if !head.ends_with(']') || head.is_empty() => {
            match (scheme.as_str(), port) {
                ("https", "443") | ("http", "80") => head,
                _ => host,
            }
        }
        _ => host,
    };
    let host = host.to_ascii_lowercase();
    if host.is_empty() {
        return None;
    }
    Some(format!("{scheme}://{host}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_origin_is_spelled_the_way_javascript_spells_it() {
        // The strip looks an icon up under `new URL(tab.url).origin`. These are the answers that
        // gives, checked against the URL Standard's definition of a serialised origin.
        for (url, want) in [
            ("https://example.com/", "https://example.com"),
            ("https://example.com", "https://example.com"),
            ("https://example.com/a/b?c=d#e", "https://example.com"),
            ("http://example.com:8080/x", "http://example.com:8080"),
            // Default ports are dropped.
            ("https://example.com:443/x", "https://example.com"),
            ("http://example.com:80/x", "http://example.com"),
            // A non-default port on the other scheme is not.
            ("https://example.com:80/x", "https://example.com:80"),
            // Credentials are not part of an origin.
            ("https://user:pw@example.com/x", "https://example.com"),
            // And the host is compared case-insensitively.
            ("HTTPS://Example.COM/x", "https://example.com"),
        ] {
            assert_eq!(origin_of(url).as_deref(), Some(want), "{url}");
        }
    }

    #[test]
    fn only_the_schemes_that_can_share_an_icon_get_a_key() {
        for url in [
            "bru://chrome/top.html",
            "file:///home/x/page.html",
            "data:text/html;base64,PGgxPkE8L2gxPg==",
            "about:blank",
            "",
            "https://",
        ] {
            assert_eq!(origin_of(url), None, "{url} should have no favicon key");
        }
    }

    #[test]
    fn one_icon_is_asked_for_once() {
        // `on_favicon_urlchange` fires on every navigation, and a site open in five tabs would
        // otherwise start five downloads of the same bytes.
        asked().lock().unwrap().clear();
        let key = "https://example.com https://example.com/favicon.ico".to_string();
        assert!(asked().lock().unwrap().insert(key.clone()));
        assert!(!asked().lock().unwrap().insert(key));
        asked().lock().unwrap().clear();
    }
}
