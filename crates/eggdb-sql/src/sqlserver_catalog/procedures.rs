use super::information_schema::{column_values, referenced_constraint_name};
use super::types::{DATA_TYPES, DataTypeInfo, SqlServerTypeName};
use super::{
    ColumnInfo, ObjectKind, ObjectSource, SqlServerColumnType, SqlServerIndex, SqlServerObject,
    SqlServerObjects, UNKNOWN_DATE, default_definition, foreign_key_facts, relation_columns,
};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::pg_catalog::{CatalogSnapshot, ConstraintSource, IndexSource, index_columns};
use crate::session::{OutputColumn, QueryResult};
use crate::types::{SqlType, Value};
use std::cmp::Ordering;

const SYSNAME: SqlType = SqlType::Varchar(Some(128));
const SMALLINT: SqlType = SqlType::SmallInt;
const TINYINT: SqlType = SqlType::SmallInt;
const INT: SqlType = SqlType::Integer;
const REMARKS: SqlType = SqlType::Varchar(Some(254));
const NVARCHAR_4000: SqlType = SqlType::Varchar(Some(4000));
const NVARCHAR_128: SqlType = SqlType::Varchar(Some(128));
const NVARCHAR_MAX: SqlType = SqlType::Text;
const NOT_DEFERRABLE: i64 = 7;
const NOT_PSEUDO: i64 = 1;
const ODBC_3: i64 = 3;
const ODBC_2: i64 = 2;

/// The catalog procedures EggDB answers in the Transact-SQL dialect, by `EXEC` in a batch or
/// by an RPC request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CatalogProcedure {
    Tables,
    Columns,
    Columns100,
    PrimaryKeys,
    ForeignKeys,
    Statistics,
    Statistics100,
    SpecialColumns,
    SpecialColumns100,
    DataTypeInfo,
    DataTypeInfo100,
    StoredProcedures,
    ProcedureColumns,
    ProcedureColumns100,
    Help,
    HelpIndex,
    Databases,
    ColumnsManaged,
    IndexesManaged,
    IndexColumnsManaged,
    TableCollations100,
}

impl CatalogProcedure {
    const ALL: [Self; 21] = [
        Self::Tables,
        Self::Columns,
        Self::Columns100,
        Self::PrimaryKeys,
        Self::ForeignKeys,
        Self::Statistics,
        Self::Statistics100,
        Self::SpecialColumns,
        Self::SpecialColumns100,
        Self::DataTypeInfo,
        Self::DataTypeInfo100,
        Self::StoredProcedures,
        Self::ProcedureColumns,
        Self::ProcedureColumns100,
        Self::Help,
        Self::HelpIndex,
        Self::Databases,
        Self::ColumnsManaged,
        Self::IndexesManaged,
        Self::IndexColumnsManaged,
        Self::TableCollations100,
    ];

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Tables => "sp_tables",
            Self::Columns => "sp_columns",
            Self::Columns100 => "sp_columns_100",
            Self::PrimaryKeys => "sp_pkeys",
            Self::ForeignKeys => "sp_fkeys",
            Self::Statistics => "sp_statistics",
            Self::Statistics100 => "sp_statistics_100",
            Self::SpecialColumns => "sp_special_columns",
            Self::SpecialColumns100 => "sp_special_columns_100",
            Self::DataTypeInfo => "sp_datatype_info",
            Self::DataTypeInfo100 => "sp_datatype_info_100",
            Self::StoredProcedures => "sp_stored_procedures",
            Self::ProcedureColumns => "sp_sproc_columns",
            Self::ProcedureColumns100 => "sp_sproc_columns_100",
            Self::Help => "sp_help",
            Self::HelpIndex => "sp_helpindex",
            Self::Databases => "sp_databases",
            Self::ColumnsManaged => "sp_columns_managed",
            Self::IndexesManaged => "sp_indexes_managed",
            Self::IndexColumnsManaged => "sp_indexcolumns_managed",
            Self::TableCollations100 => "sp_tablecollations_100",
        }
    }

    pub(crate) fn named(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|procedure| procedure.name() == name)
    }

    pub(crate) fn parameters(self) -> &'static [&'static str] {
        match self {
            Self::Tables => &[
                "@table_name",
                "@table_owner",
                "@table_qualifier",
                "@table_type",
                "@fusepattern",
            ],
            Self::Columns => &[
                "@table_name",
                "@table_owner",
                "@table_qualifier",
                "@column_name",
                "@odbcver",
            ],
            Self::Columns100 => &[
                "@table_name",
                "@table_owner",
                "@table_qualifier",
                "@column_name",
                "@namescope",
                "@odbcver",
                "@fusepattern",
            ],
            Self::PrimaryKeys => &["@table_name", "@table_owner", "@table_qualifier"],
            Self::ForeignKeys => &[
                "@pktable_name",
                "@pktable_owner",
                "@pktable_qualifier",
                "@fktable_name",
                "@fktable_owner",
                "@fktable_qualifier",
            ],
            Self::Statistics | Self::Statistics100 => &[
                "@table_name",
                "@table_owner",
                "@table_qualifier",
                "@index_name",
                "@is_unique",
                "@accuracy",
            ],
            Self::SpecialColumns | Self::SpecialColumns100 => &[
                "@table_name",
                "@table_owner",
                "@table_qualifier",
                "@col_type",
                "@scope",
                "@nullable",
                "@odbcver",
            ],
            Self::DataTypeInfo | Self::DataTypeInfo100 => &["@data_type", "@odbcver"],
            Self::StoredProcedures => &["@sp_name", "@sp_owner", "@sp_qualifier", "@fusepattern"],
            Self::ProcedureColumns | Self::ProcedureColumns100 => &[
                "@procedure_name",
                "@procedure_owner",
                "@procedure_qualifier",
                "@column_name",
                "@odbcver",
                "@fusepattern",
            ],
            Self::Help | Self::HelpIndex => &["@objname"],
            Self::Databases => &[],
            Self::ColumnsManaged => &["@catalog", "@owner", "@table", "@column", "@schematype"],
            Self::IndexesManaged => &["@catalog", "@owner", "@table", "@name"],
            Self::IndexColumnsManaged => {
                &["@catalog", "@owner", "@table", "@constraintname", "@column"]
            }
            Self::TableCollations100 => &["@object"],
        }
    }

    fn required(self) -> usize {
        match self {
            Self::Columns
            | Self::Columns100
            | Self::PrimaryKeys
            | Self::Statistics
            | Self::Statistics100
            | Self::SpecialColumns
            | Self::SpecialColumns100
            | Self::HelpIndex => 1,
            _ => 0,
        }
    }
}

fn procedure_error(code: u16, state: SqlState, message: String) -> SqlError {
    SqlError::new(state, message).with_sqlserver_code(code)
}

/// Binds the arguments of a call, given by position or by `@name` (in any case), to the
/// procedure's parameters; a parameter left out or given `DEFAULT` is `None`.
pub(crate) fn bind_arguments(
    procedure: CatalogProcedure,
    arguments: Vec<(Option<String>, Option<Value>)>,
) -> SqlResult<Vec<Option<Value>>> {
    let parameters = procedure.parameters();
    let mut bound: Vec<Option<Value>> = vec![None; parameters.len()];
    let mut supplied = vec![false; parameters.len()];
    let mut next = 0;
    for (name, value) in arguments {
        let at = match name {
            Some(name) => {
                let lower = name.to_ascii_lowercase();
                parameters
                    .iter()
                    .position(|parameter| *parameter == lower)
                    .ok_or_else(|| {
                        procedure_error(
                            8145,
                            SqlState::UNDEFINED_PARAMETER,
                            format!(
                                "{name} is not a parameter for procedure {}.",
                                procedure.name()
                            ),
                        )
                    })?
            }
            None => {
                let at = next;
                next += 1;
                if at >= parameters.len() {
                    return Err(procedure_error(
                        8144,
                        SqlState::SYNTAX_ERROR,
                        format!(
                            "Procedure or function {} has too many arguments specified.",
                            procedure.name()
                        ),
                    ));
                }
                at
            }
        };
        if let Some(slot) = bound.get_mut(at) {
            *slot = value.filter(|value| !value.is_null());
        }
        if let Some(flag) = supplied.get_mut(at) {
            *flag = true;
        }
    }
    for (at, parameter) in parameters.iter().enumerate().take(procedure.required()) {
        if !supplied.get(at).copied().unwrap_or(false) {
            return Err(procedure_error(
                201,
                SqlState::UNDEFINED_PARAMETER,
                format!(
                    "Procedure or function '{}' expects parameter '{parameter}', which was not supplied.",
                    procedure.name()
                ),
            ));
        }
    }
    Ok(bound)
}

