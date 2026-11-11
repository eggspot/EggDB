//! The pure parts of the Windows UI Automation bridge, compiled and tested on every operating
//! system: the UI Automation ids the bridge answers with, each [`AccessRole`]'s control type,
//! walking the accessibility tree the way a fragment navigates (the window's own node is the
//! fragment root the window hosts), finding the element under a point, and the bounding
//! rectangles in screen pixels, which control patterns a node offers and which property
//! changes and events a new tree brings. The values come from Microsoft's public UI
//! Automation documentation.

use std::collections::HashMap;

use super::{AccessCell, AccessNode, AccessRect, AccessRole, focused_node};

/// `UiaRootObjectId`: the object id `WM_GETOBJECT` carries when UI Automation asks a window
/// for its root provider.
pub const ROOT_OBJECT_ID: i32 = -25;
/// `UiaAppendRuntimeId`: the first part of a runtime id the system completes with the
/// hosting window's id.
pub const APPEND_RUNTIME_ID: i32 = 3;
/// `ProviderOptions_ServerSideProvider`.
pub const SERVER_SIDE_PROVIDER: i32 = 0x2;

/// `UIA_ControlTypePropertyId`.
pub const CONTROL_TYPE_PROPERTY: i32 = 30003;
/// `UIA_NamePropertyId`.
pub const NAME_PROPERTY: i32 = 30005;
/// `UIA_HasKeyboardFocusPropertyId`.
pub const HAS_KEYBOARD_FOCUS_PROPERTY: i32 = 30008;
/// `UIA_IsKeyboardFocusablePropertyId`.
pub const IS_KEYBOARD_FOCUSABLE_PROPERTY: i32 = 30009;
/// `UIA_IsEnabledPropertyId`.
pub const IS_ENABLED_PROPERTY: i32 = 30010;
/// `UIA_AutomationIdPropertyId`.
pub const AUTOMATION_ID_PROPERTY: i32 = 30011;
/// `UIA_HelpTextPropertyId`: the bridge gives a text field's placeholder here.
pub const HELP_TEXT_PROPERTY: i32 = 30013;
/// `UIA_LiveSettingPropertyId`.
pub const LIVE_SETTING_PROPERTY: i32 = 30135;
/// `Polite`, the live setting of a status line.
pub const LIVE_SETTING_POLITE: i32 = 1;
/// `Assertive`, the live setting of an alert.
pub const LIVE_SETTING_ASSERTIVE: i32 = 2;
/// `UIA_AcceleratorKeyPropertyId`: the bridge gives a node's shortcut here.
pub const ACCELERATOR_KEY_PROPERTY: i32 = 30006;
/// `UIA_IsPasswordPropertyId`.
pub const IS_PASSWORD_PROPERTY: i32 = 30019;
/// `UIA_ValueValuePropertyId`.
pub const VALUE_VALUE_PROPERTY: i32 = 30045;
/// `UIA_ValueIsReadOnlyPropertyId`.
pub const VALUE_IS_READ_ONLY_PROPERTY: i32 = 30046;
/// `UIA_GridRowCountPropertyId`.
pub const GRID_ROW_COUNT_PROPERTY: i32 = 30062;
/// `UIA_GridColumnCountPropertyId`.
pub const GRID_COLUMN_COUNT_PROPERTY: i32 = 30063;
/// `UIA_GridItemRowPropertyId`.
pub const GRID_ITEM_ROW_PROPERTY: i32 = 30064;
/// `UIA_GridItemColumnPropertyId`.
pub const GRID_ITEM_COLUMN_PROPERTY: i32 = 30065;
/// `UIA_ExpandCollapseExpandCollapseStatePropertyId`.
pub const EXPAND_COLLAPSE_STATE_PROPERTY: i32 = 30070;
/// `UIA_SelectionItemIsSelectedPropertyId`.
pub const SELECTION_ITEM_IS_SELECTED_PROPERTY: i32 = 30079;
/// `UIA_ToggleToggleStatePropertyId`.
pub const TOGGLE_STATE_PROPERTY: i32 = 30086;
/// `UIA_IsDataValidForFormPropertyId`.
pub const IS_DATA_VALID_FOR_FORM_PROPERTY: i32 = 30103;
/// `UIA_PositionInSetPropertyId`.
pub const POSITION_IN_SET_PROPERTY: i32 = 30152;
/// `UIA_SizeOfSetPropertyId`.
pub const SIZE_OF_SET_PROPERTY: i32 = 30153;
/// `UIA_LevelPropertyId`.
pub const LEVEL_PROPERTY: i32 = 30154;
/// `UIA_FullDescriptionPropertyId`: the bridge gives a node's description here.
pub const FULL_DESCRIPTION_PROPERTY: i32 = 30159;
/// `UIA_HeadingLevelPropertyId`.
pub const HEADING_LEVEL_PROPERTY: i32 = 30173;
/// `HeadingLevel1`, the level of the screen's heading.
pub const HEADING_LEVEL_1: i32 = 80051;

