//! Key notation, the binding trie, and the key-sequence parser.
//!
//! A behavioural port of qutebrowser 3.7.0's `keyinput/keyutils.py` (how a key is spelled) and
//! `keyinput/basekeyparser.py` (`BindingTrie`, `MatchResult`, `_match_count`, `handle`). The
//! spelling is copied exactly — `j`, `J`, `<Ctrl-a>`, `<Shift-Escape>` — so a binding written in
//! `~/.config/bru/config.lua` reads identically to the same binding in a qutebrowser config.
//!
//! **Nothing here may reference `mlua`.** This is the key path: pressing `j` runs
//! [`KeyParsers::handle`] and nothing else. Lua exists only in `src/config.rs`, only at startup,
//! and its only output is the owned [`BindingTrie`] this module consumes.
//!
//! Nothing here touches CEF either. [`KeyInfo::from_cef`] takes the three plain integers a
//! `cef::KeyEvent` carries and returns a value; `src/keys.rs` supplies them.

use crate::commands::Command;
use crate::modes::Mode;
use std::collections::HashMap;
use std::fmt;

// ---------------------------------------------------------------------------------------------
// Modifiers
// ---------------------------------------------------------------------------------------------

/// The modifier keys held during a keypress.
///
/// `AltGr` is deliberately absent: `KeySequence.append_event` drops `GroupSwitchModifier`
/// unconditionally, because there is no way to write it in a binding. Caps Lock and Num Lock are
/// absent because they are lock state, not modifiers.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub struct Modifiers(u8);

impl Modifiers {
    pub const NONE: Modifiers = Modifiers(0);
    pub const CTRL: Modifiers = Modifiers(1 << 0);
    pub const ALT: Modifiers = Modifiers(1 << 1);
    pub const SHIFT: Modifiers = Modifiers(1 << 2);
    pub const META: Modifiers = Modifiers(1 << 3);
    /// The key came from the numeric keypad. Stripped on the second match attempt, which is the
    /// whole reason `_match_without_modifiers` exists.
    pub const KEYPAD: Modifiers = Modifiers(1 << 4);

    // Bit values from `cef_event_flags_t`, sys bindings x86_64_unknown_linux_gnu.rs:2699–2724.
    const CEF_SHIFT_DOWN: u32 = 2;
    const CEF_CONTROL_DOWN: u32 = 4;
    const CEF_ALT_DOWN: u32 = 8;
    const CEF_COMMAND_DOWN: u32 = 128;
    const CEF_IS_KEY_PAD: u32 = 512;

    /// Translate CEF's `KeyEvent::modifiers` bitfield.
    pub fn from_cef(modifiers: u32) -> Modifiers {
        let mut m = Modifiers::NONE;
        if modifiers & Self::CEF_SHIFT_DOWN != 0 {
            m |= Modifiers::SHIFT;
        }
        if modifiers & Self::CEF_CONTROL_DOWN != 0 {
            m |= Modifiers::CTRL;
        }
        if modifiers & Self::CEF_ALT_DOWN != 0 {
            m |= Modifiers::ALT;
        }
        if modifiers & Self::CEF_COMMAND_DOWN != 0 {
            m |= Modifiers::META;
        }
        if modifiers & Self::CEF_IS_KEY_PAD != 0 {
            m |= Modifiers::KEYPAD;
        }
        // EVENTFLAG_ALTGR_DOWN is intentionally not mapped.
        m
    }

