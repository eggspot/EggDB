use crate::error::{SqlError, SqlResult, SqlState};
use crate::json::{Json, Node, parse_document};
use crate::types::{SqlType, Value};

pub(crate) const FIT: &str = "eggdb_fit";
pub(crate) const PREDICT: &str = "eggdb_predict";
pub(crate) const FUNCTIONS: [&str; 2] = [FIT, PREDICT];

const MAX_FEATURES: usize = 256;
const MAX_ROWS: usize = 10_000_000;
const LOGISTIC_ITERATIONS: usize = 50;
const RIDGE: f64 = 1e-8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Linear,
    Logistic,
}

impl Kind {
    fn named(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "linear" | "linear_regression" => Some(Self::Linear),
            "logistic" | "logistic_regression" => Some(Self::Logistic),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Linear => "linear",
            Self::Logistic => "logistic",
        }
    }
}

pub(crate) fn result_type(name: &str) -> Option<SqlType> {
    match name {
        FIT => Some(SqlType::Jsonb),
        PREDICT => Some(SqlType::Double),
        _ => None,
    }
}

fn invalid(message: impl Into<String>) -> SqlError {
    SqlError::new(SqlState::INVALID_PARAMETER_VALUE, message)
}

fn number_of(node: &Node) -> Option<f64> {
    match &node.value {
        Json::Number(text) => text.parse::<f64>().ok().filter(|number| number.is_finite()),
        Json::Boolean(flag) => Some(if *flag { 1.0 } else { 0.0 }),
        _ => None,
    }
}

fn json_text(value: &Value) -> Option<String> {
    match value {
        Value::Json(text) | Value::Jsonb(text) | Value::Text(text) => Some(text.clone()),
        _ => None,
    }
}

fn rows_of(value: &Value) -> SqlResult<(Vec<f64>, Vec<Vec<f64>>)> {
    let text = json_text(value).ok_or_else(|| invalid("eggdb_fit takes its rows as a JSON array"))?;
    let document = parse_document(&text).map_err(|error| invalid(error.message))?;
    let Json::Array(rows) = document.value else {
        return Err(invalid("eggdb_fit takes its rows as a JSON array of [target, features...]"));
    };
    if rows.len() > MAX_ROWS {
        return Err(invalid("eggdb_fit takes at most 10,000,000 rows"));
    }
    let mut targets = Vec::with_capacity(rows.len());
    let mut features: Vec<Vec<f64>> = Vec::with_capacity(rows.len());
    let mut width = None;
    for row in rows {
        let Json::Array(items) = row.value else {
            return Err(invalid("each row is a JSON array of [target, features...]"));
        };
        let numbers: Option<Vec<f64>> = items.iter().map(number_of).collect();
        let Some(numbers) = numbers else {
            continue;
        };
        let Some((target, rest)) = numbers.split_first() else {
            continue;
        };
        if rest.len() > MAX_FEATURES {
            return Err(invalid("eggdb_fit takes at most 256 features"));
        }
        match width {
            None => width = Some(rest.len()),
            Some(known) if known != rest.len() => {
                return Err(invalid("every row must have the same number of features"));
            }
            Some(_) => {}
        }
        targets.push(*target);
        features.push(rest.to_vec());
    }
    if targets.is_empty() {
        return Err(invalid("eggdb_fit found no rows without NULLs to fit"));
    }
    Ok((targets, features))
}

fn with_intercept(row: &[f64]) -> Vec<f64> {
    let mut out = Vec::with_capacity(row.len() + 1);
    out.push(1.0);
    out.extend_from_slice(row);
    out
}

fn solve(mut matrix: Vec<Vec<f64>>, mut vector: Vec<f64>) -> Option<Vec<f64>> {
    let size = vector.len();
    for column in 0..size {
        let pivot = (column..size).max_by(|a, b| matrix[*a][column].abs().total_cmp(&matrix[*b][column].abs()))?;
        if matrix[pivot][column].abs() < 1e-12 {
            return None;
        }
        matrix.swap(column, pivot);
        vector.swap(column, pivot);
        for row in column + 1..size {
            let factor = matrix[row][column] / matrix[column][column];
            if factor == 0.0 {
                continue;
            }
            for at in column..size {
                matrix[row][at] -= factor * matrix[column][at];
            }
            vector[row] -= factor * vector[column];
        }
    }
    let mut solution = vec![0.0; size];
    for row in (0..size).rev() {
        let known: f64 = (row + 1..size).map(|at| matrix[row][at] * solution[at]).sum();
        solution[row] = (vector[row] - known) / matrix[row][row];
    }
    Some(solution)
}

