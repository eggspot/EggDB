use super::{NAMESPACE, Reader, dialect_code, dialect_of, put_bytes, put_varint};
use crate::error::SqlResult;
use crate::partitions::{
    BoundText, IndexLink, KeyPart, MAX_BOUND_VALUES, MAX_INDEX_LINKS, MAX_KEY_COLUMNS,
    PartitionBound, PartitionKey, PartitionRecord, PartitionStrategy,
};

pub(super) const PARTITION_SPACE: u8 = 0x0E;
const RECORD_VERSION: u8 = 1;
const EXPRESSIONS_AND_INDEXES_VERSION: u8 = 2;
const COLUMN_PART: u8 = 0;
const EXPRESSION_PART: u8 = 1;
const KEY_FLAG: u8 = 1;
const PARENT_FLAG: u8 = 2;
const KNOWN_FLAGS: u8 = KEY_FLAG | PARENT_FLAG;
const RANGE: u8 = 1;
const LIST: u8 = 2;
const HASH: u8 = 3;
const DEFAULT: u8 = 4;
const VALUE: u8 = 0;
const NULL: u8 = 1;
const MIN_VALUE: u8 = 2;
const MAX_VALUE: u8 = 3;

pub(crate) fn partition_key(table: &str) -> Vec<u8> {
    let mut key = NAMESPACE.to_vec();
    key.push(PARTITION_SPACE);
    key.extend_from_slice(table.as_bytes());
    key
}

pub(crate) fn partitions_prefix() -> Vec<u8> {
    partition_key("")
}

fn strategy_code(strategy: PartitionStrategy) -> u8 {
    match strategy {
        PartitionStrategy::Range => RANGE,
        PartitionStrategy::List => LIST,
        PartitionStrategy::Hash => HASH,
    }
}

fn strategy_of(code: u8) -> Option<PartitionStrategy> {
    Some(match code {
        RANGE => PartitionStrategy::Range,
        LIST => PartitionStrategy::List,
        HASH => PartitionStrategy::Hash,
        _ => return None,
    })
}

fn put_values(out: &mut Vec<u8>, values: &[BoundText]) {
    put_varint(out, values.len() as u64);
    for value in values {
        match value {
            BoundText::Value(text) => {
                out.push(VALUE);
                put_bytes(out, text.as_bytes());
            }
            BoundText::Null => out.push(NULL),
            BoundText::MinValue => out.push(MIN_VALUE),
            BoundText::MaxValue => out.push(MAX_VALUE),
        }
    }
}

fn values(reader: &mut Reader<'_>) -> SqlResult<Vec<BoundText>> {
    let count = reader.count()?;
    if count > MAX_BOUND_VALUES {
        return Err(reader.damaged());
    }
    let mut values = Vec::with_capacity(count);
    for _ in 0..count {
        values.push(match reader.byte()? {
            VALUE => BoundText::Value(reader.text()?),
            NULL => BoundText::Null,
            MIN_VALUE => BoundText::MinValue,
            MAX_VALUE => BoundText::MaxValue,
            _ => return Err(reader.damaged()),
        });
    }
    Ok(values)
}

fn needs_version_2(record: &PartitionRecord) -> bool {
    record.key.as_ref().is_some_and(PartitionKey::has_expressions)
        || (record.parent.is_some() && !record.index_links.is_empty())
}

pub(crate) fn encode_partitioning(record: &PartitionRecord) -> Vec<u8> {
    let version_2 = needs_version_2(record);
    let mut out = vec![if version_2 {
        EXPRESSIONS_AND_INDEXES_VERSION
    } else {
        RECORD_VERSION
    }];
    let mut flags = 0;
    if record.key.is_some() {
        flags |= KEY_FLAG;
    }
    if record.parent.is_some() {
        flags |= PARENT_FLAG;
    }
    out.push(flags);
    if let Some(key) = &record.key {
        out.push(strategy_code(key.strategy));
        put_varint(&mut out, key.parts.len() as u64);
        for part in &key.parts {
            match part {
                KeyPart::Column(column) => {
                    if version_2 {
                        out.push(COLUMN_PART);
                    }
                    put_bytes(&mut out, column.as_bytes());
                }
                KeyPart::Expression { text, dialect } => {
                    out.push(EXPRESSION_PART);
                    out.push(dialect_code(*dialect));
                    put_bytes(&mut out, text.as_bytes());
                }
            }
        }
    }
    if let Some((parent, bound)) = &record.parent {
        put_bytes(&mut out, parent.as_bytes());
        match bound {
            PartitionBound::Range { from, to } => {
                out.push(RANGE);
                put_values(&mut out, from);
                put_values(&mut out, to);
            }
            PartitionBound::List(values) => {
                out.push(LIST);
                put_values(&mut out, values);
            }
            PartitionBound::Hash { modulus, remainder } => {
                out.push(HASH);
                put_varint(&mut out, u64::from(*modulus));
                put_varint(&mut out, u64::from(*remainder));
            }
            PartitionBound::Default => out.push(DEFAULT),
        }
        if version_2 {
            put_varint(&mut out, record.index_links.len() as u64);
            for link in &record.index_links {
                put_varint(&mut out, u64::from(link.index));
                put_varint(&mut out, u64::from(link.parent_index));
            }
        }
    }
    out
}

