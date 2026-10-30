use crate::aggregate::WindowAggregate;
use crate::ast::{AggregateFunction, BinaryOperator, Expr, ExprKind, Literal, Name, OrderItem};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::{Evaluator, Scope, children, children_mut};
use crate::types::{
    Conversion, SortDirection, SqlType, Value, compare_in_directions, compare_values, convert,
    decode_directions, encode_directions,
};
use std::cmp::Ordering;
use std::ops::Range;

pub(crate) const WINDOW_CALL: &str = "window_call";
const AGGREGATE_PREFIX: &str = "aggregate ";
const HEADER: usize = 9;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WindowFunction {
    RowNumber,
    Rank,
    DenseRank,
    PercentRank,
    CumeDist,
    Ntile,
    Lag,
    Lead,
    FirstValue,
    LastValue,
    NthValue,
    Aggregate(AggregateFunction),
}

impl WindowFunction {
    pub(crate) fn named(name: &str) -> Option<Self> {
        Some(match name {
            "row_number" => Self::RowNumber,
            "rank" => Self::Rank,
            "dense_rank" => Self::DenseRank,
            "percent_rank" => Self::PercentRank,
            "cume_dist" => Self::CumeDist,
            "ntile" => Self::Ntile,
            "lag" => Self::Lag,
            "lead" => Self::Lead,
            "first_value" => Self::FirstValue,
            "last_value" => Self::LastValue,
            "nth_value" => Self::NthValue,
            _ => return None,
        })
    }

    fn decoded(text: &str) -> Option<Self> {
        match text.strip_prefix(AGGREGATE_PREFIX) {
            Some(aggregate) => AggregateFunction::ALL
                .into_iter()
                .find(|function| function.name() == aggregate)
                .map(Self::Aggregate),
            None => Self::named(text),
        }
    }

    fn encoded(self) -> String {
        match self {
            Self::Aggregate(function) => format!("{AGGREGATE_PREFIX}{}", function.name()),
            other => other.name().to_owned(),
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::RowNumber => "row_number",
            Self::Rank => "rank",
            Self::DenseRank => "dense_rank",
            Self::PercentRank => "percent_rank",
            Self::CumeDist => "cume_dist",
            Self::Ntile => "ntile",
            Self::Lag => "lag",
            Self::Lead => "lead",
            Self::FirstValue => "first_value",
            Self::LastValue => "last_value",
            Self::NthValue => "nth_value",
            Self::Aggregate(function) => function.name(),
        }
    }

