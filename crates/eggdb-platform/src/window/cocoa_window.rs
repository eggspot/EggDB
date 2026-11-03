use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::ffi::{CStr, c_char, c_int, c_void};
use std::fmt;
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use super::cocoa::{self, CgPoint, CgRect, CgSize};
use super::{
    AccessNode, KeyPress, Modifiers, OpenFileDialog, PointerButton, Window, WindowError,
    WindowEvent, WindowOptions, check_clipboard_text, check_frame,
};

mod accessibility;

use accessibility::AccessElements;

type Id = *mut c_void;

const PASTEBOARD_TYPE_STRING: &str = "public.utf8-plain-text";
type Sel = *const c_void;

#[cfg(target_arch = "x86_64")]
type ObjcBool = i8;
#[cfg(target_arch = "x86_64")]
const YES: ObjcBool = 1;
#[cfg(target_arch = "x86_64")]
const NO: ObjcBool = 0;
#[cfg(target_arch = "x86_64")]
const BOOL_GETTER_TYPES: &CStr = c"c@:";
#[cfg(target_arch = "x86_64")]
const BOOL_DELEGATE_TYPES: &CStr = c"c@:@";
#[cfg(target_arch = "x86_64")]
const BOOL_SETTER_TYPES: &CStr = c"v@:c";
#[cfg(target_arch = "x86_64")]
const BOOL_SELECTOR_TYPES: &CStr = c"c@::";
#[cfg(target_arch = "x86_64")]
const RECT_SENDER: &CStr = c"objc_msgSend_stret";

#[cfg(not(target_arch = "x86_64"))]
type ObjcBool = bool;
#[cfg(not(target_arch = "x86_64"))]
const YES: ObjcBool = true;
#[cfg(not(target_arch = "x86_64"))]
const NO: ObjcBool = false;
#[cfg(not(target_arch = "x86_64"))]
const BOOL_GETTER_TYPES: &CStr = c"B@:";
#[cfg(not(target_arch = "x86_64"))]
const BOOL_DELEGATE_TYPES: &CStr = c"B@:@";
#[cfg(not(target_arch = "x86_64"))]
const BOOL_SETTER_TYPES: &CStr = c"v@:B";
#[cfg(not(target_arch = "x86_64"))]
const BOOL_SELECTOR_TYPES: &CStr = c"B@::";
#[cfg(not(target_arch = "x86_64"))]
const RECT_SENDER: &CStr = c"objc_msgSend";

const VIEW_CLASS: &CStr = c"EggDBView";
const VOID_OBJECT_TYPES: &CStr = c"v@:@";
const OBJECT_GETTER_TYPES: &CStr = c"@@:";
const DRAW_RECT_TYPES: &CStr = c"v@:{CGRect={CGPoint=dd}{CGSize=dd}}";
const DRAG_OPERATION_TYPES: &CStr = c"Q@:@";
const FILE_URL_PASTEBOARD_TYPE: &str = "public.file-url";
const DRAG_OPERATION_NONE: u64 = 0;
const DRAG_OPERATION_COPY: u64 = 1;
const MODAL_RESPONSE_OK: u64 = 1;
const MOST_DROPPED_FILES: u64 = 1024;

const RTLD_LAZY: c_int = 0x1;
const RTLD_LOCAL: c_int = 0x4;
const LIBOBJC: &CStr = c"/usr/lib/libobjc.A.dylib";
const APPKIT: &CStr = c"/System/Library/Frameworks/AppKit.framework/AppKit";
const FOUNDATION: &CStr = c"/System/Library/Frameworks/Foundation.framework/Foundation";
const CORE_GRAPHICS: &CStr = c"/System/Library/Frameworks/CoreGraphics.framework/CoreGraphics";
const CORE_FOUNDATION: &CStr =
    c"/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation";

const STYLE_TITLED_CLOSABLE_MINIATURIZABLE_RESIZABLE: u64 = 0b1111;
const BACKING_STORE_BUFFERED: u64 = 2;
const ACTIVATION_POLICY_REGULAR: isize = 0;
const EVENT_MASK_ANY: u64 = u64::MAX;
const UTF8_STRING_ENCODING: u64 = 4;
const ALPHA_NONE_SKIP_LAST: u32 = 5;
const RENDERING_INTENT_DEFAULT: i32 = 0;
const INTERPOLATION_NONE: i32 = 1;

type GetClass = unsafe extern "C" fn(*const c_char) -> Id;
type RegisterSelector = unsafe extern "C" fn(*const c_char) -> Sel;
type AllocateClassPair = unsafe extern "C" fn(Id, *const c_char, usize) -> Id;
type RegisterClassPair = unsafe extern "C" fn(Id);
type AddMethod = unsafe extern "C" fn(Id, Sel, *const c_void, *const c_char) -> ObjcBool;
type PoolPush = unsafe extern "C" fn() -> *mut c_void;
type PoolPop = unsafe extern "C" fn(*mut c_void);
type SendId = unsafe extern "C" fn(Id, Sel) -> Id;
type SendVoid = unsafe extern "C" fn(Id, Sel);
type SendF64 = unsafe extern "C" fn(Id, Sel) -> f64;
type SendU16 = unsafe extern "C" fn(Id, Sel) -> u16;
type SendU64 = unsafe extern "C" fn(Id, Sel) -> u64;
type SendText = unsafe extern "C" fn(Id, Sel) -> *const c_char;
type SendRect = unsafe extern "C" fn(Id, Sel) -> CgRect;
type SendVoidId = unsafe extern "C" fn(Id, Sel, Id);
type SendVoidInteger = unsafe extern "C" fn(Id, Sel, isize);
type SendVoidBool = unsafe extern "C" fn(Id, Sel, ObjcBool);
type SendVoidSize = unsafe extern "C" fn(Id, Sel, CgSize);
type SendVoidRect = unsafe extern "C" fn(Id, Sel, CgRect);
type SendIdInteger = unsafe extern "C" fn(Id, Sel, isize) -> Id;
type SendIdObjects = unsafe extern "C" fn(Id, Sel, *const Id, usize) -> Id;
type SendIdIdId = unsafe extern "C" fn(Id, Sel, Id, Id) -> Id;
type SendIdIdBool = unsafe extern "C" fn(Id, Sel, Id, ObjcBool) -> Id;
type SendBoolId = unsafe extern "C" fn(Id, Sel, Id) -> ObjcBool;
type PostNotification = unsafe extern "C" fn(Id, Id);
type SendIdF64 = unsafe extern "C" fn(Id, Sel, f64) -> Id;
type SendPoint = unsafe extern "C" fn(Id, Sel) -> CgPoint;
type SendIsize = unsafe extern "C" fn(Id, Sel) -> isize;
type SendBool = unsafe extern "C" fn(Id, Sel) -> ObjcBool;
type SendIdId = unsafe extern "C" fn(Id, Sel, Id) -> Id;
type SendBoolIdId = unsafe extern "C" fn(Id, Sel, Id, Id) -> ObjcBool;
type SendInitTracking = unsafe extern "C" fn(Id, Sel, CgRect, u64, Id, Id) -> Id;
type SendIdRect = unsafe extern "C" fn(Id, Sel, CgRect) -> Id;
type SendNextEvent = unsafe extern "C" fn(Id, Sel, u64, Id, Id, ObjcBool) -> Id;
type SendInitWindow = unsafe extern "C" fn(Id, Sel, CgRect, u64, u64, ObjcBool) -> Id;
type SendInitString = unsafe extern "C" fn(Id, Sel, *const u8, usize, u64) -> Id;
type CreateObject = unsafe extern "C" fn() -> Id;
type ReleaseObject = unsafe extern "C" fn(Id);
type DataCreate = unsafe extern "C" fn(Id, *const u8, isize) -> Id;
type DataProviderCreate = unsafe extern "C" fn(Id) -> Id;
type ImageCreate = unsafe extern "C" fn(
    usize,
    usize,
    usize,
    usize,
    usize,
    Id,
    u32,
    Id,
    *const f64,
    bool,
    i32,
) -> Id;
type DrawImage = unsafe extern "C" fn(Id, CgRect, Id);
type SetFillColor = unsafe extern "C" fn(Id, f64, f64, f64, f64);
type FillRect = unsafe extern "C" fn(Id, CgRect);
type SetInterpolation = unsafe extern "C" fn(Id, i32);

