//! The pure parts of the AT-SPI2 bridge on Linux and the BSDs, compiled and tested on every
//! operating system: the object paths and references the bridge answers with, each
//! [`AccessRole`]'s AT-SPI role and each node's state set, the answers to the
//! `org.a11y.atspi.Accessible`, `Component`, `Application`, `Action`, `Text`, `EditableText`,
//! `Selection`, `Table` and `TableCell` methods and the D-Bus `Properties` calls a screen
//! reader such as Orca makes, the requests those calls queue for the window
//! ([`AtspiApplication::take_requests`]), and the `Object:StateChanged`, `TextChanged`,
//! `TextCaretMoved`, `TextSelectionChanged`, `SelectionChanged`, `PropertyChange`, `Focus`,
//! `ChildrenChanged` and `Window` events raised when the tree or the window's focus changes.
//! The values come from the public AT-SPI2 D-Bus interface descriptions; the socket that
//! carries the messages is the caller's (D256, D278).
//!
//! The application is the root object; the window's own node becomes its frame, because no
//! other part of the system exposes an X11 window to AT-SPI2.

use super::uia::is_keyboard_focusable;
use super::{
    AccessAction, AccessCaret, AccessNode, AccessRect, AccessRequest, AccessRole,
    MAX_ACCESS_VALUE_CHARS, access_request, focused_node,
};
use crate::dbus::{Message, MessageKind, NO_REPLY_EXPECTED, Value};

/// The session bus name of the service that knows the accessibility bus's address.
pub const ACCESSIBILITY_BUS_NAME: &str = "org.a11y.Bus";
/// The object path of that service.
pub const ACCESSIBILITY_BUS_PATH: &str = "/org/a11y/bus";
/// The accessibility bus name of the registry applications embed themselves in.
pub const REGISTRY_NAME: &str = "org.a11y.atspi.Registry";
/// The path of an application's root object, and of the registry's desktop.
pub const ROOT_PATH: &str = "/org/a11y/atspi/accessible/root";
/// The path of the null object reference.
pub const NULL_PATH: &str = "/org/a11y/atspi/null";
/// `org.a11y.atspi.Accessible`.
pub const ACCESSIBLE_INTERFACE: &str = "org.a11y.atspi.Accessible";
/// `org.a11y.atspi.Component`.
pub const COMPONENT_INTERFACE: &str = "org.a11y.atspi.Component";
/// `org.a11y.atspi.Application`.
pub const APPLICATION_INTERFACE: &str = "org.a11y.atspi.Application";
/// `org.a11y.atspi.Action`, offered by nodes a screen reader may activate, toggle or expand.
pub const ACTION_INTERFACE: &str = "org.a11y.atspi.Action";
/// `org.a11y.atspi.Text`, offered by text fields and text areas.
pub const TEXT_INTERFACE: &str = "org.a11y.atspi.Text";
/// `org.a11y.atspi.EditableText`, offered by text nodes whose value can be replaced.
pub const EDITABLE_TEXT_INTERFACE: &str = "org.a11y.atspi.EditableText";
/// `org.a11y.atspi.Selection`, offered by tab lists, trees and grids.
pub const SELECTION_INTERFACE: &str = "org.a11y.atspi.Selection";
/// `org.a11y.atspi.Table`, offered by grids that know their size.
pub const TABLE_INTERFACE: &str = "org.a11y.atspi.Table";
/// `org.a11y.atspi.TableCell`, offered by cells that know where they are.
pub const TABLE_CELL_INTERFACE: &str = "org.a11y.atspi.TableCell";
/// The most screen-reader requests kept until the window takes them; later ones are refused.
pub const MAX_QUEUED_REQUESTS: usize = 64;
/// `org.a11y.atspi.Socket`, whose `Embed` registers an application.
pub const SOCKET_INTERFACE: &str = "org.a11y.atspi.Socket";
/// `org.freedesktop.DBus.Properties`.
pub const PROPERTIES_INTERFACE: &str = "org.freedesktop.DBus.Properties";
/// The AT-SPI2 version the bridge speaks.
pub const ATSPI_VERSION: &str = "2.1";

const NODE_PATH_PREFIX: &str = "/org/a11y/atspi/accessible/";
const INTROSPECTABLE_INTERFACE: &str = "org.freedesktop.DBus.Introspectable";
const PEER_INTERFACE: &str = "org.freedesktop.DBus.Peer";
const OBJECT_EVENTS: &str = "org.a11y.atspi.Event.Object";
const FOCUS_EVENTS: &str = "org.a11y.atspi.Event.Focus";
const WINDOW_EVENTS: &str = "org.a11y.atspi.Event.Window";
const TOOLKIT_NAME: &str = "EggDB";

const UNKNOWN_METHOD: &str = "org.freedesktop.DBus.Error.UnknownMethod";
const UNKNOWN_OBJECT: &str = "org.freedesktop.DBus.Error.UnknownObject";
const UNKNOWN_INTERFACE: &str = "org.freedesktop.DBus.Error.UnknownInterface";
const UNKNOWN_PROPERTY: &str = "org.freedesktop.DBus.Error.UnknownProperty";
const PROPERTY_READ_ONLY: &str = "org.freedesktop.DBus.Error.PropertyReadOnly";
const INVALID_ARGS: &str = "org.freedesktop.DBus.Error.InvalidArgs";

/// `ATSPI_ROLE_ALERT`.
pub const ROLE_ALERT: u32 = 2;
/// `ATSPI_ROLE_CHECK_BOX`.
pub const ROLE_CHECK_BOX: u32 = 7;
/// `ATSPI_ROLE_COMBO_BOX`.
pub const ROLE_COMBO_BOX: u32 = 11;
/// `ATSPI_ROLE_FRAME`.
pub const ROLE_FRAME: u32 = 23;
/// `ATSPI_ROLE_PAGE_TAB`.
pub const ROLE_PAGE_TAB: u32 = 37;
/// `ATSPI_ROLE_PAGE_TAB_LIST`.
pub const ROLE_PAGE_TAB_LIST: u32 = 38;
/// `ATSPI_ROLE_PANEL`.
pub const ROLE_PANEL: u32 = 39;
/// `ATSPI_ROLE_PUSH_BUTTON`.
pub const ROLE_PUSH_BUTTON: u32 = 43;
/// `ATSPI_ROLE_STATUS_BAR`.
pub const ROLE_STATUS_BAR: u32 = 54;
/// `ATSPI_ROLE_TABLE`.
pub const ROLE_TABLE: u32 = 55;
/// `ATSPI_ROLE_TABLE_CELL`.
pub const ROLE_TABLE_CELL: u32 = 56;
/// `ATSPI_ROLE_TABLE_COLUMN_HEADER`.
pub const ROLE_TABLE_COLUMN_HEADER: u32 = 57;
/// `ATSPI_ROLE_TEXT`.
pub const ROLE_TEXT: u32 = 61;
/// `ATSPI_ROLE_TOOL_BAR`.
pub const ROLE_TOOL_BAR: u32 = 63;
/// `ATSPI_ROLE_TREE`.
pub const ROLE_TREE: u32 = 65;
/// `ATSPI_ROLE_APPLICATION`.
pub const ROLE_APPLICATION: u32 = 75;
/// `ATSPI_ROLE_ENTRY`.
pub const ROLE_ENTRY: u32 = 79;
/// `ATSPI_ROLE_HEADING`.
pub const ROLE_HEADING: u32 = 83;
/// `ATSPI_ROLE_LINK`.
pub const ROLE_LINK: u32 = 88;
/// `ATSPI_ROLE_TREE_ITEM`.
pub const ROLE_TREE_ITEM: u32 = 91;

/// `ATSPI_STATE_ACTIVE`.
pub const STATE_ACTIVE: u32 = 1;
/// `ATSPI_STATE_CHECKED`.
pub const STATE_CHECKED: u32 = 4;
/// `ATSPI_STATE_COLLAPSED`.
pub const STATE_COLLAPSED: u32 = 5;
/// `ATSPI_STATE_EDITABLE`.
pub const STATE_EDITABLE: u32 = 7;
/// `ATSPI_STATE_ENABLED`.
pub const STATE_ENABLED: u32 = 8;
/// `ATSPI_STATE_EXPANDABLE`.
pub const STATE_EXPANDABLE: u32 = 9;
/// `ATSPI_STATE_EXPANDED`.
pub const STATE_EXPANDED: u32 = 10;
/// `ATSPI_STATE_FOCUSABLE`.
pub const STATE_FOCUSABLE: u32 = 11;
/// `ATSPI_STATE_FOCUSED`.
pub const STATE_FOCUSED: u32 = 12;
/// `ATSPI_STATE_MULTI_LINE`.
pub const STATE_MULTI_LINE: u32 = 17;
/// `ATSPI_STATE_SELECTABLE`.
pub const STATE_SELECTABLE: u32 = 22;
/// `ATSPI_STATE_SELECTED`.
pub const STATE_SELECTED: u32 = 23;
/// `ATSPI_STATE_SENSITIVE`.
pub const STATE_SENSITIVE: u32 = 24;
/// `ATSPI_STATE_SHOWING`.
pub const STATE_SHOWING: u32 = 25;
/// `ATSPI_STATE_SINGLE_LINE`.
pub const STATE_SINGLE_LINE: u32 = 26;
/// `ATSPI_STATE_VISIBLE`.
pub const STATE_VISIBLE: u32 = 30;
/// `ATSPI_STATE_INVALID_ENTRY`.
pub const STATE_INVALID_ENTRY: u32 = 36;
/// `ATSPI_STATE_SELECTABLE_TEXT`.
pub const STATE_SELECTABLE_TEXT: u32 = 38;
/// `ATSPI_STATE_IS_DEFAULT`.
pub const STATE_IS_DEFAULT: u32 = 39;
/// `ATSPI_STATE_CHECKABLE`.
pub const STATE_CHECKABLE: u32 = 41;
/// `ATSPI_STATE_HAS_POPUP`.
pub const STATE_HAS_POPUP: u32 = 42;
/// `ATSPI_STATE_READ_ONLY`.
pub const STATE_READ_ONLY: u32 = 43;

/// `ATSPI_TEXT_GRANULARITY_CHAR`.
pub const GRANULARITY_CHAR: u32 = 0;
/// `ATSPI_TEXT_GRANULARITY_WORD`.
pub const GRANULARITY_WORD: u32 = 1;
/// `ATSPI_TEXT_GRANULARITY_SENTENCE`, answered as a line.
pub const GRANULARITY_SENTENCE: u32 = 2;
/// `ATSPI_TEXT_GRANULARITY_LINE`.
pub const GRANULARITY_LINE: u32 = 3;
/// `ATSPI_TEXT_GRANULARITY_PARAGRAPH`, answered as a line.
pub const GRANULARITY_PARAGRAPH: u32 = 4;

/// `ATSPI_TEXT_BOUNDARY_CHAR`.
pub const BOUNDARY_CHAR: u32 = 0;
/// `ATSPI_TEXT_BOUNDARY_WORD_START`.
pub const BOUNDARY_WORD_START: u32 = 1;
/// `ATSPI_TEXT_BOUNDARY_WORD_END`.
pub const BOUNDARY_WORD_END: u32 = 2;
/// `ATSPI_TEXT_BOUNDARY_SENTENCE_START`, answered as a line.
pub const BOUNDARY_SENTENCE_START: u32 = 3;
/// `ATSPI_TEXT_BOUNDARY_SENTENCE_END`, answered as a line.
pub const BOUNDARY_SENTENCE_END: u32 = 4;
/// `ATSPI_TEXT_BOUNDARY_LINE_START`.
pub const BOUNDARY_LINE_START: u32 = 5;
/// `ATSPI_TEXT_BOUNDARY_LINE_END`.
pub const BOUNDARY_LINE_END: u32 = 6;

/// `ATSPI_COORD_TYPE_SCREEN`.
pub const COORD_SCREEN: u32 = 0;
/// `ATSPI_COORD_TYPE_WINDOW`.
pub const COORD_WINDOW: u32 = 1;
/// `ATSPI_COORD_TYPE_PARENT`.
pub const COORD_PARENT: u32 = 2;

/// `ATSPI_LAYER_WIDGET`.
pub const LAYER_WIDGET: u32 = 3;
/// `ATSPI_LAYER_WINDOW`.
pub const LAYER_WINDOW: u32 = 7;

/// The AT-SPI role of a node.
pub fn role(role: AccessRole) -> u32 {
    match role {
        AccessRole::Window => ROLE_FRAME,
        AccessRole::Group | AccessRole::TabPanel => ROLE_PANEL,
        AccessRole::Heading => ROLE_HEADING,
        AccessRole::TabList => ROLE_PAGE_TAB_LIST,
        AccessRole::Tab => ROLE_PAGE_TAB,
        AccessRole::Status => ROLE_STATUS_BAR,
        AccessRole::TextField => ROLE_ENTRY,
        AccessRole::Button => ROLE_PUSH_BUTTON,
        AccessRole::CheckBox => ROLE_CHECK_BOX,
        AccessRole::Link => ROLE_LINK,
        AccessRole::Toolbar => ROLE_TOOL_BAR,
        AccessRole::ComboBox => ROLE_COMBO_BOX,
        AccessRole::TextArea => ROLE_TEXT,
        AccessRole::Alert => ROLE_ALERT,
        AccessRole::Tree => ROLE_TREE,
        AccessRole::TreeItem => ROLE_TREE_ITEM,
        AccessRole::Grid => ROLE_TABLE,
        AccessRole::ColumnHeader => ROLE_TABLE_COLUMN_HEADER,
        AccessRole::Cell => ROLE_TABLE_CELL,
    }
}

/// The name `GetRoleName` gives a node's role.
pub fn role_name(role: AccessRole) -> &'static str {
    match role {
        AccessRole::Window => "frame",
        AccessRole::Group | AccessRole::TabPanel => "panel",
        AccessRole::Heading => "heading",
        AccessRole::TabList => "page tab list",
        AccessRole::Tab => "page tab",
        AccessRole::Status => "status bar",
        AccessRole::TextField => "entry",
        AccessRole::Button => "push button",
        AccessRole::CheckBox => "check box",
        AccessRole::Link => "link",
        AccessRole::Toolbar => "tool bar",
        AccessRole::ComboBox => "combo box",
        AccessRole::TextArea => "text",
        AccessRole::Alert => "alert",
        AccessRole::Tree => "tree",
        AccessRole::TreeItem => "tree item",
        AccessRole::Grid => "table",
        AccessRole::ColumnHeader => "table column header",
        AccessRole::Cell => "table cell",
    }
}

fn is_text_role(role: AccessRole) -> bool {
    matches!(role, AccessRole::TextField | AccessRole::TextArea)
}

