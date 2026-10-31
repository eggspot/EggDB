use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::Value;

pub(crate) const FUNCTIONS: [&str; 3] = ["ff1_encrypt", "ff1_decrypt", "laplace_noise"];

const ALPHABET: &str = "0123456789abcdefghijklmnopqrstuvwxyz";

pub(crate) const PRIVACY_SCRIPT: &str = r#"
CREATE SCHEMA eggdb_privacy;
CREATE FUNCTION eggdb_privacy.k_anonymity(schema_name text, table_name text, quasi_identifiers text[]) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    grouped text := '';
    smallest bigint;
BEGIN
    IF coalesce(array_length(quasi_identifiers, 1), 0) = 0 THEN
        RAISE EXCEPTION 'k_anonymity needs at least one quasi-identifier column' USING ERRCODE = '22023';
    END IF;
    FOR column_at IN 1..array_length(quasi_identifiers, 1) LOOP
        IF column_at > 1 THEN
            grouped := grouped || ', ';
        END IF;
        grouped := grouped || quote_ident(quasi_identifiers[column_at]);
    END LOOP;
    EXECUTE format('SELECT min(n) FROM (SELECT count(*) AS n FROM %I.%I GROUP BY %s) g', schema_name, table_name, grouped) INTO smallest;
    RETURN coalesce(smallest, 0);
END
$$;
"#;

fn invalid(message: impl Into<String>) -> SqlError {
    SqlError::new(SqlState::INVALID_PARAMETER_VALUE, message)
}

fn ff1(name: &str, values: &[Value]) -> SqlResult<Value> {
    let (text, key, tweak, radix) = match values {
        [Value::Text(text), Value::Bytea(key), Value::Bytea(tweak)] => (text, key, tweak, 10),
        [Value::Text(text), Value::Bytea(key), Value::Bytea(tweak), Value::Integer(radix)] => (text, key, tweak, *radix),
        _ => return Err(SqlError::new(SqlState::UNDEFINED_FUNCTION, format!("{name} takes (text, bytea key, bytea tweak [, integer radix])"))),
    };
    let radix = u32::try_from(radix).ok().filter(|radix| (2..=36).contains(radix)).ok_or_else(|| invalid(format!("{name} takes a radix from 2 to 36, not {radix}")))?;
    let alphabet = &ALPHABET[..radix as usize];
    let numerals = text
        .chars()
        .map(|character| alphabet.find(character).and_then(|at| u32::try_from(at).ok()))
        .collect::<Option<Vec<u32>>>()
        .ok_or_else(|| invalid(format!("{name}'s value may only use the characters {alphabet}")))?;
    let cipher = eggdb_crypto::Ff1::new(key, radix).map_err(|_| invalid(format!("{name} takes an AES key of 16, 24 or 32 bytes")))?;
    let result = if name == "ff1_encrypt" { cipher.encrypt(&numerals, tweak) } else { cipher.decrypt(&numerals, tweak) };
    let numerals = result.map_err(|error| match error {
        eggdb_crypto::Ff1Error::Length => invalid(format!("{name} needs at least a million possible values ({radix}^length) and halves of at most 120 bits")),
        _ => invalid(format!("{name} cannot use this value or tweak")),
    })?;
    Ok(Value::Text(numerals.iter().filter_map(|numeral| ALPHABET.as_bytes().get(*numeral as usize)).map(|byte| char::from(*byte)).collect()))
}

fn number(value: &Value) -> Option<f64> {
    match value {
        Value::SmallInt(number) => Some(f64::from(*number)),
        Value::Integer(number) => Some(f64::from(*number)),
        Value::Double(number) => Some(*number),
        Value::Real(number) => Some(f64::from(*number)),
        _ => None,
    }
}

fn laplace_noise(values: &[Value]) -> SqlResult<Value> {
    let [sensitivity, epsilon] = values else {
        return Err(SqlError::new(SqlState::UNDEFINED_FUNCTION, "laplace_noise takes (sensitivity, epsilon)"));
    };
    let (Some(sensitivity), Some(epsilon)) = (number(sensitivity), number(epsilon)) else {
        return Err(SqlError::new(SqlState::UNDEFINED_FUNCTION, "laplace_noise takes two numbers"));
    };
    if !(sensitivity.is_finite() && sensitivity > 0.0 && epsilon.is_finite() && epsilon > 0.0) {
        return Err(invalid("laplace_noise needs a positive sensitivity and a positive epsilon"));
    }
    let mut bytes = [0u8; 8];
    let uniform = loop {
        crate::random::fill(&mut bytes)?;
        let fraction = (u64::from_be_bytes(bytes) >> 11) as f64 / (1u64 << 53) as f64;
        if fraction > 0.0 {
            break fraction - 0.5;
        }
    };
    let scale = sensitivity / epsilon;
    Ok(Value::Double(-scale * uniform.signum() * (1.0 - 2.0 * uniform.abs()).ln()))
}

