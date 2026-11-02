use std::ffi::CStr;

use super::super::cocoa::{self, CgRect};
use super::super::{AccessNode, AccessRole, focused_node, same_access_structure};
use super::{Id, NO, ObjcBool, Sel, WindowState, YES, state_for};

const FOCUS_CHANGED: &str = "AXFocusedUIElementChanged";
const VALUE_CHANGED: &str = "AXValueChanged";

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

impl WindowState {
    pub(super) fn update_accessibility(&self, nodes: &[AccessNode]) {
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
        if focused_node(nodes) != old_focus
            && let Some(element) = access.focused_element()
        {
            self.post(element, FOCUS_CHANGED);
        }
        if !rebuilt {
            for ((old, new), element) in old_nodes.iter().zip(nodes).zip(&access.elements) {
                let changed = old.state.checked != new.state.checked || old.value != new.value;
                if changed && !element.is_null() {
                    self.post(*element, VALUE_CHANGED);
                }
            }
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
        let Ok(class) = runtime.class(c"NSAccessibilityElement") else {
            return std::ptr::null_mut();
        };
        // SAFETY: `alloc` on a class returns a new, uninitialized instance.
        let element = unsafe { (runtime.send_id)(class, runtime.selector(c"alloc")) };
        if element.is_null() {
            return element;
        }
        // SAFETY: `element` is a fresh `NSAccessibilityElement`; `init` returns it initialized
        // (owned by us, released in `release_elements`) or nil.
        let element = unsafe { (runtime.send_id)(element, runtime.selector(c"init")) };
        if element.is_null() {
            return element;
        }
        self.send_flag_to(element, c"setAccessibilityElement:", YES);
        self.set_string(element, c"setAccessibilityRole:", ax.role);
        element
    }

    fn set_string(&self, element: Id, selector: &CStr, text: &str) {
        let Ok(string) = self.runtime.string(text) else {
            return;
        };
        self.send_object_to(element, selector, string);
        self.send_to(string, c"release");
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
            if ax.name_is_value {
                self.set_string(element, c"setAccessibilityValue:", &node.name);
            } else {
                self.set_string(element, c"setAccessibilityLabel:", &node.name);
                self.describe_value(element, node);
            }
            if let Some(placeholder) = &node.placeholder {
                self.set_string(element, c"setAccessibilityPlaceholderValue:", placeholder);
            }
            self.send_flag_to(
                element,
                c"setAccessibilityEnabled:",
                flag(!node.state.disabled),
            );
            self.send_flag_to(
                element,
                c"setAccessibilityFocused:",
                flag(node.state.focused),
            );
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

    fn describe_value(&self, element: Id, node: &AccessNode) {
        if let Some(value) = &node.value {
            self.set_string(element, c"setAccessibilityValue:", value);
        } else if let Some(checked) = node.state.checked {
            self.send_object_to(element, c"setAccessibilityValue:", self.number(checked));
        } else if node.role == AccessRole::Tab {
            let selected = self.number(node.state.selected);
            self.send_object_to(element, c"setAccessibilityValue:", selected);
        }
    }

    fn set_frame(&self, element: Id, frame: CgRect) {
        let runtime = &self.runtime;
        // SAFETY: `element` is a live `NSAccessibilityElement`;
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
