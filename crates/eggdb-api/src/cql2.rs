//! OGC's Common Query Language (CQL2, OGC 21-065) filters for OGC API - Features (D879), in
//! both encodings, `cql2-text` and `cql2-json`, turned into SQL conditions: logical
//! operators, comparisons, `LIKE`, `BETWEEN`, `IN`, `IS NULL`, `CASEI`, arithmetic, the
//! spatial functions over WKT and `BBOX` literals, and the temporal functions over instants
//! and intervals. Properties become quoted columns of the collection's table; every literal is
//! quoted, so a filter never adds SQL of its own.

use crate::request::literal;
use eggdb_sql::json::{Json, Node};

const MAX_DEPTH: usize = 64;
const MAX_FILTER_BYTES: usize = 64 << 10;
const CRS84_SRID: u32 = 4326;

/// The coordinate reference system a filter's geometry literals are written in: its SRID, and
/// whether its axes come latitude first (EPSG's geographic systems).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LiteralCrs {
    pub(crate) srid: u32,
    pub(crate) flipped: bool,
}

impl LiteralCrs {
    pub(crate) const CRS84: Self = Self {
        srid: CRS84_SRID,
        flipped: false,
    };

    fn placed(self, geometry: String) -> String {
        let geometry = if self.flipped { format!("ST_FlipCoordinates({geometry})") } else { geometry };
        if self.srid == CRS84_SRID {
            geometry
        } else {
            format!("ST_Transform({geometry}, {CRS84_SRID})")
        }
    }
}

const SPATIAL: [(&str, &str); 8] = [
    ("s_intersects", "ST_Intersects"),
    ("s_equals", "ST_Equals"),
    ("s_disjoint", "ST_Disjoint"),
    ("s_touches", "ST_Touches"),
    ("s_within", "ST_Within"),
    ("s_overlaps", "ST_Overlaps"),
    ("s_crosses", "ST_Crosses"),
    ("s_contains", "ST_Contains"),
];

const GEOMETRY_WORDS: [&str; 7] = [
    "point",
    "linestring",
    "polygon",
    "multipoint",
    "multilinestring",
    "multipolygon",
    "geometrycollection",
];

/// How a filter names the collection's columns: the SQL for a property, or why it has none.
pub(crate) type Columns<'c> = &'c dyn Fn(&str) -> Result<String, String>;

#[derive(Debug, Clone, PartialEq)]
enum Operand {
    Sql(String),
    Interval(Option<String>, Option<String>),
}

impl Operand {
    fn sql(self) -> Result<String, String> {
        match self {
            Self::Sql(sql) => Ok(sql),
            Self::Interval(..) => Err("an INTERVAL is only an argument of the temporal functions".to_owned()),
        }
    }
}

fn instant_bounds(operand: Operand) -> (Option<String>, Option<String>) {
    match operand {
        Operand::Sql(sql) => (Some(sql.clone()), Some(sql)),
        Operand::Interval(start, end) => (start, end),
    }
}

fn temporal(function: &str, left: Operand, right: Operand) -> Result<String, String> {
    let (left_start, left_end) = instant_bounds(left);
    let (right_start, right_end) = instant_bounds(right);
    let compare = |left: &Option<String>, symbol: &str, right: &Option<String>| match (left, right) {
        (Some(left), Some(right)) => format!("({left} {symbol} {right})"),
        _ => "TRUE".to_owned(),
    };
    Ok(match function {
        "t_before" => compare(&left_end, "<", &right_start),
        "t_after" => compare(&left_start, ">", &right_end),
        "t_equals" => format!(
            "({} AND {})",
            compare(&left_start, "=", &right_start),
            compare(&left_end, "=", &right_end)
        ),
        "t_intersects" => format!(
            "({} AND {})",
            compare(&left_start, "<=", &right_end),
            compare(&left_end, ">=", &right_start)
        ),
        "t_disjoint" => format!(
            "(NOT ({} AND {}))",
            compare(&left_start, "<=", &right_end),
            compare(&left_end, ">=", &right_start)
        ),
        "t_during" => format!(
            "({} AND {})",
            compare(&left_start, ">", &right_start),
            compare(&left_end, "<", &right_end)
        ),
        "t_contains" => format!(
            "({} AND {})",
            compare(&left_start, "<", &right_start),
            compare(&left_end, ">", &right_end)
        ),
        "t_starts" => format!(
            "({} AND {})",
            compare(&left_start, "=", &right_start),
            compare(&left_end, "<", &right_end)
        ),
        "t_finishes" => format!(
            "({} AND {})",
            compare(&left_start, ">", &right_start),
            compare(&left_end, "=", &right_end)
        ),
        "t_meets" => compare(&left_end, "=", &right_start),
        "t_metby" => compare(&left_start, "=", &right_end),
        "t_overlaps" => format!(
            "({} AND {} AND {})",
            compare(&left_start, "<", &right_start),
            compare(&left_end, ">", &right_start),
            compare(&left_end, "<", &right_end)
        ),
        "t_overlappedby" => format!(
            "({} AND {} AND {})",
            compare(&left_start, ">", &right_start),
            compare(&left_start, "<", &right_end),
            compare(&left_end, ">", &right_end)
        ),
        other => return Err(format!("the temporal function {other} is not known")),
    })
}

