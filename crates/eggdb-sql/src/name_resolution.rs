use crate::OutputColumn;
use crate::ast::{
    AlterAction, AlterTable, ConstraintKind, CreateTable, Delete, DerivedTable, Expr, ExprKind,
    FromClause, GeneratedTable, Insert, InsertSource, InsertValue, JoinCondition, MergeStatement,
    Name, OnDuplicate, Query, Select, SelectItem, SetExpr, Statement, TableReference, Update,
};
use crate::dialect::Dialect;
use crate::encoding::ViewDefinition;
use crate::error::{SqlError, SqlResult, SqlState};
use crate::eval::children_mut;
use crate::execute::{Relation, TableCache};
use crate::lexer::tokenize_in;
use crate::parser::{Nesting, Parameters, Parser};
use crate::schemas::{self, DEFAULT_SCHEMA, base_name, display, is_qualified};
use crate::settings::Settings;
use crate::store::KeyValueRead;
use std::collections::HashSet;
use std::sync::Arc;

const MAX_QUERY_NESTING: usize = 32;
const MAX_NESTING: usize = 64;
const MAX_QUERIES: usize = 1024;
const MAX_VIEW_TEXT_PER_STATEMENT: usize = 64 << 20;

#[derive(Debug, Default)]
pub(crate) struct Resolution {
    pub(crate) references: Vec<String>,
    pub(crate) expanded: Vec<String>,
    recorded: Option<HashSet<String>>,
    seen_expanded: Option<HashSet<String>>,
}

pub(crate) struct Resolver<'a> {
    reader: &'a dyn KeyValueRead,
    tables: &'a mut TableCache,
    settings: Option<&'a Settings>,
    walk_expressions: bool,
    selects: usize,
    view_bytes: usize,
    depth: usize,
    levels: usize,
    recorded_at: Option<usize>,
    stack: Vec<String>,
    base_path: Arc<[String]>,
    view_paths: Vec<Arc<[String]>>,
    resolution: Resolution,
}

fn nested_too_deep(position: usize) -> SqlError {
    SqlError::new(SqlState::STATEMENT_TOO_COMPLEX, "stack depth limit exceeded")
        .with_hint(format!(
            "The statement and the views it uses nest queries more than {MAX_QUERY_NESTING} levels deep."
        ))
        .at(position)
}

fn too_many_queries(position: usize) -> SqlError {
    SqlError::new(
        SqlState::STATEMENT_TOO_COMPLEX,
        format!("a statement can hold at most {MAX_QUERIES} SELECTs, counting those of its views"),
    )
    .at(position)
}

pub(crate) fn check_view_text(text: &str, dialect: Dialect, settings: &Settings) -> SqlResult<()> {
    let tokens = tokenize_in(text, dialect)?;
    let mut parser = Parser::new(text, &tokens)
        .with_context(Parameters::Absent, settings)
        .in_dialect(dialect);
    parser.view_query().map(|_| ())
}

pub(crate) fn parse_view(
    view: &ViewDefinition,
    settings: Option<&Settings>,
    start: Nesting,
) -> SqlResult<(Query, usize)> {
    let damaged = || crate::encoding::corrupt("view definition");
    let tokens = tokenize_in(&view.query, view.dialect).map_err(|_| damaged())?;
    let defaults;
    let settings = match settings {
        Some(settings) => settings,
        None => {
            defaults = Settings::default();
            &defaults
        }
    };
    let mut parser = Parser::new(&view.query, &tokens)
        .with_context(Parameters::Absent, settings)
        .in_dialect(view.dialect)
        .nested_at(start);
    match parser.view_query() {
        Ok(found) => Ok(found),
        Err(error) if error.state() == SqlState::STATEMENT_TOO_COMPLEX => Err(error),
        Err(_) => Err(damaged()),
    }
}

