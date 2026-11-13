use crate::json::Json;
use eggdb_graph::Value;

const MAX_STEPS: usize = 1_000;
const MAX_NESTING: usize = 16;
const PREDICATES: [&str; 17] = [
    "eq",
    "neq",
    "gt",
    "gte",
    "lt",
    "lte",
    "inside",
    "outside",
    "between",
    "within",
    "without",
    "startingWith",
    "endingWith",
    "containing",
    "notStartingWith",
    "notEndingWith",
    "notContaining",
];
const TOKEN_TYPES: [&str; 7] = ["T", "Order", "Column", "Scope", "Cardinality", "Pop", "Direction"];
const BARE_TOKENS: [&str; 13] =
    ["desc", "asc", "incr", "decr", "shuffle", "keys", "local", "global", "single", "list", "set", "first", "last"];

/// A comparison a `has` or `is` step tests, such as `P.gt(3)`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Predicate {
    pub(crate) name: String,
    pub(crate) values: Vec<Value>,
}

/// One argument of a step.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Arg {
    Value(Value),
    Predicate(Predicate),
    Token(String),
    Traversal(Vec<Step>),
}

/// One step of a traversal: its name and arguments.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Step {
    pub(crate) name: String,
    pub(crate) args: Vec<Arg>,
}

/// Why a script or a bytecode request could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParseError(pub(crate) String);

type Parsed<T> = Result<T, ParseError>;

fn error<T>(message: impl Into<String>) -> Parsed<T> {
    Err(ParseError(message.into()))
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Word(String),
    Text(String),
    Integer(i64),
    Float(f64),
    Symbol(char),
}

fn number_token(text: &str) -> Parsed<Token> {
    let trimmed = text.trim_end_matches(['L', 'l', 'd', 'D', 'f', 'F', 'i', 'I', 'b', 'B', 's', 'S', 'm', 'M', 'n', 'N']);
    let floating = text.ends_with(['d', 'D', 'f', 'F', 'm', 'M']) || trimmed.contains(['.', 'e', 'E']);
    if floating {
        trimmed.parse::<f64>().map(Token::Float).or_else(|_| error(format!("{text} is not a number")))
    } else {
        trimmed.parse::<i64>().map(Token::Integer).or_else(|_| error(format!("{text} is not a whole number in range")))
    }
}

fn quoted(chars: &[char], at: &mut usize, quote: char) -> Parsed<String> {
    let mut out = String::new();
    *at += 1;
    while let Some(&character) = chars.get(*at) {
        *at += 1;
        match character {
            '\\' => {
                let escaped = chars.get(*at).copied().ok_or_else(|| ParseError("a string ends with \\".into()))?;
                *at += 1;
                out.push(match escaped {
                    'n' => '\n',
                    't' => '\t',
                    'r' => '\r',
                    'b' => '\u{8}',
                    'f' => '\u{c}',
                    '0' => '\0',
                    'u' => {
                        let digits: String = chars.get(*at..*at + 4).map(|digits| digits.iter().collect()).unwrap_or_default();
                        *at += 4;
                        u32::from_str_radix(&digits, 16)
                            .ok()
                            .and_then(char::from_u32)
                            .ok_or_else(|| ParseError(format!("\\u{digits} is not a character")))?
                    }
                    other => other,
                });
            }
            character if character == quote => return Ok(out),
            character => out.push(character),
        }
    }
    error("a string is not closed")
}

