use super::{NAMESPACE, Reader, put_bytes, put_type, put_varint};
use crate::error::SqlResult;
use crate::types::{DefinedKind, SqlType};
use crate::user_types::{
    Attribute, DomainCheck, DomainDefinition, EnumLabel, MAX_ATTRIBUTES, MAX_DOMAIN_CHECKS,
    MAX_ENUM_LABELS, MAX_LABEL_BYTES, TypeBody, UserType,
};

const TYPE_SPACE: u8 = 0x0A;
const RECORD_VERSION: u8 = 1;
const ENUM_KIND: u8 = 1;
const COMPOSITE_KIND: u8 = 2;
const RANGE_KIND: u8 = 3;
const DOMAIN_KIND: u8 = 4;
const NOT_NULL_FLAG: u8 = 1;

pub(crate) fn user_type_key(name: &str) -> Vec<u8> {
    let mut key = NAMESPACE.to_vec();
    key.push(TYPE_SPACE);
    key.extend_from_slice(name.as_bytes());
    key
}

pub(crate) fn user_types_prefix() -> Vec<u8> {
    user_type_key("")
}

pub(crate) fn defined_kind_code(kind: DefinedKind) -> u8 {
    match kind {
        DefinedKind::Enum => ENUM_KIND,
        DefinedKind::Composite => COMPOSITE_KIND,
        DefinedKind::Range => RANGE_KIND,
        DefinedKind::Domain => DOMAIN_KIND,
    }
}

pub(crate) fn defined_kind_of(code: u8) -> Option<DefinedKind> {
    [
        DefinedKind::Enum,
        DefinedKind::Composite,
        DefinedKind::Range,
        DefinedKind::Domain,
    ]
    .into_iter()
    .find(|kind| defined_kind_code(*kind) == code)
}

pub(crate) fn encode_user_type(user_type: &UserType) -> Vec<u8> {
    let mut out = vec![RECORD_VERSION];
    out.push(defined_kind_code(user_type.kind()));
    out.extend_from_slice(&user_type.oid.to_le_bytes());
    match &user_type.body {
        TypeBody::Enum(labels) => {
            put_varint(&mut out, labels.len() as u64);
            for label in labels {
                out.extend_from_slice(&label.sort_order.to_bits().to_le_bytes());
                put_bytes(&mut out, label.label.as_bytes());
            }
        }
        TypeBody::Composite(attributes) => {
            put_varint(&mut out, attributes.len() as u64);
            for attribute in attributes {
                put_bytes(&mut out, attribute.name.as_bytes());
                put_type(&mut out, attribute.sql_type);
            }
        }
        TypeBody::Range { subtype } => put_type(&mut out, *subtype),
        TypeBody::Domain(domain) => {
            put_type(&mut out, domain.base);
            out.push(if domain.not_null { NOT_NULL_FLAG } else { 0 });
            match &domain.default {
                Some(text) => {
                    out.push(1);
                    put_bytes(&mut out, text.as_bytes());
                }
                None => out.push(0),
            }
            put_varint(&mut out, domain.checks.len() as u64);
            for check in &domain.checks {
                put_bytes(&mut out, check.name.as_bytes());
                put_bytes(&mut out, check.expression.as_bytes());
                out.push(u8::from(check.validated));
            }
        }
    }
    out
}

fn member_type(reader: &mut Reader<'_>, own_oid: u32) -> SqlResult<SqlType> {
    let sql_type = reader.sql_type(true, true, true)?;
    if sql_type.defined().is_some_and(|defined| defined.oid == own_oid) {
        return Err(reader.damaged());
    }
    Ok(sql_type)
}

