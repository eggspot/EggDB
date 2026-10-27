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
pub(crate) const MAX_VIEW_TEXT: usize = 1 << 20;

pub(crate) fn schema_key(name: &str) -> Vec<u8> {
    let mut key = NAMESPACE.to_vec();
    key.push(SCHEMA_SPACE);
    key.extend_from_slice(name.as_bytes());
    key
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
    pub(crate) source: String,
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
            put_bytes(&mut out, column.source.as_bytes());
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
        if count == 0
            || count > MAX_COLUMNS
            || query.trim().is_empty()
            || query.len() > MAX_VIEW_TEXT
        {
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
            let source = reader.name()?;
            if columns.iter().any(|known| known.name == column) {
                return Err(reader.damaged());
            }
            columns.push(ViewColumn {
                name: column,
                sql_type,
                source,
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

#[cfg(test)]
mod tests {
    use super::{
        ViewColumn, ViewDefinition, decode_comment, decode_schema, encode_comment, encode_schema,
        is_view_record,
    };
    use crate::encoding::{Column, TableDefinition};
    use crate::types::SqlType;

    fn view() -> ViewDefinition {
        ViewDefinition {
            name: "app\0v".into(),
            dialect: crate::Dialect::PostgreSql,
            query: "SELECT a, b FROM app.t".into(),
            columns: vec![
                ViewColumn {
                    name: "a".into(),
                    sql_type: Some(SqlType::Integer),
                    source: "a".into(),
                },
                ViewColumn {
                    name: "b".into(),
                    sql_type: None,
                    source: "?column?".into(),
                },
            ],
            search_path: vec!["app".into(), "public".into()],
            references: vec!["app\0t".into(), "t".into()],
            expands_wildcard: true,
        }
    }

    fn schema_table() -> TableDefinition {
        TableDefinition {
            id: 9,
            name: "app\0t".into(),
            columns: vec![Column {
                name: "a".into(),
                sql_type: SqlType::Integer,
                not_null: false,
                default: None,
                auto_increment: false,
            }],
            ..TableDefinition::default()
        }
    }

    #[test]
    fn views_round_trip_and_every_truncation_is_damage() {
        let bytes = view().encode();
        assert!(is_view_record(&bytes));
        assert_eq!(ViewDefinition::decode("app\0v", &bytes).unwrap(), view());
        for cut in 0..bytes.len() {
            assert!(ViewDefinition::decode("app\0v", &bytes[..cut]).is_err());
        }
        let mut longer = bytes.clone();
        longer.push(0);
        assert!(ViewDefinition::decode("app\0v", &longer).is_err());
    }

    #[test]
    fn hostile_view_bytes_are_errors_never_panics_or_huge_allocations() {
        let bytes = view().encode();
        for at in 0..bytes.len() {
            for value in [0u8, 1, 2, 3, 4, 7, 8, 0x7F, 0x80, 0xFE, 0xFF] {
                let mut changed = bytes.clone();
                changed[at] = value;
                let _ = ViewDefinition::decode("app\0v", &changed);
            }
        }
        let huge = [
            8u8, 0, 0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x7F,
        ];
        assert!(ViewDefinition::decode("v", &huge).is_err());
        for name in ["\0v", "v\0", "public\0v", "a\0b\0c"] {
            let error = ViewDefinition::decode(name, &bytes).unwrap_err();
            assert_eq!(error.state().code(), "XX001");
        }
    }

    #[test]
    fn a_table_in_a_schema_is_version_7_which_older_releases_refuse_by_name() {
        let table = schema_table();
        let bytes = table.encode();
        assert_eq!(bytes[0], 7, "releases that read up to version 6 refuse it");
        assert_eq!(TableDefinition::decode("app\0t", &bytes).unwrap(), table);
        assert!(TableDefinition::decode("t", &bytes).is_err());
        let mut public = schema_table();
        public.name = "t".into();
        let plain = public.encode();
        assert_eq!(plain[0], 1, "a public table keeps its exact bytes");
        assert!(TableDefinition::decode("app\0t", &plain).is_err());
        for cut in 0..bytes.len() {
            assert!(TableDefinition::decode("app\0t", &bytes[..cut]).is_err());
        }
        for name in ["\0t", "t\0", "public\0t", "a\0b\0c"] {
            assert!(TableDefinition::decode(name, &bytes).is_err(), "{name:?}");
        }
    }

    #[test]
    fn a_view_record_read_as_a_table_names_the_view() {
        let error = TableDefinition::decode("app\0v", &view().encode()).unwrap_err();
        assert_eq!(error.state().code(), "42809");
    }

    #[test]
    fn schema_and_comment_records_round_trip_and_refuse_damage() {
        decode_schema("app", &encode_schema()).unwrap();
        assert!(decode_schema("app", &[]).is_err());
        assert!(decode_schema("app", &[1, 0]).is_err());
        assert_eq!(
            decode_schema("app", &[2]).unwrap_err().state().code(),
            "0A000"
        );
        assert!(decode_schema("", &encode_schema()).is_err());
        assert!(decode_schema("a\0b", &encode_schema()).is_err());
        assert_eq!(decode_comment(&encode_comment("hello")).unwrap(), "hello");
        assert_eq!(decode_comment(&encode_comment("")).unwrap(), "");
        assert!(decode_comment(&[]).is_err());
        assert!(decode_comment(&[1, 0xFF, 0xFE]).is_err());
        assert_eq!(decode_comment(&[9]).unwrap_err().state().code(), "0A000");
    }

    #[test]
    fn a_zero_byte_in_a_stored_column_index_or_constraint_name_is_damage() {
        let damaged = |table: &TableDefinition| {
            TableDefinition::decode(&table.name, &table.encode())
                .is_err_and(|error| error.state().code() == "XX001")
        };
        let mut column = schema_table();
        column.columns[0].name = "a\0b".into();
        assert!(damaged(&column));
        let mut index = schema_table();
        index.indexes = vec![crate::encoding::IndexDefinition {
            id: 1,
            name: "app\0t_a_idx".into(),
            unique: false,
            columns: vec![0],
        }];
        assert!(damaged(&index));
        let mut key = schema_table();
        key.primary_key = vec![0];
        key.primary_key_name = Some("app\0t_pkey".into());
        assert!(damaged(&key));
        let mut check = schema_table();
        check.checks = vec![crate::encoding::CheckConstraint {
            name: "app\0t_a_check".into(),
            dialect: crate::Dialect::PostgreSql,
            expression: "a > 0".into(),
        }];
        assert!(damaged(&check));
        let mut fine = schema_table();
        fine.primary_key = vec![0];
        assert_eq!(fine.primary_key_constraint_name(), "t_pkey");
        assert!(!damaged(&fine));
    }
}
