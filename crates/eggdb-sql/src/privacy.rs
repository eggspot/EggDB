use crate::error::{SqlError, SqlResult, SqlState};
use crate::types::Value;

pub(crate) const FUNCTIONS: [&str; 8] = ["ff1_encrypt", "ff1_decrypt", "laplace_noise", "gen_random_bytes", "eggdb_seal", "eggdb_unseal", "eggdb_blind_index", "snapping_mechanism"];
const LOWEST_EXPONENT: i32 = -1022;
const DEFAULT_BLIND_INDEX_BITS: i64 = 64;
const MOST_RANDOM_BYTES: i64 = 1024;
const SEAL_VERSION: u8 = 1;

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
CREATE FUNCTION eggdb_privacy.l_diversity(schema_name text, table_name text, quasi_identifiers text[], sensitive_column text) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    grouped text := '';
    smallest bigint;
BEGIN
    IF coalesce(array_length(quasi_identifiers, 1), 0) = 0 THEN
        RAISE EXCEPTION 'l_diversity needs at least one quasi-identifier column' USING ERRCODE = '22023';
    END IF;
    FOR column_at IN 1..array_length(quasi_identifiers, 1) LOOP
        IF column_at > 1 THEN
            grouped := grouped || ', ';
        END IF;
        grouped := grouped || quote_ident(quasi_identifiers[column_at]);
    END LOOP;
    EXECUTE format('SELECT min(n) FROM (SELECT count(DISTINCT %I) AS n FROM %I.%I GROUP BY %s) g', sensitive_column, schema_name, table_name, grouped) INTO smallest;
    RETURN coalesce(smallest, 0);
END
$$;
CREATE FUNCTION eggdb_privacy.entropy_l_diversity(schema_name text, table_name text, quasi_identifiers text[], sensitive_column text) RETURNS double precision LANGUAGE plpgsql AS $$
DECLARE
    grouped text := '';
    class_columns text := '';
    joined text := '';
    smallest double precision;
BEGIN
    IF coalesce(array_length(quasi_identifiers, 1), 0) = 0 THEN
        RAISE EXCEPTION 'entropy_l_diversity needs at least one quasi-identifier column' USING ERRCODE = '22023';
    END IF;
    FOR column_at IN 1..array_length(quasi_identifiers, 1) LOOP
        IF column_at > 1 THEN
            grouped := grouped || ', ';
            class_columns := class_columns || ', ';
            joined := joined || ' AND ';
        END IF;
        grouped := grouped || quote_ident(quasi_identifiers[column_at]);
        class_columns := class_columns || 'g.' || quote_ident(quasi_identifiers[column_at]);
        joined := joined || format('g.%1$I IS NOT DISTINCT FROM c.%1$I', quasi_identifiers[column_at]);
    END LOOP;
    EXECUTE format('SELECT min(d) FROM (SELECT exp(-sum((g.n::double precision / c.total) * ln(g.n::double precision / c.total))) AS d FROM (SELECT %1$s, %2$I, count(*) AS n FROM %3$I.%4$I WHERE %2$I IS NOT NULL GROUP BY %1$s, %2$I) g JOIN (SELECT %1$s, count(*) AS total FROM %3$I.%4$I WHERE %2$I IS NOT NULL GROUP BY %1$s) c ON %5$s GROUP BY %6$s) classes',
        grouped, sensitive_column, schema_name, table_name, joined, class_columns) INTO smallest;
    RETURN coalesce(smallest, 0);
END
$$;
CREATE TABLE eggdb_privacy.classifications (schema_name text NOT NULL, table_name text NOT NULL, column_name text NOT NULL, category text NOT NULL, share double precision, sampled_rows bigint NOT NULL, classified_at timestamptz NOT NULL DEFAULT now(), PRIMARY KEY (schema_name, table_name, column_name, category));
CREATE FUNCTION eggdb_privacy.luhn(digits text) RETURNS boolean LANGUAGE plpgsql AS $$
DECLARE
    clean text := regexp_replace(digits, '[^0-9]', '', 'g');
    total integer := 0;
    digit integer;
BEGIN
    IF length(clean) < 12 OR length(clean) > 19 THEN
        RETURN false;
    END IF;
    FOR position_from_right IN 1..length(clean) LOOP
        digit := substr(clean, length(clean) - position_from_right + 1, 1)::integer;
        IF position_from_right % 2 = 0 THEN
            digit := digit * 2;
            IF digit > 9 THEN
                digit := digit - 9;
            END IF;
        END IF;
        total := total + digit;
    END LOOP;
    RETURN total % 10 = 0;