fn tokens(script: &str) -> Parsed<Vec<Token>> {
    let chars: Vec<char> = script.chars().collect();
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(&character) = chars.get(at) {
        if character.is_whitespace() {
            at += 1;
        } else if character == '/' && chars.get(at + 1) == Some(&'/') {
            while chars.get(at).is_some_and(|character| *character != '\n') {
                at += 1;
            }
        } else if character == '\'' || character == '"' {
            out.push(Token::Text(quoted(&chars, &mut at, character)?));
        } else if character.is_ascii_digit()
            || (character == '-' && chars.get(at + 1).is_some_and(char::is_ascii_digit) && !matches!(out.last(), Some(Token::Integer(_) | Token::Float(_) | Token::Word(_))))
        {
            let start = at;
            at += 1;
            while chars.get(at).is_some_and(|next| next.is_ascii_alphanumeric() || *next == '.' && chars.get(at + 1).is_some_and(char::is_ascii_digit)) {
                if matches!(chars[at], 'e' | 'E') && matches!(chars.get(at + 1), Some('-' | '+')) {
                    at += 1;
                }
                at += 1;
            }
            out.push(number_token(&chars[start..at].iter().collect::<String>())?);
        } else if character.is_alphabetic() || character == '_' || character == '$' {
            let start = at;
            while chars.get(at).is_some_and(|next| next.is_alphanumeric() || *next == '_' || *next == '$') {
                at += 1;
            }
            out.push(Token::Word(chars[start..at].iter().collect()));
        } else if "().,[];:=".contains(character) {
            out.push(Token::Symbol(character));
            at += 1;
        } else {
            return error(format!("the script has an unexpected {character:?}"));
        }
        if out.len() > MAX_STEPS * 8 {
            return error("the script is too long");
        }
    }
    Ok(out)
}

struct ScriptReader<'t> {
    tokens: &'t [Token],
    at: usize,
    bindings: &'t [(String, Value)],
}

impl ScriptReader<'_> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at)
    }

    fn symbol(&mut self, expected: char) -> bool {
        let found = self.peek() == Some(&Token::Symbol(expected));
        if found {
            self.at += 1;
        }
        found
    }

    fn word(&mut self) -> Option<String> {
        match self.peek() {
            Some(Token::Word(word)) => {
                let word = word.clone();
                self.at += 1;
                Some(word)
            }
            _ => None,
        }
    }

    fn steps(&mut self, depth: usize) -> Parsed<Vec<Step>> {
        let mut steps = Vec::new();
        loop {
            let name = self.word().ok_or_else(|| ParseError("a step name is expected here".into()))?;
            if !self.symbol('(') {
                return error(format!("the step {name} needs parentheses"));
            }
            let args = self.args(depth)?;
            steps.push(Step { name, args });
            if steps.len() > MAX_STEPS {
                return error("the traversal has too many steps");
            }
            if !self.symbol('.') {
                return Ok(steps);
            }
        }
    }

    fn args(&mut self, depth: usize) -> Parsed<Vec<Arg>> {
        let mut args = Vec::new();
        if self.symbol(')') {
            return Ok(args);
        }
        loop {
            args.push(self.arg(depth)?);
            if self.symbol(')') {
                return Ok(args);
            }
            if !self.symbol(',') {
                return error("arguments are separated by commas");
            }
        }
    }

    fn literal(&mut self, depth: usize) -> Parsed<Value> {
        match self.arg(depth)? {
            Arg::Value(value) => Ok(value),
            _ => error("a list holds only values"),
        }
    }

    fn arg(&mut self, depth: usize) -> Parsed<Arg> {
        if depth > MAX_NESTING {
            return error("the traversal nests too deeply");
        }
        let token = self.peek().cloned().ok_or_else(|| ParseError("the script ends inside a step".into()))?;
        match token {
            Token::Text(text) => {
                self.at += 1;
                Ok(Arg::Value(Value::String(text)))
            }
            Token::Integer(number) => {
                self.at += 1;
                Ok(Arg::Value(Value::Integer(number)))
            }
            Token::Float(number) => {
                self.at += 1;
                Ok(Arg::Value(Value::Float(number)))
            }
            Token::Symbol('[') => {
                self.at += 1;
                let mut items = Vec::new();
                if !self.symbol(']') {
                    loop {
                        items.push(self.literal(depth + 1)?);
                        if self.symbol(']') {
                            break;
                        }
                        if !self.symbol(',') {
                            return error("list items are separated by commas");
                        }
                    }
                }
                Ok(Arg::Value(Value::List(items)))
            }
            Token::Word(word) => self.word_arg(&word, depth),
            Token::Symbol(symbol) => error(format!("an argument cannot start with {symbol:?}")),
        }
    }

    fn word_arg(&mut self, word: &str, depth: usize) -> Parsed<Arg> {
        match word {
            "true" | "false" => {
                self.at += 1;
                return Ok(Arg::Value(Value::Boolean(word == "true")));
            }
            "null" => {
                self.at += 1;
                return Ok(Arg::Value(Value::Null));
            }
            _ => {}
        }
        let next = self.tokens.get(self.at + 1);
        if word == "__" && next == Some(&Token::Symbol('.')) {
            self.at += 2;
            return Ok(Arg::Traversal(self.steps(depth + 1)?));
        }
        if (word == "P" || word == "TextP") && next == Some(&Token::Symbol('.')) {
            self.at += 2;
            let name = self.word().ok_or_else(|| ParseError("a predicate name is expected after P.".into()))?;
            return self.predicate(name, depth);
        }
        if TOKEN_TYPES.contains(&word) && next == Some(&Token::Symbol('.')) {
            self.at += 2;
            let name = self.word().ok_or_else(|| ParseError(format!("a name is expected after {word}.")))?;
            return Ok(Arg::Token(name));
        }
        if next == Some(&Token::Symbol('(')) {
            if PREDICATES.contains(&word) {
                self.at += 1;
                return self.predicate(word.to_owned(), depth);
            }
            return Ok(Arg::Traversal(self.steps(depth + 1)?));
        }
        self.at += 1;
        if let Some((_, value)) = self.bindings.iter().find(|(name, _)| name == word) {
            return Ok(Arg::Value(value.clone()));
        }
        if BARE_TOKENS.contains(&word) || matches!(word, "label" | "id" | "key" | "value" | "values" | "in" | "out" | "both") {
            return Ok(Arg::Token(word.to_owned()));
        }
        error(format!("{word} is not a binding, a token or a step"))
    }

    fn predicate(&mut self, name: String, depth: usize) -> Parsed<Arg> {
        if !PREDICATES.contains(&name.as_str()) {
            return error(format!("{name} is not a predicate EggDB knows"));
        }
        if !self.symbol('(') {
            return error(format!("the predicate {name} needs parentheses"));
        }
        let mut values = Vec::new();
        if !self.symbol(')') {
            loop {
                values.push(self.literal(depth + 1)?);
                if self.symbol(')') {
                    break;
                }
                if !self.symbol(',') {
                    return error("predicate arguments are separated by commas");
                }
            }
        }
        if matches!(name.as_str(), "within" | "without") && values.len() == 1 {
            if let Some(Value::List(items)) = values.first().cloned() {
                values = items;
            }
        }
        Ok(Arg::Predicate(Predicate { name, values }))
    }
}