/// `UIA_ButtonControlTypeId`.
pub const BUTTON_CONTROL: i32 = 50000;
/// `UIA_MenuControlTypeId`.
pub const MENU_CONTROL: i32 = 50009;
/// `UIA_MenuItemControlTypeId`.
pub const MENU_ITEM_CONTROL: i32 = 50011;
/// `UIA_CheckBoxControlTypeId`.
pub const CHECK_BOX_CONTROL: i32 = 50002;
/// `UIA_ComboBoxControlTypeId`.
pub const COMBO_BOX_CONTROL: i32 = 50003;
/// `UIA_EditControlTypeId`.
pub const EDIT_CONTROL: i32 = 50004;
/// `UIA_HyperlinkControlTypeId`.
pub const HYPERLINK_CONTROL: i32 = 50005;
/// `UIA_TabControlTypeId`.
pub const TAB_CONTROL: i32 = 50018;
/// `UIA_TabItemControlTypeId`.
pub const TAB_ITEM_CONTROL: i32 = 50019;
/// `UIA_TextControlTypeId`.
pub const TEXT_CONTROL: i32 = 50020;
/// `UIA_ToolBarControlTypeId`.
pub const TOOL_BAR_CONTROL: i32 = 50021;
/// `UIA_TreeControlTypeId`.
pub const TREE_CONTROL: i32 = 50023;
/// `UIA_TreeItemControlTypeId`.
pub const TREE_ITEM_CONTROL: i32 = 50024;
/// `UIA_GroupControlTypeId`.
pub const GROUP_CONTROL: i32 = 50026;
/// `UIA_DataGridControlTypeId`.
pub const DATA_GRID_CONTROL: i32 = 50028;
/// `UIA_DataItemControlTypeId`.
pub const DATA_ITEM_CONTROL: i32 = 50029;
/// `UIA_PaneControlTypeId`.
pub const PANE_CONTROL: i32 = 50033;
/// `UIA_HeaderItemControlTypeId`.
pub const HEADER_ITEM_CONTROL: i32 = 50035;

/// `UIA_InvokePatternId`.
pub const INVOKE_PATTERN: i32 = 10000;
/// `UIA_ValuePatternId`.
pub const VALUE_PATTERN: i32 = 10002;
/// `UIA_ExpandCollapsePatternId`.
pub const EXPAND_COLLAPSE_PATTERN: i32 = 10005;
/// `UIA_GridPatternId`.
pub const GRID_PATTERN: i32 = 10006;
/// `UIA_GridItemPatternId`.
pub const GRID_ITEM_PATTERN: i32 = 10007;
/// `UIA_SelectionItemPatternId`.
pub const SELECTION_ITEM_PATTERN: i32 = 10010;
/// `UIA_TablePatternId`.
pub const TABLE_PATTERN: i32 = 10012;
/// `UIA_TableItemPatternId`.
pub const TABLE_ITEM_PATTERN: i32 = 10013;
/// `UIA_TogglePatternId`.
pub const TOGGLE_PATTERN: i32 = 10015;

/// Every control pattern the bridge implements.
pub const PATTERNS: [i32; 9] = [
    INVOKE_PATTERN,
    VALUE_PATTERN,
    EXPAND_COLLAPSE_PATTERN,
    GRID_PATTERN,
    GRID_ITEM_PATTERN,
    SELECTION_ITEM_PATTERN,
    TABLE_PATTERN,
    TABLE_ITEM_PATTERN,
    TOGGLE_PATTERN,
];

/// `ToggleState_Off`.
pub const TOGGLE_OFF: i32 = 0;
/// `ToggleState_On`.
pub const TOGGLE_ON: i32 = 1;
/// `ExpandCollapseState_Collapsed`.
pub const COLLAPSED: i32 = 0;
/// `ExpandCollapseState_Expanded`.
pub const EXPANDED: i32 = 1;
/// `ExpandCollapseState_LeafNode`.
pub const LEAF_NODE: i32 = 3;
/// `RowOrColumnMajor_RowMajor`, how a grid's cells are laid out.
pub const ROW_MAJOR: i32 = 0;

/// `UIA_AutomationFocusChangedEventId`.
pub const FOCUS_CHANGED_EVENT: i32 = 20005;
/// `UIA_Invoke_InvokedEventId`.
pub const INVOKED_EVENT: i32 = 20009;
/// `UIA_SelectionItem_ElementSelectedEventId`.
pub const ELEMENT_SELECTED_EVENT: i32 = 20012;
/// `UIA_LiveRegionChangedEventId`.
pub const LIVE_REGION_CHANGED_EVENT: i32 = 20024;

/// A rectangle in screen pixels, laid out as UI Automation's `UiaRect`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct UiaRect {
    /// The left edge.
    pub left: f64,
    /// The top edge.
    pub top: f64,
    /// The width.
    pub width: f64,
    /// The height.
    pub height: f64,
}

