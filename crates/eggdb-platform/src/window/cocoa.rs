//! The pure parts of the macOS (Cocoa) window backend, compiled and tested on every operating
//! system: translating `NSEvent` key codes, modifier flags and characters into the GUI's keys,
//! turning a view's size in points and its backing scale factor into device pixels, and placing
//! a frame's image at the top left of a view whose origin is at the bottom left, and mapping
//! accessibility nodes to NSAccessibility roles, frames, values, ranges, actions and
//! notifications, and turning mouse events' positions, button numbers and scroll deltas into
//! the GUI's pointer events. The values come from Apple's public AppKit, Core Graphics and
//! NSAccessibility documentation.

use super::{
    AccessAction, AccessCaret, AccessNode, AccessRect, AccessRole, Key, Modifiers, PointerButton,
};

/// `NSEventModifierFlagShift`.
pub const FLAG_SHIFT: u64 = 1 << 17;
/// `NSEventModifierFlagControl`.
pub const FLAG_CONTROL: u64 = 1 << 18;
/// `NSEventModifierFlagOption`.
pub const FLAG_OPTION: u64 = 1 << 19;
/// `NSEventModifierFlagCommand`.
pub const FLAG_COMMAND: u64 = 1 << 20;

/// `kVK_Return`.
pub const KEY_RETURN: u16 = 0x24;
/// `kVK_Tab`.
pub const KEY_TAB: u16 = 0x30;
/// `kVK_Space`.
pub const KEY_SPACE: u16 = 0x31;
/// `kVK_Delete`, the key labelled Delete that erases backwards.
pub const KEY_DELETE: u16 = 0x33;
/// `kVK_Escape`.
pub const KEY_ESCAPE: u16 = 0x35;
/// `kVK_ANSI_KeypadEnter`.
pub const KEY_KEYPAD_ENTER: u16 = 0x4C;
/// `kVK_Home`.
pub const KEY_HOME: u16 = 0x73;
/// `kVK_ForwardDelete`.
pub const KEY_FORWARD_DELETE: u16 = 0x75;
/// `kVK_F4`.
pub const KEY_F4: u16 = 0x76;
/// `kVK_F5`.
pub const KEY_F5: u16 = 0x60;
/// `kVK_F2`.
pub const KEY_F2: u16 = 0x78;
/// `kVK_F10`.
pub const KEY_F10: u16 = 0x6D;
/// `kVK_ContextualMenu`, the menu key some keyboards have.
pub const KEY_CONTEXTUAL_MENU: u16 = 0x6E;
/// `kVK_PageUp`.
pub const KEY_PAGE_UP: u16 = 0x74;
/// `kVK_PageDown`.
pub const KEY_PAGE_DOWN: u16 = 0x79;
/// `kVK_End`.
pub const KEY_END: u16 = 0x77;
/// `kVK_LeftArrow`.
pub const KEY_LEFT: u16 = 0x7B;
/// `kVK_RightArrow`.
pub const KEY_RIGHT: u16 = 0x7C;
/// `kVK_DownArrow`.
pub const KEY_DOWN: u16 = 0x7D;
/// `kVK_UpArrow`.
pub const KEY_UP: u16 = 0x7E;

/// A point in a view's coordinates (`CGPoint` on 64-bit systems).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CgPoint {
    /// The horizontal coordinate, in points.
    pub x: f64,
    /// The vertical coordinate, in points, growing upwards.
    pub y: f64,
}

/// A size in points (`CGSize` on 64-bit systems).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CgSize {
    /// The width in points.
    pub width: f64,
    /// The height in points.
    pub height: f64,
}

/// A rectangle in points (`CGRect` and `NSRect` on 64-bit systems).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CgRect {
    /// The bottom-left corner.
    pub origin: CgPoint,
    /// The width and height.
    pub size: CgSize,
}

/// A range of characters or indexes (`NSRange` on 64-bit systems).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NsRange {
    /// The first index.
    pub location: usize,
    /// How many indexes it covers.
    pub length: usize,
}

/// `NSAccessibilityFocusedUIElementChangedNotification`.
pub const FOCUSED_ELEMENT_CHANGED: &str = "AXFocusedUIElementChanged";
/// `NSAccessibilityValueChangedNotification`.
pub const VALUE_CHANGED: &str = "AXValueChanged";
/// `NSAccessibilitySelectedTextChangedNotification`.
pub const SELECTED_TEXT_CHANGED: &str = "AXSelectedTextChanged";
/// `NSAccessibilitySelectedChildrenChangedNotification`.
pub const SELECTED_CHILDREN_CHANGED: &str = "AXSelectedChildrenChanged";
/// `NSAccessibilitySelectedRowsChangedNotification`.
pub const SELECTED_ROWS_CHANGED: &str = "AXSelectedRowsChanged";
/// `NSAccessibilitySelectedCellsChangedNotification`.
pub const SELECTED_CELLS_CHANGED: &str = "AXSelectedCellsChanged";
/// `NSAccessibilityRowExpandedNotification`.
pub const ROW_EXPANDED: &str = "AXRowExpanded";
/// `NSAccessibilityRowCollapsedNotification`.
pub const ROW_COLLAPSED: &str = "AXRowCollapsed";

