use std::fmt;
use std::ops::Range;

use eggdb_platform::window::{Key, KeyPress};

use crate::canvas::Rect;
use crate::connection::ResultTable;
use crate::dialect::QueryDialect;
use crate::main_window::{BODY, CODE, Painter, SMALL, elided};
use crate::status_line::thousands;

/// A node's height in the drawn tree.
pub const NODE_HEIGHT: f32 = 40.0;
/// The distance from one level's top to the next level's top.
pub const LEVEL_SPACING: f32 = 56.0;
/// The gap between two siblings' subtrees.
pub const SIBLING_GAP: f32 = 24.0;
/// The narrowest a node is drawn.
pub const NODE_MIN_WIDTH: f32 = 120.0;
/// The widest a node is drawn; longer operations are cut with `…`.
pub const NODE_MAX_WIDTH: f32 = 320.0;
/// The margin between the panel's edges and a tree larger than the panel.
pub const PANEL_MARGIN: f32 = 16.0;
/// The share of the root's total from which a node is `warm`.
pub const WARM_SHARE: f64 = 0.20;
/// The share of the root's total from which a node is `hot`.
pub const HOT_SHARE: f64 = 0.50;
/// How many times the estimated and the actual rows differ for a node to be `misestimated`.
pub const MISESTIMATE_FACTOR: f64 = 10.0;
/// The most nodes a plan may have before it is refused as too large to draw.
pub const MAX_NODES: usize = 1000;
/// The deepest a plan's tree may be.
pub const MAX_DEPTH: usize = 64;
/// The most lines a plan's text form may have.
pub const MAX_LINES: usize = 20_000;
const MAX_DETAILS: usize = 64;
const NODE_PADDING: f32 = 12.0;
const TOOLTIP_GAP: f32 = 4.0;
const TOOLTIP_PADDING: f32 = 8.0;
const MOST_TOOLTIP_LINES: usize = 24;
const RADIUS: f32 = 4.0;
const EXPLAIN_WORD: &str = "explain";
const XPLAN_COLUMN: &str = "PLAN_TABLE_OUTPUT";
const SHOWPLAN_TEXT_COLUMN: &str = "StmtText";
const SHOWPLAN_BRANCH: &str = "|--";
const SHOWPLAN_OBJECT: &str = "OBJECT:(";
const ORACLE_EXPLAIN: &str = "EXPLAIN PLAN FOR ";
const ORACLE_DISPLAY: &str = "SELECT * FROM TABLE(DBMS_XPLAN.DISPLAY)";
/// How long the pointer rests on a node before its tooltip shows (`docs/ux/workspace.md` 5.4).
pub const TOOLTIP_DELAY: std::time::Duration = std::time::Duration::from_millis(600);
/// The widest a node's tooltip is drawn.
pub const TOOLTIP_MAX_WIDTH: f32 = 480.0;

/// Why the engine's answer could not be read as a plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    /// The answer had no node.
    Empty,
    /// The answer had more than [`MAX_NODES`] nodes.
    TooManyNodes,
    /// The tree was deeper than [`MAX_DEPTH`].
    TooDeep,
    /// The text form had more than [`MAX_LINES`] lines.
    TooManyLines,
    /// The answer's columns are not a plan's.
    NotAPlan,
}

impl fmt::Display for PlanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::Empty => "The engine's plan has no node.".to_owned(),
            Self::TooManyNodes => {
                format!(
                    "The plan has more than {MAX_NODES} nodes: switch to Text (Alt+T) to read it."
                )
            }
            Self::TooDeep => {
                format!(
                    "The plan is deeper than {MAX_DEPTH} levels: switch to Text (Alt+T) to read it."
                )
            }
            Self::TooManyLines => format!("The plan has more than {MAX_LINES} lines."),
            Self::NotAPlan => "The statement's answer is not a plan.".to_owned(),
        };
        formatter.write_str(&text)
    }
}

/// One node of a plan: the operation as the engine names it, its estimates, its measures
/// under `ANALYZE`, and its place in the tree.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PlanNode {
    /// `Index Scan using users_pkey on users`, `Hash Join`, `SCAN customers`.
    pub operation: String,
    /// The engine's extra lines, such as `Filter: (total > 100)`.
    pub details: Vec<String>,
    /// The table the node reads, when its operation names one.
    pub relation: Option<String>,
    /// The estimated startup and total cost.
    pub cost: Option<(f64, f64)>,
    /// The estimated rows.
    pub estimated_rows: Option<f64>,
    /// The measured startup and total time of one loop, in milliseconds.
    pub actual_time_ms: Option<(f64, f64)>,
    /// The rows one loop returned.
    pub actual_rows: Option<f64>,
    /// How many times the node ran.
    pub loops: Option<f64>,
    /// Whether the engine says the node never ran.
    pub never_executed: bool,
    /// The pages the node read, from the cache or from disk.
    pub buffers: Option<u64>,
    /// The parent's index; `None` for the root.
    pub parent: Option<usize>,
    /// The children's indexes, in the engine's order.
    pub children: Vec<usize>,
    /// The level, 0 for the root.
    pub depth: usize,
}

/// How a node is tinted (`docs/ux/workspace.md` 5.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Heat {
    /// Under [`WARM_SHARE`] of the root's total, or no share known.
    Plain,
    /// From [`WARM_SHARE`] to under [`HOT_SHARE`].
    Warm,
    /// [`HOT_SHARE`] and over.
    Hot,
}

impl Heat {
    /// The heat of a node with `share` of the root's total.
    pub fn of(share: Option<f64>) -> Self {
        match share {
            Some(share) if share >= HOT_SHARE => Self::Hot,
            Some(share) if share >= WARM_SHARE => Self::Warm,
            _ => Self::Plain,
        }
    }
}

/// The colour role a piece of a node's second row is drawn in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ink {
    /// `textSecondary`.
    Secondary,
    /// `warning`.
    Warning,
    /// `error`.
    Error,
}

/// A plan read from the engine's answer to `EXPLAIN`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Plan {
    /// The nodes; the root is the first, and a parent comes before its children.
    pub nodes: Vec<PlanNode>,
    /// Whether the plan carries measures (`EXPLAIN ANALYZE`).
    pub analyze: bool,
    /// The plan's text form, the engine's rows as the CLI prints them.
    pub text: Vec<String>,
    /// `Planning Time`, in milliseconds.
    pub planning_ms: Option<f64>,
    /// `Execution Time`, in milliseconds.
    pub execution_ms: Option<f64>,
}

impl Plan {
    /// Reads a statement's answer to `EXPLAIN`: PostgreSQL's one-column text form, SQLite's
    /// `id, parent, notused, detail` rows, MySQL's tabular rows, Oracle's
    /// `DBMS_XPLAN.DISPLAY` lines (`PLAN_TABLE_OUTPUT`), SQL Server's `SHOWPLAN_ALL` and
    /// `STATISTICS PROFILE` rows or its `SHOWPLAN_TEXT` tree (`StmtText`).
    pub fn from_result(table: &ResultTable) -> Result<Self, PlanError> {
        let column = |name: &str| {
            table
                .columns
                .iter()
                .position(|column| column.eq_ignore_ascii_case(name))
        };
        if let (Some(id), Some(parent), Some(detail)) =
            (column("id"), column("parent"), column("detail"))
        {
            return Self::from_sqlite_rows(&table.rows, (id, parent, detail));
        }
        if column("NodeId").is_some() && column("Parent").is_some() {
            return Self::from_showplan_rows(table);
        }
        if let (Some(name), Some(kind)) = (column("table"), column("type")) {
            return Self::from_mysql_rows(&table.rows, (name, kind, column("key"), column("rows")));
        }
        if table.columns.len() == 1 {
            let lines: Vec<String> = table
                .rows
                .iter()
                .map(|row| row.first().cloned().flatten().unwrap_or_default())
                .collect();
            if column(XPLAN_COLUMN).is_some() {
                return Self::from_xplan_lines(&lines);
            }
            if column(SHOWPLAN_TEXT_COLUMN).is_some() {
                return Self::from_showplan_text(&lines);
            }
            return Self::from_postgresql_text(&lines);
        }
        Err(PlanError::NotAPlan)
    }

    /// Reads Oracle's `DBMS_XPLAN.DISPLAY` lines: the rows of the `Id | Operation | Name`
    /// table, each operation's depth its indent, with the `Rows` and `Cost (%CPU)` columns
    /// when the format shows them, and each `Predicate Information` line under its node.
    pub fn from_xplan_lines(lines: &[String]) -> Result<Self, PlanError> {
        if lines.len() > MAX_LINES {
            return Err(PlanError::TooManyLines);
        }
        let mut plan = Self {
            text: lines.to_vec(),
            ..Self::default()
        };
        let mut header: Option<Vec<String>> = None;
        let mut open: Vec<(usize, usize)> = Vec::new();
        let mut ids: Vec<(u64, usize)> = Vec::new();
        let mut predicates = false;
        let mut last_predicate: Option<usize> = None;
        for line in lines {
            let trimmed = line.trim();
            if trimmed.starts_with('|') {
                let inner = trimmed.strip_prefix('|').unwrap_or(trimmed);
                let cells: Vec<&str> = inner
                    .strip_suffix('|')
                    .unwrap_or(inner)
                    .split('|')
                    .collect();
                let Some(names) = &header else {
                    if cells
                        .iter()
                        .any(|cell| cell.trim().eq_ignore_ascii_case("Operation"))
                    {
                        header = Some(cells.iter().map(|cell| cell.trim().to_owned()).collect());
                    }
                    continue;
                };
                let Some(row) = XplanRow::read(names, &cells) else {
                    continue;
                };
                while open.last().is_some_and(|&(indent, _)| indent >= row.indent) {
                    open.pop();
                }
                let parent = open
                    .last()
                    .map(|&(_, index)| index)
                    .or_else(|| (!plan.nodes.is_empty()).then_some(0));
                let index = plan.push_node(row.node(parent))?;
                open.push((row.indent, index));
                ids.push((row.id, index));
                continue;
            }
            if trimmed.starts_with("Predicate Information") {
                predicates = true;
                continue;
            }
            if trimmed == "Note" {
                predicates = false;
                continue;
            }
            if !predicates || trimmed.is_empty() || trimmed.starts_with('-') {
                continue;
            }
            let numbered = trimmed.split_once(" - ").and_then(|(id, rest)| {
                let id = id
                    .trim()
                    .trim_start_matches('*')
                    .trim()
                    .parse::<u64>()
                    .ok()?;
                let index = ids.iter().find(|(known, _)| *known == id)?.1;
                Some((index, rest.trim()))
            });
            let (owner, text) = match numbered {
                Some((index, rest)) => (Some(index), rest),
                None => (last_predicate, trimmed),
            };
            if let Some(node) = owner.and_then(|index| plan.nodes.get_mut(index))
                && node.details.len() < MAX_DETAILS
            {
                node.details.push(text.to_owned());
            }
            last_predicate = owner;
        }
        if plan.nodes.is_empty() {
            return Err(PlanError::Empty);
        }
        Ok(plan)
    }