impl<'a> Resolver<'a> {
    pub(crate) fn new(
        reader: &'a dyn KeyValueRead,
        tables: &'a mut TableCache,
        settings: Option<&'a Settings>,
        walk_expressions: bool,
    ) -> SqlResult<Self> {
        let path = schemas::shared_relation_path(reader)?;
        Ok(Self {
            reader,
            tables,
            settings,
            walk_expressions,
            selects: 0,
            view_bytes: 0,
            depth: 0,
            levels: 0,
            recorded_at: None,
            stack: Vec::new(),
            base_path: path,
            view_paths: Vec::new(),
            resolution: Resolution::default(),
        })
    }

    fn with_path(mut self, path: Vec<String>) -> SqlResult<Self> {
        let mut existing = Vec::with_capacity(path.len());
        for schema in path {
            if schemas::schema_exists(self.reader, &schema)? {
                existing.push(schema);
            }
        }
        self.base_path = existing.into();
        Ok(self)
    }

    fn resolve_name(&self, name: &str) -> SqlResult<String> {
        Ok(self.resolved_name(name)?.unwrap_or_else(|| name.to_owned()))
    }

    fn resolved_name(&self, name: &str) -> SqlResult<Option<String>> {
        if is_qualified(name) {
            let plain = schemas::normalized(name);
            return Ok((plain.len() != name.len()).then(|| plain.to_owned()));
        }
        if crate::temp_tables::is_temp_table(name) {
            return Ok(None);
        }
        let path: &[String] = self.view_paths.last().unwrap_or(&self.base_path);
        if schemas::is_default_path(path) {
            return Ok(None);
        }
        for schema in path {
            let candidate = schemas::qualified(schema, name);
            if self
                .reader
                .get(&crate::encoding::table_key(&candidate))?
                .is_some()
                || self
                    .reader
                    .get(&crate::encoding::sequence_key(&candidate))?
                    .is_some()
            {
                return Ok((candidate != name).then_some(candidate));
            }
        }
        if path.iter().any(|schema| schema == DEFAULT_SCHEMA) {
            Ok(None)
        } else {
            Ok(Some(schemas::unreachable_name(name)))
        }
    }

    pub(crate) fn names(&mut self, names: &mut [Name]) -> SqlResult<()> {
        for name in names {
            name.text = self.resolve_name(&name.text)?;
        }
        Ok(())
    }

    pub(crate) fn name(&mut self, name: &mut Name) -> SqlResult<()> {
        name.text = self.resolve_name(&name.text)?;
        Ok(())
    }

    pub(crate) fn target(&mut self, reference: &mut TableReference) -> SqlResult<()> {
        if let Some(relation) =
            crate::pg_catalog::catalog_reference(self.reader, &reference.name.text)?
        {
            return Err(crate::pg_catalog::read_only(relation.name()).at(reference.name.position));
        }
        let resolved = self.resolve_name(&reference.name.text)?;
        if is_qualified(&reference.name.text) && reference.alias.is_none() {
            reference.alias = Some(base_name(&resolved).to_owned());
        }
        reference.name.text = resolved;
        Ok(())
    }

    pub(crate) fn query(&mut self, query: &mut Query) -> SqlResult<()> {
        let mut ctes = Vec::new();
        self.walk_query(query, &mut ctes)
    }

    pub(crate) fn select(&mut self, select: &mut Select) -> SqlResult<()> {
        let mut ctes = Vec::new();
        self.walk_select(select, &mut ctes)
    }