/// An element the bridge exposes: the fragment root the window hosts, or one node of the
/// accessibility tree by id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Element {
    /// The fragment root, standing for the window's own node.
    Root,
    /// The node with this id.
    Node(u32),
}

/// A `NavigateDirection` from one fragment to another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// `NavigateDirection_Parent`.
    Parent,
    /// `NavigateDirection_NextSibling`.
    NextSibling,
    /// `NavigateDirection_PreviousSibling`.
    PreviousSibling,
    /// `NavigateDirection_FirstChild`.
    FirstChild,
    /// `NavigateDirection_LastChild`.
    LastChild,
}

impl Direction {
    /// The direction a `NavigateDirection` value names, `None` for an unknown value.
    pub fn from_raw(value: i32) -> Option<Self> {
        Some(match value {
            0 => Self::Parent,
            1 => Self::NextSibling,
            2 => Self::PreviousSibling,
            3 => Self::FirstChild,
            4 => Self::LastChild,
            _ => return None,
        })
    }
}

/// The UI Automation control type of a role; `None` for the window, which the system already
/// exposes.
pub fn control_type(role: AccessRole) -> Option<i32> {
    Some(match role {
        AccessRole::Window => return None,
        AccessRole::Group => GROUP_CONTROL,
        AccessRole::TabPanel => PANE_CONTROL,
        AccessRole::Heading | AccessRole::Status | AccessRole::Alert => TEXT_CONTROL,
        AccessRole::TabList => TAB_CONTROL,
        AccessRole::Tab => TAB_ITEM_CONTROL,
        AccessRole::TextField | AccessRole::SearchBox | AccessRole::TextArea => EDIT_CONTROL,
        AccessRole::Button => BUTTON_CONTROL,
        AccessRole::CheckBox => CHECK_BOX_CONTROL,
        AccessRole::Link => HYPERLINK_CONTROL,
        AccessRole::Toolbar => TOOL_BAR_CONTROL,
        AccessRole::ComboBox => COMBO_BOX_CONTROL,
        AccessRole::Tree => TREE_CONTROL,
        AccessRole::TreeItem => TREE_ITEM_CONTROL,
        AccessRole::Grid => DATA_GRID_CONTROL,
        AccessRole::ColumnHeader => HEADER_ITEM_CONTROL,
        AccessRole::Cell => DATA_ITEM_CONTROL,
        AccessRole::Menu => MENU_CONTROL,
        AccessRole::MenuItem => MENU_ITEM_CONTROL,
    })
}

/// Whether a node can take the keyboard focus: it offers the focus action, or it is a
/// control that always can.
pub fn is_keyboard_focusable(node: &AccessNode) -> bool {
    !node.state.disabled
        && (node.actions.focus
            || matches!(
                node.role,
                AccessRole::TextField
                    | AccessRole::SearchBox
                    | AccessRole::Button
                    | AccessRole::CheckBox
                    | AccessRole::Tab
                    | AccessRole::Link
            ))
}

/// Whether a node offers a control pattern, from the actions it offers, its role and the
/// fields it fills.
pub fn supports_pattern(node: &AccessNode, pattern: i32) -> bool {
    let actions = node.actions;
    match pattern {
        INVOKE_PATTERN => actions.invoke,
        TOGGLE_PATTERN => actions.toggle || node.state.checked.is_some(),
        VALUE_PATTERN => {
            node.value.is_some()
                && matches!(
                    node.role,
                    AccessRole::TextField
                        | AccessRole::SearchBox
                        | AccessRole::TextArea
                        | AccessRole::ComboBox
                        | AccessRole::Cell
                )
        }
        EXPAND_COLLAPSE_PATTERN => actions.expand_collapse || node.state.expanded.is_some(),
        SELECTION_ITEM_PATTERN => actions.select,
        GRID_PATTERN | TABLE_PATTERN => node.table.is_some() && node.role == AccessRole::Grid,
        GRID_ITEM_PATTERN | TABLE_ITEM_PATTERN => {
            node.cell.is_some() && node.role == AccessRole::Cell
        }
        _ => false,
    }
}

/// The control patterns a node offers, in [`PATTERNS`] order.
pub fn patterns(node: &AccessNode) -> Vec<i32> {
    PATTERNS
        .into_iter()
        .filter(|&pattern| supports_pattern(node, pattern))
        .collect()
}

/// A node's `ToggleState`: on when it is checked, off otherwise.
pub fn toggle_state(node: &AccessNode) -> i32 {
    if node.state.checked == Some(true) {
        TOGGLE_ON
    } else {
        TOGGLE_OFF
    }
}

/// A node's `ExpandCollapseState`; a node that cannot expand is a leaf.
pub fn expand_collapse_state(node: &AccessNode) -> i32 {
    match node.state.expanded {
        Some(true) => EXPANDED,
        Some(false) => COLLAPSED,
        None => LEAF_NODE,
    }
}

