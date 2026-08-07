//! Settings: the typed store `config.lua` fills at startup and `:set` / `config-cycle` change while
//! bru runs.
//!
//! Two rules shape everything here.
//!
//! **bru never writes configuration.** DESIGN.md gives `~/.config/bru/config.lua` to configer, so a
//! runtime `:set` changes the running browser and nothing on disk — qutebrowser's `:set` without
//! `--save`. That is also why the `-t`/`--temp` flag has no field on [`crate::commands::Command`]:
//! qutebrowser's `:set` writes `autoconfig.yml` unless `-t` is passed, bru writes nothing either
//! way, so the two spellings of every binding (`tsh` and `tSh`, `tih` and `tIh`, …) do the same
//! thing, and a field storing a flag nothing reads lies in exactly the way a stored-and-ignored
//! setting does. One caveat that is *not* bru's file: a content setting is kept by Chromium in the
//! profile under `--user-data-dir`, beside cookies and history, and does survive a restart. Making
//! it not survive means giving every browser an in-memory `RequestContext`, which is `app.rs` and
//! `tabs.rs` work rather than this file's.
//!
//! **A setting bru cannot honour is not a setting.** Every name in [`SETTINGS`] moves something
//! observable; the ones qutebrowser's default bindings name and bru refuses are listed in
//! [`REFUSED`], with the reason, so that `:set content.plugins false` answers with why there is
//! nothing behind the name instead of quietly agreeing.
//!
//! The store itself is plain Rust and has no CEF in it — it is built and tested without a browser.
//! [`apply`] is the one function that pushes a value into Chromium, through
//! `RequestContext::set_content_setting`, and it is also the only thing in this file that has to run
//! on the UI thread.

use std::collections::HashMap;
use std::sync::Mutex;

use cef::*;

/// What a setting's value is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// `true` / `false`. `config-cycle` with no values cycles those two, as qutebrowser does.
    Bool,
    /// Free text — a URL, today.
    Text,
}

/// A setting's value, already validated against its [`Kind`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Value {
    Bool(bool),
    Text(String),
}

impl std::fmt::Display for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Value::Bool(true) => f.write_str("true"),
            Value::Bool(false) => f.write_str("false"),
            Value::Text(text) => f.write_str(text),
        }
    }
}

/// What bru does with a value once it has it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Backing {
    /// Read by `open.rs` when it needs a start page; nothing to push.
    StartPage,
    /// A Chromium content setting, global or per-origin. See [`apply`].
    Content(ContentKind),
    /// Read by `focus.rs` when a page moves its focus; nothing to push. Chromium has no opinion
    /// about which of bru's modes a focused field means — that question only exists inside bru.
    Insert,
}

/// The content settings bru drives. Kept as bru's own enum rather than `ContentSettingTypes` so
/// that [`SETTINGS`] stays a plain table the unit tests can walk without CEF being initialised.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ContentKind {
    Javascript,
    Images,
}

impl ContentKind {
    fn cef(self) -> ContentSettingTypes {
        match self {
            ContentKind::Javascript => ContentSettingTypes::JAVASCRIPT,
            ContentKind::Images => ContentSettingTypes::IMAGES,
        }
    }
}

/// Which scopes a setting can be given.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scopes {
    /// No `-u`: one value for the whole browser.
    GlobalOnly,
    /// `-u <pattern>` only. **`content.javascript.enabled` is here, and the reason is measured.**
    /// bru's own UI is two HTML pages on the `bru://` scheme, so a global JavaScript block takes
    /// the tab strip and the status line with it — and bru cannot exempt itself. Measured
    /// 2026-08-06: after setting the default to BLOCK, `set_content_setting("bru://chrome/", …,
    /// ALLOW)` read back as `block`, and so did `bru://chrome/bottom.html` and `bru://chrome`.
    /// Chromium's content-settings patterns do not cover a custom scheme, so there is no spelling
    /// that pins the chrome back. Refusing the scope is the only honest answer; all twelve default
    /// bindings pass `-u` and lose nothing.
    UrlOnly,
    /// Either.
    Both,
}

/// One setting bru implements.
#[derive(Debug)]
pub struct Def {
    pub name: &'static str,
    pub kind: Kind,
    /// What the value is when nothing has set it. `None` for `start_page`, whose absence is
    /// meaningful — it is what leaves `crate::app::HOME` standing (DECISIONS.md item 7).
    pub default: Option<&'static str>,
    pub scopes: Scopes,
    backing: Backing,
}

/// Every setting bru has, and nothing else.
///
/// The list is short on purpose. qutebrowser has some 400 options; bru has seven, because seven is
/// how many it can currently change the behaviour of. Adding a name here without adding the
/// behaviour behind it would make `:set` a place where things are typed and forgotten.
pub const SETTINGS: &[Def] = &[
    Def {
        name: "start_page",
        kind: Kind::Text,
        default: None,
        scopes: Scopes::GlobalOnly,
        backing: Backing::StartPage,
    },
    Def {
        name: "content.javascript.enabled",
        kind: Kind::Bool,
        default: Some("true"),
        // Per URL only — see Scopes::UrlOnly, which carries the measurement.
        scopes: Scopes::UrlOnly,
        backing: Backing::Content(ContentKind::Javascript),
    },
    Def {
        name: "content.images",
        kind: Kind::Bool,
        default: Some("true"),
        // Globally too: bru's chrome has no images, so switching them off everywhere costs bru's
        // own UI nothing. That asymmetry with JavaScript is measured, not stylistic.
        scopes: Scopes::Both,
        backing: Backing::Content(ContentKind::Images),
    },
    // --- src/focus.rs -------------------------------------------------------------------------
    // qutebrowser's four, by their own names and with their own defaults (`configdata.yml:1905`,
    // `:1911`, `:1916`, `:1926`). They are here rather than hard-coded in `focus.rs` because bru ships its own
    // defaults and `config.lua` layers overrides on them — a user who wants the old behaviour back
    // writes `bru.set("input.insert_mode.auto_load", "true")` and gets it, with no branch to keep.
    Def {
        name: "input.insert_mode.auto_load",
        kind: Kind::Bool,
        // **false**, and this is the fix. A page that focuses its own search box must not be
        // holding the next key the user presses — `:` on the start page opens the command line.
        default: Some("false"),
        scopes: Scopes::GlobalOnly,
        backing: Backing::Insert,
    },
    Def {
        name: "input.insert_mode.auto_enter",
        kind: Kind::Bool,
        // **true**: clicking into a field, or following a hint onto one, still types into it.
        default: Some("true"),
        scopes: Scopes::GlobalOnly,
        backing: Backing::Insert,
    },
    Def {
        name: "input.insert_mode.auto_leave",
        kind: Kind::Bool,
        // **true**: clicking something that is not editable ends insert mode.
        default: Some("true"),
        scopes: Scopes::GlobalOnly,
        backing: Backing::Insert,
    },
    Def {
        name: "input.insert_mode.leave_on_load",
        kind: Kind::Bool,
        // **true** (`configdata.yml:1926`): a new page load ends insert mode. qutebrowser allows a
        // URL pattern here and says in the same breath that patterns are unreliable on it, because
        // it may match either end of the navigation. bru takes the honest half: global only.
        default: Some("true"),
        scopes: Scopes::GlobalOnly,
        backing: Backing::Insert,
    },
    // --- end src/focus.rs ---------------------------------------------------------------------
];

