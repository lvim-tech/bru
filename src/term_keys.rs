//! C6: a kitty keyboard protocol event turned into the `KeyEvent` CEF expects, so that
//! `bindings.rs::KeyInfo::from_cef` reads terminal keys with no second table. Filled in by its own phase.