unsafe extern "C" {
    fn dlopen(path: *const c_char, mode: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn dlerror() -> *const c_char;
    fn pthread_main_np() -> c_int;
}

#[derive(Clone, Copy)]
struct Library(*mut c_void);

fn loader_error() -> String {
    // SAFETY: `dlerror` takes no arguments and returns null or a NUL-terminated message.
    let message = unsafe { dlerror() };
    if message.is_null() {
        return "unknown error".to_owned();
    }
    // SAFETY: a non-null `dlerror` result is a NUL-terminated string that stays valid until
    // the next loader call on this thread.
    unsafe { CStr::from_ptr(message) }
        .to_string_lossy()
        .into_owned()
}

fn open_library(path: &CStr) -> Result<Library, WindowError> {
    // SAFETY: `path` is NUL-terminated; loading a system library runs only its own
    // initializers.
    let handle = unsafe { dlopen(path.as_ptr(), RTLD_LAZY | RTLD_LOCAL) };
    if handle.is_null() {
        return Err(WindowError::NoDisplay(format!(
            "{} could not be loaded: {}",
            path.to_string_lossy(),
            loader_error()
        )));
    }
    Ok(Library(handle))
}

fn symbol(library: Library, name: &CStr) -> Result<*mut c_void, WindowError> {
    // SAFETY: `library` is a handle `dlopen` returned and `name` is NUL-terminated.
    let address = unsafe { dlsym(library.0, name.as_ptr()) };
    if address.is_null() {
        return Err(WindowError::Protocol(format!(
            "{} is missing from the system libraries",
            name.to_string_lossy()
        )));
    }
    Ok(address)
}

macro_rules! load {
    ($library:expr, $name:expr, $prototype:ty) => {{
        let address = symbol($library, $name)?;
        // SAFETY: `address` is the non-null address `dlsym` found for this symbol in a system
        // library, and the prototype is its C declaration from Apple's headers (for
        // `objc_msgSend`, the declaration of the methods it is called for).
        unsafe { std::mem::transmute::<*mut c_void, $prototype>(address) }
    }};
}

struct Runtime {
    get_class: GetClass,
    register_selector: RegisterSelector,
    allocate_class_pair: AllocateClassPair,
    register_class_pair: RegisterClassPair,
    add_method: AddMethod,
    pool_push: PoolPush,
    pool_pop: PoolPop,
    send_id: SendId,
    send_void: SendVoid,
    send_f64: SendF64,
    send_u16: SendU16,
    send_u64: SendU64,
    send_text: SendText,
    send_rect: SendRect,
    send_void_id: SendVoidId,
    send_void_integer: SendVoidInteger,
    send_void_bool: SendVoidBool,
    send_void_size: SendVoidSize,
    send_void_rect: SendVoidRect,
    send_id_integer: SendIdInteger,
    send_id_objects: SendIdObjects,
    send_id_id_id: SendIdIdId,
    send_id_id_bool: SendIdIdBool,
    send_bool_id: SendBoolId,
    post_notification: PostNotification,
    send_id_f64: SendIdF64,
    send_point: SendPoint,
    send_isize: SendIsize,
    send_bool: SendBool,
    send_id_id: SendIdId,
    send_bool_id_id: SendBoolIdId,
    send_init_tracking: SendInitTracking,
    send_id_rect: SendIdRect,
    send_next_event: SendNextEvent,
    send_init_window: SendInitWindow,
    send_init_string: SendInitString,
    color_space_create: CreateObject,
    color_space_release: ReleaseObject,
    data_create: DataCreate,
    cf_release: ReleaseObject,
    data_provider_create: DataProviderCreate,
    data_provider_release: ReleaseObject,
    image_create: ImageCreate,
    image_release: ReleaseObject,
    draw_image: DrawImage,
    set_fill_color: SetFillColor,
    fill_rect: FillRect,
    set_interpolation: SetInterpolation,
    default_mode: Id,
}

impl fmt::Debug for Runtime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Runtime").finish_non_exhaustive()
    }
}

impl Runtime {
    fn load() -> Result<Self, WindowError> {
        let objc = open_library(LIBOBJC)?;
        let appkit = open_library(APPKIT)?;
        let foundation = open_library(FOUNDATION)?;
        let graphics = open_library(CORE_GRAPHICS)?;
        let core = open_library(CORE_FOUNDATION)?;
        let default_mode_address = symbol(foundation, c"NSDefaultRunLoopMode")?.cast::<Id>();
        // SAFETY: `NSDefaultRunLoopMode` is a constant `NSString *` that Foundation sets when
        // it loads, and the address is aligned for a pointer.
        let default_mode = unsafe { default_mode_address.read() };
        Ok(Self {
            get_class: load!(objc, c"objc_getClass", GetClass),
            register_selector: load!(objc, c"sel_registerName", RegisterSelector),
            allocate_class_pair: load!(objc, c"objc_allocateClassPair", AllocateClassPair),
            register_class_pair: load!(objc, c"objc_registerClassPair", RegisterClassPair),
            add_method: load!(objc, c"class_addMethod", AddMethod),
            pool_push: load!(objc, c"objc_autoreleasePoolPush", PoolPush),
            pool_pop: load!(objc, c"objc_autoreleasePoolPop", PoolPop),
            send_id: load!(objc, c"objc_msgSend", SendId),
            send_void: load!(objc, c"objc_msgSend", SendVoid),
            send_f64: load!(objc, c"objc_msgSend", SendF64),
            send_u16: load!(objc, c"objc_msgSend", SendU16),
            send_u64: load!(objc, c"objc_msgSend", SendU64),
            send_text: load!(objc, c"objc_msgSend", SendText),
            send_rect: load!(objc, RECT_SENDER, SendRect),
            send_void_id: load!(objc, c"objc_msgSend", SendVoidId),
            send_void_integer: load!(objc, c"objc_msgSend", SendVoidInteger),
            send_void_bool: load!(objc, c"objc_msgSend", SendVoidBool),
            send_void_size: load!(objc, c"objc_msgSend", SendVoidSize),
            send_void_rect: load!(objc, c"objc_msgSend", SendVoidRect),
            send_id_integer: load!(objc, c"objc_msgSend", SendIdInteger),
            send_id_objects: load!(objc, c"objc_msgSend", SendIdObjects),
            send_id_id_id: load!(objc, c"objc_msgSend", SendIdIdId),
            send_id_id_bool: load!(objc, c"objc_msgSend", SendIdIdBool),
            send_bool_id: load!(objc, c"objc_msgSend", SendBoolId),
            post_notification: load!(appkit, c"NSAccessibilityPostNotification", PostNotification),
            send_id_f64: load!(objc, c"objc_msgSend", SendIdF64),
            send_point: load!(objc, c"objc_msgSend", SendPoint),
            send_isize: load!(objc, c"objc_msgSend", SendIsize),
            send_bool: load!(objc, c"objc_msgSend", SendBool),
            send_id_id: load!(objc, c"objc_msgSend", SendIdId),
            send_bool_id_id: load!(objc, c"objc_msgSend", SendBoolIdId),
            send_init_tracking: load!(objc, c"objc_msgSend", SendInitTracking),
            send_id_rect: load!(objc, c"objc_msgSend", SendIdRect),
            send_next_event: load!(objc, c"objc_msgSend", SendNextEvent),
            send_init_window: load!(objc, c"objc_msgSend", SendInitWindow),
            send_init_string: load!(objc, c"objc_msgSend", SendInitString),
            color_space_create: load!(graphics, c"CGColorSpaceCreateDeviceRGB", CreateObject),
            color_space_release: load!(graphics, c"CGColorSpaceRelease", ReleaseObject),
            data_create: load!(core, c"CFDataCreate", DataCreate),
            cf_release: load!(core, c"CFRelease", ReleaseObject),
            data_provider_create: load!(
                graphics,
                c"CGDataProviderCreateWithCFData",
                DataProviderCreate
            ),
            data_provider_release: load!(graphics, c"CGDataProviderRelease", ReleaseObject),
            image_create: load!(graphics, c"CGImageCreate", ImageCreate),
            image_release: load!(graphics, c"CGImageRelease", ReleaseObject),
            draw_image: load!(graphics, c"CGContextDrawImage", DrawImage),
            set_fill_color: load!(graphics, c"CGContextSetRGBFillColor", SetFillColor),
            fill_rect: load!(graphics, c"CGContextFillRect", FillRect),
            set_interpolation: load!(
                graphics,
                c"CGContextSetInterpolationQuality",
                SetInterpolation
            ),
            default_mode,
        })
    }

