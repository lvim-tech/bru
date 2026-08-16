//! Hint mode — `f`, `F`, and the fourteen `;`-prefixed bindings around them.
//!
//! A behavioural port of qutebrowser 3.7.0's `browser/hints.py` (label generation, filtering,
//! groups, targets, `--rapid`, `--first`) and `javascript/webelem.js` (which elements are hintable,
//! see `chrome/hints.js`).
//!
//! Three rules shape this file, and none of them is a preference:
//!
//! - **The follow is a real click.** `host.send_mouse_click_event` at the element's centre, on
//!   Chromium's own input path — the same reasoning as the wheel rule that bru exists for. A
//!   synthetic `element.click()` skips hover, focus and every handler that checks `isTrusted`;
//!   a real click does not.
//! - **Keys are matched in Rust.** [`BindingTrie`] was made generic over its value so that hint mode
//!   could put labels in it (`modeparsers.py:135`, `HintKeyParser.update_bindings`). The page draws
//!   what it is told and decides nothing; a keystroke never crosses into it to be matched.
//! - **The page is not trusted.** `chrome/hints.js` answers through the message router, which every
//!   page can reach. The answer is accepted only while a session bru itself started is open, only
//!   from that session's browser, and only carrying the token that session minted.

use cef::*;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use crate::bindings::{BindingTrie, Key, KeyInfo, Match, NamedKey};
// The one decoder for what a page wrote, beside the escaper for what is written to it — see
// `ipc::percent_decode` for why it is not a copy per module any more.
use crate::ipc::percent_decode;
use crate::modes::Mode;
use crate::state::BruState;
use crate::tabs::SharedState;

/// `hints.chars`, configdata.yml:1723 — the home row, and qutebrowser's own default.
///
// --- unhardcoded -------------------------------------------------------------------------------
/// **It is `hints.chars`'s default now**, and [`chars`] is what generates labels from. The value is
/// unchanged; what has changed is the sentence that used to be here, which said changing it was "a
/// config question, not a code one" while there was no config to ask.
// --- end unhardcoded ---------------------------------------------------------------------------
pub const CHARS: &str = "asdfghjkl";

/// `hints.min_chars`, configdata.yml:1752. `hints.min_chars`'s default; [`min_chars`] reads it.
const MIN_CHARS: usize = 1;

// --- unhardcoded -------------------------------------------------------------------------------
/// `hints.auto_follow`, configdata.yml:1673 — when a hint is followed without `<Return>`.
///
/// All four of qutebrowser's values. bru had `UniqueMatch` compiled in with a doc comment
/// explaining why the other three were absent; the reason given was that "bru ships no
/// configuration", which DESIGN.md's correction of 2026-08-06 says is not true of bru.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AutoFollow {
    /// Follow as soon as one label is showing, **even before a key is pressed**. `hints.py:657`
    /// calls `_handle_auto_follow` once from `_start_cb` "to make auto_follow == 'always' work", so
    /// `f` on a page with one link follows it; bru fires it from `on_collected` for the same reason.
    Always,
    /// Follow as soon as one label is showing *and something has been typed* —
    /// `bool(keystr or filterstr)`. bru's default and qutebrowser's.
    UniqueMatch,
    /// Follow when the whole label has been typed. **This is the rule bru followed on before
    /// `auto_follow` existed as a function**, and it is not qutebrowser's default.
    FullMatch,
    /// Never. `<Return>` — `hint-follow` — is what follows, and this value is the only one under
    /// which that binding has a job.
    Never,
}

impl AutoFollow {
    /// The setting, read once when a session starts. The name is `settings.rs`'s own spelling.
    fn current() -> AutoFollow {
        match crate::settings::choice_of("hints.auto_follow") {
            "always" => AutoFollow::Always,
            "full-match" => AutoFollow::FullMatch,
            "never" => AutoFollow::Never,
            // Including the empty answer a process with no settings store gives, which is a
            // renderer or a unit test: bru's own default is what stands there.
            _ => AutoFollow::UniqueMatch,
        }
    }
}

/// `hints.chars`, or [`CHARS`] in a process with no settings store behind it.
fn chars() -> Vec<char> {
    crate::settings::text_of("hints.chars")
        .unwrap_or_else(|| CHARS.to_string())
        .chars()
        .collect()
}

/// `hints.min_chars`, or [`MIN_CHARS`] in a process with no settings store behind it.
///
/// `Kind::Int`'s range on it is 1..5 and the store cannot hold anything else, so the ceiling here
/// is not a second opinion about the range — it is what keeps a `0` from a store-less process, and
/// anything else that is not a length, out of an exponent.
fn min_chars() -> usize {
    match crate::settings::int_of("hints.min_chars") {
        length if length >= 1 => (length as usize).min(5),
        _ => MIN_CHARS,
    }
}
// --- end unhardcoded ---------------------------------------------------------------------------

/// The page half, injected into the tab's main frame. Not served over `bru://`: it has to run in
/// the page's own world to see the page's elements.
const HINTS_JS: &str = include_str!("../chrome/hints.js");

/// Which elements get a label. The keys of `hints.selectors`, configdata.yml:1803.
///
/// The selector lists themselves live in `chrome/hints.js`, because that is where they are used;
/// this is only the name that crosses over. A group is **not** the `all` list filtered afterwards —
/// `links` is `a[href], area[href], link[href], [role="link"][href]`, a different query — and
/// `chrome/hints.js` refuses a name it does not know rather than answering with `all`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Group {
    All,
    Links,
    Images,
    Media,
    Url,
    Inputs,
}

impl Group {
    /// The name `chrome/hints.js` keys `SELECTORS` by. Also what the parser accepts.
    pub fn name(self) -> &'static str {
        match self {
            Group::All => "all",
            Group::Links => "links",
            Group::Images => "images",
            Group::Media => "media",
            Group::Url => "url",
            Group::Inputs => "inputs",
        }
    }
}

/// What following a hint does. qutebrowser's `hints.Target`, minus the five that belong to commands
/// bru has not built (`run`, `spawn`, `userscript`, `delete`, `right-click`).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Target {
    /// `normal` / `current` (`f`) — click the element where it sits, on Chromium's own input path.
    Normal,
    /// `tab` / `tab-bg` (`F`, `;b`) — the element's URL in a background tab.
    ///
    /// `tab` honours `tabs.background`, whose default is **true** (configdata.yml:2217), so the two
    /// spellings land in the same place until bru has a setting for it.
    TabBg,
    /// `tab-fg` (`;f`) — the element's URL in a tab that is switched to.
    TabFg,
    /// `window` (`wf`) — the element's URL in a new top-level window, as `:open -w` and `U` already
    /// make one. It was a foreground tab until `src/window.rs` grew a second window to open.
    Window,
    /// `hover` (`;h`) — move the mouse onto the element and leave it there.
    Hover,
    /// `yank` (`;y`) — the URL to the clipboard. **Needs [`Clipboard`], which nobody installs yet.**
    Yank,
    /// `yank-primary` (`;Y`) — the URL to the primary selection. Same hole.
    YankPrimary,
    /// `download` (`;d`) — **needs [`Downloads`], which nobody installs yet.**
    Download,
    /// `fill` (`;o`, `;O`) — put the command line text into `:`, with `{hint-url}` replaced.
    Fill(String),
}

impl Target {
    /// `hints.py:start`'s `no_rapid_targets`, exactly.
    ///
    /// qutebrowser refuses `--rapid` with `tab-fg` (it opens a tab and switches to it), `fill` (it
    /// leaves hint mode by entering command mode), `right-click` and `delete`. `window` is **not**
    /// in that list, and bru no longer adds it: it did while `window` was a foreground tab, which
    /// is the same objection as `tab-fg`'s, and that stopped being true when [`Target::Window`]
    /// started opening a window. `;R` (`hint --rapid links window`) is live because of this line.
    fn allows_rapid(&self) -> bool {
        !matches!(self, Target::TabFg | Target::Fill(_))
    }

    /// Whether following needs the element's URL out of the page. The other targets act on the
    /// element's position and never leave Rust.
    fn needs_url(&self) -> bool {
        !matches!(self, Target::Normal | Target::Hover)
    }

    /// `HINT_TEXTS`, hints.py:640 — what the status line would say. Printed rather than shown; the
    /// status bar has no field for it yet.
    fn describe(&self) -> &'static str {
        match self {
            Target::Normal => "Follow hint",
            Target::TabBg => "Follow hint in background tab",
            Target::TabFg => "Follow hint in foreground tab",
            Target::Window => "Follow hint in new window",
            Target::Hover => "Hover over a hint",
            Target::Yank => "Yank hint to clipboard",
            Target::YankPrimary => "Yank hint to primary selection",
            Target::Download => "Download hint",
            Target::Fill(_) => "Set hint in commandline",
        }
    }
}

/// One run of hint mode: from `f` to a follow, an `<Escape>`, or a page that reported nothing.
///
/// With `--rapid` a follow does **not** end it: the labels stay drawn, the typed chain is reset,
/// and the next label follows another element. That is the whole difference between `;b` and `;r`.
struct Session {
    /// The tab this belongs to. A hint session survives no tab switch — checked on every answer
    /// and on every key.
    ///
    /// Still here now that the sessions are keyed by *window*, and it is not redundant: a window has
    /// many tabs and a session belongs to exactly one of them. The window says which session a key
    /// is about; this says whether the key reached the tab the labels are drawn on.
    browser_id: i32,
    group: Group,
    target: Target,
    /// `--rapid`: the session outlives a follow.
    rapid: bool,
    /// `--first`: follow the first element as soon as the page answers, with no keypress at all.
    first: bool,
    /// `hints.py`'s `first_run` — false from the second follow of a rapid session on. The clipboard
    /// workstream needs it: a rapid yank appends to what is already there rather than replacing it.
    first_run: bool,
    /// Minted here, handed to the injected script, and required back on every answer.
    token: String,
    /// Click points in view coordinates, in the order the page reported them.
    points: Vec<(i32, i32)>,
    /// Labels, in the same order as `points`.
    labels: Vec<String>,
    /// label → index into `points`. The generic [`BindingTrie`] doing hint mode's half of its job.
    trie: BindingTrie<usize>,
    /// The `hint:` bindings, configdata.yml:3884 — `<Ctrl-F>`, `<Ctrl-R>`, `<Ctrl-B>`, `<Return>`.
    ///
    /// `HintKeyParser.handle` (modeparsers.py:196) tries the **command** parser before the labels,
    /// which is the only reason those four keys can do anything while hint mode is on. Snapshotted
    /// per session out of the live table, so a `config.lua` that rebinds them is honoured.
    commands: BindingTrie<crate::commands::Command>,
    /// The chain typed towards one of those bindings, kept apart from the label chain the way
    /// qutebrowser keeps two parsers apart.
    command_sequence: Vec<KeyInfo>,
    /// The hint characters typed so far.
    sequence: Vec<KeyInfo>,
    // --- unhardcoded -------------------------------------------------------------------------
    /// `hints.auto_follow`, snapshotted when the session started.
    ///
    /// **Per session rather than per keypress**, which is the same decision `commands` above
    /// carries: a page that is already labelled was labelled under one rule, and changing the rule
    /// under a chain that has been half typed would follow something the user did not aim at. It
    /// also keeps the settings mutex off the hint key path, which is the nearest thing to the
    /// scroll key path in this file.
    auto_follow: AutoFollow,
    // --- end unhardcoded -----------------------------------------------------------------------
    /// When `start` injected the script, so collection can be timed rather than asserted.
    started: std::time::Instant,
}

/// Every open hint session, keyed by the window it belongs to.
///
/// **One per window, because that is where qutebrowser puts it.** `HintManager` is constructed
/// inside `modeman.init(win_id)` and registered `objreg.register('hintmanager', …, scope='window')`
/// (`keyinput/modeman.py:76-79`); every `hint` command is `@cmdutils.register(instance='hintmanager',
/// scope='window')` (`hints.py:660`). The comment at `hints.py:1025` still says "we have one
/// HintManager per tab" and has been wrong since that registration moved — read the registration,
/// not the comment.
///
/// bru had one `Mutex<Option<Session>>` for the process. That was invisible while everything else
/// was process-wide too, and stopped being so when the mode became one window's: pressing `f` in a
/// second window evicted the first window's session and left the first window sitting in hint mode
/// with nothing behind it.
///
/// Keyed by window and **not** by browser on purpose. A window hints one tab at a time — one
/// `HintContext` per `HintManager` — so `f` in another tab of the same window replaces the session
/// rather than adding a second. [`Session::browser_id`] is what tells those two tabs apart.
fn sessions() -> &'static Mutex<HashMap<u32, Session>> {
    static SESSIONS: LazyLock<Mutex<HashMap<u32, Session>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    &SESSIONS
}

