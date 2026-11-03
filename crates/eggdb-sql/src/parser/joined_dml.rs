use super::Parser;
use crate::ast::{Delete, FromClause, JoinedTables, Name, Statement, TableReference};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::lexer::TokenKind;
use crate::schemas::base_name;

pub(super) struct WrittenColumn {
    pub(super) table: Option<Name>,
    pub(super) column: Name,
}

fn reference_at(tables: &FromClause, at: usize) -> Option<&TableReference> {
    match at.checked_sub(1) {
        None => Some(&tables.first),
        Some(join) => tables.joins.get(join).map(|join| &join.table),
    }
}

fn same_table(written: &str, label: &str) -> bool {
    written == label || base_name(written) == label || written == base_name(label)
}

pub(super) fn label_of(reference: &TableReference) -> &str {
    reference
        .alias
        .as_deref()
        .unwrap_or_else(|| base_name(&reference.name.text))
}

fn position_of(tables: &FromClause, label: &str, by_table_name: bool) -> Option<usize> {
    let listed: Vec<&TableReference> = std::iter::once(&tables.first)
        .chain(tables.joins.iter().map(|join| &join.table))
        .collect();
    if let Some(at) = listed
        .iter()
        .position(|reference| reference.alias.as_deref() == Some(label))
    {
        return Some(at);
    }
    let named = |unaliased_only: bool| -> Vec<usize> {
        listed
            .iter()
            .enumerate()
            .filter(|(_, reference)| !unaliased_only || reference.alias.is_none())
            .filter(|(_, reference)| same_table(&reference.name.text, label))
            .map(|(at, _)| at)
            .collect()
    };
    let unaliased = named(true);
    match unaliased.as_slice() {
        [at] => return Some(*at),
        [] if by_table_name => {}
        _ => return None,
    }
    match named(false).as_slice() {
        [at] => Some(*at),
        _ => None,
    }
}

fn unbound_column(table: &Name, column: &Name, sqlserver: bool) -> SqlError {
    if sqlserver {
        return SqlError::new(
            SqlState::UNDEFINED_COLUMN,
            format!(
                "The multi-part identifier \"{}.{}\" could not be bound.",
                table.text, column.text
            ),
        )
        .with_sqlserver_code(4104)
        .at(table.position);
    }
    SqlError::new(
        SqlState::UNDEFINED_COLUMN,
        format!(
            "Unknown column '{}.{}' in 'field list'",
            table.text, column.text
        ),
    )
    .with_vendor_code(1054)
    .at(table.position)
}

fn unknown_deleted_table(label: &Name) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_TABLE,
        format!("Unknown table '{}' in MULTI DELETE", label.text),
    )
    .with_vendor_code(1109)
    .at(label.position)
}

fn check_written_tables(
    allowed: &[&str],
    written: &[WrittenColumn],
    sqlserver: bool,
) -> SqlResult<()> {
    for column in written {
        if let Some(table) = &column.table
            && !allowed.iter().any(|label| same_table(label, &table.text))
        {
            return Err(unbound_column(table, &column.column, sqlserver));
        }
    }
    Ok(())
}

pub(super) fn joined_label<'t>(
    table: &'t TableReference,
    joined: Option<&JoinedTables>,
) -> Option<&'t str> {
    joined.map(|_| label_of(table))
}