    fn selector(&self, name: &CStr) -> Sel {
        // SAFETY: `name` is NUL-terminated; registering a selector only interns the name.
        unsafe { (self.register_selector)(name.as_ptr()) }
    }

    fn class(&self, name: &CStr) -> Result<Id, WindowError> {
        // SAFETY: `name` is NUL-terminated; the lookup only reads the runtime's class table.
        let class = unsafe { (self.get_class)(name.as_ptr()) };
        if class.is_null() {
            return Err(WindowError::Protocol(format!(
                "the Objective-C class {} is missing",
                name.to_string_lossy()
            )));
        }
        Ok(class)
    }

    fn with_pool<T>(&self, work: impl FnOnce() -> T) -> T {
        // SAFETY: pushing an autorelease pool has no preconditions.
        let pool = unsafe { (self.pool_push)() };
        let result = work();
        // SAFETY: `pool` is the token pushed above, popped once and in order.
        unsafe { (self.pool_pop)(pool) };
        result
    }

    fn string(&self, text: &str) -> Result<Id, WindowError> {
        let class = self.class(c"NSString")?;
        // SAFETY: `alloc` on a class returns a new, uninitialized instance.
        let string = unsafe { (self.send_id)(class, self.selector(c"alloc")) };
        if string.is_null() {
            return Err(WindowError::Protocol(
                "NSString could not be allocated".to_owned(),
            ));
        }
        let bytes = text.as_bytes();
        // SAFETY: `string` is a fresh `NSString` and `bytes` holds `bytes.len()` bytes of
        // UTF-8, which the initializer copies.
        let string = unsafe {
            (self.send_init_string)(
                string,
                self.selector(c"initWithBytes:length:encoding:"),
                bytes.as_ptr(),
                bytes.len(),
                UTF8_STRING_ENCODING,
            )
        };
        if string.is_null() {
            return Err(WindowError::Protocol(
                "the window title could not be made into an NSString".to_owned(),
            ));
        }
        Ok(string)
    }

    fn text(&self, string: Id) -> String {
        if string.is_null() {
            return String::new();
        }
        // SAFETY: `string` is a live `NSString`; `UTF8String` returns a NUL-terminated buffer
        // that lives as long as the current autorelease pool.
        let text = unsafe { (self.send_text)(string, self.selector(c"UTF8String")) };
        if text.is_null() {
            return String::new();
        }
        // SAFETY: `text` is the non-null, NUL-terminated buffer returned above.
        unsafe { CStr::from_ptr(text) }
            .to_string_lossy()
            .into_owned()
    }

    fn add_method(
        &self,
        class: Id,
        class_name: &CStr,
        name: &CStr,
        implementation: *const c_void,
        types: &CStr,
    ) -> Result<(), WindowError> {
        // SAFETY: `class` is an unregistered class pair, `implementation` is an `extern "C"`
        // function whose parameters match `types`, and both strings are NUL-terminated.
        let added = unsafe {
            (self.add_method)(class, self.selector(name), implementation, types.as_ptr())
        };
        if added == NO {
            return Err(WindowError::Protocol(format!(
                "the method {} could not be added to {}",
                name.to_string_lossy(),
                class_name.to_string_lossy()
            )));
        }
        Ok(())
    }

    fn register_class(
        &self,
        name: &CStr,
        superclass: &CStr,
        methods: &[(&CStr, *const c_void, &CStr)],
    ) -> Result<Id, WindowError> {
        // SAFETY: the name is NUL-terminated; the lookup only reads the runtime's class table.
        let existing = unsafe { (self.get_class)(name.as_ptr()) };
        if !existing.is_null() {
            return Ok(existing);
        }
        let superclass = self.class(superclass)?;
        // SAFETY: `superclass` is a registered class and the name is NUL-terminated and not
        // registered.
        let class = unsafe { (self.allocate_class_pair)(superclass, name.as_ptr(), 0) };
        if class.is_null() {
            return Err(WindowError::Protocol(format!(
                "the class {} could not be created",
                name.to_string_lossy()
            )));
        }
        for &(method, implementation, types) in methods {
            self.add_method(class, name, method, implementation, types)?;
        }
        // SAFETY: `class` was allocated above, has all its methods and is registered once.
        unsafe { (self.register_class_pair)(class) };
        Ok(class)
    }