    /// Reads SQL Server's `SHOWPLAN_ALL` rows, and its `STATISTICS PROFILE` rows with `Rows`
    /// and `Executes` first: the statement's row is the root, and each `PLAN_ROW` hangs under
    /// the row whose `NodeId` is its `Parent`.
    pub fn from_showplan_rows(table: &ResultTable) -> Result<Self, PlanError> {
        let column = |name: &str| {
            table
                .columns
                .iter()
                .position(|column| column.eq_ignore_ascii_case(name))
        };
        let (Some(node_at), Some(parent_at)) = (column("NodeId"), column("Parent")) else {
            return Err(PlanError::NotAPlan);
        };
        if table.rows.len() > MAX_NODES {
            return Err(PlanError::TooManyNodes);
        }
        let at = ShowplanColumns {
            text: column(SHOWPLAN_TEXT_COLUMN),
            physical: column("PhysicalOp"),
            logical: column("LogicalOp"),
            argument: column("Argument"),
            kind: column("Type"),
            estimated_rows: column("EstimateRows"),
            cost: column("TotalSubtreeCost"),
            rows: column("Rows"),
            executes: column("Executes"),
        };
        let mut plan = Self {
            analyze: at.rows.is_some(),
            ..Self::default()
        };
        let mut ids: Vec<(String, usize)> = Vec::new();
        for row in &table.rows {
            let cell =
                |place: Option<usize>| place.and_then(|place| row.get(place).cloned().flatten());
            let text = cell(at.text).unwrap_or_default();
            plan.text.extend(text.lines().map(str::to_owned));
            let parent_id = cell(Some(parent_at)).unwrap_or_default();
            let parent = ids
                .iter()
                .find(|(known, _)| *known == parent_id)
                .map(|&(_, index)| index)
                .or_else(|| (!plan.nodes.is_empty()).then_some(0));
            let operator = cell(at.kind).is_none_or(|kind| kind.eq_ignore_ascii_case("PLAN_ROW"));
            let mut node = if operator {
                let argument = cell(at.argument);
                let physical =
                    cell(at.physical).unwrap_or_else(|| showplan_physical(&text).to_owned());
                let (operation, relation) = sqlserver_operation(&physical, argument.as_deref());
                let mut details: Vec<String> = cell(at.logical)
                    .filter(|logical| *logical != physical)
                    .into_iter()
                    .collect();
                details.extend(argument);
                PlanNode {
                    operation,
                    relation,
                    details,
                    ..PlanNode::default()
                }
            } else {
                PlanNode {
                    operation: cell(at.kind).unwrap_or_else(|| "Statement".to_owned()),
                    details: vec![text.trim().to_owned()],
                    ..PlanNode::default()
                }
            };
            let executes = cell(at.executes).as_deref().and_then(number);
            let rows = cell(at.rows).as_deref().and_then(number);
            node.estimated_rows = cell(at.estimated_rows).as_deref().and_then(number);
            node.cost = cell(at.cost)
                .as_deref()
                .and_then(number)
                .map(|total| (0.0, total));
            node.loops = executes.filter(|executes| *executes > 0.0);
            node.never_executed = executes == Some(0.0);
            node.actual_rows = rows.map(|rows| rows / node.loops.unwrap_or(1.0));
            node.parent = parent;
            let index = plan.push_node(node)?;
            ids.push((cell(Some(node_at)).unwrap_or_default(), index));
        }
        if plan.nodes.is_empty() {
            return Err(PlanError::Empty);
        }
        Ok(plan)
    }

    /// Reads SQL Server's `SHOWPLAN_TEXT` tree: one operator per `|--` line, its depth five
    /// columns per level; lines without `|--`, such as the statement's text, are left out.
    pub fn from_showplan_text(lines: &[String]) -> Result<Self, PlanError> {
        if lines.len() > MAX_LINES {
            return Err(PlanError::TooManyLines);
        }
        let mut plan = Self {
            text: lines.to_vec(),
            ..Self::default()
        };
        let mut open: Vec<usize> = Vec::new();
        for line in lines {
            let Some(at) = line.find(SHOWPLAN_BRANCH) else {
                continue;
            };
            let level = at.saturating_sub(2) / 5;
            let body = line[at + SHOWPLAN_BRANCH.len()..].trim();
            open.truncate(level);
            let parent = open
                .last()
                .copied()
                .or_else(|| (!plan.nodes.is_empty()).then_some(0));
            let physical = showplan_physical(body);
            let (operation, relation) = sqlserver_operation(physical, Some(body));
            let details = if body.len() > physical.len() {
                vec![body.to_owned()]
            } else {
                Vec::new()
            };
            let index = plan.push_node(PlanNode {
                operation,
                relation,
                details,
                parent,
                ..PlanNode::default()
            })?;
            open.push(index);
        }
        if plan.nodes.is_empty() {
            return Err(PlanError::Empty);
        }
        Ok(plan)
    }

    fn push_node(&mut self, mut node: PlanNode) -> Result<usize, PlanError> {
        if self.nodes.len() >= MAX_NODES {
            return Err(PlanError::TooManyNodes);
        }
        let parent = node.parent.filter(|&parent| parent < self.nodes.len());
        node.parent = parent;
        node.depth = parent.map_or(0, |parent| self.nodes[parent].depth + 1);
        if node.depth > MAX_DEPTH {
            return Err(PlanError::TooDeep);
        }
        let index = self.nodes.len();
        if let Some(parent) = parent {
            self.nodes[parent].children.push(index);
        }
        self.nodes.push(node);
        Ok(index)
    }

    /// Reads PostgreSQL's text form: the root on the first line, each child on a line with
    /// `->` indented under its parent, the engine's extra lines under their node, and the
    /// `Planning Time` and `Execution Time` lines after the tree.
    pub fn from_postgresql_text(lines: &[String]) -> Result<Self, PlanError> {
        if lines.len() > MAX_LINES {
            return Err(PlanError::TooManyLines);
        }
        let mut plan = Self {
            text: lines.to_vec(),
            ..Self::default()
        };
        let mut open: Vec<(usize, usize)> = Vec::new();
        let mut footer = false;
        for line in lines {
            let indent = line.len() - line.trim_start_matches(' ').len();
            let body = line.trim();
            if body.is_empty() {
                continue;
            }
            if plan.nodes.is_empty() {
                plan.nodes.push(node_from_text(
                    body.strip_prefix("->").unwrap_or(body).trim_start(),
                    None,
                    0,
                ));
                open.push((0, 0));
                continue;
            }
            if indent == 0 {
                footer = true;
            }
            if footer {
                plan.read_footer(body);
                continue;
            }
            if let Some(rest) = body.strip_prefix("->") {
                while open.last().is_some_and(|&(column, _)| column >= indent) {
                    open.pop();
                }
                let parent = open.last().map_or(0, |&(_, index)| index);
                let depth = plan.nodes[parent].depth + 1;
                if depth > MAX_DEPTH {
                    return Err(PlanError::TooDeep);
                }
                if plan.nodes.len() >= MAX_NODES {
                    return Err(PlanError::TooManyNodes);
                }
                let index = plan.nodes.len();
                plan.nodes
                    .push(node_from_text(rest.trim_start(), Some(parent), depth));
                plan.nodes[parent].children.push(index);
                open.push((indent, index));
                continue;
            }
            let owner = open
                .iter()
                .rev()
                .find(|&&(column, _)| column < indent)
                .map_or(0, |&(_, index)| index);
            let node = &mut plan.nodes[owner];
            match buffers_of(body) {
                Some(pages) => node.buffers = Some(node.buffers.unwrap_or(0).saturating_add(pages)),
                None if node.details.len() < MAX_DETAILS => node.details.push(body.to_owned()),
                None => {}
            }
        }
        if plan.nodes.is_empty() {
            return Err(PlanError::Empty);
        }
        plan.analyze = plan.execution_ms.is_some()
            || plan.nodes.iter().any(|node| {
                node.actual_rows.is_some() || node.actual_time_ms.is_some() || node.never_executed
            });
        Ok(plan)
    }

    /// Reads SQLite's `EXPLAIN QUERY PLAN` rows, `columns` being the places of `id`,
    /// `parent` and `detail`: a row whose parent is no earlier row's id hangs under the root,
    /// which is `QUERY PLAN` when more than one row has none.
    pub fn from_sqlite_rows(
        rows: &[Vec<Option<String>>],
        columns: (usize, usize, usize),
    ) -> Result<Self, PlanError> {
        if rows.len() >= MAX_NODES {
            return Err(PlanError::TooManyNodes);
        }
        let cell = |row: &Vec<Option<String>>, at: usize| {
            row.get(at).cloned().flatten().unwrap_or_default()
        };
        let mut text = Vec::with_capacity(rows.len() + 1);
        text.push("QUERY PLAN".to_owned());
        let mut ids: Vec<(String, usize)> = Vec::with_capacity(rows.len());
        let mut nodes = vec![PlanNode {
            operation: "QUERY PLAN".to_owned(),
            ..PlanNode::default()
        }];
        for row in rows {
            let (id, parent_id, detail) = (
                cell(row, columns.0),
                cell(row, columns.1),
                cell(row, columns.2),
            );
            let parent = ids
                .iter()
                .find(|(known, _)| *known == parent_id)
                .map_or(0, |&(_, index)| index);
            let depth = nodes[parent].depth + 1;
            if depth > MAX_DEPTH {
                return Err(PlanError::TooDeep);
            }
            let branch = if depth > 1 { "`--" } else { "" };
            text.push(format!(
                "{}{branch}{detail}",
                "   ".repeat(depth.saturating_sub(1))
            ));
            let index = nodes.len();
            nodes.push(PlanNode {
                relation: sqlite_relation(&detail),
                operation: detail,
                parent: Some(parent),
                depth,
                ..PlanNode::default()
            });
            nodes[parent].children.push(index);
            ids.push((id, index));
        }
        if nodes.len() == 1 {
            return Err(PlanError::Empty);
        }
        if nodes[0].children.len() == 1 {
            nodes = lift_only_child(nodes);
        }
        Ok(Self {
            nodes,
            text,
            ..Self::default()
        })
    }

    /// Reads MySQL's tabular `EXPLAIN`, `columns` being the places of `table`, `type`, `key`
    /// and `rows`: one row is one table read, joined in order by nested loops.
    pub fn from_mysql_rows(
        rows: &[Vec<Option<String>>],
        columns: (usize, usize, Option<usize>, Option<usize>),
    ) -> Result<Self, PlanError> {
        if rows.is_empty() {
            return Err(PlanError::Empty);
        }
        if rows.len() >= MAX_NODES {
            return Err(PlanError::TooManyNodes);
        }
        let cell = |row: &Vec<Option<String>>, at: Option<usize>| {
            at.and_then(|at| row.get(at).cloned().flatten())
        };
        let reads: Vec<PlanNode> = rows
            .iter()
            .map(|row| {
                let table = cell(row, Some(columns.0)).unwrap_or_default();
                let kind = cell(row, Some(columns.1)).unwrap_or_default();
                let operation = match cell(row, columns.2) {
                    Some(key) => format!("{kind} on {table} using {key}"),
                    None => format!("{kind} on {table}"),
                };
                PlanNode {
                    operation,
                    relation: (!table.is_empty()).then_some(table),
                    estimated_rows: cell(row, columns.3).as_deref().and_then(number),
                    ..PlanNode::default()
                }
            })
            .collect();
        let text = reads.iter().map(|node| node.operation.clone()).collect();
        let mut nodes = Vec::with_capacity(reads.len() + 1);
        if reads.len() > 1 {
            nodes.push(PlanNode {
                operation: "Nested Loop".to_owned(),
                children: (1..=reads.len()).collect(),
                ..PlanNode::default()
            });
            nodes.extend(reads.into_iter().map(|node| PlanNode {
                parent: Some(0),
                depth: 1,
                ..node
            }));
        } else {
            nodes.extend(reads);
        }
        Ok(Self {
            nodes,
            text,
            ..Self::default()
        })
    }

