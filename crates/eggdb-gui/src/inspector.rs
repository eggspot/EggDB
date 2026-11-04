use crate::canvas::Rect;
use crate::catalog::{CatalogColumn, CatalogIndex, CatalogTrigger};
use crate::color::Rgba;
use crate::connection::{Connection, ResultTable};
use crate::dialect::QueryDialect;
use crate::grid::quote_name;
use crate::main_window::{BODY, CODE, Painter, SMALL, chevron, elided};
use crate::object_browser::{Group, ObjectTree, TreeItem};
use crate::status_line;

/// From this window width the inspector is a docked region, open by default; narrower,
/// `Ctrl/Cmd+I` opens it over the main area.
pub const DOCKED_FROM_WIDTH: usize = 1440;
/// The inspector's width.
pub const INSPECTOR_WIDTH: f32 = 280.0;
/// How many sample rows the inspector fetches.
pub const SAMPLE_ROWS: usize = 5;

const SAMPLE_COLUMNS: usize = 3;
const DDL_LINES: usize = 12;
const DDL_RING_OUTSET: f32 = 3.0;
const PADDING: f32 = 12.0;
const TOP: f32 = 44.0;
const BOTTOM: f32 = 776.0;
const SAMPLE_ROW_HEIGHT: f32 = 20.0;
const TITLE: (f32, f32) = (15.0, 20.0);

/// A block of the inspector that folds under its heading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InspectorBlock {
    /// `Columns (9)`.
    Columns,
    /// `Indexes (3)`.
    Indexes,
    /// `Triggers (2)`, on a table that has triggers.
    Triggers,
    /// `DDL`.
    Ddl,
    /// `Sample rows`.
    SampleRows,
}

/// A table's or view's first rows and row count, fetched for the inspector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sample {
    /// The table or view they come from.
    pub of: String,
    /// How many rows it has, when the count answered.
    pub row_count: Option<usize>,
    /// Its first [`SAMPLE_ROWS`] rows.
    pub rows: ResultTable,
}

/// The inspector region (`docs/ux/workspace.md` 4.3): whether it is open and how, its folded
/// blocks, and the sample of the selected table.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Inspector {
    /// Open as a region beside the main area.
    pub docked: bool,
    /// Open over the main area's right edge, with a shadow.
    pub overlay: bool,
    /// The blocks folded under their headings.
    pub collapsed: Vec<InspectorBlock>,
    /// The selected table's sample, once fetched.
    pub sample: Option<Sample>,
    /// Whether the `DDL` box has the focus, after `Show DDL` (`docs/ux/workspace.md` 10.9).
    pub ddl_focused: bool,
}

impl Inspector {
    /// The inspector as a window `width` wide first shows it: docked from
    /// [`DOCKED_FROM_WIDTH`], closed below.
    pub fn for_width(width: usize) -> Self {
        Self {
            docked: width >= DOCKED_FROM_WIDTH,
            ..Self::default()
        }
    }

    /// Whether it shows.
    pub fn is_open(&self) -> bool {
        self.docked || self.overlay
    }

    /// `Ctrl/Cmd+I`: docks or undocks it in a wide window, opens or closes the overlay in a
    /// narrower one.
    pub fn toggle(&mut self, width: usize) {
        if width >= DOCKED_FROM_WIDTH {
            self.docked = !self.docked;
            self.overlay = false;
        } else {
            self.overlay = !self.overlay;
            self.docked = false;
        }
    }

    /// Folds or unfolds `block`.
    pub fn toggle_block(&mut self, block: InspectorBlock) {
        if let Some(at) = self.collapsed.iter().position(|&folded| folded == block) {
            self.collapsed.remove(at);
        } else {
            self.collapsed.push(block);
        }
    }

    /// Whether `block` shows its content.
    pub fn is_expanded(&self, block: InspectorBlock) -> bool {
        !self.collapsed.contains(&block)
    }
}

