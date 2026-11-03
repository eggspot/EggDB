mod automation;

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use super::win32::{self, Utf16Characters};
use super::{
    AccessNode, KeyPress, MAX_CLIPBOARD_TEXT_BYTES, Modifiers, OpenFileDialog, PointerButton,
    Window, WindowError, WindowEvent, WindowOptions, check_clipboard_text, check_frame,
};
use automation::Automation;

type Handle = *mut c_void;
type WindowProcedure = unsafe extern "system" fn(Handle, u32, usize, isize) -> isize;

const CLASS_NAME: &str = "EggDBWindow";
const ERROR_CLASS_ALREADY_EXISTS: i32 = 1410;

const WS_OVERLAPPEDWINDOW: u32 = 0x00CF_0000;
const CW_USEDEFAULT: i32 = i32::MIN;
const CS_VREDRAW: u32 = 0x0001;
const CS_HREDRAW: u32 = 0x0002;
const SW_SHOW: i32 = 5;
const GWLP_USERDATA: i32 = -21;
const IDC_ARROW: usize = 32512;
const BLACK_BRUSH: i32 = 4;
const DIB_RGB_COLORS: u32 = 0;
const SRCCOPY: u32 = 0x00CC_0020;
const BI_RGB: u32 = 0;
const SWP_NOMOVE: u32 = 0x0002;
const SWP_NOZORDER: u32 = 0x0004;
const SWP_NOACTIVATE: u32 = 0x0010;
const PM_REMOVE: u32 = 0x0001;
const QS_ALLINPUT: u32 = 0x04FF;
const MWMO_INPUTAVAILABLE: u32 = 0x0004;
const INFINITE: u32 = u32::MAX;
const SIZE_MINIMIZED: usize = 1;
const MIN_TRACK_SIZE_POINT_IN_MINMAXINFO: usize = 3;
const DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2: isize = -4;

