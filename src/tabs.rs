//! Tabs.
//!
//! CEF's Views framework has no tab control, and bru does not want one — DESIGN.md draws the strip
//! itself. A tab is a `BrowserView` that is a child of the window like any other; switching is
//! `set_visible`, so the renderer of the tab you left stays alive and coming back to it is
//! instant. Nothing is reloaded and no state is serialised.
//!
//! **No CEF call is made while the state mutex is held**, and that is not a style preference.
//! Measured 2026-08-06: `window.add_child_view_at` on a browser view makes CEF create the browser
//! *synchronously*, which calls straight back into `LifeSpanHandler::on_after_created`, which takes
//! this same mutex — bru hung on its first frame with the window never mapped. So each operation
//! here reads what it needs under a short lock, lets go, and only then talks to CEF.

use cef::*;
use std::sync::{Arc, Mutex};

use crate::state::BruState;
use crate::window::BruBrowserViewDelegate;

pub type SharedState = Arc<Mutex<BruState>>;

/// One tab, as the strip draws it — the four fields a label can be built from, copied out of the
/// state.
///
/// **A copy rather than a borrow, and that is the whole point.** See [`render_tabs`].
pub struct TabRow {
    pub title: String,
    pub url: String,
    pub pinned: bool,
    pub muted: bool,
    /// `{id}` in a title format is the browser's, and a tab that has not been created yet has none.
    pub browser_id: Option<i32>,
}

/// A window's tabs, taken under the state lock and rendered without it.
#[derive(Default)]
pub struct TabsSnapshot {
    rows: Vec<TabRow>,
    active: usize,
}

pub struct Tab {
    pub(crate) view: BrowserView,
    /// Learned from `BrowserViewDelegate::on_browser_created`, not at creation: `browser_view_create`
    /// returns before the browser exists, so this is `None` for the moment in between.
    pub(crate) browser_id: Option<i32>,
    pub(crate) title: String,
    pub(crate) url: String,
    /// `<Ctrl-p>` — a tab that keeps its place and does not get closed by accident. It is bru's own
    /// flag and nothing in CEF knows about it; what it changes is `close_current`, `close_others`
    /// and the class the strip draws.
    pub(crate) pinned: bool,
    /// `<Alt-m>` — the mirror of `host.is_audio_muted()`. Kept here as well as asked of CEF because
    /// the strip is rendered from this struct and a tab's browser may not exist yet, and because a
    /// restored session has to re-apply it to a browser that has not been made.
    pub(crate) muted: bool,
}

/// The plain state operations. None of these touch CEF.
///
/// Every one of them that names no window acts on the **current** one — `state.rs` keeps which that
/// is. That is what let a second window arrive without `session.rs`, `hints.rs`, `completers.rs`,
/// `spawn.rs`, `settings.rs`, `scroll.rs` or `history.rs` changing a line: they all ask about "the
/// tabs" and mean the ones in front of the user. The three that key off a *browser* instead —
/// [`BruState::set_tab_url`], [`BruState::set_tab_title`] and [`BruState::is_active_browser`] —
/// search every window, because a page in a background window still reports its title.
impl BruState {
    /// The current window's tab views.
    pub fn tab_views(&self) -> Vec<BrowserView> {
        self.current_slot()
            .map(|slot| slot.tabs.iter().map(|tab| tab.view.clone()).collect())
            .unwrap_or_default()
    }

    pub fn tab_views_in(&self, window: u32) -> Vec<BrowserView> {
        self.slot(window)
            .map(|slot| slot.tabs.iter().map(|tab| tab.view.clone()).collect())
            .unwrap_or_default()
    }

    /// Ties a browser to the tab whose view it was made for. Called once per tab, and across every
    /// window: a view knows nothing about which window it was created for.
    pub fn note_tab_browser(&mut self, view: &mut BrowserView, identifier: i32) {
        for slot in &mut self.windows {
            for tab in &mut slot.tabs {
                if tab.view.is_same(Some(&mut View::from(&*view))) != 0 {
                    tab.browser_id = Some(identifier);
                    return;
                }
            }
        }
    }

    /// Which window a browser's tab is in, and where in that window's strip. Searches every window.
    fn locate_tab(&self, identifier: i32) -> Option<(u32, usize)> {
        self.windows.iter().find_map(|slot| {
            slot.tabs
                .iter()
                .position(|tab| tab.browser_id == Some(identifier))
                .map(|index| (slot.id, index))
        })
    }

    /// True when `identifier` is the browser of the tab showing **in its own window** — which is the
    /// only tab whose address and title belong in that window's status line. A background window's
    /// showing tab answers true, and rightly: it is what that window's bar has to say.
    pub fn is_active_browser(&self, identifier: i32) -> bool {
        match self.locate_tab(identifier) {
            Some((window, index)) => {
                self.slot(window).map(|slot| slot.active) == Some(index)
            }
            None => false,
        }
    }

    /// The browser id of the showing tab, if it has one yet.
    pub fn active_tab_browser_id(&self) -> Option<i32> {
        self.current_slot()
            .and_then(|slot| slot.tabs.get(slot.active))
            .and_then(|tab| tab.browser_id)
    }

    /// Records a tab's address. Answers **which window** the tab is in, and `None` when the browser
    /// is not a tab at all — which is how a chrome strip reporting its own bru:// URL is kept out of
    /// the status line, and how the display handler knows whose bar to push into.
    pub fn set_tab_url(&mut self, identifier: i32, url: String) -> Option<u32> {
        let (window, index) = self.locate_tab(identifier)?;
        self.slot_mut(window)?.tabs[index].url = url;
        Some(window)
    }

    pub fn set_tab_title(&mut self, identifier: i32, title: String) -> Option<u32> {
        let (window, index) = self.locate_tab(identifier)?;
        self.slot_mut(window)?.tabs[index].title = title;
        Some(window)
    }

    /// What the current window's tab strip renders, as data — **not** as the rendered JSON.
    ///
    /// See [`render_tabs`] for why the two are separate. In short: rendering can run a Lua function
    /// per tab, these are `BruState` methods, and every caller holds the state lock across them.
    pub fn tabs_snapshot(&self) -> TabsSnapshot {
        match self.current_window_id() {
            Some(window) => self.tabs_snapshot_in(window),
            None => TabsSnapshot::default(),
        }
    }

    /// The same for a named window. Two windows draw two strips, and a push into the wrong one is
    /// how a background window ends up listing the tabs of the one in front of it.
    pub fn tabs_snapshot_in(&self, window: u32) -> TabsSnapshot {
        let Some(slot) = self.slot(window) else {
            return TabsSnapshot::default();
        };
        TabsSnapshot {
            rows: slot
                .tabs
                .iter()
                .map(|tab| TabRow {
                    title: tab.title.clone(),
                    url: tab.url.clone(),
                    pinned: tab.pinned,
                    muted: tab.muted,
                    browser_id: tab.browser_id,
                })
                .collect(),
            active: slot.active,
        }
    }