    pub fn contains(self, other: Modifiers) -> bool {
        self.0 & other.0 == other.0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    fn without(self, other: Modifiers) -> Modifiers {
        Modifiers(self.0 & !other.0)
    }
}

impl std::ops::BitOr for Modifiers {
    type Output = Modifiers;
    fn bitor(self, rhs: Modifiers) -> Modifiers {
        Modifiers(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for Modifiers {
    fn bitor_assign(&mut self, rhs: Modifiers) {
        self.0 |= rhs.0;
    }
}

impl fmt::Display for Modifiers {
    /// The `Ctrl+Alt+Shift+Meta+` prefix, in Qt's `QKeySequence::toString` order.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (bit, name) in [
            (Modifiers::CTRL, "Ctrl+"),
            (Modifiers::ALT, "Alt+"),
            (Modifiers::SHIFT, "Shift+"),
            (Modifiers::META, "Meta+"),
            (Modifiers::KEYPAD, "Num+"),
        ] {
            if self.contains(bit) {
                f.write_str(name)?;
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------------------------

/// A key that has a name rather than a character.
///
/// Display spellings are Qt's, because that is what qutebrowser's configs and docs use: `Ins`, not
/// `Insert`; `PgDown`, not `PageDown`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum NamedKey {
    Escape,
    Tab,
    Backspace,
    Return,
    Enter,
    Insert,
    Delete,
    Home,
    End,
    Left,
    Up,
    Right,
    Down,
    PgUp,
    PgDown,
    Space,
    Menu,
    /// The dedicated browser-back key. Bound to `back` by default, spelled `<back>`.
    BrowserBack,
    /// The dedicated browser-forward key, spelled `<forward>`.
    BrowserForward,
    F(u8),
}

impl NamedKey {
    fn display(self) -> String {
        match self {
            NamedKey::Escape => "Escape".into(),
            NamedKey::Tab => "Tab".into(),
            NamedKey::Backspace => "Backspace".into(),
            NamedKey::Return => "Return".into(),
            NamedKey::Enter => "Enter".into(),
            NamedKey::Insert => "Ins".into(),
            NamedKey::Delete => "Del".into(),
            NamedKey::Home => "Home".into(),
            NamedKey::End => "End".into(),
            NamedKey::Left => "Left".into(),
            NamedKey::Up => "Up".into(),
            NamedKey::Right => "Right".into(),
            NamedKey::Down => "Down".into(),
            NamedKey::PgUp => "PgUp".into(),
            NamedKey::PgDown => "PgDown".into(),
            NamedKey::Space => "Space".into(),
            NamedKey::Menu => "Menu".into(),
            NamedKey::BrowserBack => "back".into(),
            NamedKey::BrowserForward => "forward".into(),
            NamedKey::F(n) => format!("F{n}"),
        }
    }

    /// Look a key up by the lowercase name used inside `<...>`.
    ///
    /// `_parse_special_key` lowercases the whole body, so `<Escape>`, `<escape>` and `<ESCAPE>`
    /// are the same binding, exactly as in qutebrowser.
    fn from_name(name: &str) -> Option<NamedKey> {
        let named = match name {
            "escape" | "esc" => NamedKey::Escape,
            "tab" => NamedKey::Tab,
            // Shift+Tab arrives as Tab with the Shift bit; `KeySequence.append_event` rewrites
            // Qt's Key_Backtab the same way, "because nobody would configure Shift-Backtab".
            "backtab" => NamedKey::Tab,
            "backspace" => NamedKey::Backspace,
            "return" => NamedKey::Return,
            "enter" => NamedKey::Enter,
            "ins" | "insert" => NamedKey::Insert,
            "del" | "delete" => NamedKey::Delete,
            "home" => NamedKey::Home,
            "end" => NamedKey::End,
            "left" => NamedKey::Left,
            "up" => NamedKey::Up,
            "right" => NamedKey::Right,
            "down" => NamedKey::Down,
            "pgup" | "pageup" => NamedKey::PgUp,
            "pgdown" | "pagedown" => NamedKey::PgDown,
            "space" => NamedKey::Space,
            "menu" => NamedKey::Menu,
            "back" => NamedKey::BrowserBack,
            "forward" => NamedKey::BrowserForward,
            _ => {
                let n: u8 = name.strip_prefix('f')?.parse().ok()?;
                if (1..=24).contains(&n) {
                    NamedKey::F(n)
                } else {
                    return None;
                }
            }
        };
        Some(named)
    }
}

/// One key, without modifiers.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Key {
    /// A key that types a character, stored in qutebrowser's canonical form: letters uppercase
    /// (`Key_A` is `0x41` whether or not Shift was held), everything else as produced.
    Char(char),
    /// A key with a name.
    Named(NamedKey),
}

/// A key plus the modifiers held with it. qutebrowser's `keyutils.KeyInfo`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct KeyInfo {
    pub key: Key,
    pub mods: Modifiers,
}

impl KeyInfo {
    // Built from parsed config and by the tests; the CEF path uses `from_cef` instead.
    #[allow(dead_code)]
    pub fn new(key: Key, mods: Modifiers) -> KeyInfo {
        KeyInfo { key, mods }
    }

    /// Build a key token from what CEF hands `on_pre_key_event`.
    ///
    /// - `windows_key_code` is `KeyEvent::windows_key_code` (a `VKEY_*` value; CEF normalises to
    ///   Windows virtual key codes on every platform).
    /// - `modifiers` is `KeyEvent::modifiers`, a `cef_event_flags_t` bitfield.
    /// - `character` is `KeyEvent::character`, the UTF-16 code unit the keystroke produced. It may
    ///   be `0` on a RAWKEYDOWN — the CHAR event is what reliably carries it — in which case
    ///   [`us_layout_char`] fills in.
    ///
    /// Returns `None` for a keypress that cannot start or continue a chain: a bare modifier key
    /// (`is_modifier_key` in `basekeyparser.handle`) or a code that maps to no key at all.
    ///
    /// When Ctrl, Alt or Meta is held the `character` field is ignored and the US table is used
    /// directly, because a modified keystroke's character is a control code or empty. That also
    /// makes `<Ctrl-a>` mean the physical `a` key under a Cyrillic layout, which is what Chromium
    /// itself does with its shortcuts.
    pub fn from_cef(windows_key_code: i32, modifiers: u32, character: u16) -> Option<KeyInfo> {
        let mut mods = Modifiers::from_cef(modifiers);

        if is_modifier_vkey(windows_key_code) {
            return None;
        }
        if let Some(named) = named_key_for_vkey(windows_key_code) {
            return Some(KeyInfo { key: Key::Named(named), mods });
        }

        let latched = mods.contains(Modifiers::CTRL)
            || mods.contains(Modifiers::ALT)
            || mods.contains(Modifiers::META);
        let produced = if latched {
            us_layout_char(windows_key_code, mods.contains(Modifiers::SHIFT))
        } else {
            char::from_u32(character as u32)
                .filter(|c| !c.is_control())
                .or_else(|| us_layout_char(windows_key_code, mods.contains(Modifiers::SHIFT)))
        }?;

        // `KeySequence.append_event`: "We don't care about a shift modifier with symbols (Shift-:
        // should match a `:` binding even though we typed it with a shift on a US keyboard).
        // However, we *do* care about Shift being involved if we got an upper-case letter."
        if mods == Modifiers::SHIFT && !produced.is_uppercase() {
            mods = Modifiers::NONE;
        }

        Some(KeyInfo { key: Key::Char(canonical_char(produced)), mods })
    }

    /// The text this key would type. Port of `KeyInfo.text()`.
    fn text(&self) -> Option<char> {
        match self.key {
            Key::Named(NamedKey::Space) => Some(' '),
            Key::Named(NamedKey::Tab) => Some('\t'),
            Key::Named(NamedKey::Backspace) => Some('\u{8}'),
            Key::Named(NamedKey::Return) | Key::Named(NamedKey::Enter) => Some('\r'),
            Key::Named(NamedKey::Escape) => Some('\u{1b}'),
            Key::Named(_) => None,
            Key::Char(c) => {
                if self.mods.contains(Modifiers::SHIFT) {
                    Some(c)
                } else {
                    c.to_lowercase().next()
                }
            }
        }
    }

    /// `is_non_alnum` from `modeman._handle_keypress`: a modifier beyond Shift is held, or the key
    /// types nothing visible. With `input.forward_unbound_keys = auto` (the default) these are the
    /// keys normal mode lets through to the page when nothing matched.
    pub fn is_non_alnum(&self) -> bool {
        let has_modifier = !self.mods.without(Modifiers::SHIFT).is_empty();
        has_modifier
            || match self.text() {
                None => true,
                Some(c) => c.is_whitespace(),
            }
    }

    fn strip_keypad(self) -> KeyInfo {
        KeyInfo { key: self.key, mods: self.mods.without(Modifiers::KEYPAD) }
    }
}

impl fmt::Display for KeyInfo {
    /// Port of `KeyInfo.__str__`: `j`, `J`, `<Ctrl-a>`-style `<Ctrl+a>`, `<Shift+Escape>`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Key::Char(c) = self.key {
            if self.mods == Modifiers::NONE {
                return write!(f, "{}", c.to_lowercase());
            }
            if self.mods == Modifiers::SHIFT {
                return write!(f, "{}", c.to_uppercase());
            }
        }
        let name = match self.key {
            // "Use special binding syntax, but <Ctrl-a> instead of <Ctrl-A>".
            Key::Char(c) => c.to_lowercase().to_string(),
            Key::Named(n) => n.display(),
        };
        write!(f, "<{}{}>", self.mods, name)
    }
}

/// The canonical stored form of a character key: letters uppercase, everything else untouched.
///
/// Qt reaches the same place from the other side — `Qt::Key_A` is `0x41` regardless of Shift — and
/// this is what makes `a` and `A` share a trie node while differing in their modifiers.
fn canonical_char(c: char) -> char {
    if c.is_lowercase() { c.to_uppercase().next().unwrap_or(c) } else { c }
}

/// Format a key sequence the way the status bar shows it: `gg`, `3j`, `<Ctrl+a>`.
pub fn sequence_to_string(sequence: &[KeyInfo]) -> String {
    sequence.iter().map(KeyInfo::to_string).collect()
}

// ---------------------------------------------------------------------------------------------
// Windows virtual key codes
// ---------------------------------------------------------------------------------------------

fn is_modifier_vkey(vkey: i32) -> bool {
    // VK_SHIFT, VK_CONTROL, VK_MENU (alt), VK_LWIN, VK_RWIN, VK_CAPITAL, VK_NUMLOCK, VK_LSHIFT..
    matches!(vkey, 0x10..=0x12 | 0x14 | 0x5B | 0x5C | 0x90 | 0x91 | 0xA0..=0xA5)
}

fn named_key_for_vkey(vkey: i32) -> Option<NamedKey> {
    let named = match vkey {
        0x08 => NamedKey::Backspace,
        0x09 => NamedKey::Tab,
        0x0D => NamedKey::Return,
        0x1B => NamedKey::Escape,
        0x20 => NamedKey::Space,
        0x21 => NamedKey::PgUp,
        0x22 => NamedKey::PgDown,
        0x23 => NamedKey::End,
        0x24 => NamedKey::Home,
        0x25 => NamedKey::Left,
        0x26 => NamedKey::Up,
        0x27 => NamedKey::Right,
        0x28 => NamedKey::Down,
        0x2D => NamedKey::Insert,
        0x2E => NamedKey::Delete,
        0x5D => NamedKey::Menu,
        0xA6 => NamedKey::BrowserBack,
        0xA7 => NamedKey::BrowserForward,
        0x70..=0x87 => NamedKey::F((vkey - 0x6F) as u8),
        _ => return None,
    };
    Some(named)
}

/// **The one place that assumes a US keyboard layout.**
///
/// CEF gives a Windows virtual key code, which names a physical key, not the character it types.
/// When `KeyEvent::character` is unavailable — it is `0` on RAWKEYDOWN for some drivers, and is a
/// control code whenever Ctrl is held — this table says what the key would produce on `us`.
///
/// This machine runs `xkb_rules_layout=us,bg` with `grp:alt_shift_toggle` (measured 2026-08-06 in
/// `~/.config/mango/keyboard.conf`), so the second layout genuinely is in use and this table is
/// wrong for unmodified punctuation typed under `bg` whenever `character` was unavailable. See
/// DECISIONS.md item 10. Replacing it means asking the compositor or xkbcommon for the active
/// layout; every caller goes through this function, so the replacement is local.
pub fn us_layout_char(vkey: i32, shift: bool) -> Option<char> {
    const SHIFTED_DIGITS: [char; 10] = [')', '!', '@', '#', '$', '%', '^', '&', '*', '('];

    let c = match vkey {
        0x30..=0x39 => {
            let i = (vkey - 0x30) as usize;
            if shift { SHIFTED_DIGITS[i] } else { (b'0' + i as u8) as char }
        }
        0x41..=0x5A => {
            let c = (b'A' + (vkey - 0x41) as u8) as char;
            if shift { c } else { c.to_ascii_lowercase() }
        }
        // Numeric keypad: always a digit, Shift or not.
        0x60..=0x69 => (b'0' + (vkey - 0x60) as u8) as char,
        0x6A => '*',
        0x6B => '+',
        0x6D => '-',
        0x6E => '.',
        0x6F => '/',
        0xBA => if shift { ':' } else { ';' },
        0xBB => if shift { '+' } else { '=' },
        0xBC => if shift { '<' } else { ',' },
        0xBD => if shift { '_' } else { '-' },
        0xBE => if shift { '>' } else { '.' },
        0xBF => if shift { '?' } else { '/' },
        0xC0 => if shift { '~' } else { '`' },
        0xDB => if shift { '{' } else { '[' },
        0xDC => if shift { '|' } else { '\\' },
        0xDD => if shift { '}' } else { ']' },
        0xDE => if shift { '"' } else { '\'' },
        _ => return None,
    };
    Some(c)
}

// ---------------------------------------------------------------------------------------------
// Parsing a keystring
// ---------------------------------------------------------------------------------------------

/// A binding's key column could not be understood.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct KeyParseError(pub String);

impl fmt::Display for KeyParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Parse a keystring such as `gg`, `J`, `<Ctrl-Shift-N>` or `;b` into a key sequence.
///
/// Port of `_parse_keystring` / `_parse_special_key` / `_parse_single_key`. In particular:
/// an uppercase letter implies Shift, and the body of a `<...>` group is lowercased before
/// anything else, so `<Ctrl-A>` and `<Ctrl-a>` are the same binding — as they are in qutebrowser.
pub fn parse_key_sequence(keystr: &str) -> Result<Vec<KeyInfo>, KeyParseError> {
    let mut out = Vec::new();
    let mut special_body = String::new();
    let mut special = false;

    for c in keystr.chars() {
        if c == '>' {
            if special {
                out.push(parse_special_key(&special_body)?);
                special_body.clear();
                special = false;
            } else {
                out.push(parse_single_key('>')?);
            }
        } else if c == '<' {
            special = true;
        } else if special {
            special_body.push(c);
        } else {
            out.push(parse_single_key(c)?);
        }
    }

    // An unterminated `<` is a literal `<` followed by whatever came after it.
    if special {
        out.push(parse_single_key('<')?);
        for c in special_body.chars() {
            out.push(parse_single_key(c)?);
        }
    }

    if out.is_empty() {
        return Err(KeyParseError("empty keystring".to_string()));
    }
    Ok(out)
}

fn parse_single_key(c: char) -> Result<KeyInfo, KeyParseError> {
    if c.is_control() {
        return Err(KeyParseError(format!("control character {:?} in a keystring", c)));
    }
    let mods = if c.is_uppercase() { Modifiers::SHIFT } else { Modifiers::NONE };
    Ok(KeyInfo { key: Key::Char(canonical_char(c)), mods })
}

fn parse_special_key(body: &str) -> Result<KeyInfo, KeyParseError> {
    let original = body;
    let mut s = body.to_lowercase();
    for (from, to) in [
        ("control", "ctrl"),
        ("windows", "meta"),
        ("mod4", "meta"),
        ("command", "meta"),
        ("cmd", "meta"),
        ("super", "meta"),
        ("mod1", "alt"),
        ("less", "<"),
        ("greater", ">"),
    ] {
        s = s.replace(from, to);
    }
    for m in ["ctrl", "meta", "alt", "shift", "num"] {
        s = s.replace(&format!("{m}-"), &format!("{m}+"));
    }

    let mut mods = Modifiers::NONE;
    loop {
        let mut matched = false;
        for (name, bit) in [
            ("ctrl+", Modifiers::CTRL),
            ("meta+", Modifiers::META),
            ("alt+", Modifiers::ALT),
            ("shift+", Modifiers::SHIFT),
            ("num+", Modifiers::KEYPAD),
        ] {
            if let Some(rest) = s.strip_prefix(name) {
                mods |= bit;
                s = rest.to_string();
                matched = true;
                break;
            }
        }
        if !matched {
            break;
        }
    }

    if s.is_empty() {
        return Err(KeyParseError(format!("<{original}> names no key")));
    }

    let key = if s.chars().count() == 1 {
        let c = s.chars().next().expect("checked non-empty");
        if c.is_control() {
            return Err(KeyParseError(format!("<{original}> names no key")));
        }
        Key::Char(canonical_char(c))
    } else {
        match NamedKey::from_name(&s) {
            Some(n) => Key::Named(n),
            None => return Err(KeyParseError(format!("unknown key <{original}>"))),
        }
    };
    Ok(KeyInfo { key, mods })
}

// ---------------------------------------------------------------------------------------------
// The trie
// ---------------------------------------------------------------------------------------------

/// The three outcomes of matching. `QKeySequence.SequenceMatch` in qutebrowser.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MatchType {
    /// No binding starts with what has been typed. The chain is abandoned.
    NoMatch,
    /// What has been typed is a prefix of at least one binding. Keep waiting.
    PartialMatch,
    /// A binding is complete.
    ExactMatch,
}