fn time_literal(kind: &str, text: &str) -> Result<String, String> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b':' | b'.' | b'+')) {
        return Err(format!("'{text}' is not a {kind}"));
    }
    Ok(match kind {
        "date" => format!("CAST({} AS date)", literal(text)),
        _ => format!("CAST({} AS timestamptz)", literal(text)),
    })
}

fn interval_end(text: &str) -> Result<Option<String>, String> {
    if text == ".." {
        return Ok(None);
    }
    let kind = if text.len() == 10 { "date" } else { "timestamp" };
    time_literal(kind, text).map(Some)
}

fn geometry_sql(wkt: &str, crs: LiteralCrs) -> Result<String, String> {
    if !wkt
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | b'(' | b')' | b',' | b'.' | b'-' | b'+'))
    {
        return Err("the geometry is not well-known text".to_owned());
    }
    Ok(crs.placed(format!("ST_GeomFromText({}, {})", literal(wkt), crs.srid)))
}

fn bbox_sql(numbers: &[String], crs: LiteralCrs) -> Result<String, String> {
    let parsed: Vec<f64> = numbers
        .iter()
        .filter_map(|number| number.trim_matches(['(', ')']).parse::<f64>().ok())
        .collect();
    if parsed.len() != numbers.len() || parsed.iter().any(|value| !value.is_finite()) {
        return Err("BBOX takes numbers".to_owned());
    }
    let (first_low, second_low, first_high, second_high) = match parsed.as_slice() {
        [first_low, second_low, first_high, second_high] | [first_low, second_low, _, first_high, second_high, _] => {
            (*first_low, *second_low, *first_high, *second_high)
        }
        _ => return Err("BBOX takes four or six numbers".to_owned()),
    };
    let (west, south, east, north) = if crs.flipped {
        (second_low, first_low, second_high, first_high)
    } else {
        (first_low, second_low, first_high, second_high)
    };
    let envelope = format!("ST_MakeEnvelope({west}, {south}, {east}, {north}, {})", crs.srid);
    Ok(LiteralCrs { flipped: false, ..crs }.placed(envelope))
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Word(String),
    Quoted(String),
    Text(String),
    Number(String),
    Symbol(&'static str),
    End,
}

fn tokens(text: &str) -> Result<Vec<(Token, usize, usize)>, String> {
    if text.len() > MAX_FILTER_BYTES {
        return Err("the filter is too long".to_owned());
    }
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let start = at;
        let byte = bytes[at];
        let token = match byte {
            b' ' | b'\t' | b'\n' | b'\r' => {
                at += 1;
                continue;
            }
            b'(' | b')' | b',' | b'=' | b'+' | b'-' | b'*' | b'/' | b'%' => {
                at += 1;
                Token::Symbol(match byte {
                    b'(' => "(",
                    b')' => ")",
                    b',' => ",",
                    b'=' => "=",
                    b'+' => "+",
                    b'-' => "-",
                    b'*' => "*",
                    b'/' => "/",
                    _ => "%",
                })
            }
            b'<' | b'>' => {
                at += 1;
                let symbol = match (byte, bytes.get(at)) {
                    (b'<', Some(b'>')) => "<>",
                    (b'<', Some(b'=')) => "<=",
                    (b'>', Some(b'=')) => ">=",
                    (b'<', _) => "<",
                    _ => ">",
                };
                if symbol.len() == 2 {
                    at += 1;
                }
                Token::Symbol(symbol)
            }
            b'\'' | b'"' => {
                let quote = byte;
                at += 1;
                let mut value = String::new();
                loop {
                    let Some(character) = text.get(at..).and_then(|rest| rest.chars().next()) else {
                        return Err("an unterminated quote in the filter".to_owned());
                    };
                    at += character.len_utf8();
                    if character == char::from(quote) {
                        if bytes.get(at) == Some(&quote) {
                            value.push(character);
                            at += 1;
                            continue;
                        }
                        break;
                    }
                    value.push(character);
                }
                if quote == b'"' { Token::Quoted(value) } else { Token::Text(value) }
            }
            b'0'..=b'9' | b'.' => {
                while bytes.get(at).is_some_and(|next| next.is_ascii_digit() || *next == b'.') {
                    at += 1;
                }
                if matches!(bytes.get(at), Some(b'e' | b'E')) {
                    at += 1;
                    if matches!(bytes.get(at), Some(b'+' | b'-')) {
                        at += 1;
                    }
                    while bytes.get(at).is_some_and(u8::is_ascii_digit) {
                        at += 1;
                    }
                }
                Token::Number(text[start..at].to_owned())
            }
            _ if byte.is_ascii_alphabetic() || byte == b'_' || byte >= 0x80 => {
                while bytes
                    .get(at)
                    .is_some_and(|next| next.is_ascii_alphanumeric() || matches!(next, b'_' | b':' | b'.') || *next >= 0x80)
                {
                    at += 1;
                }
                Token::Word(text[start..at].to_owned())
            }
            _ => return Err(format!("unexpected character at offset {start} in the filter")),
        };
        found.push((token, start, at));
    }
    found.push((Token::End, text.len(), text.len()));
    Ok(found)
}

