use crate::ast::{Comment, CommentTarget, Name, RelationKind};
use crate::dialect::Dialect;
use crate::encoding::{
    CommentKind, TableDefinition, comment_key, comment_kind_prefix, comments_prefix,
    decode_comment, encode_comment, prefix_end, schema_comment_key,
};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::execute::{Relation, TableCache};
use crate::schemas::{self, display};
use crate::session::QueryResult;
use crate::store::{KeyValueRead, KeyValueWrite, ScanDirection};

const MOST_COMMENT_BYTES: usize = 1 << 20;

fn missing_relation(name: &Name) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_TABLE,
        format!("relation \"{}\" does not exist", display(&name.text)),
    )
    .at(name.position)
}

fn wrong_kind(name: &Name, kind: &str) -> SqlError {
    SqlError::new(
        SqlState::WRONG_OBJECT_TYPE,
        format!("\"{}\" is not a {kind}", display(&name.text)),
    )
    .at(name.position)
}

fn store(writer: &mut dyn KeyValueWrite, key: &[u8], text: Option<&str>) -> SqlResult<QueryResult> {
    match text {
        Some(text) if !text.is_empty() => {
            if text.len() > MOST_COMMENT_BYTES {
                return Err(SqlError::new(
                    SqlState::PROGRAM_LIMIT_EXCEEDED,
                    format!("a comment can hold at most {MOST_COMMENT_BYTES} bytes"),
                ));
            }
            writer.put(key, &encode_comment(text))?;
        }
        _ => {
            writer.delete(key)?;
        }
    }
    Ok(QueryResult::command("COMMENT"))
}

pub(crate) fn comment_on(
    writer: &mut dyn KeyValueWrite,
    tables: &mut TableCache,
    comment: &Comment,
    dialect: Dialect,
) -> SqlResult<QueryResult> {
    let text = comment.text.as_deref();
    match &comment.target {
        CommentTarget::Extension(_) => Ok(QueryResult::command("COMMENT")),
        CommentTarget::Schema(name) => {
            if !schemas::schema_exists(writer, &name.text)? {
                return Err(schemas::missing_schema(&name.text, dialect).at(name.position));
            }
            store(writer, &schema_comment_key(&name.text), text)
        }
        CommentTarget::Relation { kind, name } => {
            let found = tables.relation(writer, &name.text)?;
            let fits = match (kind, &found) {
                (RelationKind::Table, Some(Relation::Table(_))) => true,
                (RelationKind::View, Some(Relation::View(_))) => true,
                (RelationKind::Sequence, None) => {
                    crate::sequences::read(writer, &name.text)?.is_some()
                }
                (_, None) => return Err(missing_relation(name)),
                _ => false,
            };
            if !fits {
                if *kind == RelationKind::Sequence {
                    return Err(missing_relation(name));
                }
                let shown = match kind {
                    RelationKind::Table => "table",
                    RelationKind::View => "view",
                    RelationKind::Sequence => "sequence",
                };
                return Err(wrong_kind(name, shown));
            }
            store(
                writer,
                &comment_key(&name.text, CommentKind::Relation, ""),
                text,
            )
        }
        CommentTarget::Column { table, column } => {
            let exists = match tables.relation(writer, &table.text)? {
                Some(Relation::Table(definition)) => {
                    definition.column_index(&column.text).is_some()
                }
                Some(Relation::View(view)) => {
                    view.columns.iter().any(|known| known.name == column.text)
                }
                None => return Err(missing_relation(table)),
            };
            if !exists {
                return Err(SqlError::new(
                    SqlState::UNDEFINED_COLUMN,
                    format!(
                        "column \"{}\" of relation \"{}\" does not exist",
                        column.text,
                        schemas::base_name(&table.text)
                    ),
                )
                .at(column.position));
            }
            store(
                writer,
                &comment_key(&table.text, CommentKind::Column, &column.text),
                text,
            )
        }
        CommentTarget::Constraint { table, name } => {
            let definition = tables.require(writer, table)?;
            if !definition.constraint_name_taken(&name.text) {
                return Err(SqlError::new(
                    SqlState::UNDEFINED_OBJECT,
                    format!(
                        "constraint \"{}\" for table \"{}\" does not exist",
                        name.text,
                        schemas::base_name(&table.text)
                    ),
                )
                .at(name.position));
            }
            store(
                writer,
                &comment_key(&table.text, CommentKind::Constraint, &name.text),
                text,
            )
        }
        CommentTarget::Index(name) => {
            let Some(table) = index_holder(writer, &name.text)? else {
                return Err(SqlError::new(
                    SqlState::UNDEFINED_TABLE,
                    format!("relation \"{}\" does not exist", display(&name.text)),
                )
                .at(name.position));
            };
            store(
                writer,
                &comment_key(
                    &table.name,
                    CommentKind::Index,
                    schemas::base_name(&name.text),
                ),
                text,
            )
        }
    }
}

