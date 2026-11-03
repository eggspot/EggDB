use eggdb_platform::window::{AccessNode, AccessRect, AccessRole, Key, KeyPress};

use crate::canvas::Rect;
use crate::dialect::QueryDialect;
use crate::font::Typeface;
use crate::grid::quote_name;
use crate::main_window::{BODY, Button, CODE, Painter, SMALL, elided, focus_ring};

const DIALOG_WIDTH: f32 = 420.0;
const DIALOG_HEIGHT: f32 = 192.0;
const STATEMENT_BOX: (f32, f32, f32, f32) = (24.0, 78.0, 372.0, 48.0);
const STATEMENT_TEXT_WIDTH: f32 = 356.0;
const STATEMENT_LINES: usize = 2;
const BUTTON_TOP: f32 = 144.0;
const BUTTONS_RIGHT: f32 = 396.0;
const BUTTON_GAP: f32 = 8.0;
const BUTTON_MIN: f32 = 72.0;
const LABEL_ADVANCE: f32 = 7.5;
const TITLE_STYLE: (f32, f32) = (15.0, 20.0);
const ERROR_LIMIT: f32 = 372.0;

/// The dialog's node.
pub const NODE_DROP_DIALOG: u32 = 64_400;
/// Its read-only statement box.
pub const NODE_DROP_STATEMENT: u32 = 64_401;
/// Its `Drop` button.
pub const NODE_DROP_CONFIRM: u32 = 64_402;
/// Its `Cancel` button.
pub const NODE_DROP_CANCEL: u32 = 64_403;

/// What a drop removes, with what its statement needs beyond the name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DropKind {
    /// A table.
    Table,
    /// A view.
    View,
    /// An index on `table`.
    Index {
        /// The table it indexes.
        table: String,
    },
    /// A sequence.
    Sequence,
    /// A trigger on `table`.
    Trigger {
        /// The table it fires on.
        table: String,
    },
}

impl DropKind {
    /// The word the title and toast use: `table`, `view`, `index`, `sequence`, `trigger`.
    pub fn word(&self) -> &'static str {
        match self {
            Self::Table => "table",
            Self::View => "view",
            Self::Index { .. } => "index",
            Self::Sequence => "sequence",
            Self::Trigger { .. } => "trigger",
        }
    }
}

/// The object a drop removes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropTarget {
    /// Its kind.
    pub kind: DropKind,
    /// Its bare name.
    pub name: String,
}

/// The statement that drops `target` in `dialect`, its names quoted as `Insert name` quotes
/// them (`docs/ux/workspace.md` 10.2): MySQL and SQL Server drop an index `ON` its table,
/// PostgreSQL drops a trigger `ON` its table.
pub fn drop_statement(target: &DropTarget, dialect: QueryDialect) -> String {
    let name = quote_name(&target.name, dialect);
    match &target.kind {
        DropKind::Table => format!("DROP TABLE {name};"),
        DropKind::View => format!("DROP VIEW {name};"),
        DropKind::Sequence => format!("DROP SEQUENCE {name};"),
        DropKind::Index { table }
            if matches!(dialect, QueryDialect::MySql | QueryDialect::SqlServer) =>
        {
            format!("DROP INDEX {name} ON {};", quote_name(table, dialect))
        }
        DropKind::Index { .. } => format!("DROP INDEX {name};"),
        DropKind::Trigger { table } if dialect == QueryDialect::PostgreSql => {
            format!("DROP TRIGGER {name} ON {};", quote_name(table, dialect))
        }
        DropKind::Trigger { .. } => format!("DROP TRIGGER {name};"),
    }
}

/// The body line while another tab keeps the drop from running: `tab` runs, or holds a
/// transaction.
pub fn blocked_body(tab: &str, running: bool) -> String {
    if running {
        format!("Wait for {tab} to finish first.")
    } else {
        format!("COMMIT or ROLLBACK in {tab} first.")
    }
}

