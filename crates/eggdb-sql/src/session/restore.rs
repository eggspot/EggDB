use super::{Block, Session};
use crate::ast::Name;
use crate::constraints::ConstraintClause;
use crate::encoding::{TableDefinition, is_view_record, table_key};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::store::KeyValueStore;
use std::sync::Arc;

impl<'s, S: KeyValueStore + 's> Session<'s, S> {
    pub(crate) fn ensure_schema(&mut self, name: &str) -> SqlResult<bool> {
        if name == crate::schemas::DEFAULT_SCHEMA {
            return Ok(false);
        }
        let exists = self.read(|reader, _| crate::schemas::schema_exists(reader, name))?;
        if exists {
            return Ok(false);
        }
        let dialect = self.dialect;
        let schema = Name {
            text: name.to_owned(),
            position: 0,
        };
        self.write("CREATE SCHEMA", |writer, _| {
            crate::schemas::create_schema(writer, &schema, true, dialect)
        })?;
        if let Block::Writing(_) = self.block {
            self.commit()?;
        }
        Ok(true)
    }

    pub(crate) fn oracle_constraint_clause(
        &mut self,
        table: &str,
        constraint: &str,
    ) -> SqlResult<Option<ConstraintClause>> {
        let settings = Arc::clone(&self.settings);
        let name = crate::parser::oracle_table_name(table, &settings)?;
        let _path = self.enter_search_path(&settings);
        let definition = self.read(|reader, _| {
            let resolved = crate::schemas::resolve_relation(reader, &name.text)?;
            match reader.get(&table_key(&resolved))? {
                Some(bytes) if !is_view_record(&bytes) => {
                    TableDefinition::decode(&resolved, &bytes).map(Some)
                }
                _ => Ok(None),
            }
        })?;
        let Some(definition) = definition else {
            return Err(
                SqlError::new(SqlState::UNDEFINED_TABLE, "table or view does not exist")
                    .with_oracle_code(942),
            );
        };
        Ok(crate::constraints::constraint_clause(
            &definition,
            constraint,
        ))
    }
}
