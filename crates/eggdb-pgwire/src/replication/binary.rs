//! The text form of a column value that `pgoutput` sent in its type's binary send format
//! (`binary 'true'`), for the built-in types EggDB maps.

use super::ReplicationError;

const BOOL: u32 = 16;
const BYTEA: u32 = 17;
const INT8: u32 = 20;
const INT2: u32 = 21;
const INT4: u32 = 23;
const TEXT: u32 = 25;
const JSON: u32 = 114;
const FLOAT4: u32 = 700;
const FLOAT8: u32 = 701;
const BPCHAR: u32 = 1042;
const VARCHAR: u32 = 1043;
const DATE: u32 = 1082;
const TIME: u32 = 1083;
const TIMESTAMP: u32 = 1114;
const TIMESTAMPTZ: u32 = 1184;
const INTERVAL: u32 = 1186;
const TIMETZ: u32 = 1266;
const NUMERIC: u32 = 1700;
const UUID: u32 = 2950;
const JSONB: u32 = 3802;

const DECODED: [u32; 20] = [
    BOOL,
    BYTEA,
    INT8,
    INT2,
    INT4,
    TEXT,
    JSON,
    FLOAT4,
    FLOAT8,
    BPCHAR,
    VARCHAR,
    DATE,
    TIME,
    TIMESTAMP,
    TIMESTAMPTZ,
    INTERVAL,
    TIMETZ,
    NUMERIC,
    UUID,
    JSONB,
];

const MICROS_PER_DAY: i64 = 86_400_000_000;
const DAYS_FROM_CIVIL_EPOCH_TO_POSTGRES_EPOCH: i64 = 10_957;
const NUMERIC_POSITIVE: u16 = 0x0000;
const NUMERIC_NEGATIVE: u16 = 0x4000;
const NUMERIC_NAN: u16 = 0xC000;
const NUMERIC_POSITIVE_INFINITY: u16 = 0xD000;
const NUMERIC_NEGATIVE_INFINITY: u16 = 0xF000;
const NUMERIC_BASE: u16 = 10_000;
const JSONB_VERSION: u8 = 1;

/// Whether a value of this type, sent in its binary send format, can be read back into text.
pub fn decodes_binary(type_oid: u32) -> bool {
    DECODED.contains(&type_oid)
}

fn malformed(type_oid: u32) -> ReplicationError {
    ReplicationError(format!(
        "a binary value of type {type_oid} does not have that type's layout"
    ))
}

fn fixed<const N: usize>(bytes: &[u8], type_oid: u32) -> Result<[u8; N], ReplicationError> {
    bytes.try_into().map_err(|_| malformed(type_oid))
}

fn utf8(bytes: &[u8], type_oid: u32) -> Result<String, ReplicationError> {
    String::from_utf8(bytes.to_vec()).map_err(|_| malformed(type_oid))
}

fn float_text(value: f64) -> String {
    if value.is_nan() {
        "NaN".to_owned()
    } else if value.is_infinite() {
        (if value > 0.0 { "Infinity" } else { "-Infinity" }).to_owned()
    } else if value == 0.0 || (1e-5..1e16).contains(&value.abs()) {
        format!("{value}")
    } else {
        format!("{value:e}")
    }
}

fn float4_text(value: f32) -> String {
    if value.is_finite() && value != 0.0 && !(1e-5..1e16).contains(&value.abs()) {
        format!("{value:e}")
    } else if value.is_finite() {
        format!("{value}")
    } else {
        float_text(f64::from(value))
    }
}

fn civil_from_days(days_since_civil_epoch: i64) -> (i64, u32, u32) {
    let shifted = days_since_civil_epoch + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_index + 2) / 5 + 1) as u32;
    let month = (if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    }) as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

fn date_parts(days_since_postgres_epoch: i64) -> (String, bool) {
    let (year, month, day) =
        civil_from_days(days_since_postgres_epoch + DAYS_FROM_CIVIL_EPOCH_TO_POSTGRES_EPOCH);
    let before_christ = year <= 0;
    let shown_year = if before_christ { 1 - year } else { year };
    (
        format!("{shown_year:04}-{month:02}-{day:02}"),
        before_christ,
    )
}

fn time_of_day_text(micros: u64) -> String {
    let seconds = micros / 1_000_000;
    let fraction = micros % 1_000_000;
    let mut text = format!(
        "{:02}:{:02}:{:02}",
        seconds / 3_600,
        seconds / 60 % 60,
        seconds % 60
    );
    if fraction != 0 {
        let digits = format!("{fraction:06}");
        text.push('.');
        text.push_str(digits.trim_end_matches('0'));
    }
    text
}

