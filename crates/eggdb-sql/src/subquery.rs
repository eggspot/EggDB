use crate::ast::{
    BinaryOperator, CommonTable, Expr, ExprKind, FromClause, GeneratedTable, JoinCondition,
    Literal, Query, Select, SelectItem, SetExpr, Subquery, SubqueryRows, SubqueryState,
    SubqueryTest, TableReference,
};
use crate::dialect::Dialect;
use crate::encoding::{TableDefinition, encode_row};
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::{Evaluator, Scope, Source, and, children, children_mut, compare_truth, or};
use crate::execute::{self, TableCache};
use crate::join;
use crate::memory::WorkMemory;
use crate::session::{OutputColumn, QueryResult};
use crate::set_operations::{self, Branch};
use crate::store::KeyValueRead;
use crate::temp_tables::{Intermediate, Overlay, TempTableNames, TempTables};
use crate::types::{SqlType, Value, equality_key};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

const HASHED_FROM: usize = 8;
const MEMO_BYTES: usize = 32 << 20;
const MOST_RECURSION_STEPS: usize = 1_000_000;
const MYSQL_MOST_RECURSION_STEPS: usize = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    Exact,
    Float,
    Text,
    Boolean,
    Local,
    Zoned,
    Jsonb,
    Time,
    TimeTz,
    Interval,
    Bytea,
    Uuid,
}

fn family(value: &Value) -> Option<Family> {
    Some(match value {
        Value::SmallInt(_)
        | Value::Integer(_)
        | Value::BigInt(_)
        | Value::Numeric(_)
        | Value::Oid(_)
        | Value::Reg(_) => Family::Exact,
        Value::Real(_) | Value::Double(_) => Family::Float,
        Value::Text(_) | Value::Bpchar(_) => Family::Text,
        Value::Time(_) => Family::Time,
        Value::TimeTz { .. } => Family::TimeTz,
        Value::Interval(_) => Family::Interval,
        Value::Bytea(_) => Family::Bytea,
        Value::Uuid(_) => Family::Uuid,
        Value::Boolean(_) => Family::Boolean,
        Value::Date(_) | Value::Timestamp(_) => Family::Local,
        Value::TimestampTz(_) => Family::Zoned,
        Value::Jsonb(_) => Family::Jsonb,
        Value::Null | Value::Json(_) | Value::Array(_) => return None,
    })
}

#[derive(Debug)]
pub(crate) struct HashedValues {
    family: Family,
    keys: HashSet<Vec<u8>>,
    has_null: bool,
}

fn hashed(values: &[Value]) -> Option<HashedValues> {
    let mut shared = None;
    let mut keys = HashSet::with_capacity(values.len());
    let mut has_null = false;
    for value in values {
        if value.is_null() {
            has_null = true;
            continue;
        }
        let found = family(value)?;
        if *shared.get_or_insert(found) != found {
            return None;
        }
        let mut key = Vec::new();
        equality_key(&mut key, value);
        keys.insert(key);
    }
    Some(HashedValues {
        family: shared?,
        keys,
        has_null,
    })
}

impl SubqueryRows {
    fn from_result(result: QueryResult, test: &SubqueryTest) -> SqlResult<Self> {
        let sql_type = result
            .columns
            .as_ref()
            .and_then(|columns| columns.first())
            .and_then(|column| column.sql_type);
        let rows = result.rows.len();
        let values: Vec<Value> = if matches!(test, SubqueryTest::Exists) {
            Vec::new()
        } else {
            result
                .rows
                .into_iter()
                .map(|row| row.into_iter().next().unwrap_or(Value::Null))
                .collect()
        };
        let bytes = WorkMemory::row_bytes(&values);
        let hashing = if values.len() >= HASHED_FROM {
            bytes
        } else {
            0
        };
        Ok(Self {
            sql_type,
            values,
            rows,
            hashed: std::sync::OnceLock::new(),
            _held: crate::memory::HeldMemory::holding(bytes.saturating_add(hashing))?,
        })
    }

    fn bytes(&self) -> usize {
        WorkMemory::row_bytes(&self.values)
    }

    fn member(&self, value: &Value) -> SqlResult<Option<bool>> {
        if self.values.is_empty() {
            return Ok(Some(false));
        }
        if value.is_null() {
            return Ok(None);
        }
        if self.values.len() >= HASHED_FROM
            && let Some(hashed) = self.hashed.get_or_init(|| hashed(&self.values))
            && family(value) == Some(hashed.family)
        {
            let mut key = Vec::new();
            equality_key(&mut key, value);
            return Ok(if hashed.keys.contains(&key) {
                Some(true)
            } else if hashed.has_null {
                None
            } else {
                Some(false)
            });
        }
        let mut result = Some(false);
        for item in &self.values {
            result = or(result, compare_truth(value, item, "=")?);
            if result == Some(true) {
                break;
            }
        }
        Ok(result)
    }

    fn quantified(
        &self,
        value: &Value,
        operator: BinaryOperator,
        all: bool,
    ) -> SqlResult<Option<bool>> {
        if operator == BinaryOperator::Equal && !all {
            return self.member(value);
        }
        if operator == BinaryOperator::NotEqual && all {
            return Ok(self.member(value)?.map(|found| !found));
        }
        let mut result = Some(all);
        for item in &self.values {
            let truth = compare_truth(value, item, operator.symbol())?;
            result = if all {
                and(result, truth)
            } else {
                or(result, truth)
            };
            if result == Some(!all) {
                break;
            }
        }
        Ok(result)
    }
}

fn internal(message: &str) -> SqlError {
    SqlError::new(SqlState::INTERNAL_ERROR, message.to_owned())
}

fn more_than_one_row() -> SqlError {
    SqlError::new(
        SqlState::CARDINALITY_VIOLATION,
        "more than one row returned by a subquery used as an expression",
    )
}

pub(crate) fn evaluate(evaluator: &Evaluator<'_>, subquery: &Subquery) -> SqlResult<Value> {
    let rows = match &subquery.state {
        SubqueryState::Rows(rows) => Arc::clone(rows),
        SubqueryState::Correlated { template, .. } => evaluator
            .correlation
            .ok_or_else(|| {
                SqlError::not_supported(
                    "a subquery that refers to the outer query here is not supported yet",
                )
            })?
            .rows(*template, evaluator.row)?,
        SubqueryState::Unprepared => {
            return Err(SqlError::not_supported(
                "a subquery in this part of a statement is not supported yet",
            ));
        }
    };
    let truth = |found: Option<bool>| Ok(found.map_or(Value::Null, Value::Boolean));
    match &subquery.test {
        SubqueryTest::Scalar => {
            if rows.rows > 1 && subquery.dialect != Dialect::Sqlite {
                return Err(more_than_one_row());
            }
            Ok(rows.values.first().cloned().unwrap_or(Value::Null))
        }
        SubqueryTest::Exists => Ok(Value::Boolean(rows.rows > 0)),
        SubqueryTest::Array => crate::arrays::from_rows(rows.values.clone(), rows.sql_type),
        SubqueryTest::In { expr, negated } => {
            let value = evaluator.eval(expr, rows.sql_type)?;
            truth(rows.member(&value)?.map(|found| found != *negated))
        }
        SubqueryTest::Compare {
            expr,
            operator,
            all,
        } => {
            let value = evaluator.eval(expr, rows.sql_type)?;
            truth(rows.quantified(&value, *operator, *all)?)
        }
    }
}

