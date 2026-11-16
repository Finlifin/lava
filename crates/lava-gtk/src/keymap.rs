//! GDK4 keyval and modifier bitmask translation into canonical [`UiEvent::Key`] events.

use std::ffi::CStr;

use lava_proto::{UiEvent, mods};
use libc::c_uint;

use crate::ffi::{
    self, GDK_ALT_MASK, GDK_CONTROL_MASK, GDK_META_MASK, GDK_SHIFT_MASK, GDK_SUPER_MASK,
};

const GDK_KEY_ISO_LEVEL3_SHIFT: u32 = 0xfe03;
const GDK_KEY_ISO_LEFT_TAB: u32 = 0xfe20;

const GDK_KEY_BACKSPACE: u32 = 0xff08;
const GDK_KEY_TAB: u32 = 0xff09;
const GDK_KEY_RETURN: u32 = 0xff0d;
const GDK_KEY_ESCAPE: u32 = 0xff1b;

const GDK_KEY_HOME: u32 = 0xff50;
const GDK_KEY_LEFT: u32 = 0xff51;
const GDK_KEY_UP: u32 = 0xff52;
const GDK_KEY_RIGHT: u32 = 0xff53;
const GDK_KEY_DOWN: u32 = 0xff54;
const GDK_KEY_PAGE_UP: u32 = 0xff55;
const GDK_KEY_PAGE_DOWN: u32 = 0xff56;
const GDK_KEY_END: u32 = 0xff57;
const GDK_KEY_INSERT: u32 = 0xff63;
const GDK_KEY_NUM_LOCK: u32 = 0xff7f;

const GDK_KEY_KP_SPACE: u32 = 0xff80;
const GDK_KEY_KP_TAB: u32 = 0xff89;
const GDK_KEY_KP_ENTER: u32 = 0xff8d;
const GDK_KEY_KP_HOME: u32 = 0xff95;
const GDK_KEY_KP_LEFT: u32 = 0xff96;
const GDK_KEY_KP_UP: u32 = 0xff97;
const GDK_KEY_KP_RIGHT: u32 = 0xff98;
const GDK_KEY_KP_DOWN: u32 = 0xff99;
const GDK_KEY_KP_PAGE_UP: u32 = 0xff9a;
const GDK_KEY_KP_PAGE_DOWN: u32 = 0xff9b;
const GDK_KEY_KP_END: u32 = 0xff9c;
const GDK_KEY_KP_INSERT: u32 = 0xff9e;
const GDK_KEY_KP_DELETE: u32 = 0xff9f;

const GDK_KEY_F1: u32 = 0xffbe;
const GDK_KEY_F12: u32 = 0xffc9;

const GDK_KEY_SHIFT_L: u32 = 0xffe1;
const GDK_KEY_SHIFT_R: u32 = 0xffe2;
const GDK_KEY_CONTROL_L: u32 = 0xffe3;
const GDK_KEY_CONTROL_R: u32 = 0xffe4;
const GDK_KEY_CAPS_LOCK: u32 = 0xffe5;
const GDK_KEY_SHIFT_LOCK: u32 = 0xffe6;
const GDK_KEY_META_L: u32 = 0xffe7;
const GDK_KEY_META_R: u32 = 0xffe8;
const GDK_KEY_ALT_L: u32 = 0xffe9;
const GDK_KEY_ALT_R: u32 = 0xffea;
const GDK_KEY_SUPER_L: u32 = 0xffeb;
const GDK_KEY_SUPER_R: u32 = 0xffec;
const GDK_KEY_HYPER_L: u32 = 0xffed;
const GDK_KEY_HYPER_R: u32 = 0xffee;

const GDK_KEY_DELETE: u32 = 0xffff;
const GDK_KEY_SPACE: u32 = 0x0020;

/// Converts a GDK4 `GdkModifierType` bitmask into `lava_proto::mods` flags.
pub(crate) fn translate_modifiers(gdk_state: c_uint) -> u32 {
    let mut out = 0;
    if (gdk_state & GDK_SHIFT_MASK) != 0 {
        out |= mods::SHIFT;
    }
    if (gdk_state & GDK_CONTROL_MASK) != 0 {
        out |= mods::CTRL;
    }
    if (gdk_state & GDK_ALT_MASK) != 0 {
        out |= mods::ALT;
    }
    if (gdk_state & (GDK_SUPER_MASK | GDK_META_MASK)) != 0 {
        out |= mods::SUPER;
    }
    out
}

