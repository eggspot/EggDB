use super::Parser;
use crate::ast::{CreateTableAs, Name, Statement};
use crate::error::{SqlError, SqlResult};

pub(super) struct SelectIntoTable {
    name: Name,
    temporary: bool,
}

fn not_first_query(position: usize) -> SqlError {
    SqlError::syntax(
        "SELECT INTO must be the first query in a statement containing a UNION, INTERSECT or EXCEPT operator.",
        position,
    )
    .with_sqlserver_code(196)
}

fn into_not_allowed(position: usize) -> SqlError {
    SqlError::syntax("SELECT ... INTO is not allowed here", position)
}

impl Parser<'_> {
    pub(super) fn query_or_select_into(&mut self) -> SqlResult<Statement> {
        let outer = self.select_into.replace(None);
        let parsed = self.query_statement();
        let into = std::mem::replace(&mut self.select_into, outer).flatten();
        let query = parsed?;
        let Some(table) = into else {
            return Ok(query);
        };
        if !matches!(query, Statement::Select(_) | Statement::Query(_)) {
            return Err(into_not_allowed(table.name.position));
        }
        Ok(Statement::CreateTableAs(Box::new(CreateTableAs {
            name: table.name,
            temporary: table.temporary,
            query,
        })))
    }

    pub(super) fn select_into_table(&mut self) -> SqlResult<()> {
        let position = self.position();
        let dialect_allows = self.sqlserver() || self.postgresql();
        if !dialect_allows
            || self.query_depth > 0
            || self.function_body
            || self.routine_frame.is_some()
            || self.select_into.is_none()
        {
            return Err(self.not_supported("SELECT INTO"));
        }
        if matches!(self.select_into, Some(Some(_))) {
            return Err(not_first_query(position));
        }
        self.at += 1;
        let mut temporary = false;
        if self.postgresql() {
            temporary = self.take_word("temporary") || self.take_word("temp");
            if !temporary {
                self.take_word("unlogged");
            }
            self.take_word("table");
        }
        let name = self.table_name()?;
        if self.sqlserver() && crate::temporary::is_sqlserver_temporary(&name.text) {
            temporary = true;
        }
        self.select_into = Some(Some(SelectIntoTable { name, temporary }));
        Ok(())
    }
}