const WM_SIZE: u32 = 0x0005;
const WM_SETFOCUS: u32 = 0x0007;
const SPI_GETCLIENTAREAANIMATION: u32 = 0x1042;
const WM_KILLFOCUS: u32 = 0x0008;
const WM_PAINT: u32 = 0x000F;
const WM_CLOSE: u32 = 0x0010;
const WM_ERASEBKGND: u32 = 0x0014;
const WM_GETMINMAXINFO: u32 = 0x0024;
const WM_GETOBJECT: u32 = 0x003D;
const WM_NCCREATE: u32 = 0x0081;
const WM_KEYDOWN: u32 = 0x0100;
const WM_CHAR: u32 = 0x0102;
const WM_SYSKEYDOWN: u32 = 0x0104;
const WM_DPICHANGED: u32 = 0x02E0;
const WM_DROPFILES: u32 = 0x0233;
const WM_MOUSEMOVE: u32 = 0x0200;
const WM_LBUTTONDOWN: u32 = 0x0201;
const WM_LBUTTONUP: u32 = 0x0202;
const WM_RBUTTONDOWN: u32 = 0x0204;
const WM_RBUTTONUP: u32 = 0x0205;
const WM_MBUTTONDOWN: u32 = 0x0207;
const WM_MBUTTONUP: u32 = 0x0208;
const WM_MOUSEWHEEL: u32 = 0x020A;
const WM_MOUSEHWHEEL: u32 = 0x020E;
const WM_MOUSELEAVE: u32 = 0x02A3;
const TME_LEAVE: u32 = 0x0000_0002;
const CF_UNICODETEXT: u32 = 13;
const GMEM_MOVEABLE: u32 = 0x0002;
const CLIPBOARD_OPEN_ATTEMPTS: u32 = 10;
const CLIPBOARD_RETRY: Duration = Duration::from_millis(10);
const DRAG_QUERY_COUNT: u32 = u32::MAX;
const MOST_DROPPED_FILES: u32 = 1024;
const OFN_NOCHANGEDIR: u32 = 0x0000_0008;
const OFN_PATHMUSTEXIST: u32 = 0x0000_0800;
const OFN_FILEMUSTEXIST: u32 = 0x0000_1000;
const OFN_EXPLORER: u32 = 0x0008_0000;
const DIALOG_PATH_UNITS: usize = 32_768;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Point {
    x: i32,
    y: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct TrackMouseEventParams {
    size: u32,
    flags: u32,
    window: Handle,
    hover_time: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[repr(C)]
struct WindowClass {
    size: u32,
    style: u32,
    procedure: WindowProcedure,
    class_extra: i32,
    window_extra: i32,
    instance: Handle,
    icon: Handle,
    cursor: Handle,
    background: Handle,
    menu_name: *const u16,
    class_name: *const u16,
    small_icon: Handle,
}

#[repr(C)]
struct Message {
    window: Handle,
    message: u32,
    wparam: usize,
    lparam: isize,
    time: u32,
    point: Point,
}

#[repr(C)]
struct PaintStruct {
    hdc: Handle,
    erase: i32,
    paint: Rect,
    restore: i32,
    incremental_update: i32,
    reserved: [u8; 32],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct BitmapInfoHeader {
    size: u32,
    width: i32,
    height: i32,
    planes: u16,
    bit_count: u16,
    compression: u32,
    size_image: u32,
    x_pels_per_meter: i32,
    y_pels_per_meter: i32,
    colors_used: u32,
    colors_important: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct BitmapInfo {
    header: BitmapInfoHeader,
    colors: [u32; 1],
}

#[repr(C)]
struct OpenFileName {
    struct_size: u32,
    owner: Handle,
    instance: Handle,
    filter: *const u16,
    custom_filter: *mut u16,
    custom_filter_units: u32,
    filter_index: u32,
    file: *mut u16,
    file_units: u32,
    file_title: *mut u16,
    file_title_units: u32,
    initial_directory: *const u16,
    title: *const u16,
    flags: u32,
    file_offset: u16,
    file_extension: u16,
    default_extension: *const u16,
    custom_data: isize,
    hook: *const c_void,
    template_name: *const u16,
    reserved_pointer: *mut c_void,
    reserved_word: u32,
    flags_ex: u32,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GlobalAlloc(flags: u32, bytes: usize) -> Handle;
    fn GlobalLock(memory: Handle) -> *mut c_void;
    fn GlobalUnlock(memory: Handle) -> i32;
    fn GlobalFree(memory: Handle) -> Handle;
    fn GlobalSize(memory: Handle) -> usize;
    fn GetModuleHandleW(name: *const u16) -> Handle;
}

#[link(name = "comdlg32")]
unsafe extern "system" {
    fn GetOpenFileNameW(dialog: *mut OpenFileName) -> i32;
    fn CommDlgExtendedError() -> u32;
}

#[link(name = "shell32")]
unsafe extern "system" {
    fn DragAcceptFiles(window: Handle, accept: i32);
    fn DragQueryFileW(drop: Handle, index: u32, file: *mut u16, units: u32) -> u32;
    fn DragFinish(drop: Handle);
}

fn dropped_files(drop: Handle) -> Vec<PathBuf> {
    // SAFETY: `drop` is the `HDROP` of the `WM_DROPFILES` being handled; asking for the
    // count with a null buffer writes nothing.
    let count = unsafe { DragQueryFileW(drop, DRAG_QUERY_COUNT, std::ptr::null_mut(), 0) };
    let mut paths = Vec::new();
    for index in 0..count.min(MOST_DROPPED_FILES) {
        // SAFETY: as above; a null buffer asks for the path's length in UTF-16 units.
        let units = unsafe { DragQueryFileW(drop, index, std::ptr::null_mut(), 0) };
        let Some(capacity) = (units as usize).checked_add(1) else {
            continue;
        };
        if capacity > DIALOG_PATH_UNITS {
            continue;
        }
        let mut buffer = vec![0u16; capacity];
        // SAFETY: `buffer` holds `capacity` writable units, the path's length plus its NUL,
        // which is the size passed.
        unsafe { DragQueryFileW(drop, index, buffer.as_mut_ptr(), capacity as u32) };
        if let Some(path) = win32::path_from_wide_buffer(&buffer) {
            paths.push(path);
        }
    }
    // SAFETY: `drop` is released once, after the last query.
    unsafe { DragFinish(drop) };
    paths
}

#[link(name = "user32")]
unsafe extern "system" {
    fn SetCapture(window: Handle) -> Handle;
    fn ReleaseCapture() -> i32;
    fn TrackMouseEvent(track: *mut TrackMouseEventParams) -> i32;
    fn ScreenToClient(window: Handle, point: *mut Point) -> i32;
    fn OpenClipboard(owner: Handle) -> i32;
    fn SystemParametersInfoW(action: u32, parameter: u32, value: *mut i32, update: u32) -> i32;
    fn CloseClipboard() -> i32;
    fn EmptyClipboard() -> i32;
    fn SetClipboardData(format: u32, memory: Handle) -> Handle;
    fn GetClipboardData(format: u32) -> Handle;
    fn IsClipboardFormatAvailable(format: u32) -> i32;
    fn RegisterClassExW(class: *const WindowClass) -> u16;
    fn CreateWindowExW(
        ex_style: u32,
        class_name: *const u16,
        window_name: *const u16,
        style: u32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        parent: Handle,
        menu: Handle,
        instance: Handle,
        param: *mut c_void,
    ) -> Handle;
    fn DefWindowProcW(window: Handle, message: u32, wparam: usize, lparam: isize) -> isize;
    fn DestroyWindow(window: Handle) -> i32;
    fn ShowWindow(window: Handle, command: i32) -> i32;
    fn UpdateWindow(window: Handle) -> i32;
    fn InvalidateRect(window: Handle, rect: *const Rect, erase: i32) -> i32;
    fn GetClientRect(window: Handle, rect: *mut Rect) -> i32;
    fn SetWindowPos(
        window: Handle,
        insert_after: Handle,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        flags: u32,
    ) -> i32;
    fn PeekMessageW(
        message: *mut Message,
        window: Handle,
        filter_min: u32,
        filter_max: u32,
        remove: u32,
    ) -> i32;
    fn TranslateMessage(message: *const Message) -> i32;
    fn DispatchMessageW(message: *const Message) -> isize;
    fn MsgWaitForMultipleObjectsEx(
        count: u32,
        handles: *const Handle,
        milliseconds: u32,
        wake_mask: u32,
        flags: u32,
    ) -> u32;
    #[cfg_attr(target_pointer_width = "32", link_name = "SetWindowLongW")]
    fn SetWindowLongPtrW(window: Handle, index: i32, value: isize) -> isize;
    #[cfg_attr(target_pointer_width = "32", link_name = "GetWindowLongW")]
    fn GetWindowLongPtrW(window: Handle, index: i32) -> isize;
    fn GetKeyState(virtual_key: i32) -> i16;
    fn BeginPaint(window: Handle, paint: *mut PaintStruct) -> Handle;
    fn EndPaint(window: Handle, paint: *const PaintStruct) -> i32;
    fn FillRect(hdc: Handle, rect: *const Rect, brush: Handle) -> i32;
    fn LoadCursorW(instance: Handle, name: *const u16) -> Handle;
    fn GetDpiForWindow(window: Handle) -> u32;
    fn SetProcessDpiAwarenessContext(context: Handle) -> i32;
    fn AdjustWindowRectExForDpi(
        rect: *mut Rect,
        style: u32,
        menu: i32,
        ex_style: u32,
        dpi: u32,
    ) -> i32;
}

#[link(name = "gdi32")]
unsafe extern "system" {
    fn StretchDIBits(
        hdc: Handle,
        x_dest: i32,
        y_dest: i32,
        dest_width: i32,
        dest_height: i32,
        x_source: i32,
        y_source: i32,
        source_width: i32,
        source_height: i32,
        bits: *const c_void,
        info: *const BitmapInfo,
        usage: u32,
        raster_operation: u32,
    ) -> i32;
    fn GetStockObject(object: i32) -> Handle;
}

fn last_error(call: &str) -> WindowError {
    WindowError::Io(format!(
        "{call} failed: {}",
        std::io::Error::last_os_error()
    ))
}

#[derive(Debug, Default)]
struct Frame {
    width: i32,
    height: i32,
    bgra: Vec<u8>,
}

#[derive(Debug)]
struct WindowState {
    events: RefCell<VecDeque<WindowEvent>>,
    frame: RefCell<Frame>,
    characters: Cell<Utf16Characters>,
    size: Cell<(u32, u32)>,
    dpi: Cell<u32>,
    min_size: (u32, u32),
    automation: Automation,
    tracking_leave: Cell<bool>,
}

impl WindowState {
    fn push(&self, event: WindowEvent) {
        if let Ok(mut events) = self.events.try_borrow_mut() {
            events.push_back(event);
        }
    }

    fn pop(&self) -> Option<WindowEvent> {
        self.events.try_borrow_mut().ok()?.pop_front()
    }

    fn scale(&self) -> f32 {
        win32::scale_from_dpi(self.dpi.get())
    }
}

fn track_leave(window: Handle, state: &WindowState) {
    if state.tracking_leave.get() {
        return;
    }
    let mut track = TrackMouseEventParams {
        size: std::mem::size_of::<TrackMouseEventParams>() as u32,
        flags: TME_LEAVE,
        window,
        hover_time: 0,
    };
    // SAFETY: `track` is a filled `TRACKMOUSEEVENT` with its size, live for the call; the
    // system copies it.
    let tracking = unsafe { TrackMouseEvent(&mut track) } != 0;
    state.tracking_leave.set(tracking);
}

fn pointer_button(message: u32) -> Option<(PointerButton, bool)> {
    Some(match message {
        WM_LBUTTONDOWN => (PointerButton::Primary, true),
        WM_LBUTTONUP => (PointerButton::Primary, false),
        WM_RBUTTONDOWN => (PointerButton::Secondary, true),
        WM_RBUTTONUP => (PointerButton::Secondary, false),
        WM_MBUTTONDOWN => (PointerButton::Middle, true),
        WM_MBUTTONUP => (PointerButton::Middle, false),
        _ => return None,
    })
}

fn pointer_message(
    window: Handle,
    state: &WindowState,
    message: u32,
    wparam: usize,
    lparam: isize,
) {
    let (x, y) = win32::point_from_lparam(lparam);
    let (x, y) = (x as f32, y as f32);
    match message {
        WM_MOUSEMOVE => {
            track_leave(window, state);
            state.push(WindowEvent::PointerMoved { x, y });
        }
        WM_MOUSELEAVE => {
            state.tracking_leave.set(false);
            state.push(WindowEvent::PointerLeft);
        }
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
            let (screen_x, screen_y) = win32::point_from_lparam(lparam);
            let mut point = Point {
                x: screen_x,
                y: screen_y,
            };
            // SAFETY: `window` is ours and `point` a writable `POINT` for the call.
            unsafe { ScreenToClient(window, &mut point) };
            let notches = win32::wheel_notches(wparam);
            let (lines_down, lines_right) = if message == WM_MOUSEWHEEL {
                (-notches, 0.0)
            } else {
                (0.0, notches)
            };
            state.push(WindowEvent::Wheel {
                x: point.x as f32,
                y: point.y as f32,
                lines_down,
                lines_right,
                modifiers: held_modifiers(),
            });
        }
        _ => {
            let Some((button, down)) = pointer_button(message) else {
                return;
            };
            let modifiers = held_modifiers();
            if down {
                // SAFETY: `window` is ours; capturing keeps the release coming to it when the
                // pointer leaves the window with the button held.
                unsafe { SetCapture(window) };
                state.push(WindowEvent::PointerPressed {
                    x,
                    y,
                    button,
                    modifiers,
                });
            } else {
                // SAFETY: the call takes no arguments and only ends this thread's capture.
                unsafe { ReleaseCapture() };
                state.push(WindowEvent::PointerReleased {
                    x,
                    y,
                    button,
                    modifiers,
                });
            }
        }
    }
}

struct OpenedClipboard;

impl OpenedClipboard {
    fn open(owner: Handle) -> Result<Self, WindowError> {
        for _ in 0..CLIPBOARD_OPEN_ATTEMPTS {
            // SAFETY: `owner` is this live window, passed by value; another program holding
            // the clipboard makes the call fail, and it is tried again.
            if unsafe { OpenClipboard(owner) } != 0 {
                return Ok(Self);
            }
            std::thread::sleep(CLIPBOARD_RETRY);
        }
        Err(last_error("OpenClipboard"))
    }
}

impl Drop for OpenedClipboard {
    fn drop(&mut self) {
        // SAFETY: this thread opened the clipboard in `OpenedClipboard::open`.
        unsafe { CloseClipboard() };
    }
}

fn put_clipboard_units(owner: Handle, units: &[u16]) -> Result<(), WindowError> {
    let _open = OpenedClipboard::open(owner)?;
    // SAFETY: the clipboard is open on this thread.
    if unsafe { EmptyClipboard() } == 0 {
        return Err(last_error("EmptyClipboard"));
    }
    let bytes = std::mem::size_of_val(units);
    // SAFETY: the call takes flags and a size by value and returns a new block or null.
    let memory = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) };
    if memory.is_null() {
        return Err(last_error("GlobalAlloc"));
    }
    // SAFETY: `memory` is the movable block just allocated.
    let target = unsafe { GlobalLock(memory) }.cast::<u16>();
    if target.is_null() {
        // SAFETY: the block is still ours: the clipboard has not taken it.
        unsafe { GlobalFree(memory) };
        return Err(last_error("GlobalLock"));
    }
    // SAFETY: `target` points at the locked block of `bytes` bytes, aligned for `u16`
    // (global blocks are 8-byte aligned), which `units` does not overlap.
    unsafe { std::ptr::copy_nonoverlapping(units.as_ptr(), target, units.len()) };
    // SAFETY: the block was locked once above.
    unsafe { GlobalUnlock(memory) };
    // SAFETY: the clipboard is open and emptied by this thread; on success the system owns
    // `memory`.
    if unsafe { SetClipboardData(CF_UNICODETEXT, memory) }.is_null() {
        let error = last_error("SetClipboardData");
        // SAFETY: the clipboard refused the block, so it is still ours.
        unsafe { GlobalFree(memory) };
        return Err(error);
    }
    Ok(())
}

fn clipboard_units(owner: Handle) -> Result<Option<Vec<u16>>, WindowError> {
    let _open = OpenedClipboard::open(owner)?;
    // SAFETY: the call takes a format by value.
    if unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT) } == 0 {
        return Ok(None);
    }
    // SAFETY: the clipboard is open on this thread; the block stays the clipboard's.
    let memory = unsafe { GetClipboardData(CF_UNICODETEXT) };
    if memory.is_null() {
        return Ok(None);
    }
    // SAFETY: `memory` is the clipboard's live block while the clipboard is open.
    let size = unsafe { GlobalSize(memory) };
    // SAFETY: as above; locking only pins it for reading.
    let source = unsafe { GlobalLock(memory) }.cast::<u16>().cast_const();
    if source.is_null() {
        return Ok(None);
    }
    let count = (size / 2).min(MAX_CLIPBOARD_TEXT_BYTES);
    // SAFETY: `source` points at the locked block of `size` bytes, aligned for `u16`, and
    // `count` units fit in it; the slice is copied before the block is unlocked.
    let units = unsafe { std::slice::from_raw_parts(source, count) }.to_vec();
    // SAFETY: the block was locked once above.
    unsafe { GlobalUnlock(memory) };
    Ok(Some(units))
}

