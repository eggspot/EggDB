use super::Session;
use crate::ast::{ConstraintKind, ConstraintSpec, ExprKind, Literal};
use crate::dialect::Dialect;
use crate::error::SqlResult;
use crate::mysql_set::{SET_MEMBERS_FUNCTION, SET_NORMALIZE_FUNCTION};
use crate::schemas::{DEFAULT_SCHEMA, base_name, schema_of};

fn identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

fn set_column(constraint: &ConstraintSpec) -> Option<(String, String)> {
    let ConstraintKind::Check { expr, .. } = &constraint.kind else {
        return None;
    };
    let ExprKind::Function { name, arguments } = &expr.kind else {
        return None;
    };
    if name != SET_MEMBERS_FUNCTION {
        return None;
    }
    match arguments.as_slice() {
        [column, members] => match (&column.kind, &members.kind) {
            (ExprKind::Column { name, .. }, ExprKind::Literal(Literal::String(members))) => {
                Some((name.clone(), members.clone()))
            }
            _ => None,
        },
        _ => None,
    }
}

impl Session<'_> {
    /// Stores the values of `table`'s MySQL `SET` columns as MySQL does, in member order with
    /// each member once and numbers read as bit masks, through a trigger that runs before the
    /// columns' checks (D970).
    pub(super) fn keep_set_member_order<'c>(
        &mut self,
        table: &str,
        constraints: impl IntoIterator<Item = &'c ConstraintSpec>,
    ) -> SqlResult<()> {
        let columns: Vec<(String, String)> = constraints.into_iter().filter_map(set_column).collect();
        if columns.is_empty() {
            return Ok(());
        }
        let schema = schema_of(table);
        let schema = if schema.is_empty() { DEFAULT_SCHEMA } else { schema };
        let name = base_name(table);
        let assignments: String = columns
            .iter()
            .map(|(column, members)| {
                format!(
                    "NEW.{column} := {SET_NORMALIZE_FUNCTION}(NEW.{column}, {members}); ",
                    column = identifier(column),
                    members = literal(members)
                )
            })
            .collect();
        let function = identifier(&format!("set_order_{schema}_{name}"));
        let text = format!(
            "CREATE SCHEMA IF NOT EXISTS eggdb_internal; \
             CREATE FUNCTION eggdb_internal.{function}() RETURNS trigger LANGUAGE plpgsql AS $set_order$ \
             BEGIN {assignments}RETURN NEW; END \
             $set_order$; \
             CREATE TRIGGER eggdb_set_order BEFORE INSERT OR UPDATE ON {}.{} FOR EACH ROW \
             EXECUTE FUNCTION eggdb_internal.{function}()",
            identifier(schema),
            identifier(name),
        );
        let dialect = self.dialect;
        self.dialect = Dialect::PostgreSql;
        let outcome = self.run_quietly(&text);
        self.dialect = dialect;
        outcome.map(drop)
    }
}

#[cfg(test)]
mod tests {
    use crate::memory_store::MemoryStore;
    use crate::{Dialect, Session};

    fn text(session: &mut Session<'_>, sql: &str) -> String {
        let mut found = String::new();
        session
            .execute(sql, &mut |result| found = crate::tests::render(&result))
            .unwrap_or_else(|error| panic!("{sql}: {error}"));
        found
    }

    #[test]
    fn set_values_are_written_in_member_order_and_numbers_read_as_masks() {
        let store = MemoryStore::default();
        let mut session = Session::with_dialect(&store, Dialect::MySql);
        text(&mut session, "CREATE TABLE shirts (id INT PRIMARY KEY, sizes SET('s','m','l'))");
        text(&mut session, "INSERT INTO shirts VALUES (1, 'l,s,S'), (2, 6)");
        text(&mut session, "UPDATE shirts SET sizes = 'm,s,m' WHERE id = 2");
        let stored = text(&mut session, "SELECT sizes FROM shirts ORDER BY id");
        assert!(stored.contains("s,l"), "{stored}");
        assert!(stored.contains("s,m"), "{stored}");
        assert!(session.execute("INSERT INTO shirts VALUES (3, 'xl')", &mut |_| {}).is_err());
    }
}