    /// Whether the tab at `index` keeps its place — what `tab-close` and `tab-only` consult before
    /// they take a tab away.
    pub fn tab_pinned(&self, index: usize) -> bool {
        self.current_slot()
            .and_then(|slot| slot.tabs.get(index))
            .map(|tab| tab.pinned)
            .unwrap_or(false)
    }

    pub fn tab_muted(&self, index: usize) -> bool {
        self.current_slot()
            .and_then(|slot| slot.tabs.get(index))
            .map(|tab| tab.muted)
            .unwrap_or(false)
    }

    /// Flip the pin on the tab at `index` and answer what it became. `tab-pin` is a toggle in
    /// qutebrowser (`commands.py:278`), not a setter.
    pub fn toggle_tab_pinned(&mut self, index: usize) -> bool {
        match self.current_slot_mut().and_then(|slot| slot.tabs.get_mut(index)) {
            Some(tab) => {
                tab.pinned = !tab.pinned;
                tab.pinned
            }
            None => false,
        }
    }

    pub fn set_tab_pinned(&mut self, index: usize, pinned: bool) {
        if let Some(tab) = self.current_slot_mut().and_then(|slot| slot.tabs.get_mut(index)) {
            tab.pinned = pinned;
        }
    }

    /// Flip the mute flag and answer what it became. The CEF call that acts on it is
    /// [`toggle_mute`], outside the lock.
    pub fn toggle_tab_muted(&mut self, index: usize) -> bool {
        match self.current_slot_mut().and_then(|slot| slot.tabs.get_mut(index)) {
            Some(tab) => {
                tab.muted = !tab.muted;
                tab.muted
            }
            None => false,
        }
    }

    pub fn set_tab_muted(&mut self, index: usize, muted: bool) {
        if let Some(tab) = self.current_slot_mut().and_then(|slot| slot.tabs.get_mut(index)) {
            tab.muted = muted;
        }
    }

    /// The browser identifier of each tab, in strip order. `None` for a tab whose browser CEF has
    /// not made yet. Sessions need every tab's browser, not only the showing one's.
    pub fn tab_browser_ids(&self) -> Vec<Option<i32>> {
        self.current_slot()
            .map(|slot| slot.tabs.iter().map(|tab| tab.browser_id).collect())
            .unwrap_or_default()
    }

    /// [`Self::tab_browser_ids`] for a named window rather than the current one.
    ///
    /// `scrollbar::reinject_everywhere` needs every tab of *every* window, and the current-window
    /// form would have silently done one window's worth of work and reported nothing.
    pub fn tab_browser_ids_in(&self, window: u32) -> Vec<Option<i32>> {
        self.slot(window)
            .map(|slot| slot.tabs.iter().map(|tab| tab.browser_id).collect())
            .unwrap_or_default()
    }

    pub fn tab_count(&self) -> usize {
        self.current_slot().map(|slot| slot.tabs.len()).unwrap_or(0)
    }

    // --- src/remote.rs -------------------------------------------------------------------------
    /// One window's tabs as `(url, title)`, in strip order.
    ///
    /// For `bru --remote tabs`, which is how `lvim-tex`'s `is_alive` asks whether the PDF it opened
    /// is still on screen. The views and the browsers stay behind this: a caller over a socket wants
    /// two strings, and handing out a `BrowserView` to answer that would be handing out the tab.
    pub fn tabs_in(&self, window: u32) -> Vec<(String, String)> {
        self.slot(window)
            .map(|slot| {
                slot.tabs.iter().map(|tab| (tab.url.clone(), tab.title.clone())).collect()
            })
            .unwrap_or_default()
    }
    // --- end src/remote.rs ---------------------------------------------------------------------

    pub fn tab_count_in(&self, window: u32) -> usize {
        self.slot(window).map(|slot| slot.tabs.len()).unwrap_or(0)
    }

    pub fn active_tab(&self) -> usize {
        self.current_slot().map(|slot| slot.active).unwrap_or(0)
    }

    pub fn active_tab_in(&self, window: u32) -> usize {
        self.slot(window).map(|slot| slot.active).unwrap_or(0)
    }

    pub fn set_active_in(&mut self, window: u32, index: usize) {
        if let Some(slot) = self.slot_mut(window) {
            if index != slot.active {
                slot.last_active = Some(slot.active);
            }
            slot.active = index;
        }
    }

    /// Where `tab-focus last` goes. `None` until a second tab has been shown.
    pub fn last_active_tab(&self) -> Option<usize> {
        self.current_slot()
            .and_then(|slot| slot.last_active.filter(|index| *index < slot.tabs.len()))
    }

    /// The address of a tab, as the display handler last reported it.
    /// The title of the tab at `index`, for the status line on a switch.
    pub fn tab_title(&self, index: usize) -> Option<String> {
        self.current_slot()
            .and_then(|slot| slot.tabs.get(index))
            .map(|tab| tab.title.clone())
    }

    pub fn tab_url(&self, index: usize) -> Option<String> {
        self.current_slot()
            .and_then(|slot| slot.tabs.get(index))
            .map(|tab| tab.url.clone())
    }

    pub fn tab_url_in(&self, window: u32, index: usize) -> Option<String> {
        self.slot(window)
            .and_then(|slot| slot.tabs.get(index))
            .map(|tab| tab.url.clone())
    }

    pub fn tab_title_in(&self, window: u32, index: usize) -> Option<String> {
        self.slot(window)
            .and_then(|slot| slot.tabs.get(index))
            .map(|tab| tab.title.clone())
    }

    /// The `depth`-th most recently closed tab's URL, removed from the undo stack. Depth 1 is the
    /// tab closed last, which is what a bare `u` wants; `2u` reaches the one before it
    /// (`commands.py:831-861`, where the count *is* the depth).
    pub fn take_closed_tab(&mut self, depth: usize) -> Option<String> {
        let index = self.closed.len().checked_sub(depth.max(1))?;
        Some(self.closed.remove(index))
    }

    /// Moves the showing tab to `to`, keeping it the showing one.
    ///
    /// Only bru's own order changes; the tabs' order among the window's children does not, and does
    /// not need to. One tab is visible at a time, so the window stacks nothing — what the strip
    /// draws comes from [`BruState::tabs_json`], which reads this vector.
    pub fn move_tab(&mut self, from: usize, to: usize) {
        let Some(slot) = self.current_slot_mut() else {
            return;
        };
        if from >= slot.tabs.len() || to >= slot.tabs.len() || from == to {
            return;
        }
        let tab = slot.tabs.remove(from);
        slot.tabs.insert(to, tab);
        slot.active = to;
        slot.last_active = None;
    }