END
$$;
CREATE FUNCTION eggdb_privacy.value_category(value text) RETURNS text LANGUAGE plpgsql AS $$
BEGIN
    IF value ~ '^[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}$' THEN
        RETURN 'email';
    ELSIF value ~ '^[0-9][0-9 -]{11,22}[0-9]$' AND eggdb_privacy.luhn(value) THEN
        RETURN 'payment_card';
    ELSIF upper(replace(value, ' ', '')) ~ '^[A-Z]{2}[0-9]{2}[A-Z0-9]{11,30}$' THEN
        RETURN 'iban';
    ELSIF value ~ '^((25[0-5]|2[0-4][0-9]|1?[0-9]?[0-9])\.){3}(25[0-5]|2[0-4][0-9]|1?[0-9]?[0-9])$' THEN
        RETURN 'ip_address';
    ELSIF value ~ '^\+?[0-9][0-9 ().-]{6,18}[0-9]$' THEN
        RETURN 'phone';
    END IF;
    RETURN NULL;
END
$$;
CREATE FUNCTION eggdb_privacy.name_category(column_name text) RETURNS text LANGUAGE plpgsql AS $$
DECLARE
    lowered text := lower(column_name);
BEGIN
    IF lowered ~ '(^|_)(ssn|social_security|national_id|passport|tax_id|cccd|cmnd)($|_)' THEN
        RETURN 'national_id';
    ELSIF lowered ~ '(^|_)(birth|dob|birthday|date_of_birth)($|_)' THEN
        RETURN 'date_of_birth';
    ELSIF lowered ~ '(^|_)((first|last|full|given|family|middle)_?)?name($|_)' AND lowered !~ '(user|file|table|host|domain|product|company)_?name' THEN
        RETURN 'person_name';
    ELSIF lowered ~ '(^|_)(address|street|postcode|zip|zipcode)($|_)' THEN
        RETURN 'address';
    ELSIF lowered ~ '(^|_)(gender|sex|religion|ethnicity|race|health|diagnosis)($|_)' THEN
        RETURN 'sensitive';
    END IF;
    RETURN NULL;
END
$$;
CREATE FUNCTION eggdb_privacy.classify(schema_name text, table_name text, sample_size integer DEFAULT 1000, threshold double precision DEFAULT 0.8) RETURNS TABLE (column_name text, category text, share double precision) LANGUAGE plpgsql AS $$
DECLARE
    found_column record;
    value text;
    sampled bigint;
    by_name text;
    counts jsonb;
    kind text;
    best text;
    best_count bigint;
BEGIN
    DELETE FROM eggdb_privacy.classifications c WHERE c.schema_name = classify.schema_name AND c.table_name = classify.table_name;
    FOR found_column IN SELECT c.column_name AS name, c.data_type FROM information_schema.columns c WHERE c.table_schema = classify.schema_name AND c.table_name = classify.table_name ORDER BY c.ordinal_position LOOP
        by_name := eggdb_privacy.name_category(found_column.name);
        sampled := 0;
        counts := '{}'::jsonb;
        IF found_column.data_type IN ('text', 'character varying', 'character', 'inet') THEN
            FOR value IN EXECUTE format('SELECT %I::text FROM %I.%I WHERE %I IS NOT NULL LIMIT %s', found_column.name, classify.schema_name, classify.table_name, found_column.name, greatest(sample_size, 1)) LOOP
                sampled := sampled + 1;
                kind := eggdb_privacy.value_category(trim(value));
                IF kind IS NOT NULL THEN
                    counts := jsonb_set(counts, ARRAY[kind], to_jsonb(coalesce((counts ->> kind)::bigint, 0) + 1));
                END IF;
            END LOOP;
        END IF;
        best := NULL;
        best_count := 0;
        FOR kind IN SELECT jsonb_object_keys(counts) LOOP
            IF (counts ->> kind)::bigint > best_count THEN
                best := kind;
                best_count := (counts ->> kind)::bigint;
            END IF;
        END LOOP;
        IF best IS NOT NULL AND sampled > 0 AND best_count::double precision / sampled >= threshold THEN
            INSERT INTO eggdb_privacy.classifications (schema_name, table_name, column_name, category, share, sampled_rows)
                VALUES (classify.schema_name, classify.table_name, found_column.name, best, best_count::double precision / sampled, sampled);
            column_name := found_column.name;
            category := best;
            share := best_count::double precision / sampled;
            RETURN NEXT;
        ELSIF by_name IS NOT NULL THEN
            INSERT INTO eggdb_privacy.classifications (schema_name, table_name, column_name, category, share, sampled_rows)
                VALUES (classify.schema_name, classify.table_name, found_column.name, by_name, NULL, sampled);
            column_name := found_column.name;
            category := by_name;
            share := NULL;
            RETURN NEXT;
        END IF;
    END LOOP;