fn is_focusable(node: &AccessNode) -> bool {
    !node.state.disabled && (is_keyboard_focusable(node) || node.actions.focus)
}

fn is_selectable(node: &AccessNode) -> bool {
    node.actions.select
        || matches!(
            node.role,
            AccessRole::Tab | AccessRole::TreeItem | AccessRole::Cell
        )
}

fn is_selection_container(node: &AccessNode) -> bool {
    matches!(
        node.role,
        AccessRole::TabList | AccessRole::Tree | AccessRole::Grid
    )
}

fn has_text(node: &AccessNode) -> bool {
    is_text_role(node.role)
        && (node.value.is_some() || node.caret.is_some() || node.actions.set_value)
}

fn description(node: &AccessNode) -> &str {
    node.description
        .as_deref()
        .or(node.placeholder.as_deref())
        .unwrap_or_default()
}

/// The AT-SPI states of a node, in ascending order; `window_active` adds `ACTIVE` to the
/// window's own node.
pub fn states(node: &AccessNode, window_active: bool) -> Vec<u32> {
    let state = node.state;
    let mut states = vec![STATE_VISIBLE, STATE_SHOWING];
    if !state.disabled {
        states.extend([STATE_ENABLED, STATE_SENSITIVE]);
    }
    if is_focusable(node) {
        states.push(STATE_FOCUSABLE);
    }
    if state.focused {
        states.push(STATE_FOCUSED);
    }
    if node.role == AccessRole::Window && window_active {
        states.push(STATE_ACTIVE);
    }
    if node.role == AccessRole::CheckBox || state.checked.is_some() {
        states.push(STATE_CHECKABLE);
        if state.checked == Some(true) {
            states.push(STATE_CHECKED);
        }
    }
    if is_selectable(node) {
        states.push(STATE_SELECTABLE);
        if state.selected {
            states.push(STATE_SELECTED);
        }
    }
    match state.expanded {
        Some(true) => states.extend([STATE_EXPANDABLE, STATE_EXPANDED]),
        Some(false) => states.extend([STATE_EXPANDABLE, STATE_COLLAPSED]),
        None => {}
    }
    match node.role {
        AccessRole::TextField => states.push(STATE_SINGLE_LINE),
        AccessRole::TextArea => states.push(STATE_MULTI_LINE),
        AccessRole::ComboBox => states.push(STATE_HAS_POPUP),
        _ => {}
    }
    if is_text_role(node.role) {
        if !state.read_only {
            states.push(STATE_EDITABLE);
        }
        if node.caret.is_some() {
            states.push(STATE_SELECTABLE_TEXT);
        }
    }
    if state.read_only {
        states.push(STATE_READ_ONLY);
    }
    if state.invalid {
        states.push(STATE_INVALID_ENTRY);
    }
    if state.default_button {
        states.push(STATE_IS_DEFAULT);
    }
    states.sort_unstable();
    states.dedup();
    states
}

/// A state list as `GetState` returns it: state `n` is bit `n % 32` of word `n / 32`.
pub fn state_set(states: &[u32]) -> [u32; 2] {
    let mut words = [0u32; 2];
    for &state in states {
        if let Some(word) = words.get_mut((state / 32) as usize) {
            *word |= 1 << (state % 32);
        }
    }
    words
}

/// An object the bridge exposes: the application, or one node of the tree by id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Object {
    /// The application's root object.
    Application,
    /// The node with this id.
    Node(u32),
}

impl Object {
    /// The object's D-Bus path.
    pub fn path(self) -> String {
        match self {
            Self::Application => ROOT_PATH.to_owned(),
            Self::Node(id) => format!("{NODE_PATH_PREFIX}{id}"),
        }
    }

    /// The object a D-Bus path names, `None` for paths the bridge never hands out.
    pub fn from_path(path: &str) -> Option<Self> {
        if path == ROOT_PATH {
            return Some(Self::Application);
        }
        let digits = path.strip_prefix(NODE_PATH_PREFIX)?;
        if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        digits.parse().ok().map(Self::Node)
    }
}

/// An object reference `(so)`: a bus name and an object path.
pub fn reference(bus_name: &str, path: &str) -> Value {
    Value::Struct(vec![
        Value::String(bus_name.to_owned()),
        Value::ObjectPath(path.to_owned()),
    ])
}

/// The null object reference.
pub fn null_reference() -> Value {
    reference("", NULL_PATH)
}

fn parent_object(nodes: &[AccessNode], node: &AccessNode) -> Object {
    match node.parent {
        Some(parent) if parent != node.id && nodes.iter().any(|other| other.id == parent) => {
            Object::Node(parent)
        }
        _ => Object::Application,
    }
}

fn children(nodes: &[AccessNode], of: Object) -> Vec<u32> {
    nodes
        .iter()
        .filter(|node| parent_object(nodes, node) == of)
        .map(|node| node.id)
        .collect()
}

fn index_in_parent(nodes: &[AccessNode], node: &AccessNode) -> i32 {
    children(nodes, parent_object(nodes, node))
        .iter()
        .position(|&child| child == node.id)
        .and_then(|index| i32::try_from(index).ok())
        .unwrap_or(-1)
}

fn is_inside(nodes: &[AccessNode], node: &AccessNode, ancestor: u32) -> bool {
    let mut current = node;
    for _ in 0..=nodes.len() {
        if current.id == ancestor {
            return true;
        }
        let Object::Node(parent) = parent_object(nodes, current) else {
            return false;
        };
        let Some(next) = nodes.iter().find(|other| other.id == parent) else {
            return false;
        };
        current = next;
    }
    false
}

fn contains(bounds: AccessRect, x: f32, y: f32) -> bool {
    x >= bounds.x && y >= bounds.y && x < bounds.x + bounds.width && y < bounds.y + bounds.height
}

fn nearest_ancestor<'a>(
    nodes: &'a [AccessNode],
    node: &AccessNode,
    wanted: impl Fn(&AccessNode) -> bool,
) -> Option<&'a AccessNode> {
    let mut current = node;
    for _ in 0..nodes.len() {
        let Object::Node(parent) = parent_object(nodes, current) else {
            return None;
        };
        let next = nodes.iter().find(|other| other.id == parent)?;
        if wanted(next) {
            return Some(next);
        }
        current = next;
    }
    None
}

fn grid_of<'a>(nodes: &'a [AccessNode], node: &AccessNode) -> Option<&'a AccessNode> {
    nearest_ancestor(nodes, node, |ancestor| ancestor.role == AccessRole::Grid)
}

fn grid_members(
    nodes: &[AccessNode],
    grid: u32,
    role: AccessRole,
) -> impl Iterator<Item = &AccessNode> {
    nodes.iter().filter(move |node| {
        node.role == role
            && node.cell.is_some()
            && grid_of(nodes, node).is_some_and(|found| found.id == grid)
    })
}

fn selected_items(nodes: &[AccessNode], container: u32) -> Vec<u32> {
    nodes
        .iter()
        .filter(|node| {
            node.state.selected
                && is_selectable(node)
                && nearest_ancestor(nodes, node, is_selection_container)
                    .is_some_and(|found| found.id == container)
        })
        .map(|node| node.id)
        .collect()
}

fn selection_owner(nodes: &[AccessNode], node: &AccessNode) -> Object {
    nearest_ancestor(nodes, node, is_selection_container).map_or_else(
        || parent_object(nodes, node),
        |found| Object::Node(found.id),
    )
}

fn offset_value(offset: usize) -> Value {
    Value::Int32(i32::try_from(offset).unwrap_or(i32::MAX))
}

fn event_with_details(
    path: &str,
    interface: &str,
    member: &str,
    detail: &str,
    (detail1, detail2): (i32, i32),
    any: Value,
) -> Message {
    Message::signal(
        0,
        path,
        interface,
        member,
        vec![
            Value::String(detail.to_owned()),
            Value::Int32(detail1),
            Value::Int32(detail2),
            Value::Variant(Box::new(any)),
            Value::Array("{sv}".to_owned(), Vec::new()),
        ],
    )
}

fn event(
    path: &str,
    interface: &str,
    member: &str,
    detail: &str,
    detail1: i32,
    any: Value,
) -> Message {
    event_with_details(path, interface, member, detail, (detail1, 0), any)
}

fn state_changed(object: Object, state: &str, on: bool) -> Message {
    event(
        &object.path(),
        OBJECT_EVENTS,
        "StateChanged",
        state,
        i32::from(on),
        Value::Int32(0),
    )
}

fn property_changed(object: Object, property: &str, value: &str) -> Message {
    event(
        &object.path(),
        OBJECT_EVENTS,
        "PropertyChange",
        property,
        0,
        Value::String(value.to_owned()),
    )
}

fn text_changed(
    object: Object,
    kind: &str,
    chars: &[char],
    (start, end): (usize, usize),
) -> Message {
    let length = end.saturating_sub(start);
    let carried = text_slice(
        chars,
        start,
        end.min(start.saturating_add(MAX_ACCESS_VALUE_CHARS)),
    );
    let to_i32 = |number: usize| i32::try_from(number).unwrap_or(i32::MAX);
    event_with_details(
        &object.path(),
        OBJECT_EVENTS,
        "TextChanged",
        kind,
        (to_i32(start), to_i32(length)),
        Value::String(carried),
    )
}

fn text_changed_events(object: Object, old: &str, new: &str) -> Vec<Message> {
    let old: Vec<char> = old.chars().collect();
    let new: Vec<char> = new.chars().collect();
    let prefix = old
        .iter()
        .zip(&new)
        .take_while(|(before, after)| before == after)
        .count();
    let room = old.len().min(new.len()).saturating_sub(prefix);
    let suffix = old
        .iter()
        .rev()
        .zip(new.iter().rev())
        .take(room)
        .take_while(|(before, after)| before == after)
        .count();
    let deleted = (prefix, old.len().saturating_sub(suffix));
    let inserted = (prefix, new.len().saturating_sub(suffix));
    let mut events = Vec::new();
    if deleted.1 > deleted.0 {
        events.push(text_changed(object, "delete", &old, deleted));
    }
    if inserted.1 > inserted.0 {
        events.push(text_changed(object, "insert", &new, inserted));
    }
    events
}

fn caret_events(object: Object, before: Option<AccessCaret>, now: AccessCaret) -> Vec<Message> {
    let mut events = Vec::new();
    if before.map(|caret| caret.caret) != Some(now.caret) {
        events.push(event(
            &object.path(),
            OBJECT_EVENTS,
            "TextCaretMoved",
            "",
            i32::try_from(now.caret).unwrap_or(i32::MAX),
            Value::Int32(0),
        ));
    }
    let is_empty = |(start, end): (usize, usize)| start == end;
    let (was, range) = (before.map(AccessCaret::range), now.range());
    if was != Some(range) && !(was.is_none_or(is_empty) && is_empty(range)) {
        events.push(event(
            &object.path(),
            OBJECT_EVENTS,
            "TextSelectionChanged",
            "",
            0,
            Value::Int32(0),
        ));
    }
    events
}

fn node_change_events(before: &AccessNode, node: &AccessNode) -> Vec<Message> {
    let object = Object::Node(node.id);
    let mut events = Vec::new();
    if before.name != node.name {
        events.push(property_changed(object, "accessible-name", &node.name));
    }
    if description(before) != description(node) {
        events.push(property_changed(
            object,
            "accessible-description",
            description(node),
        ));
    }
    if before.value != node.value {
        let value = node.value.as_deref().unwrap_or_default();
        if is_text_role(node.role) {
            events.extend(text_changed_events(
                object,
                before.value.as_deref().unwrap_or_default(),
                value,
            ));
        } else {
            events.push(property_changed(object, "accessible-value", value));
        }
    }
    let (was, now) = (before.state, node.state);
    if was.checked != now.checked {
        events.push(state_changed(object, "checked", now.checked == Some(true)));
    }
    if was.selected != now.selected {
        events.push(state_changed(object, "selected", now.selected));
    }
    if was.expanded.is_some() != now.expanded.is_some() {
        events.push(state_changed(object, "expandable", now.expanded.is_some()));
    }
    if (was.expanded == Some(true)) != (now.expanded == Some(true)) {
        events.push(state_changed(
            object,
            "expanded",
            now.expanded == Some(true),
        ));
    }
    if was.invalid != now.invalid {
        events.push(state_changed(object, "invalid-entry", now.invalid));
    }
    if was.read_only != now.read_only {
        events.push(state_changed(object, "read-only", now.read_only));
    }
    if is_text_role(node.role)
        && let Some(caret) = node.caret
    {
        events.extend(caret_events(object, before.caret, caret));
    }
    events
}

#[derive(Debug)]
struct CallError {
    name: &'static str,
    text: String,
}

impl CallError {
    fn new(name: &'static str, text: impl Into<String>) -> Self {
        Self {
            name,
            text: text.into(),
        }
    }

    fn unknown_method(interface: &str, member: &str) -> Self {
        Self::new(
            UNKNOWN_METHOD,
            format!("no method {interface}.{member} here"),
        )
    }
}

fn argument(body: &[Value], index: usize) -> Result<&Value, CallError> {
    body.get(index)
        .ok_or_else(|| CallError::new(INVALID_ARGS, format!("argument {index} is missing")))
}

fn i32_argument(body: &[Value], index: usize) -> Result<i32, CallError> {
    match argument(body, index)? {
        Value::Int32(number) => Ok(*number),
        _ => Err(CallError::new(
            INVALID_ARGS,
            format!("argument {index} is not an int32"),
        )),
    }
}

fn u32_argument(body: &[Value], index: usize) -> Result<u32, CallError> {
    argument(body, index)?
        .as_u32()
        .ok_or_else(|| CallError::new(INVALID_ARGS, format!("argument {index} is not a uint32")))
}

fn str_argument(body: &[Value], index: usize) -> Result<&str, CallError> {
    match argument(body, index)? {
        Value::String(text) => Ok(text),
        _ => Err(CallError::new(
            INVALID_ARGS,
            format!("argument {index} is not a string"),
        )),
    }
}

fn text_slice(chars: &[char], start: usize, end: usize) -> String {
    chars
        .get(start..end)
        .map(|slice| slice.iter().collect())
        .unwrap_or_default()
}

fn offset_from_start(offset: i32, length: usize) -> usize {
    usize::try_from(offset).map_or(0, |offset| offset.min(length))
}

fn offset_to_end(offset: i32, length: usize) -> usize {
    usize::try_from(offset).map_or(length, |offset| offset.min(length))
}

