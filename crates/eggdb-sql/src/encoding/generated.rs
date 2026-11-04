use super::{Column, Reader, dialect_code, dialect_of, put_bytes, put_varint};
use crate::error::SqlResult;

const VIRTUAL: u8 = 0;
const STORED: u8 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GeneratedColumn {
    pub(crate) column: usize,
    pub(crate) dialect: crate::Dialect,
    pub(crate) expression: String,
    pub(crate) stored: bool,
}

pub(super) fn encode_generated(generated: &[GeneratedColumn], out: &mut Vec<u8>) {
    put_varint(out, generated.len() as u64);
    for entry in generated {
        put_varint(out, entry.column as u64);
        out.push(if entry.stored { STORED } else { VIRTUAL });
        out.push(dialect_code(entry.dialect));
        put_bytes(out, entry.expression.as_bytes());
    }
}

pub(super) fn decode_generated(
    reader: &mut Reader<'_>,
    columns: &[Column],
) -> SqlResult<Vec<GeneratedColumn>> {
    let count = reader.count()?;
    if count == 0 || count > columns.len() {
        return Err(reader.damaged());
    }
    let mut generated: Vec<GeneratedColumn> = Vec::with_capacity(count);
    for _ in 0..count {
        let column = usize::try_from(reader.varint()?).map_err(|_| reader.damaged())?;
        let stored = match reader.byte()? {
            VIRTUAL => false,
            STORED => true,
            _ => return Err(reader.damaged()),
        };
        let dialect = dialect_of(reader.byte()?).ok_or_else(|| reader.damaged())?;
        let expression = reader.text()?;
        let fits = columns
            .get(column)
            .is_some_and(|found| found.default.is_none() && !found.auto_increment)
            && generated
                .last()
                .is_none_or(|previous| previous.column < column)
            && !expression.trim().is_empty();
        if !fits {
            return Err(reader.damaged());
        }
        generated.push(GeneratedColumn {
            column,
            dialect,
            expression,
            stored,
        });
    }
    Ok(generated)
}

pub(crate) fn generated_without_column(
    generated: &[GeneratedColumn],
    removed: usize,
) -> Vec<GeneratedColumn> {
    generated
        .iter()
        .filter(|entry| entry.column != removed)
        .map(|entry| GeneratedColumn {
            column: if entry.column > removed {
                entry.column - 1
            } else {
                entry.column
            },
            ..entry.clone()
        })
        .collect()
}