struct Arguments {
    values: Vec<Option<Value>>,
}

impl Arguments {
    fn text(&self, at: usize) -> Option<String> {
        self.values.get(at)?.as_ref()?.to_text()
    }

    fn number(&self, at: usize) -> Option<i64> {
        let value = self.values.get(at)?.as_ref()?;
        value
            .as_integer()
            .or_else(|| value.to_text()?.trim().parse().ok())
    }

    fn flag(&self, at: usize, default: bool) -> bool {
        self.number(at).map_or(default, |number| number != 0)
    }
}

pub(crate) fn like(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.to_lowercase().chars().collect();
    let text: Vec<char> = text.to_lowercase().chars().collect();
    let mut at_pattern = 0;
    let mut at_text = 0;
    let mut star: Option<(usize, usize)> = None;
    while at_text < text.len() {
        if let Some(step) = single_match(&pattern, at_pattern, text[at_text]) {
            at_pattern += step;
            at_text += 1;
        } else if pattern.get(at_pattern) == Some(&'%') {
            star = Some((at_pattern, at_text));
            at_pattern += 1;
        } else if let Some((star_pattern, star_text)) = star {
            at_pattern = star_pattern + 1;
            at_text = star_text + 1;
            star = Some((star_pattern, star_text + 1));
        } else {
            return false;
        }
    }
    pattern[at_pattern.min(pattern.len())..]
        .iter()
        .all(|character| *character == '%')
}

fn single_match(pattern: &[char], at: usize, character: char) -> Option<usize> {
    match pattern.get(at)? {
        '%' => None,
        '_' => Some(1),
        '[' => {
            let close = pattern
                .get(at + 1..)?
                .iter()
                .skip(1)
                .position(|found| *found == ']')
                .map(|offset| at + 2 + offset)?;
            let set = pattern.get(at + 1..close)?;
            let (negated, set) = match set.split_first() {
                Some(('^', rest)) if !rest.is_empty() => (true, rest),
                _ => (false, set),
            };
            let mut found = false;
            let mut index = 0;
            while index < set.len() {
                if set.get(index + 1) == Some(&'-') && index + 2 < set.len() {
                    if set[index] <= character && character <= set[index + 2] {
                        found = true;
                    }
                    index += 3;
                } else {
                    if set[index] == character {
                        found = true;
                    }
                    index += 1;
                }
            }
            (found != negated).then_some(close + 1 - at)
        }
        literal => (*literal == character).then_some(1),
    }
}

struct NameFilter {
    pattern: Option<String>,
    use_pattern: bool,
}

impl NameFilter {
    fn new(pattern: Option<String>, use_pattern: bool) -> Self {
        Self {
            pattern,
            use_pattern,
        }
    }

    fn exact(name: Option<String>) -> Self {
        Self::new(name, false)
    }

    fn matches(&self, name: &str) -> bool {
        match &self.pattern {
            None => true,
            Some(pattern) if self.use_pattern => like(pattern, name),
            Some(pattern) => pattern.eq_ignore_ascii_case(name),
        }
    }
}

fn compare_values(left: &Value, right: &Value) -> Ordering {
    match (left, right) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Less,
        (_, Value::Null) => Ordering::Greater,
        _ => match (left.as_integer(), right.as_integer()) {
            (Some(left), Some(right)) => left.cmp(&right),
            _ => {
                let left = left.to_text().unwrap_or_default().to_lowercase();
                let right = right.to_text().unwrap_or_default().to_lowercase();
                left.cmp(&right)
            }
        },
    }
}

fn sort_rows(rows: &mut [Vec<Value>], keys: &[usize]) {
    rows.sort_by(|left, right| {
        for key in keys {
            let order = match (left.get(*key), right.get(*key)) {
                (Some(left), Some(right)) => compare_values(left, right),
                _ => Ordering::Equal,
            };
            if order != Ordering::Equal {
                return order;
            }
        }
        Ordering::Equal
    });
}

fn output(columns: &[(&str, SqlType)]) -> Vec<OutputColumn> {
    columns
        .iter()
        .map(|(name, sql_type)| OutputColumn {
            name: (*name).to_owned(),
            sql_type: Some(*sql_type),
        })
        .collect()
}

fn result(columns: &[(&str, SqlType)], rows: Vec<Vec<Value>>) -> QueryResult {
    QueryResult::rows(format!("SELECT {}", rows.len()), output(columns), rows)
}

fn text(value: &str) -> Value {
    Value::Text(value.to_owned())
}

fn small(value: i64) -> Value {
    i16::try_from(value).map_or(Value::Null, Value::SmallInt)
}

fn int(value: i64) -> Value {
    i32::try_from(value).map_or(Value::Null, Value::Integer)
}

fn optional_small(value: Option<i64>) -> Value {
    value.map_or(Value::Null, small)
}

fn optional_int(value: Option<i64>) -> Value {
    value.map_or(Value::Null, int)
}

fn yes_or_no(value: bool) -> Value {
    text(if value { "YES" } else { "NO" })
}

struct Scope<'a> {
    snapshot: &'a CatalogSnapshot,
    objects: &'a SqlServerObjects,
    database: &'a str,
}

impl<'a> Scope<'a> {
    fn qualifier_matches(&self, qualifier: Option<&String>, use_pattern: bool) -> bool {
        NameFilter::new(qualifier.cloned(), use_pattern).matches(self.database)
    }

    fn schema(&self, object: &SqlServerObject) -> &'a str {
        self.objects.schema_name(object.schema_id)
    }

    fn relations(
        &self,
        kinds: &[ObjectKind],
        owner: &NameFilter,
        name: &NameFilter,
    ) -> Vec<&'a SqlServerObject> {
        let objects: &'a SqlServerObjects = self.objects;
        let mut found: Vec<&'a SqlServerObject> = objects
            .relations()
            .filter(|object| kinds.contains(&object.kind))
            .filter(|object| owner.matches(objects.schema_name(object.schema_id)))
            .filter(|object| name.matches(&object.name))
            .collect();
        found.sort_by(|left, right| {
            (objects.schema_name(left.schema_id), left.name.as_str())
                .cmp(&(objects.schema_name(right.schema_id), right.name.as_str()))
        });
        found
    }

    fn columns(&self, object: &SqlServerObject) -> Vec<ColumnInfo<'a>> {
        self.snapshot
            .relations
            .get(object.relation)
            .map_or_else(Vec::new, relation_columns)
    }

    fn default_of(&self, object: &SqlServerObject, column: usize) -> Option<String> {
        self.snapshot
            .relations
            .get(object.relation)?
            .table()?
            .columns
            .get(column)?
            .default
            .as_deref()
            .map(default_definition)
    }
}

