//! C6: a terminal key event turned into the three integers `KeyInfo::from_cef` already reads.
//!
//! bru knows exactly one way to spell a keypress, and it is the one in `src/bindings.rs`:
//! `KeyInfo::from_cef(windows_key_code, modifiers, character)`. The window frontend gets those
//! three from CEF. The terminal frontend has to *make* them out of bytes, and this module is the
//! only place that does. **The point of the whole exercise is that there is no second key table**:
//! a terminal key and an X11 key that mean `<Ctrl-a>` arrive at the binding trie through the same
//! function, so a binding cannot work in one frontend and not the other, and `bindings.rs` did not
//! have to learn that terminals exist.
//!
//! ## Why that is achievable at all, and where it stops being achievable
//!
//! `from_cef` needs a Windows virtual key code that names a *physical* key, because when Ctrl, Alt
//! or Meta is held it ignores the character entirely and asks [`us_layout_char`] what that physical
//! key types on a US keyboard — which is how `<Ctrl-a>` stays `<Ctrl-a>` under a Cyrillic layout,
//! in bru as in Chromium itself.
//!
//! A terminal does not send physical keys. It sends characters. The kitty keyboard protocol is what
//! closes the gap: with the *report alternate keys* flag set it sends, alongside the character the
//! key produced in the current layout, the **base layout key** — "the key corresponding to the
//! physical key in the standard PC-101 layout", i.e. the US character. That is precisely the thing
//! `us_layout_char` maps from, so pressing the physical `a` key under `bg` arrives here as key
//! `1092` (`ф`) with base layout key `97` (`a`), becomes `VKEY_A`, and comes out of `from_cef` as
//! `ф` when typed and as `<Ctrl-a>` when Ctrl is down. Both are right, and neither needed a table
//! of bru's own.
//!
//! Where it stops:
//!
//! - **A key with no US counterpart, with Ctrl held.** No base layout key means no vkey means
//!   `us_layout_char` returns `None` and `from_cef` drops the event. Nothing can be done from this
//!   side: bru would have to be told what the physical key *is*, and the terminal never says.
//! - **Legacy input** (no kitty protocol, or a terminal that does not implement it). There is no
//!   base layout key, no shifted key and no event type in a bare byte, so a Cyrillic letter with
//!   Alt held is lost the same way, `Ctrl-H` cannot be told from Backspace, nor `Ctrl-I` from Tab.
//!   Every one of those is called out at the line that makes the choice.
//! - **Non-BMP characters.** CEF's `character` is one UTF-16 code unit, so an astral codepoint
//!   cannot be put in it — by CEF's own shape, not by ours. [`TermKey::text`] carries the real
//!   `char` for the caller to type into the page.
//!
//! ## What this module expects the terminal to have been put in
//!
//! Enabling the protocol is C3's job (`term_session.rs`), not this one's, but the parser is written
//! for a terminal that was asked for all five progressive-enhancement flags — `CSI > 31 u`:
//!
//! ```text
//!  1  disambiguate escape codes    Escape arrives as CSI 27 u, so a lone ESC needs no timeout
//!  2  report event types           press / repeat / release, instead of press only
//!  4  report alternate keys        the shifted key and the base layout key — see above
//!  8  report all keys as escapes   Ctrl-C is a key, not a SIGINT the tty ate first
//! 16  report associated text       what the keypress actually types, dead keys composed
//! ```
//!
//! It parses the legacy forms anyway, because the flags can be refused, tmux can be in the middle,
//! and the first bytes arrive before the reply to the query does.
//!
//! ## Incremental by construction
//!
//! Input arrives byte by byte from a pty. A parser that guesses at a half-arrived escape sequence
//! writes `[200~` into the page, so [`next_event`] answers [`Step::Incomplete`] and asks for more
//! rather than ever guessing. The one genuinely undecidable case is a lone `ESC` — Escape pressed,
//! or the first byte of something longer — and it is undecidable *only* without flag 1 above; the
//! caller resolves it with a timeout and [`next_event_at_timeout`].

use crate::bindings::{KeyInfo, us_layout_char};

// ---------------------------------------------------------------------------------------------
// The two bitfields
// ---------------------------------------------------------------------------------------------

// Bit values from `cef_event_flags_t`. **Taken from `bindings.rs` rather than repeated**, so that
// the module reading this bitfield and the module writing it cannot come to disagree about what a
// bit means: `Modifiers::from_cef` is the reader, and everything below is the writer.
use crate::bindings::Modifiers as CefFlags;
const CEF_CAPS_LOCK_ON: u32 = CefFlags::CEF_CAPS_LOCK_ON;
const CEF_SHIFT_DOWN: u32 = CefFlags::CEF_SHIFT_DOWN;
const CEF_CONTROL_DOWN: u32 = CefFlags::CEF_CONTROL_DOWN;
const CEF_ALT_DOWN: u32 = CefFlags::CEF_ALT_DOWN;
const CEF_COMMAND_DOWN: u32 = CefFlags::CEF_COMMAND_DOWN;
const CEF_NUM_LOCK_ON: u32 = CefFlags::CEF_NUM_LOCK_ON;
const CEF_IS_KEY_PAD: u32 = CefFlags::CEF_IS_KEY_PAD;

// The kitty modifier mask, which is sent as mask+1 so that the field is never `0` and never
// vanishes into a default. `Modifiers::from_cef` reads only shift/ctrl/alt/command/keypad; the two
// lock bits are passed on for CEF's benefit and deliberately mean nothing to a binding, because
// Caps Lock is state and not a modifier.
const KITTY_SHIFT: u32 = 1;
const KITTY_ALT: u32 = 2;
const KITTY_CTRL: u32 = 4;
const KITTY_SUPER: u32 = 8;
// Held, reported, and impossible to write in a binding — see `cef_modifiers`.
#[allow(dead_code)]
const KITTY_HYPER: u32 = 16;
const KITTY_META: u32 = 32;
const KITTY_CAPS_LOCK: u32 = 64;
const KITTY_NUM_LOCK: u32 = 128;

/// Translate the kitty modifier field (already +1, as it comes off the wire) into CEF's bitfield.
///
/// `super` becomes `EVENTFLAG_COMMAND_DOWN`, which is what `Modifiers::from_cef` calls `Meta` —
/// the same identification X11 and Qt make, where the Windows key is `Mod4` is `MetaModifier`.
/// kitty's separate `meta` bit joins it there: it is the X11 `Meta` that virtually no layout binds
/// separately from Super, and folding it in is better than dropping a modifier that was held.
/// `hyper` has no CEF flag and no way to be written in a binding, so it is dropped — the same
/// treatment, and for the same reason, that `EVENTFLAG_ALTGR_DOWN` gets in `bindings.rs`.
fn cef_modifiers(field: Option<u32>) -> u32 {
    let mask = field.unwrap_or(1).saturating_sub(1);
    let mut m = 0;
    if mask & KITTY_SHIFT != 0 {
        m |= CEF_SHIFT_DOWN;
    }
    if mask & KITTY_ALT != 0 {
        m |= CEF_ALT_DOWN;
    }
    if mask & KITTY_CTRL != 0 {
        m |= CEF_CONTROL_DOWN;
    }
    if mask & (KITTY_SUPER | KITTY_META) != 0 {
        m |= CEF_COMMAND_DOWN;
    }
    if mask & KITTY_CAPS_LOCK != 0 {
        m |= CEF_CAPS_LOCK_ON;
    }
    if mask & KITTY_NUM_LOCK != 0 {
        m |= CEF_NUM_LOCK_ON;
    }
    // KITTY_HYPER is intentionally not mapped.
    m
}

// ---------------------------------------------------------------------------------------------
// What comes out
// ---------------------------------------------------------------------------------------------

/// Press, repeat or release — the kitty protocol's event type, defaulted to `Press` for every
/// legacy form, because a bare byte is a press and a terminal that does not report releases is not
/// withholding one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeyEventKind {
    Press,
    /// Auto-repeat. A held `j` scrolls, so this is a press as far as bindings are concerned; it is
    /// kept apart from `Press` only because CEF wants to know and because a key that must not
    /// repeat (`:` opening the command line) is easier to write when the difference survives.
    Repeat,
    Release,
}

/// One terminal key event, in the shape CEF's `KeyEvent` wants.
///
/// The three integers are what [`KeyInfo::from_cef`] consumes. [`TermKey::text`] is the fourth
/// thing and the one CEF's own struct cannot hold: the character this keystroke *types*, when that
/// differs from `character`. They differ exactly when the honest answer for the binding layer and
/// the honest answer for the page are not the same value — see [`TermKey::character`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TermKey {
    /// A `VKEY_*` value: the physical key, resolved through the base layout key.
    ///
    /// `0` means "this terminal never said which physical key it was, and there is no US key that
    /// types this character" — a legal state that costs nothing while no modifier is latched, and
    /// costs the whole event when one is.
    pub windows_key_code: i32,
    /// `cef_event_flags_t`.
    pub modifiers: u32,
    /// `KeyEvent::character`, the UTF-16 code unit — **or `0`, meaning "ask the US table"**.
    ///
    /// `0` is not a fallback for failure here, it is a request: `from_cef` fills an absent
    /// character in from [`us_layout_char`], and that is the only way to say `Shift-1` is `!` when
    /// the terminal reported the key as `1` and the shift as a modifier bit. See `character_for`.
    pub character: u16,
    /// What to type into the page, if this keystroke types anything. Non-BMP safe, dead-key
    /// composition included (it arrives in the protocol's third parameter).
    pub text: Option<char>,
    pub event: KeyEventKind,
}

impl TermKey {
    /// The binding-layer view of this event, through the one function that spells keys.
    ///
    /// Releases return `None` **on purpose**: a release that reaches the trie is every binding
    /// firing twice, and the check belongs at the seam rather than in each caller that would
    /// eventually forget it. The event type is still on [`TermKey::event`] for the CEF path, which
    /// does need to send the key-up.
    #[allow(dead_code)] // Waits for C3: nothing reads terminal input until the session loop exists.
    pub fn to_key_info(self) -> Option<KeyInfo> {
        if self.event == KeyEventKind::Release {
            return None;
        }
        KeyInfo::from_cef(self.windows_key_code, self.modifiers, self.character)
    }

    /// A press or an auto-repeat: the events that make something happen.
    #[allow(dead_code)] // Waits for C3, with `to_key_info`.
    pub fn is_press(self) -> bool {
        matches!(self.event, KeyEventKind::Press | KeyEventKind::Repeat)
    }