pub(crate) fn result_type(subquery: &Subquery) -> Option<SqlType> {
    match &subquery.test {
        SubqueryTest::Scalar => match &subquery.state {
            SubqueryState::Rows(rows) => rows.sql_type,
            SubqueryState::Correlated { sql_type, .. } => *sql_type,
            SubqueryState::Unprepared => None,
        },
        SubqueryTest::Array => match &subquery.state {
            SubqueryState::Rows(rows) => rows.sql_type,
            SubqueryState::Correlated { sql_type, .. } => *sql_type,
            SubqueryState::Unprepared => None,
        }
        .and_then(crate::arrays::ArrayElement::of)
        .map(SqlType::Array),
        _ => Some(SqlType::Boolean),
    }
}

pub(crate) fn first_column_name(query: &Query) -> Option<String> {
    let mut body = &query.body;
    loop {
        match body {
            SetExpr::Select(select) => {
                return match select.items.first()? {
                    SelectItem::Expr {
                        alias: Some(alias), ..
                    } => Some(alias.clone()),
                    SelectItem::Expr { expr, .. } => Some(crate::eval::output_name(expr)),
                    SelectItem::Wildcard { .. } => None,
                };
            }
            SetExpr::Operation(operation) => body = &operation.left,
            SetExpr::Query(inner) => body = &inner.body,
        }
    }
}

#[derive(Clone, Copy)]
struct Outer<'a> {
    scope: Scope<'a>,
    next: Option<&'a Outer<'a>>,
}

fn mentions_inner_column(expr: &Expr) -> bool {
    matches!(expr.kind, ExprKind::Column { .. })
        || children(expr).into_iter().any(mentions_inner_column)
}

pub(crate) struct Template {
    query: Query,
    prepared: Prepared,
    outer: Vec<usize>,
    test: SubqueryTest,
    lookup: Option<KeyLookup>,
}

struct KeyLookup {
    outer: Vec<usize>,
    families: Vec<Option<Family>>,
    keys: HashSet<Vec<u8>>,
    found: Arc<SubqueryRows>,
    missing: Arc<SubqueryRows>,
    _held: crate::memory::HeldMemory,
}

impl KeyLookup {
    fn probe(&self, row: &[Value]) -> Option<Arc<SubqueryRows>> {
        let mut key = Vec::new();
        for (&index, family_of_keys) in self.outer.iter().zip(&self.families) {
            let value = row.get(index)?;
            if value.is_null() {
                return Some(Arc::clone(&self.missing));
            }
            match family_of_keys {
                None => return Some(Arc::clone(&self.missing)),
                Some(known) if family(value) != Some(*known) => return None,
                Some(_) => equality_key(&mut key, value),
            }
        }
        Some(Arc::clone(if self.keys.contains(&key) {
            &self.found
        } else {
            &self.missing
        }))
    }
}

fn existence(rows: usize) -> SqlResult<Arc<SubqueryRows>> {
    Ok(Arc::new(SubqueryRows {
        sql_type: None,
        values: Vec::new(),
        rows,
        hashed: std::sync::OnceLock::new(),
        _held: crate::memory::HeldMemory::holding(0)?,
    }))
}

fn split_and(expr: Expr) -> Vec<Expr> {
    match expr.kind {
        ExprKind::And(parts) => parts,
        kind => vec![Expr {
            kind,
            position: expr.position,
        }],
    }
}

fn mentions_outer(expr: &Expr) -> bool {
    let mut found = Vec::new();
    collect_outer(expr, &mut found);
    !found.is_empty()
}

fn key_query(query: &Query) -> Option<(Query, Vec<usize>)> {
    let SetExpr::Select(select) = &query.body else {
        return None;
    };
    if !query.with.is_empty()
        || !query.order_by.is_empty()
        || query.offset.is_some()
        || select.offset.is_some()
        || select.source_row_limit.is_some()
        || !select.group_by.is_empty()
        || select.having.is_some()
        || crate::aggregate::is_aggregate_query(select)
    {
        return None;
    }
    if let Some(from) = &select.from
        && (from
            .joins
            .iter()
            .any(|join| matches!(&join.condition, JoinCondition::On(on) if mentions_outer(on)))
            || std::iter::once(&from.first)
                .chain(from.joins.iter().map(|join| &join.table))
                .any(|table| table.generated.is_some()))
    {
        return None;
    }
    let mut outer = Vec::new();
    let mut items = Vec::new();
    let mut kept = Vec::new();
    for conjunct in split_and(select.filter.clone()?) {
        if !mentions_outer(&conjunct) {
            kept.push(conjunct);
            continue;
        }
        let ExprKind::Binary(BinaryOperator::Equal, left, right) = &conjunct.kind else {
            return None;
        };
        let (inner, index) = match (&left.kind, &right.kind) {
            (_, ExprKind::OuterColumn { index, .. }) if !mentions_outer(left) => (left, *index),
            (ExprKind::OuterColumn { index, .. }, _) if !mentions_outer(right) => (right, *index),
            _ => return None,
        };
        outer.push(index);
        items.push(SelectItem::Expr {
            expr: (**inner).clone(),
            alias: None,
        });
    }
    if outer.is_empty() {
        return None;
    }
    let position = select.filter.as_ref().map_or(0, |filter| filter.position);
    let filter = match kept.len() {
        0 => None,
        1 => kept.pop(),
        _ => Some(Expr {
            kind: ExprKind::And(kept),
            position,
        }),
    };
    let keys = Select {
        distinct: true,
        items,
        from: select.from.clone(),
        filter,
        group_by: Vec::new(),
        having: None,
        order_by: Vec::new(),
        limit: None,
        offset: None,
        source_row_limit: None,
    };
    Some((Query::of_select(keys, query.position), outer))
}

fn build_lookup(
    env: &Env<'_>,
    tables: &mut TableCache,
    query: &Query,
    prepared: &Prepared,
) -> SqlResult<Option<KeyLookup>> {
    let Some((keys_query, outer)) = key_query(query) else {
        return Ok(None);
    };
    let result = execute_query(env, tables, &keys_query, prepared)?;
    let mut families: Vec<Option<Family>> = vec![None; outer.len()];
    let mut mixed = false;
    let mut keys = HashSet::with_capacity(result.rows.len());
    let mut bytes = 0usize;
    for row in &result.rows {
        if row.iter().any(Value::is_null) {
            continue;
        }
        let mut key = Vec::new();
        for (value, known) in row.iter().zip(families.iter_mut()) {
            let found = family(value);
            match known {
                None => *known = found,
                Some(existing) if Some(*existing) != found => mixed = true,
                Some(_) => {}
            }
            equality_key(&mut key, value);
        }
        bytes = bytes.saturating_add(key.len() + 48);
        keys.insert(key);
    }
    if mixed || families.iter().any(Option::is_none) && !keys.is_empty() {
        return Ok(None);
    }
    Ok(Some(KeyLookup {
        outer,
        families,
        keys,
        found: existence(1)?,
        missing: existence(0)?,
        _held: crate::memory::HeldMemory::holding(bytes)?,
    }))
}

pub(crate) struct Prepared {
    temp: TempTables,
    templates: Vec<Template>,
}

impl Prepared {
    fn new() -> Self {
        Self {
            temp: TempTables::new(),
            templates: Vec::new(),
        }
    }
}

#[derive(Clone, Copy)]
struct Env<'a> {
    reader: &'a dyn KeyValueRead,
    names: &'a TempTableNames,
    columns_only: bool,
}