/// Runs a catalog procedure on its bound arguments ([`bind_arguments`]) and returns its
/// result sets in order.
pub(crate) fn run_procedure(
    snapshot: &CatalogSnapshot,
    procedure: CatalogProcedure,
    values: Vec<Option<Value>>,
) -> SqlResult<Vec<QueryResult>> {
    let scope = Scope {
        snapshot,
        objects: snapshot.sqlserver_objects(),
        database: snapshot.database.as_str(),
    };
    let arguments = Arguments { values };
    Ok(match procedure {
        CatalogProcedure::Tables => vec![tables(&scope, &arguments)],
        CatalogProcedure::Columns => vec![columns(&scope, &arguments, false)],
        CatalogProcedure::Columns100 => vec![columns(&scope, &arguments, true)],
        CatalogProcedure::PrimaryKeys => vec![primary_keys(&scope, &arguments)],
        CatalogProcedure::ForeignKeys => vec![foreign_keys(&scope, &arguments)],
        CatalogProcedure::Statistics | CatalogProcedure::Statistics100 => {
            vec![statistics(&scope, &arguments)]
        }
        CatalogProcedure::SpecialColumns | CatalogProcedure::SpecialColumns100 => {
            vec![special_columns(&scope, &arguments)]
        }
        CatalogProcedure::DataTypeInfo | CatalogProcedure::DataTypeInfo100 => {
            vec![data_type_info(&arguments)]
        }
        CatalogProcedure::StoredProcedures => vec![result(STORED_PROCEDURE_COLUMNS, Vec::new())],
        CatalogProcedure::ProcedureColumns | CatalogProcedure::ProcedureColumns100 => {
            vec![result(PROCEDURE_COLUMN_COLUMNS, Vec::new())]
        }
        CatalogProcedure::Help => help(&scope, &arguments)?,
        CatalogProcedure::HelpIndex => vec![help_index(&scope, &arguments)?],
        CatalogProcedure::Databases => vec![result(
            DATABASE_COLUMNS,
            vec![vec![text(scope.database), Value::Integer(0), Value::Null]],
        )],
        CatalogProcedure::ColumnsManaged => vec![columns_managed(&scope, &arguments)],
        CatalogProcedure::IndexesManaged => vec![indexes_managed(&scope, &arguments)],
        CatalogProcedure::IndexColumnsManaged => vec![index_columns_managed(&scope, &arguments)],
        CatalogProcedure::TableCollations100 => vec![table_collations(&scope, &arguments)],
    })
}

const TABLE_COLUMNS: &[(&str, SqlType)] = &[
    ("TABLE_QUALIFIER", SYSNAME),
    ("TABLE_OWNER", SYSNAME),
    ("TABLE_NAME", SYSNAME),
    ("TABLE_TYPE", SqlType::Varchar(Some(32))),
    ("REMARKS", REMARKS),
];

fn table_types(text: Option<String>) -> Option<Vec<String>> {
    let text = text?;
    let types: Vec<String> = text
        .replace('\'', "")
        .split(',')
        .map(|kind| kind.trim().to_ascii_uppercase())
        .filter(|kind| !kind.is_empty())
        .collect();
    (!types.is_empty()).then_some(types)
}

fn tables(scope: &Scope<'_>, arguments: &Arguments) -> QueryResult {
    let use_pattern = arguments.flag(4, true);
    if !scope.qualifier_matches(arguments.text(2).as_ref(), use_pattern) {
        return result(TABLE_COLUMNS, Vec::new());
    }
    let wanted = table_types(arguments.text(3));
    let wants = |kind: &str| {
        wanted
            .as_ref()
            .is_none_or(|types| types.iter().any(|wanted| wanted == kind))
    };
    let mut kinds = Vec::new();
    if wants("TABLE") {
        kinds.push(ObjectKind::Table);
    }
    if wants("VIEW") {
        kinds.push(ObjectKind::View);
    }
    let owner = NameFilter::new(arguments.text(1), use_pattern);
    let name = NameFilter::new(arguments.text(0), use_pattern);
    let mut rows: Vec<Vec<Value>> = scope
        .relations(&kinds, &owner, &name)
        .into_iter()
        .map(|object| {
            let kind = if object.kind == ObjectKind::View {
                "VIEW"
            } else {
                "TABLE"
            };
            vec![
                text(scope.database),
                text(scope.schema(object)),
                text(&object.name),
                text(kind),
                Value::Null,
            ]
        })
        .collect();
    sort_rows(&mut rows, &[3, 0, 1, 2]);
    result(TABLE_COLUMNS, rows)
}

const COLUMNS_COLUMNS: &[(&str, SqlType)] = &[
    ("TABLE_QUALIFIER", SYSNAME),
    ("TABLE_OWNER", SYSNAME),
    ("TABLE_NAME", SYSNAME),
    ("COLUMN_NAME", SYSNAME),
    ("DATA_TYPE", SMALLINT),
    ("TYPE_NAME", SYSNAME),
    ("PRECISION", INT),
    ("LENGTH", INT),
    ("SCALE", SMALLINT),
    ("RADIX", SMALLINT),
    ("NULLABLE", SMALLINT),
    ("REMARKS", REMARKS),
    ("COLUMN_DEF", NVARCHAR_4000),
    ("SQL_DATA_TYPE", SMALLINT),
    ("SQL_DATETIME_SUB", SMALLINT),
    ("CHAR_OCTET_LENGTH", INT),
    ("ORDINAL_POSITION", INT),
    ("IS_NULLABLE", REMARKS),
    ("SS_DATA_TYPE", TINYINT),
];

const COLUMNS_100_COLUMNS: &[(&str, SqlType)] = &[
    ("TABLE_QUALIFIER", SYSNAME),
    ("TABLE_OWNER", SYSNAME),
    ("TABLE_NAME", SYSNAME),
    ("COLUMN_NAME", SYSNAME),
    ("DATA_TYPE", SMALLINT),
    ("TYPE_NAME", SYSNAME),
    ("PRECISION", INT),
    ("LENGTH", INT),
    ("SCALE", SMALLINT),
    ("RADIX", SMALLINT),
    ("NULLABLE", SMALLINT),
    ("REMARKS", REMARKS),
    ("COLUMN_DEF", NVARCHAR_4000),
    ("SQL_DATA_TYPE", SMALLINT),
    ("SQL_DATETIME_SUB", SMALLINT),
    ("CHAR_OCTET_LENGTH", INT),
    ("ORDINAL_POSITION", INT),
    ("IS_NULLABLE", REMARKS),
    ("SS_IS_SPARSE", SMALLINT),
    ("SS_IS_COLUMN_SET", SMALLINT),
    ("SS_IS_COMPUTED", SMALLINT),
    ("SS_IS_IDENTITY", SMALLINT),
    ("SS_UDT_CATALOG_NAME", NVARCHAR_128),
    ("SS_UDT_SCHEMA_NAME", NVARCHAR_128),
    ("SS_UDT_ASSEMBLY_TYPE_NAME", NVARCHAR_MAX),
    ("SS_XML_SCHEMACOLLECTION_CATALOG_NAME", NVARCHAR_128),
    ("SS_XML_SCHEMACOLLECTION_SCHEMA_NAME", NVARCHAR_128),
    ("SS_XML_SCHEMACOLLECTION_NAME", NVARCHAR_128),
    ("SS_DATA_TYPE", TINYINT),
];

fn odbc_type(name: SqlServerTypeName, version: i64) -> i64 {
    let code = name.odbc_type();
    if version >= ODBC_3 {
        return code;
    }
    match code {
        93 => 11,
        91 => 9,
        other => other,
    }
}

fn odbc_sql_type(name: SqlServerTypeName, version: i64) -> (i64, Option<i64>) {
    if version >= ODBC_3 {
        name.odbc_sql_type()
    } else {
        (odbc_type(name, version), None)
    }
}

fn type_name(column: &ColumnInfo<'_>) -> String {
    let name = column.column_type.name.name();
    if column.identity {
        format!("{name} identity")
    } else {
        name.to_owned()
    }
}

struct DescribedColumn<'a> {
    object: &'a SqlServerObject,
    position: usize,
    column: ColumnInfo<'a>,
}

fn matching_columns<'a>(
    scope: &Scope<'a>,
    owner: &NameFilter,
    table: &NameFilter,
    column: &NameFilter,
) -> Vec<DescribedColumn<'a>> {
    let mut found = Vec::new();
    for object in scope.relations(&[ObjectKind::Table, ObjectKind::View], owner, table) {
        for (position, facts) in scope.columns(object).into_iter().enumerate() {
            if column.matches(facts.name) {
                found.push(DescribedColumn {
                    object,
                    position,
                    column: facts,
                });
            }
        }
    }
    found
}

