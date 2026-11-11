use std::collections::HashMap;
use std::ffi::{CStr, c_void};
use std::rc::Rc;

use super::super::cocoa::{self, AxSelector, AxValue, CgRect, NsRange};
use super::super::{
    AccessAction, AccessNode, AccessRole, MAX_ACCESS_VALUE_CHARS, WindowEvent, access_request,
    focused_node, same_access_structure,
};
use super::{
    BOOL_GETTER_TYPES, BOOL_SELECTOR_TYPES, BOOL_SETTER_TYPES, Id, NO, OBJECT_GETTER_TYPES,
    ObjcBool, Runtime, Sel, VIEWS, VOID_OBJECT_TYPES, WindowError, WindowState, YES, state_for,
};

const ELEMENT_CLASS: &CStr = c"EggDBAccessElement";
const RANGE_GETTER_TYPES: &CStr = c"{_NSRange=QQ}@:";
const RANGE_SETTER_TYPES: &CStr = c"v@:{_NSRange=QQ}";
const INTEGER_GETTER_TYPES: &CStr = c"q@:";
const CELL_FOR_COLUMN_AND_ROW_TYPES: &CStr = c"@@:qq";

const SELECTORS_ONLY_SOME_NODES_ANSWER: [(&CStr, AxSelector); 18] = [
    (c"accessibilityPerformPress", AxSelector::PerformPress),
    (c"setAccessibilityFocused:", AxSelector::SetFocused),
    (c"accessibilityValue", AxSelector::Value),
    (c"setAccessibilityValue:", AxSelector::SetValue),
    (
        c"accessibilitySelectedTextRange",
        AxSelector::SelectedTextRange,
    ),
    (
        c"setAccessibilitySelectedTextRange:",
        AxSelector::SetSelectedTextRange,
    ),
    (c"isAccessibilityExpanded", AxSelector::Expanded),
    (c"isAccessibilityDisclosed", AxSelector::Expanded),
    (c"setAccessibilityExpanded:", AxSelector::SetExpanded),
    (c"setAccessibilityDisclosed:", AxSelector::SetExpanded),
    (c"accessibilityHelp", AxSelector::Help),
    (c"accessibilityRowCount", AxSelector::RowAndColumnCount),
    (c"accessibilityColumnCount", AxSelector::RowAndColumnCount),
    (c"accessibilityRowIndexRange", AxSelector::IndexRanges),
    (c"accessibilityColumnIndexRange", AxSelector::IndexRanges),
    (c"accessibilityDisclosureLevel", AxSelector::DisclosureLevel),
    (c"accessibilityCellForColumn:row:", AxSelector::GridCells),
    (
        c"accessibilityColumnHeaderUIElements",
        AxSelector::GridCells,
    ),
];

#[derive(Debug)]
pub(super) struct AccessElements {
    nodes: Vec<AccessNode>,
    elements: Vec<Id>,
    top_level: Id,
}

impl Default for AccessElements {
    fn default() -> Self {
        Self {
            nodes: Vec::new(),
            elements: Vec::new(),
            top_level: std::ptr::null_mut(),
        }
    }
}

impl AccessElements {
    fn element(&self, id: u32) -> Option<Id> {
        let at = self.nodes.iter().position(|node| node.id == id)?;
        self.element_at(at)
    }

    fn element_at(&self, at: usize) -> Option<Id> {
        self.elements
            .get(at)
            .copied()
            .filter(|element| !element.is_null())
    }

    fn focused_element(&self) -> Option<Id> {
        self.element(focused_node(&self.nodes)?)
    }
}

pub(super) extern "C" fn accessibility_children(view: Id, _selector: Sel) -> Id {
    state_for(view)
        .and_then(|state| {
            state
                .access
                .try_borrow()
                .ok()
                .map(|access| access.top_level)
        })
        .unwrap_or(std::ptr::null_mut())
}

pub(super) extern "C" fn accessibility_focused_element(view: Id, _selector: Sel) -> Id {
    state_for(view)
        .and_then(|state| {
            state
                .access
                .try_borrow()
                .ok()
                .and_then(|access| access.focused_element())
        })
        .unwrap_or(view)
}

