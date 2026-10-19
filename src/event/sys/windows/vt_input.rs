//! Decode VT characters carried by native Windows console records.
//!
//! ConPTY preserves paste boundaries when VT input is enabled. Native key-up,
//! modifier, mouse and resize records still coexist with this character stream.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use super::vt_parse::parse_event;
use crate::event::internal::InternalEvent;

// Match the existing Unix input parser's standalone-Escape ambiguity window.
const ESCAPE_TIMEOUT: Duration = Duration::from_millis(20);

#[derive(Default)]
pub(crate) struct VtInput {
    buffer: Vec<u8>,
    high_surrogate: Option<u16>,
    escape_deadline: Option<Instant>,
    events: VecDeque<InternalEvent>,
}

impl VtInput {
    pub(crate) fn is_buffering(&self) -> bool {
        !self.buffer.is_empty()
    }

    pub(crate) fn in_paste(&self) -> bool {
        self.buffer.starts_with(b"\x1b[200~")
    }

    pub(crate) fn push_utf16(&mut self, unit: u16) {
        if let Some(high) = self.high_surrogate.take() {
            if (0xdc00..=0xdfff).contains(&unit) {
                let scalar =
                    0x10000 + ((u32::from(high) - 0xd800) << 10) + u32::from(unit) - 0xdc00;
                self.push_char(char::from_u32(scalar).unwrap_or(char::REPLACEMENT_CHARACTER));
                return;
            }
            self.push_char(char::REPLACEMENT_CHARACTER);
        }
        match unit {
            0xd800..=0xdbff => self.high_surrogate = Some(unit),
            0xdc00..=0xdfff => self.push_char(char::REPLACEMENT_CHARACTER),
            _ => self
                .push_char(char::from_u32(u32::from(unit)).unwrap_or(char::REPLACEMENT_CHARACTER)),
        }
    }

    pub(crate) fn push_char(&mut self, ch: char) {
        let mut utf8 = [0u8; 4];
        for byte in ch.encode_utf8(&mut utf8).bytes() {
            self.buffer.push(byte);
            match parse_event(&self.buffer, true) {
                Ok(Some(event)) => {
                    // Windows cursor/palette queries still use their native APIs.
                    if matches!(event, InternalEvent::Event(_)) {
                        self.events.push_back(event);
                    }
                    self.buffer.clear();
                }
                Ok(None) => {
                    let finished_osc = self.buffer.starts_with(b"\x1b]")
                        && (byte == 7 || self.buffer.ends_with(b"\x1b\\"));
                    let finished_csi = self.buffer.len() > 2
                        && self.buffer.starts_with(b"\x1b[")
                        && (0x40..=0x7e).contains(&byte)
                        && !self.in_paste()
                        && !self.buffer.starts_with(b"\x1b[[")
                        && !self.buffer.starts_with(b"\x1b[M");
                    if finished_osc || finished_csi {
                        self.buffer.clear();
                    }
                }
                Err(_) => self.buffer.clear(),
            }
        }
        self.escape_deadline =
            (self.buffer.as_slice() == b"\x1b").then(|| Instant::now() + ESCAPE_TIMEOUT);
    }

    pub(crate) fn timeout(&self, timeout: Option<Duration>) -> Option<Duration> {
        self.escape_deadline.map_or(timeout, |deadline| {
            let remaining = deadline.saturating_duration_since(Instant::now());
            Some(timeout.map_or(remaining, |timeout| timeout.min(remaining)))
        })
    }

    // Only resolve a lone Escape after the source has drained both its local
    // records and the ready OS queue. A descheduled consumer must not split an
    // already-queued paste opener merely because wall-clock time elapsed.
    pub(crate) fn flush_escape(&mut self) {
        if self
            .escape_deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.escape_deadline = None;
            if let Ok(Some(event)) = parse_event(&self.buffer, false) {
                self.events.push_back(event);
            }
            self.buffer.clear();
        }
    }

    pub(crate) fn next_event(&mut self) -> Option<InternalEvent> {
        self.events.pop_front()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Event, KeyCode, KeyEvent, KeyModifiers};

    fn push(input: &mut VtInput, text: &str) {
        for unit in text.encode_utf16() {
            input.push_utf16(unit);
        }
    }

    #[test]
    #[cfg(feature = "bracketed-paste")]
    fn paste_preserves_unicode_whitespace_and_empty_payload() {
        for payload in ["", "\n\nZażółć 界 🦀\r\n\t\nEND\n\n"] {
            let mut input = VtInput::default();
            push(&mut input, "\x1b[200~");
            push(&mut input, payload);
            assert!(input.next_event().is_none());
            push(&mut input, "\x1b[201~");
            assert_eq!(
                input.next_event(),
                Some(InternalEvent::Event(Event::Paste(payload.into())))
            );
            assert!(input.next_event().is_none());
        }
    }

    #[test]
    #[cfg(feature = "bracketed-paste")]
    fn queued_paste_opener_survives_consumer_descheduling() {
        let mut input = VtInput::default();
        push(&mut input, "\x1b");
        input.escape_deadline = Some(Instant::now() - ESCAPE_TIMEOUT);
        // Checking for a decoded event is not proof the OS input queue is empty.
        assert!(input.next_event().is_none());
        push(&mut input, "[200~text\x1b[201~");
        assert_eq!(
            input.next_event(),
            Some(InternalEvent::Event(Event::Paste("text".into())))
        );
    }

    #[test]
    fn standalone_escape_resolves_only_after_idle() {
        let mut input = VtInput::default();
        push(&mut input, "\x1b");
        input.escape_deadline = Some(Instant::now() - ESCAPE_TIMEOUT);
        input.flush_escape();
        assert_eq!(
            input.next_event(),
            Some(InternalEvent::Event(Event::Key(KeyCode::Esc.into())))
        );
        assert!(!input.is_buffering());
    }

    #[test]
    fn fragmented_keyboard_sequences_and_alt_text() {
        let mut input = VtInput::default();
        for (wire, key) in [
            ("\x1b[D", KeyEvent::new(KeyCode::Left, KeyModifiers::NONE)),
            (
                "\x1b[1;5C",
                KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL),
            ),
            (
                "\x1bx",
                KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT),
            ),
        ] {
            for ch in wire.chars() {
                input.push_char(ch);
            }
            assert_eq!(
                input.next_event(),
                Some(InternalEvent::Event(Event::Key(key)))
            );
        }
    }

    #[test]
    fn malformed_utf16_does_not_consume_the_following_scalar() {
        let mut input = VtInput::default();
        input.push_utf16(0xd800);
        input.push_utf16('x' as u16);
        assert!(
            matches!(input.next_event(), Some(InternalEvent::Event(Event::Key(key))) if key.code == KeyCode::Char(char::REPLACEMENT_CHARACTER))
        );
        assert!(
            matches!(input.next_event(), Some(InternalEvent::Event(Event::Key(key))) if key.code == KeyCode::Char('x'))
        );
    }
}