    /// Removes every tab but the showing one and hands their views back to be dropped.
    ///
    /// A pinned tab survives unless `force`. qutebrowser's `:tab-only` takes `--pinned
    /// prompt|close|keep` and defaults to `prompt` (`commands.py:780-826`); bru has no yes/no mode
    /// to prompt in, so the default is the answer a prompt would most likely get — keep it — and
    /// `-f` is the way to say otherwise.
    pub fn take_other_tabs(&mut self, force: bool) -> Vec<BrowserView> {
        let Some(slot) = self.current_slot_mut() else {
            return Vec::new();
        };
        if slot.tabs.is_empty() {
            return Vec::new();
        }
        let active = slot.active;
        let mut taken = Vec::new();
        let mut kept = Vec::new();
        let mut closed = Vec::new();
        let mut new_active = 0;
        for (index, tab) in std::mem::take(&mut slot.tabs).into_iter().enumerate() {
            if index == active {
                new_active = kept.len();
                kept.push(tab);
            } else if tab.pinned && !force {
                kept.push(tab);
            } else {
                closed.push(tab.url.clone());
                taken.push(tab.view.clone());
            }
        }
        slot.tabs = kept;
        slot.active = new_active;
        slot.last_active = None;
        self.closed.extend(closed);
        taken
    }

    /// Appends a tab to a named window and answers its index in that window's strip. `None` when
    /// there is no such window — a `:tab-give 7` typed at a window that has since closed.
    pub fn push_tab_in(&mut self, window: u32, view: BrowserView) -> Option<usize> {
        let slot = self.slot_mut(window)?;
        slot.tabs.push(Tab {
            view,
            browser_id: None,
            title: String::new(),
            url: String::new(),
            pinned: false,
            muted: false,
        });
        Some(slot.tabs.len() - 1)
    }

    /// Removes the showing tab and moves the selection to the one that takes its place.
    pub fn take_active_tab(&mut self) -> Option<BrowserView> {
        let window = self.current_window_id()?;
        let tab = self.detach_active_tab_in(window)?;
        // Kept so `u` can open it again. Only the URL — see `BruState::closed`.
        self.closed.push(tab.url.clone());
        Some(tab.view)
    }

    /// The same removal without the undo entry: the tab is not being closed, it is being handed to
    /// another window. Whole, because `tab-give` has to put it back somewhere — a `BrowserView`
    /// alone would lose the pin, the mute and the browser id.
    ///
    /// **The window is named rather than assumed**, and that is not tidiness: `window::create` makes
    /// the window it opens current, so a `gD` that detaches into a new one would otherwise take the
    /// showing tab of the window it had just made. Measured — the first run of `gD` reported
    /// `windows=[0:2 1:0]`, the tab still in the window it was supposed to leave.
    pub(crate) fn detach_active_tab_in(&mut self, window: u32) -> Option<Tab> {
        let slot = self.slot_mut(window)?;
        if slot.tabs.is_empty() {
            return None;
        }
        let tab = slot.tabs.remove(slot.active);
        if slot.active >= slot.tabs.len() && !slot.tabs.is_empty() {
            slot.active = slot.tabs.len() - 1;
        }
        slot.last_active = None;
        Some(tab)
    }

    /// Puts a whole tab into a named window, keeping everything it carried, and answers its index.
    pub(crate) fn attach_tab_in(&mut self, window: u32, tab: Tab) -> Option<usize> {
        let slot = self.slot_mut(window)?;
        slot.tabs.push(tab);
        Some(slot.tabs.len() - 1)
    }
}

/// Opens a tab on `url`. `background` leaves the current one showing, the way qutebrowser's
/// `:open -b` does.
pub fn new_tab(state: &SharedState, url: &str, background: bool) {
    let target = state
        .lock()
        .expect("state mutex poisoned")
        .current_window_id();
    if let Some(target) = target {
        new_tab_in(state, target, url, background);
    }
}

// --- tabs and statusbar ------------------------------------------------------------------------

/// A strip with nothing in it, in the shape the strip with something in it has.
///
/// One string rather than three literals: the payload gained three keys when the presentation
/// settings arrived, and a fallback that kept the old two-key shape would leave a window with no
/// tabs drawing its titles left-aligned no matter what `tabs.title.alignment` says — for exactly as
/// long as it took the first tab to appear.
/// Turn a window's tabs into the JSON its strip draws — **outside the state lock**, because this
/// can run a Lua function.
///
/// `tabs.title.format` may be a function, and it is called once per tab. It used to be called from
/// a `BruState` method, which every caller reaches with the state mutex held: `push_tabs_everywhere`
/// locks and maps every window through it, and so do `select_in`, `close_others`, `toggle_pin`,
/// `move_current` and `new_tab_in`. Rust's `Mutex` is not reentrant, so any `bru.*` binding that
/// took `BruState` synchronously would have hung the browser on the next strip rebuild.
///
/// **That is not hypothetical here.** Measured 2026-08-07: a `config.lua` whose `tabs.title.format`
/// indexed a nil field printed its error inline, the error reached `ipc::push_bar`, which takes the
/// same mutex to find the window in front, and the process sat there until `--close-after-ms`. The
/// remedy then was to post that one message (`settings.rs`, the `FN_ERROR_SINK` comment). This is
/// the same hazard from the other side, fixed by construction rather than one entry point at a time:
/// the state hands over data, and everything that can call into Lua happens after the lock is gone.
///
/// It is also the shape `ipc::set_mode_for` already uses for the same reason, and the one
/// `bar_json_for` uses for the bar.
pub fn render_tabs(snapshot: &TabsSnapshot) -> String {
    // --- tabs and statusbar ----------------------------------------------------------------
    // Read once for the whole strip rather than once per tab: `text_of` takes the settings
    // mutex, and a window with twenty tabs would take it eighty times for four answers that
    // cannot change in between.
    // --- setting functions -----------------------------------------------------------------
    // **Read once, called per tab**, and the two halves of that sentence are two different
    // costs. Reading takes the settings mutex — 43.7 ns — and neither answer can change between
    // the first tab and the twentieth, which is why this line was already outside the loop. A
    // *function* is the other thing: what it answers depends on the tab, so it has to run once
    // per tab, and `Template` is the split that lets the store be read once and the function be
    // called many times. The number that costs is in `.claude/PLUGINS.md` under P2.
    let format = crate::settings::template_of("tabs.title.format");
    let pinned_format = crate::settings::template_of("tabs.title.format_pinned");
    // --- end setting functions ---------------------------------------------------------------
    let count = snapshot.rows.len();
    let entries: Vec<String> = snapshot
        .rows
        .iter()
        .enumerate()
        .map(|(index, tab)| {
            // `label` is what the strip draws and `title`/`url` are what it draws it *from*; both
            // are sent because the strip still needs the raw URL for the favicon key and the
            // tooltip. `top.js` reads `label` and falls back to the old `title || url` when it is
            // absent, so a strip that has not been reloaded after an upgrade still draws something.
            let template = if tab.pinned { &pinned_format } else { &format };
            let label = label_for(template, tab, index, snapshot.active, count);
            format!(
                "{{\"title\":\"{}\",\"url\":\"{}\",\"label\":\"{}\",\"active\":{},\"pinned\":{},\"muted\":{}}}",
                crate::ipc::json_escape(&tab.title),
                crate::ipc::json_escape(&tab.url),
                crate::ipc::json_escape(&label),
                index == snapshot.active,
                tab.pinned,
                tab.muted,
            )
        })
        .collect();
    // The three presentation settings ride with the tabs rather than in the bar's payload: they are
    // the tab strip's, the strip is pushed on its own, and a strip that had to wait for a bar push
    // to learn its own alignment would be a strip that is right one push late.
    format!(
        "{{\"tabs\":[{}],\"favicons\":\"{}\",\"align\":\"{}\",\"tooltips\":{}}}",
        entries.join(","),
        crate::ipc::json_escape(&crate::settings::choice_of("tabs.favicons.show")),
        crate::ipc::json_escape(&crate::settings::choice_of("tabs.title.alignment")),
        crate::settings::is_on("tabs.tooltips"),
    )
}

