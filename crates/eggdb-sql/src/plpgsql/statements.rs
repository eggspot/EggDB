use crate::ast::Statement;
use crate::dialect::Dialect;
use crate::error::{SqlError, SqlResult};
use crate::execute::{self, TableCache};
use crate::name_resolution::Binding;
use crate::session::QueryResult;
use crate::settings::Settings;
use crate::store::KeyValueWrite;

const DIALECT: Dialect = Dialect::PostgreSql;

pub(crate) fn processed_rows(result: &QueryResult) -> u64 {
    if result.columns.is_some() {
        return result.rows.len() as u64;
    }
    result
        .tag
        .rsplit(' ')
        .next()
        .and_then(|count| count.parse().ok())
        .unwrap_or(0)
}

fn not_in_routine(what: &str) -> SqlError {
    SqlError::not_supported(format!("{what} inside a PL/pgSQL or SQL function"))
}

pub(crate) fn run_sql(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    settings: &Settings,
    statement: Statement,
) -> SqlResult<QueryResult> {
    crate::cancel::check()?;
    let binding = Binding {
        settings,
        walk_expressions: true,
    };
    match statement {
        Statement::Select(mut select) => {
            binding.select(&*writer, tables, &mut select)?;
            execute::select(&*writer, tables, &select, false)
        }
        Statement::Query(mut query) => {
            binding.query(&*writer, tables, &mut query)?;
            execute::query(&*writer, tables, &query, false)
        }
        Statement::Insert(mut insert) => {
            binding.insert(&*writer, tables, &mut insert)?;
            execute::insert(writer, tables, &insert)
        }
        Statement::Update(mut update) => {
            binding.update(&*writer, tables, &mut update)?;
            execute::update(writer, tables, &update)
        }
        Statement::Delete(mut delete) => {
            binding.delete(&*writer, tables, &mut delete)?;
            execute::delete(writer, tables, &delete)
        }
        Statement::Merge(mut merge) => {
            binding.merge(&*writer, tables, &mut merge)?;
            crate::merge::merge(writer, tables, &merge)
        }
        Statement::With(with) => crate::with_statement::run(writer, tables, &with, &binding),
        Statement::Truncate(mut table) => {
            binding.target(&*writer, tables, &mut table)?;
            execute::truncate(writer, tables, &table)
        }
        Statement::CreateTable(mut create) => {
            binding.create_table(&*writer, tables, &mut create)?;
            execute::create_table(writer, tables, &create, DIALECT)
        }
        Statement::DropTable {
            mut names,
            if_exists,
            cascade,
        } => {
            binding.names(&*writer, tables, &mut names)?;
            execute::drop_tables(writer, tables, &names, if_exists, cascade, DIALECT)
        }
        Statement::CreateIndex(mut create) => {
            binding.names(&*writer, tables, std::slice::from_mut(&mut create.table))?;
            crate::indexes::create_index(writer, tables, &create, DIALECT)
        }
        Statement::DropIndex(mut drop) => {
            if let Some(table) = &mut drop.table {
                binding.names(&*writer, tables, std::slice::from_mut(table))?;
            }
            crate::indexes::drop_index(writer, tables, &drop, DIALECT)
        }
        Statement::AlterTable(mut alter) => {
            binding.alter_table(&*writer, tables, &mut alter)?;
            crate::alter::alter_table(writer, tables, &alter, DIALECT)
        }
        Statement::CreateSequence(mut create) => {
            create.name.text = crate::schemas::creation_name(&*writer, &create.name)?;
            crate::sequences::create_sequence(writer, tables, &create, DIALECT)
        }
        Statement::DropSequence {
            mut names,
            if_exists,
            cascade,
        } => {
            binding.names(&*writer, tables, &mut names)?;
            crate::sequences::drop_sequences(writer, tables, &names, if_exists, cascade, DIALECT)
        }
        Statement::CreateTrigger(mut create) => {
            binding.names(&*writer, tables, std::slice::from_mut(&mut create.table))?;
            crate::triggers::create_trigger(writer, tables, &create)
        }
        Statement::DropTrigger(mut drop) => {
            if let Some(table) = &mut drop.table {
                binding.names(&*writer, tables, std::slice::from_mut(table))?;
            }
            crate::triggers::drop_triggers(writer, tables, &drop, DIALECT)
        }
        Statement::CreateRoutine(create) => {
            let check = crate::routines::BodyCheck::of_setting(
                settings.get("check_function_bodies").ok().as_deref(),
            );
            crate::routines::create_routine(writer, tables, &create, check)
        }
        Statement::DropRoutine(drop) => crate::routines::drop_routines(writer, tables, &drop, DIALECT),
        Statement::Ignored { tag, notice } => {
            let result = QueryResult::command(tag);
            Ok(match notice {
                Some(notice) => result.with_notice(notice),
                None => result,
            })
        }
        Statement::Begin { .. } | Statement::Commit | Statement::Rollback => {
            Err(not_in_routine("a transaction command"))
        }
        Statement::Set { .. } | Statement::Reset { .. } => Err(not_in_routine("SET")),
        _ => Err(not_in_routine("this statement")),
    }
}
