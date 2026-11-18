//! Win32 virtual-key (`VK_*`) and `WM_CHAR` / UTF-16 translation into canonical [`UiEvent::Key`].

use lava_proto::{UiEvent, mods};

use crate::win_ffi;

pub(crate) const VK_BACK: u32 = 0x08;
pub(crate) const VK_TAB: u32 = 0x09;
pub(crate) const VK_RETURN: u32 = 0x0D;
pub(crate) const VK_SHIFT: u32 = 0x10;
pub(crate) const VK_CONTROL: u32 = 0x11;
pub(crate) const VK_MENU: u32 = 0x12; // Alt
pub(crate) const VK_PAUSE: u32 = 0x13;
pub(crate) const VK_CAPITAL: u32 = 0x14;
pub(crate) const VK_ESCAPE: u32 = 0x1B;
pub(crate) const VK_SPACE: u32 = 0x20;
pub(crate) const VK_PRIOR: u32 = 0x21; // Page Up
pub(crate) const VK_NEXT: u32 = 0x22; // Page Down
pub(crate) const VK_END: u32 = 0x23;
pub(crate) const VK_HOME: u32 = 0x24;
pub(crate) const VK_LEFT: u32 = 0x25;
pub(crate) const VK_UP: u32 = 0x26;
pub(crate) const VK_RIGHT: u32 = 0x27;
pub(crate) const VK_DOWN: u32 = 0x28;
pub(crate) const VK_INSERT: u32 = 0x2D;
pub(crate) const VK_DELETE: u32 = 0x2E;
pub(crate) const VK_LWIN: u32 = 0x5B;
pub(crate) const VK_RWIN: u32 = 0x5C;
pub(crate) const VK_F1: u32 = 0x70;
pub(crate) const VK_F12: u32 = 0x7B;
pub(crate) const VK_NUMLOCK: u32 = 0x90;
pub(crate) const VK_SCROLL: u32 = 0x91;
pub(crate) const VK_LSHIFT: u32 = 0xA0;
pub(crate) const VK_RSHIFT: u32 = 0xA1;
pub(crate) const VK_LCONTROL: u32 = 0xA2;
pub(crate) const VK_RCONTROL: u32 = 0xA3;
pub(crate) const VK_LMENU: u32 = 0xA4;
pub(crate) const VK_RMENU: u32 = 0xA5;

// OEM punctuation virtual keys on standard US/ANSI layouts for Ctrl/Alt chords
const VK_OEM_1: u32 = 0xBA; // `;:`
const VK_OEM_PLUS: u32 = 0xBB; // `=+`
const VK_OEM_COMMA: u32 = 0xBC; // `,<`
const VK_OEM_MINUS: u32 = 0xBD; // `-_`
const VK_OEM_PERIOD: u32 = 0xBE; // `.>`
const VK_OEM_2: u32 = 0xBF; // `/?`
const VK_OEM_3: u32 = 0xC0; // `` `~ ``
const VK_OEM_4: u32 = 0xDB; // `[{`
const VK_OEM_5: u32 = 0xDC; // `\|`
const VK_OEM_6: u32 = 0xDD; // `]}`
const VK_OEM_7: u32 = 0xDE; // `'"`

pub(crate) fn query_win32_modifiers() -> u32 {
    // SAFETY: `GetKeyState` is a pure thread-queue query safe to call at any time on Win32.
    unsafe {
        let mut m = 0u32;
        let shift = (win_ffi::GetKeyState(VK_SHIFT as i32) as u16 & 0x8000) != 0;
        let ctrl = (win_ffi::GetKeyState(VK_CONTROL as i32) as u16 & 0x8000) != 0;
        let alt = (win_ffi::GetKeyState(VK_MENU as i32) as u16 & 0x8000) != 0;
        let lwin = (win_ffi::GetKeyState(VK_LWIN as i32) as u16 & 0x8000) != 0;
        let rwin = (win_ffi::GetKeyState(VK_RWIN as i32) as u16 & 0x8000) != 0;

        // Detect Windows AltGr (Right Alt synthesized as Left Ctrl + Right Alt) so European
        // layouts typing `{` / `[` / `@` via AltGr aren't misinterpreted as Ctrl+Alt chords.
        let rmenu = (win_ffi::GetKeyState(VK_RMENU as i32) as u16 & 0x8000) != 0;
        let rctrl = (win_ffi::GetKeyState(VK_RCONTROL as i32) as u16 & 0x8000) != 0;
        let is_altgr = ctrl && alt && rmenu && !rctrl;

        if shift {
            m |= mods::SHIFT;
        }
        if !is_altgr {
            if ctrl {
                m |= mods::CTRL;
            }
            if alt {
                m |= mods::ALT;
            }
        }
        if lwin || rwin {
            m |= mods::SUPER;
        }
        m
    }
}

