//! Movements, and the scroll percentage the status bar shows.
//!
//! **Every movement goes through `send_mouse_wheel_event`.** That is the one rule this project
//! cannot bend: `window.scrollBy` is what qutebrowser does and it is the feel that made bru worth
//! building. Nothing in here calls a scrolling function on the page — the only JavaScript is a
//! read-only `scrollTop` probe for the status bar, and it never moves anything.
//!
//! Two things CEF does not hand you, and how each is answered:
//!
//! - **The viewport height**, for `<Ctrl-D>`/`<Ctrl-F>` and friends: `View::bounds()` on the tab's
//!   own `BrowserView` (bindings 38380). Synchronous, always current, no round trip.
//! - **The scroll position**, for `[42%]` and for `scroll-to-perc 50`: nothing in CEF reports it.
//!   It is asked of the page, over a process message the renderer answers with `V8Context::eval`
//!   (bindings 29757). That is acceptable *for reporting* a position and for computing one jump;
//!   it is never how a movement is performed.

use cef::*;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::commands::ScrollDirection;
use crate::tabs::SharedState;

/// Pixels per press. Chromium's wheel notch is 40 on Linux, so this is three notches — what a mouse
/// delivers per click, and near enough to qutebrowser's step for the two to be compared.
///
// --- unhardcoded -------------------------------------------------------------------------------
/// **This is now `scroll.step_px`'s default and nothing else reads it directly.** The value a `j`
/// uses is [`step`]; this is what that answers until something sets the setting, and it is
/// unchanged at 120 — the number the whole project exists for, measured on this machine and not
/// moved.
// --- end unhardcoded ---------------------------------------------------------------------------
pub const STEP: i32 = 120;

// --- unhardcoded -------------------------------------------------------------------------------
/// `scroll.step_px`, cached where a keypress can read it without a lock.
///
/// **This is the whole reason `scroll.step_px` has a `Backing` of its own.** Every other setting
/// lifted out of a `const` this round is read through `settings::int_of`, which takes the settings
/// mutex — fine for a download starting or a message being posted, and not fine for `j`, which is
/// the key this browser was built to make feel right. `settings::apply` writes here when the
/// setting changes; [`step`] is a relaxed load, which is what a `const` compiled into the same
/// function costs plus one uncontended read of a cache line.
///
/// Relaxed is the right ordering and not the lazy one: there is exactly one writer, it is on the UI
/// thread, and so is every reader — the only thing an `Acquire` would buy is an ordering against
/// stores this value has no relationship with.
static STEP_PX: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(STEP);

/// How far one `j` moves, in pixels. On the key path — see [`STEP_PX`].
pub fn step() -> i32 {
    STEP_PX.load(Ordering::Relaxed)
}

/// Put `scroll.step_px` where [`step`] will find it. The one caller is `settings::apply`.
pub fn set_step(px: i32) {
    STEP_PX.store(px.clamp(1, 10_000), Ordering::Relaxed);
}
// --- end unhardcoded ---------------------------------------------------------------------------

/// A ceiling on `<count><command>`. qutebrowser has none, but a typo like `99999j` should not lock
/// the UI thread up sending wheel events.
const MAX_COUNT: u32 = 1000;

/// The delta one of `gg`/`G`'s wheel events carries. Any value at or above the viewport height is
/// the same value — see [`jump`] — and this one is large enough to stay that way on any display.
const JUMP_STEP: i32 = 20_000;

/// How many of them `gg`/`G` sends. Each moves at most one viewport, so this is 80 screens: 100 000
/// px at the 1 257 px viewport measured here, and more on a shorter window. Everything past the end
/// of the document is a no-op inside Chromium, and 80 of them cost nothing measurable — the posted
/// task after a `G` still ran on its 250 ms mark, so the UI thread lost under a millisecond.
const JUMP_MAX: u32 = 80;

// -----------------------------------------------------------------------------------------------
// The commands
// -----------------------------------------------------------------------------------------------

