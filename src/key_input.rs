//! `VirtualKey` and Set-1 scan codes translated into the W3C key vocabulary
//! [`waterui_core::key`] speaks.
//!
//! `KeyRoutedEventArgs` reports the layout-resolved [`VirtualKey`] plus a
//! `CorePhysicalKeyStatus` carrying the hardware scan code and the repeat
//! flag; the modifier chord is not on the event, so it is read straight from
//! `GetKeyboardState`.

use waterui_core::key::{Code, Key, Modifiers, NamedKey};

use crate::bindings::{self, VirtualKey};

/// The W3C `KeyboardEvent.key` for a [`VirtualKey`]/`scan_code` pair.
///
/// Printable keys resolve through `ToUnicodeEx` against the live keyboard
/// state and layout, so Shift+a arrives as "A" and a non-QWERTY layout's
/// letters arrive as the glyphs its key caps show; a negative return is a
/// dead key, surfaced as [`NamedKey::Dead`]. Keys with no printable mapping
/// answer from the named-key table, and anything left is
/// [`NamedKey::Unidentified`].
pub(crate) fn surface_key(key: VirtualKey, scan_code: u32) -> Key {
    if let Some(named) = named_key(key) {
        return Key::Named(named);
    }
    let state = keyboard_state();
    let mut utf16 = [0u16; 8];
    // SAFETY: `state` and `utf16` outlive the call; the 0x4 flag keeps
    // the probe from consuming the keyboard's pending dead-key state.
    let written = unsafe {
        bindings::ToUnicodeEx(
            u32::try_from(key.0).expect("VirtualKey codes are non-negative"),
            scan_code,
            state.as_ptr(),
            windows_core::PWSTR(utf16.as_mut_ptr()),
            i32::try_from(utf16.len()).expect("the buffer fits i32"),
            0x4,
            bindings::GetKeyboardLayout(0),
        )
    };
    if written < 0 {
        return Key::Named(NamedKey::Dead);
    }
    if written > 0 {
        let len = usize::try_from(written).expect("the count is positive");
        return Key::Character(String::from_utf16_lossy(&utf16[..len]));
    }
    Key::Named(NamedKey::Unidentified)
}

/// The named-key half of [`surface_key`]: every non-printable [`VirtualKey`]
/// the backend names, `None` for keys `ToUnicodeEx` should resolve.
fn named_key(key: VirtualKey) -> Option<NamedKey> {
    let named = match key {
        VirtualKey::Back => NamedKey::Backspace,
        VirtualKey::Tab => NamedKey::Tab,
        VirtualKey::Clear => NamedKey::Clear,
        VirtualKey::Enter => NamedKey::Enter,
        VirtualKey::Shift | VirtualKey::LeftShift | VirtualKey::RightShift => NamedKey::Shift,
        VirtualKey::Control | VirtualKey::LeftControl | VirtualKey::RightControl => {
            NamedKey::Control
        }
        VirtualKey::Menu | VirtualKey::LeftMenu | VirtualKey::RightMenu => NamedKey::Alt,
        VirtualKey::Pause => NamedKey::Pause,
        VirtualKey::CapitalLock => NamedKey::CapsLock,
        VirtualKey::Kana => NamedKey::KanaMode,
        VirtualKey::Junja => NamedKey::JunjaMode,
        VirtualKey::Final => NamedKey::FinalMode,
        VirtualKey::Kanji => NamedKey::KanjiMode,
        VirtualKey::Escape => NamedKey::Escape,
        VirtualKey::Convert => NamedKey::Convert,
        VirtualKey::NonConvert => NamedKey::NonConvert,
        VirtualKey::Accept => NamedKey::Accept,
        VirtualKey::ModeChange => NamedKey::ModeChange,
        VirtualKey::PageUp => NamedKey::PageUp,
        VirtualKey::PageDown => NamedKey::PageDown,
        VirtualKey::End => NamedKey::End,
        VirtualKey::Home => NamedKey::Home,
        VirtualKey::Left => NamedKey::ArrowLeft,
        VirtualKey::Up => NamedKey::ArrowUp,
        VirtualKey::Right => NamedKey::ArrowRight,
        VirtualKey::Down => NamedKey::ArrowDown,
        VirtualKey::Select => NamedKey::Select,
        VirtualKey::Print => NamedKey::Print,
        VirtualKey::Execute => NamedKey::Execute,
        VirtualKey::Snapshot => NamedKey::PrintScreen,
        VirtualKey::Insert => NamedKey::Insert,
        VirtualKey::Delete => NamedKey::Delete,
        VirtualKey::Help => NamedKey::Help,
        VirtualKey::LeftWindows | VirtualKey::RightWindows => NamedKey::Meta,
        VirtualKey::Application => NamedKey::ContextMenu,
        VirtualKey::Sleep => NamedKey::Standby,
        VirtualKey::F1 => NamedKey::F1,
        VirtualKey::F2 => NamedKey::F2,
        VirtualKey::F3 => NamedKey::F3,
        VirtualKey::F4 => NamedKey::F4,
        VirtualKey::F5 => NamedKey::F5,
        VirtualKey::F6 => NamedKey::F6,
        VirtualKey::F7 => NamedKey::F7,
        VirtualKey::F8 => NamedKey::F8,
        VirtualKey::F9 => NamedKey::F9,
        VirtualKey::F10 => NamedKey::F10,
        VirtualKey::F11 => NamedKey::F11,
        VirtualKey::F12 => NamedKey::F12,
        VirtualKey::F13 => NamedKey::F13,
        VirtualKey::F14 => NamedKey::F14,
        VirtualKey::F15 => NamedKey::F15,
        VirtualKey::F16 => NamedKey::F16,
        VirtualKey::F17 => NamedKey::F17,
        VirtualKey::F18 => NamedKey::F18,
        VirtualKey::F19 => NamedKey::F19,
        VirtualKey::F20 => NamedKey::F20,
        VirtualKey::F21 => NamedKey::F21,
        VirtualKey::F22 => NamedKey::F22,
        VirtualKey::F23 => NamedKey::F23,
        VirtualKey::F24 => NamedKey::F24,
        VirtualKey::NumberKeyLock => NamedKey::NumLock,
        VirtualKey::Scroll => NamedKey::ScrollLock,
        VirtualKey::GoBack => NamedKey::BrowserBack,
        VirtualKey::GoForward => NamedKey::BrowserForward,
        VirtualKey::Refresh => NamedKey::BrowserRefresh,
        VirtualKey::Stop => NamedKey::BrowserStop,
        VirtualKey::Search => NamedKey::BrowserSearch,
        VirtualKey::Favorites => NamedKey::BrowserFavorites,
        VirtualKey::GoHome => NamedKey::BrowserHome,
        _ => return None,
    };
    Some(named)
}