pub(crate) fn is_pure_modifier_vk(vk: u32) -> bool {
    matches!(
        vk,
        VK_SHIFT
            | VK_CONTROL
            | VK_MENU
            | VK_LSHIFT
            | VK_RSHIFT
            | VK_LCONTROL
            | VK_RCONTROL
            | VK_LMENU
            | VK_RMENU
            | VK_LWIN
            | VK_RWIN
            | VK_CAPITAL
            | VK_NUMLOCK
            | VK_SCROLL
            | VK_PAUSE
    )
}

pub(crate) fn special_vk_name(vk: u32) -> Option<&'static str> {
    match vk {
        VK_RETURN => Some("Return"),
        VK_ESCAPE => Some("Escape"),
        VK_BACK => Some("BackSpace"),
        VK_TAB => Some("Tab"),
        VK_DELETE => Some("Delete"),
        VK_INSERT => Some("Insert"),
        VK_HOME => Some("Home"),
        VK_END => Some("End"),
        VK_PRIOR => Some("Page_Up"),
        VK_NEXT => Some("Page_Down"),
        VK_UP => Some("Up"),
        VK_DOWN => Some("Down"),
        VK_LEFT => Some("Left"),
        VK_RIGHT => Some("Right"),
        VK_F1 => Some("F1"),
        0x71 => Some("F2"),
        0x72 => Some("F3"),
        0x73 => Some("F4"),
        0x74 => Some("F5"),
        0x75 => Some("F6"),
        0x76 => Some("F7"),
        0x77 => Some("F8"),
        0x78 => Some("F9"),
        0x79 => Some("F10"),
        0x7A => Some("F11"),
        VK_F12 => Some("F12"),
        _ => None,
    }
}

fn vk_chord_char(vk: u32, shift: bool) -> Option<char> {
    match vk {
        0x41..=0x5A => Some((vk as u8 as char).to_ascii_lowercase()),
        0x30 => Some(if shift { ')' } else { '0' }),
        0x31 => Some(if shift { '!' } else { '1' }),
        0x32 => Some(if shift { '@' } else { '2' }),
        0x33 => Some(if shift { '#' } else { '3' }),
        0x34 => Some(if shift { '$' } else { '4' }),
        0x35 => Some(if shift { '%' } else { '5' }),
        0x36 => Some(if shift { '^' } else { '6' }),
        0x37 => Some(if shift { '&' } else { '7' }),
        0x38 => Some(if shift { '*' } else { '8' }),
        0x39 => Some(if shift { '(' } else { '9' }),
        VK_OEM_1 => Some(if shift { ':' } else { ';' }),
        VK_OEM_PLUS => Some(if shift { '+' } else { '=' }),
        VK_OEM_COMMA => Some(if shift { '<' } else { ',' }),
        VK_OEM_MINUS => Some(if shift { '_' } else { '-' }),
        VK_OEM_PERIOD => Some(if shift { '>' } else { '.' }),
        VK_OEM_2 => Some(if shift { '?' } else { '/' }),
        VK_OEM_3 => Some(if shift { '~' } else { '`' }),
        VK_OEM_4 => Some(if shift { '{' } else { '[' }),
        VK_OEM_5 => Some(if shift { '|' } else { '\\' }),
        VK_OEM_6 => Some(if shift { '}' } else { ']' }),
        VK_OEM_7 => Some(if shift { '"' } else { '\'' }),
        _ => None,
    }
}

