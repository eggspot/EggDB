use crate::ast::{Expr, ExprKind, GeneratedTable, Literal, Name, TableFunction};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::Evaluator;
use crate::json::{self, Json, Node};
use crate::json_functions::RecordSink;
use crate::json_path::{self, Failure, JsonPath};
use crate::session::OutputColumn;
use crate::types::{Conversion, DefinedKind, SqlType, Value, convert};
use std::cell::Cell;

/// The internal name of `JSON_EXISTS(...)`.
pub(crate) const EXISTS: &str = "*json_exists*";
/// The internal name of `JSON_VALUE(...)`.
pub(crate) const VALUE: &str = "*json_value*";
/// The internal name of `JSON_QUERY(...)`.
pub(crate) const QUERY: &str = "*json_query*";
/// The internal name of the `JSON_TABLE(...)` table function.
pub(crate) const TABLE: &str = "*json_table*";
/// The marker the parser puts where `json_populate_record` names a table's row type.
pub(crate) const ROW_TYPE: &str = "*row_type*";
/// The functions that fill a row type's columns from a JSON object.
pub(crate) const POPULATE: [&str; 4] = [
    "json_populate_record",
    "jsonb_populate_record",
    "json_populate_recordset",
    "jsonb_populate_recordset",
];

/// The column name PostgreSQL gives a select-list item that calls one of the SQL/JSON functions.
pub(crate) fn column_name(name: &str) -> Option<&'static str> {
    match name {
        EXISTS => Some("json_exists"),
        VALUE => Some("json_value"),
        QUERY => Some("json_query"),
        TABLE => Some("json_table"),
        _ => None,
    }
}

const MAX_TABLE_ROWS: u64 = 1_000_000;
/// The deepest `NESTED PATH` a `JSON_TABLE` may have.
pub(crate) const MAX_NESTING: usize = 32;
const TABLE_FIXED_ARGUMENTS: usize = 3;
const POPULATE_FIXED_ARGUMENTS: usize = 2;

/// What an `ON EMPTY` or `ON ERROR` clause does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Behavior {
    /// `NULL`.
    Null,
    /// `ERROR`: the error is reported.
    Error,
    /// `DEFAULT expression`.
    Default,
    /// `EMPTY [ARRAY]`.
    EmptyArray,
    /// `EMPTY OBJECT`.
    EmptyObject,
    /// `TRUE` (`JSON_EXISTS`).
    True,
    /// `FALSE` (`JSON_EXISTS`).
    False,
    /// `UNKNOWN` (`JSON_EXISTS`).
    Unknown,
}

impl Behavior {
    fn code(self) -> i64 {
        match self {
            Self::Null => 0,
            Self::Error => 1,
            Self::Default => 2,
            Self::EmptyArray => 3,
            Self::EmptyObject => 4,
            Self::True => 5,
            Self::False => 6,
            Self::Unknown => 7,
        }
    }

    fn of_code(code: i64) -> Option<Self> {
        Some(match code {
            0 => Self::Null,
            1 => Self::Error,
            2 => Self::Default,
            3 => Self::EmptyArray,
            4 => Self::EmptyObject,
            5 => Self::True,
            6 => Self::False,
            7 => Self::Unknown,
            _ => return None,
        })
    }
}

/// `JSON_QUERY`'s wrapper clause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Wrapper {
    /// `WITHOUT [ARRAY] WRAPPER`, the default.
    Without,
    /// `WITH CONDITIONAL [ARRAY] WRAPPER`: wraps several items, not a single one.
    Conditional,
    /// `WITH [UNCONDITIONAL] [ARRAY] WRAPPER`: always wraps.
    Unconditional,
}

impl Wrapper {
    fn code(self) -> i64 {
        match self {
            Self::Without => 0,
            Self::Conditional => 1,
            Self::Unconditional => 2,
        }
    }

    fn of_code(code: i64) -> Option<Self> {
        Some(match code {
            0 => Self::Without,
            1 => Self::Conditional,
            2 => Self::Unconditional,
            _ => return None,
        })
    }
}

/// `JSON_QUERY`'s quotes clause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Quotes {
    /// `KEEP QUOTES`, the default: a string result stays a JSON string.
    Keep,
    /// `OMIT QUOTES`: a string result is its text.
    Omit,
}

impl Quotes {
    fn code(self) -> i64 {
        match self {
            Self::Keep => 0,
            Self::Omit => 1,
        }
    }

    fn of_code(code: i64) -> Option<Self> {
        Some(match code {
            0 => Self::Keep,
            1 => Self::Omit,
            _ => return None,
        })
    }
}

/// Which of the three SQL/JSON query functions a call is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QueryKind {
    /// `JSON_EXISTS`.
    Exists,
    /// `JSON_VALUE`.
    Value,
    /// `JSON_QUERY`.
    Query,
}

/// A behavior with the `DEFAULT` expression it names, if any.
pub(crate) struct BehaviorClause {
    pub(crate) behavior: Behavior,
    pub(crate) default: Option<Expr>,
}

impl BehaviorClause {
    /// The clause used when none is written.
    pub(crate) fn of(behavior: Behavior) -> Self {
        Self {
            behavior,
            default: None,
        }
    }
}

/// A parsed `JSON_EXISTS`, `JSON_VALUE` or `JSON_QUERY` call.
pub(crate) struct QueryCall {
    pub(crate) kind: QueryKind,
    pub(crate) document: Expr,
    pub(crate) path: Expr,
    pub(crate) passing: Vec<(String, Expr)>,
    pub(crate) returning: Option<SqlType>,
    pub(crate) wrapper: Wrapper,
    pub(crate) quotes: Quotes,
    pub(crate) on_empty: BehaviorClause,
    pub(crate) on_error: BehaviorClause,
    pub(crate) position: usize,
}