/// The W3C `KeyboardEvent.code` for a Set-1 scan code.
///
/// `is_extended` distinguishes the `0xE0`-prefixed codes (right modifiers,
/// the navigation block, `NumpadEnter`, `NumpadDivide`, `PrintScreen`) from
/// the base set; `CorePhysicalKeyStatus::is_extended_key` carries it.
pub(crate) fn surface_code(scan_code: u32, is_extended: bool) -> Code {
    if is_extended {
        extended_code(scan_code)
    } else {
        base_code(scan_code)
    }
}

/// The `0xE0`-prefixed half of the Set-1 table.
fn extended_code(scan_code: u32) -> Code {
    match scan_code {
        0x1C => Code::NumpadEnter,
        0x1D => Code::ControlRight,
        0x35 => Code::NumpadDivide,
        0x37 => Code::PrintScreen,
        0x38 => Code::AltRight,
        0x46 => Code::Pause,
        0x47 => Code::Home,
        0x48 => Code::ArrowUp,
        0x49 => Code::PageUp,
        0x4B => Code::ArrowLeft,
        0x4D => Code::ArrowRight,
        0x4F => Code::End,
        0x50 => Code::ArrowDown,
        0x51 => Code::PageDown,
        0x52 => Code::Insert,
        0x53 => Code::Delete,
        0x5B => Code::MetaLeft,
        0x5C => Code::MetaRight,
        0x5D => Code::ContextMenu,
        _ => Code::Unidentified,
    }
}

