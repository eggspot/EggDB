use crate::error::{SqlError, SqlState};
use crate::types::{NumericModifier, SqlType, Value};

pub(crate) const MAX_DECIMAL_PRECISION: u8 = 38;
pub(crate) const MAX_TIME_SCALE: u8 = 7;
pub(crate) const STORED_TIME_SCALE: u8 = 6;
pub(crate) const UNSPECIFIED_CAST_LENGTH: u32 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Length {
    Given(u32),
    Max,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SqlServerType {
    TinyInt,
    SmallInt,
    Int,
    BigInt,
    Bit,
    Decimal { precision: u8, scale: u8 },
    Money,
    SmallMoney,
    Float,
    Real,
    Date,
    Time { scale: u8 },
    DateTime,
    SmallDateTime,
    DateTime2 { scale: u8 },
    DateTimeOffset { scale: u8 },
    Char(Length),
    VarChar(Length),
    NChar(Length),
    NVarChar(Length),
    Text,
    NText,
    Binary(Length),
    VarBinary(Length),
    Image,
    UniqueIdentifier,
}

fn stored_scale(scale: u8) -> Option<u8> {
    Some(scale.min(STORED_TIME_SCALE))
}

impl SqlServerType {
    pub(crate) fn engine_type(self) -> SqlType {
        match self {
            Self::TinyInt | Self::SmallInt => SqlType::SmallInt,
            Self::Int => SqlType::Integer,
            Self::BigInt => SqlType::BigInt,
            Self::Bit => SqlType::Boolean,
            Self::Decimal { precision, scale } => SqlType::Numeric(Some(NumericModifier {
                precision: u16::from(precision),
                scale: i16::from(scale),
            })),
            Self::Money => SqlType::Numeric(Some(NumericModifier {
                precision: 19,
                scale: 4,
            })),
            Self::SmallMoney => SqlType::Numeric(Some(NumericModifier {
                precision: 10,
                scale: 4,
            })),
            Self::Float => SqlType::Double,
            Self::Real => SqlType::Real,
            Self::Date => SqlType::Date,
            Self::Time { scale } => SqlType::Time(stored_scale(scale)),
            Self::DateTime => SqlType::Timestamp(Some(3)),
            Self::SmallDateTime => SqlType::Timestamp(Some(0)),
            Self::DateTime2 { scale } => SqlType::Timestamp(stored_scale(scale)),
            Self::DateTimeOffset { scale } => SqlType::TimestampTz(stored_scale(scale)),
            Self::Char(Length::Given(length)) | Self::NChar(Length::Given(length)) => {
                SqlType::Bpchar(Some(length))
            }
            Self::VarChar(Length::Given(length)) | Self::NVarChar(Length::Given(length)) => {
                SqlType::Varchar(Some(length))
            }
            Self::Char(Length::Max)
            | Self::NChar(Length::Max)
            | Self::VarChar(Length::Max)
            | Self::NVarChar(Length::Max)
            | Self::Text
            | Self::NText => SqlType::Text,
            Self::Binary(_) | Self::VarBinary(_) | Self::Image => SqlType::Bytea,
            Self::UniqueIdentifier => SqlType::Uuid,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::TinyInt => "tinyint",
            Self::SmallInt => "smallint",
            Self::Int => "int",
            Self::BigInt => "bigint",
            Self::Bit => "bit",
            Self::Decimal { .. } => "numeric",
            Self::Money => "money",
            Self::SmallMoney => "smallmoney",
            Self::Float => "float",
            Self::Real => "real",
            Self::Date => "date",
            Self::Time { .. } => "time",
            Self::DateTime => "datetime",
            Self::SmallDateTime => "smalldatetime",
            Self::DateTime2 { .. } => "datetime2",
            Self::DateTimeOffset { .. } => "datetimeoffset",
            Self::Char(_) => "char",
            Self::VarChar(_) => "varchar",
            Self::NChar(_) => "nchar",
            Self::NVarChar(_) => "nvarchar",
            Self::Text => "text",
            Self::NText => "ntext",
            Self::Binary(_) => "binary",
            Self::VarBinary(_) => "varbinary",
            Self::Image => "image",
            Self::UniqueIdentifier => "uniqueidentifier",
        }
    }

    pub(crate) fn kind(self) -> TypeFamily {
        match self {
            Self::TinyInt
            | Self::SmallInt
            | Self::Int
            | Self::BigInt
            | Self::Decimal { .. }
            | Self::Money
            | Self::SmallMoney
            | Self::Float
            | Self::Real => TypeFamily::Number,
            Self::Bit => TypeFamily::Bit,
            Self::Date => TypeFamily::Date,
            Self::Time { .. } => TypeFamily::Time,
            Self::DateTime
            | Self::SmallDateTime
            | Self::DateTime2 { .. }
            | Self::DateTimeOffset { .. } => TypeFamily::DateTime,
            Self::Char(_)
            | Self::VarChar(_)
            | Self::NChar(_)
            | Self::NVarChar(_)
            | Self::Text
            | Self::NText => TypeFamily::Character,
            Self::Binary(_) | Self::VarBinary(_) | Self::Image => TypeFamily::Binary,
            Self::UniqueIdentifier => TypeFamily::Guid,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TypeFamily {
    Number,
    Bit,
    Date,
    Time,
    DateTime,
    Character,
    Binary,
    Guid,
    Other,
}

impl TypeFamily {
    pub(crate) fn of(value: &Value) -> Self {
        match value {
            Value::SmallInt(_)
            | Value::Integer(_)
            | Value::BigInt(_)
            | Value::Real(_)
            | Value::Double(_)
            | Value::Numeric(_) => Self::Number,
            Value::Boolean(_) => Self::Bit,
            Value::Date(_) => Self::Date,
            Value::Time(_) => Self::Time,
            Value::Timestamp(_) | Value::TimestampTz(_) => Self::DateTime,
            Value::Text(_) | Value::Bpchar(_) => Self::Character,
            Value::Bytea(_) => Self::Binary,
            Value::Uuid(_) => Self::Guid,
            _ => Self::Other,
        }
    }

    pub(crate) fn of_engine(sql_type: SqlType) -> Self {
        match sql_type {
            SqlType::SmallInt
            | SqlType::Integer
            | SqlType::BigInt
            | SqlType::Real
            | SqlType::Double
            | SqlType::Numeric(_) => Self::Number,
            SqlType::Boolean => Self::Bit,
            SqlType::Date => Self::Date,
            SqlType::Time(_) => Self::Time,
            SqlType::Timestamp(_) | SqlType::TimestampTz(_) => Self::DateTime,
            SqlType::Text | SqlType::Varchar(_) | SqlType::Bpchar(_) => Self::Character,
            SqlType::Bytea => Self::Binary,
            SqlType::Uuid => Self::Guid,
            _ => Self::Other,
        }
    }

    fn is_temporal(self) -> bool {
        matches!(self, Self::Date | Self::Time | Self::DateTime)
    }

    pub(crate) fn allows(self, target: Self) -> bool {
        if self == target || self == Self::Other || target == Self::Other {
            return true;
        }
        let numeric = |kind: Self| matches!(kind, Self::Number | Self::Bit);
        match (self, target) {
            (Self::Date, Self::Time) | (Self::Time, Self::Date) => false,
            (source, target) if source.is_temporal() && numeric(target) => false,
            (source, target) if numeric(source) && target.is_temporal() => false,
            (Self::Guid, target) | (target, Self::Guid) => {
                matches!(target, Self::Character | Self::Binary)
            }
            (Self::Binary, target) if target.is_temporal() => false,
            _ => true,
        }
    }
}

pub(crate) fn precedence(value: &Value) -> Option<u8> {
    Some(match value {
        Value::TimestampTz(_) => 25,
        Value::Timestamp(_) => 24,
        Value::Date(_) => 21,
        Value::Time(_) => 20,
        Value::Double(_) => 19,
        Value::Real(_) => 18,
        Value::Numeric(_) => 17,
        Value::BigInt(_) => 14,
        Value::Integer(_) => 13,
        Value::SmallInt(_) => 12,
        Value::Boolean(_) => 10,
        Value::Uuid(_) => 5,
        Value::Text(_) | Value::Bpchar(_) => 4,
        Value::Bytea(_) => 2,
        _ => return None,
    })
}

pub(crate) fn value_type_name(value: &Value) -> &'static str {
    match value {
        Value::Boolean(_) => "bit",
        Value::SmallInt(_) => "smallint",
        Value::Integer(_) => "int",
        Value::BigInt(_) => "bigint",
        Value::Real(_) => "real",
        Value::Double(_) => "float",
        Value::Numeric(_) => "numeric",
        Value::Text(_) => "varchar",
        Value::Bpchar(_) => "char",
        Value::Date(_) => "date",
        Value::Time(_) => "time",
        Value::Timestamp(_) => "datetime2",
        Value::TimestampTz(_) => "datetimeoffset",
        Value::Uuid(_) => "uniqueidentifier",
        Value::Bytea(_) => "varbinary",
        Value::Null => "NULL",
        _ => "sql_variant",
    }
}

pub(crate) fn engine_type_name(sql_type: SqlType) -> &'static str {
    match sql_type {
        SqlType::Boolean => "bit",
        SqlType::SmallInt => "smallint",
        SqlType::Integer => "int",
        SqlType::BigInt => "bigint",
        SqlType::Real => "real",
        SqlType::Double => "float",
        SqlType::Numeric(_) => "numeric",
        SqlType::Text | SqlType::Varchar(_) => "varchar",
        SqlType::Bpchar(_) => "char",
        SqlType::Date => "date",
        SqlType::Time(_) => "time",
        SqlType::Timestamp(_) => "datetime2",
        SqlType::TimestampTz(_) => "datetimeoffset",
        SqlType::Uuid => "uniqueidentifier",
        SqlType::Bytea => "varbinary",
        _ => "sql_variant",
    }
}

pub(crate) fn operand_clash(source: &str, target: &str) -> SqlError {
    SqlError::new(
        SqlState::DATATYPE_MISMATCH,
        format!("Operand type clash: {source} is incompatible with {target}"),
    )
    .with_sqlserver_code(206)
}

pub(crate) fn explicit_conversion_refused(source: &str, target: &str) -> SqlError {
    SqlError::new(
        SqlState::CANNOT_COERCE,
        format!("Explicit conversion from data type {source} to {target} is not allowed."),
    )
    .with_sqlserver_code(529)
}

pub(crate) fn incompatible_in_operator(left: &Value, right: &Value, operator: &str) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_FUNCTION,
        format!(
            "The data types {} and {} are incompatible in the {operator} operator.",
            value_type_name(left),
            value_type_name(right)
        ),
    )
    .with_sqlserver_code(402)
}