/// The toast after a drop succeeded: `Dropped table orders`.
pub fn dropped_toast(target: &DropTarget) -> String {
    format!("Dropped {} {}", target.kind.word(), target.name)
}

/// The body line after a failed drop: `Error 2BP01: …`, or the message alone without a code.
pub fn error_line(code: &str, message: &str) -> String {
    if code.is_empty() {
        format!("Error: {message}")
    } else {
        format!("Error {code}: {message}")
    }
}

const BODY_TEXT: &str = "Runs at once in its own session; there is no undo.";

/// Where the dialog's focus is; `Tab` cycles them in this order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropFocus {
    /// The read-only statement box.
    Statement,
    /// The `Drop` button.
    Drop,
    /// The `Cancel` button, where the focus opens.
    Cancel,
}

/// What a key or click in the Drop dialog decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DropAnswer {
    /// The dialog stays open.
    Pending,
    /// Close it without dropping.
    Cancel,
    /// Run this statement in a session of its own.
    Drop(String),
}

/// The dialog that confirms a drop (`docs/ux/workspace.md` 10.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropPrompt {
    /// What it drops.
    pub target: DropTarget,
    /// The statement, in the active tab's dialect.
    pub statement: String,
    /// Why `Drop` is disabled: another tab runs or holds a transaction.
    pub blocked: Option<String>,
    /// The engine's error line after a failed try.
    pub error: Option<String>,
    /// Whether the statement was sent and its answer has not come back.
    pub dropping: bool,
    /// Where the focus is.
    pub focus: DropFocus,
}

impl DropPrompt {
    /// The dialog for `target` in `dialect`, `blocked` naming why it cannot drop yet.
    pub fn new(target: DropTarget, dialect: QueryDialect, blocked: Option<String>) -> Self {
        let statement = drop_statement(&target, dialect);
        Self {
            target,
            statement,
            blocked,
            error: None,
            dropping: false,
            focus: DropFocus::Cancel,
        }
    }

    /// `Drop table orders?`.
    pub fn title(&self) -> String {
        format!("Drop {} {}?", self.target.kind.word(), self.target.name)
    }

    /// The line under the title: the error, the reason it is blocked, or the warning.
    pub fn body(&self) -> &str {
        self.error
            .as_deref()
            .or(self.blocked.as_deref())
            .unwrap_or(BODY_TEXT)
    }

    /// Whether `Drop` acts.
    pub fn can_drop(&self) -> bool {
        self.blocked.is_none() && !self.dropping
    }

    /// The statement went out: `Drop` waits for its answer.
    pub fn press_drop(&mut self) -> DropAnswer {
        if !self.can_drop() {
            return DropAnswer::Pending;
        }
        self.dropping = true;
        self.error = None;
        DropAnswer::Drop(self.statement.clone())
    }

    /// The drop failed with `line`: the dialog stays with it and `Drop` acts again.
    pub fn failed(&mut self, line: String) {
        self.dropping = false;
        self.error = Some(line);
    }

    /// Answers a key: `Tab` cycles statement, `Drop`, `Cancel` (`Shift+Tab` back), `Enter`
    /// and `Space` press the focused button, `Esc` cancels.
    pub fn key(&mut self, press: KeyPress) -> DropAnswer {
        match press.key {
            Key::Escape => DropAnswer::Cancel,
            Key::Tab if press.modifiers.shift => {
                self.focus = match self.focus {
                    DropFocus::Statement => DropFocus::Cancel,
                    DropFocus::Drop => DropFocus::Statement,
                    DropFocus::Cancel => DropFocus::Drop,
                };
                DropAnswer::Pending
            }
            Key::Tab => {
                self.focus = match self.focus {
                    DropFocus::Statement => DropFocus::Drop,
                    DropFocus::Drop => DropFocus::Cancel,
                    DropFocus::Cancel => DropFocus::Statement,
                };
                DropAnswer::Pending
            }
            Key::Enter | Key::Space => match self.focus {
                DropFocus::Drop => self.press_drop(),
                DropFocus::Cancel => DropAnswer::Cancel,
                DropFocus::Statement => DropAnswer::Pending,
            },
            _ => DropAnswer::Pending,
        }
    }
}