fn literal(value: Literal, position: usize) -> Expr {
    Expr {
        kind: ExprKind::Literal(value),
        position,
    }
}

fn code_literal(code: i64, position: usize) -> Expr {
    literal(Literal::Integer(code), position)
}

fn typed_null(sql_type: Option<SqlType>, position: usize) -> Expr {
    let null = literal(Literal::Null, position);
    match sql_type {
        Some(sql_type) => Expr {
            kind: ExprKind::Cast {
                expr: Box::new(null),
                sql_type,
            },
            position,
        },
        None => null,
    }
}

fn passing_object(passing: Vec<(String, Expr)>, position: usize) -> Expr {
    if passing.is_empty() {
        return literal(Literal::Null, position);
    }
    let mut arguments = Vec::with_capacity(passing.len() * 2);
    for (name, value) in passing {
        arguments.push(literal(Literal::String(name), position));
        arguments.push(value);
    }
    Expr {
        kind: ExprKind::Function {
            name: "jsonb_build_object".to_owned(),
            arguments,
        },
        position,
    }
}

fn default_argument(clause: BehaviorClause, position: usize) -> Expr {
    clause
        .default
        .unwrap_or_else(|| literal(Literal::Null, position))
}

/// Builds the internal call for a parsed SQL/JSON query function.
pub(crate) fn query_call(call: QueryCall) -> Expr {
    let position = call.position;
    let mut arguments = vec![
        call.document,
        call.path,
        passing_object(call.passing, position),
    ];
    let name = match call.kind {
        QueryKind::Exists => {
            arguments.push(code_literal(call.on_error.behavior.code(), position));
            EXISTS
        }
        QueryKind::Value | QueryKind::Query => {
            arguments.push(typed_null(call.returning, position));
            if call.kind == QueryKind::Query {
                arguments.push(code_literal(call.wrapper.code(), position));
                arguments.push(code_literal(call.quotes.code(), position));
            }
            arguments.push(code_literal(call.on_empty.behavior.code(), position));
            arguments.push(default_argument(call.on_empty, position));
            arguments.push(code_literal(call.on_error.behavior.code(), position));
            arguments.push(default_argument(call.on_error, position));
            if call.kind == QueryKind::Query {
                QUERY
            } else {
                VALUE
            }
        }
    };
    Expr {
        kind: ExprKind::Function {
            name: name.to_owned(),
            arguments,
        },
        position,
    }
}

/// Whether `name` is one of the internal SQL/JSON query functions.
pub(crate) fn is_query_function(name: &str) -> bool {
    matches!(name, EXISTS | VALUE | QUERY)
}

fn function_label(name: &str) -> &'static str {
    match name {
        EXISTS => "JSON_EXISTS",
        QUERY => "JSON_QUERY",
        _ => "JSON_VALUE",
    }
}

/// The result type of an internal SQL/JSON query function: `boolean` for `JSON_EXISTS`, the
/// `RETURNING` type for the others, `text` for `JSON_VALUE` and `jsonb` for `JSON_QUERY` by
/// default.
pub(crate) fn query_result_type(
    name: &str,
    arguments: &[Expr],
    scope: crate::eval::Scope<'_>,
) -> Option<SqlType> {
    let returning = || {
        arguments
            .get(3)
            .and_then(|argument| crate::typing::static_type(argument, scope))
    };
    match name {
        EXISTS => Some(SqlType::Boolean),
        VALUE => Some(returning().unwrap_or(SqlType::Text)),
        QUERY => Some(returning().unwrap_or(SqlType::Jsonb)),
        _ => None,
    }
}

fn malformed(name: &str) -> SqlError {
    SqlError::new(
        SqlState::SYNTAX_ERROR,
        format!("malformed {} call", function_label(name)),
    )
}

fn code_at(name: &str, arguments: &[Expr], index: usize) -> SqlResult<i64> {
    match arguments.get(index).map(|argument| &argument.kind) {
        Some(ExprKind::Literal(Literal::Integer(code))) => Ok(*code),
        _ => Err(malformed(name)),
    }
}

fn behavior_at(name: &str, arguments: &[Expr], index: usize) -> SqlResult<Behavior> {
    Behavior::of_code(code_at(name, arguments, index)?).ok_or_else(|| malformed(name))
}

fn argument<'e>(name: &str, arguments: &'e [Expr], index: usize) -> SqlResult<&'e Expr> {
    arguments.get(index).ok_or_else(|| malformed(name))
}

fn soft(state: SqlState, message: impl Into<String>) -> Failure {
    Failure::Suppressible(SqlError::new(state, message))
}

fn document_of(label: &str, value: &Value) -> SqlResult<Json> {
    match json_path::document_tree(value) {
        Some(document) => document,
        None => Err(SqlError::new(
            SqlState::DATATYPE_MISMATCH,
            format!(
                "{label}() argument 1 must be of type jsonb, not {}",
                value.type_name()
            ),
        )),
    }
}

fn vars_of(value: &Value) -> SqlResult<Option<Json>> {
    if value.is_null() {
        return Ok(None);
    }
    let vars = document_of("PASSING", value)?;
    if !matches!(vars, Json::Object(_)) {
        return Err(SqlError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            "the PASSING clause must name each variable",
        ));
    }
    Ok(Some(vars))
}

fn path_of(label: &str, value: &Value) -> SqlResult<JsonPath> {
    let Some(text) = value.character_text() else {
        return Err(SqlError::new(
            SqlState::DATATYPE_MISMATCH,
            format!(
                "{label}() path must be of type jsonpath, not {}",
                value.type_name()
            ),
        ));
    };
    json_path::parse(text)
}

enum Extracted {
    Found(Value),
    Empty,
}