    fn view_class(&self) -> Result<Id, WindowError> {
        let draw: extern "C" fn(Id, Sel, CgRect) = draw_rect;
        let key: extern "C" fn(Id, Sel, Id) = key_down;
        let yes: extern "C" fn(Id, Sel) -> ObjcBool = answer_yes;
        let should_close: extern "C" fn(Id, Sel, Id) -> ObjcBool = window_should_close;
        let resized: extern "C" fn(Id, Sel, Id) = window_did_resize;
        let rescaled: extern "C" fn(Id, Sel, Id) = window_did_change_backing_properties;
        let became_key: extern "C" fn(Id, Sel, Id) = window_did_become_key;
        let resigned_key: extern "C" fn(Id, Sel, Id) = window_did_resign_key;
        let children: extern "C" fn(Id, Sel) -> Id = accessibility::accessibility_children;
        let focused: extern "C" fn(Id, Sel) -> Id = accessibility::accessibility_focused_element;
        let drag_entered: extern "C" fn(Id, Sel, Id) -> u64 = dragging_entered;
        let drag_exited: extern "C" fn(Id, Sel, Id) = dragging_exited;
        let drag_performed: extern "C" fn(Id, Sel, Id) -> ObjcBool = perform_drag_operation;
        let pointer: [(&CStr, extern "C" fn(Id, Sel, Id)); 11] = [
            (c"mouseDown:", mouse_down),
            (c"mouseUp:", mouse_up),
            (c"rightMouseDown:", right_mouse_down),
            (c"rightMouseUp:", right_mouse_up),
            (c"otherMouseDown:", other_mouse_down),
            (c"otherMouseUp:", other_mouse_up),
            (c"mouseMoved:", mouse_moved),
            (c"mouseDragged:", mouse_moved),
            (c"rightMouseDragged:", mouse_moved),
            (c"otherMouseDragged:", mouse_moved),
            (c"scrollWheel:", scroll_wheel),
        ];
        let exited: extern "C" fn(Id, Sel, Id) = mouse_exited;
        let mut methods: Vec<(&CStr, *const c_void, &CStr)> = pointer
            .iter()
            .map(|(name, method)| (*name, *method as *const c_void, VOID_OBJECT_TYPES))
            .collect();
        methods.push((c"mouseExited:", exited as *const c_void, VOID_OBJECT_TYPES));
        methods.extend([
            (c"drawRect:", draw as *const c_void, DRAW_RECT_TYPES),
            (c"keyDown:", key as *const c_void, VOID_OBJECT_TYPES),
            (
                c"acceptsFirstResponder",
                yes as *const c_void,
                BOOL_GETTER_TYPES,
            ),
            (c"isOpaque", yes as *const c_void, BOOL_GETTER_TYPES),
            (
                c"windowShouldClose:",
                should_close as *const c_void,
                BOOL_DELEGATE_TYPES,
            ),
            (
                c"windowDidResize:",
                resized as *const c_void,
                VOID_OBJECT_TYPES,
            ),
            (
                c"windowDidChangeBackingProperties:",
                rescaled as *const c_void,
                VOID_OBJECT_TYPES,
            ),
            (
                c"windowDidBecomeKey:",
                became_key as *const c_void,
                VOID_OBJECT_TYPES,
            ),
            (
                c"windowDidResignKey:",
                resigned_key as *const c_void,
                VOID_OBJECT_TYPES,
            ),
            (
                c"accessibilityChildren",
                children as *const c_void,
                OBJECT_GETTER_TYPES,
            ),
            (
                c"accessibilityFocusedUIElement",
                focused as *const c_void,
                OBJECT_GETTER_TYPES,
            ),
            (
                c"draggingEntered:",
                drag_entered as *const c_void,
                DRAG_OPERATION_TYPES,
            ),
            (
                c"draggingExited:",
                drag_exited as *const c_void,
                VOID_OBJECT_TYPES,
            ),
            (
                c"performDragOperation:",
                drag_performed as *const c_void,
                BOOL_DELEGATE_TYPES,
            ),
        ]);
        self.register_class(VIEW_CLASS, c"NSView", &methods)
    }
}

#[derive(Debug, Default)]
struct Frame {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

#[derive(Debug)]
struct WindowState {
    runtime: Runtime,
    application: Cell<Id>,
    window: Cell<Id>,
    view: Cell<Id>,
    events: RefCell<VecDeque<WindowEvent>>,
    frame: RefCell<Frame>,
    size: Cell<(u32, u32)>,
    scale: Cell<f32>,
    access: RefCell<AccessElements>,
}

thread_local! {
    static VIEWS: RefCell<Vec<(usize, Weak<WindowState>)>> = const { RefCell::new(Vec::new()) };
    static LAUNCHED: Cell<bool> = const { Cell::new(false) };
}

fn state_for(view: Id) -> Option<Rc<WindowState>> {
    VIEWS
        .try_with(|views| {
            let views = views.try_borrow().ok()?;
            views
                .iter()
                .find(|(address, _)| *address == view.addr())
                .and_then(|(_, state)| state.upgrade())
        })
        .ok()
        .flatten()
}

fn register_view(view: Id, state: &Rc<WindowState>) {
    let _ = VIEWS.try_with(|views| {
        if let Ok(mut views) = views.try_borrow_mut() {
            views.retain(|(_, entry)| entry.strong_count() > 0);
            views.push((view.addr(), Rc::downgrade(state)));
        }
    });
}

fn unregister_view(view: Id) {
    let _ = VIEWS.try_with(|views| {
        if let Ok(mut views) = views.try_borrow_mut() {
            views.retain(|(address, _)| *address != view.addr());
        }
    });
}

extern "C" fn draw_rect(view: Id, _selector: Sel, _dirty: CgRect) {
    if let Some(state) = state_for(view) {
        state.draw(view);
    }
}

extern "C" fn key_down(view: Id, _selector: Sel, event: Id) {
    if let Some(state) = state_for(view) {
        state.key_down(event);
    }
}

#[derive(Debug, Clone, Copy)]
enum MouseChange {
    Pressed(PointerButton),
    Released(PointerButton),
    OtherPressed,
    OtherReleased,
    Moved,
}

fn mouse(view: Id, event: Id, change: MouseChange) {
    if let Some(state) = state_for(view) {
        state.mouse(event, change);
    }
}

extern "C" fn mouse_down(view: Id, _selector: Sel, event: Id) {
    mouse(view, event, MouseChange::Pressed(PointerButton::Primary));
}

extern "C" fn mouse_up(view: Id, _selector: Sel, event: Id) {
    mouse(view, event, MouseChange::Released(PointerButton::Primary));
}

extern "C" fn right_mouse_down(view: Id, _selector: Sel, event: Id) {
    mouse(view, event, MouseChange::Pressed(PointerButton::Secondary));
}

extern "C" fn right_mouse_up(view: Id, _selector: Sel, event: Id) {
    mouse(view, event, MouseChange::Released(PointerButton::Secondary));
}

extern "C" fn other_mouse_down(view: Id, _selector: Sel, event: Id) {
    mouse(view, event, MouseChange::OtherPressed);
}

extern "C" fn other_mouse_up(view: Id, _selector: Sel, event: Id) {
    mouse(view, event, MouseChange::OtherReleased);
}

extern "C" fn mouse_moved(view: Id, _selector: Sel, event: Id) {
    mouse(view, event, MouseChange::Moved);
}

extern "C" fn scroll_wheel(view: Id, _selector: Sel, event: Id) {
    if let Some(state) = state_for(view) {
        state.scroll(event);
    }
}

extern "C" fn mouse_exited(view: Id, _selector: Sel, _event: Id) {
    if let Some(state) = state_for(view) {
        state.push(WindowEvent::PointerLeft);
    }
}

extern "C" fn answer_yes(_view: Id, _selector: Sel) -> ObjcBool {
    YES
}

extern "C" fn window_should_close(view: Id, _selector: Sel, _sender: Id) -> ObjcBool {
    match state_for(view) {
        Some(state) => {
            state.push(WindowEvent::CloseRequested);
            NO
        }
        None => YES,
    }
}

extern "C" fn window_did_resize(view: Id, _selector: Sel, _notification: Id) {
    if let Some(state) = state_for(view) {
        state.refresh_size();
    }
}

extern "C" fn window_did_change_backing_properties(view: Id, _selector: Sel, _notification: Id) {
    if let Some(state) = state_for(view) {
        state.refresh_scale();
        state.refresh_size();
    }
}

extern "C" fn window_did_become_key(view: Id, _selector: Sel, _notification: Id) {
    if let Some(state) = state_for(view) {
        state.push(WindowEvent::Focused(true));
    }
}

extern "C" fn window_did_resign_key(view: Id, _selector: Sel, _notification: Id) {
    if let Some(state) = state_for(view) {
        state.push(WindowEvent::Focused(false));
    }
}

extern "C" fn dragging_entered(view: Id, _selector: Sel, sender: Id) -> u64 {
    let Some(state) = state_for(view) else {
        return DRAG_OPERATION_NONE;
    };
    let paths = state.runtime.with_pool(|| state.dragged_files(sender));
    if paths.is_empty() {
        return DRAG_OPERATION_NONE;
    }
    state.push(WindowEvent::FileDragEntered(paths));
    DRAG_OPERATION_COPY
}

extern "C" fn dragging_exited(view: Id, _selector: Sel, _sender: Id) {
    if let Some(state) = state_for(view) {
        state.push(WindowEvent::FileDragLeft);
    }
}

extern "C" fn perform_drag_operation(view: Id, _selector: Sel, sender: Id) -> ObjcBool {
    let Some(state) = state_for(view) else {
        return NO;
    };
    let paths = state.runtime.with_pool(|| state.dragged_files(sender));
    if paths.is_empty() {
        return NO;
    }
    state.push(WindowEvent::FilesDropped(paths));
    YES
}

impl WindowState {
    fn dragged_files(&self, sender: Id) -> Vec<PathBuf> {
        if sender.is_null() {
            return Vec::new();
        }
        let runtime = &self.runtime;
        let Ok(url_class) = runtime.class(c"NSURL") else {
            return Vec::new();
        };
        let Ok(array_class) = runtime.class(c"NSArray") else {
            return Vec::new();
        };
        // SAFETY: `sender` is the `NSDraggingInfo` AppKit passed to the drag method being
        // answered; `draggingPasteboard` returns its pasteboard.
        let pasteboard =
            unsafe { (runtime.send_id)(sender, runtime.selector(c"draggingPasteboard")) };
        if pasteboard.is_null() {
            return Vec::new();
        }
        let classes = [url_class];
        // SAFETY: `classes` holds one live class object, and the count says so; the array
        // returned is autoreleased into the caller's pool.
        let class_list = unsafe {
            (runtime.send_id_objects)(
                array_class,
                runtime.selector(c"arrayWithObjects:count:"),
                classes.as_ptr(),
                classes.len(),
            )
        };
        if class_list.is_null() {
            return Vec::new();
        }
        // SAFETY: `pasteboard` is live, `class_list` is an `NSArray` of classes, and nil
        // options are allowed; the array returned is autoreleased.
        let urls = unsafe {
            (runtime.send_id_id_id)(
                pasteboard,
                runtime.selector(c"readObjectsForClasses:options:"),
                class_list,
                std::ptr::null_mut(),
            )
        };
        if urls.is_null() {
            return Vec::new();
        }
        // SAFETY: `urls` is a live `NSArray`; `count` returns an `NSUInteger`.
        let count = unsafe { (runtime.send_u64)(urls, runtime.selector(c"count")) };
        let mut paths = Vec::new();
        for index in 0..count.min(MOST_DROPPED_FILES) {
            // SAFETY: `index` is below the array's count; the element is an `NSURL`.
            let url = unsafe {
                (runtime.send_id_integer)(urls, runtime.selector(c"objectAtIndex:"), index as isize)
            };
            if url.is_null() {
                continue;
            }
            // SAFETY: `url` is a live `NSURL`; `filePathURL` is nil for a URL that names no file.
            let file_url = unsafe { (runtime.send_id)(url, runtime.selector(c"filePathURL")) };
            if file_url.is_null() {
                continue;
            }
            // SAFETY: `file_url` is a live file `NSURL`; `path` returns an `NSString`.
            let path = unsafe { (runtime.send_id)(file_url, runtime.selector(c"path")) };
            let text = runtime.text(path);
            if !text.is_empty() {
                paths.push(PathBuf::from(text));
            }
        }
        paths
    }

