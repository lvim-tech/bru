//! The `bru://` scheme, and the chrome assets it serves.
//!
//! The tab strip and the status line are HTML pages, so they need an origin. A custom scheme gives
//! them one that is not `file://` and not a `data:` URI: a real, secure, CORS-enabled origin that
//! the message router's security check can name in one comparison (`bru://`).
//!
//! Everything except the theme is compiled in — the binary carries its own chrome, as DESIGN.md
//! requires ("bru ships a binary and generated themes, nothing a person edits"). `theme.css` is the
//! one exception: it is read from `~/.config/bru/theme.css` on every request, so `themer` can swap a
//! palette under a running browser and a reload is enough to see it.

use cef::wrapper::byte_read_handler::{ByteReadHandler, ByteStream};
use cef::wrapper::stream_resource_handler::StreamResourceHandler;
use cef::*;
use std::sync::{Arc, Mutex};

/// The scheme name, spelled once.
const SCHEME: &str = "bru";

/// Everything is served under one host, so `bru://chrome/top.html` is a URL and not a hostname
/// guess. A STANDARD scheme is parsed like http, which means relative `<link href="chrome.css">`
/// resolves the way an author expects.
const ORIGIN: &str = "bru://chrome";

const TOP_HTML: &[u8] = include_bytes!("../chrome/top.html");
const BOTTOM_HTML: &[u8] = include_bytes!("../chrome/bottom.html");
const CHROME_CSS: &[u8] = include_bytes!("../chrome/chrome.css");
const TOP_JS: &[u8] = include_bytes!("../chrome/top.js");
const BOTTOM_JS: &[u8] = include_bytes!("../chrome/bottom.js");
const COMPLETION_JS: &[u8] = include_bytes!("../chrome/completion.js");
// --- src/prompt.rs ---------------------------------------------------------
const PROMPT_JS: &[u8] = include_bytes!("../chrome/prompt.js");
// --- end src/prompt.rs -----------------------------------------------------
// --- src/cookies.rs --------------------------------------------------------
const COOKIES_JS: &[u8] = include_bytes!("../chrome/cookies.js");
// --- end src/cookies.rs ----------------------------------------------------

/// The theme bru ships with, used until themer has written one to `~/.config/bru/theme.css`.
///
/// It is a real generated file, copied verbatim from `lvim-colorscheme`'s `extras/bru/` — which is
/// what DESIGN.md means by "bru ships a binary and generated themes, nothing a person edits". A
/// hand-written stub is what was here first, and it defined 11 of the 62 custom properties
/// `chrome.css` reads, so the chrome rendered as unstyled black-on-white: the layout was right and
/// every colour was missing. A generated file cannot drift from the stylesheet that way, because
/// the same generator produces the whole vocabulary.
const DEFAULT_THEME_CSS: &str = include_str!("../chrome/theme.css");

/// Called from `App::on_register_custom_schemes`, which runs in **every** process — browser,
/// renderer, GPU, zygote. A renderer that never heard of `bru://` treats it as an opaque origin and
/// refuses to load the page, which is why the App has to reach `execute_process` too.
pub fn register_scheme(registrar: Option<&mut SchemeRegistrar>) {
    let Some(registrar) = registrar else {
        return;
    };

    // SchemeOptions implements no BitOr — verified against the bindings on 2026-08-06 — so the
    // flags are combined on the raw values.
    let options = (SchemeOptions::STANDARD.get_raw()
        | SchemeOptions::SECURE.get_raw()
        | SchemeOptions::CORS_ENABLED.get_raw()) as i32;

    registrar.add_custom_scheme(Some(&CefString::from(SCHEME)), options);
}

/// Called from `on_context_initialized`, before any browser exists. Registering the factory after a
/// browser has already asked for a `bru://` URL is a load error nobody can see the cause of.
pub fn register_factory() {
    let mut factory = BruSchemeHandlerFactory::new();
    register_scheme_handler_factory(Some(&CefString::from(SCHEME)), None, Some(&mut factory));
}