/// `scroll <direction>` — `j`, `k`, `h`, `l`, `gg`-less top/bottom, `<PgUp>`/`<PgDn>`.
///
/// The count multiplies, exactly as qutebrowser's `scrollcommands.scroll` does: `3j` is three steps
/// of `j`, not one step three times as long. `top` and `bottom` ignore it, also as qutebrowser does
/// (`scrollcommands.py`:62).
pub fn scroll(
    state: &SharedState,
    browser: &mut Browser,
    direction: ScrollDirection,
    count: Option<u32>,
) {
    let repeat = repeat(count);
    // --- unhardcoded ---------------------------------------------------------------------------
    // Read once per press, not once per event: `10j` is one load and ten wheel events, and the step
    // cannot change between the first and the tenth. See `STEP_PX`.
    let step = step();
    // --- end unhardcoded -----------------------------------------------------------------------
    match direction {
        ScrollDirection::Down => wheel_times(browser, 0, -step, repeat),
        ScrollDirection::Up => wheel_times(browser, 0, step, repeat),
        ScrollDirection::Left => wheel_times(browser, step, 0, repeat),
        ScrollDirection::Right => wheel_times(browser, -step, 0, repeat),
        ScrollDirection::Top => jump(browser, false),
        ScrollDirection::Bottom => jump(browser, true),
        ScrollDirection::PageUp => scroll_page(state, browser, 0.0, -1.0, count),
        ScrollDirection::PageDown => scroll_page(state, browser, 0.0, 1.0, count),
    }
    request_position(browser);
}

/// `scroll-px <dx> <dy>` — the count multiplies the *distance*, not the number of events
/// (`scrollcommands.py`:22).
///
/// **Both axes are negated.** The command's arguments are `window.scrollBy`'s, where positive is
/// right and down; a wheel delta is the direction the *content* moves under the pointer, so it is
/// the other way round on both. Only `dy` was negated before M11, and measured 2026-08-06
/// `scroll-px 2000 0` from x=360 landed at x=0 — it scrolled left, into the stop, instead of
/// 2 000 px right.
pub fn scroll_px(state: &SharedState, browser: &mut Browser, dx: i32, dy: i32, count: Option<u32>) {
    let multiplier = repeat(count) as i32;
    wheel_far(
        state,
        browser,
        -dx.saturating_mul(multiplier),
        -dy.saturating_mul(multiplier),
    );
    request_position(browser);
}

/// `scroll-page <x> <y>` — `<Ctrl-F>` is `0 1`, `<Ctrl-B>` is `0 -1`, `<Ctrl-D>` is `0 0.5`,
/// `<Ctrl-U>` is `0 -0.5`. A page is the tab view's own height, which is the viewport: the window
/// minus the two chrome strips.
pub fn scroll_page(state: &SharedState, browser: &mut Browser, x: f64, y: f64, count: Option<u32>) {
    let Some((width, height)) = viewport(state) else {
        return;
    };
    let multiplier = repeat(count) as f64;
    let dx = (x * multiplier * f64::from(width)).round() as i32;
    let dy = (y * multiplier * f64::from(height)).round() as i32;
    wheel_far(state, browser, -dx, -dy);
    request_position(browser);
}

/// `scroll-to-perc [perc] [-x]` — `gg` is `scroll-to-perc 0`, `G` is a bare `scroll-to-perc`.
///
/// A count stands in for the percentage, and a bare command means the end of the page
/// (`scrollcommands.py`:84–87). 0 and 100 are jumps and need no position at all; anything in
/// between is a distance from where the page is now, so it uses the last reported position and does
/// nothing if none has arrived yet. That is the one place a stale reading could show, and it is why
/// a position query follows every movement rather than only the ones that display one.
pub fn scroll_to_perc(
    state: &SharedState,
    browser: &mut Browser,
    perc: Option<f64>,
    horizontal: bool,
    count: Option<u32>,
) {
    let perc = match (perc, count) {
        (_, Some(count)) => f64::from(count),
        (Some(perc), None) => perc,
        (None, None) => 100.0,
    };
    let perc = perc.clamp(0.0, 100.0);

    if !horizontal {
        if perc <= 0.0 {
            jump(browser, false);
            request_position(browser);
            return;
        }
        if perc >= 100.0 {
            jump(browser, true);
            request_position(browser);
            return;
        }
    }

    // An interior percentage: how far is it from here to there?
    let Some(position) = position() else {
        // Nothing has reported yet. Ask, so the next press works, and refuse to guess at this one —
        // a wrong guess would scroll somewhere the user did not ask for.
        request_position(browser);
        return;
    };
    let (max, now) = if horizontal {
        (position.max_x, position.x)
    } else {
        (position.max_y, position.y)
    };
    let delta = ((perc / 100.0) * max - now).round() as i32;
    if horizontal {
        wheel_far(state, browser, -delta, 0);
    } else {
        wheel_far(state, browser, 0, -delta);
    }
    request_position(browser);
}