fn typed_text(text: String, returning: SqlType) -> SqlResult<Value> {
    match returning {
        SqlType::Text => Ok(Value::Text(text)),
        SqlType::Json => {
            json::validate(&text)?;
            Ok(Value::Json(text))
        }
        _ => convert(Value::Text(text), returning, Conversion::Explicit),
    }
}

fn json_result(item: &Json, quotes: Quotes, returning: SqlType) -> SqlResult<Value> {
    if let (Quotes::Omit, Json::String(text)) = (quotes, item) {
        return typed_text(text.clone(), returning);
    }
    let text = json::normalized_value(item)?;
    match returning {
        SqlType::Jsonb => Ok(Value::Jsonb(text)),
        SqlType::Json => Ok(Value::Json(text)),
        _ => typed_text(text, returning),
    }
}

fn scalar_result(item: &Json, returning: SqlType) -> SqlResult<Value> {
    if matches!(returning, SqlType::Json | SqlType::Jsonb) {
        return json_result(item, Quotes::Keep, returning);
    }
    let text = match item {
        Json::String(text) | Json::Number(text) => text.clone(),
        Json::Boolean(flag) => flag.to_string(),
        _ => json::normalized_value(item)?,
    };
    typed_text(text, returning)
}

fn extract_value(items: &[Json], returning: SqlType, subject: &str) -> Result<Extracted, Failure> {
    match items {
        [] => Ok(Extracted::Empty),
        [Json::Null] => Ok(Extracted::Found(Value::Null)),
        [Json::Object(_) | Json::Array(_)] => Err(soft(
            SqlState::SQL_JSON_SCALAR_REQUIRED,
            format!("{subject} should return single scalar item"),
        )),
        [item] => scalar_result(item, returning)
            .map(Extracted::Found)
            .map_err(Failure::Suppressible),
        _ => Err(soft(
            SqlState::MORE_THAN_ONE_SQL_JSON_ITEM,
            format!("{subject} should return single item"),
        )),
    }
}

fn extract_query(
    items: Vec<Json>,
    wrapper: Wrapper,
    quotes: Quotes,
    returning: SqlType,
    subject: &str,
) -> Result<Extracted, Failure> {
    let wrap = match wrapper {
        Wrapper::Without => false,
        Wrapper::Conditional => items.len() > 1,
        Wrapper::Unconditional => true,
    };
    let item = if wrap {
        Json::Array(
            items
                .into_iter()
                .map(|value| Node {
                    value,
                    start: 0,
                    end: 0,
                })
                .collect(),
        )
    } else {
        let mut items = items.into_iter();
        match (items.next(), items.next()) {
            (None, _) => return Ok(Extracted::Empty),
            (Some(item), None) => item,
            (Some(_), Some(_)) => {
                return Err(Failure::Suppressible(
                    SqlError::new(
                        SqlState::MORE_THAN_ONE_SQL_JSON_ITEM,
                        format!("{subject} should return single item without wrapper"),
                    )
                    .with_hint("Use the WITH WRAPPER clause to wrap SQL/JSON items into an array."),
                ));
            }
        }
    };
    json_result(&item, quotes, returning)
        .map(Extracted::Found)
        .map_err(Failure::Suppressible)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Clause {
    OnEmpty,
    OnError,
}

fn cast_default(value: Value, returning: SqlType) -> SqlResult<Value> {
    match value {
        Value::Null => Ok(Value::Null),
        Value::Text(text) => typed_text(text, returning),
        value => convert(value, returning, Conversion::Explicit),
    }
}

fn behavior_value(behavior: Behavior, returning: SqlType) -> SqlResult<Value> {
    match behavior {
        Behavior::EmptyArray => json_result(&Json::Array(Vec::new()), Quotes::Keep, returning),
        Behavior::EmptyObject => json_result(&Json::Object(Vec::new()), Quotes::Keep, returning),
        _ => Ok(Value::Null),
    }
}

struct Handling {
    on_empty: Behavior,
    on_error: Behavior,
    returning: SqlType,
}

impl Handling {
    fn finish(
        &self,
        attempt: Result<Extracted, Failure>,
        defaults: &mut dyn FnMut(Clause) -> SqlResult<Value>,
    ) -> SqlResult<Value> {
        let attempt = match attempt {
            Ok(Extracted::Found(value)) => Ok(value),
            Ok(Extracted::Empty) => match self.on_empty {
                Behavior::Error => {
                    return Err(SqlError::new(
                        SqlState::NO_SQL_JSON_ITEM,
                        "no SQL/JSON item found for specified path",
                    ));
                }
                Behavior::Default => defaults(Clause::OnEmpty)
                    .and_then(|value| cast_default(value, self.returning))
                    .map_err(Failure::Suppressible),
                behavior => behavior_value(behavior, self.returning).map_err(Failure::Suppressible),
            },
            Err(failure) => Err(failure),
        };
        match attempt {
            Ok(value) => Ok(value),
            Err(Failure::Fatal(error)) => Err(error),
            Err(Failure::Suppressible(error)) => match self.on_error {
                Behavior::Error => Err(error),
                Behavior::Default => {
                    defaults(Clause::OnError).and_then(|value| cast_default(value, self.returning))
                }
                behavior => behavior_value(behavior, self.returning),
            },
        }
    }
}

fn exists_result(
    attempt: Result<Vec<Json>, Failure>,
    on_error: Behavior,
) -> SqlResult<Option<bool>> {
    match attempt {
        Ok(items) => Ok(Some(!items.is_empty())),
        Err(Failure::Fatal(error)) => Err(error),
        Err(Failure::Suppressible(error)) => match on_error {
            Behavior::Error => Err(error),
            Behavior::True => Ok(Some(true)),
            Behavior::Unknown | Behavior::Null => Ok(None),
            _ => Ok(Some(false)),
        },
    }
}

/// Evaluates an internal `JSON_EXISTS`, `JSON_VALUE` or `JSON_QUERY` call. The `DEFAULT`
/// expressions are evaluated only when their clause applies.
pub(crate) fn query_function(
    evaluator: &Evaluator<'_>,
    name: &str,
    arguments: &[Expr],
) -> SqlResult<Value> {
    let label = function_label(name);
    let expected = match name {
        EXISTS => 4,
        VALUE => 8,
        _ => 10,
    };
    if arguments.len() != expected {
        return Err(malformed(name));
    }
    let document = evaluator.eval(argument(name, arguments, 0)?, Some(SqlType::Jsonb))?;
    let path = evaluator.eval(argument(name, arguments, 1)?, Some(SqlType::Text))?;
    if document.is_null() || path.is_null() {
        return Ok(Value::Null);
    }
    let document = document_of(label, &document)?;
    let path = path_of(label, &path)?;
    let vars = vars_of(&evaluator.eval(argument(name, arguments, 2)?, Some(SqlType::Jsonb))?)?;
    let attempt = json_path::evaluate(&document, &path, vars.as_ref());
    if name == EXISTS {
        let on_error = behavior_at(name, arguments, 3)?;
        return Ok(exists_result(attempt, on_error)?.map_or(Value::Null, Value::Boolean));
    }
    let returning = query_result_type(name, arguments, evaluator.scope).unwrap_or(SqlType::Text);
    let subject = format!("JSON path expression in {label}");
    let first_behavior = if name == QUERY { 6 } else { 4 };
    let handling = Handling {
        on_empty: behavior_at(name, arguments, first_behavior)?,
        on_error: behavior_at(name, arguments, first_behavior + 2)?,
        returning,
    };
    let shape = if name == QUERY {
        let wrapper =
            Wrapper::of_code(code_at(name, arguments, 4)?).ok_or_else(|| malformed(name))?;
        let quotes =
            Quotes::of_code(code_at(name, arguments, 5)?).ok_or_else(|| malformed(name))?;
        Some((wrapper, quotes))
    } else {
        None
    };
    let extracted = attempt.and_then(|items| match shape {
        Some((wrapper, quotes)) => extract_query(items, wrapper, quotes, returning, &subject),
        None => extract_value(&items, returning, &subject),
    });
    handling.finish(extracted, &mut |clause| {
        let index = match clause {
            Clause::OnEmpty => first_behavior + 1,
            Clause::OnError => first_behavior + 3,
        };
        evaluator.eval(argument(name, arguments, index)?, Some(returning))
    })
}

/// One column of a `JSON_TABLE`'s `COLUMNS` clause.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TableColumn {
    /// `name FOR ORDINALITY`.
    Ordinality,
    /// `name type [FORMAT JSON] [PATH path] ...`, read like `JSON_VALUE` or `JSON_QUERY`.
    Regular(RegularColumn),
    /// `name type EXISTS [PATH path] [... ON ERROR]`.
    Exists {
        /// The column's path.
        path: String,
        /// What an error in the path does.
        on_error: Behavior,
    },
    /// `NESTED [PATH] path COLUMNS (...)`.
    Nested {
        /// The path whose items are the child rows.
        path: String,
        /// The child rows' columns.
        columns: Vec<TableColumn>,
    },
}

