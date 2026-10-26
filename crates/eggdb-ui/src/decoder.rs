use crate::terminal::{Event, KeyCode, KeyEvent, Modifiers};

const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";
const LONGEST_SEQUENCE: usize = 64;

/// Turns the bytes a terminal sends into [`Event`]s: keys, escape sequences (CSI and SS3),
/// UTF-8 characters and bracketed paste, across reads that split them anywhere.
///
/// A lone `Esc` byte could start a sequence, so it stays pending until more bytes arrive or
/// the caller decides none will (25 ms of quiet, in the real terminal) and calls
/// [`InputDecoder::flush`]. Hostile or garbled input never panics; bytes that make no sense are
/// dropped or shown as U+FFFD.
#[derive(Debug, Default)]
pub struct InputDecoder {
    pending: Vec<u8>,
    paste: Option<Vec<u8>>,
}

enum Step {
    Event(Option<Event>, usize),
    NeedMore,
}

impl InputDecoder {
    /// A decoder with nothing pending.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds bytes and returns every event they complete.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Event> {
        self.pending.extend_from_slice(bytes);
        self.decode(false)
    }

    /// Whether bytes are waiting for more to complete them.
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty() && self.paste.is_none()
    }

    /// Decodes what is pending as if no more bytes will come: a lone `Esc` is the Esc key, and
    /// an incomplete sequence becomes the keys its bytes spell.
    pub fn flush(&mut self) -> Vec<Event> {
        self.decode(true)
    }

    fn decode(&mut self, flush: bool) -> Vec<Event> {
        let mut events = Vec::new();
        let mut at = 0;
        while at < self.pending.len() {
            if let Some(paste) = self.paste.as_mut() {
                let rest = &self.pending[at..];
                match find(rest, PASTE_END) {
                    Some(end) => {
                        paste.extend_from_slice(&rest[..end]);
                        let text = String::from_utf8_lossy(paste).into_owned();
                        self.paste = None;
                        events.push(Event::Paste(text));
                        at += end + PASTE_END.len();
                        continue;
                    }
                    None => {
                        let keep = longest_partial_suffix(rest, PASTE_END);
                        paste.extend_from_slice(&rest[..rest.len() - keep]);
                        at = self.pending.len() - keep;
                        break;
                    }
                }
            }
            if self.pending[at..].starts_with(PASTE_START) {
                self.paste = Some(Vec::new());
                at += PASTE_START.len();
                continue;
            }
            match step(&self.pending[at..], flush) {
                Step::Event(event, used) => {
                    events.extend(event);
                    at += used.max(1);
                }
                Step::NeedMore => break,
            }
        }
        self.pending.drain(..at.min(self.pending.len()));
        events
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn longest_partial_suffix(bytes: &[u8], needle: &[u8]) -> usize {
    (1..needle.len())
        .rev()
        .find(|&length| bytes.ends_with(&needle[..length]))
        .unwrap_or(0)
}

fn key(code: KeyCode, modifiers: Modifiers) -> Option<Event> {
    Some(Event::Key(KeyEvent { code, modifiers }))
}

fn step(bytes: &[u8], flush: bool) -> Step {
    match bytes[0] {
        0x1b => escape(bytes, flush),
        _ => match single(bytes, flush) {
            Some((event, used)) => Step::Event(event, used),
            None => Step::NeedMore,
        },
    }
}

fn single(bytes: &[u8], flush: bool) -> Option<(Option<Event>, usize)> {
    let none = Modifiers::NONE;
    let byte = bytes[0];
    let event = match byte {
        b'\r' | b'\n' => key(KeyCode::Enter, none),
        b'\t' => key(KeyCode::Tab, none),
        0x7f | 0x08 => key(KeyCode::Backspace, none),
        0x00 => key(KeyCode::Char(' '), Modifiers::CTRL),
        0x01..=0x1a => key(KeyCode::Char(char::from(b'a' + byte - 1)), Modifiers::CTRL),
        0x1c..=0x1f => key(KeyCode::Char(char::from(byte + 0x40)), Modifiers::CTRL),
        _ => return utf8(bytes, flush),
    };
    Some((event, 1))
}

fn utf8(bytes: &[u8], flush: bool) -> Option<(Option<Event>, usize)> {
    let length = match bytes[0] {
        0x00..=0x7f => 1,
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return Some((key(KeyCode::Char('\u{fffd}'), Modifiers::NONE), 1)),
    };
    if bytes.len() < length {
        let continuations_so_far = bytes[1..].iter().all(|b| b & 0xc0 == 0x80);
        if !flush && continuations_so_far {
            return None;
        }
        return Some((key(KeyCode::Char('\u{fffd}'), Modifiers::NONE), 1));
    }
    match std::str::from_utf8(&bytes[..length])
        .ok()
        .and_then(|text| text.chars().next())
    {
        Some(character) => Some((key(KeyCode::Char(character), Modifiers::NONE), length)),
        None => Some((key(KeyCode::Char('\u{fffd}'), Modifiers::NONE), 1)),
    }
}

fn escape(bytes: &[u8], flush: bool) -> Step {
    let Some(&next) = bytes.get(1) else {
        return if flush {
            Step::Event(key(KeyCode::Esc, Modifiers::NONE), 1)
        } else {
            Step::NeedMore
        };
    };
    match next {
        b'[' => csi(bytes, flush),
        b'O' => ss3(bytes, flush),
        0x1b => Step::Event(key(KeyCode::Esc, Modifiers::NONE), 1),
        _ => match single(&bytes[1..], flush) {
            Some((Some(Event::Key(mut event)), used)) => {
                event.modifiers.alt = true;
                Step::Event(Some(Event::Key(event)), used + 1)
            }
            Some((other, used)) => Step::Event(other, used + 1),
            None => Step::NeedMore,
        },
    }
}

fn ss3(bytes: &[u8], flush: bool) -> Step {
    let Some(&last) = bytes.get(2) else {
        return if flush {
            Step::Event(key(KeyCode::Char('O'), Modifiers::ALT), 2)
        } else {
            Step::NeedMore
        };
    };
    let code = match last {
        b'A' => KeyCode::Up,
        b'B' => KeyCode::Down,
        b'C' => KeyCode::Right,
        b'D' => KeyCode::Left,
        b'H' => KeyCode::Home,
        b'F' => KeyCode::End,
        b'P' => KeyCode::F(1),
        b'Q' => KeyCode::F(2),
        b'R' => KeyCode::F(3),
        b'S' => KeyCode::F(4),
        b'M' => KeyCode::Enter,
        _ => return Step::Event(None, 3),
    };
    Step::Event(key(code, Modifiers::NONE), 3)
}

fn csi(bytes: &[u8], flush: bool) -> Step {
    let body = &bytes[2..];
    let scanned = &body[..body.len().min(LONGEST_SEQUENCE + 1)];
    let Some(final_at) = scanned.iter().position(|b| !(0x20..=0x3f).contains(b)) else {
        if scanned.len() > LONGEST_SEQUENCE {
            return Step::Event(None, 2 + LONGEST_SEQUENCE);
        }
        if !flush {
            return Step::NeedMore;
        }
        return if body.is_empty() {
            Step::Event(key(KeyCode::Char('['), Modifiers::ALT), 2)
        } else {
            Step::Event(None, 2 + body.len())
        };
    };
    if !(0x40..=0x7e).contains(&body[final_at]) {
        return Step::Event(None, 2 + final_at);
    }
    let used = 2 + final_at + 1;
    let parameters = &body[..final_at];
    let numbers: Vec<u32> = parameters
        .split(|&b| b == b';')
        .map(|part| {
            part.iter()
                .filter(|d| d.is_ascii_digit())
                .try_fold(0u32, |n, d| {
                    n.checked_mul(10)?.checked_add(u32::from(*d - b'0'))
                })
                .unwrap_or(0)
        })
        .collect();
    let modifiers = modifiers_from(numbers.get(1).copied());
    let code = match body[final_at] {
        b'A' => KeyCode::Up,
        b'B' => KeyCode::Down,
        b'C' => KeyCode::Right,
        b'D' => KeyCode::Left,
        b'H' => KeyCode::Home,
        b'F' => KeyCode::End,
        b'Z' => KeyCode::BackTab,
        b'P' => KeyCode::F(1),
        b'Q' => KeyCode::F(2),
        b'R' => KeyCode::F(3),
        b'S' => KeyCode::F(4),
        b'~' => match numbers.first().copied().unwrap_or(0) {
            1 | 7 => KeyCode::Home,
            2 => KeyCode::Insert,
            3 => KeyCode::Delete,
            4 | 8 => KeyCode::End,
            5 => KeyCode::PageUp,
            6 => KeyCode::PageDown,
            11..=15 => KeyCode::F(digit(numbers[0] - 10)),
            17..=21 => KeyCode::F(digit(numbers[0] - 11)),
            23 | 24 => KeyCode::F(digit(numbers[0] - 12)),
            _ => return Step::Event(None, used),
        },
        _ => return Step::Event(None, used),
    };
    Step::Event(key(code, modifiers), used)
}

fn digit(n: u32) -> u8 {
    u8::try_from(n).unwrap_or(0)
}

fn modifiers_from(parameter: Option<u32>) -> Modifiers {
    let bits = parameter.unwrap_or(1).saturating_sub(1);
    Modifiers {
        shift: bits & 1 != 0,
        alt: bits & 2 != 0,
        ctrl: bits & 4 != 0,
    }
}

#[cfg(test)]
mod tests {
    use super::InputDecoder;
    use crate::terminal::{Event, KeyCode, KeyEvent, Modifiers};

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::plain(code))
    }

    fn with(code: KeyCode, modifiers: Modifiers) -> Event {
        Event::Key(KeyEvent { code, modifiers })
    }

    fn all(bytes: &[u8]) -> Vec<Event> {
        let mut decoder = InputDecoder::new();
        let mut events = decoder.feed(bytes);
        events.extend(decoder.flush());
        events
    }

    #[test]
    fn arrows_and_editing_keys_in_both_spellings() {
        assert_eq!(
            all(b"\x1b[A\x1b[B\x1bOC\x1bOD\x1b[H\x1b[4~\x1b[5~\x1b[6~\x1b[3~\x1b[Z"),
            [
                key(KeyCode::Up),
                key(KeyCode::Down),
                key(KeyCode::Right),
                key(KeyCode::Left),
                key(KeyCode::Home),
                key(KeyCode::End),
                key(KeyCode::PageUp),
                key(KeyCode::PageDown),
                key(KeyCode::Delete),
                key(KeyCode::BackTab),
            ]
        );
    }

    #[test]
    fn modifier_parameters_are_decoded() {
        assert_eq!(
            all(b"\x1b[1;5C\x1b[1;2A\x1b[5;3~"),
            [
                with(KeyCode::Right, Modifiers::CTRL),
                with(
                    KeyCode::Up,
                    Modifiers {
                        shift: true,
                        ..Modifiers::NONE
                    }
                ),
                with(KeyCode::PageUp, Modifiers::ALT),
            ]
        );
    }

    #[test]
    fn control_letters_enter_tab_and_backspace() {
        assert_eq!(
            all(b"\x03\r\t\x7f\x13"),
            [
                with(KeyCode::Char('c'), Modifiers::CTRL),
                key(KeyCode::Enter),
                key(KeyCode::Tab),
                key(KeyCode::Backspace),
                with(KeyCode::Char('s'), Modifiers::CTRL),
            ]
        );
    }

    #[test]
    fn a_lone_escape_waits_for_the_flush() {
        let mut decoder = InputDecoder::new();
        assert!(decoder.feed(b"\x1b").is_empty());
        assert!(decoder.has_pending());
        assert_eq!(decoder.flush(), [key(KeyCode::Esc)]);
        assert!(!decoder.has_pending());
    }

    #[test]
    fn escape_then_a_letter_is_alt() {
        assert_eq!(
            all(b"\x1bx\x1b\r"),
            [
                with(KeyCode::Char('x'), Modifiers::ALT),
                with(KeyCode::Enter, Modifiers::ALT),
            ]
        );
    }

    #[test]
    fn sequences_split_across_reads_are_joined() {
        let mut decoder = InputDecoder::new();
        assert!(decoder.feed(b"\x1b").is_empty());
        assert!(decoder.feed(b"[").is_empty());
        assert!(decoder.feed(b"1;5").is_empty());
        assert_eq!(decoder.feed(b"B"), [with(KeyCode::Down, Modifiers::CTRL)]);
    }

    #[test]
    fn utf8_characters_split_across_reads_are_joined() {
        let mut decoder = InputDecoder::new();
        let bytes = "日é".as_bytes();
        let mut events = Vec::new();
        for byte in bytes {
            events.extend(decoder.feed(&[*byte]));
        }
        assert_eq!(events, [key(KeyCode::Char('日')), key(KeyCode::Char('é'))]);
    }

    #[test]
    fn invalid_utf8_is_a_replacement_character() {
        assert_eq!(
            all(b"\xff\xc3a"),
            [
                key(KeyCode::Char('\u{fffd}')),
                key(KeyCode::Char('\u{fffd}')),
                key(KeyCode::Char('a')),
            ]
        );
    }

    #[test]
    fn bracketed_paste_is_one_event_and_never_keys() {
        let mut decoder = InputDecoder::new();
        let mut events = decoder.feed(b"\x1b[200~line 1\nq\x1b[2");
        events.extend(decoder.feed(b"01~j"));
        assert_eq!(
            events,
            [
                Event::Paste("line 1\nq".to_owned()),
                key(KeyCode::Char('j'))
            ]
        );
    }

    #[test]
    fn function_keys() {
        assert_eq!(
            all(b"\x1bOP\x1b[15~\x1b[24~"),
            [key(KeyCode::F(1)), key(KeyCode::F(5)), key(KeyCode::F(12))]
        );
    }

    #[test]
    fn unknown_sequences_are_dropped() {
        assert_eq!(all(b"\x1b[?1;2cq"), [key(KeyCode::Char('q'))]);
    }

    #[test]
    fn a_recorded_session_decodes_to_the_expected_events() {
        let log = b"jj\x1b[B\x1b[6~/user:\r\x1b\x1bx\x1b[200~pasted\x1b[201~\x1b[1;5D?q";
        let events = all(log);
        assert_eq!(events.len(), 17);
        assert_eq!(events[0], key(KeyCode::Char('j')));
        assert_eq!(events[2], key(KeyCode::Down));
        assert_eq!(events[3], key(KeyCode::PageDown));
        assert_eq!(events[10], key(KeyCode::Enter));
        assert_eq!(events[11], key(KeyCode::Esc));
        assert_eq!(events[12], with(KeyCode::Char('x'), Modifiers::ALT));
        assert_eq!(events[13], Event::Paste("pasted".to_owned()));
        assert_eq!(events[14], with(KeyCode::Left, Modifiers::CTRL));
        assert_eq!(events[16], key(KeyCode::Char('q')));
    }

    fn next_random(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    #[test]
    fn random_bytes_never_panic_and_split_points_do_not_matter() {
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let alphabet = b"\x1b[O;~0123456789ABCDHFZPQRSu\xc3\xa9\xe6\x97\xa5\xff\x7f\r\tjq";
        for _ in 0..3000 {
            let length = (next_random(&mut state) % 48) as usize;
            let bytes: Vec<u8> = (0..length)
                .map(|_| {
                    let pick = next_random(&mut state);
                    if pick.is_multiple_of(4) {
                        (pick >> 8) as u8
                    } else {
                        alphabet[(pick >> 8) as usize % alphabet.len()]
                    }
                })
                .collect();
            let whole = all(&bytes);
            let mut decoder = InputDecoder::new();
            let mut pieces = Vec::new();
            let mut at = 0;
            while at < bytes.len() {
                let take = 1 + (next_random(&mut state) % 5) as usize;
                let end = (at + take).min(bytes.len());
                pieces.extend(decoder.feed(&bytes[at..end]));
                at = end;
            }
            pieces.extend(decoder.flush());
            assert_eq!(whole, pieces, "input {bytes:?}");
        }
    }
}