fn columns(scope: &Scope<'_>, arguments: &Arguments, extended: bool) -> QueryResult {
    let shape = if extended {
        COLUMNS_100_COLUMNS
    } else {
        COLUMNS_COLUMNS
    };
    let (version_at, pattern_at) = if extended { (5, Some(6)) } else { (4, None) };
    let version = arguments.number(version_at).unwrap_or(ODBC_2);
    let use_pattern = pattern_at.is_none_or(|at| arguments.flag(at, true));
    if !scope.qualifier_matches(arguments.text(2).as_ref(), false) {
        return result(shape, Vec::new());
    }
    let found = matching_columns(
        scope,
        &NameFilter::new(arguments.text(1), use_pattern),
        &NameFilter::new(arguments.text(0), use_pattern),
        &NameFilter::new(arguments.text(3), use_pattern),
    );
    let rows = found
        .iter()
        .map(|described| {
            let column = &described.column;
            let column_type = column.column_type;
            let (sql_data_type, datetime_sub) = odbc_sql_type(column_type.name, version);
            let mut row = vec![
                text(scope.database),
                text(scope.schema(described.object)),
                text(&described.object.name),
                text(column.name),
                small(odbc_type(column_type.name, version)),
                text(&type_name(column)),
                int(column_type.odbc_precision()),
                int(column_type.odbc_length()),
                optional_small(column_type.odbc_scale()),
                optional_small(column_type.odbc_radix()),
                small(i64::from(column.nullable)),
                Value::Null,
                scope
                    .default_of(described.object, described.position)
                    .map_or(Value::Null, Value::Text),
                small(sql_data_type),
                optional_small(datetime_sub),
                optional_int(column_type.odbc_octet_length()),
                int(described.position as i64 + 1),
                yes_or_no(column.nullable),
            ];
            if extended {
                row.extend([
                    small(0),
                    small(0),
                    small(0),
                    small(i64::from(column.identity)),
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Null,
                ]);
            }
            row.push(small(column_type.tds_type(column.nullable)));
            row
        })
        .collect();
    result(shape, rows)
}

const PRIMARY_KEY_COLUMNS: &[(&str, SqlType)] = &[
    ("TABLE_QUALIFIER", SYSNAME),
    ("TABLE_OWNER", SYSNAME),
    ("TABLE_NAME", SYSNAME),
    ("COLUMN_NAME", SYSNAME),
    ("KEY_SEQ", SMALLINT),
    ("PK_NAME", SYSNAME),
];

fn primary_keys(scope: &Scope<'_>, arguments: &Arguments) -> QueryResult {
    if !scope.qualifier_matches(arguments.text(2).as_ref(), false) {
        return result(PRIMARY_KEY_COLUMNS, Vec::new());
    }
    let mut rows = Vec::new();
    for object in scope.relations(
        &[ObjectKind::Table],
        &NameFilter::exact(arguments.text(1)),
        &NameFilter::exact(arguments.text(0)),
    ) {
        let Some(table) = scope
            .snapshot
            .relations
            .get(object.relation)
            .and_then(|entry| entry.table())
        else {
            continue;
        };
        if table.primary_key.is_empty() {
            continue;
        }
        let name = table.primary_key_constraint_name();
        for (sequence, column) in table.primary_key.iter().enumerate() {
            let Some(column) = table.columns.get(*column) else {
                continue;
            };
            rows.push(vec![
                text(scope.database),
                text(scope.schema(object)),
                text(&object.name),
                text(&column.name),
                small(sequence as i64 + 1),
                text(&name),
            ]);
        }
    }
    result(PRIMARY_KEY_COLUMNS, rows)
}

const FOREIGN_KEY_COLUMNS: &[(&str, SqlType)] = &[
    ("PKTABLE_QUALIFIER", SYSNAME),
    ("PKTABLE_OWNER", SYSNAME),
    ("PKTABLE_NAME", SYSNAME),
    ("PKCOLUMN_NAME", SYSNAME),
    ("FKTABLE_QUALIFIER", SYSNAME),
    ("FKTABLE_OWNER", SYSNAME),
    ("FKTABLE_NAME", SYSNAME),
    ("FKCOLUMN_NAME", SYSNAME),
    ("KEY_SEQ", SMALLINT),
    ("UPDATE_RULE", SMALLINT),
    ("DELETE_RULE", SMALLINT),
    ("FK_NAME", SYSNAME),
    ("PK_NAME", SYSNAME),
    ("DEFERRABILITY", SMALLINT),
];

fn odbc_rule(action: crate::ast::ReferentialAction) -> i64 {
    match action {
        crate::ast::ReferentialAction::Cascade => 0,
        crate::ast::ReferentialAction::NoAction | crate::ast::ReferentialAction::Restrict => 1,
        crate::ast::ReferentialAction::SetNull => 2,
        crate::ast::ReferentialAction::SetDefault => 3,
    }
}

fn foreign_keys(scope: &Scope<'_>, arguments: &Arguments) -> QueryResult {
    let primary_table = NameFilter::exact(arguments.text(0));
    let primary_owner = NameFilter::exact(arguments.text(1));
    let foreign_table = NameFilter::exact(arguments.text(3));
    let foreign_owner = NameFilter::exact(arguments.text(4));
    if !scope.qualifier_matches(arguments.text(2).as_ref(), false)
        || !scope.qualifier_matches(arguments.text(5).as_ref(), false)
    {
        return result(FOREIGN_KEY_COLUMNS, Vec::new());
    }
    let objects = scope.objects;
    let mut rows = Vec::new();
    for object in objects
        .objects
        .iter()
        .filter(|object| object.kind == ObjectKind::ForeignKey)
    {
        let ObjectSource::Constraint(at) = object.source else {
            continue;
        };
        let Some(ConstraintSource::Foreign(key_at)) = scope
            .snapshot
            .constraints
            .get(at)
            .map(|constraint| constraint.source)
        else {
            continue;
        };
        let Some(facts) = foreign_key_facts(scope.snapshot, object.relation, key_at) else {
            continue;
        };
        let (Some(child), Some(parent)) = (
            objects.of_relation(object.relation),
            facts
                .referenced_relation
                .and_then(|relation| objects.of_relation(relation)),
        ) else {
            continue;
        };
        if !(primary_table.matches(&parent.name)
            && primary_owner.matches(scope.schema(parent))
            && foreign_table.matches(&child.name)
            && foreign_owner.matches(scope.schema(child)))
        {
            continue;
        }
        let child_columns = scope.columns(child);
        let parent_columns = scope.columns(parent);
        let primary_name = referenced_constraint_name(
            scope.snapshot,
            facts.referenced_relation,
            facts.referenced_index,
        );
        for (sequence, column) in facts.key.columns.iter().enumerate() {
            let parent_column = facts
                .referenced_columns
                .get(sequence)
                .and_then(|at| parent_columns.get(*at))
                .map_or(Value::Null, |found| text(found.name));
            rows.push(vec![
                text(scope.database),
                text(scope.schema(parent)),
                text(&parent.name),
                parent_column,
                text(scope.database),
                text(scope.schema(child)),
                text(&child.name),
                child_columns
                    .get(*column)
                    .map_or(Value::Null, |found| text(found.name)),
                small(sequence as i64 + 1),
                small(odbc_rule(facts.key.on_update)),
                small(odbc_rule(facts.key.on_delete)),
                text(&object.name),
                primary_name.as_deref().map_or(Value::Null, text),
                small(NOT_DEFERRABLE),
            ]);
        }
    }
    sort_rows(&mut rows, &[4, 5, 6, 8]);
    result(FOREIGN_KEY_COLUMNS, rows)
}

const STATISTICS_COLUMNS: &[(&str, SqlType)] = &[
    ("TABLE_QUALIFIER", SYSNAME),
    ("TABLE_OWNER", SYSNAME),
    ("TABLE_NAME", SYSNAME),
    ("NON_UNIQUE", SMALLINT),
    ("INDEX_QUALIFIER", SYSNAME),
    ("INDEX_NAME", SYSNAME),
    ("TYPE", SMALLINT),
    ("SEQ_IN_INDEX", SMALLINT),
    ("COLUMN_NAME", SYSNAME),
    ("COLLATION", SqlType::Bpchar(Some(1))),
    ("CARDINALITY", INT),
    ("PAGES", INT),
    ("FILTER_CONDITION", SqlType::Varchar(Some(128))),
];

fn index_unique(scope: &Scope<'_>, object: &SqlServerObject, index: &SqlServerIndex) -> bool {
    let Some(table) = scope
        .snapshot
        .relations
        .get(object.relation)
        .and_then(|entry| entry.table())
    else {
        return false;
    };
    index
        .source
        .is_some_and(|source| crate::pg_catalog::index_is_unique(table, source))
}