struct TextParser<'t, 'c> {
    text: &'t str,
    tokens: Vec<(Token, usize, usize)>,
    at: usize,
    depth: usize,
    columns: Columns<'c>,
    crs: LiteralCrs,
}

impl TextParser<'_, '_> {
    fn peek(&self) -> &Token {
        self.tokens.get(self.at).map_or(&Token::End, |(token, _, _)| token)
    }

    fn advance(&mut self) -> Token {
        let token = self.peek().clone();
        self.at += 1;
        token
    }

    fn is_word(&self, word: &str) -> bool {
        matches!(self.peek(), Token::Word(found) if found.eq_ignore_ascii_case(word))
    }

    fn is_word_at(&self, ahead: usize, word: &str) -> bool {
        matches!(self.tokens.get(self.at + ahead), Some((Token::Word(found), _, _)) if found.eq_ignore_ascii_case(word))
    }

    fn take_word(&mut self, word: &str) -> bool {
        let found = self.is_word(word);
        if found {
            self.at += 1;
        }
        found
    }

    fn take(&mut self, symbol: &str) -> bool {
        let found = matches!(self.peek(), Token::Symbol(found) if *found == symbol);
        if found {
            self.at += 1;
        }
        found
    }

    fn expect(&mut self, symbol: &str) -> Result<(), String> {
        if self.take(symbol) {
            return Ok(());
        }
        Err(format!("expected '{symbol}' in the filter"))
    }