END
$$;
CREATE TABLE eggdb_privacy.budgets (dataset text PRIMARY KEY, epsilon_total double precision NOT NULL CHECK (epsilon_total > 0), epsilon_spent double precision NOT NULL DEFAULT 0);
CREATE TABLE eggdb_privacy.budget_spending (id bigserial PRIMARY KEY, dataset text NOT NULL, epsilon double precision NOT NULL, purpose text NOT NULL, spent_at timestamptz NOT NULL DEFAULT now(), spent_by text NOT NULL DEFAULT current_user);
CREATE FUNCTION eggdb_privacy.spend(dataset text, epsilon double precision, purpose text) RETURNS void LANGUAGE plpgsql AS $$
DECLARE
    remaining double precision;
BEGIN
    IF epsilon IS NULL OR epsilon <= 0 THEN
        RAISE EXCEPTION 'epsilon must be positive' USING ERRCODE = '22023';
    END IF;
    SELECT b.epsilon_total - b.epsilon_spent INTO remaining FROM eggdb_privacy.budgets b WHERE b.dataset = spend.dataset FOR UPDATE;
    IF remaining IS NULL THEN
        RAISE EXCEPTION 'dataset % has no privacy budget', dataset USING ERRCODE = '42704';
    END IF;
    IF epsilon > remaining THEN
        RAISE EXCEPTION 'the privacy budget of % has % left, less than the % asked', dataset, remaining, epsilon USING ERRCODE = '53400';
    END IF;
    UPDATE eggdb_privacy.budgets SET epsilon_spent = epsilon_spent + epsilon WHERE budgets.dataset = spend.dataset;
    INSERT INTO eggdb_privacy.budget_spending (dataset, epsilon, purpose) VALUES (spend.dataset, epsilon, purpose);
END
$$;
CREATE FUNCTION eggdb_privacy.dp_count(dataset text, query text, epsilon double precision, bound double precision DEFAULT NULL) RETURNS double precision LANGUAGE plpgsql AS $$
DECLARE
    exact bigint;
BEGIN
    PERFORM eggdb_privacy.spend(dataset, epsilon, 'count');
    EXECUTE format('SELECT count(*) FROM (%s) q', query) INTO exact;
    IF bound IS NOT NULL THEN
        RETURN greatest(snapping_mechanism(exact, 1, epsilon, bound), 0);
    END IF;
    RETURN greatest(exact + laplace_noise(1, epsilon), 0);
END
$$;
CREATE FUNCTION eggdb_privacy.dp_sum(dataset text, query text, column_name text, lower_bound double precision, upper_bound double precision, epsilon double precision, bound double precision DEFAULT NULL) RETURNS double precision LANGUAGE plpgsql AS $$
DECLARE
    exact double precision;
BEGIN
    IF lower_bound > upper_bound THEN
        RAISE EXCEPTION 'the lower bound is above the upper bound' USING ERRCODE = '22023';
    END IF;
    PERFORM eggdb_privacy.spend(dataset, epsilon, 'sum');
    EXECUTE format('SELECT coalesce(sum(least(greatest(%I::double precision, %s), %s)), 0) FROM (%s) q', column_name, lower_bound, upper_bound, query) INTO exact;
    IF bound IS NOT NULL THEN
        RETURN snapping_mechanism(exact, greatest(abs(lower_bound), abs(upper_bound)), epsilon, bound);
    END IF;
    RETURN exact + laplace_noise(greatest(abs(lower_bound), abs(upper_bound)), epsilon);
END
$$;
CREATE FUNCTION eggdb_privacy.dp_avg(dataset text, query text, column_name text, lower_bound double precision, upper_bound double precision, epsilon double precision) RETURNS double precision LANGUAGE plpgsql AS $$
DECLARE
    total double precision;
    counted double precision;
BEGIN
    total := eggdb_privacy.dp_sum(dataset, query, column_name, lower_bound, upper_bound, epsilon / 2);
    counted := eggdb_privacy.dp_count(dataset, query, epsilon / 2);
    IF counted < 1 THEN
        RETURN NULL;
    END IF;
    RETURN least(greatest(total / counted, lower_bound), upper_bound);
