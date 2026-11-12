use crate::error::{SqlError, SqlResult, SqlState};

const DIMENSION_TAG: u64 = 0x0A;
const VARIABLE_TAG: u64 = 0x0B;
const ATTRIBUTE_TAG: u64 = 0x0C;
const STREAMING: u64 = 0xFFFF_FFFF;
const MOST_ELEMENTS: usize = 10_000_000;
const MOST_ENTRIES: u64 = 100_000;

fn invalid(message: impl std::fmt::Display) -> SqlError {
    SqlError::new(SqlState::DATA_CORRUPTED, format!("invalid NetCDF file: {message}"))
}

/// One value of a NetCDF variable: a number, or the text of a character variable's last
/// dimension.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Element {
    Number(f64),
    Text(String),
}

struct Cursor<'b> {
    bytes: &'b [u8],
    at: usize,
    wide: bool,
}

impl<'b> Cursor<'b> {
    fn take(&mut self, count: usize) -> SqlResult<&'b [u8]> {
        let end = self.at.checked_add(count).filter(|end| *end <= self.bytes.len()).ok_or_else(|| invalid("the header is cut short"))?;
        let taken = &self.bytes[self.at..end];
        self.at = end;
        Ok(taken)
    }

    fn u32(&mut self) -> SqlResult<u64> {
        Ok(u64::from(u32::from_be_bytes(self.take(4)?.try_into().map_err(|_| invalid("short"))?)))
    }

    fn u64(&mut self) -> SqlResult<u64> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().map_err(|_| invalid("short"))?))
    }

    fn count(&mut self) -> SqlResult<u64> {
        if self.wide { self.u64() } else { self.u32() }
    }

    fn name(&mut self) -> SqlResult<String> {
        let length = usize::try_from(self.count()?).map_err(|_| invalid("a name is too long"))?;
        let raw = self.take(length)?;
        self.take((4 - length % 4) % 4)?;
        Ok(String::from_utf8_lossy(raw).into_owned())
    }

    fn list(&mut self, tag: u64) -> SqlResult<u64> {
        let found = self.u32()?;
        let count = self.count()?;
        if found == 0 && count == 0 {
            return Ok(0);
        }
        if found != tag || count > MOST_ENTRIES {
            return Err(invalid("a header list is damaged"));
        }
        Ok(count)
    }
}

fn type_size(kind: u64) -> SqlResult<usize> {
    Ok(match kind {
        1 | 2 | 7 => 1,
        3 | 8 => 2,
        4 | 5 | 9 => 4,
        6 | 10 | 11 => 8,
        other => return Err(invalid(format!("type {other} is unknown"))),
    })
}

fn number(kind: u64, raw: &[u8]) -> f64 {
    match kind {
        1 => f64::from(raw[0] as i8),
        7 => f64::from(raw[0]),
        3 => f64::from(i16::from_be_bytes([raw[0], raw[1]])),
        8 => f64::from(u16::from_be_bytes([raw[0], raw[1]])),
        4 => f64::from(i32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]])),
        9 => f64::from(u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]])),
        5 => f64::from(f32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]])),
        6 => f64::from_be_bytes(raw.try_into().unwrap_or([0; 8])),
        10 => i64::from_be_bytes(raw.try_into().unwrap_or([0; 8])) as f64,
        _ => u64::from_be_bytes(raw.try_into().unwrap_or([0; 8])) as f64,
    }
}

fn skip_attributes(cursor: &mut Cursor<'_>) -> SqlResult<()> {
    for _ in 0..cursor.list(ATTRIBUTE_TAG)? {
        cursor.name()?;
        let kind = cursor.u32()?;
        let count = usize::try_from(cursor.count()?).map_err(|_| invalid("an attribute is too long"))?;
        let bytes = count.checked_mul(type_size(kind)?).ok_or_else(|| invalid("an attribute is too long"))?;
        cursor.take(bytes)?;
        cursor.take((4 - bytes % 4) % 4)?;
    }
    Ok(())
}

struct Variable {
    name: String,
    shape: Vec<usize>,
    record: bool,
    kind: u64,
    size: usize,
    begin: usize,
}