fn statistics(scope: &Scope<'_>, arguments: &Arguments) -> QueryResult {
    if !scope.qualifier_matches(arguments.text(2).as_ref(), false) {
        return result(STATISTICS_COLUMNS, Vec::new());
    }
    let index_filter = NameFilter::new(arguments.text(3), true);
    let unique_only = arguments
        .text(4)
        .is_some_and(|flag| flag.trim().eq_ignore_ascii_case("y"));
    let mut rows = Vec::new();
    for object in scope.relations(
        &[ObjectKind::Table],
        &NameFilter::exact(arguments.text(1)),
        &NameFilter::exact(arguments.text(0)),
    ) {
        rows.push(vec![
            text(scope.database),
            text(scope.schema(object)),
            text(&object.name),
            Value::Null,
            Value::Null,
            Value::Null,
            small(0),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
        ]);
        let Some(table) = scope
            .snapshot
            .relations
            .get(object.relation)
            .and_then(|entry| entry.table())
        else {
            continue;
        };
        for index in scope.objects.indexes(scope.snapshot, object.relation) {
            let Some(source) = index.source else {
                continue;
            };
            let unique = index_unique(scope, object, &index);
            if unique_only && !unique {
                continue;
            }
            let Some(name) = scope
                .objects
                .index_name(scope.snapshot, object.relation, &index)
            else {
                continue;
            };
            if !index_filter.matches(&name) {
                continue;
            }
            let kind = if index.is_clustered() { 1 } else { 3 };
            for (sequence, column) in index_columns(table, source).iter().enumerate() {
                let Some(column) = table.columns.get(*column) else {
                    continue;
                };
                rows.push(vec![
                    text(scope.database),
                    text(scope.schema(object)),
                    text(&object.name),
                    small(i64::from(!unique)),
                    text(&object.name),
                    text(&name),
                    small(kind),
                    small(sequence as i64 + 1),
                    text(&column.name),
                    Value::Bpchar("A".to_owned()),
                    Value::Null,
                    Value::Null,
                    Value::Null,
                ]);
            }
        }
    }
    sort_rows(&mut rows, &[3, 6, 4, 5, 7]);
    result(STATISTICS_COLUMNS, rows)
}

const SPECIAL_COLUMN_COLUMNS: &[(&str, SqlType)] = &[
    ("SCOPE", SMALLINT),
    ("COLUMN_NAME", SYSNAME),
    ("DATA_TYPE", SMALLINT),
    ("TYPE_NAME", SYSNAME),
    ("PRECISION", INT),
    ("LENGTH", INT),
    ("SCALE", SMALLINT),
    ("PSEUDO_COLUMN", SMALLINT),
];

fn special_columns(scope: &Scope<'_>, arguments: &Arguments) -> QueryResult {
    let row_identifier = arguments
        .text(3)
        .is_none_or(|kind| kind.trim().eq_ignore_ascii_case("r"));
    if !row_identifier || !scope.qualifier_matches(arguments.text(2).as_ref(), false) {
        return result(SPECIAL_COLUMN_COLUMNS, Vec::new());
    }
    let session_scope = arguments
        .text(4)
        .is_none_or(|kind| kind.trim().eq_ignore_ascii_case("t"));
    let version = arguments.number(6).unwrap_or(ODBC_2);
    let mut rows = Vec::new();
    for object in scope.relations(
        &[ObjectKind::Table],
        &NameFilter::exact(arguments.text(1)),
        &NameFilter::exact(arguments.text(0)),
    ) {
        let Some(table) = scope
            .snapshot
            .relations
            .get(object.relation)
            .and_then(|entry| entry.table())
        else {
            continue;
        };
        let key: &[usize] = if table.primary_key.is_empty() {
            table
                .indexes
                .iter()
                .find(|index| index.unique)
                .map_or(&[][..], |index| index.columns.as_slice())
        } else {
            &table.primary_key
        };
        let columns = scope.columns(object);
        for column in key {
            let Some(facts) = columns.get(*column) else {
                continue;
            };
            let column_type = facts.column_type;
            rows.push(vec![
                small(if session_scope { 2 } else { 0 }),
                text(facts.name),
                small(odbc_type(column_type.name, version)),
                text(&type_name(facts)),
                int(column_type.odbc_precision()),
                int(column_type.odbc_length()),
                optional_small(column_type.odbc_scale()),
                small(NOT_PSEUDO),
            ]);
        }
    }
    result(SPECIAL_COLUMN_COLUMNS, rows)
}

const DATA_TYPE_COLUMNS: &[(&str, SqlType)] = &[
    ("TYPE_NAME", SYSNAME),
    ("DATA_TYPE", SMALLINT),
    ("PRECISION", INT),
    ("LITERAL_PREFIX", SqlType::Varchar(Some(32))),
    ("LITERAL_SUFFIX", SqlType::Varchar(Some(32))),
    ("CREATE_PARAMS", SqlType::Varchar(Some(32))),
    ("NULLABLE", SMALLINT),
    ("CASE_SENSITIVE", SMALLINT),
    ("SEARCHABLE", SMALLINT),
    ("UNSIGNED_ATTRIBUTE", SMALLINT),
    ("MONEY", SMALLINT),
    ("AUTO_INCREMENT", SMALLINT),
    ("LOCAL_TYPE_NAME", SYSNAME),
    ("MINIMUM_SCALE", SMALLINT),
    ("MAXIMUM_SCALE", SMALLINT),
    ("SQL_DATA_TYPE", SMALLINT),
    ("SQL_DATETIME_SUB", SMALLINT),
    ("NUM_PREC_RADIX", INT),
    ("INTERVAL_PRECISION", SMALLINT),
    ("USERTYPE", SMALLINT),
];

fn data_type_row(found: &DataTypeInfo, version: i64) -> Vec<Value> {
    let quotes = found.literal_quotes();
    let (sql_data_type, datetime_sub) = odbc_sql_type(found.name, version);
    let radix = match found.name {
        SqlServerTypeName::Real | SqlServerTypeName::Float => Some(2),
        name if name.is_exact_number() => Some(10),
        _ => None,
    };
    let scale = found.scale_range();
    vec![
        text(found.type_name),
        small(odbc_type(found.name, version)),
        int(found.precision),
        quotes.map_or(Value::Null, |(prefix, _)| text(prefix)),
        quotes
            .map(|(_, suffix)| suffix)
            .filter(|suffix| !suffix.is_empty())
            .map_or(Value::Null, text),
        found.create_params().map_or(Value::Null, text),
        small(1),
        small(i64::from(found.name.is_character())),
        small(found.searchable()),
        found
            .unsigned()
            .map_or(Value::Null, |unsigned| small(i64::from(unsigned))),
        small(i64::from(found.money())),
        found
            .auto_increment()
            .map_or(Value::Null, |auto| small(i64::from(auto))),
        text(found.type_name),
        optional_small(scale.map(|(minimum, _)| minimum)),
        optional_small(scale.map(|(_, maximum)| maximum)),
        small(sql_data_type),
        optional_small(datetime_sub),
        optional_int(radix),
        Value::Null,
        small(0),
    ]
}

fn data_type_info(arguments: &Arguments) -> QueryResult {
    let version = arguments.number(1).unwrap_or(ODBC_2);
    let wanted = arguments.number(0).filter(|code| *code != 0);
    let mut rows: Vec<Vec<Value>> = DATA_TYPES
        .iter()
        .filter(|found| wanted.is_none_or(|code| odbc_type(found.name, version) == code))
        .map(|found| data_type_row(found, version))
        .collect();
    rows.sort_by(|left, right| match (left.get(1), right.get(1)) {
        (Some(left), Some(right)) => compare_values(left, right),
        _ => Ordering::Equal,
    });
    result(DATA_TYPE_COLUMNS, rows)
}

const STORED_PROCEDURE_COLUMNS: &[(&str, SqlType)] = &[
    ("PROCEDURE_QUALIFIER", SYSNAME),
    ("PROCEDURE_OWNER", SYSNAME),
    ("PROCEDURE_NAME", SqlType::Varchar(Some(134))),
    ("NUM_INPUT_PARAMS", INT),
    ("NUM_OUTPUT_PARAMS", INT),
    ("NUM_RESULT_SETS", INT),
    ("REMARKS", REMARKS),
    ("PROCEDURE_TYPE", SMALLINT),
];