    fn new(windows_key_code: i32, modifiers: u32, character: u16, text: Option<char>) -> TermKey {
        TermKey { windows_key_code, modifiers, character, text, event: KeyEventKind::Press }
    }

    fn with_event(mut self, event: KeyEventKind) -> TermKey {
        self.event = event;
        self
    }
}

/// A mouse report, in the units the terminal sent it in.
///
/// **`x` and `y` are whatever the terminal is reporting in** — cells under SGR 1006, pixels under
/// SGR 1016 — and this type does not know which. Converting is the caller's, because only the
/// caller knows which mode it asked the terminal for and how big a cell is; a struct that guessed
/// would be wrong by a factor of the font size and look like a browser clicking at random.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TermMouse {
    /// `0` left, `1` middle, `2` right; `64`/`65` are wheel up/down, which SGR encodes as buttons.
    pub button: u16,
    pub x: i32,
    pub y: i32,
    /// `cef_event_flags_t`, from the same bits the modifier keys use.
    pub modifiers: u32,
    /// `true` for a press or a drag, `false` for the release — SGR's final byte, `M` against `m`.
    pub pressed: bool,
    /// A motion report rather than a press: bit 32 of the button field.
    pub motion: bool,
}

/// An in-band resize report: what mode 2048 says the pane has become.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TermResize {
    pub rows: u16,
    pub cols: u16,
    /// The pane's pixels. The escape's last two parameters are allowed to be zero, which means the
    /// terminal is not saying — the size arithmetic falls back to the grid times the cell.
    pub width: u32,
    pub height: u32,
}

/// `CSI 48 ; rows ; cols ; height ; width t` — a mode 2048 report, exactly five fields.
///
/// **Height before width on the wire, `(width, height)` in the struct**, the same swap every `t`
/// reply in `term_session.rs` makes: the standard's order is the opposite of everything else in
/// the terminal path, and remembering that is this function's job and nobody else's. A report with
/// no grid is no report — zero rows or columns describes a pane nothing can be laid out in.
fn parse_resize_report(params: &[u8]) -> Option<TermResize> {
    let text = std::str::from_utf8(params).ok()?;
    let mut fields = text.split(';');
    if fields.next()?.parse::<u32>().ok()? != 48 {
        return None;
    }
    let rows: u16 = fields.next()?.parse().ok()?;
    let cols: u16 = fields.next()?.parse().ok()?;
    let height: u32 = fields.next()?.parse().ok()?;
    let width: u32 = fields.next()?.parse().ok()?;
    if fields.next().is_some() || rows == 0 || cols == 0 {
        return None;
    }
    Some(TermResize { rows, cols, width, height })
}

/// `CSI < button ; x ; y M|m`.
fn parse_sgr_mouse(params: &[u8], final_byte: u8) -> Option<TermMouse> {
    let text = std::str::from_utf8(params).ok()?;
    let mut fields = text.split(';');
    let raw_button: u32 = fields.next()?.parse().ok()?;
    let x: i32 = fields.next()?.parse().ok()?;
    let y: i32 = fields.next()?.parse().ok()?;
    if fields.next().is_some() {
        return None;
    }
    // The modifier bits live in the button field: 4 shift, 8 alt, 16 ctrl. Bit 32 is motion and bit
    // 64 marks the wheel, which SGR reports as buttons 64 and 65 rather than as a scroll.
    let mut modifiers = 0u32;
    if raw_button & 4 != 0 {
        modifiers |= CEF_SHIFT_DOWN;
    }
    if raw_button & 8 != 0 {
        modifiers |= CEF_ALT_DOWN;
    }
    if raw_button & 16 != 0 {
        modifiers |= CEF_CONTROL_DOWN;
    }
    Some(TermMouse {
        button: u16::try_from(raw_button & 0b1100_0011).ok()?,
        x,
        y,
        modifiers,
        pressed: final_byte == b'M',
        motion: raw_button & 32 != 0,
    })
}

/// What the front of the buffer turned out to be, and how much of it to drop.
///
/// The byte count is on the variant rather than returned alongside because the two are never
/// separately meaningful: consuming the wrong number of bytes for a step is how a parser starts
/// reading the tail of one sequence as the head of the next.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Step {
    /// A key event, and the bytes it took.
    Key(TermKey, usize),
    /// A mouse report, and the bytes it took. Separate from [`Step::Ignored`] because a click is
    /// something to act on and a focus change is not; they arrive through the same `CSI <` door and
    /// were told apart nowhere until something wanted the clicks.
    Mouse(TermMouse, usize),
    /// An in-band resize report (mode 2048), and the bytes it took. Separate from
    /// [`Step::Ignored`] for the same reason the mouse is: the session asked the terminal for these
    /// reports — they carry the pane's pixels *in order with the bytes around them*, which
    /// `SIGWINCH` does not — and a report that is parsed and then dropped is a mode that was turned
    /// on for nothing.
    Resize(TermResize, usize),
    /// A complete, understood sequence that is not a key: a mouse report, a focus change, the reply
    /// to a query, a bracketed-paste marker. Drop these bytes and carry on — quietly, because they
    /// are not errors and there will be many.
    Ignored(usize),
    /// Not a whole sequence yet. Read more bytes and ask again; consume nothing.
    Incomplete,
    /// Bytes that cannot begin anything. Drop them and resynchronise. The count is deliberately
    /// generous — a malformed CSI is dropped whole rather than one byte at a time, because feeding
    /// `[`, `1`, `;` back through as text is precisely the garbage-into-the-page failure.
    Invalid(usize),
}