/// A regular `JSON_TABLE` column.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RegularColumn {
    /// The column's path.
    pub(crate) path: String,
    /// Whether the column is read like `JSON_QUERY` rather than `JSON_VALUE`.
    pub(crate) query: bool,
    /// The wrapper clause.
    pub(crate) wrapper: Wrapper,
    /// The quotes clause.
    pub(crate) quotes: Quotes,
    /// The `ON EMPTY` behavior.
    pub(crate) on_empty: Behavior,
    /// Which of the call's `DEFAULT` expressions `ON EMPTY` names.
    pub(crate) on_empty_default: Option<usize>,
    /// The `ON ERROR` behavior.
    pub(crate) on_error: Behavior,
    /// Which of the call's `DEFAULT` expressions `ON ERROR` names.
    pub(crate) on_error_default: Option<usize>,
}

/// A `JSON_TABLE`'s row path, its `ON ERROR` and its columns.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TablePlan {
    /// The path whose items are the rows.
    pub(crate) path: String,
    /// `ERROR` or `EMPTY` (the default) for an error in the row path.
    pub(crate) on_error: Behavior,
    /// The columns, nested ones included.
    pub(crate) columns: Vec<TableColumn>,
}

struct PlanWriter(String);

impl PlanWriter {
    fn number(&mut self, number: i64) {
        self.0.push_str(&number.to_string());
        self.0.push(';');
    }

    fn text(&mut self, text: &str) {
        self.0.push_str(&text.len().to_string());
        self.0.push(':');
        self.0.push_str(text);
    }

    fn optional(&mut self, index: Option<usize>) {
        self.number(index.map_or(-1, |index| i64::try_from(index).unwrap_or(i64::MAX)));
    }

    fn columns(&mut self, columns: &[TableColumn]) {
        self.number(i64::try_from(columns.len()).unwrap_or(i64::MAX));
        for column in columns {
            match column {
                TableColumn::Ordinality => self.number(0),
                TableColumn::Regular(regular) => {
                    self.number(1);
                    self.text(&regular.path);
                    self.number(i64::from(regular.query));
                    self.number(regular.wrapper.code());
                    self.number(regular.quotes.code());
                    self.number(regular.on_empty.code());
                    self.optional(regular.on_empty_default);
                    self.number(regular.on_error.code());
                    self.optional(regular.on_error_default);
                }
                TableColumn::Exists { path, on_error } => {
                    self.number(2);
                    self.text(path);
                    self.number(on_error.code());
                }
                TableColumn::Nested { path, columns } => {
                    self.number(3);
                    self.text(path);
                    self.columns(columns);
                }
            }
        }
    }
}

struct PlanReader<'t> {
    text: &'t str,
    at: usize,
}