fn timestamp_text(micros: i64, zone_suffix: &str) -> String {
    match micros {
        i64::MAX => "infinity".to_owned(),
        i64::MIN => "-infinity".to_owned(),
        _ => {
            let (date, before_christ) = date_parts(micros.div_euclid(MICROS_PER_DAY));
            let time = time_of_day_text(micros.rem_euclid(MICROS_PER_DAY) as u64);
            format!(
                "{date} {time}{zone_suffix}{}",
                if before_christ { " BC" } else { "" }
            )
        }
    }
}

fn zone_text(seconds_west_of_utc: i32) -> String {
    let east = -i64::from(seconds_west_of_utc);
    let sign = if east < 0 { '-' } else { '+' };
    let east = east.unsigned_abs();
    let mut text = format!("{sign}{:02}", east / 3_600);
    if east % 3_600 != 0 {
        text.push_str(&format!(":{:02}", east / 60 % 60));
        if east % 60 != 0 {
            text.push_str(&format!(":{:02}", east % 60));
        }
    }
    text
}

fn interval_text(bytes: &[u8]) -> Result<String, ReplicationError> {
    let bytes: [u8; 16] = fixed(bytes, INTERVAL)?;
    let micros = i64::from_be_bytes(bytes[..8].try_into().map_err(|_| malformed(INTERVAL))?);
    let days = i32::from_be_bytes(bytes[8..12].try_into().map_err(|_| malformed(INTERVAL))?);
    let months = i32::from_be_bytes(bytes[12..].try_into().map_err(|_| malformed(INTERVAL))?);
    let sign = if micros < 0 { "-" } else { "" };
    Ok(format!(
        "{months} mons {days} days {sign}{}",
        time_of_day_text(micros.unsigned_abs())
    ))
}

fn numeric_text(bytes: &[u8]) -> Result<String, ReplicationError> {
    let header: [u8; 8] = bytes
        .get(..8)
        .and_then(|header| header.try_into().ok())
        .ok_or_else(|| malformed(NUMERIC))?;
    let digit_count = usize::from(u16::from_be_bytes([header[0], header[1]]));
    let weight = i64::from(i16::from_be_bytes([header[2], header[3]]));
    let sign = u16::from_be_bytes([header[4], header[5]]);
    let scale = usize::from(u16::from_be_bytes([header[6], header[7]]) & 0x3FFF);
    let body = &bytes[8..];
    if body.len() != digit_count * 2 {
        return Err(malformed(NUMERIC));
    }
    let digits = body
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let digit = u16::from_be_bytes(*pair);
            (digit < NUMERIC_BASE)
                .then_some(digit)
                .ok_or_else(|| malformed(NUMERIC))
        })
        .collect::<Result<Vec<u16>, _>>()?;
    let mut text = match sign {
        NUMERIC_NAN => return Ok("NaN".to_owned()),
        NUMERIC_POSITIVE_INFINITY => return Ok("Infinity".to_owned()),
        NUMERIC_NEGATIVE_INFINITY => return Ok("-Infinity".to_owned()),
        NUMERIC_NEGATIVE => String::from("-"),
        NUMERIC_POSITIVE => String::new(),
        _ => return Err(malformed(NUMERIC)),
    };
    let digit_at = |index: i64| -> u16 {
        usize::try_from(index)
            .ok()
            .and_then(|index| digits.get(index).copied())
            .unwrap_or(0)
    };
    if weight < 0 {
        text.push('0');
    } else {
        text.push_str(&digit_at(0).to_string());
        for index in 1..=weight {
            text.push_str(&format!("{:04}", digit_at(index)));
        }
    }
    if scale > 0 {
        let mut fraction = String::with_capacity(scale + 4);
        let mut index = weight + 1;
        while fraction.len() < scale {
            fraction.push_str(&format!("{:04}", digit_at(index)));
            index += 1;
        }
        fraction.truncate(scale);
        text.push('.');
        text.push_str(&fraction);
    }
    Ok(text)
}

fn uuid_text(bytes: &[u8]) -> Result<String, ReplicationError> {
    let bytes: [u8; 16] = fixed(bytes, UUID)?;
    let mut text = String::with_capacity(36);
    for (index, byte) in bytes.iter().enumerate() {
        if matches!(index, 4 | 6 | 8 | 10) {
            text.push('-');
        }
        text.push_str(&format!("{byte:02x}"));
    }
    Ok(text)
}

fn bytea_text(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(2 + bytes.len() * 2);
    text.push_str("\\x");
    for byte in bytes {
        text.push_str(&format!("{byte:02x}"));
    }
    text
}