pub(crate) fn decode_user_type(name: &str, bytes: &[u8]) -> SqlResult<UserType> {
    let mut reader = Reader::new(bytes, "type definition");
    reader.version()?;
    if name.is_empty() || !crate::schemas::is_well_formed(name) {
        return Err(reader.damaged());
    }
    let kind = defined_kind_of(reader.byte()?).ok_or_else(|| reader.damaged())?;
    let oid = reader.u32()?;
    if oid < 16_384 || oid > i32::MAX as u32 {
        return Err(reader.damaged());
    }
    let body = match kind {
        DefinedKind::Enum => {
            let count = reader.count()?;
            if count > MAX_ENUM_LABELS {
                return Err(reader.damaged());
            }
            let mut labels: Vec<EnumLabel> = Vec::with_capacity(count);
            for _ in 0..count {
                let sort_order = f64::from_bits(u64::from_le_bytes(reader.array()?));
                let label = reader.text()?;
                let ascending = labels
                    .last()
                    .is_none_or(|previous| previous.sort_order < sort_order);
                if !sort_order.is_finite()
                    || !ascending
                    || label.is_empty()
                    || label.len() > MAX_LABEL_BYTES
                    || labels.iter().any(|known| known.label == label)
                {
                    return Err(reader.damaged());
                }
                labels.push(EnumLabel { sort_order, label });
            }
            TypeBody::Enum(labels)
        }
        DefinedKind::Composite => {
            let count = reader.count()?;
            if count > MAX_ATTRIBUTES {
                return Err(reader.damaged());
            }
            let mut attributes: Vec<Attribute> = Vec::with_capacity(count);
            for _ in 0..count {
                let attribute_name = reader.plain_name()?;
                let sql_type = member_type(&mut reader, oid)?;
                if attributes.iter().any(|known| known.name == attribute_name)
                    || sql_type
                        .defined()
                        .is_some_and(|defined| defined.kind == DefinedKind::Domain)
                {
                    return Err(reader.damaged());
                }
                attributes.push(Attribute {
                    name: attribute_name,
                    sql_type,
                });
            }
            TypeBody::Composite(attributes)
        }
        DefinedKind::Range => {
            let subtype = member_type(&mut reader, oid)?;
            if !crate::ranges::is_subtype(subtype) {
                return Err(reader.damaged());
            }
            TypeBody::Range { subtype }
        }
        DefinedKind::Domain => {
            let base = member_type(&mut reader, oid)?;
            let not_null = match reader.byte()? {
                0 => false,
                NOT_NULL_FLAG => true,
                _ => return Err(reader.damaged()),
            };
            let default = match reader.byte()? {
                0 => None,
                1 => {
                    let text = reader.text()?;
                    if text.trim().is_empty() {
                        return Err(reader.damaged());
                    }
                    Some(text)
                }
                _ => return Err(reader.damaged()),
            };
            let count = reader.count()?;
            if count > MAX_DOMAIN_CHECKS {
                return Err(reader.damaged());
            }
            let mut checks: Vec<DomainCheck> = Vec::with_capacity(count);
            for _ in 0..count {
                let check_name = reader.plain_name()?;
                let expression = reader.text()?;
                let validated = match reader.byte()? {
                    0 => false,
                    1 => true,
                    _ => return Err(reader.damaged()),
                };
                if expression.trim().is_empty() || checks.iter().any(|known| known.name == check_name)
                {
                    return Err(reader.damaged());
                }
                checks.push(DomainCheck {
                    name: check_name,
                    expression,
                    validated,
                });
            }
            TypeBody::Domain(DomainDefinition {
                base,
                not_null,
                default,
                checks,
            })
        }
    };
    reader.finish()?;
    Ok(UserType {
        oid,
        name: name.to_owned(),
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::{decode_user_type, encode_user_type};
    use crate::types::{DefinedType, SqlType};
    use crate::user_types::{
        Attribute, DomainCheck, DomainDefinition, EnumLabel, TypeBody, UserType,
    };

    fn samples() -> Vec<UserType> {
        vec![
            UserType {
                oid: 20_000,
                name: "mood".into(),
                body: TypeBody::Enum(vec![
                    EnumLabel {
                        sort_order: 1.0,
                        label: "sad".into(),
                    },
                    EnumLabel {
                        sort_order: 1.5,
                        label: "ok".into(),
                    },
                    EnumLabel {
                        sort_order: 2.0,
                        label: "happy".into(),
                    },
                ]),
            },
            UserType {
                oid: 20_001,
                name: "app\0address".into(),
                body: TypeBody::Composite(vec![
                    Attribute {
                        name: "street".into(),
                        sql_type: SqlType::Text,
                    },
                    Attribute {
                        name: "mood".into(),
                        sql_type: DefinedType::enumerated(20_000),
                    },
                    Attribute {
                        name: "tags".into(),
                        sql_type: SqlType::Array(crate::arrays::ArrayElement::Text),
                    },
                ]),
            },
            UserType {
                oid: 20_002,
                name: "floatrange".into(),
                body: TypeBody::Range {
                    subtype: SqlType::Double,
                },
            },
            UserType {
                oid: 20_003,
                name: "posint".into(),
                body: TypeBody::Domain(DomainDefinition {
                    base: SqlType::Integer,
                    not_null: true,
                    default: Some("1".into()),
                    checks: vec![DomainCheck {
                        name: "posint_check".into(),
                        expression: "VALUE > 0".into(),
                        validated: false,
                    }],
                }),
            },
        ]
    }

    #[test]
    fn type_definitions_round_trip_and_pin_their_bytes() {
        for sample in samples() {
            let bytes = encode_user_type(&sample);
            assert_eq!(decode_user_type(&sample.name, &bytes).unwrap(), sample);
        }
        let mood = encode_user_type(&samples()[0]);
        assert_eq!(&mood[..6], &[1, 1, 0x20, 0x4E, 0, 0]);
        assert_eq!(mood[6], 3);
        assert_eq!(&mood[7..15], &1.0f64.to_bits().to_le_bytes());
        assert_eq!(&mood[15..19], &[3, b's', b'a', b'd']);
    }

    #[test]
    fn damaged_type_definitions_are_errors_never_panics() {
        for sample in samples() {
            let bytes = encode_user_type(&sample);
            for cut in 0..bytes.len() {
                assert!(decode_user_type(&sample.name, &bytes[..cut]).is_err());
            }
            for at in 0..bytes.len() {
                for flip in [0x01u8, 0x80, 0xFF] {
                    let mut changed = bytes.clone();
                    changed[at] ^= flip;
                    let _ = decode_user_type(&sample.name, &changed);
                }
            }
            let mut longer = bytes.clone();
            longer.push(0);
            assert!(decode_user_type(&sample.name, &longer).is_err());
        }
        let mut unordered = encode_user_type(&samples()[0]);
        unordered[7..15].copy_from_slice(&5.0f64.to_bits().to_le_bytes());
        assert!(decode_user_type("mood", &unordered).is_err());
        let mut newer = encode_user_type(&samples()[0]);
        newer[0] = 2;
        assert!(decode_user_type("mood", &newer).is_err());
        let mut low_oid = encode_user_type(&samples()[0]);
        low_oid[2..6].copy_from_slice(&23u32.to_le_bytes());
        assert!(decode_user_type("mood", &low_oid).is_err());
    }
}