    pub(crate) fn expressions<'e>(
        &mut self,
        exprs: impl IntoIterator<Item = &'e mut Expr>,
    ) -> SqlResult<()> {
        if !self.walk_expressions {
            return Ok(());
        }
        let mut ctes = Vec::new();
        for expr in exprs {
            self.walk_expr(expr, &mut ctes)?;
        }
        Ok(())
    }

    fn enter(&mut self, position: usize) -> SqlResult<()> {
        self.depth += 1;
        if self.depth > MAX_NESTING {
            return Err(nested_too_deep(position));
        }
        Ok(())
    }

    fn nested_query(
        &mut self,
        query: &mut Query,
        ctes: &mut Vec<String>,
        position: usize,
    ) -> SqlResult<()> {
        self.depth += 1;
        self.levels += 1;
        let walked = if self.depth > MAX_NESTING || self.levels > MAX_QUERY_NESTING {
            Err(nested_too_deep(position))
        } else {
            self.walk_query(query, ctes)
        };
        self.depth -= 1;
        self.levels -= 1;
        walked
    }

    fn walk_query(&mut self, query: &mut Query, ctes: &mut Vec<String>) -> SqlResult<()> {
        let base = ctes.len();
        for table in &mut query.with {
            ctes.push(table.name.text.clone());
            self.nested_query(&mut table.query, ctes, table.name.position)?;
        }
        let mut bodies: Vec<&mut SetExpr> = vec![&mut query.body];
        while let Some(body) = bodies.pop() {
            match body {
                SetExpr::Select(select) => self.walk_select(select, ctes)?,
                SetExpr::Operation(operation) => {
                    bodies.push(&mut operation.right);
                    bodies.push(&mut operation.left);
                }
                SetExpr::Query(inner) => {
                    let position = inner.position;
                    self.nested_query(inner, ctes, position)?;
                }
            }
        }
        if self.walk_expressions {
            for item in &mut query.order_by {
                self.walk_expr(&mut item.expr, ctes)?;
            }
            for expr in query.limit.iter_mut().chain(query.offset.iter_mut()) {
                self.walk_expr(expr, ctes)?;
            }
        }
        ctes.truncate(base);
        Ok(())
    }

    fn walk_select(&mut self, select: &mut Select, ctes: &mut Vec<String>) -> SqlResult<()> {
        self.selects += 1;
        if self.selects > MAX_QUERIES {
            return Err(too_many_queries(0));
        }
        if let Some(from) = &mut select.from {
            self.walk_reference(&mut from.first, ctes)?;
            for join in &mut from.joins {
                self.walk_reference(&mut join.table, ctes)?;
            }
        }
        if !self.walk_expressions {
            return Ok(());
        }
        for item in &mut select.items {
            if let SelectItem::Expr { expr, .. } = item {
                self.walk_expr(expr, ctes)?;
            }
        }
        let mut others: Vec<&mut Expr> = select.filter.iter_mut().collect();
        others.extend(select.group_by.iter_mut());
        others.extend(select.having.iter_mut());
        others.extend(select.order_by.iter_mut().map(|item| &mut item.expr));
        others.extend(select.limit.iter_mut());
        others.extend(select.offset.iter_mut());
        if let Some(from) = &mut select.from {
            for join in &mut from.joins {
                if let JoinCondition::On(expr) = &mut join.condition {
                    others.push(expr);
                }
            }
        }
        for expr in others {
            self.walk_expr(expr, ctes)?;
        }
        Ok(())
    }

    fn walk_expr(&mut self, expr: &mut Expr, ctes: &mut Vec<String>) -> SqlResult<()> {
        self.enter(expr.position)?;
        let walked = self.walk_expr_inner(expr, ctes);
        self.depth -= 1;
        walked
    }

    fn walk_expr_inner(&mut self, expr: &mut Expr, ctes: &mut Vec<String>) -> SqlResult<()> {
        match &mut expr.kind {
            ExprKind::Subquery(subquery) => {
                if let Some(operand) = subquery.test.operand_mut() {
                    self.walk_expr(operand, ctes)?;
                }
                let position = subquery.query.position;
                self.nested_query(&mut subquery.query, ctes, position)
            }
            ExprKind::SessionValue { name, value } if *name == "current_schema" => {
                *value = schemas::current_schema(self.reader)?.unwrap_or_default();
                Ok(())
            }
            ExprKind::SessionValue { name, value } if *name == "current_schemas" => {
                *value = schemas::current_schemas(self.reader, value == "true")?;
                Ok(())
            }
            _ => {
                for child in children_mut(expr) {
                    self.walk_expr(child, ctes)?;
                }
                Ok(())
            }
        }
    }

    fn record(&mut self, name: &str) {
        if self.recorded_at == Some(self.stack.len())
            && self
                .resolution
                .recorded
                .get_or_insert_with(HashSet::new)
                .insert(name.to_owned())
        {
            self.resolution.references.push(name.to_owned());
        }
    }

    fn walk_reference(
        &mut self,
        reference: &mut TableReference,
        ctes: &mut Vec<String>,
    ) -> SqlResult<()> {
        if let Some(derived) = &mut reference.derived {
            return self.nested_query(&mut derived.query, ctes, reference.name.position);
        }
        if let Some(generated) = &mut reference.generated {
            if self.walk_expressions {
                for expr in crate::set_returning::expressions_mut(generated) {
                    self.walk_expr(expr, ctes)?;
                }
            }
            return Ok(());
        }
        if ctes.contains(&reference.name.text) {
            return Ok(());
        }
        if let Some(relation) =
            crate::pg_catalog::catalog_reference(self.reader, &reference.name.text)?
        {
            if reference.alias.is_none() {
                reference.alias = Some(relation.name().to_owned());
            }
            reference.generated = Some(Box::new(GeneratedTable::Catalog(relation)));
            return Ok(());
        }
        let written_qualified = is_qualified(&reference.name.text);
        if let Some(resolved) = self.resolved_name(&reference.name.text)? {
            reference.name.text = resolved;
        }
        if written_qualified && reference.alias.is_none() {
            reference.alias = Some(base_name(&reference.name.text).to_owned());
        }
        let relation = self.tables.relation(self.reader, &reference.name.text)?;
        if relation.is_some() && self.recorded_at.is_some() {
            let name = reference.name.text.clone();
            self.record(&name);
        }
        match relation {
            Some(Relation::View(view)) => self.expand(reference, view),
            _ => Ok(()),
        }
    }

    fn expand(
        &mut self,
        reference: &mut TableReference,
        view: Arc<ViewDefinition>,
    ) -> SqlResult<()> {
        let position = reference.name.position;
        if self.stack.contains(&view.name) {
            return Err(SqlError::new(
                SqlState::INVALID_OBJECT_DEFINITION,
                format!(
                    "infinite recursion detected in rules for relation \"{}\"",
                    display(&view.name)
                ),
            )
            .at(position));
        }
        let query_depth = self.levels + 1;
        if query_depth > MAX_QUERY_NESTING {
            return Err(nested_too_deep(position));
        }
        self.view_bytes = self.view_bytes.saturating_add(view.query.len());
        if self.view_bytes > MAX_VIEW_TEXT_PER_STATEMENT {
            return Err(SqlError::new(
                SqlState::PROGRAM_LIMIT_EXCEEDED,
                format!(
                    "a statement can expand at most {MAX_VIEW_TEXT_PER_STATEMENT} bytes of view text"
                ),
            )
            .with_hint("Use the views fewer times in one statement, or make them smaller.")
            .at(position));
        }
        let (mut query, selects) = parse_view(
            &view,
            self.settings,
            Nesting {
                depth: self.depth + 1,
                query_depth,
                queries: self.selects,
            },
        )
        .map_err(|error| error.at_if_unplaced(position))?;
        if selects > MAX_QUERIES {
            return Err(too_many_queries(position));
        }
        if self
            .resolution
            .seen_expanded
            .get_or_insert_with(HashSet::new)
            .insert(view.name.clone())
        {
            self.resolution.expanded.push(view.name.clone());
        }
        let mut path = Vec::with_capacity(view.search_path.len());
        for schema in &view.search_path {
            if schemas::schema_exists(self.reader, schema)? {
                path.push(schema.clone());
            }
        }
        self.stack.push(view.name.clone());
        self.view_paths.push(path.into());
        let walk_expressions = std::mem::replace(&mut self.walk_expressions, true);
        self.depth += 1;
        self.levels += 1;
        let mut ctes = Vec::new();
        let walked = self.walk_query(&mut query, &mut ctes);
        self.depth -= 1;
        self.levels -= 1;
        self.walk_expressions = walk_expressions;
        self.view_paths.pop();
        self.stack.pop();
        walked?;
        let column_names: Vec<Name> = view
            .columns
            .iter()
            .map(|column| Name {
                text: column.name.clone(),
                position,
            })
            .collect();
        let label = base_name(&view.name).to_owned();
        let derived = if view.expands_wildcard {
            self.wildcard_view(query, &view, column_names, &label, position)?
        } else {
            DerivedTable {
                query,
                column_names,
                dialect: view.dialect,
            }
        };
        if reference.alias.is_none() {
            reference.alias = Some(label);
        }
        reference.derived = Some(Box::new(derived));
        Ok(())
    }

    pub(crate) fn finish(self) -> Resolution {
        self.resolution
    }
}

