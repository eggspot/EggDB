use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use eggdb_platform::window::{Key, KeyPress};

use crate::canvas::Rect;
use crate::catalog::Catalog;
use crate::main_window::{
    BODY, Button, Painter, SMALL, chevron, focus_ring, icon_database, icon_folder, icon_table,
};

/// How long after a letter the next one still extends the type-ahead prefix.
pub const TYPE_AHEAD_WINDOW: Duration = Duration::from_secs(1);
/// The top of the tree's first row, under the filter field.
pub const ROWS_TOP: f32 = 88.0;
/// The height of a tree row.
pub const ROW_HEIGHT: f32 = 24.0;
/// How many rows the tree shows at once at 800 px.
pub const ROWS_IN_VIEW: usize = 28;
/// The sidebar's width.
pub const SIDEBAR_WIDTH: f32 = 260.0;

const FILTER_LIMIT: usize = 256;
const TYPE_AHEAD_LIMIT: usize = 64;

/// A model group of the tree's level 1, in the tree's order (`docs/ux/workspace.md` 4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Group {
    /// The tables.
    Tables,
    /// The views.
    Views,
    /// Every table's secondary indexes.
    Indexes,
    /// The sequences.
    Sequences,
    /// The key-value key spaces.
    Keys,
}

impl Group {
    /// Every group, in the tree's order.
    pub const ALL: [Self; 5] = [
        Self::Tables,
        Self::Views,
        Self::Indexes,
        Self::Sequences,
        Self::Keys,
    ];

    /// The group's label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Tables => "Tables",
            Self::Views => "Views",
            Self::Indexes => "Indexes",
            Self::Sequences => "Sequences",
            Self::Keys => "Keys",
        }
    }
}

/// One object of the tree; the numbers index the [`Catalog`]'s lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TreeItem {
    /// The connection, level 0.
    Connection,
    /// A model group, level 1.
    Group(Group),
    /// A table.
    Table(usize),
    /// A table's `Columns` group.
    Columns(usize),
    /// A table's `Indexes` group.
    TableIndexes(usize),
    /// A column of a table.
    Column {
        /// The table.
        table: usize,
        /// The column, in the table's order.
        column: usize,
    },
    /// An index in its table's `Indexes` group.
    TableIndex(usize),
    /// A view.
    View(usize),
    /// An index in the `Indexes` group.
    Index(usize),
    /// A sequence.
    Sequence(usize),
    /// A key space.
    KeySpace(usize),
    /// A table's `Triggers` group, listed when the table has triggers.
    TableTriggers(usize),
    /// A trigger in its table's `Triggers` group.
    Trigger(usize),
}

/// The 12 × 12 icon a row draws before its label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowIcon {
    /// The connection's stacked disks.
    Database,
    /// A folder, for groups.
    Folder,
    /// A key, for the `Keys` group.
    Key,
    /// A grid, for tables and views.
    Table,
    /// A bolt, for triggers.
    Bolt,
    /// No icon.
    Blank,
}

/// One visible row of the tree, as it is drawn and read by a screen reader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeRow {
    /// The object.
    pub item: TreeItem,
    /// Its level, 0 for the connection.
    pub level: usize,
    /// Its label.
    pub label: String,
    /// The right text: a count, `pk`, `unique`, `2 of 12` while filtering.
    pub detail: Option<String>,
    /// `Some(true)` expanded, `Some(false)` collapsed, `None` for a leaf.
    pub expanded: Option<bool>,
    /// Its position among its siblings, from 1.
    pub position: usize,
    /// How many siblings it has, itself included.
    pub set_size: usize,
    /// Its icon.
    pub icon: RowIcon,
}

/// What a key in the tree or its filter asks the main window to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeAction {
    /// Nothing beyond redrawing.
    None,
    /// Open a table or view in a new query tab and run it.
    Open(TreeItem),
    /// Insert the object's name at the editor's caret.
    InsertName(TreeItem),
    /// Show the object in the inspector.
    Inspect(TreeItem),
    /// Read the catalog again.
    Refresh,
    /// Move the focus to the next region.
    LeaveForward,
    /// Move the focus to the previous region.
    LeaveBack,
}

/// The object browser's tree (`docs/ux/workspace.md` 4.1, 4.2): the catalog it lists, which
/// rows are expanded, the current and the selected row, and the filter. Children are listed
/// only when their parent is expanded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectTree {
    /// The connection's name, the label of level 0.
    pub connection: String,
    /// The objects it lists.
    pub catalog: Catalog,
    /// The expanded rows.
    pub expanded: BTreeSet<TreeItem>,
    /// The row with the ring.
    pub current: TreeItem,
    /// The row the inspector shows.
    pub selected: Option<TreeItem>,
    /// The filter field's text.
    pub filter: String,
    /// The first row drawn.
    pub first_row: usize,
    typed: String,
    typed_at: Option<Instant>,
}

impl ObjectTree {
    /// The tree of `catalog` as it first shows: the connection and `Tables` expanded.
    pub fn new(connection: &str, catalog: Catalog) -> Self {
        Self {
            connection: connection.to_owned(),
            catalog,
            expanded: [TreeItem::Connection, TreeItem::Group(Group::Tables)]
                .into_iter()
                .collect(),
            current: TreeItem::Connection,
            selected: None,
            filter: String::new(),
            first_row: 0,
            typed: String::new(),
            typed_at: None,
        }
    }

    /// Replaces the catalog after a refresh, keeping the expanded, current and selected rows
    /// that still exist by name.
    pub fn set_catalog(&mut self, catalog: Catalog) {
        let old = std::mem::replace(&mut self.catalog, catalog);
        let resolve = |tree: &Self, item: TreeItem| tree.resolve(&old, item);
        self.expanded = self
            .expanded
            .iter()
            .filter_map(|&item| resolve(self, item))
            .collect();
        self.current = resolve(self, self.current).unwrap_or(TreeItem::Connection);
        self.selected = self.selected.and_then(|item| resolve(self, item));
        self.scroll_into_view();
    }

