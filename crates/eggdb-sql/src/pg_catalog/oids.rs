use super::{CatalogType, RegKind};
use crate::arrays::ArrayElement;
use crate::interval::IntervalFields;
use crate::types::{NumericModifier, SqlType};
use std::collections::HashSet;

pub(crate) const PG_CATALOG: u32 = 11;
pub(crate) const PG_TOAST: u32 = 99;
pub(crate) const PUBLIC: u32 = 2200;
pub(crate) const INFORMATION_SCHEMA: u32 = 13000;
pub(crate) const SESSION_USER: u32 = 10;
pub(crate) const DATABASE_OWNER: u32 = 6171;
pub(crate) const DATABASE: u32 = 16384;
pub(crate) const HEAP: u32 = 2;
pub(crate) const BTREE: u32 = 403;
pub(crate) const DEFAULT_COLLATION: u32 = 100;
pub(crate) const C_COLLATION: u32 = 950;
pub(crate) const DEFAULT_TABLESPACE: u32 = 1663;
pub(crate) const UTF8_ENCODING: i32 = 6;
const FIRST_USER_OID: u32 = 16385;

pub(crate) const PREDEFINED_ROLES: [(u32, &str); 14] = [
    (3373, "pg_monitor"),
    (3374, "pg_read_all_settings"),
    (3375, "pg_read_all_stats"),
    (3377, "pg_stat_scan_tables"),
    (4200, "pg_signal_backend"),
    (4544, "pg_checkpoint"),
    (4550, "pg_use_reserved_connections"),
    (4569, "pg_read_server_files"),
    (4570, "pg_write_server_files"),
    (4571, "pg_execute_server_program"),
    (6171, "pg_database_owner"),
    (6181, "pg_read_all_data"),
    (6182, "pg_write_all_data"),
    (6304, "pg_create_subscription"),
];

pub(crate) const SYSTEM_SCHEMAS: [(u32, &str, u32); 4] = [
    (PG_CATALOG, "pg_catalog", SESSION_USER),
    (PG_TOAST, "pg_toast", SESSION_USER),
    (PUBLIC, "public", DATABASE_OWNER),
    (INFORMATION_SCHEMA, "information_schema", SESSION_USER),
];

pub(crate) struct BuiltinType {
    pub(crate) oid: u32,
    pub(crate) typname: &'static str,
    pub(crate) shown: &'static str,
    pub(crate) length: i16,
    pub(crate) by_value: bool,
    pub(crate) kind: char,
    pub(crate) category: char,
    pub(crate) preferred: bool,
    pub(crate) element: u32,
    pub(crate) array: u32,
    pub(crate) align: char,
    pub(crate) storage: char,
    pub(crate) collation: u32,
}