/// The modifiers held, from an `NSEvent`'s `modifierFlags`.
pub fn modifiers_from_flags(flags: u64) -> Modifiers {
    Modifiers {
        shift: flags & FLAG_SHIFT != 0,
        control: flags & FLAG_CONTROL != 0,
        alt: flags & FLAG_OPTION != 0,
        command: flags & FLAG_COMMAND != 0,
    }
}

/// The key a hardware key code stands for when it is not a key that types text.
pub fn key_from_key_code(code: u16) -> Option<Key> {
    Some(match code {
        KEY_TAB => Key::Tab,
        KEY_RETURN | KEY_KEYPAD_ENTER => Key::Enter,
        KEY_ESCAPE => Key::Escape,
        KEY_SPACE => Key::Space,
        KEY_DELETE => Key::Backspace,
        KEY_FORWARD_DELETE => Key::Delete,
        KEY_LEFT => Key::Left,
        KEY_RIGHT => Key::Right,
        KEY_UP => Key::Up,
        KEY_DOWN => Key::Down,
        KEY_HOME => Key::Home,
        KEY_END => Key::End,
        KEY_F4 => Key::F4,
        KEY_F2 => Key::F2,
        KEY_F5 => Key::F5,
        KEY_F10 => Key::F10,
        KEY_CONTEXTUAL_MENU => Key::ContextMenu,
        KEY_PAGE_UP => Key::PageUp,
        KEY_PAGE_DOWN => Key::PageDown,
        _ => return None,
    })
}

/// Whether a key's character is read from `charactersIgnoringModifiers` (with Command or
/// Control held, so Cmd+Q arrives as `q` and Ctrl+A as `a` rather than a control code) instead
/// of `characters` (which carries what Option and dead keys composed).
pub fn reads_unmodified_characters(modifiers: Modifiers) -> bool {
    modifiers.command || modifiers.control
}

/// The character keys in an `NSEvent`'s characters: control codes, the space bar (answered by
/// its key code) and the private-use range AppKit gives function keys (`U+F700` to `U+F8FF`)
/// are left out.
pub fn keys_from_characters(text: &str) -> Vec<Key> {
    text.chars()
        .filter(|&c| !c.is_control() && c != ' ' && !('\u{F700}'..='\u{F8FF}').contains(&c))
        .map(Key::Character)
        .collect()
}

/// Device pixels per point from `backingScaleFactor`, and 1 for a value that is not a positive
/// finite number.
pub fn scale_from_backing(factor: f64) -> f32 {
    if factor.is_finite() && factor > 0.0 {
        factor as f32
    } else {
        1.0
    }
}

/// A view's size in device pixels from its size in points and the scale, each side rounded
/// and kept within `0..=u32::MAX`.
pub fn device_size(size: CgSize, scale: f32) -> (u32, u32) {
    let pixels = |points: f64| {
        let scaled = (points * f64::from(scale)).round();
        if scaled.is_finite() && scaled > 0.0 {
            scaled.min(f64::from(u32::MAX)) as u32
        } else {
            0
        }
    };
    (pixels(size.width), pixels(size.height))
}

/// Where a `width` × `height` device-pixel frame is drawn in a view `view_height` points tall
/// whose origin is at the bottom left: at the top left, one device pixel per frame pixel.
pub fn frame_rect(width: u32, height: u32, scale: f32, view_height: f64) -> CgRect {
    let scale = f64::from(scale_from_backing(f64::from(scale)));
    let size = CgSize {
        width: f64::from(width) / scale,
        height: f64::from(height) / scale,
    };
    CgRect {
        origin: CgPoint {
            x: 0.0,
            y: view_height - size.height,
        },
        size,
    }
}

/// How a node is exposed through NSAccessibility: its `AXRole`, and whether its visible text is
/// its value (static text) rather than its label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AxRole {
    /// The `NSAccessibilityRole` string, such as `AXButton`.
    pub role: &'static str,
    /// Whether the name is reported as the `AXValue`, as static text does.
    pub name_is_value: bool,
}

/// The NSAccessibility subrole of a password field.
pub const SECURE_TEXT_FIELD_SUBROLE: &str = "AXSecureTextField";

/// The NSAccessibility subrole of a search field.
pub const SEARCH_FIELD_SUBROLE: &str = "AXSearchField";

/// The NSAccessibility role for `role`, `None` for the window, which `NSWindow` already exposes.
pub fn ax_role(role: AccessRole) -> Option<AxRole> {
    let (role, name_is_value) = match role {
        AccessRole::Window => return None,
        AccessRole::Group | AccessRole::TabPanel => ("AXGroup", false),
        AccessRole::Heading => ("AXHeading", true),
        AccessRole::TabList => ("AXTabGroup", false),
        AccessRole::Tab => ("AXRadioButton", false),
        AccessRole::Status => ("AXStaticText", true),
        AccessRole::TextField | AccessRole::SearchBox => ("AXTextField", false),
        AccessRole::Button => ("AXButton", false),
        AccessRole::CheckBox => ("AXCheckBox", false),
        AccessRole::Link => ("AXLink", false),
        AccessRole::Toolbar => ("AXToolbar", false),
        AccessRole::ComboBox => ("AXPopUpButton", false),
        AccessRole::TextArea => ("AXTextArea", false),
        AccessRole::Alert => ("AXGroup", false),
        AccessRole::Tree => ("AXOutline", false),
        AccessRole::TreeItem => ("AXRow", false),
        AccessRole::Grid => ("AXTable", false),
        AccessRole::ColumnHeader | AccessRole::Cell => ("AXCell", false),
        AccessRole::Menu => ("AXMenu", false),
        AccessRole::MenuItem => ("AXMenuItem", false),
    };
    Some(AxRole {
        role,
        name_is_value,
    })
}

