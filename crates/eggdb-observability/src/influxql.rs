use crate::api::json_string;
use crate::store::{SCHEMA, identifier, literal, run};
use eggdb_sql::{QueryResult, Session, Value};

const UNIX_EPOCH_FROM_2000_MICROS: i64 = 946_684_800_000_000;
const MICROS_PER_DAY: i64 = 86_400_000_000;
const STOP_WORDS: [&str; 7] = ["GROUP", "ORDER", "LIMIT", "OFFSET", "SLIMIT", "SOFFSET", "TZ"];
const MOST_FILLED_BUCKETS: i64 = 100_000;

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Name { text: String, quoted: bool },
    Text(String),
    Number(String),
    Duration(String, i64),
    Regex(String),
    Operator(String),
    Open,
    Close,
    Comma,
    Star,
}

impl Token {
    fn keyword(&self, word: &str) -> bool {
        matches!(self, Token::Name { text, quoted: false } if text.eq_ignore_ascii_case(word))
    }
}

fn duration_sql(number: &str, unit: &str) -> Option<(String, i64)> {
    let (amount, unit, micros_per_unit) = match unit {
        "ns" => (number.parse::<f64>().ok()? / 1000.0, "microseconds", 1.0),
        "u" | "µ" | "µs" | "us" => (number.parse().ok()?, "microseconds", 1.0),
        "ms" => (number.parse().ok()?, "milliseconds", 1e3),
        "s" => (number.parse().ok()?, "seconds", 1e6),
        "m" => (number.parse().ok()?, "minutes", 6e7),
        "h" => (number.parse().ok()?, "hours", 3.6e9),
        "d" => (number.parse().ok()?, "days", 8.64e10),
        "w" => (number.parse().ok()?, "weeks", 6.048e11),
        _ => return None,
    };
    let micros: f64 = amount * micros_per_unit;
    Some((format!("'{amount} {unit}'::interval"), micros.round() as i64))
}

fn regex_may_start(found: &[Token]) -> bool {
    match found.last() {
        Some(Token::Operator(operator)) => operator == "=~" || operator == "!~",
        Some(token) if token.keyword("from") => true,
        Some(Token::Comma) => found
            .iter()
            .rev()
            .find_map(|token| {
                if token.keyword("from") {
                    Some(true)
                } else if ["select", "where", "group", "by"].iter().any(|word| token.keyword(word)) {
                    Some(false)
                } else {
                    None
                }
            })
            .unwrap_or(false),
        _ => false,
    }
}

fn tokens(text: &str) -> Result<Vec<Token>, String> {
    let characters: Vec<char> = text.chars().collect();
    let mut found = Vec::new();
    let mut at = 0;
    while at < characters.len() {
        let character = characters[at];
        if character.is_whitespace() {
            at += 1;
            continue;
        }
        let quoted_until = |at: usize, quote: char| -> Result<(String, usize), String> {
            let mut out = String::new();
            let mut index = at + 1;
            while index < characters.len() {
                match characters[index] {
                    '\\' if index + 1 < characters.len() => {
                        out.push(characters[index + 1]);
                        index += 2;
                    }
                    found if found == quote => return Ok((out, index + 1)),
                    other => {
                        out.push(other);
                        index += 1;
                    }
                }
            }
            Err(format!("a quoted {quote} is not closed"))
        };
        match character {
            '"' => {
                let (text, next) = quoted_until(at, '"')?;
                found.push(Token::Name { text, quoted: true });
                at = next;
            }
            '\'' => {
                let (text, next) = quoted_until(at, '\'')?;
                found.push(Token::Text(text));
                at = next;
            }
            '/' if regex_may_start(&found) => {
                let (text, next) = quoted_until(at, '/')?;
                found.push(Token::Regex(text));
                at = next;
            }
            '(' => {
                found.push(Token::Open);
                at += 1;
            }
            ')' => {
                found.push(Token::Close);
                at += 1;
            }
            ',' => {
                found.push(Token::Comma);
                at += 1;
            }
            '*' => {
                found.push(Token::Star);
                at += 1;
            }
            digit if digit.is_ascii_digit() || (digit == '.' && characters.get(at + 1).is_some_and(char::is_ascii_digit)) => {
                let start = at;
                while at < characters.len() && (characters[at].is_ascii_digit() || matches!(characters[at], '.' | 'e' | 'E')) {
                    at += 1;
                }
                let number: String = characters[start..at].iter().collect();
                let unit_start = at;
                while at < characters.len() && (characters[at].is_alphabetic()) {
                    at += 1;
                }
                let unit: String = characters[unit_start..at].iter().collect();
                if unit.is_empty() {
                    found.push(Token::Number(number));
                } else {
                    let (sql, micros) =
                        duration_sql(&number, &unit).ok_or_else(|| format!("invalid duration {number}{unit}"))?;
                    found.push(Token::Duration(sql, micros));
                }
            }
            letter if letter.is_alphabetic() || letter == '_' => {
                let start = at;
                while at < characters.len() && (characters[at].is_alphanumeric() || matches!(characters[at], '_' | '.')) {
                    at += 1;
                }
                found.push(Token::Name {
                    text: characters[start..at].iter().collect(),
                    quoted: false,
                });
            }
            _ => {
                let two: String = characters[at..(at + 2).min(characters.len())].iter().collect();
                let operator = match two.as_str() {
                    "!=" | "<>" | "<=" | ">=" | "=~" | "!~" | "::" => two,
                    _ if matches!(character, '=' | '<' | '>' | '+' | '-' | '/' | '%') => character.to_string(),
                    _ => return Err(format!("unexpected {character}")),
                };
                at += operator.chars().count();
                found.push(Token::Operator(operator));
            }
        }
    }
    Ok(found)
}

fn column(name: &str) -> String {
    identifier(name)
}