impl Resolver<'_> {
    fn wildcard_view(
        &mut self,
        query: Query,
        view: &ViewDefinition,
        column_names: Vec<Name>,
        label: &str,
        position: usize,
    ) -> SqlResult<DerivedTable> {
        match view.dialect {
            Dialect::PostgreSql => Ok(DerivedTable {
                query: Query::of_select(
                    narrowed(query, column_names, label, view.dialect, position),
                    position,
                ),
                column_names: Vec::new(),
                dialect: view.dialect,
            }),
            Dialect::Sqlite if view.has_column_list => {
                let present = self.output_columns(&query)?;
                if present.len() != column_names.len() {
                    return Err(SqlError::new(
                        SqlState::INVALID_OBJECT_DEFINITION,
                        format!(
                            "expected {} columns for '{label}' but got {}",
                            column_names.len(),
                            present.len()
                        ),
                    )
                    .at(position));
                }
                Ok(DerivedTable {
                    query,
                    column_names,
                    dialect: view.dialect,
                })
            }
            Dialect::Sqlite => Ok(DerivedTable {
                query,
                column_names: Vec::new(),
                dialect: view.dialect,
            }),
            Dialect::MySql | Dialect::Oracle | Dialect::SqlServer => {
                let present = self.output_columns(&query)?;
                let sources = source_positions(view, &present)
                    .ok_or_else(|| invalid_view(view, self.settings))?;
                Ok(DerivedTable {
                    query: Query::of_select(
                        by_source_position(query, view, &present, &sources, label, position),
                        position,
                    ),
                    column_names: Vec::new(),
                    dialect: view.dialect,
                })
            }
        }
    }

    fn output_columns(&mut self, query: &Query) -> SqlResult<Vec<OutputColumn>> {
        let described = crate::execute::query(self.reader, self.tables, query, true)?;
        Ok(described.columns.unwrap_or_default())
    }
}