pub(crate) fn index_holder(
    reader: &dyn KeyValueRead,
    name: &str,
) -> SqlResult<Option<TableDefinition>> {
    let schemas_to_search = match name.split_once(schemas::QUALIFIER) {
        Some((schema, _)) => vec![schema.to_owned()],
        None => schemas::relation_path(reader)?,
    };
    let index = schemas::base_name(name);
    let tables = crate::indexes::every_table(reader)?;
    for schema in schemas_to_search {
        if let Some(table) = tables.iter().find(|table| {
            schemas::schema_of(&table.name) == schema && table.index_named(index).is_some()
        }) {
            return Ok(Some(table.clone()));
        }
    }
    Ok(None)
}

fn read_comment(reader: &dyn KeyValueRead, key: &[u8]) -> SqlResult<Option<String>> {
    reader
        .get(key)?
        .map(|bytes| decode_comment(&bytes))
        .transpose()
}

pub(crate) fn relation_comment(reader: &dyn KeyValueRead, name: &str) -> SqlResult<Option<String>> {
    read_comment(reader, &comment_key(name, CommentKind::Relation, ""))
}

pub(crate) fn column_comments(
    reader: &dyn KeyValueRead,
    table: &str,
) -> SqlResult<Vec<(String, String)>> {
    let prefix = comment_kind_prefix(table, CommentKind::Column);
    let mut found = Vec::new();
    let mut failure = None;
    reader.scan(
        &prefix,
        &prefix_end(&prefix),
        ScanDirection::Forward,
        &mut |key, value| {
            let column = std::str::from_utf8(&key[prefix.len()..]);
            match (column, decode_comment(value)) {
                (Ok(column), Ok(text)) => found.push((column.to_owned(), text)),
                (_, Err(error)) => {
                    failure = Some(error);
                    return Ok(false);
                }
                (Err(_), _) => {
                    failure = Some(crate::encoding::corrupt("comment"));
                    return Ok(false);
                }
            }
            Ok(true)
        },
    )?;
    match failure {
        Some(error) => Err(error),
        None => Ok(found),
    }
}

pub(crate) fn forget_owner(writer: &mut dyn KeyValueWrite, owner: &str) -> SqlResult<()> {
    crate::store::delete_prefix(writer, &comments_prefix(owner))
}

pub(crate) fn forget_part(
    writer: &mut dyn KeyValueWrite,
    owner: &str,
    kind: CommentKind,
    part: &str,
) -> SqlResult<()> {
    writer.delete(&comment_key(owner, kind, part))?;
    Ok(())
}

pub(crate) fn move_part(
    writer: &mut dyn KeyValueWrite,
    owner: &str,
    kind: CommentKind,
    from: &str,
    to: &str,
) -> SqlResult<()> {
    let key = comment_key(owner, kind, from);
    if let Some(bytes) = writer.get(&key)? {
        writer.delete(&key)?;
        writer.put(&comment_key(owner, kind, to), &bytes)?;
    }
    Ok(())
}

pub(crate) fn move_owner(writer: &mut dyn KeyValueWrite, from: &str, to: &str) -> SqlResult<()> {
    let old = comments_prefix(from);
    let new = comments_prefix(to);
    let mut moved = Vec::new();
    let mut memory = crate::memory::WorkMemory::new();
    writer.scan(
        &old,
        &prefix_end(&old),
        ScanDirection::Forward,
        &mut |key, value| {
            memory.charge(key.len() + value.len() + 64)?;
            moved.push((key[old.len()..].to_vec(), value.to_vec()));
            Ok(true)
        },
    )?;
    crate::store::delete_prefix(writer, &new)?;
    for (rest, value) in moved {
        let mut from_key = old.clone();
        from_key.extend_from_slice(&rest);
        writer.delete(&from_key)?;
        let mut to_key = new.clone();
        to_key.extend_from_slice(&rest);
        writer.put(&to_key, &value)?;
    }
    Ok(())
}

pub(crate) fn move_schema_comment(
    writer: &mut dyn KeyValueWrite,
    from: &str,
    to: &str,
) -> SqlResult<()> {
    let key = schema_comment_key(from);
    if let Some(bytes) = writer.get(&key)? {
        writer.delete(&key)?;
        writer.put(&schema_comment_key(to), &bytes)?;
    }
    Ok(())
}

pub(crate) fn store_created(
    writer: &mut dyn KeyValueWrite,
    table: &str,
    create: &crate::ast::CreateTable,
) -> SqlResult<()> {
    if let Some(comment) = &create.comment {
        store(
            writer,
            &comment_key(table, CommentKind::Relation, ""),
            Some(comment),
        )?;
    }
    for column in &create.columns {
        if let Some(comment) = &column.comment {
            store(
                writer,
                &comment_key(table, CommentKind::Column, &column.name.text),
                Some(comment),
            )?;
        }
    }
    Ok(())
}