/// The window a browser is in, or `None` for a browser bru never put in one.
///
/// Every entry point into this file has a `Browser` and has to turn it into the key of the map
/// above. It is a linear walk of the window list (`BruState::window_of_browser`), which is why
/// [`handle_key`] does not call it until it already knows a session exists somewhere.
fn window_of(state: &SharedState, browser: &mut Browser) -> Option<u32> {
    let id = browser.identifier();
    state
        .lock()
        .expect("state mutex poisoned")
        .window_of_browser(id)
}

/// `BRU_DEBUG_HINTS=1` traces the labels and the keys that reach hint mode. Off by default: the
/// label list is one line per `f`, which is one line too many in a real session.
fn debug(message: &str) {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("BRU_DEBUG_HINTS").is_some()) {
        eprintln!("bru[hints]: {message}");
    }
}

// ------------------------------------------------------------------------------------------------
// Label generation — hints.py `_hint_strings` and below
// ------------------------------------------------------------------------------------------------

/// `utils.ceil_log`: `max(1, ceil(log(number, base)))`, in integer arithmetic so that the answer
/// does not depend on a float's last bit.
fn ceil_log(number: usize, base: usize) -> usize {
    assert!(number >= 1 && base >= 2, "math domain error");
    let mut result = 1;
    let mut accum = base;
    while accum < number {
        result += 1;
        accum *= base;
    }
    result
}

/// `_number_to_hint_str`: 8 becomes `jk`, padded to `digits` with the first character.
fn number_to_hint_str(mut number: usize, chars: &[char], digits: usize) -> String {
    let base = chars.len();
    let mut out: Vec<char> = Vec::new();
    loop {
        let remainder = number % base;
        out.insert(0, chars[remainder]);
        number -= remainder;
        number /= base;
        if number == 0 {
            break;
        }
    }
    while out.len() < digits {
        out.insert(0, chars[0]);
    }
    out.into_iter().collect()
}

/// `_shuffle_hints`: spread labels starting with the same character evenly through the list, so
/// that neighbouring links do not all begin with `a`.
fn shuffle_hints(hints: Vec<String>, length: usize) -> Vec<String> {
    let mut buckets: Vec<Vec<String>> = vec![Vec::new(); length];
    for (i, hint) in hints.into_iter().enumerate() {
        let bucket = i % length;
        buckets[bucket].push(hint);
    }
    buckets.into_iter().flatten().collect()
}

// --- unhardcoded -------------------------------------------------------------------------------
/// `_hint_strings`: the labels for `count` elements, under whatever `hints.chars`,
/// `hints.min_chars` and `hints.scatter` are set to.
///
/// The settings are read here, once per `f`, rather than inside the two generators — the two of
/// them plus the caller would otherwise take three locks for one answer.
pub fn hint_strings(count: usize) -> Vec<String> {
    let chars = chars();
    let min_chars = min_chars();
    // `hints.scatter`, configdata.yml:1795. True is Vimium's and bru's default and was bru's only
    // behaviour; false is dwb's, which is `_hint_linear`.
    if crate::settings::is_on("hints.scatter") || crate::settings::def("hints.scatter").is_none() {
        hint_scattered(count, &chars, min_chars)
    } else {
        hint_linear(count, &chars, min_chars)
    }
}

/// `_hint_linear` (hints.py:495) — constant-length labels in the elements' own order, the way dwb
/// draws them. What `hints.scatter false` selects.
///
/// It is the shorter of the two by a long way, and the difference is the whole of what the setting
/// buys: no variable length, so nothing is a prefix of anything, and no shuffle, so the first link
/// on the page is `aa` and the second is `as`.
fn hint_linear(count: usize, chars: &[char], min_chars: usize) -> Vec<String> {
    if count == 0 {
        return Vec::new();
    }
    let needed = min_chars.max(ceil_log(count, chars.len()));
    (0..count)
        .map(|i| number_to_hint_str(i, chars, needed))
        .collect()
}
// --- end unhardcoded ---------------------------------------------------------------------------

/// `_hint_scattered`, which is what `hints.scatter` (default true) selects.
///
/// Variable-length labels, Vimium-style: as many links as will fit get a label one character
/// shorter than the worst case. The short ones are never a prefix of a long one, because the long
/// ones start at `short_count * len(chars)` — which is what makes an exact match final and lets a
/// hint follow the moment it is complete.
fn hint_scattered(count: usize, chars: &[char], min_chars: usize) -> Vec<String> {
    if count == 0 {
        return Vec::new();
    }
    let needed = min_chars.max(ceil_log(count, chars.len()));

    let short_count = if needed > min_chars && needed > 1 {
        let total_space = chars.len().pow(needed as u32);
        total_space.saturating_sub(count) / (chars.len() - 1)
    } else {
        0
    };
    let long_count = count - short_count;

    let mut strings = Vec::with_capacity(count);
    if needed > 1 {
        for i in 0..short_count {
            strings.push(number_to_hint_str(i, chars, needed - 1));
        }
    }
    let start = short_count * chars.len();
    for i in start..start + long_count {
        strings.push(number_to_hint_str(i, chars, needed));
    }

    shuffle_hints(strings, chars.len())
}

// ------------------------------------------------------------------------------------------------
// Starting and ending a session
// ------------------------------------------------------------------------------------------------

/// `hint [--rapid] [--first] <group> <target>` — start hint mode on `browser`.
///
/// Returns after asking the page for its elements; hint mode is not entered until the page answers,
/// which is what stops `f` on a blank tab from trapping the keyboard in a mode with nothing in it.
pub fn start(
    state: &SharedState,
    browser: &mut Browser,
    group: Group,
    target: Target,
    rapid: bool,
    first: bool,
) {
    // hints.py:1030 raises CommandError here rather than doing something surprising, and so does
    // this. `;R` is the only default binding that lands on it.
    if rapid && !target.allows_rapid() {
        eprintln!("bru: rapid hinting makes no sense with target {}", target.describe());
        return;
    }
    let Some(window) = window_of(state, browser) else {
        eprintln!("bru: hint: this tab is in no window");
        return;
    };
    let Some(frame) = browser.main_frame() else {
        return;
    };
    let token = mint_token();
    let commands = hint_mode_bindings(state);
    let live = state.lock().expect("state mutex poisoned").window_ids();

    // Whatever session **this window** had is replaced, and no other window's is touched. Its labels
    // come off this page in `collect`, which begins with the script's own `clear()`; on any other
    // page they went when it was navigated.
    {
        let mut guard = sessions().lock().expect("hint sessions mutex poisoned");
        // A closed window takes its mode with it (`BruState::forget_window`) and nothing tells this
        // file, so the map is swept here instead of hooking `on_before_close` in `keys.rs` for a few
        // hundred bytes. Nothing depends on the sweep for correctness — a session whose window is
        // gone can never be reached by a key, because `window_of_browser` cannot name that window
        // again — so this is only about not holding a page's coordinates for the life of the process.
        guard.retain(|window, _| live.contains(window));
        guard.insert(
            window,
            Session {
                browser_id: browser.identifier(),
                group,
                target,
                rapid,
                first,
                first_run: true,
                token: token.clone(),
                points: Vec::new(),
                labels: Vec::new(),
                trie: BindingTrie::new(),
                commands,
                command_sequence: Vec::new(),
                sequence: Vec::new(),
                // --- unhardcoded -----------------------------------------------------------
                // Once, here, beside the bindings snapshot — see the field.
                auto_follow: AutoFollow::current(),
                // --- end unhardcoded -------------------------------------------------------
                started: std::time::Instant::now(),
            },
        );
    }

    // The script is injected on every `f` rather than once per page load: a navigation throws the
    // world away, and there is no cheap way to know from here whether this one still has it.
    let code = format!(
        "{HINTS_JS}\nwindow.__bru_hints.collect(\"{token}\",\"{}\");",
        group.name()
    );
    frame.execute_java_script(Some(&CefString::from(code.as_str())), None, 0);

    // Nothing enters hint mode here — `on_collected` does, once there is something to hint. `f` on
    // a page with no links must not trap the keyboard in a mode with nothing in it.
}

/// The `hint:` mode's own bindings, parsed into a trie, out of the table the browser is running on.
///
/// Taken once per session rather than per keystroke: `bindings_snapshot` clones the whole table,
/// and a keypress must not do that. There are four of them, so the trie costs nothing to keep.
fn hint_mode_bindings(state: &SharedState) -> BindingTrie<crate::commands::Command> {
    let mut trie = BindingTrie::new();
    let Some(bindings) = state
        .lock()
        .expect("state mutex poisoned")
        .bindings_snapshot()
    else {
        return trie;
    };
    for (mode, keys, command) in bindings.all() {
        if mode != Mode::Hint {
            continue;
        }
        // `<Escape>` is handled before this trie is consulted — see `handle_key`.
        match (crate::bindings::parse_key_sequence(&keys), crate::commands::parse(&command)) {
            (Ok(sequence), Ok(command)) => {
                trie.insert(&sequence, command);
            }
            _ => eprintln!("bru: hint-mode binding {keys:?} -> {command:?} was dropped"),
        }
    }
    trie
}

/// `<Escape>` in hint mode, and every path that ends a session: the one belonging to the window
/// `browser` is in.
pub fn cancel(state: &SharedState, browser: &mut Browser) {
    let Some(window) = window_of(state, browser) else {
        return;
    };
    cancel_in(state, window, browser);
}

/// The same with the window already in hand, which is what [`handle_key`] has by the time it gets
/// to `<Escape>`.
fn cancel_in(state: &SharedState, window: u32, browser: &mut Browser) {
    let had = sessions()
        .lock()
        .expect("hint sessions mutex poisoned")
        .remove(&window)
        .is_some();
    if had {
        clear_labels(browser);
    }
    leave_mode(state, window);
}

/// Take the labels off the page. Sent as its own script so that it runs even when the session is
/// already gone from Rust's side.
fn clear_labels(browser: &mut Browser) {
    let Some(frame) = browser.main_frame() else {
        return;
    };
    frame.execute_java_script(
        Some(&CefString::from(
            "window.__bru_hints && window.__bru_hints.clear();",
        )),
        None,
        0,
    );
}

/// Leave hint mode **in one window**, and tell that window's bar.
///
/// Named rather than current throughout, because the whole point of `;R` is that the key ending a
/// session can arrive at a different window from the one holding it.
fn leave_mode(state: &SharedState, window: u32) {
    let now = {
        let mut guard = state.lock().expect("state mutex poisoned");
        if guard.mode_in(window) != Mode::Hint {
            return;
        }
        guard.leave_mode_in(window);
        guard.mode_in(window)
    };
    crate::ipc::set_mode_for(window, now.name().to_string());
    crate::ipc::set_keystring_for(window, String::new());
}

/// A token no page can guess, so that the only `hints` answer bru believes is the one it asked for.
///
/// **It is a nonce and not a key, and that argument still holds — but it is not the only guard by
/// accident, so it should not be the weak one by construction either.** Two other checks stand
/// beside it: a session bru itself opened must be live in this window, and the query must come from
/// that session's browser. To forge an answer a page must already *be* the page bru is hinting, and
/// then produce a value it cannot read.
///
/// What that argument does not cover is prediction. `RandomState` is designed to stop hash-flooding,
/// not to resist an adversary, and a nanosecond clock is low-entropy and observable from the page
/// itself. Guessing wrong costs nothing — the query simply fails, and nothing rate-limits the
/// attempts — so a value that merely *looks* like 128 bits is the wrong shape for the one check
/// that stands between a page and steering its own hint resolution.
///
/// Sixteen bytes from `/dev/urandom`, then. bru has no RNG dependency and does not need one for a
/// single read on a path that runs once per hint session; the old construction stays as the
/// fallback for the case where the device cannot be opened, because a browser that refuses to hint
/// because `/dev/urandom` is missing is worse than one that hints with a weaker nonce.
///
/// Shared with `caret.rs`, which asks the same question of the same kind of page answer. It was
/// copied there and the copies were identical; a security primitive in two places is a primitive
/// that drifts.
pub(crate) fn mint_token() -> String {
    use std::io::Read;
    let mut buf = [0u8; 16];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut buf))
        .is_ok()
    {
        return buf.iter().map(|byte| format!("{byte:02x}")).collect();
    }
    use std::hash::{BuildHasher, Hasher, RandomState};
    let a = RandomState::new().build_hasher().finish();
    let b = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!("{a:016x}{b:016x}")
}

// ------------------------------------------------------------------------------------------------
// What the page answers
// ------------------------------------------------------------------------------------------------