fn held_modifiers() -> Modifiers {
    // SAFETY: `GetKeyState` takes a virtual-key code by value and only reads the thread's
    // keyboard state.
    let shift = unsafe { GetKeyState(win32::VK_SHIFT) };
    // SAFETY: as above.
    let control = unsafe { GetKeyState(win32::VK_CONTROL) };
    // SAFETY: as above.
    let alt = unsafe { GetKeyState(win32::VK_MENU) };
    win32::modifiers_from_key_states(shift, control, alt)
}

fn outer_size(width: i32, height: i32, dpi: u32) -> (i32, i32) {
    let mut rect = Rect {
        left: 0,
        top: 0,
        right: width,
        bottom: height,
    };
    // SAFETY: `rect` is a live, writable `RECT` for the duration of the call.
    let adjusted = unsafe { AdjustWindowRectExForDpi(&mut rect, WS_OVERLAPPEDWINDOW, 0, 0, dpi) };
    if adjusted == 0 {
        return (width, height);
    }
    (
        rect.right.saturating_sub(rect.left),
        rect.bottom.saturating_sub(rect.top),
    )
}

fn client_size(window: Handle) -> (u32, u32) {
    let mut rect = Rect::default();
    // SAFETY: `window` is a window this backend created and `rect` is writable.
    let got = unsafe { GetClientRect(window, &mut rect) };
    if got == 0 {
        return (0, 0);
    }
    let length = |from: i32, to: i32| u32::try_from(to.saturating_sub(from)).unwrap_or(0);
    (length(rect.left, rect.right), length(rect.top, rect.bottom))
}

