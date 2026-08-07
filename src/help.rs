//! `bru://help` — every key and every command, generated from the tables bru actually runs on.
//!
//! It is built at request time from [`crate::config::Bindings`], [`crate::exec::is_live`] and
//! [`crate::exec::refusal`], not written by hand, for one reason: a help page maintained separately
//! drifts, and a help page that disagrees with the browser is worse than no help page. If
//! `config.lua` rebinds a key, this shows the user's key. If a milestone implements a command, its
//! rows stop saying "not yet".
//!
//! **A row is in one of three states, and the third one is why this file was touched again.** Live
//! and "not yet" are not enough: thirteen of the 264 default bindings name something CEF 151 cannot
//! do at all, and marking those "not yet" reads as a promise. They say **refused**, with the reason
//! the module that measured it wrote — see [`crate::exec::refusal`]. The distinction is the
//! difference between a key waiting for work and a key waiting for nothing.
//!
//! Served like the rest of the chrome, over the `bru://` scheme — see `src/chrome.rs`.

use crate::commands;
use crate::config::Bindings;
use crate::modes::Mode;

/// What a row says about its binding.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    /// Pressing it does something.
    Live,
    /// Bound, parsed, and waiting for a milestone.
    NotYet,
    /// Bound, parsed, and waiting for nothing — with the reason.
    Refused(&'static str),
}

impl State {
    /// The one place a command string is turned into a row's state, so the page and the count
    /// cannot disagree about a binding.
    fn of(cmd: &str) -> State {
        let Ok(command) = commands::parse(cmd) else {
            // A `config.lua` naming a command bru has never heard of. Not refused — bru may grow
            // it — and not live.
            return State::NotYet;
        };
        if crate::exec::is_live(&command) {
            return State::Live;
        }
        match crate::exec::refusal(&command) {
            Some(why) => State::Refused(why),
            None => State::NotYet,
        }
    }

    /// The `<tr>` class, which is what `chrome.css` styles by.
    fn class(self) -> &'static str {
        match self {
            State::Live => "live",
            State::NotYet => "todo",
            State::Refused(_) => "refused",
        }
    }

    /// The right-hand column.
    fn label(self) -> &'static str {
        match self {
            State::Live => "",
            State::NotYet => "not yet",
            State::Refused(_) => "refused",
        }
    }
}

