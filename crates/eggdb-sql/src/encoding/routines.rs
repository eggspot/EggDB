use super::{NAMESPACE, Reader, dialect_code, dialect_of, put_bytes, put_varint};
use crate::error::SqlResult;
use crate::routines::{MAX_OVERLOADS, MAX_ROUTINE_TEXT, RoutineKind, StoredRoutine};

pub(super) const ROUTINE_SPACE: u8 = 0x0C;
const FUNCTION_KIND: u8 = 1;
const PROCEDURE_KIND: u8 = 2;

pub(crate) fn routine_key(name: &str) -> Vec<u8> {
    let mut key = NAMESPACE.to_vec();
    key.push(ROUTINE_SPACE);
    key.extend_from_slice(name.as_bytes());
    key
}

pub(crate) fn routines_prefix() -> Vec<u8> {
    routine_key("")
}

fn kind_code(kind: RoutineKind) -> u8 {
    match kind {
        RoutineKind::Function => FUNCTION_KIND,
        RoutineKind::Procedure => PROCEDURE_KIND,
    }
}

fn kind_of(code: u8) -> Option<RoutineKind> {
    match code {
        FUNCTION_KIND => Some(RoutineKind::Function),
        PROCEDURE_KIND => Some(RoutineKind::Procedure),
        _ => None,
    }
}

pub(crate) fn encode_routines(routines: &[StoredRoutine]) -> Vec<u8> {
    let mut out = vec![1];
    put_varint(&mut out, routines.len() as u64);
    for routine in routines {
        out.push(dialect_code(routine.dialect));
        out.push(kind_code(routine.kind));
        put_bytes(&mut out, routine.identity.as_bytes());
        put_bytes(&mut out, routine.text.as_bytes());
    }
    out
}

pub(super) fn bounded_text(reader: &mut Reader<'_>) -> SqlResult<String> {
    let text = reader.text()?;
    if text.len() > MAX_ROUTINE_TEXT {
        return Err(reader.damaged());
    }
    Ok(text)
}

pub(crate) fn decode_routines(name: &str, bytes: &[u8]) -> SqlResult<Vec<StoredRoutine>> {
    let mut reader = Reader::new(bytes, "routine definitions");
    reader.version()?;
    if name.is_empty() || !crate::schemas::is_well_formed(name) {
        return Err(reader.damaged());
    }
    let count = reader.count()?;
    if count == 0 || count > MAX_OVERLOADS {
        return Err(reader.damaged());
    }
    let mut routines: Vec<StoredRoutine> = Vec::with_capacity(count);
    for _ in 0..count {
        let dialect = dialect_of(reader.byte()?).ok_or_else(|| reader.damaged())?;
        let kind = kind_of(reader.byte()?).ok_or_else(|| reader.damaged())?;
        let identity = bounded_text(&mut reader)?;
        let text = bounded_text(&mut reader)?;
        if text.is_empty() || routines.iter().any(|known| known.identity == identity) {
            return Err(reader.damaged());
        }
        routines.push(StoredRoutine {
            dialect,
            kind,
            identity,
            text,
        });
    }
    reader.finish()?;
    Ok(routines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::Dialect;

    fn sample() -> Vec<StoredRoutine> {
        vec![
            StoredRoutine {
                dialect: Dialect::PostgreSql,
                kind: RoutineKind::Function,
                identity: "integer".to_owned(),
                text: "CREATE FUNCTION f(a integer) RETURNS integer LANGUAGE sql AS 'SELECT a'"
                    .to_owned(),
            },
            StoredRoutine {
                dialect: Dialect::PostgreSql,
                kind: RoutineKind::Procedure,
                identity: String::new(),
                text: "CREATE PROCEDURE f() LANGUAGE plpgsql AS $$BEGIN END$$".to_owned(),
            },
        ]
    }

    #[test]
    fn routine_records_round_trip() {
        let routines = sample();
        let bytes = encode_routines(&routines);
        assert_eq!(decode_routines("f", &bytes).unwrap(), routines);
        assert!(routine_key("app\0f").starts_with(&routines_prefix()));
    }

    #[test]
    fn damaged_routine_records_are_errors_not_panics() {
        let bytes = encode_routines(&sample());
        for cut in 0..bytes.len() {
            assert!(decode_routines("f", &bytes[..cut]).is_err());
        }
        let mut wrong_kind = bytes.clone();
        wrong_kind[3] = 9;
        assert!(decode_routines("f", &wrong_kind).is_err());
        assert!(decode_routines("", &bytes).is_err());
        let mut longer = bytes.clone();
        longer.push(0);
        assert!(decode_routines("f", &longer).is_err());
        let duplicated = encode_routines(&[sample()[0].clone(), sample()[0].clone()]);
        assert!(decode_routines("f", &duplicated).is_err());
        assert!(decode_routines("f", &encode_routines(&[])).is_err());
    }
}