fn same_column_name(dialect: Dialect, left: &str, right: &str) -> bool {
    match dialect {
        Dialect::MySql => left
            .chars()
            .flat_map(char::to_lowercase)
            .eq(right.chars().flat_map(char::to_lowercase)),
        _ => left == right,
    }
}

fn source_positions(view: &ViewDefinition, present: &[OutputColumn]) -> Option<Vec<usize>> {
    let unchanged = present.len() == view.columns.len()
        && present
            .iter()
            .zip(&view.columns)
            .all(|(found, column)| found.name == column.source);
    if unchanged {
        return Some((0..present.len()).collect());
    }
    let mut claimed = vec![false; present.len()];
    let mut positions = Vec::with_capacity(view.columns.len());
    for column in &view.columns {
        let unclaimed = |matches: &dyn Fn(&str) -> bool| {
            present
                .iter()
                .enumerate()
                .position(|(index, found)| !claimed[index] && matches(&found.name))
        };
        let position = unclaimed(&|name| name == column.source)
            .or_else(|| unclaimed(&|name| same_column_name(view.dialect, name, &column.source)))?;
        claimed[position] = true;
        positions.push(position);
    }
    Some(positions)
}

fn invalid_view(view: &ViewDefinition, settings: Option<&Settings>) -> SqlError {
    let name = base_name(&view.name);
    match view.dialect {
        Dialect::MySql => SqlError::new(
            SqlState::MYSQL_GENERAL,
            format!(
                "View '{}.{name}' references invalid table(s) or column(s) or function(s) or definer/invoker of view lack rights to use them",
                settings.map_or_else(|| "eggdb".to_owned(), |settings| settings.database.clone())
            ),
        )
        .with_vendor_code(1356),
        Dialect::SqlServer => SqlError::new(
            SqlState::INVALID_OBJECT_DEFINITION,
            format!(
                "Could not use view or function '{}' because of binding errors.",
                crate::schemas::display(&view.name)
            ),
        )
        .with_sqlserver_code(4413),
        _ => SqlError::new(
            SqlState::INVALID_OBJECT_DEFINITION,
            format!(
                "view \"{}\" has errors",
                crate::lexer::swap_uniform_case(name)
            ),
        )
        .with_oracle_code(4063),
    }
}

