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
    /// An ordered list of text, with bru's own entries compiled in. See [`ListShape`].
    List(&'static ListShape),
    // --- unhardcoded -------------------------------------------------------------------------
    /// A whole number inside a range bru names. See [`IntShape`].
    ///
    /// **`config-cycle` with no values is refused for one**, and that is the one place this kind
    /// differs from every other scalar. A [`Kind::Bool`] has two values and a [`Kind::Choice`] has
    /// its list, so both can be walked without being told what to walk; a number has 2^64 of them
    /// and "the next one" is not a thing an option means. `:config-cycle messages.timeout 1000
    /// 3000` is spelled out and works, and the error for the bare form says so rather than
    /// answering with qutebrowser's generic "needs at least two values", which reads like a bug in
    /// the command rather than a fact about the option.
    Int(&'static IntShape),
    /// A string of at least two distinct non-blank characters — qutebrowser's `UniqueCharString`
    /// with `minlen: 2` (`configdata.yml:1717-1718`), and it has exactly one setting, `hints.chars`.
    ///
    /// **It is a kind rather than a [`Kind::Text`] because a one-character value is a division by
    /// zero, not a preference.** `hints::hint_strings` divides the label space by
    /// `chars.len() - 1`, so `:set hints.chars a` would take the browser down; and a value with a
    /// character twice would generate two elements the same label, which
    /// `labels_are_unique_and_prefix_free` exists to say cannot happen. `Kind::Text`'s only rule is
    /// that the value is not empty, which catches neither.
    Chars,
    // --- end unhardcoded ---------------------------------------------------------------------
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

// -----------------------------------------------------------------------------------------------
// Lists
// -----------------------------------------------------------------------------------------------

/// The shape of a [`Kind::List`] setting: the entries bru ships, and what an entry has to be.
///
/// ## An override **appends**, exactly as a dictionary's merges
///
/// The argument is [`DictShape`]'s, word for word, and it is not weaker for a list: "A patch that
/// silently deleted the nine-tenths of a dictionary it did not mention would not be a patch."
/// `bru.set("content.blocking.adblock.lists", { "https://…/mine.txt" })` therefore adds a third
/// list and leaves EasyList and EasyPrivacy where they are, and
/// [`crate::commands::Command::ConfigListRemove`] is the only way to make one of bru's own stop
/// existing — the same division of labour the two dict commands already have.
///
/// **This kind exists because a setting already needed it**, which is `settings.rs`'s own rule
/// applied to a `Kind` rather than to a name: `adblock::DEFAULT_LISTS` was a compiled-in `[&str; 2]`
/// that `:adblock-update` fetched and nothing could change. It is this setting's `defaults` now, and
/// `:config-list-add content.blocking.adblock.lists <url>` followed by `:adblock-update` fetches
/// three. A `Kind::List` with no setting behind it would have been a kind typed and forgotten.
#[derive(PartialEq, Eq, Debug)]
pub struct ListShape {
    /// The entries bru ships, in order. A bru with no `~/.config/bru/` has exactly these.
    pub defaults: &'static [&'static str],
    /// What an entry has to be.
    pub value: ListValue,
}

/// What a list setting's entries are checked against.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ListValue {
    /// An `http://` or `https://` URL bru will fetch. Checked here rather than at fetch time
    /// because a typo that is only noticed by `:adblock-update` is a typo noticed on the network.
    FetchableUrl,
    // --- tabs and statusbar --------------------------------------------------------------------
    /// The name of a field the status line can draw — `statusbar.widgets`.
    ///
    /// Checked against [`STATUSBAR_WIDGET_NAMES`] rather than against anything the chrome says,
    /// because the chrome cannot refuse: `bottom.js` walks the list and looks each name up, and an
    /// entry it has no element for would be a word typed into a setting and then silently skipped.
    /// The error names qutebrowser's widgets bru has not got and says why, so `history` answers with
    /// the reason instead of with the list.
    StatusbarWidget,
    // --- end tabs and statusbar ----------------------------------------------------------------
    // --- unhardcoded ---------------------------------------------------------------------------
    /// A zoom level, in percent, spelled qutebrowser's way (`150%`) or bru's (`150`). Both are
    /// taken because `:zoom 150` and `:zoom 150%` are already the same command here — the `%` is a
    /// suffix `commands.rs` strips — and a list that refused the spelling its own command accepts
    /// would be a rule with nothing behind it. [`percent_of`] is the one reader, and it sorts and
    /// deduplicates numerically, so `133` and `133%` cannot become two levels.
    Percent,
    // --- end unhardcoded -----------------------------------------------------------------------
}

impl ListShape {
    /// The entries bru ships, as the vector a [`Value::List`] holds.
    pub fn default_list(&self) -> Vec<String> {
        self.defaults.iter().map(|entry| entry.to_string()).collect()
    }

    /// Whether one entry may go into this list, and why not when it may not.
    fn check(&self, name: &str, value: &str) -> Result<(), String> {
        let value = value.trim();
        if value.is_empty() {
            return Err(format!("{name}: an entry cannot be empty"));
        }
        match self.value {
            ListValue::FetchableUrl => {
                if value.starts_with("http://") || value.starts_with("https://") {
                    Ok(())
                } else {
                    Err(format!(
                        "{name}: {value:?} is not a URL bru could fetch — it needs an http:// or \
                         https:// scheme"
                    ))
                }
            }
            // --- tabs and statusbar ------------------------------------------------------------
            ListValue::StatusbarWidget => {
                if STATUSBAR_WIDGET_NAMES.contains(&value) {
                    return Ok(());
                }
                if let Some(why) = REFUSED_WIDGETS
                    .iter()
                    .find(|(refused, _)| *refused == value)
                    .map(|(_, why)| *why)
                {
                    return Err(format!("{name}: bru has no {value} widget — {why}"));
                }
                Err(format!(
                    "{name}: {value:?} is not one of {}",
                    STATUSBAR_WIDGET_NAMES.join(", ")
                ))
            }
            // --- end tabs and statusbar --------------------------------------------------------
            // --- unhardcoded -------------------------------------------------------------------
            ListValue::Percent => match percent_of(value) {
                Some(percent) if (1..=10_000).contains(&percent) => Ok(()),
                Some(percent) => Err(format!(
                    "{name}: {percent}% is outside 1..10000 percent"
                )),
                None => Err(format!(
                    "{name}: {value:?} is not a zoom level — it needs to be a whole number of \
                     percent, with or without the % sign"
                )),
            },
            // --- end unhardcoded ---------------------------------------------------------------
        }
    }
}

// --- unhardcoded -------------------------------------------------------------------------------
/// `150%` or `150` as the number 150. The one reader of a [`ListValue::Percent`] entry.
pub fn percent_of(entry: &str) -> Option<u32> {
    entry.trim().trim_end_matches('%').trim().parse::<u32>().ok()
}

/// `zoom.levels`, qutebrowser's own name and its own sixteen levels (`configdata.yml:2700-2722`) —
/// which is where `exec::ZOOM_LEVELS` took them from, as a compiled-in `[u32; 16]` nothing could
/// reach.
///
/// **The order is the reader's, not the list's**, and that is the one thing this shape needs saying
/// about it. `zoom-in` steps along the levels by index, so an override that *appends* — which is
/// what every bru list does, see [`ListShape`] — would otherwise put `133%` after `500%` and make
/// `+` jump from 500 to 133. `crate::exec::zoom_levels` sorts, so the list is a **set** of levels
/// and the entry order in it means nothing.
pub static ZOOM_LEVELS: ListShape = ListShape {
    defaults: &[
        "25%", "33%", "50%", "67%", "75%", "90%", "100%", "110%", "125%", "150%", "175%", "200%",
        "250%", "300%", "400%", "500%",
    ],
    value: ListValue::Percent,
};
// --- end unhardcoded ---------------------------------------------------------------------------

/// The filter lists `:adblock-update` fetches. qutebrowser's own name and its own two defaults
/// (`configdata.yml:886-889`), which is where `adblock::DEFAULT_LISTS` took them from.
pub static ADBLOCK_LISTS: ListShape = ListShape {
    defaults: &crate::adblock::DEFAULT_LISTS,
    value: ListValue::FetchableUrl,
};

// --- tabs and statusbar ------------------------------------------------------------------------

/// Every field `#statusline` can draw, by the name `statusbar.widgets` calls it.
///
/// Five of the six are qutebrowser's own spelling (`configdata.yml:2191-2211`), so a line copied out
/// of a `config.py` keeps working: `keypress`, `search_match`, `url`, `scroll`, `tabs`. The sixth is
/// bru's, because the thing it draws is bru's — qutebrowser gives downloads a second row of their
/// own rather than a status-bar widget, so there is no name to copy.
///
/// **`mode` is deliberately not here.** The mode pill is not inside `#statusline` at all: it is the
/// bar's first grid column and the status group is the third (`chrome.css`), so it cannot be ordered
/// against these six and a name for it would be a name that moved nothing.
pub const STATUSBAR_WIDGET_NAMES: &[&str] =
    &["keypress", "search_match", "url", "scroll", "tabs", "download"];

/// qutebrowser's other five widgets, and why bru has none of them. Read by [`ListValue`]'s check, so
/// `:config-list-add statusbar.widgets history` answers with the reason rather than with the list.
const REFUSED_WIDGETS: &[(&str, &str)] = &[
    (
        "history",
        "it is a pair of arrows saying whether `H` and `L` would go anywhere, and bru's per-tab \
         history is `navigate.rs`'s, which answers that question only when it is asked. Drawing it \
         would mean asking on every push.",
    ),
    (
        "progress",
        "bru draws load state on the tab itself — the stripe down a tab's leading edge, \
         `#tabs .tab.loading::before` — rather than as a bar in the status line.",
    ),
    (
        "scroll_raw",
        "`42` where `scroll` draws `[42%]`. It is the same number in a second spelling, and \
         `scroll.rs` builds the string once; two spellings would be two formats to keep true for a \
         field that already says what it means.",
    ),
    (
        "clock",
        "it redraws itself once a second whether anything happened or not, and every push through \
         `ipc::push_for` runs a script in the chrome renderer. A clock would put that on a timer \
         for the life of the browser.",
    ),
    (
        "text",
        "`text:foo` is static text with the string after the colon. The value would be part of the \
         entry rather than a name, which is the one shape this list's check cannot hold — an entry \
         is a widget's name and nothing else.",
    ),
];

/// `statusbar.widgets` — which fields the status line draws, in the order it draws them.
///
/// The defaults are what `bottom.html` already laid out, which was transcribed from this very
/// setting's default (`configdata.yml:2212`) minus the two widgets bru has not got. So a bru with no
/// `~/.config/bru/` draws exactly what it drew before the setting existed; that is the test.
///
/// **Reordering is remove-then-add**, and that follows from [`ListShape`] rather than from anything
/// here: an override *appends*, so `:config-list-remove statusbar.widgets url` followed by
/// `:config-list-add statusbar.widgets url` moves the URL to the end of the row. Replacing the whole
/// list in one call would be the other design, and it is the one DESIGN.md rules out — a patch that
/// silently deleted the five entries it did not mention would not be a patch.
pub static STATUSBAR_WIDGETS: ListShape = ListShape {
    defaults: STATUSBAR_WIDGET_NAMES,
    value: ListValue::StatusbarWidget,
};
// --- end tabs and statusbar --------------------------------------------------------------------

// --- unhardcoded -------------------------------------------------------------------------------
// -----------------------------------------------------------------------------------------------
// Numbers
// -----------------------------------------------------------------------------------------------

/// The shape of a [`Kind::Int`] setting: the range bru will hold, what the number counts, and what
/// a sentinel at one end of the range means.
///
/// **The range is not decoration.** Every number lifted out of the code was a `const` that the rest
/// of its file was written against: `hints::MIN_CHARS` is an exponent, `scroll::STEP` is multiplied
/// by a count that can be 1000, and `messages::LOG_LIMIT` sizes a `Vec` that is written on every
/// message. A setting that took any `i64` would turn each of those from a constant into a way to
/// hang the browser, which is the opposite of what unhardcoding one is for. So the bound is stated
/// where the value is, next to the default it belongs to, and `:set` refuses what is outside it and
/// says what the range was.
#[derive(PartialEq, Eq, Debug)]
pub struct IntShape {
    /// The smallest value bru will hold, inclusive.
    pub min: i64,
    /// The largest, inclusive. Bounded even where qutebrowser is not — see the type's own note.
    pub max: i64,
    /// What the number counts: "milliseconds", "pixels", "characters". It goes in the error and in
    /// `bru://chrome/settings`'s "what it takes" column, where "a whole number" on its own says
    /// nothing about whether 3000 is a long time or a short one.
    pub unit: &'static str,
    /// What the value at `min` means when it is a sentinel rather than the smallest sensible
    /// amount — `-1` on `downloads.remove_finished` is "never", not "minus one millisecond". Empty
    /// when there is none, which is most of them.
    pub sentinel: &'static str,
}

impl IntShape {
    /// Whether a number may go into this setting, and why not when it may not.
    fn check(&self, name: &str, value: i64) -> Result<(), String> {
        if value >= self.min && value <= self.max {
            return Ok(());
        }
        let sentinel = if self.sentinel.is_empty() {
            String::new()
        } else {
            format!(" ({} is {})", self.min, self.sentinel)
        };
        Err(format!(
            "{name}: {value} is outside {}..{} {}{sentinel}",
            self.min, self.max, self.unit
        ))
    }
}

/// `hints.min_chars` — how many characters the shortest label may be.
///
/// qutebrowser's `minval: 1` (`configdata.yml:1751`) and no maximum. bru caps at 5 because the
/// number is an **exponent**: `hint_strings` asks for `chars.len().pow(needed)` labels, so 5 on the
/// nine-character default is 59,049 and 20 is 12 quintillion and an instant overflow panic. Five is
/// past anything a page needs — the largest page measured here wanted three.
static HINT_MIN_CHARS: IntShape =
    IntShape { min: 1, max: 5, unit: "characters", sentinel: "" };

/// `messages.timeout` — how long a message stays in the bar.
///
/// qutebrowser's `minval: 0` with 0 meaning "never clear" (`configdata.yml:2060`), which bru now
/// honours too. The maximum is a day: a timeout longer than that is "never" spelled the long way,
/// and the value is handed to `post_delayed_task` as milliseconds.
static MESSAGE_TIMEOUT: IntShape =
    IntShape { min: 0, max: 86_400_000, unit: "milliseconds", sentinel: "never clear" };

/// `messages.limit` — how many messages `:messages` keeps.
///
/// **bru's own name**: qutebrowser has no such option — the comment in `message.rs` cited
/// `configdata.yml:2044` for it and that line is `keyhint.delay`. The number 100 is still bru's and
/// still what it was. The ceiling is 10,000 because the buffer is a `Vec` walked from the front on
/// every overflow.
static MESSAGE_LIMIT: IntShape =
    IntShape { min: 1, max: 10_000, unit: "messages", sentinel: "" };

/// `downloads.remove_finished` — how long a finished download's row stays.
///
/// qutebrowser's `minval: -1`, with -1 meaning never (`configdata.yml:1557-1565`), and bru keeps
/// both. The maximum is a day, for [`MESSAGE_TIMEOUT`]'s reason.
static REMOVE_FINISHED: IntShape =
    IntShape { min: -1, max: 86_400_000, unit: "milliseconds", sentinel: "never remove" };

/// `zoom.default` — the level `=` returns to.
///
/// qutebrowser spells it as a percentage string (`100%`); bru's is the number, because `:zoom 150`
/// and `:zoom 150%` are already the same command here and the `%` is a suffix the parser strips.
/// The range is Chromium's own: `set_zoom_level` takes a logarithm, and 1% and 10,000% are past
/// where a page is readable in either direction.
static ZOOM_DEFAULT: IntShape =
    IntShape { min: 1, max: 10_000, unit: "percent", sentinel: "" };

/// `scroll.step_px` — how far one `j` moves.
///
/// **bru's own name, and the one number this project exists for.** qutebrowser has no option here
/// at all: it scrolls by lines through `window.scrollBy`, which is the mechanism DESIGN.md rejects.
/// 120 is three of Chromium's 40-pixel wheel notches and is exactly what `scroll::STEP` has always
/// been; changing the default would be changing the feel the project was built to get.
///
/// The ceiling is one wheel event's own ceiling: measured (CEF-NOTES, "Input"), a single
/// `send_mouse_wheel_event` moves at most one viewport whatever the delta says, so a step past a
/// screen height is a number that lies about what it does. 10,000 is past any display here and
/// still a number `j` multiplied by a count of 1000 cannot overflow.
static SCROLL_STEP: IntShape =
    IntShape { min: 1, max: 10_000, unit: "pixels", sentinel: "" };
// --- end unhardcoded ---------------------------------------------------------------------------

// --- src/scrollbar.rs --------------------------------------------------------------------------
/// `scrollbar.width`, in CSS pixels.
///
/// Chromium's native bar measures 15 here and the completion's is 6, so the useful range sits
/// around them rather than at either end. **1 is allowed and is not the same as off**: a one-pixel
/// bar is a hairline the page still reserves space for, which is a look somebody may want, and
/// `scrollbar.style false` is what "no bar of bru's" means. 64 is past any pointer target and is
/// there to stop a typo from taking half the viewport.
static SCROLLBAR_WIDTH: IntShape =
    IntShape { min: 1, max: 64, unit: "pixels", sentinel: "" };
// --- end src/scrollbar.rs ----------------------------------------------------------------------

/// A setting's value, already validated against its [`Kind`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Value {
    Bool(bool),
    Text(String),
    // --- unhardcoded ---------------------------------------------------------------------------
    /// A whole number, already inside its [`IntShape`]'s range.
    Int(i64),
    // --- end unhardcoded -----------------------------------------------------------------------
    /// A whole dictionary, bru's defaults with the user's overrides merged over them. The map is
    /// complete rather than a diff, so reading one is one lookup and never a merge.
    Dict(BTreeMap<String, String>),
    /// A whole list, bru's defaults with the user's additions appended. Complete for the same
    /// reason a `Dict` is: what reads it wants the list, not a diff and a merge.
    List(Vec<String>),
}