    fn accept_file_drops(&self, view: Id) -> Result<(), WindowError> {
        let runtime = &self.runtime;
        let file_type = runtime.string(FILE_URL_PASTEBOARD_TYPE)?;
        let array_class = runtime.class(c"NSArray")?;
        let types = [file_type];
        // SAFETY: `types` holds one live `NSString`, and the count says so; the array
        // retains it and is autoreleased into the caller's pool.
        let list = unsafe {
            (runtime.send_id_objects)(
                array_class,
                runtime.selector(c"arrayWithObjects:count:"),
                types.as_ptr(),
                types.len(),
            )
        };
        self.send_object_to(view, c"registerForDraggedTypes:", list);
        self.send_to(file_type, c"release");
        Ok(())
    }

    fn choose_file(&self, dialog: &OpenFileDialog) -> Result<Option<PathBuf>, WindowError> {
        let runtime = &self.runtime;
        let panel_class = runtime.class(c"NSOpenPanel")?;
        // SAFETY: `openPanel` on `NSOpenPanel` returns an autoreleased panel.
        let panel = unsafe { (runtime.send_id)(panel_class, runtime.selector(c"openPanel")) };
        if panel.is_null() {
            return Err(WindowError::Protocol(
                "the open panel could not be created".to_owned(),
            ));
        }
        self.send_flag_to(panel, c"setCanChooseFiles:", YES);
        self.send_flag_to(panel, c"setCanChooseDirectories:", NO);
        self.send_flag_to(panel, c"setAllowsMultipleSelection:", NO);
        if let Some(directory) = &dialog.start_in {
            let url_class = runtime.class(c"NSURL")?;
            let path = runtime.string(&directory.to_string_lossy())?;
            // SAFETY: `path` is a live `NSString`; the class method returns an autoreleased
            // directory `NSURL`.
            let url = unsafe {
                (runtime.send_id_id_bool)(
                    url_class,
                    runtime.selector(c"fileURLWithPath:isDirectory:"),
                    path,
                    YES,
                )
            };
            self.send_to(path, c"release");
            self.send_object_to(panel, c"setDirectoryURL:", url);
        }
        // SAFETY: `panel` is live; `runModal` runs the panel's own event loop on this, the
        // main, thread and returns an `NSModalResponse`.
        let response = unsafe { (runtime.send_u64)(panel, runtime.selector(c"runModal")) };
        self.send_object_to(
            self.window.get(),
            c"makeKeyAndOrderFront:",
            std::ptr::null_mut(),
        );
        if response != MODAL_RESPONSE_OK {
            return Ok(None);
        }
        // SAFETY: `panel` is live; `URL` returns the chosen file's `NSURL`, or nil.
        let url = unsafe { (runtime.send_id)(panel, runtime.selector(c"URL")) };
        if url.is_null() {
            return Ok(None);
        }
        // SAFETY: `url` is a live file `NSURL`; `path` returns an `NSString`.
        let path = unsafe { (runtime.send_id)(url, runtime.selector(c"path")) };
        let text = runtime.text(path);
        Ok((!text.is_empty()).then(|| PathBuf::from(text)))
    }

    fn push(&self, event: WindowEvent) {
        if let Ok(mut events) = self.events.try_borrow_mut() {
            events.push_back(event);
        }
    }

    fn pop(&self) -> Option<WindowEvent> {
        self.events.try_borrow_mut().ok()?.pop_front()
    }

    fn send_to(&self, receiver: Id, selector: &CStr) {
        if receiver.is_null() {
            return;
        }
        let runtime = &self.runtime;
        // SAFETY: `receiver` is a live object of this window and `selector` names one of its
        // methods that takes no arguments.
        unsafe { (runtime.send_void)(receiver, runtime.selector(selector)) };
    }

    fn send_object_to(&self, receiver: Id, selector: &CStr, argument: Id) {
        if receiver.is_null() {
            return;
        }
        let runtime = &self.runtime;
        // SAFETY: `receiver` is a live object of this window and `selector` names one of its
        // methods that takes one object (or nil); a returned value, if any, is not used.
        unsafe { (runtime.send_void_id)(receiver, runtime.selector(selector), argument) };
    }

    fn send_flag_to(&self, receiver: Id, selector: &CStr, flag: ObjcBool) {
        if receiver.is_null() {
            return;
        }
        let runtime = &self.runtime;
        // SAFETY: `receiver` is a live object of this window and `selector` names one of its
        // methods that takes one `BOOL`.
        unsafe { (runtime.send_void_bool)(receiver, runtime.selector(selector), flag) };
    }