pub(crate) const BUILTIN_TYPES: [BuiltinType; 66] = [
    BuiltinType {
        oid: 16,
        typname: "bool",
        shown: "boolean",
        length: 1,
        by_value: true,
        kind: 'b',
        category: 'B',
        preferred: true,
        element: 0,
        array: 1000,
        align: 'c',
        storage: 'p',
        collation: 0,
    },
    BuiltinType {
        oid: 17,
        typname: "bytea",
        shown: "bytea",
        length: -1,
        by_value: false,
        kind: 'b',
        category: 'U',
        preferred: false,
        element: 0,
        array: 1001,
        align: 'i',
        storage: 'x',
        collation: 0,
    },
    BuiltinType {
        oid: 18,
        typname: "char",
        shown: "\"char\"",
        length: 1,
        by_value: true,
        kind: 'b',
        category: 'Z',
        preferred: false,
        element: 0,
        array: 1002,
        align: 'c',
        storage: 'p',
        collation: 0,
    },
    BuiltinType {
        oid: 19,
        typname: "name",
        shown: "name",
        length: 64,
        by_value: false,
        kind: 'b',
        category: 'S',
        preferred: false,
        element: 18,
        array: 1003,
        align: 'c',
        storage: 'p',
        collation: 950,
    },
    BuiltinType {
        oid: 20,
        typname: "int8",
        shown: "bigint",
        length: 8,
        by_value: true,
        kind: 'b',
        category: 'N',
        preferred: false,
        element: 0,
        array: 1016,
        align: 'd',
        storage: 'p',
        collation: 0,
    },
    BuiltinType {
        oid: 21,
        typname: "int2",
        shown: "smallint",
        length: 2,
        by_value: true,
        kind: 'b',
        category: 'N',
        preferred: false,
        element: 0,
        array: 1005,
        align: 's',
        storage: 'p',
        collation: 0,
    },
    BuiltinType {
        oid: 22,
        typname: "int2vector",
        shown: "int2vector",
        length: -1,
        by_value: false,
        kind: 'b',
        category: 'A',
        preferred: false,
        element: 21,
        array: 1006,
        align: 'i',
        storage: 'p',
        collation: 0,
    },
    BuiltinType {
        oid: 23,
        typname: "int4",
        shown: "integer",
        length: 4,
        by_value: true,
        kind: 'b',
        category: 'N',
        preferred: false,
        element: 0,
        array: 1007,
        align: 'i',
        storage: 'p',
        collation: 0,
    },
    BuiltinType {
        oid: 24,
        typname: "regproc",
        shown: "regproc",
        length: 4,
        by_value: true,
        kind: 'b',
        category: 'N',
        preferred: false,
        element: 0,
        array: 1008,
        align: 'i',
        storage: 'p',
        collation: 0,
    },
    BuiltinType {
        oid: 25,
        typname: "text",
        shown: "text",
        length: -1,
        by_value: false,
        kind: 'b',
        category: 'S',
        preferred: true,
        element: 0,
        array: 1009,
        align: 'i',
        storage: 'x',
        collation: 100,
    },
    BuiltinType {
        oid: 26,
        typname: "oid",
        shown: "oid",
        length: 4,
        by_value: true,
        kind: 'b',
        category: 'N',
        preferred: true,
        element: 0,
        array: 1028,
        align: 'i',
        storage: 'p',
        collation: 0,
    },
    BuiltinType {
        oid: 28,
        typname: "xid",
        shown: "xid",
        length: 4,
        by_value: true,
        kind: 'b',
        category: 'U',
        preferred: false,
        element: 0,
        array: 1011,
        align: 'i',
        storage: 'p',
        collation: 0,
    },
    BuiltinType {
        oid: 30,
        typname: "oidvector",
        shown: "oidvector",
        length: -1,
        by_value: false,
        kind: 'b',
        category: 'A',
        preferred: false,
        element: 26,
        array: 1013,
        align: 'i',
        storage: 'p',
        collation: 0,
    },
    BuiltinType {
        oid: 114,
        typname: "json",
        shown: "json",
        length: -1,
        by_value: false,
        kind: 'b',
        category: 'U',
        preferred: false,
        element: 0,
        array: 199,
        align: 'i',
        storage: 'x',
        collation: 0,
    },
    BuiltinType {
        oid: 194,
        typname: "pg_node_tree",
        shown: "pg_node_tree",
        length: -1,
        by_value: false,
        kind: 'b',
        category: 'Z',
        preferred: false,
        element: 0,
        array: 0,
        align: 'i',
        storage: 'x',
        collation: 100,
    },
    BuiltinType {
        oid: 199,
        typname: "_json",
        shown: "json[]",
        length: -1,
        by_value: false,
        kind: 'b',
        category: 'A',
        preferred: false,
        element: 114,
        array: 0,
        align: 'i',
        storage: 'x',
        collation: 0,
    },
    BuiltinType {
        oid: 700,
        typname: "float4",
        shown: "real",
        length: 4,
        by_value: true,
        kind: 'b',
        category: 'N',
        preferred: false,
        element: 0,
        array: 1021,
        align: 'i',
        storage: 'p',
        collation: 0,
    },
    BuiltinType {
        oid: 701,
        typname: "float8",
        shown: "double precision",
        length: 8,
        by_value: true,
        kind: 'b',
        category: 'N',
        preferred: true,
        element: 0,
        array: 1022,
        align: 'd',
        storage: 'p',
        collation: 0,
    },
    BuiltinType {
        oid: 1000,
        typname: "_bool",
        shown: "boolean[]",
        length: -1,
        by_value: false,
        kind: 'b',
        category: 'A',
        preferred: false,
        element: 16,
        array: 0,
        align: 'i',
        storage: 'x',
        collation: 0,
    },
    BuiltinType {
        oid: 1001,
        typname: "_bytea",
        shown: "bytea[]",
        length: -1,
        by_value: false,
        kind: 'b',
        category: 'A',
        preferred: false,
        element: 17,
        array: 0,
        align: 'i',
        storage: 'x',
        collation: 0,
    },
    BuiltinType {
        oid: 1002,
        typname: "_char",
        shown: "\"char\"[]",
        length: -1,
        by_value: false,
        kind: 'b',
        category: 'A',
        preferred: false,
        element: 18,
        array: 0,
        align: 'i',
        storage: 'x',
        collation: 0,
    },
    BuiltinType {
        oid: 1003,
        typname: "_name",
        shown: "name[]",
        length: -1,
        by_value: false,
        kind: 'b',
        category: 'A',
        preferred: false,
        element: 19,
        array: 0,
        align: 'i',
        storage: 'x',
        collation: 950,
    },
    BuiltinType {
        oid: 1005,
        typname: "_int2",
        shown: "smallint[]",
        length: -1,
        by_value: false,
        kind: 'b',
        category: 'A',
        preferred: false,
        element: 21,
        array: 0,
        align: 'i',
        storage: 'x',
        collation: 0,
    },
    BuiltinType {
        oid: 1006,
        typname: "_int2vector",
        shown: "int2vector[]",
        length: -1,
        by_value: false,
        kind: 'b',
        category: 'A',
        preferred: false,
        element: 22,
        array: 0,
        align: 'i',
        storage: 'x',
        collation: 0,
    },
    BuiltinType {
        oid: 1007,
        typname: "_int4",
        shown: "integer[]",
        length: -1,
        by_value: false,
        kind: 'b',
        category: 'A',
        preferred: false,
        element: 23,
        array: 0,
        align: 'i',
        storage: 'x',
        collation: 0,
    },
    BuiltinType {
        oid: 1008,
        typname: "_regproc",
        shown: "regproc[]",
        length: -1,
        by_value: false,
        kind: 'b',
        category: 'A',
        preferred: false,
        element: 24,
        array: 0,
        align: 'i',
        storage: 'x',
        collation: 0,
    },
    BuiltinType {
        oid: 1009,
        typname: "_text",
        shown: "text[]",
        length: -1,
        by_value: false,
        kind: 'b',
        category: 'A',
        preferred: false,
        element: 25,
        array: 0,
        align: 'i',
        storage: 'x',
        collation: 100,
    },
    BuiltinType {
        oid: 1011,
        typname: "_xid",
        shown: "xid[]",
        length: -1,
        by_value: false,
        kind: 'b',
        category: 'A',
        preferred: false,
        element: 28,
        array: 0,
        align: 'i',
        storage: 'x',
        collation: 0,
    },
    BuiltinType {
        oid: 1013,
        typname: "_oidvector",
        shown: "oidvector[]",
        length: -1,
        by_value: false,
        kind: 'b',
        category: 'A',
        preferred: false,
        element: 30,
        array: 0,
        align: 'i',
        storage: 'x',
        collation: 0,
    },
    BuiltinType {
        oid: 1014,
        typname: "_bpchar",
        shown: "character[]",
        length: -1,
        by_value: false,
        kind: 'b',
        category: 'A',
        preferred: false,
        element: 1042,
        array: 0,
        align: 'i',
        storage: 'x',
        collation: 100,
    },
    BuiltinType {
        oid: 1015,
        typname: "_varchar",
        shown: "character varying[]",
        length: -1,
        by_value: false,
        kind: 'b',
        category: 'A',
        preferred: false,
        element: 1043,
        array: 0,
        align: 'i',
        storage: 'x',
        collation: 100,
    },
    BuiltinType {
        oid: 1016,
        typname: "_int8",
        shown: "bigint[]",
        length: -1,
        by_value: false,
        kind: 'b',
        category: 'A',
        preferred: false,
        element: 20,
        array: 0,
        align: 'd',
        storage: 'x',
        collation: 0,
    },
    BuiltinType {
        oid: 1021,
        typname: "_float4",
        shown: "real[]",
        length: -1,
        by_value: false,
        kind: 'b',
        category: 'A',
        preferred: false,
        element: 700,
        array: 0,
        align: 'i',
        storage: 'x',
        collation: 0,
    },
    BuiltinType {
        oid: 1022,
        typname: "_float8",
        shown: "double precision[]",
        length: -1,
        by_value: false,
        kind: 'b',
        category: 'A',
        preferred: false,
        element: 701,
        array: 0,
        align: 'd',
        storage: 'x',
        collation: 0,
    },
    BuiltinType {
        oid: 1028,
        typname: "_oid",
        shown: "oid[]",
        length: -1,
        by_value: false,
        kind: 'b',
        category: 'A',
        preferred: false,
        element: 26,
        array: 0,
        align: 'i',
        storage: 'x',
        collation: 0,
    },
    BuiltinType {
        oid: 1033,
        typname: "aclitem",
        shown: "aclitem",
        length: 16,
        by_value: false,
        kind: 'b',
        category: 'U',
        preferred: false,
        element: 0,
        array: 1034,
        align: 'd',
        storage: 'p',
        collation: 0,
    },
    BuiltinType {
        oid: 1034,
        typname: "_aclitem",
        shown: "aclitem[]",
        length: -1,
        by_value: false,
        kind: 'b',
        category: 'A',
        preferred: false,
        element: 1033,
        array: 0,
        align: 'd',
        storage: 'x',
        collation: 0,
    },
    BuiltinType {
        oid: 1042,
        typname: "bpchar",
        shown: "character",
        length: -1,
        by_value: false,
        kind: 'b',
        category: 'S',
        preferred: false,
        element: 0,
        array: 1014,
        align: 'i',
        storage: 'x',
        collation: 100,
    },
    BuiltinType {
        oid: 1043,
        typname: "varchar",
        shown: "character varying",
        length: -1,
        by_value: false,
        kind: 'b',
        category: 'S',
        preferred: false,
        element: 0,
        array: 1015,
        align: 'i',
        storage: 'x',
        collation: 100,
    },
    BuiltinType {
        oid: 1082,
        typname: "date",
        shown: "date",
        length: 4,
        by_value: true,
        kind: 'b',
        category: 'D',
        preferred: false,
        element: 0,
        array: 1182,
        align: 'i',
        storage: 'p',
        collation: 0,
    },
    BuiltinType {
        oid: 1083,
        typname: "time",
        shown: "time without time zone",
        length: 8,
        by_value: true,
        kind: 'b',
        category: 'D',
        preferred: false,
        element: 0,
        array: 1183,
        align: 'd',
        storage: 'p',
        collation: 0,
    },
    BuiltinType {
        oid: 1114,
        typname: "timestamp",
        shown: "timestamp without time zone",
        length: 8,
        by_value: true,
        kind: 'b',
        category: 'D',
        preferred: false,
        element: 0,
        array: 1115,
        align: 'd',
        storage: 'p',
        collation: 0,
    },
    BuiltinType {
        oid: 1115,
        typname: "_timestamp",
        shown: "timestamp without time zone[]",
        length: -1,
        by_value: false,
        kind: 'b',
        category: 'A',
        preferred: false,
        element: 1114,
        array: 0,
        align: 'd',
        storage: 'x',
        collation: 0,
    },
    BuiltinType {
        oid: 1182,
        typname: "_date",
        shown: "date[]",
        length: -1,
        by_value: false,
        kind: 'b',
        category: 'A',
        preferred: false,
        element: 1082,
        array: 0,
        align: 'i',
        storage: 'x',
        collation: 0,
    },
    BuiltinType {
        oid: 1183,
        typname: "_time",
        shown: "time without time zone[]",
        length: -1,
        by_value: false,
        kind: 'b',
        category: 'A',
        preferred: false,
        element: 1083,
        array: 0,
        align: 'd',
        storage: 'x',
        collation: 0,
    },
    BuiltinType {
        oid: 1184,
        typname: "timestamptz",
        shown: "timestamp with time zone",
        length: 8,
        by_value: true,
        kind: 'b',
        category: 'D',
        preferred: true,
        element: 0,
        array: 1185,
        align: 'd',
        storage: 'p',
        collation: 0,
    },
    BuiltinType {
        oid: 1185,
        typname: "_timestamptz",
        shown: "timestamp with time zone[]",
        length: -1,
        by_value: false,
        kind: 'b',
        category: 'A',
        preferred: false,
        element: 1184,
        array: 0,
        align: 'd',
        storage: 'x',
        collation: 0,
    },
    BuiltinType {
        oid: 1186,
        typname: "interval",
        shown: "interval",
        length: 16,
        by_value: false,
        kind: 'b',
        category: 'T',
        preferred: true,
        element: 0,
        array: 1187,
        align: 'd',
        storage: 'p',
        collation: 0,
    },
    BuiltinType {
        oid: 1187,
        typname: "_interval",
        shown: "interval[]",
        length: -1,
        by_value: false,
        kind: 'b',
        category: 'A',
        preferred: false,
        element: 1186,
        array: 0,
        align: 'd',
        storage: 'x',
        collation: 0,
    },
    BuiltinType {
        oid: 1231,
        typname: "_numeric",
        shown: "numeric[]",
        length: -1,
        by_value: false,
        kind: 'b',
        category: 'A',
        preferred: false,
        element: 1700,
        array: 0,
        align: 'i',
        storage: 'x',
        collation: 0,
    },
    BuiltinType {
        oid: 1266,
        typname: "timetz",
        shown: "time with time zone",
        length: 12,
        by_value: false,
        kind: 'b',
        category: 'D',
        preferred: false,
        element: 0,
        array: 1270,
        align: 'd',
        storage: 'p',
        collation: 0,
    },
    BuiltinType {
        oid: 1270,
        typname: "_timetz",
        shown: "time with time zone[]",
        length: -1,
        by_value: false,
        kind: 'b',
        category: 'A',
        preferred: false,
        element: 1266,
        array: 0,
        align: 'd',
        storage: 'x',
        collation: 0,
    },
    BuiltinType {
        oid: 1700,
        typname: "numeric",
        shown: "numeric",
        length: -1,
        by_value: false,
        kind: 'b',
        category: 'N',
        preferred: false,
        element: 0,
        array: 1231,
        align: 'i',
        storage: 'm',
        collation: 0,
    },
    BuiltinType {
        oid: 2205,
        typname: "regclass",
        shown: "regclass",
        length: 4,
        by_value: true,
        kind: 'b',
        category: 'N',
        preferred: false,
        element: 0,
        array: 2210,
        align: 'i',
        storage: 'p',
        collation: 0,
    },
    BuiltinType {
        oid: 2206,
        typname: "regtype",
        shown: "regtype",
        length: 4,
        by_value: true,
        kind: 'b',
        category: 'N',
        preferred: false,
        element: 0,
        array: 2211,
        align: 'i',
        storage: 'p',
        collation: 0,
    },
    BuiltinType {
        oid: 2210,
        typname: "_regclass",
        shown: "regclass[]",
        length: -1,
        by_value: false,
        kind: 'b',
        category: 'A',
        preferred: false,
        element: 2205,
        array: 0,
        align: 'i',
        storage: 'x',
        collation: 0,
    },
    BuiltinType {
        oid: 2211,
        typname: "_regtype",
        shown: "regtype[]",
        length: -1,
        by_value: false,
        kind: 'b',
        category: 'A',
        preferred: false,
        element: 2206,
        array: 0,
        align: 'i',
        storage: 'x',
        collation: 0,
    },
    BuiltinType {
        oid: 2277,
        typname: "anyarray",
        shown: "anyarray",
        length: -1,
        by_value: false,
        kind: 'p',
        category: 'P',
        preferred: false,
        element: 0,
        array: 0,
        align: 'd',
        storage: 'x',
        collation: 0,
    },
    BuiltinType {
        oid: 2950,
        typname: "uuid",
        shown: "uuid",
        length: 16,
        by_value: false,
        kind: 'b',
        category: 'U',
        preferred: false,
        element: 0,
        array: 2951,
        align: 'c',
        storage: 'p',
        collation: 0,
    },
    BuiltinType {
        oid: 2951,
        typname: "_uuid",
        shown: "uuid[]",
        length: -1,
        by_value: false,
        kind: 'b',
        category: 'A',
        preferred: false,
        element: 2950,
        array: 0,
        align: 'i',
        storage: 'x',
        collation: 0,
    },
    BuiltinType {
        oid: 3802,
        typname: "jsonb",
        shown: "jsonb",
        length: -1,
        by_value: false,
        kind: 'b',
        category: 'U',
        preferred: false,
        element: 0,
        array: 3807,
        align: 'i',
        storage: 'x',
        collation: 0,
    },
    BuiltinType {
        oid: 3807,
        typname: "_jsonb",
        shown: "jsonb[]",
        length: -1,
        by_value: false,
        kind: 'b',
        category: 'A',
        preferred: false,
        element: 3802,
        array: 0,
        align: 'i',
        storage: 'x',
        collation: 0,
    },
    BuiltinType {
        oid: 4089,
        typname: "regnamespace",
        shown: "regnamespace",
        length: 4,
        by_value: true,
        kind: 'b',
        category: 'N',
        preferred: false,
        element: 0,
        array: 4090,
        align: 'i',
        storage: 'p',
        collation: 0,
    },
    BuiltinType {
        oid: 4090,
        typname: "_regnamespace",
        shown: "regnamespace[]",
        length: -1,
        by_value: false,
        kind: 'b',
        category: 'A',
        preferred: false,
        element: 4089,
        array: 0,
        align: 'i',
        storage: 'x',
        collation: 0,
    },
    BuiltinType {
        oid: 4096,
        typname: "regrole",
        shown: "regrole",
        length: 4,
        by_value: true,
        kind: 'b',
        category: 'N',
        preferred: false,
        element: 0,
        array: 4097,
        align: 'i',
        storage: 'p',
        collation: 0,
    },
    BuiltinType {
        oid: 4097,
        typname: "_regrole",
        shown: "regrole[]",
        length: -1,
        by_value: false,
        kind: 'b',
        category: 'A',
        preferred: false,
        element: 4096,
        array: 0,
        align: 'i',
        storage: 'x',
        collation: 0,
    },
];