fn is_word_char(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

fn word_range(chars: &[char], at: usize) -> (usize, usize) {
    let length = chars.len();
    let at = at.min(length);
    let is_word = |index: usize| chars.get(index).copied().is_some_and(is_word_char);
    let inside = if is_word(at) {
        Some(at)
    } else {
        (0..at)
            .rev()
            .find(|&index| is_word(index))
            .or_else(|| (at..length).find(|&index| is_word(index)))
    };
    let Some(inside) = inside else {
        return (at, at);
    };
    let start = (0..inside)
        .rev()
        .take_while(|&index| is_word(index))
        .last()
        .unwrap_or(inside);
    let end = (inside..length)
        .find(|&index| !is_word(index))
        .unwrap_or(length);
    (start, end)
}

fn line_range(chars: &[char], at: usize) -> (usize, usize) {
    let length = chars.len();
    let at = at.min(length);
    let is_break = |index: usize| chars.get(index) == Some(&'\n');
    let start = (0..at)
        .rev()
        .find(|&index| is_break(index))
        .map_or(0, |index| index + 1);
    let end = (at..length)
        .find(|&index| is_break(index))
        .unwrap_or(length);
    (start, end)
}

fn char_range(chars: &[char], at: usize) -> (usize, usize) {
    let at = at.min(chars.len());
    (at, at.saturating_add(1).min(chars.len()))
}

fn granularity_range(
    chars: &[char],
    at: usize,
    granularity: u32,
) -> Result<(usize, usize), CallError> {
    match granularity {
        GRANULARITY_CHAR => Ok(char_range(chars, at)),
        GRANULARITY_WORD => Ok(word_range(chars, at)),
        GRANULARITY_SENTENCE | GRANULARITY_LINE | GRANULARITY_PARAGRAPH => {
            Ok(line_range(chars, at))
        }
        _ => Err(CallError::new(
            INVALID_ARGS,
            format!("no text granularity {granularity}"),
        )),
    }
}

fn boundary_range(chars: &[char], at: usize, boundary: u32) -> Result<(usize, usize), CallError> {
    let length = chars.len();
    let is_word = |index: usize| chars.get(index).copied().is_some_and(is_word_char);
    match boundary {
        BOUNDARY_CHAR => Ok(char_range(chars, at)),
        BOUNDARY_WORD_START => {
            let (start, end) = word_range(chars, at);
            let next = (end..length)
                .find(|&index| is_word(index))
                .unwrap_or(length);
            Ok((start, next))
        }
        BOUNDARY_WORD_END => {
            let (start, end) = word_range(chars, at);
            let previous = (0..start)
                .rev()
                .find(|&index| is_word(index))
                .map_or(0, |index| index + 1);
            Ok((previous, end))
        }
        BOUNDARY_SENTENCE_START | BOUNDARY_LINE_START => {
            let (start, end) = line_range(chars, at);
            Ok((start, if end < length { end + 1 } else { end }))
        }
        BOUNDARY_SENTENCE_END | BOUNDARY_LINE_END => {
            let (start, end) = line_range(chars, at);
            Ok((start.saturating_sub(1), end))
        }
        _ => Err(CallError::new(
            INVALID_ARGS,
            format!("no text boundary type {boundary}"),
        )),
    }
}

fn text_answer(chars: &[char], (start, end): (usize, usize)) -> Vec<Value> {
    vec![
        Value::String(text_slice(chars, start, end)),
        offset_value(start),
        offset_value(end),
    ]
}

fn byte_prefix(text: &str, length: i32) -> &str {
    let Ok(limit) = usize::try_from(length) else {
        return text;
    };
    if limit >= text.len() {
        return text;
    }
    (0..=limit)
        .rev()
        .find(|&index| text.is_char_boundary(index))
        .and_then(|index| text.get(..index))
        .unwrap_or_default()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodeAction {
    Click,
    Toggle,
    ExpandOrContract,
}

impl NodeAction {
    fn name(self) -> &'static str {
        match self {
            Self::Click => "click",
            Self::Toggle => "toggle",
            Self::ExpandOrContract => "expand or contract",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Click => "Activates it",
            Self::Toggle => "Turns it on or off",
            Self::ExpandOrContract => "Shows or hides what it holds",
        }
    }

    fn key_binding(self, node: &AccessNode) -> &str {
        match self {
            Self::Click => node.shortcut.as_deref().unwrap_or_default(),
            Self::Toggle | Self::ExpandOrContract => "",
        }
    }

    fn request(self, node: &AccessNode) -> AccessAction {
        match self {
            Self::Click => AccessAction::Invoke,
            Self::Toggle => AccessAction::Toggle,
            Self::ExpandOrContract if node.state.expanded == Some(true) => AccessAction::Collapse,
            Self::ExpandOrContract => AccessAction::Expand,
        }
    }
}

fn node_actions(node: &AccessNode) -> Vec<NodeAction> {
    let offered = [
        (node.actions.invoke, NodeAction::Click),
        (node.actions.toggle, NodeAction::Toggle),
        (node.actions.expand_collapse, NodeAction::ExpandOrContract),
    ];
    offered
        .into_iter()
        .filter(|(offers, _)| *offers)
        .map(|(_, action)| action)
        .collect()
}

fn node_interfaces(node: &AccessNode) -> Vec<&'static str> {
    let mut interfaces = vec![ACCESSIBLE_INTERFACE, COMPONENT_INTERFACE];
    if !node_actions(node).is_empty() {
        interfaces.push(ACTION_INTERFACE);
    }
    if has_text(node) {
        interfaces.push(TEXT_INTERFACE);
        if node.actions.set_value {
            interfaces.push(EDITABLE_TEXT_INTERFACE);
        }
    }
    if is_selection_container(node) {
        interfaces.push(SELECTION_INTERFACE);
    }
    if node.role == AccessRole::Grid && node.table.is_some() {
        interfaces.push(TABLE_INTERFACE);
    }
    if node.role == AccessRole::Cell && node.cell.is_some() {
        interfaces.push(TABLE_CELL_INTERFACE);
    }
    interfaces
}

fn node_chars(node: &AccessNode) -> Vec<char> {
    node.value.as_deref().unwrap_or_default().chars().collect()
}

fn set_caret(anchor: usize, caret: usize) -> AccessAction {
    AccessAction::SetCaret(AccessCaret { anchor, caret })
}

fn text_request(
    node: &AccessNode,
    member: &str,
    body: &[Value],
) -> Result<Option<AccessAction>, CallError> {
    let length = node_chars(node).len();
    let selection = node
        .caret
        .map(AccessCaret::range)
        .filter(|(start, end)| start != end);
    Ok(match member {
        "SetCaretOffset" => {
            let offset = offset_from_start(i32_argument(body, 0)?, length);
            Some(set_caret(offset, offset))
        }
        "SetSelection" => {
            let index = i32_argument(body, 0)?;
            let start = offset_from_start(i32_argument(body, 1)?, length);
            let end = offset_to_end(i32_argument(body, 2)?, length);
            (index == 0).then(|| set_caret(start, end))
        }
        "AddSelection" => {
            let start = offset_from_start(i32_argument(body, 0)?, length);
            let end = offset_to_end(i32_argument(body, 1)?, length);
            selection.is_none().then(|| set_caret(start, end))
        }
        "RemoveSelection" => {
            let index = i32_argument(body, 0)?;
            let caret = node.caret.map_or(0, |caret| caret.caret.min(length));
            (index == 0 && selection.is_some()).then(|| set_caret(caret, caret))
        }
        _ => return Err(CallError::unknown_method(TEXT_INTERFACE, member)),
    })
}

fn editable_text_request(
    node: &AccessNode,
    member: &str,
    body: &[Value],
) -> Result<Option<AccessAction>, CallError> {
    let chars = node_chars(node);
    let length = chars.len();
    Ok(match member {
        "SetTextContents" => Some(AccessAction::SetValue(str_argument(body, 0)?.to_owned())),
        "InsertText" => {
            let position = offset_from_start(i32_argument(body, 0)?, length);
            let inserted = byte_prefix(str_argument(body, 1)?, i32_argument(body, 2)?);
            if length.saturating_add(inserted.chars().count()) > MAX_ACCESS_VALUE_CHARS {
                return Ok(None);
            }
            let mut value = text_slice(&chars, 0, position);
            value.push_str(inserted);
            value.push_str(&text_slice(&chars, position, length));
            Some(AccessAction::SetValue(value))
        }
        "DeleteText" => {
            let start = offset_from_start(i32_argument(body, 0)?, length);
            let end = offset_to_end(i32_argument(body, 1)?, length).max(start);
            let mut value = text_slice(&chars, 0, start);
            value.push_str(&text_slice(&chars, end, length));
            Some(AccessAction::SetValue(value))
        }
        "CutText" | "PasteText" => None,
        _ => return Err(CallError::unknown_method(EDITABLE_TEXT_INTERFACE, member)),
    })
}

/// The application as AT-SPI2 sees it: the accessibility tree, the unique bus name its
/// objects live at, the desktop it is embedded in and the window's place on the screen. It
/// answers method calls and turns tree changes into events; [`Message`]s it returns carry
/// serial 0 for the connection to number.
#[derive(Debug, Clone)]
pub struct AtspiApplication {
    unique_name: String,
    name: String,
    nodes: Vec<AccessNode>,
    desktop: Option<(String, String)>,
    id: i32,
    scale: f32,
    origin: (i32, i32),
    window_active: bool,
    requests: Vec<AccessRequest>,
}

impl AtspiApplication {
    /// The application `name` whose connection to the accessibility bus is `unique_name`.
    pub fn new(unique_name: &str, name: &str) -> Self {
        Self {
            unique_name: unique_name.to_owned(),
            name: name.to_owned(),
            nodes: Vec::new(),
            desktop: None,
            id: 0,
            scale: 1.0,
            origin: (0, 0),
            window_active: false,
            requests: Vec::new(),
        }
    }

    /// The requests screen readers made since the last call, oldest first; at most
    /// [`MAX_QUEUED_REQUESTS`] are kept.
    pub fn take_requests(&mut self) -> Vec<AccessRequest> {
        std::mem::take(&mut self.requests)
    }

    fn queue(&mut self, id: u32, action: Option<AccessAction>) -> Value {
        let request = action.and_then(|action| access_request(&self.nodes, id, action));
        let queued = match request {
            Some(request) if self.requests.len() < MAX_QUEUED_REQUESTS => {
                self.requests.push(request);
                true
            }
            _ => false,
        };
        Value::Boolean(queued)
    }

    /// The `Socket.Embed` call that registers the application with the registry.
    pub fn embed_call(&self) -> Message {
        Message::method_call(
            0,
            REGISTRY_NAME,
            ROOT_PATH,
            SOCKET_INTERFACE,
            "Embed",
            vec![reference(&self.unique_name, ROOT_PATH)],
        )
    }

    /// Takes the registry's reply to `Embed`: the desktop, which becomes the application's
    /// parent. A reply of another shape is ignored.
    pub fn embedded(&mut self, reply: &Message) {
        if let Some(Value::Struct(fields)) = reply.body.first()
            && let [Value::String(name), Value::ObjectPath(path)] = fields.as_slice()
        {
            self.desktop = Some((name.clone(), path.clone()));
        }
    }

    /// The window's scale (device pixels per logical pixel) and the screen position of its
    /// top left in device pixels.
    pub fn set_geometry(&mut self, scale: f32, origin: (i32, i32)) {
        self.scale = if scale.is_finite() && scale > 0.0 {
            scale
        } else {
            1.0
        };
        self.origin = origin;
    }

    /// Replaces the tree and returns the events for what changed: nodes added and removed,
    /// names, descriptions, values, text, carets and selections, checked, selected,
    /// expanded and invalid states, and the focus moving.
    pub fn set_tree(&mut self, nodes: &[AccessNode]) -> Vec<Message> {
        let old = std::mem::replace(&mut self.nodes, nodes.to_vec());
        let mut events = Vec::new();
        let mut selection_owners = Vec::new();
        for removed in old.iter().filter(|node| self.node(node.id).is_none()) {
            events.push(event(
                &parent_object(&old, removed).path(),
                OBJECT_EVENTS,
                "ChildrenChanged",
                "remove",
                index_in_parent(&old, removed),
                self.reference(Object::Node(removed.id)),
            ));
        }
        for node in &self.nodes {
            let Some(before) = old.iter().find(|other| other.id == node.id) else {
                events.push(event(
                    &parent_object(&self.nodes, node).path(),
                    OBJECT_EVENTS,
                    "ChildrenChanged",
                    "add",
                    index_in_parent(&self.nodes, node),
                    self.reference(Object::Node(node.id)),
                ));
                continue;
            };
            events.extend(node_change_events(before, node));
            if before.state.selected != node.state.selected {
                let owner = selection_owner(&self.nodes, node);
                if !selection_owners.contains(&owner) {
                    selection_owners.push(owner);
                }
            }
        }
        for owner in selection_owners {
            events.push(event(
                &owner.path(),
                OBJECT_EVENTS,
                "SelectionChanged",
                "",
                0,
                Value::Int32(0),
            ));
        }
        let had_frame = old.iter().any(|node| node.role == AccessRole::Window);
        if self.window_active && !had_frame && self.frame().is_some() {
            events.extend(self.activation_events(true));
            return events;
        }
        let (was, now) = (focused_node(&old), focused_node(&self.nodes));
        if was != now {
            if let Some(id) = was
                && self.node(id).is_some()
            {
                events.push(state_changed(Object::Node(id), "focused", false));
            }
            if let Some(id) = now {
                events.extend(self.focus_events(id));
            }
        }
        events
    }

    /// Records whether the window has the keyboard focus and returns the `Window:Activate` or
    /// `Window:Deactivate` events, with the focused node announced again on activation.
    pub fn set_window_active(&mut self, active: bool) -> Vec<Message> {
        if self.window_active == active {
            return Vec::new();
        }
        self.window_active = active;
        self.activation_events(active)
    }

    fn activation_events(&self, active: bool) -> Vec<Message> {
        let Some(frame) = self.frame() else {
            return Vec::new();
        };
        let object = Object::Node(frame.id);
        let mut events = vec![
            event(
                &object.path(),
                WINDOW_EVENTS,
                if active { "Activate" } else { "Deactivate" },
                "",
                0,
                Value::String(frame.name.clone()),
            ),
            state_changed(object, "active", active),
        ];
        if active && let Some(id) = focused_node(&self.nodes) {
            events.extend(self.focus_events(id));
        }
        events
    }

    /// The reply to a method call, `None` for other messages and for calls that asked for no
    /// reply.
    pub fn answer(&mut self, call: &Message) -> Option<Message> {
        if call.kind != MessageKind::MethodCall {
            return None;
        }
        let result = self.reply(call);
        if call.flags & NO_REPLY_EXPECTED != 0 {
            return None;
        }
        Some(match result {
            Ok(body) => Message::method_return(0, call, body),
            Err(error) => Message::error(0, call, error.name, &error.text),
        })
    }