/// What the inspector shows for one tree row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Description {
    /// The name in the header.
    pub name: String,
    /// The line under it: `Table · 3 rows`, `Index on orders`, `Collection · 312 keys`.
    pub subtitle: String,
    /// The `Columns` block's rows.
    pub columns: Vec<CatalogColumn>,
    /// The `Indexes` block's rows.
    pub indexes: Vec<CatalogIndex>,
    /// The `Triggers` block's rows, on a table.
    pub triggers: Vec<CatalogTrigger>,
    /// The `DDL` block's statement.
    pub ddl: Option<String>,
    /// That block's heading: `DDL`, or `Action` for a trigger, whose statement is what it runs.
    pub ddl_heading: &'static str,
    /// The `Sample rows` block's rows, once fetched.
    pub sample: Option<ResultTable>,
}

impl Description {
    fn titled(name: String, subtitle: String) -> Self {
        Self {
            name,
            subtitle,
            columns: Vec::new(),
            indexes: Vec::new(),
            triggers: Vec::new(),
            ddl: None,
            ddl_heading: "DDL",
            sample: None,
        }
    }

    /// The blocks it has, top to bottom, each with its heading.
    pub fn blocks(&self) -> Vec<(InspectorBlock, String)> {
        let mut blocks = Vec::new();
        if !self.columns.is_empty() {
            blocks.push((
                InspectorBlock::Columns,
                format!("Columns ({})", self.columns.len()),
            ));
        }
        if !self.indexes.is_empty() {
            blocks.push((
                InspectorBlock::Indexes,
                format!("Indexes ({})", self.indexes.len()),
            ));
        }
        if !self.triggers.is_empty() {
            blocks.push((
                InspectorBlock::Triggers,
                format!("Triggers ({})", self.triggers.len()),
            ));
        }
        if self.ddl.is_some() {
            blocks.push((InspectorBlock::Ddl, self.ddl_heading.to_owned()));
        }
        if self.sample.is_some() {
            blocks.push((InspectorBlock::SampleRows, "Sample rows".to_owned()));
        }
        blocks
    }
}

fn counted(count: usize, one: &str, many: &str) -> String {
    if count == 1 {
        format!("1 {one}")
    } else {
        format!("{count} {many}")
    }
}