fn flag(value: bool) -> ObjcBool {
    if value { YES } else { NO }
}

fn open_windows() -> Vec<Rc<WindowState>> {
    VIEWS
        .try_with(|views| {
            views
                .try_borrow()
                .map(|views| {
                    views
                        .iter()
                        .filter_map(|(_, state)| state.upgrade())
                        .collect()
                })
                .unwrap_or_default()
        })
        .unwrap_or_default()
}

fn with_node<T>(
    element: Id,
    answer: impl FnOnce(&WindowState, &AccessElements, usize) -> T,
) -> Option<T> {
    for state in open_windows() {
        let Ok(access) = state.access.try_borrow() else {
            continue;
        };
        let Some(at) = access.elements.iter().position(|&known| known == element) else {
            continue;
        };
        if at >= access.nodes.len() {
            return None;
        }
        return Some(answer(&state, &access, at));
    }
    None
}

fn read_node<T>(element: Id, read: impl FnOnce(&AccessNode) -> T) -> Option<T> {
    with_node(element, |_, access, at| read(&access.nodes[at]))
}

fn request(
    element: Id,
    action: impl FnOnce(&WindowState, &AccessNode) -> Option<AccessAction>,
) -> bool {
    with_node(element, |state, access, at| {
        let node = &access.nodes[at];
        let Some(request) =
            action(state, node).and_then(|action| access_request(&access.nodes, node.id, action))
        else {
            return false;
        };
        state.push(WindowEvent::AccessRequested(request));
        true
    })
    .unwrap_or(false)
}

extern "C" fn perform_press(element: Id, _selector: Sel) -> ObjcBool {
    flag(request(element, |_, node| cocoa::press_action(node)))
}

extern "C" fn is_focused(element: Id, _selector: Sel) -> ObjcBool {
    flag(read_node(element, |node| node.state.focused).unwrap_or(false))
}

extern "C" fn set_focused(element: Id, _selector: Sel, focused: ObjcBool) {
    if focused != NO {
        request(element, |_, _| Some(AccessAction::Focus));
    }
}

extern "C" fn is_enabled(element: Id, _selector: Sel) -> ObjcBool {
    flag(read_node(element, |node| !node.state.disabled).unwrap_or(true))
}

extern "C" fn value(element: Id, _selector: Sel) -> Id {
    with_node(element, |state, access, at| {
        match cocoa::ax_value(&access.nodes[at]) {
            Some(AxValue::Text(text)) => state.autoreleased_string(text),
            Some(AxValue::Flag(value)) => state.number(value),
            None => std::ptr::null_mut(),
        }
    })
    .unwrap_or(std::ptr::null_mut())
}

extern "C" fn set_value(element: Id, _selector: Sel, value: Id) {
    request(element, |state, _| {
        state.text_argument(value).map(AccessAction::SetValue)
    });
}

extern "C" fn selected_text_range(element: Id, _selector: Sel) -> NsRange {
    read_node(element, cocoa::selected_text_range)
        .flatten()
        .unwrap_or_default()
}

extern "C" fn set_selected_text_range(element: Id, _selector: Sel, range: NsRange) {
    request(element, |_, node| {
        cocoa::caret_from_selected_text_range(node, range).map(AccessAction::SetCaret)
    });
}

extern "C" fn is_expanded(element: Id, _selector: Sel) -> ObjcBool {
    flag(read_node(element, |node| node.state.expanded == Some(true)).unwrap_or(false))
}

extern "C" fn set_expanded(element: Id, _selector: Sel, expanded: ObjcBool) {
    request(element, |_, _| Some(cocoa::expand_action(expanded != NO)));
}

extern "C" fn is_selector_allowed(element: Id, _selector: Sel, asked: Sel) -> ObjcBool {
    let allowed = with_node(element, |state, access, at| {
        SELECTORS_ONLY_SOME_NODES_ANSWER
            .iter()
            .find(|(name, _)| state.runtime.selector(name) == asked)
            .is_none_or(|&(_, selector)| cocoa::ax_selector_allowed(&access.nodes[at], selector))
    });
    flag(allowed.unwrap_or(true))
}

