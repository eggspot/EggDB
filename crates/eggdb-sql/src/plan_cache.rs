use crate::ast::{Expr, ExprKind, Literal, Select, Statement};
use crate::dialect::Dialect;
use crate::eval::children_mut;
use crate::lexer::{Token, TokenKind};
use crate::settings::Settings;
use std::sync::Arc;

const MOST_ENTRIES: usize = 64;
const LONGEST_STATEMENT: usize = 4096;
const NOT_A_LITERAL: u8 = 0;

pub(crate) struct PlanCache {
    entries: Vec<CachedPlan>,
    next_replaced: usize,
    key: Vec<u8>,
}

struct CachedPlan {
    hash: u64,
    key: Vec<u8>,
    dialect: Dialect,
    settings: Arc<Settings>,
    template: Option<Select>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StatementShape {
    first: usize,
    stop: usize,
    hash: u64,
}

impl StatementShape {
    pub(crate) fn stop(self) -> usize {
        self.stop
    }
}

pub(crate) enum CachedStatement {
    Parsed(Statement),
    NeverCached,
    Unknown,
}

impl Default for PlanCache {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            next_replaced: 0,
            key: Vec::new(),
        }
    }
}

fn caches_dialect(dialect: Dialect) -> bool {
    matches!(
        dialect,
        Dialect::PostgreSql | Dialect::MySql | Dialect::Sqlite
    )
}

fn literal_class(kind: &TokenKind<'_>) -> u8 {
    match kind {
        TokenKind::Integer(_) => 1,
        TokenKind::Decimal(_) => 2,
        TokenKind::String(_) => 3,
        _ => NOT_A_LITERAL,
    }
}

fn depends_on_values_outside_the_text(kind: &TokenKind<'_>) -> bool {
    matches!(
        kind,
        TokenKind::Parameter(_)
            | TokenKind::Placeholder
            | TokenKind::NumberedParameter(_)
            | TokenKind::Variable(_)
            | TokenKind::Binary(_)
    )
}

fn starts_a_select(token: Option<&Token<'_>>) -> bool {
    matches!(token.map(|token| &token.kind), Some(TokenKind::Word(word)) if word.eq_ignore_ascii_case("select"))
}

fn hash_of(key: &[u8]) -> u64 {
    let mut hash = 0x9E37_79B9_7F4A_7C15_u64 ^ key.len() as u64;
    let mut chunks = key.chunks_exact(8);
    for chunk in &mut chunks {
        let word = u64::from_le_bytes(chunk.try_into().unwrap_or([0; 8]));
        hash = (hash ^ word)
            .wrapping_mul(0x0000_0100_0000_01B3)
            .rotate_left(29);
    }
    for &byte in chunks.remainder() {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01B3);
    }
    hash
}

pub(crate) fn literal_of(kind: &TokenKind<'_>) -> Option<Literal> {
    Some(match kind {
        TokenKind::Integer(digits) => match digits.parse::<i64>() {
            Ok(value) => Literal::Integer(value),
            Err(_) => Literal::Decimal((*digits).to_owned()),
        },
        TokenKind::Decimal(text) => Literal::Decimal((*text).to_owned()),
        TokenKind::String(text) => Literal::String(text.as_ref().to_owned()),
        _ => return None,
    })
}

fn collect_literals<'e>(expr: &'e mut Expr, found: &mut Vec<&'e mut Expr>) {
    if matches!(expr.kind, ExprKind::Literal(_)) {
        found.push(expr);
        return;
    }
    for child in children_mut(expr) {
        collect_literals(child, found);
    }
}

fn literals_of(select: &mut Select) -> Vec<&mut Expr> {
    let mut found = Vec::new();
    for expr in crate::subquery::select_expressions_mut(select) {
        collect_literals(expr, &mut found);
    }
    found
}

fn literal_tokens<'k, 't>(
    tokens: &'k [Token<'t>],
    shape: StatementShape,
) -> impl Iterator<Item = &'k Token<'t>> {
    tokens
        .get(shape.first..shape.stop)
        .unwrap_or(&[])
        .iter()
        .filter(|token| literal_class(&token.kind) != NOT_A_LITERAL)
}