fn linear(targets: &[f64], features: &[Vec<f64>]) -> SqlResult<Vec<f64>> {
    let width = features.first().map_or(0, Vec::len) + 1;
    let mut gram = vec![vec![0.0; width]; width];
    let mut moment = vec![0.0; width];
    for (target, row) in targets.iter().zip(features) {
        let row = with_intercept(row);
        for i in 0..width {
            moment[i] += row[i] * target;
            for j in 0..width {
                gram[i][j] += row[i] * row[j];
            }
        }
    }
    for (at, diagonal) in gram.iter_mut().enumerate() {
        diagonal[at] += RIDGE;
    }
    solve(gram, moment).ok_or_else(|| invalid("the features are collinear; drop the redundant ones"))
}

fn sigmoid(value: f64) -> f64 {
    1.0 / (1.0 + (-value).exp())
}

fn logistic(targets: &[f64], features: &[Vec<f64>]) -> SqlResult<Vec<f64>> {
    if targets.iter().any(|target| *target != 0.0 && *target != 1.0) {
        return Err(invalid("logistic regression needs targets of 0 and 1 (or false and true)"));
    }
    let width = features.first().map_or(0, Vec::len) + 1;
    let rows: Vec<Vec<f64>> = features.iter().map(|row| with_intercept(row)).collect();
    let mut weights = vec![0.0; width];
    for _ in 0..LOGISTIC_ITERATIONS {
        let mut hessian = vec![vec![0.0; width]; width];
        let mut gradient = vec![0.0; width];
        for (target, row) in targets.iter().zip(&rows) {
            let linear: f64 = row.iter().zip(&weights).map(|(x, w)| x * w).sum();
            let probability = sigmoid(linear);
            let error = target - probability;
            let curvature = (probability * (1.0 - probability)).max(1e-9);
            for i in 0..width {
                gradient[i] += row[i] * error;
                for j in 0..width {
                    hessian[i][j] += row[i] * row[j] * curvature;
                }
            }
        }
        for (at, diagonal) in hessian.iter_mut().enumerate() {
            diagonal[at] += 1e-6;
            gradient[at] -= 1e-6 * weights[at];
        }
        let Some(step) = solve(hessian, gradient) else {
            break;
        };
        let mut largest: f64 = 0.0;
        for (weight, change) in weights.iter_mut().zip(&step) {
            *weight += change;
            largest = largest.max(change.abs());
        }
        if largest < 1e-10 {
            break;
        }
    }
    Ok(weights)
}

fn model_text(kind: Kind, weights: &[f64], rows: usize) -> String {
    let coefficients: Vec<String> = weights.iter().skip(1).map(|weight| format!("{weight}")).collect();
    format!(
        "{{\"kind\": \"{}\", \"rows\": {rows}, \"intercept\": {}, \"coefficients\": [{}]}}",
        kind.name(),
        weights.first().copied().unwrap_or(0.0),
        coefficients.join(", ")
    )
}

fn fit(kind: &Value, rows: &Value) -> SqlResult<Value> {
    let kind = kind
        .character_text()
        .and_then(Kind::named)
        .ok_or_else(|| invalid("eggdb_fit knows the kinds 'linear' and 'logistic'"))?;
    let (targets, features) = rows_of(rows)?;
    let weights = match kind {
        Kind::Linear => linear(&targets, &features)?,
        Kind::Logistic => logistic(&targets, &features)?,
    };
    if weights.iter().any(|weight| !weight.is_finite()) {
        return Err(invalid("the fit did not converge"));
    }
    let text = model_text(kind, &weights, targets.len());
    let node = crate::json::parse(&text, true)?;
    Ok(Value::Jsonb(crate::json::normalized_value(&node.value)?))
}

