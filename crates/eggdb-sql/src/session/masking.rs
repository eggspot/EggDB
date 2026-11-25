use super::{Block, QueryResult, Session};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::{Token, TokenKind, tokenize_in};
use crate::masking::{self, MaskFunction, ResultMasks};
use crate::store::{KeyValueRead, KeyValueWrite};
use std::collections::HashSet;

const DEFAULT_SCHEMAS: [&str; 2] = ["dbo", "public"];

/// A `CREATE TABLE` whose columns carry `MASKED WITH (FUNCTION = '...')`: the table, its masks,
/// and the statement's text without them, which the engine runs.
pub(super) struct MaskedCreate {
    pub(super) table: String,
    pub(super) masks: Vec<(String, MaskFunction)>,
    pub(super) text: String,
}

enum MaskCommand {
    Add { table: String, column: String, function: MaskFunction },
    Drop { table: String, column: String },
    Grant { role: String },
    Revoke { role: String },
}

fn identifier(token: &Token<'_>) -> Option<String> {
    match &token.kind {
        TokenKind::Word(word) => Some(word.to_ascii_lowercase()),
        TokenKind::QuotedIdentifier(name) => Some(name.clone()),
        _ => None,
    }
}

fn is_word(token: Option<&Token<'_>>, word: &str) -> bool {
    matches!(token.map(|token| &token.kind), Some(TokenKind::Word(found)) if found.eq_ignore_ascii_case(word))
}

