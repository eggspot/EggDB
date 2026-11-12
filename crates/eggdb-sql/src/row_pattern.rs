use crate::error::{SqlError, SqlResult, SqlState};
use crate::json::{Json, parse_document};

const MOST_ROWS: usize = 1_000_000;
const MOST_INSTRUCTIONS: usize = 10_000;
const MOST_STEPS: usize = 20_000_000;
const MOST_PATTERN_BYTES: usize = 4096;
const MOST_NESTING: usize = 50;
const MOST_REPEAT: u32 = 1000;

#[derive(Debug, Clone)]
enum Pattern {
    Variable(String),
    Sequence(Vec<Pattern>),
    Either(Box<Pattern>, Box<Pattern>),
    Repeat { inner: Box<Pattern>, least: u32, most: Option<u32>, greedy: bool },
}

struct PatternParser<'a> {
    text: &'a [u8],
    at: usize,
    depth: usize,
}

impl PatternParser<'_> {
    fn skip_spaces(&mut self) {
        while self.at < self.text.len() && self.text[self.at].is_ascii_whitespace() {
            self.at += 1;
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.skip_spaces();
        self.text.get(self.at).copied()
    }

    fn alternation(&mut self) -> Option<Pattern> {
        self.depth += 1;
        if self.depth > MOST_NESTING {
            return None;
        }
        let mut left = self.sequence()?;
        while self.peek() == Some(b'|') {
            self.at += 1;
            let right = self.sequence()?;
            left = Pattern::Either(Box::new(left), Box::new(right));
        }
        self.depth -= 1;
        Some(left)
    }

    fn sequence(&mut self) -> Option<Pattern> {
        let mut items = Vec::new();
        while let Some(next) = self.peek() {
            if next == b'|' || next == b')' {
                break;
            }
            items.push(self.quantified()?);
        }
        if items.is_empty() {
            return None;
        }
        Some(if items.len() == 1 { items.remove(0) } else { Pattern::Sequence(items) })
    }

    fn number(&mut self) -> Option<u32> {
        self.skip_spaces();
        let start = self.at;
        while self.at < self.text.len() && self.text[self.at].is_ascii_digit() {
            self.at += 1;
        }
        std::str::from_utf8(self.text.get(start..self.at)?).ok()?.parse().ok()
    }

    fn quantified(&mut self) -> Option<Pattern> {
        let mut item = self.atom()?;
        loop {
            let (least, most) = match self.peek() {
                Some(b'*') => {
                    self.at += 1;
                    (0, None)
                }
                Some(b'+') => {
                    self.at += 1;
                    (1, None)
                }
                Some(b'?') => {
                    self.at += 1;
                    (0, Some(1))
                }
                Some(b'{') => {
                    self.at += 1;
                    let least = if self.peek() == Some(b',') { 0 } else { self.number()? };
                    let most = if self.peek() == Some(b',') {
                        self.at += 1;
                        if self.peek() == Some(b'}') { None } else { Some(self.number()?) }
                    } else {
                        Some(least)
                    };
                    if self.peek() != Some(b'}') {
                        return None;
                    }
                    self.at += 1;
                    (least, most)
                }
                _ => return Some(item),
            };
            if least > MOST_REPEAT || most.is_some_and(|most| most > MOST_REPEAT || most < least) {
                return None;
            }
            let greedy = if self.text.get(self.at) == Some(&b'?') {
                self.at += 1;
                false
            } else {
                true
            };
            item = Pattern::Repeat { inner: Box::new(item), least, most, greedy };
        }
    }

    fn atom(&mut self) -> Option<Pattern> {
        match self.peek()? {
            b'(' => {
                self.at += 1;
                let inner = self.alternation()?;
                if self.peek() != Some(b')') {
                    return None;
                }
                self.at += 1;
                Some(inner)
            }
            byte if byte.is_ascii_alphabetic() || byte == b'_' => {
                let start = self.at;
                while self.at < self.text.len() && (self.text[self.at].is_ascii_alphanumeric() || self.text[self.at] == b'_') {
                    self.at += 1;
                }
                let name = std::str::from_utf8(self.text.get(start..self.at)?).ok()?;
                Some(Pattern::Variable(name.to_ascii_uppercase()))
            }
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Instruction {
    Row(usize),
    Split(usize, usize),
    Jump(usize),
    Match,
}

struct Compiler {
    program: Vec<Instruction>,
    variables: Vec<String>,
}

impl Compiler {
    fn emit(&mut self, instruction: Instruction) -> Option<usize> {
        if self.program.len() >= MOST_INSTRUCTIONS {
            return None;
        }
        self.program.push(instruction);
        Some(self.program.len() - 1)
    }

    fn variable(&mut self, name: &str) -> usize {
        match self.variables.iter().position(|known| known == name) {
            Some(index) => index,
            None => {
                self.variables.push(name.to_owned());
                self.variables.len() - 1
            }
        }
    }

    fn optional(&mut self, inner: &Pattern, greedy: bool) -> Option<()> {
        let split = self.emit(Instruction::Split(0, 0))?;
        let body = self.program.len();
        self.compile(inner)?;
        let end = self.program.len();
        self.program[split] = if greedy { Instruction::Split(body, end) } else { Instruction::Split(end, body) };
        Some(())
    }

    fn compile(&mut self, pattern: &Pattern) -> Option<()> {
        match pattern {
            Pattern::Variable(name) => {
                let index = self.variable(name);
                self.emit(Instruction::Row(index))?;
            }
            Pattern::Sequence(items) => {
                for item in items {
                    self.compile(item)?;
                }
            }
            Pattern::Either(left, right) => {
                let split = self.emit(Instruction::Split(0, 0))?;
                let first = self.program.len();
                self.compile(left)?;
                let jump = self.emit(Instruction::Jump(0))?;
                let second = self.program.len();
                self.compile(right)?;
                let end = self.program.len();
                self.program[split] = Instruction::Split(first, second);
                self.program[jump] = Instruction::Jump(end);
            }
            Pattern::Repeat { inner, least, most, greedy } => {
                for _ in 0..*least {
                    self.compile(inner)?;
                }
                match most {
                    Some(most) => {
                        for _ in *least..*most {
                            self.optional(inner, *greedy)?;
                        }
                    }
                    None => {
                        let split = self.emit(Instruction::Split(0, 0))?;
                        let body = self.program.len();
                        self.compile(inner)?;
                        self.emit(Instruction::Jump(split))?;
                        let end = self.program.len();
                        self.program[split] = if *greedy { Instruction::Split(body, end) } else { Instruction::Split(end, body) };
                    }
                }
            }
        }
        Some(())
    }
}

struct Matcher<'a> {
    program: &'a [Instruction],
    rows: &'a [Vec<Option<bool>>],
    steps: usize,
}

impl Matcher<'_> {
    fn holds(&self, row: usize, variable: usize) -> bool {
        self.rows.get(row).is_some_and(|flags| flags.get(variable).copied().flatten().unwrap_or(true))
    }

    fn run(&mut self, start: usize) -> SqlResult<Option<(usize, Vec<usize>)>> {
        let mut assigned: Vec<usize> = Vec::new();
        let mut pending: Vec<(usize, usize, usize, usize)> = vec![(0, start, 0, usize::MAX)];
        while let Some((mut pc, mut row, kept, loop_row)) = pending.pop() {
            assigned.truncate(kept);
            let mut entered_at = loop_row;
            loop {
                self.steps += 1;
                if self.steps > MOST_STEPS {
                    return Err(SqlError::new(SqlState::PROGRAM_LIMIT_EXCEEDED, "row_pattern_matches: the pattern takes too many steps on these rows"));
                }
                match self.program.get(pc) {
                    Some(Instruction::Row(variable)) => {
                        if !self.holds(row, *variable) {
                            break;
                        }
                        assigned.push(*variable);
                        row += 1;
                        pc += 1;
                    }
                    Some(Instruction::Split(first, second)) => {
                        pending.push((*second, row, assigned.len(), entered_at));
                        pc = *first;
                    }
                    Some(Instruction::Jump(target)) => {
                        if *target < pc {
                            if entered_at == row {
                                if let Some(Instruction::Split(first, second)) = self.program.get(*target) {
                                    pc = if *first == target + 1 { *second } else { *first };
                                    continue;
                                }
                            }
                            entered_at = row;
                        }
                        pc = *target;
                    }
                    Some(Instruction::Match) => return Ok(Some((row, assigned))),
                    None => break,
                }
            }
        }
        Ok(None)
    }
}

/// Finds the matches of a row pattern (`MATCH_RECOGNIZE`'s `PATTERN`, with variables,
/// concatenation, `|`, grouping and the `*`, `+`, `?` and `{n,m}` quantifiers, greedy or
/// reluctant) over `rows`, a JSON array with one object per row saying which variables hold
/// for it (`{"UP": true, "DOWN": false}`); a variable a row does not mention holds, as an
/// undefined pattern variable does. Matches don't overlap: after each, the search goes on
/// past its last row. Returns a JSON array of `{"start", "end", "classifiers"}`, rows counted
/// from 0.
pub(crate) fn matches(rows_json: &str, pattern_text: &str) -> SqlResult<String> {
    let invalid = |message: &str| SqlError::new(SqlState::INVALID_PARAMETER_VALUE, format!("row_pattern_matches: {message}"));
    if pattern_text.len() > MOST_PATTERN_BYTES {
        return Err(invalid("the pattern is too long"));
    }
    let mut parser = PatternParser { text: pattern_text.as_bytes(), at: 0, depth: 0 };
    let pattern = parser.alternation().ok_or_else(|| invalid("the pattern is not valid"))?;
    if parser.peek().is_some() {
        return Err(invalid("the pattern is not valid"));
    }
    let mut compiler = Compiler { program: Vec::new(), variables: Vec::new() };
    compiler.compile(&pattern).ok_or_else(|| invalid("the pattern is too large"))?;
    compiler.emit(Instruction::Match).ok_or_else(|| invalid("the pattern is too large"))?;
    let document = parse_document(rows_json).map_err(|error| invalid(&error.message))?;
    let Json::Array(items) = document.value else {
        return Err(invalid("the rows must be a JSON array"));
    };
    if items.len() > MOST_ROWS {
        return Err(SqlError::new(SqlState::PROGRAM_LIMIT_EXCEEDED, format!("row_pattern_matches takes at most {MOST_ROWS} rows")));
    }
    let mut rows = Vec::with_capacity(items.len());
    for item in &items {
        let Json::Object(members) = &item.value else {
            return Err(invalid("each row must be an object of variables"));
        };
        let mut flags = vec![None; compiler.variables.len()];
        for (name, value) in members {
            if let Some(index) = compiler.variables.iter().position(|variable| variable.eq_ignore_ascii_case(name)) {
                flags[index] = Some(matches!(value.value, Json::Boolean(true)));
            }
        }
        rows.push(flags);
    }
    let mut matcher = Matcher { program: &compiler.program, rows: &rows, steps: 0 };
    let mut out = String::from("[");
    let mut start = 0;
    let mut found = 0usize;
    while start < rows.len() {
        match matcher.run(start)? {
            Some((end, assigned)) if end > start => {
                if found > 0 {
                    out.push_str(", ");
                }
                found += 1;
                let classifiers: Vec<String> = assigned.iter().map(|variable| format!("\"{}\"", compiler.variables[*variable])).collect();
                out.push_str(&format!("{{\"start\": {start}, \"end\": {}, \"classifiers\": [{}]}}", end - 1, classifiers.join(", ")));
                start = end;
            }
            _ => start += 1,
        }
    }
    out.push(']');
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_v_shape_is_found_greedily_without_overlap() {
        let rows = r#"[{"DOWN": false, "UP": false}, {"DOWN": true, "UP": false}, {"DOWN": true, "UP": false}, {"DOWN": false, "UP": true}, {"DOWN": false, "UP": true}, {"DOWN": true, "UP": false}]"#;
        assert_eq!(
            matches(rows, "STRT DOWN+ UP+").unwrap(),
            r#"[{"start": 0, "end": 4, "classifiers": ["STRT", "DOWN", "DOWN", "UP", "UP"]}]"#
        );
        assert_eq!(matches(rows, "DOWN{2} UP?").unwrap(), r#"[{"start": 1, "end": 3, "classifiers": ["DOWN", "DOWN", "UP"]}]"#);
        assert_eq!(matches(rows, "(UP | DOWN)+?").unwrap().matches("start").count(), 5);
    }

    #[test]
    fn empty_loops_and_bad_patterns_stop() {
        assert_eq!(matches(r#"[{"A": false}, {"A": true}]"#, "(A?)* B").unwrap(), r#"[{"start": 0, "end": 0, "classifiers": ["B"]}, {"start": 1, "end": 1, "classifiers": ["B"]}]"#);
        assert!(matches("[]", "A |").is_err());
        assert!(matches("[]", "A{5,2}").is_err());
        assert!(matches("[{}]", &"(".repeat(100)).is_err());
    }
}
