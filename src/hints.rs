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
use std::sync::Mutex;

use crate::bindings::{BindingTrie, Key, KeyInfo, Match, NamedKey};
use crate::modes::Mode;
use crate::state::BruState;
use crate::tabs::SharedState;

/// `hints.chars`, configdata.yml:1723. qutebrowser's default and bru's only value for now — it is
/// the home row, and DESIGN.md's "same keys" makes changing it a config question, not a code one.
pub const CHARS: &str = "asdfghjkl";

/// `hints.min_chars`, configdata.yml:1752.
const MIN_CHARS: usize = 1;

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
    /// When `start` injected the script, so collection can be timed rather than asserted.
    started: std::time::Instant,
}

fn session() -> &'static Mutex<Option<Session>> {
    static SESSION: Mutex<Option<Session>> = Mutex::new(None);
    &SESSION
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

/// `_hint_scattered`, which is what `hints.scatter` (default true) selects.
///
/// Variable-length labels, Vimium-style: as many links as will fit get a label one character
/// shorter than the worst case. The short ones are never a prefix of a long one, because the long
/// ones start at `short_count * len(chars)` — which is what makes an exact match final and lets a
/// hint follow the moment it is complete.
pub fn hint_strings(count: usize) -> Vec<String> {
    if count == 0 {
        return Vec::new();
    }
    let chars: Vec<char> = CHARS.chars().collect();
    let needed = MIN_CHARS.max(ceil_log(count, chars.len()));

    let short_count = if needed > MIN_CHARS && needed > 1 {
        let total_space = chars.len().pow(needed as u32);
        total_space.saturating_sub(count) / (chars.len() - 1)
    } else {
        0
    };
    let long_count = count - short_count;

    let mut strings = Vec::with_capacity(count);
    if needed > 1 {
        for i in 0..short_count {
            strings.push(number_to_hint_str(i, &chars, needed - 1));
        }
    }
    let start = short_count * chars.len();
    for i in start..start + long_count {
        strings.push(number_to_hint_str(i, &chars, needed));
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
    let Some(frame) = browser.main_frame() else {
        return;
    };
    let token = mint_token();

    // Whatever session was open is replaced. Its labels come off this page in `collect`, which
    // begins with the script's own `clear()`; on any other page they went when it was navigated.
    *session().lock().expect("hint session mutex poisoned") = Some(Session {
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
        commands: hint_mode_bindings(state),
        command_sequence: Vec::new(),
        sequence: Vec::new(),
        started: std::time::Instant::now(),
    });

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

/// `<Escape>` in hint mode, and every path that ends a session.
pub fn cancel(state: &SharedState, browser: &mut Browser) {
    let had = session().lock().expect("hint session mutex poisoned").take().is_some();
    if had {
        clear_labels(browser);
    }
    leave_mode(state);
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

fn leave_mode(state: &SharedState) {
    let now = {
        let mut guard = state.lock().expect("state mutex poisoned");
        if guard.mode() != Mode::Hint {
            return;
        }
        guard.leave_mode();
        guard.mode()
    };
    crate::ipc::set_mode(now.name().to_string());
    crate::ipc::set_keystring(String::new());
}

/// A token no page can guess, so that the only `hints` answer bru believes is the one it asked for.
///
/// Not a cryptographic RNG — bru has no dependency for one and this is not a key. It is a nonce
/// against a page volunteering coordinates, and it is combined with two other checks (an open
/// session, and the right browser).
fn mint_token() -> String {
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
/// into a failed query. Three things have to line up: a session is open, it belongs to the browser
/// the query came from, and it carries the token bru minted for it.
pub fn on_page_query(browser: Option<&Browser>, request: &str) -> bool {
    let Some(browser) = browser else {
        return false;
    };
    let id = browser.identifier();
    let (Some(token), Some(kind), Some(data)) = (
        field(request, "token"),
        field(request, "kind"),
        field(request, "data").map(|d| percent_decode(&d)),
    ) else {
        return false;
    };

    {
        let guard = session().lock().expect("hint session mutex poisoned");
        let Some(open) = guard.as_ref() else {
            return false;
        };
        if open.browser_id != id || open.token != token {
            return false;
        }
    }

    let Some(state) = BruState::instance() else {
        return false;
    };
    let mut browser = browser.clone();

    match kind.as_str() {
        "elems" => on_collected(&state, &mut browser, &data),
        "href" => on_href(&state, &mut browser, &data),
        _ => return false,
    }
    true
}

/// The page has reported its hintable elements. Generate the labels, draw them, enter hint mode.
fn on_collected(state: &SharedState, browser: &mut Browser, data: &str) {
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
        let guard = session().lock().expect("hint session mutex poisoned");
        (
            guard.as_ref().map(|s| s.started.elapsed()),
            guard.as_ref().map(|s| s.group.name()).unwrap_or("?"),
            guard.as_ref().map(|s| s.target.describe()).unwrap_or("?"),
        )
    };
    eprintln!(
        "bru[hints]: group {group}: {} elements in {:.1} ms ({} bytes of payload) — {describe}",
        points.len(),
        elapsed.map(|e| e.as_secs_f64() * 1000.0).unwrap_or(0.0),
        data.len(),
    );

    if points.is_empty() {
        // qutebrowser: message.error("No elements found."), and no mode change.
        *session().lock().expect("hint session mutex poisoned") = None;
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

    let first = {
        let mut guard = session().lock().expect("hint session mutex poisoned");
        let Some(open) = guard.as_mut() else {
            return;
        };
        open.points = points;
        open.labels = labels.clone();
        open.trie = trie;
        open.sequence.clear();
        open.first
    };

    debug(&format!("labels {}", labels.join(" ")));

    let list = labels
        .iter()
        .map(|label| format!("\"{label}\""))
        .collect::<Vec<_>>()
        .join(",");
    show(
        browser,
        &format!("window.__bru_hints.show([{list}],{});", label_style_json()),
    );

    let entered = state
        .lock()
        .expect("state mutex poisoned")
        .enter_mode(Mode::Hint, false);
    if entered {
        crate::ipc::set_mode(Mode::Hint.name().to_string());
        crate::ipc::set_keystring(String::new());
    }

    // `--first` (`gi`): follow element 0 without waiting for a key. `hints.py:_start_cb` fires
    // `strings[0]`, which is the label of the *first element* — the labels are scattered, so the
    // index is 0 and the label is not necessarily "a".
    if first {
        follow(state, browser, 0);
    }
}

/// The page has reported the URL behind a followed hint — every target except `normal` and `hover`.
fn on_href(state: &SharedState, browser: &mut Browser, url: &str) {
    let Some((target, rapid, first_run)) = ({
        let guard = session().lock().expect("hint session mutex poisoned");
        guard.as_ref().map(|s| (s.target.clone(), s.rapid, s.first_run))
    }) else {
        return;
    };

    // The session ends *here* and not in `follow`, because `clear()` on the page drops the element
    // array along with the labels — it is the teardown, not a repaint — and `href` would then be
    // looking up an index in an empty array. Measured 2026-08-06: clearing first reported no URL
    // for every link on the page.
    finish(state, browser, rapid);

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
    eprintln!("bru[hints]: {} -> {url}", target.describe());
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

/// `{"fg":…,"bg":…,"border":…,"match":…}` for `chrome/hints.js`'s `show`, out of the theme's
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
        // qutebrowser's `hints.border` is `1px solid #E3BE23` and no generated theme sets it, so
        // the outline takes the label's own foreground: in-theme by construction, and visible
        // against the background whatever the palette does, because it is the colour the text on
        // that background is already using.
        ("border", "--hints-fg"),
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
    if state.lock().expect("state mutex poisoned").mode() != Mode::Hint {
        return None;
    }
    debug(&format!("key {info}"));

    // A session belongs to one tab, and `--rapid` is the first thing that can outlive a tab switch.
    // If the key arrived at a different browser the labels are usually on a page nobody is looking
    // at, and matching against them would follow a link the user cannot see. End the session and
    // answer `None`, so `keys.rs` treats this as the ordinary key it now is.
    //
    // (The stale labels stay on the other tab until its next `collect`, which begins with `clear`,
    // or until it navigates. Reaching across to remove them would mean holding that tab's `Browser`
    // for the length of the session, and a live reference is what stops a tab from closing —
    // CEF-NOTES, Tabs.)
    //
    // **`;R` is the one session that has to survive it, and the reason is the session's own doing.**
    // `hint --rapid links window` opens a window per follow; a new toplevel takes the compositor's
    // keyboard focus, so the second label of every rapid window session is delivered to the window
    // the *first* one made. Ending on that is `;f` with extra steps — measured 2026-08-06, and both
    // the plain change and a focus-back from the follow task are in the report. So for that one
    // combination the key is aimed back at the browser the labels are drawn on, which is the same
    // decision `keys.rs` already makes for a key that lands on a chrome strip (CEF-NOTES trap 11).
    // `<Escape>` below is aimed there too, so the session is still one key from over.
    let foreign = {
        let guard = session().lock().expect("hint session mutex poisoned");
        match guard.as_ref() {
            None => Foreign::Same,
            Some(open) => Foreign::decide(
                open.browser_id,
                browser.identifier(),
                open.rapid,
                &open.target,
            ),
        }
    };
    let mut hinting;
    let browser: &mut Browser = match foreign {
        Foreign::Same => browser,
        Foreign::End => {
            *session().lock().expect("hint session mutex poisoned") = None;
            leave_mode(state);
            return None;
        }
        Foreign::AimBack(id) => {
            let found = state.lock().expect("state mutex poisoned").browser_with_id(id);
            match found {
                Some(found) => {
                    debug(&format!("key aimed back at browser {id} for a rapid window session"));
                    hinting = found;
                    &mut hinting
                }
                // The hinting tab has gone since the last follow. Nothing to draw on and nothing to
                // click; this is the ordinary stale case after all.
                None => {
                    *session().lock().expect("hint session mutex poisoned") = None;
                    leave_mode(state);
                    return None;
                }
            }
        }
    };

// --- per-window mode -----------------------------------------------------------------------
    // Hint mode with **no** session at all, which is the other half of the same question now that
    // a mode belongs to one window: there is one `SESSION` for the process, so `f` in a second
    // window replaces the first window's, and the first window is then left in hint mode with
    // nothing behind it. The old answer was further down — "nothing can match, and the keys must
    // not reach the page", i.e. swallow everything until `<Escape>` — and while the mode was
    // process-wide that state could not last, because the other window's next key left hint mode
    // for both. It can last now. So it is ended here instead: leave the mode and answer `None`, and
    // the key goes on to be the ordinary key it is.
    let sessionless = session()
        .lock()
        .expect("hint session mutex poisoned")
        .is_none();
    if sessionless {
        leave_mode(state);
        return None;
    }
// --- end per-window mode -------------------------------------------------------------------

    // `<Escape>` is `mode-leave` in the `hint:` table, and it is taken *before* that table rather
    // than through it: `exec`'s `mode-leave` knows how to leave a mode and nothing about a hint
    // session, so running it would leave the labels drawn over the page and the session open behind
    // them. qutebrowser does not need the special case because `HintManager.on_mode_left` is wired
    // to the mode manager's signal; bru has no such signal, and one function is cheaper than one.
    if info.key == Key::Named(NamedKey::Escape) {
        cancel(state, browser);
        return Some(true);
    }

    // `HintKeyParser.handle` (modeparsers.py:196): the command parser gets a dry run first, and
    // only a key it does not want is offered to the labels. This is what makes `<Ctrl-F>`
    // (`hint links`), `<Ctrl-R>` (`hint --rapid links tab-bg`) and `<Ctrl-B>` (`hint all tab-bg`)
    // work from inside hint mode. Hint labels are spelled out of `hints.chars`, so nothing here can
    // shadow a label.
    let binding = {
        let mut guard = session().lock().expect("hint session mutex poisoned");
        match guard.as_mut() {
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
        let mut guard = session().lock().expect("hint session mutex poisoned");
        if let Some(open) = guard.as_mut() {
            open.sequence.pop();
        }
        drop(guard);
        redraw(browser);
        return Some(true);
    }

    let outcome = {
        let mut guard = session().lock().expect("hint session mutex poisoned");
        let Some(open) = guard.as_mut() else {
            // In hint mode with no session: nothing can match, and the keys must not reach the page.
            return Some(true);
        };
        open.sequence.push(info);
        match open.trie.matches(&open.sequence) {
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
        Outcome::Follow(index) => follow(state, browser, index),
        Outcome::Pending | Outcome::NoMatch => redraw(browser),
    }
    Some(true)
}

enum Outcome {
    Follow(usize),
    Pending,
    NoMatch,
}

/// Where a key that reached hint mode should be acted on — see the block in [`handle_key`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Foreign {
    /// It arrived at the browser the labels are on.
    Same,
    /// It arrived somewhere else, and the session goes.
    End,
    /// It arrived somewhere else because the session put it there. Aim it at this browser.
    AimBack(i32),
}

impl Foreign {
    /// The rule, with no CEF in it, so that it can be asserted in a unit test — `handle_key` posts
    /// tasks and cannot be called from one (CEF-NOTES trap 13).
    fn decide(session_browser: i32, key_browser: i32, rapid: bool, target: &Target) -> Foreign {
        if session_browser == key_browser {
            Foreign::Same
        } else if rapid && *target == Target::Window {
            Foreign::AimBack(session_browser)
        } else {
            Foreign::End
        }
    }
}

/// Push the current chain to the status bar and to the page's labels.
///
/// The visible set is computed here, in Rust, and sent as a list of indices. The page is told which
/// labels to show; it is never asked which ones match.
fn redraw(browser: &mut Browser) {
    let (keystring, visible, matched_len) = {
        let guard = session().lock().expect("hint session mutex poisoned");
        let Some(open) = guard.as_ref() else {
            return;
        };
        let typed = crate::bindings::sequence_to_string(&open.sequence);
        let visible: Vec<String> = open
            .labels
            .iter()
            .enumerate()
            .filter(|(_, label)| label.starts_with(&typed))
            .map(|(index, _)| index.to_string())
            .collect();
        (typed.clone(), visible, typed.chars().count())
    };

    crate::ipc::set_keystring(keystring);
    show(
        browser,
        &format!(
            "window.__bru_hints && window.__bru_hints.filter([{}],{matched_len});",
            visible.join(",")
        ),
    );
}

/// A hint matched. The element's position is enough for `normal` and `hover`; every other target
/// needs its URL, which only the page can give, so those ask and continue in [`on_href`].
fn follow(state: &SharedState, browser: &mut Browser, index: usize) {
    let (target, rapid, point, token) = {
        let guard = session().lock().expect("hint session mutex poisoned");
        let Some(open) = guard.as_ref() else {
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
    finish(state, browser, rapid);

    match target {
        Target::Normal => {
            eprintln!("bru[hints]: clicking hint {index} at ({}, {})", point.0, point.1);
            click(browser, point.0, point.1);
        }
        // `;h`. qutebrowser calls `elem.hover()`, which dispatches a synthetic `mouseover`; bru
        // moves the real pointer instead, for the same reason the follow is a real click — a
        // synthetic event skips everything that checks `isTrusted`, and a menu that opens on hover
        // is exactly the kind of thing that checks.
        Target::Hover => {
            eprintln!("bru[hints]: hovering hint {index} at ({}, {})", point.0, point.1);
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
fn finish(state: &SharedState, browser: &mut Browser, rapid: bool) {
    if !rapid {
        *session().lock().expect("hint session mutex poisoned") = None;
        clear_labels(browser);
        leave_mode(state);
        return;
    }

    {
        let mut guard = session().lock().expect("hint session mutex poisoned");
        let Some(open) = guard.as_mut() else {
            return;
        };
        open.sequence.clear();
        open.first_run = false;
    }
    redraw(browser);
}

/// The follow, on Chromium's real input path.
///
/// A move first, because hover state is what a page's own handlers look at and a press with no
/// preceding move arrives at an element that was never entered. Then press and release, one click.
fn click(browser: &mut Browser, x: i32, y: i32) {
    let Some(host) = browser.host() else {
        return;
    };
    let event = MouseEvent { x, y, modifiers: 0 };
    host.send_mouse_move_event(Some(&event), 0);
    host.send_mouse_click_event(Some(&event), MouseButtonType::LEFT, 0, 1);
    host.send_mouse_click_event(Some(&event), MouseButtonType::LEFT, 1, 1);
}

/// `;h` — the move without the press. `mouse_leave = 0` means the pointer is entering, which is
/// what raises `mouseover`/`:hover` on the element and everything nested under it.
fn hover(browser: &mut Browser, x: i32, y: i32) {
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

/// Undo `encodeURIComponent`. Only the bytes it escapes appear, and it always emits `%XX` pairs.
fn percent_decode(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&src[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
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
/// | anything else | fed to the key parser one character at a time |
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
            let (mode, tabs, active, windows) = {
                let guard = state.lock().expect("state mutex poisoned");
                (guard.mode(), guard.tab_count(), guard.active_tab(), guard.window_count())
            };
            let guard = session().lock().expect("hint session mutex poisoned");
            eprintln!(
                "hint-script: after {:?} -> mode {mode}, {} hints, chain {:?}, \
                 tabs={tabs} active={active} windows={windows}, url={}",
                self.step,
                guard.as_ref().map(|s| s.labels.len()).unwrap_or(0),
                guard
                    .as_ref()
                    .map(|s| crate::bindings::sequence_to_string(&s.sequence))
                    .unwrap_or_default(),
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

    /// A key that arrives at the wrong browser ends the session — except for the one session whose
    /// own follows are what sent it there.
    ///
    /// Measured before it was written: with this rule absent, `--hint-script='hint --rapid links
    /// window,a,s,d'` read `mode normal, 0 hints` from the second step on, because a new toplevel
    /// takes the compositor's keyboard focus and `;R` opens one per follow.
    #[test]
    fn only_a_rapid_window_session_survives_the_focus_its_own_follow_moved() {
        // The ordinary case, and the one the check was written for: a tab switch during `;r`.
        assert_eq!(Foreign::decide(7, 7, true, &Target::TabBg), Foreign::Same);
        assert_eq!(Foreign::decide(7, 9, true, &Target::TabBg), Foreign::End);
        assert_eq!(Foreign::decide(7, 9, false, &Target::Normal), Foreign::End);
        // `wf` is not rapid: one window, one follow, and the session is over before the key that
        // would land in the new window is pressed.
        assert_eq!(Foreign::decide(7, 9, false, &Target::Window), Foreign::End);
        // `;R`, and only `;R`.
        assert_eq!(Foreign::decide(7, 9, true, &Target::Window), Foreign::AimBack(7));
        assert_eq!(Foreign::decide(7, 7, true, &Target::Window), Foreign::Same);
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
            "{\"fg\":\"#232929\",\"bg\":\"#af9e6b\",\"match\":\"#535d4e\",\"border\":\"#232929\"}"
        );

        // The theme bru ships with has to work, or the default install draws qutebrowser's yellow.
        let shipped = label_style_json_from(include_str!("../chrome/theme.css"));
        for field in ["fg", "bg", "match", "border"] {
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

    #[test]
    fn a_token_is_not_guessable_and_not_reused() {
        let a = mint_token();
        let b = mint_token();
        assert_eq!(a.len(), 32);
        assert_ne!(a, b, "two sessions must not share a token");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
