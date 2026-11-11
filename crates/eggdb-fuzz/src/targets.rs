use crate::mutate::Random;
use eggdb_parquet::{Cell, Column, ColumnType};
use std::panic::{AssertUnwindSafe, catch_unwind};

const CORPUS_SIZE: usize = 24;
const LARGEST_DECODED: usize = 1 << 20;

/// One decoder EggDB runs on untrusted bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Target {
    Zstd,
    Lz4,
    Sqlite,
    Arrow,
}

fn sample_text(random: &mut Random) -> Vec<u8> {
    let words: [&[u8]; 6] = [b"egg ", b"database ", b"row ", b"page ", b"0123 ", b"\xff\x00"];
    let mut text = Vec::new();
    for _ in 0..random.below(400) {
        text.extend_from_slice(words[random.below(words.len())]);
    }
    if random.below(3) == 0 {
        text.extend(random.bytes(random.below(200)));
    }
    text
}

fn guarded<T>(work: impl FnOnce() -> T) -> Result<T, String> {
    catch_unwind(AssertUnwindSafe(work)).map_err(|panic| {
        panic
            .downcast_ref::<&str>()
            .map(|text| (*text).to_owned())
            .or_else(|| panic.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "a panic".to_owned())
    })
}

impl Target {
    pub(crate) fn named(name: &str) -> Option<Self> {
        Some(match name {
            "zstd" => Self::Zstd,
            "lz4" => Self::Lz4,
            "sqlite" => Self::Sqlite,
            "arrow" => Self::Arrow,
            _ => return None,
        })
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Zstd => "zstd",
            Self::Lz4 => "lz4",
            Self::Sqlite => "sqlite",
            Self::Arrow => "arrow",
        }
    }

    /// Valid inputs to start mutating from.
    pub(crate) fn corpus(self, random: &mut Random) -> Vec<Vec<u8>> {
        (0..CORPUS_SIZE)
            .map(|_| match self {
                Self::Zstd => eggdb_storage::compress_zstd(&sample_text(random)),
                Self::Lz4 => eggdb_storage::compress_lz4_frame(&sample_text(random)),
                Self::Sqlite => sqlite_sample(random),
                Self::Arrow => arrow_sample(random),
            })
            .collect()
    }

    /// Decodes `input`, which must not panic; an input whose decoding succeeds is checked further
    /// where the format allows (a decompressed frame compresses and decompresses back).
    pub(crate) fn check(self, input: &[u8]) -> Result<(), String> {
        match self {
            Self::Zstd => {
                let decoded = guarded(|| eggdb_storage::decompress_zstd(input, LARGEST_DECODED))?;
                if let Ok(plain) = decoded {
                    let again = eggdb_storage::compress_zstd(&plain);
                    let back = guarded(|| eggdb_storage::decompress_zstd(&again, plain.len()))?;
                    if back.as_deref() != Ok(plain.as_slice()) {
                        return Err("a decoded frame does not round-trip".to_owned());
                    }
                }
                Ok(())
            }
            Self::Lz4 => {
                guarded(|| eggdb_storage::decompress_lz4_frame(input, LARGEST_DECODED))?;
                Ok(())
            }
            Self::Sqlite => {
                guarded(|| {
                    if let Ok(file) = eggdb_sqlite_format::SqliteFile::from_bytes(input.to_vec(), &[])
                        && let Ok(schema) = file.schema()
                    {
                        for entry in schema {
                            let _ = file.rows(entry.root_page);
                        }
                    }
                })?;
                Ok(())
            }
            Self::Arrow => {
                guarded(|| eggdb_arrow::read(input))?;
                Ok(())
            }
        }
    }
}

fn sqlite_sample(random: &mut Random) -> Vec<u8> {
    let rows = (0..random.below(300) as i64)
        .map(|rowid| eggdb_sqlite_format::SqliteRow {
            rowid,
            values: vec![
                eggdb_sqlite_format::SqliteValue::Integer(rowid * 7),
                eggdb_sqlite_format::SqliteValue::Text(String::from_utf8_lossy(&sample_text(random)).into_owned()),
            ],
        })
        .collect();
    let objects = vec![eggdb_sqlite_format::SqliteObject::Table {
        name: "t".to_owned(),
        sql: "CREATE TABLE t(a, b)".to_owned(),
        rows,
    }];
    eggdb_sqlite_format::write_database(&objects).unwrap_or_default()
}

fn arrow_sample(random: &mut Random) -> Vec<u8> {
    let count = random.below(50);
    let numbers = Column {
        name: "n".to_owned(),
        column_type: ColumnType::Int64,
        nullable: true,
        cells: (0..count).map(|at| if at % 5 == 0 { Cell::Null } else { Cell::Int(at as i64) }).collect(),
    };
    let texts = Column {
        name: "t".to_owned(),
        column_type: ColumnType::Text,
        nullable: false,
        cells: (0..count).map(|at| Cell::Bytes(format!("row {at}").into_bytes())).collect(),
    };
    eggdb_arrow::write_stream(&[numbers, texts]).unwrap_or_default()
}