// --- setting functions ---------------------------------------------------------------------------
/// What one tab is labelled: the template filled in, or the function's answer.
///
/// **The placeholders are the template's and are not applied to a function's answer**, and that is
/// the decision this function exists to carry. A function that answers `"{index}: hello"` gets a tab
/// labelled `{index}: hello`, literally — because the function *had* the index and chose not to use
/// it, and rewriting its answer behind its back would mean a page whose real title contained
/// `{current_title}` came out different from every other page. The template and the function are two
/// ways of saying what a tab is called, not one wrapped in the other.
///
/// `tabs.title.format_pinned` is handed the same table with `pinned = true`, so one function can
/// serve both — which is what makes taking a function on the sibling setting a line rather than a
/// second vocabulary.
fn label_for(
    template: &Option<crate::settings::Template>,
    tab: &TabRow,
    index: usize,
    active: usize,
    count: usize,
) -> String {
    // Nothing stored and no default — unreachable for these two, which both ship one, and answered
    // rather than unwrapped because a `Template` that is `None` is what a *third* setting taking a
    // function would look like on the day somebody points this at one.
    let Some(template) = template else {
        return String::new();
    };
    if let Some(literal) = template.literal() {
        return format_title(literal, tab, index, active, count);
    }
    let answered = template.call(&[
        // 1-based, the number the strip counts by and the number `{index}` prints. A function that
        // wants the 0-based one subtracts, which is the arithmetic having a function is for.
        ("index", crate::lua::Arg::Int(index as i64 + 1)),
        // bru's own fallback, kept from the template path: a tab that has not been given a title yet
        // is its address rather than an empty string, so `tab.title` is never `""` for a real page
        // and a config does not have to write the `or tab.url` itself.
        (
            "title",
            crate::lua::Arg::Text(if tab.title.is_empty() {
                tab.url.clone()
            } else {
                tab.title.clone()
            }),
        ),
        ("url", crate::lua::Arg::Text(tab.url.clone())),
        ("pinned", crate::lua::Arg::Bool(tab.pinned)),
        ("muted", crate::lua::Arg::Bool(tab.muted)),
    ]);
    // **A function that did not answer falls back to bru's own format, substituted.** Not to the
    // default *string*: `Template::default_text` is `{audio}{index}: {current_title}`, and handing
    // that straight to the strip drew those very characters on a tab — seen 2026-08-07 on the run
    // that was checking a throwing function did not take the browser down. What a person whose
    // function is broken should see is the tab they had before they wrote it, which is this line.
    answered.unwrap_or_else(|| format_title(template.default_text(), tab, index, active, count))
}
// --- end setting functions -------------------------------------------------------------------------

/// `tabs.title.format` and `tabs.title.format_pinned`, filled in for one tab.
///
/// **`cmdline.rs`'s `{url}`/`{title}` replacement is the pattern**, deliberately: a chain of
/// `str::replace` calls, no parser, no template crate. The braces are not nestable and no
/// placeholder is a prefix of another once the `{` is counted, so the chain cannot be order-
/// dependent — `{index}` does not match inside `{aligned_index}`, because the character after that
/// `{` is an `a`.
///
/// **A placeholder bru cannot fill is left standing, not blanked**, and that is the whole of the
/// answer to "what does `{perc}` do". qutebrowser refuses an unknown field when the config is read;
/// bru's `Kind::Text` has no per-setting validator to refuse it in, so the choice is between a tab
/// that reads `{perc}: Example` and one that reads `: Example`. The first says what happened where
/// the second hides it. The four that stay literal are `{perc}`, `{perc_raw}`, `{scroll_pos}` and
/// `{backend}`: the first three are the scroll position of a tab that is not showing, and `scroll.rs`
/// only ever hears from the tab that is — a background tab's percentage is a number bru does not
/// have, not one it has not got round to.
///
/// The rest are qutebrowser's own, `configdata.yml:2378-2404`:
///
/// | | |
/// |---|---|
/// | `{current_title}` | the page's title, or its URL when it has not sent one |
/// | `{title_sep}` | `" - "` when there is a title, empty otherwise |
/// | `{index}` | 1-based, the number the strip counts by |
/// | `{aligned_index}` | the same, right-padded so a strip of ten does not stagger |
/// | `{relative_index}` | signed, against the tab that is showing |
/// | `{id}` | the CEF browser identifier, or `-` before CEF has made one |
/// | `{audio}` | `[M] ` when muted, empty otherwise |
/// | `{host}`, `{protocol}`, `{current_url}` | pulled out of the tab's address |
/// | `{private}` | always empty: every browser bru makes shares the one `RequestContext` |
fn format_title(format: &str, tab: &TabRow, index: usize, active: usize, count: usize) -> String {
    format_fields(
        format,
        &tab.title,
        &tab.url,
        tab.muted,
        tab.browser_id,
        index,
        active,
        count,
    )
}

/// [`format_title`] with the tab's fields spelled out instead of a `&Tab`.
///
/// Split out for one reason: a `Tab` owns a `BrowserView`, which cannot be made without CEF, and a
/// unit test that cannot construct its input is a unit test that gets written as a second copy of
/// the substitution. This is the whole of the behaviour and the tests call exactly it.
#[allow(clippy::too_many_arguments)]
fn format_fields(
    format: &str,
    tab_title: &str,
    url: &str,
    muted: bool,
    browser_id: Option<i32>,
    index: usize,
    active: usize,
    count: usize,
) -> String {
    if !format.contains('{') {
        return format.to_string();
    }
    // bru's own fallback, kept from before the setting existed: a tab that has not been given a
    // title yet shows its address rather than an empty box.
    let title = if tab_title.is_empty() { url } else { tab_title };
    let (scheme, rest) = match url.split_once("://") {
        Some((scheme, rest)) => (scheme, rest),
        None => ("", url),
    };
    let authority = rest.split('/').next().unwrap_or("");
    let host = authority.split('@').next_back().unwrap_or(authority);
    let host = host.split(':').next().unwrap_or(host);

    let number = index + 1;
    let width = count.to_string().len();
    let relative = index as isize - active as isize;

    format
        .replace("{current_title}", title)
        .replace("{title_sep}", if tab_title.is_empty() { "" } else { " - " })
        .replace("{aligned_index}", &format!("{number:>width$}"))
        .replace("{relative_index}", &relative.to_string())
        .replace("{index}", &number.to_string())
        .replace(
            "{id}",
            &browser_id
                .map(|id| id.to_string())
                .unwrap_or_else(|| "-".to_string()),
        )
        .replace("{audio}", if muted { "[M] " } else { "" })
        .replace("{host}", host)
        .replace("{protocol}", scheme)
        .replace("{current_url}", url)
        .replace("{private}", "")
}