/// Whether a node's value can only be read.
pub fn is_value_read_only(node: &AccessNode) -> bool {
    node.state.read_only || !node.actions.set_value
}

/// A count or index as UI Automation's `int`, the largest one for what does not fit.
pub fn saturating_int(value: usize) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

fn node_by_id(nodes: &[AccessNode], id: u32) -> Option<&AccessNode> {
    nodes.iter().find(|node| node.id == id)
}

fn nearest_ancestor(
    nodes: &[AccessNode],
    id: u32,
    wanted: impl Fn(AccessRole) -> bool,
) -> Option<u32> {
    let mut parent = node_by_id(nodes, id)?.parent;
    for _ in 0..nodes.len() {
        let node = node_by_id(nodes, parent?)?;
        if wanted(node.role) {
            return Some(node.id);
        }
        parent = node.parent;
    }
    None
}

/// The grid a cell or column header belongs to: its nearest ancestor that is a grid.
pub fn containing_grid(nodes: &[AccessNode], id: u32) -> Option<u32> {
    nearest_ancestor(nodes, id, |role| role == AccessRole::Grid)
}

/// The container a selectable node is selected in: its nearest ancestor that is a tab list,
/// a tree or a grid.
pub fn selection_container(nodes: &[AccessNode], id: u32) -> Option<u32> {
    nearest_ancestor(nodes, id, |role| {
        matches!(
            role,
            AccessRole::TabList | AccessRole::Tree | AccessRole::Grid
        )
    })
}

/// The cell of grid `grid` at `row` and `column`, when the tree holds it.
pub fn grid_item(nodes: &[AccessNode], grid: u32, row: usize, column: usize) -> Option<u32> {
    let wanted = Some(AccessCell { row, column });
    nodes
        .iter()
        .filter(|node| node.role == AccessRole::Cell && node.cell == wanted)
        .find(|node| containing_grid(nodes, node.id) == Some(grid))
        .map(|node| node.id)
}

/// The column headers of grid `grid` in reading order, only those of `column` when it is
/// given.
pub fn column_headers(nodes: &[AccessNode], grid: u32, column: Option<usize>) -> Vec<u32> {
    nodes
        .iter()
        .filter(|node| node.role == AccessRole::ColumnHeader)
        .filter(|node| column.is_none() || node.cell.map(|cell| cell.column) == column)
        .filter(|node| containing_grid(nodes, node.id) == Some(grid))
        .map(|node| node.id)
        .collect()
}

/// The UI Automation properties that changed between two trees, as (node id, property id),
/// for the exposed nodes present in both: values, toggle and expand/collapse states, and
/// selection, each only where the new node offers its pattern.
pub fn property_changes(old: &[AccessNode], new: &[AccessNode]) -> Vec<(u32, i32)> {
    let before: HashMap<u32, &AccessNode> = old.iter().map(|node| (node.id, node)).collect();
    let mut changes = Vec::new();
    for node in new.iter().filter(|node| node.role != AccessRole::Window) {
        let Some(old) = before.get(&node.id) else {
            continue;
        };
        let checks = [
            (VALUE_PATTERN, VALUE_VALUE_PROPERTY, old.value != node.value),
            (
                TOGGLE_PATTERN,
                TOGGLE_STATE_PROPERTY,
                toggle_state(old) != toggle_state(node),
            ),
            (
                EXPAND_COLLAPSE_PATTERN,
                EXPAND_COLLAPSE_STATE_PROPERTY,
                expand_collapse_state(old) != expand_collapse_state(node),
            ),
            (
                SELECTION_ITEM_PATTERN,
                SELECTION_ITEM_IS_SELECTED_PROPERTY,
                old.state.selected != node.state.selected,
            ),
        ];
        for (pattern, property, changed) in checks {
            if changed && supports_pattern(node, pattern) {
                changes.push((node.id, property));
            }
        }
    }
    changes
}

/// The selectable nodes present in both trees that became selected.
pub fn newly_selected(old: &[AccessNode], new: &[AccessNode]) -> Vec<u32> {
    let before: HashMap<u32, &AccessNode> = old.iter().map(|node| (node.id, node)).collect();
    new.iter()
        .filter(|node| node.state.selected && supports_pattern(node, SELECTION_ITEM_PATTERN))
        .filter(|node| before.get(&node.id).is_some_and(|old| !old.state.selected))
        .map(|node| node.id)
        .collect()
}

/// The status lines and alerts whose text appeared or changed between two trees; nothing
/// for the first tree, so opening a window announces nothing.
pub fn live_regions_changed(old: &[AccessNode], new: &[AccessNode]) -> Vec<u32> {
    if old.is_empty() {
        return Vec::new();
    }
    let before: HashMap<u32, &AccessNode> = old.iter().map(|node| (node.id, node)).collect();
    new.iter()
        .filter(|node| matches!(node.role, AccessRole::Status | AccessRole::Alert))
        .filter(|node| !node.name.is_empty())
        .filter(|node| before.get(&node.id).is_none_or(|old| old.name != node.name))
        .map(|node| node.id)
        .collect()
}

