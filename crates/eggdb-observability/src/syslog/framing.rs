const MOST_LENGTH_DIGITS: usize = 9;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FramingError {
    BadLength,
    TooLong,
}

#[derive(Debug)]
pub(crate) struct Framer {
    buffer: Vec<u8>,
    start: usize,
    searched_to: usize,
    most: usize,
}

impl Framer {
    pub(crate) fn new(most: usize) -> Self {
        Self {
            buffer: Vec::new(),
            start: 0,
            searched_to: 0,
            most,
        }
    }

    pub(crate) fn push(&mut self, bytes: &[u8]) {
        if self.start > 0 {
            let dropped = self.start.min(self.buffer.len());
            self.buffer.drain(..dropped);
            self.searched_to = self.searched_to.saturating_sub(dropped);
            self.start = 0;
        }
        self.buffer.extend_from_slice(bytes);
    }

    fn pending(&self) -> &[u8] {
        self.buffer.get(self.start..).unwrap_or_default()
    }

    fn skip_separators(&mut self) {
        let skipped = self
            .pending()
            .iter()
            .take_while(|byte| matches!(byte, b'\n' | b'\r' | b'\0'))
            .count();
        self.start += skipped;
        self.searched_to = self.searched_to.max(self.start);
    }

    pub(crate) fn next_frame(&mut self) -> Result<Option<Vec<u8>>, FramingError> {
        self.skip_separators();
        let line_end = match self.pending().first() {
            None => return Ok(None),
            Some(b'1'..=b'9') => None,
            Some(_) => Some(self.line_end()),
        };
        let pending = self.pending();
        let (frame, consumed) = match line_end {
            None => match octet_counted(pending, self.most)? {
                Some(found) => found,
                None => return Ok(None),
            },
            Some(Some(end)) if end <= self.most => {
                let line = pending.get(..end).unwrap_or_default();
                (line.strip_suffix(b"\r").unwrap_or(line), end + 1)
            }
            Some(Some(_)) => return Err(FramingError::TooLong),
            Some(None) if pending.len() > self.most => return Err(FramingError::TooLong),
            Some(None) => return Ok(None),
        };
        let frame = frame.to_vec();
        self.start += consumed;
        self.searched_to = self.start;
        Ok(Some(frame))
    }

    fn line_end(&mut self) -> Option<usize> {
        let searched = self.searched_to.saturating_sub(self.start);
        let unsearched = self.pending().get(searched..).unwrap_or_default();
        match unsearched
            .iter()
            .position(|byte| matches!(byte, b'\n' | b'\0'))
        {
            Some(at) => Some(searched + at),
            None => {
                self.searched_to = self.buffer.len();
                None
            }
        }
    }

    pub(crate) fn finish(mut self) -> Option<Vec<u8>> {
        self.skip_separators();
        let rest = self.pending();
        let counted = matches!(rest.first(), Some(b'1'..=b'9'));
        (!rest.is_empty() && !counted && rest.len() <= self.most).then(|| rest.to_vec())
    }
}

fn octet_counted(pending: &[u8], most: usize) -> Result<Option<(&[u8], usize)>, FramingError> {
    let digits = pending
        .iter()
        .take(MOST_LENGTH_DIGITS + 1)
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    match pending.get(digits) {
        None if digits <= MOST_LENGTH_DIGITS => return Ok(None),
        Some(b' ') => {}
        _ => return Err(FramingError::BadLength),
    }
    let length = pending
        .get(..digits)
        .unwrap_or_default()
        .iter()
        .try_fold(0_usize, |length, &digit| {
            length
                .checked_mul(10)?
                .checked_add(usize::from(digit - b'0'))
        })
        .ok_or(FramingError::BadLength)?;
    if length > most {
        return Err(FramingError::TooLong);
    }
    let from = digits + 1;
    let to = from + length;
    Ok(pending.get(from..to).map(|frame| (frame, to)))
}