impl Step {
    /// How many bytes to drop from the front of the buffer.
    #[allow(dead_code)] // Waits for C3's read loop, which is the only thing that drains a buffer.
    pub fn consumed(&self) -> usize {
        match self {
            Step::Key(_, n)
            | Step::Mouse(_, n)
            | Step::Resize(_, n)
            | Step::Ignored(n)
            | Step::Invalid(n) => *n,
            Step::Incomplete => 0,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The entry points
// ---------------------------------------------------------------------------------------------

/// A CSI that has grown this long without a final byte is not a CSI.
///
/// The longest real sequence here is a `u` form with three parameters and a couple of codepoints of
/// associated text, around twenty bytes. The cap exists so that a terminal spraying `0x3B` cannot
/// wedge the input loop into never consuming anything again.
const MAX_SEQUENCE: usize = 64;

/// Read one event off the front of `input`.
///
/// Pure: no terminal, no CEF, no state between calls. The caller owns the buffer, calls this until
/// it answers [`Step::Incomplete`], and drops [`Step::consumed`] bytes after each answer.
#[allow(dead_code)] // Waits for C3.
pub fn next_event(input: &[u8]) -> Step {
    parse(input, false, 0)
}

/// The same, told that no more bytes are coming for now.
///
/// This exists for one byte: `ESC`. Alone it is the Escape key; followed by `[` it is a CSI;
/// followed by a letter it is Alt-that-letter. Nothing but time separates the three, so the caller
/// waits its escape-timeout (25 ms is what vim and kitty use) and, if nothing arrived, asks again
/// through this door, where a lone `ESC` is Escape and `ESC ESC` is `<Alt-Escape>`.
///
/// With the disambiguate flag on, this is never needed: Escape arrives as `CSI 27 u`.
#[allow(dead_code)] // Waits for C3.
pub fn next_event_at_timeout(input: &[u8]) -> Step {
    parse(input, true, 0)
}

fn parse(input: &[u8], flush: bool, depth: u8) -> Step {
    match input.first() {
        None => Step::Incomplete,
        Some(0x1B) => parse_escape(input, flush, depth),
        Some(_) => parse_legacy(input),
    }
}

// ---------------------------------------------------------------------------------------------
// ESC-prefixed forms
// ---------------------------------------------------------------------------------------------

fn parse_escape(input: &[u8], flush: bool, depth: u8) -> Step {
    debug_assert_eq!(input.first(), Some(&0x1B));
    let Some(&second) = input.get(1) else {
        // Escape, or the beginning of everything else. Only the clock knows.
        return if flush {
            Step::Key(TermKey::new(VKEY_ESCAPE, 0, 0, None), 1)
        } else {
            Step::Incomplete
        };
    };

    match second {
        b'[' => parse_csi(input),
        b'O' => parse_ss3(input),
        // `ESC ESC` is Alt-Escape, and `ESC ESC [ A` is Alt-Up: xterm's meta-sends-escape prefixes
        // the *whole* sequence, so the honest reading is "parse what follows and add Alt". The
        // depth cap is because a third ESC is not a key anyone pressed — it is a stream of Escapes
        // arriving faster than the timeout, and each is dealt with one at a time.
        0x1B if depth < 1 => match parse(&input[1..], flush, depth + 1) {
            Step::Key(k, n) => Step::Key(alt(k), n + 1),
            // A mouse report does not become Alt-anything: the modifiers a click carries are in its
            // own button field, and a leading ESC before one is the prefix bytes, not a held key.
            Step::Mouse(m, n) => Step::Mouse(m, n + 1),
            // Nor does a resize report — a pane has no modifiers.
            Step::Resize(r, n) => Step::Resize(r, n + 1),
            Step::Ignored(n) => Step::Ignored(n + 1),
            Step::Invalid(n) => Step::Invalid(n + 1),
            Step::Incomplete => Step::Incomplete,
        },
        0x1B => {
            if flush {
                Step::Key(TermKey::new(VKEY_ESCAPE, 0, 0, None), 1)
            } else {
                Step::Incomplete
            }
        }
        // DCS, OSC, PM, APC: a terminal answering a query bru asked (`term.rs` asks several), which
        // is never a key and must not be read as one. Runs to `ESC \` or `BEL`.
        b'P' | b']' | b'^' | b'_' => parse_string_sequence(input),
        // Alt-anything, the legacy way. `ESC` then the key exactly as it would have arrived alone.
        _ => match parse_legacy(&input[1..]) {
            Step::Key(k, n) => Step::Key(alt(k), n + 1),
            Step::Mouse(m, n) => Step::Mouse(m, n + 1),
            Step::Resize(r, n) => Step::Resize(r, n + 1),
            Step::Ignored(n) => Step::Ignored(n + 1),
            Step::Invalid(n) => Step::Invalid(n + 1),
            Step::Incomplete => Step::Incomplete,
        },
    }
}

/// Add Alt to a key that arrived ESC-prefixed.
///
/// The text goes with it: `ESC a` is `<Alt-a>` and types nothing, so leaving `text` set would type
/// an `a` into the page on every Alt binding.
fn alt(k: TermKey) -> TermKey {
    TermKey { modifiers: k.modifiers | CEF_ALT_DOWN, text: None, ..k }
}

/// `ESC P`/`ESC ]`/`ESC ^`/`ESC _` … `ESC \` or `BEL`. Consumed and ignored, never a key.
fn parse_string_sequence(input: &[u8]) -> Step {
    let mut i = 2;
    while i < input.len() {
        if input[i] == 0x07 {
            return Step::Ignored(i + 1);
        }
        if input[i] == 0x1B && input.get(i + 1) == Some(&b'\\') {
            return Step::Ignored(i + 2);
        }
        // A lone ESC that is not the start of an ST ends the string sequence as far as we care:
        // the terminal is speaking again and we would rather resynchronise than swallow keys.
        if input[i] == 0x1B && input.len() > i + 1 {
            return Step::Ignored(i);
        }
        i += 1;
    }
    Step::Incomplete
}

/// `ESC O <letter>`: the application-keypad forms. F1–F4 and the arrows in their oldest spelling,
/// which is what a terminal falls back to when nothing has been enabled.
fn parse_ss3(input: &[u8]) -> Step {
    let Some(&b) = input.get(2) else {
        return Step::Incomplete;
    };
    // `ESC O 1 ; 5 P` — some terminals put modifiers in an SS3, which is not in any standard but is
    // in the wild. Re-read it as a CSI, whose parameter parsing is the same.
    if b.is_ascii_digit() {
        return parse_csi_body(input, 2);
    }
    let key = match b {
        b'A' => Some((VKEY_UP, false)),
        b'B' => Some((VKEY_DOWN, false)),
        b'C' => Some((VKEY_RIGHT, false)),
        b'D' => Some((VKEY_LEFT, false)),
        b'E' => Some((VKEY_CLEAR, true)),
        b'F' => Some((VKEY_END, false)),
        b'H' => Some((VKEY_HOME, false)),
        b'M' => Some((VKEY_RETURN, true)),
        b'P' => Some((VKEY_F1, false)),
        b'Q' => Some((VKEY_F1 + 1, false)),
        b'R' => Some((VKEY_F1 + 2, false)),
        b'S' => Some((VKEY_F1 + 3, false)),
        // The keypad's own arithmetic, in application mode.
        b'j' => Some((VKEY_MULTIPLY, true)),
        b'k' => Some((VKEY_ADD, true)),
        b'm' => Some((VKEY_SUBTRACT, true)),
        b'n' => Some((VKEY_DECIMAL, true)),
        b'o' => Some((VKEY_DIVIDE, true)),
        b'p'..=b'y' => Some((VKEY_NUMPAD0 + (b - b'p') as i32, true)),
        _ => None,
    };
    match key {
        Some((vkey, keypad)) => {
            let mods = if keypad { CEF_IS_KEY_PAD } else { 0 };
            Step::Key(TermKey::new(vkey, mods, 0, None), 3)
        }
        None => Step::Ignored(3),
    }
}

// ---------------------------------------------------------------------------------------------
// CSI
// ---------------------------------------------------------------------------------------------

fn parse_csi(input: &[u8]) -> Step {
    parse_csi_body(input, 2)
}

/// `start` is the index of the first parameter byte — 2 for a real `ESC [`, and also 2 for the
/// `ESC O 1 ; 5 P` misspelling, which is why this is a parameter.
fn parse_csi_body(input: &[u8], start: usize) -> Step {
    let mut i = start;
    // Parameter bytes: digits, `;`, `:`, and the private markers `<=>?`.
    while i < input.len() && (0x30..=0x3F).contains(&input[i]) {
        i += 1;
    }
    let params_end = i;
    // Intermediate bytes, which no key form uses but which a valid sequence may carry.
    while i < input.len() && (0x20..=0x2F).contains(&input[i]) {
        i += 1;
    }
    if i > MAX_SEQUENCE {
        return Step::Invalid(i);
    }
    let Some(&final_byte) = input.get(i) else {
        return Step::Incomplete;
    };
    let consumed = i + 1;
    if !(0x40..=0x7E).contains(&final_byte) {
        return Step::Invalid(consumed);
    }
    let raw = &input[start..params_end];

    // A private-parameter CSI is a report, never a key: `CSI < … M` is an SGR mouse event,
    // `CSI ? … u` is the terminal answering what keyboard flags it has, `CSI > … c` is a device
    // attribute. All of them arrive on the same stdin as the keys do.
    if let Some(&first) = raw.first() {
        if first == b'<' && matches!(final_byte, b'M' | b'm') {
            return match parse_sgr_mouse(&raw[1..], final_byte) {
                Some(mouse) => Step::Mouse(mouse, consumed),
                // A `CSI <` that is not three numbers is not a mouse report bru understands, and
                // guessing at one would click somewhere nobody pointed.
                None => Step::Ignored(consumed),
            };
        }
        if matches!(first, b'<' | b'=' | b'>' | b'?') {
            return Step::Ignored(consumed);
        }
    }
    // X10 mouse reporting: three raw bytes follow the `M` and they are not parameters.
    if final_byte == b'M' && raw.is_empty() {
        return if input.len() >= consumed + 3 {
            Step::Ignored(consumed + 3)
        } else {
            Step::Incomplete
        };
    }

    // A window report. The one this frontend asked for — mode 2048's `CSI 48 … t` — is an event to
    // act on; every other `t` (a `14t`/`16t` answer arriving late, a report bru never requested) is
    // a reply to drop. Parsed here because this is the parser that owns how many bytes it took.
    if final_byte == b't' {
        return match parse_resize_report(raw) {
            Some(resize) => Step::Resize(resize, consumed),
            None => Step::Ignored(consumed),
        };
    }

    let p = Params::parse(raw);
    match final_byte {
        b'u' => csi_u(&p, consumed),
        b'~' => csi_tilde(&p, consumed),
        b'A'..=b'H' | b'P'..=b'S' | b'Z' => csi_letter(final_byte, &p, consumed),
        // `CSI I` / `CSI O` are focus in and out; everything else with a letter final is a report.
        _ => Step::Ignored(consumed),
    }
}

/// The parameters of a CSI, as `;`-separated fields of `:`-separated subfields.
///
/// Fixed-size and by value: this runs once per keystroke, and a `Vec<Vec<_>>` per keystroke buys
/// nothing when the protocol never uses more than three fields (`key;modifiers;text`) and never
/// needs more than three subfields of the first (`key:shifted:base`). Anything past that is
/// dropped, which is what a parameter nobody reads deserves.
#[derive(Default, Debug)]
struct Params {
    fields: [[Option<u32>; 3]; 3],
}

impl Params {
    fn parse(bytes: &[u8]) -> Params {
        let mut p = Params::default();
        let (mut field, mut sub) = (0usize, 0usize);
        let mut cur: Option<u32> = None;
        for &b in bytes {
            match b {
                b'0'..=b'9' => {
                    let d = u32::from(b - b'0');
                    cur = Some(cur.unwrap_or(0).saturating_mul(10).saturating_add(d));
                }
                b':' => {
                    p.store(field, sub, cur);
                    sub += 1;
                    cur = None;
                }
                b';' => {
                    p.store(field, sub, cur);
                    field += 1;
                    sub = 0;
                    cur = None;
                }
                // The private markers are handled before this is ever called.
                _ => {}
            }
        }
        p.store(field, sub, cur);
        p
    }

    fn store(&mut self, field: usize, sub: usize, value: Option<u32>) {
        if field < self.fields.len() && sub < self.fields[field].len() {
            self.fields[field][sub] = value;
        }
    }

    fn get(&self, field: usize, sub: usize) -> Option<u32> {
        self.fields.get(field).and_then(|f| f.get(sub)).copied().flatten()
    }
}

/// `1` press, `2` repeat, `3` release; absent means press, because a terminal that was not asked to
/// report event types only ever reports presses.
fn event_kind(field: Option<u32>) -> KeyEventKind {
    match field {
        Some(2) => KeyEventKind::Repeat,
        Some(3) => KeyEventKind::Release,
        _ => KeyEventKind::Press,
    }
}

/// `CSI key[:shifted[:base]] ; mods[:event] ; text u` — the protocol's general form, and the one
/// every key takes once *report all keys as escape codes* is on.
fn csi_u(p: &Params, consumed: usize) -> Step {
    let Some(code) = p.get(0, 0) else {
        return Step::Ignored(consumed);
    };
    let mods = cef_modifiers(p.get(1, 0));
    let event = event_kind(p.get(1, 1));
    let text = p.get(2, 0).and_then(char::from_u32).filter(|c| !c.is_control());

    match key_for_code(code, p.get(0, 1), p.get(0, 2), mods, text) {
        Some(k) => Step::Key(k.with_event(event), consumed),
        None => Step::Ignored(consumed),
    }
}

/// `CSI number ; mods ~` — the vt220 numbering, which kitty keeps for the keys that had one.
fn csi_tilde(p: &Params, consumed: usize) -> Step {
    let Some(number) = p.get(0, 0) else {
        return Step::Ignored(consumed);
    };
    let mods = cef_modifiers(p.get(1, 0));
    let event = event_kind(p.get(1, 1));

    let vkey = match number {
        1 | 7 => VKEY_HOME,
        2 => VKEY_INSERT,
        3 => VKEY_DELETE,
        4 | 8 => VKEY_END,
        5 => VKEY_PRIOR,
        6 => VKEY_NEXT,
        11..=15 => VKEY_F1 + (number - 11) as i32,
        17..=21 => VKEY_F1 + (number - 17 + 5) as i32,
        23 | 24 => VKEY_F1 + (number - 23 + 10) as i32,
        25 | 26 => VKEY_F1 + (number - 25 + 12) as i32,
        28 => VKEY_F1 + 14,
        // kitty spells the Menu key `CSI 29 ~`. xterm calls the same sequence F16, and the two
        // cannot both be honoured; the terminal this is written for wins, and a bru running under
        // xterm loses a function key nobody has.
        29 => VKEY_MENU_APP,
        31..=34 => VKEY_F1 + (number - 31 + 16) as i32,
        // Bracketed paste. The markers are not keys and the body between them is not keys either —
        // whoever turned bracketed paste on owns the bytes in between, and this parser must not be
        // the thing that decides a paste is typing.
        200 | 201 => return Step::Ignored(consumed),
        _ => return Step::Ignored(consumed),
    };
    Step::Key(TermKey::new(vkey, mods, 0, None).with_event(event), consumed)
}

/// `CSI [1;mods] <letter>` — arrows, Home/End, F1–F4, and Shift-Tab.
fn csi_letter(final_byte: u8, p: &Params, consumed: usize) -> Step {
    let first = p.get(0, 0);
    let mut mods = cef_modifiers(p.get(1, 0));
    let event = event_kind(p.get(1, 1));

    // `CSI 24 ; 80 R` is the cursor position bru asked for in `term.rs`, and `CSI 1 ; 5 R` is
    // Ctrl-F3. The dummy first parameter of a key form is always 1, which is what tells them apart
    // — and it is why kitty encodes F3 as `CSI 13 ~` instead, so that the clash never arises.
    if final_byte == b'R' && first.unwrap_or(1) != 1 {
        return Step::Ignored(consumed);
    }
    // `CSI 3 G` moves the cursor; only `CSI 1 ; … <letter>` and the bare form are keys.
    if !matches!(first, None | Some(1)) {
        return Step::Ignored(consumed);
    }

    let (vkey, keypad) = match final_byte {
        b'A' => (VKEY_UP, false),
        b'B' => (VKEY_DOWN, false),
        b'C' => (VKEY_RIGHT, false),
        b'D' => (VKEY_LEFT, false),
        b'E' => (VKEY_CLEAR, true),
        b'F' => (VKEY_END, false),
        b'H' => (VKEY_HOME, false),
        b'P' => (VKEY_F1, false),
        b'Q' => (VKEY_F1 + 1, false),
        b'R' => (VKEY_F1 + 2, false),
        b'S' => (VKEY_F1 + 3, false),
        // `CSI Z` is Shift-Tab and carries the Shift in its identity rather than in a parameter.
        // `NamedKey::from_name` already folds `backtab` onto Tab for the same reason.
        b'Z' => {
            mods |= CEF_SHIFT_DOWN;
            (VKEY_TAB, false)
        }
        _ => return Step::Ignored(consumed),
    };
    if keypad {
        mods |= CEF_IS_KEY_PAD;
    }
    Step::Key(TermKey::new(vkey, mods, 0, None).with_event(event), consumed)
}

// ---------------------------------------------------------------------------------------------
// A kitty key code becomes a virtual key
// ---------------------------------------------------------------------------------------------

/// Turn a kitty `u`-form key code into a [`TermKey`], resolving text keys through the layout.
///
/// `None` means the key is real and recognised but has nowhere to go in CEF's vocabulary — a media
/// key with no `VKEY_*`, or F25 and above. Dropping it is right; inventing a code for it is not.
fn key_for_code(
    code: u32,
    shifted: Option<u32>,
    base: Option<u32>,
    mods: u32,
    text: Option<char>,
) -> Option<TermKey> {
    if let Some((vkey, keypad)) = functional_vkey(code) {
        let mods = if keypad { mods | CEF_IS_KEY_PAD } else { mods };
        return Some(TermKey::new(vkey, mods, character_u16(text), text));
    }
    if is_functional_code(code) {
        // Inside kitty's private-use block, so it is a key, but not one CEF names.
        return None;
    }

    // A text key. The base layout key is the physical key's US character and is exactly what
    // `us_layout_char` maps from; without it, the key's own codepoint is the best guess, and it is
    // the right guess whenever the layout is US.
    let key_char = char::from_u32(code)?;
    let base_char = base.and_then(char::from_u32).unwrap_or(key_char);
    let vkey = vkey_for_text_char(base_char).unwrap_or(0);

    let shift = mods & CEF_SHIFT_DOWN != 0;
    let character = character_for(key_char, shifted.and_then(char::from_u32), text, shift);
    let typed = text.or_else(|| {
        // With Ctrl, Alt or Meta down the keystroke types nothing; it is a command, and `from_cef`
        // ignores the character for exactly that reason.
        if mods & (CEF_CONTROL_DOWN | CEF_ALT_DOWN | CEF_COMMAND_DOWN) != 0 {
            None
        } else {
            char::from_u32(u32::from(character)).filter(|c| !c.is_control())
        }
    });
    Some(TermKey::new(vkey, mods, character, typed))
}

/// Which UTF-16 unit to hand `from_cef`, and — more often than not — why the answer is `0`.
///
/// The rule that matters is the Shift one. kitty reports the *unshifted* codepoint as the key, so
/// Shift-`a` is key `97` with the Shift bit; handing `from_cef` a `97` and a Shift would make it
/// spell the key `a`, because it strips a lone Shift from anything that is not already uppercase.
/// So:
///
/// - the terminal's own associated text wins outright — it is the only party that knows what a
///   dead-key composition produced;
/// - then the reported shifted key, which is what *report alternate keys* is for;
/// - then, for a letter, the uppercase of the key itself, so that Shift-`ф` is `Ф` and not the `A`
///   the US table would otherwise supply;
/// - and otherwise `0`, which asks `from_cef` for the US shifted table — the only place that knows
///   Shift-`1` is `!`.
fn character_for(key: char, shifted: Option<char>, text: Option<char>, shift: bool) -> u16 {
    if let Some(t) = text {
        return character_u16(Some(t));
    }
    if shift {
        if let Some(s) = shifted {
            return character_u16(Some(s));
        }
        if key.is_alphabetic() {
            return character_u16(key.to_uppercase().next());
        }
        return 0;
    }
    if key.is_control() {
        return 0;
    }
    character_u16(Some(key))
}

/// A `char` as CEF's `character` field, which is one UTF-16 code unit and therefore cannot hold an
/// astral codepoint. Those become `0` — the key event still carries the physical key, and
/// [`TermKey::text`] still carries the character for whoever types it into the page.
fn character_u16(c: Option<char>) -> u16 {
    match c {
        Some(c) if (c as u32) <= 0xFFFF => c as u32 as u16,
        _ => 0,
    }
}

/// kitty's private-use block for keys that have no Unicode codepoint of their own.
fn is_functional_code(code: u32) -> bool {
    (57344..=63743).contains(&code)
}

/// The key codes that are not text: the named keys with an ASCII code, and kitty's private block.
///
/// Returns the `VKEY_*` and whether the key is on the numeric keypad, which CEF carries as a
/// modifier flag and `bindings.rs` spells `<Num+…>`.
fn functional_vkey(code: u32) -> Option<(i32, bool)> {
    let key = match code {
        9 => (VKEY_TAB, false),
        13 => (VKEY_RETURN, false),
        27 => (VKEY_ESCAPE, false),
        // Space is a *named* key in `bindings.rs` (`<Space>`), not a `Char(' ')`, so it has to come
        // through here — resolving it as text would produce a key no binding can name.
        32 => (VKEY_SPACE, false),
        127 => (VKEY_BACK, false),

        57358 => (VKEY_CAPITAL, false),
        57359 => (VKEY_SCROLL, false),
        57360 => (VKEY_NUMLOCK, false),
        57361 => (VKEY_SNAPSHOT, false),
        57362 => (VKEY_PAUSE, false),
        57363 => (VKEY_MENU_APP, false),
        // F13–F24. kitty goes on to F35; Windows virtual keys stop at F24 and so does
        // `named_key_for_vkey`, so the rest are dropped rather than folded onto something else.
        57376..=57387 => (VKEY_F1 + 12 + (code - 57376) as i32, false),

        57399..=57408 => (VKEY_NUMPAD0 + (code - 57399) as i32, true),
        57409 => (VKEY_DECIMAL, true),
        57410 => (VKEY_DIVIDE, true),
        57411 => (VKEY_MULTIPLY, true),
        57412 => (VKEY_SUBTRACT, true),
        57413 => (VKEY_ADD, true),
        57414 => (VKEY_RETURN, true),
        // Keypad `=` has no virtual key of its own on Windows either; the main-row `=` plus the
        // keypad flag is the closest true statement.
        57415 => (VKEY_OEM_PLUS, true),
        57416 => (VKEY_SEPARATOR, true),
        57417 => (VKEY_LEFT, true),
        57418 => (VKEY_RIGHT, true),
        57419 => (VKEY_UP, true),
        57420 => (VKEY_DOWN, true),
        57421 => (VKEY_PRIOR, true),
        57422 => (VKEY_NEXT, true),
        57423 => (VKEY_HOME, true),
        57424 => (VKEY_END, true),
        57425 => (VKEY_INSERT, true),
        57426 => (VKEY_DELETE, true),
        57427 => (VKEY_CLEAR, true),

        57430 => (VKEY_MEDIA_PLAY_PAUSE, false),
        57432 => (VKEY_MEDIA_STOP, false),
        57435 => (VKEY_MEDIA_NEXT_TRACK, false),
        57436 => (VKEY_MEDIA_PREV_TRACK, false),
        57438 => (VKEY_VOLUME_DOWN, false),
        57439 => (VKEY_VOLUME_UP, false),
        57440 => (VKEY_VOLUME_MUTE, false),

        // The modifier keys themselves. They map to the vkeys `is_modifier_vkey` knows, so
        // `from_cef` drops them — which is what has to happen: a chain must not be broken by the
        // Ctrl that begins it. Hyper and Meta have no virtual key; they go to the Super ones,
        // where the only thing that happens to them is being dropped as modifiers anyway.
        57441 => (VKEY_LSHIFT, false),
        57442 => (VKEY_LCONTROL, false),
        57443 => (VKEY_LMENU, false),
        57444..=57446 => (VKEY_LWIN, false),
        57447 => (VKEY_RSHIFT, false),
        57448 => (VKEY_RCONTROL, false),
        57449 => (VKEY_RMENU, false),
        57450..=57452 => (VKEY_RWIN, false),
        // ISO_Level3_Shift and ISO_Level5_Shift: AltGr, which arrives as the right Alt.
        57453 | 57454 => (VKEY_RMENU, false),
        _ => return None,
    };
    Some(key)
}

/// **The inverse of [`us_layout_char`], computed *with* [`us_layout_char`].**
///
/// This is the one place that could have become the second key table the module exists to avoid: a
/// character has to become a virtual key, and writing that mapping out again is writing the US
/// layout down a second time, in a second file, to drift against the first the day either is
/// touched. Searching the vkey space through the same function instead makes drift impossible by
/// construction — the answer is *defined* as "the key `us_layout_char` says types this".
///
/// The scan is bounded to the main keyboard, and that is not an optimisation. The keypad codes
/// `0x60..=0x69` also type `0`–`9` and `0x6D` also types `-`, so including them would make the
/// inverse ambiguous; a keypad key never arrives here anyway, because the protocol gives it a
/// functional code of its own and [`functional_vkey`] has already claimed it. Within the main
/// keyboard the mapping is injective, which `us_layout_char_inverts_exactly` asserts key by key.
///
/// Returns the vkey and whether Shift is needed to type the character on a US keyboard.
fn us_vkey_for_char(c: char) -> Option<(i32, bool)> {
    const MAIN_KEYBOARD: [(i32, i32); 4] = [(0x30, 0x39), (0x41, 0x5A), (0xBA, 0xC0), (0xDB, 0xDE)];
    for shift in [false, true] {
        for (first, last) in MAIN_KEYBOARD {
            for vkey in first..=last {
                if us_layout_char(vkey, shift) == Some(c) {
                    return Some((vkey, shift));
                }
            }
        }
    }
    None
}

/// The virtual key for a character key, with the two that the US table cannot answer for.
fn vkey_for_text_char(c: char) -> Option<i32> {
    if c == ' ' {
        return Some(VKEY_SPACE);
    }
    // A layout reports letters unshifted, but a stray uppercase (a legacy byte, mostly) names the
    // same physical key.
    let lowered = c.to_lowercase().next().unwrap_or(c);
    us_vkey_for_char(lowered).or_else(|| us_vkey_for_char(c)).map(|(vkey, _)| vkey)
}

// ---------------------------------------------------------------------------------------------
// Legacy input: bare bytes
// ---------------------------------------------------------------------------------------------

fn parse_legacy(input: &[u8]) -> Step {
    let b = input[0];
    if let Some(key) = c0_key(b) {
        return Step::Key(key, 1);
    }
    match decode_utf8(input) {
        Utf8::Incomplete => Step::Incomplete,
        Utf8::Invalid(n) => Step::Invalid(n),
        Utf8::Char(c, n) => Step::Key(text_key(c), n),
    }
}

/// A character that arrived as itself, with no protocol around it.
///
/// The Shift is reconstructed from the US table because it has to be: `from_cef` strips a Shift it
/// was not given, so an `A` sent with no modifiers comes back spelled `a`. Anything the US layout
/// cannot type — every Cyrillic letter, for one — gets no virtual key, which is harmless while the
/// character carries the meaning and fatal the moment a modifier is latched. That is the legacy
/// path's cost, and the kitty protocol's base layout key is what buys it off.
fn text_key(c: char) -> TermKey {
    if c == ' ' {
        return TermKey::new(VKEY_SPACE, 0, 0, Some(' '));
    }
    let (vkey, shift) = us_vkey_for_char(c).unwrap_or((0, false));
    let mods = if shift { CEF_SHIFT_DOWN } else { 0 };
    TermKey::new(vkey, mods, character_u16(Some(c)), Some(c))
}

/// The C0 controls, which is how a terminal has always spelled Ctrl.
///
/// Three of the collisions here are older than any of us and none of them can be resolved from this
/// side — the byte is the same byte:
///
/// - `0x09` is Tab **and** Ctrl-I; `0x0D` is Return **and** Ctrl-M. The named key wins, because a
///   browser whose Tab key does nothing is broken in a way `<Ctrl-i>` is not.
/// - `0x08` is Backspace **and** Ctrl-H. Backspace wins for the same reason. kitty sends `0x7F` for
///   Backspace and `0x08` for Ctrl-H, so on kitty this costs nothing; on a terminal configured the
///   other way it would cost Backspace, which is the worse loss.
/// - `0x0A` is Ctrl-J, and it is *also* what Enter arrives as here: `Raw::enter` in `src/term.rs`
///   clears only `ECHO` and `ICANON`, leaving `ICRNL` on, so the tty turns the CR into an LF before
///   bru ever sees it. Return therefore has to win `0x0A` too, and `<Ctrl-j>` is unreachable in the
///   legacy path. Under the kitty protocol it arrives as `CSI 106;5u` and is reachable again.
///
/// Every one of them is decidable the moment the protocol is on, which is the argument for turning
/// it on rather than for guessing better here.
fn c0_key(b: u8) -> Option<TermKey> {
    let key = match b {
        // NUL is Ctrl-Space (Ctrl-@ on the ASCII table the wiring came from).
        0x00 => TermKey::new(VKEY_SPACE, CEF_CONTROL_DOWN, 0, None),
        0x08 | 0x7F => TermKey::new(VKEY_BACK, 0, 0, None),
        0x09 => TermKey::new(VKEY_TAB, 0, 0, None),
        0x0A | 0x0D => TermKey::new(VKEY_RETURN, 0, 0, None),
        0x1B => return None, // Handled by `parse_escape`, which knows what may follow it.
        // Ctrl-A … Ctrl-Z. CEF puts the control code itself in `character` (see the `Ctrl-V` case
        // in `bindings.rs`'s own tests), so that is what goes here; `from_cef` ignores it and reads
        // the US table, which is the whole point of the latched-modifier branch.
        0x01..=0x1A => {
            let vkey = i32::from(b - 1) + 0x41;
            TermKey::new(vkey, CEF_CONTROL_DOWN, u16::from(b), None)
        }
        0x1C => TermKey::new(VKEY_OEM_5, CEF_CONTROL_DOWN, u16::from(b), None),
        0x1D => TermKey::new(VKEY_OEM_6, CEF_CONTROL_DOWN, u16::from(b), None),
        // Ctrl-^ and Ctrl-_ are Shift keystrokes on a US keyboard, and saying so is what makes
        // `us_layout_char` produce `^` and `_` rather than `6` and `-`.
        0x1E => TermKey::new(0x36, CEF_CONTROL_DOWN | CEF_SHIFT_DOWN, u16::from(b), None),
        0x1F => TermKey::new(VKEY_OEM_MINUS, CEF_CONTROL_DOWN | CEF_SHIFT_DOWN, u16::from(b), None),
        _ => return None,
    };
    Some(key)
}

enum Utf8 {
    Char(char, usize),
    Incomplete,
    Invalid(usize),
}

/// One UTF-8 scalar off the front of the buffer, or the news that it has not all arrived.
///
/// A terminal hands over a multi-byte character in whatever pieces the read happened to end on, so
/// half a `ф` is a normal thing to see and must not become a replacement character in the page.
/// `std::str::from_utf8` does the validating — overlong forms and surrogates included — once the
/// length byte says the bytes are all here.
fn decode_utf8(input: &[u8]) -> Utf8 {
    let first = input[0];
    let len = match first {
        0x00..=0x7F => 1,
        0xC2..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF4 => 4,
        // A continuation byte or an invalid leader: nothing can start here.
        _ => return Utf8::Invalid(1),
    };
    if input.len() < len {
        // Everything present so far must at least be a plausible prefix, or waiting is pointless.
        if input[1..].iter().all(|b| (0x80..=0xBF).contains(b)) {
            return Utf8::Incomplete;
        }
        return Utf8::Invalid(1);
    }
    match std::str::from_utf8(&input[..len]) {
        Ok(s) => match s.chars().next() {
            Some(c) => Utf8::Char(c, len),
            None => Utf8::Invalid(1),
        },
        Err(_) => Utf8::Invalid(1),
    }
}

// ---------------------------------------------------------------------------------------------
// Windows virtual key codes
// ---------------------------------------------------------------------------------------------
//
// The subset this module needs, named rather than spelled `0x25` at every use. `bindings.rs` has
// the same numbers on the other side of the seam (`named_key_for_vkey`, `is_modifier_vkey`,
// `us_layout_char`) and this list is deliberately only the constants — the *meaning* of each code
// stays in the one file that owns it.

const VKEY_BACK: i32 = 0x08;
const VKEY_TAB: i32 = 0x09;
const VKEY_CLEAR: i32 = 0x0C;
const VKEY_RETURN: i32 = 0x0D;
const VKEY_PAUSE: i32 = 0x13;
const VKEY_CAPITAL: i32 = 0x14;
const VKEY_ESCAPE: i32 = 0x1B;
const VKEY_SPACE: i32 = 0x20;
const VKEY_PRIOR: i32 = 0x21;
const VKEY_NEXT: i32 = 0x22;
const VKEY_END: i32 = 0x23;
const VKEY_HOME: i32 = 0x24;
const VKEY_LEFT: i32 = 0x25;
const VKEY_UP: i32 = 0x26;
const VKEY_RIGHT: i32 = 0x27;
const VKEY_DOWN: i32 = 0x28;
const VKEY_SNAPSHOT: i32 = 0x2C;
const VKEY_INSERT: i32 = 0x2D;
const VKEY_DELETE: i32 = 0x2E;
const VKEY_LWIN: i32 = 0x5B;
const VKEY_RWIN: i32 = 0x5C;
const VKEY_MENU_APP: i32 = 0x5D;
const VKEY_NUMPAD0: i32 = 0x60;
const VKEY_MULTIPLY: i32 = 0x6A;
const VKEY_ADD: i32 = 0x6B;
const VKEY_SEPARATOR: i32 = 0x6C;
const VKEY_SUBTRACT: i32 = 0x6D;
const VKEY_DECIMAL: i32 = 0x6E;
const VKEY_DIVIDE: i32 = 0x6F;
const VKEY_F1: i32 = 0x70;
const VKEY_NUMLOCK: i32 = 0x90;
const VKEY_SCROLL: i32 = 0x91;
const VKEY_LSHIFT: i32 = 0xA0;
const VKEY_RSHIFT: i32 = 0xA1;
const VKEY_LCONTROL: i32 = 0xA2;
const VKEY_RCONTROL: i32 = 0xA3;
const VKEY_LMENU: i32 = 0xA4;
const VKEY_RMENU: i32 = 0xA5;
const VKEY_VOLUME_MUTE: i32 = 0xAD;
const VKEY_VOLUME_DOWN: i32 = 0xAE;
const VKEY_VOLUME_UP: i32 = 0xAF;
const VKEY_MEDIA_NEXT_TRACK: i32 = 0xB0;
const VKEY_MEDIA_PREV_TRACK: i32 = 0xB1;
const VKEY_MEDIA_STOP: i32 = 0xB2;
const VKEY_MEDIA_PLAY_PAUSE: i32 = 0xB3;
const VKEY_OEM_PLUS: i32 = 0xBB;
const VKEY_OEM_MINUS: i32 = 0xBD;
const VKEY_OEM_5: i32 = 0xDC;
const VKEY_OEM_6: i32 = 0xDD;

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------
//
// This is the module that can be tested completely without a terminal and without CEF — bytes in,
// three integers out — so it is, and the assertions are written against the *binding spelling* the
// key ends up with rather than against the integers. A test that says `<Ctrl-a>` fails when the
// translation is wrong; a test that says `0x41` also passes when `from_cef` would have refused it.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bindings::{Key, NamedKey, parse_key_sequence};

    /// The binding a key sequence spells, so a test can be written the way a config is.
    fn spelled(keystr: &str) -> KeyInfo {
        let seq = parse_key_sequence(keystr).expect("the test's own keystring must parse");
        assert_eq!(seq.len(), 1, "a test key is one key");
        seq[0]
    }

    /// Parse one event and say what binding it means, going through `KeyInfo::from_cef` — which is
    /// the whole claim this module makes.
    fn binding(input: &[u8]) -> Option<KeyInfo> {
        match next_event(input) {
            Step::Key(k, n) => {
                assert_eq!(n, input.len(), "the test input is exactly one sequence");
                k.to_key_info()
            }
            other => panic!("expected a key, got {other:?}"),
        }
    }

    fn key(input: &[u8]) -> TermKey {
        match next_event(input) {
            Step::Key(k, n) => {
                assert_eq!(n, input.len(), "the test input is exactly one sequence");
                k
            }
            other => panic!("expected a key, got {other:?}"),
        }
    }

    // --- the inverse of the US table --------------------------------------------------------

    #[test]
    fn us_layout_char_inverts_exactly() {
        // Every main-keyboard key, in both shift states, comes back as itself. This is the
        // assertion `us_vkey_for_char`'s doc comment rests on: within this range the mapping is
        // injective, so searching it is a definition and not a heuristic.
        for (first, last) in [(0x30, 0x39), (0x41, 0x5A), (0xBA, 0xC0), (0xDB, 0xDE)] {
            for vkey in first..=last {
                for shift in [false, true] {
                    if let Some(c) = us_layout_char(vkey, shift) {
                        assert_eq!(
                            us_vkey_for_char(c),
                            Some((vkey, shift)),
                            "vkey {vkey:#x} shift {shift} types {c:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn a_character_no_us_key_types_has_no_virtual_key() {
        assert_eq!(us_vkey_for_char('ф'), None);
        assert_eq!(us_vkey_for_char('€'), None);
        // Space is not in the US table at all — `vkey_for_text_char` is where it is answered.
        assert_eq!(us_vkey_for_char(' '), None);
        assert_eq!(vkey_for_text_char(' '), Some(VKEY_SPACE));
    }

    // --- the kitty `u` form, letters --------------------------------------------------------

    #[test]
    fn plain_a() {
        assert_eq!(binding(b"\x1b[97u"), Some(spelled("a")));
        // With associated text reported, which is the form bru actually asks for.
        assert_eq!(binding(b"\x1b[97;;97u"), Some(spelled("a")));
        assert_eq!(key(b"\x1b[97;;97u").text, Some('a'));
    }

    #[test]
    fn shift_a_with_the_shifted_key_reported() {
        assert_eq!(binding(b"\x1b[97:65;2u"), Some(spelled("A")));
        assert_eq!(key(b"\x1b[97:65;2u").text, Some('A'));
    }

    #[test]
    fn shift_a_without_alternate_keys() {
        // No shifted key reported: the uppercase of the key itself is right for a letter, and the
        // Shift must survive or `A` would be spelled `a`.
        assert_eq!(binding(b"\x1b[97;2u"), Some(spelled("A")));
    }

    #[test]
    fn shift_one_is_an_exclamation_mark() {
        // Not a letter, so the character is left at 0 on purpose and the US shifted table answers.
        assert_eq!(binding(b"\x1b[49;2u"), Some(spelled("!")));
        // And when the terminal does report the shifted key, that is used instead.
        assert_eq!(binding(b"\x1b[49:33;2u"), Some(spelled("!")));
    }

    #[test]
    fn ctrl_a() {
        assert_eq!(binding(b"\x1b[97;5u"), Some(spelled("<Ctrl-a>")));
        assert_eq!(key(b"\x1b[97;5u").modifiers, CEF_CONTROL_DOWN);
        // Ctrl types nothing into the page.
        assert_eq!(key(b"\x1b[97;5u").text, None);
    }

    #[test]
    fn alt_a() {
        assert_eq!(binding(b"\x1b[97;3u"), Some(spelled("<Alt-a>")));
    }

    #[test]
    fn ctrl_shift_f() {
        assert_eq!(binding(b"\x1b[102;6u"), Some(spelled("<Ctrl-Shift-F>")));
    }

    #[test]
    fn super_is_meta() {
        assert_eq!(binding(b"\x1b[97;9u"), Some(spelled("<Meta-a>")));
        // kitty's separate meta bit lands in the same place.
        assert_eq!(binding(b"\x1b[97;33u"), Some(spelled("<Meta-a>")));
    }

    #[test]
    fn ctrl_c_and_ctrl_d() {
        assert_eq!(binding(b"\x1b[99;5u"), Some(spelled("<Ctrl-c>")));
        assert_eq!(binding(b"\x1b[100;5u"), Some(spelled("<Ctrl-d>")));
    }

    #[test]
    fn caps_lock_is_state_and_not_a_modifier() {
        // Caps Lock on, `a` pressed: the terminal reports the lock bit and the text `A`. It reaches
        // CEF as the lock flag, and the binding layer — which has no way to write Caps Lock in a
        // binding — sees an ordinary `a`, exactly as qutebrowser does.
        let k = key(b"\x1b[97;65;65u");
        assert_eq!(k.modifiers, CEF_CAPS_LOCK_ON);
        assert_eq!(k.to_key_info(), Some(spelled("a")));
    }

    // --- layouts -----------------------------------------------------------------------------

    #[test]
    fn a_cyrillic_letter_types_itself() {
        // `ф` on the physical `a` key: key 1092, base layout key 97, text 1092.
        let k = key("\x1b[1092::97;;1092u".as_bytes());
        assert_eq!(k.windows_key_code, 0x41);
        assert_eq!(k.text, Some('ф'));
        assert_eq!(k.to_key_info(), Some(spelled("ф")));
    }

    #[test]
    fn a_cyrillic_letter_with_ctrl_is_the_physical_key() {
        // The claim the whole module is built on: `<Ctrl-a>` stays `<Ctrl-a>` under `bg`, because
        // the base layout key names the physical key and `from_cef` reads the US table.
        assert_eq!(binding("\x1b[1092::97;5u".as_bytes()), Some(spelled("<Ctrl-a>")));
    }

    #[test]
    fn shift_and_a_cyrillic_letter_without_alternate_keys() {
        // No shifted key reported, so the uppercase of the key itself is used — `Ф`, not the `A`
        // the US table would have supplied.
        let k = key("\x1b[1092::97;2u".as_bytes());
        assert_eq!(k.to_key_info(), Some(spelled("Ф")));
    }

    #[test]
    fn a_key_with_no_us_counterpart_and_a_modifier_is_lost() {
        // Named here rather than hidden: with no base layout key there is no virtual key, and
        // `from_cef` ignores the character whenever Ctrl is down. The event is parsed, the binding
        // is not spellable, and nothing pretends otherwise.
        let k = key("\x1b[1092;5u".as_bytes());
        assert_eq!(k.windows_key_code, 0);
        assert_eq!(k.to_key_info(), None);
    }

    // --- named keys ---------------------------------------------------------------------------

    #[test]
    fn escape_enter_tab_backspace_space() {
        assert_eq!(binding(b"\x1b[27u"), Some(spelled("<Escape>")));
        assert_eq!(binding(b"\x1b[13u"), Some(spelled("<Return>")));
        assert_eq!(binding(b"\x1b[9u"), Some(spelled("<Tab>")));
        assert_eq!(binding(b"\x1b[127u"), Some(spelled("<Backspace>")));
        assert_eq!(binding(b"\x1b[32u"), Some(spelled("<Space>")));
        assert_eq!(binding(b"\x1b[27;5u"), Some(spelled("<Ctrl-Escape>")));
    }

    #[test]
    fn space_is_a_named_key_and_not_a_character() {
        // A `Char(' ')` would make `<Space>` unbindable, so this is asserted at the key and not
        // only at the spelling.
        assert_eq!(key(b"\x1b[32u").to_key_info().unwrap().key, Key::Named(NamedKey::Space));
        assert_eq!(key(b" ").to_key_info().unwrap().key, Key::Named(NamedKey::Space));
    }

    #[test]
    fn the_arrows_in_all_three_spellings() {
        for (csi, ss3, spelling) in [
            (&b"\x1b[A"[..], &b"\x1bOA"[..], "<Up>"),
            (&b"\x1b[B"[..], &b"\x1bOB"[..], "<Down>"),
            (&b"\x1b[C"[..], &b"\x1bOC"[..], "<Right>"),
            (&b"\x1b[D"[..], &b"\x1bOD"[..], "<Left>"),
        ] {
            assert_eq!(binding(csi), Some(spelled(spelling)), "CSI form of {spelling}");
            assert_eq!(binding(ss3), Some(spelled(spelling)), "SS3 form of {spelling}");
        }
        // And the modified form, which is where the dummy first parameter comes from.
        assert_eq!(binding(b"\x1b[1;5A"), Some(spelled("<Ctrl-Up>")));
        assert_eq!(binding(b"\x1b[1;2D"), Some(spelled("<Shift-Left>")));
    }

    #[test]
    fn home_end_page_and_editing_keys() {
        assert_eq!(binding(b"\x1b[H"), Some(spelled("<Home>")));
        assert_eq!(binding(b"\x1b[F"), Some(spelled("<End>")));
        assert_eq!(binding(b"\x1b[1~"), Some(spelled("<Home>")));
        assert_eq!(binding(b"\x1b[4~"), Some(spelled("<End>")));
        assert_eq!(binding(b"\x1b[7~"), Some(spelled("<Home>")));
        assert_eq!(binding(b"\x1b[8~"), Some(spelled("<End>")));
        assert_eq!(binding(b"\x1b[5~"), Some(spelled("<PgUp>")));
        assert_eq!(binding(b"\x1b[6~"), Some(spelled("<PgDown>")));
        assert_eq!(binding(b"\x1b[2~"), Some(spelled("<Insert>")));
        assert_eq!(binding(b"\x1b[3~"), Some(spelled("<Delete>")));
        assert_eq!(binding(b"\x1b[6;5~"), Some(spelled("<Ctrl-PgDown>")));
        assert_eq!(binding(b"\x1b[29~"), Some(spelled("<Menu>")));
    }

    #[test]
    fn the_function_keys() {
        assert_eq!(binding(b"\x1bOP"), Some(spelled("<F1>")));
        assert_eq!(binding(b"\x1b[1;2P"), Some(spelled("<Shift-F1>")));
        assert_eq!(binding(b"\x1b[11~"), Some(spelled("<F1>")));
        assert_eq!(binding(b"\x1b[13~"), Some(spelled("<F3>")));
        assert_eq!(binding(b"\x1b[15~"), Some(spelled("<F5>")));
        assert_eq!(binding(b"\x1b[17~"), Some(spelled("<F6>")));
        assert_eq!(binding(b"\x1b[21~"), Some(spelled("<F10>")));
        assert_eq!(binding(b"\x1b[23~"), Some(spelled("<F11>")));
        assert_eq!(binding(b"\x1b[24~"), Some(spelled("<F12>")));
        // kitty's private block for F13 upwards.
        assert_eq!(binding(b"\x1b[57376u"), Some(spelled("<F13>")));
        assert_eq!(binding(b"\x1b[57387u"), Some(spelled("<F24>")));
    }

    #[test]
    fn f25_and_above_have_no_virtual_key_and_are_dropped() {
        // Windows stops at F24 and so does `named_key_for_vkey`; folding F25 onto something else
        // would be inventing a key.
        assert_eq!(next_event(b"\x1b[57388u"), Step::Ignored(8));
    }

    #[test]
    fn shift_tab() {
        assert_eq!(binding(b"\x1b[Z"), Some(spelled("<Shift-Tab>")));
        // `<backtab>` is the same key, which is `bindings.rs`'s own decision.
        assert_eq!(binding(b"\x1b[Z"), Some(spelled("<Shift-backtab>")));
    }

    #[test]
    fn the_keypad_carries_its_flag() {
        let k = key(b"\x1b[57400u"); // KP_1
        assert_eq!(k.windows_key_code, 0x61);
        assert_eq!(k.modifiers & CEF_IS_KEY_PAD, CEF_IS_KEY_PAD);
        assert_eq!(k.to_key_info(), Some(spelled("<Num+1>")));

        assert_eq!(binding(b"\x1b[57414u"), Some(spelled("<Num+Return>")));
        assert_eq!(key(b"\x1b[57413u").windows_key_code, VKEY_ADD);
        assert_eq!(key(b"\x1bOp").windows_key_code, VKEY_NUMPAD0);
        assert_eq!(key(b"\x1bOp").modifiers & CEF_IS_KEY_PAD, CEF_IS_KEY_PAD);
    }

    #[test]
    fn a_bare_modifier_starts_no_chain() {
        // `from_cef` returns None for these, which is what keeps the Ctrl of `<Ctrl-a>` from
        // arriving as a key of its own and breaking the chain.
        for code in [57441u32, 57442, 57443, 57444, 57447, 57448, 57449, 57453] {
            let seq = format!("\x1b[{code};1u");
            assert_eq!(key(seq.as_bytes()).to_key_info(), None, "code {code}");
        }
        // Caps Lock and Num Lock as keys, likewise.
        assert_eq!(key(b"\x1b[57358u").to_key_info(), None);
        assert_eq!(key(b"\x1b[57360u").to_key_info(), None);
    }

    // --- event types --------------------------------------------------------------------------

    #[test]
    fn press_repeat_and_release() {
        assert_eq!(key(b"\x1b[97;1:1u").event, KeyEventKind::Press);
        assert_eq!(key(b"\x1b[97;1:2u").event, KeyEventKind::Repeat);
        assert_eq!(key(b"\x1b[97;1:3u").event, KeyEventKind::Release);

        assert!(key(b"\x1b[97;1:2u").is_press());
        assert!(!key(b"\x1b[97;1:3u").is_press());
    }

    #[test]
    fn a_release_is_recognised_and_then_deliberately_dropped() {
        let k = key(b"\x1b[97;1:3u");
        // It parsed fully — the virtual key is there for the CEF key-up.
        assert_eq!(k.windows_key_code, 0x41);
        // And it cannot reach the binding trie, which would otherwise fire everything twice.
        assert_eq!(k.to_key_info(), None);
        // A repeat, by contrast, is a press.
        assert_eq!(key(b"\x1b[97;1:2u").to_key_info(), Some(spelled("a")));
    }

    #[test]
    fn a_release_with_modifiers_still_parses_its_modifiers() {
        let k = key(b"\x1b[97;5:3u");
        assert_eq!(k.modifiers, CEF_CONTROL_DOWN);
        assert_eq!(k.event, KeyEventKind::Release);
    }

    // --- legacy input -------------------------------------------------------------------------

    #[test]
    fn a_bare_byte_is_a_key() {
        assert_eq!(binding(b"a"), Some(spelled("a")));
        assert_eq!(binding(b"A"), Some(spelled("A")));
        assert_eq!(binding(b"j"), Some(spelled("j")));
        assert_eq!(binding(b"!"), Some(spelled("!")));
        assert_eq!(binding(b":"), Some(spelled(":")));
        assert_eq!(binding(b"/"), Some(spelled("/")));
        assert_eq!(binding(b"?"), Some(spelled("?")));
    }

    #[test]
    fn an_uppercase_byte_keeps_its_shift() {
        // `from_cef` strips a Shift it was not given, so the US table has to put it back or `A`
        // comes out spelled `a`.
        assert_eq!(key(b"A").modifiers, CEF_SHIFT_DOWN);
        assert_ne!(binding(b"A"), Some(spelled("a")));
    }

    #[test]
    fn the_c0_controls_are_ctrl_keys() {
        assert_eq!(binding(b"\x01"), Some(spelled("<Ctrl-a>")));
        assert_eq!(binding(b"\x03"), Some(spelled("<Ctrl-c>")));
        assert_eq!(binding(b"\x04"), Some(spelled("<Ctrl-d>")));
        assert_eq!(binding(b"\x16"), Some(spelled("<Ctrl-v>")));
        assert_eq!(binding(b"\x1a"), Some(spelled("<Ctrl-z>")));
        assert_eq!(binding(b"\x00"), Some(spelled("<Ctrl-Space>")));
        assert_eq!(binding(b"\x1c"), Some(spelled("<Ctrl-\\>")));
        assert_eq!(binding(b"\x1d"), Some(spelled("<Ctrl-]>")));
    }

    #[test]
    fn ctrl_v_carries_the_control_code_the_way_cef_does() {
        // `bindings.rs` documents `from_cef(0x56, CTRL, 0x16)` as a real event; the legacy path
        // produces exactly that triple.
        let k = key(b"\x16");
        assert_eq!((k.windows_key_code, k.modifiers, k.character), (0x56, CEF_CONTROL_DOWN, 0x16));
    }

    #[test]
    fn the_three_ancient_collisions() {
        // Tab, not Ctrl-I.
        assert_eq!(binding(b"\x09"), Some(spelled("<Tab>")));
        // Return, not Ctrl-M — and Return again for 0x0A, because `Raw::enter` leaves ICRNL on.
        assert_eq!(binding(b"\x0d"), Some(spelled("<Return>")));
        assert_eq!(binding(b"\x0a"), Some(spelled("<Return>")));
        // Backspace, not Ctrl-H, from either byte.
        assert_eq!(binding(b"\x7f"), Some(spelled("<Backspace>")));
        assert_eq!(binding(b"\x08"), Some(spelled("<Backspace>")));
        // All of them are decidable again under the protocol.
        assert_eq!(binding(b"\x1b[105;5u"), Some(spelled("<Ctrl-i>")));
        assert_eq!(binding(b"\x1b[106;5u"), Some(spelled("<Ctrl-j>")));
        assert_eq!(binding(b"\x1b[104;5u"), Some(spelled("<Ctrl-h>")));
    }

    #[test]
    fn esc_prefixed_is_alt() {
        assert_eq!(binding(b"\x1ba"), Some(spelled("<Alt-a>")));
        assert_eq!(binding(b"\x1bA"), Some(spelled("<Alt-Shift-A>")));
        assert_eq!(binding(b"\x1b1"), Some(spelled("<Alt-1>")));
        // Alt does not type.
        assert_eq!(key(b"\x1ba").text, None);
    }

    #[test]
    fn esc_prefixed_control_is_ctrl_alt() {
        assert_eq!(binding(b"\x1b\x01"), Some(spelled("<Ctrl-Alt-a>")));
    }

    #[test]
    fn esc_before_a_sequence_is_alt_and_that_sequence() {
        // xterm's meta-sends-escape puts the ESC in front of the whole thing.
        assert_eq!(binding(b"\x1b\x1b[A"), Some(spelled("<Alt-Up>")));
    }

    #[test]
    fn a_utf8_letter_arrives_whole_or_not_at_all() {
        // `ф` is two bytes, and a read can end between them.
        assert_eq!(next_event("ф".as_bytes()), Step::Key(text_key('ф'), 2));
        assert_eq!(binding("ф".as_bytes()), Some(spelled("ф")));
        assert_eq!(next_event(&"ф".as_bytes()[..1]), Step::Incomplete);
        // Four-byte scalars too.
        assert_eq!(next_event(&"😀".as_bytes()[..3]), Step::Incomplete);
        let k = key("😀".as_bytes());
        assert_eq!(k.text, Some('😀'));
        // CEF's `character` is one UTF-16 unit and cannot hold it; `text` is why that survives.
        assert_eq!(k.character, 0);
    }

    #[test]
    fn a_cyrillic_byte_with_alt_is_the_legacy_paths_cost() {
        // No base layout key in a bare byte, so no virtual key, so `from_cef` has nothing to read
        // once Alt is latched. Asserted so that the loss is a decision and not a surprise.
        let mut bytes = vec![0x1b];
        bytes.extend_from_slice("ф".as_bytes());
        let k = key(&bytes);
        assert_eq!(k.windows_key_code, 0);
        assert_eq!(k.to_key_info(), None);
    }

    // --- incompleteness -----------------------------------------------------------------------

    #[test]
    fn nothing_yet() {
        assert_eq!(next_event(b""), Step::Incomplete);
        assert_eq!(next_event_at_timeout(b""), Step::Incomplete);
    }

    #[test]
    fn a_half_arrived_sequence_is_never_guessed_at() {
        for partial in [
            &b"\x1b"[..],
            &b"\x1b["[..],
            &b"\x1b[1"[..],
            &b"\x1b[1;"[..],
            &b"\x1b[1;5"[..],
            &b"\x1b[97:65;2"[..],
            &b"\x1bO"[..],
            &b"\x1b\x1b"[..],
            &b"\x1b\x1b["[..],
            &b"\x1b\x1b[1;5"[..],
        ] {
            assert_eq!(next_event(partial), Step::Incomplete, "{partial:?}");
        }
    }

    #[test]
    fn a_lone_escape_needs_the_clock() {
        // The one undecidable case, and the only reason `next_event_at_timeout` exists.
        assert_eq!(next_event(b"\x1b"), Step::Incomplete);
        let escape = TermKey::new(VKEY_ESCAPE, 0, 0, None);
        assert_eq!(next_event_at_timeout(b"\x1b"), Step::Key(escape, 1));
        match next_event_at_timeout(b"\x1b") {
            Step::Key(k, 1) => assert_eq!(k.to_key_info(), Some(spelled("<Escape>"))),
            other => panic!("expected Escape, got {other:?}"),
        }
        // Two of them, still with nothing following, is Alt-Escape.
        match next_event_at_timeout(b"\x1b\x1b") {
            Step::Key(k, 2) => assert_eq!(k.to_key_info(), Some(spelled("<Alt-Escape>"))),
            other => panic!("expected Alt-Escape, got {other:?}"),
        }
        // And with the disambiguate flag on, none of this is reached at all.
        assert_eq!(binding(b"\x1b[27u"), Some(spelled("<Escape>")));
    }

    #[test]
    fn a_sequence_that_never_ends_does_not_wedge_the_loop() {
        let runaway = [&b"\x1b["[..], &b";".repeat(MAX_SEQUENCE + 4)[..]].concat();
        match next_event(&runaway) {
            Step::Invalid(n) => assert!(n > MAX_SEQUENCE, "dropped {n} bytes"),
            other => panic!("expected the cap to fire, got {other:?}"),
        }
    }

    // --- garbage ------------------------------------------------------------------------------

    #[test]
    fn garbage_is_dropped_whole_rather_than_a_byte_at_a_time() {
        // A CSI with a byte that can appear nowhere in one: the whole malformed sequence goes,
        // because feeding `[` and `1` back through as text is the failure this guards.
        match next_event(b"\x1b[1\x07") {
            Step::Invalid(n) => assert_eq!(n, 4),
            other => panic!("expected Invalid, got {other:?}"),
        }
        // A stray continuation byte begins nothing.
        assert_eq!(next_event(&[0x80]), Step::Invalid(1));
        // An invalid leader, likewise.
        assert_eq!(next_event(&[0xFF, 0xFF]), Step::Invalid(1));
        // A truncated scalar followed by something that is not a continuation byte is not worth
        // waiting for.
        assert_eq!(next_event(&[0xD1, 0x41]), Step::Invalid(1));
    }

    /// **The SGR mouse report is data now, not noise.** It used to be `Ignored` with every other
    /// report on this stdin; it became a step of its own the day something wanted to click.
    #[test]
    fn an_sgr_mouse_report_is_read_rather_than_dropped() {
        let Step::Mouse(press, n) = next_event(b"\x1b[<0;10;20M") else {
            panic!("a left press at 10,20")
        };
        assert_eq!((n, press.button, press.x, press.y), (11, 0, 10, 20));
        assert!(press.pressed);
        assert!(!press.motion);

        let Step::Mouse(release, _) = next_event(b"\x1b[<0;10;20m") else {
            panic!("the release of the same press")
        };
        assert!(!release.pressed);

        // The modifiers travel in the button field: 16 is ctrl, and it must not be read as a button.
        let Step::Mouse(ctrl, _) = next_event(b"\x1b[<16;5;5M") else {
            panic!("a ctrl-click")
        };
        assert_eq!(ctrl.button, 0);
        assert_eq!(ctrl.modifiers, super::CEF_CONTROL_DOWN);

        // Bit 32 is motion; the button underneath it is still the button.
        let Step::Mouse(drag, _) = next_event(b"\x1b[<32;7;7M") else {
            panic!("a drag")
        };
        assert!(drag.motion);
        assert_eq!(drag.button, 0);

        // The wheel is reported as buttons 64 and 65 rather than as a scroll.
        let Step::Mouse(wheel, _) = next_event(b"\x1b[<64;1;1M") else {
            panic!("a wheel notch")
        };
        assert_eq!(wheel.button, 64);

        // A `CSI <` that is not three numbers is not a report to guess at.
        assert_eq!(next_event(b"\x1b[<0;10M"), Step::Ignored(8));
        assert_eq!(next_event(b"\x1b[<0;10;20;30M"), Step::Ignored(14));
        // And half of one is not one yet.
        assert_eq!(next_event(b"\x1b[<0;10;2"), Step::Incomplete);
    }

    #[test]
    fn reports_that_share_the_keyboards_stdin_are_ignored_and_not_typed() {
        // The X10 form, whose three trailing bytes are not parameters. Still ignored: bru asks the
        // terminal for SGR, so an X10 report is a terminal answering a question nobody asked, and
        // its coordinates cap at 223 anyway.
        assert_eq!(next_event(b"\x1b[M\x20\x21\x22"), Step::Ignored(6));
        // The rest of what shares this stdin.
        assert_eq!(next_event(b"\x1b[M\x20"), Step::Incomplete);
        // Focus in and out.
        assert_eq!(next_event(b"\x1b[I"), Step::Ignored(3));
        assert_eq!(next_event(b"\x1b[O"), Step::Ignored(3));
        // The reply to "what keyboard flags have you got".
        assert_eq!(next_event(b"\x1b[?31u"), Step::Ignored(6));
        // A device attributes reply.
        assert_eq!(next_event(b"\x1b[?62;c"), Step::Ignored(7));
        // The cursor position report `term.rs` asks for — and F3, which shares its final byte.
        assert_eq!(next_event(b"\x1b[24;80R"), Step::Ignored(8));
        assert_eq!(binding(b"\x1b[1;5R"), Some(spelled("<Ctrl-F3>")));
        // A DCS reply, such as the one a tmux passthrough or an XTGETTCAP query produces.
        assert_eq!(next_event(b"\x1bP>|kitty\x1b\\"), Step::Ignored(11));
        assert_eq!(next_event(b"\x1b]11;rgb:00/00/00\x07"), Step::Ignored(18));
        assert_eq!(next_event(b"\x1bP>|kitty"), Step::Incomplete);
    }

    #[test]
    fn bracketed_paste_markers_are_not_keys() {
        assert_eq!(next_event(b"\x1b[200~"), Step::Ignored(6));
        assert_eq!(next_event(b"\x1b[201~"), Step::Ignored(6));
    }

    // --- streaming ----------------------------------------------------------------------------

    #[test]
    fn several_events_come_out_of_one_buffer_in_order() {
        // What the read loop actually does: keep asking, keep dropping what was consumed.
        let mut buf: &[u8] = b"gg\x1b[97;5u\x1b[A\x1b[<0;1;1M\x1bOP";
        let mut got = Vec::new();
        let mut clicks: Vec<TermMouse> = Vec::new();
        loop {
            match next_event(buf) {
                Step::Key(k, n) => {
                    got.push(k.to_key_info().map(|i| i.to_string()));
                    buf = &buf[n..];
                }
                // The click in the middle of the stream is now a step of its own, and the keys
                // either side of it must still come out in order.
                Step::Mouse(m, n) => {
                    clicks.push(m);
                    buf = &buf[n..];
                }
                Step::Resize(_, n) | Step::Ignored(n) | Step::Invalid(n) => buf = &buf[n..],
                Step::Incomplete => break,
            }
        }
        assert!(buf.is_empty(), "the whole buffer was consumed");
        assert_eq!(clicks.len(), 1, "the mouse report was read once");
        assert_eq!(clicks[0].button, 0);
        assert_eq!((clicks[0].x, clicks[0].y), (1, 1));
        assert!(clicks[0].pressed);
        let got: Vec<String> = got.into_iter().map(|s| s.unwrap_or_default()).collect();
        assert_eq!(got, ["g", "g", "<Ctrl+a>", "<Up>", "<F1>"]);
    }

    /// A mode 2048 report is an event, not noise: the grid in the escape's order, the pixels
    /// swapped into `(width, height)`, and the whole sequence consumed.
    #[test]
    fn an_in_band_resize_report_is_a_step_of_its_own() {
        let report = b"\x1b[48;54;110;1350;990t";
        assert_eq!(
            next_event(report),
            Step::Resize(
                TermResize { rows: 54, cols: 110, width: 990, height: 1350 },
                report.len()
            )
        );
        // The pixel fields are allowed to be zero — the terminal declining to say.
        assert_eq!(
            next_event(b"\x1b[48;54;110;0;0t"),
            Step::Resize(TermResize { rows: 54, cols: 110, width: 0, height: 0 }, 16)
        );
        // No grid is no report, and any other `t` is a reply to drop — consumed either way.
        assert_eq!(next_event(b"\x1b[48;0;110;1350;990t"), Step::Ignored(20));
        assert_eq!(next_event(b"\x1b[4;1350;990t"), Step::Ignored(13));
        assert_eq!(next_event(b"\x1b[48;54;110;1350;990;7t"), Step::Ignored(23), "six fields");
        // Split across reads it waits like every other sequence.
        assert_eq!(next_event(b"\x1b[48;54;1"), Step::Incomplete);
    }

    #[test]
    fn a_key_split_across_two_reads_survives_it() {
        let whole = b"\x1b[1092::97;5u";
        for split in 1..whole.len() {
            assert_eq!(next_event(&whole[..split]), Step::Incomplete, "split at {split}");
        }
        assert_eq!(binding(whole), Some(spelled("<Ctrl-a>")));
    }

    #[test]
    fn the_consumed_count_matches_the_variant() {
        assert_eq!(next_event(b"a").consumed(), 1);
        assert_eq!(next_event(b"\x1b[A").consumed(), 3);
        assert_eq!(next_event(b"\x1b[I").consumed(), 3);
        assert_eq!(next_event(b"\x1b").consumed(), 0);
    }
}