fn member<'n>(node: &'n Node, name: &str) -> Option<&'n Node> {
    match &node.value {
        Json::Object(members) => members.iter().find(|(key, _)| key == name).map(|(_, value)| value),
        _ => None,
    }
}

fn predict(model: &Value, features: &Value) -> SqlResult<Value> {
    let text = json_text(model).ok_or_else(|| invalid("eggdb_predict takes a model made by eggdb_fit"))?;
    let document = parse_document(&text).map_err(|error| invalid(error.message))?;
    let kind = match member(&document, "kind").map(|node| &node.value) {
        Some(Json::String(kind)) => Kind::named(kind),
        _ => None,
    }
    .ok_or_else(|| invalid("the model has no kind"))?;
    let intercept = member(&document, "intercept")
        .and_then(number_of)
        .ok_or_else(|| invalid("the model has no intercept"))?;
    let coefficients: Vec<f64> = match member(&document, "coefficients").map(|node| &node.value) {
        Some(Json::Array(items)) => items.iter().filter_map(number_of).collect(),
        _ => return Err(invalid("the model has no coefficients")),
    };
    let inputs: Vec<f64> = match features {
        Value::Array(array) => {
            let mut inputs = Vec::with_capacity(array.items.len());
            for item in &array.items {
                match item.as_float() {
                    Some(number) => inputs.push(number),
                    None => return Ok(Value::Null),
                }
            }
            inputs
        }
        other => match other.as_float() {
            Some(number) => vec![number],
            None => return Ok(Value::Null),
        },
    };
    if inputs.len() != coefficients.len() {
        return Err(invalid(format!(
            "the model takes {} features, not {}",
            coefficients.len(),
            inputs.len()
        )));
    }
    let linear: f64 = intercept
        + coefficients
            .iter()
            .zip(&inputs)
            .map(|(weight, input)| weight * input)
            .sum::<f64>();
    Ok(Value::Double(match kind {
        Kind::Linear => linear,
        Kind::Logistic => sigmoid(linear),
    }))
}

/// `eggdb_fit(kind, rows)` fits a linear or logistic regression to rows given as a JSON array of
/// `[target, feature, ...]` arrays (as `jsonb_agg(jsonb_build_array(...))` makes them) and returns
/// the model as JSON; `eggdb_predict(model, features)` applies one to an array of features.
pub(crate) fn function(name: &str, values: &[Value]) -> Option<SqlResult<Value>> {
    if !FUNCTIONS.contains(&name) {
        return None;
    }
    if values.iter().any(Value::is_null) {
        return Some(Ok(Value::Null));
    }
    Some(match (name, values) {
        (FIT, [kind, rows]) => fit(kind, rows),
        (PREDICT, [model, features]) => predict(model, features),
        _ => Err(SqlError::new(
            SqlState::UNDEFINED_FUNCTION,
            format!("function {name} takes two arguments"),
        )),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(text: &str) -> Value {
        Value::Jsonb(text.to_owned())
    }

    #[test]
    fn a_linear_fit_recovers_its_line_and_predicts_with_it() {
        let model = fit(
            &Value::Text("linear".into()),
            &rows("[[3, 1], [5, 2], [7, 3], [9, 4]]"),
        )
        .unwrap();
        let predicted = predict(&model, &Value::Double(10.0)).unwrap();
        match predicted {
            Value::Double(value) => assert!((value - 21.0).abs() < 1e-6, "{value}"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn a_logistic_fit_separates_its_classes() {
        let model = fit(
            &Value::Text("logistic".into()),
            &rows("[[0, 1], [0, 2], [1, 3], [0, 2.5], [1, 3.5], [1, 4], [0, 1.5], [1, 2.8]]"),
        )
        .unwrap();
        let low = predict(&model, &Value::Double(1.0)).unwrap();
        let high = predict(&model, &Value::Double(4.0)).unwrap();
        match (low, high) {
            (Value::Double(low), Value::Double(high)) => assert!(low < 0.5 && high > 0.5),
            other => panic!("unexpected {other:?}"),
        }
        assert!(fit(&Value::Text("logistic".into()), &rows("[[2, 1]]")).is_err());
    }
}