/// The node with this id, unless it is the window's own node.
pub fn exposed_node(nodes: &[AccessNode], id: u32) -> Option<&AccessNode> {
    nodes
        .iter()
        .find(|node| node.id == id && node.role != AccessRole::Window)
}

/// The element a node hangs from once the window's own node is replaced by the fragment
/// root; `None` when the node is not exposed.
pub fn exposed_parent(nodes: &[AccessNode], id: u32) -> Option<Element> {
    let mut parent = exposed_node(nodes, id)?.parent;
    for _ in 0..nodes.len() {
        let Some(parent_id) = parent else {
            return Some(Element::Root);
        };
        match nodes.iter().find(|node| node.id == parent_id) {
            None => return Some(Element::Root),
            Some(node) if node.role == AccessRole::Window => parent = node.parent,
            Some(node) => return Some(Element::Node(node.id)),
        }
    }
    Some(Element::Root)
}

/// The exposed children of an element, in reading order.
pub fn children(nodes: &[AccessNode], of: Element) -> Vec<u32> {
    nodes
        .iter()
        .filter(|node| {
            node.role != AccessRole::Window && exposed_parent(nodes, node.id) == Some(of)
        })
        .map(|node| node.id)
        .collect()
}

/// Where a fragment's `Navigate` leads; the root's parent and siblings are the hosting
/// window's to give, so they are `None` here.
pub fn navigate(nodes: &[AccessNode], from: Element, direction: Direction) -> Option<Element> {
    match direction {
        Direction::FirstChild => children(nodes, from).first().copied().map(Element::Node),
        Direction::LastChild => children(nodes, from).last().copied().map(Element::Node),
        Direction::Parent => match from {
            Element::Root => None,
            Element::Node(id) => exposed_parent(nodes, id),
        },
        Direction::NextSibling | Direction::PreviousSibling => {
            let Element::Node(id) = from else {
                return None;
            };
            let siblings = children(nodes, exposed_parent(nodes, id)?);
            let index = siblings.iter().position(|&sibling| sibling == id)?;
            let sibling = if direction == Direction::NextSibling {
                siblings.get(index.checked_add(1)?)
            } else {
                siblings.get(index.checked_sub(1)?)
            };
            sibling.copied().map(Element::Node)
        }
    }
}

fn contains(bounds: AccessRect, x: f32, y: f32) -> bool {
    x >= bounds.x && y >= bounds.y && x < bounds.x + bounds.width && y < bounds.y + bounds.height
}

/// The innermost exposed node under a point in logical pixels from the window's top left;
/// later nodes in reading order win over earlier ones that also contain it.
pub fn element_at(nodes: &[AccessNode], x: f32, y: f32) -> Option<u32> {
    nodes
        .iter()
        .rev()
        .find(|node| node.role != AccessRole::Window && contains(node.bounds, x, y))
        .map(|node| node.id)
}

/// A node's bounds in screen pixels, for a window at `scale` device pixels per logical pixel
/// whose client area's top left is at `client_origin` on the screen.
pub fn screen_rect(bounds: AccessRect, scale: f32, client_origin: (i32, i32)) -> UiaRect {
    let scale = f64::from(scale);
    UiaRect {
        left: f64::from(client_origin.0) + f64::from(bounds.x) * scale,
        top: f64::from(client_origin.1) + f64::from(bounds.y) * scale,
        width: f64::from(bounds.width) * scale,
        height: f64::from(bounds.height) * scale,
    }
}

/// A node's runtime id, completed by the system with the hosting window's id.
pub fn runtime_id(id: u32) -> [i32; 2] {
    [APPEND_RUNTIME_ID, id as i32]
}

/// The node that took the focus between two trees, when the focus moved to an exposed node.
pub fn focus_moved_to(old: &[AccessNode], new: &[AccessNode]) -> Option<u32> {
    let focused = focused_node(new)?;
    (focused_node(old) != Some(focused) && exposed_node(new, focused).is_some()).then_some(focused)
}

#[cfg(test)]
mod tests {
    use super::super::{AccessActions, AccessTable, AccessTreePosition};
    use super::*;

    fn node(
        id: u32,
        parent: Option<u32>,
        role: AccessRole,
        bounds: (f32, f32, f32, f32),
    ) -> AccessNode {
        AccessNode {
            id,
            parent,
            role,
            name: format!("node {id}"),
            bounds: AccessRect {
                x: bounds.0,
                y: bounds.1,
                width: bounds.2,
                height: bounds.3,
            },
            ..AccessNode::default()
        }
    }

    fn bare(id: u32, parent: Option<u32>, role: AccessRole) -> AccessNode {
        node(id, parent, role, (0.0, 0.0, 1.0, 1.0))
    }