pub(crate) fn builtin_type(oid: u32) -> Option<&'static BuiltinType> {
    BUILTIN_TYPES
        .binary_search_by_key(&oid, |found| found.oid)
        .ok()
        .and_then(|index| BUILTIN_TYPES.get(index))
}

pub(crate) fn builtin_type_named(name: &str) -> Option<&'static BuiltinType> {
    let name = match name {
        "int" => "int4",
        "float" => "float8",
        "decimal" => "numeric",
        "character" | "char" => "bpchar",
        other => other,
    };
    BUILTIN_TYPES
        .iter()
        .find(|found| found.typname == name || found.shown == name)
}

pub(crate) fn type_oid(sql_type: SqlType) -> u32 {
    match sql_type {
        SqlType::SmallInt => 21,
        SqlType::Integer => 23,
        SqlType::BigInt => 20,
        SqlType::Real => 700,
        SqlType::Double => 701,
        SqlType::Boolean => 16,
        SqlType::Text => 25,
        SqlType::Varchar(_) => 1043,
        SqlType::Numeric(_) => 1700,
        SqlType::Date => 1082,
        SqlType::Timestamp(_) => 1114,
        SqlType::TimestampTz(_) => 1184,
        SqlType::Json => 114,
        SqlType::Jsonb => 3802,
        SqlType::Time(_) => 1083,
        SqlType::TimeTz(_) => 1266,
        SqlType::Interval { .. } => 1186,
        SqlType::Bytea => 17,
        SqlType::Uuid => 2950,
        SqlType::Bpchar(_) => 1042,
        SqlType::Catalog(kind) => catalog_type_oid(kind),
        SqlType::Array(element) => array_type_oid(element),
    }
}