/// The result of matching a key sequence against the trie.
///
/// `MatchResult.__post_init__` asserts "command is not None iff ExactMatch"; here the enum makes
/// that unrepresentable rather than asserted.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Match<'a, V> {
    NoMatch,
    Partial,
    Exact(&'a V),
}

impl<V> Match<'_, V> {
    pub fn match_type(&self) -> MatchType {
        match self {
            Match::NoMatch => MatchType::NoMatch,
            Match::Partial => MatchType::PartialMatch,
            Match::Exact(_) => MatchType::ExactMatch,
        }
    }
}

/// A set of bindings, as a trie over key sequences. Port of `basekeyparser.BindingTrie`.
///
/// Generic over the value so that stage 2's hint mode can put hint labels in the same structure,
/// which is what `HintKeyParser.update_bindings` does.
#[derive(Debug, Clone)]
pub struct BindingTrie<V> {
    children: HashMap<KeyInfo, BindingTrie<V>>,
    value: Option<V>,
}

// Written out rather than derived: `#[derive(Default)]` would demand `V: Default`, and a command
// has no sensible default.
impl<V> Default for BindingTrie<V> {
    fn default() -> Self {
        BindingTrie::new()
    }
}

impl<V> BindingTrie<V> {
    pub fn new() -> BindingTrie<V> {
        BindingTrie { children: HashMap::new(), value: None }
    }

