//! Turning a site's Content Security Policy off, by name, so bru's own stylesheet can go in.
//!
//! ## The hole this fills, and how it was found
//!
//! Measured 2026-08-08 while auditing which selectors in the per-site stylesheets still match
//! anything: `html.duckduckgo.com` rendered **white** on a dark theme, with `#bru-userstyle` sitting
//! in the document holding all 75,986 characters of CSS. The element was there, `document.contains`
//! was true, its namespace was XHTML and its parent and position were identical to a page where the
//! theme works — and `styleElem.sheet` was **null**, so `document.styleSheets.length` was 1, the
//! site's own.
//!
//! It is not the keeper and not the element. Removing and re-appending it, re-setting its
//! `textContent`, moving it into `<head>`, and creating a brand-new `<style>` from the page's own JS
//! all left `sheet` null. The `securitypolicyviolation` event named it — the event is async, so it
//! has to be collected on `window` and read in a second call:
//!
//! ```text
//! style-src-elem | default-src 'none' ; connect-src https://duck.ai …
//! ```
//!
//! **Every site file is affected, and so is `scrollbar.rs`**: both go into the page as `<style>`
//! elements through the same keeper in `chrome/userstyle.js`. What still works is an inline style
//! *attribute* (`document.body.style.outline` measured 2.9px), because a policy with no
//! `style-src-attr` of its own does not cover it — which is why such a page looks alive while the
//! theme is simply missing.
//!
//! ## Why a list, and not a switch
//!
//! Bypassing CSP for the theme bypasses it for **everything the page does** — every script source
//! and every connection the policy was written to refuse. That is not a price to pay silently on
//! every site to fix the colours on two, so the choice was the user's and the answer was a list:
//! the hole is opened by name, and only where somebody has looked. `content.csp.bypass` ships
//! **empty**, which means a bru nobody has configured never turns a policy off.
//!
//! ## An entry covers its subdomains, exactly as a styles folder does
//!
//! `duckduckgo.com` in the list covers `html.duckduckgo.com` and `lite.duckduckgo.com`, because
//! [`crate::userstyles::covers`] is the same suffix walk that decides which `styles/<host>/` folders
//! apply to a page. Two spellings of "this site" that disagreed would be a trap, and this one is
//! already written down: the last label alone is never a candidate, so `com` cannot be an entry that
//! matches a third of the web.
//!
//! ## Where it is applied, and the lag that decided how
//!
//! `Page.setBypassCSP` through `BrowserHost::execute_dev_tools_method` (bindings
//! x86_64_unknown_linux_gnu.rs:12670), from `on_before_browse`.
//!
//! **Calling it there and letting the navigation proceed is wrong, and the first build did exactly
//! that.** Measured 2026-08-08 against a local page that serves
//! `default-src 'none'; style-src 'none'` and an inline script, so both halves of a policy can be
//! seen at once: every answer was **one navigation late**. Listed host, first load — policy still
//! honoured. Then an unlisted host — policy bypassed, the previous page's answer. Then the listed
//! host again — honoured. The flag is set on the frame that is asking, and the document being
//! fetched has already been given its policy by the time the DevTools message is handled; what our
//! call actually configures is the document *after* it.
//!
//! So the transition is what has to be paid for, and it is paid once: when the answer for the
//! URL about to load differs from the one in force for that browser, the flag is set, the
//! navigation is **cancelled**, and the same URL is loaded again from a posted task — by which time
//! the new value is in force and the document comes up under it. Nothing has been fetched at that
//! point, so nothing flashes; `on_before_browse` runs before the request goes out.
//!
//! **Two kinds of navigation are never cancelled**, and both for the same reason: re-issuing them
//! with `load_url` would not be the same navigation.
//!
//! - **A POST.** `load_url` sends a GET, which loses whatever was typed into the form. A form post
//!   to a site whose CSP the user has chosen to bypass is rare; losing a form's contents to make its
//!   colours right is not a trade this gets to make.
//! - **A step through history**, which `TransitionType::FORWARD_BACK_FLAG` names. `load_url` pushes
//!   a *new* entry, so cancelling `<Back>` and re-issuing it would leave the tab on the right page
//!   with the history wrong and nothing to go forward to — a worse bug than the one being fixed.
//!
//! Both take the policy they were given for one load, and the load after them is right.
//!
//! **The state is remembered per browser, and forgotten when the browser closes.** Chromium starts
//! every browser honouring policies, so an unknown browser is `false`; if a closed browser's
//! identifier were left in the map, a new tab that reused the number would inherit an answer that
//! was never set on it.