fn repeat(count: Option<u32>) -> u32 {
    count.unwrap_or(1).clamp(1, MAX_COUNT)
}

// -----------------------------------------------------------------------------------------------
// The wheel
// -----------------------------------------------------------------------------------------------

/// Chromium delivers a wheel event to whatever sits under the cursor, so it needs a position inside
/// the page rather than over a scrollable child.
fn wheel(browser: &mut Browser, dx: i32, dy: i32) {
    if dx == 0 && dy == 0 {
        return;
    }
    let Some(host) = browser.host() else {
        return;
    };
    let mouse = MouseEvent { x: 10, y: 10, modifiers: 0 };
    host.send_mouse_wheel_event(Some(&mouse), dx, dy);
}

fn wheel_times(browser: &mut Browser, dx: i32, dy: i32, times: u32) {
    let Some(host) = browser.host() else {
        return;
    };
    let mouse = MouseEvent { x: 10, y: 10, modifiers: 0 };
    for _ in 0..times {
        host.send_mouse_wheel_event(Some(&mouse), dx, dy);
    }
}

/// One movement of any size, as however many wheel events it takes.
///
/// **A single wheel event never moves the page further than its own viewport**, whatever the delta
/// says — measured 2026-08-06, see [`jump`]. So a delta that is worth more than a screen, which
/// `<Ctrl-F>` with a count and any interior `scroll-to-perc` are, has to be spread over several.
/// The slice is half the view's height, comfortably inside the cap on any page (the page's own
/// viewport is the view less its scrollbars, ~14 px here), so a full page costs two events and a
/// jump to the middle of a long document costs a couple of dozen. The arithmetic is in `i64` and
/// each slice is a difference of running totals, so the pieces add up to exactly the delta asked
/// for rather than to a rounded-down approximation of it.
fn wheel_far(state: &SharedState, browser: &mut Browser, dx: i32, dy: i32) {
    if dx == 0 && dy == 0 {
        return;
    }
    let (slice_x, slice_y) = match viewport(state) {
        Some((width, height)) => ((width / 2).max(1), (height / 2).max(1)),
        // No view to measure — one screen is never smaller than this on a window worth using.
        None => (256, 256),
    };
    let steps = (dx.abs() / slice_x).max(dy.abs() / slice_y) + 1;

    let Some(host) = browser.host() else {
        return;
    };
    let mouse = MouseEvent { x: 10, y: 10, modifiers: 0 };
    let (dx, dy, steps) = (i64::from(dx), i64::from(dy), i64::from(steps));
    let (mut sent_x, mut sent_y) = (0i64, 0i64);
    for step in 1..=steps {
        let (want_x, want_y) = (dx * step / steps, dy * step / steps);
        host.send_mouse_wheel_event(
            Some(&mouse),
            (want_x - sent_x) as ::std::os::raw::c_int,
            (want_y - sent_y) as ::std::os::raw::c_int,
        );
        (sent_x, sent_y) = (want_x, want_y);
    }
}

