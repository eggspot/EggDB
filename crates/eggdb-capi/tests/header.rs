//! `include/eggdb.h` against the Rust code it declares: every constant with its value, every
//! exported function with its number of parameters, and every struct with its fields' names and
//! types in order, so the header cannot drift from the library.

use eggdb_capi::{
    EGGDB_BLOB, EGGDB_DURABILITY_FULL, EGGDB_DURABILITY_NORMAL, EGGDB_DURABILITY_OFF,
    EGGDB_ERR_ALREADY_EXISTS, EGGDB_ERR_CACHE_FULL, EGGDB_ERR_COLLECTION_NOT_FOUND,
    EGGDB_ERR_CORRUPT, EGGDB_ERR_DATABASE_FULL, EGGDB_ERR_IN_USE, EGGDB_ERR_INCOMPATIBLE,
    EGGDB_ERR_INTERNAL, EGGDB_ERR_INVALID_ARGUMENT, EGGDB_ERR_INVALID_COLLECTION_NAME,
    EGGDB_ERR_INVALID_OPTION, EGGDB_ERR_IO, EGGDB_ERR_LOCKED, EGGDB_ERR_NEEDS_REOPEN,
    EGGDB_ERR_NOT_A_DATABASE, EGGDB_ERR_NOT_AN_INTEGER, EGGDB_ERR_NOT_FOUND, EGGDB_ERR_OTHER,
    EGGDB_ERR_OUT_OF_MEMORY, EGGDB_ERR_PERMISSION_DENIED, EGGDB_ERR_READ_ONLY,
    EGGDB_ERR_READ_TRANSACTION, EGGDB_ERR_RESERVED_KEY, EGGDB_ERR_SNAPSHOT_TOO_OLD,
    EGGDB_ERR_SQL, EGGDB_ERR_TOO_LARGE, EGGDB_ERR_TRANSACTION_FAILED, EGGDB_ERR_WRITER_ACTIVE,
    EGGDB_INTEGER, EGGDB_NULL, EGGDB_OK, EGGDB_OPEN_NO_CREATE, EGGDB_OPEN_READ_ONLY, EGGDB_REAL,
    EGGDB_SCAN_END_INCLUSIVE, EGGDB_SCAN_PREFIX, EGGDB_SCAN_REVERSE, EGGDB_TEXT,
};
use std::collections::BTreeMap;
use std::path::Path;

const CONSTANTS: [(&str, i64); 42] = [
    ("EGGDB_OK", EGGDB_OK as i64),
    ("EGGDB_ERR_NOT_FOUND", EGGDB_ERR_NOT_FOUND as i64),
    ("EGGDB_ERR_PERMISSION_DENIED", EGGDB_ERR_PERMISSION_DENIED as i64),
    ("EGGDB_ERR_LOCKED", EGGDB_ERR_LOCKED as i64),
    ("EGGDB_ERR_READ_ONLY", EGGDB_ERR_READ_ONLY as i64),
    ("EGGDB_ERR_NOT_A_DATABASE", EGGDB_ERR_NOT_A_DATABASE as i64),
    ("EGGDB_ERR_INCOMPATIBLE", EGGDB_ERR_INCOMPATIBLE as i64),
    ("EGGDB_ERR_CORRUPT", EGGDB_ERR_CORRUPT as i64),
    ("EGGDB_ERR_IO", EGGDB_ERR_IO as i64),
    ("EGGDB_ERR_OUT_OF_MEMORY", EGGDB_ERR_OUT_OF_MEMORY as i64),
    ("EGGDB_ERR_TOO_LARGE", EGGDB_ERR_TOO_LARGE as i64),
    ("EGGDB_ERR_CACHE_FULL", EGGDB_ERR_CACHE_FULL as i64),
    ("EGGDB_ERR_WRITER_ACTIVE", EGGDB_ERR_WRITER_ACTIVE as i64),
    ("EGGDB_ERR_SNAPSHOT_TOO_OLD", EGGDB_ERR_SNAPSHOT_TOO_OLD as i64),
    ("EGGDB_ERR_TRANSACTION_FAILED", EGGDB_ERR_TRANSACTION_FAILED as i64),
    ("EGGDB_ERR_NEEDS_REOPEN", EGGDB_ERR_NEEDS_REOPEN as i64),
    ("EGGDB_ERR_DATABASE_FULL", EGGDB_ERR_DATABASE_FULL as i64),
    ("EGGDB_ERR_INVALID_OPTION", EGGDB_ERR_INVALID_OPTION as i64),
    ("EGGDB_ERR_COLLECTION_NOT_FOUND", EGGDB_ERR_COLLECTION_NOT_FOUND as i64),
    ("EGGDB_ERR_INVALID_COLLECTION_NAME", EGGDB_ERR_INVALID_COLLECTION_NAME as i64),
    ("EGGDB_ERR_RESERVED_KEY", EGGDB_ERR_RESERVED_KEY as i64),
    ("EGGDB_ERR_ALREADY_EXISTS", EGGDB_ERR_ALREADY_EXISTS as i64),
    ("EGGDB_ERR_NOT_AN_INTEGER", EGGDB_ERR_NOT_AN_INTEGER as i64),
    ("EGGDB_ERR_OTHER", EGGDB_ERR_OTHER as i64),
    ("EGGDB_ERR_INVALID_ARGUMENT", EGGDB_ERR_INVALID_ARGUMENT as i64),
    ("EGGDB_ERR_IN_USE", EGGDB_ERR_IN_USE as i64),
    ("EGGDB_ERR_READ_TRANSACTION", EGGDB_ERR_READ_TRANSACTION as i64),
    ("EGGDB_ERR_SQL", EGGDB_ERR_SQL as i64),
    ("EGGDB_ERR_INTERNAL", EGGDB_ERR_INTERNAL as i64),
    ("EGGDB_OPEN_READ_ONLY", EGGDB_OPEN_READ_ONLY as i64),
    ("EGGDB_OPEN_NO_CREATE", EGGDB_OPEN_NO_CREATE as i64),
    ("EGGDB_DURABILITY_FULL", EGGDB_DURABILITY_FULL as i64),
    ("EGGDB_DURABILITY_NORMAL", EGGDB_DURABILITY_NORMAL as i64),
    ("EGGDB_DURABILITY_OFF", EGGDB_DURABILITY_OFF as i64),
    ("EGGDB_SCAN_REVERSE", EGGDB_SCAN_REVERSE as i64),
    ("EGGDB_SCAN_END_INCLUSIVE", EGGDB_SCAN_END_INCLUSIVE as i64),
    ("EGGDB_SCAN_PREFIX", EGGDB_SCAN_PREFIX as i64),
    ("EGGDB_NULL", EGGDB_NULL as i64),
    ("EGGDB_INTEGER", EGGDB_INTEGER as i64),
    ("EGGDB_REAL", EGGDB_REAL as i64),
    ("EGGDB_TEXT", EGGDB_TEXT as i64),
    ("EGGDB_BLOB", EGGDB_BLOB as i64),
];

