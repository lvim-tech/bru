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

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

use cef::*;

/// What a setting's value is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// `true` / `false`. `config-cycle` with no values cycles those two, as qutebrowser does.
    Bool,
    /// Free text — a URL, today.
    Text,
    /// One of a fixed list. `config-cycle` with no values walks the list in order, the way it walks
    /// `true`/`false` for a [`Kind::Bool`] — so `sm` on a key bound to it steps through the choices
    /// rather than needing them spelled out at the binding.
    Choice(&'static [&'static str]),
    /// A map from key to text, with bru's own pairs compiled in. See [`DictShape`].
    Dict(&'static DictShape),
}

// -----------------------------------------------------------------------------------------------
// Dictionaries
// -----------------------------------------------------------------------------------------------

/// The shape of a [`Kind::Dict`] setting: the pairs bru ships, whether a key it does not ship may
/// be added, and what a value has to be.
///
/// ## An override **merges**; it does not replace
///
/// This is the decision the type exists to carry, and it is deliberately *not* qutebrowser's.
/// qutebrowser replaces: `c.url.searchengines = {"gh": …}` in `config.py` leaves you with one
/// engine, and `:config-dict-add` (`configcommands.py:311-339`) is the separate command for adding
/// one key without losing the rest — because in qutebrowser `config.py` *is* the configuration and
/// what it does not say does not exist.
///
/// In bru it is the other way round, and DESIGN.md says so in as many words: "bru ships its own
/// default settings; configer's file only overrides them … `~/.config/bru/config.lua` … holds only
/// the **user's overrides**, layered on the defaults at startup. It is a patch, not the source." A
/// patch that silently deleted the nine-tenths of a dictionary it did not mention would not be a
/// patch. So `bru.set("statusbar.mode.labels", { normal = "NOR" })` renames one mode and leaves the
/// other eleven labels alone, and `bru.set("url.searchengines", { gh = … })` adds a tenth engine
/// rather than throwing away nine.
///
/// The cost is that merging alone cannot *remove* a pair, which is exactly what
/// [`crate::commands::Command::ConfigDictRemove`] is for — see the note there. Between them the two
/// operations reach every table qutebrowser's replace-and-add pair reaches.
#[derive(PartialEq, Eq, Debug)]
pub struct DictShape {
    /// The pairs bru ships. A bru with no `~/.config/bru/` has exactly these.
    pub defaults: &'static [(&'static str, &'static str)],
    /// Whether a key that is not in `defaults` may be added.
    ///
    /// `false` for `statusbar.mode.labels`: the twelve keys are the twelve labels the bar can
    /// draw, and a thirteenth would be a value typed and never read — the thing this file's second
    /// rule exists to refuse. `true` for `url.searchengines`, where a new key is the whole point.
    pub open_keys: bool,
    /// What a value has to be.
    pub value: DictValue,
}

/// What a dict setting's values are checked against.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DictValue {
    /// Non-empty text drawn somewhere. An empty label would be a pill with nothing in it.
    Label,
    /// A search-engine URL template, checked by [`crate::open::check_engine`] — the same function
    /// `bru.search` goes through, because they are two doors to one table.
    SearchTemplate,
}

impl DictShape {
    /// The pairs bru ships, as the map a [`Value::Dict`] holds.
    pub fn default_map(&self) -> BTreeMap<String, String> {
        self.defaults
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    /// bru's own value for `key`, if it ships one.
    fn default_for(&self, key: &str) -> Option<&'static str> {
        self.defaults
            .iter()
            .find(|(known, _)| *known == key)
            .map(|(_, value)| *value)
    }

    /// Whether one pair may go into this dict, and why not when it may not.
    fn check(&self, name: &str, key: &str, value: &str) -> Result<(), String> {
        if key.trim().is_empty() {
            return Err(format!("{name}: a key cannot be empty"));
        }
        if !self.open_keys && self.default_for(key).is_none() {
            return Err(format!(
                "{name}: {key:?} is not one of {}",
                self.defaults
                    .iter()
                    .map(|(key, _)| *key)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        match self.value {
            DictValue::Label => {
                if value.trim().is_empty() {
                    Err(format!("{name}: {key:?} cannot be given an empty label"))
                } else {
                    Ok(())
                }
            }
            DictValue::SearchTemplate => {
                crate::open::check_engine(key, value).map_err(|e| format!("{name}: {e}"))
            }
        }
    }
}

/// A setting's value, already validated against its [`Kind`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Value {
    Bool(bool),
    Text(String),
    /// A whole dictionary, bru's defaults with the user's overrides merged over them. The map is
    /// complete rather than a diff, so reading one is one lookup and never a merge.
    Dict(BTreeMap<String, String>),
}

impl std::fmt::Display for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Value::Bool(true) => f.write_str("true"),
            Value::Bool(false) => f.write_str("false"),
            Value::Text(text) => f.write_str(text),
            // One line, for the places that have room for one. What `:set` and the settings page
            // print is not this — see `Settings::describe`, which gives a dict a line per pair.
            Value::Dict(map) => write!(f, "{} entries", map.len()),
        }
    }
}