fn damaged_plan() -> SqlError {
    SqlError::new(SqlState::SYNTAX_ERROR, "malformed JSON_TABLE call")
}

impl<'t> PlanReader<'t> {
    fn number(&mut self) -> SqlResult<i64> {
        let rest = self.text.get(self.at..).ok_or_else(damaged_plan)?;
        let end = rest.find(';').ok_or_else(damaged_plan)?;
        let number = rest
            .get(..end)
            .and_then(|digits| digits.parse().ok())
            .ok_or_else(damaged_plan)?;
        self.at += end + 1;
        Ok(number)
    }

    fn text(&mut self) -> SqlResult<&'t str> {
        let rest = self.text.get(self.at..).ok_or_else(damaged_plan)?;
        let colon = rest.find(':').ok_or_else(damaged_plan)?;
        let length: usize = rest
            .get(..colon)
            .and_then(|digits| digits.parse().ok())
            .ok_or_else(damaged_plan)?;
        let start = self.at + colon + 1;
        let end = start.checked_add(length).ok_or_else(damaged_plan)?;
        let text = self.text.get(start..end).ok_or_else(damaged_plan)?;
        self.at = end;
        Ok(text)
    }

    fn behavior(&mut self) -> SqlResult<Behavior> {
        Behavior::of_code(self.number()?).ok_or_else(damaged_plan)
    }

    fn optional(&mut self) -> SqlResult<Option<usize>> {
        let number = self.number()?;
        if number < 0 {
            return Ok(None);
        }
        usize::try_from(number)
            .map(Some)
            .map_err(|_| damaged_plan())
    }

    fn columns(&mut self, depth: usize) -> SqlResult<Vec<TableColumn>> {
        if depth > MAX_NESTING {
            return Err(damaged_plan());
        }
        let count = self.number()?;
        let mut columns = Vec::new();
        for _ in 0..count {
            let column = match self.number()? {
                0 => TableColumn::Ordinality,
                1 => TableColumn::Regular(RegularColumn {
                    path: self.text()?.to_owned(),
                    query: self.number()? != 0,
                    wrapper: Wrapper::of_code(self.number()?).ok_or_else(damaged_plan)?,
                    quotes: Quotes::of_code(self.number()?).ok_or_else(damaged_plan)?,
                    on_empty: self.behavior()?,
                    on_empty_default: self.optional()?,
                    on_error: self.behavior()?,
                    on_error_default: self.optional()?,
                }),
                2 => TableColumn::Exists {
                    path: self.text()?.to_owned(),
                    on_error: self.behavior()?,
                },
                3 => TableColumn::Nested {
                    path: self.text()?.to_owned(),
                    columns: self.columns(depth + 1)?,
                },
                _ => return Err(damaged_plan()),
            };
            columns.push(column);
        }
        Ok(columns)
    }
}

impl TablePlan {
    fn encode(&self) -> String {
        let mut writer = PlanWriter(String::new());
        writer.text(&self.path);
        writer.number(self.on_error.code());
        writer.columns(&self.columns);
        writer.0
    }

    fn decode(text: &str) -> SqlResult<Self> {
        let mut reader = PlanReader { text, at: 0 };
        let path = reader.text()?.to_owned();
        let on_error = reader.behavior()?;
        let columns = reader.columns(0)?;
        if reader.at != text.len() {
            return Err(damaged_plan());
        }
        Ok(Self {
            path,
            on_error,
            columns,
        })
    }
}

/// A parsed `JSON_TABLE` call.
pub(crate) struct TableCall {
    pub(crate) document: Expr,
    pub(crate) plan: TablePlan,
    pub(crate) passing: Vec<(String, Expr)>,
    pub(crate) names: Vec<Name>,
    pub(crate) types: Vec<SqlType>,
    pub(crate) defaults: Vec<Expr>,
    pub(crate) position: usize,
}

/// Builds the internal table function for a parsed `JSON_TABLE`: the document, the plan, the
/// `PASSING` object, one typed placeholder per output column and then the `DEFAULT`
/// expressions.
pub(crate) fn table_function(call: TableCall) -> TableFunction {
    let position = call.position;
    let mut arguments = vec![
        call.document,
        literal(Literal::String(call.plan.encode()), position),
        passing_object(call.passing, position),
    ];
    arguments.extend(
        call.types
            .into_iter()
            .map(|sql_type| typed_null(Some(sql_type), position)),
    );
    arguments.extend(call.defaults);
    TableFunction {
        name: TABLE.to_owned(),
        arguments,
        ordinality: false,
        column_names: call.names,
        position,
    }
}

/// Whether `name` is a table function whose columns come from typed placeholders.
pub(crate) fn defines_columns(name: &str) -> bool {
    name == TABLE || POPULATE.contains(&name)
}

fn fixed_arguments(name: &str) -> usize {
    if name == TABLE {
        TABLE_FIXED_ARGUMENTS
    } else {
        POPULATE_FIXED_ARGUMENTS
    }
}

fn row_type_required(name: &str) -> SqlError {
    SqlError::new(
        SqlState::DATATYPE_MISMATCH,
        format!("first argument of {name} must be a row type"),
    )
}

/// Checks the arguments of `JSON_TABLE`'s internal function.
pub(crate) fn table_type(types: &[Option<SqlType>]) -> SqlResult<SqlType> {
    if types.len() <= TABLE_FIXED_ARGUMENTS {
        return Err(damaged_plan());
    }
    Ok(SqlType::Text)
}

/// Checks the arguments of `json(b)_populate_record(set)` once their row type is filled in.
pub(crate) fn populate_type(types: &[Option<SqlType>]) -> SqlResult<SqlType> {
    if types.len() <= POPULATE_FIXED_ARGUMENTS {
        return Err(row_type_required("json_populate_record"));
    }
    Ok(SqlType::Text)
}