END
$$;
CREATE FUNCTION eggdb_privacy.masked_copy(schema_name text, table_name text, target_table text, masks jsonb) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    found_column record;
    chosen text[] := '{}';
    rule text;
    copied bigint;
BEGIN
    FOR found_column IN SELECT c.column_name AS name, c.data_type FROM information_schema.columns c WHERE c.table_schema = masked_copy.schema_name AND c.table_name = masked_copy.table_name ORDER BY c.ordinal_position LOOP
        rule := masks ->> found_column.name;
        chosen := chosen || CASE
            WHEN rule IS NULL OR rule = 'keep' THEN format('%I', found_column.name)
            WHEN rule = 'null' THEN format('NULL::%s AS %I', found_column.data_type, found_column.name)
            WHEN rule = 'redact' THEN format('CASE WHEN %I IS NULL THEN NULL ELSE repeat(''x'', length(%I::text)) END AS %I', found_column.name, found_column.name, found_column.name)
            WHEN rule = 'hash' THEN format('md5(%I::text) AS %I', found_column.name, found_column.name)
            WHEN rule = 'email' THEN format('CASE WHEN %I IS NULL THEN NULL ELSE ''user'' || substr(md5(%I::text), 1, 8) || ''@example.com'' END AS %I', found_column.name, found_column.name, found_column.name)
            WHEN rule ~ '^keep_last:[0-9]+$' THEN format('CASE WHEN %I IS NULL THEN NULL ELSE repeat(''*'', greatest(length(%I::text) - %s, 0)) || right(%I::text, %s) END AS %I', found_column.name, found_column.name, split_part(rule, ':', 2), found_column.name, split_part(rule, ':', 2), found_column.name)
            ELSE NULL
        END;
        IF chosen[array_length(chosen, 1)] IS NULL THEN
            RAISE EXCEPTION 'unknown mask % for column %', rule, found_column.name USING ERRCODE = '22023';
        END IF;
    END LOOP;
    IF coalesce(array_length(chosen, 1), 0) = 0 THEN
        RAISE EXCEPTION 'table %.% does not exist', schema_name, table_name USING ERRCODE = '42P01';
    END IF;
    EXECUTE format('CREATE TABLE %s AS SELECT %s FROM %I.%I', target_table, array_to_string(chosen, ', '), schema_name, table_name);
    GET DIAGNOSTICS copied = ROW_COUNT;
    RETURN copied;
END
$$;
CREATE FUNCTION eggdb_privacy.synthetic_copy(schema_name text, table_name text, target_table text, row_count bigint, minimum_group integer DEFAULT 5) RETURNS bigint LANGUAGE plpgsql AS $$
DECLARE
    found_column record;
    chosen text[] := '{}';
    null_rate double precision;
    mean double precision;
    spread double precision;
    lowest text;
    highest text;
    choices text[];
    weights bigint[];
    kept bigint;
    running bigint;
    position integer;
    choice text;
    branches text;
    expression text;
    made bigint;