    fn deeper(&mut self) -> Result<(), String> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err("the filter nests too deeply".to_owned());
        }
        Ok(())
    }

    fn whole(&mut self) -> Result<String, String> {
        let condition = self.or_condition()?;
        if *self.peek() != Token::End {
            return Err("unexpected text after the end of the filter".to_owned());
        }
        Ok(condition)
    }

    fn or_condition(&mut self) -> Result<String, String> {
        self.deeper()?;
        let mut parts = vec![self.and_condition()?];
        while self.take_word("or") {
            parts.push(self.and_condition()?);
        }
        self.depth -= 1;
        Ok(if parts.len() == 1 { parts.remove(0) } else { format!("({})", parts.join(" OR ")) })
    }

    fn and_condition(&mut self) -> Result<String, String> {
        let mut parts = vec![self.not_condition()?];
        while self.take_word("and") {
            parts.push(self.not_condition()?);
        }
        Ok(if parts.len() == 1 { parts.remove(0) } else { format!("({})", parts.join(" AND ")) })
    }

    fn not_condition(&mut self) -> Result<String, String> {
        if self.take_word("not") {
            self.deeper()?;
            let inner = self.not_condition()?;
            self.depth -= 1;
            return Ok(format!("(NOT {inner})"));
        }
        self.predicate()
    }

    fn predicate(&mut self) -> Result<String, String> {
        if matches!(self.peek(), Token::Symbol("(")) {
            let (resume, depth) = (self.at, self.depth);
            self.at += 1;
            if let Ok(inner) = self.or_condition()
                && self.take(")")
                && !self.at_comparison()
            {
                return Ok(inner);
            }
            self.at = resume;
            self.depth = depth;
        }
        let left = self.scalar()?;
        let negated = self.is_word("not") && (self.is_word_at(1, "like") || self.is_word_at(1, "between") || self.is_word_at(1, "in"));
        if negated {
            self.at += 1;
        }
        let not = if negated { "NOT " } else { "" };
        if self.take_word("like") {
            let pattern = self.scalar()?.sql()?;
            return Ok(format!("({} {not}LIKE {pattern})", left.sql()?));
        }
        if self.take_word("between") {
            let low = self.scalar()?.sql()?;
            if !self.take_word("and") {
                return Err("BETWEEN needs AND".to_owned());
            }
            let high = self.scalar()?.sql()?;
            return Ok(format!("({} {not}BETWEEN {low} AND {high})", left.sql()?));
        }
        if self.take_word("in") {
            self.expect("(")?;
            let mut items = vec![self.scalar()?.sql()?];
            while self.take(",") {
                items.push(self.scalar()?.sql()?);
            }
            self.expect(")")?;
            return Ok(format!("({} {not}IN ({}))", left.sql()?, items.join(", ")));
        }
        if self.take_word("is") {
            let negated = self.take_word("not");
            if !self.take_word("null") {
                return Err("IS needs NULL".to_owned());
            }
            return Ok(format!("({} IS {}NULL)", left.sql()?, if negated { "NOT " } else { "" }));
        }
        for symbol in ["=", "<>", "<=", ">=", "<", ">"] {
            if self.take(symbol) {
                let right = self.scalar()?.sql()?;
                return Ok(format!("({} {symbol} {right})", left.sql()?));
            }
        }
        left.sql()
    }

    fn at_comparison(&self) -> bool {
        matches!(self.peek(), Token::Symbol("=" | "<>" | "<=" | ">=" | "<" | ">" | "+" | "-" | "*" | "/" | "%"))
            || ["like", "between", "in", "is"].iter().any(|word| self.is_word(word))
            || (self.is_word("not") && ["like", "between", "in"].iter().any(|word| self.is_word_at(1, word)))
    }

    fn scalar(&mut self) -> Result<Operand, String> {
        let mut left = self.term()?;
        loop {
            let symbol = if self.take("+") {
                "+"
            } else if self.take("-") {
                "-"
            } else {
                return Ok(left);
            };
            let right = self.term()?.sql()?;
            left = Operand::Sql(format!("({} {symbol} {right})", left.sql()?));
        }
    }

    fn term(&mut self) -> Result<Operand, String> {
        let mut left = self.factor()?;
        loop {
            let symbol = if self.take("*") {
                "*"
            } else if self.take("/") {
                "/"
            } else if self.take("%") || self.take_word("mod") {
                "%"
            } else if self.take_word("div") {
                "/"
            } else {
                return Ok(left);
            };
            let right = self.factor()?.sql()?;
            left = Operand::Sql(format!("({} {symbol} {right})", left.sql()?));
        }
    }

    fn factor(&mut self) -> Result<Operand, String> {
        if self.take("-") {
            self.deeper()?;
            let inner = self.factor()?.sql()?;
            self.depth -= 1;
            return Ok(Operand::Sql(format!("(-{inner})")));
        }
        self.primary()
    }

    fn arguments(&mut self) -> Result<Vec<Operand>, String> {
        self.expect("(")?;
        let mut arguments = Vec::new();
        if self.take(")") {
            return Ok(arguments);
        }
        loop {
            self.deeper()?;
            let argument = if self.peek_is_condition_start() {
                Operand::Sql(self.or_condition()?)
            } else {
                self.scalar()?
            };
            self.depth -= 1;
            arguments.push(argument);
            if self.take(")") {
                return Ok(arguments);
            }
            self.expect(",")?;
        }
    }

    fn peek_is_condition_start(&self) -> bool {
        self.is_word("not")
    }

    fn wkt(&mut self) -> Result<Operand, String> {
        let Some(&(_, start, _)) = self.tokens.get(self.at) else {
            return Err("a geometry is missing".to_owned());
        };
        self.at += 1;
        let mut depth = 0usize;
        loop {
            match self.tokens.get(self.at) {
                Some((Token::Symbol("("), _, _)) => depth += 1,
                Some((Token::Symbol(")"), _, end)) => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        let end = *end;
                        self.at += 1;
                        let wkt = self.text.get(start..end).unwrap_or_default();
                        return geometry_sql(wkt, self.crs).map(Operand::Sql);
                    }
                }
                Some((Token::End, _, _)) | None => return Err("an unterminated geometry".to_owned()),
                _ => {}
            }
            self.at += 1;
        }
    }

    fn text_argument(&mut self) -> Result<String, String> {
        match self.advance() {
            Token::Text(text) => Ok(text),
            _ => Err("expected a quoted string".to_owned()),
        }
    }

    fn primary(&mut self) -> Result<Operand, String> {
        if let Token::Word(word) = self.peek().clone() {
            let lowered = word.to_ascii_lowercase();
            let called = matches!(self.tokens.get(self.at + 1), Some((Token::Symbol("("), _, _)));
            if GEOMETRY_WORDS.contains(&lowered.as_str()) && called {
                return self.wkt();
            }
            if called {
                self.at += 1;
                return self.call(&lowered);
            }
        }
        match self.advance() {
            Token::Symbol("(") => {
                self.deeper()?;
                let inner = self.scalar()?.sql()?;
                self.depth -= 1;
                self.expect(")")?;
                Ok(Operand::Sql(format!("({inner})")))
            }
            Token::Number(number) => {
                if number.parse::<f64>().is_err() {
                    return Err(format!("{number} is not a number"));
                }
                Ok(Operand::Sql(number))
            }
            Token::Text(text) => Ok(Operand::Sql(literal(&text))),
            Token::Quoted(name) => (self.columns)(&name).map(Operand::Sql),
            Token::Word(word) => match word.to_ascii_lowercase().as_str() {
                "true" => Ok(Operand::Sql("TRUE".to_owned())),
                "false" => Ok(Operand::Sql("FALSE".to_owned())),
                "null" => Ok(Operand::Sql("NULL".to_owned())),
                _ => (self.columns)(&word).map(Operand::Sql),
            },
            other => Err(format!("unexpected {other:?} in the filter")),
        }
    }

    fn call(&mut self, name: &str) -> Result<Operand, String> {
        match name {
            "date" | "timestamp" => {
                self.expect("(")?;
                let text = self.text_argument()?;
                self.expect(")")?;
                time_literal(name, &text).map(Operand::Sql)
            }
            "interval" => {
                self.expect("(")?;
                let start = self.text_argument()?;
                self.expect(",")?;
                let end = self.text_argument()?;
                self.expect(")")?;
                Ok(Operand::Interval(interval_end(&start)?, interval_end(&end)?))
            }
            "bbox" => {
                let numbers: Result<Vec<String>, String> = self.arguments()?.into_iter().map(Operand::sql).collect();
                bbox_sql(&numbers?, self.crs).map(Operand::Sql)
            }
            _ => {
                let arguments = self.arguments()?;
                call_sql(name, arguments).map(Operand::Sql)
            }
        }
    }
}