/// The dialog's rect, centred in a window of `size`.
pub fn dialog_rect(size: (f32, f32)) -> Rect {
    Rect::new(
        ((size.0 - DIALOG_WIDTH) / 2.0).round(),
        ((size.1 - DIALOG_HEIGHT) / 2.0).round(),
        DIALOG_WIDTH,
        DIALOG_HEIGHT,
    )
}

/// The statement box's rect in the dialog at `dialog`.
pub fn statement_rect(dialog: Rect) -> Rect {
    let (x, y, w, h) = STATEMENT_BOX;
    Rect::new(dialog.x + x, dialog.y + y, w, h)
}

/// The `Drop` and `Cancel` buttons' rects in the dialog at `dialog`, `Cancel` rightmost.
pub fn button_rects(dialog: Rect) -> (Rect, Rect) {
    let width = |label: &str| (label.chars().count() as f32 * LABEL_ADVANCE + 24.0).max(BUTTON_MIN);
    let cancel_width = width("Cancel");
    let drop_width = width("Drop");
    let cancel_x = dialog.x + BUTTONS_RIGHT - cancel_width;
    let drop_x = cancel_x - BUTTON_GAP - drop_width;
    let top = dialog.y + BUTTON_TOP;
    (
        Rect::new(drop_x, top, drop_width, 28.0),
        Rect::new(cancel_x, top, cancel_width, 28.0),
    )
}