wrap_scheme_handler_factory! {
    pub struct BruSchemeHandlerFactory;

    impl SchemeHandlerFactory {
        fn create(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _scheme_name: Option<&CefString>,
            request: Option<&mut Request>,
        ) -> Option<ResourceHandler> {
            let url = request
                .map(|request| CefString::from(&request.url()).to_string())
                .unwrap_or_default();

            let (mime, bytes) = asset(&url)?;
            serve(mime, bytes)
        }
    }
}

/// Map a `bru://` URL to the bytes behind it. `None` means 404 — CEF shows its own error page,
/// which is the right answer for a typo in a `<link>`.
fn asset(url: &str) -> Option<(&'static str, Vec<u8>)> {
    // Chromium hands over the full URL including query and fragment; neither addresses an asset.
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let path = path.strip_prefix(ORIGIN)?;

    match path {
        "/top.html" => Some(("text/html", TOP_HTML.to_vec())),
        "" | "/" | "/bottom.html" => Some(("text/html", BOTTOM_HTML.to_vec())),
        "/chrome.css" => Some(("text/css", CHROME_CSS.to_vec())),
        "/top.js" => Some(("text/javascript", TOP_JS.to_vec())),
        "/bottom.js" => Some(("text/javascript", BOTTOM_JS.to_vec())),
        "/completion.js" => Some(("text/javascript", COMPLETION_JS.to_vec())),
        // --- src/prompt.rs -----------------------------------------------------------------
        "/prompt.js" => Some(("text/javascript", PROMPT_JS.to_vec())),
        // --- end src/prompt.rs -------------------------------------------------------------
        // Generated per request from the live binding table, so it can never drift from the keys
        // the browser is actually running on. See src/help.rs.
        "/help" | "/help.html" => Some(("text/html", crate::help::current_page().into_bytes())),
// --- src/history.rs --------------------------------------------------------
        // Generated per request from `history.sqlite` and the two mark files, for the same reason
        // `/help` is: a page written separately from what it describes drifts. See src/history.rs.
        "/history" | "/history.html" => Some(("text/html", crate::history::history_page().into_bytes())),
        "/bookmarks" | "/bookmarks.html" => Some(("text/html", crate::history::marks_page().into_bytes())),
// --- end src/history.rs ----------------------------------------------------
// --- src/settingspage.rs ---------------------------------------------------
        // What a bare `:set` opens. Generated per request from `settings::SETTINGS` and from what
        // Chromium answers, for the same reason `/help` is. See src/settingspage.rs.
        "/settings" | "/settings.html" => {
            Some(("text/html", crate::settingspage::current_page().into_bytes()))
        }
// --- end src/settingspage.rs -----------------------------------------------
// --- src/cookies.rs --------------------------------------------------------
        // The one generated page here that carries **no data at all**. Cookies come from an
        // asynchronous visitor and this function runs on the IO thread and must answer now, so what
        // is served is a shell that asks for its rows over `cefQuery`. See src/cookies.rs.
        "/cookies" | "/cookies.html" => Some(("text/html", crate::cookies::page().into_bytes())),
        "/cookies.js" => Some(("text/javascript", COOKIES_JS.to_vec())),
// --- end src/cookies.rs ----------------------------------------------------
        "/theme.css" => Some(("text/css", theme_css())),
// --- src/utilcmds.rs -------------------------------------------------------
        // `:version`, `:messages` and `:process`. Generated per request from what the running
        // browser knows, for the same reason `/help` is — and `/messages` is the one page here that
        // is asked with a query string, which is why `asset` takes the URL and not only the path.
        "/version" | "/version.html" => {
            Some(("text/html", crate::utilcmds::version_page().into_bytes()))
        }
        "/messages" | "/messages.html" => {
            let query = url.split_once('?').map(|(_, query)| query).unwrap_or("");
            let query = query.split('#').next().unwrap_or(query);
            Some(("text/html", crate::utilcmds::messages_page(query).into_bytes()))
        }
        // `/process` lists them all and `/process/<pid>` is the one `:process` opens.
        path if path == "/process" || path.starts_with("/process/") => Some((
            "text/html",
            crate::utilcmds::process_page(path.trim_start_matches("/process")).into_bytes(),
        )),
// --- end src/utilcmds.rs ---------------------------------------------------
        _ => None,
    }
}