/// Translates a `WM_KEYDOWN` / `WM_SYSKEYDOWN` virtual key when it is a special key, `Space`, or a
/// `Ctrl`/`Alt`/`Super` chord. Printable unmodified / `Shift`-only characters return `None` here so
/// `TranslateMessage` -> `WM_CHAR` delivers the exact layout-translated Unicode character (including
/// IME CJK input and dead keys) without duplicate events!
pub(crate) fn translate_win32_keydown(vk: u32, modifiers: u32) -> Option<UiEvent> {
    if is_pure_modifier_vk(vk) {
        return None;
    }

    let has_chord_mod = (modifiers & (mods::CTRL | mods::ALT | mods::SUPER)) != 0;

    if vk == VK_SPACE {
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

    if let Some(special) = special_vk_name(vk) {
        return Some(UiEvent::Key {
            key: special.to_string(),
            modifiers,
            text: None,
        });
    }

    if has_chord_mod {
        let shift = (modifiers & mods::SHIFT) != 0;
        if let Some(ch) = vk_chord_char(vk, shift) {
            // Strip SHIFT when punctuation already shifted (e.g. `Ctrl+?` -> `key: "?"` with `CTRL`),
            // while keeping `SHIFT` on alphabetic chords (`Ctrl+Shift+P` -> `key: "p"`, `CTRL | SHIFT`).
            let clean_mods = if !ch.is_ascii_alphabetic() && shift && vk != (ch as u32) {
                modifiers & !mods::SHIFT
            } else {
                modifiers
            };
            return Some(UiEvent::Key {
                key: ch.to_string(),
                modifiers: clean_mods,
                text: None,
            });
        }
    }

    None
}

/// Translates a `WM_CHAR` / decoded Unicode scalar value into [`UiEvent::Key`].
/// Ignores control characters (`< 0x20`, `0x7F`) and Space (`0x20`), which are already handled in
/// `translate_win32_keydown`.
pub(crate) fn translate_win32_char(ch: char, modifiers: u32) -> Option<UiEvent> {
    if ch.is_control() || ch == ' ' {
        return None;
    }
    let has_chord_mod = (modifiers & (mods::CTRL | mods::ALT | mods::SUPER)) != 0;
    if has_chord_mod {
        return None;
    }
    let clean_mods = modifiers & !mods::SHIFT;
    let s = ch.to_string();
    Some(UiEvent::Key {
        key: s.clone(),
        modifiers: clean_mods,
        text: Some(s),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn special_keys_and_shift_tab_map_to_canonical_names() {
        assert_eq!(
            translate_win32_keydown(VK_RETURN, 0),
            Some(UiEvent::Key {
                key: "Return".to_string(),
                modifiers: 0,
                text: None,
            })
        );
        assert_eq!(
            translate_win32_keydown(VK_ESCAPE, 0),
            Some(UiEvent::Key {
                key: "Escape".to_string(),
                modifiers: 0,
                text: None,
            })
        );
        assert_eq!(
            translate_win32_keydown(VK_TAB, mods::SHIFT),
            Some(UiEvent::Key {
                key: "Tab".to_string(),
                modifiers: mods::SHIFT,
                text: None,
            })
        );
        assert_eq!(
            translate_win32_keydown(VK_SPACE, 0),
            Some(UiEvent::Key {
                key: "Space".to_string(),
                modifiers: 0,
                text: Some(" ".to_string()),
            })
        );
    }

    #[test]
    fn ctrl_and_alt_chords_and_cjk_chars_normalize_cleanly() {
        assert_eq!(
            translate_win32_keydown(b'N' as u32, mods::ALT),
            Some(UiEvent::Key {
                key: "n".to_string(),
                modifiers: mods::ALT,
                text: None,
            })
        );
        assert_eq!(
            translate_win32_keydown(b'P' as u32, mods::CTRL | mods::SHIFT),
            Some(UiEvent::Key {
                key: "p".to_string(),
                modifiers: mods::CTRL | mods::SHIFT,
                text: None,
            })
        );
        assert_eq!(
            translate_win32_char('你', 0),
            Some(UiEvent::Key {
                key: "你".to_string(),
                modifiers: 0,
                text: Some("你".to_string()),
            })
        );
        assert_eq!(translate_win32_keydown(VK_LSHIFT, mods::SHIFT), None);
    }
}
