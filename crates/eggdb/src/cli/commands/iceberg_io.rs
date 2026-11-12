use super::Session;
use super::avro_io::columns_of as avro_columns;
use crate::cli::args::CommandName;
use crate::cli::failure::{Exit, Failure};
use crate::cli::render;
use eggdb_parquet::{Cell, Column};
use eggdb_sql::json::{Json, Node, parse_document};
use std::path::{Path, PathBuf};

const MOST_METADATA_BYTES: u64 = 64 << 20;
const MOST_DATA_FILE_BYTES: u64 = 4 << 30;
const MOST_DATA_FILES: usize = 100_000;
const DELETED: i64 = 2;

fn member<'n>(node: &'n Node, name: &str) -> Option<&'n Node> {
    match &node.value {
        Json::Object(members) => members.iter().find(|(key, _)| key == name).map(|(_, value)| value),
        _ => None,
    }
}

fn text_member(node: &Node, name: &str) -> Option<String> {
    match &member(node, name)?.value {
        Json::String(text) => Some(text.clone()),
        Json::Number(number) => Some(number.clone()),
        _ => None,
    }
}

fn leading_number(name: &str) -> Option<u64> {
    let digits: String = name.trim_start_matches('v').chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

fn metadata_file(location: &Path) -> Result<PathBuf, String> {
    if location.is_file() {
        return Ok(location.to_path_buf());
    }
    let directory = location.join("metadata");
    if let Ok(hint) = std::fs::read_to_string(directory.join("version-hint.text")) {
        let candidate = directory.join(format!("v{}.metadata.json", hint.trim()));
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    let entries = std::fs::read_dir(&directory).map_err(|error| format!("cannot list {}: {error}", directory.display()))?;
    entries
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".metadata.json"))
        .filter_map(|name| leading_number(&name).map(|number| (number, name)))
        .max()
        .map(|(_, name)| directory.join(name))
        .ok_or_else(|| format!("{} holds no Iceberg metadata file", directory.display()))
}

fn read_limited(path: &Path, most: u64, what: &str) -> Result<Vec<u8>, String> {
    let size = std::fs::metadata(path).map_err(|error| format!("cannot read the {what} {}: {error}", path.display()))?.len();
    if size > most {
        return Err(format!("the {what} {} is larger than the import reads at once", path.display()));
    }
    std::fs::read(path).map_err(|error| format!("cannot read the {what} {}: {error}", path.display()))
}

struct Table {
    written_location: String,
    location: PathBuf,
}

impl Table {
    fn local(&self, uri: &str) -> Result<PathBuf, String> {
        let path = uri.strip_prefix("file://").or_else(|| uri.strip_prefix("file:")).unwrap_or(uri);
        if path.contains("://") {
            return Err(format!("{uri} is not a local file; EggDB reads Iceberg tables from the file system"));
        }
        let direct = PathBuf::from(path);
        if direct.exists() {
            return Ok(direct);
        }
        let written = self
            .written_location
            .strip_prefix("file://")
            .or_else(|| self.written_location.strip_prefix("file:"))
            .unwrap_or(&self.written_location);
        match path.strip_prefix(written) {
            Some(rest) => Ok(self.location.join(rest.trim_start_matches('/'))),
            None => Ok(direct),
        }
    }
}

fn column<'c>(columns: &'c [Column], name: &str) -> Option<&'c Column> {
    columns.iter().find(|column| column.name == name)
}