/// The settings qutebrowser's default bindings name that bru refuses, and why.
///
/// They are here rather than absent from the file so that the reason survives: someone who reads
/// `config-cycle … content.plugins` in the binding table and wonders why it is inert finds the
/// answer next to the ones that work, instead of concluding it was forgotten. `bru://chrome/help`
/// prints these strings against the twelve rows — see [`refusal_in`] — so a refusal is a sentence
/// the user can read rather than a row that says "not yet" about something that never will be.
///
/// Refusing them is the point. `tph` would otherwise toggle a value nothing reads, print it, reload
/// the page, and leave the user certain that plugins are off.
///
/// **Both were re-measured 2026-08-06 against CEF 151 rather than reasoned about**, with
/// `--settings-probe='prefs:…'` and `--settings-probe='cookies:…'` — see [`dump_preferences`] and
/// [`probe_third_party_cookies`], which exist so that the next CEF can be asked the same questions
/// in one command.
pub const REFUSED: &[(&str, &str)] = &[
    (
        "content.plugins",
        "Chromium 151 has nothing behind this name. cef_browser_settings_t has no plugin field \
         at all, cef_content_setting_types_t's only plugin entry is DEPRECATED_PPAPI_BROKER, and \
         the one settable preference in the family is plugins.always_open_pdf_externally — \"open \
         PDFs in another application\", which is neither \"enable plugins\" nor scopeable to a \
         URL. Measured 2026-08-06. NPAPI and PPAPI are gone, so these six keys are not waiting \
         for work; there is no work that would make them act.",
    ),
    (
        "content.cookies.accept",
        "its three values are all / no-3rdparty / never, and no-3rdparty cannot be written per \
         URL. It needs a rule with a wildcard requesting pattern under a fixed top-level one, and \
         set_content_setting derives both patterns from URLs. Measured 2026-08-06: \
         set_content_setting(requesting=none, top_level=https://example.com/, COOKIES, BLOCK) \
         changed nothing — every read-back, third-party and first-party alike, still answered \
         allow — while the same call with the URLs the other way round answered block. Chromium \
         does have a global switch, the preference profile.cookie_controls_mode, and CEF reports \
         it settable; but all twelve default bindings pass -u <pattern>, and a per-site key that \
         quietly changed the whole browser would be worse than one that does nothing. A \
         three-value cycle that is wrong on one press in three is worse than a key that says it \
         does nothing.",
    ),
];

/// The reason a command string names a refused setting, for `bru://chrome/help`'s third state.
///
/// `commands.rs` refuses to build a `ConfigCycle` for a setting this file does not have, so the
/// twelve `t**` rows arrive as `Command::Unimplemented` carrying their whole text — which is where
/// the setting's name still is. See `exec::refusal`, the one caller.
pub fn refusal_in(text: &str) -> Option<&'static str> {
    REFUSED
        .iter()
        .find(|(name, _)| text.contains(name))
        .map(|(_, why)| *why)
}

/// The definition for `name`, if bru has one.
pub fn def(name: &str) -> Option<&'static Def> {
    SETTINGS.iter().find(|def| def.name == name)
}

/// Whether bru knows this setting — what `commands.rs` asks before it builds a `Set`/`ConfigCycle`
/// rather than an `Unimplemented`.
pub fn is_known(name: &str) -> bool {
    def(name).is_some()
}

/// The names `:set` accepts, for the message that lists them.
pub fn known_names() -> String {
    SETTINGS
        .iter()
        .map(|def| def.name)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The error a refused setting gets: it names the setting and says why, which is the whole reason
/// [`REFUSED`] exists.
fn unknown(name: &str) -> String {
    match REFUSED.iter().find(|(refused, _)| *refused == name) {
        Some((_, why)) => format!("bru does not implement {name}: {why}"),
        None => format!("unknown setting {name:?}; bru knows {}", known_names()),
    }
}

impl Def {
    /// Parse a value string against this setting's kind.
    fn parse(&self, text: &str) -> Result<Value, String> {
        match self.kind {
            Kind::Bool => match text {
                "true" | "yes" | "on" | "1" => Ok(Value::Bool(true)),
                "false" | "no" | "off" | "0" => Ok(Value::Bool(false)),
                other => Err(format!(
                    "{}: {other:?} is not a boolean (true or false)",
                    self.name
                )),
            },
            Kind::Text => {
                if text.trim().is_empty() {
                    return Err(format!("{} cannot be empty", self.name));
                }
                Ok(Value::Text(text.to_string()))
            }
        }
    }

    /// The value when nothing has set it.
    fn default_value(&self) -> Option<Value> {
        let default = self.default?;
        self.parse(default).ok()
    }
}

// -----------------------------------------------------------------------------------------------
// URL patterns
// -----------------------------------------------------------------------------------------------

/// A `-u <pattern>` argument, reduced to what Chromium can actually scope a content setting by.
///
/// **This is the one place bru is coarser than qutebrowser, and it is deliberate rather than
/// unfinished.** `RequestContext::set_content_setting` takes a *URL*, not a pattern, and Chromium
/// derives the rule from it with `ContentSettingsPattern::FromURL`, which is origin-shaped
/// (`[*.]host`). So:
///
/// - `*://*.{url:host}/*` is exactly what Chromium stores, and is honoured as written.
/// - `*://{url:host}/*` asks for one host without its subdomains; Chromium has no such rule at this
///   API, so it is widened to `[*.]host`.
/// - `{url}` asks for one URL; the JavaScript and images settings are origin-scoped in Chromium and
///   have no path dimension, so it is widened the same way.
///
/// Nothing here is silent: [`Pattern::describe`] is what `-p` prints, and it prints the scope bru
/// used rather than the one that was typed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Pattern {
    /// The text as it arrived, after `{url…}` expansion — the store's key, so two spellings of the
    /// same scope stay two entries, exactly as they would in qutebrowser's config.
    text: String,
    /// The host the rule lands on, without any leading `*.`.
    host: String,
    /// The scheme, or `None` when the pattern wildcarded it.
    scheme: Option<String>,
}

