use super::super::routines::{
    ParameterFacts, module_definition, parameter_facts, routine_definition, scalar_return_type,
};
use super::super::types::SqlServerTypeName;
use super::{
    Arguments, INT, NVARCHAR_128, NVARCHAR_4000, NameFilter, ODBC_2, PROCEDURE_COLUMN_COLUMNS,
    STORED_PROCEDURE_COLUMNS, SYSNAME, SMALLINT, Scope, int, object_named, odbc_sql_type,
    odbc_type, optional_int, optional_small, procedure_error, result, small, sort_rows, text,
    yes_or_no,
};
use crate::error::{SqlResult, SqlState};
use crate::routines::RoutineDefinition;
use crate::session::QueryResult;
use crate::tsql::TsqlReturns;
use crate::types::{SqlType, Value};
use super::super::{ObjectKind, SqlServerColumnType, SqlServerObject};

const SQL_PT_PROCEDURE: i64 = 2;
const SQL_PT_FUNCTION: i64 = 3;
const SQL_PARAM_INPUT: i64 = 1;
const SQL_PARAM_INPUT_OUTPUT: i64 = 2;
const SQL_RESULT_COL: i64 = 3;
const SQL_RETURN_VALUE: i64 = 5;
const DIRECTION_INPUT: i64 = 1;
const DIRECTION_INPUT_OUTPUT: i64 = 2;
const DIRECTION_RETURN_VALUE: i64 = 4;
const HELP_TEXT_LINE: usize = 255;
const RETURN_VALUE_NAME: &str = "@RETURN_VALUE";

const HELP_TEXT_COLUMNS: &[(&str, SqlType)] = &[("Text", SqlType::Varchar(Some(255)))];

const HELP_PARAMETER_COLUMNS: &[(&str, SqlType)] = &[
    ("Parameter_name", SYSNAME),
    ("Type", SYSNAME),
    ("Length", SMALLINT),
    ("Prec", INT),
    ("Scale", INT),
    ("Param_order", INT),
    ("Collation", SYSNAME),
];

const PROCEDURE_PARAMETER_COLUMNS: &[(&str, SqlType)] = &[
    ("PROCEDURE_CATALOG", SYSNAME),
    ("PROCEDURE_SCHEMA", SYSNAME),
    ("PROCEDURE_NAME", NVARCHAR_128),
    ("PARAMETER_NAME", SYSNAME),
    ("ORDINAL_POSITION", INT),
    ("PARAMETER_TYPE", SMALLINT),
    ("PARAMETER_HASDEFAULT", SqlType::Boolean),
    ("PARAMETER_DEFAULT", NVARCHAR_4000),
    ("IS_NULLABLE", SqlType::Boolean),
    ("DATA_TYPE", SMALLINT),
    ("CHARACTER_MAXIMUM_LENGTH", INT),
    ("CHARACTER_OCTET_LENGTH", INT),
    ("NUMERIC_PRECISION", SMALLINT),
    ("NUMERIC_SCALE", SMALLINT),
    ("DESCRIPTION", NVARCHAR_128),
    ("TYPE_CATALOG_NAME", SYSNAME),
    ("TYPE_SCHEMA_NAME", SYSNAME),
    ("TYPE_NAME", SYSNAME),
    ("LOCAL_TYPE_NAME", SYSNAME),
    ("SS_DATETIME_PRECISION", SMALLINT),
    ("XML_CATALOGNAME", SYSNAME),
    ("XML_SCHEMANAME", SYSNAME),
    ("XML_SCHEMACOLLECTIONNAME", SYSNAME),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ParameterTypeCodes {
    SqlDbType,
    OleDb,
}

struct DescribedParameter<'a> {
    name: &'a str,
    column_type: SqlServerColumnType,
    odbc_kind: i64,
    direction: i64,
    ordinal: i64,
}

impl<'a> Scope<'a> {
    fn routines(&self, owner: &NameFilter, name: &NameFilter) -> Vec<&'a SqlServerObject> {
        let objects = self.objects;
        let mut found: Vec<&'a SqlServerObject> = objects
            .routines()
            .filter(|object| owner.matches(objects.schema_name(object.schema_id)))
            .filter(|object| name.matches(&object.name))
            .collect();
        found.sort_by(|left, right| {
            (objects.schema_name(left.schema_id), left.name.as_str())
                .cmp(&(objects.schema_name(right.schema_id), right.name.as_str()))
        });
        found
    }

    fn routine(&self, object: &SqlServerObject) -> Option<&'a RoutineDefinition> {
        routine_definition(self.snapshot, object)
    }
}