extern "C" fn help(element: Id, _selector: Sel) -> Id {
    with_node(element, |state, access, at| {
        access.nodes[at]
            .description
            .as_deref()
            .map_or(std::ptr::null_mut(), |text| state.autoreleased_string(text))
    })
    .unwrap_or(std::ptr::null_mut())
}

extern "C" fn row_count(element: Id, _selector: Sel) -> isize {
    read_node(element, |node| node.table.map_or(0, |table| table.rows)).map_or(0, cocoa::ns_integer)
}

extern "C" fn column_count(element: Id, _selector: Sel) -> isize {
    read_node(element, |node| node.table.map_or(0, |table| table.columns))
        .map_or(0, cocoa::ns_integer)
}

extern "C" fn row_index_range(element: Id, _selector: Sel) -> NsRange {
    read_node(element, cocoa::row_index_range).unwrap_or_default()
}

extern "C" fn column_index_range(element: Id, _selector: Sel) -> NsRange {
    read_node(element, cocoa::column_index_range).unwrap_or_default()
}

extern "C" fn disclosure_level(element: Id, _selector: Sel) -> isize {
    read_node(element, cocoa::disclosure_level).map_or(0, cocoa::ns_integer)
}

extern "C" fn cell_for_column_and_row(
    element: Id,
    _selector: Sel,
    column: isize,
    row: isize,
) -> Id {
    let (Ok(column), Ok(row)) = (usize::try_from(column), usize::try_from(row)) else {
        return std::ptr::null_mut();
    };
    with_node(element, |_, access, at| {
        cocoa::cell_at(&access.nodes, at, column, row).and_then(|cell| access.element_at(cell))
    })
    .flatten()
    .unwrap_or(std::ptr::null_mut())
}

extern "C" fn column_headers(element: Id, _selector: Sel) -> Id {
    with_node(element, |state, access, at| {
        let headers: Vec<Id> = cocoa::column_headers(&access.nodes, at)
            .into_iter()
            .filter_map(|header| access.element_at(header))
            .collect();
        state.array(&headers)
    })
    .unwrap_or(std::ptr::null_mut())
}

