use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use super::super::uia::{self, Direction, Element, UiaRect};
use super::super::{
    AccessAction, AccessNode, AccessRequest, AccessRole, MAX_ACCESS_VALUE_CHARS, access_request,
    focused_node,
};
use super::{Handle, Point};

type HResult = i32;
type Out = *mut *mut c_void;

const S_OK: HResult = 0;
const E_NOINTERFACE: HResult = 0x8000_4002_u32 as i32;
const E_POINTER: HResult = 0x8000_4003_u32 as i32;
const E_INVALIDARG: HResult = 0x8007_0057_u32 as i32;
const E_OUTOFMEMORY: HResult = 0x8007_000E_u32 as i32;
const UIA_E_ELEMENTNOTENABLED: HResult = 0x8004_0200_u32 as i32;
const UIA_E_ELEMENTNOTAVAILABLE: HResult = 0x8004_0201_u32 as i32;
const UIA_E_INVALIDOPERATION: HResult = 0x8013_1509_u32 as i32;

const VT_EMPTY: u16 = 0;
const VT_I4: u16 = 3;
const VT_BSTR: u16 = 8;
const VT_BOOL: u16 = 11;
const VT_UNKNOWN: u16 = 13;
const VARIANT_TRUE: usize = 0xFFFF;
const VARIANT_FALSE: usize = 0;

const WM_NULL: u32 = 0x0000;
const MAX_PENDING_REQUESTS: usize = 64;
const MAX_VALUE_UNITS: usize = MAX_ACCESS_VALUE_CHARS * 2 + 1;

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
struct Guid {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

impl Guid {
    const fn new(data1: u32, data2: u16, data3: u16, data4: [u8; 8]) -> Self {
        Self {
            data1,
            data2,
            data3,
            data4,
        }
    }
}

const IID_IUNKNOWN: Guid = Guid::new(0, 0, 0, [0xC0, 0, 0, 0, 0, 0, 0, 0x46]);
const IID_SIMPLE: Guid = Guid::new(
    0xd6dd_68d1,
    0x86fd,
    0x4332,
    [0x86, 0x66, 0x9a, 0xbe, 0xde, 0xa2, 0xd2, 0x4c],
);
const IID_FRAGMENT: Guid = Guid::new(
    0xf706_3da8,
    0x8359,
    0x439c,
    [0x92, 0x97, 0xbb, 0xc5, 0x29, 0x9a, 0x7d, 0x87],
);
const IID_FRAGMENT_ROOT: Guid = Guid::new(
    0x620c_e2a5,
    0xab8f,
    0x40a9,
    [0x86, 0xcb, 0xde, 0x3c, 0x75, 0x59, 0x9b, 0x58],
);
const IID_INVOKE: Guid = Guid::new(
    0x54fc_b24b,
    0xe18e,
    0x47a2,
    [0xb4, 0xd3, 0xec, 0xcb, 0xe7, 0x75, 0x99, 0xa2],
);
const IID_TOGGLE: Guid = Guid::new(
    0x56d0_0bd0,
    0xc4f4,
    0x433c,
    [0xa8, 0x36, 0x1a, 0x52, 0xa5, 0x7e, 0x08, 0x92],
);
const IID_VALUE: Guid = Guid::new(
    0xc793_5180,
    0x6fb3,
    0x4201,
    [0xb1, 0x74, 0x7d, 0xf7, 0x3a, 0xdb, 0xf6, 0x4a],
);
const IID_EXPAND_COLLAPSE: Guid = Guid::new(
    0xd847_d3a5,
    0xcab0,
    0x4a98,
    [0x8c, 0x32, 0xec, 0xb4, 0x5c, 0x59, 0xad, 0x24],
);
const IID_SELECTION_ITEM: Guid = Guid::new(
    0x2aca_d808,
    0xb2d4,
    0x452d,
    [0xa4, 0x07, 0x91, 0xff, 0x1a, 0xd1, 0x67, 0xb2],
);
const IID_GRID: Guid = Guid::new(
    0xb17d_6187,
    0x0907,
    0x464b,
    [0xa1, 0x68, 0x0e, 0xf1, 0x7a, 0x15, 0x72, 0xb1],
);
const IID_GRID_ITEM: Guid = Guid::new(
    0xd025_41f1,
    0xfb81,
    0x4d64,
    [0xae, 0x32, 0xf5, 0x20, 0xf8, 0xa6, 0xdb, 0xd1],
);
const IID_TABLE: Guid = Guid::new(
    0x9c86_0395,
    0x97b3,
    0x490a,
    [0xb5, 0x2a, 0x85, 0x8c, 0xc2, 0x2a, 0xf1, 0x66],
);
const IID_TABLE_ITEM: Guid = Guid::new(
    0xb973_4fa6,
    0x771f,
    0x4d78,
    [0x9c, 0x90, 0x25, 0x17, 0x99, 0x93, 0x49, 0xcd],
);

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