/// A `{"type":"hints"}` query from a web page. Called by `src/ipc.rs` **before** its `bru://` check,
/// and it is the only thing a page may say to bru.
///
/// Returns false for anything that is not an answer to a session bru started, which `ipc.rs` turns
/// into a failed query. Three things have to line up: a session is open **in the window this page is
/// in**, it belongs to the browser the query came from, and it carries the token bru minted for it.
///
/// The window is the new part and it costs nothing in safety: the browser check alone was already
/// enough to refuse another tab's page. It is what says *which* session the answer is for, now that
/// two can be open at once.
pub fn on_page_query(browser: Option<&Browser>, request: &str) -> bool {
    let Some(browser) = browser else {
        return false;
    };
    let id = browser.identifier();
    // **The raw field is taken here; it is decoded after the checks below pass.** `data` is
    // page-controlled bytes, and `percent_decode` is a parser running on hostile input — so it runs
    // only for a query bru has already decided to believe. It is panic-free today (see its own
    // comment on the bound), and this ordering is what keeps a future edit to it from becoming a
    // page-triggered abort of the whole process rather than a refused query.
    let (Some(token), Some(kind), Some(raw)) = (
        field(request, "token"),
        field(request, "kind"),
        field(request, "data"),
    ) else {
        return false;
    };

    let Some(state) = BruState::instance() else {
        return false;
    };
    let Some(window) = state
        .lock()
        .expect("state mutex poisoned")
        .window_of_browser(id)
    else {
        return false;
    };

    {
        let guard = sessions().lock().expect("hint sessions mutex poisoned");
        let Some(open) = guard.get(&window) else {
            return false;
        };
        if open.browser_id != id || open.token != token {
            return false;
        }
    }

    // Believed now, and only now decoded.
    let data = percent_decode(&raw);
    let mut browser = browser.clone();

    match kind.as_str() {
        "elems" => on_collected(&state, window, &mut browser, &data),
        "href" => on_href(&state, window, &mut browser, &data),
        _ => return false,
    }
    true
}

/// The page has reported its hintable elements. Generate the labels, draw them, enter hint mode.
fn on_collected(state: &SharedState, window: u32, browser: &mut Browser, data: &str) {
    let points: Vec<(i32, i32)> = data
        .split('|')
        .filter(|part| !part.is_empty())
        .filter_map(|part| {
            let mut fields = part.split(',');
            let x = fields.next()?.parse().ok()?;
            let y = fields.next()?.parse().ok()?;
            Some((x, y))
        })
        .collect();

    let (elapsed, group, describe) = {
        let guard = sessions().lock().expect("hint sessions mutex poisoned");
        let open = guard.get(&window);
        (
            open.map(|s| s.started.elapsed()),
            open.map(|s| s.group.name()).unwrap_or("?"),
            open.map(|s| s.target.describe()).unwrap_or("?"),
        )
    };
    // The window is in the line because two of these can now be interleaved on one terminal, and
    // "33 elements" twice says nothing about which page answered.
    eprintln!(
        "bru[hints]: window {window} group {group}: {} elements in {:.1} ms \
         ({} bytes of payload) — {describe}",
        points.len(),
        elapsed.map(|e| e.as_secs_f64() * 1000.0).unwrap_or(0.0),
        data.len(),
    );

    if points.is_empty() {
        // qutebrowser: message.error("No elements found."), and no mode change.
        sessions()
            .lock()
            .expect("hint sessions mutex poisoned")
            .remove(&window);
        eprintln!("bru: no hintable elements found");
        return;
    }

    let labels = hint_strings(points.len());
    let mut trie = BindingTrie::new();
    for (index, label) in labels.iter().enumerate() {
        match crate::bindings::parse_key_sequence(label) {
            Ok(sequence) => {
                trie.insert(&sequence, index);
            }
            Err(e) => eprintln!("bru: hint label {label:?} is not a key sequence: {e}"),
        }
    }

    let (first, mode) = {
        let mut guard = sessions().lock().expect("hint sessions mutex poisoned");
        let Some(open) = guard.get_mut(&window) else {
            return;
        };
        open.points = points;
        open.labels = labels.clone();
        open.trie = trie;
        open.sequence.clear();
        (open.first, open.auto_follow)
    };

    // --- unhardcoded ---------------------------------------------------------------------------
    // `hints.uppercase`, configdata.yml:1878 — **the drawn label only**, which is qutebrowser's own
    // behaviour and not a simplification of it: `HintLabel._update_text` (hints.py:113-119) upper-
    // cases what it paints, while `HintKeyParser.update_bindings` (modeparsers.py:235) is given the
    // raw strings. So `A` is drawn and `a` is typed, the trie is untouched, and nothing about the
    // key path changes. Uppercasing the trie instead would need Shift on every hint.
    let uppercase = crate::settings::is_on("hints.uppercase");
    let drawn: Vec<String> = labels
        .iter()
        .map(|label| if uppercase { label.to_uppercase() } else { label.clone() })
        .collect();
    let list = drawn
        .iter()
        .map(|label| format!("\"{label}\""))
        .collect::<Vec<_>>()
        .join(",");

    // Both lists, because they are two different things the moment `hints.uppercase` is on and the
    // whole risk of that setting is drawing one and matching the other. `BRU_DEBUG_HINTS=1` is the
    // only place the drawn one can be read — it is a string in a `execute_java_script` call and
    // never crosses back.
    debug(&format!(
        "window {window} labels {} drawn {}",
        labels.join(" "),
        drawn.join(" ")
    ));
    // --- end unhardcoded -----------------------------------------------------------------------
    show(
        browser,
        &format!("window.__bru_hints.show([{list}],{});", label_style_json()),
    );

    // Hint mode is entered in the window whose page answered, **by name**. It used to be
    // `enter_mode`, i.e. the current one, which was the same thing while a session was — and stopped
    // being so the moment a second window could hint: a page that answers a moment after the user
    // has clicked into the other window would have put *that* window into hint mode, with no session
    // behind it and the labels drawn somewhere else.
    let entered = state
        .lock()
        .expect("state mutex poisoned")
        .enter_mode_in(window, Mode::Hint, false);
    if entered {
        crate::ipc::set_mode_for(window, Mode::Hint.name().to_string());
        crate::ipc::set_keystring_for(window, String::new());
    }

    // `--first` (`gi`): follow element 0 without waiting for a key. `hints.py:_start_cb` fires
    // `strings[0]`, which is the label of the *first element* — the labels are scattered, so the
    // index is 0 and the label is not necessarily "a".
    if first {
        follow(state, window, browser, 0);
        return;
    }

    // --- unhardcoded ---------------------------------------------------------------------------
    // `hints.auto_follow always`, and this is the only place it can act: `_handle_auto_follow` is
    // called once from `_start_cb` (hints.py:657) "to make auto_follow == 'always' work", because
    // a page with one link has one label showing before any key is pressed. Every other value
    // answers `None` for an empty chain, so this costs them one comparison.
    if mode == AutoFollow::Always && labels.len() == 1 {
        follow(state, window, browser, 0);
    }
    // --- end unhardcoded -----------------------------------------------------------------------
}

/// The page has reported the URL behind a followed hint — every target except `normal` and `hover`.
fn on_href(state: &SharedState, window: u32, browser: &mut Browser, url: &str) {
    let Some((target, rapid, first_run)) = ({
        let guard = sessions().lock().expect("hint sessions mutex poisoned");
        guard
            .get(&window)
            .map(|s| (s.target.clone(), s.rapid, s.first_run))
    }) else {
        return;
    };

    // The session ends *here* and not in `follow`, because `clear()` on the page drops the element
    // array along with the labels — it is the teardown, not a repaint — and `href` would then be
    // looking up an index in an empty array. Measured 2026-08-06: clearing first reported no URL
    // for every link on the page.
    finish(state, window, browser, rapid);

    if url.is_empty() {
        eprintln!("bru: no suitable link found for this element");
        return;
    }

    // **No CEF call that navigates, and nothing that takes bru's own locks, from here.** This runs
    // inside the message router's query handler, and the router holds `browser_query_info_map`
    // across that call. Opening a tab is `window.add_child_view_at`, which creates the browser
    // *synchronously* (CEF-NOTES, Tabs) and navigates it, which reaches
    // `RequestHandler::on_before_browse`, which bru must forward to the router, which takes that
    // same lock. Measured 2026-08-06: bru froze with its window still painted, and `eu-stack`
    // showed the whole ring —
    //   on_process_message_received → on_query_str → hints::on_page_query → tabs::new_tab
    //   → add_child_view_at → …Navigate → on_before_browse → cancel_pending_for → BrowserInfoMap
    //   → lock_contended.
    // Posting it puts the work on the next turn of the message loop, by which time the router has
    // let go. Every URL target goes through here, not just the two that navigate: `fill` focuses
    // the bottom chrome and `download` will start a request, and neither is worth a second
    // deadlock to find out about.
    eprintln!("bru[hints]: window {window}: {} -> {url}", target.describe());
    let mut task = FollowUrl::new(target, url.to_string(), first_run);
    post_task(ThreadId::UI, Some(&mut task));
}

wrap_task! {
    struct FollowUrl {
        target: Target,
        url: String,
        first_run: bool,
    }

    impl Task {
        fn execute(&self) {
            let Some(state) = BruState::instance() else {
                return;
            };
            match &self.target {
                // `f`/`;h` never get here — they act on the element, not on a URL.
                Target::Normal | Target::Hover => {}

                Target::TabBg => crate::tabs::new_tab(&state, &self.url, true),
                Target::TabFg => crate::tabs::new_tab(&state, &self.url, false),
                // A window of its own, the same one `:open -w` makes. This was a foreground tab
                // until `src/window.rs` had a second window to open, and it is the one line `wf`
                // needed.
                //
                // `;R` needed nothing here and one thing in `handle_key`, and which one was
                // measured rather than foreseen — see `Foreign` there. Handing focus back from
                // this arm was tried first and loses a race it cannot win: the compositor's
                // activation of the new toplevel arrives *after* the task that made it, so
                // `tabs::focus` on the hinting window is undone by `on_window_activation_changed`
                // a moment later. Measured 2026-08-06, with the focus-back in place:
                //
                //   bru[hints]: window 1 opened; focus asked back to Some(0), state says Some(0)
                //   hint-script: after "s" -> mode normal, 0 hints, ... windows=2
                //
                // Whether a session is open behind this is read there rather than here, so nothing
                // about opening a window depends on how the session that asked for it ends.
                Target::Window => {
                    crate::window::open(&state, &self.url);
                }

                // `;o` / `;O`. qutebrowser's `preset_cmd_text`: substitute `{hint-url}`, then
                // refuse anything that is not a command line — a text not starting with `:`, `/`
                // or `?` cannot be one, and filling it would leave the user in command mode with
                // something unrunnable.
                Target::Fill(text) => {
                    let text = text.replace("{hint-url}", &self.url);
                    match text.chars().next() {
                        Some(first) if crate::cmdline::STARTCHARS.contains(first) => {
                            crate::cmdline::cmd_set_text(&text, false, false, false, None);
                        }
                        _ => eprintln!("bru: invalid command text {text:?}"),
                    }
                }

                // --- the two holes -------------------------------------------------------------
                // `;y`, `;Y`, `;d`. The URL is in hand and correct; what is missing is the sink.
                // Both are another workstream's, and a second implementation here would be the
                // thing that has to be deleted later. See `Clipboard` and `Downloads`.
                Target::Yank | Target::YankPrimary => {
                    let selection = self.target == Target::YankPrimary;
                    match clipboard() {
                        Some(clipboard) => clipboard.yank_url(&self.url, selection, self.first_run),
                        None => eprintln!(
                            "bru: hint yank has no clipboard yet — \
                             hints::install_clipboard has not been called"
                        ),
                    }
                }
                Target::Download => match downloads() {
                    Some(downloads) => downloads.download_url(&self.url),
                    None => eprintln!(
                        "bru: hint download has no download manager yet — \
                         hints::install_downloads has not been called"
                    ),
                },
            }
        }
    }
}

// ------------------------------------------------------------------------------------------------
// The two things hint mode needs and does not own
// ------------------------------------------------------------------------------------------------

/// `wl-copy` / `wl-paste`, decided 2026-08-06 (STAGE3-CONTRACTS.md). **Not implemented here.**
///
/// `;y` and `;Y` are `hint links yank` / `yank-primary`, and `:yank` is its own command in another
/// workstream. This is the whole of what hint mode needs from it; the module that owns the
/// clipboard implements it and calls [`install_clipboard`] once at startup. Until then `;y` prints
/// why it did nothing, which is the honest answer and is why `exec::is_live` says it is not live.
pub trait Clipboard: Send + Sync {
    /// `hints.py:HintActions.yank`. `selection` is the primary selection rather than the clipboard.
    ///
    /// `first_run` is false for the second and later follows of a **rapid** session, and it is the
    /// one piece of hint state the clipboard cannot work out for itself: qutebrowser *appends* to
    /// what is already there, joined with a newline, so `;r`-style repeated yanking collects a list
    /// instead of overwriting it each time.
    fn yank_url(&self, url: &str, selection: bool, first_run: bool);
}

/// `;d` — `hint links download`. **Not implemented here**; the download handler is its own
/// workstream and this is all hint mode asks of it. See [`Clipboard`] for the shape and the reason.
pub trait Downloads: Send + Sync {
    /// Start a download of `url`, as `hints.py:HintActions.download` does.
    fn download_url(&self, url: &str);
}

