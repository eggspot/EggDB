use crate::native::Cell;

pub(crate) const SQL_C_CHAR: i16 = 1;
pub(crate) const SQL_C_WCHAR: i16 = -8;
pub(crate) const SQL_C_LONG: i16 = 4;
pub(crate) const SQL_C_SLONG: i16 = -16;
pub(crate) const SQL_C_SHORT: i16 = 5;
pub(crate) const SQL_C_SSHORT: i16 = -15;
pub(crate) const SQL_C_SBIGINT: i16 = -25;
pub(crate) const SQL_C_DOUBLE: i16 = 8;
pub(crate) const SQL_C_FLOAT: i16 = 7;
pub(crate) const SQL_C_BIT: i16 = -7;
pub(crate) const SQL_C_BINARY: i16 = -2;

pub(crate) enum Converted {
    Null,
    Fixed(Vec<u8>),
    Text(Vec<u8>),
    Wide(Vec<u8>),
    Binary(Vec<u8>),
}

fn text_of(cell: &Cell) -> Option<String> {
    match cell {
        Cell::Null => None,
        Cell::Integer(number) => Some(number.to_string()),
        Cell::Real(number) => Some(number.to_string()),
        Cell::Text(text) => Some(text.clone()),
        Cell::Blob(bytes) => Some(bytes.iter().map(|byte| format!("{byte:02X}")).collect()),
    }
}

fn integer_of(cell: &Cell) -> Option<i64> {
    match cell {
        Cell::Integer(number) => Some(*number),
        Cell::Real(number) => Some(*number as i64),
        Cell::Text(text) => text.trim().parse().ok(),
        Cell::Null | Cell::Blob(_) => None,
    }
}

fn real_of(cell: &Cell) -> Option<f64> {
    match cell {
        Cell::Integer(number) => Some(*number as f64),
        Cell::Real(number) => Some(*number),
        Cell::Text(text) => text.trim().parse().ok(),
        Cell::Null | Cell::Blob(_) => None,
    }
}

pub(crate) fn convert(cell: &Cell, target_type: i16) -> Result<Converted, &'static str> {
    if *cell == Cell::Null {
        return Ok(Converted::Null);
    }
    let not_a_number = "the value is not a number";
    Ok(match target_type {
        SQL_C_LONG | SQL_C_SLONG => {
            let number = integer_of(cell).ok_or(not_a_number)?;
            let narrow = i32::try_from(number).map_err(|_| "the value does not fit a 32-bit integer")?;
            Converted::Fixed(narrow.to_ne_bytes().to_vec())
        }
        SQL_C_SHORT | SQL_C_SSHORT => {
            let number = integer_of(cell).ok_or(not_a_number)?;
            let narrow = i16::try_from(number).map_err(|_| "the value does not fit a 16-bit integer")?;
            Converted::Fixed(narrow.to_ne_bytes().to_vec())
        }
        SQL_C_SBIGINT => Converted::Fixed(integer_of(cell).ok_or(not_a_number)?.to_ne_bytes().to_vec()),
        SQL_C_DOUBLE => Converted::Fixed(real_of(cell).ok_or(not_a_number)?.to_ne_bytes().to_vec()),
        SQL_C_FLOAT => Converted::Fixed((real_of(cell).ok_or(not_a_number)? as f32).to_ne_bytes().to_vec()),
        SQL_C_BIT => Converted::Fixed(vec![u8::from(integer_of(cell).ok_or(not_a_number)? != 0)]),
        SQL_C_BINARY => match cell {
            Cell::Blob(bytes) => Converted::Binary(bytes.clone()),
            other => Converted::Binary(text_of(other).unwrap_or_default().into_bytes()),
        },
        SQL_C_WCHAR => Converted::Wide(
            text_of(cell)
                .unwrap_or_default()
                .encode_utf16()
                .flat_map(u16::to_ne_bytes)
                .collect(),
        ),
        _ => match cell {
            Cell::Blob(bytes) if target_type == crate::handles::SQL_C_DEFAULT => Converted::Binary(bytes.clone()),
            other => Converted::Text(text_of(other).unwrap_or_default().into_bytes()),
        },
    })
}

pub(crate) fn parameter_text(value_type: i16, bytes: &[u8]) -> Result<String, &'static str> {
    let fixed = |size: usize| bytes.get(..size).ok_or("the parameter buffer is too short");
    Ok(match value_type {
        SQL_C_LONG | SQL_C_SLONG => i32::from_ne_bytes(fixed(4)?.try_into().map_err(|_| "bad buffer")?).to_string(),
        SQL_C_SHORT | SQL_C_SSHORT => i16::from_ne_bytes(fixed(2)?.try_into().map_err(|_| "bad buffer")?).to_string(),
        SQL_C_SBIGINT => i64::from_ne_bytes(fixed(8)?.try_into().map_err(|_| "bad buffer")?).to_string(),
        SQL_C_DOUBLE => f64::from_ne_bytes(fixed(8)?.try_into().map_err(|_| "bad buffer")?).to_string(),
        SQL_C_FLOAT => f32::from_ne_bytes(fixed(4)?.try_into().map_err(|_| "bad buffer")?).to_string(),
        SQL_C_BIT => u8::from(fixed(1)?[0] != 0).to_string(),
        SQL_C_WCHAR => {
            let units: Vec<u16> = bytes
                .chunks_exact(2)
                .map(|pair| u16::from_ne_bytes([pair[0], pair[1]]))
                .collect();
            String::from_utf16(&units).map_err(|_| "the parameter is not UTF-16")?
        }
        _ => String::from_utf8(bytes.to_vec()).map_err(|_| "the parameter is not UTF-8")?,
    })
}

pub(crate) fn fixed_size(value_type: i16) -> Option<usize> {
    match value_type {
        SQL_C_LONG | SQL_C_SLONG | SQL_C_FLOAT => Some(4),
        SQL_C_SHORT | SQL_C_SSHORT => Some(2),
        SQL_C_SBIGINT | SQL_C_DOUBLE => Some(8),
        SQL_C_BIT => Some(1),
        _ => None,
    }
}

pub(crate) fn hex_literal(bytes: &[u8]) -> String {
    let digits: String = bytes.iter().map(|byte| format!("{byte:02X}")).collect();
    format!("X'{digits}'")
}

#[cfg(test)]
mod tests {
    use super::{Converted, SQL_C_CHAR, SQL_C_SLONG, SQL_C_WCHAR, convert, parameter_text};
    use crate::native::Cell;

    #[test]
    fn values_convert_to_the_types_odbc_asks_for() {
        assert!(matches!(convert(&Cell::Text("42".into()), SQL_C_SLONG), Ok(Converted::Fixed(bytes)) if bytes == 42i32.to_ne_bytes()));
        assert!(matches!(convert(&Cell::Integer(7), SQL_C_CHAR), Ok(Converted::Text(bytes)) if bytes == b"7"));
        assert!(matches!(convert(&Cell::Blob(vec![0, 255]), SQL_C_CHAR), Ok(Converted::Text(bytes)) if bytes == b"00FF"));
        assert!(matches!(convert(&Cell::Text("é".into()), SQL_C_WCHAR), Ok(Converted::Wide(bytes)) if bytes == 0xE9u16.to_ne_bytes()));
        assert!(matches!(convert(&Cell::Null, SQL_C_SLONG), Ok(Converted::Null)));
        assert!(convert(&Cell::Integer(1 << 40), SQL_C_SLONG).is_err());
        assert_eq!(parameter_text(SQL_C_SLONG, &5i32.to_ne_bytes()).as_deref(), Ok("5"));
    }
}
