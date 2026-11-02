//! The pure parts of the Windows UI Automation bridge, compiled and tested on every operating
//! system: the UI Automation ids the bridge answers with, each [`AccessRole`]'s control type,
//! walking the accessibility tree the way a fragment navigates (the window's own node is the
//! fragment root the window hosts), finding the element under a point, and the bounding
//! rectangles in screen pixels. The values come from Microsoft's public UI Automation
//! documentation.

use super::{AccessNode, AccessRect, AccessRole, focused_node};

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
/// `UIA_HeadingLevelPropertyId`.
pub const HEADING_LEVEL_PROPERTY: i32 = 30173;
/// `HeadingLevel1`, the level of the screen's heading.
pub const HEADING_LEVEL_1: i32 = 80051;

/// `UIA_ButtonControlTypeId`.
pub const BUTTON_CONTROL: i32 = 50000;
/// `UIA_CheckBoxControlTypeId`.
pub const CHECK_BOX_CONTROL: i32 = 50002;
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
/// `UIA_GroupControlTypeId`.
pub const GROUP_CONTROL: i32 = 50026;
/// `UIA_PaneControlTypeId`.
pub const PANE_CONTROL: i32 = 50033;

/// `UIA_AutomationFocusChangedEventId`.
pub const FOCUS_CHANGED_EVENT: i32 = 20005;

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
        AccessRole::Heading | AccessRole::Status => TEXT_CONTROL,
        AccessRole::TabList => TAB_CONTROL,
        AccessRole::Tab => TAB_ITEM_CONTROL,
        AccessRole::TextField => EDIT_CONTROL,
        AccessRole::Button => BUTTON_CONTROL,
        AccessRole::CheckBox => CHECK_BOX_CONTROL,
        AccessRole::Link => HYPERLINK_CONTROL,
    })
}

/// Whether a node can take the keyboard focus.
pub fn is_keyboard_focusable(node: &AccessNode) -> bool {
    !node.state.disabled
        && matches!(
            node.role,
            AccessRole::TextField
                | AccessRole::Button
                | AccessRole::CheckBox
                | AccessRole::Tab
                | AccessRole::Link
        )
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
    use super::super::AccessState;
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
            value: None,
            placeholder: None,
            state: AccessState::default(),
            bounds: AccessRect {
                x: bounds.0,
                y: bounds.1,
                width: bounds.2,
                height: bounds.3,
            },
        }
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
}