fn by_source_position(
    query: Query,
    view: &ViewDefinition,
    present: &[OutputColumn],
    sources: &[usize],
    label: &str,
    position: usize,
) -> Select {
    let mut renamed: Vec<Name> = present
        .iter()
        .enumerate()
        .map(|(index, column)| Name {
            text: format!(
                "{}{}{index}",
                column.name,
                crate::temp_tables::TEMP_TABLE_MARKER
            ),
            position,
        })
        .collect();
    for (column, source) in view.columns.iter().zip(sources) {
        if let Some(name) = renamed.get_mut(*source) {
            name.text.clone_from(&column.name);
        }
    }
    let items = column_items(
        view.columns.iter().map(|column| column.name.as_str()),
        position,
    );
    select_from(items, query, renamed, label, view.dialect, position)
}

fn column_items<'n>(names: impl Iterator<Item = &'n str>, position: usize) -> Vec<SelectItem> {
    names
        .map(|name| SelectItem::Expr {
            expr: Expr {
                kind: ExprKind::Column {
                    qualifier: None,
                    name: name.to_owned(),
                },
                position,
            },
            alias: None,
        })
        .collect()
}

fn select_from(
    items: Vec<SelectItem>,
    query: Query,
    column_names: Vec<Name>,
    label: &str,
    dialect: Dialect,
    position: usize,
) -> Select {
    Select {
        distinct: false,
        items,
        from: Some(FromClause {
            first: TableReference {
                name: Name {
                    text: label.to_owned(),
                    position,
                },
                alias: Some(label.to_owned()),
                derived: Some(Box::new(DerivedTable {
                    query,
                    column_names,
                    dialect,
                })),
                generated: None,
            },
            joins: Vec::new(),
        }),
        filter: None,
        group_by: Vec::new(),
        having: None,
        order_by: Vec::new(),
        limit: None,
        offset: None,
        source_row_limit: None,
    }
}

fn narrowed(
    query: Query,
    column_names: Vec<Name>,
    label: &str,
    dialect: Dialect,
    position: usize,
) -> Select {
    let items = column_items(column_names.iter().map(|name| name.text.as_str()), position);
    select_from(items, query, column_names, label, dialect, position)
}

pub(crate) fn resolve_view_body(
    reader: &dyn KeyValueRead,
    tables: &mut TableCache,
    view: &str,
    query: &mut Query,
    path: Vec<String>,
    settings: Option<&Settings>,
) -> SqlResult<Resolution> {
    let mut resolver = Resolver::new(reader, tables, settings, true)?.with_path(path)?;
    resolver.stack.push(view.to_owned());
    resolver.recorded_at = Some(1);
    resolver.levels = 1;
    resolver.depth = 1;
    resolver.query(query)?;
    Ok(resolver.finish())
}

