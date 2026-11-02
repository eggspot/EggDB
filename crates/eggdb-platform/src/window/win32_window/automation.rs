use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use super::super::uia::{self, Direction, Element, UiaRect};
use super::super::{AccessNode, AccessRole, focused_node};
use super::{Handle, Point};

type HResult = i32;
type Out = *mut *mut c_void;

const S_OK: HResult = 0;
const E_NOINTERFACE: HResult = 0x8000_4002_u32 as i32;
const E_POINTER: HResult = 0x8000_4003_u32 as i32;
const E_INVALIDARG: HResult = 0x8007_0057_u32 as i32;
const E_OUTOFMEMORY: HResult = 0x8007_000E_u32 as i32;
const UIA_E_ELEMENTNOTAVAILABLE: HResult = 0x8004_0201_u32 as i32;

const VT_EMPTY: u16 = 0;
const VT_I4: u16 = 3;
const VT_BSTR: u16 = 8;
const VT_BOOL: u16 = 11;
const VARIANT_TRUE: usize = 0xFFFF;
const VARIANT_FALSE: usize = 0;

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
struct Guid {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

const IID_IUNKNOWN: Guid = Guid {
    data1: 0x0000_0000,
    data2: 0x0000,
    data3: 0x0000,
    data4: [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
};
const IID_SIMPLE: Guid = Guid {
    data1: 0xd6dd_68d1,
    data2: 0x86fd,
    data3: 0x4332,
    data4: [0x86, 0x66, 0x9a, 0xbe, 0xde, 0xa2, 0xd2, 0x4c],
};
const IID_FRAGMENT: Guid = Guid {
    data1: 0xf706_3da8,
    data2: 0x8359,
    data3: 0x439c,
    data4: [0x92, 0x97, 0xbb, 0xc5, 0x29, 0x9a, 0x7d, 0x87],
};
const IID_FRAGMENT_ROOT: Guid = Guid {
    data1: 0x620c_e2a5,
    data2: 0xab8f,
    data3: 0x40a9,
    data4: [0x86, 0xcb, 0xde, 0x3c, 0x75, 0x59, 0x9b, 0x58],
};

#[repr(C)]
struct Variant {
    kind: u16,
    reserved: [u16; 3],
    value: [usize; 2],
}

impl Variant {
    fn empty() -> Self {
        Self {
            kind: VT_EMPTY,
            reserved: [0; 3],
            value: [0; 2],
        }
    }

    fn integer(value: i32) -> Self {
        Self {
            kind: VT_I4,
            value: [value as u32 as usize, 0],
            ..Self::empty()
        }
    }

    fn boolean(value: bool) -> Self {
        Self {
            kind: VT_BOOL,
            value: [if value { VARIANT_TRUE } else { VARIANT_FALSE }, 0],
            ..Self::empty()
        }
    }

    fn string(text: &str) -> Option<Self> {
        let string = allocate_string(text)?;
        Some(Self {
            kind: VT_BSTR,
            value: [string.expose_provenance(), 0],
            ..Self::empty()
        })
    }
}

#[cfg(test)]
#[repr(C)]
struct UnknownVtable {
    query_interface: QueryInterface,
    add_ref: AddRef,
    release: Release,
}

type QueryInterface = unsafe extern "system" fn(*mut c_void, *const Guid, Out) -> HResult;
type AddRef = unsafe extern "system" fn(*mut c_void) -> u32;
type Release = unsafe extern "system" fn(*mut c_void) -> u32;

#[repr(C)]
struct SimpleVtable {
    query_interface: QueryInterface,
    add_ref: AddRef,
    release: Release,
    provider_options: unsafe extern "system" fn(*mut c_void, *mut i32) -> HResult,
    pattern_provider: unsafe extern "system" fn(*mut c_void, i32, Out) -> HResult,
    property_value: unsafe extern "system" fn(*mut c_void, i32, *mut Variant) -> HResult,
    host_provider: unsafe extern "system" fn(*mut c_void, Out) -> HResult,
}

#[repr(C)]
struct FragmentVtable {
    query_interface: QueryInterface,
    add_ref: AddRef,
    release: Release,
    navigate: unsafe extern "system" fn(*mut c_void, i32, Out) -> HResult,
    runtime_id: unsafe extern "system" fn(*mut c_void, Out) -> HResult,
    bounding_rectangle: unsafe extern "system" fn(*mut c_void, *mut UiaRect) -> HResult,
    embedded_fragment_roots: unsafe extern "system" fn(*mut c_void, Out) -> HResult,
    set_focus: unsafe extern "system" fn(*mut c_void) -> HResult,
    fragment_root: unsafe extern "system" fn(*mut c_void, Out) -> HResult,
}

#[repr(C)]
struct FragmentRootVtable {
    query_interface: QueryInterface,
    add_ref: AddRef,
    release: Release,
    element_from_point: unsafe extern "system" fn(*mut c_void, f64, f64, Out) -> HResult,
    focus: unsafe extern "system" fn(*mut c_void, Out) -> HResult,
}

static SIMPLE_VTABLE: SimpleVtable = SimpleVtable {
    query_interface: simple_query_interface,
    add_ref: simple_add_ref,
    release: simple_release,
    provider_options,
    pattern_provider,
    property_value,
    host_provider,
};

static FRAGMENT_VTABLE: FragmentVtable = FragmentVtable {
    query_interface: fragment_query_interface,
    add_ref: fragment_add_ref,
    release: fragment_release,
    navigate,
    runtime_id,
    bounding_rectangle,
    embedded_fragment_roots,
    set_focus,
    fragment_root,
};

static FRAGMENT_ROOT_VTABLE: FragmentRootVtable = FragmentRootVtable {
    query_interface: root_query_interface,
    add_ref: root_add_ref,
    release: root_release,
    element_from_point,
    focus,
};

#[link(name = "uiautomationcore")]
unsafe extern "system" {
    fn UiaReturnRawElementProvider(
        window: Handle,
        wparam: usize,
        lparam: isize,
        provider: *mut c_void,
    ) -> isize;
    fn UiaHostProviderFromHwnd(window: Handle, provider: Out) -> HResult;
    fn UiaRaiseAutomationEvent(provider: *mut c_void, event: i32) -> HResult;
    fn UiaClientsAreListening() -> i32;
}

#[link(name = "oleaut32")]
unsafe extern "system" {
    fn SysAllocStringLen(text: *const u16, length: u32) -> *mut u16;
    fn SafeArrayCreateVector(kind: u16, lower_bound: i32, elements: u32) -> *mut c_void;
    fn SafeArrayPutElement(
        array: *mut c_void,
        indices: *const i32,
        value: *const c_void,
    ) -> HResult;
    fn SafeArrayDestroy(array: *mut c_void) -> HResult;
}

#[link(name = "user32")]
unsafe extern "system" {
    fn ClientToScreen(window: Handle, point: *mut Point) -> i32;
}

fn allocate_string(text: &str) -> Option<*mut u16> {
    let units: Vec<u16> = text.encode_utf16().collect();
    let length = u32::try_from(units.len()).ok()?;
    // SAFETY: `units` holds `length` UTF-16 units and stays alive for the call, which copies
    // them into a new BSTR the caller then owns.
    let string = unsafe { SysAllocStringLen(units.as_ptr(), length) };
    (!string.is_null()).then_some(string)
}

#[derive(Debug, Default)]
struct AutomationTree {
    window: usize,
    nodes: Vec<AccessNode>,
    scale: f32,
}

#[derive(Debug, Default)]
pub(super) struct Automation {
    tree: Arc<Mutex<AutomationTree>>,
}

impl Automation {
    pub(super) fn attach(&self, window: Handle) {
        if let Ok(mut tree) = self.tree.lock() {
            tree.window = window.expose_provenance();
        }
    }

    pub(super) fn detach(&self, window: Handle) {
        if let Ok(mut tree) = self.tree.lock() {
            tree.window = 0;
            tree.nodes.clear();
        }
        // SAFETY: `window` is the live window this bridge served; a null provider tells UI
        // Automation to release what it holds for the window.
        unsafe { UiaReturnRawElementProvider(window, 0, 0, std::ptr::null_mut()) };
    }

    pub(super) fn answer_get_object(
        &self,
        window: Handle,
        wparam: usize,
        lparam: isize,
    ) -> Option<isize> {
        if lparam as i32 != uia::ROOT_OBJECT_ID {
            return None;
        }
        let root = Provider::create(&self.tree, Element::Root);
        let simple = Provider::interface(root, SIMPLE_OFFSET);
        // SAFETY: `simple` is the live provider's `IRawElementProviderSimple` interface; UI
        // Automation takes its own reference.
        let result = unsafe { UiaReturnRawElementProvider(window, wparam, lparam, simple) };
        // SAFETY: `root` holds the one reference `create` gave this function.
        unsafe { Provider::release(root) };
        Some(result)
    }

    pub(super) fn update(&self, nodes: &[AccessNode], scale: f32) {
        let moved_to = {
            let Ok(mut tree) = self.tree.lock() else {
                return;
            };
            let moved_to = uia::focus_moved_to(&tree.nodes, nodes);
            tree.nodes.clear();
            tree.nodes.extend_from_slice(nodes);
            tree.scale = scale;
            moved_to
        };
        let Some(id) = moved_to else {
            return;
        };
        // SAFETY: the call takes no arguments and only reads UI Automation's client list.
        if unsafe { UiaClientsAreListening() } == 0 {
            return;
        }
        let provider = Provider::create(&self.tree, Element::Node(id));
        let simple = Provider::interface(provider, SIMPLE_OFFSET);
        // SAFETY: `simple` is the live provider's `IRawElementProviderSimple` interface, and no
        // lock on the tree is held while UI Automation calls back into it.
        unsafe { UiaRaiseAutomationEvent(simple, uia::FOCUS_CHANGED_EVENT) };
        // SAFETY: `provider` holds the one reference `create` gave this function.
        unsafe { Provider::release(provider) };
    }
}

#[repr(C)]
struct Provider {
    simple: &'static SimpleVtable,
    fragment: &'static FragmentVtable,
    root: &'static FragmentRootVtable,
    references: AtomicU32,
    tree: Arc<Mutex<AutomationTree>>,
    element: Element,
}

const SIMPLE_OFFSET: usize = std::mem::offset_of!(Provider, simple);
const FRAGMENT_OFFSET: usize = std::mem::offset_of!(Provider, fragment);
const ROOT_OFFSET: usize = std::mem::offset_of!(Provider, root);

enum Lookup {
    Root(usize),
    Node(usize, AccessNode, f32),
}

impl Provider {
    fn create(tree: &Arc<Mutex<AutomationTree>>, element: Element) -> *mut Provider {
        Box::into_raw(Box::new(Provider {
            simple: &SIMPLE_VTABLE,
            fragment: &FRAGMENT_VTABLE,
            root: &FRAGMENT_ROOT_VTABLE,
            references: AtomicU32::new(1),
            tree: Arc::clone(tree),
            element,
        }))
    }

    fn interface(this: *mut Provider, offset: usize) -> *mut c_void {
        this.cast::<u8>().wrapping_add(offset).cast()
    }

    fn create_out(
        tree: &Arc<Mutex<AutomationTree>>,
        element: Option<Element>,
        out: Out,
        offset: usize,
    ) -> HResult {
        let interface = element.map_or(std::ptr::null_mut(), |element| {
            Self::interface(Self::create(tree, element), offset)
        });
        write_out(out, interface)
    }

    fn add_ref(&self) -> u32 {
        self.references
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1)
    }

    unsafe fn release(this: *mut Provider) -> u32 {
        // SAFETY: the caller holds a reference, so the provider is alive.
        let left = unsafe { &*this }
            .references
            .fetch_sub(1, Ordering::AcqRel)
            .saturating_sub(1);
        if left == 0 {
            // SAFETY: the last reference is gone, so nothing else reaches the box `create`
            // leaked.
            drop(unsafe { Box::from_raw(this) });
        }
        left
    }

    fn lookup(&self) -> Result<Lookup, HResult> {
        let tree = self.tree.lock().map_err(|_| UIA_E_ELEMENTNOTAVAILABLE)?;
        if tree.window == 0 {
            return Err(UIA_E_ELEMENTNOTAVAILABLE);
        }
        match self.element {
            Element::Root => Ok(Lookup::Root(tree.window)),
            Element::Node(id) => uia::exposed_node(&tree.nodes, id)
                .map(|node| Lookup::Node(tree.window, node.clone(), tree.scale))
                .ok_or(UIA_E_ELEMENTNOTAVAILABLE),
        }
    }

    fn with_nodes<T>(&self, read: impl FnOnce(&AutomationTree) -> T) -> Result<T, HResult> {
        let tree = self.tree.lock().map_err(|_| UIA_E_ELEMENTNOTAVAILABLE)?;
        if tree.window == 0 {
            return Err(UIA_E_ELEMENTNOTAVAILABLE);
        }
        if let Element::Node(id) = self.element
            && uia::exposed_node(&tree.nodes, id).is_none()
        {
            return Err(UIA_E_ELEMENTNOTAVAILABLE);
        }
        Ok(read(&tree))
    }

    fn query(this: *mut Provider, iid: *const Guid, out: Out) -> HResult {
        // SAFETY: COM passes a valid IID pointer or null; null is refused.
        let Some(&iid) = (unsafe { iid.as_ref() }) else {
            let _ = write_out(out, std::ptr::null_mut());
            return E_POINTER;
        };
        // SAFETY: `this` is a live provider, reached through one of its interfaces.
        let provider = unsafe { &*this };
        let offset = if iid == IID_IUNKNOWN || iid == IID_SIMPLE {
            SIMPLE_OFFSET
        } else if iid == IID_FRAGMENT {
            FRAGMENT_OFFSET
        } else if iid == IID_FRAGMENT_ROOT && provider.element == Element::Root {
            ROOT_OFFSET
        } else {
            let _ = write_out(out, std::ptr::null_mut());
            return E_NOINTERFACE;
        };
        provider.add_ref();
        write_out(out, Self::interface(this, offset))
    }
}

fn write_out(out: Out, value: *mut c_void) -> HResult {
    if out.is_null() {
        return E_POINTER;
    }
    // SAFETY: COM out parameters point to writable storage for one interface pointer.
    unsafe { out.write(value) };
    S_OK
}

fn provider_at(this: *mut c_void, offset: usize) -> *mut Provider {
    this.cast::<u8>().wrapping_sub(offset).cast()
}

fn provider_ref<'a>(this: *mut c_void, offset: usize) -> &'a Provider {
    // SAFETY: every interface pointer handed out points `offset` bytes into a live provider,
    // kept alive by the reference the caller holds for the call.
    unsafe { &*provider_at(this, offset) }
}

unsafe extern "system" fn simple_query_interface(
    this: *mut c_void,
    iid: *const Guid,
    out: Out,
) -> HResult {
    Provider::query(provider_at(this, SIMPLE_OFFSET), iid, out)
}

unsafe extern "system" fn simple_add_ref(this: *mut c_void) -> u32 {
    provider_ref(this, SIMPLE_OFFSET).add_ref()
}

unsafe extern "system" fn simple_release(this: *mut c_void) -> u32 {
    // SAFETY: the caller gives up the reference it holds on this interface.
    unsafe { Provider::release(provider_at(this, SIMPLE_OFFSET)) }
}

unsafe extern "system" fn fragment_query_interface(
    this: *mut c_void,
    iid: *const Guid,
    out: Out,
) -> HResult {
    Provider::query(provider_at(this, FRAGMENT_OFFSET), iid, out)
}

unsafe extern "system" fn fragment_add_ref(this: *mut c_void) -> u32 {
    provider_ref(this, FRAGMENT_OFFSET).add_ref()
}

unsafe extern "system" fn fragment_release(this: *mut c_void) -> u32 {
    // SAFETY: the caller gives up the reference it holds on this interface.
    unsafe { Provider::release(provider_at(this, FRAGMENT_OFFSET)) }
}

unsafe extern "system" fn root_query_interface(
    this: *mut c_void,
    iid: *const Guid,
    out: Out,
) -> HResult {
    Provider::query(provider_at(this, ROOT_OFFSET), iid, out)
}

unsafe extern "system" fn root_add_ref(this: *mut c_void) -> u32 {
    provider_ref(this, ROOT_OFFSET).add_ref()
}

unsafe extern "system" fn root_release(this: *mut c_void) -> u32 {
    // SAFETY: the caller gives up the reference it holds on this interface.
    unsafe { Provider::release(provider_at(this, ROOT_OFFSET)) }
}

unsafe extern "system" fn provider_options(_this: *mut c_void, out: *mut i32) -> HResult {
    if out.is_null() {
        return E_POINTER;
    }
    // SAFETY: `out` is a non-null `ProviderOptions` out parameter.
    unsafe { out.write(uia::SERVER_SIDE_PROVIDER) };
    S_OK
}

unsafe extern "system" fn pattern_provider(_this: *mut c_void, _pattern: i32, out: Out) -> HResult {
    write_out(out, std::ptr::null_mut())
}

fn node_property(node: &AccessNode, property: i32) -> Option<Variant> {
    Some(match property {
        uia::CONTROL_TYPE_PROPERTY => Variant::integer(uia::control_type(node.role)?),
        uia::NAME_PROPERTY => Variant::string(&node.name)?,
        uia::HAS_KEYBOARD_FOCUS_PROPERTY => Variant::boolean(node.state.focused),
        uia::IS_KEYBOARD_FOCUSABLE_PROPERTY => Variant::boolean(uia::is_keyboard_focusable(node)),
        uia::IS_ENABLED_PROPERTY => Variant::boolean(!node.state.disabled),
        uia::AUTOMATION_ID_PROPERTY => Variant::string(&node.id.to_string())?,
        uia::HELP_TEXT_PROPERTY => Variant::string(node.placeholder.as_deref()?)?,
        uia::HEADING_LEVEL_PROPERTY if node.role == AccessRole::Heading => {
            Variant::integer(uia::HEADING_LEVEL_1)
        }
        uia::LIVE_SETTING_PROPERTY if node.role == AccessRole::Status => {
            Variant::integer(uia::LIVE_SETTING_POLITE)
        }
        _ => return None,
    })
}

unsafe extern "system" fn property_value(
    this: *mut c_void,
    property: i32,
    out: *mut Variant,
) -> HResult {
    if out.is_null() {
        return E_POINTER;
    }
    let variant = match provider_ref(this, SIMPLE_OFFSET).lookup() {
        Err(error) => return error,
        Ok(Lookup::Root(_)) => Variant::empty(),
        Ok(Lookup::Node(_, node, _)) => {
            node_property(&node, property).unwrap_or_else(Variant::empty)
        }
    };
    // SAFETY: `out` is a non-null `VARIANT` out parameter; the caller owns what it receives.
    unsafe { out.write(variant) };
    S_OK
}

unsafe extern "system" fn host_provider(this: *mut c_void, out: Out) -> HResult {
    let window = match provider_ref(this, SIMPLE_OFFSET).lookup() {
        Err(error) => return error,
        Ok(Lookup::Node(..)) => return write_out(out, std::ptr::null_mut()),
        Ok(Lookup::Root(window)) => window,
    };
    if out.is_null() {
        return E_POINTER;
    }
    // SAFETY: `window` is the live window this bridge serves (its address was exposed when it
    // was attached) and `out` is a non-null out parameter for the host provider.
    unsafe { UiaHostProviderFromHwnd(std::ptr::with_exposed_provenance_mut(window), out) }
}

unsafe extern "system" fn navigate(this: *mut c_void, direction: i32, out: Out) -> HResult {
    let provider = provider_ref(this, FRAGMENT_OFFSET);
    let Some(direction) = Direction::from_raw(direction) else {
        let _ = write_out(out, std::ptr::null_mut());
        return E_INVALIDARG;
    };
    match provider.with_nodes(|tree| uia::navigate(&tree.nodes, provider.element, direction)) {
        Err(error) => error,
        Ok(target) => Provider::create_out(&provider.tree, target, out, FRAGMENT_OFFSET),
    }
}

unsafe extern "system" fn runtime_id(this: *mut c_void, out: Out) -> HResult {
    let id = match provider_ref(this, FRAGMENT_OFFSET).lookup() {
        Err(error) => return error,
        Ok(Lookup::Root(_)) => return write_out(out, std::ptr::null_mut()),
        Ok(Lookup::Node(_, node, _)) => node.id,
    };
    if out.is_null() {
        return E_POINTER;
    }
    let parts = uia::runtime_id(id);
    // SAFETY: the call takes its arguments by value and returns a new array or null.
    let array = unsafe { SafeArrayCreateVector(VT_I4, 0, parts.len() as u32) };
    if array.is_null() {
        return E_OUTOFMEMORY;
    }
    for (index, part) in (0_i32..).zip(parts) {
        // SAFETY: `array` is the live two-element `VT_I4` vector created above, `index` is
        // within it and `part` is an `i32` alive for the call, which copies it.
        let stored = unsafe { SafeArrayPutElement(array, &index, (&raw const part).cast()) };
        if stored != S_OK {
            // SAFETY: `array` was created above and is not handed out.
            unsafe { SafeArrayDestroy(array) };
            return stored;
        }
    }
    write_out(out, array)
}

unsafe extern "system" fn bounding_rectangle(this: *mut c_void, out: *mut UiaRect) -> HResult {
    if out.is_null() {
        return E_POINTER;
    }
    let rect = match provider_ref(this, FRAGMENT_OFFSET).lookup() {
        Err(error) => return error,
        Ok(Lookup::Root(_)) => UiaRect::default(),
        Ok(Lookup::Node(window, node, scale)) => {
            uia::screen_rect(node.bounds, scale, client_origin(window))
        }
    };
    // SAFETY: `out` is a non-null `UiaRect` out parameter.
    unsafe { out.write(rect) };
    S_OK
}

fn client_origin(window: usize) -> (i32, i32) {
    let mut origin = Point::default();
    // SAFETY: `window` is the live window this bridge serves and `origin` is writable.
    unsafe { ClientToScreen(std::ptr::with_exposed_provenance_mut(window), &mut origin) };
    (origin.x, origin.y)
}

unsafe extern "system" fn embedded_fragment_roots(_this: *mut c_void, out: Out) -> HResult {
    write_out(out, std::ptr::null_mut())
}

unsafe extern "system" fn set_focus(this: *mut c_void) -> HResult {
    match provider_ref(this, FRAGMENT_OFFSET).lookup() {
        Err(error) => error,
        Ok(_) => S_OK,
    }
}

unsafe extern "system" fn fragment_root(this: *mut c_void, out: Out) -> HResult {
    let provider = provider_ref(this, FRAGMENT_OFFSET);
    match provider.lookup() {
        Err(error) => error,
        Ok(_) => Provider::create_out(&provider.tree, Some(Element::Root), out, ROOT_OFFSET),
    }
}

unsafe extern "system" fn element_from_point(
    this: *mut c_void,
    x: f64,
    y: f64,
    out: Out,
) -> HResult {
    let provider = provider_ref(this, ROOT_OFFSET);
    let found = provider.with_nodes(|tree| {
        let (left, top) = client_origin(tree.window);
        let scale = f64::from(tree.scale.max(f32::MIN_POSITIVE));
        let logical = |screen: f64, origin: i32| ((screen - f64::from(origin)) / scale) as f32;
        uia::element_at(&tree.nodes, logical(x, left), logical(y, top))
    });
    match found {
        Err(error) => error,
        Ok(id) => Provider::create_out(&provider.tree, id.map(Element::Node), out, FRAGMENT_OFFSET),
    }
}

unsafe extern "system" fn focus(this: *mut c_void, out: Out) -> HResult {
    let provider = provider_ref(this, ROOT_OFFSET);
    match provider.with_nodes(|tree| {
        focused_node(&tree.nodes).filter(|&id| uia::exposed_node(&tree.nodes, id).is_some())
    }) {
        Err(error) => error,
        Ok(id) => Provider::create_out(&provider.tree, id.map(Element::Node), out, FRAGMENT_OFFSET),
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::{AccessRect, AccessState};
    use super::*;

    #[link(name = "oleaut32")]
    unsafe extern "system" {
        fn SysFreeString(string: *mut u16);
        fn SysStringLen(string: *const u16) -> u32;
    }

    fn node(
        id: u32,
        parent: Option<u32>,
        role: AccessRole,
        name: &str,
        focused: bool,
    ) -> AccessNode {
        AccessNode {
            id,
            parent,
            role,
            name: name.to_owned(),
            value: None,
            placeholder: None,
            state: AccessState {
                focused,
                ..AccessState::default()
            },
            bounds: AccessRect {
                x: 10.0,
                y: 20.0,
                width: 30.0,
                height: 40.0,
            },
        }
    }

    fn automation() -> Automation {
        let automation = Automation::default();
        if let Ok(mut tree) = automation.tree.lock() {
            tree.window = 1;
            tree.scale = 1.0;
            tree.nodes = vec![
                node(1, None, AccessRole::Window, "EggDB", false),
                node(2, Some(1), AccessRole::Heading, "Open a database", false),
                node(3, Some(1), AccessRole::Button, "Open", true),
            ];
        }
        automation
    }

    fn query(interface: *mut c_void, iid: &Guid) -> (HResult, *mut c_void) {
        let mut out = std::ptr::null_mut();
        // SAFETY: `interface` points to a live provider's interface, whose first field is a
        // vtable starting with `QueryInterface`.
        let vtable = unsafe { *interface.cast::<&UnknownVtable>() };
        // SAFETY: as above; `out` is writable.
        let result = unsafe { (vtable.query_interface)(interface, iid, &mut out) };
        (result, out)
    }

    fn release(interface: *mut c_void) -> u32 {
        // SAFETY: `interface` points to a live provider's interface holding a reference.
        let vtable = unsafe { *interface.cast::<&UnknownVtable>() };
        // SAFETY: as above; the reference is given up here.
        unsafe { (vtable.release)(interface) }
    }

    fn take_string(variant: Variant) -> String {
        assert_eq!(variant.kind, VT_BSTR);
        let string: *mut u16 = std::ptr::with_exposed_provenance_mut(variant.value[0]);
        // SAFETY: `string` is the BSTR `property_value` allocated.
        let length = unsafe { SysStringLen(string) } as usize;
        // SAFETY: a BSTR holds `SysStringLen` UTF-16 units.
        let units = unsafe { std::slice::from_raw_parts(string, length) };
        let text = String::from_utf16_lossy(units);
        // SAFETY: the BSTR is ours to free and is not used after this.
        unsafe { SysFreeString(string) };
        text
    }

    #[test]
    fn the_root_answers_its_interfaces_and_leads_to_the_nodes() {
        let automation = automation();
        let root = Provider::create(&automation.tree, Element::Root);
        let simple = Provider::interface(root, SIMPLE_OFFSET);
        let (result, fragment) = query(simple, &IID_FRAGMENT);
        assert_eq!(result, S_OK);
        let (result, fragment_root) = query(simple, &IID_FRAGMENT_ROOT);
        assert_eq!(result, S_OK);
        let (result, none) = query(
            simple,
            &Guid {
                data1: 1,
                ..IID_IUNKNOWN
            },
        );
        assert_eq!((result, none), (E_NOINTERFACE, std::ptr::null_mut()));

        let mut child = std::ptr::null_mut();
        // SAFETY: `fragment` is the provider's fragment interface and `child` is writable.
        assert_eq!(unsafe { navigate(fragment, 3, &mut child) }, S_OK);
        assert!(!child.is_null());
        let child_provider = provider_ref(child, FRAGMENT_OFFSET);
        assert_eq!(child_provider.element, Element::Node(2));
        let mut variant = Variant::empty();
        let child_simple = Provider::interface(provider_at(child, FRAGMENT_OFFSET), SIMPLE_OFFSET);
        // SAFETY: `child_simple` is the child's simple interface and `variant` is writable.
        let result = unsafe { property_value(child_simple, uia::NAME_PROPERTY, &mut variant) };
        assert_eq!(result, S_OK);
        assert_eq!(take_string(variant), "Open a database");
        let mut variant = Variant::empty();
        // SAFETY: as above.
        let result =
            unsafe { property_value(child_simple, uia::CONTROL_TYPE_PROPERTY, &mut variant) };
        assert_eq!(result, S_OK);
        assert_eq!(
            (variant.kind, variant.value[0]),
            (VT_I4, uia::TEXT_CONTROL as usize)
        );
        let (result, child_root) = query(child_simple, &IID_FRAGMENT_ROOT);
        assert_eq!((result, child_root), (E_NOINTERFACE, std::ptr::null_mut()));

        let mut focused = std::ptr::null_mut();
        // SAFETY: `fragment_root` is the root's fragment-root interface and `focused` writable.
        assert_eq!(unsafe { focus(fragment_root, &mut focused) }, S_OK);
        assert_eq!(
            provider_ref(focused, FRAGMENT_OFFSET).element,
            Element::Node(3)
        );
        let mut variant = Variant::empty();
        let focused_simple =
            Provider::interface(provider_at(focused, FRAGMENT_OFFSET), SIMPLE_OFFSET);
        // SAFETY: as above.
        let result = unsafe {
            property_value(
                focused_simple,
                uia::HAS_KEYBOARD_FOCUS_PROPERTY,
                &mut variant,
            )
        };
        assert_eq!(result, S_OK);
        assert_eq!((variant.kind, variant.value[0]), (VT_BOOL, VARIANT_TRUE));

        for interface in [focused, child, fragment_root, fragment] {
            release(interface);
        }
        assert_eq!(release(simple), 0);
    }

    #[test]
    fn providers_outliving_their_window_report_the_element_gone() {
        let automation = automation();
        let node = Provider::create(&automation.tree, Element::Node(3));
        if let Ok(mut tree) = automation.tree.lock() {
            tree.window = 0;
            tree.nodes.clear();
        }
        let simple = Provider::interface(node, SIMPLE_OFFSET);
        let mut variant = Variant::empty();
        // SAFETY: `simple` is the provider's simple interface and `variant` is writable.
        let result = unsafe { property_value(simple, uia::NAME_PROPERTY, &mut variant) };
        assert_eq!(result, UIA_E_ELEMENTNOTAVAILABLE);
        assert_eq!(release(simple), 0);
    }
}