use cef::*;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// The setting that holds the hosts. Read at navigation time; see [`bypassed`].
pub const SETTING: &str = "content.csp.bypass";

/// What each browser has actually been told, by identifier. See the module note on why this is
/// remembered rather than set afresh every time.
fn in_force() -> &'static Mutex<HashMap<i32, bool>> {
    static IN_FORCE: OnceLock<Mutex<HashMap<i32, bool>>> = OnceLock::new();
    IN_FORCE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Forget a browser that has gone, so its identifier can be reused without carrying its answer.
/// Called from `ipc::on_before_close`.
pub fn forget(id: i32) {
    in_force().lock().expect("csp mutex poisoned").remove(&id);
}

/// Whether `url`'s host is one the user has listed, or a subdomain of one.
///
/// Split out from the CEF call so the matching can be tested without a browser.
fn bypassed(url: &str, listed: &[String]) -> bool {
    if listed.is_empty() {
        return false;
    }
    listed.iter().any(|entry| crate::userstyles::covers(entry, url))
}

/// Put the right answer in force for the document about to load, and say whether this navigation
/// has to be cancelled so that it can be.
///
/// Called from `RequestHandler::on_before_browse`, whose return value is this one. Subframes are
/// left alone: the flag is the browser's, an iframe cannot be given one of its own, and a page whose
/// host is listed carries its frames with it anyway.
pub fn on_before_browse(
    browser: Option<&mut Browser>,
    frame: Option<&Frame>,
    url: &str,
    method: &str,
    reissuable: bool,
) -> bool {
    let Some(frame) = frame else { return false };
    if frame.is_main() == 0 {
        return false;
    }
    let Some(browser) = browser else { return false };
    let id = browser.identifier();
    let wanted = bypassed(url, &crate::settings::list_of(SETTING));
    {
        let mut map = in_force().lock().expect("csp mutex poisoned");
        // Chromium starts out honouring policies, so a browser nothing has been set on is `false`.
        if map.get(&id).copied().unwrap_or(false) == wanted {
            return false;
        }
        map.insert(id, wanted);
    }
    let Some(host) = browser.host() else { return false };
    set_bypass(&host, wanted);
    if !should_reissue(method, reissuable) {
        return false;
    }
    let mut task = Reload::new(id, url.to_string());
    post_task(ThreadId::UI, Some(&mut task));
    true
}

/// Whether a navigation may be asked for a second time, now that the flag has been changed.
///
/// **Split out so it can be tested without a browser**, the way `bypassed` already is. The decision
/// itself is the module note's: the bypass value only reaches the *next* document, so the one being
/// asked for has to be asked for again — and that is only honest for a navigation that re-issuing
/// reproduces.
///
/// A POST is not one. Replaying it as a GET would drop the body, so a form submitted to a listed
/// host would arrive as an empty request and the user would watch their form vanish; the page loads
/// with its policy still honoured instead, which is the smaller wrong. A navigation Chromium says is
/// not reissuable is the other: a step through history re-issued would push a new entry and lose the
/// way forward.
fn should_reissue(method: &str, reissuable: bool) -> bool {
    method.eq_ignore_ascii_case("GET") && reissuable
}

/// The one DevTools call, both ways.
///
/// `message_id` is any positive integer — bru sends no observer, so nothing correlates a reply, and
/// the C API only requires it to be non-zero. The params dictionary is the method's whole argument:
/// `{"enabled": <bool>}`.
fn set_bypass(host: &BrowserHost, enabled: bool) {
    let Some(mut params) = dictionary_value_create() else {
        return;
    };
    params.set_bool(Some(&CefString::from("enabled")), i32::from(enabled));
    host.execute_dev_tools_method(
        1,
        Some(&CefString::from("Page.setBypassCSP")),
        Some(&mut params),
    );
}