/// Rebuild and push every window's strip.
///
/// The one caller is `settings::apply`, for a `Backing::Chrome` setting. It is here rather than in
/// `ipc.rs` because the payload is built from `BruState` and `ipc.rs` only ever holds the string —
/// `push_bar_everywhere` re-renders the strip too, but with the *cached* JSON, which is the one
/// built under the old format.
pub fn push_tabs_everywhere() {
    let Some(state) = crate::state::BruState::instance() else {
        return;
    };
    let per_window: Vec<(u32, TabsSnapshot)> = {
        let Ok(state) = state.lock() else {
            return;
        };
        state
            .window_ids()
            .into_iter()
            .map(|window| (window, state.tabs_snapshot_in(window)))
            .collect()
    };
    // Rendered after the lock is dropped: `render_tabs` can call a Lua title function.
    for (window, snapshot) in per_window {
        crate::ipc::set_tabs_for(window, render_tabs(&snapshot));
    }
}

// --- end tabs and statusbar --------------------------------------------------------------------

/// Opens a tab on `url` in a named window.
///
/// This is the entry point the popup workstream wants: a `on_before_popup` that has decided which
/// window a `target="_blank"` belongs to says so here rather than assuming the current one. It is
/// safe to call from a posted UI task and **not** from inside a message-router query handler — it
/// creates a browser (CEF-NOTES trap 12).
pub fn new_tab_in(state: &SharedState, window_id: u32, url: &str, background: bool) {
    let (client, pages, layout) = {
        let state = state.lock().expect("state mutex poisoned");
        let (pages, layout) = state.pages_of(window_id);
        (state.client(), pages, layout)
    };
    let Some(mut client) = client else {
        return;
    };

    let mut delegate = BruBrowserViewDelegate::new(state.clone(), window_id);
    let settings = BrowserSettings::default();
    let Some(view) = browser_view_create(
        Some(&mut client),
        Some(&CefString::from(url)),
        Some(&settings),
        None,
        None,
        Some(&mut delegate),
    ) else {
        return;
    };

    let Some(index) = state
        .lock()
        .expect("state mutex poisoned")
        .push_tab_in(window_id, view.clone())
    else {
        return;
    };

// --- plugin events ---------------------------------------------------------
    // `tab-opened`, after the tab is in the window's list and so has an index to name — and before
    // `select_in` below, so that a foreground tab's `tab-opened` arrives before its `tab-switched`
    // rather than after. There is no title yet; the page has not been asked for one.
    crate::events::fire(crate::events::Event::TabOpened, Some(window_id), || {
        vec![
            ("index", crate::lua::Arg::Int(index as i64)),
            ("url", crate::lua::Arg::Text(url.to_string())),
            ("title", crate::lua::Arg::Text(String::new())),
        ]
    });
// --- end plugin events -----------------------------------------------------

    // At startup the first tab is made before there is a window to put it in; `attach_all_in` picks
    // it up once the window exists.
    if let Some(pages) = pages {
        attach(&pages, layout.as_ref(), &view, index);
    }

    if !background {
        select_in(state, window_id, index);
        return;
    }

    // A background tab is in the strip, and in the count the bar reports, the moment it is opened —
    // not when its page eventually commits an address. `select_in` is what pushes for a foreground
    // tab and this branch pushed nothing at all, so until now the only thing that told the chrome
    // about an `:open -b` was `on_address_change` in `keys.rs`, one network round trip later.
    let snapshot = state
        .lock()
        .expect("state mutex poisoned")
        .tabs_snapshot_in(window_id);
    crate::ipc::set_tabs_for(window_id, render_tabs(&snapshot));
}

/// Puts every tab of one window into it. Called once per window, from `on_window_created`.
pub fn attach_all_in(state: &SharedState, window_id: u32) {
    let (views, pages, layout) = {
        let state = state.lock().expect("state mutex poisoned");
        let (pages, layout) = state.pages_of(window_id);
        (state.tab_views_in(window_id), pages, layout)
    };
    let Some(pages) = pages else {
        return;
    };
    for (index, view) in views.iter().enumerate() {
        attach(&pages, layout.as_ref(), view, index);
    }
}

/// Put one tab's view in its window's pages panel, at its own index among the tabs.
///
/// **No strip offset any more, and that is the point of the panel.** The tab views used to be
/// direct children of the window, sitting between however many chrome strips were above the pages
/// and however many below, so every one of them had to be placed at `index +
/// window::leading_strip_count()`. They are children of the pages panel now, whose only other
/// children are what `devtools.position right` puts beside them — so the index into that window's
/// tabs is the index into the panel, and the strips are somebody else's arithmetic.
fn attach(pages: &Panel, layout: Option<&BoxLayout>, view: &BrowserView, index: usize) {
    let mut view = View::from(view);
    pages.add_child_view_at(Some(&mut view), index as i32);
    if let Some(layout) = layout {
        layout.set_flex_for_view(Some(&mut view), 1);
    }
    view.set_visible(0);
}

/// Shows one tab of the current window and hides the rest.
pub fn select(state: &SharedState, index: usize) {
    let target = state
        .lock()
        .expect("state mutex poisoned")
        .current_window_id();
    if let Some(target) = target {
        select_in(state, target, index);
    }
}

