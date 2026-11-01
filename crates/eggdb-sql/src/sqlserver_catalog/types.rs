use crate::sqlserver_types::{Length, SqlServerType};
use crate::types::SqlType;

const MAX_NVARCHAR_LENGTH: u32 = 4000;
const DEFAULT_FRACTIONAL_DIGITS: u8 = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SqlServerTypeName {
    TinyInt,
    SmallInt,
    Int,
    BigInt,
    Bit,
    Decimal,
    Numeric,
    Money,
    SmallMoney,
    Real,
    Float,
    Date,
    Time,
    DateTime,
    SmallDateTime,
    DateTime2,
    DateTimeOffset,
    Char,
    VarChar,
    Text,
    NChar,
    NVarChar,
    NText,
    Binary,
    VarBinary,
    Image,
    UniqueIdentifier,
    SqlVariant,
    Xml,
    SysName,
}

pub(crate) struct SystemType {
    pub(crate) name: SqlServerTypeName,
    pub(crate) system_type_id: i64,
    pub(crate) user_type_id: i64,
    pub(crate) max_length: i64,
    pub(crate) precision: i64,
    pub(crate) scale: i64,
    pub(crate) collated: bool,
}

const fn system_type(
    name: SqlServerTypeName,
    system_type_id: i64,
    max_length: i64,
    precision: i64,
    scale: i64,
    collated: bool,
) -> SystemType {
    SystemType {
        name,
        system_type_id,
        user_type_id: system_type_id,
        max_length,
        precision,
        scale,
        collated,
    }
}

pub(crate) static SYSTEM_TYPES: [SystemType; 30] = [
    system_type(SqlServerTypeName::Image, 34, 16, 0, 0, false),
    system_type(SqlServerTypeName::Text, 35, 16, 0, 0, true),
    system_type(SqlServerTypeName::UniqueIdentifier, 36, 16, 0, 0, false),
    system_type(SqlServerTypeName::Date, 40, 3, 10, 0, false),
    system_type(SqlServerTypeName::Time, 41, 5, 16, 7, false),
    system_type(SqlServerTypeName::DateTime2, 42, 8, 27, 7, false),
    system_type(SqlServerTypeName::DateTimeOffset, 43, 10, 34, 7, false),
    system_type(SqlServerTypeName::TinyInt, 48, 1, 3, 0, false),
    system_type(SqlServerTypeName::SmallInt, 52, 2, 5, 0, false),
    system_type(SqlServerTypeName::Int, 56, 4, 10, 0, false),
    system_type(SqlServerTypeName::SmallDateTime, 58, 4, 16, 0, false),
    system_type(SqlServerTypeName::Real, 59, 4, 24, 0, false),
    system_type(SqlServerTypeName::Money, 60, 8, 19, 4, false),
    system_type(SqlServerTypeName::DateTime, 61, 8, 23, 3, false),
    system_type(SqlServerTypeName::Float, 62, 8, 53, 0, false),
    system_type(SqlServerTypeName::SqlVariant, 98, 8016, 0, 0, false),
    system_type(SqlServerTypeName::NText, 99, 16, 0, 0, true),
    system_type(SqlServerTypeName::Bit, 104, 1, 1, 0, false),
    system_type(SqlServerTypeName::Decimal, 106, 17, 38, 38, false),
    system_type(SqlServerTypeName::Numeric, 108, 17, 38, 38, false),
    system_type(SqlServerTypeName::SmallMoney, 122, 4, 10, 4, false),
    system_type(SqlServerTypeName::BigInt, 127, 8, 19, 0, false),
    system_type(SqlServerTypeName::VarBinary, 165, 8000, 0, 0, false),
    system_type(SqlServerTypeName::VarChar, 167, 8000, 0, 0, true),
    system_type(SqlServerTypeName::Binary, 173, 8000, 0, 0, false),
    system_type(SqlServerTypeName::Char, 175, 8000, 0, 0, true),
    system_type(SqlServerTypeName::NVarChar, 231, 8000, 0, 0, true),
    system_type(SqlServerTypeName::NChar, 239, 8000, 0, 0, true),
    system_type(SqlServerTypeName::Xml, 241, -1, 0, 0, false),
    SystemType {
        name: SqlServerTypeName::SysName,
        system_type_id: 231,
        user_type_id: 256,
        max_length: 256,
        precision: 0,
        scale: 0,
        collated: true,
    },
];