static CLIPBOARD: std::sync::OnceLock<Box<dyn Clipboard>> = std::sync::OnceLock::new();
static DOWNLOADS: std::sync::OnceLock<Box<dyn Downloads>> = std::sync::OnceLock::new();

/// Hands the clipboard workstream's implementation to hint mode, once, at startup. `app.rs` calls
/// it with `clip::HintClipboard`, which is what makes `;y` and `;Y` live.
pub fn install_clipboard(clipboard: Box<dyn Clipboard>) {
    let _ = CLIPBOARD.set(clipboard);
}

/// Hands the download workstream's implementation to hint mode, once, at startup. `app.rs` calls it
/// with `clip::HintDownloads`, which is what makes `;d` live.
pub fn install_downloads(downloads: Box<dyn Downloads>) {
    let _ = DOWNLOADS.set(downloads);
}

fn clipboard() -> Option<&'static dyn Clipboard> {
    CLIPBOARD.get().map(|c| c.as_ref())
}

fn downloads() -> Option<&'static dyn Downloads> {
    DOWNLOADS.get().map(|d| d.as_ref())
}

fn show(browser: &mut Browser, code: &str) {
    let Some(frame) = browser.main_frame() else {
        return;
    };
    frame.execute_java_script(Some(&CefString::from(code)), None, 0);
}

// ------------------------------------------------------------------------------------------------
// The labels' colours — bru's theme, carried into the page
// ------------------------------------------------------------------------------------------------

/// `{"fg":…,"bg":…,"match":…}` for `chrome/hints.js`'s `show`, out of the theme's
/// `--hints-*` custom properties.
///
/// The labels are drawn **in the page**, and a page cannot fetch `bru://chrome/theme.css`: the
/// scheme is registered CORS-enabled but the page's origin is not `bru://`, so the request is a
/// cross-origin one and is refused. Which is why the colours are resolved here and sent down with
/// the labels rather than read by a stylesheet — the alternative, and what was here first, is
/// qutebrowser's palette hard-coded in the JS, yellow labels on a themed browser.
///
/// Anything missing or unresolvable is simply left out and `chrome/hints.js` falls back to
/// qutebrowser's own defaults, so a theme that predates `--hints-*` still draws readable labels.
fn label_style_json() -> String {
    label_style_json_from(&theme_css())
}

fn label_style_json_from(css: &str) -> String {
    let properties = custom_properties(css);
    let mut fields: Vec<String> = Vec::new();
    for (field, property) in [
        ("fg", "--hints-fg"),
        ("bg", "--hints-bg"),
        ("match", "--hints-match-fg"),
        // No `border`. qutebrowser draws `1px solid #E3BE23` around a label and bru did too, taking
        // the label's own foreground for it; `chrome/hints.js` casts a shadow instead, because at
        // 12px a line on the edge read as a smudge on it. The shadow needs no field of its own — it
        // is mixed from `fg` in the page, which is the same colour this entry used to send twice.
    ] {
        if let Some(value) = resolve(&properties, property, 0).filter(|v| is_safe_colour(v)) {
            fields.push(format!("\"{field}\":\"{value}\""));
        }
    }
    format!("{{{}}}", fields.join(","))
}

/// The theme, read on every hint session for the same reason `src/chrome.rs` reads it on every
/// request: it belongs to themer, which rewrites it when the desktop theme changes, and a cache
/// would mean restarting bru to see a new palette.
///
/// (Duplicated from `chrome.rs` rather than shared — that module is another workstream's this
/// round. Merging the two into one `pub(crate) fn theme_css` is a two-line cleanup.)
fn theme_css() -> String {
    /// The theme bru ships with, used until themer has written one.
    const DEFAULT: &str = include_str!("../chrome/theme.css");

    let path = if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").filter(|d| !d.is_empty()) {
        Some(std::path::PathBuf::from(dir).join("bru/theme.css"))
    } else {
        std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .map(|home| std::path::PathBuf::from(home).join(".config/bru/theme.css"))
    };
    path.and_then(|path| std::fs::read_to_string(path).ok())
        .unwrap_or_else(|| DEFAULT.to_string())
}

/// Every `--name: value;` in the sheet. Not a CSS parser: the file is generated by
/// `lvim-colorscheme`, one declaration per line, and the only thing being read out of it is the
/// `:root` block's custom properties.
fn custom_properties(css: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in css.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("--") else {
            continue;
        };
        let Some((name, value)) = rest.split_once(':') else {
            continue;
        };
        let value = value.trim().trim_end_matches(';').trim();
        if value.is_empty() {
            continue;
        }
        out.push((format!("--{}", name.trim()), value.to_string()));
    }
    out
}

/// A property's value, following `var(--other)` to whatever is behind it.
///
/// The generated themes are two deep — `--hints-bg: var(--yellow)` — and the depth limit is only
/// there so a hand-edited file with a cycle in it cannot hang the browser.
fn resolve(properties: &[(String, String)], name: &str, depth: usize) -> Option<String> {
    if depth > 8 {
        return None;
    }
    let value = properties
        .iter()
        .rev()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())?;

    match value.strip_prefix("var(").and_then(|v| v.strip_suffix(')')) {
        Some(inner) => resolve(properties, inner.trim(), depth + 1),
        None => Some(value.to_string()),
    }
}

/// A colour is about to be interpolated into a JS string literal and then into `style.cssText`, and
/// the file it came from is one themer writes. Nothing that could end either context gets through:
/// no quote, no backslash, no semicolon, no brace, no angle bracket.
fn is_safe_colour(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "#(), .%-/".contains(c))
}

// ------------------------------------------------------------------------------------------------
// The key parser — HintKeyParser, over the same trie as every other mode
// ------------------------------------------------------------------------------------------------

/// One key while hint mode is current.
///
/// `None` means bru is not in hint mode and `src/keys.rs` should carry on to the ordinary parser.
/// `Some(swallow)` is what `on_pre_key_event` returns; it is always `true`, because hint mode
/// consumes everything — a key that reached the page here would be typed into whatever the last
/// click focused.
pub fn handle_key(state: &SharedState, browser: &mut Browser, info: KeyInfo) -> Option<bool> {
    // **The fast path, and the one this file is judged on.** `keys.rs` calls this for every key in
    // every mode, so what happens when nothing is hinting is what `j` pays for hint mode existing:
    // one uncontended lock, a `HashMap::is_empty`, and — only if the map is empty — the same indexed
    // mode read the single session did. `window_of_browser` walks the window list and their tabs, so
    // it is deliberately below this line and not above it.
    let hinting_somewhere = !sessions()
        .lock()
        .expect("hint sessions mutex poisoned")
        .is_empty();
    if !hinting_somewhere {
        // Hint mode with no session **anywhere** is reachable and has to be cleaned up rather than
        // sat in: `mode-enter` takes any mode name (`commands.rs`, `Mode::from_name`), so
        // `:mode-enter hint` puts a window into hint mode with nothing behind it. `Mode::Hint`
        // swallows what it cannot match, so leaving it alone would trap the keyboard until
        // `<Escape>`. Leave the mode and answer `None`; the key goes on to be the ordinary key it is.
        let window = {
            let guard = state.lock().expect("state mutex poisoned");
            (guard.mode() == Mode::Hint).then(|| guard.current_window_id()).flatten()
        };
        if let Some(window) = window {
            leave_mode(state, window);
        }
        return None;
    }

    // Something is hinting. Now it is worth asking which window this key arrived at — `keys.rs` has
    // already made that window current, so this is the same window `BruState::mode` would answer
    // for, asked by browser because that is the fact the decision below is about.
    let key_browser = browser.identifier();
    let (key_window, key_window_in_hint_mode, open) = {
        let guard = state.lock().expect("state mutex poisoned");
        let Some(key_window) = guard.window_of_browser(key_browser) else {
            // A browser bru never put in a window. There is nothing to route.
            return None;
        };
        let sessions = sessions().lock().expect("hint sessions mutex poisoned");
        let open: Vec<Open> = sessions
            .iter()
            .map(|(window, session)| Open {
                window: *window,
                browser: session.browser_id,
                in_hint_mode: guard.mode_in(*window) == Mode::Hint,
                rapid_window: session.rapid && session.target == Target::Window,
            })
            .collect();
        (key_window, guard.mode_in(key_window) == Mode::Hint, open)
    };

    let foreign = Foreign::decide(key_window, key_browser, key_window_in_hint_mode, &open);
    if foreign != Foreign::Ordinary {
        debug(&format!("key {info} at window {key_window} -> {foreign:?}"));
    }

    // Declared out here and filled in one arm, so that the `Browser` the `&mut` points at outlives
    // the match. Holding it is also what stops that tab from closing for the length of this call
    // (CEF-NOTES, Tabs) — one keystroke, and never across the session.
    let mut hinting;
    let (window, browser): (u32, &mut Browser) = match foreign {
        Foreign::Ordinary => return None,
        // In hint mode with nothing behind it — see the `:mode-enter hint` note above. Reached here
        // rather than in the fast path when some *other* window is hinting.
        Foreign::StaleMode(window) => {
            leave_mode(state, window);
            return None;
        }
        Foreign::End(window) => {
            sessions()
                .lock()
                .expect("hint sessions mutex poisoned")
                .remove(&window);
            leave_mode(state, window);
            return None;
        }
        Foreign::Same(window) => (window, browser),
        Foreign::AimBack(window, id) => {
            let found = state.lock().expect("state mutex poisoned").browser_with_id(id);
            match found {
                Some(found) => {
                    hinting = found;
                    (window, &mut hinting)
                }
                // The hinting tab has gone since the last follow. Nothing to draw on and nothing to
                // click; this is the ordinary stale case after all.
                None => {
                    sessions()
                        .lock()
                        .expect("hint sessions mutex poisoned")
                        .remove(&window);
                    leave_mode(state, window);
                    return None;
                }
            }
        }
    };

    // `<Escape>` is `mode-leave` in the `hint:` table, and it is taken *before* that table rather
    // than through it: `exec`'s `mode-leave` knows how to leave a mode and nothing about a hint
    // session, so running it would leave the labels drawn over the page and the session open behind
    // them. qutebrowser does not need the special case because `HintManager.on_mode_left` is wired
    // to the mode manager's signal; bru has no such signal, and one function is cheaper than one.
    //
    // Aimed at the session's window, so `<Escape>` ends a `;R` from whichever window its own follows
    // have moved the focus to — the session is still one key from over.
    if info.key == Key::Named(NamedKey::Escape) {
        cancel_in(state, window, browser);
        return Some(true);
    }

    // `HintKeyParser.handle` (modeparsers.py:196): the command parser gets a dry run first, and
    // only a key it does not want is offered to the labels. This is what makes `<Ctrl-F>`
    // (`hint links`), `<Ctrl-R>` (`hint --rapid links tab-bg`) and `<Ctrl-B>` (`hint all tab-bg`)
    // work from inside hint mode. Hint labels are spelled out of `hints.chars`, so nothing here can
    // shadow a label.
    let binding = {
        let mut guard = sessions().lock().expect("hint sessions mutex poisoned");
        match guard.get_mut(&window) {
            None => crate::bindings::MatchType::NoMatch,
            Some(open) => {
                open.command_sequence.push(info);
                match open.commands.matches(&open.command_sequence) {
                    Match::Exact(command) => {
                        let command = command.clone();
                        open.command_sequence.clear();
                        // Not run under the lock: `exec::run` reaches `hints::start`, which takes
                        // it again.
                        drop(guard);
                        crate::exec::run(state, browser, &command, None);
                        return Some(true);
                    }
                    Match::Partial => crate::bindings::MatchType::PartialMatch,
                    Match::NoMatch => {
                        open.command_sequence.clear();
                        crate::bindings::MatchType::NoMatch
                    }
                }
            }
        }
    };
    if binding == crate::bindings::MatchType::PartialMatch {
        return Some(true);
    }

    // `HintKeyParser._handle_filter_key`: backspace walks the chain back rather than clearing it.
    if info.key == Key::Named(NamedKey::Backspace) {
        let mut guard = sessions().lock().expect("hint sessions mutex poisoned");
        if let Some(open) = guard.get_mut(&window) {
            open.sequence.pop();
        }
        drop(guard);
        // Backspacing can leave one label showing as easily as typing forwards can, and
        // qutebrowser fires `_handle_auto_follow` from the same `handle_partial_key` either way.
        if let Some(index) = redraw(window, browser) {
            follow(state, window, browser, index);
        }
        return Some(true);
    }

    let outcome = {
        let mut guard = sessions().lock().expect("hint sessions mutex poisoned");
        let Some(open) = guard.get_mut(&window) else {
            // Unreachable: `Foreign::decide` saw this session a few statements ago, and the only
            // paths between here and there that can remove one — `cancel_in` and the command trie's
            // `exec::run` — both `return` before reaching this. It is a `return` and not an `expect`
            // because a panic inside `on_pre_key_event` takes the browser with it, and hint mode
            // swallows a key it cannot match either way.
            return Some(true);
        };
        open.sequence.push(info);
        match open.trie.matches(&open.sequence) {
            // --- unhardcoded -------------------------------------------------------------------
            // An exact match is `hints.auto_follow full-match`, which is also what `always` and
            // `unique-match` do once the label is complete — so three of the four values follow
            // here. `never` is the one that does not: the chain stays, one label is left showing,
            // and `<Return>` is what acts on it. See `follow_current`.
            Match::Exact(_) if open.auto_follow == AutoFollow::Never => Outcome::Pending,
            // --- end unhardcoded ---------------------------------------------------------------
            Match::Exact(index) => Outcome::Follow(*index),
            Match::Partial => Outcome::Pending,
            Match::NoMatch => {
                // `BaseKeyParser.handle` clears the chain on a no-match. In letter mode there is
                // nothing else to do with the key: it names no hint.
                open.sequence.clear();
                Outcome::NoMatch
            }
        }
    };

    match outcome {
        Outcome::Follow(index) => follow(state, window, browser, index),
        // `hints.auto_follow` is asked here rather than only on an exact match — see [`auto_follow`]
        // for what the difference is and for the measurement that says it is nothing today.
        Outcome::Pending | Outcome::NoMatch => {
            if let Some(index) = redraw(window, browser) {
                follow(state, window, browser, index);
            }
        }
    }
    Some(true)
}