fn condition_sql(tokens: &[Token]) -> Result<String, String> {
    let mut out = Vec::new();
    let mut previous_was_time = false;
    let mut index = 0;
    while index < tokens.len() {
        let token = &tokens[index];
        let piece = match token {
            Token::Name { text, quoted: false } if text.eq_ignore_ascii_case("and") || text.eq_ignore_ascii_case("or") => {
                text.to_ascii_uppercase()
            }
            Token::Name { text, quoted: false } if text.eq_ignore_ascii_case("now") && tokens.get(index + 1) == Some(&Token::Open) => {
                index += 2;
                if tokens.get(index) != Some(&Token::Close) {
                    return Err("now() takes no arguments".to_owned());
                }
                "now()".to_owned()
            }
            Token::Name { text, .. } => {
                previous_was_time = text == "time";
                index += 1;
                out.push(column(text));
                continue;
            }
            Token::Text(text) => literal(text),
            Token::Number(number) if previous_was_time => format!("to_timestamp({number}::double precision / 1e9)"),
            Token::Number(number) => number.clone(),
            Token::Duration(interval, _) => interval.clone(),
            Token::Regex(pattern) => literal(pattern),
            Token::Operator(operator) => match operator.as_str() {
                "=~" => "~".to_owned(),
                "!~" => "!~".to_owned(),
                "::" => return Err("type casts are not supported in WHERE".to_owned()),
                other => other.to_owned(),
            },
            Token::Open => "(".to_owned(),
            Token::Close => ")".to_owned(),
            Token::Comma => ",".to_owned(),
            Token::Star => "*".to_owned(),
        };
        if !matches!(token, Token::Operator(_)) {
            previous_was_time = previous_was_time && matches!(token, Token::Operator(_));
        }
        out.push(piece);
        index += 1;
    }
    Ok(out.join(" "))
}

#[derive(Debug, Clone, PartialEq)]
enum Expr {
    Field(String),
    Number(String),
    Call { function: String, field: Option<String>, argument: Option<String> },
    Binary(Box<Expr>, char, Box<Expr>),
}

impl Expr {
    fn calls(&self) -> bool {
        match self {
            Expr::Call { .. } => true,
            Expr::Binary(left, _, right) => left.calls() || right.calls(),
            Expr::Field(_) | Expr::Number(_) => false,
        }
    }

    fn reads_fields(&self) -> bool {
        match self {
            Expr::Field(_) => true,
            Expr::Binary(left, _, right) => left.reads_fields() || right.reads_fields(),
            Expr::Call { .. } | Expr::Number(_) => false,
        }
    }

    fn names(&self, out: &mut Vec<String>) {
        match self {
            Expr::Field(name) => out.push(name.clone()),
            Expr::Call { function, .. } => out.push(function.clone()),
            Expr::Binary(left, _, right) => {
                left.names(out);
                right.names(out);
            }
            Expr::Number(_) => {}
        }
    }
}