/// The page, as HTML. Styled from `chrome.css` and the theme, like the strips.
pub fn page(bindings: &Bindings) -> String {
    let rows = bindings.all();

    // No total: the summary counts what acts and what is refused, and does not divide one by the
    // other or by anything else.
    let (live, refused) = rows.iter().fold((0usize, 0usize), |(live, refused), (_, _, cmd)| {
        match State::of(cmd) {
            State::Live => (live + 1, refused),
            State::Refused(_) => (live, refused + 1),
            State::NotYet => (live, refused),
        }
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

    // **A count, not a fraction, and no machinery.** This said "251 of 264 bindings do something
    // today. The rest are bound and parsed so that a chain like `gg` still works…" — a sentence
    // that explained bru's trie to someone who opened the page to find out what a key does. Both
    // halves were wrong for the reader: the "x of y" reads as a score against somebody else's
    // total, and *why* a dead key stays in the table is an implementation detail. It is a real
    // reason and it belongs where it is acted on — dropping those rows would make `t` answer
    // NoMatch and eat a pending chain — but it belongs in `config.rs`, not here.
    //
    // What is left is the two numbers and the promise that each dead key says why.
    out.push_str(&format!(
        "<h1>bru</h1>\n<p class=\"summary\">{live} keys do something. \
         {refused} say why they do not.</p>\n"
    ));

    for mode in Mode::ALL {
        let in_mode: Vec<_> = rows.iter().filter(|(m, _, _)| *m == mode).collect();
        if in_mode.is_empty() {
            continue;
        }
        out.push_str(&format!("<h2>{}</h2>\n<table>\n", escape(mode.name())));
        for (_, keys, cmd) in in_mode {
            let state = State::of(cmd);
            // The reason goes inside the command cell rather than into a row of its own: one `<tr>`
            // per binding is what `every_binding_appears` counts, and what keeps the striping from
            // pairing a row with its own explanation.
            let why = match state {
                State::Refused(why) => format!("<span class=\"why\">{}</span>", escape(why)),
                _ => String::new(),
            };
            out.push_str(&format!(
                "<tr class=\"{}\"><td class=\"keys\">{}</td><td class=\"cmd\">{}{why}</td><td class=\"state\">{}</td></tr>\n",
                state.class(),
                escape(keys),
                escape(cmd),
                state.label(),
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

    /// **The page never names qutebrowser.** Asked for by the user 2026-08-07: bru's own help is
    /// not the place to measure bru against anything — it states bru's numbers and bru's reasons.
    ///
    /// The refusals are where this went wrong. They were argued by comparison — "qutebrowser drives
    /// it through QWebEngineSettings::PluginsEnabled", "the key is dead in qutebrowser 3.7.0 too" —
    /// which is right in a commit message and wrong on a page someone opens to find out what a key
    /// does. The measurements stayed; the comparisons went.
    ///
    /// Comments and commit messages are not covered and should not be: `.claude/` and the source
    /// are where the port is documented, and naming what a behaviour was ported from is how the
    /// next person checks it.
    #[test]
    fn the_page_measures_bru_against_nothing() {
        let html = page(&bindings());
        let text = html.to_lowercase();
        assert!(
            !text.contains("qutebrowser"),
            "the help page names qutebrowser; it states bru's own numbers and reasons only"
        );
        // `qute://` and `qutebrowser`'s own file names are the same mistake wearing a shorter word.
        for needle in ["qute://", "configdata.yml", "qwebengine"] {
            assert!(!text.contains(needle), "the help page still points at {needle:?}");
        }
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
        // src/macros.rs.
        assert!(html.contains(r#"<tr class="live"><td class="keys">j</td><td class="cmd">scroll down</td>"#));
        assert!(html.contains(r#"<tr class="live"><td class="keys">yy</td><td class="cmd">yank</td>"#));
        assert!(html.contains(r#"<tr class="live"><td class="keys">q</td><td class="cmd">macro-record</td>"#));

        // **No default binding says "not yet" any more.** Every one of qutebrowser's 264 either
        // acts or is refused, and this is where that stops being a claim: the filter is `is_live`
        // and not "does it parse to a variant" — `command-history-prev` and every `rl-*` parse to
        // `Unimplemented` and are live all the same, because they reach `cmdline.rs` by name.
        // Asking the parser here marked them "not yet" and is the same mistake that undercounted
        // the live bindings by 17 for a whole stage.
        let waiting: Vec<&str> = crate::config::DEFAULT_BINDINGS
            .iter()
            .map(|(_mode, _keys, text)| *text)
            .filter(|text| State::of(text) == State::NotYet)
            .collect();
        assert!(waiting.is_empty(), "these still say \"not yet\": {waiting:?}");
        assert!(!html.contains(">not yet<"), "and so the page must not print the words");

        // The state is still reachable, and still rendered — a `config.lua` may name a command
        // qutebrowser has and bru has not built. Without this the branch would be untested the
        // moment the defaults stopped using it.
        let mut b = bindings();
        b.bind("normal", "ZW", "click-element id foo").expect("a valid binding");
        let html = page(&b);
        assert_eq!(State::of("click-element id foo"), State::NotYet);
        assert!(html.contains(
            r#"<tr class="todo"><td class="keys">ZW</td><td class="cmd">click-element id foo</td><td class="state">not yet</td></tr>"#
        ));
    }

    /// The third state, and the thirteen rows that are in it.
    ///
    /// "not yet" against a key nothing can ever implement invites the same investigation every few
    /// months; three of them have been paid for already. These rows say **refused** and carry the
    /// reason the module that measured it wrote.
    #[test]
    fn a_binding_nothing_can_implement_says_refused_and_why() {
        let html = page(&bindings());

        // The twelve `t**` rows: six `content.plugins`, six `content.cookies.accept`.
        let twelve: Vec<&str> = crate::config::DEFAULT_BINDINGS
            .iter()
            .filter(|(mode, keys, _)| *mode == "normal" && keys.starts_with('t') && keys.len() == 3)
            .filter(|(_, _, cmd)| {
                cmd.contains("content.plugins") || cmd.contains("content.cookies.accept")
            })
            .map(|(_, keys, _)| *keys)
            .collect();
        assert_eq!(twelve.len(), 12, "the t** rows moved: {twelve:?}");
        for keys in &twelve {
            let row = format!(r#"<td class="keys">{keys}</td>"#);
            let at = html.find(&row).unwrap_or_else(|| panic!("no row for {keys}"));
            let row = &html[..at];
            assert!(row.ends_with(r#"<tr class="refused">"#), "{keys} is not marked refused");
        }
        assert!(html.contains("NPAPI and PPAPI are gone"), "the plugins reason is not on the page");
        assert!(html.contains("no-3rdparty cannot be written per URL"), "the cookies reason is not");

        // And `<Return>` in hint mode, whose reason is `hints.rs`'s.
        assert!(html.contains(r#"<tr class="refused"><td class="keys">&lt;Return&gt;</td><td class="cmd">hint-follow"#));
        assert!(html.contains("there is never a hint waiting to be followed"));

        // Thirteen rows, and the summary says so rather than only the table.
        assert_eq!(html.matches(r#"<tr class="refused">"#).count(), 13);
        assert!(html.contains("13 say why they do not"), "the summary undercounts");
        // And it states counts rather than a fraction of anything — the user asked for that
        // 2026-08-07, and a helpful-looking "x of y" is exactly what crept back last time.
        assert!(!html.contains(" of 264"), "the summary is measuring against a total again");

        // A refused row is not a "not yet" row, or the third state is decoration.
        assert!(!html.contains(r#"<td class="cmd">hint-follow</td><td class="state">not yet</td>"#));

        // The twelve are chains — `config-cycle … ;; reload` — and the reason must come from the
        // half that is refused whichever half is written first. Asking `exec::refusal` for the
        // reversed spelling is what proves it does not depend on the order, which it did until a
        // deliberate break walked it past the `config-cycle` and into `reload`.
        let forwards = "config-cycle -p -u *://x/* content.plugins ;; reload";
        let backwards = "reload ;; config-cycle -p -u *://x/* content.plugins";
        assert!(matches!(State::of(forwards), State::Refused(_)));
        assert_eq!(State::of(forwards), State::of(backwards));
    }

    /// A reason is prose written by a person and lands inside a table cell. It goes through the
    /// same escape as everything else, and the page carries the escaped form.
    #[test]
    fn a_refusal_reason_cannot_escape_its_cell() {
        let html = page(&bindings());
        for (_, why) in crate::settings::REFUSED {
            assert!(html.contains(&escape(why)), "the escaped reason is not on the page");
            // The plugins reason contains ASCII quotes, so this is not a vacuous check: the raw
            // string must *not* be there.
            if escape(why) != *why {
                assert!(!html.contains(*why), "an unescaped reason reached the page");
            }
        }
        assert_ne!(
            escape(crate::settings::REFUSED[0].1),
            crate::settings::REFUSED[0].1,
            "if no reason needs escaping this test asserts nothing"
        );
        assert_eq!(escape("a <b> & \"c\""), "a &lt;b&gt; &amp; &quot;c&quot;");
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