impl std::fmt::Display for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Value::Bool(true) => f.write_str("true"),
            Value::Bool(false) => f.write_str("false"),
            Value::Text(text) => f.write_str(text),
            // --- unhardcoded -----------------------------------------------------------------
            // The number, and nothing else — `:set messages.timeout` prints `3000` and that is the
            // spelling `:set messages.timeout 3000` takes back, which is what `Display` is for
            // here. The unit is in `bru://chrome/settings`'s own column, where there is room.
            Value::Int(number) => write!(f, "{number}"),
            // --- end unhardcoded -------------------------------------------------------------
            // One line, for the places that have room for one. What `:set` and the settings page
            // print is not this — see `Settings::describe`, which gives a dict a line per pair.
            Value::Dict(map) => write!(f, "{} entries", map.len()),
            Value::List(entries) => write!(f, "{} entries", entries.len()),
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
    // --- tabs and statusbar --------------------------------------------------------------------
    /// Something the tab strip or the window's own layout is made of.
    ///
    /// One arm for twelve settings rather than one arm each, because they all end in the same three
    /// calls and none of them has a Chromium side: rebuild the strip's payload, push the bar, and
    /// let `window.rs` decide which strips are visible and in what order. Which of the three a given
    /// setting actually moves is the difference between a redraw and a relayout, and asking that
    /// here would mean a table of twelve answers that has to be kept true — where doing all three is
    /// correct for every one of them and costs a relayout that `window::apply_chrome_layout` already
    /// skips when nothing moved.
    Chrome,
    // --- end tabs and statusbar ----------------------------------------------------------------
    /// A Chromium content setting, global or per-origin. See [`apply`].
    Content(ContentKind),
    /// Read by `focus.rs` when a page moves its focus; nothing to push. Chromium has no opinion
    /// about which of bru's modes a focused field means — that question only exists inside bru.
    Insert,
    /// `adblock.rs`'s filter lists. Nothing to push and nothing to rebuild either: the value is
    /// read when `:adblock-update` fetches, and changing the setting must **not** start a download
    /// on its own — `adblock.rs` says in as many words that it is "the only thing in bru that
    /// reaches the network on its own account, and it does it because somebody typed the command".
    AdblockLists,
    // --- unhardcoded ---------------------------------------------------------------------------
    /// Read out of this store at the moment it is wanted, and pushed nowhere.
    ///
    /// This is the backing of every value that was a `const` in the file that used it: the hint
    /// characters, the editor command, the download directory, the message timeout, the zoom
    /// levels. There is nothing to apply because there was nothing to apply before — the code read
    /// the constant where it stood, and now it reads the setting in the same place.
    ///
    /// **A session-shaped reader is still a reader.** `hints.rs` snapshots its five into the
    /// `Session` when `f` starts one, exactly as it already snapshots the `hint:` bindings, so
    /// changing `hints.chars` mid-session does not relabel a page that is already labelled. That is
    /// a property of the reader and not of this arm; nothing here has to know about it.
    Read,
    /// `scroll.rs`'s cached step. **The one setting in this group that is on the key path**, and
    /// the only reason it is not [`Backing::Read`]: `j` must not take the settings mutex, so the
    /// value lives in an `AtomicI32` beside the wheel code and this pushes it there. Measured — see
    /// `scroll::the_key_path_cost_of_reading_the_step`.
    ScrollStep,
    // --- src/scrollbar.rs -----------------------------------------------------------------------
    /// The scrollbar bru draws down the side of a page. Nothing in Chromium holds this — the value
    /// becomes a `<style>` element the load handler puts into each document — so applying it means
    /// running that injection again in every open tab, which is what makes `:set` change the page
    /// under the user rather than the next page they open.
    Scrollbar,
    // --- end src/scrollbar.rs -------------------------------------------------------------------
    // --- end unhardcoded -----------------------------------------------------------------------
// --- content settings --------------------------------------------------------------------------
    /// A Chromium **preference** — the other half of Chromium's settings, beside the
    /// content-settings map. `RequestContext` implements `ImplPreferenceManager`, so both are
    /// reachable from the same object, and [`dump_preferences`] is what found each of these.
    ///
    /// A preference is **global to the profile and has no URL dimension at all**, which is why
    /// every setting backed by one is [`Scopes::GlobalOnly`] even where qutebrowser marks it
    /// `supports_pattern: true`. That is the honest half of the trade: `content.headers.do_not_track`
    /// per site is not a thing CEF 151 can express, and a `-u` that silently changed the whole
    /// browser would be the failure `content.cookies.accept` is refused for.
    Preference(PrefKind),
// --- end content settings ----------------------------------------------------------------------
}

/// The content settings bru drives. Kept as bru's own enum rather than `ContentSettingTypes` so
/// that [`SETTINGS`] stays a plain table the unit tests can walk without CEF being initialised.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ContentKind {
    Javascript,
    Images,
// --- content settings --------------------------------------------------------------------------
    Autoplay,
    /// `content.mute`, and the one **inverted** member of this enum — see [`ContentKind::inverted`].
    Sound,
    Popups,
    Notifications,
    Geolocation,
    MicrophoneCapture,
    CameraCapture,
    PointerLock,
    ProtocolHandlers,
    PersistentStorage,
    Clipboard,
// --- end content settings ----------------------------------------------------------------------
}

impl ContentKind {
    fn cef(self) -> ContentSettingTypes {
        match self {
            ContentKind::Javascript => ContentSettingTypes::JAVASCRIPT,
            ContentKind::Images => ContentSettingTypes::IMAGES,
// --- content settings --------------------------------------------------------------------------
            ContentKind::Autoplay => ContentSettingTypes::AUTOPLAY,
            ContentKind::Sound => ContentSettingTypes::SOUND,
            ContentKind::Popups => ContentSettingTypes::POPUPS,
            ContentKind::Notifications => ContentSettingTypes::NOTIFICATIONS,
            ContentKind::Geolocation => ContentSettingTypes::GEOLOCATION,
            ContentKind::MicrophoneCapture => ContentSettingTypes::MEDIASTREAM_MIC,
            ContentKind::CameraCapture => ContentSettingTypes::MEDIASTREAM_CAMERA,
            ContentKind::PointerLock => ContentSettingTypes::POINTER_LOCK,
            ContentKind::ProtocolHandlers => ContentSettingTypes::PROTOCOL_HANDLERS,
            ContentKind::PersistentStorage => ContentSettingTypes::PERSISTENT_STORAGE,
            ContentKind::Clipboard => ContentSettingTypes::CLIPBOARD_READ_WRITE,
// --- end content settings ----------------------------------------------------------------------
        }
    }

// --- content settings --------------------------------------------------------------------------
    /// Whether the setting's `true` is Chromium's `BLOCK`.
    ///
    /// One member answers yes, and it is the only reason this function exists: qutebrowser spells
    /// the option `content.mute` — *is the tab silent* — and Chromium spells the rule `SOUND` —
    /// *may the tab make a noise*. `:set content.mute true` writing `ALLOW` would be a switch
    /// wired backwards, and the name is the thing that cannot change: it is qutebrowser's, and
    /// `<Alt-m>` is already bound to `tab-mute` beside it.
    fn inverted(self) -> bool {
        matches!(self, ContentKind::Sound)
    }
// --- end content settings ----------------------------------------------------------------------
}

// --- content settings ----------------------------------------------------------------------------
/// The Chromium preferences bru drives, for the `content.*` names that have one rather than a
/// content setting. bru's own enum for [`ContentKind`]'s reason: [`SETTINGS`] stays walkable
/// without CEF.
///
/// **Each name here was measured to exist and to be settable**, with
/// `--settings-probe='prefs:<dotted name>'`, before it was written down — and the dotted path
/// matters: `get_all_preferences` answers a *nested* dictionary whose top-level keys are first
/// components only, so `intl` is a dictionary and `intl.accept_languages` cannot be found by
/// walking them. `has_preference`/`can_set_preference` answer for the full path and are what the
/// probe asks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PrefKind {
    /// `enable_do_not_track` — measured exists=1 settable=1 type=VTYPE_BOOL, 2026-08-07.
    DoNotTrack,
    /// `enable_a_ping` — the same, and it is `<a ping>`'s switch.
    HyperlinkAuditing,
    /// **Written to `intl.selected_languages` and read from `intl.accept_languages`**, and this is
    /// the one place in this file where those two are different names. Both exist and both report
    /// settable, and writing the one whose name matches the header is the wrong move: Chromium
    /// *derives* `intl.accept_languages` from `intl.selected_languages` and puts its own answer
    /// back whenever it recomputes.
    ///
    /// Measured 2026-08-07, and it is not subtle — one `:set content.headers.accept_language
    /// fr-FR,fr` writing `intl.accept_languages`, then five navigations on this machine's own
    /// server:
    ///
    /// ```text
    /// /n0  Accept-Language: en-US,en;q=0.9      (before the set)
    /// /n1  Accept-Language: fr-FR,fr;q=0.9      (after it)
    /// /n2  Accept-Language: en-US,en;q=0.9
    /// /n3  Accept-Language: en-US,en;q=0.9
    /// /n4  Accept-Language: en-US,en;q=0.9
    /// ```
    ///
    /// **One navigation, and then Chromium put it back** — which read back as success the whole
    /// time, because the read was taken while the value was still there. A setting that works for
    /// the first page load after it is typed is worse than one that does not work at all, and only
    /// a page's own behaviour catches it.
    AcceptLanguage,
}

impl PrefKind {
    /// The dotted name bru writes.
    fn name(self) -> &'static str {
        match self {
            PrefKind::DoNotTrack => "enable_do_not_track",
            PrefKind::HyperlinkAuditing => "enable_a_ping",
            PrefKind::AcceptLanguage => "intl.selected_languages",
        }
    }

    /// The dotted name bru reads back, which is the same one for all but the language list — see
    /// [`PrefKind::AcceptLanguage`].
    fn read_name(self) -> &'static str {
        match self {
            PrefKind::AcceptLanguage => "intl.accept_languages",
            other => other.name(),
        }
    }
}
// --- end content settings ------------------------------------------------------------------------

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
        // The two prompt modes, added when `prompt.rs` added the modes. The merge is what found
        // them missing: `command_modes_three_labels_are_three_keys` walks `Mode::ALL`, so a mode
        // that arrives without a label here fails the build rather than quietly drawing its own
        // name beside neighbours drawing the user's word.
        ("prompt", "prompt"),
        ("yesno", "yesno"),
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

// --- content settings --------------------------------------------------------------------------
/// qutebrowser's `BoolAsk`, which is nine of its `content.*` options and seven of bru's.
///
/// The order is the order `config-cycle` with no values walks, and it is `true`, `false`, `ask`
/// rather than alphabetical: the two decisive answers first, the one that asks last, so a key bound
/// to a bare cycle steps allow → block → prompt and back rather than stopping to ask in the middle.
pub static BOOL_ASK: &[&str] = &["true", "false", "ask"];
// --- end content settings ----------------------------------------------------------------------

/// Every setting bru has, and nothing else.
///
/// The list is exactly as long as the behaviour behind it: every name here moves something, and
/// adding one without that would make `:set` a place where things are typed and forgotten.
///
/// **The cheapest of them are the ones that were already behaviour.** The block fenced
/// `unhardcoded` is fourteen values that were `const`s in the files that read them — the hint
/// characters, the message timeout, the zoom levels, the wheel step. Each already moved something
/// observable, so this file's own rule was satisfied before the name was typed; what changed is
/// only that a person can now reach the number.
///
/// **Two of the six are dictionaries**, and they are the first settings in bru whose value is not a
/// scalar — see [`Kind::Dict`] and [`DictShape`]. DESIGN.md asked for Lua as the config language
/// "so that a setting can hold a function, not only a scalar"; these are the first two to spend
/// that, and `bru.set("statusbar.mode.labels", { normal = "NOR" })` is what it buys.
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
        // What a lone engine name means: `aw` is the Arch wiki's front page with this on, and a
        // search for the word "aw" with it off. qutebrowser ships it off (configdata.yml:2583);
        // bru ships it on, asked for by the user 2026-08-07 — and bru owns its own defaults.
        name: "url.open_base_url",
        kind: Kind::Bool,
        default: Some("true"),
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
// --- content settings --------------------------------------------------------------------------
    // qutebrowser has 75 `content.*` options. These are the ones CEF 151 has something behind, and
    // every one of them was measured on a page rather than only read back — the read-back and the
    // behaviour are different claims and only the second is worth anything.
    //
    // **Twelve of them are one mechanism**: `RequestContext::set_content_setting`, which
    // `content.javascript.enabled` and `content.images` already went through. Three are a Chromium
    // *preference* instead, which is [`Backing::Preference`] and is global by construction.
    //
    // The three-valued ones spell their values `true` / `false` / `ask`, which is qutebrowser's
    // `BoolAsk` verbatim, and `ask` is honest here only because `prompt.rs` has a permission
    // handler: `on_show_permission_prompt` draws bru's own yes/no question. Without it `ask` would
    // be a value that silently denied.
    Def {
        // `configdata.yml:464`. Chromium's own default is ALLOW, and so is bru's.
        name: "content.autoplay",
        kind: Kind::Bool,
        default: Some("true"),
        scopes: Scopes::Both,
        backing: Backing::Content(ContentKind::Autoplay),
    },
    Def {
        // `configdata.yml:1332`, and the one setting in this file whose `true` is Chromium's
        // `BLOCK` — see `ContentKind::inverted`. bru already has `:tab-mute` on `<Alt-m>`; this is
        // the same silence asked for by host instead of by tab.
        name: "content.mute",
        kind: Kind::Bool,
        default: Some("false"),
        scopes: Scopes::Both,
        backing: Backing::Content(ContentKind::Sound),
    },
    Def {
        // `configdata.yml:966`. Chromium calls it POPUPS and its header says the rule "governs both
        // popups and unwanted redirects like tab-unders and framebusting", which is wider than the
        // name promises and is a reason to keep qutebrowser's default of `false`.
        //
        // **It is not `on_before_popup`**, and the difference is worth the sentence: trap 14 has bru
        // returning 1 from that callback so a `window.open` becomes a bru tab instead of a second
        // window. This setting is upstream of it — Chromium never asks when the rule is BLOCK, so
        // `popups.rs` never sees the call at all.
        name: "content.javascript.can_open_tabs_automatically",
        kind: Kind::Bool,
        default: Some("false"),
        scopes: Scopes::Both,
        backing: Backing::Content(ContentKind::Popups),
    },
    Def {
        // `configdata.yml:1147`.
        name: "content.notifications.enabled",
        kind: Kind::Choice(BOOL_ASK),
        default: Some("ask"),
        scopes: Scopes::Both,
        backing: Backing::Content(ContentKind::Notifications),
    },
    Def {
        // `configdata.yml:688`.
        name: "content.geolocation",
        kind: Kind::Choice(BOOL_ASK),
        default: Some("ask"),
        scopes: Scopes::Both,
        backing: Backing::Content(ContentKind::Geolocation),
    },
    Def {
        // `configdata.yml:1113`. Chromium keeps microphone and camera in two rules, which is why
        // qutebrowser's third name for the pair is refused rather than implemented — see [`REFUSED`].
        name: "content.media.audio_capture",
        kind: Kind::Choice(BOOL_ASK),
        default: Some("ask"),
        scopes: Scopes::Both,
        backing: Backing::Content(ContentKind::MicrophoneCapture),
    },
    Def {
        // `configdata.yml:1127`.
        name: "content.media.video_capture",
        kind: Kind::Choice(BOOL_ASK),
        default: Some("ask"),
        scopes: Scopes::Both,
        backing: Backing::Content(ContentKind::CameraCapture),
    },
    Def {
        // `configdata.yml:694`. Chromium's POINTER_LOCK, which is the same API under its current
        // name — `Element.requestPointerLock`, which used to be `webkitRequestPointerLock`.
        name: "content.mouse_lock",
        kind: Kind::Choice(BOOL_ASK),
        default: Some("ask"),
        scopes: Scopes::Both,
        backing: Backing::Content(ContentKind::PointerLock),
    },
    Def {
        // `configdata.yml:1258`. `navigator.registerProtocolHandler`.
        name: "content.register_protocol_handler",
        kind: Kind::Choice(BOOL_ASK),
        default: Some("ask"),
        scopes: Scopes::Both,
        backing: Backing::Content(ContentKind::ProtocolHandlers),
    },
    Def {
        // `configdata.yml:1211`. `navigator.storage.persist()` — whether the origin's storage
        // survives Chromium's eviction, which is what qutebrowser's name means too.
        name: "content.persistent_storage",
        kind: Kind::Choice(BOOL_ASK),
        default: Some("ask"),
        scopes: Scopes::Both,
        backing: Backing::Content(ContentKind::PersistentStorage),
    },
    Def {
        // `configdata.yml:940`. qutebrowser has four values and bru has three, and the missing one
        // is `access-paste`: Chromium keeps one rule, CLIPBOARD_READ_WRITE, whose header calls it
        // "full access to the system clipboard (sanitized read without user gesture, and
        // unsanitized read and write with user gesture)" — read and paste together. There is no
        // rule that grants reading without pasting, so `access` and `access-paste` would be two
        // spellings of one write. Shipping both would be a value typed and forgotten; the error
        // lists the three that differ.
        name: "content.javascript.clipboard",
        kind: Kind::Choice(&["none", "access", "ask"]),
        default: Some("ask"),
        scopes: Scopes::Both,
        backing: Backing::Content(ContentKind::Clipboard),
    },
    Def {
        // `configdata.yml:728`. A **preference**, so global only, where qutebrowser allows a
        // pattern — see [`Backing::Preference`].
        name: "content.headers.do_not_track",
        kind: Kind::Bool,
        default: Some("true"),
        scopes: Scopes::GlobalOnly,
        backing: Backing::Preference(PrefKind::DoNotTrack),
    },
    Def {
        // `configdata.yml:923`. `<a ping>` and `navigator.sendBeacon`, which is the tracking ping
        // a link fires on its way out. qutebrowser ships it off and so does bru.
        name: "content.hyperlink_auditing",
        kind: Kind::Bool,
        default: Some("false"),
        scopes: Scopes::GlobalOnly,
        backing: Backing::Preference(PrefKind::HyperlinkAuditing),
    },
    Def {
        // `configdata.yml:701`, and **the value is not the header** — that is the one thing worth
        // knowing here and it is measured rather than reasoned about. qutebrowser's default is the
        // header text, `en-US,en;q=0.9`, because Qt sends the string as written. Chromium's
        // `intl.accept_languages` is a *language list* and Chromium computes the quality values
        // itself: measured 2026-08-07, `:set content.headers.accept_language de-DE,de,fr` produced
        // `Accept-Language: de-DE,de;q=0.9,fr;q=0.8` on the wire, read off a request this machine
        // served. Writing qutebrowser's spelling with the `;q=0.9` in it is accepted and then
        // normalised back to `en-US,en`, so shipping that as bru's default would make
        // `:set content.headers.accept_language?` and `bru://chrome/settings` print two different
        // strings for one setting. bru ships the list.
        //
        // `Kind::Text` rather than a list because a comma-separated language list is one field
        // value everywhere it is written down, and `:config-list-add` on it would be a new
        // vocabulary for something the user already types as one string.
        name: "content.headers.accept_language",
        kind: Kind::Text,
        default: Some("en-US,en"),
        scopes: Scopes::GlobalOnly,
        backing: Backing::Preference(PrefKind::AcceptLanguage),
    },