    fn read_footer(&mut self, body: &str) {
        let milliseconds = |rest: &str| number(rest.trim().trim_end_matches("ms").trim());
        if let Some(rest) = body
            .strip_prefix("Planning Time:")
            .or_else(|| body.strip_prefix("Planning time:"))
        {
            self.planning_ms = milliseconds(rest);
        } else if let Some(rest) = body
            .strip_prefix("Execution Time:")
            .or_else(|| body.strip_prefix("Execution time:"))
            .or_else(|| body.strip_prefix("Total runtime:"))
        {
            self.execution_ms = milliseconds(rest);
        }
    }

    /// The node's total time over all its loops, in milliseconds: the root's is the
    /// statement's `Execution Time` when the engine measures no node.
    pub fn total_time_ms(&self, index: usize) -> Option<f64> {
        let node = self.nodes.get(index)?;
        if node.never_executed {
            return Some(0.0);
        }
        match node.actual_time_ms {
            Some((_, total)) => Some(total * node.loops.unwrap_or(1.0)),
            None if index == 0 => self.execution_ms,
            None => None,
        }
    }

    /// The node's own time, its total less its children's, when all of them are known.
    pub fn own_time_ms(&self, index: usize) -> Option<f64> {
        let total = self.total_time_ms(index)?;
        let mut children = 0.0;
        for &child in &self.nodes.get(index)?.children {
            children += self.total_time_ms(child)?;
        }
        Some((total - children).max(0.0))
    }

    /// The node's own cost, its total cost less its children's, when all of them are known.
    pub fn own_cost(&self, index: usize) -> Option<f64> {
        let node = self.nodes.get(index)?;
        let (_, total) = node.cost?;
        let mut children = 0.0;
        for &child in &node.children {
            children += self.nodes.get(child)?.cost?.1;
        }
        Some((total - children).max(0.0))
    }

    /// The node's share of the root's total: of its time under `ANALYZE` when the engine
    /// measures it, else of its cost; `None` when the engine gives neither.
    pub fn share(&self, index: usize) -> Option<f64> {
        let by_time = || {
            let root = self.total_time_ms(0).filter(|total| *total > 0.0)?;
            Some(self.own_time_ms(index)? / root)
        };
        let by_cost = || {
            let root = self
                .nodes
                .first()?
                .cost
                .map(|(_, total)| total)
                .filter(|total| *total > 0.0)?;
            Some(self.own_cost(index)? / root)
        };
        let share = if self.analyze {
            by_time().or_else(by_cost)
        } else {
            by_cost()
        };
        share.map(|share| share.clamp(0.0, 1.0))
    }

    /// The node's heat.
    pub fn heat(&self, index: usize) -> Heat {
        Heat::of(self.share(index))
    }

    /// The node's rows over all its loops as the engine measured them.
    pub fn actual_rows_total(&self, index: usize) -> Option<f64> {
        let node = self.nodes.get(index)?;
        Some(node.actual_rows? * node.loops.unwrap_or(1.0))
    }

    /// The estimated and the actual rows when they differ by [`MISESTIMATE_FACTOR`] or more.
    pub fn misestimate(&self, index: usize) -> Option<(f64, f64)> {
        let node = self.nodes.get(index)?;
        let (estimated, actual) = (node.estimated_rows?, node.actual_rows?);
        let (high, low) = (
            estimated.max(actual).max(1.0),
            estimated.min(actual).max(1.0),
        );
        (high / low >= MISESTIMATE_FACTOR)
            .then(|| (estimated, self.actual_rows_total(index).unwrap_or(actual)))
    }

    /// The node's second row without its share: `cost 0.29..8.31 · rows 1` under `EXPLAIN`,
    /// `12.4 ms · rows 1,204 · buffers 38` under `ANALYZE`, leaving out what the engine does
    /// not give.
    pub fn facts(&self, index: usize) -> String {
        let Some(node) = self.nodes.get(index) else {
            return String::new();
        };
        let mut parts = Vec::new();
        if self.analyze {
            if let Some(time) = self.own_time_ms(index) {
                parts.push(milliseconds_text(time));
            }
            if let Some(rows) = self.actual_rows_total(index).or(node.estimated_rows) {
                parts.push(format!("rows {}", rows_count(rows)));
            }
            if let Some(pages) = node.buffers {
                parts.push(format!(
                    "buffers {}",
                    thousands(usize::try_from(pages).unwrap_or(usize::MAX))
                ));
            }
        } else {
            if let Some((startup, total)) = node.cost {
                parts.push(format!("cost {startup:.2}..{total:.2}"));
            }
            if let Some(rows) = node.estimated_rows {
                parts.push(format!("rows {}", rows_count(rows)));
            }
        }
        parts.join(" · ")
    }

    /// The node's second row in its drawn pieces: the facts in `textSecondary`, the share
    /// from 20 % in `warning` or `error`, and a misestimate in `warning`.
    pub fn second_row(&self, index: usize) -> Vec<(String, Ink)> {
        let mut pieces = Vec::new();
        let facts = self.facts(index);
        if !facts.is_empty() {
            pieces.push((facts, Ink::Secondary));
        }
        let mut add = |text: String, ink: Ink| {
            let text = if pieces.is_empty() {
                text
            } else {
                format!(" · {text}")
            };
            pieces.push((text, ink));
        };
        let share = self.share(index);
        match (Heat::of(share), share) {
            (Heat::Warm, Some(share)) => add(percent_text(share), Ink::Warning),
            (Heat::Hot, Some(share)) => add(percent_text(share), Ink::Error),
            _ => {}
        }
        if let Some((estimated, actual)) = self.misestimate(index) {
            add(
                format!("est. {} → {}", rows_count(estimated), rows_count(actual)),
                Ink::Warning,
            );
        }
        pieces
    }

    /// What a screen reader says after the node's name: `cost 0.29 to 8.31, rows 1, 62
    /// percent, hot`.
    pub fn spoken(&self, index: usize) -> String {
        let Some(node) = self.nodes.get(index) else {
            return String::new();
        };
        let mut parts = Vec::new();
        if self.analyze {
            if let Some(time) = self.own_time_ms(index) {
                parts.push(milliseconds_text(time).replace(" ms", " milliseconds"));
            }
            if let Some(rows) = self.actual_rows_total(index).or(node.estimated_rows) {
                parts.push(format!("rows {}", rows_count(rows)));
            }
            if let Some(pages) = node.buffers {
                parts.push(format!(
                    "buffers {}",
                    thousands(usize::try_from(pages).unwrap_or(usize::MAX))
                ));
            }
        } else {
            if let Some((startup, total)) = node.cost {
                parts.push(format!("cost {startup:.2} to {total:.2}"));
            }
            if let Some(rows) = node.estimated_rows {
                parts.push(format!("rows {}", rows_count(rows)));
            }
        }
        let share = self.share(index);
        if let Some(share) = share.filter(|share| *share >= WARM_SHARE) {
            parts.push(format!("{} percent", (share * 100.0).round()));
        }
        match Heat::of(share) {
            Heat::Hot => parts.push("hot".to_owned()),
            Heat::Warm => parts.push("warm".to_owned()),
            Heat::Plain => {}
        }
        if let Some((estimated, actual)) = self.misestimate(index) {
            parts.push(format!(
                "misestimated, estimated {} rows, actual {}",
                rows_count(estimated),
                rows_count(actual)
            ));
        }
        parts.join(", ")
    }
}

struct ShowplanColumns {
    text: Option<usize>,
    physical: Option<usize>,
    logical: Option<usize>,
    argument: Option<usize>,
    kind: Option<usize>,
    estimated_rows: Option<usize>,
    cost: Option<usize>,
    rows: Option<usize>,
    executes: Option<usize>,
}

struct XplanRow {
    id: u64,
    indent: usize,
    operation: String,
    name: Option<String>,
    rows: Option<f64>,
    cost: Option<f64>,
}

impl XplanRow {
    fn read(names: &[String], cells: &[&str]) -> Option<Self> {
        let cell = |name: &str| {
            names
                .iter()
                .position(|known| known.eq_ignore_ascii_case(name))
                .and_then(|place| cells.get(place).copied())
        };
        let id = cell("Id")?
            .trim()
            .trim_start_matches('*')
            .trim()
            .parse()
            .ok()?;
        let raw = cell("Operation")?;
        let operation = raw.trim();
        if operation.is_empty() {
            return None;
        }
        let indent = (raw.len() - raw.trim_start_matches(' ').len()).saturating_sub(1);
        let cost = names
            .iter()
            .position(|known| known.starts_with("Cost"))
            .and_then(|place| cells.get(place))
            .and_then(|text| text.split_whitespace().next())
            .and_then(oracle_number);
        Some(Self {
            id,
            indent,
            operation: operation.to_owned(),
            name: cell("Name")
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_owned),
            rows: cell("Rows").and_then(|text| oracle_number(text.trim())),
            cost,
        })
    }

    fn node(&self, parent: Option<usize>) -> PlanNode {
        let reads_table = self.operation.starts_with("TABLE ACCESS");
        PlanNode {
            operation: match &self.name {
                Some(name) => format!("{} {name}", self.operation),
                None => self.operation.clone(),
            },
            relation: self.name.clone().filter(|_| reads_table),
            estimated_rows: self.rows,
            cost: self.cost.map(|total| (0.0, total)),
            parent,
            ..PlanNode::default()
        }
    }
}

fn oracle_number(text: &str) -> Option<f64> {
    let (digits, scale) = match text.char_indices().last()? {
        (at, 'K') => (&text[..at], 1e3),
        (at, 'M') => (&text[..at], 1e6),
        (at, 'G') => (&text[..at], 1e9),
        (at, 'T') => (&text[..at], 1e12),
        (at, 'P') => (&text[..at], 1e15),
        (at, 'E') => (&text[..at], 1e18),
        _ => (text, 1.0),
    };
    number(digits).map(|value| value * scale)
}

fn showplan_physical(body: &str) -> &str {
    body.split('(').next().unwrap_or(body).trim()
}

fn sqlserver_operation(physical: &str, argument: Option<&str>) -> (String, Option<String>) {
    let Some(object) = argument.and_then(showplan_object) else {
        return (physical.to_owned(), None);
    };
    let parts = bracketed_parts(&object);
    let table_only = physical.starts_with("Table ");
    let relation = if table_only || parts.len() < 2 {
        parts.last().cloned()
    } else {
        parts.get(parts.len() - 2).cloned()
    };
    (format!("{physical} {object}"), relation)
}

fn showplan_object(argument: &str) -> Option<String> {
    let start = argument.find(SHOWPLAN_OBJECT)? + SHOWPLAN_OBJECT.len();
    let rest = &argument[start..];
    let mut quoted = false;
    let mut end = rest.len();
    let mut characters = rest.char_indices().peekable();
    while let Some((at, character)) = characters.next() {
        match character {
            '[' if !quoted => quoted = true,
            ']' if quoted => {
                if characters.peek().is_some_and(|&(_, next)| next == ']') {
                    characters.next();
                } else {
                    quoted = false;
                }
            }
            ')' if !quoted => {
                end = at;
                break;
            }
            _ => {}
        }
    }
    let object = &rest[..end];
    let object = object.rfind(" AS [").map_or(object, |at| &object[..at]);
    let object = object.trim();
    (!object.is_empty()).then(|| object.to_owned())
}

