//! A small backtracking regular-expression matcher for SPARQL's `REGEX` and `REPLACE`, over
//! the common subset of XPath's syntax: literal characters and escapes, `.`, classes with
//! ranges and negation, `\d \D \w \W \s \S`, the anchors `^` and `$`, groups `( )` and
//! `(?: )`, alternation `|`, and the quantifiers `* + ? {m} {m,} {m,n}` with their lazy forms.
//! Every match is bounded by a step budget, so a hostile pattern fails instead of hanging.

use std::cell::Cell;

const MAX_PATTERN_CHARS: usize = 4096;
const MAX_REPEAT: usize = 10_000;
const MAX_NESTING: usize = 32;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Node {
    Literal(char),
    Any,
    Class { items: Vec<ClassItem>, negated: bool },
    Start,
    End,
    Group(Vec<Vec<Node>>),
    Repeat { node: Box<Node>, min: usize, max: Option<usize>, greedy: bool },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ClassItem {
    Range(char, char),
    Digit(bool),
    Word(bool),
    Space(bool),
}

fn shorthand(character: char) -> Option<ClassItem> {
    Some(match character {
        'd' => ClassItem::Digit(true),
        'D' => ClassItem::Digit(false),
        'w' => ClassItem::Word(true),
        'W' => ClassItem::Word(false),
        's' => ClassItem::Space(true),
        'S' => ClassItem::Space(false),
        _ => return None,
    })
}

fn escaped(character: char) -> char {
    match character {
        'n' => '\n',
        'r' => '\r',
        't' => '\t',
        other => other,
    }
}

impl ClassItem {
    fn holds(&self, character: char) -> bool {
        match self {
            Self::Range(low, high) => (*low..=*high).contains(&character),
            Self::Digit(wanted) => character.is_ascii_digit() == *wanted,
            Self::Word(wanted) => (character.is_alphanumeric() || character == '_') == *wanted,
            Self::Space(wanted) => character.is_whitespace() == *wanted,
        }
    }
}

struct Reader {
    characters: Vec<char>,
    at: usize,
    depth: usize,
}

impl Reader {
    fn peek(&self) -> Option<char> {
        self.characters.get(self.at).copied()
    }

    fn alternatives(&mut self) -> Option<Vec<Vec<Node>>> {
        let mut branches = vec![self.sequence()?];
        while self.peek() == Some('|') {
            self.at += 1;
            branches.push(self.sequence()?);
        }
        Some(branches)
    }

    fn sequence(&mut self) -> Option<Vec<Node>> {
        let mut nodes = Vec::new();
        while let Some(character) = self.peek() {
            if character == '|' || character == ')' {
                break;
            }
            let atom = self.atom()?;
            nodes.push(self.quantified(atom)?);
        }
        Some(nodes)
    }

    fn number(&mut self) -> Option<usize> {
        let start = self.at;
        while self.peek().is_some_and(|character| character.is_ascii_digit()) {
            self.at += 1;
        }
        let text: String = self.characters.get(start..self.at)?.iter().collect();
        text.parse().ok().filter(|value| *value <= MAX_REPEAT)
    }

    fn quantified(&mut self, atom: Node) -> Option<Node> {
        let (min, max) = match self.peek() {
            Some('*') => {
                self.at += 1;
                (0, None)
            }
            Some('+') => {
                self.at += 1;
                (1, None)
            }
            Some('?') => {
                self.at += 1;
                (0, Some(1))
            }
            Some('{') => {
                self.at += 1;
                let min = self.number()?;
                let max = if self.peek() == Some(',') {
                    self.at += 1;
                    if self.peek() == Some('}') { None } else { Some(self.number()?) }
                } else {
                    Some(min)
                };
                if self.peek() != Some('}') || max.is_some_and(|max| max < min) {
                    return None;
                }
                self.at += 1;
                (min, max)
            }
            _ => return Some(atom),
        };
        if matches!(atom, Node::Start | Node::End) {
            return None;
        }
        let greedy = if self.peek() == Some('?') {
            self.at += 1;
            false
        } else {
            true
        };
        Some(Node::Repeat {
            node: Box::new(atom),
            min,
            max,
            greedy,
        })
    }

    fn class(&mut self) -> Option<Node> {
        let negated = self.peek() == Some('^');
        if negated {
            self.at += 1;
        }
        let mut items = Vec::new();
        let mut first = true;
        loop {
            let character = self.peek()?;
            self.at += 1;
            if character == ']' && !first {
                break;
            }
            first = false;
            let low = if character == '\\' {
                let next = self.peek()?;
                self.at += 1;
                if let Some(item) = shorthand(next) {
                    items.push(item);
                    continue;
                }
                escaped(next)
            } else {
                character
            };
            if self.peek() == Some('-') && self.characters.get(self.at + 1).is_some_and(|next| *next != ']') {
                self.at += 1;
                let mut high = self.peek()?;
                self.at += 1;
                if high == '\\' {
                    high = escaped(self.peek()?);
                    self.at += 1;
                }
                if high < low {
                    return None;
                }
                items.push(ClassItem::Range(low, high));
            } else {
                items.push(ClassItem::Range(low, low));
            }
        }
        Some(Node::Class { items, negated })
    }

    fn atom(&mut self) -> Option<Node> {
        let character = self.peek()?;
        self.at += 1;
        Some(match character {
            '.' => Node::Any,
            '^' => Node::Start,
            '$' => Node::End,
            '[' => return self.class(),
            '(' => {
                self.depth += 1;
                if self.depth > MAX_NESTING {
                    return None;
                }
                if self.peek() == Some('?') {
                    self.at += 1;
                    if self.peek() != Some(':') {
                        return None;
                    }
                    self.at += 1;
                }
                let branches = self.alternatives()?;
                if self.peek() != Some(')') {
                    return None;
                }
                self.at += 1;
                self.depth -= 1;
                Node::Group(branches)
            }
            '\\' => {
                let next = self.peek()?;
                self.at += 1;
                match shorthand(next) {
                    Some(item) => Node::Class {
                        items: vec![item],
                        negated: false,
                    },
                    None => Node::Literal(escaped(next)),
                }
            }
            '*' | '+' | '?' | '{' | ')' => return None,
            other => Node::Literal(other),
        })
    }
}

/// The pattern read, or `None` when it is malformed or too long.
pub(crate) fn parse(pattern: &str) -> Option<Vec<Vec<Node>>> {
    let characters: Vec<char> = pattern.chars().collect();
    if characters.len() > MAX_PATTERN_CHARS {
        return None;
    }
    let mut reader = Reader {
        characters,
        at: 0,
        depth: 0,
    };
    let branches = reader.alternatives()?;
    (reader.at == reader.characters.len()).then_some(branches)
}

type Continuation<'c> = &'c dyn Fn(usize, &mut usize) -> Option<bool>;

fn single(node: &Node, text: &[char], at: usize) -> bool {
    let Some(character) = text.get(at) else {
        return false;
    };
    match node {
        Node::Literal(expected) => character == expected,
        Node::Any => *character != '\n',
        Node::Class { items, negated } => items.iter().any(|item| item.holds(*character)) != *negated,
        _ => false,
    }
}

fn sequence(nodes: &[Node], text: &[char], at: usize, steps: &mut usize, budget: usize, then: Continuation<'_>) -> Option<bool> {
    *steps += 1;
    if *steps > budget {
        return None;
    }
    let Some((first, rest)) = nodes.split_first() else {
        return then(at, steps);
    };
    match first {
        Node::Start => {
            if at == 0 {
                sequence(rest, text, at, steps, budget, then)
            } else {
                Some(false)
            }
        }
        Node::End => {
            if at == text.len() {
                sequence(rest, text, at, steps, budget, then)
            } else {
                Some(false)
            }
        }
        Node::Group(branches) => {
            for branch in branches {
                let after = |end: usize, steps: &mut usize| sequence(rest, text, end, steps, budget, then);
                if sequence(branch, text, at, steps, budget, &after)? {
                    return Some(true);
                }
            }
            Some(false)
        }
        Node::Repeat { node, min, max, greedy } => repeat(node, *min, *max, *greedy, 0, rest, text, at, steps, budget, then),
        single_node => {
            if single(single_node, text, at) {
                sequence(rest, text, at + 1, steps, budget, then)
            } else {
                Some(false)
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn repeat(
    node: &Node,
    min: usize,
    max: Option<usize>,
    greedy: bool,
    count: usize,
    rest: &[Node],
    text: &[char],
    at: usize,
    steps: &mut usize,
    budget: usize,
    then: Continuation<'_>,
) -> Option<bool> {
    let more = |steps: &mut usize| -> Option<bool> {
        if max.is_some_and(|max| count >= max) {
            return Some(false);
        }
        let after = |end: usize, steps: &mut usize| {
            if end == at && count >= min {
                return Some(false);
            }
            repeat(node, min, max, greedy, count + 1, rest, text, end, steps, budget, then)
        };
        sequence(std::slice::from_ref(node), text, at, steps, budget, &after)
    };
    let done = |steps: &mut usize| -> Option<bool> {
        if count >= min {
            sequence(rest, text, at, steps, budget, then)
        } else {
            Some(false)
        }
    };
    if greedy {
        if more(steps)? {
            return Some(true);
        }
        done(steps)
    } else {
        if done(steps)? {
            return Some(true);
        }
        more(steps)
    }
}

/// Whether the pattern matches `text` starting at `start`; `None` when the step budget runs
/// out.
pub(crate) fn matches_at(pattern: &[Vec<Node>], text: &[char], start: usize, steps: &mut usize, budget: usize) -> Option<bool> {
    let group = [Node::Group(pattern.to_vec())];
    let accept = |_: usize, _: &mut usize| Some(true);
    sequence(&group, text, start, steps, budget, &accept)
}

fn match_end(pattern: &[Vec<Node>], text: &[char], start: usize, steps: &mut usize, budget: usize) -> Option<Option<usize>> {
    let group = [Node::Group(pattern.to_vec())];
    let end = Cell::new(None);
    let accept = |at: usize, _: &mut usize| {
        end.set(Some(at));
        Some(true)
    };
    sequence(&group, text, start, steps, budget, &accept)?;
    Some(end.get())
}

/// `text` with every match of `pattern` replaced by `replacement` (where `\$` and `\\` are a
/// literal `$` and `\`; group references are not supported); `None` for a malformed
/// pattern, a group reference, or a pattern that matches the empty string.
pub(crate) fn replace_all(text: &str, pattern: &str, replacement: &str, insensitive: bool) -> Option<String> {
    let mut literal = String::new();
    let mut characters = replacement.chars();
    while let Some(character) = characters.next() {
        match character {
            '\\' => literal.push(characters.next()?),
            '$' => return None,
            other => literal.push(other),
        }
    }
    let folded_pattern = if insensitive { pattern.to_lowercase() } else { pattern.to_owned() };
    let parsed = parse(&folded_pattern)?;
    let original: Vec<char> = text.chars().collect();
    let searched: Vec<char> = if insensitive { text.to_lowercase().chars().collect() } else { original.clone() };
    if searched.len() != original.len() {
        return None;
    }
    let mut steps = 0usize;
    let budget = 1_000_000;
    let mut out = String::new();
    let mut at = 0;
    while at <= searched.len() {
        match match_end(&parsed, &searched, at, &mut steps, budget)? {
            Some(end) if end == at => return None,
            Some(end) => {
                out.push_str(&literal);
                at = end;
            }
            None => {
                if let Some(character) = original.get(at) {
                    out.push(*character);
                }
                at += 1;
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finds(pattern: &str, text: &str) -> Option<bool> {
        let parsed = parse(pattern)?;
        let characters: Vec<char> = text.chars().collect();
        let mut steps = 0;
        for start in 0..=characters.len() {
            if matches_at(&parsed, &characters, start, &mut steps, 100_000)? {
                return Some(true);
            }
        }
        Some(false)
    }

    #[test]
    fn the_common_syntax_matches() {
        assert_eq!(finds("^a.c$", "abc"), Some(true));
        assert_eq!(finds("^a.c$", "abcd"), Some(false));
        assert_eq!(finds("[0-9]{2,3}-\\d+", "call 555-1234"), Some(true));
        assert_eq!(finds("(cat|dog)s?$", "hotdogs"), Some(true));
        assert_eq!(finds("[^a-z]", "abc"), Some(false));
        assert_eq!(finds("a(?:b|c)*d", "abcbd"), Some(true));
        assert_eq!(finds("x{3}", "xx"), Some(false));
    }

    #[test]
    fn bad_and_costly_patterns_fail() {
        assert_eq!(parse("a("), None);
        assert_eq!(parse("*a"), None);
        assert_eq!(parse("[z-a]"), None);
        assert_eq!(finds("(a*)*b", &"a".repeat(40)), None);
    }

    #[test]
    fn replacements_cover_every_match() {
        assert_eq!(replace_all("abcabc", "b", "X", false).as_deref(), Some("aXcaXc"));
        assert_eq!(replace_all("ABC", "b", "-", true).as_deref(), Some("A-C"));
        assert_eq!(replace_all("abc", "x*", "-", false), None);
        assert_eq!(replace_all("abc", "b", "$1", false), None);
    }
}