    fn resolve(&self, old: &Catalog, item: TreeItem) -> Option<TreeItem> {
        let table = |index: usize| {
            let name = &old.tables.get(index)?.name;
            self.catalog
                .tables
                .iter()
                .position(|table| &table.name == name)
        };
        let index_named = |index: usize| {
            let name = &old.indexes.get(index)?.name;
            self.catalog
                .indexes
                .iter()
                .position(|found| &found.name == name)
        };
        Some(match item {
            TreeItem::Connection | TreeItem::Group(_) => item,
            TreeItem::Table(index) => TreeItem::Table(table(index)?),
            TreeItem::Columns(index) => TreeItem::Columns(table(index)?),
            TreeItem::TableIndexes(index) => TreeItem::TableIndexes(table(index)?),
            TreeItem::TableTriggers(index) => TreeItem::TableTriggers(table(index)?),
            TreeItem::Trigger(index) => {
                let old = old.triggers.get(index)?;
                TreeItem::Trigger(self.catalog.triggers.iter().position(|found| {
                    found.name == old.name && found.table == old.table
                })?)
            }
            TreeItem::Column {
                table: owner,
                column,
            } => {
                let name = &old.tables.get(owner)?.columns.get(column)?.name;
                let owner = table(owner)?;
                let column = self
                    .catalog
                    .tables
                    .get(owner)?
                    .columns
                    .iter()
                    .position(|found| &found.name == name)?;
                TreeItem::Column {
                    table: owner,
                    column,
                }
            }
            TreeItem::TableIndex(index) => TreeItem::TableIndex(index_named(index)?),
            TreeItem::Index(index) => TreeItem::Index(index_named(index)?),
            TreeItem::View(index) => {
                let name = &old.views.get(index)?.name;
                TreeItem::View(
                    self.catalog
                        .views
                        .iter()
                        .position(|view| &view.name == name)?,
                )
            }
            TreeItem::Sequence(index) => {
                let name = &old.sequences.get(index)?.name;
                TreeItem::Sequence(
                    self.catalog
                        .sequences
                        .iter()
                        .position(|found| &found.name == name)?,
                )
            }
            TreeItem::KeySpace(index) => {
                let name = old.key_spaces.get(index)?.label();
                TreeItem::KeySpace(
                    self.catalog
                        .key_spaces
                        .iter()
                        .position(|space| space.label() == name)?,
                )
            }
        })
    }

    /// The groups the connection has, in the tree's order; `Tables` always.
    pub fn groups(&self) -> Vec<Group> {
        Group::ALL
            .into_iter()
            .filter(|&group| group == Group::Tables || self.group_size(group) > 0)
            .collect()
    }

    fn group_size(&self, group: Group) -> usize {
        match group {
            Group::Tables => self.catalog.tables.len(),
            Group::Views => self.catalog.views.len(),
            Group::Indexes => self.catalog.indexes.len(),
            Group::Sequences => self.catalog.sequences.len(),
            Group::Keys => self.catalog.key_spaces.len(),
        }
    }

    fn table_name(&self, table: usize) -> &str {
        self.catalog
            .tables
            .get(table)
            .map_or("", |table| table.name.as_str())
    }

    fn child_count(&self, item: TreeItem) -> usize {
        match item {
            TreeItem::Connection => self.groups().len(),
            TreeItem::Group(group) => self.group_size(group),
            TreeItem::Table(table) => {
                let name = self.table_name(table);
                1 + usize::from(self.catalog.indexes_of(name).next().is_some())
                    + usize::from(self.catalog.triggers_of(name).next().is_some())
            }
            TreeItem::Columns(table) => self
                .catalog
                .tables
                .get(table)
                .map_or(0, |table| table.columns.len()),
            TreeItem::TableIndexes(table) => {
                self.catalog.indexes_of(self.table_name(table)).count()
            }
            TreeItem::TableTriggers(table) => {
                self.catalog.triggers_of(self.table_name(table)).count()
            }
            _ => 0,
        }
    }

    /// The children of `item`, listed when it expands.
    pub fn children(&self, item: TreeItem) -> Vec<TreeItem> {
        match item {
            TreeItem::Connection => self.groups().into_iter().map(TreeItem::Group).collect(),
            TreeItem::Group(Group::Tables) => (0..self.catalog.tables.len())
                .map(TreeItem::Table)
                .collect(),
            TreeItem::Group(Group::Views) => {
                (0..self.catalog.views.len()).map(TreeItem::View).collect()
            }
            TreeItem::Group(Group::Indexes) => (0..self.catalog.indexes.len())
                .map(TreeItem::Index)
                .collect(),
            TreeItem::Group(Group::Sequences) => (0..self.catalog.sequences.len())
                .map(TreeItem::Sequence)
                .collect(),
            TreeItem::Group(Group::Keys) => (0..self.catalog.key_spaces.len())
                .map(TreeItem::KeySpace)
                .collect(),
            TreeItem::Table(table) => {
                let mut children = vec![TreeItem::Columns(table)];
                if self
                    .catalog
                    .indexes_of(self.table_name(table))
                    .next()
                    .is_some()
                {
                    children.push(TreeItem::TableIndexes(table));
                }
                if self
                    .catalog
                    .triggers_of(self.table_name(table))
                    .next()
                    .is_some()
                {
                    children.push(TreeItem::TableTriggers(table));
                }
                children
            }
            TreeItem::Columns(table) => (0..self.child_count(item))
                .map(|column| TreeItem::Column { table, column })
                .collect(),
            TreeItem::TableIndexes(table) => {
                let name = self.table_name(table);
                self.catalog
                    .indexes
                    .iter()
                    .enumerate()
                    .filter(|(_, index)| index.table == name)
                    .map(|(at, _)| TreeItem::TableIndex(at))
                    .collect()
            }
            TreeItem::TableTriggers(table) => {
                let name = self.table_name(table);
                self.catalog
                    .triggers
                    .iter()
                    .enumerate()
                    .filter(|(_, trigger)| trigger.table == name)
                    .map(|(at, _)| TreeItem::Trigger(at))
                    .collect()
            }
            _ => Vec::new(),
        }
    }