    /// Bind a sequence, returning whatever was bound to it before.
    pub fn insert(&mut self, sequence: &[KeyInfo], value: V) -> Option<V> {
        let mut node = self;
        for key in sequence {
            node = node.children.entry(*key).or_insert_with(BindingTrie::new);
        }
        node.value.replace(value)
    }

    /// Unbind a sequence, pruning the branch that is left empty.
    // `bru.unbind`'s half of the trie. Reached through `Config`, not from the key path.
    #[allow(dead_code)]
    pub fn remove(&mut self, sequence: &[KeyInfo]) -> Option<V> {
        let Some((first, rest)) = sequence.split_first() else {
            return self.value.take();
        };
        let child = self.children.get_mut(first)?;
        let removed = child.remove(rest);
        if child.value.is_none() && child.children.is_empty() {
            self.children.remove(first);
        }
        removed
    }

    /// Match a key sequence. Port of `BindingTrie.matches`.
    ///
    /// A node with a value is an exact match even if it also has children (`g` would be, if both
    /// `g` and `gg` were bound); a node with children but no value is partial; a node with neither
    /// only exists in an empty trie.
    pub fn matches(&self, sequence: &[KeyInfo]) -> Match<'_, V> {
        let mut node = self;
        for key in sequence {
            match node.children.get(key) {
                Some(child) => node = child,
                None => return Match::NoMatch,
            }
        }
        match &node.value {
            Some(value) => Match::Exact(value),
            None if !node.children.is_empty() => Match::Partial,
            None => Match::NoMatch,
        }
    }

    /// How many sequences are bound.
    // Trie size, for the tests and for `:bind`'s listing.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        usize::from(self.value.is_some())
            + self.children.values().map(BindingTrie::len).sum::<usize>()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

// ---------------------------------------------------------------------------------------------
// The key parser
// ---------------------------------------------------------------------------------------------

/// What a keypress asks the caller to do.
#[derive(Clone, PartialEq, Debug)]
pub enum KeyAction {
    /// A binding completed. Run it.
    Run { command: Command, count: Option<u32> },
    /// The key extended a chain or was absorbed as a count digit. Nothing to run yet.
    Pending,
    /// Nothing matched. Any pending chain has been discarded.
    NoMatch,
}

/// The full answer to one keypress.
#[derive(Clone, PartialEq, Debug)]
pub struct KeyOutcome {
    pub action: KeyAction,
    /// What the status bar's keystring widget should now show — count and pending chain, e.g.
    /// `3` after `3`, `g` after `g`, empty once something ran.
    pub keystring: String,
    /// Whether `on_pre_key_event` should return 1, i.e. the page never sees this key.
    pub swallow: bool,
}