BEGIN
    IF row_count < 0 OR row_count > 100000000 THEN
        RAISE EXCEPTION 'row_count must be 0 to 100000000' USING ERRCODE = '22023';
    END IF;
    IF minimum_group < 1 THEN
        RAISE EXCEPTION 'minimum_group must be at least 1' USING ERRCODE = '22023';
    END IF;
    FOR found_column IN SELECT c.column_name AS name, c.data_type FROM information_schema.columns c WHERE c.table_schema = synthetic_copy.schema_name AND c.table_name = synthetic_copy.table_name ORDER BY c.ordinal_position LOOP
        EXECUTE format('SELECT CASE WHEN count(*) = 0 THEN 1 ELSE 1 - count(%1$I)::double precision / count(*) END FROM %2$I.%3$I', found_column.name, schema_name, table_name) INTO null_rate;
        IF found_column.data_type IN ('ARRAY', 'USER-DEFINED') THEN
            expression := 'NULL';
        ELSIF found_column.data_type IN ('smallint', 'integer', 'bigint', 'numeric', 'real', 'double precision') THEN
            EXECUTE format('SELECT avg(%1$I)::double precision, coalesce(stddev_samp(%1$I), 0)::double precision, min(%1$I)::text, max(%1$I)::text FROM %2$I.%3$I', found_column.name, schema_name, table_name)
                INTO mean, spread, lowest, highest;
            expression := format('least(greatest(%L::double precision + %L::double precision * sqrt(-2 * ln(1 - random())) * cos(2 * pi() * random()), %L::double precision), %L::double precision)',
                coalesce(mean, 0), spread, coalesce(lowest, '0'), coalesce(highest, '0'));
            IF found_column.data_type IN ('smallint', 'integer', 'bigint') THEN
                expression := format('round(%s)::%s', expression, found_column.data_type);
            ELSE
                expression := format('(%s)::%s', expression, found_column.data_type);
            END IF;
        ELSIF found_column.data_type = 'date' THEN
            EXECUTE format('SELECT min(%1$I)::text, max(%1$I)::text FROM %2$I.%3$I', found_column.name, schema_name, table_name) INTO lowest, highest;
            expression := CASE WHEN lowest IS NULL THEN 'NULL::date'
                ELSE format('(%1$L::date + floor(random() * (%2$L::date - %1$L::date + 1))::integer)', lowest, highest) END;
        ELSIF found_column.data_type LIKE 'timestamp%' THEN
            EXECUTE format('SELECT min(%1$I)::text, max(%1$I)::text FROM %2$I.%3$I', found_column.name, schema_name, table_name) INTO lowest, highest;
            expression := CASE WHEN lowest IS NULL THEN format('NULL::%s', found_column.data_type)
                ELSE format('(%1$L::%3$s + (%2$L::%3$s - %1$L::%3$s) * random())', lowest, highest, found_column.data_type) END;
        ELSE
            EXECUTE format('SELECT array_agg(t.v ORDER BY t.n DESC, t.v), array_agg(t.n ORDER BY t.n DESC, t.v), coalesce(sum(t.n), 0) FROM (SELECT %1$I::text AS v, count(*) AS n FROM %2$I.%3$I WHERE %1$I IS NOT NULL GROUP BY 1 HAVING count(*) >= %4$s ORDER BY 2 DESC, 1 LIMIT 100) t',
                found_column.name, schema_name, table_name, minimum_group) INTO choices, weights, kept;
            IF kept = 0 THEN
                expression := 'NULL';
            ELSE
                position := 0;
                running := 0;
                branches := '';
                FOREACH choice IN ARRAY choices LOOP
                    position := position + 1;
                    running := running + weights[position];
                    branches := branches || format(' WHEN x.r < %s THEN %L', running::double precision / kept, choice);
                END LOOP;
                expression := format('(SELECT CASE%s END FROM (SELECT random() + 0 * g.n AS r) x)::%s', branches, found_column.data_type);
            END IF;
        END IF;
        chosen := chosen || format('CASE WHEN random() < %s THEN NULL ELSE %s END AS %I', null_rate, expression, found_column.name);
    END LOOP;
    IF coalesce(array_length(chosen, 1), 0) = 0 THEN
        RAISE EXCEPTION 'table %.% does not exist', schema_name, table_name USING ERRCODE = '42P01';
    END IF;
    EXECUTE format('CREATE TABLE %s AS SELECT * FROM %I.%I WHERE false', target_table, schema_name, table_name);
    EXECUTE format('INSERT INTO %s SELECT %s FROM generate_series(1, %s) AS g(n)', target_table, array_to_string(chosen, ', '), row_count);
    GET DIAGNOSTICS made = ROW_COUNT;
    RETURN made;
END
$$;
"#;

fn invalid(message: impl Into<String>) -> SqlError {
    SqlError::new(SqlState::INVALID_PARAMETER_VALUE, message)
}