fn state_of<'a>(window: Handle) -> Option<&'a WindowState> {
    // SAFETY: `GetWindowLongPtrW` reads the window's user data slot, which this backend sets
    // to a `WindowState` pointer at `WM_NCCREATE` and clears before the state is freed.
    let pointer = unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) } as *const WindowState;
    // SAFETY: a non-null pointer in the slot points to the `WindowState` held by the
    // `Win32Window` that owns `window`, which outlives every message dispatched to it.
    unsafe { pointer.as_ref() }
}

fn paint(window: Handle, state: &WindowState) {
    let mut paint = PaintStruct {
        hdc: std::ptr::null_mut(),
        erase: 0,
        paint: Rect::default(),
        restore: 0,
        incremental_update: 0,
        reserved: [0; 32],
    };
    // SAFETY: `window` is ours and `paint` is a writable `PAINTSTRUCT`.
    let hdc = unsafe { BeginPaint(window, &mut paint) };
    if hdc.is_null() {
        return;
    }
    let (client_width, client_height) = client_size(window);
    let client_width = i32::try_from(client_width).unwrap_or(i32::MAX);
    let client_height = i32::try_from(client_height).unwrap_or(i32::MAX);
    let drawn = match state.frame.try_borrow() {
        Ok(frame) if !frame.bgra.is_empty() => {
            draw_frame(hdc, &frame);
            Some((frame.width, frame.height))
        }
        _ => None,
    };
    // SAFETY: `GetStockObject` returns a shared system brush that is never freed.
    let black = unsafe { GetStockObject(BLACK_BRUSH) };
    let uncovered = match drawn {
        Some((width, height)) => [
            Rect {
                left: width,
                top: 0,
                right: client_width,
                bottom: client_height,
            },
            Rect {
                left: 0,
                top: height,
                right: width.min(client_width),
                bottom: client_height,
            },
        ],
        None => [
            Rect {
                left: 0,
                top: 0,
                right: client_width,
                bottom: client_height,
            },
            Rect::default(),
        ],
    };
    for rect in uncovered {
        if rect.right > rect.left && rect.bottom > rect.top {
            // SAFETY: `hdc` is the painting context `BeginPaint` returned and `rect` is a
            // live `RECT`.
            unsafe { FillRect(hdc, &rect, black) };
        }
    }
    // SAFETY: `paint` is the structure `BeginPaint` filled for `window`.
    unsafe { EndPaint(window, &paint) };
    if drawn.is_none() {
        state.push(WindowEvent::Redraw);
    }
}