/// The output columns: the declared names with their placeholders' types.
pub(crate) fn columns(
    name: &str,
    names: &[String],
    types: &[Option<SqlType>],
) -> Vec<OutputColumn> {
    names
        .iter()
        .zip(types.iter().skip(fixed_arguments(name)))
        .map(|(name, sql_type)| OutputColumn {
            name: name.clone(),
            sql_type: *sql_type,
        })
        .collect()
}

/// Produces the rows of `JSON_TABLE` or of a `json(b)_populate_record(set)` call.
pub(crate) fn rows(
    name: &str,
    values: &[Value],
    names: &[String],
    types: &[Option<SqlType>],
    sink: &mut RecordSink<'_>,
) -> SqlResult<()> {
    if name == TABLE {
        table_rows(values, names, types, sink)
    } else {
        populate_rows(name, values, names, types, sink)
    }
}

/// Where a `json(b)_populate_record(set)` call takes its row type from.
pub(crate) enum RowType {
    /// A table or view, by the name the call wrote.
    Table(Name),
    /// A composite type's attributes.
    Columns(Vec<(String, Option<SqlType>)>),
}

/// The row type a `json(b)_populate_record(set)` call names, when it is not filled in yet.
pub(crate) fn populate_row_type(generated: &GeneratedTable) -> Option<RowType> {
    let GeneratedTable::Function(function) = generated else {
        return None;
    };
    if !POPULATE.contains(&function.name.as_str()) || function.arguments.len() != 2 {
        return None;
    }
    match &function.arguments.first()?.kind {
        ExprKind::Function { name, arguments } if name == ROW_TYPE => {
            match arguments
                .first()
                .map(|argument| (&argument.kind, argument.position))
            {
                Some((ExprKind::Literal(Literal::String(table)), position)) => {
                    Some(RowType::Table(Name {
                        text: table.clone(),
                        position,
                    }))
                }
                _ => None,
            }
        }
        ExprKind::Cast {
            sql_type: SqlType::Defined(defined),
            ..
        } if defined.kind == DefinedKind::Composite => {
            let attributes = crate::user_types::composite_attributes(defined.oid)?;
            Some(RowType::Columns(
                attributes
                    .into_iter()
                    .map(|attribute| (attribute.name, Some(attribute.sql_type)))
                    .collect(),
            ))
        }
        _ => None,
    }
}

/// The error for a row type naming no table, view or composite type.
pub(crate) fn undefined_row_type(name: &Name) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_OBJECT,
        format!("type \"{}\" does not exist", name.text),
    )
    .at(name.position)
}

/// Fills in a `json(b)_populate_record(set)` call's row type: the marker becomes `NULL` and
/// each column of the row type a typed placeholder.
pub(crate) fn fill_row_type(
    generated: &mut GeneratedTable,
    row_type: Vec<(String, Option<SqlType>)>,
) -> SqlResult<()> {
    let GeneratedTable::Function(function) = generated else {
        return Ok(());
    };
    if !function.column_names.is_empty() {
        return Err(
            SqlError::not_supported(format!("a column alias list on {}()", function.name))
                .at(function.position),
        );
    }
    if row_type.is_empty() {
        return Err(row_type_required(&function.name).at(function.position));
    }
    let position = function.position;
    if let Some(first) = function.arguments.first_mut()
        && matches!(&first.kind, ExprKind::Function { name, .. } if name == ROW_TYPE)
    {
        *first = literal(Literal::Null, position);
    }
    for (name, sql_type) in row_type {
        function.arguments.push(typed_null(sql_type, position));
        function.column_names.push(Name {
            text: name,
            position,
        });
    }
    Ok(())
}

fn not_an_object(name: &str, found: &Json) -> SqlError {
    let what = match found {
        Json::Array(_) => "an array",
        _ => "a scalar",
    };
    SqlError::new(
        SqlState::INVALID_PARAMETER_VALUE,
        format!("cannot call {name} on {what}"),
    )
}

fn base_fields(name: &str, base: &Value, columns: usize) -> SqlResult<Vec<Value>> {
    match base {
        Value::Null => Ok(vec![Value::Null; columns]),
        Value::Defined(defined) => match defined.as_ref() {
            crate::types::DefinedValue::Record(record) if record.fields.len() == columns => {
                Ok(record.fields.clone())
            }
            _ => Err(row_type_required(name)),
        },
        _ => Err(row_type_required(name)),
    }
}

fn populate_rows(
    name: &str,
    values: &[Value],
    names: &[String],
    types: &[Option<SqlType>],
    sink: &mut RecordSink<'_>,
) -> SqlResult<()> {
    let [base, document, ..] = values else {
        return Err(row_type_required(name));
    };
    if names.is_empty() {
        return Err(row_type_required(name));
    }
    let declared: Vec<(&str, Option<SqlType>)> = names
        .iter()
        .map(String::as_str)
        .zip(types.iter().skip(POPULATE_FIXED_ARGUMENTS).copied())
        .collect();
    let defaults = base_fields(name, base, declared.len())?;
    let set = name.ends_with("set");
    let text = match document {
        Value::Null if set => return Ok(()),
        Value::Null => return sink(defaults),
        Value::Json(text) | Value::Jsonb(text) | Value::Text(text) => text.as_str(),
        other => {
            return Err(SqlError::new(
                SqlState::UNDEFINED_FUNCTION,
                format!(
                    "function {name}(record, {}) does not exist",
                    other.type_name()
                ),
            ));
        }
    };
    let binary = name.starts_with("jsonb");
    let parsed = json::parse(text, binary)?;
    let record = |object: &Node| -> SqlResult<Vec<Value>> {
        let Json::Object(members) = &object.value else {
            return Err(not_an_object(name, &object.value));
        };
        declared
            .iter()
            .zip(&defaults)
            .map(|((column, sql_type), default)| {
                match members.iter().rev().find(|(key, _)| key == column) {
                    None => Ok(default.clone()),
                    Some((_, node)) => crate::json_to_record::field(text, node, *sql_type, binary),
                }
            })
            .collect()
    };
    if !set {
        return sink(record(&parsed)?);
    }
    let Json::Array(elements) = &parsed.value else {
        return Err(SqlError::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("argument of {name} must be an array of objects"),
        ));
    };
    for element in elements {
        if !matches!(element.value, Json::Object(_)) {
            return Err(SqlError::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!("argument of {name} must be an array of objects"),
            ));
        }
        sink(record(element)?)?;
    }
    Ok(())
}