/// Shows one tab of a named window. It does **not** make that window current: a background window
/// finishing a load and choosing a tab must not steal the keyboard from the one in front.
pub fn select_in(state: &SharedState, window_id: u32, index: usize) {
    let views = {
        let mut state = state.lock().expect("state mutex poisoned");
        if index >= state.tab_count_in(window_id) {
            return;
        }
        state.set_active_in(window_id, index);
        state.tab_views_in(window_id)
    };

    for (i, view) in views.iter().enumerate() {
        View::from(view).set_visible(i32::from(i == index));
    }

    // Visibility alone does not move focus, and a hidden view that keeps it swallows every key —
    // the new tab would look right and answer nothing.
    View::from(&views[index]).request_focus();

    // --- src/devtools.rs ------------------------------------------------------------------------
    // An inspector belongs to one browser, so it is shown only while that browser's tab is. Without
    // this it would stay open under a page it knows nothing about.
    let showing = state
        .lock()
        .expect("state mutex poisoned")
        .tab_browser_in(window_id, index);
    crate::devtools::follow_tab(state, window_id, showing);
    // --- end src/devtools.rs --------------------------------------------------------------------

    // The bar's scroll percentage and match count belong to the page that was showing, and the new
    // tab is somewhere else in a document of its own. Clearing them is what stops `[73%]` sitting
    // over a tab that is at the top; the new tab's own position arrives as soon as it is scrolled.
    crate::scroll::forget();
    crate::find::forget();

    // And the address and title, which otherwise only move when a page navigates: the display
    // handler fires on navigation, and switching tabs is not one. Without this the status line keeps
    // the URL of the tab you just left — measured after the stage-2 merge, with the bar reading
    // example.com over a vesti.bg page.
    let (url, title, snapshot) = {
        let state = state.lock().expect("state mutex poisoned");
        (
            state.tab_url_in(window_id, index).unwrap_or_default(),
            state.tab_title_in(window_id, index).unwrap_or_default(),
            state.tabs_snapshot_in(window_id),
        )
    };
    let tabs = render_tabs(&snapshot);
// --- plugin events ---------------------------------------------------------
    // `tab-switched`, before the bar is told — so that a handler asking bru what is showing gets the
    // tab it was just told about rather than the one being left. It carries the new tab's own url
    // and title, which are already in hand from the read above and cost nothing to hand on.
    crate::events::fire(crate::events::Event::TabSwitched, Some(window_id), || {
        vec![
            ("index", crate::lua::Arg::Int(index as i64)),
            ("url", crate::lua::Arg::Text(url.clone())),
            ("title", crate::lua::Arg::Text(title.clone())),
        ]
    });
// --- end plugin events -----------------------------------------------------
    crate::ipc::set_url_for(window_id, url);
    crate::ipc::set_title_for(window_id, title);
    // Which tab the strip draws as selected is per window too, and a switch fires no display
    // callback to push it.
    crate::ipc::set_tabs_for(window_id, tabs);
    // --- tabs and statusbar --------------------------------------------------------------------
    // `tabs.show switching` shows the strip for 800 ms after a switch and hides it again. Under
    // every other value this is one string compare and a return.
    crate::window::note_tab_switch(window_id);
    // --- end tabs and statusbar ----------------------------------------------------------------
}

pub fn next_tab(state: &SharedState) {
    let (active, count) = {
        let state = state.lock().expect("state mutex poisoned");
        (state.active_tab(), state.tab_count())
    };
    if count == 0 {
        return;
    }
    // --- tabs and statusbar --------------------------------------------------------------------
    // `tabs.wrap`. The modulo below *is* the `true` half of the setting, written as an operator —
    // which is why this is one `if` rather than a new code path: `false` stops on the last tab
    // instead of coming back round to the first, and `select` on the index that is already active
    // is a no-op the strip never sees.
    select(state, step(active, count, 1));
    // --- end tabs and statusbar ----------------------------------------------------------------
}

pub fn prev_tab(state: &SharedState) {
    let (active, count) = {
        let state = state.lock().expect("state mutex poisoned");
        (state.active_tab(), state.tab_count())
    };
    if count == 0 {
        return;
    }
    // --- tabs and statusbar --------------------------------------------------------------------
    select(state, step(active, count, -1));
    // --- end tabs and statusbar ----------------------------------------------------------------
}

// --- tabs and statusbar ------------------------------------------------------------------------
/// One step through the strip, wrapping or not as `tabs.wrap` says.
///
/// Pure, and separate from the two commands so that it can be tested without a window: everything
/// else in this file needs CEF, and the arithmetic is the whole of what the setting changes.
pub(crate) fn step_with(active: usize, count: usize, by: isize, wrap: bool) -> usize {
    if count == 0 {
        return 0;
    }
    let next = active as isize + by;
    if wrap {
        return next.rem_euclid(count as isize) as usize;
    }
    next.clamp(0, count as isize - 1) as usize
}

fn step(active: usize, count: usize, by: isize) -> usize {
    step_with(active, count, by, crate::settings::is_on("tabs.wrap"))
}
// --- end tabs and statusbar --------------------------------------------------------------------

/// Closes every tab but the showing one — `co`, qutebrowser's `:tab-only`.
///
/// The views come out of the window and are dropped, exactly as [`close_current`] does it, and for
/// the same reason: `host.close_browser` on a Views browser closes the window it is parented to.
///
/// Pinned tabs stay unless `force` — see [`BruState::take_other_tabs`].
pub fn close_others(state: &SharedState, force: bool) {
    let (closed, pages, tabs, active, window_id) = {
        let mut state = state.lock().expect("state mutex poisoned");
        let Some(window_id) = state.current_window_id() else {
            return;
        };
        let closed = state.take_other_tabs(force);
        (
            closed,
            state.pages(),
            state.tabs_snapshot(),
            state.active_tab(),
            window_id,
        )
    };
    let tabs = render_tabs(&tabs);
    if closed.is_empty() {
        return;
    }
    for view in &closed {
        if let Some(pages) = &pages {
            pages.remove_child_view(Some(&mut View::from(view)));
        }
    }
    drop(closed);

    crate::ipc::set_tabs_for(window_id, tabs);
    // Not 0: pinned tabs may have stayed in front of the one that is showing.
    select_in(state, window_id, active);
}

/// `<Ctrl-p>` — pin or unpin the showing tab.
///
/// The flag is bru's; nothing in CEF has a concept of a pinned browser. What it buys is the two
/// close paths refusing to take the tab away without `-f`, and the `pinned` class on the strip,
/// which `chrome/chrome.css` already had colours for.
pub fn toggle_pin(state: &SharedState) {
    let snapshot = {
        let mut state = state.lock().expect("state mutex poisoned");
        let index = state.active_tab();
        state.toggle_tab_pinned(index);
        state.tabs_snapshot()
    };
    crate::ipc::set_tabs(render_tabs(&snapshot));
}

/// `<Alt-m>` — mute or unmute the showing tab.
///
/// `host.set_audio_muted` (bindings 12784) is the CEF side, and it is called after the lock is
/// dropped like every other CEF call in this file.
pub fn toggle_mute(state: &SharedState) {
    let (muted, browser, tabs) = {
        let mut state = state.lock().expect("state mutex poisoned");
        let index = state.active_tab();
        let muted = state.toggle_tab_muted(index);
        let browser = state.active_browser();
        (muted, browser, state.tabs_snapshot())
    };
    let tabs = render_tabs(&tabs);
    if let Some(host) = browser.and_then(|browser| browser.host()) {
        host.set_audio_muted(i32::from(muted));
    }
    crate::ipc::set_tabs(tabs);
}

/// Re-apply a tab's mute flag to the browser CEF eventually made for it — the one thing a restored
/// session cannot do at the moment it creates the tab, because `browser_view_create` returns before
/// the browser exists.
pub fn apply_mute(state: &SharedState, index: usize) {
    let (muted, browser) = {
        let mut state = state.lock().expect("state mutex poisoned");
        let muted = state.tab_muted(index);
        let id = state.tab_browser_ids().get(index).copied().flatten();
        (muted, id.and_then(|id| state.browser_with_id(id)))
    };
    if !muted {
        return;
    }
    if let Some(host) = browser.and_then(|browser| browser.host()) {
        host.set_audio_muted(1);
    }
}

