use super::{Dialect, PROGRAMS_TABLE, RoutineFrame, Run, SqlResult, Value, parsed, quoted, scheduler_error};
use std::collections::BTreeMap;

const CHAINS_TABLE: &str = "eggdb_internal.oracle_scheduler_chains";
const STEPS_TABLE: &str = "eggdb_internal.oracle_scheduler_chain_steps";
const RULES_TABLE: &str = "eggdb_internal.oracle_scheduler_chain_rules";
pub(super) const CREATE_CHAIN_TABLES: [&str; 3] = [
    "CREATE TABLE IF NOT EXISTS eggdb_internal.oracle_scheduler_chains (chain_name text PRIMARY KEY, enabled boolean NOT NULL)",
    "CREATE TABLE IF NOT EXISTS eggdb_internal.oracle_scheduler_chain_steps (chain_name text NOT NULL, step_name text NOT NULL, program_name text NOT NULL, PRIMARY KEY (chain_name, step_name))",
    "CREATE TABLE IF NOT EXISTS eggdb_internal.oracle_scheduler_chain_rules (chain_name text NOT NULL, rule_name text NOT NULL, condition text NOT NULL, action text NOT NULL, PRIMARY KEY (chain_name, rule_name))",
];
pub(super) const CHAIN_MEMBERS: [&str; 7] = [
    "create_chain",
    "drop_chain",
    "define_chain_step",
    "drop_chain_step",
    "define_chain_rule",
    "drop_chain_rule",
    "run_chain",
];
const INVALID_VALUE: u16 = 27_452;
const ALREADY_EXISTS: u16 = 27_477;
const DOES_NOT_EXIST: u16 = 27_476;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StepState {
    NotStarted,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WantedState {
    Completed,
    Succeeded,
    Failed,
    NotStarted,
    Never,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Condition {
    Constant(bool),
    Step { step: String, state: WantedState },
    Not(Box<Condition>),
    And(Box<Condition>, Box<Condition>),
    Or(Box<Condition>, Box<Condition>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RuleAction {
    Start(Vec<String>),
    End,
}

fn tokens(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    for character in text.chars() {
        match character {
            '(' | ')' => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
                tokens.push(character.to_string());
            }
            '"' => {}
            space if space.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            other => current.push(other.to_ascii_uppercase()),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

struct ConditionParser {
    tokens: Vec<String>,
    at: usize,
}

impl ConditionParser {
    fn parse(text: &str) -> Result<Condition, String> {
        let mut parser = Self {
            tokens: tokens(text),
            at: 0,
        };
        let condition = parser.any()?;
        match parser.tokens.get(parser.at) {
            None => Ok(condition),
            Some(extra) => Err(format!("unexpected \"{extra}\" in chain rule condition \"{text}\"")),
        }
    }

    fn take(&mut self, word: &str) -> bool {
        if self.tokens.get(self.at).is_some_and(|token| token == word) {
            self.at += 1;
            return true;
        }
        false
    }

    fn next_token(&mut self) -> Option<String> {
        let token = self.tokens.get(self.at).cloned();
        if token.is_some() {
            self.at += 1;
        }
        token
    }

    fn any(&mut self) -> Result<Condition, String> {
        let mut left = self.all()?;
        while self.take("OR") {
            let right = self.all()?;
            left = Condition::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn all(&mut self) -> Result<Condition, String> {
        let mut left = self.single()?;
        while self.take("AND") {
            let right = self.single()?;
            left = Condition::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn single(&mut self) -> Result<Condition, String> {
        if self.take("NOT") {
            return Ok(Condition::Not(Box::new(self.single()?)));
        }
        if self.take("(") {
            let inner = self.any()?;
            if !self.take(")") {
                return Err("a chain rule condition misses a closing parenthesis".to_owned());
            }
            return Ok(inner);
        }
        let word = self
            .next_token()
            .ok_or_else(|| "a chain rule condition ends too early".to_owned())?;
        match word.as_str() {
            "TRUE" => return Ok(Condition::Constant(true)),
            "FALSE" => return Ok(Condition::Constant(false)),
            ")" | "AND" | "OR" => {
                return Err(format!("unexpected \"{word}\" in a chain rule condition"));
            }
            _ => {}
        }
        let state = match self.next_token().as_deref() {
            Some("COMPLETED") => WantedState::Completed,
            Some("SUCCEEDED") => WantedState::Succeeded,
            Some("FAILED") => WantedState::Failed,
            Some("NOT_STARTED") => WantedState::NotStarted,
            Some("STOPPED" | "RUNNING" | "PAUSED" | "STALLED") => WantedState::Never,
            Some(other) => return Err(format!("unknown chain step state \"{other}\"")),
            None => return Err(format!("chain step {word} has no state to test")),
        };
        Ok(Condition::Step { step: word, state })
    }
}

fn holds(condition: &Condition, states: &BTreeMap<String, StepState>) -> Result<bool, String> {
    Ok(match condition {
        Condition::Constant(value) => *value,
        Condition::Not(inner) => !holds(inner, states)?,
        Condition::And(left, right) => holds(left, states)? && holds(right, states)?,
        Condition::Or(left, right) => holds(left, states)? || holds(right, states)?,
        Condition::Step { step, state } => {
            let current = states
                .get(step)
                .ok_or_else(|| format!("\"{step}\" is not a step of the chain"))?;
            match state {
                WantedState::Completed => *current != StepState::NotStarted,
                WantedState::Succeeded => *current == StepState::Succeeded,
                WantedState::Failed => *current == StepState::Failed,
                WantedState::NotStarted => *current == StepState::NotStarted,
                WantedState::Never => false,
            }
        }
    })
}

fn parse_action(text: &str) -> Result<RuleAction, String> {
    let upper = text.trim().to_ascii_uppercase();
    if upper == "END" || upper.starts_with("END ") {
        return Ok(RuleAction::End);
    }
    if let Some(rest) = upper.strip_prefix("START ") {
        let steps: Vec<String> = rest
            .split(',')
            .map(|step| step.trim().trim_matches('"').to_owned())
            .filter(|step| !step.is_empty())
            .collect();
        if !steps.is_empty() {
            return Ok(RuleAction::Start(steps));
        }
    }
    Err(format!("unknown chain rule action \"{text}\""))
}

fn step_list(text: &str) -> Vec<String> {
    text.split(',')
        .map(|step| step.trim().trim_matches('"').to_ascii_uppercase())
        .filter(|step| !step.is_empty())
        .collect()
}

impl Run<'_> {
    pub(super) fn chain_exists(&mut self, chain: &str) -> SqlResult<bool> {
        let found = self.scheduler_sql(&format!(
            "SELECT 1 FROM {CHAINS_TABLE} WHERE chain_name = {}",
            quoted(chain)
        ))?;
        Ok(!found.rows.is_empty())
    }

    pub(super) fn set_chain_enabled(&mut self, chain: &str, enabled: bool) -> SqlResult<()> {
        self.scheduler_sql(&format!(
            "UPDATE {CHAINS_TABLE} SET enabled = {enabled} WHERE chain_name = {}",
            quoted(chain)
        ))?;
        Ok(())
    }

    fn require_chain(&mut self, chain: &str) -> SqlResult<()> {
        if self.chain_exists(chain)? {
            Ok(())
        } else {
            Err(scheduler_error(DOES_NOT_EXIST, format!("\"{chain}\" does not exist")))
        }
    }

    pub(super) fn scheduler_chain_call(
        &mut self,
        member: &str,
        chain: &str,
        arguments: &[(String, Value)],
    ) -> SqlResult<()> {
        let text_of = |name: &str| {
            arguments
                .iter()
                .find(|(known, _)| known == name)
                .and_then(|(_, value)| value.to_text())
                .filter(|text| !text.trim().is_empty())
        };
        let required = |name: &str| {
            text_of(name)
                .ok_or_else(|| scheduler_error(INVALID_VALUE, format!("{name} is required")))
        };
        match member {
            "create_chain" => {
                if self.chain_exists(chain)? {
                    return Err(scheduler_error(ALREADY_EXISTS, format!("\"{chain}\" already exists")));
                }
                self.scheduler_sql(&format!(
                    "INSERT INTO {CHAINS_TABLE} VALUES ({}, false)",
                    quoted(chain)
                ))?;
            }
            "drop_chain" => {
                self.require_chain(chain)?;
                for table in [STEPS_TABLE, RULES_TABLE, CHAINS_TABLE] {
                    self.scheduler_sql(&format!(
                        "DELETE FROM {table} WHERE chain_name = {}",
                        quoted(chain)
                    ))?;
                }
            }
            "define_chain_step" => {
                self.require_chain(chain)?;
                let step = required("step_name")?.trim().to_ascii_uppercase();
                let program = required("program_name")?.trim().to_ascii_uppercase();
                self.scheduler_sql(&format!(
                    "DELETE FROM {STEPS_TABLE} WHERE chain_name = {} AND step_name = {}",
                    quoted(chain),
                    quoted(&step)
                ))?;
                self.scheduler_sql(&format!(
                    "INSERT INTO {STEPS_TABLE} VALUES ({}, {}, {})",
                    quoted(chain),
                    quoted(&step),
                    quoted(&program)
                ))?;
            }
            "define_chain_rule" => {
                self.require_chain(chain)?;
                let condition = required("condition")?;
                let action = required("action")?;
                ConditionParser::parse(&condition)
                    .map_err(|message| scheduler_error(INVALID_VALUE, message))?;
                parse_action(&action).map_err(|message| scheduler_error(INVALID_VALUE, message))?;
                let rule = match text_of("rule_name") {
                    Some(rule) => rule.trim().to_ascii_uppercase(),
                    None => {
                        let counted = self.scheduler_sql(&format!(
                            "SELECT count(*) FROM {RULES_TABLE} WHERE chain_name = {}",
                            quoted(chain)
                        ))?;
                        let count = counted
                            .rows
                            .first()
                            .and_then(|row| row.first())
                            .and_then(Value::as_integer)
                            .unwrap_or(0);
                        format!("RULE${}", count + 1)
                    }
                };
                self.scheduler_sql(&format!(
                    "DELETE FROM {RULES_TABLE} WHERE chain_name = {} AND rule_name = {}",
                    quoted(chain),
                    quoted(&rule)
                ))?;
                self.scheduler_sql(&format!(
                    "INSERT INTO {RULES_TABLE} VALUES ({}, {}, {}, {})",
                    quoted(chain),
                    quoted(&rule),
                    quoted(&condition),
                    quoted(&action)
                ))?;
            }
            "drop_chain_step" | "drop_chain_rule" => {
                self.require_chain(chain)?;
                let (table, column, argument) = if member == "drop_chain_step" {
                    (STEPS_TABLE, "step_name", "step_name")
                } else {
                    (RULES_TABLE, "rule_name", "rule_name")
                };
                let name = required(argument)?.trim().to_ascii_uppercase();
                let removed = self.scheduler_sql(&format!(
                    "DELETE FROM {table} WHERE chain_name = {} AND {column} = {} RETURNING 1",
                    quoted(chain),
                    quoted(&name)
                ))?;
                if removed.rows.is_empty() {
                    return Err(scheduler_error(DOES_NOT_EXIST, format!("\"{name}\" does not exist")));
                }
            }
            "run_chain" => self.run_chain(chain, text_of("start_steps").as_deref())?,
            _ => {}
        }
        Ok(())
    }

    fn run_chain(&mut self, chain: &str, start_steps: Option<&str>) -> SqlResult<()> {
        let found = self.scheduler_sql(&format!(
            "SELECT enabled FROM {CHAINS_TABLE} WHERE chain_name = {}",
            quoted(chain)
        ))?;
        let Some(row) = found.rows.first() else {
            return Err(scheduler_error(DOES_NOT_EXIST, format!("\"{chain}\" does not exist")));
        };
        if !matches!(row.first(), Some(Value::Boolean(true))) {
            return Err(scheduler_error(DOES_NOT_EXIST, format!("chain \"{chain}\" is not enabled")));
        }
        let listed = self.scheduler_sql(&format!(
            "SELECT step_name, program_name FROM {STEPS_TABLE} WHERE chain_name = {} ORDER BY step_name",
            quoted(chain)
        ))?;
        let steps: Vec<(String, String)> = listed
            .rows
            .iter()
            .map(|row| {
                (
                    row.first().and_then(Value::to_text).unwrap_or_default(),
                    row.get(1).and_then(Value::to_text).unwrap_or_default(),
                )
            })
            .collect();
        let listed = self.scheduler_sql(&format!(
            "SELECT condition, action FROM {RULES_TABLE} WHERE chain_name = {} ORDER BY rule_name",
            quoted(chain)
        ))?;
        let mut rules = Vec::with_capacity(listed.rows.len());
        for row in &listed.rows {
            let condition = row.first().and_then(Value::to_text).unwrap_or_default();
            let action = row.get(1).and_then(Value::to_text).unwrap_or_default();
            rules.push((
                ConditionParser::parse(&condition)
                    .map_err(|message| scheduler_error(INVALID_VALUE, message))?,
                parse_action(&action).map_err(|message| scheduler_error(INVALID_VALUE, message))?,
            ));
        }
        let mut states: BTreeMap<String, StepState> = steps
            .iter()
            .map(|(step, _)| (step.clone(), StepState::NotStarted))
            .collect();
        let unknown = |step: &str| {
            scheduler_error(INVALID_VALUE, format!("\"{step}\" is not a step of chain \"{chain}\""))
        };
        let mut pending = start_steps.map(step_list).unwrap_or_default();
        if let Some(step) = pending.iter().find(|step| !states.contains_key(*step)) {
            return Err(unknown(step));
        }
        for _ in 0..=steps.len() + 1 {
            if pending.is_empty() {
                let mut ended = false;
                for (condition, action) in &rules {
                    if !holds(condition, &states)
                        .map_err(|message| scheduler_error(INVALID_VALUE, message))?
                    {
                        continue;
                    }
                    match action {
                        RuleAction::End => ended = true,
                        RuleAction::Start(list) => {
                            for step in list {
                                match states.get(step) {
                                    None => return Err(unknown(step)),
                                    Some(StepState::NotStarted) if !pending.contains(step) => {
                                        pending.push(step.clone());
                                    }
                                    Some(_) => {}
                                }
                            }
                        }
                    }
                }
                if ended || pending.is_empty() {
                    break;
                }
            }
            for step in std::mem::take(&mut pending) {
                let state = self.run_chain_step(&steps, &step)?;
                states.insert(step, state);
            }
        }
        Ok(())
    }

    fn run_chain_step(&mut self, steps: &[(String, String)], step: &str) -> SqlResult<StepState> {
        let Some((_, program)) = steps.iter().find(|(name, _)| name == step) else {
            return Ok(StepState::Failed);
        };
        let found = self.scheduler_sql(&format!(
            "SELECT program_type, program_action FROM {PROGRAMS_TABLE} WHERE program_name = {}",
            quoted(program)
        ))?;
        let Some(row) = found.rows.first() else {
            return Ok(StepState::Failed);
        };
        let program_type = row.first().and_then(Value::to_text).unwrap_or_default();
        let action = row.get(1).and_then(Value::to_text).unwrap_or_default();
        let block = crate::jobs::scheduler_block(&program_type, &action);
        let outcome = parsed(&block, &self.settings, &RoutineFrame::default(), Dialect::Oracle)
            .and_then(|statement| self.execute(statement));
        Ok(if outcome.is_ok() {
            StepState::Succeeded
        } else {
            StepState::Failed
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conditions_test_step_states_with_and_or_not_and_parentheses() {
        let states: BTreeMap<String, StepState> = [
            ("A".to_owned(), StepState::Succeeded),
            ("B".to_owned(), StepState::Failed),
            ("C".to_owned(), StepState::NotStarted),
        ]
        .into_iter()
        .collect();
        let check = |text: &str| holds(&ConditionParser::parse(text).expect(text), &states).expect(text);
        assert!(check("TRUE"));
        assert!(check("a SUCCEEDED AND b FAILED"));
        assert!(check("a completed and b completed"));
        assert!(!check("c COMPLETED"));
        assert!(check("NOT c COMPLETED"));
        assert!(check("(c SUCCEEDED OR b FAILED) AND c NOT_STARTED"));
        assert!(!check("a RUNNING"));
        assert!(ConditionParser::parse("a FINISHED").is_err());
        assert!(ConditionParser::parse("(a SUCCEEDED").is_err());
        assert!(holds(&ConditionParser::parse("z SUCCEEDED").expect("z"), &states).is_err());
    }

    #[test]
    fn actions_start_steps_or_end_the_chain() {
        assert_eq!(
            parse_action("START b, \"c\""),
            Ok(RuleAction::Start(vec!["B".to_owned(), "C".to_owned()]))
        );
        assert_eq!(parse_action("end"), Ok(RuleAction::End));
        assert_eq!(parse_action("END 3"), Ok(RuleAction::End));
        assert!(parse_action("STOP a").is_err());
    }
}
