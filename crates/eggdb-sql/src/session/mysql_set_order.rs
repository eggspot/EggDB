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

fn year_column(constraint: &ConstraintSpec) -> Option<String> {
    let ConstraintKind::Check { text, .. } = &constraint.kind else {
        return None;
    };
    let column = constraint.column.as_ref()?;
    let quoted = format!("`{}`", column.text.replace('`', "``"));
    (*text == format!("{quoted} between 1901 and 2155 or {quoted} = 0")).then(|| column.text.clone())
}

impl Session<'_> {
    /// Stores the values of `table`'s MySQL `SET` columns as MySQL does, in member order with
    /// each member once and numbers read as bit masks, and its `YEAR` columns' two-digit years
    /// as the years they stand for, through a trigger that runs before the columns' checks
    /// (D970, D971).
    pub(super) fn keep_set_member_order<'c>(
        &mut self,
        table: &str,
        constraints: impl IntoIterator<Item = &'c ConstraintSpec>,
    ) -> SqlResult<()> {
        let constraints: Vec<&ConstraintSpec> = constraints.into_iter().collect();
        let columns: Vec<(String, String)> = constraints.iter().copied().filter_map(set_column).collect();
        let years: Vec<String> = constraints.iter().copied().filter_map(year_column).collect();
        if columns.is_empty() && years.is_empty() {
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
            .chain(years.iter().map(|column| {
                format!(
                    "NEW.{column} := CASE WHEN NEW.{column} BETWEEN 1 AND 69 THEN NEW.{column} + 2000 \
                     WHEN NEW.{column} BETWEEN 70 AND 99 THEN NEW.{column} + 1900 ELSE NEW.{column} END; ",
                    column = identifier(column)
                )
            }))
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

    #[test]
    fn two_digit_years_become_the_years_mysql_reads_them_as() {
        let store = MemoryStore::default();
        let mut session = Session::with_dialect(&store, Dialect::MySql);
        text(&mut session, "CREATE TABLE vintages (id INT PRIMARY KEY, made YEAR)");
        text(&mut session, "INSERT INTO vintages VALUES (1, 5), (2, 87), (3, 1999), (4, 0)");
        let stored = text(&mut session, "SELECT made FROM vintages ORDER BY id");
        assert_eq!(stored, "made\n2005\n1987\n1999\n0\nSELECT 4", "{stored}");
    }
}
