//! Regression coverage for characters delivered through Windows console input records.

use super::*;
use crossterm_winapi::InputRecord;
use winapi::um::wincon::{INPUT_RECORD, KEY_EVENT};

fn alt_code_record(virtual_key: u16, key_down: bool, unit: u16) -> KeyEventRecord {
    let mut record: INPUT_RECORD = unsafe { std::mem::zeroed() };
    record.EventType = KEY_EVENT;
    unsafe {
        let key = record.Event.KeyEvent_mut();
        key.bKeyDown = i32::from(key_down);
        key.wRepeatCount = 1;
        key.wVirtualKeyCode = virtual_key;
        *key.uChar.UnicodeChar_mut() = unit;
        key.dwControlKeyState =
            if key_down || (VK_NUMPAD0..=VK_NUMPAD9).contains(&(virtual_key as i32)) {
                LEFT_ALT_PRESSED
            } else {
                0
            };
    }
    let InputRecord::KeyEvent(key) = InputRecord::from(record) else {
        panic!("expected a key input record");
    };
    key
}

#[test]
fn committed_alt_code_surrogates_produce_text_presses() {
    let mut buffers = SurrogateBuffers::default();
    for ch in ['🦀', '🥚'] {
        let mut units = [0u16; 2];
        let units = ch.encode_utf16(&mut units);
        for (index, unit) in units.iter().enumerate() {
            // The real ConPTY trace uses ignored Alt/numpad events between the
            // UTF-16 units, then commits each unit on VK_MENU key-up.
            for record in [
                alt_code_record(VK_MENU as u16, true, 0),
                alt_code_record((VK_NUMPAD0 + 6) as u16, true, 0),
                alt_code_record((VK_NUMPAD0 + 6) as u16, false, 0),
            ] {
                assert_eq!(handle_key_event(record, &mut buffers), None);
            }
            assert_eq!(
                handle_key_event(alt_code_record(VK_MENU as u16, false, *unit), &mut buffers),
                (index == 1)
                    .then(|| Event::Key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)))
            );
        }
    }
}

#[test]
fn committed_alt_code_scalar_produces_a_text_press() {
    let mut buffers = SurrogateBuffers::default();
    for ch in ['é', '界'] {
        assert_eq!(
            handle_key_event(
                alt_code_record(VK_MENU as u16, false, ch as u16),
                &mut buffers
            ),
            Some(Event::Key(KeyEvent::new(
                KeyCode::Char(ch),
                KeyModifiers::NONE
            )))
        );
    }
}

#[test]
fn preserves_escape_and_literal_characters_from_console_records() {
    for (virtual_key_code, character, code) in [
        (0, '\u{1b}', KeyCode::Esc),
        (VK_ESCAPE as u16, '\u{1b}', KeyCode::Esc),
        (0, '[', KeyCode::Char('[')),
        (0, '1', KeyCode::Char('1')),
        (0, '3', KeyCode::Char('3')),
        (0, ';', KeyCode::Char(';')),
        (0, '2', KeyCode::Char('2')),
        (0, 'u', KeyCode::Char('u')),
    ] {
        for (key_down, kind) in [(1, KeyEventKind::Press), (0, KeyEventKind::Release)] {
            for (control_key_state, modifiers) in [
                (0, KeyModifiers::NONE),
                (SHIFT_PRESSED, KeyModifiers::SHIFT),
            ] {
                // INPUT_RECORD contains only integers and unions of integer records.
                let mut record: INPUT_RECORD = unsafe { std::mem::zeroed() };
                record.EventType = KEY_EVENT;
                // Select the KEY_EVENT union member before conversion by crossterm_winapi.
                unsafe {
                    let key = record.Event.KeyEvent_mut();
                    key.bKeyDown = key_down;
                    key.wRepeatCount = 1;
                    key.wVirtualKeyCode = virtual_key_code;
                    *key.uChar.UnicodeChar_mut() = character as u16;
                    key.dwControlKeyState = control_key_state;
                }
                let InputRecord::KeyEvent(key) = InputRecord::from(record) else {
                    panic!("expected a key input record");
                };
                let mut surrogate_buffer = SurrogateBuffers::default();
                assert_eq!(
                    handle_key_event(key, &mut surrogate_buffer),
                    Some(Event::Key(KeyEvent::new_with_kind(code, modifiers, kind))),
                    "virtual key {virtual_key_code}, character {character:?}"
                );
            }
        }
    }
}

#[test]
fn preserves_interleaved_utf16_press_and_release_records() {
    let mut surrogate_buffer = SurrogateBuffers::default();
    for ch in ['🦀', '🥚'] {
        let mut units = [0u16; 2];
        let units = ch.encode_utf16(&mut units);
        for (unit, key_down, expected) in [
            (units[0], 1, None),
            (units[0], 0, None),
            (units[1], 1, Some(KeyEventKind::Press)),
            (units[1], 0, Some(KeyEventKind::Release)),
        ] {
            let mut record: INPUT_RECORD = unsafe { std::mem::zeroed() };
            record.EventType = KEY_EVENT;
            unsafe {
                let key = record.Event.KeyEvent_mut();
                key.bKeyDown = key_down;
                key.wRepeatCount = 1;
                *key.uChar.UnicodeChar_mut() = unit;
            }
            let InputRecord::KeyEvent(key) = InputRecord::from(record) else {
                panic!("expected key input record");
            };
            assert_eq!(
                handle_key_event(key, &mut surrogate_buffer),
                expected.map(|kind| Event::Key(KeyEvent::new_with_kind(
                    KeyCode::Char(ch),
                    KeyModifiers::NONE,
                    kind
                )))
            );
        }
    }
}