pub(crate) struct Binding<'a> {
    pub(crate) settings: &'a Settings,
    pub(crate) walk_expressions: bool,
}

impl Binding<'_> {
    fn resolver<'r>(
        &'r self,
        reader: &'r dyn KeyValueRead,
        tables: &'r mut TableCache,
    ) -> SqlResult<Resolver<'r>> {
        crate::pg_catalog::load_if_wanted(reader)?;
        Resolver::new(reader, tables, Some(self.settings), self.walk_expressions)
    }

    pub(crate) fn select(
        &self,
        reader: &dyn KeyValueRead,
        tables: &mut TableCache,
        select: &mut Select,
    ) -> SqlResult<()> {
        self.resolver(reader, tables)?.select(select)
    }

    pub(crate) fn query(
        &self,
        reader: &dyn KeyValueRead,
        tables: &mut TableCache,
        query: &mut Query,
    ) -> SqlResult<()> {
        self.resolver(reader, tables)?.query(query)
    }

    pub(crate) fn names(
        &self,
        reader: &dyn KeyValueRead,
        tables: &mut TableCache,
        names: &mut [Name],
    ) -> SqlResult<()> {
        self.resolver(reader, tables)?.names(names)
    }

    pub(crate) fn target(
        &self,
        reader: &dyn KeyValueRead,
        tables: &mut TableCache,
        reference: &mut TableReference,
    ) -> SqlResult<()> {
        self.resolver(reader, tables)?.target(reference)
    }

    pub(crate) fn insert(
        &self,
        reader: &dyn KeyValueRead,
        tables: &mut TableCache,
        insert: &mut Insert,
    ) -> SqlResult<()> {
        let mut resolver = self.resolver(reader, tables)?;
        resolver.target(&mut insert.table)?;
        if !self.walk_expressions {
            return Ok(());
        }
        let mut exprs: Vec<&mut Expr> = Vec::new();
        if let InsertSource::Values(rows) = &mut insert.source {
            for row in rows {
                for value in row {
                    if let InsertValue::Expr(expr) = value {
                        exprs.push(expr);
                    }
                }
            }
        }
        if let OnDuplicate::Update(assignments) = &mut insert.on_duplicate {
            exprs.extend(assignments.iter_mut().map(|(_, expr)| expr));
        }
        exprs.extend(returning_exprs(&mut insert.returning));
        resolver.expressions(exprs)
    }

    pub(crate) fn update(
        &self,
        reader: &dyn KeyValueRead,
        tables: &mut TableCache,
        update: &mut Update,
    ) -> SqlResult<()> {
        let mut resolver = self.resolver(reader, tables)?;
        resolver.target(&mut update.table)?;
        if !self.walk_expressions {
            return Ok(());
        }
        let mut exprs: Vec<&mut Expr> = update
            .assignments
            .iter_mut()
            .filter_map(|(_, value)| match value {
                InsertValue::Expr(expr) => Some(expr),
                InsertValue::Default => None,
            })
            .collect();
        exprs.extend(update.filter.iter_mut());
        exprs.extend(returning_exprs(&mut update.returning));
        resolver.expressions(exprs)
    }

    pub(crate) fn merge(
        &self,
        reader: &dyn KeyValueRead,
        tables: &mut TableCache,
        merge: &mut MergeStatement,
    ) -> SqlResult<()> {
        self.select(reader, tables, &mut merge.source)?;
        let mut resolver = self.resolver(reader, tables)?;
        resolver.target(&mut merge.target)?;
        if !self.walk_expressions {
            return Ok(());
        }
        let mut exprs: Vec<&mut Expr> = vec![&mut merge.on];
        if let Some(update) = &mut merge.update {
            exprs.extend(update.assignments.iter_mut().map(|(_, expr)| expr));
            exprs.extend(update.condition.iter_mut());
            exprs.extend(update.delete_condition.iter_mut());
        }
        if let Some(insert) = &mut merge.insert {
            exprs.extend(insert.values.iter_mut());
            exprs.extend(insert.condition.iter_mut());
        }
        resolver.expressions(exprs)
    }

    pub(crate) fn delete(
        &self,
        reader: &dyn KeyValueRead,
        tables: &mut TableCache,
        delete: &mut Delete,
    ) -> SqlResult<()> {
        let mut resolver = self.resolver(reader, tables)?;
        resolver.target(&mut delete.table)?;
        if !self.walk_expressions {
            return Ok(());
        }
        let mut exprs: Vec<&mut Expr> = delete.filter.iter_mut().collect();
        exprs.extend(returning_exprs(&mut delete.returning));
        resolver.expressions(exprs)
    }

    pub(crate) fn create_table(
        &self,
        reader: &dyn KeyValueRead,
        tables: &mut TableCache,
        create: &mut CreateTable,
    ) -> SqlResult<()> {
        create.name.text = schemas::creation_name(reader, &create.name)?;
        let mut resolver = self.resolver(reader, tables)?;
        for spec in create
            .columns
            .iter_mut()
            .flat_map(|column| column.constraints.iter_mut())
            .chain(create.constraints.iter_mut())
        {
            if let ConstraintKind::ForeignKey(key) = &mut spec.kind {
                resolver.name(&mut key.parent)?;
            }
        }
        Ok(())
    }

    pub(crate) fn alter_table(
        &self,
        reader: &dyn KeyValueRead,
        tables: &mut TableCache,
        alter: &mut AlterTable,
    ) -> SqlResult<()> {
        let mut resolver = self.resolver(reader, tables)?;
        resolver.name(&mut alter.name)?;
        for action in &mut alter.actions {
            if let AlterAction::AddConstraint(spec) = action
                && let ConstraintKind::ForeignKey(key) = &mut spec.kind
            {
                resolver.name(&mut key.parent)?;
            }
        }
        Ok(())
    }

    pub(crate) fn statement(
        &self,
        reader: &dyn KeyValueRead,
        tables: &mut TableCache,
        statement: &mut Statement,
    ) -> SqlResult<()> {
        match statement {
            Statement::Select(select) => self.select(reader, tables, select),
            Statement::Query(query) => self.query(reader, tables, query),
            Statement::Insert(insert) => self.insert(reader, tables, insert),
            Statement::Update(update) => self.update(reader, tables, update),
            Statement::Delete(delete) => self.delete(reader, tables, delete),
            Statement::Merge(merge) => self.merge(reader, tables, merge),
            Statement::Explain(explain) => self.statement(reader, tables, &mut explain.statement),
            _ => Ok(()),
        }
    }
}

fn returning_exprs(returning: &mut Option<Vec<SelectItem>>) -> impl Iterator<Item = &mut Expr> {
    returning
        .iter_mut()
        .flatten()
        .filter_map(|item| match item {
            SelectItem::Expr { expr, .. } => Some(expr),
            SelectItem::Wildcard { .. } => None,
        })
}

impl Binding<'_> {
    pub(crate) fn comment(
        &self,
        reader: &dyn KeyValueRead,
        tables: &mut TableCache,
        comment: &mut crate::ast::Comment,
    ) -> SqlResult<()> {
        use crate::ast::CommentTarget;
        let mut resolver = self.resolver(reader, tables)?;
        match &mut comment.target {
            CommentTarget::Relation { name, .. } => resolver.name(name),
            CommentTarget::Column { table, .. } | CommentTarget::Constraint { table, .. } => {
                resolver.name(table)
            }
            CommentTarget::Index(_) | CommentTarget::Schema(_) | CommentTarget::Extension(_) => {
                Ok(())
            }
        }
    }
}