const PROCEDURE_COLUMN_COLUMNS: &[(&str, SqlType)] = &[
    ("PROCEDURE_QUALIFIER", SYSNAME),
    ("PROCEDURE_OWNER", SYSNAME),
    ("PROCEDURE_NAME", SqlType::Varchar(Some(134))),
    ("COLUMN_NAME", SYSNAME),
    ("COLUMN_TYPE", SMALLINT),
    ("DATA_TYPE", SMALLINT),
    ("TYPE_NAME", SYSNAME),
    ("PRECISION", INT),
    ("LENGTH", INT),
    ("SCALE", SMALLINT),
    ("RADIX", SMALLINT),
    ("NULLABLE", SMALLINT),
    ("REMARKS", REMARKS),
    ("COLUMN_DEF", NVARCHAR_4000),
    ("SQL_DATA_TYPE", SMALLINT),
    ("SQL_DATETIME_SUB", SMALLINT),
    ("CHAR_OCTET_LENGTH", INT),
    ("ORDINAL_POSITION", INT),
    ("IS_NULLABLE", REMARKS),
    ("SS_DATA_TYPE", TINYINT),
];

const DATABASE_COLUMNS: &[(&str, SqlType)] = &[
    ("DATABASE_NAME", SYSNAME),
    ("DATABASE_SIZE", INT),
    ("REMARKS", REMARKS),
];

fn object_named<'a>(scope: &Scope<'a>, written: &str) -> SqlResult<&'a SqlServerObject> {
    let parts = super::split_multipart_name(written);
    let other_database = parts.as_ref().is_some_and(|parts| {
        parts.len() >= 3
            && parts.get(parts.len() - 3).is_some_and(|database| {
                !database.is_empty() && !database.eq_ignore_ascii_case(scope.database)
            })
    });
    let objects: &'a SqlServerObjects = scope.objects;
    let found = if other_database {
        None
    } else {
        objects.resolve_name(written)
    };
    found.ok_or_else(|| {
        procedure_error(
            15009,
            SqlState::UNDEFINED_TABLE,
            format!(
                "The object '{written}' does not exist in database '{}' or is invalid for this operation.",
                scope.database
            ),
        )
    })
}

const HELP_OBJECTS_COLUMNS: &[(&str, SqlType)] = &[
    ("Name", SYSNAME),
    ("Owner", SYSNAME),
    ("Object_type", SqlType::Varchar(Some(31))),
];

const HELP_OBJECT_COLUMNS: &[(&str, SqlType)] = &[
    ("Name", SYSNAME),
    ("Owner", SYSNAME),
    ("Type", SqlType::Varchar(Some(31))),
    ("Created_datetime", SqlType::Timestamp(Some(3))),
];

const HELP_COLUMN_COLUMNS: &[(&str, SqlType)] = &[
    ("Column_name", SYSNAME),
    ("Type", SYSNAME),
    ("Computed", SqlType::Varchar(Some(35))),
    ("Length", INT),
    ("Prec", SqlType::Bpchar(Some(5))),
    ("Scale", SqlType::Bpchar(Some(5))),
    ("Nullable", SqlType::Varchar(Some(35))),
    ("TrimTrailingBlanks", SqlType::Varchar(Some(35))),
    ("FixedLenNullInSource", SqlType::Varchar(Some(35))),
    ("Collation", SYSNAME),
];

const HELP_IDENTITY_COLUMNS: &[(&str, SqlType)] = &[
    ("Identity", SYSNAME),
    ("Seed", SqlType::Numeric(None)),
    ("Increment", SqlType::Numeric(None)),
    ("Not For Replication", INT),
];

const HELP_ROWGUID_COLUMNS: &[(&str, SqlType)] = &[("RowGuidCol", SYSNAME)];

const HELP_FILEGROUP_COLUMNS: &[(&str, SqlType)] = &[("Data_located_on_filegroup", SYSNAME)];

const HELP_INDEX_COLUMNS: &[(&str, SqlType)] = &[
    ("index_name", SYSNAME),
    ("index_description", SqlType::Varchar(Some(210))),
    ("index_keys", SqlType::Varchar(Some(2078))),
];

const HELP_CONSTRAINT_COLUMNS: &[(&str, SqlType)] = &[
    ("constraint_type", SqlType::Varchar(Some(146))),
    ("constraint_name", SYSNAME),
    ("delete_action", SqlType::Varchar(Some(9))),
    ("update_action", SqlType::Varchar(Some(9))),
    ("status_enabled", SqlType::Varchar(Some(8))),
    ("status_for_replication", SqlType::Varchar(Some(19))),
    ("constraint_keys", SqlType::Varchar(Some(2078))),
];

const HELP_REFERENCED_COLUMNS: &[(&str, SqlType)] = &[(
    "Table is referenced by foreign key",
    SqlType::Varchar(Some(516)),
)];

fn object_type_description(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Table => "user table",
        ObjectKind::View => "view",
        ObjectKind::Sequence => "sequence object",
        ObjectKind::PrimaryKey => "primary key cns",
        ObjectKind::Unique => "unique key cns",
        ObjectKind::ForeignKey => "foreign key cns",
        ObjectKind::Check => "check cns",
        ObjectKind::Default => "default (maybe cns)",
    }
}

fn padded(value: Option<i64>) -> Value {
    Value::Bpchar(match value {
        Some(number) => format!("{number:<5}"),
        None => "     ".to_owned(),
    })
}

fn help(scope: &Scope<'_>, arguments: &Arguments) -> SqlResult<Vec<QueryResult>> {
    let Some(written) = arguments.text(0) else {
        let mut rows: Vec<Vec<Value>> = scope
            .objects
            .objects
            .iter()
            .map(|object| {
                vec![
                    text(&object.name),
                    text(scope.schema(object)),
                    text(object_type_description(object.kind)),
                ]
            })
            .collect();
        sort_rows(&mut rows, &[2, 1, 0]);
        return Ok(vec![result(HELP_OBJECTS_COLUMNS, rows)]);
    };
    let object = object_named(scope, &written)?;
    let mut results = vec![result(
        HELP_OBJECT_COLUMNS,
        vec![vec![
            text(&object.name),
            text(scope.schema(object)),
            text(object_type_description(object.kind)),
            UNKNOWN_DATE,
        ]],
    )];
    if !matches!(object.kind, ObjectKind::Table | ObjectKind::View) {
        return Ok(results);
    }
    let columns = scope.columns(object);
    let column_rows = columns
        .iter()
        .map(|column| {
            let column_type = column.column_type;
            let character = column_type.name.is_character();
            let precision = (!character && column_type.numeric_precision().is_some())
                .then_some(column_type.precision);
            let scale = column_type.numeric_scale();
            vec![
                text(column.name),
                text(column_type.name.name()),
                text("no"),
                int(column_type.max_length()),
                padded(precision),
                padded(scale),
                text(if column.nullable { "yes" } else { "no" }),
                text(if character { "no" } else { "(n/a)" }),
                text(if character { "yes" } else { "(n/a)" }),
                if character {
                    text(super::COLLATION)
                } else {
                    Value::Null
                },
            ]
        })
        .collect();
    results.push(result(HELP_COLUMN_COLUMNS, column_rows));
    let identity = columns.iter().find(|column| column.identity).map_or_else(
        || {
            vec![
                text("No identity column defined."),
                Value::Null,
                Value::Null,
                Value::Null,
            ]
        },
        |column| {
            vec![
                text(column.name),
                Value::Numeric(crate::numeric::Numeric::from_i64(column.identity_seed.seed)),
                Value::Numeric(crate::numeric::Numeric::from_i64(
                    column.identity_seed.increment,
                )),
                int(0),
            ]
        },
    );
    results.push(result(HELP_IDENTITY_COLUMNS, vec![identity]));
    results.push(result(
        HELP_ROWGUID_COLUMNS,
        vec![vec![text("No rowguidcol column defined.")]],
    ));
    if object.kind == ObjectKind::View {
        return Ok(results);
    }
    results.push(result(HELP_FILEGROUP_COLUMNS, vec![vec![text("PRIMARY")]]));
    let indexes = index_rows(scope, object);
    if !indexes.is_empty() {
        results.push(result(HELP_INDEX_COLUMNS, indexes));
    }
    let constraints = constraint_rows(scope, object);
    if !constraints.is_empty() {
        results.push(result(HELP_CONSTRAINT_COLUMNS, constraints));
    }
    let referenced = referencing_rows(scope, object);
    if !referenced.is_empty() {
        results.push(result(HELP_REFERENCED_COLUMNS, referenced));
    }
    Ok(results)
}