    fn count(value: usize) -> Self {
        Self::integer(uia::saturating_int(value))
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

    fn bitwise_copy(&self) -> Self {
        Self {
            kind: self.kind,
            reserved: self.reserved,
            value: self.value,
        }
    }
}

struct OwnedVariant(Variant);

impl Drop for OwnedVariant {
    fn drop(&mut self) {
        // SAFETY: the variant was built by this bridge (empty, an integer, a boolean or a BSTR
        // it owns) and is not used after this; clearing frees its BSTR.
        unsafe { VariantClear(&mut self.0) };
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
type Method = unsafe extern "system" fn(*mut c_void) -> HResult;
type IntGetter = unsafe extern "system" fn(*mut c_void, *mut i32) -> HResult;
type OutGetter = unsafe extern "system" fn(*mut c_void, Out) -> HResult;

#[repr(C)]
struct SimpleVtable {
    query_interface: QueryInterface,
    add_ref: AddRef,
    release: Release,
    provider_options: unsafe extern "system" fn(*mut c_void, *mut i32) -> HResult,
    pattern_provider: unsafe extern "system" fn(*mut c_void, i32, Out) -> HResult,
    property_value: unsafe extern "system" fn(*mut c_void, i32, *mut Variant) -> HResult,
    host_provider: OutGetter,
}

#[repr(C)]
struct FragmentVtable {
    query_interface: QueryInterface,
    add_ref: AddRef,
    release: Release,
    navigate: unsafe extern "system" fn(*mut c_void, i32, Out) -> HResult,
    runtime_id: OutGetter,
    bounding_rectangle: unsafe extern "system" fn(*mut c_void, *mut UiaRect) -> HResult,
    embedded_fragment_roots: OutGetter,
    set_focus: Method,
    fragment_root: OutGetter,
}

#[repr(C)]
struct FragmentRootVtable {
    query_interface: QueryInterface,
    add_ref: AddRef,
    release: Release,
    element_from_point: unsafe extern "system" fn(*mut c_void, f64, f64, Out) -> HResult,
    focus: OutGetter,
}

#[repr(C)]
struct InvokeVtable {
    query_interface: QueryInterface,
    add_ref: AddRef,
    release: Release,
    invoke: Method,
}

#[repr(C)]
struct ToggleVtable {
    query_interface: QueryInterface,
    add_ref: AddRef,
    release: Release,
    toggle: Method,
    get_toggle_state: IntGetter,
}

#[repr(C)]
struct ValueVtable {
    query_interface: QueryInterface,
    add_ref: AddRef,
    release: Release,
    set_value: unsafe extern "system" fn(*mut c_void, *const u16) -> HResult,
    get_value: unsafe extern "system" fn(*mut c_void, *mut *mut u16) -> HResult,
    get_is_read_only: IntGetter,
}

#[repr(C)]
struct ExpandCollapseVtable {
    query_interface: QueryInterface,
    add_ref: AddRef,
    release: Release,
    expand: Method,
    collapse: Method,
    get_expand_collapse_state: IntGetter,
}

#[repr(C)]
struct SelectionItemVtable {
    query_interface: QueryInterface,
    add_ref: AddRef,
    release: Release,
    select: Method,
    add_to_selection: Method,
    remove_from_selection: Method,
    get_is_selected: IntGetter,
    get_selection_container: OutGetter,
}

#[repr(C)]
struct GridVtable {
    query_interface: QueryInterface,
    add_ref: AddRef,
    release: Release,
    get_item: unsafe extern "system" fn(*mut c_void, i32, i32, Out) -> HResult,
    get_row_count: IntGetter,
    get_column_count: IntGetter,
}

#[repr(C)]
struct GridItemVtable {
    query_interface: QueryInterface,
    add_ref: AddRef,
    release: Release,
    get_row: IntGetter,
    get_column: IntGetter,
    get_row_span: IntGetter,
    get_column_span: IntGetter,
    get_containing_grid: OutGetter,
}

#[repr(C)]
struct TableVtable {
    query_interface: QueryInterface,
    add_ref: AddRef,
    release: Release,
    get_row_headers: OutGetter,
    get_column_headers: OutGetter,
    get_row_or_column_major: IntGetter,
}

#[repr(C)]
struct TableItemVtable {
    query_interface: QueryInterface,
    add_ref: AddRef,
    release: Release,
    get_row_header_items: OutGetter,
    get_column_header_items: OutGetter,
}

static SIMPLE_VTABLE: SimpleVtable = SimpleVtable {
    query_interface: forward_query_interface::<SIMPLE_OFFSET>,
    add_ref: forward_add_ref::<SIMPLE_OFFSET>,
    release: forward_release::<SIMPLE_OFFSET>,
    provider_options,
    pattern_provider,
    property_value,
    host_provider,
};

static FRAGMENT_VTABLE: FragmentVtable = FragmentVtable {
    query_interface: forward_query_interface::<FRAGMENT_OFFSET>,
    add_ref: forward_add_ref::<FRAGMENT_OFFSET>,
    release: forward_release::<FRAGMENT_OFFSET>,
    navigate,
    runtime_id,
    bounding_rectangle,
    embedded_fragment_roots,
    set_focus,
    fragment_root,
};

static FRAGMENT_ROOT_VTABLE: FragmentRootVtable = FragmentRootVtable {
    query_interface: forward_query_interface::<ROOT_OFFSET>,
    add_ref: forward_add_ref::<ROOT_OFFSET>,
    release: forward_release::<ROOT_OFFSET>,
    element_from_point,
    focus,
};

static INVOKE_VTABLE: InvokeVtable = InvokeVtable {
    query_interface: forward_query_interface::<INVOKE_OFFSET>,
    add_ref: forward_add_ref::<INVOKE_OFFSET>,
    release: forward_release::<INVOKE_OFFSET>,
    invoke,
};

static TOGGLE_VTABLE: ToggleVtable = ToggleVtable {
    query_interface: forward_query_interface::<TOGGLE_OFFSET>,
    add_ref: forward_add_ref::<TOGGLE_OFFSET>,
    release: forward_release::<TOGGLE_OFFSET>,
    toggle,
    get_toggle_state,
};

static VALUE_VTABLE: ValueVtable = ValueVtable {
    query_interface: forward_query_interface::<VALUE_OFFSET>,
    add_ref: forward_add_ref::<VALUE_OFFSET>,
    release: forward_release::<VALUE_OFFSET>,
    set_value,
    get_value,
    get_is_read_only,
};

static EXPAND_COLLAPSE_VTABLE: ExpandCollapseVtable = ExpandCollapseVtable {
    query_interface: forward_query_interface::<EXPAND_COLLAPSE_OFFSET>,
    add_ref: forward_add_ref::<EXPAND_COLLAPSE_OFFSET>,
    release: forward_release::<EXPAND_COLLAPSE_OFFSET>,
    expand,
    collapse,
    get_expand_collapse_state,
};

static SELECTION_ITEM_VTABLE: SelectionItemVtable = SelectionItemVtable {
    query_interface: forward_query_interface::<SELECTION_ITEM_OFFSET>,
    add_ref: forward_add_ref::<SELECTION_ITEM_OFFSET>,
    release: forward_release::<SELECTION_ITEM_OFFSET>,
    select,
    add_to_selection,
    remove_from_selection,
    get_is_selected,
    get_selection_container,
};

static GRID_VTABLE: GridVtable = GridVtable {
    query_interface: forward_query_interface::<GRID_OFFSET>,
    add_ref: forward_add_ref::<GRID_OFFSET>,
    release: forward_release::<GRID_OFFSET>,
    get_item,
    get_row_count,
    get_column_count,
};

static GRID_ITEM_VTABLE: GridItemVtable = GridItemVtable {
    query_interface: forward_query_interface::<GRID_ITEM_OFFSET>,
    add_ref: forward_add_ref::<GRID_ITEM_OFFSET>,
    release: forward_release::<GRID_ITEM_OFFSET>,
    get_row,
    get_column,
    get_row_span: get_span,
    get_column_span: get_span,
    get_containing_grid,
};

static TABLE_VTABLE: TableVtable = TableVtable {
    query_interface: forward_query_interface::<TABLE_OFFSET>,
    add_ref: forward_add_ref::<TABLE_OFFSET>,
    release: forward_release::<TABLE_OFFSET>,
    get_row_headers: empty_provider_array::<TABLE_OFFSET>,
    get_column_headers,
    get_row_or_column_major,
};

static TABLE_ITEM_VTABLE: TableItemVtable = TableItemVtable {
    query_interface: forward_query_interface::<TABLE_ITEM_OFFSET>,
    add_ref: forward_add_ref::<TABLE_ITEM_OFFSET>,
    release: forward_release::<TABLE_ITEM_OFFSET>,
    get_row_header_items: empty_provider_array::<TABLE_ITEM_OFFSET>,
    get_column_header_items,
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
    fn UiaRaiseAutomationPropertyChangedEvent(
        provider: *mut c_void,
        property: i32,
        old: Variant,
        new: Variant,
    ) -> HResult;
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
    fn VariantClear(variant: *mut Variant) -> HResult;
}

#[link(name = "user32")]
unsafe extern "system" {
    fn ClientToScreen(window: Handle, point: *mut Point) -> i32;
    fn PostMessageW(window: Handle, message: u32, wparam: usize, lparam: isize) -> i32;
}

fn allocate_string(text: &str) -> Option<*mut u16> {
    let units: Vec<u16> = text.encode_utf16().collect();
    let length = u32::try_from(units.len()).ok()?;
    // SAFETY: `units` holds `length` UTF-16 units and stays alive for the call, which copies
    // them into a new BSTR the caller then owns.
    let string = unsafe { SysAllocStringLen(units.as_ptr(), length) };
    (!string.is_null()).then_some(string)
}

fn read_wide_string(text: *const u16, limit: usize) -> Option<String> {
    if text.is_null() {
        return None;
    }
    let mut units = Vec::new();
    for index in 0..limit {
        // SAFETY: `text` is the caller's NUL-terminated string and no unit before `index` was
        // the terminator, so this unit is still inside it.
        let unit = unsafe { text.wrapping_add(index).read() };
        if unit == 0 {
            return String::from_utf16(&units).ok();
        }
        units.push(unit);
    }
    None
}

#[derive(Debug, Default)]
struct AutomationTree {
    window: usize,
    nodes: Vec<AccessNode>,
    scale: f32,
    requests: Vec<AccessRequest>,
}

struct PropertyChange {
    node: u32,
    property: i32,
    old: OwnedVariant,
    new: OwnedVariant,
}

#[derive(Default)]
struct TreeEvents {
    properties: Vec<PropertyChange>,
    selected: Vec<u32>,
    live_regions: Vec<u32>,
    focus: Option<u32>,
}

impl TreeEvents {
    fn between(old: &[AccessNode], new: &[AccessNode]) -> Self {
        let properties = uia::property_changes(old, new)
            .into_iter()
            .filter_map(|(node, property)| {
                let (before, after) = (node_with_id(old, node)?, node_with_id(new, node)?);
                let property_of = |node: &AccessNode| {
                    OwnedVariant(node_property(node, property).unwrap_or_else(Variant::empty))
                };
                Some(PropertyChange {
                    node,
                    property,
                    old: property_of(before),
                    new: property_of(after),
                })
            })
            .collect();
        Self {
            properties,
            selected: uia::newly_selected(old, new),
            live_regions: uia::live_regions_changed(old, new),
            focus: uia::focus_moved_to(old, new),
        }
    }
}

fn node_with_id(nodes: &[AccessNode], id: u32) -> Option<&AccessNode> {
    nodes.iter().find(|node| node.id == id)
}

fn raise_for_node(tree: &Arc<Mutex<AutomationTree>>, id: u32, raise: impl FnOnce(*mut c_void)) {
    let provider = Provider::create(tree, Element::Node(id));
    raise(Provider::interface(provider, SIMPLE_OFFSET));
    // SAFETY: `provider` holds the one reference `create` gave this function.
    unsafe { Provider::release(provider) };
}

fn raise_event(tree: &Arc<Mutex<AutomationTree>>, id: u32, event: i32) {
    raise_for_node(tree, id, |simple| {
        // SAFETY: `simple` is the live provider's `IRawElementProviderSimple` interface, and no
        // lock on the tree is held while UI Automation calls back into it.
        unsafe { UiaRaiseAutomationEvent(simple, event) };
    });
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
            tree.requests.clear();
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

    pub(super) fn take_requests(&self) -> Vec<AccessRequest> {
        self.tree
            .lock()
            .map(|mut tree| std::mem::take(&mut tree.requests))
            .unwrap_or_default()
    }

    pub(super) fn update(&self, nodes: &[AccessNode], scale: f32) {
        // SAFETY: the call takes no arguments and only reads UI Automation's client list.
        let listening = unsafe { UiaClientsAreListening() } != 0;
        let events = {
            let Ok(mut tree) = self.tree.lock() else {
                return;
            };
            let events = if listening {
                TreeEvents::between(&tree.nodes, nodes)
            } else {
                TreeEvents::default()
            };
            tree.nodes.clear();
            tree.nodes.extend_from_slice(nodes);
            tree.scale = scale;
            events
        };
        for change in &events.properties {
            raise_for_node(&self.tree, change.node, |simple| {
                // SAFETY: `simple` is the live provider's `IRawElementProviderSimple`
                // interface, no lock on the tree is held while UI Automation calls back into
                // it, and the VARIANTs are bitwise copies it only reads; `events` keeps and
                // frees the originals.
                unsafe {
                    UiaRaiseAutomationPropertyChangedEvent(
                        simple,
                        change.property,
                        change.old.0.bitwise_copy(),
                        change.new.0.bitwise_copy(),
                    )
                };
            });
        }
        for &id in &events.selected {
            raise_event(&self.tree, id, uia::ELEMENT_SELECTED_EVENT);
        }
        for &id in &events.live_regions {
            raise_event(&self.tree, id, uia::LIVE_REGION_CHANGED_EVENT);
        }
        if let Some(id) = events.focus {
            raise_event(&self.tree, id, uia::FOCUS_CHANGED_EVENT);
        }
    }
}

#[repr(C)]
struct Provider {
    simple: &'static SimpleVtable,
    fragment: &'static FragmentVtable,
    root: &'static FragmentRootVtable,
    invoke: &'static InvokeVtable,
    toggle: &'static ToggleVtable,
    value: &'static ValueVtable,
    expand_collapse: &'static ExpandCollapseVtable,
    selection_item: &'static SelectionItemVtable,
    grid: &'static GridVtable,
    grid_item: &'static GridItemVtable,
    table: &'static TableVtable,
    table_item: &'static TableItemVtable,
    references: AtomicU32,
    tree: Arc<Mutex<AutomationTree>>,
    element: Element,
}

const SIMPLE_OFFSET: usize = std::mem::offset_of!(Provider, simple);
const FRAGMENT_OFFSET: usize = std::mem::offset_of!(Provider, fragment);
const ROOT_OFFSET: usize = std::mem::offset_of!(Provider, root);
const INVOKE_OFFSET: usize = std::mem::offset_of!(Provider, invoke);
const TOGGLE_OFFSET: usize = std::mem::offset_of!(Provider, toggle);
const VALUE_OFFSET: usize = std::mem::offset_of!(Provider, value);
const EXPAND_COLLAPSE_OFFSET: usize = std::mem::offset_of!(Provider, expand_collapse);
const SELECTION_ITEM_OFFSET: usize = std::mem::offset_of!(Provider, selection_item);
const GRID_OFFSET: usize = std::mem::offset_of!(Provider, grid);
const GRID_ITEM_OFFSET: usize = std::mem::offset_of!(Provider, grid_item);
const TABLE_OFFSET: usize = std::mem::offset_of!(Provider, table);
const TABLE_ITEM_OFFSET: usize = std::mem::offset_of!(Provider, table_item);

const PATTERN_INTERFACES: [(i32, Guid, usize); 9] = [
    (uia::INVOKE_PATTERN, IID_INVOKE, INVOKE_OFFSET),
    (uia::TOGGLE_PATTERN, IID_TOGGLE, TOGGLE_OFFSET),
    (uia::VALUE_PATTERN, IID_VALUE, VALUE_OFFSET),
    (
        uia::EXPAND_COLLAPSE_PATTERN,
        IID_EXPAND_COLLAPSE,
        EXPAND_COLLAPSE_OFFSET,
    ),
    (
        uia::SELECTION_ITEM_PATTERN,
        IID_SELECTION_ITEM,
        SELECTION_ITEM_OFFSET,
    ),
    (uia::GRID_PATTERN, IID_GRID, GRID_OFFSET),
    (uia::GRID_ITEM_PATTERN, IID_GRID_ITEM, GRID_ITEM_OFFSET),
    (uia::TABLE_PATTERN, IID_TABLE, TABLE_OFFSET),
    (uia::TABLE_ITEM_PATTERN, IID_TABLE_ITEM, TABLE_ITEM_OFFSET),
];

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
            invoke: &INVOKE_VTABLE,
            toggle: &TOGGLE_VTABLE,
            value: &VALUE_VTABLE,
            expand_collapse: &EXPAND_COLLAPSE_VTABLE,
            selection_item: &SELECTION_ITEM_VTABLE,
            grid: &GRID_VTABLE,
            grid_item: &GRID_ITEM_VTABLE,
            table: &TABLE_VTABLE,
            table_item: &TABLE_ITEM_VTABLE,
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

    fn node(&self) -> Result<AccessNode, HResult> {
        match self.lookup()? {
            Lookup::Root(_) => Err(UIA_E_ELEMENTNOTENABLED),
            Lookup::Node(_, node, _) => Ok(node),
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

    fn request(&self, action: AccessAction) -> HResult {
        let Element::Node(id) = self.element else {
            return UIA_E_ELEMENTNOTENABLED;
        };
        let window = {
            let Ok(mut tree) = self.tree.lock() else {
                return UIA_E_ELEMENTNOTAVAILABLE;
            };
            if tree.window == 0 || uia::exposed_node(&tree.nodes, id).is_none() {
                return UIA_E_ELEMENTNOTAVAILABLE;
            }
            let Some(request) = access_request(&tree.nodes, id, action) else {
                return UIA_E_ELEMENTNOTENABLED;
            };
            if tree.requests.len() < MAX_PENDING_REQUESTS {
                tree.requests.push(request);
            }
            tree.window
        };
        // SAFETY: `window` is the window this bridge serves (its address was exposed when it
        // was attached); posting an empty message to it only wakes its event loop, and a
        // window destroyed meanwhile makes the call fail harmlessly.
        unsafe { PostMessageW(std::ptr::with_exposed_provenance_mut(window), WM_NULL, 0, 0) };
        S_OK
    }

    fn query(this: *mut Provider, iid: *const Guid, out: Out) -> HResult {
        // SAFETY: COM passes a valid IID pointer or null; null is refused.
        let Some(&iid) = (unsafe { iid.as_ref() }) else {
            let _ = write_out(out, std::ptr::null_mut());
            return E_POINTER;
        };
        // SAFETY: `this` is a live provider, reached through one of its interfaces.
        let provider = unsafe { &*this };
        let is_root = provider.element == Element::Root;
        let offset = if iid == IID_IUNKNOWN || iid == IID_SIMPLE {
            Some(SIMPLE_OFFSET)
        } else if iid == IID_FRAGMENT {
            Some(FRAGMENT_OFFSET)
        } else if iid == IID_FRAGMENT_ROOT {
            is_root.then_some(ROOT_OFFSET)
        } else if is_root {
            None
        } else {
            PATTERN_INTERFACES
                .iter()
                .find(|(_, known, _)| *known == iid)
                .map(|&(_, _, offset)| offset)
        };
        let Some(offset) = offset else {
            let _ = write_out(out, std::ptr::null_mut());
            return E_NOINTERFACE;
        };
        provider.add_ref();
        write_out(out, Self::interface(this, offset))
    }
}

fn write<T>(out: *mut T, value: T) -> HResult {
    if out.is_null() {
        return E_POINTER;
    }
    // SAFETY: COM out parameters point to writable storage for one value of their type.
    unsafe { out.write(value) };
    S_OK
}

fn write_out(out: Out, value: *mut c_void) -> HResult {
    write(out, value)
}

fn provider_at(this: *mut c_void, offset: usize) -> *mut Provider {
    this.cast::<u8>().wrapping_sub(offset).cast()
}

fn provider_ref<'a>(this: *mut c_void, offset: usize) -> &'a Provider {
    // SAFETY: every interface pointer handed out points `offset` bytes into a live provider,
    // kept alive by the reference the caller holds for the call.
    unsafe { &*provider_at(this, offset) }
}

fn read_node<T>(
    this: *mut c_void,
    offset: usize,
    out: *mut T,
    read: impl FnOnce(&AccessNode) -> T,
) -> HResult {
    if out.is_null() {
        return E_POINTER;
    }
    match provider_ref(this, offset).node() {
        Err(error) => error,
        Ok(node) => write(out, read(&node)),
    }
}

fn provider_array(tree: &Arc<Mutex<AutomationTree>>, ids: &[u32], out: Out) -> HResult {
    if out.is_null() {
        return E_POINTER;
    }
    let Ok(length) = u32::try_from(ids.len()) else {
        return E_OUTOFMEMORY;
    };
    // SAFETY: the call takes its arguments by value and returns a new array or null.
    let array = unsafe { SafeArrayCreateVector(VT_UNKNOWN, 0, length) };
    if array.is_null() {
        return E_OUTOFMEMORY;
    }
    for (index, &id) in (0_i32..).zip(ids) {
        let provider = Provider::create(tree, Element::Node(id));
        let simple = Provider::interface(provider, SIMPLE_OFFSET);
        // SAFETY: `array` is the live `VT_UNKNOWN` vector created above and `index` is within
        // it; for `VT_UNKNOWN` the call takes the interface pointer itself and adds its own
        // reference.
        let stored = unsafe { SafeArrayPutElement(array, &index, simple) };
        // SAFETY: `provider` holds the one reference `create` gave this function.
        unsafe { Provider::release(provider) };
        if stored != S_OK {
            // SAFETY: `array` was created above and is not handed out; destroying it releases
            // the providers already stored.
            unsafe { SafeArrayDestroy(array) };
            return stored;
        }
    }
    write_out(out, array)
}

unsafe extern "system" fn forward_query_interface<const OFFSET: usize>(
    this: *mut c_void,
    iid: *const Guid,
    out: Out,
) -> HResult {
    Provider::query(provider_at(this, OFFSET), iid, out)
}

unsafe extern "system" fn forward_add_ref<const OFFSET: usize>(this: *mut c_void) -> u32 {
    provider_ref(this, OFFSET).add_ref()
}

unsafe extern "system" fn forward_release<const OFFSET: usize>(this: *mut c_void) -> u32 {
    // SAFETY: the caller gives up the reference it holds on this interface.
    unsafe { Provider::release(provider_at(this, OFFSET)) }
}

unsafe extern "system" fn provider_options(_this: *mut c_void, out: *mut i32) -> HResult {
    write(out, uia::SERVER_SIDE_PROVIDER)
}

unsafe extern "system" fn pattern_provider(this: *mut c_void, pattern: i32, out: Out) -> HResult {
    let provider = provider_ref(this, SIMPLE_OFFSET);
    let node = match provider.lookup() {
        Err(error) => {
            let _ = write_out(out, std::ptr::null_mut());
            return error;
        }
        Ok(Lookup::Root(_)) => return write_out(out, std::ptr::null_mut()),
        Ok(Lookup::Node(_, node, _)) => node,
    };
    let offset = PATTERN_INTERFACES
        .iter()
        .find(|&&(known, _, _)| known == pattern && uia::supports_pattern(&node, pattern))
        .map(|&(_, _, offset)| offset);
    let Some(offset) = offset else {
        return write_out(out, std::ptr::null_mut());
    };
    if out.is_null() {
        return E_POINTER;
    }
    provider.add_ref();
    write_out(
        out,
        Provider::interface(provider_at(this, SIMPLE_OFFSET), offset),
    )
}

fn node_property(node: &AccessNode, property: i32) -> Option<Variant> {
    let supports = |pattern| uia::supports_pattern(node, pattern);
    let cell = node.cell.filter(|_| supports(uia::GRID_ITEM_PATTERN));
    let table = node.table.filter(|_| supports(uia::GRID_PATTERN));
    Some(match property {
        uia::CONTROL_TYPE_PROPERTY => Variant::integer(uia::control_type(node.role)?),
        uia::NAME_PROPERTY => Variant::string(&node.name)?,
        uia::HAS_KEYBOARD_FOCUS_PROPERTY => Variant::boolean(node.state.focused),
        uia::IS_KEYBOARD_FOCUSABLE_PROPERTY => Variant::boolean(uia::is_keyboard_focusable(node)),
        uia::IS_ENABLED_PROPERTY => Variant::boolean(!node.state.disabled),
        uia::AUTOMATION_ID_PROPERTY => Variant::string(&node.id.to_string())?,
        uia::HELP_TEXT_PROPERTY => Variant::string(
            node.placeholder
                .as_deref()
                .or(node.description.as_deref())?,
        )?,
        uia::FULL_DESCRIPTION_PROPERTY => Variant::string(node.description.as_deref()?)?,
        uia::ACCELERATOR_KEY_PROPERTY => Variant::string(node.shortcut.as_deref()?)?,
        uia::IS_DATA_VALID_FOR_FORM_PROPERTY => Variant::boolean(!node.state.invalid),
        uia::HEADING_LEVEL_PROPERTY if node.role == AccessRole::Heading => {
            Variant::integer(uia::HEADING_LEVEL_1)
        }
        uia::LIVE_SETTING_PROPERTY if node.role == AccessRole::Status => {
            Variant::integer(uia::LIVE_SETTING_POLITE)
        }
        uia::LIVE_SETTING_PROPERTY if node.role == AccessRole::Alert => {
            Variant::integer(uia::LIVE_SETTING_ASSERTIVE)
        }
        uia::VALUE_VALUE_PROPERTY if supports(uia::VALUE_PATTERN) => {
            Variant::string(node.value.as_deref().unwrap_or_default())?
        }
        uia::VALUE_IS_READ_ONLY_PROPERTY if supports(uia::VALUE_PATTERN) => {
            Variant::boolean(uia::is_value_read_only(node))
        }
        uia::TOGGLE_STATE_PROPERTY if supports(uia::TOGGLE_PATTERN) => {
            Variant::integer(uia::toggle_state(node))
        }
        uia::EXPAND_COLLAPSE_STATE_PROPERTY if supports(uia::EXPAND_COLLAPSE_PATTERN) => {
            Variant::integer(uia::expand_collapse_state(node))
        }
        uia::SELECTION_ITEM_IS_SELECTED_PROPERTY if supports(uia::SELECTION_ITEM_PATTERN) => {
            Variant::boolean(node.state.selected)
        }
        uia::GRID_ROW_COUNT_PROPERTY => Variant::count(table?.rows),
        uia::GRID_COLUMN_COUNT_PROPERTY => Variant::count(table?.columns),
        uia::GRID_ITEM_ROW_PROPERTY => Variant::count(cell?.row),
        uia::GRID_ITEM_COLUMN_PROPERTY => Variant::count(cell?.column),
        uia::LEVEL_PROPERTY => Variant::count(node.tree_position?.level),
        uia::POSITION_IN_SET_PROPERTY => Variant::count(node.tree_position?.position),
        uia::SIZE_OF_SET_PROPERTY => Variant::count(node.tree_position?.set_size),
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
    write(out, variant)
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
    let provider = provider_ref(this, FRAGMENT_OFFSET);
    match provider.lookup() {
        Err(error) => error,
        Ok(Lookup::Root(_)) => S_OK,
        Ok(Lookup::Node(..)) => provider.request(AccessAction::Focus),
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

fn request_action(this: *mut c_void, offset: usize, action: AccessAction) -> HResult {
    provider_ref(this, offset).request(action)
}

unsafe extern "system" fn invoke(this: *mut c_void) -> HResult {
    request_action(this, INVOKE_OFFSET, AccessAction::Invoke)
}

unsafe extern "system" fn toggle(this: *mut c_void) -> HResult {
    request_action(this, TOGGLE_OFFSET, AccessAction::Toggle)
}

unsafe extern "system" fn get_toggle_state(this: *mut c_void, out: *mut i32) -> HResult {
    read_node(this, TOGGLE_OFFSET, out, uia::toggle_state)
}

unsafe extern "system" fn set_value(this: *mut c_void, text: *const u16) -> HResult {
    let Some(text) = read_wide_string(text, MAX_VALUE_UNITS) else {
        return E_INVALIDARG;
    };
    if text.chars().count() > MAX_ACCESS_VALUE_CHARS {
        return E_INVALIDARG;
    }
    request_action(this, VALUE_OFFSET, AccessAction::SetValue(text))
}

unsafe extern "system" fn get_value(this: *mut c_void, out: *mut *mut u16) -> HResult {
    if out.is_null() {
        return E_POINTER;
    }
    let node = match provider_ref(this, VALUE_OFFSET).node() {
        Err(error) => return error,
        Ok(node) => node,
    };
    let Some(string) = allocate_string(node.value.as_deref().unwrap_or_default()) else {
        return E_OUTOFMEMORY;
    };
    write(out, string)
}

unsafe extern "system" fn get_is_read_only(this: *mut c_void, out: *mut i32) -> HResult {
    read_node(this, VALUE_OFFSET, out, |node| {
        i32::from(uia::is_value_read_only(node))
    })
}

unsafe extern "system" fn expand(this: *mut c_void) -> HResult {
    request_action(this, EXPAND_COLLAPSE_OFFSET, AccessAction::Expand)
}

unsafe extern "system" fn collapse(this: *mut c_void) -> HResult {
    request_action(this, EXPAND_COLLAPSE_OFFSET, AccessAction::Collapse)
}

unsafe extern "system" fn get_expand_collapse_state(this: *mut c_void, out: *mut i32) -> HResult {
    read_node(
        this,
        EXPAND_COLLAPSE_OFFSET,
        out,
        uia::expand_collapse_state,
    )
}

unsafe extern "system" fn select(this: *mut c_void) -> HResult {
    request_action(this, SELECTION_ITEM_OFFSET, AccessAction::Select)
}

unsafe extern "system" fn add_to_selection(this: *mut c_void) -> HResult {
    request_action(this, SELECTION_ITEM_OFFSET, AccessAction::Select)
}

unsafe extern "system" fn remove_from_selection(this: *mut c_void) -> HResult {
    match provider_ref(this, SELECTION_ITEM_OFFSET).node() {
        Err(error) => error,
        Ok(_) => UIA_E_INVALIDOPERATION,
    }
}

unsafe extern "system" fn get_is_selected(this: *mut c_void, out: *mut i32) -> HResult {
    read_node(this, SELECTION_ITEM_OFFSET, out, |node| {
        i32::from(node.state.selected)
    })
}

fn related_out(
    this: *mut c_void,
    offset: usize,
    out: Out,
    related: impl FnOnce(&[AccessNode], u32) -> Option<u32>,
) -> HResult {
    let provider = provider_ref(this, offset);
    let Element::Node(id) = provider.element else {
        let _ = write_out(out, std::ptr::null_mut());
        return UIA_E_ELEMENTNOTENABLED;
    };
    match provider.with_nodes(|tree| related(&tree.nodes, id)) {
        Err(error) => {
            let _ = write_out(out, std::ptr::null_mut());
            error
        }
        Ok(found) => {
            Provider::create_out(&provider.tree, found.map(Element::Node), out, SIMPLE_OFFSET)
        }
    }
}

unsafe extern "system" fn get_selection_container(this: *mut c_void, out: Out) -> HResult {
    related_out(this, SELECTION_ITEM_OFFSET, out, uia::selection_container)
}

unsafe extern "system" fn get_item(this: *mut c_void, row: i32, column: i32, out: Out) -> HResult {
    let (Ok(row), Ok(column)) = (usize::try_from(row), usize::try_from(column)) else {
        let _ = write_out(out, std::ptr::null_mut());
        return E_INVALIDARG;
    };
    let provider = provider_ref(this, GRID_OFFSET);
    let Element::Node(grid) = provider.element else {
        let _ = write_out(out, std::ptr::null_mut());
        return UIA_E_ELEMENTNOTENABLED;
    };
    let found = provider.with_nodes(|tree| {
        let table = uia::exposed_node(&tree.nodes, grid)
            .and_then(|node| node.table)
            .unwrap_or_default();
        if row >= table.rows || column >= table.columns {
            return Err(E_INVALIDARG);
        }
        Ok(uia::grid_item(&tree.nodes, grid, row, column))
    });
    match found.and_then(|found| found) {
        Err(error) => {
            let _ = write_out(out, std::ptr::null_mut());
            error
        }
        Ok(cell) => {
            Provider::create_out(&provider.tree, cell.map(Element::Node), out, SIMPLE_OFFSET)
        }
    }
}

unsafe extern "system" fn get_row_count(this: *mut c_void, out: *mut i32) -> HResult {
    read_node(this, GRID_OFFSET, out, |node| {
        uia::saturating_int(node.table.unwrap_or_default().rows)
    })
}

unsafe extern "system" fn get_column_count(this: *mut c_void, out: *mut i32) -> HResult {
    read_node(this, GRID_OFFSET, out, |node| {
        uia::saturating_int(node.table.unwrap_or_default().columns)
    })
}

unsafe extern "system" fn get_row(this: *mut c_void, out: *mut i32) -> HResult {
    read_node(this, GRID_ITEM_OFFSET, out, |node| {
        uia::saturating_int(node.cell.unwrap_or_default().row)
    })
}

unsafe extern "system" fn get_column(this: *mut c_void, out: *mut i32) -> HResult {
    read_node(this, GRID_ITEM_OFFSET, out, |node| {
        uia::saturating_int(node.cell.unwrap_or_default().column)
    })
}

unsafe extern "system" fn get_span(this: *mut c_void, out: *mut i32) -> HResult {
    read_node(this, GRID_ITEM_OFFSET, out, |_| 1)
}

unsafe extern "system" fn get_containing_grid(this: *mut c_void, out: Out) -> HResult {
    related_out(this, GRID_ITEM_OFFSET, out, uia::containing_grid)
}

fn headers_out(
    this: *mut c_void,
    offset: usize,
    out: Out,
    headers: impl FnOnce(&[AccessNode], u32) -> Vec<u32>,
) -> HResult {
    let provider = provider_ref(this, offset);
    let Element::Node(id) = provider.element else {
        let _ = write_out(out, std::ptr::null_mut());
        return UIA_E_ELEMENTNOTENABLED;
    };
    match provider.with_nodes(|tree| headers(&tree.nodes, id)) {
        Err(error) => {
            let _ = write_out(out, std::ptr::null_mut());
            error
        }
        Ok(ids) => provider_array(&provider.tree, &ids, out),
    }
}

unsafe extern "system" fn empty_provider_array<const OFFSET: usize>(
    this: *mut c_void,
    out: Out,
) -> HResult {
    headers_out(this, OFFSET, out, |_, _| Vec::new())
}

unsafe extern "system" fn get_column_headers(this: *mut c_void, out: Out) -> HResult {
    headers_out(this, TABLE_OFFSET, out, |nodes, grid| {
        uia::column_headers(nodes, grid, None)
    })
}

unsafe extern "system" fn get_row_or_column_major(this: *mut c_void, out: *mut i32) -> HResult {
    read_node(this, TABLE_OFFSET, out, |_| uia::ROW_MAJOR)
}

unsafe extern "system" fn get_column_header_items(this: *mut c_void, out: Out) -> HResult {
    headers_out(this, TABLE_ITEM_OFFSET, out, |nodes, id| {
        let column = uia::exposed_node(nodes, id)
            .and_then(|node| node.cell)
            .map(|cell| cell.column);
        match (uia::containing_grid(nodes, id), column) {
            (Some(grid), Some(column)) => uia::column_headers(nodes, grid, Some(column)),
            _ => Vec::new(),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::super::super::{AccessActions, AccessCell, AccessRect, AccessState, AccessTable};
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
            ..AccessNode::default()
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
            tree.nodes[2].actions = AccessActions {
                invoke: true,
                focus: true,
                ..AccessActions::default()
            };
            let mut check_box = node(4, Some(1), AccessRole::CheckBox, "Read only", false);
            check_box.state.checked = Some(true);
            check_box.actions.toggle = true;
            let mut field = node(5, Some(1), AccessRole::TextField, "Host", false);
            field.value = Some("shop".to_owned());
            field.actions.set_value = true;
            let mut grid = node(6, Some(1), AccessRole::Grid, "Results", false);
            grid.table = Some(AccessTable {
                rows: 2,
                columns: 2,
            });
            let mut header = node(7, Some(6), AccessRole::ColumnHeader, "id", false);
            header.cell = Some(AccessCell { row: 0, column: 1 });
            let mut cell = node(8, Some(6), AccessRole::Cell, "42", false);
            cell.cell = Some(AccessCell { row: 1, column: 1 });
            cell.value = Some("42".to_owned());
            tree.nodes.extend([check_box, field, grid, header, cell]);
        }
        automation
    }

    fn simple_of(automation: &Automation, id: u32) -> *mut c_void {
        Provider::interface(
            Provider::create(&automation.tree, Element::Node(id)),
            SIMPLE_OFFSET,
        )
    }

    fn pattern(simple: *mut c_void, pattern: i32) -> *mut c_void {
        let mut out = std::ptr::null_mut();
        // SAFETY: `simple` is a live provider's simple interface and `out` is writable.
        assert_eq!(unsafe { pattern_provider(simple, pattern, &mut out) }, S_OK);
        out
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain([0]).collect()
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
        take_bstr(std::ptr::with_exposed_provenance_mut(variant.value[0]))
    }

    fn take_bstr(string: *mut u16) -> String {
        assert!(!string.is_null());
        // SAFETY: `string` is a BSTR the bridge allocated.
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

    #[test]
    fn a_button_invoked_by_a_screen_reader_queues_the_request() {
        let automation = automation();
        let simple = simple_of(&automation, 3);
        let invoke_pattern = pattern(simple, uia::INVOKE_PATTERN);
        assert!(!invoke_pattern.is_null());
        assert!(pattern(simple, uia::TOGGLE_PATTERN).is_null());
        let (result, queried) = query(simple, &IID_INVOKE);
        assert_eq!(result, S_OK);
        // SAFETY: `invoke_pattern` is the provider's `IInvokeProvider` interface.
        assert_eq!(unsafe { invoke(invoke_pattern) }, S_OK);
        let fragment = Provider::interface(provider_at(simple, SIMPLE_OFFSET), FRAGMENT_OFFSET);
        // SAFETY: `fragment` is the provider's fragment interface.
        assert_eq!(unsafe { set_focus(fragment) }, S_OK);
        assert_eq!(
            automation.take_requests(),
            [
                AccessRequest {
                    node: 3,
                    action: AccessAction::Invoke
                },
                AccessRequest {
                    node: 3,
                    action: AccessAction::Focus
                }
            ]
        );
        assert!(automation.take_requests().is_empty());
        for interface in [queried, invoke_pattern] {
            release(interface);
        }
        assert_eq!(release(simple), 0);
    }

    #[test]
    fn a_node_without_the_action_refuses_it() {
        let automation = automation();
        let heading = simple_of(&automation, 2);
        assert!(pattern(heading, uia::INVOKE_PATTERN).is_null());
        let invoke_pattern =
            Provider::interface(provider_at(heading, SIMPLE_OFFSET), INVOKE_OFFSET);
        // SAFETY: the heading's provider is alive and `invoke_pattern` points into it.
        assert_eq!(unsafe { invoke(invoke_pattern) }, UIA_E_ELEMENTNOTENABLED);
        let fragment = Provider::interface(provider_at(heading, SIMPLE_OFFSET), FRAGMENT_OFFSET);
        // SAFETY: as above, for the fragment interface.
        assert_eq!(unsafe { set_focus(fragment) }, UIA_E_ELEMENTNOTENABLED);
        let check_box = simple_of(&automation, 4);
        let value_pattern =
            Provider::interface(provider_at(check_box, SIMPLE_OFFSET), VALUE_OFFSET);
        let text = wide("x");
        // SAFETY: the checkbox's provider is alive and `text` is NUL-terminated.
        let result = unsafe { set_value(value_pattern, text.as_ptr()) };
        assert_eq!(result, UIA_E_ELEMENTNOTENABLED);
        let root = Provider::create(&automation.tree, Element::Root);
        let root_simple = Provider::interface(root, SIMPLE_OFFSET);
        assert!(pattern(root_simple, uia::INVOKE_PATTERN).is_null());
        let (result, none) = query(root_simple, &IID_INVOKE);
        assert_eq!((result, none), (E_NOINTERFACE, std::ptr::null_mut()));
        assert!(automation.take_requests().is_empty());
        for interface in [root_simple, check_box, heading] {
            assert_eq!(release(interface), 0);
        }
    }

    #[test]
    fn a_checkbox_reports_its_toggle_state_and_queues_a_toggle() {
        let automation = automation();
        let simple = simple_of(&automation, 4);
        let toggle_pattern = pattern(simple, uia::TOGGLE_PATTERN);
        assert!(!toggle_pattern.is_null());
        let mut state = -1;
        // SAFETY: `toggle_pattern` is the provider's `IToggleProvider`; `state` is writable.
        assert_eq!(
            unsafe { get_toggle_state(toggle_pattern, &mut state) },
            S_OK
        );
        assert_eq!(state, uia::TOGGLE_ON);
        let mut variant = Variant::empty();
        // SAFETY: `simple` is the provider's simple interface and `variant` is writable.
        let result = unsafe { property_value(simple, uia::TOGGLE_STATE_PROPERTY, &mut variant) };
        assert_eq!(result, S_OK);
        assert_eq!((variant.kind, variant.value[0]), (VT_I4, 1));
        // SAFETY: as above.
        assert_eq!(unsafe { toggle(toggle_pattern) }, S_OK);
        assert_eq!(
            automation.take_requests(),
            [AccessRequest {
                node: 4,
                action: AccessAction::Toggle
            }]
        );
        release(toggle_pattern);
        assert_eq!(release(simple), 0);
    }

    #[test]
    fn a_text_field_gives_its_value_and_queues_a_new_one() {
        let automation = automation();
        let simple = simple_of(&automation, 5);
        let value_pattern = pattern(simple, uia::VALUE_PATTERN);
        assert!(!value_pattern.is_null());
        let mut string = std::ptr::null_mut();
        // SAFETY: `value_pattern` is the provider's `IValueProvider`; `string` is writable.
        assert_eq!(unsafe { get_value(value_pattern, &mut string) }, S_OK);
        assert_eq!(take_bstr(string), "shop");
        let mut read_only = -1;
        // SAFETY: as above.
        assert_eq!(
            unsafe { get_is_read_only(value_pattern, &mut read_only) },
            S_OK
        );
        assert_eq!(read_only, 0);
        let text = wide("shop.eggdb");
        // SAFETY: as above; `text` is NUL-terminated.
        assert_eq!(unsafe { set_value(value_pattern, text.as_ptr()) }, S_OK);
        // SAFETY: a null string is refused before it is read.
        assert_eq!(
            unsafe { set_value(value_pattern, std::ptr::null()) },
            E_INVALIDARG
        );
        let unpaired = [0xD800, 0];
        // SAFETY: `unpaired` is NUL-terminated.
        assert_eq!(
            unsafe { set_value(value_pattern, unpaired.as_ptr()) },
            E_INVALIDARG
        );
        assert_eq!(
            automation.take_requests(),
            [AccessRequest {
                node: 5,
                action: AccessAction::SetValue("shop.eggdb".to_owned())
            }]
        );
        release(value_pattern);
        assert_eq!(release(simple), 0);
    }

    #[test]
    fn requests_past_the_bound_are_dropped() {
        let automation = automation();
        let simple = simple_of(&automation, 3);
        let invoke_pattern = pattern(simple, uia::INVOKE_PATTERN);
        for _ in 0..MAX_PENDING_REQUESTS + 5 {
            // SAFETY: `invoke_pattern` is the provider's `IInvokeProvider` interface.
            assert_eq!(unsafe { invoke(invoke_pattern) }, S_OK);
        }
        assert_eq!(automation.take_requests().len(), MAX_PENDING_REQUESTS);
        release(invoke_pattern);
        assert_eq!(release(simple), 0);
    }

    #[test]
    fn a_grid_finds_its_cells_and_a_cell_knows_its_place() {
        let automation = automation();
        let grid = simple_of(&automation, 6);
        let grid_pattern = pattern(grid, uia::GRID_PATTERN);
        assert!(!grid_pattern.is_null());
        let mut count = -1;
        // SAFETY: `grid_pattern` is the provider's `IGridProvider`; `count` is writable.
        assert_eq!(unsafe { get_row_count(grid_pattern, &mut count) }, S_OK);
        assert_eq!(count, 2);
        let mut found = std::ptr::null_mut();
        // SAFETY: as above; `found` is writable.
        assert_eq!(unsafe { get_item(grid_pattern, 1, 1, &mut found) }, S_OK);
        assert_eq!(provider_ref(found, SIMPLE_OFFSET).element, Element::Node(8));
        let mut missing = std::ptr::null_mut();
        // SAFETY: as above.
        assert_eq!(unsafe { get_item(grid_pattern, 0, 0, &mut missing) }, S_OK);
        assert!(missing.is_null());
        // SAFETY: as above.
        assert_eq!(
            unsafe { get_item(grid_pattern, 2, 0, &mut missing) },
            E_INVALIDARG
        );
        // SAFETY: as above.
        assert_eq!(
            unsafe { get_item(grid_pattern, -1, 0, &mut missing) },
            E_INVALIDARG
        );

        let item = pattern(found, uia::GRID_ITEM_PATTERN);
        assert!(!item.is_null());
        let (mut row, mut column) = (-1, -1);
        // SAFETY: `item` is the cell's `IGridItemProvider`; `row` and `column` are writable.
        assert_eq!(unsafe { get_row(item, &mut row) }, S_OK);
        // SAFETY: as above.
        assert_eq!(unsafe { get_column(item, &mut column) }, S_OK);
        assert_eq!((row, column), (1, 1));
        let mut containing = std::ptr::null_mut();
        // SAFETY: as above; `containing` is writable.
        assert_eq!(unsafe { get_containing_grid(item, &mut containing) }, S_OK);
        assert_eq!(
            provider_ref(containing, SIMPLE_OFFSET).element,
            Element::Node(6)
        );
        let mut variant = Variant::empty();
        // SAFETY: `found` is the cell's simple interface and `variant` is writable.
        let result = unsafe { property_value(found, uia::GRID_ITEM_COLUMN_PROPERTY, &mut variant) };
        assert_eq!(result, S_OK);
        assert_eq!((variant.kind, variant.value[0]), (VT_I4, 1));

        let table = pattern(grid, uia::TABLE_PATTERN);
        let mut headers = std::ptr::null_mut();
        // SAFETY: `table` is the grid's `ITableProvider`; `headers` is writable.
        assert_eq!(unsafe { get_column_headers(table, &mut headers) }, S_OK);
        assert!(!headers.is_null());
        // SAFETY: `headers` is the array the call returned and is not used after this.
        assert_eq!(unsafe { SafeArrayDestroy(headers) }, S_OK);
        let table_item = pattern(found, uia::TABLE_ITEM_PATTERN);
        let mut header_items = std::ptr::null_mut();
        // SAFETY: `table_item` is the cell's `ITableItemProvider`; `header_items` is writable.
        let result = unsafe { get_column_header_items(table_item, &mut header_items) };
        assert_eq!(result, S_OK);
        // SAFETY: as above.
        assert_eq!(unsafe { SafeArrayDestroy(header_items) }, S_OK);

        for interface in [table_item, table, containing, item, grid_pattern] {
            release(interface);
        }
        assert_eq!(release(found), 0);
        assert_eq!(release(grid), 0);
    }
}
