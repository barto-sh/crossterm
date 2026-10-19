use std::collections::VecDeque;
use std::time::Duration;

use crossterm_winapi::{Console, ConsoleMode, Handle, InputRecord};

use crate::event::{
    Event,
    sys::windows::{parse::MouseButtonsPressed, poll::WinApiPoll},
};

#[cfg(feature = "event-stream")]
use crate::event::sys::Waker;
use crate::event::{
    internal::InternalEvent,
    source::EventSource,
    sys::windows::parse::{SurrogateBuffers, handle_key_event, handle_mouse_event},
    sys::windows::vt_input::VtInput,
    timeout::PollTimeout,
};

pub(crate) struct WindowsEventSource {
    console: Console,
    poll: WinApiPoll,
    surrogate_buffer: SurrogateBuffers,
    mouse_buttons_pressed: MouseButtonsPressed,
    // Preserve console record order while amortizing Win32 reads during a paste.
    pending_records: VecDeque<InputRecord>,
    vt_input: VtInput,
    vt_input_enabled: bool,
}

impl WindowsEventSource {
    pub(crate) fn new() -> std::io::Result<WindowsEventSource> {
        let console = Console::from(Handle::current_in_handle()?);
        Ok(WindowsEventSource {
            console,

            #[cfg(not(feature = "event-stream"))]
            poll: WinApiPoll::new(),
            #[cfg(feature = "event-stream")]
            poll: WinApiPoll::new()?,

            surrogate_buffer: SurrogateBuffers::default(),
            mouse_buttons_pressed: MouseButtonsPressed::default(),
            pending_records: VecDeque::new(),
            vt_input: VtInput::default(),
            vt_input_enabled: false,
        })
    }
}

impl EventSource for WindowsEventSource {
    fn try_read(&mut self, timeout: Option<Duration>) -> std::io::Result<Option<InternalEvent>> {
        let poll_timeout = PollTimeout::new(timeout);

        loop {
            if let Some(event) = self.vt_input.next_event() {
                return Ok(Some(event));
            }
            let records_ready = if self.pending_records.is_empty() {
                if let Some(true) = self
                    .poll
                    .poll(self.vt_input.timeout(poll_timeout.leftover()))?
                {
                    self.vt_input_enabled =
                        ConsoleMode::from(Handle::current_in_handle()?).mode()? & 0x0200 != 0;
                    self.pending_records
                        .extend(self.console.read_console_input()?);
                }
                !self.pending_records.is_empty()
            } else {
                true
            };
            if records_ready {
                if let Some(record) = self.pending_records.pop_front() {
                    let event = match record {
                        InputRecord::KeyEvent(record) => {
                            if self.vt_input_enabled
                                && record.key_down
                                && record.virtual_key_code == 0
                            {
                                self.vt_input.push_utf16(record.u_char);
                                self.vt_input.next_event()
                            } else {
                                let event = handle_key_event(record, &mut self.surrogate_buffer);
                                if self.vt_input_enabled && self.vt_input.is_buffering() {
                                    match event {
                                        Some(Event::Key(key))
                                            if key.kind == crate::event::KeyEventKind::Press =>
                                        {
                                            match key.code {
                                                crate::event::KeyCode::Char(ch) => {
                                                    self.vt_input.push_char(ch)
                                                }
                                                crate::event::KeyCode::Enter
                                                    if self.vt_input.in_paste() =>
                                                {
                                                    self.vt_input.push_char('\r')
                                                }
                                                crate::event::KeyCode::Tab
                                                    if self.vt_input.in_paste() =>
                                                {
                                                    self.vt_input.push_char('\t')
                                                }
                                                _ => {}
                                            }
                                        }
                                        _ => {}
                                    }
                                    self.vt_input.next_event()
                                } else {
                                    event.map(InternalEvent::Event)
                                }
                            }
                        }
                        InputRecord::MouseEvent(record) => {
                            let mouse_event =
                                handle_mouse_event(record, &self.mouse_buttons_pressed);
                            self.mouse_buttons_pressed = MouseButtonsPressed {
                                left: record.button_state.left_button(),
                                right: record.button_state.right_button(),
                                middle: record.button_state.middle_button(),
                            };

                            mouse_event.map(InternalEvent::Event)
                        }
                        InputRecord::WindowBufferSizeEvent(record) => {
                            // windows starts counting at 0, unix at 1, add one to replicate unix behaviour.
                            Some(InternalEvent::Event(Event::Resize(
                                (record.size.x as i32 + 1) as u16,
                                (record.size.y as i32 + 1) as u16,
                            )))
                        }
                        InputRecord::FocusEvent(record) => {
                            let event = if record.set_focus {
                                Event::FocusGained
                            } else {
                                Event::FocusLost
                            };
                            Some(InternalEvent::Event(event))
                        }
                        _ => None,
                    };

                    if let Some(event) = event {
                        return Ok(Some(event));
                    }
                }
            }

            if !records_ready {
                self.vt_input.flush_escape();
            }
            if let Some(event) = self.vt_input.next_event() {
                return Ok(Some(event));
            }
            if poll_timeout.elapsed() && self.pending_records.is_empty() {
                return Ok(None);
            }
        }
    }

    #[cfg(feature = "event-stream")]
    fn waker(&self) -> Waker {
        self.poll.waker()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{KeyCode, KeyEventKind};
    use winapi::um::{
        wincon::{INPUT_RECORD, KEY_EVENT},
        winuser::VK_SHIFT,
    };

    fn key_record(virtual_key: u16, character: u16) -> InputRecord {
        let mut record: INPUT_RECORD = unsafe { std::mem::zeroed() };
        record.EventType = KEY_EVENT;
        unsafe {
            let key = record.Event.KeyEvent_mut();
            key.bKeyDown = 1;
            key.wRepeatCount = 1;
            key.wVirtualKeyCode = virtual_key;
            *key.uChar.UnicodeChar_mut() = character;
        }
        InputRecord::from(record)
    }

    #[test]
    fn zero_timeout_skips_ignored_records_already_in_memory() {
        let mut source = WindowsEventSource::new().expect("event source");
        source
            .pending_records
            .extend([key_record(VK_SHIFT as u16, 0), key_record(0, 'x' as u16)]);
        let event = source
            .try_read(Some(Duration::ZERO))
            .expect("buffered read");
        assert!(matches!(event, Some(InternalEvent::Event(Event::Key(key)))
            if key.code == KeyCode::Char('x') && key.kind == KeyEventKind::Press));
    }

    #[test]
    #[cfg(feature = "bracketed-paste")]
    fn queued_opener_is_drained_before_escape_timeout() {
        let mut source = WindowsEventSource::new().expect("event source");
        source.vt_input_enabled = true;
        source.vt_input.push_char('\x1b');
        source.pending_records.extend(
            "[200~text\r\n\x1b[201~"
                .encode_utf16()
                .map(|unit| key_record(0, unit)),
        );
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(
            source
                .try_read(Some(Duration::ZERO))
                .expect("buffered paste"),
            Some(InternalEvent::Event(Event::Paste("text\r\n".into())))
        );
        assert!(source.pending_records.is_empty());
    }
}