impl Runtime {
    pub(super) fn element_class(&self) -> Result<Id, WindowError> {
        let press: extern "C" fn(Id, Sel) -> ObjcBool = perform_press;
        let focused: extern "C" fn(Id, Sel) -> ObjcBool = is_focused;
        let focus: extern "C" fn(Id, Sel, ObjcBool) = set_focused;
        let enabled: extern "C" fn(Id, Sel) -> ObjcBool = is_enabled;
        let get_value: extern "C" fn(Id, Sel) -> Id = value;
        let put_value: extern "C" fn(Id, Sel, Id) = set_value;
        let text_range: extern "C" fn(Id, Sel) -> NsRange = selected_text_range;
        let put_text_range: extern "C" fn(Id, Sel, NsRange) = set_selected_text_range;
        let expanded: extern "C" fn(Id, Sel) -> ObjcBool = is_expanded;
        let expand: extern "C" fn(Id, Sel, ObjcBool) = set_expanded;
        let allowed: extern "C" fn(Id, Sel, Sel) -> ObjcBool = is_selector_allowed;
        let get_help: extern "C" fn(Id, Sel) -> Id = help;
        let rows: extern "C" fn(Id, Sel) -> isize = row_count;
        let columns: extern "C" fn(Id, Sel) -> isize = column_count;
        let row_range: extern "C" fn(Id, Sel) -> NsRange = row_index_range;
        let column_range: extern "C" fn(Id, Sel) -> NsRange = column_index_range;
        let level: extern "C" fn(Id, Sel) -> isize = disclosure_level;
        let cell: extern "C" fn(Id, Sel, isize, isize) -> Id = cell_for_column_and_row;
        let headers: extern "C" fn(Id, Sel) -> Id = column_headers;
        let methods: [(&CStr, *const c_void, &CStr); 21] = [
            (
                c"accessibilityPerformPress",
                press as *const c_void,
                BOOL_GETTER_TYPES,
            ),
            (
                c"isAccessibilityFocused",
                focused as *const c_void,
                BOOL_GETTER_TYPES,
            ),
            (
                c"setAccessibilityFocused:",
                focus as *const c_void,
                BOOL_SETTER_TYPES,
            ),
            (
                c"isAccessibilityEnabled",
                enabled as *const c_void,
                BOOL_GETTER_TYPES,
            ),
            (
                c"accessibilityValue",
                get_value as *const c_void,
                OBJECT_GETTER_TYPES,
            ),
            (
                c"setAccessibilityValue:",
                put_value as *const c_void,
                VOID_OBJECT_TYPES,
            ),
            (
                c"accessibilitySelectedTextRange",
                text_range as *const c_void,
                RANGE_GETTER_TYPES,
            ),
            (
                c"setAccessibilitySelectedTextRange:",
                put_text_range as *const c_void,
                RANGE_SETTER_TYPES,
            ),
            (
                c"isAccessibilityExpanded",
                expanded as *const c_void,
                BOOL_GETTER_TYPES,
            ),
            (
                c"setAccessibilityExpanded:",
                expand as *const c_void,
                BOOL_SETTER_TYPES,
            ),
            (
                c"isAccessibilityDisclosed",
                expanded as *const c_void,
                BOOL_GETTER_TYPES,
            ),
            (
                c"setAccessibilityDisclosed:",
                expand as *const c_void,
                BOOL_SETTER_TYPES,
            ),
            (
                c"isAccessibilitySelectorAllowed:",
                allowed as *const c_void,
                BOOL_SELECTOR_TYPES,
            ),
            (
                c"accessibilityHelp",
                get_help as *const c_void,
                OBJECT_GETTER_TYPES,
            ),
            (
                c"accessibilityRowCount",
                rows as *const c_void,
                INTEGER_GETTER_TYPES,
            ),
            (
                c"accessibilityColumnCount",
                columns as *const c_void,
                INTEGER_GETTER_TYPES,
            ),
            (
                c"accessibilityRowIndexRange",
                row_range as *const c_void,
                RANGE_GETTER_TYPES,
            ),
            (
                c"accessibilityColumnIndexRange",
                column_range as *const c_void,
                RANGE_GETTER_TYPES,
            ),
            (
                c"accessibilityDisclosureLevel",
                level as *const c_void,
                INTEGER_GETTER_TYPES,
            ),
            (
                c"accessibilityCellForColumn:row:",
                cell as *const c_void,
                CELL_FOR_COLUMN_AND_ROW_TYPES,
            ),
            (
                c"accessibilityColumnHeaderUIElements",
                headers as *const c_void,
                OBJECT_GETTER_TYPES,
            ),
        ];
        self.register_class(ELEMENT_CLASS, c"NSAccessibilityElement", &methods)
    }
}

impl WindowState {
    pub(super) fn update_accessibility(&self, nodes: &[AccessNode]) {
        let posts = {
            let Ok(mut access) = self.access.try_borrow_mut() else {
                return;
            };
            let rebuilt = !same_access_structure(&access.nodes, nodes);
            let old_focus = focused_node(&access.nodes);
            let old_nodes = std::mem::take(&mut access.nodes);
            if rebuilt {
                self.release_elements(&mut access);
                access.elements = nodes.iter().map(|node| self.create_element(node)).collect();
            }
            access.nodes = nodes.to_vec();
            self.describe_elements(&access);
            if rebuilt {
                self.link_elements(&mut access);
            }
            notifications(&access, &old_nodes, old_focus)
        };
        for (element, name) in posts {
            self.post(element, name);
        }
    }

    pub(super) fn release_accessibility(&self) {
        if let Ok(mut access) = self.access.try_borrow_mut() {
            self.release_elements(&mut access);
            access.nodes.clear();
        }
    }

    fn release_elements(&self, access: &mut AccessElements) {
        for element in access.elements.drain(..) {
            self.send_to(element, c"release");
        }
        let top_level = std::mem::replace(&mut access.top_level, std::ptr::null_mut());
        self.send_to(top_level, c"release");
    }