/// Moves the showing tab to `to` in the strip — `gm`, `gJ`, `gK`.
pub fn move_current(state: &SharedState, to: usize) {
    let snapshot = {
        let mut state = state.lock().expect("state mutex poisoned");
        let from = state.active_tab();
        state.move_tab(from, to);
        state.tabs_snapshot()
    };
    crate::ipc::set_tabs(render_tabs(&snapshot));
}

/// Closes the showing tab. Closing the last one closes the window, which is what the plan settled
/// on — qutebrowser's `tabs.last_close` default keeps a blank tab instead, and that is
/// DECISIONS.md item 6, still open.
pub fn close_current(state: &SharedState, force: bool) {
    let (closed, remaining, pages, window, active, window_id, closing) = {
        let mut state = state.lock().expect("state mutex poisoned");
        // A pinned tab is not closed by a bare `d`. qutebrowser prompts here
        // (`tabbedbrowser.py:431`); bru has no yes/no mode, so it says why and does nothing, and
        // `:tab-close -f` is the way through.
        if state.tab_pinned(state.active_tab()) && !force {
            eprintln!("bru: tab is pinned — :tab-close -f to close it anyway");
            return;
        }
        let Some(window_id) = state.current_window_id() else {
            return;
        };
// --- plugin events ---------------------------------------------------------
        // What the tab was, read before it is taken — afterwards there is a `BrowserView` and
        // nothing to ask about it. Behind the handler count so that a bru with no plugins does not
        // clone two strings per `d`.
        let closing = if crate::events::handler_count(crate::events::Event::TabClosed) != 0 {
            let index = state.active_tab();
            Some((
                index,
                state.tab_url(index).unwrap_or_default(),
                state.tab_title(index).unwrap_or_default(),
            ))
        } else {
            None
        };
// --- end plugin events -----------------------------------------------------
        let closed = state.take_active_tab();
        (
            closed,
            state.tab_count(),
            state.pages(),
            state.window(),
            state.active_tab(),
            window_id,
            closing,
        )
    };
    let Some(closed) = closed else {
        return;
    };

    // A tab is closed by taking its view out of the window and letting the last reference to that
    // view go. **Not** by `host.close_browser`: a Views browser treats the window it is parented
    // to as its host widget, so closing the browser closes the whole window. Measured 2026-08-06 —
    // closing one of three tabs ran `on_before_close` five times, once per browser in the window,
    // and the process quit. Calling it *after* `remove_child_view` is worse still: CEF then CHECKs
    // in `CefBrowserPlatformDelegateViews::CloseHostWindow` on the widget the view no longer has,
    // and the process aborts with SIGTRAP.
    if let Some(pages) = &pages {
        pages.remove_child_view(Some(&mut View::from(&closed)));
    }
    drop(closed);

// --- plugin events ---------------------------------------------------------
    // `tab-closed`, after the browser is actually gone, so a handler that counts tabs counts the
    // ones that are left. `closing` is `None` when nothing is registered, which is the branch.
    //
    // **Only `:tab-close` fires this**, not `:tab-only` and not a window closing: `take_other_tabs`
    // hands back `BrowserView`s with no index or address left to name, and a window closing takes
    // its tabs with it without passing here at all. Named rather than left to be discovered — see
    // this workstream's report.
    if let Some((index, url, title)) = closing {
        crate::events::fire(crate::events::Event::TabClosed, Some(window_id), || {
            vec![
                ("index", crate::lua::Arg::Int(index as i64)),
                ("url", crate::lua::Arg::Text(url)),
                ("title", crate::lua::Arg::Text(title)),
            ]
        });
    }
// --- end plugin events -----------------------------------------------------

    // The last tab of *this* window closes *this* window. The process only ends when the last
    // window's last browser does — `BruState::on_before_close` counts browsers, not tabs.
    if remaining == 0 {
        if let Some(window) = window {
            window.close();
        }
        return;
    }

    select_in(state, window_id, active);
}

/// `gD` — take the showing tab out of its window and put it in another one, whole.
///
/// `to` is `None` for qutebrowser's bare `:tab-give`, which detaches into a **new** window
/// (`commands.py:460-500`), and `Some(id)` for `:tab-give <win-id>`.
///
/// The move is a re-parent, not a clone: the same `BrowserView`, and therefore the same browser,
/// the same renderer and the same navigation history. Cloning the URL instead would silently lose
/// `H`/`L` on the moved tab, which reads as a bug rather than as a gap.
///
/// **The reference is held across the re-parent, and that is the whole trick.** CEF-NOTES says a
/// tab is closed by `remove_child_view` *and then dropping the view*; here the view is removed and
/// immediately added to another window while `tab` still owns it, so the drop that would close the
/// browser never happens. Measured — see the report.
pub fn give_tab(state: &SharedState, to: Option<u32>) {
    let (from_window, count) = {
        let state = state.lock().expect("state mutex poisoned");
        match state.current_window_id() {
            Some(id) => (id, state.tab_count()),
            None => return,
        }
    };

    if to == Some(from_window) {
        crate::message::error("tab-give: that is the window the tab is already in");
        return;
    }

    // qutebrowser refuses to detach the only tab of a window (`commands.py:483`): the window it
    // came from would close as the new one opened, which is a no-op with a flicker in it.
    if to.is_none() && count < 2 {
        crate::message::error("tab-give: cannot detach from a window with only one tab");
        return;
    }

    let target = match to {
        Some(id) => {
            if state.lock().expect("state mutex poisoned").slot(id).is_none() {
                crate::message::error(&format!("tab-give: there is no window with id {id}"));
                return;
            }
            id
        }
        // A window with no tab in it yet: the tab about to be handed over is its first, so opening
        // it on a URL would load a page only to hide it a moment later.
        None => crate::window::create(state, crate::window::FirstTab::None),
    };

    // Out of bru's book-keeping first, and out of the old window's children second. The `Tab` is
    // ours for the whole of the rest of this function, so the view's last reference is never
    // dropped and the browser is never closed.
    let (tab, old_pages, remaining, old_active) = {
        let mut state = state.lock().expect("state mutex poisoned");
        let tab = state.detach_active_tab_in(from_window);
        (
            tab,
            state.pages_of(from_window).0,
            state.tab_count_in(from_window),
            state.active_tab_in(from_window),
        )
    };
    let Some(tab) = tab else {
        return;
    };

    if let Some(old_pages) = &old_pages {
        old_pages.remove_child_view(Some(&mut View::from(&tab.view)));
    }

    let view = tab.view.clone();
    let (index, new_pages, layout) = {
        let mut state = state.lock().expect("state mutex poisoned");
        let index = state.attach_tab_in(target, tab);
        let (pages, layout) = state.pages_of(target);
        (index, pages, layout)
    };
    let Some(index) = index else {
        return;
    };

    // Adding a browser view that already has a browser does *not* create a second one — the browser
    // follows its view to the new window. Nothing else in bru re-parents a view, so this is the one
    // `add_child_view_at` whose browser already exists.
    if let Some(pages) = &new_pages {
        attach(pages, layout.as_ref(), &view, index);
    }

    select_in(state, target, index);
    if remaining > 0 {
        // The window it came from shows whatever took its place.
        select_in(state, from_window, old_active);
        let snapshot = state
            .lock()
            .expect("state mutex poisoned")
            .tabs_snapshot_in(from_window);
        crate::ipc::set_tabs_for(from_window, render_tabs(&snapshot));
    } else {
        // It gave away its last tab, so it goes — the same rule `close_current` follows. Only
        // reachable through `:tab-give <id>`; the detaching spelling refuses a single-tab window
        // above, because opening a window as another closes is a flicker and nothing else.
        crate::window::close(state, from_window);
    }

    // And the receiving window comes to the front, which is what "give" means when you are the one
    // pressing `gD`.
    focus(state, target);
}

