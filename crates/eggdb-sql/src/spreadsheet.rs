use crate::error::{SqlError, SqlResult, SqlState};
use crate::json::{Json, parse_document};
use std::collections::{HashMap, VecDeque};

const MOST_CELLS: usize = 100_000;
const MOST_RANGE_CELLS: usize = 100_000;
const MOST_FORMULA_BYTES: usize = 8192;
const MOST_NESTING: usize = 100;
const MOST_COLUMN: u32 = 16_384;
const MOST_ROW: u32 = 1_048_576;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct Cell {
    column: u32,
    row: u32,
}

impl Cell {
    fn name(self) -> String {
        let mut letters = Vec::new();
        let mut column = self.column;
        while column > 0 {
            let remainder = (column - 1) % 26;
            letters.push(char::from(b'A' + remainder as u8));
            column = (column - 1) / 26;
        }
        letters.reverse();
        format!("{}{}", letters.into_iter().collect::<String>(), self.row)
    }
}

fn parse_cell(text: &str) -> Option<Cell> {
    let text = text.trim().trim_start_matches('$');
    let letters_end = text.find(|character: char| !character.is_ascii_alphabetic())?;
    let (letters, rest) = text.split_at(letters_end);
    let digits = rest.trim_start_matches('$');
    if letters.is_empty() || letters.len() > 3 || digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let column = letters.bytes().try_fold(0u32, |sum, byte| Some(sum * 26 + u32::from(byte.to_ascii_uppercase() - b'A') + 1))?;
    let row: u32 = digits.parse().ok()?;
    (column <= MOST_COLUMN && (1..=MOST_ROW).contains(&row)).then_some(Cell { column, row })
}

#[derive(Debug, Clone, PartialEq)]
enum Value {
    Empty,
    Number(f64),
    Text(String),
    Boolean(bool),
    Error(&'static str),
}

#[derive(Debug, Clone)]
enum Expression {
    Literal(Value),
    Reference(Cell),
    Range(Cell, Cell),
    Negate(Box<Expression>),
    Percent(Box<Expression>),
    Binary(char, Box<Expression>, Box<Expression>),
    Compare(&'static str, Box<Expression>, Box<Expression>),
    Call(String, Vec<Expression>),
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Number(f64),
    Text(String),
    Word(String),
    Symbol(&'static str),
}

const SYMBOLS: [&str; 16] = ["<>", "<=", ">=", "+", "-", "*", "/", "^", "&", "=", "<", ">", "(", ")", ",", ":"];

fn tokens(formula: &str) -> Option<Vec<Token>> {
    let mut found = Vec::new();
    let bytes = formula.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        let byte = bytes[at];
        if byte.is_ascii_whitespace() {
            at += 1;
        } else if byte == b'"' {
            let mut text = String::new();
            let mut position = at + 1;
            loop {
                let next = formula.get(position..)?.chars().next()?;
                position += next.len_utf8();
                if next == '"' {
                    if formula.get(position..)?.starts_with('"') {
                        text.push('"');
                        position += 1;
                        continue;
                    }
                    break;
                }
                text.push(next);
            }
            found.push(Token::Text(text));
            at = position;
        } else if byte.is_ascii_digit() || (byte == b'.' && bytes.get(at + 1).is_some_and(u8::is_ascii_digit)) {
            let start = at;
            while at < bytes.len() && (bytes[at].is_ascii_digit() || bytes[at] == b'.') {
                at += 1;
            }
            if at < bytes.len() && (bytes[at] == b'e' || bytes[at] == b'E') {
                let mut exponent = at + 1;
                if exponent < bytes.len() && (bytes[exponent] == b'+' || bytes[exponent] == b'-') {
                    exponent += 1;
                }
                if exponent < bytes.len() && bytes[exponent].is_ascii_digit() {
                    at = exponent;
                    while at < bytes.len() && bytes[at].is_ascii_digit() {
                        at += 1;
                    }
                }
            }
            found.push(Token::Number(formula.get(start..at)?.parse().ok()?));
        } else if byte.is_ascii_alphabetic() || byte == b'$' || byte == b'_' {
            let start = at;
            while at < bytes.len() && (bytes[at].is_ascii_alphanumeric() || bytes[at] == b'$' || bytes[at] == b'_' || bytes[at] == b'.') {
                at += 1;
            }
            found.push(Token::Word(formula.get(start..at)?.to_owned()));
        } else if byte == b'%' {
            found.push(Token::Symbol("%"));
            at += 1;
        } else {
            let rest = formula.get(at..)?;
            let symbol = SYMBOLS.iter().find(|symbol| rest.starts_with(**symbol))?;
            found.push(Token::Symbol(symbol));
            at += symbol.len();
        }
    }
    Some(found)
}

struct Parser {
    tokens: Vec<Token>,
    at: usize,
    depth: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at)
    }

