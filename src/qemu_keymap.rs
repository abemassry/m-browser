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