// --- tabs and statusbar ------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    /// `format_fields` with the arguments in the order the tests read them. Not a second
    /// implementation of anything: it forwards, and the substitution it is checking is the one the
    /// strip runs.
    #[allow(clippy::too_many_arguments)]
    fn formatted(
        format: &str,
        title: &str,
        url: &str,
        muted: bool,
        index: usize,
        active: usize,
        count: usize,
    ) -> String {
        format_fields(format, title, url, muted, None, index, active, count)
    }

    #[test]
    fn the_default_format_is_what_the_strip_drew_before_the_setting_existed() {
        assert_eq!(
            formatted(
                "{audio}{index}: {current_title}",
                "Example",
                "https://example.com/",
                false,
                0,
                0,
                1
            ),
            "1: Example"
        );
        // The mute marker was a hard-coded `"[M] "` in top.js. It is `{audio}` now, and it is still
        // the same three characters in the same place.
        assert_eq!(
            formatted(
                "{audio}{index}: {current_title}",
                "Example",
                "https://example.com/",
                true,
                2,
                0,
                3
            ),
            "[M] 3: Example"
        );
        // A tab with no title yet draws its address, which is what `tab.title || tab.url` did.
        assert_eq!(
            formatted("{current_title}", "", "https://example.com/x", false, 0, 0, 1),
            "https://example.com/x"
        );
    }

    #[test]
    fn every_placeholder_bru_can_fill_is_filled() {
        let out = formatted(
            "{index}|{aligned_index}|{relative_index}|{host}|{protocol}|{current_url}|{title_sep}|{private}|",
            "T",
            "https://user@example.com:8443/path",
            false,
            2,
            0,
            10,
        );
        assert_eq!(
            out,
            "3| 3|2|example.com|https|https://user@example.com:8443/path| - ||"
        );
        // `{aligned_index}` pads to the width of the count, so a strip of ten does not stagger.
        assert_eq!(formatted("{aligned_index}", "T", "", false, 0, 0, 10), " 1");
        assert_eq!(formatted("{aligned_index}", "T", "", false, 0, 0, 9), "1");
        // Relative to the tab that is showing, signed.
        assert_eq!(formatted("{relative_index}", "T", "", false, 0, 3, 5), "-3");
    }

    /// The four bru cannot fill are left standing rather than blanked — see `format_fields`. A tab
    /// reading `{perc}: Example` says what happened; one reading `: Example` hides it.
    #[test]
    fn a_placeholder_bru_cannot_fill_stays_on_the_screen() {
        for unknown in ["{perc}", "{perc_raw}", "{scroll_pos}", "{backend}", "{nonsense}"] {
            let out = formatted(&format!("{unknown} {{current_title}}"), "T", "", false, 0, 0, 1);
            assert_eq!(out, format!("{unknown} T"), "{unknown} was quietly swallowed");
        }
    }

    /// `{index}` must not match inside `{aligned_index}` — the chain of `str::replace` calls is only
    /// safe because no placeholder is a prefix of another once the brace is counted.
    #[test]
    fn no_placeholder_eats_another() {
        assert_eq!(formatted("{aligned_index}", "T", "", false, 4, 0, 5), "5");
        assert_eq!(formatted("{relative_index}", "T", "", false, 4, 4, 5), "0");
    }

    /// `tabs.wrap`. The modulo the two commands already used *is* the true half, so the false half
    /// is the only new behaviour and this is what says so.
    #[test]
    fn tabs_wrap_false_stops_at_each_end() {
        assert_eq!(step_with(2, 3, 1, true), 0);
        assert_eq!(step_with(0, 3, -1, true), 2);
        assert_eq!(step_with(2, 3, 1, false), 2);
        assert_eq!(step_with(0, 3, -1, false), 0);
        // In the middle the two agree, which is what makes this a setting about the ends only.
        assert_eq!(step_with(1, 3, 1, true), step_with(1, 3, 1, false));
        assert_eq!(step_with(1, 3, -1, true), step_with(1, 3, -1, false));
        // One tab is both ends at once.
        assert_eq!(step_with(0, 1, 1, false), 0);
        assert_eq!(step_with(0, 1, 1, true), 0);
    }
}
// --- end tabs and statusbar --------------------------------------------------------------------

/// Bring a window to the front and make it the one commands act on.
pub fn focus(state: &SharedState, window_id: u32) {
    let window = {
        let mut state = state.lock().expect("state mutex poisoned");
        if !state.focus_window(window_id) {
            return;
        }
        state.window_handle(window_id)
    };
    if let Some(window) = window {
        window.show();
        window.activate();
    }
}

/// Select a tab on the next turn of the UI loop.
///
/// The one caller is a click on the tab strip, which arrives inside the message router's query
/// handler — and CEF-NOTES trap 12 forbids touching a browser from there: `select` focuses a view,
/// the router holds `browser_query_info_map` across the handler, and `on_before_browse` wants that
/// same lock. Posting steps outside it.
///
/// `from_browser` is the chrome browser the click came from, and it is what says *which window's*
/// strip was clicked. It is resolved in the posted task rather than in the handler, so the handler
/// takes no lock at all.
pub fn schedule_select(from_browser: Option<i32>, index: usize) {
    let mut task = SelectTab::new(from_browser, index);
    post_task(ThreadId::UI, Some(&mut task));
}

wrap_task! {
    struct SelectTab {
        from_browser: Option<i32>,
        index: usize,
    }

    impl Task {
        fn execute(&self) {
            debug_assert_ne!(currently_on(ThreadId::UI), 0);
            let Some(state) = BruState::instance() else {
                return;
            };
            let window = {
                let guard = state.lock().expect("state mutex poisoned");
                self.from_browser
                    .and_then(|id| guard.window_of_browser(id))
                    .or_else(|| guard.current_window_id())
            };
            if let Some(window) = window {
                select_in(&state, window, self.index);
            }
        }
    }
}