impl<'a> Env<'a> {
    fn with_reader<'b>(&self, reader: &'b dyn KeyValueRead) -> Env<'b>
    where
        'a: 'b,
    {
        Env {
            reader,
            names: self.names,
            columns_only: self.columns_only,
        }
    }
}

struct CteScope<'p> {
    names: &'p [(String, String)],
    parent: Option<&'p CteScope<'p>>,
}

fn find_cte<'p>(scope: Option<&'p CteScope<'p>>, name: &str) -> Option<&'p str> {
    let mut at = scope;
    while let Some(scope) = at {
        if let Some((_, temp)) = scope.names.iter().rev().find(|(cte, _)| cte == name) {
            return Some(temp);
        }
        at = scope.parent;
    }
    None
}

pub(crate) struct Correlation<'a> {
    reader: &'a dyn KeyValueRead,
    templates: &'a [Template],
    names: &'a TempTableNames,
    tables: RefCell<TableCache>,
    memo: RefCell<Memo>,
}

#[derive(Default)]
struct Memo {
    entries: HashMap<(usize, Vec<u8>), Arc<SubqueryRows>>,
    bytes: usize,
}

impl<'a> Correlation<'a> {
    fn new(env: &Env<'a>, templates: &'a [Template]) -> Self {
        Self {
            reader: env.reader,
            templates,
            names: env.names,
            tables: RefCell::new(TableCache::default()),
            memo: RefCell::new(Memo::default()),
        }
    }

    pub(crate) fn rows(&self, template: usize, row: &[Value]) -> SqlResult<Arc<SubqueryRows>> {
        let found = self
            .templates
            .get(template)
            .ok_or_else(|| internal("a correlated subquery is missing"))?;
        let outer: Vec<Value> = found
            .outer
            .iter()
            .map(|&index| row.get(index).cloned().unwrap_or(Value::Null))
            .collect();
        let key = (template, encode_row(&outer));
        if let Some(lookup) = &found.lookup
            && let Some(rows) = lookup.probe(row)
        {
            return Ok(rows);
        }
        if let Some(rows) = self
            .memo
            .try_borrow()
            .ok()
            .and_then(|memo| memo.entries.get(&key).cloned())
        {
            return Ok(rows);
        }
        let mut instance = found.query.clone();
        for_each_select_mut(&mut instance, &mut |select| {
            for expr in select_expressions_mut(select) {
                substitute(expr, row);
            }
        });
        let generated = generated_rows(&mut instance)?;
        let overlay = Overlay::new(self.reader, &found.prepared.temp);
        let instance_overlay;
        let reader: &dyn KeyValueRead = match &generated {
            Some(temp) => {
                instance_overlay = Overlay::new(&overlay, temp);
                &instance_overlay
            }
            None => &overlay,
        };
        let env = Env {
            reader,
            names: self.names,
            columns_only: false,
        };
        let result = match self.tables.try_borrow_mut() {
            Ok(mut tables) => execute_query(&env, &mut tables, &instance, &found.prepared)?,
            Err(_) => execute_query(&env, &mut TableCache::default(), &instance, &found.prepared)?,
        };
        let rows = Arc::new(SubqueryRows::from_result(result, &found.test)?);
        if let Ok(mut memo) = self.memo.try_borrow_mut() {
            let bytes = key.1.len() + rows.bytes();
            if memo.bytes.saturating_add(bytes) > MEMO_BYTES {
                memo.entries.clear();
                memo.bytes = 0;
            }
            memo.bytes += bytes;
            memo.entries.insert(key, Arc::clone(&rows));
        }
        Ok(rows)
    }
}

fn generated_rows(query: &mut Query) -> SqlResult<Option<TempTables>> {
    let mut temp: Option<TempTables> = None;
    let mut failure = None;
    for_each_select_mut(query, &mut |select| {
        let Some(from) = &select.from else {
            return;
        };
        for reference in
            std::iter::once(&from.first).chain(from.joins.iter().map(|join| &join.table))
        {
            let Some(generated) = &reference.generated else {
                continue;
            };
            if failure.is_some() {
                return;
            }
            let filled = (|| {
                let definition = crate::temp_tables::temp_definition(&reference.name.text)
                    .ok_or_else(|| {
                        internal("a generated table of a correlated subquery is missing")
                    })?;
                let evaluator = Evaluator {
                    scope: Scope::empty(),
                    row: &[],
                    correlation: None,
                };
                temp.get_or_insert_with(TempTables::new)
                    .fill_streamed(&definition, |store| {
                        crate::set_returning::generate_rows(generated, &evaluator, store)
                    })
            })();
            if let Err(error) = filled {
                failure = Some(error);
            }
        }
    });
    match failure {
        Some(error) => Err(error),
        None => Ok(temp),
    }
}

fn substitute(expr: &mut Expr, row: &[Value]) {
    if let ExprKind::OuterColumn { index, sql_type } = &expr.kind {
        expr.kind = ExprKind::Constant {
            value: row.get(*index).cloned().unwrap_or(Value::Null),
            sql_type: *sql_type,
        };
        return;
    }
    for child in children_mut(expr) {
        substitute(child, row);
    }
}

fn collect_outer(expr: &Expr, out: &mut Vec<usize>) {
    if let ExprKind::OuterColumn { index, .. } = &expr.kind {
        if !out.contains(index) {
            out.push(*index);
        }
        return;
    }
    for child in children(expr) {
        collect_outer(child, out);
    }
}

fn for_each_select_mut(query: &mut Query, visit: &mut dyn FnMut(&mut Select)) {
    let mut pending: Vec<&mut SetExpr> = vec![&mut query.body];
    while let Some(body) = pending.pop() {
        match body {
            SetExpr::Select(select) => visit(select),
            SetExpr::Operation(operation) => {
                pending.push(&mut operation.left);
                pending.push(&mut operation.right);
            }
            SetExpr::Query(inner) => pending.push(&mut inner.body),
        }
    }
}

fn for_each_select(query: &Query, visit: &mut dyn FnMut(&Select)) {
    let mut pending: Vec<&SetExpr> = vec![&query.body];
    while let Some(body) = pending.pop() {
        match body {
            SetExpr::Select(select) => visit(select),
            SetExpr::Operation(operation) => {
                pending.push(&operation.left);
                pending.push(&operation.right);
            }
            SetExpr::Query(inner) => pending.push(&inner.body),
        }
    }
}

fn select_expressions_mut(select: &mut Select) -> Vec<&mut Expr> {
    let mut exprs: Vec<&mut Expr> = Vec::new();
    for item in &mut select.items {
        if let SelectItem::Expr { expr, .. } = item {
            exprs.push(expr);
        }
    }
    exprs.extend(select.filter.iter_mut());
    exprs.extend(select.group_by.iter_mut());
    exprs.extend(select.having.iter_mut());
    exprs.extend(select.order_by.iter_mut().map(|item| &mut item.expr));
    exprs.extend(select.limit.iter_mut());
    exprs.extend(select.offset.iter_mut());
    if let Some(from) = &mut select.from {
        if let Some(generated) = &mut from.first.generated {
            exprs.extend(crate::set_returning::expressions_mut(generated));
        }
        for join in &mut from.joins {
            if let JoinCondition::On(expr) = &mut join.condition {
                exprs.push(expr);
            }
            if let Some(generated) = &mut join.table.generated {
                exprs.extend(crate::set_returning::expressions_mut(generated));
            }
        }
    }
    exprs
}