/// `gg` and `G`, still on the wheel path.
///
/// **One `send_mouse_wheel_event` moves the page by at most one viewport, whatever the delta says.**
/// Measured 2026-08-06 on a 20 000 px page in a 1 257 px viewport: deltas of 1 300, 5 000 and
/// 10 000 000 each moved the page exactly 1 256–1 257 px, while 1 000 moved 1 000. That is a hard
/// cap in Chromium's wheel handling and it is why PLAN.md's "large wheel deltas" answer cannot work
/// on its own — the plan left `gg`/`G` open, and this is the measurement that closes it. Repeating
/// the event is what gets there: each one starts from where the last finished, and 80 of them
/// reached the bottom of that page inside the first 400 ms sample, in one press.
///
/// `send_key_event(Home/End)` was the alternative and is rejected, not on speed but on reach: a key
/// goes to whatever the page has focused. Measured on the same page with an `<input autofocus>`,
/// `End` moved it **0 px** — the caret went to the end of the field — where `G` still went to
/// 18 776. A wheel event is aimed at a point in the page and always finds the scroller under it.
fn jump(browser: &mut Browser, down: bool) {
    let Some(host) = browser.host() else {
        return;
    };
    let mouse = MouseEvent { x: 10, y: 10, modifiers: 0 };
    let dy = if down { -JUMP_STEP } else { JUMP_STEP };
    for _ in 0..JUMP_MAX {
        host.send_mouse_wheel_event(Some(&mouse), 0, dy);
    }
}

/// The tab view's bounds, which is the page viewport: the window less the two chrome strips.
///
/// The lock is taken, read, and dropped before any CEF call — `tabs.rs` explains why at length.
fn viewport(state: &SharedState) -> Option<(i32, i32)> {
    let (views, active) = {
        let state = state.lock().ok()?;
        (state.tab_views(), state.active_tab())
    };
    let bounds = View::from(views.get(active)?).bounds();
    (bounds.width > 0 && bounds.height > 0).then_some((bounds.width, bounds.height))
}

// -----------------------------------------------------------------------------------------------
// Where the page is — asked of the page, never used to move it
// -----------------------------------------------------------------------------------------------

/// Browser → renderer: "where are you?".
const QUERY: &str = "bru.scroll.query";
/// Renderer → browser: `"x,y,max_x,max_y,width,height"`, all CSS pixels.
const REPORT: &str = "bru.scroll.report";

/// The probe. `document.scrollingElement` is the element that scrolls in both standards and quirks
/// mode; `scrollHeight - clientHeight` is how far it can go, which is what a percentage is against
/// (qutebrowser computes the same thing as `contentsSize.height() - widget.height()`,
/// `webenginetab.py`:528).
const PROBE: &str = "(function(){var e=document.scrollingElement||document.documentElement;\
if(!e){return '';}\
return [e.scrollLeft,e.scrollTop,\
Math.max(0,e.scrollWidth-e.clientWidth),Math.max(0,e.scrollHeight-e.clientHeight),\
e.clientWidth,e.clientHeight].join(',');})()";

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Position {
    pub x: f64,
    pub y: f64,
    pub max_x: f64,
    pub max_y: f64,
    pub width: f64,
    pub height: f64,
}

fn cell() -> &'static Mutex<Option<Position>> {
    static POSITION: Mutex<Option<Position>> = Mutex::new(None);
    &POSITION
}

/// The last position the page reported, or `None` before it has reported one.
pub fn position() -> Option<Position> {
    cell().lock().ok().and_then(|position| *position)
}

/// Forget the position — the page under it is gone. Called when a tab is switched or a load starts;
/// without it `scroll-to-perc 50` on a fresh page would aim at the old one's dimensions.
pub fn forget() {
    if let Ok(mut position) = cell().lock() {
        *position = None;
    }
    crate::ipc::set_scroll(String::new());
}

/// Ask the page where it is, once the scrolling has settled.
///
/// Debounced, and that is the point: a held `j` is the one path this project exists to keep cheap,
/// and a process message per keypress would put IPC on it. Each call bumps a sequence number and
/// posts a task; only the task holding the latest number asks. A burst of presses therefore costs
/// one round trip, `SETTLE_MS` after the last of them — by which time Chromium's scroll animation
/// has finished, so the number reported is the one the page came to rest at.
pub fn request_position(browser: &mut Browser) {
    const SETTLE_MS: i64 = 150;

    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed) + 1;
    let Some(frame) = browser.main_frame() else {
        return;
    };
    let mut task = AskPosition::new(frame, sequence);
    post_delayed_task(ThreadId::UI, Some(&mut task), SETTLE_MS);
}

/// Bumped by every `request_position`; only the task holding the current value asks.
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