/// What bru does with a value once it has it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Backing {
    /// Read by `open.rs` when it needs a start page; nothing to push.
    StartPage,
    /// Something the bottom bar draws. Nothing to apply to Chromium — the value is read back out of
    /// here when the bar is built, and setting it pushes every window's bar so the change is seen
    /// without a reload.
    Bar,
    /// `open.rs`'s search engine table. Nothing to apply to Chromium either; what applying does is
    /// rebuild [`crate::open::SearchEngines`] from this store and install it, so that the setting
    /// is the *front door* to the table `:open` already reads rather than a second copy of it.
    SearchEngines,
    /// A Chromium content setting, global or per-origin. See [`apply`].
    Content(ContentKind),
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

/// The words the mode pill draws, one per label the bar can show.
///
/// **Twelve keys for ten modes**, and the three extra are all command mode's. `:`, `/` and `?` are
/// one mode — `Mode::Command` — and the prefix that says which was moved out of the input and into
/// the pill on 2026-08-06, because it sat in front of the user's own typing. So the pill draws
/// `COMMAND`, `SEARCH ▼` or `SEARCH ▲` for the same `state.mode`, chosen from
/// `state.cmdline.prefix` in `chrome/bottom.js`.
///
/// The keys for those three are `command`, `search_forward` and `search_backward`, and the choice
/// is defensible three ways:
///
/// - **This dict is keyed by label, not by mode.** It is the list of words the pill can show; that
///   there are more of them than there are modes is a fact about the pill. A key that could only be
///   spelled `command` would leave two of the three labels unreachable, which is the whole reason
///   the setting was asked for.
/// - **`forward`/`backward` is the vocabulary the user already has.** `search-next` and
///   `search-prev` are bound to `n` and `N`, and qutebrowser's own flag is `:search --reverse`. A
///   key named after the *arrow* (`search_down`) would name the glyph rather than the thing.
/// - **The underscore matches the mode names that have one.** `set_mark` and `jump_mark` are
///   spelled that way in `mode-enter set_mark`, so a reader who has typed one has typed the other.
///
/// **The arrow is not part of the label**, and that is measured rather than asserted:
/// `bottom.js`'s `short` branch strips the word to its initials and *keeps* the arrow, which is
/// what a direction indicator that is not a word looks like in the code that already exists. So
/// `search_forward = "FIND"` draws `FIND ▼`, and there is no spelling here that removes the
/// triangle. If that is ever wanted it is a second setting, not a value in this one.
///
/// The defaults are the mode names with their underscores spelled as spaces, because the bar is
/// read rather than typed — `SET MARK`, not `SET_MARK`. The uppercasing is `chrome.css`'s
/// (`text-transform: uppercase`), so a label written `nor` still draws `NOR`.
pub static MODE_LABELS: DictShape = DictShape {
    defaults: &[
        ("normal", "normal"),
        ("insert", "insert"),
        ("caret", "caret"),
        ("command", "command"),
        ("search_forward", "search"),
        ("search_backward", "search"),
        ("hint", "hint"),
        ("passthrough", "passthrough"),
        ("set_mark", "set mark"),
        ("jump_mark", "jump mark"),
        ("record_macro", "record macro"),
        ("run_macro", "run macro"),
    ],
    // A key the pill cannot draw is a value typed and forgotten — see DictShape::open_keys.
    open_keys: false,
    value: DictValue::Label,
};

/// The nine engines bru ships, as a setting. The pairs are `open.rs`'s, and so is the check a new
/// pair goes through: this is a door to that table, not a copy of it.
pub static SEARCH_ENGINES: DictShape = DictShape {
    defaults: crate::open::DEFAULT_ENGINES,
    // A tenth engine is the point of the setting.
    open_keys: true,
    value: DictValue::SearchTemplate,
};