    fn bounds(&self) -> CgRect {
        let view = self.view.get();
        if view.is_null() {
            return CgRect::default();
        }
        let runtime = &self.runtime;
        // SAFETY: `view` is this window's live view; `bounds` returns an `NSRect`.
        unsafe { (runtime.send_rect)(view, runtime.selector(c"bounds")) }
    }

    fn refresh_size(&self) {
        let size = cocoa::device_size(self.bounds().size, self.scale.get());
        if size != self.size.get() {
            self.size.set(size);
            self.push(WindowEvent::Resized {
                width: size.0,
                height: size.1,
            });
        }
    }

    fn refresh_scale(&self) {
        let window = self.window.get();
        if window.is_null() {
            return;
        }
        let runtime = &self.runtime;
        // SAFETY: `window` is this live `NSWindow`; `backingScaleFactor` returns a `CGFloat`.
        let factor = unsafe { (runtime.send_f64)(window, runtime.selector(c"backingScaleFactor")) };
        let scale = cocoa::scale_from_backing(factor);
        if scale != self.scale.get() {
            self.scale.set(scale);
            self.push(WindowEvent::ScaleChanged(scale));
        }
    }

    fn event_position(&self, event: Id) -> (f32, f32) {
        let runtime = &self.runtime;
        // SAFETY: `event` is the mouse `NSEvent` AppKit passed to the view; `locationInWindow`
        // returns an `NSPoint`.
        let location =
            unsafe { (runtime.send_point)(event, runtime.selector(c"locationInWindow")) };
        cocoa::pointer_position(location, self.bounds().size.height, self.scale.get())
    }

    fn event_modifiers(&self, event: Id) -> Modifiers {
        let runtime = &self.runtime;
        // SAFETY: `event` is a live `NSEvent`; `modifierFlags` returns an `NSUInteger`.
        let flags = unsafe { (runtime.send_u64)(event, runtime.selector(c"modifierFlags")) };
        cocoa::modifiers_from_flags(flags)
    }

    fn mouse(&self, event: Id, change: MouseChange) {
        let (x, y) = self.event_position(event);
        let modifiers = self.event_modifiers(event);
        let other = || {
            let runtime = &self.runtime;
            // SAFETY: `event` is a live mouse `NSEvent`; `buttonNumber` returns an `NSInteger`.
            let number = unsafe { (runtime.send_isize)(event, runtime.selector(c"buttonNumber")) };
            cocoa::other_button(number)
        };
        let (button, pressed) = match change {
            MouseChange::Moved => {
                self.push(WindowEvent::PointerMoved { x, y });
                return;
            }
            MouseChange::Pressed(button) => (Some(button), true),
            MouseChange::Released(button) => (Some(button), false),
            MouseChange::OtherPressed => (other(), true),
            MouseChange::OtherReleased => (other(), false),
        };
        let Some(button) = button else {
            return;
        };
        self.push(if pressed {
            WindowEvent::PointerPressed {
                x,
                y,
                button,
                modifiers,
            }
        } else {
            WindowEvent::PointerReleased {
                x,
                y,
                button,
                modifiers,
            }
        });
    }

    fn scroll(&self, event: Id) {
        let runtime = &self.runtime;
        // SAFETY: `event` is the `NSEvent` AppKit passed to `scrollWheel:`; both deltas are
        // `CGFloat`s.
        let delta_x = unsafe { (runtime.send_f64)(event, runtime.selector(c"scrollingDeltaX")) };
        // SAFETY: as above.
        let delta_y = unsafe { (runtime.send_f64)(event, runtime.selector(c"scrollingDeltaY")) };
        // SAFETY: as above; `hasPreciseScrollingDeltas` returns a `BOOL`.
        let precise =
            unsafe { (runtime.send_bool)(event, runtime.selector(c"hasPreciseScrollingDeltas")) };
        let (lines_down, lines_right) = cocoa::wheel_lines(delta_x, delta_y, precise != NO);
        if lines_down == 0.0 && lines_right == 0.0 {
            return;
        }
        let (x, y) = self.event_position(event);
        self.push(WindowEvent::Wheel {
            x,
            y,
            lines_down,
            lines_right,
            modifiers: self.event_modifiers(event),
        });
    }

    fn track_pointer(&self, view: Id) -> Result<(), WindowError> {
        let runtime = &self.runtime;
        let class = runtime.class(c"NSTrackingArea")?;
        // SAFETY: `alloc` on a class returns a new, uninitialized instance.
        let area = unsafe { (runtime.send_id)(class, runtime.selector(c"alloc")) };
        if area.is_null() {
            return Ok(());
        }
        // SAFETY: `area` is a fresh `NSTrackingArea`; `initWithRect:options:owner:userInfo:`
        // takes an `NSRect` (ignored with `NSTrackingInVisibleRect`), the options, the owning
        // view and a nil dictionary.
        let area = unsafe {
            (runtime.send_init_tracking)(
                area,
                runtime.selector(c"initWithRect:options:owner:userInfo:"),
                CgRect::default(),
                cocoa::TRACKING_OPTIONS,
                view,
                std::ptr::null_mut(),
            )
        };
        if area.is_null() {
            return Ok(());
        }
        self.send_object_to(view, c"addTrackingArea:", area);
        self.send_to(area, c"release");
        Ok(())
    }

    fn pasteboard(&self) -> Result<Id, WindowError> {
        let runtime = &self.runtime;
        let class = runtime.class(c"NSPasteboard")?;
        // SAFETY: `generalPasteboard` is a class method returning the shared pasteboard.
        let board = unsafe { (runtime.send_id)(class, runtime.selector(c"generalPasteboard")) };
        if board.is_null() {
            Err(WindowError::Protocol(
                "the general pasteboard is not available".to_owned(),
            ))
        } else {
            Ok(board)
        }
    }

    fn put_clipboard(&self, text: &str) -> Result<(), WindowError> {
        let runtime = &self.runtime;
        let board = self.pasteboard()?;
        // SAFETY: `board` is the live general pasteboard; `clearContents` returns an
        // `NSInteger`, the new change count.
        unsafe { (runtime.send_isize)(board, runtime.selector(c"clearContents")) };
        let string = runtime.string(text)?;
        let kind = runtime.string(PASTEBOARD_TYPE_STRING)?;
        // SAFETY: `board` is live; `setString:forType:` takes two `NSString`s and returns a
        // `BOOL`.
        let done = unsafe {
            (runtime.send_bool_id_id)(board, runtime.selector(c"setString:forType:"), string, kind)
        };
        self.send_to(string, c"release");
        self.send_to(kind, c"release");
        if done == NO {
            return Err(WindowError::Io(
                "the pasteboard refused the text".to_owned(),
            ));
        }
        Ok(())
    }

    fn clipboard(&self) -> Result<Option<String>, WindowError> {
        let runtime = &self.runtime;
        let board = self.pasteboard()?;
        let kind = runtime.string(PASTEBOARD_TYPE_STRING)?;
        // SAFETY: `board` is live; `stringForType:` takes an `NSString` and returns an
        // autoreleased `NSString` or nil.
        let string =
            unsafe { (runtime.send_id_id)(board, runtime.selector(c"stringForType:"), kind) };
        self.send_to(kind, c"release");
        if string.is_null() {
            return Ok(None);
        }
        let text = runtime.text(string);
        check_clipboard_text(&text)?;
        Ok(Some(text))
    }