fn bracketed_parts(object: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut characters = object.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '[' if !quoted => quoted = true,
            ']' if quoted => {
                if characters.peek() == Some(&']') {
                    characters.next();
                    current.push(']');
                } else {
                    quoted = false;
                }
            }
            '.' if !quoted => parts.push(std::mem::take(&mut current)),
            character => current.push(character),
        }
    }
    parts.push(current);
    parts.retain(|part| !part.is_empty());
    parts
}

fn lift_only_child(nodes: Vec<PlanNode>) -> Vec<PlanNode> {
    nodes
        .into_iter()
        .skip(1)
        .map(|mut node| {
            node.depth -= 1;
            node.parent = match node.parent {
                Some(0) | None => None,
                Some(parent) => Some(parent - 1),
            };
            node.children = node.children.iter().map(|child| child - 1).collect();
            node
        })
        .collect()
}

fn node_from_text(text: &str, parent: Option<usize>, depth: usize) -> PlanNode {
    let split = ["(cost=", "(actual", "(never executed)"]
        .iter()
        .filter_map(|marker| text.find(marker))
        .min()
        .unwrap_or(text.len());
    let operation = text[..split].trim().to_owned();
    let mut node = PlanNode {
        relation: postgresql_relation(&operation),
        operation,
        parent,
        depth,
        ..PlanNode::default()
    };
    let mut rest = &text[split..];
    while let Some(open) = rest.find('(') {
        let after = &rest[open + 1..];
        let Some(close) = after.find(')') else {
            break;
        };
        read_group(&mut node, &after[..close]);
        rest = &after[close + 1..];
    }
    node
}

fn read_group(node: &mut PlanNode, group: &str) {
    if group.trim() == "never executed" {
        node.never_executed = true;
        return;
    }
    let actual = group.trim_start().starts_with("actual");
    for pair in group.split_whitespace() {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        match (actual, key) {
            (false, "cost") => node.cost = range_of(value),
            (false, "rows") => node.estimated_rows = number(value),
            (true, "time") => node.actual_time_ms = range_of(value),
            (true, "rows") => node.actual_rows = number(value),
            (true, "loops") => node.loops = number(value),
            _ => {}
        }
    }
}

fn number(text: &str) -> Option<f64> {
    text.trim()
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite() && *value >= 0.0)
}

fn range_of(text: &str) -> Option<(f64, f64)> {
    let (start, end) = text.split_once("..")?;
    Some((number(start)?, number(end)?))
}

fn buffers_of(body: &str) -> Option<u64> {
    let rest = body.strip_prefix("Buffers:")?;
    let mut pages: u64 = 0;
    for pair in rest.split(|character: char| character.is_whitespace() || character == ',') {
        if let Some((key, value)) = pair.split_once('=')
            && (key == "hit" || key == "read")
        {
            pages = pages.saturating_add(value.parse::<u64>().unwrap_or(0));
        }
    }
    Some(pages)
}

fn unquoted(word: &str) -> Option<String> {
    let word = word.trim_matches(|character| matches!(character, '"' | '`' | '[' | ']' | ','));
    (!word.is_empty()).then(|| word.to_owned())
}

fn postgresql_relation(operation: &str) -> Option<String> {
    let at = operation.find(" on ")?;
    let name = operation[at + 4..].split_whitespace().next()?;
    let name = name.rsplit('.').next().unwrap_or(name);
    unquoted(name)
}

fn sqlite_relation(detail: &str) -> Option<String> {
    let mut words = detail.split_whitespace();
    let verb = words.next()?;
    if !matches!(verb, "SCAN" | "SEARCH") {
        return None;
    }
    let name = match words.next()? {
        "TABLE" => words.next()?,
        name => name,
    };
    unquoted(name)
}

fn rows_count(rows: f64) -> String {
    thousands(rows.round().min(usize::MAX as f64) as usize)
}

fn percent_text(share: f64) -> String {
    format!("{} %", (share * 100.0).round())
}

/// A time as a node's row shows it: `12.4 ms`, `0.04 ms`.
pub fn milliseconds_text(milliseconds: f64) -> String {
    if milliseconds >= 1.0 {
        format!("{milliseconds:.1} ms")
    } else {
        format!("{milliseconds:.2} ms")
    }
}

/// The statements `Explain` runs for one statement in a dialect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExplainStatements {
    /// What runs first, such as `SET SHOWPLAN_ALL ON`.
    pub before: Vec<String>,
    /// What runs, in order.
    pub statements: Vec<String>,
    /// What runs last whatever happened, such as `SET SHOWPLAN_ALL OFF`.
    pub after: Vec<String>,
    /// Which outcome answers with the plan, counting the `before` statements and, when the
    /// run is rolled back, the `BEGIN` or `SAVEPOINT` that opens it.
    pub plan_at: usize,
    /// Which outcome is the statement's own, whose error points into the editor.
    pub statement_at: usize,
    /// How many bytes were put before the statement, to move an error's position back.
    pub prefix_len: usize,
    /// Whether the statements run through [`crate::connection::Connection::run_rolled_back`],
    /// so nothing they write stays.
    pub rolled_back: bool,
}

/// The statements that explain `statement` in `dialect` (`docs/ux/workspace.md` 5.1):
/// `EXPLAIN`, or `EXPLAIN (ANALYZE, BUFFERS)` with `analyze`, which is always rolled back
/// whatever the statement is, since a leading comment, `CREATE TABLE … AS`, `SELECT … INTO`,
/// `EXECUTE` or a function can write; SQLite's `EXPLAIN QUERY PLAN`; Oracle's `EXPLAIN PLAN
/// FOR` then `SELECT * FROM TABLE(DBMS_XPLAN.DISPLAY)`, rolled back so no `PLAN_TABLE` row
/// stays; SQL Server's `SET SHOWPLAN_ALL ON`, or with `analyze` `SET STATISTICS PROFILE ON`
/// around a rolled-back run, turned off again after. A statement that already starts with
/// `EXPLAIN` runs as it is, rolled back too since it may say `ANALYZE` (in Oracle's dialect
/// it is followed by the display). The error is the `Messages` text for what EggDB cannot
/// explain in a dialect yet.
pub fn explain_statements(
    dialect: QueryDialect,
    statement: &str,
    analyze: bool,
) -> Result<ExplainStatements, String> {
    let words: Vec<String> = statement
        .split(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
        .filter(|word| !word.is_empty())
        .take(1)
        .map(str::to_ascii_lowercase)
        .collect();
    let typed = words.first().is_some_and(|word| word == EXPLAIN_WORD);
    match dialect {
        QueryDialect::Oracle => return oracle_explain(statement, typed, analyze),
        QueryDialect::SqlServer => return Ok(sqlserver_explain(statement, analyze)),
        _ => {}
    }
    if typed {
        return Ok(rolled_back(statement.to_owned(), 0));
    }
    if analyze && dialect != QueryDialect::PostgreSql {
        return Err(format!(
            "not_supported: cannot explain (analyze) in {}'s dialect: EggDB measures plans in PostgreSQL's and SQL Server's dialects only for now. Switch the dialect to PostgreSQL to analyze the query; every dialect sees the same tables.",
            dialect.label()
        ));
    }
    let prefix = match (dialect, analyze) {
        (QueryDialect::Sqlite, _) => "EXPLAIN QUERY PLAN ",
        (_, true) => "EXPLAIN (ANALYZE, BUFFERS) ",
        (_, false) => "EXPLAIN ",
    };
    let explained = format!("{prefix}{statement}");
    let prefix_len = prefix.len();
    Ok(if analyze {
        rolled_back(explained, prefix_len)
    } else {
        ExplainStatements {
            before: Vec::new(),
            statements: vec![explained],
            after: Vec::new(),
            plan_at: 0,
            statement_at: 0,
            prefix_len,
            rolled_back: false,
        }
    })
}

fn oracle_explain(
    statement: &str,
    typed: bool,
    analyze: bool,
) -> Result<ExplainStatements, String> {
    if analyze {
        return Err("not_supported: cannot explain (analyze) in Oracle's dialect: Oracle measures a run through DBMS_XPLAN.DISPLAY_CURSOR, which is not in EggDB yet. Explain (Ctrl/Cmd+E) shows the plan; switch the dialect to PostgreSQL to analyze the query; every dialect sees the same tables.".to_owned());
    }
    let (explained, prefix_len) = if typed {
        (statement.to_owned(), 0)
    } else {
        (format!("{ORACLE_EXPLAIN}{statement}"), ORACLE_EXPLAIN.len())
    };
    Ok(ExplainStatements {
        before: Vec::new(),
        statements: vec![explained, ORACLE_DISPLAY.to_owned()],
        after: Vec::new(),
        plan_at: 2,
        statement_at: 1,
        prefix_len,
        rolled_back: true,
    })
}

fn sqlserver_explain(statement: &str, analyze: bool) -> ExplainStatements {
    let option = if analyze {
        "STATISTICS PROFILE"
    } else {
        "SHOWPLAN_ALL"
    };
    let at = 1 + usize::from(analyze);
    ExplainStatements {
        before: vec![format!("SET {option} ON")],
        statements: vec![statement.to_owned()],
        after: vec![format!("SET {option} OFF")],
        plan_at: at,
        statement_at: at,
        prefix_len: 0,
        rolled_back: analyze,
    }
}

fn rolled_back(statement: String, prefix_len: usize) -> ExplainStatements {
    ExplainStatements {
        before: Vec::new(),
        statements: vec![statement],
        after: Vec::new(),
        plan_at: 1,
        statement_at: 1,
        prefix_len,
        rolled_back: true,
    }
}

/// Where a node sits in the drawn tree, from the tree's top-left corner.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NodeBox {
    /// The left edge.
    pub x: f32,
    /// The top edge.
    pub y: f32,
    /// The width; the height is [`NODE_HEIGHT`].
    pub w: f32,
}

/// The drawn tree: each node's box and the tree's size.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PlanLayout {
    /// The nodes' boxes, in the plan's order.
    pub boxes: Vec<NodeBox>,
    /// The tree's width.
    pub width: f32,
    /// The tree's height.
    pub height: f32,
}

/// A node's width for its two rows' widths: the wider plus the padding, from
/// [`NODE_MIN_WIDTH`] to [`NODE_MAX_WIDTH`].
pub fn node_width(first_row: f32, second_row: f32) -> f32 {
    (first_row.max(second_row) + 2.0 * NODE_PADDING).clamp(NODE_MIN_WIDTH, NODE_MAX_WIDTH)
}

/// A text's width at `size` in an interface face whose glyphs advance six tenths of the
/// size: the width a node is laid out with before a real face draws it.
pub fn estimated_width(text: &str, size: f32) -> f32 {
    text.chars().count() as f32 * (size * 0.6).round()
}

