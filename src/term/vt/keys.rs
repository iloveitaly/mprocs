//! Key protocol tables shared by the input decoder (bytes from the outer
//! terminal) and the pty encoder (bytes to child programs), so what dekit
//! understands and what it speaks stay symmetric.

use crate::term::key::{
  Key, KeyCode, KeyEventKind, KeyEventState, KeyMods, MediaKeyCode, ModKeyCode,
};

/// CSI/SS3 cursor-key finals.
pub fn final_key(final_: u8) -> Option<KeyCode> {
  Some(match final_ {
    b'A' => KeyCode::Up,
    b'B' => KeyCode::Down,
    b'C' => KeyCode::Right,
    b'D' => KeyCode::Left,
    b'F' => KeyCode::End,
    b'H' => KeyCode::Home,
    b'P' => KeyCode::F(1),
    b'Q' => KeyCode::F(2),
    // F3 is CSI R, which collides with the cursor position report.
    b'S' => KeyCode::F(4),
    _ => return None,
  })
}

/// SS3 finals: like `final_key` plus F3, whose CSI form collides with the
/// cursor position report.
pub fn ss3_key(final_: u8) -> Option<KeyCode> {
  match final_ {
    b'R' => Some(KeyCode::F(3)),
    _ => final_key(final_),
  }
}

pub fn cursor_key_final(code: KeyCode) -> Option<u8> {
  Some(match code {
    KeyCode::Up => b'A',
    KeyCode::Down => b'B',
    KeyCode::Right => b'C',
    KeyCode::Left => b'D',
    KeyCode::End => b'F',
    KeyCode::Home => b'H',
    _ => return None,
  })
}

/// `CSI n ~` key numbers (xterm assignments).
pub fn tilde_key(n: u16) -> Option<KeyCode> {
  Some(match n {
    1 | 7 => KeyCode::Home,
    2 => KeyCode::Insert,
    3 => KeyCode::Delete,
    4 | 8 => KeyCode::End,
    5 => KeyCode::PageUp,
    6 => KeyCode::PageDown,
    n @ 11..=15 => KeyCode::F(n as u8 - 10),
    n @ 17..=21 => KeyCode::F(n as u8 - 11),
    n @ 23..=26 => KeyCode::F(n as u8 - 12),
    n @ 28..=29 => KeyCode::F(n as u8 - 13),
    n @ 31..=34 => KeyCode::F(n as u8 - 14),
    _ => return None,
  })
}

pub fn key_tilde(code: KeyCode) -> Option<u16> {
  Some(match code {
    KeyCode::Insert => 2,
    KeyCode::Delete => 3,
    KeyCode::PageUp => 5,
    KeyCode::PageDown => 6,
    KeyCode::F(n) => match n {
      1..=5 => n as u16 + 10,
      6..=10 => n as u16 + 11,
      11..=14 => n as u16 + 12,
      15..=16 => n as u16 + 13,
      17..=20 => n as u16 + 14,
      _ => return None,
    },
    _ => return None,
  })
}

/// Map c to its Ctrl equivalent.
pub fn ctrl_char(c: char) -> Option<char> {
  Some(match c {
    '@' | '`' | ' ' | '2' => '\x00',
    'A'..='Z' => ((c as u8 - b'A') + 1) as char,
    'a'..='z' => ((c as u8 - b'a') + 1) as char,
    '[' | '3' | '{' => '\x1b',
    '\\' | '4' | '|' => '\x1c',
    ']' | '5' | '}' => '\x1d',
    '^' | '6' | '~' => '\x1e',
    '_' | '7' | '/' => '\x1f',
    '8' | '?' => '\x7f', // `Delete`
    _ => return None,
  })
}

/// The key press a plain char or C0 byte decodes to.
pub fn char_key(c: char) -> Option<Key> {
  let key = match c {
    '\r' | '\n' => Key::new(KeyCode::Enter, KeyMods::NONE),
    '\t' => Key::new(KeyCode::Tab, KeyMods::NONE),
    '\x7F' => Key::new(KeyCode::Backspace, KeyMods::NONE),
    c @ '\x01'..='\x1A' => Key::new(
      KeyCode::Char((c as u8 - 0x1 + b'a') as char),
      KeyMods::CONTROL,
    ),
    c @ '\x1C'..='\x1F' => Key::new(
      KeyCode::Char((c as u8 - 0x1C + b'4') as char),
      KeyMods::CONTROL,
    ),
    '\0' => Key::new(KeyCode::Char(' '), KeyMods::CONTROL),
    c if (c as u32) < 0x20 || c == '\u{8d}' => return None,
    c => Key::new(KeyCode::Char(c), KeyMods::NONE),
  };
  Some(key)
}

/// xterm-style modifier mask (the wire value is this plus one).
pub fn mods_mask(mods: KeyMods) -> u8 {
  let mut mask = 0;
  if mods.contains(KeyMods::SHIFT) {
    mask |= 1;
  }
  if mods.contains(KeyMods::ALT) {
    mask |= 2;
  }
  if mods.contains(KeyMods::CONTROL) {
    mask |= 4;
  }
  mask
}

pub fn mask_mods(wire: u8) -> KeyMods {
  let mask = wire.saturating_sub(1);
  let mut mods = KeyMods::empty();
  if mask & 1 != 0 {
    mods |= KeyMods::SHIFT;
  }
  if mask & 2 != 0 {
    mods |= KeyMods::ALT;
  }
  if mask & 4 != 0 {
    mods |= KeyMods::CONTROL;
  }
  if mask & 8 != 0 {
    mods |= KeyMods::SUPER;
  }
  if mask & 16 != 0 {
    mods |= KeyMods::HYPER;
  }
  if mask & 32 != 0 {
    mods |= KeyMods::META;
  }
  mods
}