/// A node's `accessibilityFrameInParentSpace`: `bounds` (logical pixels from the window's top
/// left, which are points) in a view `view_height` points tall whose origin is at the bottom
/// left, relative to the parent element's `parent` bounds when the parent is an element rather
/// than the view.
pub fn frame_in_parent_space(
    bounds: AccessRect,
    parent: Option<AccessRect>,
    view_height: f64,
) -> CgRect {
    let bottom_up = |rect: AccessRect| {
        (
            f64::from(rect.x),
            view_height - f64::from(rect.y) - f64::from(rect.height),
        )
    };
    let (x, y) = bottom_up(bounds);
    let (parent_x, parent_y) = parent.map_or((0.0, 0.0), bottom_up);
    CgRect {
        origin: CgPoint {
            x: x - parent_x,
            y: y - parent_y,
        },
        size: CgSize {
            width: f64::from(bounds.width),
            height: f64::from(bounds.height),
        },
    }
}

/// What a node's `accessibilityValue` is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AxValue<'a> {
    /// An `NSString`.
    Text(&'a str),
    /// An `NSNumber` of 1 for true and 0 for false.
    Flag(bool),
}

/// A node's `accessibilityValue`: the name of static text, a text node's value, a checkbox's
/// checked state or a tab's selected state; `None` for the window and nodes without a value.
pub fn ax_value(node: &AccessNode) -> Option<AxValue<'_>> {
    let ax = ax_role(node.role)?;
    if ax.name_is_value {
        return Some(AxValue::Text(&node.name));
    }
    if let Some(value) = &node.value {
        return Some(AxValue::Text(value));
    }
    if let Some(checked) = node.state.checked {
        return Some(AxValue::Flag(checked));
    }
    (node.role == AccessRole::Tab).then_some(AxValue::Flag(node.state.selected))
}

/// The request `accessibilityPerformPress` makes of a node: toggle a checkbox, invoke a
/// button, select a tab, tree item or cell, or flip an expandable node that offers nothing
/// else; `None` when the node offers none of them or is disabled.
pub fn press_action(node: &AccessNode) -> Option<AccessAction> {
    let actions = node.actions;
    let action = if actions.toggle {
        AccessAction::Toggle
    } else if actions.invoke {
        AccessAction::Invoke
    } else if actions.select {
        AccessAction::Select
    } else if actions.expand_collapse {
        match node.state.expanded {
            Some(true) => AccessAction::Collapse,
            Some(false) => AccessAction::Expand,
            None => return None,
        }
    } else {
        return None;
    };
    node.allows(&action).then_some(action)
}

/// The request `setAccessibilityExpanded:` and `setAccessibilityDisclosed:` make.
pub fn expand_action(expand: bool) -> AccessAction {
    if expand {
        AccessAction::Expand
    } else {
        AccessAction::Collapse
    }
}

/// A text node's `accessibilitySelectedTextRange`, counted in characters (`AccessCaret`'s
/// unit); `None` for a node without a caret.
pub fn selected_text_range(node: &AccessNode) -> Option<NsRange> {
    let (start, end) = node.caret?.range();
    Some(NsRange {
        location: start,
        length: end - start,
    })
}

/// The caret a screen reader's `setAccessibilitySelectedTextRange:` asks for, counted in
/// characters and kept within the node's value, with the caret at the range's end; `None`
/// for a node without a caret.
pub fn caret_from_selected_text_range(node: &AccessNode, range: NsRange) -> Option<AccessCaret> {
    node.caret?;
    let count = node
        .value
        .as_deref()
        .map_or(0, |value| value.chars().count());
    Some(AccessCaret {
        anchor: range.location.min(count),
        caret: range.location.saturating_add(range.length).min(count),
    })
}

/// The NSAccessibility selectors that only some nodes answer: the ones through which a
/// screen reader changes a node, and the getters of values only some roles have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AxSelector {
    /// `accessibilityPerformPress`.
    PerformPress,
    /// `setAccessibilityFocused:`.
    SetFocused,
    /// `accessibilityValue`.
    Value,
    /// `setAccessibilityValue:`.
    SetValue,
    /// `accessibilitySelectedTextRange`.
    SelectedTextRange,
    /// `setAccessibilitySelectedTextRange:`.
    SetSelectedTextRange,
    /// `isAccessibilityExpanded` and `isAccessibilityDisclosed`.
    Expanded,
    /// `setAccessibilityExpanded:` and `setAccessibilityDisclosed:`.
    SetExpanded,
    /// `accessibilityHelp`.
    Help,
    /// `accessibilityRowCount` and `accessibilityColumnCount`.
    RowAndColumnCount,
    /// `accessibilityRowIndexRange` and `accessibilityColumnIndexRange`.
    IndexRanges,
    /// `accessibilityDisclosureLevel`.
    DisclosureLevel,
    /// `accessibilityCellForColumn:row:` and `accessibilityColumnHeaderUIElements`.
    GridCells,
}