fn is_pure_modifier_keyval(keyval: u32) -> bool {
    matches!(
        keyval,
        GDK_KEY_SHIFT_L
            | GDK_KEY_SHIFT_R
            | GDK_KEY_CONTROL_L
            | GDK_KEY_CONTROL_R
            | GDK_KEY_CAPS_LOCK
            | GDK_KEY_SHIFT_LOCK
            | GDK_KEY_META_L
            | GDK_KEY_META_R
            | GDK_KEY_ALT_L
            | GDK_KEY_ALT_R
            | GDK_KEY_SUPER_L
            | GDK_KEY_SUPER_R
            | GDK_KEY_HYPER_L
            | GDK_KEY_HYPER_R
            | GDK_KEY_ISO_LEVEL3_SHIFT
            | GDK_KEY_NUM_LOCK
    )
}

fn special_key_name(keyval: u32) -> Option<&'static str> {
    match keyval {
        GDK_KEY_RETURN | GDK_KEY_KP_ENTER => Some("Return"),
        GDK_KEY_ESCAPE => Some("Escape"),
        GDK_KEY_BACKSPACE => Some("BackSpace"),
        GDK_KEY_TAB | GDK_KEY_KP_TAB => Some("Tab"),
        GDK_KEY_DELETE | GDK_KEY_KP_DELETE => Some("Delete"),
        GDK_KEY_INSERT | GDK_KEY_KP_INSERT => Some("Insert"),
        GDK_KEY_HOME | GDK_KEY_KP_HOME => Some("Home"),
        GDK_KEY_END | GDK_KEY_KP_END => Some("End"),
        GDK_KEY_PAGE_UP | GDK_KEY_KP_PAGE_UP => Some("Page_Up"),
        GDK_KEY_PAGE_DOWN | GDK_KEY_KP_PAGE_DOWN => Some("Page_Down"),
        GDK_KEY_UP | GDK_KEY_KP_UP => Some("Up"),
        GDK_KEY_DOWN | GDK_KEY_KP_DOWN => Some("Down"),
        GDK_KEY_LEFT | GDK_KEY_KP_LEFT => Some("Left"),
        GDK_KEY_RIGHT | GDK_KEY_KP_RIGHT => Some("Right"),
        GDK_KEY_F1 => Some("F1"),
        0xffbf => Some("F2"),
        0xffc0 => Some("F3"),
        0xffc1 => Some("F4"),
        0xffc2 => Some("F5"),
        0xffc3 => Some("F6"),
        0xffc4 => Some("F7"),
        0xffc5 => Some("F8"),
        0xffc6 => Some("F9"),
        0xffc7 => Some("F10"),
        0xffc8 => Some("F11"),
        GDK_KEY_F12 => Some("F12"),
        _ => None,
    }
}