    fn create_element(&self, node: &AccessNode) -> Id {
        let Some(ax) = cocoa::ax_role(node.role) else {
            return std::ptr::null_mut();
        };
        let runtime = &self.runtime;
        let Ok(class) = runtime.element_class() else {
            return std::ptr::null_mut();
        };
        // SAFETY: `alloc` on a class returns a new, uninitialized instance.
        let element = unsafe { (runtime.send_id)(class, runtime.selector(c"alloc")) };
        if element.is_null() {
            return element;
        }
        // SAFETY: `element` is a fresh `EggDBAccessElement`; `init` returns it initialized
        // (owned by us, released in `release_elements`) or nil.
        let element = unsafe { (runtime.send_id)(element, runtime.selector(c"init")) };
        if element.is_null() {
            return element;
        }
        self.send_flag_to(element, c"setAccessibilityElement:", YES);
        self.set_string(element, c"setAccessibilityRole:", ax.role);
        if node.state.password {
            self.set_string(
                element,
                c"setAccessibilitySubrole:",
                cocoa::SECURE_TEXT_FIELD_SUBROLE,
            );
        } else if node.role == AccessRole::SearchBox {
            self.set_string(
                element,
                c"setAccessibilitySubrole:",
                cocoa::SEARCH_FIELD_SUBROLE,
            );
        }
        element
    }

    fn set_string(&self, element: Id, selector: &CStr, text: &str) {
        let Ok(string) = self.runtime.string(text) else {
            return;
        };
        self.send_object_to(element, selector, string);
        self.send_to(string, c"release");
    }

    fn autoreleased_string(&self, text: &str) -> Id {
        let runtime = &self.runtime;
        let Ok(string) = runtime.string(text) else {
            return std::ptr::null_mut();
        };
        // SAFETY: `string` is the `NSString` created above, owned by us; `autorelease` hands
        // it to the current autorelease pool and returns it.
        unsafe { (runtime.send_id)(string, runtime.selector(c"autorelease")) }
    }

    fn text_argument(&self, value: Id) -> Option<String> {
        if value.is_null() {
            return None;
        }
        let runtime = &self.runtime;
        let class = runtime.class(c"NSString").ok()?;
        // SAFETY: `value` is the object AppKit passed; `isKindOfClass:` takes a class and
        // returns a `BOOL`.
        let is_string =
            unsafe { (runtime.send_bool_id)(value, runtime.selector(c"isKindOfClass:"), class) };
        if is_string == NO {
            return None;
        }
        // SAFETY: `value` is an `NSString`; `length` returns its UTF-16 length.
        let units = unsafe { (runtime.send_u64)(value, runtime.selector(c"length")) };
        let most_units = u64::try_from(MAX_ACCESS_VALUE_CHARS)
            .unwrap_or(u64::MAX)
            .saturating_mul(2);
        (units <= most_units).then(|| runtime.text(value))
    }

    fn number(&self, value: bool) -> Id {
        let runtime = &self.runtime;
        let Ok(class) = runtime.class(c"NSNumber") else {
            return std::ptr::null_mut();
        };
        // SAFETY: `numberWithInteger:` is a class method taking an `NSInteger` and returning an
        // autoreleased `NSNumber`.
        unsafe {
            (runtime.send_id_integer)(
                class,
                runtime.selector(c"numberWithInteger:"),
                isize::from(value),
            )
        }
    }

    fn describe_elements(&self, access: &AccessElements) {
        let view_height = self.bounds().size.height;
        for (node, &element) in access.nodes.iter().zip(&access.elements) {
            let Some(ax) = cocoa::ax_role(node.role).filter(|_| !element.is_null()) else {
                continue;
            };
            if !ax.name_is_value {
                self.set_string(element, c"setAccessibilityLabel:", &node.name);
            }
            if let Some(placeholder) = &node.placeholder {
                self.set_string(element, c"setAccessibilityPlaceholderValue:", placeholder);
            }
            self.send_flag_to(
                element,
                c"setAccessibilitySelected:",
                flag(node.state.selected),
            );
            let parent_bounds = node
                .parent
                .and_then(|parent| access.nodes.iter().find(|other| other.id == parent))
                .filter(|parent| parent.role != AccessRole::Window)
                .map(|parent| parent.bounds);
            let frame = cocoa::frame_in_parent_space(node.bounds, parent_bounds, view_height);
            self.set_frame(element, frame);
        }
    }