/// `isAccessibilitySelectorAllowed:` for a selector only some nodes answer.
pub fn ax_selector_allowed(node: &AccessNode, selector: AxSelector) -> bool {
    match selector {
        AxSelector::PerformPress => press_action(node).is_some(),
        AxSelector::SetFocused => node.allows(&AccessAction::Focus),
        AxSelector::Value => ax_value(node).is_some(),
        AxSelector::SetValue => node.allows(&AccessAction::SetValue(String::new())),
        AxSelector::SelectedTextRange => node.caret.is_some(),
        AxSelector::SetSelectedTextRange => {
            node.allows(&AccessAction::SetCaret(AccessCaret::default()))
        }
        AxSelector::Expanded => node.state.expanded.is_some(),
        AxSelector::SetExpanded => {
            node.state.expanded.is_some() && node.allows(&AccessAction::Expand)
        }
        AxSelector::Help => node.description.is_some(),
        AxSelector::RowAndColumnCount => node.table.is_some(),
        AxSelector::IndexRanges => node.cell.is_some(),
        AxSelector::DisclosureLevel => node.tree_position.is_some(),
        AxSelector::GridCells => node.role == AccessRole::Grid,
    }
}

/// A count or index as an `NSInteger`, saturating at its maximum.
pub fn ns_integer(count: usize) -> isize {
    isize::try_from(count).unwrap_or(isize::MAX)
}

/// A tree item's `accessibilityDisclosureLevel`: `0` for a top-level item.
pub fn disclosure_level(node: &AccessNode) -> usize {
    node.tree_position
        .map_or(0, |position| position.level.saturating_sub(1))
}

/// A cell's or column header's `accessibilityRowIndexRange`: its data row, one row long.
pub fn row_index_range(node: &AccessNode) -> NsRange {
    node.cell.map_or(NsRange::default(), |cell| NsRange {
        location: cell.row,
        length: 1,
    })
}

/// A cell's or column header's `accessibilityColumnIndexRange`: its column, one column wide.
pub fn column_index_range(node: &AccessNode) -> NsRange {
    node.cell.map_or(NsRange::default(), |cell| NsRange {
        location: cell.column,
        length: 1,
    })
}

/// The index in `nodes` of the parent of the node at `at`.
pub fn parent_index(nodes: &[AccessNode], at: usize) -> Option<usize> {
    let parent = nodes.get(at)?.parent?;
    nodes.iter().position(|node| node.id == parent)
}

/// The index in `nodes` of the closest ancestor of the node at `at` that has `role`.
pub fn nearest_ancestor(nodes: &[AccessNode], at: usize, role: AccessRole) -> Option<usize> {
    let mut current = at;
    for _ in 0..nodes.len() {
        current = parent_index(nodes, current)?;
        if nodes[current].role == role {
            return Some(current);
        }
    }
    None
}

/// The index in `nodes` of the cell at `column` and `row` of the grid at `grid`
/// (`accessibilityCellForColumn:row:`).
pub fn cell_at(nodes: &[AccessNode], grid: usize, column: usize, row: usize) -> Option<usize> {
    (0..nodes.len()).find(|&at| {
        let node = &nodes[at];
        node.role == AccessRole::Cell
            && node
                .cell
                .is_some_and(|cell| cell.row == row && cell.column == column)
            && nearest_ancestor(nodes, at, AccessRole::Grid) == Some(grid)
    })
}

/// The indexes in `nodes` of the column headers of the grid at `grid`, in order
/// (`accessibilityColumnHeaderUIElements`).
pub fn column_headers(nodes: &[AccessNode], grid: usize) -> Vec<usize> {
    (0..nodes.len())
        .filter(|&at| {
            nodes[at].role == AccessRole::ColumnHeader
                && nearest_ancestor(nodes, at, AccessRole::Grid) == Some(grid)
        })
        .collect()
}

/// The notifications a node's own element posts when it changes from `old` to `new`.
pub fn change_notifications(old: &AccessNode, new: &AccessNode) -> Vec<&'static str> {
    let mut names = Vec::new();
    if ax_value(old) != ax_value(new) {
        names.push(VALUE_CHANGED);
    }
    if old.caret != new.caret && new.caret.is_some() {
        names.push(SELECTED_TEXT_CHANGED);
    }
    if old.state.expanded != new.state.expanded {
        match new.state.expanded {
            Some(true) => names.push(ROW_EXPANDED),
            Some(false) => names.push(ROW_COLLAPSED),
            None => {}
        }
    }
    names
}

/// The notifications a node's containers post when its selected state changes: the role of
/// the closest ancestor that posts each one, `None` for the parent itself.
pub fn selection_change_notifications(
    role: AccessRole,
) -> &'static [(Option<AccessRole>, &'static str)] {
    match role {
        AccessRole::TreeItem => &[
            (None, SELECTED_CHILDREN_CHANGED),
            (Some(AccessRole::Tree), SELECTED_ROWS_CHANGED),
        ],
        AccessRole::Cell => &[
            (None, SELECTED_CHILDREN_CHANGED),
            (Some(AccessRole::Grid), SELECTED_CELLS_CHANGED),
        ],
        AccessRole::Tab => &[(None, SELECTED_CHILDREN_CHANGED)],
        _ => &[],
    }
}