fn draw_frame(hdc: Handle, frame: &Frame) {
    let info = BitmapInfo {
        header: BitmapInfoHeader {
            size: std::mem::size_of::<BitmapInfoHeader>() as u32,
            width: frame.width,
            height: frame.height.saturating_neg(),
            planes: 1,
            bit_count: 32,
            compression: BI_RGB,
            size_image: 0,
            x_pels_per_meter: 0,
            y_pels_per_meter: 0,
            colors_used: 0,
            colors_important: 0,
        },
        colors: [0],
    };
    // SAFETY: `frame.bgra` holds `width` × `height` pixels of four bytes (checked by
    // `present`), which is what the top-down 32-bit `BI_RGB` header in `info` describes, and
    // both stay alive for the call.
    unsafe {
        StretchDIBits(
            hdc,
            0,
            0,
            frame.width,
            frame.height,
            0,
            0,
            frame.width,
            frame.height,
            frame.bgra.as_ptr().cast(),
            &info,
            DIB_RGB_COLORS,
            SRCCOPY,
        )
    };
}

fn handle_message(
    window: Handle,
    state: &WindowState,
    message: u32,
    wparam: usize,
    lparam: isize,
) -> Option<isize> {
    match message {
        WM_PAINT => paint(window, state),
        WM_ERASEBKGND => return Some(1),
        WM_GETOBJECT => return state.automation.answer_get_object(window, wparam, lparam),
        WM_CLOSE => state.push(WindowEvent::CloseRequested),
        WM_DROPFILES => {
            let paths = dropped_files(std::ptr::without_provenance_mut(wparam));
            if !paths.is_empty() {
                state.push(WindowEvent::FilesDropped(paths));
            }
        }
        WM_MOUSEMOVE | WM_MOUSELEAVE | WM_MOUSEWHEEL | WM_MOUSEHWHEEL | WM_LBUTTONDOWN
        | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP | WM_MBUTTONDOWN | WM_MBUTTONUP => {
            pointer_message(window, state, message, wparam, lparam);
        }
        WM_SETFOCUS => state.push(WindowEvent::Focused(true)),
        WM_KILLFOCUS => state.push(WindowEvent::Focused(false)),
        WM_SIZE => {
            let size = win32::client_size_from_lparam(lparam);
            if wparam != SIZE_MINIMIZED && size != state.size.get() {
                state.size.set(size);
                state.push(WindowEvent::Resized {
                    width: size.0,
                    height: size.1,
                });
            }
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            let modifiers = held_modifiers();
            if let Some(key) = win32::key_from_virtual_key(wparam, modifiers) {
                state.push(WindowEvent::Key(KeyPress { key, modifiers }));
            }
            if message == WM_SYSKEYDOWN {
                return None;
            }
        }
        WM_CHAR => {
            let mut characters = state.characters.get();
            let character = characters.push(win32::low_word(wparam));
            state.characters.set(characters);
            if let Some(press) =
                character.and_then(|c| win32::key_press_from_character(c, held_modifiers()))
            {
                state.push(WindowEvent::Key(press));
            }
        }
        WM_GETMINMAXINFO => {
            let scale = state.scale();
            let (width, height) = outer_size(
                win32::device_pixels(state.min_size.0, scale),
                win32::device_pixels(state.min_size.1, scale),
                state.dpi.get(),
            );
            let min_track_size =
                (lparam as *mut Point).wrapping_add(MIN_TRACK_SIZE_POINT_IN_MINMAXINFO);
            // SAFETY: for `WM_GETMINMAXINFO` the system passes a writable `MINMAXINFO` in
            // `lparam` for the duration of the message, and its `ptMinTrackSize` is the
            // `POINT` at this index.
            if let Some(point) = unsafe { min_track_size.as_mut() } {
                *point = Point {
                    x: width,
                    y: height,
                };
            }
        }
        WM_DPICHANGED => {
            let dpi = u32::from(win32::low_word(wparam));
            if dpi != state.dpi.get() {
                state.dpi.set(dpi);
                state.push(WindowEvent::ScaleChanged(win32::scale_from_dpi(dpi)));
            }
            let suggested = lparam as *const Rect;
            // SAFETY: for `WM_DPICHANGED` the system passes the suggested window `RECT` in
            // `lparam` for the duration of the message.
            if let Some(rect) = unsafe { suggested.as_ref() } {
                let rect = *rect;
                // SAFETY: `window` is ours; the call only moves and resizes it.
                unsafe {
                    SetWindowPos(
                        window,
                        std::ptr::null_mut(),
                        rect.left,
                        rect.top,
                        rect.right.saturating_sub(rect.left),
                        rect.bottom.saturating_sub(rect.top),
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    )
                };
            }
        }
        _ => return None,
    }
    Some(0)
}