    fn set_frame(&self, element: Id, frame: CgRect) {
        let runtime = &self.runtime;
        // SAFETY: `element` is a live `EggDBAccessElement`;
        // `setAccessibilityFrameInParentSpace:` takes an `NSRect`.
        unsafe {
            (runtime.send_void_rect)(
                element,
                runtime.selector(c"setAccessibilityFrameInParentSpace:"),
                frame,
            )
        };
    }

    fn array(&self, objects: &[Id]) -> Id {
        let runtime = &self.runtime;
        let Ok(class) = runtime.class(c"NSArray") else {
            return std::ptr::null_mut();
        };
        // SAFETY: `objects` holds `objects.len()` live, non-nil objects, which the class
        // method copies into a new autoreleased `NSArray`.
        unsafe {
            (runtime.send_id_objects)(
                class,
                runtime.selector(c"arrayWithObjects:count:"),
                objects.as_ptr(),
                objects.len(),
            )
        }
    }

    fn link_elements(&self, access: &mut AccessElements) {
        let view = self.view.get();
        let parent_element = |node: &AccessNode| node.parent.and_then(|id| access.element(id));
        let mut top_level = Vec::new();
        for (node, &element) in access.nodes.iter().zip(&access.elements) {
            if element.is_null() {
                continue;
            }
            match parent_element(node) {
                Some(parent) => self.send_object_to(element, c"setAccessibilityParent:", parent),
                None => {
                    self.send_object_to(element, c"setAccessibilityParent:", view);
                    top_level.push(element);
                }
            }
            let children: Vec<Id> = access
                .nodes
                .iter()
                .zip(&access.elements)
                .filter(|(child, child_element)| {
                    child.parent == Some(node.id) && !child_element.is_null()
                })
                .map(|(_, &child_element)| child_element)
                .collect();
            if !children.is_empty() {
                let children = self.array(&children);
                self.send_object_to(element, c"setAccessibilityChildren:", children);
            }
        }
        let array = self.array(&top_level);
        self.send_to(array, c"retain");
        access.top_level = array;
    }

    fn post(&self, element: Id, notification: &str) {
        let Ok(name) = self.runtime.string(notification) else {
            return;
        };
        // SAFETY: `element` is a live accessibility element of this window and `name` a live
        // `NSString` naming an NSAccessibility notification.
        unsafe { (self.runtime.post_notification)(element, name) };
        self.send_to(name, c"release");
    }
}

fn notifications(
    access: &AccessElements,
    old_nodes: &[AccessNode],
    old_focus: Option<u32>,
) -> Vec<(Id, &'static str)> {
    let mut posts = Vec::new();
    let mut add = |element: Id, name: &'static str| {
        if !posts.contains(&(element, name)) {
            posts.push((element, name));
        }
    };
    if focused_node(&access.nodes) != old_focus
        && let Some(element) = access.focused_element()
    {
        add(element, cocoa::FOCUSED_ELEMENT_CHANGED);
    }
    let old_by_id: HashMap<u32, &AccessNode> =
        old_nodes.iter().map(|node| (node.id, node)).collect();
    for (at, new) in access.nodes.iter().enumerate() {
        let Some(old) = old_by_id.get(&new.id).filter(|old| old.role == new.role) else {
            continue;
        };
        if let Some(element) = access.element_at(at) {
            for name in cocoa::change_notifications(old, new) {
                add(element, name);
            }
        }
        if old.state.selected == new.state.selected {
            continue;
        }
        for &(container, name) in cocoa::selection_change_notifications(new.role) {
            let target = match container {
                None => cocoa::parent_index(&access.nodes, at),
                Some(role) => cocoa::nearest_ancestor(&access.nodes, at, role),
            };
            if let Some(element) = target.and_then(|target| access.element_at(target)) {
                add(element, name);
            }
        }
    }
    posts
}
