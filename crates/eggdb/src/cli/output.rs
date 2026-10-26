use super::failure::{Failure, output_failure};
use std::io::Write;

const FLUSH_AT_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Style {
    Bold,
    Dim,
    Green,
    Red,
    Yellow,
}

impl Style {
    fn sgr(self) -> &'static str {
        match self {
            Self::Bold => "\x1b[1m",
            Self::Dim => "\x1b[2m",
            Self::Green => "\x1b[32m",
            Self::Red => "\x1b[31m",
            Self::Yellow => "\x1b[33m",
        }
    }
}

const RESET: &str = "\x1b[0m";

pub(crate) struct Output<'a> {
    sink: &'a mut dyn Write,
    buffer: Vec<u8>,
    color: bool,
}

impl<'a> Output<'a> {
    pub(crate) fn new(sink: &'a mut dyn Write, color: bool) -> Self {
        Self {
            sink,
            buffer: Vec::new(),
            color,
        }
    }

    pub(crate) fn bytes(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    pub(crate) fn text(&mut self, text: &str) {
        self.bytes(text.as_bytes());
    }

    pub(crate) fn styled(&mut self, style: Style, text: &str) {
        if self.color && !text.is_empty() {
            self.text(style.sgr());
            self.text(text);
            self.text(RESET);
        } else {
            self.text(text);
        }
    }

    pub(crate) fn line_done(&mut self) -> Result<(), Failure> {
        if self.buffer.len() >= FLUSH_AT_BYTES {
            self.write_buffer()?;
        }
        Ok(())
    }

    pub(crate) fn write_through(&mut self, bytes: &[u8]) -> Result<(), Failure> {
        self.write_buffer()?;
        self.sink.write_all(bytes).map_err(output_failure)
    }

    pub(crate) fn sink(&mut self) -> Result<&mut dyn Write, Failure> {
        self.write_buffer()?;
        Ok(&mut *self.sink)
    }

    pub(crate) fn finish(&mut self) -> Result<(), Failure> {
        self.write_buffer()?;
        self.sink.flush().map_err(output_failure)
    }

    fn write_buffer(&mut self) -> Result<(), Failure> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let written = self.sink.write_all(&self.buffer);
        self.buffer.clear();
        written.map_err(output_failure)
    }
}

#[cfg(test)]
mod tests {
    use super::{Output, Style};
    use crate::cli::failure::Failure;
    use std::io::Write;

    #[test]
    fn styles_appear_only_with_colour() {
        let mut plain = Vec::new();
        let mut output = Output::new(&mut plain, false);
        output.styled(Style::Bold, "key");
        output.finish().unwrap();
        assert_eq!(plain, b"key");

        let mut coloured = Vec::new();
        let mut output = Output::new(&mut coloured, true);
        output.styled(Style::Red, "eggdb:");
        output.finish().unwrap();
        assert_eq!(coloured, b"\x1b[31meggdb:\x1b[0m");
    }

    struct ClosedPipe;

    impl Write for ClosedPipe {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_closed_pipe_is_reported_as_such() {
        let mut pipe = ClosedPipe;
        let mut output = Output::new(&mut pipe, false);
        output.text("line\n");
        assert_eq!(output.finish(), Err(Failure::OutputClosed));
    }
}