fn numbered_name(object: &SqlServerObject) -> String {
    let number = if object.kind == ObjectKind::Procedure {
        1
    } else {
        0
    };
    format!("{};{number}", object.name)
}

fn described_parameters(definition: &RoutineDefinition) -> Vec<DescribedParameter<'_>> {
    let mut described = Vec::with_capacity(definition.parameters.len() + 1);
    let returns = definition.tsql.as_deref().map(|tsql| &tsql.returns);
    match returns {
        Some(TsqlReturns::Status) => described.push(DescribedParameter {
            name: RETURN_VALUE_NAME,
            column_type: SqlServerColumnType::of(SqlType::Integer),
            odbc_kind: SQL_RETURN_VALUE,
            direction: DIRECTION_RETURN_VALUE,
            ordinal: 0,
        }),
        Some(TsqlReturns::Scalar(_)) => {
            if let Some(column_type) = scalar_return_type(definition) {
                described.push(DescribedParameter {
                    name: RETURN_VALUE_NAME,
                    column_type,
                    odbc_kind: SQL_RETURN_VALUE,
                    direction: DIRECTION_RETURN_VALUE,
                    ordinal: 0,
                });
            }
        }
        Some(TsqlReturns::InlineTable | TsqlReturns::Table { .. }) | None => {}
    }
    for (position, facts) in parameter_facts(definition).into_iter().enumerate() {
        let ParameterFacts {
            name,
            column_type,
            output,
            ..
        } = facts;
        described.push(DescribedParameter {
            name,
            column_type,
            odbc_kind: if output {
                SQL_PARAM_INPUT_OUTPUT
            } else {
                SQL_PARAM_INPUT
            },
            direction: if output {
                DIRECTION_INPUT_OUTPUT
            } else {
                DIRECTION_INPUT
            },
            ordinal: i64::try_from(position).map_or(i64::MAX, |at| at + 1),
        });
    }
    described
}

pub(super) fn stored_procedures(scope: &Scope<'_>, arguments: &Arguments) -> QueryResult {
    let use_pattern = arguments.flag(3, true);
    if !scope.qualifier_matches(arguments.text(2).as_ref(), false) {
        return result(STORED_PROCEDURE_COLUMNS, Vec::new());
    }
    let owner = NameFilter::new(arguments.text(1), use_pattern);
    let name = NameFilter::new(arguments.text(0), use_pattern);
    let mut rows: Vec<Vec<Value>> = scope
        .routines(&owner, &name)
        .into_iter()
        .map(|object| {
            let kind = if object.kind == ObjectKind::Procedure {
                SQL_PT_PROCEDURE
            } else {
                SQL_PT_FUNCTION
            };
            vec![
                text(scope.database),
                text(scope.schema(object)),
                text(&numbered_name(object)),
                int(-1),
                int(-1),
                int(-1),
                Value::Null,
                small(kind),
            ]
        })
        .collect();
    sort_rows(&mut rows, &[0, 1, 2]);
    result(STORED_PROCEDURE_COLUMNS, rows)
}

