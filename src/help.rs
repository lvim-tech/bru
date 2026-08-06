//! `bru://help` — every key and every command, generated from the tables bru actually runs on.
//!
//! It is built at request time from [`crate::config::Bindings`] and [`crate::exec::is_live`], not
//! written by hand, for one reason: a help page maintained separately drifts, and a help page that
//! disagrees with the browser is worse than no help page. If `config.lua` rebinds a key, this shows
//! the user's key. If a milestone implements a command, its rows stop saying "not yet".
//!
//! Served like the rest of the chrome, over the `bru://` scheme — see `src/chrome.rs`.

use crate::commands;
use crate::config::Bindings;
use crate::modes::Mode;

/// The page, as HTML. Styled from `chrome.css` and the theme, like the strips.
pub fn page(bindings: &Bindings) -> String {
    let rows = bindings.all();

    let (live, total) = rows.iter().fold((0usize, 0usize), |(live, total), (_, _, cmd)| {
        let ok = commands::parse(cmd).map(|c| crate::exec::is_live(&c)).unwrap_or(false);
        (live + usize::from(ok), total + 1)
    });

    let mut out = String::with_capacity(64 * 1024);
    out.push_str(
        r#"<!doctype html>
<meta charset="utf-8">
<title>bru — keys and commands</title>
<link rel="stylesheet" href="chrome.css">
<link rel="stylesheet" href="theme.css">
<body data-view="help">
<main id="help">
"#,
    );

    out.push_str(&format!(
        "<h1>bru</h1>\n<p class=\"summary\">{live} of {total} bindings do something today. \
         The rest are bound and parsed so that a chain like <code>gg</code> still works, \
         and say so when pressed.</p>\n"
    ));

    for mode in Mode::ALL {
        let in_mode: Vec<_> = rows.iter().filter(|(m, _, _)| *m == mode).collect();
        if in_mode.is_empty() {
            continue;
        }
        out.push_str(&format!("<h2>{}</h2>\n<table>\n", escape(mode.name())));
        for (_, keys, cmd) in in_mode {
            let live = commands::parse(cmd)
                .map(|c| crate::exec::is_live(&c))
                .unwrap_or(false);
            out.push_str(&format!(
                "<tr class=\"{}\"><td class=\"keys\">{}</td><td class=\"cmd\">{}</td><td class=\"state\">{}</td></tr>\n",
                if live { "live" } else { "todo" },
                escape(keys),
                escape(cmd),
                if live { "" } else { "not yet" },
            ));
        }
        out.push_str("</table>\n");
    }

    out.push_str("</main>\n");
    out
}

/// The page is built from command strings, which come from `config.lua` — the user's own file, but
/// still a file, and one typo away from putting a `<` where markup begins.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn bindings() -> Bindings {
        crate::config::Config::load_from(None).bindings
    }

    #[test]
    fn every_binding_appears() {
        let b = bindings();
        let html = page(&b);
        let rows = html.matches("<tr class=").count();
        assert_eq!(rows, b.all().len(), "one row per binding, no more and no less");
        assert!(rows > 200, "the qutebrowser defaults are there: {rows} rows");
    }

    #[test]
    fn the_page_says_which_keys_work() {
        let html = page(&bindings());
        // `j` scrolls today, and so does `yy` since src/clip.rs; `q` records a macro since
        // src/macros.rs, and `.` is what is bound and waiting now.
        assert!(html.contains(r#"<tr class="live"><td class="keys">j</td><td class="cmd">scroll down</td>"#));
        assert!(html.contains(r#"<tr class="live"><td class="keys">yy</td><td class="cmd">yank</td>"#));
        assert!(html.contains(r#"<tr class="live"><td class="keys">q</td><td class="cmd">macro-record</td>"#));
        assert!(html.contains(r#"<td class="cmd">cmd-repeat-last</td><td class="state">not yet</td>"#));
    }

    /// A `config.lua` that rebinds a key must change the page, or it is documentation of something
    /// other than this browser.
    #[test]
    fn it_describes_the_users_bindings_and_not_qutebrowsers() {
        let mut b = bindings();
        b.bind("normal", "ZX", "scroll down").expect("a valid binding");
        let html = page(&b);
        assert!(html.contains("ZX"), "a rebound key has to show up");
    }

    #[test]
    fn markup_in_a_command_string_cannot_escape_its_cell() {
        let mut b = bindings();
        b.bind("normal", "ZY", "open <script>alert(1)</script>")
            .expect("a valid binding");
        let html = page(&b);
        assert!(!html.contains("<script>alert(1)"), "it must arrive as text");
        assert!(html.contains("&lt;script&gt;alert(1)"));
    }
}

/// The page for whatever bindings are loaded right now.
///
/// `chrome.rs` serves this on every request, so a `config.lua` reloaded in a later milestone is
/// reflected without a restart.
pub fn current_page() -> String {
    match crate::state::BruState::instance() {
        Some(state) => {
            let bindings = state.lock().expect("state mutex poisoned").bindings_snapshot();
            match bindings {
                Some(bindings) => page(&bindings),
                None => "<!doctype html><meta charset=\"utf-8\"><body>no bindings loaded".to_string(),
            }
        }
        None => "<!doctype html><meta charset=\"utf-8\"><body>no browser state".to_string(),
    }
}