unsafe extern "system" fn window_procedure(
    window: Handle,
    message: u32,
    wparam: usize,
    lparam: isize,
) -> isize {
    if message == WM_NCCREATE {
        let create_params = lparam as *const *mut c_void;
        // SAFETY: for `WM_NCCREATE` the system passes the `CREATESTRUCTW` built from
        // `CreateWindowExW`'s arguments in `lparam`, whose first field is the creation
        // parameter.
        if let Some(&state) = unsafe { create_params.as_ref() } {
            // SAFETY: `window` is the window being created; the slot receives the
            // `WindowState` pointer `open` passed as the creation parameter.
            unsafe { SetWindowLongPtrW(window, GWLP_USERDATA, state as isize) };
        }
    } else if let Some(state) = state_of(window)
        && let Some(result) = handle_message(window, state, message, wparam, lparam)
    {
        return result;
    }
    // SAFETY: the default procedure accepts every message with the parameters the system
    // passed to this one.
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}

fn register_class(instance: Handle, class_name: &[u16]) -> Result<(), WindowError> {
    // SAFETY: `IDC_ARROW` is a predefined cursor id, passed as an integer resource name with
    // no instance, as `LoadCursorW` documents.
    let cursor = unsafe {
        LoadCursorW(
            std::ptr::null_mut(),
            std::ptr::without_provenance(IDC_ARROW),
        )
    };
    let class = WindowClass {
        size: std::mem::size_of::<WindowClass>() as u32,
        style: CS_HREDRAW | CS_VREDRAW,
        procedure: window_procedure,
        class_extra: 0,
        window_extra: 0,
        instance,
        icon: std::ptr::null_mut(),
        cursor,
        background: std::ptr::null_mut(),
        menu_name: std::ptr::null(),
        class_name: class_name.as_ptr(),
        small_icon: std::ptr::null_mut(),
    };
    // SAFETY: `class` is a complete `WNDCLASSEXW` whose class name is NUL-terminated and
    // alive for the call; the system copies what it keeps.
    let atom = unsafe { RegisterClassExW(&class) };
    if atom == 0
        && std::io::Error::last_os_error().raw_os_error() != Some(ERROR_CLASS_ALREADY_EXISTS)
    {
        return Err(last_error("RegisterClassExW"));
    }
    Ok(())
}

#[derive(Debug)]
pub(crate) struct Win32Window {
    window: Handle,
    state: Rc<WindowState>,
}