    fn key_down(&self, event: Id) {
        let runtime = &self.runtime;
        // SAFETY: `event` is the `NSEvent` AppKit passed to `keyDown:`; `keyCode` returns an
        // `unsigned short`.
        let code = unsafe { (runtime.send_u16)(event, runtime.selector(c"keyCode")) };
        // SAFETY: as above; `modifierFlags` returns an `NSUInteger`.
        let flags = unsafe { (runtime.send_u64)(event, runtime.selector(c"modifierFlags")) };
        let modifiers = cocoa::modifiers_from_flags(flags);
        if let Some(key) = cocoa::key_from_key_code(code) {
            self.push(WindowEvent::Key(KeyPress { key, modifiers }));
            return;
        }
        let selector = if cocoa::reads_unmodified_characters(modifiers) {
            c"charactersIgnoringModifiers"
        } else {
            c"characters"
        };
        // SAFETY: as above; both selectors return an autoreleased `NSString` or nil for a key
        // event.
        let characters = unsafe { (runtime.send_id)(event, runtime.selector(selector)) };
        for key in cocoa::keys_from_characters(&runtime.text(characters)) {
            self.push(WindowEvent::Key(KeyPress { key, modifiers }));
        }
    }

    fn draw(&self, view: Id) {
        let runtime = &self.runtime;
        let Ok(graphics_class) = runtime.class(c"NSGraphicsContext") else {
            return;
        };
        // SAFETY: `currentContext` is a class method returning the context `drawRect:` draws
        // into, or nil.
        let current =
            unsafe { (runtime.send_id)(graphics_class, runtime.selector(c"currentContext")) };
        if current.is_null() {
            return;
        }
        // SAFETY: `current` is a live `NSGraphicsContext`; `CGContext` returns its Core
        // Graphics context.
        let context = unsafe { (runtime.send_id)(current, runtime.selector(c"CGContext")) };
        if context.is_null() {
            return;
        }
        // SAFETY: `view` is the view being drawn; `bounds` returns an `NSRect`.
        let bounds = unsafe { (runtime.send_rect)(view, runtime.selector(c"bounds")) };
        // SAFETY: `context` is live for the whole `drawRect:`.
        unsafe { (runtime.set_fill_color)(context, 0.0, 0.0, 0.0, 1.0) };
        // SAFETY: as above.
        unsafe { (runtime.fill_rect)(context, bounds) };
        let drawn = match self.frame.try_borrow() {
            Ok(frame) if !frame.rgba.is_empty() => {
                self.draw_frame(context, &frame, bounds.size.height);
                true
            }
            _ => false,
        };
        if !drawn {
            self.push(WindowEvent::Redraw);
        }
    }

    fn draw_frame(&self, context: Id, frame: &Frame, view_height: f64) {
        let runtime = &self.runtime;
        let (Ok(length), Ok(width), Ok(height)) = (
            isize::try_from(frame.rgba.len()),
            usize::try_from(frame.width),
            usize::try_from(frame.height),
        ) else {
            return;
        };
        // SAFETY: `frame.rgba` holds `length` bytes and is alive for the call, which copies
        // them.
        let data =
            unsafe { (runtime.data_create)(std::ptr::null_mut(), frame.rgba.as_ptr(), length) };
        if data.is_null() {
            return;
        }
        // SAFETY: `data` is a live `CFData`, which the provider retains.
        let provider = unsafe { (runtime.data_provider_create)(data) };
        // SAFETY: `data` was created above and is released once, here.
        unsafe { (runtime.cf_release)(data) };
        if provider.is_null() {
            return;
        }
        // SAFETY: creating the device RGB color space has no preconditions.
        let space = unsafe { (runtime.color_space_create)() };
        // SAFETY: `provider` holds `width` × `height` pixels of four bytes (checked by
        // `present`), which the 8-bit, 32-bit-per-pixel, skip-last layout describes; a null
        // `space` makes the call return null.
        let image = unsafe {
            (runtime.image_create)(
                width,
                height,
                8,
                32,
                width * 4,
                space,
                ALPHA_NONE_SKIP_LAST,
                provider,
                std::ptr::null(),
                false,
                RENDERING_INTENT_DEFAULT,
            )
        };
        // SAFETY: `space` was created above and is released once; the image keeps its own
        // reference, and releasing null does nothing.
        unsafe { (runtime.color_space_release)(space) };
        // SAFETY: `provider` was created above and is released once; the image keeps its own
        // reference.
        unsafe { (runtime.data_provider_release)(provider) };
        if image.is_null() {
            return;
        }
        let rect = cocoa::frame_rect(frame.width, frame.height, self.scale.get(), view_height);
        // SAFETY: `context` is live for the whole `drawRect:`.
        unsafe { (runtime.set_interpolation)(context, INTERPOLATION_NONE) };
        // SAFETY: `context` and `image` are live.
        unsafe { (runtime.draw_image)(context, rect, image) };
        // SAFETY: `image` was created above and is released once, here.
        unsafe { (runtime.image_release)(image) };
    }

    fn build(state: &Rc<Self>, options: &WindowOptions) -> Result<(), WindowError> {
        let runtime = &state.runtime;
        let application_class = runtime.class(c"NSApplication")?;
        // SAFETY: `sharedApplication` is a class method returning the process's application.
        let application =
            unsafe { (runtime.send_id)(application_class, runtime.selector(c"sharedApplication")) };
        if application.is_null() {
            return Err(WindowError::NoDisplay(
                "NSApplication could not be started".to_owned(),
            ));
        }
        state.application.set(application);
        if !LAUNCHED.with(Cell::get) {
            // SAFETY: `application` is live; `setActivationPolicy:` takes an `NSInteger` and
            // its `BOOL` result is not used.
            unsafe {
                (runtime.send_void_integer)(
                    application,
                    runtime.selector(c"setActivationPolicy:"),
                    ACTIVATION_POLICY_REGULAR,
                )
            };
            state.send_to(application, c"finishLaunching");
            LAUNCHED.with(|launched| launched.set(true));
        }
        let view_class = runtime.view_class()?;
        let content = CgRect {
            size: CgSize {
                width: f64::from(options.width.max(1)),
                height: f64::from(options.height.max(1)),
            },
            ..CgRect::default()
        };
        let window_class = runtime.class(c"NSWindow")?;
        // SAFETY: `alloc` on a class returns a new, uninitialized instance.
        let window = unsafe { (runtime.send_id)(window_class, runtime.selector(c"alloc")) };
        if window.is_null() {
            return Err(WindowError::Protocol(
                "NSWindow could not be allocated".to_owned(),
            ));
        }
        // SAFETY: `window` is a fresh `NSWindow`; the initializer takes the content rectangle,
        // the style mask, the backing store type and a `BOOL`.
        let window = unsafe {
            (runtime.send_init_window)(
                window,
                runtime.selector(c"initWithContentRect:styleMask:backing:defer:"),
                content,
                STYLE_TITLED_CLOSABLE_MINIATURIZABLE_RESIZABLE,
                BACKING_STORE_BUFFERED,
                NO,
            )
        };
        if window.is_null() {
            return Err(WindowError::Protocol(
                "NSWindow could not be created".to_owned(),
            ));
        }
        state.window.set(window);
        state.send_flag_to(window, c"setReleasedWhenClosed:", NO);
        let title = runtime.string(&options.title)?;
        state.send_object_to(window, c"setTitle:", title);
        state.send_to(title, c"release");
        let min_size = CgSize {
            width: f64::from(options.min_width),
            height: f64::from(options.min_height),
        };
        // SAFETY: `window` is live; `setContentMinSize:` takes an `NSSize`.
        unsafe {
            (runtime.send_void_size)(window, runtime.selector(c"setContentMinSize:"), min_size)
        };
        // SAFETY: `alloc` on a class returns a new, uninitialized instance.
        let view = unsafe { (runtime.send_id)(view_class, runtime.selector(c"alloc")) };
        if view.is_null() {
            return Err(WindowError::Protocol(
                "the view could not be allocated".to_owned(),
            ));
        }
        // SAFETY: `view` is a fresh `NSView` subclass instance; `initWithFrame:` takes an
        // `NSRect`.
        let view =
            unsafe { (runtime.send_id_rect)(view, runtime.selector(c"initWithFrame:"), content) };
        if view.is_null() {
            return Err(WindowError::Protocol(
                "the view could not be created".to_owned(),
            ));
        }
        state.view.set(view);
        register_view(view, state);
        state.accept_file_drops(view)?;
        state.track_pointer(view)?;
        state.send_object_to(window, c"setContentView:", view);
        state.send_object_to(window, c"setDelegate:", view);
        state.send_to(window, c"center");
        state.send_object_to(window, c"makeKeyAndOrderFront:", std::ptr::null_mut());
        state.send_object_to(window, c"makeFirstResponder:", view);
        state.send_flag_to(application, c"activateIgnoringOtherApps:", YES);
        state.refresh_scale();
        state.refresh_size();
        while state.pop().is_some() {}
        Ok(())
    }