/// The one asset that is not compiled in. Read per request rather than cached: the file belongs to
/// themer, which rewrites it whenever the desktop theme changes, and a cache would mean bru had to
/// be restarted to notice.
fn theme_css() -> Vec<u8> {
    match theme_path().and_then(|path| std::fs::read(path).ok()) {
        Some(bytes) => bytes,
        None => DEFAULT_THEME_CSS.as_bytes().to_vec(),
    }
}

fn theme_path() -> Option<std::path::PathBuf> {
    // XDG, hand-rolled rather than pulled in as a dependency: two environment variables.
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").filter(|dir| !dir.is_empty()) {
        return Some(std::path::PathBuf::from(dir).join("bru/theme.css"));
    }
    let home = std::env::var_os("HOME").filter(|home| !home.is_empty())?;
    Some(std::path::PathBuf::from(home).join(".config/bru/theme.css"))
}

/// Hand a `Vec<u8>` to CEF as a resource. The crate ships every piece of this: a ReadHandler over a
/// byte slice, a StreamReader over the handler, and a ResourceHandler over the stream.
fn serve(mime: &str, bytes: Vec<u8>) -> Option<ResourceHandler> {
    let mut read_handler = ByteReadHandler::new(Arc::new(Mutex::new(ByteStream::new(bytes))));
    let stream = stream_reader_create_for_handler(Some(&mut read_handler))?;
    Some(StreamResourceHandler::new_with_stream(
        mime.to_string(),
        stream,
    ))
}

// -----------------------------------------------------------------------------------------------
// The split between the two stylesheets, checked rather than asserted
// -----------------------------------------------------------------------------------------------
//
// `chrome/top.html` and `chrome/bottom.html` both carry the same comment — "chrome.css carries the
// layout and not one colour; theme.css carries the colours and not one rule" — and DESIGN.md rests
// on it: swapping a theme is swapping one file, which stays true only while no colour has leaked
// into the stylesheet that is compiled into this binary. Nothing checked it until now; the rule was
// six months of discipline with nothing underneath it, and one `color: #5a6158` typed in a hurry
// would have made `~/.config/bru/theme.css` silently partial.
//
// The check is a small CSS reader rather than a grep, and the reason is in the file it reads:
// `#completion` is a selector and `/* Chromium's #1f1f1f */` is prose, and a grep for `#` flags
// both. Declarations are the only thing a colour can hide in.

#[cfg(test)]
mod tests {
    /// One `(property, value)` per declaration, selectors and comments dropped.
    ///
    /// Depth counting is what separates the two: everything outside `{}` is a selector, everything
    /// inside is a declaration list. bru's two stylesheets have no nested at-rule — no `@media`, no
    /// `@supports` — so one counter is enough; a nested block would put its selector through
    /// `split_once(':')` and be discarded for having no value, which fails safe rather than loudly.
    fn declarations_in(css: &str) -> Vec<(String, String)> {
        let css = strip_comments(css);
        let mut out = Vec::new();
        let mut depth = 0usize;
        let mut current = String::new();
        for c in css.chars() {
            match c {
                '{' => {
                    depth += 1;
                    current.clear();
                }
                '}' => {
                    push_declaration(&current, &mut out);
                    depth = depth.saturating_sub(1);
                    current.clear();
                }
                ';' if depth > 0 => {
                    push_declaration(&current, &mut out);
                    current.clear();
                }
                _ if depth > 0 => current.push(c),
                _ => {}
            }
        }
        out
    }

    fn push_declaration(text: &str, out: &mut Vec<(String, String)>) {
        let Some((prop, value)) = text.split_once(':') else {
            return;
        };
        let (prop, value) = (prop.trim(), value.trim());
        if prop.is_empty() || value.is_empty() {
            return;
        }
        out.push((prop.to_string(), value.to_string()));
    }

    fn strip_comments(css: &str) -> String {
        let mut out = String::with_capacity(css.len());
        let mut rest = css;
        while let Some(at) = rest.find("/*") {
            out.push_str(&rest[..at]);
            // An unterminated comment swallows the remainder, which is what a browser does too.
            match rest[at + 2..].find("*/") {
                Some(end) => rest = &rest[at + 2 + end + 2..],
                None => return out,
            }
        }
        out.push_str(rest);
        out
    }