fn call_sql(name: &str, arguments: Vec<Operand>) -> Result<String, String> {
    if let Some((_, function)) = SPATIAL.iter().find(|(known, _)| *known == name) {
        let [left, right]: [Operand; 2] = arguments
            .try_into()
            .map_err(|_| format!("{} takes two geometries", name.to_ascii_uppercase()))?;
        return Ok(format!("{function}({}, {})", left.sql()?, right.sql()?));
    }
    if name.starts_with("t_") {
        let [left, right]: [Operand; 2] = arguments
            .try_into()
            .map_err(|_| format!("{} takes two times", name.to_ascii_uppercase()))?;
        return temporal(name, left, right);
    }
    match name {
        "casei" | "accenti" => {
            let [value]: [Operand; 1] = arguments
                .try_into()
                .map_err(|_| format!("{} takes one value", name.to_ascii_uppercase()))?;
            let value = value.sql()?;
            Ok(if name == "casei" { format!("lower({value})") } else { value })
        }
        _ => Err(format!("the function {name} is not known")),
    }
}

/// A `cql2-text` filter whose geometry literals are in `crs`, as an SQL condition.
pub(crate) fn text_filter(text: &str, columns: Columns<'_>, crs: LiteralCrs) -> Result<String, String> {
    let mut parser = TextParser {
        text,
        tokens: tokens(text)?,
        at: 0,
        depth: 0,
        columns,
        crs,
    };
    parser.whole()
}