enum Outcome {
    Follow(usize),
    Pending,
    NoMatch,
}

/// One open session, reduced to the four facts [`Foreign::decide`] needs. Built in [`handle_key`]
/// from the map and the mode manager, so that the rule itself has no CEF in it and can be asserted
/// in a unit test — `handle_key` posts tasks and cannot be called from one (CEF-NOTES trap 13).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Open {
    window: u32,
    browser: i32,
    /// Whether that window is actually in hint mode. It is not between `f` and the page's answer:
    /// `start` makes the session and `on_collected` enters the mode, which is what stops `f` on a
    /// blank page from trapping the keyboard.
    in_hint_mode: bool,
    /// `hint --rapid links window`, and nothing else. The one session whose own follows move the
    /// compositor's focus to a window it does not live in.
    rapid_window: bool,
}

/// Where a key that reached [`handle_key`] should be acted on.
///
/// **"Foreign" means something different now that a session belongs to a window.** It used to mean
/// "the key arrived at a browser that is not the session's", with one session for the process to
/// compare against. A key arriving at window B while window B has its own session is not foreign at
/// all, and the interesting question moved up a level: which of the open sessions, if any, is this
/// key about?
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Foreign {
    /// Nothing here is hint mode's. `keys.rs` carries on to the ordinary parser.
    Ordinary,
    /// This window is in hint mode with no session behind it. Leave the mode and pass the key on.
    StaleMode(u32),
    /// This window's session is on another of its tabs. Drop it and pass the key on.
    End(u32),
    /// Act on this window's own session, at the browser the key arrived at.
    Same(u32),
    /// Act on **another** window's session, at that window's hinting browser — the key is only here
    /// because that session's own follow opened the window it landed in.
    AimBack(u32, i32),
}

impl Foreign {
    /// The rule, with no CEF in it.
    ///
    /// Four cases, in the order they are asked:
    ///
    /// 1. **This window has a session.** If the key reached the tab the labels are on, act
    ///    ([`Foreign::Same`]). If it reached another tab of the same window, the labels are on a page
    ///    nobody is looking at and matching against them would follow a link the user cannot see, so
    ///    the session goes ([`Foreign::End`]) — a `--rapid` session is the only one that can outlive
    ///    a tab switch to begin with. If the window is not in hint mode yet, the page has not
    ///    answered `f` and there is nothing to match: the key is ordinary.
    /// 2. **This window is in hint mode with no session.** `:mode-enter hint` is a live command and
    ///    that is what it leaves behind. `Mode::Hint` swallows what it cannot match, so the mode has
    ///    to be left rather than sat in.
    /// 3. **No session here, and exactly one rapid-window session elsewhere.** That is `;R`, and it
    ///    is the whole reason this function is not just "does this window have a session".
    ///    `hint --rapid links window` opens a window per follow; a new toplevel takes the
    ///    compositor's keyboard focus, so the second label of every rapid window session is delivered
    ///    to the window the *first* one made. Measured 2026-08-06 without it: `;R` opened one window
    ///    and stopped. The key is aimed back at the browser the labels are drawn on, which is the same
    ///    decision `keys.rs` already makes for a key that lands on a chrome strip (trap 11).
    /// 4. **Two of those at once.** Refused rather than guessed: nothing in the key says which of the
    ///    two sessions opened the window it landed in, and picking one would send a keystroke into a
    ///    page at random. It needs both windows to be running `;R` simultaneously, which needs the
    ///    second `;R` to have been typed while the first was swallowing keys — so it is a state
    ///    nobody can reach today, and this is what it does if that ever changes.
    fn decide(
        key_window: u32,
        key_browser: i32,
        key_window_in_hint_mode: bool,
        open: &[Open],
    ) -> Foreign {
        if let Some(here) = open.iter().find(|session| session.window == key_window) {
            if !here.in_hint_mode {
                return Foreign::Ordinary;
            }
            return if here.browser == key_browser {
                Foreign::Same(key_window)
            } else {
                Foreign::End(key_window)
            };
        }

        // A stale mode is cleaned up before a key is lent to another window's session: the window in
        // front is the one the user is looking at, and leaving them stuck in a mode nothing can match
        // is worse than costing `;R` one keystroke.
        if key_window_in_hint_mode {
            return Foreign::StaleMode(key_window);
        }

        let mut rapid = open
            .iter()
            .filter(|session| session.rapid_window && session.in_hint_mode);
        match (rapid.next(), rapid.next()) {
            (Some(only), None) => Foreign::AimBack(only.window, only.browser),
            _ => Foreign::Ordinary,
        }
    }
}

/// `hints.auto_follow`, configdata.yml:1673 — the rule, with the setting's value passed in.
///
/// `visible` is the indices still showing and `typed` the chain that left them.
///
// --- unhardcoded -------------------------------------------------------------------------------
/// **All four values, and the default is the behaviour bru already had.** What was here before was
/// `unique-match` written out as the only rule, with a doc comment explaining that the other three
/// "are not implemented, because bru ships no configuration" — which was the reading of DESIGN.md
/// the user corrected on 2026-08-06. Two of them are still worth knowing about:
///
/// - **[`AutoFollow::FullMatch`] is the rule bru followed before this function existed** — an exact
///   match on the whole label, which `handle_key` still takes as `Outcome::Follow`. It is *not*
///   what qutebrowser does by default, and the difference is measurable only under a `min_chars`
///   above 1 — see the measurement below.
/// - **[`AutoFollow::Never`] is the only value under which `<Return>` has a job.** `hint-follow`
///   was inert and documented as permanently refused; it is live now, and it is live *because* this
///   value exists. Under any other value it says why there is nothing to follow rather than doing
///   nothing.
// --- end unhardcoded ---------------------------------------------------------------------------
///
/// `always` also fires with an empty chain — `_handle_auto_follow` is called once from `_start_cb`
/// (hints.py:657) "to make auto_follow == 'always' work", so a page with one link is followed
/// before a key is pressed. `unique-match` is `bool(keystr or filterstr)`, so an empty chain never
/// follows; that is what the `typed.is_empty()` arm is, and it is why `f` on a one-link page still
/// waits for `a`.
///
/// **Measured 2026-08-06: with `hints.chars` and `hints.min_chars` at their defaults this can
/// never fire before the full label is typed.** Over 1..1,000,000 elements there is no prefix that
/// is not itself a label and yet leaves one label showing — the smallest such set has two in it.
/// See `unique_match_cannot_beat_a_full_match_under_the_default_hints_chars`. So this replaces a
/// rule that happens to agree with it, and the agreement is a property of `hint_strings`'
/// arithmetic rather than of the rule: a `min_chars` of 2 breaks it on the first page with one
/// link, which is what `unique_match_follows_a_label_that_was_never_typed_out` pins down.
fn auto_follow(mode: AutoFollow, visible: &[usize], typed: &str) -> Option<usize> {
    match (mode, visible) {
        // --- unhardcoded -----------------------------------------------------------------------
        (AutoFollow::Always, [only]) => Some(*only),
        // The whole label, and nothing shorter. `handle_key`'s `Match::Exact` is what fires it, so
        // there is nothing for this function to answer.
        (AutoFollow::FullMatch, _) | (AutoFollow::Never, _) => None,
        // --- end unhardcoded -------------------------------------------------------------------
        (AutoFollow::UniqueMatch, [only]) if !typed.is_empty() => Some(*only),
        _ => None,
    }
}

/// Push the current chain to the status bar and to the page's labels, and answer with the hint
/// [`auto_follow`] wants followed, if any.
///
/// The visible set is computed here, in Rust, and sent as a list of indices. The page is told which
/// labels to show; it is never asked which ones match, and never which one to follow.
fn redraw(window: u32, browser: &mut Browser) -> Option<usize> {
    let (keystring, visible, matched_len, mode) = {
        let guard = sessions().lock().expect("hint sessions mutex poisoned");
        let open = guard.get(&window)?;
        let typed = crate::bindings::sequence_to_string(&open.sequence);
        let visible: Vec<usize> = open
            .labels
            .iter()
            .enumerate()
            .filter(|(_, label)| label.starts_with(&typed))
            .map(|(index, _)| index)
            .collect();
        (typed.clone(), visible, typed.chars().count(), open.auto_follow)
    };

    // The hinting window's bar, not the one in front. During `;R` the key that filtered these labels
    // arrived at a window the session's own follow opened, and the chain belongs on the bar of the
    // window whose page is showing them.
    crate::ipc::set_keystring_for(window, keystring.clone());
    show(
        browser,
        &format!(
            "window.__bru_hints && window.__bru_hints.filter([{}],{matched_len});",
            visible
                .iter()
                .map(|index| index.to_string())
                .collect::<Vec<_>>()
                .join(",")
        ),
    );
    auto_follow(mode, &visible, &keystring)
}

// --- unhardcoded -------------------------------------------------------------------------------
/// `hint-follow` — `<Return>` in hint mode, and the whole of what `hints.auto_follow never` buys.
///
/// **This binding was refused rather than unimplemented until the setting existed**, and the
/// refusal said exactly why: "It would have a job only if following on a unique match could be
/// turned off, and bru has no such setting." It has one, so this follows the label the chain has
/// left standing.
///
/// It follows a *unique* one, which is qutebrowser's own rule — `follow_hint` with no argument
/// raises `HintingError("No hint to follow")` when nothing is filtered and picks the single
/// remaining one when something is. Under any other value of `hints.auto_follow` the label would
/// already have been followed before the key arrived, so this says so instead of doing nothing;
/// that is the difference between a key that is inert and a key that answers.
pub fn follow_current(state: &SharedState, browser: &mut Browser) {
    let Some(window) = window_of(state, browser) else {
        return;
    };
    let found = {
        let guard = sessions().lock().expect("hint sessions mutex poisoned");
        guard.get(&window).map(|open| {
            let typed = crate::bindings::sequence_to_string(&open.sequence);
            let visible: Vec<usize> = open
                .labels
                .iter()
                .enumerate()
                .filter(|(_, label)| label.starts_with(&typed))
                .map(|(index, _)| index)
                .collect();
            (open.auto_follow, visible)
        })
    };
    let Some((mode, visible)) = found else {
        crate::message::error("hint-follow: no hints are showing");
        return;
    };
    if mode != AutoFollow::Never {
        crate::message::error(
            "hint-follow: with hints.auto_follow at anything but `never`, a hint that could be \
             followed has been followed already — set it to `never` to follow with <Return>",
        );
        return;
    }
    match visible.as_slice() {
        [only] => follow(state, window, browser, *only),
        [] => crate::message::error("hint-follow: nothing matches what has been typed"),
        many => crate::message::error(&format!(
            "hint-follow: {} hints still match — type more of the label",
            many.len()
        )),
    }
}
// --- end unhardcoded ---------------------------------------------------------------------------