    /// The row's label.
    pub fn label(&self, item: TreeItem) -> String {
        let catalog = &self.catalog;
        match item {
            TreeItem::Connection => self.connection.clone(),
            TreeItem::Group(group) => group.label().to_owned(),
            TreeItem::Table(table) => self.table_name(table).to_owned(),
            TreeItem::Columns(_) => "Columns".to_owned(),
            TreeItem::TableIndexes(_) => "Indexes".to_owned(),
            TreeItem::TableTriggers(_) => "Triggers".to_owned(),
            TreeItem::Trigger(trigger) => catalog
                .triggers
                .get(trigger)
                .map_or_else(String::new, |trigger| trigger.name.clone()),
            TreeItem::Column { table, column } => catalog
                .tables
                .get(table)
                .and_then(|table| table.columns.get(column))
                .map_or_else(String::new, |column| {
                    format!("{} {}", column.name, column.type_name)
                }),
            TreeItem::TableIndex(index) => catalog
                .indexes
                .get(index)
                .map_or_else(String::new, |index| index.name.clone()),
            TreeItem::Index(index) => catalog
                .indexes
                .get(index)
                .map_or_else(String::new, |index| {
                    format!("{} on {}", index.name, index.table)
                }),
            TreeItem::View(view) => catalog
                .views
                .get(view)
                .map_or_else(String::new, |view| view.name.clone()),
            TreeItem::Sequence(sequence) => catalog
                .sequences
                .get(sequence)
                .map_or_else(String::new, |sequence| sequence.name.clone()),
            TreeItem::KeySpace(space) => catalog
                .key_spaces
                .get(space)
                .map_or_else(String::new, |space| space.label().to_owned()),
        }
    }

    fn detail(&self, item: TreeItem) -> Option<String> {
        let catalog = &self.catalog;
        match item {
            TreeItem::Group(group) => Some(self.group_size(group).to_string()),
            TreeItem::Columns(_) | TreeItem::TableIndexes(_) | TreeItem::TableTriggers(_) => {
                Some(self.child_count(item).to_string())
            }
            TreeItem::Trigger(trigger) => Some(catalog.triggers.get(trigger)?.events_text()),
            TreeItem::Column { table, column } => {
                let column = catalog.tables.get(table)?.columns.get(column)?;
                if column.primary_key {
                    Some("pk".to_owned())
                } else {
                    column.not_null.then(|| "not null".to_owned())
                }
            }
            TreeItem::TableIndex(index) | TreeItem::Index(index) => catalog
                .indexes
                .get(index)?
                .unique
                .then(|| "unique".to_owned()),
            TreeItem::Sequence(sequence) => catalog.sequences.get(sequence)?.current.clone(),
            TreeItem::KeySpace(space) => Some(catalog.key_spaces.get(space)?.keys.text()),
            _ => None,
        }
    }

    /// The name `Shift+Enter` inserts and `Enter` opens: a table's, view's, index's,
    /// sequence's, column's or collection's; `None` for groups, the connection and the
    /// default key space.
    pub fn name_of(&self, item: TreeItem) -> Option<String> {
        let catalog = &self.catalog;
        match item {
            TreeItem::Table(table) => Some(catalog.tables.get(table)?.name.clone()),
            TreeItem::View(view) => Some(catalog.views.get(view)?.name.clone()),
            TreeItem::Index(index) | TreeItem::TableIndex(index) => {
                Some(catalog.indexes.get(index)?.name.clone())
            }
            TreeItem::Sequence(sequence) => Some(catalog.sequences.get(sequence)?.name.clone()),
            TreeItem::Trigger(trigger) => Some(catalog.triggers.get(trigger)?.name.clone()),
            TreeItem::Column { table, column } => {
                Some(catalog.tables.get(table)?.columns.get(column)?.name.clone())
            }
            TreeItem::KeySpace(space) => catalog.key_spaces.get(space)?.collection.clone(),
            _ => None,
        }
    }

    /// The table a column, an index or a trigger belongs to, as its tree row.
    pub fn table_of(&self, item: TreeItem) -> Option<TreeItem> {
        let catalog = &self.catalog;
        let name = match item {
            TreeItem::Column { table, .. } => return Some(TreeItem::Table(table)),
            TreeItem::Index(index) | TreeItem::TableIndex(index) => {
                &catalog.indexes.get(index)?.table
            }
            TreeItem::Trigger(trigger) => &catalog.triggers.get(trigger)?.table,
            _ => return None,
        };
        catalog
            .tables
            .iter()
            .position(|table| &table.name == name)
            .map(TreeItem::Table)
    }

    fn icon(item: TreeItem) -> RowIcon {
        match item {
            TreeItem::Connection => RowIcon::Database,
            TreeItem::Group(Group::Keys) => RowIcon::Key,
            TreeItem::Group(_)
            | TreeItem::Columns(_)
            | TreeItem::TableIndexes(_)
            | TreeItem::TableTriggers(_) => RowIcon::Folder,
            TreeItem::Table(_) | TreeItem::View(_) => RowIcon::Table,
            TreeItem::Trigger(_) => RowIcon::Bolt,
            _ => RowIcon::Blank,
        }
    }

