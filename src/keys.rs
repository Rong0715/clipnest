//! Keyboard helpers: shortcut labels and layout-independent key characters.

use gtk::gdk;
use gtk::gdk::ModifierType;
use gtk::prelude::*;

/// Ctrl, Alt, Shift and Super, without lock or button state.
pub fn relevant(mods: ModifierType) -> ModifierType {
    mods & (ModifierType::CONTROL_MASK
        | ModifierType::ALT_MASK
        | ModifierType::SHIFT_MASK
        | ModifierType::SUPER_MASK)
}

pub fn mods_label(mods: ModifierType) -> String {
    let mut s = String::new();
    if mods.contains(ModifierType::SUPER_MASK) {
        s.push_str("Super+");
    }
    if mods.contains(ModifierType::CONTROL_MASK) {
        s.push_str("Ctrl+");
    }
    if mods.contains(ModifierType::ALT_MASK) {
        s.push_str("Alt+");
    }
    if mods.contains(ModifierType::SHIFT_MASK) {
        s.push_str("Shift+");
    }
    s
}

/// Label for an item's quick-copy key, e.g. "Ctrl+1" or "Ctrl+B".
pub fn char_label(c: &str) -> String {
    format!("{}{}", mods_label(ModifierType::CONTROL_MASK), c.to_uppercase())
}

/// Human-readable form of a GTK accelerator such as `<Shift><Super>c`.
pub fn accelerator_label(accel: &str) -> String {
    let mut mods = String::new();
    let mut rest = accel;
    while let Some(start) = rest.find('<') {
        let Some(end) = rest[start..].find('>') else { break };
        let m = &rest[start + 1..start + end];
        mods.push_str(match m.to_ascii_lowercase().as_str() {
            "control" | "ctrl" | "primary" => "Ctrl+",
            "alt" | "mod1" => "Alt+",
            "shift" => "Shift+",
            "super" | "meta" | "mod4" => "Super+",
            _ => "",
        });
        rest = &rest[start + end + 1..];
    }
    if rest.is_empty() {
        return "None".to_owned();
    }
    format!("{mods}{}", if rest.chars().count() == 1 { rest.to_uppercase() } else { rest.to_owned() })
}

/// Normalizes a key event to a lower-case, unshifted, preferably Latin character so
/// that e.g. Ctrl+1 works with Shift held and shortcuts work on non-Latin layouts.
pub fn event_char(display: &gdk::Display, key: gdk::Key, keycode: u32, layout: u32) -> Option<String> {
    let mut candidates: Vec<gdk::Key> = Vec::new();
    if let Some((k, ..)) = display.translate_key(keycode, ModifierType::empty(), layout as i32) {
        candidates.push(k);
    }
    candidates.push(key.to_lower());
    if let Some((k, ..)) = display.translate_key(keycode, ModifierType::empty(), 0) {
        candidates.push(k);
    }
    candidates
        .into_iter()
        .filter_map(|k| k.to_lower().to_unicode())
        .find(|c| c.is_ascii_graphic())
        .map(|c| c.to_ascii_lowercase().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        assert_eq!(accelerator_label("<Shift><Super>c"), "Shift+Super+C");
        assert_eq!(accelerator_label("<Control>space"), "Ctrl+space");
        assert_eq!(char_label("b"), "Ctrl+B");
    }
}