/// A hint matched. The element's position is enough for `normal` and `hover`; every other target
/// needs its URL, which only the page can give, so those ask and continue in [`on_href`].
fn follow(state: &SharedState, window: u32, browser: &mut Browser, index: usize) {
    let (target, rapid, point, token) = {
        let guard = sessions().lock().expect("hint sessions mutex poisoned");
        let Some(open) = guard.get(&window) else {
            return;
        };
        let Some(point) = open.points.get(index).copied() else {
            return;
        };
        (open.target.clone(), open.rapid, point, open.token.clone())
    };

    if target.needs_url() {
        // Ask before clearing, and clear in `on_href` — see the comment there for why the order is
        // not free. The session stays open until the page answers.
        show(
            browser,
            &format!("window.__bru_hints.href(\"{token}\",{index});"),
        );
        return;
    }

    // qutebrowser leaves hint mode *before* running the handler (`hints.py:_fire`), and so does
    // this: the click that follows may navigate, and a mode left afterwards is a mode left on a
    // page that no longer exists.
    finish(state, window, browser, rapid);

    match target {
        Target::Normal => {
            eprintln!(
                "bru[hints]: window {window} clicking hint {index} at ({}, {})",
                point.0, point.1
            );
            click(browser, point.0, point.1);
        }
        // `;h`. qutebrowser calls `elem.hover()`, which dispatches a synthetic `mouseover`; bru
        // moves the real pointer instead, for the same reason the follow is a real click — a
        // synthetic event skips everything that checks `isTrusted`, and a menu that opens on hover
        // is exactly the kind of thing that checks.
        Target::Hover => {
            eprintln!(
                "bru[hints]: window {window} hovering hint {index} at ({}, {})",
                point.0, point.1
            );
            hover(browser, point.0, point.1);
        }
        _ => unreachable!("every other target needs a URL and returned above"),
    }
}

/// End a follow: with `--rapid` the session and the labels stay and only the typed chain is reset;
/// without it, everything goes.
///
/// `hints.py:_fire` again — the rapid branch resets the filter and redraws every label with no
/// highlighted prefix, which is what lets the next label be typed straight away.
fn finish(state: &SharedState, window: u32, browser: &mut Browser, rapid: bool) {
    if !rapid {
        sessions()
            .lock()
            .expect("hint sessions mutex poisoned")
            .remove(&window);
        clear_labels(browser);
        leave_mode(state, window);
        return;
    }

    {
        let mut guard = sessions().lock().expect("hint sessions mutex poisoned");
        let Some(open) = guard.get_mut(&window) else {
            return;
        };
        open.sequence.clear();
        open.first_run = false;
    }
    // The chain has just been emptied, so `auto_follow` answers `None` by construction — a rapid
    // session must not fire its next hint before a key is pressed. That is `always`'s behaviour and
    // not `unique-match`'s; see [`auto_follow`].
    // (Called for its effect, and bound first: `debug_assert!(redraw(..).is_none())` would put the
    // only call to `redraw` inside a macro that is compiled out of a release build.)
    let auto = redraw(window, browser);
    debug_assert!(auto.is_none(), "a rapid reset must not auto-follow");
}

/// The follow, on Chromium's real input path.
///
/// A move first, because hover state is what a page's own handlers look at and a press with no
/// preceding move arrives at an element that was never entered. Then press and release, one click.
fn click(browser: &mut Browser, x: i32, y: i32) {
    // The page measured this point in CSS pixels and a mouse event is in view coordinates; page
    // zoom is the ratio. See `exec::view_point` for the follow that missed a 30px field by a pixel.
    let (x, y) = crate::exec::view_point(browser, x, y);
    // --- src/focus.rs ---------------------------------------------------------------------
    // The whole click lives in `focus.rs` now, in three ordered pieces:
    //
    // - It **arms the caret move first** and waits for the renderer to say so, which is what puts
    //   the caret after a field's existing text with no painted frame in between — see
    //   `focus::click_through` for the 6-of-10 race a fire-and-forget arm measurably loses.
    // - The events themselves are the move-press-release this function used to send.
    // - Then `focus::after_click` asks what the click landed on, because a hint onto a field that
    //   **already had the focus** changes no focus and fires no callback. Measured 2026-08-07 on
    //   `https://start.duckduckgo.com/`, whose own script focuses its search box: `hint inputs`
    //   then `a` clicked at (729, 539), no focus change was reported, and the mode stayed normal.
    //   The click has to say so itself, and since the Escape of 2026-08-09 it is the only way a
    //   web page's field enters insert mode at all.
    crate::focus::click_through(browser, x, y);
    // --- end src/focus.rs -----------------------------------------------------------------
}

/// `;h` — the move without the press. `mouse_leave = 0` means the pointer is entering, which is
/// what raises `mouseover`/`:hover` on the element and everything nested under it.
fn hover(browser: &mut Browser, x: i32, y: i32) {
    let (x, y) = crate::exec::view_point(browser, x, y);
    let Some(host) = browser.host() else {
        return;
    };
    host.send_mouse_move_event(Some(&MouseEvent { x, y, modifiers: 0 }), 0);
}

// ------------------------------------------------------------------------------------------------
// Reading the page's answer
// ------------------------------------------------------------------------------------------------