fn catalog_type_oid(kind: CatalogType) -> u32 {
    match kind {
        CatalogType::Oid => 26,
        CatalogType::Name => 19,
        CatalogType::InternalChar => 18,
        CatalogType::NodeTree => 194,
        CatalogType::Xid => 28,
        CatalogType::AclItem => 1033,
        CatalogType::AnyArray => 2277,
        CatalogType::Reg(RegKind::Class) => 2205,
        CatalogType::Reg(RegKind::Type) => 2206,
        CatalogType::Reg(RegKind::Namespace) => 4089,
        CatalogType::Reg(RegKind::Role) => 4096,
        CatalogType::Reg(RegKind::Proc) => 24,
    }
}

fn array_type_oid(element: ArrayElement) -> u32 {
    match element {
        ArrayElement::Int2Vector => 22,
        ArrayElement::OidVector => 30,
        other => builtin_type(type_oid(other.sql_type())).map_or(0, |found| found.array),
    }
}

fn interval_range(fields: IntervalFields) -> i32 {
    const YEAR: i32 = 4;
    const MONTH: i32 = 2;
    const DAY: i32 = 8;
    const HOUR: i32 = 1024;
    const MINUTE: i32 = 2048;
    const SECOND: i32 = 4096;
    match fields {
        IntervalFields::Year => YEAR,
        IntervalFields::Month => MONTH,
        IntervalFields::Day => DAY,
        IntervalFields::Hour => HOUR,
        IntervalFields::Minute => MINUTE,
        IntervalFields::Second => SECOND,
        IntervalFields::YearToMonth => YEAR | MONTH,
        IntervalFields::DayToHour => DAY | HOUR,
        IntervalFields::DayToMinute => DAY | HOUR | MINUTE,
        IntervalFields::DayToSecond => DAY | HOUR | MINUTE | SECOND,
        IntervalFields::HourToMinute => HOUR | MINUTE,
        IntervalFields::HourToSecond => HOUR | MINUTE | SECOND,
        IntervalFields::MinuteToSecond => MINUTE | SECOND,
    }
}