wrap_task! {
    struct AskPosition {
        frame: Frame,
        sequence: u64,
    }

    impl Task {
        fn execute(&self) {
            if SEQUENCE.load(Ordering::Relaxed) != self.sequence {
                // A later press has already been queued; that one will ask.
                return;
            }
            let Some(mut message) = process_message_create(Some(&CefString::from(QUERY))) else {
                return;
            };
            self.frame.send_process_message(ProcessId::RENDERER, Some(&mut message));
        }
    }
}

/// Browser side of the reply. Called from `ipc::on_process_message_received` before the message
/// router sees the message; returns true when it was ours.
pub fn on_report(browser: Option<&Browser>, message: Option<&ProcessMessage>) -> bool {
    let Some(message) = message else {
        return false;
    };
    if CefString::from(&message.name()).to_string() != REPORT {
        return false;
    }

    // Only the tab that is showing may write the status bar. A background tab finishing a load
    // would otherwise report its own position into the bar of the one on screen.
    let is_active = match (browser, crate::state::BruState::instance()) {
        (Some(browser), Some(state)) => {
            let id = browser.identifier();
            state
                .lock()
                .map(|state| state.is_active_browser(id))
                .unwrap_or(false)
        }
        _ => false,
    };
    if !is_active {
        return true;
    }

    let Some(arguments) = message.argument_list() else {
        return true;
    };
    let text = CefString::from(&arguments.string(0)).to_string();
    let Some(position) = parse_report(&text) else {
        return true;
    };

    if let Ok(mut cell) = cell().lock() {
        *cell = Some(position);
    }
    crate::ipc::set_scroll(perc_text(position.max_y, position.y));
    true
}

/// Renderer side. Called from `ipc::renderer_on_process_message_received`; returns true when the
/// message was ours. **Nothing here may touch `BruState`** — this runs in the render process, where
/// that struct exists and is empty.
pub fn renderer_on_query(frame: Option<&Frame>, message: Option<&ProcessMessage>) -> bool {
    let Some(message) = message else {
        return false;
    };
    if CefString::from(&message.name()).to_string() != QUERY {
        return false;
    }
    let Some(frame) = frame else {
        return true;
    };
    let Some(text) = evaluate(frame, PROBE) else {
        return true;
    };
    let Some(mut reply) = process_message_create(Some(&CefString::from(REPORT))) else {
        return true;
    };
    if let Some(arguments) = reply.argument_list() {
        arguments.set_string(0, Some(&CefString::from(text.as_str())));
    }
    frame.send_process_message(ProcessId::BROWSER, Some(&mut reply));
    true
}

/// Run an expression in the frame's own V8 context and return it as a string.
///
/// `eval` has to be called between `enter` and `exit` — outside that scope there is no context for
/// the script to belong to and CEF refuses. This is the only JavaScript bru runs against a page on
/// the movement path, and it reads; it never scrolls.
fn evaluate(frame: &Frame, code: &str) -> Option<String> {
    let context = frame.v8_context()?;
    if context.enter() == 0 {
        return None;
    }
    let mut value: Option<V8Value> = None;
    let mut exception: Option<V8Exception> = None;
    let ok = context.eval(
        Some(&CefString::from(code)),
        None,
        0,
        Some(&mut value),
        Some(&mut exception),
    );
    let text = (ok != 0)
        .then_some(value)
        .flatten()
        .map(|value| CefString::from(&value.string_value()).to_string());
    context.exit();
    text
}

fn parse_report(text: &str) -> Option<Position> {
    let mut fields = text.split(',').map(|field| field.trim().parse::<f64>());
    let mut next = || fields.next().and_then(Result::ok);
    Some(Position {
        x: next()?,
        y: next()?,
        max_x: next()?,
        max_y: next()?,
        width: next()?,
        height: next()?,
    })
}

/// qutebrowser's percentage widget, verbatim: `[top]`, `[bot]`, `[NN%]`, and `[top]` for a page
/// that does not scroll at all (`percentage.py`:_calc_strings, `webenginetab.py`:528–534, where an
/// unscrollable page is 0 rather than an error).
fn perc_text(max: f64, now: f64) -> String {
    if max <= 0.0 {
        return "[top]".to_string();
    }
    let perc = (100.0 / max * now).round().clamp(0.0, 100.0) as i32;
    match perc {
        0 => "[top]".to_string(),
        100 => "[bot]".to_string(),
        perc => format!("[{perc:02}%]"),
    }
}