#[cfg(test)]
mod tests {
    use super::{Length, SqlServerType, TypeFamily, precedence};
    use crate::types::{SqlType, Value};

    #[test]
    fn sql_server_types_map_onto_the_engine_types_with_their_scales_capped_at_six() {
        assert_eq!(SqlServerType::TinyInt.engine_type(), SqlType::SmallInt);
        assert_eq!(SqlServerType::Bit.engine_type(), SqlType::Boolean);
        assert_eq!(
            SqlServerType::DateTime2 { scale: 7 }.engine_type(),
            SqlType::Timestamp(Some(6))
        );
        assert_eq!(
            SqlServerType::DateTime.engine_type(),
            SqlType::Timestamp(Some(3))
        );
        assert_eq!(
            SqlServerType::NVarChar(Length::Max).engine_type(),
            SqlType::Text
        );
        assert_eq!(
            SqlServerType::NChar(Length::Given(5)).engine_type(),
            SqlType::Bpchar(Some(5))
        );
        assert_eq!(SqlServerType::UniqueIdentifier.engine_type(), SqlType::Uuid);
    }

    #[test]
    fn precedence_puts_dates_over_numbers_over_bit_over_guids_over_strings_over_bytes() {
        let order = [
            Value::TimestampTz(0),
            Value::Timestamp(0),
            Value::Date(0),
            Value::Double(0.0),
            Value::Integer(0),
            Value::Boolean(false),
            Value::Uuid([0; 16]),
            Value::Text(String::new()),
            Value::Bytea(Vec::new()),
        ];
        for pair in order.windows(2) {
            assert!(precedence(&pair[0]) > precedence(&pair[1]), "{pair:?}");
        }
    }

    #[test]
    fn the_conversion_chart_refuses_dates_with_numbers_and_guids_with_numbers() {
        assert!(!TypeFamily::Date.allows(TypeFamily::Number));
        assert!(!TypeFamily::Number.allows(TypeFamily::DateTime));
        assert!(!TypeFamily::Guid.allows(TypeFamily::Number));
        assert!(!TypeFamily::Date.allows(TypeFamily::Time));
        assert!(TypeFamily::Character.allows(TypeFamily::Guid));
        assert!(TypeFamily::Character.allows(TypeFamily::Date));
        assert!(TypeFamily::Bit.allows(TypeFamily::Number));
    }
}