impl Win32Window {
    pub(crate) fn open(options: &WindowOptions) -> Result<Self, WindowError> {
        // SAFETY: the call takes a pseudo-handle by value; it fails harmlessly when the
        // process's DPI awareness was already set (by a manifest or an earlier window).
        unsafe {
            SetProcessDpiAwarenessContext(std::ptr::without_provenance_mut(
                DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2 as usize,
            ))
        };
        // SAFETY: a null name asks for the running executable's module handle.
        let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
        let class_name = win32::wide_string(CLASS_NAME);
        register_class(instance, &class_name)?;
        let title = win32::wide_string(&options.title);
        let state = Rc::new(WindowState {
            events: RefCell::new(VecDeque::new()),
            frame: RefCell::new(Frame::default()),
            characters: Cell::new(Utf16Characters::default()),
            size: Cell::new((0, 0)),
            dpi: Cell::new(win32::BASE_DPI),
            min_size: (options.min_width, options.min_height),
            automation: Automation::default(),
            tracking_leave: Cell::new(false),
        });
        // SAFETY: the class is registered, both strings are NUL-terminated and alive for the
        // call, and the creation parameter points to `state`, which this `Win32Window` keeps
        // alive (at a fixed address) until after the window is destroyed.
        let window = unsafe {
            CreateWindowExW(
                0,
                class_name.as_ptr(),
                title.as_ptr(),
                WS_OVERLAPPEDWINDOW,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                instance,
                Rc::as_ptr(&state).cast_mut().cast(),
            )
        };
        if window.is_null() {
            return Err(last_error("CreateWindowExW"));
        }
        let this = Self { window, state };
        this.state.automation.attach(this.window);
        // SAFETY: `window` was just created by this backend; the call only lets it receive
        // `WM_DROPFILES`.
        unsafe { DragAcceptFiles(this.window, 1) };
        // SAFETY: `window` was just created by this backend.
        let dpi = unsafe { GetDpiForWindow(this.window) };
        this.state
            .dpi
            .set(if dpi == 0 { win32::BASE_DPI } else { dpi });
        let scale = this.state.scale();
        let (width, height) = outer_size(
            win32::device_pixels(options.width, scale),
            win32::device_pixels(options.height, scale),
            this.state.dpi.get(),
        );
        // SAFETY: `window` is ours; the call only resizes it.
        unsafe {
            SetWindowPos(
                this.window,
                std::ptr::null_mut(),
                0,
                0,
                width,
                height,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            )
        };
        // SAFETY: `window` is ours.
        unsafe { ShowWindow(this.window, SW_SHOW) };
        // SAFETY: `window` is ours; this paints it once before the first frame arrives.
        unsafe { UpdateWindow(this.window) };
        this.state.size.set(client_size(this.window));
        while this.state.pop().is_some() {}
        Ok(this)
    }

    fn pump(&self) {
        let mut message = Message {
            window: std::ptr::null_mut(),
            message: 0,
            wparam: 0,
            lparam: 0,
            time: 0,
            point: Point::default(),
        };
        loop {
            // SAFETY: `message` is a writable `MSG`; a null window reads every message of
            // this thread.
            let got = unsafe { PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_REMOVE) };
            if got == 0 {
                return;
            }
            // SAFETY: `message` was just filled by `PeekMessageW`.
            unsafe { TranslateMessage(&message) };
            // SAFETY: as above; dispatching runs `window_procedure`, which only reaches the
            // state through shared references.
            unsafe { DispatchMessageW(&message) };
        }
    }
}

impl Drop for Win32Window {
    fn drop(&mut self) {
        self.state.automation.detach(self.window);
        // SAFETY: `window` is ours; clearing the slot stops the procedure from reaching the
        // state, which is freed after this.
        unsafe { SetWindowLongPtrW(self.window, GWLP_USERDATA, 0) };
        // SAFETY: `window` is ours and destroyed once, here.
        unsafe { DestroyWindow(self.window) };
    }
}

impl Window for Win32Window {
    fn size(&self) -> (u32, u32) {
        self.state.size.get()
    }

    fn scale(&self) -> f32 {
        self.state.scale()
    }

    fn present(&mut self, width: u32, height: u32, rgba: &[u8]) -> Result<(), WindowError> {
        check_frame(width, height, rgba)?;
        let (Ok(frame_width), Ok(frame_height)) = (i32::try_from(width), i32::try_from(height))
        else {
            return Err(WindowError::InvalidInput(format!(
                "a {width} × {height} frame is larger than a Win32 bitmap can take"
            )));
        };
        {
            let Ok(mut frame) = self.state.frame.try_borrow_mut() else {
                return Err(WindowError::Protocol(
                    "a frame was presented while the window was painting".to_owned(),
                ));
            };
            frame.width = frame_width;
            frame.height = frame_height;
            win32::bgra_from_rgba(rgba, &mut frame.bgra);
        }
        // SAFETY: `window` is ours; a null rectangle invalidates the whole client area.
        unsafe { InvalidateRect(self.window, std::ptr::null(), 0) };
        // SAFETY: `window` is ours; this sends `WM_PAINT` now, which draws the stored frame.
        unsafe { UpdateWindow(self.window) };
        Ok(())
    }

    fn reduced_motion(&mut self) -> bool {
        let mut animates: i32 = 1;
        // SAFETY: `SPI_GETCLIENTAREAANIMATION` writes one `BOOL` to the pointer, which is a
        // live `i32` of this frame.
        let read = unsafe {
            SystemParametersInfoW(SPI_GETCLIENTAREAANIMATION, 0, &raw mut animates, 0)
        };
        read != 0 && animates == 0
    }