    fn grid_tree() -> Vec<AccessNode> {
        let mut grid = bare(2, Some(1), AccessRole::Grid);
        grid.table = Some(AccessTable {
            rows: 2,
            columns: 2,
        });
        let mut nodes = vec![bare(1, None, AccessRole::Window), grid];
        for (id, column) in [(3, 0), (4, 1)] {
            let mut header = bare(id, Some(2), AccessRole::ColumnHeader);
            header.cell = Some(AccessCell { row: 0, column });
            nodes.push(header);
        }
        nodes.push(bare(5, Some(2), AccessRole::Group));
        for (id, row, column) in [(6, 0, 0), (7, 0, 1), (8, 1, 0), (9, 1, 1)] {
            let mut cell = bare(id, Some(5), AccessRole::Cell);
            cell.cell = Some(AccessCell { row, column });
            cell.value = Some(format!("{row}:{column}"));
            cell.actions.select = true;
            nodes.push(cell);
        }
        nodes
    }

    fn tree() -> Vec<AccessNode> {
        vec![
            node(1, None, AccessRole::Window, (0.0, 0.0, 800.0, 600.0)),
            node(2, Some(1), AccessRole::Heading, (10.0, 10.0, 200.0, 30.0)),
            node(3, Some(1), AccessRole::Group, (10.0, 50.0, 400.0, 300.0)),
            node(4, Some(3), AccessRole::TextField, (20.0, 60.0, 300.0, 32.0)),
            node(5, Some(3), AccessRole::Button, (20.0, 100.0, 100.0, 32.0)),
            node(6, Some(1), AccessRole::Link, (10.0, 400.0, 80.0, 20.0)),
        ]
    }

    #[test]
    fn roles_map_to_control_types_and_the_window_is_left_to_the_system() {
        assert_eq!(control_type(AccessRole::Window), None);
        assert_eq!(control_type(AccessRole::Button), Some(BUTTON_CONTROL));
        assert_eq!(control_type(AccessRole::TextField), Some(EDIT_CONTROL));
        assert_eq!(control_type(AccessRole::CheckBox), Some(CHECK_BOX_CONTROL));
        assert_eq!(control_type(AccessRole::Tab), Some(TAB_ITEM_CONTROL));
        assert_eq!(control_type(AccessRole::Status), Some(TEXT_CONTROL));
        let mut button = node(5, None, AccessRole::Button, (0.0, 0.0, 1.0, 1.0));
        assert!(is_keyboard_focusable(&button));
        button.state.disabled = true;
        assert!(!is_keyboard_focusable(&button));
        assert!(!is_keyboard_focusable(&node(
            2,
            None,
            AccessRole::Heading,
            (0.0, 0.0, 1.0, 1.0)
        )));
    }

    #[test]
    fn navigation_replaces_the_window_node_with_the_root() {
        let nodes = tree();
        assert_eq!(children(&nodes, Element::Root), [2, 3, 6]);
        assert_eq!(children(&nodes, Element::Node(3)), [4, 5]);
        assert_eq!(
            navigate(&nodes, Element::Root, Direction::FirstChild),
            Some(Element::Node(2))
        );
        assert_eq!(
            navigate(&nodes, Element::Root, Direction::LastChild),
            Some(Element::Node(6))
        );
        assert_eq!(navigate(&nodes, Element::Root, Direction::Parent), None);
        assert_eq!(
            navigate(&nodes, Element::Root, Direction::NextSibling),
            None
        );
        assert_eq!(
            navigate(&nodes, Element::Node(2), Direction::Parent),
            Some(Element::Root)
        );
        assert_eq!(
            navigate(&nodes, Element::Node(4), Direction::Parent),
            Some(Element::Node(3))
        );
        assert_eq!(
            navigate(&nodes, Element::Node(2), Direction::NextSibling),
            Some(Element::Node(3))
        );
        assert_eq!(
            navigate(&nodes, Element::Node(3), Direction::NextSibling),
            Some(Element::Node(6))
        );
        assert_eq!(
            navigate(&nodes, Element::Node(6), Direction::NextSibling),
            None
        );
        assert_eq!(
            navigate(&nodes, Element::Node(2), Direction::PreviousSibling),
            None
        );
        assert_eq!(
            navigate(&nodes, Element::Node(5), Direction::PreviousSibling),
            Some(Element::Node(4))
        );
        assert_eq!(
            navigate(&nodes, Element::Node(5), Direction::FirstChild),
            None
        );
        assert_eq!(navigate(&nodes, Element::Node(1), Direction::Parent), None);
        assert_eq!(
            navigate(&nodes, Element::Node(99), Direction::NextSibling),
            None
        );
        assert_eq!(Direction::from_raw(3), Some(Direction::FirstChild));
        assert_eq!(Direction::from_raw(5), None);
        assert_eq!(Direction::from_raw(-1), None);
    }

