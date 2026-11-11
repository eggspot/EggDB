const MAX_CAPTURES: usize = 32;
const MAX_DEPTH: usize = 200;
const SPECIALS: &[u8] = b"^$*+?.([%-";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Capture {
    Text(usize, usize),
    Position(usize),
}

#[derive(Clone, Copy)]
enum Length {
    Open,
    Position,
    Closed(usize),
}

struct Matcher<'a> {
    subject: &'a [u8],
    pattern: &'a [u8],
    depth: usize,
    starts: Vec<usize>,
    lengths: Vec<Length>,
}

fn class_matches(class: u8, byte: u8) -> bool {
    let matched = match class.to_ascii_lowercase() {
        b'a' => byte.is_ascii_alphabetic(),
        b'd' => byte.is_ascii_digit(),
        b'l' => byte.is_ascii_lowercase(),
        b's' => byte.is_ascii_whitespace() || byte == 0x0b,
        b'u' => byte.is_ascii_uppercase(),
        b'w' => byte.is_ascii_alphanumeric(),
        b'x' => byte.is_ascii_hexdigit(),
        b'p' => byte.is_ascii_punctuation(),
        b'c' => byte.is_ascii_control(),
        b'g' => byte.is_ascii_graphic(),
        b'z' => byte == 0,
        _ => return class == byte,
    };
    if class.is_ascii_uppercase() { !matched } else { matched }
}

impl<'a> Matcher<'a> {
    fn class_end(&self, at: usize) -> Result<usize, String> {
        let pattern = self.pattern;
        let mut at = at;
        let first = pattern[at];
        at += 1;
        if first == b'%' {
            if at >= pattern.len() {
                return Err("malformed pattern (ends with '%')".to_owned());
            }
            return Ok(at + 1);
        }
        if first == b'[' {
            if pattern.get(at) == Some(&b'^') {
                at += 1;
            }
            loop {
                let Some(&byte) = pattern.get(at) else {
                    return Err("malformed pattern (missing ']')".to_owned());
                };
                at += 1;
                if byte == b'%' {
                    if at >= pattern.len() {
                        return Err("malformed pattern (missing ']')".to_owned());
                    }
                    at += 1;
                }
                match pattern.get(at) {
                    Some(b']') => return Ok(at + 1),
                    None => return Err("malformed pattern (missing ']')".to_owned()),
                    Some(_) => {}
                }
            }
        }
        Ok(at)
    }

    fn set_matches(&self, from: usize, end: usize, byte: u8) -> bool {
        let pattern = self.pattern;
        let last = end - 1;
        let mut at = from;
        let mut found = true;
        if pattern.get(at + 1) == Some(&b'^') {
            found = false;
            at += 1;
        }
        at += 1;
        while at < last {
            if pattern[at] == b'%' {
                at += 1;
                if class_matches(pattern[at], byte) {
                    return found;
                }
            } else if pattern.get(at + 1) == Some(&b'-') && at + 2 < last {
                if pattern[at] <= byte && byte <= pattern[at + 2] {
                    return found;
                }
                at += 2;
            } else if pattern[at] == byte {
                return found;
            }
            at += 1;
        }
        !found
    }

    fn single(&self, position: usize, from: usize, end: usize) -> bool {
        let Some(&byte) = self.subject.get(position) else {
            return false;
        };
        match self.pattern[from] {
            b'.' => true,
            b'%' => class_matches(self.pattern[from + 1], byte),
            b'[' => self.set_matches(from, end, byte),
            literal => literal == byte,
        }
    }