/// One mode's bindings and its pending state. Port of `basekeyparser.BaseKeyParser`.
#[derive(Debug)]
pub struct KeyParser {
    mode: Mode,
    bindings: BindingTrie<Command>,
    sequence: Vec<KeyInfo>,
    count: String,
}

impl KeyParser {
    pub fn new(mode: Mode, bindings: BindingTrie<Command>) -> KeyParser {
        KeyParser { mode, bindings, sequence: Vec::new(), count: String::new() }
    }

    // For inspecting a mode's table — the tests, and `:bind`'s listing when it lands.
    #[allow(dead_code)]
    pub fn bindings(&self) -> &BindingTrie<Command> {
        &self.bindings
    }

    /// The pending chain, as the status bar shows it.
    pub fn keystring(&self) -> String {
        format!("{}{}", self.count, sequence_to_string(&self.sequence))
    }

    /// Throw away the pending chain and count. `clear_keystring`.
    pub fn clear(&mut self) {
        self.sequence.clear();
        self.count.clear();
    }

    /// Take the pending chain and count out, leaving the parser empty.
    ///
    /// **This exists so that the chain can be per window while the table stays per process.** A
    /// `BindingTrie` is a few hundred nodes and there is no reason to build one per window; a
    /// half-typed `g` is a fact about one window and there is every reason not to share it. So
    /// `BruState::handle_key` swaps this window's pair in before the key and takes it back out
    /// after — see there. Two moves of a `Vec` and a `String`, which is six words and no
    /// allocation: the buffers travel by pointer and are handed straight back.
    pub fn take_pending(&mut self) -> (Vec<KeyInfo>, String) {
        (std::mem::take(&mut self.sequence), std::mem::take(&mut self.count))
    }

    /// Whether anything is half-typed. Cheap enough to ask on the key path, and what lets
    /// `BruState::handle_key` skip the swap entirely for a key that matches outright — which is
    /// almost every key, and certainly `j`.
    pub fn has_pending(&self) -> bool {
        !self.sequence.is_empty() || !self.count.is_empty()
    }

    /// Put a pending chain and count back. The other half of [`KeyParser::take_pending`].
    pub fn set_pending(&mut self, sequence: Vec<KeyInfo>, count: String) {
        self.sequence = sequence;
        self.count = count;
    }

    /// Handle one keypress. Port of `BaseKeyParser.handle` plus the filtering tail of
    /// `modeman._handle_keypress`.
    pub fn handle(&mut self, info: KeyInfo) -> KeyOutcome {
        let mut sequence = self.sequence.clone();
        sequence.push(info);

        let mut match_type = self.bindings.matches(&sequence).match_type();

        // `_match_without_modifiers`: retry with the keypad bit stripped. Note that qutebrowser
        // keeps the stripped sequence whether or not it matched.
        if match_type == MatchType::NoMatch {
            sequence = sequence.iter().map(|k| k.strip_keypad()).collect();
            match_type = self.bindings.matches(&sequence).match_type();
        }

        // `_match_key_mapping` sits here in qutebrowser. bru has no `bindings.key_mappings`, and
        // its NoMatch branch is a no-op, so there is nothing to port.

        if match_type == MatchType::NoMatch && self.match_count(&sequence) {
            // The digit is absorbed into the count and never joins the sequence.
            return KeyOutcome {
                action: KeyAction::Pending,
                keystring: self.keystring(),
                swallow: true,
            };
        }

        self.sequence = sequence;

        match self.bindings.matches(&self.sequence) {
            Match::Exact(command) => {
                let command = command.clone();
                let count = if self.count.is_empty() { None } else { self.count.parse().ok() };
                let overflowed = !self.count.is_empty() && count.is_none();
                self.clear();
                if overflowed {
                    // `_handle_result` reports "Failed to parse count" and clears. The key was
                    // still ours, so it does not reach the page.
                    return KeyOutcome {
                        action: KeyAction::NoMatch,
                        keystring: String::new(),
                        swallow: true,
                    };
                }
                KeyOutcome {
                    action: KeyAction::Run { command, count },
                    keystring: String::new(),
                    swallow: true,
                }
            }
            Match::Partial => KeyOutcome {
                action: KeyAction::Pending,
                keystring: self.keystring(),
                swallow: true,
            },
            Match::NoMatch => {
                self.clear();
                KeyOutcome {
                    action: KeyAction::NoMatch,
                    keystring: String::new(),
                    swallow: self.mode.swallows_unmatched(info.is_non_alnum()),
                }
            }
        }
    }

    /// Port of `_match_count`.
    ///
    /// `input.match_counts` defaults to true and bru does not make it configurable, so it is
    /// assumed. The digit is taken from the *last* key of the sequence, which is why `g3` puts a
    /// count of 3 behind a still-pending `g` — qutebrowser does exactly that.
    ///
    /// A leading `0` is not a count: with an empty count so far, `0` falls through to be an
    /// ordinary key (`g0` is a real binding, `0` alone is not).
    fn match_count(&mut self, sequence: &[KeyInfo]) -> bool {
        if !self.mode.supports_count() {
            return false;
        }
        let Some(last) = sequence.last() else {
            return false;
        };
        let text = last.to_string();
        let mut chars = text.chars();
        let (Some(c), None) = (chars.next(), chars.next()) else {
            return false;
        };
        if !c.is_ascii_digit() {
            return false;
        }
        if self.count.is_empty() && c == '0' {
            return false;
        }
        self.count.push(c);
        true
    }
}

/// One [`KeyParser`] per mode. This is what `src/state.rs` holds and `src/keys.rs` calls.
#[derive(Debug)]
pub struct KeyParsers {
    parsers: HashMap<Mode, KeyParser>,
}

impl KeyParsers {
    /// Build from a per-mode set of tries, as produced by
    /// [`crate::config::Config::into_parsers`].
    pub fn new(mut tries: HashMap<Mode, BindingTrie<Command>>) -> KeyParsers {
        let parsers = Mode::ALL
            .into_iter()
            .map(|mode| {
                let trie = tries.remove(&mode).unwrap_or_default();
                (mode, KeyParser::new(mode, trie))
            })
            .collect();
        KeyParsers { parsers }
    }