const FULL_INTERVAL_RANGE: i32 = 0x7FFF;
const NO_INTERVAL_PRECISION: i32 = 0xFFFF;

pub(crate) fn type_modifier(sql_type: SqlType) -> i32 {
    let length = |length: u32| i32::try_from(length).map_or(-1, |length| length.saturating_add(4));
    match sql_type {
        SqlType::Varchar(Some(limit)) | SqlType::Bpchar(Some(limit)) => length(limit),
        SqlType::Numeric(Some(modifier)) => {
            ((i32::from(modifier.precision) << 16) | (i32::from(modifier.scale) & 0xFFFF)) + 4
        }
        SqlType::Timestamp(Some(precision))
        | SqlType::TimestampTz(Some(precision))
        | SqlType::Time(Some(precision))
        | SqlType::TimeTz(Some(precision)) => i32::from(precision),
        SqlType::Interval {
            fields: None,
            precision: None,
        } => -1,
        SqlType::Interval { fields, precision } => {
            let range = fields.map_or(FULL_INTERVAL_RANGE, interval_range);
            (range << 16) | precision.map_or(NO_INTERVAL_PRECISION, i32::from)
        }
        _ => -1,
    }
}

fn modified_type(oid: u32, modifier: i32) -> Option<SqlType> {
    if modifier < 0 {
        return None;
    }
    let precision = || {
        u8::try_from(modifier)
            .ok()
            .filter(|precision| *precision <= 6)
    };
    Some(match oid {
        1043 => SqlType::Varchar(Some(u32::try_from(modifier - 4).ok()?)),
        1042 => SqlType::Bpchar(Some(u32::try_from(modifier - 4).ok()?)),
        1700 => {
            let packed = modifier - 4;
            SqlType::Numeric(Some(NumericModifier {
                precision: u16::try_from(packed >> 16).ok()?,
                scale: (packed & 0xFFFF) as u16 as i16,
            }))
        }
        1083 => SqlType::Time(Some(precision()?)),
        1266 => SqlType::TimeTz(Some(precision()?)),
        1114 => SqlType::Timestamp(Some(precision()?)),
        1184 => SqlType::TimestampTz(Some(precision()?)),
        1186 => {
            let range = modifier >> 16;
            let digits = modifier & 0xFFFF;
            let fields = if range == FULL_INTERVAL_RANGE {
                None
            } else {
                Some(
                    IntervalFields::ALL
                        .into_iter()
                        .find(|fields| interval_range(*fields) == range)?,
                )
            };
            let precision = if digits == NO_INTERVAL_PRECISION {
                None
            } else {
                Some(u8::try_from(digits).ok().filter(|digits| *digits <= 6)?)
            };
            SqlType::Interval { fields, precision }
        }
        _ => return None,
    })
}