impl SqlServerTypeName {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::TinyInt => "tinyint",
            Self::SmallInt => "smallint",
            Self::Int => "int",
            Self::BigInt => "bigint",
            Self::Bit => "bit",
            Self::Decimal => "decimal",
            Self::Numeric => "numeric",
            Self::Money => "money",
            Self::SmallMoney => "smallmoney",
            Self::Real => "real",
            Self::Float => "float",
            Self::Date => "date",
            Self::Time => "time",
            Self::DateTime => "datetime",
            Self::SmallDateTime => "smalldatetime",
            Self::DateTime2 => "datetime2",
            Self::DateTimeOffset => "datetimeoffset",
            Self::Char => "char",
            Self::VarChar => "varchar",
            Self::Text => "text",
            Self::NChar => "nchar",
            Self::NVarChar => "nvarchar",
            Self::NText => "ntext",
            Self::Binary => "binary",
            Self::VarBinary => "varbinary",
            Self::Image => "image",
            Self::UniqueIdentifier => "uniqueidentifier",
            Self::SqlVariant => "sql_variant",
            Self::Xml => "xml",
            Self::SysName => "sysname",
        }
    }

    pub(crate) fn system(self) -> &'static SystemType {
        SYSTEM_TYPES
            .iter()
            .find(|found| found.name == self)
            .unwrap_or(&SYSTEM_TYPES[0])
    }

    pub(crate) fn odbc_type(self) -> i64 {
        match self {
            Self::Bit => -7,
            Self::TinyInt => -6,
            Self::SmallInt => 5,
            Self::Int => 4,
            Self::BigInt => -5,
            Self::Decimal | Self::Money | Self::SmallMoney => 3,
            Self::Numeric => 2,
            Self::Real => 7,
            Self::Float => 6,
            Self::Char => 1,
            Self::VarChar => 12,
            Self::Text => -1,
            Self::NChar => -8,
            Self::NVarChar | Self::SysName => -9,
            Self::NText => -10,
            Self::Binary => -2,
            Self::VarBinary => -3,
            Self::Image => -4,
            Self::UniqueIdentifier => -11,
            Self::DateTime | Self::SmallDateTime | Self::DateTime2 => 93,
            Self::Date => 91,
            Self::Time => -154,
            Self::DateTimeOffset => -155,
            Self::SqlVariant => -150,
            Self::Xml => -152,
        }
    }

    pub(crate) fn odbc_sql_type(self) -> (i64, Option<i64>) {
        match self {
            Self::DateTime | Self::SmallDateTime | Self::DateTime2 => (9, Some(3)),
            Self::Date => (9, Some(1)),
            Self::Time => (-154, Some(0)),
            Self::DateTimeOffset => (-155, Some(0)),
            other => (other.odbc_type(), None),
        }
    }

    pub(crate) fn is_unicode(self) -> bool {
        matches!(
            self,
            Self::NChar | Self::NVarChar | Self::NText | Self::SysName
        )
    }

    pub(crate) fn is_character(self) -> bool {
        self.is_unicode() || matches!(self, Self::Char | Self::VarChar | Self::Text)
    }

    pub(crate) fn is_exact_number(self) -> bool {
        matches!(
            self,
            Self::TinyInt
                | Self::SmallInt
                | Self::Int
                | Self::BigInt
                | Self::Decimal
                | Self::Numeric
                | Self::Money
                | Self::SmallMoney
        )
    }

    pub(crate) fn is_temporal(self) -> bool {
        matches!(
            self,
            Self::Date
                | Self::Time
                | Self::DateTime
                | Self::SmallDateTime
                | Self::DateTime2
                | Self::DateTimeOffset
        )
    }
}

