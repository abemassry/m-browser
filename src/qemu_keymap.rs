//! Translation from winit physical key codes to Linux evdev key codes.
//!
//! The QEMU wasm module receives key events as Linux evdev codes and converts
//! them with `qemu_input_linux_to_qcode()`. evdev codes are a stable, public
//! ABI, so using them keeps this table independent of QEMU's internal
//! `QKeyCode` enum numbering.
//!
//! Values come from `linux/input-event-codes.h`.

use winit::keyboard::KeyCode;

/// Map a winit physical key to its Linux evdev code.
///
/// Returns `None` for keys with no evdev equivalent; callers should drop
/// those rather than sending a bogus code to the guest.
pub fn keycode_to_evdev(code: KeyCode) -> Option<i32> {
    let v = match code {
        // ---- Row 1: escape and function keys ----
        KeyCode::Escape => 1,
        KeyCode::F1 => 59,
        KeyCode::F2 => 60,
        KeyCode::F3 => 61,
        KeyCode::F4 => 62,
        KeyCode::F5 => 63,
        KeyCode::F6 => 64,
        KeyCode::F7 => 65,
        KeyCode::F8 => 66,
        KeyCode::F9 => 67,
        KeyCode::F10 => 68,
        KeyCode::F11 => 87,
        KeyCode::F12 => 88,
        KeyCode::F13 => 183,
        KeyCode::F14 => 184,
        KeyCode::F15 => 185,
        KeyCode::F16 => 186,
        KeyCode::F17 => 187,
        KeyCode::F18 => 188,
        KeyCode::F19 => 189,
        KeyCode::F20 => 190,
        KeyCode::F21 => 191,
        KeyCode::F22 => 192,
        KeyCode::F23 => 193,
        KeyCode::F24 => 194,

        // ---- Number row ----
        KeyCode::Backquote => 41,
        KeyCode::Digit1 => 2,
        KeyCode::Digit2 => 3,
        KeyCode::Digit3 => 4,
        KeyCode::Digit4 => 5,
        KeyCode::Digit5 => 6,
        KeyCode::Digit6 => 7,
        KeyCode::Digit7 => 8,
        KeyCode::Digit8 => 9,
        KeyCode::Digit9 => 10,
        KeyCode::Digit0 => 11,
        KeyCode::Minus => 12,
        KeyCode::Equal => 13,
        KeyCode::Backspace => 14,

        // ---- Letters and punctuation ----
        KeyCode::Tab => 15,
        KeyCode::KeyQ => 16,
        KeyCode::KeyW => 17,
        KeyCode::KeyE => 18,
        KeyCode::KeyR => 19,
        KeyCode::KeyT => 20,
        KeyCode::KeyY => 21,
        KeyCode::KeyU => 22,
        KeyCode::KeyI => 23,
        KeyCode::KeyO => 24,
        KeyCode::KeyP => 25,
        KeyCode::BracketLeft => 26,
        KeyCode::BracketRight => 27,
        KeyCode::Enter => 28,
        KeyCode::ControlLeft => 29,
        KeyCode::KeyA => 30,
        KeyCode::KeyS => 31,
        KeyCode::KeyD => 32,
        KeyCode::KeyF => 33,
        KeyCode::KeyG => 34,
        KeyCode::KeyH => 35,
        KeyCode::KeyJ => 36,
        KeyCode::KeyK => 37,
        KeyCode::KeyL => 38,
        KeyCode::Semicolon => 39,
        KeyCode::Quote => 40,
        KeyCode::ShiftLeft => 42,
        KeyCode::Backslash => 43,
        KeyCode::KeyZ => 44,
        KeyCode::KeyX => 45,
        KeyCode::KeyC => 46,
        KeyCode::KeyV => 47,
        KeyCode::KeyB => 48,
        KeyCode::KeyN => 49,
        KeyCode::KeyM => 50,
        KeyCode::Comma => 51,
        KeyCode::Period => 52,
        KeyCode::Slash => 53,
        KeyCode::ShiftRight => 54,
        KeyCode::AltLeft => 56,
        KeyCode::Space => 57,
        KeyCode::CapsLock => 58,

        // ---- Modifiers and menu ----
        KeyCode::ControlRight => 97,
        KeyCode::AltRight => 100,
        KeyCode::SuperLeft => 125,
        KeyCode::SuperRight => 126,
        KeyCode::ContextMenu => 127,

        // ---- Navigation cluster ----
        KeyCode::PrintScreen => 99,
        KeyCode::ScrollLock => 70,
        KeyCode::Pause => 119,
        KeyCode::Insert => 110,
        KeyCode::Home => 102,
        KeyCode::PageUp => 104,
        KeyCode::Delete => 111,
        KeyCode::End => 107,
        KeyCode::PageDown => 109,
        KeyCode::ArrowUp => 103,
        KeyCode::ArrowLeft => 105,
        KeyCode::ArrowDown => 108,
        KeyCode::ArrowRight => 106,

        // ---- Numeric keypad ----
        KeyCode::NumLock => 69,
        KeyCode::NumpadDivide => 98,
        KeyCode::NumpadMultiply => 55,
        KeyCode::NumpadSubtract => 74,
        KeyCode::NumpadAdd => 78,
        KeyCode::NumpadEnter => 96,
        KeyCode::Numpad1 => 79,
        KeyCode::Numpad2 => 80,
        KeyCode::Numpad3 => 81,
        KeyCode::Numpad4 => 75,
        KeyCode::Numpad5 => 76,
        KeyCode::Numpad6 => 77,
        KeyCode::Numpad7 => 71,
        KeyCode::Numpad8 => 72,
        KeyCode::Numpad9 => 73,
        KeyCode::Numpad0 => 82,
        KeyCode::NumpadDecimal => 83,
        KeyCode::NumpadEqual => 117,
        KeyCode::NumpadComma => 121,

        // ---- International / IME ----
        KeyCode::IntlBackslash => 86,
        KeyCode::IntlRo => 89,
        KeyCode::IntlYen => 124,
        KeyCode::Convert => 92,
        KeyCode::NonConvert => 94,
        KeyCode::KanaMode => 93,
        KeyCode::Lang1 => 122,
        KeyCode::Lang2 => 123,

        // ---- Media keys the PC keyboard model understands ----
        KeyCode::AudioVolumeMute => 113,
        KeyCode::AudioVolumeDown => 114,
        KeyCode::AudioVolumeUp => 115,
        KeyCode::Power => 116,

        _ => return None,
    };
    Some(v)
}