/// One field out of the flat object `chrome/hints.js` sends. Every value it writes is either hex or
/// percent-encoded, so no value can contain a quote and this needs no escape handling.
fn field(src: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":\"");
    let at = src.find(&needle)? + needle.len();
    let rest = &src[at..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

// ------------------------------------------------------------------------------------------------
// The debug switch
// ------------------------------------------------------------------------------------------------

/// `--hint-script='hint images,esc,f,a,s' --hint-step-ms=2000` drives hint mode from posted UI
/// tasks. A step is one of:
///
/// | | |
/// |---|---|
/// | `hint …` | any `hint` command string, through the real parser and the real dispatcher |
/// | `esc` | cancel the session |
/// | `report` | print every window's session and mode without pressing anything |
/// | anything else | fed to the key parser one character at a time |
///
/// **It aims at `state.active_browser()`, which is the window in front, so it cannot start a session
/// in a second window on its own.** That is what `--cmd='win1:hint links'` is for, and what
/// `--cmdline-script='key:win1:a'` is for; `report` is here so the two of them have something that
/// prints both sessions at once.
///
/// There is deliberately **no `f` shorthand**. `f` is a hint label — `hints.chars` is `asdfghjkl` —
/// and a step that meant "start hinting" would shadow the fourth label on every page; it did, and
/// a follow that quietly re-collected instead looked exactly like a click that missed.
///
/// It exists for the same reason as `--tab-script`. The only key-injection tool on this machine is
/// `wtype`, which attaches a virtual keyboard, and CEF segfaults in `xkb_state_update_mask` when
/// the keymap arrives — measured 2026-08-06, 2/3 runs, with the leftover keystrokes landing in
/// whatever the compositor focuses next. So keys cannot drive an unattended check here, and this
/// drives the very functions the keys call instead. Inert unless the switch is passed.
pub fn schedule_hint_script(steps: &str, interval_ms: i64) {
    for (i, step) in steps.split(',').filter(|s| !s.is_empty()).enumerate() {
        let mut task = HintStep::new(step.to_string());
        post_delayed_task(ThreadId::UI, Some(&mut task), interval_ms * (i as i64 + 1));
    }
}

wrap_task! {
    struct HintStep {
        step: String,
    }

    impl Task {
        fn execute(&self) {
            let Some(state) = BruState::instance() else {
                return;
            };
            let browser = state.lock().expect("state mutex poisoned").active_browser();
            let Some(mut browser) = browser else {
                eprintln!("hint-script: no tab to aim at");
                return;
            };

            match self.step.as_str() {
                "esc" => cancel(&state, &mut browser),
                // Nothing pressed; the report below is the whole step.
                "report" => {}
                // Collection is a round trip through the page, so there is nothing to report for a
                // `hint` step here; `on_collected` prints the count and the timing when the answer
                // lands. The groups and the targets are reached the way a keypress reaches them,
                // through `commands::parse` and `exec::run`, so what this drives is what `;i` does
                // and not a second path.
                step if step.starts_with("hint") => match crate::commands::parse(step) {
                    Ok(command) => {
                        eprintln!("hint-script: {step:?} -> {command:?}");
                        return crate::exec::run(&state, &mut browser, &command, None);
                    }
                    Err(e) => eprintln!("hint-script: {step:?}: {e}"),
                },
                // A step in `<…>` spelling is one key, not a run of characters — that is how the
                // `hint:` bindings (`<Ctrl-F>`, `<Ctrl-R>`, `<Ctrl-B>`) can be driven at all.
                keys if keys.starts_with('<') => {
                    match crate::bindings::parse_key_sequence(keys) {
                        Ok(sequence) => {
                            for info in sequence {
                                handle_key(&state, &mut browser, info);
                            }
                        }
                        Err(e) => eprintln!("hint-script: {keys:?}: {e}"),
                    }
                }
                keys => {
                    for c in keys.chars() {
                        match crate::bindings::parse_key_sequence(&c.to_string()) {
                            Ok(sequence) => {
                                for info in sequence {
                                    handle_key(&state, &mut browser, info);
                                }
                            }
                            Err(e) => eprintln!("hint-script: {c:?}: {e}"),
                        }
                    }
                }
            }

            // Tabs, windows and the URL as well as the mode, because that is what the targets
            // change and there is otherwise nothing to read: `;f` and `;b` differ only in which
            // tab is active, `wf` and `;f` only in whether a window appeared, and `;h` and `f`
            // only in what the page did about it.
            let (tabs, active, windows, modes) = {
                let guard = state.lock().expect("state mutex poisoned");
                let modes: Vec<String> = guard
                    .window_ids()
                    .into_iter()
                    .map(|window| format!("win{window} {}", guard.mode_in(window)))
                    .collect();
                (guard.tab_count(), guard.active_tab(), guard.window_count(), modes)
            };
            // **Every** window's session, not "the" session. Two of them open at once is the whole
            // claim this workstream makes, and one line naming only the current window's could not
            // tell that state from the one it replaced.
            let open = {
                let guard = sessions().lock().expect("hint sessions mutex poisoned");
                let mut windows: Vec<u32> = guard.keys().copied().collect();
                windows.sort_unstable();
                windows
                    .into_iter()
                    .map(|window| {
                        let session = &guard[&window];
                        format!(
                            "win{window}: {} hints on browser {} chain {:?}{}",
                            session.labels.len(),
                            session.browser_id,
                            crate::bindings::sequence_to_string(&session.sequence),
                            if session.rapid { " rapid" } else { "" },
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("; ")
            };
            eprintln!(
                "hint-script: after {:?} -> modes [{}], sessions [{open}], \
                 tabs={tabs} active={active} windows={windows}, url={}",
                self.step,
                modes.join(", "),
                crate::ipc::current_url(),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chars() -> Vec<char> {
        CHARS.chars().collect()
    }

    #[test]
    fn ceil_log_matches_qutebrowsers() {
        // utils.ceil_log's own docstring: max(1, ceil(log(number, base))).
        assert_eq!(ceil_log(1, 9), 1);
        assert_eq!(ceil_log(9, 9), 1);
        assert_eq!(ceil_log(10, 9), 2);
        assert_eq!(ceil_log(81, 9), 2);
        assert_eq!(ceil_log(82, 9), 3);
        assert_eq!(ceil_log(729, 9), 3);
        assert_eq!(ceil_log(730, 9), 4);
    }

    #[test]
    fn number_to_hint_str_counts_in_base_nine() {
        let chars = chars();
        // The first nine numbers are the alphabet itself.
        assert_eq!(number_to_hint_str(0, &chars, 0), "a");
        assert_eq!(number_to_hint_str(8, &chars, 0), "l");
        // Then it carries, and the carry digit is a real digit rather than a pad: 9 is "10" in
        // base nine, which is "sa" and not "aa".
        assert_eq!(number_to_hint_str(9, &chars, 0), "sa");
        assert_eq!(number_to_hint_str(10, &chars, 0), "ss");
        assert_eq!(number_to_hint_str(80, &chars, 0), "ll");
        assert_eq!(number_to_hint_str(81, &chars, 0), "saa");
        // Padding to `digits` uses the first character, as "0" would in base 10.
        assert_eq!(number_to_hint_str(0, &chars, 2), "aa");
        assert_eq!(number_to_hint_str(8, &chars, 3), "aal");
    }

    #[test]
    fn labels_are_unique_and_prefix_free() {
        // Prefix-freedom is what makes an exact match final: with `a` and `as` both bound, `a`
        // could never follow without waiting for a second key. `_hint_scattered` guarantees it by
        // starting the long labels at short_count * len(chars), and this is the property the whole
        // follow-on-exact-match path rests on.
        for count in [1usize, 2, 8, 9, 10, 25, 80, 81, 82, 200, 729, 730, 1000] {
            let labels = hint_strings(count);
            assert_eq!(labels.len(), count, "{count} elements must get {count} labels");

            let mut sorted = labels.clone();
            sorted.sort();
            let unique = {
                let mut u = sorted.clone();
                u.dedup();
                u
            };
            assert_eq!(sorted, unique, "duplicate label with {count} elements");

            for (i, a) in sorted.iter().enumerate() {
                if let Some(b) = sorted.get(i + 1) {
                    assert!(
                        !b.starts_with(a.as_str()),
                        "{a:?} is a prefix of {b:?} with {count} elements"
                    );
                }
            }

            for label in &labels {
                assert!(
                    label.chars().all(|c| CHARS.contains(c)),
                    "{label:?} is not spelled out of hints.chars"
                );
            }
        }
    }

    #[test]
    fn short_labels_are_used_before_long_ones() {
        // 10 elements do not need two characters each: nine of them fit in one, and Vimium's trick
        // — which qutebrowser copies — is to hand those out first.
        let labels = hint_strings(10);
        let short = labels.iter().filter(|l| l.chars().count() == 1).count();
        assert_eq!(short, 8, "10 elements: 8 one-character labels and 2 two-character ones");
        assert_eq!(labels.iter().filter(|l| l.chars().count() == 2).count(), 2);

        // Nine or fewer never needs more than one character at all.
        for count in 1..=9 {
            assert!(
                hint_strings(count).iter().all(|l| l.chars().count() == 1),
                "{count} elements should all get single-character labels"
            );
        }
    }

    #[test]
    fn hints_are_scattered_rather_than_run_in_order() {
        // The whole output for 27 elements, taken from running qutebrowser's own `_hint_strings`
        // (hints.py:424) against the same input — chars=asdfghjkl, min_chars=1, scatter=true. A
        // reference value rather than a property, because every step of the algorithm is visible in
        // it: six short labels, twenty-one long ones, and `_shuffle_hints` interleaving them.
        assert_eq!(
            hint_strings(27),
            vec![
                "a", "jf", "kf", "s", "jg", "kg", "d", "jh", "kh", "f", "jj", "kj", "g", "jk",
                "kk", "h", "jl", "kl", "ja", "ka", "la", "js", "ks", "ls", "jd", "kd", "ld",
            ]
        );
        assert_eq!(hint_strings(9), vec!["a", "s", "d", "f", "g", "h", "j", "k", "l"]);
        assert_eq!(
            hint_strings(10),
            vec!["a", "ls", "s", "d", "f", "g", "h", "j", "k", "la"]
        );

        // What the shuffle is for: neighbouring links must not all begin with the same letter, or
        // a page of 90 links has `a?` on the first nine and nothing else anywhere near them.
        let labels = hint_strings(90);
        let runs = labels
            .windows(2)
            .filter(|pair| pair[0].chars().next() == pair[1].chars().next())
            .count();
        assert!(runs < 10, "{runs} adjacent pairs share a first character out of 89");
    }

    #[test]
    fn the_trie_answers_hint_labels_the_way_it_answers_bindings() {
        // The point of BindingTrie being generic: hint mode is another value type, not another
        // structure. modeparsers.py:135, HintKeyParser.update_bindings.
        let labels = hint_strings(20);
        let mut trie: BindingTrie<usize> = BindingTrie::new();
        for (index, label) in labels.iter().enumerate() {
            trie.insert(&crate::bindings::parse_key_sequence(label).unwrap(), index);
        }
        assert_eq!(trie.len(), 20);

        for (index, label) in labels.iter().enumerate() {
            let sequence = crate::bindings::parse_key_sequence(label).unwrap();
            match trie.matches(&sequence) {
                Match::Exact(found) => assert_eq!(*found, index),
                other => panic!("{label:?} should match exactly, got {:?}", other.match_type()),
            }
            // Every prefix short of the whole label is partial, never exact.
            for cut in 1..sequence.len() {
                assert_eq!(
                    trie.matches(&sequence[..cut]).match_type(),
                    crate::bindings::MatchType::PartialMatch,
                    "{label:?} truncated to {cut} keys"
                );
            }
        }

        // A character outside hints.chars names nothing.
        let z = crate::bindings::parse_key_sequence("z").unwrap();
        assert_eq!(trie.matches(&z).match_type(), crate::bindings::MatchType::NoMatch);
    }

    #[test]
    fn the_page_payload_is_read_back_exactly() {
        let request = "{\"type\":\"hints\",\"token\":\"deadbeef\",\"kind\":\"elems\",\
                       \"data\":\"12%2C34%7C56%2C78\"}";
        assert_eq!(field(request, "token").as_deref(), Some("deadbeef"));
        assert_eq!(field(request, "kind").as_deref(), Some("elems"));
        assert_eq!(
            percent_decode(&field(request, "data").unwrap()),
            "12,34|56,78"
        );
        assert_eq!(field(request, "missing"), None);

        // A URL, which is what `F` gets back. encodeURIComponent escapes the separators bru would
        // otherwise have to unpick, and leaves the rest alone.
        assert_eq!(
            percent_decode("https%3A%2F%2Fwww.vesti.bg%2Fa%3Fb%3D1%26c%3D%C3%A4"),
            "https://www.vesti.bg/a?b=1&c=ä"
        );
    }

    /// Every `Group` bru can name must be a key of `SELECTORS` in `chrome/hints.js`, and the JS
    /// must not offer a group Rust cannot ask for.
    ///
    /// The two lists are in different languages in different files, and the failure when they
    /// drift is silent: `;i` collects nothing and prints "no hintable elements found", which reads
    /// as a page with no images. Checked against the file itself rather than against a copy of it.
    #[test]
    fn every_group_name_is_a_selector_list_the_page_knows() {
        let groups = [
            Group::All,
            Group::Links,
            Group::Images,
            Group::Media,
            Group::Url,
            Group::Inputs,
        ];
        for group in groups {
            assert!(
                HINTS_JS.contains(&format!("\"{}\": [", group.name())),
                "chrome/hints.js has no SELECTORS entry for {:?}",
                group.name()
            );
        }

        // The other direction: count the entries in the object literal. `hints.selectors`'
        // default has exactly these six (configdata.yml:1803).
        let start = HINTS_JS.find("const SELECTORS = {").expect("SELECTORS is gone");
        let end = start + HINTS_JS[start..].find("\n    };").expect("SELECTORS never closes");
        let declared = HINTS_JS[start..end]
            .lines()
            .filter(|line| line.trim_end().ends_with("\": ["))
            .count();
        assert_eq!(declared, groups.len(), "chrome/hints.js declares a group Rust cannot name");
    }

    /// `;i` must ask for a different set of elements than `f`, not the same set filtered.
    #[test]
    fn the_group_selectors_are_qutebrowsers_own() {
        // `links` is anchors *with an href* — the distinction that makes `;o` fill a command line
        // rather than sometimes filling nothing.
        assert!(HINTS_JS.contains("\"a[href]\""));
        assert!(HINTS_JS.contains("\"area[href]\""));
        // `images` is one selector, and the `all` list's `img` is a different entry.
        assert!(HINTS_JS.contains("\"images\": [\n            \"img\",\n        ],"));
        // `inputs` is typed inputs plus contenteditable and textarea — never `input[type=hidden]`,
        // never a button.
        assert!(HINTS_JS.contains("'input[type=\"password\"]'"));
        assert!(HINTS_JS.contains("\"input:not([type])\""));
        assert!(!HINTS_JS.contains("\"inputs\": [\n            \"button\""));
    }

    #[test]
    fn rapid_is_refused_exactly_where_qutebrowser_refuses_it() {
        // hints.py:1027's `no_rapid_targets`, and nothing else — `window` was bru's own addition
        // for as long as it opened a foreground tab, and came out when it stopped.
        assert!(!Target::TabFg.allows_rapid());
        assert!(!Target::Fill(":open {hint-url}".to_string()).allows_rapid());
        assert!(Target::Window.allows_rapid());
        // And the ones `;r` and `<Ctrl-R>` actually use.
        assert!(Target::TabBg.allows_rapid());
        assert!(Target::Normal.allows_rapid());
        assert!(Target::Hover.allows_rapid());
        assert!(Target::Yank.allows_rapid());
        assert!(Target::Download.allows_rapid());
    }

    /// `unique-match` is a different rule from the full match bru followed on before it, and this
    /// is the difference, in the one shape that can show it.
    ///
    /// Nothing `hint_strings` produces has this shape today — see the test below — so without this
    /// one the whole of [`auto_follow`] could be deleted and every other test would still pass.
    #[test]
    fn unique_match_follows_a_label_that_was_never_typed_out() {
        // What `hints.min_chars = 2` (configdata.yml:1752) does to a page with one link: the label
        // is `aa`, and one keystroke leaves it alone on the page without having been typed.
        let unique = AutoFollow::UniqueMatch;
        assert_eq!(auto_follow(unique, &[0], "a"), Some(0), "one label showing is a unique match");
        // `always` would follow this; `unique-match` is `bool(keystr or filterstr)` and does not.
        // It is what stops `f` on a one-link page from clicking before a key is pressed.
        assert_eq!(auto_follow(unique, &[0], ""), None);
        // Two showing is not unique, whatever has been typed.
        assert_eq!(auto_follow(unique, &[3, 7], "l"), None);
        // Nothing showing is not a match either — a chain that named no label.
        assert_eq!(auto_follow(unique, &[], "z"), None);
        // The index answered is the label's, not its position in the visible list.
        assert_eq!(auto_follow(unique, &[41], "jk"), Some(41));
    }

    // --- unhardcoded ---------------------------------------------------------------------------
    /// **The other three values of `hints.auto_follow`, each doing the one thing it names.**
    ///
    /// Written against the rule rather than against the browser, which is what the rule is a
    /// function for. What it cannot say is whether `handle_key` asks it — see
    /// `never_leaves_an_exact_match_standing_for_return`, which reads the source for that, and the
    /// live run in the report.
    #[test]
    fn the_four_values_of_auto_follow_are_four_rules() {
        // `always`: one label showing is enough, typed or not. This is the value under which `f` on
        // a page with one link clicks it without a keypress.
        assert_eq!(auto_follow(AutoFollow::Always, &[0], ""), Some(0));
        assert_eq!(auto_follow(AutoFollow::Always, &[0], "a"), Some(0));
        assert_eq!(auto_follow(AutoFollow::Always, &[3, 7], ""), None, "two is not one");

        // `full-match`: nothing here ever fires. The follow happens on the trie's exact match in
        // `handle_key`, which is the rule bru had before `auto_follow` was a function.
        assert_eq!(auto_follow(AutoFollow::FullMatch, &[0], ""), None);
        assert_eq!(auto_follow(AutoFollow::FullMatch, &[0], "a"), None);

        // `never`: nothing fires here either, and `handle_key` does not fire on an exact match — so
        // the only thing that follows is `hint-follow`.
        assert_eq!(auto_follow(AutoFollow::Never, &[0], "a"), None);
        assert_eq!(auto_follow(AutoFollow::Never, &[0], ""), None);

        // And the default is `unique-match`, which is what bru did before any of this existed.
        assert_eq!(AutoFollow::current(), AutoFollow::UniqueMatch);
    }

    /// **`never` is only worth anything if `handle_key` stops following an exact match**, and that
    /// is a branch inside a function that posts CEF tasks and cannot be called from a test (trap
    /// 13). So this reads the source for the branch, the way
    /// `settings::the_wire_from_the_store_to_the_pill_is_still_connected` reads it for the bar.
    ///
    /// A weak test of a strong kind: it cannot say the branch is right, only that nobody has
    /// deleted it and left `never` following on the last character of the label anyway.
    ///
    /// **It reads the file above `mod tests` and not the whole of it**, and that is not tidiness:
    /// written the obvious way it passed with the branch deleted, because the needle it looks for
    /// is a string literal *in this function* and `contains` found itself. Measured 2026-08-07 —
    /// the branch was cut out of `handle_key`, `cargo test never_leaves` stayed green, and the only
    /// thing the assertion had proved was that it was still written down. That is the round's own
    /// lesson about a check that shares an assumption with its fix, in one line of source.
    #[test]
    fn never_leaves_an_exact_match_standing_for_return() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let whole = std::fs::read_to_string(root.join("src/hints.rs")).expect("readable");
        let source = whole
            .split_once("mod tests {")
            .expect("hints.rs has a test module")
            .0;
        assert!(
            source.contains("Match::Exact(_) if open.auto_follow == AutoFollow::Never"),
            "handle_key follows an exact match under `never`, which leaves <Return> nothing to do"
        );
        let exec = std::fs::read_to_string(root.join("src/exec.rs")).expect("readable");
        assert!(
            exec.contains("Command::HintFollow => crate::hints::follow_current"),
            "hint-follow is inert again"
        );
    }
    // --- end unhardcoded -----------------------------------------------------------------------

    /// **The measurement `<Return>` rests on**, and the reason `unique-match` changes no behaviour
    /// today: with `hints.chars` and `hints.min_chars` at their defaults, a chain that leaves one
    /// label showing has always typed that label out in full.
    ///
    /// Checked over every prefix of every label for 1..1000 elements. The same sweep run in Python
    /// to 1,000,000 found the same thing, and the smallest visible set a non-label prefix can leave
    /// is two — so there is no state in which a hint is sitting followable with a key still to
    /// press, which is what an inert `hint-follow` claims.
    #[test]
    fn unique_match_cannot_beat_a_full_match_under_the_default_hints_chars() {
        use std::collections::HashMap;
        for count in 1..1000usize {
            let labels = hint_strings(count);
            let mut showing: HashMap<String, usize> = HashMap::new();
            for label in &labels {
                for cut in 1..=label.chars().count() {
                    *showing
                        .entry(label.chars().take(cut).collect::<String>())
                        .or_default() += 1;
                }
            }
            let complete: std::collections::HashSet<&String> = labels.iter().collect();
            for (prefix, visible) in &showing {
                if complete.contains(prefix) {
                    // Prefix-freedom, from the other direction: a whole label shows only itself.
                    assert_eq!(*visible, 1, "{count} labels: {prefix:?} is a label and shows {visible}");
                } else {
                    assert!(
                        *visible >= 2,
                        "{count} labels: {prefix:?} shows one label and is not one — \
                         unique-match would follow a key before a full match does"
                    );
                }
            }
        }
    }

    /// One window's session, in the window it belongs to.
    fn open(window: u32, browser: i32) -> Open {
        Open { window, browser, in_hint_mode: true, rapid_window: false }
    }

    /// A key that arrives at the wrong tab of the hinting window ends the session; a key that
    /// arrives at a window with a session of its own is answered by *that* session.
    ///
    /// This is the whole per-window rule. Before it, one `SESSION` for the process meant `f` in a
    /// second window evicted the first window's, and the first window then sat in hint mode with
    /// nothing behind it.
    #[test]
    fn each_window_answers_its_own_key_from_its_own_session() {
        let both = [open(0, 7), open(1, 9)];

        // Two sessions, two windows, and each key lands on its own.
        assert_eq!(Foreign::decide(0, 7, true, &both), Foreign::Same(0));
        assert_eq!(Foreign::decide(1, 9, true, &both), Foreign::Same(1));

        // A key at another *tab* of a hinting window ends that window's session and nobody else's.
        assert_eq!(Foreign::decide(0, 8, true, &both), Foreign::End(0));

        // A window with no session and not in hint mode is not hint mode's business, however many
        // other windows are hinting.
        assert_eq!(Foreign::decide(2, 5, false, &both), Foreign::Ordinary);

        // A session whose page has not answered `f` yet: there is no mode and there are no labels,
        // so the key is the ordinary key it is.
        let waiting = [Open { in_hint_mode: false, ..open(0, 7) }];
        assert_eq!(Foreign::decide(0, 7, false, &waiting), Foreign::Ordinary);
    }

    /// `:mode-enter hint` is a live command and leaves a window in hint mode with no session. Hint
    /// mode swallows what it cannot match, so that state has to be left rather than sat in.
    #[test]
    fn hint_mode_with_no_session_is_left_rather_than_swallowed() {
        assert_eq!(Foreign::decide(0, 7, true, &[]), Foreign::StaleMode(0));
        // And with another window hinting, which is the case the fast path in `handle_key` cannot
        // answer: the stale window is still the one in front, and it is still cleaned up first.
        assert_eq!(
            Foreign::decide(1, 9, true, &[Open { rapid_window: true, ..open(0, 7) }]),
            Foreign::StaleMode(1)
        );
    }

    /// A key that arrives at the wrong *window* ends the session — except for the one session whose
    /// own follows are what sent it there.
    ///
    /// Measured before it was written: with this rule absent, `--hint-script='hint --rapid links
    /// window,a,s,d'` read `mode normal, 0 hints` from the second step on, because a new toplevel
    /// takes the compositor's keyboard focus and `;R` opens one per follow.
    #[test]
    fn only_a_rapid_window_session_survives_the_focus_its_own_follow_moved() {
        // `;R` in window 0, and the key delivered to the window its follow just opened.
        let rapid = [Open { rapid_window: true, ..open(0, 7) }];
        assert_eq!(Foreign::decide(1, 9, false, &rapid), Foreign::AimBack(0, 7));
        // Back in its own window it is an ordinary key of its own session.
        assert_eq!(Foreign::decide(0, 7, true, &rapid), Foreign::Same(0));

        // `;r` (rapid, but into a background tab) opens no window, so a key that arrives elsewhere
        // is not its doing and it does not get the key back.
        let tab_bg = [open(0, 7)];
        assert_eq!(Foreign::decide(1, 9, false, &tab_bg), Foreign::Ordinary);
        // Nor does `wf`: one window, one follow, and the session is over before the key that would
        // land in the new window is pressed — `rapid_window` is false for it by construction.
        assert_eq!(Foreign::decide(1, 9, false, &[open(0, 7)]), Foreign::Ordinary);

        // A rapid-window session whose own window has left hint mode is over; it does not keep
        // pulling keys back.
        let done = [Open { rapid_window: true, in_hint_mode: false, ..open(0, 7) }];
        assert_eq!(Foreign::decide(1, 9, false, &done), Foreign::Ordinary);
    }

    /// Two rapid-window sessions and a key in a third window: nothing in the key says which of them
    /// opened it, so it is refused rather than sent into a page at random.
    #[test]
    fn two_rapid_window_sessions_claim_no_key_between_them() {
        let two = [
            Open { rapid_window: true, ..open(0, 7) },
            Open { rapid_window: true, ..open(1, 9) },
        ];
        assert_eq!(Foreign::decide(2, 5, false, &two), Foreign::Ordinary);
        // Each is still answered in its own window.
        assert_eq!(Foreign::decide(0, 7, true, &two), Foreign::Same(0));
        assert_eq!(Foreign::decide(1, 9, true, &two), Foreign::Same(1));
    }

    #[test]
    fn only_the_two_element_targets_stay_inside_rust() {
        // A target that needs a URL costs a round trip to the page; one that does not must not
        // take it, or `f` — the binding this project is measured on — grows a query per follow.
        assert!(!Target::Normal.needs_url());
        assert!(!Target::Hover.needs_url());
        for target in [
            Target::TabBg,
            Target::TabFg,
            Target::Window,
            Target::Yank,
            Target::YankPrimary,
            Target::Download,
            Target::Fill(":open {hint-url}".to_string()),
        ] {
            assert!(target.needs_url(), "{target:?} has no URL to act on");
        }
    }

    #[test]
    fn the_labels_take_their_colours_from_the_theme() {
        // The shape a generated theme has: a palette, then the `--hints-*` names pointing into it.
        let css = "\
:root {
    --bg: #232929;
    --yellow: #af9e6b;
    --comment: #535d4e;

    /* c.colors.hints.* */
    --hints-fg: var(--bg);
    --hints-bg: var(--yellow);
    --hints-match-fg: var(--comment);
}";
        let style = label_style_json_from(css);
        assert_eq!(
            style,
            "{\"fg\":\"#232929\",\"bg\":\"#af9e6b\",\"match\":\"#535d4e\"}"
        );

        // The theme bru ships with has to work, or the default install draws qutebrowser's yellow.
        let shipped = label_style_json_from(include_str!("../chrome/theme.css"));
        for field in ["fg", "bg", "match"] {
            assert!(shipped.contains(&format!("\"{field}\":\"#")), "{field} missing from {shipped}");
        }

        // A sheet with no `--hints-*` names at all sends nothing, and `chrome/hints.js` falls back
        // to qutebrowser's palette rather than drawing invisible labels.
        assert_eq!(label_style_json_from(":root { --bg: #000; }"), "{}");
    }

    #[test]
    fn a_var_chain_is_followed_and_a_cycle_is_not() {
        let properties = custom_properties(
            "--a: var(--b);\n--b: var(--c);\n--c: #fff;\n--loop: var(--loop);\n--plain: red;",
        );
        assert_eq!(resolve(&properties, "--a", 0).as_deref(), Some("#fff"));
        assert_eq!(resolve(&properties, "--plain", 0).as_deref(), Some("red"));
        assert_eq!(resolve(&properties, "--loop", 0), None, "a cycle must not hang the browser");
        assert_eq!(resolve(&properties, "--missing", 0), None);
    }

    #[test]
    fn a_colour_cannot_break_out_of_the_string_it_is_written_into() {
        // The value is interpolated into a JS string literal and then into `style.cssText`. The
        // file it comes from is themer's, not a page's — but a generator bug should be a missing
        // colour, not a script running in every page bru hints.
        assert!(is_safe_colour("#af9e6b"));
        assert!(is_safe_colour("rgba(255, 247, 133, 0.9)"));
        assert!(!is_safe_colour("\";alert(1);//"));
        assert!(!is_safe_colour("red; background: url(x)"));
        assert!(!is_safe_colour("</script>"));
        assert!(!is_safe_colour(""));
        assert!(!is_safe_colour(&"#".repeat(65)));
    }

    /// **What a per-window session costs the key path.**
    ///
    /// `keys.rs` calls `handle_key` for every key in every mode, and the first thing it does is ask
    /// whether anything is hinting. That question used to be `Option::is_none` on one slot and is now
    /// `HashMap::is_empty` on a map; everything else `handle_key` does — `window_of_browser`, the
    /// `Vec<Open>`, `Foreign::decide` — is below the early return and is never reached by `j`.
    ///
    /// `handle_key` itself cannot be called from a test: it takes a `Browser`, and it posts tasks
    /// (CEF-NOTES trap 13). So this measures the one line of it that `j` runs, against the one line
    /// it replaced, in the same test on the same machine — and against a `HashMap` with two sessions
    /// in it, which is the state the whole workstream exists to allow and is *still* only an
    /// `is_empty`.
    #[test]
    fn the_key_path_cost_of_a_per_window_hint_session() {
        const ROUNDS: u32 = 1_000_000;

        let empty: Mutex<HashMap<u32, u32>> = Mutex::new(HashMap::new());
        let one: Mutex<Option<u32>> = Mutex::new(None);
        let two: Mutex<HashMap<u32, u32>> =
            Mutex::new(HashMap::from([(0u32, 7u32), (1u32, 9u32)]));

        for _ in 0..20_000 {
            std::hint::black_box(std::hint::black_box(&empty).lock().unwrap().is_empty());
        }

        let start = std::time::Instant::now();
        for _ in 0..ROUNDS {
            std::hint::black_box(std::hint::black_box(&empty).lock().unwrap().is_empty());
        }
        let per_window = start.elapsed().as_nanos() as f64 / ROUNDS as f64;

        // What it replaced: one `Mutex<Option<Session>>` for the process.
        let start = std::time::Instant::now();
        for _ in 0..ROUNDS {
            std::hint::black_box(std::hint::black_box(&one).lock().unwrap().is_none());
        }
        let process_wide = start.elapsed().as_nanos() as f64 / ROUNDS as f64;

        // And with two sessions open, which is the case that did not exist before.
        let start = std::time::Instant::now();
        for _ in 0..ROUNDS {
            std::hint::black_box(std::hint::black_box(&two).lock().unwrap().is_empty());
        }
        let two_open = start.elapsed().as_nanos() as f64 / ROUNDS as f64;

        println!(
            "hints: `j` asks whether anything is hinting in {per_window:.3} ns against \
             {process_wide:.3} ns for the one session this replaced, and {two_open:.3} ns with two \
             windows hinting at once"
        );
        // Loose on purpose, like the sibling bounds in `state.rs`: this runs unoptimised under
        // `cargo test` on a machine with other agents on it, and what it catches is an allocation or
        // a scan appearing on the key path, not a nanosecond either way.
        assert!(
            per_window < process_wide * 3.0 + 20.0,
            "the key path must stay free: {per_window:.3} ns against {process_wide:.3} ns"
        );
        assert!(
            two_open < process_wide * 3.0 + 20.0,
            "two open sessions must cost the key path nothing extra: {two_open:.3} ns against \
             {process_wide:.3} ns"
        );
    }

    #[test]
    fn a_token_is_not_guessable_and_not_reused() {
        let a = mint_token();
        let b = mint_token();
        assert_eq!(a.len(), 32);
        assert_ne!(a, b, "two sessions must not share a token");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// **This does not prove the token is unpredictable, and it does not distinguish the CSPRNG from
    /// what came before it.** Run against the old `RandomState` + clock construction on 2026-08-09 it
    /// passed unchanged — `RandomState` reseeds per call, so its prefixes differ too. By this
    /// project's rule a test that passes with and without the fix is worth nothing *as verification
    /// of that fix*, and this one is not offered as such: the property that changed is statistical
    /// and a unit test cannot hold it. The argument for `/dev/urandom` is in `mint_token`'s own
    /// comment, and it is an argument, not a measurement.
    ///
    /// What this is worth keeping for is the failure that would actually happen one day: a mint that
    /// stops varying, or varies only in its tail because a clock has quietly become the only source.
    /// A thousand tokens sharing a prefix is that shape, and nothing else here would notice it.
    #[test]
    fn a_thousand_tokens_share_no_prefix() {
        let tokens: Vec<String> = (0..1000).map(|_| mint_token()).collect();
        let whole: std::collections::HashSet<&String> = tokens.iter().collect();
        assert_eq!(whole.len(), 1000, "a token was minted twice");
        let prefixes: std::collections::HashSet<&str> =
            tokens.iter().map(|token| &token[..8]).collect();
        assert_eq!(
            prefixes.len(),
            1000,
            "two tokens share their first 32 bits — the entropy is not where it should be"
        );
    }
    // The malformed-escape cases — a page ending the browser with `%` before a multi-byte character
    // — moved with the decoder itself into `ipc.rs`, which is where the one copy now lives.
}