/// `NSTrackingMouseEnteredAndExited | NSTrackingMouseMoved | NSTrackingActiveInKeyWindow |
/// NSTrackingInVisibleRect`: a tracking area that follows the view's visible rectangle and
/// reports moves and exits while the window is the key window.
pub const TRACKING_OPTIONS: u64 = 0x01 | 0x02 | 0x20 | 0x200;

/// Points of precise (touchpad) scrolling that make one line.
pub const PRECISE_SCROLL_POINTS_PER_LINE: f64 = 20.0;

/// The position of an event's `locationInWindow` (points, from the bottom left of a window
/// whose content view fills it) in device pixels from the view's top-left corner.
pub fn pointer_position(location: CgPoint, view_height: f64, scale: f32) -> (f32, f32) {
    let scale = f64::from(scale);
    let x = location.x * scale;
    let y = (view_height - location.y) * scale;
    (x as f32, y as f32)
}

/// The button of an `otherMouseDown:`'s `buttonNumber`: 2 is the middle button; others are
/// not used.
pub fn other_button(number: isize) -> Option<PointerButton> {
    (number == 2).then_some(PointerButton::Middle)
}

/// Lines down and right for a `scrollWheel:` event's `scrollingDeltaX` and `scrollingDeltaY`:
/// AppKit's deltas are positive when the content should move down or right (showing what is
/// above or left), already turned by the natural-scrolling setting, so they change sign;
/// precise (touchpad) deltas are in points and become lines of
/// [`PRECISE_SCROLL_POINTS_PER_LINE`].
pub fn wheel_lines(delta_x: f64, delta_y: f64, precise: bool) -> (f32, f32) {
    let per_line = if precise {
        PRECISE_SCROLL_POINTS_PER_LINE
    } else {
        1.0
    };
    let finite = |value: f64| if value.is_finite() { value } else { 0.0 };
    (
        (-finite(delta_y) / per_line) as f32,
        (-finite(delta_x) / per_line) as f32,
    )
}

#[cfg(test)]
mod tests {
    use super::super::{AccessActions, AccessCell, AccessState, AccessTreePosition};
    use super::*;

    fn node(id: u32, parent: Option<u32>, role: AccessRole) -> AccessNode {
        AccessNode {
            id,
            parent,
            role,
            ..AccessNode::default()
        }
    }

    fn cell(id: u32, parent: u32, role: AccessRole, row: usize, column: usize) -> AccessNode {
        AccessNode {
            cell: Some(AccessCell { row, column }),
            ..node(id, Some(parent), role)
        }
    }

    fn text_area(value: &str, anchor: usize, caret: usize) -> AccessNode {
        AccessNode {
            value: Some(value.to_owned()),
            caret: Some(AccessCaret { anchor, caret }),
            actions: AccessActions {
                set_value: true,
                focus: true,
                ..AccessActions::default()
            },
            ..node(1, None, AccessRole::TextArea)
        }
    }

    #[test]
    fn the_d278_roles_map_to_their_ax_roles() {
        let cases = [
            (AccessRole::Toolbar, "AXToolbar"),
            (AccessRole::ComboBox, "AXPopUpButton"),
            (AccessRole::TextArea, "AXTextArea"),
            (AccessRole::Alert, "AXGroup"),
            (AccessRole::Tree, "AXOutline"),
            (AccessRole::TreeItem, "AXRow"),
            (AccessRole::Grid, "AXTable"),
            (AccessRole::ColumnHeader, "AXCell"),
            (AccessRole::Cell, "AXCell"),
            (AccessRole::Menu, "AXMenu"),
            (AccessRole::MenuItem, "AXMenuItem"),
            (AccessRole::SearchBox, "AXTextField"),
        ];
        for (role, expected) in cases {
            assert_eq!(
                ax_role(role),
                Some(AxRole {
                    role: expected,
                    name_is_value: false
                })
            );
        }
    }

    #[test]
    fn values_come_from_static_text_text_checkboxes_and_tabs() {
        let status = AccessNode {
            name: "Connected".to_owned(),
            ..node(1, None, AccessRole::Status)
        };
        assert_eq!(ax_value(&status), Some(AxValue::Text("Connected")));
        assert_eq!(
            ax_value(&text_area("select 1", 0, 0)),
            Some(AxValue::Text("select 1"))
        );
        let checkbox = AccessNode {
            state: AccessState {
                checked: Some(true),
                ..AccessState::default()
            },
            ..node(2, None, AccessRole::CheckBox)
        };
        assert_eq!(ax_value(&checkbox), Some(AxValue::Flag(true)));
        assert_eq!(
            ax_value(&node(3, None, AccessRole::Tab)),
            Some(AxValue::Flag(false))
        );
        assert_eq!(ax_value(&node(4, None, AccessRole::Button)), None);
        assert_eq!(ax_value(&node(5, None, AccessRole::Window)), None);
    }