/// The unprefixed half of the Set-1 table.
fn base_code(scan_code: u32) -> Code {
    match scan_code {
        0x01 => Code::Escape,
        0x02 => Code::Digit1,
        0x03 => Code::Digit2,
        0x04 => Code::Digit3,
        0x05 => Code::Digit4,
        0x06 => Code::Digit5,
        0x07 => Code::Digit6,
        0x08 => Code::Digit7,
        0x09 => Code::Digit8,
        0x0A => Code::Digit9,
        0x0B => Code::Digit0,
        0x0C => Code::Minus,
        0x0D => Code::Equal,
        0x0E => Code::Backspace,
        0x0F => Code::Tab,
        0x10 => Code::KeyQ,
        0x11 => Code::KeyW,
        0x12 => Code::KeyE,
        0x13 => Code::KeyR,
        0x14 => Code::KeyT,
        0x15 => Code::KeyY,
        0x16 => Code::KeyU,
        0x17 => Code::KeyI,
        0x18 => Code::KeyO,
        0x19 => Code::KeyP,
        0x1A => Code::BracketLeft,
        0x1B => Code::BracketRight,
        0x1C => Code::Enter,
        0x1D => Code::ControlLeft,
        0x1E => Code::KeyA,
        0x1F => Code::KeyS,
        0x20 => Code::KeyD,
        0x21 => Code::KeyF,
        0x22 => Code::KeyG,
        0x23 => Code::KeyH,
        0x24 => Code::KeyJ,
        0x25 => Code::KeyK,
        0x26 => Code::KeyL,
        0x27 => Code::Semicolon,
        0x28 => Code::Quote,
        0x29 => Code::Backquote,
        0x2A => Code::ShiftLeft,
        0x2B => Code::Backslash,
        0x2C => Code::KeyZ,
        0x2D => Code::KeyX,
        0x2E => Code::KeyC,
        0x2F => Code::KeyV,
        0x30 => Code::KeyB,
        0x31 => Code::KeyN,
        0x32 => Code::KeyM,
        0x33 => Code::Comma,
        0x34 => Code::Period,
        0x35 => Code::Slash,
        0x36 => Code::ShiftRight,
        0x37 => Code::NumpadMultiply,
        0x38 => Code::AltLeft,
        0x39 => Code::Space,
        0x3A => Code::CapsLock,
        0x3B => Code::F1,
        0x3C => Code::F2,
        0x3D => Code::F3,
        0x3E => Code::F4,
        0x3F => Code::F5,
        0x40 => Code::F6,
        0x41 => Code::F7,
        0x42 => Code::F8,
        0x43 => Code::F9,
        0x44 => Code::F10,
        0x45 => Code::NumLock,
        0x46 => Code::ScrollLock,
        0x47 => Code::Numpad7,
        0x48 => Code::Numpad8,
        0x49 => Code::Numpad9,
        0x4A => Code::NumpadSubtract,
        0x4B => Code::Numpad4,
        0x4C => Code::Numpad5,
        0x4D => Code::Numpad6,
        0x4E => Code::NumpadAdd,
        0x4F => Code::Numpad1,
        0x50 => Code::Numpad2,
        0x51 => Code::Numpad3,
        0x52 => Code::Numpad0,
        0x53 => Code::NumpadDecimal,
        0x56 => Code::IntlBackslash,
        0x57 => Code::F11,
        0x58 => Code::F12,
        _ => Code::Unidentified,
    }
}

/// The current virtual-key state array from `GetKeyboardState` — shared by
/// the modifier chord below and `surface_key`'s `ToUnicodeEx` probe.
fn keyboard_state() -> [u8; 256] {
    let mut state = [0u8; 256];
    // SAFETY: `state` is a 256-byte buffer, exactly the layout the API fills.
    let read = unsafe { bindings::GetKeyboardState(state.as_mut_ptr()) };
    assert!(
        read.as_bool(),
        "GetKeyboardState failed on the UI thread that is handling a key event"
    );
    state
}

/// The W3C modifier chord, read from `GetKeyboardState` since a key event
/// carries none of its own.
pub(crate) fn surface_modifiers() -> Modifiers {
    // VK_SHIFT, VK_CONTROL, VK_MENU (Alt), VK_CAPITAL, VK_NUMLOCK, the Win keys.
    const VK_SHIFT: usize = 0x10;
    const VK_CONTROL: usize = 0x11;
    const VK_MENU: usize = 0x12;
    const VK_CAPITAL: usize = 0x14;
    const VK_LWIN: usize = 0x5B;
    const VK_RWIN: usize = 0x5C;
    const VK_NUMLOCK: usize = 0x90;
    // The high bit is "key is down"; the low bit is the toggle state.
    const DOWN: u8 = 0x80;
    const TOGGLED: u8 = 0x01;

    let state = keyboard_state();
    let mut modifiers = Modifiers::empty();
    modifiers.set(Modifiers::SHIFT, state[VK_SHIFT] & DOWN != 0);
    modifiers.set(Modifiers::CONTROL, state[VK_CONTROL] & DOWN != 0);
    modifiers.set(Modifiers::ALT, state[VK_MENU] & DOWN != 0);
    modifiers.set(
        Modifiers::META,
        state[VK_LWIN] & DOWN != 0 || state[VK_RWIN] & DOWN != 0,
    );
    modifiers.set(Modifiers::CAPS_LOCK, state[VK_CAPITAL] & TOGGLED != 0);
    modifiers.set(Modifiers::NUM_LOCK, state[VK_NUMLOCK] & TOGGLED != 0);
    modifiers
}