fn select_expressions(select: &Select) -> Vec<&Expr> {
    let mut exprs: Vec<&Expr> = Vec::new();
    for item in &select.items {
        if let SelectItem::Expr { expr, .. } = item {
            exprs.push(expr);
        }
    }
    exprs.extend(select.filter.iter());
    exprs.extend(select.group_by.iter());
    exprs.extend(select.having.iter());
    exprs.extend(select.order_by.iter().map(|item| &item.expr));
    exprs.extend(select.limit.iter());
    exprs.extend(select.offset.iter());
    if let Some(from) = &select.from {
        for table in std::iter::once(&from.first).chain(from.joins.iter().map(|join| &join.table)) {
            if let Some(generated) = &table.generated {
                exprs.extend(crate::set_returning::expressions(generated));
            }
        }
        for join in &from.joins {
            if let JoinCondition::On(expr) = &join.condition {
                exprs.push(expr);
            }
        }
    }
    exprs
}

pub(crate) fn mentions_subquery(expr: &Expr) -> bool {
    let any = |exprs: &[Expr]| exprs.iter().any(mentions_subquery);
    match &expr.kind {
        ExprKind::Subquery(_) => true,
        ExprKind::Literal(_)
        | ExprKind::Column { .. }
        | ExprKind::Slot { .. }
        | ExprKind::SessionValue { .. }
        | ExprKind::OuterColumn { .. }
        | ExprKind::Constant { .. } => false,
        ExprKind::Unary(_, operand)
        | ExprKind::IsNull { expr: operand, .. }
        | ExprKind::IsTruth { expr: operand, .. }
        | ExprKind::Cast { expr: operand, .. } => mentions_subquery(operand),
        ExprKind::Binary(_, left, right)
        | ExprKind::IsDistinctFrom { left, right, .. }
        | ExprKind::Like {
            expr: left,
            pattern: right,
            ..
        } => mentions_subquery(left) || mentions_subquery(right),
        ExprKind::Between {
            expr, low, high, ..
        } => mentions_subquery(expr) || mentions_subquery(low) || mentions_subquery(high),
        ExprKind::InList { expr, list, .. } => mentions_subquery(expr) || any(list),
        ExprKind::And(operands) | ExprKind::Or(operands) => any(operands),
        ExprKind::Case {
            operand,
            branches,
            otherwise,
        } => {
            operand.as_deref().is_some_and(mentions_subquery)
                || branches.iter().any(|(condition, result)| {
                    mentions_subquery(condition) || mentions_subquery(result)
                })
                || otherwise.as_deref().is_some_and(mentions_subquery)
        }
        ExprKind::Function { arguments, .. }
        | ExprKind::SqliteFunction { arguments, .. }
        | ExprKind::OracleFunction { arguments, .. }
        | ExprKind::SqlServerFunction { arguments, .. } => any(arguments),
        ExprKind::Aggregate { argument, .. } => argument.as_deref().is_some_and(mentions_subquery),
    }
}

pub(crate) fn mentions_correlated(expr: &Expr) -> bool {
    matches!(
        &expr.kind,
        ExprKind::Subquery(subquery) if matches!(subquery.state, SubqueryState::Correlated { .. })
    ) || children(expr).into_iter().any(mentions_correlated)
}

pub(crate) fn needs_preparation(select: &Select) -> bool {
    let derived = select.from.as_ref().is_some_and(|from| {
        std::iter::once(&from.first)
            .chain(from.joins.iter().map(|join| &join.table))
            .any(|table| table.derived.is_some() || table.generated.is_some())
    });
    derived
        || select_expressions(select)
            .into_iter()
            .any(mentions_subquery)
        || crate::set_returning::select_needs_expansion(select)
}

pub(crate) fn prepare_in_place(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    exprs: Vec<&mut Expr>,
    scope: Scope<'_>,
) -> SqlResult<()> {
    let _shared = crate::memory::share_statement_memory();
    let names = TempTableNames::default();
    let env = Env {
        reader,
        names: &names,
        columns_only: false,
    };
    let mut templates = Vec::new();
    let mut correlated = false;
    for expr in exprs {
        rewrite(
            &env,
            tables,
            expr,
            scope,
            None,
            None,
            &mut templates,
            &mut correlated,
        )?;
        if !templates.is_empty() {
            return Err(SqlError::not_supported(
                "a subquery that refers to the rows being changed is not supported yet",
            )
            .with_hint(
                "Compute the values with a SELECT first, or use a subquery that stands alone.",
            )
            .at(expr.position));
        }
    }
    Ok(())
}

pub(crate) fn run_select(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    select: &Select,
    columns_only: bool,
) -> SqlResult<QueryResult> {
    run_statement(
        reader,
        tables,
        &Query::of_select(select.clone(), 0),
        columns_only,
    )
}

pub(crate) fn run_statement(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    query: &Query,
    columns_only: bool,
) -> SqlResult<QueryResult> {
    let _shared = crate::memory::share_statement_memory();
    let names = TempTableNames::default();
    let env = Env {
        reader,
        names: &names,
        columns_only,
    };
    run_query(&env, tables, query, None)
}

fn run_query(
    env: &Env<'_>,
    tables: &mut TableCache,
    query: &Query,
    ctes: Option<&CteScope<'_>>,
) -> SqlResult<QueryResult> {
    let mut prepared = Prepared::new();
    let (query, _) = prepare_query(env, tables, query, None, ctes, &mut prepared)?;
    let overlay = Overlay::new(env.reader, &prepared.temp);
    execute_query(&env.with_reader(&overlay), tables, &query, &prepared)
}

fn execute_query(
    env: &Env<'_>,
    tables: &mut TableCache,
    query: &Query,
    prepared: &Prepared,
) -> SqlResult<QueryResult> {
    let correlation = Correlation::new(env, &prepared.templates);
    execute_query_with(env, tables, query, &correlation)
}

fn execute_query_with(
    env: &Env<'_>,
    tables: &mut TableCache,
    query: &Query,
    correlation: &Correlation<'_>,
) -> SqlResult<QueryResult> {
    let result = match &query.body {
        SetExpr::Select(select) => {
            return execute::select_prepared(
                env.reader,
                tables,
                select,
                env.columns_only,
                Some(correlation),
            );
        }
        SetExpr::Query(inner) => execute_query_with(env, tables, inner, correlation)?,
        SetExpr::Operation(_) => execute_operations(env, tables, &query.body, correlation)?.result,
    };
    set_operations::order_and_limit(
        result,
        &query.order_by,
        query.limit.as_ref(),
        query.offset.as_ref(),
    )
}

fn execute_operations(
    env: &Env<'_>,
    tables: &mut TableCache,
    body: &SetExpr,
    correlation: &Correlation<'_>,
) -> SqlResult<Branch> {
    let mut spine = Vec::new();
    let mut leftmost = body;
    while let SetExpr::Operation(operation) = leftmost {
        spine.push(operation);
        leftmost = &operation.left;
    }
    let mut result = execute_branch(env, tables, leftmost, correlation)?;
    for operation in spine.into_iter().rev() {
        let right = execute_branch(env, tables, &operation.right, correlation)?;
        result = Branch {
            result: set_operations::combine(operation, result, right)?,
            unknown: Vec::new(),
        };
    }
    Ok(result)
}