    /// The visible rows, top to bottom: the expanded rows' children, and while filtering
    /// only the rows whose label holds the filter (any case) and their ancestors, with every
    /// group open and its count as `1 of 3`.
    pub fn rows(&self) -> Vec<TreeRow> {
        let mut rows = Vec::new();
        self.collect(TreeItem::Connection, 0, 1, 1, &mut rows);
        rows
    }

    fn collect(
        &self,
        item: TreeItem,
        level: usize,
        position: usize,
        set_size: usize,
        rows: &mut Vec<TreeRow>,
    ) -> bool {
        let filtering = !self.filter.is_empty();
        let count = self.child_count(item);
        let forced = filtering && matches!(item, TreeItem::Connection | TreeItem::Group(_));
        let open = count > 0 && (forced || self.expanded.contains(&item));
        let label = self.label(item);
        let matched =
            !filtering || item == TreeItem::Connection || contains_any_case(&label, &self.filter);
        let at = rows.len();
        rows.push(TreeRow {
            item,
            level,
            label,
            detail: None,
            expanded: (count > 0).then_some(open),
            position,
            set_size,
            icon: Self::icon(item),
        });
        let mut shown = 0;
        if open {
            for (index, child) in self.children(item).into_iter().enumerate() {
                if self.collect(child, level + 1, index + 1, count, rows) {
                    shown += 1;
                }
            }
        }
        if !matched && shown == 0 {
            rows.truncate(at);
            return false;
        }
        rows[at].detail = if filtering && matches!(item, TreeItem::Group(_)) {
            Some(format!("{shown} of {count}"))
        } else {
            self.detail(item)
        };
        true
    }

    /// Answers a key while the tree has the focus (`docs/ux/workspace.md` 4.2); `now` times
    /// the type-ahead.
    pub fn key(&mut self, press: KeyPress, now: Instant) -> TreeAction {
        let rows = self.rows();
        let Some(last) = rows.len().checked_sub(1) else {
            return TreeAction::None;
        };
        let at = rows
            .iter()
            .position(|row| row.item == self.current)
            .unwrap_or(0);
        let row = &rows[at];
        let primary = press.modifiers.control || press.modifiers.command;
        let action = match press.key {
            Key::Up => self.move_to(&rows, at.saturating_sub(1)),
            Key::Down => self.move_to(&rows, (at + 1).min(last)),
            Key::Home => self.move_to(&rows, 0),
            Key::End => self.move_to(&rows, last),
            Key::Right => match row.expanded {
                Some(false) => {
                    self.expanded.insert(row.item);
                    TreeAction::None
                }
                Some(true)
                    if rows
                        .get(at + 1)
                        .is_some_and(|next| next.level == row.level + 1) =>
                {
                    self.move_to(&rows, at + 1)
                }
                _ => TreeAction::None,
            },
            Key::Left => {
                let forced = !self.filter.is_empty()
                    && matches!(row.item, TreeItem::Connection | TreeItem::Group(_));
                if row.expanded == Some(true) && !forced {
                    self.expanded.remove(&row.item);
                    TreeAction::None
                } else {
                    let parent = rows[..at]
                        .iter()
                        .rposition(|above| above.level + 1 == row.level);
                    parent.map_or(TreeAction::None, |parent| self.move_to(&rows, parent))
                }
            }
            Key::Enter if press.modifiers.shift => {
                if self.name_of(row.item).is_some() {
                    TreeAction::InsertName(row.item)
                } else {
                    TreeAction::None
                }
            }
            Key::Enter => match row.item {
                TreeItem::Table(_) | TreeItem::View(_) => {
                    self.selected = Some(row.item);
                    TreeAction::Open(row.item)
                }
                _ if row.expanded.is_some() => {
                    self.toggle(row.item, row.expanded == Some(true));
                    TreeAction::None
                }
                TreeItem::Sequence(_) => TreeAction::None,
                item @ (TreeItem::Column { .. }
                | TreeItem::Index(_)
                | TreeItem::TableIndex(_)
                | TreeItem::Trigger(_)) => match self.table_of(item) {
                    Some(table) => {
                        self.selected = Some(item);
                        TreeAction::Open(table)
                    }
                    None => TreeAction::None,
                },
                item => {
                    self.selected = Some(item);
                    TreeAction::Inspect(item)
                }
            },
            Key::Space => {
                self.selected = Some(row.item);
                TreeAction::Inspect(row.item)
            }
            Key::F5 => TreeAction::Refresh,
            Key::Tab if press.modifiers.shift => TreeAction::LeaveBack,
            Key::Tab => TreeAction::LeaveForward,
            Key::Character('*') if !primary => {
                let level = row.level;
                let parent = rows[..at]
                    .iter()
                    .rposition(|above| above.level + 1 == level);
                let from = parent.map_or(0, |parent| parent + 1);
                for sibling in rows[from..].iter().take_while(|below| below.level >= level) {
                    if sibling.level == level && sibling.expanded == Some(false) {
                        self.expanded.insert(sibling.item);
                    }
                }
                TreeAction::None
            }
            Key::Character(letter) if !primary && !letter.is_control() => {
                self.type_ahead(letter, now, &rows, at);
                TreeAction::None
            }
            _ => TreeAction::None,
        };
        self.scroll_into_view();
        action
    }

    fn toggle(&mut self, item: TreeItem, expanded: bool) {
        if expanded && self.filter.is_empty() {
            self.expanded.remove(&item);
        } else {
            self.expanded.insert(item);
        }
    }

    fn move_to(&mut self, rows: &[TreeRow], index: usize) -> TreeAction {
        if let Some(row) = rows.get(index) {
            self.current = row.item;
        }
        TreeAction::None
    }