// -----------------------------------------------------------------------------------------------
// The debug switch that drives all of this without a keyboard
// -----------------------------------------------------------------------------------------------

/// `--scroll-script=pos,G,pos,gg,pos --scroll-step-ms=800` runs movements from posted UI tasks and
/// prints the page position before each one.
///
/// It exists for the same reason `--tab-script` does: `wtype` is the only key-injection tool on
/// this machine and CEF segfaults in `xkb_state_update_mask` when its virtual keyboard's keymap
/// arrives (CEF-NOTES.md, "Injecting keys on this machine"). So an unattended check cannot press a
/// key, and this drives the very functions a key would call. Inert unless the switch is passed.
///
/// Steps: `j` `k` `h` `l` `gg` `G` `C-d` `C-u` `C-f` `C-b`, `perc:<n>`, `px:<dx>:<dy>`,
/// `wheel:<dy>` (one raw event, for measuring what a delta is worth), `end`/`home`
/// (`send_key_event`, kept only so the report's comparison can be re-run), and `pos`.
///
/// A leading number is the count, so `10j` and `3C-d` are written as they are typed.
///
/// M11's search rides along on the same switch, because it is driven the same way and needs no
/// second one: `/<text>` and `?<text>` start a search, `n` and `N` continue it, `clear` ends it.
/// The steps are comma-separated, so search text with a comma in it cannot be written here.
pub fn schedule_script(steps: &str, interval_ms: i64) {
    started();
    for (index, step) in steps.split(',').filter(|s| !s.is_empty()).enumerate() {
        let mut task = ScrollStep::new(step.to_string());
        post_delayed_task(ThreadId::UI, Some(&mut task), interval_ms * (index as i64 + 1));
    }
}

/// When the script was scheduled, so every line carries a millisecond — "how long did `G` take" is
/// not a question a log without timestamps can answer.
fn started() -> &'static std::time::Instant {
    static STARTED: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    STARTED.get_or_init(std::time::Instant::now)
}

wrap_task! {
    struct ScrollStep {
        step: String,
    }

    impl Task {
        fn execute(&self) {
            let Some(state) = crate::state::BruState::instance() else {
                return;
            };
            let browser = state.lock().expect("state mutex poisoned").active_browser();
            let Some(mut browser) = browser else {
                return;
            };

            let view = viewport(&state)
                .map(|(w, h)| format!("view={w}x{h}"))
                .unwrap_or_else(|| "view=?".to_string());
            let at = started().elapsed().as_millis();
            let found = crate::find::matches();
            match position() {
                Some(p) => eprintln!(
                    "scroll-script: {at:>6}ms before {:<8} y={:.0}/{:.0} x={:.0}/{:.0} client={:.0}x{:.0} {view} {} {found:?}",
                    self.step,
                    p.y,
                    p.max_y,
                    p.x,
                    p.max_x,
                    p.width,
                    p.height,
                    perc_text(p.max_y, p.y),
                ),
                None => eprintln!(
                    "scroll-script: {at:>6}ms before {:<8} {view} (no position yet) {found:?}",
                    self.step
                ),
            }

            // A leading number is the count, so `10j` in the script is `10j` at the keyboard. It is
            // stripped here rather than in each arm because every movement takes one.
            let digits = self.step.find(|c: char| !c.is_ascii_digit()).unwrap_or(0);
            let count = self.step[..digits].parse::<u32>().ok();
            let step = &self.step[digits..];

            match step {
                "j" => scroll(&state, &mut browser, ScrollDirection::Down, count),
                "k" => scroll(&state, &mut browser, ScrollDirection::Up, count),
                "h" => scroll(&state, &mut browser, ScrollDirection::Left, count),
                "l" => scroll(&state, &mut browser, ScrollDirection::Right, count),
                "gg" => scroll_to_perc(&state, &mut browser, Some(0.0), false, None),
                "G" => scroll_to_perc(&state, &mut browser, None, false, count),
                "C-d" => scroll_page(&state, &mut browser, 0.0, 0.5, count),
                "C-u" => scroll_page(&state, &mut browser, 0.0, -0.5, count),
                "C-f" => scroll_page(&state, &mut browser, 0.0, 1.0, count),
                "C-b" => scroll_page(&state, &mut browser, 0.0, -1.0, count),
                "pos" => request_position(&mut browser),
                "n" => crate::find::search_next(&mut browser, count),
                "N" => crate::find::search_prev(&mut browser, count),
                "clear" => crate::find::clear(&mut browser),
                other if other.starts_with('/') => {
                    crate::find::search(&mut browser, &other[1..], false)
                }
                other if other.starts_with('?') => {
                    crate::find::search(&mut browser, &other[1..], true)
                }
                other if other.starts_with("perc:") || other.starts_with("percx:") => {
                    let horizontal = other.starts_with("percx:");
                    let argument = &other[if horizontal { 6 } else { 5 }..];
                    match argument.parse::<f64>() {
                        Ok(perc) => {
                            scroll_to_perc(&state, &mut browser, Some(perc), horizontal, None)
                        }
                        Err(_) => eprintln!("scroll-script: bad percentage in {other}"),
                    }
                }
                other if other.starts_with("px:") => {
                    let mut fields = other[3..].split(':').map(str::parse::<i32>);
                    match (fields.next(), fields.next()) {
                        (Some(Ok(dx)), Some(Ok(dy))) => {
                            scroll_px(&state, &mut browser, dx, dy, count)
                        }
                        _ => eprintln!("scroll-script: bad pixels in {other}"),
                    }
                }
                other if other.starts_with("wheel:") => match other[6..].parse::<i32>() {
                    Ok(dy) => {
                        wheel(&mut browser, 0, dy);
                        request_position(&mut browser);
                    }
                    Err(_) => eprintln!("scroll-script: bad delta in {other}"),
                },
                "home" | "end" => {
                    send_named_key(&mut browser, if step == "home" { 0x24 } else { 0x23 });
                    request_position(&mut browser);
                }
                other => eprintln!("scroll-script: no step named {other}"),
            }
        }
    }
}