fn ff1(name: &str, values: &[Value]) -> SqlResult<Value> {
    let (text, key, tweak, radix) = match values {
        [Value::Text(text), Value::Bytea(key), Value::Bytea(tweak)] => (text, key, tweak, 10),
        [
            Value::Text(text),
            Value::Bytea(key),
            Value::Bytea(tweak),
            Value::Integer(radix),
        ] => (text, key, tweak, *radix),
        _ => {
            return Err(SqlError::new(
                SqlState::UNDEFINED_FUNCTION,
                format!("{name} takes (text, bytea key, bytea tweak [, integer radix])"),
            ));
        }
    };
    let radix = u32::try_from(radix)
        .ok()
        .filter(|radix| (2..=36).contains(radix))
        .ok_or_else(|| invalid(format!("{name} takes a radix from 2 to 36, not {radix}")))?;
    let alphabet = &ALPHABET[..radix as usize];
    let numerals = text
        .chars()
        .map(|character| {
            alphabet
                .find(character)
                .and_then(|at| u32::try_from(at).ok())
        })
        .collect::<Option<Vec<u32>>>()
        .ok_or_else(|| {
            invalid(format!(
                "{name}'s value may only use the characters {alphabet}"
            ))
        })?;
    let cipher = eggdb_crypto::Ff1::new(key, radix)
        .map_err(|_| invalid(format!("{name} takes an AES key of 16, 24 or 32 bytes")))?;
    let result = if name == "ff1_encrypt" {
        cipher.encrypt(&numerals, tweak)
    } else {
        cipher.decrypt(&numerals, tweak)
    };
    let numerals = result.map_err(|error| match error {
        eggdb_crypto::Ff1Error::Length => invalid(format!("{name} needs at least a million possible values ({radix}^length) and halves of at most 120 bits")),
        _ => invalid(format!("{name} cannot use this value or tweak")),
    })?;
    Ok(Value::Text(
        numerals
            .iter()
            .filter_map(|numeral| ALPHABET.as_bytes().get(*numeral as usize))
            .map(|byte| char::from(*byte))
            .collect(),
    ))
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
        return Err(SqlError::new(
            SqlState::UNDEFINED_FUNCTION,
            "laplace_noise takes (sensitivity, epsilon)",
        ));
    };
    let (Some(sensitivity), Some(epsilon)) = (number(sensitivity), number(epsilon)) else {
        return Err(SqlError::new(
            SqlState::UNDEFINED_FUNCTION,
            "laplace_noise takes two numbers",
        ));
    };
    if !(sensitivity.is_finite() && sensitivity > 0.0 && epsilon.is_finite() && epsilon > 0.0) {
        return Err(invalid(
            "laplace_noise needs a positive sensitivity and a positive epsilon",
        ));
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
    Ok(Value::Double(
        -scale * uniform.signum() * (1.0 - 2.0 * uniform.abs()).ln(),
    ))
}

fn uniform_over_doubles() -> SqlResult<f64> {
    let mut bytes = [0u8; 8];
    let mut exponent: i32 = -1;
    loop {
        crate::random::fill(&mut bytes)?;
        let word = u64::from_be_bytes(bytes);
        if word == 0 {
            exponent -= 64;
            if exponent <= LOWEST_EXPONENT {
                return Ok(f64::MIN_POSITIVE);
            }
            continue;
        }
        exponent -= word.leading_zeros() as i32;
        break;
    }
    crate::random::fill(&mut bytes)?;
    let mantissa = u64::from_be_bytes(bytes) >> 12;
    let significand = 1.0 + mantissa as f64 / (1u64 << 52) as f64;
    Ok(significand * 2f64.powi(exponent.max(LOWEST_EXPONENT)))
}

fn snapping_mechanism(values: &[Value]) -> SqlResult<Value> {
    let [value, sensitivity, epsilon, bound] = values else {
        return Err(SqlError::new(SqlState::UNDEFINED_FUNCTION, "snapping_mechanism takes (value, sensitivity, epsilon, bound)"));
    };
    let (Some(value), Some(sensitivity), Some(epsilon), Some(bound)) = (number(value), number(sensitivity), number(epsilon), number(bound)) else {
        return Err(SqlError::new(SqlState::UNDEFINED_FUNCTION, "snapping_mechanism takes four numbers"));
    };
    let positive = |number: f64| number.is_finite() && number > 0.0;
    if !(value.is_finite() && positive(sensitivity) && positive(epsilon) && positive(bound)) {
        return Err(invalid("snapping_mechanism needs a finite value and a positive sensitivity, epsilon and bound"));
    }
    let scale = sensitivity / epsilon;
    if !positive(scale) || scale >= bound {
        return Err(invalid("snapping_mechanism needs a bound above sensitivity / epsilon"));
    }
    let snap = 2f64.powi(scale.log2().ceil() as i32);
    let mut sign = [0u8; 1];
    crate::random::fill(&mut sign)?;
    let direction = if sign[0] & 1 == 0 { 1.0 } else { -1.0 };
    let noisy = value.clamp(-bound, bound) + direction * scale * uniform_over_doubles()?.ln();
    Ok(Value::Double(((noisy / snap).round() * snap).clamp(-bound, bound)))
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
        "snapping_mechanism" => snapping_mechanism(values),
        "gen_random_bytes" => random_bytes(values),
        "eggdb_seal" | "eggdb_unseal" => sealed(name, values),
        "eggdb_blind_index" => blind_index(values),
        _ => ff1(name, values),
    })
}

