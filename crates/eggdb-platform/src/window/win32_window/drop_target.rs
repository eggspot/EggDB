use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::path::PathBuf;
use std::rc::Rc;

use super::super::win32::{DROPEFFECT_NONE, FileDragTarget};
use super::{Handle, Point, WindowState, dropped_paths};

type HResult = i32;
type Out = *mut *mut c_void;

const S_OK: HResult = 0;
const E_NOINTERFACE: HResult = 0x8000_4002_u32 as i32;
const E_POINTER: HResult = 0x8000_4003_u32 as i32;
const E_UNEXPECTED: HResult = 0x8000_FFFF_u32 as i32;
const CF_HDROP: u16 = 15;
const DVASPECT_CONTENT: u32 = 1;
const ALL_ITEMS: i32 = -1;
const TYMED_HGLOBAL: u32 = 1;

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
struct Guid {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

const COM_BASE: [u8; 8] = [0xC0, 0, 0, 0, 0, 0, 0, 0x46];
const IID_IUNKNOWN: Guid = Guid {
    data1: 0,
    data2: 0,
    data3: 0,
    data4: COM_BASE,
};
const IID_IDROPTARGET: Guid = Guid {
    data1: 0x0000_0122,
    data2: 0,
    data3: 0,
    data4: COM_BASE,
};

#[repr(C)]
struct FormatEtc {
    format: u16,
    target_device: *mut c_void,
    aspect: u32,
    index: i32,
    tymed: u32,
}

#[repr(C)]
struct StorageMedium {
    tymed: u32,
    handle: Handle,
    release_owner: *mut c_void,
}

type GetData = unsafe extern "system" fn(*mut c_void, *const FormatEtc, *mut StorageMedium) -> HResult;

#[repr(C)]
struct DataObjectVtable {
    unknown: [*const c_void; 3],
    get_data: GetData,
}

#[repr(C)]
struct DataObject {
    vtable: *const DataObjectVtable,
}

#[repr(C)]
struct DropTargetVtable {
    query_interface: unsafe extern "system" fn(*mut c_void, *const Guid, Out) -> HResult,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    drag_enter: unsafe extern "system" fn(*mut c_void, *mut c_void, u32, Point, *mut u32) -> HResult,
    drag_over: unsafe extern "system" fn(*mut c_void, u32, Point, *mut u32) -> HResult,
    drag_leave: unsafe extern "system" fn(*mut c_void) -> HResult,
    drop: unsafe extern "system" fn(*mut c_void, *mut c_void, u32, Point, *mut u32) -> HResult,
}

static DROP_TARGET_VTABLE: DropTargetVtable = DropTargetVtable {
    query_interface,
    add_ref,
    release,
    drag_enter,
    drag_over,
    drag_leave,
    drop: drop_files,
};

#[link(name = "ole32")]
unsafe extern "system" {
    fn OleInitialize(reserved: *mut c_void) -> HResult;
    fn OleUninitialize();
    fn RegisterDragDrop(window: Handle, target: *mut c_void) -> HResult;
    fn RevokeDragDrop(window: Handle) -> HResult;
    fn ReleaseStgMedium(medium: *mut StorageMedium);
}

#[repr(C)]
struct DropTarget {
    vtable: &'static DropTargetVtable,
    references: Cell<u32>,
    drag: RefCell<FileDragTarget>,
    state: Rc<WindowState>,
}

impl DropTarget {
    fn at<'a>(this: *mut c_void) -> &'a DropTarget {
        // SAFETY: OLE calls the methods with the pointer `RegisterDragDrop` was given, which
        // points to a live `DropTarget` while OLE holds the reference it took.
        unsafe { &*this.cast::<DropTarget>() }
    }

    unsafe fn release(this: *mut DropTarget) -> u32 {
        // SAFETY: the caller holds a reference, so the target is alive.
        let target = unsafe { &*this };
        let left = target.references.get().saturating_sub(1);
        target.references.set(left);
        if left == 0 {
            // SAFETY: the last reference is gone, so nothing reaches the box `register`
            // leaked any more.
            drop(unsafe { Box::from_raw(this) });
        }
        left
    }