/// A synthetic Home/End, kept for the `gg`/`G` comparison in M11's report and used by nothing else.
/// It is *not* how bru moves: a key goes to whatever the page has focused, and an input field eats
/// it, where a wheel event always reaches the scroller under the cursor.
fn send_named_key(browser: &mut Browser, windows_key_code: i32) {
    let Some(host) = browser.host() else {
        return;
    };
    for type_ in [KeyEventType::RAWKEYDOWN, KeyEventType::KEYDOWN, KeyEventType::KEYUP] {
        let event = KeyEvent {
            type_,
            windows_key_code,
            native_key_code: 0,
            ..Default::default()
        };
        host.send_key_event(Some(&event));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentages_read_the_way_qutebrowsers_do() {
        assert_eq!(perc_text(1000.0, 0.0), "[top]");
        assert_eq!(perc_text(1000.0, 1000.0), "[bot]");
        assert_eq!(perc_text(1000.0, 500.0), "[50%]");
        // Two digits, zero-padded — qutebrowser's '[{:02}%]'.
        assert_eq!(perc_text(1000.0, 70.0), "[07%]");
        // A page shorter than the window never scrolls, and qutebrowser calls that the top rather
        // than an error.
        assert_eq!(perc_text(0.0, 0.0), "[top]");
        // Past the end (rubber-banding, or a report that raced a resize) still reads as the bottom.
        assert_eq!(perc_text(1000.0, 1200.0), "[bot]");
    }

    #[test]
    fn a_report_is_six_numbers() {
        let position = parse_report("0,1234.5,0,9000,1900,1300").expect("well-formed report");
        assert_eq!(position.y, 1234.5);
        assert_eq!(position.max_y, 9000.0);
        assert_eq!(position.height, 1300.0);
        assert_eq!(parse_report(""), None);
        assert_eq!(parse_report("1,2,3"), None);
        assert_eq!(parse_report("a,b,c,d,e,f"), None);
    }

    #[test]
    fn a_count_multiplies_but_is_capped() {
        assert_eq!(repeat(None), 1);
        assert_eq!(repeat(Some(0)), 1);
        assert_eq!(repeat(Some(10)), 10);
        assert_eq!(repeat(Some(99_999)), MAX_COUNT);
    }
}