// The cancelled navigation, asked for again once the flag is in force.
//
// It carries the identifier rather than the browser because it outlives the callback that made it,
// and the tab can be closed between the cancel and this: `browser_with_id` then answers `None` and
// the task does nothing, which is the right amount of nothing. The comment is out here because
// `wrap_task!` matches the struct itself and a doc comment on it expands to an attribute the macro
// has no rule for.
wrap_task! {
    struct Reload {
        id: i32,
        url: String,
    }

    impl Task {
        fn execute(&self) {
            let Some(state) = crate::state::BruState::instance() else {
                return;
            };
            let browser = state
                .lock()
                .expect("state mutex poisoned")
                .browser_with_id(self.id);
            if let Some(frame) = browser.and_then(|browser| browser.main_frame()) {
                frame.load_url(Some(&CefString::from(self.url.as_str())));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listed(entries: &[&str]) -> Vec<String> {
        entries.iter().map(|entry| entry.to_string()).collect()
    }

    /// The empty list is the shipped one, and it must never open anything.
    #[test]
    fn a_bru_nobody_has_configured_bypasses_nothing() {
        assert!(!bypassed("https://html.duckduckgo.com/html/?q=x", &[]));
        assert!(!bypassed("https://example.com/", &[]));
    }

    #[test]
    fn a_listed_host_is_bypassed_and_another_is_not() {
        let list = listed(&["html.duckduckgo.com"]);
        assert!(bypassed("https://html.duckduckgo.com/html/?q=x", &list));
        assert!(!bypassed("https://duckduckgo.com/?q=x", &list));
        assert!(!bypassed("https://example.com/", &list));
    }

    /// The reason the list can name the site rather than each of its hosts — and the reason the
    /// two measured DuckDuckGo endpoints need one entry between them, not two.
    #[test]
    fn an_entry_covers_its_subdomains() {
        let list = listed(&["duckduckgo.com"]);
        assert!(bypassed("https://html.duckduckgo.com/html/?q=x", &list));
        assert!(bypassed("https://lite.duckduckgo.com/lite/?q=x", &list));
        assert!(bypassed("https://duckduckgo.com/", &list));
        // Not a site that merely ends in the same letters.
        assert!(!bypassed("https://notduckduckgo.com/", &list));
    }

    /// `www.` is stripped by the host walk, so both spellings are the one site.
    #[test]
    fn www_is_not_a_subdomain_here() {
        let list = listed(&["google.com"]);
        assert!(bypassed("https://www.google.com/", &list));
    }

    /// A page bru serves itself has no policy to bypass and no host a user would list.
    #[test]
    fn brus_own_pages_are_not_matched_by_an_ordinary_entry() {
        let list = listed(&["duckduckgo.com"]);
        assert!(!bypassed("bru://chrome/help", &list));
        assert!(!bypassed("about:blank", &list));
    }
    /// A form POST to a listed host must not be replayed as a GET.
    ///
    /// The bypass only reaches the next document, so a transition re-asks for the navigation — and
    /// re-asking is honest only when it reproduces what was asked. A POST re-issued is a GET with no
    /// body: the form is gone. A navigation Chromium calls not-reissuable is a step through history,
    /// which re-issued would push a new entry and lose the way forward. Both load with the old policy
    /// still in force instead, which is the smaller wrong and is what the module note says.
    ///
    /// Verified by flipping the predicate to `true`: the POST and history rows fail.
    #[test]
    fn only_a_reissuable_get_is_asked_for_twice() {
        assert!(should_reissue("GET", true));
        assert!(should_reissue("get", true), "the method is compared without case");
        assert!(!should_reissue("POST", true), "a POST re-issued loses its body");
        assert!(!should_reissue("GET", false), "a history step re-issued loses the way forward");
        assert!(!should_reissue("POST", false));
    }

}
