use crate::ast::Name;
use crate::error::SqlResult;
use crate::roles::permission_denied;

#[cfg(test)]
mod tests;

/// One entry of a `FROM` list (or a write's target) as column privilege checks see it: the
/// label queries name it by, its columns when they are known, and the columns the session may
/// not read.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ScopeItem {
    label: Option<String>,
    relation: String,
    columns: Option<Vec<String>>,
    unreadable: Vec<String>,
}

impl ScopeItem {
    /// A stored table or view with known columns, of which `unreadable` may not be read.
    pub(crate) fn relation(
        label: String,
        relation: String,
        columns: Vec<String>,
        unreadable: Vec<String>,
    ) -> Self {
        Self {
            label: Some(label),
            relation,
            columns: Some(columns),
            unreadable,
        }
    }

    /// A derived table, function, common table or catalog relation: readable, with columns
    /// these checks do not know.
    pub(crate) fn opaque(label: Option<String>) -> Self {
        Self {
            label,
            relation: String::new(),
            columns: None,
            unreadable: Vec::new(),
        }
    }

    fn answers_to(&self, qualifier: &str) -> bool {
        let Some(label) = self.label.as_deref() else {
            return false;
        };
        label == qualifier
            || label == crate::schemas::base_name(qualifier)
            || qualifier
                .rsplit('.')
                .next()
                .is_some_and(|last| last == label)
    }

    fn hides(&self, column: &str) -> bool {
        self.unreadable.iter().any(|hidden| hidden == column)
    }

    fn has_column(&self, column: &str) -> bool {
        self.columns
            .as_ref()
            .is_some_and(|columns| columns.iter().any(|known| known == column))
    }

    fn is_opaque(&self) -> bool {
        self.columns.is_none()
    }

    fn check_whole(&self) -> SqlResult<()> {
        if self.unreadable.is_empty() {
            Ok(())
        } else {
            Err(permission_denied(&self.relation))
        }
    }
}

/// The `FROM` lists in reach of the expression being resolved, innermost last, so a column
/// reference is checked against the entry it binds to, as PostgreSQL binds it.
#[derive(Debug, Default)]
pub(crate) struct ColumnScopes {
    levels: Vec<Vec<ScopeItem>>,
}

impl ColumnScopes {
    pub(crate) fn open(&mut self) {
        self.levels.push(Vec::new());
    }

    pub(crate) fn close(&mut self) {
        self.levels.pop();
    }

    pub(crate) fn add(&mut self, item: ScopeItem) {
        if let Some(level) = self.levels.last_mut() {
            level.push(item);
        }
    }

    /// Sets the scopes aside while a view's own query is resolved: a view's text cannot name
    /// the query around it.
    pub(crate) fn suspend(&mut self) -> Vec<Vec<ScopeItem>> {
        std::mem::take(&mut self.levels)
    }

    pub(crate) fn resume(&mut self, levels: Vec<Vec<ScopeItem>>) {
        self.levels = levels;
    }

    /// Whether any entry in reach hides a column, which is when expressions must be walked.
    pub(crate) fn has_unreadable(&self) -> bool {
        self.levels
            .iter()
            .flatten()
            .any(|item| !item.unreadable.is_empty())
    }

    /// Checks a column reference (`name`, `qualifier.name`, or a whole row as `qualifier.*`
    /// or a bare label) against the entry it binds to.
    pub(crate) fn check_column(&self, qualifier: Option<&str>, name: &str) -> SqlResult<()> {
        if name == "*" {
            return self.check_wildcard(qualifier, false);
        }
        match qualifier {
            Some(qualifier) => self.check_qualified(qualifier, name),
            None => self.check_unqualified(name),
        }
    }

    fn check_qualified(&self, qualifier: &str, name: &str) -> SqlResult<()> {
        for level in self.levels.iter().rev() {
            let mut matched = false;
            for item in level.iter().filter(|item| item.answers_to(qualifier)) {
                matched = true;
                if item.hides(name) {
                    return Err(permission_denied(&item.relation));
                }
            }
            if matched {
                return Ok(());
            }
        }
        Ok(())
    }

    fn check_unqualified(&self, name: &str) -> SqlResult<()> {
        for level in self.levels.iter().rev() {
            let mut bound = false;
            for item in level {
                if item.hides(name) {
                    return Err(permission_denied(&item.relation));
                }
                bound |= item.has_column(name);
            }
            if bound {
                return Ok(());
            }
            let mut whole_row = false;
            for item in level
                .iter()
                .filter(|item| item.label.as_deref() == Some(name))
            {
                whole_row = true;
                item.check_whole()?;
            }
            if whole_row && !level.iter().any(ScopeItem::is_opaque) {
                return Ok(());
            }
        }
        Ok(())
    }

    /// Checks `*` (every entry of the innermost list) or `qualifier.*`; `innermost_only`
    /// keeps a select list's `*` to its own `FROM`.
    pub(crate) fn check_wildcard(
        &self,
        qualifier: Option<&str>,
        innermost_only: bool,
    ) -> SqlResult<()> {
        match qualifier {
            None => {
                let levels: Vec<&Vec<ScopeItem>> = if innermost_only {
                    self.levels.last().into_iter().collect()
                } else {
                    self.levels.iter().collect()
                };
                for item in levels.into_iter().flatten() {
                    item.check_whole()?;
                }
                Ok(())
            }
            Some(qualifier) => {
                for level in self.levels.iter().rev() {
                    let mut matched = false;
                    for item in level.iter().filter(|item| item.answers_to(qualifier)) {
                        matched = true;
                        item.check_whole()?;
                    }
                    if matched {
                        return Ok(());
                    }
                }
                Ok(())
            }
        }
    }

    /// Checks the columns of `JOIN ... USING (columns)`, which read the column of every entry
    /// of the innermost list that has it.
    pub(crate) fn check_using(&self, columns: &[Name]) -> SqlResult<()> {
        let Some(level) = self.levels.last() else {
            return Ok(());
        };
        for column in columns {
            for item in level {
                if item.hides(&column.text) {
                    return Err(permission_denied(&item.relation));
                }
            }
        }
        Ok(())
    }
}

/// The columns of `columns` that `readable` does not allow, in table order.
pub(crate) fn unreadable_columns(
    columns: &[String],
    readable: &crate::roles::ColumnReach,
) -> Vec<String> {
    columns
        .iter()
        .filter(|column| !readable.allows(column))
        .cloned()
        .collect()
}