    #[test]
    fn a_press_toggles_invokes_selects_or_flips_what_the_node_offers() {
        let with = |actions: AccessActions, expanded: Option<bool>| AccessNode {
            actions,
            state: AccessState {
                expanded,
                ..AccessState::default()
            },
            ..node(1, None, AccessRole::TreeItem)
        };
        let toggle = AccessActions {
            toggle: true,
            invoke: true,
            ..AccessActions::default()
        };
        assert_eq!(
            press_action(&with(toggle, None)),
            Some(AccessAction::Toggle)
        );
        let invoke = AccessActions {
            invoke: true,
            select: true,
            ..AccessActions::default()
        };
        assert_eq!(
            press_action(&with(invoke, None)),
            Some(AccessAction::Invoke)
        );
        let select = AccessActions {
            select: true,
            expand_collapse: true,
            ..AccessActions::default()
        };
        assert_eq!(
            press_action(&with(select, Some(false))),
            Some(AccessAction::Select)
        );
        let expand = AccessActions {
            expand_collapse: true,
            ..AccessActions::default()
        };
        assert_eq!(
            press_action(&with(expand, Some(false))),
            Some(AccessAction::Expand)
        );
        assert_eq!(
            press_action(&with(expand, Some(true))),
            Some(AccessAction::Collapse)
        );
        assert_eq!(press_action(&with(expand, None)), None);
        assert_eq!(press_action(&with(AccessActions::default(), None)), None);
        let mut disabled = with(toggle, None);
        disabled.state.disabled = true;
        assert_eq!(press_action(&disabled), None);
        assert_eq!(expand_action(true), AccessAction::Expand);
        assert_eq!(expand_action(false), AccessAction::Collapse);
    }

    #[test]
    fn selected_text_ranges_count_characters_start_first() {
        assert_eq!(
            selected_text_range(&text_area("héllo", 4, 1)),
            Some(NsRange {
                location: 1,
                length: 3
            })
        );
        assert_eq!(
            selected_text_range(&text_area("héllo", 2, 2)),
            Some(NsRange {
                location: 2,
                length: 0
            })
        );
        assert_eq!(
            selected_text_range(&node(1, None, AccessRole::Button)),
            None
        );
    }

    #[test]
    fn a_requested_text_range_becomes_a_caret_within_the_value() {
        let area = text_area("héllo", 0, 0);
        let caret =
            |location, length| caret_from_selected_text_range(&area, NsRange { location, length });
        assert_eq!(
            caret(1, 2),
            Some(AccessCaret {
                anchor: 1,
                caret: 3
            })
        );
        assert_eq!(
            caret(4, 10),
            Some(AccessCaret {
                anchor: 4,
                caret: 5
            })
        );
        assert_eq!(
            caret(usize::MAX, usize::MAX),
            Some(AccessCaret {
                anchor: 5,
                caret: 5
            })
        );
        let button = node(2, None, AccessRole::Button);
        assert_eq!(
            caret_from_selected_text_range(&button, NsRange::default()),
            None
        );
    }

    #[test]
    fn selectors_are_allowed_only_for_what_the_node_offers() {
        let mut area = text_area("x", 0, 0);
        assert!(ax_selector_allowed(&area, AxSelector::SetValue));
        assert!(ax_selector_allowed(&area, AxSelector::SetFocused));
        assert!(ax_selector_allowed(&area, AxSelector::SetSelectedTextRange));
        assert!(ax_selector_allowed(&area, AxSelector::Value));
        assert!(ax_selector_allowed(&area, AxSelector::SelectedTextRange));
        assert!(!ax_selector_allowed(&area, AxSelector::PerformPress));
        assert!(!ax_selector_allowed(&area, AxSelector::SetExpanded));
        assert!(!ax_selector_allowed(&area, AxSelector::Expanded));
        assert!(!ax_selector_allowed(&area, AxSelector::Help));
        assert!(!ax_selector_allowed(&area, AxSelector::RowAndColumnCount));
        assert!(!ax_selector_allowed(&area, AxSelector::IndexRanges));
        assert!(!ax_selector_allowed(&area, AxSelector::DisclosureLevel));
        assert!(!ax_selector_allowed(&area, AxSelector::GridCells));
        area.description = Some("Query editor".to_owned());
        assert!(ax_selector_allowed(&area, AxSelector::Help));
        area.state.read_only = true;
        assert!(!ax_selector_allowed(&area, AxSelector::SetValue));
        let item = AccessNode {
            actions: AccessActions {
                expand_collapse: true,
                ..AccessActions::default()
            },
            state: AccessState {
                expanded: Some(false),
                ..AccessState::default()
            },
            ..node(3, None, AccessRole::TreeItem)
        };
        assert!(ax_selector_allowed(&item, AxSelector::SetExpanded));
        assert!(ax_selector_allowed(&item, AxSelector::Expanded));
        assert!(!ax_selector_allowed(&item, AxSelector::Value));
        assert!(ax_selector_allowed(&item, AxSelector::PerformPress));
        let leaf = AccessNode {
            state: AccessState::default(),
            ..item.clone()
        };
        assert!(!ax_selector_allowed(&leaf, AxSelector::SetExpanded));
    }