fn crate_file(relative: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

fn header() -> String {
    crate_file("include/eggdb.h")
}

fn rust_sources() -> String {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files: Vec<_> = std::fs::read_dir(&directory)
        .unwrap_or_else(|error| panic!("{}: {error}", directory.display()))
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|extension| extension == "rs"))
        .collect();
    files.sort();
    files
        .iter()
        .map(|path| {
            std::fs::read_to_string(path)
                .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn identifier_at(text: &str, start: usize) -> &str {
    let length = text[start..]
        .bytes()
        .take_while(|&byte| is_identifier_byte(byte))
        .count();
    &text[start..start + length]
}

fn inside_parentheses(text: &str, open: usize) -> &str {
    let close = text[open..]
        .find(')')
        .map_or(text.len(), |offset| open + offset);
    &text[open + 1..close]
}

fn parameter_count(list: &str) -> usize {
    let list = list.trim();
    if list.is_empty() || list == "void" {
        return 0;
    }
    list.split(',').filter(|part| !part.trim().is_empty()).count()
}

fn header_defines() -> BTreeMap<String, i64> {
    header()
        .lines()
        .filter_map(|line| line.strip_prefix("#define EGGDB_"))
        .filter_map(|rest| {
            let mut words = rest.split_whitespace();
            let name = words.next()?;
            let value = words.next()?.parse().ok()?;
            Some((format!("EGGDB_{name}"), value))
        })
        .collect()
}

fn header_functions() -> BTreeMap<String, usize> {
    let header = header();
    let declarations: String = header
        .lines()
        .filter(|line| !line.starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    let mut functions = BTreeMap::new();
    let mut statement_start = 0;
    for (index, byte) in declarations.bytes().enumerate() {
        if byte != b';' && byte != b'}' && byte != b'{' {
            continue;
        }
        let statement = declarations[statement_start..index].trim();
        statement_start = index + 1;
        if statement.starts_with("typedef") || statement.starts_with("static") {
            continue;
        }
        let Some(open) = statement.find('(') else {
            continue;
        };
        let before = statement[..open].trim_end();
        let name_start = before
            .rfind(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
            .map_or(0, |at| at + 1);
        let name = &before[name_start..];
        if name.starts_with("eggdb_") {
            let count = parameter_count(inside_parentheses(statement, open));
            functions.insert(name.to_owned(), count);
        }
    }
    functions
}

fn exported_functions() -> BTreeMap<String, usize> {
    let sources = rust_sources();
    let marker = "extern \"C\" fn ";
    let mut functions = BTreeMap::new();
    let mut from = 0;
    while let Some(offset) = sources[from..].find(marker) {
        let start = from + offset + marker.len();
        let name = identifier_at(&sources, start);
        let open = start + name.len();
        from = open;
        if sources.as_bytes().get(open) != Some(&b'(') {
            continue;
        }
        functions.insert(
            name.to_owned(),
            parameter_count(inside_parentheses(&sources, open)),
        );
    }
    functions
}

fn rust_constant_names() -> Vec<String> {
    let sources = rust_sources();
    let marker = "pub const ";
    let mut names = Vec::new();
    let mut from = 0;
    while let Some(offset) = sources[from..].find(marker) {
        let start = from + offset + marker.len();
        let name = identifier_at(&sources, start);
        from = start + name.len();
        if name.starts_with("EGGDB_") {
            names.push(name.to_owned());
        }
    }
    names.sort();
    names
}

fn rust_type_name(c_name: &str) -> String {
    c_name
        .split('_')
        .map(|word| {
            let mut characters = word.chars();
            characters.next().map_or_else(String::new, |first| {
                first.to_ascii_uppercase().to_string() + characters.as_str()
            })
        })
        .collect()
}

fn rust_type_of(c_type: &str) -> String {
    let c_type = c_type.split_whitespace().collect::<Vec<_>>().join(" ");
    let pointers = c_type.matches('*').count();
    let base = c_type
        .replace('*', " ")
        .replace("const", " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let base = match base.as_str() {
        "uint8_t" => "u8".to_owned(),
        "uint32_t" => "u32".to_owned(),
        "uint64_t" => "u64".to_owned(),
        "int32_t" => "i32".to_owned(),
        "int64_t" => "i64".to_owned(),
        "size_t" => "usize".to_owned(),
        "double" => "f64".to_owned(),
        "int" => "c_int".to_owned(),
        "char" => "c_char".to_owned(),
        other => rust_type_name(other),
    };
    "*const ".repeat(pointers) + &base
}

fn header_structs() -> BTreeMap<String, Vec<(String, String)>> {
    let header = header();
    let mut structs = BTreeMap::new();
    let mut from = 0;
    let marker = "typedef struct ";
    while let Some(offset) = header[from..].find(marker) {
        let start = from + offset + marker.len();
        let name = identifier_at(&header, start);
        from = start + name.len();
        let rest = header[from..].trim_start();
        if !rest.starts_with('{') {
            continue;
        }
        let open = header.len() - rest.len();
        let close = header[open..]
            .find('}')
            .map_or(header.len(), |offset| open + offset);
        let fields = header[open + 1..close]
            .split(';')
            .map(str::trim)
            .filter(|field| !field.is_empty())
            .map(|field| {
                let name_start = field
                    .rfind(|character: char| {
                        !(character.is_ascii_alphanumeric() || character == '_')
                    })
                    .map_or(0, |at| at + 1);
                (
                    field[name_start..].to_owned(),
                    rust_type_of(&field[..name_start]),
                )
            })
            .collect();
        structs.insert(rust_type_name(name), fields);
        from = close;
    }
    structs
}

fn rust_struct_fields(name: &str) -> Vec<(String, String)> {
    let sources = rust_sources();
    let marker = format!("pub struct {name} {{");
    let start = sources
        .find(&marker)
        .unwrap_or_else(|| panic!("no `{marker}` in src"))
        + marker.len();
    let end = sources[start..]
        .find("\n}")
        .map_or(sources.len(), |offset| start + offset);
    sources[start..end]
        .lines()
        .map(str::trim)
        .filter_map(|line| line.strip_prefix("pub "))
        .filter_map(|field| {
            let (field_name, field_type) = field.split_once(':')?;
            let field_type = field_type.trim().trim_end_matches(',');
            Some((
                field_name.trim().to_owned(),
                field_type.replace("*mut ", "*const "),
            ))
        })
        .collect()
}

#[test]
fn every_constant_is_defined_with_its_value() {
    let defined = header_defines();
    for (name, value) in CONSTANTS {
        assert_eq!(defined.get(name), Some(&value), "{name} in eggdb.h");
    }
    let mut header_names: Vec<String> = defined.into_keys().collect();
    header_names.sort();
    assert_eq!(header_names, rust_constant_names());
}

#[test]
fn every_exported_function_is_declared_with_its_parameters() {
    let declared = header_functions();
    let exported = exported_functions();
    assert!(exported.len() >= 30, "found only {exported:?}");
    assert_eq!(declared, exported);
}

#[test]
fn every_struct_has_the_fields_of_its_rust_twin() {
    let structs = header_structs();
    let names: Vec<&str> = structs.keys().map(String::as_str).collect();
    assert_eq!(
        names,
        [
            "EggdbBackupReport",
            "EggdbCompactReport",
            "EggdbEntry",
            "EggdbOptions",
            "EggdbParameter",
            "EggdbResult",
            "EggdbValue",
        ]
    );
    for (name, fields) in &structs {
        assert_eq!(fields, &rust_struct_fields(name), "{name}");
    }
}

#[test]
fn the_type_names_translate_as_the_header_spells_them() {
    assert_eq!(rust_type_of("const char *const *"), "*const *const c_char");
    assert_eq!(rust_type_of("const eggdb_value *"), "*const EggdbValue");
    assert_eq!(rust_type_of("uint64_t "), "u64");
    assert_eq!(parameter_count("void"), 0);
    assert_eq!(parameter_count("eggdb_db *db,\n    uint64_t *removed,\n"), 2);
}