fn json_operand(node: &Node, columns: Columns<'_>, crs: LiteralCrs, depth: usize) -> Result<Operand, String> {
    if depth > MAX_DEPTH {
        return Err("the filter nests too deeply".to_owned());
    }
    match &node.value {
        Json::Null => Ok(Operand::Sql("NULL".to_owned())),
        Json::Boolean(flag) => Ok(Operand::Sql(if *flag { "TRUE" } else { "FALSE" }.to_owned())),
        Json::Number(number) => {
            if number.parse::<f64>().is_err() {
                return Err(format!("{number} is not a number"));
            }
            Ok(Operand::Sql(number.clone()))
        }
        Json::String(text) => Ok(Operand::Sql(literal(text))),
        Json::Array(items) => {
            let listed: Result<Vec<String>, String> = items
                .iter()
                .map(|item| json_operand(item, columns, crs, depth + 1).and_then(Operand::sql))
                .collect();
            Ok(Operand::Sql(format!("({})", listed?.join(", "))))
        }
        Json::Object(members) => {
            let member = |name: &str| members.iter().find(|(key, _)| key == name).map(|(_, value)| value);
            if let Some(Node {
                value: Json::String(name),
                ..
            }) = member("property")
            {
                return columns(name).map(Operand::Sql);
            }
            if let Some(Node {
                value: Json::String(date),
                ..
            }) = member("date")
            {
                return time_literal("date", date).map(Operand::Sql);
            }
            if let Some(Node {
                value: Json::String(stamp),
                ..
            }) = member("timestamp")
            {
                return time_literal("timestamp", stamp).map(Operand::Sql);
            }
            if let Some(Node {
                value: Json::Array(ends),
                ..
            }) = member("interval")
            {
                let end_of = |node: Option<&Node>| -> Result<Option<String>, String> {
                    match node.map(|node| &node.value) {
                        Some(Json::String(text)) => interval_end(text),
                        Some(Json::Object(_)) => node.map_or(Ok(None), |node| {
                            json_operand(node, columns, crs, depth + 1).and_then(Operand::sql).map(Some)
                        }),
                        _ => Err("an interval holds two times".to_owned()),
                    }
                };
                return Ok(Operand::Interval(end_of(ends.first())?, end_of(ends.get(1))?));
            }
            if let Some(Node {
                value: Json::Array(numbers),
                ..
            }) = member("bbox")
            {
                let numbers: Vec<String> = numbers
                    .iter()
                    .map(|number| match &number.value {
                        Json::Number(text) => text.clone(),
                        _ => String::new(),
                    })
                    .collect();
                return bbox_sql(&numbers, crs).map(Operand::Sql);
            }
            if (member("type").is_some() && member("coordinates").is_some()) || member("geometries").is_some() {
                return Ok(Operand::Sql(format!(
                    "ST_SetSRID(ST_GeomFromGeoJSON({}), {CRS84_SRID})",
                    literal(&geojson_text(node))
                )));
            }
            json_condition(node, columns, crs, depth + 1).map(Operand::Sql)
        }
    }
}