/// Describes `item` of `tree` for the inspector, with `sample` when it is the item's.
pub fn describe(tree: &ObjectTree, item: TreeItem, sample: Option<&Sample>) -> Description {
    let catalog = &tree.catalog;
    let own_sample = |name: &str| sample.filter(|sample| sample.of == name);
    match item {
        TreeItem::Connection => {
            let mut parts = vec![counted(catalog.tables.len(), "table", "tables")];
            if !catalog.views.is_empty() {
                parts.push(counted(catalog.views.len(), "view", "views"));
            }
            if !catalog.indexes.is_empty() {
                parts.push(counted(catalog.indexes.len(), "index", "indexes"));
            }
            if !catalog.key_spaces.is_empty() {
                parts.push(counted(catalog.key_spaces.len(), "key space", "key spaces"));
            }
            Description::titled(
                tree.connection.clone(),
                format!("Database · {}", parts.join(" · ")),
            )
        }
        TreeItem::Group(group) => {
            let count = tree.children(item).len();
            let (one, many) = match group {
                Group::Tables => ("table", "tables"),
                Group::Views => ("view", "views"),
                Group::Indexes => ("index", "indexes"),
                Group::Sequences => ("sequence", "sequences"),
                Group::Keys => ("key space", "key spaces"),
            };
            Description::titled(group.label().to_owned(), counted(count, one, many))
        }
        TreeItem::Table(table)
        | TreeItem::Columns(table)
        | TreeItem::TableIndexes(table)
        | TreeItem::TableTriggers(table) => {
            let Some(table) = catalog.tables.get(table) else {
                return Description::titled(String::new(), String::new());
            };
            let sample = own_sample(&table.name);
            let subtitle = match sample.and_then(|sample| sample.row_count) {
                Some(rows) => format!("Table · {}", status_line::rows_text(rows)),
                None => "Table".to_owned(),
            };
            Description {
                columns: table.columns.clone(),
                indexes: catalog.indexes_of(&table.name).cloned().collect(),
                triggers: catalog.triggers_of(&table.name).cloned().collect(),
                ddl: Some(table.ddl.clone()).filter(|ddl| !ddl.is_empty()),
                sample: sample.map(|sample| sample.rows.clone()),
                ..Description::titled(table.name.clone(), subtitle)
            }
        }
        TreeItem::Column { table, column } => {
            let Some((table, column)) = catalog
                .tables
                .get(table)
                .and_then(|table| Some((table, table.columns.get(column)?)))
            else {
                return Description::titled(String::new(), String::new());
            };
            Description {
                columns: vec![column.clone()],
                ..Description::titled(
                    column.name.clone(),
                    format!("Column of {} · {}", table.name, column.type_name),
                )
            }
        }
        TreeItem::View(view) => {
            let Some(view) = catalog.views.get(view) else {
                return Description::titled(String::new(), String::new());
            };
            let sample = own_sample(&view.name);
            let subtitle = match sample.and_then(|sample| sample.row_count) {
                Some(rows) => format!("View · {}", status_line::rows_text(rows)),
                None => "View".to_owned(),
            };
            Description {
                columns: view.columns.clone(),
                ddl: Some(view.ddl.clone()),
                sample: sample.map(|sample| sample.rows.clone()),
                ..Description::titled(view.name.clone(), subtitle)
            }
        }
        TreeItem::Index(index) | TreeItem::TableIndex(index) => {
            let Some(index) = catalog.indexes.get(index) else {
                return Description::titled(String::new(), String::new());
            };
            Description {
                indexes: vec![index.clone()],
                ddl: Some(index.ddl.clone()).filter(|ddl| !ddl.is_empty()),
                ..Description::titled(index.name.clone(), format!("Index on {}", index.table))
            }
        }
        TreeItem::Sequence(sequence) => {
            let Some(sequence) = catalog.sequences.get(sequence) else {
                return Description::titled(String::new(), String::new());
            };
            let subtitle = sequence.current.as_ref().map_or_else(
                || "Sequence".to_owned(),
                |value| format!("Sequence · current {value}"),
            );
            Description::titled(sequence.name.clone(), subtitle)
        }
        TreeItem::KeySpace(space) => {
            let Some(space) = catalog.key_spaces.get(space) else {
                return Description::titled(String::new(), String::new());
            };
            let kind = if space.collection.is_some() {
                "Collection"
            } else {
                "Key space"
            };
            let keys = match space.keys {
                crate::catalog::KeyCount::Exactly(1) => "1 key".to_owned(),
                count => format!("{} keys", count.text()),
            };
            Description::titled(space.label().to_owned(), format!("{kind} · {keys}"))
        }
        TreeItem::Trigger(trigger) => {
            let Some(trigger) = catalog.triggers.get(trigger) else {
                return Description::titled(String::new(), String::new());
            };
            Description {
                ddl: Some(trigger.action.clone()).filter(|action| !action.is_empty()),
                ddl_heading: "Action",
                ..Description::titled(trigger.name.clone(), trigger.summary())
            }
        }
    }
}

/// The name whose sample the inspector fetches for `item`: a table's or a view's.
pub fn sampled_name(tree: &ObjectTree, item: TreeItem) -> Option<String> {
    match item {
        TreeItem::Table(table)
        | TreeItem::Columns(table)
        | TreeItem::TableIndexes(table)
        | TreeItem::TableTriggers(table) => Some(tree.catalog.tables.get(table)?.name.clone()),
        TreeItem::View(view) => Some(tree.catalog.views.get(view)?.name.clone()),
        _ => None,
    }
}