    fn focus_events(&self, id: u32) -> Vec<Message> {
        let object = Object::Node(id);
        vec![
            state_changed(object, "focused", true),
            event(
                &object.path(),
                FOCUS_EVENTS,
                "Focus",
                "",
                0,
                Value::Int32(0),
            ),
        ]
    }

    fn node(&self, id: u32) -> Option<&AccessNode> {
        self.nodes.iter().find(|node| node.id == id)
    }

    fn frame(&self) -> Option<&AccessNode> {
        self.nodes
            .iter()
            .find(|node| node.role == AccessRole::Window)
    }

    fn reference(&self, object: Object) -> Value {
        reference(&self.unique_name, &object.path())
    }

    fn interfaces(&self, object: Object) -> Vec<&'static str> {
        match object {
            Object::Application => vec![ACCESSIBLE_INTERFACE, APPLICATION_INTERFACE],
            Object::Node(id) => self.node(id).map_or_else(
                || vec![ACCESSIBLE_INTERFACE, COMPONENT_INTERFACE],
                node_interfaces,
            ),
        }
    }

    fn introspection(&self, object: Option<Object>) -> String {
        let interfaces = object.map_or_else(Vec::new, |object| self.interfaces(object));
        let mut xml = String::from("<node>");
        for interface in interfaces
            .iter()
            .chain([&PROPERTIES_INTERFACE, &INTROSPECTABLE_INTERFACE])
        {
            xml.push_str(&format!("<interface name=\"{interface}\"/>"));
        }
        xml.push_str("</node>");
        xml
    }

    fn reply(&mut self, call: &Message) -> Result<Vec<Value>, CallError> {
        let member = call.member.as_deref().unwrap_or_default();
        let path = call.path.as_deref().unwrap_or_default();
        let object = Object::from_path(path).filter(|&object| match object {
            Object::Application => true,
            Object::Node(id) => self.node(id).is_some(),
        });
        match call.interface.as_deref() {
            Some(INTROSPECTABLE_INTERFACE) if member == "Introspect" => {
                return Ok(vec![Value::String(self.introspection(object))]);
            }
            Some(PEER_INTERFACE) if member == "Ping" => return Ok(Vec::new()),
            _ => {}
        }
        let object =
            object.ok_or_else(|| CallError::new(UNKNOWN_OBJECT, format!("no object at {path}")))?;
        match call.interface.as_deref() {
            Some(PROPERTIES_INTERFACE) => self.properties_call(object, member, &call.body),
            Some(interface) if self.interfaces(object).contains(&interface) => {
                self.interface_call(object, interface, member, &call.body)
            }
            Some(interface) => Err(CallError::new(
                UNKNOWN_INTERFACE,
                format!("{path} has no interface {interface}"),
            )),
            None => {
                for interface in self.interfaces(object) {
                    match self.interface_call(object, interface, member, &call.body) {
                        Err(error) if error.name == UNKNOWN_METHOD => {}
                        result => return result,
                    }
                }
                Err(CallError::unknown_method("", member))
            }
        }
    }

    fn interface_call(
        &mut self,
        object: Object,
        interface: &str,
        member: &str,
        body: &[Value],
    ) -> Result<Vec<Value>, CallError> {
        match (interface, object) {
            (ACCESSIBLE_INTERFACE, _) => self.accessible_call(object, member, body),
            (COMPONENT_INTERFACE, Object::Node(id)) => self.component_call(id, member, body),
            (ACTION_INTERFACE, Object::Node(id)) => self.action_call(id, member, body),
            (TEXT_INTERFACE, Object::Node(id)) => self.text_call(id, member, body),
            (EDITABLE_TEXT_INTERFACE, Object::Node(id)) => {
                self.editable_text_call(id, member, body)
            }
            (SELECTION_INTERFACE, Object::Node(id)) => self.selection_call(id, member, body),
            (TABLE_INTERFACE, Object::Node(id)) => self.table_call(id, member, body),
            (TABLE_CELL_INTERFACE, Object::Node(id)) => self.table_cell_call(id, member, body),
            (APPLICATION_INTERFACE, Object::Application) if member == "GetLocale" => {
                Ok(vec![Value::String("C".to_owned())])
            }
            _ => Err(CallError::unknown_method(interface, member)),
        }
    }

    fn accessible_call(
        &self,
        object: Object,
        member: &str,
        body: &[Value],
    ) -> Result<Vec<Value>, CallError> {
        let node = match object {
            Object::Application => None,
            Object::Node(id) => self.node(id),
        };
        let value = match member {
            "GetChildAtIndex" => {
                let index = i32_argument(body, 0)?;
                usize::try_from(index)
                    .ok()
                    .and_then(|index| children(&self.nodes, object).get(index).copied())
                    .map_or_else(null_reference, |id| self.reference(Object::Node(id)))
            }
            "GetChildren" => Value::Array(
                "(so)".to_owned(),
                children(&self.nodes, object)
                    .into_iter()
                    .map(|id| self.reference(Object::Node(id)))
                    .collect(),
            ),
            "GetIndexInParent" => {
                Value::Int32(node.map_or(-1, |node| index_in_parent(&self.nodes, node)))
            }
            "GetRelationSet" => Value::Array("(ua(so))".to_owned(), Vec::new()),
            "GetRole" => Value::UInt32(node.map_or(ROLE_APPLICATION, |node| role(node.role))),
            "GetRoleName" | "GetLocalizedRoleName" => Value::String(
                node.map_or("application", |node| role_name(node.role))
                    .to_owned(),
            ),
            "GetState" => {
                let states = node.map_or_else(Vec::new, |node| states(node, self.window_active));
                Value::Array(
                    "u".to_owned(),
                    state_set(&states).into_iter().map(Value::UInt32).collect(),
                )
            }
            "GetAttributes" => string_map(attributes(node)),
            "GetApplication" => self.reference(Object::Application),
            "GetInterfaces" => Value::Array(
                "s".to_owned(),
                self.interfaces(object)
                    .into_iter()
                    .map(|name| Value::String(name.to_owned()))
                    .collect(),
            ),
            _ => return Err(CallError::unknown_method(ACCESSIBLE_INTERFACE, member)),
        };
        Ok(vec![value])
    }

    fn device_rect(&self, bounds: AccessRect) -> (i32, i32, i32, i32) {
        let device = |logical: f32| (logical * self.scale).round() as i32;
        (
            device(bounds.x),
            device(bounds.y),
            device(bounds.width),
            device(bounds.height),
        )
    }

    fn coordinate_origin(&self, node: &AccessNode, coord_type: u32) -> (i32, i32) {
        match coord_type {
            COORD_SCREEN => (
                self.origin.0.saturating_neg(),
                self.origin.1.saturating_neg(),
            ),
            COORD_PARENT => match parent_object(&self.nodes, node) {
                Object::Node(parent) => self.node(parent).map_or((0, 0), |parent| {
                    let (x, y, _, _) = self.device_rect(parent.bounds);
                    (x, y)
                }),
                Object::Application => (0, 0),
            },
            _ => (0, 0),
        }
    }

    fn extents(&self, node: &AccessNode, coord_type: u32) -> (i32, i32, i32, i32) {
        let (x, y, width, height) = self.device_rect(node.bounds);
        let (left, top) = self.coordinate_origin(node, coord_type);
        (x.saturating_sub(left), y.saturating_sub(top), width, height)
    }

    fn logical_point(&self, node: &AccessNode, body: &[Value]) -> Result<(f32, f32), CallError> {
        let (x, y) = (i32_argument(body, 0)?, i32_argument(body, 1)?);
        let (left, top) = self.coordinate_origin(node, u32_argument(body, 2)?);
        let to_logical =
            |device: i32, offset: i32| device.saturating_add(offset) as f32 / self.scale;
        Ok((to_logical(x, left), to_logical(y, top)))
    }

    fn component_call(
        &mut self,
        id: u32,
        member: &str,
        body: &[Value],
    ) -> Result<Vec<Value>, CallError> {
        if member == "GrabFocus" {
            return Ok(vec![self.queue(id, Some(AccessAction::Focus))]);
        }
        let node = self
            .node(id)
            .ok_or_else(|| CallError::new(UNKNOWN_OBJECT, format!("no node {id}")))?;
        Ok(match member {
            "GetExtents" => {
                let (x, y, width, height) = self.extents(node, u32_argument(body, 0)?);
                vec![Value::Struct(vec![
                    Value::Int32(x),
                    Value::Int32(y),
                    Value::Int32(width),
                    Value::Int32(height),
                ])]
            }
            "GetPosition" => {
                let (x, y, _, _) = self.extents(node, u32_argument(body, 0)?);
                vec![Value::Int32(x), Value::Int32(y)]
            }
            "GetSize" => {
                let (_, _, width, height) = self.extents(node, COORD_WINDOW);
                vec![Value::Int32(width), Value::Int32(height)]
            }
            "Contains" => {
                let (x, y) = self.logical_point(node, body)?;
                vec![Value::Boolean(contains(node.bounds, x, y))]
            }
            "GetAccessibleAtPoint" => {
                let (x, y) = self.logical_point(node, body)?;
                let found = self
                    .nodes
                    .iter()
                    .rev()
                    .find(|other| {
                        contains(other.bounds, x, y) && is_inside(&self.nodes, other, node.id)
                    })
                    .map_or_else(null_reference, |other| {
                        self.reference(Object::Node(other.id))
                    });
                vec![found]
            }
            "GetLayer" => vec![Value::UInt32(if node.role == AccessRole::Window {
                LAYER_WINDOW
            } else {
                LAYER_WIDGET
            })],
            "GetMDIZOrder" => vec![Value::Int16(0)],
            "GetAlpha" => vec![Value::Double(1.0)],
            _ => return Err(CallError::unknown_method(COMPONENT_INTERFACE, member)),
        })
    }

    fn properties(&self, object: Object, interface: &str) -> Option<Vec<(&'static str, Value)>> {
        let node = match object {
            Object::Application => None,
            Object::Node(id) => self.node(id),
        };
        if !self.interfaces(object).contains(&interface) {
            return None;
        }
        match (interface, object) {
            (ACCESSIBLE_INTERFACE, _) => {
                let parent = match (object, node) {
                    (Object::Application, _) => self
                        .desktop
                        .as_ref()
                        .map_or_else(null_reference, |(name, path)| reference(name, path)),
                    (_, Some(node)) => self.reference(parent_object(&self.nodes, node)),
                    (_, None) => null_reference(),
                };
                let child_count =
                    i32::try_from(children(&self.nodes, object).len()).unwrap_or(i32::MAX);
                let text = |value: &str| Value::String(value.to_owned());
                Some(vec![
                    (
                        "Name",
                        text(node.map_or(self.name.as_str(), |node| node.name.as_str())),
                    ),
                    (
                        "Description",
                        text(node.map(description).unwrap_or_default()),
                    ),
                    ("Parent", parent),
                    ("ChildCount", Value::Int32(child_count)),
                    ("Locale", text("")),
                    (
                        "AccessibleId",
                        text(
                            node.map(|node| node.id.to_string())
                                .unwrap_or_default()
                                .as_str(),
                        ),
                    ),
                    ("HelpText", text("")),
                ])
            }
            (APPLICATION_INTERFACE, Object::Application) => Some(vec![
                ("ToolkitName", Value::String(TOOLKIT_NAME.to_owned())),
                (
                    "ToolkitVersion",
                    Value::String(env!("CARGO_PKG_VERSION").to_owned()),
                ),
                (
                    "Version",
                    Value::String(env!("CARGO_PKG_VERSION").to_owned()),
                ),
                ("AtspiVersion", Value::String(ATSPI_VERSION.to_owned())),
                ("Id", Value::Int32(self.id)),
            ]),
            (COMPONENT_INTERFACE | EDITABLE_TEXT_INTERFACE, Object::Node(_)) => Some(Vec::new()),
            (_, Object::Node(id)) => self.node_properties(id, interface),
            _ => None,
        }
    }

    fn node_properties(&self, id: u32, interface: &str) -> Option<Vec<(&'static str, Value)>> {
        let node = self.node(id)?;
        let count = |number: usize| Value::Int32(i32::try_from(number).unwrap_or(i32::MAX));
        match interface {
            ACTION_INTERFACE => Some(vec![("NActions", count(node_actions(node).len()))]),
            TEXT_INTERFACE => {
                let length = node_chars(node).len();
                let caret = node.caret.map_or(Value::Int32(-1), |caret| {
                    offset_value(caret.caret.min(length))
                });
                Some(vec![
                    ("CharacterCount", count(length)),
                    ("CaretOffset", caret),
                ])
            }
            SELECTION_INTERFACE => Some(vec![(
                "NSelectedChildren",
                count(selected_items(&self.nodes, id).len()),
            )]),
            TABLE_INTERFACE => {
                let table = node.table.unwrap_or_default();
                Some(vec![
                    ("NRows", count(table.rows)),
                    ("NColumns", count(table.columns)),
                    ("Caption", null_reference()),
                    ("Summary", null_reference()),
                    ("NSelectedRows", Value::Int32(0)),
                    ("NSelectedColumns", Value::Int32(0)),
                ])
            }
            TABLE_CELL_INTERFACE => {
                let cell = node.cell.unwrap_or_default();
                let table = grid_of(&self.nodes, node)
                    .map_or_else(null_reference, |grid| self.reference(Object::Node(grid.id)));
                Some(vec![
                    ("ColumnSpan", Value::Int32(1)),
                    (
                        "Position",
                        Value::Struct(vec![count(cell.row), count(cell.column)]),
                    ),
                    ("RowSpan", Value::Int32(1)),
                    ("Table", table),
                ])
            }
            _ => None,
        }
    }

    fn action_call(
        &mut self,
        id: u32,
        member: &str,
        body: &[Value],
    ) -> Result<Vec<Value>, CallError> {
        let node = self
            .node(id)
            .ok_or_else(|| CallError::new(UNKNOWN_OBJECT, format!("no node {id}")))?;
        let actions = node_actions(node);
        let chosen = |index: i32| {
            usize::try_from(index)
                .ok()
                .and_then(|index| actions.get(index).copied())
        };
        let text = |value: &str| Value::String(value.to_owned());
        let value = match member {
            "GetNActions" => Value::Int32(i32::try_from(actions.len()).unwrap_or(i32::MAX)),
            "GetName" | "GetLocalizedName" => {
                text(chosen(i32_argument(body, 0)?).map_or("", NodeAction::name))
            }
            "GetDescription" => {
                text(chosen(i32_argument(body, 0)?).map_or("", NodeAction::description))
            }
            "GetKeyBinding" => {
                text(chosen(i32_argument(body, 0)?).map_or("", |action| action.key_binding(node)))
            }
            "GetActions" => Value::Array(
                "(sss)".to_owned(),
                actions
                    .iter()
                    .map(|action| {
                        Value::Struct(vec![
                            text(action.name()),
                            text(action.description()),
                            text(action.key_binding(node)),
                        ])
                    })
                    .collect(),
            ),
            "DoAction" => {
                let request = chosen(i32_argument(body, 0)?).map(|action| action.request(node));
                return Ok(vec![self.queue(id, request)]);
            }
            _ => return Err(CallError::unknown_method(ACTION_INTERFACE, member)),
        };
        Ok(vec![value])
    }

    fn text_call(
        &mut self,
        id: u32,
        member: &str,
        body: &[Value],
    ) -> Result<Vec<Value>, CallError> {
        let node = self
            .node(id)
            .ok_or_else(|| CallError::new(UNKNOWN_OBJECT, format!("no node {id}")))?;
        if matches!(
            member,
            "SetCaretOffset" | "SetSelection" | "AddSelection" | "RemoveSelection"
        ) {
            let request = text_request(node, member, body)?;
            return Ok(vec![self.queue(id, request)]);
        }
        let chars = node_chars(node);
        let length = chars.len();
        let selection = node
            .caret
            .map(AccessCaret::range)
            .map(|(start, end)| (start.min(length), end.min(length)))
            .filter(|(start, end)| start != end);
        let at = || -> Result<usize, CallError> {
            Ok(offset_from_start(i32_argument(body, 0)?, length))
        };
        let no_attributes = || string_map(Vec::new());
        Ok(match member {
            "GetText" => {
                let start = offset_from_start(i32_argument(body, 0)?, length);
                let end = offset_to_end(i32_argument(body, 1)?, length).max(start);
                vec![Value::String(text_slice(&chars, start, end))]
            }
            "GetCharacterAtOffset" => {
                let index = usize::try_from(i32_argument(body, 0)?).ok();
                let character = index.and_then(|index| chars.get(index)).copied();
                vec![Value::Int32(character.map_or(0, |character| {
                    i32::try_from(u32::from(character)).unwrap_or(0)
                }))]
            }
            "GetStringAtOffset" => text_answer(
                &chars,
                granularity_range(&chars, at()?, u32_argument(body, 1)?)?,
            ),
            "GetTextAtOffset" => text_answer(
                &chars,
                boundary_range(&chars, at()?, u32_argument(body, 1)?)?,
            ),
            "GetTextBeforeOffset" => {
                let boundary = u32_argument(body, 1)?;
                let (start, _) = boundary_range(&chars, at()?, boundary)?;
                if start == 0 {
                    text_answer(&chars, (0, 0))
                } else {
                    text_answer(&chars, boundary_range(&chars, start - 1, boundary)?)
                }
            }
            "GetTextAfterOffset" => {
                let boundary = u32_argument(body, 1)?;
                let (_, end) = boundary_range(&chars, at()?, boundary)?;
                if end >= length {
                    text_answer(&chars, (length, length))
                } else {
                    text_answer(&chars, boundary_range(&chars, end, boundary)?)
                }
            }
            "GetNSelections" => vec![Value::Int32(i32::from(selection.is_some()))],
            "GetSelection" => {
                let index = i32_argument(body, 0)?;
                let (start, end) = selection.filter(|_| index == 0).unwrap_or((0, 0));
                vec![offset_value(start), offset_value(end)]
            }
            "GetAttributes" | "GetAttributeRun" => {
                at()?;
                vec![no_attributes(), Value::Int32(0), offset_value(length)]
            }
            "GetAttributeValue" => vec![Value::String(String::new())],
            "GetDefaultAttributes" | "GetDefaultAttributeSet" => vec![no_attributes()],
            "GetCharacterExtents" | "GetRangeExtents" => {
                let coord_type =
                    u32_argument(body, if member == "GetRangeExtents" { 2 } else { 1 })?;
                let (x, y, width, height) = self.extents(node, coord_type);
                vec![
                    Value::Int32(x),
                    Value::Int32(y),
                    Value::Int32(width),
                    Value::Int32(height),
                ]
            }
            "GetOffsetAtPoint" => vec![Value::Int32(-1)],
            "ScrollSubstringTo" | "ScrollSubstringToPoint" => vec![Value::Boolean(false)],
            _ => return Err(CallError::unknown_method(TEXT_INTERFACE, member)),
        })
    }

    fn editable_text_call(
        &mut self,
        id: u32,
        member: &str,
        body: &[Value],
    ) -> Result<Vec<Value>, CallError> {
        if member == "CopyText" {
            return Ok(Vec::new());
        }
        let node = self
            .node(id)
            .ok_or_else(|| CallError::new(UNKNOWN_OBJECT, format!("no node {id}")))?;
        let request = editable_text_request(node, member, body)?;
        Ok(vec![self.queue(id, request)])
    }

    fn selection_call(
        &mut self,
        id: u32,
        member: &str,
        body: &[Value],
    ) -> Result<Vec<Value>, CallError> {
        let child = |index: i32| {
            usize::try_from(index)
                .ok()
                .and_then(|index| children(&self.nodes, Object::Node(id)).get(index).copied())
        };
        let value = match member {
            "GetSelectedChild" => {
                let index = i32_argument(body, 0)?;
                usize::try_from(index)
                    .ok()
                    .and_then(|index| selected_items(&self.nodes, id).get(index).copied())
                    .map_or_else(null_reference, |child| self.reference(Object::Node(child)))
            }
            "SelectChild" => {
                let Some(child) = child(i32_argument(body, 0)?) else {
                    return Ok(vec![Value::Boolean(false)]);
                };
                return Ok(vec![self.queue(child, Some(AccessAction::Select))]);
            }
            "IsChildSelected" => Value::Boolean(
                child(i32_argument(body, 0)?)
                    .and_then(|child| self.node(child))
                    .is_some_and(|child| child.state.selected),
            ),
            "DeselectSelectedChild" | "DeselectChild" | "SelectAll" | "ClearSelection" => {
                Value::Boolean(false)
            }
            _ => return Err(CallError::unknown_method(SELECTION_INTERFACE, member)),
        };
        Ok(vec![value])
    }

    fn cell_at(&self, grid: u32, row: i32, column: i32) -> Option<&AccessNode> {
        let (row, column) = (usize::try_from(row).ok()?, usize::try_from(column).ok()?);
        grid_members(&self.nodes, grid, AccessRole::Cell).find(|node| {
            node.cell
                .is_some_and(|cell| cell.row == row && cell.column == column)
        })
    }

    fn column_header(&self, grid: u32, column: i32) -> Option<&AccessNode> {
        let column = usize::try_from(column).ok()?;
        grid_members(&self.nodes, grid, AccessRole::ColumnHeader)
            .find(|node| node.cell.is_some_and(|cell| cell.column == column))
    }

    fn table_call(&self, id: u32, member: &str, body: &[Value]) -> Result<Vec<Value>, CallError> {
        let table = self
            .node(id)
            .ok_or_else(|| CallError::new(UNKNOWN_OBJECT, format!("no node {id}")))?
            .table
            .unwrap_or_default();
        let to_i32 = |number: usize| i32::try_from(number).unwrap_or(-1);
        let cells = table.rows.saturating_mul(table.columns);
        let index_of = |row: i32, column: i32| {
            let (row, column) = (usize::try_from(row).ok()?, usize::try_from(column).ok()?);
            if row >= table.rows || column >= table.columns {
                return None;
            }
            row.checked_mul(table.columns)?.checked_add(column)
        };
        let position_of = |index: i32| {
            usize::try_from(index)
                .ok()
                .filter(|&index| index < cells && table.columns > 0)
                .map(|index| (index / table.columns, index % table.columns))
        };
        let reference_to = |node: Option<&AccessNode>| {
            node.map_or_else(null_reference, |node| self.reference(Object::Node(node.id)))
        };
        let pair = || -> Result<(i32, i32), CallError> {
            Ok((i32_argument(body, 0)?, i32_argument(body, 1)?))
        };
        Ok(match member {
            "GetAccessibleAt" => {
                let (row, column) = pair()?;
                vec![reference_to(self.cell_at(id, row, column))]
            }
            "GetIndexAt" => {
                let (row, column) = pair()?;
                vec![Value::Int32(index_of(row, column).map_or(-1, to_i32))]
            }
            "GetRowAtIndex" => vec![Value::Int32(
                position_of(i32_argument(body, 0)?).map_or(-1, |(row, _)| to_i32(row)),
            )],
            "GetColumnAtIndex" => vec![Value::Int32(
                position_of(i32_argument(body, 0)?).map_or(-1, |(_, column)| to_i32(column)),
            )],
            "GetRowDescription" => vec![Value::String(String::new())],
            "GetColumnDescription" => vec![Value::String(
                self.column_header(id, i32_argument(body, 0)?)
                    .map(|header| header.name.clone())
                    .unwrap_or_default(),
            )],
            "GetRowExtentAt" | "GetColumnExtentAt" => {
                let (row, column) = pair()?;
                vec![Value::Int32(i32::from(index_of(row, column).is_some()))]
            }
            "GetRowHeader" => vec![null_reference()],
            "GetColumnHeader" => {
                vec![reference_to(self.column_header(id, i32_argument(body, 0)?))]
            }
            "GetSelectedRows" | "GetSelectedColumns" => {
                vec![Value::Array("i".to_owned(), Vec::new())]
            }
            "IsRowSelected"
            | "IsColumnSelected"
            | "AddRowSelection"
            | "AddColumnSelection"
            | "RemoveRowSelection"
            | "RemoveColumnSelection" => vec![Value::Boolean(false)],
            "IsSelected" => {
                let (row, column) = pair()?;
                vec![Value::Boolean(
                    self.cell_at(id, row, column)
                        .is_some_and(|cell| cell.state.selected),
                )]
            }
            "GetRowColumnExtentsAtIndex" => {
                let index = i32_argument(body, 0)?;
                match position_of(index) {
                    Some((row, column)) => {
                        let selected = self
                            .cell_at(id, to_i32(row), to_i32(column))
                            .is_some_and(|cell| cell.state.selected);
                        vec![
                            Value::Boolean(true),
                            Value::Int32(to_i32(row)),
                            Value::Int32(to_i32(column)),
                            Value::Int32(1),
                            Value::Int32(1),
                            Value::Boolean(selected),
                        ]
                    }
                    None => vec![
                        Value::Boolean(false),
                        Value::Int32(0),
                        Value::Int32(0),
                        Value::Int32(0),
                        Value::Int32(0),
                        Value::Boolean(false),
                    ],
                }
            }
            _ => return Err(CallError::unknown_method(TABLE_INTERFACE, member)),
        })
    }

    fn table_cell_call(
        &self,
        id: u32,
        member: &str,
        _body: &[Value],
    ) -> Result<Vec<Value>, CallError> {
        let node = self
            .node(id)
            .ok_or_else(|| CallError::new(UNKNOWN_OBJECT, format!("no node {id}")))?;
        let cell = node.cell.unwrap_or_default();
        let count = |number: usize| Value::Int32(i32::try_from(number).unwrap_or(i32::MAX));
        Ok(match member {
            "GetRowColumnSpan" => vec![
                count(cell.row),
                count(cell.column),
                Value::Int32(1),
                Value::Int32(1),
            ],
            "GetRowHeaderCells" => vec![Value::Array("(so)".to_owned(), Vec::new())],
            "GetColumnHeaderCells" => {
                let headers = grid_of(&self.nodes, node)
                    .and_then(|grid| self.column_header(grid.id, i32::try_from(cell.column).ok()?))
                    .map(|header| self.reference(Object::Node(header.id)))
                    .into_iter()
                    .collect();
                vec![Value::Array("(so)".to_owned(), headers)]
            }
            _ => return Err(CallError::unknown_method(TABLE_CELL_INTERFACE, member)),
        })
    }

    fn properties_call(
        &mut self,
        object: Object,
        member: &str,
        body: &[Value],
    ) -> Result<Vec<Value>, CallError> {
        let interface = str_argument(body, 0)?;
        let properties = self.properties(object, interface).ok_or_else(|| {
            CallError::new(UNKNOWN_INTERFACE, format!("no interface {interface} here"))
        })?;
        match member {
            "Get" => {
                let name = str_argument(body, 1)?;
                properties
                    .into_iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| vec![Value::Variant(Box::new(value))])
                    .ok_or_else(|| {
                        CallError::new(UNKNOWN_PROPERTY, format!("no property {interface}.{name}"))
                    })
            }
            "GetAll" => Ok(vec![Value::Array(
                "{sv}".to_owned(),
                properties
                    .into_iter()
                    .map(|(key, value)| {
                        Value::DictEntry(
                            Box::new(Value::String(key.to_owned())),
                            Box::new(Value::Variant(Box::new(value))),
                        )
                    })
                    .collect(),
            )]),
            "Set" => {
                let name = str_argument(body, 1)?;
                match (interface, name, argument(body, 2)?) {
                    (APPLICATION_INTERFACE, "Id", Value::Variant(value)) => match **value {
                        Value::Int32(id) => {
                            self.id = id;
                            Ok(Vec::new())
                        }
                        _ => Err(CallError::new(INVALID_ARGS, "Id is an int32")),
                    },
                    _ => Err(CallError::new(
                        PROPERTY_READ_ONLY,
                        format!("{interface}.{name} cannot be set"),
                    )),
                }
            }
            _ => Err(CallError::unknown_method(PROPERTIES_INTERFACE, member)),
        }
    }
}