fn random_bytes(values: &[Value]) -> SqlResult<Value> {
    let count = values.first().and_then(Value::as_integer).filter(|count| (1..=MOST_RANDOM_BYTES).contains(count)).ok_or_else(|| {
        SqlError::new(SqlState::INVALID_PARAMETER_VALUE, format!("gen_random_bytes takes a length from 1 to {MOST_RANDOM_BYTES}"))
    })?;
    let mut bytes = vec![0u8; count as usize];
    crate::random::fill(&mut bytes)?;
    Ok(Value::Bytea(bytes))
}

fn blind_index(values: &[Value]) -> SqlResult<Value> {
    let (value, key) = match values {
        [value, key] | [value, key, _] => (value, key),
        _ => return Err(SqlError::new(SqlState::UNDEFINED_FUNCTION, "eggdb_blind_index takes a value, a key and optionally a bit count")),
    };
    let bits = match values.get(2) {
        Some(bits) => bits.as_integer().filter(|bits| (8..=256).contains(bits) && bits % 8 == 0).ok_or_else(|| {
            SqlError::new(SqlState::INVALID_PARAMETER_VALUE, "eggdb_blind_index keeps 8 to 256 bits, a multiple of 8")
        })?,
        None => DEFAULT_BLIND_INDEX_BITS,
    };
    let key = bytes_of(key);
    if key.len() < 16 {
        return Err(SqlError::new(SqlState::INVALID_PARAMETER_VALUE, "eggdb_blind_index needs a key of at least 16 bytes"));
    }
    let digest = eggdb_crypto::hmac_sha256(&key, &bytes_of(value));
    Ok(Value::Text(digest[..(bits / 8) as usize].iter().map(|byte| format!("{byte:02x}")).collect()))
}

fn bytes_of(value: &Value) -> Vec<u8> {
    match value {
        Value::Bytea(bytes) => bytes.clone(),
        other => other.to_text().map(String::into_bytes).unwrap_or_default(),
    }
}

fn sealed(name: &str, values: &[Value]) -> SqlResult<Value> {
    let [data, key] = values else {
        return Err(SqlError::new(SqlState::UNDEFINED_FUNCTION, format!("{name} takes data and a key")));
    };
    let cipher = eggdb_crypto::AesGcm::new(&bytes_of(key)).map_err(|_| {
        SqlError::new(SqlState::INVALID_PARAMETER_VALUE, format!("{name}: the key must be 16, 24 or 32 bytes"))
    })?;
    let data = bytes_of(data);
    if name == "eggdb_seal" {
        let mut nonce = [0u8; eggdb_crypto::AEAD_NONCE_BYTES];
        crate::random::fill(&mut nonce)?;
        let mut buffer = data;
        let tag = cipher.seal_in_place(&nonce, &[SEAL_VERSION], &mut buffer);
        let mut out = Vec::with_capacity(1 + nonce.len() + buffer.len() + tag.len());
        out.push(SEAL_VERSION);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&buffer);
        out.extend_from_slice(&tag);
        return Ok(Value::Bytea(out));
    }
    let damaged = || SqlError::new(SqlState::INVALID_PARAMETER_VALUE, "eggdb_unseal: the data was not sealed with this key, or was changed");
    let header = 1 + eggdb_crypto::AEAD_NONCE_BYTES;
    if data.len() < header + eggdb_crypto::AEAD_TAG_BYTES || data[0] != SEAL_VERSION {
        return Err(damaged());
    }
    let nonce: [u8; eggdb_crypto::AEAD_NONCE_BYTES] = data[1..header].try_into().map_err(|_| damaged())?;
    let tag_at = data.len() - eggdb_crypto::AEAD_TAG_BYTES;
    let tag: [u8; eggdb_crypto::AEAD_TAG_BYTES] = data[tag_at..].try_into().map_err(|_| damaged())?;
    let mut buffer = data[header..tag_at].to_vec();
    cipher.open_in_place(&nonce, &[SEAL_VERSION], &mut buffer, &tag).map_err(|_| damaged())?;
    Ok(Value::Bytea(buffer))
}

pub(crate) fn result_type(name: &str) -> crate::types::SqlType {
    if matches!(name, "laplace_noise" | "snapping_mechanism") {
        crate::types::SqlType::Double
    } else if matches!(name, "gen_random_bytes" | "eggdb_seal" | "eggdb_unseal") {
        crate::types::SqlType::Bytea
    } else {
        crate::types::SqlType::Text
    }
}