/// Fetches `name`'s row count and first [`SAMPLE_ROWS`] rows over `connection`; a part
/// that fails is left empty.
pub fn fetch_sample(connection: &mut Connection, name: &str) -> Sample {
    let quoted = quote_name(name, QueryDialect::PostgreSql);
    let row_count = connection
        .run(&format!("SELECT count(*) FROM {quoted}"))
        .ok()
        .and_then(|tables| tables.into_iter().next())
        .and_then(|table| table.rows.into_iter().next())
        .and_then(|row| row.into_iter().next().flatten())
        .and_then(|text| text.parse().ok());
    let rows = connection
        .run(&format!("SELECT * FROM {quoted} LIMIT {SAMPLE_ROWS}"))
        .ok()
        .and_then(|tables| tables.into_iter().next())
        .unwrap_or_default();
    Sample {
        of: name.to_owned(),
        row_count,
        rows,
    }
}

/// The inspector's rect, `inspector_width` wide, in a window `width` wide.
pub fn region(width: f32, inspector_width: f32) -> Rect {
    Rect::new(width - inspector_width, TOP, inspector_width, BOTTOM - TOP)
}

/// Where each block's heading is drawn, top to bottom, for the accessibility tree.
pub fn heading_tops(
    description: &Description,
    inspector: &Inspector,
) -> Vec<(InspectorBlock, String, f32)> {
    let mut placed = Vec::new();
    let mut y = TOP + 64.0;
    for (block, heading) in description.blocks() {
        placed.push((block, heading, y));
        y += 28.0;
        if inspector.is_expanded(block) {
            y += block_height(description, block) + 8.0;
        }
    }
    placed
}

fn ddl_lines(ddl: &str) -> Vec<&str> {
    ddl.lines().take(DDL_LINES).collect()
}

fn block_height(description: &Description, block: InspectorBlock) -> f32 {
    match block {
        InspectorBlock::Columns => 20.0 * description.columns.len() as f32,
        InspectorBlock::Indexes => 20.0 * description.indexes.len() as f32,
        InspectorBlock::Triggers => 36.0 * description.triggers.len() as f32,
        InspectorBlock::Ddl => {
            16.0 * ddl_lines(description.ddl.as_deref().unwrap_or("")).len() as f32 + 16.0
        }
        InspectorBlock::SampleRows => {
            SAMPLE_ROW_HEIGHT
                * (1 + description
                    .sample
                    .as_ref()
                    .map_or(0, |rows| rows.rows.len().min(SAMPLE_ROWS))) as f32
        }
    }
}

fn badge(paint: &mut Painter<'_, '_>, text: &str, right: f32, top: f32) -> f32 {
    let theme = paint.theme;
    let width = paint.faces.ui.width(text, SMALL.0) + 12.0;
    let rect = Rect::new(right - width, top + 2.0, width, 16.0);
    paint.canvas.fill_rounded_rect(rect, 8.0, theme.bg2);
    let face = paint.faces.ui;
    paint.text(
        face,
        text,
        rect.x + 6.0,
        top + 3.0,
        SMALL,
        theme.text_secondary,
    );
    rect.x
}