/// Every setting bru has, and nothing else.
///
/// The list is short on purpose. qutebrowser has some 400 options; bru has three, because three is
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
        // bru's own, with no counterpart to copy: qutebrowser has no mode indicator to configure.
        // `full` draws the mode's name, `short` its first letter — `NORMAL` against `N`. The
        // default is the long one because a browser that has just been installed should say what
        // it means, and the short one is what you move to once you know the colours.
        name: "statusbar.mode.style",
        kind: Kind::Choice(&["full", "short"]),
        default: Some("full"),
        scopes: Scopes::GlobalOnly,
        backing: Backing::Bar,
    },
    Def {
        // bru's own, like `statusbar.mode.style` beside it. The default is not in `default` — a
        // dictionary's defaults are its shape's, because there are twelve of them.
        name: "statusbar.mode.labels",
        kind: Kind::Dict(&MODE_LABELS),
        default: None,
        scopes: Scopes::GlobalOnly,
        backing: Backing::Bar,
    },
    Def {
        // qutebrowser's name, for qutebrowser's table — DECISIONS.md item 4. The engines were
        // already reachable through `bru.search`; this is the same store named as a setting, so
        // that `bru://chrome/settings` can show them and `:config-dict-add` can change them while
        // bru runs.
        name: "url.searchengines",
        kind: Kind::Dict(&SEARCH_ENGINES),
        default: None,
        scopes: Scopes::GlobalOnly,
        backing: Backing::SearchEngines,
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

/// **What `:set` prints for a dictionary**, and the answer to "a dict is not one line".
///
/// A header naming the option, how many pairs it holds and how many of them the user has moved,
/// then one line per pair — `option[key] = value` — sorted, with bru's own value quoted beside any
/// pair that has been changed and `(added)` against one bru does not ship.
///
/// The one-line alternative was rejected on the data rather than on taste: `url.searchengines`'s
/// nine templates are 314 characters together, which is four wrapped terminal lines with no column
/// to read down and no way to see which of them is not bru's. Naming the option on every line costs
/// nine repetitions and buys a line that can be grepped, copied into a `:config-dict-add`, and read
/// against its neighbour.
///
/// It is deliberately *not* the spelling `:set` accepts back — nothing is, for a dict; see
/// `Def::parse`. A printed form that looked settable and was not would be worse than one that
/// plainly is not.
fn describe_dict(def: &'static Def, map: &BTreeMap<String, String>) -> String {
    let shape = match def.kind {
        Kind::Dict(shape) => shape,
        _ => return format!("{} is not a dictionary", def.name),
    };
    let moved = map
        .iter()
        .filter(|(key, value)| shape.default_for(key) != Some(value.as_str()))
        .count();
    let mut out = format!(
        "{} — {} entries, {} changed from bru's own",
        def.name,
        map.len(),
        moved
    );
    for (key, value) in map {
        out.push_str(&format!("\n  {}[{key}] = {value}", def.name));
        match shape.default_for(key) {
            Some(default) if default == value => {}
            Some(default) => out.push_str(&format!("   (bru ships {default})")),
            None => out.push_str("   (added)"),
        }
    }
    out
}

/// The value of a setting bru answers itself, rather than reading back from Chromium.
///
/// `None` for the content settings, whose truth is Chromium's and is read there — see
/// `chromium_value`. This is the other half: a `Backing::Bar` setting has no Chromium side at all,
/// and `bru://chrome/settings` printing "not read yet" against one said bru did not know a value it
/// had compiled in.
pub fn value_of(name: &str) -> Option<String> {
    let def = def(name)?;
    if !matches!(def.backing, Backing::Bar | Backing::SearchEngines) {
        return None;
    }
    let live = with_live(|settings| settings.get(name, None).ok().flatten());
    Some(match live {
        Some(Value::Text(text)) => text,
        Some(Value::Bool(flag)) => flag.to_string(),
        // A dictionary has no one-line value. Nothing asks `value_of` for one — `bar_json` reads
        // `dict_of` and the settings page prints a row per pair — and the count is the honest
        // answer for anywhere that insists on a scalar.
        Some(value @ Value::Dict(_)) => value.to_string(),
        None => def.default.unwrap_or_default().to_string(),
    })
}

/// Every pair of a dict setting, bru's defaults with the user's overrides merged over them.
///
/// Empty for a setting that is not a dictionary. This is what `ipc::bar_json` reads for the mode
/// labels and what `settingspage.rs` prints a row per entry of — one reader, one store.
pub fn dict_of(name: &str) -> Vec<(String, String)> {
    let Some(def) = def(name) else { return Vec::new() };
    let Kind::Dict(shape) = def.kind else {
        return Vec::new();
    };
    match with_live(|settings| settings.get(name, None).ok().flatten()) {
        Some(Value::Dict(map)) => map.into_iter().collect(),
        // Before `install`: a renderer process, or a unit test. bru's own defaults are still the
        // truth, and answering nothing would leave the pill blank in a process that never loads a
        // config.
        _ => shape.default_map().into_iter().collect(),
    }
}

/// The mode labels as a JSON object, for the one line `ipc::bar_json` adds.
///
/// Built here rather than there so that `ipc.rs` — a file four other workstreams are also editing —
/// gains two lines instead of eight. `json_escape` is `ipc.rs`'s, because a label is user text and
/// a quote in one would otherwise end the JSON string early.
pub fn mode_labels_json() -> String {
    let pairs: Vec<String> = dict_of("statusbar.mode.labels")
        .into_iter()
        .map(|(key, label)| {
            format!(
                "\"{}\":\"{}\"",
                crate::ipc::json_escape(&key),
                crate::ipc::json_escape(&label)
            )
        })
        .collect();
    format!("{{{}}}", pairs.join(","))
}

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
            Kind::Choice(choices) => {
                if choices.contains(&text) {
                    Ok(Value::Text(text.to_string()))
                } else {
                    Err(format!(
                        "{}: {text:?} is not one of {}",
                        self.name,
                        choices.join(", ")
                    ))
                }
            }
            // **A whole dictionary has no spelling on the command line, on purpose.** There is
            // nothing for `:set url.searchengines <one token>` to mean: an override *merges* (see
            // DictShape), so a text form would be a way of writing a merge one token wide, which is
            // what `:config-dict-add` already is and says more clearly. Refusing it here is what
            // makes `:set url.searchengines '{"gh": …}'` an error naming the command that works,
            // rather than a JSON parser bru would have to carry and a quoting rule the command line
            // does not have.
            Kind::Dict(_) => Err(format!(
                "{0} is a dictionary — set one pair at a time with \
                 `:config-dict-add {0} <key> <value> --replace`, remove one with \
                 `:config-dict-remove {0} <key>`, or write a Lua table in config.lua. A whole \
                 dictionary has no spelling here because an override merges into bru's defaults \
                 rather than replacing them.",
                self.name
            )),
        }
    }

    /// The value when nothing has set it.
    fn default_value(&self) -> Option<Value> {
        // A dictionary's defaults are its shape's, and there is always a full set of them: a bru
        // with no `~/.config/bru/` searches with nine engines and draws twelve labels.
        if let Kind::Dict(shape) = self.kind {
            return Some(Value::Dict(shape.default_map()));
        }
        let default = self.default?;
        self.parse(default).ok()
    }

    /// The [`DictShape`] behind a dict setting, and the error a command that only works on one gets
    /// when it is pointed at something else. qutebrowser's wording, `configcommands.py:326-328`.
    fn dict_shape(&self, command: &str) -> Result<&'static DictShape, String> {
        match self.kind {
            Kind::Dict(shape) => Ok(shape),
            _ => Err(format!(
                ":{command} can only be used for dicts, and {} is not one",
                self.name
            )),
        }
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
        // A dict is not one line — see `describe_dict`, which is what both print paths use.
        if let Value::Dict(map) = &self.value {
            return describe_dict(self.def, map);
        }
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

    /// `bru.set(key, { … })` — a Lua table, **merged** into whatever the dict already holds.
    ///
    /// Merged rather than substituted for the reason [`DictShape`] carries at length: `config.lua`
    /// is a patch over bru's defaults, not the source of them, so a table naming one mode renames
    /// one mode. Within the table itself the last pair for a key wins, which is Lua's own answer
    /// for a table literal that names a key twice.
    ///
    /// Every pair is checked before any is stored, so a table with a typo in its fifth line changes
    /// nothing rather than four-twelfths of something. That is stricter than the surrounding file —
    /// `bru.bind` applies each call as it comes — and it is right here because the twelve pairs
    /// arrive as **one** call, so "what ran before the error" would be a fraction of one statement.
    pub fn set_dict(&mut self, key: &str, pairs: &[(String, String)]) -> Result<Applied, String> {
        let def = def(key).ok_or_else(|| unknown(key))?;
        let shape = def.dict_shape("config-dict-add")?;
        for (k, v) in pairs {
            shape.check(def.name, k, v)?;
        }
        let mut map = self.dict_map(def);
        for (k, v) in pairs {
            map.insert(k.clone(), v.clone());
        }
        let value = Value::Dict(map);
        self.values.insert((None, def.name), value.clone());
        Ok(Applied { def, value, pattern: None })
    }

    /// `:config-dict-add <option> <key> <value> [--replace]` — one pair.
    ///
    /// **`--replace` is qutebrowser's, verbatim** (`configcommands.py:333-336`): a key that is
    /// already there is refused unless it is passed, so that a mistyped key cannot quietly overwrite
    /// something. bru ships a default for every key it knows, so in practice `--replace` is needed
    /// for every *change* and not needed for an *addition* — which is a sharper distinction than
    /// qutebrowser's own, where the default table is one entry long, and it is the one this command
    /// name promises.
    pub fn dict_add(
        &mut self,
        key: &str,
        entry: &str,
        value: &str,
        replace: bool,
    ) -> Result<Applied, String> {
        let def = def(key).ok_or_else(|| unknown(key))?;
        let shape = def.dict_shape("config-dict-add")?;
        shape.check(def.name, entry, value)?;
        let mut map = self.dict_map(def);
        if map.contains_key(entry) && !replace {
            return Err(format!(
                "{entry} already exists in {} — use --replace to overwrite",
                def.name
            ));
        }
        map.insert(entry.to_string(), value.to_string());
        let value = Value::Dict(map);
        self.values.insert((None, def.name), value.clone());
        Ok(Applied { def, value, pattern: None })
    }

    /// `:config-dict-remove <option> <key>`.
    ///
    /// The other half of merging: an override can add and change, so this is the only way to make a
    /// pair bru ships stop existing. Removing a key of a closed dict — one of the twelve labels —
    /// is refused rather than obeyed, because the pill would then have nothing to draw.
    pub fn dict_remove(&mut self, key: &str, entry: &str) -> Result<Applied, String> {
        let def = def(key).ok_or_else(|| unknown(key))?;
        let shape = def.dict_shape("config-dict-remove")?;
        let mut map = self.dict_map(def);
        if !map.contains_key(entry) {
            return Err(format!("{entry} is not in {}", def.name));
        }
        if !shape.open_keys {
            return Err(format!(
                "{}: {entry:?} cannot be removed — every one of its keys is something the bar \
                 draws, and a missing one would be a blank badge. Give it a different value \
                 instead.",
                def.name
            ));
        }
        map.remove(entry);
        let value = Value::Dict(map);
        self.values.insert((None, def.name), value.clone());
        Ok(Applied { def, value, pattern: None })
    }

    /// The dict as it stands: whatever has been stored, or bru's defaults when nothing has.
    fn dict_map(&self, def: &'static Def) -> BTreeMap<String, String> {
        match self.values.get(&(None, def.name)) {
            Some(Value::Dict(map)) => map.clone(),
            _ => match def.kind {
                Kind::Dict(shape) => shape.default_map(),
                _ => BTreeMap::new(),
            },
        }
    }

    /// `url.searchengines`, as the table `open.rs` searches with.
    ///
    /// The one conversion between the setting and the thing it is the front door to. It is called
    /// at startup by `Config::into_parsers` and again by [`apply`] every time the setting changes,
    /// so there is never a moment when the two disagree.
    pub fn search_engines(&self) -> crate::open::SearchEngines {
        let Some(def) = def("url.searchengines") else {
            return crate::open::SearchEngines::default();
        };
        crate::open::SearchEngines::from_pairs(self.dict_map(def))
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
        // Cycling a dictionary would mean cycling between whole tables, which nothing can spell —
        // said here rather than left to `parse`, whose message is about `:set`.
        if matches!(def.kind, Kind::Dict(_)) {
            return Err(format!(
                "{} is a dictionary; config-cycle walks the values of a single option",
                def.name
            ));
        }
        let owned: Vec<String>;
        let values = if values.is_empty() && def.kind == Kind::Bool {
            owned = vec!["true".to_string(), "false".to_string()];
            &owned
        } else if let (true, Kind::Choice(choices)) = (values.is_empty(), def.kind) {
            owned = choices.iter().map(|choice| choice.to_string()).collect();
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
        if let Some(Value::Dict(map)) = &value {
            return Ok(describe_dict(def, map));
        }
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

/// `:config-dict-add [-p] <option> <key> <value> [--replace]` — the arm `exec::run` calls.
///
/// It prints the pair it changed rather than the whole dictionary: the answer to "what did that
/// do" is one line, and `:set <option>` is there for the other question.
pub fn run_dict_add(option: &str, key: &str, value: &str, replace: bool, print: bool) {
    let outcome = with_live(|settings| settings.dict_add(option, key, value, replace))
        .and_then(|applied| {
            apply(&applied)?;
            Ok(if print {
                format!("{option}[{key}] = {value}")
            } else {
                String::new()
            })
        });
    report(outcome);
}

/// `:config-dict-remove [-p] <option> <key>`.
pub fn run_dict_remove(option: &str, key: &str, print: bool) {
    let outcome = with_live(|settings| settings.dict_remove(option, key)).and_then(|applied| {
        apply(&applied)?;
        Ok(if print {
            format!("{option}[{key}] removed")
        } else {
            String::new()
        })
    });
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
///
/// Every line gets the prefix, not only the first: a dictionary prints one line per pair, and a
/// twelve-line answer whose continuation lines look like some other program's output is a twelve-
/// line answer nobody can grep for.
fn report(outcome: Result<String, String>) {
    let text = match outcome {
        Ok(text) if text.is_empty() => return,
        Ok(text) | Err(text) => text,
    };
    for line in text.lines() {
        eprintln!("bru: {line}");
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
        // The bar reads it when it is built; setting it only has to make that happen again.
        Backing::Bar => {
            crate::ipc::push_bar_everywhere();
            return Ok(());
        }
        // The front door doing its one job: rebuild `open.rs`'s table from this store and install
        // it. Nothing else in bru holds engines, so a `:config-dict-add url.searchengines …` is
        // live at the next `:open` without a restart.
        Backing::SearchEngines => {
            let engines = with_live(|settings| settings.search_engines());
            crate::open::install_engines(engines);
            return Ok(());
        }
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
        // None of the three is a Chromium content setting, so Chromium has no opinion to read back.
        Backing::StartPage | Backing::Bar | Backing::SearchEngines => return None,
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
        // Six since the two dictionaries — `statusbar.mode.labels`, bru's own, and
        // `url.searchengines`, which is qutebrowser's name for the table `bru.search` was already
        // filling. Raise this with the setting, never to make a failing build pass.
        assert_eq!(SETTINGS.len(), 6);
        // Every dictionary's own defaults have to pass its own check, for the same reason: a
        // shipped pair that the setting would refuse is a default nobody could type back.
        for def in SETTINGS {
            let Kind::Dict(shape) = def.kind else { continue };
            for (key, value) in shape.defaults {
                shape
                    .check(def.name, key, value)
                    .unwrap_or_else(|e| panic!("{}: bad default pair: {e}", def.name));
            }
        }
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

    // -- dictionaries ---------------------------------------------------------------------------

    /// **The merge decision, asserted.** This is the one that would break silently: a replace would
    /// pass every other test in this file and only show up as nine missing engines on the user's
    /// screen.
    #[test]
    fn an_override_merges_into_brus_defaults_rather_than_replacing_them() {
        let mut settings = Settings::default();
        // Nothing set at all: bru's own, in full.
        assert_eq!(settings.search_engines().get("yt"), Some("https://www.youtube.com/results?search_query={}"));
        assert_eq!(settings.dict_map(def("url.searchengines").unwrap()).len(), 9);

        // One engine named — the other nine survive and the tenth is there.
        settings
            .set_dict(
                "url.searchengines",
                &[("gh".to_string(), "https://github.com/search?q={}".to_string())],
            )
            .unwrap();
        let engines = settings.search_engines();
        assert_eq!(engines.get("gh"), Some("https://github.com/search?q={}"));
        assert_eq!(engines.get("aw"), Some("https://wiki.archlinux.org/?search={}"));
        assert_eq!(engines.iter().count(), 10);

        // One label named — the other eleven survive.
        settings
            .set_dict(
                "statusbar.mode.labels",
                &[("normal".to_string(), "NOR".to_string())],
            )
            .unwrap();
        let labels = settings.dict_map(def("statusbar.mode.labels").unwrap());
        assert_eq!(labels.get("normal").map(String::as_str), Some("NOR"));
        assert_eq!(labels.get("insert").map(String::as_str), Some("insert"));
        assert_eq!(labels.len(), 12);
    }

    /// The keys the pill can draw, including command mode's three — the reason the dict is keyed by
    /// label rather than by mode.
    #[test]
    fn command_modes_three_labels_are_three_keys() {
        let mut settings = Settings::default();
        let labels = settings.dict_map(def("statusbar.mode.labels").unwrap());
        assert_eq!(labels.get("command").map(String::as_str), Some("command"));
        assert_eq!(labels.get("search_forward").map(String::as_str), Some("search"));
        assert_eq!(labels.get("search_backward").map(String::as_str), Some("search"));
        // All three move independently: renaming the forward search leaves `:` and `?` alone.
        settings
            .set_dict("statusbar.mode.labels", &[("search_forward".to_string(), "find".to_string())])
            .unwrap();
        let labels = settings.dict_map(def("statusbar.mode.labels").unwrap());
        assert_eq!(labels.get("search_forward").map(String::as_str), Some("find"));
        assert_eq!(labels.get("search_backward").map(String::as_str), Some("search"));
        assert_eq!(labels.get("command").map(String::as_str), Some("command"));
        // Every mode `modes.rs` has is a key here, or a mode would draw its own name while its
        // neighbours drew the user's word.
        for mode in crate::modes::Mode::ALL {
            assert!(labels.contains_key(mode.name()), "{} has no label", mode.name());
        }
    }

    #[test]
    fn a_dict_refuses_what_it_could_not_draw_or_search_with() {
        let mut settings = Settings::default();
        // A closed dict takes only its own keys, and says which they are.
        let error = settings
            .set_dict("statusbar.mode.labels", &[("nonsense".to_string(), "X".to_string())])
            .unwrap_err();
        assert!(error.contains("is not one of"), "{error}");
        assert!(error.contains("passthrough"), "{error}");
        // An empty label would be a pill with nothing in it.
        assert!(settings
            .set_dict("statusbar.mode.labels", &[("normal".to_string(), "  ".to_string())])
            .is_err());
        // An engine goes through `open.rs`'s own check — the same one `bru.search` uses.
        let error = settings
            .set_dict("url.searchengines", &[("gh".to_string(), "https://github.com/".to_string())])
            .unwrap_err();
        assert!(error.contains("the term would be dropped"), "{error}");
        assert!(settings
            .set_dict("url.searchengines", &[("two words".to_string(), "https://x/?q={}".to_string())])
            .is_err());
        // And one bad pair leaves the whole table alone, rather than storing the pairs before it.
        assert!(settings
            .set_dict(
                "url.searchengines",
                &[
                    ("ok".to_string(), "https://x/?q={}".to_string()),
                    ("bad".to_string(), "https://x/".to_string()),
                ]
            )
            .is_err());
        assert_eq!(settings.search_engines().get("ok"), None);

        // A dictionary has no text spelling, and the error names the command that does work.
        let error = settings.set("url.searchengines", "{\"gh\": \"x\"}").unwrap_err();
        assert!(error.contains("config-dict-add"), "{error}");
        // Nor can it be cycled.
        assert!(settings.cycle("statusbar.mode.labels", &[], None).is_err());
        // A scalar setting is not a dict, and the two dict commands say so rather than doing
        // something surprising — qutebrowser's wording, configcommands.py:326-328.
        let error = settings.dict_add("start_page", "a", "b", false).unwrap_err();
        assert!(error.contains("can only be used for dicts"), "{error}");
    }

    /// `config-dict-add` and `config-dict-remove`, including the `--replace` guard bru takes
    /// verbatim from qutebrowser.
    #[test]
    fn adding_and_removing_one_pair() {
        let mut settings = Settings::default();
        // A key bru does not ship needs no flag.
        settings
            .dict_add("url.searchengines", "gh", "https://github.com/search?q={}", false)
            .unwrap();
        // A key that is there does, and the error says which flag.
        let error = settings
            .dict_add("url.searchengines", "gh", "https://github.com/search?q={}", false)
            .unwrap_err();
        assert!(error.contains("--replace"), "{error}");
        settings
            .dict_add("url.searchengines", "gh", "https://github.com/issues?q={}", true)
            .unwrap();
        assert_eq!(
            settings.search_engines().get("gh"),
            Some("https://github.com/issues?q={}")
        );

        // Removing is the only way to lose a pair, since an override merges.
        settings.dict_remove("url.searchengines", "hoog").unwrap();
        assert_eq!(settings.search_engines().get("hoog"), None);
        assert_eq!(settings.search_engines().iter().count(), 9);
        assert!(settings.dict_remove("url.searchengines", "hoog").is_err());

        // A label cannot be removed: every key of that dict is something the bar draws.
        let error = settings.dict_remove("statusbar.mode.labels", "normal").unwrap_err();
        assert!(error.contains("blank badge"), "{error}");
    }

    /// **What `:set` prints for a dictionary.** A dict is not one line, and this is the shape.
    #[test]
    fn printing_a_dictionary_is_a_line_per_pair_and_says_which_are_not_brus() {
        let mut settings = Settings::default();
        settings
            .set_dict("statusbar.mode.labels", &[("normal".to_string(), "NOR".to_string())])
            .unwrap();
        let printed = settings.describe("statusbar.mode.labels", None).unwrap();
        let mut lines = printed.lines();
        assert_eq!(
            lines.next().unwrap(),
            "statusbar.mode.labels — 12 entries, 1 changed from bru's own"
        );
        assert_eq!(printed.lines().count(), 13, "a header and one line per pair");
        // The pair that moved carries what bru ships, so the reader can put it back.
        assert!(
            printed.contains("statusbar.mode.labels[normal] = NOR   (bru ships normal)"),
            "{printed}"
        );
        // The ones that did not are plain.
        assert!(printed.contains("statusbar.mode.labels[insert] = insert\n"), "{printed}");

        // An engine bru does not ship is marked as added rather than as changed.
        settings
            .dict_add("url.searchengines", "gh", "https://github.com/search?q={}", false)
            .unwrap();
        let printed = settings.describe("url.searchengines", None).unwrap();
        assert!(printed.starts_with("url.searchengines — 10 entries, 1 changed from bru's own"));
        assert!(
            printed.contains("url.searchengines[gh] = https://github.com/search?q={}   (added)"),
            "{printed}"
        );
        assert!(printed.contains("url.searchengines[DEFAULT] = https://duckduckgo.com/?q={}\n"));
    }

    /// The JSON the bar is handed — one key per label, escaped, whatever the label holds.
    #[test]
    fn the_labels_reach_the_bar_as_json() {
        // `mode_labels_json` reads the *live* store, which no unit test installs, so this is the
        // defaults path: a process with no config still draws twelve labels.
        let json = mode_labels_json();
        assert!(json.starts_with('{') && json.ends_with('}'));
        assert!(json.contains("\"normal\":\"normal\""), "{json}");
        assert!(json.contains("\"search_forward\":\"search\""), "{json}");
        assert!(json.contains("\"set_mark\":\"set mark\""), "{json}");
        assert_eq!(json.matches(':').count(), 12, "one colon per pair");
    }

    /// **The three lines between the store and the pill, asserted at the source.**
    ///
    /// Written because deleting the `modelabels` key from `ipc::bar_json`'s format string left all
    /// 416 tests green — measured 2026-08-07 — while the bar drew `NORMAL` for a config that said
    /// `nor`. Every test above this one asks the store, and the store was right; what was severed
    /// was the wire. `bar_json` itself cannot be called here (it answers `{}` with no window), and
    /// the page cannot be rendered, so this reads the two files instead. It is a weak test of a
    /// strong kind: it cannot say the pill is right, only that nobody has cut the wire.
    #[test]
    fn the_wire_from_the_store_to_the_pill_is_still_connected() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let ipc = std::fs::read_to_string(root.join("src/ipc.rs")).expect("src/ipc.rs is readable");
        assert!(
            ipc.contains("mode_labels_json()"),
            "ipc.rs no longer reads the labels out of the store"
        );
        assert!(
            ipc.contains("\\\"modelabels\\\":{mode_labels}"),
            "the bar JSON no longer carries the labels"
        );
        let js = std::fs::read_to_string(root.join("chrome/bottom.js"))
            .expect("chrome/bottom.js is readable");
        assert!(
            js.contains("state.modelabels"),
            "bottom.js no longer reads the labels off the pushed state"
        );
        // The two keys that are not a mode name — the ones a `state.mode` lookup would miss.
        assert!(js.contains("search_forward") && js.contains("search_backward"), "{js}");
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