    /// The CSS colour functions. A bare `rgb(1,2,3)` is the spelling that would slip past a reader
    /// looking only for `#`.
    const COLOUR_FUNCTIONS: &[&str] = &[
        "rgb", "rgba", "hsl", "hsla", "hwb", "lab", "lch", "oklab", "oklch", "color", "color-mix",
    ];

    /// Every CSS named colour, and deliberately **not** `transparent` or `currentcolor`: those two
    /// name a relationship rather than a colour — "paint nothing" and "whatever the cascade already
    /// decided" — and `chrome/theme.css` itself uses `transparent` for qutebrowser's `none`.
    const NAMED_COLOURS: &str = "\
        aliceblue antiquewhite aqua aquamarine azure beige bisque black blanchedalmond blue \
        blueviolet brown burlywood cadetblue chartreuse chocolate coral cornflowerblue cornsilk \
        crimson cyan darkblue darkcyan darkgoldenrod darkgray darkgreen darkgrey darkkhaki \
        darkmagenta darkolivegreen darkorange darkorchid darkred darksalmon darkseagreen \
        darkslateblue darkslategray darkslategrey darkturquoise darkviolet deeppink deepskyblue \
        dimgray dimgrey dodgerblue firebrick floralwhite forestgreen fuchsia gainsboro ghostwhite \
        gold goldenrod gray green greenyellow grey honeydew hotpink indianred indigo ivory khaki \
        lavender lavenderblush lawngreen lemonchiffon lightblue lightcoral lightcyan \
        lightgoldenrodyellow lightgray lightgreen lightgrey lightpink lightsalmon lightseagreen \
        lightskyblue lightslategray lightslategrey lightsteelblue lightyellow lime limegreen linen \
        magenta maroon mediumaquamarine mediumblue mediumorchid mediumpurple mediumseagreen \
        mediumslateblue mediumspringgreen mediumturquoise mediumvioletred midnightblue mintcream \
        mistyrose moccasin navajowhite navy oldlace olive olivedrab orange orangered orchid \
        palegoldenrod palegreen paleturquoise palevioletred papayawhip peachpuff peru pink plum \
        powderblue purple rebeccapurple red rosybrown royalblue saddlebrown salmon sandybrown \
        seagreen seashell sienna silver skyblue slateblue slategray slategrey snow springgreen \
        steelblue tan teal thistle tomato turquoise violet wheat white whitesmoke yellow \
        yellowgreen";

    /// Every colour written literally in `css`, in the order they appear.
    ///
    /// Three spellings, because all three are what a colour looks like: `#rgb`/`#rrggbb`, a
    /// `rgb(`-shaped function, and a named colour. A `var(--…)` reference is none of them — it names
    /// a colour the *theme* chose, which is the arrangement this is here to protect — so a custom
    /// property name is consumed and discarded wherever it appears, on either side of the colon.
    fn colours_in(css: &str) -> Vec<String> {
        let mut found = Vec::new();
        for (_, value) in declarations_in(css) {
            let chars: Vec<char> = value.chars().collect();
            let mut i = 0;
            while i < chars.len() {
                if chars[i] == '#' {
                    let mut end = i + 1;
                    while end < chars.len() && chars[end].is_ascii_hexdigit() {
                        end += 1;
                    }
                    // 3, 4, 6 and 8 are the legal hex-colour lengths; anything else is not one.
                    if matches!(end - i - 1, 3 | 4 | 6 | 8) {
                        found.push(chars[i..end].iter().collect());
                    }
                    i = end;
                } else if chars[i] == '-' && chars.get(i + 1) == Some(&'-') {
                    i += 2;
                    while i < chars.len() && is_ident(chars[i]) {
                        i += 1;
                    }
                } else if chars[i].is_ascii_alphabetic() {
                    let start = i;
                    while i < chars.len() && is_ident(chars[i]) {
                        i += 1;
                    }
                    let word: String = chars[start..i].iter().collect();
                    let lower = word.to_ascii_lowercase();
                    let mut after = i;
                    while chars.get(after) == Some(&' ') {
                        after += 1;
                    }
                    if chars.get(after) == Some(&'(') {
                        if COLOUR_FUNCTIONS.contains(&lower.as_str()) {
                            found.push(format!("{lower}("));
                        }
                    } else if NAMED_COLOURS.split_whitespace().any(|name| name == lower) {
                        found.push(word);
                    }
                } else {
                    i += 1;
                }
            }
        }
        found
    }