/// Draws the inspector for `description`, `width` wide, at the window's right edge; as an
/// overlay it casts a shadow on the main area.
pub(crate) fn draw(
    paint: &mut Painter<'_, '_>,
    inspector: &Inspector,
    description: &Description,
    width: f32,
) {
    let theme = paint.theme;
    let faces = paint.faces;
    let area = region(paint.width, width);
    if inspector.overlay {
        for step in 0..12 {
            let alpha = (41 * (step + 1) / 12) as u8;
            paint.rect(
                area.x - 12.0 + step as f32,
                area.y + 4.0,
                1.0,
                area.h - 4.0,
                Rgba {
                    r: 0,
                    g: 0,
                    b: 0,
                    a: alpha,
                },
            );
        }
    }
    paint.rect(area.x, area.y, area.w, area.h, theme.bg1);
    paint.rect(area.x, area.y, 1.0, area.h, theme.border);
    let left = area.x + PADDING;
    let right = area.x + area.w - PADDING;
    let inner = right - left;
    let name = elided(faces.ui_bold, &description.name, inner, TITLE.0);
    paint.text(
        faces.ui_bold,
        &name,
        left,
        TOP + 12.0,
        TITLE,
        theme.text_primary,
    );
    let subtitle = elided(faces.ui, &description.subtitle, inner, SMALL.0);
    paint.text(
        faces.ui,
        &subtitle,
        left,
        TOP + 36.0,
        SMALL,
        theme.text_secondary,
    );
    for (block, heading, top) in heading_tops(description, inspector) {
        if top > BOTTOM - 28.0 {
            break;
        }
        let open = inspector.is_expanded(block);
        chevron(paint, left, top + 8.0, open);
        paint.text(
            faces.ui_bold,
            &heading,
            left + 16.0,
            top + 4.0,
            BODY,
            theme.text_primary,
        );
        if block == InspectorBlock::Ddl {
            paint.text_right(faces.ui, "Copy", right, top + 4.0, BODY, theme.accent);
        }
        if open {
            let focused = inspector.ddl_focused && block == InspectorBlock::Ddl;
            draw_block(paint, description, block, top + 28.0, (left, right), focused);
        }
    }
}