fn string_map(entries: Vec<(&str, String)>) -> Value {
    Value::Array(
        "{ss}".to_owned(),
        entries
            .into_iter()
            .map(|(key, value)| {
                Value::DictEntry(
                    Box::new(Value::String(key.to_owned())),
                    Box::new(Value::String(value)),
                )
            })
            .collect(),
    )
}

fn attributes(node: Option<&AccessNode>) -> Vec<(&'static str, String)> {
    let mut attributes = vec![("toolkit", TOOLKIT_NAME.to_owned())];
    let Some(node) = node else {
        return attributes;
    };
    let live = |politeness: &str| {
        [
            ("live", politeness.to_owned()),
            ("container-live", politeness.to_owned()),
        ]
    };
    match node.role {
        AccessRole::Heading => attributes.push(("level", "1".to_owned())),
        AccessRole::Status => attributes.extend(live("polite")),
        AccessRole::Alert => attributes.extend(live("assertive")),
        AccessRole::TreeItem => {
            if let Some(position) = node.tree_position {
                attributes.extend([
                    ("level", position.level.to_string()),
                    ("posinset", position.position.to_string()),
                    ("setsize", position.set_size.to_string()),
                ]);
            }
        }
        _ => {}
    }
    if let Some(shortcut) = &node.shortcut {
        attributes.push(("keyshortcuts", shortcut.clone()));
    }
    attributes
}