fn execute_branch(
    env: &Env<'_>,
    tables: &mut TableCache,
    body: &SetExpr,
    correlation: &Correlation<'_>,
) -> SqlResult<Branch> {
    match body {
        SetExpr::Select(select) => Ok(Branch {
            result: execute::select_prepared(
                env.reader,
                tables,
                select,
                env.columns_only,
                Some(correlation),
            )?,
            unknown: if select
                .items
                .iter()
                .any(|item| matches!(item, SelectItem::Wildcard { .. }))
            {
                Vec::new()
            } else {
                set_operations::unknown_columns(&select.items)
            },
        }),
        SetExpr::Query(inner) => Ok(Branch {
            result: execute_query_with(env, tables, inner, correlation)?,
            unknown: Vec::new(),
        }),
        SetExpr::Operation(_) => execute_operations(env, tables, body, correlation),
    }
}

fn prepare_query(
    env: &Env<'_>,
    tables: &mut TableCache,
    query: &Query,
    parent: Option<&Outer<'_>>,
    ctes: Option<&CteScope<'_>>,
    prepared: &mut Prepared,
) -> SqlResult<(Query, bool)> {
    let mut names: Vec<(String, String)> = Vec::with_capacity(query.with.len());
    for table in &query.with {
        let visible = CteScope {
            names: &names,
            parent: ctes,
        };
        let recursive = (query.recursive
            || matches!(
                dialect_of(&table.query),
                Some(Dialect::Sqlite | Dialect::Oracle)
            ))
            && references_table(&table.query, &table.name.text);
        let result = {
            let overlay = Overlay::new(env.reader, &prepared.temp);
            let env = env.with_reader(&overlay);
            if recursive {
                recursive_table(&env, tables, table, Some(&visible))?
            } else {
                run_query(&env, tables, &table.query, Some(&visible))?
            }
        };
        let columns = result.columns.unwrap_or_default();
        let temp = prepared.temp.add(
            env.names,
            Intermediate {
                columns: &columns,
                renamed: &table.column_names,
                label: &table.name.text,
                dialect: table.dialect,
                position: table.name.position,
            },
            result.rows,
        )?;
        names.push((table.name.text.clone(), temp));
    }
    let visible = CteScope {
        names: &names,
        parent: ctes,
    };
    let mut prepared_query = Query {
        with: Vec::new(),
        recursive: false,
        body: query.body.clone(),
        order_by: query.order_by.clone(),
        limit: query.limit.clone(),
        offset: query.offset.clone(),
        position: query.position,
    };
    let mut correlated = false;
    let mut failure = None;
    let mut bodies: Vec<&mut SetExpr> = vec![&mut prepared_query.body];
    while let Some(body) = bodies.pop() {
        match body {
            SetExpr::Select(select) => {
                match prepare_select(env, tables, select, parent, Some(&visible), prepared) {
                    Ok(found) => correlated |= found,
                    Err(error) => {
                        failure = Some(error);
                        break;
                    }
                }
            }
            SetExpr::Operation(operation) => {
                bodies.push(&mut operation.right);
                bodies.push(&mut operation.left);
            }
            SetExpr::Query(inner) => {
                match prepare_query(env, tables, inner, parent, Some(&visible), prepared) {
                    Ok((done, found)) => {
                        **inner = done;
                        correlated |= found;
                    }
                    Err(error) => {
                        failure = Some(error);
                        break;
                    }
                }
            }
        }
    }
    if let Some(error) = failure {
        return Err(error);
    }
    for expr in prepared_query
        .limit
        .iter_mut()
        .chain(prepared_query.offset.iter_mut())
    {
        let overlay = Overlay::new(env.reader, &prepared.temp);
        let inner_env = env.with_reader(&overlay);
        let mut templates = Vec::new();
        rewrite(
            &inner_env,
            tables,
            expr,
            Scope::empty(),
            None,
            Some(&visible),
            &mut templates,
            &mut correlated,
        )?;
    }
    Ok((prepared_query, correlated))
}

fn dialect_of(query: &Query) -> Option<Dialect> {
    match &query.body {
        SetExpr::Operation(operation) => Some(operation.dialect),
        _ => None,
    }
}

fn references_table(query: &Query, name: &str) -> bool {
    let mut found = false;
    for table in &query.with {
        found |= table.name.text != name && references_table(&table.query, name);
    }
    for_each_select(query, &mut |select| {
        if let Some(from) = &select.from {
            for table in
                std::iter::once(&from.first).chain(from.joins.iter().map(|join| &join.table))
            {
                found |= match &table.derived {
                    Some(derived) => references_table(&derived.query, name),
                    None => table.name.text == name,
                };
            }
        }
        for expr in select_expressions(select) {
            found |= expr_references_table(expr, name);
        }
    });
    found
}

fn expr_references_table(expr: &Expr, name: &str) -> bool {
    if let ExprKind::Subquery(subquery) = &expr.kind
        && references_table(&subquery.query, name)
    {
        return true;
    }
    children(expr)
        .into_iter()
        .any(|child| expr_references_table(child, name))
}

fn recursive_table(
    env: &Env<'_>,
    tables: &mut TableCache,
    table: &CommonTable,
    ctes: Option<&CteScope<'_>>,
) -> SqlResult<QueryResult> {
    let name = &table.name.text;
    let SetExpr::Operation(operation) = &table.query.body else {
        return Err(SqlError::new(
            SqlState::INVALID_RECURSION,
            format!(
                "recursive query \"{name}\" does not have the form non-recursive-term UNION [ALL] recursive-term"
            ),
        )
        .at(table.name.position));
    };
    if operation.operator != crate::ast::SetOperator::Union
        || !table.query.order_by.is_empty()
        || table.query.limit.is_some()
        || table.query.offset.is_some()
    {
        return Err(SqlError::new(
            SqlState::INVALID_RECURSION,
            format!(
                "recursive query \"{name}\" does not have the form non-recursive-term UNION [ALL] recursive-term"
            ),
        )
        .at(table.name.position));
    }
    let seed_query = Query {
        with: table.query.with.clone(),
        recursive: table.query.recursive,
        body: operation.left.clone(),
        order_by: Vec::new(),
        limit: None,
        offset: None,
        position: table.query.position,
    };
    if references_table(&seed_query, name) {
        return Err(SqlError::new(
            SqlState::INVALID_RECURSION,
            format!("recursive reference to query \"{name}\" must not appear within its non-recursive term"),
        )
        .at(table.name.position));
    }
    let step_query = Query {
        body: operation.right.clone(),
        ..seed_query.clone()
    };
    let seed = run_query(env, tables, &seed_query, ctes)?;
    let columns = seed.columns.clone().unwrap_or_default();
    let mut memory = WorkMemory::new();
    let mut seen: HashSet<Vec<u8>> = HashSet::new();
    let mut all_rows: Vec<Vec<Value>> = Vec::new();
    let mut working = Vec::new();
    for row in seed.rows {
        if operation.all || first_time(&mut seen, &row, &mut memory)? {
            memory.charge_row(&row)?;
            all_rows.push(row.clone());
            working.push(row);
        }
    }
    let most = if operation.dialect == Dialect::MySql {
        MYSQL_MOST_RECURSION_STEPS
    } else {
        MOST_RECURSION_STEPS
    };
    let mut steps = 0usize;
    while !working.is_empty() {
        steps += 1;
        if steps > most {
            return Err(SqlError::new(
                SqlState::STATEMENT_TOO_COMPLEX,
                format!("Recursive query aborted after {} iterations.", most + 1),
            )
            .with_hint(format!(
                "A recursive WITH query may run at most {most} steps; make sure its recursive term stops producing rows."
            )));
        }
        let mut step_temp = TempTables::new();
        let temp = step_temp.add(
            env.names,
            Intermediate {
                columns: &columns,
                renamed: &table.column_names,
                label: name,
                dialect: table.dialect,
                position: table.name.position,
            },
            std::mem::take(&mut working),
        )?;
        let names = [(name.clone(), temp)];
        let visible = CteScope {
            names: &names,
            parent: ctes,
        };
        let overlay = Overlay::new(env.reader, &step_temp);
        let step = run_query(
            &env.with_reader(&overlay),
            tables,
            &step_query,
            Some(&visible),
        )?;
        let width = columns.len();
        if step.columns.as_ref().map_or(0, Vec::len) != width {
            return Err(set_operations::column_count_mismatch(
                operation.operator,
                operation.dialect,
                operation.position,
            ));
        }
        for row in step.rows {
            let row = set_operations::conform(row, &columns)?;
            if operation.all || first_time(&mut seen, &row, &mut memory)? {
                memory.charge_row(&row)?;
                all_rows.push(row.clone());
                working.push(row);
            }
        }
    }
    Ok(QueryResult::rows(
        format!("SELECT {}", all_rows.len()),
        columns,
        all_rows,
    ))
}