pub(super) fn procedure_columns(scope: &Scope<'_>, arguments: &Arguments) -> QueryResult {
    let version = arguments.number(4).unwrap_or(ODBC_2);
    let use_pattern = arguments.flag(5, true);
    if !scope.qualifier_matches(arguments.text(2).as_ref(), false) {
        return result(PROCEDURE_COLUMN_COLUMNS, Vec::new());
    }
    let owner = NameFilter::new(arguments.text(1), use_pattern);
    let name = NameFilter::new(arguments.text(0), use_pattern);
    let column = NameFilter::new(arguments.text(3), use_pattern);
    let mut rows = Vec::new();
    for object in scope.routines(&owner, &name) {
        let Some(definition) = scope.routine(object) else {
            continue;
        };
        let mut described = described_parameters(definition);
        if let Some(TsqlReturns::Table { columns, .. }) =
            definition.tsql.as_deref().map(|tsql| &tsql.returns)
        {
            for (position, (name, sql_type)) in columns.iter().enumerate() {
                described.push(DescribedParameter {
                    name: name.as_str(),
                    column_type: SqlServerColumnType::of(*sql_type),
                    odbc_kind: SQL_RESULT_COL,
                    direction: DIRECTION_INPUT,
                    ordinal: i64::try_from(position).map_or(i64::MAX, |at| at + 1),
                });
            }
        }
        for parameter in described {
            if !column.matches(parameter.name) {
                continue;
            }
            let column_type = parameter.column_type;
            let (sql_data_type, datetime_sub) = odbc_sql_type(column_type.name, version);
            rows.push(vec![
                text(scope.database),
                text(scope.schema(object)),
                text(&numbered_name(object)),
                text(parameter.name),
                small(parameter.odbc_kind),
                small(odbc_type(column_type.name, version)),
                text(column_type.name.name()),
                int(column_type.odbc_precision()),
                int(column_type.odbc_length()),
                optional_small(column_type.odbc_scale()),
                optional_small(column_type.odbc_radix()),
                small(1),
                Value::Null,
                Value::Null,
                small(sql_data_type),
                optional_small(datetime_sub),
                optional_int(column_type.odbc_octet_length()),
                int(parameter.ordinal),
                yes_or_no(true),
                small(column_type.tds_type(true)),
            ]);
        }
    }
    result(PROCEDURE_COLUMN_COLUMNS, rows)
}

pub(super) fn help_text(scope: &Scope<'_>, arguments: &Arguments) -> SqlResult<QueryResult> {
    let written = arguments.text(0).unwrap_or_default();
    let object = object_named(scope, &written)?;
    let Some(definition) = module_definition(scope.snapshot, scope.objects, object) else {
        return Err(procedure_error(
            15197,
            SqlState::UNDEFINED_OBJECT,
            format!("There is no text for object '{written}'."),
        ));
    };
    let rows = text_lines(&definition)
        .into_iter()
        .map(|line| vec![Value::Text(line)])
        .collect();
    Ok(result(HELP_TEXT_COLUMNS, rows))
}

fn text_lines(definition: &str) -> Vec<String> {
    let mut lines = Vec::new();
    for line in definition.split_inclusive('\n') {
        let characters: Vec<char> = line.chars().collect();
        for chunk in characters.chunks(HELP_TEXT_LINE) {
            lines.push(chunk.iter().collect());
        }
    }
    lines
}

pub(super) fn help_parameters(scope: &Scope<'_>, object: &SqlServerObject) -> Option<QueryResult> {
    let definition = scope.routine(object)?;
    let rows: Vec<Vec<Value>> = parameter_facts(definition)
        .into_iter()
        .enumerate()
        .map(|(position, facts)| {
            let column_type = facts.column_type;
            let collated = column_type.name.is_character();
            vec![
                text(facts.name),
                text(column_type.name.name()),
                small(column_type.max_length()),
                int(column_type.odbc_precision()),
                optional_int(column_type.odbc_scale()),
                int(i64::try_from(position).map_or(i64::MAX, |at| at + 1)),
                if collated {
                    text(super::super::COLLATION)
                } else {
                    Value::Null
                },
            ]
        })
        .collect();
    if rows.is_empty() {
        return None;
    }
    Some(result(HELP_PARAMETER_COLUMNS, rows))
}