pub(crate) fn function(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    if !FUNCTIONS.contains(&name) {
        return None;
    }
    if values.iter().any(Value::is_null) {
        return Some(Ok(Value::Null));
    }
    Some(match name {
        "laplace_noise" => laplace_noise(values),
        _ => ff1(name, values),
    })
}

pub(crate) fn result_type(name: &str) -> crate::types::SqlType {
    if name == "laplace_noise" { crate::types::SqlType::Double } else { crate::types::SqlType::Text }
}

#[cfg(test)]
mod tests {
    use crate::memory_store::MemoryStore;
    use crate::tests::render;
    use crate::{Session, SqlError};

    fn run(session: &mut Session<'_, MemoryStore>, sql: &str) -> Result<String, SqlError> {
        let mut outputs = Vec::new();
        session.execute(sql, &mut |result| outputs.push(render(&result)))?;
        Ok(outputs.pop().unwrap_or_default())
    }

    #[test]
    fn ff1_tokens_keep_the_format_and_decrypt_back() {
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        let key = "'\\x2b7e151628aed2a6abf7158809cf4f3c'::bytea";
        assert_eq!(
            run(&mut session, &format!("SELECT ff1_encrypt('0123456789', {key}, '\\x'::bytea) AS token, ff1_decrypt('6124200773', {key}, '\\x39383736353433323130'::bytea) AS plain")).unwrap(),
            "token|plain\n2433477484|0123456789\nSELECT 1"
        );
        assert_eq!(
            run(&mut session, &format!("SELECT ff1_encrypt('0123456789abcdefghi', {key}, '\\x3737373770717273373737'::bytea, 36) AS token")).unwrap(),
            "token\na9tv40mll9kdu509eum\nSELECT 1"
        );
        for bad in [
            format!("SELECT ff1_encrypt('12345', {key}, '\\x'::bytea)"),
            format!("SELECT ff1_encrypt('12a456789', {key}, '\\x'::bytea)"),
            "SELECT ff1_encrypt('1234567890', '\\x00'::bytea, '\\x'::bytea)".to_owned(),
            format!("SELECT ff1_encrypt('1234567890', {key}, '\\x'::bytea, 37)"),
        ] {
            assert!(run(&mut session, &bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn laplace_noise_is_centred_and_scaled_by_sensitivity_over_epsilon() {
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        let summary = run(&mut session, "SELECT abs(avg(x)) < 0.2 AS centred, avg(abs(x)) BETWEEN 1.6 AND 2.4 AS scaled FROM (SELECT laplace_noise(1, 0.5) AS x FROM generate_series(1, 4000)) s").unwrap();
        assert_eq!(summary, "centred|scaled\nt|t\nSELECT 1");
        assert!(run(&mut session, "SELECT laplace_noise(1, 0)").is_err());
    }

    #[test]
    fn k_anonymity_is_the_smallest_group_of_quasi_identifiers() {
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        for sql in [
            "CREATE EXTENSION eggdb_privacy",
            "CREATE TABLE patients (id integer PRIMARY KEY, zip text, age_band text, diagnosis text)",
            "INSERT INTO patients VALUES (1, '100', '30-39', 'a'), (2, '100', '30-39', 'b'), (3, '100', '30-39', 'c'), (4, '200', '40-49', 'd'), (5, '200', '40-49', 'e')",
        ] {
            run(&mut session, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        }
        assert_eq!(run(&mut session, "SELECT eggdb_privacy.k_anonymity('public', 'patients', ARRAY['zip', 'age_band']) AS k").unwrap(), "k\n2\nSELECT 1");
        assert_eq!(run(&mut session, "SELECT eggdb_privacy.k_anonymity('public', 'patients', ARRAY['zip', 'diagnosis']) AS k").unwrap(), "k\n1\nSELECT 1");
        assert!(run(&mut session, "SELECT eggdb_privacy.k_anonymity('public', 'patients', ARRAY[]::text[])").is_err());
    }
}