fn first_time(
    seen: &mut HashSet<Vec<u8>>,
    row: &[Value],
    memory: &mut WorkMemory,
) -> SqlResult<bool> {
    let mut key = Vec::new();
    for value in row {
        equality_key(&mut key, value);
    }
    if seen.contains(&key) {
        return Ok(false);
    }
    memory.charge(key.len() + 32)?;
    seen.insert(key);
    Ok(true)
}

fn prepare_select(
    env: &Env<'_>,
    tables: &mut TableCache,
    select: &mut Select,
    parent: Option<&Outer<'_>>,
    ctes: Option<&CteScope<'_>>,
    prepared: &mut Prepared,
) -> SqlResult<bool> {
    let mut correlated_from = false;
    if crate::set_returning::select_needs_expansion(select) {
        crate::set_returning::check_placement(select)?;
        crate::set_returning::expand_select_list(select)?;
    }
    if let Some(from) = &mut select.from {
        for index in 0..=from.joins.len() {
            let checked = std::iter::once(&from.first)
                .chain(from.joins.iter().map(|join| &join.table))
                .nth(index)
                .is_some_and(may_name_siblings);
            let siblings = if checked {
                siblings_of(from, index, ctes)
            } else {
                Vec::new()
            };
            let Some(reference) = reference_mut(from, index) else {
                continue;
            };
            correlated_from |=
                prepare_reference(env, tables, reference, parent, ctes, prepared, &siblings)?;
        }
    }
    let overlay = Overlay::new(env.reader, &prepared.temp);
    let definitions: Vec<Arc<TableDefinition>> = match &select.from {
        Some(from) => std::iter::once(&from.first)
            .chain(from.joins.iter().map(|join| &join.table))
            .map(|reference| tables.require(&overlay, &reference.name))
            .collect::<SqlResult<_>>()?,
        None => Vec::new(),
    };
    let from = select.from.clone();
    let joined = match &from {
        Some(from) if !from.joins.is_empty() => Some(join::resolve(from, &definitions, None)?),
        _ => None,
    };
    let scope = match (&from, &joined, definitions.first()) {
        (_, Some(joined), _) => Scope::Many(&joined.sources),
        (Some(from), None, Some(table)) => Scope::One(Source {
            table,
            label: from.first.alias.as_deref().unwrap_or(&table.name),
            offset: 0,
        }),
        _ => Scope::empty(),
    };
    let mut correlated = false;
    let mut templates = std::mem::take(&mut prepared.templates);
    let outcome = (|| {
        let overlay = Overlay::new(env.reader, &prepared.temp);
        let inner_env = env.with_reader(&overlay);
        let mut targets: Vec<&mut Expr> = Vec::new();
        for item in &mut select.items {
            if let SelectItem::Expr { expr, .. } = item {
                targets.push(expr);
            }
        }
        targets.extend(select.filter.iter_mut());
        targets.extend(select.having.iter_mut());
        if let Some(from) = &mut select.from {
            for join in &mut from.joins {
                if let JoinCondition::On(expr) = &mut join.condition {
                    targets.push(expr);
                }
            }
        }
        for expr in targets {
            rewrite(
                &inner_env,
                tables,
                expr,
                scope,
                parent,
                ctes,
                &mut templates,
                &mut correlated,
            )?;
        }
        let mut others: Vec<&mut Expr> = select.group_by.iter_mut().collect();
        others.extend(select.order_by.iter_mut().map(|item| &mut item.expr));
        others.extend(select.limit.iter_mut());
        others.extend(select.offset.iter_mut());
        for expr in others {
            rewrite(
                &inner_env,
                tables,
                expr,
                scope,
                None,
                ctes,
                &mut templates,
                &mut correlated,
            )?;
        }
        Ok(())
    })();
    prepared.templates = templates;
    outcome.map(|()| correlated || correlated_from)
}

fn prepare_reference(
    env: &Env<'_>,
    tables: &mut TableCache,
    reference: &mut TableReference,
    parent: Option<&Outer<'_>>,
    ctes: Option<&CteScope<'_>>,
    prepared: &mut Prepared,
    siblings: &[Sibling],
) -> SqlResult<bool> {
    if let Some(generated) = reference.generated.take() {
        return prepare_generated(
            env,
            tables,
            reference,
            *generated,
            Surroundings {
                parent,
                ctes,
                siblings,
            },
            prepared,
        );
    }
    if let Some(derived) = reference.derived.take() {
        let result = {
            let overlay = Overlay::new(env.reader, &prepared.temp);
            run_query(&env.with_reader(&overlay), tables, &derived.query, ctes)?
        };
        let columns = result.columns.unwrap_or_default();
        let label = reference.alias.as_deref().unwrap_or("unnamed_subquery");
        let temp = prepared.temp.add(
            env.names,
            Intermediate {
                columns: &columns,
                renamed: &derived.column_names,
                label,
                dialect: derived.dialect,
                position: reference.name.position,
            },
            result.rows,
        )?;
        reference.name.text = temp;
        return Ok(false);
    }
    if let Some(temp) = find_cte(ctes, &reference.name.text) {
        if reference.alias.is_none() {
            reference.alias = Some(reference.name.text.clone());
        }
        reference.name.text = temp.to_owned();
    }
    Ok(false)
}

struct Surroundings<'s> {
    parent: Option<&'s Outer<'s>>,
    ctes: Option<&'s CteScope<'s>>,
    siblings: &'s [Sibling],
}

struct Sibling {
    label: String,
    table: Option<String>,
}

fn reference_mut(from: &mut FromClause, index: usize) -> Option<&mut TableReference> {
    match index.checked_sub(1) {
        None => Some(&mut from.first),
        Some(join) => from.joins.get_mut(join).map(|join| &mut join.table),
    }
}

fn may_name_siblings(reference: &TableReference) -> bool {
    matches!(
        reference.generated.as_deref(),
        Some(GeneratedTable::Function(_))
    )
}

fn siblings_of(from: &FromClause, index: usize, ctes: Option<&CteScope<'_>>) -> Vec<Sibling> {
    std::iter::once(&from.first)
        .chain(from.joins.iter().map(|join| &join.table))
        .enumerate()
        .filter(|(other, _)| *other != index)
        .map(|(_, reference)| {
            let name = &reference.name.text;
            let readable = reference.derived.is_none()
                && (reference.generated.is_none() || crate::temp_tables::is_temp_table(name));
            Sibling {
                label: reference.alias.clone().unwrap_or_else(|| name.clone()),
                table: readable.then(|| find_cte(ctes, name).unwrap_or(name).to_owned()),
            }
        })
        .collect()
}