fn cell_text(cell: &Cell) -> Option<String> {
    match cell {
        Cell::Bytes(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
        Cell::Int(number) => Some(number.to_string()),
        _ => None,
    }
}

fn manifest_paths(table: &Table, metadata: &Node) -> Result<Vec<PathBuf>, String> {
    let current = text_member(metadata, "current-snapshot-id").unwrap_or_else(|| "-1".to_owned());
    if current == "-1" {
        return Ok(Vec::new());
    }
    let Some(Json::Array(snapshots)) = member(metadata, "snapshots").map(|node| &node.value) else {
        return Err("the metadata lists no snapshots".to_owned());
    };
    let snapshot = snapshots
        .iter()
        .find(|snapshot| text_member(snapshot, "snapshot-id").as_deref() == Some(current.as_str()))
        .ok_or_else(|| format!("the metadata has no snapshot {current}"))?;
    if let Some(list) = text_member(snapshot, "manifest-list") {
        let bytes = read_limited(&table.local(&list)?, MOST_METADATA_BYTES, "manifest list")?;
        let columns = avro_columns(&bytes)?;
        if let Some(content) = column(&columns, "content")
            && content.cells.iter().any(|cell| matches!(cell, Cell::Int(1)))
        {
            return Err("the snapshot has delete files (format version 2 row-level deletes), which the import does not apply yet".to_owned());
        }
        let paths = column(&columns, "manifest_path").ok_or("the manifest list has no manifest_path")?;
        return paths.cells.iter().filter_map(cell_text).map(|path| table.local(&path)).collect();
    }
    match member(snapshot, "manifests").map(|node| &node.value) {
        Some(Json::Array(manifests)) => manifests
            .iter()
            .filter_map(|node| match &node.value {
                Json::String(path) => Some(table.local(path)),
                _ => None,
            })
            .collect(),
        _ => Err("the snapshot names no manifest list or manifests".to_owned()),
    }
}

fn data_files(table: &Table, manifest: &Path) -> Result<Vec<PathBuf>, String> {
    let bytes = read_limited(manifest, MOST_METADATA_BYTES, "manifest")?;
    let columns = avro_columns(&bytes)?;
    let statuses = column(&columns, "status").ok_or("a manifest has no status")?;
    let files = column(&columns, "data_file").ok_or("a manifest has no data_file")?;
    let mut paths = Vec::new();
    for (status, file) in statuses.cells.iter().zip(&files.cells) {
        if matches!(status, Cell::Int(DELETED)) {
            continue;
        }
        let Some(text) = cell_text(file) else {
            continue;
        };
        let document = parse_document(&text).map_err(|error| format!("a manifest entry: {}", error.message))?;
        if text_member(&document, "content").is_some_and(|content| content != "0") {
            return Err("the snapshot has delete files, which the import does not apply yet".to_owned());
        }
        let format = text_member(&document, "file_format").unwrap_or_default();
        if !format.eq_ignore_ascii_case("parquet") {
            return Err(format!("a data file is {format}; the import reads Parquet data files"));
        }
        let path = text_member(&document, "file_path").ok_or("a manifest entry has no file_path")?;
        paths.push(table.local(&path)?);
    }
    Ok(paths)
}

fn appended(mut merged: Vec<Column>, more: Vec<Column>) -> Vec<Column> {
    let before = merged.first().map_or(0, |column| column.cells.len());
    let added = more.first().map_or(0, |column| column.cells.len());
    for column in &more {
        if !merged.iter().any(|known| known.name == column.name) {
            merged.push(Column {
                name: column.name.clone(),
                column_type: column.column_type,
                nullable: true,
                cells: vec![Cell::Null; before],
            });
        }
    }
    for known in &mut merged {
        match more.iter().find(|column| column.name == known.name) {
            Some(column) => known.cells.extend(column.cells.iter().cloned()),
            None => known.cells.extend(std::iter::repeat_n(Cell::Null, added)),
        }
    }
    merged
}

/// The rows of the Iceberg table at `location` (its directory, or one of its metadata files)
/// as of its current snapshot, as table columns.
fn table_columns(location: &Path) -> Result<Vec<Column>, String> {
    let metadata_path = metadata_file(location)?;
    let bytes = read_limited(&metadata_path, MOST_METADATA_BYTES, "metadata file")?;
    let text = String::from_utf8(bytes).map_err(|_| "the metadata file is not UTF-8".to_owned())?;
    let metadata = parse_document(&text).map_err(|error| format!("the metadata file: {}", error.message))?;
    let table_location = if location.is_file() {
        metadata_path.parent().and_then(Path::parent).map(Path::to_path_buf).unwrap_or_default()
    } else {
        location.to_path_buf()
    };
    let table = Table {
        written_location: text_member(&metadata, "location").unwrap_or_default(),
        location: table_location,
    };
    let mut files = Vec::new();
    for manifest in manifest_paths(&table, &metadata)? {
        files.extend(data_files(&table, &manifest)?);
        if files.len() > MOST_DATA_FILES {
            return Err(format!("the table has more than {MOST_DATA_FILES} data files"));
        }
    }
    let mut merged = Vec::new();
    for file in files {
        let bytes = read_limited(&file, MOST_DATA_FILE_BYTES, "data file")?;
        let columns = eggdb_parquet::read(&bytes).map_err(|error| format!("{}: {}", file.display(), error.0))?;
        merged = appended(merged, columns);
    }
    Ok(merged)
}

impl Session<'_> {
    pub(super) fn import_iceberg(&mut self, location: &Path) -> Result<Exit, Failure> {
        let shown = render::text(location.display().to_string().as_bytes());
        let table = self.request.csv.table.clone().unwrap_or_default();
        if table.is_empty() {
            return Err(Failure::usage(
                "eggdb import --iceberg needs --table, the table to fill".to_owned(),
                Some(CommandName::Import),
            ));
        }
        let columns = table_columns(location)
            .map_err(|error| Failure::database(format!("cannot import {shown}: {error}"), Exit::DatabaseError))?;
        self.import_columns(&shown, &table, &columns)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_files_merge_by_column_name_and_paths_follow_a_moved_table() {
        let first = vec![Column {
            name: "id".to_owned(),
            column_type: eggdb_parquet::ColumnType::Int64,
            nullable: true,
            cells: vec![Cell::Int(1)],
        }];
        let second = vec![
            Column { name: "id".to_owned(), column_type: eggdb_parquet::ColumnType::Int64, nullable: true, cells: vec![Cell::Int(2)] },
            Column {
                name: "note".to_owned(),
                column_type: eggdb_parquet::ColumnType::Text,
                nullable: true,
                cells: vec![Cell::Bytes(b"x".to_vec())],
            },
        ];
        let merged = appended(appended(Vec::new(), first), second);
        assert_eq!(merged[1].cells, [Cell::Null, Cell::Bytes(b"x".to_vec())]);
        let table = Table {
            written_location: "file:///warehouse/db/orders".to_owned(),
            location: PathBuf::from("/tmp/orders"),
        };
        assert_eq!(
            table.local("file:///warehouse/db/orders/data/00000.parquet").unwrap(),
            PathBuf::from("/tmp/orders/data/00000.parquet")
        );
        assert!(table.local("s3://bucket/orders/data.parquet").is_err());
        assert_eq!(leading_number("00012-uuid.metadata.json"), Some(12));
        assert_eq!(leading_number("v3.metadata.json"), Some(3));
    }
}