    /// Feed a key to the parser for `mode`, on the parser's own pending chain.
    ///
    /// **Not the key path any more.** `BruState::handle_key` reaches `parser_mut` directly, because
    /// it has to swap the *window's* chain in and out around the call — the tables are one set for
    /// the process and the half-typed keys are not. This convenience is what the binding tests use,
    /// and it is exactly right for them: a test has no windows.
    #[cfg(test)]
    pub fn handle(&mut self, mode: Mode, info: KeyInfo) -> KeyOutcome {
        self.parser_mut(mode).handle(info)
    }

    /// Drop the pending chain of a mode. Called when a mode is left — `modeman.leave` clears the
    /// keychain unconditionally (qutebrowser issue 1805).
    pub fn clear(&mut self, mode: Mode) {
        self.parser_mut(mode).clear();
    }

    // Read-only counterpart of `parser_mut`; the key path only ever needs the mutable one.
    #[allow(dead_code)]
    pub fn parser(&self, mode: Mode) -> &KeyParser {
        self.parsers.get(&mode).expect("a parser exists for every Mode::ALL")
    }

    pub fn parser_mut(&mut self, mode: Mode) -> &mut KeyParser {
        self.parsers.get_mut(&mode).expect("a parser exists for every Mode::ALL")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::ScrollDirection;

    fn seq(keystr: &str) -> Vec<KeyInfo> {
        parse_key_sequence(keystr).unwrap_or_else(|e| panic!("{keystr:?}: {e}"))
    }

    fn key(keystr: &str) -> KeyInfo {
        let s = seq(keystr);
        assert_eq!(s.len(), 1, "{keystr:?} is not a single key");
        s[0]
    }

    fn trie(bindings: &[(&str, &str)]) -> BindingTrie<Command> {
        let mut t = BindingTrie::new();
        for (keys, cmd) in bindings {
            t.insert(&seq(keys), crate::commands::parse(cmd).unwrap());
        }
        t
    }

    fn normal(bindings: &[(&str, &str)]) -> KeyParser {
        KeyParser::new(Mode::Normal, trie(bindings))
    }

    // -- notation ------------------------------------------------------------------------------

    #[test]
    fn keystrings_round_trip_through_qutebrowsers_spelling() {
        for s in ["j", "J", "gg", ";b", "yY", "[[", "@", "`", "'", "-", "+", "=", "g$"] {
            assert_eq!(sequence_to_string(&seq(s)), s, "round trip of {s:?}");
        }
        // Uppercase implies Shift; lowercase implies none.
        assert_eq!(key("j"), KeyInfo::new(Key::Char('J'), Modifiers::NONE));
        assert_eq!(key("J"), KeyInfo::new(Key::Char('J'), Modifiers::SHIFT));
        assert_ne!(key("j"), key("J"));
    }

    #[test]
    fn special_keys_are_case_insensitive_inside_the_brackets() {
        // `_parse_special_key` lowercases the body, so these are one binding, not two.
        assert_eq!(key("<Ctrl-A>"), key("<ctrl-a>"));
        assert_eq!(key("<Ctrl-A>"), key("<Control+A>"));
        assert_eq!(key("<Ctrl-A>"), KeyInfo::new(Key::Char('A'), Modifiers::CTRL));
        // ...but Shift is a separate modifier, so <Ctrl-N> and <Ctrl-Shift-N> stay distinct.
        assert_ne!(key("<Ctrl-N>"), key("<Ctrl-Shift-N>"));
        assert_eq!(
            key("<Ctrl-Shift-N>"),
            KeyInfo::new(Key::Char('N'), Modifiers::CTRL | Modifiers::SHIFT)
        );
        assert_eq!(key("<Shift-Escape>"), KeyInfo::new(Key::Named(NamedKey::Escape), Modifiers::SHIFT));
        assert_eq!(key("<F5>"), KeyInfo::new(Key::Named(NamedKey::F(5)), Modifiers::NONE));
        assert_eq!(key("<back>"), KeyInfo::new(Key::Named(NamedKey::BrowserBack), Modifiers::NONE));
        assert_eq!(key("<Ctrl-^>"), KeyInfo::new(Key::Char('^'), Modifiers::CTRL));
        assert_eq!(key("<Alt-Backspace>").mods, Modifiers::ALT);
    }

    #[test]
    fn bad_keystrings_are_rejected() {
        assert!(parse_key_sequence("").is_err());
        assert!(parse_key_sequence("<Ctrl-Nonsense>").is_err());
        assert!(parse_key_sequence("<Ctrl->").is_err());
    }

    #[test]
    fn cef_key_events_produce_the_same_tokens_as_the_config_spelling() {
        const SHIFT: u32 = 2;
        const CTRL: u32 = 4;
        const ALT: u32 = 8;

        // j: VKEY_J with character 'j'.
        assert_eq!(KeyInfo::from_cef(0x4A, 0, b'j' as u16), Some(key("j")));
        // J: VKEY_J, Shift held, character 'J'.
        assert_eq!(KeyInfo::from_cef(0x4A, SHIFT, b'J' as u16), Some(key("J")));
        // Escape, Return, F5, PgDown by virtual key code alone.
        assert_eq!(KeyInfo::from_cef(0x1B, 0, 0), Some(key("<Escape>")));
        assert_eq!(KeyInfo::from_cef(0x1B, SHIFT, 0), Some(key("<Shift-Escape>")));
        assert_eq!(KeyInfo::from_cef(0x0D, 0, 0), Some(key("<Return>")));
        assert_eq!(KeyInfo::from_cef(0x74, 0, 0), Some(key("<F5>")));
        assert_eq!(KeyInfo::from_cef(0x22, CTRL, 0), Some(key("<Ctrl-PgDown>")));
        // Ctrl chords: the character field is a control code or empty, so the layout table runs.
        assert_eq!(KeyInfo::from_cef(0x56, CTRL, 0x16), Some(key("<Ctrl-V>")));
        assert_eq!(KeyInfo::from_cef(0x4E, CTRL | SHIFT, 0), Some(key("<Ctrl-Shift-N>")));
        assert_eq!(KeyInfo::from_cef(0x31, ALT, 0), Some(key("<Alt-1>")));
        // Bare modifier keys start nothing.
        assert_eq!(KeyInfo::from_cef(0x10, SHIFT, 0), None);
        assert_eq!(KeyInfo::from_cef(0x11, CTRL, 0), None);
    }

    #[test]
    fn shift_is_dropped_for_symbols_but_not_for_letters() {
        const SHIFT: u32 = 2;
        // Shift+1 on a US keyboard types `!`; the binding is written `!`, not `<Shift-1>`.
        assert_eq!(KeyInfo::from_cef(0x31, SHIFT, b'!' as u16), Some(key("!")));
        // Shift+; types `:`, and `:` is bound in normal mode.
        assert_eq!(KeyInfo::from_cef(0xBA, SHIFT, b':' as u16), Some(key(":")));
        // Shift+a types `A`, and `A` must not match an `a` binding.
        assert_eq!(KeyInfo::from_cef(0x41, SHIFT, b'A' as u16), Some(key("A")));
        assert_ne!(KeyInfo::from_cef(0x41, SHIFT, b'A' as u16), Some(key("a")));
    }

    #[test]
    fn the_us_layout_table_covers_the_punctuation_the_defaults_bind() {
        // With no character from CEF, these must still resolve. This is the fallback that
        // DECISIONS.md item 10 is about.
        assert_eq!(KeyInfo::from_cef(0xBA, 0, 0), Some(key(";")));
        assert_eq!(KeyInfo::from_cef(0xBA, 2, 0), Some(key(":")));
        assert_eq!(KeyInfo::from_cef(0xBF, 0, 0), Some(key("/")));
        assert_eq!(KeyInfo::from_cef(0xBF, 2, 0), Some(key("?")));
        assert_eq!(KeyInfo::from_cef(0xBD, 0, 0), Some(key("-")));
        assert_eq!(KeyInfo::from_cef(0xBB, 0, 0), Some(key("=")));
        assert_eq!(KeyInfo::from_cef(0xBB, 2, 0), Some(key("+")));
        assert_eq!(KeyInfo::from_cef(0xDB, 0, 0), Some(key("[")));
        assert_eq!(KeyInfo::from_cef(0xC0, 0, 0), Some(key("`")));
        assert_eq!(KeyInfo::from_cef(0xDE, 0, 0), Some(key("'")));
        assert_eq!(KeyInfo::from_cef(0x32, 2, 0), Some(key("@")));
    }

    // -- the trie ------------------------------------------------------------------------------

    #[test]
    fn partial_exact_and_no_match() {
        let t = trie(&[("gg", "scroll-to-perc 0"), ("j", "scroll down")]);

        // `g` is a prefix of `gg` and nothing else: partial.
        assert_eq!(t.matches(&seq("g")).match_type(), MatchType::PartialMatch);
        // `gg` is bound: exact, and it carries the command.
        match t.matches(&seq("gg")) {
            Match::Exact(cmd) => {
                assert_eq!(*cmd, crate::commands::parse("scroll-to-perc 0").unwrap())
            }
            other => panic!("expected an exact match, got {:?}", other.match_type()),
        }
        // `gx` is bound to nothing and is not a prefix of anything: no match.
        assert_eq!(t.matches(&seq("gx")).match_type(), MatchType::NoMatch);
        // Past a leaf is also no match.
        assert_eq!(t.matches(&seq("jj")).match_type(), MatchType::NoMatch);
        // An empty trie has no match even for the empty sequence.
        assert_eq!(BindingTrie::<Command>::new().matches(&[]).match_type(), MatchType::NoMatch);
    }

    #[test]
    fn a_bound_prefix_is_exact_even_with_children() {
        // `g` bound and `gg` bound: `g` matches exactly, the way `BindingTrie.matches` checks the
        // command before the children.
        let t = trie(&[("g", "tab-next"), ("gg", "scroll-to-perc 0")]);
        assert_eq!(t.matches(&seq("g")).match_type(), MatchType::ExactMatch);
        assert_eq!(t.matches(&seq("gg")).match_type(), MatchType::ExactMatch);
    }

    #[test]
    fn insert_and_remove() {
        let mut t = trie(&[("gg", "scroll-to-perc 0")]);
        assert_eq!(t.len(), 1);
        let old = t.insert(&seq("gg"), Command::TabNext);
        assert_eq!(old, Some(crate::commands::parse("scroll-to-perc 0").unwrap()));
        assert_eq!(t.len(), 1);

        assert_eq!(t.remove(&seq("gg")), Some(Command::TabNext));
        assert_eq!(t.len(), 0);
        // The `g` branch was pruned, so `g` is no longer a partial match.
        assert_eq!(t.matches(&seq("g")).match_type(), MatchType::NoMatch);
        assert_eq!(t.remove(&seq("gg")), None);
    }

    // -- the parser ----------------------------------------------------------------------------

    #[test]
    fn a_chain_stays_pending_and_clears_after_a_no_match() {
        let mut p = normal(&[("gg", "scroll-to-perc 0"), ("j", "scroll down")]);

        let out = p.handle(key("g"));
        assert_eq!(out.action, KeyAction::Pending);
        assert_eq!(out.keystring, "g", "the pending g must show in the status bar");
        assert!(out.swallow);

        // `gx` is nothing: the chain is abandoned and the keystring goes empty.
        let out = p.handle(key("x"));
        assert_eq!(out.action, KeyAction::NoMatch);
        assert_eq!(out.keystring, "");
        assert_eq!(p.keystring(), "", "the pending string must clear after a NoMatch");

        // And the parser is usable again straight away.
        assert_eq!(p.handle(key("g")).action, KeyAction::Pending);
        let out = p.handle(key("g"));
        assert_eq!(
            out.action,
            KeyAction::Run { command: crate::commands::parse("scroll-to-perc 0").unwrap(), count: None }
        );
        assert_eq!(p.keystring(), "");
    }

    #[test]
    fn counts_accumulate() {
        let mut p = normal(&[("j", "scroll down")]);

        // 3j -> scroll down, count 3.
        let out = p.handle(key("3"));
        assert_eq!(out.action, KeyAction::Pending);
        assert_eq!(out.keystring, "3");
        assert!(out.swallow, "a count digit must not reach the page");
        let out = p.handle(key("j"));
        assert_eq!(
            out.action,
            KeyAction::Run { command: Command::Scroll(ScrollDirection::Down), count: Some(3) }
        );

        // 10j -> count 10, not 1 then 0.
        p.handle(key("1"));
        let out = p.handle(key("0"));
        assert_eq!(out.keystring, "10");
        let out = p.handle(key("j"));
        assert_eq!(
            out.action,
            KeyAction::Run { command: Command::Scroll(ScrollDirection::Down), count: Some(10) }
        );

        // A bare j has no count at all.
        let out = p.handle(key("j"));
        assert_eq!(
            out.action,
            KeyAction::Run { command: Command::Scroll(ScrollDirection::Down), count: None }
        );
    }

    #[test]
    fn a_leading_zero_is_not_a_count() {
        // basekeyparser._match_count: `not (not self._count and txt == '0')`. `g0` is a real
        // binding (tab-focus 1); a count of "0…" is not.
        let mut p = normal(&[("j", "scroll down"), ("g0", "tab-focus 1")]);

        let out = p.handle(key("0"));
        assert_eq!(out.action, KeyAction::NoMatch, "a leading 0 must not start a count");
        assert_eq!(p.keystring(), "");

        // It really is only the *leading* zero: after a 1 it counts.
        p.handle(key("1"));
        assert_eq!(p.handle(key("0")).keystring, "10");
        p.clear();

        // And `g0` still resolves as a binding rather than as a count.
        assert_eq!(p.handle(key("g")).action, KeyAction::Pending);
        assert_eq!(
            p.handle(key("0")).action,
            KeyAction::Run { command: crate::commands::parse("tab-focus 1").unwrap(), count: None }
        );
    }

    #[test]
    fn modes_without_count_support_treat_digits_as_keys() {
        let mut p = KeyParser::new(Mode::Insert, trie(&[("<Escape>", "mode-leave")]));
        let out = p.handle(key("3"));
        assert_eq!(out.action, KeyAction::NoMatch);
        assert!(!out.swallow, "insert mode must let the digit through to the page");
        assert_eq!(p.keystring(), "");
    }

    #[test]
    fn unmatched_keys_reach_the_page_only_where_they_should() {
        // Normal mode with forward_unbound_keys=auto: alphanumerics are eaten, the rest passes.
        let mut p = normal(&[("j", "scroll down")]);
        assert!(p.handle(key("z")).swallow);
        assert!(!p.handle(key("<Ctrl-z>")).swallow);
        assert!(!p.handle(key("<Up>")).swallow);
    }

    #[test]
    fn passthrough_binds_only_shift_escape() {
        // configdata.yml `passthrough:` has exactly one entry. Everything else must reach the page
        // — that is what the mode is for.
        let mut p = KeyParser::new(Mode::Passthrough, trie(&[("<Shift-Escape>", "mode-leave")]));
        assert_eq!(p.bindings().len(), 1);

        let out = p.handle(key("<Shift-Escape>"));
        assert_eq!(out.action, KeyAction::Run { command: Command::ModeLeave, count: None });
        assert!(out.swallow);

        for k in ["j", "J", "3", "<Escape>", "<Ctrl-v>", "<Return>", "<Up>", " "] {
            let out = p.handle(key(k));
            assert_eq!(out.action, KeyAction::NoMatch, "{k:?} must not match in passthrough");
            assert!(!out.swallow, "passthrough must forward {k:?} to the page");
        }
    }

    #[test]
    fn keypad_digits_are_stripped_and_then_count() {
        // `_match_without_modifiers` exists so that a numpad 3 counts like a top-row 3.
        let numpad_3 = KeyInfo::from_cef(0x63, 512, 0).unwrap();
        assert!(numpad_3.mods.contains(Modifiers::KEYPAD));
        assert_ne!(numpad_3, key("3"));

        let mut p = normal(&[("j", "scroll down")]);
        assert_eq!(p.handle(numpad_3).keystring, "3");
        assert_eq!(
            p.handle(key("j")).action,
            KeyAction::Run { command: Command::Scroll(ScrollDirection::Down), count: Some(3) }
        );
    }

    #[test]
    fn key_parsers_route_by_mode() {
        let mut tries = HashMap::new();
        tries.insert(Mode::Normal, trie(&[("j", "scroll down")]));
        tries.insert(Mode::Insert, trie(&[("<Escape>", "mode-leave")]));
        let mut parsers = KeyParsers::new(tries);

        assert!(matches!(
            parsers.handle(Mode::Normal, key("j")).action,
            KeyAction::Run { .. }
        ));
        // The same key in insert mode is just a `j` for the page.
        let out = parsers.handle(Mode::Insert, key("j"));
        assert_eq!(out.action, KeyAction::NoMatch);
        assert!(!out.swallow);
        // Every mode has a parser even when nothing was bound for it.
        assert!(parsers.parser(Mode::Command).bindings().is_empty());
    }

    #[test]
    fn leaving_a_mode_clears_its_pending_chain() {
        let mut tries = HashMap::new();
        tries.insert(Mode::Normal, trie(&[("gg", "scroll-to-perc 0")]));
        let mut parsers = KeyParsers::new(tries);
        parsers.handle(Mode::Normal, key("g"));
        assert_eq!(parsers.parser(Mode::Normal).keystring(), "g");
        parsers.clear(Mode::Normal);
        assert_eq!(parsers.parser(Mode::Normal).keystring(), "");
    }
}

#[cfg(test)]
mod merge_probe {
    use super::*;