fn index_rows(scope: &Scope<'_>, object: &SqlServerObject) -> Vec<Vec<Value>> {
    let Some(table) = scope
        .snapshot
        .relations
        .get(object.relation)
        .and_then(|entry| entry.table())
    else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    for index in scope.objects.indexes(scope.snapshot, object.relation) {
        let Some(source) = index.source else {
            continue;
        };
        let Some(name) = scope
            .objects
            .index_name(scope.snapshot, object.relation, &index)
        else {
            continue;
        };
        let unique = index_unique(scope, object, &index);
        let mut description = if index.is_clustered() {
            "clustered".to_owned()
        } else {
            "nonclustered".to_owned()
        };
        if unique {
            description.push_str(", unique");
        }
        if source == IndexSource::Primary {
            description.push_str(", primary key");
        } else if unique {
            description.push_str(", unique key");
        }
        description.push_str(" located on PRIMARY");
        let keys: Vec<&str> = index_columns(table, source)
            .iter()
            .filter_map(|column| table.columns.get(*column))
            .map(|column| column.name.as_str())
            .collect();
        rows.push(vec![
            text(&name),
            text(&description),
            text(&keys.join(", ")),
        ]);
    }
    sort_rows(&mut rows, &[0]);
    rows
}

fn action_name(action: crate::ast::ReferentialAction) -> &'static str {
    match action {
        crate::ast::ReferentialAction::NoAction | crate::ast::ReferentialAction::Restrict => {
            "No Action"
        }
        crate::ast::ReferentialAction::Cascade => "Cascade",
        crate::ast::ReferentialAction::SetNull => "Set Null",
        crate::ast::ReferentialAction::SetDefault => "Set Default",
    }
}

fn not_applicable_row(kind: String, name: &str, keys: String) -> Vec<Value> {
    vec![
        text(&kind),
        text(name),
        text("(n/a)"),
        text("(n/a)"),
        text("(n/a)"),
        text("(n/a)"),
        text(&keys),
    ]
}

fn constraint_rows(scope: &Scope<'_>, object: &SqlServerObject) -> Vec<Vec<Value>> {
    let Some(table) = scope
        .snapshot
        .relations
        .get(object.relation)
        .and_then(|entry| entry.table())
    else {
        return Vec::new();
    };
    let column_names = |columns: &[usize]| {
        columns
            .iter()
            .filter_map(|column| table.columns.get(*column))
            .map(|column| column.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut rows = Vec::new();
    for constraint in scope
        .objects
        .objects
        .iter()
        .filter(|found| found.parent_object_id == object.object_id)
    {
        match constraint.source {
            ObjectSource::Default(column) => {
                let Some(column_name) = table.columns.get(column).map(|found| found.name.clone())
                else {
                    continue;
                };
                rows.push(not_applicable_row(
                    format!("DEFAULT on column {column_name}"),
                    &constraint.name,
                    scope.default_of(object, column).unwrap_or_default(),
                ));
            }
            ObjectSource::Constraint(at) => {
                let Some(source) = scope.snapshot.constraints.get(at).map(|found| found.source)
                else {
                    continue;
                };
                match source {
                    ConstraintSource::Primary => rows.push(not_applicable_row(
                        "PRIMARY KEY (clustered)".to_owned(),
                        &constraint.name,
                        column_names(&table.primary_key),
                    )),
                    ConstraintSource::Unique(position) => rows.push(not_applicable_row(
                        "UNIQUE (non-clustered)".to_owned(),
                        &constraint.name,
                        column_names(index_columns(table, IndexSource::Secondary(position))),
                    )),
                    ConstraintSource::Check(position) => {
                        let used = crate::information_schema::check_columns(table, position);
                        let kind = match used.as_slice() {
                            [only] => format!(
                                "CHECK on column {}",
                                table
                                    .columns
                                    .get(*only)
                                    .map_or("", |found| found.name.as_str())
                            ),
                            _ => "CHECK Table Level ".to_owned(),
                        };
                        let definition = table
                            .checks
                            .get(position)
                            .map(|check| super::check_definition(&check.expression))
                            .unwrap_or_default();
                        rows.push(vec![
                            text(&kind),
                            text(&constraint.name),
                            text("(n/a)"),
                            text("(n/a)"),
                            text("Enabled"),
                            text("Is_For_Replication"),
                            text(&definition),
                        ]);
                    }
                    ConstraintSource::Foreign(position) => {
                        let Some(facts) =
                            foreign_key_facts(scope.snapshot, object.relation, position)
                        else {
                            continue;
                        };
                        rows.push(vec![
                            text("FOREIGN KEY"),
                            text(&constraint.name),
                            text(action_name(facts.key.on_delete)),
                            text(action_name(facts.key.on_update)),
                            text("Enabled"),
                            text("Is_For_Replication"),
                            text(&column_names(&facts.key.columns)),
                        ]);
                        let parent = facts
                            .referenced_relation
                            .and_then(|relation| scope.objects.of_relation(relation));
                        let reference = match parent {
                            Some(parent) => format!(
                                "REFERENCES {}.{}.{} ({})",
                                scope.database,
                                scope.schema(parent),
                                parent.name,
                                facts.key.parent_columns.join(", ")
                            ),
                            None => format!("REFERENCES {}", facts.key.parent),
                        };
                        rows.push(vec![
                            text(" "),
                            text(" "),
                            text(" "),
                            text(" "),
                            text(" "),
                            text(" "),
                            text(&reference),
                        ]);
                    }
                }
            }
            ObjectSource::Relation => {}
        }
    }
    rows
}

fn referencing_rows(scope: &Scope<'_>, object: &SqlServerObject) -> Vec<Vec<Value>> {
    let mut rows = Vec::new();
    for constraint in scope
        .objects
        .objects
        .iter()
        .filter(|found| found.kind == ObjectKind::ForeignKey)
    {
        let ObjectSource::Constraint(at) = constraint.source else {
            continue;
        };
        let Some(ConstraintSource::Foreign(key_at)) =
            scope.snapshot.constraints.get(at).map(|found| found.source)
        else {
            continue;
        };
        let Some(facts) = foreign_key_facts(scope.snapshot, constraint.relation, key_at) else {
            continue;
        };
        if facts.referenced_relation != Some(object.relation) {
            continue;
        }
        let Some(child) = scope.objects.of_relation(constraint.relation) else {
            continue;
        };
        rows.push(vec![text(&format!(
            "{}.{}.{}: {}",
            scope.database,
            scope.schema(child),
            child.name,
            constraint.name
        ))]);
    }
    sort_rows(&mut rows, &[0]);
    rows
}

fn help_index(scope: &Scope<'_>, arguments: &Arguments) -> SqlResult<QueryResult> {
    let written = arguments.text(0).unwrap_or_default();
    let object = object_named(scope, &written)?;
    Ok(result(HELP_INDEX_COLUMNS, index_rows(scope, object)))
}

const COLUMNS_MANAGED_COLUMNS: &[(&str, SqlType)] = &[
    ("TABLE_CATALOG", SYSNAME),
    ("TABLE_SCHEMA", SYSNAME),
    ("TABLE_NAME", SYSNAME),
    ("COLUMN_NAME", SYSNAME),
    ("ORDINAL_POSITION", INT),
    ("COLUMN_DEFAULT", NVARCHAR_4000),
    ("IS_NULLABLE", SqlType::Varchar(Some(3))),
    ("DATA_TYPE", NVARCHAR_128),
    ("CHARACTER_MAXIMUM_LENGTH", INT),
    ("CHARACTER_OCTET_LENGTH", INT),
    ("NUMERIC_PRECISION", TINYINT),
    ("NUMERIC_PRECISION_RADIX", SMALLINT),
    ("NUMERIC_SCALE", INT),
    ("DATETIME_PRECISION", SMALLINT),
    ("CHARACTER_SET_CATALOG", SYSNAME),
    ("CHARACTER_SET_SCHEMA", SYSNAME),
    ("CHARACTER_SET_NAME", SYSNAME),
    ("COLLATION_CATALOG", SYSNAME),
    ("IS_FILESTREAM", SqlType::Varchar(Some(3))),
    ("IS_SPARSE", SqlType::Varchar(Some(3))),
    ("IS_COLUMN_SET", SqlType::Varchar(Some(3))),
];

const MANAGED_SHAPE: &[crate::pg_catalog::CatalogColumn] = &[
    super::column("data_type", SYSNAME),
    super::column("character_maximum_length", INT),
    super::column("character_octet_length", INT),
    super::column("numeric_precision", TINYINT),
    super::column("numeric_precision_radix", SMALLINT),
    super::column("numeric_scale", INT),
    super::column("datetime_precision", SMALLINT),
    super::column("character_set_name", SYSNAME),
    super::column("collation_name", SYSNAME),
];

fn type_values(column_type: SqlServerColumnType) -> Vec<Value> {
    column_values(super::CatalogRow::new(MANAGED_SHAPE), column_type).done()
}

fn columns_managed(scope: &Scope<'_>, arguments: &Arguments) -> QueryResult {
    if !scope.qualifier_matches(arguments.text(0).as_ref(), false) {
        return result(COLUMNS_MANAGED_COLUMNS, Vec::new());
    }
    let found = matching_columns(
        scope,
        &NameFilter::exact(arguments.text(1)),
        &NameFilter::exact(arguments.text(2)),
        &NameFilter::exact(arguments.text(3)),
    );
    let rows = found
        .iter()
        .map(|described| {
            let column = &described.column;
            let typed = type_values(column.column_type);
            let typed_value = |at: usize| typed.get(at).cloned().unwrap_or(Value::Null);
            vec![
                text(scope.database),
                text(scope.schema(described.object)),
                text(&described.object.name),
                text(column.name),
                int(described.position as i64 + 1),
                scope
                    .default_of(described.object, described.position)
                    .map_or(Value::Null, Value::Text),
                yes_or_no(column.nullable),
                typed_value(0),
                typed_value(1),
                typed_value(2),
                typed_value(3),
                typed_value(4),
                typed_value(5),
                typed_value(6),
                Value::Null,
                Value::Null,
                typed_value(7),
                Value::Null,
                yes_or_no(false),
                yes_or_no(false),
                yes_or_no(false),
            ]
        })
        .collect();
    result(COLUMNS_MANAGED_COLUMNS, rows)
}

const INDEXES_MANAGED_COLUMNS: &[(&str, SqlType)] = &[
    ("constraint_catalog", SYSNAME),
    ("constraint_schema", SYSNAME),
    ("constraint_name", SYSNAME),
    ("table_catalog", SYSNAME),
    ("table_schema", SYSNAME),
    ("table_name", SYSNAME),
    ("index_name", SYSNAME),
    ("type_desc", SqlType::Varchar(Some(60))),
];

struct NamedIndex<'a> {
    object: &'a SqlServerObject,
    index: SqlServerIndex,
    name: String,
}

fn named_indexes<'a>(
    scope: &Scope<'a>,
    owner: &NameFilter,
    table: &NameFilter,
    index_name: &NameFilter,
) -> Vec<NamedIndex<'a>> {
    let mut found = Vec::new();
    for object in scope.relations(&[ObjectKind::Table], owner, table) {
        let mut indexes: Vec<NamedIndex<'a>> = scope
            .objects
            .indexes(scope.snapshot, object.relation)
            .into_iter()
            .filter_map(|index| {
                let name = scope
                    .objects
                    .index_name(scope.snapshot, object.relation, &index)?;
                index_name.matches(&name).then_some(NamedIndex {
                    object,
                    index,
                    name,
                })
            })
            .collect();
        indexes.sort_by(|left, right| left.name.cmp(&right.name));
        found.extend(indexes);
    }
    found
}