fn geojson_text(node: &Node) -> String {
    match &node.value {
        Json::Null => "null".to_owned(),
        Json::Boolean(flag) => flag.to_string(),
        Json::Number(number) => number.clone(),
        Json::String(text) => crate::json_string(text),
        Json::Array(items) => format!("[{}]", items.iter().map(geojson_text).collect::<Vec<_>>().join(",")),
        Json::Object(members) => format!(
            "{{{}}}",
            members
                .iter()
                .map(|(key, value)| format!("{}:{}", crate::json_string(key), geojson_text(value)))
                .collect::<Vec<_>>()
                .join(",")
        ),
    }
}

fn json_condition(node: &Node, columns: Columns<'_>, crs: LiteralCrs, depth: usize) -> Result<String, String> {
    if depth > MAX_DEPTH {
        return Err("the filter nests too deeply".to_owned());
    }
    let Json::Object(members) = &node.value else {
        return match &node.value {
            Json::Boolean(flag) => Ok(if *flag { "TRUE" } else { "FALSE" }.to_owned()),
            _ => Err("a cql2-json condition is an object with op and args".to_owned()),
        };
    };
    let member = |name: &str| members.iter().find(|(key, _)| key == name).map(|(_, value)| value);
    let Some(Node {
        value: Json::String(op),
        ..
    }) = member("op")
    else {
        return Err("a cql2-json condition needs op".to_owned());
    };
    let arguments: Vec<&Node> = match member("args") {
        Some(Node {
            value: Json::Array(items),
            ..
        }) => items.iter().collect(),
        None => Vec::new(),
        Some(_) => return Err("args is an array".to_owned()),
    };
    let op = op.to_ascii_lowercase();
    let operand = |at: usize| -> Result<Operand, String> {
        let node = arguments.get(at).ok_or_else(|| format!("{op} needs more arguments"))?;
        json_operand(node, columns, crs, depth + 1)
    };
    Ok(match op.as_str() {
        "and" | "or" => {
            let mut parts = Vec::new();
            for argument in &arguments {
                parts.push(json_condition(argument, columns, crs, depth + 1)?);
            }
            if parts.is_empty() {
                return Err(format!("{op} needs arguments"));
            }
            format!("({})", parts.join(if op == "and" { " AND " } else { " OR " }))
        }
        "not" => {
            let inner = arguments.first().ok_or_else(|| "not needs an argument".to_owned())?;
            format!("(NOT {})", json_condition(inner, columns, crs, depth + 1)?)
        }
        "=" | "<>" | "<" | ">" | "<=" | ">=" => format!("({} {op} {})", operand(0)?.sql()?, operand(1)?.sql()?),
        "like" => format!("({} LIKE {})", operand(0)?.sql()?, operand(1)?.sql()?),
        "between" => format!(
            "({} BETWEEN {} AND {})",
            operand(0)?.sql()?,
            operand(1)?.sql()?,
            operand(2)?.sql()?
        ),
        "in" => format!("({} IN {})", operand(0)?.sql()?, operand(1)?.sql()?),
        "isnull" => format!("({} IS NULL)", operand(0)?.sql()?),
        "+" | "-" | "*" | "/" | "%" | "div" => {
            let symbol = if op == "div" { "/" } else { op.as_str() };
            format!("({} {symbol} {})", operand(0)?.sql()?, operand(1)?.sql()?)
        }
        other => {
            let mut values = Vec::new();
            for at in 0..arguments.len() {
                values.push(operand(at)?);
            }
            call_sql(other, values)?
        }
    })
}