// --- end content settings ----------------------------------------------------------------------
// --- config commands ---------------------------------------------------------------------------
    Def {
        // qutebrowser's name for qutebrowser's list (`configdata.yml:886`), and bru's own two
        // defaults are the ones `adblock.rs` already shipped. It is the first `Kind::List` in bru
        // and it exists because the list was already there and already unreachable — see
        // [`ListShape`], which carries that argument at length.
        name: "content.blocking.adblock.lists",
        kind: Kind::List(&ADBLOCK_LISTS),
        // A list's defaults are its shape's, for the same reason a dictionary's are.
        default: None,
        scopes: Scopes::GlobalOnly,
        backing: Backing::AdblockLists,
    },
// --- end config commands -----------------------------------------------------------------------
// --- tabs and statusbar ------------------------------------------------------------------------
// The two strips bru draws itself, made configurable. None of this reaches CEF's content settings:
// the tab strip and the status line are HTML pages bru writes, and the window is a `BoxLayout` bru
// fills, so every one of these moves something bru already owns. That is also why they share one
// `Backing::Chrome` — see the arm.
    Def {
        // qutebrowser's own (`configdata.yml:2217`), and the one setting in this block that does
        // not touch a strip at all: it decides where a middle-click's tab *goes*. `popups.rs`'s
        // header named the two arms it swaps a week before this existed, and the swap is exactly
        // what `webview.py:113-121` does — `WebBrowserTab` becomes a background tab and
        // `WebBrowserBackgroundTab` a foreground one when this is false.
        name: "tabs.background",
        kind: Kind::Bool,
        default: Some("true"),
        scopes: Scopes::GlobalOnly,
        backing: Backing::Chrome,
    },
    Def {
        // `configdata.yml:2343`. The user's own qutebrowser config binds `xt` to
        // `config-cycle tabs.show always never`, which is the shortest possible argument for this
        // being the first `tabs.*` setting bru grows.
        //
        // **`switching` is built, and its delay is not a setting.** qutebrowser pairs it with
        // `tabs.show_switching_delay` (an Int, 800 ms); bru has no integer kind yet — a second
        // workstream is adding one this round — so bru uses qutebrowser's own 800 and refuses the
        // name rather than shipping a knob spelled as text. See REFUSED.
        name: "tabs.show",
        kind: Kind::Choice(&["always", "never", "multiple", "switching"]),
        default: Some("always"),
        scopes: Scopes::GlobalOnly,
        backing: Backing::Chrome,
    },
    Def {
        // `configdata.yml:2333`, and **two of qutebrowser's four values**. Top and bottom are the
        // order of two children of one vertical `BoxLayout` and are honoured live, through
        // `Panel::reorder_child_view`.
        //
        // `left` and `right` are refused rather than half-built, and the reason is structural: a
        // vertical strip is not a reordering of this layout but a second layout inside it — an
        // outer vertical box holding a horizontal box that holds the strip and the pages — and
        // every index in `tabs::attach` is into the window's own child list. bru would also have to
        // grow `tabs.width` to say how wide the column is, which is a percentage-or-pixels type it
        // has no kind for. Refusing the pair is one honest sentence; building half of it would be a
        // value that sets and draws nothing.
        name: "tabs.position",
        kind: Kind::Choice(&["top", "bottom"]),
        default: Some("top"),
        scopes: Scopes::GlobalOnly,
        backing: Backing::Chrome,
    },
    Def {
        // `configdata.yml:2378`, with qutebrowser's own default string. The placeholders bru fills
        // are listed on `crate::tabs::format_title`, which is where the substitution happens —
        // `cmdline.rs`'s `{url}`/`{title}` replacement is the pattern it copies rather than a
        // template engine.
        name: "tabs.title.format",
        kind: Kind::Text,
        default: Some("{audio}{index}: {current_title}"),
        scopes: Scopes::GlobalOnly,
        backing: Backing::Chrome,
    },
    Def {
        // `configdata.yml:2419`. A pinned tab is `flex: 0 0 auto` in `chrome.css` — it shrinks to
        // its contents — so a short format is what makes pinning worth anything.
        name: "tabs.title.format_pinned",
        kind: Kind::Text,
        default: Some("{index}"),
        scopes: Scopes::GlobalOnly,
        backing: Backing::Chrome,
    },
    Def {
        // `configdata.yml:2254`. `pinned` is the value that pays for the setting: pinned tabs shrink
        // to their contents, so an icon and no words is what a row of them is for.
        name: "tabs.favicons.show",
        kind: Kind::Choice(&["always", "never", "pinned"]),
        default: Some("always"),
        scopes: Scopes::GlobalOnly,
        backing: Backing::Chrome,
    },
    Def {
        // `configdata.yml:2368`. `text-align` on `#tabs .title`, pushed as a value the page puts on
        // an attribute — the stylesheet holds the three rules, so nothing here writes CSS.
        name: "tabs.title.alignment",
        kind: Kind::Choice(&["left", "right", "center"]),
        default: Some("left"),
        scopes: Scopes::GlobalOnly,
        backing: Backing::Chrome,
    },
    Def {
        // `configdata.yml:2531`. `tabs::next_tab` and `prev_tab` are `(active + 1) % count` today,
        // which is the `true` half of this setting written as an arithmetic operator; `false` stops
        // at each end instead.
        name: "tabs.wrap",
        kind: Kind::Bool,
        default: Some("true"),
        scopes: Scopes::GlobalOnly,
        backing: Backing::Chrome,
    },
    Def {
        // `configdata.yml:2545`. The strip sets `el.title` to the tab's URL, which is the whole of
        // what a tooltip is here; `false` leaves the attribute off. qutebrowser's own note says it
        // "only affects windows opened after it has been set" — bru's takes effect on the next push,
        // because the strip is redrawn from JSON rather than from a widget tree.
        name: "tabs.tooltips",
        kind: Kind::Bool,
        default: Some("true"),
        scopes: Scopes::GlobalOnly,
        backing: Backing::Chrome,
    },
    Def {
        // `configdata.yml:2167`, and the other half of the user's `xx` binding. `in-mode` is
        // qutebrowser's `StatusBar.maybe_hide` — shown in every mode but normal.
        //
        // **`never` keeps the bar for as long as something is being typed into it**, and that is a
        // measured deviation rather than a preference: bru's command line is an `<input>` inside a
        // `BrowserView`, and a view that is not visible cannot take focus, so a literal reading of
        // `never` would make `:` a key that does nothing and leaves no way to type the `:set` that
        // undoes it. qutebrowser has the same setting and not the same problem, because its command
        // line is a widget it shows on demand. See `window::strip_hidden`.
        name: "statusbar.show",
        kind: Kind::Choice(&["always", "never", "in-mode"]),
        default: Some("always"),
        scopes: Scopes::GlobalOnly,
        backing: Backing::Chrome,
    },
    Def {
        // `configdata.yml:2186`. The same `reorder_child_view` as `tabs.position`, and the same
        // ordering rule qutebrowser's `MainWindow._add_widgets` has: a status bar asked to go to the
        // top goes above the tab strip, and one at the bottom goes below it.
        name: "statusbar.position",
        kind: Kind::Choice(&["top", "bottom"]),
        default: Some("bottom"),
        scopes: Scopes::GlobalOnly,
        backing: Backing::Chrome,
    },
    Def {
        // `configdata.yml:2191`. bru's bar had this list hard-coded — `bottom.html` says in as many
        // words that its span order was transcribed from this setting's own default — so this is the
        // second setting in bru that was already a compiled-in constant with no door to it, after
        // `content.blocking.adblock.lists`. A list's defaults are its shape's; see STATUSBAR_WIDGETS.
        name: "statusbar.widgets",
        kind: Kind::List(&STATUSBAR_WIDGETS),
        default: None,
        scopes: Scopes::GlobalOnly,
        backing: Backing::Chrome,
    },
// --- end tabs and statusbar --------------------------------------------------------------------
// --- unhardcoded -------------------------------------------------------------------------------
// Fourteen values that were already behaviour and were nailed shut: a `const` in the file that
// used it, with qutebrowser exposing every one of them as an option. Every default here is **the
// value bru had on 2026-08-07 and not qutebrowser's**, and where the two differ the `Def` says so —
// adopting qutebrowser's on the way past would be a behaviour change smuggled inside a refactor.
//
// The rule `settings.rs` opens with is satisfied before the name is typed: each of these already
// moved something observable, which is why they are the cheapest configurability bru can buy.
    Def {
        // `hints.chars`, configdata.yml:1714. Same name, same default, same type — qutebrowser's is
        // a `UniqueCharString` with `minlen: 2`, which is [`Kind::Chars`].
        name: "hints.chars",
        kind: Kind::Chars,
        default: Some("asdfghjkl"),
        scopes: Scopes::GlobalOnly,
        backing: Backing::Read,
    },
    Def {
        // `hints.min_chars`, configdata.yml:1747. Same name, same default of 1. bru's range has a
        // ceiling qutebrowser's does not — see [`HINT_MIN_CHARS`], where the number is an exponent.
        name: "hints.min_chars",
        kind: Kind::Int(&HINT_MIN_CHARS),
        default: Some("1"),
        scopes: Scopes::GlobalOnly,
        backing: Backing::Read,
    },
    Def {
        // `hints.auto_follow`, configdata.yml:1673. All four of qutebrowser's values, at
        // qutebrowser's default — and the default is the behaviour bru already had, hard-coded,
        // with a doc comment saying so. `never` is what gives `hint-follow` (`<Return>` in hint
        // mode) a job; it had none, and `hints::WHY_HINT_FOLLOW_IS_REFUSED` said in as many words
        // that it would have one "only if following on a unique match could be turned off, and bru
        // has no such setting". It has one now, so that refusal is gone and the binding is live.
        name: "hints.auto_follow",
        kind: Kind::Choice(&["always", "unique-match", "full-match", "never"]),
        default: Some("unique-match"),
        scopes: Scopes::GlobalOnly,
        backing: Backing::Read,
    },
    Def {
        // `hints.uppercase`, configdata.yml:1878. Same name, same default of false.
        name: "hints.uppercase",
        kind: Kind::Bool,
        default: Some("false"),
        scopes: Scopes::GlobalOnly,
        backing: Backing::Read,
    },
    Def {
        // `hints.scatter`, configdata.yml:1795. Same name, same default of true — `hint_strings`
        // was `_hint_scattered` and only that, and `_hint_linear` (dwb's order) is what false now
        // selects.
        name: "hints.scatter",
        kind: Kind::Bool,
        default: Some("true"),
        scopes: Scopes::GlobalOnly,
        backing: Backing::Read,
    },
    // --- src/scrollbar.rs -----------------------------------------------------------------------
    Def {
        // How wide the bar is, and how tall a horizontal one is. Twelve sits between Chromium's
        // native 15 and the completion's 6: wide enough to take with the pointer, narrow enough
        // that a page laid out against the native bar does not reflow when bru's replaces it.
        name: "scrollbar.width",
        kind: Kind::Int(&SCROLLBAR_WIDTH),
        default: Some("12"),
        scopes: Scopes::GlobalOnly,
        backing: Backing::Scrollbar,
    },
    Def {
        // **Unset means the theme**, which is the point rather than a missing default: the colour
        // then comes from `--completion-scrollbar-fg` in whatever `theme.css` is in force, so
        // swapping the theme moves the scrollbar with everything else and this setting is for the
        // one person who wants it not to. Any CSS colour — `#a7c080`, `rebeccapurple`,
        // `rgb(0 0 0 / 40%)` — because the string is handed to Chromium, which is the only thing
        // here that knows what a colour is. Same shape as `editor.command`, where unset is the
        // `$BRU_EDITOR` chain rather than nothing.
        name: "scrollbar.thumb",
        kind: Kind::Text,
        default: None,
        scopes: Scopes::GlobalOnly,
        backing: Backing::Scrollbar,
    },
    Def {
        // The track behind the thumb. Unset is `--completion-scrollbar-bg` from the theme, for
        // `scrollbar.thumb`'s reason. `transparent` is a value, and it is how the bar is made to
        // float over the page rather than sit in a channel.
        name: "scrollbar.track",
        kind: Kind::Text,
        default: None,
        scopes: Scopes::GlobalOnly,
        backing: Backing::Scrollbar,
    },
    Def {
        // **bru's own name — qutebrowser has no scrollbar setting at all.** It cannot: Qt draws
        // QtWebEngine's scrollbar and a page stylesheet has no say over it. bru's is drawn by
        // Chromium out of five `::-webkit-scrollbar` rules, which is why there is something to
        // switch off here. Off leaves Chromium's native bar, arrows and all.
        name: "scrollbar.style",
        kind: Kind::Bool,
        default: Some("true"),
        scopes: Scopes::GlobalOnly,
        backing: Backing::Scrollbar,
    },
    Def {
        // **Which side of the cascade bru takes when a page styles its own scrollbar.** True — the
        // default — puts bru's stylesheet first in the document and adds no `!important`, so
        // GitHub's and Discord's own rules come later and win. False makes every declaration
        // `!important` and bru wins everywhere.
        //
        // Neither setting can reach a page that sets `scrollbar-color`: that property and the
        // `::-webkit-scrollbar` pseudo-elements are mutually exclusive in Chromium, and `!important`
        // decides between two declarations rather than between two scrollbar mechanisms.
        name: "scrollbar.page_overrides",
        kind: Kind::Bool,
        default: Some("true"),
        scopes: Scopes::GlobalOnly,
        backing: Backing::Scrollbar,
    },
    // --- end src/scrollbar.rs -------------------------------------------------------------------
    Def {
        // `editor.command`, configdata.yml:1569. **bru's default is not qutebrowser's, and the
        // difference is deliberate.** qutebrowser ships `["gvim", "-f", "{file}", "-c", …]`; bru has
        // never had gvim on the machine it is built for, and `editor.rs` has read
        // `$BRU_EDITOR`/`$VISUAL`/`$EDITOR` and then `nvim` since it was written. Unsetting is what
        // leaves that chain standing — the same shape `start_page` has and for the same reason, so
        // a bru with no config still opens the editor the rest of this desktop opens.
        //
        // It is one string rather than qutebrowser's list because `spawn::shlex` is what
        // `editor_argv` already parses it with, and because a list in bru *appends* (see
        // [`ListShape`]) — an override that added `--flag` to the end of somebody else's editor is
        // not what anyone means by setting their editor.
        name: "editor.command",
        kind: Kind::Text,
        default: None,
        scopes: Scopes::GlobalOnly,
        backing: Backing::Read,
    },
    Def {
        // `downloads.location.directory`, configdata.yml:1468. qutebrowser's default is `null` and
        // so is bru's: unset means the desktop's answer — `$XDG_DOWNLOAD_DIR`, then
        // `user-dirs.dirs`, then `~/Downloads` — which is what `downloads::download_dir` has always
        // done and what its own doc comment predicted this setting would sit in front of.
        name: "downloads.location.directory",
        kind: Kind::Text,
        default: None,
        scopes: Scopes::GlobalOnly,
        backing: Backing::Read,
    },
    Def {
        // `downloads.location.prompt`, configdata.yml:1478. Same name, same default of true, which
        // is the behaviour `src/prompt.rs` gave bru and hard-coded. False is qutebrowser's own
        // meaning: use `downloads.location.directory` without asking.
        name: "downloads.location.prompt",
        kind: Kind::Bool,
        default: Some("true"),
        scopes: Scopes::GlobalOnly,
        backing: Backing::Read,
    },
    Def {
        // `downloads.remove_finished`, configdata.yml:1557. Same name, same default of -1, which is
        // "never" and is exactly what bru did: a finished row stayed until `:download-clear`.
        name: "downloads.remove_finished",
        kind: Kind::Int(&REMOVE_FINISHED),
        default: Some("-1"),
        scopes: Scopes::GlobalOnly,
        backing: Backing::Read,
    },
    Def {
        // `messages.timeout`, configdata.yml:2052. Same name, same default of 3000, and 0 means
        // "never clear" as it does there — which is new behaviour, since `message::TIMEOUT_MS` had
        // no way to say it.
        name: "messages.timeout",
        kind: Kind::Int(&MESSAGE_TIMEOUT),
        default: Some("3000"),
        scopes: Scopes::GlobalOnly,
        backing: Backing::Read,
    },
    Def {
        // **bru's own name**: `message::LOG_LIMIT` cited `configdata.yml:2044` for a
        // `messages.limit` that does not exist — that line is `keyhint.delay`. The 100 is bru's own
        // and is unchanged; only the name is now something a person can type.
        name: "messages.limit",
        kind: Kind::Int(&MESSAGE_LIMIT),
        default: Some("100"),
        scopes: Scopes::GlobalOnly,
        backing: Backing::Read,
    },
    Def {
        // `zoom.default`, configdata.yml:2695. qutebrowser spells the value `100%`; bru's is the
        // number, because `:zoom 150` and `:zoom 150%` are already one command here. The level is
        // the same 100 `exec::ZOOM_DEFAULT` was.
        name: "zoom.default",
        kind: Kind::Int(&ZOOM_DEFAULT),
        default: Some("100"),
        scopes: Scopes::GlobalOnly,
        backing: Backing::Read,
    },
    Def {
        // `zoom.levels`, configdata.yml:2700 — qutebrowser's sixteen, which is where
        // `exec::ZOOM_LEVELS` copied them from. A list's defaults are its shape's; see
        // [`ZOOM_LEVELS`] for why the order in it does not matter.
        name: "zoom.levels",
        kind: Kind::List(&ZOOM_LEVELS),
        default: None,
        scopes: Scopes::GlobalOnly,
        backing: Backing::Read,
    },
    Def {
        // **bru's own name for the number this project exists for.** qutebrowser has no option here
        // — it scrolls by lines through `window.scrollBy`, the mechanism DESIGN.md rejects — so
        // there was nothing to copy. 120 is `scroll::STEP` unchanged, and it is the one default in
        // this block that must not move: see [`SCROLL_STEP`].
        name: "scroll.step_px",
        kind: Kind::Int(&SCROLL_STEP),
        default: Some("120"),
        scopes: Scopes::GlobalOnly,
        backing: Backing::ScrollStep,
    },