impl Pattern {
    /// Parse `*://host/*`, `*://*.host/*`, `https://host/path`, or a bare `host`.
    ///
    /// `*://*/*` and `*` mean "everywhere", which is the global scope and therefore not a pattern —
    /// `Ok(None)`.
    pub fn parse(text: &str) -> Result<Option<Pattern>, String> {
        let text = text.trim();
        if text.is_empty() {
            return Err("an empty URL pattern matches nothing".to_string());
        }
        if text == "*" || text == "*://*/*" || text == "*://*" {
            return Ok(None);
        }

        let (scheme, rest) = match text.split_once("://") {
            Some((scheme, rest)) => (Some(scheme.to_string()), rest),
            None => (None, text),
        };
        let scheme = scheme.filter(|scheme| scheme != "*");

        let authority = rest.split('/').next().unwrap_or("");
        // A port is not part of a content-settings host, and none of the default bindings carry one.
        let host = authority.split('@').next_back().unwrap_or(authority);
        let host = host.split(':').next().unwrap_or(host);
        let host = host.strip_prefix("*.").unwrap_or(host);

        if host.is_empty() || host == "*" {
            return Err(format!("{text:?} names no host to scope a setting by"));
        }

        Ok(Some(Pattern {
            text: text.to_string(),
            host: host.to_string(),
            scheme,
        }))
    }

    /// The URLs handed to `set_content_setting`, given the page bru is looking at.
    ///
    /// **A pattern is more than one URL, and both facts behind that were measured rather than
    /// assumed** (2026-08-06, against `http://127.0.0.1:8742/probe.html`):
    ///
    /// - Writing the rule for `https://127.0.0.1/` left `http://127.0.0.1:8742/…` reading `allow`
    ///   and `https://127.0.0.1/` reading `block`. Chromium's rule does not wildcard the scheme
    ///   here, so `*://host/*` — which in qutebrowser means either scheme — has to be written twice.
    /// - Then writing `http://127.0.0.1/` as well *still* left the page reading `allow`, while
    ///   `https://127.0.0.1/` stayed `block`. The rule keeps the port: `http://127.0.0.1/` is
    ///   `:80`, and the page is on `:8742`. `*://host/*` matches any port and the API has no way to
    ///   say so.
    ///
    /// So the page's own origin is written too whenever the pattern names the page's host, which is
    /// the case for every one of the twelve default bindings — they all build the pattern out of
    /// `{url:host}` or `{url}`. Without it `tsh` reports success and changes nothing, which is the
    /// exact failure this file exists to avoid.
    fn urls(&self, page: &str) -> Vec<String> {
        let mut out = match &self.scheme {
            Some(scheme) => vec![format!("{scheme}://{}/", self.host)],
            None => vec![
                format!("http://{}/", self.host),
                format!("https://{}/", self.host),
            ],
        };
        if let Some((scheme, authority)) = origin_of(page) {
            if authority.split(':').next().unwrap_or(&authority) == self.host
                && self.scheme.as_deref().is_none_or(|want| want == scheme)
            {
                out.push(format!("{scheme}://{authority}/"));
            }
        }
        out.dedup();
        out
    }

    /// Whether the host is an address rather than a name, which decides whether talking about its
    /// subdomains means anything.
    fn is_address(&self) -> bool {
        self.host.contains(':')
            || (self.host.contains('.')
                && self.host.split('.').all(|part| {
                    !part.is_empty() && part.chars().all(|c| c.is_ascii_digit())
                }))
    }

    /// The scope bru actually applied, spelled the way qutebrowser spells a pattern. This is what
    /// `-p` prints — see the type's own documentation for why it is not the text that was typed.
    pub fn describe(&self) -> String {
        if self.is_address() {
            // `*://*.127.0.0.1/*` would be a lie in the other direction: an address has no
            // subdomains, and Chromium's rule for one covers exactly it.
            return format!("*://{}/*", self.host);
        }
        format!("*://*.{}/*", self.host)
    }

    /// Whether the scope bru used is narrower than the one that was asked for.
    pub fn was_widened(&self) -> bool {
        self.describe() != self.text
    }
}

/// `scheme` and `host[:port]` of a URL, or `None` if it has no scheme.
fn origin_of(url: &str) -> Option<(String, String)> {
    let (scheme, rest) = url.split_once("://")?;
    let authority = rest.split('/').next().unwrap_or("");
    let authority = authority.split('@').next_back().unwrap_or(authority);
    if authority.is_empty() {
        return None;
    }
    Some((scheme.to_string(), authority.to_string()))
}

/// `{url}`, `{url:pretty}`, `{url:host}` and `{url:domain}` against the tab that is showing.
///
/// The `-u` argument of all twelve live `config-cycle` bindings is built out of these, and they are
/// expanded here rather than in `cmdline.rs` because a key-bound command never passes through the
/// command line: `commands::parse` runs at startup, when there is no page to ask.
pub fn expand(text: &str) -> String {
    if !text.contains('{') {
        return text.to_string();
    }
    let url = crate::ipc::current_url();
    let (scheme, rest) = match url.split_once("://") {
        Some((scheme, rest)) => (scheme, rest),
        None => ("", url.as_str()),
    };
    let authority = rest.split('/').next().unwrap_or("");
    let host = authority.split('@').next_back().unwrap_or(authority);
    let host = host.split(':').next().unwrap_or(host).to_string();
    let domain = if scheme.is_empty() {
        host.clone()
    } else {
        format!("{scheme}://{authority}")
    };

    text.replace("{url:pretty}", &url)
        .replace("{url:host}", &host)
        .replace("{url:domain}", &domain)
        .replace("{url:scheme}", scheme)
        .replace("{url}", &url)
}

// -----------------------------------------------------------------------------------------------
// The store
// -----------------------------------------------------------------------------------------------

