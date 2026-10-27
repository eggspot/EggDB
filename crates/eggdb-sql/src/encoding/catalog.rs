use super::{
    MAX_COLUMNS, NAMESPACE, Reader, VIEW_FORMAT_VERSION, dialect_code, dialect_of, put_bytes,
    put_type, put_varint,
};
use crate::dialect::Dialect;
use crate::error::SqlResult;
use crate::types::SqlType;

const SCHEMA_SPACE: u8 = 0x07;
const DEPENDENCY_SPACE: u8 = 0x08;
const COMMENT_SPACE: u8 = 0x09;
const RECORD_VERSION: u8 = 1;
const NO_TYPE: u8 = 0;
pub(crate) const MAX_SEARCH_PATH: usize = 64;
pub(crate) const MAX_VIEW_REFERENCES: usize = 4096;

pub(crate) fn schema_key(name: &str) -> Vec<u8> {
    let mut key = NAMESPACE.to_vec();
    key.push(SCHEMA_SPACE);
    key.extend_from_slice(name.as_bytes());
    key
}

pub(crate) fn schemas_prefix() -> Vec<u8> {
    schema_key("")
}

pub(crate) fn encode_schema() -> Vec<u8> {
    vec![RECORD_VERSION]
}

pub(crate) fn decode_schema(name: &str, bytes: &[u8]) -> SqlResult<()> {
    let mut reader = Reader::new(bytes, "schema");
    reader.version()?;
    reader.finish()?;
    if name.is_empty() || name.contains(crate::schemas::QUALIFIER) {
        return Err(reader.damaged());
    }
    Ok(())
}

pub(crate) fn dependencies_prefix(referenced: &str) -> Vec<u8> {
    let mut key = NAMESPACE.to_vec();
    key.push(DEPENDENCY_SPACE);
    put_bytes(&mut key, referenced.as_bytes());
    key
}

pub(crate) fn dependency_key(referenced: &str, dependent: &str) -> Vec<u8> {
    let mut key = dependencies_prefix(referenced);
    key.extend_from_slice(dependent.as_bytes());
    key
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommentKind {
    Relation,
    Column,
    Index,
    Constraint,
    Schema,
}

impl CommentKind {
    fn code(self) -> u8 {
        match self {
            Self::Relation => 1,
            Self::Column => 2,
            Self::Index => 3,
            Self::Constraint => 4,
            Self::Schema => 5,
        }
    }
}

pub(crate) fn comments_prefix(owner: &str) -> Vec<u8> {
    let mut key = NAMESPACE.to_vec();
    key.push(COMMENT_SPACE);
    put_bytes(&mut key, owner.as_bytes());
    key
}

pub(crate) fn comment_key(owner: &str, kind: CommentKind, part: &str) -> Vec<u8> {
    let mut key = comments_prefix(owner);
    key.push(kind.code());
    key.extend_from_slice(part.as_bytes());
    key
}

pub(crate) fn schema_comment_key(schema: &str) -> Vec<u8> {
    comment_key("", CommentKind::Schema, schema)
}

pub(crate) fn comment_kind_prefix(owner: &str, kind: CommentKind) -> Vec<u8> {
    comment_key(owner, kind, "")
}

pub(crate) fn encode_comment(text: &str) -> Vec<u8> {
    let mut out = vec![RECORD_VERSION];
    out.extend_from_slice(text.as_bytes());
    out
}

pub(crate) fn decode_comment(bytes: &[u8]) -> SqlResult<String> {
    let mut reader = Reader::new(bytes, "comment");
    reader.version()?;
    let rest = reader.take(bytes.len().saturating_sub(1))?;
    String::from_utf8(rest.to_vec()).map_err(|_| reader.damaged())
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ViewColumn {
    pub(crate) name: String,
    pub(crate) sql_type: Option<SqlType>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ViewDefinition {
    pub(crate) name: String,
    pub(crate) dialect: Dialect,
    pub(crate) query: String,
    pub(crate) columns: Vec<ViewColumn>,
    pub(crate) search_path: Vec<String>,
    pub(crate) references: Vec<String>,
    pub(crate) expands_wildcard: bool,
}

impl ViewDefinition {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = vec![VIEW_FORMAT_VERSION, dialect_code(self.dialect)];
        out.push(u8::from(self.expands_wildcard));
        put_bytes(&mut out, self.query.as_bytes());
        put_varint(&mut out, self.columns.len() as u64);
        for column in &self.columns {
            put_bytes(&mut out, column.name.as_bytes());
            match column.sql_type {
                Some(sql_type) => put_type(&mut out, sql_type),
                None => out.push(NO_TYPE),
            }
        }
        put_varint(&mut out, self.search_path.len() as u64);
        for schema in &self.search_path {
            put_bytes(&mut out, schema.as_bytes());
        }
        put_varint(&mut out, self.references.len() as u64);
        for reference in &self.references {
            put_bytes(&mut out, reference.as_bytes());
        }
        out
    }

    pub(crate) fn decode(name: &str, bytes: &[u8]) -> SqlResult<Self> {
        let mut reader = Reader::new(bytes, "view definition");
        if reader.byte()? != VIEW_FORMAT_VERSION || !crate::schemas::is_well_formed(name) {
            return Err(reader.damaged());
        }
        let dialect = dialect_of(reader.byte()?).ok_or_else(|| reader.damaged())?;
        let expands_wildcard = match reader.byte()? {
            0 => false,
            1 => true,
            _ => return Err(reader.damaged()),
        };
        let query = reader.text()?;
        let count = reader.count()?;
        if count == 0 || count > MAX_COLUMNS || query.trim().is_empty() {
            return Err(reader.damaged());
        }
        let mut columns: Vec<ViewColumn> = Vec::with_capacity(count);
        for _ in 0..count {
            let column = reader.name()?;
            let sql_type = if reader.bytes.get(reader.at) == Some(&NO_TYPE) {
                reader.byte()?;
                None
            } else {
                Some(reader.sql_type(true, true)?)
            };
            if columns.iter().any(|known| known.name == column) {
                return Err(reader.damaged());
            }
            columns.push(ViewColumn {
                name: column,
                sql_type,
            });
        }
        let path_length = reader.count()?;
        if path_length > MAX_SEARCH_PATH {
            return Err(reader.damaged());
        }
        let mut search_path = Vec::with_capacity(path_length);
        for _ in 0..path_length {
            let schema = reader.name()?;
            if schema.contains(crate::schemas::QUALIFIER) {
                return Err(reader.damaged());
            }
            search_path.push(schema);
        }
        let reference_count = reader.count()?;
        if reference_count > MAX_VIEW_REFERENCES {
            return Err(reader.damaged());
        }
        let mut references = Vec::with_capacity(reference_count);
        for _ in 0..reference_count {
            let reference = reader.name()?;
            if !crate::schemas::is_well_formed(&reference) {
                return Err(reader.damaged());
            }
            references.push(reference);
        }
        reader.finish()?;
        Ok(Self {
            name: name.to_owned(),
            dialect,
            query,
            columns,
            search_path,
            references,
            expands_wildcard,
        })
    }
}

pub(crate) fn is_view_record(bytes: &[u8]) -> bool {
    bytes.first() == Some(&VIEW_FORMAT_VERSION)
}