#[cfg(test)]
mod tests {
    use super::super::{AccessActions, AccessCell, AccessTable, AccessTreePosition};
    use super::*;

    const UNIQUE: &str = ":1.42";

    fn node(id: u32, parent: Option<u32>, role: AccessRole, name: &str) -> AccessNode {
        AccessNode {
            id,
            parent,
            role,
            name: name.to_owned(),
            bounds: AccessRect {
                x: 10.0 * id as f32,
                y: 20.0,
                width: 100.0,
                height: 30.0,
            },
            ..AccessNode::default()
        }
    }

    fn tree() -> Vec<AccessNode> {
        let mut window = node(1, None, AccessRole::Window, "Connect to EggDB");
        window.bounds = AccessRect {
            x: 0.0,
            y: 0.0,
            width: 800.0,
            height: 600.0,
        };
        let mut file = node(3, Some(2), AccessRole::TextField, "File");
        file.placeholder = Some("data.eggdb".to_owned());
        file.state.focused = true;
        let mut remember = node(4, Some(2), AccessRole::CheckBox, "Remember");
        remember.state.checked = Some(true);
        let mut connect = node(5, Some(2), AccessRole::Button, "Connect");
        connect.state.default_button = true;
        vec![
            window,
            node(2, Some(1), AccessRole::Group, "Connection"),
            file,
            remember,
            connect,
        ]
    }

    fn application() -> AtspiApplication {
        let mut application = AtspiApplication::new(UNIQUE, "eggdb-gui");
        application.set_tree(&tree());
        application
    }

    fn call(path: &str, interface: &str, member: &str, body: Vec<Value>) -> Message {
        let mut call = Message::method_call(7, UNIQUE, path, interface, member, body);
        call.sender = Some(":1.9".to_owned());
        call
    }

    fn answer_body(
        application: &mut AtspiApplication,
        path: &str,
        interface: &str,
        member: &str,
        body: Vec<Value>,
    ) -> Vec<Value> {
        let call = call(path, interface, member, body);
        let mut reply = application.answer(&call).expect("a reply");
        reply.serial = 3;
        let decoded = Message::decode(&reply.encode().expect("encodes")).expect("decodes");
        assert_eq!(decoded.kind, MessageKind::MethodReturn, "{decoded:?}");
        assert_eq!(decoded.reply_serial, Some(7));
        assert_eq!(decoded.destination.as_deref(), Some(":1.9"));
        decoded.body
    }

    fn node_reference(id: u32) -> Value {
        reference(UNIQUE, &Object::Node(id).path())
    }

    #[test]
    fn roles_and_role_names_follow_the_atspi_enumeration() {
        assert_eq!(role(AccessRole::Window), ROLE_FRAME);
        assert_eq!(role(AccessRole::Button), 43);
        assert_eq!(role(AccessRole::CheckBox), 7);
        assert_eq!(role(AccessRole::TextField), 79);
        assert_eq!(role(AccessRole::Heading), 83);
        assert_eq!(role(AccessRole::Link), 88);
        assert_eq!(role(AccessRole::TabList), 38);
        assert_eq!(role(AccessRole::Tab), 37);
        assert_eq!(role(AccessRole::Status), 54);
        assert_eq!(role(AccessRole::TabPanel), ROLE_PANEL);
        assert_eq!(role_name(AccessRole::Button), "push button");
        assert_eq!(role_name(AccessRole::CheckBox), "check box");
        assert_eq!(role_name(AccessRole::TabList), "page tab list");
    }

    #[test]
    fn states_map_to_atspi_bits() {
        let nodes = tree();
        assert_eq!(
            states(&nodes[2], false),
            vec![
                STATE_EDITABLE,
                STATE_ENABLED,
                STATE_FOCUSABLE,
                STATE_FOCUSED,
                STATE_SENSITIVE,
                STATE_SHOWING,
                STATE_SINGLE_LINE,
                STATE_VISIBLE,
            ]
        );
        assert!(states(&nodes[3], false).contains(&STATE_CHECKED));
        assert!(states(&nodes[3], false).contains(&STATE_CHECKABLE));
        assert!(states(&nodes[4], false).contains(&STATE_IS_DEFAULT));
        assert!(!states(&nodes[0], false).contains(&STATE_ACTIVE));
        assert!(states(&nodes[0], true).contains(&STATE_ACTIVE));
        let mut disabled = nodes[4].clone();
        disabled.state.disabled = true;
        let disabled = states(&disabled, false);
        assert!(!disabled.contains(&STATE_ENABLED));
        assert!(!disabled.contains(&STATE_FOCUSABLE));
        let mut tab = node(9, None, AccessRole::Tab, "File");
        tab.state.selected = true;
        assert!(states(&tab, false).contains(&STATE_SELECTED));
        assert_eq!(
            state_set(&[STATE_FOCUSED, STATE_VISIBLE, STATE_IS_DEFAULT, 99]),
            [(1 << 12) | (1 << 30), 1 << 7]
        );
    }

    #[test]
    fn paths_name_the_application_and_the_nodes() {
        assert_eq!(Object::Application.path(), ROOT_PATH);
        assert_eq!(Object::Node(5).path(), "/org/a11y/atspi/accessible/5");
        assert_eq!(Object::from_path(ROOT_PATH), Some(Object::Application));
        assert_eq!(
            Object::from_path("/org/a11y/atspi/accessible/5"),
            Some(Object::Node(5))
        );
        for bad in [
            "/org/a11y/atspi/accessible/",
            "/org/a11y/atspi/accessible/+5",
            "/org/a11y/atspi/accessible/99999999999",
            "/org/a11y/atspi/accessible/5/6",
            "/",
        ] {
            assert_eq!(Object::from_path(bad), None, "{bad}");
        }
    }

    #[test]
    fn embedding_names_the_root_and_records_the_desktop() {
        let mut application = application();
        let embed = application.embed_call();
        assert_eq!(embed.destination.as_deref(), Some(REGISTRY_NAME));
        assert_eq!(embed.interface.as_deref(), Some(SOCKET_INTERFACE));
        assert_eq!(embed.body, vec![reference(UNIQUE, ROOT_PATH)]);
        let mut reply = Message::method_return(2, &embed, vec![reference(":1.0", ROOT_PATH)]);
        reply.serial = 2;
        application.embedded(&reply);
        assert_eq!(
            answer_body(
                &mut application,
                ROOT_PATH,
                PROPERTIES_INTERFACE,
                "Get",
                vec![
                    Value::String(ACCESSIBLE_INTERFACE.to_owned()),
                    Value::String("Parent".to_owned()),
                ],
            ),
            vec![Value::Variant(Box::new(reference(":1.0", ROOT_PATH)))]
        );
    }

    #[test]
    fn accessible_methods_reply_with_their_signatures() {
        let mut application = application();
        let mut ask = |path: &str, member: &str, body: Vec<Value>| {
            answer_body(&mut application, path, ACCESSIBLE_INTERFACE, member, body)
        };
        let group = Object::Node(2).path();
        assert_eq!(
            ask(ROOT_PATH, "GetChildren", vec![]),
            vec![Value::Array("(so)".to_owned(), vec![node_reference(1)])]
        );
        assert_eq!(
            ask(&group, "GetChildren", vec![]),
            vec![Value::Array(
                "(so)".to_owned(),
                vec![node_reference(3), node_reference(4), node_reference(5)]
            )]
        );
        assert_eq!(
            ask(&group, "GetChildAtIndex", vec![Value::Int32(1)]),
            vec![node_reference(4)]
        );
        assert_eq!(
            ask(&group, "GetChildAtIndex", vec![Value::Int32(9)]),
            vec![null_reference()]
        );
        assert_eq!(
            ask(&Object::Node(5).path(), "GetIndexInParent", vec![]),
            vec![Value::Int32(2)]
        );
        assert_eq!(
            ask(ROOT_PATH, "GetRole", vec![]),
            vec![Value::UInt32(ROLE_APPLICATION)]
        );
        assert_eq!(
            ask(&Object::Node(5).path(), "GetRole", vec![]),
            vec![Value::UInt32(ROLE_PUSH_BUTTON)]
        );
        assert_eq!(
            ask(&Object::Node(4).path(), "GetRoleName", vec![]),
            vec![Value::String("check box".to_owned())]
        );
        let focused = state_set(&states(&tree()[2], false));
        assert_eq!(
            ask(&Object::Node(3).path(), "GetState", vec![]),
            vec![Value::Array(
                "u".to_owned(),
                vec![Value::UInt32(focused[0]), Value::UInt32(focused[1])]
            )]
        );
        assert_eq!(
            ask(&group, "GetApplication", vec![]),
            vec![reference(UNIQUE, ROOT_PATH)]
        );
        assert_eq!(
            ask(&group, "GetInterfaces", vec![]),
            vec![Value::Array(
                "s".to_owned(),
                vec![
                    Value::String(ACCESSIBLE_INTERFACE.to_owned()),
                    Value::String(COMPONENT_INTERFACE.to_owned()),
                ]
            )]
        );
        assert_eq!(
            ask(&group, "GetRelationSet", vec![]),
            vec![Value::Array("(ua(so))".to_owned(), vec![])]
        );
        let attributes = ask(&group, "GetAttributes", vec![]);
        assert!(
            matches!(attributes.as_slice(), [Value::Array(signature, entries)]
            if signature == "{ss}" && entries.len() == 1)
        );
    }

    #[test]
    fn properties_are_read_one_by_one_and_all_together() {
        let mut application = application();
        let mut get = |path: &str, interface: &str, name: &str| {
            answer_body(
                &mut application,
                path,
                PROPERTIES_INTERFACE,
                "Get",
                vec![
                    Value::String(interface.to_owned()),
                    Value::String(name.to_owned()),
                ],
            )
        };
        let variant = |value: Value| vec![Value::Variant(Box::new(value))];
        let file = Object::Node(3).path();
        assert_eq!(
            get(&file, ACCESSIBLE_INTERFACE, "Name"),
            variant(Value::String("File".to_owned()))
        );
        assert_eq!(
            get(&file, ACCESSIBLE_INTERFACE, "Description"),
            variant(Value::String("data.eggdb".to_owned()))
        );
        assert_eq!(
            get(&file, ACCESSIBLE_INTERFACE, "Parent"),
            variant(node_reference(2))
        );
        assert_eq!(
            get(&Object::Node(1).path(), ACCESSIBLE_INTERFACE, "Parent"),
            variant(reference(UNIQUE, ROOT_PATH))
        );
        assert_eq!(
            get(ROOT_PATH, ACCESSIBLE_INTERFACE, "Parent"),
            variant(null_reference())
        );
        assert_eq!(
            get(ROOT_PATH, ACCESSIBLE_INTERFACE, "Name"),
            variant(Value::String("eggdb-gui".to_owned()))
        );
        assert_eq!(
            get(&Object::Node(2).path(), ACCESSIBLE_INTERFACE, "ChildCount"),
            variant(Value::Int32(3))
        );
        assert_eq!(
            get(ROOT_PATH, APPLICATION_INTERFACE, "ToolkitName"),
            variant(Value::String("EggDB".to_owned()))
        );
        let all = answer_body(
            &mut application,
            ROOT_PATH,
            PROPERTIES_INTERFACE,
            "GetAll",
            vec![Value::String(APPLICATION_INTERFACE.to_owned())],
        );
        assert!(matches!(all.as_slice(), [Value::Array(signature, entries)]
            if signature == "{sv}" && entries.len() == 5));
        assert_eq!(
            answer_body(
                &mut application,
                ROOT_PATH,
                PROPERTIES_INTERFACE,
                "Set",
                vec![
                    Value::String(APPLICATION_INTERFACE.to_owned()),
                    Value::String("Id".to_owned()),
                    Value::Variant(Box::new(Value::Int32(12))),
                ],
            ),
            vec![]
        );
        assert_eq!(application.id, 12);
    }

    #[test]
    fn component_methods_answer_in_device_pixels() {
        let mut application = application();
        application.set_geometry(2.0, (100, 50));
        let button = Object::Node(5).path();
        let mut ask = |member: &str, body: Vec<Value>| {
            answer_body(&mut application, &button, COMPONENT_INTERFACE, member, body)
        };
        let rect = |x, y, width, height| {
            vec![Value::Struct(vec![
                Value::Int32(x),
                Value::Int32(y),
                Value::Int32(width),
                Value::Int32(height),
            ])]
        };
        assert_eq!(
            ask("GetExtents", vec![Value::UInt32(COORD_WINDOW)]),
            rect(100, 40, 200, 60)
        );
        assert_eq!(
            ask("GetExtents", vec![Value::UInt32(COORD_SCREEN)]),
            rect(200, 90, 200, 60)
        );
        assert_eq!(
            ask("GetExtents", vec![Value::UInt32(COORD_PARENT)]),
            rect(60, 0, 200, 60)
        );
        assert_eq!(
            ask("GetPosition", vec![Value::UInt32(COORD_SCREEN)]),
            vec![Value::Int32(200), Value::Int32(90)]
        );
        assert_eq!(
            ask("GetSize", vec![]),
            vec![Value::Int32(200), Value::Int32(60)]
        );
        assert_eq!(
            ask(
                "Contains",
                vec![
                    Value::Int32(210),
                    Value::Int32(100),
                    Value::UInt32(COORD_SCREEN)
                ]
            ),
            vec![Value::Boolean(true)]
        );
        assert_eq!(
            ask(
                "Contains",
                vec![
                    Value::Int32(0),
                    Value::Int32(0),
                    Value::UInt32(COORD_WINDOW)
                ]
            ),
            vec![Value::Boolean(false)]
        );
        assert_eq!(ask("GetLayer", vec![]), vec![Value::UInt32(LAYER_WIDGET)]);
        assert_eq!(ask("GetMDIZOrder", vec![]), vec![Value::Int16(0)]);
        assert_eq!(ask("GrabFocus", vec![]), vec![Value::Boolean(false)]);
        assert_eq!(ask("GetAlpha", vec![]), vec![Value::Double(1.0)]);
        assert_eq!(
            answer_body(
                &mut application,
                &Object::Node(1).path(),
                COMPONENT_INTERFACE,
                "GetAccessibleAtPoint",
                vec![
                    Value::Int32(110),
                    Value::Int32(45),
                    Value::UInt32(COORD_WINDOW)
                ],
            ),
            vec![node_reference(5)]
        );
    }