    fn answer(&self, effect: *mut u32, update: impl FnOnce(&mut FileDragTarget, u32) -> u32) -> HResult {
        let Some(allowed) = read_effect(effect) else {
            return E_POINTER;
        };
        let Ok(mut drag) = self.drag.try_borrow_mut() else {
            return E_UNEXPECTED;
        };
        let chosen = update(&mut drag, allowed);
        write_effect(effect, chosen)
    }
}

fn read_effect(effect: *mut u32) -> Option<u32> {
    if effect.is_null() {
        return None;
    }
    // SAFETY: OLE passes `pdwEffect` as a valid in/out `DWORD` for the call.
    Some(unsafe { effect.read() })
}

fn write_effect(effect: *mut u32, value: u32) -> HResult {
    if effect.is_null() {
        return E_POINTER;
    }
    // SAFETY: as in `read_effect`.
    unsafe { effect.write(value) };
    S_OK
}

fn data_paths(data: *mut c_void) -> Vec<PathBuf> {
    if data.is_null() {
        return Vec::new();
    }
    let format = FormatEtc {
        format: CF_HDROP,
        target_device: std::ptr::null_mut(),
        aspect: DVASPECT_CONTENT,
        index: ALL_ITEMS,
        tymed: TYMED_HGLOBAL,
    };
    let mut medium = StorageMedium {
        tymed: 0,
        handle: std::ptr::null_mut(),
        release_owner: std::ptr::null_mut(),
    };
    // SAFETY: `data` is the `IDataObject` OLE passes for this call, alive for it; a COM
    // object's first field is its vtable pointer.
    let vtable = unsafe { (*data.cast::<DataObject>()).vtable };
    if vtable.is_null() {
        return Vec::new();
    }
    // SAFETY: an `IDataObject` vtable starts with `IUnknown`'s three methods, then `GetData`.
    let get_data = unsafe { (*vtable).get_data };
    // SAFETY: `format` and `medium` are live for the call; `GetData` fills `medium` only when
    // it succeeds.
    let got = unsafe { get_data(data, &format, &mut medium) };
    if got < 0 {
        return Vec::new();
    }
    let paths = if medium.tymed == TYMED_HGLOBAL && !medium.handle.is_null() {
        dropped_paths(medium.handle)
    } else {
        Vec::new()
    };
    // SAFETY: `medium` was filled by the successful `GetData` above and is released once.
    unsafe { ReleaseStgMedium(&mut medium) };
    paths
}

unsafe extern "system" fn query_interface(this: *mut c_void, iid: *const Guid, out: Out) -> HResult {
    if out.is_null() {
        return E_POINTER;
    }
    // SAFETY: COM passes a valid IID pointer or null; null is refused.
    let known = unsafe { iid.as_ref() }.is_some_and(|&iid| iid == IID_IUNKNOWN || iid == IID_IDROPTARGET);
    let interface = if known {
        let target = DropTarget::at(this);
        target.references.set(target.references.get().saturating_add(1));
        this
    } else {
        std::ptr::null_mut()
    };
    // SAFETY: `out` is COM's non-null out parameter for one interface pointer.
    unsafe { out.write(interface) };
    if known { S_OK } else { E_NOINTERFACE }
}

unsafe extern "system" fn add_ref(this: *mut c_void) -> u32 {
    let target = DropTarget::at(this);
    let count = target.references.get().saturating_add(1);
    target.references.set(count);
    count
}

unsafe extern "system" fn release(this: *mut c_void) -> u32 {
    // SAFETY: the caller gives up the reference it holds.
    unsafe { DropTarget::release(this.cast()) }
}

unsafe extern "system" fn drag_enter(
    this: *mut c_void,
    data: *mut c_void,
    _keys: u32,
    _point: Point,
    effect: *mut u32,
) -> HResult {
    let target = DropTarget::at(this);
    let paths = data_paths(data);
    let mut event = None;
    let answer = target.answer(effect, |drag, allowed| {
        let (chosen, entered) = drag.enter(paths, allowed);
        event = entered;
        chosen
    });
    if let Some(event) = event {
        target.state.push(event);
    }
    answer
}

unsafe extern "system" fn drag_over(
    this: *mut c_void,
    _keys: u32,
    _point: Point,
    effect: *mut u32,
) -> HResult {
    DropTarget::at(this).answer(effect, |drag, allowed| drag.over(allowed))
}

unsafe extern "system" fn drag_leave(this: *mut c_void) -> HResult {
    let target = DropTarget::at(this);
    let left = target.drag.try_borrow_mut().ok().and_then(|mut drag| drag.leave());
    if let Some(event) = left {
        target.state.push(event);
    }
    S_OK
}

unsafe extern "system" fn drop_files(
    this: *mut c_void,
    data: *mut c_void,
    _keys: u32,
    _point: Point,
    effect: *mut u32,
) -> HResult {
    let target = DropTarget::at(this);
    let paths = data_paths(data);
    let mut event = None;
    let answer = target.answer(effect, |drag, allowed| {
        let (chosen, dropped) = drag.drop(paths, allowed);
        event = dropped;
        chosen
    });
    if let Some(event) = event {
        target.state.push(event);
    }
    if answer != S_OK {
        let _ = write_effect(effect, DROPEFFECT_NONE);
    }
    answer
}

/// The window's OLE drop target, registered with `RegisterDragDrop` after `OleInitialize`:
/// `DragEnter` and `DragLeave` give the drag-over ring its events, which `WM_DROPFILES` never
/// could, and `Drop` gives the dropped files. Dropping it revokes the target and balances
/// `OleInitialize`.
#[derive(Debug)]
pub(super) struct DropRegistration {
    window: Handle,
    target: *mut DropTarget,
}

impl DropRegistration {
    /// Registers a drop target for `window` that queues its events on `state`, or `None` when
    /// OLE cannot be initialised on this thread (another apartment model) or refuses it, so the
    /// window keeps `WM_DROPFILES`.
    pub(super) fn register(window: Handle, state: &Rc<WindowState>) -> Option<Self> {
        // SAFETY: the reserved parameter must be null; the call initialises OLE for this
        // thread or fails when the thread already chose another apartment model.
        let initialized = unsafe { OleInitialize(std::ptr::null_mut()) };
        if initialized < 0 {
            return None;
        }
        let target = Box::into_raw(Box::new(DropTarget {
            vtable: &DROP_TARGET_VTABLE,
            references: Cell::new(1),
            drag: RefCell::new(FileDragTarget::default()),
            state: Rc::clone(state),
        }));
        // SAFETY: `window` was created by this backend on this thread, and `target` points to
        // a live object laid out as an `IDropTarget` (its vtable first); OLE takes its own
        // reference when it accepts it.
        let registered = unsafe { RegisterDragDrop(window, target.cast()) };
        if registered < 0 {
            // SAFETY: this gives up the one reference `Box::into_raw` made, which frees it.
            unsafe { DropTarget::release(target) };
            // SAFETY: balances the successful `OleInitialize` above.
            unsafe { OleUninitialize() };
            return None;
        }
        Some(Self { window, target })
    }
}

impl Drop for DropRegistration {
    fn drop(&mut self) {
        // SAFETY: `window` is still alive (the window drops this before `DestroyWindow`); the
        // call releases OLE's reference to the target.
        unsafe { RevokeDragDrop(self.window) };
        // SAFETY: this gives up the reference `register` kept.
        unsafe { DropTarget::release(self.target) };
        // SAFETY: balances the `OleInitialize` that `register` made.
        unsafe { OleUninitialize() };
    }
}