    fn type_ahead(&mut self, letter: char, now: Instant, rows: &[TreeRow], at: usize) {
        let continuing = matches!(self.typed_at, Some(last) if now.saturating_duration_since(last) <= TYPE_AHEAD_WINDOW);
        if !continuing {
            self.typed.clear();
        }
        if self.typed.chars().count() < TYPE_AHEAD_LIMIT {
            self.typed.extend(letter.to_lowercase());
        }
        self.typed_at = Some(now);
        let start = if continuing { at } else { at + 1 };
        let found = (0..rows.len())
            .map(|step| (start + step) % rows.len())
            .find(|&index| rows[index].label.to_lowercase().starts_with(&self.typed));
        if let Some(index) = found {
            self.current = rows[index].item;
        }
    }

    /// Answers a key while the filter field has the focus: letters edit it, `Esc` clears it
    /// and returns to the tree, `Down`, `Enter` and `Tab` go to the tree.
    pub fn filter_key(&mut self, press: KeyPress) -> TreeAction {
        let primary = press.modifiers.control || press.modifiers.command;
        let action = match press.key {
            Key::Escape => {
                self.filter.clear();
                TreeAction::LeaveForward
            }
            Key::Backspace => {
                self.filter.pop();
                TreeAction::None
            }
            Key::Tab if press.modifiers.shift => TreeAction::LeaveBack,
            Key::Down | Key::Enter | Key::Tab => TreeAction::LeaveForward,
            Key::Space if self.filter.chars().count() < FILTER_LIMIT => {
                self.filter.push(' ');
                TreeAction::None
            }
            Key::Character(letter)
                if !primary
                    && !letter.is_control()
                    && self.filter.chars().count() < FILTER_LIMIT =>
            {
                self.filter.push(letter);
                TreeAction::None
            }
            _ => TreeAction::None,
        };
        self.keep_current_visible();
        action
    }

    fn keep_current_visible(&mut self) {
        let rows = self.rows();
        if !rows.iter().any(|row| row.item == self.current) {
            self.current = rows
                .iter()
                .find(|row| row.level > 0 && contains_any_case(&row.label, &self.filter))
                .map_or(TreeItem::Connection, |row| row.item);
        }
        self.first_row = 0;
        self.scroll_into_view();
    }

    /// Scrolls so the current row is among the [`ROWS_IN_VIEW`] drawn.
    pub fn scroll_into_view(&mut self) {
        let rows = self.rows();
        let at = rows
            .iter()
            .position(|row| row.item == self.current)
            .unwrap_or(0);
        if at < self.first_row {
            self.first_row = at;
        } else if at >= self.first_row + ROWS_IN_VIEW {
            self.first_row = at + 1 - ROWS_IN_VIEW;
        }
        self.first_row = self.first_row.min(rows.len().saturating_sub(1));
    }
}

fn contains_any_case(label: &str, filter: &str) -> bool {
    label.to_lowercase().contains(&filter.to_lowercase())
}

/// Where the sidebar's focus is, for drawing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SidebarFocus {
    None,
    Filter,
    Tree,
}

fn icon_key(paint: &mut Painter<'_, '_>, x: f32, y: f32) {
    let color = paint.theme.text_secondary;
    paint
        .canvas
        .stroke_circle(x + 3.5, y + 6.0, 3.0, 1.5, color);
    paint
        .canvas
        .line((x + 6.5, y + 6.0), (x + 12.0, y + 6.0), 1.5, color);
    paint
        .canvas
        .line((x + 10.0, y + 6.0), (x + 10.0, y + 9.0), 1.5, color);
}

fn icon_bolt(paint: &mut Painter<'_, '_>, x: f32, y: f32) {
    let color = paint.theme.text_secondary;
    let points = [(7.0, 0.0), (3.0, 7.0), (6.0, 7.0), (5.0, 12.0), (9.0, 5.0), (6.0, 5.0), (7.0, 0.0)]
        .map(|(dx, dy)| (x + dx, y + dy));
    paint.polyline(&points, 1.5, color);
}

pub(crate) fn draw(
    paint: &mut Painter<'_, '_>,
    tree: &ObjectTree,
    access: &str,
    focus: SidebarFocus,
) {
    let theme = paint.theme;
    let faces = paint.faces;
    paint.rect(0.0, 44.0, SIDEBAR_WIDTH, 732.0, theme.bg1);
    paint.rect(SIDEBAR_WIDTH - 1.0, 44.0, 1.0, 732.0, theme.border);
    let filter_box = Rect::new(8.0, 52.0, 244.0, 28.0);
    paint.button(filter_box, Button::Secondary);
    paint
        .canvas
        .stroke_circle(20.0, 64.0, 8.0, 1.5, theme.text_secondary);
    paint
        .canvas
        .line((23.0, 67.0), (26.0, 70.0), 1.5, theme.text_secondary);
    if tree.filter.is_empty() {
        paint.text(
            faces.ui,
            "Filter objects",
            32.0,
            57.0,
            BODY,
            theme.text_secondary,
        );
    } else {
        let shown = crate::main_window::elided(faces.ui, &tree.filter, 212.0, BODY.0);
        paint.text(faces.ui, &shown, 32.0, 57.0, BODY, theme.text_primary);
    }
    if focus == SidebarFocus::Filter {
        focus_ring(paint, Rect::new(6.0, 50.0, 248.0, 32.0));
    }
    let rows = tree.rows();
    for (offset, row) in rows
        .iter()
        .enumerate()
        .skip(tree.first_row)
        .take(ROWS_IN_VIEW)
    {
        let y = ROWS_TOP + ROW_HEIGHT * (offset - tree.first_row) as f32;
        draw_row(paint, tree, row, y, access, focus == SidebarFocus::Tree);
    }
}