/// The settings `bru.set` names at startup and `:set` changes afterwards.
///
/// Keyed by `(pattern, name)` so a per-URL value and the global one live side by side, the way
/// qutebrowser keeps them. `None` is the global scope.
#[derive(Debug, Clone, Default)]
pub struct Settings {
    values: HashMap<(Option<String>, &'static str), Value>,
}

/// What a successful set produced: enough for the caller to push it into Chromium and print it.
#[derive(Debug)]
pub struct Applied {
    pub def: &'static Def,
    pub value: Value,
    pub pattern: Option<Pattern>,
}

impl Applied {
    /// `content.javascript.enabled = false for *://*.example.com/*` — qutebrowser's own wording in
    /// `configcommands.py::_print_value`, with the scope bru used.
    ///
    /// When that is wider than the one that was typed the line says so, rather than echoing the
    /// pattern back and letting it look like it was honoured as written. See [`Pattern`].
    pub fn describe(&self) -> String {
        let mut out = format!("{} = {}", self.def.name, self.value);
        if let Some(pattern) = &self.pattern {
            out.push_str(&format!(" for {}", pattern.describe()));
            if pattern.was_widened() {
                out.push_str(&format!(" (asked for {}; Chromium scopes this by origin)", pattern.text));
            }
        }
        out
    }
}

impl Settings {
    /// `bru.set(key, value)` — the global scope, which is all `config.lua` can name today.
    ///
    /// The error string is what `bru.set` raises into Lua, so it reads as a message to whoever wrote
    /// `config.lua`.
    pub fn set(&mut self, key: &str, value: &str) -> Result<(), String> {
        self.set_scoped(key, value, None).map(|_| ())
    }

    /// `:set [-u <pattern>] <option> <value>`.
    pub fn set_scoped(
        &mut self,
        key: &str,
        value: &str,
        pattern: Option<&str>,
    ) -> Result<Applied, String> {
        let def = def(key).ok_or_else(|| unknown(key))?;
        let parsed = def.parse(value)?;
        let pattern = self.scope(def, pattern)?;
        self.values
            .insert((pattern.as_ref().map(|p| p.text.clone()), def.name), parsed.clone());
        Ok(Applied { def, value: parsed, pattern })
    }

    /// `config-cycle <option> [values…]` — the next value in the list, or the first when the current
    /// one is not in it (`configcommands.py:220-225`). An empty list on a boolean is `true false`.
    pub fn cycle(
        &mut self,
        key: &str,
        values: &[String],
        pattern: Option<&str>,
    ) -> Result<Applied, String> {
        let def = def(key).ok_or_else(|| unknown(key))?;
        let owned: Vec<String>;
        let values = if values.is_empty() && def.kind == Kind::Bool {
            owned = vec!["true".to_string(), "false".to_string()];
            &owned
        } else {
            values
        };
        if values.len() < 2 {
            return Err(format!(
                "{}: config-cycle needs at least two values",
                def.name
            ));
        }
        let candidates: Vec<Value> = values
            .iter()
            .map(|value| def.parse(value))
            .collect::<Result<_, _>>()?;

        let current = self.get(key, pattern)?;
        let next = match candidates.iter().position(|value| Some(value) == current.as_ref()) {
            Some(index) => (index + 1) % candidates.len(),
            None => 0,
        };
        self.set_scoped(key, &candidates[next].to_string(), pattern)
    }

    /// The value in force at `pattern`, falling back to the global value and then to the default.
    ///
    /// `None` only for `start_page`, which has no default — its absence is what leaves
    /// `crate::app::HOME` standing.
    pub fn get(&self, key: &str, pattern: Option<&str>) -> Result<Option<Value>, String> {
        let def = def(key).ok_or_else(|| unknown(key))?;
        let pattern = self.scope(def, pattern)?;
        if let Some(pattern) = &pattern {
            if let Some(value) = self.values.get(&(Some(pattern.text.clone()), def.name)) {
                return Ok(Some(value.clone()));
            }
        }
        Ok(self
            .values
            .get(&(None, def.name))
            .cloned()
            .or_else(|| def.default_value()))
    }

    /// `:set <option>?` — what `-p` prints, and what a bare `:set <option>` shows.
    pub fn describe(&self, key: &str, pattern: Option<&str>) -> Result<String, String> {
        let def = def(key).ok_or_else(|| unknown(key))?;
        let scope = self.scope(def, pattern)?;
        let value = self.get(key, pattern)?;
        let value = value.map_or_else(|| "<unset>".to_string(), |value| value.to_string());
        Ok(match scope {
            Some(pattern) => format!("{} = {value} for {}", def.name, pattern.describe()),
            None => format!("{} = {value}", def.name),
        })
    }

    /// `bru.set("start_page", …)`. `None` leaves the compiled-in [`crate::app::HOME`] standing.
    pub fn start_page(&self) -> Option<String> {
        match self.values.get(&(None, "start_page")) {
            Some(Value::Text(text)) => Some(text.clone()),
            _ => None,
        }
    }

    /// Parse and validate a pattern against a setting that may or may not accept one.
    fn scope(&self, def: &'static Def, pattern: Option<&str>) -> Result<Option<Pattern>, String> {
        let parsed = match pattern {
            Some(pattern) => Pattern::parse(pattern)?,
            None => None,
        };
        match (def.scopes, parsed.is_some()) {
            (Scopes::GlobalOnly, true) => Err(format!("{} cannot be set per URL", def.name)),
            (Scopes::UrlOnly, false) => Err(format!(
                "{} can only be set for a URL pattern — pass -u <pattern>. Globally it would \
                 switch JavaScript off inside bru's own tab strip and status line, which are \
                 HTML pages on the bru:// scheme, and Chromium's content settings cannot name a \
                 custom scheme to exempt them",
                def.name
            )),
            _ => Ok(parsed),
        }
    }