    fn close(&self) {
        let window = self.window.replace(std::ptr::null_mut());
        let view = self.view.replace(std::ptr::null_mut());
        if !view.is_null() {
            unregister_view(view);
        }
        self.runtime.with_pool(|| {
            self.release_accessibility();
            self.send_object_to(window, c"setDelegate:", std::ptr::null_mut());
            self.send_object_to(window, c"orderOut:", std::ptr::null_mut());
            self.send_to(window, c"close");
            self.send_to(window, c"release");
            self.send_to(view, c"release");
        });
    }

    fn wait_for_event(&self, wait: Option<Duration>) {
        let runtime = &self.runtime;
        let application = self.application.get();
        let Ok(date_class) = runtime.class(c"NSDate") else {
            return;
        };
        runtime.with_pool(|| {
            let date = match wait {
                // SAFETY: `distantFuture` is a class method returning an autoreleased date.
                None => unsafe {
                    (runtime.send_id)(date_class, runtime.selector(c"distantFuture"))
                },
                // SAFETY: `dateWithTimeIntervalSinceNow:` is a class method taking seconds and
                // returning an autoreleased date.
                Some(wait) => unsafe {
                    (runtime.send_id_f64)(
                        date_class,
                        runtime.selector(c"dateWithTimeIntervalSinceNow:"),
                        wait.as_secs_f64(),
                    )
                },
            };
            // SAFETY: `application` is the live `NSApplication`; the method takes a mask, a date
            // (or nil), a run loop mode and a `BOOL`, and returns an autoreleased event or nil.
            let event = unsafe {
                (runtime.send_next_event)(
                    application,
                    runtime.selector(c"nextEventMatchingMask:untilDate:inMode:dequeue:"),
                    EVENT_MASK_ANY,
                    date,
                    runtime.default_mode,
                    YES,
                )
            };
            if !event.is_null() {
                self.send_object_to(application, c"sendEvent:", event);
                self.send_to(application, c"updateWindows");
            }
        });
    }
}

#[derive(Debug)]
pub(crate) struct CocoaWindow {
    state: Rc<WindowState>,
}

impl CocoaWindow {
    pub(crate) fn open(options: &WindowOptions) -> Result<Self, WindowError> {
        // SAFETY: `pthread_main_np` takes no arguments and only reports the calling thread.
        if unsafe { pthread_main_np() } != 1 {
            return Err(WindowError::InvalidInput(
                "a macOS window can only be opened on the main thread".to_owned(),
            ));
        }
        let state = Rc::new(WindowState {
            runtime: Runtime::load()?,
            application: Cell::new(std::ptr::null_mut()),
            window: Cell::new(std::ptr::null_mut()),
            view: Cell::new(std::ptr::null_mut()),
            events: RefCell::new(VecDeque::new()),
            frame: RefCell::new(Frame::default()),
            size: Cell::new((0, 0)),
            scale: Cell::new(1.0),
            access: RefCell::new(AccessElements::default()),
        });
        let built = state
            .runtime
            .with_pool(|| WindowState::build(&state, options));
        if let Err(error) = built {
            state.close();
            return Err(error);
        }
        Ok(Self { state })
    }
}

impl Drop for CocoaWindow {
    fn drop(&mut self) {
        self.state.close();
    }
}

impl Window for CocoaWindow {
    fn size(&self) -> (u32, u32) {
        self.state.size.get()
    }

    fn scale(&self) -> f32 {
        self.state.scale.get()
    }

    fn present(&mut self, width: u32, height: u32, rgba: &[u8]) -> Result<(), WindowError> {
        check_frame(width, height, rgba)?;
        {
            let Ok(mut frame) = self.state.frame.try_borrow_mut() else {
                return Err(WindowError::Protocol(
                    "a frame was presented while the window was drawing".to_owned(),
                ));
            };
            frame.width = width;
            frame.height = height;
            frame.rgba.clear();
            frame.rgba.extend_from_slice(rgba);
        }
        let view = self.state.view.get();
        self.state.runtime.with_pool(|| {
            self.state.send_flag_to(view, c"setNeedsDisplay:", YES);
            self.state.send_to(view, c"displayIfNeeded");
        });
        Ok(())
    }

    fn set_accessibility_tree(&mut self, nodes: &[AccessNode]) {
        let state = &self.state;
        state
            .runtime
            .with_pool(|| state.update_accessibility(nodes));
    }

    fn choose_file_to_open(
        &mut self,
        dialog: &OpenFileDialog,
    ) -> Result<Option<PathBuf>, WindowError> {
        let state = &self.state;
        state.runtime.with_pool(|| state.choose_file(dialog))
    }

    fn put_clipboard_text(&mut self, text: &str) -> Result<(), WindowError> {
        check_clipboard_text(text)?;
        let state = &self.state;
        state.runtime.with_pool(|| state.put_clipboard(text))
    }

    fn clipboard_text(&mut self) -> Result<Option<String>, WindowError> {
        let state = &self.state;
        state.runtime.with_pool(|| state.clipboard())
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
            let wait = deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
            self.state.wait_for_event(wait);
            if let Some(event) = self.state.pop() {
                return Ok(Some(event));
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Ok(None);
            }
        }
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
    fn opening_off_the_main_thread_is_an_error_not_a_crash() {
        let result = std::thread::spawn(|| CocoaWindow::open(&options()).map(|_| ())).join();
        assert!(matches!(result, Ok(Err(WindowError::InvalidInput(_)))));
    }

    #[test]
    fn the_system_libraries_classes_and_view_class_load() {
        let runtime = match Runtime::load() {
            Ok(runtime) => runtime,
            Err(error) => panic!("AppKit did not load: {error}"),
        };
        assert!(runtime.class(c"NSWindow").is_ok());
        assert!(runtime.class(c"NSApplication").is_ok());
        assert!(runtime.class(c"NSAccessibilityElement").is_ok());
        assert!(runtime.class(c"NoSuchEggDBClass").is_err());
        assert!(!runtime.default_mode.is_null());
        let first = runtime.view_class();
        let second = runtime.view_class();
        assert!(first.is_ok());
        assert_eq!(first, second);
        let element = runtime.element_class();
        assert!(element.is_ok());
        assert_eq!(element, runtime.element_class());
        assert_ne!(element, first);
        let text = runtime.with_pool(|| {
            let string = runtime.string("EggDB é").ok()?;
            let text = runtime.text(string);
            // SAFETY: `string` is the `NSString` created above, released once here.
            unsafe { (runtime.send_void)(string, runtime.selector(c"release")) };
            Some(text)
        });
        assert_eq!(text.as_deref(), Some("EggDB é"));
    }
}