    #[test]
    fn grids_and_trees_report_levels_indexes_cells_and_headers() {
        let item = AccessNode {
            tree_position: Some(AccessTreePosition {
                level: 3,
                position: 1,
                set_size: 2,
            }),
            ..node(1, None, AccessRole::TreeItem)
        };
        assert_eq!(disclosure_level(&item), 2);
        assert_eq!(disclosure_level(&node(2, None, AccessRole::Button)), 0);
        let nodes = [
            node(10, None, AccessRole::Window),
            node(11, Some(10), AccessRole::Grid),
            cell(12, 11, AccessRole::ColumnHeader, 0, 0),
            cell(13, 11, AccessRole::ColumnHeader, 0, 1),
            node(14, Some(11), AccessRole::Group),
            cell(15, 14, AccessRole::Cell, 0, 0),
            cell(16, 14, AccessRole::Cell, 0, 1),
            node(17, Some(10), AccessRole::Grid),
            cell(18, 17, AccessRole::Cell, 0, 1),
        ];
        assert_eq!(
            row_index_range(&nodes[6]),
            NsRange {
                location: 0,
                length: 1
            }
        );
        assert_eq!(
            column_index_range(&nodes[6]),
            NsRange {
                location: 1,
                length: 1
            }
        );
        assert_eq!(row_index_range(&nodes[1]), NsRange::default());
        assert_eq!(parent_index(&nodes, 5), Some(4));
        assert_eq!(parent_index(&nodes, 0), None);
        assert_eq!(nearest_ancestor(&nodes, 5, AccessRole::Grid), Some(1));
        assert_eq!(nearest_ancestor(&nodes, 1, AccessRole::Grid), None);
        assert_eq!(cell_at(&nodes, 1, 1, 0), Some(6));
        assert_eq!(cell_at(&nodes, 7, 1, 0), Some(8));
        assert_eq!(cell_at(&nodes, 1, 2, 0), None);
        assert_eq!(column_headers(&nodes, 1), [2, 3]);
        assert!(column_headers(&nodes, 7).is_empty());
        assert!(ax_selector_allowed(&nodes[1], AxSelector::GridCells));
        assert!(ax_selector_allowed(&nodes[6], AxSelector::IndexRanges));
        assert!(ax_selector_allowed(&item, AxSelector::DisclosureLevel));
        assert_eq!(ns_integer(7), 7);
        assert_eq!(ns_integer(usize::MAX), isize::MAX);
    }

    #[test]
    fn ancestors_of_a_parent_cycle_end() {
        let nodes = [
            node(1, Some(2), AccessRole::Group),
            node(2, Some(1), AccessRole::Group),
        ];
        assert_eq!(nearest_ancestor(&nodes, 0, AccessRole::Grid), None);
    }

    #[test]
    fn changes_post_value_caret_and_expansion_notifications() {
        let old = text_area("select", 6, 6);
        let mut new = old.clone();
        assert!(change_notifications(&old, &new).is_empty());
        new.value = Some("select 1".to_owned());
        new.caret = Some(AccessCaret {
            anchor: 8,
            caret: 8,
        });
        assert_eq!(
            change_notifications(&old, &new),
            [VALUE_CHANGED, SELECTED_TEXT_CHANGED]
        );
        let collapsed = AccessNode {
            state: AccessState {
                expanded: Some(false),
                ..AccessState::default()
            },
            ..node(2, None, AccessRole::TreeItem)
        };
        let mut expanded = collapsed.clone();
        expanded.state.expanded = Some(true);
        assert_eq!(change_notifications(&collapsed, &expanded), [ROW_EXPANDED]);
        assert_eq!(change_notifications(&expanded, &collapsed), [ROW_COLLAPSED]);
        assert_eq!(
            selection_change_notifications(AccessRole::Cell),
            [
                (None, SELECTED_CHILDREN_CHANGED),
                (Some(AccessRole::Grid), SELECTED_CELLS_CHANGED)
            ]
        );
        assert_eq!(
            selection_change_notifications(AccessRole::TreeItem)[1],
            (Some(AccessRole::Tree), SELECTED_ROWS_CHANGED)
        );
        assert!(selection_change_notifications(AccessRole::Button).is_empty());
    }

    #[test]
    fn every_role_but_the_window_has_an_ax_role() {
        assert_eq!(ax_role(AccessRole::Window), None);
        let button = ax_role(AccessRole::Button);
        assert_eq!(
            button,
            Some(AxRole {
                role: "AXButton",
                name_is_value: false
            })
        );
        assert_eq!(
            ax_role(AccessRole::Tab).map(|ax| ax.role),
            Some("AXRadioButton")
        );
        assert_eq!(
            ax_role(AccessRole::TabList).map(|ax| ax.role),
            Some("AXTabGroup")
        );
        assert_eq!(
            ax_role(AccessRole::CheckBox).map(|ax| ax.role),
            Some("AXCheckBox")
        );
        assert_eq!(
            ax_role(AccessRole::TextField).map(|ax| ax.role),
            Some("AXTextField")
        );
        assert!(ax_role(AccessRole::Status).is_some_and(|ax| ax.name_is_value));
        assert!(ax_role(AccessRole::Heading).is_some_and(|ax| ax.name_is_value));
    }

    #[test]
    fn frames_flip_to_a_bottom_left_origin_and_become_relative_to_the_parent() {
        let card = AccessRect {
            x: 360.0,
            y: 140.0,
            width: 560.0,
            height: 480.0,
        };
        let input = AccessRect {
            x: 384.0,
            y: 282.0,
            width: 420.0,
            height: 28.0,
        };
        let in_view = frame_in_parent_space(input, None, 800.0);
        assert_eq!(
            in_view,
            CgRect {
                origin: CgPoint { x: 384.0, y: 490.0 },
                size: CgSize {
                    width: 420.0,
                    height: 28.0
                }
            }
        );
        let in_card = frame_in_parent_space(input, Some(card), 800.0);
        assert_eq!(in_card.origin, CgPoint { x: 24.0, y: 310.0 });
        assert_eq!(in_card.size, in_view.size);
    }