/// Every element of every variable of a NetCDF classic file (CDF-1, the 64-bit-offset CDF-2
/// and the 64-bit-data CDF-5): the variable's name, the element's coordinates and its value,
/// in row-major order; a character variable gives one text per string along its last
/// dimension. Record variables are read record by record, interleaved as the format stores
/// them.
pub(crate) fn elements(bytes: &[u8]) -> SqlResult<Vec<(String, Vec<i64>, Element)>> {
    let version = match bytes.get(..4) {
        Some([b'C', b'D', b'F', version @ (1 | 2 | 5)]) => *version,
        _ => return Err(invalid("not a NetCDF classic file")),
    };
    let mut cursor = Cursor { bytes, at: 4, wide: version == 5 };
    let records = cursor.count()?;
    let records = usize::try_from(if records == STREAMING { 0 } else { records }).map_err(|_| invalid("too many records"))?;
    let mut dimensions = Vec::new();
    for _ in 0..cursor.list(DIMENSION_TAG)? {
        cursor.name()?;
        dimensions.push(usize::try_from(cursor.count()?).map_err(|_| invalid("a dimension is too long"))?);
    }
    skip_attributes(&mut cursor)?;
    let mut variables = Vec::new();
    for _ in 0..cursor.list(VARIABLE_TAG)? {
        let name = cursor.name()?;
        let rank = cursor.count()?;
        if rank > 64 {
            return Err(invalid("a variable has too many dimensions"));
        }
        let mut shape = Vec::new();
        let mut record = false;
        for axis in 0..rank {
            let id = usize::try_from(cursor.count()?).map_err(|_| invalid("a dimension id is damaged"))?;
            let length = *dimensions.get(id).ok_or_else(|| invalid("a variable names an unknown dimension"))?;
            if length == 0 {
                if axis != 0 {
                    return Err(invalid("only a variable's first dimension may be the record dimension"));
                }
                record = true;
                shape.push(records);
            } else {
                shape.push(length);
            }
        }
        skip_attributes(&mut cursor)?;
        let kind = cursor.u32()?;
        type_size(kind)?;
        let size = usize::try_from(cursor.count()?).map_err(|_| invalid("a variable size is damaged"))?;
        let begin = usize::try_from(if version == 1 { cursor.u32()? } else { cursor.u64()? })
            .map_err(|_| invalid("an offset is damaged"))?;
        variables.push(Variable { name, shape, record, kind, size, begin });
    }
    let record_variables = variables.iter().filter(|variable| variable.record).count();
    let slab_bytes = |variable: &Variable| -> SqlResult<usize> {
        variable.shape[1..]
            .iter()
            .try_fold(type_size(variable.kind)?, |total, length| total.checked_mul(*length))
            .ok_or_else(|| invalid("a variable is too large"))
    };
    let record_size = if record_variables == 1 {
        variables.iter().find(|variable| variable.record).map_or(Ok(0), slab_bytes)?
    } else {
        variables.iter().filter(|variable| variable.record).map(|variable| variable.size).sum()
    };
    let mut out = Vec::new();
    for variable in &variables {
        let element_size = type_size(variable.kind)?;
        let count = variable.shape.iter().try_fold(1usize, |total, length| total.checked_mul(*length)).ok_or_else(|| invalid("a variable is too large"))?;
        if out.len().saturating_add(count) > MOST_ELEMENTS {
            return Err(invalid(format!("more than {MOST_ELEMENTS} elements")));
        }
        let mut data = Vec::new();
        if variable.record {
            let slab = slab_bytes(variable)?;
            for record in 0..records {
                let start = record
                    .checked_mul(record_size)
                    .and_then(|offset| offset.checked_add(variable.begin))
                    .ok_or_else(|| invalid("a record offset is too large"))?;
                let chunk = bytes.get(start..start + slab).ok_or_else(|| invalid(format!("{}'s data runs past the end", variable.name)))?;
                data.extend_from_slice(chunk);
            }
        } else {
            let length = count.checked_mul(element_size).ok_or_else(|| invalid("a variable is too large"))?;
            let end = variable.begin.checked_add(length).ok_or_else(|| invalid("a variable is too large"))?;
            data.extend_from_slice(bytes.get(variable.begin..end).ok_or_else(|| invalid(format!("{}'s data runs past the end", variable.name)))?);
        }
        if variable.kind == 2 {
            let width = variable.shape.last().copied().unwrap_or(1).max(1);
            let outer = &variable.shape[..variable.shape.len().saturating_sub(1)];
            let mut point = vec![0usize; outer.len()];
            for chunk in data.chunks(width) {
                let text = String::from_utf8_lossy(chunk).trim_end_matches('\0').to_owned();
                out.push((variable.name.clone(), point.iter().map(|at| *at as i64).collect(), Element::Text(text)));
                advance(&mut point, outer);
            }
            continue;
        }
        let mut point = vec![0usize; variable.shape.len()];
        for chunk in data.chunks(element_size) {
            out.push((
                variable.name.clone(),
                point.iter().map(|at| *at as i64).collect(),
                Element::Number(number(variable.kind, chunk)),
            ));
            advance(&mut point, &variable.shape);
        }
    }
    Ok(out)
}

fn advance(point: &mut [usize], shape: &[usize]) {
    for axis in (0..point.len()).rev() {
        point[axis] += 1;
        if point[axis] < shape[axis] {
            return;
        }
        point[axis] = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(out: &mut Vec<u8>, text: &str) {
        out.extend_from_slice(&(text.len() as u32).to_be_bytes());
        out.extend_from_slice(text.as_bytes());
        out.resize(out.len() + (4 - text.len() % 4) % 4, 0);
    }

    #[test]
    fn classic_files_give_every_element_with_its_coordinates() {
        let mut file = b"CDF\x01".to_vec();
        file.extend_from_slice(&0u32.to_be_bytes());
        file.extend_from_slice(&(DIMENSION_TAG as u32).to_be_bytes());
        file.extend_from_slice(&1u32.to_be_bytes());
        name(&mut file, "x");
        file.extend_from_slice(&3u32.to_be_bytes());
        file.extend_from_slice(&[0; 8]);
        file.extend_from_slice(&(VARIABLE_TAG as u32).to_be_bytes());
        file.extend_from_slice(&1u32.to_be_bytes());
        name(&mut file, "t");
        file.extend_from_slice(&1u32.to_be_bytes());
        file.extend_from_slice(&0u32.to_be_bytes());
        file.extend_from_slice(&[0; 8]);
        file.extend_from_slice(&3u32.to_be_bytes());
        file.extend_from_slice(&8u32.to_be_bytes());
        let begin = file.len() + 4;
        file.extend_from_slice(&(begin as u32).to_be_bytes());
        for value in [10i16, -2, 7] {
            file.extend_from_slice(&value.to_be_bytes());
        }
        file.extend_from_slice(&[0, 0]);
        let found = elements(&file).unwrap();
        assert_eq!(
            found,
            vec![
                ("t".to_owned(), vec![0], Element::Number(10.0)),
                ("t".to_owned(), vec![1], Element::Number(-2.0)),
                ("t".to_owned(), vec![2], Element::Number(7.0)),
            ]
        );
        assert!(elements(b"CDF\x03").is_err());
    }
}