const TERMINAL_STEPS: [&str; 6] = ["toList", "toSet", "iterate", "toBulkSet", "hasNext", "tryNext"];

/// The traversal a Gremlin script (`g.V().has('name', 'marko').out('knows')`) stands for. Only
/// the last statement of the script runs; a final `toList()`, `iterate()` or `next()` is read as
/// the end of the traversal (`next(n)` as `limit(n)`).
pub(crate) fn parse_script(script: &str, bindings: &[(String, Value)]) -> Parsed<Vec<Step>> {
    let tokens = tokens(script)?;
    let statements: Vec<&[Token]> = tokens.split(|token| *token == Token::Symbol(';')).filter(|statement| !statement.is_empty()).collect();
    let Some(last) = statements.last() else {
        return error("the script is empty");
    };
    let mut reader = ScriptReader { tokens: last, at: 0, bindings };
    let source = reader.word().ok_or_else(|| ParseError("a traversal starts with g".into()))?;
    if source != "g" || !reader.symbol('.') {
        return error(format!("a traversal starts from its source, such as g., not {source}"));
    }
    let mut steps = reader.steps(0)?;
    if reader.at != last.len() {
        return error("the script has more after the traversal");
    }
    while let Some(step) = steps.last() {
        if TERMINAL_STEPS.contains(&step.name.as_str()) {
            steps.pop();
        } else if step.name == "next" {
            let limit = step.args.first().cloned();
            steps.pop();
            steps.push(Step { name: "limit".into(), args: vec![limit.unwrap_or(Arg::Value(Value::Integer(1)))] });
            break;
        } else {
            break;
        }
    }
    let source_steps = ["withSideEffect", "withStrategies", "withoutStrategies", "with", "withBulk", "withSack", "withComputer"];
    steps.retain(|step| !source_steps.contains(&step.name.as_str()));
    Ok(steps)
}

