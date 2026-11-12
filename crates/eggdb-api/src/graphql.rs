//! A reader for GraphQL documents, as the October 2021 specification defines them: operations
//! with variables, selection sets with aliases, arguments and directives, and fragments. Type
//! system definitions are refused; variable types are read and not kept.

const MAX_DEPTH: usize = 64;
const MAX_DOCUMENT_BYTES: usize = 1 << 20;

/// A GraphQL input value.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum GqlValue {
    Variable(String),
    Int(String),
    Float(String),
    String(String),
    Boolean(bool),
    Null,
    Enum(String),
    List(Vec<GqlValue>),
    Object(Vec<(String, GqlValue)>),
}

impl GqlValue {
    pub(crate) fn field(&self, name: &str) -> Option<&GqlValue> {
        match self {
            Self::Object(fields) => fields.iter().find(|(key, _)| key == name).map(|(_, value)| value),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Directive {
    pub(crate) name: String,
    pub(crate) arguments: Vec<(String, GqlValue)>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Field {
    pub(crate) alias: Option<String>,
    pub(crate) name: String,
    pub(crate) arguments: Vec<(String, GqlValue)>,
    pub(crate) directives: Vec<Directive>,
    pub(crate) selections: Vec<Selection>,
}

impl Field {
    pub(crate) fn key(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.name)
    }

    pub(crate) fn argument(&self, name: &str) -> Option<&GqlValue> {
        self.arguments.iter().find(|(key, _)| key == name).map(|(_, value)| value)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Selection {
    Field(Field),
    FragmentSpread {
        name: String,
        directives: Vec<Directive>,
    },
    InlineFragment {
        type_condition: Option<String>,
        directives: Vec<Directive>,
        selections: Vec<Selection>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OperationKind {
    Query,
    Mutation,
    Subscription,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct VariableDefinition {
    pub(crate) name: String,
    pub(crate) default: Option<GqlValue>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Operation {
    pub(crate) kind: OperationKind,
    pub(crate) name: Option<String>,
    pub(crate) variables: Vec<VariableDefinition>,
    pub(crate) selections: Vec<Selection>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Fragment {
    pub(crate) name: String,
    pub(crate) type_condition: String,
    pub(crate) selections: Vec<Selection>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Document {
    pub(crate) operations: Vec<Operation>,
    pub(crate) fragments: Vec<Fragment>,
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Punctuator(char),
    Spread,
    Name(String),
    Int(String),
    Float(String),
    String(String),
    End,
}

fn is_name_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

fn is_name_part(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

struct Lexer<'t> {
    text: &'t str,
    bytes: &'t [u8],
    at: usize,
}

impl Lexer<'_> {
    fn skip_ignored(&mut self) {
        while let Some(&byte) = self.bytes.get(self.at) {
            match byte {
                b' ' | b'\t' | b'\n' | b'\r' | b',' => self.at += 1,
                0xEF if self.bytes.get(self.at..self.at + 3) == Some(&[0xEF, 0xBB, 0xBF]) => self.at += 3,
                b'#' => {
                    while self.bytes.get(self.at).is_some_and(|&byte| byte != b'\n' && byte != b'\r') {
                        self.at += 1;
                    }
                }
                _ => return,
            }
        }
    }

    fn next(&mut self) -> Result<Token, String> {
        self.skip_ignored();
        let Some(&byte) = self.bytes.get(self.at) else {
            return Ok(Token::End);
        };
        match byte {
            b'!' | b'$' | b'&' | b'(' | b')' | b':' | b'=' | b'@' | b'[' | b']' | b'{' | b'|' | b'}' => {
                self.at += 1;
                Ok(Token::Punctuator(char::from(byte)))
            }
            b'.' if self.bytes.get(self.at..self.at + 3) == Some(b"...") => {
                self.at += 3;
                Ok(Token::Spread)
            }
            b'"' if self.bytes.get(self.at..self.at + 3) == Some(b"\"\"\"") => self.block_string(),
            b'"' => self.string(),
            b'-' | b'0'..=b'9' => self.number(),
            _ if is_name_start(byte) => {
                let start = self.at;
                while self.bytes.get(self.at).copied().is_some_and(is_name_part) {
                    self.at += 1;
                }
                Ok(Token::Name(self.text[start..self.at].to_owned()))
            }
            _ => Err(format!("Syntax Error: Unexpected character at offset {}.", self.at)),
        }
    }

    fn digits(&mut self) -> usize {
        let start = self.at;
        while self.bytes.get(self.at).is_some_and(u8::is_ascii_digit) {
            self.at += 1;
        }
        self.at - start
    }

    fn number(&mut self) -> Result<Token, String> {
        let start = self.at;
        if self.bytes.get(self.at) == Some(&b'-') {
            self.at += 1;
        }
        let integer_start = self.at;
        if self.digits() == 0 {
            return Err(format!("Syntax Error: Invalid number at offset {start}."));
        }
        if self.bytes.get(integer_start) == Some(&b'0') && self.at - integer_start > 1 {
            return Err(format!("Syntax Error: Invalid number, unexpected digit after 0 at offset {start}."));
        }
        let mut float = false;
        if self.bytes.get(self.at) == Some(&b'.') {
            self.at += 1;
            float = true;
            if self.digits() == 0 {
                return Err(format!("Syntax Error: Invalid number at offset {start}."));
            }
        }
        if matches!(self.bytes.get(self.at), Some(b'e' | b'E')) {
            self.at += 1;
            float = true;
            if matches!(self.bytes.get(self.at), Some(b'+' | b'-')) {
                self.at += 1;
            }
            if self.digits() == 0 {
                return Err(format!("Syntax Error: Invalid number at offset {start}."));
            }
        }
        if self.bytes.get(self.at).copied().is_some_and(|byte| is_name_start(byte) || byte == b'.') {
            return Err(format!("Syntax Error: Invalid number at offset {start}."));
        }
        let text = self.text[start..self.at].to_owned();
        Ok(if float { Token::Float(text) } else { Token::Int(text) })
    }

    fn string(&mut self) -> Result<Token, String> {
        let start = self.at;
        self.at += 1;
        let mut value = String::new();
        loop {
            let Some(character) = self.text.get(self.at..).and_then(|rest| rest.chars().next()) else {
                return Err(format!("Syntax Error: Unterminated string at offset {start}."));
            };
            match character {
                '"' => {
                    self.at += 1;
                    return Ok(Token::String(value));
                }
                '\n' | '\r' => return Err(format!("Syntax Error: Unterminated string at offset {start}.")),
                '\\' => {
                    self.at += 1;
                    let escaped = self.bytes.get(self.at).copied();
                    self.at += 1;
                    match escaped {
                        Some(b'"') => value.push('"'),
                        Some(b'\\') => value.push('\\'),
                        Some(b'/') => value.push('/'),
                        Some(b'b') => value.push('\u{8}'),
                        Some(b'f') => value.push('\u{c}'),
                        Some(b'n') => value.push('\n'),
                        Some(b'r') => value.push('\r'),
                        Some(b't') => value.push('\t'),
                        Some(b'u') => value.push(self.unicode_escape(start)?),
                        _ => return Err(format!("Syntax Error: Invalid character escape sequence at offset {start}.")),
                    }
                }
                other => {
                    value.push(other);
                    self.at += other.len_utf8();
                }
            }
        }
    }

    fn hex4(&mut self, start: usize) -> Result<u32, String> {
        let digits = self
            .text
            .get(self.at..self.at + 4)
            .ok_or_else(|| format!("Syntax Error: Invalid Unicode escape sequence at offset {start}."))?;
        let code = u32::from_str_radix(digits, 16)
            .map_err(|_| format!("Syntax Error: Invalid Unicode escape sequence at offset {start}."))?;
        self.at += 4;
        Ok(code)
    }

    fn unicode_escape(&mut self, start: usize) -> Result<char, String> {
        let invalid = || format!("Syntax Error: Invalid Unicode escape sequence at offset {start}.");
        let code = if self.bytes.get(self.at) == Some(&b'{') {
            self.at += 1;
            let length = self.bytes[self.at..].iter().take_while(|byte| byte.is_ascii_hexdigit()).count();
            if length == 0 || length > 8 || self.bytes.get(self.at + length) != Some(&b'}') {
                return Err(invalid());
            }
            let code = u32::from_str_radix(&self.text[self.at..self.at + length], 16).map_err(|_| invalid())?;
            self.at += length + 1;
            code
        } else {
            let high = self.hex4(start)?;
            if (0xD800..0xDC00).contains(&high) && self.bytes.get(self.at..self.at + 2) == Some(b"\\u") {
                self.at += 2;
                let low = self.hex4(start)?;
                if !(0xDC00..0xE000).contains(&low) {
                    return Err(invalid());
                }
                0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00)
            } else {
                high
            }
        };
        char::from_u32(code).ok_or_else(invalid)
    }

    fn block_string(&mut self) -> Result<Token, String> {
        let start = self.at;
        self.at += 3;
        let mut raw = String::new();
        loop {
            let rest = self.text.get(self.at..).unwrap_or_default();
            if rest.is_empty() {
                return Err(format!("Syntax Error: Unterminated string at offset {start}."));
            }
            if rest.starts_with("\"\"\"") {
                self.at += 3;
                return Ok(Token::String(block_string_value(&raw)));
            }
            if rest.starts_with("\\\"\"\"") {
                raw.push_str("\"\"\"");
                self.at += 4;
                continue;
            }
            let character = rest.chars().next().unwrap_or_default();
            raw.push(character);
            self.at += character.len_utf8();
        }
    }
}

fn leading_whitespace(line: &str) -> usize {
    line.len() - line.trim_start_matches([' ', '\t']).len()
}

fn block_string_value(raw: &str) -> String {
    let lines: Vec<&str> = raw.split("\r\n").flat_map(|part| part.split(['\n', '\r'])).collect();
    let common = lines
        .iter()
        .skip(1)
        .filter(|line| !line.trim_start_matches([' ', '\t']).is_empty())
        .map(|line| leading_whitespace(line))
        .min();
    let mut kept: Vec<String> = lines
        .iter()
        .enumerate()
        .map(|(index, line)| match common {
            Some(indent) if index > 0 => line.get(indent.min(line.len())..).unwrap_or_default().to_owned(),
            _ => (*line).to_owned(),
        })
        .collect();
    while kept.first().is_some_and(|line| line.trim_start_matches([' ', '\t']).is_empty()) {
        kept.remove(0);
    }
    while kept.last().is_some_and(|line| line.trim_start_matches([' ', '\t']).is_empty()) {
        kept.pop();
    }
    kept.join("\n")
}

fn shown(token: &Token) -> String {
    match token {
        Token::Punctuator(character) => format!("\"{character}\""),
        Token::Spread => "\"...\"".to_owned(),
        Token::Name(name) => format!("Name \"{name}\""),
        Token::Int(text) | Token::Float(text) => format!("Number \"{text}\""),
        Token::String(_) => "String".to_owned(),
        Token::End => "<EOF>".to_owned(),
    }
}

struct Parser<'t> {
    lexer: Lexer<'t>,
    current: Token,
    depth: usize,
}

impl Parser<'_> {
    fn advance(&mut self) -> Result<Token, String> {
        let next = self.lexer.next()?;
        Ok(std::mem::replace(&mut self.current, next))
    }

    fn is(&self, punctuator: char) -> bool {
        self.current == Token::Punctuator(punctuator)
    }

    fn take(&mut self, punctuator: char) -> Result<bool, String> {
        if self.is(punctuator) {
            self.advance()?;
            return Ok(true);
        }
        Ok(false)
    }

    fn expect(&mut self, punctuator: char) -> Result<(), String> {
        if self.take(punctuator)? {
            return Ok(());
        }
        Err(format!("Syntax Error: Expected \"{punctuator}\", found {}.", self.shown()))
    }

    fn shown(&self) -> String {
        shown(&self.current)
    }

    fn is_name(&self, word: &str) -> bool {
        matches!(&self.current, Token::Name(name) if name == word)
    }

    fn name(&mut self) -> Result<String, String> {
        match self.advance()? {
            Token::Name(name) => Ok(name),
            other => Err(format!("Syntax Error: Expected Name, found {}.", shown(&other))),
        }
    }

    fn deeper(&mut self) -> Result<(), String> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(format!("Syntax Error: the document nests more than {MAX_DEPTH} levels."));
        }
        Ok(())
    }

    fn document(&mut self) -> Result<Document, String> {
        let mut document = Document::default();
        if self.current == Token::End {
            return Err("Syntax Error: Unexpected <EOF>.".to_owned());
        }
        while self.current != Token::End {
            if self.is('{') {
                let selections = self.selection_set()?;
                document.operations.push(Operation {
                    kind: OperationKind::Query,
                    name: None,
                    variables: Vec::new(),
                    selections,
                });
                continue;
            }
            let Token::Name(word) = &self.current else {
                return Err(format!("Syntax Error: Unexpected {}.", self.shown()));
            };
            let kind = match word.as_str() {
                "query" => OperationKind::Query,
                "mutation" => OperationKind::Mutation,
                "subscription" => OperationKind::Subscription,
                "fragment" => {
                    self.advance()?;
                    document.fragments.push(self.fragment()?);
                    continue;
                }
                _ => {
                    return Err(format!(
                        "Syntax Error: Unexpected {}; type system definitions are not executable.",
                        self.shown()
                    ));
                }
            };
            self.advance()?;
            let name = match &self.current {
                Token::Name(_) => Some(self.name()?),
                _ => None,
            };
            let variables = self.variable_definitions()?;
            self.directives()?;
            let selections = self.selection_set()?;
            document.operations.push(Operation {
                kind,
                name,
                variables,
                selections,
            });
        }
        Ok(document)
    }

    fn fragment(&mut self) -> Result<Fragment, String> {
        let name = self.name()?;
        if name == "on" {
            return Err("Syntax Error: Unexpected Name \"on\".".to_owned());
        }
        if !self.is_name("on") {
            return Err(format!("Syntax Error: Expected \"on\", found {}.", self.shown()));
        }
        self.advance()?;
        let type_condition = self.name()?;
        self.directives()?;
        let selections = self.selection_set()?;
        Ok(Fragment {
            name,
            type_condition,
            selections,
        })
    }

    fn variable_definitions(&mut self) -> Result<Vec<VariableDefinition>, String> {
        let mut definitions = Vec::new();
        if !self.take('(')? {
            return Ok(definitions);
        }
        while !self.take(')')? {
            self.expect('$')?;
            let name = self.name()?;
            self.expect(':')?;
            self.variable_type()?;
            let default = if self.take('=')? { Some(self.value(true)?) } else { None };
            self.directives()?;
            definitions.push(VariableDefinition { name, default });
        }
        if definitions.is_empty() {
            return Err("Syntax Error: Expected \"$\", found \")\".".to_owned());
        }
        Ok(definitions)
    }

    fn variable_type(&mut self) -> Result<(), String> {
        self.deeper()?;
        if self.take('[')? {
            self.variable_type()?;
            self.expect(']')?;
        } else {
            self.name()?;
        }
        self.take('!')?;
        self.depth -= 1;
        Ok(())
    }

    fn directives(&mut self) -> Result<Vec<Directive>, String> {
        let mut directives = Vec::new();
        while self.take('@')? {
            let name = self.name()?;
            let arguments = self.arguments()?;
            directives.push(Directive { name, arguments });
        }
        Ok(directives)
    }

    fn arguments(&mut self) -> Result<Vec<(String, GqlValue)>, String> {
        let mut arguments = Vec::new();
        if !self.take('(')? {
            return Ok(arguments);
        }
        while !self.take(')')? {
            let name = self.name()?;
            self.expect(':')?;
            arguments.push((name, self.value(false)?));
        }
        if arguments.is_empty() {
            return Err("Syntax Error: Expected Name, found \")\".".to_owned());
        }
        Ok(arguments)
    }

    fn value(&mut self, constant: bool) -> Result<GqlValue, String> {
        self.deeper()?;
        let value = match self.advance()? {
            Token::Punctuator('$') if !constant => GqlValue::Variable(self.name()?),
            Token::Int(text) => GqlValue::Int(text),
            Token::Float(text) => GqlValue::Float(text),
            Token::String(text) => GqlValue::String(text),
            Token::Name(name) => match name.as_str() {
                "true" => GqlValue::Boolean(true),
                "false" => GqlValue::Boolean(false),
                "null" => GqlValue::Null,
                _ => GqlValue::Enum(name),
            },
            Token::Punctuator('[') => {
                let mut items = Vec::new();
                while !self.take(']')? {
                    if self.current == Token::End {
                        return Err("Syntax Error: Expected \"]\", found <EOF>.".to_owned());
                    }
                    items.push(self.value(constant)?);
                }
                GqlValue::List(items)
            }
            Token::Punctuator('{') => {
                let mut fields = Vec::new();
                while !self.take('}')? {
                    let name = self.name()?;
                    self.expect(':')?;
                    fields.push((name, self.value(constant)?));
                }
                GqlValue::Object(fields)
            }
            other => return Err(format!("Syntax Error: Unexpected {}.", shown(&other))),
        };
        self.depth -= 1;
        Ok(value)
    }

    fn selection_set(&mut self) -> Result<Vec<Selection>, String> {
        self.deeper()?;
        self.expect('{')?;
        let mut selections = Vec::new();
        while !self.take('}')? {
            if self.current == Token::End {
                return Err("Syntax Error: Expected Name, found <EOF>.".to_owned());
            }
            selections.push(self.selection()?);
        }
        if selections.is_empty() {
            return Err("Syntax Error: Expected Name, found \"}\".".to_owned());
        }
        self.depth -= 1;
        Ok(selections)
    }

    fn selection(&mut self) -> Result<Selection, String> {
        if self.current == Token::Spread {
            self.advance()?;
            if matches!(&self.current, Token::Name(name) if name != "on") {
                let name = self.name()?;
                let directives = self.directives()?;
                return Ok(Selection::FragmentSpread { name, directives });
            }
            let type_condition = if self.is_name("on") {
                self.advance()?;
                Some(self.name()?)
            } else {
                None
            };
            let directives = self.directives()?;
            let selections = self.selection_set()?;
            return Ok(Selection::InlineFragment {
                type_condition,
                directives,
                selections,
            });
        }
        let first = self.name()?;
        let (alias, name) = if self.take(':')? {
            (Some(first), self.name()?)
        } else {
            (None, first)
        };
        let arguments = self.arguments()?;
        let directives = self.directives()?;
        let selections = if self.is('{') { self.selection_set()? } else { Vec::new() };
        Ok(Selection::Field(Field {
            alias,
            name,
            arguments,
            directives,
            selections,
        }))
    }
}

/// Reads a GraphQL document, or says why it is not one.
pub(crate) fn parse(text: &str) -> Result<Document, String> {
    if text.len() > MAX_DOCUMENT_BYTES {
        return Err(format!("the document is longer than {MAX_DOCUMENT_BYTES} bytes"));
    }
    let mut lexer = Lexer {
        text,
        bytes: text.as_bytes(),
        at: 0,
    };
    let current = lexer.next()?;
    let mut parser = Parser {
        lexer,
        current,
        depth: 0,
    };
    parser.document()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_operations_fragments_and_values() {
        let document = parse(
            r#"
            # a comment
            query Orders($limit: Int! = 10, $ids: [Int!]) @cached {
              recent: orders(limit: $limit, where: {id: {_in: $ids}, note: {_eq: "a\"b\u00e9"}}, order_by: [{id: desc}]) {
                id
                ...Parts
                ... on orders @include(if: true) { total }
              }
            }
            fragment Parts on orders { customer, placed_at }
            mutation { delete_orders(where: {total: {_lt: -1.5e2}}) { affected_rows } }
            "#,
        )
        .unwrap();
        assert_eq!(document.operations.len(), 2);
        assert_eq!(document.fragments.len(), 1);
        let query = &document.operations[0];
        assert_eq!(query.name.as_deref(), Some("Orders"));
        assert_eq!(query.variables[0].default, Some(GqlValue::Int("10".to_owned())));
        let Selection::Field(field) = &query.selections[0] else {
            panic!("a field");
        };
        assert_eq!(field.key(), "recent");
        assert_eq!(field.argument("limit"), Some(&GqlValue::Variable("limit".to_owned())));
        let filter = field.argument("where").unwrap();
        assert_eq!(
            filter.field("note").and_then(|note| note.field("_eq")),
            Some(&GqlValue::String("a\"bé".to_owned()))
        );
        assert_eq!(field.selections.len(), 3);
        assert_eq!(document.operations[1].kind, OperationKind::Mutation);
    }

    #[test]
    fn block_strings_lose_their_common_indent() {
        let document = parse("{ a(text: \"\"\"\n    first\n      second\n  \"\"\") }").unwrap();
        let Selection::Field(field) = &document.operations[0].selections[0] else {
            panic!("a field");
        };
        assert_eq!(field.argument("text"), Some(&GqlValue::String("first\n  second".to_owned())));
    }

    #[test]
    fn hostile_documents_fail_without_panicking() {
        for text in [
            "",
            "{",
            "{ a(",
            "{ a(b: ) }",
            "{ a(b: \"x) }",
            "{ a(b: 012) }",
            "{ a(b: \"\\u{110000}\") }",
            "type Query { a: Int }",
            "fragment on on T { a }",
            "{ ... }",
            "query ($a: ) { b }",
        ] {
            assert!(parse(text).is_err(), "{text}");
        }
        let deep = format!("{}{}", "{ a ".repeat(200), "}".repeat(200));
        assert!(parse(&deep).is_err());
        let deep_value = format!("{{ a(b: {}1{}) }}", "[".repeat(200), "]".repeat(200));
        assert!(parse(&deep_value).is_err());
    }
}