/// Translates a GDK4 `(keyval, state)` pair into a canonical [`UiEvent::Key`], returning `None`
/// for pure modifier key presses.
pub(crate) fn translate_gdk_key(keyval: c_uint, gdk_state: c_uint) -> Option<UiEvent> {
    if is_pure_modifier_keyval(keyval) {
        return None;
    }

    let modifiers = translate_modifiers(gdk_state);
    let has_chord_mod = (modifiers & (mods::CTRL | mods::ALT | mods::SUPER)) != 0;

    if keyval == GDK_KEY_ISO_LEFT_TAB {
        return Some(UiEvent::Key {
            key: "Tab".to_string(),
            modifiers: modifiers | mods::SHIFT,
            text: None,
        });
    }

    if keyval == GDK_KEY_SPACE || keyval == GDK_KEY_KP_SPACE {
        return Some(UiEvent::Key {
            key: "Space".to_string(),
            modifiers,
            text: if has_chord_mod {
                None
            } else {
                Some(" ".to_string())
            },
        });
    }

    if let Some(special) = special_key_name(keyval) {
        return Some(UiEvent::Key {
            key: special.to_string(),
            modifiers,
            text: None,
        });
    }

    // SAFETY: `gdk_keyval_to_unicode` is a pure table lookup converting any `guint` keysym to a
    // UCS-4 scalar value (or `0` if the keysym has no Unicode character mapping).
    let codepoint = unsafe { ffi::gdk_keyval_to_unicode(keyval) };
    if codepoint != 0
        && let Some(ch) = char::from_u32(codepoint)
        && !ch.is_control()
    {
        if has_chord_mod {
            let normalized = if ch.is_ascii_alphabetic() {
                ch.to_ascii_lowercase()
            } else {
                ch
            };
            return Some(UiEvent::Key {
                key: normalized.to_string(),
                modifiers,
                text: None,
            });
        }

        let s = ch.to_string();
        return Some(UiEvent::Key {
            key: s.clone(),
            modifiers,
            text: Some(s),
        });
    }

    // SAFETY: `gdk_keyval_name` returns either null or a static NUL-terminated ASCII string owned
    // by GDK for the lifetime of the process.
    let raw_name = unsafe { ffi::gdk_keyval_name(keyval) };
    if !raw_name.is_null() {
        // SAFETY: `raw_name` was checked non-null and points to a static NUL-terminated C string.
        let name_cstr = unsafe { CStr::from_ptr(raw_name) };
        if let Ok(name) = name_cstr.to_str()
            && !name.is_empty()
        {
            return Some(UiEvent::Key {
                key: name.to_string(),
                modifiers,
                text: None,
            });
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use lava_proto::{UiEvent, mods};

    use super::*;

    #[test]
    fn ctrl_and_alt_ascii_chords_normalize_cleanly() {
        assert_eq!(
            translate_gdk_key(u32::from(b'x'), GDK_CONTROL_MASK),
            Some(UiEvent::Key {
                key: "x".to_string(),
                modifiers: mods::CTRL,
                text: None,
            })
        );
        assert_eq!(
            translate_gdk_key(u32::from(b'X'), GDK_CONTROL_MASK | GDK_SHIFT_MASK),
            Some(UiEvent::Key {
                key: "x".to_string(),
                modifiers: mods::CTRL | mods::SHIFT,
                text: None,
            })
        );
        assert_eq!(
            translate_gdk_key(u32::from(b'x'), GDK_ALT_MASK),
            Some(UiEvent::Key {
                key: "x".to_string(),
                modifiers: mods::ALT,
                text: None,
            })
        );
        assert_eq!(
            translate_gdk_key(u32::from(b's'), GDK_SUPER_MASK),
            Some(UiEvent::Key {
                key: "s".to_string(),
                modifiers: mods::SUPER,
                text: None,
            })
        );
    }

    #[test]
    fn special_keys_and_iso_left_tab_map_to_canonical_names() {
        assert_eq!(
            translate_gdk_key(GDK_KEY_ISO_LEFT_TAB, 0),
            Some(UiEvent::Key {
                key: "Tab".to_string(),
                modifiers: mods::SHIFT,
                text: None,
            })
        );
        assert_eq!(
            translate_gdk_key(GDK_KEY_ESCAPE, 0),
            Some(UiEvent::Key {
                key: "Escape".to_string(),
                modifiers: 0,
                text: None,
            })
        );
        assert_eq!(
            translate_gdk_key(GDK_KEY_RETURN, 0),
            Some(UiEvent::Key {
                key: "Return".to_string(),
                modifiers: 0,
                text: None,
            })
        );
        assert_eq!(
            translate_gdk_key(GDK_KEY_SPACE, 0),
            Some(UiEvent::Key {
                key: "Space".to_string(),
                modifiers: 0,
                text: Some(" ".to_string()),
            })
        );
        assert_eq!(
            translate_gdk_key(GDK_KEY_SPACE, GDK_CONTROL_MASK),
            Some(UiEvent::Key {
                key: "Space".to_string(),
                modifiers: mods::CTRL,
                text: None,
            })
        );
        assert_eq!(
            translate_gdk_key(GDK_KEY_F1, 0),
            Some(UiEvent::Key {
                key: "F1".to_string(),
                modifiers: 0,
                text: None,
            })
        );
        assert_eq!(
            translate_gdk_key(GDK_KEY_F12, 0),
            Some(UiEvent::Key {
                key: "F12".to_string(),
                modifiers: 0,
                text: None,
            })
        );
    }

    #[test]
    fn pure_modifier_keys_are_ignored() {
        for kv in [
            GDK_KEY_SHIFT_L,
            GDK_KEY_SHIFT_R,
            GDK_KEY_CONTROL_L,
            GDK_KEY_CONTROL_R,
            GDK_KEY_ALT_L,
            GDK_KEY_ALT_R,
            GDK_KEY_SUPER_L,
            GDK_KEY_SUPER_R,
            GDK_KEY_META_L,
            GDK_KEY_META_R,
        ] {
            assert_eq!(translate_gdk_key(kv, 0), None);
        }
    }

    #[test]
    fn unicode_printable_cjk_and_symbols_produce_text() {
        // GDK encodes direct Unicode keysyms >= U+0100 as `0x0100_0000 | codepoint`.
        let cjk_keyval = 0x0100_0000 | ('中' as u32);
        assert_eq!(
            translate_gdk_key(cjk_keyval, 0),
            Some(UiEvent::Key {
                key: "中".to_string(),
                modifiers: 0,
                text: Some("中".to_string()),
            })
        );

        let lambda_keyval = 0x0100_0000 | ('λ' as u32);
        assert_eq!(
            translate_gdk_key(lambda_keyval, 0),
            Some(UiEvent::Key {
                key: "λ".to_string(),
                modifiers: 0,
                text: Some("λ".to_string()),
            })
        );
    }
}