    fn take_symbol(&mut self, wanted: &str) -> bool {
        if matches!(self.peek(), Some(Token::Symbol(symbol)) if *symbol == wanted) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn deeper(&mut self) -> Option<()> {
        self.depth += 1;
        (self.depth <= MOST_NESTING).then_some(())
    }

    fn comparison(&mut self) -> Option<Expression> {
        self.deeper()?;
        let mut left = self.concatenation()?;
        loop {
            let operator = match self.peek() {
                Some(Token::Symbol(symbol @ ("=" | "<>" | "<" | "<=" | ">" | ">="))) => *symbol,
                _ => break,
            };
            self.at += 1;
            let right = self.concatenation()?;
            left = Expression::Compare(operator, Box::new(left), Box::new(right));
        }
        self.depth -= 1;
        Some(left)
    }

    fn concatenation(&mut self) -> Option<Expression> {
        let mut left = self.additive()?;
        while self.take_symbol("&") {
            let right = self.additive()?;
            left = Expression::Binary('&', Box::new(left), Box::new(right));
        }
        Some(left)
    }

    fn additive(&mut self) -> Option<Expression> {
        let mut left = self.term()?;
        loop {
            let operator = if self.take_symbol("+") {
                '+'
            } else if self.take_symbol("-") {
                '-'
            } else {
                break;
            };
            let right = self.term()?;
            left = Expression::Binary(operator, Box::new(left), Box::new(right));
        }
        Some(left)
    }

    fn term(&mut self) -> Option<Expression> {
        let mut left = self.power()?;
        loop {
            let operator = if self.take_symbol("*") {
                '*'
            } else if self.take_symbol("/") {
                '/'
            } else {
                break;
            };
            let right = self.power()?;
            left = Expression::Binary(operator, Box::new(left), Box::new(right));
        }
        Some(left)
    }

    fn power(&mut self) -> Option<Expression> {
        let mut left = self.unary()?;
        while self.take_symbol("^") {
            let right = self.unary()?;
            left = Expression::Binary('^', Box::new(left), Box::new(right));
        }
        Some(left)
    }

    fn unary(&mut self) -> Option<Expression> {
        self.deeper()?;
        let parsed = if self.take_symbol("-") {
            Expression::Negate(Box::new(self.unary()?))
        } else if self.take_symbol("+") {
            self.unary()?
        } else {
            let mut primary = self.primary()?;
            while self.take_symbol("%") {
                primary = Expression::Percent(Box::new(primary));
            }
            primary
        };
        self.depth -= 1;
        Some(parsed)
    }

    fn primary(&mut self) -> Option<Expression> {
        let token = self.peek()?.clone();
        self.at += 1;
        match token {
            Token::Number(number) => Some(Expression::Literal(Value::Number(number))),
            Token::Text(text) => Some(Expression::Literal(Value::Text(text))),
            Token::Symbol("(") => {
                let inner = self.comparison()?;
                self.take_symbol(")").then_some(inner)
            }
            Token::Word(word) => {
                if self.take_symbol("(") {
                    let mut arguments = Vec::new();
                    if !self.take_symbol(")") {
                        loop {
                            arguments.push(self.comparison()?);
                            if self.take_symbol(")") {
                                break;
                            }
                            if !self.take_symbol(",") {
                                return None;
                            }
                        }
                    }
                    return Some(Expression::Call(word.to_ascii_uppercase(), arguments));
                }
                if word.eq_ignore_ascii_case("TRUE") {
                    return Some(Expression::Literal(Value::Boolean(true)));
                }
                if word.eq_ignore_ascii_case("FALSE") {
                    return Some(Expression::Literal(Value::Boolean(false)));
                }
                let first = parse_cell(&word)?;
                if self.take_symbol(":") {
                    let Some(Token::Word(second)) = self.peek().cloned() else {
                        return None;
                    };
                    self.at += 1;
                    let second = parse_cell(&second)?;
                    return Some(Expression::Range(first, second));
                }
                Some(Expression::Reference(first))
            }
            Token::Symbol(_) => None,
        }
    }
}

fn parse_formula(formula: &str) -> Option<Expression> {
    if formula.len() > MOST_FORMULA_BYTES {
        return None;
    }
    let mut parser = Parser { tokens: tokens(formula)?, at: 0, depth: 0 };
    let expression = parser.comparison()?;
    (parser.at == parser.tokens.len()).then_some(expression)
}

fn range_cells(first: Cell, second: Cell) -> Option<Vec<Cell>> {
    let (left, right) = (first.column.min(second.column), first.column.max(second.column));
    let (top, bottom) = (first.row.min(second.row), first.row.max(second.row));
    let area = (right - left + 1) as usize * (bottom - top + 1) as usize;
    if area > MOST_RANGE_CELLS {
        return None;
    }
    Some((top..=bottom).flat_map(|row| (left..=right).map(move |column| Cell { column, row })).collect())
}

fn references(expression: &Expression, found: &mut Vec<Cell>) -> bool {
    match expression {
        Expression::Literal(_) => true,
        Expression::Reference(cell) => {
            found.push(*cell);
            true
        }
        Expression::Range(first, second) => match range_cells(*first, *second) {
            Some(cells) => {
                found.extend(cells);
                true
            }
            None => false,
        },
        Expression::Negate(inner) | Expression::Percent(inner) => references(inner, found),
        Expression::Binary(_, left, right) | Expression::Compare(_, left, right) => references(left, found) && references(right, found),
        Expression::Call(_, arguments) => arguments.iter().all(|argument| references(argument, found)),
    }
}

fn number_of(value: &Value) -> Result<f64, &'static str> {
    match value {
        Value::Empty => Ok(0.0),
        Value::Number(number) => Ok(*number),
        Value::Boolean(flag) => Ok(if *flag { 1.0 } else { 0.0 }),
        Value::Text(text) => text.trim().parse().map_err(|_| "#VALUE!"),
        Value::Error(error) => Err(error),
    }
}