    #[test]
    fn a_parent_cycle_or_a_missing_parent_ends_at_the_root() {
        let nodes = vec![
            node(1, Some(2), AccessRole::Window, (0.0, 0.0, 1.0, 1.0)),
            node(2, Some(1), AccessRole::Window, (0.0, 0.0, 1.0, 1.0)),
            node(3, Some(1), AccessRole::Button, (0.0, 0.0, 1.0, 1.0)),
            node(4, Some(40), AccessRole::Button, (0.0, 0.0, 1.0, 1.0)),
        ];
        assert_eq!(exposed_parent(&nodes, 3), Some(Element::Root));
        assert_eq!(exposed_parent(&nodes, 4), Some(Element::Root));
        assert_eq!(exposed_parent(&nodes, 1), None);
    }

    #[test]
    fn the_innermost_node_under_a_point_is_found() {
        let nodes = tree();
        assert_eq!(element_at(&nodes, 30.0, 70.0), Some(4));
        assert_eq!(element_at(&nodes, 200.0, 200.0), Some(3));
        assert_eq!(element_at(&nodes, 700.0, 500.0), None);
        assert_eq!(element_at(&nodes, 320.0, 60.0), Some(3));
    }

    #[test]
    fn bounds_become_screen_pixels_and_runtime_ids_are_appended() {
        let bounds = AccessRect {
            x: 10.0,
            y: 20.0,
            width: 30.0,
            height: 40.0,
        };
        assert_eq!(
            screen_rect(bounds, 1.5, (100, -200)),
            UiaRect {
                left: 115.0,
                top: -170.0,
                width: 45.0,
                height: 60.0,
            }
        );
        assert_eq!(runtime_id(7), [APPEND_RUNTIME_ID, 7]);
    }

    #[test]
    fn a_focus_move_is_reported_once() {
        let old = tree();
        let mut new = tree();
        assert_eq!(focus_moved_to(&old, &new), None);
        new[3].state.focused = true;
        assert_eq!(focus_moved_to(&old, &new), Some(4));
        assert_eq!(focus_moved_to(&new, &new), None);
        let mut window_focused = tree();
        window_focused[0].state.focused = true;
        assert_eq!(focus_moved_to(&old, &window_focused), None);
    }

    #[test]
    fn the_new_roles_map_to_their_control_types() {
        assert_eq!(control_type(AccessRole::Toolbar), Some(TOOL_BAR_CONTROL));
        assert_eq!(control_type(AccessRole::ComboBox), Some(COMBO_BOX_CONTROL));
        assert_eq!(control_type(AccessRole::TextArea), Some(EDIT_CONTROL));
        assert_eq!(control_type(AccessRole::Alert), Some(TEXT_CONTROL));
        assert_eq!(control_type(AccessRole::Tree), Some(TREE_CONTROL));
        assert_eq!(control_type(AccessRole::TreeItem), Some(TREE_ITEM_CONTROL));
        assert_eq!(control_type(AccessRole::Grid), Some(DATA_GRID_CONTROL));
        assert_eq!(
            control_type(AccessRole::ColumnHeader),
            Some(HEADER_ITEM_CONTROL)
        );
        assert_eq!(control_type(AccessRole::Cell), Some(DATA_ITEM_CONTROL));
    }

    #[test]
    fn the_focus_action_makes_any_node_focusable() {
        let mut item = bare(4, None, AccessRole::TreeItem);
        assert!(!is_keyboard_focusable(&item));
        item.actions.focus = true;
        assert!(is_keyboard_focusable(&item));
        item.state.disabled = true;
        assert!(!is_keyboard_focusable(&item));
        assert!(is_keyboard_focusable(&bare(5, None, AccessRole::Link)));
    }