/// Lays the plan out top-down (`docs/ux/workspace.md` 5.2): the root at the top centre,
/// children below their parent [`LEVEL_SPACING`] apart, siblings' subtrees
/// [`SIBLING_GAP`] apart, each parent centred over its children.
pub fn layout(plan: &Plan, widths: &[f32]) -> PlanLayout {
    let count = plan.nodes.len();
    let width_of = |index: usize| widths.get(index).copied().unwrap_or(NODE_MIN_WIDTH);
    let mut subtree = vec![0.0_f32; count];
    for index in (0..count).rev() {
        let children = &plan.nodes[index].children;
        let together: f32 = children
            .iter()
            .filter(|&&child| child > index && child < count)
            .map(|&child| subtree[child])
            .sum::<f32>()
            + SIBLING_GAP * children.len().saturating_sub(1) as f32;
        subtree[index] = width_of(index).max(together);
    }
    let mut left = vec![0.0_f32; count];
    let mut boxes = Vec::with_capacity(count);
    let mut deepest = 0;
    for index in 0..count {
        let node = &plan.nodes[index];
        let w = width_of(index);
        deepest = deepest.max(node.depth);
        boxes.push(NodeBox {
            x: left[index] + (subtree[index] - w) / 2.0,
            y: node.depth as f32 * LEVEL_SPACING,
            w,
        });
        let children: Vec<usize> = node
            .children
            .iter()
            .copied()
            .filter(|&child| child > index && child < count)
            .collect();
        let together: f32 = children.iter().map(|&child| subtree[child]).sum::<f32>()
            + SIBLING_GAP * children.len().saturating_sub(1) as f32;
        let mut pen = left[index] + (subtree[index] - together) / 2.0;
        for child in children {
            left[child] = pen;
            pen += subtree[child] + SIBLING_GAP;
        }
    }
    PlanLayout {
        boxes,
        width: subtree.first().copied().unwrap_or(0.0),
        height: if count == 0 {
            0.0
        } else {
            deepest as f32 * LEVEL_SPACING + NODE_HEIGHT
        },
    }
}

/// What a key in the plan did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanAnswer {
    /// The current node moved.
    Moved,
    /// The current node was selected and its range highlighted in the editor.
    Selected,
    /// The selection and the highlight were cleared.
    Cleared,
    /// `Tab` leaves the plan forward.
    LeaveForward,
    /// `Shift+Tab` leaves the plan back.
    LeaveBack,
    /// `Ctrl/Cmd+Shift+Space` showed or hid the current node's tooltip, or `Esc` hid it.
    Tooltip,
    /// The plan does not use the key.
    Ignored,
}

/// What showed a node's tooltip (`docs/ux/workspace.md` 5.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TooltipCause {
    /// The pointer rested on the node for [`TOOLTIP_DELAY`]; it hides when the pointer leaves.
    Pointer,
    /// `Ctrl/Cmd+Shift+Space`; it hides on `Esc`, the same chord or a move.
    Keyboard,
}

/// The `Plan` segment of the results (`docs/ux/workspace.md` 5): the plan, its layout, the
/// current and selected node, the editor range the selection highlights, the scroll offset
/// and whether the text form shows.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanView {
    /// The plan.
    pub plan: Plan,
    /// Its layout.
    pub layout: PlanLayout,
    /// The node with the focus ring.
    pub current: usize,
    /// The selected node.
    pub selected: Option<usize>,
    /// The editor's bytes the selected node is about.
    pub highlight: Option<Range<usize>>,
    /// The explained statement's bytes in the editor's text.
    pub statement: Range<usize>,
    /// The explained statement's text.
    pub statement_text: String,
    /// How far the tree is scrolled right and down.
    pub scroll: (f32, f32),
    /// Whether the text form shows instead of the tree (`Alt+T`).
    pub text_shown: bool,
    /// Whether the plan has the keyboard focus.
    pub focused: bool,
    /// The node whose tooltip shows, and what showed it.
    pub tooltip: Option<(usize, TooltipCause)>,
}

impl PlanView {
    /// The plan of the statement at `statement` in the editor, laid out with
    /// [`estimated_width`], the root current.
    pub fn new(plan: Plan, statement: Range<usize>, statement_text: String) -> Self {
        let widths: Vec<f32> = (0..plan.nodes.len())
            .map(|index| {
                let second: String = plan
                    .second_row(index)
                    .into_iter()
                    .map(|(text, _)| text)
                    .collect();
                node_width(
                    estimated_width(&plan.nodes[index].operation, BODY.0),
                    estimated_width(&second, SMALL.0),
                )
            })
            .collect();
        let layout = layout(&plan, &widths);
        Self {
            plan,
            layout,
            current: 0,
            selected: None,
            highlight: None,
            statement,
            statement_text,
            scroll: (0.0, 0.0),
            text_shown: false,
            focused: false,
            tooltip: None,
        }
    }

    /// Answers a key (`docs/ux/workspace.md` 5.4), keeping the current node in a panel of
    /// `panel` width and height.
    pub fn key(&mut self, press: KeyPress, panel: (f32, f32)) -> PlanAnswer {
        let Some(node) = self.plan.nodes.get(self.current) else {
            return PlanAnswer::Ignored;
        };
        let siblings: &[usize] = match node.parent.and_then(|parent| self.plan.nodes.get(parent)) {
            Some(parent) => &parent.children,
            None => &[],
        };
        let place = siblings.iter().position(|&sibling| sibling == self.current);
        let primary = press.modifiers.control || press.modifiers.command;
        if press.key == Key::Space && primary && press.modifiers.shift {
            self.tooltip = match self.tooltip {
                Some((node, _)) if node == self.current => None,
                _ => Some((self.current, TooltipCause::Keyboard)),
            };
            return PlanAnswer::Tooltip;
        }
        if press.key == Key::Escape && self.tooltip.is_some() {
            self.tooltip = None;
            return PlanAnswer::Tooltip;
        }
        let next = match press.key {
            Key::Up => node.parent,
            Key::Down => node.children.first().copied(),
            Key::Left => place
                .and_then(|place| place.checked_sub(1))
                .and_then(|place| siblings.get(place).copied()),
            Key::Right => place.and_then(|place| siblings.get(place + 1).copied()),
            Key::Home => Some(0),
            Key::Enter | Key::Space => {
                self.selected = Some(self.current);
                self.highlight = Some(self.node_range(self.current));
                return PlanAnswer::Selected;
            }
            Key::Escape if self.selected.is_some() || self.highlight.is_some() => {
                self.selected = None;
                self.highlight = None;
                return PlanAnswer::Cleared;
            }
            Key::Tab if press.modifiers.shift => return PlanAnswer::LeaveBack,
            Key::Tab => return PlanAnswer::LeaveForward,
            _ => return PlanAnswer::Ignored,
        };
        match next.filter(|&next| next < self.plan.nodes.len()) {
            Some(next) => {
                self.current = next;
                if self
                    .tooltip
                    .is_some_and(|(_, cause)| cause == TooltipCause::Keyboard)
                {
                    self.tooltip = None;
                }
                self.scroll_into_view(panel);
                PlanAnswer::Moved
            }
            None => PlanAnswer::Ignored,
        }
    }

    /// The editor's bytes node `index` is about: the first whole-word mention of the table
    /// it reads in the statement, else the whole statement.
    pub fn node_range(&self, index: usize) -> Range<usize> {
        let relation = self
            .plan
            .nodes
            .get(index)
            .and_then(|node| node.relation.as_deref());
        match relation.and_then(|name| find_word(&self.statement_text, name)) {
            Some(at) => {
                let start = self.statement.start + at;
                let length = relation.map_or(0, str::len);
                start..start + length
            }
            None => self.statement.clone(),
        }
    }

    /// Where the tree's top-left corner is drawn in `panel`: centred when it fits, else at
    /// the margin less the scroll.
    pub fn origin(&self, panel: Rect) -> (f32, f32) {
        let x = if self.layout.width + 2.0 * PANEL_MARGIN <= panel.w {
            panel.x + ((panel.w - self.layout.width) / 2.0).floor()
        } else {
            panel.x + PANEL_MARGIN - self.scroll.0
        };
        (x, panel.y + PANEL_MARGIN - self.scroll.1)
    }

    /// Scrolls so the current node is inside a panel of `panel` width and height.
    pub fn scroll_into_view(&mut self, panel: (f32, f32)) {
        let Some(node) = self.layout.boxes.get(self.current) else {
            return;
        };
        let (view_w, view_h) = (
            (panel.0 - 2.0 * PANEL_MARGIN).max(0.0),
            (panel.1 - 2.0 * PANEL_MARGIN).max(0.0),
        );
        let (mut x, mut y) = self.scroll;
        if node.x < x {
            x = node.x;
        } else if node.x + node.w > x + view_w {
            x = node.x + node.w - view_w;
        }
        if node.y < y {
            y = node.y;
        } else if node.y + NODE_HEIGHT > y + view_h {
            y = node.y + NODE_HEIGHT - view_h;
        }
        self.scroll = (x.max(0.0), y.max(0.0));
    }

    /// Shows the tooltip of `node`, the node the pointer has rested on for
    /// [`TOOLTIP_DELAY`], or hides a tooltip the pointer showed when `node` is `None`; a
    /// tooltip the keyboard showed stays. Returns whether the tooltip changed.
    pub fn show_pointer_tooltip(&mut self, node: Option<usize>) -> bool {
        let wanted = match (
            node.filter(|&node| node < self.plan.nodes.len()),
            self.tooltip,
        ) {
            (Some(node), _) => Some((node, TooltipCause::Pointer)),
            (None, Some((_, TooltipCause::Keyboard))) => self.tooltip,
            (None, _) => None,
        };
        let changed = wanted != self.tooltip;
        self.tooltip = wanted;
        changed
    }

    /// The lines of node `index`'s tooltip: its operation and its second row, neither cut,
    /// then the engine's extra lines such as `Filter: (total > 100)`.
    pub fn tooltip_lines(&self, index: usize) -> Vec<String> {
        let Some(node) = self.plan.nodes.get(index) else {
            return Vec::new();
        };
        let second: String = self
            .plan
            .second_row(index)
            .into_iter()
            .map(|(text, _)| text)
            .collect();
        let mut lines = vec![node.operation.clone()];
        if !second.is_empty() {
            lines.push(second);
        }
        lines.extend(node.details.iter().cloned());
        lines
    }

    /// Where node `index`'s tooltip of `size` (width, height) is drawn in `panel`: under the
    /// node, or over it when there is no room below, kept inside the panel.
    pub fn tooltip_rect(&self, index: usize, panel: Rect, size: (f32, f32)) -> Option<Rect> {
        let node = self.node_rect(index, panel)?;
        let (w, h) = (
            size.0.min(TOOLTIP_MAX_WIDTH).min(panel.w),
            size.1.min(panel.h),
        );
        let x = node.x.clamp(panel.x, (panel.x + panel.w - w).max(panel.x));
        let below = node.y + NODE_HEIGHT + TOOLTIP_GAP;
        let y = if below + h <= panel.y + panel.h {
            below
        } else {
            (node.y - TOOLTIP_GAP - h).max(panel.y)
        };
        Some(Rect::new(x, y, w, h))
    }

    /// Node `index`'s rectangle on the window when the tree is drawn in `panel`.
    pub fn node_rect(&self, index: usize, panel: Rect) -> Option<Rect> {
        let node = self.layout.boxes.get(index)?;
        let (x, y) = self.origin(panel);
        Some(Rect::new(x + node.x, y + node.y, node.w, NODE_HEIGHT))
    }
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte >= 0x80
}

fn find_word(haystack: &str, word: &str) -> Option<usize> {
    let (hay, needle) = (haystack.as_bytes(), word.as_bytes());
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).find(|&at| {
        let end = at + needle.len();
        hay[at..end].eq_ignore_ascii_case(needle)
            && (at == 0 || !is_word_byte(hay[at - 1]))
            && (end == hay.len() || !is_word_byte(hay[end]))
    })
}