pub(super) fn procedure_parameters(
    scope: &Scope<'_>,
    arguments: &Arguments,
    codes: ParameterTypeCodes,
) -> QueryResult {
    let name = NameFilter::exact(arguments.text(0));
    let owner = NameFilter::exact(arguments.text(2));
    let parameter = NameFilter::exact(arguments.text(3));
    let mut rows = Vec::new();
    for object in scope.routines(&owner, &name) {
        let Some(definition) = scope.routine(object) else {
            continue;
        };
        let defaults: Vec<Option<&str>> = std::iter::once(None)
            .chain(parameter_facts(definition).into_iter().map(|facts| facts.default))
            .collect();
        let shown_name = match codes {
            ParameterTypeCodes::SqlDbType => object.name.clone(),
            ParameterTypeCodes::OleDb => numbered_name(object),
        };
        for described in described_parameters(definition) {
            if !parameter.matches(described.name) {
                continue;
            }
            let column_type = described.column_type;
            let default = usize::try_from(described.ordinal)
                .ok()
                .and_then(|at| defaults.get(at).copied().flatten());
            let data_type = match codes {
                ParameterTypeCodes::SqlDbType => sql_db_type(column_type.name),
                ParameterTypeCodes::OleDb => ole_db_type(column_type.name),
            };
            let datetime_scale = matches!(
                column_type.name,
                SqlServerTypeName::Time
                    | SqlServerTypeName::DateTime2
                    | SqlServerTypeName::DateTimeOffset
            )
            .then_some(column_type.scale);
            rows.push(vec![
                text(scope.database),
                text(scope.schema(object)),
                text(&shown_name),
                text(described.name),
                int(described.ordinal),
                small(described.direction),
                Value::Boolean(default.is_some()),
                default.map_or(Value::Null, text),
                Value::Boolean(true),
                small(data_type),
                optional_int(column_type.character_length().or_else(|| {
                    matches!(
                        column_type.name,
                        SqlServerTypeName::VarBinary | SqlServerTypeName::Binary
                    )
                    .then(|| column_type.octet_length().unwrap_or(-1))
                })),
                optional_int(column_type.octet_length()),
                optional_small(column_type.numeric_precision()),
                optional_small(column_type.numeric_scale()),
                Value::Null,
                Value::Null,
                Value::Null,
                text(column_type.name.name()),
                text(column_type.name.name()),
                optional_small(datetime_scale),
                Value::Null,
                Value::Null,
                Value::Null,
            ]);
        }
    }
    result(PROCEDURE_PARAMETER_COLUMNS, rows)
}

fn sql_db_type(name: SqlServerTypeName) -> i64 {
    match name {
        SqlServerTypeName::BigInt => 0,
        SqlServerTypeName::Binary => 1,
        SqlServerTypeName::Bit => 2,
        SqlServerTypeName::Char => 3,
        SqlServerTypeName::DateTime => 4,
        SqlServerTypeName::Decimal | SqlServerTypeName::Numeric => 5,
        SqlServerTypeName::Float => 6,
        SqlServerTypeName::Image => 7,
        SqlServerTypeName::Int => 8,
        SqlServerTypeName::Money => 9,
        SqlServerTypeName::NChar => 10,
        SqlServerTypeName::NText => 11,
        SqlServerTypeName::NVarChar | SqlServerTypeName::SysName => 12,
        SqlServerTypeName::Real => 13,
        SqlServerTypeName::UniqueIdentifier => 14,
        SqlServerTypeName::SmallDateTime => 15,
        SqlServerTypeName::SmallInt => 16,
        SqlServerTypeName::SmallMoney => 17,
        SqlServerTypeName::Text => 18,
        SqlServerTypeName::TinyInt => 20,
        SqlServerTypeName::VarBinary => 21,
        SqlServerTypeName::VarChar => 22,
        SqlServerTypeName::SqlVariant => 23,
        SqlServerTypeName::Xml => 25,
        SqlServerTypeName::Date => 31,
        SqlServerTypeName::Time => 32,
        SqlServerTypeName::DateTime2 => 33,
        SqlServerTypeName::DateTimeOffset => 34,
    }
}

fn ole_db_type(name: SqlServerTypeName) -> i64 {
    match name {
        SqlServerTypeName::SmallInt => 2,
        SqlServerTypeName::Int => 3,
        SqlServerTypeName::Real => 4,
        SqlServerTypeName::Float => 5,
        SqlServerTypeName::Money | SqlServerTypeName::SmallMoney => 6,
        SqlServerTypeName::Bit => 11,
        SqlServerTypeName::SqlVariant => 12,
        SqlServerTypeName::TinyInt => 17,
        SqlServerTypeName::BigInt => 20,
        SqlServerTypeName::UniqueIdentifier => 72,
        SqlServerTypeName::Binary | SqlServerTypeName::VarBinary | SqlServerTypeName::Image => 128,
        SqlServerTypeName::Char | SqlServerTypeName::VarChar | SqlServerTypeName::Text => 129,
        SqlServerTypeName::NChar
        | SqlServerTypeName::NVarChar
        | SqlServerTypeName::NText
        | SqlServerTypeName::SysName => 130,
        SqlServerTypeName::Decimal | SqlServerTypeName::Numeric => 131,
        SqlServerTypeName::Date => 133,
        SqlServerTypeName::DateTime
        | SqlServerTypeName::SmallDateTime
        | SqlServerTypeName::DateTime2 => 135,
        SqlServerTypeName::Xml => 141,
        SqlServerTypeName::Time => 145,
        SqlServerTypeName::DateTimeOffset => 146,
    }
}