/// Linux evdev code for a character on a US keyboard, and whether it needs
/// Shift. Used so typed text also reaches the guest's own keyboard.
pub fn char_to_evdev(ch: char) -> Option<(i32, bool)> {
    const LETTERS: &[u8; 26] = &[
        30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17,
        45, 21, 44,
    ];
    const DIGITS: &[u8; 10] = &[11, 2, 3, 4, 5, 6, 7, 8, 9, 10];
    const SHIFTED_DIGITS: &str = ")!@#$%^&*(";
    let key = match ch {
        'a'..='z' => (LETTERS[(ch as u8 - b'a') as usize] as i32, false),
        'A'..='Z' => (LETTERS[(ch as u8 - b'A') as usize] as i32, true),
        '0'..='9' => (DIGITS[(ch as u8 - b'0') as usize] as i32, false),
        '\r' | '\n' => (KEY_ENTER, false),
        '\t' => (15, false),
        '\u{1b}' => (1, false),
        '\u{7f}' | '\u{8}' => (14, false),
        ' ' => (57, false),
        '-' => (12, false),
        '_' => (12, true),
        '=' => (13, false),
        '+' => (13, true),
        '[' => (26, false),
        '{' => (26, true),
        ']' => (27, false),
        '}' => (27, true),
        ';' => (39, false),
        ':' => (39, true),
        '\'' => (40, false),
        '"' => (40, true),
        '`' => (41, false),
        '~' => (41, true),
        '\\' => (43, false),
        '|' => (43, true),
        ',' => (51, false),
        '<' => (51, true),
        '.' => (52, false),
        '>' => (52, true),
        '/' => (53, false),
        '?' => (53, true),
        _ => {
            let i = SHIFTED_DIGITS.find(ch)?;
            (DIGITS[i] as i32, true)
        }
    };
    Some(key)
}

/// evdev codes used by `text_events` and for shortcuts.
pub const KEY_LEFTCTRL: i32 = 29;
pub const KEY_LEFTSHIFT: i32 = 42;
pub const KEY_ENTER: i32 = 28;
pub const KEY_L: i32 = 38;

/// Events typing `text`: for each character a key press and release (with
/// Shift where needed) for the guest's keyboard, plus a text event (type 5)
/// for QEMU's text consoles; each kind of console ignores the other. `\n`
/// and `\r` are Enter. Characters a US keyboard does not have are sent as
/// text only. QEMU's display delivers queued key events at its own pace (a
/// few per display refresh), so the whole text can be queued at once.
pub fn text_events(text: &str) -> Vec<crate::qemu_gfx::InputEvent> {
    use crate::qemu_gfx::InputEvent;
    const KEY: i32 = 3;
    const TEXT: i32 = 5;
    let key = |code: i32, down: bool| InputEvent { event_type: KEY, code, value: down as i32 };
    let mut events = Vec::new();
    for ch in text.chars() {
        let ch = if ch == '\n' { '\r' } else { ch };
        if let Some((code, shift)) = char_to_evdev(ch) {
            if shift {
                events.push(key(KEY_LEFTSHIFT, true));
            }
            events.push(key(code, true));
            events.push(key(code, false));
            if shift {
                events.push(key(KEY_LEFTSHIFT, false));
            }
        }
        events.push(InputEvent { event_type: TEXT, code: ch as i32, value: 0 });
    }
    events
}

/// Events pressing `key` while `modifier` is held, such as Ctrl+L.
pub fn chord_events(modifier: i32, key: i32) -> Vec<crate::qemu_gfx::InputEvent> {
    use crate::qemu_gfx::InputEvent;
    let event = |code: i32, down: bool| InputEvent { event_type: 3, code, value: down as i32 };
    vec![event(modifier, true), event(key, true), event(key, false), event(modifier, false)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_becomes_key_presses() {
        let events: Vec<(i32, i32, i32)> =
            text_events("aB\n").iter().map(|e| (e.event_type, e.code, e.value)).collect();
        assert_eq!(
            events,
            [
                (3, 30, 1), (3, 30, 0), (5, 'a' as i32, 0),
                (3, KEY_LEFTSHIFT, 1), (3, 48, 1), (3, 48, 0), (3, KEY_LEFTSHIFT, 0), (5, 'B' as i32, 0),
                (3, KEY_ENTER, 1), (3, KEY_ENTER, 0), (5, '\r' as i32, 0),
            ]
        );
        let ctrl_l: Vec<(i32, i32)> = chord_events(KEY_LEFTCTRL, KEY_L).iter().map(|e| (e.code, e.value)).collect();
        assert_eq!(ctrl_l, [(29, 1), (38, 1), (38, 0), (29, 0)]);
        assert_eq!(char_to_evdev('/'), Some((53, false)));
        assert_eq!(char_to_evdev(':'), Some((39, true)));
        assert_eq!(char_to_evdev('\u{e9}'), None);
    }
}