fn inside(rect: Rect, area: Rect) -> bool {
    rect.x >= area.x
        && rect.y >= area.y
        && rect.x + rect.w <= area.x + area.w
        && rect.y + rect.h <= area.y + area.h
}

fn point_inside(point: (f32, f32), area: Rect) -> bool {
    point.0 >= area.x
        && point.1 >= area.y
        && point.0 <= area.x + area.w
        && point.1 <= area.y + area.h
}

/// Draws the plan's tree in `area` (`docs/ux/workspace.md` 5.2 to 5.4): the links, each
/// node's two rows and share bar, the heat's border, the selection, the `hovered` node's
/// fill, the focus ring and the tooltip. Nodes not wholly inside `area` are left out until
/// scrolled to.
pub(crate) fn draw(
    paint: &mut Painter<'_, '_>,
    view: &PlanView,
    area: Rect,
    hovered: Option<usize>,
) {
    let theme = paint.theme;
    let faces = paint.faces;
    let rects: Vec<Option<Rect>> = (0..view.plan.nodes.len())
        .map(|index| view.node_rect(index, area))
        .collect();
    for (index, node) in view.plan.nodes.iter().enumerate() {
        let Some(Some(parent)) = rects.get(index) else {
            continue;
        };
        let from = (parent.x + (parent.w / 2.0).floor(), parent.y + NODE_HEIGHT);
        for &child in &node.children {
            let Some(Some(child)) = rects.get(child) else {
                continue;
            };
            let to_x = child.x + (child.w / 2.0).floor();
            let points = [
                from,
                (from.0, from.1 + 8.0),
                (to_x, from.1 + 8.0),
                (to_x, child.y),
            ];
            if points.iter().all(|&point| point_inside(point, area)) {
                paint.polyline(&points, 1.0, theme.border_strong);
            }
        }
    }
    for (index, rect) in rects.iter().copied().enumerate() {
        let Some(rect) = rect.filter(|rect| inside(*rect, area)) else {
            continue;
        };
        let (border, border_width) = match view.plan.heat(index) {
            Heat::Plain => (theme.border_control, 1.0),
            Heat::Warm => (theme.warning, 2.0),
            Heat::Hot => (theme.error, 2.0),
        };
        let fill = if view.selected == Some(index) {
            theme.selection
        } else if hovered == Some(index) {
            theme.bg2
        } else {
            theme.bg0
        };
        paint.canvas.fill_rounded_rect(rect, RADIUS, border);
        let inner = Rect::new(
            rect.x + border_width,
            rect.y + border_width,
            rect.w - 2.0 * border_width,
            rect.h - 2.0 * border_width,
        );
        paint
            .canvas
            .fill_rounded_rect(inner, RADIUS - border_width, fill);
        let text_width = rect.w - 2.0 * NODE_PADDING;
        let operation = elided(
            faces.ui,
            &view.plan.nodes[index].operation,
            text_width,
            BODY.0,
        );
        paint.text(
            faces.ui,
            &operation,
            rect.x + NODE_PADDING,
            rect.y + 4.0,
            BODY,
            theme.text_primary,
        );
        let mut pen = rect.x + NODE_PADDING;
        let right = rect.x + rect.w - NODE_PADDING;
        for (text, ink) in view.plan.second_row(index) {
            let color = match ink {
                Ink::Secondary => theme.text_secondary,
                Ink::Warning => theme.warning,
                Ink::Error => theme.error,
            };
            let shown = elided(faces.ui, &text, right - pen, SMALL.0);
            if shown.is_empty() {
                break;
            }
            pen = paint.text(faces.ui, &shown, pen, rect.y + 22.0, SMALL, color);
        }
        paint.rect(
            rect.x + NODE_PADDING,
            rect.y + 34.0,
            text_width,
            4.0,
            theme.bg2,
        );
        if let Some(share) = view.plan.share(index) {
            paint.rect(
                rect.x + NODE_PADDING,
                rect.y + 34.0,
                text_width * share as f32,
                4.0,
                theme.accent,
            );
        }
        if view.focused && view.current == index {
            paint
                .canvas
                .stroke_outside_rounded_rect(rect, RADIUS, 2.0, theme.focus_ring);
        }
    }
    if let Some((index, _)) = view.tooltip {
        draw_tooltip(paint, view, index, area);
    }
}

/// `line` cut into pieces of at most `per_line` characters, at least one character each.
pub fn wrapped(line: &str, per_line: usize) -> Vec<String> {
    let characters: Vec<char> = line.chars().collect();
    if characters.is_empty() {
        return vec![String::new()];
    }
    characters
        .chunks(per_line.max(1))
        .map(|piece| piece.iter().collect())
        .collect()
}