    #[test]
    fn modifier_flags_map_to_modifiers() {
        assert_eq!(modifiers_from_flags(0), Modifiers::default());
        assert_eq!(
            modifiers_from_flags(FLAG_COMMAND | FLAG_SHIFT | 0x100),
            Modifiers {
                shift: true,
                control: false,
                alt: false,
                command: true,
            }
        );
        assert_eq!(
            modifiers_from_flags(FLAG_CONTROL | FLAG_OPTION),
            Modifiers {
                shift: false,
                control: true,
                alt: true,
                command: false,
            }
        );
    }

    #[test]
    fn key_codes_map_to_the_gui_keys() {
        let cases = [
            (KEY_TAB, Key::Tab),
            (KEY_RETURN, Key::Enter),
            (KEY_KEYPAD_ENTER, Key::Enter),
            (KEY_ESCAPE, Key::Escape),
            (KEY_SPACE, Key::Space),
            (KEY_DELETE, Key::Backspace),
            (KEY_FORWARD_DELETE, Key::Delete),
            (KEY_LEFT, Key::Left),
            (KEY_RIGHT, Key::Right),
            (KEY_UP, Key::Up),
            (KEY_DOWN, Key::Down),
            (KEY_HOME, Key::Home),
            (KEY_END, Key::End),
            (KEY_F4, Key::F4),
            (KEY_F2, Key::F2),
            (KEY_F5, Key::F5),
            (KEY_F10, Key::F10),
            (KEY_CONTEXTUAL_MENU, Key::ContextMenu),
            (KEY_PAGE_UP, Key::PageUp),
            (KEY_PAGE_DOWN, Key::PageDown),
        ];
        for (code, key) in cases {
            assert_eq!(key_from_key_code(code), Some(key));
        }
        assert_eq!(key_from_key_code(0x0C), None);
        assert_eq!(key_from_key_code(u16::MAX), None);
    }

    #[test]
    fn shortcuts_read_the_unmodified_characters() {
        let command = modifiers_from_flags(FLAG_COMMAND);
        let control = modifiers_from_flags(FLAG_CONTROL);
        let option = modifiers_from_flags(FLAG_OPTION | FLAG_SHIFT);
        assert!(reads_unmodified_characters(command));
        assert!(reads_unmodified_characters(control));
        assert!(!reads_unmodified_characters(option));
        assert!(!reads_unmodified_characters(Modifiers::default()));
    }

    #[test]
    fn characters_become_keys_without_control_codes_or_function_keys() {
        assert_eq!(keys_from_characters("q"), [Key::Character('q')]);
        assert_eq!(
            keys_from_characters("é😀"),
            [Key::Character('é'), Key::Character('😀')]
        );
        assert!(keys_from_characters("\u{1}").is_empty());
        assert!(keys_from_characters(" ").is_empty());
        assert!(keys_from_characters("\r\t\u{7F}").is_empty());
        assert!(keys_from_characters("\u{F702}").is_empty());
        assert!(keys_from_characters("").is_empty());
    }

    #[test]
    fn backing_factors_become_scales() {
        assert_eq!(scale_from_backing(2.0), 2.0);
        assert_eq!(scale_from_backing(1.0), 1.0);
        assert_eq!(scale_from_backing(0.0), 1.0);
        assert_eq!(scale_from_backing(-2.0), 1.0);
        assert_eq!(scale_from_backing(f64::NAN), 1.0);
        assert_eq!(scale_from_backing(f64::INFINITY), 1.0);
    }

    #[test]
    fn point_sizes_become_device_pixels() {
        let size = CgSize {
            width: 640.0,
            height: 480.5,
        };
        assert_eq!(device_size(size, 1.0), (640, 481));
        assert_eq!(device_size(size, 2.0), (1280, 961));
        let odd = CgSize {
            width: -3.0,
            height: f64::NAN,
        };
        assert_eq!(device_size(odd, 2.0), (0, 0));
        let huge = CgSize {
            width: 1e300,
            height: 1.0,
        };
        assert_eq!(device_size(huge, 1.0), (u32::MAX, 1));
    }

    #[test]
    fn frames_are_placed_at_the_top_left_one_pixel_per_pixel() {
        assert_eq!(
            frame_rect(1280, 960, 2.0, 500.0),
            CgRect {
                origin: CgPoint { x: 0.0, y: 20.0 },
                size: CgSize {
                    width: 640.0,
                    height: 480.0,
                },
            }
        );
        assert_eq!(
            frame_rect(100, 50, 0.0, 50.0),
            CgRect {
                origin: CgPoint { x: 0.0, y: 0.0 },
                size: CgSize {
                    width: 100.0,
                    height: 50.0,
                },
            }
        );
    }

    #[test]
    fn mouse_positions_buttons_and_scrolls_become_pointer_events() {
        assert_eq!(
            pointer_position(CgPoint { x: 100.0, y: 700.0 }, 800.0, 2.0),
            (200.0, 200.0)
        );
        assert_eq!(other_button(2), Some(PointerButton::Middle));
        assert_eq!(other_button(4), None);
        assert_eq!(wheel_lines(0.0, 3.0, false), (-3.0, 0.0));
        assert_eq!(wheel_lines(-40.0, -20.0, true), (1.0, 2.0));
        assert_eq!(wheel_lines(f64::NAN, f64::INFINITY, false), (0.0, 0.0));
    }
}