/// A `cql2-json` filter whose `BBOX` literals are in `crs` (GeoJSON is always CRS84), as an
/// SQL condition.
pub(crate) fn json_filter(node: &Node, columns: Columns<'_>, crs: LiteralCrs) -> Result<String, String> {
    json_condition(node, columns, crs, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn columns(name: &str) -> Result<String, String> {
        match name {
            "name" | "population" | "updated" | "geometry" => Ok(format!("t.\"{name}\"")),
            other => Err(format!("{other} is not queryable")),
        }
    }

    #[test]
    fn text_filters_become_quoted_sql() {
        assert_eq!(
            text_filter("population > 1000 AND (name LIKE 'Ber%' OR name IN ('Paris', 'O''Hare'))", &columns, LiteralCrs::CRS84).unwrap(),
            "((t.\"population\" > 1000) AND ((t.\"name\" LIKE 'Ber%') OR (t.\"name\" IN ('Paris', 'O''Hare'))))"
        );
        assert_eq!(
            text_filter("S_INTERSECTS(geometry, POLYGON((0 0, 1 0, 1 1, 0 0))) AND NOT name IS NULL", &columns, LiteralCrs::CRS84).unwrap(),
            "(ST_Intersects(t.\"geometry\", ST_GeomFromText('POLYGON((0 0, 1 0, 1 1, 0 0))', 4326)) AND (NOT (t.\"name\" IS NULL)))"
        );
        assert_eq!(
            text_filter("T_DURING(updated, INTERVAL('2020-01-01', '..'))", &columns, LiteralCrs::CRS84).unwrap(),
            "((t.\"updated\" > CAST('2020-01-01' AS date)) AND TRUE)"
        );
        assert_eq!(
            text_filter("population BETWEEN 10 AND 20 * 2 AND CASEI(name) = casei('berlin')", &columns, LiteralCrs::CRS84).unwrap(),
            "((t.\"population\" BETWEEN 10 AND (20 * 2)) AND (lower(t.\"name\") = lower('berlin')))"
        );
        assert_eq!(
            text_filter("S_WITHIN(geometry, BBOX(-10, 40, 10, 50))", &columns, LiteralCrs::CRS84).unwrap(),
            "ST_Within(t.\"geometry\", ST_MakeEnvelope(-10, 40, 10, 50, 4326))"
        );
    }

    #[test]
    fn literals_in_epsg_4326_swap_axes_and_projected_ones_transform() {
        let lat_first = LiteralCrs { srid: 4326, flipped: true };
        assert_eq!(
            text_filter("S_WITHIN(geometry, BBOX(40, -10, 50, 10))", &columns, lat_first).unwrap(),
            "ST_Within(t.\"geometry\", ST_MakeEnvelope(-10, 40, 10, 50, 4326))"
        );
        let mercator = LiteralCrs { srid: 3857, flipped: false };
        assert_eq!(
            text_filter("S_INTERSECTS(geometry, POINT(1000 2000))", &columns, mercator).unwrap(),
            "ST_Intersects(t.\"geometry\", ST_Transform(ST_GeomFromText('POINT(1000 2000)', 3857), 4326))"
        );
    }

    #[test]
    fn hostile_text_filters_fail() {
        for bad in [
            "",
            "secret = 1",
            "name = 'open",
            "name LIKE",
            "S_INTERSECTS(geometry, POINT(1 2); DROP TABLE x)",
            "TIMESTAMP('x''); DROP')",
            "population IN 1",
            "UNKNOWNFN(name)",
        ] {
            assert!(text_filter(bad, &columns, LiteralCrs::CRS84).is_err(), "{bad}");
        }
        let deep = format!("{}name = 'a'{}", "(".repeat(200), ")".repeat(200));
        assert!(text_filter(&deep, &columns, LiteralCrs::CRS84).is_err());
    }

    #[test]
    fn json_filters_become_the_same_sql() {
        let document = eggdb_sql::json::parse_document(
            r#"{"op":"and","args":[{"op":">","args":[{"property":"population"},1000]},{"op":"s_intersects","args":[{"property":"geometry"},{"bbox":[0,0,1,1]}]}]}"#,
        )
        .unwrap();
        assert_eq!(
            json_filter(&document, &columns, LiteralCrs::CRS84).unwrap(),
            "((t.\"population\" > 1000) AND ST_Intersects(t.\"geometry\", ST_MakeEnvelope(0, 0, 1, 1, 4326)))"
        );
    }
}
