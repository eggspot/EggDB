use super::{ParseFor, Session};
use crate::ast::{RoutineTarget, Statement};
use crate::destructive::{ClassifierCatalog, TableKeys};
use crate::routines::RoutineKind;

impl ClassifierCatalog for Session<'_> {
    fn table_keys(&mut self, table: &str) -> Option<TableKeys> {
        self.read(|reader, tables| crate::destructive::table_keys(reader, tables, table))
            .ok()
            .flatten()
    }

    fn is_user_routine(&mut self, name: &str) -> bool {
        self.read(|reader, tables| crate::routines::resolve(reader, tables, name))
            .map_or(true, |found| {
                found.is_some_and(|named| !named.routines.is_empty())
            })
    }

    fn triggers_calling(
        &mut self,
        kind: Option<RoutineKind>,
        target: &RoutineTarget,
    ) -> Option<Vec<String>> {
        let found = self
            .read(|reader, tables| crate::routines::triggers_calling(reader, tables, kind, target))
            .ok()?;
        Some(
            found
                .into_iter()
                .map(|(table, trigger)| {
                    format!(
                        "{}.{trigger}",
                        table.replace(crate::schemas::QUALIFIER, ".")
                    )
                })
                .collect(),
        )
    }

    fn materialized_view_query(&mut self, name: &str) -> Option<String> {
        Session::materialized_view_query(self, name).ok().flatten()
    }

    fn parsed(&mut self, text: &str) -> Option<Statement> {
        let tokens = crate::lexer::tokenize_in(text, self.dialect).ok()?;
        self.one_statement_in(text, &tokens, ParseFor::Classifying)
            .ok()?
            .ok()?
    }
}