    #[test]
    fn patterns_follow_the_actions_role_and_fields() {
        let mut button = bare(1, None, AccessRole::Button);
        assert!(patterns(&button).is_empty());
        button.actions.invoke = true;
        assert_eq!(patterns(&button), [INVOKE_PATTERN]);

        let mut check_box = bare(2, None, AccessRole::CheckBox);
        check_box.state.checked = Some(false);
        assert_eq!(patterns(&check_box), [TOGGLE_PATTERN]);
        assert_eq!(toggle_state(&check_box), TOGGLE_OFF);
        check_box.state.checked = Some(true);
        assert_eq!(toggle_state(&check_box), TOGGLE_ON);

        let mut field = bare(3, None, AccessRole::TextField);
        assert!(!supports_pattern(&field, VALUE_PATTERN));
        field.value = Some(String::new());
        assert_eq!(patterns(&field), [VALUE_PATTERN]);
        assert!(is_value_read_only(&field));
        field.actions = AccessActions {
            set_value: true,
            focus: true,
            ..AccessActions::default()
        };
        assert!(!is_value_read_only(&field));
        field.state.read_only = true;
        assert!(is_value_read_only(&field));
        let mut heading = bare(4, None, AccessRole::Heading);
        heading.value = Some("text".to_owned());
        assert!(!supports_pattern(&heading, VALUE_PATTERN));

        let mut item = bare(5, None, AccessRole::TreeItem);
        assert_eq!(expand_collapse_state(&item), LEAF_NODE);
        item.state.expanded = Some(false);
        item.actions.select = true;
        assert_eq!(
            patterns(&item),
            [EXPAND_COLLAPSE_PATTERN, SELECTION_ITEM_PATTERN]
        );
        assert_eq!(expand_collapse_state(&item), COLLAPSED);
        item.state.expanded = Some(true);
        assert_eq!(expand_collapse_state(&item), EXPANDED);

        let nodes = grid_tree();
        assert_eq!(patterns(&nodes[1]), [GRID_PATTERN, TABLE_PATTERN]);
        assert_eq!(
            patterns(&nodes[5]),
            [
                VALUE_PATTERN,
                GRID_ITEM_PATTERN,
                SELECTION_ITEM_PATTERN,
                TABLE_ITEM_PATTERN
            ]
        );
        assert!(patterns(&nodes[2]).is_empty());
        let mut tree = bare(6, None, AccessRole::Tree);
        tree.table = Some(AccessTable::default());
        assert!(!supports_pattern(&tree, GRID_PATTERN));
        assert!(!supports_pattern(&button, 12345));
        assert_eq!(saturating_int(7), 7);
        assert_eq!(saturating_int(usize::MAX), i32::MAX);
    }

    #[test]
    fn grid_items_and_headers_are_found_under_their_grid() {
        let nodes = grid_tree();
        assert_eq!(grid_item(&nodes, 2, 1, 0), Some(8));
        assert_eq!(grid_item(&nodes, 2, 0, 1), Some(7));
        assert_eq!(grid_item(&nodes, 2, 2, 0), None);
        assert_eq!(grid_item(&nodes, 5, 1, 0), None);
        assert_eq!(containing_grid(&nodes, 9), Some(2));
        assert_eq!(containing_grid(&nodes, 2), None);
        assert_eq!(selection_container(&nodes, 9), Some(2));
        assert_eq!(selection_container(&nodes, 99), None);
        assert_eq!(column_headers(&nodes, 2, None), [3, 4]);
        assert_eq!(column_headers(&nodes, 2, Some(1)), [4]);
        assert!(column_headers(&nodes, 5, None).is_empty());
        let cycle = vec![
            bare(1, Some(2), AccessRole::Group),
            bare(2, Some(1), AccessRole::Group),
        ];
        assert_eq!(containing_grid(&cycle, 1), None);
    }

    #[test]
    fn property_changes_list_what_changed_on_nodes_in_both_trees() {
        let old = grid_tree();
        let mut new = grid_tree();
        assert!(property_changes(&old, &new).is_empty());
        new[5].value = Some("edited".to_owned());
        new[6].state.selected = true;
        new[2].value = Some("ignored without a value pattern".to_owned());
        let mut appeared = bare(10, Some(1), AccessRole::CheckBox);
        appeared.state.checked = Some(true);
        new.push(appeared);
        assert_eq!(
            property_changes(&old, &new),
            [
                (6, VALUE_VALUE_PROPERTY),
                (7, SELECTION_ITEM_IS_SELECTED_PROPERTY)
            ]
        );
        assert_eq!(newly_selected(&old, &new), [7]);
        assert!(newly_selected(&new, &old).is_empty());

        let mut check_box = bare(3, None, AccessRole::CheckBox);
        check_box.state.checked = Some(false);
        let mut item = bare(4, None, AccessRole::TreeItem);
        item.state.expanded = Some(false);
        item.tree_position = Some(AccessTreePosition {
            level: 1,
            position: 1,
            set_size: 1,
        });
        let old = vec![check_box.clone(), item.clone()];
        check_box.state.checked = Some(true);
        item.state.expanded = Some(true);
        let new = vec![check_box, item];
        assert_eq!(
            property_changes(&old, &new),
            [
                (3, TOGGLE_STATE_PROPERTY),
                (4, EXPAND_COLLAPSE_STATE_PROPERTY)
            ]
        );
    }

    #[test]
    fn live_regions_announce_new_and_changed_text_after_the_first_tree() {
        let mut status = bare(2, Some(1), AccessRole::Status);
        status.name = "Connecting".to_owned();
        let old = vec![bare(1, None, AccessRole::Window), status.clone()];
        assert!(live_regions_changed(&[], &old).is_empty());
        assert!(live_regions_changed(&old, &old).is_empty());
        let mut new = old.clone();
        new[1].name = "Connected".to_owned();
        let mut alert = bare(3, Some(1), AccessRole::Alert);
        alert.name = "syntax error".to_owned();
        new.push(alert);
        let mut silent = bare(4, Some(1), AccessRole::Alert);
        silent.name = String::new();
        new.push(silent);
        assert_eq!(live_regions_changed(&old, &new), [2, 3]);
    }
}