fn significant<'t>(tokens: &'t [Token<'t>]) -> &'t [Token<'t>] {
    let mut end = tokens.len();
    while end > 0 && matches!(tokens[end - 1].kind, TokenKind::End | TokenKind::Semicolon) {
        end -= 1;
    }
    &tokens[..end]
}

fn table_name(tokens: &[Token<'_>], at: usize) -> Option<(String, usize)> {
    let first = identifier(tokens.get(at)?)?;
    if matches!(tokens.get(at + 1).map(|token| &token.kind), Some(TokenKind::Dot)) {
        let second = identifier(tokens.get(at + 2)?)?;
        let name = if DEFAULT_SCHEMAS.contains(&first.as_str()) { second } else { format!("{first}.{second}") };
        return Some((name, at + 3));
    }
    Some((first, at + 1))
}

fn masked_with(tokens: &[Token<'_>], at: usize) -> Option<(MaskFunctionText, usize)> {
    if !(is_word(tokens.get(at), "masked") && is_word(tokens.get(at + 1), "with")) {
        return None;
    }
    if !matches!(tokens.get(at + 2).map(|token| &token.kind), Some(TokenKind::LeftParen)) || !is_word(tokens.get(at + 3), "function") {
        return None;
    }
    if !matches!(tokens.get(at + 4).map(|token| &token.kind), Some(TokenKind::Operator("="))) {
        return None;
    }
    let Some(TokenKind::String(text)) = tokens.get(at + 5).map(|token| &token.kind) else {
        return None;
    };
    if !matches!(tokens.get(at + 6).map(|token| &token.kind), Some(TokenKind::RightParen)) {
        return None;
    }
    Some((MaskFunctionText(text.to_string()), at + 7))
}

struct MaskFunctionText(String);

fn command_of(tokens: &[Token<'_>]) -> SqlResult<Option<MaskCommand>> {
    let tokens = significant(tokens);
    if is_word(tokens.first(), "grant") || is_word(tokens.first(), "revoke") {
        let granting = is_word(tokens.first(), "grant");
        let preposition = if granting { "to" } else { "from" };
        if tokens.len() == 4 && is_word(tokens.get(1), "unmask") && is_word(tokens.get(2), preposition) {
            let Some(role) = identifier(&tokens[3]) else {
                return Ok(None);
            };
            return Ok(Some(if granting { MaskCommand::Grant { role } } else { MaskCommand::Revoke { role } }));
        }
        return Ok(None);
    }
    if !(is_word(tokens.first(), "alter") && is_word(tokens.get(1), "table")) {
        return Ok(None);
    }
    let Some((table, at)) = table_name(tokens, 2) else {
        return Ok(None);
    };
    if !(is_word(tokens.get(at), "alter") && is_word(tokens.get(at + 1), "column")) {
        return Ok(None);
    }
    let Some(column) = tokens.get(at + 2).and_then(identifier) else {
        return Ok(None);
    };
    let rest = at + 3;
    if is_word(tokens.get(rest), "drop") && is_word(tokens.get(rest + 1), "masked") && tokens.len() == rest + 2 {
        return Ok(Some(MaskCommand::Drop { table, column }));
    }
    if is_word(tokens.get(rest), "add")
        && let Some((text, end)) = masked_with(tokens, rest + 1)
        && end == tokens.len()
    {
        let function = MaskFunction::parse(&text.0)?;
        return Ok(Some(MaskCommand::Add { table, column, function }));
    }
    Ok(None)
}

const ALIASING_WORDS: [&str; 6] = ["as", "union", "intersect", "except", "minus", "with"];

/// Whether every result column of the statement in `tokens` is named after the stored column
/// it shows: one `SELECT` whose list holds only plain or qualified column names, or `*`, and no
/// `AS`, set operation or `WITH` anywhere, so no result column can carry another column's
/// values under its name.
fn names_its_columns_plainly(tokens: &[Token<'_>]) -> bool {
    let tokens = significant(tokens);
    let selects = tokens.iter().filter(|token| is_word(Some(token), "select")).count();
    if selects != 1 || !is_word(tokens.first(), "select") {
        return false;
    }
    if tokens.iter().any(|token| ALIASING_WORDS.iter().any(|word| is_word(Some(token), word))) {
        return false;
    }
    let mut expecting_name = true;
    for token in &tokens[1..] {
        if is_word(Some(token), "from") {
            return !expecting_name;
        }
        match &token.kind {
            TokenKind::Operator("*") if expecting_name => expecting_name = false,
            TokenKind::Word(_) | TokenKind::QuotedIdentifier(_) if expecting_name => expecting_name = false,
            TokenKind::Dot if !expecting_name => expecting_name = true,
            TokenKind::Comma if !expecting_name => expecting_name = true,
            _ => return false,
        }
    }
    !expecting_name
}

fn undefined_table(table: &str) -> SqlError {
    SqlError::new(SqlState::UNDEFINED_TABLE, format!("relation \"{table}\" does not exist")).with_sqlserver_code(4902)
}

fn column_of(reader: &dyn KeyValueRead, table: &str, column: &str) -> SqlResult<String> {
    let bytes = reader.get(&crate::encoding::table_key(table))?.ok_or_else(|| undefined_table(table))?;
    let definition = crate::encoding::TableDefinition::decode(table, &bytes)?;
    definition
        .columns
        .iter()
        .find(|known| known.name.eq_ignore_ascii_case(column))
        .map(|known| known.name.clone())
        .ok_or_else(|| {
            SqlError::new(SqlState::UNDEFINED_COLUMN, format!("column \"{column}\" of relation \"{table}\" does not exist")).with_sqlserver_code(4924)
        })
}

fn not_allowed(what: &str) -> SqlError {
    SqlError::new(SqlState::INSUFFICIENT_PRIVILEGE, format!("permission denied: only a superuser{what} may do this")).with_sqlserver_code(15247)
}

const MOST_NESTED_OBJECTS: usize = 32;
const WRITING_WORDS: [&str; 7] = ["insert", "update", "merge", "create", "replace", "upsert", "copy"];
const DYNAMIC_WORDS: [&str; 6] = ["do", "execute", "exec", "prepare", "call", "sp_executesql"];

fn first_word(tokens: &[Token<'_>]) -> Option<String> {
    tokens.first().and_then(|token| match &token.kind {
        TokenKind::Word(word) => Some(word.to_ascii_lowercase()),
        _ => None,
    })
}

fn writes_rows(tokens: &[Token<'_>]) -> bool {
    match first_word(tokens).as_deref() {
        Some(word) if WRITING_WORDS.contains(&word) => true,
        Some("select" | "with") => tokens.iter().any(|token| is_word(Some(token), "into")),
        _ => false,
    }
}

fn runs_dynamic_text(tokens: &[Token<'_>]) -> bool {
    first_word(tokens).is_some_and(|word| DYNAMIC_WORDS.contains(&word.as_str()))
        || tokens.iter().any(|token| is_word(Some(token), "sp_executesql"))
}

fn copies_masked_values() -> SqlError {
    SqlError::new(
        SqlState::INSUFFICIENT_PRIVILEGE,
        "permission denied: this role holds no UNMASK, so it may not copy masked columns into rows or run dynamic SQL while they are masked",
    )
    .with_hint("Read masked columns with SELECT, or ask for GRANT UNMASK.")
    .with_sqlserver_code(15247)
}

fn identifiers_of(text: &str, dialect: crate::dialect::Dialect) -> Vec<String> {
    match tokenize_in(text, dialect) {
        Ok(tokens) => tokens.iter().filter_map(identifier).map(|word| word.to_ascii_lowercase()).collect(),
        Err(_) => Vec::new(),
    }
}

/// Adds to `named` the identifiers in the query text of the views and the bodies of the
/// routines `named` holds, and of the views and routines those reach, so a column a view
/// selects or a function returns counts as named; whether any view or routine was found.
fn through_views_and_routines(reader: &dyn KeyValueRead, named: &mut HashSet<String>) -> SqlResult<bool> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut found = false;
    let mut frontier: Vec<String> = named.iter().cloned().collect();
    for _ in 0..MOST_NESTED_OBJECTS {
        let mut next = Vec::new();
        for name in frontier {
            if !seen.insert(name.clone()) {
                continue;
            }
            let mut words = Vec::new();
            if let Some(bytes) = reader.get(&crate::encoding::table_key(&name))?
                && crate::encoding::is_view_record(&bytes)
            {
                let view = crate::encoding::ViewDefinition::decode(&name, &bytes)?;
                words.extend(identifiers_of(&view.query, view.dialect));
                words.extend(view.references.iter().map(|reference| reference.to_ascii_lowercase()));
                found = true;
            }
            if let Some(bytes) = reader.get(&crate::encoding::routine_key(&name))? {
                for routine in crate::encoding::decode_routines(&name, &bytes)? {
                    words.extend(identifiers_of(&routine.text, routine.dialect));
                }
                found = true;
            }
            for word in words {
                if named.insert(word.clone()) {
                    next.push(word);
                }
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    Ok(found)
}

impl Session<'_> {
    fn with_block_writer(&mut self, write: impl FnOnce(&mut dyn KeyValueWrite) -> SqlResult<()>) -> SqlResult<()> {
        if let Block::Writing(writer) = &mut self.block {
            return write(writer);
        }
        let mut writer = self.store.begin_boxed_write()?;
        write(&mut writer)?;
        writer.commit_boxed()
    }

    fn owns_or_superuser(&mut self, table: &str) -> SqlResult<bool> {
        let Some(role) = self.settings.checked_role.clone() else {
            return Ok(true);
        };
        self.with_block_reader(|reader| {
            let owner = crate::roles::owner_of(reader, table)?;
            Ok(owner.is_some_and(|owner| crate::roles::role_closure(reader, &role).is_ok_and(|held| held.contains(&owner))))
        })
    }

    /// Runs `text` when it is one of the masking commands: `ALTER TABLE t ALTER COLUMN c ADD
    /// MASKED WITH (FUNCTION = '...')`, `... DROP MASKED`, `GRANT UNMASK TO r` or `REVOKE UNMASK
    /// FROM r` (D1362); whether it was.
    pub(super) fn prepare_masking_commands(&mut self, text: &str, on_result: &mut dyn FnMut(QueryResult) -> bool) -> SqlResult<bool> {
        let lowered = text.to_ascii_lowercase();
        if !(lowered.contains("masked") || lowered.contains("unmask")) {
            return Ok(false);
        }
        let Ok(tokens) = tokenize_in(text, self.dialect) else {
            return Ok(false);
        };
        let Some(command) = command_of(&tokens)? else {
            return Ok(false);
        };
        let tag = match command {
            MaskCommand::Add { table, column, function } => {
                if !self.owns_or_superuser(&table)? {
                    return Err(not_allowed(" or the table's owner"));
                }
                let column = self.with_block_reader(|reader| column_of(reader, &table, &column))?;
                self.with_block_writer(|writer| masking::set_mask(writer, &table, &column, &function))?;
                "ALTER TABLE"
            }
            MaskCommand::Drop { table, column } => {
                if !self.owns_or_superuser(&table)? {
                    return Err(not_allowed(" or the table's owner"));
                }
                let column = self.with_block_reader(|reader| column_of(reader, &table, &column))?;
                self.with_block_writer(|writer| masking::drop_mask(writer, &table, &column).map(|_| ()))?;
                "ALTER TABLE"
            }
            MaskCommand::Grant { role } => {
                if self.settings.checked_role.is_some() {
                    return Err(not_allowed(""));
                }
                self.with_block_writer(|writer| masking::grant_unmask(writer, &role))?;
                "GRANT"
            }
            MaskCommand::Revoke { role } => {
                if self.settings.checked_role.is_some() {
                    return Err(not_allowed(""));
                }
                self.with_block_writer(|writer| masking::revoke_unmask(writer, &role).map(|_| ()))?;
                "REVOKE"
            }
        };
        on_result(QueryResult::command(tag));
        Ok(true)
    }

    /// The masks a `CREATE TABLE` declares on its columns with `MASKED WITH (FUNCTION =
    /// '...')`, and its text without them.
    pub(super) fn masked_columns_declared(&self, text: &str) -> SqlResult<Option<MaskedCreate>> {
        if !text.to_ascii_lowercase().contains("masked") {
            return Ok(None);
        }
        let Ok(tokens) = tokenize_in(text, self.dialect) else {
            return Ok(None);
        };
        if !(is_word(tokens.first(), "create") && is_word(tokens.get(1), "table")) {
            return Ok(None);
        }
        let mut at = 2;
        if is_word(tokens.get(at), "if") && is_word(tokens.get(at + 1), "not") && is_word(tokens.get(at + 2), "exists") {
            at += 3;
        }
        let Some((table, after_name)) = table_name(&tokens, at) else {
            return Ok(None);
        };
        if !matches!(tokens.get(after_name).map(|token| &token.kind), Some(TokenKind::LeftParen)) {
            return Ok(None);
        }
        let mut masks = Vec::new();
        let mut removed: Vec<(usize, usize)> = Vec::new();
        let mut depth = 0usize;
        let mut column: Option<String> = None;
        let mut expecting_name = false;
        let mut index = after_name;
        while let Some(token) = tokens.get(index) {
            match token.kind {
                TokenKind::LeftParen => {
                    depth += 1;
                    if depth == 1 {
                        expecting_name = true;
                    }
                }
                TokenKind::RightParen => depth = depth.saturating_sub(1),
                TokenKind::Comma if depth == 1 => expecting_name = true,
                _ if depth == 1 && expecting_name => {
                    column = identifier(token);
                    expecting_name = false;
                }
                _ => {}
            }
            if depth == 1
                && let Some((function, end)) = masked_with(&tokens, index)
            {
                let Some(name) = column.clone() else {
                    return Ok(None);
                };
                masks.push((name, MaskFunction::parse(&function.0)?));
                removed.push((token.start, tokens[end - 1].end));
                index = end;
                continue;
            }
            index += 1;
        }
        if masks.is_empty() {
            return Ok(None);
        }
        let mut stripped = String::with_capacity(text.len());
        let mut kept_from = 0;
        for (start, end) in removed {
            stripped.push_str(&text[kept_from..start]);
            kept_from = end;
        }
        stripped.push_str(&text[kept_from..]);
        Ok(Some(MaskedCreate { table, masks, text: stripped }))
    }

    /// Stores the masks a `CREATE TABLE` that ran declared.
    pub(super) fn record_masks(&mut self, create: &MaskedCreate) -> SqlResult<()> {
        let mut resolved = Vec::new();
        for (column, function) in &create.masks {
            let column = self.with_block_reader(|reader| column_of(reader, &create.table, column))?;
            resolved.push((column, function.clone()));
        }
        self.with_block_writer(|writer| {
            for (column, function) in &resolved {
                masking::set_mask(writer, &create.table, column, function)?;
            }
            Ok(())
        })
    }

    /// The masks the results of `text` get: `None` for a superuser, a role holding `UNMASK`,
    /// or a statement that names no masked column (D1348).
    pub(super) fn result_masks(&mut self, text: &str) -> SqlResult<Option<ResultMasks>> {
        let Some(role) = self.settings.checked_role.clone() else {
            return Ok(None);
        };
        let Ok(tokens) = tokenize_in(text, self.dialect) else {
            return Ok(None);
        };
        let mut named: HashSet<String> = tokens.iter().filter_map(identifier).map(|name| name.to_ascii_lowercase()).collect();
        let mut plain = names_its_columns_plainly(&tokens);
        let writes = writes_rows(&tokens);
        let dynamic = runs_dynamic_text(&tokens);
        self.with_block_reader(|reader| {
            let masks = masking::every_mask(reader)?;
            if masks.is_empty() || masking::holds_unmask(reader, &role)? {
                return Ok(None);
            }
            if through_views_and_routines(reader, &mut named)? {
                plain = false;
            }
            let held = crate::roles::role_closure(reader, &role)?;
            let mut exempt = HashSet::new();
            for mask in &masks {
                if !exempt.contains(&mask.table)
                    && crate::roles::owner_of(reader, &mask.table)?.is_some_and(|owner| held.contains(&owner))
                {
                    exempt.insert(mask.table.clone());
                }
            }
            if dynamic && masks.iter().any(|mask| !exempt.contains(&mask.table)) {
                return Err(copies_masked_values());
            }
            let result = ResultMasks::for_statement(reader, &masks, &exempt, &named, plain)?;
            if result.is_some() && writes {
                return Err(copies_masked_values());
            }
            Ok(result)
        })
    }
}