    fn is_ident(c: char) -> bool {
        c.is_ascii_alphanumeric() || c == '-' || c == '_'
    }

    /// The rule itself, in the direction that matters: the stylesheet compiled into the binary must
    /// name no colour, or a theme file cannot fully replace one.
    #[test]
    fn chrome_css_carries_not_one_colour() {
        let css = std::str::from_utf8(super::CHROME_CSS).expect("chrome.css is UTF-8");
        let found = colours_in(css);
        assert!(
            found.is_empty(),
            "chrome/chrome.css writes {} colour(s) that theme.css can never override: {found:?}",
            found.len(),
        );
    }

    /// And the other direction, on the theme bru ships: colours and nothing else. This one is a
    /// *report* rather than a rule bru enforces — `theme.css` is generated by lvim-colorscheme and
    /// belongs to themer — but a generator that started emitting layout would break the split just
    /// as thoroughly from the other side, and bru is where that would be noticed.
    #[test]
    fn theme_css_carries_not_one_rule() {
        let stray: Vec<String> = declarations_in(super::DEFAULT_THEME_CSS)
            .into_iter()
            .filter(|(prop, _)| !prop.starts_with("--"))
            .map(|(prop, value)| format!("{prop}: {value}"))
            .collect();
        assert!(
            stray.is_empty(),
            "chrome/theme.css declares {} thing(s) that are not custom properties: {stray:?}",
            stray.len(),
        );
    }

    /// What the check is for. Each of these is a way a colour has actually got into a stylesheet.
    #[test]
    fn the_check_catches_every_spelling_of_a_colour() {
        assert_eq!(colours_in("a { color: #ff0000; }"), ["#ff0000"]);
        assert_eq!(colours_in("a { color: #F00; }"), ["#F00"]);
        assert_eq!(colours_in("a { background: rgb(90, 97, 88); }"), ["rgb("]);
        assert_eq!(colours_in("a { background: rgba(0,0,0,.5); }"), ["rgba("]);
        assert_eq!(colours_in("a { background: hsl(120 50% 40%); }"), ["hsl("]);
        assert_eq!(colours_in("a { color: rebeccapurple; }"), ["rebeccapurple"]);
        // Case is not a hiding place, and neither is a shorthand: a colour in the third position of
        // `border` is the one a reader scanning for `color:` walks straight past.
        assert_eq!(colours_in("a { color: DarkRed; }"), ["DarkRed"]);
        assert_eq!(
            colours_in("a { border-bottom: 1px solid firebrick; }"),
            ["firebrick"]
        );
    }

    /// And what it must not catch, or it would be turned off within a week.
    #[test]
    fn the_check_passes_what_the_rule_allows() {
        for value in [
            "transparent",
            "currentColor",
            "inherit",
            "var(--statusbar-url-fg)",
            // The trap in the previous line, spelled out: `--red` is a theme name, not a colour
            // written here, and a checker that tokenised it would flag every stylesheet that uses
            // the palette directly.
            "var(--red)",
            "1px solid var(--completion-border)",
        ] {
            assert!(
                colours_in(&format!("a {{ color: {value}; }}")).is_empty(),
                "{value} is allowed and was flagged"
            );
        }
        // A selector is not a declaration and a comment is not either: `#completion` is an id, and
        // chrome.css's own prose quotes Chromium's `#1f1f1f` while explaining a failure signature.
        assert!(colours_in("/* #1f1f1f, and the word red */\n#completion { display: none; }").is_empty());
        // Nor is a hash of the wrong length: `#12345` is no colour.
        assert!(colours_in("a { color: #12345; }").is_empty());
    }
}