fn column_references<'e>(expr: &'e Expr, found: &mut Vec<(Option<&'e str>, &'e str, usize)>) {
    if let ExprKind::Column { qualifier, name } = &expr.kind {
        found.push((qualifier.as_deref(), name, expr.position));
        return;
    }
    for child in children(expr) {
        column_references(child, found);
    }
}

fn subquery_column_references(expr: &Expr, found: &mut Vec<(Option<String>, String, usize)>) {
    match &expr.kind {
        ExprKind::Column { qualifier, name } => {
            found.push((qualifier.clone(), name.clone(), expr.position));
            return;
        }
        ExprKind::Subquery(subquery) => for_each_select(&subquery.query, &mut |select| {
            for inner in select_expressions(select) {
                subquery_column_references(inner, found);
            }
        }),
        _ => {}
    }
    for child in children(expr) {
        subquery_column_references(child, found);
    }
}

fn names_sibling(
    env: &Env<'_>,
    tables: &mut TableCache,
    prepared: &Prepared,
    siblings: &[Sibling],
    (qualifier, name, position): (Option<&str>, &str, usize),
) -> bool {
    match qualifier {
        Some(qualifier) => siblings.iter().any(|sibling| sibling.label == qualifier),
        None => {
            let overlay = Overlay::new(env.reader, &prepared.temp);
            siblings.iter().any(|sibling| {
                sibling.table.as_ref().is_some_and(|table| {
                    tables
                        .require(
                            &overlay,
                            &crate::ast::Name {
                                text: table.clone(),
                                position,
                            },
                        )
                        .is_ok_and(|definition| definition.column_index(name).is_some())
                })
            })
        }
    }
}

fn lateral_not_supported(position: usize) -> SqlError {
    SqlError::not_supported(
        "a function in FROM that names another FROM item (LATERAL) is not supported yet",
    )
    .at(position)
}

fn lateral_reference(
    env: &Env<'_>,
    tables: &mut TableCache,
    prepared: &Prepared,
    generated: &GeneratedTable,
    siblings: &[Sibling],
) -> SqlResult<()> {
    if siblings.is_empty() {
        return Ok(());
    }
    let mut references = Vec::new();
    for expr in crate::set_returning::expressions(generated) {
        column_references(expr, &mut references);
    }
    for reference in references {
        if names_sibling(env, tables, prepared, siblings, reference) {
            return Err(lateral_not_supported(reference.2));
        }
    }
    Ok(())
}

fn lateral_through_subquery(
    env: &Env<'_>,
    tables: &mut TableCache,
    prepared: &Prepared,
    generated: &GeneratedTable,
    siblings: &[Sibling],
    error: SqlError,
) -> SqlError {
    let Some(missing_at) = error.position() else {
        return error;
    };
    if siblings.is_empty() || !is_missing(&error) {
        return error;
    }
    if !matches!(generated, GeneratedTable::Function(_)) {
        return error;
    }
    let mut references = Vec::new();
    for expr in crate::set_returning::expressions(generated) {
        subquery_column_references(expr, &mut references);
    }
    let lateral = references.iter().any(|(qualifier, name, position)| {
        *position == missing_at
            && names_sibling(
                env,
                tables,
                prepared,
                siblings,
                (qualifier.as_deref(), name, *position),
            )
    });
    if lateral {
        lateral_not_supported(missing_at)
    } else {
        error
    }
}

enum Filling {
    Rows(crate::set_returning::GeneratedRows),
    Streamed {
        columns: Vec<crate::session::OutputColumn>,
        function: crate::set_returning::BoundFunction,
    },
}

fn prepare_generated(
    env: &Env<'_>,
    tables: &mut TableCache,
    reference: &mut TableReference,
    mut generated: GeneratedTable,
    surroundings: Surroundings<'_>,
    prepared: &mut Prepared,
) -> SqlResult<bool> {
    let Surroundings {
        parent,
        ctes,
        siblings,
    } = surroundings;
    let alias_given = reference.alias.is_some();
    let label = reference
        .alias
        .clone()
        .unwrap_or_else(|| reference.name.text.clone());
    let position = reference.name.position;
    crate::set_returning::refuse_nested_in_from(&generated)?;
    let mut deferred = false;
    let built = match &mut generated {
        GeneratedTable::Expansion(expansion) => {
            let inner = {
                let overlay = Overlay::new(env.reader, &prepared.temp);
                run_query(&env.with_reader(&overlay), tables, &expansion.query, ctes)
            }
            .map_err(|error| {
                if parent.is_some() && is_missing(&error) {
                    SqlError::not_supported(
                        "a set-returning function in the select list of a subquery that names the outer query is not supported yet",
                    )
                    .at(expansion.position)
                } else {
                    error
                }
            })?;
            Filling::Rows(crate::set_returning::expanded(expansion, inner)?)
        }
        GeneratedTable::Catalog(relation) => Filling::Rows(crate::pg_catalog::generated_rows(
            env.reader,
            *relation,
            env.columns_only,
        )?),
        GeneratedTable::Variable(name) => Filling::Rows(crate::table_variables::generated_rows(
            name,
            env.columns_only,
        )?),
        GeneratedTable::Values(_) | GeneratedTable::Function(_) => {
            lateral_reference(env, tables, prepared, &generated, siblings)?;
            let mut correlated = false;
            {
                let overlay = Overlay::new(env.reader, &prepared.temp);
                let inner_env = env.with_reader(&overlay);
                let mut templates = Vec::new();
                let rewritten = crate::set_returning::expressions_mut(&mut generated)
                    .into_iter()
                    .try_for_each(|expr| {
                        rewrite(
                            &inner_env,
                            tables,
                            expr,
                            Scope::empty(),
                            parent,
                            ctes,
                            &mut templates,
                            &mut correlated,
                        )
                        .map(drop)
                    });
                if let Err(error) = rewritten {
                    return Err(lateral_through_subquery(
                        env, tables, prepared, &generated, siblings, error,
                    ));
                }
                if !templates.is_empty() {
                    return Err(SqlError::not_supported(
                        "a subquery that names the outer query inside VALUES or a function in FROM is not supported yet",
                    )
                    .at(position));
                }
            }
            deferred = correlated;
            let evaluator = Evaluator {
                scope: Scope::empty(),
                row: &[],
                correlation: None,
            };
            match &generated {
                GeneratedTable::Function(function) if correlated || env.columns_only => {
                    Filling::Rows(crate::set_returning::GeneratedRows::empty(
                        crate::set_returning::table_function_columns(
                            function,
                            &label,
                            alias_given,
                            Scope::empty(),
                        )?,
                    ))
                }
                GeneratedTable::Values(list) if correlated || env.columns_only => {
                    Filling::Rows(crate::set_returning::GeneratedRows::empty(
                        crate::set_returning::values_columns(list, Scope::empty())?,
                    ))
                }
                GeneratedTable::Function(function) => {
                    let bound = crate::set_returning::bind_function(function, &evaluator)?;
                    Filling::Streamed {
                        columns: bound.columns(function, &label, alias_given),
                        function: bound,
                    }
                }
                GeneratedTable::Values(list) => {
                    Filling::Rows(crate::set_returning::values_rows(list, &evaluator)?)
                }
                GeneratedTable::Expansion(_)
                | GeneratedTable::Catalog(_)
                | GeneratedTable::Variable(_) => {
                    Filling::Rows(crate::set_returning::GeneratedRows::empty(Vec::new()))
                }
            }
        }
    };
    let renamed = match &generated {
        GeneratedTable::Function(function) => function.column_names.clone(),
        GeneratedTable::Values(list) => list.column_names.clone(),
        GeneratedTable::Expansion(_) | GeneratedTable::Catalog(_) | GeneratedTable::Variable(_) => {
            Vec::new()
        }
    };
    let (columns, rows, streamed, charge) = match built {
        Filling::Rows(built) => (built.columns, built.rows, None, Some(built.charge)),
        Filling::Streamed { columns, function } => (columns, Vec::new(), Some(function), None),
    };
    let shape = Intermediate {
        columns: &columns,
        renamed: &renamed,
        label: &label,
        dialect: Dialect::PostgreSql,
        position,
    };
    let temp = match streamed {
        Some(function) => prepared
            .temp
            .add_streamed(env.names, shape, |store| function.generate(store))?,
        None => prepared.temp.add(env.names, shape, rows)?,
    };
    drop(charge);
    reference.name.text = temp;
    reference.alias = Some(label);
    if deferred {
        reference.generated = Some(Box::new(generated));
    }
    Ok(deferred)
}