/// The text form PostgreSQL's input function accepts for a value of `type_oid` sent in its
/// binary send format; an error for a type [`decodes_binary`] refuses or bytes that do not have
/// the type's layout.
pub fn binary_value_text(type_oid: u32, bytes: &[u8]) -> Result<String, ReplicationError> {
    Ok(match type_oid {
        BOOL => match fixed::<1>(bytes, type_oid)? {
            [0] => "false".to_owned(),
            [1] => "true".to_owned(),
            _ => return Err(malformed(type_oid)),
        },
        BYTEA => bytea_text(bytes),
        INT2 => i16::from_be_bytes(fixed(bytes, type_oid)?).to_string(),
        INT4 => i32::from_be_bytes(fixed(bytes, type_oid)?).to_string(),
        INT8 => i64::from_be_bytes(fixed(bytes, type_oid)?).to_string(),
        TEXT | JSON | BPCHAR | VARCHAR => utf8(bytes, type_oid)?,
        JSONB => match bytes.split_first() {
            Some((&JSONB_VERSION, text)) => utf8(text, type_oid)?,
            _ => return Err(malformed(type_oid)),
        },
        FLOAT4 => float4_text(f32::from_bits(u32::from_be_bytes(fixed(bytes, type_oid)?))),
        FLOAT8 => float_text(f64::from_bits(u64::from_be_bytes(fixed(bytes, type_oid)?))),
        DATE => match i32::from_be_bytes(fixed(bytes, type_oid)?) {
            i32::MAX => "infinity".to_owned(),
            i32::MIN => "-infinity".to_owned(),
            days => {
                let (date, before_christ) = date_parts(i64::from(days));
                if before_christ {
                    format!("{date} BC")
                } else {
                    date
                }
            }
        },
        TIME => {
            let micros = i64::from_be_bytes(fixed(bytes, type_oid)?);
            if !(0..=MICROS_PER_DAY).contains(&micros) {
                return Err(malformed(type_oid));
            }
            time_of_day_text(micros as u64)
        }
        TIMETZ => {
            let bytes: [u8; 12] = fixed(bytes, type_oid)?;
            let micros = i64::from_be_bytes(bytes[..8].try_into().map_err(|_| malformed(TIMETZ))?);
            let zone = i32::from_be_bytes(bytes[8..].try_into().map_err(|_| malformed(TIMETZ))?);
            if !(0..=MICROS_PER_DAY).contains(&micros) {
                return Err(malformed(type_oid));
            }
            format!("{}{}", time_of_day_text(micros as u64), zone_text(zone))
        }
        TIMESTAMP => timestamp_text(i64::from_be_bytes(fixed(bytes, type_oid)?), ""),
        TIMESTAMPTZ => timestamp_text(i64::from_be_bytes(fixed(bytes, type_oid)?), "+00"),
        INTERVAL => interval_text(bytes)?,
        NUMERIC => numeric_text(bytes)?,
        UUID => uuid_text(bytes)?,
        _ => {
            return Err(ReplicationError(format!(
                "values of type {type_oid} arrived in binary, which follow cannot read; follow again to read them as text"
            )));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numeric(digits: &[u16], weight: i16, sign: u16, scale: u16) -> Vec<u8> {
        let mut bytes = (digits.len() as u16).to_be_bytes().to_vec();
        bytes.extend_from_slice(&weight.to_be_bytes());
        bytes.extend_from_slice(&sign.to_be_bytes());
        bytes.extend_from_slice(&scale.to_be_bytes());
        for digit in digits {
            bytes.extend_from_slice(&digit.to_be_bytes());
        }
        bytes
    }

    #[test]
    fn integers_booleans_and_text_read_back() {
        assert_eq!(
            binary_value_text(INT4, &(-42i32).to_be_bytes()).unwrap(),
            "-42"
        );
        assert_eq!(binary_value_text(INT2, &7i16.to_be_bytes()).unwrap(), "7");
        assert_eq!(
            binary_value_text(INT8, &i64::MIN.to_be_bytes()).unwrap(),
            "-9223372036854775808"
        );
        assert_eq!(binary_value_text(BOOL, &[1]).unwrap(), "true");
        assert_eq!(binary_value_text(TEXT, "it's".as_bytes()).unwrap(), "it's");
        assert_eq!(
            binary_value_text(JSONB, b"\x01{\"n\": 1}").unwrap(),
            "{\"n\": 1}"
        );
        assert_eq!(binary_value_text(BYTEA, &[0xDE, 0xAD]).unwrap(), "\\xdead");
    }

    #[test]
    fn numerics_keep_their_scale_and_sign() {
        assert_eq!(
            binary_value_text(NUMERIC, &numeric(&[12, 3400], 0, NUMERIC_POSITIVE, 2)).unwrap(),
            "12.34"
        );
        assert_eq!(
            binary_value_text(NUMERIC, &numeric(&[1, 2345, 6789], 1, NUMERIC_NEGATIVE, 4)).unwrap(),
            "-12345.6789"
        );
        assert_eq!(
            binary_value_text(NUMERIC, &numeric(&[5], -2, NUMERIC_POSITIVE, 8)).unwrap(),
            "0.00000005"
        );
        assert_eq!(
            binary_value_text(NUMERIC, &numeric(&[], 0, NUMERIC_POSITIVE, 2)).unwrap(),
            "0.00"
        );
        assert_eq!(
            binary_value_text(NUMERIC, &numeric(&[1], 2, NUMERIC_POSITIVE, 0)).unwrap(),
            "100000000"
        );
        assert_eq!(
            binary_value_text(NUMERIC, &numeric(&[], 0, NUMERIC_NAN, 0)).unwrap(),
            "NaN"
        );
        assert!(binary_value_text(NUMERIC, &numeric(&[10_000], 0, NUMERIC_POSITIVE, 0)).is_err());
        assert!(binary_value_text(NUMERIC, &numeric(&[1], 0, 0x1234, 0)).is_err());
    }

    #[test]
    fn dates_and_times_print_as_postgresql_reads_them() {
        assert_eq!(
            binary_value_text(DATE, &0i32.to_be_bytes()).unwrap(),
            "2000-01-01"
        );
        assert_eq!(
            binary_value_text(DATE, &9_497i32.to_be_bytes()).unwrap(),
            "2026-01-01"
        );
        assert_eq!(
            binary_value_text(DATE, &i32::MAX.to_be_bytes()).unwrap(),
            "infinity"
        );
        assert_eq!(
            binary_value_text(DATE, &(-730_120i32).to_be_bytes()).unwrap(),
            "0001-12-31 BC"
        );
        let at = 9_497i64 * MICROS_PER_DAY + 3_723_000_500;
        assert_eq!(
            binary_value_text(TIMESTAMPTZ, &at.to_be_bytes()).unwrap(),
            "2026-01-01 01:02:03.0005+00"
        );
        assert_eq!(
            binary_value_text(TIMESTAMP, &(-1i64).to_be_bytes()).unwrap(),
            "1999-12-31 23:59:59.999999"
        );
        let mut timetz = 3_600_000_000i64.to_be_bytes().to_vec();
        timetz.extend_from_slice(&(-19_800i32).to_be_bytes());
        assert_eq!(
            binary_value_text(TIMETZ, &timetz).unwrap(),
            "01:00:00+05:30"
        );
        let mut interval = (-5_000_000i64).to_be_bytes().to_vec();
        interval.extend_from_slice(&3i32.to_be_bytes());
        interval.extend_from_slice(&14i32.to_be_bytes());
        assert_eq!(
            binary_value_text(INTERVAL, &interval).unwrap(),
            "14 mons 3 days -00:00:05"
        );
    }

    #[test]
    fn floats_and_uuids_read_back() {
        assert_eq!(
            binary_value_text(FLOAT8, &1.5f64.to_be_bytes()).unwrap(),
            "1.5"
        );
        assert_eq!(
            binary_value_text(FLOAT8, &f64::INFINITY.to_be_bytes()).unwrap(),
            "Infinity"
        );
        assert_eq!(
            binary_value_text(FLOAT8, &1e300f64.to_be_bytes()).unwrap(),
            "1e300"
        );
        assert_eq!(
            binary_value_text(FLOAT4, &0.25f32.to_be_bytes()).unwrap(),
            "0.25"
        );
        let uuid: Vec<u8> = (0u8..16).collect();
        assert_eq!(
            binary_value_text(UUID, &uuid).unwrap(),
            "00010203-0405-0607-0809-0a0b0c0d0e0f"
        );
    }

    #[test]
    fn wrong_lengths_and_unknown_types_are_errors_not_panics() {
        for type_oid in DECODED {
            for length in 0..20 {
                let bytes = vec![0xFFu8; length];
                let _ = binary_value_text(type_oid, &bytes);
            }
        }
        assert!(binary_value_text(INT4, &[0, 1]).is_err());
        assert!(binary_value_text(TIME, &(-1i64).to_be_bytes()).is_err());
        assert!(binary_value_text(JSONB, b"\x02{}").is_err());
        assert!(binary_value_text(600, &[0; 16]).is_err());
        assert!(!decodes_binary(600));
        assert!(decodes_binary(NUMERIC));
    }
}