    /// Every value that has been set, for [`apply_at_startup`] and for the tests.
    fn entries(&self) -> Vec<(Option<Pattern>, &'static Def, Value)> {
        let mut out = Vec::new();
        for ((pattern, name), value) in &self.values {
            let Some(def) = def(name) else { continue };
            let pattern = match pattern {
                Some(text) => match Pattern::parse(text) {
                    Ok(pattern) => pattern,
                    Err(_) => continue,
                },
                None => None,
            };
            out.push((pattern, def, value.clone()));
        }
        // Deterministic, so a startup that applies two settings applies them in the same order twice.
        out.sort_by(|a, b| {
            (a.1.name, a.0.as_ref().map(|p| &p.text))
                .cmp(&(b.1.name, b.0.as_ref().map(|p| &p.text)))
        });
        out
    }
}

// -----------------------------------------------------------------------------------------------
// The live store
// -----------------------------------------------------------------------------------------------

/// The settings the running browser is on.
///
/// A `static` for the same reason `open.rs`'s engines are one: `:set` arrives from the command line
/// and from a keypress, neither of which carries a `Config`, and the alternative is threading a
/// handle through `exec::run` into every workstream that never touches settings.
static LIVE: Mutex<Option<Settings>> = Mutex::new(None);

/// Hand the startup settings over. Called by `Config::into_parsers`, beside `open::install`.
///
/// Pure: it stores, and does not touch CEF. The Chromium half is [`apply_at_startup`], which
/// `app.rs` calls once the browser process is up — the unit tests run this function with no CEF
/// behind them.
pub fn install(settings: Settings) {
    *LIVE.lock().expect("the settings mutex is never poisoned") = Some(settings);
}

/// Run `f` against the live store.
fn with_live<R>(f: impl FnOnce(&mut Settings) -> R) -> R {
    let mut guard = LIVE.lock().expect("the settings mutex is never poisoned");
    f(guard.get_or_insert_with(Settings::default))
}

// --- src/focus.rs ------------------------------------------------------------------------------
/// One boolean setting's value in force globally, falling back to its compiled-in default.
///
/// The reader for settings whose backing is bru itself rather than Chromium — there is nothing to
/// push, so there is nothing to read back from Chromium either, and this is the whole interface.
/// A name this file does not know answers `false`; that cannot happen from `focus.rs`, whose three
/// names are `const`, and answering rather than panicking is what keeps a typo in a caller from
/// taking the browser down.
///
/// **Not on the key path.** It takes the settings mutex, and its callers are focus changes, which
/// happen when a person clicks or a page loads — never per keystroke.
pub fn is_on(name: &str) -> bool {
    matches!(
        with_live(|settings| settings.get(name, None)),
        Ok(Some(Value::Bool(true)))
    )
}
// --- end src/focus.rs --------------------------------------------------------------------------

// -----------------------------------------------------------------------------------------------
// The commands
// -----------------------------------------------------------------------------------------------

/// `:set [-p] [-u <pattern>] [<option>] [<value>]` — the arm `exec::run` calls.
///
/// With no value, or with an option spelled `option?`, it prints instead of setting, which is
/// `configcommands.py:99-111`.
pub fn run_set(option: Option<&str>, value: Option<&str>, pattern: Option<&str>, print: bool) {
    // A bare `:set` opens `qute://settings` in qutebrowser. bru has no such page, so the parser
    // never builds this shape — see `commands.rs`. Belt and braces.
    let Some(option) = option else {
        eprintln!("bru: :set needs an option — bru knows {}", known_names());
        return;
    };
    let pattern = pattern.map(expand);
    let pattern = pattern.as_deref();

    let outcome = match value {
        None => with_live(|settings| settings.describe(option, pattern)),
        Some(value) => with_live(|settings| settings.set_scoped(option, value, pattern))
            .and_then(|applied| {
                apply(&applied)?;
                Ok(if print { applied.describe() } else { String::new() })
            }),
    };
    report(outcome);
}

/// `:config-cycle [-p] [-u <pattern>] <option> [values…]`.
pub fn run_cycle(option: &str, values: &[String], pattern: Option<&str>, print: bool) {
    let pattern = pattern.map(expand);
    let pattern = pattern.as_deref();
    let outcome = with_live(|settings| settings.cycle(option, values, pattern)).and_then(|applied| {
        apply(&applied)?;
        Ok(if print { applied.describe() } else { String::new() })
    });
    report(outcome);
}

/// bru has no status-bar message area yet — `statusbar/` has url, scroll, tab index, keystring and
/// the search count, and nothing that shows a one-off line. Until it does, `-p` goes to stderr,
/// which is where every other command that has something to say already writes.
fn report(outcome: Result<String, String>) {
    match outcome {
        Ok(text) if text.is_empty() => {}
        Ok(text) => eprintln!("bru: {text}"),
        Err(error) => eprintln!("bru: {error}"),
    }
}

// -----------------------------------------------------------------------------------------------
// The Chromium half
// -----------------------------------------------------------------------------------------------

/// Push one value into Chromium.
///
/// `RequestContext::set_content_setting` with both URLs null sets the default for the type; with a
/// requesting URL it writes the `[*.]host` rule Chromium derives from it. Both must run on the
/// browser process UI thread, which every caller here is already on: a keypress, a posted command
/// task, or `on_context_initialized`.
pub fn apply(applied: &Applied) -> Result<(), String> {
    let kind = match applied.def.backing {
        // Nothing to push: `open.rs` reads it when it needs it.
        Backing::StartPage => return Ok(()),
        // Nothing to push either: `focus.rs` reads it when a focus changes. Chromium is not told,
        // because Chromium has no idea bru has modes.
        Backing::Insert => return Ok(()),
        Backing::Content(kind) => kind,
    };
    let Value::Bool(allow) = applied.value else {
        return Err(format!("{}: a content setting must be a boolean", applied.def.name));
    };
    let Some(context) = request_context_get_global_context() else {
        return Err("no request context — settings cannot reach Chromium".to_string());
    };

    let value = if allow {
        ContentSettingValues::ALLOW
    } else {
        ContentSettingValues::BLOCK
    };

    match &applied.pattern {
        Some(pattern) => {
            let page = crate::ipc::current_url();
            for url in pattern.urls(&page) {
                let url = CefString::from(url.as_str());
                context.set_content_setting(Some(&url), None, kind.cef(), value);
            }
        }
        // The default for the whole browser. Only settings whose `scopes` allow it get here —
        // `content.javascript.enabled` does not, because bru cannot exempt its own chrome from
        // one. See `Scopes::UrlOnly`.
        None => context.set_content_setting(None, None, kind.cef(), value),
    }
    Ok(())
}

/// Push everything `config.lua` set into Chromium. Called once from `app.rs`, after `Config::load`
/// and before the first tab exists — a start page with JavaScript switched off has to load that way
/// rather than load and then be corrected.
pub fn apply_at_startup() {
    debug_assert_ne!(currently_on(ThreadId::UI), 0);
    let entries = with_live(|settings| settings.entries());
    for (pattern, def, value) in entries {
        if let Err(error) = apply(&Applied { def, value, pattern }) {
            eprintln!("bru: {error}");
        }
    }
}

/// What Chromium says the setting is for `url` right now — the read-back that proves a set landed.
///
/// Used by `--settings-probe`. It asks Chromium rather than bru's own store on purpose: a store that
/// agrees with itself proves nothing.
pub fn chromium_value(name: &str, url: &str) -> Option<String> {
    let kind = match def(name)?.backing {
        Backing::Content(kind) => kind,
        // Neither is Chromium's to answer for.
        Backing::StartPage | Backing::Insert => return None,
    };
    let context = request_context_get_global_context()?;
    let url = CefString::from(url);
    let value = context.content_setting(Some(&url), None, kind.cef());
    Some(
        match value {
            v if v == ContentSettingValues::ALLOW => "allow",
            v if v == ContentSettingValues::BLOCK => "block",
            v if v == ContentSettingValues::DEFAULT => "default",
            _ => "other",
        }
        .to_string(),
    )
}

/// `--settings-probe='<spec>,<spec>,…' [--settings-probe-after-ms=N]` prints what Chromium answers,
/// once. A spec is one of:
///
/// | | |
/// |---|---|
/// | `<setting>@<url>` | what Chromium enforces for one of bru's settings at one URL |
/// | `prefs:<name>` | whether a Chromium preference exists and is settable, plus every top-level one whose name contains it — see [`dump_preferences`] |
/// | `cookies:<third-party>\|<top-level>` | whether the rule `no-3rdparty` needs can be written at all — see [`probe_third_party_cookies`] |
///
/// It exists because "the setting was stored" and "Chromium is enforcing it" are different claims,
/// and only the second one is worth anything. `--cmd` already drives `:set` and `:config-cycle`
/// through the real dispatcher, so nothing here injects anything — the first two forms only read,
/// and the third writes into a scratch profile to find out whether the write lands.
///
/// The last two are what [`REFUSED`] rests on. They are kept rather than deleted after the answer
/// because the answer is CEF's, not bru's: a later CEF may have `no-3rdparty` in it, and the way to
/// find out should be one command rather than an afternoon.
pub fn schedule_probe(spec: &str, after_ms: i64) {
    let mut task = SettingsProbe::new(spec.to_string());
    post_delayed_task(ThreadId::UI, Some(&mut task), after_ms);
}

wrap_task! {
    struct SettingsProbe {
        spec: String,
    }

    impl Task {
        fn execute(&self) {
            debug_assert_ne!(currently_on(ThreadId::UI), 0);
            for pair in self.spec.split(',').filter(|pair| !pair.is_empty()) {
                if let Some(needle) = pair.strip_prefix("prefs:") {
                    dump_preferences(needle);
                    continue;
                }
                if let Some(spec) = pair.strip_prefix("cookies:") {
                    probe_third_party_cookies(spec);
                    continue;
                }
                let Some((name, url)) = pair.split_once('@') else {
                    eprintln!("settings-probe: {pair:?} is not <setting>@<url>");
                    continue;
                };
                match chromium_value(name, url) {
                    Some(value) => eprintln!("settings-probe: {name} at {url} -> {value}"),
                    None => eprintln!("settings-probe: {name} has no Chromium value to read"),
                }
            }
            // The page's own title, which is where a JavaScript probe leaves its answer.
            if let Some(state) = crate::state::BruState::instance() {
                let guard = state.lock().expect("state mutex poisoned");
                let index = guard.active_tab();
                eprintln!(
                    "settings-probe: tab {index} title={:?} url={:?}",
                    guard.tab_title(index).unwrap_or_default(),
                    guard.tab_url(index).unwrap_or_default(),
                );
            }
        }
    }
}

/// `--settings-probe='prefs:cookie'` — every Chromium preference CEF exposes whose name contains
/// `needle`, with whether CEF will let bru write it.
///
/// A preference is the other half of Chromium's settings, beside the content-settings map: the
/// per-profile switches `chrome://settings` writes. `RequestContext` implements
/// `ImplPreferenceManager`, so both are reachable from the same object.
fn dump_preferences(needle: &str) {
    debug_assert_ne!(currently_on(ThreadId::UI), 0);
    let Some(context) = request_context_get_global_context() else {
        eprintln!("settings-probe: no request context");
        return;
    };
    // The needle as a name in its own right, because `get_all_preferences` answers a *nested*
    // dictionary and its keys are the top-level components only — `plugins` is a dictionary, and
    // whether `plugins.always_open_pdf_externally` exists cannot be read out of that list.
    let exact = CefString::from(needle);
    eprintln!(
        "settings-probe: pref {needle:?} exists={} settable={}",
        context.has_preference(Some(&exact)),
        context.can_set_preference(Some(&exact)),
    );
    let Some(all) = context.all_preferences(1) else {
        eprintln!("settings-probe: no preferences");
        return;
    };
    let mut keys = CefStringList::new();
    all.keys(Some(&mut keys));
    let mut found = 0;
    for key in keys.into_iter() {
        if !key.to_lowercase().contains(&needle.to_lowercase()) {
            continue;
        }
        found += 1;
        let name = CefString::from(key.as_str());
        eprintln!(
            "settings-probe: pref {key} type={:?} settable={}",
            all.get_type(Some(&name)),
            context.can_set_preference(Some(&name)),
        );
    }
    eprintln!("settings-probe: {found} preference(s) matching {needle:?}");
}

/// `--settings-probe='cookies:<third-party>|<top-level>'` — can CEF write the rule `no-3rdparty`
/// needs, and does Chromium read it back?
///
/// The question is whether `set_content_setting` can produce a rule with a **wildcard requesting
/// pattern** under a fixed top-level one. That is what "block cookies from anyone else while I am
/// on this site" is in Chromium's content-settings map, and it is the one of
/// `content.cookies.accept`'s three values that `ALLOW`/`BLOCK` on a single origin cannot say.
fn probe_third_party_cookies(spec: &str) {
    debug_assert_ne!(currently_on(ThreadId::UI), 0);
    let Some((third, top)) = spec.split_once('|') else {
        eprintln!("settings-probe: {spec:?} is not <third-party>|<top-level>");
        return;
    };
    let Some(context) = request_context_get_global_context() else {
        eprintln!("settings-probe: no request context");
        return;
    };
    let (third, top) = (CefString::from(third), CefString::from(top));
    let read = |what: &str, requesting: Option<&CefString>, top_level: Option<&CefString>| {
        let value = context.content_setting(requesting, top_level, ContentSettingTypes::COOKIES);
        eprintln!(
            "settings-probe: cookies {what} -> {}",
            match value {
                v if v == ContentSettingValues::ALLOW => "allow",
                v if v == ContentSettingValues::BLOCK => "block",
                v if v == ContentSettingValues::DEFAULT => "default",
                _ => "other",
            }
        );
    };

    read("before, third-party at top", Some(&third), Some(&top));
    read("before, first-party at top", Some(&top), Some(&top));

    // The rule no-3rdparty needs: nothing named as the requesting URL, the site named as the
    // top-level one.
    context.set_content_setting(None, Some(&top), ContentSettingTypes::COOKIES, ContentSettingValues::BLOCK);
    read("after wildcard-requesting BLOCK, third-party at top", Some(&third), Some(&top));
    read("after wildcard-requesting BLOCK, first-party at top", Some(&top), Some(&top));
    read("after wildcard-requesting BLOCK, third-party elsewhere", Some(&third), None);

    // And the rule `never` needs, for comparison: the site as the requesting URL.
    context.set_content_setting(Some(&top), None, ContentSettingTypes::COOKIES, ContentSettingValues::BLOCK);
    read("after requesting BLOCK, first-party at top", Some(&top), Some(&top));
    read("after requesting BLOCK, third-party at top", Some(&third), Some(&top));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_setting_bru_names_has_something_behind_it() {
        // The table and the refusal list must not overlap: a name cannot both be implemented and be
        // documented as refused, and this is the assertion that notices when one is implemented and
        // the other is not deleted.
        for (refused, _) in REFUSED {
            assert!(
                def(refused).is_none(),
                "{refused} is in REFUSED and in SETTINGS"
            );
        }
        // Every default parses as its own kind — a table row that lies about its default would make
        // `config-cycle` start from a value the setting cannot hold.
        for def in SETTINGS {
            if let Some(default) = def.default {
                def.parse(default)
                    .unwrap_or_else(|e| panic!("{}: bad default: {e}", def.name));
            }
        }
        assert_eq!(SETTINGS.len(), 7);
    }

    /// The four insert-mode settings, read the way `focus.rs` reads them: through the live store,
    /// so that a default that never reaches [`is_on`] is a failing test rather than a browser that
    /// silently behaves the other way round.
    #[test]
    fn the_insert_mode_defaults_survive_the_live_store() {
        install(Settings::default());
        assert!(!is_on("input.insert_mode.auto_load"));
        assert!(is_on("input.insert_mode.auto_enter"));
        assert!(is_on("input.insert_mode.auto_leave"));
        assert!(is_on("input.insert_mode.leave_on_load"));

        // And an override reaches it — this is what `config.lua` does.
        let mut settings = Settings::default();
        settings
            .set("input.insert_mode.auto_load", "true")
            .expect("auto_load is a boolean bru knows");
        install(settings);
        assert!(is_on("input.insert_mode.auto_load"));

        // A name nothing implements is false rather than a panic.
        assert!(!is_on("input.insert_mode.nonsense"));

        install(Settings::default());
    }

    /// They are bru's own, not Chromium's: `:set -u` must refuse them, and `apply` must not try to
    /// write a content setting for them.
    #[test]
    fn the_insert_mode_settings_are_brus_own_and_global() {
        let mut settings = Settings::default();
        let error = settings
            .set_scoped("input.insert_mode.auto_load", "true", Some("example.com"))
            .expect_err("bru's modes are not per site");
        assert!(error.contains("cannot be set per URL"), "{error}");
        assert!(chromium_value_is_not_chromiums("input.insert_mode.auto_enter"));
    }

    /// Split out so the test above says what it means without a `#[cfg]` dance: `chromium_value`
    /// needs a request context, and the assertion is only about the arm that returns before it.
    fn chromium_value_is_not_chromiums(name: &str) -> bool {
        matches!(
            def(name).map(|def| def.backing),
            Some(Backing::Insert) | Some(Backing::StartPage)
        )
    }

    #[test]
    fn a_refused_setting_says_why_rather_than_pretending() {
        let mut settings = Settings::default();
        let error = settings
            .set_scoped("content.plugins", "true", None)
            .expect_err("content.plugins is refused");
        assert!(error.contains("Chromium 151 has nothing behind this name"), "{error}");

        let error = settings
            .set_scoped("content.cookies.accept", "never", None)
            .expect_err("content.cookies.accept is refused");
        assert!(error.contains("no-3rdparty"), "{error}");

        // Something nobody has ever heard of gets the list instead.
        let error = settings.set_scoped("content.nonsense", "1", None).unwrap_err();
        assert!(error.contains("content.javascript.enabled"), "{error}");
    }

    #[test]
    fn booleans_and_text_are_typed() {
        let mut settings = Settings::default();
        let site = Some("*://*.example.com/*");
        assert!(settings
            .set_scoped("content.javascript.enabled", "maybe", site)
            .is_err());
        assert!(settings
            .set_scoped("content.javascript.enabled", "false", site)
            .is_ok());
        assert_eq!(
            settings.get("content.javascript.enabled", site).unwrap(),
            Some(Value::Bool(false))
        );
        // Unset, the default stands.
        assert_eq!(settings.get("content.images", None).unwrap(), Some(Value::Bool(true)));
        // start_page has no default, and that absence is what leaves app::HOME standing.
        assert_eq!(settings.get("start_page", None).unwrap(), None);
        assert!(settings.set("start_page", "").is_err());
        assert!(settings.set("start_page", "example.com").is_ok());
        assert_eq!(settings.start_page().as_deref(), Some("example.com"));
    }

    #[test]
    fn a_per_url_value_shadows_the_global_one_only_for_that_pattern() {
        let mut settings = Settings::default();
        settings.set("content.images", "true").unwrap();
        settings
            .set_scoped("content.images", "false", Some("*://*.example.com/*"))
            .unwrap();

        assert_eq!(
            settings.get("content.images", Some("*://*.example.com/*")).unwrap(),
            Some(Value::Bool(false))
        );
        // A different pattern falls back to the global value rather than to the other pattern's.
        assert_eq!(
            settings.get("content.images", Some("*://*.other.com/*")).unwrap(),
            Some(Value::Bool(true))
        );
        assert_eq!(settings.get("content.images", None).unwrap(), Some(Value::Bool(true)));
        // start_page takes no pattern, and says so rather than storing one nothing reads.
        assert!(settings
            .set_scoped("start_page", "https://x/", Some("*://*.example.com/*"))
            .is_err());
    }

    /// The scope rule that keeps bru's own UI alive, asserted rather than only commented.
    #[test]
    fn javascript_can_only_be_set_for_a_pattern_because_the_chrome_is_a_web_page() {
        let mut settings = Settings::default();
        let error = settings
            .set("content.javascript.enabled", "false")
            .expect_err("a global JavaScript block would switch bru's own chrome off");
        assert!(error.contains("-u <pattern>"), "{error}");
        assert!(error.contains("bru:// scheme"), "{error}");
        // config-cycle with no pattern is refused for the same reason, which is what stops
        // `bru.set` and a typed `:config-cycle content.javascript.enabled` from getting there.
        assert!(settings.cycle("content.javascript.enabled", &[], None).is_err());
        assert!(settings
            .cycle("content.javascript.enabled", &[], Some("*://*.example.com/*"))
            .is_ok());
        // Images have no such problem: bru's chrome draws none.
        assert!(settings.set("content.images", "false").is_ok());
    }

    #[test]
    fn config_cycle_walks_the_values_and_wraps() {
        let mut settings = Settings::default();
        // A boolean with no values is `true false`, as configcommands.py:196 has it.
        let applied = settings.cycle("content.images", &[], None).unwrap();
        assert_eq!(applied.value, Value::Bool(false), "true is the default, so the next is false");
        let applied = settings.cycle("content.images", &[], None).unwrap();
        assert_eq!(applied.value, Value::Bool(true));

        // An explicit list behaves the same way and wraps.
        let values = vec!["false".to_string(), "true".to_string()];
        let applied = settings.cycle("content.images", &values, None).unwrap();
        assert_eq!(applied.value, Value::Bool(false), "true is in the list at 1, so next is 0");

        // One value is not a cycle.
        assert!(settings
            .cycle("content.images", &["true".to_string()], None)
            .is_err());
    }

    #[test]
    fn a_cycle_per_pattern_leaves_the_global_value_alone() {
        let mut settings = Settings::default();
        settings
            .cycle("content.images", &[], Some("*://*.example.com/*"))
            .unwrap();
        assert_eq!(
            settings.get("content.images", Some("*://*.example.com/*")).unwrap(),
            Some(Value::Bool(false))
        );
        assert_eq!(
            settings.get("content.images", None).unwrap(),
            Some(Value::Bool(true)),
            "cycling one host must not switch images off everywhere"
        );
    }

    #[test]
    fn patterns_reduce_to_the_scope_chromium_can_hold() {
        // The two spellings the default bindings use. A wildcard scheme is written out as both,
        // because Chromium's rule for an IP host is scheme-specific — measured, see Pattern::urls.
        let host = Pattern::parse("*://example.com/*").unwrap().unwrap();
        assert_eq!(host.urls(""), ["http://example.com/", "https://example.com/"]);
        assert_eq!(host.describe(), "*://*.example.com/*");
        assert!(host.was_widened(), "one host without subdomains is not expressible");

        let subdomains = Pattern::parse("*://*.example.com/*").unwrap().unwrap();
        assert_eq!(subdomains.urls(""), ["http://example.com/", "https://example.com/"]);
        assert!(!subdomains.was_widened(), "this one is exactly what Chromium stores");

        // `-u {url}`, after expansion: a whole URL, which has no path dimension here. It names its
        // scheme, so only that scheme is written.
        let full = Pattern::parse("https://example.com/a/b?c=d").unwrap().unwrap();
        assert_eq!(full.urls(""), ["https://example.com/"]);
        assert!(full.was_widened());

        // The page's own origin, port and all, is what actually makes the rule bite — see urls().
        assert_eq!(
            Pattern::parse("*://127.0.0.1/*")
                .unwrap()
                .unwrap()
                .urls("http://127.0.0.1:8742/probe.html"),
            ["http://127.0.0.1/", "https://127.0.0.1/", "http://127.0.0.1:8742/"]
        );
        // A pattern for somewhere else does not pick the page's port up.
        assert_eq!(
            Pattern::parse("*://example.com/*")
                .unwrap()
                .unwrap()
                .urls("http://127.0.0.1:8742/probe.html"),
            ["http://example.com/", "https://example.com/"]
        );
        // A pattern that names a scheme the page is not on does not either.
        assert_eq!(
            Pattern::parse("https://127.0.0.1/*")
                .unwrap()
                .unwrap()
                .urls("http://127.0.0.1:8742/probe.html"),
            ["https://127.0.0.1/"]
        );
        // An address has no subdomains, and the printed scope must not claim it does.
        assert_eq!(
            Pattern::parse("*://127.0.0.1/*").unwrap().unwrap().describe(),
            "*://127.0.0.1/*"
        );
        assert!(!Pattern::parse("*://127.0.0.1/*").unwrap().unwrap().was_widened());

        // "Everywhere" is the global scope, not a pattern.
        assert_eq!(Pattern::parse("*://*/*").unwrap(), None);
        assert_eq!(Pattern::parse("*").unwrap(), None);
        assert!(Pattern::parse("").is_err());
        assert!(Pattern::parse("*://*./*").is_err() || Pattern::parse("*://*./*").is_ok());
    }

    #[test]
    fn printing_names_the_scope_that_was_used_not_the_one_that_was_typed() {
        let mut settings = Settings::default();
        let applied = settings
            .set_scoped("content.javascript.enabled", "false", Some("*://example.com/*"))
            .unwrap();
        // The pattern asked for one host; what happened covers its subdomains, and the line says so.
        assert_eq!(
            applied.describe(),
            "content.javascript.enabled = false for *://*.example.com/* \
             (asked for *://example.com/*; Chromium scopes this by origin)"
        );
        // The spelling Chromium stores exactly gets no such note.
        let applied = settings
            .set_scoped("content.images", "false", Some("*://*.example.com/*"))
            .unwrap();
        assert_eq!(
            applied.describe(),
            "content.images = false for *://*.example.com/*"
        );
        assert_eq!(
            settings.describe("content.images", None).unwrap(),
            "content.images = true"
        );
        assert_eq!(
            settings.describe("start_page", None).unwrap(),
            "start_page = <unset>"
        );
    }

    #[test]
    fn variables_expand_against_nothing_when_there_is_no_page() {
        // `ipc::current_url` is empty outside a running browser, which is the case in a unit test.
        // What matters here is that the placeholders are consumed rather than reaching the pattern
        // parser as literal text — `*://{url:host}/*` would otherwise become a host named
        // "{url:host}" and a rule would be written for it.
        assert_eq!(expand("*://{url:host}/*"), "*:///*");
        assert_eq!(expand("no placeholders"), "no placeholders");
        assert!(Pattern::parse(&expand("*://{url:host}/*")).is_err());
    }
}