fn draw_row(
    paint: &mut Painter<'_, '_>,
    tree: &ObjectTree,
    row: &TreeRow,
    y: f32,
    access: &str,
    focused: bool,
) {
    let theme = paint.theme;
    let faces = paint.faces;
    let indent = 16.0 * row.level as f32;
    if tree.selected == Some(row.item) {
        let fill = if focused {
            theme.selection
        } else {
            theme.selection_inactive
        };
        paint
            .canvas
            .fill_rounded_rect(Rect::new(4.0, y, 252.0, ROW_HEIGHT), 4.0, fill);
    }
    if let Some(open) = row.expanded {
        chevron(paint, 8.0 + indent, y + 8.0, open);
    }
    let icon_x = 24.0 + indent;
    match row.icon {
        RowIcon::Database => icon_database(paint, icon_x, y + 6.0),
        RowIcon::Folder => icon_folder(paint, icon_x, y + 6.0),
        RowIcon::Key => icon_key(paint, icon_x, y + 6.0),
        RowIcon::Table => icon_table(paint, icon_x, y + 6.0),
        RowIcon::Bolt => icon_bolt(paint, icon_x, y + 6.0),
        RowIcon::Blank => {}
    }
    let detail = if row.item == TreeItem::Connection {
        Some(access.to_owned())
    } else {
        row.detail.clone()
    };
    let detail_width = detail
        .as_deref()
        .map_or(0.0, |text| faces.ui.width(text, SMALL.0) + 8.0);
    let face = if row.level == 0 {
        faces.ui_bold
    } else {
        faces.ui
    };
    let left = 44.0 + indent;
    let shown = crate::main_window::elided(face, &row.label, 244.0 - detail_width - left, BODY.0);
    paint.text(face, &shown, left, y + 3.0, BODY, theme.text_primary);
    if let Some(detail) = detail {
        paint.text_right(
            faces.ui,
            &detail,
            244.0,
            y + 5.0,
            SMALL,
            theme.text_secondary,
        );
    }
    if focused && tree.current == row.item {
        focus_ring(paint, Rect::new(4.0, y, 252.0, ROW_HEIGHT));
    }
}