    #[test]
    fn application_and_peer_calls_are_answered() {
        let mut application = application();
        assert_eq!(
            answer_body(
                &mut application,
                ROOT_PATH,
                APPLICATION_INTERFACE,
                "GetLocale",
                vec![Value::UInt32(0)],
            ),
            vec![Value::String("C".to_owned())]
        );
        assert_eq!(
            answer_body(&mut application, "/", PEER_INTERFACE, "Ping", vec![]),
            vec![]
        );
        let xml = answer_body(
            &mut application,
            ROOT_PATH,
            INTROSPECTABLE_INTERFACE,
            "Introspect",
            vec![],
        );
        assert!(
            matches!(xml.as_slice(), [Value::String(text)] if text.contains(APPLICATION_INTERFACE))
        );
        let mut without_interface = call(&Object::Node(5).path(), "", "GetRole", vec![]);
        without_interface.interface = None;
        let reply = application.answer(&without_interface).expect("a reply");
        assert_eq!(reply.body, vec![Value::UInt32(ROLE_PUSH_BUTTON)]);
    }

    #[test]
    fn bad_calls_get_errors_and_no_reply_calls_get_nothing() {
        let mut application = application();
        let error_name = |application: &mut AtspiApplication, call: Message| {
            let reply = application.answer(&call).expect("a reply");
            let decoded =
                Message::decode(&Message { serial: 4, ..reply }.encode().unwrap()).unwrap();
            assert_eq!(decoded.kind, MessageKind::Error);
            decoded.error_name
        };
        let cases = [
            (
                call(
                    "/org/a11y/atspi/accessible/77",
                    ACCESSIBLE_INTERFACE,
                    "GetRole",
                    vec![],
                ),
                UNKNOWN_OBJECT,
            ),
            (
                call(ROOT_PATH, ACCESSIBLE_INTERFACE, "Explode", vec![]),
                UNKNOWN_METHOD,
            ),
            (
                call(
                    ROOT_PATH,
                    COMPONENT_INTERFACE,
                    "GetExtents",
                    vec![Value::UInt32(0)],
                ),
                UNKNOWN_INTERFACE,
            ),
            (
                call(
                    &Object::Node(2).path(),
                    ACCESSIBLE_INTERFACE,
                    "GetChildAtIndex",
                    vec![Value::String("x".to_owned())],
                ),
                INVALID_ARGS,
            ),
            (
                call(
                    &Object::Node(2).path(),
                    COMPONENT_INTERFACE,
                    "GetExtents",
                    vec![],
                ),
                INVALID_ARGS,
            ),
            (
                call(
                    ROOT_PATH,
                    PROPERTIES_INTERFACE,
                    "Get",
                    vec![
                        Value::String(ACCESSIBLE_INTERFACE.to_owned()),
                        Value::String("Nope".to_owned()),
                    ],
                ),
                UNKNOWN_PROPERTY,
            ),
            (
                call(
                    ROOT_PATH,
                    PROPERTIES_INTERFACE,
                    "Set",
                    vec![
                        Value::String(ACCESSIBLE_INTERFACE.to_owned()),
                        Value::String("Name".to_owned()),
                        Value::Variant(Box::new(Value::Int32(1))),
                    ],
                ),
                PROPERTY_READ_ONLY,
            ),
        ];
        for (call, expected) in cases {
            assert_eq!(
                error_name(&mut application, call).as_deref(),
                Some(expected)
            );
        }
        let mut quiet = call(ROOT_PATH, ACCESSIBLE_INTERFACE, "GetRole", vec![]);
        quiet.flags = NO_REPLY_EXPECTED;
        assert!(application.answer(&quiet).is_none());
        let signal = Message::signal(1, ROOT_PATH, ACCESSIBLE_INTERFACE, "GetRole", vec![]);
        assert!(application.answer(&signal).is_none());
    }

    fn summary(events: &[Message]) -> Vec<(String, String, String, Option<i32>)> {
        events
            .iter()
            .map(|event| {
                let detail = event
                    .body
                    .first()
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let detail1 = match event.body.get(1) {
                    Some(Value::Int32(number)) => Some(*number),
                    _ => None,
                };
                (
                    event.path.clone().unwrap_or_default(),
                    event.member.clone().unwrap_or_default(),
                    detail.to_owned(),
                    detail1,
                )
            })
            .collect()
    }

    #[test]
    fn a_focus_move_raises_state_changed_and_focus_events() {
        let mut application = application();
        let mut nodes = tree();
        nodes[2].state.focused = false;
        nodes[4].state.focused = true;
        let events = application.set_tree(&nodes);
        assert_eq!(
            summary(&events),
            vec![
                (
                    Object::Node(3).path(),
                    "StateChanged".to_owned(),
                    "focused".to_owned(),
                    Some(0)
                ),
                (
                    Object::Node(5).path(),
                    "StateChanged".to_owned(),
                    "focused".to_owned(),
                    Some(1)
                ),
                (
                    Object::Node(5).path(),
                    "Focus".to_owned(),
                    String::new(),
                    Some(0)
                ),
            ]
        );
        for mut event in events {
            assert_eq!(event.kind, MessageKind::Signal);
            event.serial = 9;
            let decoded = Message::decode(&event.encode().unwrap()).unwrap();
            assert_eq!(signature_of_body(&decoded), "siiva{sv}");
        }
        assert!(application.set_tree(&nodes).is_empty());
    }

    fn signature_of_body(message: &Message) -> String {
        crate::dbus::signature_of(&message.body)
    }

    #[test]
    fn tree_changes_and_window_activation_raise_events() {
        let mut application = application();
        let mut nodes = tree();
        nodes[3].state.checked = Some(false);
        nodes[4].name = "Open".to_owned();
        nodes.remove(1);
        nodes.push(node(8, Some(1), AccessRole::Status, "File not found"));
        let events = summary(&application.set_tree(&nodes));
        assert!(events.contains(&(
            Object::Node(1).path(),
            "ChildrenChanged".to_owned(),
            "remove".to_owned(),
            Some(0)
        )));
        assert!(events.contains(&(
            Object::Node(1).path(),
            "ChildrenChanged".to_owned(),
            "add".to_owned(),
            Some(0)
        )));
        assert!(events.contains(&(
            Object::Node(4).path(),
            "StateChanged".to_owned(),
            "checked".to_owned(),
            Some(0)
        )));
        assert!(events.contains(&(
            Object::Node(5).path(),
            "PropertyChange".to_owned(),
            "accessible-name".to_owned(),
            Some(0)
        )));
        let activated = summary(&application.set_window_active(true));
        assert_eq!(
            activated,
            vec![
                (
                    Object::Node(1).path(),
                    "Activate".to_owned(),
                    String::new(),
                    Some(0)
                ),
                (
                    Object::Node(1).path(),
                    "StateChanged".to_owned(),
                    "active".to_owned(),
                    Some(1)
                ),
                (
                    Object::Node(3).path(),
                    "StateChanged".to_owned(),
                    "focused".to_owned(),
                    Some(1)
                ),
                (
                    Object::Node(3).path(),
                    "Focus".to_owned(),
                    String::new(),
                    Some(0)
                ),
            ]
        );
        assert!(application.set_window_active(true).is_empty());
        assert_eq!(summary(&application.set_window_active(false)).len(), 2);
    }

    #[test]
    fn a_window_focused_before_its_first_tree_is_activated_with_that_tree() {
        let mut application = AtspiApplication::new(UNIQUE, "eggdb-gui");
        assert!(application.set_window_active(true).is_empty());
        let events = summary(&application.set_tree(&tree()));
        assert!(events.contains(&(
            Object::Node(1).path(),
            "Activate".to_owned(),
            String::new(),
            Some(0)
        )));
        assert!(events.contains(&(
            Object::Node(3).path(),
            "Focus".to_owned(),
            String::new(),
            Some(0)
        )));
    }

    fn editor_tree() -> Vec<AccessNode> {
        let mut window = node(1, None, AccessRole::Window, "Query");
        window.bounds = AccessRect {
            x: 0.0,
            y: 0.0,
            width: 800.0,
            height: 600.0,
        };
        let mut run = node(11, Some(10), AccessRole::Button, "Run");
        run.actions.invoke = true;
        run.shortcut = Some("Ctrl+Enter".to_owned());
        let mut wrap = node(12, Some(10), AccessRole::CheckBox, "Wrap lines");
        wrap.actions.toggle = true;
        wrap.state.checked = Some(false);
        let mut host = node(20, Some(1), AccessRole::TextField, "Host");
        host.value = Some("shop.eggdb".to_owned());
        host.caret = Some(AccessCaret {
            anchor: 4,
            caret: 10,
        });
        host.actions = AccessActions {
            set_value: true,
            focus: true,
            ..AccessActions::default()
        };
        let mut public = node(31, Some(30), AccessRole::TreeItem, "public");
        public.state.expanded = Some(false);
        public.state.selected = true;
        public.actions = AccessActions {
            expand_collapse: true,
            select: true,
            ..AccessActions::default()
        };
        public.tree_position = Some(AccessTreePosition {
            level: 1,
            position: 1,
            set_size: 2,
        });
        public.description = Some("2 tables".to_owned());
        let mut sales = node(32, Some(30), AccessRole::TreeItem, "sales");
        sales.actions.select = true;
        sales.tree_position = Some(AccessTreePosition {
            level: 1,
            position: 2,
            set_size: 2,
        });
        let mut results = node(40, Some(1), AccessRole::Grid, "Results");
        results.table = Some(AccessTable {
            rows: 100,
            columns: 2,
        });
        let in_grid = |id: u32, role: AccessRole, name: &str, column: usize| {
            let mut member = node(id, Some(40), role, name);
            member.cell = Some(AccessCell { row: 0, column });
            member
        };
        let mut ada = in_grid(44, AccessRole::Cell, "Ada", 1);
        ada.state.selected = true;
        vec![
            window,
            node(10, Some(1), AccessRole::Toolbar, "Query tools"),
            run,
            wrap,
            host,
            node(30, Some(1), AccessRole::Tree, "Objects"),
            public,
            sales,
            results,
            in_grid(41, AccessRole::ColumnHeader, "id", 0),
            in_grid(42, AccessRole::ColumnHeader, "name", 1),
            in_grid(43, AccessRole::Cell, "1", 0),
            ada,
        ]
    }

    fn editor() -> AtspiApplication {
        let mut application = AtspiApplication::new(UNIQUE, "eggdb-gui");
        application.set_tree(&editor_tree());
        application
    }

    fn ask_node(
        application: &mut AtspiApplication,
        id: u32,
        interface: &str,
        member: &str,
        body: Vec<Value>,
    ) -> Vec<Value> {
        answer_body(
            application,
            &Object::Node(id).path(),
            interface,
            member,
            body,
        )
    }

    fn property(application: &mut AtspiApplication, id: u32, interface: &str, name: &str) -> Value {
        let body = ask_node(
            application,
            id,
            PROPERTIES_INTERFACE,
            "Get",
            vec![
                Value::String(interface.to_owned()),
                Value::String(name.to_owned()),
            ],
        );
        match body.as_slice() {
            [Value::Variant(value)] => (**value).clone(),
            other => panic!("not one variant: {other:?}"),
        }
    }

    fn interfaces_of(application: &mut AtspiApplication, id: u32) -> Vec<String> {
        match ask_node(
            application,
            id,
            ACCESSIBLE_INTERFACE,
            "GetInterfaces",
            vec![],
        )
        .as_slice()
        {
            [Value::Array(_, names)] => names
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
            other => panic!("not a list of names: {other:?}"),
        }
    }

    fn text(value: &str) -> Value {
        Value::String(value.to_owned())
    }

    fn requested(node: u32, action: AccessAction) -> AccessRequest {
        AccessRequest { node, action }
    }

    #[test]
    fn new_roles_and_states_follow_the_atspi_enumeration() {
        assert_eq!(role(AccessRole::Toolbar), 63);
        assert_eq!(role(AccessRole::ComboBox), 11);
        assert_eq!(role(AccessRole::TextArea), 61);
        assert_eq!(role(AccessRole::Alert), 2);
        assert_eq!(role(AccessRole::Tree), 65);
        assert_eq!(role(AccessRole::TreeItem), 91);
        assert_eq!(role(AccessRole::Grid), 55);
        assert_eq!(role(AccessRole::ColumnHeader), 57);
        assert_eq!(role(AccessRole::Cell), 56);
        assert_eq!(role_name(AccessRole::Toolbar), "tool bar");
        assert_eq!(role_name(AccessRole::ComboBox), "combo box");
        assert_eq!(role_name(AccessRole::TreeItem), "tree item");
        assert_eq!(role_name(AccessRole::ColumnHeader), "table column header");
        assert_eq!(role_name(AccessRole::Cell), "table cell");
        let mut query = node(9, None, AccessRole::TextArea, "Query");
        query.value = Some("select 1".to_owned());
        query.state.invalid = true;
        let editable = states(&query, false);
        for state in [STATE_MULTI_LINE, STATE_EDITABLE, STATE_INVALID_ENTRY] {
            assert!(editable.contains(&state), "{state}");
        }
        assert!(!editable.contains(&STATE_SINGLE_LINE));
        query.state.read_only = true;
        let read_only = states(&query, false);
        assert!(read_only.contains(&STATE_READ_ONLY));
        assert!(!read_only.contains(&STATE_EDITABLE));
        let nodes = editor_tree();
        let collapsed = states(&nodes[6], false);
        for state in [
            STATE_EXPANDABLE,
            STATE_COLLAPSED,
            STATE_SELECTABLE,
            STATE_SELECTED,
        ] {
            assert!(collapsed.contains(&state), "{state}");
        }
        assert!(!collapsed.contains(&STATE_EXPANDED));
        let mut expanded = nodes[6].clone();
        expanded.state.expanded = Some(true);
        assert!(states(&expanded, false).contains(&STATE_EXPANDED));
        assert!(!states(&nodes[7], false).contains(&STATE_EXPANDABLE));
        assert!(states(&nodes[4], false).contains(&STATE_FOCUSABLE));
        assert!(states(&nodes[4], false).contains(&STATE_SELECTABLE_TEXT));
        assert!(states(&nodes[12], false).contains(&STATE_SELECTED));
    }