/// `text` broken per character into lines at most `width` wide in `face` at the code size,
/// at most `limit` lines, the last cut with `…` when more is left.
pub fn wrapped_per_character(
    face: &dyn Typeface,
    text: &str,
    width: f32,
    limit: usize,
) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();
    for letter in text.chars() {
        let mut candidate = line.clone();
        candidate.push(letter);
        if !line.is_empty() && face.width(&candidate, CODE.0) > width {
            lines.push(std::mem::take(&mut line));
            line.push(letter);
        } else {
            line = candidate;
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    if lines.len() > limit {
        lines.truncate(limit);
        if let Some(last) = lines.last_mut() {
            last.pop();
            last.push('…');
        }
    }
    lines
}

pub(crate) fn draw(paint: &mut Painter<'_, '_>, prompt: &DropPrompt, size: (f32, f32)) {
    let theme = paint.theme;
    let faces = paint.faces;
    paint
        .canvas
        .fill_rect(Rect::new(0.0, 0.0, size.0, size.1), theme.scrim);
    let dialog = dialog_rect(size);
    paint.canvas.fill_rounded_rect(dialog, 8.0, theme.border);
    paint.canvas.fill_rounded_rect(
        Rect::new(
            dialog.x + 1.0,
            dialog.y + 1.0,
            dialog.w - 2.0,
            dialog.h - 2.0,
        ),
        7.0,
        theme.bg0,
    );
    paint.text(
        faces.ui_bold,
        &prompt.title(),
        dialog.x + 24.0,
        dialog.y + 20.0,
        TITLE_STYLE,
        theme.text_primary,
    );
    if prompt.error.is_some() {
        let line = elided(faces.ui, prompt.body(), ERROR_LIMIT, SMALL.0);
        paint.text(
            faces.ui,
            &line,
            dialog.x + 24.0,
            dialog.y + 54.0,
            SMALL,
            theme.error,
        );
    } else {
        let line = elided(faces.ui, prompt.body(), ERROR_LIMIT, BODY.0);
        paint.text(
            faces.ui,
            &line,
            dialog.x + 24.0,
            dialog.y + 52.0,
            BODY,
            theme.text_primary,
        );
    }
    let statement = statement_rect(dialog);
    paint.canvas.fill_rounded_rect(statement, 4.0, theme.border);
    paint.canvas.fill_rounded_rect(
        Rect::new(
            statement.x + 1.0,
            statement.y + 1.0,
            statement.w - 2.0,
            statement.h - 2.0,
        ),
        3.0,
        theme.bg1,
    );
    for (index, line) in wrapped_per_character(
        faces.mono,
        &prompt.statement,
        STATEMENT_TEXT_WIDTH,
        STATEMENT_LINES,
    )
    .iter()
    .enumerate()
    {
        paint.text(
            faces.mono,
            line,
            statement.x + 8.0,
            statement.y + 4.0 + 20.0 * index as f32,
            CODE,
            theme.text_primary,
        );
    }
    let (drop, cancel) = button_rects(dialog);
    paint.button(drop, Button::Secondary);
    let drop_color = if prompt.can_drop() {
        theme.error
    } else {
        theme.text_disabled
    };
    paint.text(
        faces.ui,
        "Drop",
        drop.x + 12.0,
        drop.y + 5.0,
        BODY,
        drop_color,
    );
    paint.button(cancel, Button::Primary);
    paint.text(
        faces.ui,
        "Cancel",
        cancel.x + 12.0,
        cancel.y + 5.0,
        BODY,
        theme.on_accent,
    );
    let focused = match prompt.focus {
        DropFocus::Statement => statement,
        DropFocus::Drop => drop,
        DropFocus::Cancel => cancel,
    };
    focus_ring(paint, focused);
}

pub(crate) fn nodes(
    nodes: &mut Vec<AccessNode>,
    prompt: &DropPrompt,
    parent: u32,
    size: (f32, f32),
) {
    let dialog = dialog_rect(size);
    let bounds = |rect: Rect| AccessRect {
        x: rect.x,
        y: rect.y,
        width: rect.w,
        height: rect.h,
    };
    nodes.push(AccessNode {
        id: NODE_DROP_DIALOG,
        parent: Some(parent),
        role: AccessRole::Group,
        name: prompt.title(),
        description: Some(prompt.body().to_owned()),
        bounds: bounds(dialog),
        ..AccessNode::default()
    });
    let mut statement = AccessNode {
        id: NODE_DROP_STATEMENT,
        parent: Some(NODE_DROP_DIALOG),
        role: AccessRole::TextArea,
        name: "Statement".to_owned(),
        value: Some(prompt.statement.clone()),
        bounds: bounds(statement_rect(dialog)),
        ..AccessNode::default()
    };
    statement.state.read_only = true;
    statement.state.focused = prompt.focus == DropFocus::Statement;
    nodes.push(statement);
    let (drop, cancel) = button_rects(dialog);
    for (id, label, rect, focus) in [
        (NODE_DROP_CONFIRM, "Drop", drop, DropFocus::Drop),
        (NODE_DROP_CANCEL, "Cancel", cancel, DropFocus::Cancel),
    ] {
        let mut button = AccessNode {
            id,
            parent: Some(NODE_DROP_DIALOG),
            role: AccessRole::Button,
            name: label.to_owned(),
            bounds: bounds(rect),
            ..AccessNode::default()
        };
        button.state.focused = prompt.focus == focus;
        button.state.default_button = focus == DropFocus::Cancel;
        button.state.disabled = id == NODE_DROP_CONFIRM && !prompt.can_drop();
        button.actions.invoke = !button.state.disabled;
        nodes.push(button);
    }
}

/// Whether node `id` is one of the dialog's.
pub fn is_drop_node(id: u32) -> bool {
    (NODE_DROP_DIALOG..=NODE_DROP_CANCEL).contains(&id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(kind: DropKind, name: &str) -> DropTarget {
        DropTarget {
            kind,
            name: name.to_owned(),
        }
    }

    fn press(key: Key, shift: bool) -> KeyPress {
        let mut press = KeyPress::plain(key);
        press.modifiers.shift = shift;
        press
    }

    #[test]
    fn each_dialect_drops_with_its_own_statement_and_quoting() {
        let index = target(
            DropKind::Index {
                table: "orders".to_owned(),
            },
            "orders_customer_id_idx",
        );
        let trigger = target(
            DropKind::Trigger {
                table: "orders".to_owned(),
            },
            "orders_audit",
        );
        assert_eq!(
            drop_statement(&target(DropKind::Table, "orders"), QueryDialect::SqlServer),
            "DROP TABLE orders;"
        );
        assert_eq!(
            drop_statement(&index, QueryDialect::MySql),
            "DROP INDEX orders_customer_id_idx ON orders;"
        );
        assert_eq!(
            drop_statement(&index, QueryDialect::PostgreSql),
            "DROP INDEX orders_customer_id_idx;"
        );
        assert_eq!(
            drop_statement(&trigger, QueryDialect::PostgreSql),
            "DROP TRIGGER orders_audit ON orders;"
        );
        assert_eq!(
            drop_statement(&trigger, QueryDialect::Oracle),
            "DROP TRIGGER \"orders_audit\";"
        );
        assert_eq!(
            drop_statement(&target(DropKind::View, "Order Totals"), QueryDialect::MySql),
            "DROP VIEW `Order Totals`;"
        );
        assert_eq!(
            drop_statement(
                &target(DropKind::Sequence, "Order Seq"),
                QueryDialect::SqlServer
            ),
            "DROP SEQUENCE [Order Seq];"
        );
    }

    #[test]
    fn the_dialog_opens_on_cancel_and_tab_cycles_statement_drop_cancel() {
        let mut prompt = DropPrompt::new(
            target(DropKind::Table, "orders"),
            QueryDialect::PostgreSql,
            None,
        );
        assert_eq!(prompt.title(), "Drop table orders?");
        assert_eq!(prompt.body(), BODY_TEXT);
        assert_eq!(prompt.focus, DropFocus::Cancel);
        assert_eq!(prompt.key(press(Key::Tab, false)), DropAnswer::Pending);
        assert_eq!(prompt.focus, DropFocus::Statement);
        prompt.key(press(Key::Tab, false));
        assert_eq!(prompt.focus, DropFocus::Drop);
        prompt.key(press(Key::Tab, true));
        assert_eq!(prompt.focus, DropFocus::Statement);
        prompt.key(press(Key::Tab, false));
        assert_eq!(
            prompt.key(press(Key::Enter, false)),
            DropAnswer::Drop("DROP TABLE orders;".to_owned())
        );
        assert_eq!(prompt.key(press(Key::Enter, false)), DropAnswer::Pending);
        prompt.failed(error_line("2BP01", "cannot drop table orders"));
        assert_eq!(prompt.body(), "Error 2BP01: cannot drop table orders");
        assert!(prompt.can_drop());
        assert_eq!(prompt.key(press(Key::Escape, false)), DropAnswer::Cancel);
    }

    #[test]
    fn another_tab_running_or_in_a_transaction_disables_drop() {
        let mut prompt = DropPrompt::new(
            target(DropKind::Table, "orders"),
            QueryDialect::PostgreSql,
            Some(blocked_body("Query 2", false)),
        );
        assert_eq!(prompt.body(), "COMMIT or ROLLBACK in Query 2 first.");
        prompt.focus = DropFocus::Drop;
        assert_eq!(prompt.key(press(Key::Enter, false)), DropAnswer::Pending);
        assert_eq!(
            blocked_body("Query 3", true),
            "Wait for Query 3 to finish first."
        );
        assert_eq!(
            dropped_toast(&target(
                DropKind::Trigger {
                    table: "t".to_owned()
                },
                "g"
            )),
            "Dropped trigger g"
        );
        assert_eq!(
            dialog_rect((1280.0, 800.0)),
            Rect::new(430.0, 304.0, 420.0, 192.0)
        );
    }
}