/// How a column of an engine type shows in SQL Server's catalog: the SQL Server type it is
/// kept for (D123 maps several SQL Server types onto one engine type, so the most common one
/// is shown) with its length, precision and scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SqlServerColumnType {
    pub(crate) name: SqlServerTypeName,
    pub(crate) length: Option<u32>,
    pub(crate) precision: i64,
    pub(crate) scale: i64,
}

fn fractional(precision: Option<u8>) -> i64 {
    i64::from(precision.unwrap_or(DEFAULT_FRACTIONAL_DIGITS))
}

fn with_fraction(base: i64, scale: i64) -> i64 {
    if scale > 0 { base + scale + 1 } else { base }
}

fn time_bytes(scale: i64) -> i64 {
    match scale {
        0..=2 => 3,
        3..=4 => 4,
        _ => 5,
    }
}

fn decimal_bytes(precision: i64) -> i64 {
    match precision {
        0..=9 => 5,
        10..=19 => 9,
        20..=28 => 13,
        _ => 17,
    }
}

impl SqlServerColumnType {
    pub(crate) fn of(sql_type: SqlType) -> Self {
        let plain = |name: SqlServerTypeName| {
            let system = name.system();
            Self {
                name,
                length: None,
                precision: system.precision,
                scale: system.scale,
            }
        };
        let sized = |name: SqlServerTypeName, length: Option<u32>| Self {
            name,
            length: length.filter(|length| *length <= MAX_NVARCHAR_LENGTH),
            precision: 0,
            scale: 0,
        };
        match sql_type {
            SqlType::SmallInt => plain(SqlServerTypeName::SmallInt),
            SqlType::Integer => plain(SqlServerTypeName::Int),
            SqlType::BigInt => plain(SqlServerTypeName::BigInt),
            SqlType::Boolean => plain(SqlServerTypeName::Bit),
            SqlType::Real => plain(SqlServerTypeName::Real),
            SqlType::Double => plain(SqlServerTypeName::Float),
            SqlType::Numeric(Some(modifier)) => Self {
                name: SqlServerTypeName::Decimal,
                length: None,
                precision: i64::from(modifier.precision),
                scale: i64::from(modifier.scale.max(0)),
            },
            SqlType::Numeric(None) => Self {
                name: SqlServerTypeName::Numeric,
                length: None,
                precision: 38,
                scale: 0,
            },
            SqlType::Date => plain(SqlServerTypeName::Date),
            SqlType::Time(precision) => {
                let scale = fractional(precision);
                Self {
                    name: SqlServerTypeName::Time,
                    length: None,
                    precision: with_fraction(8, scale),
                    scale,
                }
            }
            SqlType::Timestamp(Some(3)) => plain(SqlServerTypeName::DateTime),
            SqlType::Timestamp(precision) => {
                let scale = fractional(precision);
                Self {
                    name: SqlServerTypeName::DateTime2,
                    length: None,
                    precision: with_fraction(19, scale),
                    scale,
                }
            }
            SqlType::TimestampTz(precision) => {
                let scale = fractional(precision);
                Self {
                    name: SqlServerTypeName::DateTimeOffset,
                    length: None,
                    precision: with_fraction(26, scale),
                    scale,
                }
            }
            SqlType::Varchar(length) => sized(SqlServerTypeName::NVarChar, length),
            SqlType::Bpchar(Some(length)) if length <= MAX_NVARCHAR_LENGTH => {
                sized(SqlServerTypeName::NChar, Some(length))
            }
            SqlType::Bytea => sized(SqlServerTypeName::VarBinary, None),
            SqlType::Uuid => plain(SqlServerTypeName::UniqueIdentifier),
            _ => sized(SqlServerTypeName::NVarChar, None),
        }
    }

