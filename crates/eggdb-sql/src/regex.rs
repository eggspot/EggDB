use crate::ast::BinaryOperator;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::Value;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

pub(crate) const MAX_PATTERN_BYTES: usize = 64 * 1024;
pub(crate) const MAX_INSTRUCTIONS: usize = 100_000;
pub(crate) const MAX_REPEAT: u32 = 255;
pub(crate) const MAX_GROUP_NESTING: usize = 256;
pub(crate) const MATCH_STEPS: u64 = 1_000_000_000;
const MOST_CACHED_PATTERNS: usize = 256;
const MOST_CACHED_PATTERN_BYTES: usize = 1 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum CaseSensitivity {
    Sensitive,
    Insensitive,
}

#[derive(Debug)]
pub(crate) struct StepBudget {
    left: u64,
}

impl StepBudget {
    pub(crate) fn new(steps: u64) -> Self {
        Self { left: steps }
    }

    fn spend(&mut self, steps: u64) -> SqlResult<()> {
        match self.left.checked_sub(steps) {
            Some(left) => {
                self.left = left;
                Ok(())
            }
            None => {
                self.left = 0;
                Err(SqlError::new(
                    SqlState::PROGRAM_LIMIT_EXCEEDED,
                    "regular expression match is too complex",
                )
                .with_hint(format!(
                    "One match may take at most {MATCH_STEPS} regular expression steps; simplify the pattern or match a shorter text."
                )))
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NamedClass {
    Alpha,
    Digit,
    Alnum,
    Upper,
    Lower,
    Space,
    Punct,
    Xdigit,
    Blank,
    Cntrl,
    Graph,
    Print,
    Word,
}

impl NamedClass {
    fn named(name: &str) -> Option<Self> {
        Some(match name {
            "alpha" => Self::Alpha,
            "digit" => Self::Digit,
            "alnum" => Self::Alnum,
            "upper" => Self::Upper,
            "lower" => Self::Lower,
            "space" => Self::Space,
            "punct" => Self::Punct,
            "xdigit" => Self::Xdigit,
            "blank" => Self::Blank,
            "cntrl" => Self::Cntrl,
            "graph" => Self::Graph,
            "print" => Self::Print,
            "word" => Self::Word,
            _ => return None,
        })
    }

    fn contains(self, character: char) -> bool {
        if !character.is_ascii() {
            return false;
        }
        match self {
            Self::Alpha => character.is_ascii_alphabetic(),
            Self::Digit => character.is_ascii_digit(),
            Self::Alnum => character.is_ascii_alphanumeric(),
            Self::Upper => character.is_ascii_uppercase(),
            Self::Lower => character.is_ascii_lowercase(),
            Self::Space => matches!(character, ' ' | '\t' | '\n' | '\r' | '\u{b}' | '\u{c}'),
            Self::Punct => character.is_ascii_punctuation(),
            Self::Xdigit => character.is_ascii_hexdigit(),
            Self::Blank => matches!(character, ' ' | '\t'),
            Self::Cntrl => character.is_ascii_control(),
            Self::Graph => character.is_ascii_graphic(),
            Self::Print => character.is_ascii_graphic() || character == ' ',
            Self::Word => character.is_ascii_alphanumeric() || character == '_',
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClassItem {
    Range(char, char),
    Named(NamedClass),
    Excluded(NamedClass),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CharClass {
    items: Vec<ClassItem>,
    negated: bool,
}

impl CharClass {
    fn of(named: NamedClass, negated: bool) -> Self {
        Self {
            items: vec![ClassItem::Named(named)],
            negated,
        }
    }

    fn holds(&self, character: char) -> bool {
        self.items.iter().any(|item| match *item {
            ClassItem::Range(low, high) => low <= character && character <= high,
            ClassItem::Named(named) => named.contains(character),
            ClassItem::Excluded(named) => !named.contains(character),
        })
    }

    fn matches(&self, character: char, case: CaseSensitivity) -> bool {
        let found = self.holds(character)
            || (case == CaseSensitivity::Insensitive
                && character.is_ascii_alphabetic()
                && (self.holds(character.to_ascii_lowercase())
                    || self.holds(character.to_ascii_uppercase())));
        found != self.negated
    }
}

#[derive(Debug)]
enum Node {
    Empty,
    Literal(char),
    Any,
    Class(usize),
    Start,
    End,
    Concat(Vec<Node>),
    Alternate(Vec<Node>),
    Repeat {
        node: Box<Node>,
        min: u32,
        max: Option<u32>,
    },
}

impl Node {
    fn size(&self) -> u64 {
        match self {
            Self::Empty => 0,
            Self::Literal(_) | Self::Any | Self::Class(_) | Self::Start | Self::End => 1,
            Self::Concat(nodes) => nodes.iter().map(Node::size).fold(0, u64::saturating_add),
            Self::Alternate(nodes) => nodes
                .iter()
                .map(Node::size)
                .fold(0, u64::saturating_add)
                .saturating_add(2 * (nodes.len() as u64).saturating_sub(1)),
            Self::Repeat { node, min, max } => {
                let inner = node.size();
                let required = inner.saturating_mul(u64::from(*min));
                let optional = match max {
                    None => inner.saturating_add(2),
                    Some(max) => {
                        u64::from(max.saturating_sub(*min)).saturating_mul(inner.saturating_add(1))
                    }
                };
                required.saturating_add(optional)
            }
        }
    }

    fn anchored(&self) -> bool {
        match self {
            Self::Start => true,
            Self::Concat(nodes) => nodes.first().is_some_and(Node::anchored),
            Self::Alternate(nodes) => !nodes.is_empty() && nodes.iter().all(Node::anchored),
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Instruction {
    Literal(char),
    Any,
    Class(u32),
    Start,
    End,
    Split(u32, u32),
    Jump(u32),
    Match,
}

#[derive(Debug)]
pub(crate) struct Regex {
    program: Vec<Instruction>,
    classes: Vec<CharClass>,
    case: CaseSensitivity,
    anchored: bool,
}

fn invalid(reason: &str) -> SqlError {
    SqlError::new(
        SqlState::INVALID_REGULAR_EXPRESSION,
        format!("invalid regular expression: {reason}"),
    )
}

fn unsupported(what: &str) -> SqlError {
    SqlError::not_supported(format!(
        "{what} in regular expressions are not supported yet"
    ))
}

fn too_complex() -> SqlError {
    invalid("regular expression is too complex")
}

struct PatternParser {
    chars: Vec<char>,
    at: usize,
    depth: usize,
    case: CaseSensitivity,
    classes: Vec<CharClass>,
}

impl PatternParser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.at).copied()
    }

    fn peek_at(&self, ahead: usize) -> Option<char> {
        self.chars.get(self.at + ahead).copied()
    }

    fn take(&mut self, character: char) -> bool {
        if self.peek() == Some(character) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn literal(&self, character: char) -> Node {
        Node::Literal(match self.case {
            CaseSensitivity::Sensitive => character,
            CaseSensitivity::Insensitive => character.to_ascii_lowercase(),
        })
    }

    fn class(&mut self, class: CharClass) -> SqlResult<Node> {
        if self.classes.len() >= MAX_INSTRUCTIONS {
            return Err(too_complex());
        }
        self.classes.push(class);
        Ok(Node::Class(self.classes.len() - 1))
    }

    fn alternation(&mut self) -> SqlResult<Node> {
        let mut branches = vec![self.branch()?];
        while self.take('|') {
            branches.push(self.branch()?);
        }
        Ok(if branches.len() == 1 {
            branches.pop().unwrap_or(Node::Empty)
        } else {
            Node::Alternate(branches)
        })
    }

    fn branch(&mut self) -> SqlResult<Node> {
        let mut pieces = Vec::new();
        while let Some(character) = self.peek() {
            if character == '|' || character == ')' {
                break;
            }
            pieces.push(self.piece()?);
        }
        Ok(match pieces.len() {
            0 => Node::Empty,
            1 => pieces.pop().unwrap_or(Node::Empty),
            _ => Node::Concat(pieces),
        })
    }

    fn at_quantifier(&self) -> bool {
        match self.peek() {
            Some('*' | '+' | '?') => true,
            Some('{') => self.peek_at(1).is_some_and(|next| next.is_ascii_digit()),
            _ => false,
        }
    }

    fn piece(&mut self) -> SqlResult<Node> {
        if self.at_quantifier() {
            return Err(invalid("quantifier operand invalid"));
        }
        let atom = self.atom()?;
        if !self.at_quantifier() {
            return Ok(atom);
        }
        if matches!(atom, Node::Start | Node::End) {
            return Err(invalid("quantifier operand invalid"));
        }
        let (min, max) = self.quantifier()?;
        self.take('?');
        if self.at_quantifier() {
            return Err(invalid("quantifier operand invalid"));
        }
        Ok(Node::Repeat {
            node: Box::new(atom),
            min,
            max,
        })
    }

    fn quantifier(&mut self) -> SqlResult<(u32, Option<u32>)> {
        let Some(character) = self.peek() else {
            return Err(invalid("quantifier operand invalid"));
        };
        self.at += 1;
        Ok(match character {
            '*' => (0, None),
            '+' => (1, None),
            '?' => (0, Some(1)),
            _ => {
                let min = self.bound()?;
                let max = if self.take(',') {
                    if self.peek().is_some_and(|next| next.is_ascii_digit()) {
                        Some(self.bound()?)
                    } else {
                        None
                    }
                } else {
                    Some(min)
                };
                if !self.take('}') {
                    return Err(match self.peek() {
                        None => invalid("braces {} not balanced"),
                        Some(_) => invalid("invalid repetition count(s)"),
                    });
                }
                if max.is_some_and(|max| max < min) {
                    return Err(invalid("invalid repetition count(s)"));
                }
                (min, max)
            }
        })
    }

    fn bound(&mut self) -> SqlResult<u32> {
        let mut value: u32 = 0;
        let mut digits = 0;
        while let Some(digit) = self.peek().and_then(|character| character.to_digit(10)) {
            self.at += 1;
            digits += 1;
            value = value.saturating_mul(10).saturating_add(digit);
        }
        if digits == 0 {
            return Err(match self.peek() {
                None => invalid("braces {} not balanced"),
                Some(_) => invalid("invalid repetition count(s)"),
            });
        }
        if value > MAX_REPEAT {
            return Err(invalid("invalid repetition count(s)"));
        }
        Ok(value)
    }

    fn atom(&mut self) -> SqlResult<Node> {
        let Some(character) = self.peek() else {
            return Ok(Node::Empty);
        };
        self.at += 1;
        match character {
            '(' => self.group(),
            '.' => Ok(Node::Any),
            '^' => Ok(Node::Start),
            '$' => Ok(Node::End),
            '[' => self.bracket(),
            '\\' => self.escape(),
            other => Ok(self.literal(other)),
        }
    }

    fn group(&mut self) -> SqlResult<Node> {
        if self.peek() == Some('?') {
            match self.peek_at(1) {
                Some(':') => self.at += 2,
                Some('#') => {
                    while let Some(character) = self.peek() {
                        self.at += 1;
                        if character == ')' {
                            return Ok(Node::Empty);
                        }
                    }
                    return Err(invalid("parentheses () not balanced"));
                }
                Some('=' | '!' | '<') => {
                    return Err(unsupported("lookahead and lookbehind constraints"));
                }
                Some(letter) if letter.is_ascii_alphabetic() => {
                    return Err(unsupported("embedded options"));
                }
                _ => return Err(invalid("quantifier operand invalid")),
            }
        }
        self.depth += 1;
        if self.depth > MAX_GROUP_NESTING {
            return Err(too_complex());
        }
        let inner = self.alternation()?;
        self.depth -= 1;
        if !self.take(')') {
            return Err(invalid("parentheses () not balanced"));
        }
        Ok(inner)
    }

    fn escape(&mut self) -> SqlResult<Node> {
        let Some(character) = self.peek() else {
            return Err(invalid("invalid escape \\ sequence"));
        };
        self.at += 1;
        let class = |named, negated| CharClass::of(named, negated);
        match character {
            'd' => self.class(class(NamedClass::Digit, false)),
            'D' => self.class(class(NamedClass::Digit, true)),
            'w' => self.class(class(NamedClass::Word, false)),
            'W' => self.class(class(NamedClass::Word, true)),
            's' => self.class(class(NamedClass::Space, false)),
            'S' => self.class(class(NamedClass::Space, true)),
            'm' | 'M' | 'y' | 'Y' | 'A' | 'Z' => Err(unsupported("constraint escapes")),
            '1'..='9' => Err(unsupported("back references")),
            _ => {
                let literal = self.escaped_character(character)?;
                Ok(self.literal(literal))
            }
        }
    }

    fn escaped_character(&mut self, character: char) -> SqlResult<char> {
        let control =
            |code: u32| char::from_u32(code).ok_or_else(|| invalid("invalid escape \\ sequence"));
        match character {
            'a' => Ok('\u{7}'),
            'b' => Ok('\u{8}'),
            'B' => Ok('\\'),
            'e' => Ok('\u{1b}'),
            'f' => Ok('\u{c}'),
            'n' => Ok('\n'),
            'r' => Ok('\r'),
            't' => Ok('\t'),
            'v' => Ok('\u{b}'),
            '0' => Ok('\0'),
            'c' => {
                let Some(next) = self.peek() else {
                    return Err(invalid("invalid escape \\ sequence"));
                };
                self.at += 1;
                control(u32::from(next) & 0x1F)
            }
            'x' => {
                let code = self.hex_digits(1, 8)?;
                control(code)
            }
            'u' => {
                let code = self.hex_digits(4, 4)?;
                control(code)
            }
            'U' => {
                let code = self.hex_digits(8, 8)?;
                control(code)
            }
            other if other.is_ascii_alphanumeric() => Err(invalid("invalid escape \\ sequence")),
            other => Ok(other),
        }
    }

    fn hex_digits(&mut self, least: usize, most: usize) -> SqlResult<u32> {
        let mut code: u32 = 0;
        let mut count = 0;
        while count < most {
            let Some(digit) = self.peek().and_then(|character| character.to_digit(16)) else {
                break;
            };
            self.at += 1;
            count += 1;
            code = code.saturating_mul(16).saturating_add(digit);
        }
        if count < least {
            return Err(invalid("invalid escape \\ sequence"));
        }
        Ok(code)
    }

    fn bracket(&mut self) -> SqlResult<Node> {
        let negated = self.take('^');
        let mut items = Vec::new();
        let mut first = true;
        loop {
            let Some(character) = self.peek() else {
                return Err(invalid("brackets [] not balanced"));
            };
            if character == ']' && !first {
                self.at += 1;
                break;
            }
            first = false;
            match self.bracket_element()? {
                BracketElement::Class(item) => {
                    if self.peek() == Some('-') && self.peek_at(1).is_some_and(|next| next != ']') {
                        return Err(invalid("invalid character range"));
                    }
                    items.push(item);
                }
                BracketElement::Character(low) => {
                    if self.peek() == Some('-') && self.peek_at(1).is_some_and(|next| next != ']') {
                        self.at += 1;
                        let BracketElement::Character(high) = self.bracket_element()? else {
                            return Err(invalid("invalid character range"));
                        };
                        if high < low {
                            return Err(invalid("invalid character range"));
                        }
                        items.push(ClassItem::Range(low, high));
                    } else {
                        items.push(ClassItem::Range(low, low));
                    }
                }
            }
        }
        self.class(CharClass { items, negated })
    }

    fn bracket_element(&mut self) -> SqlResult<BracketElement> {
        let Some(character) = self.peek() else {
            return Err(invalid("brackets [] not balanced"));
        };
        self.at += 1;
        if character == '[' {
            match self.peek() {
                Some(':') => {
                    self.at += 1;
                    let name = self.until_closing(':', "invalid character class")?;
                    return NamedClass::named(&name)
                        .map(|named| BracketElement::Class(ClassItem::Named(named)))
                        .ok_or_else(|| invalid("invalid character class"));
                }
                Some(delimiter @ ('.' | '=')) => {
                    self.at += 1;
                    let name = self.until_closing(delimiter, "invalid collating element")?;
                    return collating_element(&name)
                        .map(BracketElement::Character)
                        .ok_or_else(|| invalid("invalid collating element"));
                }
                _ => return Ok(BracketElement::Character('[')),
            }
        }
        if character == '\\' {
            let Some(escaped) = self.peek() else {
                return Err(invalid("brackets [] not balanced"));
            };
            self.at += 1;
            return match escaped {
                'd' => Ok(BracketElement::Class(ClassItem::Named(NamedClass::Digit))),
                'w' => Ok(BracketElement::Class(ClassItem::Named(NamedClass::Word))),
                's' => Ok(BracketElement::Class(ClassItem::Named(NamedClass::Space))),
                'D' => Ok(BracketElement::Class(ClassItem::Excluded(
                    NamedClass::Digit,
                ))),
                'W' => Ok(BracketElement::Class(ClassItem::Excluded(NamedClass::Word))),
                'S' => Ok(BracketElement::Class(ClassItem::Excluded(
                    NamedClass::Space,
                ))),
                other => self.escaped_character(other).map(BracketElement::Character),
            };
        }
        Ok(BracketElement::Character(character))
    }

    fn until_closing(&mut self, delimiter: char, too_long: &str) -> SqlResult<String> {
        let mut name = String::new();
        loop {
            match (self.peek(), self.peek_at(1)) {
                (Some(found), Some(']')) if found == delimiter => {
                    self.at += 2;
                    return Ok(name);
                }
                (Some(character), _) => {
                    self.at += 1;
                    name.push(character);
                    if name.len() > 32 {
                        return Err(invalid(too_long));
                    }
                }
                (None, _) => return Err(invalid("brackets [] not balanced")),
            }
        }
    }
}

enum BracketElement {
    Character(char),
    Class(ClassItem),
}

const COLLATING_ELEMENTS: &[(&str, char)] = &[
    ("NUL", '\u{0}'),
    ("SOH", '\u{1}'),
    ("STX", '\u{2}'),
    ("ETX", '\u{3}'),
    ("EOT", '\u{4}'),
    ("ENQ", '\u{5}'),
    ("ACK", '\u{6}'),
    ("BEL", '\u{7}'),
    ("alert", '\u{7}'),
    ("BS", '\u{8}'),
    ("backspace", '\u{8}'),
    ("HT", '\t'),
    ("tab", '\t'),
    ("LF", '\n'),
    ("newline", '\n'),
    ("VT", '\u{b}'),
    ("vertical-tab", '\u{b}'),
    ("FF", '\u{c}'),
    ("form-feed", '\u{c}'),
    ("CR", '\r'),
    ("carriage-return", '\r'),
    ("SO", '\u{e}'),
    ("SI", '\u{f}'),
    ("DLE", '\u{10}'),
    ("DC1", '\u{11}'),
    ("DC2", '\u{12}'),
    ("DC3", '\u{13}'),
    ("DC4", '\u{14}'),
    ("NAK", '\u{15}'),
    ("SYN", '\u{16}'),
    ("ETB", '\u{17}'),
    ("CAN", '\u{18}'),
    ("EM", '\u{19}'),
    ("SUB", '\u{1a}'),
    ("ESC", '\u{1b}'),
    ("IS4", '\u{1c}'),
    ("FS", '\u{1c}'),
    ("IS3", '\u{1d}'),
    ("GS", '\u{1d}'),
    ("IS2", '\u{1e}'),
    ("RS", '\u{1e}'),
    ("IS1", '\u{1f}'),
    ("US", '\u{1f}'),
    ("space", ' '),
    ("exclamation-mark", '!'),
    ("quotation-mark", '"'),
    ("number-sign", '#'),
    ("dollar-sign", '$'),
    ("percent-sign", '%'),
    ("ampersand", '&'),
    ("apostrophe", '\''),
    ("left-parenthesis", '('),
    ("right-parenthesis", ')'),
    ("asterisk", '*'),
    ("plus-sign", '+'),
    ("comma", ','),
    ("hyphen", '-'),
    ("hyphen-minus", '-'),
    ("period", '.'),
    ("full-stop", '.'),
    ("slash", '/'),
    ("solidus", '/'),
    ("zero", '0'),
    ("one", '1'),
    ("two", '2'),
    ("three", '3'),
    ("four", '4'),
    ("five", '5'),
    ("six", '6'),
    ("seven", '7'),
    ("eight", '8'),
    ("nine", '9'),
    ("colon", ':'),
    ("semicolon", ';'),
    ("less-than-sign", '<'),
    ("equals-sign", '='),
    ("greater-than-sign", '>'),
    ("question-mark", '?'),
    ("commercial-at", '@'),
    ("left-square-bracket", '['),
    ("backslash", '\\'),
    ("reverse-solidus", '\\'),
    ("right-square-bracket", ']'),
    ("circumflex", '^'),
    ("circumflex-accent", '^'),
    ("underscore", '_'),
    ("low-line", '_'),
    ("grave-accent", '`'),
    ("left-brace", '{'),
    ("left-curly-bracket", '{'),
    ("vertical-line", '|'),
    ("right-brace", '}'),
    ("right-curly-bracket", '}'),
    ("tilde", '~'),
    ("DEL", '\u{7f}'),
];

fn collating_element(name: &str) -> Option<char> {
    let mut characters = name.chars();
    if let (Some(only), None) = (characters.next(), characters.next()) {
        return Some(only);
    }
    COLLATING_ELEMENTS
        .iter()
        .find(|(known, _)| *known == name)
        .map(|(_, character)| *character)
}

struct Compiler {
    program: Vec<Instruction>,
}

impl Compiler {
    fn next(&self) -> u32 {
        u32::try_from(self.program.len()).unwrap_or(u32::MAX)
    }

    fn emit(&mut self, instruction: Instruction) -> usize {
        self.program.push(instruction);
        self.program.len() - 1
    }

    fn patch(&mut self, at: usize, instruction: Instruction) {
        if let Some(slot) = self.program.get_mut(at) {
            *slot = instruction;
        }
    }

    fn compile(&mut self, node: &Node) {
        match node {
            Node::Empty => {}
            Node::Literal(character) => {
                self.emit(Instruction::Literal(*character));
            }
            Node::Any => {
                self.emit(Instruction::Any);
            }
            Node::Class(index) => {
                self.emit(Instruction::Class(
                    u32::try_from(*index).unwrap_or(u32::MAX),
                ));
            }
            Node::Start => {
                self.emit(Instruction::Start);
            }
            Node::End => {
                self.emit(Instruction::End);
            }
            Node::Concat(nodes) => {
                for node in nodes {
                    self.compile(node);
                }
            }
            Node::Alternate(nodes) => {
                let mut jumps = Vec::with_capacity(nodes.len());
                for (index, node) in nodes.iter().enumerate() {
                    if index + 1 == nodes.len() {
                        self.compile(node);
                        break;
                    }
                    let split = self.emit(Instruction::Split(0, 0));
                    let first = self.next();
                    self.compile(node);
                    jumps.push(self.emit(Instruction::Jump(0)));
                    let second = self.next();
                    self.patch(split, Instruction::Split(first, second));
                }
                let end = self.next();
                for jump in jumps {
                    self.patch(jump, Instruction::Jump(end));
                }
            }
            Node::Repeat { node, min, max } => {
                for _ in 0..*min {
                    self.compile(node);
                }
                match max {
                    None => {
                        let split = self.emit(Instruction::Split(0, 0));
                        let body = self.next();
                        self.compile(node);
                        self.emit(Instruction::Jump(u32::try_from(split).unwrap_or(u32::MAX)));
                        let end = self.next();
                        self.patch(split, Instruction::Split(body, end));
                    }
                    Some(max) => {
                        let mut splits = Vec::new();
                        for _ in *min..*max {
                            splits.push(self.emit(Instruction::Split(0, 0)));
                            self.compile(node);
                        }
                        let end = self.next();
                        for split in splits {
                            let body = u32::try_from(split + 1).unwrap_or(u32::MAX);
                            self.patch(split, Instruction::Split(body, end));
                        }
                    }
                }
            }
        }
    }
}

struct Threads {
    dense: Vec<u32>,
    sparse: Vec<u32>,
}

impl Threads {
    fn new(size: usize) -> Self {
        Self {
            dense: Vec::with_capacity(size),
            sparse: vec![0; size],
        }
    }

    fn contains(&self, pc: u32) -> bool {
        let Some(&slot) = self.sparse.get(pc as usize) else {
            return true;
        };
        self.dense.get(slot as usize) == Some(&pc)
    }

    fn insert(&mut self, pc: u32) {
        if let Some(slot) = self.sparse.get_mut(pc as usize) {
            *slot = u32::try_from(self.dense.len()).unwrap_or(u32::MAX);
            self.dense.push(pc);
        }
    }

    fn clear(&mut self) {
        self.dense.clear();
    }
}

impl Regex {
    pub(crate) fn compile(pattern: &str, case: CaseSensitivity) -> SqlResult<Self> {
        if pattern.len() > MAX_PATTERN_BYTES {
            return Err(too_complex());
        }
        let body = if let Some(rest) = pattern.strip_prefix("***:") {
            rest
        } else if pattern.starts_with("***=") {
            return Err(unsupported("literal patterns (***=)"));
        } else if pattern.starts_with("***") {
            return Err(invalid("invalid embedded option"));
        } else {
            pattern
        };
        let mut parser = PatternParser {
            chars: body.chars().collect(),
            at: 0,
            depth: 0,
            case,
            classes: Vec::new(),
        };
        let node = parser.alternation()?;
        if parser.at < parser.chars.len() {
            return Err(invalid("parentheses () not balanced"));
        }
        if node.size() >= MAX_INSTRUCTIONS as u64 {
            return Err(too_complex());
        }
        let anchored = node.anchored();
        let mut compiler = Compiler {
            program: Vec::new(),
        };
        compiler.compile(&node);
        compiler.emit(Instruction::Match);
        Ok(Self {
            program: compiler.program,
            classes: parser.classes,
            case,
            anchored,
        })
    }

    pub(crate) fn is_match(&self, text: &str, budget: &mut StepBudget) -> SqlResult<bool> {
        let size = self.program.len();
        let mut current = Threads::new(size);
        let mut next = Threads::new(size);
        let mut stack: Vec<u32> = Vec::new();
        let mut characters = text.chars().peekable();
        let mut position = 0usize;
        loop {
            if !self.anchored || position == 0 {
                let at_end = characters.peek().is_none();
                if self.add(&mut current, &mut stack, 0, position == 0, at_end, budget)? {
                    return Ok(true);
                }
            }
            let Some(character) = characters.next() else {
                return Ok(false);
            };
            if current.dense.is_empty() && self.anchored {
                return Ok(false);
            }
            budget.spend(current.dense.len() as u64 + 1)?;
            let folded = match self.case {
                CaseSensitivity::Sensitive => character,
                CaseSensitivity::Insensitive => character.to_ascii_lowercase(),
            };
            let at_end = characters.peek().is_none();
            let mut matched = false;
            for index in 0..current.dense.len() {
                let Some(&pc) = current.dense.get(index) else {
                    break;
                };
                let advances = match self.program.get(pc as usize) {
                    Some(Instruction::Literal(expected)) => *expected == folded,
                    Some(Instruction::Any) => true,
                    Some(Instruction::Class(class)) => self
                        .classes
                        .get(*class as usize)
                        .is_some_and(|class| class.matches(character, self.case)),
                    _ => false,
                };
                if advances && self.add(&mut next, &mut stack, pc + 1, false, at_end, budget)? {
                    matched = true;
                    break;
                }
            }
            if matched {
                return Ok(true);
            }
            std::mem::swap(&mut current, &mut next);
            next.clear();
            position += 1;
        }
    }

    fn add(
        &self,
        threads: &mut Threads,
        stack: &mut Vec<u32>,
        start: u32,
        at_start: bool,
        at_end: bool,
        budget: &mut StepBudget,
    ) -> SqlResult<bool> {
        stack.clear();
        stack.push(start);
        let mut visited = 0u64;
        while let Some(pc) = stack.pop() {
            if threads.contains(pc) {
                continue;
            }
            threads.insert(pc);
            visited += 1;
            match self.program.get(pc as usize) {
                Some(Instruction::Match) => {
                    budget.spend(visited)?;
                    return Ok(true);
                }
                Some(Instruction::Jump(target)) => stack.push(*target),
                Some(Instruction::Split(first, second)) => {
                    stack.push(*second);
                    stack.push(*first);
                }
                Some(Instruction::Start) if at_start => stack.push(pc + 1),
                Some(Instruction::End) if at_end => stack.push(pc + 1),
                _ => {}
            }
        }
        budget.spend(visited)?;
        Ok(false)
    }
}

struct StatementRegexes {
    sensitive: HashMap<String, Rc<Regex>>,
    insensitive: HashMap<String, Rc<Regex>>,
    cached_bytes: usize,
}

impl StatementRegexes {
    fn compiled(&mut self, case: CaseSensitivity) -> &mut HashMap<String, Rc<Regex>> {
        match case {
            CaseSensitivity::Sensitive => &mut self.sensitive,
            CaseSensitivity::Insensitive => &mut self.insensitive,
        }
    }
}

thread_local! {
    static STATEMENT: RefCell<Option<StatementRegexes>> = const { RefCell::new(None) };
}

pub(crate) struct EnteredStatement {
    previous: Option<StatementRegexes>,
}

impl Drop for EnteredStatement {
    fn drop(&mut self) {
        let previous = self.previous.take();
        STATEMENT.with(|statement| {
            if let Ok(mut statement) = statement.try_borrow_mut() {
                *statement = previous;
            }
        });
    }
}

pub(crate) fn enter_statement() -> EnteredStatement {
    let fresh = StatementRegexes {
        sensitive: HashMap::new(),
        insensitive: HashMap::new(),
        cached_bytes: 0,
    };
    let previous = STATEMENT.with(|statement| {
        statement
            .try_borrow_mut()
            .ok()
            .and_then(|mut statement| statement.replace(fresh))
    });
    EnteredStatement { previous }
}

fn compiled(pattern: &str, case: CaseSensitivity) -> SqlResult<Rc<Regex>> {
    let cached = STATEMENT.with(|statement| {
        statement
            .try_borrow_mut()
            .ok()
            .and_then(|mut statement| statement.as_mut()?.compiled(case).get(pattern).cloned())
    });
    if let Some(regex) = cached {
        return Ok(regex);
    }
    let regex = Rc::new(Regex::compile(pattern, case)?);
    STATEMENT.with(|statement| {
        if let Ok(mut statement) = statement.try_borrow_mut()
            && let Some(statement) = statement.as_mut()
            && statement.sensitive.len() + statement.insensitive.len() < MOST_CACHED_PATTERNS
            && statement.cached_bytes.saturating_add(pattern.len()) <= MOST_CACHED_PATTERN_BYTES
        {
            statement.cached_bytes += pattern.len();
            statement
                .compiled(case)
                .insert(pattern.to_owned(), Rc::clone(&regex));
        }
    });
    Ok(regex)
}

pub(crate) fn matches(text: &str, pattern: &str, case: CaseSensitivity) -> SqlResult<bool> {
    matches_within(text, pattern, case, MATCH_STEPS)
}

pub(crate) fn matches_within(
    text: &str,
    pattern: &str,
    case: CaseSensitivity,
    steps: u64,
) -> SqlResult<bool> {
    compiled(pattern, case)?.is_match(text, &mut StepBudget::new(steps))
}

fn no_operator(symbol: &str, left: &Value, right: &Value) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_FUNCTION,
        format!(
            "operator does not exist: {} {symbol} {}",
            left.type_name(),
            right.type_name()
        ),
    )
    .with_hint(
        "No operator matches the given name and argument types. You might need to add explicit type casts.",
    )
}

pub(crate) fn operator(
    operator: BinaryOperator,
    text: &Value,
    pattern: &Value,
) -> SqlResult<Value> {
    let (negated, case) = match operator {
        BinaryOperator::RegexMatch => (false, CaseSensitivity::Sensitive),
        BinaryOperator::RegexMatchFolded => (false, CaseSensitivity::Insensitive),
        BinaryOperator::RegexNoMatch => (true, CaseSensitivity::Sensitive),
        _ => (true, CaseSensitivity::Insensitive),
    };
    let subject = match text {
        Value::Text(text) | Value::Bpchar(text) => Some(text.as_str()),
        _ => None,
    };
    match (text, pattern) {
        (Value::Null, other) | (other, Value::Null)
            if other.is_null() || other.character_text().is_some() =>
        {
            Ok(Value::Null)
        }
        _ => match (subject, pattern.character_text()) {
            (Some(subject), Some(pattern)) => {
                Ok(Value::Boolean(matches(subject, pattern, case)? != negated))
            }
            _ => Err(no_operator(operator.symbol(), text, pattern)),
        },
    }
}