fn token_at(literals: &[&Token<'_>], position: usize) -> Option<usize> {
    literals
        .binary_search_by_key(&position, |token| token.start)
        .ok()
}

fn literals_come_from_tokens(
    template: &mut Select,
    tokens: &[Token<'_>],
    shape: StatementShape,
) -> bool {
    let literals: Vec<&Token<'_>> = literal_tokens(tokens, shape).collect();
    let mut used = vec![false; literals.len()];
    for slot in literals_of(template) {
        let ExprKind::Literal(found) = &slot.kind else {
            return false;
        };
        let Some(index) = token_at(&literals, slot.position) else {
            return false;
        };
        let Some(token) = literals.get(index) else {
            return false;
        };
        if literal_of(&token.kind).as_ref() != Some(found) {
            return false;
        }
        if let Some(seen) = used.get_mut(index) {
            *seen = true;
        }
    }
    used.iter().all(|&seen| seen)
}

fn with_literals_of(
    template: &Select,
    tokens: &[Token<'_>],
    shape: StatementShape,
) -> Option<Select> {
    let literals: Vec<&Token<'_>> = literal_tokens(tokens, shape).collect();
    let mut select = template.clone();
    for slot in literals_of(&mut select) {
        let token = literals.get(token_at(&literals, slot.position)?)?;
        slot.kind = ExprKind::Literal(literal_of(&token.kind)?);
    }
    Some(select)
}

impl PlanCache {
    /// Describes the statement that starts at token `first`, when it is a `SELECT` this cache
    /// may hold: a dialect whose parser turns every constant into a literal of its token
    /// alone, no parameters or variables, and at most 4 KiB of text. Its key is every token
    /// with each constant's text left out, so statements that differ only in the values of
    /// their constants, written at the same byte offsets, share one entry.
    pub(crate) fn shape(
        &mut self,
        text: &str,
        tokens: &[Token<'_>],
        first: usize,
        dialect: Dialect,
    ) -> Option<StatementShape> {
        if !caches_dialect(dialect) || !starts_a_select(tokens.get(first)) {
            return None;
        }
        let rest = tokens.get(first..)?;
        let stop = first
            + rest
                .iter()
                .position(|token| matches!(token.kind, TokenKind::Semicolon | TokenKind::End))
                .unwrap_or(rest.len());
        let start = tokens.get(first)?.start;
        let end = tokens.get(stop).map_or(text.len(), |token| token.start);
        if end.saturating_sub(start) > LONGEST_STATEMENT || u32::try_from(end).is_err() {
            return None;
        }
        self.key.clear();
        for token in tokens.get(first..stop)? {
            if depends_on_values_outside_the_text(&token.kind) {
                return None;
            }
            let class = literal_class(&token.kind);
            self.key
                .extend_from_slice(&u32::try_from(token.start).ok()?.to_le_bytes());
            self.key
                .extend_from_slice(&u32::try_from(token.end).ok()?.to_le_bytes());
            self.key.push(class);
            if class == NOT_A_LITERAL {
                self.key
                    .extend_from_slice(text.as_bytes().get(token.start..token.end)?);
            }
        }
        Some(StatementShape {
            first,
            stop,
            hash: hash_of(&self.key),
        })
    }

    fn entry(
        &self,
        shape: StatementShape,
        dialect: Dialect,
        settings: &Arc<Settings>,
    ) -> Option<usize> {
        self.entries.iter().position(|entry| {
            entry.hash == shape.hash
                && entry.dialect == dialect
                && Arc::ptr_eq(&entry.settings, settings)
                && entry.key == self.key
        })
    }

    /// The parsed statement for `shape`, the statement [`PlanCache::shape`] described last,
    /// with the constants of `tokens` in place of the cached ones.
    pub(crate) fn find(
        &self,
        shape: StatementShape,
        tokens: &[Token<'_>],
        dialect: Dialect,
        settings: &Arc<Settings>,
    ) -> CachedStatement {
        let Some(index) = self.entry(shape, dialect, settings) else {
            return CachedStatement::Unknown;
        };
        match self
            .entries
            .get(index)
            .and_then(|entry| entry.template.as_ref())
        {
            None => CachedStatement::NeverCached,
            Some(template) => match with_literals_of(template, tokens, shape) {
                Some(select) => CachedStatement::Parsed(Statement::Select(select)),
                None => CachedStatement::Unknown,
            },
        }
    }

    /// Keeps `parsed`, the statement of `shape` that the parser ended at token `end`, for the
    /// statements of the same shape that follow. A statement that is not a plain `SELECT`, or
    /// one with a literal the parser did not take from exactly one of its constants, is
    /// remembered as never cached, so its shape is not examined again.
    pub(crate) fn remember(
        &mut self,
        shape: StatementShape,
        tokens: &[Token<'_>],
        dialect: Dialect,
        settings: &Arc<Settings>,
        parsed: &Statement,
        end: usize,
    ) {
        if self.entry(shape, dialect, settings).is_some() {
            return;
        }
        let template = match parsed {
            Statement::Select(select) if end == shape.stop => {
                let mut template = select.clone();
                literals_come_from_tokens(&mut template, tokens, shape).then_some(template)
            }
            _ => None,
        };
        let plan = CachedPlan {
            hash: shape.hash,
            key: self.key.clone(),
            dialect,
            settings: Arc::clone(settings),
            template,
        };
        if self.entries.len() < MOST_ENTRIES {
            self.entries.push(plan);
            return;
        }
        if let Some(replaced) = self.entries.get_mut(self.next_replaced) {
            *replaced = plan;
        }
        self.next_replaced = (self.next_replaced + 1) % MOST_ENTRIES;
    }

    /// Forgets every cached statement.
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.next_replaced = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::{CachedStatement, PlanCache};
    use crate::ast::{ExprKind, Literal, Statement};
    use crate::dialect::Dialect;
    use crate::lexer::tokenize_in;
    use crate::parser::{Parameters, Parser};
    use crate::settings::Settings;
    use std::sync::Arc;

    fn parsed(text: &str, dialect: Dialect, settings: &Settings) -> (Statement, usize) {
        let tokens = tokenize_in(text, dialect).expect("tokens");
        let mut parser = Parser::new(text, &tokens)
            .with_context(Parameters::Absent, settings)
            .in_dialect(dialect);
        let statement = parser.statement().expect("statement");
        (statement, parser.resume_point().0)
    }

    fn cached(
        cache: &mut PlanCache,
        text: &str,
        dialect: Dialect,
        settings: &Arc<Settings>,
    ) -> CachedStatement {
        let tokens = tokenize_in(text, dialect).expect("tokens");
        let shape = cache.shape(text, &tokens, 0, dialect).expect("shape");
        let found = cache.find(shape, &tokens, dialect, settings);
        if matches!(found, CachedStatement::Unknown) {
            let (statement, end) = parsed(text, dialect, settings);
            cache.remember(shape, &tokens, dialect, settings, &statement, end);
        }
        found
    }

    fn literals(statement: &Statement) -> Vec<Literal> {
        let Statement::Select(select) = statement else {
            panic!("not a select");
        };
        let mut select = select.clone();
        super::literals_of(&mut select)
            .into_iter()
            .filter_map(|expr| match &expr.kind {
                ExprKind::Literal(literal) => Some(literal.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_statement_with_other_constants_of_the_same_length_is_the_parsed_one() {
        let settings = Arc::new(Settings::default());
        let mut cache = PlanCache::default();
        let first = "SELECT name, score FROM people WHERE id = 41 AND name <> 'ab'";
        let second = "SELECT name, score FROM people WHERE id = 97 AND name <> 'cd'";
        assert!(matches!(
            cached(&mut cache, first, Dialect::PostgreSql, &settings),
            CachedStatement::Unknown
        ));
        let CachedStatement::Parsed(statement) =
            cached(&mut cache, second, Dialect::PostgreSql, &settings)
        else {
            panic!("the second statement was not cached");
        };
        assert_eq!(
            statement,
            parsed(second, Dialect::PostgreSql, &settings).0
        );
        assert_eq!(
            literals(&statement),
            vec![Literal::Integer(97), Literal::String("cd".to_owned())]
        );
    }

    #[test]
    fn every_dialect_it_caches_gives_the_parsed_statement() {
        let settings = Arc::new(Settings::default());
        for dialect in [Dialect::PostgreSql, Dialect::MySql, Dialect::Sqlite] {
            let mut cache = PlanCache::default();
            let texts = [
                "SELECT a + 1.5, 'x' FROM t WHERE b BETWEEN 10 AND 20 ORDER BY 1 LIMIT 5",
                "SELECT a + 2.5, 'y' FROM t WHERE b BETWEEN 30 AND 40 ORDER BY 2 LIMIT 7",
                "SELECT a + 9.5, 'z' FROM t WHERE b BETWEEN 99999999999999999999 AND 20 ORDER BY 1 LIMIT 5",
            ];
            for text in texts {
                if let CachedStatement::Parsed(statement) =
                    cached(&mut cache, text, dialect, &settings)
                {
                    assert_eq!(statement, parsed(text, dialect, &settings).0, "{text}");
                }
            }
        }
    }

    #[test]
    fn constants_of_another_length_or_kind_make_another_shape() {
        let settings = Arc::new(Settings::default());
        let mut cache = PlanCache::default();
        cached(
            &mut cache,
            "SELECT * FROM t WHERE id = 5",
            Dialect::PostgreSql,
            &settings,
        );
        for text in [
            "SELECT * FROM t WHERE id = 55",
            "SELECT * FROM t WHERE id = 'a'",
            "SELECT * FROM t WHERE id > 5",
        ] {
            assert!(
                matches!(
                    cached(&mut cache, text, Dialect::PostgreSql, &settings),
                    CachedStatement::Unknown
                ),
                "{text}"
            );
        }
    }

    #[test]
    fn folded_and_unwalked_constants_are_never_cached() {
        let settings = Arc::new(Settings::default());
        for text in [
            "SELECT * FROM t WHERE id = -5",
            "SELECT * FROM t WHERE id IN (SELECT 1)",
        ] {
            let mut cache = PlanCache::default();
            cached(&mut cache, text, Dialect::PostgreSql, &settings);
            assert!(
                matches!(
                    cached(&mut cache, text, Dialect::PostgreSql, &settings),
                    CachedStatement::NeverCached
                ),
                "{text}"
            );
        }
    }

    #[test]
    fn other_settings_dialects_and_statements_are_not_shared() {
        let settings = Arc::new(Settings::default());
        let mut cache = PlanCache::default();
        let text = "SELECT a FROM t WHERE b = 1";
        cached(&mut cache, text, Dialect::PostgreSql, &settings);
        let other = Arc::new(Settings::default());
        assert!(matches!(
            cached(&mut cache, text, Dialect::PostgreSql, &other),
            CachedStatement::Unknown
        ));
        assert!(matches!(
            cached(&mut cache, text, Dialect::Sqlite, &settings),
            CachedStatement::Unknown
        ));
        let tokens = tokenize_in("INSERT INTO t VALUES (1)", Dialect::PostgreSql).expect("tokens");
        assert!(
            cache
                .shape("INSERT INTO t VALUES (1)", &tokens, 0, Dialect::PostgreSql)
                .is_none()
        );
        let tokens = tokenize_in("SELECT $1", Dialect::PostgreSql).expect("tokens");
        assert!(
            cache
                .shape("SELECT $1", &tokens, 0, Dialect::PostgreSql)
                .is_none()
        );
        let tokens = tokenize_in("SELECT 1", Dialect::Oracle).expect("tokens");
        assert!(cache.shape("SELECT 1", &tokens, 0, Dialect::Oracle).is_none());
    }

    #[test]
    fn the_cache_holds_at_most_its_entries() {
        let settings = Arc::new(Settings::default());
        let mut cache = PlanCache::default();
        for width in 1..200 {
            let text = format!("SELECT {}", "9".repeat(width));
            cached(&mut cache, &text, Dialect::PostgreSql, &settings);
        }
        assert!(cache.entries.len() <= super::MOST_ENTRIES);
        cache.clear();
        assert!(cache.entries.is_empty());
    }
}