fn operator_of(token: &Token) -> Option<(char, u8)> {
    match token {
        Token::Star => Some(('*', 2)),
        Token::Operator(operator) => match operator.as_str() {
            "+" => Some(('+', 1)),
            "-" => Some(('-', 1)),
            "/" => Some(('/', 2)),
            "%" => Some(('%', 2)),
            _ => None,
        },
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Item {
    All,
    Field { name: String, alias: Option<String> },
    Call { function: String, field: Option<String>, argument: Option<String>, alias: Option<String> },
    Math { expr: Expr, alias: Option<String> },
}

impl Item {
    fn aggregates(&self) -> bool {
        match self {
            Item::Call { .. } => true,
            Item::Math { expr, .. } => expr.calls(),
            Item::All | Item::Field { .. } => false,
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq)]
enum Fill {
    #[default]
    Null,
    None,
    Previous,
    Linear,
    Number(f64),
}

#[derive(Debug, Default)]
struct Bucket {
    interval: String,
    micros: i64,
    offset_sql: Option<String>,
    offset_micros: i64,
}

#[derive(Debug, Clone, PartialEq)]
enum Source {
    Name(String),
    Pattern(String),
}

#[derive(Debug, Default)]
struct Select {
    items: Vec<Item>,
    sources: Vec<Source>,
    group_all: bool,
    series_limit: Option<usize>,
    series_offset: Option<usize>,
    condition: Option<String>,
    lower_time: Option<String>,
    upper_time: Option<String>,
    bucket: Option<Bucket>,
    fill: Fill,
    group_tags: Vec<String>,
    descending: bool,
    limit: Option<String>,
    offset: Option<String>,
}

fn bound_sql(tokens: &[Token]) -> Option<String> {
    match tokens {
        [Token::Number(number)] => Some(format!("to_timestamp({number}::double precision / 1e9)")),
        [Token::Text(text)] => Some(format!("{}::timestamptz", literal(text))),
        _ => condition_sql(tokens).ok(),
    }
}

fn time_bounds(tokens: &[Token]) -> (Option<String>, Option<String>) {
    let mut conjuncts: Vec<&[Token]> = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    for (index, token) in tokens.iter().enumerate() {
        match token {
            Token::Open => depth += 1,
            Token::Close => depth = depth.saturating_sub(1),
            _ if depth == 0 && token.keyword("or") => return (None, None),
            _ if depth == 0 && token.keyword("and") => {
                conjuncts.push(&tokens[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    conjuncts.push(&tokens[start..]);
    let (mut lower, mut upper) = (None, None);
    for conjunct in conjuncts {
        let [Token::Name { text, .. }, Token::Operator(operator), rest @ ..] = conjunct else {
            continue;
        };
        if text != "time" || rest.is_empty() {
            continue;
        }
        match operator.as_str() {
            ">" | ">=" => lower = bound_sql(rest),
            "<" | "<=" => upper = bound_sql(rest),
            _ => {}
        }
    }
    (lower, upper)
}

struct Reader {
    tokens: Vec<Token>,
    at: usize,
}

impl Reader {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at)
    }

    fn next(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.at).cloned();
        self.at += 1;
        token
    }

    fn keyword(&mut self, word: &str) -> bool {
        if self.peek().is_some_and(|token| token.keyword(word)) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn expect_keyword(&mut self, word: &str) -> Result<(), String> {
        if self.keyword(word) {
            Ok(())
        } else {
            Err(format!("found {:?}, expected {word}", self.peek()))
        }
    }

    fn name(&mut self) -> Result<String, String> {
        match self.next() {
            Some(Token::Name { text, .. }) => Ok(text),
            other => Err(format!("found {other:?}, expected an identifier")),
        }
    }

    fn alias(&mut self) -> Result<Option<String>, String> {
        if self.keyword("as") { self.name().map(Some) } else { Ok(None) }
    }

    fn skip_cast(&mut self) {
        if self.peek() == Some(&Token::Operator("::".to_owned())) {
            self.at += 2;
        }
    }

    fn item(&mut self) -> Result<Item, String> {
        if self.peek() == Some(&Token::Star) {
            self.at += 1;
            return Ok(Item::All);
        }
        let expr = self.expression(0)?;
        let alias = self.alias()?;
        Ok(match expr {
            Expr::Field(name) => Item::Field { name, alias },
            Expr::Call { function, field, argument } => Item::Call { function, field, argument, alias },
            expr => Item::Math { expr, alias },
        })
    }

    fn expression(&mut self, lowest: u8) -> Result<Expr, String> {
        let mut left = self.operand()?;
        while let Some((operator, precedence)) = self.peek().and_then(operator_of) {
            if precedence < lowest {
                break;
            }
            self.at += 1;
            let right = self.expression(precedence + 1)?;
            left = Expr::Binary(Box::new(left), operator, Box::new(right));
        }
        Ok(left)
    }

    fn operand(&mut self) -> Result<Expr, String> {
        match self.peek().cloned() {
            Some(Token::Number(number)) => {
                self.at += 1;
                Ok(Expr::Number(number))
            }
            Some(Token::Operator(operator)) if operator == "-" => {
                self.at += 1;
                match self.next() {
                    Some(Token::Number(number)) => Ok(Expr::Number(format!("-{number}"))),
                    other => Err(format!("found {other:?} after -")),
                }
            }
            Some(Token::Open) => {
                self.at += 1;
                let inner = self.expression(0)?;
                if self.next() != Some(Token::Close) {
                    return Err("a parenthesis is not closed".to_owned());
                }
                Ok(inner)
            }
            _ => match self.call_or_field()? {
                Item::Field { name, .. } => Ok(Expr::Field(name)),
                Item::Call { function, field, argument, .. } => Ok(Expr::Call { function, field, argument }),
                _ => Err("expected a field or a function".to_owned()),
            },
        }
    }

    fn call_or_field(&mut self) -> Result<Item, String> {
        let name = self.name()?;
        if self.peek() == Some(&Token::Open) {
            self.at += 1;
            let field = match self.next() {
                Some(Token::Star) => None,
                Some(Token::Name { text, .. }) => Some(text),
                other => return Err(format!("found {other:?} in {name}()")),
            };
            self.skip_cast();
            let argument = if self.peek() == Some(&Token::Comma) {
                self.at += 1;
                match self.next() {
                    Some(Token::Number(number)) => Some(number),
                    other => return Err(format!("found {other:?} as the second argument of {name}()")),
                }
            } else {
                None
            };
            if self.next() != Some(Token::Close) {
                return Err(format!("{name}() is not closed"));
            }
            return Ok(Item::Call {
                function: name.to_ascii_lowercase(),
                field,
                argument,
                alias: None,
            });
        }
        self.skip_cast();
        Ok(Item::Field { name, alias: None })
    }

    fn select(&mut self) -> Result<Select, String> {
        let mut select = Select::default();
        loop {
            select.items.push(self.item()?);
            if self.peek() == Some(&Token::Comma) {
                self.at += 1;
            } else {
                break;
            }
        }
        self.expect_keyword("from")?;
        loop {
            select.sources.push(match self.next() {
                Some(Token::Name { text, quoted: true }) => Source::Name(text),
                Some(Token::Name { text, quoted: false }) => {
                    Source::Name(text.rsplit('.').next().unwrap_or_default().to_owned())
                }
                Some(Token::Regex(pattern)) => Source::Pattern(pattern),
                other => return Err(format!("found {other:?}, expected a measurement")),
            });
            if self.peek() == Some(&Token::Comma) {
                self.at += 1;
            } else {
                break;
            }
        }
        if self.keyword("where") {
            let start = self.at;
            let mut depth = 0usize;
            while let Some(token) = self.peek() {
                match token {
                    Token::Open => depth += 1,
                    Token::Close => depth = depth.saturating_sub(1),
                    _ if depth == 0 && STOP_WORDS.iter().any(|word| token.keyword(word)) => break,
                    _ => {}
                }
                self.at += 1;
            }
            let condition = &self.tokens[start..self.at];
            select.condition = Some(condition_sql(condition)?);
            (select.lower_time, select.upper_time) = time_bounds(condition);
        }
        if self.keyword("group") {
            self.expect_keyword("by")?;
            loop {
                if self.peek().is_some_and(|token| token.keyword("time")) && self.tokens.get(self.at + 1) == Some(&Token::Open) {
                    self.at += 2;
                    let Some(Token::Duration(interval, micros)) = self.next() else {
                        return Err("GROUP BY time() needs a duration".to_owned());
                    };
                    let mut bucket = Bucket {
                        interval,
                        micros,
                        ..Bucket::default()
                    };
                    if self.peek() == Some(&Token::Comma) {
                        self.at += 1;
                        let negative = self.peek() == Some(&Token::Operator("-".to_owned()));
                        if negative {
                            self.at += 1;
                        }
                        let Some(Token::Duration(offset, offset_micros)) = self.next() else {
                            return Err("a GROUP BY time() offset must be a duration".to_owned());
                        };
                        bucket.offset_sql = Some(format!("{} {offset}", if negative { "-" } else { "+" }));
                        bucket.offset_micros = if negative { -offset_micros } else { offset_micros };
                    }
                    if self.next() != Some(Token::Close) {
                        return Err("GROUP BY time( is not closed".to_owned());
                    }
                    select.bucket = Some(bucket);
                } else if self.peek() == Some(&Token::Star) {
                    self.at += 1;
                    select.group_all = true;
                } else {
                    select.group_tags.push(self.name()?);
                }
                if self.peek() == Some(&Token::Comma) {
                    self.at += 1;
                } else {
                    break;
                }
            }
            if self.keyword("fill") {
                if self.next() != Some(Token::Open) {
                    return Err("fill needs ( ".to_owned());
                }
                let negative = self.peek() == Some(&Token::Operator("-".to_owned()));
                if negative {
                    self.at += 1;
                }
                select.fill = match self.next() {
                    Some(Token::Number(number)) => {
                        let value: f64 = number.parse().map_err(|_| format!("invalid fill value {number}"))?;
                        Fill::Number(if negative { -value } else { value })
                    }
                    Some(token) if token.keyword("none") => Fill::None,
                    Some(token) if token.keyword("null") => Fill::Null,
                    Some(token) if token.keyword("previous") => Fill::Previous,
                    Some(token) if token.keyword("linear") => Fill::Linear,
                    other => return Err(format!("found {other:?}, expected none, null, previous, linear or a number in fill()")),
                };
                if self.next() != Some(Token::Close) {
                    return Err("fill( is not closed".to_owned());
                }
            }
        }
        if self.keyword("order") {
            self.expect_keyword("by")?;
            self.expect_keyword("time")?;
            if self.keyword("desc") {
                select.descending = true;
            } else {
                self.keyword("asc");
            }
        }
        while let Some(token) = self.peek().cloned() {
            let target = if token.keyword("limit") {
                &mut select.limit
            } else if token.keyword("offset") {
                &mut select.offset
            } else if token.keyword("slimit") || token.keyword("soffset") {
                self.at += 1;
                let count = match self.next() {
                    Some(Token::Number(number)) => number.parse::<usize>().map_err(|_| format!("invalid count {number}"))?,
                    other => return Err(format!("found {other:?}, expected a whole number")),
                };
                if token.keyword("slimit") {
                    select.series_limit = Some(count);
                } else {
                    select.series_offset = Some(count);
                }
                continue;
            } else {
                return Err(format!("unexpected {token:?}"));
            };
            self.at += 1;
            match self.next() {
                Some(Token::Number(number)) if number.chars().all(|character| character.is_ascii_digit()) => {
                    *target = Some(number);
                }
                other => return Err(format!("found {other:?}, expected a whole number")),
            }
        }
        Ok(select)
    }
}

fn aggregate_sql(function: &str, field: &str, argument: Option<&str>) -> Result<String, String> {
    let column = column(field);
    Ok(match function {
        "mean" => format!("avg({column})"),
        "count" | "sum" | "min" | "max" => format!("{function}({column})"),
        "median" => format!("percentile_cont(0.5) WITHIN GROUP (ORDER BY {column})"),
        "percentile" => {
            let percent: f64 = argument.and_then(|text| text.parse().ok()).ok_or("percentile() needs a percentage")?;
            format!("percentile_cont({}) WITHIN GROUP (ORDER BY {column})", percent / 100.0)
        }
        "spread" => format!("(max({column}) - min({column}))"),
        "stddev" => format!("stddev_samp({column})"),
        "first" => format!("first({column}, \"time\")"),
        "last" => format!("last({column}, \"time\")"),
        other => return Err(format!("undefined function {other}()")),
    })
}

fn expr_sql(expr: &Expr) -> Result<String, String> {
    Ok(match expr {
        Expr::Field(name) => column(name),
        Expr::Number(number) => number.clone(),
        Expr::Call { function, .. } if function == "distinct" => {
            return Err("distinct() cannot be used in an expression".to_owned());
        }
        Expr::Call { function, field, argument } => {
            let field = match field {
                Some(field) => field.clone(),
                None if function == "count" => "time".to_owned(),
                None => return Err(format!("{function}(*) is not supported yet")),
            };
            aggregate_sql(function, &field, argument.as_deref())?
        }
        Expr::Binary(left, '/', right) => {
            format!("(({})::double precision / ({}))", expr_sql(left)?, expr_sql(right)?)
        }
        Expr::Binary(left, operator, right) => format!("({} {operator} {})", expr_sql(left)?, expr_sql(right)?),
    })
}

struct Planned {
    sql: String,
    tag_count: usize,
}

fn distinct_field(select: &Select) -> Option<(&str, Option<&str>)> {
    select.items.iter().find_map(|item| match item {
        Item::Call { function, field: Some(field), alias, .. } if function == "distinct" => {
            Some((field.as_str(), alias.as_deref()))
        }
        _ => None,
    })
}

fn plan(select: &Select, measurement: &str, columns: &[(String, String)]) -> Result<Planned, String> {
    let group_tags: Vec<String> = if select.group_all {
        columns
            .iter()
            .filter(|(name, kind)| name != "time" && kind == "text")
            .map(|(name, _)| name.clone())
            .collect()
    } else {
        select.group_tags.clone()
    };
    let distinct = distinct_field(select);
    if distinct.is_some() && select.items.len() != 1 {
        return Err("aggregate function distinct() cannot be combined with other functions or fields".to_owned());
    }
    let aggregate = select.items.iter().any(Item::aggregates);
    let mixed_expression = select
        .items
        .iter()
        .any(|item| matches!(item, Item::Math { expr, .. } if expr.calls() && expr.reads_fields()));
    if mixed_expression || (aggregate && select.items.iter().any(|item| !item.aggregates())) {
        return Err("mixing aggregate and non-aggregate queries is not supported".to_owned());
    }
    if select.bucket.is_some() && !aggregate {
        return Err("GROUP BY time() requires an aggregate function".to_owned());
    }
    let mut outputs = Vec::new();
    for tag in &group_tags {
        outputs.push(format!("{} AS {}", column(tag), column(tag)));
    }
    outputs.push(match (&select.bucket, aggregate) {
        (Some(bucket), _) => format!(
            "time_bucket({}, \"time\", '1970-01-01T00:00:00Z'::timestamptz{}) AS \"time\"",
            bucket.interval,
            bucket.offset_sql.as_deref().map_or(String::new(), |offset| format!(" {offset}"))
        ),
        (None, true) => "'1970-01-01T00:00:00Z'::timestamptz AS \"time\"".to_owned(),
        (None, false) => "\"time\"".to_owned(),
    });
    for item in &select.items {
        match item {
            Item::All => {
                for (name, _) in columns.iter().filter(|(name, _)| name != "time" && !group_tags.contains(name)) {
                    outputs.push(column(name));
                }
            }
            Item::Field { name, alias } => {
                outputs.push(format!("{} AS {}", column(name), column(alias.as_deref().unwrap_or(name))));
            }
            Item::Math { expr, alias } => {
                let name = alias.clone().unwrap_or_else(|| {
                    let mut names = Vec::new();
                    expr.names(&mut names);
                    if names.is_empty() { "value".to_owned() } else { names.join("_") }
                });
                outputs.push(format!("{} AS {}", expr_sql(expr)?, column(&name)));
            }
            Item::Call { function, .. } if function == "distinct" => {
                if let Some((field, alias)) = distinct {
                    outputs.push(format!("{} AS {}", column(field), column(alias.unwrap_or("distinct"))));
                } else {
                    return Err("distinct() needs a field".to_owned());
                }
            }
            Item::Call { function, field, argument, alias } => {
                let field = match field {
                    Some(field) => field.clone(),
                    None if function == "count" => "time".to_owned(),
                    None => return Err(format!("{function}(*) is not supported yet")),
                };
                outputs.push(format!(
                    "{} AS {}",
                    aggregate_sql(function, &field, argument.as_deref())?,
                    column(alias.as_deref().unwrap_or(function))
                ));
            }
        }
    }
    let mut sql = format!(
        "SELECT {} FROM {SCHEMA}.{}",
        outputs.join(", "),
        identifier(measurement)
    );
    if let Some(condition) = &select.condition {
        sql.push_str(&format!(" WHERE {condition}"));
    }
    let tag_positions: Vec<String> = (1..=group_tags.len()).map(|at| at.to_string()).collect();
    let time_position = group_tags.len() + 1;
    if aggregate {
        let mut keys = tag_positions.clone();
        if select.bucket.is_some() {
            keys.push(time_position.to_string());
        }
        if distinct.is_some() {
            keys.push((time_position + 1).to_string());
        }
        if !keys.is_empty() {
            sql.push_str(&format!(" GROUP BY {}", keys.join(", ")));
        }
    }
    let mut order = tag_positions;
    order.push(format!("{time_position}{}", if select.descending { " DESC" } else { "" }));
    if distinct.is_some() {
        order.push((time_position + 1).to_string());
    }
    sql.push_str(&format!(" ORDER BY {}", order.join(", ")));
    if !fills(select) {
        if let Some(limit) = &select.limit {
            sql.push_str(&format!(" LIMIT {limit}"));
        }
        if let Some(offset) = &select.offset {
            sql.push_str(&format!(" OFFSET {offset}"));
        }
    }
    Ok(Planned {
        sql,
        tag_count: group_tags.len(),
    })
}

fn fills(select: &Select) -> bool {
    select.fill != Fill::None
        && distinct_field(select).is_none()
        && select.bucket.as_ref().is_some_and(|bucket| bucket.micros > 0)
}

fn time_of(row: &[Value]) -> Option<i64> {
    match row.first() {
        Some(Value::TimestampTz(micros) | Value::Timestamp(micros)) => Some(*micros),
        _ => None,
    }
}

fn linear(known: &std::collections::BTreeMap<i64, Vec<Value>>, at: i64, column: usize) -> Value {
    let present = |row: &&Vec<Value>| !matches!(row.get(column), None | Some(Value::Null));
    let before = known.range(..at).rev().find(|(_, row)| present(row));
    let after = known.range(at + 1..).find(|(_, row)| present(row));
    let (Some((&start, first)), Some((&end, last))) = (before, after) else {
        return Value::Null;
    };
    let (first, last) = (&first[column], &last[column]);
    let (span, offset) = (i128::from(end - start), i128::from(at - start));
    if let (Some(first), Some(last)) = (first.as_integer(), last.as_integer()) {
        let value = i128::from(first) + (i128::from(last) - i128::from(first)) * offset / span;
        return i64::try_from(value).map_or(Value::Null, Value::BigInt);
    }
    match (first.as_float(), last.as_float()) {
        (Some(first), Some(last)) => Value::Double(first + (last - first) * offset as f64 / span as f64),
        _ => Value::Null,
    }
}

fn filled(select: &Select, rows: Vec<Vec<Value>>, range: Option<(i64, i64)>) -> Result<Vec<Vec<Value>>, String> {
    let Some(bucket) = select.bucket.as_ref().filter(|_| fills(select)) else {
        return Ok(rows);
    };
    let known: std::collections::BTreeMap<i64, Vec<Value>> =
        rows.into_iter().filter_map(|row| time_of(&row).map(|at| (at, row))).collect();
    let (Some(&first_known), Some(&last_known)) = (known.keys().next(), known.keys().next_back()) else {
        return Ok(Vec::new());
    };
    let width = bucket.micros;
    let origin = bucket.offset_micros - UNIX_EPOCH_FROM_2000_MICROS;
    let floor = |at: i64| origin + (at - origin).div_euclid(width) * width;
    let (first, last) = range.map_or((first_known, last_known), |(lower, upper)| (floor(lower), floor(upper)));
    if (last - first) / width > MOST_FILLED_BUCKETS {
        return Err(format!("fill() would make more than {MOST_FILLED_BUCKETS} points; narrow the time range"));
    }
    let columns = known.values().next().map_or(1, Vec::len);
    let mut out: Vec<Vec<Value>> = Vec::new();
    let mut at = first;
    while at <= last {
        if let Some(row) = known.get(&at) {
            out.push(row.clone());
        } else {
            let mut row = vec![Value::TimestampTz(at)];
            for column in 1..columns {
                row.push(match &select.fill {
                    Fill::Number(number) if number.fract() == 0.0 && number.abs() < 9e15 => Value::BigInt(*number as i64),
                    Fill::Number(number) => Value::Double(*number),
                    Fill::Previous => out.last().map_or(Value::Null, |previous| previous[column].clone()),
                    Fill::Linear => linear(&known, at, column),
                    Fill::Null | Fill::None => Value::Null,
                });
            }
            out.push(row);
        }
        at += width;
    }
    if select.descending {
        out.reverse();
    }
    let skip = select.offset.as_deref().and_then(|offset| offset.parse().ok()).unwrap_or(0);
    let take = select.limit.as_deref().and_then(|limit| limit.parse().ok()).unwrap_or(usize::MAX);
    Ok(out.into_iter().skip(skip).take(take).collect())
}

fn time_range(session: &mut Session<'_>, select: &Select) -> Result<Option<(i64, i64)>, String> {
    let Some(lower) = &select.lower_time else {
        return Ok(None);
    };
    let upper = select.upper_time.as_deref().unwrap_or("now()");
    let sql = format!("SELECT ({lower})::timestamptz, ({upper})::timestamptz");
    let result = run(session, &sql)?
        .into_iter()
        .rev()
        .find(|result| result.columns.is_some())
        .ok_or("the time range could not be read")?;
    Ok(match result.rows.first().map(Vec::as_slice) {
        Some([Value::TimestampTz(lower), Value::TimestampTz(upper)]) if lower <= upper => Some((*lower, *upper)),
        _ => None,
    })
}

fn civil(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 };
    (year_of_era + era * 400 + i64::from(month <= 2), month, day)
}

fn time_json(micros_since_2000: i64, epoch: Option<&str>) -> String {
    let unix_micros = micros_since_2000.saturating_add(UNIX_EPOCH_FROM_2000_MICROS);
    match epoch {
        Some("ns") => (i128::from(unix_micros) * 1000).to_string(),
        Some("u" | "µ") => unix_micros.to_string(),
        Some("ms") => unix_micros.div_euclid(1000).to_string(),
        Some("s") => unix_micros.div_euclid(1_000_000).to_string(),
        Some("m") => unix_micros.div_euclid(60_000_000).to_string(),
        Some("h") => unix_micros.div_euclid(3_600_000_000).to_string(),
        _ => {
            let (year, month, day) = civil(unix_micros.div_euclid(MICROS_PER_DAY));
            let of_day = unix_micros.rem_euclid(MICROS_PER_DAY);
            let seconds = of_day / 1_000_000;
            let fraction = of_day % 1_000_000;
            let fraction = if fraction == 0 {
                String::new()
            } else {
                format!(".{fraction:06}").trim_end_matches('0').to_owned()
            };
            format!(
                "\"{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}{fraction}Z\"",
                seconds / 3600,
                seconds / 60 % 60,
                seconds % 60
            )
        }
    }
}

fn value_json(value: &Value, epoch: Option<&str>) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Boolean(flag) => flag.to_string(),
        Value::TimestampTz(micros) | Value::Timestamp(micros) => time_json(*micros, epoch),
        Value::SmallInt(_) | Value::Integer(_) | Value::BigInt(_) | Value::Numeric(_) => value.to_text().unwrap_or_default(),
        Value::Double(number) if number.is_finite() => number.to_string(),
        Value::Real(number) if number.is_finite() => number.to_string(),
        Value::Double(_) | Value::Real(_) => "null".to_owned(),
        other => json_string(&other.to_text().unwrap_or_default()),
    }
}

fn series_json(name: &str, tags: Option<&[(String, String)]>, columns: &[String], rows: &[Vec<String>]) -> String {
    let tags = tags.map_or(String::new(), |tags| {
        let pairs: Vec<String> = tags.iter().map(|(key, value)| format!("{}:{}", json_string(key), json_string(value))).collect();
        format!(",\"tags\":{{{}}}", pairs.join(","))
    });
    let columns: Vec<String> = columns.iter().map(|name| json_string(name)).collect();
    let rows: Vec<String> = rows.iter().map(|row| format!("[{}]", row.join(","))).collect();
    format!(
        "{{\"name\":{}{tags},\"columns\":[{}],\"values\":[{}]}}",
        json_string(name),
        columns.join(","),
        rows.join(",")
    )
}

fn matching_measurements(session: &mut Session<'_>, catalog: &Catalog, pattern: &str) -> Result<Vec<String>, String> {
    if catalog.is_empty() {
        return Ok(Vec::new());
    }
    let names: Vec<String> = catalog.keys().map(|name| literal(name)).collect();
    let sql = format!(
        "SELECT name FROM unnest(ARRAY[{}]::text[]) AS name WHERE name ~ {} ORDER BY name",
        names.join(", "),
        literal(pattern)
    );
    let result = run(session, &sql)?
        .into_iter()
        .rev()
        .find(|result| result.columns.is_some())
        .ok_or("the measurements could not be matched")?;
    Ok(result
        .rows
        .iter()
        .filter_map(|row| row.first().and_then(Value::to_text))
        .collect())
}

fn select_series(session: &mut Session<'_>, select: &Select, epoch: Option<&str>) -> Result<Vec<String>, String> {
    let catalog = crate::store::catalog(session)?;
    let mut measurements: Vec<String> = Vec::new();
    for source in &select.sources {
        let found = match source {
            Source::Name(name) => vec![name.clone()],
            Source::Pattern(pattern) => matching_measurements(session, &catalog, pattern)?,
        };
        for name in found {
            if !measurements.contains(&name) {
                measurements.push(name);
            }
        }
    }
    let mut out = Vec::new();
    for measurement in &measurements {
        if let Some(columns) = catalog.get(measurement) {
            out.extend(measurement_series(session, select, measurement, columns, epoch)?);
        }
    }
    let skip = select.series_offset.unwrap_or(0);
    let take = select.series_limit.unwrap_or(usize::MAX);
    Ok(out.into_iter().skip(skip).take(take).collect())
}

fn measurement_series(
    session: &mut Session<'_>,
    select: &Select,
    measurement: &str,
    columns: &[(String, String)],
    epoch: Option<&str>,
) -> Result<Vec<String>, String> {
    let planned = plan(select, measurement, columns)?;
    let result: QueryResult = run(session, &planned.sql)?
        .into_iter()
        .rev()
        .find(|result| result.columns.is_some())
        .ok_or("the query returned no rows")?;
    let names: Vec<String> = result.columns.iter().flatten().map(|column| column.name.clone()).collect();
    let shown: Vec<String> = names[planned.tag_count..].to_vec();
    let mut series: Vec<(Vec<(String, String)>, Vec<Vec<Value>>)> = Vec::new();
    for row in result.rows {
        let tags: Vec<(String, String)> = names[..planned.tag_count]
            .iter()
            .zip(&row)
            .map(|(name, value)| (name.clone(), value.to_text().unwrap_or_default()))
            .collect();
        let values: Vec<Value> = row.into_iter().skip(planned.tag_count).collect();
        match series.last_mut() {
            Some((held, rows)) if *held == tags => rows.push(values),
            _ => series.push((tags, vec![values])),
        }
    }
    let range = if fills(select) { time_range(session, select)? } else { None };
    let mut out = Vec::with_capacity(series.len());
    for (tags, rows) in series {
        let rows: Vec<Vec<String>> = filled(select, rows, range)?
            .iter()
            .map(|row| row.iter().map(|value| value_json(value, epoch)).collect())
            .collect();
        out.push(series_json(
            measurement,
            (planned.tag_count > 0).then_some(tags.as_slice()),
            &shown,
            &rows,
        ));
    }
    Ok(out)
}

type Catalog = std::collections::BTreeMap<String, Vec<(String, String)>>;

fn keys_of<'c>(catalog: &'c Catalog, measurement: Option<&String>) -> Vec<(&'c String, &'c Vec<(String, String)>)> {
    catalog
        .iter()
        .filter(|(name, _)| measurement.is_none_or(|wanted| *name == wanted))
        .collect()
}

fn show(session: &mut Session<'_>, reader: &mut Reader, database: &str) -> Result<Vec<String>, String> {
    let catalog = crate::store::catalog(session)?;
    let from = |reader: &mut Reader| -> Result<Option<String>, String> {
        if reader.keyword("from") { reader.name().map(Some) } else { Ok(None) }
    };
    let text = |kind: &str| kind == "text";
    if reader.keyword("databases") {
        return Ok(vec![series_json("databases", None, &["name".to_owned()], &[vec![json_string(database)]])]);
    }
    if reader.keyword("measurements") {
        let rows: Vec<Vec<String>> = catalog.keys().map(|name| vec![json_string(name)]).collect();
        return Ok(vec![series_json("measurements", None, &["name".to_owned()], &rows)]);
    }
    if reader.keyword("retention") {
        reader.expect_keyword("policies")?;
        let columns: Vec<String> = ["name", "duration", "shardGroupDuration", "replicaN", "default"].iter().map(|name| (*name).to_owned()).collect();
        return Ok(vec![series_json("", None, &columns, &[vec![json_string("autogen"), json_string("0s"), json_string("168h0m0s"), "1".to_owned(), "true".to_owned()]])]);
    }
    if reader.keyword("tag") {
        if reader.keyword("keys") {
            let measurement = from(reader)?;
            return Ok(keys_of(&catalog, measurement.as_ref())
                .into_iter()
                .map(|(name, columns)| {
                    let rows: Vec<Vec<String>> = columns.iter().filter(|(_, kind)| text(kind)).map(|(column, _)| vec![json_string(column)]).collect();
                    series_json(name, None, &["tagKey".to_owned()], &rows)
                })
                .collect());
        }
        reader.expect_keyword("values")?;
        let measurement = from(reader)?;
        reader.expect_keyword("with")?;
        reader.expect_keyword("key")?;
        if reader.next() != Some(Token::Operator("=".to_owned())) {
            return Err("SHOW TAG VALUES needs WITH KEY = \"tag\"".to_owned());
        }
        let key = reader.name()?;
        let mut out = Vec::new();
        for (name, columns) in keys_of(&catalog, measurement.as_ref()) {
            if !columns.iter().any(|(column, kind)| *column == key && text(kind)) {
                continue;
            }
            let result = run(
                session,
                &format!(
                    "SELECT DISTINCT {} FROM {SCHEMA}.{} WHERE {} IS NOT NULL AND {} <> '' ORDER BY 1",
                    column(&key),
                    identifier(name),
                    column(&key),
                    column(&key)
                ),
            )?
            .into_iter()
            .rev()
            .find(|result| result.columns.is_some())
            .ok_or("the query returned no rows")?;
            let rows: Vec<Vec<String>> = result
                .rows
                .iter()
                .map(|row| vec![json_string(&key), json_string(&row.first().and_then(Value::to_text).unwrap_or_default())])
                .collect();
            out.push(series_json(name, None, &["key".to_owned(), "value".to_owned()], &rows));
        }
        return Ok(out);
    }
    if reader.keyword("field") {
        reader.expect_keyword("keys")?;
        let measurement = from(reader)?;
        return Ok(keys_of(&catalog, measurement.as_ref())
            .into_iter()
            .map(|(name, columns)| {
                let rows: Vec<Vec<String>> = columns
                    .iter()
                    .filter(|(column, kind)| column != "time" && !text(kind))
                    .map(|(column, kind)| {
                        let field_type = match kind.as_str() {
                            "bigint" => "integer",
                            "numeric" => "unsigned",
                            "boolean" => "boolean",
                            "string" => "string",
                            _ => "float",
                        };
                        vec![json_string(column), json_string(field_type)]
                    })
                    .collect();
                series_json(name, None, &["fieldKey".to_owned(), "fieldType".to_owned()], &rows)
            })
            .collect());
    }
    Err(format!("SHOW {:?} is not supported", reader.peek()))
}

fn statements(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for character in text.chars() {
        match quote {
            Some(open) if character == open => quote = None,
            Some(_) => {}
            None if matches!(character, '\'' | '"') => quote = Some(character),
            None if character == ';' => {
                found.push(std::mem::take(&mut current));
                continue;
            }
            None => {}
        }
        current.push(character);
    }
    found.push(current);
    found.into_iter().filter(|statement| !statement.trim().is_empty()).collect()
}

fn statement(session: &mut Session<'_>, text: &str, database: &str, epoch: Option<&str>) -> Result<Vec<String>, String> {
    let mut reader = Reader { tokens: tokens(text)?, at: 0 };
    if reader.keyword("select") {
        let select = reader.select()?;
        return select_series(session, &select, epoch);
    }
    if reader.keyword("show") {
        return show(session, &mut reader, database);
    }
    if reader.keyword("create") && reader.keyword("database") {
        return Ok(Vec::new());
    }
    Err(format!("error parsing query: found {:?}, expected SELECT, SHOW or CREATE DATABASE", reader.tokens.first()))
}

/// The answer to InfluxDB 1.x's `/query`: each statement of `text` (InfluxQL `SELECT` over a
/// measurement's table, `SHOW` of databases, measurements, tag keys, tag values, field keys and
/// retention policies, `CREATE DATABASE`) as one result with its series or its error.
pub(crate) fn answer(session: &mut Session<'_>, text: &str, database: &str, epoch: Option<&str>) -> String {
    let results: Vec<String> = statements(text)
        .iter()
        .enumerate()
        .map(|(index, statement_text)| match statement(session, statement_text, database, epoch) {
            Ok(series) if series.is_empty() => format!("{{\"statement_id\":{index}}}"),
            Ok(series) => format!("{{\"statement_id\":{index},\"series\":[{}]}}", series.join(",")),
            Err(message) => format!("{{\"statement_id\":{index},\"error\":{}}}", json_string(&message)),
        })
        .collect();
    format!("{{\"results\":[{}]}}\n", results.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn planned(text: &str, columns: &[(&str, &str)]) -> String {
        let mut reader = Reader { tokens: tokens(text).unwrap(), at: 0 };
        assert!(reader.keyword("select"));
        let select = reader.select().unwrap();
        let columns: Vec<(String, String)> = columns.iter().map(|(name, kind)| ((*name).to_owned(), (*kind).to_owned())).collect();
        let Some(Source::Name(measurement)) = select.sources.first() else {
            panic!("one measurement");
        };
        plan(&select, measurement, &columns).unwrap().sql
    }

    #[test]
    fn influxql_becomes_sql() {
        assert_eq!(
            planned(
                "SELECT mean(\"usage\") FROM cpu WHERE host = 'a' AND time > now() - 1h GROUP BY time(5m), host",
                &[("time", "timestamp with time zone"), ("host", "text"), ("usage", "double precision")]
            ),
            "SELECT \"host\" AS \"host\", time_bucket('5 minutes'::interval, \"time\", '1970-01-01T00:00:00Z'::timestamptz) AS \"time\", avg(\"usage\") AS \"mean\" FROM metrics.\"cpu\" WHERE \"host\" = 'a' AND \"time\" > now() - '1 hours'::interval GROUP BY 1, 2 ORDER BY 1, 2"
        );
        assert_eq!(
            planned(
                "SELECT count(v) FROM m WHERE time >= now() - 1h GROUP BY time(10m, -2m) fill(0) LIMIT 3",
                &[("time", "x"), ("v", "double precision")]
            ),
            "SELECT time_bucket('10 minutes'::interval, \"time\", '1970-01-01T00:00:00Z'::timestamptz - '2 minutes'::interval) AS \"time\", count(\"v\") AS \"count\" FROM metrics.\"m\" WHERE \"time\" >= now() - '1 hours'::interval GROUP BY 1 ORDER BY 1"
        );
        assert_eq!(
            planned("SELECT * FROM cpu ORDER BY time DESC LIMIT 2", &[("time", "x"), ("host", "text"), ("usage", "double precision")]),
            "SELECT \"time\", \"host\", \"usage\" FROM metrics.\"cpu\" ORDER BY 1 DESC LIMIT 2"
        );
        assert_eq!(time_json(0, None), "\"2000-01-01T00:00:00Z\"");
        assert_eq!(time_json(0, Some("s")), "946684800");
        assert_eq!(statements("SHOW DATABASES; SELECT 'a;b' FROM x").len(), 2);
        assert_eq!(
            planned(
                "SELECT distinct(\"host\") FROM cpu GROUP BY *",
                &[("time", "x"), ("dc", "text"), ("host", "text"), ("usage", "double precision")]
            ),
            "SELECT \"dc\" AS \"dc\", \"host\" AS \"host\", '1970-01-01T00:00:00Z'::timestamptz AS \"time\", \"host\" AS \"distinct\" FROM metrics.\"cpu\" GROUP BY 1, 2, 4 ORDER BY 1, 2, 3, 4"
        );
        assert_eq!(
            planned("SELECT a + b * 2, a / b AS ratio FROM m", &[("time", "x"), ("a", "bigint"), ("b", "bigint")]),
            "SELECT \"time\", (\"a\" + (\"b\" * 2)) AS \"a_b\", ((\"a\")::double precision / (\"b\")) AS \"ratio\" FROM metrics.\"m\" ORDER BY 1"
        );
        assert_eq!(
            planned("SELECT mean(a) * 100 FROM m GROUP BY time(1m) fill(none)", &[("time", "x"), ("a", "double precision")]),
            "SELECT time_bucket('1 minutes'::interval, \"time\", '1970-01-01T00:00:00Z'::timestamptz) AS \"time\", (avg(\"a\") * 100) AS \"mean\" FROM metrics.\"m\" GROUP BY 1 ORDER BY 1"
        );
    }

    fn filled_values(fill: &str, rows: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
        let mut reader = Reader {
            tokens: tokens(&format!("SELECT mean(v) FROM m GROUP BY time(1m) fill({fill})")).unwrap(),
            at: 0,
        };
        assert!(reader.keyword("select"));
        let select = reader.select().unwrap();
        filled(&select, rows, None).unwrap()
    }

    #[test]
    fn fill_makes_the_empty_buckets() {
        let minute = 60_000_000;
        let base = -UNIX_EPOCH_FROM_2000_MICROS + 600 * minute;
        let rows = vec![
            vec![Value::TimestampTz(base), Value::Double(1.0)],
            vec![Value::TimestampTz(base + 3 * minute), Value::Double(4.0)],
        ];
        let linear = filled_values("linear", rows.clone());
        assert_eq!(linear.len(), 4);
        assert_eq!(linear[1], vec![Value::TimestampTz(base + minute), Value::Double(2.0)]);
        let previous = filled_values("previous", rows.clone());
        assert_eq!(previous[2][1], Value::Double(1.0));
        let zero = filled_values("0", rows.clone());
        assert_eq!(zero[1][1], Value::BigInt(0));
        assert_eq!(filled_values("null", rows.clone())[2][1], Value::Null);
        assert_eq!(filled_values("none", rows).len(), 2);
    }
}