enum PreparedColumn {
    Ordinality {
        slot: usize,
    },
    Regular {
        slot: usize,
        path: JsonPath,
        column: RegularColumn,
    },
    Exists {
        slot: usize,
        path: JsonPath,
        on_error: Behavior,
    },
    Nested {
        path: JsonPath,
        columns: Vec<PreparedColumn>,
    },
}

fn prepare(columns: &[TableColumn], next: &mut usize) -> SqlResult<Vec<PreparedColumn>> {
    let mut prepared = Vec::with_capacity(columns.len());
    for column in columns {
        let slot = *next;
        prepared.push(match column {
            TableColumn::Ordinality => {
                *next += 1;
                PreparedColumn::Ordinality { slot }
            }
            TableColumn::Regular(regular) => {
                *next += 1;
                PreparedColumn::Regular {
                    slot,
                    path: json_path::parse(&regular.path)?,
                    column: regular.clone(),
                }
            }
            TableColumn::Exists { path, on_error } => {
                *next += 1;
                PreparedColumn::Exists {
                    slot,
                    path: json_path::parse(path)?,
                    on_error: *on_error,
                }
            }
            TableColumn::Nested { path, columns } => PreparedColumn::Nested {
                path: json_path::parse(path)?,
                columns: prepare(columns, next)?,
            },
        });
    }
    Ok(prepared)
}

struct TableRun<'a> {
    vars: Option<&'a Json>,
    names: &'a [String],
    types: &'a [Option<SqlType>],
    defaults: &'a [Value],
    on_error: Behavior,
    width: usize,
    produced: Cell<u64>,
    row_limit: u64,
}

fn too_many_rows(row_limit: u64) -> SqlError {
    SqlError::new(
        SqlState::PROGRAM_LIMIT_EXCEEDED,
        format!("JSON_TABLE would produce more than {row_limit} rows"),
    )
}

fn ordinal(index: usize) -> i64 {
    i64::try_from(index).map_or(i64::MAX, |index| index.saturating_add(1))
}

impl TableRun<'_> {
    fn charge_one_row(&self) -> SqlResult<()> {
        let total = self.produced.get().saturating_add(1);
        self.produced.set(total);
        if total > self.row_limit {
            return Err(too_many_rows(self.row_limit));
        }
        Ok(())
    }

    fn column_type(&self, slot: usize) -> SqlType {
        self.types
            .get(slot)
            .copied()
            .flatten()
            .unwrap_or(SqlType::Text)
    }

    fn column_name(&self, slot: usize) -> &str {
        self.names.get(slot).map_or("", String::as_str)
    }

    fn default_value(&self, index: Option<usize>) -> SqlResult<Value> {
        index
            .and_then(|index| self.defaults.get(index))
            .cloned()
            .ok_or_else(damaged_plan)
    }

    fn regular(
        &self,
        item: &Json,
        slot: usize,
        path: &JsonPath,
        column: &RegularColumn,
    ) -> SqlResult<Value> {
        let returning = self.column_type(slot);
        let subject = format!(
            "JSON path expression for column \"{}\"",
            self.column_name(slot)
        );
        let extracted = json_path::evaluate(item, path, self.vars).and_then(|items| {
            if column.query {
                extract_query(items, column.wrapper, column.quotes, returning, &subject)
            } else {
                extract_value(&items, returning, &subject)
            }
        });
        let handling = Handling {
            on_empty: column.on_empty,
            on_error: column.on_error,
            returning,
        };
        handling.finish(extracted, &mut |clause| match clause {
            Clause::OnEmpty => self.default_value(column.on_empty_default),
            Clause::OnError => self.default_value(column.on_error_default),
        })
    }

    fn exists(
        &self,
        item: &Json,
        slot: usize,
        path: &JsonPath,
        on_error: Behavior,
    ) -> SqlResult<Value> {
        let found = exists_result(json_path::evaluate(item, path, self.vars), on_error)?;
        match found {
            None => Ok(Value::Null),
            Some(flag) => match self.column_type(slot) {
                SqlType::Boolean => Ok(Value::Boolean(flag)),
                sql_type => convert(Value::Boolean(flag), sql_type, Conversion::Explicit),
            },
        }
    }

    fn items(&self, document: &Json, path: &JsonPath) -> SqlResult<Vec<Json>> {
        match json_path::evaluate(document, path, self.vars) {
            Ok(items) => Ok(items),
            Err(Failure::Suppressible(error)) if self.on_error == Behavior::Error => Err(error),
            Err(Failure::Suppressible(_)) => Ok(Vec::new()),
            Err(Failure::Fatal(error)) => Err(error),
        }
    }

    fn rows_for(
        &self,
        item: &Json,
        position: i64,
        columns: &[PreparedColumn],
    ) -> SqlResult<Vec<Vec<Value>>> {
        crate::stack_depth::check()?;
        let mut own = Vec::new();
        let mut children: Vec<Vec<Value>> = Vec::new();
        for column in columns {
            match column {
                PreparedColumn::Ordinality { slot } => {
                    let number = Value::BigInt(position);
                    let value = match self.column_type(*slot) {
                        SqlType::BigInt => number,
                        sql_type => convert(number, sql_type, Conversion::Explicit)?,
                    };
                    own.push((*slot, value));
                }
                PreparedColumn::Regular { slot, path, column } => {
                    own.push((*slot, self.regular(item, *slot, path, column)?));
                }
                PreparedColumn::Exists {
                    slot,
                    path,
                    on_error,
                } => own.push((*slot, self.exists(item, *slot, path, *on_error)?)),
                PreparedColumn::Nested { path, columns } => {
                    for (index, child) in self.items(item, path)?.iter().enumerate() {
                        children.extend(self.rows_for(child, ordinal(index), columns)?);
                    }
                }
            }
        }
        if children.is_empty() {
            self.charge_one_row()?;
            children.push(vec![Value::Null; self.width]);
        }
        for row in &mut children {
            for (slot, value) in &own {
                if let Some(cell) = row.get_mut(*slot) {
                    cell.clone_from(value);
                }
            }
        }
        Ok(children)
    }
}