fn draw_tooltip(paint: &mut Painter<'_, '_>, view: &PlanView, index: usize, area: Rect) {
    let theme = paint.theme;
    let faces = paint.faces;
    let advance = faces.mono.width("M", CODE.0).max(1.0);
    let room = TOOLTIP_MAX_WIDTH.min(area.w) - 2.0 * TOOLTIP_PADDING;
    let per_line = (room / advance).floor().max(1.0) as usize;
    let lines: Vec<String> = view
        .tooltip_lines(index)
        .iter()
        .flat_map(|line| wrapped(line, per_line))
        .take(MOST_TOOLTIP_LINES)
        .collect();
    let widest = lines
        .iter()
        .map(|line| faces.mono.width(line, CODE.0))
        .fold(0.0_f32, f32::max);
    let size = (
        widest + 2.0 * TOOLTIP_PADDING,
        lines.len() as f32 * CODE.1 + 2.0 * TOOLTIP_PADDING,
    );
    let Some(rect) = view.tooltip_rect(index, area, size) else {
        return;
    };
    for (spread, alpha) in [(8.0, 10u8), (4.0, 14), (0.0, 16)] {
        let shadow = Rect::new(
            rect.x - spread / 2.0,
            rect.y + 4.0 - spread / 2.0,
            rect.w + spread,
            rect.h + spread,
        );
        let color = crate::color::Rgba {
            r: 0,
            g: 0,
            b: 0,
            a: alpha,
        };
        paint
            .canvas
            .fill_rounded_rect(shadow, 6.0 + spread / 2.0, color);
    }
    paint.canvas.fill_rounded_rect(rect, 6.0, theme.border);
    let inner = Rect::new(rect.x + 1.0, rect.y + 1.0, rect.w - 2.0, rect.h - 2.0);
    paint.canvas.fill_rounded_rect(inner, 5.0, theme.bg0);
    for (row, line) in lines.iter().enumerate() {
        let top = rect.y + TOOLTIP_PADDING + row as f32 * CODE.1;
        if top + CODE.1 > rect.y + rect.h {
            break;
        }
        let shown = elided(faces.mono, line, rect.w - 2.0 * TOOLTIP_PADDING, CODE.0);
        paint.text(
            faces.mono,
            &shown,
            rect.x + TOOLTIP_PADDING,
            top,
            CODE,
            theme.text_primary,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eggdb_platform::window::Modifiers;

    fn lines(text: &str) -> Vec<String> {
        text.lines().map(str::to_owned).collect()
    }

    fn press(key: Key) -> KeyPress {
        KeyPress {
            key,
            modifiers: Modifiers::default(),
        }
    }

    const POSTGRESQL_PLAN: &str = "Hash Join  (cost=1.09..2.30 rows=10 width=72)
  Hash Cond: (o.customer_id = c.id)
  ->  Seq Scan on orders o  (cost=0.00..1.10 rows=10 width=40)
        Filter: (total > 100)
  ->  Hash  (cost=1.04..1.04 rows=4 width=36)
        ->  Seq Scan on customers c  (cost=0.00..1.04 rows=4 width=36)";

    #[test]
    fn postgresql_text_reads_into_a_tree_with_costs_rows_and_details() {
        let plan = Plan::from_postgresql_text(&lines(POSTGRESQL_PLAN)).unwrap();
        assert_eq!(plan.nodes.len(), 4);
        assert!(!plan.analyze);
        let root = &plan.nodes[0];
        assert_eq!(root.operation, "Hash Join");
        assert_eq!(root.cost, Some((1.09, 2.30)));
        assert_eq!(root.children, [1, 2]);
        assert_eq!(root.details, ["Hash Cond: (o.customer_id = c.id)"]);
        assert_eq!(plan.nodes[1].operation, "Seq Scan on orders o");
        assert_eq!(plan.nodes[1].relation.as_deref(), Some("orders"));
        assert_eq!(plan.nodes[1].details, ["Filter: (total > 100)"]);
        assert_eq!(plan.nodes[2].children, [3]);
        assert_eq!((plan.nodes[3].parent, plan.nodes[3].depth), (Some(2), 2));
        assert_eq!(plan.facts(1), "cost 0.00..1.10 · rows 10");
    }

    #[test]
    fn analyze_text_gives_own_times_rows_buffers_and_misestimates() {
        let text = "Nested Loop  (cost=0.00..20.00 rows=10 width=8) (actual time=0.100..10.000 rows=1204 loops=1)
  Buffers: shared hit=30 read=8
  ->  Seq Scan on a  (cost=0.00..10.00 rows=10 width=4) (actual time=0.010..6.200 rows=1204 loops=1)
        Buffers: shared hit=20
  ->  Index Scan using b_pkey on b  (cost=0.00..1.00 rows=1 width=4) (actual time=0.001..0.003 rows=1 loops=1000)
Planning Time: 0.120 ms
Execution Time: 10.400 ms";
        let plan = Plan::from_postgresql_text(&lines(text)).unwrap();
        assert!(plan.analyze);
        assert_eq!(
            (plan.planning_ms, plan.execution_ms),
            (Some(0.12), Some(10.4))
        );
        assert_eq!(plan.nodes[0].buffers, Some(38));
        let index_scan = plan.total_time_ms(2).unwrap();
        assert!((index_scan - 3.0).abs() < 1e-9, "{index_scan}");
        let own_root = plan.own_time_ms(0).unwrap();
        assert!((own_root - 0.8).abs() < 1e-9, "{own_root}");
        assert_eq!(plan.facts(1), "6.2 ms · rows 1,204 · buffers 20");
        assert_eq!(plan.misestimate(1), Some((10.0, 1204.0)));
        let pieces = plan.second_row(1);
        assert_eq!(pieces[1], (" · 62 %".to_owned(), Ink::Error));
        assert_eq!(pieces[2], (" · est. 10 → 1,204".to_owned(), Ink::Warning));
        assert_eq!(plan.heat(2), Heat::Warm);
        assert_eq!(plan.heat(0), Heat::Plain);
        assert!(
            plan.spoken(1).contains("62 percent, hot, misestimated"),
            "{}",
            plan.spoken(1)
        );
    }

    #[test]
    fn the_engines_analyze_form_puts_rows_and_buffers_on_the_root_and_its_time_from_execution() {
        let text = "Seq Scan on t (actual rows=3 loops=1)\n  Buffers: shared hit=3 read=2\nPlanning:\n  Buffers: shared hit=1\nPlanning Time: 0.010 ms\nExecution Time: 0.250 ms";
        let plan = Plan::from_postgresql_text(&lines(text)).unwrap();
        assert_eq!(plan.nodes.len(), 1);
        assert_eq!(plan.nodes[0].operation, "Seq Scan on t");
        assert_eq!(plan.nodes[0].buffers, Some(5));
        assert_eq!(plan.facts(0), "0.25 ms · rows 3 · buffers 5");
        assert_eq!(plan.share(0), Some(1.0));
    }

    #[test]
    fn a_plan_without_costs_has_no_share_and_no_cost_row() {
        let plan =
            Plan::from_postgresql_text(&lines("Index Scan using customers_pkey on customers"))
                .unwrap();
        assert_eq!(plan.share(0), None);
        assert_eq!(plan.heat(0), Heat::Plain);
        assert_eq!(plan.facts(0), "");
        assert!(plan.second_row(0).is_empty());
    }

    #[test]
    fn the_thresholds_are_twenty_and_fifty_percent() {
        assert_eq!(Heat::of(Some(0.1999)), Heat::Plain);
        assert_eq!(Heat::of(Some(0.20)), Heat::Warm);
        assert_eq!(Heat::of(Some(0.25)), Heat::Warm);
        assert_eq!(Heat::of(Some(0.4999)), Heat::Warm);
        assert_eq!(Heat::of(Some(0.50)), Heat::Hot);
        assert_eq!(Heat::of(None), Heat::Plain);
        let text = "Append  (cost=0.00..100.00 rows=4 width=4)
  ->  Seq Scan on a  (cost=0.00..62.00 rows=1 width=4)
  ->  Seq Scan on b  (cost=0.00..20.00 rows=1 width=4)";
        let plan = Plan::from_postgresql_text(&lines(text)).unwrap();
        assert_eq!(
            (plan.heat(0), plan.heat(1), plan.heat(2)),
            (Heat::Plain, Heat::Hot, Heat::Warm)
        );
        assert_eq!(plan.second_row(2)[1], (" · 20 %".to_owned(), Ink::Warning));
    }

    #[test]
    fn malformed_text_never_panics_and_huge_plans_are_refused() {
        for text in [
            "",
            "->",
            "  ->  ",
            "Sort (cost=",
            "Sort (cost=1..x rows=-3) (actual time=..)",
            "Sort (cost=NaN..inf rows=1e400)",
            "x\n                                    ->  y\n  ->  z\n      ->  é (cost=1..2)",
            "Buffers: shared hit=99999999999999999999999",
            "Limit\n Buffers: shared hit=1 read=x\n->  oops",
            "Ünïcode (actual rows=1 loops=0)\n  ->  ✓ on \"\" (cost=0..0)",
        ] {
            let _ = Plan::from_postgresql_text(&lines(text));
        }
        assert_eq!(Plan::from_postgresql_text(&[]), Err(PlanError::Empty));
        let deep: Vec<String> = std::iter::once("Root".to_owned())
            .chain(
                (1..=MAX_DEPTH + 1)
                    .map(|level| format!("{}->  Node", " ".repeat(2 + 6 * (level - 1)))),
            )
            .collect();
        assert_eq!(Plan::from_postgresql_text(&deep), Err(PlanError::TooDeep));
        let wide: Vec<String> = std::iter::once("Append".to_owned())
            .chain((0..MAX_NODES).map(|_| "  ->  Seq Scan on t".to_owned()))
            .collect();
        assert_eq!(
            Plan::from_postgresql_text(&wide),
            Err(PlanError::TooManyNodes)
        );
        let long = vec!["x".to_owned(); MAX_LINES + 1];
        assert_eq!(
            Plan::from_postgresql_text(&long),
            Err(PlanError::TooManyLines)
        );
        let broken = Plan::from_postgresql_text(&lines("Sort (cost=1..x rows=-3)")).unwrap();
        assert_eq!(
            (broken.nodes[0].cost, broken.nodes[0].estimated_rows),
            (None, None)
        );
    }

    #[test]
    fn sqlite_rows_become_a_tree_by_parent() {
        let row = |id: &str, parent: &str, detail: &str| {
            vec![
                Some(id.to_owned()),
                Some(parent.to_owned()),
                Some("0".to_owned()),
                Some(detail.to_owned()),
            ]
        };
        let rows = vec![
            row("2", "0", "SCAN orders"),
            row(
                "5",
                "0",
                "SEARCH customers USING INTEGER PRIMARY KEY (rowid=?)",
            ),
            row("9", "5", "USE TEMP B-TREE FOR ORDER BY"),
        ];
        let plan = Plan::from_sqlite_rows(&rows, (0, 1, 3)).unwrap();
        assert_eq!(plan.nodes[0].operation, "QUERY PLAN");
        assert_eq!(plan.nodes[0].children, [1, 2]);
        assert_eq!(plan.nodes[2].relation.as_deref(), Some("customers"));
        assert_eq!(plan.nodes[3].parent, Some(2));
        let single = Plan::from_sqlite_rows(&[row("2", "0", "SCAN t")], (0, 1, 3)).unwrap();
        assert_eq!(single.nodes.len(), 1);
        assert_eq!((single.nodes[0].parent, single.nodes[0].depth), (None, 0));
        assert_eq!(
            Plan::from_sqlite_rows(&[], (0, 1, 3)),
            Err(PlanError::Empty)
        );
        let _ = Plan::from_sqlite_rows(&[vec![None]], (0, 1, 3));
        let flat = |count: usize| -> Vec<Vec<Option<String>>> {
            (1..=count)
                .map(|id| row(&id.to_string(), "0", "SCAN t"))
                .collect()
        };
        let largest = Plan::from_sqlite_rows(&flat(MAX_NODES - 1), (0, 1, 3)).unwrap();
        assert_eq!(largest.nodes.len(), MAX_NODES, "the root counts");
        assert_eq!(
            Plan::from_sqlite_rows(&flat(MAX_NODES), (0, 1, 3)),
            Err(PlanError::TooManyNodes)
        );
    }

    fn table(columns: &[&str], rows: Vec<Vec<Option<&str>>>) -> ResultTable {
        ResultTable {
            columns: columns.iter().map(|column| (*column).to_owned()).collect(),
            kinds: vec![crate::grid::ValueKind::Text; columns.len()],
            row_count: rows.len(),
            rows: rows
                .into_iter()
                .map(|row| {
                    row.into_iter()
                        .map(|cell| cell.map(str::to_owned))
                        .collect()
                })
                .collect(),
            tag: "SELECT".to_owned(),
        }
    }

    #[test]
    fn oracle_display_lines_become_a_tree_by_indent_with_rows_costs_and_predicates() {
        let display = lines(
            "Plan hash value: 3956160932

---------------------------------------------------------------------------------------
| Id  | Operation                    | Name        | Rows  | Bytes | Cost (%CPU)| Time     |
---------------------------------------------------------------------------------------
|   0 | SELECT STATEMENT             |             |    14K|   532 |     4   (0)| 00:00:01 |
|   1 |  NESTED LOOPS                |             |    14 |   532 |     4   (0)| 00:00:01 |
|   2 |   TABLE ACCESS FULL          | DEPT        |     4 |    52 |     3   (0)| 00:00:01 |
|*  3 |   TABLE ACCESS BY INDEX ROWID| EMP         |     4 |    52 |     1   (0)| 00:00:01 |
|*  4 |    INDEX RANGE SCAN          | EMP_DEPT_IX |     4 |       |     0   (0)| 00:00:01 |
---------------------------------------------------------------------------------------

Predicate Information (identified by operation id):
---------------------------------------------------

   3 - filter(\"E\".\"SAL\">100)
   4 - access(\"E\".\"DEPTNO\"=\"D\".\"DEPTNO\")

Note
-----
   - EggDB gives no cost, row or time estimates in a plan",
        );
        let plan = Plan::from_result(&table(
            &["PLAN_TABLE_OUTPUT"],
            display
                .iter()
                .map(|line| vec![Some(line.as_str())])
                .collect(),
        ))
        .unwrap();
        assert_eq!(plan.nodes.len(), 5);
        assert_eq!(plan.nodes[0].operation, "SELECT STATEMENT");
        assert_eq!(plan.nodes[0].estimated_rows, Some(14_000.0));
        assert_eq!(plan.nodes[1].children, [2, 3]);
        assert_eq!(plan.nodes[2].operation, "TABLE ACCESS FULL DEPT");
        assert_eq!(plan.nodes[2].relation.as_deref(), Some("DEPT"));
        assert_eq!((plan.nodes[4].parent, plan.nodes[4].depth), (Some(3), 3));
        assert_eq!(plan.nodes[4].relation, None);
        assert_eq!(plan.nodes[3].details, ["filter(\"E\".\"SAL\">100)"]);
        assert_eq!(plan.nodes[0].cost, Some((0.0, 4.0)));
        assert_eq!(plan.own_cost(3), Some(1.0));
        assert_eq!(plan.text, display);
        assert!(!plan.analyze);
        let missing = lines("Error: cannot fetch last explain plan from PLAN_TABLE");
        assert_eq!(Plan::from_xplan_lines(&missing), Err(PlanError::Empty));
        for hostile in ["|", "||||", "| Id | Operation |", "|*x| y |", "   - - -"] {
            let _ =
                Plan::from_xplan_lines(&lines(&format!("| Id | Operation | Name |\n{hostile}")));
        }
    }

    #[test]
    fn showplan_all_rows_hang_under_their_parent_node_with_the_statement_as_root() {
        let columns = [
            "Rows",
            "Executes",
            "StmtText",
            "StmtId",
            "NodeId",
            "Parent",
            "PhysicalOp",
            "LogicalOp",
            "Argument",
            "EstimateRows",
            "TotalSubtreeCost",
            "Type",
        ];
        let row = |rows: Option<&'static str>,
                   text: &'static str,
                   node: &'static str,
                   parent: &'static str,
                   physical: Option<&'static str>,
                   logical: Option<&'static str>,
                   argument: Option<&'static str>,
                   kind: &'static str| {
            vec![
                rows,
                rows.map(|_| "1"),
                Some(text),
                Some("1"),
                Some(node),
                Some(parent),
                physical,
                logical,
                argument,
                None,
                None,
                Some(kind),
            ]
        };
        let profiled = table(
            &columns,
            vec![
                row(
                    Some("1"),
                    "SELECT * FROM t WHERE id = 1",
                    "1",
                    "0",
                    None,
                    None,
                    None,
                    "SELECT",
                ),
                row(
                    Some("1"),
                    "  |--Nested Loops(Inner Join)",
                    "2",
                    "1",
                    Some("Nested Loops"),
                    Some("Inner Join"),
                    None,
                    "PLAN_ROW",
                ),
                row(
                    None,
                    "       |--Clustered Index Seek(OBJECT:([t].[t_pkey] AS [t]), ORDERED FORWARD)",
                    "3",
                    "2",
                    Some("Clustered Index Seek"),
                    Some("Clustered Index Seek"),
                    Some("OBJECT:([t].[t_pkey] AS [t]), ORDERED FORWARD"),
                    "PLAN_ROW",
                ),
                row(
                    None,
                    "       |--Table Scan(OBJECT:([odd]]name] AS [o]))",
                    "4",
                    "2",
                    Some("Table Scan"),
                    Some("Table Scan"),
                    Some("OBJECT:([odd]]name] AS [o])"),
                    "PLAN_ROW",
                ),
            ],
        );
        let plan = Plan::from_result(&profiled).unwrap();
        assert!(plan.analyze);
        assert_eq!(plan.nodes[0].operation, "SELECT");
        assert_eq!(plan.nodes[0].details, ["SELECT * FROM t WHERE id = 1"]);
        assert_eq!(plan.nodes[1].details, ["Inner Join"]);
        assert_eq!(plan.nodes[1].children, [2, 3]);
        assert_eq!(plan.nodes[2].operation, "Clustered Index Seek [t].[t_pkey]");
        assert_eq!(plan.nodes[2].relation.as_deref(), Some("t"));
        assert_eq!(plan.nodes[3].relation.as_deref(), Some("odd]name"));
        assert_eq!(plan.actual_rows_total(0), Some(1.0));
        assert_eq!(plan.actual_rows_total(2), None);
        assert_eq!(plan.facts(0), "rows 1");
        assert_eq!(plan.text[1], "  |--Nested Loops(Inner Join)");
        let mut orphan = profiled.clone();
        orphan.rows[2][5] = Some("99".to_owned());
        assert_eq!(Plan::from_result(&orphan).unwrap().nodes[2].parent, Some(0));
    }

    #[test]
    fn showplan_text_reads_the_tree_by_its_branch_columns() {
        let text = lines(
            "SELECT * FROM t JOIN u ON u.id = t.u
  |--Hash Match(Inner Join)
       |--Clustered Index Scan(OBJECT:([t].[t_pkey] AS [t]))
       |--Clustered Index Scan(OBJECT:([u].[u_pkey] AS [u]))",
        );
        let plan = Plan::from_result(&table(
            &["StmtText"],
            text.iter().map(|line| vec![Some(line.as_str())]).collect(),
        ))
        .unwrap();
        assert_eq!(plan.nodes.len(), 3);
        assert_eq!(plan.nodes[0].operation, "Hash Match");
        assert_eq!(plan.nodes[0].children, [1, 2]);
        assert_eq!(plan.nodes[2].relation.as_deref(), Some("u"));
        assert_eq!(
            plan.nodes[1].details,
            ["Clustered Index Scan(OBJECT:([t].[t_pkey] AS [t]))"]
        );
        assert_eq!(
            Plan::from_showplan_text(&lines("SELECT 1")),
            Err(PlanError::Empty)
        );
        let deep: Vec<String> = (0..80)
            .map(|level| format!("  {}|--Sort", "     ".repeat(level)))
            .collect();
        assert_eq!(Plan::from_showplan_text(&deep), Err(PlanError::TooDeep));
    }

    #[test]
    fn mysql_rows_join_by_nested_loops() {
        let rows = vec![
            vec![
                Some("o".to_owned()),
                Some("ALL".to_owned()),
                None,
                Some("10".to_owned()),
            ],
            vec![
                Some("c".to_owned()),
                Some("eq_ref".to_owned()),
                Some("PRIMARY".to_owned()),
                Some("1".to_owned()),
            ],
        ];
        let plan = Plan::from_mysql_rows(&rows, (0, 1, Some(2), Some(3))).unwrap();
        assert_eq!(plan.nodes[0].operation, "Nested Loop");
        assert_eq!(plan.nodes[2].operation, "eq_ref on c using PRIMARY");
        assert_eq!(plan.facts(1), "rows 10");
    }

    #[test]
    fn layout_centres_parents_over_their_children_with_the_specs_gaps() {
        let plan = Plan::from_postgresql_text(&lines(POSTGRESQL_PLAN)).unwrap();
        let layout = layout(&plan, &[200.0, 150.0, 120.0, 260.0]);
        let boxes = &layout.boxes;
        assert_eq!(boxes[1].y, LEVEL_SPACING);
        assert_eq!(boxes[3].y, 2.0 * LEVEL_SPACING);
        assert_eq!(layout.width, 150.0 + SIBLING_GAP + 260.0);
        assert_eq!(layout.height, 2.0 * LEVEL_SPACING + NODE_HEIGHT);
        assert_eq!(
            boxes[2].x - (boxes[1].x + boxes[1].w),
            SIBLING_GAP + (260.0 - 120.0) / 2.0
        );
        let centre = |b: &NodeBox| b.x + b.w / 2.0;
        assert_eq!(centre(&boxes[0]), layout.width / 2.0);
        assert_eq!(centre(&boxes[2]), centre(&boxes[3]));
        assert_eq!(node_width(10.0, 0.0), NODE_MIN_WIDTH);
        assert_eq!(node_width(1000.0, 0.0), NODE_MAX_WIDTH);
        assert_eq!(node_width(200.0, 210.0), 234.0);
    }

    #[test]
    fn keys_walk_the_tree_select_a_node_and_highlight_its_table() {
        let plan = Plan::from_postgresql_text(&lines(POSTGRESQL_PLAN)).unwrap();
        let statement =
            "SELECT * FROM orders o JOIN customers c ON o.customer_id = c.id WHERE total > 100";
        let mut view = PlanView::new(plan, 10..10 + statement.len(), statement.to_owned());
        let panel = (1020.0, 358.0);
        assert_eq!(view.key(press(Key::Up), panel), PlanAnswer::Ignored);
        assert_eq!(view.key(press(Key::Down), panel), PlanAnswer::Moved);
        assert_eq!(view.current, 1);
        assert_eq!(view.key(press(Key::Left), panel), PlanAnswer::Ignored);
        assert_eq!(view.key(press(Key::Right), panel), PlanAnswer::Moved);
        assert_eq!(view.current, 2);
        assert_eq!(view.key(press(Key::Down), panel), PlanAnswer::Moved);
        assert_eq!(view.key(press(Key::Enter), panel), PlanAnswer::Selected);
        let at = statement.find("customers").unwrap();
        assert_eq!(view.highlight, Some(10 + at..10 + at + "customers".len()));
        assert_eq!(view.key(press(Key::Up), panel), PlanAnswer::Moved);
        assert_eq!(view.key(press(Key::Enter), panel), PlanAnswer::Selected);
        assert_eq!(
            view.highlight,
            Some(10..10 + statement.len()),
            "Hash reads no table"
        );
        assert_eq!(view.key(press(Key::Escape), panel), PlanAnswer::Cleared);
        assert_eq!((view.selected, view.highlight.clone()), (None, None));
        assert_eq!(view.key(press(Key::Home), panel), PlanAnswer::Moved);
        assert_eq!(view.current, 0);
        assert_eq!(view.key(press(Key::Tab), panel), PlanAnswer::LeaveForward);
    }

    #[test]
    fn a_wide_tree_scrolls_to_the_current_node() {
        let text: String = std::iter::once("Append  (cost=0.00..10.00 rows=1 width=4)".to_owned())
            .chain((0..12).map(|n| {
                format!("  ->  Seq Scan on table_number_{n}  (cost=0.00..0.50 rows=1 width=4)")
            }))
            .collect::<Vec<_>>()
            .join("\n");
        let plan = Plan::from_postgresql_text(&lines(&text)).unwrap();
        let mut view = PlanView::new(plan, 0..0, String::new());
        let panel = (1020.0, 358.0);
        assert!(view.layout.width > panel.0);
        view.key(press(Key::Down), panel);
        for _ in 0..11 {
            view.key(press(Key::Right), panel);
        }
        let last = view.layout.boxes[12];
        assert!(view.scroll.0 > 0.0);
        assert!(last.x + last.w - view.scroll.0 <= panel.0 - 2.0 * PANEL_MARGIN + 0.01);
    }

    #[test]
    fn explain_statements_follow_the_dialect() {
        let plain = explain_statements(QueryDialect::PostgreSql, "SELECT 1", false).unwrap();
        assert_eq!(
            (plain.statements, plain.prefix_len),
            (vec!["EXPLAIN SELECT 1".to_owned()], 8)
        );
        let analyzed =
            explain_statements(QueryDialect::PostgreSql, "UPDATE t SET a = 1", true).unwrap();
        assert_eq!(
            analyzed.statements,
            ["EXPLAIN (ANALYZE, BUFFERS) UPDATE t SET a = 1"]
        );
        assert!(analyzed.rolled_back);
        assert_eq!(analyzed.plan_at, 1);
        for hidden_write in [
            "/* audit */ DELETE FROM t",
            "CREATE TABLE copy AS SELECT * FROM t",
            "SELECT * INTO copy FROM t",
            "select 1",
        ] {
            let read = explain_statements(QueryDialect::PostgreSql, hidden_write, true).unwrap();
            assert!(read.rolled_back, "{hidden_write}");
        }
        let typed = explain_statements(
            QueryDialect::PostgreSql,
            "EXPLAIN ANALYZE DELETE FROM t",
            false,
        )
        .unwrap();
        assert!(typed.rolled_back);
        let sqlite = explain_statements(QueryDialect::Sqlite, "SELECT 1", false).unwrap();
        assert_eq!(sqlite.statements, ["EXPLAIN QUERY PLAN SELECT 1"]);
        let typed = explain_statements(QueryDialect::MySql, "explain select 1", false).unwrap();
        assert_eq!(typed.plan_at, 1);
        assert_eq!(
            (typed.statements, typed.prefix_len),
            (vec!["explain select 1".to_owned()], 0)
        );
        let oracle = explain_statements(QueryDialect::Oracle, "SELECT 1 FROM dual", false).unwrap();
        assert_eq!(
            oracle.statements,
            [
                "EXPLAIN PLAN FOR SELECT 1 FROM dual",
                "SELECT * FROM TABLE(DBMS_XPLAN.DISPLAY)"
            ]
        );
        assert_eq!(
            (oracle.plan_at, oracle.statement_at, oracle.prefix_len),
            (2, 1, 17)
        );
        assert!(oracle.rolled_back && oracle.before.is_empty() && oracle.after.is_empty());
        let typed = explain_statements(
            QueryDialect::Oracle,
            "explain plan for select 1 from dual",
            false,
        )
        .unwrap();
        assert_eq!(typed.statements[0], "explain plan for select 1 from dual");
        assert_eq!(typed.prefix_len, 0);
        let analyzed = explain_statements(QueryDialect::Oracle, "SELECT 1 FROM dual", true);
        assert!(analyzed.is_err_and(|message| message.contains("DBMS_XPLAN.DISPLAY_CURSOR")));
        let shown = explain_statements(QueryDialect::SqlServer, "SELECT 1", false).unwrap();
        assert_eq!(
            (shown.before, shown.statements, shown.after),
            (
                vec!["SET SHOWPLAN_ALL ON".to_owned()],
                vec!["SELECT 1".to_owned()],
                vec!["SET SHOWPLAN_ALL OFF".to_owned()]
            )
        );
        assert_eq!(
            (shown.plan_at, shown.statement_at, shown.rolled_back),
            (1, 1, false)
        );
        let profiled = explain_statements(QueryDialect::SqlServer, "DELETE FROM t", true).unwrap();
        assert_eq!(profiled.before, ["SET STATISTICS PROFILE ON"]);
        assert_eq!(profiled.after, ["SET STATISTICS PROFILE OFF"]);
        assert_eq!((profiled.plan_at, profiled.rolled_back), (2, true));
        assert!(explain_statements(QueryDialect::MySql, "SELECT 1", true).is_err());
    }

    #[test]
    fn find_word_matches_whole_words_only() {
        assert_eq!(
            find_word("SELECT * FROM orders_old, Orders", "orders"),
            Some(26)
        );
        assert_eq!(find_word("é orders", "orders"), Some(3));
        assert_eq!(find_word("x", "orders"), None);
        assert_eq!(find_word("abc", ""), None);
    }
}