    /// The catalog form of a type as it was declared (a routine parameter or return type),
    /// keeping the exact SQL Server type, length, precision and scale.
    pub(crate) fn declared(declared: SqlServerType) -> Self {
        let plain = |name: SqlServerTypeName| {
            let system = name.system();
            Self {
                name,
                length: None,
                precision: system.precision,
                scale: system.scale,
            }
        };
        let sized = |name: SqlServerTypeName, length: Length| Self {
            name,
            length: match length {
                Length::Given(length) => Some(length),
                Length::Max => None,
            },
            precision: 0,
            scale: 0,
        };
        let timed = |name: SqlServerTypeName, base: i64, scale: u8| {
            let scale = i64::from(scale);
            Self {
                name,
                length: None,
                precision: with_fraction(base, scale),
                scale,
            }
        };
        match declared {
            SqlServerType::TinyInt => plain(SqlServerTypeName::TinyInt),
            SqlServerType::SmallInt => plain(SqlServerTypeName::SmallInt),
            SqlServerType::Int => plain(SqlServerTypeName::Int),
            SqlServerType::BigInt => plain(SqlServerTypeName::BigInt),
            SqlServerType::Bit => plain(SqlServerTypeName::Bit),
            SqlServerType::Decimal { precision, scale } => Self {
                name: SqlServerTypeName::Decimal,
                length: None,
                precision: i64::from(precision),
                scale: i64::from(scale),
            },
            SqlServerType::Money => plain(SqlServerTypeName::Money),
            SqlServerType::SmallMoney => plain(SqlServerTypeName::SmallMoney),
            SqlServerType::Float => plain(SqlServerTypeName::Float),
            SqlServerType::Real => plain(SqlServerTypeName::Real),
            SqlServerType::Date => plain(SqlServerTypeName::Date),
            SqlServerType::Time { scale } => timed(SqlServerTypeName::Time, 8, scale),
            SqlServerType::DateTime => plain(SqlServerTypeName::DateTime),
            SqlServerType::SmallDateTime => plain(SqlServerTypeName::SmallDateTime),
            SqlServerType::DateTime2 { scale } => timed(SqlServerTypeName::DateTime2, 19, scale),
            SqlServerType::DateTimeOffset { scale } => {
                timed(SqlServerTypeName::DateTimeOffset, 26, scale)
            }
            SqlServerType::Char(length) => sized(SqlServerTypeName::Char, length),
            SqlServerType::VarChar(length) => sized(SqlServerTypeName::VarChar, length),
            SqlServerType::NChar(length) => sized(SqlServerTypeName::NChar, length),
            SqlServerType::NVarChar(length) => sized(SqlServerTypeName::NVarChar, length),
            SqlServerType::Text => plain(SqlServerTypeName::Text),
            SqlServerType::NText => plain(SqlServerTypeName::NText),
            SqlServerType::Binary(length) => sized(SqlServerTypeName::Binary, length),
            SqlServerType::VarBinary(length) => sized(SqlServerTypeName::VarBinary, length),
            SqlServerType::Image => plain(SqlServerTypeName::Image),
            SqlServerType::UniqueIdentifier => plain(SqlServerTypeName::UniqueIdentifier),
        }
    }

    pub(crate) fn is_max(self) -> bool {
        matches!(
            self.name,
            SqlServerTypeName::NVarChar | SqlServerTypeName::VarChar | SqlServerTypeName::VarBinary
        ) && self.length.is_none()
    }

    pub(crate) fn system_type_id(self) -> i64 {
        self.name.system().system_type_id
    }

    pub(crate) fn max_length(self) -> i64 {
        match self.name {
            SqlServerTypeName::NVarChar | SqlServerTypeName::NChar => self
                .length
                .map_or(-1, |length| i64::from(length).saturating_mul(2)),
            SqlServerTypeName::VarBinary
            | SqlServerTypeName::VarChar
            | SqlServerTypeName::Char
            | SqlServerTypeName::Binary => self.length.map_or(-1, i64::from),
            SqlServerTypeName::Decimal | SqlServerTypeName::Numeric => {
                decimal_bytes(self.precision)
            }
            SqlServerTypeName::Time => time_bytes(self.scale),
            SqlServerTypeName::DateTime2 => time_bytes(self.scale) + 3,
            SqlServerTypeName::DateTimeOffset => time_bytes(self.scale) + 5,
            other => other.system().max_length,
        }
    }

    pub(crate) fn character_length(self) -> Option<i64> {
        if !self.name.is_character() {
            return None;
        }
        Some(self.length.map_or(-1, i64::from))
    }

