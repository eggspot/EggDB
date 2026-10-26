use crate::decoder::InputDecoder;
use crate::frame::Frame;
use crate::render::Renderer;
use crate::terminal::{Capabilities, Colors, Event, Size, Terminal};
use eggdb_platform::terminal::{Input, RawTerminal, wants_unicode};
use std::collections::VecDeque;
use std::ffi::OsStr;
use std::io;
use std::time::{Duration, Instant};

const ESCAPE_WAIT: Duration = Duration::from_millis(25);
const SILENCE_THAT_ENDS_A_PASTE: Duration = Duration::from_secs(1);

/// The real terminal behind standard input and output, through the platform layer's
/// [`RawTerminal`]: raw mode while it lives, input decoded into events, frames drawn as the
/// difference from the last one.
#[derive(Debug)]
pub struct OsTerminal {
    raw: RawTerminal,
    decoder: InputDecoder,
    queue: VecDeque<Event>,
    renderer: Renderer,
    capabilities: Capabilities,
    size: Size,
    output: Vec<u8>,
    last_input: Instant,
}

/// What `--mono` and `--ascii` ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Fallbacks {
    /// No colours, attributes only.
    pub mono: bool,
    /// ASCII only, no box lines or arrows.
    pub ascii: bool,
}

/// What the terminal can show given the environment (`NO_COLOR`, `TERM`, the locale) and the
/// `--mono` and `--ascii` options. `TERM=dumb`, or `--mono` with `--ascii`, is the plain build:
/// no colours, no attributes, ASCII only.
pub fn detect_capabilities(fallbacks: Fallbacks) -> Capabilities {
    capabilities_from(
        fallbacks,
        std::env::var_os("NO_COLOR").as_deref(),
        std::env::var_os("TERM").as_deref(),
        wants_unicode(),
    )
}

fn capabilities_from(
    fallbacks: Fallbacks,
    no_color: Option<&OsStr>,
    term: Option<&OsStr>,
    unicode: bool,
) -> Capabilities {
    let dumb = term == Some(OsStr::new("dumb"));
    let plain = dumb || (fallbacks.mono && fallbacks.ascii);
    let no_color = no_color.is_some_and(|value| !value.is_empty());
    Capabilities {
        colors: if plain || no_color || fallbacks.mono {
            Colors::None
        } else {
            Colors::Ansi16
        },
        attributes: !plain,
        unicode: unicode && !plain && !fallbacks.ascii,
    }
}

impl OsTerminal {
    /// Takes the terminal. Fails with [`io::ErrorKind::Unsupported`] when standard input and
    /// output are not both terminals.
    pub fn open(capabilities: Capabilities) -> io::Result<Self> {
        let raw = RawTerminal::enter()?;
        let size = raw.size()?;
        Ok(Self {
            raw,
            decoder: InputDecoder::new(),
            queue: VecDeque::new(),
            renderer: Renderer::new(capabilities.attributes),
            capabilities,
            size: Size {
                cols: size.cols,
                rows: size.rows,
            },
            output: Vec::new(),
            last_input: Instant::now(),
        })
    }
}

impl Terminal for OsTerminal {
    fn size(&self) -> Size {
        self.size
    }

    fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    fn next_event(&mut self, timeout: Duration) -> io::Result<Option<Event>> {
        let deadline = Instant::now().checked_add(timeout);
        let mut buffer = [0u8; 4096];
        loop {
            if let Some(event) = self.queue.pop_front() {
                return Ok(Some(event));
            }
            let left = deadline.map_or(timeout, |deadline| {
                deadline.saturating_duration_since(Instant::now())
            });
            let silence_left = SILENCE_THAT_ENDS_A_PASTE.saturating_sub(self.last_input.elapsed());
            let wait = if self.decoder.has_pending() {
                ESCAPE_WAIT
            } else if self.decoder.in_paste() {
                left.min(silence_left)
            } else {
                left
            };
            match self.raw.read_input(&mut buffer, wait)? {
                Input::Bytes(count) => {
                    self.last_input = Instant::now();
                    let events = self.decoder.feed(&buffer[..count.min(buffer.len())]);
                    self.queue.extend(events);
                }
                Input::Resized => {
                    let size = self.raw.size()?;
                    self.size = Size {
                        cols: size.cols,
                        rows: size.rows,
                    };
                    self.renderer.reset();
                    return Ok(Some(Event::Resize(self.size)));
                }
                Input::Closed => return Ok(Some(Event::Interrupt)),
                Input::TimedOut if self.decoder.has_pending() => {
                    let events = self.decoder.flush();
                    self.queue.extend(events);
                }
                Input::TimedOut
                    if self.decoder.in_paste()
                        && self.last_input.elapsed() >= SILENCE_THAT_ENDS_A_PASTE =>
                {
                    let events = self.decoder.end_paste();
                    self.queue.extend(events);
                }
                Input::TimedOut => return Ok(None),
            }
            if self.queue.is_empty() && left.is_zero() && !self.decoder.has_pending() {
                return Ok(None);
            }
        }
    }

    fn present(&mut self, frame: &Frame) -> io::Result<()> {
        self.output.clear();
        self.renderer.render(frame, &mut self.output);
        self.raw.write_all(&self.output)
    }
}

#[cfg(test)]
mod tests {
    use super::{Fallbacks, capabilities_from};
    use crate::terminal::{Capabilities, Colors};
    use std::ffi::OsStr;

    #[test]
    fn a_capable_terminal_gets_everything() {
        assert_eq!(
            capabilities_from(Fallbacks::default(), None, Some(OsStr::new("xterm")), true),
            Capabilities::FULL
        );
    }

    #[test]
    fn no_color_and_mono_keep_attributes() {
        for (fallbacks, no_color) in [
            (Fallbacks::default(), Some(OsStr::new("1"))),
            (
                Fallbacks {
                    mono: true,
                    ascii: false,
                },
                None,
            ),
        ] {
            let found = capabilities_from(fallbacks, no_color, None, true);
            assert_eq!(found.colors, Colors::None);
            assert!(found.attributes);
            assert!(found.unicode);
        }
    }

    #[test]
    fn a_dumb_terminal_and_mono_ascii_are_the_plain_build() {
        let plain = Capabilities {
            colors: Colors::None,
            attributes: false,
            unicode: false,
        };
        assert_eq!(
            capabilities_from(Fallbacks::default(), None, Some(OsStr::new("dumb")), true),
            plain
        );
        assert_eq!(
            capabilities_from(
                Fallbacks {
                    mono: true,
                    ascii: true
                },
                None,
                None,
                true
            ),
            plain
        );
    }

    #[test]
    fn ascii_alone_keeps_colours() {
        let found = capabilities_from(
            Fallbacks {
                mono: false,
                ascii: true,
            },
            None,
            None,
            true,
        );
        assert_eq!(found.colors, Colors::Ansi16);
        assert!(!found.unicode);
    }
}