    fn capture_text(&self, index: usize) -> Result<&'a [u8], String> {
        match self.lengths.get(index) {
            Some(Length::Closed(length)) => Ok(&self.subject[self.starts[index]..self.starts[index] + length]),
            _ => Err(format!("invalid capture index %{}", index + 1)),
        }
    }

    fn do_match(&mut self, position: usize, at: usize) -> Result<Option<usize>, String> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err("pattern too complex".to_owned());
        }
        let outcome = self.match_here(position, at);
        self.depth -= 1;
        outcome
    }

    fn match_here(&mut self, mut position: usize, mut at: usize) -> Result<Option<usize>, String> {
        loop {
            if at >= self.pattern.len() {
                return Ok(Some(position));
            }
            match self.pattern[at] {
                b'(' => {
                    let kind = if self.pattern.get(at + 1) == Some(&b')') {
                        (Length::Position, at + 2)
                    } else {
                        (Length::Open, at + 1)
                    };
                    if self.starts.len() >= MAX_CAPTURES {
                        return Err("too many captures".to_owned());
                    }
                    self.starts.push(position);
                    self.lengths.push(kind.0);
                    let found = self.do_match(position, kind.1)?;
                    if found.is_none() {
                        self.starts.pop();
                        self.lengths.pop();
                    }
                    return Ok(found);
                }
                b')' => {
                    let Some(open) = self.lengths.iter().rposition(|length| matches!(length, Length::Open)) else {
                        return Err("invalid pattern capture".to_owned());
                    };
                    self.lengths[open] = Length::Closed(position - self.starts[open]);
                    let found = self.do_match(position, at + 1)?;
                    if found.is_none() {
                        self.lengths[open] = Length::Open;
                    }
                    return Ok(found);
                }
                b'$' if at + 1 == self.pattern.len() => {
                    return Ok((position == self.subject.len()).then_some(position));
                }
                b'%' if self.pattern.get(at + 1) == Some(&b'b') => {
                    let (Some(&open), Some(&close)) = (self.pattern.get(at + 2), self.pattern.get(at + 3)) else {
                        return Err("missing arguments to '%b'".to_owned());
                    };
                    if self.subject.get(position) != Some(&open) {
                        return Ok(None);
                    }
                    let mut level = 1;
                    let mut scan = position + 1;
                    loop {
                        let Some(&byte) = self.subject.get(scan) else {
                            return Ok(None);
                        };
                        if byte == close {
                            level -= 1;
                            if level == 0 {
                                break;
                            }
                        } else if byte == open {
                            level += 1;
                        }
                        scan += 1;
                    }
                    position = scan + 1;
                    at += 4;
                    continue;
                }
                b'%' if self.pattern.get(at + 1) == Some(&b'f') => {
                    at += 2;
                    if self.pattern.get(at) != Some(&b'[') {
                        return Err("missing '[' after '%f' in pattern".to_owned());
                    }
                    let end = self.class_end(at)?;
                    let previous = if position == 0 { 0 } else { self.subject[position - 1] };
                    let current = self.subject.get(position).copied().unwrap_or(0);
                    if self.set_matches(at, end, previous) || !self.set_matches(at, end, current) {
                        return Ok(None);
                    }
                    at = end;
                    continue;
                }
                b'%' if self.pattern.get(at + 1).is_some_and(u8::is_ascii_digit) => {
                    let index = (self.pattern[at + 1] - b'1') as usize;
                    let text = self.capture_text(index)?;
                    if !self.subject[position..].starts_with(text) {
                        return Ok(None);
                    }
                    position += text.len();
                    at += 2;
                    continue;
                }
                _ => {}
            }
            let end = self.class_end(at)?;
            let matches_here = self.single(position, at, end);
            match self.pattern.get(end) {
                Some(b'?') => {
                    if matches_here && let Some(found) = self.do_match(position + 1, end + 1)? {
                        return Ok(Some(found));
                    }
                    at = end + 1;
                }
                Some(b'+') => {
                    return if matches_here { self.expand(position + 1, at, end) } else { Ok(None) };
                }
                Some(b'*') => return self.expand(position, at, end),
                Some(b'-') => return self.shortest(position, at, end),
                _ => {
                    if !matches_here {
                        return Ok(None);
                    }
                    position += 1;
                    at = end;
                }
            }
        }
    }

    fn expand(&mut self, position: usize, at: usize, end: usize) -> Result<Option<usize>, String> {
        let mut count = 0;
        while self.single(position + count, at, end) {
            count += 1;
        }
        loop {
            if let Some(found) = self.do_match(position + count, end + 1)? {
                return Ok(Some(found));
            }
            if count == 0 {
                return Ok(None);
            }
            count -= 1;
        }
    }

    fn shortest(&mut self, mut position: usize, at: usize, end: usize) -> Result<Option<usize>, String> {
        loop {
            if let Some(found) = self.do_match(position, end + 1)? {
                return Ok(Some(found));
            }
            if self.single(position, at, end) {
                position += 1;
            } else {
                return Ok(None);
            }
        }
    }

    fn captures(&self, start: usize, end: usize) -> Vec<Capture> {
        if self.starts.is_empty() {
            return vec![Capture::Text(start, end)];
        }
        self.starts
            .iter()
            .zip(&self.lengths)
            .map(|(&from, length)| match length {
                Length::Position => Capture::Position(from + 1),
                Length::Closed(length) => Capture::Text(from, from + length),
                Length::Open => Capture::Text(from, from),
            })
            .collect()
    }
}

/// A match of a Lua pattern: where the whole match lies, and its captures (the whole match when the
/// pattern has none).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Found {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) captures: Vec<Capture>,
    pub(crate) explicit_captures: bool,
}

/// Matches the unanchored pattern body `pattern` at exactly byte `position` of `subject`.
pub(crate) fn match_at(subject: &[u8], pattern: &[u8], position: usize) -> Result<Option<Found>, String> {
    let mut matcher = Matcher {
        subject,
        pattern,
        depth: 0,
        starts: Vec::new(),
        lengths: Vec::new(),
    };
    Ok(matcher.do_match(position, 0)?.map(|end| Found {
        start: position,
        end,
        explicit_captures: !matcher.starts.is_empty(),
        captures: matcher.captures(position, end),
    }))
}

/// Splits a leading `^` off `pattern`: whether it is anchored, and the rest.
pub(crate) fn anchored(pattern: &[u8]) -> (bool, &[u8]) {
    match pattern.split_first() {
        Some((b'^', rest)) => (true, rest),
        _ => (false, pattern),
    }
}

/// Looks for `pattern` in `subject` starting at byte `init`, as Lua 5.1's `string.find` does.
pub(crate) fn find(subject: &[u8], pattern: &[u8], init: usize) -> Result<Option<Found>, String> {
    let (anchored, body) = anchored(pattern);
    let mut start = init;
    while start <= subject.len() {
        if let Some(found) = match_at(subject, body, start)? {
            return Ok(Some(found));
        }
        if anchored {
            break;
        }
        start += 1;
    }
    Ok(None)
}

/// Whether `pattern` has no magic characters, so a plain search finds the same thing.
pub(crate) fn is_plain(pattern: &[u8]) -> bool {
    !pattern.iter().any(|byte| SPECIALS.contains(byte))
}