    pub(crate) fn octet_length(self) -> Option<i64> {
        match self.name {
            SqlServerTypeName::NVarChar | SqlServerTypeName::NChar => Some(
                self.length
                    .map_or(-1, |length| i64::from(length).saturating_mul(2)),
            ),
            SqlServerTypeName::VarBinary
            | SqlServerTypeName::VarChar
            | SqlServerTypeName::Char
            | SqlServerTypeName::Binary => Some(self.length.map_or(-1, i64::from)),
            _ => None,
        }
    }

    pub(crate) fn numeric_precision(self) -> Option<i64> {
        match self.name {
            SqlServerTypeName::Real | SqlServerTypeName::Float => Some(self.precision),
            name if name.is_exact_number() => Some(self.precision),
            _ => None,
        }
    }

    pub(crate) fn numeric_radix(self) -> Option<i64> {
        match self.name {
            SqlServerTypeName::Real | SqlServerTypeName::Float => Some(2),
            name if name.is_exact_number() => Some(10),
            _ => None,
        }
    }

    pub(crate) fn numeric_scale(self) -> Option<i64> {
        self.name.is_exact_number().then_some(self.scale)
    }

    pub(crate) fn datetime_precision(self) -> Option<i64> {
        match self.name {
            SqlServerTypeName::Date | SqlServerTypeName::SmallDateTime => Some(0),
            SqlServerTypeName::DateTime => Some(3),
            name if name.is_temporal() => Some(self.scale),
            _ => None,
        }
    }

    pub(crate) fn odbc_precision(self) -> i64 {
        match self.name {
            SqlServerTypeName::NVarChar | SqlServerTypeName::NChar => {
                self.length.map_or(0, i64::from)
            }
            SqlServerTypeName::VarBinary
            | SqlServerTypeName::VarChar
            | SqlServerTypeName::Char
            | SqlServerTypeName::Binary => self.length.map_or(0, i64::from),
            SqlServerTypeName::Real => 7,
            SqlServerTypeName::Float => 15,
            SqlServerTypeName::UniqueIdentifier => 36,
            _ => self.precision,
        }
    }

    pub(crate) fn odbc_length(self) -> i64 {
        match self.name {
            SqlServerTypeName::NVarChar | SqlServerTypeName::NChar => self
                .length
                .map_or(0, |length| i64::from(length).saturating_mul(2)),
            SqlServerTypeName::VarBinary
            | SqlServerTypeName::VarChar
            | SqlServerTypeName::Char
            | SqlServerTypeName::Binary => self.length.map_or(0, i64::from),
            SqlServerTypeName::Decimal | SqlServerTypeName::Numeric => self.precision + 2,
            SqlServerTypeName::Money => 21,
            SqlServerTypeName::DateTime => 16,
            SqlServerTypeName::Date
            | SqlServerTypeName::Time
            | SqlServerTypeName::DateTime2
            | SqlServerTypeName::DateTimeOffset => self.precision.saturating_mul(2),
            other => other.system().max_length,
        }
    }

    pub(crate) fn odbc_scale(self) -> Option<i64> {
        match self.name {
            name if name.is_exact_number() => Some(self.scale),
            SqlServerTypeName::DateTime
            | SqlServerTypeName::DateTime2
            | SqlServerTypeName::Time
            | SqlServerTypeName::DateTimeOffset => Some(self.scale),
            SqlServerTypeName::SmallDateTime => Some(0),
            _ => None,
        }
    }

    pub(crate) fn odbc_radix(self) -> Option<i64> {
        match self.name {
            SqlServerTypeName::Real | SqlServerTypeName::Float => Some(10),
            name if name.is_exact_number() => Some(10),
            _ => None,
        }
    }

    pub(crate) fn odbc_octet_length(self) -> Option<i64> {
        match self.name {
            SqlServerTypeName::NVarChar
            | SqlServerTypeName::NChar
            | SqlServerTypeName::VarBinary
            | SqlServerTypeName::VarChar
            | SqlServerTypeName::Char
            | SqlServerTypeName::Binary => Some(self.odbc_length()),
            _ => None,
        }
    }