fn key_part(reader: &mut Reader<'_>, version: u8) -> SqlResult<KeyPart> {
    if version < EXPRESSIONS_AND_INDEXES_VERSION {
        return Ok(KeyPart::Column(reader.name()?));
    }
    match reader.byte()? {
        COLUMN_PART => Ok(KeyPart::Column(reader.name()?)),
        EXPRESSION_PART => {
            let dialect = dialect_of(reader.byte()?).ok_or_else(|| reader.damaged())?;
            let text = reader.text()?;
            if text.is_empty() {
                return Err(reader.damaged());
            }
            Ok(KeyPart::Expression { text, dialect })
        }
        _ => Err(reader.damaged()),
    }
}

fn index_links(reader: &mut Reader<'_>) -> SqlResult<Vec<IndexLink>> {
    let count = reader.count()?;
    if count > MAX_INDEX_LINKS {
        return Err(reader.damaged());
    }
    let mut links: Vec<IndexLink> = Vec::with_capacity(count);
    for _ in 0..count {
        let index = u32::try_from(reader.varint()?).map_err(|_| reader.damaged())?;
        let parent_index = u32::try_from(reader.varint()?).map_err(|_| reader.damaged())?;
        if links
            .iter()
            .any(|link| link.index == index || link.parent_index == parent_index)
        {
            return Err(reader.damaged());
        }
        links.push(IndexLink {
            index,
            parent_index,
        });
    }
    Ok(links)
}

pub(crate) fn decode_partitioning(table: &str, bytes: &[u8]) -> SqlResult<PartitionRecord> {
    let mut reader = Reader::new(bytes, "partitioning");
    let version = reader.version_up_to(EXPRESSIONS_AND_INDEXES_VERSION)?;
    if table.is_empty() || !crate::schemas::is_well_formed(table) {
        return Err(reader.damaged());
    }
    let flags = reader.byte()?;
    if flags == 0 || flags & !KNOWN_FLAGS != 0 {
        return Err(reader.damaged());
    }
    let key = if flags & KEY_FLAG != 0 {
        let strategy = strategy_of(reader.byte()?).ok_or_else(|| reader.damaged())?;
        let count = reader.count()?;
        if count == 0 || count > MAX_KEY_COLUMNS {
            return Err(reader.damaged());
        }
        let mut parts = Vec::with_capacity(count);
        for _ in 0..count {
            parts.push(key_part(&mut reader, version)?);
        }
        if strategy == PartitionStrategy::List && count != 1 {
            return Err(reader.damaged());
        }
        Some(PartitionKey { strategy, parts })
    } else {
        None
    };
    let parent = if flags & PARENT_FLAG != 0 {
        let parent = reader.name()?;
        if parent == table || !crate::schemas::is_well_formed(&parent) {
            return Err(reader.damaged());
        }
        let bound = match reader.byte()? {
            RANGE => {
                let from = values(&mut reader)?;
                let to = values(&mut reader)?;
                if from.is_empty() || from.len() != to.len() {
                    return Err(reader.damaged());
                }
                PartitionBound::Range { from, to }
            }
            LIST => PartitionBound::List(values(&mut reader)?),
            HASH => {
                let modulus = u32::try_from(reader.varint()?).map_err(|_| reader.damaged())?;
                let remainder = u32::try_from(reader.varint()?).map_err(|_| reader.damaged())?;
                if modulus == 0 || remainder >= modulus {
                    return Err(reader.damaged());
                }
                PartitionBound::Hash { modulus, remainder }
            }
            DEFAULT => PartitionBound::Default,
            _ => return Err(reader.damaged()),
        };
        Some((parent, bound))
    } else {
        None
    };
    let index_links = if version >= EXPRESSIONS_AND_INDEXES_VERSION && parent.is_some() {
        index_links(&mut reader)?
    } else {
        Vec::new()
    };
    reader.finish()?;
    Ok(PartitionRecord {
        key,
        parent,
        index_links,
    })
}