/// What a screen reader reads after a row's name: its right text, then its level, its
/// position and its state, as `2.1M rows, level 2, 2 of 3, collapsed`.
pub fn row_description(row: &TreeRow, detail: Option<&str>) -> String {
    let mut parts: Vec<String> = detail.map(str::to_owned).into_iter().collect();
    parts.push(format!("level {}", row.level + 1));
    parts.push(format!("{} of {}", row.position, row.set_size));
    match row.expanded {
        Some(true) => parts.push("expanded".to_owned()),
        Some(false) => parts.push("collapsed".to_owned()),
        None => {}
    }
    parts.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{
        CatalogColumn, CatalogIndex, CatalogSequence, CatalogTable, CatalogTrigger, CatalogView,
        KeyCount, KeySpace,
    };

    fn column(name: &str, primary_key: bool) -> CatalogColumn {
        CatalogColumn {
            name: name.to_owned(),
            type_name: "integer".to_owned(),
            primary_key,
            not_null: primary_key,
        }
    }

    fn table(name: &str) -> CatalogTable {
        CatalogTable {
            name: name.to_owned(),
            columns: vec![column("id", true), column("total", false)],
            ddl: format!("CREATE TABLE {name} (id integer PRIMARY KEY, total integer);"),
        }
    }

    fn shop() -> Catalog {
        Catalog {
            tables: vec![table("customers"), table("orders"), table("products")],
            views: vec![CatalogView {
                name: "big_orders".into(),
                columns: vec![column("id", false)],
                ddl: "CREATE VIEW big_orders AS SELECT id FROM orders;".into(),
            }],
            indexes: vec![
                CatalogIndex {
                    name: "customers_name_idx".into(),
                    table: "customers".into(),
                    columns: vec!["name".into()],
                    unique: false,
                    ddl: String::new(),
                },
                CatalogIndex {
                    name: "orders_total_idx".into(),
                    table: "orders".into(),
                    columns: vec!["total".into()],
                    unique: false,
                    ddl: String::new(),
                },
                CatalogIndex {
                    name: "products_name_key".into(),
                    table: "products".into(),
                    columns: vec!["name".into()],
                    unique: true,
                    ddl: String::new(),
                },
            ],
            sequences: vec![
                CatalogSequence {
                    name: "invoice_numbers".into(),
                    current: Some("12".into()),
                },
                CatalogSequence {
                    name: "ticket_numbers".into(),
                    current: None,
                },
            ],
            key_spaces: vec![
                KeySpace {
                    collection: None,
                    keys: KeyCount::Exactly(4),
                },
                KeySpace {
                    collection: Some("sessions".into()),
                    keys: KeyCount::Exactly(312),
                },
            ],
            triggers: vec![CatalogTrigger {
                name: "orders_stamp".into(),
                table: "orders".into(),
                timing: "BEFORE".into(),
                events: vec!["INSERT".into(), "UPDATE".into()],
                action: "EXECUTE FUNCTION stamp()".into(),
            }],
        }
    }

    fn press(key: Key) -> KeyPress {
        KeyPress::plain(key)
    }

    fn labels(tree: &ObjectTree) -> Vec<String> {
        tree.rows().into_iter().map(|row| row.label).collect()
    }

    #[test]
    fn the_first_rows_are_the_connection_and_the_open_tables_then_each_group_in_order() {
        let tree = ObjectTree::new("shop", shop());
        assert_eq!(
            labels(&tree),
            [
                "shop",
                "Tables",
                "customers",
                "orders",
                "products",
                "Views",
                "Indexes",
                "Sequences",
                "Keys"
            ]
        );
        let rows = tree.rows();
        assert_eq!(rows[1].detail.as_deref(), Some("3"));
        assert_eq!(rows[5].detail.as_deref(), Some("1"));
        assert_eq!(rows[8].detail.as_deref(), Some("2"));
        assert_eq!(
            (
                rows[3].level,
                rows[3].position,
                rows[3].set_size,
                rows[3].expanded
            ),
            (2, 2, 3, Some(false))
        );
        assert_eq!(rows[8].icon, RowIcon::Key);
        let empty = ObjectTree::new("empty", Catalog::default());
        assert_eq!(labels(&empty), ["empty", "Tables"]);
        assert_eq!(empty.rows()[1].expanded, None, "an empty group is a leaf");
    }

    #[test]
    fn children_are_listed_only_once_their_parent_expands() {
        let mut tree = ObjectTree::new("shop", shop());
        let now = Instant::now();
        tree.key(press(Key::Down), now);
        tree.key(press(Key::Left), now);
        assert_eq!(
            labels(&tree),
            ["shop", "Tables", "Views", "Indexes", "Sequences", "Keys"]
        );
        tree.key(press(Key::Right), now);
        tree.key(press(Key::Right), now);
        assert_eq!(tree.current, TreeItem::Table(0));
        tree.key(press(Key::Down), now);
        tree.key(press(Key::Right), now);
        let rows = tree.rows();
        assert_eq!(rows[4].label, "Columns");
        assert_eq!(rows[5].label, "Indexes");
        assert_eq!(rows[5].detail.as_deref(), Some("1"));
        assert_eq!(rows[6].label, "Triggers");
        assert_eq!(rows[6].detail.as_deref(), Some("1"));
        assert_eq!(rows[6].item, TreeItem::TableTriggers(1));
        tree.key(press(Key::Right), now);
        tree.key(press(Key::Right), now);
        assert_eq!(tree.current, TreeItem::Columns(1));
        tree.key(press(Key::Right), now);
        let rows = tree.rows();
        assert_eq!(rows[5].label, "id integer");
        assert_eq!(rows[5].detail.as_deref(), Some("pk"));
        assert_eq!(rows[5].level, 4);
        tree.key(press(Key::Left), now);
        assert_eq!(
            tree.current,
            TreeItem::Columns(1),
            "Left on a leaf moves to its parent"
        );
        tree.key(press(Key::Left), now);
        assert_eq!(
            tree.rows()[5].label,
            "Indexes",
            "Left on an expanded row collapses it"
        );
        tree.key(press(Key::Left), now);
        assert_eq!(tree.current, TreeItem::Table(1));
    }

    #[test]
    fn home_end_and_the_star_key_move_and_expand_siblings() {
        let mut tree = ObjectTree::new("shop", shop());
        let now = Instant::now();
        tree.key(press(Key::End), now);
        assert_eq!(tree.current, TreeItem::Group(Group::Keys));
        tree.key(press(Key::Character('*')), now);
        let rows = tree.rows();
        assert!(
            rows.iter()
                .filter(|row| row.level == 1)
                .all(|row| row.expanded == Some(true))
        );
        assert!(
            rows.iter()
                .any(|row| row.label == "products_name_key on products"
                    && row.detail.as_deref() == Some("unique"))
        );
        assert!(
            rows.iter()
                .any(|row| row.label == "invoice_numbers" && row.detail.as_deref() == Some("12"))
        );
        assert!(
            rows.iter()
                .any(|row| row.label == "sessions" && row.detail.as_deref() == Some("312"))
        );
        tree.key(press(Key::Home), now);
        assert_eq!(tree.current, TreeItem::Connection);
        tree.key(press(Key::Up), now);
        assert_eq!(tree.current, TreeItem::Connection);
    }

    #[test]
    fn letters_jump_to_the_next_row_with_that_prefix_for_a_second() {
        let mut tree = ObjectTree::new("shop", shop());
        let now = Instant::now();
        tree.key(press(Key::Character('p')), now);
        assert_eq!(tree.current, TreeItem::Table(2));
        tree.key(press(Key::Character('r')), now + Duration::from_millis(300));
        assert_eq!(tree.current, TreeItem::Table(2), "`pr` stays on products");
        tree.key(
            press(Key::Character('o')),
            now + Duration::from_millis(2_000),
        );
        assert_eq!(
            tree.current,
            TreeItem::Table(1),
            "after a second `o` starts again: orders"
        );
        tree.key(
            press(Key::Character('V')),
            now + Duration::from_millis(4_000),
        );
        assert_eq!(tree.current, TreeItem::Group(Group::Views));
    }

    #[test]
    fn enter_opens_tables_toggles_groups_and_space_selects() {
        let mut tree = ObjectTree::new("shop", shop());
        let now = Instant::now();
        tree.key(press(Key::Down), now);
        tree.key(press(Key::Down), now);
        tree.key(press(Key::Down), now);
        assert_eq!(
            tree.key(press(Key::Enter), now),
            TreeAction::Open(TreeItem::Table(1))
        );
        assert_eq!(tree.selected, Some(TreeItem::Table(1)));
        let shift_enter = KeyPress {
            key: Key::Enter,
            modifiers: eggdb_platform::window::Modifiers {
                shift: true,
                ..Default::default()
            },
        };
        assert_eq!(
            tree.key(shift_enter, now),
            TreeAction::InsertName(TreeItem::Table(1))
        );
        assert_eq!(tree.name_of(TreeItem::Table(1)).as_deref(), Some("orders"));
        tree.key(press(Key::End), now);
        assert_eq!(tree.key(press(Key::Enter), now), TreeAction::None);
        assert!(tree.expanded.contains(&TreeItem::Group(Group::Keys)));
        tree.key(press(Key::Down), now);
        assert_eq!(
            tree.key(press(Key::Space), now),
            TreeAction::Inspect(TreeItem::KeySpace(0))
        );
        assert_eq!(
            tree.name_of(TreeItem::KeySpace(0)),
            None,
            "the default key space has no name"
        );
        assert_eq!(tree.key(press(Key::F5), now), TreeAction::Refresh);
    }

    #[test]
    fn a_tables_triggers_group_lists_each_trigger_with_its_events() {
        let mut tree = ObjectTree::new("shop", shop());
        tree.expanded.insert(TreeItem::Table(1));
        tree.expanded.insert(TreeItem::TableTriggers(1));
        tree.expanded.insert(TreeItem::Table(0));
        let rows = tree.rows();
        let trigger = rows
            .iter()
            .find(|row| row.item == TreeItem::Trigger(0))
            .expect("the trigger row is listed");
        assert_eq!(trigger.label, "orders_stamp");
        assert_eq!(trigger.detail.as_deref(), Some("INSERT, UPDATE"));
        assert_eq!(trigger.level, 4);
        assert_eq!(trigger.expanded, None);
        assert_eq!(tree.name_of(TreeItem::Trigger(0)).as_deref(), Some("orders_stamp"));
        assert!(
            !rows.iter().any(|row| row.item == TreeItem::TableTriggers(0)),
            "a table without triggers has no Triggers group"
        );
        let mut refreshed = shop();
        refreshed.triggers.insert(
            0,
            CatalogTrigger {
                name: "audit".into(),
                table: "customers".into(),
                timing: "AFTER".into(),
                events: vec!["DELETE".into()],
                action: String::new(),
            },
        );
        tree.current = TreeItem::Trigger(0);
        tree.set_catalog(refreshed);
        assert_eq!(tree.current, TreeItem::Trigger(1), "the refresh keeps the trigger by name");
    }

    #[test]
    fn enter_on_a_column_index_or_trigger_opens_its_table_and_on_a_sequence_nothing() {
        let mut tree = ObjectTree::new("shop", shop());
        let now = Instant::now();
        for open in [
            TreeItem::Table(0),
            TreeItem::TableIndexes(0),
            TreeItem::Table(1),
            TreeItem::Columns(1),
            TreeItem::TableTriggers(1),
            TreeItem::Group(Group::Indexes),
        ] {
            tree.expanded.insert(open);
        }
        tree.current = TreeItem::Column { table: 1, column: 1 };
        assert_eq!(tree.key(press(Key::Enter), now), TreeAction::Open(TreeItem::Table(1)));
        assert_eq!(tree.selected, Some(TreeItem::Column { table: 1, column: 1 }));
        tree.current = TreeItem::Index(2);
        assert_eq!(tree.key(press(Key::Enter), now), TreeAction::Open(TreeItem::Table(2)));
        tree.current = TreeItem::TableIndex(0);
        assert_eq!(tree.key(press(Key::Enter), now), TreeAction::Open(TreeItem::Table(0)));
        tree.current = TreeItem::Trigger(0);
        assert_eq!(tree.key(press(Key::Enter), now), TreeAction::Open(TreeItem::Table(1)));
        tree.expanded.insert(TreeItem::Group(Group::Sequences));
        tree.current = TreeItem::Sequence(0);
        assert_eq!(tree.key(press(Key::Enter), now), TreeAction::None);
        assert_eq!(tree.table_of(TreeItem::Sequence(0)), None);
    }

    #[test]
    fn the_filter_keeps_matching_rows_and_their_ancestors() {
        let mut tree = ObjectTree::new("shop", shop());
        for letter in "ORD".chars() {
            tree.filter_key(press(Key::Character(letter)));
        }
        let rows = tree.rows();
        let shown: Vec<&str> = rows.iter().map(|row| row.label.as_str()).collect();
        assert_eq!(
            shown,
            [
                "shop",
                "Tables",
                "orders",
                "Views",
                "big_orders",
                "Indexes",
                "orders_total_idx on orders"
            ]
        );
        assert_eq!(rows[1].detail.as_deref(), Some("1 of 3"));
        assert_eq!(tree.current, TreeItem::Connection);
        tree.filter_key(press(Key::Backspace));
        tree.filter_key(press(Key::Backspace));
        tree.filter_key(press(Key::Backspace));
        assert_eq!(tree.filter, "");
        tree.filter_key(press(Key::Character('x')));
        assert_eq!(
            tree.filter_key(press(Key::Escape)),
            TreeAction::LeaveForward
        );
        assert_eq!(tree.filter, "");
    }

    #[test]
    fn a_refresh_keeps_the_rows_that_still_exist() {
        let mut tree = ObjectTree::new("shop", shop());
        tree.expanded.insert(TreeItem::Table(1));
        tree.current = TreeItem::Table(2);
        tree.selected = Some(TreeItem::Table(1));
        let mut smaller = shop();
        smaller.tables.remove(0);
        tree.set_catalog(smaller);
        assert_eq!(tree.current, TreeItem::Table(1));
        assert_eq!(tree.selected, Some(TreeItem::Table(0)));
        assert!(tree.expanded.contains(&TreeItem::Table(0)));
    }

    #[test]
    fn the_current_row_stays_in_view() {
        let catalog = Catalog {
            tables: (0..100).map(|n| table(&format!("t{n:03}"))).collect(),
            ..Catalog::default()
        };
        let mut tree = ObjectTree::new("big", catalog);
        tree.key(press(Key::End), Instant::now());
        assert_eq!(tree.first_row, 102 - ROWS_IN_VIEW);
        tree.key(press(Key::Home), Instant::now());
        assert_eq!(tree.first_row, 0);
    }

    #[test]
    fn a_row_reads_its_detail_level_position_and_state() {
        let tree = ObjectTree::new("shop", shop());
        let rows = tree.rows();
        assert_eq!(
            row_description(&rows[3], None),
            "level 3, 2 of 3, collapsed"
        );
        assert_eq!(
            row_description(&rows[1], Some("3")),
            "3, level 2, 1 of 5, expanded"
        );
    }
}