    pub(crate) fn tds_type(self, nullable: bool) -> i64 {
        match (self.name, nullable) {
            (SqlServerTypeName::TinyInt, false) => 48,
            (SqlServerTypeName::SmallInt, false) => 52,
            (SqlServerTypeName::Int, false) => 56,
            (SqlServerTypeName::BigInt, false) => 127,
            (
                SqlServerTypeName::TinyInt
                | SqlServerTypeName::SmallInt
                | SqlServerTypeName::Int
                | SqlServerTypeName::BigInt,
                true,
            ) => 38,
            (SqlServerTypeName::Bit, false) => 50,
            (SqlServerTypeName::Bit, true) => 104,
            (SqlServerTypeName::Real, false) => 59,
            (SqlServerTypeName::Float, false) => 62,
            (SqlServerTypeName::Real | SqlServerTypeName::Float, true) => 109,
            (SqlServerTypeName::Decimal, false) => 55,
            (SqlServerTypeName::Decimal, true) => 106,
            (SqlServerTypeName::Numeric, false) => 63,
            (SqlServerTypeName::Numeric, true) => 108,
            (SqlServerTypeName::DateTime, false) => 61,
            (SqlServerTypeName::DateTime, true) => 111,
            (SqlServerTypeName::NChar | SqlServerTypeName::NVarChar, _) if !self.is_max() => 39,
            _ => 0,
        }
    }
}

pub(crate) struct DataTypeInfo {
    pub(crate) type_name: &'static str,
    pub(crate) name: SqlServerTypeName,
    pub(crate) precision: i64,
    pub(crate) identity: bool,
}

const fn info(
    type_name: &'static str,
    name: SqlServerTypeName,
    precision: i64,
    identity: bool,
) -> DataTypeInfo {
    DataTypeInfo {
        type_name,
        name,
        precision,
        identity,
    }
}

impl DataTypeInfo {
    pub(crate) fn literal_quotes(&self) -> Option<(&'static str, &'static str)> {
        match self.name {
            name if name.is_unicode() => Some(("N'", "'")),
            name if name.is_character() || name.is_temporal() => Some(("'", "'")),
            SqlServerTypeName::UniqueIdentifier => Some(("'", "'")),
            SqlServerTypeName::Binary | SqlServerTypeName::VarBinary | SqlServerTypeName::Image => {
                Some(("0x", ""))
            }
            SqlServerTypeName::Money | SqlServerTypeName::SmallMoney => Some(("$", "")),
            _ => None,
        }
    }

    pub(crate) fn create_params(&self) -> Option<&'static str> {
        match self.name {
            SqlServerTypeName::NVarChar
            | SqlServerTypeName::NChar
            | SqlServerTypeName::VarChar
            | SqlServerTypeName::Char
            | SqlServerTypeName::VarBinary
            | SqlServerTypeName::Binary => Some("max length"),
            SqlServerTypeName::Decimal | SqlServerTypeName::Numeric if self.identity => {
                Some("precision")
            }
            SqlServerTypeName::Decimal | SqlServerTypeName::Numeric => Some("precision,scale"),
            SqlServerTypeName::Time
            | SqlServerTypeName::DateTime2
            | SqlServerTypeName::DateTimeOffset => Some("scale"),
            _ => None,
        }
    }

    pub(crate) fn searchable(&self) -> i64 {
        match self.name {
            SqlServerTypeName::Text | SqlServerTypeName::NText => 1,
            SqlServerTypeName::Image | SqlServerTypeName::Xml => 0,
            name if name.is_character() || name.is_temporal() => 3,
            _ => 2,
        }
    }

    pub(crate) fn unsigned(&self) -> Option<bool> {
        match self.name {
            SqlServerTypeName::TinyInt => Some(true),
            SqlServerTypeName::Real | SqlServerTypeName::Float => Some(false),
            name if name.is_exact_number() => Some(false),
            _ => None,
        }
    }

    pub(crate) fn money(&self) -> bool {
        matches!(
            self.name,
            SqlServerTypeName::Money | SqlServerTypeName::SmallMoney
        )
    }

    pub(crate) fn auto_increment(&self) -> Option<bool> {
        match self.name {
            SqlServerTypeName::Real | SqlServerTypeName::Float => Some(false),
            name if name.is_exact_number() => Some(self.identity),
            _ => None,
        }
    }

    pub(crate) fn scale_range(&self) -> Option<(i64, i64)> {
        match self.name {
            SqlServerTypeName::Decimal | SqlServerTypeName::Numeric if !self.identity => {
                Some((0, 38))
            }
            SqlServerTypeName::Money | SqlServerTypeName::SmallMoney => Some((4, 4)),
            SqlServerTypeName::Time
            | SqlServerTypeName::DateTime2
            | SqlServerTypeName::DateTimeOffset => Some((0, 7)),
            SqlServerTypeName::DateTime => Some((3, 3)),
            name if name.is_exact_number() => Some((0, 0)),
            SqlServerTypeName::Bit | SqlServerTypeName::SmallDateTime => Some((0, 0)),
            _ => None,
        }
    }
}