/// The traversal of a GraphSON `g:Bytecode` value: its `step` instructions, each a name and its
/// arguments in GraphSON. Source instructions (`withStrategies` and the like) are left out.
pub(crate) fn parse_bytecode(bytecode: &Json, depth: usize) -> Parsed<Vec<Step>> {
    if depth > MAX_NESTING {
        return error("the traversal nests too deeply");
    }
    let body = match bytecode.field("@type").and_then(Json::text) {
        Some("g:Bytecode") => bytecode.field("@value").unwrap_or(&Json::Null),
        _ => bytecode,
    };
    let Some(Json::Array(instructions)) = body.field("step") else {
        return Ok(Vec::new());
    };
    if instructions.len() > MAX_STEPS {
        return error("the traversal has too many steps");
    }
    let mut steps = Vec::with_capacity(instructions.len());
    for instruction in instructions {
        let Json::Array(parts) = instruction else {
            return error("a bytecode instruction is a list");
        };
        let Some((name, args)) = parts.split_first() else {
            return error("a bytecode instruction is empty");
        };
        let name = name.text().ok_or_else(|| ParseError("a bytecode instruction starts with its name".into()))?;
        let args = args.iter().map(|arg| bytecode_arg(arg, depth)).collect::<Parsed<Vec<_>>>()?;
        steps.push(Step { name: name.to_owned(), args });
    }
    Ok(steps)
}

fn bytecode_arg(json: &Json, depth: usize) -> Parsed<Arg> {
    let kind = json.field("@type").and_then(Json::text).unwrap_or_default();
    let value = json.field("@value").unwrap_or(&Json::Null);
    Ok(match kind {
        "g:Bytecode" => Arg::Traversal(parse_bytecode(json, depth + 1)?),
        "g:P" | "g:TextP" => {
            let name = value.field("predicate").and_then(Json::text).unwrap_or_default().to_owned();
            if !PREDICATES.contains(&name.as_str()) {
                return error(format!("{name} is not a predicate EggDB knows"));
            }
            let argument = crate::graphson::value_of(value.field("value").unwrap_or(&Json::Null), depth + 1)
                .map_err(ParseError)?;
            let values = match argument {
                Value::List(items) if matches!(name.as_str(), "within" | "without" | "between" | "inside" | "outside") => items,
                other => vec![other],
            };
            Arg::Predicate(Predicate { name, values })
        }
        "g:T" | "g:Order" | "g:Column" | "g:Scope" | "g:Cardinality" | "g:Pop" | "g:Direction" | "g:Operator" => {
            Arg::Token(value.text().unwrap_or_default().to_owned())
        }
        _ => Arg::Value(crate::graphson::value_of(json, depth + 1).map_err(ParseError)?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_script_and_its_bytecode_read_as_the_same_steps() {
        let script = parse_script("g.V().has('person', 'age', P.gt(29L)).out(\"knows\").values('name').toList()", &[]).unwrap();
        assert_eq!(script.len(), 4);
        assert_eq!(
            script[1].args[2],
            Arg::Predicate(Predicate { name: "gt".into(), values: vec![Value::Integer(29)] })
        );
        let bytecode = crate::json::parse(
            br#"{"@type":"g:Bytecode","@value":{"step":[["V"],["has","person","age",{"@type":"g:P","@value":{"predicate":"gt","value":{"@type":"g:Int64","@value":29}}}],["out","knows"],["values","name"]]}}"#,
        )
        .unwrap();
        assert_eq!(parse_bytecode(&bytecode, 0).unwrap(), script);
        let nested = parse_script("x = 1; g.V(x).addE('knows').to(__.V(2)).next()", &[("x".into(), Value::Integer(1))]).unwrap();
        assert_eq!(nested[0].args, vec![Arg::Value(Value::Integer(1))]);
        assert_eq!(nested[2].args, vec![Arg::Traversal(vec![Step { name: "V".into(), args: vec![Arg::Value(Value::Integer(2))] }])]);
        assert_eq!(nested[3].name, "limit");
        assert!(parse_script("g.V().order().by('age', Order.desc).by(T.label)", &[]).is_ok());
        assert!(parse_script("Runtime.getRuntime().exec('x')", &[]).is_err());
    }
}