    #[test]
    fn each_node_offers_the_interfaces_its_kind_needs() {
        let mut application = editor();
        let with = |extra: &[&str]| -> Vec<String> {
            [ACCESSIBLE_INTERFACE, COMPONENT_INTERFACE]
                .iter()
                .chain(extra)
                .map(|name| (*name).to_owned())
                .collect()
        };
        assert_eq!(interfaces_of(&mut application, 10), with(&[]));
        assert_eq!(
            interfaces_of(&mut application, 11),
            with(&[ACTION_INTERFACE])
        );
        assert_eq!(
            interfaces_of(&mut application, 20),
            with(&[TEXT_INTERFACE, EDITABLE_TEXT_INTERFACE])
        );
        assert_eq!(
            interfaces_of(&mut application, 30),
            with(&[SELECTION_INTERFACE])
        );
        assert_eq!(
            interfaces_of(&mut application, 31),
            with(&[ACTION_INTERFACE])
        );
        assert_eq!(
            interfaces_of(&mut application, 40),
            with(&[SELECTION_INTERFACE, TABLE_INTERFACE])
        );
        assert_eq!(
            interfaces_of(&mut application, 44),
            with(&[TABLE_CELL_INTERFACE])
        );
        let xml = ask_node(
            &mut application,
            40,
            INTROSPECTABLE_INTERFACE,
            "Introspect",
            vec![],
        );
        assert!(matches!(xml.as_slice(), [Value::String(xml)] if xml.contains(TABLE_INTERFACE)));
        let missing = call(
            &Object::Node(10).path(),
            TEXT_INTERFACE,
            "GetText",
            vec![Value::Int32(0), Value::Int32(-1)],
        );
        let reply = application.answer(&missing).expect("a reply");
        assert_eq!(reply.kind, MessageKind::Error);
        assert_eq!(reply.error_name.as_deref(), Some(UNKNOWN_INTERFACE));
        let attributes = ask_node(
            &mut application,
            31,
            ACCESSIBLE_INTERFACE,
            "GetAttributes",
            vec![],
        );
        let entry =
            |key: &str, value: &str| Value::DictEntry(Box::new(text(key)), Box::new(text(value)));
        assert!(matches!(attributes.as_slice(), [Value::Array(_, entries)]
            if entries.contains(&entry("level", "1"))
                && entries.contains(&entry("posinset", "1"))
                && entries.contains(&entry("setsize", "2"))));
        assert_eq!(
            property(&mut application, 31, ACCESSIBLE_INTERFACE, "Description"),
            text("2 tables")
        );
    }

    #[test]
    fn actions_queue_invoke_toggle_and_expand_and_refuse_other_indexes() {
        let mut application = editor();
        assert_eq!(
            property(&mut application, 11, ACTION_INTERFACE, "NActions"),
            Value::Int32(1)
        );
        assert_eq!(
            ask_node(
                &mut application,
                11,
                ACTION_INTERFACE,
                "GetName",
                vec![Value::Int32(0)]
            ),
            vec![text("click")]
        );
        assert_eq!(
            ask_node(&mut application, 11, ACTION_INTERFACE, "GetActions", vec![]),
            vec![Value::Array(
                "(sss)".to_owned(),
                vec![Value::Struct(vec![
                    text("click"),
                    text("Activates it"),
                    text("Ctrl+Enter")
                ])]
            )]
        );
        let act = |application: &mut AtspiApplication, id: u32, index: i32| {
            ask_node(
                application,
                id,
                ACTION_INTERFACE,
                "DoAction",
                vec![Value::Int32(index)],
            )
        };
        assert_eq!(act(&mut application, 11, 0), vec![Value::Boolean(true)]);
        assert_eq!(act(&mut application, 12, 0), vec![Value::Boolean(true)]);
        assert_eq!(act(&mut application, 31, 0), vec![Value::Boolean(true)]);
        assert_eq!(act(&mut application, 11, 1), vec![Value::Boolean(false)]);
        assert_eq!(act(&mut application, 11, -1), vec![Value::Boolean(false)]);
        assert_eq!(
            application.take_requests(),
            vec![
                requested(11, AccessAction::Invoke),
                requested(12, AccessAction::Toggle),
                requested(31, AccessAction::Expand),
            ]
        );
        assert!(application.take_requests().is_empty());
        for _ in 0..MAX_QUEUED_REQUESTS {
            assert_eq!(act(&mut application, 11, 0), vec![Value::Boolean(true)]);
        }
        assert_eq!(act(&mut application, 11, 0), vec![Value::Boolean(false)]);
        assert_eq!(application.take_requests().len(), MAX_QUEUED_REQUESTS);
    }

    #[test]
    fn text_reads_the_value_caret_and_selection_in_characters() {
        let mut application = editor();
        let mut read = |member: &str, body: Vec<Value>| {
            ask_node(&mut application, 20, TEXT_INTERFACE, member, body)
        };
        let offsets = |start: i32, end: i32| vec![Value::Int32(start), Value::Int32(end)];
        assert_eq!(read("GetText", offsets(0, -1)), vec![text("shop.eggdb")]);
        assert_eq!(read("GetText", offsets(5, 99)), vec![text("eggdb")]);
        assert_eq!(read("GetText", offsets(8, 3)), vec![text("")]);
        assert_eq!(read("GetNSelections", vec![]), vec![Value::Int32(1)]);
        assert_eq!(read("GetSelection", vec![Value::Int32(0)]), offsets(4, 10));
        assert_eq!(read("GetSelection", vec![Value::Int32(1)]), offsets(0, 0));
        assert_eq!(
            read(
                "GetStringAtOffset",
                vec![Value::Int32(6), Value::UInt32(GRANULARITY_WORD)]
            ),
            vec![text("eggdb"), Value::Int32(5), Value::Int32(10)]
        );
        assert_eq!(
            read(
                "GetStringAtOffset",
                vec![Value::Int32(3), Value::UInt32(GRANULARITY_LINE)]
            ),
            vec![text("shop.eggdb"), Value::Int32(0), Value::Int32(10)]
        );
        assert_eq!(
            read(
                "GetTextAtOffset",
                vec![Value::Int32(1), Value::UInt32(BOUNDARY_CHAR)]
            ),
            vec![text("h"), Value::Int32(1), Value::Int32(2)]
        );
        assert_eq!(
            read(
                "GetTextAtOffset",
                vec![Value::Int32(0), Value::UInt32(BOUNDARY_WORD_START)]
            ),
            vec![text("shop."), Value::Int32(0), Value::Int32(5)]
        );
        assert_eq!(
            read("GetCharacterAtOffset", vec![Value::Int32(4)]),
            vec![Value::Int32(46)]
        );
        assert_eq!(
            read("SetCaretOffset", vec![Value::Int32(2)]),
            vec![Value::Boolean(true)]
        );
        assert_eq!(
            read(
                "SetSelection",
                vec![Value::Int32(0), Value::Int32(0), Value::Int32(4)]
            ),
            vec![Value::Boolean(true)]
        );
        assert_eq!(
            property(&mut application, 20, TEXT_INTERFACE, "CaretOffset"),
            Value::Int32(10)
        );
        assert_eq!(
            property(&mut application, 20, TEXT_INTERFACE, "CharacterCount"),
            Value::Int32(10)
        );
        assert_eq!(
            application.take_requests(),
            vec![
                requested(20, set_caret(2, 2)),
                requested(20, set_caret(0, 4)),
            ]
        );
    }

    #[test]
    fn editable_text_turns_each_edit_into_one_new_value() {
        let mut application = editor();
        let mut edit = |member: &str, body: Vec<Value>| {
            ask_node(&mut application, 20, EDITABLE_TEXT_INTERFACE, member, body)
        };
        let done = vec![Value::Boolean(true)];
        assert_eq!(edit("SetTextContents", vec![text("db.local")]), done);
        assert_eq!(
            edit(
                "InsertText",
                vec![Value::Int32(4), text("-eu"), Value::Int32(3)]
            ),
            done
        );
        assert_eq!(
            edit(
                "InsertText",
                vec![Value::Int32(99), text("/x"), Value::Int32(-1)]
            ),
            done
        );
        assert_eq!(
            edit("DeleteText", vec![Value::Int32(4), Value::Int32(-1)]),
            done
        );
        assert_eq!(
            edit("PasteText", vec![Value::Int32(0)]),
            vec![Value::Boolean(false)]
        );
        assert_eq!(
            edit("CopyText", vec![Value::Int32(0), Value::Int32(4)]),
            vec![]
        );
        let value = |text: &str| requested(20, AccessAction::SetValue(text.to_owned()));
        assert_eq!(
            application.take_requests(),
            vec![
                value("db.local"),
                value("shop-eu.eggdb"),
                value("shop.eggdb/x"),
                value("shop"),
            ]
        );
    }

    #[test]
    fn grab_focus_queues_a_focus_request_for_nodes_that_take_it() {
        let mut application = editor();
        assert_eq!(
            ask_node(
                &mut application,
                20,
                COMPONENT_INTERFACE,
                "GrabFocus",
                vec![]
            ),
            vec![Value::Boolean(true)]
        );
        assert_eq!(
            ask_node(
                &mut application,
                43,
                COMPONENT_INTERFACE,
                "GrabFocus",
                vec![]
            ),
            vec![Value::Boolean(false)]
        );
        assert_eq!(
            application.take_requests(),
            vec![requested(20, AccessAction::Focus)]
        );
    }

    #[test]
    fn tables_find_cells_and_headers_by_row_and_column() {
        let mut application = editor();
        assert_eq!(
            property(&mut application, 40, TABLE_INTERFACE, "NRows"),
            Value::Int32(100)
        );
        assert_eq!(
            property(&mut application, 40, TABLE_INTERFACE, "NColumns"),
            Value::Int32(2)
        );
        assert_eq!(
            property(&mut application, 44, TABLE_CELL_INTERFACE, "Position"),
            Value::Struct(vec![Value::Int32(0), Value::Int32(1)])
        );
        assert_eq!(
            property(&mut application, 44, TABLE_CELL_INTERFACE, "Table"),
            node_reference(40)
        );
        assert_eq!(
            ask_node(
                &mut application,
                44,
                TABLE_CELL_INTERFACE,
                "GetRowColumnSpan",
                vec![]
            ),
            vec![
                Value::Int32(0),
                Value::Int32(1),
                Value::Int32(1),
                Value::Int32(1)
            ]
        );
        let mut table = |member: &str, row: i32, column: i32| {
            ask_node(
                &mut application,
                40,
                TABLE_INTERFACE,
                member,
                vec![Value::Int32(row), Value::Int32(column)],
            )
        };
        assert_eq!(table("GetAccessibleAt", 0, 1), vec![node_reference(44)]);
        assert_eq!(table("GetAccessibleAt", 50, 0), vec![null_reference()]);
        assert_eq!(table("GetIndexAt", 1, 1), vec![Value::Int32(3)]);
        assert_eq!(table("GetIndexAt", 100, 0), vec![Value::Int32(-1)]);
        assert_eq!(table("IsSelected", 0, 1), vec![Value::Boolean(true)]);
        assert_eq!(table("GetRowExtentAt", 0, 0), vec![Value::Int32(1)]);
        let mut by_index = |member: &str, index: i32| {
            ask_node(
                &mut application,
                40,
                TABLE_INTERFACE,
                member,
                vec![Value::Int32(index)],
            )
        };
        assert_eq!(by_index("GetColumnHeader", 1), vec![node_reference(42)]);
        assert_eq!(by_index("GetColumnDescription", 0), vec![text("id")]);
        assert_eq!(by_index("GetRowAtIndex", 3), vec![Value::Int32(1)]);
        assert_eq!(by_index("GetColumnAtIndex", 3), vec![Value::Int32(1)]);
        assert_eq!(by_index("GetRowAtIndex", 200), vec![Value::Int32(-1)]);
    }

    #[test]
    fn selection_reports_the_selected_child_and_selects_by_index() {
        let mut application = editor();
        assert_eq!(
            property(
                &mut application,
                30,
                SELECTION_INTERFACE,
                "NSelectedChildren"
            ),
            Value::Int32(1)
        );
        let mut select = |member: &str, index: i32| {
            ask_node(
                &mut application,
                30,
                SELECTION_INTERFACE,
                member,
                vec![Value::Int32(index)],
            )
        };
        assert_eq!(select("GetSelectedChild", 0), vec![node_reference(31)]);
        assert_eq!(select("GetSelectedChild", 1), vec![null_reference()]);
        assert_eq!(select("IsChildSelected", 0), vec![Value::Boolean(true)]);
        assert_eq!(select("SelectChild", 1), vec![Value::Boolean(true)]);
        assert_eq!(select("SelectChild", 9), vec![Value::Boolean(false)]);
        assert_eq!(
            application.take_requests(),
            vec![requested(32, AccessAction::Select)]
        );
    }

    #[test]
    fn text_changes_are_a_deletion_then_an_insertion_between_the_common_ends() {
        let object = Object::Node(20);
        let change = |detail: &str, offset: i32| {
            (
                object.path(),
                "TextChanged".to_owned(),
                detail.to_owned(),
                Some(offset),
            )
        };
        let deleted = text_changed_events(object, "shop.eggdb", "shop");
        assert_eq!(summary(&deleted), vec![change("delete", 4)]);
        assert_eq!(
            deleted.first().map(|event| event.body.get(2..4)),
            Some(Some(
                &[Value::Int32(6), Value::Variant(Box::new(text(".eggdb")))][..]
            ))
        );
        let replaced = text_changed_events(object, "aXb", "aYYb");
        assert_eq!(
            summary(&replaced),
            vec![change("delete", 1), change("insert", 1)]
        );
        let unicode = text_changed_events(object, "héllo", "hé llo");
        assert_eq!(summary(&unicode), vec![change("insert", 2)]);
        assert!(text_changed_events(object, "same", "same").is_empty());
    }

    #[test]
    fn tree_changes_raise_text_caret_expanded_and_selection_events() {
        let mut application = editor();
        let mut nodes = editor_tree();
        nodes[4].value = Some("shop.eu.eggdb".to_owned());
        nodes[4].caret = Some(AccessCaret {
            anchor: 7,
            caret: 7,
        });
        nodes[6].state.expanded = Some(true);
        nodes[6].description = Some("3 tables".to_owned());
        nodes[12].state.selected = false;
        let events = application.set_tree(&nodes);
        let entry = |id: u32, member: &str, detail: &str, detail1: i32| {
            (
                Object::Node(id).path(),
                member.to_owned(),
                detail.to_owned(),
                Some(detail1),
            )
        };
        assert_eq!(
            summary(&events),
            vec![
                entry(20, "TextChanged", "insert", 6),
                entry(20, "TextCaretMoved", "", 7),
                entry(20, "TextSelectionChanged", "", 0),
                entry(31, "PropertyChange", "accessible-description", 0),
                entry(31, "StateChanged", "expanded", 1),
                entry(44, "StateChanged", "selected", 0),
                entry(40, "SelectionChanged", "", 0),
            ]
        );
        assert_eq!(
            events.first().map(|event| event.body.get(2..4)),
            Some(Some(
                &[Value::Int32(3), Value::Variant(Box::new(text("u.e")))][..]
            ))
        );
        for mut event in events {
            event.serial = 9;
            let decoded = Message::decode(&event.encode().unwrap()).unwrap();
            assert_eq!(signature_of_body(&decoded), "siiva{sv}");
        }
        assert!(application.set_tree(&nodes).is_empty());
    }
}