fn draw_block(
    paint: &mut Painter<'_, '_>,
    description: &Description,
    block: InspectorBlock,
    top: f32,
    (left, right): (f32, f32),
    focused: bool,
) {
    let theme = paint.theme;
    let faces = paint.faces;
    let fits = |y: f32| y + 20.0 <= BOTTOM - 8.0;
    match block {
        InspectorBlock::Columns => {
            for (index, column) in description.columns.iter().enumerate() {
                let y = top + 20.0 * index as f32;
                if !fits(y) {
                    break;
                }
                let type_width = faces.mono.width(&column.type_name, SMALL.0);
                paint.text_right(
                    faces.mono,
                    &column.type_name,
                    right,
                    y + 3.0,
                    SMALL,
                    theme.text_secondary,
                );
                let mut badge_right = right - type_width - 8.0;
                if column.primary_key {
                    badge_right = badge(paint, "pk", badge_right, y) - 4.0;
                } else if column.not_null {
                    badge_right = badge(paint, "not null", badge_right, y) - 4.0;
                }
                let name = elided(
                    faces.mono,
                    &column.name,
                    (badge_right - left).max(0.0),
                    CODE.0,
                );
                paint.text(faces.mono, &name, left, y, CODE, theme.text_primary);
            }
        }
        InspectorBlock::Indexes => {
            for (index, found) in description.indexes.iter().enumerate() {
                let y = top + 20.0 * index as f32;
                if !fits(y) {
                    break;
                }
                let badge_right = if found.unique {
                    badge(paint, "unique", right, y) - 4.0
                } else {
                    right
                };
                let text = format!("{} ({})", found.name, found.columns.join(", "));
                let text = elided(faces.mono, &text, (badge_right - left).max(0.0), CODE.0);
                paint.text(faces.mono, &text, left, y, CODE, theme.text_primary);
            }
        }
        InspectorBlock::Triggers => {
            for (index, trigger) in description.triggers.iter().enumerate() {
                let y = top + 36.0 * index as f32;
                if !fits(y + 16.0) {
                    break;
                }
                let name = elided(faces.mono, &trigger.name, right - left, CODE.0);
                paint.text(faces.mono, &name, left, y, CODE, theme.text_primary);
                let when = format!("{} {}", trigger.timing, trigger.events_text());
                let when = elided(faces.ui, &when, right - left, SMALL.0);
                paint.text(faces.ui, &when, left, y + 20.0, SMALL, theme.text_secondary);
            }
        }
        InspectorBlock::Ddl => {
            let ddl = description.ddl.as_deref().unwrap_or("");
            let lines = ddl_lines(ddl);
            let height = 16.0 * lines.len() as f32 + 8.0;
            let box_rect = Rect::new(
                left,
                top,
                right - left,
                height.min(BOTTOM - 8.0 - top).max(0.0),
            );
            paint.canvas.stroke_rect(box_rect, theme.border);
            if focused {
                crate::main_window::focus_ring(
                    paint,
                    Rect::new(
                        box_rect.x - DDL_RING_OUTSET,
                        box_rect.y - DDL_RING_OUTSET,
                        box_rect.w + 2.0 * DDL_RING_OUTSET,
                        box_rect.h + 2.0 * DDL_RING_OUTSET,
                    ),
                );
            }
            for (index, line) in lines.iter().enumerate() {
                let y = top + 4.0 + 16.0 * index as f32;
                if y + 16.0 > box_rect.y + box_rect.h {
                    break;
                }
                let shown = elided(faces.mono, line, right - left - 16.0, SMALL.0);
                paint.text(
                    faces.mono,
                    &shown,
                    left + 8.0,
                    y,
                    (SMALL.0, 16.0),
                    theme.text_primary,
                );
            }
        }
        InspectorBlock::SampleRows => {
            let Some(sample) = &description.sample else {
                return;
            };
            let shown_columns = sample.columns.len().min(SAMPLE_COLUMNS);
            let more = sample.columns.len() > SAMPLE_COLUMNS;
            let cells = shown_columns + usize::from(more);
            if cells == 0 {
                return;
            }
            let cell_width = (right - left) / cells as f32;
            let mut header: Vec<String> = sample
                .columns
                .iter()
                .take(SAMPLE_COLUMNS)
                .cloned()
                .collect();
            if more {
                header.push("…".to_owned());
            }
            sample_line(
                paint,
                &header,
                (left, top),
                cell_width,
                theme.text_secondary,
            );
            for (index, row) in sample.rows.iter().take(SAMPLE_ROWS).enumerate() {
                let y = top + SAMPLE_ROW_HEIGHT * (index + 1) as f32;
                if !fits(y) {
                    break;
                }
                let mut values: Vec<String> = row
                    .iter()
                    .take(SAMPLE_COLUMNS)
                    .map(|value| value.clone().unwrap_or_else(|| "NULL".to_owned()))
                    .collect();
                if more {
                    values.push("…".to_owned());
                }
                sample_line(paint, &values, (left, y), cell_width, theme.text_primary);
            }
        }
    }
}