pub(crate) fn format_type(oid: u32, modifier: Option<i32>) -> String {
    let Some(found) = builtin_type(oid) else {
        return "???".to_owned();
    };
    if oid == 1042 && modifier == Some(-1) {
        return "bpchar".to_owned();
    }
    let modifier = modifier.unwrap_or(-1);
    if found.category == 'A'
        && let Some(element) = builtin_type(found.element)
        && element.array == oid
    {
        return format!("{}[]", format_type(element.oid, Some(modifier)));
    }
    match modified_type(oid, modifier) {
        Some(sql_type) => sql_type.name(),
        None => found.shown.to_owned(),
    }
}

fn fnv1a(parts: &[&[u8]]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for (index, part) in parts.iter().enumerate() {
        if index > 0 {
            hash ^= 0xFF;
            hash = hash.wrapping_mul(0x0100_0193);
        }
        for byte in *part {
            hash ^= u32::from(*byte);
            hash = hash.wrapping_mul(0x0100_0193);
        }
    }
    hash
}

fn folded(hash: u32) -> u32 {
    let span = u64::from(u32::MAX) - u64::from(FIRST_USER_OID) + 1;
    (u64::from(FIRST_USER_OID) + u64::from(hash) % span) as u32
}

fn next(oid: u32) -> u32 {
    if oid == u32::MAX {
        FIRST_USER_OID
    } else {
        oid + 1
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Space {
    Relation,
    Constraint,
    Namespace,
    AttrDef,
}

impl Space {
    fn tag(self) -> &'static [u8] {
        match self {
            Self::Relation => b"relation",
            Self::Constraint => b"constraint",
            Self::Namespace => b"namespace",
            Self::AttrDef => b"attrdef",
        }
    }
}

pub(crate) struct OidRequest {
    pub(crate) kind: u8,
    pub(crate) owner: String,
    pub(crate) name: String,
}

pub(crate) fn assign(space: Space, requests: &[OidRequest]) -> Vec<u32> {
    let mut order: Vec<usize> = (0..requests.len()).collect();
    order.sort_by(|&a, &b| {
        let (left, right) = (&requests[a], &requests[b]);
        (left.kind, &left.owner, &left.name).cmp(&(right.kind, &right.owner, &right.name))
    });
    let mut taken: HashSet<u32> = HashSet::with_capacity(requests.len());
    let mut assigned = vec![0; requests.len()];
    for index in order {
        let request = &requests[index];
        let mut oid = folded(fnv1a(&[
            space.tag(),
            &[request.kind],
            request.owner.as_bytes(),
            request.name.as_bytes(),
        ]));
        while !taken.insert(oid) {
            oid = next(oid);
        }
        assigned[index] = oid;
    }
    assigned
}