fn text_of(value: &Value) -> Result<String, &'static str> {
    match value {
        Value::Empty => Ok(String::new()),
        Value::Number(number) => Ok(number_text(*number)),
        Value::Boolean(flag) => Ok(if *flag { "TRUE" } else { "FALSE" }.to_owned()),
        Value::Text(text) => Ok(text.clone()),
        Value::Error(error) => Err(error),
    }
}

fn truth_of(value: &Value) -> Result<bool, &'static str> {
    match value {
        Value::Text(text) if text.eq_ignore_ascii_case("TRUE") => Ok(true),
        Value::Text(text) if text.eq_ignore_ascii_case("FALSE") => Ok(false),
        Value::Text(_) => Err("#VALUE!"),
        other => number_of(other).map(|number| number != 0.0),
    }
}

fn number_text(number: f64) -> String {
    if number == number.trunc() && number.abs() < 1e15 {
        format!("{}", number as i64)
    } else {
        format!("{number}")
    }
}

fn finite(number: f64) -> Value {
    if number.is_finite() { Value::Number(number) } else { Value::Error("#NUM!") }
}

struct Sheet<'a> {
    computed: &'a HashMap<Cell, Value>,
}

impl Sheet<'_> {
    fn cell(&self, cell: Cell) -> Value {
        self.computed.get(&cell).cloned().unwrap_or(Value::Empty)
    }

    fn flattened(&self, arguments: &[Expression]) -> Result<Vec<Value>, &'static str> {
        let mut values = Vec::new();
        for argument in arguments {
            match argument {
                Expression::Range(first, second) => {
                    let cells = range_cells(*first, *second).ok_or("#REF!")?;
                    values.extend(cells.into_iter().map(|cell| self.cell(cell)));
                }
                other => values.push(self.evaluate(other)),
            }
        }
        Ok(values)
    }

    fn numbers(&self, arguments: &[Expression]) -> Result<Vec<f64>, &'static str> {
        let mut numbers = Vec::new();
        for value in self.flattened(arguments)? {
            match value {
                Value::Number(number) => numbers.push(number),
                Value::Error(error) => return Err(error),
                _ => {}
            }
        }
        Ok(numbers)
    }

    fn call(&self, name: &str, arguments: &[Expression]) -> Result<Value, &'static str> {
        let argument = |index: usize| arguments.get(index).map(|argument| self.evaluate(argument)).ok_or("#VALUE!");
        match name {
            "SUM" => Ok(finite(self.numbers(arguments)?.iter().sum())),
            "AVERAGE" => {
                let numbers = self.numbers(arguments)?;
                if numbers.is_empty() {
                    return Err("#DIV/0!");
                }
                Ok(finite(numbers.iter().sum::<f64>() / numbers.len() as f64))
            }
            "MIN" => Ok(Value::Number(self.numbers(arguments)?.into_iter().reduce(f64::min).unwrap_or(0.0))),
            "MAX" => Ok(Value::Number(self.numbers(arguments)?.into_iter().reduce(f64::max).unwrap_or(0.0))),
            "COUNT" => Ok(Value::Number(self.numbers(arguments)?.len() as f64)),
            "COUNTA" => Ok(Value::Number(self.flattened(arguments)?.iter().filter(|value| **value != Value::Empty).count() as f64)),
            "IF" => {
                if !(2..=3).contains(&arguments.len()) {
                    return Err("#VALUE!");
                }
                if truth_of(&argument(0)?)? {
                    Ok(argument(1)?)
                } else if arguments.len() == 3 {
                    Ok(argument(2)?)
                } else {
                    Ok(Value::Boolean(false))
                }
            }
            "IFERROR" => match argument(0)? {
                Value::Error(_) => argument(1),
                value => Ok(value),
            },
            "AND" | "OR" => {
                let mut result = name == "AND";
                for value in self.flattened(arguments)? {
                    if value == Value::Empty {
                        continue;
                    }
                    let truth = truth_of(&value)?;
                    result = if name == "AND" { result && truth } else { result || truth };
                }
                Ok(Value::Boolean(result))
            }
            "NOT" => Ok(Value::Boolean(!truth_of(&argument(0)?)?)),
            "ABS" => Ok(Value::Number(number_of(&argument(0)?)?.abs())),
            "SQRT" => {
                let number = number_of(&argument(0)?)?;
                if number < 0.0 { Err("#NUM!") } else { Ok(Value::Number(number.sqrt())) }
            }
            "ROUND" => {
                let number = number_of(&argument(0)?)?;
                let places = if arguments.len() > 1 { number_of(&argument(1)?)? } else { 0.0 };
                if !(-15.0..=15.0).contains(&places) {
                    return Err("#NUM!");
                }
                let scale = 10f64.powi(places.trunc() as i32);
                Ok(finite((number * scale).round() / scale))
            }
            "CONCAT" | "CONCATENATE" => {
                let mut joined = String::new();
                for value in self.flattened(arguments)? {
                    joined.push_str(&text_of(&value)?);
                }
                Ok(Value::Text(joined))
            }
            "LEN" => Ok(Value::Number(text_of(&argument(0)?)?.chars().count() as f64)),
            "UPPER" => Ok(Value::Text(text_of(&argument(0)?)?.to_uppercase())),
            "LOWER" => Ok(Value::Text(text_of(&argument(0)?)?.to_lowercase())),
            _ => Err("#NAME?"),
        }
    }

    fn compare(operator: &str, left: &Value, right: &Value) -> Result<bool, &'static str> {
        let ordering = match (left, right) {
            (Value::Error(error), _) | (_, Value::Error(error)) => return Err(error),
            (Value::Text(left), Value::Text(right)) => left.to_lowercase().cmp(&right.to_lowercase()),
            (Value::Text(_), _) => std::cmp::Ordering::Greater,
            (_, Value::Text(_)) => std::cmp::Ordering::Less,
            (left, right) => number_of(left)?.partial_cmp(&number_of(right)?).ok_or("#NUM!")?,
        };
        Ok(match operator {
            "=" => ordering.is_eq(),
            "<>" => ordering.is_ne(),
            "<" => ordering.is_lt(),
            "<=" => ordering.is_le(),
            ">" => ordering.is_gt(),
            _ => ordering.is_ge(),
        })
    }

    fn evaluate(&self, expression: &Expression) -> Value {
        let result = match expression {
            Expression::Literal(value) => Ok(value.clone()),
            Expression::Reference(cell) => match self.cell(*cell) {
                Value::Error(error) => Err(error),
                value => Ok(value),
            },
            Expression::Range(..) => Err("#VALUE!"),
            Expression::Negate(inner) => number_of(&self.evaluate(inner)).map(|number| Value::Number(-number)),
            Expression::Percent(inner) => number_of(&self.evaluate(inner)).map(|number| Value::Number(number / 100.0)),
            Expression::Compare(operator, left, right) => Self::compare(operator, &self.evaluate(left), &self.evaluate(right)).map(Value::Boolean),
            Expression::Binary('&', left, right) => text_of(&self.evaluate(left)).and_then(|left| text_of(&self.evaluate(right)).map(|right| Value::Text(left + &right))),
            Expression::Binary(operator, left, right) => number_of(&self.evaluate(left)).and_then(|left| {
                let right = number_of(&self.evaluate(right))?;
                match operator {
                    '+' => Ok(finite(left + right)),
                    '-' => Ok(finite(left - right)),
                    '*' => Ok(finite(left * right)),
                    '/' if right == 0.0 => Err("#DIV/0!"),
                    '/' => Ok(finite(left / right)),
                    _ => Ok(finite(left.powf(right))),
                }
            }),
            Expression::Call(name, arguments) => self.call(name, arguments),
        };
        result.unwrap_or_else(Value::Error)
    }
}