    /// `<Ctrl-T>` must resolve from what CEF actually delivers. It is bound to `open -t`, and if it
    /// does not match, normal mode does not swallow it (a Ctrl chord is `is_non_alnum`), Chromium
    /// sees it, and opens `chrome://newtab/` inside bru — measured 2026-08-06 in a real session.
    #[test]
    fn ctrl_t_from_cef_matches_its_binding() {
        const VKEY_T: i32 = 0x54;
        const CTRL: u32 = 1 << 2; // EVENTFLAG_CONTROL_DOWN

        let from_event = KeyInfo::from_cef(VKEY_T, CTRL, 0).expect("Ctrl-T is not a bare modifier");
        let from_config = parse_key_sequence("<Ctrl-T>").expect("the default bindings parse");

        assert_eq!(
            vec![from_event], from_config,
            "what CEF delivers for Ctrl-T must equal what <Ctrl-T> parses to"
        );
    }

    /// Same question for a plain letter, which is known to work — so a failure above is about the
    /// modifier path and not about letters in general.
    #[test]
    fn plain_j_from_cef_matches_its_binding() {
        const VKEY_J: i32 = 0x4A;
        let from_event = KeyInfo::from_cef(VKEY_J, 0, b'j' as u16).expect("j is not a modifier");
        let from_config = parse_key_sequence("j").expect("j parses");
        assert_eq!(vec![from_event], from_config);
    }
}