fn table_rows(
    values: &[Value],
    names: &[String],
    types: &[Option<SqlType>],
    sink: &mut RecordSink<'_>,
) -> SqlResult<()> {
    table_rows_within(values, names, types, MAX_TABLE_ROWS, sink)
}

fn table_rows_within(
    values: &[Value],
    names: &[String],
    types: &[Option<SqlType>],
    row_limit: u64,
    sink: &mut RecordSink<'_>,
) -> SqlResult<()> {
    let [document, plan, vars, rest @ ..] = values else {
        return Err(damaged_plan());
    };
    let Some(plan) = plan.character_text() else {
        return Err(damaged_plan());
    };
    let plan = TablePlan::decode(plan)?;
    let width = names.len();
    let mut next = 0;
    let columns = prepare(&plan.columns, &mut next)?;
    if next != width || rest.len() < width {
        return Err(damaged_plan());
    }
    if document.is_null() {
        return Ok(());
    }
    let document = document_of("JSON_TABLE", document)?;
    let vars = vars_of(vars)?;
    let root = json_path::parse(&plan.path)?;
    let column_types = types.get(TABLE_FIXED_ARGUMENTS..).unwrap_or(&[]);
    let run = TableRun {
        vars: vars.as_ref(),
        names,
        types: column_types,
        defaults: rest.get(width..).unwrap_or(&[]),
        on_error: plan.on_error,
        width,
        produced: Cell::new(0),
        row_limit,
    };
    for (index, item) in run.items(&document, &root)?.iter().enumerate() {
        for row in run.rows_for(item, ordinal(index), &columns)? {
            sink(row)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plans_survive_encoding_and_reject_damage() {
        let plan = TablePlan {
            path: "$.a[*] ? (@ != \"x;y:z\")".to_owned(),
            on_error: Behavior::Error,
            columns: vec![
                TableColumn::Ordinality,
                TableColumn::Regular(RegularColumn {
                    path: "$.\"é\"".to_owned(),
                    query: true,
                    wrapper: Wrapper::Conditional,
                    quotes: Quotes::Omit,
                    on_empty: Behavior::Default,
                    on_empty_default: Some(0),
                    on_error: Behavior::Null,
                    on_error_default: None,
                }),
                TableColumn::Nested {
                    path: "$.b".to_owned(),
                    columns: vec![TableColumn::Exists {
                        path: "$.c".to_owned(),
                        on_error: Behavior::Unknown,
                    }],
                },
            ],
        };
        let encoded = plan.encode();
        assert_eq!(TablePlan::decode(&encoded).ok(), Some(plan));
        for cut in 0..encoded.len() {
            if let Some(prefix) = encoded.get(..cut) {
                assert!(TablePlan::decode(prefix).is_err());
            }
        }
        assert!(TablePlan::decode("99999999999999999999:x").is_err());
        assert!(TablePlan::decode("1:$0;1;9;").is_err());
    }

    #[test]
    fn deeply_nested_plans_are_refused() {
        let mut text = String::from("1:$0;");
        for _ in 0..=MAX_NESTING + 1 {
            text.push_str("1;3;1:$");
        }
        text.push_str("0;");
        assert!(TablePlan::decode(&text).is_err());
    }

    fn two_level_nested_rows(row_limit: u64) -> SqlResult<Vec<Vec<Value>>> {
        let plan = TablePlan {
            path: "$".to_owned(),
            on_error: Behavior::Error,
            columns: vec![TableColumn::Nested {
                path: "$[*]".to_owned(),
                columns: vec![
                    TableColumn::Ordinality,
                    TableColumn::Nested {
                        path: "$[*]".to_owned(),
                        columns: vec![TableColumn::Ordinality],
                    },
                ],
            }],
        };
        let values = vec![
            Value::Text("[[1, 2, 3], [4, 5, 6]]".into()),
            Value::Text(plan.encode()),
            Value::Null,
            Value::Null,
            Value::Null,
        ];
        let names = vec!["outer".to_owned(), "inner".to_owned()];
        let mut types = vec![Some(SqlType::Text); TABLE_FIXED_ARGUMENTS];
        types.extend([Some(SqlType::BigInt), Some(SqlType::BigInt)]);
        let mut rows = Vec::new();
        table_rows_within(&values, &names, &types, row_limit, &mut |row| {
            rows.push(row);
            Ok(())
        })?;
        Ok(rows)
    }

    #[test]
    fn the_row_limit_counts_each_output_row_once_across_nesting_levels() {
        let rows = two_level_nested_rows(6).map_err(|error| error.message().to_owned());
        assert_eq!(rows.map(|rows| rows.len()), Ok(6));
        let refused = two_level_nested_rows(5).map(|rows| rows.len());
        assert_eq!(
            refused.map_err(|error| error.state()),
            Err(SqlState::PROGRAM_LIMIT_EXCEEDED)
        );
    }
}