impl Parser<'_> {
    pub(super) fn written_column(&mut self) -> SqlResult<WrittenColumn> {
        let first = self.name()?;
        if (self.mysql() || self.sqlserver()) && self.peek_kind() == &TokenKind::Dot {
            self.at += 1;
            let column = self.name()?;
            return Ok(WrittenColumn {
                table: Some(first),
                column,
            });
        }
        Ok(WrittenColumn {
            table: None,
            column: first,
        })
    }

    fn at_listed_join(&self) -> bool {
        self.peek_kind() == &TokenKind::Comma
            || ["join", "inner", "left", "right", "cross"]
                .iter()
                .any(|word| self.is_word(word))
    }

    pub(super) fn update_target(&mut self) -> SqlResult<(TableReference, Option<FromClause>)> {
        let start = self.at;
        let table = self.table_reference()?;
        if !self.mysql() || !self.at_listed_join() {
            return Ok((table, None));
        }
        self.at = start;
        let tables = self.joined_tables()?;
        Ok((tables.first.clone(), Some(tables)))
    }

    pub(super) fn listed_update(
        &self,
        table: &mut TableReference,
        tables: FromClause,
        written: &[WrittenColumn],
    ) -> SqlResult<JoinedTables> {
        let mut assigned_at = Vec::with_capacity(written.len());
        for column in written {
            assigned_at.push(match &column.table {
                Some(name) => Some(
                    position_of(&tables, &name.text, false)
                        .ok_or_else(|| unbound_column(name, &column.column, false))?,
                ),
                None => None,
            });
        }
        let target_at = assigned_at.iter().flatten().next().copied().unwrap_or(0);
        if let Some(reference) = reference_at(&tables, target_at) {
            *table = reference.clone();
        }
        Ok(JoinedTables {
            tables,
            target_at: Some(target_at),
            more_targets_at: Vec::new(),
            assigned_at,
            writes_any_table: true,
        })
    }

    pub(super) fn joined_target(
        &self,
        table: &mut TableReference,
        tables: FromClause,
        written: &[WrittenColumn],
    ) -> SqlResult<JoinedTables> {
        let written_name = table.name.text.clone();
        let target_at = if self.sqlserver() && table.alias.is_none() {
            position_of(&tables, &written_name, true)
        } else {
            None
        };
        if let Some(reference) = target_at.and_then(|at| reference_at(&tables, at)) {
            *table = reference.clone();
        }
        check_written_tables(
            &[label_of(table), written_name.as_str()],
            written,
            self.sqlserver(),
        )?;
        Ok(JoinedTables {
            target_at,
            ..JoinedTables::beside_target(tables)
        })
    }

    pub(super) fn check_single_target(
        &self,
        table: &TableReference,
        written: &[WrittenColumn],
    ) -> SqlResult<()> {
        check_written_tables(
            &[label_of(table), table.name.text.as_str()],
            written,
            self.sqlserver(),
        )
    }

    fn deleted_tables(&mut self) -> SqlResult<Vec<Name>> {
        let mut labels = Vec::new();
        loop {
            let mut label = self.name()?;
            while self.peek_kind() == &TokenKind::Dot {
                self.at += 1;
                if self.take_operator("*") {
                    break;
                }
                label = self.name()?;
            }
            labels.push(label);
            if !self.take(&TokenKind::Comma) {
                return Ok(labels);
            }
        }
    }

    pub(super) fn mysql_multiple_table_delete(&mut self) -> SqlResult<Option<Statement>> {
        let start = self.at;
        let after_from = self.take_word("from");
        let Ok(labels) = self.deleted_tables() else {
            self.at = start;
            return Ok(None);
        };
        if !self.is_word(if after_from { "using" } else { "from" }) {
            self.at = start;
            return Ok(None);
        }
        self.at += 1;
        let tables = self.joined_tables()?;
        let mut positions: Vec<usize> = Vec::with_capacity(labels.len());
        for label in &labels {
            let at = position_of(&tables, &label.text, false)
                .ok_or_else(|| unknown_deleted_table(label))?;
            if !positions.contains(&at) {
                positions.push(at);
            }
        }
        let target_at = positions.first().copied().unwrap_or(0);
        let Some(table) = reference_at(&tables, target_at).cloned() else {
            return Err(self.unexpected());
        };
        let filter = self.filter()?;
        Ok(Some(Statement::Delete(Delete {
            table,
            filter,
            returning: None,
            using: Some(JoinedTables {
                tables,
                target_at: Some(target_at),
                more_targets_at: positions.into_iter().skip(1).collect(),
                assigned_at: Vec::new(),
                writes_any_table: true,
            }),
        })))
    }
}