pub(crate) static DATA_TYPES: [DataTypeInfo; 36] = [
    info("sql_variant", SqlServerTypeName::SqlVariant, 8000, false),
    info(
        "uniqueidentifier",
        SqlServerTypeName::UniqueIdentifier,
        36,
        false,
    ),
    info("ntext", SqlServerTypeName::NText, 1_073_741_823, false),
    info("xml", SqlServerTypeName::Xml, 0, false),
    info("nvarchar", SqlServerTypeName::NVarChar, 4000, false),
    info("sysname", SqlServerTypeName::SysName, 128, false),
    info("date", SqlServerTypeName::Date, 10, false),
    info("time", SqlServerTypeName::Time, 16, false),
    info("datetime2", SqlServerTypeName::DateTime2, 27, false),
    info(
        "datetimeoffset",
        SqlServerTypeName::DateTimeOffset,
        34,
        false,
    ),
    info("nchar", SqlServerTypeName::NChar, 4000, false),
    info("bit", SqlServerTypeName::Bit, 1, false),
    info("tinyint", SqlServerTypeName::TinyInt, 3, false),
    info("tinyint identity", SqlServerTypeName::TinyInt, 3, true),
    info("bigint", SqlServerTypeName::BigInt, 19, false),
    info("bigint identity", SqlServerTypeName::BigInt, 19, true),
    info("image", SqlServerTypeName::Image, 2_147_483_647, false),
    info("varbinary", SqlServerTypeName::VarBinary, 8000, false),
    info("binary", SqlServerTypeName::Binary, 8000, false),
    info("text", SqlServerTypeName::Text, 2_147_483_647, false),
    info("char", SqlServerTypeName::Char, 8000, false),
    info("numeric", SqlServerTypeName::Numeric, 38, false),
    info("numeric() identity", SqlServerTypeName::Numeric, 38, true),
    info("decimal", SqlServerTypeName::Decimal, 38, false),
    info("money", SqlServerTypeName::Money, 19, false),
    info("smallmoney", SqlServerTypeName::SmallMoney, 10, false),
    info("decimal() identity", SqlServerTypeName::Decimal, 38, true),
    info("int", SqlServerTypeName::Int, 10, false),
    info("int identity", SqlServerTypeName::Int, 10, true),
    info("smallint", SqlServerTypeName::SmallInt, 5, false),
    info("smallint identity", SqlServerTypeName::SmallInt, 5, true),
    info("float", SqlServerTypeName::Float, 53, false),
    info("real", SqlServerTypeName::Real, 24, false),
    info("varchar", SqlServerTypeName::VarChar, 8000, false),
    info("datetime", SqlServerTypeName::DateTime, 23, false),
    info("smalldatetime", SqlServerTypeName::SmallDateTime, 16, false),
];

pub(crate) fn named_system_type(name: &str) -> Option<&'static SystemType> {
    let lower = name.to_ascii_lowercase();
    SYSTEM_TYPES.iter().find(|found| found.name.name() == lower)
}

pub(crate) fn system_type_with_id(user_type_id: i64) -> Option<&'static SystemType> {
    SYSTEM_TYPES
        .iter()
        .find(|found| found.user_type_id == user_type_id)
}