fn sample_line(
    paint: &mut Painter<'_, '_>,
    values: &[String],
    (left, top): (f32, f32),
    cell_width: f32,
    ink: Rgba,
) {
    let mono = paint.faces.mono;
    for (index, value) in values.iter().enumerate() {
        let x = left + cell_width * index as f32;
        let shown = elided(mono, value, cell_width - 8.0, SMALL.0);
        paint.text(mono, &shown, x, top + 3.0, SMALL, ink);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::main_window::MainWindow;

    #[test]
    fn it_opens_by_default_only_from_1440_px() {
        assert!(!Inspector::for_width(1280).is_open());
        assert!(Inspector::for_width(1440).docked);
        let mut narrow = Inspector::for_width(1280);
        narrow.toggle(1280);
        assert!(narrow.overlay && !narrow.docked);
        narrow.toggle(1280);
        assert!(!narrow.is_open());
        let mut wide = Inspector::for_width(1600);
        wide.toggle(1600);
        assert!(!wide.is_open());
    }

    #[test]
    fn a_table_shows_its_columns_indexes_ddl_and_sample() {
        let window = MainWindow::sample();
        let sample = Sample {
            of: "orders".to_owned(),
            row_count: Some(3),
            rows: ResultTable {
                columns: vec!["id".into(), "customer_id".into(), "total".into()],
                rows: vec![vec![Some("1".into()), Some("1".into()), None]],
                row_count: 1,
                ..ResultTable::default()
            },
        };
        let orders = window
            .tree
            .catalog
            .tables
            .iter()
            .position(|table| table.name == "orders")
            .unwrap();
        let description = describe(&window.tree, TreeItem::Table(orders), Some(&sample));
        assert_eq!(description.name, "orders");
        assert_eq!(description.subtitle, "Table · 3 rows");
        let headings: Vec<String> = description
            .blocks()
            .into_iter()
            .map(|(_, heading)| heading)
            .collect();
        assert_eq!(
            headings,
            ["Columns (3)", "Indexes (1)", "DDL", "Sample rows"]
        );
        let other = describe(&window.tree, TreeItem::Table(0), Some(&sample));
        assert_eq!(
            other.subtitle, "Table",
            "another table's sample is not used"
        );
        assert_eq!(other.sample, None);
    }

    #[test]
    fn a_table_with_triggers_lists_them_between_indexes_and_ddl() {
        let mut window = MainWindow::sample();
        let trigger = |name: &str, events: &[&str]| CatalogTrigger {
            name: name.to_owned(),
            table: "orders".to_owned(),
            timing: "AFTER".to_owned(),
            events: events.iter().map(|event| (*event).to_owned()).collect(),
            action: "EXECUTE FUNCTION audit()".to_owned(),
        };
        window.tree.catalog.triggers = vec![
            trigger("orders_audit", &["INSERT", "UPDATE"]),
            trigger("orders_touch", &["UPDATE"]),
        ];
        let orders = window
            .tree
            .catalog
            .tables
            .iter()
            .position(|table| table.name == "orders")
            .unwrap();
        let description = describe(&window.tree, TreeItem::Table(orders), None);
        let headings: Vec<String> = description
            .blocks()
            .into_iter()
            .map(|(_, heading)| heading)
            .collect();
        assert_eq!(
            headings,
            ["Columns (3)", "Indexes (1)", "Triggers (2)", "DDL"]
        );
        let tops = heading_tops(&description, &Inspector::for_width(1440));
        assert_eq!(tops[3].2 - tops[2].2, 28.0 + 72.0 + 8.0);
        let customers = describe(&window.tree, TreeItem::Table(0), None);
        assert!(customers.triggers.is_empty());
    }

    #[test]
    fn folded_blocks_move_the_next_heading_up() {
        let window = MainWindow::sample();
        let description = describe(&window.tree, TreeItem::Table(1), None);
        let mut inspector = Inspector::for_width(1440);
        let open = heading_tops(&description, &inspector);
        inspector.toggle_block(InspectorBlock::Columns);
        let folded = heading_tops(&description, &inspector);
        assert_eq!(open[0].2, folded[0].2);
        assert!(folded[1].2 < open[1].2);
        inspector.toggle_block(InspectorBlock::Columns);
        assert!(inspector.is_expanded(InspectorBlock::Columns));
    }

    #[test]
    fn other_rows_describe_themselves() {
        let window = MainWindow::sample();
        let tree = &window.tree;
        assert_eq!(
            describe(tree, TreeItem::Connection, None).subtitle,
            "Database · 3 tables · 1 view · 3 indexes · 1 key space"
        );
        assert_eq!(
            describe(tree, TreeItem::Index(1), None).subtitle,
            "Index on orders"
        );
        assert_eq!(
            describe(tree, TreeItem::KeySpace(0), None).subtitle,
            "Collection · 2 keys"
        );
        assert_eq!(
            describe(tree, TreeItem::Group(Group::Tables), None).subtitle,
            "3 tables"
        );
        assert_eq!(sampled_name(tree, TreeItem::Index(0)), None);
        assert_eq!(
            sampled_name(tree, TreeItem::Table(2)).as_deref(),
            Some("products")
        );
    }
}
