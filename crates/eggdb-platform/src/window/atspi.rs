//! The pure parts of the AT-SPI2 bridge on Linux and the BSDs, compiled and tested on every
//! operating system: the object paths and references the bridge answers with, each
//! [`AccessRole`]'s AT-SPI role and each node's state set, the answers to the
//! `org.a11y.atspi.Accessible`, `Component` and `Application` methods and the D-Bus
//! `Properties` calls a screen reader such as Orca makes, and the `Object:StateChanged`,
//! `Focus`, `ChildrenChanged` and `Window` events raised when the tree or the window's focus
//! changes. The values come from the public AT-SPI2 D-Bus interface descriptions; the socket
//! that carries the messages is the caller's (DNEW-I29).
//!
//! The application is the root object; the window's own node becomes its frame, because no
//! other part of the system exposes an X11 window to AT-SPI2.

use super::uia::is_keyboard_focusable;
use super::{AccessNode, AccessRect, AccessRole, focused_node};
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

/// `ATSPI_ROLE_CHECK_BOX`.
pub const ROLE_CHECK_BOX: u32 = 7;
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
/// `ATSPI_ROLE_APPLICATION`.
pub const ROLE_APPLICATION: u32 = 75;
/// `ATSPI_ROLE_ENTRY`.
pub const ROLE_ENTRY: u32 = 79;
/// `ATSPI_ROLE_HEADING`.
pub const ROLE_HEADING: u32 = 83;
/// `ATSPI_ROLE_LINK`.
pub const ROLE_LINK: u32 = 88;

/// `ATSPI_STATE_ACTIVE`.
pub const STATE_ACTIVE: u32 = 1;
/// `ATSPI_STATE_CHECKED`.
pub const STATE_CHECKED: u32 = 4;
/// `ATSPI_STATE_EDITABLE`.
pub const STATE_EDITABLE: u32 = 7;
/// `ATSPI_STATE_ENABLED`.
pub const STATE_ENABLED: u32 = 8;
/// `ATSPI_STATE_FOCUSABLE`.
pub const STATE_FOCUSABLE: u32 = 11;
/// `ATSPI_STATE_FOCUSED`.
pub const STATE_FOCUSED: u32 = 12;
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
/// `ATSPI_STATE_IS_DEFAULT`.
pub const STATE_IS_DEFAULT: u32 = 39;
/// `ATSPI_STATE_CHECKABLE`.
pub const STATE_CHECKABLE: u32 = 41;

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
    }
}