#[cfg(test)]
mod tests {
    use crate::memory_store::MemoryStore;
    use crate::tests::render;
    use crate::{Session, SqlError};

    fn run(session: &mut Session<'_>, sql: &str) -> Result<String, SqlError> {
        let mut outputs = Vec::new();
        session.execute(sql, &mut |result| outputs.push(render(&result)))?;
        Ok(outputs.pop().unwrap_or_default())
    }

    #[test]
    fn ff1_tokens_keep_the_format_and_decrypt_back() {
        crate::memory_store::use_seeded_random_bytes();
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
        crate::memory_store::use_seeded_random_bytes();
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        let summary = run(&mut session, "SELECT abs(avg(x)) < 0.2 AS centred, avg(abs(x)) BETWEEN 1.6 AND 2.4 AS scaled FROM (SELECT laplace_noise(1, 0.5) AS x FROM generate_series(1, 4000)) s").unwrap();
        assert_eq!(summary, "centred|scaled\nt|t\nSELECT 1");
        assert!(run(&mut session, "SELECT laplace_noise(1, 0)").is_err());
    }

    #[test]
    fn k_anonymity_is_the_smallest_group_of_quasi_identifiers() {
        crate::memory_store::use_seeded_random_bytes();
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
        assert!(
            run(
                &mut session,
                "SELECT eggdb_privacy.k_anonymity('public', 'patients', ARRAY[]::text[])"
            )
            .is_err()
        );
    }

    #[test]
    fn synthetic_copies_follow_each_column_and_drop_rare_values() {
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        for sql in [
            "CREATE EXTENSION eggdb_privacy",
            "CREATE TABLE customers (id integer, country text, signed_up date)",
            "INSERT INTO customers SELECT g.n, CASE WHEN g.n <= 6 THEN 'VN' WHEN g.n <= 9 THEN 'SE' ELSE 'XX' END, DATE '2026-01-01' + g.n FROM generate_series(1, 10) AS g(n)",
        ] {
            run(&mut session, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        }
        assert_eq!(
            run(&mut session, "SELECT eggdb_privacy.synthetic_copy('public', 'customers', 'customers_synthetic', 200) AS made").unwrap(),
            "made\n200\nSELECT 1"
        );
        assert_eq!(
            run(&mut session, "SELECT count(DISTINCT country) AS countries, min(id) >= 1 AND max(id) <= 10 AS ids_in_range, min(signed_up) >= DATE '2026-01-02' AND max(signed_up) <= DATE '2026-01-11' AS dates_in_range FROM customers_synthetic").unwrap(),
            "countries|ids_in_range|dates_in_range\n1|t|t\nSELECT 1"
        );
    }

    #[test]
    fn l_diversity_counts_sensitive_values_per_class_and_snapping_stays_in_bounds() {
        crate::memory_store::use_seeded_random_bytes();
        let store = MemoryStore::default();
        let mut session = Session::new(&store);
        for sql in [
            "CREATE EXTENSION eggdb_privacy",
            "CREATE TABLE patients (id integer PRIMARY KEY, zip text, diagnosis text)",
            "INSERT INTO patients VALUES (1, '100', 'flu'), (2, '100', 'flu'), (3, '100', 'cold'), (4, '200', 'flu'), (5, '200', 'asthma'), (6, '200', 'cold')",
        ] {
            run(&mut session, sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
        }
        assert_eq!(run(&mut session, "SELECT eggdb_privacy.l_diversity('public', 'patients', ARRAY['zip'], 'diagnosis') AS l").unwrap(), "l\n2\nSELECT 1");
        assert_eq!(
            run(&mut session, "SELECT round(eggdb_privacy.entropy_l_diversity('public', 'patients', ARRAY['zip'], 'diagnosis')::numeric, 3) AS l").unwrap(),
            "l\n1.890\nSELECT 1"
        );
        assert_eq!(
            run(&mut session, "SELECT bool_and(x BETWEEN -50 AND 50) AS bounded, bool_and(x = round(x / 2) * 2) AS snapped FROM (SELECT snapping_mechanism(10, 1, 0.5, 50) AS x FROM generate_series(1, 500)) s").unwrap(),
            "bounded|snapped\nt|t\nSELECT 1"
        );
        assert!(run(&mut session, "SELECT snapping_mechanism(10, 1, 0.5, 1)").is_err());
    }
}