    fn set_accessibility_tree(&mut self, nodes: &[AccessNode]) {
        self.state.automation.update(nodes, self.state.scale());
    }

    fn next_event(
        &mut self,
        timeout: Option<Duration>,
    ) -> Result<Option<WindowEvent>, WindowError> {
        let deadline = timeout.map(|wait| Instant::now() + wait);
        loop {
            if let Some(event) = self.state.pop() {
                return Ok(Some(event));
            }
            self.pump();
            for request in self.state.automation.take_requests() {
                self.state.push(WindowEvent::AccessRequested(request));
            }
            if let Some(event) = self.state.pop() {
                return Ok(Some(event));
            }
            let milliseconds = match deadline {
                None => INFINITE,
                Some(deadline) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Ok(None);
                    }
                    u32::try_from(left.as_millis().max(1)).unwrap_or(INFINITE - 1)
                }
            };
            // SAFETY: no handles are passed; the call only waits for this thread's input.
            unsafe {
                MsgWaitForMultipleObjectsEx(
                    0,
                    std::ptr::null(),
                    milliseconds,
                    QS_ALLINPUT,
                    MWMO_INPUTAVAILABLE,
                )
            };
        }
    }

    fn choose_file_to_open(
        &mut self,
        dialog: &OpenFileDialog,
    ) -> Result<Option<PathBuf>, WindowError> {
        let filter = win32::open_file_filter(&dialog.filters);
        let initial = dialog
            .start_in
            .as_ref()
            .map(|directory| win32::wide_string(&directory.to_string_lossy()));
        let mut file = vec![0u16; DIALOG_PATH_UNITS];
        let mut request = OpenFileName {
            struct_size: std::mem::size_of::<OpenFileName>() as u32,
            owner: self.window,
            instance: std::ptr::null_mut(),
            filter: filter.as_ptr(),
            custom_filter: std::ptr::null_mut(),
            custom_filter_units: 0,
            filter_index: 1,
            file: file.as_mut_ptr(),
            file_units: DIALOG_PATH_UNITS as u32,
            file_title: std::ptr::null_mut(),
            file_title_units: 0,
            initial_directory: initial
                .as_ref()
                .map_or(std::ptr::null(), |wide| wide.as_ptr()),
            title: std::ptr::null(),
            flags: OFN_EXPLORER | OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR,
            file_offset: 0,
            file_extension: 0,
            default_extension: std::ptr::null(),
            custom_data: 0,
            hook: std::ptr::null(),
            template_name: std::ptr::null(),
            reserved_pointer: std::ptr::null_mut(),
            reserved_word: 0,
            flags_ex: 0,
        };
        // SAFETY: `request` is a complete `OPENFILENAMEW` of its own size; the filter, the
        // initial directory and the file buffer it points to stay alive for the call, and the
        // buffer holds `file_units` writable units. The dialog runs its own message loop,
        // which reaches this window's procedure through shared references only.
        let chosen = unsafe { GetOpenFileNameW(&mut request) };
        if chosen != 0 {
            return Ok(win32::path_from_wide_buffer(&file));
        }
        // SAFETY: the call has no parameters and reads the thread's last dialog error.
        let error = unsafe { CommDlgExtendedError() };
        if error == 0 {
            Ok(None)
        } else {
            Err(WindowError::Io(format!(
                "the open-file dialog failed with error {error:#06x}"
            )))
        }
    }

    fn put_clipboard_text(&mut self, text: &str) -> Result<(), WindowError> {
        check_clipboard_text(text)?;
        put_clipboard_units(self.window, &win32::clipboard_units(text))
    }

    fn clipboard_text(&mut self) -> Result<Option<String>, WindowError> {
        let Some(units) = clipboard_units(self.window)? else {
            return Ok(None);
        };
        let text = win32::text_from_clipboard_units(&units);
        check_clipboard_text(&text)?;
        Ok(Some(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> WindowOptions {
        WindowOptions {
            title: "EggDB window test".to_owned(),
            width: 320,
            height: 200,
            min_width: 100,
            min_height: 80,
        }
    }

    #[test]
    fn a_window_opens_takes_frames_and_answers_without_blocking() {
        let Ok(mut window) = Win32Window::open(&options()) else {
            return;
        };
        let (width, height) = window.size();
        assert!(width > 0 && height > 0);
        assert!(window.scale() >= 1.0);
        let frame = vec![0x40; width as usize * height as usize * 4];
        assert_eq!(window.present(width, height, &frame), Ok(()));
        assert!(window.present(width, height, &frame[1..]).is_err());
        for _ in 0..8 {
            let event = window.next_event(Some(Duration::from_millis(5)));
            assert!(event.is_ok());
            if event == Ok(None) {
                break;
            }
        }
        assert_eq!(window.next_event(Some(Duration::ZERO)).map(|_| ()), Ok(()));
    }

    #[test]
    fn two_windows_share_the_class_and_close_independently() {
        let (Ok(first), Ok(second)) =
            (Win32Window::open(&options()), Win32Window::open(&options()))
        else {
            return;
        };
        assert_ne!(first.window, second.window);
        drop(first);
        assert!(second.size().0 > 0);
    }
}
