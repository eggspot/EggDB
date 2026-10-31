use crate::{Cell, Column, ColumnType, read, write};

fn sample() -> Vec<Column> {
    let rows = 1_000;
    vec![
        Column { name: "id".to_owned(), column_type: ColumnType::Int64, nullable: false, cells: (0..rows).map(Cell::Int).collect() },
        Column {
            name: "name".to_owned(),
            column_type: ColumnType::Text,
            nullable: true,
            cells: (0..rows).map(|row| if row % 7 == 0 { Cell::Null } else { Cell::Bytes(format!("user {row}").into_bytes()) }).collect(),
        },
        Column { name: "active".to_owned(), column_type: ColumnType::Boolean, nullable: false, cells: (0..rows).map(|row| Cell::Boolean(row % 3 == 0)).collect() },
        Column { name: "score".to_owned(), column_type: ColumnType::Double, nullable: true, cells: (0..rows).map(|row| if row == 5 { Cell::Null } else { Cell::Float(row as f64 / 4.0) }).collect() },
        Column { name: "born".to_owned(), column_type: ColumnType::Date, nullable: false, cells: (0..rows).map(|row| Cell::Int(19_000 + row)).collect() },
        Column { name: "seen".to_owned(), column_type: ColumnType::Timestamp { utc: true }, nullable: false, cells: (0..rows).map(|row| Cell::Int(1_700_000_000_000_000 + row)).collect() },
        Column { name: "price".to_owned(), column_type: ColumnType::Decimal { precision: 10, scale: 2 }, nullable: false, cells: (0..rows).map(|row| Cell::Decimal(i128::from(row) * 101)).collect() },
        Column { name: "big".to_owned(), column_type: ColumnType::Decimal { precision: 30, scale: 0 }, nullable: false, cells: (0..rows).map(|row| Cell::Decimal(-i128::from(row) * 10i128.pow(25))).collect() },
        Column { name: "small".to_owned(), column_type: ColumnType::Int32, nullable: true, cells: (0..rows).map(|row| Cell::Int(-row)).collect() },
    ]
}

#[test]
fn a_written_file_reads_back_unchanged() {
    let columns = sample();
    let file = write(&columns).unwrap();
    assert!(file.starts_with(b"PAR1") && file.ends_with(b"PAR1"));
    assert_eq!(read(&file).unwrap(), columns);
    assert_eq!(read(&write(&[]).unwrap()).unwrap(), Vec::new());
}

#[test]
fn writing_refuses_values_that_do_not_fit() {
    let mut columns = sample();
    columns[0].cells[3] = Cell::Null;
    assert!(write(&columns).is_err());
    let mut columns = sample();
    columns[1].cells.pop();
    assert!(write(&columns).is_err());
    let mut columns = sample();
    columns[8].cells[0] = Cell::Int(i64::MAX);
    assert!(write(&columns).is_err());
}

#[test]
fn damaged_files_are_errors_not_panics() {
    let file = write(&sample()).unwrap();
    assert!(read(b"PAR1PAR1").is_err());
    assert!(read(b"not parquet at all").is_err());
    for cut in (0..file.len()).step_by(97) {
        let mut damaged = file[..cut].to_vec();
        damaged.extend_from_slice(&file[file.len() - 8..]);
        let _ = read(&damaged);
    }
    for at in (4..file.len() - 8).step_by(13) {
        let mut damaged = file.clone();
        damaged[at] ^= 0xa5;
        let _ = read(&damaged);
    }
    let mut huge_footer = file.clone();
    let length = huge_footer.len();
    huge_footer[length - 8..length - 4].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(read(&huge_footer).is_err());
}
