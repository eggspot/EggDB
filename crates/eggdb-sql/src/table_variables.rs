use crate::error::{SqlError, SqlResult, SqlState};
use crate::memory::WorkMemory;
use crate::session::OutputColumn;
use crate::set_returning::GeneratedRows;
use crate::types::Value;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct TableVariable {
    pub(crate) columns: Vec<OutputColumn>,
    pub(crate) rows: Vec<Vec<Value>>,
    pub(crate) rules: TableVariableRules,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct TableVariableRules {
    pub(crate) required: Vec<usize>,
    pub(crate) keys: Vec<UniqueKey>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UniqueKey {
    pub(crate) columns: Vec<usize>,
    pub(crate) primary: bool,
}

impl TableVariable {
    pub(crate) fn declared(columns: Vec<OutputColumn>, rules: TableVariableRules) -> Self {
        Self {
            columns,
            rows: Vec::new(),
            rules,
        }
    }

    pub(crate) fn change_rows(
        &mut self,
        name: &str,
        change: impl FnOnce(&mut Vec<Vec<Value>>),
    ) -> SqlResult<()> {
        if self.rules == TableVariableRules::default() {
            change(&mut self.rows);
            return Ok(());
        }
        let before = self.rows.clone();
        change(&mut self.rows);
        let checked = self.check_rules(name);
        if checked.is_err() {
            self.rows = before;
        }
        checked
    }

    fn check_rules(&self, name: &str) -> SqlResult<()> {
        for row in &self.rows {
            for &column in &self.rules.required {
                if row.get(column).is_none_or(Value::is_null) {
                    return Err(self.null_violation(name, column, row));
                }
            }
        }
        for key in &self.rules.keys {
            let mut seen = HashSet::with_capacity(self.rows.len());
            for row in &self.rows {
                let values: Vec<Option<String>> = key
                    .columns
                    .iter()
                    .map(|&column| row.get(column).and_then(Value::to_text))
                    .collect();
                if !seen.insert(values) {
                    return Err(self.duplicate_key(name, key, row));
                }
            }
        }
        Ok(())
    }

    fn column_name(&self, column: usize) -> &str {
        self.columns
            .get(column)
            .map_or("", |column| column.name.as_str())
    }

    fn null_violation(&self, name: &str, column: usize, row: &[Value]) -> SqlError {
        SqlError::new(
            SqlState::NOT_NULL_VIOLATION,
            format!(
                "null value in column \"{}\" of relation \"{name}\" violates not-null constraint",
                self.column_name(column)
            ),
        )
        .with_detail(format!("Failing row contains ({}).", shown_values(row)))
    }

    fn duplicate_key(&self, name: &str, key: &UniqueKey, row: &[Value]) -> SqlError {
        let names: Vec<&str> = key
            .columns
            .iter()
            .map(|&column| self.column_name(column))
            .collect();
        let values: Vec<Value> = key
            .columns
            .iter()
            .filter_map(|&column| row.get(column).cloned())
            .collect();
        let suffix = if key.primary { "pkey" } else { "key" };
        let constraint = format!("{name}_{}_{suffix}", names.join("_"));
        SqlError::new(
            SqlState::UNIQUE_VIOLATION,
            format!("duplicate key value violates unique constraint \"{constraint}\""),
        )
        .with_detail(format!(
            "Key ({})=({}) already exists.",
            names.join(", "),
            shown_values(&values)
        ))
        .with_table(name)
    }
}

fn shown_values(row: &[Value]) -> String {
    let shown: Vec<String> = row
        .iter()
        .map(|value| value.to_text().unwrap_or_else(|| "null".into()))
        .collect();
    shown.join(", ")
}

pub(crate) type TableVariables = Arc<HashMap<String, TableVariable>>;

thread_local! {
    static VARIABLES: RefCell<Vec<TableVariables>> = const { RefCell::new(Vec::new()) };
}

pub(crate) struct EnteredVariables {
    depth: usize,
}

impl Drop for EnteredVariables {
    fn drop(&mut self) {
        VARIABLES.with(|frames| {
            if let Ok(mut frames) = frames.try_borrow_mut() {
                frames.truncate(self.depth);
            }
        });
    }
}

pub(crate) fn enter(variables: &TableVariables) -> EnteredVariables {
    let depth = VARIABLES.with(|frames| match frames.try_borrow_mut() {
        Ok(mut frames) => {
            let depth = frames.len();
            frames.push(Arc::clone(variables));
            depth
        }
        Err(_) => usize::MAX,
    });
    EnteredVariables { depth }
}

pub(crate) fn current() -> TableVariables {
    VARIABLES.with(|frames| {
        frames
            .try_borrow()
            .ok()
            .and_then(|frames| frames.last().cloned())
            .unwrap_or_default()
    })
}

pub(crate) fn release_entered() {
    VARIABLES.with(|frames| {
        if let Ok(mut frames) = frames.try_borrow_mut()
            && let Some(top) = frames.last_mut()
        {
            *top = TableVariables::default();
        }
    });
}

pub(crate) fn undeclared(name: &str) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_TABLE,
        format!("Must declare the table variable \"{name}\"."),
    )
    .with_sqlserver_code(1087)
}

pub(crate) fn generated_rows(name: &str, columns_only: bool) -> SqlResult<GeneratedRows> {
    let found = VARIABLES.with(|frames| {
        frames.try_borrow().ok().and_then(|frames| {
            frames
                .last()
                .and_then(|variables| variables.get(name).cloned())
        })
    });
    let Some(variable) = found else {
        return Err(undeclared(name));
    };
    if columns_only {
        return Ok(GeneratedRows::empty(variable.columns));
    }
    let mut charge = WorkMemory::new();
    for row in &variable.rows {
        charge.charge_row(row)?;
    }
    Ok(GeneratedRows {
        columns: variable.columns,
        rows: variable.rows,
        charge,
    })
}