fn is_missing(error: &SqlError) -> bool {
    error.state() == SqlState::UNDEFINED_COLUMN || error.state() == SqlState::UNDEFINED_TABLE
}

#[allow(clippy::too_many_arguments)]
fn rewrite(
    env: &Env<'_>,
    tables: &mut TableCache,
    expr: &mut Expr,
    scope: Scope<'_>,
    parent: Option<&Outer<'_>>,
    ctes: Option<&CteScope<'_>>,
    templates: &mut Vec<Template>,
    correlated: &mut bool,
) -> SqlResult<()> {
    match &mut expr.kind {
        ExprKind::Column { qualifier, name } => {
            let Some(outer) = parent else {
                return Ok(());
            };
            let Err(error) = scope.column(qualifier.as_deref(), name, expr.position) else {
                return Ok(());
            };
            if !is_missing(&error) {
                return Ok(());
            }
            if let Ok(index) = outer
                .scope
                .column(qualifier.as_deref(), name, expr.position)
            {
                expr.kind = ExprKind::OuterColumn {
                    index,
                    sql_type: outer.scope.column_type(index),
                };
                *correlated = true;
                return Ok(());
            }
            let mut farther = outer.next;
            while let Some(level) = farther {
                if level
                    .scope
                    .column(qualifier.as_deref(), name, expr.position)
                    .is_ok()
                {
                    return Err(SqlError::not_supported(
                        "a subquery that refers to a query two or more levels out is not supported yet",
                    )
                    .at(expr.position));
                }
                farther = level.next;
            }
            Ok(())
        }
        ExprKind::Aggregate {
            argument: Some(argument),
            ..
        } => {
            rewrite(
                env, tables, argument, scope, parent, ctes, templates, correlated,
            )?;
            if parent.is_some() && mentions_outer(argument) && !mentions_inner_column(argument) {
                return Err(SqlError::not_supported(
                    "an aggregate over only the outer query's columns inside a subquery is not supported yet",
                )
                .with_hint("Compute the aggregate in the outer query, or name a column of the subquery's own tables in it.")
                .at(expr.position));
            }
            Ok(())
        }
        ExprKind::Subquery(subquery) => {
            if let Some(operand) = subquery.test.operand_mut() {
                rewrite(
                    env, tables, operand, scope, parent, ctes, templates, correlated,
                )?;
            }
            let outer = Outer {
                scope,
                next: parent,
            };
            prepare_subquery(env, tables, subquery, &outer, ctes, templates)
        }
        _ => {
            for child in children_mut(expr) {
                rewrite(
                    env, tables, child, scope, parent, ctes, templates, correlated,
                )?;
            }
            Ok(())
        }
    }
}

fn row_cap(subquery: &Subquery) -> Option<i64> {
    match subquery.test {
        SubqueryTest::Exists => Some(1),
        SubqueryTest::Scalar if subquery.dialect == Dialect::Sqlite => Some(1),
        SubqueryTest::Scalar => Some(2),
        _ => None,
    }
}

fn with_cap(mut query: Query, cap: Option<i64>) -> Query {
    let Some(cap) = cap else {
        return query;
    };
    let literal = Expr {
        kind: ExprKind::Literal(Literal::Integer(cap)),
        position: query.position,
    };
    match &mut query.body {
        SetExpr::Select(select) if query.with.is_empty() && query.limit.is_none() => {
            if select.limit.is_none() {
                select.limit = Some(literal);
            }
        }
        _ => {
            if query.limit.is_none() {
                query.limit = Some(literal);
            }
        }
    }
    query
}

fn subquery_columns_error(dialect: Dialect, count: usize, position: usize) -> SqlError {
    let error = if dialect == Dialect::Sqlite {
        SqlError::new(
            SqlState::SYNTAX_ERROR,
            format!("sub-select returns {count} columns - expected 1"),
        )
    } else {
        SqlError::new(
            SqlState::SYNTAX_ERROR,
            "subquery must return only one column",
        )
    };
    error.at(position)
}

fn prepare_subquery(
    env: &Env<'_>,
    tables: &mut TableCache,
    subquery: &mut Subquery,
    outer: &Outer<'_>,
    ctes: Option<&CteScope<'_>>,
    templates: &mut Vec<Template>,
) -> SqlResult<()> {
    let query = with_cap(subquery.query.clone(), row_cap(subquery));
    let mut inner = Prepared::new();
    let (query, correlated) = prepare_query(env, tables, &query, Some(outer), ctes, &mut inner)?;
    let overlay = Overlay::new(env.reader, &inner.temp);
    let inner_env = Env {
        reader: &overlay,
        names: env.names,
        columns_only: env.columns_only || correlated,
    };
    let result = execute_query(&inner_env, tables, &query, &inner)?;
    let count = result.columns.as_ref().map_or(0, Vec::len);
    if count != 1 && !matches!(subquery.test, SubqueryTest::Exists) {
        return Err(subquery_columns_error(
            subquery.dialect,
            count,
            subquery.query.position,
        ));
    }
    let rows = SubqueryRows::from_result(result, &subquery.test)?;
    if correlated {
        let mut outer = Vec::new();
        for_each_select(&query, &mut |select| {
            for expr in select_expressions(select) {
                collect_outer(expr, &mut outer);
            }
        });
        let sql_type = rows.sql_type;
        let lookup = if matches!(subquery.test, SubqueryTest::Exists) && !env.columns_only {
            let keys_env = Env {
                reader: &overlay,
                names: env.names,
                columns_only: false,
            };
            build_lookup(&keys_env, tables, &query, &inner)?
        } else {
            None
        };
        templates.push(Template {
            query,
            prepared: inner,
            outer,
            test: subquery.test.clone(),
            lookup,
        });
        subquery.state = SubqueryState::Correlated {
            template: templates.len() - 1,
            sql_type,
        };
    } else {
        subquery.state = SubqueryState::Rows(Arc::new(rows));
    }
    Ok(())
}

pub(crate) fn output_column(expr: &Expr) -> Option<OutputColumn> {
    let ExprKind::Subquery(subquery) = &expr.kind else {
        return None;
    };
    Some(OutputColumn {
        name: match subquery.test {
            SubqueryTest::Scalar => first_column_name(&subquery.query)?,
            SubqueryTest::Exists => "exists".to_owned(),
            SubqueryTest::Array => "array".to_owned(),
            _ => return None,
        },
        sql_type: result_type(subquery),
    })
}
