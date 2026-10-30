use super::routines::bounded_text;
use super::{NAMESPACE, Reader, put_bytes};
use crate::error::SqlResult;
use crate::plsql::StoredPackage;

pub(super) const PACKAGE_SPACE: u8 = 0x0D;
const SPECIFICATION_ONLY: u8 = 0;
const WITH_BODY: u8 = 1;

pub(crate) fn package_key(name: &str) -> Vec<u8> {
    let mut key = NAMESPACE.to_vec();
    key.push(PACKAGE_SPACE);
    key.extend_from_slice(name.as_bytes());
    key
}

pub(crate) fn packages_prefix() -> Vec<u8> {
    package_key("")
}

pub(crate) fn encode_package(package: &StoredPackage) -> Vec<u8> {
    let mut out = vec![1];
    match &package.body {
        None => {
            out.push(SPECIFICATION_ONLY);
            put_bytes(&mut out, package.specification.as_bytes());
        }
        Some(body) => {
            out.push(WITH_BODY);
            put_bytes(&mut out, package.specification.as_bytes());
            put_bytes(&mut out, body.as_bytes());
        }
    }
    out
}

pub(crate) fn decode_package(name: &str, bytes: &[u8]) -> SqlResult<StoredPackage> {
    let mut reader = Reader::new(bytes, "package definitions");
    reader.version()?;
    if name.is_empty() || !crate::schemas::is_well_formed(name) {
        return Err(reader.damaged());
    }
    let shape = reader.byte()?;
    let specification = bounded_text(&mut reader)?;
    let body = match shape {
        SPECIFICATION_ONLY => None,
        WITH_BODY => Some(bounded_text(&mut reader)?),
        _ => return Err(reader.damaged()),
    };
    if specification.is_empty() || body.as_ref().is_some_and(String::is_empty) {
        return Err(reader.damaged());
    }
    reader.finish()?;
    Ok(StoredPackage {
        specification,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> StoredPackage {
        StoredPackage {
            specification: "CREATE PACKAGE p AS v NUMBER; END p;".to_owned(),
            body: Some("CREATE PACKAGE BODY p AS BEGIN v := 1; END p;".to_owned()),
        }
    }

    #[test]
    fn package_records_round_trip_with_and_without_a_body() {
        let with_body = sample();
        assert_eq!(decode_package("p", &encode_package(&with_body)).unwrap(), with_body);
        let without_body = StoredPackage {
            body: None,
            ..sample()
        };
        assert_eq!(
            decode_package("p", &encode_package(&without_body)).unwrap(),
            without_body
        );
        assert!(package_key("hr\0p").starts_with(&packages_prefix()));
    }

    #[test]
    fn package_records_pin_their_bytes() {
        let package = StoredPackage {
            specification: "S".to_owned(),
            body: Some("B".to_owned()),
        };
        assert_eq!(encode_package(&package), vec![1, 1, 1, b'S', 1, b'B']);
    }

    #[test]
    fn damaged_package_records_are_errors_not_panics() {
        let bytes = encode_package(&sample());
        for cut in 0..bytes.len() {
            assert!(decode_package("p", &bytes[..cut]).is_err());
        }
        let mut wrong_shape = bytes.clone();
        wrong_shape[1] = 7;
        assert!(decode_package("p", &wrong_shape).is_err());
        let mut trailing = bytes;
        trailing.push(0);
        assert!(decode_package("p", &trailing).is_err());
        assert!(decode_package("", &encode_package(&sample())).is_err());
    }
}