fn located_index(scope: &Scope<'_>, named: &NamedIndex<'_>) -> Vec<Value> {
    vec![
        text(scope.database),
        text(scope.schema(named.object)),
        text(&named.name),
        text(scope.database),
        text(scope.schema(named.object)),
        text(&named.object.name),
    ]
}

fn indexes_managed(scope: &Scope<'_>, arguments: &Arguments) -> QueryResult {
    if !scope.qualifier_matches(arguments.text(0).as_ref(), false) {
        return result(INDEXES_MANAGED_COLUMNS, Vec::new());
    }
    let rows = named_indexes(
        scope,
        &NameFilter::exact(arguments.text(1)),
        &NameFilter::exact(arguments.text(2)),
        &NameFilter::exact(arguments.text(3)),
    )
    .iter()
    .map(|named| {
        let mut row = located_index(scope, named);
        row.push(text(&named.name));
        row.push(text(if named.index.is_clustered() {
            "CLUSTERED"
        } else {
            "NONCLUSTERED"
        }));
        row
    })
    .collect();
    result(INDEXES_MANAGED_COLUMNS, rows)
}

const INDEX_COLUMNS_MANAGED_COLUMNS: &[(&str, SqlType)] = &[
    ("constraint_catalog", SYSNAME),
    ("constraint_schema", SYSNAME),
    ("constraint_name", SYSNAME),
    ("table_catalog", SYSNAME),
    ("table_schema", SYSNAME),
    ("table_name", SYSNAME),
    ("column_name", SYSNAME),
    ("ordinal_position", INT),
    ("KeyType", TINYINT),
    ("index_name", SYSNAME),
];

fn index_columns_managed(scope: &Scope<'_>, arguments: &Arguments) -> QueryResult {
    if !scope.qualifier_matches(arguments.text(0).as_ref(), false) {
        return result(INDEX_COLUMNS_MANAGED_COLUMNS, Vec::new());
    }
    let column_filter = NameFilter::exact(arguments.text(4));
    let mut rows = Vec::new();
    for named in named_indexes(
        scope,
        &NameFilter::exact(arguments.text(1)),
        &NameFilter::exact(arguments.text(2)),
        &NameFilter::exact(arguments.text(3)),
    ) {
        let Some(source) = named.index.source else {
            continue;
        };
        let Some(table) = scope
            .snapshot
            .relations
            .get(named.object.relation)
            .and_then(|entry| entry.table())
        else {
            continue;
        };
        let columns = scope.columns(named.object);
        for (ordinal, column) in index_columns(table, source).iter().enumerate() {
            let Some(facts) = columns.get(*column) else {
                continue;
            };
            if !column_filter.matches(facts.name) {
                continue;
            }
            let mut row = located_index(scope, &named);
            row.extend([
                text(facts.name),
                int(ordinal as i64 + 1),
                small(facts.column_type.system_type_id()),
                text(&named.name),
            ]);
            rows.push(row);
        }
    }
    result(INDEX_COLUMNS_MANAGED_COLUMNS, rows)
}

const TDS_COLLATION: [u8; 5] = [0x09, 0x04, 0x00, 0x22, 0x00];

const TABLE_COLLATION_COLUMNS: &[(&str, SqlType)] = &[
    ("colid", INT),
    ("name", SYSNAME),
    ("tds_collation_100", SqlType::Bytea),
    ("collation_100", SYSNAME),
];

fn table_collations(scope: &Scope<'_>, arguments: &Arguments) -> QueryResult {
    let Some(object) = arguments
        .text(0)
        .and_then(|written| scope.objects.resolve_name(&written))
        .filter(|object| matches!(object.kind, ObjectKind::Table | ObjectKind::View))
    else {
        return result(TABLE_COLLATION_COLUMNS, Vec::new());
    };
    let rows = scope
        .columns(object)
        .iter()
        .enumerate()
        .map(|(position, column)| {
            let character = column.column_type.name.is_character();
            vec![
                int(position as i64 + 1),
                text(column.name),
                if character {
                    Value::Bytea(TDS_COLLATION.to_vec())
                } else {
                    Value::Null
                },
                if character {
                    text(super::COLLATION)
                } else {
                    Value::Null
                },
            ]
        })
        .collect();
    result(TABLE_COLLATION_COLUMNS, rows)
}