enum Content {
    Constant(Value),
    Formula(Option<Expression>),
}

fn content_of(node: &Json) -> Content {
    match node {
        Json::Null => Content::Constant(Value::Empty),
        Json::Boolean(flag) => Content::Constant(Value::Boolean(*flag)),
        Json::Number(text) => Content::Constant(text.parse().map(Value::Number).unwrap_or(Value::Error("#NUM!"))),
        Json::String(text) => match text.strip_prefix('=') {
            Some(formula) => Content::Formula(parse_formula(formula)),
            None => match text.trim().parse::<f64>() {
                Ok(number) if !text.trim().is_empty() && number.is_finite() => Content::Constant(Value::Number(number)),
                _ if text.eq_ignore_ascii_case("TRUE") => Content::Constant(Value::Boolean(true)),
                _ if text.eq_ignore_ascii_case("FALSE") => Content::Constant(Value::Boolean(false)),
                _ => Content::Constant(Value::Text(text.clone())),
            },
        },
        Json::Array(_) | Json::Object(_) => Content::Constant(Value::Error("#VALUE!")),
    }
}

fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            control if (control as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", control as u32)),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

fn value_json(value: &Value) -> String {
    match value {
        Value::Empty => "null".to_owned(),
        Value::Number(number) if number.is_finite() => number_text(*number),
        Value::Number(_) => json_string("#NUM!"),
        Value::Text(text) => json_string(text),
        Value::Boolean(flag) => flag.to_string(),
        Value::Error(error) => json_string(error),
    }
}

/// Recalculates a sheet given as a JSON object of cells (`{"A1": 10, "B1": "=A1*2"}`): numbers,
/// text, booleans and formulas that start with `=`. Returns every cell's value as a JSON object
/// in row, then column order, with spreadsheet errors (`#DIV/0!`, `#REF!`, `#NAME?`,
/// `#VALUE!`, `#NUM!`, `#CYCLE!`) as strings. Cells are evaluated in dependency order without
/// recursion, so long chains cannot exhaust the stack.
pub(crate) fn recalculate(sheet_json: &str) -> SqlResult<String> {
    let invalid = |message: String| SqlError::new(SqlState::INVALID_PARAMETER_VALUE, message);
    let document = parse_document(sheet_json).map_err(|error| invalid(format!("spreadsheet_recalculate: {}", error.message)))?;
    let Json::Object(members) = document.value else {
        return Err(invalid("spreadsheet_recalculate takes an object of cells".to_owned()));
    };
    if members.len() > MOST_CELLS {
        return Err(SqlError::new(SqlState::PROGRAM_LIMIT_EXCEEDED, format!("spreadsheet_recalculate takes at most {MOST_CELLS} cells")));
    }
    let mut contents: HashMap<Cell, Content> = HashMap::with_capacity(members.len());
    for (name, node) in &members {
        let cell = parse_cell(name).ok_or_else(|| invalid(format!("spreadsheet_recalculate: \"{name}\" is not a cell name such as A1")))?;
        contents.insert(cell, content_of(&node.value));
    }
    let mut dependencies: HashMap<Cell, Vec<Cell>> = HashMap::new();
    let mut computed: HashMap<Cell, Value> = HashMap::with_capacity(contents.len());
    for (cell, content) in &contents {
        match content {
            Content::Constant(value) => {
                computed.insert(*cell, value.clone());
            }
            Content::Formula(None) => {
                computed.insert(*cell, Value::Error("#NAME?"));
            }
            Content::Formula(Some(expression)) => {
                let mut found = Vec::new();
                if references(expression, &mut found) {
                    found.retain(|referenced| matches!(contents.get(referenced), Some(Content::Formula(Some(_)))));
                    found.sort_unstable();
                    found.dedup();
                    dependencies.insert(*cell, found);
                } else {
                    computed.insert(*cell, Value::Error("#REF!"));
                    dependencies.insert(*cell, Vec::new());
                }
            }
        }
    }
    let mut waiting: HashMap<Cell, usize> = HashMap::with_capacity(dependencies.len());
    let mut dependents: HashMap<Cell, Vec<Cell>> = HashMap::new();
    for (cell, needed) in &dependencies {
        waiting.insert(*cell, needed.len());
        for referenced in needed {
            dependents.entry(*referenced).or_default().push(*cell);
        }
    }
    let mut ready: VecDeque<Cell> = waiting.iter().filter(|(_, count)| **count == 0).map(|(cell, _)| *cell).collect();
    let mut ordered: Vec<Cell> = ready.iter().copied().collect();
    ordered.sort_unstable();
    ready = ordered.into_iter().collect();
    while let Some(cell) = ready.pop_front() {
        if let (false, Some(Content::Formula(Some(expression)))) = (computed.contains_key(&cell), contents.get(&cell)) {
            let value = Sheet { computed: &computed }.evaluate(expression);
            computed.insert(cell, value);
        }
        if let Some(next) = dependents.get(&cell) {
            for dependent in next {
                if let Some(count) = waiting.get_mut(dependent) {
                    *count -= 1;
                    if *count == 0 {
                        ready.push_back(*dependent);
                    }
                }
            }
        }
    }
    for cell in dependencies.keys() {
        computed.entry(*cell).or_insert(Value::Error("#CYCLE!"));
    }
    let mut cells: Vec<&Cell> = computed.keys().collect();
    cells.sort_unstable_by_key(|cell| (cell.row, cell.column));
    let mut out = String::from("{");
    for (index, cell) in cells.into_iter().enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        out.push_str(&json_string(&cell.name()));
        out.push_str(": ");
        out.push_str(&value_json(&computed[cell]));
    }
    out.push('}');
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formulas_follow_their_references_in_any_order() {
        assert_eq!(
            recalculate(r#"{"C1": "=A1*B1", "A1": 3, "B1": "4", "D1": "=SUM(A1:C1)", "A2": "=IF(D1>10, \"big\", \"small\") & \"!\""}"#).unwrap(),
            r#"{"A1": 3, "B1": 4, "C1": 12, "D1": 19, "A2": "big!"}"#
        );
    }

    #[test]
    fn errors_cycles_and_long_chains() {
        assert_eq!(recalculate(r#"{"A1": "=1/0", "B1": "=A1+1", "C1": "=IFERROR(A1, 0)"}"#).unwrap(), r##"{"A1": "#DIV/0!", "B1": "#DIV/0!", "C1": 0}"##);
        assert_eq!(recalculate(r#"{"A1": "=B1", "B1": "=A1", "C1": "=NOPE(1)"}"#).unwrap(), r##"{"A1": "#CYCLE!", "B1": "#CYCLE!", "C1": "#NAME?"}"##);
        let mut chain = String::from(r#"{"A1": 1"#);
        for row in 2..=20_000 {
            chain.push_str(&format!(r#", "A{row}": "=A{}+1""#, row - 1));
        }
        chain.push('}');
        assert!(recalculate(&chain).unwrap().ends_with(r#""A20000": 20000}"#));
        assert!(recalculate(r#"{"not a cell": 1}"#).is_err());
    }
}