// --- end unhardcoded ---------------------------------------------------------------------------
// --- lua runtime ---------------------------------------------------------------------------------
    Def {
        // **bru's own, with no counterpart to copy** — qutebrowser has no plugins to switch off.
        // It is read once, by `plugins::load_at_startup`, before the directory is opened; nothing
        // reads it afterwards, so turning it off while bru runs does not unload what is loaded.
        // `:plugin-disable <name>` is the runtime spelling and it is deliberately per plugin,
        // because "switch every plugin off right now" is a thing a person wants roughly never and
        // a restart already does.
        //
        // The default is `true`, and bru owns that: a directory the user has put a plugin in is a
        // directory the user meant to put a plugin in. What the setting buys is a way to start a
        // browser that reads none of them without moving the files — which is what you want at the
        // moment a plugin is what broke the last start.
        name: "plugins.enabled",
        kind: Kind::Bool,
        default: Some("true"),
        scopes: Scopes::GlobalOnly,
        backing: Backing::Read,
    },
// --- end lua runtime -----------------------------------------------------------------------------
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
         the settable preferences in the family are plugins.always_open_pdf_externally — \"open \
         PDFs in another application\" — and webkit.webprefs.plugins_enabled, which in Chromium \
         151 governs the built-in PDF viewer and MimeHandlerView and nothing else. Measured \
         2026-08-06, and the second preference re-measured 2026-08-07, which corrected this line: \
         it used to say always_open_pdf_externally was the only one. Neither is \"enable plugins\" \
         and neither can be scoped to a URL, which is how all six default bindings spell it. \
         NPAPI and PPAPI are gone, so these six keys are not waiting for work; there is no work \
         that would make them act.",
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
    // --- tabs and statusbar --------------------------------------------------------------------
    // The `tabs.*` and `statusbar.*` names bru does not implement and will not until something
    // changes. Names that are simply not built yet are **not** here: `:set` answers those with
    // "unknown setting", which is the truth, and a reason is only worth keeping when it is a reason
    // rather than a date.
    (
        "tabs.last_close",
        "what happens when the last tab closes is DECISIONS.md item 6, and it is still open — the \
         user has not chosen between bru's current behaviour (the window closes) and qutebrowser's \
         default (`ignore`, which keeps the window with a blank tab). Shipping the setting would \
         settle an open question by picking a default for it, which is the one thing an agent may \
         not do here.",
    ),
    (
        "tabs.title.elide",
        "CSS can only elide at the end. `#tabs .title` is `text-overflow: ellipsis`, which is this \
         setting's `right`; `left` and `center` would mean bru measuring the rendered width of each \
         title and cutting the string itself, and the strip is handed titles as JSON and never \
         learns how wide a tab came out. Chromium has no `text-overflow: ellipsis center`.",
    ),
    (
        "tabs.padding",
        "it is a four-number Padding, and bru has no numeric setting kind — no Kind::Int, and no \
         dict value that is a pixel count. Spelling four integers as text so that the name could \
         exist would be the shape this file's second rule refuses.",
    ),
    (
        "statusbar.padding",
        "the same four-number Padding as tabs.padding, and the same missing numeric kind.",
    ),
    (
        "tabs.indicator.width",
        "a pixel count, and bru has no Kind::Int. The stripe itself is real — `#tabs .tab::before` \
         draws it and the theme colours it — so this is a missing kind rather than a missing \
         feature, and it is one line the day bru has one.",
    ),
    (
        "tabs.favicons.scale",
        "a float, and bru has no float kind either. `tabs.favicons.show` is the half of this that \
         needs no number and it is implemented.",
    ),
    (
        "tabs.show_switching_delay",
        "milliseconds, and bru has no Kind::Int. `tabs.show switching` is built and uses \
         qutebrowser's own default of 800 ms; a delay spelled as text would be a number parsed \
         twice and validated nowhere.",
    ),
    (
        "tabs.mousewheel_switching",
        "bru's tab strip takes no wheel events at all. The one pointer input it accepts is a left \
         click, forwarded from `chrome/top.js` as a `tab-select` query; a wheel over the strip \
         scrolls nothing because the strip's document is exactly as tall as the strip.",
    ),
    (
        "tabs.close_mouse_button",
        "the strip listens for one thing, `click`, and CEF gives a chrome page no middle- or \
         right-button channel bru has wired. Closing a tab with the mouse is not a behaviour bru \
         has, so this would be a setting that chooses between three ways of doing nothing.",
    ),
    (
        "tabs.close_mouse_button_on_bar",
        "it names what the button of tabs.close_mouse_button does on the empty part of the strip, \
         and that button does not exist here — see tabs.close_mouse_button.",
    ),
    (
        "tabs.tabs_are_windows",
        "DESIGN.md settles the shape as \"one window, many tabs, switched in place\", and it is not \
         reopenable. bru does have more than one window — `:open -w`, `gD`, `U` — but a mode where \
         every tab is a window would make the tab strip, `tabs.position` and `tabs.show` draw \
         nothing.",
    ),
    // --- end tabs and statusbar ----------------------------------------------------------------
// --- content settings ----------------------------------------------------------------------------
    // **These strings are read by a person on `bru://chrome/settings`, so they name nothing but
    // bru and CEF.** `help.rs::the_page_measures_bru_against_nothing` enforces that for the page it
    // owns; the rule is the user's and it is the same rule here, whether or not a key is bound to
    // one of these names today. Where a setting came from belongs in the comments above and in the
    // commit message, not in the answer someone gets when they type it.
    (
        "content.desktop_capture",
        "the rule exists and reading it kills the browser. CEF 151 has \
         CEF_CONTENT_SETTING_TYPE_DISPLAY_CAPTURE and cef-rs exposes it, but \
         RequestContext::get_content_setting on it does not fail and does not answer wrongly — the \
         browser process takes SIGTRAP and exits 133 with nothing on stderr. Measured 2026-08-07 \
         under gdb: HostContentSettingsMap::GetContentSetting immediate-crashes \
         (base/immediate_crash.h:180), reached from CefRequestContextImpl::GetContentSetting \
         (request_context_impl.cc:600). Every other type bru drives was swept the same way, one \
         browser process each, and this is the only one that does it. It costs more than one \
         setting: this page reads every setting it lists, so a row here would take the browser \
         down whenever the page was opened.",
    ),
    (
        "content.media.audio_video_capture",
        "Chromium keeps the microphone and the camera in two separate rules and has no third for \
         the pair — MEDIASTREAM_MIC and MEDIASTREAM_CAMERA are the whole of it in \
         cef_content_setting_types_t. This name could only be implemented by writing both, and it \
         would then silently overwrite content.media.audio_capture and content.media.video_capture \
         beside it, and be overwritten by them in turn. Both halves are implemented; set them in \
         one line and nothing is lost.",
    ),
    (
        "content.local_storage",
        "Chromium has no localStorage rule of its own. Measured 2026-08-07: no LOCAL_STORAGE in \
         cef_content_setting_types_t, and webkit.webprefs.local_storage_enabled answers exists=0 \
         settable=0, while cef_browser_settings_t::local_storage is read once when a browser is \
         created and so cannot change a tab that is already open. What is left is COOKIES, which \
         is the site's whole data store — a setting named local_storage that also threw away the \
         site's cookies would be a switch wired to two things and labelled with one of them.",
    ),
    (
        "content.canvas_reading",
        "nothing in CEF 151 has it. There is no entry in cef_content_setting_types_t, no field in \
         cef_browser_settings_t, and no preference: measured 2026-08-07 with \
         --settings-probe='prefs:canvas', which matched none. Refusing canvas readback in Chromium \
         is a decision taken when Blink is built, not one a running browser can take.",
    ),
    (
        "content.webgl",
        "the only spelling CEF 151 offers is cef_browser_settings_t::webgl, which is read once when \
         a browser is created. It is not a content setting — there is no WEBGL in \
         cef_content_setting_types_t — and not a preference either: webkit.webprefs.webgl1_enabled \
         and webkit.webprefs.webgl2_enabled both answer exists=0 settable=0, measured 2026-08-07. \
         So it could not be given a URL pattern, which is how every per-site key in bru is written, \
         and could not change a tab that is already open. A setting that needed a new tab to take \
         effect and could not be scoped would be two surprises rather than one option.",
    ),
    (
        "content.default_encoding",
        "cef_browser_settings_t::default_encoding, with content.webgl's objection: read once when \
         a browser is created, with no content setting and no preference behind it — \
         webkit.webprefs.default_encoding answers exists=0 settable=0, measured 2026-08-07. \
         Chromium determines a page's encoding from its bytes and its headers, so the field is a \
         fallback for documents that declare nothing, and bru cannot scope it or change it live.",
    ),
    (
        "content.xss_auditing",
        "the XSS Auditor was taken out of Chromium at M78 and CEF 151 is M151. There is no content \
         setting, no preference, and not even the command-line switch it used to answer to: \
         measured 2026-08-07, `strings libcef.so` contains no xss-auditor at all. There is nothing \
         behind the name to switch on or off.",
    ),
    (
        "content.pdfjs",
        "it asks for a different PDF viewer, which is a feature rather than a setting. Chromium's \
         viewer is built in, and the one preference CEF exposes beside it is \
         plugins.always_open_pdf_externally (exists=1 settable=1, measured 2026-08-07), which \
         means \"hand the file to another program\" — the opposite question. Honouring this name \
         would mean bru shipping and serving a JavaScript PDF viewer of its own.",
    ),
// --- end content settings ------------------------------------------------------------------------
];

/// **What `:set` prints for a dictionary**, and the answer to "a dict is not one line".
///
/// A header naming the option, how many pairs it holds and how many of them the user has moved,
/// then one line per pair — `option[key] = value` — sorted, with bru's own value quoted beside any
/// pair that has been changed and `(added)` against one bru does not ship.
///
/// The one-line alternative was rejected on the data rather than on taste: `url.searchengines`'s
/// nine templates are **321 characters** together, 386 with their keys and separators — measured
/// 2026-08-07, not guessed — which is three wrapped lines in an 132-column terminal with no column
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

// --- config commands ---------------------------------------------------------------------------
/// **What `:set` prints for a list** — [`describe_dict`]'s shape, one line per entry.
///
/// The index is 0-based and is not decoration: it is the only thing that tells two EasyList URLs
/// differing in one path component apart at a glance, and the order is the order they are fetched
/// and compiled in.
fn describe_list(def: &'static Def, entries: &[String]) -> String {
    let shape = match def.kind {
        Kind::List(shape) => shape,
        _ => return format!("{} is not a list", def.name),
    };
    let added = entries
        .iter()
        .filter(|entry| !shape.defaults.contains(&entry.as_str()))
        .count();
    let missing = shape
        .defaults
        .iter()
        .filter(|entry| !entries.iter().any(|held| held == *entry))
        .count();
    let mut out = format!(
        "{} — {} entries, {added} added and {missing} of bru's own removed",
        def.name,
        entries.len()
    );
    for (index, entry) in entries.iter().enumerate() {
        out.push_str(&format!("\n  {}[{index}] = {entry}", def.name));
        if !shape.defaults.contains(&entry.as_str()) {
            out.push_str("   (added)");
        }
    }
    out
}

/// Every entry of a list setting, bru's defaults with the user's additions appended.
///
/// Empty for a setting that is not a list. This is what `adblock.rs` reads when `:adblock-update`
/// fetches — one reader, one store, exactly as [`dict_of`] is for the labels and the engines.
pub fn list_of(name: &str) -> Vec<String> {
    let Some(def) = def(name) else { return Vec::new() };
    let Kind::List(shape) = def.kind else {
        return Vec::new();
    };
    match with_live(|settings| settings.get(name, None).ok().flatten()) {
        Some(Value::List(entries)) => entries,
        // Before `install`, or in a unit test: bru's own are still the truth.
        _ => shape.default_list(),
    }
}
// --- end config commands -----------------------------------------------------------------------