/// The AT-SPI states of a node, in ascending order; `window_active` adds `ACTIVE` to the
/// window's own node.
pub fn states(node: &AccessNode, window_active: bool) -> Vec<u32> {
    let mut states = vec![STATE_VISIBLE, STATE_SHOWING];
    if !node.state.disabled {
        states.extend([STATE_ENABLED, STATE_SENSITIVE]);
    }
    if is_keyboard_focusable(node) {
        states.push(STATE_FOCUSABLE);
    }
    if node.state.focused {
        states.push(STATE_FOCUSED);
    }
    match node.role {
        AccessRole::Window if window_active => states.push(STATE_ACTIVE),
        AccessRole::CheckBox => {
            states.push(STATE_CHECKABLE);
            if node.state.checked == Some(true) {
                states.push(STATE_CHECKED);
            }
        }
        AccessRole::Tab => {
            states.push(STATE_SELECTABLE);
            if node.state.selected {
                states.push(STATE_SELECTED);
            }
        }
        AccessRole::TextField => states.extend([STATE_EDITABLE, STATE_SINGLE_LINE]),
        _ => {}
    }
    if node.state.default_button {
        states.push(STATE_IS_DEFAULT);
    }
    states.sort_unstable();
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

fn event(
    path: &str,
    interface: &str,
    member: &str,
    detail: &str,
    detail1: i32,
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
            Value::Int32(0),
            Value::Variant(Box::new(any)),
            Value::Array("{sv}".to_owned(), Vec::new()),
        ],
    )
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
        }
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
    /// names, checked and selected states, and the focus moving.
    pub fn set_tree(&mut self, nodes: &[AccessNode]) -> Vec<Message> {
        let old = std::mem::replace(&mut self.nodes, nodes.to_vec());
        let mut events = Vec::new();
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
            let object = Object::Node(node.id);
            if before.name != node.name {
                events.push(event(
                    &object.path(),
                    OBJECT_EVENTS,
                    "PropertyChange",
                    "accessible-name",
                    0,
                    Value::String(node.name.clone()),
                ));
            }
            if before.state.checked != node.state.checked {
                events.push(state_changed(
                    object,
                    "checked",
                    node.state.checked == Some(true),
                ));
            }
            if before.state.selected != node.state.selected {
                events.push(state_changed(object, "selected", node.state.selected));
            }
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

    fn interfaces(object: Object) -> &'static [&'static str] {
        match object {
            Object::Application => &[ACCESSIBLE_INTERFACE, APPLICATION_INTERFACE],
            Object::Node(_) => &[ACCESSIBLE_INTERFACE, COMPONENT_INTERFACE],
        }
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
                return Ok(vec![Value::String(introspection(object))]);
            }
            Some(PEER_INTERFACE) if member == "Ping" => return Ok(Vec::new()),
            _ => {}
        }
        let object =
            object.ok_or_else(|| CallError::new(UNKNOWN_OBJECT, format!("no object at {path}")))?;
        match call.interface.as_deref() {
            Some(PROPERTIES_INTERFACE) => self.properties_call(object, member, &call.body),
            Some(interface) if Self::interfaces(object).contains(&interface) => {
                self.interface_call(object, interface, member, &call.body)
            }
            Some(interface) => Err(CallError::new(
                UNKNOWN_INTERFACE,
                format!("{path} has no interface {interface}"),
            )),
            None => {
                for interface in Self::interfaces(object) {
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
            "GetAttributes" => Value::Array(
                "{ss}".to_owned(),
                attributes(node)
                    .into_iter()
                    .map(|(key, value)| {
                        Value::DictEntry(
                            Box::new(Value::String(key.to_owned())),
                            Box::new(Value::String(value.to_owned())),
                        )
                    })
                    .collect(),
            ),
            "GetApplication" => self.reference(Object::Application),
            "GetInterfaces" => Value::Array(
                "s".to_owned(),
                Self::interfaces(object)
                    .iter()
                    .map(|name| Value::String((*name).to_owned()))
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
        &self,
        id: u32,
        member: &str,
        body: &[Value],
    ) -> Result<Vec<Value>, CallError> {
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
            "GrabFocus" => vec![Value::Boolean(false)],
            "GetAlpha" => vec![Value::Double(1.0)],
            _ => return Err(CallError::unknown_method(COMPONENT_INTERFACE, member)),
        })
    }

    fn properties(&self, object: Object, interface: &str) -> Option<Vec<(&'static str, Value)>> {
        let node = match object {
            Object::Application => None,
            Object::Node(id) => self.node(id),
        };
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
                        text(
                            node.and_then(|node| node.placeholder.as_deref())
                                .unwrap_or_default(),
                        ),
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
            (COMPONENT_INTERFACE, Object::Node(_)) => Some(Vec::new()),
            _ => None,
        }
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

fn attributes(node: Option<&AccessNode>) -> Vec<(&'static str, &'static str)> {
    let mut attributes = vec![("toolkit", TOOLKIT_NAME)];
    match node.map(|node| node.role) {
        Some(AccessRole::Heading) => attributes.push(("level", "1")),
        Some(AccessRole::Status) => {
            attributes.extend([("live", "polite"), ("container-live", "polite")]);
        }
        _ => {}
    }
    attributes
}

fn introspection(object: Option<Object>) -> String {
    let interfaces = object.map_or(&[][..], AtspiApplication::interfaces);
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

#[cfg(test)]
mod tests {
    use super::super::AccessState;
    use super::*;

    const UNIQUE: &str = ":1.42";

    fn node(id: u32, parent: Option<u32>, role: AccessRole, name: &str) -> AccessNode {
        AccessNode {
            id,
            parent,
            role,
            name: name.to_owned(),
            value: None,
            placeholder: None,
            state: AccessState::default(),
            bounds: AccessRect {
                x: 10.0 * id as f32,
                y: 20.0,
                width: 100.0,
                height: 30.0,
            },
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
}