    pub(crate) fn accepts_arguments(self, count: usize) -> bool {
        match self {
            Self::RowNumber | Self::Rank | Self::DenseRank | Self::PercentRank | Self::CumeDist => {
                count == 0
            }
            Self::Ntile | Self::FirstValue | Self::LastValue => count == 1,
            Self::Lag | Self::Lead => (1..=3).contains(&count),
            Self::NthValue => count == 2,
            Self::Aggregate(_) => count <= 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameUnits {
    Rows,
    Range,
    Groups,
}

impl FrameUnits {
    fn keyword(self) -> &'static str {
        match self {
            Self::Rows => "ROWS",
            Self::Range => "RANGE",
            Self::Groups => "GROUPS",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameBound {
    UnboundedPreceding,
    Preceding,
    CurrentRow,
    Following,
    UnboundedFollowing,
}

impl FrameBound {
    fn has_offset(self) -> bool {
        matches!(self, Self::Preceding | Self::Following)
    }

    fn code(self) -> char {
        match self {
            Self::UnboundedPreceding => 'U',
            Self::Preceding => 'P',
            Self::CurrentRow => 'C',
            Self::Following => 'F',
            Self::UnboundedFollowing => 'V',
        }
    }

    fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            b'U' => Self::UnboundedPreceding,
            b'P' => Self::Preceding,
            b'C' => Self::CurrentRow,
            b'F' => Self::Following,
            b'V' => Self::UnboundedFollowing,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameExclusion {
    NoOthers,
    CurrentRow,
    Group,
    Ties,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Frame {
    pub(crate) units: FrameUnits,
    pub(crate) start: FrameBound,
    pub(crate) end: FrameBound,
    pub(crate) exclusion: FrameExclusion,
}

impl Frame {
    const DEFAULT: Self = Self {
        units: FrameUnits::Range,
        start: FrameBound::UnboundedPreceding,
        end: FrameBound::CurrentRow,
        exclusion: FrameExclusion::NoOthers,
    };

    const WHOLE_PARTITION: Self = Self {
        units: FrameUnits::Rows,
        start: FrameBound::UnboundedPreceding,
        end: FrameBound::UnboundedFollowing,
        exclusion: FrameExclusion::NoOthers,
    };

    fn code(self) -> String {
        let units = match self.units {
            FrameUnits::Rows => 'R',
            FrameUnits::Range => 'A',
            FrameUnits::Groups => 'G',
        };
        let exclusion = match self.exclusion {
            FrameExclusion::NoOthers => 'N',
            FrameExclusion::CurrentRow => 'C',
            FrameExclusion::Group => 'G',
            FrameExclusion::Ties => 'T',
        };
        [units, self.start.code(), self.end.code(), exclusion]
            .into_iter()
            .collect()
    }

    fn from_code(code: &str) -> Option<Self> {
        let [units, start, end, exclusion] = code.as_bytes() else {
            return None;
        };
        Some(Self {
            units: match units {
                b'R' => FrameUnits::Rows,
                b'A' => FrameUnits::Range,
                b'G' => FrameUnits::Groups,
                _ => return None,
            },
            start: FrameBound::from_code(*start)?,
            end: FrameBound::from_code(*end)?,
            exclusion: match exclusion {
                b'N' => FrameExclusion::NoOthers,
                b'C' => FrameExclusion::CurrentRow,
                b'G' => FrameExclusion::Group,
                b'T' => FrameExclusion::Ties,
                _ => return None,
            },
        })
    }

    pub(crate) fn checked(self, position: usize) -> SqlResult<Self> {
        let refused = |message: &str| Err(windowing_error(message).at(position));
        if self.start == FrameBound::UnboundedFollowing {
            return refused("frame start cannot be UNBOUNDED FOLLOWING");
        }
        if self.end == FrameBound::UnboundedPreceding {
            return refused("frame end cannot be UNBOUNDED PRECEDING");
        }
        if self.start == FrameBound::CurrentRow && self.end == FrameBound::Preceding {
            return refused("frame starting from current row cannot have preceding rows");
        }
        if self.start == FrameBound::Following
            && matches!(self.end, FrameBound::Preceding | FrameBound::CurrentRow)
        {
            return refused("frame starting from following row cannot have preceding rows");
        }
        Ok(self)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FrameClause {
    pub(crate) frame: Frame,
    pub(crate) start_offset: Option<Expr>,
    pub(crate) end_offset: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct WindowDefinition {
    pub(crate) base: Option<Name>,
    pub(crate) partition: Vec<Expr>,
    pub(crate) order: Vec<OrderItem>,
    pub(crate) frame: Option<FrameClause>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum WindowReference {
    Inline(WindowDefinition),
    Named(Name),
}

pub(crate) struct WindowCall {
    pub(crate) function: WindowFunction,
    pub(crate) arguments: Vec<Expr>,
    pub(crate) filter: Option<Expr>,
    pub(crate) window: WindowReference,
    pub(crate) position: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WindowSpec {
    pub(crate) function: WindowFunction,
    pub(crate) arguments: usize,
    pub(crate) filtered: bool,
    pub(crate) partition: usize,
    pub(crate) order: Vec<SortDirection>,
    pub(crate) frame: Option<Frame>,
    pub(crate) start_offset: Option<Expr>,
    pub(crate) end_offset: Option<Expr>,
    pub(crate) position: usize,
}

struct Parts {
    function: WindowFunction,
    arguments: Vec<Expr>,
    filter: Option<Expr>,
    partition: Vec<Expr>,
    order: Vec<OrderItem>,
    frame: Option<FrameClause>,
    reference: String,
    position: usize,
}

fn windowing_error(message: impl Into<String>) -> SqlError {
    SqlError::new(SqlState::WINDOWING_ERROR, message)
}

fn undefined_window(name: &str, position: usize) -> SqlError {
    SqlError::new(
        SqlState::UNDEFINED_OBJECT,
        format!("window \"{name}\" does not exist"),
    )
    .at(position)
}

fn lost_definition(position: usize) -> SqlError {
    SqlError::new(
        SqlState::INTERNAL_ERROR,
        "a window call lost its definition",
    )
    .at(position)
}

pub(crate) fn call(call: WindowCall) -> Expr {
    let WindowCall {
        function,
        arguments,
        filter,
        window,
        position,
    } = call;
    let (reference, definition) = match window {
        WindowReference::Named(name) => (format!("={}", name.text), WindowDefinition::default()),
        WindowReference::Inline(definition) => (
            definition
                .base
                .as_ref()
                .map_or_else(String::new, |base| format!("({}", base.text)),
            definition,
        ),
    };
    encode(Parts {
        function,
        arguments,
        filter,
        partition: definition.partition,
        order: definition.order,
        frame: definition.frame,
        reference,
        position,
    })
}

fn encode(parts: Parts) -> Expr {
    let position = parts.position;
    let literal = |literal: Literal| Expr {
        kind: ExprKind::Literal(literal),
        position,
    };
    let directions = parts
        .order
        .iter()
        .map(SortDirection::of)
        .collect::<Vec<_>>();
    let (frame_code, start_offset, end_offset) = match parts.frame {
        Some(clause) => (clause.frame.code(), clause.start_offset, clause.end_offset),
        None => (String::new(), None, None),
    };
    let mut arguments = vec![
        literal(Literal::String(parts.function.encoded())),
        literal(Literal::String(encode_directions(&directions))),
        literal(Literal::Integer(parts.partition.len() as i64)),
        literal(Literal::Integer(parts.arguments.len() as i64)),
        literal(Literal::String(frame_code)),
        start_offset.unwrap_or_else(|| literal(Literal::Null)),
        end_offset.unwrap_or_else(|| literal(Literal::Null)),
        literal(Literal::String(parts.reference)),
        literal(Literal::Boolean(parts.filter.is_some())),
    ];
    arguments.extend(parts.arguments);
    arguments.extend(parts.filter);
    arguments.extend(parts.partition);
    arguments.extend(parts.order.into_iter().map(|item| item.expr));
    Expr {
        kind: ExprKind::Function {
            name: WINDOW_CALL.to_owned(),
            arguments,
        },
        position,
    }
}

fn literal_text(expr: &Expr) -> Option<String> {
    match &expr.kind {
        ExprKind::Literal(Literal::String(text)) => Some(text.clone()),
        _ => None,
    }
}

fn literal_count(expr: &Expr) -> Option<usize> {
    match expr.kind {
        ExprKind::Literal(Literal::Integer(count)) => usize::try_from(count).ok(),
        _ => None,
    }
}

fn given_offset(expr: Expr) -> Option<Expr> {
    (!matches!(expr.kind, ExprKind::Literal(Literal::Null))).then_some(expr)
}

fn parts(expr: Expr) -> Option<Parts> {
    let position = expr.position;
    let ExprKind::Function { name, arguments } = expr.kind else {
        return None;
    };
    if name != WINDOW_CALL || arguments.len() < HEADER {
        return None;
    }
    let mut arguments = arguments.into_iter();
    let mut header: Vec<Expr> = arguments.by_ref().take(HEADER).collect();
    let function = WindowFunction::decoded(&literal_text(header.first()?)?)?;
    let directions = decode_directions(&literal_text(header.get(1)?)?);
    let partition_count = literal_count(header.get(2)?)?;
    let argument_count = literal_count(header.get(3)?)?;
    let frame_code = literal_text(header.get(4)?)?;
    let reference = literal_text(header.get(7)?)?;
    let filtered = matches!(
        header.get(8)?.kind,
        ExprKind::Literal(Literal::Boolean(true))
    );
    header.truncate(7);
    let end_offset = header.pop()?;
    let start_offset = header.pop()?;
    let frame = if frame_code.is_empty() {
        None
    } else {
        Some(FrameClause {
            frame: Frame::from_code(&frame_code)?,
            start_offset: given_offset(start_offset),
            end_offset: given_offset(end_offset),
        })
    };
    let call_arguments: Vec<Expr> = arguments.by_ref().take(argument_count).collect();
    let filter = if filtered {
        Some(arguments.next()?)
    } else {
        None
    };
    let partition: Vec<Expr> = arguments.by_ref().take(partition_count).collect();
    let order_keys: Vec<Expr> = arguments.collect();
    if call_arguments.len() != argument_count
        || partition.len() != partition_count
        || order_keys.len() != directions.len()
    {
        return None;
    }
    let order = order_keys
        .into_iter()
        .zip(directions)
        .map(|(expr, direction)| OrderItem {
            expr,
            descending: direction.descending,
            nulls_first: Some(direction.nulls_first),
        })
        .collect();
    Some(Parts {
        function,
        arguments: call_arguments,
        filter,
        partition,
        order,
        frame,
        reference,
        position,
    })
}

pub(crate) fn is_call(expr: &Expr) -> bool {
    matches!(&expr.kind, ExprKind::Function { name, .. } if name == WINDOW_CALL)
}

pub(crate) fn first_call(expr: &Expr) -> Option<&Expr> {
    if is_call(expr) {
        return Some(expr);
    }
    children(expr).into_iter().find_map(first_call)
}

pub(crate) fn function_name(arguments: &[Expr]) -> String {
    arguments
        .first()
        .and_then(literal_text)
        .and_then(|text| WindowFunction::decoded(&text))
        .map_or_else(|| "?column?".to_owned(), |function| function.name().to_owned())
}

pub(crate) fn split(expr: Expr) -> Option<(WindowSpec, Vec<Expr>)> {
    let parts = parts(expr)?;
    if !parts.reference.is_empty() {
        return None;
    }
    let (frame, start_offset, end_offset) = match parts.frame {
        Some(clause) => (Some(clause.frame), clause.start_offset, clause.end_offset),
        None => (None, None, None),
    };
    let spec = WindowSpec {
        function: parts.function,
        arguments: parts.arguments.len(),
        filtered: parts.filter.is_some(),
        partition: parts.partition.len(),
        order: parts.order.iter().map(SortDirection::of).collect(),
        frame,
        start_offset,
        end_offset,
        position: parts.position,
    };
    let mut keys = parts.arguments;
    keys.extend(parts.filter);
    keys.extend(parts.partition);
    keys.extend(parts.order.into_iter().map(|item| item.expr));
    Some((spec, keys))
}

fn mentions_column(expr: &Expr) -> bool {
    matches!(
        expr.kind,
        ExprKind::Column { .. } | ExprKind::OuterColumn { .. } | ExprKind::Subquery(_)
    ) || children(expr).into_iter().any(mentions_column)
}

pub(crate) fn check_call(expr: &Expr) -> SqlResult<()> {
    let position = expr.position;
    let parts = parts(expr.clone()).ok_or_else(|| lost_definition(position))?;
    if !parts.reference.is_empty() {
        let name = parts
            .reference
            .trim_start_matches(['=', '('])
            .to_owned();
        return Err(undefined_window(&name, position));
    }
    let keys = parts
        .arguments
        .iter()
        .chain(parts.filter.iter())
        .chain(parts.partition.iter())
        .chain(parts.order.iter().map(|item| &item.expr));
    for key in keys {
        if let Some(inner) = first_call(key) {
            return Err(windowing_error("window function calls cannot be nested").at(inner.position));
        }
    }
    let Some(clause) = &parts.frame else {
        return Ok(());
    };
    let frame = clause.frame;
    let offsets = frame.start.has_offset() || frame.end.has_offset();
    if frame.units == FrameUnits::Range && offsets && parts.order.len() != 1 {
        return Err(windowing_error(
            "RANGE with offset PRECEDING/FOLLOWING requires exactly one ORDER BY column",
        )
        .at(position));
    }
    if frame.units == FrameUnits::Groups && parts.order.is_empty() {
        return Err(windowing_error("GROUPS mode requires an ORDER BY clause").at(position));
    }
    for offset in clause.start_offset.iter().chain(clause.end_offset.iter()) {
        if mentions_column(offset) {
            return Err(SqlError::new(
                SqlState::INVALID_COLUMN_REFERENCE,
                format!(
                    "argument of {} must not contain variables",
                    frame.units.keyword()
                ),
            )
            .at(offset.position));
        }
    }
    Ok(())
}

fn inherit(
    partition: &mut Vec<Expr>,
    order: &mut Vec<OrderItem>,
    name: &str,
    definition: &WindowDefinition,
    position: usize,
) -> SqlResult<()> {
    if !partition.is_empty() {
        return Err(windowing_error(format!(
            "cannot override PARTITION BY clause of window \"{name}\""
        ))
        .at(position));
    }
    partition.clone_from(&definition.partition);
    if !definition.order.is_empty() {
        if !order.is_empty() {
            return Err(windowing_error(format!(
                "cannot override ORDER BY clause of window \"{name}\""
            ))
            .at(position));
        }
        order.clone_from(&definition.order);
    }
    if definition.frame.is_some() {
        return Err(windowing_error(format!(
            "cannot copy window \"{name}\" because it has a frame clause"
        ))
        .with_hint("Omit the parentheses in this OVER clause.")
        .at(position));
    }
    Ok(())
}

pub(crate) fn define_windows(
    listed: Vec<(Name, WindowDefinition)>,
) -> SqlResult<Vec<(String, WindowDefinition)>> {
    let mut defined: Vec<(String, WindowDefinition)> = Vec::with_capacity(listed.len());
    for (name, mut definition) in listed {
        if defined.iter().any(|(known, _)| *known == name.text) {
            return Err(windowing_error(format!(
                "window \"{}\" is already defined",
                name.text
            ))
            .at(name.position));
        }
        if let Some(base) = definition.base.take() {
            let found = defined
                .iter()
                .find(|(known, _)| *known == base.text)
                .map(|(_, found)| found.clone())
                .ok_or_else(|| undefined_window(&base.text, base.position))?;
            inherit(
                &mut definition.partition,
                &mut definition.order,
                &base.text,
                &found,
                base.position,
            )?;
        }
        defined.push((name.text, definition));
    }
    Ok(defined)
}

fn merge(parts: &mut Parts, windows: &[(String, WindowDefinition)]) -> SqlResult<()> {
    let reference = std::mem::take(&mut parts.reference);
    let (bare, name) = match reference.strip_prefix('=') {
        Some(name) => (true, name),
        None => (false, reference.strip_prefix('(').unwrap_or(&reference)),
    };
    let definition = windows
        .iter()
        .find(|(known, _)| known == name)
        .map(|(_, definition)| definition)
        .ok_or_else(|| undefined_window(name, parts.position))?;
    if bare {
        parts.partition.clone_from(&definition.partition);
        parts.order.clone_from(&definition.order);
        parts.frame.clone_from(&definition.frame);
        return Ok(());
    }
    inherit(
        &mut parts.partition,
        &mut parts.order,
        name,
        definition,
        parts.position,
    )
}

pub(crate) fn resolve_references(
    expr: &mut Expr,
    windows: &[(String, WindowDefinition)],
) -> SqlResult<()> {
    if is_call(expr) {
        let position = expr.position;
        let taken = std::mem::replace(
            expr,
            Expr {
                kind: ExprKind::Literal(Literal::Null),
                position,
            },
        );
        let mut parts = parts(taken).ok_or_else(|| lost_definition(position))?;
        if !parts.reference.is_empty() {
            merge(&mut parts, windows)?;
        }
        *expr = encode(parts);
        return Ok(());
    }
    for child in children_mut(expr) {
        resolve_references(child, windows)?;
    }
    Ok(())
}

pub(crate) struct WindowResult {
    pub(crate) values: Vec<Value>,
    pub(crate) sql_type: Option<SqlType>,
}

struct Layout {
    filter: Option<usize>,
    partition: Range<usize>,
    order: Range<usize>,
}

impl WindowSpec {
    pub(crate) fn keys(&self) -> usize {
        self.arguments + usize::from(self.filtered) + self.partition + self.order.len()
    }

    fn layout(&self) -> Layout {
        let partition_start = self.arguments + usize::from(self.filtered);
        let order_start = partition_start + self.partition;
        Layout {
            filter: self.filtered.then_some(self.arguments),
            partition: partition_start..order_start,
            order: order_start..order_start + self.order.len(),
        }
    }

    fn argument_type(&self, key_types: &[Option<SqlType>]) -> Option<SqlType> {
        if self.arguments == 0 {
            None
        } else {
            key_types.first().copied().flatten()
        }
    }

    pub(crate) fn result_type(&self, key_types: &[Option<SqlType>]) -> Option<SqlType> {
        match self.function {
            WindowFunction::RowNumber | WindowFunction::Rank | WindowFunction::DenseRank => {
                Some(SqlType::BigInt)
            }
            WindowFunction::Ntile => Some(SqlType::Integer),
            WindowFunction::PercentRank | WindowFunction::CumeDist => Some(SqlType::Double),
            WindowFunction::Lag
            | WindowFunction::Lead
            | WindowFunction::FirstValue
            | WindowFunction::LastValue
            | WindowFunction::NthValue => self.argument_type(key_types),
            WindowFunction::Aggregate(function) => {
                crate::aggregate::window_result_type(function, self.argument_type(key_types))
            }
        }
    }
}

struct Offsets {
    start: Option<Value>,
    end: Option<Value>,
}

fn is_negative(value: &Value) -> bool {
    value.as_float().is_some_and(|number| number < 0.0)
}

fn checked_offset(value: Option<&Value>, which: &str, units: FrameUnits) -> SqlResult<()> {
    match value {
        None | Some(Value::Null) => Err(SqlError::new(
            SqlState::NULL_VALUE_NOT_ALLOWED,
            format!("frame {which} offset must not be null"),
        )),
        Some(value) if is_negative(value) => Err(SqlError::new(
            SqlState::INVALID_PRECEDING_OR_FOLLOWING_SIZE,
            format!("frame {which} offset must not be negative"),
        )),
        Some(value) if units != FrameUnits::Range && value.as_integer().is_none() => {
            Err(SqlError::new(
                SqlState::DATATYPE_MISMATCH,
                format!(
                    "argument of {} must be type bigint, not type {}",
                    units.keyword(),
                    value.type_name()
                ),
            ))
        }
        Some(_) => Ok(()),
    }
}

impl Offsets {
    fn of(spec: &WindowSpec) -> SqlResult<Self> {
        let evaluator = Evaluator {
            scope: Scope::empty(),
            row: &[],
            correlation: None,
        };
        let start = spec
            .start_offset
            .as_ref()
            .map(|expr| evaluator.eval(expr, None))
            .transpose()?;
        let end = spec
            .end_offset
            .as_ref()
            .map(|expr| evaluator.eval(expr, None))
            .transpose()?;
        if let Some(frame) = spec.frame {
            if frame.start.has_offset() {
                checked_offset(start.as_ref(), "starting", frame.units)
                    .map_err(|error| error.at(spec.position))?;
            }
            if frame.end.has_offset() {
                checked_offset(end.as_ref(), "ending", frame.units)
                    .map_err(|error| error.at(spec.position))?;
            }
        }
        Ok(Self { start, end })
    }

    fn count(value: Option<&Value>) -> usize {
        value
            .and_then(Value::as_integer)
            .and_then(|count| usize::try_from(count).ok())
            .unwrap_or(usize::MAX)
    }
}

fn slice<'r>(row: &'r [Value], range: &Range<usize>) -> &'r [Value] {
    row.get(range.clone()).unwrap_or(&[])
}

fn directed(ordering: Ordering, direction: SortDirection) -> Ordering {
    if direction.descending {
        ordering.reverse()
    } else {
        ordering
    }
}

fn first_where(range: Range<usize>, holds: impl Fn(usize) -> bool) -> usize {
    let (mut low, mut high) = (range.start, range.end);
    while low < high {
        let middle = low + (high - low) / 2;
        if holds(middle) {
            high = middle;
        } else {
            low = middle + 1;
        }
    }
    low
}

fn is_overflow(error: &SqlError) -> bool {
    error.state() == SqlState::NUMERIC_VALUE_OUT_OF_RANGE
        || error.state() == SqlState::DATETIME_FIELD_OVERFLOW
}

pub(crate) fn evaluate(
    spec: &WindowSpec,
    keys: &[Vec<Value>],
    key_types: &[Option<SqlType>],
) -> SqlResult<WindowResult> {
    let layout = spec.layout();
    let offsets = Offsets::of(spec)?;
    let sql_type = spec.result_type(key_types);
    let argument_type = spec.argument_type(key_types);
    if let WindowFunction::Aggregate(function) = spec.function {
        crate::aggregate::check_window_argument(function, argument_type)
            .map_err(|error| error.at_if_unplaced(spec.position))?;
    }
    let partition_directions = vec![
        SortDirection {
            descending: false,
            nulls_first: false,
        };
        spec.partition
    ];
    let row = |at: usize| keys.get(at).map_or(&[][..], Vec::as_slice);
    let mut sorted: Vec<usize> = (0..keys.len()).collect();
    sorted.sort_by(|&left, &right| {
        compare_in_directions(
            &partition_directions,
            slice(row(left), &layout.partition),
            slice(row(right), &layout.partition),
        )
        .then_with(|| {
            compare_in_directions(
                &spec.order,
                slice(row(left), &layout.order),
                slice(row(right), &layout.order),
            )
        })
    });
    let mut values = vec![Value::Null; keys.len()];
    let mut start = 0;
    while start < sorted.len() {
        let mut end = start + 1;
        while end < sorted.len()
            && compare_in_directions(
                &partition_directions,
                slice(row(sorted[start]), &layout.partition),
                slice(row(sorted[end]), &layout.partition),
            ) == Ordering::Equal
        {
            end += 1;
        }
        let partition = Partition::new(spec, &layout, keys, &sorted[start..end]);
        partition
            .fill(spec, &offsets, argument_type, sql_type, &mut values)
            .map_err(|error| error.at_if_unplaced(spec.position))?;
        start = end;
    }
    Ok(WindowResult { values, sql_type })
}

struct Partition<'k> {
    keys: &'k [Vec<Value>],
    members: &'k [usize],
    layout: &'k Layout,
    group_of: Vec<usize>,
    groups: Vec<(usize, usize)>,
    non_null: (usize, usize),
}

impl<'k> Partition<'k> {
    fn new(
        spec: &WindowSpec,
        layout: &'k Layout,
        keys: &'k [Vec<Value>],
        members: &'k [usize],
    ) -> Self {
        let mut partition = Self {
            keys,
            members,
            layout,
            group_of: Vec::with_capacity(members.len()),
            groups: Vec::new(),
            non_null: (0, members.len()),
        };
        let mut group_start = 0;
        for position in 0..members.len() {
            if position > 0
                && compare_in_directions(
                    &spec.order,
                    slice(partition.row(position - 1), &layout.order),
                    slice(partition.row(position), &layout.order),
                ) != Ordering::Equal
            {
                partition.groups.push((group_start, position));
                group_start = position;
            }
            partition.group_of.push(partition.groups.len());
        }
        if !members.is_empty() {
            partition.groups.push((group_start, members.len()));
        }
        if let Some(direction) = spec.order.first() {
            let nulls = (0..members.len())
                .filter(|&position| partition.value(position, layout.order.start).is_null())
                .count();
            partition.non_null = if direction.nulls_first {
                (nulls, members.len())
            } else {
                (0, members.len() - nulls)
            };
        }
        partition
    }

    fn len(&self) -> usize {
        self.members.len()
    }

    fn row(&self, position: usize) -> &'k [Value] {
        self.members
            .get(position)
            .and_then(|&at| self.keys.get(at))
            .map_or(&[][..], Vec::as_slice)
    }

    fn value(&self, position: usize, index: usize) -> Value {
        self.row(position)
            .get(index)
            .cloned()
            .unwrap_or(Value::Null)
    }

    fn group(&self, position: usize) -> usize {
        self.group_of.get(position).copied().unwrap_or(0)
    }

    fn group_bounds(&self, group: usize) -> (usize, usize) {
        self.groups
            .get(group)
            .copied()
            .unwrap_or((self.len(), self.len()))
    }

    fn peers(&self, position: usize) -> (usize, usize) {
        self.group_bounds(self.group(position))
    }

    fn store(&self, values: &mut [Value], position: usize, value: Value) {
        if let Some(slot) = self
            .members
            .get(position)
            .and_then(|&at| values.get_mut(at))
        {
            *slot = value;
        }
    }

    fn fill(
        &self,
        spec: &WindowSpec,
        offsets: &Offsets,
        argument_type: Option<SqlType>,
        sql_type: Option<SqlType>,
        values: &mut [Value],
    ) -> SqlResult<()> {
        let count = self.len();
        match spec.function {
            WindowFunction::RowNumber => {
                for position in 0..count {
                    self.store(values, position, Value::BigInt(position as i64 + 1));
                }
            }
            WindowFunction::Rank => {
                for position in 0..count {
                    let rank = self.peers(position).0 as i64 + 1;
                    self.store(values, position, Value::BigInt(rank));
                }
            }
            WindowFunction::DenseRank => {
                for position in 0..count {
                    let rank = self.group(position) as i64 + 1;
                    self.store(values, position, Value::BigInt(rank));
                }
            }
            WindowFunction::PercentRank => {
                for position in 0..count {
                    let rank = if count > 1 {
                        self.peers(position).0 as f64 / (count - 1) as f64
                    } else {
                        0.0
                    };
                    self.store(values, position, Value::Double(rank));
                }
            }
            WindowFunction::CumeDist => {
                for position in 0..count {
                    let share = self.peers(position).1 as f64 / count as f64;
                    self.store(values, position, Value::Double(share));
                }
            }
            WindowFunction::Ntile => self.ntile(values)?,
            WindowFunction::Lag | WindowFunction::Lead => {
                self.shifted(spec, sql_type, values)?;
            }
            WindowFunction::FirstValue
            | WindowFunction::LastValue
            | WindowFunction::NthValue
            | WindowFunction::Aggregate(_) => {
                self.framed(spec, offsets, argument_type, values)?;
            }
        }
        Ok(())
    }

    fn ntile(&self, values: &mut [Value]) -> SqlResult<()> {
        let buckets = self.value(0, 0);
        if buckets.is_null() {
            return Ok(());
        }
        let Some(buckets) = buckets.as_integer() else {
            return Err(SqlError::new(
                SqlState::UNDEFINED_FUNCTION,
                format!("function ntile({}) does not exist", buckets.type_name()),
            ));
        };
        if buckets <= 0 {
            return Err(SqlError::new(
                SqlState::INVALID_ARGUMENT_FOR_NTILE,
                "argument of ntile must be greater than zero",
            ));
        }
        let count = self.len() as i64;
        let per_bucket = count / buckets;
        let larger = count % buckets;
        let larger_rows = larger * (per_bucket + 1);
        for position in 0..self.len() {
            let at = position as i64;
            let bucket = if at < larger_rows {
                at / (per_bucket + 1)
            } else {
                larger + (at - larger_rows) / per_bucket.max(1)
            };
            let bucket = i32::try_from(bucket + 1).unwrap_or(i32::MAX);
            self.store(values, position, Value::Integer(bucket));
        }
        Ok(())
    }

    fn shifted(
        &self,
        spec: &WindowSpec,
        sql_type: Option<SqlType>,
        values: &mut [Value],
    ) -> SqlResult<()> {
        let backward = spec.function == WindowFunction::Lag;
        let count = self.len();
        for position in 0..count {
            let offset = if spec.arguments >= 2 {
                self.value(position, 1)
            } else {
                Value::BigInt(1)
            };
            if offset.is_null() {
                continue;
            }
            let Some(offset) = offset.as_integer() else {
                return Err(SqlError::new(
                    SqlState::UNDEFINED_FUNCTION,
                    format!(
                        "function {}(unknown, {}) does not exist",
                        spec.function.name(),
                        offset.type_name()
                    ),
                ));
            };
            let at = position as i64;
            let target = if backward {
                at.checked_sub(offset)
            } else {
                at.checked_add(offset)
            };
            let found = target
                .and_then(|target| usize::try_from(target).ok())
                .filter(|&target| target < count);
            let value = match found {
                Some(target) => self.value(target, 0),
                None if spec.arguments >= 3 => {
                    let fallback = self.value(position, 2);
                    match (sql_type, fallback.sql_type()) {
                        (Some(wanted), Some(given)) if wanted != given => {
                            convert(fallback, wanted, Conversion::Explicit)?
                        }
                        _ => fallback,
                    }
                }
                None => Value::Null,
            };
            self.store(values, position, value);
        }
        Ok(())
    }

    fn excluded(&self, exclusion: FrameExclusion, position: usize, candidate: usize) -> bool {
        match exclusion {
            FrameExclusion::NoOthers => false,
            FrameExclusion::CurrentRow => candidate == position,
            FrameExclusion::Group => {
                let (start, end) = self.peers(position);
                (start..end).contains(&candidate)
            }
            FrameExclusion::Ties => {
                let (start, end) = self.peers(position);
                candidate != position && (start..end).contains(&candidate)
            }
        }
    }

    fn range_bound(
        &self,
        direction: SortDirection,
        position: usize,
        bound: FrameBound,
        offset: &Value,
        is_start: bool,
    ) -> SqlResult<usize> {
        let order_at = self.layout.order.start;
        let current = self.value(position, order_at);
        let (peer_start, peer_end) = self.peers(position);
        if current.is_null() {
            return Ok(if is_start { peer_start } else { peer_end });
        }
        let (first, last) = self.non_null;
        let preceding = bound == FrameBound::Preceding;
        let operator = if preceding != direction.descending {
            BinaryOperator::Subtract
        } else {
            BinaryOperator::Add
        };
        let target = match crate::eval::arithmetic(operator, &current, offset) {
            Ok(target) => target,
            Err(error) if is_overflow(&error) => return Ok(if preceding { first } else { last }),
            Err(error) => return Err(error),
        };
        let compared = |candidate: usize| {
            directed(
                compare_values(&self.value(candidate, order_at), &target),
                direction,
            )
        };
        Ok(if is_start {
            first_where(first..last, |candidate| {
                compared(candidate) != Ordering::Less
            })
        } else {
            first_where(first..last, |candidate| {
                compared(candidate) == Ordering::Greater
            })
        })
    }

    fn frame_range(
        &self,
        frame: Frame,
        offsets: &Offsets,
        direction: Option<SortDirection>,
        position: usize,
    ) -> SqlResult<(usize, usize)> {
        let count = self.len();
        let start_count = Offsets::count(offsets.start.as_ref());
        let end_count = Offsets::count(offsets.end.as_ref());
        let start = match (frame.units, frame.start) {
            (_, FrameBound::UnboundedPreceding) => 0,
            (_, FrameBound::UnboundedFollowing) => count,
            (FrameUnits::Rows, FrameBound::Preceding) => position.saturating_sub(start_count),
            (FrameUnits::Rows, FrameBound::CurrentRow) => position,
            (FrameUnits::Rows, FrameBound::Following) => {
                position.saturating_add(start_count).min(count)
            }
            (FrameUnits::Groups, FrameBound::Preceding) => {
                let group = self.group(position);
                match group.checked_sub(start_count) {
                    Some(earlier) => self.group_bounds(earlier).0,
                    None => 0,
                }
            }
            (FrameUnits::Groups, FrameBound::CurrentRow)
            | (FrameUnits::Range, FrameBound::CurrentRow) => self.peers(position).0,
            (FrameUnits::Groups, FrameBound::Following) => {
                let group = self.group(position).saturating_add(start_count);
                self.group_bounds(group).0
            }
            (FrameUnits::Range, bound) => match (direction, offsets.start.as_ref()) {
                (Some(direction), Some(offset)) => {
                    self.range_bound(direction, position, bound, offset, true)?
                }
                _ => 0,
            },
        };
        let end = match (frame.units, frame.end) {
            (_, FrameBound::UnboundedFollowing) => count,
            (_, FrameBound::UnboundedPreceding) => 0,
            (FrameUnits::Rows, FrameBound::Preceding) => {
                position.saturating_add(1).saturating_sub(end_count)
            }
            (FrameUnits::Rows, FrameBound::CurrentRow) => position + 1,
            (FrameUnits::Rows, FrameBound::Following) => position
                .saturating_add(end_count)
                .saturating_add(1)
                .min(count),
            (FrameUnits::Groups, FrameBound::Preceding) => {
                let group = self.group(position);
                match group.checked_sub(end_count) {
                    Some(earlier) => self.group_bounds(earlier).1,
                    None => 0,
                }
            }
            (FrameUnits::Groups, FrameBound::CurrentRow)
            | (FrameUnits::Range, FrameBound::CurrentRow) => self.peers(position).1,
            (FrameUnits::Groups, FrameBound::Following) => {
                let group = self.group(position).saturating_add(end_count);
                if group >= self.groups.len() {
                    count
                } else {
                    self.group_bounds(group).1
                }
            }
            (FrameUnits::Range, bound) => match (direction, offsets.end.as_ref()) {
                (Some(direction), Some(offset)) => {
                    self.range_bound(direction, position, bound, offset, false)?
                }
                _ => count,
            },
        };
        Ok((start, end.max(start)))
    }

    fn feed(
        &self,
        aggregate: &mut WindowAggregate,
        spec: &WindowSpec,
        position: usize,
    ) -> SqlResult<()> {
        if let Some(filter) = self.layout.filter
            && !matches!(self.value(position, filter), Value::Boolean(true))
        {
            return Ok(());
        }
        match spec.arguments {
            0 => {
                aggregate.add_row();
                Ok(())
            }
            1 => aggregate.add(self.value(position, 0), None),
            _ => aggregate.add(self.value(position, 0), Some(self.value(position, 1))),
        }
    }

    fn framed(
        &self,
        spec: &WindowSpec,
        offsets: &Offsets,
        argument_type: Option<SqlType>,
        values: &mut [Value],
    ) -> SqlResult<()> {
        let frame = spec.frame.unwrap_or(if spec.order.is_empty() {
            Frame::WHOLE_PARTITION
        } else {
            Frame::DEFAULT
        });
        let direction = spec.order.first().copied();
        let count = self.len();
        if let WindowFunction::Aggregate(function) = spec.function {
            let running = frame.start == FrameBound::UnboundedPreceding
                && frame.exclusion == FrameExclusion::NoOthers;
            let mut aggregate = WindowAggregate::new(function, argument_type, spec.position);
            let mut added = 0;
            for position in 0..count {
                let (start, end) = self.frame_range(frame, offsets, direction, position)?;
                if running {
                    while added < end {
                        self.feed(&mut aggregate, spec, added)?;
                        added += 1;
                    }
                } else {
                    aggregate.reset();
                    for candidate in start..end {
                        if !self.excluded(frame.exclusion, position, candidate) {
                            self.feed(&mut aggregate, spec, candidate)?;
                        }
                    }
                }
                self.store(values, position, aggregate.current()?);
            }
            return Ok(());
        }
        for position in 0..count {
            let (start, end) = self.frame_range(frame, offsets, direction, position)?;
            let mut rows =
                (start..end).filter(|&candidate| !self.excluded(frame.exclusion, position, candidate));
            let chosen = match spec.function {
                WindowFunction::FirstValue => rows.next(),
                WindowFunction::LastValue => rows.last(),
                _ => {
                    let wanted = self.value(position, 1);
                    if wanted.is_null() {
                        continue;
                    }
                    match wanted.as_integer() {
                        Some(number) if number > 0 => {
                            rows.nth(usize::try_from(number - 1).unwrap_or(usize::MAX))
                        }
                        Some(_) => {
                            return Err(SqlError::new(
                                SqlState::INVALID_ARGUMENT_FOR_NTH_VALUE,
                                "argument of nth_value must be greater than zero",
                            ));
                        }
                        None => {
                            return Err(SqlError::new(
                                SqlState::UNDEFINED_FUNCTION,
                                format!(
                                    "function nth_value(unknown, {}) does not exist",
                                    wanted.type_name()
                                ),
                            ));
                        }
                    }
                }
            };
            let value = chosen.map_or(Value::Null, |candidate| self.value(candidate, 0));
            self.store(values, position, value);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Frame, FrameBound, FrameExclusion, FrameUnits, WindowFunction, WindowSpec, evaluate,
    };
    use crate::ast::{AggregateFunction, Expr, ExprKind, Literal};
    use crate::types::{SortDirection, SqlType, Value};

    const ASCENDING: SortDirection = SortDirection {
        descending: false,
        nulls_first: false,
    };

    fn spec(function: WindowFunction, arguments: usize) -> WindowSpec {
        WindowSpec {
            function,
            arguments,
            filtered: false,
            partition: 1,
            order: vec![ASCENDING],
            frame: None,
            start_offset: None,
            end_offset: None,
            position: 0,
        }
    }

    fn integer(value: i64) -> Expr {
        Expr {
            kind: ExprKind::Literal(Literal::Integer(value)),
            position: 0,
        }
    }

    fn keys() -> Vec<Vec<Value>> {
        [(1, 30), (2, 10), (1, 10), (1, 30), (2, 20), (1, 40)]
            .into_iter()
            .map(|(group, value)| vec![Value::Integer(group), Value::Integer(value)])
            .collect()
    }

    fn with_argument() -> Vec<Vec<Value>> {
        keys()
            .into_iter()
            .map(|row| {
                let mut keyed = vec![row[1].clone()];
                keyed.extend(row);
                keyed
            })
            .collect()
    }

    fn values(spec: &WindowSpec, keys: &[Vec<Value>]) -> Vec<Value> {
        let types = vec![Some(SqlType::Integer); spec.keys()];
        evaluate(spec, keys, &types)
            .map(|result| result.values)
            .unwrap_or_default()
    }

    fn big(numbers: &[i64]) -> Vec<Value> {
        numbers.iter().map(|&number| Value::BigInt(number)).collect()
    }

    #[test]
    fn ranking_functions_count_within_each_partition_in_window_order() {
        assert_eq!(
            values(&spec(WindowFunction::RowNumber, 0), &keys()),
            big(&[2, 1, 1, 3, 2, 4])
        );
        assert_eq!(
            values(&spec(WindowFunction::Rank, 0), &keys()),
            big(&[2, 1, 1, 2, 2, 4])
        );
        assert_eq!(
            values(&spec(WindowFunction::DenseRank, 0), &keys()),
            big(&[2, 1, 1, 2, 2, 3])
        );
        assert_eq!(
            values(&spec(WindowFunction::CumeDist, 0), &keys())[5],
            Value::Double(1.0)
        );
    }

    #[test]
    fn ntile_spreads_the_remainder_over_the_first_buckets() {
        let mut ntile = spec(WindowFunction::Ntile, 1);
        ntile.partition = 0;
        ntile.order = Vec::new();
        let rows: Vec<Vec<Value>> = (0..5).map(|_| vec![Value::Integer(2)]).collect();
        assert_eq!(
            values(&ntile, &rows),
            [1, 1, 1, 2, 2].map(Value::Integer).to_vec()
        );
    }

    #[test]
    fn lag_and_lead_read_neighbours_in_the_partition() {
        let lag = values(&spec(WindowFunction::Lag, 1), &with_argument());
        assert_eq!(
            lag,
            vec![
                Value::Integer(10),
                Value::Null,
                Value::Null,
                Value::Integer(30),
                Value::Integer(10),
                Value::Integer(30),
            ]
        );
        let lead = values(&spec(WindowFunction::Lead, 1), &with_argument());
        assert_eq!(lead[2], Value::Integer(30));
        assert_eq!(lead[5], Value::Null);
    }

    #[test]
    fn a_running_sum_includes_peers_by_default() {
        let sum = values(
            &spec(WindowFunction::Aggregate(AggregateFunction::Sum), 1),
            &with_argument(),
        );
        assert_eq!(sum, big(&[70, 10, 10, 70, 30, 110]));
    }

    #[test]
    fn a_rows_frame_slides_over_neighbours() {
        let mut sliding = spec(WindowFunction::Aggregate(AggregateFunction::Sum), 1);
        sliding.frame = Some(Frame {
            units: FrameUnits::Rows,
            start: FrameBound::Preceding,
            end: FrameBound::Following,
            exclusion: FrameExclusion::NoOthers,
        });
        sliding.start_offset = Some(integer(1));
        sliding.end_offset = Some(integer(1));
        assert_eq!(
            values(&sliding, &with_argument()),
            big(&[70, 30, 40, 100, 30, 70])
        );
    }

    #[test]
    fn a_range_frame_with_an_offset_compares_values() {
        let mut ranged = spec(WindowFunction::Aggregate(AggregateFunction::Count), 0);
        ranged.frame = Some(Frame {
            units: FrameUnits::Range,
            start: FrameBound::Preceding,
            end: FrameBound::CurrentRow,
            exclusion: FrameExclusion::NoOthers,
        });
        ranged.start_offset = Some(integer(10));
        assert_eq!(values(&ranged, &keys()), big(&[2, 1, 1, 2, 2, 3]));
    }

    #[test]
    fn exclusion_leaves_out_the_current_row_or_its_peers() {
        let mut excluded = spec(WindowFunction::Aggregate(AggregateFunction::Count), 0);
        excluded.frame = Some(Frame {
            units: FrameUnits::Rows,
            start: FrameBound::UnboundedPreceding,
            end: FrameBound::UnboundedFollowing,
            exclusion: FrameExclusion::Group,
        });
        assert_eq!(values(&excluded, &keys()), big(&[2, 1, 3, 2, 1, 3]));
    }

    #[test]
    fn frames_that_run_backwards_are_refused() {
        let frame = Frame {
            units: FrameUnits::Rows,
            start: FrameBound::CurrentRow,
            end: FrameBound::Preceding,
            exclusion: FrameExclusion::NoOthers,
        };
        assert!(frame.checked(0).is_err());
        assert!(
            Frame {
                start: FrameBound::UnboundedFollowing,
                ..frame
            }
            .checked(0)
            .is_err()
        );
    }

    #[test]
    fn frame_codes_round_trip() {
        let frame = Frame {
            units: FrameUnits::Groups,
            start: FrameBound::Preceding,
            end: FrameBound::UnboundedFollowing,
            exclusion: FrameExclusion::Ties,
        };
        assert_eq!(Frame::from_code(&frame.code()), Some(frame));
    }
}