pub fn mask_state(wire: u8) -> KeyEventState {
  let mask = wire.saturating_sub(1);
  let mut state = KeyEventState::empty();
  if mask & 64 != 0 {
    state |= KeyEventState::CAPS_LOCK;
  }
  if mask & 128 != 0 {
    state |= KeyEventState::NUM_LOCK;
  }
  state
}

pub fn event_kind(n: u8) -> KeyEventKind {
  match n {
    1 => KeyEventKind::Press,
    2 => KeyEventKind::Repeat,
    3 => KeyEventKind::Release,
    _ => KeyEventKind::Press,
  }
}

/// Kitty keyboard protocol functional key codepoints.
pub fn functional_key(codepoint: u32) -> Option<(KeyCode, KeyEventState)> {
  let keypad = match codepoint {
    57399 => Some(KeyCode::Char('0')),
    57400 => Some(KeyCode::Char('1')),
    57401 => Some(KeyCode::Char('2')),
    57402 => Some(KeyCode::Char('3')),
    57403 => Some(KeyCode::Char('4')),
    57404 => Some(KeyCode::Char('5')),
    57405 => Some(KeyCode::Char('6')),
    57406 => Some(KeyCode::Char('7')),
    57407 => Some(KeyCode::Char('8')),
    57408 => Some(KeyCode::Char('9')),
    57409 => Some(KeyCode::Char('.')),
    57410 => Some(KeyCode::Char('/')),
    57411 => Some(KeyCode::Char('*')),
    57412 => Some(KeyCode::Char('-')),
    57413 => Some(KeyCode::Char('+')),
    57414 => Some(KeyCode::Enter),
    57415 => Some(KeyCode::Char('=')),
    57416 => Some(KeyCode::Char(',')),
    57417 => Some(KeyCode::Left),
    57418 => Some(KeyCode::Right),
    57419 => Some(KeyCode::Up),
    57420 => Some(KeyCode::Down),
    57421 => Some(KeyCode::PageUp),
    57422 => Some(KeyCode::PageDown),
    57423 => Some(KeyCode::Home),
    57424 => Some(KeyCode::End),
    57425 => Some(KeyCode::Insert),
    57426 => Some(KeyCode::Delete),
    57427 => Some(KeyCode::KeypadBegin),
    _ => None,
  };
  if let Some(code) = keypad {
    return Some((code, KeyEventState::KEYPAD));
  }

  let code = match codepoint {
    57358 => KeyCode::CapsLock,
    57359 => KeyCode::ScrollLock,
    57360 => KeyCode::NumLock,
    57361 => KeyCode::PrintScreen,
    57362 => KeyCode::Pause,
    57363 => KeyCode::Menu,
    n @ 57376..=57398 => KeyCode::F((n - 57376 + 13) as u8),
    57428 => KeyCode::Media(MediaKeyCode::Play),
    57429 => KeyCode::Media(MediaKeyCode::Pause),
    57430 => KeyCode::Media(MediaKeyCode::PlayPause),
    57431 => KeyCode::Media(MediaKeyCode::Reverse),
    57432 => KeyCode::Media(MediaKeyCode::Stop),
    57433 => KeyCode::Media(MediaKeyCode::FastForward),
    57434 => KeyCode::Media(MediaKeyCode::Rewind),
    57435 => KeyCode::Media(MediaKeyCode::Next),
    57436 => KeyCode::Media(MediaKeyCode::Prev),
    57437 => KeyCode::Media(MediaKeyCode::Record),
    57438 => KeyCode::Media(MediaKeyCode::VolumeDown),
    57439 => KeyCode::Media(MediaKeyCode::VolumeUp),
    57440 => KeyCode::Media(MediaKeyCode::VolumeMute),
    57441 => KeyCode::Modifier(ModKeyCode::LeftShift),
    57442 => KeyCode::Modifier(ModKeyCode::LeftControl),
    57443 => KeyCode::Modifier(ModKeyCode::LeftAlt),
    57444 => KeyCode::Modifier(ModKeyCode::LeftSuper),
    57445 => KeyCode::Modifier(ModKeyCode::LeftHyper),
    57446 => KeyCode::Modifier(ModKeyCode::LeftMeta),
    57447 => KeyCode::Modifier(ModKeyCode::RightShift),
    57448 => KeyCode::Modifier(ModKeyCode::RightControl),
    57449 => KeyCode::Modifier(ModKeyCode::RightAlt),
    57450 => KeyCode::Modifier(ModKeyCode::RightSuper),
    57451 => KeyCode::Modifier(ModKeyCode::RightHyper),
    57452 => KeyCode::Modifier(ModKeyCode::RightMeta),
    _ => return None,
  };
  Some((code, KeyEventState::empty()))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn tilde_round_trip() {
    for code in [
      KeyCode::Insert,
      KeyCode::Delete,
      KeyCode::PageUp,
      KeyCode::PageDown,
      KeyCode::F(1),
      KeyCode::F(5),
      KeyCode::F(6),
      KeyCode::F(12),
      KeyCode::F(20),
    ] {
      let n = key_tilde(code).unwrap();
      assert_eq!(tilde_key(n), Some(code), "tilde {n}");
    }
  }

  #[test]
  fn ctrl_chars() {
    assert_eq!(ctrl_char('a'), Some('\x01'));
    assert_eq!(ctrl_char('Z'), Some('\x1a'));
    assert_eq!(ctrl_char(' '), Some('\x00'));
    assert_eq!(ctrl_char('['), Some('\x1b'));
    assert_eq!(ctrl_char('é'), None);
  }
}