/// The value of a setting bru answers itself, rather than reading back from Chromium.
///
/// `None` for the content settings, whose truth is Chromium's and is read there — see
/// `chromium_value`. This is the other half: a `Backing::Bar` setting has no Chromium side at all,
/// and `bru://chrome/settings` printing "not read yet" against one said bru did not know a value it
/// had compiled in.
pub fn value_of(name: &str) -> Option<String> {
    let def = def(name)?;
    // `Backing::Insert` and `Backing::SearchEngines` are here for the same reason `Backing::Bar`
    // is: all of them move bru's own behaviour and are never written to Chromium, so "not read yet"
    // would be a statement about a copy Chromium does not keep.
    if !matches!(
        def.backing,
        Backing::Bar
            | Backing::Insert
            | Backing::SearchEngines
            | Backing::AdblockLists
            // --- tabs and statusbar: `Backing::Chrome` is bru's own too ------------------------
            | Backing::Chrome
            // --- unhardcoded -------------------------------------------------------------------
            // Every one of these is read out of this store when it is wanted, so bru knows its
            // own value and "not read yet" would be a statement about a copy Chromium never had.
            | Backing::Read
            | Backing::ScrollStep
            // --- end unhardcoded ---------------------------------------------------------------
            // --- src/scrollbar.rs --------------------------------------------------------------
            | Backing::Scrollbar
            // --- end src/scrollbar.rs ----------------------------------------------------------
    ) {
        return None;
    }
    let live = with_live(|settings| settings.get(name, None).ok().flatten());
    Some(match live {
        Some(Value::Text(text)) => text,
        Some(Value::Bool(flag)) => flag.to_string(),
        // --- unhardcoded -----------------------------------------------------------------------
        Some(Value::Int(number)) => number.to_string(),
        // --- end unhardcoded -------------------------------------------------------------------
        // A dictionary has no one-line value. Nothing asks `value_of` for one — `bar_json` reads
        // `dict_of` and the settings page prints a row per pair — and the count is the honest
        // answer for anywhere that insists on a scalar. A list is the same.
        Some(value @ (Value::Dict(_) | Value::List(_))) => value.to_string(),
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

// --- tabs and statusbar ------------------------------------------------------------------------
/// `statusbar.widgets` as a JSON array, for the one key `ipc::bar_json_for` adds.
///
/// Built here rather than there for `mode_labels_json`'s reason, word for word: `ipc.rs` is a file
/// several workstreams are editing at once, and it gains one line instead of eight. An entry cannot
/// contain a quote — [`ListValue::StatusbarWidget`] only accepts one of six fixed names — but it
/// goes through `json_escape` anyway, because the check and the encoder should not have to agree.
pub fn widgets_json() -> String {
    let entries: Vec<String> = list_of("statusbar.widgets")
        .into_iter()
        .map(|name| format!("\"{}\"", crate::ipc::json_escape(&name)))
        .collect();
    format!("[{}]", entries.join(","))
}
// --- end tabs and statusbar --------------------------------------------------------------------

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

// --- config commands ---------------------------------------------------------------------------
/// Whether this setting's value is a list rather than a map.
///
/// One caller: `config.rs`, which has a Lua table in its hand and has to decide whether to read it
/// as `{ a = "b" }` or as `{ "a", "b" }`. Asking the setting is the only way — the two are the same
/// Lua type, and a table with no entries is both.
pub fn is_list(name: &str) -> bool {
    matches!(def(name).map(|def| def.kind), Some(Kind::List(_)))
}
// --- end config commands -------------------------------------------------------------------

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
            // The same refusal, for the same reason — see the dict arm above. An override appends,
            // so a text form would be a way of writing an append one token wide, which is what
            // `:config-list-add` already is and says more clearly.
            Kind::List(_) => Err(format!(
                "{0} is a list — add one entry with `:config-list-add {0} <value>`, remove one \
                 with `:config-list-remove {0} <value>`, or write a Lua table in config.lua. A \
                 whole list has no spelling here because an override appends to bru's defaults \
                 rather than replacing them.",
                self.name
            )),
            // --- unhardcoded -------------------------------------------------------------------
            Kind::Int(shape) => {
                // `150%` as well as `150`, so that `:set zoom.default 150%` — which is how
                // qutebrowser spells its own value and how `:zoom` already takes one — is not an
                // error about a character the rest of bru accepts.
                let text = text.trim();
                let text = if shape.unit == "percent" {
                    text.trim_end_matches('%').trim()
                } else {
                    text
                };
                let number: i64 = text.parse().map_err(|_| {
                    format!(
                        "{}: {text:?} is not a whole number of {}",
                        self.name, shape.unit
                    )
                })?;
                shape.check(self.name, number)?;
                Ok(Value::Int(number))
            }
            Kind::Chars => {
                let text = text.trim();
                // qutebrowser's `minlen: 2` — and here it is not a style rule: `hint_strings`
                // divides by `chars.len() - 1`.
                if text.chars().count() < 2 {
                    return Err(format!(
                        "{}: {text:?} is too short — hint labels are built by dividing the label \
                         space by one less than the number of characters, so there have to be at \
                         least two",
                        self.name
                    ));
                }
                if text.chars().any(char::is_whitespace) {
                    return Err(format!(
                        "{}: {text:?} has whitespace in it, and a label cannot be typed with a \
                         space in the middle of it",
                        self.name
                    ));
                }
                let mut seen: Vec<char> = text.chars().collect();
                seen.sort_unstable();
                let before = seen.len();
                seen.dedup();
                if seen.len() != before {
                    return Err(format!(
                        "{}: {text:?} uses a character twice, which would give two elements the \
                         same label",
                        self.name
                    ));
                }
                Ok(Value::Text(text.to_string()))
            }
            // --- end unhardcoded ---------------------------------------------------------------
        }
    }

    /// The value when nothing has set it.
    fn default_value(&self) -> Option<Value> {
        // A dictionary's defaults are its shape's, and there is always a full set of them: a bru
        // with no `~/.config/bru/` searches with nine engines and draws twelve labels.
        if let Kind::Dict(shape) = self.kind {
            return Some(Value::Dict(shape.default_map()));
        }
        if let Kind::List(shape) = self.kind {
            return Some(Value::List(shape.default_list()));
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

// --- config commands ---------------------------------------------------------------------------
    /// [`Def::dict_shape`]'s twin, with qutebrowser's own wording for lists
    /// (`configcommands.py:296-298`).
    fn list_shape(&self, command: &str) -> Result<&'static ListShape, String> {
        match self.kind {
            Kind::List(shape) => Ok(shape),
            _ => Err(format!(
                ":{command} can only be used for lists, and {} is not one",
                self.name
            )),
        }
    }
// --- end config commands -------------------------------------------------------------------
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

// --- config commands ---------------------------------------------------------------------------
/// What `:config-unset` and `:config-clear` produced for one setting: enough for the caller to put
/// Chromium back and to print what happened.
#[derive(Debug)]
pub struct Unset {
    pub def: &'static Def,
    /// Whether anything was stored under that scope at all. qutebrowser raises when nothing was
    /// (`configcommands.py:259-263`); bru answers, because bru's default is a place you can be.
    pub was_customized: bool,
    /// What is in force now — bru's own, or the global value when a pattern was the thing unset.
    pub value: Option<Value>,
    pub pattern: Option<Pattern>,
}

impl Unset {
    pub fn describe(&self) -> String {
        let value = self
            .value
            .as_ref()
            .map_or_else(|| "<unset>".to_string(), |value| value.to_string());
        let scope = match &self.pattern {
            Some(pattern) => format!(" for {}", pattern.describe()),
            None => String::new(),
        };
        if self.was_customized {
            format!("{} = {value}{scope} (bru's own)", self.def.name)
        } else {
            format!("{} was already bru's own: {value}{scope}", self.def.name)
        }
    }
}

/// A Lua string literal, for the lines `:config-diff` prints.
///
/// Double quotes and backslashes are the only two characters Lua's long-form needs escaped inside a
/// `"…"` literal; a newline cannot reach here, because no setting's value survives `tokenize`.
fn lua_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
// --- end config commands -------------------------------------------------------------------

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
        if let Value::List(entries) = &self.value {
            return describe_list(self.def, entries);
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

// --- config commands ---------------------------------------------------------------------------
    /// `bru.set(key, { … })` for a **list** setting — a Lua array, **appended** to what the list
    /// already holds.
    ///
    /// Appended rather than substituted for [`ListShape`]'s reason, which is [`DictShape`]'s. An
    /// entry that is already there is skipped rather than duplicated: two identical filter-list
    /// URLs would be fetched twice into one file name, so a duplicate is not a list with two
    /// members but a list with one and a wasted request.
    ///
    /// Every entry is checked before any is stored, exactly as [`Settings::set_dict`] does it and
    /// for the same reason — the whole table is one Lua statement.
    pub fn set_list(&mut self, key: &str, values: &[String]) -> Result<Applied, String> {
        let def = def(key).ok_or_else(|| unknown(key))?;
        let shape = def.list_shape("config-list-add")?;
        for value in values {
            shape.check(def.name, value)?;
        }
        let mut entries = self.list_vec(def);
        for value in values {
            let value = value.trim().to_string();
            if !entries.contains(&value) {
                entries.push(value);
            }
        }
        let value = Value::List(entries);
        self.values.insert((None, def.name), value.clone());
        Ok(Applied { def, value, pattern: None })
    }

    /// `:config-list-add <option> <value>` — one entry, appended.
    ///
    /// qutebrowser appends without looking (`configcommands.py:305-307`); bru refuses a duplicate,
    /// because bru ships defaults and qutebrowser does not. Adding EasyList to a list that already
    /// has EasyList is a typo in every case a person could have meant, and the error says so
    /// instead of leaving two rows that download to one file.
    pub fn list_add(&mut self, key: &str, value: &str) -> Result<Applied, String> {
        let def = def(key).ok_or_else(|| unknown(key))?;
        let shape = def.list_shape("config-list-add")?;
        shape.check(def.name, value)?;
        let value_owned = value.trim().to_string();
        let mut entries = self.list_vec(def);
        if entries.contains(&value_owned) {
            return Err(format!("{value_owned} is already in {}", def.name));
        }
        entries.push(value_owned);
        let value = Value::List(entries);
        self.values.insert((None, def.name), value.clone());
        Ok(Applied { def, value, pattern: None })
    }

    /// `:config-list-remove <option> <value>` — qutebrowser's own error when it is not there
    /// (`configcommands.py:361-362`).
    ///
    /// The other half of appending, and the only way to lose one of bru's own two filter lists.
    pub fn list_remove(&mut self, key: &str, value: &str) -> Result<Applied, String> {
        let def = def(key).ok_or_else(|| unknown(key))?;
        def.list_shape("config-list-remove")?;
        let wanted = value.trim();
        let mut entries = self.list_vec(def);
        let Some(at) = entries.iter().position(|entry| entry == wanted) else {
            return Err(format!("{wanted} is not in {}", def.name));
        };
        entries.remove(at);
        let value = Value::List(entries);
        self.values.insert((None, def.name), value.clone());
        Ok(Applied { def, value, pattern: None })
    }

    /// The list as it stands: whatever has been stored, or bru's defaults when nothing has.
    fn list_vec(&self, def: &'static Def) -> Vec<String> {
        match self.values.get(&(None, def.name)) {
            Some(Value::List(entries)) => entries.clone(),
            _ => match def.kind {
                Kind::List(shape) => shape.default_list(),
                _ => Vec::new(),
            },
        }
    }

    /// `:config-unset [-u <pattern>] <option>` — put one setting back to bru's own default.
    ///
    /// **This is the command the old reading of DESIGN.md had nothing for.** When bru shipped no
    /// configuration there was no default to go back to and "unset" could only mean "forget",
    /// which is a different thing; now every setting and its default value are bru's, compiled in,
    /// so unsetting is a real destination rather than an absence.
    ///
    /// `Ok(false)` is qutebrowser's "not customized" (`configcommands.py:259-263`) and is an error
    /// there. Here it is an answer, because bru's defaults are a thing you can be already on.
    pub fn unset(&mut self, key: &str, pattern: Option<&str>) -> Result<Unset, String> {
        let def = def(key).ok_or_else(|| unknown(key))?;
        let pattern = self.scope(def, pattern)?;
        let removed = self
            .values
            .remove(&(pattern.as_ref().map(|p| p.text.clone()), def.name))
            .is_some();
        // What is now in force — the global value if a pattern was unset, otherwise bru's own.
        let value = self.get(key, pattern.as_ref().map(|p| p.text.as_str()))?;
        Ok(Unset { def, was_customized: removed, value, pattern })
    }

    /// `:config-clear` — every setting back to bru's own default, in one move.
    ///
    /// Returns what was thrown away, so that the caller can push each one back into Chromium: a
    /// content setting lives in the profile under `--user-data-dir` and survives forgetting.
    pub fn clear(&mut self) -> Vec<Unset> {
        let keys: Vec<(Option<String>, &'static str)> = self.values.keys().cloned().collect();
        let mut out = Vec::new();
        for (pattern, name) in keys {
            if let Ok(unset) = self.unset(name, pattern.as_deref()) {
                out.push(unset);
            }
        }
        out.sort_by(|a, b| {
            (a.def.name, a.pattern.as_ref().map(|p| &p.text))
                .cmp(&(b.def.name, b.pattern.as_ref().map(|p| &p.text)))
        });
        out
    }

    /// Every setting that is not on bru's own value, as the Lua that would put it back.
    ///
    /// **This is `:config-diff`, and it is also the whole of what `:config-write-py` would have
    /// been** — see [`crate::commands::Command::ConfigWritePy`], which is refused. qutebrowser's
    /// `config-diff` opens `qute://configdiff` and its `config-write-py` writes the file; bru's
    /// answer to both is the text, printed, for the user to paste into the file that is theirs.
    ///
    /// A dictionary or a list prints only the pairs and entries that are not bru's, because an
    /// override merges: pasting the whole table back would be pasting bru's defaults into a patch.
    /// A per-URL value has no Lua spelling at all — `bru.set` is global — so it is printed as the
    /// `:set -u` line that produced it, marked as such rather than as Lua that would not run.
    pub fn diff(&self) -> Vec<String> {
        let mut out = Vec::new();
        for def in SETTINGS {
            match def.kind {
                Kind::Dict(shape) => {
                    let map = self.dict_map(def);
                    let mut pairs: Vec<String> = map
                        .iter()
                        .filter(|(key, value)| shape.default_for(key) != Some(value.as_str()))
                        .map(|(key, value)| format!("[{}] = {}", lua_string(key), lua_string(value)))
                        .collect();
                    for (key, _) in shape.defaults {
                        if !map.contains_key(*key) {
                            out.push(format!(
                                "-- removed at runtime, and config.lua cannot say so: \
                                 :config-dict-remove {} {key}",
                                def.name
                            ));
                        }
                    }
                    if !pairs.is_empty() {
                        pairs.sort();
                        out.push(format!(
                            "bru.set({}, {{ {} }})",
                            lua_string(def.name),
                            pairs.join(", ")
                        ));
                    }
                }
                Kind::List(shape) => {
                    let entries = self.list_vec(def);
                    let added: Vec<String> = entries
                        .iter()
                        .filter(|entry| !shape.defaults.contains(&entry.as_str()))
                        .map(|entry| lua_string(entry))
                        .collect();
                    for entry in shape.defaults {
                        if !entries.iter().any(|held| held == entry) {
                            out.push(format!(
                                "-- removed at runtime, and config.lua cannot say so: \
                                 :config-list-remove {} {entry}",
                                def.name
                            ));
                        }
                    }
                    if !added.is_empty() {
                        out.push(format!(
                            "bru.set({}, {{ {} }})",
                            lua_string(def.name),
                            added.join(", ")
                        ));
                    }
                }
                _ => {
                    for (pattern, name) in self.values.keys() {
                        if *name != def.name {
                            continue;
                        }
                        let Some(value) = self.values.get(&(pattern.clone(), name)) else {
                            continue;
                        };
                        // Already bru's own: `:set content.images true` on a bru that ships true.
                        if pattern.is_none() && Some(value) == def.default_value().as_ref() {
                            continue;
                        }
                        out.push(match pattern {
                            Some(pattern) => format!(
                                "-- per URL, and config.lua cannot say so: \
                                 :set -u {pattern} {} {value}",
                                def.name
                            ),
                            None => format!(
                                "bru.set({}, {})",
                                lua_string(def.name),
                                lua_string(&value.to_string())
                            ),
                        });
                    }
                }
            }
        }
        out.sort();
        out
    }
// --- end config commands -------------------------------------------------------------------

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
        if matches!(def.kind, Kind::List(_)) {
            return Err(format!(
                "{} is a list; config-cycle walks the values of a single option",
                def.name
            ));
        }
        // --- unhardcoded -----------------------------------------------------------------------
        // **What `config-cycle` with no values does for a number**: it says what to type instead.
        // A `Kind::Bool` has two values and a `Kind::Choice` has its list, so both can be walked
        // without being told what to walk; a number does not, and "the next one" is not something
        // `messages.timeout` means. Falling through to the generic "needs at least two values"
        // below would read as a complaint about the command rather than a fact about the option,
        // which is the same failure `REFUSED` exists to avoid one level up.
        if let (true, Kind::Int(shape)) = (values.is_empty(), def.kind) {
            return Err(format!(
                "{0} is a number in {1}..{2} {3}, and there is no next one to step to — name the \
                 values to walk, as in `:config-cycle {0} {1} {2}`",
                def.name, shape.min, shape.max, shape.unit
            ));
        }
        // --- end unhardcoded -------------------------------------------------------------------
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
        if let Some(Value::List(entries)) = &value {
            return Ok(describe_list(def, entries));
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

// --- config commands ---------------------------------------------------------------------------
/// A copy of the store the running browser is on.
///
/// One caller: `:config-source`, which runs a second `config.lua` **over** what is already set, the
/// way qutebrowser's own `:config-source` reads `config.py` over the running config. It has to
/// start from what is live rather than from bru's defaults, or sourcing a file would silently undo
/// every `:set` typed since startup — and `:config-source --clear` is the spelling that asks for
/// exactly that and says so.
pub fn snapshot() -> Settings {
    with_live(|settings| settings.clone())
}
// --- end config commands -------------------------------------------------------------------

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

// --- tabs and statusbar ------------------------------------------------------------------------
/// One [`Kind::Text`] or [`Kind::Chars`] setting's value in force globally, falling back to its
/// compiled-in default.
///
/// [`is_on`]'s twin, and it exists for the same reason: a `Backing::Chrome` setting has no Chromium
/// side to read back from, so this is the whole interface between the store and the two strips. A
/// name this file does not know answers with the empty string rather than panicking — the callers'
/// names are all `const` in `tabs.rs`, and a typo in one should not take the browser down with it.
///
/// This is [`text_of`] with the default filled in, and the split between the two is not decoration:
/// three settings ship **no** default, and for each of them the absence is the behaviour —
/// `editor.command` unset is the `$BRU_EDITOR` chain, `downloads.location.directory` unset is the
/// desktop's XDG answer. Those callers want the `None`; a caller whose setting always has a value
/// wants this and should not have to spell an `unwrap` that can never fire.
///
/// **Not on the key path**, and that is worth saying twice for this one: it takes the settings mutex
/// and `tabs_json_in` calls it per push. A push happens when a tab is added, closed, selected,
/// retitled or re-addressed — never per keystroke, because `keys.rs` reaches `set_keystring`, which
/// pushes the *bar*.
pub fn text_or_default(name: &str) -> String {
    text_of(name).unwrap_or_else(|| {
        def(name)
            .and_then(|def| def.default)
            .unwrap_or_default()
            .to_string()
    })
}
// --- end tabs and statusbar --------------------------------------------------------------------

// --- unhardcoded -------------------------------------------------------------------------------
/// One [`Kind::Int`] setting's value in force globally, falling back to its compiled-in default.
///
/// [`is_on`]'s twin, and the same warning applies with more force: **it takes the settings mutex,
/// so it is not for the key path.** `scroll.step_px` is the one of these that a keypress needs, and
/// it does not come through here — see [`Backing::ScrollStep`] and `scroll::step`.
///
/// A name this file does not know, or one that is not a number, answers `0`. None of the callers
/// can reach that: every one of them passes a `&'static str` that is a `Def` in this file, and
/// `every_setting_bru_names_has_something_behind_it` walks them. Answering rather than panicking is
/// what keeps a typo in a caller from taking the browser down mid-download.
pub fn int_of(name: &str) -> i64 {
    match with_live(|settings| settings.get(name, None)) {
        Ok(Some(Value::Int(number))) => number,
        _ => 0,
    }
}

/// One [`Kind::Text`] or [`Kind::Chars`] setting's value in force globally.
///
/// `None` means nothing has set it **and bru ships no default for it**, which for the three
/// settings that use it is meaningful rather than missing: `editor.command` unset is the
/// `$BRU_EDITOR`/`$VISUAL`/`$EDITOR`/`nvim` chain, `downloads.location.directory` unset is the
/// desktop's XDG answer. Not for the key path, for [`int_of`]'s reason.
pub fn text_of(name: &str) -> Option<String> {
    match with_live(|settings| settings.get(name, None)) {
        Ok(Some(Value::Text(text))) => Some(text),
        _ => None,
    }
}

/// One [`Kind::Choice`] setting's value in force globally, or bru's own when nothing has set it.
///
/// A `Choice` is stored as a [`Value::Text`] — the parse is what checks it is one of the choices —
/// so this is [`text_of`] with the default filled in, which every `Choice` has by construction.
pub fn choice_of(name: &str) -> &'static str {
    let default = def(name).and_then(|def| def.default).unwrap_or("");
    let Some(live) = text_of(name) else {
        return default;
    };
    // Back to a `&'static str`: the value came out of the setting's own `Kind::Choice` list, so one
    // of those is what it is. Falling back to the default rather than leaking is what keeps this
    // from being a slow leak on a setting somebody cycles.
    match def(name).map(|def| def.kind) {
        Some(Kind::Choice(choices)) => choices
            .iter()
            .find(|choice| **choice == live)
            .copied()
            .unwrap_or(default),
        _ => default,
    }
}
// --- end unhardcoded ---------------------------------------------------------------------------

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

// --- config commands ---------------------------------------------------------------------------
/// `:config-list-add <option> <value>` — the arm `exec::run` calls.
pub fn run_list_add(option: &str, value: &str, print: bool) {
    let outcome = with_live(|settings| settings.list_add(option, value)).and_then(|applied| {
        apply(&applied)?;
        Ok(if print {
            format!("{option} += {value}")
        } else {
            String::new()
        })
    });
    report(outcome);
}

/// `:config-list-remove <option> <value>`.
pub fn run_list_remove(option: &str, value: &str, print: bool) {
    let outcome = with_live(|settings| settings.list_remove(option, value)).and_then(|applied| {
        apply(&applied)?;
        Ok(if print {
            format!("{option} -= {value}")
        } else {
            String::new()
        })
    });
    report(outcome);
}

/// `:config-unset [-u <pattern>] <option>` — one setting back to bru's own.
pub fn run_unset(option: &str, pattern: Option<&str>) {
    let pattern = pattern.map(expand);
    let outcome = with_live(|settings| settings.unset(option, pattern.as_deref())).and_then(
        |unset| {
            let line = unset.describe();
            reset(&unset)?;
            Ok(line)
        },
    );
    say(outcome);
}

/// `:config-clear` — every setting back to bru's own.
///
/// **`--save` is refused rather than ignored**, and the difference matters here more than it did
/// for `:set`'s `-t`. qutebrowser's `--save` deletes what is in `autoconfig.yml`; the file bru would
/// have to touch to mean the same thing is `~/.config/bru/config.lua`, which is configer's and
/// hand-written — DESIGN.md gives bru the defaults and gives that file the user's overrides, and bru
/// writing it would make it neither. Ignoring the flag would leave a user who typed it believing
/// their file had been emptied, which is the one outcome worse than refusing.
pub fn run_clear(save: bool) {
    let unset = with_live(|settings| settings.clear());
    let count = unset.iter().filter(|u| u.was_customized).count();
    for one in &unset {
        if let Err(error) = reset(one) {
            eprintln!("bru: {error}");
        }
    }
    if save {
        crate::message::error(
            "config-clear: --save has nothing to do. ~/.config/bru/config.lua is hand-written and \
             bru never writes it; the settings this browser is running on are back to bru's own, \
             and the file is untouched.",
        );
    }
    say(Ok(match count {
        0 => "config-clear: nothing was customized".to_string(),
        1 => "config-clear: 1 setting back to bru's own".to_string(),
        n => format!("config-clear: {n} settings back to bru's own"),
    }));
}

/// `:config-diff` — the Lua that would reproduce this browser's settings, printed.
///
/// It prints rather than opening a page, and prints the *config file's* language rather than a
/// table: what a reader of this command wants is the lines to paste into the file that is theirs.
/// The binding half is `config.rs`'s, and `exec.rs` prints the two together.
pub fn diff_lines() -> Vec<String> {
    with_live(|settings| settings.diff())
}

/// Put Chromium back where an [`Unset`] left bru.
///
/// **Forgetting a content setting is not the same as unsetting it**, and this is the whole reason
/// this function is not `apply`. Chromium keeps the rule in the profile under `--user-data-dir`,
/// beside cookies and history, so a bru that merely dropped the value from its own store would
/// restart with JavaScript still blocked on the host and no record of why.
/// `ContentSettingValues::DEFAULT` is Chromium's "there is no rule here", which is what bru's own
/// default means.
fn reset(unset: &Unset) -> Result<(), String> {
    let kind = match unset.def.backing {
        Backing::Content(kind) => kind,
        // Everything else is read out of bru's own store when it is wanted, so removing the value
        // is the whole of the work — except that the bar and the engine table are built eagerly and
        // have to be rebuilt.
// --- content settings --------------------------------------------------------------------------
        // A `Backing::Preference` comes through here too, and re-applying is exactly right for it:
        // a preference has no `DEFAULT` sentinel the way a content setting does — Chromium either
        // holds a value or holds Chromium's own — so putting bru's own back is the only spelling of
        // "unset" that leaves the profile agreeing with what `:set <option>` now prints. Every
        // preference-backed setting ships a default for that reason.
// --- end content settings ----------------------------------------------------------------------
        _ => {
            let Some(value) = unset.value.clone() else {
                return Ok(());
            };
            return apply(&Applied { def: unset.def, value, pattern: unset.pattern.clone() });
        }
    };
    let Some(context) = request_context_get_global_context() else {
        return Err("no request context — settings cannot reach Chromium".to_string());
    };
    match &unset.pattern {
        Some(pattern) => {
            let page = crate::ipc::current_url();
            for url in pattern.urls(&page) {
                let url = CefString::from(url.as_str());
                context.set_content_setting(Some(&url), None, kind.cef(), ContentSettingValues::DEFAULT);
            }
        }
        None => context.set_content_setting(None, None, kind.cef(), ContentSettingValues::DEFAULT),
    }
    Ok(())
}

/// One line, in the bar as well as on stderr.
///
/// [`report`] is what `:set` and the two dict commands use and it stays as it is: its answer can be
/// a fifteen-line dictionary, and a fifteen-line dictionary in a one-line bar is not an answer. The
/// commands below all answer in one line, and a one-line answer that only reaches stderr is an
/// answer nobody running a browser sees — `message.rs` exists now, and it echoes to stderr as well.
fn say(outcome: Result<String, String>) {
    match outcome {
        Ok(text) if text.is_empty() => {}
        Ok(text) => crate::message::info(&text),
        Err(text) => crate::message::error(&text),
    }
}
// --- end config commands -------------------------------------------------------------------

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
        // Nothing to push either: `focus.rs` reads it when a focus changes. Chromium is not told,
        // because Chromium has no idea bru has modes.
        Backing::Insert => return Ok(()),
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
        // Nothing to push, and deliberately nothing to fetch — see `Backing::AdblockLists`.
        Backing::AdblockLists => return Ok(()),
        // --- tabs and statusbar ----------------------------------------------------------------
        // All three, for every one of the twelve — see `Backing::Chrome`. Each is cheap and each
        // already stops early when nothing moved: `push_tabs_everywhere` rebuilds one JSON string
        // per window, `push_bar_everywhere` one per window, and `apply_chrome_layout` compares the
        // strips' order and visibility against what it last applied before it touches CEF.
        Backing::Chrome => {
            crate::tabs::push_tabs_everywhere();
            crate::ipc::push_bar_everywhere();
            crate::window::apply_chrome_layout_everywhere();
            return Ok(());
        }
        // --- end tabs and statusbar ------------------------------------------------------------
        // --- unhardcoded -----------------------------------------------------------------------
        // Nothing to push: the file that used to hold the constant reads the setting where the
        // constant stood. See `Backing::Read`.
        Backing::Read => return Ok(()),
        // The one exception, and the reason it is its own arm: `j` may not take this mutex, so the
        // step is kept in an atomic beside the wheel code and this is what puts it there.
        Backing::ScrollStep => {
            if let Value::Int(px) = applied.value {
                crate::scroll::set_step(px as i32);
            }
            return Ok(());
        }
        // --- src/scrollbar.rs ---------------------------------------------------------------
        // Re-run the injection in every open tab. The script removes what a previous run left
        // before it puts anything in, so this is "apply" for both settings and for either value.
        Backing::Scrollbar => {
            crate::scrollbar::reinject_everywhere();
            return Ok(());
        }
        // --- end src/scrollbar.rs -----------------------------------------------------------
        // --- end unhardcoded -------------------------------------------------------------------
// --- content settings --------------------------------------------------------------------------
        Backing::Preference(pref) => return write_preference(pref, &applied.value),
// --- end content settings ----------------------------------------------------------------------
        Backing::Content(kind) => kind,
    };
    let Some(value) = content_value(kind, &applied.value) else {
        return Err(format!(
            "{}: {} is not a value a content setting can hold",
            applied.def.name, applied.value
        ));
    };
    let Some(context) = request_context_get_global_context() else {
        return Err("no request context — settings cannot reach Chromium".to_string());
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

// --- content settings ----------------------------------------------------------------------------
/// One of bru's values as one of Chromium's, or `None` when the setting cannot hold it.
///
/// Three vocabularies meet here and the mapping is the whole of the translation:
///
/// - a [`Kind::Bool`] arrives as `Value::Bool`, and `true` is `ALLOW` — unless the kind is
///   inverted, which is `content.mute` and nothing else (see [`ContentKind::inverted`]);
/// - a [`Kind::Choice`] arrives as `Value::Text` holding one of [`BOOL_ASK`], and `ask` is
///   `ContentSettingValues::ASK` — the value `prompt.rs`'s permission handler is what makes honest;
/// - `content.javascript.clipboard` spells its two decisive values `none` and `access` instead,
///   because that is qutebrowser's word for the same rule.
///
/// `None` is unreachable from `:set`, which parses against the `Kind` first. It is the belt to
/// `Def::parse`'s braces, and it is what a dictionary or a list pointed at a content setting gets.
fn content_value(kind: ContentKind, value: &Value) -> Option<ContentSettingValues> {
    let (allow, block) = if kind.inverted() {
        (ContentSettingValues::BLOCK, ContentSettingValues::ALLOW)
    } else {
        (ContentSettingValues::ALLOW, ContentSettingValues::BLOCK)
    };
    match value {
        Value::Bool(true) => Some(allow),
        Value::Bool(false) => Some(block),
        Value::Text(text) => match text.as_str() {
            "true" | "access" => Some(allow),
            "false" | "none" => Some(block),
            "ask" => Some(ContentSettingValues::ASK),
            _ => None,
        },
        // A number is not a value any of the thirteen content-setting types can hold: Chromium's
        // `ContentSetting` is a four-valued enum, and the settings that carry a `Kind::Int` are all
        // `Backing::Read` or `Backing::ScrollStep`, which never reach here. Refused with the same
        // `None` a dictionary gets rather than rounded into `ALLOW`.
        Value::Int(_) | Value::Dict(_) | Value::List(_) => None,
    }
}

/// Write one Chromium preference, or say why it could not be written.
///
/// `set_preference` fills an **out-parameter** with Chromium's own complaint when it refuses — a
/// name that does not exist, or a value of the wrong type — and that string is worth more than a
/// boolean, so it is what the error carries. Trap 9 does not bite here: the string is one CEF
/// *writes*, and it is read back through `CefString::from(&…)` the way `app.rs` reads a switch.
///
/// `Value::Text` covers `intl.accept_languages` and `Value::Bool` the other two; a dictionary or a
/// list has no preference behind it and says so rather than writing something shaped wrongly.
fn write_preference(pref: PrefKind, value: &Value) -> Result<(), String> {
    let Some(context) = request_context_get_global_context() else {
        return Err("no request context — settings cannot reach Chromium".to_string());
    };
    let Some(mut cef_value) = value_create() else {
        return Err(format!("{}: CEF would not make a value to write", pref.name()));
    };
    match value {
        Value::Bool(flag) => {
            cef_value.set_bool(i32::from(*flag));
        }
        Value::Text(text) => {
            cef_value.set_string(Some(&CefString::from(text.as_str())));
        }
        // Chromium preferences do hold integers — `net.network_prediction_options` is one — but
        // none of the three bru drives is one, so there is no `set_int` call to reach. A number
        // arriving here means a setting was given `Backing::Preference` and a `Kind::Int` without
        // this arm being written, which is a mistake to say out loud rather than to coerce.
        Value::Int(_) | Value::Dict(_) | Value::List(_) => {
            return Err(format!(
                "{}: a preference bru drives is a boolean or a string, not a number or a table",
                pref.name()
            ));
        }
    }
    let name = CefString::from(pref.name());
    // **Not `CefString::default()`, and this is trap 9 read from the other end.** `Default` for a
    // `CefStringData` is `Borrowed(None)`, and `From<&mut CefStringUtf16> for *mut
    // _cef_string_utf16_t` answers a **null pointer** for the borrowed form — so CEF is handed
    // nowhere to write, and every failure comes back with an empty reason. Measured 2026-08-07: the
    // write below failed and said "Chromium refused the write and said nothing", twice, for two
    // different reasons neither of which was visible. `CefString::from(&str)` is the `Clear` form,
    // which owns a real `cef_string_utf16_t` for CEF to overwrite, and the placeholder has to be
    // non-empty because `cef_string_utf8_to_utf16` of an empty string produces `None` and puts the
    // null pointer back.
    let mut error = CefString::from("-");
    if context.set_preference(Some(&name), Some(&mut cef_value), Some(&mut error)) == 0 {
        let error = error.to_string();
        let error = if error == "-" { String::new() } else { error };
        return Err(if error.is_empty() {
            format!("{}: Chromium refused the write and said nothing", pref.name())
        } else {
            format!("{}: {error}", pref.name())
        });
    }
    Ok(())
}
// --- end content settings ------------------------------------------------------------------------

// --- content settings ----------------------------------------------------------------------------
/// What Chromium holds for one preference right now, in words — the read-back half of
/// [`write_preference`], and the same "ask Chromium, not bru's own store" rule
/// [`chromium_value`] is built on.
///
/// **`ImplPreferenceManager::preference` is the one to use, and `get_all_preferences` is a trap.**
/// Measured 2026-08-07, and this function was written the wrong way round first: the nested
/// dictionary `get_all_preferences(1)` answers **does not contain the leaves**. `intl` is in it and
/// is a `VTYPE_DICTIONARY`, and `--settings-probe='prefkeys:intl'` reports it has **0 keys** — after
/// a successful write of `intl.accept_languages`, which `has_preference` and `can_set_preference`
/// both answer 1 for and which `preference()` reads back as `de-DE,de`. Walking the dictionary
/// therefore says "not a value bru reads (VTYPE_INVALID)" about a preference that is set.
///
/// So the rule is: **`has_preference` / `can_set_preference` / `preference` take the full dotted
/// path and are the truth; `get_all_preferences` is for browsing the top level and nothing else.**
/// (cef-rs spells `get_preference` as `preference` — it is easy to miss when scanning the trait.)
///
/// Must run on the UI thread, exactly as trap 15's `get_content_setting` must.
fn read_preference(pref: PrefKind) -> Option<String> {
    let context = request_context_get_global_context()?;
    let name = CefString::from(pref.read_name());
    let value = context.preference(Some(&name))?;
    Some(match value.get_type() {
        t if t == ValueType::BOOL => (value.bool() != 0).to_string(),
        t if t == ValueType::STRING => CefString::from(&value.string()).to_string(),
        t if t == ValueType::INT => value.int().to_string(),
        // A name that is not there at all, or one holding a shape bru does not write. The type is
        // in the string because "not set" alone cannot tell those two apart, and that ambiguity
        // cost a measurement.
        other => format!("not a value bru reads ({other:?})"),
    })
}
// --- end content settings ------------------------------------------------------------------------

/// Push everything `config.lua` set into Chromium. Called once from `app.rs`, after `Config::load`
/// and before the first tab exists — a start page with JavaScript switched off has to load that way
/// rather than load and then be corrected.
pub fn apply_at_startup() {
    debug_assert_ne!(currently_on(ThreadId::UI), 0);
// --- content settings --------------------------------------------------------------------------
    // **A preference-backed setting has its default written even when nothing set it**, and that is
    // the difference between DESIGN.md's "a bru with no `~/.config/bru/` is fully configured" and a
    // browser that only reports its defaults. Measured 2026-08-07 on an untouched profile:
    // Chromium's own `enable_do_not_track` is **false**, `enable_a_ping` is **true** and
    // `intl.accept_languages` is **en-US,en**, while qutebrowser's — and therefore bru's — are
    // true, false and `en-US,en;q=0.9`. Without this loop `:set content.headers.do_not_track?`
    // would print `true` at a browser that was sending no DNT header at all, which is the exact
    // shape of lie this file exists to refuse.
    //
    // A content setting needs no such loop: `ContentSettingValues::DEFAULT` *is* bru's default, so
    // "nothing written" and "bru's own" are the same state there. A preference has no such
    // sentinel — see `reset`.
    for def in SETTINGS {
        if !matches!(def.backing, Backing::Preference(_)) {
            continue;
        }
        let Ok(Some(value)) = with_live(|settings| settings.get(def.name, None)) else {
            continue;
        };
        if let Err(error) = apply(&Applied { def, value, pattern: None }) {
            eprintln!("bru: {error}");
        }
    }
// --- end content settings ----------------------------------------------------------------------
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
// --- content settings --------------------------------------------------------------------------
        // Chromium *does* have an opinion here, and it is not a content setting: it is a
        // preference, read back out of the profile rather than out of the content-settings map.
        // The URL is ignored, because a preference has no URL dimension — see
        // [`Backing::Preference`].
        Backing::Preference(pref) => return read_preference(pref),
// --- end content settings ----------------------------------------------------------------------
        // None of these is a Chromium content setting, so Chromium has no opinion to read back.
        Backing::StartPage
        | Backing::Insert
        | Backing::Bar
        | Backing::SearchEngines
        | Backing::AdblockLists
        // --- tabs and statusbar ----------------------------------------------------------------
        | Backing::Chrome
        // --- unhardcoded -----------------------------------------------------------------------
        | Backing::Read
        | Backing::ScrollStep
        // --- end unhardcoded -------------------------------------------------------------------
        // --- src/scrollbar.rs ------------------------------------------------------------------
        | Backing::Scrollbar
        // --- end src/scrollbar.rs --------------------------------------------------------------
        => {
            return None;
        }
    };
    let context = request_context_get_global_context()?;
    let url = CefString::from(url);
    let value = context.content_setting(Some(&url), None, kind.cef());
// --- content settings --------------------------------------------------------------------------
    // **The one row whose Chromium word reads as its own opposite.** `content.mute` is `true` when
    // Chromium's SOUND rule is `block`, so `bru://chrome/settings` drew "content.mute … block" for
    // a tab that had just been muted — seen on the page 2026-08-07, not reasoned about — and
    // "block" beside the word "mute" is read as "muting is blocked" by anyone who has not just
    // finished reading `ContentKind::inverted`. Chromium's word stays, because this column is
    // Chromium's answer and uniformity is what makes it worth reading; what is added is the two
    // words that say which way round it is for this one setting.
    if kind.inverted() {
        return Some(
            match value {
                v if v == ContentSettingValues::ALLOW => "allow (audible)",
                v if v == ContentSettingValues::BLOCK => "block (silent)",
                v if v == ContentSettingValues::DEFAULT => "default (audible)",
                _ => "other",
            }
            .to_string(),
        );
    }
// --- end content settings ----------------------------------------------------------------------
    Some(
        match value {
            v if v == ContentSettingValues::ALLOW => "allow",
            v if v == ContentSettingValues::BLOCK => "block",
            v if v == ContentSettingValues::DEFAULT => "default",
// --- content settings --------------------------------------------------------------------------
            // **Added because the answer was already arriving and being called `other`.** Measured
            // 2026-08-07 on a profile that had never been written to: every one of the seven
            // three-valued settings — notifications, geolocation, the two media captures, pointer
            // lock, display capture, protocol handlers, persistent storage, the clipboard — read
            // back as this, and the probe printed `other` for all of them. A word that lumps the
            // browser's own default in with a value nobody has a name for is the wrong answer a
            // reader believes, which is trap 15's lesson one layer up.
            v if v == ContentSettingValues::ASK => "ask",
            v if v == ContentSettingValues::SESSION_ONLY => "session only",
// --- end content settings ----------------------------------------------------------------------
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
// --- content settings --------------------------------------------------------------------------
                if let Some(path) = pair.strip_prefix("prefkeys:") {
                    dump_preference_keys(path);
                    continue;
                }
// --- end content settings ----------------------------------------------------------------------
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

// --- content settings ----------------------------------------------------------------------------
/// `--settings-probe='prefkeys:intl'` — the keys **inside** one nested preference dictionary, with
/// their types.
///
/// [`dump_preferences`] lists only the top level, and that is the whole difficulty with
/// `get_all_preferences`: `intl` is a dictionary and `intl.accept_languages` cannot be seen from
/// outside it. An empty path lists the top level, which is what `prefs:` does with an empty needle.
///
/// It exists because a question this shape came up twice in one afternoon and both times the answer
/// was a rebuild away. `prefkeys:` is one command.
fn dump_preference_keys(path: &str) {
    debug_assert_ne!(currently_on(ThreadId::UI), 0);
    let Some(context) = request_context_get_global_context() else {
        eprintln!("settings-probe: no request context");
        return;
    };
    let Some(mut dict) = context.all_preferences(1) else {
        eprintln!("settings-probe: no preferences");
        return;
    };
    for part in path.split('.').filter(|part| !part.is_empty()) {
        let key = CefString::from(part);
        match dict.dictionary(Some(&key)) {
            Some(inner) => dict = inner,
            None => {
                eprintln!("settings-probe: prefkeys {path}: {part:?} is not a dictionary here");
                return;
            }
        }
    }
    let mut keys = CefStringList::new();
    dict.keys(Some(&mut keys));
    let mut count = 0;
    for key in keys.into_iter() {
        let name = CefString::from(key.as_str());
        count += 1;
        eprintln!(
            "settings-probe: prefkeys {path}.{key} type={:?}",
            dict.get_type(Some(&name))
        );
    }
    eprintln!("settings-probe: prefkeys {path} has {count} key(s)");
}
// --- end content settings ------------------------------------------------------------------------

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
        // Eight: three content/start-page settings, `statusbar.mode.style`, which is bru's own
        // rather than a name copied from qutebrowser, and the four `input.insert_mode.*` that
        // `focus.rs` reads. Raise this with the setting, never to make a failing build pass.
        // Ten: `start_page`, the two content settings, `statusbar.mode.style`, the four
        // `input.insert_mode.*` that `focus.rs` reads, and the two dictionaries —
        // `statusbar.mode.labels` and `url.searchengines`. Raise this with the setting, never to
        // make a failing build pass.
        // Twelve since `content.blocking.adblock.lists`, which is bru's first `Kind::List` and was
        // `adblock::DEFAULT_LISTS` before it was a setting.
        // **Forty**, the twelve above plus three workstreams that landed on them together:
        //
        // - **+12** — nine `tabs.*` and three `statusbar.*`, which make the two strips bru draws
        //   itself configurable.
        // - **+14** — the block fenced `unhardcoded`, every one of which was a `const` in the file
        //   that read it.
        // - **+14** — the block fenced `content settings`: eleven Chromium content settings and
        //   three Chromium preferences.
        //
        // - **+1** — `plugins.enabled`, the block fenced `lua runtime`.
        //
        // Raise this with the setting, never to make a failing build pass.
        assert_eq!(SETTINGS.len(), 58);
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
        // And every list's own, for the same reason.
        for def in SETTINGS {
            let Kind::List(shape) = def.kind else { continue };
            for value in shape.defaults {
                shape
                    .check(def.name, value)
                    .unwrap_or_else(|e| panic!("{}: bad default entry: {e}", def.name));
            }
        }
    }

    // --- tabs and statusbar --------------------------------------------------------------------
    /// The two strips' settings, checked at the store rather than at the strip: what the strip does
    /// with a value needs CEF and a window, what the store does with one does not, and this is the
    /// half that can be asserted without either.
    #[test]
    fn the_two_strips_settings_hold_only_what_they_can_draw() {
        let mut settings = Settings::default();

        // `tabs.position` takes two of qutebrowser's four, and the refusal names the two it takes.
        let error = settings.set("tabs.position", "left").unwrap_err();
        assert!(error.contains("top, bottom"), "{error}");
        assert!(settings.set("tabs.position", "bottom").is_ok());

        // `tabs.show`'s four are all built. `switching` is the one with a delay behind it.
        for value in ["always", "never", "multiple", "switching"] {
            assert!(settings.set("tabs.show", value).is_ok(), "{value}");
        }
        assert!(settings.set("tabs.show", "sometimes").is_err());

        // `statusbar.show`'s three, and `in-mode` spelled with the hyphen qutebrowser uses.
        for value in ["always", "never", "in-mode"] {
            assert!(settings.set("statusbar.show", value).is_ok(), "{value}");
        }
        assert!(settings.set("statusbar.show", "in_mode").is_err());

        // A format is free text, and an empty one is refused by `Kind::Text` — a tab drawn with no
        // string at all is a tab that cannot be told from its neighbour.
        assert!(settings.set("tabs.title.format", "{index}").is_ok());
        assert!(settings.set("tabs.title.format", "").is_err());

        // None of the twelve takes a URL pattern: they are all about a strip, and there is one
        // strip per window rather than one per site.
        for name in ["tabs.show", "statusbar.show", "tabs.background"] {
            assert!(
                settings
                    .set_scoped(name, "never", Some("*://*.example.com/*"))
                    .is_err(),
                "{name} accepted a pattern"
            );
        }

        // `config-cycle` with no values walks a Choice in order — which is what makes a key bound to
        // `config-cycle tabs.show always never` work, and the user's own qutebrowser config has
        // exactly that binding.
        settings.set("tabs.show", "always").unwrap();
        let applied = settings
            .cycle("tabs.show", &["always".into(), "never".into()], None)
            .unwrap();
        assert_eq!(applied.value, Value::Text("never".to_string()));
    }

    /// `statusbar.widgets` is a list of names, and a name it cannot draw is refused with the reason
    /// rather than accepted and skipped.
    #[test]
    fn the_status_line_refuses_a_widget_it_has_not_got() {
        let mut settings = Settings::default();

        // bru's own six are the defaults, in the order `bottom.html` lays the spans out.
        assert_eq!(
            list_of_in(&settings, "statusbar.widgets"),
            vec!["keypress", "search_match", "url", "scroll", "tabs", "download"]
        );

        // One qutebrowser has and bru does not, with the reason.
        let error = settings.list_add("statusbar.widgets", "history").unwrap_err();
        assert!(error.contains("bru has no history widget"), "{error}");
        assert!(error.contains("`H` and `L`"), "{error}");
        let error = settings.list_add("statusbar.widgets", "clock").unwrap_err();
        assert!(error.contains("once a second"), "{error}");

        // One nobody has: the list, not a reason.
        let error = settings.list_add("statusbar.widgets", "weather").unwrap_err();
        assert!(error.contains("keypress, search_match"), "{error}");

        // Reordering is remove-then-add, which is what an appending override buys — see
        // STATUSBAR_WIDGETS.
        settings.list_remove("statusbar.widgets", "url").unwrap();
        settings.list_add("statusbar.widgets", "url").unwrap();
        assert_eq!(
            list_of_in(&settings, "statusbar.widgets"),
            vec!["keypress", "search_match", "scroll", "tabs", "download", "url"]
        );

        // And a widget can be taken away for good, which is what makes the list a list rather than
        // an order.
        settings.list_remove("statusbar.widgets", "download").unwrap();
        assert!(!list_of_in(&settings, "statusbar.widgets").contains(&"download".to_string()));
    }

    /// `list_of` reads the live store, which these tests do not install into. This is the same
    /// question asked of a store in hand.
    fn list_of_in(settings: &Settings, name: &str) -> Vec<String> {
        match settings.get(name, None) {
            Ok(Some(Value::List(entries))) => entries,
            _ => Vec::new(),
        }
    }

    /// Every `tabs.*` and `statusbar.*` name bru refuses says why, and none of them is also
    /// implemented — the same pair of rules the file's first test applies to the whole table, aimed
    /// at the twelve names this workstream added a reason for.
    #[test]
    fn the_refused_strip_settings_name_a_reason_rather_than_a_date() {
        let mut settings = Settings::default();
        for name in [
            "tabs.last_close",
            "tabs.title.elide",
            "tabs.padding",
            "statusbar.padding",
            "tabs.indicator.width",
            "tabs.favicons.scale",
            "tabs.show_switching_delay",
            "tabs.mousewheel_switching",
            "tabs.close_mouse_button",
            "tabs.close_mouse_button_on_bar",
            "tabs.tabs_are_windows",
        ] {
            let error = settings.set_scoped(name, "1", None).unwrap_err();
            assert!(
                error.starts_with(&format!("bru does not implement {name}: ")),
                "{name}: {error}"
            );
            // "not yet", "later", "TODO" — a date rather than a reason. The whole point of REFUSED
            // is that the sentence still means something in a year.
            let why = error.to_lowercase();
            for excuse in ["not yet", "todo", "coming", "for now"] {
                assert!(!why.contains(excuse), "{name} is refused with an excuse: {error}");
            }
        }
    }
    // --- end tabs and statusbar ----------------------------------------------------------------

// --- content settings ----------------------------------------------------------------------------
    /// Every value a content setting can be given maps to one of Chromium's, and `content.mute`
    /// maps to the *opposite* one.
    ///
    /// The polarity is the assertion worth having: it is one `matches!` away from being wrong in a
    /// way that reads correctly everywhere else — `:set content.mute true` would print
    /// `content.mute = true`, the store would agree with itself, and the tab would make a noise.
    /// Measured against the browser on 2026-08-07 as well: with the inversion deliberately
    /// disabled, `:set -u *://127.0.0.1/* content.mute true` read back `allow`, and with it back,
    /// `block`.
    #[test]
    fn a_content_setting_maps_bru_words_to_chromium_words_and_mute_is_backwards() {
        use ContentSettingValues as V;
        let m = |kind, value| content_value(kind, &value);

        assert_eq!(m(ContentKind::Images, Value::Bool(true)), Some(V::ALLOW));
        assert_eq!(m(ContentKind::Images, Value::Bool(false)), Some(V::BLOCK));
        // Silence is what `true` asks for, so Chromium is told to block the sound.
        assert_eq!(m(ContentKind::Sound, Value::Bool(true)), Some(V::BLOCK));
        assert_eq!(m(ContentKind::Sound, Value::Bool(false)), Some(V::ALLOW));
        assert!(ContentKind::Sound.inverted());
        for kind in [ContentKind::Images, ContentKind::Geolocation, ContentKind::Popups] {
            assert!(!kind.inverted(), "{kind:?} must not be inverted");
        }

        // The three-valued ones arrive as text, and `ask` is a value in its own right.
        let text = |s: &str| Value::Text(s.to_string());
        assert_eq!(m(ContentKind::Geolocation, text("true")), Some(V::ALLOW));
        assert_eq!(m(ContentKind::Geolocation, text("false")), Some(V::BLOCK));
        assert_eq!(m(ContentKind::Geolocation, text("ask")), Some(V::ASK));
        // The clipboard says the same three things in its own words.
        assert_eq!(m(ContentKind::Clipboard, text("access")), Some(V::ALLOW));
        assert_eq!(m(ContentKind::Clipboard, text("none")), Some(V::BLOCK));
        assert_eq!(m(ContentKind::Clipboard, text("ask")), Some(V::ASK));

        // Nothing else is a content setting's value, and `apply` says so rather than guessing.
        assert_eq!(m(ContentKind::Images, text("maybe")), None);
        assert_eq!(m(ContentKind::Images, Value::List(Vec::new())), None);
    }

    /// The three-valued settings cycle through their three values with no arguments, which is what
    /// makes a key bound to a bare `config-cycle` on one of them useful.
    #[test]
    fn a_three_valued_content_setting_cycles_without_being_given_the_values() {
        let mut settings = Settings::default();
        let site = Some("*://*.example.com/*");
        // bru ships `ask`, so the first step is the one after it — which wraps to the front.
        let seen: Vec<String> = (0..4)
            .map(|_| {
                settings
                    .cycle("content.geolocation", &[], site)
                    .expect("geolocation cycles")
                    .value
                    .to_string()
            })
            .collect();
        assert_eq!(seen, ["true", "false", "ask", "true"]);

        // And a value outside the list is refused rather than stored, so `ask` cannot be spelled
        // `prompt` by accident.
        assert!(settings.set_scoped("content.geolocation", "prompt", site).is_err());
        // The clipboard's fourth value has no separate rule in Chromium and is not accepted; the
        // error names the three that differ. See the Def.
        let error = settings
            .set_scoped("content.javascript.clipboard", "access-paste", site)
            .unwrap_err();
        assert!(error.contains("none, access, ask"), "{error}");
    }

    /// A preference-backed setting is global, and says so instead of storing a pattern nothing
    /// reads. Chromium's preferences have no URL dimension at all — see `Backing::Preference`.
    #[test]
    fn a_preference_backed_setting_takes_no_url_pattern() {
        let mut settings = Settings::default();
        for name in [
            "content.headers.do_not_track",
            "content.hyperlink_auditing",
            "content.headers.accept_language",
        ] {
            let error = settings
                .set_scoped(name, "true", Some("*://*.example.com/*"))
                .expect_err("a preference cannot be scoped");
            assert!(error.contains("cannot be set per URL"), "{name}: {error}");
        }
        // Globally they are ordinary settings, and bru's defaults are its own rather than
        // Chromium's — measured, and `apply_at_startup` is what puts them in force.
        assert_eq!(
            settings.get("content.headers.do_not_track", None).unwrap(),
            Some(Value::Bool(true))
        );
        assert_eq!(
            settings.get("content.hyperlink_auditing", None).unwrap(),
            Some(Value::Bool(false))
        );
        assert_eq!(
            settings.get("content.headers.accept_language", None).unwrap(),
            Some(Value::Text("en-US,en".to_string()))
        );
    }
// --- end content settings ------------------------------------------------------------------------

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
        // Fourteen since `prompt.rs` brought `prompt` and `yesno`. The number moves with
        // `MODE_LABELS`, and it moving is the point: an override must not shrink the dict.
        assert_eq!(labels.len(), 14);
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
            "statusbar.mode.labels — 14 entries, 1 changed from bru's own"
        );
        assert_eq!(printed.lines().count(), 15, "a header and one line per pair");
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
        assert_eq!(json.matches(':').count(), 14, "one colon per pair");
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

    // -- lists, unsetting and the diff -----------------------------------------------------------

    /// **The append decision, asserted**, and it is [`DictShape`]'s merge decision in the other
    /// container. A replace would pass every other test in this file and show up only as a bru that
    /// blocks nothing because `config.lua` named one filter list.
    #[test]
    fn a_list_override_appends_to_brus_defaults_rather_than_replacing_them() {
        let mut settings = Settings::default();
        let name = "content.blocking.adblock.lists";
        // Nothing set: bru's own two, in the order they are written.
        assert_eq!(
            settings.list_vec(def(name).unwrap()),
            crate::adblock::DEFAULT_LISTS.to_vec()
        );

        settings
            .set_list(name, &["https://example.com/mine.txt".to_string()])
            .unwrap();
        let entries = settings.list_vec(def(name).unwrap());
        assert_eq!(entries.len(), 3, "the two bru ships are still there");
        assert_eq!(entries[2], "https://example.com/mine.txt", "and the new one is last");

        // An entry already there is not appended twice: two identical URLs would be fetched twice
        // into one file name.
        settings
            .set_list(name, &[crate::adblock::DEFAULT_LISTS[0].to_string()])
            .unwrap();
        assert_eq!(settings.list_vec(def(name).unwrap()).len(), 3);
        let error = settings
            .list_add(name, crate::adblock::DEFAULT_LISTS[0])
            .unwrap_err();
        assert!(error.contains("already in"), "{error}");

        // Removing is the only way to lose one of bru's own.
        settings.list_remove(name, crate::adblock::DEFAULT_LISTS[0]).unwrap();
        let entries = settings.list_vec(def(name).unwrap());
        assert_eq!(entries.len(), 2);
        assert!(!entries.contains(&crate::adblock::DEFAULT_LISTS[0].to_string()));
        let error = settings.list_remove(name, "https://nowhere/").unwrap_err();
        assert!(error.contains("is not in"), "{error}");
    }

    #[test]
    fn a_list_refuses_what_bru_could_not_fetch_and_is_not_a_scalar() {
        let mut settings = Settings::default();
        let name = "content.blocking.adblock.lists";
        let error = settings.list_add(name, "easylist.txt").unwrap_err();
        assert!(error.contains("http:// or https://"), "{error}");
        assert!(settings.list_add(name, "   ").is_err());
        // One bad entry leaves the whole list alone, exactly as one bad pair leaves a dict alone.
        assert!(settings
            .set_list(
                name,
                &["https://ok/a.txt".to_string(), "nope".to_string()]
            )
            .is_err());
        assert_eq!(settings.list_vec(def(name).unwrap()).len(), 2);

        // No text spelling, no cycling, and the two list commands refuse a setting that is not one.
        let error = settings.set(name, "https://x/a.txt").unwrap_err();
        assert!(error.contains("config-list-add"), "{error}");
        assert!(settings.cycle(name, &[], None).is_err());
        let error = settings.list_add("start_page", "https://x/").unwrap_err();
        assert!(error.contains("can only be used for lists"), "{error}");
        let error = settings.dict_add(name, "a", "b", false).unwrap_err();
        assert!(error.contains("can only be used for dicts"), "{error}");
    }

    #[test]
    fn printing_a_list_is_a_line_per_entry_and_says_which_are_not_brus() {
        let mut settings = Settings::default();
        let name = "content.blocking.adblock.lists";
        settings.list_add(name, "https://example.com/mine.txt").unwrap();
        let printed = settings.describe(name, None).unwrap();
        let mut lines = printed.lines();
        assert_eq!(
            lines.next().unwrap(),
            "content.blocking.adblock.lists — 3 entries, 1 added and 0 of bru's own removed"
        );
        assert_eq!(printed.lines().count(), 4, "a header and one line per entry");
        assert!(
            printed.contains("content.blocking.adblock.lists[2] = https://example.com/mine.txt   (added)"),
            "{printed}"
        );
        settings.list_remove(name, crate::adblock::DEFAULT_LISTS[0]).unwrap();
        assert!(
            settings
                .describe(name, None)
                .unwrap()
                .starts_with("content.blocking.adblock.lists — 2 entries, 1 added and 1 of bru's own removed"),
            "{printed}"
        );
    }

    /// `:config-unset` and `:config-clear` — the two commands that need bru to *have* a default.
    #[test]
    fn unsetting_puts_a_setting_back_to_brus_own() {
        let mut settings = Settings::default();
        settings.set("content.images", "false").unwrap();
        assert_eq!(settings.get("content.images", None).unwrap(), Some(Value::Bool(false)));

        let unset = settings.unset("content.images", None).unwrap();
        assert!(unset.was_customized);
        // Back to bru's own — which is the whole point, and is a place rather than an absence.
        assert_eq!(unset.value, Some(Value::Bool(true)));
        assert_eq!(settings.get("content.images", None).unwrap(), Some(Value::Bool(true)));
        assert_eq!(unset.describe(), "content.images = true (bru's own)");

        // Unsetting what was never set is an answer, not an error.
        let unset = settings.unset("content.images", None).unwrap();
        assert!(!unset.was_customized);
        assert!(unset.describe().contains("was already bru's own"), "{}", unset.describe());
        assert!(settings.unset("content.nonsense", None).is_err());

        // A per-URL value comes off on its own, leaving the global one standing.
        let site = Some("*://*.example.com/*");
        settings.set("content.images", "false").unwrap();
        settings.set_scoped("content.images", "true", site).unwrap();
        let unset = settings.unset("content.images", site).unwrap();
        assert!(unset.was_customized);
        assert_eq!(unset.value, Some(Value::Bool(false)), "the global value is what is left");
        assert_eq!(settings.get("content.images", None).unwrap(), Some(Value::Bool(false)));

        // A dict and a list go back whole.
        settings.dict_add("url.searchengines", "gh", "https://x/?q={}", false).unwrap();
        settings.list_add("content.blocking.adblock.lists", "https://x/l.txt").unwrap();
        settings.unset("url.searchengines", None).unwrap();
        settings.unset("content.blocking.adblock.lists", None).unwrap();
        assert_eq!(settings.search_engines().iter().count(), 9);
        assert_eq!(settings.list_vec(def("content.blocking.adblock.lists").unwrap()).len(), 2);
    }

    #[test]
    fn clearing_puts_everything_back_at_once() {
        let mut settings = Settings::default();
        settings.set("content.images", "false").unwrap();
        settings.set("start_page", "example.com").unwrap();
        settings
            .set_scoped("content.javascript.enabled", "false", Some("*://*.example.com/*"))
            .unwrap();
        settings.dict_add("url.searchengines", "gh", "https://x/?q={}", false).unwrap();

        let cleared = settings.clear();
        assert_eq!(cleared.len(), 4, "one per stored value, pattern included: {cleared:?}");
        assert!(cleared.iter().all(|one| one.was_customized));
        // Deterministic, so two runs report the same order.
        assert_eq!(
            cleared.iter().map(|one| one.def.name).collect::<Vec<_>>(),
            ["content.images", "content.javascript.enabled", "start_page", "url.searchengines"]
        );

        assert_eq!(settings.get("content.images", None).unwrap(), Some(Value::Bool(true)));
        // `start_page` is the one whose default is an absence — `app::HOME` stands again.
        assert_eq!(settings.get("start_page", None).unwrap(), None);
        assert_eq!(settings.search_engines().iter().count(), 9);
        assert!(settings.diff().is_empty());
    }

    /// **`:config-diff` prints Lua, and it is also the whole of what `:config-write-py` refused.**
    #[test]
    fn the_diff_is_the_lua_that_would_reproduce_it() {
        let mut settings = Settings::default();
        assert!(settings.diff().is_empty(), "a fresh bru differs from itself in nothing");

        settings.set("start_page", "example.com").unwrap();
        settings.set("content.images", "false").unwrap();
        settings.dict_add("url.searchengines", "gh", "https://x/?q={}", false).unwrap();
        settings.list_add("content.blocking.adblock.lists", "https://x/l.txt").unwrap();
        let diff = settings.diff();

        assert!(diff.contains(&r#"bru.set("start_page", "example.com")"#.to_string()), "{diff:?}");
        assert!(diff.contains(&r#"bru.set("content.images", "false")"#.to_string()), "{diff:?}");
        // Only the pair that is not bru's: pasting the whole table back would be pasting bru's own
        // defaults into a file that is supposed to be a patch.
        assert!(
            diff.contains(&r#"bru.set("url.searchengines", { ["gh"] = "https://x/?q={}" })"#.to_string()),
            "{diff:?}"
        );
        assert!(
            diff.contains(&r#"bru.set("content.blocking.adblock.lists", { "https://x/l.txt" })"#.to_string()),
            "{diff:?}"
        );

        // Setting something to the value bru already ships is not a difference.
        let mut settings = Settings::default();
        settings.set("content.images", "true").unwrap();
        assert!(settings.diff().is_empty(), "{:?}", settings.diff());

        // The two things `config.lua` cannot say are said as the commands that did them, marked.
        let mut settings = Settings::default();
        settings
            .set_scoped("content.javascript.enabled", "false", Some("*://*.example.com/*"))
            .unwrap();
        settings.dict_remove("url.searchengines", "hoog").unwrap();
        settings
            .list_remove("content.blocking.adblock.lists", crate::adblock::DEFAULT_LISTS[1])
            .unwrap();
        let diff = settings.diff();
        assert!(diff.iter().any(|line| line.contains(":set -u *://*.example.com/*")), "{diff:?}");
        assert!(diff.iter().any(|line| line.contains(":config-dict-remove url.searchengines hoog")), "{diff:?}");
        assert!(diff.iter().any(|line| line.contains(":config-list-remove")), "{diff:?}");
        for line in &diff {
            if line.starts_with("--") {
                continue;
            }
            assert!(line.starts_with("bru."), "a diff line is Lua or a comment: {line}");
        }
    }

    /// A value with a quote in it is a Lua string literal and not the end of one.
    #[test]
    fn a_diff_line_escapes_what_lua_would_choke_on() {
        assert_eq!(lua_string(r#"a"b\c"#), r#""a\"b\\c""#);
        let mut settings = Settings::default();
        settings.set("start_page", "a\"b").unwrap();
        assert_eq!(settings.diff(), [r#"bru.set("start_page", "a\"b")"#]);
    }

    // --- unhardcoded ---------------------------------------------------------------------------
    // -- the whole-number kind --------------------------------------------------------------------

    /// **A default this file changed is a behaviour it broke**, so this asserts every one of the
    /// fourteen against the constant it was lifted out of, by name.
    ///
    /// It is the most important test in this block and the least clever: the whole risk of turning
    /// a `const` into a setting is that the number moves on the way past, and nothing else here
    /// would notice. `crate::scroll::STEP` and the rest are still compiled in as the fallbacks
    /// their store-less processes use, so both sides of each of these is a real value and not a
    /// restatement of one literal.
    #[test]
    fn every_lifted_default_is_the_value_the_constant_had() {
        let settings = Settings::default();
        let number = |name: &str| match settings.get(name, None).unwrap() {
            Some(Value::Int(number)) => number,
            other => panic!("{name} is {other:?}, not a number"),
        };
        let text = |name: &str| match settings.get(name, None).unwrap() {
            Some(Value::Text(text)) => text,
            other => panic!("{name} is {other:?}, not text"),
        };
        let flag = |name: &str| settings.get(name, None).unwrap() == Some(Value::Bool(true));

        // hints.rs
        assert_eq!(text("hints.chars"), crate::hints::CHARS);
        assert_eq!(number("hints.min_chars"), 1);
        assert_eq!(text("hints.auto_follow"), "unique-match");
        assert!(!flag("hints.uppercase"));
        assert!(flag("hints.scatter"), "hint_strings was _hint_scattered and only that");
        // editor.rs and downloads.rs: unset, which is what leaves the environment and the XDG
        // answer standing. An absence is the default here, exactly as it is for `start_page`.
        assert_eq!(settings.get("editor.command", None).unwrap(), None);
        assert_eq!(settings.get("downloads.location.directory", None).unwrap(), None);
        assert!(flag("downloads.location.prompt"), "src/prompt.rs asks, and asked unconditionally");
        assert_eq!(number("downloads.remove_finished"), -1, "a finished row stayed until cd");
        // message.rs
        assert_eq!(number("messages.timeout"), 3000);
        assert_eq!(number("messages.limit"), 100);
        // exec.rs
        assert_eq!(number("zoom.default"), 100);
        let levels: Vec<u32> = ZOOM_LEVELS
            .defaults
            .iter()
            .filter_map(|entry| percent_of(entry))
            .collect();
        assert_eq!(
            levels,
            [25, 33, 50, 67, 75, 90, 100, 110, 125, 150, 175, 200, 250, 300, 400, 500]
        );
        // scroll.rs — the one the project exists for.
        assert_eq!(number("scroll.step_px"), i64::from(crate::scroll::STEP));
        assert_eq!(number("scroll.step_px"), 120);
        // And nothing in the block has been given a Chromium side it does not have. Safe to call
        // with no CEF behind it (trap 13) exactly because of that: `chromium_value` answers `None`
        // for these two backings before it reaches for a request context.
        for def in SETTINGS {
            if matches!(def.backing, Backing::Read | Backing::ScrollStep) {
                assert!(
                    chromium_value(def.name, "https://example.com/").is_none(),
                    "{} claims a Chromium value",
                    def.name
                );
                assert!(value_of(def.name).is_some(), "{} answers nothing", def.name);
            }
        }
    }

    /// `Kind::Int`: what it accepts, what it refuses, and what the refusal says.
    #[test]
    fn a_number_is_typed_and_bounded_and_says_what_the_bounds_were() {
        let mut settings = Settings::default();
        assert!(settings.set("messages.timeout", "500").is_ok());
        assert_eq!(settings.get("messages.timeout", None).unwrap(), Some(Value::Int(500)));

        // Not a number at all.
        let error = settings.set("messages.timeout", "soon").unwrap_err();
        assert!(error.contains("is not a whole number of milliseconds"), "{error}");
        // A number outside the range, with the range and the sentinel in the message.
        let error = settings.set("messages.timeout", "-1").unwrap_err();
        assert!(error.contains("outside 0..86400000 milliseconds"), "{error}");
        assert!(error.contains("(0 is never clear)"), "{error}");
        // The sentinel is the *end* of the range where there is one, so -1 is legal there.
        assert!(settings.set("downloads.remove_finished", "-1").is_ok());
        assert!(settings.set("downloads.remove_finished", "-2").is_err());
        // `hints.min_chars` has a ceiling qutebrowser has not, because the number is an exponent.
        assert!(settings.set("hints.min_chars", "5").is_ok());
        let error = settings.set("hints.min_chars", "20").unwrap_err();
        assert!(error.contains("outside 1..5 characters"), "{error}");
        assert!(settings.set("hints.min_chars", "0").is_err());
        // A percent takes qutebrowser's own spelling as well as bru's, because `:zoom 150%` does.
        assert!(settings.set("zoom.default", "150%").is_ok());
        assert_eq!(settings.get("zoom.default", None).unwrap(), Some(Value::Int(150)));
        assert!(settings.set("zoom.default", "150").is_ok());
        // ...and only where the unit is percent: a timeout with a % in it is a typo.
        assert!(settings.set("messages.timeout", "500%").is_err());

        // What it prints is the number and nothing else, which is the spelling `:set` takes back.
        assert_eq!(
            settings.describe("scroll.step_px", None).unwrap(),
            "scroll.step_px = 120"
        );
        settings.set("scroll.step_px", "200").unwrap();
        assert_eq!(
            settings.set_scoped("scroll.step_px", "240", None).unwrap().describe(),
            "scroll.step_px = 240"
        );
        // And it is global-only, like every setting in this block: Chromium scopes content
        // settings, and none of these is one.
        assert!(settings
            .set_scoped("scroll.step_px", "240", Some("*://*.example.com/*"))
            .is_err());
    }

    /// **What `config-cycle` with no values does for a number**: it refuses, and names the values
    /// to type. A `Bool` has two and a `Choice` has its list; a number has neither.
    #[test]
    fn config_cycle_needs_the_values_for_a_number_and_says_so() {
        let mut settings = Settings::default();
        let error = settings.cycle("messages.timeout", &[], None).unwrap_err();
        assert!(error.contains("there is no next one to step to"), "{error}");
        assert!(error.contains(":config-cycle messages.timeout 0 86400000"), "{error}");

        // Spelled out, it walks them and wraps like any other kind.
        let values = vec!["1000".to_string(), "3000".to_string()];
        assert_eq!(
            settings.cycle("messages.timeout", &values, None).unwrap().value,
            Value::Int(1000),
            "3000 is the default and is in the list at 1, so the next is 0"
        );
        assert_eq!(
            settings.cycle("messages.timeout", &values, None).unwrap().value,
            Value::Int(3000)
        );
        // A value the setting could not hold is refused before anything is stored.
        assert!(settings
            .cycle("hints.min_chars", &["1".to_string(), "99".to_string()], None)
            .is_err());
        assert_eq!(settings.get("hints.min_chars", None).unwrap(), Some(Value::Int(1)));

        // The four values of `hints.auto_follow` cycle with no values, because a Choice can.
        assert_eq!(
            settings.cycle("hints.auto_follow", &[], None).unwrap().value,
            Value::Text("full-match".to_string()),
            "unique-match is at 1 in the list, so the next is at 2"
        );
    }

    /// `Kind::Chars`: the three things a one-character or repeating `hints.chars` would do.
    #[test]
    fn hint_chars_refuses_what_would_divide_by_zero_or_label_twice() {
        let mut settings = Settings::default();
        assert!(settings.set("hints.chars", "aoeuidnths").is_ok());
        assert_eq!(
            settings.get("hints.chars", None).unwrap(),
            Some(Value::Text("aoeuidnths".to_string()))
        );

        // One character: `hint_strings` divides the label space by `len - 1`.
        let error = settings.set("hints.chars", "a").unwrap_err();
        assert!(error.contains("at least two"), "{error}");
        // A character twice: two elements with one label.
        let error = settings.set("hints.chars", "asdfa").unwrap_err();
        assert!(error.contains("uses a character twice"), "{error}");
        // A space: not typeable as part of a label.
        assert!(settings.set("hints.chars", "as df").is_err());
        assert!(settings.set("hints.chars", "").is_err());
        // Non-ASCII is fine — it is a character set, not an alphabet.
        assert!(settings.set("hints.chars", "фыва").is_ok());
    }

    /// `zoom.levels` is a list whose entries are percentages, in either spelling, and whose order
    /// does not matter because its reader sorts.
    #[test]
    fn zoom_levels_takes_both_spellings_and_refuses_what_is_not_a_level() {
        let mut settings = Settings::default();
        let name = "zoom.levels";
        assert_eq!(settings.list_vec(def(name).unwrap()).len(), 16);

        assert!(settings.list_add(name, "133%").is_ok());
        assert!(settings.list_add(name, "133").is_ok(), "bru's own spelling is taken too");
        let error = settings.list_add(name, "big").unwrap_err();
        assert!(error.contains("not a zoom level"), "{error}");
        assert!(settings.list_add(name, "0%").is_err());
        assert!(settings.list_add(name, "20000%").is_err());
        // Removing one of bru's own is the only way to lose it, as for every bru list.
        settings.list_remove(name, "25%").unwrap();
        assert_eq!(settings.list_vec(def(name).unwrap()).len(), 17);

        // Both spellings read back as the same number, which is what stops `133` and `133%` from
        // becoming two levels once `exec::zoom_levels` has deduplicated them.
        assert_eq!(percent_of("133%"), Some(133));
        assert_eq!(percent_of(" 133 "), Some(133));
        assert_eq!(percent_of("x"), None);
    }
    // --- end unhardcoded -------------------------------------------------------------------------

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
